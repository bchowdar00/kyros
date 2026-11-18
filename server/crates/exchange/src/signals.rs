//! In-process supervisor for the BTC signal producers.
//!
//! The signals prototype ships three producer families as library code:
//! the **spot** lane (`btc_signals::spot_run`), the **venue** lanes
//! (Binance, Bybit, Chainlink oracle, cross; `btc_signals::lane_runtime`)
//! and the **prediction** lanes (Kalshi, Polymarket;
//! `btc_signals::prediction_run`). Their binaries are thin wrappers, and
//! this module is the exchange's equivalent wrapper: it decides which
//! lanes to run from the environment, then runs every one of them
//! concurrently alongside the exchange.
//!
//! # Threading
//!
//! Every lane runs on its own OS thread with its own single-threaded
//! Tokio runtime, exactly the way the standalone binaries run
//! (`#[tokio::main(flavor = "current_thread")]`). That gives real
//! parallelism across lanes and between the lanes and the exchange, keeps
//! a lane's blocking file I/O off the exchange's runtime, and isolates a
//! lane panic to its own thread. The exchange's own runtime never awaits a
//! lane future.
//!
//! # Lane selection
//!
//! `SIGNALS_ENABLED` (default on) gates the whole thing. Which lanes start
//! follows the prototype's allow-list `SIGNALS_ENABLED_VENUES` (default
//! `coinbase,kalshi,kraken,polymarket`) plus credential presence:
//!
//! | lane        | runs when                                              |
//! |-------------|--------------------------------------------------------|
//! | spot        | `coinbase` and `kraken` enabled (no credentials needed) |
//! | polymarket  | `polymarket` enabled (no credentials needed)            |
//! | kalshi      | `kalshi` enabled and `KALSHI_API_KEY_ID` +              |
//! |             | `KALSHI_PRIVATE_KEY_PATH` present                       |
//! | binance     | `binance` enabled                                       |
//! | bybit       | `bybit` enabled                                         |
//! | oracle      | `oracle` enabled (Chainlink RPC, public defaults)       |
//! | cross       | `bybit`, `coinbase` and `kraken` enabled                |
//!
//! A lane whose admission the prototype refuses is skipped with a log
//! line; it never stops the exchange from starting.
//!
//! # Output
//!
//! Every lane runs in the prototype's rotation mode under
//! `SIGNALS_LANE_ROOT/<lane>/` (default `./signals-data`): hour-long
//! `gen-live-<lane>-<unix>` generations of `output.jsonl`, `journal.jsonl`
//! and `RUN-META.json`, with a `CURRENT` pointer to the newest. Those files
//! are the durable record (journals, checkpoints, replay); the exchange does
//! not read them live.
//!
//! The live path is in-process: every lane runs with a stream hook
//! (`btc_signals::stream_accounting::StreamHook`) that is called with each
//! stream message the lane writes, before the bytes reach the file. The
//! hook does one thing, it sends the message as a [`LaneMessage`] on the
//! unbounded channel [`spawn_signal_producers`] creates before any lane
//! starts, so the lane thread never blocks and never takes an exchange
//! lock. `SignalProducers::take_messages` hands the receiver to the signal
//! bridge (`crate::automation::lane_bridge`), which types each update
//! against the registry and feeds the signals platform. Messages that
//! arrive before the bridge attaches simply wait in the channel.
//!
//! # Shutdown
//!
//! The lanes listen for SIGINT/SIGTERM themselves and seal their open
//! generation when one arrives. Because installing those listeners
//! replaces the default "die on Ctrl-C" behaviour for the whole process,
//! [`install_shutdown`] restores it: on Ctrl-C it gives the lanes a short
//! grace period to seal, then exits the process.

use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use btc_signals::stream_accounting::StreamHook;
use signals_core::SignalStreamMessage;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use btc_feeds::live::ChainlinkPollOptions;
use btc_signals::checkpoint::{DEFAULT_CHECKPOINT_MAX_BYTES, DEFAULT_CHECKPOINT_QUEUE_CAPACITY};
use btc_signals::journal::DEFAULT_JOURNAL_QUEUE_CAPACITY;
use btc_signals::lane::{self, ChainlinkPollDriver, LaneDriverSpec, LaneSpec, PollDriver};
use btc_signals::lane_checkpoint::DEFAULT_LANE_CHECKPOINT_MAX_BYTES;
use btc_signals::lane_gate::{IndexSocket, admit_lane};
use btc_signals::lane_journal::DEFAULT_LANE_JOURNAL_MAX_BYTES;
use btc_signals::lane_runtime::{LaneIndexGating, LaneRotation, LaneRunArguments, run_lane_with_hook};
use btc_signals::prediction_checkpoint::DEFAULT_PREDICTION_CHECKPOINT_MAX_BYTES;
use btc_signals::prediction_journal::DEFAULT_PREDICTION_JOURNAL_MAX_BYTES;
use btc_signals::prediction_run::{RotationConfig, RunArguments, run_with_hook as run_prediction};
use btc_signals::spot_run::{SpotRunArgs, run_spot_signals_with_hook};
use signals_core::{EnabledVenues, KALSHI_LANE, POLYMARKET_LANE, PredictionMarketVenue, SPOT_LANE};

/// Master switch. Anything other than `0`, `false`, `no` or `off` (case
/// insensitive) means on. Default on.
pub const ENABLED_ENV: &str = "SIGNALS_ENABLED";
/// Directory the per-lane generation trees live under.
pub const LANE_ROOT_ENV: &str = "SIGNALS_LANE_ROOT";
/// Rotation period for every lane's file generations, in seconds (≥ 60).
pub const ROTATE_SECONDS_ENV: &str = "SIGNALS_ROTATE_SECONDS";

const DEFAULT_LANE_ROOT: &str = "signals-data";
const DEFAULT_ROTATE_SECONDS: u64 = 3600;
const MIN_ROTATE_SECONDS: u64 = 60;
/// Recorded in each generation's `RUN-META.json`; provenance only.
const HEADROOM_FACTOR: u64 = 2;
/// The prototype's upper run bound. A lane in rotation mode runs to it
/// and the supervisor simply starts it again.
const MAX_RUN_SECONDS: u64 = 7 * 24 * 60 * 60;
const CHECKPOINT_INTERVAL_SECONDS: u64 = 5;
const MAX_UPDATES: u64 = 10_000_000;
const MAX_OUTPUT_BYTES: u64 = 2_147_483_648;
const SPOT_MAX_JOURNAL_BYTES: u64 = 4_294_967_296;
const PREDICTION_DISCOVERY_RETRY_MS: u64 = 2_000;
/// Restart back-off after a lane run fails: first retry, and the ceiling.
const RESTART_MIN: Duration = Duration::from_secs(5);
const RESTART_MAX: Duration = Duration::from_secs(120);
/// How long Ctrl-C waits for lanes to seal their generations before the
/// process exits regardless.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(8);
/// Stack for each lane thread. The runners' futures are deep and wide
/// (the spot runner overflows a default 2 MB thread stack before its
/// first tick); the binaries get the main thread's 8 MB. This is reserved
/// address space, not resident memory.
const LANE_THREAD_STACK: usize = 64 * 1024 * 1024;

/// One lane the supervisor may run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lane {
    Spot,
    Polymarket,
    Kalshi,
    Binance,
    Bybit,
    Oracle,
    Cross,
}

impl Lane {
    const ALL: [Lane; 7] = [
        Lane::Spot,
        Lane::Polymarket,
        Lane::Kalshi,
        Lane::Binance,
        Lane::Bybit,
        Lane::Oracle,
        Lane::Cross,
    ];

    fn name(self) -> &'static str {
        match self {
            Lane::Spot => "spot",
            Lane::Polymarket => "polymarket",
            Lane::Kalshi => "kalshi",
            Lane::Binance => "binance",
            Lane::Bybit => "bybit",
            Lane::Oracle => "oracle",
            Lane::Cross => "cross",
        }
    }

    fn venue_spec(self) -> Option<&'static LaneSpec> {
        match self {
            Lane::Binance => Some(&lane::BINANCE),
            Lane::Bybit => Some(&lane::BYBIT),
            Lane::Oracle => Some(&lane::ORACLE),
            Lane::Cross => Some(&lane::CROSS),
            Lane::Spot | Lane::Polymarket | Lane::Kalshi => None,
        }
    }

    /// Whether the allow-list and credentials let this lane run. Returns
    /// the reason it cannot, for the log.
    fn eligibility(self, enabled: &EnabledVenues) -> Result<(), String> {
        let need = |tokens: &[&str]| -> Result<(), String> {
            let missing: Vec<&str> = tokens
                .iter()
                .copied()
                .filter(|t| !enabled.is_enabled(t))
                .collect();
            if missing.is_empty() {
                Ok(())
            } else {
                Err(format!(
                    "{} not in {}",
                    missing.join(", "),
                    signals_core::VENUE_GATE_ENV
                ))
            }
        };
        match self {
            Lane::Spot => need(SPOT_LANE.required),
            Lane::Polymarket => need(POLYMARKET_LANE.required),
            Lane::Kalshi => {
                need(KALSHI_LANE.required)?;
                btc_feeds::KalshiPassthroughClient::from_env()
                    .map(|_| ())
                    .map_err(|e| format!("Kalshi credentials unavailable: {e}"))
            }
            Lane::Binance | Lane::Bybit | Lane::Cross => {
                need(self.venue_spec().expect("venue lane").requirement.required)
            }
            // The oracle lane's requirement is empty (it always admits), so
            // it is opted in explicitly with its own token to keep the
            // default quiet.
            Lane::Oracle => need(&["oracle"]),
        }
    }
}

/// Supervisor configuration, read once from the environment.
#[derive(Debug, Clone)]
struct Config {
    lane_root: PathBuf,
    rotate_seconds: u64,
}

impl Config {
    fn from_env() -> Self {
        let lane_root = std::env::var(LANE_ROOT_ENV)
            .ok()
            .filter(|v| !v.trim().is_empty())
            .map_or_else(|| PathBuf::from(DEFAULT_LANE_ROOT), PathBuf::from);
        let rotate_seconds = std::env::var(ROTATE_SECONDS_ENV)
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(DEFAULT_ROTATE_SECONDS)
            .max(MIN_ROTATE_SECONDS);
        Self {
            lane_root,
            rotate_seconds,
        }
    }
}

/// The lane root directory the supervisor writes under (`SIGNALS_LANE_ROOT`,
/// default `./signals-data`).
pub fn lane_root() -> PathBuf {
    Config::from_env().lane_root
}

/// One stream message a lane wrote, as delivered in-process.
#[derive(Debug, Clone)]
pub struct LaneMessage {
    /// The supervisor's lane name (`spot`, `binance`, `kalshi`, …).
    pub lane: &'static str,
    pub message: SignalStreamMessage,
}

/// The in-process stream hook for one lane: clone the message and send it.
/// Nothing else happens on the lane thread. A send fails only once the
/// receiver is gone, which is process shutdown, so the result is dropped.
fn lane_stream_hook(lane: &'static str, tx: UnboundedSender<LaneMessage>) -> StreamHook {
    Arc::new(move |message: &SignalStreamMessage| {
        let _ = tx.send(LaneMessage { lane, message: message.clone() });
    })
}

/// Handle to the running supervisor: the lanes it started, the in-process
/// message stream, and one receiver that yields a lane name each time a
/// lane thread exits for good.
pub struct SignalProducers {
    lanes: Vec<&'static str>,
    messages: Option<UnboundedReceiver<LaneMessage>>,
    exited: mpsc::Receiver<&'static str>,
}

impl SignalProducers {
    /// Names of the lanes that were started.
    pub fn lanes(&self) -> &[&'static str] {
        &self.lanes
    }

    /// The in-process stream of every lane's messages. `None` after the
    /// first call; the bridge owns it from then on.
    pub fn take_messages(&mut self) -> Option<UnboundedReceiver<LaneMessage>> {
        self.messages.take()
    }
}

fn enabled_by_env() -> bool {
    match std::env::var(ENABLED_ENV) {
        Ok(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        ),
        Err(_) => true,
    }
}

/// Start every eligible lane on its own thread. Never fails: a lane that
/// cannot start is logged and skipped. Returns `None` when the producers
/// are disabled or nothing is eligible.
pub fn spawn_signal_producers() -> Option<SignalProducers> {
    if !enabled_by_env() {
        tracing::info!("signals: disabled by {ENABLED_ENV}");
        return None;
    }
    // The lanes' TLS stack is rustls with the ring provider, and so is the
    // exchange's own (tokio-tungstenite, the Pyth client). Install the
    // process default once here so no lane races another for it.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let enabled = match EnabledVenues::from_env() {
        Ok(e) => e,
        Err(e) => {
            tracing::error!(error = %e, "signals: {} is invalid; no lanes started", signals_core::VENUE_GATE_ENV);
            return None;
        }
    };
    let config = Config::from_env();
    if let Err(e) = std::fs::create_dir_all(&config.lane_root) {
        tracing::error!(error = %e, root = %config.lane_root.display(), "signals: cannot create lane root; no lanes started");
        return None;
    }

    let (tx, exited) = mpsc::channel();
    // The in-process message stream exists before any lane thread does, so
    // no message a lane writes is ever lost to a race with the bridge.
    let (messages_tx, messages) = unbounded_channel();
    let mut lanes = Vec::new();
    for lane in Lane::ALL {
        match lane.eligibility(&enabled) {
            Ok(()) => {}
            Err(reason) => {
                tracing::info!(lane = lane.name(), %reason, "signals: lane skipped");
                continue;
            }
        }
        let config = config.clone();
        let tx = tx.clone();
        let hook = lane_stream_hook(lane.name(), messages_tx.clone());
        let spawned = std::thread::Builder::new()
            .name(format!("signals-{}", lane.name()))
            .stack_size(LANE_THREAD_STACK)
            .spawn(move || {
                supervise(lane, &config, hook);
                let _ = tx.send(lane.name());
            });
        match spawned {
            Ok(_) => lanes.push(lane.name()),
            Err(e) => tracing::error!(lane = lane.name(), error = %e, "signals: could not spawn lane thread"),
        }
    }
    if lanes.is_empty() {
        tracing::warn!("signals: no eligible lanes; nothing started");
        return None;
    }
    tracing::info!(
        lanes = ?lanes,
        root = %config.lane_root.display(),
        rotate_seconds = config.rotate_seconds,
        "signals: producers started"
    );
    Some(SignalProducers { lanes, messages: Some(messages), exited })
}

/// Restore "Ctrl-C ends the process" now that the lanes have installed
/// their own SIGINT/SIGTERM listeners. Waits up to [`SHUTDOWN_GRACE`] for
/// every lane thread to seal and exit, then terminates.
pub fn install_shutdown(producers: SignalProducers) {
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_err() {
            return;
        }
        tracing::info!(lanes = ?producers.lanes, "signals: shutdown requested; sealing lane generations");
        let expected = producers.lanes.len();
        let exited = producers.exited;
        let waited = tokio::task::spawn_blocking(move || {
            let deadline = std::time::Instant::now() + SHUTDOWN_GRACE;
            let mut done = 0;
            while done < expected {
                let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) else {
                    break;
                };
                match exited.recv_timeout(left) {
                    Ok(name) => {
                        tracing::info!(lane = name, "signals: lane sealed");
                        done += 1;
                    }
                    Err(_) => break,
                }
            }
            done
        })
        .await
        .unwrap_or(0);
        if waited < expected {
            tracing::warn!(sealed = waited, expected, "signals: grace period elapsed; exiting anyway");
        }
        std::process::exit(0);
    });
}

/// Run one lane forever on the current thread: build a single-threaded
/// runtime, run the lane, and restart it with back-off on failure or when
/// it reaches the prototype's run bound. Returns only on an operator
/// signal, when the lane has sealed its generation.
fn supervise(lane: Lane, config: &Config, hook: StreamHook) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!(lane = lane.name(), error = %e, "signals: cannot build lane runtime");
            return;
        }
    };
    let lane_root = config.lane_root.join(lane.name());
    let mut backoff = RESTART_MIN;
    loop {
        let epoch = publisher_epoch(lane);
        tracing::info!(lane = lane.name(), publisher_epoch = %epoch, root = %lane_root.display(), "signals: lane starting");
        let outcome = runtime.block_on(run_once(lane, &lane_root, config.rotate_seconds, epoch, hook.clone()));
        match outcome {
            Ok(Exit::Interrupted) => {
                tracing::info!(lane = lane.name(), "signals: lane stopped on operator signal");
                return;
            }
            Ok(Exit::Refused) => {
                tracing::warn!(lane = lane.name(), "signals: lane admission refused; not retrying");
                return;
            }
            Ok(Exit::Deadline) => {
                tracing::info!(lane = lane.name(), "signals: lane reached its run bound; restarting");
                backoff = RESTART_MIN;
            }
            Err(e) => {
                tracing::warn!(lane = lane.name(), error = format!("{e:#}"), delay_secs = backoff.as_secs(), "signals: lane failed; restarting");
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(RESTART_MAX);
            }
        }
    }
}

enum Exit {
    /// SIGINT/SIGTERM: the lane sealed and returned.
    Interrupted,
    /// The run bound elapsed; the lane sealed and returned.
    Deadline,
    /// The allow-list refused the lane. A configuration decision, not a
    /// transient failure.
    Refused,
}

fn publisher_epoch(lane: Lane) -> String {
    let unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("exchange-{}-{unix}", lane.name())
}

async fn run_once(lane: Lane, lane_root: &Path, rotate_seconds: u64, epoch: String, hook: StreamHook) -> Result<Exit> {
    std::fs::create_dir_all(lane_root)
        .with_context(|| format!("create lane root {}", lane_root.display()))?;
    let checkpoint = lane_root.join("checkpoint.json");
    // Every start is a fresh run under a new publisher epoch, and the
    // prototype refuses to overwrite an existing checkpoint on a fresh
    // run. Move the previous process's checkpoint aside rather than
    // failing forever on the second start.
    if checkpoint.exists() {
        let prev = lane_root.join("checkpoint.prev.json");
        std::fs::rename(&checkpoint, &prev)
            .with_context(|| format!("move stale checkpoint {}", checkpoint.display()))?;
        tracing::info!(lane = lane.name(), "signals: stale checkpoint moved aside");
    }
    match lane {
        Lane::Spot => run_spot(lane_root, checkpoint, rotate_seconds, epoch, hook).await,
        Lane::Polymarket => {
            run_prediction_lane(PredictionMarketVenue::Polymarket, lane_root, checkpoint, rotate_seconds, epoch, hook).await
        }
        Lane::Kalshi => {
            run_prediction_lane(PredictionMarketVenue::Kalshi, lane_root, checkpoint, rotate_seconds, epoch, hook).await
        }
        Lane::Binance | Lane::Bybit | Lane::Oracle | Lane::Cross => {
            run_venue_lane(lane.venue_spec().expect("venue lane"), lane_root, checkpoint, rotate_seconds, epoch, hook).await
        }
    }
}

async fn run_spot(lane_root: &Path, checkpoint: PathBuf, rotate_seconds: u64, epoch: String, hook: StreamHook) -> Result<Exit> {
    let admission = match admit_lane(&SPOT_LANE, IndexSocket::NotConfigured) {
        Ok(a) => a,
        Err(_) => return Ok(Exit::Refused),
    };
    tracing::info!("signals: {}", admission.startup_line());
    let interrupted = run_spot_signals_with_hook(SpotRunArgs {
        output: None,
        journal: None,
        checkpoint: Some(checkpoint),
        lane_root: Some(lane_root.to_path_buf()),
        rotate_seconds: Some(rotate_seconds),
        headroom_factor: HEADROOM_FACTOR,
        resume_from_checkpoint: false,
        publisher_epoch: epoch,
        duration_seconds: None,
        max_updates: MAX_UPDATES,
        max_bytes: MAX_OUTPUT_BYTES,
        max_journal_bytes: SPOT_MAX_JOURNAL_BYTES,
        journal_queue_capacity: DEFAULT_JOURNAL_QUEUE_CAPACITY,
        max_checkpoint_bytes: DEFAULT_CHECKPOINT_MAX_BYTES,
        checkpoint_queue_capacity: DEFAULT_CHECKPOINT_QUEUE_CAPACITY,
        checkpoint_interval_seconds: CHECKPOINT_INTERVAL_SECONDS,
        kraken_btc_book_depth: 100,
        induce_coinbase_trade_gap_once: false,
        coinbase_induced_reconnect_seconds: None,
        kraken_induced_reconnect_seconds: None,
    }, Some(hook))
    .await?;
    Ok(if interrupted { Exit::Interrupted } else { Exit::Deadline })
}

async fn run_prediction_lane(
    venue: PredictionMarketVenue,
    lane_root: &Path,
    checkpoint: PathBuf,
    rotate_seconds: u64,
    epoch: String,
    hook: StreamHook,
) -> Result<Exit> {
    let (requirement, socket) = match venue {
        PredictionMarketVenue::Kalshi => (&KALSHI_LANE, IndexSocket::Configured),
        PredictionMarketVenue::Polymarket => (&POLYMARKET_LANE, IndexSocket::NotConfigured),
    };
    let admission = match admit_lane(requirement, socket) {
        Ok(a) => a,
        Err(_) => return Ok(Exit::Refused),
    };
    tracing::info!("signals: {}", admission.startup_line());
    let duration = Duration::from_secs(MAX_RUN_SECONDS);
    let (_report, reason) = run_prediction(RunArguments {
        venue,
        output: None,
        journal: None,
        checkpoint,
        resume_from_checkpoint: false,
        publisher_epoch: epoch,
        duration,
        duration_seconds_report: duration.as_secs(),
        discovery_retry_ms: PREDICTION_DISCOVERY_RETRY_MS,
        checkpoint_interval_seconds: CHECKPOINT_INTERVAL_SECONDS,
        induced_reconnect_seconds: None,
        max_updates: MAX_UPDATES,
        max_output_bytes: MAX_OUTPUT_BYTES,
        max_journal_bytes: DEFAULT_PREDICTION_JOURNAL_MAX_BYTES,
        max_checkpoint_bytes: DEFAULT_PREDICTION_CHECKPOINT_MAX_BYTES,
        index_sockets: admission.index_sockets(),
        rotation: Some(RotationConfig {
            lane_root: lane_root.to_path_buf(),
            lane: requirement.lane.to_owned(),
            rotate_seconds,
            headroom_factor: HEADROOM_FACTOR,
            resumed: false,
        }),
    }, Some(hook))
    .await?;
    Ok(if reason.interrupted() { Exit::Interrupted } else { Exit::Deadline })
}

async fn run_venue_lane(
    spec: &'static LaneSpec,
    lane_root: &Path,
    checkpoint: PathBuf,
    rotate_seconds: u64,
    epoch: String,
    hook: StreamHook,
) -> Result<Exit> {
    let composite = matches!(spec.driver, LaneDriverSpec::Composite { .. });
    let socket = if composite {
        IndexSocket::Configured
    } else {
        IndexSocket::NotConfigured
    };
    let admission = match admit_lane(spec.requirement, socket) {
        Ok(a) => a,
        Err(_) => return Ok(Exit::Refused),
    };
    tracing::info!("signals: {}", admission.startup_line());
    let index_gating = composite.then(LaneIndexGating::from_env);
    let poll_driver = poll_driver(spec, Duration::from_secs(MAX_RUN_SECONDS), &epoch)?;
    let (_report, reason) = run_lane_with_hook(LaneRunArguments {
        spec,
        output: PathBuf::new(),
        journal: PathBuf::new(),
        checkpoint,
        resume_from_checkpoint: false,
        publisher_epoch: epoch,
        duration_seconds: MAX_RUN_SECONDS,
        checkpoint_interval_seconds: CHECKPOINT_INTERVAL_SECONDS,
        max_updates: MAX_UPDATES,
        max_output_bytes: MAX_OUTPUT_BYTES,
        max_journal_bytes: DEFAULT_LANE_JOURNAL_MAX_BYTES,
        max_checkpoint_bytes: DEFAULT_LANE_CHECKPOINT_MAX_BYTES,
        poll_driver,
        stream_runner: None,
        index_gating,
        rotation: Some(LaneRotation {
            lane_root: lane_root.to_path_buf(),
            rotate_seconds,
            headroom_factor: HEADROOM_FACTOR,
        }),
    }, Some(hook))
    .await?;
    Ok(if reason.interrupted() { Exit::Interrupted } else { Exit::Deadline })
}

/// The Chainlink poll driver for a poll-driven (oracle) lane; `None` for
/// stream-only lanes. Mirrors the `venue-signals` binary.
fn poll_driver(
    spec: &LaneSpec,
    duration: Duration,
    publisher_epoch: &str,
) -> Result<Option<Box<dyn PollDriver>>> {
    match spec.driver {
        LaneDriverSpec::Poll { label, .. } | LaneDriverSpec::Composite { label, .. } => {
            let endpoints = btc_feeds::chainlink::configured_rpc_endpoints()
                .map_err(|reason| anyhow::anyhow!("chainlink RPC endpoints: {reason}"))?;
            let driver = ChainlinkPollDriver::reqwest(
                label,
                ChainlinkPollOptions {
                    endpoints,
                    duration,
                    publisher_epoch: publisher_epoch.to_owned(),
                    shutdown: None,
                },
            )
            .context("build the Chainlink oracle poll driver")?;
            Ok(Some(Box::new(driver)))
        }
        LaneDriverSpec::Streams(_) => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The lane hook is a clone and a send: the message arrives on the
    /// channel tagged with its lane, and a closed receiver is not an error
    /// (the lane thread must never fail because the exchange is shutting
    /// down).
    #[test]
    fn lane_hook_delivers_tagged_messages_and_tolerates_a_closed_receiver() {
        let (tx, mut rx) = unbounded_channel();
        let hook = lane_stream_hook("binance", tx);
        let message = SignalStreamMessage::Reset {
            schema_version: signals_core::SIGNAL_STREAM_SCHEMA_VERSION,
            publisher_epoch: "test".into(),
            sequence: 0,
            produced_at: "2026-09-06T00:00:00Z".into(),
            reason: signals_core::StreamResetReason::PublisherEpochStarted,
        };
        hook(&message);
        let got = rx.try_recv().expect("one message");
        assert_eq!(got.lane, "binance");
        assert_eq!(got.message, message);
        drop(rx);
        hook(&message);
    }

    #[test]
    fn default_allow_list_runs_spot_and_polymarket_only_without_credentials() {
        let enabled = EnabledVenues::parse(None).unwrap();
        assert!(Lane::Spot.eligibility(&enabled).is_ok());
        assert!(Lane::Polymarket.eligibility(&enabled).is_ok());
        assert!(Lane::Binance.eligibility(&enabled).is_err());
        assert!(Lane::Bybit.eligibility(&enabled).is_err());
        assert!(Lane::Oracle.eligibility(&enabled).is_err());
        assert!(Lane::Cross.eligibility(&enabled).is_err());
        // Kalshi is allow-listed by default but needs credentials, which
        // the test environment does not have.
        let kalshi = Lane::Kalshi.eligibility(&enabled);
        assert!(kalshi.is_err() || btc_feeds::KalshiPassthroughClient::from_env().is_ok());
    }

    #[test]
    fn opting_in_venues_enables_their_lanes() {
        let enabled = EnabledVenues::parse(Some("coinbase,kraken,bybit,oracle")).unwrap();
        assert!(Lane::Bybit.eligibility(&enabled).is_ok());
        assert!(Lane::Oracle.eligibility(&enabled).is_ok());
        assert!(Lane::Cross.eligibility(&enabled).is_ok());
        assert!(Lane::Binance.eligibility(&enabled).is_err());
        assert!(Lane::Polymarket.eligibility(&enabled).is_err());
    }

    #[test]
    fn publisher_epoch_is_per_lane_and_short() {
        let e = publisher_epoch(Lane::Spot);
        assert!(e.starts_with("exchange-spot-"));
        assert!(e.len() <= 128);
    }
}
