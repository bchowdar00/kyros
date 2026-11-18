//! Observe-only per-feed liveness health derived from admitted pipeline inputs.
//!
//! This layer OBSERVES the same admitted inputs the signal pipeline already
//! processes — market events, availability transitions, and the periodic health
//! tick — and projects them into a per-feed health board. It never triggers a
//! reconnect, commands recovery, or mutates feed state; recovery stays in the
//! feed adapters where it lives today (SIG-041, observe-and-escalate never
//! command).
//!
//! Because the board is a pure function of the admitted-input sequence, replay
//! of the same inputs reproduces the same health trajectory. Health is
//! therefore report-only: the admitted inputs it reads are already journaled
//! (`SIG-021`), so no new journal record and no journal-schema bump are
//! required. Derived health, like a `SignalUpdate`, is reconstructed on replay
//! rather than persisted.
//!
//! The liveness contract for each feed is derived only from fields that already
//! exist in the source definition (`expected_heartbeat_ms`). A feed whose
//! definition declares none receives a documented conservative default and is
//! marked as such in its snapshot entry.
//!
//! A feed may also carry a calibrated liveness-bound override, which takes
//! precedence over both the declared and default derivations. These overrides
//! are measured from quiet-gap distributions observed in a soak, not guessed:
//! the current values come from the 2026-08-17 tranche-1 spot soak. Like
//! cadence, an override is runner-supplied operational metadata and never enters
//! the source-definition JSON that binds retained journal fingerprints.
//!
//! Feeds fall into two cadence classes. A `Periodic` stream (a book-levels
//! snapshot) refreshes on a venue timer, so silence beyond its bound is a real
//! fault and degrades as before. An `EventDriven` stream (trades, and the
//! tickers that publish only on trades) is legitimately quiet in a quiet market,
//! so it degrades only when its venue has no within-bound `Periodic` companion
//! at the same tick; a venue with no `Periodic` feed at all falls back to the
//! bound (conservative). Classification lives in runner code, not the source
//! definition JSON: those bytes are bound into the contract fingerprints of
//! retained journal evidence, so changing them would invalidate replay
//! validation of retained bundles. Cadence is producer-side operational
//! metadata; a future deliberate definition-schema revision may absorb it.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use serde::Serialize;
use signals_core::FeedHealthStatus;

/// Multiplier applied to a feed's declared heartbeat to obtain the age at which
/// a quiet-but-connected feed is considered degraded. Tolerating three missed
/// venue heartbeats (data-as-heartbeat, `SIG-041`) absorbs ordinary jitter
/// without masking a wedged feed engine that still answers transport pings.
pub const MISSED_HEARTBEAT_ALLOWANCE: u64 = 3;

/// Conservative liveness bound, in milliseconds, for a feed whose source
/// definition declares no `expected_heartbeat_ms`. Metadata/snapshot feeds and
/// the prediction-market venues currently omit the field; thirty seconds is long
/// enough not to false-alarm on a legitimately low-cadence feed while still
/// catching a truly dead one. Reported explicitly via `LivenessBoundSource`.
pub const DEFAULT_LIVENESS_BOUND_MS: u64 = 30_000;

/// A feed continuously sick (degraded, reconnecting, or down) for at least this
/// long, in milliseconds, escalates once to a report-level sustained-sickness
/// notice.
pub const ESCALATION_SUSTAINED_SICK_MS: u64 = 30_000;

/// Window, in milliseconds, over which abnormal availability events are counted
/// for the reconnect-storm escalation and the reported anomaly velocity.
pub const ANOMALY_WINDOW_MS: u64 = 60_000;

/// Number of abnormal availability events within `ANOMALY_WINDOW_MS` that
/// escalates once to a report-level reconnect-storm notice.
pub const RECONNECT_STORM_THRESHOLD: u32 = 5;

/// Bounded periodic cadence, in nanoseconds, for observe-only `feed_health`
/// stream frames. It matches the 60-second signal-snapshot cadence so an
/// idle-but-healthy board still refreshes without emitting on every admitted
/// event. Emission is otherwise transition-driven.
pub const FEED_HEALTH_PERIODIC_INTERVAL_NS: u64 = 60 * 1_000_000_000;

/// Upper bound on retained abnormal-event timestamps per feed.
const MAX_ANOMALY_SAMPLES: usize = 256;

/// Upper bound on retained escalation notices for the whole board.
const MAX_ESCALATIONS: usize = 4_096;

const NS_PER_MS: u64 = 1_000_000;

/// Where a feed's liveness bound came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LivenessBoundSource {
    /// Derived from the source definition's declared `expected_heartbeat_ms`.
    Declared,
    /// Conservative default because the source definition declares none.
    Default,
    /// A runner-supplied override calibrated from measured quiet-gap
    /// distributions, taking precedence over the declared and default paths.
    Calibrated,
}

/// Abstracted availability transition the board observes. Each runner maps its
/// concrete unavailable-reason vocabulary onto these so the board stays
/// independent of any one venue's reason enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedAvailability {
    /// The feed connection or subscription was lost.
    Down,
    /// A recovery/reconnect for the feed is under way.
    Reconnecting,
}

/// How a feed produces data, governing the quiet-degrade path. Deliberately not
/// `Serialize`: cadence is producer-side operational metadata and must never
/// enter the stream or report output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedCadence {
    /// Refreshes on a venue timer; silence beyond its bound is a real fault.
    Periodic,
    /// Publishes only on market activity; quiet is expected in a quiet market.
    EventDriven,
}

/// One feed's declared liveness contract, supplied at board construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedContract {
    /// The feed's `source_id`, matching the events it emits.
    pub source_id: String,
    /// A stable venue label; availability transitions are keyed on it.
    pub venue: String,
    /// The source definition's `expected_heartbeat_ms`, if declared.
    pub expected_heartbeat_ms: Option<u64>,
    /// Whether the feed is periodic or event-driven, set by the runner.
    pub cadence: FeedCadence,
    /// A calibrated liveness bound, in milliseconds, that overrides the
    /// declared/default derivation when set. Runner-supplied from soak data.
    pub liveness_bound_override_ms: Option<u64>,
}

/// A distinct sustained-sickness category surfaced in the run report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EscalationKind {
    /// A feed stayed sick beyond `ESCALATION_SUSTAINED_SICK_MS`.
    SustainedSickness,
    /// Abnormal availability events crossed `RECONNECT_STORM_THRESHOLD`
    /// within `ANOMALY_WINDOW_MS`.
    ReconnectStorm,
}

/// A marked report-level notice. Emitting it does nothing beyond recording it;
/// the board never acts on an escalation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FeedEscalation {
    pub source_id: String,
    pub kind: EscalationKind,
    pub status: FeedHealthStatus,
    pub raised_at_ns: u64,
    pub detail: String,
}

/// Machine-readable per-feed health, one entry per tracked feed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FeedHealthEntry {
    pub source_id: String,
    pub venue: String,
    pub status: FeedHealthStatus,
    pub liveness_bound_ms: u64,
    pub liveness_bound_source: LivenessBoundSource,
    pub last_event_age_ms: Option<u64>,
    pub down_since_ns: Option<u64>,
    pub disconnect_count: u64,
    pub reconnect_count: u64,
    pub anomaly_velocity: u32,
    /// Report-only count of quiet-degrades this feed avoided because a within-bound
    /// `Periodic` companion on its venue was alive. Appended last so the existing
    /// report field order is undisturbed; it never reaches the stream shape.
    pub quiet_suppressed_ticks: u64,
}

/// A structured health snapshot taken at the current observation clock.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FeedHealthSnapshot {
    pub taken_at_ns: u64,
    pub feeds: Vec<FeedHealthEntry>,
    pub escalations: Vec<FeedEscalation>,
    pub escalations_dropped: u64,
    /// Per-source count of events the lane observed on a declared stream but does
    /// not consume (F1): a source produced by one of the lane's streams that no
    /// identity reads, routed past the processor's signal computation and counted
    /// here instead of aborting the lane. Empty for a feed-only lane. It is
    /// report/telemetry only and never reaches the observe-only `feed_health`
    /// stream frame (whose projection is unchanged).
    pub ignored_events: std::collections::BTreeMap<String, u64>,
    /// Count of USDT/USD reference frames rejected as unusable (a crossed,
    /// non-positive, or otherwise-unparseable quote) (G1). A rejected reference
    /// frame keeps the prior rate and is telemetry only — never a lane error, so
    /// one locked-book tick cannot take the venue's 18 identities down. Report
    /// only; not in the stream frame.
    pub reference_frames_rejected: u64,
    /// Count of departed-market final settlement decisions (ruling E37c) that
    /// expired unpublished because the completing settlement-window frame never
    /// arrived within the retention window. Telemetry only; not in the stream
    /// frame.
    pub final_decisions_expired: u64,
}

#[derive(Debug, Clone)]
struct FeedState {
    source_id: String,
    venue: String,
    cadence: FeedCadence,
    liveness_bound_ns: u64,
    liveness_bound_ms: u64,
    liveness_bound_source: LivenessBoundSource,
    status: FeedHealthStatus,
    last_event_ns: Option<u64>,
    down_since_ns: Option<u64>,
    sick_since_ns: Option<u64>,
    disconnect_count: u64,
    reconnect_count: u64,
    quiet_suppressed_ticks: u64,
    anomalies: VecDeque<u64>,
    sustained_escalated: bool,
    storm_escalated: bool,
}

impl FeedState {
    fn record_anomaly(&mut self, now_ns: u64, window_ns: u64) {
        self.anomalies.push_back(now_ns);
        while let Some(&front) = self.anomalies.front() {
            if now_ns.saturating_sub(front) > window_ns {
                self.anomalies.pop_front();
            } else {
                break;
            }
        }
        while self.anomalies.len() > MAX_ANOMALY_SAMPLES {
            self.anomalies.pop_front();
        }
    }

    fn anomaly_velocity(&self, now_ns: u64, window_ns: u64) -> u32 {
        let count = self
            .anomalies
            .iter()
            .filter(|&&stamp| now_ns.saturating_sub(stamp) <= window_ns)
            .count();
        u32::try_from(count).unwrap_or(u32::MAX)
    }

    /// Records a delivering feed. Returns whether the status changed.
    fn become_live(&mut self, now_ns: u64) -> bool {
        self.last_event_ns = Some(now_ns);
        if self.status != FeedHealthStatus::Live {
            self.status = FeedHealthStatus::Live;
            self.down_since_ns = None;
            self.sick_since_ns = None;
            self.sustained_escalated = false;
            self.storm_escalated = false;
            return true;
        }
        false
    }

    fn mark_sick_since(&mut self, now_ns: u64) {
        if self.sick_since_ns.is_none() {
            self.sick_since_ns = Some(now_ns);
            self.sustained_escalated = false;
        }
    }
}

/// Central observe-only health board shared by every runner.
pub struct FeedHealthBoard {
    feeds: BTreeMap<String, FeedState>,
    last_now_ns: u64,
    escalations: VecDeque<FeedEscalation>,
    escalations_dropped: u64,
    /// Monotonic counter bumped on every reportable change — any feed status
    /// transition or newly raised escalation. A stream emitter compares this
    /// scalar instead of allocating a per-feed signature on the hot path.
    revision: u64,
    sustained_ns: u64,
    window_ns: u64,
    storm_threshold: u32,
    /// Per-source count of observed-not-consumed events routed past the processor
    /// (F1). Keyed by `source_id`; each maps 1:1 to the declared stream producing
    /// it.
    ignored_events: BTreeMap<String, u64>,
    /// Count of rejected USDT/USD reference frames (G1).
    reference_frames_rejected: u64,
    /// Count of expired E37c pending final decisions.
    final_decisions_expired: u64,
}

impl FeedHealthBoard {
    /// Build a board from each feed's declared liveness contract. Every feed
    /// starts `Down` since `start_ns` (it has not been observed live yet); the
    /// first market event promotes it to `Live`.
    pub fn new(contracts: impl IntoIterator<Item = FeedContract>, start_ns: u64) -> Self {
        let mut feeds = BTreeMap::new();
        for contract in contracts {
            // Override (when Some) → declared × allowance → default.
            let (bound_ms, bound_source) = match contract.liveness_bound_override_ms {
                Some(override_ms) => (override_ms, LivenessBoundSource::Calibrated),
                None => match contract.expected_heartbeat_ms {
                    Some(heartbeat_ms) => (
                        heartbeat_ms.saturating_mul(MISSED_HEARTBEAT_ALLOWANCE),
                        LivenessBoundSource::Declared,
                    ),
                    None => (DEFAULT_LIVENESS_BOUND_MS, LivenessBoundSource::Default),
                },
            };
            let state = FeedState {
                source_id: contract.source_id.clone(),
                venue: contract.venue,
                cadence: contract.cadence,
                liveness_bound_ns: bound_ms.saturating_mul(NS_PER_MS),
                liveness_bound_ms: bound_ms,
                liveness_bound_source: bound_source,
                status: FeedHealthStatus::Down,
                last_event_ns: None,
                down_since_ns: Some(start_ns),
                sick_since_ns: Some(start_ns),
                disconnect_count: 0,
                reconnect_count: 0,
                quiet_suppressed_ticks: 0,
                anomalies: VecDeque::new(),
                sustained_escalated: false,
                storm_escalated: false,
            };
            feeds.insert(contract.source_id, state);
        }
        Self {
            feeds,
            last_now_ns: start_ns,
            escalations: VecDeque::new(),
            escalations_dropped: 0,
            revision: 0,
            sustained_ns: ESCALATION_SUSTAINED_SICK_MS.saturating_mul(NS_PER_MS),
            window_ns: ANOMALY_WINDOW_MS.saturating_mul(NS_PER_MS),
            storm_threshold: RECONNECT_STORM_THRESHOLD,
            ignored_events: BTreeMap::new(),
            reference_frames_rejected: 0,
            final_decisions_expired: 0,
        }
    }

    /// Record one rejected USDT/USD reference frame (G1): a crossed/non-positive/
    /// unusable quote that leaves the prior rate in place. Telemetry only; the
    /// reference feed's liveness is unaffected (the frame still arrived).
    pub fn note_reference_rejected(&mut self, now_ns: u64) {
        self.advance_clock(now_ns);
        self.reference_frames_rejected = self.reference_frames_rejected.saturating_add(1);
    }

    /// Record `count` departed-market final settlement decisions (ruling E37c)
    /// that expired unpublished. Telemetry only; a lapsed slot is a defined
    /// no-op (nothing was owed), never a lane error. `now_ns` only advances the
    /// observation clock.
    pub fn note_final_decisions_expired(&mut self, count: u64, now_ns: u64) {
        self.advance_clock(now_ns);
        self.final_decisions_expired = self.final_decisions_expired.saturating_add(count);
    }

    /// Record one observed-not-consumed event (F1): a source a declared stream
    /// produces that no identity reads. It is counted here and NOT admitted as a
    /// live feed, so it never enters producer readiness. `now_ns` only advances
    /// the observation clock.
    pub fn note_ignored(&mut self, source_id: &str, now_ns: u64) {
        self.advance_clock(now_ns);
        *self.ignored_events.entry(source_id.to_owned()).or_insert(0) += 1;
    }

    fn bump_revision(&mut self) {
        self.revision = self.revision.saturating_add(1);
    }

    fn advance_clock(&mut self, now_ns: u64) {
        if now_ns > self.last_now_ns {
            self.last_now_ns = now_ns;
        }
    }

    /// A market event for `source_id` proves the feed is delivering: it becomes
    /// `Live` and its last-event age resets.
    pub fn observe_event(&mut self, source_id: &str, now_ns: u64) {
        self.advance_clock(now_ns);
        let changed = self
            .feeds
            .get_mut(source_id)
            .is_some_and(|feed| feed.become_live(now_ns));
        if changed {
            self.bump_revision();
        }
    }

    /// An availability transition for `venue` applies to every feed of that
    /// venue. `Down` marks a lost connection/subscription; `Reconnecting` marks
    /// an in-progress recovery. `detail` records the concrete provider reason
    /// for the report without coupling the board to any reason enum.
    pub fn observe_availability(
        &mut self,
        venue: &str,
        availability: FeedAvailability,
        detail: &str,
        now_ns: u64,
    ) {
        self.advance_clock(now_ns);
        let window_ns = self.window_ns;
        let storm_threshold = self.storm_threshold;
        let affected: Vec<String> = self
            .feeds
            .values()
            .filter(|feed| feed.venue == venue)
            .map(|feed| feed.source_id.clone())
            .collect();

        let mut pending = Vec::new();
        let mut changed = false;
        for source_id in affected {
            let Some(feed) = self.feeds.get_mut(&source_id) else {
                continue;
            };
            let prior_status = feed.status;
            feed.mark_sick_since(now_ns);
            match availability {
                FeedAvailability::Down => {
                    if feed.down_since_ns.is_none() {
                        feed.down_since_ns = Some(now_ns);
                    }
                    feed.status = FeedHealthStatus::Down;
                    feed.disconnect_count = feed.disconnect_count.saturating_add(1);
                }
                FeedAvailability::Reconnecting => {
                    if feed.down_since_ns.is_none() {
                        feed.down_since_ns = Some(now_ns);
                    }
                    feed.status = FeedHealthStatus::Reconnecting;
                    feed.reconnect_count = feed.reconnect_count.saturating_add(1);
                }
            }
            if feed.status != prior_status {
                changed = true;
            }
            feed.record_anomaly(now_ns, window_ns);
            if !feed.storm_escalated && feed.anomaly_velocity(now_ns, window_ns) >= storm_threshold
            {
                feed.storm_escalated = true;
                pending.push(FeedEscalation {
                    source_id: feed.source_id.clone(),
                    kind: EscalationKind::ReconnectStorm,
                    status: feed.status,
                    raised_at_ns: now_ns,
                    detail: format!(
                        "{}: {} abnormal availability events within {} ms ({})",
                        feed.source_id,
                        feed.anomaly_velocity(now_ns, window_ns),
                        ANOMALY_WINDOW_MS,
                        detail
                    ),
                });
            }
        }
        if changed || !pending.is_empty() {
            self.bump_revision();
        }
        self.push_escalations(pending);
    }

    /// A successful socket (re)connection for `venue` returns every feed of that
    /// venue to healthy. A connect is a genuine positive liveness signal — the
    /// transport is up and a subscription snapshot is imminent — so it clears the
    /// down/reconnecting and sustained-sickness state exactly like a delivering
    /// event, resetting the data-age clock to `now_ns` so the feed gets its full
    /// liveness bound to produce data before it can degrade again. It never sets a
    /// feed sick, so it cannot break the rollover-is-never-sick invariant.
    pub fn observe_connected(&mut self, venue: &str, now_ns: u64) {
        self.advance_clock(now_ns);
        let affected: Vec<String> = self
            .feeds
            .values()
            .filter(|feed| feed.venue == venue)
            .map(|feed| feed.source_id.clone())
            .collect();
        let mut changed = false;
        for source_id in affected {
            if let Some(feed) = self.feeds.get_mut(&source_id) {
                // Some adapters expose only disconnected/connected boundaries,
                // with no synthetic "reconnecting" notice. Count that direct
                // recovery exactly once, but never count the initial connection
                // from the board's fail-closed starting state.
                if feed.status == FeedHealthStatus::Down
                    && feed.disconnect_count > feed.reconnect_count
                {
                    feed.reconnect_count = feed.reconnect_count.saturating_add(1);
                }
                if feed.become_live(now_ns) {
                    changed = true;
                }
            }
        }
        if changed {
            self.bump_revision();
        }
    }

    /// Advance time on the periodic maintenance path: age each feed against its
    /// liveness bound (a connected feed silent beyond the bound degrades) and
    /// raise a one-shot sustained-sickness escalation for any feed sick past the
    /// threshold.
    pub fn observe_tick(&mut self, now_ns: u64) {
        self.advance_clock(now_ns);
        let sustained_ns = self.sustained_ns;
        // Companion check: resolve, before any mutation this tick, which venues
        // have a `Periodic` feed that is demonstrably alive (`Live` and within its
        // own bound). Aliveness is judged on the raw age so the outcome does not
        // depend on the order feeds are visited below. An `EventDriven` feed only
        // suppresses its quiet-degrade when its venue is in this set; both a venue
        // with no `Periodic` feed and one whose `Periodic` feeds are all past bound
        // are absent from it and so fall through to the unchanged degrade path.
        let mut venues_with_live_periodic: BTreeSet<String> = BTreeSet::new();
        for feed in self.feeds.values() {
            if feed.cadence == FeedCadence::Periodic
                && feed.status == FeedHealthStatus::Live
                && feed.last_event_ns.is_some_and(|last_event_ns| {
                    now_ns.saturating_sub(last_event_ns) <= feed.liveness_bound_ns
                })
            {
                venues_with_live_periodic.insert(feed.venue.clone());
            }
        }
        let mut pending = Vec::new();
        let mut changed = false;
        for feed in self.feeds.values_mut() {
            if feed.status == FeedHealthStatus::Live
                && let Some(last_event_ns) = feed.last_event_ns
            {
                let age_ns = now_ns.saturating_sub(last_event_ns);
                if age_ns > feed.liveness_bound_ns {
                    if feed.cadence == FeedCadence::EventDriven
                        && venues_with_live_periodic.contains(&feed.venue)
                    {
                        // One increment per feed per tick; overflowing u64 is
                        // unreachable. Saturating matches the sibling counters.
                        feed.quiet_suppressed_ticks = feed.quiet_suppressed_ticks.saturating_add(1);
                    } else {
                        feed.status = FeedHealthStatus::Degraded;
                        feed.mark_sick_since(now_ns);
                        changed = true;
                    }
                }
            }
            if let Some(sick_since_ns) = feed.sick_since_ns
                && !feed.sustained_escalated
                && now_ns.saturating_sub(sick_since_ns) >= sustained_ns
            {
                feed.sustained_escalated = true;
                pending.push(FeedEscalation {
                    source_id: feed.source_id.clone(),
                    kind: EscalationKind::SustainedSickness,
                    status: feed.status,
                    raised_at_ns: now_ns,
                    detail: format!(
                        "{}: {:?} for {} ms",
                        feed.source_id,
                        feed.status,
                        now_ns.saturating_sub(sick_since_ns) / NS_PER_MS
                    ),
                });
            }
        }
        if changed || !pending.is_empty() {
            self.bump_revision();
        }
        self.push_escalations(pending);
    }

    fn push_escalations(&mut self, pending: Vec<FeedEscalation>) {
        for escalation in pending {
            self.escalations.push_back(escalation);
            while self.escalations.len() > MAX_ESCALATIONS {
                self.escalations.pop_front();
                self.escalations_dropped = self.escalations_dropped.saturating_add(1);
            }
        }
    }

    /// The current status of one tracked feed, or `None` if it is not tracked.
    pub fn status(&self, source_id: &str) -> Option<FeedHealthStatus> {
        self.feeds.get(source_id).map(|feed| feed.status)
    }

    /// A monotonic revision that advances exactly on a reportable change (a feed
    /// status transition or a newly raised escalation). A stream emitter compares
    /// this scalar to decide whether to send a frame, with no per-event
    /// allocation.
    pub fn emission_revision(&self) -> u64 {
        self.revision
    }

    /// A structured snapshot taken at the latest observation clock.
    pub fn snapshot(&self) -> FeedHealthSnapshot {
        let now_ns = self.last_now_ns;
        let feeds = self
            .feeds
            .values()
            .map(|feed| FeedHealthEntry {
                source_id: feed.source_id.clone(),
                venue: feed.venue.clone(),
                status: feed.status,
                liveness_bound_ms: feed.liveness_bound_ms,
                liveness_bound_source: feed.liveness_bound_source,
                last_event_age_ms: feed
                    .last_event_ns
                    .map(|last_event_ns| now_ns.saturating_sub(last_event_ns) / NS_PER_MS),
                down_since_ns: feed.down_since_ns,
                disconnect_count: feed.disconnect_count,
                reconnect_count: feed.reconnect_count,
                anomaly_velocity: feed.anomaly_velocity(now_ns, self.window_ns),
                quiet_suppressed_ticks: feed.quiet_suppressed_ticks,
            })
            .collect();
        FeedHealthSnapshot {
            taken_at_ns: now_ns,
            feeds,
            escalations: self.escalations.iter().cloned().collect(),
            escalations_dropped: self.escalations_dropped,
            ignored_events: self.ignored_events.clone(),
            reference_frames_rejected: self.reference_frames_rejected,
            final_decisions_expired: self.final_decisions_expired,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEARTBEAT_MS: u64 = 1_000;
    // Declared bound = 1000 ms * 3 = 3000 ms.
    const BOUND_NS: u64 = HEARTBEAT_MS * MISSED_HEARTBEAT_ALLOWANCE * NS_PER_MS;
    const SUSTAINED_NS: u64 = ESCALATION_SUSTAINED_SICK_MS * NS_PER_MS;
    const WINDOW_NS: u64 = ANOMALY_WINDOW_MS * NS_PER_MS;

    fn board() -> FeedHealthBoard {
        FeedHealthBoard::new(
            [
                FeedContract {
                    source_id: "coinbase.exchange.btc-usd.ticker".to_owned(),
                    venue: "coinbase".to_owned(),
                    expected_heartbeat_ms: Some(HEARTBEAT_MS),
                    cadence: FeedCadence::EventDriven,
                    liveness_bound_override_ms: None,
                },
                FeedContract {
                    source_id: "kraken.spot.btc-usd.ticker".to_owned(),
                    venue: "kraken".to_owned(),
                    expected_heartbeat_ms: Some(HEARTBEAT_MS),
                    cadence: FeedCadence::EventDriven,
                    liveness_bound_override_ms: None,
                },
            ],
            0,
        )
    }

    fn status(board: &FeedHealthBoard, id: &str) -> FeedHealthStatus {
        board.status(id).expect("feed is tracked")
    }

    fn feed(board: &FeedHealthBoard, id: &str) -> FeedHealthEntry {
        board
            .snapshot()
            .feeds
            .into_iter()
            .find(|feed| feed.source_id == id)
            .expect("feed is tracked")
    }

    const TRADES_ID: &str = "kraken.spot.btc-usd.trades";
    const BOOK_ID: &str = "kraken.spot.btc-usd.book-levels";

    /// A venue with an event-driven trades feed and its periodic book companion.
    fn cadence_board() -> FeedHealthBoard {
        FeedHealthBoard::new(
            [
                FeedContract {
                    source_id: TRADES_ID.to_owned(),
                    venue: "kraken".to_owned(),
                    expected_heartbeat_ms: Some(HEARTBEAT_MS),
                    cadence: FeedCadence::EventDriven,
                    liveness_bound_override_ms: None,
                },
                FeedContract {
                    source_id: BOOK_ID.to_owned(),
                    venue: "kraken".to_owned(),
                    expected_heartbeat_ms: Some(HEARTBEAT_MS),
                    cadence: FeedCadence::Periodic,
                    liveness_bound_override_ms: None,
                },
            ],
            0,
        )
    }

    #[test]
    fn feeds_start_down_since_construction() {
        let board = board();
        assert_eq!(
            status(&board, "coinbase.exchange.btc-usd.ticker"),
            FeedHealthStatus::Down
        );
        let snapshot = board.snapshot();
        let entry = &snapshot.feeds[0];
        assert_eq!(entry.down_since_ns, Some(0));
        assert_eq!(entry.last_event_age_ms, None);
        assert_eq!(entry.disconnect_count, 0);
    }

    #[test]
    fn declared_bound_multiplies_heartbeat_and_default_bound_is_used_when_absent() {
        let board = FeedHealthBoard::new(
            [
                FeedContract {
                    source_id: "declared".to_owned(),
                    venue: "v".to_owned(),
                    expected_heartbeat_ms: Some(HEARTBEAT_MS),
                    cadence: FeedCadence::Periodic,
                    liveness_bound_override_ms: None,
                },
                FeedContract {
                    source_id: "absent".to_owned(),
                    venue: "v".to_owned(),
                    expected_heartbeat_ms: None,
                    cadence: FeedCadence::Periodic,
                    liveness_bound_override_ms: None,
                },
            ],
            0,
        );
        let snapshot = board.snapshot();
        let declared = snapshot
            .feeds
            .iter()
            .find(|feed| feed.source_id == "declared")
            .expect("declared feed");
        let absent = snapshot
            .feeds
            .iter()
            .find(|feed| feed.source_id == "absent")
            .expect("absent feed");
        assert_eq!(
            declared.liveness_bound_ms,
            HEARTBEAT_MS * MISSED_HEARTBEAT_ALLOWANCE
        );
        assert_eq!(
            declared.liveness_bound_source,
            LivenessBoundSource::Declared
        );
        assert_eq!(absent.liveness_bound_ms, DEFAULT_LIVENESS_BOUND_MS);
        assert_eq!(absent.liveness_bound_source, LivenessBoundSource::Default);
    }

    #[test]
    fn edge_down_to_live_on_market_event() {
        let mut board = board();
        board.observe_event("coinbase.exchange.btc-usd.ticker", 500);
        assert_eq!(
            status(&board, "coinbase.exchange.btc-usd.ticker"),
            FeedHealthStatus::Live
        );
        let snapshot = board.snapshot();
        let entry = &snapshot.feeds[0];
        assert_eq!(entry.down_since_ns, None);
        assert_eq!(entry.last_event_age_ms, Some(0));
    }

    #[test]
    fn edge_live_to_degraded_on_age_then_back_to_live_on_event() {
        let mut board = board();
        let id = "coinbase.exchange.btc-usd.ticker";
        board.observe_event(id, 1_000);
        // Tick within the bound keeps it live.
        board.observe_tick(1_000 + BOUND_NS);
        assert_eq!(status(&board, id), FeedHealthStatus::Live);
        // Tick beyond the bound degrades it.
        board.observe_tick(1_000 + BOUND_NS + 1);
        assert_eq!(status(&board, id), FeedHealthStatus::Degraded);
        // A fresh event restores it.
        board.observe_event(id, 1_000 + BOUND_NS + 2);
        assert_eq!(status(&board, id), FeedHealthStatus::Live);
    }

    #[test]
    fn edge_availability_down_and_reconnecting_and_recovery() {
        let mut board = board();
        let id = "coinbase.exchange.btc-usd.ticker";
        board.observe_event(id, 100);
        assert_eq!(status(&board, id), FeedHealthStatus::Live);

        board.observe_availability("coinbase", FeedAvailability::Down, "disconnected", 200);
        assert_eq!(status(&board, id), FeedHealthStatus::Down);
        assert_eq!(board.snapshot().feeds[0].disconnect_count, 1);
        assert_eq!(board.snapshot().feeds[0].down_since_ns, Some(200));

        board.observe_availability("coinbase", FeedAvailability::Reconnecting, "recovery", 300);
        assert_eq!(status(&board, id), FeedHealthStatus::Reconnecting);
        assert_eq!(board.snapshot().feeds[0].reconnect_count, 1);
        // Down-since is retained across the reconnecting phase.
        assert_eq!(board.snapshot().feeds[0].down_since_ns, Some(200));

        board.observe_event(id, 400);
        assert_eq!(status(&board, id), FeedHealthStatus::Live);
        assert_eq!(board.snapshot().feeds[0].down_since_ns, None);
    }

    #[test]
    fn observe_connected_restores_live_after_a_socket_outage() {
        let mut board = board();
        let id = "coinbase.exchange.btc-usd.ticker";
        board.observe_event(id, 100);
        board.observe_availability(
            "coinbase",
            FeedAvailability::Down,
            "socket_disconnected",
            200,
        );
        board.observe_availability(
            "coinbase",
            FeedAvailability::Reconnecting,
            "socket_reconnecting",
            300,
        );
        assert_eq!(status(&board, id), FeedHealthStatus::Reconnecting);
        // A successful reconnect for the venue returns every feed of it to live
        // and clears the outage state, resetting the data-age clock to the connect.
        board.observe_connected("coinbase", 400);
        assert_eq!(status(&board, id), FeedHealthStatus::Live);
        let entry = &board.snapshot().feeds[0];
        assert_eq!(entry.down_since_ns, None);
        assert_eq!(entry.last_event_age_ms, Some(0));
        // The other venue is untouched by a connect it did not experience.
        assert_eq!(
            status(&board, "kraken.spot.btc-usd.ticker"),
            FeedHealthStatus::Down
        );
    }

    #[test]
    fn direct_disconnected_to_connected_counts_one_recovery_not_initial_connect() {
        let mut board = board();
        let id = "coinbase.exchange.btc-usd.ticker";
        board.observe_connected("coinbase", 100);
        assert_eq!(
            board
                .snapshot()
                .feeds
                .into_iter()
                .find(|feed| feed.source_id == id)
                .expect("coinbase feed")
                .reconnect_count,
            0
        );

        board.observe_availability(
            "coinbase",
            FeedAvailability::Down,
            "socket_disconnected",
            200,
        );
        board.observe_connected("coinbase", 300);
        let entry = board
            .snapshot()
            .feeds
            .into_iter()
            .find(|feed| feed.source_id == id)
            .expect("coinbase feed");
        assert_eq!(entry.disconnect_count, 1);
        assert_eq!(entry.reconnect_count, 1);
        assert_eq!(entry.status, FeedHealthStatus::Live);
    }

    #[test]
    fn availability_only_affects_its_own_venue() {
        let mut board = board();
        board.observe_event("coinbase.exchange.btc-usd.ticker", 100);
        board.observe_event("kraken.spot.btc-usd.ticker", 100);
        board.observe_availability("coinbase", FeedAvailability::Down, "disconnected", 200);
        assert_eq!(
            status(&board, "coinbase.exchange.btc-usd.ticker"),
            FeedHealthStatus::Down
        );
        assert_eq!(
            status(&board, "kraken.spot.btc-usd.ticker"),
            FeedHealthStatus::Live
        );
    }

    #[test]
    fn age_is_computed_against_the_supplied_clock() {
        let mut board = board();
        let id = "kraken.spot.btc-usd.ticker";
        board.observe_event(id, 1_000 * NS_PER_MS);
        // Advance the fake clock by 2500 ms via a tick.
        board.observe_tick(3_500 * NS_PER_MS);
        let entry = board
            .snapshot()
            .feeds
            .iter()
            .find(|feed| feed.source_id == id)
            .cloned()
            .expect("kraken feed");
        assert_eq!(entry.last_event_age_ms, Some(2_500));
    }

    #[test]
    fn sustained_sickness_escalates_once() {
        let id = "coinbase.exchange.btc-usd.ticker";
        let mut board = FeedHealthBoard::new(
            [FeedContract {
                source_id: id.to_owned(),
                venue: "coinbase".to_owned(),
                expected_heartbeat_ms: Some(HEARTBEAT_MS),
                cadence: FeedCadence::EventDriven,
                liveness_bound_override_ms: None,
            }],
            0,
        );
        board.observe_event(id, 0);
        board.observe_availability("coinbase", FeedAvailability::Down, "disconnected", 1_000);
        // Not yet sustained.
        board.observe_tick(1_000 + SUSTAINED_NS - 1);
        assert!(board.snapshot().escalations.is_empty());
        // Crosses the sustained threshold: one escalation.
        board.observe_tick(1_000 + SUSTAINED_NS);
        let escalations = board.snapshot().escalations;
        assert_eq!(escalations.len(), 1);
        assert_eq!(escalations[0].kind, EscalationKind::SustainedSickness);
        assert_eq!(escalations[0].status, FeedHealthStatus::Down);
        // Still sustained on the next tick, but not re-raised.
        board.observe_tick(1_000 + SUSTAINED_NS + SUSTAINED_NS);
        assert_eq!(board.snapshot().escalations.len(), 1);
    }

    #[test]
    fn reconnect_storm_escalates_on_threshold() {
        let mut board = board();
        let id = "coinbase.exchange.btc-usd.ticker";
        board.observe_event(id, 0);
        // Threshold abnormal events inside the window.
        for step in 1..=RECONNECT_STORM_THRESHOLD {
            let now = u64::from(step) * NS_PER_MS;
            board.observe_availability("coinbase", FeedAvailability::Reconnecting, "recovery", now);
        }
        let escalations = board.snapshot().escalations;
        assert_eq!(escalations.len(), 1);
        assert_eq!(escalations[0].kind, EscalationKind::ReconnectStorm);
        let entry = &board.snapshot().feeds[0];
        assert!(entry.anomaly_velocity >= RECONNECT_STORM_THRESHOLD);
    }

    #[test]
    fn anomaly_velocity_decays_outside_the_window() {
        let mut board = board();
        let id = "kraken.spot.btc-usd.ticker";
        board.observe_availability("kraken", FeedAvailability::Down, "disconnected", 1_000);
        // Immediately after, velocity counts the one event.
        board.observe_tick(1_000);
        let entry = board
            .snapshot()
            .feeds
            .iter()
            .find(|feed| feed.source_id == id)
            .cloned()
            .expect("kraken feed");
        assert_eq!(entry.anomaly_velocity, 1);
        // Well past the window, it decays to zero.
        board.observe_tick(1_000 + WINDOW_NS + NS_PER_MS);
        let entry = board
            .snapshot()
            .feeds
            .iter()
            .find(|feed| feed.source_id == id)
            .cloned()
            .expect("kraken feed");
        assert_eq!(entry.anomaly_velocity, 0);
    }

    #[test]
    fn same_admitted_events_produce_the_same_trajectory() {
        let script = |board: &mut FeedHealthBoard| {
            let cb = "coinbase.exchange.btc-usd.ticker";
            let kr = "kraken.spot.btc-usd.ticker";
            board.observe_event(cb, 100);
            board.observe_event(kr, 100);
            board.observe_tick(1_000);
            board.observe_availability("coinbase", FeedAvailability::Down, "disconnected", 2_000);
            board.observe_tick(3_000);
            board.observe_availability(
                "coinbase",
                FeedAvailability::Reconnecting,
                "recovery",
                4_000,
            );
            board.observe_event(cb, 5_000);
            board.observe_tick(6_000 + BOUND_NS);
        };
        let mut first = board();
        script(&mut first);
        let mut second = board();
        script(&mut second);
        assert_eq!(first.snapshot(), second.snapshot());
    }

    #[test]
    fn snapshot_feeds_are_ordered_by_source_id() {
        let board = board();
        let ids: Vec<String> = board
            .snapshot()
            .feeds
            .into_iter()
            .map(|feed| feed.source_id)
            .collect();
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(ids, sorted);
    }

    #[test]
    fn event_driven_quiet_is_suppressed_while_periodic_companion_is_alive() {
        let mut board = cadence_board();
        board.observe_event(TRADES_ID, 1_000);
        board.observe_event(BOOK_ID, 1_000);
        // The book refreshes again just before the tick, so it is within its bound;
        // trades stay silent past theirs.
        board.observe_event(BOOK_ID, 1_000 + BOUND_NS);
        board.observe_tick(1_000 + BOUND_NS + 1);
        assert_eq!(status(&board, TRADES_ID), FeedHealthStatus::Live);
        assert_eq!(status(&board, BOOK_ID), FeedHealthStatus::Live);
        assert_eq!(feed(&board, TRADES_ID).quiet_suppressed_ticks, 1);
    }

    #[test]
    fn event_driven_degrades_when_periodic_companion_is_also_past_bound() {
        let mut board = cadence_board();
        board.observe_event(TRADES_ID, 1_000);
        board.observe_event(BOOK_ID, 1_000);
        // Neither feed refreshes; the book is past its own bound so it is no longer
        // a live companion.
        board.observe_tick(1_000 + BOUND_NS + 1);
        assert_eq!(status(&board, TRADES_ID), FeedHealthStatus::Degraded);
        assert_eq!(status(&board, BOOK_ID), FeedHealthStatus::Degraded);
        assert_eq!(feed(&board, TRADES_ID).quiet_suppressed_ticks, 0);
    }

    #[test]
    fn event_driven_degrades_on_a_venue_with_no_periodic_feed() {
        let mut board = FeedHealthBoard::new(
            [FeedContract {
                source_id: TRADES_ID.to_owned(),
                venue: "kraken".to_owned(),
                expected_heartbeat_ms: Some(HEARTBEAT_MS),
                cadence: FeedCadence::EventDriven,
                liveness_bound_override_ms: None,
            }],
            0,
        );
        board.observe_event(TRADES_ID, 1_000);
        board.observe_tick(1_000 + BOUND_NS + 1);
        assert_eq!(status(&board, TRADES_ID), FeedHealthStatus::Degraded);
        assert_eq!(feed(&board, TRADES_ID).quiet_suppressed_ticks, 0);
    }

    #[test]
    fn periodic_feed_past_bound_degrades() {
        let mut board = FeedHealthBoard::new(
            [FeedContract {
                source_id: BOOK_ID.to_owned(),
                venue: "kraken".to_owned(),
                expected_heartbeat_ms: Some(HEARTBEAT_MS),
                cadence: FeedCadence::Periodic,
                liveness_bound_override_ms: None,
            }],
            0,
        );
        board.observe_event(BOOK_ID, 1_000);
        board.observe_tick(1_000 + BOUND_NS + 1);
        assert_eq!(status(&board, BOOK_ID), FeedHealthStatus::Degraded);
    }

    #[test]
    fn availability_down_ignores_the_companion_rule() {
        let mut board = cadence_board();
        board.observe_event(TRADES_ID, 100);
        board.observe_event(BOOK_ID, 100);
        // The companion check only guards the quiet-degrade path; a real
        // availability transition takes the event-driven feed down regardless.
        board.observe_availability("kraken", FeedAvailability::Down, "disconnected", 200);
        assert_eq!(status(&board, TRADES_ID), FeedHealthStatus::Down);
        assert_eq!(feed(&board, TRADES_ID).quiet_suppressed_ticks, 0);
    }

    #[test]
    fn suppressed_feed_recovers_age_on_a_fresh_event_without_touching_the_counter() {
        let mut board = cadence_board();
        board.observe_event(TRADES_ID, 1_000);
        board.observe_event(BOOK_ID, 1_000);
        board.observe_event(BOOK_ID, 1_000 + BOUND_NS);
        board.observe_tick(1_000 + BOUND_NS + 1);
        assert_eq!(feed(&board, TRADES_ID).quiet_suppressed_ticks, 1);
        // A fresh trade resets the age but leaves the suppression tally untouched.
        board.observe_event(TRADES_ID, 1_000 + BOUND_NS + 5);
        assert_eq!(status(&board, TRADES_ID), FeedHealthStatus::Live);
        let entry = feed(&board, TRADES_ID);
        assert_eq!(entry.last_event_age_ms, Some(0));
        assert_eq!(entry.quiet_suppressed_ticks, 1);
    }

    #[test]
    fn calibrated_override_wins_over_declared_heartbeat_and_reports_source() {
        let board = FeedHealthBoard::new(
            [FeedContract {
                source_id: "overridden".to_owned(),
                venue: "v".to_owned(),
                expected_heartbeat_ms: Some(HEARTBEAT_MS),
                cadence: FeedCadence::EventDriven,
                liveness_bound_override_ms: Some(60_000),
            }],
            0,
        );
        let entry = &board.snapshot().feeds[0];
        // The override supersedes the declared heartbeat×allowance derivation.
        assert_eq!(entry.liveness_bound_ms, 60_000);
        assert_eq!(entry.liveness_bound_source, LivenessBoundSource::Calibrated);
    }

    #[test]
    fn calibrated_periodic_feed_survives_its_publication_cadence_then_degrades() {
        const OVERRIDE_MS: u64 = 180_000;
        const CADENCE_NS: u64 = 60_000 * NS_PER_MS;
        let mut board = FeedHealthBoard::new(
            [FeedContract {
                source_id: "twap".to_owned(),
                venue: "polymarket".to_owned(),
                expected_heartbeat_ms: None,
                cadence: FeedCadence::Periodic,
                liveness_bound_override_ms: Some(OVERRIDE_MS),
            }],
            0,
        );
        let mut now = CADENCE_NS;
        board.observe_event("twap", now);
        // Three 60 s publication cycles: a tick one cadence after each event stays
        // within the 180 s override, so the feed never degrades.
        for _ in 0..3 {
            now += CADENCE_NS;
            board.observe_tick(now);
            assert_eq!(status(&board, "twap"), FeedHealthStatus::Live);
            board.observe_event("twap", now);
        }
        // Quiet past the override degrades once.
        board.observe_tick(now + OVERRIDE_MS * NS_PER_MS + 1);
        assert_eq!(status(&board, "twap"), FeedHealthStatus::Degraded);
    }

    #[test]
    fn calibrated_bound_governs_companion_aliveness() {
        const OVERRIDE_MS: u64 = 60_000;
        let mut board = FeedHealthBoard::new(
            [
                FeedContract {
                    source_id: TRADES_ID.to_owned(),
                    venue: "kraken".to_owned(),
                    expected_heartbeat_ms: Some(HEARTBEAT_MS),
                    cadence: FeedCadence::EventDriven,
                    liveness_bound_override_ms: None,
                },
                FeedContract {
                    source_id: BOOK_ID.to_owned(),
                    venue: "kraken".to_owned(),
                    expected_heartbeat_ms: Some(HEARTBEAT_MS),
                    cadence: FeedCadence::Periodic,
                    liveness_bound_override_ms: Some(OVERRIDE_MS),
                },
            ],
            0,
        );
        board.observe_event(TRADES_ID, 1_000);
        board.observe_event(BOOK_ID, 1_000);
        // A tick past the declared 3 s bound but within the book's 60 s calibrated
        // override: the book counts as a live companion on its override, so the
        // trades quiet-degrade stays suppressed. Without the override the book
        // would be past bound and the trades feed would degrade.
        board.observe_tick(1_000 + BOUND_NS + 1);
        assert_eq!(status(&board, TRADES_ID), FeedHealthStatus::Live);
        assert_eq!(status(&board, BOOK_ID), FeedHealthStatus::Live);
        assert_eq!(feed(&board, TRADES_ID).quiet_suppressed_ticks, 1);
    }
}
