use std::collections::BTreeMap;
use std::io::Write;
use std::sync::Arc;

use chrono::{DateTime, Duration as ChronoDuration, SecondsFormat, Utc};
use signals_core::{
    ConflationPolicy, FeedHealthReport, InstanceStatusEntry, InstanceStatusReasonClass,
    InstanceStatusState, ProducerStatus, QualityState, SIGNAL_HEARTBEAT_INTERVAL_MS,
    SIGNAL_SNAPSHOT_INTERVAL_MS, SIGNAL_STREAM_SCHEMA_VERSION, ScopeKind, SignalInstanceKey,
    SignalScope, SignalStreamMessage, SignalUpdate, StreamResetReason, Validate, ValidationError,
    ValuesSemantics, instance_status_reason_class, projected_scope_kind,
    validate_signal_scope_binding,
};
use thiserror::Error;

use crate::candidate_settlement_signals::{
    KALSHI_ORACLE_DIVERGENCE_ID, KALSHI_SETTLEMENT_ANCHOR_STATE_ID,
};
use crate::feed_health::FeedHealthSnapshot;

/// The two Kalshi settlement identities whose invalidation the E37c roll defers
/// when the departing generation froze a pending final decision (ruling M1):
/// they are invalidated in the departed out-of-band path immediately AFTER the
/// final decision publishes, or when its slot expires, never at the roll.
fn is_deferred_settlement_identity(signal_id: &str) -> bool {
    signal_id == KALSHI_SETTLEMENT_ANCHOR_STATE_ID || signal_id == KALSHI_ORACLE_DIVERGENCE_ID
}

const HEARTBEAT_INTERVAL_NS: u64 = SIGNAL_HEARTBEAT_INTERVAL_MS * 1_000_000;
const SNAPSHOT_INTERVAL_NS: u64 = SIGNAL_SNAPSHOT_INTERVAL_MS * 1_000_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProducerHealth {
    pub status: ProducerStatus,
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurrentInstanceStatus {
    pub entry: InstanceStatusEntry,
    pub last_emit_ns: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceLedgerEntry {
    pub last_decision_sequence: Option<u64>,
    pub status: Option<CurrentInstanceStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidatedTombstone {
    pub entry: InstanceStatusEntry,
    pub original_valid_until: String,
}

#[derive(Debug, Error)]
pub enum StreamAccountingError {
    #[error("signal stream sequence mismatch: expected {expected}, actual {actual}")]
    StreamSequence { expected: u64, actual: u64 },
    #[error("stream output reached its {0} update limit")]
    UpdateLimit(u64),
    #[error("stream output reached its {0} byte limit")]
    ByteLimit(u64),
    #[error("stream accounting counter exhausted")]
    Counter,
    #[error(transparent)]
    Validation(#[from] ValidationError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Timestamp(#[from] chrono::ParseError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("snapshot state has no decision-or-status anchor")]
    MissingSnapshotAnchor,
    #[error("stream health is unavailable")]
    MissingStreamHealth,
    #[error("stream accounting received an unexpected market transition")]
    UnexpectedMarketTransition,
    #[error("stream accounting received an unexpected feed health frame")]
    UnexpectedFeedHealth,
    #[error("instance status reason is outside the frozen stream-v3 registry: {0}")]
    UnknownInstanceStatusReason(String),
    #[error("signal definition interval is missing for {0}@{1}")]
    MissingSignalInterval(String, String),
    #[error("invalid market-departure status reason: {0}")]
    InvalidDepartureReason(String),
    #[error("a ready decision carried non-empty quality reasons")]
    InvalidReadyQuality,
}

pub fn stream_timestamp(
    wall_start: &DateTime<Utc>,
    now_ns: u64,
) -> Result<String, StreamAccountingError> {
    let delta = ChronoDuration::nanoseconds(
        i64::try_from(now_ns).map_err(|_| StreamAccountingError::Counter)?,
    );
    wall_start
        .checked_add_signed(delta)
        .map(|timestamp| timestamp.to_rfc3339_opts(SecondsFormat::Nanos, true))
        .ok_or(StreamAccountingError::Counter)
}

/// An in-process observer of every stream message a pipeline writes.
///
/// Called from [`StreamAccounting::write_stream_message`] with the validated
/// message *before* its bytes reach the writer, so an in-process consumer
/// (the exchange's signal bridge) sees an update without waiting on file
/// I/O. The hook runs on the lane's own thread inside the write path: it
/// must not block, and it must not fail (it returns nothing). The file
/// output is unchanged whether or not a hook is installed.
pub type StreamHook = Arc<dyn Fn(&SignalStreamMessage) + Send + Sync>;

pub struct StreamAccounting<'a, W: Write> {
    pub writer: &'a mut W,
    /// Optional in-process observer; see [`StreamHook`].
    pub hook: Option<&'a StreamHook>,
    pub scratch: &'a mut Vec<u8>,
    pub publisher_epoch: &'a str,
    pub wall_start: DateTime<Utc>,
    pub maximum_updates: u64,
    pub maximum_bytes: u64,
    pub next_stream_sequence: &'a mut u64,
    pub last_signal_batch_sequence: &'a mut Option<u64>,
    pub last_decision_or_status_sequence: &'a mut Option<u64>,
    pub latest_updates: &'a mut BTreeMap<SignalInstanceKey, SignalUpdate>,
    pub instance_ledger: &'a mut BTreeMap<SignalInstanceKey, InstanceLedgerEntry>,
    pub invalidated_tombstones: &'a mut BTreeMap<SignalInstanceKey, InvalidatedTombstone>,
    pub signal_intervals_ms: &'a BTreeMap<(String, String), u64>,
    pub venue_gap_active: bool,
    pub producer_health: &'a mut Option<ProducerHealth>,
    pub next_heartbeat_ns: &'a mut Option<u64>,
    pub next_snapshot_ns: &'a mut Option<u64>,
    pub updates_written: &'a mut u64,
    pub bytes_written: &'a mut u64,
    pub stream_messages_written: &'a mut u64,
    pub signal_batches_written: &'a mut u64,
    pub snapshots_written: &'a mut u64,
    pub heartbeats_written: &'a mut u64,
    pub status_messages_written: &'a mut u64,
    pub reset_messages_written: &'a mut u64,
    pub market_transitions_written: Option<&'a mut u64>,
    pub feed_health_written: Option<&'a mut u64>,
}

impl<W: Write> StreamAccounting<'_, W> {
    pub fn ensure_stream_started(&mut self, now_ns: u64) -> Result<(), StreamAccountingError> {
        if self.producer_health.is_some() {
            return Ok(());
        }
        let produced_at = stream_timestamp(&self.wall_start, now_ns)?;
        let reset = SignalStreamMessage::Reset {
            schema_version: SIGNAL_STREAM_SCHEMA_VERSION,
            publisher_epoch: self.publisher_epoch.to_owned(),
            sequence: *self.next_stream_sequence,
            produced_at: produced_at.clone(),
            reason: StreamResetReason::PublisherEpochStarted,
        };
        self.write_stream_message(&reset)?;
        let health = ProducerHealth {
            status: ProducerStatus::Starting,
            reasons: vec!["signal_set_incomplete".to_owned()],
        };
        let status = SignalStreamMessage::Status {
            schema_version: SIGNAL_STREAM_SCHEMA_VERSION,
            publisher_epoch: self.publisher_epoch.to_owned(),
            sequence: *self.next_stream_sequence,
            produced_at,
            status: health.status,
            reasons: health.reasons.clone(),
            as_of_sequence: None,
        };
        self.write_stream_message(&status)?;
        *self.producer_health = Some(health);
        *self.next_heartbeat_ns = Some(checked_add(now_ns, HEARTBEAT_INTERVAL_NS)?);
        *self.next_snapshot_ns = Some(checked_add(now_ns, SNAPSHOT_INTERVAL_NS)?);
        Ok(())
    }

    pub fn write_signal_batch(
        &mut self,
        now_ns: u64,
        evaluations: Vec<SignalUpdate>,
    ) -> Result<(), StreamAccountingError> {
        let produced_at = stream_timestamp(&self.wall_start, now_ns)?;
        let mut ready_updates = Vec::new();
        let mut pending_statuses = Vec::new();

        for update in evaluations {
            validate_signal_scope_binding(&update.signal_id, &update.scope)?;
            let key = update.instance_key();
            if update.quality.state == QualityState::Ready {
                if !update.quality.reasons.is_empty() {
                    return Err(StreamAccountingError::InvalidReadyQuality);
                }
                self.instance_ledger.insert(
                    key.clone(),
                    InstanceLedgerEntry {
                        last_decision_sequence: Some(update.sequence),
                        status: None,
                    },
                );
                self.invalidated_tombstones.remove(&key);
                self.latest_updates.insert(key, update.clone());
                ready_updates.push(update);
                continue;
            }

            let mut reasons = update.quality.reasons.clone();
            reasons.sort();
            reasons.dedup();
            if reasons.is_empty() {
                return Err(StreamAccountingError::UnknownInstanceStatusReason(
                    "<empty>".to_owned(),
                ));
            }
            let mut highest = InstanceStatusReasonClass::Warming;
            for reason in &reasons {
                let class = instance_status_reason_class(reason).ok_or_else(|| {
                    StreamAccountingError::UnknownInstanceStatusReason(reason.clone())
                })?;
                highest = highest.max(class);
            }
            let previous = self.instance_ledger.get(&key);
            let last_decision_sequence = previous.and_then(|entry| entry.last_decision_sequence);
            let state = match highest {
                InstanceStatusReasonClass::Unavailable => InstanceStatusState::Unavailable,
                InstanceStatusReasonClass::Degraded => InstanceStatusState::Degraded,
                InstanceStatusReasonClass::Warming if last_decision_sequence.is_none() => {
                    InstanceStatusState::WarmingUp
                }
                InstanceStatusReasonClass::Warming => InstanceStatusState::Degraded,
            };
            let interval_ms = self
                .signal_intervals_ms
                .get(&(update.signal_id.clone(), update.signal_version.clone()))
                .copied()
                .ok_or_else(|| {
                    StreamAccountingError::MissingSignalInterval(
                        update.signal_id.clone(),
                        update.signal_version.clone(),
                    )
                })?;
            let interval_ns = interval_ms
                .checked_mul(1_000_000)
                .ok_or(StreamAccountingError::Counter)?;
            let prior_status = previous.and_then(|entry| entry.status.as_ref());
            let transition = prior_status.is_none_or(|status| {
                status.entry.state != state || status.entry.reasons != reasons
            });
            let effective_time = if transition {
                produced_at.clone()
            } else {
                prior_status
                    .expect("a non-transition has a prior status")
                    .entry
                    .effective_time
                    .clone()
            };
            let periodic_due = prior_status
                .is_some_and(|status| now_ns >= status.last_emit_ns.saturating_add(interval_ns));
            let emit = transition || periodic_due;
            let status = CurrentInstanceStatus {
                entry: InstanceStatusEntry {
                    signal_id: update.signal_id.clone(),
                    signal_version: update.signal_version.clone(),
                    scope: update.scope.clone(),
                    state,
                    reasons,
                    effective_time,
                    last_decision_sequence,
                },
                last_emit_ns: if emit {
                    now_ns
                } else {
                    prior_status
                        .expect("an unchanged status has a prior emission")
                        .last_emit_ns
                },
            };
            self.instance_ledger.insert(
                key.clone(),
                InstanceLedgerEntry {
                    last_decision_sequence,
                    status: Some(status.clone()),
                },
            );
            if emit {
                pending_statuses.push((key, status.entry));
            }
        }

        let update_count =
            u64::try_from(ready_updates.len()).map_err(|_| StreamAccountingError::Counter)?;
        let next_updates = checked_add(*self.updates_written, update_count)?;
        if next_updates > self.maximum_updates {
            return Err(StreamAccountingError::UpdateLimit(self.maximum_updates));
        }

        pending_statuses.sort_by(|left, right| left.0.cmp(&right.0));
        for (_, status) in pending_statuses {
            self.write_instance_status(&produced_at, status)?;
        }

        if ready_updates.is_empty() {
            return Ok(());
        }
        let batch_sequence = *self.next_stream_sequence;
        let message = SignalStreamMessage::SignalBatch {
            schema_version: SIGNAL_STREAM_SCHEMA_VERSION,
            publisher_epoch: self.publisher_epoch.to_owned(),
            sequence: batch_sequence,
            produced_at,
            conflation_policy: ConflationPolicy::None,
            values_semantics: ValuesSemantics::FullReplace,
            updates: ready_updates,
        };
        self.write_stream_message(&message)?;
        *self.updates_written = next_updates;
        *self.last_signal_batch_sequence = Some(batch_sequence);
        *self.last_decision_or_status_sequence = Some(batch_sequence);
        Ok(())
    }

    fn write_instance_status(
        &mut self,
        produced_at: &str,
        status: InstanceStatusEntry,
    ) -> Result<(), StreamAccountingError> {
        let sequence = *self.next_stream_sequence;
        let message = SignalStreamMessage::InstanceStatus {
            schema_version: SIGNAL_STREAM_SCHEMA_VERSION,
            publisher_epoch: self.publisher_epoch.to_owned(),
            sequence,
            produced_at: produced_at.to_owned(),
            signal_id: status.signal_id,
            signal_version: status.signal_version,
            scope: status.scope,
            state: status.state,
            reasons: status.reasons,
            effective_time: status.effective_time,
            last_decision_sequence: status.last_decision_sequence,
        };
        self.write_stream_message(&message)?;
        *self.last_decision_or_status_sequence = Some(sequence);
        Ok(())
    }

    pub fn invalidate_market_generation(
        &mut self,
        now_ns: u64,
        market_id: &str,
        market_generation: u64,
        reason: &str,
        defer_settlement_identities: bool,
    ) -> Result<(), StreamAccountingError> {
        validate_departure_reason(reason)?;
        let produced_at = stream_timestamp(&self.wall_start, now_ns)?;
        let scope = SignalScope::Market {
            market_id: market_id.to_owned(),
            market_generation,
        };
        // A market rollover invalidates only this generation's MARKET-scoped
        // instances. A global identity the lane also publishes — the armed
        // Kalshi lane's `btc.cfbrti.oracle_reference` (E36/E43) — is not bound to
        // any market: it survives the rollover and must never be tombstoned under
        // a market scope, which would both invalidate a live global instance and
        // trip the admission boundary (a global venue permits only global scope).
        //
        // When this generation departs into a pending E37c final decision
        // (`defer_settlement_identities`), the two settlement identities are also
        // skipped here: their invalidation is emitted in the departed out-of-band
        // path AFTER the final decision publishes (or when the slot expires), so
        // each identity's wire sequence stays ready(final) -> invalidated and never
        // invalidated -> ready (ruling M1).
        let mut keys = self
            .signal_intervals_ms
            .keys()
            .filter(|(signal_id, _)| {
                projected_scope_kind(signal_id, None) == Some(ScopeKind::Market)
            })
            .filter(|(signal_id, _)| {
                !(defer_settlement_identities && is_deferred_settlement_identity(signal_id))
            })
            .map(|(signal_id, signal_version)| SignalInstanceKey {
                signal_id: signal_id.clone(),
                signal_version: signal_version.clone(),
                scope: scope.clone(),
            })
            .collect::<Vec<_>>();
        keys.sort();
        for key in keys {
            self.invalidate_instance_key(key, &produced_at, reason)?;
        }
        Ok(())
    }

    /// Invalidate exactly the two Kalshi settlement identities for a DEPARTED
    /// market generation, deferred out of the roll by
    /// [`Self::invalidate_market_generation`] (ruling M1). Called from the
    /// departed out-of-band path immediately after the departed final decision
    /// publishes, or when its slot expires unpublished, so each identity's wire
    /// sequence is ready(final) -> invalidated and never invalidated -> ready.
    pub fn invalidate_departed_settlement_identities(
        &mut self,
        now_ns: u64,
        market_id: &str,
        market_generation: u64,
        reason: &str,
    ) -> Result<(), StreamAccountingError> {
        validate_departure_reason(reason)?;
        let produced_at = stream_timestamp(&self.wall_start, now_ns)?;
        let scope = SignalScope::Market {
            market_id: market_id.to_owned(),
            market_generation,
        };
        let mut keys = self
            .signal_intervals_ms
            .keys()
            .filter(|(signal_id, _)| is_deferred_settlement_identity(signal_id))
            .map(|(signal_id, signal_version)| SignalInstanceKey {
                signal_id: signal_id.clone(),
                signal_version: signal_version.clone(),
                scope: scope.clone(),
            })
            .collect::<Vec<_>>();
        keys.sort();
        for key in keys {
            self.invalidate_instance_key(key, &produced_at, reason)?;
        }
        Ok(())
    }

    /// Invalidate one explicit instance key (any scope, including `multi_market`):
    /// write its terminal `invalidated` status and retain a suppression tombstone
    /// if its last decision had not lapsed. The multi lane uses this to invalidate
    /// a bound cross-venue instance when a leg departs (C1/E57, V10/L2), which
    /// `invalidate_market_generation` cannot express because that helper is scoped
    /// to a single venue's market generation.
    pub fn invalidate_instance(
        &mut self,
        now_ns: u64,
        key: SignalInstanceKey,
        reason: &str,
    ) -> Result<(), StreamAccountingError> {
        let produced_at = stream_timestamp(&self.wall_start, now_ns)?;
        self.invalidate_instance_key(key, &produced_at, reason)
    }

    /// Tombstone one instance key: write its `invalidated` status and, if its last
    /// decision had not yet lapsed, retain a suppression tombstone. Shared by the
    /// roll invalidation and the deferred settlement-identity invalidation.
    fn invalidate_instance_key(
        &mut self,
        key: SignalInstanceKey,
        produced_at: &str,
        reason: &str,
    ) -> Result<(), StreamAccountingError> {
        validate_signal_scope_binding(&key.signal_id, &key.scope)?;
        let ledger = self.instance_ledger.remove(&key);
        let decision = self.latest_updates.remove(&key);
        let last_decision_sequence = ledger
            .as_ref()
            .and_then(|entry| entry.last_decision_sequence)
            .or_else(|| decision.as_ref().map(|update| update.sequence));
        let entry = InstanceStatusEntry {
            signal_id: key.signal_id.clone(),
            signal_version: key.signal_version.clone(),
            scope: key.scope.clone(),
            state: InstanceStatusState::Invalidated,
            reasons: vec![reason.to_owned()],
            effective_time: produced_at.to_owned(),
            last_decision_sequence,
        };
        self.write_instance_status(produced_at, entry.clone())?;
        self.invalidated_tombstones.remove(&key);
        if let Some(update) = decision
            && timestamp_is_after(&update.valid_until, produced_at)?
        {
            self.invalidated_tombstones.insert(
                key,
                InvalidatedTombstone {
                    entry,
                    original_valid_until: update.valid_until,
                },
            );
        }
        Ok(())
    }

    pub fn maybe_write_status(
        &mut self,
        now_ns: u64,
        expected_signal_count: usize,
    ) -> Result<(), StreamAccountingError> {
        let health = self.current_health(expected_signal_count);
        if self.producer_health.as_ref() == Some(&health) {
            return Ok(());
        }
        let message = SignalStreamMessage::Status {
            schema_version: SIGNAL_STREAM_SCHEMA_VERSION,
            publisher_epoch: self.publisher_epoch.to_owned(),
            sequence: *self.next_stream_sequence,
            produced_at: stream_timestamp(&self.wall_start, now_ns)?,
            status: health.status,
            reasons: health.reasons.clone(),
            as_of_sequence: *self.last_signal_batch_sequence,
        };
        self.write_stream_message(&message)?;
        *self.producer_health = Some(health);
        Ok(())
    }

    /// Emit a `status` frame from a caller-computed [`ProducerHealth`], deduped
    /// against the last emitted health exactly as [`Self::maybe_write_status`]
    /// does. A lane whose readiness is not a function of a signal-instance ledger
    /// — the feed-only venue lanes, whose readiness is "every required stream is
    /// live" — computes its own health and passes it here. The spot and
    /// prediction paths keep using [`Self::maybe_write_status`] and are untouched.
    pub fn maybe_write_status_health(
        &mut self,
        now_ns: u64,
        health: ProducerHealth,
    ) -> Result<(), StreamAccountingError> {
        if self.producer_health.as_ref() == Some(&health) {
            return Ok(());
        }
        let message = SignalStreamMessage::Status {
            schema_version: SIGNAL_STREAM_SCHEMA_VERSION,
            publisher_epoch: self.publisher_epoch.to_owned(),
            sequence: *self.next_stream_sequence,
            produced_at: stream_timestamp(&self.wall_start, now_ns)?,
            status: health.status,
            reasons: health.reasons.clone(),
            as_of_sequence: *self.last_signal_batch_sequence,
        };
        self.write_stream_message(&message)?;
        *self.producer_health = Some(health);
        Ok(())
    }

    pub fn maybe_write_snapshot(&mut self, now_ns: u64) -> Result<(), StreamAccountingError> {
        let Some(deadline_ns) = *self.next_snapshot_ns else {
            return Ok(());
        };
        if now_ns < deadline_ns {
            return Ok(());
        }
        let Some(as_of_sequence) = *self.last_decision_or_status_sequence else {
            // There is no legal snapshot anchor until the first decision or
            // instance status. Remaining unbound must not terminate a producer.
            *self.next_snapshot_ns = Some(checked_add(now_ns, SNAPSHOT_INTERVAL_NS)?);
            return Ok(());
        };
        let produced_at = stream_timestamp(&self.wall_start, now_ns)?;
        let expired_updates = self
            .latest_updates
            .iter()
            .filter_map(|(key, update)| {
                timestamp_is_after(&update.valid_until, &produced_at)
                    .map(|unexpired| (!unexpired).then(|| key.clone()))
                    .transpose()
            })
            .collect::<Result<Vec<_>, _>>()?;
        for key in expired_updates {
            self.latest_updates.remove(&key);
        }
        let expired_tombstones = self
            .invalidated_tombstones
            .iter()
            .filter_map(|(key, tombstone)| {
                timestamp_is_after(&tombstone.original_valid_until, &produced_at)
                    .map(|unexpired| (!unexpired).then(|| key.clone()))
                    .transpose()
            })
            .collect::<Result<Vec<_>, _>>()?;
        for key in expired_tombstones {
            self.invalidated_tombstones.remove(&key);
        }
        let mut statuses = BTreeMap::new();
        for (key, ledger) in self.instance_ledger.iter() {
            if let Some(status) = &ledger.status {
                statuses.insert(key.clone(), status.entry.clone());
            }
        }
        for (key, tombstone) in self.invalidated_tombstones.iter() {
            statuses.insert(key.clone(), tombstone.entry.clone());
        }
        let message = SignalStreamMessage::Snapshot {
            schema_version: SIGNAL_STREAM_SCHEMA_VERSION,
            publisher_epoch: self.publisher_epoch.to_owned(),
            sequence: *self.next_stream_sequence,
            produced_at,
            as_of_sequence,
            conflation_policy: ConflationPolicy::None,
            values_semantics: ValuesSemantics::FullReplace,
            updates: self.latest_updates.values().cloned().collect(),
            statuses: statuses.into_values().collect(),
        };
        self.write_stream_message(&message)?;
        *self.next_snapshot_ns = Some(checked_add(now_ns, SNAPSHOT_INTERVAL_NS)?);
        Ok(())
    }

    /// Emit one observe-only `feed_health` frame from a board snapshot. The
    /// caller decides when to emit (on a health transition or the bounded
    /// periodic cadence); this method only frames, validates, and counts it.
    /// `down_since` is rendered on the session wall origin from the snapshot's
    /// monotonic instant.
    pub fn write_feed_health(
        &mut self,
        now_ns: u64,
        snapshot: &FeedHealthSnapshot,
    ) -> Result<(), StreamAccountingError> {
        let mut feeds = Vec::with_capacity(snapshot.feeds.len());
        for entry in &snapshot.feeds {
            let down_since = match entry.down_since_ns {
                Some(down_since_ns) => Some(stream_timestamp(&self.wall_start, down_since_ns)?),
                None => None,
            };
            feeds.push(FeedHealthReport {
                source_id: entry.source_id.clone(),
                venue: entry.venue.clone(),
                status: entry.status,
                last_event_age_ms: entry.last_event_age_ms,
                down_since,
                disconnect_count: entry.disconnect_count,
                reconnect_count: entry.reconnect_count,
                anomaly_velocity: entry.anomaly_velocity,
            });
        }
        let message = SignalStreamMessage::FeedHealth {
            schema_version: SIGNAL_STREAM_SCHEMA_VERSION,
            publisher_epoch: self.publisher_epoch.to_owned(),
            sequence: *self.next_stream_sequence,
            produced_at: stream_timestamp(&self.wall_start, now_ns)?,
            feeds,
        };
        self.write_stream_message(&message)
    }

    pub fn maybe_write_heartbeat(&mut self, now_ns: u64) -> Result<(), StreamAccountingError> {
        let Some(deadline_ns) = *self.next_heartbeat_ns else {
            return Ok(());
        };
        if now_ns < deadline_ns {
            return Ok(());
        }
        let status = self
            .producer_health
            .as_ref()
            .ok_or(StreamAccountingError::MissingStreamHealth)?
            .status;
        let message = SignalStreamMessage::Heartbeat {
            schema_version: SIGNAL_STREAM_SCHEMA_VERSION,
            publisher_epoch: self.publisher_epoch.to_owned(),
            sequence: *self.next_stream_sequence,
            produced_at: stream_timestamp(&self.wall_start, now_ns)?,
            heartbeat_interval_ms: SIGNAL_HEARTBEAT_INTERVAL_MS,
            status,
            as_of_sequence: *self.last_signal_batch_sequence,
        };
        self.write_stream_message(&message)?;
        *self.next_heartbeat_ns = Some(checked_add(now_ns, HEARTBEAT_INTERVAL_NS)?);
        Ok(())
    }

    pub fn current_health(&self, expected_signal_count: usize) -> ProducerHealth {
        if self.venue_gap_active
            || self.instance_ledger.values().any(|entry| {
                entry.status.as_ref().is_some_and(|status| {
                    matches!(
                        status.entry.state,
                        InstanceStatusState::Degraded | InstanceStatusState::Unavailable
                    )
                })
            })
        {
            return ProducerHealth {
                status: ProducerStatus::Degraded,
                reasons: vec!["signal_set_degraded".to_owned()],
            };
        }
        if self.instance_ledger.len() < expected_signal_count {
            return ProducerHealth {
                status: ProducerStatus::Starting,
                reasons: vec!["signal_set_incomplete".to_owned()],
            };
        }
        if self.instance_ledger.values().any(|entry| {
            entry
                .status
                .as_ref()
                .is_some_and(|status| status.entry.state == InstanceStatusState::WarmingUp)
        }) {
            return ProducerHealth {
                status: ProducerStatus::Starting,
                reasons: vec!["signal_set_warming_up".to_owned()],
            };
        }
        ProducerHealth {
            status: ProducerStatus::Ready,
            reasons: Vec::new(),
        }
    }

    pub fn write_stream_message(
        &mut self,
        message: &SignalStreamMessage,
    ) -> Result<(), StreamAccountingError> {
        if message.sequence() != *self.next_stream_sequence {
            return Err(StreamAccountingError::StreamSequence {
                expected: *self.next_stream_sequence,
                actual: message.sequence(),
            });
        }
        message.validate()?;
        if let Some(hook) = self.hook {
            hook(message);
        }
        self.scratch.clear();
        serde_json::to_writer(&mut *self.scratch, message)?;
        self.scratch.push(b'\n');
        let line_bytes =
            u64::try_from(self.scratch.len()).map_err(|_| StreamAccountingError::Counter)?;
        let next_bytes = checked_add(*self.bytes_written, line_bytes)?;
        if next_bytes > self.maximum_bytes {
            return Err(StreamAccountingError::ByteLimit(self.maximum_bytes));
        }
        self.writer.write_all(&self.scratch[..])?;
        *self.bytes_written = next_bytes;
        *self.stream_messages_written = checked_add(*self.stream_messages_written, 1)?;
        match message {
            SignalStreamMessage::SignalBatch { .. } => {
                *self.signal_batches_written = checked_add(*self.signal_batches_written, 1)?;
            }
            SignalStreamMessage::Snapshot { .. } => {
                *self.snapshots_written = checked_add(*self.snapshots_written, 1)?;
            }
            SignalStreamMessage::InstanceStatus { .. } => {}
            SignalStreamMessage::Heartbeat { .. } => {
                *self.heartbeats_written = checked_add(*self.heartbeats_written, 1)?;
            }
            SignalStreamMessage::Status { .. } => {
                *self.status_messages_written = checked_add(*self.status_messages_written, 1)?;
            }
            SignalStreamMessage::Reset { .. } => {
                *self.reset_messages_written = checked_add(*self.reset_messages_written, 1)?;
            }
            SignalStreamMessage::MarketTransition { .. } => {
                match &mut self.market_transitions_written {
                    Some(counter) => **counter = checked_add(**counter, 1)?,
                    None => return Err(StreamAccountingError::UnexpectedMarketTransition),
                }
            }
            SignalStreamMessage::FeedHealth { .. } => match &mut self.feed_health_written {
                Some(counter) => **counter = checked_add(**counter, 1)?,
                None => return Err(StreamAccountingError::UnexpectedFeedHealth),
            },
        }
        *self.next_stream_sequence = checked_add(*self.next_stream_sequence, 1)?;
        Ok(())
    }
}

/// The market-departure reasons a market-scoped invalidation may carry, shared
/// by the roll invalidation and its deferred settlement-identity counterpart.
fn validate_departure_reason(reason: &str) -> Result<(), StreamAccountingError> {
    if !matches!(
        reason,
        "market_rollover"
            | "market_discovery_unreachable"
            | "next_market_unlisted"
            | "market_discovery_invalid"
            | "market_open_gap"
            | "provider_lifecycle_change"
    ) {
        return Err(StreamAccountingError::InvalidDepartureReason(
            reason.to_owned(),
        ));
    }
    Ok(())
}

fn checked_add(value: u64, increment: u64) -> Result<u64, StreamAccountingError> {
    value
        .checked_add(increment)
        .ok_or(StreamAccountingError::Counter)
}

fn timestamp_is_after(left: &str, right: &str) -> Result<bool, StreamAccountingError> {
    let left = DateTime::parse_from_rfc3339(left)?;
    let right = DateTime::parse_from_rfc3339(right)?;
    Ok(left > right)
}

#[cfg(test)]
mod tests {
    use super::*;
    use signals_core::{
        FeedHealthStatus, Quality, SIGNAL_UPDATE_SCHEMA_VERSION, SignalStreamMessage,
    };
    use std::collections::BTreeMap;

    const MARKET_ID: &str = "btc-updown-5m-1786726800";

    struct AccountingHarness {
        writer: Vec<u8>,
        hook: Option<StreamHook>,
        maximum_bytes: u64,
        scratch: Vec<u8>,
        next_stream_sequence: u64,
        last_signal_batch_sequence: Option<u64>,
        last_decision_or_status_sequence: Option<u64>,
        latest_updates: BTreeMap<SignalInstanceKey, SignalUpdate>,
        instance_ledger: BTreeMap<SignalInstanceKey, InstanceLedgerEntry>,
        invalidated_tombstones: BTreeMap<SignalInstanceKey, InvalidatedTombstone>,
        signal_intervals_ms: BTreeMap<(String, String), u64>,
        producer_health: Option<ProducerHealth>,
        next_heartbeat_ns: Option<u64>,
        next_snapshot_ns: Option<u64>,
        updates_written: u64,
        bytes_written: u64,
        stream_messages_written: u64,
        signal_batches_written: u64,
        snapshots_written: u64,
        heartbeats_written: u64,
        status_messages_written: u64,
        reset_messages_written: u64,
        market_transitions_written: u64,
        feed_health_written: u64,
    }

    impl AccountingHarness {
        fn new(signals: &[(&str, &str)]) -> Self {
            Self {
                writer: Vec::new(),
                hook: None,
                maximum_bytes: u64::MAX,
                scratch: Vec::new(),
                next_stream_sequence: 0,
                last_signal_batch_sequence: None,
                last_decision_or_status_sequence: None,
                latest_updates: BTreeMap::new(),
                instance_ledger: BTreeMap::new(),
                invalidated_tombstones: BTreeMap::new(),
                signal_intervals_ms: signals
                    .iter()
                    .map(|(signal_id, signal_version)| {
                        ((signal_id.to_string(), signal_version.to_string()), 1_000)
                    })
                    .collect(),
                producer_health: None,
                next_heartbeat_ns: None,
                next_snapshot_ns: None,
                updates_written: 0,
                bytes_written: 0,
                stream_messages_written: 0,
                signal_batches_written: 0,
                snapshots_written: 0,
                heartbeats_written: 0,
                status_messages_written: 0,
                reset_messages_written: 0,
                market_transitions_written: 0,
                feed_health_written: 0,
            }
        }

        fn accounting(&mut self, venue_gap_active: bool) -> StreamAccounting<'_, Vec<u8>> {
            StreamAccounting {
                writer: &mut self.writer,
                hook: self.hook.as_ref(),
                scratch: &mut self.scratch,
                publisher_epoch: "stream-v3-test",
                wall_start: wall_start(),
                maximum_updates: u64::MAX,
                maximum_bytes: self.maximum_bytes,
                next_stream_sequence: &mut self.next_stream_sequence,
                last_signal_batch_sequence: &mut self.last_signal_batch_sequence,
                last_decision_or_status_sequence: &mut self.last_decision_or_status_sequence,
                latest_updates: &mut self.latest_updates,
                instance_ledger: &mut self.instance_ledger,
                invalidated_tombstones: &mut self.invalidated_tombstones,
                signal_intervals_ms: &self.signal_intervals_ms,
                venue_gap_active,
                producer_health: &mut self.producer_health,
                next_heartbeat_ns: &mut self.next_heartbeat_ns,
                next_snapshot_ns: &mut self.next_snapshot_ns,
                updates_written: &mut self.updates_written,
                bytes_written: &mut self.bytes_written,
                stream_messages_written: &mut self.stream_messages_written,
                signal_batches_written: &mut self.signal_batches_written,
                snapshots_written: &mut self.snapshots_written,
                heartbeats_written: &mut self.heartbeats_written,
                status_messages_written: &mut self.status_messages_written,
                reset_messages_written: &mut self.reset_messages_written,
                market_transitions_written: Some(&mut self.market_transitions_written),
                feed_health_written: Some(&mut self.feed_health_written),
            }
        }

        fn messages(&self) -> Vec<SignalStreamMessage> {
            self.writer
                .split(|byte| *byte == b'\n')
                .filter(|line| !line.is_empty())
                .map(|line| serde_json::from_slice(line).expect("stream line deserializes"))
                .collect()
        }
    }

    fn wall_start() -> DateTime<Utc> {
        "2026-08-25T00:00:00Z"
            .parse()
            .expect("test wall origin is valid")
    }

    fn global_update(
        signal_id: &str,
        sequence: u64,
        now_ns: u64,
        valid_until_ns: u64,
        state: QualityState,
        reasons: &[&str],
    ) -> SignalUpdate {
        update(
            signal_id,
            sequence,
            now_ns,
            valid_until_ns,
            state,
            reasons,
            SignalScope::Global,
        )
    }

    fn market_update(
        signal_id: &str,
        sequence: u64,
        now_ns: u64,
        valid_until_ns: u64,
        state: QualityState,
        reasons: &[&str],
    ) -> SignalUpdate {
        update(
            signal_id,
            sequence,
            now_ns,
            valid_until_ns,
            state,
            reasons,
            SignalScope::Market {
                market_id: MARKET_ID.to_owned(),
                market_generation: 7,
            },
        )
    }

    fn update(
        signal_id: &str,
        sequence: u64,
        now_ns: u64,
        valid_until_ns: u64,
        state: QualityState,
        reasons: &[&str],
        scope: SignalScope,
    ) -> SignalUpdate {
        let produced_at = stream_timestamp(&wall_start(), now_ns).expect("timestamp");
        SignalUpdate {
            schema_version: SIGNAL_UPDATE_SCHEMA_VERSION,
            signal_id: signal_id.to_owned(),
            signal_version: "1.0.0".to_owned(),
            publisher_epoch: "stream-v3-test".to_owned(),
            sequence,
            event_time: produced_at.clone(),
            produced_at: produced_at.clone(),
            valid_from: produced_at,
            valid_until: stream_timestamp(&wall_start(), valid_until_ns).expect("valid-until"),
            quality: Quality {
                state,
                reasons: reasons.iter().map(|reason| (*reason).to_owned()).collect(),
            },
            values: BTreeMap::new(),
            scope,
        }
    }

    fn ledger_status(
        signal_id: &str,
        state: InstanceStatusState,
    ) -> (SignalInstanceKey, InstanceLedgerEntry) {
        let entry = InstanceStatusEntry {
            signal_id: signal_id.to_owned(),
            signal_version: "1.0.0".to_owned(),
            scope: SignalScope::Global,
            state,
            reasons: vec![
                match state {
                    InstanceStatusState::WarmingUp => "coinbase_warming_up",
                    InstanceStatusState::Degraded => "coinbase_stale",
                    InstanceStatusState::Unavailable => "coinbase_disconnected",
                    InstanceStatusState::Invalidated => {
                        unreachable!("global invalidation is invalid")
                    }
                }
                .to_owned(),
            ],
            effective_time: "2026-08-25T00:00:00Z".to_owned(),
            last_decision_sequence: None,
        };
        (
            entry.instance_key(),
            InstanceLedgerEntry {
                last_decision_sequence: None,
                status: Some(CurrentInstanceStatus {
                    entry,
                    last_emit_ns: 0,
                }),
            },
        )
    }

    /// Locks the byte-for-byte shape of the stream-emitted per-feed record. The
    /// observe-only board's report struct may gain fields, but `FeedHealthReport`
    /// is the frame consumers decode, so any accidental field leak or reorder here
    /// must fail loudly.
    #[test]
    fn feed_health_report_serialized_shape_is_stable() {
        let report = FeedHealthReport {
            source_id: "kraken.spot.btc-usd.trades".to_owned(),
            venue: "kraken".to_owned(),
            status: FeedHealthStatus::Degraded,
            last_event_age_ms: Some(4_200),
            down_since: Some("2026-08-17T00:00:00Z".to_owned()),
            disconnect_count: 2,
            reconnect_count: 1,
            anomaly_velocity: 3,
        };
        let json = serde_json::to_string(&report).expect("serialize feed health report");
        assert_eq!(
            json,
            r#"{"source_id":"kraken.spot.btc-usd.trades","venue":"kraken","status":"degraded","last_event_age_ms":4200,"down_since":"2026-08-17T00:00:00Z","disconnect_count":2,"reconnect_count":1,"anomaly_velocity":3}"#
        );
    }

    /// The in-process hook sees every written message exactly once, in
    /// stream order, and the file output is byte-identical with or without
    /// a hook installed.
    #[test]
    fn stream_hook_observes_each_written_message_in_order() {
        use std::sync::Mutex;

        let signals = [("btc.external.micro_momentum", "1.0.0")];
        let mut plain = AccountingHarness::new(&signals);
        let mut hooked = AccountingHarness::new(&signals);
        let seen: Arc<Mutex<Vec<SignalStreamMessage>>> = Arc::new(Mutex::new(Vec::new()));
        {
            let seen = seen.clone();
            hooked.hook = Some(Arc::new(move |message: &SignalStreamMessage| {
                seen.lock().unwrap().push(message.clone());
            }));
        }
        for harness in [&mut plain, &mut hooked] {
            let ready = global_update("btc.external.micro_momentum", 0, 0, 60_000_000_000, QualityState::Ready, &[]);
            harness.accounting(false).write_signal_batch(0, vec![ready]).expect("batch");
            let warming = global_update("btc.external.micro_momentum", 1, 1_000_000_000, 60_000_000_000, QualityState::WarmingUp, &["kraken_warming_up"]);
            harness.accounting(false).write_signal_batch(1_000_000_000, vec![warming]).expect("status");
        }
        assert_eq!(plain.writer, hooked.writer, "the hook never changes the file output");
        let messages = hooked.messages();
        assert!(messages.len() >= 2, "a reset, a batch and a status were written: {messages:?}");
        assert_eq!(*seen.lock().unwrap(), messages, "one observation per written message, in stream order");
    }

    /// The hook runs before the bytes reach the writer: with a zero byte cap
    /// the write is refused after the hook has already observed the message,
    /// so the observer has the message and the file has nothing.
    #[test]
    fn stream_hook_runs_before_the_write() {
        use std::sync::Mutex;

        let mut harness = AccountingHarness::new(&[("btc.external.micro_momentum", "1.0.0")]);
        harness.maximum_bytes = 0;
        let seen: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));
        {
            let seen = seen.clone();
            harness.hook = Some(Arc::new(move |message: &SignalStreamMessage| {
                seen.lock().unwrap().push(message.sequence());
            }));
        }
        let ready = global_update("btc.external.micro_momentum", 0, 0, 60_000_000_000, QualityState::Ready, &[]);
        let err = harness.accounting(false).write_signal_batch(0, vec![ready]).unwrap_err();
        assert!(matches!(err, StreamAccountingError::ByteLimit(0)), "{err:?}");
        assert!(harness.writer.is_empty(), "nothing reached the writer");
        assert_eq!(*seen.lock().unwrap(), vec![0], "the first message (the reset at sequence 0) was observed first");
    }

    #[test]
    fn statuses_sort_before_ready_batch_and_empty_batches_are_suppressed() {
        let signals = [
            ("btc.external.absorption_state", "1.0.0"),
            ("btc.external.micro_momentum", "1.0.0"),
            ("btc.external.vwap_deviation", "1.0.0"),
        ];
        let mut harness = AccountingHarness::new(&signals);
        harness
            .accounting(false)
            .write_signal_batch(
                0,
                vec![
                    global_update(
                        "btc.external.vwap_deviation",
                        0,
                        0,
                        60_000_000_000,
                        QualityState::WarmingUp,
                        &["kraken_warming_up"],
                    ),
                    global_update(
                        "btc.external.micro_momentum",
                        0,
                        0,
                        60_000_000_000,
                        QualityState::Ready,
                        &[],
                    ),
                    global_update(
                        "btc.external.absorption_state",
                        0,
                        0,
                        60_000_000_000,
                        QualityState::WarmingUp,
                        &["coinbase_warming_up"],
                    ),
                ],
            )
            .expect("mixed boundary writes");

        let messages = harness.messages();
        assert_eq!(messages.len(), 3);
        let status_ids = messages[..2]
            .iter()
            .map(|message| match message {
                SignalStreamMessage::InstanceStatus { signal_id, .. } => signal_id.as_str(),
                other => panic!("expected instance status, got {other:?}"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            status_ids,
            [
                "btc.external.absorption_state",
                "btc.external.vwap_deviation"
            ]
        );
        match &messages[2] {
            SignalStreamMessage::SignalBatch { updates, .. } => {
                assert_eq!(updates.len(), 1);
                assert_eq!(updates[0].signal_id, "btc.external.micro_momentum");
            }
            other => panic!("expected signal batch, got {other:?}"),
        }

        harness
            .accounting(false)
            .write_signal_batch(
                100_000_000,
                vec![global_update(
                    "btc.external.absorption_state",
                    0,
                    100_000_000,
                    60_000_000_000,
                    QualityState::WarmingUp,
                    &["reference_history_warming_up"],
                )],
            )
            .expect("status-only boundary writes");
        assert!(matches!(
            harness.messages().last(),
            Some(SignalStreamMessage::InstanceStatus { .. })
        ));
        assert_eq!(harness.signal_batches_written, 1);
    }

    #[test]
    fn status_effective_time_restatement_and_ready_recovery_are_deterministic() {
        let signal_id = "btc.external.realized_volatility";
        let mut harness = AccountingHarness::new(&[(signal_id, "1.0.0")]);
        let warming = |now_ns| {
            global_update(
                signal_id,
                0,
                now_ns,
                60_000_000_000,
                QualityState::WarmingUp,
                &["reference_history_warming_up"],
            )
        };

        harness
            .accounting(false)
            .write_signal_batch(0, vec![warming(0)])
            .expect("initial status");
        harness
            .accounting(false)
            .write_signal_batch(500_000_000, vec![warming(500_000_000)])
            .expect("unchanged status before cadence");
        assert_eq!(harness.messages().len(), 1);
        harness
            .accounting(false)
            .write_signal_batch(1_000_000_000, vec![warming(1_000_000_000)])
            .expect("cadence restatement");
        harness
            .accounting(false)
            .write_signal_batch(5_000_000_000, vec![warming(5_000_000_000)])
            .expect("late cadence emits once");

        let statuses = harness
            .messages()
            .into_iter()
            .filter_map(|message| match message {
                SignalStreamMessage::InstanceStatus {
                    effective_time,
                    state,
                    last_decision_sequence,
                    ..
                } => Some((effective_time, state, last_decision_sequence)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(statuses.len(), 3);
        assert!(
            statuses
                .iter()
                .all(|(effective_time, _, _)| effective_time == "2026-08-25T00:00:00.000000000Z")
        );

        harness
            .accounting(false)
            .write_signal_batch(
                5_100_000_000,
                vec![global_update(
                    signal_id,
                    0,
                    5_100_000_000,
                    60_000_000_000,
                    QualityState::WarmingUp,
                    &["volatility_warming_up"],
                )],
            )
            .expect("reason change emits immediately");
        let messages = harness.messages();
        let changed_effective_time = match messages.last().expect("changed status") {
            SignalStreamMessage::InstanceStatus { effective_time, .. } => effective_time,
            other => panic!("expected changed status, got {other:?}"),
        };
        assert_eq!(changed_effective_time, "2026-08-25T00:00:05.100000000Z");

        harness
            .accounting(false)
            .write_signal_batch(
                5_200_000_000,
                vec![global_update(
                    signal_id,
                    7,
                    5_200_000_000,
                    60_000_000_000,
                    QualityState::Ready,
                    &[],
                )],
            )
            .expect("ready recovery is a decision only");
        assert!(matches!(
            harness.messages().last(),
            Some(SignalStreamMessage::SignalBatch { .. })
        ));

        harness
            .accounting(false)
            .write_signal_batch(5_300_000_000, vec![warming(5_300_000_000)])
            .expect("post-decision warming maps to degraded");
        match harness.messages().last().expect("post-decision status") {
            SignalStreamMessage::InstanceStatus {
                state,
                last_decision_sequence,
                ..
            } => {
                assert_eq!(*state, InstanceStatusState::Degraded);
                assert_eq!(*last_decision_sequence, Some(7));
            }
            other => panic!("expected post-decision status, got {other:?}"),
        }
    }

    #[test]
    fn snapshot_preserves_unexpired_decision_beside_current_status_and_expires_strictly() {
        let signal_id = "btc.external.spot_reference";
        let mut harness = AccountingHarness::new(&[(signal_id, "1.0.0")]);
        harness
            .accounting(false)
            .write_signal_batch(
                0,
                vec![global_update(
                    signal_id,
                    3,
                    0,
                    60_000_000_000,
                    QualityState::Ready,
                    &[],
                )],
            )
            .expect("ready decision");
        harness
            .accounting(false)
            .write_signal_batch(
                1_000_000_000,
                vec![global_update(
                    signal_id,
                    3,
                    1_000_000_000,
                    60_000_000_000,
                    QualityState::Degraded,
                    &["reference_constituents_unavailable"],
                )],
            )
            .expect("non-ready state");

        harness.next_snapshot_ns = Some(1_000_000_000);
        harness
            .accounting(false)
            .maybe_write_snapshot(1_000_000_000)
            .expect("unexpired snapshot");
        match harness.messages().last().expect("snapshot") {
            SignalStreamMessage::Snapshot {
                as_of_sequence,
                updates,
                statuses,
                ..
            } => {
                assert_eq!(*as_of_sequence, 1);
                assert_eq!(updates.len(), 1);
                assert_eq!(statuses.len(), 1);
                assert_eq!(updates[0].instance_key(), statuses[0].instance_key());
            }
            other => panic!("expected snapshot, got {other:?}"),
        }

        harness.next_snapshot_ns = Some(60_000_000_000);
        harness
            .accounting(false)
            .maybe_write_snapshot(60_000_000_000)
            .expect("strict-expiry snapshot");
        match harness.messages().last().expect("expiry snapshot") {
            SignalStreamMessage::Snapshot {
                updates, statuses, ..
            } => {
                assert!(updates.is_empty());
                assert_eq!(statuses.len(), 1);
            }
            other => panic!("expected expiry snapshot, got {other:?}"),
        }
    }

    #[test]
    fn snapshot_deadline_without_an_anchor_is_safe_and_rescheduled() {
        let mut harness = AccountingHarness::new(&[("btc.external.spot_reference", "1.0.0")]);
        harness.next_snapshot_ns = Some(60_000_000_000);
        harness
            .accounting(false)
            .maybe_write_snapshot(60_000_000_000)
            .expect("unbound producer skips an unanchored snapshot");
        assert!(harness.messages().is_empty());
        assert_eq!(harness.next_snapshot_ns, Some(120_000_000_000));
    }

    #[test]
    fn departure_always_emits_live_invalidations_but_only_tombstones_live_decisions() {
        let signals = [
            ("btc.polymarket.executable_edge", "1.0.0"),
            ("btc.polymarket.fair_up_probability", "1.0.0"),
            ("btc.polymarket.market_contract", "1.0.0"),
        ];
        let mut harness = AccountingHarness::new(&signals);
        harness
            .accounting(false)
            .write_signal_batch(
                0,
                vec![
                    market_update(
                        "btc.polymarket.executable_edge",
                        4,
                        0,
                        60_000_000_000,
                        QualityState::Ready,
                        &[],
                    ),
                    market_update(
                        "btc.polymarket.fair_up_probability",
                        0,
                        0,
                        60_000_000_000,
                        QualityState::WarmingUp,
                        &["volatility_warming_up"],
                    ),
                    market_update(
                        "btc.polymarket.market_contract",
                        9,
                        0,
                        500_000_000,
                        QualityState::Ready,
                        &[],
                    ),
                ],
            )
            .expect("seed market generation");
        let before_departure = harness.messages().len();
        harness
            .accounting(false)
            .invalidate_market_generation(1_000_000_000, MARKET_ID, 7, "market_rollover", false)
            .expect("market departure invalidates generation");

        let messages = harness.messages();
        let invalidations = &messages[before_departure..];
        assert_eq!(invalidations.len(), 3);
        assert!(invalidations.iter().all(|message| matches!(
            message,
            SignalStreamMessage::InstanceStatus {
                state: InstanceStatusState::Invalidated,
                ..
            }
        )));
        let last_decisions = invalidations
            .iter()
            .map(|message| match message {
                SignalStreamMessage::InstanceStatus {
                    last_decision_sequence,
                    ..
                } => *last_decision_sequence,
                _ => unreachable!(),
            })
            .collect::<Vec<_>>();
        assert_eq!(last_decisions, [Some(4), None, Some(9)]);
        assert!(harness.instance_ledger.is_empty());
        assert!(harness.latest_updates.is_empty());
        assert_eq!(harness.invalidated_tombstones.len(), 1);

        harness.next_snapshot_ns = Some(1_000_000_000);
        harness
            .accounting(false)
            .maybe_write_snapshot(1_000_000_000)
            .expect("departure snapshot");
        match harness.messages().last().expect("departure snapshot") {
            SignalStreamMessage::Snapshot {
                updates, statuses, ..
            } => {
                assert!(updates.is_empty());
                assert_eq!(statuses.len(), 1);
                assert_eq!(statuses[0].signal_id, "btc.polymarket.executable_edge");
            }
            other => panic!("expected departure snapshot, got {other:?}"),
        }

        harness.next_snapshot_ns = Some(60_000_000_000);
        harness
            .accounting(false)
            .maybe_write_snapshot(60_000_000_000)
            .expect("expired tombstone snapshot");
        match harness
            .messages()
            .last()
            .expect("expired tombstone snapshot")
        {
            SignalStreamMessage::Snapshot { statuses, .. } => assert!(statuses.is_empty()),
            other => panic!("expected expired tombstone snapshot, got {other:?}"),
        }
    }

    #[test]
    fn provider_lifecycle_change_is_an_admitted_departure_reason() {
        let mut harness = AccountingHarness::new(&[("btc.polymarket.market_contract", "1.0.0")]);
        harness
            .accounting(false)
            .invalidate_market_generation(
                1_000_000_000,
                MARKET_ID,
                7,
                "provider_lifecycle_change",
                false,
            )
            .expect("provider lifecycle departure invalidates the generation");
        assert!(matches!(
            harness.messages().last(),
            Some(SignalStreamMessage::InstanceStatus {
                state: InstanceStatusState::Invalidated,
                reasons,
                ..
            }) if reasons == &["provider_lifecycle_change"]
        ));
    }

    #[test]
    fn producer_health_uses_instance_ledger_with_fixed_precedence() {
        let signals = [
            ("btc.external.absorption_state", "1.0.0"),
            ("btc.external.vwap_deviation", "1.0.0"),
        ];
        let mut harness = AccountingHarness::new(&signals);
        assert_eq!(
            harness.accounting(false).current_health(2),
            ProducerHealth {
                status: ProducerStatus::Starting,
                reasons: vec!["signal_set_incomplete".to_owned()],
            }
        );
        let (key, entry) = ledger_status(
            "btc.external.absorption_state",
            InstanceStatusState::WarmingUp,
        );
        harness.instance_ledger.insert(key, entry);
        let (key, entry) = ledger_status(
            "btc.external.vwap_deviation",
            InstanceStatusState::WarmingUp,
        );
        harness.instance_ledger.insert(key, entry);
        assert_eq!(
            harness.accounting(false).current_health(2),
            ProducerHealth {
                status: ProducerStatus::Starting,
                reasons: vec!["signal_set_warming_up".to_owned()],
            }
        );
        let (key, entry) =
            ledger_status("btc.external.vwap_deviation", InstanceStatusState::Degraded);
        harness.instance_ledger.insert(key, entry);
        assert_eq!(
            harness.accounting(false).current_health(2),
            ProducerHealth {
                status: ProducerStatus::Degraded,
                reasons: vec!["signal_set_degraded".to_owned()],
            }
        );
        for entry in harness.instance_ledger.values_mut() {
            entry.status = None;
            entry.last_decision_sequence = Some(0);
        }
        assert_eq!(
            harness.accounting(false).current_health(2),
            ProducerHealth {
                status: ProducerStatus::Ready,
                reasons: Vec::new(),
            }
        );
        assert_eq!(
            harness.accounting(true).current_health(2),
            ProducerHealth {
                status: ProducerStatus::Degraded,
                reasons: vec!["signal_set_degraded".to_owned()],
            }
        );
    }

    #[test]
    fn unknown_instance_status_reason_fails_closed_without_output() {
        let signal_id = "btc.external.spot_reference";
        let mut harness = AccountingHarness::new(&[(signal_id, "1.0.0")]);
        let error = harness
            .accounting(false)
            .write_signal_batch(
                0,
                vec![global_update(
                    signal_id,
                    0,
                    0,
                    60_000_000_000,
                    QualityState::Degraded,
                    &["a_63rd_reason_must_stop_the_build"],
                )],
            )
            .expect_err("unknown reason must be rejected");
        assert!(matches!(
            error,
            StreamAccountingError::UnknownInstanceStatusReason(reason)
                if reason == "a_63rd_reason_must_stop_the_build"
        ));
        assert!(harness.writer.is_empty());
        assert!(harness.instance_ledger.is_empty());
    }
}
