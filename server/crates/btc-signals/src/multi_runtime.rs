//! The dual-venue `multi` prediction lane runtime (C1 phase 3, ruling E57).
//!
//! Composes two [`PredictionPipeline`]s in apply-only (replay) mode over one
//! shared output writer, so the Kalshi (index-disarmed) and Polymarket venue
//! frames — including their per-venue `market_transition` frames — land on one
//! merged stream. Each admission is journaled with a venue tag in admission
//! order; the two `btc.multi.*` blends are computed by [`MultiProcessor`] from the
//! venues' just-published updates (harvested from each pipeline's output through a
//! tee) and published under the lane's own publisher epoch. Because every frame
//! is a pure function of the ordered, venue-tagged input stream, a run and a
//! replay of the **gen-0** merged journal are byte-identical. A rotated
//! generation (N ≥ 1) replayed standalone is not: its header carries only each
//! leg's `active_binding`, not a per-leg processor checkpoint, so the rolling
//! ring/bar, order-book and spot-reference computations re-warm and the
//! book-derived non-ring identities (e.g. `prediction_liquidity`) re-warm with
//! them — the same re-warm the single-venue lane takes on any resume (see the
//! standalone-replay test in `multi_runtime::tests`).
//!
//! The inner pipelines are driven through `admit_replay` for BOTH run and replay
//! (E57 "apply-only mode"): the outer runtime — not the inner pipeline — owns the
//! merged journal, so the inner pipelines never journal. This keeps run and
//! replay the same code path and makes byte-identity structural.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::rc::Rc;

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use signals_core::{
    PredictionMarketVenue, SignalInstanceKey, SignalScope, SignalStreamMessage, SignalUpdate,
};

use crate::file_generation::FileGenerationCaps;
use crate::lane::LaneClock;
use crate::multi_signals::{MultiCheckpointState, MultiProcessor, MultiVenue};
use crate::prediction_journal::{
    PredictionJournalInput, PredictionJournalSession, PredictionRuntimeConfig,
};
use crate::prediction_runtime::{PredictionPipeline, PredictionPipelineError};
use crate::prediction_signals::ActiveBindingCheckpoint;
use crate::stream_accounting::{
    InstanceLedgerEntry, InvalidatedTombstone, ProducerHealth, StreamAccounting,
    StreamAccountingError,
};
use crate::{PredictionSignalError, PredictionSignalProcessor};

/// One venue-tagged admission in the merged journal (admission order).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MultiJournalRecord {
    pub venue: MultiVenueTag,
    pub input: PredictionJournalInput,
}

/// The venue tag persisted in the merged journal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MultiVenueTag {
    Kalshi,
    Polymarket,
}

impl MultiVenueTag {
    /// The prediction market venue this tag names.
    pub fn market_venue(self) -> PredictionMarketVenue {
        self.into()
    }

    fn multi_venue(self) -> MultiVenue {
        match self {
            Self::Kalshi => MultiVenue::Kalshi,
            Self::Polymarket => MultiVenue::Polymarket,
        }
    }
}

impl From<MultiVenueTag> for PredictionMarketVenue {
    fn from(tag: MultiVenueTag) -> Self {
        match tag {
            MultiVenueTag::Kalshi => Self::Kalshi,
            MultiVenueTag::Polymarket => Self::Polymarket,
        }
    }
}

/// Errors from the multi lane runtime.
#[derive(Debug, thiserror::Error)]
pub enum MultiRuntimeError {
    #[error("inner prediction pipeline failed: {0}")]
    Pipeline(#[from] PredictionPipelineError),
    #[error("multi processor failed: {0}")]
    Multi(#[from] MultiSignalErrorWrap),
    #[error("multi stream accounting failed: {0}")]
    Accounting(#[from] StreamAccountingError),
    #[error("multi runtime i/o failed: {0}")]
    Io(#[from] io::Error),
    #[error("merged journal record is invalid: {0}")]
    Journal(String),
    #[error("prediction journal exceeds {maximum} bytes at {attempted} bytes")]
    JournalSizeLimit { attempted: u64, maximum: u64 },
}

/// Newtype wrapper so `MultiSignalError` (which is not `std::error::Error`-shaped
/// for `#[from]`) threads through `thiserror`.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct MultiSignalErrorWrap(pub String);

impl From<PredictionSignalError> for MultiRuntimeError {
    fn from(error: PredictionSignalError) -> Self {
        Self::Pipeline(PredictionPipelineError::Processor(error))
    }
}

impl From<crate::multi_signals::MultiSignalError> for MultiRuntimeError {
    fn from(error: crate::multi_signals::MultiSignalError) -> Self {
        Self::Multi(MultiSignalErrorWrap(error.to_string()))
    }
}

/// A `Write` handle onto the shared merged output. Cloning yields another handle
/// onto the same buffer, so the two inner pipelines and the multi publisher all
/// append to one stream.
#[derive(Clone)]
struct SharedWriter<W: Write> {
    shared: Rc<RefCell<W>>,
}

impl<W: Write> Write for SharedWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.shared.borrow_mut().write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.shared.borrow_mut().flush()
    }
}

/// A tee `Write` for one inner pipeline: forwards every byte to the shared merged
/// output AND appends it to a per-venue tap the outer runtime drains after each
/// admission to harvest the venue's just-published updates.
#[derive(Clone)]
struct TeeWriter<W: Write> {
    shared: Rc<RefCell<W>>,
    tap: Rc<RefCell<Vec<u8>>>,
}

impl<W: Write> Write for TeeWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.tap.borrow_mut().extend_from_slice(buf);
        self.shared.borrow_mut().write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.shared.borrow_mut().flush()
    }
}

const DEFAULT_MAX_UPDATES: u64 = 10_000_000;
const DEFAULT_MAX_BYTES: u64 = 2_147_483_648;

/// The throwaway publisher epoch a rotated leg's re-warm seed processor is built
/// under, so restoring the binding under the real (derived) leg output epoch never
/// collides with the seed in the embedded spot reference's epoch lineage. It is
/// structurally distinct from any derived leg epoch (`<prefix>-<unix>.<venue>`),
/// so it can never equal a real leg epoch, and it never reaches the wire — only
/// the restore target epoch stamps published frames.
const REWARM_SEED_EPOCH: &str = "multi-rotation-rewarm-seed";

/// The dual-venue multi lane runtime.
pub struct MultiLaneRuntime<W: Write> {
    shared: Rc<RefCell<W>>,
    /// The multi publisher's own persistent handle onto the shared output, so the
    /// `StreamAccounting` borrow has a `&mut W` that outlives each call without a
    /// per-decision allocation or leak.
    multi_writer: SharedWriter<W>,
    kalshi: VenueLane<W>,
    polymarket: VenueLane<W>,
    multi: MultiProcessor,
    publisher_epoch: String,
    /// The base process admission epochs the two legs' feeds stamp events with.
    /// Constant for the life of the process (a rotation only changes the derived
    /// output epoch), so a rotated generation's session carries these unchanged.
    kalshi_admission_epoch: String,
    polymarket_admission_epoch: String,
    /// Each leg's active market binding, tracked purely from the venue-tagged
    /// inputs this runtime admits (a `Bind` sets it, an `Unavailable` clears it),
    /// so it always mirrors the corresponding inner processor's warm `active`
    /// contract without reaching into the pipeline. Carried into a rotated
    /// generation's merged-journal header ([`Self::rotated_journal_header`]) so a
    /// standalone replay of that generation restores each leg's binding and admits
    /// the market-relative events that follow the boundary without a fresh Bind
    /// (E64/C5). `None` while a leg is unbound (pre-bind or in a roll gap).
    kalshi_binding: Option<ActiveBindingCheckpoint>,
    polymarket_binding: Option<ActiveBindingCheckpoint>,
    wall_start: DateTime<Utc>,
    /// Monotonic decision clock for the multi publisher: the running maximum of
    /// every admission's `now_ns`, so the multi frames' timestamps never regress
    /// even when the two venues' clocks interleave.
    multi_now_ns: u64,
    /// The `multi_market` scope of the currently-bound blend instance, if any.
    /// Tracked so a leg departure can invalidate it (V10/L2) before the inner
    /// pipeline writes that leg's `market_transition unavailable` frame.
    bound_multi_scope: Option<SignalScope>,
    kalshi_admissions: u64,
    polymarket_admissions: u64,
    /// The merged-output caps the single-venue path enforces per stream, applied
    /// here to the ONE merged stream (E57): the authoritative merged counters
    /// below sum every contributor (both inner pipelines' venue frames and the
    /// multi publisher's blend frames), so the merged `output.jsonl` is bounded
    /// exactly as a single-venue lane's output is. Each contributor's inner cap
    /// is set to the same value, so no inner counter — which sees only its own
    /// share — ever trips before the merged total does.
    max_updates: u64,
    max_output_bytes: u64,
    /// Total signal updates and total bytes written to the merged output across
    /// all contributors; checked after every admission against the caps above,
    /// raising the same `UpdateLimit`/`ByteLimit` overflow (exit 78) the
    /// single-venue pipeline raises.
    merged_updates_written: u64,
    merged_output_bytes: u64,
    // --- multi publisher stream-accounting state (mirrors PredictionPipeline) ---
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
    scratch: Vec<u8>,
}

/// One inner venue pipeline plus its output tap.
struct VenueLane<W: Write> {
    pipeline: PredictionPipeline<TeeWriter<W>>,
    tap: Rc<RefCell<Vec<u8>>>,
}

/// The bounded resume state of the multi lane (E57 "checkpoint = both pipelines +
/// multi state"). The two inner pipelines run in apply-only mode, which — with no
/// `prediction_runtime.rs` edits — exposes no per-processor checkpoint document,
/// so the durable recovery source for the venue state is the **merged journal**
/// itself (replay-to-resume, the same deterministic path the byte-identical test
/// proves): this checkpoint records how far each venue has been admitted plus the
/// multi processor's own emitter memory, and a resume replays the merged journal
/// from the start under a new publisher epoch (the spot-pipeline restart rule).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MultiLaneCheckpoint {
    pub kalshi_admissions: u64,
    pub polymarket_admissions: u64,
    pub multi: MultiCheckpointState,
}

impl<W: Write> MultiLaneRuntime<W> {
    /// Build the runtime over a shared output, the two venue sessions, and the
    /// lane's own publisher epoch and wall start.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        shared: Rc<RefCell<W>>,
        publisher_epoch: impl Into<String>,
        wall_start: DateTime<Utc>,
        kalshi_session: &PredictionJournalSession,
        polymarket_session: &PredictionJournalSession,
        kalshi_binding: Option<ActiveBindingCheckpoint>,
        polymarket_binding: Option<ActiveBindingCheckpoint>,
        max_updates: u64,
        max_output_bytes: u64,
    ) -> Result<Self, MultiRuntimeError> {
        let kalshi = Self::build_lane(
            &shared,
            PredictionMarketVenue::Kalshi,
            kalshi_session,
            kalshi_binding.as_ref(),
            max_updates,
            max_output_bytes,
        )?;
        let polymarket = Self::build_lane(
            &shared,
            PredictionMarketVenue::Polymarket,
            polymarket_session,
            polymarket_binding.as_ref(),
            max_updates,
            max_output_bytes,
        )?;
        let publisher_epoch = publisher_epoch.into();
        let multi = MultiProcessor::new(publisher_epoch.clone())?;
        let signal_intervals_ms = multi
            .definitions()
            .into_iter()
            .map(|definition| {
                (
                    (definition.signal_id.clone(), definition.version.clone()),
                    definition.approximate_interval_ms,
                )
            })
            .collect();
        let multi_writer = SharedWriter {
            shared: Rc::clone(&shared),
        };
        // The admission epoch is the base process epoch the still-running feeds
        // stamp every admitted event with (E64/C5) — distinct from each leg's
        // per-generation OUTPUT epoch (`publisher_epoch`, which stamps emitted
        // frames). `admission_epoch()` returns the session's explicit admission
        // epoch when set (live and rotated generations) and falls back to its
        // output epoch otherwise (single-epoch sessions), so a rotation carries
        // the base process epoch — not the derived output epoch — into each
        // successor generation's sessions and header.
        let kalshi_admission_epoch = kalshi_session.admission_epoch().to_owned();
        let polymarket_admission_epoch = polymarket_session.admission_epoch().to_owned();
        Ok(Self {
            shared,
            multi_writer,
            kalshi,
            polymarket,
            multi,
            publisher_epoch,
            kalshi_admission_epoch,
            polymarket_admission_epoch,
            kalshi_binding,
            polymarket_binding,
            wall_start,
            multi_now_ns: 0,
            bound_multi_scope: None,
            kalshi_admissions: 0,
            polymarket_admissions: 0,
            max_updates,
            max_output_bytes,
            merged_updates_written: 0,
            merged_output_bytes: 0,
            next_stream_sequence: 0,
            last_signal_batch_sequence: None,
            last_decision_or_status_sequence: None,
            latest_updates: BTreeMap::new(),
            instance_ledger: BTreeMap::new(),
            invalidated_tombstones: BTreeMap::new(),
            signal_intervals_ms,
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
            scratch: Vec::new(),
        })
    }

    fn build_lane(
        shared: &Rc<RefCell<W>>,
        venue: PredictionMarketVenue,
        session: &PredictionJournalSession,
        active_binding: Option<&ActiveBindingCheckpoint>,
        max_updates: u64,
        max_output_bytes: u64,
    ) -> Result<VenueLane<W>, MultiRuntimeError> {
        let wall_start = parse_wall_start(&session.wall_start)?;
        // Kalshi runs disarmed on the multi lane (E57); Polymarket never arms.
        let index_armed = false;
        let processor = match active_binding {
            // A rotated generation whose leg was bound at the boundary: rebuild the
            // active market from the carried binding so the market-relative events
            // that follow the boundary replay gap-free, WITHOUT a fresh Bind — the
            // deterministic path the single-venue lane takes via
            // `restore_rotated_indexed` (E64/C5). The rest of the processor
            // re-warms: the multi lane holds no per-leg processor checkpoint, so
            // only the binding continues while the rolling ring/bar and spot
            // reference identities honestly re-warm from the post-boundary stream.
            Some(binding) => {
                // A fresh processor's checkpoint state is a complete, valid empty
                // shell (every expected feature present and empty, a fresh spot
                // reference); stamping the carried binding onto it is exactly the
                // gap-free rotated-replay input `restore_rotated_indexed` expects,
                // and re-parsing the retained contract event reproduces the same
                // `ActiveContract` the live warm processor held. The seed processor
                // is built under a throwaway epoch — structurally distinct from any
                // derived leg output epoch (which always ends in `.kalshi` /
                // `.polymarket`) — so restoring under the real leg epoch does not
                // trip the spot reference's reused-epoch lineage guard. Each leg is
                // an independent processor, so the two may share one seed epoch (the
                // guard is per-processor).
                let mut state = PredictionSignalProcessor::new_indexed(
                    venue,
                    REWARM_SEED_EPOCH,
                    wall_start,
                    index_armed,
                )?
                .checkpoint_state();
                state.last_market_generation = binding.generation;
                state.active_binding = Some(binding.clone());
                PredictionSignalProcessor::restore_rotated_indexed(
                    session.publisher_epoch.clone(),
                    wall_start,
                    state,
                    index_armed,
                )?
            }
            None => PredictionSignalProcessor::new_indexed(
                venue,
                session.publisher_epoch.clone(),
                wall_start,
                index_armed,
            )?,
        };
        let tap = Rc::new(RefCell::new(Vec::new()));
        let tee = TeeWriter {
            shared: Rc::clone(shared),
            tap: Rc::clone(&tap),
        };
        let pipeline =
            PredictionPipeline::replay(tee, processor, session, max_updates, max_output_bytes)?;
        Ok(VenueLane { pipeline, tap })
    }

    /// Admit one venue-tagged input: route it to the inner pipeline (which writes
    /// its venue frames), harvest the pipeline's just-published updates, and drive
    /// the multi blends' decision.
    pub fn admit(
        &mut self,
        venue: MultiVenueTag,
        input: PredictionJournalInput,
    ) -> Result<(), MultiRuntimeError> {
        let now_ns = input.now_ns();
        // V10/L2: a departing leg invalidates the bound cross-venue instance, and
        // that invalidation must reach the merged output BEFORE the inner pipeline
        // writes the leg's own `market_transition unavailable`. Emit it here,
        // ahead of `admit_replay`, so the byte order on the wire is
        // invalidated(multi) -> leg unavailable (the new instance, if any, binds
        // fresh under a new scope once the successor generation re-aligns).
        if matches!(input, PredictionJournalInput::Unavailable { .. }) {
            self.multi_now_ns = self.multi_now_ns.max(now_ns);
            let invalidate_at = self.multi_now_ns;
            self.invalidate_bound_multi(invalidate_at)?;
        }
        // Read the leg's binding transition BEFORE the input is routed (and moved)
        // into the inner pipeline; it is committed only after `admit_replay`
        // accepts the input, so a rejected admission never perturbs the tracked
        // binding.
        let binding_transition = leg_binding_transition(&input);
        {
            let lane = match venue {
                MultiVenueTag::Kalshi => &mut self.kalshi,
                MultiVenueTag::Polymarket => &mut self.polymarket,
            };
            lane.tap.borrow_mut().clear();
            lane.pipeline.admit_replay(input)?;
        }
        {
            let binding_slot = match venue {
                MultiVenueTag::Kalshi => &mut self.kalshi_binding,
                MultiVenueTag::Polymarket => &mut self.polymarket_binding,
            };
            match binding_transition {
                LegBindingTransition::Bind(binding) => *binding_slot = Some(binding),
                LegBindingTransition::Clear => *binding_slot = None,
                LegBindingTransition::Keep => {}
            }
        }
        match venue {
            MultiVenueTag::Kalshi => self.kalshi_admissions += 1,
            MultiVenueTag::Polymarket => self.polymarket_admissions += 1,
        }
        self.harvest(venue)?;
        self.multi_now_ns = self.multi_now_ns.max(now_ns);
        self.drive_multi()?;
        self.check_output_caps()?;
        Ok(())
    }

    /// Fail the run with the same overflow the single-venue pipeline raises once
    /// the merged output crosses a cap. The counters are the authoritative merged
    /// totals (every venue frame plus every blend frame), so the merged
    /// `output.jsonl` is bounded exactly as a single-venue lane's output is, and
    /// the error is the identical `UpdateLimit`/`ByteLimit` (exit 78).
    fn check_output_caps(&self) -> Result<(), MultiRuntimeError> {
        if self.merged_updates_written > self.max_updates {
            return Err(PredictionPipelineError::UpdateLimit(self.max_updates).into());
        }
        if self.merged_output_bytes > self.max_output_bytes {
            return Err(PredictionPipelineError::ByteLimit(self.max_output_bytes).into());
        }
        Ok(())
    }

    /// Parse the venue lane's tap (the frames it just wrote) and feed the multi
    /// processor: ready venue updates through `observe`, an `unavailable` market
    /// transition through `mark_leg_unavailable` (L2).
    fn harvest(&mut self, venue: MultiVenueTag) -> Result<(), MultiRuntimeError> {
        let tap = match venue {
            MultiVenueTag::Kalshi => Rc::clone(&self.kalshi.tap),
            MultiVenueTag::Polymarket => Rc::clone(&self.polymarket.tap),
        };
        let bytes = tap.borrow().clone();
        // The tee mirrors exactly the bytes this venue's inner pipeline just wrote
        // to the merged output, so the tap's length is this admission's merged-byte
        // contribution from the venue frames.
        self.merged_output_bytes = self
            .merged_output_bytes
            .checked_add(bytes.len() as u64)
            .ok_or(MultiRuntimeError::Accounting(
                StreamAccountingError::Counter,
            ))?;
        let mut updates: Vec<SignalUpdate> = Vec::new();
        let mut leg_unavailable = false;
        for line in bytes.split(|byte| *byte == b'\n') {
            if line.is_empty() {
                continue;
            }
            let message: SignalStreamMessage = match serde_json::from_slice(line) {
                Ok(message) => message,
                // A partial line (the writer buffers whole frames per call, so this
                // is not expected) is skipped rather than aborting the lane.
                Err(_) => continue,
            };
            match message {
                SignalStreamMessage::SignalBatch { updates: batch, .. } => {
                    updates.extend(batch);
                }
                SignalStreamMessage::MarketTransition { phase, .. } => {
                    if matches!(phase, signals_core::MarketTransitionPhase::Unavailable) {
                        leg_unavailable = true;
                    }
                }
                _ => {}
            }
        }
        // Each harvested `SignalBatch` update is one update the inner pipeline
        // wrote to the merged wire; count them into the merged total exactly as the
        // single-venue path counts its own signal-batch updates.
        self.merged_updates_written = self
            .merged_updates_written
            .checked_add(updates.len() as u64)
            .ok_or(MultiRuntimeError::Accounting(
                StreamAccountingError::Counter,
            ))?;
        if leg_unavailable {
            self.multi.mark_leg_unavailable(venue.multi_venue());
        }
        if !updates.is_empty() {
            self.multi.observe(venue.multi_venue(), &updates)?;
        }
        Ok(())
    }

    /// Run one multi decision and write any published blend updates to the shared
    /// output under the lane's own publisher epoch.
    fn drive_multi(&mut self) -> Result<(), MultiRuntimeError> {
        let produced_at =
            crate::stream_accounting::stream_timestamp(&self.wall_start, self.multi_now_ns)
                .map_err(MultiRuntimeError::Accounting)?;
        let clock = LaneClock {
            now_ns: self.multi_now_ns,
            produced_at: produced_at.clone(),
        };
        let now_ns = self.multi_now_ns;
        let mut updates = self.multi.evaluate(&clock)?;
        // The `MultiProcessor` stamps every blend it emits with its own
        // construction epoch, which on a warm processor that has rotated is the
        // PRIOR generation's output epoch (rotation keeps the processor warm but
        // does not re-stamp it) — a blend withheld before the boundary and first
        // released as Ready in the new generation carries that stale epoch. Stamp
        // this generation's multi output epoch onto every emitted update before it
        // reaches the wire so the batch envelope and its updates agree
        // (`validate_batch_updates`) and the retained instances a later snapshot
        // re-emits carry the current epoch too. Outside a rotation the epochs are
        // already equal, so this is a no-op and every existing byte is unchanged.
        // Mirrors the single-venue lane's `restamp_output_epoch`.
        self.restamp_output_epoch(&mut updates);
        let multi_update_count = updates.len() as u64;
        // The multi publisher writes its blend frames through `StreamAccounting`,
        // which advances `self.bytes_written`; the delta across this decision is the
        // blend frames' merged-byte contribution.
        let bytes_before = self.bytes_written;
        self.accounting().ensure_stream_started(now_ns)?;
        if !updates.is_empty() {
            self.accounting().write_signal_batch(now_ns, updates)?;
        }
        self.accounting()
            .maybe_write_status(now_ns, crate::multi_signals::MULTI_SIGNAL_COUNT)?;
        self.accounting().maybe_write_snapshot(now_ns)?;
        self.accounting().maybe_write_heartbeat(now_ns)?;
        let multi_bytes =
            self.bytes_written
                .checked_sub(bytes_before)
                .ok_or(MultiRuntimeError::Accounting(
                    StreamAccountingError::Counter,
                ))?;
        self.merged_output_bytes = self.merged_output_bytes.checked_add(multi_bytes).ok_or(
            MultiRuntimeError::Accounting(StreamAccountingError::Counter),
        )?;
        self.merged_updates_written = self
            .merged_updates_written
            .checked_add(multi_update_count)
            .ok_or(MultiRuntimeError::Accounting(
                StreamAccountingError::Counter,
            ))?;
        // Track the bound instance's scope so a later leg departure can invalidate
        // exactly it (V10/L2). None while the pair is unaligned (withheld).
        self.bound_multi_scope = self.multi.aligned_scope(&produced_at)?;
        Ok(())
    }

    /// Stamp this generation's multi output `publisher_epoch` onto every blend
    /// update before it reaches the wire. The `MultiProcessor` emits under its
    /// construction epoch, which on a warm processor that has rotated is the prior
    /// generation's output epoch, not this generation's; the published frame's
    /// envelope and its updates must agree (`validate_batch_updates`), and the
    /// retained instance a later snapshot re-emits must carry the current epoch as
    /// well. Outside rotation the epochs already match, so this is a no-op and
    /// every existing byte is unchanged. Mirrors
    /// [`crate::prediction_runtime`]'s single-venue `restamp_output_epoch`.
    fn restamp_output_epoch(&self, updates: &mut [SignalUpdate]) {
        for update in updates.iter_mut() {
            if update.publisher_epoch != self.publisher_epoch {
                update.publisher_epoch.clone_from(&self.publisher_epoch);
            }
        }
    }

    /// Invalidate the bound cross-venue instance (both blends) under its tracked
    /// `multi_market` scope — the terminal `invalidated` a leg departure owes the
    /// consumer (V10/L2). Only an instance that actually reached the wire (is in
    /// the ledger) is tombstoned; the byte cost is folded into the merged totals.
    fn invalidate_bound_multi(&mut self, now_ns: u64) -> Result<(), MultiRuntimeError> {
        let Some(scope) = self.bound_multi_scope.take() else {
            return Ok(());
        };
        let keys: Vec<SignalInstanceKey> = crate::multi_signals::multi_signal_ids()
            .into_iter()
            .map(|id| SignalInstanceKey {
                signal_id: id.to_owned(),
                signal_version: crate::multi_signals::MULTI_SIGNAL_VERSION.to_owned(),
                scope: scope.clone(),
            })
            .filter(|key| self.instance_ledger.contains_key(key))
            .collect();
        for key in keys {
            let bytes_before = self.bytes_written;
            self.accounting()
                .invalidate_instance(now_ns, key, "market_rollover")?;
            let delta = self.bytes_written.checked_sub(bytes_before).ok_or(
                MultiRuntimeError::Accounting(StreamAccountingError::Counter),
            )?;
            self.merged_output_bytes = self.merged_output_bytes.checked_add(delta).ok_or(
                MultiRuntimeError::Accounting(StreamAccountingError::Counter),
            )?;
        }
        Ok(())
    }

    fn accounting(&mut self) -> StreamAccounting<'_, SharedWriter<W>> {
        StreamAccounting {
            writer: &mut self.multi_writer,
            hook: None,
            scratch: &mut self.scratch,
            publisher_epoch: &self.publisher_epoch,
            wall_start: self.wall_start,
            maximum_updates: self.max_updates,
            maximum_bytes: self.max_output_bytes,
            next_stream_sequence: &mut self.next_stream_sequence,
            last_signal_batch_sequence: &mut self.last_signal_batch_sequence,
            last_decision_or_status_sequence: &mut self.last_decision_or_status_sequence,
            latest_updates: &mut self.latest_updates,
            instance_ledger: &mut self.instance_ledger,
            invalidated_tombstones: &mut self.invalidated_tombstones,
            signal_intervals_ms: &self.signal_intervals_ms,
            venue_gap_active: false,
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
            market_transitions_written: None,
            feed_health_written: None,
        }
    }

    /// Seed a bound cross-venue instance for tests where co-temporal aligned
    /// venue journals are unavailable locally: insert a ledger entry for both
    /// blends under `scope` and track it, so a subsequent leg departure exercises
    /// the real invalidation path.
    #[cfg(test)]
    fn seed_bound_multi(&mut self, scope: SignalScope) {
        for id in crate::multi_signals::multi_signal_ids() {
            let key = SignalInstanceKey {
                signal_id: id.to_owned(),
                signal_version: crate::multi_signals::MULTI_SIGNAL_VERSION.to_owned(),
                scope: scope.clone(),
            };
            self.instance_ledger.insert(
                key,
                InstanceLedgerEntry {
                    last_decision_sequence: Some(0),
                    status: None,
                },
            );
        }
        self.bound_multi_scope = Some(scope);
    }

    /// Flush the shared output.
    pub fn flush(&mut self) -> Result<(), MultiRuntimeError> {
        self.shared.borrow_mut().flush()?;
        Ok(())
    }

    /// The derived per-generation output epochs for a rotation onto the base
    /// `output_epoch` (`<prefix>-<unix>`): the multi publisher takes it bare, each
    /// leg suffixes it (`.kalshi` / `.polymarket`), mirroring the base header's
    /// `<prefix>` / `<prefix>.<venue>` split.
    fn rotated_output_epochs(output_epoch: &str) -> (String, String) {
        (
            format!("{output_epoch}.kalshi"),
            format!("{output_epoch}.polymarket"),
        )
    }

    /// The merged-journal header a rotation-opened generation writes: its venue
    /// epochs are the derived output epochs, each carrying the base process
    /// admission epoch (`admission_epoch`) the still-running feeds keep stamping,
    /// so the generation's own journal replays under the right two epochs (E57),
    /// plus each leg's active market binding (`active_binding`, when the leg is
    /// bound at the boundary) so a standalone replay restores the binding and
    /// admits the market-relative events that follow the boundary without a fresh
    /// Bind (E64/C5). A leg unbound at the boundary carries `None` and replays
    /// cold, re-discovering on its next Bind.
    pub fn rotated_journal_header(&self, output_epoch: &str) -> MultiJournalHeader {
        let (kalshi_output_epoch, polymarket_output_epoch) =
            Self::rotated_output_epochs(output_epoch);
        let wall_start = self.wall_start.to_rfc3339_opts(SecondsFormat::Nanos, true);
        MultiJournalHeader {
            publisher_epoch: output_epoch.to_owned(),
            wall_start: wall_start.clone(),
            kalshi: VenueHeader {
                publisher_epoch: kalshi_output_epoch,
                wall_start: wall_start.clone(),
                admission_epoch: Some(self.kalshi_admission_epoch.clone()),
                active_binding: self.kalshi_binding.clone(),
            },
            polymarket: VenueHeader {
                publisher_epoch: polymarket_output_epoch,
                wall_start,
                admission_epoch: Some(self.polymarket_admission_epoch.clone()),
                active_binding: self.polymarket_binding.clone(),
            },
        }
    }

    /// Rotate the multi lane onto a new file generation (E57): reset both inner
    /// lanes' stream halves and the multi publisher's stream accounting onto the
    /// derived per-generation output epochs, keeping both venue processors and the
    /// multi blends warm in memory. The caller swaps the shared output file (and
    /// opens the successor's merged journal with [`Self::rotated_journal_header`])
    /// between the departed and successor generations, then calls this: the
    /// successor republishes a reset frame at sequence 0 for every stream, and each
    /// leg's admission sequence restarts at 0 so the successor's merged journal is
    /// contiguous for a fresh `runtime_from_header` replay. The bound multi scope,
    /// the monotonic decision clock and the cumulative admission counters survive
    /// the boundary; a market roll's `Invalidated` on the multi_market scope is
    /// unchanged.
    pub fn rotate(
        &mut self,
        output_epoch: &str,
        caps: FileGenerationCaps,
    ) -> Result<(), MultiRuntimeError> {
        let (kalshi_output_epoch, polymarket_output_epoch) =
            Self::rotated_output_epochs(output_epoch);
        let wall_start = self.wall_start.to_rfc3339_opts(SecondsFormat::Nanos, true);
        // By the time this runs the caller has ALREADY flushed + fsynced the
        // departed generation's output and swapped the shared writer to the
        // successor's (empty) file (bin `rotate_multi_generation` steps 1-2, before
        // step 4 calls this). This flush therefore drains the fresh successor
        // buffer, not the departed file; it is retained as a harmless barrier
        // ahead of the successor's first write.
        self.flush()?;

        let kalshi_session = venue_session(
            PredictionMarketVenue::Kalshi,
            kalshi_output_epoch,
            &wall_start,
            SESSION_DISCOVERY_RETRY_MS,
        )?
        .with_admission_epoch(self.kalshi_admission_epoch.clone())
        .map_err(|error| MultiRuntimeError::Journal(error.to_string()))?;
        let polymarket_session = venue_session(
            PredictionMarketVenue::Polymarket,
            polymarket_output_epoch,
            &wall_start,
            SESSION_DISCOVERY_RETRY_MS,
        )?
        .with_admission_epoch(self.polymarket_admission_epoch.clone())
        .map_err(|error| MultiRuntimeError::Journal(error.to_string()))?;

        // Rebuild each inner lane around its warm processor over a fresh tee onto
        // the (already swapped) shared output. The tee is cheaply reconstructed
        // from the same shared writer and the lane's own tap.
        let kalshi_tee = TeeWriter {
            shared: Rc::clone(&self.shared),
            tap: Rc::clone(&self.kalshi.tap),
        };
        self.kalshi
            .pipeline
            .rotate_replay(kalshi_tee, &kalshi_session, caps)?;
        let polymarket_tee = TeeWriter {
            shared: Rc::clone(&self.shared),
            tap: Rc::clone(&self.polymarket.tap),
        };
        self.polymarket
            .pipeline
            .rotate_replay(polymarket_tee, &polymarket_session, caps)?;

        // Reset the multi publisher's stream accounting exactly as a fresh
        // generation start does; keep the warm `MultiProcessor` (blend emitter
        // memory), the monotonic decision clock, the bound scope and the
        // cumulative admission counters. The blends' per-identity decision
        // sequences DO restart at zero for the successor's fresh output epoch
        // (C5 Design B): the warm blend memory crosses the boundary but each
        // identity's first ready decision in the new epoch is sequence zero.
        self.multi.reset_output_sequences();
        self.publisher_epoch = output_epoch.to_owned();
        self.max_updates = caps.max_updates;
        self.max_output_bytes = caps.max_output_bytes;
        self.merged_updates_written = 0;
        self.merged_output_bytes = 0;
        self.next_stream_sequence = 0;
        self.last_signal_batch_sequence = None;
        self.last_decision_or_status_sequence = None;
        self.latest_updates.clear();
        self.instance_ledger.clear();
        self.invalidated_tombstones.clear();
        self.producer_health = None;
        self.next_heartbeat_ns = None;
        self.next_snapshot_ns = None;
        self.updates_written = 0;
        self.bytes_written = 0;
        self.stream_messages_written = 0;
        self.signal_batches_written = 0;
        self.snapshots_written = 0;
        self.heartbeats_written = 0;
        self.status_messages_written = 0;
        self.reset_messages_written = 0;
        Ok(())
    }

    /// The next admission sequence the given venue's inner pipeline expects, so a
    /// live caller can stamp a `PredictionJournalInput` before routing it here.
    pub fn next_admission_seq(&self, venue: MultiVenueTag) -> u64 {
        match venue {
            MultiVenueTag::Kalshi => self.kalshi.pipeline.next_admission_seq(),
            MultiVenueTag::Polymarket => self.polymarket.pipeline.next_admission_seq(),
        }
    }

    /// The generation an auxiliary (non-market-binding) event for the given venue
    /// takes — the active market's, or the next reserved one in a roll gap. Used by
    /// the Polymarket Chainlink-TWAP relay, which carries no market binding.
    pub fn current_generation_for_auxiliary_source(
        &self,
        venue: MultiVenueTag,
    ) -> Result<u64, MultiRuntimeError> {
        let pipeline = match venue {
            MultiVenueTag::Kalshi => &self.kalshi.pipeline,
            MultiVenueTag::Polymarket => &self.polymarket.pipeline,
        };
        Ok(pipeline.current_generation_for_auxiliary_source()?)
    }

    /// Total signal updates written to the merged output so far (every venue
    /// frame plus every blend frame), the counter checked against `max_updates`.
    pub fn merged_updates_written(&self) -> u64 {
        self.merged_updates_written
    }

    /// Total bytes written to the merged output so far, the counter checked
    /// against `max_output_bytes`.
    pub fn merged_output_bytes(&self) -> u64 {
        self.merged_output_bytes
    }

    /// The bounded resume state (admission cursors + the multi state).
    pub fn checkpoint(&self) -> MultiLaneCheckpoint {
        MultiLaneCheckpoint {
            kalshi_admissions: self.kalshi_admissions,
            polymarket_admissions: self.polymarket_admissions,
            multi: self.multi.checkpoint_state(),
        }
    }
}

/// What one admitted input does to a leg's tracked active binding.
enum LegBindingTransition {
    /// A `Bind` establishes (or rolls to) the market carried by its contract
    /// event, mirroring the inner processor's `active` contract.
    Bind(ActiveBindingCheckpoint),
    /// An `Unavailable` departs the bound market, leaving the leg unbound.
    Clear,
    /// Every other input leaves the binding unchanged.
    Keep,
}

/// The binding transition an input carries. `Bind`/`Unavailable` are the only
/// inputs that move the inner processor's `active` contract, so tracking exactly
/// these two reproduces the processor's binding purely from the admitted stream —
/// the `generation` and `contract_event` a `Bind` carries are precisely what
/// [`PredictionSignalProcessor::checkpoint_state`] records as `active_binding`.
fn leg_binding_transition(input: &PredictionJournalInput) -> LegBindingTransition {
    match input {
        PredictionJournalInput::Bind {
            generation, event, ..
        } => LegBindingTransition::Bind(ActiveBindingCheckpoint {
            generation: *generation,
            contract_event: event.clone(),
        }),
        PredictionJournalInput::Unavailable { .. } => LegBindingTransition::Clear,
        _ => LegBindingTransition::Keep,
    }
}

fn parse_wall_start(text: &str) -> Result<DateTime<Utc>, MultiRuntimeError> {
    DateTime::parse_from_rfc3339(text)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|error| {
            MultiRuntimeError::Journal(format!("undecodable wall_start {text}: {error}"))
        })
}

/// Build a fresh venue session for the multi lane from a recorded venue journal
/// header's epoch and wall start (test + live helper). The multi lane always runs
/// both venues index-disarmed.
pub fn venue_session(
    venue: PredictionMarketVenue,
    publisher_epoch: impl Into<String>,
    wall_start: &str,
    discovery_retry_ms: u64,
) -> Result<PredictionJournalSession, MultiRuntimeError> {
    let runtime_config = PredictionRuntimeConfig::new(venue, discovery_retry_ms);
    let session = PredictionJournalSession::new(
        wall_start.to_owned(),
        publisher_epoch.into(),
        runtime_config,
    )
    .map_err(|error| MultiRuntimeError::Journal(error.to_string()))?;
    Ok(session)
}

/// One venue's recorded session identity in the merged-journal header.
///
/// `Eq` is not derived: `active_binding` embeds a `SourceEvent`, whose `values`
/// carry `serde_json::Value`s that are `PartialEq` but not `Eq`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VenueHeader {
    pub publisher_epoch: String,
    pub wall_start: String,
    /// The process admission epoch this generation's events are stamped with,
    /// distinct from `publisher_epoch` (the derived OUTPUT epoch) whenever the
    /// still-running feeds keep stamping the base process epoch. On the MULTI
    /// lane this is present from generation 0: gen 0 carries `Some(base)` (the
    /// base process epoch both legs admit against, E64/C5), and a rotation-opened
    /// generation (`<prefix>-<unix>.<venue>` output epoch, C5/E57) carries the
    /// same base epoch. Single-venue lanes leave it absent in generation 0.
    /// Absent therefore means a legacy pre-fix journal (or a single-venue gen 0),
    /// which defaults to the derived output epoch on replay — the epoch those
    /// events were actually stamped with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admission_epoch: Option<String>,
    /// The leg's active market binding at the rotation boundary, so a standalone
    /// replay of this generation restores the binding and admits the boundary's
    /// market-relative events without a fresh Bind (E64/C5). Present only on a
    /// rotation-opened generation whose leg was bound; absent (defaulted to
    /// `None`) for generation 0 and for a leg unbound at the boundary, so those
    /// header bytes are unchanged and the leg replays cold and re-discovers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_binding: Option<ActiveBindingCheckpoint>,
}

/// The merged journal's first line: the multi lane's own epoch and wall start,
/// plus each venue's recorded session identity. A replay reconstructs both inner
/// sessions and the multi publisher from this, then applies the tagged records.
///
/// `Eq` is not derived because [`VenueHeader`] is not `Eq` (see there).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MultiJournalHeader {
    pub publisher_epoch: String,
    pub wall_start: String,
    pub kalshi: VenueHeader,
    pub polymarket: VenueHeader,
}

/// The result of a merged-journal replay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MultiReplayReport {
    pub admissions: u64,
}

/// The discovery-retry the multi lane's venue sessions carry; it affects only the
/// runtime config recorded in the (fresh) session, never replay determinism.
const SESSION_DISCOVERY_RETRY_MS: u64 = 2_000;

/// Build a runtime over a shared writer from a merged-journal header (shared by
/// the live run and the replay so both reconstruct the venue sessions identically).
pub fn runtime_from_header<W: Write>(
    shared: Rc<RefCell<W>>,
    header: &MultiJournalHeader,
    max_updates: u64,
    max_output_bytes: u64,
) -> Result<MultiLaneRuntime<W>, MultiRuntimeError> {
    let mut kalshi_session = venue_session(
        PredictionMarketVenue::Kalshi,
        header.kalshi.publisher_epoch.clone(),
        &header.kalshi.wall_start,
        SESSION_DISCOVERY_RETRY_MS,
    )?;
    if let Some(admission_epoch) = &header.kalshi.admission_epoch {
        kalshi_session = kalshi_session
            .with_admission_epoch(admission_epoch.clone())
            .map_err(|error| MultiRuntimeError::Journal(error.to_string()))?;
    }
    let mut polymarket_session = venue_session(
        PredictionMarketVenue::Polymarket,
        header.polymarket.publisher_epoch.clone(),
        &header.polymarket.wall_start,
        SESSION_DISCOVERY_RETRY_MS,
    )?;
    if let Some(admission_epoch) = &header.polymarket.admission_epoch {
        polymarket_session = polymarket_session
            .with_admission_epoch(admission_epoch.clone())
            .map_err(|error| MultiRuntimeError::Journal(error.to_string()))?;
    }
    MultiLaneRuntime::new(
        shared,
        header.publisher_epoch.clone(),
        parse_wall_start(&header.wall_start)?,
        &kalshi_session,
        &polymarket_session,
        header.kalshi.active_binding.clone(),
        header.polymarket.active_binding.clone(),
        max_updates,
        max_output_bytes,
    )
}

/// Deterministically rebuild the merged stream from a retained merged journal:
/// read its header and venue-tagged records, apply each to a fresh runtime, and
/// write the output. Byte-identical to the run that produced the journal for a
/// gen-0 journal; for a rotated generation (N ≥ 1) the header carries only each
/// leg's `active_binding` (no per-leg processor checkpoint), so the rolling
/// ring/bar, order-book and spot-reference identities — including book-derived
/// non-ring ones such as `prediction_liquidity` — honestly re-warm rather than
/// reproducing byte-for-byte. This is the code path the `replay --venue multi`
/// CLI drives.
pub fn replay_multi_file(
    journal: &Path,
    output: &Path,
) -> Result<MultiReplayReport, MultiRuntimeError> {
    let text = std::fs::read_to_string(journal)?;
    let mut lines = text.lines();
    let header_line = lines
        .next()
        .ok_or_else(|| MultiRuntimeError::Journal("merged journal is empty".to_owned()))?;
    let header: MultiJournalHeader = serde_json::from_str(header_line)
        .map_err(|error| MultiRuntimeError::Journal(format!("merged journal header: {error}")))?;
    let out_file = File::create(output)?;
    let shared = Rc::new(RefCell::new(BufWriter::new(out_file)));
    // Replay is the deterministic reconstruction of a journal a bounded run already
    // wrote, so it re-applies every record unconditionally under the widest caps;
    // any cap the run enforced already bounded what the journal can contain.
    let mut runtime = runtime_from_header(
        Rc::clone(&shared),
        &header,
        DEFAULT_MAX_UPDATES,
        DEFAULT_MAX_BYTES,
    )?;
    let mut admissions = 0u64;
    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        let record: MultiJournalRecord = serde_json::from_str(line).map_err(|error| {
            MultiRuntimeError::Journal(format!("merged journal record: {error}"))
        })?;
        runtime.admit(record.venue, record.input)?;
        admissions += 1;
    }
    runtime.flush()?;
    Ok(MultiReplayReport { admissions })
}

/// The merged journal's bounded on-disk writer (the live `run --venue multi`
/// path). It bounds the merged `journal.jsonl` to `maximum_bytes` with the same
/// size-limit overflow the single-venue journal raises: each record is framed
/// whole into a scratch buffer and rejected — before any of its bytes reach the
/// file — if it would carry the journal past the cap, so a tripped journal never
/// holds a torn tail. The initiating header (the merged session record) is written
/// first, so a cap that trips on a later record still leaves the session record on
/// disk. Replay reads the journal with `std::fs::read_to_string`, never this
/// writer, so it is unaffected by the cap.
pub struct MergedJournalWriter<W: Write> {
    writer: W,
    scratch: Vec<u8>,
    bytes_written: u64,
    maximum_bytes: u64,
}

impl<W: Write> MergedJournalWriter<W> {
    /// Wrap `writer`, bounding the total bytes written to `maximum_bytes`.
    pub fn new(writer: W, maximum_bytes: u64) -> Self {
        Self {
            writer,
            scratch: Vec::with_capacity(4096),
            bytes_written: 0,
            maximum_bytes,
        }
    }

    /// Write the merged-journal header (the session record); the live run writes
    /// this first, before any tagged record.
    pub fn write_header(&mut self, header: &MultiJournalHeader) -> Result<(), MultiRuntimeError> {
        self.write_line(
            |scratch, header| serde_json::to_writer(scratch, header),
            header,
        )
    }

    /// Append one venue-tagged record in admission order.
    pub fn write_record(&mut self, record: &MultiJournalRecord) -> Result<(), MultiRuntimeError> {
        self.write_line(
            |scratch, record| serde_json::to_writer(scratch, record),
            record,
        )
    }

    /// Frame the value whole, reject it if it would cross the cap, then write it.
    fn write_line<T>(
        &mut self,
        serialize: impl FnOnce(&mut Vec<u8>, &T) -> Result<(), serde_json::Error>,
        value: &T,
    ) -> Result<(), MultiRuntimeError> {
        self.scratch.clear();
        serialize(&mut self.scratch, value)
            .map_err(|error| MultiRuntimeError::Journal(error.to_string()))?;
        self.scratch.push(b'\n');
        let line_bytes = self.scratch.len() as u64;
        let attempted =
            self.bytes_written
                .checked_add(line_bytes)
                .ok_or(MultiRuntimeError::Accounting(
                    StreamAccountingError::Counter,
                ))?;
        if attempted > self.maximum_bytes {
            return Err(MultiRuntimeError::JournalSizeLimit {
                attempted,
                maximum: self.maximum_bytes,
            });
        }
        self.writer.write_all(&self.scratch)?;
        self.bytes_written = attempted;
        Ok(())
    }

    /// Total bytes written to the merged journal so far.
    pub fn bytes_written(&self) -> u64 {
        self.bytes_written
    }

    /// Flush the underlying writer.
    pub fn flush(&mut self) -> Result<(), MultiRuntimeError> {
        self.writer.flush()?;
        Ok(())
    }
}

/// Serialize one merged-journal header line (the live run writes this first).
pub fn write_journal_header<W: Write>(
    writer: &mut W,
    header: &MultiJournalHeader,
) -> Result<(), MultiRuntimeError> {
    serde_json::to_writer(&mut *writer, header)
        .map_err(|error| MultiRuntimeError::Journal(error.to_string()))?;
    writer.write_all(b"\n")?;
    Ok(())
}

/// Serialize one merged-journal record line (the live run appends one per admission).
pub fn write_journal_record<W: Write>(
    writer: &mut W,
    record: &MultiJournalRecord,
) -> Result<(), MultiRuntimeError> {
    serde_json::to_writer(&mut *writer, record)
        .map_err(|error| MultiRuntimeError::Journal(error.to_string()))?;
    writer.write_all(b"\n")?;
    Ok(())
}

#[cfg(test)]
mod tests;
