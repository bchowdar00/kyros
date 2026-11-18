use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::Arc;

use btc_feeds::CaptureStream;
use btc_feeds::kalshi_index::{
    PYTH_VALUE_SOURCE_ID as KALSHI_PYTH_SOURCE_ID,
    SETTLEMENT_WINDOW_SOURCE_ID as KALSHI_BRTI_SETTLEMENT_SOURCE_ID,
    VALUE_5HZ_SOURCE_ID as KALSHI_BRTI_5HZ_SOURCE_ID, VALUE_SOURCE_ID as KALSHI_BRTI_SOURCE_ID,
};
use btc_feeds::polymarket_twap::SOURCE_ID as POLYMARKET_TWAP_SOURCE_ID;
use btc_feeds::prediction_live::PredictionSocketTransition;
use chrono::{DateTime, Utc};
#[cfg(test)]
use chrono::{Duration as ChronoDuration, SecondsFormat};
use serde::Serialize;
use signals_core::{
    InstanceStatusReasonClass, MarketTransitionPhase, MarketTransitionReason,
    PredictionMarketVenue, SIGNAL_STREAM_SCHEMA_VERSION, SignalInstanceKey, SignalStreamMessage,
    SignalUpdate, SourceEvent, ValidationError, instance_status_reason_class,
};
use thiserror::Error;

use crate::feed_health::{
    FEED_HEALTH_PERIODIC_INTERVAL_NS, FeedAvailability, FeedCadence, FeedContract, FeedHealthBoard,
    FeedHealthSnapshot,
};
use crate::file_generation::{FileGenerationCaps, OpenFileGeneration};
use crate::prediction_checkpoint::{
    PredictionCheckpointDocument, PredictionCheckpointError, PredictionCheckpointWriteReport,
    write_prediction_checkpoint,
};
use crate::prediction_journal::PredictionTransport;
use crate::prediction_journal::{
    PredictionContractFingerprint, PredictionJournalEnd, PredictionJournalError,
    PredictionJournalInput, PredictionJournalSession, PredictionJournalTotals,
    PredictionJournalWriteReport, PredictionJournalWriter, PredictionRuntimeConfig,
};
use crate::stream_accounting::{
    InstanceLedgerEntry, InvalidatedTombstone, ProducerHealth, StreamAccounting,
    StreamAccountingError, StreamHook,
};
use crate::{
    COINBASE_TICKER_SOURCE_ID, KRAKEN_TICKER_SOURCE_ID, PredictionBatch, PredictionSignalError,
    PredictionSignalProcessor, SettlementWindowPause, prediction_signal_count_for,
};

#[derive(Debug, Clone)]
pub struct PredictionCheckpointContext {
    pub path: PathBuf,
    pub maximum_bytes: u64,
    pub session_id: String,
    pub wall_start: String,
    pub contract_fingerprint: PredictionContractFingerprint,
    pub runtime_config: PredictionRuntimeConfig,
    pub publisher_epoch_lineage: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PredictionPipelineReport {
    pub admitted_inputs: u64,
    pub discovery_attempts: u64,
    pub bindings: u64,
    pub market_event_batches: u64,
    pub spot_event_batches: u64,
    pub source_events: u64,
    pub unavailable_transitions: u64,
    pub health_ticks: u64,
    pub socket_transitions: u64,
    pub updates_written: u64,
    pub stream_messages_written: u64,
    pub signal_batches_written: u64,
    pub snapshots_written: u64,
    pub heartbeats_written: u64,
    pub status_messages_written: u64,
    pub reset_messages_written: u64,
    pub market_transitions_written: u64,
    pub bytes_written: u64,
    pub quality_reason_entries: BTreeMap<String, u64>,
    pub feed_health: FeedHealthSnapshot,
    pub feed_health_messages_written: u64,
    /// The tolerated warm-vs-cold re-warm gap of a rotation-opened generation's
    /// cold replay, as a structured count: the absolute difference in
    /// `stream_messages_written` between the warm live run (the journal's END) and
    /// this replay (ruling 23:30Z — the terminal count self-check reports the gap
    /// instead of failing). `0` for a fresh-start generation, an unrotated run and
    /// every live finish; C5b drives it back to `0` for rotated generations too.
    pub rotation_rewarm_gap: u64,
}

/// The deterministic publication boundary shared by live operation and replay.
///
/// A live admission is accepted by the bounded journal writer before this type
/// mutates processor state. Queue saturation or writer failure fails the run;
/// the writer periodically durably syncs the accepted prefix. Each market
/// handoff is emitted before the first batch for the new market, so a consumer
/// cannot evaluate a proper prefix across the boundary.
pub struct PredictionPipeline<W: Write> {
    writer: W,
    processor: PredictionSignalProcessor,
    publisher_epoch: String,
    /// The immutable admission epoch the feeds stamp onto every source event and
    /// that every admitted event is validated against. Equal to `publisher_epoch`
    /// outside generation rotation; under rotation it stays the process
    /// `--publisher-epoch` while `publisher_epoch` becomes the per-generation
    /// output epoch (C5 two-epoch split).
    admission_epoch: String,
    /// Whether this pipeline drives a rotation-opened generation, so replay
    /// tolerates the warm-vs-cold re-warm gap in the terminal self-check (C5).
    opened_by_rotation: bool,
    wall_start: DateTime<Utc>,
    venue: PredictionMarketVenue,
    maximum_updates: u64,
    maximum_bytes: u64,
    journal: Option<PredictionJournalWriter>,
    checkpoint: Option<PredictionCheckpointContext>,
    next_admission_seq: u64,
    next_stream_sequence: u64,
    last_signal_batch_sequence: Option<u64>,
    last_decision_or_status_sequence: Option<u64>,
    latest_updates: BTreeMap<SignalInstanceKey, SignalUpdate>,
    instance_ledger: BTreeMap<SignalInstanceKey, InstanceLedgerEntry>,
    invalidated_tombstones: BTreeMap<SignalInstanceKey, InvalidatedTombstone>,
    signal_intervals_ms: BTreeMap<(String, String), u64>,
    venue_gap_active: bool,
    producer_health: Option<ProducerHealth>,
    next_heartbeat_ns: Option<u64>,
    next_snapshot_ns: Option<u64>,
    discovery_attempts: u64,
    bindings: u64,
    market_event_batches: u64,
    spot_event_batches: u64,
    source_events: u64,
    unavailable_transitions: u64,
    health_ticks: u64,
    socket_transitions: u64,
    updates_written: u64,
    stream_messages_written: u64,
    signal_batches_written: u64,
    snapshots_written: u64,
    heartbeats_written: u64,
    status_messages_written: u64,
    reset_messages_written: u64,
    market_transitions_written: u64,
    bytes_written: u64,
    feed_health: FeedHealthBoard,
    feed_health_stream: bool,
    feed_health_written: u64,
    /// Settlement-window events withheld by an outstanding repair pause (A1/F4).
    settlement_windows_withheld: u64,
    /// Index-channel sources the venue is currently refusing to serve, and the
    /// instance-status reason their dependent instances publish (ruling E25).
    index_channel_refusals: BTreeMap<String, &'static str>,
    /// Source ids a declared stream produces but the processor does not consume
    /// (E53 follow-up, mirroring the venue lane's F1 routing). Events on these
    /// sources — e.g. the armed Kalshi index socket's 5 Hz value, history and
    /// Pyth relay, which no prediction identity consumes — are routed past the
    /// processor and counted in `feed_health.ignored_events`, never a fatal
    /// `UnsupportedSource`. Empty on the disarmed Kalshi and Polymarket lanes, so
    /// their admission is byte-unchanged.
    observed_not_consumed: BTreeSet<String>,
    /// A departed generation whose roll froze an E37c final decision and therefore
    /// deferred the two settlement identities' invalidation (ruling M1). The
    /// invalidation is emitted in [`Self::write_departed_updates`] immediately
    /// AFTER the departed final decision publishes, or when its slot expires, so
    /// each identity's wire sequence stays ready(final) -> invalidated. Not
    /// checkpointed itself, but reconstructed at [`Self::build`] from the restored
    /// pending final-decision slot (`pending_final_decision_scope`), so a departed
    /// decision that releases after a restart in the gap is still followed by the
    /// twin invalidation exactly as in-process.
    pending_departed_invalidation: Option<DepartedSettlementInvalidation>,
    last_feed_health_revision: Option<u64>,
    next_feed_health_ns: Option<u64>,
    /// In-process observer of every written stream message; survives
    /// generation rotation (it belongs to the process, not the file).
    stream_hook: Option<StreamHook>,
    scratch: Vec<u8>,
}

/// The departed market scope and departure reason for a deferred E37c settlement
/// identity invalidation (ruling M1).
#[derive(Debug, Clone)]
struct DepartedSettlementInvalidation {
    market_id: String,
    market_generation: u64,
    reason: &'static str,
}

impl<W: Write> PredictionPipeline<W> {
    pub fn live(
        writer: W,
        processor: PredictionSignalProcessor,
        session: &PredictionJournalSession,
        journal: PredictionJournalWriter,
        checkpoint: PredictionCheckpointContext,
        maximum_updates: u64,
        maximum_bytes: u64,
    ) -> Result<Self, PredictionPipelineError> {
        if !live_contexts_match(&processor, session, &checkpoint) {
            return Err(PredictionPipelineError::Mode(
                "live processor, journal, and checkpoint contexts disagree".to_owned(),
            ));
        }
        Self::build(
            writer,
            processor,
            session,
            Some(journal),
            Some(checkpoint),
            maximum_updates,
            maximum_bytes,
        )
    }

    pub fn replay(
        writer: W,
        processor: PredictionSignalProcessor,
        session: &PredictionJournalSession,
        maximum_updates: u64,
        maximum_bytes: u64,
    ) -> Result<Self, PredictionPipelineError> {
        if session.publisher_epoch != processor.publisher_epoch()
            || session.venue != processor.venue()
        {
            return Err(PredictionPipelineError::Mode(
                "replay processor disagrees with the journal session".to_owned(),
            ));
        }
        Self::build(
            writer,
            processor,
            session,
            None,
            None,
            maximum_updates,
            maximum_bytes,
        )
    }

    fn build(
        writer: W,
        processor: PredictionSignalProcessor,
        session: &PredictionJournalSession,
        journal: Option<PredictionJournalWriter>,
        checkpoint: Option<PredictionCheckpointContext>,
        maximum_updates: u64,
        maximum_bytes: u64,
    ) -> Result<Self, PredictionPipelineError> {
        if maximum_updates == 0 || maximum_bytes == 0 {
            return Err(PredictionPipelineError::Mode(
                "output limits must be positive".to_owned(),
            ));
        }
        let wall_start = DateTime::parse_from_rfc3339(&session.wall_start)
            .map_err(|error| PredictionPipelineError::Mode(error.to_string()))?
            .with_timezone(&Utc);
        let next_admission_seq = session
            .initial_checkpoint
            .as_ref()
            .map_or(0, |checkpoint| checkpoint.next_admission_seq);
        let feed_health = FeedHealthBoard::new(
            processor
                .source_definitions()
                .map(|definition| FeedContract {
                    source_id: definition.source_id.clone(),
                    venue: prediction_feed_transport(&definition.source_id, session.venue)
                        .to_owned(),
                    expected_heartbeat_ms: definition.expected_heartbeat_ms,
                    cadence: prediction_feed_cadence(&definition.source_id),
                    liveness_bound_override_ms: prediction_liveness_override(&definition.source_id),
                }),
            0,
        );
        let signal_intervals_ms = processor
            .definitions()
            .map(|definition| {
                (
                    (definition.signal_id.clone(), definition.version.clone()),
                    definition.approximate_interval_ms,
                )
            })
            .collect();
        let observed_not_consumed =
            prediction_source_routing(session.venue, processor.index_armed(), &processor)?;
        // M1 restart in the gap: the deferred settlement-twin invalidation is
        // runtime state the checkpoint does not carry, but the pending
        // final-decision slot it hangs off IS checkpointed. When a restore brings
        // back a frozen slot, reconstruct the deferred invalidation from it, so
        // the departed final ready decision that releases after the restart is
        // still followed by the twin invalidation in `write_departed_updates` —
        // exactly as in-process. A fresh (non-restored) processor carries no slot,
        // so this is `None` outside a restart. The reason is `market_rollover`: an
        // E37c slot only ever freezes at a scheduled quarter close — the sole
        // departure shape that defers these twins — so it matches the in-process
        // departure reason for every case that can reach here.
        let pending_departed_invalidation =
            processor
                .pending_final_decision_scope()
                .map(
                    |(market_id, market_generation)| DepartedSettlementInvalidation {
                        market_id: market_id.to_owned(),
                        market_generation,
                        reason: "market_rollover",
                    },
                );
        Ok(Self {
            writer,
            processor,
            publisher_epoch: session.publisher_epoch.clone(),
            admission_epoch: session.admission_epoch().to_owned(),
            opened_by_rotation: session.opened_by_rotation,
            wall_start,
            venue: session.venue,
            maximum_updates,
            maximum_bytes,
            journal,
            checkpoint,
            next_admission_seq,
            next_stream_sequence: 0,
            last_signal_batch_sequence: None,
            last_decision_or_status_sequence: None,
            latest_updates: BTreeMap::new(),
            instance_ledger: BTreeMap::new(),
            invalidated_tombstones: BTreeMap::new(),
            signal_intervals_ms,
            venue_gap_active: false,
            producer_health: None,
            next_heartbeat_ns: None,
            next_snapshot_ns: None,
            discovery_attempts: 0,
            bindings: 0,
            market_event_batches: 0,
            spot_event_batches: 0,
            source_events: 0,
            unavailable_transitions: 0,
            health_ticks: 0,
            socket_transitions: 0,
            updates_written: 0,
            stream_messages_written: 0,
            signal_batches_written: 0,
            snapshots_written: 0,
            heartbeats_written: 0,
            status_messages_written: 0,
            reset_messages_written: 0,
            market_transitions_written: 0,
            bytes_written: 0,
            feed_health,
            feed_health_stream: session.runtime_config.feed_health_stream,
            feed_health_written: 0,
            settlement_windows_withheld: 0,
            index_channel_refusals: BTreeMap::new(),
            observed_not_consumed,
            pending_departed_invalidation,
            last_feed_health_revision: None,
            next_feed_health_ns: None,
            stream_hook: None,
            scratch: Vec::with_capacity(1024),
        })
    }

    /// Install an in-process observer of every written stream message
    /// ([`StreamHook`]). The file output is unchanged.
    pub fn with_stream_hook(mut self, hook: Option<StreamHook>) -> Self {
        self.stream_hook = hook;
        self
    }

    pub fn next_admission_seq(&self) -> u64 {
        self.next_admission_seq
    }

    /// Record that the venue refused an index channel's subscription, so every
    /// instance derived from `source_id` publishes `unavailable` under `reason`
    /// until the channel is accepted again.
    ///
    /// Ruling E25: the 5 Hz BRTI channel is intermittently available — the R1
    /// hour was refused on three connections and accepted on the fourth — so a
    /// refusal is a per-source availability fact, not a lane fault and not a
    /// transport outage. It must not mark the 1 Hz channel or the market
    /// channels sick, which is why this is per-source rather than a
    /// `FeedAvailability` transition on the shared `kalshi_index` transport.
    ///
    /// `reason` must be a registered instance-status reason of the `Unavailable`
    /// class; anything else is a programming error and is refused.
    pub fn refuse_index_channel(
        &mut self,
        source_id: &str,
        reason: &'static str,
    ) -> Result<(), PredictionPipelineError> {
        if instance_status_reason_class(reason) != Some(InstanceStatusReasonClass::Unavailable) {
            return Err(PredictionPipelineError::Mode(format!(
                "{reason} is not a registered unavailable instance-status reason"
            )));
        }
        self.index_channel_refusals
            .insert(source_id.to_owned(), reason);
        // A refusal on the armed 1 Hz value channel is the one that reaches the
        // published identities (F2): the settlement twins and the oracle
        // reference publish it as `unavailable`. A 5 Hz refusal (E25) never does,
        // because the processor does not consume that channel.
        if source_id == KALSHI_BRTI_SOURCE_ID {
            self.processor.set_index_channel_reason(Some(reason));
        }
        Ok(())
    }

    /// Clear a refusal once the venue accepts the channel again. Returns true
    /// when a refusal was actually standing, which is what makes the transition
    /// observable rather than idempotent noise.
    pub fn accept_index_channel(&mut self, source_id: &str) -> bool {
        if source_id == KALSHI_BRTI_SOURCE_ID {
            self.processor.set_index_channel_reason(None);
        }
        self.index_channel_refusals.remove(source_id).is_some()
    }

    /// The reason instances derived from `source_id` publish while the venue is
    /// refusing its channel, or `None` when the channel is available.
    pub fn index_channel_unavailable_reason(&self, source_id: &str) -> Option<&'static str> {
        self.index_channel_refusals.get(source_id).copied()
    }

    /// The withheld Kalshi settlement window, if one is outstanding.
    pub fn settlement_window_pause(&self) -> Option<&SettlementWindowPause> {
        self.processor.settlement_window_pause()
    }

    /// Withhold the Kalshi settlement window for a quarter pending a repair.
    ///
    /// Called when the index adapter raises
    /// [`RecoveryAction::BackfillKalshiBrti`](btc_feeds::RecoveryAction). The
    /// pause enters the checkpoint, so a restart before the repair lands resumes
    /// still withholding (audit A1/F4).
    pub fn pause_settlement_window(
        &mut self,
        pause: SettlementWindowPause,
    ) -> Result<(), PredictionPipelineError> {
        self.processor
            .pause_settlement_window(pause)
            .map_err(PredictionPipelineError::Processor)
    }

    /// Release the pause once a repair has covered its interval.
    pub fn resume_settlement_window(&mut self) {
        self.processor.resume_settlement_window();
    }

    /// Count of settlement-window events withheld by an outstanding pause.
    pub fn settlement_windows_withheld(&self) -> u64 {
        self.settlement_windows_withheld
    }

    /// Whether this event is a settlement window the current pause withholds.
    ///
    /// An event whose `quarter_close_time` is missing or unparseable is not
    /// withheld here: it fails the source-definition check in the processor,
    /// which is where a malformed contract belongs.
    fn withholds_settlement_window(&self, event: &SourceEvent) -> bool {
        if event.source_id != KALSHI_BRTI_SETTLEMENT_SOURCE_ID {
            return false;
        }
        let Some(close) = event
            .values
            .get("quarter_close_time")
            .and_then(serde_json::Value::as_str)
            .and_then(|text| DateTime::parse_from_rfc3339(text).ok())
        else {
            return false;
        };
        self.processor
            .withholds_settlement_window(close.timestamp_millis())
    }

    pub fn current_generation_for_auxiliary_source(&self) -> Result<u64, PredictionPipelineError> {
        self.processor
            .active_market_generation()
            .map(Ok)
            .unwrap_or_else(|| {
                self.processor
                    .reserve_next_generation()
                    .map_err(PredictionPipelineError::Processor)
            })
    }

    pub fn admit_live(
        &mut self,
        input: PredictionJournalInput,
    ) -> Result<(), PredictionPipelineError> {
        // Audit A1/F4: a settlement-window event an outstanding pause withholds
        // is dropped BEFORE it enters the journal, not after. The pause is
        // out-of-band recovery state the journal never carries (it lives only in
        // the checkpoint), so journaling the event and dropping it in `apply`
        // would let a replay — which has no pause — ingest and publish the very
        // window the live lane suppressed. Filtering here keeps the journal a
        // record of what was admitted, so replay reproduces live byte for byte.
        let input = self.withhold_settlement_windows(input)?;
        // A batch whose every event was withheld carries nothing to journal or
        // publish. Skip it entirely: the journal holds only admitted inputs, and
        // the admission sequence is not spent on an empty frame (which the
        // journal rejects anyway), so admission stays contiguous.
        if let PredictionJournalInput::MarketEvents { events, .. } = &input
            && events.is_empty()
        {
            return Ok(());
        }
        self.preflight(&input)?;
        let journal = self.journal.as_mut().ok_or_else(|| {
            PredictionPipelineError::Mode("live journal is unavailable".to_owned())
        })?;
        journal.enqueue(input.clone())?;
        let bytes_before = self.bytes_written;
        self.apply(input)?;
        self.flush_written_frames(bytes_before)
    }

    /// Remove the settlement-window events an outstanding pause withholds from a
    /// `MarketEvents` batch, counting each, so only admitted events are
    /// journaled and applied. Every other input passes through untouched. The
    /// batch's `admission_seq` is unchanged, so admission stays contiguous even
    /// when every event in it is withheld.
    fn withhold_settlement_windows(
        &mut self,
        input: PredictionJournalInput,
    ) -> Result<PredictionJournalInput, PredictionPipelineError> {
        let PredictionJournalInput::MarketEvents {
            admission_seq,
            generation,
            events,
            logical_receipt_ns,
            now_ns,
        } = input
        else {
            return Ok(input);
        };
        if !events
            .iter()
            .any(|event| self.withholds_settlement_window(event))
        {
            return Ok(PredictionJournalInput::MarketEvents {
                admission_seq,
                generation,
                events,
                logical_receipt_ns,
                now_ns,
            });
        }
        let mut kept = Vec::with_capacity(events.len());
        for event in events.iter() {
            if self.withholds_settlement_window(event) {
                self.settlement_windows_withheld =
                    checked_add(self.settlement_windows_withheld, 1)?;
            } else {
                kept.push(event.clone());
            }
        }
        Ok(PredictionJournalInput::MarketEvents {
            admission_seq,
            generation,
            events: Arc::new(kept),
            logical_receipt_ns,
            now_ns,
        })
    }

    /// Flush the output writer when an admission published at least one frame.
    /// A lane that only emits control frames would otherwise sit in the
    /// buffered writer until it filled, leaving `output.jsonl` empty and the
    /// stream server without a publisher epoch to read.
    fn flush_written_frames(&mut self, bytes_before: u64) -> Result<(), PredictionPipelineError> {
        if self.bytes_written != bytes_before {
            self.writer.flush()?;
        }
        Ok(())
    }

    pub fn admit_replay(
        &mut self,
        input: PredictionJournalInput,
    ) -> Result<(), PredictionPipelineError> {
        if self.journal.is_some() {
            return Err(PredictionPipelineError::Mode(
                "replay input cannot enter a live pipeline".to_owned(),
            ));
        }
        self.apply(input)
    }

    /// Rotate a REPLAY-mode (apply-only) pipeline onto a new file generation,
    /// keeping the warm processor. The multi lane's in-process rotation (E57)
    /// swaps the physical output file behind the shared writer and hands each
    /// inner lane a fresh tee onto it (`writer`); this resets the stream half
    /// exactly as a fresh generation start does — reset frame, sequence 0, cleared
    /// ledgers, fresh per-generation caps and counters, and admission sequence
    /// back to 0 so the successor generation's merged journal starts contiguous —
    /// while publishing under `session`'s output epoch. The warm processor keeps
    /// its process admission epoch (carried on `session` via
    /// `with_admission_epoch`), so events the still-running feeds stamp with the
    /// process epoch still admit. The caller flushes the shared output before the
    /// swap, so the departed generation's bytes are already durable; the old tee
    /// (dropped when `*self` is overwritten) buffers nothing of its own. Requires
    /// apply-only mode (no live journal or checkpoint); the successor session must
    /// name the same venue as the warm processor.
    pub fn rotate_replay(
        &mut self,
        writer: W,
        session: &PredictionJournalSession,
        caps: FileGenerationCaps,
    ) -> Result<(), PredictionPipelineError> {
        if self.journal.is_some() || self.checkpoint.is_some() {
            return Err(PredictionPipelineError::Mode(
                "rotate_replay requires apply-only (replay) mode".to_owned(),
            ));
        }
        if session.venue != self.venue {
            return Err(PredictionPipelineError::Mode(
                "rotate_replay session venue disagrees with the pipeline".to_owned(),
            ));
        }
        // Rebuild the stream half around the warm processor, exactly as a fresh
        // generation start builds it. The cold placeholder is dropped when `*self`
        // is overwritten; reusing `build` guarantees the reset matches a fresh
        // process (reset frame, seq 0, cleared ledgers, fresh caps/counters).
        let cold = PredictionSignalProcessor::new_indexed(
            self.venue,
            self.publisher_epoch.clone(),
            self.wall_start,
            self.processor.index_armed(),
        )
        .map_err(PredictionPipelineError::Processor)?;
        let mut warm = std::mem::replace(&mut self.processor, cold);
        // C5 Design B: restart every per-identity decision sequence at zero for the
        // successor's output epoch, matching both the fresh-generation start and an
        // independent replay of this generation's journal (whose restore starts the
        // emitters at zero), so the warm live leg and its replay number decisions
        // identically.
        warm.reset_output_sequences();
        let rebuilt = Self::build(
            writer,
            warm,
            session,
            None,
            None,
            caps.max_updates,
            caps.max_output_bytes,
        )?;
        *self = rebuilt;
        Ok(())
    }

    /// Persist the exact accepted state. Replacements are atomic and owner-only.
    pub fn checkpoint_live(
        &mut self,
        now_ns: u64,
    ) -> Result<PredictionCheckpointWriteReport, PredictionPipelineError> {
        let document = self.checkpoint_document(now_ns)?;
        let context = self.checkpoint.as_ref().ok_or_else(|| {
            PredictionPipelineError::Mode("checkpoint context is unavailable".to_owned())
        })?;
        write_prediction_checkpoint(&context.path, document, context.maximum_bytes, true)
            .map_err(PredictionPipelineError::Checkpoint)
    }

    pub fn checkpoint_document(
        &self,
        now_ns: u64,
    ) -> Result<PredictionCheckpointDocument, PredictionPipelineError> {
        let context = self.checkpoint.as_ref().ok_or_else(|| {
            PredictionPipelineError::Mode("checkpoint context is unavailable".to_owned())
        })?;
        PredictionCheckpointDocument::new(
            context.session_id.clone(),
            context.wall_start.clone(),
            now_ns,
            self.next_admission_seq,
            context.contract_fingerprint.clone(),
            context.runtime_config.clone(),
            self.processor.checkpoint_state(),
            context.publisher_epoch_lineage.clone(),
        )
        .map_err(PredictionPipelineError::Checkpoint)
    }

    pub fn finish_live(
        mut self,
        now_ns: u64,
    ) -> Result<
        (
            PredictionPipelineReport,
            PredictionJournalWriteReport,
            PredictionCheckpointWriteReport,
        ),
        PredictionPipelineError,
    > {
        let checkpoint_report = self.checkpoint_live(now_ns)?;
        self.writer.flush()?;
        let end = self.journal_end();
        let journal_report = self
            .journal
            .take()
            .ok_or_else(|| PredictionPipelineError::Mode("live journal is unavailable".to_owned()))?
            .finish(end)?;
        let report = self.report();
        Ok((report, journal_report, checkpoint_report))
    }

    pub fn finish_replay(
        mut self,
        expected: &PredictionJournalEnd,
    ) -> Result<PredictionPipelineReport, PredictionPipelineError> {
        self.writer.flush()?;
        let actual = self.journal_end();
        let mut rewarm_gap = 0_u64;
        let matches = if self.opened_by_rotation {
            // A rotation-opened generation inherits a warm processor whose rolling
            // ring/book/reference state the sealed checkpoint does not carry; a cold
            // replay honestly re-warms exactly those identities, spending more
            // decisions in non-ready states, so its published-frame counts and its
            // whole per-identity `quality_reason_entries` map legitimately differ
            // from the warm live run (ruling 23:30Z). The self-check therefore:
            //  - requires the structural totals (admitted-driven, so reproduced
            //    exactly) to match;
            //  - requires the replay to be NON-VACUOUS — a rotated generation whose
            //    replay emits zero decisions while the live run published some FAILS,
            //    rather than passing on empty structural totals (audit finding 4);
            //  - reports the tolerated published-frame difference as a structured
            //    number (`rotation_rewarm_gap`) instead of silently dropping it.
            // Per-identity fidelity of the NON-ring identities is proven at the
            // stream level by the rotation replay tests, not by the aggregate END;
            // full per-generation byte-identity for ring identities is C5b.
            rewarm_gap = expected
                .totals
                .stream_messages_written
                .abs_diff(actual.totals.stream_messages_written);
            let replay_emitted_no_decisions =
                expected.totals.updates_written > 0 && actual.totals.updates_written == 0;
            structural_totals_match(&actual.totals, &expected.totals)
                && !replay_emitted_no_decisions
        } else {
            &actual == expected
        };
        if !matches {
            return Err(PredictionPipelineError::EndMismatch {
                expected: Box::new(expected.clone()),
                actual: Box::new(actual),
            });
        }
        let mut report = self.report();
        report.rotation_rewarm_gap = rewarm_gap;
        Ok(report)
    }

    fn apply(&mut self, input: PredictionJournalInput) -> Result<(), PredictionPipelineError> {
        self.preflight(&input)?;
        self.apply_prevalidated(input)
    }

    fn preflight(&self, input: &PredictionJournalInput) -> Result<(), PredictionPipelineError> {
        if input.admission_seq() != self.next_admission_seq {
            return Err(PredictionPipelineError::AdmissionSequence {
                expected: self.next_admission_seq,
                actual: input.admission_seq(),
            });
        }
        input.validate_lifecycle_against(
            self.processor
                .active_market_id()
                .zip(self.processor.active_market_generation()),
        )?;
        if let PredictionJournalInput::Bind {
            generation,
            event,
            logical_receipt_ns,
            now_ns,
            ..
        } = input
        {
            if event.publisher_epoch != self.admission_epoch {
                return Err(PredictionPipelineError::Mode(
                    "bind source event publisher_epoch disagrees with the admission epoch"
                        .to_owned(),
                ));
            }
            self.processor.validate_market_binding(
                *generation,
                event,
                *logical_receipt_ns,
                *now_ns,
            )?;
        }
        Ok(())
    }

    fn apply_prevalidated(
        &mut self,
        input: PredictionJournalInput,
    ) -> Result<(), PredictionPipelineError> {
        let now_ns = input.now_ns();
        self.next_admission_seq = checked_add(self.next_admission_seq, 1)?;
        self.ensure_stream_started(now_ns)?;

        match input {
            PredictionJournalInput::Discovery { .. } => {
                self.discovery_attempts = checked_add(self.discovery_attempts, 1)?;
            }
            PredictionJournalInput::Bind {
                generation,
                previous_market_id,
                reason,
                event,
                logical_receipt_ns,
                now_ns,
                ..
            } => {
                let market_id = market_id(self.venue, &event)?;
                self.write_market_transition(
                    now_ns,
                    generation,
                    previous_market_id,
                    Some(market_id),
                    MarketTransitionPhase::Bound,
                    reason,
                )?;
                self.processor
                    .bind_market(generation, &event, logical_receipt_ns, now_ns)?;
                self.venue_gap_active = false;
                self.feed_health.observe_event(&event.source_id, now_ns);
                self.bindings = checked_add(self.bindings, 1)?;
                let updates = self.processor.advance_time(now_ns)?;
                if !updates.is_empty() {
                    self.write_signal_batch(now_ns, updates)?;
                }
                self.write_global_updates(now_ns)?;
                self.write_departed_updates(now_ns)?;
            }
            PredictionJournalInput::MarketEvents {
                generation,
                events,
                logical_receipt_ns,
                now_ns,
                ..
            } => {
                self.validate_market_event_generation(generation, &events)?;
                self.market_event_batches = checked_add(self.market_event_batches, 1)?;
                self.source_events = checked_add(
                    self.source_events,
                    u64::try_from(events.len()).map_err(|_| PredictionPipelineError::Counter)?,
                )?;
                for event in events.iter() {
                    if event.publisher_epoch != self.admission_epoch {
                        return Err(PredictionPipelineError::Mode(
                            "source event publisher_epoch disagrees with the admission epoch"
                                .to_owned(),
                        ));
                    }
                    // E53 follow-up: a source a declared stream produces but no
                    // identity consumes (the armed Kalshi index socket's 5 Hz
                    // value, history and Pyth relay) is routed past the processor
                    // and counted, never a fatal UnsupportedSource. A genuinely
                    // foreign source is not in this set and still refuses below.
                    if self.observed_not_consumed.contains(&event.source_id) {
                        self.feed_health.note_ignored(&event.source_id, now_ns);
                        continue;
                    }
                    // Audit A1/F4: while a quarter's prints are unrepaired, its
                    // settlement window is withheld rather than admitted. On the
                    // LIVE path these events are already filtered out before the
                    // journal (`withhold_settlement_windows`), so this guard is
                    // reached only by a replay carrying a checkpoint pause; it
                    // keeps withholding deterministic across restart.
                    if self.withholds_settlement_window(event) {
                        self.settlement_windows_withheld =
                            checked_add(self.settlement_windows_withheld, 1)?;
                        continue;
                    }
                    // A settlement-window event that is admitted (not withheld)
                    // while a pause stands is proof the adapter has repaired the
                    // gap and moved past the withheld quarter: release the pause
                    // (item a, the resume half). Deterministic from admitted
                    // events, so replay resumes exactly where the live run did.
                    if event.source_id == KALSHI_BRTI_SETTLEMENT_SOURCE_ID
                        && self.processor.settlement_window_pause().is_some()
                    {
                        self.processor.resume_settlement_window();
                    }
                    self.processor.ingest(event, logical_receipt_ns, now_ns)?;
                    self.feed_health.observe_event(&event.source_id, now_ns);
                }
                let updates = self.processor.advance_time(now_ns)?;
                if !updates.is_empty() {
                    self.write_signal_batch(now_ns, updates)?;
                }
                self.write_global_updates(now_ns)?;
                self.write_departed_updates(now_ns)?;
            }
            PredictionJournalInput::SpotEvents {
                events,
                logical_receipt_ns,
                now_ns,
                ..
            } => {
                // Ticker admissions feed the internally hosted spot reference. They
                // are not market-scoped and carry the external feed's publisher
                // epoch, so no market-generation or stream-epoch check applies; the
                // reference validates each ticker contract itself.
                self.spot_event_batches = checked_add(self.spot_event_batches, 1)?;
                self.source_events = checked_add(
                    self.source_events,
                    u64::try_from(events.len()).map_err(|_| PredictionPipelineError::Counter)?,
                )?;
                for event in events.iter() {
                    self.processor.ingest(event, logical_receipt_ns, now_ns)?;
                    self.feed_health.observe_event(&event.source_id, now_ns);
                }
                let updates = self.processor.advance_time(now_ns)?;
                if !updates.is_empty() {
                    self.write_signal_batch(now_ns, updates)?;
                }
                self.write_global_updates(now_ns)?;
                self.write_departed_updates(now_ns)?;
            }
            PredictionJournalInput::Unavailable {
                generation,
                previous_market_id,
                reason,
                now_ns,
                ..
            } => {
                let active_market_id = self
                    .processor
                    .active_market_id()
                    .ok_or_else(|| {
                        PredictionPipelineError::Mode(
                            "a market departure requires a bound venue".to_owned(),
                        )
                    })?
                    .to_owned();
                let active_generation =
                    self.processor.active_market_generation().ok_or_else(|| {
                        PredictionPipelineError::Mode(
                            "a market departure requires an active generation".to_owned(),
                        )
                    })?;
                // Freeze any pending E37c final decision for the departing market
                // BEFORE the ledger invalidation runs, so the invalidation can tell
                // whether this generation departs into a pending final decision.
                // `mark_unavailable` emits no instance evaluation (the venue is now
                // unbound), so moving it ahead of the invalidation reorders no wire
                // frame.
                let updates = self
                    .processor
                    .mark_unavailable(now_ns, unavailable_quality_reason(reason))?;
                self.venue_gap_active = true;
                self.unavailable_transitions = checked_add(self.unavailable_transitions, 1)?;
                if !updates.is_empty() {
                    return Err(PredictionPipelineError::Mode(
                        "an unbound venue produced instance evaluations".to_owned(),
                    ));
                }
                // M1: when this roll froze a final decision, the two settlement
                // identities are NOT invalidated now — their invalidation is
                // deferred to the departed out-of-band path, emitted immediately
                // AFTER the departed final decision publishes (or when the slot
                // expires), so each identity's wire sequence stays ready(final) ->
                // invalidated and never invalidated -> ready.
                let quality_reason = unavailable_quality_reason(reason);
                let defer_settlement_identities = self
                    .processor
                    .pending_final_decision_for_generation(active_generation);
                self.stream_accounting().invalidate_market_generation(
                    now_ns,
                    &active_market_id,
                    active_generation,
                    quality_reason,
                    defer_settlement_identities,
                )?;
                if defer_settlement_identities {
                    self.pending_departed_invalidation = Some(DepartedSettlementInvalidation {
                        market_id: active_market_id.clone(),
                        market_generation: active_generation,
                        reason: quality_reason,
                    });
                }
                self.write_market_transition(
                    now_ns,
                    generation,
                    previous_market_id,
                    None,
                    MarketTransitionPhase::Unavailable,
                    reason,
                )?;
                if let Some((availability, detail)) = feed_availability_for_reason(reason) {
                    self.feed_health.observe_availability(
                        venue_health_label(self.venue),
                        availability,
                        detail,
                        now_ns,
                    );
                }
            }
            PredictionJournalInput::HealthTick { now_ns, .. } => {
                self.health_ticks = checked_add(self.health_ticks, 1)?;
                self.feed_health.observe_tick(now_ns);
                // The health tick is the one-second driver for the embedded spot
                // pipeline: it samples the realized-volatility ring exactly once,
                // matching the published spot cadence.
                let updates = self.processor.health_tick(now_ns)?;
                if !updates.is_empty() {
                    self.write_signal_batch(now_ns, updates)?;
                }
                self.write_global_updates(now_ns)?;
                self.write_departed_updates(now_ns)?;
            }
            PredictionJournalInput::Connectivity {
                transport,
                transition,
                now_ns,
                ..
            } => {
                // A socket-level transition drives only the observe-only board. It
                // is deliberately not a market transition and does not mark the
                // market unavailable: the bound market is unchanged, so no signal
                // is blanked and the signal stream is not perturbed. Any resulting
                // health-frame emission flows through the shared machinery below.
                self.socket_transitions = checked_add(self.socket_transitions, 1)?;
                let venue = transport.health_label();
                match transition {
                    PredictionSocketTransition::Disconnected => {
                        self.feed_health.observe_availability(
                            venue,
                            FeedAvailability::Down,
                            "socket_disconnected",
                            now_ns,
                        )
                    }
                    PredictionSocketTransition::Reconnecting => {
                        self.feed_health.observe_availability(
                            venue,
                            FeedAvailability::Reconnecting,
                            "socket_reconnecting",
                            now_ns,
                        )
                    }
                    PredictionSocketTransition::Connected => {
                        self.feed_health.observe_connected(venue, now_ns);
                    }
                }
                // The Kalshi index socket carries the CF channels (E23/c): its
                // drop after warm-up withholds the three index-dependent
                // instances (`cfbrti_disconnected`, F1); its reconnect returns
                // them to ready. Unlike the market socket, whose drop blanks
                // nothing, this one re-drives a decision so the withdrawal is
                // published at the transition rather than only at the next tick.
                if transport == PredictionTransport::KalshiIndex {
                    let connected = matches!(transition, PredictionSocketTransition::Connected);
                    self.processor.set_index_connected(connected);
                    let updates = self.processor.advance_time(now_ns)?;
                    if !updates.is_empty() {
                        self.write_signal_batch(now_ns, updates)?;
                    }
                    self.write_global_updates(now_ns)?;
                    self.write_departed_updates(now_ns)?;
                }
            }
        }
        self.maybe_write_status(now_ns)?;
        self.maybe_write_snapshot(now_ns)?;
        self.maybe_write_heartbeat(now_ns)?;
        self.maybe_write_feed_health(now_ns)?;
        Ok(())
    }

    fn validate_market_event_generation(
        &self,
        generation: u64,
        events: &[SourceEvent],
    ) -> Result<(), PredictionPipelineError> {
        // Index events carry no market binding, exactly as the Polymarket TWAP
        // relay does: they take the current generation rather than requiring a
        // bound market, so a batch that arrives between two market generations
        // is admitted instead of refused.
        let auxiliary_only = events
            .iter()
            .all(|event| is_auxiliary_source(&event.source_id));
        let expected = if auxiliary_only {
            self.current_generation_for_auxiliary_source()?
        } else {
            self.processor.active_market_generation().ok_or_else(|| {
                PredictionPipelineError::Mode(
                    "market-relative source events arrived without a bound market".to_owned(),
                )
            })?
        };
        if generation != expected {
            return Err(PredictionPipelineError::Generation {
                expected,
                actual: generation,
            });
        }
        Ok(())
    }

    fn stream_accounting(&mut self) -> StreamAccounting<'_, W> {
        StreamAccounting {
            writer: &mut self.writer,
            hook: self.stream_hook.as_ref(),
            scratch: &mut self.scratch,
            publisher_epoch: &self.publisher_epoch,
            wall_start: self.wall_start,
            maximum_updates: self.maximum_updates,
            maximum_bytes: self.maximum_bytes,
            next_stream_sequence: &mut self.next_stream_sequence,
            last_signal_batch_sequence: &mut self.last_signal_batch_sequence,
            last_decision_or_status_sequence: &mut self.last_decision_or_status_sequence,
            latest_updates: &mut self.latest_updates,
            instance_ledger: &mut self.instance_ledger,
            invalidated_tombstones: &mut self.invalidated_tombstones,
            signal_intervals_ms: &self.signal_intervals_ms,
            venue_gap_active: self.venue_gap_active,
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

    fn ensure_stream_started(&mut self, now_ns: u64) -> Result<(), PredictionPipelineError> {
        self.stream_accounting().ensure_stream_started(now_ns)?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn write_market_transition(
        &mut self,
        now_ns: u64,
        generation: u64,
        previous_market_id: Option<String>,
        next_market_id: Option<String>,
        phase: MarketTransitionPhase,
        reason: MarketTransitionReason,
    ) -> Result<(), PredictionPipelineError> {
        let message = SignalStreamMessage::MarketTransition {
            schema_version: SIGNAL_STREAM_SCHEMA_VERSION,
            publisher_epoch: self.publisher_epoch.clone(),
            sequence: self.next_stream_sequence,
            produced_at: self.stream_timestamp(now_ns)?,
            venue: self.venue,
            market_generation: generation,
            previous_market_id,
            next_market_id,
            phase,
            reason,
        };
        self.stream_accounting().write_stream_message(&message)?;
        Ok(())
    }

    /// Publish one evaluation pass. Completeness is checked against the whole
    /// evaluated set; only the instances the coalescing rule publishes are
    /// written, so an unchanged instance neither consumes a decision sequence
    /// nor moves the published state a snapshot restates. A pass whose every
    /// ready instance is suppressed writes nothing at all: an empty
    /// `signal_batch` is not a frame this producer emits.
    fn write_signal_batch(
        &mut self,
        now_ns: u64,
        batch: PredictionBatch,
    ) -> Result<(), PredictionPipelineError> {
        // The expected size is this venue's own set for its arming: Polymarket
        // publishes the two settlement-anchor identities, and Kalshi does too
        // while its index socket is armed (E43), so one shared count would accept
        // a short batch and refuse a complete one.
        let expected = prediction_signal_count_for(self.venue, self.processor.index_armed());
        if batch.evaluations().len() != expected {
            return Err(PredictionPipelineError::Mode(format!(
                "prediction batch contains {} updates; expected {expected} for {:?}",
                batch.evaluations().len(),
                self.venue
            )));
        }
        let mut updates = batch.into_published();
        if updates.is_empty() {
            return Ok(());
        }
        self.restamp_output_epoch(&mut updates);
        self.stream_accounting()
            .write_signal_batch(now_ns, updates)?;
        Ok(())
    }

    /// Stamp this generation's output `publisher_epoch` onto every update before
    /// it reaches the wire. The processor emits under its construction epoch,
    /// which on a warm pipeline that has rotated is the process *admission*
    /// epoch, not this generation's output epoch; the published frame's envelope
    /// and its updates must agree (`validate_batch_updates`). Outside rotation
    /// output == admission, so this is a no-op and every existing byte is
    /// unchanged.
    fn restamp_output_epoch(&self, updates: &mut [SignalUpdate]) {
        for update in updates.iter_mut() {
            if update.publisher_epoch != self.publisher_epoch {
                update.publisher_epoch.clone_from(&self.publisher_epoch);
            }
        }
    }

    fn maybe_write_status(&mut self, now_ns: u64) -> Result<(), PredictionPipelineError> {
        // The producer-health count is over every instance in the ledger, so it
        // includes the global oracle reference when the index socket is armed —
        // unlike the market completeness guard, which sizes one venue's batch.
        let expected = prediction_signal_count_for(self.venue, self.processor.index_armed())
            + self.global_instance_count();
        self.stream_accounting()
            .maybe_write_status(now_ns, expected)?;
        Ok(())
    }

    /// The number of global-scope instances this lane publishes: the CF BRTI
    /// oracle reference while the Kalshi index socket is armed, otherwise none.
    fn global_instance_count(&self) -> usize {
        usize::from(self.processor.index_armed() && self.venue == PredictionMarketVenue::Kalshi)
    }

    /// Write any global-scope updates the last decision produced (the oracle
    /// reference) outside the per-market completeness guard. A global identity is
    /// not part of a venue's market batch, so it is written on its own — including
    /// while the venue is unbound, when no market batch is written at all.
    fn write_global_updates(&mut self, now_ns: u64) -> Result<(), PredictionPipelineError> {
        let mut updates = self.processor.take_global_updates();
        if updates.is_empty() {
            return Ok(());
        }
        self.restamp_output_epoch(&mut updates);
        self.stream_accounting()
            .write_signal_batch(now_ns, updates)?;
        Ok(())
    }

    /// Write a departed market's released E37c final decision (both twins)
    /// outside the per-market completeness guard, exactly like
    /// [`Self::write_global_updates`], since the pair is scoped to the departed
    /// market rather than the bound one. Also drains any pending final decisions
    /// that expired unpublished on this decision into feed-health telemetry.
    fn write_departed_updates(&mut self, now_ns: u64) -> Result<(), PredictionPipelineError> {
        let expired = self.processor.take_expired_final_decisions();
        if expired > 0 {
            self.feed_health
                .note_final_decisions_expired(u64::from(expired), now_ns);
        }
        let mut updates = self.processor.take_departed_updates();
        let released = !updates.is_empty();
        if released {
            self.restamp_output_epoch(&mut updates);
            self.stream_accounting()
                .write_signal_batch(now_ns, updates)?;
        }
        // M1: a departing generation that froze a final decision deferred the two
        // settlement identities' invalidation to here. It is emitted immediately
        // AFTER the departed final decision publishes, or when its slot expired
        // unpublished — never before — so each identity's wire sequence is
        // ready(final) -> invalidated and never invalidated -> ready.
        if (released || expired > 0)
            && let Some(pending) = self.pending_departed_invalidation.take()
        {
            self.stream_accounting()
                .invalidate_departed_settlement_identities(
                    now_ns,
                    &pending.market_id,
                    pending.market_generation,
                    pending.reason,
                )?;
        }
        Ok(())
    }

    fn maybe_write_snapshot(&mut self, now_ns: u64) -> Result<(), PredictionPipelineError> {
        self.stream_accounting().maybe_write_snapshot(now_ns)?;
        Ok(())
    }

    fn maybe_write_heartbeat(&mut self, now_ns: u64) -> Result<(), PredictionPipelineError> {
        self.stream_accounting().maybe_write_heartbeat(now_ns)?;
        Ok(())
    }

    /// Emit an observe-only `feed_health` frame when the board's reportable state
    /// transitions, and at least once per bounded periodic cadence. Gated on the
    /// session's fingerprinted capability so a pre-capability replay emits none.
    fn maybe_write_feed_health(&mut self, now_ns: u64) -> Result<(), PredictionPipelineError> {
        if !self.feed_health_stream {
            return Ok(());
        }
        let revision = self.feed_health.emission_revision();
        let changed = self.last_feed_health_revision != Some(revision);
        let periodic_due = self
            .next_feed_health_ns
            .is_none_or(|deadline| now_ns >= deadline);
        if !changed && !periodic_due {
            return Ok(());
        }
        let snapshot = self.feed_health.snapshot();
        self.stream_accounting()
            .write_feed_health(now_ns, &snapshot)?;
        self.last_feed_health_revision = Some(revision);
        self.next_feed_health_ns = Some(now_ns.saturating_add(FEED_HEALTH_PERIODIC_INTERVAL_NS));
        Ok(())
    }

    fn stream_timestamp(&self, now_ns: u64) -> Result<String, PredictionPipelineError> {
        Ok(crate::stream_accounting::stream_timestamp(
            &self.wall_start,
            now_ns,
        )?)
    }

    fn journal_end(&self) -> PredictionJournalEnd {
        PredictionJournalEnd {
            totals: PredictionJournalTotals {
                admitted_inputs: self.next_admission_seq,
                discovery_attempts: self.discovery_attempts,
                bindings: self.bindings,
                market_event_batches: self.market_event_batches,
                spot_event_batches: self.spot_event_batches,
                unavailable_transitions: self.unavailable_transitions,
                health_ticks: self.health_ticks,
                updates_written: self.updates_written,
                stream_messages_written: self.stream_messages_written,
            },
            quality_reason_entries: self.processor.quality_reason_entries(),
        }
    }

    fn report(&self) -> PredictionPipelineReport {
        PredictionPipelineReport {
            admitted_inputs: self.next_admission_seq,
            discovery_attempts: self.discovery_attempts,
            bindings: self.bindings,
            market_event_batches: self.market_event_batches,
            spot_event_batches: self.spot_event_batches,
            source_events: self.source_events,
            unavailable_transitions: self.unavailable_transitions,
            health_ticks: self.health_ticks,
            socket_transitions: self.socket_transitions,
            updates_written: self.updates_written,
            stream_messages_written: self.stream_messages_written,
            signal_batches_written: self.signal_batches_written,
            snapshots_written: self.snapshots_written,
            heartbeats_written: self.heartbeats_written,
            status_messages_written: self.status_messages_written,
            reset_messages_written: self.reset_messages_written,
            market_transitions_written: self.market_transitions_written,
            bytes_written: self.bytes_written,
            quality_reason_entries: self.processor.quality_reason_entries(),
            feed_health: self.feed_health.snapshot(),
            feed_health_messages_written: self.feed_health_written,
            // Set only by `finish_replay` on the rotation re-warm path; every other
            // report path (live finish, unrotated replay) has no re-warm gap.
            rotation_rewarm_gap: 0,
        }
    }
}

/// Stable feed-health venue label for a prediction venue.
/// The capture streams a prediction lane declares. Their union of
/// [`CaptureStream::produced_source_ids`] is everything that can reach the
/// processor's admission boundary through the market-event path; the spot
/// reference tickers (Coinbase/Kraken cores) enter through the reference and are
/// handled before the boundary, so they are not declared here.
fn prediction_declared_streams(
    venue: PredictionMarketVenue,
    index_armed: bool,
) -> Vec<CaptureStream> {
    match venue {
        PredictionMarketVenue::Kalshi => {
            let mut streams = vec![CaptureStream::KalshiMarket];
            // The index channels ride the one Kalshi market socket, armed only
            // when the venue gate armed a family (E23/E43).
            if index_armed {
                streams.push(CaptureStream::KalshiIndex);
            }
            streams
        }
        PredictionMarketVenue::Polymarket => vec![
            CaptureStream::PolymarketMarket,
            CaptureStream::PolymarketChainlinkTwap,
        ],
    }
}

/// Compute the observed-not-consumed source set for a prediction lane and refuse
/// at start-up if any consumed source has no declared producer (E53 follow-up,
/// mirroring the venue lane's F1 `configure_source_routing`).
///
/// `produced` is the union of the declared streams' inventories; `consumed` is
/// the processor's own source set. Every consumed source must be produced —
/// otherwise the lane is mis-declared (e.g. armed to consume the CF value with
/// no index stream) and refuses rather than warming forever. The returned set is
/// `produced − consumed`: sources the socket delivers that no identity consumes
/// (the armed index socket's 5 Hz value, history and Pyth relay), routed past the
/// processor and counted instead of aborting the lane.
fn prediction_source_routing(
    venue: PredictionMarketVenue,
    index_armed: bool,
    processor: &PredictionSignalProcessor,
) -> Result<BTreeSet<String>, PredictionPipelineError> {
    let produced: BTreeSet<String> = prediction_declared_streams(venue, index_armed)
        .into_iter()
        .flat_map(|stream| {
            stream
                .produced_source_ids()
                .iter()
                .map(|id| (*id).to_owned())
        })
        .collect();
    let consumed = processor.consumed_source_ids();
    if let Some(missing) = consumed.iter().find(|id| !produced.contains(*id)) {
        return Err(PredictionPipelineError::Mode(format!(
            "prediction lane consumes {missing} but no declared stream produces it"
        )));
    }
    Ok(produced.difference(&consumed).cloned().collect())
}

fn venue_health_label(venue: PredictionMarketVenue) -> &'static str {
    match venue {
        PredictionMarketVenue::Polymarket => "polymarket",
        PredictionMarketVenue::Kalshi => "kalshi",
    }
}

/// Transport ownership for each source consumed by a prediction runner. Market
/// socket transitions must not mark the independently connected spot-reference
/// or TWAP inputs sick merely because they share the same process.
fn prediction_feed_transport(source_id: &str, venue: PredictionMarketVenue) -> &'static str {
    match source_id {
        COINBASE_TICKER_SOURCE_ID => "coinbase",
        KRAKEN_TICKER_SOURCE_ID => "kraken",
        POLYMARKET_TWAP_SOURCE_ID => "polymarket_rtds",
        // The Kalshi index socket is independently connected: a market-socket
        // rollover must not mark BRTI or the Pyth relay sick.
        KALSHI_BRTI_SOURCE_ID
        | KALSHI_BRTI_SETTLEMENT_SOURCE_ID
        | KALSHI_BRTI_5HZ_SOURCE_ID
        | KALSHI_PYTH_SOURCE_ID => "kalshi_index",
        _ => venue_health_label(venue),
    }
}

/// Sources that are not bound to the lane's market and therefore take the
/// current generation rather than requiring an active binding: the Polymarket
/// Chainlink TWAP relay and the four Kalshi index channels.
fn is_auxiliary_source(source_id: &str) -> bool {
    matches!(
        source_id,
        POLYMARKET_TWAP_SOURCE_ID
            | KALSHI_BRTI_SOURCE_ID
            | KALSHI_BRTI_SETTLEMENT_SOURCE_ID
            | KALSHI_BRTI_5HZ_SOURCE_ID
            | KALSHI_PYTH_SOURCE_ID
    )
}

/// Cadence class for the feed-health companion rule, over the closed set of
/// source_ids this prediction runner consumes (both venues). Trades, quotes, and
/// market-contract metadata publish on market activity (event-driven); the
/// book-levels snapshot and the Polymarket TWAP series refresh on a timer
/// (periodic). An unrecognized id defaults to `Periodic` (degrade on bound), the
/// conservative choice for a feed of unknown cadence.
fn prediction_feed_cadence(source_id: &str) -> FeedCadence {
    match source_id {
        "kalshi.btc.trades"
        | "kalshi.btc.quotes"
        | "kalshi.btc.market-contracts"
        | "polymarket.btc.trades"
        | "polymarket.btc.quotes"
        | "polymarket.btc.market-contracts" => FeedCadence::EventDriven,
        "kalshi.btc.book-levels" | "polymarket.btc.book-levels" | POLYMARKET_TWAP_SOURCE_ID => {
            FeedCadence::Periodic
        }
        // The index channels publish on a clock, not on market activity: 1 Hz
        // and 5 Hz (both observed), and 60 messages per quarter for the
        // settlement window. The Pyth relay has delivered no frame at all, so it
        // takes the same conservative class as every unrecognized id.
        KALSHI_BRTI_SOURCE_ID
        | KALSHI_BRTI_SETTLEMENT_SOURCE_ID
        | KALSHI_BRTI_5HZ_SOURCE_ID
        | KALSHI_PYTH_SOURCE_ID => FeedCadence::Periodic,
        _ => FeedCadence::Periodic,
    }
}

/// Calibrated liveness-bound override, in milliseconds, for the feed-health
/// board, over the closed set of source_ids this prediction runner consumes. The
/// Polymarket TWAP series publishes every 60 s, past the 30 s default, so it
/// false-degrades each cycle; 180 s is that 60 s cadence × the missed-tick
/// allowance the declared path uses. Other prediction feeds keep their default
/// until a soak measures them.
fn prediction_liveness_override(source_id: &str) -> Option<u64> {
    match source_id {
        // 60 s publication cadence × 3 (MISSED_HEARTBEAT_ALLOWANCE)
        POLYMARKET_TWAP_SOURCE_ID => Some(180_000),
        // 1 Hz × 10, and the 1 Hz cadence is now OBSERVED — 120 frames in 120 s
        // on the R1 probe (ruling E23). Deliberately looser than the 5 s signal
        // staleness bound, so the board and the signal fail for different
        // reasons rather than flapping together. Still provisional as a bound:
        // a soak, not a 2-minute probe, sizes the allowance.
        KALSHI_BRTI_SOURCE_ID => Some(10_000),
        // 200 ms × 25. The 5 Hz grid is observed exact (2,201 consecutive 200 ms
        // steps, ruling E25), but the channel is intermittently refused
        // altogether — a refusal publishes `unavailable` through the venue gate
        // rather than waiting this bound out.
        KALSHI_BRTI_5HZ_SOURCE_ID => Some(5_000),
        // 60 messages in the final minute of each quarter, then silence: the
        // bound is the 900 s cycle plus 60 s of slack.
        KALSHI_BRTI_SETTLEMENT_SOURCE_ID => Some(960_000),
        // The Pyth relay delivered no frame in the probe, so no cadence is
        // known at all and it keeps the conservative 30 s default.
        _ => None,
    }
}

/// Map an admitted prediction unavailability reason onto the observe-only board's
/// abstract availability signal. A `ScheduledRollover` is a healthy market
/// handoff — the transport stays up — so it yields `None` and never marks the
/// feed sick. The other admitted unavailability reasons are genuine data-flow
/// losses, mapped to a down or reconnecting outage.
fn feed_availability_for_reason(
    reason: MarketTransitionReason,
) -> Option<(FeedAvailability, &'static str)> {
    match reason {
        MarketTransitionReason::ScheduledRollover
        | MarketTransitionReason::InitialBind
        | MarketTransitionReason::RecoveryBind => None,
        MarketTransitionReason::DiscoveryUnreachable => {
            Some((FeedAvailability::Down, "discovery_unreachable"))
        }
        MarketTransitionReason::InvalidDiscovery => {
            Some((FeedAvailability::Down, "invalid_discovery"))
        }
        MarketTransitionReason::NextMarketUnlisted => {
            Some((FeedAvailability::Reconnecting, "next_market_unlisted"))
        }
        MarketTransitionReason::CloseToNextOpenGap => {
            Some((FeedAvailability::Reconnecting, "close_to_next_open_gap"))
        }
        MarketTransitionReason::ProviderLifecycleChange => {
            Some((FeedAvailability::Reconnecting, "provider_lifecycle_change"))
        }
    }
}

fn live_contexts_match(
    processor: &PredictionSignalProcessor,
    session: &PredictionJournalSession,
    checkpoint: &PredictionCheckpointContext,
) -> bool {
    session.publisher_epoch == processor.publisher_epoch()
        && session.venue == processor.venue()
        && checkpoint.session_id == session.session_id
        && checkpoint.wall_start == session.wall_start
        && checkpoint.contract_fingerprint == session.contract_fingerprint
        && checkpoint.runtime_config == session.runtime_config
        && checkpoint
            .publisher_epoch_lineage
            .last()
            .map(String::as_str)
            == Some(processor.publisher_epoch())
}

impl PredictionPipeline<BufWriter<File>> {
    pub fn sync_output(&mut self) -> Result<(), PredictionPipelineError> {
        self.writer.flush()?;
        self.writer.get_ref().sync_data()?;
        Ok(())
    }

    /// Seal the current file generation and continue into `open` in place (C5
    /// in-process rotation).
    ///
    /// `open` is a file generation whose directory, `RUN-META.json` and `CURRENT`
    /// have already been published by
    /// [`crate::file_generation::FileGenerationLayout::open_next_with_rotation`];
    /// this fills its `output.jsonl`/`journal.jsonl`. The seal runs today's
    /// graceful-end sequence — `sync_output` → journal completion (END record) →
    /// `checkpoint_live(now_ns)` — without exiting. The caller seals
    /// `RUN-META.json` (`OpenFileGeneration::seal`) on the *departed* generation
    /// separately; this returns once the successor is publishing.
    ///
    /// **Two-epoch split.** The warm processor, the process *admission* epoch
    /// (`--publisher-epoch`, what the still-running feeds keep stamping and what
    /// admitted events are validated against), the process clock origin
    /// (`wall_start`) and the admission sequence (`next_admission_seq`, carried
    /// in the seal checkpoint) all survive the swap untouched. Everything the
    /// stream side keeps — the output/journal writers, the reset/producer-health
    /// state, `next_stream_sequence`, the published-state ledgers/tombstones and
    /// every per-generation cap and counter — is reset exactly as a fresh process
    /// start resets it, and `output_epoch` (`<prefix>-<unix>`) becomes this
    /// generation's `publisher_epoch` for the reset frame and all emitted frames.
    /// The successor's journal carries the seal checkpoint as its
    /// `initial_checkpoint`, so it replays from that point; ring identities the
    /// checkpoint does not carry re-warm on replay while the live successor keeps
    /// them warm from the in-memory processor (honest re-warm; full per-generation
    /// byte-identity for ring identities is C5b).
    ///
    /// A failure after the seal leaves the departed generation completely sealed
    /// on disk and returns the error; the caller exits rather than writing on
    /// into files that were never opened.
    /// Run today's graceful-end sequence as a seal, without exiting: flush and
    /// fsync the output, write the journal's END record, and persist a final
    /// checkpoint at `now_ns`. Leaves `self.journal` as `None`; the caller either
    /// opens a successor ([`Self::rotate`]) or exits. Used both by rotation and by
    /// the failure path that seals the still-open generation before bailing when a
    /// successor cannot be opened.
    pub fn seal_current(&mut self, now_ns: u64) -> Result<(), PredictionPipelineError> {
        self.sync_output()?;
        let end = self.journal_end();
        let journal = self.journal.take().ok_or_else(|| {
            PredictionPipelineError::Mode("live journal is unavailable at seal".to_owned())
        })?;
        journal.finish(end)?;
        self.checkpoint_live(now_ns)?;
        Ok(())
    }

    pub fn rotate(
        &mut self,
        now_ns: u64,
        open: &OpenFileGeneration,
        output_epoch: String,
        caps: FileGenerationCaps,
    ) -> Result<(), PredictionPipelineError> {
        // 1. Seal the departed generation: output → journal END → checkpoint.
        self.seal_current(now_ns)?;

        // 2. Build the successor's journal session from the seal checkpoint,
        //    carrying the process admission epoch while publishing under the new
        //    per-generation output epoch.
        let seal_document = self.checkpoint_document(now_ns)?;
        let context = self.checkpoint.as_ref().ok_or_else(|| {
            PredictionPipelineError::Mode(
                "checkpoint context is unavailable at rotation".to_owned(),
            )
        })?;
        let admission_epoch = self.admission_epoch.clone();
        let runtime_config = context.runtime_config.clone();
        let checkpoint_path = context.path.clone();
        let checkpoint_max_bytes = context.maximum_bytes;
        // Record the successor's output epoch in the lineage exactly as a resume
        // records the process epoch (`bin::run` pushes `--publisher-epoch` onto the
        // restored lineage): each file generation is a fresh publisher epoch, so
        // the sealed checkpoint's lineage lists every generation in order.
        let mut lineage = context.publisher_epoch_lineage.clone();
        lineage.push(output_epoch.clone());
        let session =
            PredictionJournalSession::resumed(output_epoch, seal_document, runtime_config)
                .map_err(|error| PredictionPipelineError::Mode(error.to_string()))?
                .with_admission_epoch(admission_epoch)
                .map_err(|error| PredictionPipelineError::Mode(error.to_string()))?
                .opened_by_rotation();

        // 3. Open the successor's output and journal writers, and its checkpoint
        //    context (same shared checkpoint path and admission lineage).
        let output = private_output(&open.output_path)?;
        let writer = BufWriter::with_capacity(1024 * 1024, output);
        let journal = PredictionJournalWriter::create(
            &open.journal_path,
            session.clone(),
            caps.max_journal_bytes,
        )?;
        let checkpoint_context = PredictionCheckpointContext {
            path: checkpoint_path,
            maximum_bytes: checkpoint_max_bytes,
            session_id: session.session_id.clone(),
            wall_start: session.wall_start.clone(),
            contract_fingerprint: session.contract_fingerprint.clone(),
            runtime_config: session.runtime_config.clone(),
            publisher_epoch_lineage: lineage,
        };

        // 4. Rebuild the stream half around the warm processor. The mem::replace
        //    parks a cold placeholder that is dropped when `*self` is overwritten;
        //    reusing `build` guarantees the reset matches a fresh process exactly.
        let cold = PredictionSignalProcessor::new_indexed(
            self.venue,
            self.publisher_epoch.clone(),
            self.wall_start,
            self.processor.index_armed(),
        )
        .map_err(PredictionPipelineError::Processor)?;
        let mut warm = std::mem::replace(&mut self.processor, cold);
        // C5 Design B: each generation is a fresh output epoch, so every
        // per-identity decision sequence restarts at zero — the same reset a
        // resume's fresh rebuild performs — while the warm feature/ring state
        // crosses the boundary intact. Without this the warm emitters would carry
        // their departed sequence into the successor's epoch, breaking the
        // "first ready decision in an epoch is sequence zero" contract.
        warm.reset_output_sequences();
        let rebuilt = Self::build(
            writer,
            warm,
            &session,
            Some(journal),
            Some(checkpoint_context),
            caps.max_updates,
            caps.max_output_bytes,
        )?;
        *self = rebuilt;
        Ok(())
    }
}

/// The journal-END totals driven purely by the admitted inputs — which a replay
/// reproduces exactly regardless of warm-up — excluding the published-frame counts
/// (`updates_written`, `stream_messages_written`) that a rotation-opened
/// generation's cold replay re-warms differently. Used only on the C5 rotation
/// re-warm path; every other replay still requires the full END to match.
fn structural_totals_match(
    actual: &PredictionJournalTotals,
    expected: &PredictionJournalTotals,
) -> bool {
    actual.admitted_inputs == expected.admitted_inputs
        && actual.discovery_attempts == expected.discovery_attempts
        && actual.bindings == expected.bindings
        && actual.market_event_batches == expected.market_event_batches
        && actual.spot_event_batches == expected.spot_event_batches
        && actual.unavailable_transitions == expected.unavailable_transitions
        && actual.health_ticks == expected.health_ticks
}

/// Create an owner-only (`0o600`) `output.jsonl` for a freshly opened generation,
/// refusing to clobber an existing file — the same discipline the binary uses for
/// the first generation's output.
fn private_output(path: &std::path::Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

fn market_id(
    venue: PredictionMarketVenue,
    event: &SourceEvent,
) -> Result<String, PredictionPipelineError> {
    let field = match venue {
        PredictionMarketVenue::Polymarket => "market_id",
        PredictionMarketVenue::Kalshi => "market_ticker",
    };
    event
        .values
        .get(field)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= 256)
        .map(ToOwned::to_owned)
        .ok_or_else(|| {
            PredictionPipelineError::Mode(format!("bind source event lacks a bounded {field}"))
        })
}

fn unavailable_quality_reason(reason: MarketTransitionReason) -> &'static str {
    match reason {
        MarketTransitionReason::ScheduledRollover => "market_rollover",
        MarketTransitionReason::DiscoveryUnreachable => "market_discovery_unreachable",
        MarketTransitionReason::NextMarketUnlisted => "next_market_unlisted",
        MarketTransitionReason::InvalidDiscovery => "market_discovery_invalid",
        MarketTransitionReason::CloseToNextOpenGap => "market_open_gap",
        MarketTransitionReason::ProviderLifecycleChange => "provider_lifecycle_change",
        MarketTransitionReason::InitialBind | MarketTransitionReason::RecoveryBind => {
            "market_unavailable"
        }
    }
}

fn checked_add(value: u64, increment: u64) -> Result<u64, PredictionPipelineError> {
    value
        .checked_add(increment)
        .ok_or(PredictionPipelineError::Counter)
}

#[derive(Debug, Error)]
pub enum PredictionPipelineError {
    #[error("prediction pipeline mode error: {0}")]
    Mode(String),
    #[error("prediction admission sequence mismatch: expected {expected}, found {actual}")]
    AdmissionSequence { expected: u64, actual: u64 },
    #[error("prediction market generation mismatch: expected {expected}, found {actual}")]
    Generation { expected: u64, actual: u64 },
    #[error("prediction stream sequence mismatch: expected {expected}, found {actual}")]
    StreamSequence { expected: u64, actual: u64 },
    #[error("prediction output exceeds {0} updates")]
    UpdateLimit(u64),
    #[error("prediction output exceeds {0} bytes")]
    ByteLimit(u64),
    #[error("prediction replay end record differs from recomputed state")]
    EndMismatch {
        expected: Box<PredictionJournalEnd>,
        actual: Box<PredictionJournalEnd>,
    },
    #[error("prediction pipeline counter exhausted")]
    Counter,
    #[error(transparent)]
    Processor(#[from] PredictionSignalError),
    #[error(transparent)]
    Journal(#[from] PredictionJournalError),
    #[error(transparent)]
    Checkpoint(#[from] PredictionCheckpointError),
    #[error(transparent)]
    Validation(#[from] ValidationError),
    #[error("prediction output I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("prediction output JSON failed: {0}")]
    Json(#[from] serde_json::Error),
}

impl From<StreamAccountingError> for PredictionPipelineError {
    fn from(error: StreamAccountingError) -> Self {
        match error {
            StreamAccountingError::StreamSequence { expected, actual } => {
                Self::StreamSequence { expected, actual }
            }
            StreamAccountingError::UpdateLimit(limit) => Self::UpdateLimit(limit),
            StreamAccountingError::ByteLimit(limit) => Self::ByteLimit(limit),
            StreamAccountingError::Counter => Self::Counter,
            StreamAccountingError::Validation(error) => Self::Validation(error),
            StreamAccountingError::Json(error) => Self::Json(error),
            StreamAccountingError::Timestamp(error) => Self::Mode(error.to_string()),
            StreamAccountingError::Io(error) => Self::Io(error),
            StreamAccountingError::MissingSnapshotAnchor => {
                Self::Mode("snapshot has no complete batch anchor".to_owned())
            }
            StreamAccountingError::MissingStreamHealth => {
                Self::Mode("stream health is absent".to_owned())
            }
            StreamAccountingError::UnexpectedMarketTransition => Self::Mode(
                "unexpected market transition in prediction stream accounting".to_owned(),
            ),
            StreamAccountingError::UnexpectedFeedHealth => Self::Mode(
                "unexpected feed health frame in prediction stream accounting".to_owned(),
            ),
            StreamAccountingError::UnknownInstanceStatusReason(reason) => Self::Mode(format!(
                "instance status reason is outside the frozen registry: {reason}"
            )),
            StreamAccountingError::MissingSignalInterval(id, version) => {
                Self::Mode(format!("signal interval is missing for {id}@{version}"))
            }
            StreamAccountingError::InvalidDepartureReason(reason) => {
                Self::Mode(format!("invalid departure reason {reason}"))
            }
            StreamAccountingError::InvalidReadyQuality => {
                Self::Mode("ready decision carried quality reasons".to_owned())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use serde_json::{Value, json};
    use signals_core::{FeedHealthStatus, InstanceStatusState, SignalInstanceKey, SignalScope};

    use super::*;
    use crate::prediction_signal_count;

    const EPOCH: &str = "prediction-runtime-test";
    const SECOND_NS: u64 = 1_000_000_000;

    #[test]
    fn rejected_contract_writes_no_bound_frame_and_consumes_no_admission() {
        let session = PredictionJournalSession::new(
            "2026-08-15T12:00:00Z".to_owned(),
            EPOCH.to_owned(),
            PredictionRuntimeConfig::new(PredictionMarketVenue::Kalshi, 2_000),
        )
        .expect("session builds");
        let processor = PredictionSignalProcessor::new(
            PredictionMarketVenue::Kalshi,
            EPOCH,
            "2026-08-15T12:00:00Z".parse().expect("wall start parses"),
        )
        .expect("processor builds");
        let mut pipeline =
            PredictionPipeline::replay(Vec::new(), processor, &session, 1_000, 1_000_000)
                .expect("pipeline builds");
        let mut event = kalshi_contract(0, 1, 0);
        event
            .values
            .insert("close_time".to_owned(), json!(timestamp(0)));
        let input = PredictionJournalInput::Bind {
            admission_seq: 0,
            generation: 1,
            previous_market_id: None,
            reason: MarketTransitionReason::InitialBind,
            event,
            logical_receipt_ns: 0,
            now_ns: 0,
        };

        pipeline
            .admit_replay(input)
            .expect_err("invalid contract is rejected before publication");

        assert_eq!(pipeline.next_admission_seq(), 0);
        assert!(pipeline.writer.is_empty());
    }

    #[test]
    fn live_context_requires_lineage_to_end_at_processor_epoch() {
        let session = PredictionJournalSession::new(
            "2026-08-15T12:00:00Z".to_owned(),
            EPOCH.to_owned(),
            PredictionRuntimeConfig::new(PredictionMarketVenue::Kalshi, 2_000),
        )
        .expect("session builds");
        let processor = PredictionSignalProcessor::new(
            PredictionMarketVenue::Kalshi,
            EPOCH,
            "2026-08-15T12:00:00Z".parse().expect("wall start parses"),
        )
        .expect("processor builds");
        let mut checkpoint = PredictionCheckpointContext {
            path: PathBuf::from("unused-checkpoint.json"),
            maximum_bytes: 1_000_000,
            session_id: session.session_id.clone(),
            wall_start: session.wall_start.clone(),
            contract_fingerprint: session.contract_fingerprint.clone(),
            runtime_config: session.runtime_config.clone(),
            publisher_epoch_lineage: vec!["different-epoch".to_owned()],
        };

        assert!(!live_contexts_match(&processor, &session, &checkpoint));
        checkpoint.publisher_epoch_lineage = vec![EPOCH.to_owned()];
        assert!(live_contexts_match(&processor, &session, &checkpoint));
    }

    /// A lane whose instances are never ready publishes only control frames.
    /// Those frames must reach the file as the admission returns, not when the
    /// 1 MiB buffer eventually fills.
    #[test]
    fn live_admission_flushes_control_frames_before_it_returns() {
        use crate::prediction_journal::DEFAULT_PREDICTION_JOURNAL_MAX_BYTES;

        let session = PredictionJournalSession::new(
            "2026-08-15T12:00:00Z".to_owned(),
            EPOCH.to_owned(),
            PredictionRuntimeConfig::new(PredictionMarketVenue::Kalshi, 2_000),
        )
        .expect("session builds");
        let processor = PredictionSignalProcessor::new(
            PredictionMarketVenue::Kalshi,
            EPOCH,
            "2026-08-15T12:00:00Z".parse().expect("wall start parses"),
        )
        .expect("processor builds");
        // A unique suffix keeps a failed run's leftover file from blocking the
        // journal writer, which never overwrites an existing path.
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock follows Unix epoch")
            .as_nanos();
        let journal_path = std::env::temp_dir().join(format!(
            "prediction-flush-{}-{unique}.jsonl",
            std::process::id()
        ));
        let journal = PredictionJournalWriter::create(
            &journal_path,
            session.clone(),
            DEFAULT_PREDICTION_JOURNAL_MAX_BYTES,
        )
        .expect("journal creates");
        let checkpoint = PredictionCheckpointContext {
            path: std::env::temp_dir().join(format!(
                "prediction-flush-{}-{unique}.checkpoint.json",
                std::process::id()
            )),
            maximum_bytes: 1_000_000,
            session_id: session.session_id.clone(),
            wall_start: session.wall_start.clone(),
            contract_fingerprint: session.contract_fingerprint.clone(),
            runtime_config: session.runtime_config.clone(),
            publisher_epoch_lineage: vec![EPOCH.to_owned()],
        };
        let mut pipeline = PredictionPipeline::live(
            BufWriter::with_capacity(1024 * 1024, Vec::new()),
            processor,
            &session,
            journal,
            checkpoint,
            1_000,
            1_000_000,
        )
        .expect("live pipeline builds");

        pipeline
            .admit_live(PredictionJournalInput::HealthTick {
                admission_seq: 0,
                now_ns: SECOND_NS,
            })
            .expect("health tick admits");

        assert!(
            !pipeline.writer.get_ref().is_empty(),
            "control frames must be flushed before the admission returns"
        );

        let _ = std::fs::remove_file(&journal_path);
    }

    #[test]
    fn rollover_boundaries_precede_batches_and_replay_deterministically() {
        let session = PredictionJournalSession::new(
            "2026-08-15T12:00:00Z".to_owned(),
            EPOCH.to_owned(),
            PredictionRuntimeConfig::new(PredictionMarketVenue::Kalshi, 2_000),
        )
        .expect("session builds");
        let inputs = vec![
            PredictionJournalInput::Bind {
                admission_seq: 0,
                generation: 1,
                previous_market_id: None,
                reason: MarketTransitionReason::InitialBind,
                event: kalshi_contract(0, 1, 0),
                logical_receipt_ns: 0,
                now_ns: 0,
            },
            PredictionJournalInput::MarketEvents {
                admission_seq: 1,
                generation: 1,
                events: Arc::new(vec![kalshi_quote(0, 1, 1)]),
                logical_receipt_ns: SECOND_NS,
                now_ns: SECOND_NS,
            },
            PredictionJournalInput::Unavailable {
                admission_seq: 2,
                generation: 2,
                previous_market_id: Some("KXBTC15M-1".to_owned()),
                reason: MarketTransitionReason::ScheduledRollover,
                now_ns: 900 * SECOND_NS,
            },
            PredictionJournalInput::Bind {
                admission_seq: 3,
                generation: 2,
                previous_market_id: Some("KXBTC15M-1".to_owned()),
                reason: MarketTransitionReason::ScheduledRollover,
                event: kalshi_contract(1, 2, 900),
                logical_receipt_ns: 900 * SECOND_NS,
                now_ns: 900 * SECOND_NS,
            },
        ];

        let first = run(&session, &inputs);
        let second = run(&session, &inputs);
        assert_eq!(first, second, "the same admissions must be byte-identical");

        let messages = first
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice::<SignalStreamMessage>(line).expect("line decodes"))
            .collect::<Vec<_>>();
        let transitions = messages
            .iter()
            .enumerate()
            .filter_map(|(index, message)| match message {
                SignalStreamMessage::MarketTransition {
                    market_generation,
                    phase,
                    ..
                } => Some((index, *market_generation, *phase)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(transitions.len(), 3);
        assert_eq!(transitions[0].1, 1);
        assert_eq!(transitions[0].2, MarketTransitionPhase::Bound);
        assert_eq!(transitions[1].1, 2);
        assert_eq!(transitions[1].2, MarketTransitionPhase::Unavailable);
        assert_eq!(transitions[2].1, 2);
        assert_eq!(transitions[2].2, MarketTransitionPhase::Bound);
        let unavailable_index = transitions[1].0;
        let rebound_index = transitions[2].0;
        // This session is Kalshi, so its expected set is the Kalshi one.
        let venue_signal_count = prediction_signal_count(PredictionMarketVenue::Kalshi);
        let invalidations = &messages[unavailable_index - venue_signal_count..unavailable_index];
        assert!(invalidations.iter().all(|message| matches!(
            message,
            SignalStreamMessage::InstanceStatus {
                state: InstanceStatusState::Invalidated,
                scope: SignalScope::Market {
                    market_generation: 1,
                    ..
                },
                ..
            }
        )));
        let invalidation_keys = invalidations
            .iter()
            .filter_map(instance_status_key)
            .collect::<Vec<_>>();
        assert!(invalidation_keys.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(unavailable_index < rebound_index);

        let rebound_statuses = messages[rebound_index + 1..]
            .iter()
            .take_while(|message| matches!(message, SignalStreamMessage::InstanceStatus { .. }))
            .collect::<Vec<_>>();
        // Every venue identity restates on the rebound except the one whose
        // first post-bind decision is ready and therefore lands in the batch.
        assert_eq!(rebound_statuses.len(), venue_signal_count - 1);
        assert!(rebound_statuses.iter().all(|message| matches!(
            message,
            SignalStreamMessage::InstanceStatus {
                scope: SignalScope::Market {
                    market_generation: 2,
                    ..
                },
                state: InstanceStatusState::WarmingUp | InstanceStatusState::Degraded,
                ..
            }
        )));
        assert!(matches!(
            messages.get(rebound_index + 1 + rebound_statuses.len()),
            Some(SignalStreamMessage::SignalBatch { updates, .. }) if updates.len() == 1
        ));

        // Mutation-style evidence: moving the transition ahead of even one
        // invalidation violates the lifecycle ordering predicate.
        let mut reordered = messages.clone();
        reordered.swap(unavailable_index - 1, unavailable_index);
        assert!(!rollover_order_is_valid(&reordered, venue_signal_count));
        assert!(rollover_order_is_valid(&messages, venue_signal_count));
    }

    #[test]
    fn an_unavailable_row_after_departure_is_rejected_while_unbound() {
        let session = PredictionJournalSession::new(
            "2026-08-15T12:00:00Z".to_owned(),
            EPOCH.to_owned(),
            PredictionRuntimeConfig::new(PredictionMarketVenue::Kalshi, 2_000),
        )
        .expect("session builds");
        let bind = PredictionJournalInput::Bind {
            admission_seq: 0,
            generation: 1,
            previous_market_id: None,
            reason: MarketTransitionReason::InitialBind,
            event: kalshi_contract(0, 1, 0),
            logical_receipt_ns: 0,
            now_ns: 0,
        };
        let departure = PredictionJournalInput::Unavailable {
            admission_seq: 1,
            generation: 2,
            previous_market_id: Some("KXBTC15M-1".to_owned()),
            reason: MarketTransitionReason::ScheduledRollover,
            now_ns: 900 * SECOND_NS,
        };
        let duplicate_departure = PredictionJournalInput::Unavailable {
            admission_seq: 2,
            generation: 2,
            previous_market_id: Some("KXBTC15M-1".to_owned()),
            reason: MarketTransitionReason::CloseToNextOpenGap,
            now_ns: 901 * SECOND_NS,
        };
        let processor = PredictionSignalProcessor::new(
            PredictionMarketVenue::Kalshi,
            EPOCH,
            "2026-08-15T12:00:00Z".parse().expect("wall start parses"),
        )
        .expect("processor builds");
        let mut pipeline =
            PredictionPipeline::replay(Vec::new(), processor, &session, 1_000, 1_000_000)
                .expect("pipeline builds");
        pipeline.admit_replay(bind).expect("initial bind admits");
        pipeline
            .admit_replay(departure)
            .expect("bound departure admits");
        assert!(matches!(
            pipeline.admit_replay(duplicate_departure),
            Err(PredictionPipelineError::Journal(PredictionJournalError::Invalid(message)))
                if message.contains("requires a bound market")
        ));
    }

    #[test]
    fn spot_and_health_admissions_are_quiet_while_market_is_unbound() {
        let session = PredictionJournalSession::new(
            "2026-08-15T12:00:00Z".to_owned(),
            EPOCH.to_owned(),
            PredictionRuntimeConfig::new(PredictionMarketVenue::Polymarket, 2_000),
        )
        .expect("session builds");
        let processor = PredictionSignalProcessor::new(
            PredictionMarketVenue::Polymarket,
            EPOCH,
            "2026-08-15T12:00:00Z".parse().expect("wall start parses"),
        )
        .expect("processor builds");
        let mut pipeline =
            PredictionPipeline::replay(Vec::new(), processor, &session, 1_000, 1_000_000)
                .expect("pipeline builds");
        pipeline
            .admit_replay(PredictionJournalInput::Bind {
                admission_seq: 0,
                generation: 1,
                previous_market_id: None,
                reason: MarketTransitionReason::InitialBind,
                event: polymarket_contract(0, 1, 0),
                logical_receipt_ns: 0,
                now_ns: 0,
            })
            .expect("initial bind admits");
        pipeline
            .admit_replay(PredictionJournalInput::Unavailable {
                admission_seq: 1,
                generation: 2,
                previous_market_id: Some("pm-1".to_owned()),
                reason: MarketTransitionReason::ProviderLifecycleChange,
                now_ns: SECOND_NS,
            })
            .expect("lifecycle departure admits");
        let batches_after_departure = pipeline.report().signal_batches_written;

        pipeline
            .admit_replay(PredictionJournalInput::MarketEvents {
                admission_seq: 2,
                generation: 2,
                events: Arc::new(vec![source_event(
                    POLYMARKET_TWAP_SOURCE_ID,
                    0,
                    2,
                    BTreeMap::from([
                        ("published_at".to_owned(), json!(timestamp(2))),
                        ("symbol".to_owned(), json!("btc/usd")),
                        ("twap_price".to_owned(), json!("60000")),
                        ("window_seconds".to_owned(), json!(60)),
                    ]),
                )]),
                logical_receipt_ns: 2 * SECOND_NS,
                now_ns: 2 * SECOND_NS,
            })
            .expect("auxiliary TWAP remains admissible during rediscovery");
        pipeline
            .admit_replay(PredictionJournalInput::SpotEvents {
                admission_seq: 3,
                events: Arc::new(vec![coinbase_ticker(1, "60000", 3)]),
                logical_receipt_ns: 3 * SECOND_NS,
                now_ns: 3 * SECOND_NS,
            })
            .expect("spot data remains admissible during rediscovery");
        pipeline
            .admit_replay(PredictionJournalInput::HealthTick {
                admission_seq: 4,
                now_ns: 4 * SECOND_NS,
            })
            .expect("health time remains admissible during rediscovery");

        assert_eq!(
            pipeline.report().signal_batches_written,
            batches_after_departure,
            "an unbound market emits no empty prediction batch"
        );
    }

    /// The batch guard is sized from the emitting venue's own expected set.
    /// Kalshi publishes eighteen identities disarmed and Polymarket twenty (the
    /// two settlement-anchor readings are Polymarket's alone; C2 adds the two
    /// probability momentum/volatility identities on both venues, and C3b
    /// (ruling E51) added five TA and timing identities to each venue's
    /// always-on set), so one shared count would accept a truncated Polymarket
    /// batch and refuse a complete one.
    #[test]
    fn signal_batch_size_follows_the_per_venue_expected_set() {
        let kalshi_count = prediction_signal_count(PredictionMarketVenue::Kalshi);
        let polymarket_count = prediction_signal_count(PredictionMarketVenue::Polymarket);
        assert_eq!(kalshi_count, 18);
        assert_eq!(polymarket_count, 20);

        for (venue, contract, expected, wrong) in [
            (
                PredictionMarketVenue::Kalshi,
                kalshi_contract(0, 1, 0),
                kalshi_count,
                polymarket_count,
            ),
            (
                PredictionMarketVenue::Polymarket,
                polymarket_contract(0, 1, 0),
                polymarket_count,
                kalshi_count,
            ),
        ] {
            let session = PredictionJournalSession::new(
                "2026-08-15T12:00:00Z".to_owned(),
                EPOCH.to_owned(),
                PredictionRuntimeConfig::new(venue, 2_000),
            )
            .expect("session builds");
            let processor = PredictionSignalProcessor::new(
                venue,
                EPOCH,
                "2026-08-15T12:00:00Z".parse().expect("wall start parses"),
            )
            .expect("processor builds");
            let mut pipeline =
                PredictionPipeline::replay(Vec::new(), processor, &session, 1_000, 1_000_000)
                    .expect("pipeline builds");
            pipeline
                .admit_replay(PredictionJournalInput::Bind {
                    admission_seq: 0,
                    generation: 1,
                    previous_market_id: None,
                    reason: MarketTransitionReason::InitialBind,
                    event: contract.clone(),
                    logical_receipt_ns: 0,
                    now_ns: 0,
                })
                .expect("initial bind admits");

            // A second processor driven through the identical admissions, so the
            // batch handed to the guard is the venue's real decision output
            // rather than a synthetic vector.
            let mut probe = PredictionSignalProcessor::new(
                venue,
                EPOCH,
                "2026-08-15T12:00:00Z".parse().expect("wall start parses"),
            )
            .expect("probe processor builds");
            probe
                .bind_market(1, &contract, 0, 0)
                .expect("probe binds the same market");
            let accepted = probe.health_tick(SECOND_NS).expect("probe emits a batch");
            assert_eq!(accepted.len(), expected, "{venue:?} batch size");
            pipeline
                .write_signal_batch(SECOND_NS, accepted.clone())
                .expect("the venue's own complete batch is accepted");

            // The other venue's size is refused, in both directions.
            let mis_sized = accepted.resized_for_completeness_test(wrong);
            let error = pipeline
                .write_signal_batch(2 * SECOND_NS, mis_sized)
                .expect_err("a batch sized for the other venue is refused");
            assert!(
                error.to_string().contains(&format!(
                    "prediction batch contains {wrong} updates; expected {expected}"
                )),
                "{venue:?}: {error}"
            );
        }
    }

    /// A byte sink shared with the pipeline so a test can read the exact output
    /// after the pipeline has taken ownership of the writer.
    #[derive(Clone)]
    struct SharedSink(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for SharedSink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("sink lock").extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A full CF BRTI value frame at `second`. The trailing window spans exactly
    /// sixty seconds, so a full window size is a complete settlement anchor.
    fn cf_value(
        sequence: u64,
        second: u64,
        index_value: &str,
        avg: &str,
        window: u32,
    ) -> SourceEvent {
        source_event(
            "kalshi.cfbenchmarks.brti.value",
            sequence,
            second,
            BTreeMap::from([
                ("index_id".to_owned(), json!("BRTI")),
                ("index_value".to_owned(), json!(index_value)),
                ("kalshi_received_at".to_owned(), json!(timestamp(second))),
                ("upstream_sequence".to_owned(), json!(sequence)),
                ("avg_60s_value".to_owned(), json!(avg)),
                ("avg_60s_window_size".to_owned(), json!(window)),
                (
                    "avg_60s_window_start".to_owned(),
                    json!(timestamp(second - 60)),
                ),
                (
                    "avg_60s_window_end_exclusive".to_owned(),
                    json!(timestamp(second)),
                ),
            ]),
        )
    }

    /// A settlement-window frame (E28/E37, F1): the derived quarter close on the
    /// 15-minute grid, the running `window_as_of` and the accumulated window
    /// size. It is the third declared input of the settlement twins, so a frame
    /// gates their readiness; completeness is `window_as_of == quarter_close`
    /// with a full sixty-print window.
    fn cf_settlement(
        sequence: u64,
        second: u64,
        close_second: u64,
        as_of_second: u64,
        window: u32,
    ) -> SourceEvent {
        source_event(
            "kalshi.cfbenchmarks.brti.settlement-window",
            sequence,
            second,
            BTreeMap::from([
                ("index_id".to_owned(), json!("BRTI")),
                ("kalshi_received_at".to_owned(), json!(timestamp(second))),
                ("upstream_sequence".to_owned(), json!(sequence)),
                ("index_value".to_owned(), json!("60150.00000000")),
                ("window_average".to_owned(), json!("60150.00000000")),
                ("window_size".to_owned(), json!(window)),
                (
                    "window_start_exclusive".to_owned(),
                    json!(timestamp(close_second - 60)),
                ),
                (
                    "window_end_inclusive".to_owned(),
                    json!(timestamp(close_second)),
                ),
                ("window_as_of".to_owned(), json!(timestamp(as_of_second))),
                (
                    "quarter_close_time".to_owned(),
                    json!(timestamp(close_second)),
                ),
            ]),
        )
    }

    /// Drive an armed Kalshi lane through a bind, a CF value frame and a health
    /// tick, returning the exact output bytes. The socket arming is the journal's
    /// own `index_armed`, so both a live run and its replay take this path.
    fn drive_armed_kalshi() -> Vec<u8> {
        let sink = SharedSink(Arc::new(std::sync::Mutex::new(Vec::new())));
        let session = PredictionJournalSession::new(
            "2026-08-15T12:00:00Z".to_owned(),
            EPOCH.to_owned(),
            PredictionRuntimeConfig::new(PredictionMarketVenue::Kalshi, 2_000)
                .with_index_armed(true),
        )
        .expect("armed session builds");
        let processor = PredictionSignalProcessor::new_indexed(
            PredictionMarketVenue::Kalshi,
            EPOCH,
            "2026-08-15T12:00:00Z".parse().expect("wall start parses"),
            true,
        )
        .expect("armed processor builds");
        let mut pipeline =
            PredictionPipeline::replay(sink.clone(), processor, &session, 10_000, 10_000_000)
                .expect("armed pipeline builds");
        pipeline
            .admit_replay(PredictionJournalInput::Bind {
                admission_seq: 0,
                generation: 1,
                previous_market_id: None,
                reason: MarketTransitionReason::InitialBind,
                event: kalshi_contract(0, 1, 0),
                logical_receipt_ns: 0,
                now_ns: 0,
            })
            .expect("bind admits");
        pipeline
            .admit_replay(PredictionJournalInput::MarketEvents {
                admission_seq: 1,
                generation: 1,
                events: Arc::new(vec![
                    cf_value(1, 120, "60200.5", "60150.25000000", 60),
                    cf_settlement(2, 120, 900, 120, 60),
                ]),
                logical_receipt_ns: 120 * SECOND_NS,
                now_ns: 120 * SECOND_NS,
            })
            .expect("CF value admits and the 13-instance batch is complete");
        pipeline
            .admit_replay(PredictionJournalInput::HealthTick {
                admission_seq: 2,
                now_ns: 121 * SECOND_NS,
            })
            .expect("health tick admits");
        drop(pipeline);
        sink.0.lock().expect("sink lock").clone()
    }

    /// A minimal event on an unconsumed/foreign source. The routing keys on the
    /// source id alone and never parses the payload, so the values are empty.
    fn bare_event(source_id: &str, sequence: u64, second: u64) -> SourceEvent {
        source_event(source_id, sequence, second, BTreeMap::new())
    }

    fn armed_kalshi_pipeline(sink: SharedSink) -> PredictionPipeline<SharedSink> {
        let session = PredictionJournalSession::new(
            "2026-08-15T12:00:00Z".to_owned(),
            EPOCH.to_owned(),
            PredictionRuntimeConfig::new(PredictionMarketVenue::Kalshi, 2_000)
                .with_index_armed(true),
        )
        .expect("armed session builds");
        let processor = PredictionSignalProcessor::new_indexed(
            PredictionMarketVenue::Kalshi,
            EPOCH,
            "2026-08-15T12:00:00Z".parse().expect("wall start parses"),
            true,
        )
        .expect("armed processor builds");
        let mut pipeline =
            PredictionPipeline::replay(sink, processor, &session, 10_000, 10_000_000)
                .expect("armed pipeline builds");
        pipeline
            .admit_replay(PredictionJournalInput::Bind {
                admission_seq: 0,
                generation: 1,
                previous_market_id: None,
                reason: MarketTransitionReason::InitialBind,
                event: kalshi_contract(0, 1, 0),
                logical_receipt_ns: 0,
                now_ns: 0,
            })
            .expect("bind admits");
        pipeline
    }

    #[test]
    fn armed_kalshi_lane_routes_unconsumed_index_sources_past_the_processor() {
        // E53 follow-up: the armed index socket delivers the 5 Hz value and the
        // Pyth relay, which no identity consumes. The lane must admit the batch
        // (never a fatal UnsupportedSource) and count them ignored, while the
        // consumed CF value still drives the oracle reference.
        let sink = SharedSink(Arc::new(std::sync::Mutex::new(Vec::new())));
        let mut pipeline = armed_kalshi_pipeline(sink.clone());
        pipeline
            .admit_replay(PredictionJournalInput::MarketEvents {
                admission_seq: 1,
                generation: 1,
                events: Arc::new(vec![
                    cf_value(1, 120, "60200.5", "60150.25000000", 60),
                    bare_event("kalshi.cfbenchmarks.brti.value-5hz", 2, 120),
                    bare_event("kalshi.pyth.btc-usd.value", 3, 120),
                ]),
                logical_receipt_ns: 120 * SECOND_NS,
                now_ns: 120 * SECOND_NS,
            })
            .expect("the batch admits without aborting on the unconsumed index sources");
        let ignored = pipeline.feed_health.snapshot().ignored_events;
        assert_eq!(
            ignored.get("kalshi.cfbenchmarks.brti.value-5hz").copied(),
            Some(1),
            "the 5 Hz value is routed past and counted"
        );
        assert_eq!(
            ignored.get("kalshi.pyth.btc-usd.value").copied(),
            Some(1),
            "the Pyth relay is routed past and counted"
        );
        let updates = batch_updates(&sink.0.lock().expect("sink lock").clone());
        assert!(
            updates
                .iter()
                .any(|update| update.signal_id == "btc.cfbrti.oracle_reference"),
            "the consumed CF value still drives the oracle reference (lane alive)"
        );
    }

    #[test]
    fn armed_kalshi_lane_routes_the_r4_index_composition_past_the_processor() {
        // Finding #4: the hermetic routing tests above never drove the r4 index
        // composition. S-RULINGS E53 records the excerpt as 6000 raw records ->
        // 6053 events = 4994 value-5hz + 999 value + 60 settlement-window; that
        // capture is a real feed excerpt, not a committed fixture (feed data stays
        // out of git), so this reconstructs the ignored 5 Hz stream at its r4 count.
        // It pins that the armed lane routes exactly ONE unconsumed source past the
        // processor at that count: a Kalshi index adapter that started emitting a
        // fourth unconsumed source would fail this test, not only the
        // produced_source_ids replay test in btc-feeds.
        const VALUE_5HZ_COUNT: u64 = 4994;
        let sink = SharedSink(Arc::new(std::sync::Mutex::new(Vec::new())));
        let mut pipeline = armed_kalshi_pipeline(sink.clone());
        let mut events = Vec::with_capacity(VALUE_5HZ_COUNT as usize + 1);
        let mut sequence = 0_u64;
        // A consumed 1 Hz CF value keeps the oracle reference alive and stands for
        // the 999 consumed value frames (feed_health exposes an ignored-events
        // count per source but no per-source consumed count, so the consumed side
        // is pinned by the published oracle reference, not a frame tally).
        sequence += 1;
        events.push(cf_value(sequence, 120, "60200.5", "60150.25000000", 60));
        for _ in 0..VALUE_5HZ_COUNT {
            sequence += 1;
            events.push(bare_event(
                "kalshi.cfbenchmarks.brti.value-5hz",
                sequence,
                120,
            ));
        }
        pipeline
            .admit_replay(PredictionJournalInput::MarketEvents {
                admission_seq: 1,
                generation: 1,
                events: Arc::new(events),
                logical_receipt_ns: 120 * SECOND_NS,
                now_ns: 120 * SECOND_NS,
            })
            .expect("the r4-composition batch admits without aborting");
        let ignored = pipeline.feed_health.snapshot().ignored_events;
        assert_eq!(
            ignored.len(),
            1,
            "exactly one unconsumed source is routed past; a fourth would appear here: {ignored:?}"
        );
        assert_eq!(
            ignored.get("kalshi.cfbenchmarks.brti.value-5hz").copied(),
            Some(VALUE_5HZ_COUNT),
            "every 5 Hz value frame is routed past and counted"
        );
        let updates = batch_updates(&sink.0.lock().expect("sink lock").clone());
        assert!(
            updates
                .iter()
                .any(|update| update.signal_id == "btc.cfbrti.oracle_reference"),
            "the consumed CF value still drives the oracle reference (lane alive)"
        );
    }

    #[test]
    fn armed_kalshi_lane_still_refuses_a_foreign_source() {
        // A source no declared stream produces is neither consumed nor
        // observed-not-consumed: it must still refuse, so the routing does not
        // become a silent accept-anything.
        let sink = SharedSink(Arc::new(std::sync::Mutex::new(Vec::new())));
        let mut pipeline = armed_kalshi_pipeline(sink);
        let result = pipeline.admit_replay(PredictionJournalInput::MarketEvents {
            admission_seq: 1,
            generation: 1,
            events: Arc::new(vec![bare_event("binance.spot.btcusdt.trades", 1, 120)]),
            logical_receipt_ns: 120 * SECOND_NS,
            now_ns: 120 * SECOND_NS,
        });
        let error = result.expect_err("a foreign source refuses");
        assert!(
            matches!(
                &error,
                PredictionPipelineError::Processor(PredictionSignalError::UnsupportedSource(id))
                    if id == "binance.spot.btcusdt.trades"
            ),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn prediction_source_routing_refuses_a_consumed_source_without_a_producer() {
        // An armed Kalshi processor consumes the CF value and the settlement
        // window. If the index stream is not declared (index_armed = false), those
        // consumed sources have no producer: refuse at start-up rather than warm
        // forever.
        let processor = PredictionSignalProcessor::new_indexed(
            PredictionMarketVenue::Kalshi,
            EPOCH,
            "2026-08-15T12:00:00Z".parse().expect("wall start parses"),
            true,
        )
        .expect("armed processor builds");
        let error = prediction_source_routing(PredictionMarketVenue::Kalshi, false, &processor)
            .expect_err("a consumed source without a producer refuses");
        assert!(
            error.to_string().contains("no declared stream produces it"),
            "unexpected error: {error}"
        );
        // With the index stream declared, every consumed source is produced, and
        // the 5 Hz value / history / Pyth relay are observed-not-consumed.
        let observed = prediction_source_routing(PredictionMarketVenue::Kalshi, true, &processor)
            .expect("armed routing resolves");
        assert!(observed.contains("kalshi.cfbenchmarks.brti.value-5hz"));
        assert!(observed.contains("kalshi.cfbenchmarks.brti.history"));
        assert!(observed.contains("kalshi.pyth.btc-usd.value"));
        assert!(!observed.contains("kalshi.cfbenchmarks.brti.value"));
        assert!(!observed.contains("kalshi.cfbenchmarks.brti.settlement-window"));
        // The Polymarket lane consumes exactly what its two streams produce, so
        // nothing is observed-not-consumed (admission byte-unchanged).
        let polymarket = PredictionSignalProcessor::new(
            PredictionMarketVenue::Polymarket,
            EPOCH,
            "2026-08-15T12:00:00Z".parse().expect("wall start parses"),
        )
        .expect("polymarket processor builds");
        assert!(
            prediction_source_routing(PredictionMarketVenue::Polymarket, false, &polymarket)
                .expect("polymarket routing resolves")
                .is_empty()
        );
    }

    #[test]
    fn kalshi_lane_with_cf_socket_publishes_the_index_identities_and_replays_byte_identically() {
        let bytes = drive_armed_kalshi();
        let updates = batch_updates(&bytes);
        // The global CF BRTI oracle reference is published at global scope.
        let reference = updates
            .iter()
            .find(|update| update.signal_id == "btc.cfbrti.oracle_reference")
            .expect("the oracle reference is published");
        assert!(matches!(reference.scope, SignalScope::Global));
        assert_eq!(
            reference.values.get("value").and_then(Value::as_str),
            Some("60200.5")
        );
        // Both Kalshi settlement identities are published, market-scoped. The
        // anchor is ready on the full sixty-second average; divergence is
        // instance_status (no spot reference fed here), so it rides either frame.
        let anchor_scope = updates
            .iter()
            .find(|update| update.signal_id == "btc.kalshi.settlement_anchor_state")
            .map(|update| &update.scope);
        assert!(matches!(anchor_scope, Some(SignalScope::Market { .. })));
        let statuses: Vec<String> = decode(&bytes)
            .into_iter()
            .filter_map(|message| match message {
                SignalStreamMessage::InstanceStatus { signal_id, .. } => Some(signal_id),
                _ => None,
            })
            .collect();
        assert!(
            statuses
                .iter()
                .any(|id| id == "btc.kalshi.oracle_divergence")
                || updates
                    .iter()
                    .any(|update| update.signal_id == "btc.kalshi.oracle_divergence"),
            "the Kalshi oracle divergence is published (ready or as instance_status)"
        );
        // The settlement anchor is ready on the full-window sixty-second average.
        let anchor = updates
            .iter()
            .find(|update| {
                update.signal_id == "btc.kalshi.settlement_anchor_state"
                    && update.quality.state == signals_core::QualityState::Ready
            })
            .expect("a ready settlement anchor");
        assert_eq!(
            anchor.values.get("anchor_price").and_then(Value::as_str),
            Some("60150.25000000")
        );
        // Replay is deterministic: the same journal re-run yields identical bytes.
        assert_eq!(
            bytes,
            drive_armed_kalshi(),
            "the armed lane replays byte-identically"
        );
    }

    /// F2 / item (b): a subscription refusal on the armed 1 Hz value channel is
    /// carried end to end through an armed pipeline — the two settlement twins
    /// and the global oracle reference publish `cfbrti_subscription_rejected` as
    /// instance_status once refused, and the acceptance that follows returns them
    /// to ready. The disarmed 5 Hz refusal path is covered separately; this is
    /// the armed 1 Hz channel the identities actually consume.
    #[test]
    fn armed_1hz_refusal_is_carried_end_to_end_and_clears_on_acceptance() {
        let sink = SharedSink(Arc::new(std::sync::Mutex::new(Vec::new())));
        let session = PredictionJournalSession::new(
            "2026-08-15T12:00:00Z".to_owned(),
            EPOCH.to_owned(),
            PredictionRuntimeConfig::new(PredictionMarketVenue::Kalshi, 2_000)
                .with_index_armed(true),
        )
        .expect("armed session builds");
        let processor = PredictionSignalProcessor::new_indexed(
            PredictionMarketVenue::Kalshi,
            EPOCH,
            "2026-08-15T12:00:00Z".parse().expect("wall start parses"),
            true,
        )
        .expect("armed processor builds");
        let mut pipeline =
            PredictionPipeline::replay(sink.clone(), processor, &session, 10_000, 10_000_000)
                .expect("armed pipeline builds");
        pipeline
            .admit_replay(PredictionJournalInput::Bind {
                admission_seq: 0,
                generation: 1,
                previous_market_id: None,
                reason: MarketTransitionReason::InitialBind,
                event: kalshi_contract(0, 1, 0),
                logical_receipt_ns: 0,
                now_ns: 0,
            })
            .expect("bind admits");
        pipeline
            .admit_replay(PredictionJournalInput::MarketEvents {
                admission_seq: 1,
                generation: 1,
                events: Arc::new(vec![
                    cf_value(1, 120, "60200.5", "60150.25000000", 60),
                    cf_settlement(2, 120, 900, 120, 60),
                ]),
                logical_receipt_ns: 120 * SECOND_NS,
                now_ns: 120 * SECOND_NS,
            })
            .expect("CF value admits");

        // The venue refuses the 1 Hz channel; the next decision withholds the
        // three index identities under the refusal reason.
        pipeline
            .refuse_index_channel(KALSHI_BRTI_SOURCE_ID, "cfbrti_subscription_rejected")
            .expect("a registered unavailable reason is accepted");
        pipeline
            .admit_replay(PredictionJournalInput::HealthTick {
                admission_seq: 2,
                now_ns: 121 * SECOND_NS,
            })
            .expect("health tick under refusal");

        let refused_ids: Vec<String> = decode(&sink.0.lock().expect("sink").clone())
            .into_iter()
            .filter_map(|message| match message {
                SignalStreamMessage::InstanceStatus {
                    signal_id, reasons, ..
                } if reasons.iter().any(|r| r == "cfbrti_subscription_rejected") => Some(signal_id),
                _ => None,
            })
            .collect();
        for id in [
            "btc.cfbrti.oracle_reference",
            "btc.kalshi.settlement_anchor_state",
            "btc.kalshi.oracle_divergence",
        ] {
            assert!(
                refused_ids.iter().any(|got| got == id),
                "{id} publishes cfbrti_subscription_rejected end to end"
            );
        }

        // Acceptance plus a fresh frame returns the settlement anchor to ready.
        assert!(pipeline.accept_index_channel(KALSHI_BRTI_SOURCE_ID));
        pipeline
            .admit_replay(PredictionJournalInput::MarketEvents {
                admission_seq: 3,
                generation: 1,
                events: Arc::new(vec![cf_value(2, 122, "60201.0", "60150.30000000", 60)]),
                logical_receipt_ns: 122 * SECOND_NS,
                now_ns: 122 * SECOND_NS,
            })
            .expect("post-accept frame admits");
        drop(pipeline);
        let anchor_ready = batch_updates(&sink.0.lock().expect("sink").clone())
            .into_iter()
            .filter(|update| update.signal_id == "btc.kalshi.settlement_anchor_state")
            .any(|update| update.quality.state == signals_core::QualityState::Ready);
        assert!(
            anchor_ready,
            "the settlement anchor is ready again after acceptance"
        );
    }

    /// Every `signal_batch` update in an output, in stream order.
    fn batch_updates(bytes: &[u8]) -> Vec<SignalUpdate> {
        decode(bytes)
            .into_iter()
            .filter_map(|message| match message {
                SignalStreamMessage::SignalBatch { updates, .. } => Some(updates),
                _ => None,
            })
            .flatten()
            .collect()
    }

    fn decode(bytes: &[u8]) -> Vec<SignalStreamMessage> {
        bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice(line).expect("line decodes"))
            .collect()
    }

    /// The market-contract card is static for the life of a market, so it is
    /// published once at the binding and then only when its declared interval
    /// falls due — not once per admitted market event.
    #[test]
    fn an_unchanged_instance_refreshes_on_its_interval_not_on_every_event() {
        let session = PredictionJournalSession::new(
            "2026-08-15T12:00:00Z".to_owned(),
            EPOCH.to_owned(),
            PredictionRuntimeConfig::new(PredictionMarketVenue::Kalshi, 2_000),
        )
        .expect("session builds");
        let mut inputs = vec![PredictionJournalInput::Bind {
            admission_seq: 0,
            generation: 1,
            previous_market_id: None,
            reason: MarketTransitionReason::InitialBind,
            event: kalshi_contract(0, 1, 0),
            logical_receipt_ns: 0,
            now_ns: 0,
        }];
        // Ten admissions a tenth of a second apart, each restating the same
        // quote: the tenth lands exactly on the one-second interval boundary.
        for step in 1..=10_u64 {
            let now_ns = step * SECOND_NS / 10;
            inputs.push(PredictionJournalInput::MarketEvents {
                admission_seq: step,
                generation: 1,
                events: Arc::new(vec![kalshi_quote(step - 1, 1, 0)]),
                logical_receipt_ns: now_ns,
                now_ns,
            });
        }

        let output = run(&session, &inputs);
        let contract_updates = batch_updates(&output)
            .into_iter()
            .filter(|update| update.signal_id == crate::KALSHI_MARKET_CONTRACT_ID)
            .collect::<Vec<_>>();
        assert_eq!(
            contract_updates.len(),
            2,
            "the static card is published at the binding and refreshed once at the interval"
        );
        assert_eq!(contract_updates[0].sequence, 0);
        assert_eq!(
            contract_updates[1].sequence, 1,
            "a suppressed evaluation consumes no decision sequence"
        );
        assert_eq!(
            contract_updates[0].values, contract_updates[1].values,
            "the refresh restates the same values under a new validity window"
        );

        // Truncating the run one admission short of the boundary proves the
        // refresh is the interval and not the event count.
        let before_boundary = run(&session, &inputs[..inputs.len() - 1]);
        assert_eq!(
            batch_updates(&before_boundary)
                .into_iter()
                .filter(|update| update.signal_id == crate::KALSHI_MARKET_CONTRACT_ID)
                .count(),
            1,
            "an unchanged value is not re-sent before its interval falls due"
        );
    }

    /// A pass whose every ready instance is suppressed writes no frame at all:
    /// an empty `signal_batch` is not a frame this producer emits.
    #[test]
    fn a_fully_suppressed_pass_writes_no_signal_batch() {
        let session = PredictionJournalSession::new(
            "2026-08-15T12:00:00Z".to_owned(),
            EPOCH.to_owned(),
            PredictionRuntimeConfig::new(PredictionMarketVenue::Kalshi, 2_000),
        )
        .expect("session builds");
        let bind = PredictionJournalInput::Bind {
            admission_seq: 0,
            generation: 1,
            previous_market_id: None,
            reason: MarketTransitionReason::InitialBind,
            event: kalshi_contract(0, 1, 0),
            logical_receipt_ns: 0,
            now_ns: 0,
        };
        // The first quote publishes what it changes; the second restates it
        // exactly, so the pass has nothing left to publish.
        let first = PredictionJournalInput::MarketEvents {
            admission_seq: 1,
            generation: 1,
            events: Arc::new(vec![kalshi_quote(0, 1, 0)]),
            logical_receipt_ns: SECOND_NS / 1_000,
            now_ns: SECOND_NS / 1_000,
        };
        let repeat = PredictionJournalInput::MarketEvents {
            admission_seq: 2,
            generation: 1,
            events: Arc::new(vec![kalshi_quote(1, 1, 0)]),
            logical_receipt_ns: SECOND_NS / 500,
            now_ns: SECOND_NS / 500,
        };

        let batches = |bytes: &[u8]| {
            decode(bytes)
                .into_iter()
                .filter(|message| matches!(message, SignalStreamMessage::SignalBatch { .. }))
                .count()
        };
        let quoted = run(&session, &[bind.clone(), first.clone()]);
        let repeated = run(&session, &[bind, first, repeat]);
        assert_eq!(
            batches(&quoted),
            2,
            "the binding and the first quote each publish a batch"
        );
        assert_eq!(
            batches(&repeated),
            2,
            "a pass with nothing to publish writes no batch of its own"
        );
        assert_eq!(
            batch_updates(&quoted),
            batch_updates(&repeated),
            "a suppressed pass leaves the published stream untouched"
        );
        // The invariant the rule leads with: a suppressed pass does not advance
        // the publisher-wide stream sequence either, so the two runs' envelopes
        // are the same frames with the same numbers.
        let sequences = |bytes: &[u8]| {
            decode(bytes)
                .into_iter()
                .map(|message| message.sequence())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            sequences(&quoted),
            sequences(&repeated),
            "a suppressed pass does not advance the publisher-wide sequence"
        );
        assert_eq!(
            quoted, repeated,
            "the extra admission adds no bytes to the stream at all"
        );
    }

    /// `snapshot.updates` restates the published state exactly: a suppressed
    /// evaluation never enters it, so a joining consumer is handed what the
    /// stream already delivered.
    #[test]
    fn a_snapshot_restates_exactly_the_published_decisions() {
        let session = PredictionJournalSession::new(
            "2026-08-15T12:00:00Z".to_owned(),
            EPOCH.to_owned(),
            PredictionRuntimeConfig::new(PredictionMarketVenue::Kalshi, 2_000),
        )
        .expect("session builds");
        let mut inputs = vec![PredictionJournalInput::Bind {
            admission_seq: 0,
            generation: 1,
            previous_market_id: None,
            reason: MarketTransitionReason::InitialBind,
            event: kalshi_contract(0, 1, 0),
            logical_receipt_ns: 0,
            now_ns: 0,
        }];
        // The snapshot cadence is a minute, so the run has to cross it.
        for second in 1..=61_u64 {
            let now_ns = second * SECOND_NS;
            inputs.push(PredictionJournalInput::MarketEvents {
                admission_seq: second,
                generation: 1,
                events: Arc::new(vec![kalshi_quote(second - 1, 1, 0)]),
                logical_receipt_ns: now_ns,
                now_ns,
            });
        }

        let output = run(&session, &inputs);
        let mut published = BTreeMap::<SignalInstanceKey, SignalUpdate>::new();
        let mut snapshots = 0_usize;
        for message in decode(&output) {
            match message {
                SignalStreamMessage::SignalBatch { updates, .. } => {
                    for update in updates {
                        published.insert(update.instance_key(), update);
                    }
                }
                SignalStreamMessage::Snapshot { updates, .. } => {
                    snapshots += 1;
                    assert_eq!(
                        updates,
                        published.values().cloned().collect::<Vec<_>>(),
                        "a snapshot restates the published state, not the evaluated one"
                    );
                }
                _ => {}
            }
        }
        assert_eq!(snapshots, 1, "the run crosses exactly one snapshot cadence");
        assert!(
            published.contains_key(&SignalInstanceKey {
                signal_id: crate::KALSHI_MARKET_CONTRACT_ID.to_owned(),
                signal_version: crate::PREDICTION_SIGNAL_VERSION.to_owned(),
                scope: SignalScope::Market {
                    market_id: "KXBTC15M-1".to_owned(),
                    market_generation: 1,
                },
            }),
            "the refreshed static card is still valid at the snapshot"
        );
    }

    fn instance_status_key(message: &SignalStreamMessage) -> Option<SignalInstanceKey> {
        match message {
            SignalStreamMessage::InstanceStatus {
                signal_id,
                signal_version,
                scope,
                ..
            } => Some(SignalInstanceKey {
                signal_id: signal_id.clone(),
                signal_version: signal_version.clone(),
                scope: scope.clone(),
            }),
            _ => None,
        }
    }

    fn rollover_order_is_valid(
        messages: &[SignalStreamMessage],
        expected_instances: usize,
    ) -> bool {
        let Some(transition_index) = messages.iter().position(|message| {
            matches!(
                message,
                SignalStreamMessage::MarketTransition {
                    phase: MarketTransitionPhase::Unavailable,
                    ..
                }
            )
        }) else {
            return false;
        };
        let Some(start) = transition_index.checked_sub(expected_instances) else {
            return false;
        };
        let keys = messages[start..transition_index]
            .iter()
            .filter_map(|message| match message {
                SignalStreamMessage::InstanceStatus {
                    state: InstanceStatusState::Invalidated,
                    ..
                } => instance_status_key(message),
                _ => None,
            })
            .collect::<Vec<_>>();
        keys.len() == expected_instances && keys.windows(2).all(|pair| pair[0] < pair[1])
    }

    fn count_feed_health(bytes: &[u8]) -> usize {
        bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .filter(|line| {
                serde_json::from_slice::<Value>(line)
                    .ok()
                    .and_then(|value| {
                        value
                            .get("type")
                            .and_then(|kind| kind.as_str())
                            .map(|kind| kind == "feed_health")
                    })
                    .unwrap_or(false)
            })
            .count()
    }

    #[test]
    fn scheduled_rollover_is_not_feed_sickness_but_discovery_loss_is() {
        // The mapping is the decision point: a scheduled handoff yields no
        // availability signal, while a discovery loss is a genuine outage.
        assert_eq!(
            feed_availability_for_reason(MarketTransitionReason::ScheduledRollover),
            None
        );
        assert_eq!(
            feed_availability_for_reason(MarketTransitionReason::InitialBind),
            None
        );
        assert!(matches!(
            feed_availability_for_reason(MarketTransitionReason::DiscoveryUnreachable),
            Some((FeedAvailability::Down, _))
        ));
        assert!(matches!(
            feed_availability_for_reason(MarketTransitionReason::NextMarketUnlisted),
            Some((FeedAvailability::Reconnecting, _))
        ));

        let session = PredictionJournalSession::new(
            "2026-08-15T12:00:00Z".to_owned(),
            EPOCH.to_owned(),
            PredictionRuntimeConfig::new(PredictionMarketVenue::Kalshi, 2_000),
        )
        .expect("session builds");
        let processor = PredictionSignalProcessor::new(
            PredictionMarketVenue::Kalshi,
            EPOCH,
            "2026-08-15T12:00:00Z".parse().expect("wall start parses"),
        )
        .expect("processor builds");
        let mut pipeline =
            PredictionPipeline::replay(Vec::new(), processor, &session, 1_000, 4_194_304)
                .expect("pipeline builds");

        pipeline
            .admit_replay(PredictionJournalInput::Bind {
                admission_seq: 0,
                generation: 1,
                previous_market_id: None,
                reason: MarketTransitionReason::InitialBind,
                event: kalshi_contract(0, 1, 0),
                logical_receipt_ns: 0,
                now_ns: 0,
            })
            .expect("initial bind admits");
        pipeline
            .admit_replay(PredictionJournalInput::MarketEvents {
                admission_seq: 1,
                generation: 1,
                events: Arc::new(vec![kalshi_quote(0, 1, 1)]),
                logical_receipt_ns: SECOND_NS,
                now_ns: SECOND_NS,
            })
            .expect("quote admits");
        assert_eq!(
            pipeline.feed_health.status("kalshi.btc.quotes"),
            Some(FeedHealthStatus::Live)
        );

        // A scheduled rollover begins: the market is briefly unavailable while it
        // hands off. The transport is healthy, so the feed must NOT be marked sick.
        pipeline
            .admit_replay(PredictionJournalInput::Unavailable {
                admission_seq: 2,
                generation: 2,
                previous_market_id: Some("KXBTC15M-1".to_owned()),
                reason: MarketTransitionReason::ScheduledRollover,
                now_ns: 900 * SECOND_NS,
            })
            .expect("scheduled rollover unavailable admits");
        assert_eq!(
            pipeline.feed_health.status("kalshi.btc.quotes"),
            Some(FeedHealthStatus::Live),
            "the rollover-unavailable edge is not feed sickness"
        );

        // The rollover COMPLETES by binding the successor market. The full
        // healthy handoff — unavailable then bound — must leave the feed Live.
        pipeline
            .admit_replay(PredictionJournalInput::Bind {
                admission_seq: 3,
                generation: 2,
                previous_market_id: Some("KXBTC15M-1".to_owned()),
                reason: MarketTransitionReason::ScheduledRollover,
                event: kalshi_contract(1, 2, 900),
                logical_receipt_ns: 900 * SECOND_NS,
                now_ns: 900 * SECOND_NS,
            })
            .expect("scheduled rollover completing bind admits");
        assert_eq!(
            pipeline.feed_health.status("kalshi.btc.market-contracts"),
            Some(FeedHealthStatus::Live),
            "the completing bind delivers market data: the feed is Live"
        );
        assert_eq!(
            pipeline.feed_health.status("kalshi.btc.quotes"),
            Some(FeedHealthStatus::Live),
            "a completed scheduled handoff leaves the feed healthy"
        );

        // A discovery-unreachable boundary IS a feed outage.
        pipeline
            .admit_replay(PredictionJournalInput::Unavailable {
                admission_seq: 4,
                generation: 3,
                previous_market_id: Some("KXBTC15M-2".to_owned()),
                reason: MarketTransitionReason::DiscoveryUnreachable,
                now_ns: 1_800 * SECOND_NS,
            })
            .expect("discovery-unreachable admits");
        assert_eq!(
            pipeline.feed_health.status("kalshi.btc.quotes"),
            Some(FeedHealthStatus::Down),
            "a discovery loss is a genuine feed outage"
        );
    }

    /// Build a bound-and-live Kalshi pipeline for socket-connectivity tests.
    fn bound_live_pipeline() -> PredictionPipeline<Vec<u8>> {
        let session = PredictionJournalSession::new(
            "2026-08-15T12:00:00Z".to_owned(),
            EPOCH.to_owned(),
            PredictionRuntimeConfig::new(PredictionMarketVenue::Kalshi, 2_000),
        )
        .expect("session builds");
        let processor = PredictionSignalProcessor::new(
            PredictionMarketVenue::Kalshi,
            EPOCH,
            "2026-08-15T12:00:00Z".parse().expect("wall start parses"),
        )
        .expect("processor builds");
        let mut pipeline =
            PredictionPipeline::replay(Vec::new(), processor, &session, 1_000, 4_194_304)
                .expect("pipeline builds");
        pipeline
            .admit_replay(PredictionJournalInput::Bind {
                admission_seq: 0,
                generation: 1,
                previous_market_id: None,
                reason: MarketTransitionReason::InitialBind,
                event: kalshi_contract(0, 1, 0),
                logical_receipt_ns: 0,
                now_ns: 0,
            })
            .expect("initial bind admits");
        pipeline
            .admit_replay(PredictionJournalInput::MarketEvents {
                admission_seq: 1,
                generation: 1,
                events: Arc::new(vec![kalshi_quote(0, 1, 1)]),
                logical_receipt_ns: SECOND_NS,
                now_ns: SECOND_NS,
            })
            .expect("quote admits");
        pipeline
    }

    #[test]
    fn socket_disconnect_drives_health_down_and_reconnect_restores() {
        let mut pipeline = bound_live_pipeline();
        assert_eq!(
            pipeline.feed_health.status("kalshi.btc.quotes"),
            Some(FeedHealthStatus::Live)
        );

        pipeline
            .admit_replay(PredictionJournalInput::SpotEvents {
                admission_seq: 2,
                events: Arc::new(vec![coinbase_ticker(1, "60000", 2)]),
                logical_receipt_ns: 2 * SECOND_NS,
                now_ns: 2 * SECOND_NS,
            })
            .expect("independent spot feed admits");
        assert_eq!(
            pipeline.feed_health.status(COINBASE_TICKER_SOURCE_ID),
            Some(FeedHealthStatus::Live)
        );

        // A genuine market-socket disconnect marks only the market transport
        // down; the independently connected spot feed remains live.
        pipeline
            .admit_replay(PredictionJournalInput::Connectivity {
                admission_seq: 3,
                transport: PredictionTransport::Kalshi,
                transition: PredictionSocketTransition::Disconnected,
                now_ns: 3 * SECOND_NS,
            })
            .expect("disconnect admits");
        assert_eq!(
            pipeline.feed_health.status("kalshi.btc.quotes"),
            Some(FeedHealthStatus::Down)
        );
        assert_eq!(
            pipeline.feed_health.status(COINBASE_TICKER_SOURCE_ID),
            Some(FeedHealthStatus::Live)
        );
        assert_eq!(pipeline.report().socket_transitions, 1);

        pipeline
            .admit_replay(PredictionJournalInput::Connectivity {
                admission_seq: 4,
                transport: PredictionTransport::Kalshi,
                transition: PredictionSocketTransition::Reconnecting,
                now_ns: 4 * SECOND_NS,
            })
            .expect("reconnect attempt admits");
        assert_eq!(
            pipeline.feed_health.status("kalshi.btc.quotes"),
            Some(FeedHealthStatus::Reconnecting)
        );

        pipeline
            .admit_replay(PredictionJournalInput::Connectivity {
                admission_seq: 5,
                transport: PredictionTransport::Kalshi,
                transition: PredictionSocketTransition::Connected,
                now_ns: 5 * SECOND_NS,
            })
            .expect("reconnect admits");
        assert_eq!(
            pipeline.feed_health.status("kalshi.btc.quotes"),
            Some(FeedHealthStatus::Live),
            "a successful connect returns the feed to healthy"
        );
        let entry = pipeline
            .report()
            .feed_health
            .feeds
            .into_iter()
            .find(|feed| feed.source_id == "kalshi.btc.quotes")
            .expect("quotes feed present");
        assert_eq!(entry.disconnect_count, 1);
        assert_eq!(entry.reconnect_count, 1);
        assert_eq!(entry.down_since_ns, None);
        let spot = pipeline
            .report()
            .feed_health
            .feeds
            .into_iter()
            .find(|feed| feed.source_id == COINBASE_TICKER_SOURCE_ID)
            .expect("spot feed present");
        assert_eq!(spot.disconnect_count, 0);
        assert_eq!(spot.reconnect_count, 0);
    }

    #[test]
    fn auxiliary_socket_lifecycle_is_journaled_to_its_own_transport() {
        let mut pipeline = bound_live_pipeline();
        let transports = [
            (PredictionTransport::Coinbase, COINBASE_TICKER_SOURCE_ID),
            (PredictionTransport::Kraken, KRAKEN_TICKER_SOURCE_ID),
        ];
        let mut admission_seq = 2;
        for (transport, source_id) in transports {
            pipeline
                .admit_replay(PredictionJournalInput::Connectivity {
                    admission_seq,
                    transport,
                    transition: PredictionSocketTransition::Disconnected,
                    now_ns: admission_seq * SECOND_NS,
                })
                .expect("auxiliary disconnect admits");
            admission_seq += 1;
            pipeline
                .admit_replay(PredictionJournalInput::Connectivity {
                    admission_seq,
                    transport,
                    transition: PredictionSocketTransition::Connected,
                    now_ns: admission_seq * SECOND_NS,
                })
                .expect("auxiliary reconnect admits");
            admission_seq += 1;

            let entry = pipeline
                .report()
                .feed_health
                .feeds
                .into_iter()
                .find(|feed| feed.source_id == source_id)
                .expect("owned auxiliary feed is present");
            assert_eq!(entry.status, FeedHealthStatus::Live);
            assert_eq!(entry.disconnect_count, 1);
            assert_eq!(entry.reconnect_count, 1);
        }

        let market = pipeline
            .report()
            .feed_health
            .feeds
            .into_iter()
            .find(|feed| feed.source_id == "kalshi.btc.quotes")
            .expect("market feed is present");
        assert_eq!(market.status, FeedHealthStatus::Live);
        assert_eq!(market.disconnect_count, 0);
        assert_eq!(market.reconnect_count, 0);

        let session = PredictionJournalSession::new(
            "2026-08-15T12:00:00Z".to_owned(),
            EPOCH.to_owned(),
            PredictionRuntimeConfig::new(PredictionMarketVenue::Polymarket, 2_000),
        )
        .expect("Polymarket session builds");
        let processor = PredictionSignalProcessor::new(
            PredictionMarketVenue::Polymarket,
            EPOCH,
            "2026-08-15T12:00:00Z".parse().expect("wall start parses"),
        )
        .expect("Polymarket processor builds");
        let mut polymarket =
            PredictionPipeline::replay(Vec::new(), processor, &session, 1_000, 4_194_304)
                .expect("Polymarket pipeline builds");
        for (admission_seq, transition) in [
            PredictionSocketTransition::Disconnected,
            PredictionSocketTransition::Connected,
        ]
        .into_iter()
        .enumerate()
        {
            polymarket
                .admit_replay(PredictionJournalInput::Connectivity {
                    admission_seq: admission_seq as u64,
                    transport: PredictionTransport::PolymarketRtds,
                    transition,
                    now_ns: admission_seq as u64 * SECOND_NS,
                })
                .expect("RTDS lifecycle admits");
        }
        let rtds = polymarket
            .report()
            .feed_health
            .feeds
            .into_iter()
            .find(|feed| feed.source_id == POLYMARKET_TWAP_SOURCE_ID)
            .expect("RTDS feed is present");
        assert_eq!(rtds.status, FeedHealthStatus::Live);
        assert_eq!(rtds.disconnect_count, 1);
        assert_eq!(rtds.reconnect_count, 1);
    }

    #[test]
    fn scheduled_rollover_produces_no_socket_disconnect_sickness() {
        // The rollover-is-never-sick invariant, asserted at the socket layer: a
        // scheduled handoff admits its lifecycle inputs but NO connectivity input,
        // so no disconnect is counted and the feed never leaves live. A genuine
        // socket disconnect, by contrast, does mark it down.
        let mut pipeline = bound_live_pipeline();
        pipeline
            .admit_replay(PredictionJournalInput::Unavailable {
                admission_seq: 2,
                generation: 2,
                previous_market_id: Some("KXBTC15M-1".to_owned()),
                reason: MarketTransitionReason::ScheduledRollover,
                now_ns: 900 * SECOND_NS,
            })
            .expect("scheduled rollover unavailable admits");
        pipeline
            .admit_replay(PredictionJournalInput::Bind {
                admission_seq: 3,
                generation: 2,
                previous_market_id: Some("KXBTC15M-1".to_owned()),
                reason: MarketTransitionReason::ScheduledRollover,
                event: kalshi_contract(1, 2, 900),
                logical_receipt_ns: 900 * SECOND_NS,
                now_ns: 900 * SECOND_NS,
            })
            .expect("scheduled rollover completing bind admits");

        assert_eq!(
            pipeline.report().socket_transitions,
            0,
            "a scheduled rollover admits no socket-connectivity input"
        );
        let entry = pipeline
            .report()
            .feed_health
            .feeds
            .into_iter()
            .find(|feed| feed.source_id == "kalshi.btc.market-contracts")
            .expect("contracts feed present");
        assert_eq!(
            entry.disconnect_count, 0,
            "a rollover never registers a socket disconnect"
        );
        assert_eq!(entry.reconnect_count, 0);
        assert_eq!(
            pipeline.feed_health.status("kalshi.btc.market-contracts"),
            Some(FeedHealthStatus::Live)
        );
    }

    #[test]
    fn connectivity_events_replay_deterministically() {
        let session = PredictionJournalSession::new(
            "2026-08-15T12:00:00Z".to_owned(),
            EPOCH.to_owned(),
            PredictionRuntimeConfig::new(PredictionMarketVenue::Kalshi, 2_000),
        )
        .expect("session builds");
        let inputs = vec![
            PredictionJournalInput::Bind {
                admission_seq: 0,
                generation: 1,
                previous_market_id: None,
                reason: MarketTransitionReason::InitialBind,
                event: kalshi_contract(0, 1, 0),
                logical_receipt_ns: 0,
                now_ns: 0,
            },
            PredictionJournalInput::Connectivity {
                admission_seq: 1,
                transport: PredictionTransport::Kalshi,
                transition: PredictionSocketTransition::Disconnected,
                now_ns: SECOND_NS,
            },
            PredictionJournalInput::Connectivity {
                admission_seq: 2,
                transport: PredictionTransport::Kalshi,
                transition: PredictionSocketTransition::Reconnecting,
                now_ns: 2 * SECOND_NS,
            },
            PredictionJournalInput::Connectivity {
                admission_seq: 3,
                transport: PredictionTransport::Kalshi,
                transition: PredictionSocketTransition::Connected,
                now_ns: 3 * SECOND_NS,
            },
        ];
        assert_eq!(
            run(&session, &inputs),
            run(&session, &inputs),
            "the same admitted connectivity events yield the same output"
        );
    }

    #[test]
    fn spot_ticker_events_replay_deterministically() {
        // The schema-8 SpotEvents variant admits Coinbase/Kraken ticker batches into
        // the internally hosted spot reference (ADR 0016). Like every other admitted
        // input they must replay byte-identically: the same ticker admissions
        // recompute the same reference and the same composite batch.
        let session = PredictionJournalSession::new(
            "2026-08-15T12:00:00Z".to_owned(),
            EPOCH.to_owned(),
            PredictionRuntimeConfig::new(PredictionMarketVenue::Kalshi, 2_000),
        )
        .expect("session builds");
        let inputs = vec![
            PredictionJournalInput::Bind {
                admission_seq: 0,
                generation: 1,
                previous_market_id: None,
                reason: MarketTransitionReason::InitialBind,
                event: kalshi_contract(0, 1, 0),
                logical_receipt_ns: 0,
                now_ns: 0,
            },
            PredictionJournalInput::SpotEvents {
                admission_seq: 1,
                events: Arc::new(vec![
                    coinbase_ticker(1, "60000", 1),
                    kraken_ticker(1, "60000", 1),
                ]),
                logical_receipt_ns: SECOND_NS,
                now_ns: SECOND_NS,
            },
            PredictionJournalInput::SpotEvents {
                admission_seq: 2,
                events: Arc::new(vec![
                    coinbase_ticker(2, "60010", 2),
                    kraken_ticker(2, "60010", 2),
                ]),
                logical_receipt_ns: 2 * SECOND_NS,
                now_ns: 2 * SECOND_NS,
            },
            PredictionJournalInput::HealthTick {
                admission_seq: 3,
                now_ns: 2 * SECOND_NS,
            },
        ];
        let first = run(&session, &inputs);
        assert!(!first.is_empty(), "the replay produced a stream");
        assert_eq!(
            first,
            run(&session, &inputs),
            "the same admitted ticker events yield byte-identical output"
        );
    }

    #[test]
    fn current_v3_prediction_journals_replay_byte_identically() {
        use crate::prediction_journal::{
            DEFAULT_PREDICTION_JOURNAL_MAX_BYTES, PredictionJournalReplayReader,
        };

        for venue in [
            PredictionMarketVenue::Polymarket,
            PredictionMarketVenue::Kalshi,
        ] {
            let session = PredictionJournalSession::new(
                "2026-08-15T12:00:00Z".to_owned(),
                EPOCH.to_owned(),
                PredictionRuntimeConfig::new(venue, 2_000),
            )
            .expect("v3 session builds");
            let inputs = match venue {
                PredictionMarketVenue::Polymarket => vec![
                    PredictionJournalInput::Bind {
                        admission_seq: 0,
                        generation: 1,
                        previous_market_id: None,
                        reason: MarketTransitionReason::InitialBind,
                        event: polymarket_contract(0, 1, 0),
                        logical_receipt_ns: 0,
                        now_ns: 0,
                    },
                    PredictionJournalInput::HealthTick {
                        admission_seq: 1,
                        now_ns: SECOND_NS,
                    },
                ],
                PredictionMarketVenue::Kalshi => vec![
                    PredictionJournalInput::Bind {
                        admission_seq: 0,
                        generation: 1,
                        previous_market_id: None,
                        reason: MarketTransitionReason::InitialBind,
                        event: kalshi_contract(0, 1, 0),
                        logical_receipt_ns: 0,
                        now_ns: 0,
                    },
                    PredictionJournalInput::MarketEvents {
                        admission_seq: 1,
                        generation: 1,
                        events: Arc::new(vec![kalshi_quote(0, 1, 1)]),
                        logical_receipt_ns: SECOND_NS,
                        now_ns: SECOND_NS,
                    },
                    PredictionJournalInput::HealthTick {
                        admission_seq: 2,
                        now_ns: 2 * SECOND_NS,
                    },
                ],
            };
            let processor = PredictionSignalProcessor::new(
                venue,
                EPOCH,
                "2026-08-15T12:00:00Z".parse().expect("wall start parses"),
            )
            .expect("v3 processor builds");
            let mut live =
                PredictionPipeline::replay(Vec::new(), processor, &session, 1_000, 1_048_576)
                    .expect("v3 live-equivalent pipeline builds");
            for input in &inputs {
                live.admit_replay(input.clone()).expect("v3 input admits");
            }
            let live_bytes = live.writer.clone();
            let end = live.journal_end();

            let lane = match venue {
                PredictionMarketVenue::Polymarket => "polymarket",
                PredictionMarketVenue::Kalshi => "kalshi",
            };
            let journal_path = std::env::temp_dir().join(format!(
                "stream-v3-{lane}-{}-{}.jsonl",
                std::process::id(),
                session.session_id
            ));
            let mut journal = PredictionJournalWriter::create(
                &journal_path,
                session.clone(),
                DEFAULT_PREDICTION_JOURNAL_MAX_BYTES,
            )
            .expect("v3 journal creates");
            for input in &inputs {
                journal.enqueue(input.clone()).expect("v3 input journals");
            }
            journal.finish(end.clone()).expect("v3 journal finishes");

            let mut reader = PredictionJournalReplayReader::open(
                &journal_path,
                DEFAULT_PREDICTION_JOURNAL_MAX_BYTES,
            )
            .expect("v3 journal opens");
            let replay_session = reader.session().clone();
            let replay_processor = PredictionSignalProcessor::new(
                venue,
                EPOCH,
                "2026-08-15T12:00:00Z".parse().expect("wall start parses"),
            )
            .expect("v3 replay processor builds");
            let mut replay = PredictionPipeline::replay(
                Vec::new(),
                replay_processor,
                &replay_session,
                1_000,
                1_048_576,
            )
            .expect("v3 replay pipeline builds");
            while let Some(input) = reader.next_input().expect("v3 journal input reads") {
                replay.admit_replay(input).expect("v3 input replays");
            }
            let replay_bytes = replay.writer.clone();
            replay
                .finish_replay(reader.end().expect("v3 journal end exists"))
                .expect("v3 replay totals agree");
            assert_eq!(
                live_bytes, replay_bytes,
                "{lane} replay must be byte-identical"
            );
            assert!(
                !live_bytes.is_empty(),
                "{lane} corpus output must be non-empty"
            );

            if let Some(root) = std::env::var_os("STREAM_V3_CORPUS_DIR") {
                let lane_dir = std::path::PathBuf::from(root).join(lane);
                std::fs::create_dir_all(&lane_dir).expect("corpus lane directory creates");
                std::fs::copy(&journal_path, lane_dir.join("journal.jsonl"))
                    .expect("corpus journal copies");
                std::fs::write(lane_dir.join("live.jsonl"), &live_bytes)
                    .expect("corpus live output writes");
                std::fs::write(lane_dir.join("replay.jsonl"), &replay_bytes)
                    .expect("corpus replay output writes");
            }
            std::fs::remove_file(journal_path).expect("temporary v3 journal removes");
        }
    }

    #[test]
    fn retained_v5_journal_replays_byte_identically_through_the_current_binary() {
        use crate::prediction_journal::{
            DEFAULT_PREDICTION_JOURNAL_MAX_BYTES, PREDICTION_JOURNAL_SCHEMA_VERSION,
            PredictionJournalReplayReader, RETAINED_ENVELOPED_PREDICTION_JOURNAL_SCHEMA_VERSION,
            write_enveloped_prediction_journal_at_version,
        };

        // Inputs a schema-5 journal could actually contain (no connectivity, which
        // did not exist at v5). Replaying them from a crafted v5 file must produce
        // byte-for-byte the same output as an equivalent current-schema file:
        // the retained evolution is output-neutral for the shared variants.
        let inputs = vec![
            PredictionJournalInput::Bind {
                admission_seq: 0,
                generation: 1,
                previous_market_id: None,
                reason: MarketTransitionReason::InitialBind,
                event: kalshi_contract(0, 1, 0),
                logical_receipt_ns: 0,
                now_ns: 0,
            },
            PredictionJournalInput::MarketEvents {
                admission_seq: 1,
                generation: 1,
                events: Arc::new(vec![kalshi_quote(0, 1, 1)]),
                logical_receipt_ns: SECOND_NS,
                now_ns: SECOND_NS,
            },
            PredictionJournalInput::HealthTick {
                admission_seq: 2,
                now_ns: 2 * SECOND_NS,
            },
        ];
        let end = PredictionJournalEnd {
            totals: PredictionJournalTotals {
                admitted_inputs: 3,
                discovery_attempts: 0,
                bindings: 1,
                market_event_batches: 1,
                spot_event_batches: 0,
                unavailable_transitions: 0,
                health_ticks: 1,
                updates_written: 0,
                stream_messages_written: 0,
            },
            quality_reason_entries: BTreeMap::new(),
        };

        let dir = std::env::temp_dir();
        let unique = std::process::id();
        let v5_path = dir.join(format!("prediction-runtime-v5-{unique}.jsonl"));
        let current_path = dir.join(format!("prediction-runtime-current-{unique}.jsonl"));

        // Current schema: the real writer, presently schema 8.
        let current_session = PredictionJournalSession::new(
            "2026-08-15T12:00:00Z".to_owned(),
            EPOCH.to_owned(),
            PredictionRuntimeConfig::new(PredictionMarketVenue::Kalshi, 2_000),
        )
        .expect("current session builds");
        let mut writer = PredictionJournalWriter::create(
            &current_path,
            current_session,
            DEFAULT_PREDICTION_JOURNAL_MAX_BYTES,
        )
        .expect("current journal creates");
        for input in &inputs {
            writer.enqueue(input.clone()).expect("input enqueues");
        }
        writer
            .finish(end.clone())
            .expect("current journal finishes");

        // v5: the same records, CRC-framed, stamped at the retained enveloped
        // schema — a stand-in for tonight's soak journals.
        let v5_session = PredictionJournalSession::new(
            "2026-08-15T12:00:00Z".to_owned(),
            EPOCH.to_owned(),
            PredictionRuntimeConfig::new(PredictionMarketVenue::Kalshi, 2_000),
        )
        .expect("v5 session builds");
        write_enveloped_prediction_journal_at_version(
            &v5_path,
            v5_session,
            &inputs,
            end,
            RETAINED_ENVELOPED_PREDICTION_JOURNAL_SCHEMA_VERSION,
        )
        .expect("v5 fixture writes");

        let replay_file = |path: &std::path::Path| -> (u16, Vec<u8>) {
            let mut reader =
                PredictionJournalReplayReader::open(path, DEFAULT_PREDICTION_JOURNAL_MAX_BYTES)
                    .expect("journal opens");
            let session = reader.session().clone();
            let version = session.journal_schema_version;
            let processor = PredictionSignalProcessor::new(
                session.venue,
                session.publisher_epoch.clone(),
                session.wall_start.parse().expect("wall start parses"),
            )
            .expect("processor builds");
            let mut pipeline =
                PredictionPipeline::replay(Vec::new(), processor, &session, 1_000, 4_194_304)
                    .expect("pipeline builds");
            while let Some(input) = reader.next_input().expect("input reads") {
                pipeline.admit_replay(input).expect("input admits");
            }
            (version, pipeline.writer)
        };

        let (current_version, current_output) = replay_file(&current_path);
        let (v5_version, v5_output) = replay_file(&v5_path);
        assert_eq!(current_version, PREDICTION_JOURNAL_SCHEMA_VERSION);
        assert_eq!(
            v5_version,
            RETAINED_ENVELOPED_PREDICTION_JOURNAL_SCHEMA_VERSION
        );
        assert!(!current_output.is_empty(), "the replay produced a stream");
        assert_eq!(
            v5_output, current_output,
            "a retained schema-5 journal replays byte-identically through the schema-8 binary"
        );

        let _ = std::fs::remove_file(v5_path);
        let _ = std::fs::remove_file(current_path);
    }

    #[test]
    fn prediction_feed_health_gates_emission_on_the_session_capability() {
        let inputs = vec![PredictionJournalInput::Bind {
            admission_seq: 0,
            generation: 1,
            previous_market_id: None,
            reason: MarketTransitionReason::InitialBind,
            event: kalshi_contract(0, 1, 0),
            logical_receipt_ns: 0,
            now_ns: 0,
        }];

        let enabled = PredictionJournalSession::new(
            "2026-08-15T12:00:00Z".to_owned(),
            EPOCH.to_owned(),
            PredictionRuntimeConfig::new(PredictionMarketVenue::Kalshi, 2_000),
        )
        .expect("enabled session builds");
        let enabled_output = run(&enabled, &inputs);
        assert!(
            count_feed_health(&enabled_output) > 0,
            "an enabled session emits feed_health frames"
        );
        assert_eq!(
            enabled_output,
            run(&enabled, &inputs),
            "an enabled session replays byte-identically"
        );

        let mut disabled_config =
            PredictionRuntimeConfig::new(PredictionMarketVenue::Kalshi, 2_000);
        disabled_config.feed_health_stream = false;
        let disabled = PredictionJournalSession::new(
            "2026-08-15T12:00:00Z".to_owned(),
            EPOCH.to_owned(),
            disabled_config,
        )
        .expect("disabled session builds");
        let disabled_output = run(&disabled, &inputs);
        assert_eq!(
            count_feed_health(&disabled_output),
            0,
            "a session without the capability emits no feed_health frames"
        );
    }

    fn run(session: &PredictionJournalSession, inputs: &[PredictionJournalInput]) -> Vec<u8> {
        let processor = PredictionSignalProcessor::new(
            PredictionMarketVenue::Kalshi,
            EPOCH,
            "2026-08-15T12:00:00Z".parse().expect("wall start parses"),
        )
        .expect("processor builds");
        let mut pipeline =
            PredictionPipeline::replay(Vec::new(), processor, session, 1_000, 1_000_000)
                .expect("pipeline builds");
        for input in inputs {
            pipeline
                .admit_replay(input.clone())
                .expect("input is admitted");
        }
        pipeline.writer
    }

    fn kalshi_contract(sequence: u64, market: u64, open_second: u64) -> SourceEvent {
        source_event(
            "kalshi.btc.market-contracts",
            sequence,
            open_second,
            BTreeMap::from([
                (
                    "market_ticker".to_owned(),
                    json!(format!("KXBTC15M-{market}")),
                ),
                (
                    "event_ticker".to_owned(),
                    json!(format!("KXBTC15M-E{market}")),
                ),
                ("title".to_owned(), json!("BTC price in fifteen minutes")),
                ("status".to_owned(), json!("active")),
                ("open_time".to_owned(), json!(timestamp(open_second))),
                ("close_time".to_owned(), json!(timestamp(open_second + 900))),
                (
                    "expected_expiration_time".to_owned(),
                    json!(timestamp(open_second + 900)),
                ),
                (
                    "legal_expiration_time".to_owned(),
                    json!(timestamp(open_second + 1_800)),
                ),
                ("benchmark".to_owned(), json!("cf_benchmarks_brti")),
                ("averaging_window_seconds".to_owned(), json!(60)),
                ("comparator".to_owned(), json!("greater_than_or_equal")),
                ("tie_outcome".to_owned(), json!("yes")),
                ("reference_value".to_owned(), json!("60100")),
                ("rules_sha256".to_owned(), json!("b".repeat(64))),
            ]),
        )
    }

    fn polymarket_contract(sequence: u64, market: u64, open_second: u64) -> SourceEvent {
        source_event(
            "polymarket.btc.market-contracts",
            sequence,
            open_second,
            BTreeMap::from([
                ("market_id".to_owned(), json!(format!("pm-{market}"))),
                ("condition_id".to_owned(), json!(format!("cond-{market}"))),
                (
                    "question_id".to_owned(),
                    json!(format!("question-{market}")),
                ),
                ("slug".to_owned(), json!(format!("btc-updown-{market}"))),
                (
                    "question".to_owned(),
                    json!("Will BTC be at or above the opening value?"),
                ),
                ("up_token_id".to_owned(), json!(format!("up-{market}"))),
                ("down_token_id".to_owned(), json!(format!("down-{market}"))),
                ("window_open".to_owned(), json!(timestamp(open_second))),
                (
                    "window_close".to_owned(),
                    json!(timestamp(open_second + 300)),
                ),
                (
                    "resolution_source".to_owned(),
                    json!("https://data.chain.link/streams/btc-usd-twap-60s-streams"),
                ),
                ("comparator".to_owned(), json!("greater_than_or_equal")),
                ("tie_outcome".to_owned(), json!("up")),
                ("minimum_tick_size".to_owned(), json!("0.01")),
                ("minimum_order_size".to_owned(), json!("5")),
                ("order_book_enabled".to_owned(), json!(true)),
                ("active".to_owned(), json!(true)),
                ("closed".to_owned(), json!(false)),
                ("rules_sha256".to_owned(), json!("a".repeat(64))),
            ]),
        )
    }

    fn kalshi_quote(sequence: u64, market: u64, second: u64) -> SourceEvent {
        source_event(
            "kalshi.btc.quotes",
            sequence,
            second,
            BTreeMap::from([
                (
                    "market_ticker".to_owned(),
                    json!(format!("KXBTC15M-{market}")),
                ),
                ("last_price".to_owned(), json!("0.41")),
                ("best_bid".to_owned(), json!("0.40")),
                ("best_ask".to_owned(), json!("0.42")),
                ("best_bid_size".to_owned(), json!("12")),
                ("best_ask_size".to_owned(), json!("6")),
                ("volume".to_owned(), json!("100")),
                ("open_interest".to_owned(), json!("50")),
            ]),
        )
    }

    fn coinbase_ticker(sequence: u64, price: &str, second: u64) -> SourceEvent {
        let event_time = timestamp(second);
        SourceEvent {
            schema_version: 1,
            source_id: "coinbase.exchange.btc-usd.ticker".to_owned(),
            source_version: "1.0.0".to_owned(),
            publisher_epoch: "coinbase-feed".to_owned(),
            sequence,
            event_time: event_time.clone(),
            received_at: event_time.clone(),
            values: BTreeMap::from([
                ("instrument".to_owned(), json!("BTC-USD")),
                ("trade_id".to_owned(), json!(100 + sequence)),
                ("price".to_owned(), json!(price)),
                ("last_size".to_owned(), json!("0.01")),
                ("best_bid".to_owned(), json!(price)),
                ("best_bid_size".to_owned(), json!("1.25")),
                ("best_ask".to_owned(), json!(price)),
                ("best_ask_size".to_owned(), json!("2.5")),
                ("open_24h".to_owned(), json!("59800")),
                ("volume_24h".to_owned(), json!("1250.5")),
                ("low_24h".to_owned(), json!("59000")),
                ("high_24h".to_owned(), json!("61000")),
                ("volume_30d".to_owned(), json!("30000")),
                ("event_time".to_owned(), json!(event_time)),
                ("source_sequence".to_owned(), json!(900 + sequence)),
            ]),
        }
    }

    fn kraken_ticker(sequence: u64, price: &str, second: u64) -> SourceEvent {
        let event_time = timestamp(second);
        SourceEvent {
            schema_version: 1,
            source_id: "kraken.spot.btc-usd.ticker".to_owned(),
            source_version: "1.0.0".to_owned(),
            publisher_epoch: "kraken-feed".to_owned(),
            sequence,
            event_time: event_time.clone(),
            received_at: event_time.clone(),
            values: BTreeMap::from([
                ("instrument".to_owned(), json!("BTC/USD")),
                ("best_bid".to_owned(), json!(price)),
                ("best_bid_size".to_owned(), json!("0.02")),
                ("best_ask".to_owned(), json!(price)),
                ("best_ask_size".to_owned(), json!("0.17")),
                ("last_price".to_owned(), json!(price)),
                ("volume_24h".to_owned(), json!("973.7")),
                ("vwap_24h".to_owned(), json!("60010")),
                ("low_24h".to_owned(), json!("59000")),
                ("high_24h".to_owned(), json!("61000")),
                ("change_24h".to_owned(), json!("-72.9")),
                ("change_percent_24h".to_owned(), json!("-0.11")),
                ("trade_count_24h".to_owned(), json!(46574)),
                ("update_kind".to_owned(), json!("update")),
                ("event_time".to_owned(), json!(event_time)),
            ]),
        }
    }

    fn source_event(
        source_id: &str,
        sequence: u64,
        second: u64,
        mut values: BTreeMap<String, Value>,
    ) -> SourceEvent {
        let event_time = timestamp(second);
        values.insert("event_time".to_owned(), json!(event_time));
        SourceEvent {
            schema_version: 1,
            source_id: source_id.to_owned(),
            source_version: "1.0.0".to_owned(),
            publisher_epoch: EPOCH.to_owned(),
            sequence,
            event_time: event_time.clone(),
            received_at: event_time,
            values,
        }
    }

    fn timestamp(second: u64) -> String {
        DateTime::parse_from_rfc3339("2026-08-15T12:00:00Z")
            .expect("origin parses")
            .with_timezone(&Utc)
            .checked_add_signed(ChronoDuration::seconds(
                i64::try_from(second).expect("test second fits"),
            ))
            .expect("timestamp fits")
            .to_rfc3339_opts(SecondsFormat::Millis, true)
    }

    /// Ruling E25 / audit item 3: a code-8 refusal of the 5 Hz channel publishes
    /// `cfbrti_subscription_rejected` on the instances derived from that source,
    /// and the acceptance that follows a reconnect clears it. The 1 Hz channel
    /// and the Kalshi market channels are never touched by it.
    #[test]
    fn an_index_channel_refusal_publishes_a_reason_and_clears_on_acceptance() {
        let session = PredictionJournalSession::new(
            "2026-09-03T06:00:00Z".to_owned(),
            EPOCH.to_owned(),
            PredictionRuntimeConfig::new(PredictionMarketVenue::Kalshi, 2_000),
        )
        .expect("session builds");
        let processor = PredictionSignalProcessor::new(
            PredictionMarketVenue::Kalshi,
            EPOCH,
            "2026-09-03T06:00:00Z".parse().expect("wall start parses"),
        )
        .expect("processor builds");
        let mut pipeline =
            PredictionPipeline::replay(Vec::new(), processor, &session, 1_000, 1_000_000)
                .expect("pipeline builds");

        assert_eq!(
            pipeline.index_channel_unavailable_reason(KALSHI_BRTI_5HZ_SOURCE_ID),
            None,
            "available until the venue says otherwise"
        );

        pipeline
            .refuse_index_channel(KALSHI_BRTI_5HZ_SOURCE_ID, "cfbrti_subscription_rejected")
            .expect("a registered unavailable reason is accepted");
        assert_eq!(
            pipeline.index_channel_unavailable_reason(KALSHI_BRTI_5HZ_SOURCE_ID),
            Some("cfbrti_subscription_rejected")
        );
        // Per-source, never per-transport: the 1 Hz channel stays available.
        assert_eq!(
            pipeline.index_channel_unavailable_reason(KALSHI_BRTI_SOURCE_ID),
            None,
            "a 5 Hz refusal must not mark the 1 Hz channel unavailable"
        );

        // The acceptance after a reconnect clears it, and reports that it did.
        assert!(pipeline.accept_index_channel(KALSHI_BRTI_5HZ_SOURCE_ID));
        assert_eq!(
            pipeline.index_channel_unavailable_reason(KALSHI_BRTI_5HZ_SOURCE_ID),
            None
        );
        assert!(
            !pipeline.accept_index_channel(KALSHI_BRTI_5HZ_SOURCE_ID),
            "clearing an unrefused channel is not a transition"
        );

        // Only a registered `Unavailable` reason may be published.
        assert!(
            pipeline
                .refuse_index_channel(KALSHI_BRTI_5HZ_SOURCE_ID, "cfbrti_warming_up")
                .is_err(),
            "a warming reason is not an unavailability"
        );
        assert!(
            pipeline
                .refuse_index_channel(KALSHI_BRTI_5HZ_SOURCE_ID, "not_a_registered_reason")
                .is_err()
        );
    }

    /// The index socket is a second, independently connected transport on the
    /// Kalshi lane. A market-socket teardown — the 15-minute rollover, or a
    /// `RediscoverPredictionMarket` — moves the `kalshi` transport only, so the
    /// index feeds cannot be marked sick 96 times a day for market reasons.
    #[test]
    fn kalshi_index_survives_market_rollover() {
        for source_id in [
            KALSHI_BRTI_SOURCE_ID,
            KALSHI_BRTI_SETTLEMENT_SOURCE_ID,
            KALSHI_BRTI_5HZ_SOURCE_ID,
            KALSHI_PYTH_SOURCE_ID,
        ] {
            assert_eq!(
                prediction_feed_transport(source_id, PredictionMarketVenue::Kalshi),
                "kalshi_index",
                "{source_id} must not ride the market socket's transport"
            );
        }
        // The adopted market channels keep the lane's own transport.
        for source_id in [
            "kalshi.btc.book-levels",
            "kalshi.btc.quotes",
            "kalshi.btc.trades",
            "kalshi.btc.market-contracts",
        ] {
            assert_eq!(
                prediction_feed_transport(source_id, PredictionMarketVenue::Kalshi),
                "kalshi"
            );
        }
        assert_eq!(
            PredictionTransport::KalshiIndex.health_label(),
            "kalshi_index"
        );
        assert!(PredictionTransport::KalshiIndex.is_allowed_for(PredictionMarketVenue::Kalshi));
        assert!(
            !PredictionTransport::KalshiIndex.is_allowed_for(PredictionMarketVenue::Polymarket)
        );
    }

    /// Index events carry no market binding, so a batch of them takes the
    /// current generation instead of requiring a bound market — exactly as the
    /// Polymarket TWAP relay already does.
    #[test]
    fn index_events_admit_as_auxiliary_generation() {
        for source_id in [
            POLYMARKET_TWAP_SOURCE_ID,
            KALSHI_BRTI_SOURCE_ID,
            KALSHI_BRTI_SETTLEMENT_SOURCE_ID,
            KALSHI_BRTI_5HZ_SOURCE_ID,
            KALSHI_PYTH_SOURCE_ID,
        ] {
            assert!(is_auxiliary_source(source_id), "{source_id} is auxiliary");
        }
        for source_id in ["kalshi.btc.quotes", "polymarket.btc.trades"] {
            assert!(
                !is_auxiliary_source(source_id),
                "{source_id} is market-relative"
            );
        }
    }

    /// The board bounds are cadence times a missed-tick allowance. The 1 Hz
    /// cadence is observed (E23); the bounds themselves stay provisional until a
    /// soak measures them.
    #[test]
    fn index_liveness_bounds_are_the_documented_cadences() {
        assert_eq!(
            prediction_liveness_override(KALSHI_BRTI_SOURCE_ID),
            Some(10_000)
        );
        assert_eq!(
            prediction_liveness_override(KALSHI_BRTI_5HZ_SOURCE_ID),
            Some(5_000)
        );
        assert_eq!(
            prediction_liveness_override(KALSHI_BRTI_SETTLEMENT_SOURCE_ID),
            Some(960_000)
        );
        // No Pyth frame has ever been observed, so the relay keeps the default.
        assert_eq!(prediction_liveness_override(KALSHI_PYTH_SOURCE_ID), None);
        for source_id in [
            KALSHI_BRTI_SOURCE_ID,
            KALSHI_BRTI_SETTLEMENT_SOURCE_ID,
            KALSHI_BRTI_5HZ_SOURCE_ID,
            KALSHI_PYTH_SOURCE_ID,
        ] {
            assert_eq!(prediction_feed_cadence(source_id), FeedCadence::Periodic);
        }
    }

    /// Regression (wave2 kalshi lane, third live window): the E37b completing
    /// tick and the market rollover that follows it are driven THROUGH the real
    /// admission/publication boundary of an armed Kalshi lane, not just the
    /// processor. The completing tick must publish exactly one final ready
    /// settlement decision carrying `settlement_window_complete: true`; a later
    /// tick on the closed market must go `market_closed`; and the scheduled
    /// rollover — which invalidates the departing generation's market-scoped
    /// instances — must admit WITHOUT a scope refusal. Before the fix the
    /// rollover tombstoned the global `btc.cfbrti.oracle_reference` under a market
    /// scope, tripping the boundary with "scope: does not match the signal-id
    /// venue namespace" and killing the lane at the quarter close.
    #[test]
    fn e37b_completing_tick_and_rollover_pass_the_admission_boundary() {
        use crate::{CFBRTI_ORACLE_REFERENCE_ID, KALSHI_SETTLEMENT_ANCHOR_STATE_ID};
        let sink = SharedSink(Arc::new(std::sync::Mutex::new(Vec::new())));
        let mut pipeline = armed_kalshi_pipeline(sink.clone());
        // Warm the settlement anchor well before the close: a full sixty-second
        // average and an incomplete window (`window_as_of` 120 != close 900).
        pipeline
            .admit_replay(PredictionJournalInput::MarketEvents {
                admission_seq: 1,
                generation: 1,
                events: Arc::new(vec![
                    cf_value(1, 120, "60200.5", "60150.25000000", 60),
                    cf_settlement(2, 120, 900, 120, 60),
                ]),
                logical_receipt_ns: 120 * SECOND_NS,
                now_ns: 120 * SECOND_NS,
            })
            .expect("pre-close frame admits and the anchor warms");

        // The completing frame arrives at the grid close: `window_as_of == close`
        // with a full window. `advance_time` at the close fires the one final
        // ready decision (E37b) before the market closes.
        pipeline
            .admit_replay(PredictionJournalInput::MarketEvents {
                admission_seq: 2,
                generation: 1,
                events: Arc::new(vec![
                    cf_value(3, 900, "60200.5", "60150.25000000", 60),
                    cf_settlement(4, 900, 900, 900, 60),
                ]),
                logical_receipt_ns: 900 * SECOND_NS,
                now_ns: 900 * SECOND_NS,
            })
            .expect("the completing tick admits without a scope refusal");

        // A tick on the now-closed market publishes `market_closed`: the final
        // decision is spent, so the anchor no longer settles.
        pipeline
            .admit_replay(PredictionJournalInput::MarketEvents {
                admission_seq: 3,
                generation: 1,
                events: Arc::new(vec![cf_value(5, 901, "60200.5", "60150.25000000", 60)]),
                logical_receipt_ns: 901 * SECOND_NS,
                now_ns: 901 * SECOND_NS,
            })
            .expect("a post-close tick admits");

        // The scheduled rollover invalidates the departing generation. This is
        // the path the third live window died on: the global oracle reference
        // must NOT be swept into the market-scoped invalidation.
        pipeline
            .admit_replay(PredictionJournalInput::Unavailable {
                admission_seq: 4,
                generation: 2,
                previous_market_id: Some("KXBTC15M-1".to_owned()),
                reason: MarketTransitionReason::ScheduledRollover,
                now_ns: 901 * SECOND_NS,
            })
            .expect("the rollover invalidation admits without a scope refusal");

        drop(pipeline);
        let bytes = sink.0.lock().expect("sink lock").clone();

        // Exactly one final ready decision carries `settlement_window_complete`
        // true, and it is `Ready` — the E37b decision fires once, not on every
        // later tick or on the rollover.
        let final_anchor: Vec<SignalUpdate> = batch_updates(&bytes)
            .into_iter()
            .filter(|update| {
                update.signal_id == KALSHI_SETTLEMENT_ANCHOR_STATE_ID
                    && update.values.get("settlement_window_complete") == Some(&json!(true))
            })
            .collect();
        assert_eq!(
            final_anchor.len(),
            1,
            "exactly one final settlement decision carries settlement_window_complete true"
        );
        assert_eq!(
            final_anchor[0].quality.state,
            signals_core::QualityState::Ready,
            "the final settlement decision is ready"
        );
        assert!(
            matches!(final_anchor[0].scope, SignalScope::Market { .. }),
            "the final settlement decision is market-scoped"
        );

        let messages = decode(&bytes);
        // The closed market publishes `market_closed` after its final decision.
        assert!(
            messages.iter().any(|message| matches!(
                message,
                SignalStreamMessage::InstanceStatus { signal_id, reasons, .. }
                    if signal_id == KALSHI_SETTLEMENT_ANCHOR_STATE_ID
                        && reasons.iter().any(|reason| reason == "market_closed")
            )),
            "the closed market publishes market_closed on the settlement anchor"
        );
        // The global oracle reference is never tombstoned under a market scope:
        // it survives the rollover, so no instance-status for it carries a
        // Market scope.
        assert!(
            !messages.iter().any(|message| matches!(
                message,
                SignalStreamMessage::InstanceStatus { signal_id, scope, .. }
                    if signal_id == CFBRTI_ORACLE_REFERENCE_ID
                        && matches!(scope, SignalScope::Market { .. })
            )),
            "the global oracle reference is never invalidated under a market scope"
        );
    }

    /// E37c — the completing settlement-window frame arrives 200 ms AFTER the
    /// scheduled rollover. The departed quarter's one final ready decision must
    /// still publish, for BOTH twins, scoped to the DEPARTED market, with
    /// `settlement_window_complete: true`, and the admission/publication boundary
    /// must accept it. Driven through the real `admit_replay` path.
    #[test]
    fn e37c_completing_frame_after_the_roll_publishes_the_departed_final_decision() {
        use crate::{KALSHI_ORACLE_DIVERGENCE_ID, KALSHI_SETTLEMENT_ANCHOR_STATE_ID};
        const MS: u64 = 1_000_000;
        let sink = SharedSink(Arc::new(std::sync::Mutex::new(Vec::new())));
        let mut pipeline = armed_kalshi_pipeline(sink.clone());
        // Warm the composite spot reference (both venues) so the divergence twin
        // has a mid to freeze at the close.
        for (seq, second) in [(1_u64, 897_u64), (2, 898)] {
            pipeline
                .admit_replay(PredictionJournalInput::SpotEvents {
                    admission_seq: seq,
                    events: Arc::new(vec![
                        coinbase_ticker(seq, "60000", second),
                        kraken_ticker(seq, "60000", second),
                    ]),
                    logical_receipt_ns: second * SECOND_NS,
                    now_ns: second * SECOND_NS,
                })
                .expect("spot warms");
        }
        // A ready settlement anchor one second before the close, window still
        // incomplete (as_of 899 != close 900).
        pipeline
            .admit_replay(PredictionJournalInput::MarketEvents {
                admission_seq: 3,
                generation: 1,
                events: Arc::new(vec![
                    cf_value(1, 899, "60200.5", "60150.25000000", 60),
                    cf_settlement(2, 899, 900, 899, 59),
                ]),
                logical_receipt_ns: 899 * SECOND_NS,
                now_ns: 899 * SECOND_NS,
            })
            .expect("pre-close anchor admits, window incomplete");
        // The scheduled rollover fires AT the close, before the completing frame.
        pipeline
            .admit_replay(PredictionJournalInput::Unavailable {
                admission_seq: 4,
                generation: 2,
                previous_market_id: Some("KXBTC15M-1".to_owned()),
                reason: MarketTransitionReason::ScheduledRollover,
                now_ns: 900 * SECOND_NS,
            })
            .expect("rollover admits and freezes the pending final decision");
        // The successor market binds.
        pipeline
            .admit_replay(PredictionJournalInput::Bind {
                admission_seq: 5,
                generation: 2,
                previous_market_id: Some("KXBTC15M-1".to_owned()),
                reason: MarketTransitionReason::ScheduledRollover,
                event: kalshi_contract(10, 2, 900),
                logical_receipt_ns: 900 * SECOND_NS + 100 * MS,
                now_ns: 900 * SECOND_NS + 100 * MS,
            })
            .expect("successor binds");
        // 200 ms after the roll the completing frame for the DEPARTED quarter
        // (window_as_of == close 900, full window) arrives.
        pipeline
            .admit_replay(PredictionJournalInput::MarketEvents {
                admission_seq: 6,
                generation: 2,
                events: Arc::new(vec![cf_settlement(3, 900, 900, 900, 60)]),
                logical_receipt_ns: 900 * SECOND_NS + 200 * MS,
                now_ns: 900 * SECOND_NS + 200 * MS,
            })
            .expect("the completing frame admits without a scope refusal");

        drop(pipeline);
        let bytes = sink.0.lock().expect("sink lock").clone();
        // The departed final decision: both twins, scoped to the DEPARTED market
        // (generation 1), Ready, carrying settlement_window_complete true and
        // window_as_of == the quarter close.
        let departed: Vec<SignalUpdate> = batch_updates(&bytes)
            .into_iter()
            .filter(|update| {
                matches!(
                    &update.scope,
                    SignalScope::Market { market_id, market_generation }
                        if market_id == "KXBTC15M-1" && *market_generation == 1
                )
            })
            .collect();
        for id in [
            KALSHI_SETTLEMENT_ANCHOR_STATE_ID,
            KALSHI_ORACLE_DIVERGENCE_ID,
        ] {
            let finals: Vec<&SignalUpdate> = departed
                .iter()
                .filter(|update| {
                    update.signal_id == id
                        && update.values.get("settlement_window_complete") == Some(&json!(true))
                })
                .collect();
            assert_eq!(
                finals.len(),
                1,
                "{id}: exactly one departed final decision with settlement_window_complete true"
            );
            let final_update = finals[0];
            assert_eq!(
                final_update.quality.state,
                signals_core::QualityState::Ready,
                "{id}: the departed final decision is ready"
            );
            assert_eq!(
                final_update
                    .values
                    .get("window_as_of")
                    .and_then(Value::as_str),
                Some(timestamp(900).as_str()),
                "{id}: window_as_of is the departed quarter close"
            );
        }

        // M1: the two settlement identities departed into a pending final
        // decision, so their invalidation is DEFERRED past the roll. On the wire
        // each identity's departed-scope (generation 1) status sequence is
        // ready(final) -> invalidated: the sole `invalidated` instance-status
        // follows the departed final ready decision, is emitted exactly once, and
        // never precedes a ready — the release deletes no tombstone because none
        // was written at the roll.
        let messages = decode(&bytes);
        let departed_gen1 = |scope: &SignalScope| {
            matches!(
                scope,
                SignalScope::Market { market_id, market_generation }
                    if market_id == "KXBTC15M-1" && *market_generation == 1
            )
        };
        for id in [
            KALSHI_SETTLEMENT_ANCHOR_STATE_ID,
            KALSHI_ORACLE_DIVERGENCE_ID,
        ] {
            let mut final_ready_at = None;
            let mut invalidated_at = None;
            let mut invalidated_count = 0;
            for (index, message) in messages.iter().enumerate() {
                match message {
                    SignalStreamMessage::SignalBatch { updates, .. } => {
                        for update in updates {
                            if update.signal_id == id
                                && departed_gen1(&update.scope)
                                && update.quality.state == signals_core::QualityState::Ready
                                && update.values.get("settlement_window_complete")
                                    == Some(&json!(true))
                            {
                                final_ready_at.get_or_insert(index);
                            }
                        }
                    }
                    SignalStreamMessage::InstanceStatus {
                        signal_id,
                        scope,
                        state,
                        ..
                    } if signal_id == id
                        && departed_gen1(scope)
                        && *state == signals_core::InstanceStatusState::Invalidated =>
                    {
                        invalidated_at.get_or_insert(index);
                        invalidated_count += 1;
                    }
                    _ => {}
                }
            }
            let final_ready_at = final_ready_at
                .unwrap_or_else(|| panic!("{id}: the departed final ready publishes"));
            let invalidated_at = invalidated_at.unwrap_or_else(|| {
                panic!("{id}: the departed identity is invalidated after release")
            });
            assert_eq!(
                invalidated_count, 1,
                "{id}: the departed identity is invalidated exactly once"
            );
            assert!(
                final_ready_at < invalidated_at,
                "{id}: ready(final) precedes invalidated (never invalidated -> ready)"
            );
        }
    }

    /// E37c — the completing frame never arrives within the retention window.
    /// The slot expires, nothing is published for the departed market, and the
    /// expiry is counted in feed health.
    #[test]
    fn e37c_pending_final_decision_expires_when_the_frame_never_arrives() {
        let sink = SharedSink(Arc::new(std::sync::Mutex::new(Vec::new())));
        let mut pipeline = armed_kalshi_pipeline(sink.clone());
        for (seq, second) in [(1_u64, 897_u64), (2, 898)] {
            pipeline
                .admit_replay(PredictionJournalInput::SpotEvents {
                    admission_seq: seq,
                    events: Arc::new(vec![
                        coinbase_ticker(seq, "60000", second),
                        kraken_ticker(seq, "60000", second),
                    ]),
                    logical_receipt_ns: second * SECOND_NS,
                    now_ns: second * SECOND_NS,
                })
                .expect("spot warms");
        }
        pipeline
            .admit_replay(PredictionJournalInput::MarketEvents {
                admission_seq: 3,
                generation: 1,
                events: Arc::new(vec![
                    cf_value(1, 899, "60200.5", "60150.25000000", 60),
                    cf_settlement(2, 899, 900, 899, 59),
                ]),
                logical_receipt_ns: 899 * SECOND_NS,
                now_ns: 899 * SECOND_NS,
            })
            .expect("pre-close anchor admits");
        pipeline
            .admit_replay(PredictionJournalInput::Unavailable {
                admission_seq: 4,
                generation: 2,
                previous_market_id: Some("KXBTC15M-1".to_owned()),
                reason: MarketTransitionReason::ScheduledRollover,
                now_ns: 900 * SECOND_NS,
            })
            .expect("rollover freezes the pending final decision");
        // Health ticks advance lane time past the 5 s retention deadline with no
        // completing frame. The lane stays unbound; the slot must still expire.
        for (seq, second) in [(5_u64, 901_u64), (6, 903), (7, 905)] {
            pipeline
                .admit_replay(PredictionJournalInput::HealthTick {
                    admission_seq: seq,
                    now_ns: second * SECOND_NS,
                })
                .expect("health tick admits");
        }

        let expired = pipeline.feed_health.snapshot().final_decisions_expired;
        let bytes = sink.0.lock().expect("sink lock").clone();
        drop(pipeline);
        assert_eq!(
            expired, 1,
            "the lapsed final decision is counted in feed health"
        );
        let departed_finals = batch_updates(&bytes).into_iter().any(|update| {
            matches!(
                &update.scope,
                SignalScope::Market { market_id, market_generation }
                    if market_id == "KXBTC15M-1" && *market_generation == 1
            ) && update.values.get("settlement_window_complete") == Some(&json!(true))
        });
        assert!(
            !departed_finals,
            "no departed final decision is published when the frame never arrives"
        );
    }

    /// M1 (Minor 1) — the completing frame arrives 200 ms AFTER the +5 s retention
    /// deadline. Expiry is tested before completeness, so the slot expires
    /// (counted) rather than releasing an arbitrarily late "final" decision: a
    /// completing frame that beats the deadline releases, one that misses it does
    /// not.
    #[test]
    fn e37c_a_completing_frame_after_the_deadline_expires_rather_than_releasing() {
        const MS: u64 = 1_000_000;
        let sink = SharedSink(Arc::new(std::sync::Mutex::new(Vec::new())));
        let mut pipeline = armed_kalshi_pipeline(sink.clone());
        for (seq, second) in [(1_u64, 897_u64), (2, 898)] {
            pipeline
                .admit_replay(PredictionJournalInput::SpotEvents {
                    admission_seq: seq,
                    events: Arc::new(vec![
                        coinbase_ticker(seq, "60000", second),
                        kraken_ticker(seq, "60000", second),
                    ]),
                    logical_receipt_ns: second * SECOND_NS,
                    now_ns: second * SECOND_NS,
                })
                .expect("spot warms");
        }
        pipeline
            .admit_replay(PredictionJournalInput::MarketEvents {
                admission_seq: 3,
                generation: 1,
                events: Arc::new(vec![
                    cf_value(1, 899, "60200.5", "60150.25000000", 60),
                    cf_settlement(2, 899, 900, 899, 59),
                ]),
                logical_receipt_ns: 899 * SECOND_NS,
                now_ns: 899 * SECOND_NS,
            })
            .expect("pre-close anchor admits, window incomplete");
        // The roll at the close freezes the pending final decision; its retention
        // deadline is +5 s (905 s).
        pipeline
            .admit_replay(PredictionJournalInput::Unavailable {
                admission_seq: 4,
                generation: 2,
                previous_market_id: Some("KXBTC15M-1".to_owned()),
                reason: MarketTransitionReason::ScheduledRollover,
                now_ns: 900 * SECOND_NS,
            })
            .expect("rollover freezes the pending final decision");
        pipeline
            .admit_replay(PredictionJournalInput::Bind {
                admission_seq: 5,
                generation: 2,
                previous_market_id: Some("KXBTC15M-1".to_owned()),
                reason: MarketTransitionReason::ScheduledRollover,
                event: kalshi_contract(10, 2, 900),
                logical_receipt_ns: 900 * SECOND_NS + 100 * MS,
                now_ns: 900 * SECOND_NS + 100 * MS,
            })
            .expect("successor binds");
        // The completing frame for the departed quarter (window_as_of == close 900,
        // full window) arrives at 905.2 s — 200 ms PAST the 905 s deadline.
        pipeline
            .admit_replay(PredictionJournalInput::MarketEvents {
                admission_seq: 6,
                generation: 2,
                events: Arc::new(vec![cf_settlement(3, 905, 900, 900, 60)]),
                logical_receipt_ns: 905 * SECOND_NS + 200 * MS,
                now_ns: 905 * SECOND_NS + 200 * MS,
            })
            .expect("the late completing frame admits");

        let expired = pipeline.feed_health.snapshot().final_decisions_expired;
        let bytes = sink.0.lock().expect("sink lock").clone();
        drop(pipeline);
        assert_eq!(
            expired, 1,
            "a completing frame past the deadline expires the slot rather than releasing"
        );
        let departed_finals = batch_updates(&bytes).into_iter().any(|update| {
            matches!(
                &update.scope,
                SignalScope::Market { market_id, market_generation }
                    if market_id == "KXBTC15M-1" && *market_generation == 1
            ) && update.values.get("settlement_window_complete") == Some(&json!(true))
        });
        assert!(
            !departed_finals,
            "no late final decision is published once the retention deadline has passed"
        );
    }

    /// E37c — a restart in the gap neither loses nor double-publishes the pending
    /// final decision. The processor is checkpointed with a filled slot, restored
    /// from that exact serialized state, and then driven through the completing
    /// frame: the departed decision releases exactly once. A further restart after
    /// the release, driven through the completing frame again, publishes nothing
    /// (the checkpointed high-water mark guards it).
    #[test]
    fn e37c_pending_final_decision_survives_a_restart_in_the_gap() {
        use crate::{
            KALSHI_ORACLE_DIVERGENCE_ID, KALSHI_SETTLEMENT_ANCHOR_STATE_ID,
            PredictionSignalCheckpointState, PredictionSignalProcessor,
        };

        // Each restart opens a fresh publisher epoch, exactly as a real resume
        // does (the spot reference refuses a reused epoch).
        fn armed_processor(
            epoch: &str,
            state: Option<PredictionSignalCheckpointState>,
        ) -> PredictionSignalProcessor {
            let wall_start = "2026-08-15T12:00:00Z".parse().expect("wall start parses");
            match state {
                Some(state) => {
                    PredictionSignalProcessor::restore_indexed(epoch, wall_start, state, true)
                        .expect("armed processor restores")
                }
                None => PredictionSignalProcessor::new_indexed(
                    PredictionMarketVenue::Kalshi,
                    epoch,
                    wall_start,
                    true,
                )
                .expect("armed processor builds"),
            }
        }

        // Warm a processor to a filled pending slot, then roll.
        let mut processor = armed_processor(EPOCH, None);
        processor
            .bind_market(1, &kalshi_contract(0, 1, 0), 0, 0)
            .expect("bind gen1");
        for (seq, second) in [(1_u64, 897_u64), (2, 898)] {
            for event in [
                coinbase_ticker(seq, "60000", second),
                kraken_ticker(seq, "60000", second),
            ] {
                processor
                    .ingest(&event, second * SECOND_NS, second * SECOND_NS)
                    .expect("spot ingests");
            }
            processor.advance_time(second * SECOND_NS).expect("advance");
        }
        for event in [
            cf_value(1, 899, "60200.5", "60150.25000000", 60),
            cf_settlement(2, 899, 900, 899, 59),
        ] {
            processor
                .ingest(&event, 899 * SECOND_NS, 899 * SECOND_NS)
                .expect("cf ingests");
        }
        processor
            .advance_time(899 * SECOND_NS)
            .expect("advance pre-close");
        processor
            .mark_unavailable(900 * SECOND_NS, "market_rollover")
            .expect("rollover");

        // The pending slot is checkpointed and survives a serialize round-trip.
        let state = processor.checkpoint_state();
        assert!(
            state.pending_final_decision.is_some(),
            "the rollover checkpointed a pending final decision"
        );
        let json = serde_json::to_string(&state).expect("state serializes");
        let restored: PredictionSignalCheckpointState =
            serde_json::from_str(&json).expect("state round-trips");
        assert_eq!(
            restored, state,
            "the pending slot round-trips byte-for-byte"
        );
        // A second copy of the exact restored state for the runtime-boundary leg
        // below, which asserts the deferred twin invalidation survives the restart.
        let restored_for_wire = restored.clone();

        // Restart in the gap: restore, then the completing frame arrives.
        let mut resumed = armed_processor("prediction-runtime-test-resume-1", Some(restored));
        resumed
            .ingest(
                &cf_settlement(3, 900, 900, 900, 60),
                900 * SECOND_NS + 200_000_000,
                900 * SECOND_NS + 200_000_000,
            )
            .expect("completing frame ingests after restore");
        resumed
            .advance_time(900 * SECOND_NS + 200_000_000)
            .expect("advance releases the departed decision");
        let released = resumed.take_departed_updates();
        // Exactly once PER TWIN — a duplicate release would slip past `.any(...)`.
        for id in [
            KALSHI_SETTLEMENT_ANCHOR_STATE_ID,
            KALSHI_ORACLE_DIVERGENCE_ID,
        ] {
            let count = released
                .iter()
                .filter(|update| {
                    update.signal_id == id
                        && update.values.get("settlement_window_complete") == Some(&json!(true))
                        && matches!(
                            &update.scope,
                            SignalScope::Market { market_id, market_generation }
                                if market_id == "KXBTC15M-1" && *market_generation == 1
                        )
                })
                .count();
            assert_eq!(
                count, 1,
                "{id}: the resumed processor releases the departed final decision exactly once"
            );
        }

        // A second restart AFTER the release must not re-publish: the high-water
        // mark is checkpointed and the slot is cleared.
        let after = resumed.checkpoint_state();
        assert!(
            after.pending_final_decision.is_none(),
            "the released slot is cleared in the checkpoint"
        );
        let mut again = armed_processor("prediction-runtime-test-resume-2", Some(after));
        again
            .ingest(
                &cf_settlement(4, 900, 900, 900, 60),
                901 * SECOND_NS,
                901 * SECOND_NS,
            )
            .expect("a repeat completing frame ingests");
        again.advance_time(901 * SECOND_NS).expect("advance");
        assert!(
            again.take_departed_updates().is_empty(),
            "a restart after the release never re-publishes the final decision"
        );

        // M1 restart-in-gap at the RUNTIME boundary. The deferred twin
        // invalidation is runtime state the checkpoint does not carry; only the
        // pending slot is checkpointed. A resumed PIPELINE must reconstruct the
        // deferred invalidation from that restored slot, else the departed final
        // ready decision releases with no following `invalidated` — forever.
        // Restore a pipeline from the same checkpoint, rebind the successor, feed
        // the completing frame, and assert ready(final) -> invalidated per twin on
        // the wire (the processor-level assertions above never exercise this).
        let wall_start: DateTime<Utc> = "2026-08-15T12:00:00Z".parse().expect("wall start parses");
        const RESUME_EPOCH: &str = "prediction-runtime-test-resume-wire";
        let resumed_processor = PredictionSignalProcessor::restore_indexed(
            RESUME_EPOCH,
            wall_start,
            restored_for_wire,
            true,
        )
        .expect("armed processor restores for the pipeline leg");
        let session = PredictionJournalSession::new(
            "2026-08-15T12:00:00Z".to_owned(),
            RESUME_EPOCH.to_owned(),
            PredictionRuntimeConfig::new(PredictionMarketVenue::Kalshi, 2_000)
                .with_index_armed(true),
        )
        .expect("resumed armed session builds");
        let sink = SharedSink(Arc::new(std::sync::Mutex::new(Vec::new())));
        let mut pipeline = PredictionPipeline::replay(
            sink.clone(),
            resumed_processor,
            &session,
            10_000,
            10_000_000,
        )
        .expect("resumed pipeline builds");
        // The index/market events a resumed lane admits carry its own (fresh)
        // publisher epoch, so retag the successor contract and completing frame.
        let mut successor = kalshi_contract(10, 2, 900);
        successor.publisher_epoch = RESUME_EPOCH.to_owned();
        pipeline
            .admit_replay(PredictionJournalInput::Bind {
                admission_seq: 0,
                generation: 2,
                previous_market_id: Some("KXBTC15M-1".to_owned()),
                reason: MarketTransitionReason::ScheduledRollover,
                event: successor,
                logical_receipt_ns: 900 * SECOND_NS + 100_000_000,
                now_ns: 900 * SECOND_NS + 100_000_000,
            })
            .expect("the successor rebinds after the restart");
        let mut completing = cf_settlement(3, 900, 900, 900, 60);
        completing.publisher_epoch = RESUME_EPOCH.to_owned();
        pipeline
            .admit_replay(PredictionJournalInput::MarketEvents {
                admission_seq: 1,
                generation: 2,
                events: Arc::new(vec![completing]),
                logical_receipt_ns: 900 * SECOND_NS + 200_000_000,
                now_ns: 900 * SECOND_NS + 200_000_000,
            })
            .expect("the completing frame admits after the restart");

        drop(pipeline);
        let bytes = sink.0.lock().expect("sink lock").clone();
        let messages = decode(&bytes);
        let departed_gen1 = |scope: &SignalScope| {
            matches!(
                scope,
                SignalScope::Market { market_id, market_generation }
                    if market_id == "KXBTC15M-1" && *market_generation == 1
            )
        };
        for id in [
            KALSHI_SETTLEMENT_ANCHOR_STATE_ID,
            KALSHI_ORACLE_DIVERGENCE_ID,
        ] {
            let mut final_ready_at = None;
            let mut invalidated_at = None;
            let mut invalidated_count = 0;
            for (index, message) in messages.iter().enumerate() {
                match message {
                    SignalStreamMessage::SignalBatch { updates, .. } => {
                        for update in updates {
                            if update.signal_id == id
                                && departed_gen1(&update.scope)
                                && update.quality.state == signals_core::QualityState::Ready
                                && update.values.get("settlement_window_complete")
                                    == Some(&json!(true))
                            {
                                final_ready_at.get_or_insert(index);
                            }
                        }
                    }
                    SignalStreamMessage::InstanceStatus {
                        signal_id,
                        scope,
                        state,
                        ..
                    } if signal_id == id
                        && departed_gen1(scope)
                        && *state == signals_core::InstanceStatusState::Invalidated =>
                    {
                        invalidated_at.get_or_insert(index);
                        invalidated_count += 1;
                    }
                    _ => {}
                }
            }
            let final_ready_at = final_ready_at.unwrap_or_else(|| {
                panic!("{id}: the departed final ready publishes after the restart")
            });
            let invalidated_at = invalidated_at.unwrap_or_else(|| {
                panic!("{id}: the departed identity is invalidated after release post-restart")
            });
            assert_eq!(
                invalidated_count, 1,
                "{id}: the departed identity is invalidated exactly once after the restart"
            );
            assert!(
                final_ready_at < invalidated_at,
                "{id}: ready(final) precedes invalidated after the restart (never invalidated -> ready)"
            );
        }
    }

    /// E37d — the completing settlement frame the dedicated index socket keeps
    /// delivering after the roll reaches the E37c slot even while the lane is
    /// UNBOUND (no successor bound yet). The persistent index socket is why the
    /// frame exists to admit at all; here it arrives 130 ms after the roll, under
    /// the next reserved generation (auxiliary, no market binding), and must
    /// release the departed market's final decision without expiring the slot.
    #[test]
    fn e37d_completing_frame_while_unbound_releases_the_departed_final_decision() {
        use crate::{KALSHI_ORACLE_DIVERGENCE_ID, KALSHI_SETTLEMENT_ANCHOR_STATE_ID};
        const MS: u64 = 1_000_000;
        let sink = SharedSink(Arc::new(std::sync::Mutex::new(Vec::new())));
        let mut pipeline = armed_kalshi_pipeline(sink.clone());
        // Warm the composite spot reference so the divergence twin has a mid to
        // freeze, then a ready anchor one second before the close with the window
        // still incomplete (as_of 899 != close 900).
        for (seq, second) in [(1_u64, 897_u64), (2, 898)] {
            pipeline
                .admit_replay(PredictionJournalInput::SpotEvents {
                    admission_seq: seq,
                    events: Arc::new(vec![
                        coinbase_ticker(seq, "60000", second),
                        kraken_ticker(seq, "60000", second),
                    ]),
                    logical_receipt_ns: second * SECOND_NS,
                    now_ns: second * SECOND_NS,
                })
                .expect("spot warms");
        }
        pipeline
            .admit_replay(PredictionJournalInput::MarketEvents {
                admission_seq: 3,
                generation: 1,
                events: Arc::new(vec![
                    cf_value(1, 899, "60200.5", "60150.25000000", 60),
                    cf_settlement(2, 899, 900, 899, 59),
                ]),
                logical_receipt_ns: 899 * SECOND_NS,
                now_ns: 899 * SECOND_NS,
            })
            .expect("pre-close anchor admits, window incomplete");
        // The scheduled rollover fires AT the close, freezing the E37c slot. No
        // successor binds: the lane stays unbound.
        pipeline
            .admit_replay(PredictionJournalInput::Unavailable {
                admission_seq: 4,
                generation: 2,
                previous_market_id: Some("KXBTC15M-1".to_owned()),
                reason: MarketTransitionReason::ScheduledRollover,
                now_ns: 900 * SECOND_NS,
            })
            .expect("rollover admits and freezes the pending final decision");
        // 130 ms after the roll, still unbound, the completing frame for the
        // DEPARTED quarter arrives on the persistent index socket. It is
        // auxiliary, so it takes the next reserved generation (2) rather than
        // requiring a bound market.
        pipeline
            .admit_replay(PredictionJournalInput::MarketEvents {
                admission_seq: 5,
                generation: 2,
                events: Arc::new(vec![cf_settlement(3, 900, 900, 900, 60)]),
                logical_receipt_ns: 900 * SECOND_NS + 130 * MS,
                now_ns: 900 * SECOND_NS + 130 * MS,
            })
            .expect("the completing frame admits while unbound without a scope refusal");

        // The slot released rather than expired.
        assert_eq!(
            pipeline.feed_health.snapshot().final_decisions_expired,
            0,
            "the completing frame released the slot; nothing expired"
        );

        drop(pipeline);
        let bytes = sink.0.lock().expect("sink lock").clone();
        let departed_gen1 = |scope: &SignalScope| {
            matches!(
                scope,
                SignalScope::Market { market_id, market_generation }
                    if market_id == "KXBTC15M-1" && *market_generation == 1
            )
        };
        // Exactly one departed final ready decision per twin, scoped to the
        // departed market, complete, and as-of the departed quarter close.
        for id in [
            KALSHI_SETTLEMENT_ANCHOR_STATE_ID,
            KALSHI_ORACLE_DIVERGENCE_ID,
        ] {
            let finals: Vec<SignalUpdate> = batch_updates(&bytes)
                .into_iter()
                .filter(|update| {
                    update.signal_id == id
                        && departed_gen1(&update.scope)
                        && update.values.get("settlement_window_complete") == Some(&json!(true))
                })
                .collect();
            assert_eq!(
                finals.len(),
                1,
                "{id}: exactly one departed final ready decision releases while unbound"
            );
            assert_eq!(
                finals[0].quality.state,
                signals_core::QualityState::Ready,
                "{id}: the departed final decision is ready"
            );
            assert_eq!(
                finals[0].values.get("window_as_of").and_then(Value::as_str),
                Some(timestamp(900).as_str()),
                "{id}: window_as_of is the departed quarter close"
            );
        }
        // The deferred settlement-twin invalidation follows the release, exactly
        // once, and never precedes a ready.
        let messages = decode(&bytes);
        for id in [
            KALSHI_SETTLEMENT_ANCHOR_STATE_ID,
            KALSHI_ORACLE_DIVERGENCE_ID,
        ] {
            let mut final_ready_at = None;
            let mut invalidated_at = None;
            let mut invalidated_count = 0;
            for (index, message) in messages.iter().enumerate() {
                match message {
                    SignalStreamMessage::SignalBatch { updates, .. } => {
                        for update in updates {
                            if update.signal_id == id
                                && departed_gen1(&update.scope)
                                && update.quality.state == signals_core::QualityState::Ready
                                && update.values.get("settlement_window_complete")
                                    == Some(&json!(true))
                            {
                                final_ready_at.get_or_insert(index);
                            }
                        }
                    }
                    SignalStreamMessage::InstanceStatus {
                        signal_id,
                        scope,
                        state,
                        ..
                    } if signal_id == id
                        && departed_gen1(scope)
                        && *state == signals_core::InstanceStatusState::Invalidated =>
                    {
                        invalidated_at.get_or_insert(index);
                        invalidated_count += 1;
                    }
                    _ => {}
                }
            }
            let final_ready_at = final_ready_at
                .unwrap_or_else(|| panic!("{id}: the departed final ready publishes"));
            let invalidated_at = invalidated_at.unwrap_or_else(|| {
                panic!("{id}: the departed identity is invalidated after release")
            });
            assert_eq!(
                invalidated_count, 1,
                "{id}: the departed identity is invalidated exactly once"
            );
            assert!(
                final_ready_at < invalidated_at,
                "{id}: ready(final) precedes invalidated (never invalidated -> ready)"
            );
        }
    }

    /// E37d — the index socket now holds its own connection, so a market-socket
    /// disconnect (the `kalshi` transport) must leave the index-dependent
    /// instances untouched; only an index-socket disconnect (`kalshi_index`)
    /// withdraws them under `cfbrti_disconnected`. Two runs: the market-only
    /// disconnect never disconnects the oracle reference; the index disconnect
    /// (the control) does.
    #[test]
    fn e37d_market_socket_disconnect_does_not_touch_the_index_socket() {
        use crate::CFBRTI_ORACLE_REFERENCE_ID;
        // A CF socket drop withdraws the global oracle reference as an
        // InstanceStatus(Unavailable) carrying `cfbrti_disconnected`, not as a
        // degraded batch update, so the detector inspects the status stream.
        let reference_disconnected = |bytes: &[u8]| -> bool {
            decode(bytes).into_iter().any(|message| {
                matches!(
                    message,
                    SignalStreamMessage::InstanceStatus {
                        signal_id,
                        reasons,
                        ..
                    } if signal_id == CFBRTI_ORACLE_REFERENCE_ID
                        && reasons.iter().any(|reason| reason == "cfbrti_disconnected")
                )
            })
        };
        let warm = |pipeline: &mut PredictionPipeline<SharedSink>| {
            pipeline
                .admit_replay(PredictionJournalInput::MarketEvents {
                    admission_seq: 1,
                    generation: 1,
                    events: Arc::new(vec![cf_value(1, 120, "60200.5", "60150.25000000", 60)]),
                    logical_receipt_ns: 120 * SECOND_NS,
                    now_ns: 120 * SECOND_NS,
                })
                .expect("cf value warms the anchor");
            pipeline
                .admit_replay(PredictionJournalInput::HealthTick {
                    admission_seq: 2,
                    now_ns: 121 * SECOND_NS,
                })
                .expect("health tick publishes the ready oracle reference");
        };

        // Run A: only the market socket drops. The index socket is a separate
        // connection, so the oracle reference must never go cfbrti_disconnected.
        let sink_a = SharedSink(Arc::new(std::sync::Mutex::new(Vec::new())));
        let mut pipeline_a = armed_kalshi_pipeline(sink_a.clone());
        warm(&mut pipeline_a);
        pipeline_a
            .admit_replay(PredictionJournalInput::Connectivity {
                admission_seq: 3,
                transport: PredictionTransport::Kalshi,
                transition: PredictionSocketTransition::Disconnected,
                now_ns: 122 * SECOND_NS,
            })
            .expect("a market-socket disconnect admits");
        pipeline_a
            .admit_replay(PredictionJournalInput::HealthTick {
                admission_seq: 4,
                now_ns: 123 * SECOND_NS,
            })
            .expect("post-disconnect health tick admits");
        drop(pipeline_a);
        assert!(
            !reference_disconnected(&sink_a.0.lock().expect("sink lock")),
            "a market-socket disconnect must not disconnect the index-fed oracle reference"
        );

        // Run B (control): the index socket drops. The oracle reference must go
        // cfbrti_disconnected, proving the mechanism is live and Run A's silence
        // is decoupling, not a dead path.
        let sink_b = SharedSink(Arc::new(std::sync::Mutex::new(Vec::new())));
        let mut pipeline_b = armed_kalshi_pipeline(sink_b.clone());
        warm(&mut pipeline_b);
        pipeline_b
            .admit_replay(PredictionJournalInput::Connectivity {
                admission_seq: 3,
                transport: PredictionTransport::KalshiIndex,
                transition: PredictionSocketTransition::Disconnected,
                now_ns: 122 * SECOND_NS,
            })
            .expect("an index-socket disconnect admits");
        pipeline_b
            .admit_replay(PredictionJournalInput::HealthTick {
                admission_seq: 4,
                now_ns: 123 * SECOND_NS,
            })
            .expect("post-disconnect health tick admits");
        drop(pipeline_b);
        assert!(
            reference_disconnected(&sink_b.0.lock().expect("sink lock")),
            "an index-socket disconnect must disconnect the index-fed oracle reference"
        );
    }
}
