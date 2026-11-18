//! One lane shell, pluggable signal processors (A6, ruling E24/E26).
//!
//! The [`LaneProcessor`] trait is the seam between the venue-agnostic lane
//! runtime ([`crate::lane_runtime`]) and the per-lane computation. Today the only
//! implementation is [`FeedOnlyProcessor`] — zero signals, `feed_health` /
//! `heartbeat` / `status` only — which every new venue lane runs until the
//! B-wave signal processors (B1/B2/B3) land. [`LaneSpec`] is the static
//! description of a lane the CLI selects with `--lane`.
//!
//! Spot stays on its own binary and stack (ruling E26): it is not a lane of this
//! shell yet and migrates onto it after the demo.

use std::collections::BTreeSet;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use btc_feeds::CaptureStream;
use btc_feeds::live::{
    ChainlinkPollOptions, ChainlinkPollSession, ChainlinkPollTransport, LiveControl, LiveError,
    ProcessClock, TimedIngestOutcome,
};
use serde::{Deserialize, Serialize};
use signals_core::{
    BINANCE_LANE, BYBIT_LANE, CROSS_LANE, FeedHealthStatus, LaneRequirement, ORACLE_LANE,
    ProducerStatus, SignalUpdate, SourceEvent,
};

/// A canonical definition document `(id, json)` a lane declares. Feed-only lanes
/// declare a synthetic source document per feed and no signal documents; a
/// future signal processor returns its real definition set here. The bytes enter
/// the lane's contract fingerprint (`lane_journal::lane_contract_fingerprint`).
pub type LaneDefinitionDocument = (String, String);

/// The source and signal definition documents a lane declares, derived from its
/// [`LaneSpec`] so the value is a pure function of the lane name — which lets the
/// contract fingerprint recompute it from a checkpoint's recorded runtime config.
/// `None` means the lane name is unknown and is refused.
pub fn lane_definition_documents(
    lane: &str,
) -> Option<(Vec<LaneDefinitionDocument>, Vec<LaneDefinitionDocument>)> {
    // The oracle lane runs a composite value-emitting processor (C1): the
    // Chainlink source and signal documents plus the Pyth Hermes source and its
    // `btc.pyth.oracle_reference` signal — a pure function, so the fingerprint
    // recomputes without a processor in hand, in the same order the composite's
    // `source_definitions()` / `signal_definitions()` concatenate them.
    if lane == "oracle" {
        let (mut sources, mut signals) = crate::oracle_signals::oracle_lane_definitions();
        let (pyth_sources, pyth_signals) = crate::oracle_signals::pyth_oracle_lane_definitions();
        sources.extend(pyth_sources);
        signals.extend(pyth_signals);
        return Some((sources, signals));
    }
    let spec = match lane {
        "binance" => &BINANCE,
        "bybit" => &BYBIT,
        "cross" => &CROSS,
        _ => return None,
    };
    // Delegate to the lane's processor so the trait's `source_definitions()` /
    // `signal_definitions()` are the single source of truth: the Binance/Bybit
    // B1 processors return their real 3-source / 13-signal spot set, so the lane
    // contract fingerprint covers them (E33); the definition set is independent
    // of the publisher epoch and wall clock passed here.
    let processor = spec
        .build_processor("lane-contract-fingerprint", chrono::DateTime::UNIX_EPOCH)
        .expect("embedded lane definitions are valid");
    Some((
        processor.source_definitions(),
        processor.signal_definitions(),
    ))
}

use crate::feed_health::{
    FeedAvailability, FeedCadence, FeedContract, FeedHealthBoard, FeedHealthSnapshot,
};
use crate::stream_accounting::ProducerHealth;

/// The default health-tick interval for a lane, in nanoseconds (1 s).
pub const LANE_HEALTH_INTERVAL_NS: u64 = 1_000_000_000;

/// The two clocks the shell owns and hands a value-emitting processor on every
/// live input and every interval tick (ADR 0027 amendment): `now_ns` is the
/// journaled logical clock the processor folds over (and coalesces against), and
/// `produced_at` is the wall-clock RFC 3339 instant the shell derives from the
/// session `wall_start` and `now_ns`. A processor stamps `produced_at` (and the
/// derived `valid_from`/`valid_until`) onto every [`SignalUpdate`] it returns; the
/// shell stamps the stream-message `sequence`. Passing both keeps the wall clock a
/// pure function of journaled state, so a replay reproduces every timestamp.
#[derive(Debug, Clone)]
pub struct LaneClock {
    /// The journaled logical clock, in nanoseconds since the publisher epoch's
    /// `wall_start`.
    pub now_ns: u64,
    /// The wall-clock decision instant, RFC 3339 with nanosecond precision.
    pub produced_at: String,
}

/// A processor plugged into the lane shell. Object-safe: the runtime holds a
/// `Box<dyn LaneProcessor>` so one shell instantiation serves every lane.
///
/// A feed-only processor returns no updates and ignores `clock.produced_at`; a
/// value-emitting processor (e.g. [`crate::oracle_signals::ChainlinkOracleProcessor`])
/// builds each [`SignalUpdate`] from the clock and coalesces on the journaled
/// `now_ns` (ADR 0027 amendment).
pub trait LaneProcessor {
    /// The signal-set id stamped into the journal session and checkpoint.
    fn signal_set_id(&self) -> &str;
    /// The signal-set version.
    fn signal_set_version(&self) -> &str;
    /// The processor identity stamped into the journal session and checkpoint
    /// (F3). It comes from the RUNNING processor, so a feed-only processor and a
    /// venue processor stamp different ids and a checkpoint written by one is
    /// refused when resumed under the other. Defaults to the signal-set id, which
    /// is what a feed-only lane already stamped.
    fn processor_id(&self) -> &str {
        self.signal_set_id()
    }
    /// The processor version stamped into the journal session and checkpoint.
    fn processor_version(&self) -> &str {
        self.signal_set_version()
    }
    /// The sources this processor routes to a handler without error, or `None`
    /// for a processor that accepts every event (the feed-only interim, whose
    /// `on_events` dispatches nothing by source). `Some(set)` turns on the lane
    /// runtime's observed-not-consumed routing: an event from a declared stream
    /// whose source is not in the set is counted as `ignored_events` and routed
    /// past the processor rather than aborting the lane (F1).
    fn consumed_sources(&self) -> Option<BTreeSet<String>> {
        None
    }
    /// The sources a declared stream must produce for a non-unavailable identity
    /// to ever reach ready. The lane runtime refuses at start-up when a required
    /// source is produced by no declared stream (F1 misconfiguration guard).
    /// Empty by default (a feed-only lane gates readiness on streams, not
    /// sources).
    fn required_sources(&self) -> BTreeSet<String> {
        BTreeSet::new()
    }
    /// Hand the processor the observed-not-consumed source set the lane runtime
    /// computed at start-up (declared-stream sources minus [`Self::consumed_sources`]).
    /// Events from these sources are counted as `ignored_events` and routed past
    /// the signal computation rather than aborting the lane (F1). The default is a
    /// no-op: a processor with no `consumed_sources` set has no observed-not-consumed
    /// routing.
    fn set_observed_not_consumed(&mut self, _sources: BTreeSet<String>) {}
    /// How many signals this processor publishes (0 for a feed-only lane).
    fn signal_count(&self) -> usize;
    /// The lane's venue tokens.
    fn venues(&self) -> &'static [&'static str];
    /// The lane's source definition documents `(id, json)`; their bytes enter the
    /// contract fingerprint. A feed-only processor returns one synthetic document
    /// per feed.
    fn source_definitions(&self) -> Vec<LaneDefinitionDocument>;
    /// The lane's signal definition documents `(id, json)`; empty for a feed-only
    /// processor.
    fn signal_definitions(&self) -> Vec<LaneDefinitionDocument>;
    /// The feed contracts backing the feed-health board.
    fn feed_contracts(&self) -> Vec<FeedContract>;

    /// Observe one stream's events (a live input). Returns any signal updates to
    /// publish (empty for a feed-only processor); ready updates the coalescing
    /// rule suppresses are omitted, non-ready evaluations are returned so the
    /// shell frames their `instance_status`.
    fn on_events(
        &mut self,
        stream: &str,
        events: &[SourceEvent],
        clock: &LaneClock,
    ) -> Result<Vec<SignalUpdate>, LaneProcessorError>;
    /// Observe a stream connectivity transition (a live input).
    fn on_connectivity(
        &mut self,
        stream: &str,
        connected: bool,
        clock: &LaneClock,
    ) -> Result<Vec<SignalUpdate>, LaneProcessorError>;
    /// Advance the processor on the lane's interval tick: a declared-interval
    /// identity refreshes its reading (staleness/age advance with the clock) and
    /// re-emits it when its interval has fallen due.
    fn on_interval(&mut self, clock: &LaneClock) -> Result<Vec<SignalUpdate>, LaneProcessorError>;

    /// Apply a C5 rotation resync batch (the rows [`Self::resync_inputs`] produced,
    /// admitted as the first records of a rotation-opened generation, and re-applied
    /// on replay via the journal's `resync` flag). Unlike [`Self::on_events`], a resync
    /// must touch ONLY the reconstructed book state and its admission cursor — NEVER
    /// the venue's aliveness / last-input clock, the feed-health board, the
    /// observed-not-consumed accounting, or any provider-clock skew sample: a rebuilt
    /// snapshot is not evidence the feed is live, so a feed silent before the boundary
    /// must still fail closed at the same instant it would in an unrotated run. Returns
    /// no updates (a resync publishes nothing; the book is ordering-skipped on the warm
    /// live process and rebuilt on a cold replay). The default is a no-op, for a lane
    /// that produces no [`Self::resync_inputs`] and so never receives a resync record.
    fn on_resync_events(
        &mut self,
        _events: &[SourceEvent],
        _clock: &LaneClock,
    ) -> Result<Vec<SignalUpdate>, LaneProcessorError> {
        Ok(Vec::new())
    }
    /// Observe an operational notice a poll driver surfaced (a start-up call or a
    /// reconnect). Journaled as evidence; a feed-only processor computes nothing
    /// from it, so the default is a no-op — which keeps the notice invisible to
    /// the output and so replay-identical (M1, ruling E39(c)).
    fn on_notice(
        &mut self,
        _stream: &str,
        _code: &str,
        _detail: &str,
        _now_ns: u64,
    ) -> Result<Vec<SignalUpdate>, LaneProcessorError> {
        Ok(Vec::new())
    }

    /// The producer health right now; the runtime writes a `status` frame from it.
    fn producer_health(&self) -> ProducerHealth;
    /// A snapshot of the feed-health board.
    fn feed_health_snapshot(&self) -> FeedHealthSnapshot;
    /// The board's emission revision, so the runtime emits `feed_health` on change.
    fn feed_health_emission_revision(&self) -> u64;

    /// The bounded resume state, as an opaque JSON value.
    fn checkpoint_state(&self) -> serde_json::Value;
    /// Restore the bounded resume state written by [`Self::checkpoint_state`].
    fn restore_state(&mut self, state: serde_json::Value) -> Result<(), LaneProcessorError>;

    /// Restart every published identity's output sequence at zero for a new file
    /// generation (C5 Design B). A rotation opens a fresh output epoch whose first
    /// ready decision must be sequence zero, so the shell calls this on the warm
    /// processor it carries across an in-place rotation boundary
    /// ([`LanePipeline::begin_generation`]). The default is a no-op — a feed-only
    /// lane publishes no signal identities and so has no decision sequence to reset.
    /// EVERY publishing processor MUST override this: it restarts each per-identity
    /// emitter's counter (a composite delegates to every sub-processor it owns) while
    /// keeping its warm feature/ring state intact. A publishing processor that omits
    /// the override silently carries a continued sequence into the new epoch and
    /// diverges from a replay of that generation.
    fn reset_output_sequences(&mut self) {}

    /// The stateful admitted inputs a C5 rotation must re-emit as the first
    /// records of a rotation-opened generation's journal, so a cold replay of
    /// that generation can rebuild the admitted state the sealed checkpoint does
    /// not carry (book snapshots the rings and OFI are folded from). Each inner
    /// `Vec` is one source's complete, already-ordered snapshot group; the shell
    /// admits it through the normal path under a fresh admission sequence, tagged
    /// `resync` at the journal-record level. On the warm live process the rows are
    /// ordering-skipped and change nothing; on a cold replay they seed the book so
    /// the next real delta is contiguous. The default is empty — a processor whose
    /// admitted state is fully checkpointed, or has no delta-reconstructed book,
    /// needs no resync (feed-only, full-image and oracle lanes).
    fn resync_inputs(&self) -> Vec<Vec<SourceEvent>> {
        Vec::new()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LaneProcessorError {
    #[error("lane processor state is invalid: {0}")]
    InvalidState(String),
    #[error("lane processor computation failed: {0}")]
    Computation(String),
}

/// The bounded checkpoint state of a feed-only processor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FeedOnlyCheckpointState {
    /// Streams observed live at checkpoint time, so a resumed lane resumes its
    /// readiness rather than restarting from `starting`.
    live_streams: Vec<String>,
    /// Whether the lane has ever reached `ready`. A resumed lane that had gone
    /// ready must report a later stream loss as `degraded`, not `starting`.
    #[serde(default)]
    ever_ready: bool,
}

/// A processor that computes no signals. It tracks per-stream connectivity and
/// emits `feed_health` / `heartbeat` / `status`, becoming `ready` once every
/// required stream is live (E26). Every new venue lane runs this until its B-wave
/// signal processor lands.
pub struct FeedOnlyProcessor {
    signal_set_id: String,
    signal_set_version: &'static str,
    venues: &'static [&'static str],
    required_streams: Vec<String>,
    contracts: Vec<FeedContract>,
    board: FeedHealthBoard,
    live_streams: BTreeSet<String>,
    ever_ready: bool,
}

impl FeedOnlyProcessor {
    /// Build a feed-only processor. `feeds` is the lane's `(stream label, venue
    /// label)` set — one feed per required stream. Readiness is "every stream
    /// live", so the board and the liveness set are keyed on the stream label.
    pub fn new(
        signal_set_id: impl Into<String>,
        signal_set_version: &'static str,
        venues: &'static [&'static str],
        feeds: &[(&str, &str)],
    ) -> Self {
        let contracts: Vec<FeedContract> = feeds
            .iter()
            .map(|(label, venue)| FeedContract {
                source_id: (*label).to_owned(),
                venue: (*venue).to_owned(),
                expected_heartbeat_ms: None,
                cadence: FeedCadence::EventDriven,
                liveness_bound_override_ms: None,
            })
            .collect();
        let required_streams = feeds.iter().map(|(label, _)| (*label).to_owned()).collect();
        let board = FeedHealthBoard::new(contracts.clone(), 0);
        Self {
            signal_set_id: signal_set_id.into(),
            signal_set_version,
            venues,
            required_streams,
            contracts,
            board,
            live_streams: BTreeSet::new(),
            ever_ready: false,
        }
    }

    /// True once every required stream is live.
    fn all_streams_live(&self) -> bool {
        !self.required_streams.is_empty()
            && self
                .required_streams
                .iter()
                .all(|stream| self.live_streams.contains(stream))
    }

    /// Record readiness after a liveness change, so a later loss reports
    /// `degraded` rather than `starting`.
    fn note_readiness(&mut self) {
        if self.all_streams_live() {
            self.ever_ready = true;
        }
    }
}

impl LaneProcessor for FeedOnlyProcessor {
    fn signal_set_id(&self) -> &str {
        &self.signal_set_id
    }

    fn signal_set_version(&self) -> &str {
        self.signal_set_version
    }

    fn signal_count(&self) -> usize {
        0
    }

    fn venues(&self) -> &'static [&'static str] {
        self.venues
    }

    fn source_definitions(&self) -> Vec<LaneDefinitionDocument> {
        // One synthetic source document per feed, derived from the contracts.
        // This is the single source of truth the fingerprint hashes (via
        // `lane_definition_documents`, which delegates here).
        self.contracts
            .iter()
            .map(|contract| {
                let document = serde_json::json!({
                    "kind": "feed_only_source",
                    "source_id": contract.source_id,
                    "venue": contract.venue,
                })
                .to_string();
                (contract.source_id.clone(), document)
            })
            .collect()
    }

    fn signal_definitions(&self) -> Vec<LaneDefinitionDocument> {
        // Feed-only lanes publish no signals; a B-wave processor returns its set.
        Vec::new()
    }

    fn feed_contracts(&self) -> Vec<FeedContract> {
        self.contracts.clone()
    }

    fn on_events(
        &mut self,
        stream: &str,
        _events: &[SourceEvent],
        clock: &LaneClock,
    ) -> Result<Vec<SignalUpdate>, LaneProcessorError> {
        // A stream that produces events is live, and its board feed is fresh.
        self.board.observe_event(stream, clock.now_ns);
        self.live_streams.insert(stream.to_owned());
        self.note_readiness();
        Ok(Vec::new())
    }

    fn on_connectivity(
        &mut self,
        stream: &str,
        connected: bool,
        clock: &LaneClock,
    ) -> Result<Vec<SignalUpdate>, LaneProcessorError> {
        if connected {
            self.board.observe_connected(stream, clock.now_ns);
            self.live_streams.insert(stream.to_owned());
            self.note_readiness();
        } else {
            self.board.observe_availability(
                stream,
                FeedAvailability::Down,
                "disconnected",
                clock.now_ns,
            );
            self.live_streams.remove(stream);
        }
        Ok(Vec::new())
    }

    fn on_interval(&mut self, clock: &LaneClock) -> Result<Vec<SignalUpdate>, LaneProcessorError> {
        self.board.observe_tick(clock.now_ns);
        Ok(Vec::new())
    }

    fn producer_health(&self) -> ProducerHealth {
        // A required stream is unhealthy either because it is not connected (absent
        // from `live_streams`) OR because it is connected-but-silent past its
        // liveness bound: the feed-health board degrades such a feed on the health
        // tick without any Disconnected notice, so `live_streams` still holds it and
        // `all_streams_live()` alone would keep reporting Ready through the whole
        // silent-socket outage. The `status` frame is the authoritative
        // producer-readiness frame (E24), so a stale feed must degrade the lane. A
        // down feed reads `{stream}_unavailable`; a connected-but-stale one reads
        // `{stream}_stale`; both are listed in required-stream order.
        let reasons: Vec<String> = self
            .required_streams
            .iter()
            .filter_map(|stream| {
                if !self.live_streams.contains(stream) {
                    Some(format!("{stream}_unavailable"))
                } else if self.board.status(stream) == Some(FeedHealthStatus::Degraded) {
                    Some(format!("{stream}_stale"))
                } else {
                    None
                }
            })
            .collect();
        if reasons.is_empty() && !self.required_streams.is_empty() {
            return ProducerHealth {
                status: ProducerStatus::Ready,
                reasons: Vec::new(),
            };
        }
        if self.ever_ready {
            // A lane that has been ready and then lost or stalled a feed is
            // degraded, not starting up again: a backwards lifecycle transition
            // would mislead a consumer. The reasons name the affected feeds.
            let mut reasons = reasons;
            if reasons.is_empty() {
                reasons.push("feed_unavailable".to_owned());
            }
            return ProducerHealth {
                status: ProducerStatus::Degraded,
                reasons,
            };
        }
        ProducerHealth {
            status: ProducerStatus::Starting,
            reasons: vec!["feeds_incomplete".to_owned()],
        }
    }

    fn feed_health_snapshot(&self) -> FeedHealthSnapshot {
        self.board.snapshot()
    }

    fn feed_health_emission_revision(&self) -> u64 {
        self.board.emission_revision()
    }

    fn checkpoint_state(&self) -> serde_json::Value {
        serde_json::to_value(FeedOnlyCheckpointState {
            live_streams: self.live_streams.iter().cloned().collect(),
            ever_ready: self.ever_ready,
        })
        .expect("feed-only checkpoint state serializes")
    }

    fn restore_state(&mut self, state: serde_json::Value) -> Result<(), LaneProcessorError> {
        let state: FeedOnlyCheckpointState = serde_json::from_value(state)
            .map_err(|error| LaneProcessorError::InvalidState(error.to_string()))?;
        // A resumed lane must not claim any feed is live until its socket
        // reconnects and reports live again: start with NO live streams (M6). The
        // readiness MEMORY is restored, so a lane that had been ready reports
        // `degraded` — not `starting` — until every feed re-confirms.
        self.live_streams.clear();
        self.ever_ready = state.ever_ready;
        Ok(())
    }
}

/// A single physical stream a lane subscribes.
#[derive(Debug, Clone, Copy)]
pub struct LaneStream {
    /// The venue token this stream belongs to.
    pub token: &'static str,
    /// The `btc-feeds` capture stream driving the socket.
    pub stream: CaptureStream,
    /// A stable per-stream label, used for the feed-health board and readiness.
    pub label: &'static str,
}

/// How a lane's feeds are driven.
pub enum LaneDriverSpec {
    /// WebSocket streams, each driven by one `run_live` worker.
    Streams(&'static [LaneStream]),
    /// A poll-driven lane (the oracle lane's `http_poll` driver). The concrete
    /// [`PollDriver`] is injected at runtime — a fixture driver today, A5's
    /// Chainlink driver at merge.
    Poll {
        /// The stream label the poll driver's feed reports under.
        label: &'static str,
        /// The venue label for the poll driver's feed.
        venue: &'static str,
    },
    /// A composite lane driven by a poll AND websocket streams concurrently
    /// (the oracle lane, C1/E37d/E48): the poll feeds the processor's consumed
    /// source exactly as the `Poll` variant does, while `streams` are driven and
    /// journaled alongside it (the Kalshi index socket and the Pyth Hermes
    /// stream). The streams' sources are observed-not-consumed until their
    /// constituents land (C1 phase 4), so they never gate the poll's readiness.
    /// `Poll`-only and `Streams`-only lanes are byte-identical — this variant
    /// adds a third path, it does not change the other two.
    Composite {
        /// The stream label the poll driver's feed reports under.
        label: &'static str,
        /// The venue label for the poll driver's feed.
        venue: &'static str,
        /// The source ids the poll driver produces. Unlike a `Streams` lane,
        /// whose produced sources come from each [`LaneStream`]'s capture-stream
        /// inventory, the poll produces through the [`PollDriver`] seam with no
        /// capture stream, so its sources are declared here. `configure_source_routing`
        /// counts them among the lane's produced sources so a processor that
        /// requires a poll source is not flagged as misconfigured.
        poll_sources: &'static [&'static str],
        /// The websocket streams driven alongside the poll.
        streams: &'static [LaneStream],
    },
}

/// The static description of one lane the CLI selects with `--lane`.
pub struct LaneSpec {
    pub requirement: &'static LaneRequirement,
    pub venues: &'static [&'static str],
    pub processor_id: &'static str,
    pub processor_version: &'static str,
    pub driver: LaneDriverSpec,
    /// Reference feeds this lane drives ALONGSIDE its own `driver` streams, whose
    /// events feed a reference component (the `UsdtReference`, E47(e)) rather than
    /// the venue's market-data path. They are driven and journaled like any
    /// stream and appear in `feed_health` so operators see the reference socket,
    /// but they NEVER enter the lane's producer readiness — the only readiness
    /// effect of the reference is `usdt_reference_stale` decided at decision time.
    /// Empty for every lane but Binance and Bybit.
    pub reference_streams: &'static [LaneStream],
    pub duration_default: u64,
}

impl LaneSpec {
    /// The feed set `(stream label, venue label)` this lane's processor tracks.
    /// Reference streams are deliberately excluded: they must never gate the
    /// feed-only processor's readiness (E47(e)).
    pub fn feeds(&self) -> Vec<(&'static str, &'static str)> {
        match &self.driver {
            LaneDriverSpec::Streams(streams) => {
                streams.iter().map(|s| (s.label, s.token)).collect()
            }
            LaneDriverSpec::Poll { label, venue } => vec![(*label, *venue)],
            // The composite lane's readiness feed is its poll; the streams are
            // observed-not-consumed (C1 phase 1) and never gate the processor,
            // so they are deliberately excluded here, exactly as the Binance and
            // Bybit reference streams are.
            LaneDriverSpec::Composite { label, venue, .. } => vec![(*label, *venue)],
        }
    }

    /// Build this lane's feed-only processor.
    pub fn feed_only_processor(&self) -> FeedOnlyProcessor {
        FeedOnlyProcessor::new(
            self.processor_id,
            self.processor_version,
            self.venues,
            &self.feeds(),
        )
    }

    /// Build this lane's runtime processor. The rights-gated Binance and Bybit
    /// venue lanes run their B1 spot-sibling processor (`btc.binance.*` /
    /// `btc.bybit.*`); the oracle lane runs the value-emitting Chainlink oracle
    /// processor (B3 identity 2). Every other lane name is a misconfiguration.
    /// `wall_start` is the produced-at baseline the venue processors derive their
    /// decision timestamps from until the shell-supplied `LaneClock` merge (E40);
    /// the oracle processor takes its wall clock from the poll driver, so it
    /// ignores this argument. For definition extraction any value serves, as the
    /// definition set does not depend on it.
    pub fn build_processor(
        &self,
        publisher_epoch: &str,
        wall_start: chrono::DateTime<chrono::Utc>,
    ) -> Result<Box<dyn LaneProcessor>, crate::spot_signals::SpotSignalError> {
        Ok(match self.requirement.lane {
            "binance" => Box::new(crate::venue_spot_signals::BinanceProcessor::new(
                publisher_epoch,
                wall_start,
            )?),
            "bybit" => Box::new(crate::venue_spot_signals::BybitProcessor::new(
                publisher_epoch,
                wall_start,
            )?),
            "oracle" => Box::new(crate::oracle_signals::OracleLaneProcessor::new(
                publisher_epoch,
            )),
            "cross" => Box::new(crate::cross_signals::CrossProcessor::new(
                publisher_epoch,
                wall_start,
            )?),
            // No wildcard fallback (F1/F0): an unknown lane name is a
            // misconfiguration, not a silent feed-only lane. A blind restart
            // cannot fix it, so it is a fatal start-up refusal (exit 78).
            other => {
                return Err(crate::spot_signals::SpotSignalError::Definition(format!(
                    "no processor is defined for lane {other}"
                )));
            }
        })
    }
}

/// The Kraken USDT/USD reference socket (E47(e)): the dedicated
/// [`CaptureStream::KrakenUsdtReference`] connection carrying only the
/// `kraken.spot.usdt-usd.ticker` frames. Driven on the Binance and Bybit lanes
/// as a reference feed so those lanes' USD-quoting identities can convert at
/// decision time. Its token is `kraken` (the reference venue), distinct from the
/// lane's own venue, and it is never a readiness-gating feed.
pub const KRAKEN_USDT_REFERENCE_STREAM: LaneStream = LaneStream {
    token: "kraken",
    stream: CaptureStream::KrakenUsdtReference,
    label: "kraken_usdt_reference",
};

/// The Binance venue lane: spot now, USD-M perp at the merge (R5).
pub const BINANCE: LaneSpec = LaneSpec {
    requirement: &BINANCE_LANE,
    venues: &["binance"],
    // F3: the lane identity names the venue processor that actually runs, not the
    // retired feed-only interim. The journal session and checkpoint are stamped
    // from the running processor's `processor_id()` (== this), so a feed-only-era
    // checkpoint (`btc.binance.feed_only`) no longer verifies on resume.
    processor_id: "btc.binance.venue",
    processor_version: "1.0.0",
    // Spot plus the USD-M perpetual on one publisher (R5). A3 splits USD-M into
    // three capture streams — the low-frequency `market` socket, the
    // high-frequency `public` socket, and the bounded REST polls. The `market`
    // socket carries the three documented-not-observed USD-M contracts (E30), so
    // the lane drops it until E30(c) settles the wire contract; the observed spot,
    // `public` and REST streams stay.
    driver: LaneDriverSpec::Streams(&[
        LaneStream {
            token: "binance",
            stream: CaptureStream::BinanceSpot,
            label: "binance_spot",
        },
        LaneStream {
            token: "binance",
            stream: CaptureStream::BinanceUsdmPublic,
            label: "binance_usdm_public",
        },
        LaneStream {
            token: "binance",
            stream: CaptureStream::BinanceUsdmRest,
            label: "binance_usdm_rest",
        },
    ]),
    reference_streams: &[KRAKEN_USDT_REFERENCE_STREAM],
    duration_default: 900,
};

/// The Bybit venue lane: spot + linear perp on one publisher (R5/R8).
pub const BYBIT: LaneSpec = LaneSpec {
    requirement: &BYBIT_LANE,
    venues: &["bybit"],
    // F3: see the Binance lane above.
    processor_id: "btc.bybit.venue",
    processor_version: "1.0.0",
    driver: LaneDriverSpec::Streams(&[
        LaneStream {
            token: "bybit",
            stream: CaptureStream::BybitSpot,
            label: "bybit_spot",
        },
        LaneStream {
            token: "bybit",
            stream: CaptureStream::BybitLinear,
            label: "bybit_linear",
        },
        LaneStream {
            token: "bybit",
            stream: CaptureStream::BybitLinearRest,
            label: "bybit_linear_rest",
        },
    ]),
    reference_streams: &[KRAKEN_USDT_REFERENCE_STREAM],
    duration_default: 900,
};

/// The oracle lane: Chainlink on-chain via the `http_poll` driver now, Pyth
/// Hermes when keyed (R5/E4). Its `venues` label is the lane's own `oracle`
/// token; its allow-list requirement is empty, so it starts unset (R7).
pub const ORACLE: LaneSpec = LaneSpec {
    requirement: &ORACLE_LANE,
    venues: &["oracle"],
    // The oracle lane publishes `btc.chainlink.oracle_reference` (B3 identity 2);
    // its processor id is the oracle signal-set id (E24: checkpoint processor id
    // = the lane's signal-set id), stamped into the journal session and checkpoint.
    processor_id: crate::oracle_signals::ORACLE_SIGNAL_SET_ID,
    processor_version: crate::oracle_signals::ORACLE_SIGNAL_SET_VERSION,
    // C1/E37d/E48: a composite lane — the Chainlink on-chain poll PLUS the Kalshi
    // index socket (CF BRTI + Pyth relay channels) and the Pyth Hermes stream, so
    // all three oracle constituents journal byte-identically in one process. The
    // poll feeds `btc.chainlink.oracle_reference` exactly as before; the two
    // streams' sources are observed-not-consumed until their constituents land
    // (C1 phase 4), so they never gate the Chainlink reading's readiness.
    driver: LaneDriverSpec::Composite {
        label: "chainlink_aggregator",
        venue: "oracle",
        poll_sources: &[crate::oracle_signals::CHAINLINK_SOURCE_ID],
        streams: &[
            LaneStream {
                token: "kalshi",
                stream: CaptureStream::KalshiIndex,
                label: "kalshi_index",
            },
            LaneStream {
                token: "pyth",
                stream: CaptureStream::PythHermesStream,
                label: "pyth_hermes",
            },
        ],
    },
    reference_streams: &[],
    duration_default: 900,
};

/// The cross lane (C1, ruling E52): it embeds `SpotReferenceProcessor` over
/// Coinbase + Kraken (the USD composite spot reference) and the Bybit linear
/// `VenueDerivativesEngine` over the raw sockets — no inter-lane bus — and
/// publishes `btc.cross.{perp_spot_basis,liquidation_vs_spot_response,derivatives_regime}`.
/// It carries NO Binance streams: under ruling E66 the cross lane is Bybit-only
/// with no Binance legs, so `liquidation_vs_spot_response` triggers on a Bybit
/// cascade alone. Bybit is the rights-gated venue on this lane's wire (R7). The
/// Kraken USDT/USD reference rides the dedicated reference socket (E47(e)) and
/// feeds only the embedded engines' USD conversion.
pub const CROSS: LaneSpec = LaneSpec {
    requirement: &CROSS_LANE,
    venues: &["bybit", "coinbase", "kraken"],
    processor_id: crate::cross_signals::CROSS_SIGNAL_SET_ID,
    processor_version: crate::cross_signals::CROSS_SIGNAL_SET_VERSION,
    driver: LaneDriverSpec::Streams(&[
        LaneStream {
            token: "coinbase",
            stream: CaptureStream::CoinbaseCore,
            label: "coinbase_core",
        },
        LaneStream {
            token: "kraken",
            stream: CaptureStream::KrakenCore,
            label: "kraken_core",
        },
        LaneStream {
            token: "bybit",
            stream: CaptureStream::BybitLinear,
            label: "bybit_linear",
        },
        LaneStream {
            token: "bybit",
            stream: CaptureStream::BybitLinearRest,
            label: "bybit_linear_rest",
        },
    ]),
    reference_streams: &[KRAKEN_USDT_REFERENCE_STREAM],
    duration_default: 900,
};

/// One operational notice a poll driver surfaces for the lane journal — the
/// evidence trail for a start-up call or a reconnect (M1). `code` is the notice
/// category and `detail` its bounded description; neither carries market data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PollNotice {
    pub code: String,
    pub detail: String,
}

/// One poll-driver step: a connectivity flag, any events read this cycle, and any
/// operational notices (start-up/reconnect) to journal.
#[derive(Debug)]
pub struct PollBatch {
    /// Whether the driver's endpoint is currently reachable.
    pub connected: bool,
    /// Events read this cycle (empty for the interim feed-only oracle lane).
    pub events: Vec<SourceEvent>,
    /// Operational notices this cycle — start-up calls and reconnect notices —
    /// journaled as evidence (empty for the fixture driver).
    pub notices: Vec<PollNotice>,
}

#[derive(Debug, thiserror::Error)]
pub enum PollDriverError {
    /// A fatal poll fault (a decoded start-up target mismatch, an internal
    /// counter/clock error): a blind restart cannot fix it (exit-78).
    #[error("poll driver failed: {0}")]
    Poll(String),
    /// Every RPC endpoint stayed down for the whole run, so start-up never
    /// verified: a transient outage a supervisor should restart (exit-75),
    /// mirroring [`run_live_poll`](btc_feeds::live::run_live_poll)'s `NoConnection`.
    #[error("poll driver saw a whole-run endpoint outage: {0}")]
    Transient(String),
}

/// The oracle lane's polling seam. A5's Chainlink JSON-RPC driver implements this
/// at merge; a fixture-backed driver ([`FixturePollDriver`]) implements it now so
/// the lane is runnable and testable before A5 lands. Object-safe: the runtime
/// holds a `Box<dyn PollDriver>`.
pub trait PollDriver {
    /// The stream label this driver's feed reports under.
    fn stream_label(&self) -> &'static str;
    /// How often to poll.
    fn poll_interval(&self) -> Duration;
    /// Poll once. `now_ns` is the lane's logical time at the poll edge.
    fn poll<'a>(
        &'a mut self,
        now_ns: u64,
    ) -> Pin<Box<dyn Future<Output = Result<PollBatch, PollDriverError>> + 'a>>;
    /// Whether the driver's feed ever verified/established during the run. The
    /// shell owns the deadline, so it — not the driver — decides a whole-run
    /// outage: on reaching the deadline, a driver that never verified exits
    /// transient (75). Defaults to `true`; only a driver that can suffer a
    /// whole-run start-up outage (the Chainlink oracle) tracks it.
    fn ever_verified(&self) -> bool {
        true
    }
}

/// A deterministic poll driver for tests and the pre-A5 interim: it reports its
/// endpoint reachable after `connect_after` polls and never yields events, so a
/// feed-only oracle lane reaches `ready` without any external dependency.
pub struct FixturePollDriver {
    label: &'static str,
    interval: Duration,
    connect_after: u32,
    polls: u32,
}

impl FixturePollDriver {
    pub fn new(label: &'static str, interval: Duration, connect_after: u32) -> Self {
        Self {
            label,
            interval,
            connect_after,
            polls: 0,
        }
    }
}

impl PollDriver for FixturePollDriver {
    fn stream_label(&self) -> &'static str {
        self.label
    }

    fn poll_interval(&self) -> Duration {
        self.interval
    }

    fn poll<'a>(
        &'a mut self,
        _now_ns: u64,
    ) -> Pin<Box<dyn Future<Output = Result<PollBatch, PollDriverError>> + 'a>> {
        Box::pin(async move {
            self.polls = self.polls.saturating_add(1);
            Ok(PollBatch {
                connected: self.polls > self.connect_after,
                events: Vec::new(),
                notices: Vec::new(),
            })
        })
    }
}

/// A5's Chainlink on-chain driver behind the [`PollDriver`] seam. It owns the
/// three-endpoint transport A5 built (start-up target verification with rotation,
/// per-request timeout, `MAX_RESPONSE_BYTES`, the round cursor across failover,
/// reconnect on faults) via a [`ChainlinkPollSession`], and the oracle lane's
/// shell drives its cadence and durability. When start-up never verifies before
/// the run deadline it returns [`PollDriverError::Transient`] so the run exits 75,
/// matching A5's `run_live_poll`. The driver's deadline equals the shell's, so it
/// reports the outage on the last tick at or after the deadline — a hair before
/// the shell's own deadline fires, which is why the whole-run outage is caught.
pub struct ChainlinkPollDriver {
    session: ChainlinkPollSession<'static>,
    label: &'static str,
    interval: Duration,
    verified: bool,
}

impl ChainlinkPollDriver {
    /// Build the production driver over the shared bounded/timed reqwest transport
    /// A5 uses. `label` is the lane's feed label; the cadence is A5's `POLL_CADENCE`.
    /// A client-build failure is transient (exit-75) exactly as in A5.
    pub fn reqwest(
        label: &'static str,
        options: ChainlinkPollOptions,
    ) -> Result<Self, PollDriverError> {
        let session =
            ChainlinkPollSession::reqwest(options, ProcessClock::start()).map_err(build_error)?;
        let interval = session.poll_interval();
        Ok(Self::from_session(label, session, interval))
    }

    /// Build the driver over an injected transport at an explicit cadence, for
    /// tests that drive the oracle lane end to end without a network.
    pub fn with_transport(
        label: &'static str,
        options: ChainlinkPollOptions,
        interval: Duration,
        transport: ChainlinkPollTransport<'static>,
    ) -> Result<Self, PollDriverError> {
        let session = ChainlinkPollSession::new(options, ProcessClock::start(), transport)
            .map_err(build_error)?;
        Ok(Self::from_session(label, session, interval))
    }

    fn from_session(
        label: &'static str,
        session: ChainlinkPollSession<'static>,
        interval: Duration,
    ) -> Self {
        Self {
            session,
            label,
            interval,
            verified: false,
        }
    }
}

/// Classify a session build failure the way A5's `classify_live` does: a transport
/// client-build failure ([`LiveError::Capture`]) is a transient a supervisor
/// restarts (exit-75); a config/contract failure is fatal (exit-78).
fn build_error(error: LiveError) -> PollDriverError {
    match error {
        LiveError::Capture(_) => PollDriverError::Transient(error.to_string()),
        other => PollDriverError::Poll(other.to_string()),
    }
}

impl PollDriver for ChainlinkPollDriver {
    fn stream_label(&self) -> &'static str {
        self.label
    }

    fn poll_interval(&self) -> Duration {
        self.interval
    }

    fn poll<'a>(
        &'a mut self,
        _now_ns: u64,
    ) -> Pin<Box<dyn Future<Output = Result<PollBatch, PollDriverError>> + 'a>> {
        Box::pin(async move {
            // The session delivers its events AND notices through this handle: the
            // lane journals both, so every start-up call (the epoch-open Connected
            // and the decimals/description replies) and every reconnect notice
            // leave a durable trail — a first-tick TargetMismatch abort still has
            // its start-up evidence (M1). The FeedOnlyProcessor computes no signals
            // from them yet, but they replay identically.
            let mut events = Vec::new();
            let mut notices: Vec<PollNotice> = Vec::new();
            let step = {
                let mut collect = |timed: TimedIngestOutcome| {
                    for notice in &timed.outcome.notices {
                        notices.push(PollNotice {
                            code: format!("{:?}", notice.code),
                            detail: notice.detail.clone(),
                        });
                    }
                    events.extend(timed.outcome.events);
                    Ok(LiveControl::Continue)
                };
                self.session
                    .poll_tick(&mut collect)
                    .await
                    .map_err(|error| PollDriverError::Poll(error.to_string()))?
            };
            if step.verified {
                self.verified = true;
            }
            // The whole-run-outage decision belongs to the shell, which owns the
            // deadline (`run_poll`): it exits transient (75) when `ever_verified`
            // is still false at the deadline. The driver reports faults as
            // `connected: false`, never a deadline error of its own.
            Ok(PollBatch {
                connected: step.connected,
                events,
                notices,
            })
        })
    }

    fn ever_verified(&self) -> bool {
        self.verified
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clk(now_ns: u64) -> LaneClock {
        LaneClock {
            now_ns,
            produced_at: "2026-09-03T00:00:00.000000000Z".to_owned(),
        }
    }

    #[test]
    fn a_feed_only_processor_transitions_starting_ready_degraded_ready() {
        let mut processor = BYBIT.feed_only_processor();
        assert_eq!(processor.signal_count(), 0);
        assert_eq!(processor.producer_health().status, ProducerStatus::Starting);
        processor
            .on_connectivity("bybit_spot", true, &clk(1))
            .expect("connectivity admits");
        // One of three streams live: still starting, never having been ready.
        assert_eq!(processor.producer_health().status, ProducerStatus::Starting);
        processor
            .on_connectivity("bybit_linear", true, &clk(2))
            .expect("connectivity admits");
        // Two of three live: still starting.
        assert_eq!(processor.producer_health().status, ProducerStatus::Starting);
        processor
            .on_connectivity("bybit_linear_rest", true, &clk(3))
            .expect("connectivity admits");
        // Every required stream live: ready with no reasons.
        let health = processor.producer_health();
        assert_eq!(health.status, ProducerStatus::Ready);
        assert!(health.reasons.is_empty());
        // A drop after ready is a DEGRADED lane naming the down feed, never a
        // backwards transition to starting (M3).
        processor
            .on_connectivity("bybit_linear", false, &clk(4))
            .expect("disconnect admits");
        let degraded = processor.producer_health();
        assert_eq!(degraded.status, ProducerStatus::Degraded);
        assert_eq!(
            degraded.reasons,
            vec!["bybit_linear_unavailable".to_owned()]
        );
        // Recovery returns it to ready.
        processor
            .on_connectivity("bybit_linear", true, &clk(5))
            .expect("reconnect admits");
        assert_eq!(processor.producer_health().status, ProducerStatus::Ready);
    }

    #[test]
    fn a_connected_but_silent_feed_degrades_the_lane() {
        // Every required stream delivers an event, so the lane is ready and the
        // board marks each feed live.
        let mut processor = BYBIT.feed_only_processor();
        for (index, stream) in ["bybit_spot", "bybit_linear", "bybit_linear_rest"]
            .into_iter()
            .enumerate()
        {
            processor
                .on_events(stream, &[], &clk((index as u64) + 1))
                .expect("events admit");
        }
        assert_eq!(processor.producer_health().status, ProducerStatus::Ready);

        // A health tick past the default 30 s liveness bound with no further
        // events degrades every silent-but-connected feed on the board. No
        // Disconnected notice arrives, so `live_streams` is untouched and
        // `all_streams_live()` stays true — the lane must nonetheless report
        // Degraded, naming each stale feed, not keep publishing Ready.
        let past_bound_ns =
            crate::feed_health::DEFAULT_LIVENESS_BOUND_MS * 1_000_000 + 1_000_000_000;
        processor
            .on_interval(&clk(past_bound_ns))
            .expect("health tick admits");
        let health = processor.producer_health();
        assert_eq!(health.status, ProducerStatus::Degraded);
        assert_eq!(
            health.reasons,
            vec![
                "bybit_spot_stale".to_owned(),
                "bybit_linear_stale".to_owned(),
                "bybit_linear_rest_stale".to_owned(),
            ]
        );

        // A fresh event on one feed clears its staleness; the other two remain
        // stale, so the lane is still degraded but names only the silent feeds.
        processor
            .on_events("bybit_spot", &[], &clk(past_bound_ns + 1))
            .expect("events admit");
        assert_eq!(
            processor.producer_health().reasons,
            vec![
                "bybit_linear_stale".to_owned(),
                "bybit_linear_rest_stale".to_owned(),
            ]
        );
    }

    #[test]
    fn a_resumed_lane_starts_degraded_until_its_feeds_reconnect() {
        // The Binance lane subscribes spot plus the two observed USD-M capture
        // streams (R5; the USD-M `market` socket is dropped as documented-not-
        // observed, E30), so every one must be live before it is ready.
        const BINANCE_STREAMS: [&str; 3] =
            ["binance_spot", "binance_usdm_public", "binance_usdm_rest"];
        // A lane that had been ready checkpoints its readiness memory.
        let mut processor = BINANCE.feed_only_processor();
        for stream in BINANCE_STREAMS {
            processor
                .on_connectivity(stream, true, &clk(1))
                .expect("connectivity admits");
        }
        assert_eq!(processor.producer_health().status, ProducerStatus::Ready);
        let state = processor.checkpoint_state();

        // On resume the lane must NOT claim a live feed it has not reconfirmed:
        // it starts `degraded` (it was ready before), never `ready` or `starting`.
        let mut restored = BINANCE.feed_only_processor();
        assert_eq!(restored.producer_health().status, ProducerStatus::Starting);
        restored.restore_state(state).expect("state restores");
        let resumed = restored.producer_health();
        assert_eq!(resumed.status, ProducerStatus::Degraded);
        // Every feed is unconfirmed on resume, named in lane-spec order.
        assert_eq!(
            resumed.reasons,
            vec![
                "binance_spot_unavailable".to_owned(),
                "binance_usdm_public_unavailable".to_owned(),
                "binance_usdm_rest_unavailable".to_owned(),
            ]
        );
        // A health tick before the sockets reconnect keeps it degraded, never ready.
        restored.on_interval(&clk(2)).expect("health tick admits");
        assert_eq!(
            restored.producer_health().status,
            ProducerStatus::Degraded,
            "a tick before reconnect must not flip a resumed lane to ready"
        );
        // Only once every feed reconnects does it return to ready.
        for stream in BINANCE_STREAMS {
            restored
                .on_connectivity(stream, true, &clk(3))
                .expect("reconnect admits");
        }
        assert_eq!(restored.producer_health().status, ProducerStatus::Ready);
    }

    #[test]
    fn a_resumed_never_ready_lane_starts_starting() {
        // A lane that never reached ready before the checkpoint resumes to
        // `starting`, not `degraded`.
        let processor = BINANCE.feed_only_processor();
        let state = processor.checkpoint_state();
        let mut restored = BINANCE.feed_only_processor();
        restored.restore_state(state).expect("state restores");
        assert_eq!(restored.producer_health().status, ProducerStatus::Starting);
    }

    #[test]
    fn the_oracle_lane_tracks_its_single_poll_feed() {
        let processor = ORACLE.feed_only_processor();
        assert_eq!(processor.venues(), &["oracle"]);
        assert_eq!(processor.feed_contracts().len(), 1);
        assert_eq!(
            processor.feed_contracts()[0].source_id,
            "chainlink_aggregator"
        );
    }
}
