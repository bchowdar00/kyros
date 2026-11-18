//! Live WebSocket runner: dial a provider, subscribe, and admit every frame
//! through the same deterministic adapter replay uses.
//!
//! A live run and a replay of the capture it produced yield identical events,
//! so nothing here may normalize on its own. Time comes from one [`ProcessClock`]
//! per process: wall time is sampled once for rendering, while freshness and
//! receipt stamps advance on the monotonic clock, so a host clock correction can
//! neither age a feed nor move a checkpoint. Transport faults reconnect with
//! backoff and admit a `Disconnected` record; a reached deadline or a
//! coordinated shutdown ends the run without manufacturing one. Counters and
//! ordinals are checked, so exhaustion fails the run rather than wrapping.

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use chrono::{DateTime, SecondsFormat, Utc};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::watch;
use tokio::time::{Instant, sleep, timeout};
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_tungstenite::{Connector, connect_async_tls_with_config};

use std::future::Future;

use signals_core::{ArmedIndexSockets, EnabledVenues, INDEX_SOCKET_TOKENS, PredictionMarketVenue};

use crate::capture::{
    BinanceUsdmRestSession, BybitRestSession, CONNECT_TIMEOUT, CaptureError,
    KRAKEN_USDT_BOOK_DEPTH, RestResponseRecord, endpoint, install_crypto_provider,
    read_idle_timeout, reconnect_delay, redial_backoff, redial_backoff_stepped, subscriptions,
    text_heartbeat, websocket_config,
};
use crate::coinbase_rest::{CoinbaseRestClient, CoinbaseRestError, CoinbaseTradeRecovery};
use crate::counters::checked_add;
use crate::kraken_auth::{KrakenAuthClient, KrakenAuthError};
use crate::kraken_rest::{
    FetchOutcome, KrakenReconciliationSummary, KrakenRestError, KrakenTradeReconciler,
};
use crate::prediction_live::{
    PredictionIndexSocketOptions, PredictionLiveAdmission, PredictionLiveError,
    PredictionLiveReport, PredictionSocketTransition, run_prediction_index_socket,
};
use crate::pyth_hermes_auth::PythHermesCredential;
use crate::replay::{KrakenBookDepths, ReplayAdapter};
use crate::{
    AdapterError, CaptureStream, FailureKind, FeedAdapter, FeedNotice, FeedNoticeCode,
    IngestOutcome, RawRecord, RecordKind, RecoveryAction, StreamTransport,
};

const MAX_RUN_DURATION: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const MAX_CONNECTION_ATTEMPTS: u64 = 100_000;

/// One wall/monotonic origin shared by every input admitted to a process.
///
/// Wall time is sampled once and retained only to render consumer timestamps.
/// Freshness and receipt stamps use nanoseconds elapsed from `mono_start`, so a
/// later host wall-clock correction cannot age feeds or move a checkpoint.
/// Journal admissions use a separate shared high-watermark and must be floored
/// by the already-sampled monotonic receipt that they include.
#[derive(Debug, Clone)]
pub struct ProcessClock {
    wall_start: DateTime<Utc>,
    mono_start: Instant,
    logical_offset_ns: u64,
    last_freshness_ns: Arc<AtomicU64>,
    last_admission_ns: Arc<AtomicU64>,
}

impl ProcessClock {
    /// Open a fresh logical timeline at the current instant, with both
    /// high-watermarks at zero.
    pub fn start() -> Self {
        // Sample the monotonic origin first. `wall_start + elapsed_ns()` is then
        // a conservative wall-clock admission time: it cannot trail a source
        // receipt sampled after `wall_start` merely because constructing the two
        // clock origins took time.
        let mono_start = Instant::now();
        let wall_start = DateTime::<Utc>::from(SystemTime::now());
        Self {
            wall_start,
            mono_start,
            logical_offset_ns: 0,
            last_freshness_ns: Arc::new(AtomicU64::new(0)),
            last_admission_ns: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Continue a prior logical timeline after a durable checkpoint.
    ///
    /// Wall time is sampled once to account conservatively for downtime. Once
    /// constructed, progression returns to the monotonic clock exclusively.
    pub fn resume(wall_start: DateTime<Utc>, checkpoint_time_ns: u64) -> Result<Self, LiveError> {
        let mono_start = Instant::now();
        let wall_now = DateTime::<Utc>::from(SystemTime::now());
        Self::resume_from_samples(wall_start, checkpoint_time_ns, wall_now, mono_start)
    }

    #[cfg(test)]
    fn resume_at(
        wall_start: DateTime<Utc>,
        checkpoint_time_ns: u64,
        wall_now: DateTime<Utc>,
    ) -> Result<Self, LiveError> {
        let mono_start = Instant::now();
        Self::resume_from_samples(wall_start, checkpoint_time_ns, wall_now, mono_start)
    }

    fn resume_from_samples(
        wall_start: DateTime<Utc>,
        checkpoint_time_ns: u64,
        wall_now: DateTime<Utc>,
        mono_start: Instant,
    ) -> Result<Self, LiveError> {
        let elapsed = wall_now
            .signed_duration_since(wall_start)
            .num_nanoseconds()
            .ok_or(LiveError::ClockRange)?;
        let recovered_time_ns = u64::try_from(elapsed).unwrap_or(0);
        let logical_offset_ns = recovered_time_ns.max(checkpoint_time_ns);
        Ok(Self {
            wall_start,
            mono_start,
            logical_offset_ns,
            last_freshness_ns: Arc::new(AtomicU64::new(logical_offset_ns)),
            last_admission_ns: Arc::new(AtomicU64::new(logical_offset_ns)),
        })
    }

    /// Sample a freshness/receipt stamp: nanoseconds elapsed on the monotonic
    /// clock since `mono_start`, plus any resumed logical offset.
    ///
    /// Non-decreasing across clones through a shared high-watermark, and immune
    /// to host wall-clock corrections. Errors with `CounterExhausted` if the
    /// elapsed nanosecond count would overflow u64.
    pub fn elapsed_ns(&self) -> Result<u64, LiveError> {
        let candidate = self
            .logical_offset_ns
            .checked_add(elapsed_ns(self.mono_start)?)
            .ok_or(LiveError::CounterExhausted)?;
        Ok(candidate.max(
            self.last_freshness_ns
                .fetch_max(candidate, Ordering::Relaxed),
        ))
    }

    /// Sample a journal admission after normalization. `receipt_floor_ns` must be
    /// the monotonic receipt carried by the admitted input. Clones share this
    /// admission-only high-watermark, while freshness remains wall-immune.
    pub fn admission_ns(&self, receipt_floor_ns: u64) -> Result<u64, LiveError> {
        let monotonic = self
            .logical_offset_ns
            .checked_add(elapsed_ns(self.mono_start)?)
            .ok_or(LiveError::CounterExhausted)?;
        let candidate = monotonic.max(receipt_floor_ns);
        Ok(candidate.max(
            self.last_admission_ns
                .fetch_max(candidate, Ordering::Relaxed),
        ))
    }

    /// The monotonic instant `duration` after the process origin, not after now.
    /// A run started late therefore still ends on the process-relative deadline.
    pub fn deadline(&self, duration: Duration) -> Instant {
        self.mono_start + duration
    }

    /// The frozen wall-clock origin every rendered timestamp is measured from.
    pub fn wall_start(&self) -> DateTime<Utc> {
        self.wall_start
    }

    /// The wall origin as an RFC 3339 UTC string with nanosecond precision.
    pub fn wall_start_text(&self) -> String {
        self.wall_start.to_rfc3339_opts(SecondsFormat::Nanos, true)
    }

    /// Render one admitted logical instant against the process wall origin.
    pub fn wall_time_at(&self, now_ns: u64) -> Result<DateTime<Utc>, LiveError> {
        let nanos = i64::try_from(now_ns).map_err(|_| LiveError::ClockRange)?;
        self.wall_start
            .checked_add_signed(chrono::Duration::nanoseconds(nanos))
            .ok_or(LiveError::ClockRange)
    }

    /// Render a live receipt from the frozen wall origin plus monotonic elapsed
    /// time. This is intentionally not a fresh `SystemTime` sample.
    pub fn receipt_time_text(&self, receipt_ns: u64) -> Result<String, LiveError> {
        Ok(self
            .wall_time_at(receipt_ns)?
            .to_rfc3339_opts(SecondsFormat::Micros, true))
    }
}

/// One adapter outcome paired with the receipt stamped before it was parsed.
#[derive(Debug, Clone)]
pub struct TimedIngestOutcome {
    /// The normalized events and notices the adapter produced from one record.
    pub outcome: IngestOutcome,
    /// Nanoseconds from the process monotonic origin, sampled before payload
    /// conversion or adapter work so the stamp excludes our own parse cost.
    pub logical_receipt_ns: u64,
}

/// The full configuration of one live run.
#[derive(Debug, Clone)]
pub struct LiveOptions {
    /// Which provider stream to dial, subscribe to, and normalize.
    pub stream: CaptureStream,
    /// How long to run before the natural deadline. Must be at least 1 second
    /// and at most 7 days.
    pub duration: Duration,
    /// Requested Kraken BTC book depth in price levels. KrakenLevel3 accepts
    /// 10, 100 or 1000; other streams also accept 25 and 500.
    pub kraken_btc_book_depth: u16,
    /// Publisher-epoch label stamped on emitted events. 1 to 128 bytes,
    /// non-blank.
    pub publisher_epoch: String,
    /// Opt-in fault exercises. The default induces nothing.
    pub diagnostics: LiveDiagnostics,
    /// Shared cancellation receiver for a coordinated multi-task run. `None`
    /// makes this run poll the operating-system stop signal itself.
    pub shutdown: Option<LiveShutdown>,
    /// Opt-in observe-only Kraken public-trade reconciliation cadence
    /// (KrakenCore only). `None` disables it. Never runs during replay.
    pub kraken_trade_reconciliation: Option<Duration>,
}

/// One cloneable receiver for a process-wide controlled shutdown.
#[derive(Debug, Clone)]
pub struct LiveShutdown {
    receiver: watch::Receiver<Option<LiveShutdownReason>>,
}

/// The sole trigger paired with all live-task shutdown receivers.
#[derive(Debug, Clone)]
pub struct LiveShutdownTrigger {
    sender: watch::Sender<Option<LiveShutdownReason>>,
}

/// The authoritative reason a coordinated live run stopped.
///
/// Workers may observe the shared cancellation at the same instant as their own
/// deadline. Carrying the coordinator's cause through the channel prevents that
/// teardown race from relabeling a natural completion as an operator interrupt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveShutdownReason {
    /// The run reached its configured deadline. A completion, not a failure.
    NaturalDeadline,
    /// An operator stop signal (SIGINT or SIGTERM) arrived while the deadline
    /// was still pending.
    OperatorSignal,
}

impl LiveShutdownReason {
    /// True only for an operator stop; a reached deadline is not an interrupt.
    pub fn interrupted(self) -> bool {
        matches!(self, Self::OperatorSignal)
    }
}

/// Build the single trigger and its first receiver for one coordinated run.
///
/// Clone the receiver once per live task. The first published reason wins, so
/// every task observes the same cause.
pub fn live_shutdown_channel() -> (LiveShutdownTrigger, LiveShutdown) {
    let (sender, receiver) = watch::channel(None);
    (LiveShutdownTrigger { sender }, LiveShutdown { receiver })
}

impl LiveShutdownTrigger {
    /// Manually publish an operator-requested stop.
    ///
    /// Production signal runners use [`coordinate_live_shutdown`] as the sole
    /// writer. This escape hatch remains for isolated callers and tests that do
    /// not own a deadline coordinator.
    pub fn cancel(&self) {
        self.cancel_with_reason(LiveShutdownReason::OperatorSignal);
    }

    fn cancel_with_reason(&self, reason: LiveShutdownReason) {
        self.sender.send_if_modified(|state| {
            if state.is_some() {
                false
            } else {
                *state = Some(reason);
                true
            }
        });
    }
}

impl LiveShutdown {
    /// Resolve once a shutdown has been published, discarding the reason.
    pub async fn wait(&mut self) -> Result<(), std::io::Error> {
        self.wait_reason().await.map(|_| ())
    }

    /// Resolve with the coordinator's cause once a shutdown has been published.
    ///
    /// Returns immediately if cancellation already happened, so a late waiter
    /// never blocks. Errors with `BrokenPipe` if every trigger was dropped
    /// without cancelling, which no coordinated run should do.
    pub async fn wait_reason(&mut self) -> Result<LiveShutdownReason, std::io::Error> {
        if let Some(reason) = *self.receiver.borrow() {
            return Ok(reason);
        }
        loop {
            self.receiver.changed().await.map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "live shutdown trigger was dropped before cancellation",
                )
            })?;
            if let Some(reason) = *self.receiver.borrow() {
                return Ok(reason);
            }
        }
    }
}

/// Coordinate one signal runner against an absolute natural deadline.
///
/// Explicit boundary checks before and after arbitration make an already reached
/// deadline authoritative regardless of which ready branch Tokio selects. A signal
/// observed while the deadline is still pending remains an interrupt.
pub async fn coordinate_live_shutdown(
    deadline: Instant,
    trigger: LiveShutdownTrigger,
) -> Result<LiveShutdownReason, std::io::Error> {
    coordinate_live_shutdown_on(deadline, trigger, wait_for_stop_signal()).await
}

/// Injectable form of [`coordinate_live_shutdown`] used by runner regressions.
pub async fn coordinate_live_shutdown_on<F>(
    deadline: Instant,
    trigger: LiveShutdownTrigger,
    signal: F,
) -> Result<LiveShutdownReason, std::io::Error>
where
    F: std::future::Future<Output = Result<(), std::io::Error>>,
{
    coordinate_live_shutdown_race_on(
        trigger,
        signal,
        tokio::time::sleep_until(deadline),
        move || Instant::now() >= deadline,
    )
    .await
}

async fn coordinate_live_shutdown_race_on<F, D, N>(
    trigger: LiveShutdownTrigger,
    signal: F,
    deadline_wait: D,
    deadline_reached: N,
) -> Result<LiveShutdownReason, std::io::Error>
where
    F: std::future::Future<Output = Result<(), std::io::Error>>,
    D: std::future::Future<Output = ()>,
    N: Fn() -> bool,
{
    tokio::pin!(signal);
    tokio::pin!(deadline_wait);
    let reason = if deadline_reached() {
        LiveShutdownReason::NaturalDeadline
    } else {
        enum RaceResult {
            Deadline,
            Signal(Result<(), std::io::Error>),
        }

        let result = tokio::select! {
            () = &mut deadline_wait => RaceResult::Deadline,
            result = &mut signal => RaceResult::Signal(result),
        };
        match result {
            RaceResult::Deadline => LiveShutdownReason::NaturalDeadline,
            RaceResult::Signal(result) => {
                if deadline_reached() {
                    LiveShutdownReason::NaturalDeadline
                } else {
                    result?;
                    LiveShutdownReason::OperatorSignal
                }
            }
        }
    };
    trigger.cancel_with_reason(reason);
    Ok(reason)
}

pub(crate) struct ShutdownWait {
    shared: Option<LiveShutdown>,
    operating_system:
        Pin<Box<dyn std::future::Future<Output = Result<(), std::io::Error>> + Send + 'static>>,
}

impl ShutdownWait {
    pub(crate) fn new(shared: Option<LiveShutdown>) -> Self {
        Self {
            shared,
            operating_system: Box::pin(wait_for_stop_signal()),
        }
    }

    pub(crate) async fn wait(&mut self) -> Result<LiveShutdownReason, std::io::Error> {
        match self.shared.as_mut() {
            Some(shared) => shared.wait_reason().await,
            None => self
                .operating_system
                .as_mut()
                .await
                .map(|()| LiveShutdownReason::OperatorSignal),
        }
    }
}

/// Resolve when the process is asked to stop by an operating-system signal.
///
/// SIGINT (Ctrl-C) and, on Unix, SIGTERM are treated identically: both mean
/// "stop now, gracefully". SIGTERM is what systemd sends by default and what
/// `docker stop` sends, so handling it lets the run reach its normal shutdown
/// path — writing the terminal journal record and exiting 130 — instead of
/// being killed mid-write with only a torn-tail prefix on disk.
///
/// Public so a binary that drives its own shared-trigger shutdown coordinator
/// (rather than letting [`ShutdownWait`] poll the signal) installs the same
/// SIGINT+SIGTERM listener instead of a bare `ctrl_c`.
pub async fn wait_for_stop_signal() -> Result<(), std::io::Error> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        let mut terminate = signal(SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result,
            _ = terminate.recv() => Ok(()),
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await
    }
}

/// Explicit fault exercises. Defaults add no parsing or forced reconnects.
#[derive(Debug, Clone, Default)]
pub struct LiveDiagnostics {
    /// Drop the second Coinbase `match` message of the run to force one trade
    /// sequence gap and prove the REST backfill path. CoinbaseCore only; the run
    /// fails with `DiagnosticIncomplete` if no drop or no backfill followed.
    pub induce_coinbase_trade_gap_once: bool,
    /// Close the socket this far into each connection to prove reconnect and
    /// resubscribe. Must be positive and shorter than the run duration.
    pub induced_reconnect_interval: Option<Duration>,
}

impl LiveOptions {
    fn validate(&self) -> Result<(), LiveError> {
        if self.duration.is_zero() || self.duration > MAX_RUN_DURATION {
            return Err(LiveError::InvalidOptions(format!(
                "duration must be between 1 second and {} seconds",
                MAX_RUN_DURATION.as_secs()
            )));
        }
        if self.publisher_epoch.trim().is_empty() || self.publisher_epoch.len() > 128 {
            return Err(LiveError::InvalidOptions(
                "publisher_epoch must contain 1 to 128 bytes".to_owned(),
            ));
        }
        let valid_depth = match self.stream {
            CaptureStream::KrakenLevel3 => matches!(self.kraken_btc_book_depth, 10 | 100 | 1000),
            _ => matches!(self.kraken_btc_book_depth, 10 | 25 | 100 | 500 | 1000),
        };
        if !valid_depth {
            return Err(LiveError::InvalidOptions(
                "Kraken depth is not supported by the selected stream".to_owned(),
            ));
        }
        if matches!(
            self.stream,
            CaptureStream::KalshiMarket | CaptureStream::PolymarketMarket
        ) {
            return Err(LiveError::InvalidOptions(
                "prediction-market streams require self-contained capture plus replay".to_owned(),
            ));
        }
        if self.diagnostics.induce_coinbase_trade_gap_once
            && self.stream != CaptureStream::CoinbaseCore
        {
            return Err(LiveError::InvalidOptions(
                "Coinbase trade-gap injection is valid only for coinbase-core".to_owned(),
            ));
        }
        if let Some(interval) = self.diagnostics.induced_reconnect_interval
            && (interval.is_zero() || interval >= self.duration)
        {
            return Err(LiveError::InvalidOptions(
                "induced reconnect interval must be positive and shorter than the run".to_owned(),
            ));
        }
        if let Some(cadence) = self.kraken_trade_reconciliation {
            if self.stream != CaptureStream::KrakenCore {
                return Err(LiveError::InvalidOptions(
                    "Kraken trade reconciliation is valid only for kraken-core".to_owned(),
                ));
            }
            if cadence.is_zero() {
                return Err(LiveError::InvalidOptions(
                    "Kraken trade reconciliation cadence must be positive".to_owned(),
                ));
            }
        }
        Ok(())
    }
}

/// Counter totals for one completed live run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LiveReport {
    /// Count of raw records admitted to the adapter, inbound and outbound.
    pub records: u64,
    /// Count of normalized events the adapter emitted across all records.
    pub events: u64,
    /// Count of feed notices the adapter emitted across all records.
    pub notices: u64,
    /// Count of those notices that carried a recovery action.
    pub recoveries: u64,
    /// Count of completed Coinbase REST trade-backfill fetches.
    pub coinbase_trade_backfills: u64,
    /// Count of dial attempts, including those that failed or timed out.
    pub connection_attempts: u64,
    /// Count of dial attempts that produced an established socket.
    pub successful_connections: u64,
    /// Count of redials after a socket ended in a transport failure or a
    /// consumer-requested reconnect. Excludes the initial connection.
    pub reconnects: u64,
    /// Count of read-idle timeouts that closed a socket, one per silent stream
    /// exceeding the per-stream idle budget.
    pub read_idle_timeouts: u64,
    /// Count of Coinbase trade messages dropped by the gap-injection diagnostic.
    /// Zero unless that fault exercise was requested.
    pub induced_coinbase_trade_drops: u64,
    /// Count of sockets closed by the reconnect-injection diagnostic. Zero
    /// unless that fault exercise was requested.
    pub induced_disconnects: u64,
    /// Count of Kraken authenticated-WebSocket tokens minted, one per
    /// KrakenLevel3 connection.
    pub kraken_tokens_created: u64,
    /// This worker's locally selected stop branch, not the authoritative
    /// process exit classification. The deadline coordinator owns that verdict.
    pub interrupted: bool,
    /// Observe-only Kraken public-trade reconciliation results, present only
    /// when the KrakenCore run opted into the maintenance cadence.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kraken_trade_reconciliation: Option<KrakenReconciliationSummary>,
}

#[derive(Default)]
struct LiveCounters {
    records: u64,
    events: u64,
    notices: u64,
    recoveries: u64,
    coinbase_trade_backfills: u64,
    connection_attempts: u64,
    successful_connections: u64,
    reconnects: u64,
    read_idle_timeouts: u64,
    induced_coinbase_trade_drops: u64,
    induced_disconnects: u64,
    kraken_tokens_created: u64,
}

#[derive(Default)]
struct DiagnosticState {
    coinbase_match_seen: bool,
    coinbase_trade_dropped: bool,
}

impl DiagnosticState {
    fn should_drop_coinbase_trade(&mut self, requested: bool, payload: &str) -> bool {
        if !requested || self.coinbase_trade_dropped {
            return false;
        }
        let Ok(message) = serde_json::from_str::<CoinbaseMessageKind>(payload) else {
            return false;
        };
        if message.kind.as_deref() != Some("match") {
            return false;
        }
        if self.coinbase_match_seen {
            self.coinbase_trade_dropped = true;
            true
        } else {
            self.coinbase_match_seen = true;
            false
        }
    }
}

#[derive(Deserialize)]
struct CoinbaseMessageKind {
    #[serde(rename = "type")]
    kind: Option<String>,
}

/// Run the live socket directly through the deterministic adapter.
///
/// The callback is the replaceable publication boundary. It must return only
/// after accepting the outcome; a failure stops the run instead of dropping
/// normalized events under backpressure. A reconnect response closes the
/// current socket after the accepted outcome so stateful consumers can demand
/// a fresh provider snapshot without terminating the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveControl {
    /// The outcome was accepted; keep reading on the current socket.
    Continue,
    /// The outcome was accepted, but the consumer needs a fresh provider
    /// snapshot. The current socket is closed and redialled; the run continues.
    Reconnect,
}

/// Drive one live WebSocket run to its deadline, publishing every normalized
/// outcome through `handle`.
///
/// Validates `options`, dials the stream's endpoint, subscribes, and admits each
/// frame through the same deterministic adapter replay uses, so a live run and a
/// replay of its capture produce identical events. Connect, disconnect, and
/// outbound heartbeat records are admitted alongside inbound text. Transport
/// failures reconnect with backoff up to `MAX_CONNECTION_ATTEMPTS`; a deadline
/// or controlled shutdown ends the run without manufacturing a disconnect.
/// Returns the run's counter totals, or the first error that stopped it.
pub async fn run_live<H>(
    options: LiveOptions,
    clock: ProcessClock,
    mut handle: H,
) -> Result<LiveReport, LiveError>
where
    H: FnMut(TimedIngestOutcome) -> Result<LiveControl, LiveError>,
{
    options.validate()?;
    install_crypto_provider()?;
    // The oracle lane's Kalshi index socket (ruling E37d) rides its own dedicated
    // connection through the prediction index-socket runner, not the generic
    // WebSocket dial below. Dispatch it before any endpoint/auth setup so it never
    // reaches the market-socket path; a disarmed lane opens no socket and returns
    // an empty report rather than aborting.
    if options.stream == CaptureStream::KalshiIndex {
        return run_live_kalshi_index(options, clock, handle).await;
    }
    // An `http_poll` stream has no socket to dial: it is driven through the same
    // per-endpoint poll loops the capture path uses, not a websocket. Dispatching
    // here is what keeps a REST poll identity from being dialled as a websocket
    // (ruling E41): `bybit_linear_rest` once failed 33 redials against
    // `https://api.bybit.com` with "URL scheme not supported" before this branch.
    if options.stream.transport() == StreamTransport::HttpPoll {
        return run_live_http_poll(options, clock, handle).await;
    }
    let kraken_auth = if options.stream == CaptureStream::KrakenLevel3 {
        Some(KrakenAuthClient::from_env()?)
    } else {
        None
    };
    let coinbase_rest = if options.stream == CaptureStream::CoinbaseCore {
        Some(CoinbaseRestClient::new()?)
    } else {
        None
    };
    // Observe-only Kraken public-trade reconciliation is opt-in and KrakenCore
    // only (validated above). It lives entirely in the live runner and never
    // enters admitted state, so replay is unaffected by construction.
    let mut kraken_reconciler = match (options.stream, options.kraken_trade_reconciliation) {
        (CaptureStream::KrakenCore, Some(cadence)) => Some(KrakenTradeReconciler::new(cadence)?),
        _ => None,
    };
    let mut adapter = ReplayAdapter::new(
        options.stream,
        options.publisher_epoch.clone(),
        KrakenBookDepths {
            btc: options.kraken_btc_book_depth,
            usdt: KRAKEN_USDT_BOOK_DEPTH,
        },
    )?;
    let deadline = clock.deadline(options.duration);
    let mut shutdown = ShutdownWait::new(options.shutdown.clone());
    let mut connection_epoch = 0_u64;
    let mut ordinal = 0_u64;
    let mut counters = LiveCounters::default();
    let mut diagnostics = DiagnosticState::default();
    // At most one observe-only reconciliation fetch is in flight across the
    // whole run. It runs as a spawned task so REST latency never blocks the
    // receive loop that stamps socket receipts; completion is folded in
    // synchronously. Persisting it here (not per-connection) enforces the
    // single-in-flight bound across reconnects.
    let mut reconcile_fetch: Option<tokio::task::JoinHandle<FetchOutcome>> = None;
    let mut interrupted = false;
    // Consecutive connections that produced no data — a dial that failed, or one
    // that opened and delivered nothing. The first inbound frame of any
    // connection resets it. It paces the redial on the capture path's 1 s → 60 s
    // ladder (ruling E27(c)): without it a venue that accepts the socket and then
    // says nothing is re-dialled the instant the idle timeout fires, for ever.
    let mut unproductive_streak: u64 = 0;

    'run: while Instant::now() < deadline {
        if counters.connection_attempts >= MAX_CONNECTION_ATTEMPTS {
            return Err(LiveError::ConnectionAttemptLimit);
        }
        // Whether THIS connection delivered any inbound frame; the first one
        // resets the redial ladder below.
        let mut connection_productive = false;
        counters.connection_attempts = checked_add(counters.connection_attempts, 1, || {
            LiveError::CounterExhausted
        })?;
        let connect_started_elapsed_ns = clock.elapsed_ns()?;
        let connected = tokio::select! {
            result = shutdown.wait() => {
                interrupted = result.map_err(LiveError::Signal)?.interrupted();
                break 'run;
            }
            () = tokio::time::sleep_until(deadline) => break 'run,
            result = timeout(
                CONNECT_TIMEOUT,
                connect_async_tls_with_config(
                    endpoint(options.stream),
                    Some(websocket_config()),
                    true,
                    Some(Connector::Rustls(crate::websocket_tls::client_config_for(
                        options.stream,
                    ))),
                ),
            ) => result,
        };
        let (mut socket, _) = match connected {
            Ok(Ok(connection)) => connection,
            outcome @ (Ok(Err(_)) | Err(_)) => {
                // Ruling E27(c): a dial that never completes must not be silent.
                // There is no connection epoch to hang a record on, so the
                // failure is reported twice, deliberately — on stderr for the
                // operator, and as an explicit feed notice for the consumer, who
                // would otherwise see only an absence of events and have no way
                // to tell a quiet venue from an unreachable one.
                let cause = match outcome {
                    Ok(Err(error)) => error.to_string(),
                    Err(_) => format!("no response within {CONNECT_TIMEOUT:?}"),
                    Ok(Ok(_)) => unreachable!("the success arm returned above"),
                };
                eprintln!(
                    "live {}: connection attempt {} to {} failed: {cause}",
                    options.stream.as_str(),
                    counters.connection_attempts,
                    endpoint(options.stream)
                );
                let notice = IngestOutcome::notice(FeedNotice::recovery(
                    FeedNoticeCode::Disconnected,
                    format!(
                        "connection attempt {} to {} failed: {cause}",
                        counters.connection_attempts,
                        endpoint(options.stream)
                    ),
                    RecoveryAction::Reconnect,
                ));
                count_outcome(&mut counters, &notice)?;
                handle(TimedIngestOutcome {
                    outcome: notice,
                    logical_receipt_ns: clock.elapsed_ns()?,
                })?;
                // A dial that never completed produced no data (E27(c)): the
                // unproductive ladder, 1 s → 60 s, one stderr line per step.
                unproductive_streak = unproductive_streak.saturating_add(1);
                let delay = unproductive_redial_delay(unproductive_streak, options.stream);
                match wait_before_redial(delay, deadline, &mut shutdown).await? {
                    ReconnectWait::Retry => continue,
                    ReconnectWait::Deadline => break 'run,
                    ReconnectWait::Interrupted => {
                        interrupted = true;
                        break 'run;
                    }
                }
            }
        };
        counters.successful_connections = checked_add(counters.successful_connections, 1, || {
            LiveError::CounterExhausted
        })?;
        connection_epoch = checked_add(connection_epoch, 1, || LiveError::CounterExhausted)?;
        ordinal = checked_add(ordinal, 1, || LiveError::CounterExhausted)?;
        let logical_receipt_ns = clock.elapsed_ns()?;
        dispatch(
            &mut adapter,
            connected_record(
                &clock,
                options.stream,
                connection_epoch,
                ordinal,
                logical_receipt_ns,
                connect_started_elapsed_ns,
            )?,
            &clock,
            logical_receipt_ns,
            coinbase_rest.as_ref(),
            kraken_reconciler.as_mut(),
            &mut counters,
            &mut handle,
        )
        .await?;

        let token = if let Some(auth) = &kraken_auth {
            let token = tokio::select! {
                result = shutdown.wait() => {
                    interrupted = result.map_err(LiveError::Signal)?.interrupted();
                    break 'run;
                }
                () = tokio::time::sleep_until(deadline) => break 'run,
                result = auth.websocket_token() => result?,
            };
            counters.kraken_tokens_created =
                checked_add(counters.kraken_tokens_created, 1, || {
                    LiveError::CounterExhausted
                })?;
            Some(token)
        } else {
            None
        };
        let mut subscriptions_ready = true;
        let mut exit_run = false;
        // `btc-feeds live` runs one capture stream through one adapter and
        // refuses `kalshi_index` outright, so it subscribes no index channel.
        // The lane that carries them on the Kalshi market socket is the
        // prediction lane (ruling E23).
        for subscription in subscriptions(
            options.stream,
            options.kraken_btc_book_depth,
            token.as_ref(),
            None,
            None,
            ArmedIndexSockets::NONE,
        )? {
            enum SubscriptionSend {
                Sent(bool),
                Exit,
            }
            let sent = tokio::select! {
                result = shutdown.wait() => {
                    interrupted = result.map_err(LiveError::Signal)?.interrupted();
                    SubscriptionSend::Exit
                }
                () = tokio::time::sleep_until(deadline) => SubscriptionSend::Exit,
                result = socket.send(Message::Text(subscription.wire.as_str().to_owned().into())) => {
                    SubscriptionSend::Sent(result.is_ok())
                }
            };
            match sent {
                SubscriptionSend::Sent(true) => {}
                SubscriptionSend::Sent(false) => {
                    subscriptions_ready = false;
                    break;
                }
                SubscriptionSend::Exit => {
                    subscriptions_ready = false;
                    exit_run = true;
                    break;
                }
            }
            ordinal = checked_add(ordinal, 1, || LiveError::CounterExhausted)?;
            let logical_receipt_ns = clock.elapsed_ns()?;
            let control = dispatch(
                &mut adapter,
                text_record(
                    &clock,
                    options.stream,
                    connection_epoch,
                    ordinal,
                    RecordKind::OutboundText,
                    subscription.recorded,
                    logical_receipt_ns,
                )?,
                &clock,
                logical_receipt_ns,
                coinbase_rest.as_ref(),
                kraken_reconciler.as_mut(),
                &mut counters,
                &mut handle,
            )
            .await?;
            if control == DispatchControl::Reconnect {
                subscriptions_ready = false;
                break;
            }
        }

        if subscriptions_ready && !exit_run {
            let induced_disconnect_at = options
                .diagnostics
                .induced_reconnect_interval
                .map(|interval| Instant::now() + interval)
                .filter(|connection_deadline| *connection_deadline < deadline);
            let receive_deadline = induced_disconnect_at.unwrap_or(deadline);
            // The ticker and its payload travel as one pair so a heartbeat can
            // never fire with the wrong stream's payload.
            let mut text_heartbeat = text_heartbeat(options.stream).map(|(interval, payload)| {
                (
                    tokio::time::interval_at(Instant::now() + interval, interval),
                    payload,
                )
            });
            // Observe-only Kraken reconciliation cadence. Present only when the
            // KrakenCore run opted in; the first tick fires after one cadence.
            let mut reconciliation = kraken_reconciler.as_ref().and_then(|_| {
                options
                    .kraken_trade_reconciliation
                    .map(|cadence| tokio::time::interval_at(Instant::now() + cadence, cadence))
            });
            loop {
                enum ReadAction {
                    Message(
                        Result<
                            Option<Result<Message, tokio_tungstenite::tungstenite::Error>>,
                            tokio::time::error::Elapsed,
                        >,
                    ),
                    Heartbeat,
                    ReconcileTick,
                    ReconcileResult(Result<FetchOutcome, tokio::task::JoinError>),
                    InducedDisconnect,
                    Exit,
                }
                let action = tokio::select! {
                    result = shutdown.wait() => {
                        interrupted = result.map_err(LiveError::Signal)?.interrupted();
                        exit_run = true;
                        ReadAction::Exit
                    }
                    () = tokio::time::sleep_until(receive_deadline) => {
                        if induced_disconnect_at.is_some() {
                            ReadAction::InducedDisconnect
                        } else {
                            exit_run = true;
                            ReadAction::Exit
                        }
                    },
                    _ = async {
                        text_heartbeat
                            .as_mut()
                            .expect("guarded text heartbeat exists")
                            .0
                            .tick()
                            .await;
                    }, if text_heartbeat.is_some() => ReadAction::Heartbeat,
                    _ = async {
                        reconciliation
                            .as_mut()
                            .expect("guarded reconciliation interval exists")
                            .tick()
                            .await;
                    }, if reconciliation.is_some() => ReadAction::ReconcileTick,
                    result = async {
                        reconcile_fetch
                            .as_mut()
                            .expect("pending reconciliation fetch exists")
                            .await
                    }, if reconcile_fetch.is_some() => ReadAction::ReconcileResult(result),
                    result = next_with_timeout(read_idle_timeout(options.stream), &mut socket) => {
                        ReadAction::Message(result)
                    },
                };
                match action {
                    ReadAction::Exit => break,
                    ReadAction::InducedDisconnect => {
                        counters.induced_disconnects =
                            checked_add(counters.induced_disconnects, 1, || {
                                LiveError::CounterExhausted
                            })?;
                        break;
                    }
                    ReadAction::Heartbeat => {
                        let payload = text_heartbeat
                            .as_ref()
                            .expect("heartbeat action fires only when the pair exists")
                            .1;
                        if socket
                            .send(Message::Text(payload.to_owned().into()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                        ordinal = checked_add(ordinal, 1, || LiveError::CounterExhausted)?;
                        let logical_receipt_ns = clock.elapsed_ns()?;
                        dispatch(
                            &mut adapter,
                            text_record(
                                &clock,
                                options.stream,
                                connection_epoch,
                                ordinal,
                                RecordKind::OutboundText,
                                payload.to_owned(),
                                logical_receipt_ns,
                            )?,
                            &clock,
                            logical_receipt_ns,
                            coinbase_rest.as_ref(),
                            kraken_reconciler.as_mut(),
                            &mut counters,
                            &mut handle,
                        )
                        .await?;
                    }
                    ReadAction::ReconcileTick => {
                        // Spawn one bounded REST fetch off the loop, at most one
                        // in flight. Spawning is non-blocking; the actual compare
                        // happens synchronously when the task completes below, so
                        // socket-receipt stamping is never delayed by REST I/O.
                        if reconcile_fetch.is_none()
                            && let Some(reconciler) = kraken_reconciler.as_ref()
                        {
                            reconcile_fetch = Some(tokio::spawn(reconciler.spawn_fetch()));
                        }
                    }
                    ReadAction::ReconcileResult(join_result) => {
                        // Fold the completed fetch in synchronously (no I/O).
                        // Observe-only: results land only in the run report, and a
                        // task panic/join error is a failed cycle, never fatal.
                        reconcile_fetch = None;
                        if let Some(reconciler) = kraken_reconciler.as_mut() {
                            match join_result {
                                Ok(outcome) => reconciler.ingest_fetch_outcome(outcome),
                                Err(_) => reconciler.record_task_failure(),
                            }
                        }
                    }
                    ReadAction::Message(Ok(Some(Ok(Message::Text(payload))))) => {
                        // The first inbound frame proves the connection productive
                        // and resets the redial ladder (E27(c)).
                        connection_productive = true;
                        // Stamp receipt before payload conversion or adapter work.
                        let logical_receipt_ns = clock.elapsed_ns()?;
                        let payload = payload.to_string();
                        if diagnostics.should_drop_coinbase_trade(
                            options.diagnostics.induce_coinbase_trade_gap_once,
                            &payload,
                        ) {
                            counters.induced_coinbase_trade_drops =
                                checked_add(counters.induced_coinbase_trade_drops, 1, || {
                                    LiveError::CounterExhausted
                                })?;
                            continue;
                        }
                        ordinal = checked_add(ordinal, 1, || LiveError::CounterExhausted)?;
                        let dispatched = dispatch(
                            &mut adapter,
                            text_record(
                                &clock,
                                options.stream,
                                connection_epoch,
                                ordinal,
                                RecordKind::InboundText,
                                payload,
                                logical_receipt_ns,
                            )?,
                            &clock,
                            logical_receipt_ns,
                            coinbase_rest.as_ref(),
                            kraken_reconciler.as_mut(),
                            &mut counters,
                            &mut handle,
                        )
                        .await;
                        match dispatched {
                            Ok(DispatchControl::Reconnect) => break,
                            Ok(DispatchControl::Continue) => {}
                            // The frame already decoded as UTF-8 text at the
                            // transport layer, so every error `dispatch` can return
                            // here is an adapter/recovery/REST/counter fault — a
                            // fail-closed refusal that must stop the run exactly as
                            // it did before E37d. Propagate it verbatim. Only a
                            // transport-frame fault (the binary/continuation arm
                            // below) degrades to a counted notice and a socket drop;
                            // a refusal must never become a silent redial loop that
                            // walks the productive ladder to the deadline with a
                            // near-empty stream.
                            Err(error) => return Err(error),
                        }
                    }
                    ReadAction::Message(Ok(Some(Ok(Message::Ping(bytes))))) => {
                        let pong_sent = tokio::select! {
                            result = shutdown.wait() => {
                                interrupted = result.map_err(LiveError::Signal)?.interrupted();
                                exit_run = true;
                                false
                            }
                            () = tokio::time::sleep_until(deadline) => {
                                exit_run = true;
                                false
                            },
                            result = socket.send(Message::Pong(bytes)) => result.is_ok(),
                        };
                        if !pong_sent {
                            break;
                        }
                    }
                    ReadAction::Message(Ok(Some(Ok(Message::Pong(_))))) => {}
                    ReadAction::Message(Ok(Some(Ok(Message::Binary(_)))))
                    | ReadAction::Message(Ok(Some(Ok(Message::Frame(_))))) => {
                        // Ruling (global follow-up to E37d): an unexpected
                        // binary/continuation frame is not fatal — a hard error
                        // would cancel every sibling lane sharing the run's
                        // `try_join!` and write no session record. Record one
                        // counted `UnsupportedMessage` notice and drop the socket;
                        // the disconnect record and reconnect ladder below take the
                        // stream honestly disconnected and re-dial. Only start-up
                        // refusals stay fatal.
                        let logical_receipt_ns = clock.elapsed_ns()?;
                        admit_unsupported_message(
                            format!(
                                "unexpected binary/continuation frame on {}",
                                options.stream.as_str()
                            ),
                            logical_receipt_ns,
                            &mut counters,
                            &mut handle,
                        )?;
                        break;
                    }
                    ReadAction::Message(Err(_)) => {
                        counters.read_idle_timeouts =
                            checked_add(counters.read_idle_timeouts, 1, || {
                                LiveError::CounterExhausted
                            })?;
                        break;
                    }
                    ReadAction::Message(Ok(Some(Ok(Message::Close(_)))))
                    | ReadAction::Message(Ok(Some(Err(_))))
                    | ReadAction::Message(Ok(None)) => break,
                }
            }
        }

        // A deadline or controlled shutdown ends the run without manufacturing
        // a transport failure. Socket close/error/reconnect paths still admit a
        // Disconnected record before retrying.
        if !admits_transport_disconnect(exit_run) {
            break 'run;
        }
        ordinal = checked_add(ordinal, 1, || LiveError::CounterExhausted)?;
        let logical_receipt_ns = clock.elapsed_ns()?;
        dispatch(
            &mut adapter,
            disconnected_record(
                &clock,
                options.stream,
                connection_epoch,
                ordinal,
                logical_receipt_ns,
            )?,
            &clock,
            logical_receipt_ns,
            coinbase_rest.as_ref(),
            kraken_reconciler.as_mut(),
            &mut counters,
            &mut handle,
        )
        .await?;
        // Two redial ladders, never zero (ruling E41 audit, MAJOR 1): a
        // connection that delivered a frame and then dropped — a subscription
        // error, a `status: error`, a rate-limit kick — is NOT "unproductive", so
        // it resets that streak; but it must still be paced, or an
        // answer-then-close venue becomes an unthrottled reconnect storm up to
        // the 100 000-attempt cap. It is paced by the pre-commit reconnect ladder
        // (500 ms → 8 s, keyed on the cumulative attempt), so Coinbase/Kraken
        // productive-drop timing is byte-identical to before this range. A
        // connection that opened and said nothing walks the unproductive ladder
        // (1 s → 60 s) instead.
        let delay = if connection_productive {
            unproductive_streak = 0;
            reconnect_delay(counters.connection_attempts)
        } else {
            unproductive_streak = unproductive_streak.saturating_add(1);
            unproductive_redial_delay(unproductive_streak, options.stream)
        };
        match wait_before_redial(delay, deadline, &mut shutdown).await? {
            ReconnectWait::Retry => {
                counters.reconnects =
                    checked_add(counters.reconnects, 1, || LiveError::CounterExhausted)?;
            }
            ReconnectWait::Deadline => break,
            ReconnectWait::Interrupted => {
                interrupted = true;
                break;
            }
        }
    }

    if counters.successful_connections == 0 && !interrupted {
        return Err(LiveError::NoConnection);
    }
    if !interrupted && options.diagnostics.induce_coinbase_trade_gap_once {
        if counters.induced_coinbase_trade_drops != 1 {
            return Err(LiveError::DiagnosticIncomplete(
                "no eligible second Coinbase match was observed".to_owned(),
            ));
        }
        if counters.coinbase_trade_backfills == 0 {
            return Err(LiveError::DiagnosticIncomplete(
                "the induced Coinbase loss did not trigger a recovery action".to_owned(),
            ));
        }
    }
    if !interrupted && options.diagnostics.induced_reconnect_interval.is_some() {
        if counters.induced_disconnects == 0 || counters.successful_connections < 2 {
            return Err(LiveError::DiagnosticIncomplete(
                "the run did not complete an induced disconnect and reconnect".to_owned(),
            ));
        }
        if options.stream == CaptureStream::KrakenLevel3 && counters.kraken_tokens_created < 2 {
            return Err(LiveError::DiagnosticIncomplete(
                "authenticated Kraken reconnect did not create at least two fresh tokens"
                    .to_owned(),
            ));
        }
    }
    Ok(LiveReport {
        records: counters.records,
        events: counters.events,
        notices: counters.notices,
        recoveries: counters.recoveries,
        coinbase_trade_backfills: counters.coinbase_trade_backfills,
        connection_attempts: counters.connection_attempts,
        successful_connections: counters.successful_connections,
        reconnects: counters.reconnects,
        read_idle_timeouts: counters.read_idle_timeouts,
        induced_coinbase_trade_drops: counters.induced_coinbase_trade_drops,
        induced_disconnects: counters.induced_disconnects,
        kraken_tokens_created: counters.kraken_tokens_created,
        interrupted,
        kraken_trade_reconciliation: kraken_reconciler.map(KrakenTradeReconciler::into_summary),
    })
}

// --- Live http_poll runner (ruling E41) ------------------------------------
//
// An `http_poll` stream is not a socket: `run_live` dispatches these through the
// SAME per-endpoint poll loops the capture path uses (`BybitRestSession`,
// `BinanceUsdmRestSession`), so a REST poll identity is never dialled as a
// websocket. A poll feed has no handshake to fail: one connection epoch is
// opened up front, every request rides it as `OutboundText` + (`InboundText` |
// `Failed`), and a served poll is what proves the connection — so a run that
// serves at least one poll satisfies the connection-deadline check, and a run
// that serves none ends `NoConnection` (transient exit-75), never a silent
// exit-0 (M5). Each record is admitted through the same deterministic adapter a
// replay uses, so live and replay agree byte for byte.

/// Admit one poll record through the replay adapter, count it, and publish any
/// resulting outcome. The shared count/forward step for the http_poll path.
fn admit_rest_record<H>(
    adapter: &mut ReplayAdapter,
    record: &RawRecord,
    logical_receipt_ns: u64,
    counters: &mut LiveCounters,
    handle: &mut H,
) -> Result<(), LiveError>
where
    H: FnMut(TimedIngestOutcome) -> Result<LiveControl, LiveError>,
{
    counters.records = checked_add(counters.records, 1, || LiveError::CounterExhausted)?;
    let outcome = adapter.ingest(record)?;
    count_outcome(counters, &outcome)?;
    if outcome.events.is_empty() && outcome.notices.is_empty() {
        return Ok(());
    }
    handle(TimedIngestOutcome {
        outcome,
        logical_receipt_ns,
    })?;
    Ok(())
}

/// Build the response record one served/failed poll admits: a served body as
/// `InboundText`, or a schema-4 `Failed` record.
fn rest_response_record(
    clock: &ProcessClock,
    stream: CaptureStream,
    connection_epoch: u64,
    ordinal: u64,
    record: RestResponseRecord,
    receipt_ns: u64,
) -> Result<RawRecord, LiveError> {
    Ok(match record {
        RestResponseRecord::Inbound(body) => text_record(
            clock,
            stream,
            connection_epoch,
            ordinal,
            RecordKind::InboundText,
            body,
            receipt_ns,
        )?,
        RestResponseRecord::Failed { kind, detail } => RawRecord::failed_at(
            stream,
            connection_epoch,
            ordinal,
            kind,
            &detail,
            clock.receipt_time_text(receipt_ns)?,
            receipt_ns,
        ),
    })
}

/// Dispatch one live `http_poll` run to its per-venue poll driver over the
/// production reqwest transport. Only the two adopted REST identities have a live
/// runner; any other `http_poll` stream (the oracle feed, the NBA crawls) is
/// driven elsewhere or is capture-only, and is refused here before any request.
async fn run_live_http_poll<H>(
    options: LiveOptions,
    clock: ProcessClock,
    handle: H,
) -> Result<LiveReport, LiveError>
where
    H: FnMut(TimedIngestOutcome) -> Result<LiveControl, LiveError>,
{
    match options.stream {
        CaptureStream::BybitLinearRest => {
            let session = BybitRestSession::reqwest(Instant::now())?;
            run_live_bybit_rest(options, clock, session, handle).await
        }
        CaptureStream::BinanceUsdmRest => {
            let session = BinanceUsdmRestSession::reqwest(Instant::now())?;
            run_live_binance_rest(options, clock, session, handle).await
        }
        CaptureStream::PythHermesStream => run_live_pyth_hermes(options, clock, handle).await,
        other => Err(LiveError::InvalidOptions(format!(
            "{} has no live http_poll runner",
            other.as_str()
        ))),
    }
}

/// Drive one live Bybit linear REST poll run to its deadline through the shared
/// [`BybitRestSession`] (the same machine the capture loop drives), publishing
/// every normalized outcome through `handle`. The session is injected so a test
/// passes a fixture transport; production builds it over the reqwest client.
async fn run_live_bybit_rest<H>(
    options: LiveOptions,
    clock: ProcessClock,
    mut session: BybitRestSession<'_>,
    mut handle: H,
) -> Result<LiveReport, LiveError>
where
    H: FnMut(TimedIngestOutcome) -> Result<LiveControl, LiveError>,
{
    const EPOCH: u64 = 1;
    let mut adapter = ReplayAdapter::new(
        options.stream,
        options.publisher_epoch.clone(),
        KrakenBookDepths {
            btc: options.kraken_btc_book_depth,
            usdt: KRAKEN_USDT_BOOK_DEPTH,
        },
    )?;
    let deadline = clock.deadline(options.duration);
    let mut shutdown = ShutdownWait::new(options.shutdown.clone());
    let mut counters = LiveCounters::default();
    let mut ordinal = 0_u64;
    let mut interrupted = false;
    let mut connected = false;

    // Open the single poll-session epoch up front, so `Failed` records have an
    // epoch and the consumer sees the session open; the connection is proven by
    // the first served poll below, not by opening the epoch.
    counters.connection_attempts = checked_add(counters.connection_attempts, 1, || {
        LiveError::CounterExhausted
    })?;
    ordinal = checked_add(ordinal, 1, || LiveError::CounterExhausted)?;
    let ns = clock.elapsed_ns()?;
    let opened = connected_record(&clock, options.stream, EPOCH, ordinal, ns, ns)?;
    admit_rest_record(&mut adapter, &opened, ns, &mut counters, &mut handle)?;

    'run: while Instant::now() < deadline {
        let (due, wait_until) = session.next_poll();
        tokio::select! {
            result = shutdown.wait() => {
                interrupted = result.map_err(LiveError::Signal)?.interrupted();
                break 'run;
            }
            () = tokio::time::sleep_until(deadline) => break 'run,
            () = tokio::time::sleep_until(wait_until) => {}
        }
        if let Some(wait) = session.spacing_wait() {
            tokio::select! {
                result = shutdown.wait() => {
                    interrupted = result.map_err(LiveError::Signal)?.interrupted();
                    break 'run;
                }
                () = tokio::time::sleep_until(deadline) => break 'run,
                () = sleep(wait) => {}
            }
        }
        session.mark_requested();

        ordinal = checked_add(ordinal, 1, || LiveError::CounterExhausted)?;
        let ns = clock.elapsed_ns()?;
        let outbound = text_record(
            &clock,
            options.stream,
            EPOCH,
            ordinal,
            RecordKind::OutboundText,
            session.request_url(due).to_owned(),
            ns,
        )?;
        admit_rest_record(&mut adapter, &outbound, ns, &mut counters, &mut handle)?;

        let outcome = tokio::select! {
            result = shutdown.wait() => {
                interrupted = result.map_err(LiveError::Signal)?.interrupted();
                break 'run;
            }
            () = tokio::time::sleep_until(deadline) => break 'run,
            outcome = session.fetch(due) => outcome,
        };
        let resolved = session.resolve(due, outcome, "live");
        if resolved.served && !connected {
            connected = true;
            counters.successful_connections =
                checked_add(counters.successful_connections, 1, || {
                    LiveError::CounterExhausted
                })?;
        }
        ordinal = checked_add(ordinal, 1, || LiveError::CounterExhausted)?;
        let ns = clock.elapsed_ns()?;
        let record =
            rest_response_record(&clock, options.stream, EPOCH, ordinal, resolved.record, ns)?;
        admit_rest_record(&mut adapter, &record, ns, &mut counters, &mut handle)?;
        if let Some(line) = resolved.log {
            eprintln!("{line}");
        }
    }

    if counters.successful_connections == 0 && !interrupted {
        return Err(LiveError::NoConnection);
    }
    Ok(build_poll_report(&counters, interrupted))
}

/// Drive one live Binance USD-M REST poll run to its deadline through the shared
/// [`BinanceUsdmRestSession`], publishing every normalized outcome through
/// `handle`. Structurally identical to the capture loop — a one-second base tick,
/// the three per-endpoint cadences, the shared budget and the IP-wide/per-endpoint
/// ladders — with the replay adapter as the sink instead of the recorder.
async fn run_live_binance_rest<H>(
    options: LiveOptions,
    clock: ProcessClock,
    mut session: BinanceUsdmRestSession<'_>,
    mut handle: H,
) -> Result<LiveReport, LiveError>
where
    H: FnMut(TimedIngestOutcome) -> Result<LiveControl, LiveError>,
{
    const EPOCH: u64 = 1;
    let mut adapter = ReplayAdapter::new(
        options.stream,
        options.publisher_epoch.clone(),
        KrakenBookDepths {
            btc: options.kraken_btc_book_depth,
            usdt: KRAKEN_USDT_BOOK_DEPTH,
        },
    )?;
    let deadline = clock.deadline(options.duration);
    let mut shutdown = ShutdownWait::new(options.shutdown.clone());
    let mut counters = LiveCounters::default();
    let mut ordinal = 0_u64;
    let mut interrupted = false;
    let mut connected = false;

    counters.connection_attempts = checked_add(counters.connection_attempts, 1, || {
        LiveError::CounterExhausted
    })?;
    ordinal = checked_add(ordinal, 1, || LiveError::CounterExhausted)?;
    let ns = clock.elapsed_ns()?;
    let opened = connected_record(&clock, options.stream, EPOCH, ordinal, ns, ns)?;
    admit_rest_record(&mut adapter, &opened, ns, &mut counters, &mut handle)?;

    let mut interval = tokio::time::interval(Duration::from_secs(1));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    'run: while Instant::now() < deadline {
        tokio::select! {
            result = shutdown.wait() => {
                interrupted = result.map_err(LiveError::Signal)?.interrupted();
                break 'run;
            }
            () = tokio::time::sleep_until(deadline) => break 'run,
            _ = interval.tick() => {}
        }
        let elapsed_ms = session.elapsed_ms()?;
        if session.is_paused(elapsed_ms) {
            continue;
        }
        for index in 0..BinanceUsdmRestSession::ENDPOINTS.len() {
            if session.is_paused(elapsed_ms) {
                break;
            }
            if !session.endpoint_due(index, elapsed_ms) {
                continue;
            }
            if !session.claim_budget(elapsed_ms) {
                continue;
            }
            let descriptor = session.begin_poll(index)?;
            ordinal = checked_add(ordinal, 1, || LiveError::CounterExhausted)?;
            let ns = clock.elapsed_ns()?;
            let outbound = text_record(
                &clock,
                options.stream,
                EPOCH,
                ordinal,
                RecordKind::OutboundText,
                descriptor,
                ns,
            )?;
            admit_rest_record(&mut adapter, &outbound, ns, &mut counters, &mut handle)?;

            let poll = tokio::select! {
                result = shutdown.wait() => {
                    interrupted = result.map_err(LiveError::Signal)?.interrupted();
                    break 'run;
                }
                () = tokio::time::sleep_until(deadline) => break 'run,
                poll = session.fetch(index) => poll,
            };
            let resolved = session.resolve_poll(index, elapsed_ms, poll, "live")?;
            if resolved.served && !connected {
                connected = true;
                counters.successful_connections =
                    checked_add(counters.successful_connections, 1, || {
                        LiveError::CounterExhausted
                    })?;
            }
            if let Some(record) = resolved.record {
                ordinal = checked_add(ordinal, 1, || LiveError::CounterExhausted)?;
                let ns = clock.elapsed_ns()?;
                let record =
                    rest_response_record(&clock, options.stream, EPOCH, ordinal, record, ns)?;
                admit_rest_record(&mut adapter, &record, ns, &mut counters, &mut handle)?;
            }
            if let Some(line) = resolved.log {
                eprintln!("{line}");
            }
        }
    }

    if counters.successful_connections == 0 && !interrupted {
        return Err(LiveError::NoConnection);
    }
    Ok(build_poll_report(&counters, interrupted))
}

/// Drive one live Pyth Hermes BTC/USD Server-Sent-Events run to its deadline over
/// the same keyed HTTPS transport the capture path uses, publishing every
/// normalized outcome through `handle`.
///
/// It mirrors the capture SSE loop's record model exactly so a replay of this
/// run's own capture reproduces its events: one logical connection (epoch 1)
/// spans the whole session — a `Connected` record opens it, the pinned outbound
/// request descriptor is admitted once, and every SSE event block that follows
/// rides that epoch as an `InboundText`. The loop never opens a second epoch and
/// never emits a `Disconnected`; every fault is a `Failed` record plus a backoff
/// redial under the same epoch (degrade-not-abort).
///
/// DEGRADE-NOT-ABORT: this runner is one arm of the oracle lane's `try_join!`, so
/// a returned `Err` would tear down the sibling Chainlink poll with it. Nothing
/// after start-up ever returns `Err`: a transport fault, a 5xx or other transient
/// non-2xx status, and a read EOF all admit a `Failed` record and redial on the
/// capture path's `reconnect_delay` ladder. A terminal 401/403/404 (a re-key or an
/// id/path change) admits its `Failed` record too — leaving the Pyth identities
/// unavailable with an honest reason — then STOPS the run cleanly (one log line,
/// no reconnect storm) rather than redialing identical refusals; it still returns
/// `Ok`, so the Chainlink poll keeps running. This mirrors the capture path's
/// terminal classification (capture.rs), which there returns `PythHermesTerminal`.
/// A missing or unusable credential at start-up is not fatal either — the stream
/// is OMITTED (an empty report is returned) so the lane runs without it. The
/// credential is a request header, never the URL, and every surfaced error string
/// passes through the credential redactor, exactly as the capture loop does.
async fn run_live_pyth_hermes<H>(
    options: LiveOptions,
    clock: ProcessClock,
    mut handle: H,
) -> Result<LiveReport, LiveError>
where
    H: FnMut(TimedIngestOutcome) -> Result<LiveControl, LiveError>,
{
    const EPOCH: u64 = 1;
    // A missing or unusable credential OMITS the hermes stream rather than failing
    // the run: this runner shares the oracle lane's `try_join!`, so a returned
    // `Err` would tear down the Chainlink poll with it. Log one line and return an
    // empty report so the lane runs poll-only. The auth error is valueless by
    // construction, but it is not printed, to keep the credential nowhere near a log.
    let Ok(credential) = PythHermesCredential::from_env() else {
        eprintln!(
            "oracle lane: pyth hermes stream omitted (no usable credential); running without it"
        );
        return Ok(build_poll_report(&LiveCounters::default(), false));
    };
    let client = crate::secure_http::client().map_err(|_| CaptureError::PythHermesClient)?;
    let url = crate::pyth_hermes::stream_url();
    let mut adapter = ReplayAdapter::new(
        options.stream,
        options.publisher_epoch.clone(),
        KrakenBookDepths::default(),
    )?;
    let deadline = clock.deadline(options.duration);
    let mut shutdown = ShutdownWait::new(options.shutdown.clone());
    let mut counters = LiveCounters::default();
    let mut ordinal = 0_u64;
    let mut interrupted = false;

    // One logical connection spans the whole session (the capture SSE loop's
    // poll-feed pattern): open epoch 1 up front so a later fault still has an
    // epoch to record its `Failed` record under, count it as connected, and
    // admit the pinned outbound descriptor once.
    counters.connection_attempts = checked_add(counters.connection_attempts, 1, || {
        LiveError::CounterExhausted
    })?;
    counters.successful_connections = checked_add(counters.successful_connections, 1, || {
        LiveError::CounterExhausted
    })?;
    ordinal = checked_add(ordinal, 1, || LiveError::CounterExhausted)?;
    let ns = clock.elapsed_ns()?;
    let opened = connected_record(&clock, options.stream, EPOCH, ordinal, ns, ns)?;
    admit_rest_record(&mut adapter, &opened, ns, &mut counters, &mut handle)?;
    ordinal = checked_add(ordinal, 1, || LiveError::CounterExhausted)?;
    let ns = clock.elapsed_ns()?;
    let descriptor = text_record(
        &clock,
        options.stream,
        EPOCH,
        ordinal,
        RecordKind::OutboundText,
        crate::pyth_hermes::request_descriptor(&url),
        ns,
    )?;
    admit_rest_record(&mut adapter, &descriptor, ns, &mut counters, &mut handle)?;

    let mut failure_streak: u64 = 0;
    'run: while Instant::now() < deadline {
        let request = credential
            .authorize(
                client
                    .get(&url)
                    .header(
                        reqwest::header::ACCEPT,
                        reqwest::header::HeaderValue::from_static("text/event-stream"),
                    )
                    .timeout(crate::pyth_hermes::REQUEST_TIMEOUT),
            )
            .map_err(|error| LiveError::InvalidOptions(error.to_string()))?;
        let send = tokio::select! {
            result = shutdown.wait() => {
                interrupted = result.map_err(LiveError::Signal)?.interrupted();
                break 'run;
            }
            () = tokio::time::sleep_until(deadline) => break 'run,
            send = request.send() => send,
        };
        let mut response = match send {
            Ok(response) => response,
            Err(error) => {
                // Transport fault after start-up: a `Failed` record (redacted,
                // bounded cause) and a backoff redial, never a returned error.
                ordinal = checked_add(ordinal, 1, || LiveError::CounterExhausted)?;
                let ns = clock.elapsed_ns()?;
                let detail = credential.redact(&crate::adapter::bounded_token(&error.to_string()));
                let failed = RawRecord::failed_at(
                    options.stream,
                    EPOCH,
                    ordinal,
                    FailureKind::Transport,
                    &detail,
                    clock.receipt_time_text(ns)?,
                    ns,
                );
                admit_rest_record(&mut adapter, &failed, ns, &mut counters, &mut handle)?;
                failure_streak = failure_streak.saturating_add(1);
                match wait_before_redial(reconnect_delay(failure_streak), deadline, &mut shutdown)
                    .await?
                {
                    ReconnectWait::Retry => continue,
                    ReconnectWait::Deadline => break 'run,
                    ReconnectWait::Interrupted => {
                        interrupted = true;
                        break 'run;
                    }
                }
            }
        };
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            ordinal = checked_add(ordinal, 1, || LiveError::CounterExhausted)?;
            let ns = clock.elapsed_ns()?;
            let failed = RawRecord::failed_at(
                options.stream,
                EPOCH,
                ordinal,
                FailureKind::HttpStatus(status),
                "Pyth Hermes SSE connect refused",
                clock.receipt_time_text(ns)?,
                ns,
            );
            admit_rest_record(&mut adapter, &failed, ns, &mut counters, &mut handle)?;
            if hermes_status_is_terminal(status) {
                // 401/403/404 is a re-key or an id/path change, never a transient:
                // redialing would only produce a reconnect storm of identical
                // refusals (~110 per 30-min generation on the ~8 s ceiling). Stop
                // the SSE run — the `Failed` record above already leaves the Pyth
                // identities unavailable with an honest reason — WITHOUT returning
                // an `Err`, which would tear down the sibling Chainlink poll
                // through the shared `try_join!`. The lane keeps running poll-only.
                // capture.rs makes the same status terminal (there, an `Err`).
                eprintln!(
                    "oracle lane: pyth hermes stream stopped, HTTP {status} is terminal (re-key or fix the id/path); running without it"
                );
                break 'run;
            }
            // A 5xx or other transient non-2xx degrades: the `Failed` record above
            // gives honest feed-health and the run redials under the same epoch on
            // the capture path's backoff ladder.
            failure_streak = failure_streak.saturating_add(1);
            match wait_before_redial(reconnect_delay(failure_streak), deadline, &mut shutdown)
                .await?
            {
                ReconnectWait::Retry => continue,
                ReconnectWait::Deadline => break 'run,
                ReconnectWait::Interrupted => {
                    interrupted = true;
                    break 'run;
                }
            }
        }
        failure_streak = 0;

        // Read the SSE byte stream for this connection, framing on the blank-line
        // boundary and admitting each complete event block as one `InboundText`
        // under epoch 1. The per-event and buffer bounds are the capture path's,
        // byte for byte, so an unbounded body cannot grow the buffer.
        let mut buffer: Vec<u8> = Vec::new();
        'read: loop {
            let chunk = tokio::select! {
                result = shutdown.wait() => {
                    interrupted = result.map_err(LiveError::Signal)?.interrupted();
                    break 'run;
                }
                () = tokio::time::sleep_until(deadline) => break 'run,
                chunk = response.chunk() => chunk,
            };
            let chunk = match chunk {
                Ok(Some(bytes)) => bytes,
                // Stream ended (24 h close) or a transport read error: redial.
                Ok(None) | Err(_) => break 'read,
            };
            if buffer.len().saturating_add(chunk.len())
                > crate::pyth_hermes::MAX_SSE_EVENT_BYTES.saturating_mul(2)
            {
                // An event that never terminates within the bound: end the
                // connection rather than grow the buffer.
                break 'read;
            }
            buffer.extend_from_slice(&chunk);
            while let Some(boundary) = find_sse_event_boundary(&buffer) {
                let block: Vec<u8> = buffer.drain(..boundary).collect();
                // Drop the blank-line separator (all leading newlines).
                while buffer.first() == Some(&b'\n') {
                    buffer.remove(0);
                }
                if block.is_empty() {
                    continue;
                }
                if block.len() > crate::pyth_hermes::MAX_SSE_EVENT_BYTES {
                    break 'read;
                }
                // Defence in depth: a provider frame that echoed the header would
                // be scrubbed before it reached the consumer.
                let (redacted, _) = credential.redact_body(&block, false);
                let Ok(text) = String::from_utf8(redacted) else {
                    // A non-UTF-8 SSE body is an unmodelled provider change: end
                    // the connection rather than admit unframable bytes.
                    break 'read;
                };
                ordinal = checked_add(ordinal, 1, || LiveError::CounterExhausted)?;
                let ns = clock.elapsed_ns()?;
                let record = text_record(
                    &clock,
                    options.stream,
                    EPOCH,
                    ordinal,
                    RecordKind::InboundText,
                    text,
                    ns,
                )?;
                admit_rest_record(&mut adapter, &record, ns, &mut counters, &mut handle)?;
            }
        }

        // The connection closed (24 h close or transport end): redial at the
        // backoff floor under the same epoch.
        failure_streak = failure_streak.saturating_add(1);
        match wait_before_redial(reconnect_delay(failure_streak), deadline, &mut shutdown).await? {
            ReconnectWait::Retry => continue,
            ReconnectWait::Deadline => break 'run,
            ReconnectWait::Interrupted => {
                interrupted = true;
                break 'run;
            }
        }
    }

    Ok(build_poll_report(&counters, interrupted))
}

/// Index at which a blank-line SSE event boundary (`\n\n`) begins. A local copy
/// of the capture path's private framing so the live SSE runner keeps identical
/// bounds without widening `capture.rs`'s surface.
fn find_sse_event_boundary(buffer: &[u8]) -> Option<usize> {
    buffer.windows(2).position(|pair| pair == b"\n\n")
}

/// Whether a Pyth Hermes SSE connect status is terminal for the run: a 401/403/404
/// is a re-key or an id/path change that will not clear on retry, so it stops the
/// stream instead of redialing. The same classification `capture.rs` applies.
const fn hermes_status_is_terminal(status: u16) -> bool {
    matches!(status, 401 | 403 | 404)
}

/// Resolve which Kalshi index-socket families the oracle lane arms from a venue
/// allow-list. Pure so the arming gate is unit-testable without mutating process
/// environment: every [`INDEX_SOCKET_TOKENS`] entry the allow-list enables is
/// armed, starting from the empty set.
pub fn oracle_index_sockets_for(enabled: &EnabledVenues) -> ArmedIndexSockets {
    let mut armed = ArmedIndexSockets::NONE;
    for token in INDEX_SOCKET_TOKENS {
        if enabled.is_enabled(token) {
            armed = armed.with(token);
        }
    }
    armed
}

/// Drive the oracle lane's dedicated Kalshi index socket (ruling E37d) by
/// wrapping [`run_prediction_index_socket`], adapting its admission boundary to
/// the live-run `handle`. A disarmed lane (the allow-list names no `cfbrti`/`pyth`
/// index family) opens no socket and returns an empty report — the intended
/// omit-when-disarmed — so the oracle lane degrades to poll + hermes only rather
/// than aborting.
async fn run_live_kalshi_index<H>(
    options: LiveOptions,
    clock: ProcessClock,
    mut handle: H,
) -> Result<LiveReport, LiveError>
where
    H: FnMut(TimedIngestOutcome) -> Result<LiveControl, LiveError>,
{
    let enabled =
        EnabledVenues::from_env().map_err(|error| LiveError::InvalidOptions(error.to_string()))?;
    let armed = oracle_index_sockets_for(&enabled);
    if !armed.any() {
        eprintln!(
            "oracle lane: kalshi index socket disarmed (SIGNALS_ENABLED_VENUES lists no \
             cfbrti/pyth); running poll + hermes only"
        );
    }
    let socket_options = PredictionIndexSocketOptions {
        venue: PredictionMarketVenue::Kalshi,
        duration: options.duration,
        publisher_epoch: options.publisher_epoch.clone(),
        index_sockets: armed,
        shutdown: options.shutdown.clone(),
    };
    let report = run_prediction_index_socket(socket_options, clock, |admission| {
        adapt_index_admission(admission, &mut handle)
    })
    .await
    .map_err(map_prediction_error)?;
    Ok(prediction_report_to_live(&report))
}

/// Adapt one [`PredictionLiveAdmission`] the index-socket runner offers into a
/// [`TimedIngestOutcome`] for the live-run `handle`. The lane handle's `Ok`
/// (either [`LiveControl`]) maps to `Ok(())`; its `Err` rides back as
/// [`PredictionLiveError::Live`] so the wrapper can unwrap it unchanged.
fn adapt_index_admission<H>(
    admission: PredictionLiveAdmission,
    handle: &mut H,
) -> Result<(), PredictionLiveError>
where
    H: FnMut(TimedIngestOutcome) -> Result<LiveControl, LiveError>,
{
    let timed = match admission {
        PredictionLiveAdmission::MarketOutcome {
            outcome, now_ns, ..
        } => TimedIngestOutcome {
            outcome,
            logical_receipt_ns: now_ns,
        },
        PredictionLiveAdmission::Connectivity { transition, now_ns } => {
            let (code, detail) = match transition {
                PredictionSocketTransition::Connected => {
                    (FeedNoticeCode::Connected, "kalshi index socket connected")
                }
                PredictionSocketTransition::Disconnected => (
                    FeedNoticeCode::Disconnected,
                    "kalshi index socket disconnected",
                ),
                PredictionSocketTransition::Reconnecting => (
                    FeedNoticeCode::Disconnected,
                    "kalshi index socket reconnecting",
                ),
            };
            TimedIngestOutcome {
                outcome: IngestOutcome::notice(FeedNotice::info(code, detail)),
                logical_receipt_ns: now_ns,
            }
        }
        // Discovery/Bind/Unavailable are never emitted by the index-socket runner
        // (it binds no market): match them so the adapter is exhaustive, no-op.
        PredictionLiveAdmission::Discovery { .. }
        | PredictionLiveAdmission::Bind { .. }
        | PredictionLiveAdmission::Unavailable { .. } => return Ok(()),
    };
    match handle(timed) {
        Ok(_) => Ok(()),
        Err(error) => Err(PredictionLiveError::Live(error)),
    }
}

/// Map a [`PredictionLiveReport`] onto a [`LiveReport`], copying the record,
/// event, notice and connection totals and filling the market-only fields with
/// defaults, as the REST runners do.
fn prediction_report_to_live(report: &PredictionLiveReport) -> LiveReport {
    LiveReport {
        records: report.records,
        events: report.events,
        notices: report.notices,
        recoveries: 0,
        coinbase_trade_backfills: 0,
        connection_attempts: report.connection_attempts,
        successful_connections: report.successful_connections,
        reconnects: report.reconnects,
        read_idle_timeouts: report.read_idle_timeouts,
        induced_coinbase_trade_drops: 0,
        induced_disconnects: 0,
        kraken_tokens_created: 0,
        interrupted: report.interrupted,
        kraken_trade_reconciliation: None,
    }
}

/// Map a [`PredictionLiveError`] back onto a [`LiveError`]: option, auth and
/// attempt-limit failures become [`LiveError::InvalidOptions`]; a wrapped
/// [`PredictionLiveError::Live`] unwraps to the original [`LiveError`]; anything
/// else rides the existing blanket conversion.
fn map_prediction_error(error: PredictionLiveError) -> LiveError {
    match error {
        PredictionLiveError::Live(inner) => inner,
        other @ (PredictionLiveError::InvalidOptions(_)
        | PredictionLiveError::KalshiAuth(_)
        | PredictionLiveError::ConnectionAttemptLimit) => {
            LiveError::InvalidOptions(other.to_string())
        }
        other => LiveError::from(other),
    }
}

/// Whether `run_live` (or `run_live_http_poll`) has a concrete runner for
/// `stream`: a pure classification with no dial and no network. It is kept in
/// lockstep with the `run_live` `KalshiIndex` early return and the
/// `run_live_http_poll` match, and returns `false` only for the streams that
/// still fall through to an unsupported/refusal error — the prediction-market
/// streams `run_live` rejects in validation, and the `http_poll` streams with no
/// concrete runner.
pub fn stream_has_live_runner(stream: CaptureStream) -> bool {
    match stream {
        // WebSocket streams `run_live` dials and subscribes generically, plus the
        // Kalshi index socket served by the dedicated `run_live_kalshi_index`
        // early return.
        CaptureStream::BinanceSpot
        | CaptureStream::BinanceUsdmPublic
        | CaptureStream::BybitSpot
        | CaptureStream::BybitLinear
        | CaptureStream::CoinbaseCore
        | CaptureStream::CoinbaseLevel3
        | CaptureStream::CoinbaseMechanisms
        | CaptureStream::KrakenCore
        | CaptureStream::KrakenLevel3
        | CaptureStream::KrakenUsdtReference
        | CaptureStream::PolymarketChainlinkTwap
        | CaptureStream::KalshiIndex => true,
        // `http_poll` streams with a concrete `run_live_http_poll` arm.
        CaptureStream::BybitLinearRest
        | CaptureStream::BinanceUsdmRest
        | CaptureStream::PythHermesStream => true,
        // Prediction-market streams `run_live` refuses in validation, and the
        // `http_poll` streams that fall through to the "no live http_poll runner"
        // refusal.
        CaptureStream::KalshiMarket
        | CaptureStream::PolymarketMarket
        | CaptureStream::ChainlinkAggregator
        | CaptureStream::NbaEspnDaily
        | CaptureStream::NbaEspnGames
        | CaptureStream::NbaEspnStatus
        | CaptureStream::PythHermesRest
        | CaptureStream::SportsDataIoNba => false,
    }
}

// --- Chainlink on-chain poll driver (A5 phase 1) ---------------------------
//
// The reusable live `http_poll` path for `chainlink.ethereum.btc-usd.rounds`.
// The generic venue lane (A6) calls this; it owns journaling and checkpoints.
// This driver owns the transport (per-request `REQUEST_TIMEOUT`, reply bounded
// by `MAX_RESPONSE_BYTES` before parsing) and R9 multi-endpoint failover.
//
// Every operational event is a `RawRecord` admitted through the same
// `ChainlinkAdapter` a replay uses, so every notice the consumer sees is
// reproduced byte-for-byte on replay (M3): a poll session is a `Connected`
// epoch; a served poll is `OutboundText` + `InboundText`; ANY fault ends the
// epoch with `Disconnected`, rotates to the next endpoint, backs off, and
// reconnects (a new `Connected` epoch) — so a `Disconnected` is the down-notice
// and the following `Connected` is the up-notice. The run never exits on a poll
// fault; only a decoded start-up target mismatch is fatal.

/// Configuration for one Chainlink on-chain poll run.
#[derive(Debug, Clone)]
pub struct ChainlinkPollOptions {
    /// Ordered RPC endpoint list. R9: at least two entries, every entry
    /// `https://` (`chainlink::configured_rpc_endpoints` builds a valid list).
    pub endpoints: Vec<String>,
    /// Run duration; 1 second to 7 days, matching the WebSocket runner.
    pub duration: Duration,
    /// Publisher-epoch label stamped on emitted events; 1 to 128 bytes.
    pub publisher_epoch: String,
    /// Shared cancellation receiver for a coordinated run. `None` polls the
    /// operating-system stop signal itself.
    pub shutdown: Option<LiveShutdown>,
}

fn validate_poll_options(options: &ChainlinkPollOptions) -> Result<(), LiveError> {
    if options.duration.is_zero() || options.duration > MAX_RUN_DURATION {
        return Err(LiveError::InvalidOptions(format!(
            "duration must be between 1 second and {} seconds",
            MAX_RUN_DURATION.as_secs()
        )));
    }
    if options.publisher_epoch.trim().is_empty() || options.publisher_epoch.len() > 128 {
        return Err(LiveError::InvalidOptions(
            "publisher_epoch must contain 1 to 128 bytes".to_owned(),
        ));
    }
    // The same rule set the environment parser applies (host present, https,
    // distinct after normalisation), so options built in code cannot pass a
    // list the environment path would refuse.
    crate::chainlink::validate_endpoint_list(&options.endpoints)
        .map_err(|reason| LiveError::InvalidOptions(format!("RPC endpoints {reason}")))?;
    Ok(())
}

/// Admit one record through the adapter and publish any outcome, counting it.
fn admit_poll_record<H>(
    adapter: &mut crate::chainlink::ChainlinkAdapter,
    record: &RawRecord,
    logical_receipt_ns: u64,
    counters: &mut LiveCounters,
    handle: &mut H,
) -> Result<LiveControl, LiveError>
where
    H: FnMut(TimedIngestOutcome) -> Result<LiveControl, LiveError>,
{
    counters.records = checked_add(counters.records, 1, || LiveError::CounterExhausted)?;
    let outcome = adapter.ingest(record)?;
    count_outcome(counters, &outcome)?;
    if outcome.events.is_empty() && outcome.notices.is_empty() {
        return Ok(LiveControl::Continue);
    }
    handle(TimedIngestOutcome {
        outcome,
        logical_receipt_ns,
    })
}

/// Production entry: build the shared bounded/timed transport and run. Wraps
/// [`run_live_poll`] over the reqwest transport A5 built; both share the one
/// [`ChainlinkPollSession`] state machine.
pub async fn run_live_poll_reqwest<H>(
    options: ChainlinkPollOptions,
    clock: ProcessClock,
    handle: H,
) -> Result<LiveReport, LiveError>
where
    H: FnMut(TimedIngestOutcome) -> Result<LiveControl, LiveError>,
{
    let client = crate::secure_http::client()
        .map_err(|_| LiveError::Capture(CaptureError::ChainlinkClient))?;
    run_live_poll(
        options,
        clock,
        move |endpoint: String, body: String| {
            let client = client.clone();
            async move { crate::capture::chainlink_poll(&client, &endpoint, &body).await }
        },
        handle,
    )
    .await
}

/// Drive one live Chainlink poll run to its deadline, publishing every
/// normalized outcome through `handle`. This is a thin loop over the one
/// [`ChainlinkPollSession`] state machine (ruling E39(c) M3): the session does
/// the start-up verification, failover, round cursor and reconnect-epoch work
/// tick by tick; this owns only the cadence, the shutdown/deadline races, and
/// the transient/mismatch exit. The A6 oracle lane drives the same session, so
/// these tests exercise the shipped path. `poll(endpoint, body)` executes one
/// `eth_call` POST and returns the verbatim UTF-8 body on a usable 2xx response,
/// or `None` for any transport, timeout, status, size, or encoding failure
/// (injected so failover is testable without a network).
pub async fn run_live_poll<'p, P, Fut, H>(
    options: ChainlinkPollOptions,
    clock: ProcessClock,
    mut poll: P,
    mut handle: H,
) -> Result<LiveReport, LiveError>
where
    P: FnMut(String, String) -> Fut + 'p,
    Fut: std::future::Future<Output = Option<String>> + 'p,
    H: FnMut(TimedIngestOutcome) -> Result<LiveControl, LiveError>,
{
    let duration = options.duration;
    let mut shutdown = ShutdownWait::new(options.shutdown.clone());
    let transport: ChainlinkPollTransport<'p> =
        Box::new(move |endpoint, body| Box::pin(poll(endpoint, body)));
    let mut session = ChainlinkPollSession::new(options, clock, transport)?;
    let deadline = session.clock.deadline(duration);
    let mut interrupted = false;

    // The first tick runs start-up + the first round immediately (no cadence
    // wait), exactly as A5's up-front start-up did; later ticks wait one cadence.
    let mut interval = tokio::time::interval(crate::chainlink::POLL_CADENCE);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut first = true;
    loop {
        if !first {
            tokio::select! {
                result = shutdown.wait() => {
                    interrupted = result.map_err(LiveError::Signal)?.interrupted();
                    break;
                }
                () = tokio::time::sleep_until(deadline) => break,
                _ = interval.tick() => {}
            }
        }
        first = false;
        // Race the tick against shutdown/deadline so a coordinated stop or the
        // deadline ends the run even mid-tick (the session is not reused after).
        tokio::select! {
            result = shutdown.wait() => {
                interrupted = result.map_err(LiveError::Signal)?.interrupted();
                break;
            }
            () = tokio::time::sleep_until(deadline) => break,
            step = session.poll_tick(&mut handle) => {
                step?;
            }
        }
    }

    // No endpoint ever verified for the whole run: mark the source down with a
    // Disconnected record and exit transient (75), never exit 0 doing nothing
    // (M5). A verified run that later lost every endpoint keeps reconnecting to
    // the deadline and exits cleanly, exactly as before.
    if !interrupted && !session.verified() {
        session.mark_down(&mut handle)?;
        return Err(LiveError::NoConnection);
    }
    Ok(build_poll_report(&session.counters, interrupted))
}

fn build_poll_report(counters: &LiveCounters, interrupted: bool) -> LiveReport {
    LiveReport {
        records: counters.records,
        events: counters.events,
        notices: counters.notices,
        recoveries: counters.recoveries,
        coinbase_trade_backfills: 0,
        connection_attempts: counters.connection_attempts,
        successful_connections: counters.successful_connections,
        reconnects: counters.reconnects,
        read_idle_timeouts: counters.read_idle_timeouts,
        induced_coinbase_trade_drops: 0,
        induced_disconnects: 0,
        kraken_tokens_created: 0,
        interrupted,
        kraken_trade_reconciliation: None,
    }
}

// --- Chainlink poll state machine (A5 transport, shared with the A6 lane) ----
//
// One [`ChainlinkPollSession`] is THE Chainlink on-chain poll state machine.
// [`run_live_poll`] above is a thin loop over it, and the A6 oracle lane drives
// the same session one cadence tick at a time. It owns A5's transport (the
// per-request timeout and `MAX_RESPONSE_BYTES` bound live inside the injected
// [`ChainlinkPollTransport`]), start-up target verification with rotation, the
// E29 endpoint list with a round cursor across failover, and reconnect-on-fault
// epoch handling. What it does NOT own is the loop or the shutdown/deadline
// races — the caller does — so the same machine serves both consumers.

/// One start-up target check: a call selector and its reply classifier.
type StartupCheck = (&'static str, fn(&str) -> crate::chainlink::TargetCheck);

/// One injected `eth_call` transport: `(endpoint, body) -> reply`. Returns the
/// verbatim UTF-8 body on a usable 2xx response, or `None` for any transport,
/// timeout, status, size, or encoding failure — a fault is a `None`, so failover
/// is testable without a network. Carries a lifetime so a borrowing test
/// transport (and the reqwest transport, which is `'static`) both fit.
pub type ChainlinkPollTransport<'t> =
    Box<dyn FnMut(String, String) -> Pin<Box<dyn Future<Output = Option<String>> + 't>> + 't>;

/// The result of one [`ChainlinkPollSession::poll_tick`]. The events and notices
/// this tick produced are delivered through the caller's `handle`; this reports
/// only the two lane-facing facts.
#[derive(Debug, Default, Clone, Copy)]
pub struct ChainlinkPollStep {
    /// Whether the oracle feed is live after this tick: `true` once start-up
    /// verification has completed and the round poll was served; `false` during
    /// verification and on any fault (which rotates to the next endpoint).
    pub connected: bool,
    /// Whether start-up target verification has completed. Once `true` it stays
    /// `true`; a run whose verification never completes is a whole-run outage.
    pub verified: bool,
}

/// The one Chainlink on-chain poll state machine, advanced one cadence tick at a
/// time. Drives A5's transport and adapter; the caller owns the loop.
pub struct ChainlinkPollSession<'t> {
    endpoints: Vec<String>,
    endpoint_index: usize,
    adapter: crate::chainlink::ChainlinkAdapter,
    clock: ProcessClock,
    epoch: u64,
    ordinal: u64,
    request_id: u64,
    epoch_open: bool,
    verified: bool,
    counters: LiveCounters,
    transport: ChainlinkPollTransport<'t>,
}

impl<'t> ChainlinkPollSession<'t> {
    /// The capture stream every record rides.
    const STREAM: CaptureStream = CaptureStream::ChainlinkAggregator;

    /// Build a session from validated options, a clock, and an injected transport
    /// (tests pass a fixture transport; production uses [`Self::reqwest`]).
    pub fn new(
        options: ChainlinkPollOptions,
        clock: ProcessClock,
        transport: ChainlinkPollTransport<'t>,
    ) -> Result<Self, LiveError> {
        validate_poll_options(&options)?;
        let adapter = crate::chainlink::ChainlinkAdapter::new(options.publisher_epoch.clone())?;
        Ok(Self {
            endpoints: options.endpoints,
            endpoint_index: 0,
            adapter,
            clock,
            epoch: 0,
            ordinal: 0,
            request_id: 0,
            epoch_open: false,
            verified: false,
            counters: LiveCounters::default(),
            transport,
        })
    }

    /// Build a session over the shared bounded/timed reqwest transport A5 built —
    /// the same one [`run_live_poll_reqwest`] uses. A client-build failure is
    /// [`LiveError::Capture`], which A5 classifies as a transient (exit-75).
    pub fn reqwest(
        options: ChainlinkPollOptions,
        clock: ProcessClock,
    ) -> Result<ChainlinkPollSession<'static>, LiveError> {
        let client = crate::secure_http::client()
            .map_err(|_| LiveError::Capture(CaptureError::ChainlinkClient))?;
        let transport: ChainlinkPollTransport<'static> = Box::new(move |endpoint, body| {
            let client = client.clone();
            Box::pin(async move { crate::capture::chainlink_poll(&client, &endpoint, &body).await })
        });
        ChainlinkPollSession::new(options, clock, transport)
    }

    /// The steady poll cadence (A5's `POLL_CADENCE`).
    #[must_use]
    pub fn poll_interval(&self) -> Duration {
        crate::chainlink::POLL_CADENCE
    }

    /// Whether start-up target verification has completed.
    #[must_use]
    pub fn verified(&self) -> bool {
        self.verified
    }

    /// Advance one cadence tick. The first tick opens epoch 1, runs the full
    /// start-up verification eagerly (decimals then description, rotating on a
    /// fault, bounded by the endpoint count) and, if it verifies, the first round
    /// poll — so the first event lands in the first tick (M1). Later ticks poll
    /// one round, rotating within the tick on a fault. A decoded target mismatch
    /// is fatal ([`LiveError::TargetMismatch`], exit-78).
    pub async fn poll_tick<H>(&mut self, handle: &mut H) -> Result<ChainlinkPollStep, LiveError>
    where
        H: FnMut(TimedIngestOutcome) -> Result<LiveControl, LiveError>,
    {
        // Epoch 1 opens up front so the start-up calls are recorded within a
        // connection epoch (M4/4.5): a run that later aborts on a mismatch still
        // leaves the Connected + Outbound/Inbound evidence in the journal.
        if self.epoch == 0 {
            self.open_epoch(handle)?;
        }
        if !self.verified {
            self.run_startup(handle).await?;
            if !self.verified {
                return Ok(ChainlinkPollStep {
                    connected: false,
                    verified: false,
                });
            }
        }
        let connected = self.poll_round(handle).await?;
        Ok(ChainlinkPollStep {
            connected,
            verified: true,
        })
    }

    /// Mark the source down with a Disconnected record — the whole-run outage
    /// notice A5 writes before exiting transient (M5).
    pub fn mark_down<H>(&mut self, handle: &mut H) -> Result<(), LiveError>
    where
        H: FnMut(TimedIngestOutcome) -> Result<LiveControl, LiveError>,
    {
        if self.epoch_open {
            self.close_epoch(handle)?;
        }
        Ok(())
    }

    /// Eager start-up verification: `decimals()` then `description()`, each
    /// admitted as Outbound + Inbound through the adapter (M2/4.5), rotating to
    /// the next endpoint on a transient or absent reply, bounded by the endpoint
    /// count so one tick cannot spin forever. A verified pair flips `verified`
    /// and counts one successful connection; a decoded mismatch is fatal.
    async fn run_startup<H>(&mut self, handle: &mut H) -> Result<(), LiveError>
    where
        H: FnMut(TimedIngestOutcome) -> Result<LiveControl, LiveError>,
    {
        let checks: [StartupCheck; 2] = [
            (
                crate::chainlink::DECIMALS_SELECTOR,
                crate::chainlink::verify_decimals_reply,
            ),
            (
                crate::chainlink::DESCRIPTION_SELECTOR,
                crate::chainlink::verify_description_reply,
            ),
        ];
        for (selector, verify) in checks {
            let mut verified_check = false;
            for _ in 0..self.endpoints.len() {
                self.request_id = checked_add(self.request_id, 1, || LiveError::CounterExhausted)?;
                let body = crate::chainlink::eth_call_body(selector, self.request_id);
                self.admit_text(RecordKind::OutboundText, body.clone(), handle)?;
                match self.call(body).await {
                    Some(payload) => {
                        self.admit_text(RecordKind::InboundText, payload.clone(), handle)?;
                        match verify(&payload) {
                            crate::chainlink::TargetCheck::Verified => {
                                verified_check = true;
                                break;
                            }
                            crate::chainlink::TargetCheck::Mismatch(reason) => {
                                return Err(LiveError::TargetMismatch(reason));
                            }
                            crate::chainlink::TargetCheck::Transient(_) => self.rotate(),
                        }
                    }
                    None => {
                        self.counters.read_idle_timeouts =
                            checked_add(self.counters.read_idle_timeouts, 1, || {
                                LiveError::CounterExhausted
                            })?;
                        self.rotate();
                    }
                }
            }
            if !verified_check {
                // This tick could not verify this check on any endpoint; the
                // caller reports the tick unverified and retries next cadence.
                return Ok(());
            }
        }
        self.verified = true;
        self.counters.successful_connections =
            checked_add(self.counters.successful_connections, 1, || {
                LiveError::CounterExhausted
            })?;
        Ok(())
    }

    /// One steady round poll. Reopens the epoch first if a prior fault closed it
    /// (a reconnect), then admits the round; a served reply is live, a fault ends
    /// the epoch with a Disconnected record, rotates, and reports it down. The
    /// persistent adapter's round cursor skips a stale round after a rotation.
    async fn poll_round<H>(&mut self, handle: &mut H) -> Result<bool, LiveError>
    where
        H: FnMut(TimedIngestOutcome) -> Result<LiveControl, LiveError>,
    {
        if !self.epoch_open {
            self.reopen_epoch(handle)?;
        }
        self.request_id = checked_add(self.request_id, 1, || LiveError::CounterExhausted)?;
        let body = crate::chainlink::request_body(self.request_id);
        self.admit_text(RecordKind::OutboundText, body.clone(), handle)?;
        match self.call(body).await {
            Some(payload) => {
                self.admit_text(RecordKind::InboundText, payload, handle)?;
                Ok(true)
            }
            None => {
                self.counters.read_idle_timeouts =
                    checked_add(self.counters.read_idle_timeouts, 1, || {
                        LiveError::CounterExhausted
                    })?;
                self.close_epoch(handle)?;
                self.rotate();
                Ok(false)
            }
        }
    }

    /// Rotate the round cursor to the next endpoint after a fault (R9).
    fn rotate(&mut self) {
        self.endpoint_index = (self.endpoint_index + 1) % self.endpoints.len();
    }

    /// Issue one request against the current endpoint through the injected
    /// transport. The borrow of `self.transport` ends when the (owned) future is
    /// returned, so the adapter can be touched after the await.
    async fn call(&mut self, body: String) -> Option<String> {
        let endpoint = self.endpoints[self.endpoint_index].clone();
        (self.transport)(endpoint, body).await
    }

    /// Open connection epoch 1 up front (a first-tick connect attempt).
    fn open_epoch<H>(&mut self, handle: &mut H) -> Result<(), LiveError>
    where
        H: FnMut(TimedIngestOutcome) -> Result<LiveControl, LiveError>,
    {
        self.counters.connection_attempts =
            checked_add(self.counters.connection_attempts, 1, || {
                LiveError::CounterExhausted
            })?;
        self.epoch = checked_add(self.epoch, 1, || LiveError::CounterExhausted)?;
        self.ordinal = checked_add(self.ordinal, 1, || LiveError::CounterExhausted)?;
        let ns = self.clock.elapsed_ns()?;
        let connected =
            connected_record(&self.clock, Self::STREAM, self.epoch, self.ordinal, ns, ns)?;
        self.admit(&connected, ns, handle)?;
        self.epoch_open = true;
        Ok(())
    }

    /// Reopen a new epoch after a fault: a reconnect that counts as a successful
    /// connection (A5 counts every reconnect epoch as one).
    fn reopen_epoch<H>(&mut self, handle: &mut H) -> Result<(), LiveError>
    where
        H: FnMut(TimedIngestOutcome) -> Result<LiveControl, LiveError>,
    {
        self.open_epoch(handle)?;
        self.counters.successful_connections =
            checked_add(self.counters.successful_connections, 1, || {
                LiveError::CounterExhausted
            })?;
        self.counters.reconnects =
            checked_add(self.counters.reconnects, 1, || LiveError::CounterExhausted)?;
        Ok(())
    }

    /// Close the current epoch with a Disconnected record after a fault.
    fn close_epoch<H>(&mut self, handle: &mut H) -> Result<(), LiveError>
    where
        H: FnMut(TimedIngestOutcome) -> Result<LiveControl, LiveError>,
    {
        self.ordinal = checked_add(self.ordinal, 1, || LiveError::CounterExhausted)?;
        let ns = self.clock.elapsed_ns()?;
        let disconnected =
            disconnected_record(&self.clock, Self::STREAM, self.epoch, self.ordinal, ns)?;
        self.admit(&disconnected, ns, handle)?;
        self.epoch_open = false;
        Ok(())
    }

    /// Build and admit one text record (Outbound/Inbound) in the current epoch.
    fn admit_text<H>(
        &mut self,
        kind: RecordKind,
        payload: String,
        handle: &mut H,
    ) -> Result<(), LiveError>
    where
        H: FnMut(TimedIngestOutcome) -> Result<LiveControl, LiveError>,
    {
        self.ordinal = checked_add(self.ordinal, 1, || LiveError::CounterExhausted)?;
        let ns = self.clock.elapsed_ns()?;
        let record = text_record(
            &self.clock,
            Self::STREAM,
            self.epoch,
            self.ordinal,
            kind,
            payload,
            ns,
        )?;
        self.admit(&record, ns, handle)
    }

    /// Admit one record through the adapter, count it, and forward any outcome to
    /// the caller's `handle` (the shared count/forward step A5 used).
    fn admit<H>(
        &mut self,
        record: &RawRecord,
        logical_receipt_ns: u64,
        handle: &mut H,
    ) -> Result<(), LiveError>
    where
        H: FnMut(TimedIngestOutcome) -> Result<LiveControl, LiveError>,
    {
        admit_poll_record(
            &mut self.adapter,
            record,
            logical_receipt_ns,
            &mut self.counters,
            handle,
        )
        .map(|_control| ())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DispatchControl {
    Continue,
    Reconnect,
}

const fn admits_transport_disconnect(exit_run: bool) -> bool {
    !exit_run
}

fn required_coinbase_recovery_count(
    after_trade_id: u64,
    through_trade_id: u64,
    recovered_event_count: usize,
) -> Result<usize, LiveError> {
    let required = through_trade_id
        .checked_sub(after_trade_id)
        .and_then(|count| usize::try_from(count).ok())
        .ok_or_else(|| {
            LiveError::InvalidRecovery("Coinbase recovery range cannot be represented".to_owned())
        })?;
    if recovered_event_count < required {
        return Err(LiveError::InvalidRecovery(
            "Coinbase recovery returned fewer trades than requested".to_owned(),
        ));
    }
    Ok(required)
}

// One admission step coordinates several orthogonal live-runner concerns —
// the adapter, the stamped clock/receipt, the optional Coinbase REST recovery
// client, the optional observe-only Kraken reconciler, run counters, and the
// publication callback. They are independent, so bundling them purely to satisfy
// the argument-count lint would group unrelated state; the count is accepted.
#[allow(clippy::too_many_arguments)]
async fn dispatch<H, R>(
    adapter: &mut ReplayAdapter,
    record: RawRecord,
    clock: &ProcessClock,
    logical_receipt_ns: u64,
    coinbase_rest: Option<&R>,
    kraken_reconciler: Option<&mut KrakenTradeReconciler>,
    counters: &mut LiveCounters,
    handle: &mut H,
) -> Result<DispatchControl, LiveError>
where
    H: FnMut(TimedIngestOutcome) -> Result<LiveControl, LiveError>,
    R: CoinbaseTradeRecovery,
{
    counters.records = checked_add(counters.records, 1, || LiveError::CounterExhausted)?;
    let outcome = adapter.ingest(&record)?;
    let recoveries = recovery_actions(&outcome.notices);
    count_outcome(counters, &outcome)?;
    // Observe-only: copy any Kraken BTC/USD trade ids into the reconciler's
    // live-runner-local tape before the outcome is handed to the publisher.
    // This reads the events the pipeline already emits and writes nothing back.
    if let Some(reconciler) = kraken_reconciler {
        reconciler.observe_events(&outcome.events);
    }
    let mut requested_control = LiveControl::Continue;
    if !outcome.events.is_empty() || !outcome.notices.is_empty() {
        requested_control = handle(TimedIngestOutcome {
            outcome,
            logical_receipt_ns,
        })?;
    }

    for recovery in recoveries {
        match recovery {
            RecoveryAction::BackfillCoinbaseTrades {
                product_id,
                after_trade_id,
                through_trade_id,
            } => {
                if product_id != "BTC-USD" {
                    return Err(LiveError::InvalidRecovery(
                        "Coinbase recovery requested an unsupported product".to_owned(),
                    ));
                }
                let client = coinbase_rest.ok_or_else(|| {
                    LiveError::InvalidRecovery(
                        "Coinbase trade recovery has no configured REST client".to_owned(),
                    )
                })?;
                let ReplayAdapter::Coinbase(coinbase) = adapter else {
                    return Err(LiveError::InvalidRecovery(
                        "Coinbase recovery was emitted by a non-Coinbase adapter".to_owned(),
                    ));
                };
                let mut recovered = client
                    .recover(coinbase, after_trade_id, through_trade_id)
                    .await?;
                let recovered_trade_count = required_coinbase_recovery_count(
                    after_trade_id,
                    through_trade_id,
                    recovered.events.len(),
                )?;
                let recovered_receipt_ns = clock.elapsed_ns()?;
                let recovered_received_at = clock.receipt_time_text(recovered_receipt_ns)?;
                for event in recovered.events.iter_mut().take(recovered_trade_count) {
                    event.received_at.clone_from(&recovered_received_at);
                }
                counters.coinbase_trade_backfills =
                    checked_add(counters.coinbase_trade_backfills, 1, || {
                        LiveError::CounterExhausted
                    })?;
                count_outcome(counters, &recovered)?;
                if handle(TimedIngestOutcome {
                    outcome: recovered,
                    logical_receipt_ns: recovered_receipt_ns,
                })? == LiveControl::Reconnect
                {
                    requested_control = LiveControl::Reconnect;
                }
            }
            // The CF Benchmarks REST passthrough has no live-runner client, and
            // reconnecting repairs nothing: the settlement window stays paused
            // until a repair covers the exact interval. The live runner refuses
            // the `kalshi_index` stream outright until the wave-A capture
            // settles its contract, so this arm is a fail-closed backstop rather
            // than a reachable path.
            RecoveryAction::BackfillKalshiBrti { .. } => {
                return Err(LiveError::InvalidRecovery(
                    "Kalshi CF Benchmarks settlement-window repair has no live-runner path"
                        .to_owned(),
                ));
            }
            RecoveryAction::Reconnect => return Ok(DispatchControl::Reconnect),
            RecoveryAction::RediscoverPredictionMarket
            | RecoveryAction::ResubscribeKrakenBook { .. }
            | RecoveryAction::ResubscribeKrakenLevel3 { .. }
            | RecoveryAction::ResubscribeKalshiBook { .. } => {
                return Ok(DispatchControl::Reconnect);
            }
        }
    }
    Ok(match requested_control {
        LiveControl::Continue => DispatchControl::Continue,
        LiveControl::Reconnect => DispatchControl::Reconnect,
    })
}

fn recovery_actions(notices: &[FeedNotice]) -> Vec<RecoveryAction> {
    notices
        .iter()
        .filter_map(|notice| notice.recovery.clone())
        .collect()
}

/// Ruling (global follow-up to E37d): admit one counted `UnsupportedMessage`
/// notice for a venue websocket that met an unexpected binary/continuation frame,
/// so the stream survives it. The caller then drops the socket and the reconnect
/// ladder re-dials — the lane stays up and the affected stream goes honestly
/// disconnected, rather than a hard error cancelling every sibling lane sharing
/// the run's `try_join!`. This path is for transport-frame faults only: an
/// adapter/recovery/REST/counter fault dispatching a decoded text frame stays
/// fatal (fail-closed), as do start-up refusals. Mirrors the existing failed-dial
/// notice path.
fn admit_unsupported_message<H>(
    detail: impl Into<String>,
    logical_receipt_ns: u64,
    counters: &mut LiveCounters,
    handle: &mut H,
) -> Result<(), LiveError>
where
    H: FnMut(TimedIngestOutcome) -> Result<LiveControl, LiveError>,
{
    let notice =
        IngestOutcome::notice(FeedNotice::info(FeedNoticeCode::UnsupportedMessage, detail));
    count_outcome(counters, &notice)?;
    handle(TimedIngestOutcome {
        outcome: notice,
        logical_receipt_ns,
    })?;
    Ok(())
}

fn count_outcome(counters: &mut LiveCounters, outcome: &IngestOutcome) -> Result<(), LiveError> {
    counters.events = checked_add(
        counters.events,
        u64::try_from(outcome.events.len()).map_err(|_| LiveError::CounterExhausted)?,
        || LiveError::CounterExhausted,
    )?;
    counters.notices = checked_add(
        counters.notices,
        u64::try_from(outcome.notices.len()).map_err(|_| LiveError::CounterExhausted)?,
        || LiveError::CounterExhausted,
    )?;
    counters.recoveries = checked_add(
        counters.recoveries,
        u64::try_from(
            outcome
                .notices
                .iter()
                .filter(|notice| notice.recovery.is_some())
                .count(),
        )
        .map_err(|_| LiveError::CounterExhausted)?,
        || LiveError::CounterExhausted,
    )?;
    Ok(())
}

enum ReconnectWait {
    Retry,
    Deadline,
    Interrupted,
}

async fn next_with_timeout<S>(
    idle_timeout: Duration,
    stream: &mut S,
) -> Result<Option<S::Item>, tokio::time::error::Elapsed>
where
    S: futures_util::Stream + Unpin,
{
    timeout(idle_timeout, stream.next()).await
}

/// The wait after an unproductive connection (a dial that never completed, or a
/// socket that opened and delivered nothing): the capture path's 1 s → 60 s
/// ladder (ruling E27(c), deliverable E41(2)), keyed on the streak, with one
/// stderr line per escalation step (`redial_backoff_stepped`). Emitting the line
/// here keeps the two unproductive call sites (dial-failure, silent-drop) in one
/// place.
fn unproductive_redial_delay(unproductive_streak: u64, stream: CaptureStream) -> Duration {
    let delay = redial_backoff(unproductive_streak);
    if unproductive_streak > 0 && redial_backoff_stepped(unproductive_streak) {
        eprintln!(
            "live {}: {unproductive_streak} connections produced no data; \
             backing off {delay:?} before re-dialling {}",
            stream.as_str(),
            endpoint(stream)
        );
    }
    delay
}

/// Wait `delay` before re-dialling, racing the shutdown coordinator and the run
/// deadline so a stop ends the run mid-backoff. The delay is chosen by the caller
/// on one of two ladders (unproductive 1 s → 60 s, productive-drop 500 ms → 8 s);
/// it is never zero, so no reconnect path can busy-loop (ruling E41 audit).
async fn wait_before_redial(
    delay: Duration,
    deadline: Instant,
    shutdown: &mut ShutdownWait,
) -> Result<ReconnectWait, LiveError> {
    tokio::select! {
        result = shutdown.wait() => {
            match result.map_err(LiveError::Signal)? {
                LiveShutdownReason::NaturalDeadline => Ok(ReconnectWait::Deadline),
                LiveShutdownReason::OperatorSignal => Ok(ReconnectWait::Interrupted),
            }
        },
        () = tokio::time::sleep_until(deadline) => Ok(ReconnectWait::Deadline),
        () = sleep(delay) => Ok(ReconnectWait::Retry),
    }
}

fn connected_record(
    clock: &ProcessClock,
    stream: CaptureStream,
    connection_epoch: u64,
    ordinal: u64,
    capture_elapsed_ns: u64,
    connect_started_elapsed_ns: u64,
) -> Result<RawRecord, LiveError> {
    Ok(RawRecord::connected_at(
        stream,
        connection_epoch,
        ordinal,
        clock.receipt_time_text(capture_elapsed_ns)?,
        capture_elapsed_ns,
        connect_started_elapsed_ns,
    ))
}

fn disconnected_record(
    clock: &ProcessClock,
    stream: CaptureStream,
    connection_epoch: u64,
    ordinal: u64,
    capture_elapsed_ns: u64,
) -> Result<RawRecord, LiveError> {
    Ok(RawRecord::disconnected_at(
        stream,
        connection_epoch,
        ordinal,
        clock.receipt_time_text(capture_elapsed_ns)?,
        capture_elapsed_ns,
    ))
}

fn text_record(
    clock: &ProcessClock,
    stream: CaptureStream,
    connection_epoch: u64,
    ordinal: u64,
    kind: RecordKind,
    payload: String,
    capture_elapsed_ns: u64,
) -> Result<RawRecord, LiveError> {
    Ok(RawRecord::text_at(
        stream,
        connection_epoch,
        ordinal,
        kind,
        payload,
        clock.receipt_time_text(capture_elapsed_ns)?,
        capture_elapsed_ns,
    ))
}

fn elapsed_ns(started: Instant) -> Result<u64, LiveError> {
    u64::try_from(started.elapsed().as_nanos()).map_err(|_| LiveError::CounterExhausted)
}

/// Every terminal failure a live run can end with. All of them stop the run.
#[derive(Debug, Error)]
pub enum LiveError {
    /// Returned when [`LiveOptions`] validation rejects the requested run shape
    /// before any socket is opened. Carries the specific rule that failed.
    #[error("invalid live options: {0}")]
    InvalidOptions(String),
    /// Returned when shared capture setup fails: installing the process crypto
    /// provider, resolving the endpoint, or building the subscription set.
    #[error(transparent)]
    Capture(#[from] CaptureError),
    /// Returned when the deterministic replay adapter rejects an admitted record.
    #[error(transparent)]
    Adapter(#[from] AdapterError),
    /// Returned when the Coinbase REST client fails to serve a trade backfill.
    #[error(transparent)]
    CoinbaseRest(#[from] CoinbaseRestError),
    /// Returned when constructing the observe-only Kraken public-trade
    /// reconciler fails. Reconciliation cycles themselves are never fatal.
    #[error(transparent)]
    KrakenRest(#[from] KrakenRestError),
    /// Returned when minting the Kraken authenticated-WebSocket token fails,
    /// which is required before a KrakenLevel3 subscription can be sent.
    #[error(transparent)]
    KrakenAuth(#[from] KrakenAuthError),
    /// Returned when the operating-system stop-signal listener cannot be
    /// installed or the shared shutdown trigger was dropped before cancelling.
    #[error("shutdown signal failed: {0}")]
    Signal(std::io::Error),
    /// Returned when the Chainlink start-up target verification decoded a value
    /// that differs from the compiled expectation (`decimals() != 8` or a wrong
    /// `description()`): a wrong proxy or a changed on-chain contract. Fatal,
    /// exit-78 (configuration) class — distinct from a transient endpoint fault,
    /// which rotates and retries rather than stopping the run.
    #[error("chainlink start-up target mismatch: {0}")]
    TargetMismatch(String),
    /// Returned when a notice requests a recovery this runner cannot serve:
    /// an unsupported product, no configured REST client, an adapter mismatch,
    /// or a returned trade range shorter than the requested gap.
    #[error("invalid recovery request: {0}")]
    InvalidRecovery(String),
    /// Returned once connection attempts reach `MAX_CONNECTION_ATTEMPTS`
    /// (100_000) within one run, before a further attempt is made.
    #[error("live connection-attempt limit reached")]
    ConnectionAttemptLimit,
    /// Returned when the run ended on its deadline with zero successful
    /// connections and no operator interrupt.
    #[error("live feed did not establish a connection before the deadline")]
    NoConnection,
    /// Returned when a requested fault exercise finished without observing the
    /// behavior it exists to prove (no induced drop, no backfill, no reconnect,
    /// or fewer than two fresh Kraken tokens). Never raised after an interrupt.
    #[error("fault exercise did not prove its expected behavior: {0}")]
    DiagnosticIncomplete(String),
    /// Returned when a u64 run counter, ordinal, or nanosecond clock read would
    /// overflow. The run fails closed rather than wrapping a sequence number.
    #[error("live counters exhausted")]
    CounterExhausted,
    /// Returned when a logical offset does not fit in i64 nanoseconds, so a wall
    /// timestamp cannot be rendered from the process origin.
    #[error("logical wall-time offset cannot be represented in nanoseconds")]
    ClockRange,
    /// Returned by the publication callback when it refuses an outcome. The run
    /// stops instead of dropping normalized events under backpressure.
    #[error("publication boundary rejected an outcome: {0}")]
    Publication(String),
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use crate::coinbase::{CoinbaseAdapter, CoinbaseRestTrade};
    use crate::coinbase_rest::CoinbaseTradeRecovery;
    use crate::{CaptureStream, Provider};
    use chrono::TimeDelta;

    use super::*;

    #[test]
    fn projected_receipts_are_causal_and_admission_watermark_is_separate() {
        let clock = ProcessClock::start();
        for _ in 0..128 {
            let receipt_ns = clock.elapsed_ns().expect("receipt clock reads");
            let receipt = DateTime::parse_from_rfc3339(
                &clock
                    .receipt_time_text(receipt_ns)
                    .expect("receipt time renders"),
            )
            .expect("receipt parses")
            .with_timezone(&Utc);
            let admitted = clock
                .wall_time_at(
                    clock
                        .admission_ns(receipt_ns)
                        .expect("admission clock reads"),
                )
                .expect("admission time renders");
            assert!(
                admitted >= receipt,
                "receipt {receipt} is later than reconstructed admission {admitted}"
            );
        }

        let freshness_before = clock.elapsed_ns().expect("freshness reads");
        let forced_admission = clock
            .admission_ns(freshness_before + 30_000_000_000)
            .expect("forced admission reads");
        assert!(clock.elapsed_ns().expect("freshness stays monotonic") < forced_admission);
        assert!(
            clock
                .clone()
                .admission_ns(0)
                .expect("cloned admission reads")
                >= forced_admission
        );
    }

    #[test]
    fn resumed_clock_accounts_for_downtime_once_and_clamps_wall_regression() {
        let wall_start = DateTime::parse_from_rfc3339("2026-08-13T12:00:00Z")
            .expect("wall start parses")
            .with_timezone(&Utc);
        let wall_now = wall_start + TimeDelta::seconds(12);
        let resumed = ProcessClock::resume_at(wall_start, 10_000_000_000, wall_now)
            .expect("forward wall time resumes");
        assert!(resumed.elapsed_ns().expect("clock reads") >= 12_000_000_000);
        assert_eq!(resumed.wall_start(), wall_start);

        let clamped = ProcessClock::resume_at(wall_start, 13_000_000_000, wall_now)
            .expect("backward wall correction clamps to the durable checkpoint");
        assert!(clamped.elapsed_ns().expect("clock reads") >= 13_000_000_000);

        let before_origin = ProcessClock::resume_at(
            wall_start,
            13_000_000_000,
            wall_start - TimeDelta::seconds(5),
        )
        .expect("wall time before the origin still resumes from the checkpoint");
        assert!(before_origin.elapsed_ns().expect("clock reads") >= 13_000_000_000);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn pending_deadline_is_rechecked_when_signal_becomes_ready_at_boundary() {
        let deadline_reached = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let coordinator_deadline = std::sync::Arc::clone(&deadline_reached);
        let (trigger, mut shutdown) = live_shutdown_channel();
        let (entered_sender, entered_receiver) = tokio::sync::oneshot::channel();
        let (signal_sender, signal_receiver) = tokio::sync::oneshot::channel();
        let signal = async move {
            entered_sender.send(()).map_err(|()| {
                std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "race-test observer was dropped",
                )
            })?;
            signal_receiver.await.map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "race-test signal sender was dropped",
                )
            })
        };
        let coordinator = tokio::spawn(coordinate_live_shutdown_race_on(
            trigger,
            signal,
            std::future::pending::<()>(),
            move || coordinator_deadline.load(std::sync::atomic::Ordering::SeqCst),
        ));

        entered_receiver
            .await
            .expect("the coordinator enters select before the signal is released");
        assert!(
            !deadline_reached.load(std::sync::atomic::Ordering::SeqCst),
            "the deadline must still be pending when select first polls the signal"
        );
        deadline_reached.store(true, std::sync::atomic::Ordering::SeqCst);
        signal_sender
            .send(())
            .expect("the selected signal future is still waiting");

        let reason = coordinator
            .await
            .expect("coordinator task joins")
            .expect("coordinator classifies the boundary race");
        assert_eq!(reason, LiveShutdownReason::NaturalDeadline);
        assert_eq!(
            shutdown
                .wait_reason()
                .await
                .expect("the shared cause is published"),
            LiveShutdownReason::NaturalDeadline
        );
    }

    #[test]
    fn coinbase_fault_injector_drops_exactly_the_second_match() {
        let mut diagnostics = DiagnosticState::default();
        let match_message = r#"{"type":"match","trade_id":1}"#;
        assert!(!diagnostics.should_drop_coinbase_trade(true, match_message));
        assert!(diagnostics.should_drop_coinbase_trade(true, match_message));
        assert!(!diagnostics.should_drop_coinbase_trade(true, match_message));
        assert!(!diagnostics.should_drop_coinbase_trade(true, r#"{"type":"heartbeat"}"#));
    }

    #[test]
    fn diagnostic_options_are_stream_scoped_and_must_be_observable() {
        let options = |stream, gap, reconnect| LiveOptions {
            stream,
            duration: Duration::from_secs(30),
            kraken_btc_book_depth: 100,
            publisher_epoch: "test".to_owned(),
            diagnostics: LiveDiagnostics {
                induce_coinbase_trade_gap_once: gap,
                induced_reconnect_interval: reconnect,
            },
            shutdown: None,
            kraken_trade_reconciliation: None,
        };
        assert!(matches!(
            options(CaptureStream::KrakenCore, true, None).validate(),
            Err(LiveError::InvalidOptions(_))
        ));
        assert!(matches!(
            options(
                CaptureStream::KrakenLevel3,
                false,
                Some(Duration::from_secs(30))
            )
            .validate(),
            Err(LiveError::InvalidOptions(_))
        ));
        options(
            CaptureStream::KrakenLevel3,
            false,
            Some(Duration::from_secs(10)),
        )
        .validate()
        .expect("observable authenticated reconnect diagnostic is valid");
    }

    #[test]
    fn coinbase_recovery_range_rejects_inversion_and_underdelivery() {
        assert!(matches!(
            required_coinbase_recovery_count(12, 11, 2),
            Err(LiveError::InvalidRecovery(_))
        ));
        assert!(matches!(
            required_coinbase_recovery_count(10, 12, 1),
            Err(LiveError::InvalidRecovery(_))
        ));
        assert_eq!(
            required_coinbase_recovery_count(10, 12, 3)
                .expect("held live events may follow the exact recovered range"),
            2
        );
    }

    #[test]
    fn controlled_live_exit_does_not_admit_a_transport_disconnect() {
        assert!(!admits_transport_disconnect(true));
        assert!(admits_transport_disconnect(false));
    }

    #[test]
    fn kraken_trade_reconciliation_is_kraken_core_only_and_must_be_positive() {
        let options = |stream, cadence| LiveOptions {
            stream,
            duration: Duration::from_secs(30),
            kraken_btc_book_depth: 100,
            publisher_epoch: "test".to_owned(),
            diagnostics: LiveDiagnostics::default(),
            shutdown: None,
            kraken_trade_reconciliation: cadence,
        };
        // Wrong stream is rejected.
        assert!(matches!(
            options(CaptureStream::CoinbaseCore, Some(Duration::from_secs(5))).validate(),
            Err(LiveError::InvalidOptions(_))
        ));
        // A zero cadence is rejected.
        assert!(matches!(
            options(CaptureStream::KrakenCore, Some(Duration::ZERO)).validate(),
            Err(LiveError::InvalidOptions(_))
        ));
        // KrakenCore with a positive cadence is valid.
        options(CaptureStream::KrakenCore, Some(Duration::from_secs(5)))
            .validate()
            .expect("KrakenCore reconciliation cadence is valid");
    }

    struct FakeRecovery {
        range: Cell<Option<(u64, u64)>>,
    }

    impl CoinbaseTradeRecovery for FakeRecovery {
        async fn recover(
            &self,
            adapter: &mut CoinbaseAdapter,
            after_trade_id: u64,
            through_trade_id: u64,
        ) -> Result<IngestOutcome, CoinbaseRestError> {
            self.range.set(Some((after_trade_id, through_trade_id)));
            adapter
                .apply_trade_backfill(
                    after_trade_id,
                    through_trade_id,
                    "2026-08-13T12:00:01.500Z",
                    vec![trade(11), trade(12)],
                )
                .map_err(CoinbaseRestError::Adapter)
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn coinbase_gap_invokes_recovery_before_live_publication_resumes() {
        let mut adapter = ReplayAdapter::new(
            CaptureStream::CoinbaseCore,
            "test",
            KrakenBookDepths { btc: 100, usdt: 10 },
        )
        .expect("adapter constructs");
        let recovery = FakeRecovery {
            range: Cell::new(None),
        };
        let mut counters = LiveCounters::default();
        let mut handled = Vec::new();
        let clock = ProcessClock::start();
        let mut handler = |outcome| {
            handled.push(outcome);
            Ok(LiveControl::Continue)
        };

        for record in [
            connected(),
            inbound(
                2,
                r#"{"type":"match","trade_id":10,"sequence":100,"product_id":"BTC-USD","price":"60000.10","size":"0.01","side":"sell","time":"2026-08-13T12:00:00Z"}"#,
            ),
            inbound(
                3,
                r#"{"type":"match","trade_id":12,"sequence":102,"product_id":"BTC-USD","price":"60001.00","size":"0.02","side":"buy","time":"2026-08-13T12:00:01Z"}"#,
            ),
            inbound(
                4,
                r#"{"type":"match","trade_id":13,"sequence":103,"product_id":"BTC-USD","price":"60002.00","size":"0.03","side":"sell","time":"2026-08-13T12:00:02Z"}"#,
            ),
        ] {
            assert_eq!(
                dispatch(
                    &mut adapter,
                    record,
                    &clock,
                    clock.elapsed_ns().expect("logical time is representable"),
                    Some(&recovery),
                    None,
                    &mut counters,
                    &mut handler,
                )
                .await
                .expect("dispatch succeeds"),
                DispatchControl::Continue
            );
        }

        // The in-hand trade 12 is held, so recovery asks REST only for ids below
        // it: the requested range ends at id - 1 (11), not id.
        assert_eq!(recovery.range.get(), Some((10, 11)));
        assert_eq!(counters.records, 4);
        assert_eq!(counters.events, 4);
        assert_eq!(counters.notices, 2);
        assert_eq!(counters.recoveries, 1);
        assert_eq!(counters.coinbase_trade_backfills, 1);
        let recovery_batch = handled
            .iter()
            .find(|timed| timed.outcome.events.len() == 2)
            .expect("REST recovery and held live trade publish together");
        assert_ne!(
            recovery_batch.outcome.events[0].received_at,
            "2026-08-13T12:00:01.500Z"
        );
        assert_eq!(
            recovery_batch.outcome.events[1].received_at, "2026-08-13T12:00:01Z",
            "held live trades retain their original receipt timestamp"
        );
        assert_eq!(
            handled.last().map(|timed| timed.outcome.events.len()),
            Some(1)
        );
        assert!(
            handled
                .iter()
                .all(|timed| timed.logical_receipt_ns <= clock.elapsed_ns().expect("clock reads"))
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn publication_failure_stops_dispatch() {
        let mut adapter = ReplayAdapter::new(
            CaptureStream::CoinbaseCore,
            "test",
            KrakenBookDepths { btc: 100, usdt: 10 },
        )
        .expect("adapter constructs");
        let recovery = FakeRecovery {
            range: Cell::new(None),
        };
        let mut counters = LiveCounters::default();
        let clock = ProcessClock::start();
        let receipt = clock.elapsed_ns().expect("clock reads");
        let mut accept = |_timed| Ok(LiveControl::Continue);
        dispatch(
            &mut adapter,
            connected(),
            &clock,
            receipt,
            Some(&recovery),
            None,
            &mut counters,
            &mut accept,
        )
        .await
        .expect("connection boundary is accepted");
        let mut reject = |_timed| Err(LiveError::Publication("rejected".to_owned()));
        let error = dispatch(
            &mut adapter,
            inbound(
                2,
                r#"{"type":"match","trade_id":10,"sequence":100,"product_id":"BTC-USD","price":"60000.10","size":"0.01","side":"sell","time":"2026-08-13T12:00:00Z"}"#,
            ),
            &clock,
            receipt,
            Some(&recovery),
            None,
            &mut counters,
            &mut reject,
        )
        .await
        .expect_err("publication rejection is fatal");
        assert!(matches!(error, LiveError::Publication(_)));
    }

    /// Ruling (global follow-up to E37d), fail-closed boundary: only a
    /// transport-frame fault degrades. A dispatch fault on a decoded text frame —
    /// here an adapter refusal — stays fatal exactly as before E37d, so the run
    /// stops with the adapter error rather than looping a silent notice+redial to
    /// the deadline. A binary/continuation frame still degrades to a counted
    /// notice with no error. The read loop is network-bound, so the two policies
    /// are pinned through the units the loop routes on: `dispatch` (the text arm's
    /// `Err(error) => return Err(error)`) and `admit_unsupported_message` (the
    /// binary arm's counted notice).
    #[tokio::test(flavor = "current_thread")]
    async fn a_text_frame_adapter_refusal_stays_fatal_while_a_binary_frame_degrades() {
        let mut adapter = ReplayAdapter::new(
            CaptureStream::CoinbaseCore,
            "test",
            KrakenBookDepths { btc: 100, usdt: 10 },
        )
        .expect("adapter constructs");
        let recovery = FakeRecovery {
            range: Cell::new(None),
        };
        let mut counters = LiveCounters::default();
        let clock = ProcessClock::start();
        let receipt = clock.elapsed_ns().expect("clock reads");
        let mut accept = |_timed| Ok(LiveControl::Continue);

        // A text frame the adapter refuses (an inbound record with no active
        // connection epoch) surfaces as the same fatal error the run failed with
        // before E37d. The narrowed text arm propagates it verbatim.
        let error = dispatch(
            &mut adapter,
            inbound(
                2,
                r#"{"type":"match","trade_id":10,"sequence":100,"product_id":"BTC-USD","price":"60000.10","size":"0.01","side":"sell","time":"2026-08-13T12:00:00Z"}"#,
            ),
            &clock,
            receipt,
            Some(&recovery),
            None,
            &mut counters,
            &mut accept,
        )
        .await
        .expect_err("an adapter refusal on a text frame is fatal");
        assert!(
            matches!(error, LiveError::Adapter(_)),
            "the text-frame refusal keeps its adapter-error variant, got {error:?}"
        );

        // The binary/continuation arm is the only survivable path: it admits one
        // counted notice and returns no error, so the socket drops and re-dials
        // instead of cancelling the lane.
        let mut degraded_counters = LiveCounters::default();
        let mut accept_degrade = |_timed| Ok(LiveControl::Continue);
        admit_unsupported_message(
            "unexpected binary/continuation frame on coinbase-core",
            receipt,
            &mut degraded_counters,
            &mut accept_degrade,
        )
        .expect("a binary frame degrades to a counted notice, never an error");
        assert_eq!(
            degraded_counters.notices, 1,
            "the binary frame is the survivable, counted path"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn accepted_consumer_recovery_request_reconnects_without_failing_dispatch() {
        let mut adapter = ReplayAdapter::new(
            CaptureStream::CoinbaseCore,
            "test",
            KrakenBookDepths { btc: 100, usdt: 10 },
        )
        .expect("adapter constructs");
        let recovery = FakeRecovery {
            range: Cell::new(None),
        };
        let mut counters = LiveCounters::default();
        let clock = ProcessClock::start();
        let receipt = clock.elapsed_ns().expect("clock reads");
        let mut accept = |_timed| Ok(LiveControl::Continue);
        dispatch(
            &mut adapter,
            connected(),
            &clock,
            receipt,
            Some(&recovery),
            None,
            &mut counters,
            &mut accept,
        )
        .await
        .expect("connection boundary is accepted");

        let mut request_reconnect = |_timed| Ok(LiveControl::Reconnect);
        let control = dispatch(
            &mut adapter,
            inbound(
                2,
                r#"{"type":"match","trade_id":10,"sequence":100,"product_id":"BTC-USD","price":"60000.10","size":"0.01","side":"sell","time":"2026-08-13T12:00:00Z"}"#,
            ),
            &clock,
            receipt,
            Some(&recovery),
            None,
            &mut counters,
            &mut request_reconnect,
        )
        .await
        .expect("accepted consumer recovery request is not a publication failure");
        assert_eq!(control, DispatchControl::Reconnect);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stalled_stream_reaches_the_read_idle_watchdog() {
        let mut stalled = futures_util::stream::pending::<()>();
        assert!(
            next_with_timeout(Duration::from_millis(1), &mut stalled)
                .await
                .is_err()
        );
    }

    fn connected() -> RawRecord {
        RawRecord {
            schema_version: 1,
            provider: Provider::CoinbaseExchange,
            stream: CaptureStream::CoinbaseCore,
            connection_epoch: 1,
            ordinal: 1,
            received_at: "2026-08-13T12:00:00Z".to_owned(),
            capture_elapsed_ns: None,
            connect_started_elapsed_ns: None,
            kind: RecordKind::Connected,
            payload: None,
        }
    }

    fn inbound(ordinal: u64, payload: &str) -> RawRecord {
        RawRecord {
            schema_version: 1,
            provider: Provider::CoinbaseExchange,
            stream: CaptureStream::CoinbaseCore,
            connection_epoch: 1,
            ordinal,
            received_at: "2026-08-13T12:00:01Z".to_owned(),
            capture_elapsed_ns: None,
            connect_started_elapsed_ns: None,
            kind: RecordKind::InboundText,
            payload: Some(payload.to_owned()),
        }
    }

    fn trade(trade_id: u64) -> CoinbaseRestTrade {
        CoinbaseRestTrade {
            trade_id,
            price: if trade_id == 11 {
                "60000.75".to_owned()
            } else {
                "60001.00".to_owned()
            },
            size: "0.01".to_owned(),
            side: "sell".to_owned(),
            time: if trade_id == 11 {
                "2026-08-13T12:00:00.500Z".to_owned()
            } else {
                "2026-08-13T12:00:01Z".to_owned()
            },
        }
    }
}

#[cfg(test)]
mod poll_tests {
    use std::cell::RefCell;
    use std::time::Duration;

    use super::{
        ChainlinkPollOptions, LiveControl, LiveError, ProcessClock, TimedIngestOutcome,
        live_shutdown_channel, run_live_poll,
    };
    use crate::FeedNoticeCode;

    const DECIMALS: &str = "313ce567";
    const DESCRIPTION: &str = "7284e416";
    const ROUND: &str = "feaf968c";

    fn decimals_reply(value: u128) -> String {
        format!(r#"{{"jsonrpc":"2.0","id":1,"result":"0x{value:064x}"}}"#)
    }

    fn description_reply(text: &str) -> String {
        let mut data_hex = String::new();
        for byte in text.as_bytes() {
            data_hex.push_str(&format!("{byte:02x}"));
        }
        while !data_hex.len().is_multiple_of(64) {
            data_hex.push('0');
        }
        format!(
            r#"{{"jsonrpc":"2.0","id":1,"result":"0x{:064x}{:064x}{data_hex}"}}"#,
            32u128,
            text.len() as u128
        )
    }

    fn round_reply(rid: u128, secs: u128) -> String {
        format!(
            r#"{{"jsonrpc":"2.0","id":1,"result":"0x{rid:064x}{:064x}{secs:064x}{secs:064x}{rid:064x}"}}"#,
            7_766_779_700_000u128
        )
    }

    fn options() -> ChainlinkPollOptions {
        ChainlinkPollOptions {
            endpoints: vec![
                "https://a.example".to_owned(),
                "https://b.example".to_owned(),
                "https://c.example".to_owned(),
            ],
            duration: Duration::from_secs(30 * 60),
            publisher_epoch: "test-epoch".to_owned(),
            shutdown: None,
        }
    }

    /// Healthy start-up; round polls follow `plan` (Some(rid,secs) = that round,
    /// None = fault). Records the endpoints polled and the request bodies seen.
    fn transport<'a>(
        plan: &'a [Option<(u128, u128)>],
        round_idx: &'a RefCell<usize>,
        endpoints_seen: &'a RefCell<Vec<String>>,
        bodies: &'a RefCell<Vec<String>>,
    ) -> impl FnMut(String, String) -> std::future::Ready<Option<String>> + 'a {
        move |endpoint: String, body: String| {
            bodies.borrow_mut().push(body.clone());
            let reply = if body.contains(DECIMALS) {
                Some(decimals_reply(8))
            } else if body.contains(DESCRIPTION) {
                Some(description_reply("BTC / USD"))
            } else {
                endpoints_seen.borrow_mut().push(endpoint);
                let mut idx = round_idx.borrow_mut();
                // Past the end of the plan, repeat its last outcome (a steady
                // round that reads as a silent duplicate).
                let outcome = if *idx < plan.len() {
                    plan[*idx]
                } else {
                    plan.last().copied().flatten()
                };
                *idx += 1;
                outcome.map(|(rid, secs)| round_reply(rid, secs))
            };
            std::future::ready(reply)
        }
    }

    /// B3 invariant: a hung request is abandoned within one poll cadence.
    #[test]
    fn request_timeout_is_shorter_than_the_poll_cadence() {
        assert!(crate::chainlink::REQUEST_TIMEOUT < crate::chainlink::POLL_CADENCE);
    }

    /// B1: a start-up JSON-RPC error on endpoint[0] rotates to endpoint[1].
    #[tokio::test(start_paused = true)]
    async fn startup_rotates_past_a_faulting_endpoint() {
        let (trigger, shutdown) = live_shutdown_channel();
        let mut options = options();
        options.duration = Duration::from_secs(90);
        options.shutdown = Some(shutdown);
        let report = run_live_poll(
            options,
            ProcessClock::start(),
            |endpoint: String, body: String| {
                let reply = if body.contains(DECIMALS) && endpoint == "https://a.example" {
                    r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32603,"message":"x"}}"#.to_owned()
                } else if body.contains(DECIMALS) {
                    decimals_reply(8)
                } else if body.contains(DESCRIPTION) {
                    description_reply("BTC / USD")
                } else {
                    round_reply((7u128 << 64) | 1, 1_756_400_000)
                };
                std::future::ready(Some(reply))
            },
            |_o: TimedIngestOutcome| Ok(LiveControl::Continue),
        )
        .await
        .expect("a transient start-up fault rotates, never aborts");
        drop(trigger);
        assert!(report.successful_connections >= 1);
    }

    /// B1/M4: a decoded start-up MISMATCH (decimals != 8) is fatal — and it
    /// still leaves journal evidence, because the Connected + start-up
    /// Outbound/Inbound records are admitted through the adapter before the
    /// mismatch is decoded (the consumer's handle sees the epoch-open notice).
    #[tokio::test(start_paused = true)]
    async fn wrong_decimals_is_a_fatal_mismatch_after_recording_the_startup() {
        let handled = RefCell::new(0usize);
        let result = run_live_poll(
            options(),
            ProcessClock::start(),
            |_e: String, body: String| {
                let reply = if body.contains(DECIMALS) {
                    decimals_reply(6)
                } else {
                    description_reply("BTC / USD")
                };
                std::future::ready(Some(reply))
            },
            |_o: TimedIngestOutcome| {
                *handled.borrow_mut() += 1;
                Ok(LiveControl::Continue)
            },
        )
        .await;
        assert!(matches!(result, Err(LiveError::TargetMismatch(_))));
        assert!(
            *handled.borrow() >= 1,
            "the Connected epoch-open evidence reaches the journal before the abort"
        );
    }

    /// M4: the start-up `decimals()`/`description()` calls are recorded (Outbound
    /// + Inbound, admitted through the adapter) and precede the first round poll.
    #[tokio::test(start_paused = true)]
    async fn startup_calls_are_recorded_before_the_first_poll() {
        let round_idx = RefCell::new(0usize);
        let endpoints_seen = RefCell::new(Vec::new());
        let bodies = RefCell::new(Vec::new());
        let plan = [Some(((7u128 << 64) | 1, 1_756_400_000u128))];
        let (trigger, shutdown) = live_shutdown_channel();
        let mut options = options();
        options.duration = Duration::from_secs(90);
        options.shutdown = Some(shutdown);
        let report = run_live_poll(
            options,
            ProcessClock::start(),
            transport(&plan, &round_idx, &endpoints_seen, &bodies),
            |_o: TimedIngestOutcome| Ok(LiveControl::Continue),
        )
        .await
        .expect("run completes");
        drop(trigger);
        let seen = bodies.borrow();
        assert!(
            seen[0].contains(DECIMALS),
            "first call is decimals(): {seen:?}"
        );
        assert!(
            seen[1].contains(DESCRIPTION),
            "second is description(): {seen:?}"
        );
        assert!(
            seen[2].contains(ROUND),
            "third is the first round poll: {seen:?}"
        );
        // Connected + 2 start-up Outbound + 2 start-up Inbound were all admitted.
        assert!(
            report.records >= 5,
            "start-up calls recorded: {}",
            report.records
        );
    }

    /// M5: all endpoints down for the whole run exits transient (75) with a
    /// Disconnected (down) notice, never exit 0 with zero work.
    #[tokio::test(start_paused = true)]
    async fn total_startup_outage_exits_transient_and_marks_down() {
        let notices: RefCell<Vec<FeedNoticeCode>> = RefCell::new(Vec::new());
        let (trigger, shutdown) = live_shutdown_channel();
        let mut options = options();
        options.duration = Duration::from_secs(5 * 60);
        options.shutdown = Some(shutdown);
        let result = run_live_poll(
            options,
            ProcessClock::start(),
            |_e: String, _b: String| std::future::ready(None),
            |o: TimedIngestOutcome| {
                for n in &o.outcome.notices {
                    notices.borrow_mut().push(n.code);
                }
                Ok(LiveControl::Continue)
            },
        )
        .await;
        drop(trigger);
        assert!(
            matches!(result, Err(LiveError::NoConnection)),
            "transient exit (75)"
        );
        let codes = notices.borrow();
        assert!(
            codes.contains(&FeedNoticeCode::Connected),
            "epoch opened: {codes:?}"
        );
        assert!(
            codes.contains(&FeedNoticeCode::Disconnected),
            "marked down: {codes:?}"
        );
    }

    /// B4: the round cursor survives a failover reconnect. After a fault, a
    /// lagging node serving an older round is skipped OutOfOrder (no backwards
    /// event); a newer round is accepted.
    #[tokio::test(start_paused = true)]
    async fn round_cursor_survives_failover() {
        let a = (7u128 << 64) | 1000;
        let round_idx = RefCell::new(0usize);
        let endpoints_seen = RefCell::new(Vec::new());
        let bodies = RefCell::new(Vec::new());
        let notices: RefCell<Vec<FeedNoticeCode>> = RefCell::new(Vec::new());
        let events = RefCell::new(0usize);
        let plan = [
            Some((a, 1_756_400_000u128)),     // round A -> event
            None,                             // fault -> Disconnected + rotate
            Some((a - 1, 1_756_399_940u128)), // lagging older round -> OutOfOrder
            Some((a + 1, 1_756_400_060u128)), // newer round -> event
            Some((a + 1, 1_756_400_060u128)),
            Some((a + 1, 1_756_400_060u128)),
        ];
        let (trigger, shutdown) = live_shutdown_channel();
        let mut options = options();
        options.duration = Duration::from_secs(30 * 60);
        options.shutdown = Some(shutdown);
        let report = run_live_poll(
            options,
            ProcessClock::start(),
            transport(&plan, &round_idx, &endpoints_seen, &bodies),
            |o: TimedIngestOutcome| {
                for n in &o.outcome.notices {
                    notices.borrow_mut().push(n.code);
                }
                *events.borrow_mut() += o.outcome.events.len();
                Ok(LiveControl::Continue)
            },
        )
        .await
        .expect("faults never exit the run");
        drop(trigger);
        assert!(!report.interrupted);
        assert_eq!(*events.borrow(), 2, "only rounds A and B emit");
        assert!(
            notices.borrow().contains(&FeedNoticeCode::OutOfOrder),
            "the lagging round after failover is OutOfOrder: {:?}",
            notices.borrow()
        );
        let seen = endpoints_seen.borrow();
        let distinct: std::collections::BTreeSet<&String> = seen.iter().collect();
        assert!(distinct.len() >= 2, "failover rotated endpoints: {seen:?}");
    }

    /// M1/M2/M3: an outage -> recovery -> second outage never exits and reports
    /// each transition as a real Disconnected/Connected record notice.
    #[tokio::test(start_paused = true)]
    async fn failover_reports_outage_recovery_and_second_outage_without_exiting() {
        let a = (7u128 << 64) | 2000;
        let round_idx = RefCell::new(0usize);
        let endpoints_seen = RefCell::new(Vec::new());
        let bodies = RefCell::new(Vec::new());
        let notices: RefCell<Vec<FeedNoticeCode>> = RefCell::new(Vec::new());
        let events = RefCell::new(0usize);
        let plan = [
            None,                             // outage 1
            Some((a, 1_756_400_000u128)),     // recovery -> event
            None,                             // outage 2
            Some((a + 1, 1_756_400_060u128)), // recovery -> event
            Some((a + 1, 1_756_400_060u128)),
            Some((a + 1, 1_756_400_060u128)),
        ];
        let (trigger, shutdown) = live_shutdown_channel();
        let mut options = options();
        options.shutdown = Some(shutdown);
        let report = run_live_poll(
            options,
            ProcessClock::start(),
            transport(&plan, &round_idx, &endpoints_seen, &bodies),
            |o: TimedIngestOutcome| {
                for n in &o.outcome.notices {
                    notices.borrow_mut().push(n.code);
                }
                *events.borrow_mut() += o.outcome.events.len();
                Ok(LiveControl::Continue)
            },
        )
        .await
        .expect("a poll fault never exits the run");
        drop(trigger);
        assert!(!report.interrupted);
        assert!(
            report.read_idle_timeouts >= 2,
            "{}",
            report.read_idle_timeouts
        );
        let codes = notices.borrow();
        assert!(
            codes
                .iter()
                .filter(|c| **c == FeedNoticeCode::Disconnected)
                .count()
                >= 2,
            "two outages reported: {codes:?}"
        );
        assert!(
            codes
                .iter()
                .filter(|c| **c == FeedNoticeCode::Connected)
                .count()
                >= 3,
            "recoveries reported: {codes:?}"
        );
        assert!(*events.borrow() >= 2, "served rounds produced events");
    }

    /// A pre-published coordinated shutdown ends the run cleanly.
    #[tokio::test(start_paused = true)]
    async fn shutdown_is_honoured_and_stops_without_error() {
        let (trigger, shutdown) = live_shutdown_channel();
        trigger.cancel();
        let mut options = options();
        options.shutdown = Some(shutdown);
        let report = run_live_poll(
            options,
            ProcessClock::start(),
            |_e: String, body: String| {
                let reply = if body.contains(DECIMALS) {
                    decimals_reply(8)
                } else if body.contains(DESCRIPTION) {
                    description_reply("BTC / USD")
                } else {
                    round_reply((7u128 << 64) | 1, 1_756_400_000)
                };
                std::future::ready(Some(reply))
            },
            |_o: TimedIngestOutcome| Ok(LiveControl::Continue),
        )
        .await
        .expect("a coordinated shutdown ends the run cleanly");
        assert!(report.interrupted);
    }
}

#[cfg(test)]
mod rest_poll_tests {
    use std::cell::RefCell;
    use std::future::{Future, ready};
    use std::pin::Pin;
    use std::rc::Rc;
    use std::time::Duration;

    use super::{
        LiveControl, LiveCounters, LiveDiagnostics, LiveError, LiveOptions, ProcessClock,
        ReconnectWait, ShutdownWait, TimedIngestOutcome, admit_unsupported_message,
        live_shutdown_channel, reconnect_delay, run_live_binance_rest, run_live_bybit_rest,
        unproductive_redial_delay, wait_before_redial,
    };
    use crate::CaptureStream;
    use crate::binance_usdm_rest::BinanceUsdmRestEndpoint;
    use crate::capture::{
        BinanceUsdmPoll, BinanceUsdmRestSession, BinanceUsdmRestTransport, BybitRestSession,
        BybitRestTransport, RestPollOutcome, redial_backoff,
    };

    // A served Bybit open-interest body (one open-interest row) and a served
    // Binance USD-M open-interest body, both taken from the replay fixtures.
    const BYBIT_OPEN_INTEREST: &str = r#"{"retCode":0,"retMsg":"OK","result":{"symbol":"BTCUSDT","category":"linear","list":[{"openInterest":"12345.6700","singleOpenInterest":"6172.83","timestamp":"1739502300000"}],"nextPageCursor":""},"retExtInfo":{},"time":1739502303204}"#;
    const BINANCE_OPEN_INTEREST: &str =
        r#"{"openInterest":"10659.50900000","symbol":"BTCUSDT","time":1788440000000}"#;

    fn rest_options(stream: CaptureStream) -> LiveOptions {
        LiveOptions {
            stream,
            duration: Duration::from_secs(30),
            kraken_btc_book_depth: 10,
            publisher_epoch: "rest-test".to_owned(),
            diagnostics: LiveDiagnostics::default(),
            shutdown: None,
            kraken_trade_reconciliation: None,
        }
    }

    /// A fixture-transport Bybit REST poll stream driven through the live runner
    /// delivers a served open-interest event and journals a `Failed` record (a
    /// recovery notice) when a later poll faults — the E41 live http_poll path.
    #[tokio::test(start_paused = true)]
    async fn bybit_rest_live_poll_delivers_events_and_journals_failed_on_a_fault() {
        // The first poll (open interest) is served; every later poll faults.
        let calls = Rc::new(RefCell::new(0u32));
        let transport: BybitRestTransport = {
            let calls = Rc::clone(&calls);
            Box::new(move |_url: &'static str| {
                *calls.borrow_mut() += 1;
                let outcome = if *calls.borrow() == 1 {
                    RestPollOutcome::Served(BYBIT_OPEN_INTEREST.to_owned())
                } else {
                    RestPollOutcome::Transport
                };
                Box::pin(ready(outcome)) as Pin<Box<dyn Future<Output = RestPollOutcome>>>
            })
        };
        let session = BybitRestSession::new(tokio::time::Instant::now(), transport);

        let sources: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
        let recoveries = Rc::new(RefCell::new(0_u64));
        let handle = {
            let sources = Rc::clone(&sources);
            let recoveries = Rc::clone(&recoveries);
            move |timed: TimedIngestOutcome| -> Result<LiveControl, LiveError> {
                for event in &timed.outcome.events {
                    sources.borrow_mut().push(event.source_id.clone());
                }
                *recoveries.borrow_mut() += timed
                    .outcome
                    .notices
                    .iter()
                    .filter(|notice| notice.recovery.is_some())
                    .count() as u64;
                Ok(LiveControl::Continue)
            }
        };

        let report = run_live_bybit_rest(
            rest_options(CaptureStream::BybitLinearRest),
            ProcessClock::start(),
            session,
            handle,
        )
        .await
        .expect("the bybit REST poll run reaches its deadline");

        assert!(
            sources
                .borrow()
                .iter()
                .any(|source| source.contains("open-interest")),
            "the served poll delivered an open-interest event through the handle"
        );
        assert!(report.events >= 1, "at least one event was admitted");
        assert!(
            *recoveries.borrow() >= 1 && report.recoveries >= 1,
            "a faulting poll journaled a Failed record as a recovery notice"
        );
        assert!(
            report.successful_connections >= 1,
            "a served poll counts as a connection (no NoConnection exit)"
        );
    }

    /// The same for the Binance USD-M REST poll stream: one served endpoint, the
    /// others fault, so the run delivers an event and journals a Failed record.
    #[tokio::test(start_paused = true)]
    async fn binance_rest_live_poll_delivers_events_and_journals_failed_on_a_fault() {
        let transport: BinanceUsdmRestTransport = Box::new(move |endpoint| {
            let outcome = if endpoint == BinanceUsdmRestEndpoint::OpenInterest {
                BinanceUsdmPoll::Body {
                    body: BINANCE_OPEN_INTEREST.to_owned(),
                    used_weight_header: None,
                }
            } else {
                BinanceUsdmPoll::Failed {
                    cause: "fixture fault".to_owned(),
                }
            };
            Box::pin(ready(outcome)) as Pin<Box<dyn Future<Output = BinanceUsdmPoll>>>
        });
        let session = BinanceUsdmRestSession::new(tokio::time::Instant::now(), transport);

        let sources: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
        let recoveries = Rc::new(RefCell::new(0_u64));
        let handle = {
            let sources = Rc::clone(&sources);
            let recoveries = Rc::clone(&recoveries);
            move |timed: TimedIngestOutcome| -> Result<LiveControl, LiveError> {
                for event in &timed.outcome.events {
                    sources.borrow_mut().push(event.source_id.clone());
                }
                *recoveries.borrow_mut() += timed
                    .outcome
                    .notices
                    .iter()
                    .filter(|notice| notice.recovery.is_some())
                    .count() as u64;
                Ok(LiveControl::Continue)
            }
        };

        let report = run_live_binance_rest(
            rest_options(CaptureStream::BinanceUsdmRest),
            ProcessClock::start(),
            session,
            handle,
        )
        .await
        .expect("the binance REST poll run reaches its deadline");

        assert!(
            sources
                .borrow()
                .iter()
                .any(|source| source.contains("open-interest")),
            "the served open-interest poll delivered an event"
        );
        assert!(report.events >= 1, "at least one event was admitted");
        assert!(
            *recoveries.borrow() >= 1 && report.recoveries >= 1,
            "a faulting endpoint journaled a Failed record as a recovery notice"
        );
        assert!(
            report.successful_connections >= 1,
            "a served poll counts as a connection"
        );
    }

    /// Deliverable E41(2): the unproductive-connection redial wait walks the
    /// capture path's 1 s → 60 s ladder, tested against a mocked clock.
    #[tokio::test(start_paused = true)]
    async fn websocket_redial_walks_the_capture_backoff_ladder() {
        // A never-firing shutdown and a far deadline, so the ladder sleep wins.
        let (_trigger, shared) = live_shutdown_channel();
        let mut shutdown = ShutdownWait::new(Some(shared));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10 * 3600);

        for streak in 1..=8_u64 {
            let delay = unproductive_redial_delay(streak, CaptureStream::BybitLinear);
            assert_eq!(
                delay,
                redial_backoff(streak),
                "streak {streak} on the ladder"
            );
            let start = tokio::time::Instant::now();
            let wait = wait_before_redial(delay, deadline, &mut shutdown)
                .await
                .expect("the redial wait resolves");
            assert!(
                matches!(wait, ReconnectWait::Retry),
                "streak {streak} retries after the backoff"
            );
            assert_eq!(
                start.elapsed(),
                redial_backoff(streak),
                "streak {streak} waits exactly its ladder step"
            );
        }
        // The seventh step and beyond are capped at 60 s.
        assert_eq!(redial_backoff(7), Duration::from_secs(60));
    }

    /// Ruling E41 audit, MAJOR 1: a venue that answers one frame and then closes
    /// resets the unproductive streak but must NOT redial with no delay — the
    /// productive-drop path is paced by the pre-commit ladder (500 ms → 8 s),
    /// keyed on the cumulative attempt, and is never zero.
    #[tokio::test(start_paused = true)]
    async fn productive_drop_redial_is_paced_by_the_pre_commit_ladder_and_never_zero() {
        let (_trigger, shared) = live_shutdown_channel();
        let mut shutdown = ShutdownWait::new(Some(shared));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10 * 3600);

        // Repeated drop-after-one-frame: the delay doubles from 500 ms toward the
        // 8 s cap and is always at least the 500 ms floor — never zero.
        let mut previous = Duration::ZERO;
        for attempt in 1..=6_u64 {
            let delay = reconnect_delay(attempt);
            assert!(
                delay >= Duration::from_millis(500),
                "attempt {attempt} never redials with no delay"
            );
            assert!(delay >= previous, "attempt {attempt} does not shrink");
            previous = delay;
            let start = tokio::time::Instant::now();
            let wait = wait_before_redial(delay, deadline, &mut shutdown)
                .await
                .expect("the productive-drop wait resolves");
            assert!(matches!(wait, ReconnectWait::Retry));
            assert_eq!(
                start.elapsed(),
                delay,
                "attempt {attempt} waits its exact delay"
            );
        }
    }

    /// Ruling (global follow-up to E37d): an unexpected binary/continuation frame
    /// on a venue websocket admits exactly one counted `UnsupportedMessage` notice
    /// (no event, no recovery) before the loop drops the socket, so the stream
    /// survives the frame instead of a hard error cancelling every sibling lane
    /// that shares the run's `try_join!`. The socket read loop itself is
    /// network-bound, so the survive-and-reconnect path is exercised through this
    /// admission unit and the reconnect-ladder tests above. A dispatch fault on a
    /// decoded text frame is the opposite case and stays fatal — see
    /// `a_text_frame_adapter_refusal_stays_fatal_while_a_binary_frame_degrades`.
    #[test]
    fn an_unexpected_frame_admits_one_counted_unsupported_notice() {
        use crate::FeedNoticeCode;
        let mut counters = LiveCounters::default();
        let mut seen: Vec<(FeedNoticeCode, usize, bool)> = Vec::new();
        let mut handle = |timed: TimedIngestOutcome| {
            for notice in &timed.outcome.notices {
                seen.push((
                    notice.code,
                    timed.outcome.events.len(),
                    notice.recovery.is_some(),
                ));
            }
            Ok(LiveControl::Continue)
        };

        admit_unsupported_message(
            "unexpected binary/continuation frame on coinbase-core",
            42,
            &mut counters,
            &mut handle,
        )
        .expect("an unsupported-frame notice admits");

        assert_eq!(seen, vec![(FeedNoticeCode::UnsupportedMessage, 0, false)]);
        assert_eq!(counters.notices, 1, "the notice is counted");
        assert_eq!(counters.events, 0, "no event is manufactured");
        assert_eq!(
            counters.recoveries, 0,
            "a survivable drop opens no repair action"
        );
    }
}

#[cfg(test)]
mod pyth_hermes_status_tests {
    use super::hermes_status_is_terminal;

    #[test]
    fn only_401_403_404_are_terminal() {
        // A re-key or an id/path change: terminal, so the run stops rather than
        // redialing identical refusals.
        for status in [401, 403, 404] {
            assert!(
                hermes_status_is_terminal(status),
                "HTTP {status} must be terminal"
            );
        }
        // Everything else — 5xx and other transients — keeps the reconnect ladder.
        for status in [200, 400, 408, 429, 500, 502, 503, 504] {
            assert!(
                !hermes_status_is_terminal(status),
                "HTTP {status} must stay retryable"
            );
        }
    }
}
