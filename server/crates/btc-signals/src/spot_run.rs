//! The spot lane runner, lifted verbatim from the `btc-signals` binary so the
//! exchange process can drive it in-process; `src/main.rs` is now a thin
//! wrapper over [`cli_main`].

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
#[cfg(test)]
use btc_feeds::live::coordinate_live_shutdown_on;
use btc_feeds::live::{
    LiveControl, LiveDiagnostics, LiveError, LiveOptions, LiveReport, LiveShutdown, ProcessClock,
    TimedIngestOutcome, coordinate_live_shutdown, live_shutdown_channel, run_live,
};
use btc_feeds::{CaptureStream, FeedNotice, FeedNoticeCode, RecoveryAction};
use crate::checkpoint::{
    CheckpointDocument, CheckpointError, CheckpointWriteReport, CheckpointWriter,
    DEFAULT_CHECKPOINT_MAX_BYTES, DEFAULT_CHECKPOINT_QUEUE_CAPACITY, load_checkpoint,
};
use crate::exit_code::{ClassifiedMessage, ExitClass, classify_live, exit_class_for};
use crate::feed_health::{
    FEED_HEALTH_PERIODIC_INTERVAL_NS, FeedAvailability, FeedCadence, FeedContract, FeedHealthBoard,
    FeedHealthSnapshot,
};
use crate::file_generation::{
    FileGenerationCaps, FileGenerationError, FileGenerationLayout, OpenFileGeneration,
    RotationSchedule, RunMeta,
};
use crate::journal::{
    ContractFingerprint, DEFAULT_JOURNAL_QUEUE_CAPACITY, JournalEnd, JournalError, JournalInput,
    JournalReplayReader, JournalSession, JournalTotals, JournalWriteReport, SignalJournalWriter,
};
use crate::lane_gate::{IndexSocket, admit_lane};
use crate::stream_accounting::{
    InstanceLedgerEntry, InvalidatedTombstone, ProducerHealth, StreamAccounting,
    StreamAccountingError, StreamHook,
};
use crate::{
    HEALTH_INTERVAL_NS, SPOT_SIGNAL_KRAKEN_BOOK_DEPTH, SpotReferenceRuntimeConfig, SpotSignalError,
    SpotSignalProcessor, UnavailableReason, Venue,
};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use clap::{Parser, Subcommand};
use serde::Serialize;
#[cfg(test)]
use signals_core::{
    SIGNAL_HEARTBEAT_INTERVAL_MS, SIGNAL_SNAPSHOT_INTERVAL_MS, SignalStreamMessage,
};
use signals_core::{SPOT_LANE, SignalInstanceKey, SignalUpdate, SourceEvent, ValidationError};
use thiserror::Error;

const DEFAULT_MAX_UPDATES: u64 = 10_000_000;
const DEFAULT_MAX_BYTES: u64 = 2_147_483_648;
const DEFAULT_MAX_JOURNAL_BYTES: u64 = 4_294_967_296;
const DEFAULT_CHECKPOINT_INTERVAL_SECONDS: u64 = 5;
/// Explicit-mode default run duration when `--duration-seconds` is omitted.
const DEFAULT_EXPLICIT_DURATION_SECONDS: u64 = 30;
/// Lane identity for the spot runner's file-generation layout and RUN-META.
const SPOT_LANE_NAME: &str = "spot";
#[cfg(test)]
const SIGNAL_HEARTBEAT_INTERVAL_NS: u64 = SIGNAL_HEARTBEAT_INTERVAL_MS * 1_000_000;
#[cfg(test)]
const SIGNAL_SNAPSHOT_INTERVAL_NS: u64 = SIGNAL_SNAPSHOT_INTERVAL_MS * 1_000_000;
const MAX_TIMING_SAMPLES: usize = 1_000_000;
const MAX_RUN_SECONDS: u64 = 7 * 24 * 60 * 60;

#[derive(Debug, Parser)]
#[command(
    name = "btc-signals",
    about = "Deterministic BTC signal processing from normalized feeds"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run every currently buildable Coinbase/Kraken spot signal to private JSONL.
    #[command(visible_alias = "run-spot-reference")]
    RunSpotSignals {
        /// Explicit-mode output path. Mutually exclusive with `--lane-root`.
        #[arg(long, conflicts_with_all = ["lane_root", "rotate_seconds"], requires = "journal")]
        output: Option<PathBuf>,
        /// Explicit-mode journal path. Mutually exclusive with `--lane-root`.
        #[arg(long, conflicts_with_all = ["lane_root", "rotate_seconds"], requires = "output")]
        journal: Option<PathBuf>,
        /// Checkpoint path. Required in explicit mode; defaults to
        /// `<lane-root>/checkpoint.json` in rotation mode.
        #[arg(long)]
        checkpoint: Option<PathBuf>,
        /// Rotation-mode lane root. With `--rotate-seconds`, the binary owns the
        /// `gen-live-spot-<unix>` layout live-run.sh owns today and rotates file
        /// generations in-process. Mutually exclusive with `--output`/`--journal`.
        #[arg(long, requires = "rotate_seconds")]
        lane_root: Option<PathBuf>,
        /// Rotation period in seconds (>= 60). Selects rotation mode together
        /// with `--lane-root`.
        #[arg(long, requires = "lane_root", value_parser = clap::value_parser!(u64).range(60..))]
        rotate_seconds: Option<u64>,
        /// Headroom factor recorded in each generation's `RUN-META.json`
        /// (rotation mode only; operator metadata, matches live-run.sh).
        #[arg(long, default_value_t = 2)]
        headroom_factor: u64,
        /// Restore the checkpoint, rotate publisher_epoch, and replace it in place.
        #[arg(long)]
        resume_from_checkpoint: bool,
        #[arg(long)]
        publisher_epoch: String,
        /// Explicit mode: total run duration (default 30). Rotation mode:
        /// optional total bound; absent runs until SIGTERM.
        #[arg(long)]
        duration_seconds: Option<u64>,
        #[arg(long, default_value_t = DEFAULT_MAX_UPDATES)]
        max_updates: u64,
        #[arg(long, default_value_t = DEFAULT_MAX_BYTES)]
        max_bytes: u64,
        #[arg(long, default_value_t = DEFAULT_MAX_JOURNAL_BYTES)]
        max_journal_bytes: u64,
        #[arg(long, default_value_t = DEFAULT_JOURNAL_QUEUE_CAPACITY)]
        journal_queue_capacity: usize,
        #[arg(long, default_value_t = DEFAULT_CHECKPOINT_MAX_BYTES)]
        max_checkpoint_bytes: u64,
        #[arg(long, default_value_t = DEFAULT_CHECKPOINT_QUEUE_CAPACITY)]
        checkpoint_queue_capacity: usize,
        #[arg(long, default_value_t = DEFAULT_CHECKPOINT_INTERVAL_SECONDS)]
        checkpoint_interval_seconds: u64,
        #[arg(long, default_value_t = 100)]
        kraken_btc_book_depth: u16,
        /// Drop one eligible Coinbase trade and require REST gap recovery.
        #[arg(long)]
        induce_coinbase_trade_gap_once: bool,
        /// Force Coinbase reconnects at this diagnostic interval.
        #[arg(long)]
        coinbase_induced_reconnect_seconds: Option<u64>,
        /// Force Kraken reconnects at this diagnostic interval.
        #[arg(long)]
        kraken_induced_reconnect_seconds: Option<u64>,
    },
    /// Deterministically rebuild the complete spot-signal output from a journal.
    #[command(visible_alias = "replay-spot-reference")]
    ReplaySpotSignals {
        #[arg(long)]
        journal: PathBuf,
        #[arg(long)]
        output: PathBuf,
        #[arg(long, default_value_t = DEFAULT_MAX_UPDATES)]
        max_updates: u64,
        #[arg(long, default_value_t = DEFAULT_MAX_BYTES)]
        max_bytes: u64,
        #[arg(long, default_value_t = DEFAULT_MAX_JOURNAL_BYTES)]
        max_journal_bytes: u64,
    },
}

/// The CLI entry point the thin binary calls; parses argv and runs the command.
pub async fn cli_main() -> ExitCode {
    let outcome = match Cli::parse().command {
        Command::RunSpotSignals {
            output,
            journal,
            checkpoint,
            lane_root,
            rotate_seconds,
            headroom_factor,
            resume_from_checkpoint,
            publisher_epoch,
            duration_seconds,
            max_updates,
            max_bytes,
            max_journal_bytes,
            journal_queue_capacity,
            max_checkpoint_bytes,
            checkpoint_queue_capacity,
            checkpoint_interval_seconds,
            kraken_btc_book_depth,
            induce_coinbase_trade_gap_once,
            coinbase_induced_reconnect_seconds,
            kraken_induced_reconnect_seconds,
        } => {
            // The live spot lane subscribes Coinbase and Kraken, so it is gated
            // on both venue tokens (ADR 0026). Replay is not gated: it reads a
            // captured journal and subscribes to nothing.
            // The spot lane has no index socket.
            let admission = match admit_lane(&SPOT_LANE, IndexSocket::NotConfigured) {
                Ok(admission) => admission,
                Err(class) => return class.exit_code(),
            };
            eprintln!("{}", admission.startup_line());
            run_spot_signals(SpotRunArgs {
                output,
                journal,
                checkpoint,
                lane_root,
                rotate_seconds,
                headroom_factor,
                resume_from_checkpoint,
                publisher_epoch,
                duration_seconds,
                max_updates,
                max_bytes,
                max_journal_bytes,
                journal_queue_capacity,
                max_checkpoint_bytes,
                checkpoint_queue_capacity,
                checkpoint_interval_seconds,
                kraken_btc_book_depth,
                induce_coinbase_trade_gap_once,
                coinbase_induced_reconnect_seconds,
                kraken_induced_reconnect_seconds,
            })
            .await
        }
        Command::ReplaySpotSignals {
            journal,
            output,
            max_updates,
            max_bytes,
            max_journal_bytes,
        } => replay_spot_signals(journal, output, max_updates, max_bytes, max_journal_bytes),
    };
    match outcome {
        Ok(true) => ExitClass::Interrupted,
        Ok(false) => ExitClass::Success,
        Err(error) => {
            eprintln!("Error: {error:?}");
            exit_class_for(&error)
        }
    }
    .exit_code()
}

/// Parsed `run-spot-signals` arguments, threaded as one struct so the two run
/// modes (explicit paths, or in-process file-generation rotation) share one
/// entry point without a 20-argument function.
pub struct SpotRunArgs {
    pub output: Option<PathBuf>,
    pub journal: Option<PathBuf>,
    pub checkpoint: Option<PathBuf>,
    pub lane_root: Option<PathBuf>,
    pub rotate_seconds: Option<u64>,
    pub headroom_factor: u64,
    pub resume_from_checkpoint: bool,
    pub publisher_epoch: String,
    pub duration_seconds: Option<u64>,
    pub max_updates: u64,
    pub max_bytes: u64,
    pub max_journal_bytes: u64,
    pub journal_queue_capacity: usize,
    pub max_checkpoint_bytes: u64,
    pub checkpoint_queue_capacity: usize,
    pub checkpoint_interval_seconds: u64,
    pub kraken_btc_book_depth: u16,
    pub induce_coinbase_trade_gap_once: bool,
    pub coinbase_induced_reconnect_seconds: Option<u64>,
    pub kraken_induced_reconnect_seconds: Option<u64>,
}

/// The two mutually exclusive spot run modes.
enum SpotRunMode {
    /// Fixed output/journal/checkpoint paths (smokes, replay, tests) — unchanged.
    Explicit {
        output: PathBuf,
        journal: PathBuf,
        checkpoint: PathBuf,
    },
    /// In-process file-generation rotation under a lane root (C5).
    Rotation {
        lane_root: PathBuf,
        rotate_seconds: u64,
        checkpoint: PathBuf,
    },
}

impl SpotRunMode {
    /// Resolve the mode from the mutually exclusive flag sets. clap already
    /// rejects mixing the two sets and enforces `--rotate-seconds >= 60`; this
    /// applies the "one mode, fully specified" rule and the rotation-mode
    /// checkpoint default (`<lane-root>/checkpoint.json`).
    fn resolve(
        output: Option<PathBuf>,
        journal: Option<PathBuf>,
        checkpoint: Option<PathBuf>,
        lane_root: Option<PathBuf>,
        rotate_seconds: Option<u64>,
    ) -> Result<Self> {
        match (lane_root, rotate_seconds) {
            (Some(lane_root), Some(rotate_seconds)) => {
                let checkpoint = checkpoint.unwrap_or_else(|| lane_root.join("checkpoint.json"));
                Ok(Self::Rotation {
                    lane_root,
                    rotate_seconds,
                    checkpoint,
                })
            }
            (None, None) => {
                let output = output.context(
                    "run-spot-signals requires either --lane-root with --rotate-seconds (rotation mode) or --output with --journal and --checkpoint (explicit mode)",
                )?;
                let journal =
                    journal.context("explicit mode requires --journal alongside --output")?;
                let checkpoint = checkpoint.context("explicit mode requires --checkpoint")?;
                Ok(Self::Explicit {
                    output,
                    journal,
                    checkpoint,
                })
            }
            _ => anyhow::bail!(
                "--lane-root and --rotate-seconds must be given together to select rotation mode"
            ),
        }
    }

    fn checkpoint_path(&self) -> &Path {
        match self {
            Self::Explicit { checkpoint, .. } | Self::Rotation { checkpoint, .. } => checkpoint,
        }
    }
}

/// State the rotation timer carries across generations. The processor, rings and
/// process clock live in the pipeline and are never rebuilt here; this owns only
/// the file-generation layout, schedule, the currently open generation and the
/// per-generation writer configuration.
struct RotationController {
    layout: FileGenerationLayout,
    schedule: RotationSchedule,
    current: OpenFileGeneration,
    wall_start: DateTime<Utc>,
    epoch_prefix: String,
    rotate_period_secs: u64,
    caps: FileGenerationCaps,
    headroom_factor: u64,
    checkpoint_path: PathBuf,
    journal_queue_capacity: usize,
    max_checkpoint_bytes: u64,
    checkpoint_queue_capacity: usize,
    runtime_config: SpotReferenceRuntimeConfig,
}

/// The output file and durability writers for one file generation.
struct OpenGenerationWriters {
    output_file: File,
    durability: LiveDurability,
}

/// Build a generation's output/journal/checkpoint writers. Shared by the first
/// generation and every rotation so the two paths open writers identically.
#[allow(clippy::too_many_arguments)]
fn open_generation_writers(
    output_path: &Path,
    journal_path: &Path,
    checkpoint_path: &Path,
    session: &JournalSession,
    initial_checkpoint: CheckpointDocument,
    max_journal_bytes: u64,
    journal_queue_capacity: usize,
    max_checkpoint_bytes: u64,
    checkpoint_queue_capacity: usize,
    replace_checkpoint: bool,
    runtime_config: &SpotReferenceRuntimeConfig,
) -> Result<OpenGenerationWriters> {
    let output_file = create_private_output(output_path, "signal output")?;
    let journal = SignalJournalWriter::create(
        journal_path,
        session.clone(),
        max_journal_bytes,
        journal_queue_capacity,
    )
    .with_context(|| {
        format!(
            "failed to create signal journal {}; existing files are never overwritten",
            journal_path.display()
        )
    })?;
    let checkpoint = CheckpointWriter::create(
        checkpoint_path,
        initial_checkpoint,
        max_checkpoint_bytes,
        checkpoint_queue_capacity,
        replace_checkpoint,
    )
    .with_context(|| {
        format!(
            "failed to initialize checkpoint {}; fresh runs never overwrite an existing checkpoint",
            checkpoint_path.display()
        )
    })?;
    let checkpoint_context = CheckpointContext {
        session_id: session.session_id.clone(),
        wall_start: session.wall_start.clone(),
        contract_fingerprint: session.contract_fingerprint.clone(),
        runtime_config: runtime_config.clone(),
    };
    Ok(OpenGenerationWriters {
        output_file,
        durability: LiveDurability {
            journal,
            checkpoint,
            checkpoint_context,
        },
    })
}

/// The `(unix-seconds, RUN-META `started_utc`)` stamp for a generation event at
/// process-clock `now_ns`, derived from the process wall-start so the directory
/// suffix and metadata timestamp agree and advance monotonically with the clock.
///
/// Using the wall-start plus elapsed (rather than a fresh `SystemTime::now`)
/// keeps the suffix monotonic and — because rotations are always at least
/// `MIN_ROTATION_PERIOD_SECS` apart — collision-free, while staying within a
/// second of real wall time in production. It also makes the layout fully
/// determined by the process clock, which is what lets a replay/rotation test
/// drive it without racing the wall clock.
fn generation_stamps(wall_start: DateTime<Utc>, now_ns: u64) -> (u64, String) {
    let instant =
        wall_start + ChronoDuration::nanoseconds(i64::try_from(now_ns).unwrap_or(i64::MAX));
    let unix = u64::try_from(instant.timestamp()).unwrap_or(0);
    let utc = instant.format("%Y-%m-%dT%H:%M:%SZ").to_string();
    (unix, utc)
}

/// Build a spot generation's `RUN-META` payload.
fn spot_run_meta(
    publisher_epoch: &str,
    now_unix: u64,
    started_utc: &str,
    rotate_period_secs: u64,
    caps: &FileGenerationCaps,
    headroom_factor: u64,
    resumed: bool,
) -> RunMeta {
    RunMeta {
        lane: SPOT_LANE_NAME.to_owned(),
        publisher_epoch: publisher_epoch.to_owned(),
        generation: format!("gen-live-{SPOT_LANE_NAME}-{now_unix}"),
        started_utc: started_utc.to_owned(),
        duration_seconds: rotate_period_secs,
        max_output_bytes: caps.max_output_bytes,
        max_journal_bytes: caps.max_journal_bytes,
        max_updates: caps.max_updates,
        headroom_factor,
        resumed,
    }
}

/// The RELATIVE time to sleep before the next rotation deadline, measured
/// against the process clock's own `elapsed_ns()` frame.
///
/// `next_deadline()` lives in the process clock's logical `elapsed_ns` frame (it
/// is seeded from `elapsed_ns()` at the first open). `ProcessClock::deadline` is
/// monotonic-relative and ignores the resumed logical offset, so on a
/// `--resume-from-checkpoint` run (every restart after the first) feeding the
/// absolute deadline into it would delay the first rotation by the whole logical
/// offset — hours in production. This subtracts the current `elapsed_ns()` from
/// the absolute deadline instead, so the wait is one period regardless of any
/// resumed offset. It is the single source of the rotation sleep duration:
/// [`run_rotation_timer`] and the deadline regression test both call it, so the
/// test cannot pass against a reverted (absolute-deadline) implementation.
fn rotation_sleep_remaining(clock: &ProcessClock, deadline_ns: u64) -> Result<Duration, LiveError> {
    let elapsed_ns = clock.elapsed_ns()?;
    Ok(Duration::from_nanos(deadline_ns.saturating_sub(elapsed_ns)))
}

/// Drive in-process file-generation rotation alongside the live feeds. In
/// explicit mode (`controller` is `None`) it simply waits for the coordinated
/// shutdown and returns. In rotation mode it rotates at each period boundary and,
/// on any failure to seal-and-reopen, records the error (with the layout's exit
/// class) and stops the run rather than continuing to write into finalized files.
async fn run_rotation_timer(
    pipeline: Rc<RefCell<SignalPipeline<BufWriter<File>>>>,
    clock: ProcessClock,
    mut shutdown: LiveShutdown,
    controller: Option<Rc<RefCell<RotationController>>>,
    rotation_failure: Rc<RefCell<Option<anyhow::Error>>>,
) -> Result<(), LiveError> {
    let Some(controller) = controller else {
        shutdown.wait_reason().await.map_err(LiveError::Signal)?;
        return Ok(());
    };
    loop {
        // Recomputed each iteration, the relative remaining time is
        // offset-independent and self-correcting (see `rotation_sleep_remaining`).
        let deadline_ns = controller.borrow().schedule.next_deadline();
        let remaining = rotation_sleep_remaining(&clock, deadline_ns)
            .map_err(|error| LiveError::Publication(error.to_string()))?;
        tokio::select! {
            reason = shutdown.wait_reason() => {
                reason.map_err(LiveError::Signal)?;
                return Ok(());
            }
            _ = tokio::time::sleep(remaining) => {
                let now_ns = clock
                    .elapsed_ns()
                    .map_err(|error| LiveError::Publication(error.to_string()))?;
                if let Err(error) = rotate_generation(&pipeline, &controller, &clock, now_ns) {
                    *rotation_failure.borrow_mut() = Some(error);
                    return Err(LiveError::Publication(
                        "spot file-generation rotation failed".to_owned(),
                    ));
                }
            }
        }
    }
}

/// Seal the current generation and open the next, in place, keeping the warm
/// processor/rings/clock (C5). Runs entirely synchronously under one borrow of
/// the pipeline and controller so it never yields with either borrowed.
fn rotate_generation(
    pipeline: &Rc<RefCell<SignalPipeline<BufWriter<File>>>>,
    controller: &Rc<RefCell<RotationController>>,
    clock: &ProcessClock,
    now_ns: u64,
) -> Result<()> {
    let mut ctrl = controller.borrow_mut();
    let mut pipe = pipeline.borrow_mut();

    // 1. Seal the current generation. Capture its checkpoint document first (the
    //    next generation's journal embeds it), then run today's graceful-end
    //    sequence as a seal and stamp RUN-META `sealed_utc`.
    let sealed_doc = pipe.checkpoint_document(now_ns)?;
    pipe.sync_output()?;
    pipe.complete_journal()?;
    pipe.complete_checkpoint(now_ns)?;
    let (_, sealed_utc) = generation_stamps(ctrl.wall_start, now_ns);
    ctrl.current.seal(sealed_utc)?;
    let previous_dir = ctrl
        .current
        .dir
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
        .context("current file generation directory has no name")?;

    // 2. Fresh output identity, named from the SCHEDULED deadline second (the
    //    RotationSchedule grid, `first_open_ns + n·period` mapped to unix), NOT
    //    the observed post-seal instant: a seal overrunning into the next wall
    //    second would otherwise shift the generation name onto a second an
    //    adjacent rotation could also produce, risking `GenerationExists` on
    //    same-second names. Consecutive fired deadlines are one period apart, so
    //    grid-named generations are always distinct and ≥ one period apart.
    //    RUN-META's `started_utc` keeps the OBSERVED open time.
    let scheduled_ns = ctrl.schedule.next_deadline();
    let (now_unix, _) = generation_stamps(ctrl.wall_start, scheduled_ns);
    let (_, started_utc) = generation_stamps(ctrl.wall_start, now_ns);
    let new_epoch = format!("{}-{now_unix}", ctrl.epoch_prefix);

    // 3. Rotate the warm processor to the new output epoch (rings stay warm).
    pipe.rotate_output_epoch(&new_epoch)?;

    // 4. Open the next generation directory, RUN-META and CURRENT.
    let meta = spot_run_meta(
        &new_epoch,
        now_unix,
        &started_utc,
        ctrl.rotate_period_secs,
        &ctrl.caps,
        ctrl.headroom_factor,
        true,
    );
    let period_secs = ctrl.rotate_period_secs;
    let next =
        match ctrl
            .layout
            .open_next_with_rotation(now_unix, Some(&previous_dir), &meta, period_secs)
        {
            Ok(next) => next,
            // A same-second generation name already exists. On the rotation path this
            // is transient — the current generation is already sealed on disk, and
            // the next boundary yields a fresh second-stamped name — so it seals and
            // exits Retryable (75) rather than holding the lane down under a Fatal
            // (78). This maps only the rotation path: the startup open of the first
            // generation keeps its Fatal class (a cold start into an occupied lane is
            // an operator error, never a blind restart).
            Err(error @ FileGenerationError::GenerationExists { .. }) => {
                return Err(anyhow::Error::new(ClassifiedMessage::new(
                    ExitClass::Retryable,
                    format!("rotation could not open the next file generation: {error}"),
                )));
            }
            Err(error) => return Err(error.into()),
        };

    // 5. Build the next generation's writers. Its journal embeds the sealed
    //    checkpoint so an independent replay restores from it (the warm rings are
    //    not checkpointed and re-warm honestly on that replay); the checkpoint
    //    writer resumes the shared checkpoint file in place.
    let mut session =
        JournalSession::resumed(new_epoch.clone(), sealed_doc, ctrl.runtime_config.clone())?;
    // Mark the new generation as rotation-opened so a cold replay's terminal
    // count self-check reports the ring re-warm difference as the documented gap
    // rather than failing (C5).
    session.opened_by_rotation = true;
    let initial_checkpoint = CheckpointDocument::new(
        session.session_id.clone(),
        session.wall_start.clone(),
        now_ns,
        0,
        session.contract_fingerprint.clone(),
        ctrl.runtime_config.clone(),
        pipe.processor().checkpoint_state(),
    )?;
    let writers = open_generation_writers(
        &next.output_path,
        &next.journal_path,
        &ctrl.checkpoint_path,
        &session,
        initial_checkpoint,
        ctrl.caps.max_journal_bytes,
        ctrl.journal_queue_capacity,
        ctrl.max_checkpoint_bytes,
        ctrl.checkpoint_queue_capacity,
        true,
        &ctrl.runtime_config,
    )?;

    // 6. Install the new generation; the warm processor is untouched, every
    //    fresh-process stream field resets so the generation opens with a reset
    //    frame at sequence 0 under the new epoch.
    pipe.begin_generation(BufWriter::new(writers.output_file), writers.durability);

    // 6b. Admit the book resync as the first records of the new generation so its
    //     journal stands alone: a cold replay rebuilds the Coinbase and Kraken
    //     books the checkpoint does not carry before the first live delta arrives.
    //     On this warm live process the rows are ordering-skipped and change
    //     nothing (value-neutral); the tagged records are counted in the new
    //     generation's admission sequence and journaled.
    pipe.admit_book_resync(now_ns)
        .context("failed to admit the rotation book resync")?;

    // 7. Skip the schedule past every boundary already reached (never replay a
    //    missed one), and adopt the new generation as current. `advance_to` must
    //    see the POST-seal clock, not the pre-seal deadline `now_ns`: a slow seal
    //    can push the clock several periods past the deadline, and skipping only
    //    to the pre-seal instant would rotate back-to-back to "catch up", opening
    //    (and immediately sealing) empty generations and risking `GenerationExists`
    //    on same-second names (see `RotationSchedule::advance_to`).
    let post_seal_ns = clock
        .elapsed_ns()
        .map(|elapsed| elapsed.max(now_ns))
        .unwrap_or(now_ns);
    // Floor the skip at the just-fired deadline so the schedule always advances
    // past it: the next rotation's `next_deadline()` is then strictly later,
    // guaranteeing the grid-named generations never collide. In production
    // `post_seal_ns >= scheduled_ns` already (we slept until the deadline), so
    // this floor only matters when a caller rotates before the deadline.
    ctrl.schedule.advance_to(post_seal_ns.max(scheduled_ns));
    ctrl.current = next;
    Ok(())
}

pub async fn run_spot_signals(args: SpotRunArgs) -> Result<bool> {
    run_spot_signals_with_hook(args, None).await
}

/// [`run_spot_signals`] with an in-process observer of every stream message
/// the run writes ([`StreamHook`]); the hook persists across generation
/// rotation. An embedding process (the exchange) uses this to consume the
/// lane's output without tailing its files.
pub async fn run_spot_signals_with_hook(args: SpotRunArgs, stream_hook: Option<StreamHook>) -> Result<bool> {
    let SpotRunArgs {
        output,
        journal,
        checkpoint,
        lane_root,
        rotate_seconds,
        headroom_factor,
        resume_from_checkpoint,
        publisher_epoch,
        duration_seconds,
        max_updates,
        max_bytes,
        max_journal_bytes,
        journal_queue_capacity,
        max_checkpoint_bytes,
        checkpoint_queue_capacity,
        checkpoint_interval_seconds,
        kraken_btc_book_depth,
        induce_coinbase_trade_gap_once,
        coinbase_induced_reconnect_seconds,
        kraken_induced_reconnect_seconds,
    } = args;

    validate_limits(max_updates, max_bytes, max_journal_bytes)?;
    anyhow::ensure!(
        max_checkpoint_bytes > 0,
        "max_checkpoint_bytes must be greater than zero"
    );
    anyhow::ensure!(
        checkpoint_interval_seconds > 0,
        "checkpoint_interval_seconds must be greater than zero"
    );
    anyhow::ensure!(
        kraken_btc_book_depth == SPOT_SIGNAL_KRAKEN_BOOK_DEPTH,
        "the current spot-signal contracts require Kraken book depth {}",
        SPOT_SIGNAL_KRAKEN_BOOK_DEPTH
    );

    // Resolve the run mode from the mutually exclusive flag sets. clap already
    // rejects mixing `--lane-root`/`--rotate-seconds` with `--output`/`--journal`
    // and enforces `--rotate-seconds >= 60`; this resolves the remaining
    // "one mode, fully specified" rule and the rotation-mode checkpoint default.
    let mode = SpotRunMode::resolve(output, journal, checkpoint, lane_root, rotate_seconds)?;

    // `--duration-seconds` is a total bound. Explicit mode keeps the historical
    // 30 s default; rotation mode treats it as optional (absent = run until
    // SIGTERM, bounded only by the hard `MAX_RUN_SECONDS` ceiling).
    let duration_seconds = match duration_seconds {
        Some(seconds) => {
            anyhow::ensure!(
                (1..=MAX_RUN_SECONDS).contains(&seconds),
                "duration_seconds must be between 1 and {MAX_RUN_SECONDS}"
            );
            seconds
        }
        None => match mode {
            SpotRunMode::Explicit { .. } => DEFAULT_EXPLICIT_DURATION_SECONDS,
            SpotRunMode::Rotation { .. } => MAX_RUN_SECONDS,
        },
    };
    for (name, interval) in [
        (
            "coinbase_induced_reconnect_seconds",
            coinbase_induced_reconnect_seconds,
        ),
        (
            "kraken_induced_reconnect_seconds",
            kraken_induced_reconnect_seconds,
        ),
    ] {
        if let Some(interval) = interval {
            anyhow::ensure!(
                interval > 0 && interval < duration_seconds,
                "{name} must be positive and shorter than duration_seconds"
            );
        }
    }

    let runtime_config = SpotReferenceRuntimeConfig::new(kraken_btc_book_depth)
        .context("spot-signal runtime configuration is invalid")?;
    let checkpoint_path = mode.checkpoint_path().to_path_buf();
    let PreparedSpotSignalRun {
        clock,
        processor,
        session,
        initial_checkpoint,
    } = prepare_spot_signal_run(
        &checkpoint_path,
        resume_from_checkpoint,
        publisher_epoch.clone(),
        max_checkpoint_bytes,
        runtime_config.clone(),
    )?;

    // Open the first file generation. In rotation mode the binary owns the
    // `gen-live-spot-<unix>` layout live-run.sh owns today (directory, RUN-META,
    // CURRENT); in explicit mode the caller supplied the paths directly.
    let caps = FileGenerationCaps {
        max_updates,
        max_output_bytes: max_bytes,
        max_journal_bytes,
    };
    let mut rotation_seed: Option<(
        FileGenerationLayout,
        RotationSchedule,
        OpenFileGeneration,
        u64,
    )> = None;
    let (output_path, journal_path) = match &mode {
        SpotRunMode::Explicit {
            output, journal, ..
        } => (output.clone(), journal.clone()),
        SpotRunMode::Rotation {
            lane_root,
            rotate_seconds,
            ..
        } => {
            let layout = FileGenerationLayout::new(lane_root, SPOT_LANE_NAME)
                .context("spot lane root is not usable for file-generation rotation")?;
            let first_open_ns = clock.elapsed_ns()?;
            let (now_unix, started_utc) = generation_stamps(clock.wall_start(), first_open_ns);
            let meta = spot_run_meta(
                &publisher_epoch,
                now_unix,
                &started_utc,
                *rotate_seconds,
                &caps,
                headroom_factor,
                resume_from_checkpoint,
            );
            let generation = layout
                .open_next_with_rotation(now_unix, None, &meta, *rotate_seconds)
                .context("failed to open the first spot file generation")?;
            let paths = (
                generation.output_path.clone(),
                generation.journal_path.clone(),
            );
            let schedule =
                RotationSchedule::try_new(first_open_ns, Duration::from_secs(*rotate_seconds))?;
            rotation_seed = Some((layout, schedule, generation, *rotate_seconds));
            paths
        }
    };
    if let SpotRunMode::Explicit { .. } = &mode {
        anyhow::ensure!(
            output_path != journal_path
                && output_path != checkpoint_path
                && journal_path != checkpoint_path,
            "output, journal, and checkpoint paths must differ"
        );
    }

    let writers = open_generation_writers(
        &output_path,
        &journal_path,
        &checkpoint_path,
        &session,
        initial_checkpoint,
        max_journal_bytes,
        journal_queue_capacity,
        max_checkpoint_bytes,
        checkpoint_queue_capacity,
        resume_from_checkpoint,
        &runtime_config,
    )?;
    let pipeline = Rc::new(RefCell::new(
        SignalPipeline::new_live(
            processor,
            clock.wall_start(),
            BufWriter::new(writers.output_file),
            max_updates,
            max_bytes,
            writers.durability,
        )
        .with_feed_health_stream(runtime_config.feed_health_stream)
        .with_stream_hook(stream_hook),
    ));

    if resume_from_checkpoint {
        let now_ns = clock.elapsed_ns()?;
        mark_resumed_sources_unavailable(&mut pipeline.borrow_mut(), now_ns)?;
    }

    // In rotation mode a controller drives the in-place seal/reopen at each
    // period boundary; a failure to open the next generation is recorded here so
    // the graceful end can surface it with the layout's own exit class.
    let rotation_failure: Rc<RefCell<Option<anyhow::Error>>> = Rc::new(RefCell::new(None));
    let rotation_controller = rotation_seed.map(|(layout, schedule, current, period_secs)| {
        Rc::new(RefCell::new(RotationController {
            layout,
            schedule,
            current,
            wall_start: clock.wall_start(),
            epoch_prefix: publisher_epoch.clone(),
            rotate_period_secs: period_secs,
            caps,
            headroom_factor,
            checkpoint_path: checkpoint_path.clone(),
            journal_queue_capacity,
            max_checkpoint_bytes,
            checkpoint_queue_capacity,
            runtime_config: runtime_config.clone(),
        }))
    });

    let duration = Duration::from_secs(duration_seconds);
    let deadline = clock.deadline(duration);
    let (shutdown_trigger, shutdown) = live_shutdown_channel();
    let coinbase_diagnostics = LiveDiagnostics {
        induce_coinbase_trade_gap_once,
        induced_reconnect_interval: coinbase_induced_reconnect_seconds.map(Duration::from_secs),
    };
    let kraken_diagnostics = LiveDiagnostics {
        induce_coinbase_trade_gap_once: false,
        induced_reconnect_interval: kraken_induced_reconnect_seconds.map(Duration::from_secs),
    };
    let coinbase_pipeline = Rc::clone(&pipeline);
    let kraken_pipeline = Rc::clone(&pipeline);
    let health_pipeline = Rc::clone(&pipeline);
    let checkpoint_pipeline = Rc::clone(&pipeline);
    let rotation_pipeline = Rc::clone(&pipeline);
    let coinbase_clock = clock.clone();
    let kraken_clock = clock.clone();
    let health_clock = clock.clone();
    let checkpoint_clock = clock.clone();
    let rotation_clock = clock.clone();
    let health_shutdown = shutdown.clone();
    let checkpoint_shutdown = shutdown.clone();
    let rotation_shutdown = shutdown.clone();
    let rotation_failure_timer = Rc::clone(&rotation_failure);
    let checkpoint_interval = Duration::from_secs(checkpoint_interval_seconds);
    let run_result = tokio::try_join!(
        run_live(
            LiveOptions {
                stream: CaptureStream::CoinbaseCore,
                duration,
                kraken_btc_book_depth,
                publisher_epoch: publisher_epoch.clone(),
                diagnostics: coinbase_diagnostics,
                shutdown: Some(shutdown.clone()),
                kraken_trade_reconciliation: None,
            },
            clock.clone(),
            move |timed| {
                handle_live_outcome(&coinbase_pipeline, Venue::Coinbase, timed, &coinbase_clock)
            },
        ),
        run_live(
            LiveOptions {
                stream: CaptureStream::KrakenCore,
                duration,
                kraken_btc_book_depth,
                publisher_epoch,
                diagnostics: kraken_diagnostics,
                shutdown: Some(shutdown.clone()),
                kraken_trade_reconciliation: None,
            },
            clock.clone(),
            move |timed| {
                handle_live_outcome(&kraken_pipeline, Venue::Kraken, timed, &kraken_clock)
            },
        ),
        run_health_timer(health_pipeline, health_clock, duration, health_shutdown),
        run_checkpoint_timer(
            checkpoint_pipeline,
            checkpoint_clock,
            duration,
            checkpoint_interval,
            checkpoint_shutdown,
        ),
        run_rotation_timer(
            rotation_pipeline,
            rotation_clock,
            rotation_shutdown,
            rotation_controller.clone(),
            rotation_failure_timer,
        ),
        async move {
            coordinate_live_shutdown(deadline, shutdown_trigger)
                .await
                .map_err(LiveError::Signal)
        },
    );

    let mut pipeline = Rc::try_unwrap(pipeline)
        .map_err(|_| anyhow::anyhow!("signal pipeline still has an active owner"))?
        .into_inner();

    // A failed next-generation open seals what already exists (the previous
    // generation's writers are finalized before the open is attempted) and then
    // stops the run. Surface it with the file-generation layout's own exit class
    // rather than the generic live-run failure, and never fall through to writing
    // more into the now-finalized generation.
    if let Some(rotation_error) = rotation_failure.borrow_mut().take() {
        let _ = pipeline.report();
        return Err(rotation_error);
    }

    let output_result = pipeline.sync_output();
    let may_complete = run_result.is_ok() && output_result.is_ok();
    let journal_result = if may_complete {
        pipeline.complete_journal()
    } else {
        pipeline.abort_journal()
    };
    let checkpoint_result = if may_complete {
        match clock.elapsed_ns() {
            Ok(now_ns) => pipeline.complete_checkpoint(now_ns),
            Err(error) => Err(PipelineError::Mode(format!(
                "final checkpoint clock failed: {error}"
            ))),
        }
    } else {
        pipeline.abort_checkpoint()
    };
    let signal_result = pipeline.report();

    let mut failures = Vec::new();
    let mut exit_class = ExitClass::Success;
    let live_reports = match run_result {
        Ok((coinbase, kraken, (), (), (), shutdown_reason)) => {
            Some((coinbase, kraken, shutdown_reason))
        }
        Err(error) => {
            exit_class = exit_class.worst(classify_live(&error));
            failures.push(format!("live run: {error}"));
            None
        }
    };
    if let Err(error) = &output_result {
        exit_class = exit_class.worst(ExitClass::Fatal);
        failures.push(format!("signal output finalization: {error}"));
    }
    let journal_report = match journal_result {
        Ok(report) => Some(report),
        Err(error) => {
            exit_class = exit_class.worst(classify_pipeline(&error));
            failures.push(format!("signal journal finalization: {error}"));
            None
        }
    };
    let checkpoint_report = match checkpoint_result {
        Ok(report) => Some(report),
        Err(error) => {
            exit_class = exit_class.worst(classify_pipeline(&error));
            failures.push(format!("signal checkpoint finalization: {error}"));
            None
        }
    };
    let signal_report = match signal_result {
        Ok(report) => Some(report),
        Err(error) => {
            exit_class = exit_class.worst(classify_pipeline(&error));
            failures.push(format!("signal report: {error}"));
            None
        }
    };
    if !failures.is_empty() {
        return Err(anyhow::Error::new(ClassifiedMessage::new(
            exit_class,
            format!(
                "spot-signal run did not complete; {}. The journal has no trusted completion status unless its terminal record was synced successfully",
                failures.join("; ")
            ),
        )));
    }

    // Rotation mode: seal the final (still-open) generation's RUN-META so its
    // `sealed_utc` is set exactly as it is for every rotated-past generation. A
    // SIGTERM or duration end reaches here just like a rotation boundary does.
    if let Some(controller) = &rotation_controller {
        let now_ns = clock.elapsed_ns().unwrap_or(0);
        let mut controller = controller.borrow_mut();
        let (_, sealed_utc) = generation_stamps(controller.wall_start, now_ns);
        controller
            .current
            .seal(sealed_utc)
            .context("failed to seal the final spot file generation")?;
    }

    let Some((coinbase, kraken, shutdown_reason)) = live_reports else {
        anyhow::bail!("live reports were unavailable after a successful run")
    };
    let Some(signal) = signal_report else {
        anyhow::bail!("signal report was unavailable after a successful run")
    };
    let Some(journal) = journal_report else {
        anyhow::bail!("journal report was unavailable after a successful run")
    };
    let Some(checkpoint) = checkpoint_report else {
        anyhow::bail!("checkpoint report was unavailable after a successful run")
    };
    let interrupted = shutdown_reason.interrupted();
    let report = SpotSignalRunReport {
        session_id: session.session_id,
        contract_fingerprint: session.contract_fingerprint.aggregate_sha256,
        coinbase,
        kraken,
        signal,
        journal,
        checkpoint,
    };
    println!(
        "{}",
        serde_json::to_string(&report).context("report serialization failed")?
    );
    Ok(interrupted)
}

struct PreparedSpotSignalRun {
    clock: ProcessClock,
    processor: SpotSignalProcessor,
    session: JournalSession,
    initial_checkpoint: CheckpointDocument,
}

fn prepare_spot_signal_run(
    checkpoint_path: &Path,
    resume_from_checkpoint: bool,
    publisher_epoch: String,
    max_checkpoint_bytes: u64,
    runtime_config: SpotReferenceRuntimeConfig,
) -> Result<PreparedSpotSignalRun> {
    let (clock, processor, session) = if resume_from_checkpoint {
        let checkpoint = load_checkpoint(checkpoint_path, max_checkpoint_bytes)
            .with_context(|| format!("failed to load checkpoint {}", checkpoint_path.display()))?;
        anyhow::ensure!(
            checkpoint
                .runtime_config
                .matches_ignoring_feed_health(&runtime_config),
            "checkpoint runtime configuration does not match the requested run"
        );
        let wall_start = DateTime::parse_from_rfc3339(&checkpoint.wall_start)
            .context("checkpoint wall_start is invalid")?
            .with_timezone(&Utc);
        let clock = ProcessClock::resume(wall_start, checkpoint.checkpoint_time_ns)
            .context("checkpoint logical clock cannot be resumed safely")?;
        let processor = SpotSignalProcessor::restore(
            &publisher_epoch,
            wall_start,
            checkpoint.processor_state.clone(),
        )
        .context("checkpoint processor state is incompatible")?;
        let session =
            JournalSession::resumed(publisher_epoch.clone(), checkpoint, runtime_config.clone())
                .context("failed to construct a resumed signal journal session")?;
        (clock, processor, session)
    } else {
        let clock = ProcessClock::start();
        let processor = SpotSignalProcessor::new(&publisher_epoch, clock.wall_start())
            .context("spot-signal processor configuration is invalid")?;
        let session = JournalSession::new(
            clock.wall_start_text(),
            publisher_epoch.clone(),
            runtime_config.clone(),
        )
        .context("failed to construct the signal journal session")?;
        (clock, processor, session)
    };

    let initial_checkpoint = CheckpointDocument::new(
        session.session_id.clone(),
        session.wall_start.clone(),
        clock.elapsed_ns()?,
        0,
        session.contract_fingerprint.clone(),
        runtime_config.clone(),
        processor.checkpoint_state(),
    )
    .context("failed to construct the initial checkpoint")?;
    Ok(PreparedSpotSignalRun {
        clock,
        processor,
        session,
        initial_checkpoint,
    })
}

/// Verify a replay's terminal counts against the journal's terminal record and
/// return the documented ring re-warm gap (`Some(updates)` for a rotation-opened
/// generation, `None` for a fresh-start one).
///
/// For a fresh-start generation the replay reproduces the terminal counts
/// exactly, so any difference is a fault. For a rotation-opened generation
/// (C5) the checkpoint carries every non-ring identity but no ring state, so the
/// rings re-warm from scratch and emit fewer ready updates than the warm live
/// run did. That difference is tolerated, but bounded on every axis:
///
/// * `updates_written`: bounded ABOVE by the warm run (a re-warm can only omit
///   ring updates, never invent readiness) and BELOW by one (a rotation replay
///   that produced nothing rebuilt no book and is broken). The tight per-identity
///   floor — every non-ring identity's Ready updates reproduced byte-for-byte —
///   is asserted by the rotation replay tests, which have the live per-identity
///   output the aggregate terminal record here does not carry.
/// * `stream_messages`: bounded BELOW by the warm run — `spot_reference` is
///   checkpointed, so no decision batch is empty and none may be missing; the
///   re-warm only ADDS warming-up transitions.
/// * quality-transition counters: included, not ignored. A cold re-warm only
///   ADDS warming-up transitions, so the replay's total may only EXCEED or equal
///   the warm run's (ring-only tolerance). A replay with FEWER quality
///   transitions than the warm run dropped a decision's quality state.
fn verify_replay_counts(
    opened_by_rotation: bool,
    replay_updates: u64,
    replay_stream_messages: u64,
    replay_quality: &std::collections::BTreeMap<String, u64>,
    terminal_updates: u64,
    terminal_stream_messages: u64,
    terminal_quality: &std::collections::BTreeMap<String, u64>,
) -> Result<Option<u64>> {
    let replay_quality_total: u64 = replay_quality.values().copied().sum();
    let terminal_quality_total: u64 = terminal_quality.values().copied().sum();
    if opened_by_rotation {
        anyhow::ensure!(
            replay_updates <= terminal_updates,
            "rotated-generation replay wrote more ready updates ({replay_updates}) than the warm live run ({terminal_updates}); a re-warm can only omit ring updates"
        );
        anyhow::ensure!(
            replay_updates > 0,
            "rotated-generation replay produced no ready updates; the book resync rebuilt nothing"
        );
        anyhow::ensure!(
            replay_stream_messages >= terminal_stream_messages,
            "rotated-generation replay dropped a decision: warm live wrote {terminal_stream_messages} stream messages, replay only {replay_stream_messages}"
        );
        anyhow::ensure!(
            replay_quality_total >= terminal_quality_total,
            "rotated-generation replay recorded FEWER quality transitions ({replay_quality_total}) than the warm live run ({terminal_quality_total}); a re-warm only adds warming-up transitions, so a shortfall means a decision's quality state went missing"
        );
        return Ok(Some(terminal_updates - replay_updates));
    }
    if replay_updates != terminal_updates {
        anyhow::bail!(
            "replay update count mismatch: terminal {terminal_updates}, replay {replay_updates}"
        );
    }
    if replay_stream_messages != terminal_stream_messages {
        anyhow::bail!(
            "replay stream-message count mismatch: terminal {terminal_stream_messages}, replay {replay_stream_messages}"
        );
    }
    if replay_quality != terminal_quality {
        anyhow::bail!(
            "replay quality-transition counters do not match the journal terminal record"
        );
    }
    Ok(None)
}

pub fn replay_spot_signals(
    journal_path: PathBuf,
    output: PathBuf,
    max_updates: u64,
    max_bytes: u64,
    max_journal_bytes: u64,
) -> Result<bool> {
    validate_limits(max_updates, max_bytes, max_journal_bytes)?;
    anyhow::ensure!(
        output != journal_path,
        "output and journal paths must differ"
    );

    let mut reader = JournalReplayReader::open(&journal_path, max_journal_bytes)
        .with_context(|| format!("failed to open journal {}", journal_path.display()))?;
    let session = reader.session().clone();
    let wall_start = DateTime::parse_from_rfc3339(&session.wall_start)
        .context("journal wall_start is invalid")?
        .with_timezone(&Utc);
    let processor = if let Some(checkpoint) = &session.initial_checkpoint {
        SpotSignalProcessor::restore(
            &session.publisher_epoch,
            wall_start,
            checkpoint.processor_state.clone(),
        )
    } else {
        SpotSignalProcessor::new(&session.publisher_epoch, wall_start)
    }
    .context("journal is incompatible with the spot-signal processor")?;
    let output_file = create_private_output(&output, "replay output")?;
    let mut pipeline = SignalPipeline::new_replay(
        processor,
        wall_start,
        BufWriter::new(output_file),
        max_updates,
        max_bytes,
    )
    .with_feed_health_stream(session.runtime_config.feed_health_stream);

    while let Some(input) = reader
        .next_input()
        .context("journal replay validation failed")?
    {
        pipeline
            .apply_replay(input)
            .context("journal input could not be replayed")?;
    }
    let terminal = reader
        .end()
        .context("journal has no terminal record")?
        .clone();
    pipeline
        .sync_output()
        .context("failed to finalize replay output")?;
    let signal = pipeline.report().context("failed to build replay report")?;
    let rewarm_update_gap = verify_replay_counts(
        session.opened_by_rotation,
        signal.updates_written,
        signal.stream_messages_written,
        &signal.quality_reason_entries,
        terminal.totals.updates_written,
        terminal.totals.stream_messages_written,
        &terminal.quality_reason_entries,
    )?;

    let report = SpotSignalReplayReport {
        session_id: session.session_id,
        contract_fingerprint: session.contract_fingerprint.aggregate_sha256,
        terminal_verified: true,
        rewarm_update_gap,
        signal,
    };
    println!(
        "{}",
        serde_json::to_string(&report).context("replay report serialization failed")?
    );
    // Replay is a deterministic, non-interruptible file operation.
    Ok(false)
}

fn validate_limits(max_updates: u64, max_bytes: u64, max_journal_bytes: u64) -> Result<()> {
    anyhow::ensure!(max_updates > 0, "max_updates must be greater than zero");
    anyhow::ensure!(max_bytes > 0, "max_bytes must be greater than zero");
    anyhow::ensure!(
        max_journal_bytes > 0,
        "max_journal_bytes must be greater than zero"
    );
    Ok(())
}

async fn run_health_timer(
    pipeline: Rc<RefCell<SignalPipeline<BufWriter<File>>>>,
    clock: ProcessClock,
    duration: Duration,
    mut shutdown: LiveShutdown,
) -> Result<(), LiveError> {
    let deadline = clock.deadline(duration);
    let interval_duration = Duration::from_nanos(HEALTH_INTERVAL_NS);
    let mut interval =
        tokio::time::interval_at(clock.deadline(interval_duration), interval_duration);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            // Stop as soon as the run is coordinated to end (SIGTERM or the
            // natural deadline). In rotation mode the deadline can be days away,
            // so the shutdown watch is what makes SIGTERM prompt.
            reason = shutdown.wait_reason() => {
                reason.map_err(LiveError::Signal)?;
                return Ok(());
            }
            _ = tokio::time::sleep_until(deadline) => return Ok(()),
            _ = interval.tick() => {
                // Record the time at which the stalled/current-thread runtime
                // actually admitted the tick, not its nominal scheduled edge.
                let now_ns = clock.elapsed_ns()?;
                pipeline
                    .borrow_mut()
                    .advance_live(now_ns)
                    .map_err(|error| LiveError::Publication(error.to_string()))?;
            }
        }
    }
}

async fn run_checkpoint_timer(
    pipeline: Rc<RefCell<SignalPipeline<BufWriter<File>>>>,
    clock: ProcessClock,
    duration: Duration,
    interval_duration: Duration,
    mut shutdown: LiveShutdown,
) -> Result<(), LiveError> {
    let deadline = clock.deadline(duration);
    let mut interval =
        tokio::time::interval_at(clock.deadline(interval_duration), interval_duration);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            reason = shutdown.wait_reason() => {
                reason.map_err(LiveError::Signal)?;
                return Ok(());
            }
            _ = tokio::time::sleep_until(deadline) => return Ok(()),
            _ = interval.tick() => {
                let now_ns = clock.elapsed_ns()?;
                let mut pipeline = pipeline.borrow_mut();
                pipeline
                    .sync_output()
                    .map_err(|error| LiveError::Publication(error.to_string()))?;
                pipeline
                    .checkpoint_live(now_ns)
                    .map_err(|error| LiveError::Publication(error.to_string()))?;
            }
        }
    }
}

fn create_private_output(path: &Path, kind: &str) -> Result<File> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        options.mode(0o600);
    }
    options.open(path).with_context(|| {
        format!(
            "failed to create {kind} {}; existing files are never overwritten",
            path.display()
        )
    })
}

fn mark_resumed_sources_unavailable<W: Write>(
    pipeline: &mut SignalPipeline<W>,
    now_ns: u64,
) -> Result<(), PipelineError> {
    pipeline.mark_unavailable_live(Venue::Coinbase, UnavailableReason::Disconnected, now_ns)?;
    pipeline.mark_unavailable_live(Venue::Kraken, UnavailableReason::Disconnected, now_ns)
}

fn handle_live_outcome(
    pipeline: &Rc<RefCell<SignalPipeline<BufWriter<File>>>>,
    venue: Venue,
    timed: TimedIngestOutcome,
    clock: &ProcessClock,
) -> Result<LiveControl, LiveError> {
    let now_ns = clock.admission_ns(timed.logical_receipt_ns)?;
    pipeline
        .borrow_mut()
        .handle_live(venue, timed, now_ns)
        .map_err(|error| LiveError::Publication(error.to_string()))
}

struct CheckpointContext {
    session_id: String,
    wall_start: String,
    contract_fingerprint: ContractFingerprint,
    runtime_config: SpotReferenceRuntimeConfig,
}

struct LiveDurability {
    journal: SignalJournalWriter,
    checkpoint: CheckpointWriter,
    checkpoint_context: CheckpointContext,
}

struct SignalPipeline<W> {
    processor: SpotSignalProcessor,
    publisher_epoch: String,
    expected_signal_count: usize,
    wall_start: DateTime<Utc>,
    writer: W,
    journal: Option<SignalJournalWriter>,
    checkpoint: Option<CheckpointWriter>,
    checkpoint_context: Option<CheckpointContext>,
    maximum_updates: u64,
    maximum_bytes: u64,
    updates_written: u64,
    stream_messages_written: u64,
    signal_batches_written: u64,
    snapshots_written: u64,
    heartbeats_written: u64,
    status_messages_written: u64,
    reset_messages_written: u64,
    bytes_written: u64,
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
    next_admission_seq: u64,
    market_events: u64,
    availability_events: u64,
    health_ticks: u64,
    decision_timing: TimingSamples,
    journal_enqueue_timing: TimingSamples,
    checkpoint_submission_timing: TimingSamples,
    provider_clock_skew: BTreeMap<Venue, SignedTimingSamples>,
    feed_health: FeedHealthBoard,
    feed_health_stream: bool,
    feed_health_written: u64,
    last_feed_health_revision: Option<u64>,
    next_feed_health_ns: Option<u64>,
    /// In-process observer of every written stream message; survives
    /// generation rotation (it belongs to the process, not the file).
    stream_hook: Option<StreamHook>,
    scratch: Vec<u8>,
}

/// Build a fresh feed-health board from a processor's declared source contracts.
/// Shared by [`SignalPipeline::new`] and [`SignalPipeline::begin_generation`] so
/// a rotated generation starts its feed-health tracking exactly as a fresh
/// process would.
fn build_feed_health(processor: &SpotSignalProcessor) -> FeedHealthBoard {
    FeedHealthBoard::new(
        processor
            .source_definitions()
            .map(|definition| FeedContract {
                source_id: definition.source_id.clone(),
                venue: venue_health_label(&definition.source_id).to_owned(),
                expected_heartbeat_ms: definition.expected_heartbeat_ms,
                cadence: spot_feed_cadence(&definition.source_id),
                liveness_bound_override_ms: spot_liveness_override(&definition.source_id),
            }),
        0,
    )
}

impl<W: Write> SignalPipeline<W> {
    fn new_live(
        processor: SpotSignalProcessor,
        wall_start: DateTime<Utc>,
        writer: W,
        maximum_updates: u64,
        maximum_bytes: u64,
        durability: LiveDurability,
    ) -> Self {
        Self::new(
            processor,
            wall_start,
            writer,
            maximum_updates,
            maximum_bytes,
            Some(durability),
        )
    }

    fn new_replay(
        processor: SpotSignalProcessor,
        wall_start: DateTime<Utc>,
        writer: W,
        maximum_updates: u64,
        maximum_bytes: u64,
    ) -> Self {
        Self::new(
            processor,
            wall_start,
            writer,
            maximum_updates,
            maximum_bytes,
            None,
        )
    }

    fn new(
        processor: SpotSignalProcessor,
        wall_start: DateTime<Utc>,
        writer: W,
        maximum_updates: u64,
        maximum_bytes: u64,
        durability: Option<LiveDurability>,
    ) -> Self {
        let (journal, checkpoint, checkpoint_context) =
            durability.map_or((None, None, None), |durability| {
                (
                    Some(durability.journal),
                    Some(durability.checkpoint),
                    Some(durability.checkpoint_context),
                )
            });
        let publisher_epoch = processor.publisher_epoch().to_owned();
        let expected_signal_count = processor.definitions().count();
        let signal_intervals_ms = processor
            .definitions()
            .map(|definition| {
                (
                    (definition.signal_id.clone(), definition.version.clone()),
                    definition.approximate_interval_ms,
                )
            })
            .collect();
        let feed_health = build_feed_health(&processor);
        Self {
            processor,
            publisher_epoch,
            expected_signal_count,
            wall_start,
            writer,
            journal,
            checkpoint,
            checkpoint_context,
            maximum_updates,
            maximum_bytes,
            updates_written: 0,
            stream_messages_written: 0,
            signal_batches_written: 0,
            snapshots_written: 0,
            heartbeats_written: 0,
            status_messages_written: 0,
            reset_messages_written: 0,
            bytes_written: 0,
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
            next_admission_seq: 0,
            market_events: 0,
            availability_events: 0,
            health_ticks: 0,
            decision_timing: TimingSamples::default(),
            journal_enqueue_timing: TimingSamples::default(),
            checkpoint_submission_timing: TimingSamples::default(),
            provider_clock_skew: BTreeMap::new(),
            feed_health,
            feed_health_stream: false,
            feed_health_written: 0,
            last_feed_health_revision: None,
            next_feed_health_ns: None,
            stream_hook: None,
            scratch: Vec::with_capacity(1024),
        }
    }

    /// Enable observe-only `feed_health` stream frames for this session. The
    /// caller passes the session's fingerprinted capability flag; a replay of a
    /// pre-capability session leaves it `false` and emits nothing.
    fn with_feed_health_stream(mut self, enabled: bool) -> Self {
        self.feed_health_stream = enabled;
        self
    }

    /// Install an in-process observer of every written stream message
    /// ([`StreamHook`]). The file output is unchanged.
    fn with_stream_hook(mut self, hook: Option<StreamHook>) -> Self {
        self.stream_hook = hook;
        self
    }

    fn handle_live(
        &mut self,
        venue: Venue,
        timed: TimedIngestOutcome,
        now_ns: u64,
    ) -> Result<LiveControl, PipelineError> {
        let bytes_before = self.bytes_written;
        let mut recovery_requests = BTreeSet::new();
        let events: Vec<SourceEvent> = timed
            .outcome
            .events
            .into_iter()
            .filter(|event| SpotSignalProcessor::accepts(&event.source_id))
            .collect();
        if !events.is_empty() {
            let input = JournalInput::MarketEvents {
                admission_seq: self.next_admission_seq,
                events: Arc::new(events),
                logical_receipt_ns: timed.logical_receipt_ns,
                now_ns,
                resync: false,
            };
            self.enqueue(input.clone())?;
            recovery_requests.extend(self.apply_input(input)?);
        }

        for notice in timed.outcome.notices {
            if let Some(reason) = unavailable_reason(&notice) {
                let input = JournalInput::Availability {
                    admission_seq: self.next_admission_seq,
                    venue,
                    reason,
                    now_ns,
                };
                self.enqueue(input.clone())?;
                recovery_requests.extend(self.apply_input(input)?);
            }
        }
        let control = if recovery_requests.contains(&venue) {
            LiveControl::Reconnect
        } else {
            LiveControl::Continue
        };
        self.flush_written_frames(bytes_before)?;
        Ok(control)
    }

    /// Flush the output writer when an admission published at least one frame.
    /// A lane that only emits control frames would otherwise sit in the
    /// buffered writer until it filled, leaving `output.jsonl` empty and the
    /// stream server without a publisher epoch to read.
    fn flush_written_frames(&mut self, bytes_before: u64) -> Result<(), PipelineError> {
        if self.bytes_written != bytes_before {
            self.writer.flush()?;
        }
        Ok(())
    }

    fn advance_live(&mut self, now_ns: u64) -> Result<(), PipelineError> {
        let input = JournalInput::HealthTick {
            admission_seq: self.next_admission_seq,
            now_ns,
        };
        self.enqueue(input.clone())?;
        let bytes_before = self.bytes_written;
        self.apply_input(input)?;
        self.flush_written_frames(bytes_before)
    }

    /// Admit the C5 rotation book resync: one single-venue `MarketEvents` batch
    /// per ready book source (Coinbase, then Kraken), tagged `resync`, as the
    /// first records of the freshly opened generation. Goes through the normal
    /// live admit path, so each batch is counted in `next_admission_seq` and
    /// written to the new journal. On this warm live process the synthesized
    /// snapshot rows are numbered to end at the book's current ordering cursor,
    /// so the ordering path skips them and the live book is unchanged; a cold
    /// replay of the new journal (empty book) rebuilds each book from them before
    /// the first live delta arrives, so the delta admits instead of fatalling.
    fn admit_book_resync(&mut self, now_ns: u64) -> Result<(), PipelineError> {
        let batches = self.processor.book_resync_inputs();
        if batches.is_empty() {
            return Ok(());
        }
        let bytes_before = self.bytes_written;
        for events in batches {
            let input = JournalInput::MarketEvents {
                admission_seq: self.next_admission_seq,
                events: Arc::new(events),
                logical_receipt_ns: now_ns,
                now_ns,
                resync: true,
            };
            self.enqueue(input.clone())?;
            self.apply_input(input)?;
        }
        self.flush_written_frames(bytes_before)
    }

    fn mark_unavailable_live(
        &mut self,
        venue: Venue,
        reason: UnavailableReason,
        now_ns: u64,
    ) -> Result<(), PipelineError> {
        let input = JournalInput::Availability {
            admission_seq: self.next_admission_seq,
            venue,
            reason,
            now_ns,
        };
        self.enqueue(input.clone())?;
        let bytes_before = self.bytes_written;
        self.apply_input(input)?;
        self.flush_written_frames(bytes_before)
    }

    fn checkpoint_live(&mut self, now_ns: u64) -> Result<(), PipelineError> {
        let started = Instant::now();
        let checkpoint = self.checkpoint_document(now_ns)?;
        let writer = self.checkpoint.as_ref().ok_or_else(|| {
            PipelineError::Mode("live checkpoint writer is unavailable".to_owned())
        })?;
        writer.enqueue(checkpoint)?;
        self.checkpoint_submission_timing
            .record(started.elapsed())?;
        Ok(())
    }

    fn apply_replay(&mut self, input: JournalInput) -> Result<(), PipelineError> {
        if self.journal.is_some() {
            return Err(PipelineError::Mode(
                "replay input cannot be applied to a live pipeline".to_owned(),
            ));
        }
        self.apply_input(input).map(|_| ())
    }

    fn enqueue(&mut self, input: JournalInput) -> Result<(), PipelineError> {
        let journal = self.journal.as_ref().ok_or_else(|| {
            PipelineError::Mode("live input requires an active journal".to_owned())
        })?;
        let started = Instant::now();
        journal.enqueue(input)?;
        self.journal_enqueue_timing.record(started.elapsed())?;
        Ok(())
    }

    /// The journal enqueue has already succeeded before this mutates processor state.
    fn apply_input(&mut self, input: JournalInput) -> Result<BTreeSet<Venue>, PipelineError> {
        if input.admission_seq() != self.next_admission_seq {
            return Err(PipelineError::AdmissionSequence {
                expected: self.next_admission_seq,
                actual: input.admission_seq(),
            });
        }
        let now_ns = input.now_ns();
        self.next_admission_seq = checked_add(self.next_admission_seq, 1)?;
        self.ensure_stream_started(now_ns)?;

        let mut recovery_requests = BTreeSet::new();
        let updates = match input {
            JournalInput::MarketEvents {
                events,
                logical_receipt_ns,
                now_ns,
                resync,
                ..
            } => {
                self.market_events = checked_add(
                    self.market_events,
                    u64::try_from(events.len()).map_err(|_| PipelineError::Counter)?,
                )?;
                let mut updates = Vec::new();
                for event in events.iter() {
                    let started = Instant::now();
                    if resync {
                        // A rotation resync row rebuilds the BOOK ONLY. It is not
                        // a venue frame, so it must not prove venue liveness
                        // (`venue_last_input_ns`/`venue_alive`), feed health, or
                        // provider-clock skew — a rotation over a silent feed
                        // must still fail closed. This same path runs on replay,
                        // because the journal record carries the `resync` flag.
                        updates.extend(self.processor.ingest_resync(
                            event,
                            logical_receipt_ns,
                            now_ns,
                        )?);
                        self.decision_timing.record(started.elapsed())?;
                    } else {
                        let venue = venue_for_event(event)?;
                        updates.extend(self.processor.ingest(event, logical_receipt_ns, now_ns)?);
                        self.decision_timing.record(started.elapsed())?;
                        self.observe_provider_clock(venue, event, logical_receipt_ns)?;
                        self.feed_health.observe_event(&event.source_id, now_ns);
                    }
                }
                recovery_requests = self.processor.take_book_recovery_requests();
                if !resync {
                    updates.extend(self.processor.emit_after_events(now_ns)?);
                }
                updates
            }
            JournalInput::Availability {
                venue,
                reason,
                now_ns,
                ..
            } => {
                self.availability_events = checked_add(self.availability_events, 1)?;
                let (availability, detail) = feed_availability(reason);
                self.feed_health.observe_availability(
                    venue_label(venue),
                    availability,
                    detail,
                    now_ns,
                );
                let started = Instant::now();
                let updates = self.processor.mark_unavailable(venue, reason, now_ns)?;
                self.decision_timing.record(started.elapsed())?;
                updates
            }
            JournalInput::HealthTick { now_ns, .. } => {
                self.health_ticks = checked_add(self.health_ticks, 1)?;
                self.feed_health.observe_tick(now_ns);
                let started = Instant::now();
                let updates = self.processor.advance_time(now_ns)?;
                self.decision_timing.record(started.elapsed())?;
                updates
            }
        };

        if !updates.is_empty() {
            self.write_signal_batch(now_ns, updates)?;
        }
        self.maybe_write_status(now_ns)?;
        self.maybe_write_snapshot(now_ns)?;
        self.maybe_write_heartbeat(now_ns)?;
        self.maybe_write_feed_health(now_ns)?;
        Ok(recovery_requests)
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

    fn ensure_stream_started(&mut self, now_ns: u64) -> Result<(), PipelineError> {
        self.stream_accounting().ensure_stream_started(now_ns)?;
        Ok(())
    }

    fn write_signal_batch(
        &mut self,
        now_ns: u64,
        updates: Vec<SignalUpdate>,
    ) -> Result<(), PipelineError> {
        self.stream_accounting()
            .write_signal_batch(now_ns, updates)?;
        Ok(())
    }

    fn maybe_write_status(&mut self, now_ns: u64) -> Result<(), PipelineError> {
        let expected = self.expected_signal_count;
        self.stream_accounting()
            .maybe_write_status(now_ns, expected)?;
        Ok(())
    }

    fn maybe_write_snapshot(&mut self, now_ns: u64) -> Result<(), PipelineError> {
        self.stream_accounting().maybe_write_snapshot(now_ns)?;
        Ok(())
    }

    fn maybe_write_heartbeat(&mut self, now_ns: u64) -> Result<(), PipelineError> {
        self.stream_accounting().maybe_write_heartbeat(now_ns)?;
        Ok(())
    }

    /// Emit an observe-only `feed_health` frame when the board's reportable state
    /// transitions, and at least once per bounded periodic cadence. Gated on the
    /// session's fingerprinted capability so a pre-capability replay emits none.
    fn maybe_write_feed_health(&mut self, now_ns: u64) -> Result<(), PipelineError> {
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

    fn observe_provider_clock(
        &mut self,
        venue: Venue,
        event: &SourceEvent,
        logical_receipt_ns: u64,
    ) -> Result<(), PipelineError> {
        let receipt_delta =
            i64::try_from(logical_receipt_ns).map_err(|_| PipelineError::Counter)?;
        let Some(local_receipt) = self
            .wall_start
            .checked_add_signed(ChronoDuration::nanoseconds(receipt_delta))
        else {
            self.provider_clock_skew
                .entry(venue)
                .or_default()
                .record_unrepresentable()?;
            return Ok(());
        };
        let provider_time = DateTime::parse_from_rfc3339(&event.event_time)
            .map_err(|error| PipelineError::ClockObservation(error.to_string()))?
            .with_timezone(&Utc);
        let Some(skew_ns) = provider_time
            .signed_duration_since(local_receipt)
            .num_nanoseconds()
        else {
            self.provider_clock_skew
                .entry(venue)
                .or_default()
                .record_unrepresentable()?;
            return Ok(());
        };
        self.provider_clock_skew
            .entry(venue)
            .or_default()
            .record(skew_ns)
    }

    fn journal_end(&self) -> JournalEnd {
        JournalEnd {
            totals: JournalTotals {
                admitted_inputs: self.next_admission_seq,
                market_events: self.market_events,
                availability_events: self.availability_events,
                health_ticks: self.health_ticks,
                updates_written: self.updates_written,
                stream_messages_written: self.stream_messages_written,
            },
            quality_reason_entries: self.processor.quality_reason_entries().clone(),
            raw_capture_references: Vec::new(),
        }
    }

    fn complete_journal(&mut self) -> Result<JournalWriteReport, PipelineError> {
        let end = self.journal_end();
        self.journal
            .take()
            .ok_or_else(|| PipelineError::Mode("live journal is unavailable".to_owned()))?
            .finish(end)
            .map_err(PipelineError::Journal)
    }

    fn abort_journal(&mut self) -> Result<JournalWriteReport, PipelineError> {
        self.journal
            .take()
            .ok_or_else(|| PipelineError::Mode("live journal is unavailable".to_owned()))?
            .abort()
            .map_err(PipelineError::Journal)
    }

    fn checkpoint_document(&self, now_ns: u64) -> Result<CheckpointDocument, PipelineError> {
        let context = self.checkpoint_context.as_ref().ok_or_else(|| {
            PipelineError::Mode("live checkpoint context is unavailable".to_owned())
        })?;
        CheckpointDocument::new(
            context.session_id.clone(),
            context.wall_start.clone(),
            now_ns,
            self.next_admission_seq,
            context.contract_fingerprint.clone(),
            context.runtime_config.clone(),
            self.processor.checkpoint_state(),
        )
        .map_err(PipelineError::Checkpoint)
    }

    fn complete_checkpoint(&mut self, now_ns: u64) -> Result<CheckpointWriteReport, PipelineError> {
        let checkpoint = self.checkpoint_document(now_ns)?;
        self.checkpoint
            .take()
            .ok_or_else(|| PipelineError::Mode("live checkpoint is unavailable".to_owned()))?
            .finish(checkpoint)
            .map_err(PipelineError::Checkpoint)
    }

    fn abort_checkpoint(&mut self) -> Result<CheckpointWriteReport, PipelineError> {
        self.checkpoint
            .take()
            .ok_or_else(|| PipelineError::Mode("live checkpoint is unavailable".to_owned()))?
            .abort()
            .map_err(PipelineError::Checkpoint)
    }

    /// The warm processor, for building a new generation's initial checkpoint.
    fn processor(&self) -> &SpotSignalProcessor {
        &self.processor
    }

    /// Rotate the warm processor and this pipeline's envelope to a new output
    /// epoch for a new file generation (C5). Every calculation ring stays warm;
    /// only the output identity re-seats (fresh epoch, output sequences zeroed,
    /// epoch appended to the lineage).
    fn rotate_output_epoch(&mut self, publisher_epoch: &str) -> Result<(), PipelineError> {
        self.processor.rotate_output_epoch(publisher_epoch)?;
        self.publisher_epoch = publisher_epoch.to_owned();
        Ok(())
    }

    fn report(&mut self) -> Result<SignalPipelineReport, PipelineError> {
        Ok(SignalPipelineReport {
            admitted_inputs: self.next_admission_seq,
            market_events: self.market_events,
            availability_events: self.availability_events,
            health_ticks: self.health_ticks,
            updates_written: self.updates_written,
            stream_messages_written: self.stream_messages_written,
            signal_batches_written: self.signal_batches_written,
            snapshots_written: self.snapshots_written,
            heartbeats_written: self.heartbeats_written,
            status_messages_written: self.status_messages_written,
            reset_messages_written: self.reset_messages_written,
            bytes_written: self.bytes_written,
            quality_reason_entries: self.processor.quality_reason_entries().clone(),
            decision_time_ns: self.decision_timing.report()?,
            journal_enqueue_time_ns: self.journal_enqueue_timing.report()?,
            checkpoint_submission_time_ns: self.checkpoint_submission_timing.report()?,
            provider_event_minus_local_receipt_ns: self
                .provider_clock_skew
                .iter()
                .map(|(venue, samples)| (*venue, samples.report()))
                .collect(),
            feed_health: self.feed_health.snapshot(),
            feed_health_messages_written: self.feed_health_written,
            decision_measurement: "each journaled source event entered the spot-signal processor through validated SignalUpdate decisions; journal enqueue, checkpoint snapshot submission, and diagnostic JSON serialization excluded",
            clock_skew_measurement: "provider event_time minus local monotonic receipt rendered on the session wall origin; observational only and never used for readiness",
        })
    }
}

impl SignalPipeline<BufWriter<File>> {
    fn sync_output(&mut self) -> Result<(), PipelineError> {
        self.writer.flush()?;
        self.writer.get_ref().sync_data()?;
        Ok(())
    }

    /// Install a new file generation's output/journal/checkpoint writers and
    /// reset every fresh-process stream-accounting field (C5 Design B).
    ///
    /// The warm processor (every calculation ring) is left untouched — the
    /// caller rotates its output epoch via [`SignalPipeline::rotate_output_epoch`]
    /// before this — so the first decision after the boundary carries every
    /// identity that was Ready before it. Everything the stream layer keeps
    /// (envelope sequence, producer health, instance ledgers, snapshot/heartbeat
    /// deadlines, feed-health board, per-generation caps counters and admission
    /// counters) restarts exactly as a fresh process would, so the generation
    /// opens with a `reset` frame at sequence 0 under the new output epoch.
    fn begin_generation(&mut self, writer: BufWriter<File>, durability: LiveDurability) {
        self.writer = writer;
        self.journal = Some(durability.journal);
        self.checkpoint = Some(durability.checkpoint);
        self.checkpoint_context = Some(durability.checkpoint_context);
        self.updates_written = 0;
        self.stream_messages_written = 0;
        self.signal_batches_written = 0;
        self.snapshots_written = 0;
        self.heartbeats_written = 0;
        self.status_messages_written = 0;
        self.reset_messages_written = 0;
        self.bytes_written = 0;
        self.next_stream_sequence = 0;
        self.last_signal_batch_sequence = None;
        self.last_decision_or_status_sequence = None;
        self.latest_updates.clear();
        self.instance_ledger.clear();
        self.invalidated_tombstones.clear();
        self.producer_health = None;
        self.next_heartbeat_ns = None;
        self.next_snapshot_ns = None;
        self.next_admission_seq = 0;
        self.market_events = 0;
        self.availability_events = 0;
        self.health_ticks = 0;
        self.decision_timing = TimingSamples::default();
        self.journal_enqueue_timing = TimingSamples::default();
        self.checkpoint_submission_timing = TimingSamples::default();
        self.provider_clock_skew.clear();
        self.feed_health = build_feed_health(&self.processor);
        self.feed_health_written = 0;
        self.last_feed_health_revision = None;
        self.next_feed_health_ns = None;
    }
}

fn unavailable_reason(notice: &FeedNotice) -> Option<UnavailableReason> {
    if notice.code == FeedNoticeCode::SubscriptionRejected {
        return Some(UnavailableReason::SubscriptionRejected);
    }
    match notice.recovery.as_ref()? {
        RecoveryAction::BackfillCoinbaseTrades { .. } => None,
        RecoveryAction::Reconnect if notice.code == FeedNoticeCode::Disconnected => {
            Some(UnavailableReason::Disconnected)
        }
        // A paused surface is not publishable, so a pending index repair is an
        // unavailability rather than a degradation (Round 2 ruling E18's
        // reasoning, applied to the one other backfill action). The spot lane
        // never ingests an index event; the arm exists so adding a recovery
        // action forces a decision here rather than defaulting.
        RecoveryAction::BackfillKalshiBrti { .. }
        | RecoveryAction::Reconnect
        | RecoveryAction::RediscoverPredictionMarket
        | RecoveryAction::ResubscribeKrakenBook { .. }
        | RecoveryAction::ResubscribeKrakenLevel3 { .. }
        | RecoveryAction::ResubscribeKalshiBook { .. } => Some(UnavailableReason::Recovery),
    }
}

/// Stable feed-health venue label for an availability transition.
fn venue_label(venue: Venue) -> &'static str {
    match venue {
        Venue::Coinbase => "coinbase",
        Venue::Kraken => "kraken",
        Venue::Binance => "binance",
        Venue::Bybit => "bybit",
    }
}

/// Feed-health venue label derived from a source identifier. It must agree with
/// [`venue_label`] so availability transitions reach the right feeds.
fn venue_health_label(source_id: &str) -> &'static str {
    if source_id.starts_with("coinbase.") {
        "coinbase"
    } else if source_id.starts_with("kraken.") {
        "kraken"
    } else {
        "unknown"
    }
}

/// Cadence class for the feed-health companion rule, over the closed set of
/// source_ids this spot runner consumes. Tickers publish only on trades and
/// trade streams are event-driven; book-levels snapshots refresh on a venue
/// timer. An unrecognized id defaults to `Periodic` (degrade on bound), the
/// conservative choice for a feed of unknown cadence.
fn spot_feed_cadence(source_id: &str) -> FeedCadence {
    match source_id {
        "coinbase.exchange.btc-usd.ticker"
        | "kraken.spot.btc-usd.ticker"
        | "coinbase.exchange.btc-usd.trades"
        | "kraken.spot.btc-usd.trades" => FeedCadence::EventDriven,
        "coinbase.exchange.btc-usd.book-levels" | "kraken.spot.btc-usd.book-levels" => {
            FeedCadence::Periodic
        }
        _ => FeedCadence::Periodic,
    }
}

/// Calibrated liveness-bound override, in milliseconds, for the feed-health
/// board, over the closed set of source_ids this spot runner consumes. Values
/// are from the 2026-08-17 14 h spot soak (bound = 3 s + measured degraded-
/// episode duration); book-levels feeds keep their declared/default bound, and an
/// unrecognized id keeps it too.
fn spot_liveness_override(source_id: &str) -> Option<u64> {
    match source_id {
        // observed max quiet gap ≈ 47.7 s
        "kraken.spot.btc-usd.trades" => Some(60_000),
        // observed max quiet gap ≈ 22.4 s
        "kraken.spot.btc-usd.ticker" => Some(30_000),
        // observed max quiet gap ≈ 5.7 s (both Coinbase event-driven streams)
        "coinbase.exchange.btc-usd.trades" | "coinbase.exchange.btc-usd.ticker" => Some(15_000),
        _ => None,
    }
}

/// Map an admitted unavailable reason onto the observe-only board's abstract
/// availability signal plus a concrete detail label for the report.
fn feed_availability(reason: UnavailableReason) -> (FeedAvailability, &'static str) {
    match reason {
        UnavailableReason::Disconnected => (FeedAvailability::Down, "disconnected"),
        UnavailableReason::SubscriptionRejected => {
            (FeedAvailability::Down, "subscription_rejected")
        }
        UnavailableReason::Recovery => (FeedAvailability::Reconnecting, "recovery"),
    }
}

fn venue_for_event(event: &SourceEvent) -> Result<Venue, PipelineError> {
    if event.source_id.starts_with("coinbase.exchange.") {
        Ok(Venue::Coinbase)
    } else if event.source_id.starts_with("kraken.spot.") {
        Ok(Venue::Kraken)
    } else {
        Err(PipelineError::Mode(format!(
            "unsupported source event {} reached the spot-signal pipeline",
            event.source_id
        )))
    }
}

fn checked_add(value: u64, increment: u64) -> Result<u64, PipelineError> {
    value.checked_add(increment).ok_or(PipelineError::Counter)
}

#[derive(Default)]
struct TimingSamples {
    total: u64,
    samples_ns: Vec<u64>,
}

impl TimingSamples {
    fn record(&mut self, elapsed: Duration) -> Result<(), PipelineError> {
        self.total = checked_add(self.total, 1)?;
        if self.samples_ns.len() < MAX_TIMING_SAMPLES {
            self.samples_ns
                .push(u64::try_from(elapsed.as_nanos()).map_err(|_| PipelineError::Counter)?);
        }
        Ok(())
    }

    fn report(&mut self) -> Result<Option<NanosecondDistribution>, PipelineError> {
        if self.samples_ns.is_empty() {
            return Ok(None);
        }
        self.samples_ns.sort_unstable();
        Ok(Some(NanosecondDistribution {
            samples_recorded: u64::try_from(self.samples_ns.len())
                .map_err(|_| PipelineError::Counter)?,
            samples_total: self.total,
            minimum: self.samples_ns[0],
            p50: percentile(&self.samples_ns, 50),
            p95: percentile(&self.samples_ns, 95),
            p99: percentile(&self.samples_ns, 99),
            maximum: self.samples_ns[self.samples_ns.len() - 1],
        }))
    }
}

#[derive(Default)]
struct SignedTimingSamples {
    samples: u64,
    unrepresentable_samples: u64,
    minimum: Option<i64>,
    maximum: Option<i64>,
    last: Option<i64>,
}

impl SignedTimingSamples {
    fn record(&mut self, value: i64) -> Result<(), PipelineError> {
        self.samples = checked_add(self.samples, 1)?;
        self.minimum = Some(self.minimum.map_or(value, |current| current.min(value)));
        self.maximum = Some(self.maximum.map_or(value, |current| current.max(value)));
        self.last = Some(value);
        Ok(())
    }

    fn record_unrepresentable(&mut self) -> Result<(), PipelineError> {
        self.unrepresentable_samples = checked_add(self.unrepresentable_samples, 1)?;
        Ok(())
    }

    fn report(&self) -> ProviderClockSkewReport {
        ProviderClockSkewReport {
            samples: self.samples,
            unrepresentable_samples: self.unrepresentable_samples,
            minimum: self.minimum,
            maximum: self.maximum,
            last: self.last,
        }
    }
}

fn percentile(sorted: &[u64], percentile: usize) -> u64 {
    let rank = sorted.len().saturating_mul(percentile).saturating_add(99) / 100;
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

#[derive(Debug, Serialize)]
pub struct SpotSignalRunReport {
    pub session_id: String,
    pub contract_fingerprint: String,
    pub coinbase: LiveReport,
    pub kraken: LiveReport,
    pub signal: SignalPipelineReport,
    pub journal: JournalWriteReport,
    pub checkpoint: CheckpointWriteReport,
}

#[derive(Debug, Serialize)]
pub struct SpotSignalReplayReport {
    pub session_id: String,
    pub contract_fingerprint: String,
    pub terminal_verified: bool,
    /// For a rotation-opened generation (`opened_by_rotation`), the number of
    /// ready updates the warm live run emitted that a cold replay omits because
    /// its rings re-warm from scratch (C5). `None` for a fresh-start generation,
    /// where the replay reproduces the terminal counts exactly. A non-zero value
    /// here is the documented re-warm gap, not a fault.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rewarm_update_gap: Option<u64>,
    pub signal: SignalPipelineReport,
}

#[derive(Debug, Serialize)]
pub struct SignalPipelineReport {
    admitted_inputs: u64,
    market_events: u64,
    availability_events: u64,
    health_ticks: u64,
    updates_written: u64,
    stream_messages_written: u64,
    signal_batches_written: u64,
    snapshots_written: u64,
    heartbeats_written: u64,
    status_messages_written: u64,
    reset_messages_written: u64,
    bytes_written: u64,
    quality_reason_entries: BTreeMap<String, u64>,
    decision_time_ns: Option<NanosecondDistribution>,
    journal_enqueue_time_ns: Option<NanosecondDistribution>,
    checkpoint_submission_time_ns: Option<NanosecondDistribution>,
    provider_event_minus_local_receipt_ns: BTreeMap<Venue, ProviderClockSkewReport>,
    feed_health: FeedHealthSnapshot,
    feed_health_messages_written: u64,
    decision_measurement: &'static str,
    clock_skew_measurement: &'static str,
}

#[derive(Debug, Serialize)]
pub struct NanosecondDistribution {
    samples_recorded: u64,
    samples_total: u64,
    minimum: u64,
    p50: u64,
    p95: u64,
    p99: u64,
    maximum: u64,
}

#[derive(Debug, Serialize)]
pub struct ProviderClockSkewReport {
    samples: u64,
    unrepresentable_samples: u64,
    minimum: Option<i64>,
    maximum: Option<i64>,
    last: Option<i64>,
}

#[derive(Debug, Error)]
enum PipelineError {
    #[error(transparent)]
    Signal(#[from] SpotSignalError),
    #[error(transparent)]
    Journal(#[from] JournalError),
    #[error(transparent)]
    Checkpoint(#[from] CheckpointError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Validation(#[from] ValidationError),
    #[error("diagnostic output reached its {0} update limit")]
    UpdateLimit(u64),
    #[error("diagnostic output reached its {0} byte limit")]
    ByteLimit(u64),
    #[error("signal admission sequence mismatch: expected {expected}, actual {actual}")]
    AdmissionSequence { expected: u64, actual: u64 },
    #[error("signal stream sequence mismatch: expected {expected}, actual {actual}")]
    StreamSequence { expected: u64, actual: u64 },
    #[error("signal pipeline mode error: {0}")]
    Mode(String),
    #[error("provider clock observation failed: {0}")]
    ClockObservation(String),
    #[error("signal diagnostic counter exhausted")]
    Counter,
}

impl From<StreamAccountingError> for PipelineError {
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
                Self::Mode("snapshot state has no signal-batch anchor".into())
            }
            StreamAccountingError::MissingStreamHealth => {
                Self::Mode("stream health is unavailable".into())
            }
            StreamAccountingError::UnexpectedMarketTransition => {
                Self::Mode("market transition reached the spot-signal stream".into())
            }
            StreamAccountingError::UnexpectedFeedHealth => {
                Self::Mode("feed health frame was not accounted in the spot-signal stream".into())
            }
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

/// Map a spot-signal pipeline failure to a supervision [`ExitClass`].
///
/// Every pipeline failure is a durability, contract, bound, or invariant fault
/// that a blind restart cannot fix, so all variants classify as `Fatal`. The
/// match is exhaustive with no wildcard so a new variant forces this decision to
/// be revisited.
fn classify_pipeline(error: &PipelineError) -> ExitClass {
    match error {
        PipelineError::Signal(_)
        | PipelineError::Journal(_)
        | PipelineError::Checkpoint(_)
        | PipelineError::Json(_)
        | PipelineError::Io(_)
        | PipelineError::Validation(_)
        | PipelineError::UpdateLimit(_)
        | PipelineError::ByteLimit(_)
        | PipelineError::AdmissionSequence { .. }
        | PipelineError::StreamSequence { .. }
        | PipelineError::Mode(_)
        | PipelineError::ClockObservation(_)
        | PipelineError::Counter => ExitClass::Fatal,
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::SPOT_SIGNAL_SET_COUNT;
    use serde_json::json;

    use super::*;

    /// Every `PipelineError` variant classifies as do-not-restart. Adding a
    /// variant forces both `classify_pipeline` and this list to be updated.
    #[test]
    fn every_pipeline_error_variant_is_fatal() {
        let cases = [
            PipelineError::UpdateLimit(1),
            PipelineError::ByteLimit(1),
            PipelineError::AdmissionSequence {
                expected: 1,
                actual: 0,
            },
            PipelineError::StreamSequence {
                expected: 1,
                actual: 0,
            },
            PipelineError::Mode(String::new()),
            PipelineError::ClockObservation(String::new()),
            PipelineError::Counter,
        ];
        for error in cases {
            assert_eq!(classify_pipeline(&error), ExitClass::Fatal);
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn spot_operator_signal_before_deadline_classifies_interrupted() {
        let (trigger, shutdown) = live_shutdown_channel();
        let mut coinbase = shutdown.clone();
        let mut kraken = shutdown;
        let coinbase_worker =
            async move { coinbase.wait_reason().await.context("Coinbase shutdown") };
        let kraken_worker = async move { kraken.wait_reason().await.context("Kraken shutdown") };
        let coordinator = async move {
            coordinate_live_shutdown_on(
                tokio::time::Instant::now() + Duration::from_secs(60),
                trigger,
                async { Ok::<_, std::io::Error>(()) },
            )
            .await
            .context("coordinate spot operator shutdown")
        };

        let (coinbase_reason, kraken_reason, reason) =
            tokio::try_join!(coinbase_worker, kraken_worker, coordinator)
                .expect("spot shutdown waiters join");
        for observed in [coinbase_reason, kraken_reason, reason] {
            assert!(
                observed.interrupted(),
                "an operator signal before the deadline must remain authoritative"
            );
        }
        assert_eq!(ExitClass::Interrupted.code(), 130);
    }

    /// A spot lane that has yet to publish a signal still emits control frames.
    /// Those frames must reach the file as the admission returns, not when the
    /// buffered writer eventually fills.
    #[test]
    fn live_admission_flushes_control_frames_before_it_returns() {
        const EPOCH: &str = "spot-flush-per-admission-test";
        let checkpoint_path = TestPath::new("flush-per-admission-checkpoint");
        let journal_path = TestPath::new("flush-per-admission-journal");
        let runtime_config = SpotReferenceRuntimeConfig::new(SPOT_SIGNAL_KRAKEN_BOOK_DEPTH)
            .expect("runtime config builds");
        let PreparedSpotSignalRun {
            clock,
            processor,
            session,
            initial_checkpoint,
        } = prepare_spot_signal_run(
            checkpoint_path.as_path(),
            false,
            EPOCH.to_owned(),
            DEFAULT_CHECKPOINT_MAX_BYTES,
            runtime_config.clone(),
        )
        .expect("fresh spot run prepares");
        let journal =
            SignalJournalWriter::create(journal_path.as_path(), session.clone(), 4_194_304, 8)
                .expect("journal creates");
        let checkpoint = CheckpointWriter::create(
            checkpoint_path.as_path(),
            initial_checkpoint,
            DEFAULT_CHECKPOINT_MAX_BYTES,
            8,
            false,
        )
        .expect("checkpoint creates");
        let checkpoint_context = CheckpointContext {
            session_id: session.session_id.clone(),
            wall_start: session.wall_start.clone(),
            contract_fingerprint: session.contract_fingerprint.clone(),
            runtime_config,
        };
        let mut pipeline = SignalPipeline::new_live(
            processor,
            clock.wall_start(),
            BufWriter::with_capacity(1024 * 1024, Vec::new()),
            1_000,
            1_000_000,
            LiveDurability {
                journal,
                checkpoint,
                checkpoint_context,
            },
        );

        pipeline
            .advance_live(HEALTH_INTERVAL_NS)
            .expect("health tick admits");

        assert!(
            !pipeline.writer.get_ref().is_empty(),
            "control frames must be flushed before the admission returns"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn reached_natural_deadline_cancels_waiters_and_finishes_spot_journal() {
        const EPOCH: &str = "spot-natural-deadline-test";
        let checkpoint_path = TestPath::new("natural-deadline-checkpoint");
        let journal_path = TestPath::new("natural-deadline-journal");
        let runtime_config = SpotReferenceRuntimeConfig::new(SPOT_SIGNAL_KRAKEN_BOOK_DEPTH)
            .expect("runtime config builds");
        let PreparedSpotSignalRun {
            clock,
            processor,
            session,
            initial_checkpoint,
        } = prepare_spot_signal_run(
            checkpoint_path.as_path(),
            false,
            EPOCH.to_owned(),
            DEFAULT_CHECKPOINT_MAX_BYTES,
            runtime_config.clone(),
        )
        .expect("fresh spot run prepares");
        let journal =
            SignalJournalWriter::create(journal_path.as_path(), session.clone(), 4_194_304, 8)
                .expect("journal creates");
        let checkpoint = CheckpointWriter::create(
            checkpoint_path.as_path(),
            initial_checkpoint,
            DEFAULT_CHECKPOINT_MAX_BYTES,
            8,
            false,
        )
        .expect("checkpoint creates");
        let checkpoint_context = CheckpointContext {
            session_id: session.session_id.clone(),
            wall_start: session.wall_start.clone(),
            contract_fingerprint: session.contract_fingerprint.clone(),
            runtime_config,
        };
        let mut pipeline = SignalPipeline::new_live(
            processor,
            clock.wall_start(),
            Vec::new(),
            1_000,
            1_000_000,
            LiveDurability {
                journal,
                checkpoint,
                checkpoint_context,
            },
        );

        let (trigger, shutdown) = live_shutdown_channel();
        let mut coinbase = shutdown.clone();
        let mut kraken = shutdown;
        let coinbase_worker =
            async move { coinbase.wait_reason().await.context("Coinbase shutdown") };
        let kraken_worker = async move { kraken.wait_reason().await.context("Kraken shutdown") };
        let deadline = clock.deadline(Duration::ZERO);
        let coordinator = async move {
            coordinate_live_shutdown_on(
                deadline,
                trigger,
                std::future::pending::<std::result::Result<(), std::io::Error>>(),
            )
            .await
            .context("coordinate test shutdown")
        };

        let (coinbase_reason, kraken_reason, reason) =
            tokio::try_join!(coinbase_worker, kraken_worker, coordinator)
                .expect("spot shutdown waiters join");
        for observed in [coinbase_reason, kraken_reason, reason] {
            assert!(
                !observed.interrupted(),
                "a reached natural deadline must publish the natural cause"
            );
        }

        let journal_report = pipeline.complete_journal().expect("spot journal completes");
        pipeline
            .complete_checkpoint(clock.elapsed_ns().expect("final clock samples"))
            .expect("spot checkpoint completes");
        assert_eq!(journal_report.records_written, 2);

        let mut reader = JournalReplayReader::open(journal_path.as_path(), 4_194_304)
            .expect("completed spot journal opens");
        assert_eq!(reader.next_input().expect("journal end reads"), None);
        reader.end().expect("journal must retain its end record");
    }

    fn wall_start() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-08-13T12:00:00Z")
            .expect("test timestamp parses")
            .with_timezone(&Utc)
    }

    #[test]
    fn diagnostic_writer_enforces_limits_before_partial_output() {
        let processor =
            SpotSignalProcessor::new("test", wall_start()).expect("definition is valid");
        let mut pipeline =
            SignalPipeline::new_replay(processor, wall_start(), Vec::new(), 1, 10_000);
        let update = SignalUpdate {
            schema_version: signals_core::SIGNAL_UPDATE_SCHEMA_VERSION,
            signal_id: "btc.external.spot_reference".to_owned(),
            signal_version: "1.0.0".to_owned(),
            publisher_epoch: "test".to_owned(),
            sequence: 0,
            event_time: "2026-08-13T12:00:00Z".to_owned(),
            produced_at: "2026-08-13T12:00:00Z".to_owned(),
            valid_from: "2026-08-13T12:00:00Z".to_owned(),
            valid_until: "2026-08-13T12:00:10Z".to_owned(),
            quality: signals_core::Quality {
                state: signals_core::QualityState::Ready,
                reasons: Vec::new(),
            },
            values: BTreeMap::from([("value".to_owned(), json!(1))]),
            scope: signals_core::SignalScope::Global,
        };
        pipeline
            .write_signal_batch(0, vec![update.clone()])
            .expect("first update fits");
        let complete_prefix = pipeline.writer.clone();
        assert!(matches!(
            pipeline.write_signal_batch(0, vec![update.clone()]),
            Err(PipelineError::UpdateLimit(1))
        ));
        assert_eq!(pipeline.writer, complete_prefix);

        let one_line_bytes = u64::try_from(complete_prefix.len()).expect("line length fits u64");
        let processor =
            SpotSignalProcessor::new("test", wall_start()).expect("definition is valid");
        let mut byte_limited =
            SignalPipeline::new_replay(processor, wall_start(), Vec::new(), 10, one_line_bytes);
        byte_limited
            .write_signal_batch(0, vec![update.clone()])
            .expect("first update exactly fits");
        let complete_prefix = byte_limited.writer.clone();
        assert!(matches!(
            byte_limited.write_signal_batch(0, vec![update]),
            Err(PipelineError::ByteLimit(limit)) if limit == one_line_bytes
        ));
        assert_eq!(byte_limited.writer, complete_prefix);
    }

    #[test]
    fn stream_frames_preserve_atomic_batches_snapshots_and_heartbeats() {
        let processor =
            SpotSignalProcessor::new("stream-test", wall_start()).expect("processor constructs");
        let mut pipeline =
            SignalPipeline::new_replay(processor, wall_start(), Vec::new(), 1_000, 1_000_000);
        for (admission_seq, now_ns) in [
            (0, 0),
            (1, SIGNAL_HEARTBEAT_INTERVAL_NS),
            (2, SIGNAL_SNAPSHOT_INTERVAL_NS),
        ] {
            pipeline
                .apply_replay(JournalInput::HealthTick {
                    admission_seq,
                    now_ns,
                })
                .expect("health input emits a valid stream");
        }

        let messages: Vec<SignalStreamMessage> = pipeline
            .writer
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice(line).expect("stream line deserializes"))
            .collect();
        assert!(matches!(
            messages.first(),
            Some(SignalStreamMessage::Reset { .. })
        ));
        assert!(
            messages
                .iter()
                .any(|message| matches!(message, SignalStreamMessage::Heartbeat { .. }))
        );
        let batches: Vec<_> = messages
            .iter()
            .filter_map(|message| match message {
                SignalStreamMessage::SignalBatch {
                    sequence, updates, ..
                } => Some((*sequence, updates)),
                _ => None,
            })
            .collect();
        assert!(
            batches.is_empty(),
            "non-ready boundaries suppress signal_batch"
        );
        assert!(
            messages
                .iter()
                .any(|message| matches!(message, SignalStreamMessage::InstanceStatus { .. }))
        );

        let snapshot = messages
            .iter()
            .find_map(|message| match message {
                SignalStreamMessage::Snapshot {
                    sequence,
                    as_of_sequence,
                    updates,
                    statuses,
                    ..
                } => Some((*sequence, *as_of_sequence, updates, statuses)),
                _ => None,
            })
            .expect("periodic snapshot emits");
        assert!(snapshot.1 < snapshot.0);
        assert!(snapshot.2.is_empty());
        assert_eq!(snapshot.3.len(), SPOT_SIGNAL_SET_COUNT);
        assert!(snapshot.3.windows(2).all(|statuses| {
            (
                &statuses[0].signal_id,
                &statuses[0].signal_version,
                &statuses[0].scope,
            ) < (
                &statuses[1].signal_id,
                &statuses[1].signal_version,
                &statuses[1].scope,
            )
        }));
        assert_eq!(pipeline.updates_written, 0);
        assert_eq!(
            pipeline.stream_messages_written,
            u64::try_from(messages.len()).expect("message count fits")
        );
    }

    #[test]
    fn stream_control_and_data_replay_byte_identically() {
        let inputs = [
            JournalInput::HealthTick {
                admission_seq: 0,
                now_ns: 0,
            },
            JournalInput::HealthTick {
                admission_seq: 1,
                now_ns: SIGNAL_HEARTBEAT_INTERVAL_NS,
            },
            JournalInput::HealthTick {
                admission_seq: 2,
                now_ns: SIGNAL_SNAPSHOT_INTERVAL_NS,
            },
        ];
        let run = || {
            let processor = SpotSignalProcessor::new("replay-stream", wall_start())
                .expect("processor constructs");
            let mut pipeline =
                SignalPipeline::new_replay(processor, wall_start(), Vec::new(), 1_000, 1_000_000);
            for input in inputs.clone() {
                pipeline.apply_replay(input).expect("journal input replays");
            }
            pipeline.writer
        };
        assert_eq!(run(), run());
    }

    #[test]
    fn feed_health_snapshot_tracks_admitted_availability_and_is_deterministic() {
        use crate::feed_health::{LivenessBoundSource, MISSED_HEARTBEAT_ALLOWANCE};
        use signals_core::FeedHealthStatus as FeedStatus;

        let run = || {
            let processor = SpotSignalProcessor::new("health-test", wall_start())
                .expect("processor constructs");
            let mut pipeline =
                SignalPipeline::new_replay(processor, wall_start(), Vec::new(), 1_000, 1_000_000);
            pipeline
                .apply_replay(JournalInput::Availability {
                    admission_seq: 0,
                    venue: Venue::Coinbase,
                    reason: UnavailableReason::Disconnected,
                    now_ns: 0,
                })
                .expect("availability replays");
            pipeline
                .apply_replay(JournalInput::HealthTick {
                    admission_seq: 1,
                    now_ns: SIGNAL_HEARTBEAT_INTERVAL_NS,
                })
                .expect("health tick replays");
            pipeline.report().expect("report builds").feed_health
        };

        let snapshot = run();
        // Every consumed spot feed is tracked: two tickers, two books, two trades.
        assert_eq!(snapshot.feeds.len(), 6);
        let coinbase: Vec<_> = snapshot
            .feeds
            .iter()
            .filter(|feed| feed.venue == "coinbase")
            .collect();
        let kraken: Vec<_> = snapshot
            .feeds
            .iter()
            .filter(|feed| feed.venue == "kraken")
            .collect();
        assert_eq!(coinbase.len(), 3);
        assert_eq!(kraken.len(), 3);
        // The admitted Coinbase disconnect marks only Coinbase feeds down.
        assert!(
            coinbase
                .iter()
                .all(|feed| feed.status == FeedStatus::Down && feed.disconnect_count == 1)
        );
        assert!(kraken.iter().all(|feed| feed.disconnect_count == 0));
        // Book-levels feeds derive their bound from the declared heartbeat; the
        // four event-driven feeds carry calibrated overrides (ADR 0014,
        // 2026-08-19).
        let declared_bound_ms = 1_000 * MISSED_HEARTBEAT_ALLOWANCE;
        for feed in &snapshot.feeds {
            let expected = match feed.source_id.as_str() {
                "kraken.spot.btc-usd.trades" => (LivenessBoundSource::Calibrated, 60_000),
                "kraken.spot.btc-usd.ticker" => (LivenessBoundSource::Calibrated, 30_000),
                "coinbase.exchange.btc-usd.trades" | "coinbase.exchange.btc-usd.ticker" => {
                    (LivenessBoundSource::Calibrated, 15_000)
                }
                _ => (LivenessBoundSource::Declared, declared_bound_ms),
            };
            assert_eq!(
                (feed.liveness_bound_source, feed.liveness_bound_ms),
                expected,
                "unexpected liveness bound for {}",
                feed.source_id
            );
        }
        // Same admitted inputs reproduce the same health trajectory.
        assert_eq!(run(), snapshot);
    }

    fn count_feed_health_frames(bytes: &[u8]) -> usize {
        bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .filter(|line| {
                serde_json::from_slice::<serde_json::Value>(line)
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

    /// Drive a live run that writes a journal, then replay the journal through a
    /// fresh pipeline gated on the recorded session capability. Returns the live
    /// stream bytes, the replay stream bytes, and the replay feed_health count.
    fn feed_health_journal_round_trip(enabled: bool) -> (Vec<u8>, Vec<u8>, usize) {
        const SECOND_NS: u64 = 1_000_000_000;
        let checkpoint_path = TestPath::new("fh-ckpt");
        let journal_path = TestPath::new("fh-journal");
        let mut runtime_config = SpotReferenceRuntimeConfig::new(100).expect("runtime config");
        runtime_config.feed_health_stream = enabled;
        let PreparedSpotSignalRun {
            clock,
            processor,
            session,
            initial_checkpoint,
        } = prepare_spot_signal_run(
            checkpoint_path.as_path(),
            false,
            "fh-epoch".to_owned(),
            DEFAULT_CHECKPOINT_MAX_BYTES,
            runtime_config.clone(),
        )
        .expect("fresh run prepares");
        let journal =
            SignalJournalWriter::create(journal_path.as_path(), session.clone(), 4_194_304, 8)
                .expect("journal creates");
        let checkpoint = CheckpointWriter::create(
            checkpoint_path.as_path(),
            initial_checkpoint,
            DEFAULT_CHECKPOINT_MAX_BYTES,
            8,
            false,
        )
        .expect("checkpoint creates");
        let checkpoint_context = CheckpointContext {
            session_id: session.session_id.clone(),
            wall_start: session.wall_start.clone(),
            contract_fingerprint: session.contract_fingerprint.clone(),
            runtime_config: runtime_config.clone(),
        };
        let mut pipeline = SignalPipeline::new_live(
            processor,
            clock.wall_start(),
            Vec::new(),
            10_000,
            4_194_304,
            LiveDurability {
                journal,
                checkpoint,
                checkpoint_context,
            },
        )
        .with_feed_health_stream(runtime_config.feed_health_stream);
        pipeline
            .mark_unavailable_live(Venue::Coinbase, UnavailableReason::Disconnected, 0)
            .expect("availability admits");
        pipeline.advance_live(SECOND_NS).expect("first tick admits");
        pipeline
            .advance_live(2 * SECOND_NS)
            .expect("second tick admits");
        let live_bytes = pipeline.writer.clone();
        pipeline.complete_journal().expect("journal completes");
        pipeline
            .complete_checkpoint(2 * SECOND_NS)
            .expect("checkpoint completes");

        let mut reader =
            JournalReplayReader::open(journal_path.as_path(), 4_194_304).expect("journal opens");
        let replay_session = reader.session().clone();
        let processor =
            SpotSignalProcessor::new(&replay_session.publisher_epoch, clock.wall_start())
                .expect("replay processor constructs");
        let mut replay = SignalPipeline::new_replay(
            processor,
            clock.wall_start(),
            Vec::new(),
            10_000,
            4_194_304,
        )
        .with_feed_health_stream(replay_session.runtime_config.feed_health_stream);
        while let Some(input) = reader.next_input().expect("journal input reads") {
            replay.apply_replay(input).expect("journal input replays");
        }
        let replay_bytes = replay.writer.clone();
        let frames = count_feed_health_frames(&replay_bytes);
        if enabled {
            preserve_v3_corpus_lane("spot", journal_path.as_path(), &live_bytes, &replay_bytes);
        }
        (live_bytes, replay_bytes, frames)
    }

    fn preserve_v3_corpus_lane(
        lane: &str,
        journal_path: &Path,
        live_bytes: &[u8],
        replay_bytes: &[u8],
    ) {
        let Some(root) = std::env::var_os("STREAM_V3_CORPUS_DIR") else {
            return;
        };
        let lane_dir = PathBuf::from(root).join(lane);
        std::fs::create_dir_all(&lane_dir).expect("corpus lane directory creates");
        std::fs::copy(journal_path, lane_dir.join("journal.jsonl")).expect("corpus journal copies");
        std::fs::write(lane_dir.join("live.jsonl"), live_bytes).expect("corpus live output writes");
        std::fs::write(lane_dir.join("replay.jsonl"), replay_bytes)
            .expect("corpus replay output writes");
    }

    #[test]
    fn feed_health_journal_round_trip_emits_and_replays_identically_when_enabled() {
        let (live, replay, frames) = feed_health_journal_round_trip(true);
        assert_eq!(live, replay, "an enabled session replays byte-identically");
        assert!(frames > 0, "an enabled session emits feed_health frames");
    }

    #[test]
    fn feed_health_journal_round_trip_is_silent_and_identical_when_disabled() {
        let (live, replay, frames) = feed_health_journal_round_trip(false);
        assert_eq!(
            live, replay,
            "a pre-capability session replays byte-identically"
        );
        assert_eq!(
            frames, 0,
            "a session without the capability emits zero feed_health frames"
        );
    }

    #[test]
    fn feed_health_emits_on_transition_and_periodic_cadence_only() {
        const SECOND_NS: u64 = 1_000_000_000;
        let processor =
            SpotSignalProcessor::new("fh-policy", wall_start()).expect("processor constructs");
        let mut pipeline =
            SignalPipeline::new_replay(processor, wall_start(), Vec::new(), 1_000, 4_194_304)
                .with_feed_health_stream(true);

        let mut seq = 0_u64;
        let mut tick = |pipeline: &mut SignalPipeline<Vec<u8>>, now_ns: u64| {
            pipeline
                .apply_replay(JournalInput::HealthTick {
                    admission_seq: seq,
                    now_ns,
                })
                .expect("health tick replays");
            seq += 1;
            count_feed_health_frames(&pipeline.writer)
        };

        // First admission emits the initial board state.
        assert_eq!(tick(&mut pipeline, 0), 1);
        // Sustained sickness at 30s is a transition: a new frame.
        assert_eq!(tick(&mut pipeline, 30 * SECOND_NS), 2);
        // One second later nothing changed and the periodic deadline is far off:
        // no per-event spam.
        assert_eq!(tick(&mut pipeline, 31 * SECOND_NS), 2);
        // The periodic cadence (60s after the last emission at 30s) forces a
        // refresh even without a transition.
        assert_eq!(tick(&mut pipeline, 90 * SECOND_NS), 3);
    }

    #[test]
    fn invalid_book_admission_requests_only_its_live_venue_recovery() {
        let processor =
            SpotSignalProcessor::new("book-recovery", wall_start()).expect("processor constructs");
        let mut pipeline =
            SignalPipeline::new_replay(processor, wall_start(), Vec::new(), 10, 100_000);
        let event_time = "2026-08-13T12:00:01Z".to_owned();
        let event = SourceEvent {
            schema_version: 1,
            source_id: "coinbase.exchange.btc-usd.book-levels".to_owned(),
            source_version: "1.0.0".to_owned(),
            publisher_epoch: "coinbase-book".to_owned(),
            sequence: 0,
            event_time: event_time.clone(),
            received_at: event_time.clone(),
            values: BTreeMap::from([
                ("instrument".to_owned(), json!("BTC-USD")),
                ("update_group".to_owned(), json!("one-sided")),
                ("update_index".to_owned(), json!(0)),
                ("update_count".to_owned(), json!(1)),
                ("update_kind".to_owned(), json!("snapshot")),
                ("side".to_owned(), json!("bid")),
                ("price".to_owned(), json!("100")),
                ("size".to_owned(), json!("1")),
                ("event_time_basis".to_owned(), json!("receipt")),
                ("event_time".to_owned(), json!(event_time)),
            ]),
        };
        let recoveries = pipeline
            .apply_input(JournalInput::MarketEvents {
                admission_seq: 0,
                events: Arc::new(vec![event]),
                logical_receipt_ns: 1_000_000_000,
                now_ns: 1_000_000_000,
                resync: false,
            })
            .expect("invalid completed book is contained and admitted");
        assert_eq!(recoveries, BTreeSet::from([Venue::Coinbase]));
        assert_eq!(pipeline.updates_written, 0);
    }

    #[test]
    fn resume_startup_replaces_checkpoint_journals_guards_and_rejects_lineage_reuse() {
        let checkpoint_path = TestPath::new("resume-checkpoint");
        let journal_path = TestPath::new("resume-journal");
        let runtime_config =
            SpotReferenceRuntimeConfig::new(100).expect("runtime config constructs");
        let mut original = SpotSignalProcessor::new("signal-e1", wall_start())
            .expect("original processor constructs");
        let emitted = original
            .advance_time(0)
            .expect("original processor emits before checkpoint");
        assert_eq!(emitted.len(), SPOT_SIGNAL_SET_COUNT);
        let original_checkpoint = CheckpointDocument::new(
            "0123456789abcdef0123456789abcdef".to_owned(),
            wall_start().to_rfc3339(),
            0,
            0,
            crate::journal::contract_fingerprint(&runtime_config)
                .expect("fingerprint constructs"),
            runtime_config.clone(),
            original.checkpoint_state(),
        )
        .expect("original checkpoint constructs");
        CheckpointWriter::create(
            checkpoint_path.as_path(),
            original_checkpoint,
            DEFAULT_CHECKPOINT_MAX_BYTES,
            2,
            false,
        )
        .expect("original checkpoint persists")
        .abort()
        .expect("original writer stops");

        let mismatch = prepare_spot_signal_run(
            checkpoint_path.as_path(),
            true,
            "signal-e2".to_owned(),
            DEFAULT_CHECKPOINT_MAX_BYTES,
            SpotReferenceRuntimeConfig::new(10).expect("mismatch config constructs"),
        )
        .err()
        .expect("runtime mismatch rejects resume");
        assert!(
            mismatch
                .to_string()
                .contains("runtime configuration does not match")
        );

        let PreparedSpotSignalRun {
            clock,
            processor,
            session,
            initial_checkpoint,
        } = prepare_spot_signal_run(
            checkpoint_path.as_path(),
            true,
            "signal-e2".to_owned(),
            DEFAULT_CHECKPOINT_MAX_BYTES,
            runtime_config.clone(),
        )
        .expect("compatible checkpoint resumes");
        assert_eq!(
            session
                .initial_checkpoint
                .as_ref()
                .expect("resumed journal embeds prior checkpoint")
                .processor_state
                .reference
                .publisher_epoch_lineage,
            ["signal-e1"]
        );
        assert_eq!(
            initial_checkpoint
                .processor_state
                .reference
                .publisher_epoch_lineage,
            ["signal-e1", "signal-e2"]
        );
        assert!(
            initial_checkpoint
                .processor_state
                .features
                .values()
                .all(|feature| feature.previous_next_sequence == 0
                    && feature.lifetime_output_count == 1)
        );

        let journal =
            SignalJournalWriter::create(journal_path.as_path(), session.clone(), 1_048_576, 8)
                .expect("resumed journal creates");
        let checkpoint = CheckpointWriter::create(
            checkpoint_path.as_path(),
            initial_checkpoint,
            DEFAULT_CHECKPOINT_MAX_BYTES,
            8,
            true,
        )
        .expect("resumed checkpoint atomically replaces prior state");
        let checkpoint_context = CheckpointContext {
            session_id: session.session_id.clone(),
            wall_start: session.wall_start.clone(),
            contract_fingerprint: session.contract_fingerprint.clone(),
            runtime_config,
        };
        let mut pipeline = SignalPipeline::new_live(
            processor,
            clock.wall_start(),
            Vec::new(),
            100,
            1_048_576,
            LiveDurability {
                journal,
                checkpoint,
                checkpoint_context,
            },
        );
        let now_ns = clock.elapsed_ns().expect("resumed clock reads");
        mark_resumed_sources_unavailable(&mut pipeline, now_ns)
            .expect("mandatory startup guards are journaled and applied");
        assert_eq!(pipeline.updates_written, 0);
        pipeline
            .complete_journal()
            .expect("resumed journal completes");
        pipeline
            .complete_checkpoint(now_ns)
            .expect("resumed checkpoint completes");

        let mut reader = JournalReplayReader::open(journal_path.as_path(), 1_048_576)
            .expect("resumed journal opens");
        assert_eq!(reader.session().publisher_epoch, "signal-e2");
        assert!(matches!(
            reader.next_input().expect("Coinbase guard reads"),
            Some(JournalInput::Availability {
                admission_seq: 0,
                venue: Venue::Coinbase,
                reason: UnavailableReason::Disconnected,
                ..
            })
        ));
        assert!(matches!(
            reader.next_input().expect("Kraken guard reads"),
            Some(JournalInput::Availability {
                admission_seq: 1,
                venue: Venue::Kraken,
                reason: UnavailableReason::Disconnected,
                ..
            })
        ));
        assert!(reader.next_input().expect("journal end reads").is_none());
        reader.end().expect("journal has a trusted terminal");

        let final_checkpoint =
            load_checkpoint(checkpoint_path.as_path(), DEFAULT_CHECKPOINT_MAX_BYTES)
                .expect("resumed checkpoint reloads");
        assert_eq!(
            final_checkpoint
                .processor_state
                .reference
                .publisher_epoch_lineage,
            ["signal-e1", "signal-e2"]
        );
        let reuse = prepare_spot_signal_run(
            checkpoint_path.as_path(),
            true,
            "signal-e1".to_owned(),
            DEFAULT_CHECKPOINT_MAX_BYTES,
            final_checkpoint.runtime_config,
        )
        .err()
        .expect("any prior epoch in the lineage rejects resume");
        assert!(reuse.to_string().contains("checkpoint processor state"));
    }

    // --- C5 in-process file-generation rotation (spot) --------------------
    //
    // These drive a live spot pipeline across a rotation and replay each
    // generation's journal at frame level (not via `replay-spot-signals`, whose
    // terminal-count self-check a warm gen>=1 journal cannot satisfy — a
    // standalone replay honestly re-warms and emits fewer ready updates; see the
    // build report). Ground truth for what survives a rotation replay: only the
    // reference quote is checkpointed (spot_reference.rs), so `spot_reference`
    // reproduces byte-identically in every generation, while every ring/book-
    // backed identity re-warms on a gen>=1 replay.

    use crate::{REALIZED_VOLATILITY_ID, SPOT_REFERENCE_ID};

    const ROTATION_MAX_JOURNAL_BYTES: u64 = DEFAULT_MAX_JOURNAL_BYTES;

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(label: &str) -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock follows Unix epoch")
                .as_nanos();
            let seq = TEST_PATH_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "btc-signal-rotation-{label}-{}-{unique}-{seq}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("lane root is created");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn rotation_ns(second: u64) -> u64 {
        second * 1_000_000_000
    }

    fn rotation_timestamp(second: u64) -> String {
        wall_start()
            .checked_add_signed(ChronoDuration::seconds(
                i64::try_from(second).expect("second fits i64"),
            ))
            .expect("timestamp is representable")
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    }

    fn rotation_source_event(
        source_id: &str,
        source_version: &str,
        publisher_epoch: &str,
        sequence: u64,
        second: u64,
        values: BTreeMap<String, serde_json::Value>,
    ) -> SourceEvent {
        let event_time = rotation_timestamp(second);
        SourceEvent {
            schema_version: 1,
            source_id: source_id.to_owned(),
            source_version: source_version.to_owned(),
            publisher_epoch: publisher_epoch.to_owned(),
            sequence,
            event_time: event_time.clone(),
            received_at: event_time,
            values,
        }
    }

    fn rotation_coinbase_ticker(second: u64) -> SourceEvent {
        let event_time = rotation_timestamp(second);
        rotation_source_event(
            "coinbase.exchange.btc-usd.ticker",
            "1.0.0",
            "coinbase-feed",
            second,
            second,
            BTreeMap::from([
                ("instrument".to_owned(), json!("BTC-USD")),
                ("trade_id".to_owned(), json!(10_000 + second)),
                ("price".to_owned(), json!("100.01")),
                ("last_size".to_owned(), json!("0.1")),
                ("best_bid".to_owned(), json!("100")),
                ("best_bid_size".to_owned(), json!("2")),
                ("best_ask".to_owned(), json!("100.02")),
                ("best_ask_size".to_owned(), json!("1")),
                ("open_24h".to_owned(), json!("100")),
                ("volume_24h".to_owned(), json!("1000")),
                ("low_24h".to_owned(), json!("90")),
                ("high_24h".to_owned(), json!("110")),
                ("volume_30d".to_owned(), json!("30000")),
                ("event_time".to_owned(), json!(event_time)),
                ("source_sequence".to_owned(), json!(20_000 + second)),
            ]),
        )
    }

    fn rotation_kraken_ticker(second: u64) -> SourceEvent {
        let event_time = rotation_timestamp(second);
        rotation_source_event(
            "kraken.spot.btc-usd.ticker",
            "1.0.0",
            "kraken-feed",
            second,
            second,
            BTreeMap::from([
                ("instrument".to_owned(), json!("BTC/USD")),
                ("best_bid".to_owned(), json!("100")),
                ("best_bid_size".to_owned(), json!("1")),
                ("best_ask".to_owned(), json!("100.02")),
                ("best_ask_size".to_owned(), json!("2")),
                ("last_price".to_owned(), json!("100.01")),
                ("volume_24h".to_owned(), json!("1000")),
                ("vwap_24h".to_owned(), json!("100.01")),
                ("low_24h".to_owned(), json!("90")),
                ("high_24h".to_owned(), json!("110")),
                ("change_24h".to_owned(), json!("1")),
                ("change_percent_24h".to_owned(), json!("1")),
                ("trade_count_24h".to_owned(), json!(1000)),
                ("update_kind".to_owned(), json!("update")),
                ("event_time".to_owned(), json!(event_time)),
            ]),
        )
    }

    fn rotation_coinbase_trade(sequence: u64) -> SourceEvent {
        rotation_source_event(
            "coinbase.exchange.btc-usd.trades",
            "1.0.0",
            "coinbase-trades",
            sequence,
            sequence,
            BTreeMap::from([
                ("instrument".to_owned(), json!("BTC-USD")),
                ("trade_id".to_owned(), json!(sequence)),
                ("price".to_owned(), json!("100.01")),
                ("size".to_owned(), json!("1")),
                ("maker_side".to_owned(), json!("sell")),
                ("aggressor_side".to_owned(), json!("buy")),
                ("event_time".to_owned(), json!(rotation_timestamp(sequence))),
            ]),
        )
    }

    fn rotation_kraken_trade(sequence: u64, second: u64, update_kind: &str) -> SourceEvent {
        rotation_source_event(
            "kraken.spot.btc-usd.trades",
            "1.0.0",
            "kraken-trades",
            sequence,
            second,
            BTreeMap::from([
                ("instrument".to_owned(), json!("BTC/USD")),
                ("trade_id".to_owned(), json!(sequence)),
                ("price".to_owned(), json!("100.01")),
                ("size".to_owned(), json!("0.5")),
                ("aggressor_side".to_owned(), json!("sell")),
                ("order_type".to_owned(), json!("market")),
                ("update_kind".to_owned(), json!(update_kind)),
                ("event_time".to_owned(), json!(rotation_timestamp(second))),
            ]),
        )
    }

    /// A book-levels source row. `sequence` is per-book-source and must be
    /// contiguous across the run (the ordering cursor advances by one per row);
    /// `group` is shared by every row of one atomic group and unique across
    /// groups.
    #[allow(clippy::too_many_arguments)]
    fn rotation_book_event(
        source_id: &str,
        publisher_epoch: &str,
        extra: Vec<(&str, serde_json::Value)>,
        sequence: u64,
        group: &str,
        index: u32,
        count: u32,
        kind: &str,
        side: &str,
        price: &str,
        size: &str,
        second: u64,
    ) -> SourceEvent {
        let event_time = rotation_timestamp(second);
        let mut values: BTreeMap<String, serde_json::Value> = extra
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect();
        values.insert("update_group".to_owned(), json!(group));
        values.insert("update_index".to_owned(), json!(index));
        values.insert("update_count".to_owned(), json!(count));
        values.insert("update_kind".to_owned(), json!(kind));
        values.insert("side".to_owned(), json!(side));
        values.insert("price".to_owned(), json!(price));
        values.insert("size".to_owned(), json!(size));
        values.insert("event_time".to_owned(), json!(event_time));
        rotation_source_event(
            source_id,
            "1.0.0",
            publisher_epoch,
            sequence,
            second,
            values,
        )
    }

    /// The Coinbase book rows for one warming second: a two-row snapshot (one
    /// bid, one ask) at second 1, then a single value-neutral bid delta each
    /// later second. Book sequences run 100.. contiguously, so at any rotation
    /// the cursor sits far above the two-level resync count and the synthesized
    /// resync rows land at or below it (ordering-skipped on the warm live run).
    fn rotation_coinbase_book_rows(second: u64) -> Vec<SourceEvent> {
        let extra = || {
            vec![
                ("instrument", json!("BTC-USD")),
                ("event_time_basis", json!("receipt")),
            ]
        };
        if second == 1 {
            vec![
                rotation_book_event(
                    "coinbase.exchange.btc-usd.book-levels",
                    "coinbase-book",
                    extra(),
                    100,
                    "cb-book-snap",
                    0,
                    2,
                    "snapshot",
                    "bid",
                    "100",
                    "2",
                    second,
                ),
                rotation_book_event(
                    "coinbase.exchange.btc-usd.book-levels",
                    "coinbase-book",
                    extra(),
                    101,
                    "cb-book-snap",
                    1,
                    2,
                    "snapshot",
                    "ask",
                    "100.02",
                    "1",
                    second,
                ),
            ]
        } else {
            let sequence = 100 + second;
            vec![rotation_book_event(
                "coinbase.exchange.btc-usd.book-levels",
                "coinbase-book",
                extra(),
                sequence,
                &format!("cb-book-{sequence}"),
                0,
                1,
                "delta",
                "bid",
                "100",
                "2",
                second,
            )]
        }
    }

    /// The Kraken book rows for one warming second, mirroring the Coinbase shape
    /// on a disjoint sequence range (200..) with Kraken's declared static fields.
    fn rotation_kraken_book_rows(second: u64) -> Vec<SourceEvent> {
        let extra = || {
            vec![
                ("instrument", json!("BTC/USD")),
                ("configured_depth", json!(SPOT_SIGNAL_KRAKEN_BOOK_DEPTH)),
                ("change_origin", json!("provider")),
                ("book_checksum", json!(0_u32)),
            ]
        };
        if second == 1 {
            vec![
                rotation_book_event(
                    "kraken.spot.btc-usd.book-levels",
                    "kraken-book",
                    extra(),
                    200,
                    "kr-book-snap",
                    0,
                    2,
                    "snapshot",
                    "bid",
                    "100",
                    "2",
                    second,
                ),
                rotation_book_event(
                    "kraken.spot.btc-usd.book-levels",
                    "kraken-book",
                    extra(),
                    201,
                    "kr-book-snap",
                    1,
                    2,
                    "snapshot",
                    "ask",
                    "100.02",
                    "1",
                    second,
                ),
            ]
        } else {
            let sequence = 200 + second;
            vec![rotation_book_event(
                "kraken.spot.btc-usd.book-levels",
                "kraken-book",
                extra(),
                sequence,
                &format!("kr-book-{sequence}"),
                0,
                1,
                "delta",
                "bid",
                "100",
                "2",
                second,
            )]
        }
    }

    /// One venue's admitted source events for a warming second (journal batches
    /// are single-venue). Ticker + trade + book-levels: the book snapshot is
    /// established in generation 0 and never checkpointed, so a generation > 0
    /// journal begins with book deltas that a cold standalone replay could not
    /// admit — until the C5 rotation resync (admitted as the first records of the
    /// new generation) rebuilds the book. These fixtures therefore exercise the
    /// full rotation replay: the checkpointed `spot_reference` (byte-identical
    /// across a generation), the ring-backed `realized_volatility`/flow identities
    /// (honest re-warm), AND the book-backed identities the resync makes
    /// replayable.
    fn rotation_coinbase_second_events(second: u64) -> Vec<SourceEvent> {
        let mut events = vec![
            rotation_coinbase_ticker(second),
            rotation_coinbase_trade(second),
        ];
        events.extend(rotation_coinbase_book_rows(second));
        events
    }

    fn rotation_kraken_second_events(second: u64) -> Vec<SourceEvent> {
        let mut events = Vec::new();
        if second == 1 {
            events.push(rotation_kraken_trade(0, 0, "snapshot"));
        }
        events.push(rotation_kraken_ticker(second));
        events.push(rotation_kraken_trade(second, second, "update"));
        events.extend(rotation_kraken_book_rows(second));
        events
    }

    fn admit_market_input(
        pipe: &mut SignalPipeline<BufWriter<File>>,
        admission_seq: &mut u64,
        events: Vec<SourceEvent>,
        second: u64,
    ) {
        let market = JournalInput::MarketEvents {
            admission_seq: *admission_seq,
            events: Arc::new(events),
            logical_receipt_ns: rotation_ns(second),
            now_ns: rotation_ns(second),
            resync: false,
        };
        pipe.enqueue(market.clone()).expect("market enqueues");
        pipe.apply_input(market).expect("market applies");
        *admission_seq += 1;
    }

    fn admit_second_live(
        pipeline: &Rc<RefCell<SignalPipeline<BufWriter<File>>>>,
        admission_seq: &mut u64,
        second: u64,
    ) {
        let mut pipe = pipeline.borrow_mut();
        admit_market_input(
            &mut pipe,
            admission_seq,
            rotation_coinbase_second_events(second),
            second,
        );
        admit_market_input(
            &mut pipe,
            admission_seq,
            rotation_kraken_second_events(second),
            second,
        );
        let tick = JournalInput::HealthTick {
            admission_seq: *admission_seq,
            now_ns: rotation_ns(second),
        };
        pipe.enqueue(tick.clone()).expect("tick enqueues");
        pipe.apply_input(tick).expect("tick applies");
        *admission_seq += 1;
    }

    struct RotationFixture {
        _lane_root: TestDir,
        gen0_dir: PathBuf,
        gen1_dir: PathBuf,
        gen0_epoch: String,
        gen1_epoch: String,
    }

    #[allow(clippy::type_complexity)]
    /// Build a fresh rotation harness: an open generation 0, a live pipeline and
    /// a controller wired for `period_secs` rotations. Returns the lane-root
    /// guard (keep it alive for the test's duration), the pipeline, the
    /// controller, the shared checkpoint path, the epoch prefix and generation
    /// 0's directory.
    fn build_rotation_harness(
        period_secs: u64,
    ) -> (
        TestDir,
        Rc<RefCell<SignalPipeline<BufWriter<File>>>>,
        Rc<RefCell<RotationController>>,
        PathBuf,
        &'static str,
        PathBuf,
    ) {
        let lane_root = TestDir::new("lane");
        let epoch_prefix = "live-spot-rotation";
        let runtime_config =
            SpotReferenceRuntimeConfig::new(SPOT_SIGNAL_KRAKEN_BOOK_DEPTH).expect("runtime config");
        let caps = FileGenerationCaps {
            max_updates: DEFAULT_MAX_UPDATES,
            max_output_bytes: DEFAULT_MAX_BYTES,
            max_journal_bytes: ROTATION_MAX_JOURNAL_BYTES,
        };
        let checkpoint_path = lane_root.path().join("checkpoint.json");

        let layout =
            FileGenerationLayout::new(lane_root.path(), SPOT_LANE_NAME).expect("layout binds");
        let (gen0_unix, gen0_started) = generation_stamps(wall_start(), 0);
        let gen0_meta = spot_run_meta(
            epoch_prefix,
            gen0_unix,
            &gen0_started,
            period_secs,
            &caps,
            2,
            false,
        );
        let gen0 = layout
            .open_next_with_rotation(gen0_unix, None, &gen0_meta, period_secs)
            .expect("gen0 opens");
        let gen0_dir = gen0.dir.clone();

        let processor = SpotSignalProcessor::new(epoch_prefix, wall_start()).expect("processor");
        let session = JournalSession::new(
            wall_start().to_rfc3339(),
            epoch_prefix.to_owned(),
            runtime_config.clone(),
        )
        .expect("session");
        let initial_checkpoint = CheckpointDocument::new(
            session.session_id.clone(),
            session.wall_start.clone(),
            0,
            0,
            session.contract_fingerprint.clone(),
            runtime_config.clone(),
            processor.checkpoint_state(),
        )
        .expect("initial checkpoint");
        let writers = open_generation_writers(
            &gen0.output_path,
            &gen0.journal_path,
            &checkpoint_path,
            &session,
            initial_checkpoint,
            caps.max_journal_bytes,
            DEFAULT_JOURNAL_QUEUE_CAPACITY,
            DEFAULT_CHECKPOINT_MAX_BYTES,
            DEFAULT_CHECKPOINT_QUEUE_CAPACITY,
            false,
            &runtime_config,
        )
        .expect("gen0 writers");
        let pipeline = Rc::new(RefCell::new(
            SignalPipeline::new_live(
                processor,
                wall_start(),
                BufWriter::new(writers.output_file),
                caps.max_updates,
                caps.max_output_bytes,
                writers.durability,
            )
            .with_feed_health_stream(runtime_config.feed_health_stream),
        ));

        let controller = Rc::new(RefCell::new(RotationController {
            layout,
            schedule: RotationSchedule::try_new(0, Duration::from_secs(period_secs))
                .expect("schedule"),
            current: gen0,
            wall_start: wall_start(),
            epoch_prefix: epoch_prefix.to_owned(),
            rotate_period_secs: period_secs,
            caps,
            headroom_factor: 2,
            checkpoint_path: checkpoint_path.clone(),
            journal_queue_capacity: DEFAULT_JOURNAL_QUEUE_CAPACITY,
            max_checkpoint_bytes: DEFAULT_CHECKPOINT_MAX_BYTES,
            checkpoint_queue_capacity: DEFAULT_CHECKPOINT_QUEUE_CAPACITY,
            runtime_config,
        }));

        (
            lane_root,
            pipeline,
            controller,
            checkpoint_path,
            epoch_prefix,
            gen0_dir,
        )
    }

    /// Drive a live spot pipeline through exactly one in-process rotation:
    /// `gen0_secs` warming seconds, a `rotate_generation` boundary, then
    /// `gen1_secs` more seconds, sealing both generations on disk.
    fn drive_one_rotation(gen0_secs: u64, gen1_secs: u64) -> RotationFixture {
        let (lane_root, pipeline, controller, _checkpoint_path, epoch_prefix, gen0_dir) =
            build_rotation_harness(60);

        let mut admission_seq = 0_u64;
        for second in 1..=gen0_secs {
            admit_second_live(&pipeline, &mut admission_seq, second);
        }

        // A fresh process clock: its `elapsed_ns()` is a few microseconds, far
        // below the synthetic `rotation_ns(gen0_secs)` boundary, so the post-seal
        // `advance_to` sees `now_ns` exactly as the pre-seal path did.
        let clock = ProcessClock::start();
        rotate_generation(&pipeline, &controller, &clock, rotation_ns(gen0_secs))
            .expect("rotation seals gen0 and opens gen1");
        let gen1_dir = controller.borrow().current.dir.clone();

        // The rotation admitted the book resync as the new generation's first
        // records, so its admission sequence has already advanced past zero;
        // continue live admission from where the resync left off.
        let mut admission_seq = pipeline.borrow().next_admission_seq;
        for second in (gen0_secs + 1)..=(gen0_secs + gen1_secs) {
            admit_second_live(&pipeline, &mut admission_seq, second);
        }

        // Final graceful end on the current (gen1) generation.
        {
            let mut pipe = pipeline.borrow_mut();
            pipe.sync_output().expect("final sync");
            pipe.complete_journal().expect("final journal");
            pipe.complete_checkpoint(rotation_ns(gen0_secs + gen1_secs))
                .expect("final checkpoint");
            let (_, sealed_utc) =
                generation_stamps(wall_start(), rotation_ns(gen0_secs + gen1_secs));
            controller
                .borrow_mut()
                .current
                .seal(sealed_utc)
                .expect("final seal");
        }
        drop(pipeline);

        RotationFixture {
            _lane_root: lane_root,
            gen0_dir,
            gen1_dir,
            gen0_epoch: epoch_prefix.to_string(),
            // The rotated generation is named from the SCHEDULED deadline (the
            // 60 s rotation period from the first open at ns 0), not the observed
            // rotation instant `rotation_ns(gen0_secs)`.
            gen1_epoch: format!(
                "{epoch_prefix}-{}",
                generation_stamps(wall_start(), 60 * 1_000_000_000).0
            ),
        }
    }

    /// Replay one generation's journal at frame level (bypassing the
    /// `replay-spot-signals` terminal-count self-check, which a warm gen>=1
    /// journal cannot satisfy), returning the produced output bytes.
    fn replay_generation_frames(journal_path: &Path) -> Vec<u8> {
        let mut reader = JournalReplayReader::open(journal_path, ROTATION_MAX_JOURNAL_BYTES)
            .expect("journal opens");
        let session = reader.session().clone();
        let wall = DateTime::parse_from_rfc3339(&session.wall_start)
            .expect("wall start parses")
            .with_timezone(&Utc);
        let processor = if let Some(checkpoint) = &session.initial_checkpoint {
            SpotSignalProcessor::restore(
                &session.publisher_epoch,
                wall,
                checkpoint.processor_state.clone(),
            )
        } else {
            SpotSignalProcessor::new(&session.publisher_epoch, wall)
        }
        .expect("replay processor");
        let mut pipeline = SignalPipeline::new_replay(
            processor,
            wall,
            Vec::new(),
            DEFAULT_MAX_UPDATES,
            DEFAULT_MAX_BYTES,
        )
        .with_feed_health_stream(session.runtime_config.feed_health_stream);
        while let Some(input) = reader.next_input().expect("journal input validates") {
            pipeline.apply_replay(input).expect("journal input replays");
        }
        pipeline.writer
    }

    /// Replay a generation's journal like [`replay_generation_frames`], but drop
    /// every ask-side row from each resync batch before applying — simulating a
    /// rotation resync that emitted bids only. Returns `Err` if a replay input is
    /// rejected (an incomplete book snapshot leaves a later real delta
    /// unadmittable), which is exactly the fault a faithful resync prevents.
    fn replay_generation_frames_bids_only_resync(journal_path: &Path) -> Result<Vec<u8>> {
        let mut reader = JournalReplayReader::open(journal_path, ROTATION_MAX_JOURNAL_BYTES)?;
        let session = reader.session().clone();
        let wall = DateTime::parse_from_rfc3339(&session.wall_start)?.with_timezone(&Utc);
        let processor = if let Some(checkpoint) = &session.initial_checkpoint {
            SpotSignalProcessor::restore(
                &session.publisher_epoch,
                wall,
                checkpoint.processor_state.clone(),
            )
        } else {
            SpotSignalProcessor::new(&session.publisher_epoch, wall)
        }?;
        let mut pipeline = SignalPipeline::new_replay(
            processor,
            wall,
            Vec::new(),
            DEFAULT_MAX_UPDATES,
            DEFAULT_MAX_BYTES,
        )
        .with_feed_health_stream(session.runtime_config.feed_health_stream);
        while let Some(input) = reader.next_input()? {
            let input = match input {
                JournalInput::MarketEvents {
                    admission_seq,
                    events,
                    logical_receipt_ns,
                    now_ns,
                    resync: true,
                } => {
                    let bids_only: Vec<SourceEvent> = events
                        .iter()
                        .filter(|event| {
                            event.values.get("side").and_then(serde_json::Value::as_str)
                                != Some("ask")
                        })
                        .cloned()
                        .collect();
                    JournalInput::MarketEvents {
                        admission_seq,
                        events: Arc::new(bids_only),
                        logical_receipt_ns,
                        now_ns,
                        resync: true,
                    }
                }
                other => other,
            };
            pipeline.apply_replay(input)?;
        }
        Ok(pipeline.writer)
    }

    #[test]
    fn a_bids_only_resync_makes_the_rotation_replay_fail_spot() {
        // Mutation-style guard: corrupt the rotation resync (drop every ask row)
        // and prove the replay can no longer reproduce the warm run. This is what
        // makes the per-identity `assert_non_ring_replays_byte_identically`
        // non-vacuous — with a faithful resync those identities match, with a
        // broken one they must not.
        let fixture = drive_one_rotation(61, 5);
        let live_frames = parse_frames(
            &std::fs::read(fixture.gen1_dir.join("output.jsonl")).expect("gen1 output"),
        );

        match replay_generation_frames_bids_only_resync(&fixture.gen1_dir.join("journal.jsonl")) {
            Err(_) => {
                // The incomplete book snapshot left a later real delta
                // unadmittable: the replay failed outright, exactly the fault the
                // faithful resync exists to prevent.
            }
            Ok(replay) => {
                // If it did not error, the book-backed non-ring identities must
                // have diverged from the warm run: a one-sided book cannot
                // reproduce them.
                let replay_frames = parse_frames(&replay);
                let diverged = non_ring_catalogue_ids().iter().any(|id| {
                    ready_updates_for(&live_frames, id) != ready_updates_for(&replay_frames, id)
                });
                assert!(
                    diverged,
                    "a bids-only resync must break at least one non-ring book-backed identity's \
                     replay; if nothing diverged the per-identity comparison would be vacuous"
                );
            }
        }
    }

    /// Every `signal_batch` frame's ready updates for `signal_id`, as canonical
    /// JSON strings (value map, quality, epoch, sequence and all).
    fn ready_updates_for(frames: &[serde_json::Value], signal_id: &str) -> Vec<String> {
        let mut ready = Vec::new();
        for frame in frames {
            if frame.get("type").and_then(|t| t.as_str()) != Some("signal_batch") {
                continue;
            }
            let Some(updates) = frame.get("updates").and_then(|u| u.as_array()) else {
                continue;
            };
            for update in updates {
                if update.get("signal_id").and_then(|s| s.as_str()) == Some(signal_id) {
                    ready.push(update.to_string());
                }
            }
        }
        ready
    }

    /// The set of signal ids that reached a ready `signal_batch` update anywhere
    /// in the frames.
    fn ready_signal_ids(frames: &[serde_json::Value]) -> std::collections::BTreeSet<String> {
        let mut ids = std::collections::BTreeSet::new();
        for frame in frames {
            if frame.get("type").and_then(|t| t.as_str()) != Some("signal_batch") {
                continue;
            }
            let Some(updates) = frame.get("updates").and_then(|u| u.as_array()) else {
                continue;
            };
            for update in updates {
                if let Some(id) = update.get("signal_id").and_then(|s| s.as_str()) {
                    ids.insert(id.to_owned());
                }
            }
        }
        ids
    }

    fn parse_frames(bytes: &[u8]) -> Vec<serde_json::Value> {
        String::from_utf8(bytes.to_vec())
            .expect("output is UTF-8")
            .lines()
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_str(line).expect("frame parses"))
            .collect()
    }

    /// The feature-name suffixes (the segment after the last `.`) of the
    /// RING-backed spot identities: their warm windows/bars/flow history are not
    /// carried by the sealed checkpoint, so a cold replay of a rotation-opened
    /// generation re-warms them. Everything else in the published catalogue is
    /// non-ring and must replay byte-for-byte. This is the ONLY hand-maintained
    /// half of the split — the non-ring set below is derived as catalogue minus
    /// these, so a newly published non-ring identity is covered automatically.
    const RING_IDENTITY_SUFFIXES: &[&str] = &[
        "realized_volatility",
        "micro_momentum",
        "horizon_momentum",
        "horizon_volatility",
        "vwap_deviation",
        "order_flow_imbalance",
        "aggressor_flow",
        "absorption_state",
        "liquidity_shock",
        "early_move_fade_context",
        "effective_spread",
        "kyle_lambda_drift",
        "tick_velocity",
        "atr",
        "rsi",
        "roc",
        "ema_cross",
        "macd",
        "bollinger",
        "zscore",
    ];

    /// The non-ring spot identities, DERIVED as the published signal catalogue
    /// (`spot_signal_contract_documents`) minus the ring identities — not a
    /// hand-picked list. These carry no warm-ring state, so a cold replay of a
    /// rotation-opened generation (its books rebuilt by the resync, its reference
    /// restored from the checkpoint) MUST reproduce each one byte-for-byte.
    fn non_ring_catalogue_ids() -> std::collections::BTreeSet<String> {
        crate::spot_signal_contract_documents()
            .into_iter()
            .map(|(id, _)| id.to_owned())
            // Signal identities only ("btc.*"); the catalogue also lists raw
            // source streams (book/trade/ticker source ids), which never appear
            // as signal_batch updates.
            .filter(|id| id.starts_with("btc."))
            .filter(|id| {
                let suffix = id.rsplit('.').next().unwrap_or(id.as_str());
                !RING_IDENTITY_SUFFIXES.contains(&suffix)
            })
            .collect()
    }

    /// Assert that a cold replay reproduced every non-ring identity's Ready
    /// updates byte-for-byte. The set compared is the catalogue-derived non-ring
    /// set intersected with what the warm live run actually published (some
    /// catalogue identities never warm in the fixture window). Panics on the
    /// first divergence.
    fn assert_non_ring_replays_byte_identically(
        live_frames: &[serde_json::Value],
        replay_frames: &[serde_json::Value],
    ) {
        let non_ring = non_ring_catalogue_ids();
        let live_ready = ready_signal_ids(live_frames);
        let to_check: Vec<&String> = non_ring
            .iter()
            .filter(|id| live_ready.contains(*id))
            .collect();
        assert!(
            to_check.len() >= 10,
            "expected many non-ring identities Ready in the warm gen (got {}); the catalogue/ring split looks wrong",
            to_check.len()
        );
        for id in to_check {
            let live = ready_updates_for(live_frames, id);
            let replay = ready_updates_for(replay_frames, id);
            assert_eq!(
                live, replay,
                "non-ring identity {id} must replay byte-identically after the rotation resync"
            );
        }
    }

    #[test]
    fn each_file_generation_journal_replays_byte_identically_spot() {
        // Warm every identity over gen0, rotate, run a short gen1.
        let fixture = drive_one_rotation(61, 5);

        // Generation 0 is a fresh start, so its journal replays byte-for-byte —
        // exactly today's guarantee.
        let gen0_output =
            std::fs::read(fixture.gen0_dir.join("output.jsonl")).expect("gen0 output");
        let gen0_replay = replay_generation_frames(&fixture.gen0_dir.join("journal.jsonl"));
        assert_eq!(
            gen0_output, gen0_replay,
            "generation 0's journal must replay byte-identically to its output"
        );

        // Generation 1 kept its rings warm in memory across the boundary, but its
        // journal embeds only the sealed checkpoint (the rings are not
        // checkpointed). A standalone replay therefore reproduces the
        // checkpointed identity (spot_reference) byte-for-byte and honestly
        // re-warms every ring/book-backed identity.
        let gen1_output =
            std::fs::read(fixture.gen1_dir.join("output.jsonl")).expect("gen1 output");
        let gen1_replay = replay_generation_frames(&fixture.gen1_dir.join("journal.jsonl"));
        let live_frames = parse_frames(&gen1_output);
        let replay_frames = parse_frames(&gen1_replay);

        // Every non-ring identity — the checkpointed spot_reference AND the
        // book-/quote-backed identities the resync rebuilds — replays
        // byte-identically, not just spot_reference.
        assert_non_ring_replays_byte_identically(&live_frames, &replay_frames);

        let live_ready = ready_signal_ids(&live_frames);
        let replay_ready = ready_signal_ids(&replay_frames);
        assert!(
            live_ready.contains(SPOT_REFERENCE_ID) && replay_ready.contains(SPOT_REFERENCE_ID),
            "spot_reference is ready across the boundary in both live and replay"
        );
        assert!(
            replay_ready.is_subset(&live_ready) && replay_ready.len() < live_ready.len(),
            "gen 1 replay honestly re-warms rings: its ready set is a strict subset of the warm live run's ({} replay vs {} live)",
            replay_ready.len(),
            live_ready.len(),
        );
        assert!(
            live_ready.contains(REALIZED_VOLATILITY_ID)
                && !replay_ready.contains(REALIZED_VOLATILITY_ID),
            "a ring identity (realized_volatility) stays ready in the warm live gen 1 but re-warms in the replay"
        );
    }

    #[test]
    fn resume_after_seal_replays_the_next_generation_spot() {
        let fixture = drive_one_rotation(61, 5);

        // Generation 1's journal carries the checkpoint sealed at generation 0 as
        // its initial checkpoint, recorded exactly as a restart records a resume:
        // the sealed checkpoint's lineage is the prior (gen 0) epoch, and the
        // resumed session runs under the new (gen 1) output epoch.
        let reader = JournalReplayReader::open(
            &fixture.gen1_dir.join("journal.jsonl"),
            ROTATION_MAX_JOURNAL_BYTES,
        )
        .expect("gen1 journal opens");
        let session = reader.session().clone();
        assert_eq!(session.publisher_epoch, fixture.gen1_epoch);
        let embedded = session
            .initial_checkpoint
            .as_ref()
            .expect("gen1 journal embeds the checkpoint sealed at gen0");
        assert_eq!(
            embedded.processor_state.reference.publisher_epoch_lineage,
            vec![fixture.gen0_epoch.clone()],
            "the embedded checkpoint is the one sealed at generation 0"
        );

        // Replaying generation 1 from that sealed checkpoint reproduces the
        // checkpointed identity and honestly re-warms the rings.
        let gen1_output =
            std::fs::read(fixture.gen1_dir.join("output.jsonl")).expect("gen1 output");
        let gen1_replay = replay_generation_frames(&fixture.gen1_dir.join("journal.jsonl"));
        let live_frames = parse_frames(&gen1_output);
        let replay_frames = parse_frames(&gen1_replay);
        assert_eq!(
            ready_updates_for(&live_frames, SPOT_REFERENCE_ID),
            ready_updates_for(&replay_frames, SPOT_REFERENCE_ID),
            "resuming from the sealed checkpoint reproduces spot_reference for gen 1"
        );
        assert!(
            ready_signal_ids(&replay_frames).is_subset(&ready_signal_ids(&live_frames)),
            "the rings re-warm honestly when gen 1 is resumed from the sealed checkpoint"
        );
    }

    #[test]
    fn rotated_generation_replays_through_the_replay_path_and_reports_the_rewarm_gap_spot() {
        let fixture = drive_one_rotation(61, 5);

        // Generation 0 is a fresh start: the production replay path reproduces its
        // terminal counts exactly (`opened_by_rotation` is false).
        replay_spot_signals(
            fixture.gen0_dir.join("journal.jsonl"),
            fixture.gen0_dir.join("replay-out.jsonl"),
            DEFAULT_MAX_UPDATES,
            DEFAULT_MAX_BYTES,
            ROTATION_MAX_JOURNAL_BYTES,
        )
        .expect("generation 0 replays and verifies its terminal counts exactly");

        // Generation 1's journal begins with book deltas; without the rotation
        // resync a cold standalone replay would fatal on the first delta ("book
        // delta arrived before a complete snapshot"). With the resync admitted as
        // the generation's first records, the replay rebuilds the books, and the
        // terminal self-check reports the ring re-warm difference as the
        // documented gap instead of failing.
        replay_spot_signals(
            fixture.gen1_dir.join("journal.jsonl"),
            fixture.gen1_dir.join("replay-out.jsonl"),
            DEFAULT_MAX_UPDATES,
            DEFAULT_MAX_BYTES,
            ROTATION_MAX_JOURNAL_BYTES,
        )
        .expect("rotation-opened generation 1 replays without fatalling and passes the re-warm-tolerant self-check");
    }

    #[test]
    fn verify_replay_counts_tolerates_only_the_ring_rewarm_gap() {
        fn quality(total: u64) -> std::collections::BTreeMap<String, u64> {
            let mut map = std::collections::BTreeMap::new();
            map.insert("warming_up".to_owned(), total);
            map
        }

        // Fresh-start generation: the replay must reproduce the terminal counts
        // exactly; any shortfall or quality mismatch is a fault, and there is no
        // re-warm gap.
        assert_eq!(
            verify_replay_counts(false, 10, 5, &quality(5), 10, 5, &quality(5))
                .expect("exact match verifies"),
            None
        );
        assert!(
            verify_replay_counts(false, 9, 5, &quality(5), 10, 5, &quality(5)).is_err(),
            "a fresh-start update shortfall is a fault"
        );
        assert!(
            verify_replay_counts(false, 10, 5, &quality(4), 10, 5, &quality(5)).is_err(),
            "a fresh-start quality-counter mismatch is a fault"
        );

        // Rotation-opened generation: fewer ready updates is the ring re-warm gap,
        // reported as a number; the replay legitimately emits MORE stream messages
        // (warming-up transitions) and MORE quality transitions.
        assert_eq!(
            verify_replay_counts(true, 7, 8, &quality(9), 10, 5, &quality(5))
                .expect("ring re-warm gap is tolerated"),
            Some(3),
            "the ring re-warm shortfall in ready updates is reported as a number"
        );
        // ...but a stream-message count below the warm run means a decision went
        // missing, which is never tolerated,
        assert!(
            verify_replay_counts(true, 7, 4, &quality(9), 10, 5, &quality(5)).is_err(),
            "a missing decision (fewer stream messages than the warm run) is a fault"
        );
        // ...the replay may never write MORE ready updates than the warm run,
        assert!(
            verify_replay_counts(true, 11, 8, &quality(9), 10, 5, &quality(5)).is_err(),
            "a rotated replay cannot exceed the warm ready-update count"
        );
        // ...a rotation replay that produced nothing rebuilt no book,
        assert!(
            verify_replay_counts(true, 0, 8, &quality(9), 10, 5, &quality(5)).is_err(),
            "a rotated replay that produced no ready updates is a fault"
        );
        // ...and FEWER quality transitions than the warm run means a decision's
        // quality state went missing (a re-warm can only add transitions).
        assert!(
            verify_replay_counts(true, 7, 8, &quality(4), 10, 5, &quality(5)).is_err(),
            "a rotated replay cannot record fewer quality transitions than the warm run"
        );
    }

    #[test]
    fn rotation_deadline_is_offset_independent_on_a_resumed_clock_spot() {
        // A production restart resumes the process clock with a large logical
        // offset (elapsed since the frozen wall origin / the checkpoint time).
        let period = Duration::from_secs(3600);
        let period_ns = u64::try_from(period.as_nanos()).expect("period fits u64");
        let offset_ns = 6 * period_ns;
        let clock = ProcessClock::resume(wall_start(), offset_ns).expect("resumed clock");
        let first_open_ns = clock.elapsed_ns().expect("first open reading");
        assert!(
            first_open_ns >= offset_ns,
            "a resumed clock's elapsed_ns starts at its logical offset"
        );

        let schedule = RotationSchedule::try_new(first_open_ns, period).expect("schedule");
        let deadline_ns = schedule.next_deadline();
        // The original bug fed this absolute logical deadline straight into the
        // monotonic-relative `ProcessClock::deadline`, which would have scheduled
        // the first rotation ~offset + period into the future.
        assert!(
            deadline_ns >= offset_ns + period_ns,
            "the absolute deadline carries the whole logical offset"
        );
        // Exercise the SAME helper `run_rotation_timer` uses to compute its sleep,
        // not a re-implementation: it sleeps the RELATIVE remaining measured
        // against `elapsed_ns()`, one period regardless of the six-hour offset.
        // Reverting to feeding the absolute deadline into `ProcessClock::deadline`
        // changes this helper and fails the test.
        let remaining_ns = u64::try_from(
            rotation_sleep_remaining(&clock, deadline_ns)
                .expect("remaining computes")
                .as_nanos(),
        )
        .expect("remaining fits u64");
        assert!(
            remaining_ns <= period_ns,
            "the remaining time never exceeds one period"
        );
        assert!(
            remaining_ns + u64::try_from(Duration::from_secs(60).as_nanos()).unwrap() >= period_ns,
            "the first rotation fires ~one period after the first open, not the offset later"
        );
    }

    #[test]
    fn rotation_appends_each_output_epoch_to_the_lineage_spot() {
        let (lane_root, pipeline, controller, _checkpoint_path, epoch_prefix, _gen0_dir) =
            build_rotation_harness(60);

        let clock = ProcessClock::start();
        let mut admission_seq = 0_u64;
        for second in 1..=3 {
            admit_second_live(&pipeline, &mut admission_seq, second);
        }
        rotate_generation(&pipeline, &controller, &clock, rotation_ns(3)).expect("first rotation");

        let mut admission_seq = pipeline.borrow().next_admission_seq;
        for second in 4..=6 {
            admit_second_live(&pipeline, &mut admission_seq, second);
        }
        rotate_generation(&pipeline, &controller, &clock, rotation_ns(6)).expect("second rotation");

        // Each rotation appends its new output epoch to the reference lineage,
        // exactly as a `--resume-from-checkpoint` restart does, so after two
        // rotations the lineage carries the prefix plus both rotated epochs.
        let lineage = pipeline
            .borrow()
            .processor()
            .checkpoint_state()
            .reference
            .publisher_epoch_lineage
            .clone();
        // Generations are named from their SCHEDULED deadlines (60 s period from
        // the first open at ns 0), not the observed rotation instants (3 s, 6 s).
        let gen1_epoch = format!(
            "{epoch_prefix}-{}",
            generation_stamps(wall_start(), 60 * 1_000_000_000).0
        );
        let gen2_epoch = format!(
            "{epoch_prefix}-{}",
            generation_stamps(wall_start(), 120 * 1_000_000_000).0
        );
        assert_eq!(
            lineage,
            vec![epoch_prefix.to_string(), gen1_epoch, gen2_epoch],
            "each rotation pushes its output epoch onto the checkpoint lineage"
        );
        drop(pipeline);
        drop(controller);
        drop(lane_root);
    }

    #[test]
    fn rotation_advance_to_uses_the_post_seal_clock_spot() {
        let (_lane_root, pipeline, controller, _checkpoint_path, _epoch_prefix, _gen0_dir) =
            build_rotation_harness(60);
        let mut admission_seq = 0_u64;
        for second in 1..=2 {
            admit_second_live(&pipeline, &mut admission_seq, second);
        }

        // A clock whose logical time is already five whole periods past the first
        // open — as if the seal itself had taken that long. `advance_to` must skip
        // to the POST-seal instant (five periods), never to the pre-seal deadline
        // `now_ns` (well under one period). Reverting to `advance_to(now_ns)`
        // leaves `periods_elapsed` at zero and fails this test. The clock's wall
        // origin is sampled at `now` so its logical offset is exactly the five
        // periods, with no accrued wall time from the frozen test origin.
        let period_ns = 60_u64 * 1_000_000_000;
        let wall_now = DateTime::<Utc>::from(std::time::SystemTime::now());
        let clock = ProcessClock::resume(wall_now, 5 * period_ns).expect("resumed clock");
        rotate_generation(&pipeline, &controller, &clock, rotation_ns(2))
            .expect("rotation seals and opens");

        assert_eq!(
            controller.borrow().schedule.periods_elapsed(),
            5,
            "advance_to skipped to the post-seal clock (five periods), not the pre-seal now_ns"
        );
    }

    /// Parse the trailing unix second from a `gen-live-spot-<unix>` directory.
    fn generation_dir_unix(dir: &Path) -> u64 {
        dir.file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.rsplit('-').next())
            .and_then(|unix| unix.parse::<u64>().ok())
            .expect("generation dir name ends in a unix second")
    }

    #[test]
    fn rotation_names_generations_from_the_scheduled_deadline_not_the_overrun_spot() {
        // A 2 s rotation grid from the first open at ns 0. Each rotation is driven
        // with an observed `now_ns` that overran its scheduled deadline by 1.5 s
        // (> 1 s, crossing a wall second). The generation MUST still be named from
        // its scheduled deadline second (2 s, then 4 s), never the overrun second
        // (3 s, then 5 s) — otherwise a slow seal could name two opens in one
        // second and collide on `GenerationExists`.
        let period_secs = 2_u64;
        let (lane_root, pipeline, controller, _checkpoint_path, _epoch_prefix, _gen0_dir) =
            build_rotation_harness(period_secs);
        let mut admission_seq = 0_u64;
        for second in 1..=3 {
            admit_second_live(&pipeline, &mut admission_seq, second);
        }
        let clock = ProcessClock::start();

        // First rotation: scheduled deadline 2 s, observed open 3.5 s (overran 1.5 s).
        rotate_generation(&pipeline, &controller, &clock, 3_500_000_000)
            .expect("first rotation seals and opens");
        let gen1_unix = generation_dir_unix(&controller.borrow().current.dir);

        // Keep the admitted seconds contiguous and after the observed open.
        let mut admission_seq = pipeline.borrow().next_admission_seq;
        admit_second_live(&pipeline, &mut admission_seq, 4);
        // Second rotation: scheduled deadline 4 s, observed open 5.5 s (overran 1.5 s).
        rotate_generation(&pipeline, &controller, &clock, 5_500_000_000)
            .expect("second rotation seals and opens");
        let gen2_unix = generation_dir_unix(&controller.borrow().current.dir);

        let scheduled_gen1 = generation_stamps(wall_start(), 2 * 1_000_000_000).0;
        let scheduled_gen2 = generation_stamps(wall_start(), 4 * 1_000_000_000).0;
        let overrun_gen1 = generation_stamps(wall_start(), 3_500_000_000).0;
        assert_ne!(
            gen1_unix, overrun_gen1,
            "the generation must not be named from the 1.5 s seal overrun second"
        );
        assert_eq!(
            gen1_unix, scheduled_gen1,
            "the first rotation is named from its scheduled deadline (2 s)"
        );
        assert_eq!(
            gen2_unix, scheduled_gen2,
            "the second rotation is named from its scheduled deadline (4 s)"
        );
        assert_eq!(
            gen2_unix - gen1_unix,
            period_secs,
            "grid-named generations are exactly one period apart, so a seal overrun can never collide two opens in one second"
        );
        drop(pipeline);
        drop(controller);
        drop(lane_root);
    }

    #[test]
    fn rotation_generation_exists_is_retryable_spot() {
        let (lane_root, pipeline, controller, _checkpoint_path, _epoch_prefix, _gen0_dir) =
            build_rotation_harness(60);
        let mut admission_seq = 0_u64;
        for second in 1..=2 {
            admit_second_live(&pipeline, &mut admission_seq, second);
        }

        // Pre-create the exact directory the rotation will try to open, forcing a
        // `GenerationExists` refusal at the open step. The generation is named
        // from the SCHEDULED deadline (60 s period from the first open at ns 0),
        // not the observed rotation instant.
        let collide_unix = generation_stamps(wall_start(), 60 * 1_000_000_000).0;
        let collide_dir = lane_root
            .path()
            .join(format!("gen-live-{SPOT_LANE_NAME}-{collide_unix}"));
        std::fs::create_dir_all(&collide_dir).expect("pre-create colliding generation dir");

        let clock = ProcessClock::start();
        let error = rotate_generation(&pipeline, &controller, &clock, rotation_ns(2))
            .expect_err("rotation must fail when the next generation dir already exists");
        assert_eq!(
            exit_class_for(&error),
            ExitClass::Retryable,
            "a GenerationExists collision on the rotation path seals-and-exits Retryable, not Fatal"
        );

        // Contrast: the SAME fault outside the rotation mapping stays Fatal (a
        // cold start into an occupied lane is an operator error). This is the
        // classification the rotation path deliberately overrides for itself.
        let raw = anyhow::Error::new(FileGenerationError::GenerationExists {
            path: collide_dir.clone(),
        });
        assert_eq!(
            exit_class_for(&raw),
            ExitClass::Fatal,
            "an unmapped GenerationExists (startup refusal) stays Fatal"
        );
        drop(pipeline);
        drop(controller);
        drop(lane_root);
    }

    /// The observable outcome of a cold-feed run: the final decision's
    /// per-identity `(values, quality)` map, the Coinbase feed-health statuses at
    /// the final tick (`source_id` -> debug string), and the Coinbase
    /// provider-clock skew sample count.
    type SilentFeedOutcome = (
        std::collections::BTreeMap<String, (serde_json::Value, serde_json::Value)>,
        std::collections::BTreeMap<String, String>,
        u64,
    );

    /// A cold-feed run: warm both venues so their books are ready, silence
    /// Coinbase after `SILENT_AFTER`, keep Kraken live, and drive to the check
    /// second. Optionally rotate at `rotate_at` (admitting the book resync, which
    /// includes ready Coinbase rows).
    fn drive_coinbase_goes_silent(rotate_at: Option<u64>) -> SilentFeedOutcome {
        const SILENT_AFTER: u64 = 12;
        const CHECK_SECOND: u64 = 23;
        let (lane_root, pipeline, controller, _checkpoint_path, _epoch_prefix, _gen0_dir) =
            build_rotation_harness(60);

        let mut admission_seq = 0_u64;
        // Warm both venues; Coinbase's last real input lands at SILENT_AFTER.
        for second in 1..=SILENT_AFTER {
            admit_second_live(&pipeline, &mut admission_seq, second);
        }
        // Coinbase silent; Kraken keeps delivering. `T` (first silent second) is
        // SILENT_AFTER + 1; the rotation is at T + 9 and the check at T + 10.
        for second in (SILENT_AFTER + 1)..=CHECK_SECOND {
            if Some(second) == rotate_at {
                let clock = ProcessClock::start();
                rotate_generation(&pipeline, &controller, &clock, rotation_ns(second))
                    .expect("rotation seals and opens");
                admission_seq = pipeline.borrow().next_admission_seq;
            }
            {
                let mut pipe = pipeline.borrow_mut();
                admit_market_input(
                    &mut pipe,
                    &mut admission_seq,
                    rotation_kraken_second_events(second),
                    second,
                );
                let tick = JournalInput::HealthTick {
                    admission_seq,
                    now_ns: rotation_ns(second),
                };
                pipe.enqueue(tick.clone()).expect("tick enqueues");
                pipe.apply_input(tick).expect("tick applies");
                admission_seq += 1;
            }
        }

        // Captured at the final tick's `now_ns`, so both runs read Coinbase's
        // feed-health at the same instant: a resync that forged liveness would
        // leave Coinbase looking fresher here than in the unrotated run.
        let coinbase_skew_samples;
        let mut coinbase_health = std::collections::BTreeMap::new();
        {
            let pipe = pipeline.borrow();
            coinbase_skew_samples = pipe
                .provider_clock_skew
                .get(&Venue::Coinbase)
                .map_or(0, |samples| samples.samples);
            for source in [
                "coinbase.exchange.btc-usd.book-levels",
                "coinbase.exchange.btc-usd.ticker",
                "coinbase.exchange.btc-usd.trades",
            ] {
                coinbase_health.insert(
                    source.to_owned(),
                    format!("{:?}", pipe.feed_health.status(source)),
                );
            }
        }

        let current_dir = controller.borrow().current.dir.clone();
        {
            let mut pipe = pipeline.borrow_mut();
            pipe.sync_output().expect("final sync");
            pipe.complete_journal().expect("final journal");
            pipe.complete_checkpoint(rotation_ns(CHECK_SECOND))
                .expect("final checkpoint");
            let (_, sealed_utc) = generation_stamps(wall_start(), rotation_ns(CHECK_SECOND));
            controller
                .borrow_mut()
                .current
                .seal(sealed_utc)
                .expect("final seal");
        }
        drop(pipeline);
        drop(controller);

        let frames =
            parse_frames(&std::fs::read(current_dir.join("output.jsonl")).expect("output"));
        // The final decision's per-identity (values, quality); epoch/sequence,
        // which a rotation legitimately changes, are excluded.
        let mut states = std::collections::BTreeMap::new();
        for frame in &frames {
            if frame.get("type").and_then(|t| t.as_str()) != Some("signal_batch") {
                continue;
            }
            let Some(updates) = frame.get("updates").and_then(|u| u.as_array()) else {
                continue;
            };
            for update in updates {
                if let Some(id) = update.get("signal_id").and_then(|s| s.as_str()) {
                    states.insert(
                        id.to_owned(),
                        (
                            update
                                .get("values")
                                .cloned()
                                .unwrap_or(serde_json::Value::Null),
                            update
                                .get("quality")
                                .cloned()
                                .unwrap_or(serde_json::Value::Null),
                        ),
                    );
                }
            }
        }
        drop(lane_root);
        (states, coinbase_health, coinbase_skew_samples)
    }

    #[test]
    fn a_rotation_over_a_silent_feed_still_fails_closed_spot() {
        // Finding 1: the rotation book resync must update the book only. A resync
        // that forged venue liveness would revive a silent Coinbase feed, so the
        // rotated run would diverge from an unrotated one. Both runs must be
        // identical: the same fail-closed identities, the same feed-health frame,
        // and no extra provider-clock skew sample from the resync.
        let (unrotated_states, _unrotated_health, _unrotated_skew) =
            drive_coinbase_goes_silent(None);
        let (rotated_states, rotated_health, rotated_skew) = drive_coinbase_goes_silent(Some(22));

        assert_eq!(
            rotated_states, unrotated_states,
            "a rotation at T+9s must not change any identity's values or quality at T+10s; a silent Coinbase feed fails closed exactly as in an unrotated run"
        );
        // A fresh generation resets its feed-health board (begin_generation), so
        // Coinbase starts Down and only a genuine market event may promote it. The
        // resync admits Coinbase BOOK rows: if it forged feed-health observation
        // it would promote that source to Live. It must stay Down.
        assert_eq!(
            rotated_health
                .get("coinbase.exchange.btc-usd.book-levels")
                .map(String::as_str),
            Some("Some(Down)"),
            "the rotation resync must not promote the silent Coinbase book feed in feed-health; a forged observation would show it Live"
        );
        assert!(
            rotated_health.values().all(|status| status != "Some(Live)"),
            "no silent Coinbase feed may be Live after the rotation resync: {rotated_health:?}"
        );
        assert_eq!(
            rotated_skew, 0,
            "the rotation resync must record no provider-clock skew sample for the silent Coinbase feed (the only Coinbase input after the fresh generation opened is the resync)"
        );
    }

    #[test]
    fn rotation_mode_cli_validation_spot() {
        let base = [
            "btc-signals",
            "run-spot-signals",
            "--publisher-epoch",
            "live-spot-1",
        ];

        // Rotation and explicit modes are mutually exclusive.
        let mixed = Cli::try_parse_from(base.iter().copied().chain([
            "--lane-root",
            "/tmp/spot-lane",
            "--rotate-seconds",
            "60",
            "--output",
            "/tmp/out.jsonl",
            "--journal",
            "/tmp/journal.jsonl",
        ]));
        assert!(mixed.is_err(), "rotation and explicit flags must conflict");

        // A rotation period below the 60 s floor is rejected at parse time.
        let too_fast = Cli::try_parse_from(base.iter().copied().chain([
            "--lane-root",
            "/tmp/spot-lane",
            "--rotate-seconds",
            "30",
        ]));
        assert!(too_fast.is_err(), "--rotate-seconds below 60 is rejected");

        // --lane-root without --rotate-seconds is incomplete.
        let lone_root = Cli::try_parse_from(base.iter().copied().chain(["--lane-root", "/tmp/x"]));
        assert!(lone_root.is_err(), "--lane-root requires --rotate-seconds");

        // Rotation mode defaults the checkpoint under the lane root.
        let mode = SpotRunMode::resolve(
            None,
            None,
            None,
            Some(PathBuf::from("/tmp/spot-lane")),
            Some(3600),
        )
        .expect("rotation mode resolves");
        match mode {
            SpotRunMode::Rotation { checkpoint, .. } => {
                assert_eq!(checkpoint, PathBuf::from("/tmp/spot-lane/checkpoint.json"));
            }
            SpotRunMode::Explicit { .. } => panic!("expected rotation mode"),
        }

        // Explicit mode requires a checkpoint alongside output/journal.
        let missing_checkpoint = SpotRunMode::resolve(
            Some(PathBuf::from("/tmp/out.jsonl")),
            Some(PathBuf::from("/tmp/journal.jsonl")),
            None,
            None,
            None,
        );
        assert!(
            missing_checkpoint.is_err(),
            "explicit mode requires an explicit checkpoint"
        );
    }

    struct TestPath(PathBuf);

    static TEST_PATH_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    impl TestPath {
        fn new(label: &str) -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock follows Unix epoch")
                .as_nanos();
            // A process-wide counter guarantees uniqueness even when two threads
            // read the clock within the same nanosecond.
            let seq = TEST_PATH_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Self(std::env::temp_dir().join(format!(
                "btc-signal-main-{label}-{}-{unique}-{seq}.jsonl",
                std::process::id()
            )))
        }

        fn as_path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestPath {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
}
