//! The venue lane shell: one runtime for every lane, driven by a pluggable
//! [`LaneProcessor`] (A6, ruling E24/E26).
//!
//! Carries the CLI-independent lifecycle the spot and prediction runners each
//! hand-rolled — admit the lane, open journal + checkpoint, spawn one worker per
//! required stream (or the `http_poll` driver for the oracle lane), join them
//! with the health/checkpoint maintenance timer and the shutdown coordinator,
//! then close durably in the fixed order. Spot and prediction are untouched; this
//! serves the `binance`, `bybit` and `oracle` lanes.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::future::Future;
use std::io::{BufWriter, Write};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use btc_feeds::FeedNoticeCode;
use btc_feeds::live::{
    LiveControl, LiveDiagnostics, LiveError, LiveOptions, LiveReport, LiveShutdown,
    LiveShutdownReason, ProcessClock, TimedIngestOutcome, coordinate_live_shutdown,
    live_shutdown_channel, run_live,
};
use chrono::{DateTime, Utc};
use serde::Serialize;
use signals_core::{SignalInstanceKey, SignalUpdate};
use tokio::time::{Instant, interval_at, sleep_until};

use btc_feeds::PythHermesCredential;
use signals_core::EnabledVenues;

use crate::exit_code::{ClassifiedMessage, ExitClass};
use crate::feed_health::{FEED_HEALTH_PERIODIC_INTERVAL_NS, FeedHealthSnapshot};
use crate::file_generation::{
    FileGenerationError, FileGenerationLayout, OpenFileGeneration, RotationSchedule, RunMeta,
};
use crate::lane::{
    LaneClock, LaneDriverSpec, LaneProcessor, LaneProcessorError, LaneSpec, PollDriver,
};
use crate::lane_checkpoint::{
    LaneCheckpointDocument, LaneCheckpointError, LaneCheckpointWriteReport, load_lane_checkpoint,
    write_lane_checkpoint,
};
use crate::lane_journal::{
    LaneContractFingerprint, LaneIdentity, LaneJournalEnd, LaneJournalError, LaneJournalInput,
    LaneJournalReplayReader, LaneJournalSession, LaneJournalTotals, LaneJournalWriteReport,
    LaneJournalWriter, LaneRuntimeConfig,
};
use crate::stream_accounting::{
    InstanceLedgerEntry, InvalidatedTombstone, ProducerHealth, StreamAccounting,
    StreamAccountingError, StreamHook, stream_timestamp,
};

/// The bounded checkpoint context a lane pipeline stamps into each checkpoint.
pub struct LaneCheckpointContext {
    pub path: PathBuf,
    pub maximum_bytes: u64,
    pub processor_id: String,
    pub processor_version: String,
    pub session_id: String,
    pub wall_start: String,
    pub contract_fingerprint: LaneContractFingerprint,
    pub runtime_config: LaneRuntimeConfig,
    pub publisher_epoch_lineage: Vec<String>,
}

/// The lane pipeline: applies admitted inputs to the processor and frames the
/// stream-v3 output. Feed-only lanes publish no `signal_batch`, only
/// `feed_health` / `heartbeat` / `status` (and `reset`).
pub struct LanePipeline<W> {
    processor: Box<dyn LaneProcessor>,
    publisher_epoch: String,
    wall_start: DateTime<Utc>,
    writer: W,
    journal: Option<LaneJournalWriter>,
    checkpoint_context: Option<LaneCheckpointContext>,
    maximum_updates: u64,
    maximum_bytes: u64,
    feed_health_stream: bool,
    /// Whether this pipeline replays (or writes) a rotation-opened generation (C5).
    /// A rotated generation's rings were warm live but re-warm cold on an independent
    /// replay, so [`Self::finish_replay`] treats the terminal stream-message-count
    /// as warm-vs-cold and reports the difference as the re-warm gap instead of
    /// failing. False for generation 0, explicit runs and crash-restart resumes.
    opened_by_rotation: bool,

    next_admission_seq: u64,
    event_batches: u64,
    connectivity_transitions: u64,
    health_ticks: u64,

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
    feed_health_written: u64,
    last_feed_health_revision: Option<u64>,
    next_feed_health_ns: Option<u64>,
    /// The last durable checkpoint write report (initial, interval, or final), so
    /// the abort path can report the resume point without re-writing.
    last_checkpoint: Option<LaneCheckpointWriteReport>,
    /// In-process observer of every written stream message; survives
    /// generation rotation (it belongs to the process, not the file).
    stream_hook: Option<StreamHook>,
    scratch: Vec<u8>,
}

impl<W: Write> LanePipeline<W> {
    #[allow(clippy::too_many_arguments)]
    fn new(
        processor: Box<dyn LaneProcessor>,
        publisher_epoch: String,
        wall_start: DateTime<Utc>,
        writer: W,
        journal: Option<LaneJournalWriter>,
        checkpoint_context: Option<LaneCheckpointContext>,
        maximum_updates: u64,
        maximum_bytes: u64,
        feed_health_stream: bool,
        next_admission_seq: u64,
    ) -> Self {
        // Register each published identity's declared interval from the
        // processor's signal definitions, so the shell's `instance_status`
        // periodic logic (stream_accounting) has an interval for every signal the
        // processor can emit. Feed-only processors return no signal documents, so
        // the map stays empty.
        let signal_intervals_ms = processor
            .signal_definitions()
            .iter()
            .filter_map(|(_, document)| {
                serde_json::from_str::<signals_core::SignalDefinition>(document).ok()
            })
            .map(|definition| {
                (
                    (definition.signal_id, definition.version),
                    definition.approximate_interval_ms,
                )
            })
            .collect();
        Self {
            processor,
            publisher_epoch,
            wall_start,
            writer,
            journal,
            checkpoint_context,
            maximum_updates,
            maximum_bytes,
            feed_health_stream,
            opened_by_rotation: false,
            next_admission_seq,
            event_batches: 0,
            connectivity_transitions: 0,
            health_ticks: 0,
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
            feed_health_written: 0,
            last_feed_health_revision: None,
            next_feed_health_ns: None,
            last_checkpoint: None,
            stream_hook: None,
            scratch: Vec::with_capacity(1024),
        }
    }

    /// Install an in-process observer of every written stream message
    /// ([`StreamHook`]). The file output is unchanged.
    pub fn with_stream_hook(mut self, hook: Option<StreamHook>) -> Self {
        self.stream_hook = hook;
        self
    }

    #[allow(clippy::too_many_arguments)]
    pub fn live(
        writer: W,
        processor: Box<dyn LaneProcessor>,
        session: &LaneJournalSession,
        journal: LaneJournalWriter,
        checkpoint_context: LaneCheckpointContext,
        maximum_updates: u64,
        maximum_bytes: u64,
        initial_checkpoint: LaneCheckpointWriteReport,
    ) -> Self {
        let next_admission_seq = journal.next_admission_seq();
        let mut pipeline = Self::new(
            processor,
            session.publisher_epoch.clone(),
            parse_wall_start(&session.wall_start),
            writer,
            Some(journal),
            Some(checkpoint_context),
            maximum_updates,
            maximum_bytes,
            session.runtime_config.feed_health_stream,
            next_admission_seq,
        );
        pipeline.opened_by_rotation = session.opened_by_rotation;
        pipeline.last_checkpoint = Some(initial_checkpoint);
        pipeline
    }

    pub fn replay(
        writer: W,
        processor: Box<dyn LaneProcessor>,
        session: &LaneJournalSession,
        maximum_updates: u64,
        maximum_bytes: u64,
    ) -> Self {
        let next_admission_seq = session
            .initial_checkpoint
            .as_ref()
            .map_or(0, |checkpoint| checkpoint.next_admission_seq);
        let mut pipeline = Self::new(
            processor,
            session.publisher_epoch.clone(),
            parse_wall_start(&session.wall_start),
            writer,
            None,
            None,
            maximum_updates,
            maximum_bytes,
            session.runtime_config.feed_health_stream,
            next_admission_seq,
        );
        pipeline.opened_by_rotation = session.opened_by_rotation;
        pipeline
    }

    pub fn next_admission_seq(&self) -> u64 {
        self.next_admission_seq
    }

    pub fn admit_live(&mut self, input: LaneJournalInput) -> Result<(), LanePipelineError> {
        let journal = self
            .journal
            .as_mut()
            .ok_or_else(|| LanePipelineError::Mode("live input requires a journal".to_owned()))?;
        journal.enqueue(input.clone())?;
        self.apply(input)
    }

    pub fn admit_replay(&mut self, input: LaneJournalInput) -> Result<(), LanePipelineError> {
        if self.journal.is_some() {
            return Err(LanePipelineError::Mode(
                "replay input cannot be applied to a live pipeline".to_owned(),
            ));
        }
        self.apply(input)
    }

    /// Admit the processor's C5 rotation resync inputs as the FIRST records of a
    /// rotation-opened generation's journal (decision 23:30Z, amendment 02:39Z). Each
    /// batch is one book source's complete snapshot rebuilt from the warm in-memory
    /// book, admitted through the normal live path but tagged `resync` at the
    /// journal-record level: on this warm process the rows are ordering-skipped and
    /// change no output, while a cold replay of the journal rebuilds the book the
    /// sealed checkpoint does not carry, so the generation's next real delta is
    /// contiguous. A processor with no delta-reconstructed book (Binance full-image,
    /// oracle) returns nothing and no resync record is written. Must run after
    /// [`Self::begin_generation`] and before any other input of the new generation;
    /// the resync uses the continuing admission sequence (not reset by a rotation).
    fn admit_resync_generation(&mut self, now_ns: u64) -> Result<(), LanePipelineError> {
        for events in self.processor.resync_inputs() {
            if events.is_empty() {
                continue;
            }
            // Each batch is a single source's already-ordered snapshot group; its own
            // rows carry the source id, so the batch stream label is that source.
            let stream = events[0].source_id.clone();
            let admission_seq = self.next_admission_seq;
            self.admit_live(LaneJournalInput::Events {
                admission_seq,
                stream,
                events: Arc::new(events),
                logical_receipt_ns: now_ns,
                now_ns,
                resync: true,
            })?;
        }
        Ok(())
    }

    fn apply(&mut self, input: LaneJournalInput) -> Result<(), LanePipelineError> {
        if input.admission_seq() != self.next_admission_seq {
            return Err(LanePipelineError::AdmissionSequence {
                expected: self.next_admission_seq,
                actual: input.admission_seq(),
            });
        }
        let now_ns = input.now_ns();
        self.next_admission_seq = self
            .next_admission_seq
            .checked_add(1)
            .ok_or(LanePipelineError::Counter)?;
        let bytes_before = self.bytes_written;
        self.ensure_stream_started(now_ns)?;
        // The shell owns both clocks: the journaled logical `now_ns` and the wall
        // `produced_at` it derives from the session `wall_start`. Both are pure
        // functions of journaled state, so a replay reproduces every timestamp the
        // processor stamps (ADR 0027 amendment).
        let clock = LaneClock {
            now_ns,
            produced_at: stream_timestamp(&self.wall_start, now_ns)?,
        };
        let mut updates = match input {
            LaneJournalInput::Events {
                stream,
                events,
                resync,
                ..
            } => {
                self.event_batches = self
                    .event_batches
                    .checked_add(1)
                    .ok_or(LanePipelineError::Counter)?;
                if resync {
                    // A C5 rotation resync: rebuild the book ONLY, via the dedicated
                    // path that never touches venue aliveness / feed-health / skew.
                    // Applied identically on the warm live process (ordering-skipped)
                    // and on a cold replay (the `resync` flag routes it here too).
                    self.processor.on_resync_events(&events, &clock)?
                } else {
                    self.processor.on_events(&stream, &events, &clock)?
                }
            }
            LaneJournalInput::Connectivity {
                stream, connected, ..
            } => {
                self.connectivity_transitions = self
                    .connectivity_transitions
                    .checked_add(1)
                    .ok_or(LanePipelineError::Counter)?;
                self.processor.on_connectivity(&stream, connected, &clock)?
            }
            LaneJournalInput::HealthTick { .. } => {
                self.health_ticks = self
                    .health_ticks
                    .checked_add(1)
                    .ok_or(LanePipelineError::Counter)?;
                self.processor.on_interval(&clock)?
            }
            LaneJournalInput::Notice {
                stream,
                code,
                detail,
                now_ns,
                ..
            } => {
                // Journaled as evidence and counted in `admitted_inputs`; a
                // feed-only processor produces nothing, so no output changes and
                // replay stays byte-identical (M1).
                self.processor.on_notice(&stream, &code, &detail, now_ns)?
            }
        };
        if !updates.is_empty() {
            // Emitted frames carry this generation's output epoch. The processor
            // stamps each update with the epoch it was built with (the process
            // epoch, kept warm across a C5 rotation); the pipeline owns the output
            // epoch, which changes per generation, so it re-stamps every update to
            // match the envelope before writing. Stream accounting requires each
            // update's `publisher_epoch` to equal the enclosing batch's, and this
            // keeps a rotated generation's frames internally consistent. In an
            // unrotated run the two epochs are already equal, so this is a no-op.
            for update in &mut updates {
                update.publisher_epoch.clone_from(&self.publisher_epoch);
            }
            self.write_signal_batch(now_ns, updates)?;
        }
        self.maybe_write_status(now_ns)?;
        self.maybe_write_snapshot(now_ns)?;
        self.maybe_write_heartbeat(now_ns)?;
        self.maybe_write_feed_health(now_ns)?;
        if self.bytes_written != bytes_before {
            self.writer.flush()?;
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
            feed_health_written: Some(&mut self.feed_health_written),
        }
    }

    fn ensure_stream_started(&mut self, now_ns: u64) -> Result<(), LanePipelineError> {
        self.stream_accounting().ensure_stream_started(now_ns)?;
        Ok(())
    }

    fn write_signal_batch(
        &mut self,
        now_ns: u64,
        updates: Vec<SignalUpdate>,
    ) -> Result<(), LanePipelineError> {
        self.stream_accounting()
            .write_signal_batch(now_ns, updates)?;
        Ok(())
    }

    fn maybe_write_status(&mut self, now_ns: u64) -> Result<(), LanePipelineError> {
        let health = self.processor.producer_health();
        self.stream_accounting()
            .maybe_write_status_health(now_ns, health)?;
        Ok(())
    }

    fn maybe_write_snapshot(&mut self, now_ns: u64) -> Result<(), LanePipelineError> {
        self.stream_accounting().maybe_write_snapshot(now_ns)?;
        Ok(())
    }

    fn maybe_write_heartbeat(&mut self, now_ns: u64) -> Result<(), LanePipelineError> {
        self.stream_accounting().maybe_write_heartbeat(now_ns)?;
        Ok(())
    }

    fn maybe_write_feed_health(&mut self, now_ns: u64) -> Result<(), LanePipelineError> {
        if !self.feed_health_stream {
            return Ok(());
        }
        let revision = self.processor.feed_health_emission_revision();
        let changed = self.last_feed_health_revision != Some(revision);
        let periodic_due = self
            .next_feed_health_ns
            .is_none_or(|deadline| now_ns >= deadline);
        if !changed && !periodic_due {
            return Ok(());
        }
        let snapshot = self.processor.feed_health_snapshot();
        self.stream_accounting()
            .write_feed_health(now_ns, &snapshot)?;
        self.last_feed_health_revision = Some(revision);
        self.next_feed_health_ns = Some(now_ns.saturating_add(FEED_HEALTH_PERIODIC_INTERVAL_NS));
        Ok(())
    }

    fn checkpoint_document(
        &self,
        now_ns: u64,
    ) -> Result<LaneCheckpointDocument, LanePipelineError> {
        let context = self.checkpoint_context.as_ref().ok_or_else(|| {
            LanePipelineError::Mode("live checkpoint context is unavailable".to_owned())
        })?;
        LaneCheckpointDocument::new(
            context.processor_id.clone(),
            context.processor_version.clone(),
            context.session_id.clone(),
            context.wall_start.clone(),
            now_ns,
            self.next_admission_seq,
            context.contract_fingerprint.clone(),
            context.runtime_config.clone(),
            self.processor.checkpoint_state(),
            context.publisher_epoch_lineage.clone(),
        )
        .map_err(LanePipelineError::Checkpoint)
    }

    pub fn checkpoint_live(&mut self, now_ns: u64) -> Result<(), LanePipelineError> {
        let document = self.checkpoint_document(now_ns)?;
        let context = self.checkpoint_context.as_ref().ok_or_else(|| {
            LanePipelineError::Mode("live checkpoint context is unavailable".to_owned())
        })?;
        let report = write_lane_checkpoint(&context.path, document, context.maximum_bytes, true)?;
        self.last_checkpoint = Some(report);
        Ok(())
    }

    fn journal_end(&self) -> LaneJournalEnd {
        LaneJournalEnd {
            totals: LaneJournalTotals {
                admitted_inputs: self.next_admission_seq,
                event_batches: self.event_batches,
                connectivity_transitions: self.connectivity_transitions,
                health_ticks: self.health_ticks,
                stream_messages_written: self.stream_messages_written,
            },
        }
    }

    pub fn report(&self) -> LanePipelineReport {
        LanePipelineReport {
            admitted_inputs: self.next_admission_seq,
            event_batches: self.event_batches,
            connectivity_transitions: self.connectivity_transitions,
            health_ticks: self.health_ticks,
            updates_written: self.updates_written,
            stream_messages_written: self.stream_messages_written,
            signal_batches_written: self.signal_batches_written,
            snapshots_written: self.snapshots_written,
            heartbeats_written: self.heartbeats_written,
            status_messages_written: self.status_messages_written,
            reset_messages_written: self.reset_messages_written,
            feed_health_messages_written: self.feed_health_written,
            bytes_written: self.bytes_written,
            rewarm_stream_message_gap: 0,
            feed_health: self.processor.feed_health_snapshot(),
        }
    }

    /// Complete the journal with its terminal end record.
    pub fn complete_journal(&mut self) -> Result<LaneJournalWriteReport, LanePipelineError> {
        let end = self.journal_end();
        self.journal
            .take()
            .ok_or_else(|| LanePipelineError::Mode("live journal is unavailable".to_owned()))?
            .finish(end)
            .map_err(LanePipelineError::Journal)
    }

    /// Abort the journal: drain and sync the recoverable prefix without a terminal
    /// record, so a failed run's journal is honestly marked incomplete.
    pub fn abort_journal(&mut self) -> Result<LaneJournalWriteReport, LanePipelineError> {
        self.journal
            .take()
            .ok_or_else(|| LanePipelineError::Mode("live journal is unavailable".to_owned()))?
            .abort()
            .map_err(LanePipelineError::Journal)
    }

    /// Write the final checkpoint and record it as the durable resume point.
    pub fn complete_checkpoint(
        &mut self,
        now_ns: u64,
    ) -> Result<LaneCheckpointWriteReport, LanePipelineError> {
        let document = self.checkpoint_document(now_ns)?;
        let context = self.checkpoint_context.as_ref().ok_or_else(|| {
            LanePipelineError::Mode("live checkpoint context is unavailable".to_owned())
        })?;
        let report = write_lane_checkpoint(&context.path, document, context.maximum_bytes, true)
            .map_err(LanePipelineError::Checkpoint)?;
        self.last_checkpoint = Some(report.clone());
        Ok(report)
    }

    /// Abort the checkpoint on a failed run: no new checkpoint is written, so the
    /// last durable checkpoint (initial or interval) stays the resume point. This
    /// mirrors the spot reference's `abort_checkpoint` (`main.rs:1318`) — there is
    /// no background writer to drain here, so it reports the last durable state so
    /// the close stays symmetric with the journal's complete/abort.
    pub fn abort_checkpoint(&mut self) -> Result<LaneCheckpointWriteReport, LanePipelineError> {
        self.last_checkpoint.clone().ok_or_else(|| {
            LanePipelineError::Mode("no durable lane checkpoint to abort onto".to_owned())
        })
    }

    /// Complete a replay: verify against the journal terminal and return the report.
    ///
    /// For a rotation-opened generation (C5) the ONLY thing allowed to differ between
    /// this replay and the live generation is the OUTPUT stream-message count: the
    /// live rings were warm while this cold replay re-warms them from a checkpoint that
    /// carries no ring state, so during the re-warm window the identities emit a
    /// different mix of `signal_batch` and `instance_status` frames (fewer Ready
    /// readings, and for a book that does not re-warm in this generation, extra
    /// warming/degraded status) — the difference can fall EITHER side of the live
    /// count. It is the documented re-warm gap (decision 23:30Z; C5b closes it):
    /// reported as `rewarm_stream_message_gap`, never fatal.
    ///
    /// "Never a missing decision" is enforced instead on the INPUT-driven totals
    /// (`admitted_inputs`, `event_batches`, `connectivity_transitions`,
    /// `health_ticks`) — each a pure function of the journal's own inputs, so a replay
    /// that applied every record reproduces them exactly. Requiring them to equal the
    /// terminal proves every input and every decision interval the live run took was
    /// replayed; only the warm-vs-cold output count is tolerated to move. Generation 0,
    /// explicit runs and crash-restart resumes keep the strict byte-for-byte parity on
    /// the stream-message count too.
    pub fn finish_replay(
        self,
        end: &LaneJournalEnd,
    ) -> Result<LanePipelineReport, LanePipelineError> {
        let mut report = self.report();
        let terminal = end.totals.stream_messages_written;
        let replayed = report.stream_messages_written;
        if self.opened_by_rotation {
            // The input-driven totals must match exactly: a rotation re-warm may move
            // the output count but can never drop or add an admitted input or a
            // decision interval.
            for (name, replayed_total, terminal_total) in [
                (
                    "admitted_inputs",
                    report.admitted_inputs,
                    end.totals.admitted_inputs,
                ),
                (
                    "event_batches",
                    report.event_batches,
                    end.totals.event_batches,
                ),
                (
                    "connectivity_transitions",
                    report.connectivity_transitions,
                    end.totals.connectivity_transitions,
                ),
                ("health_ticks", report.health_ticks, end.totals.health_ticks),
            ] {
                if replayed_total != terminal_total {
                    return Err(LanePipelineError::Mode(format!(
                        "rotated-generation replay {name} {replayed_total} disagrees with the journal terminal {terminal_total}: an input or decision went missing, not a warm-vs-cold re-warm"
                    )));
                }
            }
            report.rewarm_stream_message_gap = replayed.abs_diff(terminal);
        } else if terminal != replayed {
            return Err(LanePipelineError::Mode(format!(
                "replay stream-message count {replayed} disagrees with the journal terminal {terminal}"
            )));
        }
        Ok(report)
    }
}

impl LanePipeline<BufWriter<File>> {
    pub fn sync_output(&mut self) -> Result<(), LanePipelineError> {
        self.writer.flush()?;
        self.writer.get_ref().sync_data()?;
        Ok(())
    }

    /// Seal the current file generation (C5 rotation): the same graceful-end
    /// sequence a clean shutdown runs — `sync_output` → complete the journal → an
    /// interval-style checkpoint at `now_ns` — but without dropping the processor.
    /// Returns the sealed checkpoint document (so the next generation's journal can
    /// resume from it) and its write report (which becomes the next generation's
    /// initial durable checkpoint). Only `self.journal` is consumed; the processor,
    /// its rings, the clock and the admission epoch stay in memory for the next
    /// generation.
    fn seal_generation(
        &mut self,
        now_ns: u64,
    ) -> Result<(LaneCheckpointDocument, LaneCheckpointWriteReport), LanePipelineError> {
        self.sync_output()?;
        self.complete_journal()?;
        let document = self.checkpoint_document(now_ns)?;
        let context = self.checkpoint_context.as_ref().ok_or_else(|| {
            LanePipelineError::Mode("live checkpoint context is unavailable".to_owned())
        })?;
        let report =
            write_lane_checkpoint(&context.path, document.clone(), context.maximum_bytes, true)
                .map_err(LanePipelineError::Checkpoint)?;
        self.last_checkpoint = Some(report.clone());
        Ok((document, report))
    }

    /// Install fresh output and journal writers for the next file generation and
    /// reset every per-generation counter and stream-accounting field to exactly
    /// the state a fresh process would start with — `next_stream_sequence` at 0,
    /// no producer health (so the next admitted input re-emits the `reset` at
    /// sequence 0 through the existing path), cleared ledgers and deadlines, zeroed
    /// byte/update/message counters, and the output epoch swapped to the new
    /// generation's. The processor, its rings, the wall/process clock and
    /// `next_admission_seq` are deliberately left untouched: the admission side
    /// continues, so replay rebuilds the same processor state from the sealed
    /// checkpoint while the output stream resets like a restart.
    fn begin_generation(
        &mut self,
        writer: BufWriter<File>,
        journal: LaneJournalWriter,
        session: &LaneJournalSession,
        checkpoint_context: LaneCheckpointContext,
        initial_checkpoint: LaneCheckpointWriteReport,
    ) {
        self.writer = writer;
        self.journal = Some(journal);
        self.publisher_epoch = session.publisher_epoch.clone();
        self.checkpoint_context = Some(checkpoint_context);
        self.last_checkpoint = Some(initial_checkpoint);
        // C5 Design B: this generation is a fresh output epoch, so every per-identity
        // decision sequence restarts at zero on the warm processor the rotation
        // carries across the boundary — the same reset an independent replay of this
        // generation performs when it restores the sealed checkpoint. A feed-only
        // lane's default is a no-op. The warm rings/books/feature state are untouched.
        self.processor.reset_output_sequences();
        // Per-generation stream accounting, reset as a fresh process would.
        self.next_stream_sequence = 0;
        self.last_signal_batch_sequence = None;
        self.last_decision_or_status_sequence = None;
        self.latest_updates.clear();
        self.instance_ledger.clear();
        self.invalidated_tombstones.clear();
        self.producer_health = None;
        self.next_heartbeat_ns = None;
        self.next_snapshot_ns = None;
        self.next_feed_health_ns = None;
        self.last_feed_health_revision = None;
        // Per-generation byte/update/message counters.
        self.updates_written = 0;
        self.bytes_written = 0;
        self.stream_messages_written = 0;
        self.signal_batches_written = 0;
        self.snapshots_written = 0;
        self.heartbeats_written = 0;
        self.status_messages_written = 0;
        self.reset_messages_written = 0;
        self.feed_health_written = 0;
        // Per-generation journal-terminal totals, so each generation's journal
        // terminal counts only its own inputs (replay of that generation, which
        // starts these at 0, reproduces the same terminal). `next_admission_seq`
        // is NOT reset — it continues via the resumed session's initial checkpoint.
        self.event_batches = 0;
        self.connectivity_transitions = 0;
        self.health_ticks = 0;
    }
}

fn parse_wall_start(text: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(text)
        .expect("session wall_start is validated RFC 3339")
        .with_timezone(&Utc)
}

#[derive(Debug, Serialize)]
pub struct LanePipelineReport {
    pub admitted_inputs: u64,
    pub event_batches: u64,
    pub connectivity_transitions: u64,
    pub health_ticks: u64,
    pub updates_written: u64,
    pub stream_messages_written: u64,
    pub signal_batches_written: u64,
    pub snapshots_written: u64,
    pub heartbeats_written: u64,
    pub status_messages_written: u64,
    pub reset_messages_written: u64,
    pub feed_health_messages_written: u64,
    pub bytes_written: u64,
    /// The re-warm gap of a rotation-opened generation's replay (C5): the magnitude of
    /// the difference between the stream-message count this cold replay wrote and its
    /// warm live generation's, because the sealed checkpoint carries no ring state so
    /// the replay re-warms the rings (and any book that gets no data this generation).
    /// The difference can fall either side of the live count, so this is its absolute
    /// value. Zero for generation 0, explicit runs, crash-restart resumes and any lane
    /// with no re-warm gap; C5b closes it entirely. Reported, never fatal (23:30Z).
    pub rewarm_stream_message_gap: u64,
    pub feed_health: FeedHealthSnapshot,
}

#[derive(Debug, thiserror::Error)]
pub enum LanePipelineError {
    #[error(transparent)]
    Journal(#[from] LaneJournalError),
    #[error(transparent)]
    Checkpoint(#[from] LaneCheckpointError),
    #[error(transparent)]
    Processor(#[from] LaneProcessorError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("lane pipeline admission sequence mismatch: expected {expected}, actual {actual}")]
    AdmissionSequence { expected: u64, actual: u64 },
    #[error("lane pipeline mode error: {0}")]
    Mode(String),
    #[error("lane pipeline counter exhausted")]
    Counter,
    #[error("lane stream accounting failed: {0}")]
    Stream(String),
}

impl From<StreamAccountingError> for LanePipelineError {
    fn from(error: StreamAccountingError) -> Self {
        LanePipelineError::Stream(error.to_string())
    }
}

// ---------------------------------------------------------------------------
// The lane shell run entry point.
// ---------------------------------------------------------------------------

/// Arguments for one live lane run.
pub struct LaneRunArguments {
    pub spec: &'static LaneSpec,
    pub output: PathBuf,
    pub journal: PathBuf,
    pub checkpoint: PathBuf,
    pub resume_from_checkpoint: bool,
    pub publisher_epoch: String,
    pub duration_seconds: u64,
    pub checkpoint_interval_seconds: u64,
    pub max_updates: u64,
    pub max_output_bytes: u64,
    pub max_journal_bytes: u64,
    pub max_checkpoint_bytes: u64,
    /// The oracle lane's poll driver (a fixture driver now, A5's Chainlink driver
    /// at merge). `None` for a stream-driven lane.
    pub poll_driver: Option<Box<dyn PollDriver>>,
    /// The per-stream runner for a `Streams` lane. `None` uses the production
    /// [`LiveStreamRunner`] (`run_live` over a real socket); a test injects a
    /// fixture runner to drive the N-stream shell without a network (M2).
    pub stream_runner: Option<Box<dyn StreamRunner>>,
    /// Resolved index-socket gating for a composite lane (C1/B1-B2 audit): which
    /// allow-list tokens are armed and whether the Pyth Hermes credential is
    /// present. `None` resolves it from the environment at run time; a test
    /// injects an explicit value so the arming/credential decision never reads
    /// the host env and never dials.
    pub index_gating: Option<LaneIndexGating>,
    /// When `Some`, the run rotates file generations in place (C5): it owns the
    /// `<lane-root>/gen-live-<lane>-<unix>` layout live-run.sh owns today, and at
    /// each rotation deadline seals the current generation and opens the next
    /// without exiting, keeping the processor, its rings, the process clock and the
    /// admission epoch warm. `None` is the unchanged explicit mode: `output` and
    /// `journal` are the two files written, no rotation, byte-for-byte as today.
    pub rotation: Option<LaneRotation>,
}

/// In-process generation rotation parameters (C5). Present only in rotation mode;
/// [`LaneRunArguments::output`]/`journal` are ignored when it is set (the paths
/// come from the [`FileGenerationLayout`] instead), while `checkpoint` is the
/// shared `<lane-root>/checkpoint.json` the CLI resolves.
pub struct LaneRotation {
    /// The lane root under which each `gen-live-<lane>-<unix>` directory is created.
    pub lane_root: PathBuf,
    /// The rotation period in seconds (`--rotate-seconds`), the per-generation
    /// duration; the [`RotationSchedule`] floor (1 s) still applies.
    pub rotate_seconds: u64,
    /// The headroom factor recorded in each generation's `RUN-META.json`.
    pub headroom_factor: u64,
}

/// Everything the maintenance loop needs to seal one file generation and open the
/// next in place. Built once (opening generation 0) before the run and threaded
/// into [`maintain`], which mutates `current` and `schedule` at each boundary and
/// hands the final open generation back for its closing seal.
struct RotationContext {
    layout: FileGenerationLayout,
    schedule: RotationSchedule,
    /// The monotonic instant and process-clock nanos at generation 0's open, the
    /// drift-free basis every rotation deadline is measured from.
    first_open_instant: Instant,
    first_open_ns: u64,
    rotate_seconds: u64,
    duration_seconds: u64,
    headroom_factor: u64,
    /// The process `--publisher-epoch` — the prefix every generation's output
    /// epoch extends (generation 0 uses it verbatim). The feeds stamp admitted
    /// events with this epoch throughout; only the emitted output epoch rotates.
    epoch_prefix: String,
    identity: LaneIdentity,
    runtime_config: LaneRuntimeConfig,
    checkpoint_path: PathBuf,
    max_updates: u64,
    max_output_bytes: u64,
    max_journal_bytes: u64,
    max_checkpoint_bytes: u64,
    /// The generation currently being written, sealed at the next boundary.
    current: OpenFileGeneration,
}

/// The resolved arming for a composite lane's index sockets and Hermes stream.
#[derive(Debug, Clone)]
pub struct LaneIndexGating {
    /// The venue allow-list (`SIGNALS_ENABLED_VENUES`) that arms the index-socket
    /// tokens (`cfbrti`, `pyth`).
    pub enabled: EnabledVenues,
    /// Whether the Pyth Hermes credential is present (the stream is keyed).
    pub hermes_keyed: bool,
    /// Whether the Kalshi index-socket credentials are present in the
    /// environment. The Kalshi index socket signs its handshake with
    /// `KALSHI_API_KEY_ID` + `KALSHI_PRIVATE_KEY_PATH` (what
    /// `KalshiAuthClient::from_env` reads); the oracle unit's credential file
    /// carries only `PYTH_API_KEY`, so absent these the socket must not be armed
    /// — opening it would fail the handshake and, through the shared `try_join!`,
    /// take the Chainlink poll down (Fatal 78). This is the fail-soft gate.
    pub kalshi_index_credentialed: bool,
}

impl LaneIndexGating {
    /// Resolve from the process environment: the venue allow-list, whether the
    /// Pyth Hermes credential is configured, and whether the Kalshi index-socket
    /// credentials are present. A malformed allow-list resolves to the default
    /// (disarmed) rather than aborting the lane.
    pub fn from_env() -> Self {
        let enabled = EnabledVenues::from_env()
            .unwrap_or_else(|_| EnabledVenues::parse(None).expect("the empty allow-list parses"));
        let hermes_keyed = PythHermesCredential::from_env().is_ok();
        // The passthrough client reads exactly the Kalshi handshake credentials
        // the index socket needs (`KalshiAuthClient::from_env`); building it opens
        // no socket, so `.is_ok()` is a pure presence-and-validity check.
        let kalshi_index_credentialed = btc_feeds::KalshiPassthroughClient::from_env().is_ok();
        Self {
            enabled,
            hermes_keyed,
            kalshi_index_credentialed,
        }
    }
}

/// The composite-lane streams to actually drive under a resolved gating: the
/// Kalshi index socket only when an index-socket token (`cfbrti`/`pyth`) is armed
/// AND the Kalshi handshake credentials are present, and the Pyth Hermes stream
/// only when `pyth` is armed AND its credential is present. Every other stream is
/// always driven. Pure, so the decision is
/// unit-testable without touching the process environment or dialing (B1/B2
/// audit): an omitted stream opens no socket, so an unkeyed or disarmed host
/// never dials a rights-gated endpoint and never fails the shared join.
pub fn active_composite_streams(
    streams: &[crate::lane::LaneStream],
    gating: &LaneIndexGating,
) -> Vec<crate::lane::LaneStream> {
    streams
        .iter()
        .copied()
        .filter(|stream| match stream.stream {
            // The Kalshi index socket arms only when a family token is listed AND
            // the Kalshi handshake credentials are present: arming it without them
            // fails `KalshiAuthClient::from_env` and, through the shared join,
            // takes the Chainlink poll down (Fatal 78). Absent the credentials the
            // socket is omitted and the index-dependent identities publish
            // unavailable with an honest reason.
            btc_feeds::CaptureStream::KalshiIndex => {
                (gating.enabled.is_enabled("cfbrti") || gating.enabled.is_enabled("pyth"))
                    && gating.kalshi_index_credentialed
            }
            btc_feeds::CaptureStream::PythHermesStream => {
                gating.enabled.is_enabled("pyth") && gating.hermes_keyed
            }
            _ => true,
        })
        .collect()
}

#[derive(Debug, Serialize)]
pub struct LaneRunReport {
    pub lane: String,
    pub duration_seconds: u64,
    pub elapsed_ns: u64,
    pub live_reports: Vec<LiveReport>,
    pub poll_cycles: Option<u64>,
    pub maintenance: LaneMaintenanceReport,
    pub pipeline: LanePipelineReport,
    pub journal: LaneJournalWriteReport,
    pub checkpoint: LaneCheckpointWriteReport,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LaneMaintenanceReport {
    pub health_ticks: u64,
    pub interval_checkpoints: u64,
    pub interrupted: bool,
}

enum LaneWork {
    Streams(Vec<LiveReport>),
    Poll(u64),
    /// A composite lane's poll cycles and its stream workers' reports, produced
    /// by driving both concurrently on the shared pipeline.
    Composite {
        reports: Vec<LiveReport>,
        cycles: u64,
    },
}

/// Run one live lane to its natural deadline or an operator interrupt.
pub async fn run_lane(arguments: LaneRunArguments) -> Result<(LaneRunReport, LiveShutdownReason)> {
    run_lane_with_hook(arguments, None).await
}

/// [`run_lane`] with an in-process observer of every stream message the run
/// writes ([`StreamHook`]); the hook persists across generation rotation. An
/// embedding process (the exchange) uses this to consume the lane's output
/// without tailing its files.
pub async fn run_lane_with_hook(
    arguments: LaneRunArguments,
    stream_hook: Option<StreamHook>,
) -> Result<(LaneRunReport, LiveShutdownReason)> {
    validate_arguments(&arguments)?;
    // The processor needs the session wall clock at construction (its produced-at
    // baseline until the shell-supplied `LaneClock` merge), so the clock is
    // established BEFORE the processor — on resume from the checkpoint's
    // wall_start, otherwise fresh — and the same clock then drives the run.
    let (clock, resume_checkpoint) = establish_lane_clock(&arguments)?;
    // F0/F1: the lane runs its own real processor (the B1/B2 venue processors or
    // the value-emitting oracle processor); an unknown lane is a fatal
    // misconfiguration inside `build_processor` (no feed-only fallback).
    let processor = arguments
        .spec
        .build_processor(&arguments.publisher_epoch, clock.wall_start())
        .context("build lane processor")?;
    run_lane_with_processor_and_hook(arguments, clock, resume_checkpoint, processor, stream_hook).await
}

/// Establish the lane clock and (on resume) load the checkpoint whose `wall_start`
/// the clock adopts, so the processor built from it shares the session's
/// produced-at baseline. Fresh runs start a new clock.
fn establish_lane_clock(
    arguments: &LaneRunArguments,
) -> Result<(ProcessClock, Option<LaneCheckpointDocument>)> {
    Ok(if arguments.resume_from_checkpoint {
        let checkpoint =
            load_lane_checkpoint(&arguments.checkpoint, arguments.max_checkpoint_bytes)
                .context("load lane checkpoint")?;
        let wall_start = DateTime::parse_from_rfc3339(&checkpoint.wall_start)
            .context("parse checkpoint wall_start")?
            .with_timezone(&Utc);
        let clock = ProcessClock::resume(wall_start, checkpoint.checkpoint_time_ns)
            .context("resume logical clock")?;
        (clock, Some(checkpoint))
    } else {
        (ProcessClock::start(), None)
    })
}

/// Drive a chosen `processor` through the full live-lane body: journal, private
/// output, initial and interval checkpoints, the stream/poll workers, the health
/// timer, and the shutdown/replay-parity path. This is the lane-run seam.
///
/// Production callers use [`run_lane`], which establishes the clock and builds the
/// lane's own processor via [`crate::lane::LaneSpec::build_processor`] — an unknown
/// lane refuses there, with no feed-only fallback anywhere. Tests and embeddings
/// that must drive the shell over a specific processor (for example the N-stream
/// fixture over a `feed_only_processor()`) call this directly. `clock` is the lane
/// clock established before the processor so `clock.wall_start()` is the processor's
/// produced-at baseline; pass a fresh `ProcessClock::start()` and `None` for a
/// non-resumed run, or the clock/checkpoint pair from [`establish_lane_clock`] to
/// resume.
pub async fn run_lane_with_processor(
    arguments: LaneRunArguments,
    clock: ProcessClock,
    resume_checkpoint: Option<LaneCheckpointDocument>,
    processor: Box<dyn LaneProcessor>,
) -> Result<(LaneRunReport, LiveShutdownReason)> {
    run_lane_with_processor_and_hook(arguments, clock, resume_checkpoint, processor, None).await
}

/// [`run_lane_with_processor`] with an in-process observer of every stream
/// message the run writes ([`StreamHook`]).
pub async fn run_lane_with_processor_and_hook(
    arguments: LaneRunArguments,
    clock: ProcessClock,
    resume_checkpoint: Option<LaneCheckpointDocument>,
    mut processor: Box<dyn LaneProcessor>,
    stream_hook: Option<StreamHook>,
) -> Result<(LaneRunReport, LiveShutdownReason)> {
    let duration = Duration::from_secs(arguments.duration_seconds);
    let runtime_config = LaneRuntimeConfig::new(
        arguments.spec.requirement.lane,
        crate::lane::LANE_HEALTH_INTERVAL_NS,
    );

    // F3: the journal session and checkpoint identity name the processor that
    // actually runs, read from it rather than from the spec constant, so a
    // feed-only-era checkpoint no longer verifies against a venue processor.
    let identity = LaneIdentity {
        lane: arguments.spec.requirement.lane,
        venues: arguments
            .spec
            .venues
            .iter()
            .map(|v| (*v).to_owned())
            .collect(),
        processor_id: processor.processor_id().to_owned(),
        processor_version: processor.processor_version().to_owned(),
    };

    // F1: compute the observed-not-consumed source set from the lane's declared
    // streams and refuse at start-up if the processor requires a source no
    // declared stream produces.
    configure_source_routing(arguments.spec, processor.as_mut())?;

    let (session, lineage, replace_checkpoint) = if let Some(checkpoint) = resume_checkpoint {
        ensure_resume_matches_processor(&checkpoint, &identity)?;
        processor
            .restore_state(checkpoint.processor_state.clone())
            .context("restore lane processor state")?;
        let mut lineage = checkpoint.publisher_epoch_lineage.clone();
        lineage.push(arguments.publisher_epoch.clone());
        let session = LaneJournalSession::resumed(
            arguments.publisher_epoch.clone(),
            checkpoint,
            &identity,
            runtime_config.clone(),
        )
        .context("build resumed lane journal session")?;
        (session, lineage, true)
    } else {
        let session = LaneJournalSession::new(
            clock.wall_start_text(),
            arguments.publisher_epoch.clone(),
            &identity,
            runtime_config.clone(),
        )
        .context("build lane journal session")?;
        (session, vec![arguments.publisher_epoch.clone()], false)
    };

    // In rotation mode (C5) the runtime — not live-run.sh — owns the layout: open
    // generation 0 (`gen-live-<lane>-<unix>` + RUN-META + CURRENT) and take its
    // output/journal paths. Generation 0's output epoch is the process epoch
    // verbatim, so its session (built above) needs no distinct admission epoch and
    // its journal replays byte-for-byte exactly as an unrotated run. Explicit mode
    // uses the two paths passed in, unchanged.
    let lane_name = arguments.spec.requirement.lane;
    let (output_path, journal_path, mut rotation) = if let Some(rot) = arguments.rotation.as_ref() {
        let layout = FileGenerationLayout::new(&rot.lane_root, lane_name)
            .context("bind lane-root layout")?;
        let first_open_instant = Instant::now();
        let first_open_ns = clock
            .elapsed_ns()
            .context("sample first file generation open time")?;
        let now_unix = generation_unix(&clock, first_open_ns)?;
        let generation = format!("gen-live-{lane_name}-{now_unix}");
        let meta = RunMeta {
            lane: lane_name.to_owned(),
            publisher_epoch: arguments.publisher_epoch.clone(),
            generation,
            started_utc: run_meta_timestamp(&clock, first_open_ns)?,
            duration_seconds: arguments.duration_seconds,
            max_output_bytes: arguments.max_output_bytes,
            max_journal_bytes: arguments.max_journal_bytes,
            max_updates: arguments.max_updates,
            headroom_factor: rot.headroom_factor,
            resumed: replace_checkpoint,
        };
        let open = layout
            .open_next_with_rotation(now_unix, None, &meta, rot.rotate_seconds)
            .context("open the first file generation")?;
        let schedule =
            RotationSchedule::try_new(first_open_ns, Duration::from_secs(rot.rotate_seconds))
                .context("build rotation schedule")?;
        let output_path = open.output_path.clone();
        let journal_path = open.journal_path.clone();
        let context = RotationContext {
            layout,
            schedule,
            first_open_instant,
            first_open_ns,
            rotate_seconds: rot.rotate_seconds,
            duration_seconds: arguments.duration_seconds,
            headroom_factor: rot.headroom_factor,
            epoch_prefix: arguments.publisher_epoch.clone(),
            identity: identity.clone(),
            runtime_config: runtime_config.clone(),
            checkpoint_path: arguments.checkpoint.clone(),
            max_updates: arguments.max_updates,
            max_output_bytes: arguments.max_output_bytes,
            max_journal_bytes: arguments.max_journal_bytes,
            max_checkpoint_bytes: arguments.max_checkpoint_bytes,
            current: open,
        };
        (output_path, journal_path, Some(context))
    } else {
        (arguments.output.clone(), arguments.journal.clone(), None)
    };

    // Create the journal first, then the output, then the initial checkpoint, so
    // that a failure at any step leaves nothing behind: a failed journal creation
    // has not yet touched the output, and a failure after the journal exists
    // aborts the writer and unlinks the files it created. (The pre-fix order left
    // a 0-byte output when journal creation failed.)
    let journal_writer =
        LaneJournalWriter::create(&journal_path, session.clone(), arguments.max_journal_bytes)
            .context("create lane journal")?;
    let writer = match private_output(&output_path) {
        Ok(writer) => writer,
        Err(error) => {
            let _ = journal_writer.abort();
            let _ = std::fs::remove_file(&journal_path);
            return Err(anyhow::Error::new(error).context("create lane output"));
        }
    };
    let checkpoint_context = LaneCheckpointContext {
        path: arguments.checkpoint.clone(),
        maximum_bytes: arguments.max_checkpoint_bytes,
        processor_id: identity.processor_id.clone(),
        processor_version: identity.processor_version.clone(),
        session_id: session.session_id.clone(),
        wall_start: session.wall_start.clone(),
        contract_fingerprint: session.contract_fingerprint.clone(),
        runtime_config: session.runtime_config.clone(),
        publisher_epoch_lineage: lineage,
    };
    let initial_result = (|| -> Result<LaneCheckpointWriteReport> {
        let initial_checkpoint = LaneCheckpointDocument::new(
            checkpoint_context.processor_id.clone(),
            checkpoint_context.processor_version.clone(),
            checkpoint_context.session_id.clone(),
            checkpoint_context.wall_start.clone(),
            clock.elapsed_ns().context("sample checkpoint time")?,
            session
                .initial_checkpoint
                .as_ref()
                .map_or(0, |checkpoint| checkpoint.next_admission_seq),
            checkpoint_context.contract_fingerprint.clone(),
            checkpoint_context.runtime_config.clone(),
            processor.checkpoint_state(),
            checkpoint_context.publisher_epoch_lineage.clone(),
        )
        .context("build initial lane checkpoint")?;
        let report = write_lane_checkpoint(
            &arguments.checkpoint,
            initial_checkpoint,
            arguments.max_checkpoint_bytes,
            replace_checkpoint,
        )
        .context("persist initial lane checkpoint")?;
        Ok(report)
    })();
    let initial_checkpoint_report = match initial_result {
        Ok(report) => report,
        Err(error) => {
            // Nothing has run yet; leave no journal or output behind.
            let _ = journal_writer.abort();
            let _ = std::fs::remove_file(&journal_path);
            let _ = std::fs::remove_file(&output_path);
            return Err(error);
        }
    };

    let pipeline = Rc::new(RefCell::new(
        LanePipeline::live(
            BufWriter::with_capacity(1024 * 1024, writer),
            processor,
            &session,
            journal_writer,
            checkpoint_context,
            arguments.max_updates,
            arguments.max_output_bytes,
            initial_checkpoint_report,
        )
        .with_stream_hook(stream_hook),
    ));

    let (shutdown_trigger, shutdown) = live_shutdown_channel();
    let epoch = arguments.publisher_epoch.clone();

    let work_pipeline = Rc::clone(&pipeline);
    let work_clock = clock.clone();
    let work_shutdown = shutdown.clone();
    let spec = arguments.spec;
    let mut poll_driver = arguments.poll_driver;
    let index_gating = arguments
        .index_gating
        .unwrap_or_else(LaneIndexGating::from_env);
    let stream_runner: Box<dyn StreamRunner> = arguments
        .stream_runner
        .unwrap_or_else(|| Box::new(LiveStreamRunner));
    let work_future = async move {
        match &spec.driver {
            LaneDriverSpec::Streams(streams) => {
                let reports = run_streams(
                    stream_runner.as_ref(),
                    streams,
                    spec.reference_streams,
                    duration,
                    epoch,
                    work_shutdown,
                    work_pipeline,
                    work_clock,
                )
                .await?;
                Ok::<_, anyhow::Error>(LaneWork::Streams(reports))
            }
            LaneDriverSpec::Poll { .. } => {
                let driver = poll_driver
                    .take()
                    .context("the oracle lane requires a poll driver")?;
                let cycles =
                    run_poll(driver, duration, work_shutdown, work_pipeline, work_clock).await?;
                Ok(LaneWork::Poll(cycles))
            }
            LaneDriverSpec::Composite { streams, .. } => {
                let driver = poll_driver
                    .take()
                    .context("the composite oracle lane requires a poll driver")?;
                // B1/B2 audit: resolve which index streams to actually drive. A
                // disarmed or unkeyed stream is omitted (opens no socket), so it
                // can neither dial a rights-gated endpoint nor fail the shared
                // join and take the Chainlink poll down with it. With both index
                // streams omitted the composite lane runs the poll alone and still
                // writes its session record.
                let active = active_composite_streams(streams, &index_gating);
                // One honest line per omitted composite stream. A Kalshi index
                // socket whose family token IS armed but whose credentials are
                // absent gets the explicit credential line (the operator armed it
                // but the oracle unit's credential file carries no Kalshi key);
                // every other omission is "not armed/keyed".
                for declared in streams
                    .iter()
                    .filter(|declared| !active.iter().any(|kept| kept.stream == declared.stream))
                {
                    if declared.stream == btc_feeds::CaptureStream::KalshiIndex
                        && (index_gating.enabled.is_enabled("cfbrti")
                            || index_gating.enabled.is_enabled("pyth"))
                        && !index_gating.kalshi_index_credentialed
                    {
                        eprintln!("kalshi index not armed on the oracle lane: credentials absent");
                    } else {
                        eprintln!(
                            "oracle lane: composite stream omitted (not armed/keyed): {}",
                            declared.label
                        );
                    }
                }
                if active.is_empty() {
                    let cycles =
                        run_poll(driver, duration, work_shutdown, work_pipeline, work_clock)
                            .await?;
                    Ok(LaneWork::Composite {
                        reports: Vec::new(),
                        cycles,
                    })
                } else {
                    // Poll and the armed streams run concurrently on the shared
                    // pipeline (current-thread runtime; each holds the `RefCell`
                    // only across a synchronous ingest, never across an await).
                    let poll_pipeline = Rc::clone(&work_pipeline);
                    let poll_clock = work_clock.clone();
                    let poll_shutdown = work_shutdown.clone();
                    let (cycles, reports) = tokio::try_join!(
                        run_poll(driver, duration, poll_shutdown, poll_pipeline, poll_clock),
                        run_streams(
                            stream_runner.as_ref(),
                            &active,
                            spec.reference_streams,
                            duration,
                            epoch,
                            work_shutdown,
                            work_pipeline,
                            work_clock,
                        ),
                    )?;
                    Ok(LaneWork::Composite { reports, cycles })
                }
            }
        }
    };

    let maintenance_pipeline = Rc::clone(&pipeline);
    let maintenance_clock = clock.clone();
    let maintenance_future = maintain(
        maintenance_pipeline,
        maintenance_clock,
        duration,
        Duration::from_secs(arguments.checkpoint_interval_seconds),
        shutdown,
        rotation.take(),
    );
    let shutdown_deadline = clock.deadline(duration);
    let shutdown_future = async move {
        coordinate_live_shutdown(shutdown_deadline, shutdown_trigger)
            .await
            .context("listen for shutdown signal")
    };

    // The durable close runs on EVERY path — success, worker error, or shutdown —
    // in the fixed order the spot reference uses (main.rs:401-416): sync output,
    // then the journal (complete on success, abort on failure), then the final
    // checkpoint, then the report. `may_complete` gates the journal and the
    // checkpoint identically, so a failed run never writes a trusted terminal or
    // a final checkpoint on top of an incomplete stream. A worker error is not
    // propagated with `?` here — that would skip the close entirely and drop the
    // journal writer unjoined.
    let run_result = tokio::try_join!(work_future, maintenance_future, shutdown_future);
    // A clock error is an error, not a silent 0: it makes the final checkpoint
    // fail rather than record a time behind every interval checkpoint (m6).
    let elapsed_result = clock.elapsed_ns();
    let mut pipeline = Rc::try_unwrap(pipeline)
        .map_err(|_| anyhow::anyhow!("lane pipeline still has active owners"))?
        .into_inner();
    let output_result = pipeline.sync_output();
    let may_complete = run_result.is_ok() && output_result.is_ok();

    // A run cannot complete without a final clock reading: completing the
    // journal and then failing the checkpoint would leave a terminated journal
    // beside a stale checkpoint, which resume reads alone. Decide once, before
    // either durable close (spot-audit of 116e86d, minor 4).
    let may_complete = may_complete && elapsed_result.is_ok();
    let journal_result = if may_complete {
        pipeline.complete_journal()
    } else {
        pipeline.abort_journal()
    };
    // Journal complete/abort THEN checkpoint complete/abort, gated identically by
    // `may_complete`. A failed run aborts the checkpoint (the last durable
    // interval checkpoint stays the resume point), so the close is symmetric with
    // the journal (m4, mirroring main.rs:1318).
    let checkpoint_result = if may_complete {
        match elapsed_result.as_ref() {
            Ok(now_ns) => pipeline.complete_checkpoint(*now_ns),
            Err(error) => Err(LanePipelineError::Mode(format!(
                "final checkpoint clock failed: {error}"
            ))),
        }
    } else {
        pipeline.abort_checkpoint()
    };
    let pipeline_report = pipeline.report();

    // Resolve errors run-first so the worker's classified cause (e.g. a transient
    // LiveError → exit 75) survives; the journal is already aborted above.
    let (work, maintenance_outcome, shutdown_reason) =
        run_result.context("live lane run failed")?;
    let (maintenance, final_open) = maintenance_outcome;
    output_result.context("sync lane output")?;
    let journal_report = journal_result.context("finish live lane journal")?;
    let checkpoint_report = checkpoint_result.context("write final lane checkpoint")?;
    let elapsed_ns = elapsed_result.context("sample final logical time")?;

    // Rotation mode: seal the final generation's RUN-META (its journal and
    // checkpoint are already closed above by the shared graceful end). A crash
    // returns early via `?` before here, so a torn generation is never sealed —
    // exactly as live-run.sh leaves an unsealed dir on a hard exit today.
    if let Some(mut open) = final_open {
        open.seal(run_meta_timestamp(&clock, elapsed_ns)?)
            .context("seal the final file generation")?;
    }

    let (live_reports, poll_cycles) = match work {
        LaneWork::Streams(reports) => (reports, None),
        LaneWork::Poll(cycles) => (Vec::new(), Some(cycles)),
        LaneWork::Composite { reports, cycles } => (reports, Some(cycles)),
    };

    Ok((
        LaneRunReport {
            lane: arguments.spec.requirement.lane.to_owned(),
            duration_seconds: arguments.duration_seconds,
            elapsed_ns,
            live_reports,
            poll_cycles,
            maintenance,
            pipeline: pipeline_report,
            journal: journal_report,
            checkpoint: checkpoint_report,
        },
        shutdown_reason,
    ))
}

/// Wire the lane's observed-not-consumed source routing and enforce the required-
/// source misconfiguration guard (F1), deterministically from the static spec and
/// the processor's own consumed/required sets — so the live run and its replay
/// configure the processor identically.
///
/// A processor with no `consumed_sources` (the feed-only interim) accepts every
/// event and needs no routing. For one that declares a consumed set, the runtime
/// takes the union of its declared streams' [`produced_source_ids`] inventories,
/// records the sources the driver streams produce but the processor does not
/// consume as observed-not-consumed (routed past the processor, counted as
/// `ignored_events`), and refuses at start-up (fatal, exit 78) when a required
/// source is produced by no declared stream — a lane wired to a stream set that
/// can never make its identities ready.
fn configure_source_routing(spec: &LaneSpec, processor: &mut dyn LaneProcessor) -> Result<()> {
    let Some(consumed) = processor.consumed_sources() else {
        return Ok(());
    };
    let mut produced_driver: BTreeSet<String> = BTreeSet::new();
    // Both `Streams` and the stream half of a `Composite` lane contribute their
    // produced sources; a `Poll` lane produces through its driver, not a stream,
    // so it declares none here (its required source is a poll source and the
    // processor with no consumed set has already returned above).
    let driver_streams: &[crate::lane::LaneStream] = match &spec.driver {
        LaneDriverSpec::Streams(streams) => streams,
        LaneDriverSpec::Composite { streams, .. } => streams,
        LaneDriverSpec::Poll { .. } => &[],
    };
    for stream in driver_streams {
        for id in stream.stream.produced_source_ids() {
            produced_driver.insert((*id).to_owned());
        }
    }
    // The reference streams are declared too, so a required source produced only
    // by one is not flagged; required_sources() already excludes the reference.
    let mut produced_all = produced_driver.clone();
    for stream in spec.reference_streams {
        for id in stream.stream.produced_source_ids() {
            produced_all.insert((*id).to_owned());
        }
    }
    // A composite lane's poll produces its sources through the driver seam, not a
    // capture stream, so they are declared on the spec and counted here — a
    // required poll source (the Chainlink round source) must not read as missing.
    if let LaneDriverSpec::Composite { poll_sources, .. } = &spec.driver {
        for id in *poll_sources {
            produced_all.insert((*id).to_owned());
        }
    }
    let missing: Vec<String> = processor
        .required_sources()
        .difference(&produced_all)
        .cloned()
        .collect();
    if !missing.is_empty() {
        anyhow::bail!(
            "lane {} is misconfigured: it requires sources no declared stream produces: {}",
            spec.requirement.lane,
            missing.join(", ")
        );
    }
    // Observed-not-consumed spans BOTH driver and reference streams: a reference
    // socket carries sidecar sources it does not exist to feed (the Kraken USDT
    // reference socket also emits `kraken.spot.status` on connect, E47(e)). The
    // consumed set already contains the reference source the processor does read
    // (the USDT ticker), so it is differenced out here; the sidecar `status`
    // frame is routed past computation and counted, never an abort (F1).
    let observed_not_consumed: BTreeSet<String> =
        produced_all.difference(&consumed).cloned().collect();
    if !observed_not_consumed.is_empty() {
        // Logged once at start-up with the full list (F1).
        eprintln!(
            "lane {} observes but does not consume: {}",
            spec.requirement.lane,
            observed_not_consumed
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    processor.set_observed_not_consumed(observed_not_consumed);
    Ok(())
}

/// Refuse a resume whose checkpoint was written by a different processor: the id
/// AND the version must match (M4). The fingerprint already covers the lane's
/// definition set and runtime config, but a `1.0.0`→`2.0.0` bump keeps the same
/// lane and definitions, so the version is compared explicitly.
pub fn ensure_resume_matches_processor(
    checkpoint: &LaneCheckpointDocument,
    identity: &LaneIdentity,
) -> Result<()> {
    if checkpoint.processor_id != identity.processor_id {
        anyhow::bail!(
            "checkpoint processor id {} disagrees with the {} lane processor {}",
            checkpoint.processor_id,
            identity.lane,
            identity.processor_id
        );
    }
    if checkpoint.processor_version != identity.processor_version {
        anyhow::bail!(
            "checkpoint processor version {} disagrees with the {} lane processor version {}",
            checkpoint.processor_version,
            identity.lane,
            identity.processor_version
        );
    }
    Ok(())
}

/// Refuse a replay whose recorded journal session was written by a different lane
/// than the `--lane` spec: the lane name, the processor id AND the processor
/// version must all match. The journal's own `validate()` is self-referential (it
/// recomputes its fingerprint from its own runtime config), so it cannot catch a
/// wrong-lane replay; this check is the replay analogue of
/// [`ensure_resume_matches_processor`]. A plain error here classifies `Fatal`
/// (exit 78) — a wrong `--lane` is an operator configuration mistake, not a
/// transient to restart.
pub fn ensure_replay_matches_spec(session: &LaneJournalSession, spec: &LaneSpec) -> Result<()> {
    let expected_lane = spec.requirement.lane;
    if session.lane != expected_lane {
        anyhow::bail!(
            "journal lane {} does not match the --lane {} selected for replay",
            session.lane,
            expected_lane
        );
    }
    if session.processor_id != spec.processor_id {
        anyhow::bail!(
            "journal processor id {} does not match the {} lane processor {}",
            session.processor_id,
            expected_lane,
            spec.processor_id
        );
    }
    if session.processor_version != spec.processor_version {
        anyhow::bail!(
            "journal processor version {} does not match the {} lane processor version {}",
            session.processor_version,
            expected_lane,
            spec.processor_version
        );
    }
    Ok(())
}

/// The per-stream handler the shell binds to the lane pipeline.
type StreamHandle<'a> =
    &'a mut (dyn FnMut(TimedIngestOutcome) -> Result<LiveControl, LiveError> + 'a);

/// The seam that runs ONE stream to its deadline. Production is [`LiveStreamRunner`]
/// (A5's `run_live` over a real socket); a test injects a fixture runner that
/// produces deterministic outcomes, so the N-stream shell — the joined workers,
/// the checkpoint/health maintenance timer, and the single durable close — is
/// exercised through `run_lane` with a `Streams` spec without a network (E39(c) M2).
pub trait StreamRunner {
    fn run_stream<'a>(
        &'a self,
        options: LiveOptions,
        clock: ProcessClock,
        handle: StreamHandle<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<LiveReport, LiveError>> + 'a>>;
}

/// The production stream runner: A5's `run_live` over a real venue socket.
pub struct LiveStreamRunner;

impl StreamRunner for LiveStreamRunner {
    fn run_stream<'a>(
        &'a self,
        options: LiveOptions,
        clock: ProcessClock,
        handle: StreamHandle<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<LiveReport, LiveError>> + 'a>> {
        Box::pin(run_live(options, clock, handle))
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_streams(
    runner: &dyn StreamRunner,
    streams: &[crate::lane::LaneStream],
    reference_streams: &'static [crate::lane::LaneStream],
    duration: Duration,
    epoch: String,
    shutdown: LiveShutdown,
    pipeline: Rc<RefCell<LanePipeline<BufWriter<File>>>>,
    clock: ProcessClock,
) -> Result<Vec<LiveReport>> {
    anyhow::ensure!(
        !streams.is_empty(),
        "the lane shell requires at least one stream; the lane spec declared none"
    );
    // One `run_live` worker per declared stream, all sharing the lane pipeline,
    // checkpoint clock, epoch and shutdown coordinator. `LaneSpec::BINANCE`
    // declares four streams and `LaneSpec::BYBIT` three, so the fan-out must
    // drive any N, not just one or two. Reference streams (E47(e)) are driven in
    // the SAME concurrent fan-out as the venue's own streams — they are sockets
    // like any other; the processor routes their events to the `UsdtReference`
    // and keeps them out of readiness. They journal and feed-health identically.
    let workers = streams
        .iter()
        .chain(reference_streams.iter())
        .map(|stream| {
            run_stream_worker(
                runner,
                *stream,
                duration,
                epoch.clone(),
                shutdown.clone(),
                Rc::clone(&pipeline),
                clock.clone(),
            )
        });
    join_lane_workers(workers).await
}

/// Join every stream worker, failing the whole lane — and cancelling the rest —
/// the moment any one fails, so the durable close runs once for the whole lane.
/// Split out from [`run_streams`] so the N-way fan-out is unit-testable without a
/// live socket, which the workers themselves require.
async fn join_lane_workers<Fut>(workers: impl IntoIterator<Item = Fut>) -> Result<Vec<LiveReport>>
where
    Fut: std::future::Future<Output = Result<LiveReport>>,
{
    futures_util::future::try_join_all(workers).await
}

async fn run_stream_worker(
    runner: &dyn StreamRunner,
    stream: crate::lane::LaneStream,
    duration: Duration,
    epoch: String,
    shutdown: LiveShutdown,
    pipeline: Rc<RefCell<LanePipeline<BufWriter<File>>>>,
    clock: ProcessClock,
) -> Result<LiveReport> {
    let options = LiveOptions {
        stream: stream.stream,
        duration,
        kraken_btc_book_depth: 10,
        publisher_epoch: epoch,
        diagnostics: LiveDiagnostics::default(),
        shutdown: Some(shutdown),
        kraken_trade_reconciliation: None,
    };
    let label = stream.label;
    let handler_clock = clock.clone();
    let mut handler = move |timed: TimedIngestOutcome| {
        admit_stream_outcome(&pipeline, &handler_clock, label, timed)
    };
    let report = runner.run_stream(options, clock, &mut handler).await?;
    Ok(report)
}

fn admit_stream_outcome(
    pipeline: &Rc<RefCell<LanePipeline<BufWriter<File>>>>,
    clock: &ProcessClock,
    label: &'static str,
    timed: TimedIngestOutcome,
) -> Result<LiveControl, LiveError> {
    let transitions: Vec<bool> = timed
        .outcome
        .notices
        .iter()
        .filter_map(|notice| match notice.code {
            FeedNoticeCode::Connected => Some(true),
            FeedNoticeCode::Disconnected => Some(false),
            _ => None,
        })
        .collect();
    let events = timed.outcome.events;
    if transitions.is_empty() && events.is_empty() {
        return Ok(LiveControl::Continue);
    }
    let mut pipeline = pipeline.borrow_mut();
    let now_ns = clock.admission_ns(timed.logical_receipt_ns)?;
    // Within one outcome the causal order is: a Connected transition (the socket
    // came up) precedes the data it carried, and a Disconnected transition (the
    // socket dropped after this batch was decoded) FOLLOWS it. Admitting a
    // Disconnected before the outcome's own events would let those events
    // re-insert the stream into `live_streams` after the drop, resurrecting the
    // lane's liveness and flipping `status` back to Ready for a stream whose
    // socket is down. So admit Connected transitions first, then the events, then
    // any Disconnected transitions last.
    for connected in transitions.iter().copied().filter(|connected| *connected) {
        admit_lane_connectivity(&mut pipeline, label, connected, now_ns)?;
    }
    if !events.is_empty() {
        let admission_seq = pipeline.next_admission_seq();
        pipeline
            .admit_live(LaneJournalInput::Events {
                admission_seq,
                stream: label.to_owned(),
                events: Arc::new(events),
                logical_receipt_ns: timed.logical_receipt_ns,
                now_ns,
                resync: false,
            })
            .map_err(|error| LiveError::Publication(error.to_string()))?;
    }
    for connected in transitions.iter().copied().filter(|connected| !*connected) {
        admit_lane_connectivity(&mut pipeline, label, connected, now_ns)?;
    }
    Ok(LiveControl::Continue)
}

/// Admit one stream connectivity transition into the lane pipeline. Shared by the
/// Connected-first / Disconnected-last ordering in [`admit_stream_outcome`].
fn admit_lane_connectivity(
    pipeline: &mut LanePipeline<BufWriter<File>>,
    label: &'static str,
    connected: bool,
    now_ns: u64,
) -> Result<(), LiveError> {
    let admission_seq = pipeline.next_admission_seq();
    pipeline
        .admit_live(LaneJournalInput::Connectivity {
            admission_seq,
            stream: label.to_owned(),
            connected,
            now_ns,
        })
        .map_err(|error| LiveError::Publication(error.to_string()))
}

async fn run_poll(
    mut driver: Box<dyn PollDriver>,
    duration: Duration,
    mut shutdown: LiveShutdown,
    pipeline: Rc<RefCell<LanePipeline<BufWriter<File>>>>,
    clock: ProcessClock,
) -> Result<u64> {
    let label = driver.stream_label();
    let deadline = clock.deadline(duration);
    let interval_duration = driver.poll_interval();
    let mut ticker = interval_at(Instant::now() + interval_duration, interval_duration);
    let mut cycles = 0_u64;
    let mut last_connected: Option<bool> = None;
    // A second receiver on the same shutdown channel, so the driver poll can be
    // raced against shutdown INSIDE the ticker arm without the outer `select!`'s
    // mutable borrow of `shutdown` conflicting.
    let mut poll_shutdown = shutdown.clone();
    // Set by whichever arm ends the loop: `true` on the natural deadline (either
    // `sleep_until` or a non-interrupt shutdown), `false` on a signal interrupt.
    let reached_deadline;
    loop {
        tokio::select! {
            // The deadline coordinator fires this shutdown at the natural
            // deadline too, and it can win the race against `sleep_until` below —
            // so a NON-interrupt shutdown counts as reaching the deadline; only a
            // real signal (Ctrl-C / SIGTERM) is an interruption that never trips
            // the whole-run-outage exit.
            reason = shutdown.wait_reason() => {
                reached_deadline = !reason.context("listen for oracle shutdown")?.interrupted();
                break;
            }
            () = sleep_until(deadline) => { reached_deadline = true; break; }
            _ = ticker.tick() => {
                let poll_start = clock.elapsed_ns().context("sample poll time")?;
                // A single poll can block for tens of seconds — a whole-endpoint
                // outage over the Chainlink endpoint list makes `driver.poll`
                // await `checks × endpoints × per-request timeout`. Awaiting it
                // straight in this arm body would make shutdown and the deadline
                // unobservable for that whole span: a SIGTERM arriving mid-poll
                // would be seen only after the poll returns, and under a
                // supervisor's stop timeout the process would be SIGKILLed before
                // the durable close runs, leaving no terminal journal record. So
                // the poll is polled INSIDE a `select!` as a branch, raced against
                // shutdown and the deadline (E39).
                let batch = tokio::select! {
                    reason = poll_shutdown.wait_reason() => {
                        reached_deadline = !reason.context("listen for oracle shutdown")?.interrupted();
                        break;
                    }
                    () = sleep_until(deadline) => { reached_deadline = true; break; }
                    result = driver.poll(poll_start) => result.context("poll oracle driver")?,
                };
                cycles = cycles.checked_add(1).context("poll counter exhausted")?;
                let mut pipeline = pipeline.borrow_mut();
                // Stamp the journal in admission order, not at poll-start: a poll
                // whose transport awaited past a health tick would otherwise carry
                // a now_ns BELOW that tick's, regressing logical time and breaking
                // replay (ruling E39(c)). `admission_ns` is the monotonic
                // high-watermark the stream lane already uses; the poll-start time
                // stays as the events' logical receipt.
                let now_ns = clock
                    .admission_ns(poll_start)
                    .context("sample poll admission time")?;
                // Start-up / reconnect evidence first: the driver surfaced these
                // notices from the calls it made this cycle (M1). Journaled as
                // evidence — a first-tick TargetMismatch still leaves a trail.
                for notice in batch.notices {
                    let admission_seq = pipeline.next_admission_seq();
                    pipeline
                        .admit_live(LaneJournalInput::Notice {
                            admission_seq,
                            stream: label.to_owned(),
                            code: notice.code,
                            detail: notice.detail,
                            now_ns,
                        })
                        .context("admit oracle notice")?;
                }
                if last_connected != Some(batch.connected) {
                    let admission_seq = pipeline.next_admission_seq();
                    pipeline
                        .admit_live(LaneJournalInput::Connectivity {
                            admission_seq,
                            stream: label.to_owned(),
                            connected: batch.connected,
                            now_ns,
                        })
                        .context("admit oracle connectivity")?;
                    last_connected = Some(batch.connected);
                }
                if !batch.events.is_empty() {
                    let admission_seq = pipeline.next_admission_seq();
                    pipeline
                        .admit_live(LaneJournalInput::Events {
                            admission_seq,
                            stream: label.to_owned(),
                            events: Arc::new(batch.events),
                            logical_receipt_ns: poll_start,
                            now_ns,
                            resync: false,
                        })
                        .context("admit oracle events")?;
                }
            }
        }
    }

    // B1/M5: the shell owns the deadline, so it — not the driver — decides the
    // whole-run outage. A run that reached its deadline with the driver never
    // verified produced no data at all: mark the feed down and exit transient
    // (75, the class a supervisor restarts), never a clean 0. A verified run — or
    // one stopped by a shutdown signal — exits normally.
    if reached_deadline && !driver.ever_verified() {
        let outage_floor = clock.elapsed_ns().context("sample outage time")?;
        let now_ns = clock
            .admission_ns(outage_floor)
            .context("sample outage admission time")?;
        {
            let mut pipeline = pipeline.borrow_mut();
            if last_connected != Some(false) {
                let admission_seq = pipeline.next_admission_seq();
                pipeline
                    .admit_live(LaneJournalInput::Connectivity {
                        admission_seq,
                        stream: label.to_owned(),
                        connected: false,
                        now_ns,
                    })
                    .context("admit oracle whole-run outage")?;
            }
        }
        return Err(anyhow::Error::new(ClassifiedMessage::new(
            ExitClass::Retryable,
            format!("oracle lane {label}: no endpoint verified for the whole run"),
        )));
    }
    Ok(cycles)
}

async fn maintain(
    pipeline: Rc<RefCell<LanePipeline<BufWriter<File>>>>,
    clock: ProcessClock,
    duration: Duration,
    checkpoint_interval: Duration,
    mut shutdown: LiveShutdown,
    mut rotation: Option<RotationContext>,
) -> Result<(LaneMaintenanceReport, Option<OpenFileGeneration>)> {
    let deadline = clock.deadline(duration);
    let mut health = interval_at(
        Instant::now() + Duration::from_secs(1),
        Duration::from_secs(1),
    );
    let mut checkpoint = interval_at(Instant::now() + checkpoint_interval, checkpoint_interval);
    let mut health_ticks = 0_u64;
    let mut interval_checkpoints = 0_u64;
    let mut interrupted = false;
    // The next in-place rotation deadline, measured drift-free from generation 0's
    // open on the process clock. `deadline` (the run's own end) stands in when the
    // run does not rotate, and the guarded arm never fires then.
    let mut next_rotation_at = rotation.as_ref().map_or(deadline, next_rotation_instant);
    loop {
        tokio::select! {
            result = shutdown.wait_reason() => {
                interrupted = result.context("listen for shutdown signal")?.interrupted();
                break;
            }
            () = sleep_until(deadline) => break,
            () = sleep_until(next_rotation_at), if rotation.is_some() => {
                let now_ns = clock.elapsed_ns().context("sample rotation time")?;
                let ctx = rotation.as_mut().expect("the rotation arm runs only when present");
                {
                    let mut pipeline = pipeline.borrow_mut();
                    perform_rotation(&mut pipeline, ctx, &clock, now_ns)
                        .context("rotate to the next file generation")?;
                }
                next_rotation_at = next_rotation_instant(ctx);
            }
            _ = health.tick() => {
                let health_floor = clock.elapsed_ns().context("sample health time")?;
                // Admission order, not sample order: share the monotonic
                // high-watermark with the poll/stream admits so an interleaved
                // health tick never regresses logical time (ruling E39(c)).
                let now_ns = clock
                    .admission_ns(health_floor)
                    .context("sample health admission time")?;
                let mut pipeline = pipeline.borrow_mut();
                let admission_seq = pipeline.next_admission_seq();
                pipeline
                    .admit_live(LaneJournalInput::HealthTick { admission_seq, now_ns })
                    .context("admit lane health tick")?;
                health_ticks = health_ticks.checked_add(1).context("health tick counter exhausted")?;
            }
            _ = checkpoint.tick() => {
                let now_ns = clock.elapsed_ns().context("sample checkpoint time")?;
                let mut pipeline = pipeline.borrow_mut();
                pipeline.sync_output().context("sync interval lane output")?;
                pipeline.checkpoint_live(now_ns).context("write interval lane checkpoint")?;
                interval_checkpoints = interval_checkpoints.checked_add(1).context("checkpoint counter exhausted")?;
            }
        }
    }
    Ok((
        LaneMaintenanceReport {
            health_ticks,
            interval_checkpoints,
            interrupted,
        },
        rotation.map(|ctx| ctx.current),
    ))
}

/// The monotonic instant of the next rotation deadline, drift-free: generation 0's
/// open instant plus the schedule's next process-clock deadline measured from that
/// same open. After a seal calls [`RotationSchedule::advance_to`], the next
/// deadline (and this instant) jumps forward past any periods a slow seal missed.
fn next_rotation_instant(ctx: &RotationContext) -> Instant {
    rotation_instant(
        ctx.first_open_instant,
        ctx.first_open_ns,
        ctx.schedule.next_deadline(),
    )
}

/// The monotonic [`Instant`] to arm the rotation timer at, from generation 0's open
/// instant, generation 0's process-clock reading at that same open, and the schedule's
/// next (absolute) process-clock deadline. This is the ACTUAL computation the maintain
/// loop's rotation arm uses; testing it directly pins the resume-safety fix.
///
/// The schedule's `next_deadline()` is an ABSOLUTE process-clock reading —
/// `first_open_ns + k·period` — and on a resumed run `first_open_ns` carries a large
/// logical offset (the checkpoint time). The timer base `first_open_instant` is a plain
/// wall [`Instant`], so the deadline must be added as the RELATIVE offset
/// `next_deadline − first_open_ns` (= `k·period`), never as the absolute deadline
/// itself. Feeding the absolute reading into a relative base (as `ProcessClock::deadline`
/// would, adding the duration to `mono_start`) is the bug that lands the first rotation
/// of a resumed run hours late; subtracting `first_open_ns` here cancels the offset.
fn rotation_instant(
    first_open_instant: Instant,
    first_open_ns: u64,
    next_deadline_ns: u64,
) -> Instant {
    let offset_ns = next_deadline_ns.saturating_sub(first_open_ns);
    first_open_instant + Duration::from_nanos(offset_ns)
}

/// Seal the current file generation and open the next in place (C5). The processor,
/// its rings and the process/wall clock stay in memory; only the output/journal
/// writers and the per-generation stream accounting are swapped, so the first
/// decisions after the boundary carry every identity that was Ready before it. The
/// next generation's journal is a fresh session resumed from the sealed checkpoint
/// under a new output epoch (`<process-epoch>-<unix>`); the pipeline re-stamps every
/// emitted update with that output epoch (the feeds keep stamping admitted events
/// with the process epoch), so each generation's frames — envelope and updates alike
/// — reset like a fresh process, and replay reproduces them from the journal.
fn perform_rotation(
    pipeline: &mut LanePipeline<BufWriter<File>>,
    ctx: &mut RotationContext,
    clock: &ProcessClock,
    now_ns: u64,
) -> Result<()> {
    // Seal the current generation (journal terminal + checkpoint), keep the processor.
    let (seal_doc, seal_report) = pipeline
        .seal_generation(now_ns)
        .context("seal the current file generation")?;
    ctx.current
        .seal(run_meta_timestamp(clock, now_ns)?)
        .context("write the sealed RUN-META")?;
    // The seal fsyncs the output and completes the journal, which can take long enough
    // to overrun one or more rotation periods. Sample the clock AFTER it and use that
    // post-seal instant to advance the schedule (so a slow seal skips the periods it
    // missed instead of rotating back-to-back) and to stamp RUN-META `started_utc`
    // (so it reflects when the generation actually opened).
    let post_seal_ns = clock
        .elapsed_ns()
        .context("sample the post-seal rotation time")?;
    ctx.schedule.advance_to(post_seal_ns);

    // Name the next generation from its SCHEDULED grid second (`first_open + n·period`),
    // never the observed post-seal open: because the schedule advances at least one
    // whole period per rotation, consecutive generations' scheduled seconds are always
    // at least one period apart, so a seal overrunning into the next wall second can
    // never reuse the just-sealed generation's directory second and raise a same-second
    // `GenerationExists` (C5). RUN-META `started_utc` still records the observed open.
    let scheduled_ns = ctx.schedule.scheduled_open_ns();
    let now_unix = generation_unix(clock, scheduled_ns)?;
    let lane = ctx.identity.lane;
    let new_epoch = format!("{}-{now_unix}", ctx.epoch_prefix);
    let generation = format!("gen-live-{lane}-{now_unix}");
    let meta = RunMeta {
        lane: lane.to_owned(),
        publisher_epoch: new_epoch.clone(),
        generation,
        started_utc: run_meta_timestamp(clock, post_seal_ns)?,
        duration_seconds: ctx.duration_seconds,
        max_output_bytes: ctx.max_output_bytes,
        max_journal_bytes: ctx.max_journal_bytes,
        max_updates: ctx.max_updates,
        headroom_factor: ctx.headroom_factor,
        resumed: true,
    };
    let previous_dir = ctx
        .current
        .dir
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned);
    let next_open = match ctx.layout.open_next_with_rotation(
        now_unix,
        previous_dir.as_deref(),
        &meta,
        ctx.rotate_seconds,
    ) {
        Ok(open) => open,
        // The current generation is already sealed above, so a same-name successor
        // (a transient collision, e.g. a fast rotation landing on a second a prior
        // run's directory already used) is a Retryable condition: seal-and-exit so a
        // supervisor restarts, never Fatal. A gen-0 `GenerationExists` at startup keeps
        // its Fatal class — that open is in `run_lane_with_processor`, not this path.
        Err(FileGenerationError::GenerationExists { path }) => {
            return Err(anyhow::Error::new(ClassifiedMessage::new(
                ExitClass::Retryable,
                format!(
                    "rotation could not open the next file generation: {} already exists",
                    path.display()
                ),
            )));
        }
        Err(error) => {
            return Err(anyhow::Error::new(error)).context("open the next file generation");
        }
    };

    let session = LaneJournalSession::resumed(
        new_epoch.clone(),
        seal_doc.clone(),
        &ctx.identity,
        ctx.runtime_config.clone(),
    )
    .context("build the next generation's journal session")?
    // C5: this generation opened by rotation, so its journal begins mid-stream (a
    // resync re-snapshot, warm rings) and its terminal count is warm-vs-cold on
    // replay — the flag lets `finish_replay` report the re-warm gap instead of failing.
    .opened_by_rotation();
    let mut lineage = seal_doc.publisher_epoch_lineage.clone();
    lineage.push(new_epoch);

    let journal = LaneJournalWriter::create(
        &next_open.journal_path,
        session.clone(),
        ctx.max_journal_bytes,
    )
    .context("create the next generation's journal")?;
    let output =
        private_output(&next_open.output_path).context("create the next generation's output")?;
    let checkpoint_context = LaneCheckpointContext {
        path: ctx.checkpoint_path.clone(),
        maximum_bytes: ctx.max_checkpoint_bytes,
        processor_id: ctx.identity.processor_id.clone(),
        processor_version: ctx.identity.processor_version.clone(),
        session_id: session.session_id.clone(),
        wall_start: session.wall_start.clone(),
        contract_fingerprint: session.contract_fingerprint.clone(),
        runtime_config: session.runtime_config.clone(),
        publisher_epoch_lineage: lineage,
    };
    pipeline.begin_generation(
        BufWriter::with_capacity(1024 * 1024, output),
        journal,
        &session,
        checkpoint_context,
        seal_report,
    );
    // Write the resync re-snapshot of every delta-reconstructed book as the first
    // records of the new generation's journal, so a cold replay rebuilds the books the
    // sealed checkpoint does not carry (C5, decision 23:30Z). No-op for lanes without a
    // delta book (Binance full-image, oracle). Admit it at an `admission_ns` sample —
    // the monotonic high-watermark floored by the pre-seal receipt — exactly as every
    // other admit does (ruling E39(c)), never the raw pre-seal `elapsed_ns`, so its
    // logical time can never regress against an interleaved admit on replay.
    let resync_ns = clock
        .admission_ns(now_ns)
        .context("sample the rotation resync admission time")?;
    pipeline
        .admit_resync_generation(resync_ns)
        .context("admit the rotation resync records")?;
    ctx.current = next_open;
    Ok(())
}

/// The unix-second suffix for a generation directory/epoch, rendered from the
/// logical wall clock at `now_ns`. This is monotonic in `now_ns` but does not by
/// itself guarantee distinct suffixes — the caller must pass a SCHEDULED grid
/// deadline ([`RotationSchedule::scheduled_open_ns`]), whose values are at least
/// one whole period apart, not an observed post-seal open time (two of which can
/// share a wall second after a slow seal, the C5 `GenerationExists` collision).
fn generation_unix(clock: &ProcessClock, now_ns: u64) -> Result<u64> {
    let wall = clock
        .wall_time_at(now_ns)
        .context("render generation wall time")?;
    u64::try_from(wall.timestamp()).context("generation timestamp out of range")
}

/// A `RUN-META.json` timestamp (`started_utc`/`sealed_utc`) from the logical wall
/// clock at `now_ns`, in live-run.sh's `%Y-%m-%dT%H:%M:%SZ` shape.
fn run_meta_timestamp(clock: &ProcessClock, now_ns: u64) -> Result<String> {
    let wall = clock
        .wall_time_at(now_ns)
        .context("render RUN-META wall time")?;
    Ok(wall.format("%Y-%m-%dT%H:%M:%SZ").to_string())
}

fn private_output(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    options.mode(0o600);
    options.open(path)
}

/// Deterministically rebuild a lane's stream from its retained journal, using the
/// SAME processor the live run used: it is rebuilt from the journal's own session
/// `wall_start` (so produced-at timestamps replay identically) and lane spec via
/// [`crate::lane::LaneSpec::build_processor`]. An unknown lane refuses here, with no
/// feed-only fallback.
pub fn replay_lane(
    spec: &'static LaneSpec,
    journal_path: &Path,
    output_path: &Path,
    max_updates: u64,
    max_output_bytes: u64,
    max_journal_bytes: u64,
) -> Result<LanePipelineReport> {
    let reader = LaneJournalReplayReader::open(journal_path, max_journal_bytes)
        .context("open lane journal")?;
    let session = reader.session().clone();
    // The wall clock is the session's own wall_start, so the venue processor's
    // produced-at baseline matches the live run and the stream replays identically.
    let wall_start = DateTime::parse_from_rfc3339(&session.wall_start)
        .context("parse replay session wall_start")?
        .with_timezone(&Utc);
    let processor = spec
        .build_processor(&session.publisher_epoch, wall_start)
        .context("build replay lane processor")?;
    replay_lane_over_reader(
        spec,
        reader,
        output_path,
        max_updates,
        max_output_bytes,
        processor,
    )
}

/// Replay seam: rebuild a lane's stream from its journal over a chosen `processor`,
/// skipping [`crate::lane::LaneSpec::build_processor`]. Production replay uses
/// [`replay_lane`], which builds the lane's own processor from the journal's session
/// (an unknown lane refuses there). Tests and embeddings that must replay over a
/// specific processor — for example the N-stream fixture over a `feed_only_processor()`
/// — call this directly. The `spec` is still checked against the journal's session
/// ([`ensure_replay_matches_spec`], inside the shared body).
pub fn replay_lane_with_processor(
    spec: &'static LaneSpec,
    journal_path: &Path,
    output_path: &Path,
    max_updates: u64,
    max_output_bytes: u64,
    max_journal_bytes: u64,
    processor: Box<dyn LaneProcessor>,
) -> Result<LanePipelineReport> {
    let reader = LaneJournalReplayReader::open(journal_path, max_journal_bytes)
        .context("open lane journal")?;
    replay_lane_over_reader(
        spec,
        reader,
        output_path,
        max_updates,
        max_output_bytes,
        processor,
    )
}

fn replay_lane_over_reader(
    spec: &'static LaneSpec,
    mut reader: LaneJournalReplayReader,
    output_path: &Path,
    max_updates: u64,
    max_output_bytes: u64,
    mut processor: Box<dyn LaneProcessor>,
) -> Result<LanePipelineReport> {
    let session = reader.session().clone();
    // Refuse a journal recorded by a different lane/processor than the `--lane`
    // spec selected. The session validates only against ITSELF (its fingerprint is
    // recomputed from its own runtime config), so nothing else rejects replaying a
    // bybit journal through the binance processor: the foreign stream labels never
    // satisfy the binance required-streams, the rebuilt output silently differs
    // from the original, and `finish_replay`'s stream-message-count check can
    // coincide and pass. This is the replay analogue of the live path's
    // `ensure_resume_matches_processor`. A mismatch is a fatal configuration error
    // (exit 78), never restarted blindly.
    ensure_replay_matches_spec(&session, spec)?;
    // Configure the observed-not-consumed routing identically to the live run so
    // ignored sources are dropped, not dispatched, on replay too.
    configure_source_routing(spec, processor.as_mut())?;
    if let Some(checkpoint) = &session.initial_checkpoint {
        processor
            .restore_state(checkpoint.processor_state.clone())
            .context("restore replay processor state")?;
    }
    let output = private_output(output_path).context("create replay output")?;
    let mut pipeline = LanePipeline::replay(
        BufWriter::with_capacity(1024 * 1024, output),
        processor,
        &session,
        max_updates,
        max_output_bytes,
    );
    while let Some(input) = reader.next_input().context("read lane journal")? {
        pipeline
            .admit_replay(input)
            .context("apply lane replay input")?;
    }
    let end = reader
        .end()
        .context("lane journal has no end record")?
        .clone();
    pipeline.sync_output().context("sync replay output")?;
    pipeline.finish_replay(&end).context("finish lane replay")
}

fn validate_arguments(arguments: &LaneRunArguments) -> Result<()> {
    const MAX_RUN_SECONDS: u64 = 7 * 24 * 60 * 60;
    if arguments.duration_seconds == 0 || arguments.duration_seconds > MAX_RUN_SECONDS {
        anyhow::bail!("duration_seconds must be between 1 and {MAX_RUN_SECONDS}");
    }
    if arguments.checkpoint_interval_seconds == 0
        || arguments.checkpoint_interval_seconds >= arguments.duration_seconds
    {
        anyhow::bail!("checkpoint interval must be positive and shorter than the run");
    }
    // In rotation mode the output/journal paths come from the file-generation
    // layout, not from `arguments` (which the CLI leaves empty), so only the
    // checkpoint path is meaningful here; the distinctness rule applies to the two
    // explicit paths of explicit mode.
    if arguments.rotation.is_none()
        && (arguments.output == arguments.journal
            || arguments.output == arguments.checkpoint
            || arguments.journal == arguments.checkpoint)
    {
        anyhow::bail!("output, journal, and checkpoint paths must be distinct");
    }
    if arguments.publisher_epoch.trim().is_empty() || arguments.publisher_epoch.len() > 128 {
        anyhow::bail!("publisher_epoch must contain 1 to 128 bytes");
    }
    for (name, value) in [
        ("max_updates", arguments.max_updates),
        ("max_output_bytes", arguments.max_output_bytes),
        ("max_journal_bytes", arguments.max_journal_bytes),
        ("max_checkpoint_bytes", arguments.max_checkpoint_bytes),
    ] {
        if value == 0 {
            anyhow::bail!("{name} must be greater than zero");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{LaneIndexGating, active_composite_streams, join_lane_workers, rotation_instant};
    use crate::file_generation::RotationSchedule;
    use crate::lane::LaneStream;
    use btc_feeds::CaptureStream;
    use btc_feeds::live::LiveReport;
    use signals_core::EnabledVenues;
    use tokio::time::Instant;

    fn composite_index_streams() -> [LaneStream; 3] {
        [
            LaneStream {
                token: "cfbrti",
                stream: CaptureStream::KalshiIndex,
                label: "kalshi_index",
            },
            LaneStream {
                token: "pyth",
                stream: CaptureStream::PythHermesStream,
                label: "pyth_hermes",
            },
            // A non-index stream that is always driven, proving the gate only
            // touches the two index sockets.
            LaneStream {
                token: "coinbase",
                stream: CaptureStream::CoinbaseCore,
                label: "coinbase_core",
            },
        ]
    }

    fn index_gating(
        list: Option<&str>,
        hermes_keyed: bool,
        kalshi_index_credentialed: bool,
    ) -> LaneIndexGating {
        LaneIndexGating {
            enabled: EnabledVenues::parse(list).expect("allow-list parses"),
            hermes_keyed,
            kalshi_index_credentialed,
        }
    }

    fn keeps(active: &[LaneStream], stream: CaptureStream) -> bool {
        active.iter().any(|kept| kept.stream == stream)
    }

    #[test]
    fn kalshi_index_arms_only_when_a_family_token_and_the_credentials_are_present() {
        let streams = composite_index_streams();

        // Armed (cfbrti listed) AND credentialed: the socket opens.
        let active = active_composite_streams(&streams, &index_gating(Some("cfbrti"), false, true));
        assert!(
            keeps(&active, CaptureStream::KalshiIndex),
            "armed + credentialed opens the Kalshi index socket"
        );
        assert!(
            keeps(&active, CaptureStream::CoinbaseCore),
            "the non-index stream is always driven"
        );

        // Armed but credentials ABSENT: the socket is omitted, the lane still
        // runs (the non-index stream is kept), and the index identities go
        // unavailable rather than the lane failing its shared join.
        let active =
            active_composite_streams(&streams, &index_gating(Some("cfbrti,pyth"), true, false));
        assert!(
            !keeps(&active, CaptureStream::KalshiIndex),
            "armed without Kalshi credentials omits the index socket"
        );
        assert!(
            keeps(&active, CaptureStream::CoinbaseCore),
            "the lane keeps running without the index socket"
        );

        // Unarmed (no cfbrti/pyth): omitted regardless of credentials — as before.
        let active = active_composite_streams(&streams, &index_gating(None, false, true));
        assert!(
            !keeps(&active, CaptureStream::KalshiIndex),
            "an unarmed lane never opens the Kalshi index socket"
        );
    }

    #[test]
    fn pyth_hermes_arms_only_when_pyth_is_listed_and_keyed() {
        let streams = composite_index_streams();

        let active = active_composite_streams(&streams, &index_gating(Some("pyth"), true, false));
        assert!(
            keeps(&active, CaptureStream::PythHermesStream),
            "pyth listed + keyed opens the Hermes stream"
        );

        for gating in [
            index_gating(Some("pyth"), false, true),  // listed, unkeyed
            index_gating(Some("cfbrti"), true, true), // keyed, not listed for pyth
        ] {
            let active = active_composite_streams(&streams, &gating);
            assert!(
                !keeps(&active, CaptureStream::PythHermesStream),
                "the Hermes stream needs pyth listed AND keyed"
            );
        }
    }

    /// The rotation timer arms one period after generation 0's OPEN instant, never one
    /// period after the process clock's epoch origin. A resumed run's process clock
    /// carries a large logical offset (the checkpoint time), so `first_open_ns` is
    /// large and the schedule's `next_deadline()` is `first_open_ns + period`; the
    /// [`Instant`] the maintain loop arms must be `first_open_instant + period`, not
    /// `first_open_instant + first_open_ns + period` (hours late). This calls the ACTUAL
    /// `rotation_instant` the rotation arm uses, so reverting the fix (adding the
    /// absolute deadline to the base) fails it.
    #[test]
    fn rotation_timer_arms_relative_to_open_even_on_a_resumed_clock() {
        let period = Duration::from_secs(3_600);
        // A resumed clock: `first_open_ns` is a large logical offset (~48h), as
        // `ProcessClock::resume` produces from a checkpoint time.
        let first_open_ns = 48 * 3_600 * 1_000_000_000_u64;
        let first_open_instant = Instant::now();
        let mut schedule = RotationSchedule::new(first_open_ns, period);
        assert_eq!(
            schedule.next_deadline(),
            first_open_ns + 3_600 * 1_000_000_000,
            "the absolute deadline carries the resumed logical offset",
        );
        // The armed instant is exactly one period after the open — not offset by the
        // resumed logical time. (Adding the absolute deadline instead would land ~48h
        // later, which this equality rejects.)
        assert_eq!(
            rotation_instant(first_open_instant, first_open_ns, schedule.next_deadline()),
            first_open_instant + period,
            "the first rotation arms one period after the open, not one period after the epoch origin",
        );
        // A fresh (offset-zero) clock arms at the same relative period.
        let fresh_open = Instant::now();
        let fresh = RotationSchedule::new(0, period);
        assert_eq!(
            rotation_instant(fresh_open, 0, fresh.next_deadline()),
            fresh_open + period,
            "a fresh clock arms the timer at exactly one period too",
        );
        // After a boundary the timer arms k periods after the open, still resume-safe.
        schedule.advance_to(first_open_ns + 3_600 * 1_000_000_000);
        assert_eq!(
            rotation_instant(first_open_instant, first_open_ns, schedule.next_deadline()),
            first_open_instant + 2 * period,
            "the second rotation arms two periods after the open, offset cancelled",
        );
    }

    /// A dummy per-stream report, tagged by `records` so the join order is
    /// checkable without a live socket.
    fn stream_report(records: u64) -> LiveReport {
        LiveReport {
            records,
            events: 0,
            notices: 0,
            recoveries: 0,
            coinbase_trade_backfills: 0,
            connection_attempts: 0,
            successful_connections: 0,
            reconnects: 0,
            read_idle_timeouts: 0,
            induced_coinbase_trade_drops: 0,
            induced_disconnects: 0,
            kraken_tokens_created: 0,
            interrupted: false,
            kraken_trade_reconciliation: None,
        }
    }

    /// B1: the lane shell drives any number of streams. `LaneSpec::BINANCE`
    /// declares four and `LaneSpec::BYBIT` three — arities the old one-or-two
    /// guard rejected outright — and a single worker failing fails the whole lane.
    #[tokio::test(flavor = "current_thread")]
    async fn the_lane_shell_joins_every_stream_and_fails_on_any_worker_error() {
        // Four workers, the Binance lane's arity.
        let reports = join_lane_workers(
            (0..4u64).map(|records| async move { Ok::<_, anyhow::Error>(stream_report(records)) }),
        )
        .await
        .expect("four stream workers join into one report set");
        assert_eq!(
            reports
                .iter()
                .map(|report| report.records)
                .collect::<Vec<_>>(),
            vec![0, 1, 2, 3],
            "every declared stream contributes its report, in order"
        );

        // The two lanes the old guard rejected still declare N > 2 streams
        // (Binance dropped the documented-not-observed USD-M `market` socket, E30).
        assert_eq!(crate::lane::BINANCE.feeds().len(), 3);
        assert_eq!(crate::lane::BYBIT.feeds().len(), 3);

        // Any one worker failing fails the whole lane (so the durable close runs).
        let error = join_lane_workers((0..4u64).map(|records| async move {
            if records == 2 {
                anyhow::bail!("stream worker {records} failed")
            } else {
                Ok(stream_report(records))
            }
        }))
        .await
        .expect_err("a worker failure fails the lane");
        assert!(error.to_string().contains("stream worker 2 failed"));
    }

    /// Joining zero workers is well defined (an empty report set); `run_streams`
    /// rejects an empty stream list before ever reaching the join.
    #[tokio::test(flavor = "current_thread")]
    async fn joining_zero_workers_yields_an_empty_report_set() {
        let workers: Vec<std::future::Ready<anyhow::Result<LiveReport>>> = Vec::new();
        let joined = join_lane_workers(workers)
            .await
            .expect("zero workers join into an empty report set");
        assert!(joined.is_empty());
    }
}
