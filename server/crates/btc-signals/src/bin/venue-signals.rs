//! `venue-signals` — one binary, one lane shell, pluggable processors (A6).
//!
//! Runs any venue lane through the generic shell: `--lane binance|bybit|oracle|cross`.
//! Spot stays on `btc-signals` for this release and migrates onto this binary in
//! wave E (ruling E26). The prediction lanes keep their own `prediction-signals`
//! binary (their discovery/roll machinery is a different shell).

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result};
use btc_feeds::live::ChainlinkPollOptions;
use btc_signals::exit_code::{ExitClass, exit_class_for};
use btc_signals::lane::{self, ChainlinkPollDriver, LaneDriverSpec, LaneSpec, PollDriver};
use btc_signals::lane_checkpoint::DEFAULT_LANE_CHECKPOINT_MAX_BYTES;
use btc_signals::lane_gate::{IndexSocket, admit_lane};
use btc_signals::lane_journal::DEFAULT_LANE_JOURNAL_MAX_BYTES;
use btc_signals::lane_runtime::{
    LaneIndexGating, LaneRotation, LaneRunArguments, replay_lane, run_lane,
};
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};

const DEFAULT_MAX_UPDATES: u64 = 10_000_000;
const DEFAULT_MAX_OUTPUT_BYTES: u64 = 2_147_483_648;
const DEFAULT_CHECKPOINT_INTERVAL_SECONDS: u64 = 5;
/// The smallest `--rotate-seconds` the CLI accepts (the box runs hour-long
/// generations; a short period is almost always an operator mistake).
const MIN_ROTATE_SECONDS: u64 = 60;
/// The upper run bound; rotation mode with no `--duration-seconds` runs to it (≈
/// "until SIGTERM"). Mirrors `lane_runtime`'s validation ceiling.
const MAX_RUN_SECONDS: u64 = 7 * 24 * 60 * 60;
/// The headroom factor recorded in each generation's `RUN-META.json`.
const DEFAULT_HEADROOM_FACTOR: u64 = 2;
/// The shared checkpoint file name under a lane root in rotation mode.
const CHECKPOINT_FILE_NAME: &str = "checkpoint.json";

#[derive(Debug, Parser)]
#[command(
    name = "venue-signals",
    about = "Run any BTC venue lane through one shell with a pluggable processor"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Subscribe a lane's feeds and publish its stream-v3 output live.
    Run {
        #[arg(long, value_enum)]
        lane: CliLane,
        /// Explicit-mode output path. Mutually exclusive with `--lane-root`/`--rotate-seconds`.
        #[arg(long)]
        output: Option<PathBuf>,
        /// Explicit-mode journal path. Mutually exclusive with `--lane-root`/`--rotate-seconds`.
        #[arg(long)]
        journal: Option<PathBuf>,
        /// Checkpoint path. Required in explicit mode; defaults to
        /// `<lane-root>/checkpoint.json` in rotation mode.
        #[arg(long)]
        checkpoint: Option<PathBuf>,
        /// Rotation mode: the lane root under which `gen-live-<lane>-<unix>`
        /// generations are created. Requires `--rotate-seconds`; mutually exclusive
        /// with `--output`/`--journal`.
        #[arg(long)]
        lane_root: Option<PathBuf>,
        /// Rotation mode: the rotation period in seconds (≥ 60). Requires
        /// `--lane-root`; mutually exclusive with `--output`/`--journal`.
        #[arg(long)]
        rotate_seconds: Option<u64>,
        #[arg(long)]
        resume_from_checkpoint: bool,
        #[arg(long)]
        publisher_epoch: String,
        /// Overrides the lane's default duration when set.
        #[arg(long)]
        duration_seconds: Option<u64>,
        #[arg(long, default_value_t = DEFAULT_CHECKPOINT_INTERVAL_SECONDS)]
        checkpoint_interval_seconds: u64,
        #[arg(long, default_value_t = DEFAULT_MAX_UPDATES)]
        max_updates: u64,
        #[arg(long, default_value_t = DEFAULT_MAX_OUTPUT_BYTES)]
        max_output_bytes: u64,
        #[arg(long, default_value_t = DEFAULT_LANE_JOURNAL_MAX_BYTES)]
        max_journal_bytes: u64,
        #[arg(long, default_value_t = DEFAULT_LANE_CHECKPOINT_MAX_BYTES)]
        max_checkpoint_bytes: u64,
        /// Rotation mode: the headroom factor recorded in each generation's
        /// `RUN-META.json`, matching live-run.sh's `HEADROOM_FACTOR` (parity with
        /// `spot`/`prediction`). RUN-META provenance only — the binary does no cap
        /// arithmetic from it. Ignored in explicit mode.
        #[arg(long, default_value_t = DEFAULT_HEADROOM_FACTOR)]
        headroom_factor: u64,
    },
    /// Deterministically rebuild a lane's stream from its retained journal.
    Replay {
        #[arg(long, value_enum)]
        lane: CliLane,
        #[arg(long)]
        journal: PathBuf,
        #[arg(long)]
        output: PathBuf,
        #[arg(long, default_value_t = DEFAULT_MAX_UPDATES)]
        max_updates: u64,
        #[arg(long, default_value_t = DEFAULT_MAX_OUTPUT_BYTES)]
        max_output_bytes: u64,
        #[arg(long, default_value_t = DEFAULT_LANE_JOURNAL_MAX_BYTES)]
        max_journal_bytes: u64,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliLane {
    Binance,
    Bybit,
    Oracle,
    Cross,
}

impl CliLane {
    fn spec(self) -> &'static LaneSpec {
        match self {
            Self::Binance => &lane::BINANCE,
            Self::Bybit => &lane::BYBIT,
            Self::Oracle => &lane::ORACLE,
            Self::Cross => &lane::CROSS,
        }
    }

    /// The poll driver for a poll-driven lane. The oracle lane drives A5's
    /// Chainlink on-chain transport ([`ChainlinkPollDriver`]) behind the
    /// [`PollDriver`] seam, over the configured E29 RPC endpoint list (defaulting
    /// to [`btc_feeds::chainlink::DEFAULT_RPC_ENDPOINTS`] when unset). Tests drive
    /// the same seam with `FixturePollDriver` or `ChainlinkPollDriver::with_transport`.
    fn poll_driver(
        self,
        duration: Duration,
        publisher_epoch: &str,
    ) -> Result<Option<Box<dyn PollDriver>>> {
        match self.spec().driver {
            // The oracle lane is `Poll` (interim) or `Composite` (its Chainlink
            // poll plus the Kalshi index and Pyth Hermes streams, C1). Both drive
            // the same Chainlink poll behind the `PollDriver` seam; the composite
            // lane additionally drives its streams through the stream runner.
            LaneDriverSpec::Poll { label, .. } | LaneDriverSpec::Composite { label, .. } => {
                let endpoints = btc_feeds::chainlink::configured_rpc_endpoints()
                    .map_err(|reason| anyhow::anyhow!("chainlink RPC endpoints: {reason}"))?;
                let options = ChainlinkPollOptions {
                    endpoints,
                    duration,
                    publisher_epoch: publisher_epoch.to_owned(),
                    shutdown: None,
                };
                let driver = ChainlinkPollDriver::reqwest(label, options)
                    .context("build the Chainlink oracle poll driver")?;
                Ok(Some(Box::new(driver)))
            }
            LaneDriverSpec::Streams(_) => Ok(None),
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
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

async fn run_command() -> Result<ExitClass> {
    match Cli::parse().command {
        Command::Run {
            lane,
            output,
            journal,
            checkpoint,
            lane_root,
            rotate_seconds,
            resume_from_checkpoint,
            publisher_epoch,
            duration_seconds,
            checkpoint_interval_seconds,
            max_updates,
            max_output_bytes,
            max_journal_bytes,
            max_checkpoint_bytes,
            headroom_factor,
        } => {
            let spec = lane.spec();
            // Resolve explicit vs rotation mode and the three paths. A CLI misuse
            // (both modes, an incomplete rotation pair, or `--rotate-seconds` below
            // the floor) exits with clap's usage class before any lane work.
            let resolved = resolve_run_mode(RunModeInputs {
                output,
                journal,
                checkpoint,
                lane_root,
                rotate_seconds,
                headroom_factor,
            });
            // Every venue lane is gated on its allow-list requirement (R7). The
            // oracle lane's requirement is empty, so it always admits (R7 does
            // not apply). The composite oracle lane opens index sockets, so it
            // declares them `Configured` and `admit_lane` reports the armed
            // families truthfully from `ORACLE_LANE.index_sockets` (venue_gate.rs)
            // rather than the ad-hoc line this replaces (oracle-composite audit).
            let composite = matches!(spec.driver, LaneDriverSpec::Composite { .. });
            let socket = if composite {
                IndexSocket::Configured
            } else {
                IndexSocket::NotConfigured
            };
            let admission = match admit_lane(spec.requirement, socket) {
                Ok(admission) => admission,
                Err(class) => return Ok(class),
            };
            eprintln!("{}", admission.startup_line());
            // The composite lane resolves the full arming for the run (the
            // allow-list plus the Pyth and Kalshi credential presence). Whether an
            // armed Kalshi index socket actually opens also depends on the Kalshi
            // handshake credentials; the lane runtime reports that decision
            // fail-soft when it drives the streams. A non-composite lane drives no
            // index sockets, so it needs no gating.
            let index_gating = composite.then(LaneIndexGating::from_env);
            // Rotation mode with no `--duration-seconds` runs until SIGTERM, bounded
            // only by the runtime's maximum; explicit mode keeps the lane default.
            let run_duration = duration_seconds.unwrap_or(if resolved.rotation.is_some() {
                MAX_RUN_SECONDS
            } else {
                spec.duration_default
            });
            let poll_driver =
                lane.poll_driver(Duration::from_secs(run_duration), &publisher_epoch)?;
            let (report, shutdown_reason) = run_lane(LaneRunArguments {
                spec,
                output: resolved.output,
                journal: resolved.journal,
                checkpoint: resolved.checkpoint,
                resume_from_checkpoint,
                publisher_epoch,
                duration_seconds: run_duration,
                checkpoint_interval_seconds,
                max_updates,
                max_output_bytes,
                max_journal_bytes,
                max_checkpoint_bytes,
                poll_driver,
                stream_runner: None,
                index_gating,
                rotation: resolved.rotation,
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
            lane,
            journal,
            output,
            max_updates,
            max_output_bytes,
            max_journal_bytes,
        } => {
            let report = replay_lane(
                lane.spec(),
                &journal,
                &output,
                max_updates,
                max_output_bytes,
                max_journal_bytes,
            )
            .context("replay lane journal")?;
            println!("{}", serde_json::to_string(&report)?);
            Ok(ExitClass::Success)
        }
    }
}

/// The `Run` CLI arguments that select explicit vs rotation mode.
struct RunModeInputs {
    output: Option<PathBuf>,
    journal: Option<PathBuf>,
    checkpoint: Option<PathBuf>,
    lane_root: Option<PathBuf>,
    rotate_seconds: Option<u64>,
    headroom_factor: u64,
}

/// The resolved paths and rotation config handed to [`LaneRunArguments`].
struct ResolvedRunMode {
    output: PathBuf,
    journal: PathBuf,
    checkpoint: PathBuf,
    rotation: Option<LaneRotation>,
}

/// Resolve explicit vs rotation mode, enforcing their mutual exclusion and the
/// `--rotate-seconds` floor. Explicit mode (`--output`/`--journal`, unchanged) and
/// rotation mode (`--lane-root`/`--rotate-seconds`) are mutually exclusive; a
/// misuse exits with clap's usage class (never proceeding to a partial run). In
/// rotation mode the two files come from the file-generation layout, so
/// [`ResolvedRunMode::output`]/`journal` are left empty and the checkpoint defaults
/// to `<lane-root>/checkpoint.json` when `--checkpoint` is omitted.
fn resolve_run_mode(inputs: RunModeInputs) -> ResolvedRunMode {
    let rotation_selected = inputs.lane_root.is_some() || inputs.rotate_seconds.is_some();
    let explicit_selected = inputs.output.is_some() || inputs.journal.is_some();
    if rotation_selected && explicit_selected {
        usage_error(
            "--lane-root/--rotate-seconds (rotation mode) and --output/--journal (explicit mode) are mutually exclusive",
        );
    }
    if rotation_selected {
        let (Some(lane_root), Some(rotate_seconds)) = (inputs.lane_root, inputs.rotate_seconds)
        else {
            usage_error("rotation mode requires both --lane-root and --rotate-seconds");
        };
        if rotate_seconds < MIN_ROTATE_SECONDS {
            usage_error(&format!(
                "--rotate-seconds must be at least {MIN_ROTATE_SECONDS}"
            ));
        }
        let checkpoint = inputs
            .checkpoint
            .unwrap_or_else(|| lane_root.join(CHECKPOINT_FILE_NAME));
        ResolvedRunMode {
            output: PathBuf::new(),
            journal: PathBuf::new(),
            checkpoint,
            rotation: Some(LaneRotation {
                lane_root,
                rotate_seconds,
                headroom_factor: inputs.headroom_factor,
            }),
        }
    } else {
        let (Some(output), Some(journal), Some(checkpoint)) =
            (inputs.output, inputs.journal, inputs.checkpoint)
        else {
            usage_error(
                "explicit mode requires --output, --journal and --checkpoint (or use --lane-root/--rotate-seconds for rotation mode)",
            );
        };
        ResolvedRunMode {
            output,
            journal,
            checkpoint,
            rotation: None,
        }
    }
}

/// Emit a clap usage error (exit code 2, the CLI usage class) and terminate.
fn usage_error(message: &str) -> ! {
    Cli::command()
        .error(clap::error::ErrorKind::ArgumentConflict, message)
        .exit()
}
