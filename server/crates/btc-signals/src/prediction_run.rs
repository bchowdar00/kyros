//! The prediction lane runner, lifted verbatim from the `prediction-signals`
//! binary so the exchange process can drive it in-process;
//! `src/bin/prediction-signals.rs` is now a thin wrapper over [`cli_main`].

use std::cell::RefCell;
use std::fs::{File, OpenOptions};
use std::io::BufWriter;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
#[cfg(test)]
use btc_feeds::live::coordinate_live_shutdown_on;
use btc_feeds::live::{
    LiveControl, LiveDiagnostics, LiveOptions, LiveReport, LiveShutdown, LiveShutdownReason,
    ProcessClock, coordinate_live_shutdown, live_shutdown_channel, run_live,
};
use btc_feeds::prediction_live::{
    PredictionIndexSocketOptions, PredictionLiveAdmission, PredictionLiveError,
    PredictionLiveOptions, PredictionLiveReport, PredictionSocketTransition,
    run_prediction_index_socket, run_prediction_live,
};
use btc_feeds::{CaptureStream, FeedNoticeCode, RecoveryAction};
use crate::exit_code::{ClassifiedMessage, ExitClass, exit_class_for};
use crate::file_generation::{
    FileGenerationCaps, FileGenerationError, FileGenerationLayout, OpenFileGeneration,
    RotationSchedule, RunMeta,
};
use crate::lane_gate::{IndexSocket, admit_lane};
use crate::multi_runtime::{
    MergedJournalWriter, MultiJournalHeader, MultiJournalRecord, MultiLaneRuntime, MultiVenueTag,
    VenueHeader, replay_multi_file, runtime_from_header,
};
use crate::prediction_checkpoint::{
    DEFAULT_PREDICTION_CHECKPOINT_MAX_BYTES, PredictionCheckpointDocument,
    PredictionCheckpointResume, PredictionCheckpointWriteReport, resume_prediction_checkpoint,
    write_prediction_checkpoint,
};
use crate::prediction_journal::{
    DEFAULT_PREDICTION_JOURNAL_MAX_BYTES, PredictionJournalInput, PredictionJournalReplayReader,
    PredictionJournalSession, PredictionJournalWriteReport, PredictionJournalWriter,
    PredictionRuntimeConfig, PredictionTransport,
};
use crate::stream_accounting::StreamHook;
use crate::prediction_runtime::{
    PredictionCheckpointContext, PredictionPipeline, PredictionPipelineError,
    PredictionPipelineReport,
};
use crate::{
    COINBASE_TICKER_SOURCE_ID, KRAKEN_TICKER_SOURCE_ID, PredictionSignalProcessor,
    SPOT_SIGNAL_KRAKEN_BOOK_DEPTH, SettlementWindowPause,
};
use chrono::{DateTime, Duration as ChronoDuration, SecondsFormat, Utc};
use clap::{Parser, Subcommand, ValueEnum};
use serde::Serialize;
use signals_core::ArmedIndexSockets;
use signals_core::{
    KALSHI_LANE, LaneRequirement, POLYMARKET_LANE, PredictionMarketVenue, SourceEvent,
};
use tokio::time::{Instant, interval_at, sleep_until};

const DEFAULT_MAX_UPDATES: u64 = 10_000_000;
const DEFAULT_MAX_OUTPUT_BYTES: u64 = 2_147_483_648;
const DEFAULT_CHECKPOINT_INTERVAL_SECONDS: u64 = 5;
const MAX_RUN_SECONDS: u64 = 7 * 24 * 60 * 60;
/// Explicit-mode default when `--duration-seconds` is omitted (unchanged).
const DEFAULT_DURATION_SECONDS: u64 = 900;
/// Rotation-mode floor for `--rotate-seconds` (common brief: n >= 60).
const MIN_ROTATE_SECONDS: u64 = 60;

#[derive(Debug, Parser)]
#[command(
    name = "prediction-signals",
    about = "Auto-rolling, venue-scoped BTC prediction-market signals"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Discover, bind, ride, and automatically roll one venue's BTC markets.
    Run {
        #[arg(long, value_enum)]
        venue: CliVenue,
        /// Explicit-mode output path. Mutually exclusive with `--lane-root`.
        #[arg(long)]
        output: Option<PathBuf>,
        /// Explicit-mode journal path. Mutually exclusive with `--lane-root`.
        #[arg(long)]
        journal: Option<PathBuf>,
        /// Checkpoint path. Required in explicit mode; in rotation mode defaults
        /// to `<lane-root>/checkpoint.json`.
        #[arg(long)]
        checkpoint: Option<PathBuf>,
        /// Rotation mode: the lane directory the producer owns, under which it
        /// creates `gen-live-<lane>-<unix>` generations, `CURRENT` and the shared
        /// `checkpoint.json`. Requires `--rotate-seconds`; forbids
        /// `--output`/`--journal`.
        #[arg(long, requires = "rotate_seconds", conflicts_with_all = ["output", "journal"])]
        lane_root: Option<PathBuf>,
        /// Rotation mode: seconds between in-process file-generation rotations
        /// (>= 60). Requires `--lane-root`.
        #[arg(long, requires = "lane_root")]
        rotate_seconds: Option<u64>,
        /// Headroom factor recorded in each generation's `RUN-META.json`
        /// (rotation mode only), matching live-run.sh's cap-derivation factor.
        #[arg(long, default_value_t = 2)]
        headroom_factor: u64,
        #[arg(long)]
        resume_from_checkpoint: bool,
        #[arg(long)]
        publisher_epoch: String,
        /// Total run bound in seconds. In explicit mode absent means 900. In
        /// rotation mode absent means run until SIGTERM (no total bound).
        #[arg(long)]
        duration_seconds: Option<u64>,
        #[arg(long, default_value_t = 2_000)]
        discovery_retry_ms: u64,
        #[arg(long, default_value_t = DEFAULT_CHECKPOINT_INTERVAL_SECONDS)]
        checkpoint_interval_seconds: u64,
        #[arg(long)]
        induced_reconnect_seconds: Option<u64>,
        #[arg(long, default_value_t = DEFAULT_MAX_UPDATES)]
        max_updates: u64,
        #[arg(long, default_value_t = DEFAULT_MAX_OUTPUT_BYTES)]
        max_output_bytes: u64,
        #[arg(long, default_value_t = DEFAULT_PREDICTION_JOURNAL_MAX_BYTES)]
        max_journal_bytes: u64,
        #[arg(long, default_value_t = DEFAULT_PREDICTION_CHECKPOINT_MAX_BYTES)]
        max_checkpoint_bytes: u64,
    },
    /// Rebuild a complete stream byte-identically from its retained journal.
    Replay {
        #[arg(long)]
        journal: PathBuf,
        #[arg(long)]
        output: PathBuf,
        /// `multi` replays a merged dual-venue journal (E57); omitted (or a single
        /// venue) replays a single-venue prediction journal, whose venue is read
        /// from the journal's own session header.
        #[arg(long, value_enum)]
        venue: Option<CliVenue>,
        #[arg(long, default_value_t = DEFAULT_MAX_UPDATES)]
        max_updates: u64,
        #[arg(long, default_value_t = DEFAULT_MAX_OUTPUT_BYTES)]
        max_output_bytes: u64,
        #[arg(long, default_value_t = DEFAULT_PREDICTION_JOURNAL_MAX_BYTES)]
        max_journal_bytes: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CliVenue {
    Polymarket,
    Kalshi,
    /// The dual-venue cross-venue blend lane (C1 phase 3, ruling E57): composes
    /// the Kalshi (index-disarmed) and Polymarket pipelines in one process and
    /// publishes `btc.multi.prediction_dislocation` and `btc.multi.pair_cost_spread`.
    Multi,
}

impl CliVenue {
    /// The allow-list requirement for a single-venue live lane (ADR 0026). The
    /// `Multi` lane is gated on BOTH venues' requirements separately and never
    /// reaches this, so its arm is unreachable.
    fn lane_requirement(self) -> &'static LaneRequirement {
        match self {
            Self::Polymarket => &POLYMARKET_LANE,
            Self::Kalshi => &KALSHI_LANE,
            Self::Multi => unreachable!("the multi lane is gated on both venues, not one"),
        }
    }
}

impl From<CliVenue> for PredictionMarketVenue {
    fn from(value: CliVenue) -> Self {
        match value {
            CliVenue::Polymarket => Self::Polymarket,
            CliVenue::Kalshi => Self::Kalshi,
            CliVenue::Multi => {
                unreachable!("the multi lane is dispatched before venue conversion")
            }
        }
    }
}

#[derive(Debug, Serialize)]
pub struct RunReport {
    pub venue: PredictionMarketVenue,
    pub duration_seconds: u64,
    pub elapsed_ns: u64,
    pub prediction_live: PredictionLiveReport,
    /// The dedicated Kalshi index socket's whole-run totals (ruling E37d). Zeroed
    /// on a disarmed lane, which opens no index socket.
    pub index_live: PredictionLiveReport,
    pub coinbase_spot_live: LiveReport,
    pub kraken_spot_live: LiveReport,
    pub twap_live: Option<LiveReport>,
    pub maintenance: MaintenanceReport,
    pub pipeline: PredictionPipelineReport,
    pub journal: PredictionJournalWriteReport,
    pub checkpoint: PredictionCheckpointWriteReport,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MaintenanceReport {
    pub health_ticks: u64,
    pub interval_checkpoints: u64,
    /// This worker's locally selected stop branch. Process exit classification
    /// comes only from the deadline coordinator's immutable shutdown reason.
    pub interrupted: bool,
}

#[derive(Debug, Serialize)]
pub struct ReplayReport {
    pub venue: PredictionMarketVenue,
    pub pipeline: PredictionPipelineReport,
}

/// The CLI entry point the thin binary calls; parses argv and runs the command.
pub async fn cli_main() -> ExitCode {
    let outcome: Result<ExitClass> = run_command().await;
    match outcome {
        Ok(class) => class,
        Err(error) => {
            eprintln!("Error: {error:?}");
            exit_class_for(&error)
        }
    }
    .exit_code()
}

/// Dispatch the CLI command, returning the [`ExitClass`] for a clean run.
async fn run_command() -> Result<ExitClass> {
    match Cli::parse().command {
        Command::Run {
            venue,
            output,
            journal,
            checkpoint,
            lane_root,
            rotate_seconds,
            headroom_factor,
            resume_from_checkpoint,
            publisher_epoch,
            duration_seconds,
            discovery_retry_ms,
            checkpoint_interval_seconds,
            induced_reconnect_seconds,
            max_updates,
            max_output_bytes,
            max_journal_bytes,
            max_checkpoint_bytes,
        } => {
            // The multi lane (E57) is gated on BOTH venues' allow-list entries and
            // runs Kalshi index-disarmed; it is dispatched before the single-venue
            // admission below. It runs on the same C5-capable shell (rotation mode
            // by default; explicit mode is the rollback path), resolving its output
            // mode exactly as the single-venue lane does.
            if venue == CliVenue::Multi {
                for requirement in [&KALSHI_LANE, &POLYMARKET_LANE] {
                    let admission = match admit_lane(requirement, IndexSocket::NotConfigured) {
                        Ok(admission) => admission,
                        Err(class) => return Ok(class),
                    };
                    eprintln!("{}", admission.startup_line());
                }
                let (resolved_output, resolved_journal, resolved_checkpoint, duration, rotation) =
                    match (lane_root, rotate_seconds) {
                        (Some(lane_root), Some(rotate_seconds)) => {
                            if rotate_seconds < MIN_ROTATE_SECONDS {
                                anyhow::bail!(
                                    "--rotate-seconds must be at least {MIN_ROTATE_SECONDS}"
                                );
                            }
                            let checkpoint =
                                checkpoint.unwrap_or_else(|| lane_root.join("checkpoint.json"));
                            let duration = duration_seconds
                                .map_or(Duration::from_secs(MAX_RUN_SECONDS), Duration::from_secs);
                            let rotation = MultiRotationConfig {
                                lane_root,
                                rotate_seconds,
                                headroom_factor,
                                resumed: resume_from_checkpoint,
                            };
                            (None, None, checkpoint, duration, Some(rotation))
                        }
                        _ => {
                            let output = output
                                .context("explicit mode requires --output (or use --lane-root)")?;
                            let journal = journal
                                .context("explicit mode requires --journal (or use --lane-root)")?;
                            let checkpoint =
                                checkpoint.context("explicit mode requires --checkpoint")?;
                            let duration = Duration::from_secs(
                                duration_seconds.unwrap_or(DEFAULT_DURATION_SECONDS),
                            );
                            (Some(output), Some(journal), checkpoint, duration, None)
                        }
                    };
                let (report, shutdown_reason) = run_multi(MultiRunArguments {
                    output: resolved_output,
                    journal: resolved_journal,
                    checkpoint: resolved_checkpoint,
                    publisher_epoch,
                    duration,
                    discovery_retry_ms,
                    checkpoint_interval_seconds,
                    induced_reconnect_seconds,
                    max_updates,
                    max_output_bytes,
                    max_journal_bytes,
                    max_checkpoint_bytes,
                    rotation,
                })
                .await?;
                println!("{}", serde_json::to_string(&report)?);
                return Ok(if shutdown_reason.interrupted() {
                    ExitClass::Interrupted
                } else {
                    ExitClass::Success
                });
            }
            // Resolve the output mode. clap already enforces that `--lane-root`
            // and `--output`/`--journal` are mutually exclusive and that each
            // rotation flag implies the other (usage class); here we apply the
            // period floor, the checkpoint default/requirement, and the
            // duration-absent semantics.
            let (resolved_output, resolved_journal, resolved_checkpoint, duration, rotation) =
                match (lane_root, rotate_seconds) {
                    (Some(lane_root), Some(rotate_seconds)) => {
                        if rotate_seconds < MIN_ROTATE_SECONDS {
                            anyhow::bail!("--rotate-seconds must be at least {MIN_ROTATE_SECONDS}");
                        }
                        let checkpoint =
                            checkpoint.unwrap_or_else(|| lane_root.join("checkpoint.json"));
                        let duration = duration_seconds
                            .map_or(Duration::from_secs(MAX_RUN_SECONDS), Duration::from_secs);
                        let lane = match venue {
                            CliVenue::Kalshi => signals_core::KALSHI_LANE.lane,
                            CliVenue::Polymarket => signals_core::POLYMARKET_LANE.lane,
                            CliVenue::Multi => unreachable!("multi is dispatched above"),
                        }
                        .to_owned();
                        let rotation = RotationConfig {
                            lane_root,
                            lane,
                            rotate_seconds,
                            headroom_factor,
                            resumed: resume_from_checkpoint,
                        };
                        (None, None, checkpoint, duration, Some(rotation))
                    }
                    _ => {
                        let output = output
                            .context("explicit mode requires --output (or use --lane-root)")?;
                        let journal = journal
                            .context("explicit mode requires --journal (or use --lane-root)")?;
                        let checkpoint =
                            checkpoint.context("explicit mode requires --checkpoint")?;
                        let duration = Duration::from_secs(
                            duration_seconds.unwrap_or(DEFAULT_DURATION_SECONDS),
                        );
                        (Some(output), Some(journal), checkpoint, duration, None)
                    }
                };
            let duration_seconds_report = duration.as_secs();
            // The live prediction lane is gated on its own venue token. For
            // Kalshi a missing index token only disables the index socket, so
            // the market channels keep running (ruling E2). Replay is not
            // gated: it reads a captured journal and subscribes to nothing.
            // The armed Kalshi lane opens a dedicated index socket — a second
            // connection alongside the market socket (ruling E37d) — so it
            // declares the socket configured. That never gates admission: with no
            // index token listed the lane starts, opens no index socket, logs one
            // line and simply does not subscribe them (ruling E2/N1).
            // The Polymarket lane has no index channels at all.
            let socket = match venue {
                CliVenue::Kalshi => IndexSocket::Configured,
                CliVenue::Polymarket => IndexSocket::NotConfigured,
                CliVenue::Multi => unreachable!("multi is dispatched above"),
            };
            let admission = match admit_lane(venue.lane_requirement(), socket) {
                Ok(admission) => admission,
                Err(class) => return Ok(class),
            };
            eprintln!("{}", admission.startup_line());
            let (report, shutdown_reason) = run(RunArguments {
                venue: venue.into(),
                output: resolved_output,
                journal: resolved_journal,
                checkpoint: resolved_checkpoint,
                resume_from_checkpoint,
                publisher_epoch,
                duration,
                duration_seconds_report,
                discovery_retry_ms,
                checkpoint_interval_seconds,
                induced_reconnect_seconds,
                max_updates,
                max_output_bytes,
                max_journal_bytes,
                max_checkpoint_bytes,
                index_sockets: admission.index_sockets(),
                rotation,
            })
            .await?;
            println!("{}", serde_json::to_string(&report)?);
            Ok(if shutdown_reason.interrupted() {
                ExitClass::Interrupted
            } else {
                ExitClass::Success
            })
        }
        Command::Replay {
            journal,
            output,
            venue,
            max_updates,
            max_output_bytes,
            max_journal_bytes,
        } => {
            if venue == Some(CliVenue::Multi) {
                let report =
                    replay_multi_file(&journal, &output).context("replay merged multi journal")?;
                println!("{}", serde_json::to_string(&report)?);
                return Ok(ExitClass::Success);
            }
            let report = replay(
                &journal,
                &output,
                max_updates,
                max_output_bytes,
                max_journal_bytes,
            )?;
            println!("{}", serde_json::to_string(&report)?);
            // Replay is a deterministic, non-interruptible file operation.
            Ok(ExitClass::Success)
        }
    }
}

pub struct RunArguments {
    pub venue: PredictionMarketVenue,
    /// Explicit-mode output path; `None` in rotation mode, where the first
    /// generation's output is opened under the lane root once the clock starts.
    pub output: Option<PathBuf>,
    /// Explicit-mode journal path; `None` in rotation mode.
    pub journal: Option<PathBuf>,
    pub checkpoint: PathBuf,
    pub resume_from_checkpoint: bool,
    pub publisher_epoch: String,
    /// Total run bound. In rotation mode with no `--duration-seconds` this is the
    /// `MAX_RUN_SECONDS` ceiling, so the run continues until SIGTERM in practice.
    pub duration: Duration,
    /// The value reported in `RunReport.duration_seconds`.
    pub duration_seconds_report: u64,
    pub discovery_retry_ms: u64,
    pub checkpoint_interval_seconds: u64,
    pub induced_reconnect_seconds: Option<u64>,
    pub max_updates: u64,
    pub max_output_bytes: u64,
    pub max_journal_bytes: u64,
    pub max_checkpoint_bytes: u64,
    /// Kalshi index-channel families the venue gate armed. They ride the Kalshi
    /// market socket (observed, ruling E23), so the lane subscribes them on the
    /// connection it already holds.
    pub index_sockets: ArmedIndexSockets,
    /// Rotation-mode configuration, or `None` for explicit mode.
    pub rotation: Option<RotationConfig>,
}

/// In-process file-generation rotation configuration (C5).
pub struct RotationConfig {
    pub lane_root: PathBuf,
    pub lane: String,
    pub rotate_seconds: u64,
    pub headroom_factor: u64,
    pub resumed: bool,
}

/// Live rotation state threaded into [`maintain`]: the layout, the drift-free
/// schedule, the per-generation caps, and the currently open generation whose
/// `RUN-META.json` is sealed at the next boundary.
struct RotationRuntime {
    layout: FileGenerationLayout,
    schedule: RotationSchedule,
    rotate_seconds: u64,
    headroom_factor: u64,
    caps: FileGenerationCaps,
    /// The process admission epoch (`--publisher-epoch`) each generation's output
    /// epoch is derived from (`<prefix>-<unix>`; generation 0 = the prefix).
    epoch_prefix: String,
    current: OpenFileGeneration,
}

pub async fn run(arguments: RunArguments) -> Result<(RunReport, LiveShutdownReason)> {
    run_with_hook(arguments, None).await
}

/// [`run`] with an in-process observer of every stream message the run
/// writes ([`StreamHook`]); the hook persists across generation rotation. An
/// embedding process (the exchange) uses this to consume the lane's output
/// without tailing its files.
pub async fn run_with_hook(
    arguments: RunArguments,
    stream_hook: Option<StreamHook>,
) -> Result<(RunReport, LiveShutdownReason)> {
    validate_run_arguments(&arguments)?;
    let duration = arguments.duration;
    // CF-index ingestion, the global oracle reference and the two Kalshi
    // settlement identities switch with the armed CF socket (E43); the token is
    // the lane gate's own arming decision, so nothing here re-derives it.
    let index_armed = arguments.index_sockets.is_armed("cfbrti");
    let runtime_config =
        PredictionRuntimeConfig::new(arguments.venue, arguments.discovery_retry_ms)
            .with_index_armed(index_armed);

    // A readable checkpoint whose contract fingerprint differs from this binary
    // is superseded (the release path for a contract change, ruling N2): it is
    // archived beside its generation and the lane starts a fresh epoch instead
    // of bailing into a restart loop.
    let resume = if arguments.resume_from_checkpoint {
        match resume_prediction_checkpoint(&arguments.checkpoint, arguments.max_checkpoint_bytes)
            .context("resume prediction checkpoint")?
        {
            PredictionCheckpointResume::Resume(document) => Some(*document),
            PredictionCheckpointResume::Superseded {
                archived,
                superseded_fingerprint,
                expected_fingerprint,
            } => {
                eprintln!(
                    "prediction checkpoint contract {superseded_fingerprint} superseded by \
                     {expected_fingerprint}; archived {} and starting a fresh epoch",
                    archived.display()
                );
                None
            }
        }
    } else {
        None
    };

    let (clock, processor, session, lineage, replace_checkpoint) = if let Some(checkpoint) = resume
    {
        if !checkpoint
            .runtime_config
            .matches_ignoring_feed_health(&runtime_config)
        {
            anyhow::bail!("checkpoint runtime configuration disagrees with requested run");
        }
        let wall_start = DateTime::parse_from_rfc3339(&checkpoint.wall_start)
            .context("parse checkpoint wall_start")?
            .with_timezone(&Utc);
        let clock = ProcessClock::resume(wall_start, checkpoint.checkpoint_time_ns)
            .context("resume logical clock")?;
        let processor = PredictionSignalProcessor::restore_indexed(
            arguments.publisher_epoch.clone(),
            wall_start,
            checkpoint.processor_state.clone(),
            index_armed,
        )
        .context("restore prediction processor")?;
        let mut lineage = checkpoint.publisher_epoch_lineage.clone();
        lineage.push(arguments.publisher_epoch.clone());
        let session = PredictionJournalSession::resumed(
            arguments.publisher_epoch.clone(),
            checkpoint,
            runtime_config.clone(),
        )
        .context("build resumed journal session")?;
        (clock, processor, session, lineage, true)
    } else {
        let clock = ProcessClock::start();
        let processor = PredictionSignalProcessor::new_indexed(
            arguments.venue,
            arguments.publisher_epoch.clone(),
            clock.wall_start(),
            index_armed,
        )
        .context("build prediction processor")?;
        let session = PredictionJournalSession::new(
            clock.wall_start_text(),
            arguments.publisher_epoch.clone(),
            runtime_config.clone(),
        )
        .context("build prediction journal session")?;
        (
            clock,
            processor,
            session,
            vec![arguments.publisher_epoch.clone()],
            false,
        )
    };

    // Rotation mode: open the first file generation under the lane root now that
    // the clock exists, and derive its output/journal paths. The schedule aligns
    // to this first open on the process clock (drift-free). Explicit mode uses the
    // caller-supplied paths unchanged.
    let (output_path, journal_path, mut rotation_runtime) = match &arguments.rotation {
        Some(config) => {
            let caps = FileGenerationCaps {
                max_updates: arguments.max_updates,
                max_output_bytes: arguments.max_output_bytes,
                max_journal_bytes: arguments.max_journal_bytes,
            };
            let layout = FileGenerationLayout::new(&config.lane_root, config.lane.clone())
                .context("bind lane root")?;
            let first_open_ns = clock.elapsed_ns().context("sample first-open time")?;
            let (now_unix, started_utc) = wall_clock_stamp(&clock, first_open_ns)?;
            let meta = rotation_run_meta(
                &config.lane,
                &arguments.publisher_epoch,
                &format!("gen-live-{}-{now_unix}", config.lane),
                &started_utc,
                config.rotate_seconds,
                caps,
                config.headroom_factor,
                config.resumed,
            );
            let open = layout
                .open_next_with_rotation(now_unix, None, &meta, config.rotate_seconds)
                .context("open first file generation")?;
            let schedule = RotationSchedule::try_new(
                first_open_ns,
                Duration::from_secs(config.rotate_seconds),
            )
            .context("start rotation schedule")?;
            let output_path = open.output_path.clone();
            let journal_path = open.journal_path.clone();
            (
                output_path,
                journal_path,
                Some(RotationRuntime {
                    layout,
                    schedule,
                    rotate_seconds: config.rotate_seconds,
                    headroom_factor: config.headroom_factor,
                    caps,
                    epoch_prefix: arguments.publisher_epoch.clone(),
                    current: open,
                }),
            )
        }
        None => {
            let output = arguments
                .output
                .clone()
                .context("explicit mode requires --output")?;
            let journal = arguments
                .journal
                .clone()
                .context("explicit mode requires --journal")?;
            (output, journal, None)
        }
    };

    let writer = private_output(&output_path).context("create prediction output")?;
    let journal_writer = PredictionJournalWriter::create(
        &journal_path,
        session.clone(),
        arguments.max_journal_bytes,
    )
    .context("create prediction journal")?;
    let checkpoint_context = PredictionCheckpointContext {
        path: arguments.checkpoint.clone(),
        maximum_bytes: arguments.max_checkpoint_bytes,
        session_id: session.session_id.clone(),
        wall_start: session.wall_start.clone(),
        contract_fingerprint: session.contract_fingerprint.clone(),
        runtime_config: session.runtime_config.clone(),
        publisher_epoch_lineage: lineage,
    };
    let initial_checkpoint = PredictionCheckpointDocument::new(
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
    .context("build initial checkpoint")?;
    write_prediction_checkpoint(
        &arguments.checkpoint,
        initial_checkpoint,
        arguments.max_checkpoint_bytes,
        replace_checkpoint,
    )
    .context("persist initial checkpoint")?;

    let pipeline = Rc::new(RefCell::new(
        PredictionPipeline::live(
            BufWriter::with_capacity(1024 * 1024, writer),
            processor,
            &session,
            journal_writer,
            checkpoint_context,
            arguments.max_updates,
            arguments.max_output_bytes,
        )
        .context("build live prediction pipeline")?
        .with_stream_hook(stream_hook),
    ));

    let mut live_options = PredictionLiveOptions::new(
        arguments.venue,
        duration,
        arguments.publisher_epoch.clone(),
        pipeline
            .borrow()
            .current_generation_for_auxiliary_source()
            .context("reserve initial market generation")?,
        session
            .initial_checkpoint
            .as_ref()
            .and_then(|checkpoint| checkpoint.processor_state.active_market_id.clone()),
    );
    live_options.index_sockets = arguments.index_sockets;
    live_options.discovery_retry = Duration::from_millis(arguments.discovery_retry_ms);
    live_options.induced_reconnect_interval =
        arguments.induced_reconnect_seconds.map(Duration::from_secs);
    let (shutdown_trigger, shutdown) = live_shutdown_channel();
    live_options.shutdown = Some(shutdown.clone());
    let venue = arguments.venue;

    let prediction_pipeline = Rc::clone(&pipeline);
    let prediction_clock = clock.clone();
    let prediction_future = async move {
        run_prediction_live(live_options, prediction_clock, move |admission| {
            admit_prediction_live(&prediction_pipeline, venue, admission)
        })
        .await
        .map_err(anyhow::Error::from)
    };

    // Ruling E37d: the armed Kalshi index channels ride their own persistent
    // connection, independent of market discovery and roll, so a completing
    // settlement frame that lands after the market socket tears down to roll is
    // still received and derived. A disarmed lane (or a non-Kalshi venue) opens
    // no socket and admits nothing, so it stays byte-identical.
    let mut index_options = PredictionIndexSocketOptions::new(
        arguments.venue,
        duration,
        arguments.publisher_epoch.clone(),
        arguments.index_sockets,
    );
    index_options.shutdown = Some(shutdown.clone());
    let index_pipeline = Rc::clone(&pipeline);
    let index_clock = clock.clone();
    let index_future = async move {
        run_prediction_index_socket(index_options, index_clock, move |admission| {
            admit_prediction_index(&index_pipeline, admission)
        })
        .await
        .map_err(anyhow::Error::from)
    };

    let twap_pipeline = Rc::clone(&pipeline);
    let twap_clock = clock.clone();
    let twap_epoch = arguments.publisher_epoch.clone();
    let twap_shutdown = shutdown.clone();
    let twap_future = async move {
        if venue != PredictionMarketVenue::Polymarket {
            return Ok(None);
        }
        let options = LiveOptions {
            stream: CaptureStream::PolymarketChainlinkTwap,
            duration,
            kraken_btc_book_depth: 10,
            publisher_epoch: twap_epoch,
            diagnostics: LiveDiagnostics::default(),
            shutdown: Some(twap_shutdown),
            kraken_trade_reconciliation: None,
        };
        let report =
            run_live(options, twap_clock.clone(), move |timed| {
                let transitions = timed
                    .outcome
                    .notices
                    .iter()
                    .filter_map(|notice| auxiliary_socket_transition(notice.code))
                    .collect::<Vec<_>>();
                let events = timed.outcome.events;
                if !transitions.is_empty() || !events.is_empty() {
                    let mut pipeline = twap_pipeline.borrow_mut();
                    let now_ns = twap_clock.admission_ns(timed.logical_receipt_ns)?;
                    for transition in transitions {
                        let admission_seq = pipeline.next_admission_seq();
                        pipeline
                            .admit_live(PredictionJournalInput::Connectivity {
                                admission_seq,
                                transport: PredictionTransport::PolymarketRtds,
                                transition,
                                now_ns,
                            })
                            .map_err(|error| {
                                btc_feeds::live::LiveError::Publication(error.to_string())
                            })?;
                    }
                    if !events.is_empty() {
                        let generation =
                            pipeline.current_generation_for_auxiliary_source().map_err(
                                |error| btc_feeds::live::LiveError::Publication(error.to_string()),
                            )?;
                        let admission_seq = pipeline.next_admission_seq();
                        pipeline
                            .admit_live(PredictionJournalInput::MarketEvents {
                                admission_seq,
                                generation,
                                events: Arc::new(events),
                                logical_receipt_ns: timed.logical_receipt_ns,
                                now_ns,
                            })
                            .map_err(|error| {
                                btc_feeds::live::LiveError::Publication(error.to_string())
                            })?;
                    }
                }
                Ok(LiveControl::Continue)
            })
            .await?;
        Ok::<_, anyhow::Error>(Some(report))
    };

    // The prediction process computes S and sigma itself (ADR 0016): each venue
    // runner additionally subscribes the Coinbase and Kraken core sockets and routes
    // their normalized ticker events into the journal's SpotEvents admission path,
    // feeding the internally hosted spot reference. This reuses the exact btc-feeds
    // stream wiring the published spot runner uses for those tickers.
    let coinbase_future = run_spot_ticker(
        CaptureStream::CoinbaseCore,
        duration,
        arguments.publisher_epoch.clone(),
        shutdown.clone(),
        Rc::clone(&pipeline),
        clock.clone(),
    );
    let kraken_future = run_spot_ticker(
        CaptureStream::KrakenCore,
        duration,
        arguments.publisher_epoch.clone(),
        shutdown.clone(),
        Rc::clone(&pipeline),
        clock.clone(),
    );

    let maintenance_pipeline = Rc::clone(&pipeline);
    let maintenance_clock = clock.clone();
    let maintenance_shutdown = shutdown;
    let maintenance_future = maintain(
        maintenance_pipeline,
        maintenance_clock,
        duration,
        Duration::from_secs(arguments.checkpoint_interval_seconds),
        maintenance_shutdown,
        rotation_runtime.take(),
    );
    let shutdown_deadline = clock.deadline(duration);
    let shutdown_future = async move {
        coordinate_live_shutdown(shutdown_deadline, shutdown_trigger)
            .await
            .context("listen for shutdown signal")
    };

    // The venue market runner, its two spot-ticker runners and the dedicated
    // Kalshi index socket (ruling E37d) share the pipeline and shut down
    // together; join them as one worker so `join_live_workers` keeps its
    // four-arm shape.
    let market_and_spot = async move {
        tokio::try_join!(
            prediction_future,
            coinbase_future,
            kraken_future,
            index_future
        )
    };
    let (
        (prediction_live, coinbase_spot_live, kraken_spot_live, index_live),
        twap_live,
        (maintenance, mut final_rotation),
        shutdown_reason,
    ) = join_live_workers(
        market_and_spot,
        twap_future,
        maintenance_future,
        shutdown_future,
    )
    .await?;
    let elapsed_ns = clock.elapsed_ns().context("sample final logical time")?;
    let mut pipeline = Rc::try_unwrap(pipeline)
        .map_err(|_| anyhow::anyhow!("prediction pipeline still has active owners"))?
        .into_inner();
    pipeline.sync_output().context("sync prediction output")?;
    let (pipeline, journal, checkpoint) = pipeline
        .finish_live(elapsed_ns)
        .context("finish live prediction evidence")?;

    // Rotation mode: seal the final (still-open) generation's RUN-META so its
    // `sealed_utc` is stamped on a clean graceful end (duration expiry or SIGTERM),
    // exactly as the rotation path seals each departed generation. Explicit mode
    // has no rotation runtime, so this is inert.
    seal_final_open_generation(final_rotation.as_mut(), &clock, elapsed_ns)?;

    Ok((
        RunReport {
            venue: arguments.venue,
            duration_seconds: arguments.duration_seconds_report,
            elapsed_ns,
            prediction_live,
            index_live,
            coinbase_spot_live,
            kraken_spot_live,
            twap_live,
            maintenance,
            pipeline,
            journal,
            checkpoint,
        },
        shutdown_reason,
    ))
}

/// Reuse the published spot runner's exact ticker stream wiring: subscribe one
/// Coinbase or Kraken core socket and route only its normalized ticker events into
/// the prediction journal's `SpotEvents` admission path, feeding the internally
/// hosted spot reference (ADR 0016). Trades and book-levels on the same socket are
/// not consumed by the embedded reference, so they are filtered out here; admitting
/// them would fail the run as unsupported spot sources. Socket lifecycle
/// (reconnect, read-idle, teardown) and controlled shutdown are the same as every
/// other `run_live` worker.
async fn run_spot_ticker(
    stream: CaptureStream,
    duration: Duration,
    publisher_epoch: String,
    shutdown: LiveShutdown,
    pipeline: Rc<RefCell<PredictionPipeline<BufWriter<File>>>>,
    clock: ProcessClock,
) -> Result<LiveReport> {
    let transport = match stream {
        CaptureStream::CoinbaseCore => PredictionTransport::Coinbase,
        CaptureStream::KrakenCore => PredictionTransport::Kraken,
        _ => anyhow::bail!("embedded spot ticker requires CoinbaseCore or KrakenCore"),
    };
    let options = LiveOptions {
        stream,
        duration,
        kraken_btc_book_depth: SPOT_SIGNAL_KRAKEN_BOOK_DEPTH,
        publisher_epoch,
        diagnostics: LiveDiagnostics::default(),
        shutdown: Some(shutdown),
        kraken_trade_reconciliation: None,
    };
    let report = run_live(options, clock.clone(), move |timed| {
        let transitions = timed
            .outcome
            .notices
            .iter()
            .filter_map(|notice| auxiliary_socket_transition(notice.code))
            .collect::<Vec<_>>();
        let events: Vec<SourceEvent> = timed
            .outcome
            .events
            .into_iter()
            .filter(|event| is_spot_reference_ticker(&event.source_id))
            .collect();
        if !transitions.is_empty() || !events.is_empty() {
            let mut pipeline = pipeline.borrow_mut();
            let now_ns = clock.admission_ns(timed.logical_receipt_ns)?;
            for transition in transitions {
                let admission_seq = pipeline.next_admission_seq();
                pipeline
                    .admit_live(PredictionJournalInput::Connectivity {
                        admission_seq,
                        transport,
                        transition,
                        now_ns,
                    })
                    .map_err(|error| btc_feeds::live::LiveError::Publication(error.to_string()))?;
            }
            if !events.is_empty() {
                let admission_seq = pipeline.next_admission_seq();
                pipeline
                    .admit_live(PredictionJournalInput::SpotEvents {
                        admission_seq,
                        events: Arc::new(events),
                        logical_receipt_ns: timed.logical_receipt_ns,
                        now_ns,
                    })
                    .map_err(|error| btc_feeds::live::LiveError::Publication(error.to_string()))?;
            }
        }
        Ok(LiveControl::Continue)
    })
    .await?;
    Ok(report)
}

/// The two ticker source contracts the embedded spot reference consumes. A core
/// socket also carries trade and book-levels events the reference does not accept,
/// so only these two ride the `SpotEvents` admission path.
fn is_spot_reference_ticker(source_id: &str) -> bool {
    source_id == COINBASE_TICKER_SOURCE_ID || source_id == KRAKEN_TICKER_SOURCE_ID
}

/// Turn an index-channel subscription notice into per-source availability.
///
/// Every refused subscription is recorded by its source id, but only the source
/// a published identity consumes reaches the wire (F2). A refusal on the armed
/// 1 Hz value channel (`kalshi.cfbenchmarks.brti.value`) makes the
/// index-dependent instances — the two settlement twins and the oracle
/// reference — publish `unavailable` with `cfbrti_subscription_rejected`; the
/// `SubscriptionAccepted` that follows a reconnect clears it. A 5 Hz refusal
/// (`kalshi.cfbenchmarks.brti.value-5hz`, ruling E25) touches only 5 Hz-dependent
/// instances, of which there are none today, so it never reaches a published
/// identity. Either way the Kalshi market channels are unaffected and the lane
/// never fails (ruling E2/E25). The affected identity arrives as typed data on
/// the notice — `FeedNotice::subscription` — because `detail` is explicitly not
/// part of the contract and a lane must never route on its wording.
fn apply_index_channel_status<W: std::io::Write>(
    pipeline: &mut PredictionPipeline<W>,
    outcome: &btc_feeds::IngestOutcome,
) -> Result<(), PredictionPipelineError> {
    for notice in &outcome.notices {
        let Some(subscription) = notice.subscription else {
            continue;
        };
        if subscription.accepted {
            pipeline.accept_index_channel(subscription.source_id);
        } else {
            pipeline
                .refuse_index_channel(subscription.source_id, "cfbrti_subscription_rejected")?;
        }
    }
    Ok(())
}

/// The 15-minute grid the settlement window closes on (E28); the derived close
/// is the first `:00/:15/:30/:45` boundary at or after the proven upper bound,
/// matching `btc_feeds::kalshi_index`'s own `quarter_close_at_or_after`.
const QUARTER_HOUR_MILLIS: i64 = 900_000;

fn quarter_close_at_or_after(index_time: i64) -> i64 {
    let remainder = index_time.rem_euclid(QUARTER_HOUR_MILLIS);
    if remainder == 0 {
        index_time
    } else {
        index_time + (QUARTER_HOUR_MILLIS - remainder)
    }
}

/// Turn a `BackfillKalshiBrti` recovery notice into a settlement-window pause
/// (item a). Until a repair covers the gap, the affected quarter's settlement
/// window is withheld — never averaged over a lost print — instead of the lane
/// failing. The pause is checkpointed, so a restart mid-recovery keeps
/// withholding; the runtime releases it when a later settlement window admits.
fn apply_settlement_backfill<W: std::io::Write>(
    pipeline: &mut PredictionPipeline<W>,
    outcome: &btc_feeds::IngestOutcome,
) -> Result<(), PredictionPipelineError> {
    for notice in &outcome.notices {
        let Some(RecoveryAction::BackfillKalshiBrti {
            index_id,
            after_index_time,
            through_index_time,
        }) = &notice.recovery
        else {
            continue;
        };
        pipeline.pause_settlement_window(SettlementWindowPause {
            index_id: index_id.clone(),
            quarter_close_time: quarter_close_at_or_after(*through_index_time),
            after_index_time: *after_index_time,
            through_index_time: *through_index_time,
        })?;
    }
    Ok(())
}

fn auxiliary_socket_transition(code: FeedNoticeCode) -> Option<PredictionSocketTransition> {
    match code {
        FeedNoticeCode::Disconnected => Some(PredictionSocketTransition::Disconnected),
        FeedNoticeCode::Connected => Some(PredictionSocketTransition::Connected),
        _ => None,
    }
}

pub fn replay(
    journal_path: &Path,
    output_path: &Path,
    maximum_updates: u64,
    maximum_output_bytes: u64,
    maximum_journal_bytes: u64,
) -> Result<ReplayReport> {
    let mut reader = PredictionJournalReplayReader::open(journal_path, maximum_journal_bytes)
        .context("open prediction journal")?;
    let session = reader.session().clone();
    let wall_start = DateTime::parse_from_rfc3339(&session.wall_start)
        .context("parse prediction journal wall_start")?
        .with_timezone(&Utc);
    // Replay reconstructs the exact published set, so it reads the journal's own
    // arming (E43): an armed journal restores the settlement twins and the global
    // oracle reference; a disarmed one is byte-identical to the four-venue lane.
    let index_armed = session.runtime_config.index_armed;
    let processor = if let Some(checkpoint) = &session.initial_checkpoint {
        // A rotated file generation (its session carries an admission epoch)
        // rebuilds the active binding from the checkpoint so market-relative
        // events after the boundary replay without a fresh Bind (C5); a plain
        // crash-resume journal leaves `active = None` and re-discovers.
        if session.admission_epoch.is_some() {
            PredictionSignalProcessor::restore_rotated_indexed(
                session.publisher_epoch.clone(),
                wall_start,
                checkpoint.processor_state.clone(),
                index_armed,
            )
            .context("restore rotated replay processor")?
        } else {
            PredictionSignalProcessor::restore_indexed(
                session.publisher_epoch.clone(),
                wall_start,
                checkpoint.processor_state.clone(),
                index_armed,
            )
            .context("restore replay processor")?
        }
    } else {
        PredictionSignalProcessor::new_indexed(
            session.venue,
            session.publisher_epoch.clone(),
            wall_start,
            index_armed,
        )
        .context("build replay processor")?
    };
    let output = private_output(output_path).context("create replay output")?;
    let mut pipeline = PredictionPipeline::replay(
        BufWriter::with_capacity(1024 * 1024, output),
        processor,
        &session,
        maximum_updates,
        maximum_output_bytes,
    )
    .context("build replay pipeline")?;
    while let Some(input) = reader.next_input().context("read prediction journal")? {
        pipeline
            .admit_replay(input)
            .context("apply prediction replay input")?;
    }
    let end = reader
        .end()
        .context("prediction journal has no end record")?
        .clone();
    pipeline.sync_output().context("sync replay output")?;
    let pipeline = pipeline
        .finish_replay(&end)
        .context("finish prediction replay")?;
    Ok(ReplayReport {
        venue: session.venue,
        pipeline,
    })
}

fn admit_prediction_live(
    pipeline: &Rc<RefCell<PredictionPipeline<BufWriter<File>>>>,
    venue: PredictionMarketVenue,
    admission: PredictionLiveAdmission,
) -> Result<(), PredictionLiveError> {
    let mut pipeline = pipeline.borrow_mut();
    let admission_seq = pipeline.next_admission_seq();
    let input = match admission {
        PredictionLiveAdmission::Discovery {
            generation,
            attempt,
            now_ns,
        } => PredictionJournalInput::Discovery {
            admission_seq,
            generation,
            attempt,
            now_ns,
        },
        PredictionLiveAdmission::Bind {
            generation,
            previous_market_id,
            reason,
            event,
            logical_receipt_ns,
            now_ns,
        } => PredictionJournalInput::Bind {
            admission_seq,
            generation,
            previous_market_id,
            reason,
            event,
            logical_receipt_ns,
            now_ns,
        },
        PredictionLiveAdmission::MarketOutcome {
            generation,
            outcome,
            logical_receipt_ns,
            now_ns,
        } => {
            // Ruling E25: the venue refuses and later accepts the 5 Hz BRTI
            // channel on the same credentials, so its availability arrives as a
            // notice rather than as data. Read it BEFORE the empty-events
            // return, because a refusal carries no event — that early return is
            // exactly how this fact used to be dropped.
            apply_index_channel_status(&mut pipeline, &outcome)
                .map_err(|error| PredictionLiveError::InvalidOptions(error.to_string()))?;
            // A CF gap raises `BackfillKalshiBrti` and, like a refusal, carries
            // no event: read it BEFORE the empty-events return. It withholds the
            // affected quarter's settlement window (item a, the pause half)
            // rather than being a hard error; the resume half is driven in the
            // runtime when a settlement window is admitted past the pause.
            apply_settlement_backfill(&mut pipeline, &outcome)
                .map_err(|error| PredictionLiveError::InvalidOptions(error.to_string()))?;
            if outcome.events.is_empty() {
                return Ok(());
            }
            PredictionJournalInput::MarketEvents {
                admission_seq,
                generation,
                events: Arc::new(outcome.events),
                logical_receipt_ns,
                now_ns,
            }
        }
        PredictionLiveAdmission::Unavailable {
            generation,
            previous_market_id,
            reason,
            now_ns,
        } => PredictionJournalInput::Unavailable {
            admission_seq,
            generation,
            previous_market_id,
            reason,
            now_ns,
        },
        PredictionLiveAdmission::Connectivity { transition, now_ns } => {
            PredictionJournalInput::Connectivity {
                admission_seq,
                transport: PredictionTransport::for_venue(venue),
                transition,
                now_ns,
            }
        }
    };
    pipeline
        .admit_live(input)
        .map_err(|error| PredictionLiveError::Publication(error.to_string()))
}

/// Admission boundary for the dedicated Kalshi index socket (ruling E37d).
///
/// Index frames carry no market binding: they are auxiliary events admitted
/// under whatever generation is current when they arrive — the active market's,
/// or, in the gap between a roll and the successor bind, the next reserved one
/// (`current_generation_for_auxiliary_source`). This is what lets a completing
/// settlement-window frame that lands after the roll still reach the departed
/// market's E37c slot. Connectivity is stamped with the `KalshiIndex` transport
/// so feed health reports the index socket separately from the market socket,
/// and a market-socket disconnect never touches the index socket's health.
fn admit_prediction_index(
    pipeline: &Rc<RefCell<PredictionPipeline<BufWriter<File>>>>,
    admission: PredictionLiveAdmission,
) -> Result<(), PredictionLiveError> {
    let mut pipeline = pipeline.borrow_mut();
    match admission {
        PredictionLiveAdmission::MarketOutcome {
            outcome,
            logical_receipt_ns,
            now_ns,
            ..
        } => {
            // A 5 Hz refusal (E25) or a CF gap (BackfillKalshiBrti) carries no
            // event, so read both BEFORE the empty-events return — exactly as the
            // market handler does — else the fact is dropped.
            apply_index_channel_status(&mut pipeline, &outcome)
                .map_err(|error| PredictionLiveError::InvalidOptions(error.to_string()))?;
            apply_settlement_backfill(&mut pipeline, &outcome)
                .map_err(|error| PredictionLiveError::InvalidOptions(error.to_string()))?;
            if outcome.events.is_empty() {
                return Ok(());
            }
            let generation = pipeline
                .current_generation_for_auxiliary_source()
                .map_err(|error| PredictionLiveError::Publication(error.to_string()))?;
            let admission_seq = pipeline.next_admission_seq();
            pipeline
                .admit_live(PredictionJournalInput::MarketEvents {
                    admission_seq,
                    generation,
                    events: Arc::new(outcome.events),
                    logical_receipt_ns,
                    now_ns,
                })
                .map_err(|error| PredictionLiveError::Publication(error.to_string()))
        }
        PredictionLiveAdmission::Connectivity { transition, now_ns } => {
            let admission_seq = pipeline.next_admission_seq();
            pipeline
                .admit_live(PredictionJournalInput::Connectivity {
                    admission_seq,
                    transport: PredictionTransport::KalshiIndex,
                    transition,
                    now_ns,
                })
                .map_err(|error| PredictionLiveError::Publication(error.to_string()))
        }
        // The index runner never discovers, binds, or departs a market: those
        // admissions are the market runner's alone.
        other => Err(PredictionLiveError::InvalidOptions(format!(
            "the index socket offered an admission it never produces: {other:?}"
        ))),
    }
}

/// Derive the wall-clock second and RFC3339 `Z` timestamp at `now_ns` on the
/// process clock. The generation directory name and the `RUN-META.json`
/// `started_utc`/`sealed_utc` fields are stamped from the logical clock, not the
/// host wall clock, so they stay consistent with a resumed run's origin.
fn wall_clock_stamp(clock: &ProcessClock, now_ns: u64) -> Result<(u64, String)> {
    let at = clock.wall_start()
        + ChronoDuration::nanoseconds(
            i64::try_from(now_ns).context("logical time exceeds i64 nanoseconds")?,
        );
    let unix = u64::try_from(at.timestamp()).context("logical time predates the unix epoch")?;
    Ok((unix, at.to_rfc3339_opts(SecondsFormat::Secs, true)))
}

/// Build one generation's `RUN-META.json` payload. `publisher_epoch` is the
/// process admission epoch (constant across the run's generations, as live-run.sh
/// records it); `duration_seconds` is the per-generation rotation period.
#[allow(clippy::too_many_arguments)]
fn rotation_run_meta(
    lane: &str,
    publisher_epoch: &str,
    generation: &str,
    started_utc: &str,
    rotate_seconds: u64,
    caps: FileGenerationCaps,
    headroom_factor: u64,
    resumed: bool,
) -> RunMeta {
    RunMeta {
        lane: lane.to_owned(),
        publisher_epoch: publisher_epoch.to_owned(),
        generation: generation.to_owned(),
        started_utc: started_utc.to_owned(),
        duration_seconds: rotate_seconds,
        max_output_bytes: caps.max_output_bytes,
        max_journal_bytes: caps.max_journal_bytes,
        max_updates: caps.max_updates,
        headroom_factor,
        resumed,
    }
}

/// Convert an absolute process-clock deadline — a logical nanosecond count from
/// the frozen origin, which therefore carries any `--resume-from-checkpoint`
/// logical offset (`RotationSchedule::next_deadline` is `first_open_ns + n *
/// period`, and `first_open_ns` is a `ProcessClock::elapsed_ns` reading) — into
/// the tokio [`Instant`] at which the clock reaches it.
///
/// `ProcessClock::deadline(d)` is process-*relative* (`mono_start + d`), so
/// feeding it an absolute deadline overshoots by the logical offset and, on every
/// resumed run, lands the first rotation hours late. We instead anchor on the
/// current logical time: the remaining wait is the absolute deadline minus
/// `elapsed_ns()`, measured from now. A deadline already in the past yields a
/// zero wait, so a slow seal rotates immediately rather than never.
fn rotation_instant(clock: &ProcessClock, absolute_deadline_ns: u64) -> Result<Instant> {
    let now_ns = clock
        .elapsed_ns()
        .context("sample rotation schedule time")?;
    let remaining = absolute_deadline_ns.saturating_sub(now_ns);
    Ok(Instant::now() + Duration::from_nanos(remaining))
}

/// Seal the final still-open file generation at a clean graceful end (duration
/// expiry or SIGTERM), stamping its `RUN-META.json` `sealed_utc` from the logical
/// clock exactly as the rotation path seals each departed generation. Explicit
/// mode carries no rotation runtime, so the call is inert.
fn seal_final_open_generation(
    rotation: Option<&mut RotationRuntime>,
    clock: &ProcessClock,
    now_ns: u64,
) -> Result<()> {
    if let Some(rotation) = rotation {
        let (_, sealed_utc) = wall_clock_stamp(clock, now_ns)?;
        rotation
            .current
            .seal(&sealed_utc)
            .context("seal the final prediction file generation")?;
    }
    Ok(())
}

async fn maintain(
    pipeline: Rc<RefCell<PredictionPipeline<BufWriter<File>>>>,
    clock: ProcessClock,
    duration: Duration,
    checkpoint_interval: Duration,
    mut shutdown: LiveShutdown,
    mut rotation: Option<RotationRuntime>,
) -> Result<(MaintenanceReport, Option<RotationRuntime>)> {
    let deadline = clock.deadline(duration);
    let mut health = interval_at(
        Instant::now() + Duration::from_secs(1),
        Duration::from_secs(1),
    );
    let mut checkpoint = interval_at(Instant::now() + checkpoint_interval, checkpoint_interval);
    let mut health_ticks = 0_u64;
    let mut interval_checkpoints = 0_u64;
    let mut interrupted = false;
    // Rotation deadline as a tokio Instant on the process clock. Far in the
    // future (and never re-armed) when the run is not rotating, so the arm is
    // inert in explicit mode.
    let mut rotation_deadline = match &rotation {
        Some(state) => rotation_instant(&clock, state.schedule.next_deadline())?,
        None => clock.deadline(duration) + Duration::from_secs(1),
    };
    loop {
        tokio::select! {
            result = shutdown.wait_reason() => {
                interrupted = result
                    .context("listen for shutdown signal")?
                    .interrupted();
                break;
            }
            () = sleep_until(deadline) => break,
            () = sleep_until(rotation_deadline), if rotation.is_some() => {
                let now_ns = clock.elapsed_ns().context("sample rotation time")?;
                rotate_generation(&pipeline, &clock, now_ns, rotation.as_mut().expect("rotation present"))?;
                rotation_deadline = rotation_instant(
                    &clock,
                    rotation.as_ref().expect("rotation present").schedule.next_deadline(),
                )?;
            }
            _ = health.tick() => {
                let now_ns = clock.elapsed_ns().context("sample health time")?;
                let mut pipeline = pipeline.borrow_mut();
                let admission_seq = pipeline.next_admission_seq();
                pipeline.admit_live(PredictionJournalInput::HealthTick {
                    admission_seq,
                    now_ns,
                }).context("admit prediction health tick")?;
                health_ticks = health_ticks.checked_add(1)
                    .context("health tick counter exhausted")?;
            }
            _ = checkpoint.tick() => {
                let now_ns = clock.elapsed_ns().context("sample checkpoint time")?;
                let mut pipeline = pipeline.borrow_mut();
                pipeline.sync_output().context("sync interval prediction output")?;
                pipeline.checkpoint_live(now_ns)
                    .context("write interval prediction checkpoint")?;
                interval_checkpoints = interval_checkpoints.checked_add(1)
                    .context("checkpoint counter exhausted")?;
            }
        }
    }
    // Hand the still-open generation back so the graceful-end path in `run` can
    // seal its RUN-META after finalization (mirrors spot main.rs:1026-1036); the
    // rotation path already seals every departed generation as it rotates.
    Ok((
        MaintenanceReport {
            health_ticks,
            interval_checkpoints,
            interrupted,
        },
        rotation,
    ))
}

/// Perform one in-process file-generation rotation: open the successor
/// generation, swap the pipeline's writers into it (keeping the warm processor,
/// admission epoch, clock and admission sequence), seal the departed
/// generation's `RUN-META.json`, and advance the drift-free schedule.
///
/// If the successor cannot be opened (disk, an existing directory), the still-open
/// current generation is sealed and the error is returned so the process exits
/// with the I/O failure class rather than writing on into files that were never
/// opened.
fn rotate_generation(
    pipeline: &Rc<RefCell<PredictionPipeline<BufWriter<File>>>>,
    clock: &ProcessClock,
    now_ns: u64,
    rotation: &mut RotationRuntime,
) -> Result<()> {
    // Name the successor from its SCHEDULED grid second (`first_open + n·period`),
    // never the observed open: advance the drift-free schedule to the current logical
    // time first, then take the grid deadline it now sits on. Because the schedule
    // advances at least one whole period per rotation, consecutive generations'
    // scheduled seconds are always at least one period apart, so a seal overrunning
    // into the next wall second can never reuse the just-sealed generation's directory
    // second and raise a same-second `GenerationExists` (C5). RUN-META `started_utc`
    // still records the observed open (`stamp`).
    rotation.schedule.advance_to(now_ns);
    let scheduled_deadline_ns = rotation.schedule.scheduled_open_ns();
    let name_unix = wall_clock_stamp(clock, scheduled_deadline_ns)?.0;
    let stamp = wall_clock_stamp(clock, now_ns)?.1;
    let lane = rotation.layout.lane().to_owned();
    let generation = format!("gen-live-{lane}-{name_unix}");
    let output_epoch = format!("{}-{name_unix}", rotation.epoch_prefix);
    let departed_name = rotation
        .current
        .dir
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
        .context("departed generation directory has no name")?;
    // The successor's RUN-META records the generation's own derived output epoch
    // (`<prefix>-<name_unix>`), matching the reset frame and checkpoint lineage the
    // rotated pipeline writes — not the base prefix (gen 0 keeps the bare prefix).
    let meta = rotation_run_meta(
        &lane,
        &output_epoch,
        &generation,
        &stamp,
        rotation.rotate_seconds,
        rotation.caps,
        rotation.headroom_factor,
        true,
    );
    match rotation.layout.open_next_with_rotation(
        name_unix,
        Some(&departed_name),
        &meta,
        rotation.rotate_seconds,
    ) {
        Ok(open) => {
            pipeline
                .borrow_mut()
                .rotate(now_ns, &open, output_epoch, rotation.caps)
                .context("rotate prediction pipeline")?;
            rotation
                .current
                .seal(&stamp)
                .context("seal departed generation RUN-META")?;
            rotation.current = open;
            // The pipeline rotate + seal (fsync, journal END, checkpoint) can overrun
            // one or more rotation periods. Re-advance the schedule on the POST-seal
            // clock so a slow seal skips the periods it missed rather than leaving the
            // next deadline already in the past (which would rotate back-to-back into
            // near-empty generations). `advance_to` only moves forward, so this never
            // rewinds the pre-seal advance that named this generation. Mirrors spot
            // main.rs:645-654 and venue lane_runtime.rs:1875-1878.
            let post_seal_ns = clock
                .elapsed_ns()
                .map(|elapsed| elapsed.max(now_ns))
                .unwrap_or(now_ns);
            // Floor the skip at this rotation's scheduled deadline so the schedule
            // always advances strictly past it: a rotation arm that fires marginally
            // before the deadline (post_seal_ns < scheduled_deadline_ns) then cannot
            // leave the next deadline at the same grid second and burn a spurious
            // back-to-back rotation. Mirrors spot main.rs:653.
            rotation
                .schedule
                .advance_to(post_seal_ns.max(scheduled_deadline_ns));
            Ok(())
        }
        Err(open_error) => {
            let sealed = pipeline.borrow_mut().seal_current(now_ns);
            let _ = rotation.current.seal(&stamp);
            match sealed {
                // The departed generation is sealed on disk, so a same-second successor
                // name (a rotation landing on a second an earlier run's directory
                // already used) is transient: the next boundary yields a fresh
                // second-stamped name. Exit Retryable (75) so the supervisor restarts
                // the lane rather than holding it down under Fatal (78). This maps only
                // the rotation path; the startup open of gen-0 keeps its Fatal class (a
                // cold start into an occupied lane is an operator error, never a blind
                // restart). Mirrors spot main.rs:582 and venue lane_runtime.rs:1921.
                Ok(()) => match open_error {
                    error @ FileGenerationError::GenerationExists { .. } => {
                        Err(anyhow::Error::new(ClassifiedMessage::new(
                            ExitClass::Retryable,
                            format!("rotation could not open the next file generation: {error}"),
                        )))
                    }
                    error => Err(anyhow::Error::new(error).context("open successor generation")),
                },
                Err(seal_error) => Err(anyhow::Error::new(seal_error)
                    .context("seal current generation after a failed rotation open")),
            }
        }
    }
}

async fn join_live_workers<P, T, M, S, PO, TO, MO>(
    prediction: P,
    twap: T,
    maintenance: M,
    shutdown: S,
) -> Result<(PO, TO, MO, LiveShutdownReason)>
where
    P: std::future::Future<Output = Result<PO>>,
    T: std::future::Future<Output = Result<TO>>,
    M: std::future::Future<Output = Result<MO>>,
    S: std::future::Future<Output = Result<LiveShutdownReason>>,
{
    tokio::try_join!(prediction, twap, maintenance, shutdown)
}

fn private_output(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    options.mode(0o600);
    options.open(path)
}

fn validate_run_arguments(arguments: &RunArguments) -> Result<()> {
    let duration_seconds = arguments.duration.as_secs();
    if duration_seconds == 0 || duration_seconds > MAX_RUN_SECONDS {
        anyhow::bail!("duration_seconds must be between 1 and {MAX_RUN_SECONDS}");
    }
    if arguments.checkpoint_interval_seconds == 0
        || arguments.checkpoint_interval_seconds >= duration_seconds
    {
        anyhow::bail!("checkpoint interval must be positive and shorter than the run");
    }
    if arguments
        .induced_reconnect_seconds
        .is_some_and(|value| value == 0 || value >= duration_seconds)
    {
        anyhow::bail!("induced reconnect interval must be positive and shorter than the run");
    }
    // Explicit mode carries all three paths and they must be distinct. Rotation
    // mode derives output/journal under the lane root (distinct by construction)
    // and only the checkpoint is a free path.
    if let (Some(output), Some(journal)) = (&arguments.output, &arguments.journal)
        && (output == journal
            || output == &arguments.checkpoint
            || journal == &arguments.checkpoint)
    {
        anyhow::bail!("output, journal, and checkpoint paths must be distinct");
    }
    if arguments.publisher_epoch.trim().is_empty() || arguments.publisher_epoch.len() > 128 {
        anyhow::bail!("publisher_epoch must contain 1 to 128 bytes");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The dual-venue `multi` lane (C1 phase 3, ruling E57).
// ---------------------------------------------------------------------------

pub struct MultiRunArguments {
    /// Explicit-mode output path; `None` in rotation mode (derived under the lane
    /// root), mirroring the single-venue `RunArguments`.
    pub output: Option<PathBuf>,
    /// Explicit-mode journal path; `None` in rotation mode.
    pub journal: Option<PathBuf>,
    pub checkpoint: PathBuf,
    pub publisher_epoch: String,
    pub duration: Duration,
    pub discovery_retry_ms: u64,
    pub checkpoint_interval_seconds: u64,
    pub induced_reconnect_seconds: Option<u64>,
    pub max_updates: u64,
    pub max_output_bytes: u64,
    pub max_journal_bytes: u64,
    pub max_checkpoint_bytes: u64,
    /// Rotation-mode configuration, or `None` for explicit mode. Carries the
    /// resume flag through `resumed` for the generation's `RUN-META`.
    pub rotation: Option<MultiRotationConfig>,
}

/// In-process file-generation rotation configuration for the multi lane (C5/E57),
/// mirroring the single-venue [`RotationConfig`]. The lane name is always `multi`.
pub struct MultiRotationConfig {
    pub lane_root: PathBuf,
    pub rotate_seconds: u64,
    pub headroom_factor: u64,
    pub resumed: bool,
}

/// Live rotation state for the multi lane threaded into [`maintain_multi`]: the
/// layout, the drift-free schedule, the per-generation caps, and the currently
/// open generation sealed at the next boundary. Mirrors [`RotationRuntime`] but
/// carries `max_journal_bytes` because the multi lane opens its own merged journal
/// per generation.
struct MultiRotationRuntime {
    layout: FileGenerationLayout,
    schedule: RotationSchedule,
    rotate_seconds: u64,
    headroom_factor: u64,
    caps: FileGenerationCaps,
    epoch_prefix: String,
    max_journal_bytes: u64,
    current: OpenFileGeneration,
}

#[derive(Debug, Serialize)]
pub struct MultiRunReport {
    pub duration_seconds: u64,
    pub elapsed_ns: u64,
    pub kalshi_live: PredictionLiveReport,
    pub polymarket_live: PredictionLiveReport,
    pub coinbase_spot_live: LiveReport,
    pub kraken_spot_live: LiveReport,
    pub twap_live: LiveReport,
    pub health_ticks: u64,
    pub interval_checkpoints: u64,
    pub interrupted: bool,
}

type SharedMultiRuntime = Rc<RefCell<MultiLaneRuntime<BufWriter<File>>>>;
type SharedJournal = Rc<RefCell<MergedJournalWriter<BufWriter<File>>>>;

/// Map one live admission to a merged-journal input, stamping the venue's own
/// next admission sequence. A `MarketOutcome` carrying no event yields `None`
/// (nothing to journal or apply); the multi lane runs both venues index-disarmed,
/// so there is no index-channel status to fold in.
fn map_multi_admission(
    admission_seq: u64,
    venue: PredictionMarketVenue,
    admission: PredictionLiveAdmission,
) -> Option<PredictionJournalInput> {
    match admission {
        PredictionLiveAdmission::Discovery {
            generation,
            attempt,
            now_ns,
        } => Some(PredictionJournalInput::Discovery {
            admission_seq,
            generation,
            attempt,
            now_ns,
        }),
        PredictionLiveAdmission::Bind {
            generation,
            previous_market_id,
            reason,
            event,
            logical_receipt_ns,
            now_ns,
        } => Some(PredictionJournalInput::Bind {
            admission_seq,
            generation,
            previous_market_id,
            reason,
            event,
            logical_receipt_ns,
            now_ns,
        }),
        PredictionLiveAdmission::MarketOutcome {
            generation,
            outcome,
            logical_receipt_ns,
            now_ns,
        } => {
            if outcome.events.is_empty() {
                None
            } else {
                Some(PredictionJournalInput::MarketEvents {
                    admission_seq,
                    generation,
                    events: Arc::new(outcome.events),
                    logical_receipt_ns,
                    now_ns,
                })
            }
        }
        PredictionLiveAdmission::Unavailable {
            generation,
            previous_market_id,
            reason,
            now_ns,
        } => Some(PredictionJournalInput::Unavailable {
            admission_seq,
            generation,
            previous_market_id,
            reason,
            now_ns,
        }),
        PredictionLiveAdmission::Connectivity { transition, now_ns } => {
            Some(PredictionJournalInput::Connectivity {
                admission_seq,
                transport: PredictionTransport::for_venue(venue),
                transition,
                now_ns,
            })
        }
    }
}

/// Journal one merged record and apply it to the runtime (which drives the inner
/// venue pipeline and the two blends). Returns a bounded error string.
fn admit_multi_input(
    runtime: &SharedMultiRuntime,
    journal: &SharedJournal,
    venue: MultiVenueTag,
    build: impl FnOnce(u64) -> PredictionJournalInput,
) -> Result<(), String> {
    let seq = runtime.borrow().next_admission_seq(venue);
    let input = build(seq);
    journal
        .borrow_mut()
        .write_record(&MultiJournalRecord {
            venue,
            input: input.clone(),
        })
        .map_err(|error| error.to_string())?;
    runtime
        .borrow_mut()
        .admit(venue, input)
        .map_err(|error| error.to_string())
}

/// One venue market runner for the multi lane (index-disarmed).
#[allow(clippy::too_many_arguments)]
async fn run_multi_prediction(
    venue: PredictionMarketVenue,
    tag: MultiVenueTag,
    epoch: String,
    duration: Duration,
    discovery_retry_ms: u64,
    induced_reconnect_seconds: Option<u64>,
    shutdown: LiveShutdown,
    clock: ProcessClock,
    runtime: SharedMultiRuntime,
    journal: SharedJournal,
) -> Result<PredictionLiveReport> {
    let mut options = PredictionLiveOptions::new(venue, duration, epoch, 1, None);
    options.shutdown = Some(shutdown);
    options.discovery_retry = Duration::from_millis(discovery_retry_ms);
    options.induced_reconnect_interval = induced_reconnect_seconds.map(Duration::from_secs);
    options.index_sockets = ArmedIndexSockets::default();
    run_prediction_live(options, clock, move |admission| {
        let seq = runtime.borrow().next_admission_seq(tag);
        let Some(input) = map_multi_admission(seq, venue, admission) else {
            return Ok(());
        };
        journal
            .borrow_mut()
            .write_record(&MultiJournalRecord {
                venue: tag,
                input: input.clone(),
            })
            .map_err(|error| PredictionLiveError::Publication(error.to_string()))?;
        runtime
            .borrow_mut()
            .admit(tag, input)
            .map_err(|error| PredictionLiveError::Publication(error.to_string()))
    })
    .await
    .map_err(anyhow::Error::from)
}

/// One shared spot ticker socket fanned to BOTH venue pipelines' embedded spot
/// references (E57: the sockets are opened once per process).
#[allow(clippy::too_many_arguments)]
async fn run_multi_spot(
    stream: CaptureStream,
    transport: PredictionTransport,
    duration: Duration,
    publisher_epoch: String,
    shutdown: LiveShutdown,
    clock: ProcessClock,
    runtime: SharedMultiRuntime,
    journal: SharedJournal,
) -> Result<LiveReport> {
    let options = LiveOptions {
        stream,
        duration,
        kraken_btc_book_depth: SPOT_SIGNAL_KRAKEN_BOOK_DEPTH,
        publisher_epoch,
        diagnostics: LiveDiagnostics::default(),
        shutdown: Some(shutdown),
        kraken_trade_reconciliation: None,
    };
    let report = run_live(options, clock.clone(), move |timed| {
        let transitions = timed
            .outcome
            .notices
            .iter()
            .filter_map(|notice| auxiliary_socket_transition(notice.code))
            .collect::<Vec<_>>();
        let events: Vec<SourceEvent> = timed
            .outcome
            .events
            .into_iter()
            .filter(|event| is_spot_reference_ticker(&event.source_id))
            .collect();
        if !transitions.is_empty() || !events.is_empty() {
            let now_ns = clock.admission_ns(timed.logical_receipt_ns)?;
            for tag in [MultiVenueTag::Kalshi, MultiVenueTag::Polymarket] {
                for transition in &transitions {
                    let transition = *transition;
                    admit_multi_input(&runtime, &journal, tag, |seq| {
                        PredictionJournalInput::Connectivity {
                            admission_seq: seq,
                            transport,
                            transition,
                            now_ns,
                        }
                    })
                    .map_err(btc_feeds::live::LiveError::Publication)?;
                }
                if !events.is_empty() {
                    let events = events.clone();
                    let logical_receipt_ns = timed.logical_receipt_ns;
                    admit_multi_input(&runtime, &journal, tag, |seq| {
                        PredictionJournalInput::SpotEvents {
                            admission_seq: seq,
                            events: Arc::new(events),
                            logical_receipt_ns,
                            now_ns,
                        }
                    })
                    .map_err(btc_feeds::live::LiveError::Publication)?;
                }
            }
        }
        Ok(LiveControl::Continue)
    })
    .await?;
    Ok(report)
}

/// The Polymarket Chainlink-TWAP relay, admitted as auxiliary events to the
/// Polymarket pipeline only.
async fn run_multi_twap(
    duration: Duration,
    publisher_epoch: String,
    shutdown: LiveShutdown,
    clock: ProcessClock,
    runtime: SharedMultiRuntime,
    journal: SharedJournal,
) -> Result<LiveReport> {
    let options = LiveOptions {
        stream: CaptureStream::PolymarketChainlinkTwap,
        duration,
        kraken_btc_book_depth: 10,
        publisher_epoch,
        diagnostics: LiveDiagnostics::default(),
        shutdown: Some(shutdown),
        kraken_trade_reconciliation: None,
    };
    let report = run_live(options, clock.clone(), move |timed| {
        let transitions = timed
            .outcome
            .notices
            .iter()
            .filter_map(|notice| auxiliary_socket_transition(notice.code))
            .collect::<Vec<_>>();
        let events = timed.outcome.events;
        if !transitions.is_empty() || !events.is_empty() {
            let now_ns = clock.admission_ns(timed.logical_receipt_ns)?;
            for transition in transitions {
                admit_multi_input(&runtime, &journal, MultiVenueTag::Polymarket, |seq| {
                    PredictionJournalInput::Connectivity {
                        admission_seq: seq,
                        transport: PredictionTransport::PolymarketRtds,
                        transition,
                        now_ns,
                    }
                })
                .map_err(btc_feeds::live::LiveError::Publication)?;
            }
            if !events.is_empty() {
                let generation = runtime
                    .borrow()
                    .current_generation_for_auxiliary_source(MultiVenueTag::Polymarket)
                    .map_err(|error| btc_feeds::live::LiveError::Publication(error.to_string()))?;
                let logical_receipt_ns = timed.logical_receipt_ns;
                admit_multi_input(&runtime, &journal, MultiVenueTag::Polymarket, |seq| {
                    PredictionJournalInput::MarketEvents {
                        admission_seq: seq,
                        generation,
                        events: Arc::new(events),
                        logical_receipt_ns,
                        now_ns,
                    }
                })
                .map_err(btc_feeds::live::LiveError::Publication)?;
            }
        }
        Ok(LiveControl::Continue)
    })
    .await?;
    Ok(report)
}

/// The multi lane's health-tick and checkpoint maintenance loop: a health tick is
/// fanned to both venues; the checkpoint timer syncs output and writes the multi
/// checkpoint (both admission cursors + the multi state).
#[allow(clippy::too_many_arguments)]
async fn maintain_multi(
    runtime: SharedMultiRuntime,
    journal: SharedJournal,
    shared: Rc<RefCell<BufWriter<File>>>,
    clock: ProcessClock,
    duration: Duration,
    checkpoint_interval: Duration,
    checkpoint_path: PathBuf,
    max_checkpoint_bytes: u64,
    mut shutdown: LiveShutdown,
    mut rotation: Option<MultiRotationRuntime>,
) -> Result<((u64, u64, bool), Option<MultiRotationRuntime>)> {
    let deadline = clock.deadline(duration);
    let mut health = interval_at(
        Instant::now() + Duration::from_secs(1),
        Duration::from_secs(1),
    );
    let mut checkpoint = interval_at(Instant::now() + checkpoint_interval, checkpoint_interval);
    let mut health_ticks = 0_u64;
    let mut interval_checkpoints = 0_u64;
    let mut interrupted = false;
    // Rotation deadline as a tokio Instant on the process clock. Far in the future
    // (and never re-armed) when the run is not rotating, so the arm is inert in
    // explicit mode (mirrors `maintain`).
    let mut rotation_deadline = match &rotation {
        Some(state) => rotation_instant(&clock, state.schedule.next_deadline())?,
        None => clock.deadline(duration) + Duration::from_secs(1),
    };
    loop {
        tokio::select! {
            result = shutdown.wait_reason() => {
                interrupted = result.context("listen for shutdown signal")?.interrupted();
                break;
            }
            () = sleep_until(deadline) => break,
            () = sleep_until(rotation_deadline), if rotation.is_some() => {
                let now_ns = clock.elapsed_ns().context("sample rotation time")?;
                rotate_multi_generation(
                    &runtime,
                    &journal,
                    &shared,
                    &checkpoint_path,
                    max_checkpoint_bytes,
                    &clock,
                    now_ns,
                    rotation.as_mut().expect("rotation present"),
                )?;
                rotation_deadline = rotation_instant(
                    &clock,
                    rotation.as_ref().expect("rotation present").schedule.next_deadline(),
                )?;
            }
            _ = health.tick() => {
                let now_ns = clock.elapsed_ns().context("sample health time")?;
                for tag in [MultiVenueTag::Kalshi, MultiVenueTag::Polymarket] {
                    admit_multi_input(&runtime, &journal, tag, |seq| {
                        PredictionJournalInput::HealthTick { admission_seq: seq, now_ns }
                    })
                    .map_err(|error| anyhow::anyhow!("admit multi health tick: {error}"))?;
                }
                health_ticks = health_ticks.checked_add(1)
                    .context("health tick counter exhausted")?;
            }
            _ = checkpoint.tick() => {
                runtime.borrow_mut().flush().context("sync multi output")?;
                journal.borrow_mut().flush().context("sync multi journal")?;
                write_multi_checkpoint(&runtime, &checkpoint_path, max_checkpoint_bytes)?;
                interval_checkpoints = interval_checkpoints.checked_add(1)
                    .context("checkpoint counter exhausted")?;
            }
        }
    }
    Ok(((health_ticks, interval_checkpoints, interrupted), rotation))
}

/// Persist the multi lane's bounded checkpoint (admission cursors + multi state).
fn write_multi_checkpoint(
    runtime: &SharedMultiRuntime,
    path: &Path,
    max_checkpoint_bytes: u64,
) -> Result<()> {
    let checkpoint = runtime.borrow().checkpoint();
    let json = serde_json::to_vec(&checkpoint).context("serialize multi checkpoint")?;
    if json.len() as u64 > max_checkpoint_bytes {
        anyhow::bail!(
            "multi checkpoint is {} bytes, over the {max_checkpoint_bytes}-byte cap",
            json.len()
        );
    }
    std::fs::write(path, &json).context("persist multi checkpoint")?;
    Ok(())
}

/// Perform one in-process file-generation rotation of the multi lane (C5/E57):
/// open the successor generation, seal the departed one (flush + fsync output,
/// flush journal, seal checkpoint), swap the shared output file and the merged
/// journal into the successor, reset the runtime's stream half onto the derived
/// output epochs (keeping both venue processors and the multi blends warm), seal
/// the departed generation's `RUN-META.json`, and advance the drift-free schedule.
///
/// If the successor cannot be opened, the still-open current generation is sealed
/// and the error is returned. A same-second `GenerationExists` maps to Retryable
/// (75) — the departed generation is sealed, so the next boundary yields a fresh
/// name — exactly as the single-venue `rotate_generation`.
#[allow(clippy::too_many_arguments)]
fn rotate_multi_generation(
    runtime: &SharedMultiRuntime,
    journal: &SharedJournal,
    shared: &Rc<RefCell<BufWriter<File>>>,
    checkpoint_path: &Path,
    max_checkpoint_bytes: u64,
    clock: &ProcessClock,
    now_ns: u64,
    rotation: &mut MultiRotationRuntime,
) -> Result<()> {
    // Name the successor from its SCHEDULED grid second (`first_open + n·period`),
    // never the observed open, so a seal overrunning into the next wall second can
    // never reuse the just-sealed generation's directory second (mirrors
    // `rotate_generation`).
    rotation.schedule.advance_to(now_ns);
    let scheduled_deadline_ns = rotation.schedule.scheduled_open_ns();
    let name_unix = wall_clock_stamp(clock, scheduled_deadline_ns)?.0;
    let stamp = wall_clock_stamp(clock, now_ns)?.1;
    let lane = rotation.layout.lane().to_owned();
    let generation = format!("gen-live-{lane}-{name_unix}");
    let output_epoch = format!("{}-{name_unix}", rotation.epoch_prefix);
    let departed_name = rotation
        .current
        .dir
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
        .context("departed generation directory has no name")?;
    let meta = rotation_run_meta(
        &lane,
        &output_epoch,
        &generation,
        &stamp,
        rotation.rotate_seconds,
        rotation.caps,
        rotation.headroom_factor,
        true,
    );
    match rotation.layout.open_next_with_rotation(
        name_unix,
        Some(&departed_name),
        &meta,
        rotation.rotate_seconds,
    ) {
        Ok(open) => {
            // 1. Seal the departed generation on disk: flush + fsync the output,
            //    flush the journal, and write the sealed checkpoint.
            runtime
                .borrow_mut()
                .flush()
                .context("flush departed multi output")?;
            shared
                .borrow()
                .get_ref()
                .sync_all()
                .context("fsync departed multi output")?;
            journal
                .borrow_mut()
                .flush()
                .context("flush departed multi journal")?;
            write_multi_checkpoint(runtime, checkpoint_path, max_checkpoint_bytes)?;
            // 2. Swap the shared output file to the successor generation.
            let output =
                private_output(&open.output_path).context("create successor multi output")?;
            *shared.borrow_mut() = BufWriter::with_capacity(1024 * 1024, output);
            // 3. Open the successor merged journal and write its two-epoch header
            //    (derived output epochs + the base admission epochs).
            let journal_file =
                private_output(&open.journal_path).context("create successor multi journal")?;
            let header = runtime.borrow().rotated_journal_header(&output_epoch);
            let mut new_journal =
                MergedJournalWriter::new(BufWriter::new(journal_file), rotation.max_journal_bytes);
            new_journal
                .write_header(&header)
                .context("write successor multi journal header")?;
            *journal.borrow_mut() = new_journal;
            // 4. Reset the runtime's stream half onto the derived output epochs;
            //    both venue processors and the multi blends stay warm in memory.
            runtime
                .borrow_mut()
                .rotate(&output_epoch, rotation.caps)
                .context("rotate multi runtime")?;
            // 5. Seal the departed generation's RUN-META and adopt the successor.
            rotation
                .current
                .seal(&stamp)
                .context("seal departed multi generation RUN-META")?;
            rotation.current = open;
            // 6. Re-advance the schedule on the post-seal clock, floored at this
            //    rotation's scheduled deadline (mirrors `rotate_generation`).
            let post_seal_ns = clock
                .elapsed_ns()
                .map(|elapsed| elapsed.max(now_ns))
                .unwrap_or(now_ns);
            rotation
                .schedule
                .advance_to(post_seal_ns.max(scheduled_deadline_ns));
            Ok(())
        }
        Err(open_error) => {
            // Seal the still-open current generation before bailing.
            let _ = runtime.borrow_mut().flush();
            let _ = shared.borrow().get_ref().sync_all();
            let _ = journal.borrow_mut().flush();
            let sealed = write_multi_checkpoint(runtime, checkpoint_path, max_checkpoint_bytes);
            let _ = rotation.current.seal(&stamp);
            match sealed {
                Ok(()) => match open_error {
                    error @ FileGenerationError::GenerationExists { .. } => {
                        Err(anyhow::Error::new(ClassifiedMessage::new(
                            ExitClass::Retryable,
                            format!(
                                "rotation could not open the next multi file generation: {error}"
                            ),
                        )))
                    }
                    error => {
                        Err(anyhow::Error::new(error).context("open successor multi generation"))
                    }
                },
                Err(seal_error) => {
                    Err(seal_error.context("seal current multi generation after a failed rotation"))
                }
            }
        }
    }
}

/// Seal the final still-open multi file generation at a clean graceful end,
/// stamping its `RUN-META.json` `sealed_utc` from the logical clock exactly as the
/// rotation path seals each departed generation. Explicit mode carries no rotation
/// runtime, so the call is inert (mirrors `seal_final_open_generation`).
fn seal_final_multi_generation(
    rotation: Option<&mut MultiRotationRuntime>,
    clock: &ProcessClock,
    now_ns: u64,
) -> Result<()> {
    if let Some(rotation) = rotation {
        let (_, sealed_utc) = wall_clock_stamp(clock, now_ns)?;
        rotation
            .current
            .seal(&sealed_utc)
            .context("seal the final multi file generation")?;
    }
    Ok(())
}

/// Validate the multi lane's duration and checkpoint interval, kept as a small
/// sync helper (mirroring `validate_run_arguments`) so it can be unit-tested
/// without instantiating `run_multi`'s large orchestration future.
fn validate_multi_run_arguments(
    duration: Duration,
    checkpoint_interval_seconds: u64,
) -> Result<()> {
    let duration_seconds = duration.as_secs();
    if duration_seconds == 0 || duration_seconds > MAX_RUN_SECONDS {
        anyhow::bail!("duration_seconds must be between 1 and {MAX_RUN_SECONDS}");
    }
    if checkpoint_interval_seconds == 0 || checkpoint_interval_seconds >= duration_seconds {
        anyhow::bail!("checkpoint interval must be positive and shorter than the run");
    }
    Ok(())
}

/// Build the multi lane's generation-0 merged-journal header from the base
/// process epoch and wall start.
///
/// Each leg's OUTPUT epoch is per-leg derived (`<prefix>.<venue>`) and stamps
/// emitted frames, but the still-running feeds stamp every admitted event with
/// the base process `--publisher-epoch`, and each inner leg validates admissions
/// against a single admission epoch. So generation 0 already carries the
/// two-epoch split, and recording the base process epoch as each leg's
/// `admission_epoch` here (E64/C5) keeps live and replay validating admissions
/// identically; rotated generations derive a fresh output epoch but keep this
/// same base admission epoch. Dropping either `admission_epoch` back to `None`
/// reintroduces the derived-epoch admission mismatch a live run dies on — the
/// gen-0 header unit tests guard against exactly that regression.
fn multi_generation_zero_headers(process_epoch: &str, wall_start: &str) -> MultiJournalHeader {
    MultiJournalHeader {
        publisher_epoch: process_epoch.to_owned(),
        wall_start: wall_start.to_owned(),
        kalshi: VenueHeader {
            publisher_epoch: format!("{process_epoch}.kalshi"),
            wall_start: wall_start.to_owned(),
            admission_epoch: Some(process_epoch.to_owned()),
            active_binding: None,
        },
        polymarket: VenueHeader {
            publisher_epoch: format!("{process_epoch}.polymarket"),
            wall_start: wall_start.to_owned(),
            admission_epoch: Some(process_epoch.to_owned()),
            active_binding: None,
        },
    }
}

/// The publisher epoch a still-running multi feed stamps onto every admitted
/// event: the base process epoch (the single admission epoch both inner legs
/// validate against), NOT the per-leg derived OUTPUT epoch. The derived output
/// epoch is applied to emitted frames by the runtime, not by the feed; handing a
/// feed the derived epoch reintroduces the E64 admission mismatch (a live run
/// dies on the first admission: "source event publisher_epoch disagrees with the
/// admission epoch"). This is the single decision point so the choice is
/// unit-testable and cannot silently regress at a feed call site.
fn multi_feed_publisher_epoch(process_epoch: &str, _derived_output_epoch: &str) -> String {
    process_epoch.to_owned()
}

pub async fn run_multi(arguments: MultiRunArguments) -> Result<(MultiRunReport, LiveShutdownReason)> {
    let duration = arguments.duration;
    let duration_seconds_report = duration.as_secs();
    validate_multi_run_arguments(duration, arguments.checkpoint_interval_seconds)?;
    // Rotation-aware resume: the multi lane keeps no per-venue processor
    // checkpoint (E57), so a `--resume-from-checkpoint` restart re-discovers both
    // legs and re-warms honestly rather than refusing the flag; a gap-free rotated
    // replay of the merged journals is the deterministic recovery path. Nothing to
    // restore here, so the run starts fresh under its new epoch.
    let clock = ProcessClock::start();
    let wall_start = clock.wall_start_text();
    // Per-leg derived OUTPUT epochs, retained here only as the explicit "wrong"
    // value handed to `multi_feed_publisher_epoch` so the feed-epoch choice has a
    // single testable decision point (see the helper); the gen-0 header derives
    // them itself.
    let kalshi_epoch = format!("{}.kalshi", arguments.publisher_epoch);
    let polymarket_epoch = format!("{}.polymarket", arguments.publisher_epoch);
    let header = multi_generation_zero_headers(&arguments.publisher_epoch, &wall_start);

    // Rotation mode opens the first file generation under the lane root now that
    // the clock exists and derives its output/journal paths; explicit mode uses
    // the caller-supplied paths unchanged (mirrors the single-venue `run`).
    let (output_path, journal_path, mut rotation_runtime) = match &arguments.rotation {
        Some(config) => {
            let caps = FileGenerationCaps {
                max_updates: arguments.max_updates,
                max_output_bytes: arguments.max_output_bytes,
                max_journal_bytes: arguments.max_journal_bytes,
            };
            let layout = FileGenerationLayout::new(&config.lane_root, "multi".to_owned())
                .context("bind multi lane root")?;
            let first_open_ns = clock.elapsed_ns().context("sample first-open time")?;
            let (now_unix, started_utc) = wall_clock_stamp(&clock, first_open_ns)?;
            let meta = rotation_run_meta(
                "multi",
                &arguments.publisher_epoch,
                &format!("gen-live-multi-{now_unix}"),
                &started_utc,
                config.rotate_seconds,
                caps,
                config.headroom_factor,
                config.resumed,
            );
            let open = layout
                .open_next_with_rotation(now_unix, None, &meta, config.rotate_seconds)
                .context("open first multi file generation")?;
            let schedule = RotationSchedule::try_new(
                first_open_ns,
                Duration::from_secs(config.rotate_seconds),
            )
            .context("start multi rotation schedule")?;
            let output_path = open.output_path.clone();
            let journal_path = open.journal_path.clone();
            (
                output_path,
                journal_path,
                Some(MultiRotationRuntime {
                    layout,
                    schedule,
                    rotate_seconds: config.rotate_seconds,
                    headroom_factor: config.headroom_factor,
                    caps,
                    epoch_prefix: arguments.publisher_epoch.clone(),
                    max_journal_bytes: arguments.max_journal_bytes,
                    current: open,
                }),
            )
        }
        None => {
            let output = arguments
                .output
                .clone()
                .context("explicit mode requires --output")?;
            let journal = arguments
                .journal
                .clone()
                .context("explicit mode requires --journal")?;
            (output, journal, None)
        }
    };

    let output = private_output(&output_path).context("create multi output")?;
    let shared = Rc::new(RefCell::new(BufWriter::with_capacity(1024 * 1024, output)));
    let runtime: SharedMultiRuntime = Rc::new(RefCell::new(
        runtime_from_header(
            Rc::clone(&shared),
            &header,
            arguments.max_updates,
            arguments.max_output_bytes,
        )
        .context("build multi runtime")?,
    ));
    let journal_file = private_output(&journal_path).context("create multi journal")?;
    let journal: SharedJournal = Rc::new(RefCell::new(MergedJournalWriter::new(
        BufWriter::new(journal_file),
        arguments.max_journal_bytes,
    )));
    journal
        .borrow_mut()
        .write_header(&header)
        .context("write multi journal header")?;
    // The initial checkpoint proves the path is writable before the run opens.
    write_multi_checkpoint(
        &runtime,
        &arguments.checkpoint,
        arguments.max_checkpoint_bytes,
    )?;

    let (shutdown_trigger, shutdown) = live_shutdown_channel();

    let kalshi_future = run_multi_prediction(
        PredictionMarketVenue::Kalshi,
        MultiVenueTag::Kalshi,
        multi_feed_publisher_epoch(&arguments.publisher_epoch, &kalshi_epoch),
        duration,
        arguments.discovery_retry_ms,
        arguments.induced_reconnect_seconds,
        shutdown.clone(),
        clock.clone(),
        Rc::clone(&runtime),
        Rc::clone(&journal),
    );
    let polymarket_future = run_multi_prediction(
        PredictionMarketVenue::Polymarket,
        MultiVenueTag::Polymarket,
        multi_feed_publisher_epoch(&arguments.publisher_epoch, &polymarket_epoch),
        duration,
        arguments.discovery_retry_ms,
        arguments.induced_reconnect_seconds,
        shutdown.clone(),
        clock.clone(),
        Rc::clone(&runtime),
        Rc::clone(&journal),
    );
    let coinbase_future = run_multi_spot(
        CaptureStream::CoinbaseCore,
        PredictionTransport::Coinbase,
        duration,
        arguments.publisher_epoch.clone(),
        shutdown.clone(),
        clock.clone(),
        Rc::clone(&runtime),
        Rc::clone(&journal),
    );
    let kraken_future = run_multi_spot(
        CaptureStream::KrakenCore,
        PredictionTransport::Kraken,
        duration,
        arguments.publisher_epoch.clone(),
        shutdown.clone(),
        clock.clone(),
        Rc::clone(&runtime),
        Rc::clone(&journal),
    );
    let twap_future = run_multi_twap(
        duration,
        arguments.publisher_epoch.clone(),
        shutdown.clone(),
        clock.clone(),
        Rc::clone(&runtime),
        Rc::clone(&journal),
    );
    let maintenance_future = maintain_multi(
        Rc::clone(&runtime),
        Rc::clone(&journal),
        Rc::clone(&shared),
        clock.clone(),
        duration,
        Duration::from_secs(arguments.checkpoint_interval_seconds),
        arguments.checkpoint.clone(),
        arguments.max_checkpoint_bytes,
        shutdown,
        rotation_runtime.take(),
    );
    let shutdown_deadline = clock.deadline(duration);
    let shutdown_future = async move {
        coordinate_live_shutdown(shutdown_deadline, shutdown_trigger)
            .await
            .context("listen for shutdown signal")
    };

    // Both venue runners and both spot runners share the runtime and shut down
    // together; join them as one worker so `join_live_workers` keeps its four-arm
    // shape. A fault in one venue degrades the merged output, never cancels the
    // other (E37d): each runner owns its own pipeline's readiness.
    let venue_and_spot = async move {
        tokio::try_join!(
            kalshi_future,
            polymarket_future,
            coinbase_future,
            kraken_future
        )
    };
    let (
        (kalshi_live, polymarket_live, coinbase_spot_live, kraken_spot_live),
        twap_live,
        (maintenance, mut final_rotation),
        shutdown_reason,
    ) = join_live_workers(
        venue_and_spot,
        twap_future,
        maintenance_future,
        shutdown_future,
    )
    .await?;

    let elapsed_ns = clock.elapsed_ns().context("sample final logical time")?;
    runtime.borrow_mut().flush().context("flush multi output")?;
    journal
        .borrow_mut()
        .flush()
        .context("flush multi journal")?;
    write_multi_checkpoint(
        &runtime,
        &arguments.checkpoint,
        arguments.max_checkpoint_bytes,
    )?;
    // Seal the final (still-open) generation's RUN-META on a clean graceful end,
    // exactly as the rotation path seals each departed generation; inert in
    // explicit mode.
    seal_final_multi_generation(final_rotation.as_mut(), &clock, elapsed_ns)?;

    Ok((
        MultiRunReport {
            duration_seconds: duration_seconds_report,
            elapsed_ns,
            kalshi_live,
            polymarket_live,
            coinbase_spot_live,
            kraken_spot_live,
            twap_live,
            health_ticks: maintenance.0,
            interval_checkpoints: maintenance.1,
            interrupted: maintenance.2,
        },
        shutdown_reason,
    ))
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    /// The rotation timer arm must fire one period after the first open even on a
    /// resumed run. `RotationSchedule::next_deadline` is an ABSOLUTE process-clock
    /// nanosecond count — `first_open_ns + n * period`, and `first_open_ns =
    /// clock.elapsed_ns()` carries the `--resume-from-checkpoint` logical offset
    /// (hours/days). `ProcessClock::deadline` is process-RELATIVE (`mono_start +
    /// d`), so feeding it the absolute deadline lands the first rotation the whole
    /// resume offset late — every production restart after the first. This drives
    /// the timer arm's deadline computation (`rotation_instant`) on a resumed clock
    /// with a ~1h logical offset and proves the instant is ~one period from now,
    /// and that the old absolute-into-relative formula would have been ~1h late.
    #[test]
    fn rotation_arm_fires_one_period_after_first_open_on_a_resumed_clock() {
        const RESUME_OFFSET_NS: u64 = 3_600 * 1_000_000_000; // one hour
        let period = Duration::from_secs(120);

        // A resume whose logical offset is ~1h: wall_start is "now", so the
        // downtime term is ~0 and the offset is the checkpoint time (1h).
        let wall_start = DateTime::<Utc>::from(SystemTime::now());
        let clock = ProcessClock::resume(wall_start, RESUME_OFFSET_NS).expect("resume clock");

        // The schedule aligns to the first-open process-clock reading, exactly as
        // `bin::run` builds it — so `next_deadline()` carries the resume offset.
        let first_open_ns = clock.elapsed_ns().expect("first open time");
        assert!(
            first_open_ns >= RESUME_OFFSET_NS,
            "the resumed clock carries the logical offset ({first_open_ns} >= {RESUME_OFFSET_NS})"
        );
        let schedule = RotationSchedule::try_new(first_open_ns, period).expect("schedule");

        let before = Instant::now();
        let arm = rotation_instant(&clock, schedule.next_deadline()).expect("rotation instant");
        let after = Instant::now();

        // The fix: the arm is ~one period from NOW, not one period + the ~1h offset.
        assert!(
            arm <= after + period,
            "the first rotation must fire within one period of now, not offset+period"
        );
        assert!(
            arm >= before + period - Duration::from_secs(1),
            "the first rotation fires ~one period after the first open"
        );

        // Regression guard: the old formula fed the absolute deadline into the
        // relative API, landing the first rotation the whole resume offset late.
        let buggy = clock.deadline(Duration::from_nanos(schedule.next_deadline()));
        assert!(
            buggy > after + Duration::from_secs(3_000),
            "the absolute-into-relative bug lands the first rotation ~an hour late"
        );
    }

    /// A rotation open that hits a same-second `GenerationExists` — a successor
    /// landing on a second the current generation's directory already used — seals
    /// the departed generation and exits Retryable (75), never Fatal (78), so the
    /// supervisor restarts the lane instead of holding it down under
    /// `RestartPreventExitStatus`. Mirrors spot (main.rs) and the venue lane
    /// (lane_runtime.rs); the startup open of gen-0 keeps its Fatal class (a cold
    /// start into an occupied lane is an operator error, and lives on a different
    /// code path — `run`, not `rotate_generation`).
    #[test]
    fn rotation_generation_exists_seals_and_exits_retryable() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let lane_root = std::env::temp_dir().join(format!("pred-rot-exists-{unique}"));
        std::fs::create_dir_all(&lane_root).expect("lane root");
        let lane = "kalshi";
        let epoch = "live-prediction-genexists";
        let caps = FileGenerationCaps {
            max_updates: 10_000,
            max_output_bytes: 8 * 1024 * 1024,
            max_journal_bytes: DEFAULT_PREDICTION_JOURNAL_MAX_BYTES,
        };
        let clock = ProcessClock::start();
        let first_open_ns = clock.elapsed_ns().expect("first open time");
        let (now_unix, started) = wall_clock_stamp(&clock, first_open_ns).expect("stamp");
        let layout = FileGenerationLayout::new(&lane_root, lane.to_owned()).expect("layout binds");
        let meta = rotation_run_meta(
            lane,
            epoch,
            &format!("gen-live-{lane}-{now_unix}"),
            &started,
            60,
            caps,
            2,
            false,
        );
        let open = layout
            .open_next_with_rotation(now_unix, None, &meta, 60)
            .expect("open gen0");
        let schedule =
            RotationSchedule::try_new(first_open_ns, Duration::from_secs(60)).expect("schedule");

        let runtime_config = PredictionRuntimeConfig::new(PredictionMarketVenue::Kalshi, 2_000)
            .with_index_armed(true);
        let session = PredictionJournalSession::new(
            clock.wall_start_text(),
            epoch.to_owned(),
            runtime_config,
        )
        .expect("session builds");
        let processor = PredictionSignalProcessor::new_indexed(
            PredictionMarketVenue::Kalshi,
            epoch,
            clock.wall_start(),
            true,
        )
        .expect("processor builds");
        let checkpoint_path = lane_root.join("checkpoint.json");
        let context = PredictionCheckpointContext {
            path: checkpoint_path.clone(),
            maximum_bytes: DEFAULT_PREDICTION_CHECKPOINT_MAX_BYTES,
            session_id: session.session_id.clone(),
            wall_start: session.wall_start.clone(),
            contract_fingerprint: session.contract_fingerprint.clone(),
            runtime_config: session.runtime_config.clone(),
            publisher_epoch_lineage: vec![epoch.to_owned()],
        };
        let initial = PredictionCheckpointDocument::new(
            context.session_id.clone(),
            context.wall_start.clone(),
            0,
            0,
            context.contract_fingerprint.clone(),
            context.runtime_config.clone(),
            processor.checkpoint_state(),
            context.publisher_epoch_lineage.clone(),
        )
        .expect("initial checkpoint builds");
        write_prediction_checkpoint(
            &checkpoint_path,
            initial,
            DEFAULT_PREDICTION_CHECKPOINT_MAX_BYTES,
            false,
        )
        .expect("initial checkpoint persists");
        let output = File::create(&open.output_path).expect("output file");
        let writer = BufWriter::new(output);
        let journal = PredictionJournalWriter::create(
            &open.journal_path,
            session.clone(),
            DEFAULT_PREDICTION_JOURNAL_MAX_BYTES,
        )
        .expect("journal writer");
        let pipeline = PredictionPipeline::live(
            writer,
            processor,
            &session,
            journal,
            context,
            caps.max_updates,
            caps.max_output_bytes,
        )
        .expect("live pipeline builds");
        let pipeline = Rc::new(RefCell::new(pipeline));

        let mut rotation = RotationRuntime {
            layout,
            schedule,
            rotate_seconds: 60,
            headroom_factor: 2,
            caps,
            epoch_prefix: epoch.to_owned(),
            current: open,
        };

        // Rotate WITHIN the first period: `advance_to` keeps the schedule at
        // generation 0, so the successor is named for the same second and collides
        // with the just-opened generation directory — the same-second
        // `GenerationExists` the fix must classify Retryable.
        let now_ns = first_open_ns + 1_000_000;
        let error = rotate_generation(&pipeline, &clock, now_ns, &mut rotation)
            .expect_err("a same-second successor name must fail the rotation open");
        assert_eq!(
            exit_class_for(&error),
            ExitClass::Retryable,
            "a rotation GenerationExists seals the departed generation and exits Retryable (75), not Fatal (78): {error:#}"
        );

        let _ = std::fs::remove_dir_all(&lane_root);
    }

    /// Build a warm rotation runtime + shared pipeline over a fresh lane root, so
    /// the rotation RUN-META tests can drive `rotate_generation` /
    /// `seal_final_open_generation` without standing up the live sockets. Returns
    /// the shared pipeline and the rotation runtime (whose `current` is gen 0).
    fn build_rotation_fixture(
        lane_root: &std::path::Path,
        lane: &str,
        epoch: &str,
        clock: &ProcessClock,
        first_open_ns: u64,
    ) -> (
        Rc<RefCell<PredictionPipeline<BufWriter<File>>>>,
        RotationRuntime,
    ) {
        std::fs::create_dir_all(lane_root).expect("lane root");
        let caps = FileGenerationCaps {
            max_updates: 10_000,
            max_output_bytes: 8 * 1024 * 1024,
            max_journal_bytes: DEFAULT_PREDICTION_JOURNAL_MAX_BYTES,
        };
        let (now_unix, started) = wall_clock_stamp(clock, first_open_ns).expect("stamp");
        let layout = FileGenerationLayout::new(lane_root, lane.to_owned()).expect("layout binds");
        let meta = rotation_run_meta(
            lane,
            epoch,
            &format!("gen-live-{lane}-{now_unix}"),
            &started,
            60,
            caps,
            2,
            false,
        );
        let open = layout
            .open_next_with_rotation(now_unix, None, &meta, 60)
            .expect("open gen0");
        let schedule =
            RotationSchedule::try_new(first_open_ns, Duration::from_secs(60)).expect("schedule");

        let runtime_config = PredictionRuntimeConfig::new(PredictionMarketVenue::Kalshi, 2_000)
            .with_index_armed(true);
        let session = PredictionJournalSession::new(
            clock.wall_start_text(),
            epoch.to_owned(),
            runtime_config,
        )
        .expect("session builds");
        let processor = PredictionSignalProcessor::new_indexed(
            PredictionMarketVenue::Kalshi,
            epoch,
            clock.wall_start(),
            true,
        )
        .expect("processor builds");
        let checkpoint_path = lane_root.join("checkpoint.json");
        let context = PredictionCheckpointContext {
            path: checkpoint_path.clone(),
            maximum_bytes: DEFAULT_PREDICTION_CHECKPOINT_MAX_BYTES,
            session_id: session.session_id.clone(),
            wall_start: session.wall_start.clone(),
            contract_fingerprint: session.contract_fingerprint.clone(),
            runtime_config: session.runtime_config.clone(),
            publisher_epoch_lineage: vec![epoch.to_owned()],
        };
        let initial = PredictionCheckpointDocument::new(
            context.session_id.clone(),
            context.wall_start.clone(),
            0,
            0,
            context.contract_fingerprint.clone(),
            context.runtime_config.clone(),
            processor.checkpoint_state(),
            context.publisher_epoch_lineage.clone(),
        )
        .expect("initial checkpoint builds");
        write_prediction_checkpoint(
            &checkpoint_path,
            initial,
            DEFAULT_PREDICTION_CHECKPOINT_MAX_BYTES,
            false,
        )
        .expect("initial checkpoint persists");
        let output = File::create(&open.output_path).expect("output file");
        let writer = BufWriter::new(output);
        let journal = PredictionJournalWriter::create(
            &open.journal_path,
            session.clone(),
            DEFAULT_PREDICTION_JOURNAL_MAX_BYTES,
        )
        .expect("journal writer");
        let pipeline = PredictionPipeline::live(
            writer,
            processor,
            &session,
            journal,
            context,
            caps.max_updates,
            caps.max_output_bytes,
        )
        .expect("live pipeline builds");
        let pipeline = Rc::new(RefCell::new(pipeline));

        let rotation = RotationRuntime {
            layout,
            schedule,
            rotate_seconds: 60,
            headroom_factor: 2,
            caps,
            epoch_prefix: epoch.to_owned(),
            current: open,
        };
        (pipeline, rotation)
    }

    /// A rotated generation's `RUN-META.json` records the generation's OWN derived
    /// output epoch (`<prefix>-<name_unix>`) — the same epoch the reset frame and
    /// checkpoint lineage carry — not the base publisher prefix (which is only
    /// correct for gen 0). Regression for the C5 kalshi/polymarket RUN-META defect.
    #[test]
    fn rotated_generation_run_meta_carries_the_derived_output_epoch() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let lane_root = std::env::temp_dir().join(format!("pred-rot-epoch-{unique}"));
        let lane = "kalshi";
        let epoch = "live-prediction-epoch";
        let clock = ProcessClock::start();
        let first_open_ns = clock.elapsed_ns().expect("first open time");
        let (pipeline, mut rotation) =
            build_rotation_fixture(&lane_root, lane, epoch, &clock, first_open_ns);

        // Rotate a full period past the first open so the successor is named for a
        // distinct grid second and the open succeeds.
        let now_ns = first_open_ns + 61 * 1_000_000_000;
        rotate_generation(&pipeline, &clock, now_ns, &mut rotation)
            .expect("rotation past the first period succeeds");

        let gen1_dir_name = rotation
            .current
            .dir
            .file_name()
            .and_then(|name| name.to_str())
            .expect("gen1 dir name")
            .to_owned();
        let name_unix = gen1_dir_name
            .strip_prefix(&format!("gen-live-{lane}-"))
            .expect("gen1 dir carries the lane-stamped suffix");
        let derived = format!("{epoch}-{name_unix}");
        let run_meta = std::fs::read_to_string(
            rotation
                .current
                .dir
                .join(crate::file_generation::RUN_META_FILE_NAME),
        )
        .expect("read gen1 RUN-META");
        assert!(
            run_meta.contains(&format!("\"publisher_epoch\": \"{derived}\"")),
            "rotated RUN-META must carry the derived output epoch {derived}, got: {run_meta}"
        );
        assert!(
            !run_meta.contains(&format!("\"publisher_epoch\": \"{epoch}\"")),
            "rotated RUN-META must NOT repeat the base prefix {epoch}"
        );

        let _ = std::fs::remove_dir_all(&lane_root);
    }

    /// A clean graceful end (duration expiry or SIGTERM) seals the final still-open
    /// generation's `RUN-META.json`, stamping `sealed_utc` — the C5 defect left it
    /// null for the prediction lanes because only rotated (departed) generations
    /// were sealed. `seal_final_open_generation` is the graceful-end hook `run`
    /// calls after finalization.
    #[test]
    fn final_open_generation_is_sealed_on_graceful_end() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let lane_root = std::env::temp_dir().join(format!("pred-rot-finalseal-{unique}"));
        let lane = "polymarket";
        let epoch = "live-prediction-finalseal";
        let clock = ProcessClock::start();
        let first_open_ns = clock.elapsed_ns().expect("first open time");
        let (_pipeline, mut rotation) =
            build_rotation_fixture(&lane_root, lane, epoch, &clock, first_open_ns);

        let run_meta_path = rotation
            .current
            .dir
            .join(crate::file_generation::RUN_META_FILE_NAME);
        let before = std::fs::read_to_string(&run_meta_path).expect("read fresh RUN-META");
        assert!(
            before.contains("\"sealed_utc\": null"),
            "a freshly opened generation is unsealed, got: {before}"
        );

        let now_ns = first_open_ns + 30 * 1_000_000_000;
        seal_final_open_generation(Some(&mut rotation), &clock, now_ns)
            .expect("graceful-end seal succeeds");

        assert!(
            rotation.current.sealed_utc().is_some(),
            "the final generation is sealed in memory"
        );
        let after = std::fs::read_to_string(&run_meta_path).expect("read sealed RUN-META");
        assert!(
            !after.contains("\"sealed_utc\": null"),
            "the sealed RUN-META must no longer report a null sealed_utc, got: {after}"
        );
        assert!(
            after.contains("\"sealed_utc\": \""),
            "the sealed RUN-META must carry a sealed_utc timestamp, got: {after}"
        );

        // Explicit mode (no rotation runtime) is inert.
        seal_final_open_generation(None, &clock, now_ns).expect("no-rotation seal is a no-op");

        let _ = std::fs::remove_dir_all(&lane_root);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn shared_interrupt_stops_all_workers_before_controlled_finalization() {
        let (trigger, shutdown) = live_shutdown_channel();
        let mut prediction = shutdown.clone();
        let mut twap = shutdown.clone();
        let mut maintenance = shutdown;
        let prediction_worker = async move {
            prediction.wait().await.context("prediction shutdown")?;
            Ok::<_, anyhow::Error>("prediction_stopped")
        };
        let twap_worker = async move {
            twap.wait().await.context("TWAP shutdown")?;
            Ok::<_, anyhow::Error>("twap_stopped")
        };
        let maintenance_worker = async move {
            maintenance.wait().await.context("maintenance shutdown")?;
            Ok::<_, anyhow::Error>("maintenance_stopped")
        };
        let coordinator = async move {
            coordinate_live_shutdown_on(Instant::now() + Duration::from_secs(60), trigger, async {
                Ok::<_, std::io::Error>(())
            })
            .await
            .context("coordinate test shutdown")
        };

        let (prediction, twap, maintenance, shutdown_reason) = join_live_workers(
            prediction_worker,
            twap_worker,
            maintenance_worker,
            coordinator,
        )
        .await
        .expect("shared shutdown joins");
        let controlled_finalizer_ran = prediction == "prediction_stopped"
            && twap == "twap_stopped"
            && maintenance == "maintenance_stopped";
        assert!(shutdown_reason.interrupted());
        assert!(controlled_finalizer_ran);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_stop_signal_cancels_all_workers_and_classifies_interrupted() {
        // The real coordinator now feeds `wait_for_stop_signal()` (SIGINT or
        // SIGTERM) into `coordinate_live_shutdown`; this stands in for that future
        // resolving on a signal. It must cancel the shared trigger every worker
        // holds and report interrupted, which `main` maps to exit 130. Raising a
        // real SIGTERM here is deliberately avoided: the signal would race the
        // lazily-installed tokio handler and could terminate the shared test
        // process, so the terminate path is proven through the generic future
        // instead (see report).
        let (trigger, shutdown) = live_shutdown_channel();
        let mut cancelled = shutdown.clone();
        let worker = async move {
            cancelled.wait().await.context("worker shutdown")?;
            Ok::<_, anyhow::Error>(())
        };
        let stop_signal = async { Ok::<_, std::io::Error>(()) };
        let coordinator = async move {
            coordinate_live_shutdown_on(
                Instant::now() + Duration::from_secs(60),
                trigger,
                stop_signal,
            )
            .await
            .context("coordinate test shutdown")
        };

        let (_, _, _, shutdown_reason) = join_live_workers(
            worker,
            async { Ok::<_, anyhow::Error>(()) },
            async { Ok::<_, anyhow::Error>(()) },
            coordinator,
        )
        .await
        .expect("stop-signal coordinator joins");
        assert!(
            shutdown_reason.interrupted(),
            "a stop signal cancels the workers and classifies the run interrupted"
        );
        let class = if shutdown_reason.interrupted() {
            ExitClass::Interrupted
        } else {
            ExitClass::Success
        };
        assert_eq!(class, ExitClass::Interrupted);
        assert_eq!(class.code(), 130);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn reached_natural_deadline_cancels_workers_and_finishes_journal() {
        const EPOCH: &str = "prediction-natural-deadline-test";
        let journal_path = temporary_test_path("natural-deadline", "jsonl");
        let checkpoint_path = temporary_test_path("natural-deadline", "checkpoint.json");
        let wall_start = "2026-08-15T12:00:00Z";
        let runtime_config = PredictionRuntimeConfig::new(PredictionMarketVenue::Kalshi, 2_000);
        let session = PredictionJournalSession::new(
            wall_start.to_owned(),
            EPOCH.to_owned(),
            runtime_config.clone(),
        )
        .expect("journal session builds");
        let processor = PredictionSignalProcessor::new(
            PredictionMarketVenue::Kalshi,
            EPOCH,
            wall_start.parse().expect("wall start parses"),
        )
        .expect("processor builds");
        let journal = PredictionJournalWriter::create(
            &journal_path,
            session.clone(),
            DEFAULT_PREDICTION_JOURNAL_MAX_BYTES,
        )
        .expect("journal creates");
        let checkpoint = PredictionCheckpointContext {
            path: checkpoint_path.clone(),
            maximum_bytes: DEFAULT_PREDICTION_CHECKPOINT_MAX_BYTES,
            session_id: session.session_id.clone(),
            wall_start: session.wall_start.clone(),
            contract_fingerprint: session.contract_fingerprint.clone(),
            runtime_config,
            publisher_epoch_lineage: vec![EPOCH.to_owned()],
        };
        let pipeline = PredictionPipeline::live(
            Vec::new(),
            processor,
            &session,
            journal,
            checkpoint,
            1_000,
            1_000_000,
        )
        .expect("pipeline builds");

        let (trigger, shutdown) = live_shutdown_channel();
        let mut prediction = shutdown.clone();
        let mut twap = shutdown.clone();
        let mut maintenance = shutdown;
        let prediction_worker = async move {
            prediction
                .wait_reason()
                .await
                .context("prediction shutdown")
        };
        let twap_worker = async move { twap.wait_reason().await.context("TWAP shutdown") };
        let maintenance_worker = async move {
            maintenance
                .wait_reason()
                .await
                .context("maintenance shutdown")
        };
        let coordinator = async move {
            coordinate_live_shutdown_on(
                Instant::now(),
                trigger,
                std::future::pending::<std::result::Result<(), std::io::Error>>(),
            )
            .await
            .context("coordinate test shutdown")
        };

        let (prediction_reason, twap_reason, maintenance_reason, shutdown_reason) =
            join_live_workers(
                prediction_worker,
                twap_worker,
                maintenance_worker,
                coordinator,
            )
            .await
            .expect("reached natural deadline joins cleanly with a pending signal");
        for observed in [
            prediction_reason,
            twap_reason,
            maintenance_reason,
            shutdown_reason,
        ] {
            assert!(
                !observed.interrupted(),
                "a reached natural deadline must publish the natural cause"
            );
        }

        let (pipeline_report, journal_report, _) =
            pipeline.finish_live(0).expect("controlled finalizer runs");
        assert_eq!(pipeline_report.admitted_inputs, 0);
        assert_eq!(journal_report.records_written, 2);

        let mut reader = PredictionJournalReplayReader::open(
            &journal_path,
            DEFAULT_PREDICTION_JOURNAL_MAX_BYTES,
        )
        .expect("completed journal opens");
        assert_eq!(reader.next_input().expect("journal end reads"), None);
        assert!(reader.end().is_some(), "journal must retain its end record");

        std::fs::remove_file(journal_path).expect("journal removes");
        std::fs::remove_file(checkpoint_path).expect("checkpoint removes");
    }

    fn temporary_test_path(label: &str, extension: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock follows epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "prediction-signals-{label}-{}-{unique}.{extension}",
            std::process::id()
        ))
    }

    /// An armed, unbound Kalshi live pipeline over temporary files, so the index
    /// admission handler can be exercised through its real `admit_live` boundary
    /// (E37d). Returns the pipeline plus the three paths to clean up.
    fn armed_kalshi_live_pipeline(
        label: &str,
    ) -> (
        PredictionPipeline<BufWriter<File>>,
        PathBuf,
        PathBuf,
        PathBuf,
    ) {
        const EPOCH: &str = "prediction-index-admission-test";
        let wall_start = "2026-08-15T12:00:00Z";
        let output_path = temporary_test_path(label, "jsonl");
        let journal_path = temporary_test_path(label, "journal.jsonl");
        let checkpoint_path = temporary_test_path(label, "checkpoint.json");
        let runtime_config = PredictionRuntimeConfig::new(PredictionMarketVenue::Kalshi, 2_000)
            .with_index_armed(true);
        let session = PredictionJournalSession::new(
            wall_start.to_owned(),
            EPOCH.to_owned(),
            runtime_config.clone(),
        )
        .expect("journal session builds");
        let processor = PredictionSignalProcessor::new_indexed(
            PredictionMarketVenue::Kalshi,
            EPOCH,
            wall_start.parse().expect("wall start parses"),
            true,
        )
        .expect("armed processor builds");
        let journal = PredictionJournalWriter::create(
            &journal_path,
            session.clone(),
            DEFAULT_PREDICTION_JOURNAL_MAX_BYTES,
        )
        .expect("journal creates");
        let checkpoint = PredictionCheckpointContext {
            path: checkpoint_path.clone(),
            maximum_bytes: DEFAULT_PREDICTION_CHECKPOINT_MAX_BYTES,
            session_id: session.session_id.clone(),
            wall_start: session.wall_start.clone(),
            contract_fingerprint: session.contract_fingerprint.clone(),
            runtime_config,
            publisher_epoch_lineage: vec![EPOCH.to_owned()],
        };
        let output = BufWriter::new(File::create(&output_path).expect("output file creates"));
        let pipeline = PredictionPipeline::live(
            output,
            processor,
            &session,
            journal,
            checkpoint,
            10_000,
            100_000_000,
        )
        .expect("armed pipeline builds");
        (pipeline, output_path, journal_path, checkpoint_path)
    }

    fn auxiliary_index_event() -> SourceEvent {
        SourceEvent {
            schema_version: 1,
            // The Pyth relay is an armed-but-observed-not-consumed index source:
            // auxiliary (takes the current generation) and routed past the
            // processor, so a minimal event admits without market interpretation.
            source_id: btc_feeds::kalshi_index::PYTH_VALUE_SOURCE_ID.to_owned(),
            source_version: "1.0.0".to_owned(),
            publisher_epoch: "prediction-index-admission-test".to_owned(),
            sequence: 0,
            event_time: "2026-08-15T12:00:00Z".to_owned(),
            received_at: "2026-08-15T12:00:00Z".to_owned(),
            values: std::collections::BTreeMap::new(),
        }
    }

    /// Drain a finished prediction journal into the connectivity transport and the
    /// last market-event generation it recorded.
    fn journal_index_facts(journal_path: &Path) -> (Option<PredictionTransport>, Option<u64>) {
        let mut reader =
            PredictionJournalReplayReader::open(journal_path, DEFAULT_PREDICTION_JOURNAL_MAX_BYTES)
                .expect("journal opens");
        let mut connectivity_transport = None;
        let mut market_generation = None;
        while let Some(input) = reader.next_input().expect("journal reads") {
            match input {
                PredictionJournalInput::Connectivity { transport, .. } => {
                    connectivity_transport = Some(transport);
                }
                PredictionJournalInput::MarketEvents { generation, .. } => {
                    market_generation = Some(generation);
                }
                _ => {}
            }
        }
        (connectivity_transport, market_generation)
    }

    /// E37d (MINOR) — a connectivity transition offered by the index socket is
    /// admitted on the dedicated `KalshiIndex` transport, never the market
    /// transport, so a market-socket blip and an index-socket blip stay
    /// distinguishable in feed health.
    #[tokio::test(flavor = "current_thread")]
    async fn index_connectivity_admission_rides_the_kalshi_index_transport() {
        let (pipeline, output_path, journal_path, checkpoint_path) =
            armed_kalshi_live_pipeline("index-connectivity");
        let now_ns = 1_000_000_000_u64;
        let pipeline = Rc::new(RefCell::new(pipeline));
        admit_prediction_index(
            &pipeline,
            PredictionLiveAdmission::Connectivity {
                transition: PredictionSocketTransition::Disconnected,
                now_ns,
            },
        )
        .expect("a connectivity transition admits on the index transport");
        let mut pipeline = Rc::try_unwrap(pipeline)
            .map_err(|_| ())
            .expect("no other owner holds the pipeline")
            .into_inner();
        pipeline.sync_output().expect("output syncs");
        pipeline
            .finish_live(now_ns)
            .expect("controlled finalizer runs");

        let (connectivity_transport, _) = journal_index_facts(&journal_path);
        assert_eq!(
            connectivity_transport,
            Some(PredictionTransport::KalshiIndex),
            "the index connectivity transition rides the KalshiIndex transport"
        );

        for path in [output_path, journal_path, checkpoint_path] {
            let _ = std::fs::remove_file(path);
        }
    }

    /// E37d (MINOR) — an index frame received while the lane is unbound takes the
    /// generation the pipeline reserves next (the new-generation case), NOT a bound
    /// market generation and NOT the placeholder the socket runner passes. A
    /// regression stamping `active_market_generation` fails closed here (unbound →
    /// no active generation), and a regression trusting the placeholder is caught by
    /// the `assert_ne!`.
    #[tokio::test(flavor = "current_thread")]
    async fn unbound_index_frame_takes_the_reserved_auxiliary_generation() {
        let (pipeline, output_path, journal_path, checkpoint_path) =
            armed_kalshi_live_pipeline("index-generation");
        // Unbound: `current_generation_for_auxiliary_source` reserves the next
        // (new) generation, distinct from the placeholder the runner hands over.
        let reserved = pipeline
            .current_generation_for_auxiliary_source()
            .expect("an unbound lane reserves the next generation");
        let placeholder = reserved.wrapping_add(999);
        let now_ns = 1_000_000_000_u64;

        let pipeline = Rc::new(RefCell::new(pipeline));
        admit_prediction_index(
            &pipeline,
            PredictionLiveAdmission::MarketOutcome {
                generation: placeholder,
                outcome: btc_feeds::IngestOutcome {
                    events: vec![auxiliary_index_event()],
                    notices: Vec::new(),
                },
                logical_receipt_ns: now_ns,
                now_ns,
            },
        )
        .expect("an unbound index frame admits under the reserved generation");
        let mut pipeline = Rc::try_unwrap(pipeline)
            .map_err(|_| ())
            .expect("no other owner holds the pipeline")
            .into_inner();
        pipeline.sync_output().expect("output syncs");
        pipeline
            .finish_live(now_ns)
            .expect("controlled finalizer runs");

        let (_, market_generation) = journal_index_facts(&journal_path);
        assert_eq!(
            market_generation,
            Some(reserved),
            "an unbound index frame takes the reserved auxiliary generation"
        );
        assert_ne!(
            market_generation,
            Some(placeholder),
            "the placeholder generation from the socket runner is ignored"
        );

        for path in [output_path, journal_path, checkpoint_path] {
            let _ = std::fs::remove_file(path);
        }
    }

    #[test]
    fn multi_run_rejects_invalid_durations_and_intervals() {
        // The multi lane validates the duration and checkpoint interval up front
        // (E57); no resume is ever refused (rotation-aware resume re-warms honestly
        // instead of exiting 78). Tested through the sync validator so the giant
        // `run_multi` orchestration future is never instantiated on the stack.
        let zero = validate_multi_run_arguments(Duration::from_secs(0), 5)
            .expect_err("a zero duration is rejected");
        assert!(
            format!("{zero}").contains("duration_seconds"),
            "the message names the duration bound: {zero}",
        );
        let interval = validate_multi_run_arguments(Duration::from_secs(10), 10)
            .expect_err("a checkpoint interval not shorter than the run is rejected");
        assert!(
            format!("{interval}").contains("checkpoint interval"),
            "the message names the checkpoint interval: {interval}",
        );
        assert!(
            validate_multi_run_arguments(Duration::from_secs(900), 5).is_ok(),
            "a valid duration and interval are admitted",
        );
    }

    fn build_multi_rotation_fixture(
        lane_root: &std::path::Path,
        epoch: &str,
        clock: &ProcessClock,
        first_open_ns: u64,
    ) -> (
        SharedMultiRuntime,
        SharedJournal,
        Rc<RefCell<BufWriter<File>>>,
        MultiRotationRuntime,
    ) {
        std::fs::create_dir_all(lane_root).expect("lane root");
        let caps = FileGenerationCaps {
            max_updates: 10_000,
            max_output_bytes: 8 * 1024 * 1024,
            max_journal_bytes: DEFAULT_PREDICTION_JOURNAL_MAX_BYTES,
        };
        let (now_unix, started) = wall_clock_stamp(clock, first_open_ns).expect("stamp");
        let layout =
            FileGenerationLayout::new(lane_root, "multi".to_owned()).expect("layout binds");
        let meta = rotation_run_meta(
            "multi",
            epoch,
            &format!("gen-live-multi-{now_unix}"),
            &started,
            60,
            caps,
            2,
            false,
        );
        let open = layout
            .open_next_with_rotation(now_unix, None, &meta, 60)
            .expect("open gen0");
        let schedule =
            RotationSchedule::try_new(first_open_ns, Duration::from_secs(60)).expect("schedule");
        let wall_start = clock.wall_start_text();
        // Build gen 0 the same way the live path does, so the fixture exercises
        // the real header construction (base process epoch as each leg's
        // admission epoch) and the rotation carries that into the successor.
        let header = multi_generation_zero_headers(epoch, &wall_start);
        let output = private_output(&open.output_path).expect("gen0 output");
        let shared = Rc::new(RefCell::new(BufWriter::with_capacity(1024 * 1024, output)));
        let runtime: SharedMultiRuntime = Rc::new(RefCell::new(
            runtime_from_header(
                Rc::clone(&shared),
                &header,
                caps.max_updates,
                caps.max_output_bytes,
            )
            .expect("runtime"),
        ));
        let journal_file = private_output(&open.journal_path).expect("gen0 journal");
        let journal: SharedJournal = Rc::new(RefCell::new(MergedJournalWriter::new(
            BufWriter::new(journal_file),
            caps.max_journal_bytes,
        )));
        journal
            .borrow_mut()
            .write_header(&header)
            .expect("gen0 header");
        let checkpoint_path = lane_root.join("checkpoint.json");
        write_multi_checkpoint(
            &runtime,
            &checkpoint_path,
            DEFAULT_PREDICTION_CHECKPOINT_MAX_BYTES,
        )
        .expect("initial checkpoint");
        let rotation = MultiRotationRuntime {
            layout,
            schedule,
            rotate_seconds: 60,
            headroom_factor: 2,
            caps,
            epoch_prefix: epoch.to_owned(),
            max_journal_bytes: caps.max_journal_bytes,
            current: open,
        };
        (runtime, journal, shared, rotation)
    }

    /// Generation 0's merged-journal header records the base process epoch as
    /// BOTH legs' admission epoch (E64/C5) while keeping the per-leg derived
    /// output epoch on `publisher_epoch`. Reverting either `admission_epoch` in
    /// `multi_generation_zero_headers` back to `None` fails this test.
    #[test]
    fn multi_generation_zero_headers_stamp_the_process_epoch_as_both_admission_epochs() {
        let process_epoch = "live-multi-epoch";
        let header = multi_generation_zero_headers(process_epoch, "2026-09-05T00:00:00.000000000Z");

        assert_eq!(header.publisher_epoch, process_epoch);
        assert_eq!(header.kalshi.publisher_epoch, "live-multi-epoch.kalshi");
        assert_eq!(
            header.polymarket.publisher_epoch,
            "live-multi-epoch.polymarket"
        );
        assert_eq!(
            header.kalshi.admission_epoch.as_deref(),
            Some(process_epoch),
            "the Kalshi leg admits against the base process epoch, not the derived output epoch",
        );
        assert_eq!(
            header.polymarket.admission_epoch.as_deref(),
            Some(process_epoch),
            "the Polymarket leg admits against the base process epoch, not the derived output epoch",
        );
    }

    /// A still-running multi feed is handed the base process epoch to stamp on
    /// admitted events, never the per-leg derived OUTPUT epoch. Reverting
    /// `multi_feed_publisher_epoch` to return the derived epoch fails this test,
    /// mirroring the live-run admission mismatch the fix prevents.
    #[test]
    fn multi_feed_publisher_epoch_hands_the_process_epoch_not_the_derived() {
        let process_epoch = "live-multi-epoch";
        assert_eq!(
            multi_feed_publisher_epoch(process_epoch, "live-multi-epoch.kalshi"),
            process_epoch,
            "the Kalshi feed stamps the base process epoch",
        );
        assert_eq!(
            multi_feed_publisher_epoch(process_epoch, "live-multi-epoch.polymarket"),
            process_epoch,
            "the Polymarket feed stamps the base process epoch",
        );
    }

    /// A multi-lane rotation seals the departed generation, opens the successor,
    /// and writes the successor's two-epoch merged-journal header — the derived
    /// output epochs plus the base process admission epochs (C5/E57).
    #[test]
    fn rotate_multi_generation_seals_the_departed_and_opens_the_successor_with_two_epochs() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let lane_root = std::env::temp_dir().join(format!("multi-rot-{unique}"));
        let epoch = "live-multi-epoch";
        let clock = ProcessClock::start();
        let first_open_ns = clock.elapsed_ns().expect("first open time");
        let (runtime, journal, shared, mut rotation) =
            build_multi_rotation_fixture(&lane_root, epoch, &clock, first_open_ns);
        let departed_dir = rotation.current.dir.clone();
        let checkpoint_path = lane_root.join("checkpoint.json");

        let now_ns = first_open_ns + 61 * 1_000_000_000;
        rotate_multi_generation(
            &runtime,
            &journal,
            &shared,
            &checkpoint_path,
            DEFAULT_PREDICTION_CHECKPOINT_MAX_BYTES,
            &clock,
            now_ns,
            &mut rotation,
        )
        .expect("rotation past the first period succeeds");
        journal
            .borrow_mut()
            .flush()
            .expect("flush successor journal");

        let gen1_name = rotation
            .current
            .dir
            .file_name()
            .and_then(|name| name.to_str())
            .expect("gen1 dir name")
            .to_owned();
        let name_unix = gen1_name
            .strip_prefix("gen-live-multi-")
            .expect("gen1 dir carries the multi-stamped suffix");
        let derived = format!("{epoch}-{name_unix}");

        // The successor RUN-META carries the derived output epoch.
        let run_meta = std::fs::read_to_string(
            rotation
                .current
                .dir
                .join(crate::file_generation::RUN_META_FILE_NAME),
        )
        .expect("read gen1 RUN-META");
        assert!(
            run_meta.contains(&format!("\"publisher_epoch\": \"{derived}\"")),
            "successor RUN-META carries the derived output epoch {derived}: {run_meta}"
        );

        // The successor merged-journal header carries both epochs per venue.
        let journal_text =
            std::fs::read_to_string(&rotation.current.journal_path).expect("read gen1 journal");
        let header_line = journal_text.lines().next().expect("successor header line");
        let header: MultiJournalHeader =
            serde_json::from_str(header_line).expect("successor header decodes");
        assert_eq!(header.kalshi.publisher_epoch, format!("{derived}.kalshi"));
        assert_eq!(
            header.kalshi.admission_epoch.as_deref(),
            Some(epoch),
            "the successor header carries the base process admission epoch, not the derived output epoch",
        );
        assert_eq!(
            header.polymarket.admission_epoch.as_deref(),
            Some(epoch),
            "both legs carry the base process admission epoch after rotation",
        );

        // The departed generation's RUN-META is sealed and the checkpoint persists.
        let departed_meta = std::fs::read_to_string(
            departed_dir.join(crate::file_generation::RUN_META_FILE_NAME),
        )
        .expect("read departed RUN-META");
        assert!(
            departed_meta.contains("sealed_utc") && !departed_meta.contains("\"sealed_utc\": null"),
            "the departed generation is sealed: {departed_meta}"
        );
        assert!(checkpoint_path.exists(), "the rotation sealed a checkpoint");

        let _ = std::fs::remove_dir_all(&lane_root);
    }
}
