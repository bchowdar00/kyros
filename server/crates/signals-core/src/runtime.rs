use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One normalized output from a source adapter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceEvent {
    pub schema_version: u16,
    pub source_id: String,
    pub source_version: String,
    pub publisher_epoch: String,
    pub sequence: u64,
    pub event_time: String,
    pub received_at: String,
    pub values: BTreeMap<String, Value>,
}

/// Whether a signal update is safe to present as complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QualityState {
    Ready,
    WarmingUp,
    Degraded,
}

/// Consumer-visible readiness metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Quality {
    pub state: QualityState,
    pub reasons: Vec<String>,
}

/// One validated output from a signal processor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignalUpdate {
    pub schema_version: u16,
    pub signal_id: String,
    pub signal_version: String,
    pub publisher_epoch: String,
    pub sequence: u64,
    pub event_time: String,
    pub produced_at: String,
    pub valid_from: String,
    pub valid_until: String,
    pub quality: Quality,
    pub values: BTreeMap<String, Value>,
    pub scope: SignalScope,
}

/// One bound market a `multi_market` instance reads. The venue is carried
/// explicitly because a multi-venue instance's key spans venues, so the
/// signal-id's own segment (`multi`) cannot name it (ADR 0026).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MultiMarketLeg {
    pub venue: String,
    pub market_id: String,
    pub market_generation: u64,
}

/// The shared time window a `multi_market` instance is bound over. Both bounds
/// are RFC 3339 texts and `open` is strictly before `close` (ADR 0026).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MultiMarketWindow {
    pub open: String,
    pub close: String,
}

/// The decision/status instance scope carried on stream-v3 objects.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SignalScope {
    Global,
    Market {
        market_id: String,
        market_generation: u64,
    },
    /// Added additively in stream v3 by ADR 0026 for the `multi` venue segment.
    /// The consumer key is the full legs array plus the window, not any one leg.
    MultiMarket {
        legs: Vec<MultiMarketLeg>,
        window: MultiMarketWindow,
    },
}

/// Full machine key for one signal instance. `venue` is derived from the
/// signal-id's second segment and is therefore not stored separately.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SignalInstanceKey {
    pub signal_id: String,
    pub signal_version: String,
    pub scope: SignalScope,
}

impl SignalUpdate {
    pub fn instance_key(&self) -> SignalInstanceKey {
        SignalInstanceKey {
            signal_id: self.signal_id.clone(),
            signal_version: self.signal_version.clone(),
            scope: self.scope.clone(),
        }
    }
}

/// Consumer-visible non-ready or terminal state for one bound instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstanceStatusState {
    WarmingUp,
    Degraded,
    Unavailable,
    Invalidated,
}

/// Snapshot-carried status payload. Live status frames add the standard stream
/// envelope around this exact payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstanceStatusEntry {
    pub signal_id: String,
    pub signal_version: String,
    pub scope: SignalScope,
    pub state: InstanceStatusState,
    pub reasons: Vec<String>,
    pub effective_time: String,
    pub last_decision_sequence: Option<u64>,
}

impl InstanceStatusEntry {
    pub fn instance_key(&self) -> SignalInstanceKey {
        SignalInstanceKey {
            signal_id: self.signal_id.clone(),
            signal_version: self.signal_version.clone(),
            scope: self.scope.clone(),
        }
    }
}

/// Whether the publisher may discard intermediate signal states on this channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflationPolicy {
    None,
}

/// How a consumer applies the `values` map carried by one signal update.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValuesSemantics {
    FullReplace,
}

/// Aggregate health for the producer process, independent of any one signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProducerStatus {
    Starting,
    Ready,
    Degraded,
}

/// Why a consumer must clear any state associated with the current stream epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamResetReason {
    PublisherEpochStarted,
}

/// Prediction venue whose independently scoped market lifecycle changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PredictionMarketVenue {
    Polymarket,
    Kalshi,
}

/// Whether one prediction venue is currently safe to evaluate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MarketTransitionPhase {
    Unavailable,
    Bound,
}

/// Closed reason vocabulary for a prediction-market lifecycle transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MarketTransitionReason {
    InitialBind,
    ScheduledRollover,
    DiscoveryUnreachable,
    NextMarketUnlisted,
    InvalidDiscovery,
    CloseToNextOpenGap,
    ProviderLifecycleChange,
    RecoveryBind,
}

/// Coarse per-feed liveness status reported on the stream. Derived only from
/// admitted inputs; a producer observes and reports it but never acts on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedHealthStatus {
    /// Delivering data within its declared liveness bound.
    Live,
    /// Connected but silent beyond its liveness bound; no explicit disconnect.
    Degraded,
    /// A recovery/reconnect is in progress after a lost connection.
    Reconnecting,
    /// The connection or subscription was lost and no data has resumed.
    Down,
}

/// One feed's observed liveness health, carried in a `feed_health` frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeedHealthReport {
    pub source_id: String,
    pub venue: String,
    pub status: FeedHealthStatus,
    pub last_event_age_ms: Option<u64>,
    pub down_since: Option<String>,
    pub disconnect_count: u64,
    pub reconnect_count: u64,
    pub anomaly_velocity: u32,
}

/// One framed message on the versioned signal stream.
///
/// A signal batch is the atomic consumer-evaluation unit. The enclosing JSON
/// record is its completion boundary; consumers never apply a proper prefix of
/// `updates`. Snapshot state is anchored to the last complete decision-or-status
/// stream sequence included in the snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SignalStreamMessage {
    SignalBatch {
        schema_version: u16,
        publisher_epoch: String,
        sequence: u64,
        produced_at: String,
        conflation_policy: ConflationPolicy,
        values_semantics: ValuesSemantics,
        updates: Vec<SignalUpdate>,
    },
    Snapshot {
        schema_version: u16,
        publisher_epoch: String,
        sequence: u64,
        produced_at: String,
        as_of_sequence: u64,
        conflation_policy: ConflationPolicy,
        values_semantics: ValuesSemantics,
        updates: Vec<SignalUpdate>,
        statuses: Vec<InstanceStatusEntry>,
    },
    InstanceStatus {
        schema_version: u16,
        publisher_epoch: String,
        sequence: u64,
        produced_at: String,
        signal_id: String,
        signal_version: String,
        scope: SignalScope,
        state: InstanceStatusState,
        reasons: Vec<String>,
        effective_time: String,
        last_decision_sequence: Option<u64>,
    },
    Heartbeat {
        schema_version: u16,
        publisher_epoch: String,
        sequence: u64,
        produced_at: String,
        heartbeat_interval_ms: u64,
        status: ProducerStatus,
        as_of_sequence: Option<u64>,
    },
    Status {
        schema_version: u16,
        publisher_epoch: String,
        sequence: u64,
        produced_at: String,
        status: ProducerStatus,
        reasons: Vec<String>,
        as_of_sequence: Option<u64>,
    },
    Reset {
        schema_version: u16,
        publisher_epoch: String,
        sequence: u64,
        produced_at: String,
        reason: StreamResetReason,
    },
    MarketTransition {
        schema_version: u16,
        publisher_epoch: String,
        sequence: u64,
        produced_at: String,
        venue: PredictionMarketVenue,
        market_generation: u64,
        previous_market_id: Option<String>,
        next_market_id: Option<String>,
        phase: MarketTransitionPhase,
        reason: MarketTransitionReason,
    },
    FeedHealth {
        schema_version: u16,
        publisher_epoch: String,
        sequence: u64,
        produced_at: String,
        feeds: Vec<FeedHealthReport>,
    },
}

impl SignalStreamMessage {
    /// Decode one known stream-message kind after discriminator dispatch.
    ///
    /// Unknown kinds are intentionally ignored so adding a later control
    /// message does not break older consumers. Known kinds remain strict and
    /// still reject missing or unknown fields through Serde.
    pub fn decode_known_json(bytes: &[u8]) -> Result<Option<Self>, serde_json::Error> {
        let value: Value = serde_json::from_slice(bytes)?;
        if let Some(kind) = value.get("type").and_then(Value::as_str)
            && !matches!(
                kind,
                "signal_batch"
                    | "snapshot"
                    | "instance_status"
                    | "heartbeat"
                    | "status"
                    | "reset"
                    | "market_transition"
                    | "feed_health"
            )
        {
            return Ok(None);
        }
        serde_json::from_value(value).map(Some)
    }

    pub fn sequence(&self) -> u64 {
        match self {
            Self::SignalBatch { sequence, .. }
            | Self::Snapshot { sequence, .. }
            | Self::InstanceStatus { sequence, .. }
            | Self::Heartbeat { sequence, .. }
            | Self::Status { sequence, .. }
            | Self::Reset { sequence, .. }
            | Self::MarketTransition { sequence, .. }
            | Self::FeedHealth { sequence, .. } => *sequence,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SignalStreamMessage;

    #[test]
    fn discriminator_dispatch_ignores_unknown_kinds_but_keeps_known_kinds_strict() {
        assert_eq!(
            SignalStreamMessage::decode_known_json(br#"{"type":"future_control"}"#)
                .expect("unknown kind dispatches"),
            None
        );
        assert!(SignalStreamMessage::decode_known_json(br#"{"type":"heartbeat"}"#).is_err());
        assert!(SignalStreamMessage::decode_known_json(br#"{"sequence":0}"#).is_err());
    }
}
