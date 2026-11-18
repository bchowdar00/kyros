//! Bounded provider capture: record one channel family verbatim to an
//! append-only, owner-private JSONL file.
//!
//! Capture never normalizes. Frames and HTTP bodies are written as received so a
//! later replay through the deterministic adapters is the sole source of events,
//! and a capture stays re-interpretable when normalization changes. Every run is
//! bounded on four axes — duration, record count, output bytes, and connection
//! attempts — so an unattended run cannot fill a disk. Transport faults redial
//! with capped backoff; a reached deadline or an operator signal ends the run
//! through the same durable-close path, and a close that fails after a capture
//! failure surfaces both causes rather than hiding one.
//!
//! This module also owns the wire constants the live runners share: endpoints,
//! per-stream read-idle timeouts, heartbeat cadences and payloads, and the
//! subscription builders. Credential-bearing streams (Kraken level three,
//! Kalshi, SportsDataIO) keep secrets out of the recorded copy, and every
//! SportsDataIO error passes the redaction choke point before it leaves here.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use serde_json::{Value, json};
use thiserror::Error;
use tokio::time::{Instant, sleep, sleep_until, timeout};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::{Message, WebSocketConfig};
use tokio_tungstenite::{
    Connector, MaybeTlsStream, WebSocketStream, connect_async_tls_with_config,
};
use zeroize::Zeroizing;

use signals_core::{
    ArmedIndexSockets, BYBIT_LANE, EnabledVenues, INDEX_SOCKET_TOKENS, LaneStart, VENUE_GATE_ENV,
    lane_start,
};

use crate::MAX_MESSAGE_BYTES;
use crate::kalshi::{
    KalshiDiscovery, KalshiDiscoveryError, discover_market as discover_kalshi_market,
};
use crate::kalshi_auth::{
    KALSHI_WEBSOCKET_HOST, KalshiAuthClient, websocket_endpoint as kalshi_websocket_endpoint,
};
use crate::kraken_auth::{KrakenAuthClient, KrakenAuthError, KrakenWebSocketToken};
use crate::polymarket::{
    PolymarketDiscovery, PolymarketDiscoveryError, discover_market,
    websocket_endpoint as polymarket_websocket_endpoint,
};
use crate::pyth_hermes_auth::{PythHermesAuthError, PythHermesCredential};
use crate::raw::{
    CaptureStream, FailureKind, Provider, RawRecord, RawRecordError, RawRecorder, RecordKind,
};
use crate::secure_http;
use crate::sportsdataio_auth::{SportsDataIoAuthError, SportsDataIoCredential, redact_secret};
use crate::sportsdataio_nba::{
    RequestMatrix, RequestQuota, ResponseBudget, ResponseOutcome, SportsDataIoNbaError,
};

// Market-data-only Binance host ("User data stream is NOT available from this
// URL"); this capture path never uses user-data streams, and the host stays
// reachable in regions where stream.binance.com refuses connections.
const BINANCE_URL: &str = "wss://data-stream.binance.vision/stream";
// Binance USD-M futures market data, as COMBINED-STREAM URLs.
//
// The `/market` and `/public` paths this group first built to came from the
// spec's reading of a documentation page nobody could render; Binance publishes
// no such paths, the upgrade fails, and the failure was silent -- which is how a
// 65-minute capture produced zero records (ruling E27, and the reason every
// failed dial is now logged below). The real shapes are the raw
// `/ws/<stream>` form and the combined `/stream?streams=<a>/<b>` form, the same
// combined form the spot mirror uses; the combined URL is confirmed to upgrade
// with 101.
//
// The connection NAMES stay: `market` still means the low-frequency families
// and `public` the depth family, because that split is what binds each adapter
// to the source ids it may emit. Only the paths were never real. Subscribing
// through the URL is also why neither connection sends a SUBSCRIBE frame; see
// `subscriptions`.
//
// These are compiled constants, so no runtime value can redirect a feed
// (`docs/SECURITY.md`); `binance_usdm_urls_are_the_combined_form_of_their_streams`
// pins each one to the stream set its adapter compiles, so the URL and the
// dispatch cannot drift apart.
//
// DOCUMENTED, UNOBSERVED (ruling R1): no USD-M frame has been captured yet.
// Probe P3 IS now answered and the answer is split by region (E27): from the
// Tokyo box `fstream` never completes a connection while `fapi` REST answers;
// from a US host `fstream` upgrades in half a second while `fapi` returns 451.
// There is no `.vision` mirror for USD-M, so the spot host's geo workaround does
// not transfer, and where a USD-M lane can run at all is an owner item.
const BINANCE_USDM_PUBLIC_URL: &str =
    "wss://fstream.binance.com/stream?streams=btcusdt@depth20@100ms";
// Bybit v5 public spot and linear market-data endpoints. Both hosts terminate
// TLS 1.3, so they use the shared TLS 1.3-only client config; no exception is
// needed. Public topics require no API key.
const BYBIT_SPOT_URL: &str = "wss://stream.bybit.com/v5/public/spot";
const BYBIT_LINEAR_URL: &str = "wss://stream.bybit.com/v5/public/linear";
// Bybit v5 REST (R8: documented global host). The two linear poll identities
// (open interest, funding history) share one capture stream and one 1 req/s
// local spacing bound; each carries its query verbatim and limit=1 (a bounded
// rolling window, never retained — SIG-050). `startTime` is never sent.
const BYBIT_REST_BASE: &str = "https://api.bybit.com";
const BYBIT_OPEN_INTEREST_URL: &str = "https://api.bybit.com/v5/market/open-interest?category=linear&symbol=BTCUSDT&intervalTime=5min&limit=1";
const BYBIT_FUNDING_HISTORY_URL: &str =
    "https://api.bybit.com/v5/market/funding/history?category=linear&symbol=BTCUSDT&limit=1";
/// The IP-wide pause floor at which an honoured `Retry-After` starts after a
/// Binance USD-M `429`/`418` (ruling H1). Equal to the fastest pre-premium-index
/// endpoint cadence (the 5 s open-interest poll) and deliberately independent of
/// the endpoint cadences: adding the 1 s premium-index endpoint must never lower
/// the floor, or an honoured `Retry-After: 0` would resume in 1 s against an
/// endpoint that just refused us. The escalation ladder above it, and the 60 s
/// ban-avoidance floor it escalates toward, live in
/// [`crate::binance_usdm_rest::RateLimitBackoff`].
const IP_PAUSE_FLOOR: Duration = Duration::from_secs(5);
const BYBIT_REST_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const BYBIT_OPEN_INTEREST_POLL_CADENCE: Duration = Duration::from_secs(300);
const BYBIT_FUNDING_POLL_CADENCE: Duration = Duration::from_secs(3_600);
const BYBIT_REST_MIN_REQUEST_SPACING: Duration = Duration::from_secs(1);
// Transport / non-success backoff (spec §7: base 5 s, cap 300 s). Distinct from
// the served-error (retCode) ladder, which floors at the poll cadence and caps
// at one hour so a rate-limited venue is not re-hit every cadence.
const BYBIT_REST_BACKOFF_BASE: Duration = Duration::from_secs(5);
const BYBIT_REST_BACKOFF_CAP: Duration = Duration::from_secs(300);
const BYBIT_RETCODE_BACKOFF_FLOOR: Duration = Duration::from_secs(60);
const BYBIT_RETCODE_BACKOFF_CAP: Duration = Duration::from_secs(3_600);
const COINBASE_URL: &str = "wss://ws-feed.exchange.coinbase.com";
const KRAKEN_URL: &str = "wss://ws.kraken.com/v2";
const KRAKEN_LEVEL3_URL: &str = "wss://ws-l3.kraken.com/v2";
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_CAPTURE_DURATION: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const MAX_CAPTURE_RECORDS: u64 = 10_000_000;
const MAX_CAPTURE_BYTES: u64 = 64 * 1024 * 1024 * 1024;
const MAX_BACKOFF: Duration = Duration::from_secs(15);
/// Ceiling on the unproductive-connection redial ladder. Longer than
/// [`MAX_BACKOFF`] on purpose: that bound paces retries against a venue that is
/// answering, while this one paces them against a venue that is not.
const REDIAL_MAX_BACKOFF: Duration = Duration::from_secs(60);
const BINANCE_READ_IDLE_TIMEOUT: Duration = Duration::from_secs(10);
// The 1 Hz mark price is the liveness carrier on `/market` and depth arrives at
// 100 ms on `/public`, so ten seconds of silence on either is already an
// anomaly. Same value as the spot socket, for the same reason.
const BINANCE_USDM_READ_IDLE_TIMEOUT: Duration = Duration::from_secs(10);
// Binance closes a USD-M socket at 24 h. Rotating at 23 h means the switchover
// happens on our schedule, with a replacement already proven live, instead of
// as a venue-initiated drop in the middle of a window.
const BINANCE_USDM_CONNECTION_LIFETIME: Duration = Duration::from_secs(23 * 60 * 60);
// A rotation whose replacement never proved live is retried on this interval
// rather than a full lifetime later: the old socket is still carrying data, so
// the retry is cheap, but waiting 23 h would mean meeting the venue's own 24 h
// close with no replacement in hand.
const ROTATION_RETRY_INTERVAL: Duration = Duration::from_secs(60);
// How long the retiring socket is drained after its replacement is live. Two
// seconds is generously past the dial round-trip that opened the gap, and it is
// bounded so a chatty socket cannot hold the rotation open.
const ROTATION_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
// Cap on frames taken from a retiring socket, so the drain cannot buffer without
// limit on a fast stream.
const ROTATION_DRAIN_MAX_FRAMES: usize = 4_096;
// On a heartbeat stream the read-idle timeout is enforced at each heartbeat
// tick by comparing the time since the last successful read (see the capture
// loop); the idle timer alone would be perpetually re-armed by the tick. Bybit
// answers each ~20s client ping with a `pong` text frame, so 30s comfortably
// exceeds the healthy read cadence even on the sparse linear liquidation stream.
const BYBIT_READ_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const COINBASE_READ_IDLE_TIMEOUT: Duration = Duration::from_secs(10);
const KRAKEN_READ_IDLE_TIMEOUT: Duration = Duration::from_secs(5);
const POLYMARKET_READ_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const KALSHI_READ_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) const POLYMARKET_TEXT_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
const POLYMARKET_RTDS_TEXT_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
// Bybit's connection is dropped at the idle cap unless the client sends a ping;
// the docs recommend one every 20 seconds.
pub(crate) const BYBIT_TEXT_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(20);
// Client-initiated Bybit keepalive (inverted from Binance, which needs none).
pub(crate) const BYBIT_PING_PAYLOAD: &str = r#"{"op":"ping"}"#;
// Polymarket's keepalive is a bare `PING` literal rather than a JSON frame. This
// is the one authority for that literal, shared with the live runners.
pub(crate) const POLYMARKET_PING_PAYLOAD: &str = "PING";
pub(crate) const KRAKEN_USDT_BOOK_DEPTH: u16 = 10;

/// Bounded provider-feed capture configuration.
#[derive(Debug, Clone)]
pub struct CaptureOptions {
    /// Which provider channel family to connect to and record.
    pub stream: CaptureStream,
    /// Destination path for the append-only, owner-private JSONL capture file.
    pub output: PathBuf,
    /// How long to record before the deadline. Must be at least 1 second and at
    /// most 7 days.
    pub duration: Duration,
    /// Hard cap on recorded records, between 1 and 10_000_000. Reaching it ends
    /// the run with `RecordLimitReached`.
    pub max_records: u64,
    /// Hard cap on output bytes, between 1 and 64 GiB. Reaching it stops the run
    /// normally and still returns a report.
    pub max_bytes: u64,
    /// Requested Kraken BTC book depth in price levels. KrakenLevel3 accepts
    /// 10, 100 or 1000; other streams also accept 25 and 500.
    pub kraken_btc_book_depth: u16,
    /// Polymarket market slug to resolve through Gamma discovery. Required for
    /// the Polymarket market stream and rejected for every other stream.
    pub polymarket_slug: Option<String>,
    /// Kalshi market ticker to resolve through keyless REST discovery. Required
    /// for the Kalshi market stream and rejected for every other stream.
    pub kalshi_market_ticker: Option<String>,
    /// Path to the SportsDataIO NBA request-matrix control file. Valid ONLY for
    /// that stream; the file names operations and parameters from the compiled
    /// allowlist and never contains a credential.
    pub sportsdataio_control_file: Option<PathBuf>,
    /// Shorten the proactive connection lifetime, for exercising the rotation
    /// path without waiting 23 hours.
    ///
    /// A diagnostic, like the live runner's induced-reconnect knob, and it can
    /// only make an EXISTING rotation happen sooner: setting it on a stream
    /// with no [`connection_lifetime`] of its own refuses the run. Rotation
    /// dials a bare endpoint and sends no subscribe frame, so arming it on a
    /// stream that needs discovery, a credential or a SUBSCRIBE would trade a
    /// live subscribed socket for a silent one.
    pub connection_lifetime_override: Option<Duration>,
}

impl CaptureOptions {
    fn validate(&self) -> Result<(), CaptureError> {
        if self.duration.is_zero() || self.duration > MAX_CAPTURE_DURATION {
            return Err(CaptureError::InvalidOptions(format!(
                "duration must be between 1 second and {} seconds",
                MAX_CAPTURE_DURATION.as_secs()
            )));
        }
        if self.max_records == 0 || self.max_records > MAX_CAPTURE_RECORDS {
            return Err(CaptureError::InvalidOptions(format!(
                "max_records must be between 1 and {MAX_CAPTURE_RECORDS}"
            )));
        }
        if self.max_bytes == 0 || self.max_bytes > MAX_CAPTURE_BYTES {
            return Err(CaptureError::InvalidOptions(format!(
                "max_bytes must be between 1 and {MAX_CAPTURE_BYTES}"
            )));
        }
        let valid_depth = match self.stream {
            CaptureStream::KrakenLevel3 => matches!(self.kraken_btc_book_depth, 10 | 100 | 1000),
            _ => matches!(self.kraken_btc_book_depth, 10 | 25 | 100 | 500 | 1000),
        };
        if !valid_depth {
            return Err(CaptureError::InvalidOptions(
                "Kraken depth is not supported by the selected stream".to_owned(),
            ));
        }
        match (self.stream, self.polymarket_slug.as_deref()) {
            (CaptureStream::PolymarketMarket, Some(slug)) => {
                crate::polymarket::validate_slug(slug).map_err(|error| {
                    CaptureError::InvalidOptions(format!("invalid Polymarket slug: {error}"))
                })?;
            }
            (CaptureStream::PolymarketMarket, None) => {
                return Err(CaptureError::InvalidOptions(
                    "polymarket_slug is required for the Polymarket market stream".to_owned(),
                ));
            }
            (_, Some(_)) => {
                return Err(CaptureError::InvalidOptions(
                    "polymarket_slug is only valid for the Polymarket market stream".to_owned(),
                ));
            }
            (_, None) => {}
        }
        match (self.stream, self.kalshi_market_ticker.as_deref()) {
            (CaptureStream::KalshiMarket, Some(ticker)) => {
                crate::kalshi::validate_market_ticker(ticker).map_err(|error| {
                    CaptureError::InvalidOptions(format!("invalid Kalshi market ticker: {error}"))
                })?;
            }
            (CaptureStream::KalshiMarket, None) => {
                return Err(CaptureError::InvalidOptions(
                    "kalshi_market_ticker is required for the Kalshi market stream".to_owned(),
                ));
            }
            (_, Some(_)) => {
                return Err(CaptureError::InvalidOptions(
                    "kalshi_market_ticker is only valid for the Kalshi market stream".to_owned(),
                ));
            }
            (_, None) => {}
        }
        match (self.stream, self.sportsdataio_control_file.as_ref()) {
            (CaptureStream::SportsDataIoNba, Some(_)) => {}
            (CaptureStream::SportsDataIoNba, None) => {
                return Err(CaptureError::InvalidOptions(
                    "sportsdataio_control_file is required for the SportsDataIO NBA stream"
                        .to_owned(),
                ));
            }
            (_, Some(_)) => {
                return Err(CaptureError::InvalidOptions(
                    "sportsdataio_control_file is only valid for the SportsDataIO NBA stream"
                        .to_owned(),
                ));
            }
            (_, None) => {}
        }
        Ok(())
    }
}

/// Counter totals for one completed capture run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureReport {
    /// The provider the captured stream belongs to.
    pub provider: Provider,
    /// Count of records appended to the JSONL file; also the final ordinal.
    pub records: u64,
    /// Bytes flushed to the output file over the run, counted after the closing
    /// durability flush.
    pub bytes_written: u64,
    /// Count of dial or poll attempts, including those that failed or timed out.
    pub connection_attempts: u64,
    /// Count of attempts that produced an established connection. Zero with no
    /// interrupt is the `NoConnection` failure.
    pub successful_connections: u64,
    /// Count of reads that produced no response at all — transport, TLS,
    /// timeout, size or read failure — each of which closed the connection.
    pub read_idle_timeouts: u64,
    /// Provider ANSWERS that were recorded as evidence but carried no usable 2xx
    /// body: a 401, 429, 5xx or refused redirect. Counted separately from
    /// `read_idle_timeouts`, which stays what its name says -- a read that never
    /// produced a response at all (transport, TLS, timeout, size or read failure).
    /// Only the SportsDataIO matrix loop records non-2xx answers, so this is zero
    /// for every stream that discards them.
    pub non_success_responses: u64,
    /// Provider answers served with HTTP 200 that carried a non-zero `retCode`
    /// or an empty `result.list`: a rate-limit (10006) or an empty-window reply.
    /// Counted so an hour of rate-limiting is distinguishable from a healthy hour
    /// (only the Bybit REST poll loop sets it; zero for every other stream).
    pub served_error_responses: u64,
    /// True when an operator stop signal ended the run before its deadline.
    pub interrupted: bool,
}

/// Capture a selected channel family to an append-only private JSONL file.
pub async fn run_capture(options: CaptureOptions) -> Result<CaptureReport, CaptureError> {
    options.validate()?;
    // Audit A1/F3: the index socket is rights-gated (R7), and `btc-feeds
    // capture` is the only path that can dial it today. The gate is applied here
    // — before the credential is read and before any byte reaches disk — so an
    // operator cannot record rights-unresolved index data by running the capture
    // binary. `signals_core::venue_gate` is the one authority for the parse.
    let index_sockets = armed_index_sockets(options.stream)?;
    // Rotation is wired for the connections whose adapter declares a session
    // limit, and for nothing else. Refusing here — a real check, before any
    // byte reaches disk — is what keeps the diagnostic from arming a rotation
    // path that would dial a bare endpoint on a stream needing discovery, a
    // credential or a SUBSCRIBE frame.
    rotation_supported(&options)?;
    // Ruling R7: Binance is a rights-gated venue. The gate is applied here --
    // before any byte reaches disk -- so an operator cannot record USD-M data
    // while the RIGHTS-LEDGER row is open, exactly as the Kalshi index gate
    // above does for its own socket.
    require_enabled_venue(options.stream)?;
    // R7 rights gate: the Bybit venue is rights-unverified, so its three capture
    // streams refuse to start unless `SIGNALS_ENABLED_VENUES` lists `bybit`. The
    // gate is applied here — before any byte reaches disk — mirroring the index
    // socket above. A lane binary (group A6) maps this refusal onto EXIT_FATAL.
    ensure_bybit_capture_enabled(options.stream)?;
    install_crypto_provider()?;
    let kraken_auth = if options.stream == CaptureStream::KrakenLevel3 {
        Some(KrakenAuthClient::from_env()?)
    } else {
        None
    };
    let kalshi_auth = if matches!(
        options.stream,
        CaptureStream::KalshiMarket | CaptureStream::KalshiIndex
    ) {
        Some(KalshiAuthClient::from_env()?)
    } else {
        None
    };
    // Load the credential and compile the request matrix BEFORE the output file
    // exists: a missing credential or an invalid control file must refuse the run
    // without leaving a zero-record journal behind.
    let sportsdataio_credential = if options.stream == CaptureStream::SportsDataIoNba {
        Some(SportsDataIoCredential::from_env()?)
    } else {
        None
    };
    let sportsdataio_matrix = if options.stream == CaptureStream::SportsDataIoNba {
        // REDACTION CHOKE POINT (control-file load). The control file is the only
        // operator input on this path and its refusals echo operator values, so an
        // operator who pastes the credential into a field -- the accidental-paste
        // threat the gate names -- must not see it echoed back through serde's
        // message or our own. The credential is already loaded above; every error
        // this load can surface passes through the same redactor the capture loop
        // uses.
        let credential = sportsdataio_credential
            .as_ref()
            .ok_or(CaptureError::MissingSportsDataIoCredential)?;
        let path = options.sportsdataio_control_file.as_ref().ok_or_else(|| {
            CaptureError::InvalidOptions(
                "sportsdataio_control_file is required for the SportsDataIO NBA stream".to_owned(),
            )
        })?;
        Some(
            crate::sportsdataio_nba::read_control_file(path)
                .map_err(|error| redact_capture_error(CaptureError::from(error), credential))?,
        )
    } else {
        None
    };
    // The Pyth Hermes feed is refuse-closed behind its key: the credential is
    // loaded from the environment BEFORE the output file exists, so a missing key
    // refuses the run (exit 78) without leaving a zero-record journal behind. The
    // key is held in zeroizing memory and attached only as a request header
    // (`pyth_hermes_auth`); it never reaches argv, a log, a notice or a record.
    let pyth_hermes_credential = if matches!(
        options.stream,
        CaptureStream::PythHermesStream | CaptureStream::PythHermesRest
    ) {
        Some(PythHermesCredential::from_env()?)
    } else {
        None
    };
    let polymarket_discovery = if options.stream == CaptureStream::PolymarketMarket {
        Some(
            discover_market(options.polymarket_slug.as_deref().ok_or_else(|| {
                CaptureError::InvalidOptions("polymarket_slug is required for discovery".to_owned())
            })?)
            .await?,
        )
    } else {
        None
    };
    let kalshi_discovery = if options.stream == CaptureStream::KalshiMarket {
        Some(
            discover_kalshi_market(options.kalshi_market_ticker.as_deref().ok_or_else(|| {
                CaptureError::InvalidOptions(
                    "kalshi_market_ticker is required for discovery".to_owned(),
                )
            })?)
            .await?,
        )
    } else {
        None
    };
    let mut recorder = RawRecorder::create(&options.output, options.max_bytes).await?;
    let mut state = CaptureState::default();
    let capture_started = Instant::now();
    if let Some(discovery) = &polymarket_discovery {
        append_configuration_record(
            &mut recorder,
            options.stream,
            &mut state.ordinal,
            options.max_records,
            capture_started,
            discovery.journal_payload.clone(),
        )
        .await?;
    }
    if let Some(discovery) = &kalshi_discovery {
        append_configuration_record(
            &mut recorder,
            options.stream,
            &mut state.ordinal,
            options.max_records,
            capture_started,
            discovery.journal_payload.clone(),
        )
        .await?;
    }
    let dependencies = CaptureDependencies {
        kraken_auth: kraken_auth.as_ref(),
        kalshi_auth: kalshi_auth.as_ref(),
        polymarket_discovery: polymarket_discovery.as_ref(),
        kalshi_discovery: kalshi_discovery.as_ref(),
        index_sockets,
    };
    // The Chainlink stream rides a low-frequency HTTP JSON-RPC poll, not a
    // WebSocket, so it dispatches to a dedicated poll loop; the WebSocket
    // capture_loop is unchanged for every other stream.
    let capture_result = if options.stream == CaptureStream::ChainlinkAggregator {
        chainlink_poll_loop(&options, &mut recorder, &mut state, capture_started).await
    } else if options.stream == CaptureStream::BinanceUsdmRest {
        binance_usdm_rest_poll_loop(&options, &mut recorder, &mut state, capture_started).await
    } else if options.stream == CaptureStream::BybitLinearRest {
        bybit_rest_poll_loop(&options, &mut recorder, &mut state, capture_started).await
    } else if options.stream == CaptureStream::NbaEspnStatus {
        nba_espn_poll_loop(&options, &mut recorder, &mut state, capture_started).await
    } else if options.stream == CaptureStream::NbaEspnDaily {
        nba_espn_daily_crawl_loop(&options, &mut recorder, &mut state, capture_started).await
    } else if options.stream == CaptureStream::NbaEspnGames {
        nba_espn_games_poll_loop(&options, &mut recorder, &mut state, capture_started).await
    } else if options.stream == CaptureStream::PythHermesStream {
        let credential = pyth_hermes_credential
            .as_ref()
            .ok_or(CaptureError::MissingPythHermesCredential)?;
        pyth_hermes_sse_loop(
            &options,
            credential,
            &mut recorder,
            &mut state,
            capture_started,
        )
        .await
    } else if options.stream == CaptureStream::PythHermesRest {
        let credential = pyth_hermes_credential
            .as_ref()
            .ok_or(CaptureError::MissingPythHermesCredential)?;
        pyth_hermes_rest_poll_loop(
            &options,
            credential,
            &mut recorder,
            &mut state,
            capture_started,
        )
        .await
    } else if options.stream == CaptureStream::SportsDataIoNba {
        let credential = sportsdataio_credential
            .as_ref()
            .ok_or(CaptureError::MissingSportsDataIoCredential)?;
        let matrix = sportsdataio_matrix
            .as_ref()
            .ok_or(CaptureError::MissingSportsDataIoMatrix)?;
        // FINAL REDACTION CHOKE POINT: every error this loop can surface -- to
        // stderr, to a report, to a journal-adjacent log -- passes through the
        // credential redactor before it leaves the capture path.
        sportsdataio_nba_matrix_loop(
            &options,
            credential,
            matrix,
            &mut recorder,
            &mut state,
            capture_started,
        )
        .await
        .map_err(|error| redact_capture_error(error, credential))
    } else {
        capture_loop(
            &options,
            &dependencies,
            &mut recorder,
            &mut state,
            capture_started,
        )
        .await
    };
    let capture_result = match capture_result {
        Err(CaptureError::Raw(RawRecordError::CaptureSizeLimit { .. })) => Ok(()),
        result => result,
    };
    let bytes_written = finalize_capture(recorder, capture_result).await?;
    if state.successful_connections == 0 && !state.interrupted {
        return Err(CaptureError::NoConnection);
    }
    Ok(CaptureReport {
        provider: options.stream.provider(),
        records: state.ordinal,
        bytes_written,
        connection_attempts: state.connection_attempts,
        successful_connections: state.successful_connections,
        read_idle_timeouts: state.read_idle_timeouts,
        non_success_responses: state.non_success_responses,
        served_error_responses: state.served_error_responses,
        interrupted: state.interrupted,
    })
}

#[derive(Default)]
struct CaptureState {
    ordinal: u64,
    connection_epoch: u64,
    connection_attempts: u64,
    successful_connections: u64,
    read_idle_timeouts: u64,
    non_success_responses: u64,
    served_error_responses: u64,
    interrupted: bool,
}

struct CaptureDependencies<'a> {
    kraken_auth: Option<&'a KrakenAuthClient>,
    kalshi_auth: Option<&'a KalshiAuthClient>,
    polymarket_discovery: Option<&'a PolymarketDiscovery>,
    kalshi_discovery: Option<&'a KalshiDiscovery>,
    /// Kalshi index-channel families the venue gate armed. Empty for every
    /// stream that carries no index channel.
    index_sockets: ArmedIndexSockets,
}

async fn capture_loop(
    options: &CaptureOptions,
    dependencies: &CaptureDependencies<'_>,
    recorder: &mut RawRecorder,
    state: &mut CaptureState,
    capture_started: Instant,
) -> Result<(), CaptureError> {
    let deadline = capture_started + options.duration;
    let mut shutdown = Box::pin(tokio::signal::ctrl_c());
    // Consecutive connections that produced no data — a dial that failed, or one
    // that opened and delivered nothing. A connection that delivered a frame
    // resets it and is instead paced by the productive-drop ladder at the bottom
    // of the loop (`reconnect_delay`), never at zero delay. Without either ladder
    // a venue that accepts the socket and then says nothing — or one that answers
    // once and closes — is re-dialled the instant the idle timeout fires, for
    // ever.
    let mut unproductive_streak: u64 = 0;

    'capture: while Instant::now() < deadline && state.ordinal < options.max_records {
        if unproductive_streak > 0 {
            let delay = redial_backoff(unproductive_streak);
            if redial_backoff_stepped(unproductive_streak) {
                eprintln!(
                    "capture {}: {unproductive_streak} connections produced no data; \
                     backing off {delay:?} before re-dialling {}",
                    options.stream.as_str(),
                    endpoint(options.stream)
                );
            }
            tokio::select! {
                result = &mut shutdown => {
                    result.map_err(CaptureError::Signal)?;
                    state.interrupted = true;
                    break 'capture;
                }
                () = sleep_until(deadline) => {
                    break 'capture;
                }
                () = sleep(delay) => {}
            }
        }
        state.connection_attempts = state
            .connection_attempts
            .checked_add(1)
            .ok_or(CaptureError::CounterExhausted)?;
        let request = if let Some(auth) = dependencies.kalshi_auth {
            // Observed (E23): the index channels use the market command envelope,
            // not the per-channel paths Kalshi documents, and this capture stream
            // opens its own connection to the same endpoint (as does the live
            // lane's dedicated index socket, ruling E37d). The dial stays explicit
            // rather than folding into `websocket_request()` so the seam holds for
            // the oracle lane, which opens its own socket later.
            if options.stream == CaptureStream::KalshiIndex {
                auth.websocket_request_for(
                    KALSHI_WEBSOCKET_HOST,
                    crate::kalshi_index::INDEX_WEBSOCKET_PATH,
                )?
            } else {
                auth.websocket_request()?
            }
        } else {
            endpoint(options.stream)
                .into_client_request()
                .map_err(CaptureError::WebSocket)?
        };
        let config = websocket_config();
        // data-stream.binance.vision terminates TLS at 1.2; every other provider
        // endpoint keeps the TLS 1.3-only floor. `client_config_for` is the sole
        // owner of that mapping (P4 note lives there); every connect path selects
        // through it so none can silently pick the wrong version.
        let tls_client_config = crate::websocket_tls::client_config_for(options.stream);
        let connect_started_elapsed_ns = elapsed_ns(capture_started)?;

        let connected = tokio::select! {
            result = &mut shutdown => {
                result.map_err(CaptureError::Signal)?;
                state.interrupted = true;
                break 'capture;
            }
            () = sleep_until(deadline) => {
                break 'capture;
            }
            result = timeout(
                CONNECT_TIMEOUT,
                connect_async_tls_with_config(
                    request,
                    Some(config),
                    true,
                    Some(Connector::Rustls(tls_client_config)),
                ),
            ) => result,
        };

        let (mut socket, _) = match connected {
            Ok(Ok(connection)) => connection,
            outcome @ (Ok(Err(_)) | Err(_)) => {
                // Ruling E27(c). A dial that never completes used to be entirely
                // silent: the Tokyo box ran this loop for 65 minutes against a
                // blackholed `fstream.binance.com`, recorded nothing, and said
                // nothing. The capture file cannot hold the failure -- there is
                // no connection epoch to attach a record to -- so stderr is the
                // only honest channel, and every attempt reports its cause.
                unproductive_streak = unproductive_streak.saturating_add(1);
                if redial_backoff_stepped(unproductive_streak) {
                    report_failed_connection(
                        options.stream,
                        state.connection_attempts,
                        &match outcome {
                            Ok(Err(error)) => error.to_string(),
                            Err(_) => format!("no response within {CONNECT_TIMEOUT:?}"),
                            Ok(Ok(_)) => unreachable!("the success arm returned above"),
                        },
                    );
                }
                // The wait happens at the top of the loop, on the one ladder
                // both failure shapes share.
                continue;
            }
        };

        state.connection_epoch = state
            .connection_epoch
            .checked_add(1)
            .ok_or(CaptureError::CounterExhausted)?;
        state.successful_connections = state
            .successful_connections
            .checked_add(1)
            .ok_or(CaptureError::CounterExhausted)?;
        append_connected_record(
            recorder,
            options.stream,
            state.connection_epoch,
            &mut state.ordinal,
            options.max_records,
            capture_started,
            connect_started_elapsed_ns,
        )
        .await?;

        let token = if let Some(auth) = dependencies.kraken_auth {
            let token = tokio::select! {
                result = &mut shutdown => {
                    result.map_err(CaptureError::Signal)?;
                    state.interrupted = true;
                    break 'capture;
                }
                () = sleep_until(deadline) => {
                    break 'capture;
                }
                result = auth.websocket_token() => result?,
            };
            Some(token)
        } else {
            None
        };
        for subscription in subscriptions(
            options.stream,
            options.kraken_btc_book_depth,
            token.as_ref(),
            dependencies.polymarket_discovery,
            dependencies.kalshi_discovery,
            dependencies.index_sockets,
        )? {
            if state.ordinal >= options.max_records {
                break 'capture;
            }
            let sent_at = crate::raw::now_timestamp();
            let sent_elapsed_ns = elapsed_ns(capture_started)?;
            tokio::select! {
                result = &mut shutdown => {
                    result.map_err(CaptureError::Signal)?;
                    state.interrupted = true;
                    break 'capture;
                }
                () = sleep_until(deadline) => {
                    break 'capture;
                }
                result = socket.send(Message::Text(subscription.wire.as_str().to_owned().into())) => {
                    result.map_err(CaptureError::WebSocket)?;
                }
            }
            append_text_record(
                recorder,
                &mut state.ordinal,
                options.max_records,
                RawRecord::text_at(
                    options.stream,
                    state.connection_epoch,
                    1,
                    RecordKind::OutboundText,
                    subscription.recorded,
                    sent_at,
                    sent_elapsed_ns,
                ),
            )
            .await?;
        }

        // The heartbeat interval and payload come from one paired lookup so they
        // cannot disagree; the timer and payload are then held together for the
        // life of this connection.
        let heartbeat = text_heartbeat(options.stream);
        let mut heartbeat_timer = heartbeat
            .map(|(interval, _)| tokio::time::interval_at(Instant::now() + interval, interval));
        let heartbeat_payload = heartbeat.map(|(_, payload)| payload);
        // Time of the last successful read. Reset on connect and on every read,
        // this backs the heartbeat-tick idle check below.
        let mut last_read = Instant::now();
        // When this connection is due to be proactively replaced. `None` for
        // every stream with no venue session limit, which is every stream but
        // the two Binance USD-M sockets.
        // The override SHORTENS an existing lifetime: it never creates one
        // (`run_capture` refuses that) and never lengthens one. A longer value
        // would push the rotation past the venue's own session close, which is
        // the drop rotation exists to prevent, so it is taken as a minimum
        // rather than a replacement.
        let lifetime = connection_lifetime(options.stream).map(|lifetime| {
            options
                .connection_lifetime_override
                .map_or(lifetime, |shortened| shortened.min(lifetime))
        });
        let mut connected_at = Instant::now();
        // Did this connection deliver anything? A connection that produced data
        // was not a failure, however briefly it lived.
        let mut produced_data = false;
        loop {
            if state.ordinal >= options.max_records {
                break 'capture;
            }
            // MAKE BEFORE BREAK. The replacement is dialled and proven live —
            // one real data frame in hand — before the retiring socket is
            // closed, and the retiring socket is then drained for up to two
            // seconds so the dial round-trip is not a hole in the tape. Only
            // then is the changeover written down, and it is written in the one
            // order a replay can accept: the drained frames on the old epoch,
            // Disconnected on the old epoch, Connected on the new one, then the
            // frame that proved the replacement live. A capture file is a
            // sequence, so it cannot express two sockets open at once; what it
            // records is the instant the stream moved, and the adapter's
            // Disconnected/Connected notice pair is the rotation notice
            // (spec §7).
            //
            // WHAT THIS DOES NOT PROMISE. The drain is bounded, so a frame that
            // arrives on the retiring socket more than two seconds after the
            // replacement went live is lost; the drain also stops at the first
            // silence, so a stream that pauses mid-drain and resumes loses the
            // resumption. Duplicates are removed only where the venue gives us
            // a sequence to compare — depth `u`, aggregate-trade `a` — and only
            // against the replacement's own first frame; a `markPriceUpdate`
            // drained here and re-sent on the new connection can be published
            // twice, because that cursor resets on `Connected`. The
            // aggregate-trade cursor persists across epochs, so that stream is
            // covered by the adapter even when the sequence comparison is not
            // available. This is a small, bounded, notified window — not
            // lossless.
            if rotation_due(lifetime, connected_at.elapsed()) {
                match rotate_connection(
                    options,
                    capture_started,
                    &mut shutdown,
                    deadline,
                    &mut socket,
                )
                .await?
                {
                    RotationOutcome::Rotated(rotation) => {
                        let Rotation {
                            socket: replacement,
                            first_message,
                            connect_started_elapsed_ns,
                            drained,
                        } = *rotation;
                        // What the retiring socket delivered while we dialled,
                        // on its own epoch, before it is closed.
                        for frame in drained {
                            if state.ordinal >= options.max_records {
                                break 'capture;
                            }
                            let received_at = crate::raw::now_timestamp();
                            let drained_elapsed_ns = elapsed_ns(capture_started)?;
                            append_text_record(
                                recorder,
                                &mut state.ordinal,
                                options.max_records,
                                RawRecord::text_at(
                                    options.stream,
                                    state.connection_epoch,
                                    1,
                                    RecordKind::InboundText,
                                    frame,
                                    received_at,
                                    drained_elapsed_ns,
                                ),
                            )
                            .await?;
                        }
                        if state.ordinal < options.max_records {
                            append_disconnected_record(
                                recorder,
                                options.stream,
                                state.connection_epoch,
                                &mut state.ordinal,
                                options.max_records,
                                capture_started,
                            )
                            .await?;
                        }
                        // The old socket has no more to give us: its successor is
                        // already carrying the stream.
                        let _ = socket.close(None).await;
                        socket = replacement;
                        state.connection_epoch = state
                            .connection_epoch
                            .checked_add(1)
                            .ok_or(CaptureError::CounterExhausted)?;
                        state.successful_connections = state
                            .successful_connections
                            .checked_add(1)
                            .ok_or(CaptureError::CounterExhausted)?;
                        append_connected_record(
                            recorder,
                            options.stream,
                            state.connection_epoch,
                            &mut state.ordinal,
                            options.max_records,
                            capture_started,
                            connect_started_elapsed_ns,
                        )
                        .await?;
                        // The frame that proved the replacement live is real
                        // market data and is recorded on the new epoch rather
                        // than thrown away to keep the bookkeeping tidy. Its
                        // clock is stamped HERE, at write time, after the drained
                        // frames and the Disconnected/Connected records, so the
                        // file's `capture_elapsed_ns` stays monotonic in record
                        // order (the stamp sampled inside `rotate_connection`
                        // predates those writes and would move the clock
                        // backwards).
                        let received_at = crate::raw::now_timestamp();
                        let capture_elapsed_ns = elapsed_ns(capture_started)?;
                        append_text_record(
                            recorder,
                            &mut state.ordinal,
                            options.max_records,
                            RawRecord::text_at(
                                options.stream,
                                state.connection_epoch,
                                1,
                                RecordKind::InboundText,
                                first_message,
                                received_at,
                                capture_elapsed_ns,
                            ),
                        )
                        .await?;
                        connected_at = Instant::now();
                        last_read = Instant::now();
                        eprintln!(
                            "capture {}: rotated to a fresh connection at epoch {}",
                            options.stream.as_str(),
                            state.connection_epoch
                        );
                    }
                    RotationOutcome::Failed => {
                        // The old socket is still delivering, so a failed
                        // rotation is not a fault: retry soon rather than
                        // meeting the venue's own close with nothing in hand.
                        connected_at = Instant::now()
                            .checked_sub(
                                lifetime
                                    .unwrap_or(ROTATION_RETRY_INTERVAL)
                                    .saturating_sub(ROTATION_RETRY_INTERVAL),
                            )
                            .unwrap_or_else(Instant::now);
                    }
                    RotationOutcome::Interrupted => {
                        state.interrupted = true;
                        break 'capture;
                    }
                    RotationOutcome::Deadline => break 'capture,
                }
            }
            enum ReadAction {
                Message(
                    Result<
                        Option<Result<Message, tokio_tungstenite::tungstenite::Error>>,
                        tokio::time::error::Elapsed,
                    >,
                ),
                TextHeartbeat,
            }
            let action = tokio::select! {
                result = &mut shutdown => {
                    result.map_err(CaptureError::Signal)?;
                    state.interrupted = true;
                    break 'capture;
                }
                () = sleep_until(deadline) => {
                    break 'capture;
                }
                result = timeout(read_idle_timeout(options.stream), socket.next()) => ReadAction::Message(result),
                _ = async {
                    heartbeat_timer
                        .as_mut()
                        .expect("guarded text heartbeat exists")
                        .tick()
                        .await;
                }, if heartbeat_timer.is_some() => ReadAction::TextHeartbeat,
            };

            if matches!(action, ReadAction::TextHeartbeat) {
                // A heartbeat tick re-arms the read-idle timer every pass, so the
                // idle timeout can only be enforced here: if no read has arrived
                // within the timeout, treat it as a stall on the same path as a
                // read-idle Elapsed (count it, record the disconnect, reconnect).
                if read_idle_exceeded(last_read.elapsed(), read_idle_timeout(options.stream)) {
                    state.read_idle_timeouts = state
                        .read_idle_timeouts
                        .checked_add(1)
                        .ok_or(CaptureError::CounterExhausted)?;
                    if state.ordinal < options.max_records {
                        append_disconnected_record(
                            recorder,
                            options.stream,
                            state.connection_epoch,
                            &mut state.ordinal,
                            options.max_records,
                            capture_started,
                        )
                        .await?;
                    }
                    break;
                }
                if let Some(payload) = heartbeat_payload {
                    let sent_at = crate::raw::now_timestamp();
                    let sent_elapsed_ns = elapsed_ns(capture_started)?;
                    socket
                        .send(Message::Text(payload.to_owned().into()))
                        .await
                        .map_err(CaptureError::WebSocket)?;
                    append_text_record(
                        recorder,
                        &mut state.ordinal,
                        options.max_records,
                        RawRecord::text_at(
                            options.stream,
                            state.connection_epoch,
                            1,
                            RecordKind::OutboundText,
                            payload.to_owned(),
                            sent_at,
                            sent_elapsed_ns,
                        ),
                    )
                    .await?;
                }
                continue;
            }
            let ReadAction::Message(message) = action else {
                unreachable!("heartbeat action returned above")
            };

            match message {
                Ok(Some(Ok(Message::Text(text)))) => {
                    last_read = Instant::now();
                    produced_data = true;
                    let received_at = crate::raw::now_timestamp();
                    let capture_elapsed_ns = elapsed_ns(capture_started)?;
                    append_text_record(
                        recorder,
                        &mut state.ordinal,
                        options.max_records,
                        RawRecord::text_at(
                            options.stream,
                            state.connection_epoch,
                            1,
                            RecordKind::InboundText,
                            text.to_string(),
                            received_at,
                            capture_elapsed_ns,
                        ),
                    )
                    .await?;
                }
                Ok(Some(Ok(Message::Ping(bytes)))) => {
                    last_read = Instant::now();
                    tokio::select! {
                        result = &mut shutdown => {
                            result.map_err(CaptureError::Signal)?;
                            state.interrupted = true;
                            break 'capture;
                        }
                        () = sleep_until(deadline) => {
                            break 'capture;
                        }
                        result = socket.send(Message::Pong(bytes)) => {
                            result.map_err(CaptureError::WebSocket)?;
                        }
                    }
                }
                Ok(Some(Ok(Message::Pong(_)))) => {
                    last_read = Instant::now();
                }
                Ok(Some(Ok(Message::Close(_)))) | Ok(None) | Ok(Some(Err(_))) => {
                    if state.ordinal < options.max_records {
                        append_disconnected_record(
                            recorder,
                            options.stream,
                            state.connection_epoch,
                            &mut state.ordinal,
                            options.max_records,
                            capture_started,
                        )
                        .await?;
                    }
                    break;
                }
                Err(_) => {
                    state.read_idle_timeouts = state
                        .read_idle_timeouts
                        .checked_add(1)
                        .ok_or(CaptureError::CounterExhausted)?;
                    if state.ordinal < options.max_records {
                        append_disconnected_record(
                            recorder,
                            options.stream,
                            state.connection_epoch,
                            &mut state.ordinal,
                            options.max_records,
                            capture_started,
                        )
                        .await?;
                    }
                    break;
                }
                Ok(Some(Ok(Message::Binary(_)))) | Ok(Some(Ok(Message::Frame(_)))) => {
                    // Ruling (global follow-up to E37d): an unexpected
                    // binary/continuation frame is no longer fatal — the capture
                    // survives it as a socket drop, identically to a close, and
                    // the reconnect ladder re-dials. Capture never recorded such a
                    // frame as a `Failed` record before (it returned an error), and
                    // must not now: a `Failed` record on a websocket stream is
                    // rejected by every websocket adapter on replay, so the honest
                    // survivable representation is the disconnected record every
                    // other drop already writes.
                    if state.ordinal < options.max_records {
                        append_disconnected_record(
                            recorder,
                            options.stream,
                            state.connection_epoch,
                            &mut state.ordinal,
                            options.max_records,
                            capture_started,
                        )
                        .await?;
                    }
                    break;
                }
            }
        }
        // Two redial ladders, never zero (ruling E41 audit; mirror of the live
        // runner at live.rs). A connection that delivered a frame and then
        // dropped — a subscribe ack, an `op_response`, a `status: error`, a
        // rate-limit kick — is NOT "unproductive", so it resets that streak;
        // but it must still be PACED, or an answer-then-close venue becomes an
        // unthrottled reconnect storm that redials the instant the idle timeout
        // fires, for the whole capture. It is paced by the pre-existing
        // productive-drop ladder (`reconnect_delay`, 500 ms → 8 s, keyed on the
        // cumulative attempt); a connection that opened and said nothing walks
        // the unproductive ladder (`redial_backoff`, applied at the top of the
        // loop) instead.
        if produced_data {
            unproductive_streak = 0;
            let delay = reconnect_delay(state.connection_attempts);
            tokio::select! {
                result = &mut shutdown => {
                    result.map_err(CaptureError::Signal)?;
                    state.interrupted = true;
                    break 'capture;
                }
                () = sleep_until(deadline) => {
                    break 'capture;
                }
                () = sleep(delay) => {}
            }
        } else {
            unproductive_streak = unproductive_streak.saturating_add(1);
        }
    }
    Ok(())
}

/// True for every Bybit capture stream (spot, linear WebSocket, linear REST):
/// the whole venue rides one rights-gated allow-list token.
fn is_bybit_stream(stream: CaptureStream) -> bool {
    matches!(
        stream,
        CaptureStream::BybitSpot | CaptureStream::BybitLinear | CaptureStream::BybitLinearRest
    )
}

/// Apply the R7 rights gate (`SIGNALS_ENABLED_VENUES`) to a stream the live
/// runner is about to drive: a rights-gated venue (every Binance surface, every
/// Bybit surface) refuses unless its token is listed, exactly as the capture
/// path gates at start-up. So the live `run` command and the venue-lane shell
/// share one gate and one exit code (78); an ungated stream always admits. The
/// live runner calls this after [`ensure_live_stream_supported`], relying on it
/// rather than a blanket refusal to keep rights-gated venues off the wire.
pub fn ensure_live_venue_enabled(stream: CaptureStream) -> Result<(), CaptureError> {
    require_enabled_venue(stream)?;
    ensure_bybit_capture_enabled(stream)
}

/// [`ensure_live_venue_enabled`] against an already-read allow-list, so the
/// decision is testable without mutating the process environment.
#[cfg(test)]
fn ensure_live_venue_enabled_from(
    stream: CaptureStream,
    raw: Option<&str>,
) -> Result<(), CaptureError> {
    if let Some(token) = gated_venue_token(stream) {
        let enabled = EnabledVenues::parse(raw)
            .map_err(|error| CaptureError::InvalidOptions(error.to_string()))?;
        require_enabled_venue_with(stream, token, &enabled)?;
    }
    ensure_bybit_capture_enabled_from(stream, raw)
}

/// Apply the R7 rights gate to a Bybit capture stream, reading the allow-list
/// from the process environment.
fn ensure_bybit_capture_enabled(stream: CaptureStream) -> Result<(), CaptureError> {
    if !is_bybit_stream(stream) {
        // A non-Bybit stream never reads the variable, so a malformed allow-list
        // never takes an unrelated capture down (E2).
        return Ok(());
    }
    let raw = match std::env::var(VENUE_GATE_ENV) {
        Ok(raw) => Some(raw),
        Err(std::env::VarError::NotPresent) => None,
        Err(std::env::VarError::NotUnicode(_)) => Some("<non-utf8 value>".to_owned()),
    };
    ensure_bybit_capture_enabled_from(stream, raw.as_deref())
}

/// [`ensure_bybit_capture_enabled`] against an already-read variable, split out
/// so the decision is testable without mutating the process environment.
fn ensure_bybit_capture_enabled_from(
    stream: CaptureStream,
    raw: Option<&str>,
) -> Result<(), CaptureError> {
    if !is_bybit_stream(stream) {
        return Ok(());
    }
    let enabled = EnabledVenues::parse(raw)
        .map_err(|error| CaptureError::InvalidOptions(error.to_string()))?;
    match lane_start(&BYBIT_LANE, &enabled) {
        LaneStart::Start { .. } => Ok(()),
        LaneStart::Refuse { reason } => Err(CaptureError::VenueRefused(format!(
            "{reason}; the Bybit rights row is knowledge/RIGHTS-LEDGER.md:16"
        ))),
    }
}

/// Low-frequency HTTP JSON-RPC poll capture for the Chainlink on-chain feed.
/// This never opens a WebSocket: it appends one `Connected` record to open a
/// single logical epoch, then on a fixed cadence records each `eth_call` request
/// as `OutboundText` and its verbatim response body as `InboundText`. The raw
/// record schema is transport-agnostic, so replay of the captured request/
/// response pairs is unchanged.
async fn chainlink_poll_loop(
    options: &CaptureOptions,
    recorder: &mut RawRecorder,
    state: &mut CaptureState,
    capture_started: Instant,
) -> Result<(), CaptureError> {
    // Reuse the shared TLS-1.3-only HTTP client construction (mirrors the Kraken
    // auth client); the public node terminates TLS 1.3 and needs no API key.
    let client = secure_http::client().map_err(|_| CaptureError::ChainlinkClient)?;
    // Same failover list the live poll driver uses (B3, ruling E29): the capture
    // rotates to the next endpoint on any poll fault, so no single endpoint is
    // hardcoded on the poll path.
    let endpoints =
        crate::chainlink::configured_rpc_endpoints().map_err(|_| CaptureError::ChainlinkClient)?;
    let mut endpoint_index = 0_usize;
    let deadline = capture_started + options.duration;
    let mut shutdown = Box::pin(tokio::signal::ctrl_c());

    // One logical connection spans the whole poll session: the Connected record
    // opens epoch 1 and every request/response pair rides that epoch. A poll feed
    // has no handshake to fail, so a successful open is recorded up front.
    state.connection_attempts = state
        .connection_attempts
        .checked_add(1)
        .ok_or(CaptureError::CounterExhausted)?;
    state.connection_epoch = 1;
    state.successful_connections = state
        .successful_connections
        .checked_add(1)
        .ok_or(CaptureError::CounterExhausted)?;
    let connect_started_elapsed_ns = elapsed_ns(capture_started)?;
    append_connected_record(
        recorder,
        options.stream,
        state.connection_epoch,
        &mut state.ordinal,
        options.max_records,
        capture_started,
        connect_started_elapsed_ns,
    )
    .await?;

    // The first tick fires immediately; subsequent ticks fire every poll cadence.
    // A slow poll must not compress the following waits into a catch-up burst, so
    // missed ticks are skipped rather than fired back to back.
    let mut interval = tokio::time::interval(crate::chainlink::POLL_CADENCE);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // JSON-RPC request id; also the outbound poll ordinal within this session.
    let mut request_id: u64 = 0;
    // Consecutive failed polls, driving the shared reconnect backoff.
    let mut failure_streak: u64 = 0;

    loop {
        if state.ordinal >= options.max_records {
            break;
        }
        tokio::select! {
            result = &mut shutdown => {
                result.map_err(CaptureError::Signal)?;
                state.interrupted = true;
                break;
            }
            () = sleep_until(deadline) => {
                break;
            }
            _ = interval.tick() => {}
        }

        // Record the exact request as OutboundText BEFORE the POST, so a failed
        // poll still leaves an honest request trace with no fabricated response.
        request_id = request_id
            .checked_add(1)
            .ok_or(CaptureError::CounterExhausted)?;
        let body = crate::chainlink::request_body(request_id);
        let sent_at = crate::raw::now_timestamp();
        let sent_elapsed_ns = elapsed_ns(capture_started)?;
        append_text_record(
            recorder,
            &mut state.ordinal,
            options.max_records,
            RawRecord::text_at(
                options.stream,
                state.connection_epoch,
                1,
                RecordKind::OutboundText,
                body.clone(),
                sent_at,
                sent_elapsed_ns,
            ),
        )
        .await?;

        let poll = tokio::select! {
            result = &mut shutdown => {
                result.map_err(CaptureError::Signal)?;
                state.interrupted = true;
                break;
            }
            () = sleep_until(deadline) => {
                break;
            }
            poll = chainlink_poll(&client, &endpoints[endpoint_index], &body) => poll,
        };

        match poll {
            Some(response_body) => {
                failure_streak = 0;
                if state.ordinal < options.max_records {
                    let received_at = crate::raw::now_timestamp();
                    let capture_elapsed_ns = elapsed_ns(capture_started)?;
                    append_text_record(
                        recorder,
                        &mut state.ordinal,
                        options.max_records,
                        RawRecord::text_at(
                            options.stream,
                            state.connection_epoch,
                            1,
                            RecordKind::InboundText,
                            response_body,
                            received_at,
                            capture_elapsed_ns,
                        ),
                    )
                    .await?;
                }
            }
            None => {
                // `read_idle_timeouts` is the closest honest counter for a pull
                // feed: it counts poll cycles that produced no usable JSON-RPC
                // response (transport failure, timeout, non-success HTTP status,
                // oversize, or non-UTF-8 body). No inbound record is fabricated.
                state.read_idle_timeouts = state
                    .read_idle_timeouts
                    .checked_add(1)
                    .ok_or(CaptureError::CounterExhausted)?;
                failure_streak = failure_streak.saturating_add(1);
                // Fail over to the next endpoint on any fault (B3).
                endpoint_index = (endpoint_index + 1) % endpoints.len();
                let delay = reconnect_delay(failure_streak);
                tokio::select! {
                    result = &mut shutdown => {
                        result.map_err(CaptureError::Signal)?;
                        state.interrupted = true;
                        break;
                    }
                    () = sleep_until(deadline) => {
                        break;
                    }
                    () = sleep(delay) => {}
                }
            }
        }
    }
    Ok(())
}

/// One injected Binance USD-M REST poll: `endpoint -> outcome`. Production is
/// [`binance_usdm_rest_poll`] over the shared bounded/timed client; a test passes
/// a fixture transport so the live path is drivable without a network.
pub(crate) type BinanceUsdmRestTransport<'t> = Box<
    dyn FnMut(
            crate::binance_usdm_rest::BinanceUsdmRestEndpoint,
        ) -> Pin<Box<dyn Future<Output = BinanceUsdmPoll> + 't>>
        + 't,
>;

/// One resolved Binance USD-M REST poll: the optional response record (a served
/// body, one `Failed`, or nothing for a rate-limit pause), the counter, whether
/// the poll was served, and the pre-formatted stderr line.
pub(crate) struct BinanceRestResolved {
    /// The response record, or `None` when the outcome writes no response (a
    /// rate-limit sanction records a pause, not a response body).
    pub(crate) record: Option<RestResponseRecord>,
    /// The counter the outcome increments.
    pub(crate) counter: RestPollCounter,
    /// Whether the poll was served (a served poll = connected).
    pub(crate) served: bool,
    /// The single stderr line to emit, or `None` when no escalation step fired.
    pub(crate) log: Option<String>,
}

/// The Binance USD-M REST poll state machine, advanced one endpoint at a time
/// within one one-second base tick. Owns the four per-endpoint cadences (5 s /
/// 5 min / 1 h / 1 s), the shared 120-req/min budget, the IP-wide rate-limit ladder and
/// the per-endpoint refusal ladders. The driver owns the tick, the record sink
/// and the shutdown/deadline races; both the capture loop and the live runner
/// drive this same machine, so the two paths share one poll body (ruling E41).
pub(crate) struct BinanceUsdmRestSession<'t> {
    start: Instant,
    next_due_ms: [u64; 4],
    poll_counts: [u64; 4],
    budget: crate::binance_usdm_rest::RestBudget,
    rate_limit: crate::binance_usdm_rest::RateLimitBackoff,
    endpoint_backoff: [crate::binance_usdm_rest::RateLimitBackoff; 4],
    previous_endpoint_delay: [Duration; 4],
    paused_until_ms: u64,
    transport: BinanceUsdmRestTransport<'t>,
}

impl<'t> BinanceUsdmRestSession<'t> {
    /// The four USD-M REST identities, in the order they are polled per tick.
    pub(crate) const ENDPOINTS: [crate::binance_usdm_rest::BinanceUsdmRestEndpoint; 4] = [
        crate::binance_usdm_rest::BinanceUsdmRestEndpoint::OpenInterest,
        crate::binance_usdm_rest::BinanceUsdmRestEndpoint::OpenInterestHist,
        crate::binance_usdm_rest::BinanceUsdmRestEndpoint::FundingInfo,
        crate::binance_usdm_rest::BinanceUsdmRestEndpoint::PremiumIndex,
    ];

    /// Build a session over an injected transport, its schedule anchored at
    /// `start` (the capture/live clock origin for the ms cadences).
    pub(crate) fn new(start: Instant, transport: BinanceUsdmRestTransport<'t>) -> Self {
        Self {
            start,
            next_due_ms: [0; 4],
            poll_counts: [0; 4],
            budget: crate::binance_usdm_rest::RestBudget::new(),
            rate_limit: crate::binance_usdm_rest::RateLimitBackoff::new(),
            endpoint_backoff: std::array::from_fn(|_| {
                crate::binance_usdm_rest::RateLimitBackoff::new()
            }),
            previous_endpoint_delay: [Duration::ZERO; 4],
            paused_until_ms: 0,
            transport,
        }
    }

    /// Build a session over the shared bounded/timed reqwest client.
    pub(crate) fn reqwest(start: Instant) -> Result<BinanceUsdmRestSession<'static>, CaptureError> {
        let client = secure_http::client().map_err(|_| CaptureError::BinanceUsdmRestClient)?;
        let transport: BinanceUsdmRestTransport<'static> = Box::new(
            move |endpoint: crate::binance_usdm_rest::BinanceUsdmRestEndpoint| {
                let client = client.clone();
                Box::pin(async move { binance_usdm_rest_poll(&client, endpoint).await })
            },
        );
        Ok(BinanceUsdmRestSession::new(start, transport))
    }

    /// Milliseconds elapsed since the schedule origin, for the cadence and pause
    /// comparisons.
    pub(crate) fn elapsed_ms(&self) -> Result<u64, CaptureError> {
        u64::try_from(self.start.elapsed().as_millis()).map_err(|_| CaptureError::CounterExhausted)
    }

    /// Whether every poller is paused now (an IP-wide rate-limit sanction).
    pub(crate) fn is_paused(&self, elapsed_ms: u64) -> bool {
        elapsed_ms < self.paused_until_ms
    }

    /// Whether one endpoint is due to poll now.
    pub(crate) fn endpoint_due(&self, index: usize, elapsed_ms: u64) -> bool {
        elapsed_ms >= self.next_due_ms[index]
    }

    /// Claim one slot of the shared minute budget; `false` leaves the due time
    /// alone so the poll is retried next tick rather than skipped for a cadence.
    pub(crate) fn claim_budget(&mut self, elapsed_ms: u64) -> bool {
        self.budget.try_claim(elapsed_ms)
    }

    /// Bump the poll counter for one endpoint and return its request descriptor,
    /// the `OutboundText` payload written before the GET.
    pub(crate) fn begin_poll(&mut self, index: usize) -> Result<String, CaptureError> {
        self.poll_counts[index] = self.poll_counts[index]
            .checked_add(1)
            .ok_or(CaptureError::CounterExhausted)?;
        Ok(crate::binance_usdm_rest::request_descriptor(
            Self::ENDPOINTS[index],
            self.poll_counts[index],
        ))
    }

    /// Issue one GET against one endpoint through the injected transport.
    pub(crate) async fn fetch(&mut self, index: usize) -> BinanceUsdmPoll {
        (self.transport)(Self::ENDPOINTS[index]).await
    }

    /// Classify one endpoint's poll outcome, advance its schedule (or the IP-wide
    /// pause), and return the response record, counter, served flag and stderr
    /// line. Every arm reproduces the standalone capture loop exactly. `path`
    /// prefixes the stderr lines (`"capture"` / `"live"`) so a live lane's
    /// operator logs are never labelled `capture`.
    pub(crate) fn resolve_poll(
        &mut self,
        index: usize,
        elapsed_ms: u64,
        poll: BinanceUsdmPoll,
        path: &'static str,
    ) -> Result<BinanceRestResolved, CaptureError> {
        use crate::binance_usdm_rest::{BinanceUsdmRestEndpoint, used_weight_exhausted};
        let endpoint = Self::ENDPOINTS[index];
        let stream_token = CaptureStream::BinanceUsdmRest.as_str();
        // The base cadence is applied first; a refusal overrides it below.
        self.next_due_ms[index] = elapsed_ms.saturating_add(
            u64::try_from(endpoint.cadence().as_millis())
                .map_err(|_| CaptureError::CounterExhausted)?,
        );
        match poll {
            BinanceUsdmPoll::Body {
                body,
                used_weight_header,
            } => {
                // The venue served this poll, so the refusal ladder starts over.
                self.rate_limit.reset();
                self.endpoint_backoff[index].reset();
                self.previous_endpoint_delay[index] = Duration::ZERO;
                if used_weight_exhausted(used_weight_header.as_deref()) {
                    self.paused_until_ms = elapsed_ms.saturating_add(60_000);
                }
                Ok(BinanceRestResolved {
                    record: Some(RestResponseRecord::Inbound(body)),
                    counter: RestPollCounter::None,
                    served: true,
                    log: None,
                })
            }
            BinanceUsdmPoll::RateLimited { retry_after } => {
                // The sanction is IP-wide, so the pause is floored at the IP-wide
                // pause floor, never at a single endpoint's cadence: adding the
                // 1 s premium-index endpoint must not let an honoured
                // `Retry-After: 0` resume in 1 s against an endpoint that just
                // 429'd (ruling H1). The floor is `max(IP_PAUSE_FLOOR,
                // fastest_cadence)`; the escalation and 60 s ban-avoidance cap
                // live in `RateLimitBackoff::next_delay`.
                let fastest_cadence = Self::ENDPOINTS
                    .into_iter()
                    .map(BinanceUsdmRestEndpoint::cadence)
                    .min()
                    .unwrap_or(endpoint.cadence());
                let pause_floor = IP_PAUSE_FLOOR.max(fastest_cadence);
                let delay = self.rate_limit.next_delay(
                    retry_after.as_deref(),
                    refusal_clock(),
                    pause_floor,
                );
                let log = Some(format!(
                    "{path} {stream_token}: {} rate-limited (refusal {}); pausing every poller for {delay:?}",
                    endpoint.token(),
                    self.rate_limit.consecutive()
                ));
                self.paused_until_ms = elapsed_ms.saturating_add(
                    u64::try_from(delay.as_millis()).map_err(|_| CaptureError::CounterExhausted)?,
                );
                Ok(BinanceRestResolved {
                    record: None,
                    counter: RestPollCounter::NonSuccess,
                    served: false,
                    log,
                })
            }
            BinanceUsdmPoll::Refused { status } => {
                let delay = self.endpoint_backoff[index].next_delay(
                    None,
                    refusal_clock(),
                    endpoint.cadence(),
                );
                let log = (self.endpoint_backoff[index].consecutive() == 1
                    || delay != self.previous_endpoint_delay[index])
                    .then(|| {
                        format!(
                            "{path} {stream_token}: {} refused with HTTP {status} (refusal {}); pausing that poller for {delay:?}",
                            endpoint.token(),
                            self.endpoint_backoff[index].consecutive()
                        )
                    });
                self.previous_endpoint_delay[index] = delay;
                self.next_due_ms[index] = elapsed_ms.saturating_add(
                    u64::try_from(delay.as_millis()).map_err(|_| CaptureError::CounterExhausted)?,
                );
                Ok(BinanceRestResolved {
                    record: Some(RestResponseRecord::Failed {
                        kind: FailureKind::HttpStatus(status),
                        detail: endpoint.token().to_owned(),
                    }),
                    counter: RestPollCounter::NonSuccess,
                    served: false,
                    log,
                })
            }
            BinanceUsdmPoll::Failed { cause } => {
                let delay = self.endpoint_backoff[index].next_delay(
                    None,
                    refusal_clock(),
                    endpoint.cadence(),
                );
                let log = (self.endpoint_backoff[index].consecutive() == 1
                    || delay != self.previous_endpoint_delay[index])
                    .then(|| {
                        format!(
                            "{path} {stream_token}: {} unreachable ({cause}) (failure {}); pausing that poller for {delay:?}",
                            endpoint.token(),
                            self.endpoint_backoff[index].consecutive()
                        )
                    });
                self.previous_endpoint_delay[index] = delay;
                self.next_due_ms[index] = elapsed_ms.saturating_add(
                    u64::try_from(delay.as_millis()).map_err(|_| CaptureError::CounterExhausted)?,
                );
                Ok(BinanceRestResolved {
                    record: Some(RestResponseRecord::Failed {
                        kind: FailureKind::Transport,
                        detail: format!("{}: {cause}", endpoint.token()),
                    }),
                    // A refused connection, DNS blackhole, timeout or undecodable
                    // body is a transport fault, not a served non-success: it
                    // belongs on `read_idle_timeouts`, the read-idle class, so a
                    // blackholed host is distinguishable from N venue refusals and
                    // means the same thing as the Bybit REST sibling's mapping.
                    counter: RestPollCounter::ReadIdle,
                    served: false,
                    log,
                })
            }
        }
    }
}

/// How an SSE read loop ended, so the caller knows whether to redial, stop for
/// the deadline, or stop for an operator interrupt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SseOutcome {
    /// The connection closed (24 h server close, transport end, or an oversize
    /// event forced a redial). Redial at the backoff floor.
    Closed,
    /// The operator interrupted the run.
    Shutdown,
    /// The capture deadline was reached.
    Deadline,
    /// The record cap was reached.
    MaxRecords,
}

/// Record one refused or failed Pyth Hermes request. The Failed record rides the
/// one logical capture epoch, exactly as the poll feeds do, and its payload is
/// our own bounded description with a `failure` discriminant — never a venue
/// body, never the credential.
async fn append_pyth_hermes_failure(
    recorder: &mut RawRecorder,
    options: &CaptureOptions,
    state: &mut CaptureState,
    capture_started: Instant,
    epoch: u64,
    kind: FailureKind,
    detail: &str,
) -> Result<(), CaptureError> {
    state.non_success_responses = state
        .non_success_responses
        .checked_add(1)
        .ok_or(CaptureError::CounterExhausted)?;
    if state.ordinal < options.max_records {
        let received_at = crate::raw::now_timestamp();
        let capture_elapsed_ns = elapsed_ns(capture_started)?;
        append_text_record(
            recorder,
            &mut state.ordinal,
            options.max_records,
            RawRecord::failed_at(
                options.stream,
                epoch,
                1,
                kind,
                detail,
                received_at,
                capture_elapsed_ns,
            ),
        )
        .await?;
    }
    Ok(())
}

/// Find the index at which a blank-line SSE event boundary (`\n\n`) begins.
fn find_sse_boundary(buffer: &[u8]) -> Option<usize> {
    buffer.windows(2).position(|pair| pair == b"\n\n")
}

/// Keyed HTTPS Server-Sent-Events capture for the Pyth Hermes BTC/USD price
/// stream. Refuse-closed behind `PYTH_API_KEY` (loaded by `run_capture`).
///
/// Like the poll loops it models the whole session as one logical epoch: the
/// `Connected` record opens epoch 1, the outbound request descriptor (whose URL
/// pins `ids[]=FEED_ID`) is recorded once, and every SSE event block that
/// follows rides that epoch. A connect refusal or transport fault is a `Failed`
/// record; a 401/403 (unauthorized) or 404 (bad id/path) is terminal — no retry,
/// non-zero exit (§3.2 M6, §3.6). The credential is a request header, never the
/// URL, and every error string this can surface passes through the redactor.
async fn pyth_hermes_sse_loop(
    options: &CaptureOptions,
    credential: &PythHermesCredential,
    recorder: &mut RawRecorder,
    state: &mut CaptureState,
    capture_started: Instant,
) -> Result<(), CaptureError> {
    let client = secure_http::client().map_err(|_| CaptureError::PythHermesClient)?;
    let url = crate::pyth_hermes::stream_url();
    let deadline = capture_started + options.duration;
    let mut shutdown = Box::pin(tokio::signal::ctrl_c());

    // One logical connection spans the whole session (poll-feed pattern): open
    // epoch 1 up front so a connect-time refusal still has an epoch to be
    // recorded in, and record the pinned outbound descriptor once.
    state.connection_attempts = state
        .connection_attempts
        .checked_add(1)
        .ok_or(CaptureError::CounterExhausted)?;
    state.connection_epoch = 1;
    state.successful_connections = state
        .successful_connections
        .checked_add(1)
        .ok_or(CaptureError::CounterExhausted)?;
    let connect_started_elapsed_ns = elapsed_ns(capture_started)?;
    append_connected_record(
        recorder,
        options.stream,
        state.connection_epoch,
        &mut state.ordinal,
        options.max_records,
        capture_started,
        connect_started_elapsed_ns,
    )
    .await?;
    if state.ordinal < options.max_records {
        let sent_at = crate::raw::now_timestamp();
        let sent_elapsed_ns = elapsed_ns(capture_started)?;
        append_text_record(
            recorder,
            &mut state.ordinal,
            options.max_records,
            RawRecord::text_at(
                options.stream,
                state.connection_epoch,
                1,
                RecordKind::OutboundText,
                crate::pyth_hermes::request_descriptor(&url),
                sent_at,
                sent_elapsed_ns,
            ),
        )
        .await?;
    }

    let mut failure_streak: u64 = 0;
    loop {
        if state.ordinal >= options.max_records || Instant::now() >= deadline {
            break;
        }
        let request = credential.authorize(
            client
                .get(&url)
                .header(
                    reqwest::header::ACCEPT,
                    reqwest::header::HeaderValue::from_static("text/event-stream"),
                )
                .timeout(crate::pyth_hermes::REQUEST_TIMEOUT),
        )?;
        let send = tokio::select! {
            result = &mut shutdown => {
                result.map_err(CaptureError::Signal)?;
                state.interrupted = true;
                break;
            }
            () = sleep_until(deadline) => break,
            send = request.send() => send,
        };
        let response = match send {
            Ok(response) => response,
            Err(error) => {
                append_pyth_hermes_failure(
                    recorder,
                    options,
                    state,
                    capture_started,
                    state.connection_epoch,
                    FailureKind::Transport,
                    &credential.redact(&crate::adapter::bounded_token(&error.to_string())),
                )
                .await?;
                failure_streak = failure_streak.saturating_add(1);
                if sse_backoff(&mut shutdown, deadline, failure_streak, state).await? {
                    break;
                }
                continue;
            }
        };
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            append_pyth_hermes_failure(
                recorder,
                options,
                state,
                capture_started,
                state.connection_epoch,
                FailureKind::HttpStatus(status),
                "Pyth Hermes SSE connect refused",
            )
            .await?;
            if matches!(status, 401 | 403 | 404) {
                return Err(CaptureError::PythHermesTerminal(format!(
                    "Pyth Hermes SSE returned HTTP {status}; terminal, no retry (re-key or fix the id/path)"
                )));
            }
            failure_streak = failure_streak.saturating_add(1);
            if sse_backoff(&mut shutdown, deadline, failure_streak, state).await? {
                break;
            }
            continue;
        }
        failure_streak = 0;
        let outcome = read_sse_events(
            options,
            credential,
            recorder,
            state,
            capture_started,
            response,
        )
        .await?;
        match outcome {
            SseOutcome::Shutdown => {
                state.interrupted = true;
                break;
            }
            SseOutcome::Deadline | SseOutcome::MaxRecords => break,
            SseOutcome::Closed => {
                // 24 h close or transport end: redial at the backoff floor.
                failure_streak = failure_streak.saturating_add(1);
                if sse_backoff(&mut shutdown, deadline, failure_streak, state).await? {
                    break;
                }
            }
        }
    }
    Ok(())
}

/// Sleep the reconnect backoff, or return `true` if the deadline or an interrupt
/// intervened.
async fn sse_backoff(
    shutdown: &mut std::pin::Pin<Box<impl std::future::Future<Output = std::io::Result<()>>>>,
    deadline: Instant,
    failure_streak: u64,
    state: &mut CaptureState,
) -> Result<bool, CaptureError> {
    let delay = reconnect_delay(failure_streak);
    tokio::select! {
        result = shutdown => {
            result.map_err(CaptureError::Signal)?;
            state.interrupted = true;
            Ok(true)
        }
        () = sleep_until(deadline) => Ok(true),
        () = sleep(delay) => Ok(false),
    }
}

/// Read the SSE byte stream for one connection, framing events on the blank-line
/// boundary and recording each complete event block as one `InboundText`. The
/// per-event and per-line bounds are enforced so an unbounded body cannot grow a
/// buffer (§3.1, test T-N15).
async fn read_sse_events(
    options: &CaptureOptions,
    credential: &PythHermesCredential,
    recorder: &mut RawRecorder,
    state: &mut CaptureState,
    capture_started: Instant,
    mut response: reqwest::Response,
) -> Result<SseOutcome, CaptureError> {
    let deadline = capture_started + options.duration;
    let mut shutdown = Box::pin(tokio::signal::ctrl_c());
    let mut buffer: Vec<u8> = Vec::new();
    loop {
        if state.ordinal >= options.max_records {
            return Ok(SseOutcome::MaxRecords);
        }
        let chunk = tokio::select! {
            result = &mut shutdown => {
                result.map_err(CaptureError::Signal)?;
                return Ok(SseOutcome::Shutdown);
            }
            () = sleep_until(deadline) => return Ok(SseOutcome::Deadline),
            chunk = response.chunk() => chunk,
        };
        let chunk = match chunk {
            Ok(Some(bytes)) => bytes,
            // Stream ended (24 h close) or a transport read error: redial.
            Ok(None) | Err(_) => return Ok(SseOutcome::Closed),
        };
        if buffer.len().saturating_add(chunk.len())
            > crate::pyth_hermes::MAX_SSE_EVENT_BYTES.saturating_mul(2)
        {
            // An event that never terminates within the bound: end the
            // connection rather than grow the buffer.
            return Ok(SseOutcome::Closed);
        }
        buffer.extend_from_slice(&chunk);
        while let Some(boundary) = find_sse_boundary(&buffer) {
            let block: Vec<u8> = buffer.drain(..boundary).collect();
            // Drop the blank-line separator (all leading newlines).
            while buffer.first() == Some(&b'\n') {
                buffer.remove(0);
            }
            if block.is_empty() {
                continue;
            }
            if block.len() > crate::pyth_hermes::MAX_SSE_EVENT_BYTES {
                return Ok(SseOutcome::Closed);
            }
            // Defence in depth: a provider frame that echoed the header would be
            // scrubbed before it reached the mode-0600 journal (test T-N9).
            let (redacted, _) = credential.redact_body(&block, false);
            let Ok(text) = String::from_utf8(redacted) else {
                // A non-UTF-8 SSE body is an unmodelled provider change: end the
                // connection rather than record bytes the adapter cannot frame.
                return Ok(SseOutcome::Closed);
            };
            if state.ordinal >= options.max_records {
                return Ok(SseOutcome::MaxRecords);
            }
            let received_at = crate::raw::now_timestamp();
            let capture_elapsed_ns = elapsed_ns(capture_started)?;
            append_text_record(
                recorder,
                &mut state.ordinal,
                options.max_records,
                RawRecord::text_at(
                    options.stream,
                    state.connection_epoch,
                    1,
                    RecordKind::InboundText,
                    text,
                    received_at,
                    capture_elapsed_ns,
                ),
            )
            .await?;
        }
    }
}

/// One decoded REST latest-anchor poll outcome.
enum PythLatestPoll {
    /// A usable 2xx response body.
    Body(String),
    /// A non-2xx status.
    Refused(u16),
    /// A transport fault or a non-UTF-8 body, with a bounded, redacted cause.
    Transport(String),
}

/// Keyed HTTPS REST latest-anchor capture for the Pyth Hermes BTC/USD price.
///
/// The REST `latest` endpoint is a start-up and post-reconnect anchor, at most
/// one request per [`crate::pyth_hermes::MIN_REST_INTERVAL`]; its rows carry
/// `update_kind: "latest"` so rolling windows exclude them. Like the SSE loop it
/// opens one logical epoch, records the pinned outbound descriptor, then records
/// each request/response pair. A 401/403/404 is terminal (§3.6).
async fn pyth_hermes_rest_poll_loop(
    options: &CaptureOptions,
    credential: &PythHermesCredential,
    recorder: &mut RawRecorder,
    state: &mut CaptureState,
    capture_started: Instant,
) -> Result<(), CaptureError> {
    let client = secure_http::client().map_err(|_| CaptureError::PythHermesClient)?;
    let url = crate::pyth_hermes::latest_url();
    let deadline = capture_started + options.duration;
    let mut shutdown = Box::pin(tokio::signal::ctrl_c());

    state.connection_attempts = state
        .connection_attempts
        .checked_add(1)
        .ok_or(CaptureError::CounterExhausted)?;
    state.connection_epoch = 1;
    state.successful_connections = state
        .successful_connections
        .checked_add(1)
        .ok_or(CaptureError::CounterExhausted)?;
    let connect_started_elapsed_ns = elapsed_ns(capture_started)?;
    append_connected_record(
        recorder,
        options.stream,
        state.connection_epoch,
        &mut state.ordinal,
        options.max_records,
        capture_started,
        connect_started_elapsed_ns,
    )
    .await?;

    let mut interval = tokio::time::interval(crate::pyth_hermes::MIN_REST_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut failure_streak: u64 = 0;
    loop {
        if state.ordinal >= options.max_records {
            break;
        }
        tokio::select! {
            result = &mut shutdown => {
                result.map_err(CaptureError::Signal)?;
                state.interrupted = true;
                break;
            }
            () = sleep_until(deadline) => break,
            _ = interval.tick() => {}
        }
        // Record the pinned outbound descriptor before the GET.
        if state.ordinal < options.max_records {
            let sent_at = crate::raw::now_timestamp();
            let sent_elapsed_ns = elapsed_ns(capture_started)?;
            append_text_record(
                recorder,
                &mut state.ordinal,
                options.max_records,
                RawRecord::text_at(
                    options.stream,
                    state.connection_epoch,
                    1,
                    RecordKind::OutboundText,
                    crate::pyth_hermes::request_descriptor(&url),
                    sent_at,
                    sent_elapsed_ns,
                ),
            )
            .await?;
        }
        let poll = tokio::select! {
            result = &mut shutdown => {
                result.map_err(CaptureError::Signal)?;
                state.interrupted = true;
                break;
            }
            () = sleep_until(deadline) => break,
            poll = pyth_hermes_latest_poll(&client, &url, credential) => poll,
        };
        match poll {
            PythLatestPoll::Body(body) => {
                failure_streak = 0;
                if state.ordinal < options.max_records {
                    let (redacted, _) = credential.redact_body(body.as_bytes(), false);
                    let text = String::from_utf8(redacted)
                        .unwrap_or_else(|_| String::from("<non-utf8 body redacted>"));
                    let received_at = crate::raw::now_timestamp();
                    let capture_elapsed_ns = elapsed_ns(capture_started)?;
                    append_text_record(
                        recorder,
                        &mut state.ordinal,
                        options.max_records,
                        RawRecord::text_at(
                            options.stream,
                            state.connection_epoch,
                            1,
                            RecordKind::InboundText,
                            text,
                            received_at,
                            capture_elapsed_ns,
                        ),
                    )
                    .await?;
                }
            }
            PythLatestPoll::Refused(status) => {
                append_pyth_hermes_failure(
                    recorder,
                    options,
                    state,
                    capture_started,
                    state.connection_epoch,
                    FailureKind::HttpStatus(status),
                    "Pyth Hermes REST latest refused",
                )
                .await?;
                if matches!(status, 401 | 403 | 404) {
                    return Err(CaptureError::PythHermesTerminal(format!(
                        "Pyth Hermes REST latest returned HTTP {status}; terminal, no retry"
                    )));
                }
                failure_streak = failure_streak.saturating_add(1);
                if sse_backoff(&mut shutdown, deadline, failure_streak, state).await? {
                    break;
                }
            }
            PythLatestPoll::Transport(cause) => {
                append_pyth_hermes_failure(
                    recorder,
                    options,
                    state,
                    capture_started,
                    state.connection_epoch,
                    FailureKind::Transport,
                    &cause,
                )
                .await?;
                failure_streak = failure_streak.saturating_add(1);
                if sse_backoff(&mut shutdown, deadline, failure_streak, state).await? {
                    break;
                }
            }
        }
    }
    Ok(())
}

/// Execute one REST `latest` GET. The credential is attached as a header; a
/// transport cause is bounded and redacted before it can leave this function.
async fn pyth_hermes_latest_poll(
    client: &reqwest::Client,
    url: &str,
    credential: &PythHermesCredential,
) -> PythLatestPoll {
    let request = match credential.authorize(
        client
            .get(url)
            .header(
                reqwest::header::ACCEPT,
                reqwest::header::HeaderValue::from_static("application/json"),
            )
            .timeout(crate::pyth_hermes::REQUEST_TIMEOUT),
    ) {
        Ok(request) => request,
        Err(_) => return PythLatestPoll::Transport("credential is not a usable header".to_owned()),
    };
    let response = match request.send().await {
        Ok(response) => response,
        Err(error) => {
            return PythLatestPoll::Transport(
                credential.redact(&crate::adapter::bounded_token(&error.to_string())),
            );
        }
    };
    let (status, bytes) = match secure_http::bounded_body_with_status(
        response,
        crate::pyth_hermes::MAX_SSE_EVENT_BYTES,
    )
    .await
    {
        Ok(body) => body,
        Err(error) => {
            return PythLatestPoll::Transport(
                credential.redact(&crate::adapter::bounded_token(&error.to_string())),
            );
        }
    };
    if !(200..300).contains(&status) {
        return PythLatestPoll::Refused(status);
    }
    match String::from_utf8(bytes) {
        Ok(body) => PythLatestPoll::Body(body),
        Err(_) => PythLatestPoll::Transport("response body was not UTF-8".to_owned()),
    }
}

/// Bounded HTTP GET poll capture for the four Binance USD-M REST endpoints.
///
/// One logical connection spans the whole poll session, as in the Chainlink
/// loop: a poll feed has no handshake to fail, so a successful open is recorded
/// up front and every request/response pair rides that epoch. Each endpoint has
/// its own cadence (five seconds, five minutes, one hour, one second), and for EACH poll the
/// loop records the compiled request descriptor as `OutboundText` -- these are
/// GETs with no body, so the descriptor IS the outbound record, and it is what
/// tells a replay which parser owns the response -- then the verbatim response
/// body as `InboundText`.
///
/// Three bounds keep an unattended run polite: a self-imposed one hundred twenty
/// requests per minute across all four endpoints, a pause when the venue's used-weight
/// header crosses its threshold, and an exact `Retry-After` pause on 429/418. A
/// non-2xx response records nothing at all -- a body we never received is never
/// fabricated -- and is counted honestly instead.
///
/// The per-poll body — schedule, budget, classification, backoff, the stderr
/// lines — is the shared [`BinanceUsdmRestSession`], the same machine the live
/// runner drives; only the record sink (this recorder) differs (ruling E41).
async fn binance_usdm_rest_poll_loop(
    options: &CaptureOptions,
    recorder: &mut RawRecorder,
    state: &mut CaptureState,
    capture_started: Instant,
) -> Result<(), CaptureError> {
    let deadline = capture_started + options.duration;
    let mut shutdown = Box::pin(tokio::signal::ctrl_c());

    state.connection_attempts = state
        .connection_attempts
        .checked_add(1)
        .ok_or(CaptureError::CounterExhausted)?;
    state.connection_epoch = 1;
    state.successful_connections = state
        .successful_connections
        .checked_add(1)
        .ok_or(CaptureError::CounterExhausted)?;
    let connect_started_elapsed_ns = elapsed_ns(capture_started)?;
    append_connected_record(
        recorder,
        options.stream,
        state.connection_epoch,
        &mut state.ordinal,
        options.max_records,
        capture_started,
        connect_started_elapsed_ns,
    )
    .await?;

    // A one-second base tick drives every cadence; a slow poll must not compress
    // the following waits into a catch-up burst, so missed ticks are skipped.
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // The shared per-endpoint session (cadences, budget, ladders) over the
    // production reqwest transport.
    let mut session = BinanceUsdmRestSession::reqwest(capture_started)?;

    loop {
        if state.ordinal >= options.max_records {
            break;
        }
        tokio::select! {
            result = &mut shutdown => {
                result.map_err(CaptureError::Signal)?;
                state.interrupted = true;
                break;
            }
            () = sleep_until(deadline) => {
                break;
            }
            _ = interval.tick() => {}
        }
        let elapsed_ms = session.elapsed_ms()?;
        if session.is_paused(elapsed_ms) {
            continue;
        }

        for index in 0..BinanceUsdmRestSession::ENDPOINTS.len() {
            // A pause set by an earlier endpoint in this same tick applies to
            // the rest of the tick too.
            if session.is_paused(elapsed_ms) {
                break;
            }
            if state.ordinal >= options.max_records || !session.endpoint_due(index, elapsed_ms) {
                continue;
            }
            // The minute's allowance is shared. A refused claim leaves the due
            // time alone so the poll is retried on the next tick rather than
            // silently skipped for a whole cadence.
            if !session.claim_budget(elapsed_ms) {
                continue;
            }
            let descriptor = session.begin_poll(index)?;
            let sent_at = crate::raw::now_timestamp();
            let sent_elapsed_ns = elapsed_ns(capture_started)?;
            append_text_record(
                recorder,
                &mut state.ordinal,
                options.max_records,
                RawRecord::text_at(
                    options.stream,
                    state.connection_epoch,
                    1,
                    RecordKind::OutboundText,
                    descriptor,
                    sent_at,
                    sent_elapsed_ns,
                ),
            )
            .await?;

            let poll = tokio::select! {
                result = &mut shutdown => {
                    result.map_err(CaptureError::Signal)?;
                    state.interrupted = true;
                    return Ok(());
                }
                () = sleep_until(deadline) => {
                    return Ok(());
                }
                poll = session.fetch(index) => poll,
            };

            let resolved = session.resolve_poll(index, elapsed_ms, poll, "capture")?;
            match resolved.counter {
                RestPollCounter::None => {}
                RestPollCounter::ServedError => {
                    state.served_error_responses = state
                        .served_error_responses
                        .checked_add(1)
                        .ok_or(CaptureError::CounterExhausted)?;
                }
                RestPollCounter::NonSuccess => {
                    state.non_success_responses = state
                        .non_success_responses
                        .checked_add(1)
                        .ok_or(CaptureError::CounterExhausted)?;
                }
                RestPollCounter::ReadIdle => {
                    state.read_idle_timeouts = state
                        .read_idle_timeouts
                        .checked_add(1)
                        .ok_or(CaptureError::CounterExhausted)?;
                }
            }
            if let Some(record) = resolved.record
                && state.ordinal < options.max_records
            {
                let received_at = crate::raw::now_timestamp();
                let capture_elapsed_ns = elapsed_ns(capture_started)?;
                let record = match record {
                    RestResponseRecord::Inbound(body) => RawRecord::text_at(
                        options.stream,
                        state.connection_epoch,
                        1,
                        RecordKind::InboundText,
                        body,
                        received_at,
                        capture_elapsed_ns,
                    ),
                    RestResponseRecord::Failed { kind, detail } => RawRecord::failed_at(
                        options.stream,
                        state.connection_epoch,
                        1,
                        kind,
                        &detail,
                        received_at,
                        capture_elapsed_ns,
                    ),
                };
                append_text_record(recorder, &mut state.ordinal, options.max_records, record)
                    .await?;
            }
            if let Some(line) = resolved.log {
                eprintln!("{line}");
            }
        }
    }
    Ok(())
}

/// Wall clock for resolving an HTTP-date `Retry-After`.
///
/// `chrono`'s own clock feature is off workspace-wide, so the time comes from
/// `SystemTime`, as it does everywhere else in this crate.
fn refusal_clock() -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::<chrono::Utc>::from(SystemTime::now())
}

/// Outcome of one Binance USD-M REST poll.
pub(crate) enum BinanceUsdmPoll {
    /// A usable 2xx UTF-8 body, with the venue's used-weight header if present.
    Body {
        body: String,
        used_weight_header: Option<String>,
    },
    /// The venue rate-limited the poller (429) or is banning it (418).
    RateLimited { retry_after: Option<String> },
    /// The venue refused the request with a non-success status that is not a
    /// rate limit — 451, 403, a 5xx. The status is carried so the capture can
    /// record WHAT was refused: a body is never fabricated, but a refusal that
    /// leaves no trace is indistinguishable from a venue with nothing to say,
    /// which is exactly how a 65-minute run once produced an empty file in
    /// silence (ruling E27).
    Refused { status: u16 },
    /// A request that never produced a status: a refused connection, a DNS
    /// blackhole, a timeout, a body that would not decode. `cause` is our own
    /// bounded description — there is no venue response to quote.
    Failed { cause: String },
}

/// Execute one bounded GET against a USD-M REST endpoint.
async fn binance_usdm_rest_poll(
    client: &reqwest::Client,
    endpoint: crate::binance_usdm_rest::BinanceUsdmRestEndpoint,
) -> BinanceUsdmPoll {
    let url = format!("{}{}", crate::binance_usdm_rest::REST_HOST, endpoint.path());
    let response = match client
        .get(url)
        .header(
            reqwest::header::ACCEPT,
            reqwest::header::HeaderValue::from_static("application/json"),
        )
        .timeout(crate::binance_usdm_rest::REQUEST_TIMEOUT)
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => {
            return BinanceUsdmPoll::Failed {
                cause: crate::adapter::bounded_token(&error.to_string()),
            };
        }
    };
    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    };
    let status = response.status().as_u16();
    let retry_after = header("retry-after");
    let used_weight_header = header(crate::binance_usdm_rest::USED_WEIGHT_HEADER);
    if status == 429 || status == 418 {
        return BinanceUsdmPoll::RateLimited { retry_after };
    }
    let (status, bytes) = match secure_http::bounded_body_with_status(
        response,
        crate::binance_usdm_rest::MAX_RESPONSE_BYTES,
    )
    .await
    {
        Ok(body) => body,
        Err(error) => {
            return BinanceUsdmPoll::Failed {
                cause: crate::adapter::bounded_token(&error.to_string()),
            };
        }
    };
    if !(200..300).contains(&status) {
        return BinanceUsdmPoll::Refused { status };
    }
    match String::from_utf8(bytes) {
        Ok(body) => BinanceUsdmPoll::Body {
            body,
            used_weight_header,
        },
        Err(_) => BinanceUsdmPoll::Failed {
            cause: "response body was not UTF-8".to_owned(),
        },
    }
}

/// Execute one `eth_call` poll. Returns the verbatim response body on a usable
/// 2xx UTF-8 response, or `None` for any transport, timeout, status, size, or
/// encoding failure (the caller then backs off without recording anything).
pub(crate) async fn chainlink_poll(
    client: &reqwest::Client,
    endpoint: &str,
    body: &str,
) -> Option<String> {
    let response = client
        .post(endpoint)
        .header(
            reqwest::header::CONTENT_TYPE,
            reqwest::header::HeaderValue::from_static("application/json"),
        )
        .header(
            reqwest::header::ACCEPT,
            reqwest::header::HeaderValue::from_static("application/json"),
        )
        .timeout(crate::chainlink::REQUEST_TIMEOUT)
        .body(body.to_owned())
        .send()
        .await
        .ok()?;
    // Bound the reply at MAX_RESPONSE_BYTES BEFORE reading (content-length
    // pre-check plus a streaming cap), the same bound the adapter's oversize
    // guard applies, so live and replay agree (B3).
    let (status, bytes) =
        secure_http::bounded_body_with_status(response, crate::chainlink::MAX_RESPONSE_BYTES)
            .await
            .ok()?;
    if !(200..300).contains(&status) {
        return None;
    }
    String::from_utf8(bytes).ok()
}

/// Where a REST poll session sends the records it produces and the counters it
/// bumps. Capture appends records to a [`RawRecorder`] and bumps [`CaptureState`];
/// the live runner feeds them through the replay adapter into outcomes and bumps
/// its own counters. One session body drives either sink (ruling E41): the
/// capture path and the live path share the same per-endpoint poll loop, so a
/// transport mismatch (an `http_poll` stream dialled as a websocket) cannot
/// arise from the two paths drifting apart.
/// Which capture counter a classified REST poll outcome increments. Shared by
/// both venues' sessions so the driver, not the session, owns counter storage:
/// the same body serves the capture recorder and the live adapter (ruling E41).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RestPollCounter {
    /// A served success: no failure counter moves.
    None,
    /// A 2xx body the venue served but that carried an error (`retCode != 0` /
    /// empty list): a served-error response.
    ServedError,
    /// A non-2xx HTTP status.
    NonSuccess,
    /// A transport, timeout, or decode failure (the read-idle class).
    ReadIdle,
}

impl From<BybitRestCounter> for RestPollCounter {
    fn from(counter: BybitRestCounter) -> Self {
        match counter {
            BybitRestCounter::None => Self::None,
            BybitRestCounter::ServedError => Self::ServedError,
            BybitRestCounter::NonSuccess => Self::NonSuccess,
            BybitRestCounter::ReadIdle => Self::ReadIdle,
        }
    }
}

/// One injected Bybit REST GET: `url -> outcome`. Production is [`bybit_rest_get`]
/// over the shared bounded/timed client; a test passes a fixture transport so the
/// live path is drivable without a network (mirroring the Chainlink poll seam).
pub(crate) type BybitRestTransport<'t> =
    Box<dyn FnMut(&'static str) -> Pin<Box<dyn Future<Output = RestPollOutcome> + 't>> + 't>;

/// The response record one REST poll writes after its `OutboundText` descriptor:
/// a served body verbatim as `InboundText`, or one schema-4 `Failed` record
/// carrying the failure kind and our own bounded cause (ruling E32). Shared by
/// both venues' sessions so the capture recorder and the live adapter admit the
/// same record shapes.
pub(crate) enum RestResponseRecord {
    /// A served success: the verbatim body as `InboundText`.
    Inbound(String),
    /// A failed poll as one `Failed` record.
    Failed { kind: FailureKind, detail: String },
}

/// One resolved Bybit REST poll: the response record to write, the counter to
/// bump, whether the poll was served (the feed is live), and the pre-formatted
/// stderr line (identical for capture and live, ruling E27(c)).
pub(crate) struct BybitRestResolved {
    /// The response record: a served body as `InboundText`, or one `Failed`.
    pub(crate) record: RestResponseRecord,
    /// The counter the outcome increments.
    pub(crate) counter: RestPollCounter,
    /// Whether the poll was served (a served poll = connected).
    pub(crate) served: bool,
    /// The single stderr line to emit per fault, or `None` on a served success.
    pub(crate) log: Option<String>,
}

/// The Bybit linear REST poll state machine, advanced one poll at a time. Owns
/// the two per-endpoint schedules (open interest 300 s, funding 3600 s), the
/// 1 req/s local spacing bucket, and the injected transport. The driver owns the
/// loop, the record sink and the shutdown/deadline races; both the capture loop
/// and the live runner drive this same machine, so the two paths share one poll
/// body and an `http_poll` stream is never dialled as a websocket (ruling E41).
pub(crate) struct BybitRestSession<'t> {
    endpoints: [EndpointRest; 2],
    last_request_at: Option<Instant>,
    transport: BybitRestTransport<'t>,
}

impl<'t> BybitRestSession<'t> {
    /// Build a session with both endpoints due immediately, over an injected
    /// transport.
    pub(crate) fn new(now: Instant, transport: BybitRestTransport<'t>) -> Self {
        Self {
            endpoints: [
                EndpointRest::new(
                    BYBIT_OPEN_INTEREST_URL,
                    BYBIT_OPEN_INTEREST_POLL_CADENCE,
                    now,
                ),
                EndpointRest::new(BYBIT_FUNDING_HISTORY_URL, BYBIT_FUNDING_POLL_CADENCE, now),
            ],
            last_request_at: None,
            transport,
        }
    }

    /// Build a session over the shared bounded/timed reqwest client the capture
    /// loop uses.
    pub(crate) fn reqwest(now: Instant) -> Result<BybitRestSession<'static>, CaptureError> {
        let client = secure_http::client().map_err(|_| CaptureError::ChainlinkClient)?;
        let transport: BybitRestTransport<'static> = Box::new(move |url: &'static str| {
            let client = client.clone();
            Box::pin(async move { bybit_rest_get(&client, url).await })
        });
        Ok(BybitRestSession::new(now, transport))
    }

    /// The endpoint due soonest and the instant to wait until before polling it.
    pub(crate) fn next_poll(&self) -> (usize, Instant) {
        let due = if self.endpoints[0].next_poll_at <= self.endpoints[1].next_poll_at {
            0
        } else {
            1
        };
        (due, self.endpoints[due].next_poll_at)
    }

    /// The remaining 1 req/s spacing wait before the next request, if any. Two
    /// endpoints due together at startup are still spaced by this.
    pub(crate) fn spacing_wait(&self) -> Option<Duration> {
        let previous = self.last_request_at?;
        let since = previous.elapsed();
        (since < BYBIT_REST_MIN_REQUEST_SPACING).then(|| BYBIT_REST_MIN_REQUEST_SPACING - since)
    }

    /// Mark a request as issued now, opening the 1 req/s spacing window.
    pub(crate) fn mark_requested(&mut self) {
        self.last_request_at = Some(Instant::now());
    }

    /// The exact request URL recorded as the poll's `OutboundText` descriptor.
    pub(crate) fn request_url(&self, due: usize) -> &'static str {
        self.endpoints[due].url
    }

    /// Issue one GET against the due endpoint through the injected transport.
    pub(crate) async fn fetch(&mut self, due: usize) -> RestPollOutcome {
        let url = self.endpoints[due].url;
        (self.transport)(url).await
    }

    /// Classify one outcome, advance the due endpoint's schedule, and return the
    /// response record, the counter, the served flag and the stderr line. The
    /// classification is pure (`classify_bybit_rest_poll`) and the ladder math is
    /// [`EndpointRest::schedule_next`], both shared with capture unchanged. `path`
    /// prefixes the stderr line (`"capture"` on the capture path, `"live"` on the
    /// live runner) so a live lane's operator logs are never labelled `capture`.
    pub(crate) fn resolve(
        &mut self,
        due: usize,
        outcome: RestPollOutcome,
        path: &'static str,
    ) -> BybitRestResolved {
        let action = classify_bybit_rest_poll(outcome);
        let served = action.class == PollClass::Success;
        let delay = self.endpoints[due].schedule_next(action.class, Instant::now());
        let log = action.log_cause.map(|cause| {
            format!(
                "{path} bybit_linear_rest: {cause} on {}; next poll in {}s (streak {})",
                self.endpoints[due].url,
                delay.as_secs(),
                self.endpoints[due].failure_streak
            )
        });
        let record = match action.record {
            BybitRestActionRecord::Body(body) => RestResponseRecord::Inbound(body),
            BybitRestActionRecord::Failed { kind, detail } => {
                RestResponseRecord::Failed { kind, detail }
            }
        };
        BybitRestResolved {
            record,
            counter: action.counter.into(),
            served,
            log,
        }
    }
}

/// HTTP GET poll capture for the two Bybit linear REST identities. Like the
/// Chainlink loop it opens one logical epoch, then records each request as
/// `OutboundText` (the exact URL) and its verbatim response body as
/// `InboundText`; the normalizer classifies open interest vs funding by the
/// response shape. The two endpoints run on their own cadences (open interest
/// every 300 s, funding every 3600 s) but share a 1 req/s local spacing bound,
/// so at most one request is in flight and the venue budget is never exceeded.
///
/// The per-poll body — schedule, spacing, classification, backoff, the stderr
/// line — is the shared [`BybitRestSession`], the same machine the live runner
/// drives; only the record sink (this recorder) differs (ruling E41).
async fn bybit_rest_poll_loop(
    options: &CaptureOptions,
    recorder: &mut RawRecorder,
    state: &mut CaptureState,
    capture_started: Instant,
) -> Result<(), CaptureError> {
    let deadline = capture_started + options.duration;
    let mut shutdown = Box::pin(tokio::signal::ctrl_c());

    state.connection_attempts = state
        .connection_attempts
        .checked_add(1)
        .ok_or(CaptureError::CounterExhausted)?;
    state.connection_epoch = 1;
    state.successful_connections = state
        .successful_connections
        .checked_add(1)
        .ok_or(CaptureError::CounterExhausted)?;
    let connect_started_elapsed_ns = elapsed_ns(capture_started)?;
    append_connected_record(
        recorder,
        options.stream,
        state.connection_epoch,
        &mut state.ordinal,
        options.max_records,
        capture_started,
        connect_started_elapsed_ns,
    )
    .await?;

    // The shared per-endpoint session over the production reqwest transport. The
    // recorder's own background flusher persists the file between polls, so there
    // is no flush timer here (R1/R4).
    let mut session = BybitRestSession::reqwest(Instant::now())?;

    loop {
        if state.ordinal >= options.max_records {
            break;
        }
        // Poll the endpoint due soonest; wait until its scheduled instant.
        let (due, wait_until) = session.next_poll();
        tokio::select! {
            result = &mut shutdown => {
                result.map_err(CaptureError::Signal)?;
                state.interrupted = true;
                break;
            }
            () = sleep_until(deadline) => break,
            () = sleep_until(wait_until) => {}
        }

        // Enforce the 1 req/s local spacing before issuing the request.
        if let Some(wait) = session.spacing_wait() {
            tokio::select! {
                result = &mut shutdown => {
                    result.map_err(CaptureError::Signal)?;
                    state.interrupted = true;
                    break;
                }
                () = sleep_until(deadline) => break,
                () = sleep(wait) => {}
            }
        }
        session.mark_requested();

        // Record the exact request URL as OutboundText BEFORE the GET, so a
        // failed poll still leaves an honest request trace.
        let sent_at = crate::raw::now_timestamp();
        let sent_elapsed_ns = elapsed_ns(capture_started)?;
        append_text_record(
            recorder,
            &mut state.ordinal,
            options.max_records,
            RawRecord::text_at(
                options.stream,
                state.connection_epoch,
                1,
                RecordKind::OutboundText,
                session.request_url(due).to_owned(),
                sent_at,
                sent_elapsed_ns,
            ),
        )
        .await?;

        let outcome = tokio::select! {
            result = &mut shutdown => {
                result.map_err(CaptureError::Signal)?;
                state.interrupted = true;
                break;
            }
            () = sleep_until(deadline) => break,
            outcome = session.fetch(due) => outcome,
        };

        // Classify every outcome and set exactly the right counter; no cause is
        // swallowed (F3/E27(c)). A served success is the verbatim `InboundText`
        // body; every non-served outcome is one schema-4 `Failed` record on its
        // per-endpoint ladder (ruling E32).
        let resolved = session.resolve(due, outcome, "capture");
        match resolved.counter {
            RestPollCounter::None => {}
            RestPollCounter::ServedError => {
                state.served_error_responses = state
                    .served_error_responses
                    .checked_add(1)
                    .ok_or(CaptureError::CounterExhausted)?;
            }
            RestPollCounter::NonSuccess => {
                state.non_success_responses = state
                    .non_success_responses
                    .checked_add(1)
                    .ok_or(CaptureError::CounterExhausted)?;
            }
            RestPollCounter::ReadIdle => {
                state.read_idle_timeouts = state
                    .read_idle_timeouts
                    .checked_add(1)
                    .ok_or(CaptureError::CounterExhausted)?;
            }
        }
        if state.ordinal < options.max_records {
            let received_at = crate::raw::now_timestamp();
            let capture_elapsed_ns = elapsed_ns(capture_started)?;
            let record = match resolved.record {
                RestResponseRecord::Inbound(body) => RawRecord::text_at(
                    options.stream,
                    state.connection_epoch,
                    1,
                    RecordKind::InboundText,
                    body,
                    received_at,
                    capture_elapsed_ns,
                ),
                RestResponseRecord::Failed { kind, detail } => RawRecord::failed_at(
                    options.stream,
                    state.connection_epoch,
                    1,
                    kind,
                    &detail,
                    received_at,
                    capture_elapsed_ns,
                ),
            };
            append_text_record(recorder, &mut state.ordinal, options.max_records, record).await?;
        }

        if let Some(line) = resolved.log {
            eprintln!("{line}");
        }
    }
    Ok(())
}

/// One Bybit REST poll endpoint's independent schedule. Two of these (open
/// interest, funding history) share the loop; each advances on its own cadence
/// and backs off on its own streak, so one endpoint's rate-limit ladder never
/// delays the other.
struct EndpointRest {
    url: &'static str,
    cadence: Duration,
    next_poll_at: Instant,
    failure_streak: u64,
}

/// The scheduling class of a poll outcome (counters are set separately).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PollClass {
    /// A served success: reset the streak, next poll one cadence out.
    Success,
    /// A served error (retCode != 0 / empty list): the retCode ladder.
    ServedError,
    /// A transport, timeout, decode or non-2xx outcome: the transport ladder.
    Transient,
}

impl EndpointRest {
    fn new(url: &'static str, cadence: Duration, now: Instant) -> Self {
        Self {
            url,
            cadence,
            // Both endpoints are due immediately at startup.
            next_poll_at: now,
            failure_streak: 0,
        }
    }

    /// Advance the schedule after a poll and return the delay that was applied.
    /// A success resets the streak to the plain cadence; a served error walks the
    /// `max(cadence, 60 s) → 3600 s` ladder; a transient walks the `5 s → 300 s`
    /// ladder. The delay moves `next_poll_at` — the loop waits on it — so the
    /// ladder observably spaces retries (R4).
    fn schedule_next(&mut self, class: PollClass, now: Instant) -> Duration {
        let delay = match class {
            PollClass::Success => {
                self.failure_streak = 0;
                self.cadence
            }
            PollClass::ServedError => {
                self.failure_streak = self.failure_streak.saturating_add(1);
                bybit_retcode_backoff(self.failure_streak, self.cadence)
            }
            PollClass::Transient => {
                self.failure_streak = self.failure_streak.saturating_add(1);
                bybit_rest_backoff(self.failure_streak)
            }
        };
        self.next_poll_at = now + delay;
        delay
    }
}

/// The classified outcome of one Bybit REST GET, so the caller sets the right
/// counter and never swallows the cause.
pub(crate) enum RestPollOutcome {
    /// A 2xx UTF-8 body (which may still carry a non-zero retCode or empty list).
    Served(String),
    /// A non-2xx HTTP status.
    NonSuccess(u16),
    /// The request failed, timed out, or the body could not be read/bounded.
    Transport,
    /// A 2xx body that was not valid UTF-8.
    Decode,
}

/// Execute one Bybit REST GET, classifying the outcome. The body is bounded at
/// the shared message cap.
async fn bybit_rest_get(client: &reqwest::Client, url: &str) -> RestPollOutcome {
    let Ok(response) = client
        .get(url)
        .header(
            reqwest::header::ACCEPT,
            reqwest::header::HeaderValue::from_static("application/json"),
        )
        .timeout(BYBIT_REST_REQUEST_TIMEOUT)
        .send()
        .await
    else {
        return RestPollOutcome::Transport;
    };
    let Ok((status, bytes)) =
        secure_http::bounded_body_with_status(response, MAX_MESSAGE_BYTES).await
    else {
        return RestPollOutcome::Transport;
    };
    if !(200..300).contains(&status) {
        return RestPollOutcome::NonSuccess(status);
    }
    match String::from_utf8(bytes) {
        Ok(body) => RestPollOutcome::Served(body),
        Err(_) => RestPollOutcome::Decode,
    }
}

/// What the Bybit REST loop does with one classified poll outcome. Splitting the
/// classification out of the loop keeps it pure, so the 451 / retCode-10006
/// record shapes are unit-testable without a network (ruling E32).
#[derive(Debug)]
struct BybitRestAction {
    /// The response record appended after the request's `OutboundText`.
    record: BybitRestActionRecord,
    /// The per-endpoint backoff class.
    class: PollClass,
    /// Which capture counter the outcome increments.
    counter: BybitRestCounter,
    /// The per-poll stderr cause, or `None` for a served success.
    log_cause: Option<String>,
}

/// The record a classified Bybit REST outcome writes.
#[derive(Debug)]
pub(crate) enum BybitRestActionRecord {
    /// A genuine served success: the verbatim body as `InboundText`.
    Body(String),
    /// Every non-served outcome — a transport/decode failure, a non-2xx status,
    /// or a served-error body (`retCode != 0` / empty / unparseable list) — as
    /// one schema-4 `Failed` record carrying our own cause. `kind` is the failure
    /// discriminant: `HttpStatus` for a non-2xx status, `Transport` for a
    /// transport/decode failure, `Served` for a 2xx body the venue answered but
    /// that carried an error (whose retCode/retMsg or bytes live in `detail`).
    Failed { kind: FailureKind, detail: String },
}

/// The capture counter a classified outcome increments.
#[derive(Debug)]
enum BybitRestCounter {
    None,
    ServedError,
    NonSuccess,
    ReadIdle,
}

/// Classify one Bybit REST poll outcome into the record to write, the backoff
/// class, the counter to bump, and the stderr cause. Every non-served outcome
/// becomes exactly one `Failed` record (ruling E32); only a genuine served
/// success is recorded as an `InboundText` body and resets the streak.
fn classify_bybit_rest_poll(outcome: RestPollOutcome) -> BybitRestAction {
    match outcome {
        RestPollOutcome::Served(body) => match bybit_served_error_detail(&body) {
            None => BybitRestAction {
                record: BybitRestActionRecord::Body(body),
                class: PollClass::Success,
                counter: BybitRestCounter::None,
                log_cause: None,
            },
            // A 2xx body the venue served but that carries an error is a `Served`
            // failure, never `transport`: the request WAS served (E39(c) m1).
            Some(detail) => BybitRestAction {
                record: BybitRestActionRecord::Failed {
                    kind: FailureKind::Served,
                    detail: detail.clone(),
                },
                class: PollClass::ServedError,
                counter: BybitRestCounter::ServedError,
                log_cause: Some(detail),
            },
        },
        RestPollOutcome::NonSuccess(status) => {
            let detail = format!("non-success HTTP {status}");
            BybitRestAction {
                record: BybitRestActionRecord::Failed {
                    kind: FailureKind::HttpStatus(status),
                    detail: detail.clone(),
                },
                class: PollClass::Transient,
                counter: BybitRestCounter::NonSuccess,
                log_cause: Some(detail),
            }
        }
        RestPollOutcome::Transport => BybitRestAction {
            record: BybitRestActionRecord::Failed {
                kind: FailureKind::Transport,
                detail: "transport failure or timeout".to_owned(),
            },
            class: PollClass::Transient,
            counter: BybitRestCounter::ReadIdle,
            log_cause: Some("transport failure or timeout".to_owned()),
        },
        RestPollOutcome::Decode => BybitRestAction {
            record: BybitRestActionRecord::Failed {
                kind: FailureKind::Transport,
                detail: "non-UTF-8 body".to_owned(),
            },
            class: PollClass::Transient,
            counter: BybitRestCounter::ReadIdle,
            log_cause: Some("non-UTF-8 body".to_owned()),
        },
    }
}

/// Inspect a 2xx Bybit REST body. Returns `None` for a genuine success
/// (`retCode == 0` and a non-empty `result.list`), or `Some(detail)` naming the
/// served-error cause — the retCode and retMsg, an empty list, or (for a body
/// that does not parse) a bounded slice of the venue bytes so schema drift is
/// still diagnosable (E39(c) m1). A malformed or error body is a served error so
/// the poller backs off rather than resetting its streak.
fn bybit_served_error_detail(body: &str) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Probe {
        #[serde(rename = "retCode")]
        ret_code: i64,
        #[serde(rename = "retMsg", default)]
        ret_msg: Option<String>,
        result: ResultProbe,
    }
    #[derive(serde::Deserialize)]
    struct ResultProbe {
        #[serde(default)]
        list: Vec<serde::de::IgnoredAny>,
    }
    match serde_json::from_str::<Probe>(body) {
        Ok(probe) if probe.ret_code == 0 && !probe.result.list.is_empty() => None,
        Ok(probe) if probe.ret_code != 0 => Some(format!(
            "served-error poll retCode {} ({})",
            probe.ret_code,
            crate::adapter::bounded_token(probe.ret_msg.as_deref().unwrap_or("")),
        )),
        Ok(_) => Some("served-error poll (retCode 0, empty result.list)".to_owned()),
        Err(_) => Some(format!(
            "served-error poll (unparseable 2xx body): {}",
            crate::adapter::bounded_token(body),
        )),
    }
}

/// Transport / non-success backoff ladder (spec §7): exponential from a 5 s base,
/// doubling, capped at 300 s.
fn bybit_rest_backoff(streak: u64) -> Duration {
    exponential_backoff(BYBIT_REST_BACKOFF_BASE, BYBIT_REST_BACKOFF_CAP, streak)
}

/// Served-error backoff ladder (retCode != 0 / empty list, ruled E31/F2): base
/// `max(cadence, 60 s)`, doubling, capped at 3600 s, so sustained rate-limiting
/// (retCode 10006) spaces retries out well past the poll cadence.
fn bybit_retcode_backoff(streak: u64, cadence: Duration) -> Duration {
    let base = cadence.max(BYBIT_RETCODE_BACKOFF_FLOOR);
    exponential_backoff(base, BYBIT_RETCODE_BACKOFF_CAP, streak)
}

/// `base * 2^(streak-1)`, saturating, clamped to `cap`. `streak == 0` yields the
/// base; a served success resets the streak to 0.
fn exponential_backoff(base: Duration, cap: Duration, streak: u64) -> Duration {
    let base_secs = base.as_secs().max(1);
    let shift = streak.saturating_sub(1).min(32);
    let scaled = base_secs.saturating_mul(1_u64 << shift);
    Duration::from_secs(scaled.min(cap.as_secs()))
}

/// Multi-request HTTP GET poll capture for the NBA ESPN status feed. Generalizes
/// the Chainlink single-request poll to a compiled ordered request cycle: it opens
/// one logical epoch with a `Connected` record, then on a fixed cadence executes
/// `nba_espn::REQUEST_CYCLE` in order. For EACH request it records the outbound
/// descriptor (`{"request","cycle","index"}`) as `OutboundText` -- these are GETs
/// with no body, so the descriptor IS the outbound record -- then the verbatim
/// response body as `InboundText`. Requests are independent: a single failing
/// request is counted honestly and the cycle continues; only a fully failed cycle
/// drives the shared reconnect backoff.
async fn nba_espn_poll_loop(
    options: &CaptureOptions,
    recorder: &mut RawRecorder,
    state: &mut CaptureState,
    capture_started: Instant,
) -> Result<(), CaptureError> {
    // Reuse the shared TLS-1.3-only HTTP client (mirrors the Chainlink poll loop).
    // CRITICAL: each request overrides the User-Agent to `nba_espn::CAPTURE_USER_AGENT`
    // (see `nba_espn_poll`). ESPN's Akamai wall is a UA ALLOWLIST keyed on the
    // leading token: generic tool prefixes (curl/*, python-requests/*) pass; the
    // shared client's own `tiny-signals-prototype/0.1`, the reqwest default, an
    // empty UA, and browser UAs all 403 (probe matrix 2026-08-31, live-reproduced).
    // The override is applied per-request only; the shared secure_http default is
    // untouched for every other provider.
    let client = secure_http::client().map_err(|_| CaptureError::NbaEspnClient)?;
    let deadline = capture_started + options.duration;
    let mut shutdown = Box::pin(tokio::signal::ctrl_c());

    // One logical connection spans the whole poll session: the Connected record
    // opens epoch 1 and every request/response pair rides that epoch. A poll feed
    // has no handshake to fail, so a successful open is recorded up front.
    state.connection_attempts = state
        .connection_attempts
        .checked_add(1)
        .ok_or(CaptureError::CounterExhausted)?;
    state.connection_epoch = 1;
    state.successful_connections = state
        .successful_connections
        .checked_add(1)
        .ok_or(CaptureError::CounterExhausted)?;
    let connect_started_elapsed_ns = elapsed_ns(capture_started)?;
    append_connected_record(
        recorder,
        options.stream,
        state.connection_epoch,
        &mut state.ordinal,
        options.max_records,
        capture_started,
        connect_started_elapsed_ns,
    )
    .await?;

    let mut interval = tokio::time::interval(crate::nba_espn::POLL_CADENCE);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Cycle counter, surfaced in each outbound descriptor.
    let mut cycle: u64 = 0;
    // Consecutive fully-failed cycles, driving the shared reconnect backoff.
    let mut failure_streak: u64 = 0;

    'capture: loop {
        if state.ordinal >= options.max_records {
            break;
        }
        tokio::select! {
            result = &mut shutdown => {
                result.map_err(CaptureError::Signal)?;
                state.interrupted = true;
                break;
            }
            () = sleep_until(deadline) => {
                break;
            }
            _ = interval.tick() => {}
        }

        cycle = cycle.checked_add(1).ok_or(CaptureError::CounterExhausted)?;
        // Count how many of this cycle's requests produced a usable response, so a
        // fully failed cycle (and only that) backs off.
        let mut cycle_successes: u64 = 0;
        for (index, url) in crate::nba_espn::REQUEST_CYCLE.iter().enumerate() {
            if state.ordinal >= options.max_records {
                break 'capture;
            }
            // Record the exact request descriptor as OutboundText BEFORE the GET,
            // so a failed request still leaves an honest request trace with no
            // fabricated response.
            let descriptor = crate::nba_espn::outbound_descriptor(url, cycle, index);
            let sent_at = crate::raw::now_timestamp();
            let sent_elapsed_ns = elapsed_ns(capture_started)?;
            append_text_record(
                recorder,
                &mut state.ordinal,
                options.max_records,
                RawRecord::text_at(
                    options.stream,
                    state.connection_epoch,
                    1,
                    RecordKind::OutboundText,
                    descriptor,
                    sent_at,
                    sent_elapsed_ns,
                ),
            )
            .await?;

            let poll = tokio::select! {
                result = &mut shutdown => {
                    result.map_err(CaptureError::Signal)?;
                    state.interrupted = true;
                    break 'capture;
                }
                () = sleep_until(deadline) => {
                    break 'capture;
                }
                poll = nba_espn_poll(&client, url) => poll,
            };

            match poll {
                Some(response_body) => {
                    cycle_successes = cycle_successes.saturating_add(1);
                    if state.ordinal < options.max_records {
                        let received_at = crate::raw::now_timestamp();
                        let capture_elapsed_ns = elapsed_ns(capture_started)?;
                        append_text_record(
                            recorder,
                            &mut state.ordinal,
                            options.max_records,
                            RawRecord::text_at(
                                options.stream,
                                state.connection_epoch,
                                1,
                                RecordKind::InboundText,
                                response_body,
                                received_at,
                                capture_elapsed_ns,
                            ),
                        )
                        .await?;
                    }
                }
                None => {
                    // A single request failing is not a cycle failure: count it
                    // honestly and continue the cycle (requests are independent).
                    // No inbound record is fabricated.
                    state.read_idle_timeouts = state
                        .read_idle_timeouts
                        .checked_add(1)
                        .ok_or(CaptureError::CounterExhausted)?;
                }
            }
        }

        // Back off between cycles only when the WHOLE cycle failed.
        if cycle_successes == 0 {
            failure_streak = failure_streak.saturating_add(1);
            let delay = reconnect_delay(failure_streak);
            tokio::select! {
                result = &mut shutdown => {
                    result.map_err(CaptureError::Signal)?;
                    state.interrupted = true;
                    break;
                }
                () = sleep_until(deadline) => {
                    break;
                }
                () = sleep(delay) => {}
            }
        } else {
            failure_streak = 0;
        }
    }
    Ok(())
}

/// Execute one ESPN GET. Returns the verbatim response body on a usable 2xx UTF-8
/// response, or `None` for any transport, timeout, status, size, or encoding
/// failure. The request leads with `nba_espn::CAPTURE_USER_AGENT` -- ESPN's wall
/// allowlists generic-tool UA prefixes and 403s the shared client's own UA, the
/// reqwest default, empty, and browser UAs (probe matrix 2026-08-31). This is a
/// truthful conventional client identifier, NOT browser-fingerprint forgery (the
/// distinct cdn.nba.com TLS/JA3 block, which forgery would defeat, is not pursued).
async fn nba_espn_poll(client: &reqwest::Client, url: &str) -> Option<String> {
    let response = client
        .get(url)
        .header(
            reqwest::header::USER_AGENT,
            reqwest::header::HeaderValue::from_static(crate::nba_espn::CAPTURE_USER_AGENT),
        )
        .header(
            reqwest::header::ACCEPT,
            reqwest::header::HeaderValue::from_static("application/json"),
        )
        .timeout(crate::nba_espn::REQUEST_TIMEOUT)
        .send()
        .await
        .ok()?;
    let (status, bytes) = secure_http::bounded_body_with_status(response, MAX_MESSAGE_BYTES)
        .await
        .ok()?;
    if !(200..300).contains(&status) {
        return None;
    }
    String::from_utf8(bytes).ok()
}

/// Bounded, metered, credential-safe HTTP GET matrix capture for the SportsDataIO
/// NBA candidate source (gate SDIO-1).
///
/// Structurally this is the ESPN multi-request poll cycle with four additions the
/// gate requires, and nothing else:
///
///   * AUTHENTICATION. Every GET carries the subscription key in one header and
///     nowhere else. The URL is built from a compiled template allowlist that
///     refuses a query string outright, so a `?key=` cannot exist on this path.
///   * A QUOTA. Each request is charged to an explicit budget BEFORE it is sent.
///     When the budget is spent the capture ends CLEANLY (a finished, valid
///     journal), rather than discovering the provider's own limit as 429s.
///   * STATUS EVIDENCE. A non-2xx answer is recorded as evidence (status,
///     allowlisted headers, capped body) instead of being dropped; a 3xx is
///     recorded as `redirect_refused` and its `Location` is never read or logged,
///     because an off-host redirect target is exactly the string a
///     credential-bearing client must not chase.
///   * REDACTION. Every error leaving this path is passed through the credential
///     redactor by the caller's choke point in `run_capture`.
///
/// Unchanged from the ESPN precedent: the outbound descriptor is recorded BEFORE
/// the GET so a failed request still leaves an honest request trace, no response
/// is ever fabricated, per-request failures are counted and the cycle continues,
/// and only a wholly failed cycle drives the shared jittered backoff.
async fn sportsdataio_nba_matrix_loop(
    options: &CaptureOptions,
    credential: &SportsDataIoCredential,
    matrix: &RequestMatrix,
    recorder: &mut RawRecorder,
    state: &mut CaptureState,
    capture_started: Instant,
) -> Result<(), CaptureError> {
    // Shared TLS-1.3-only client: api.sportsdata.io terminates TLS 1.3, so no
    // exception is needed, and redirects are already refused by its policy.
    let client = secure_http::client().map_err(|_| CaptureError::SportsDataIoClient)?;
    let deadline = capture_started + options.duration;
    let mut shutdown = Box::pin(tokio::signal::ctrl_c());
    let mut quota = RequestQuota::new(matrix.max_requests());
    let mut budget = ResponseBudget::new(crate::sportsdataio_nba::MAX_RUN_RESPONSE_BYTES);

    state.connection_attempts = state
        .connection_attempts
        .checked_add(1)
        .ok_or(CaptureError::CounterExhausted)?;
    state.connection_epoch = 1;
    state.successful_connections = state
        .successful_connections
        .checked_add(1)
        .ok_or(CaptureError::CounterExhausted)?;
    let connect_started_elapsed_ns = elapsed_ns(capture_started)?;
    append_connected_record(
        recorder,
        options.stream,
        state.connection_epoch,
        &mut state.ordinal,
        options.max_records,
        capture_started,
        connect_started_elapsed_ns,
    )
    .await?;

    let mut interval = tokio::time::interval(crate::sportsdataio_nba::POLL_CADENCE);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut cycle: u64 = 0;
    let mut failure_streak: u64 = 0;

    'capture: loop {
        if state.ordinal >= options.max_records {
            break;
        }
        tokio::select! {
            result = &mut shutdown => {
                result.map_err(CaptureError::Signal)?;
                state.interrupted = true;
                break;
            }
            () = sleep_until(deadline) => {
                break;
            }
            _ = interval.tick() => {}
        }

        cycle = cycle.checked_add(1).ok_or(CaptureError::CounterExhausted)?;
        let mut cycle_successes: u64 = 0;
        for (index, entry) in matrix.entries().iter().enumerate() {
            if state.ordinal >= options.max_records {
                break 'capture;
            }
            // Charge the quota BEFORE the request is sent, so a crash can never
            // under-count what the credential was billed. Exhaustion is a clean
            // end of run, not an error.
            if quota.try_consume().is_err() {
                break 'capture;
            }
            let url = crate::sportsdataio_nba::request_url(entry.operation, &entry.parameters)?;
            let descriptor =
                crate::sportsdataio_nba::outbound_descriptor(&url, entry.operation, cycle, index);
            let sent_at = crate::raw::now_timestamp();
            let sent_elapsed_ns = elapsed_ns(capture_started)?;
            append_text_record(
                recorder,
                &mut state.ordinal,
                options.max_records,
                RawRecord::text_at(
                    options.stream,
                    state.connection_epoch,
                    1,
                    RecordKind::OutboundText,
                    descriptor,
                    sent_at,
                    sent_elapsed_ns,
                ),
            )
            .await?;

            let attempt = tokio::select! {
                result = &mut shutdown => {
                    result.map_err(CaptureError::Signal)?;
                    state.interrupted = true;
                    break 'capture;
                }
                () = sleep_until(deadline) => {
                    break 'capture;
                }
                attempt = sportsdataio_nba_request(&client, credential, &url) => attempt,
            };

            let Some(response) = attempt else {
                // Transport, TLS, timeout, size or read failure: counted honestly,
                // no inbound record fabricated, cycle continues.
                state.read_idle_timeouts = state
                    .read_idle_timeouts
                    .checked_add(1)
                    .ok_or(CaptureError::CounterExhausted)?;
                sleep(crate::sportsdataio_nba::REQUEST_PACING).await;
                continue;
            };

            // Fail-closed aggregate byte reservation: a run that would exceed its
            // response budget stops before recording, never mid-record.
            let reserved =
                u64::try_from(response.body.len()).map_err(|_| CaptureError::CounterExhausted)?;
            if budget.try_reserve(reserved).is_err() {
                break 'capture;
            }

            let received_at = crate::raw::now_timestamp();
            let capture_elapsed_ns = elapsed_ns(capture_started)?;
            let envelope = crate::sportsdataio_nba::response_envelope(
                entry.operation,
                cycle,
                index,
                &received_at,
                response.status,
                response.outcome,
                response.headers,
                &response.body,
                response.truncated,
                response.redacted,
            );
            append_text_record(
                recorder,
                &mut state.ordinal,
                options.max_records,
                RawRecord::text_at(
                    options.stream,
                    state.connection_epoch,
                    1,
                    RecordKind::InboundText,
                    envelope,
                    received_at,
                    capture_elapsed_ns,
                ),
            )
            .await?;

            if response.outcome == ResponseOutcome::Response
                && (200..300).contains(&response.status)
            {
                cycle_successes = cycle_successes.saturating_add(1);
            } else {
                // A recorded non-2xx is evidence, not a usable read. It gets its
                // OWN counter: an auth failure or a rate-limit refusal is not a
                // read idle timeout, and a report that conflated the two would let
                // a 401-for-every-request run read as a transport problem. It still
                // does not count as a cycle success, so backoff is unchanged.
                state.non_success_responses = state
                    .non_success_responses
                    .checked_add(1)
                    .ok_or(CaptureError::CounterExhausted)?;
            }

            // Honour a provider-supplied Retry-After, bounded, and never past the
            // capture deadline.
            let pause = response
                .retry_after
                .unwrap_or(crate::sportsdataio_nba::REQUEST_PACING);
            tokio::select! {
                result = &mut shutdown => {
                    result.map_err(CaptureError::Signal)?;
                    state.interrupted = true;
                    break 'capture;
                }
                () = sleep_until(deadline) => {
                    break 'capture;
                }
                () = sleep(pause) => {}
            }
        }

        if cycle_successes == 0 {
            failure_streak = failure_streak.saturating_add(1);
            let delay = reconnect_delay(failure_streak);
            tokio::select! {
                result = &mut shutdown => {
                    result.map_err(CaptureError::Signal)?;
                    state.interrupted = true;
                    break;
                }
                () = sleep_until(deadline) => {
                    break;
                }
                () = sleep(delay) => {}
            }
        } else {
            failure_streak = 0;
        }
    }
    Ok(())
}

/// One recorded SportsDataIO response: everything the journal envelope needs and
/// nothing that could carry the credential.
struct SportsDataIoResponse {
    status: u16,
    outcome: ResponseOutcome,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
    truncated: bool,
    redacted: bool,
    retry_after: Option<Duration>,
}

/// How much of one SportsDataIO response body the journal may record, decided by
/// status BEFORE the body is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BodyRecording {
    /// 2xx: the provider payload IS the evidence. Read whole under the
    /// per-response cap; an over-cap payload is refused rather than half-kept,
    /// because a partial payload is not the artefact gate SDIO-2 compares.
    Payload,
    /// 4xx/5xx: the body is status EVIDENCE. Read TRUNCATED under the small error
    /// cap, never refused -- an oversized provider error page must stay a recorded
    /// non-2xx rather than being laundered into a transport failure.
    StatusEvidence,
    /// 3xx: nothing at all. The redirect is refused, and a 3xx body routinely
    /// embeds the target URL that ADR 0021 promises is never recorded, so the body
    /// is not read and the envelope carries zero bytes.
    Nothing,
}

/// The status-to-recording rule, as one pure decision so it can be pinned by test.
fn body_recording(status: u16) -> BodyRecording {
    if (200..300).contains(&status) {
        BodyRecording::Payload
    } else if (300..400).contains(&status) {
        BodyRecording::Nothing
    } else {
        BodyRecording::StatusEvidence
    }
}

/// Read one non-2xx SportsDataIO body as bounded status evidence: at most
/// `maximum_bytes`, TRUNCATED at the cap rather than refused.
///
/// `secure_http::bounded_body_with_status` refuses an over-cap body, which is the
/// right policy for a payload and the wrong one for an error page: refusing it
/// would drop the only evidence of WHY the provider said no and count the request
/// as a transport failure. `None` is reserved for a genuine read failure.
async fn sportsdataio_error_body(
    mut response: reqwest::Response,
    maximum_bytes: usize,
) -> Option<(Vec<u8>, bool)> {
    let mut body = Vec::new();
    let mut truncated = false;
    while let Some(chunk) = response.chunk().await.ok()? {
        if append_bounded(&mut body, &chunk, maximum_bytes) {
            truncated = true;
            break;
        }
    }
    Some((body, truncated))
}

/// The bounded accumulation `sportsdataio_error_body` performs, as one pure
/// function so the truncation boundary can be driven by test without a socket or
/// a `reqwest::Response`, which cannot be constructed in this crate.
///
/// Appends as much of `chunk` as the cap still allows and returns whether the cap
/// cut it -- the caller's `truncated` flag, which is also what makes the redactor
/// drop its tail guard.
fn append_bounded(body: &mut Vec<u8>, chunk: &[u8], maximum_bytes: usize) -> bool {
    let remaining = maximum_bytes.saturating_sub(body.len());
    if chunk.len() > remaining {
        body.extend_from_slice(&chunk[..remaining]);
        return true;
    }
    body.extend_from_slice(chunk);
    false
}

/// Execute one authenticated SportsDataIO GET.
///
/// Returns `None` for any transport, TLS, timeout, size or read failure -- no
/// response is fabricated and no error object (which would hold the request, and
/// therefore its headers) is ever formatted. A provider ANSWER, 2xx or not, is
/// returned so it can be recorded as evidence, including an over-cap error page,
/// which is truncated rather than dropped. A 3xx is marked `redirect_refused`
/// with an EMPTY body: its `Location` is neither followed nor recorded, and the
/// body it would arrive with routinely repeats that target inline. Every recorded
/// body AND every retained response-header value passes through the credential
/// redactor before it can reach the journal.
async fn sportsdataio_nba_request(
    client: &reqwest::Client,
    credential: &SportsDataIoCredential,
    url: &str,
) -> Option<SportsDataIoResponse> {
    let request = credential
        .authorize(
            client
                .get(url)
                .header(
                    reqwest::header::ACCEPT,
                    reqwest::header::HeaderValue::from_static("application/json"),
                )
                .timeout(crate::sportsdataio_nba::REQUEST_TIMEOUT),
        )
        .ok()?;
    let response = request.send().await.ok()?;
    let status = response.status().as_u16();
    let headers = crate::sportsdataio_nba::allowlisted_headers(response.headers(), credential);
    let retry_after = headers
        .get("retry-after")
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_secs)
        .map(|delay| delay.min(crate::sportsdataio_nba::MAX_RETRY_AFTER));
    // What may be recorded is decided by status BEFORE the body is read, so an
    // oversized error page is truncated instead of refused and a redirect body is
    // never read at all.
    let recording = body_recording(status);
    let (body, truncated) = match recording {
        BodyRecording::Payload => (
            secure_http::bounded_body_with_status(
                response,
                crate::sportsdataio_nba::MAX_RESPONSE_BYTES,
            )
            .await
            .ok()?
            .1,
            false,
        ),
        BodyRecording::StatusEvidence => {
            sportsdataio_error_body(response, crate::sportsdataio_nba::MAX_ERROR_BODY_BYTES).await?
        }
        // The response is dropped unread: `Location` is already off the header
        // allowlist, and a 301/302 HTML body carries the same target inline.
        BodyRecording::Nothing => (Vec::new(), true),
    };
    // Last stop before the journal. A provider or upstream error page is entitled
    // to echo the request headers it received, and the journal is the one sink the
    // repository secret scanner cannot cover after the fact, so the recorded bytes
    // pass through the credential redactor. The envelope's digest is then taken
    // over exactly these bytes.
    let (body, redacted) = credential.redact_body(&body, truncated);
    let outcome = match recording {
        BodyRecording::Nothing => ResponseOutcome::RedirectRefused,
        BodyRecording::Payload | BodyRecording::StatusEvidence => ResponseOutcome::Response,
    };
    Some(SportsDataIoResponse {
        status,
        outcome,
        headers,
        body,
        truncated,
        redacted,
        retry_after,
    })
}

/// Final redaction choke point for the SportsDataIO path. Any error whose
/// rendered form contains the credential is replaced by its redacted text before
/// it can reach stderr, a report or a journal-adjacent log.
fn redact_capture_error(error: CaptureError, credential: &SportsDataIoCredential) -> CaptureError {
    let rendered = error.to_string();
    let redacted = redact_secret(&rendered, credential);
    if redacted == rendered {
        error
    } else {
        CaptureError::SportsDataIoRedacted(redacted)
    }
}

/// What one crawl request resolved to.
enum CrawlStep {
    /// The request completed. `Some` carries the verbatim 2xx body; `None` means
    /// the request failed and no inbound record was fabricated for it.
    Done(Option<String>),
    /// The crawl must stop now: shutdown signal, capture deadline, or record cap.
    Stop,
}

/// Two-phase HTTP GET crawl capture for the NBA ESPN daily reference feed.
///
/// Where the slice-1 status poll re-executes a fixed two-URL cycle on a five
/// minute cadence, this stream walks a DISCOVERED entity tree exactly ONCE per
/// capture run and then finishes -- single-cycle-run semantics. There is no
/// cadence, no repeat, and therefore no cross-cycle dedup: the run IS the daily
/// snapshot, and re-running the binary is what produces the next day's file.
///
///   phase A  teams list (discovers team ids; emits nothing on replay)
///            standings
///            per team, in ascending id order: roster, statistics, records
///            (each roster discovers that team's athlete ids)
///   phase B  per athlete, in ascending id order: statistics, gamelog, splits
///
/// For EVERY request the loop records the outbound descriptor as `OutboundText`
/// BEFORE issuing the GET, then the verbatim response body as `InboundText`. The
/// descriptor carries the request's family and entity id, which ruling R1 makes
/// load-bearing: the gamelog and splits bodies contain no occurrence of the
/// athlete id, so the descriptor is the only carrier of their identity. Writing
/// the descriptor first and fabricating no inbound on failure is exactly what
/// makes the pairing sound -- and makes an orphaned descriptor a legal state the
/// adapter reports rather than an error.
///
/// Requests are independent: a single failure is counted honestly and the crawl
/// continues, so one dead athlete never costs the other 623.
async fn nba_espn_daily_crawl_loop(
    options: &CaptureOptions,
    recorder: &mut RawRecorder,
    state: &mut CaptureState,
    capture_started: Instant,
) -> Result<(), CaptureError> {
    // CRITICAL: this crawl uses the TLS 1.2-PERMITTED HTTP client, not the shared
    // 1.3-only default. Two of ESPN's three crawl hosts cap at TLS 1.2 and refuse
    // a 1.3-only handshake, so under the default client 145 of 177 requests
    // failed, every one of them on `sports.core.api.espn.com` or
    // `site.web.api.espn.com` (measured 2026-08-31). The exception is scoped to
    // this constructor call; the shared default is untouched for every other
    // provider, including slice 1's ESPN status poll, whose single host
    // (`site.api.espn.com`) does negotiate 1.3. See
    // `secure_http::tls12_compatible_client` for the full rationale. Every
    // compiled template is still HTTPS even though ESPN's core API advertises
    // itself over plain http in its own `$ref` links, so the crawl never leaves
    // TLS.
    let client =
        secure_http::tls12_compatible_client().map_err(|_| CaptureError::NbaEspnDailyClient)?;
    let deadline = capture_started + options.duration;
    let mut shutdown = Box::pin(tokio::signal::ctrl_c());

    // One logical connection spans the whole crawl. A crawl has no handshake to
    // fail, so a successful open is recorded up front (mirrors slice 1).
    state.connection_attempts = state
        .connection_attempts
        .checked_add(1)
        .ok_or(CaptureError::CounterExhausted)?;
    state.connection_epoch = 1;
    state.successful_connections = state
        .successful_connections
        .checked_add(1)
        .ok_or(CaptureError::CounterExhausted)?;
    let connect_started_elapsed_ns = elapsed_ns(capture_started)?;
    append_connected_record(
        recorder,
        options.stream,
        state.connection_epoch,
        &mut state.ordinal,
        options.max_records,
        capture_started,
        connect_started_elapsed_ns,
    )
    .await?;

    // Monotonic request index across the whole crawl, surfaced in every descriptor.
    let mut index: u64 = 0;
    // Ids are held in ordered sets so the journal is byte-stable across runs that
    // see the same league: ESPN returns teams in an unordered-looking sequence.
    let mut teams: BTreeSet<u64> = BTreeSet::new();
    let mut athletes: BTreeSet<u64> = BTreeSet::new();

    // --- Phase A.

    macro_rules! request {
        ($url:expr, $phase:expr, $family:expr, $entity:expr) => {{
            let url = $url;
            index = index.checked_add(1).ok_or(CaptureError::CounterExhausted)?;
            match nba_espn_daily_request(
                &client,
                options,
                recorder,
                state,
                capture_started,
                deadline,
                &mut shutdown,
                &url,
                $phase,
                $family,
                $entity,
                index,
            )
            .await?
            {
                CrawlStep::Stop => return Ok(()),
                CrawlStep::Done(body) => body,
            }
        }};
    }

    if let Some(body) = request!(
        crate::nba_espn_daily::TEAMS_URL.to_owned(),
        "a",
        "teams-list",
        ""
    ) {
        nba_espn_daily_collect_ids(&body, "teams", &mut teams, crate::nba_espn_daily::MAX_TEAMS);
    }
    let _ = request!(
        crate::nba_espn_daily::STANDINGS_URL.to_owned(),
        "a",
        "standings",
        ""
    );

    for team in teams.clone() {
        let team = team.to_string();
        if let Some(body) = request!(
            crate::nba_espn_daily::render(crate::nba_espn_daily::ROSTER_TEMPLATE, &team, ""),
            "a",
            "roster",
            &team
        ) {
            nba_espn_daily_collect_ids(
                &body,
                "athletes",
                &mut athletes,
                crate::nba_espn_daily::MAX_ATHLETES,
            );
        }
        let _ = request!(
            crate::nba_espn_daily::render(
                crate::nba_espn_daily::TEAM_STATISTICS_TEMPLATE,
                &team,
                ""
            ),
            "a",
            "team-statistics",
            &team
        );
        let _ = request!(
            crate::nba_espn_daily::render(crate::nba_espn_daily::TEAM_RECORD_TEMPLATE, &team, ""),
            "a",
            "team-record",
            &team
        );
    }

    // --- Phase B.

    for athlete in athletes.clone() {
        let athlete = athlete.to_string();
        for (template, family) in [
            (
                crate::nba_espn_daily::ATHLETE_STATISTICS_TEMPLATE,
                "athlete-statistics",
            ),
            (
                crate::nba_espn_daily::ATHLETE_GAMELOG_TEMPLATE,
                "athlete-gamelog",
            ),
            (
                crate::nba_espn_daily::ATHLETE_SPLITS_TEMPLATE,
                "athlete-splits",
            ),
        ] {
            let _ = request!(
                crate::nba_espn_daily::render(template, "", &athlete),
                "b",
                family,
                &athlete
            );
        }
    }

    Ok(())
}

/// Record one crawl request's descriptor, issue the GET, and record the verbatim
/// response body. Paces before each request so the whole crawl stays inside
/// ordinary browsing behaviour.
#[allow(clippy::too_many_arguments)]
async fn nba_espn_daily_request<F>(
    client: &reqwest::Client,
    options: &CaptureOptions,
    recorder: &mut RawRecorder,
    state: &mut CaptureState,
    capture_started: Instant,
    deadline: Instant,
    shutdown: &mut Pin<Box<F>>,
    url: &str,
    phase: &str,
    family: &str,
    entity_id: &str,
    index: u64,
) -> Result<CrawlStep, CaptureError>
where
    F: Future<Output = std::io::Result<()>>,
{
    if state.ordinal >= options.max_records {
        return Ok(CrawlStep::Stop);
    }
    tokio::select! {
        result = &mut *shutdown => {
            result.map_err(CaptureError::Signal)?;
            state.interrupted = true;
            return Ok(CrawlStep::Stop);
        }
        () = sleep_until(deadline) => {
            return Ok(CrawlStep::Stop);
        }
        () = sleep(crate::nba_espn_daily::REQUEST_PACING) => {}
    }

    // The descriptor is written BEFORE the GET, so a failed request still leaves
    // an honest request trace with no fabricated response -- and so the adapter's
    // pending-descriptor pairing has something to replace rather than a silent gap.
    let descriptor =
        crate::nba_espn_daily::outbound_descriptor(url, phase, family, entity_id, index);
    let sent_at = crate::raw::now_timestamp();
    let sent_elapsed_ns = elapsed_ns(capture_started)?;
    append_text_record(
        recorder,
        &mut state.ordinal,
        options.max_records,
        RawRecord::text_at(
            options.stream,
            state.connection_epoch,
            1,
            RecordKind::OutboundText,
            descriptor,
            sent_at,
            sent_elapsed_ns,
        ),
    )
    .await?;

    let poll = tokio::select! {
        result = &mut *shutdown => {
            result.map_err(CaptureError::Signal)?;
            state.interrupted = true;
            return Ok(CrawlStep::Stop);
        }
        () = sleep_until(deadline) => {
            return Ok(CrawlStep::Stop);
        }
        poll = nba_espn_daily_get(client, url) => poll,
    };

    match poll {
        Some(body) => {
            if state.ordinal < options.max_records {
                let received_at = crate::raw::now_timestamp();
                let capture_elapsed_ns = elapsed_ns(capture_started)?;
                append_text_record(
                    recorder,
                    &mut state.ordinal,
                    options.max_records,
                    RawRecord::text_at(
                        options.stream,
                        state.connection_epoch,
                        1,
                        RecordKind::InboundText,
                        body.clone(),
                        received_at,
                        capture_elapsed_ns,
                    ),
                )
                .await?;
            }
            Ok(CrawlStep::Done(Some(body)))
        }
        None => {
            // A single failed request is not a crawl failure: count it honestly
            // and continue. No inbound record is fabricated.
            state.read_idle_timeouts = state
                .read_idle_timeouts
                .checked_add(1)
                .ok_or(CaptureError::CounterExhausted)?;
            Ok(CrawlStep::Done(None))
        }
    }
}

/// Execute one crawl GET. Returns the verbatim response body on a usable 2xx
/// UTF-8 response, or `None` for any transport, timeout, status, size, or
/// encoding failure. Sends the same allowlisted User-Agent as slice 1 -- see the
/// `nba_espn` module header for the full ESPN allowlist probe matrix.
async fn nba_espn_daily_get(client: &reqwest::Client, url: &str) -> Option<String> {
    let response = client
        .get(url)
        .header(
            reqwest::header::USER_AGENT,
            reqwest::header::HeaderValue::from_static(crate::nba_espn::CAPTURE_USER_AGENT),
        )
        .header(
            reqwest::header::ACCEPT,
            reqwest::header::HeaderValue::from_static("application/json"),
        )
        .timeout(crate::nba_espn_daily::REQUEST_TIMEOUT)
        .send()
        .await
        .ok()?;
    let (status, bytes) = secure_http::bounded_body_with_status(response, MAX_MESSAGE_BYTES)
        .await
        .ok()?;
    if !(200..300).contains(&status) {
        return None;
    }
    String::from_utf8(bytes).ok()
}

/// Recover entity ids for the next crawl phase from a discovery response.
///
/// Deliberately structure-agnostic and permissive: it walks the parsed body for
/// every object under a `<collection>` array and takes that object's own `id`, or
/// the `id` of a nested single-key wrapper (`teams[].team.id`). A discovery
/// response that fails to parse simply yields nothing -- discovery is best-effort
/// and never aborts a capture. The set is ordered and hard-bounded, so a
/// malformed body can never drive an unbounded crawl.
fn nba_espn_daily_collect_ids(
    body: &str,
    collection: &str,
    into: &mut BTreeSet<u64>,
    bound: usize,
) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return;
    };
    nba_espn_daily_walk(&value, collection, into, bound);
}

fn nba_espn_daily_walk(
    value: &serde_json::Value,
    collection: &str,
    into: &mut BTreeSet<u64>,
    bound: usize,
) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, child) in map {
                if key == collection
                    && let Some(entries) = child.as_array()
                {
                    for entry in entries {
                        if into.len() >= bound {
                            return;
                        }
                        if let Some(id) = nba_espn_daily_entry_id(entry) {
                            into.insert(id);
                        }
                    }
                    continue;
                }
                nba_espn_daily_walk(child, collection, into, bound);
            }
        }
        serde_json::Value::Array(entries) => {
            for entry in entries {
                nba_espn_daily_walk(entry, collection, into, bound);
            }
        }
        _ => {}
    }
}

/// Read one collection entry's numeric id, unwrapping the one-level
/// `{"team": {"id": ...}}` shape the teams list uses.
fn nba_espn_daily_entry_id(entry: &serde_json::Value) -> Option<u64> {
    let direct = entry.get("id").and_then(nba_espn_daily_numeric_id);
    if direct.is_some() {
        return direct;
    }
    entry
        .as_object()?
        .values()
        .find_map(|nested| nested.get("id").and_then(nba_espn_daily_numeric_id))
}

/// ESPN renders ids as either a JSON string of digits or a JSON integer.
fn nba_espn_daily_numeric_id(value: &serde_json::Value) -> Option<u64> {
    match value {
        serde_json::Value::String(id) => id.parse().ok(),
        serde_json::Value::Number(id) => id.as_u64(),
        _ => None,
    }
}

/// Repeating discovered-cycle HTTP GET capture for the NBA ESPN game-day feed.
///
/// This is the third NBA capture shape and it combines the first two. Like the
/// slice-1 status poll it REPEATS on a cadence for the whole capture, because the
/// product is the time series itself; like the slice-2 daily crawl its per-cycle
/// request list is DISCOVERED rather than compiled.
///
/// One cycle:
///
///   1. `scoreboard?dates=YYYYMMDD` for today's UTC date. Computing that date is
///      the CAPTURE side's job -- the capture host already owns a wall clock (it
///      stamps `received_at`), while the adapter must stay clock-free. The date
///      is baked into the request URL and therefore into the recorded outbound
///      descriptor, so replay always knows which slate a cycle asked for.
///   2. per discovered event id, in ascending order: odds, then predictor.
///   3. per discovered event id, ONLY when `cycle % SUMMARY_CYCLE_MODULUS == 0`:
///      the summary. The cycle index is 0-BASED so the first cycle carries the
///      summaries; see the volume rationale on `nba_espn_games::SUMMARY_CYCLE_MODULUS`.
///
/// TWO HTTP clients, routed per host by `nba_espn_games::is_core_host`: the odds
/// and predictor endpoints live on `sports.core.api.espn.com`, which is TLS 1.2
/// only (measured in slice 2), while the scoreboard and summary live on
/// `site.api.espn.com`, which negotiates TLS 1.3. Neither client's default is
/// widened for any other provider.
///
/// Failure independence follows the siblings: for EVERY request the descriptor is
/// recorded as `OutboundText` BEFORE the GET and no inbound record is fabricated
/// when a request fails, so one dead event never costs the rest of the slate. The
/// one exception is the scoreboard: it IS the cycle's request list, so its
/// failure fails the whole cycle's discovery -- the failure is counted, the shared
/// reconnect backoff applies, and the next cycle re-discovers from scratch.
async fn nba_espn_games_poll_loop(
    options: &CaptureOptions,
    recorder: &mut RawRecorder,
    state: &mut CaptureState,
    capture_started: Instant,
) -> Result<(), CaptureError> {
    let site_client = secure_http::client().map_err(|_| CaptureError::NbaEspnGamesClient)?;
    let core_client =
        secure_http::tls12_compatible_client().map_err(|_| CaptureError::NbaEspnGamesClient)?;
    let deadline = capture_started + options.duration;
    let mut shutdown = Box::pin(tokio::signal::ctrl_c());

    // One logical connection spans the whole poll session (mirrors slice 1): a
    // poll feed has no handshake to fail, so a successful open is recorded up
    // front and every request/response pair rides that epoch.
    state.connection_attempts = state
        .connection_attempts
        .checked_add(1)
        .ok_or(CaptureError::CounterExhausted)?;
    state.connection_epoch = 1;
    state.successful_connections = state
        .successful_connections
        .checked_add(1)
        .ok_or(CaptureError::CounterExhausted)?;
    let connect_started_elapsed_ns = elapsed_ns(capture_started)?;
    append_connected_record(
        recorder,
        options.stream,
        state.connection_epoch,
        &mut state.ordinal,
        options.max_records,
        capture_started,
        connect_started_elapsed_ns,
    )
    .await?;

    let mut interval = tokio::time::interval(crate::nba_espn_games::POLL_CADENCE);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // 0-BASED, deliberately: the summary predicate is `cycle % modulus == 0`, so
    // the first cycle must be index 0 and must carry the summaries. Slice 1's
    // descriptor cycle is 1-based; this one is not, and the descriptor's `cycle`
    // field carries exactly this index.
    let mut cycle: u64 = 0;
    // Consecutive failed discoveries, driving the shared reconnect backoff.
    let mut failure_streak: u64 = 0;

    'capture: loop {
        if state.ordinal >= options.max_records {
            break;
        }
        tokio::select! {
            result = &mut shutdown => {
                result.map_err(CaptureError::Signal)?;
                state.interrupted = true;
                break;
            }
            () = sleep_until(deadline) => {
                break;
            }
            _ = interval.tick() => {}
        }

        // Position of the request inside this cycle, restarting each cycle
        // (slice-1 semantics for the descriptor's `index`).
        let mut index: u64 = 0;
        let date = utc_date_key(SystemTime::now());
        let scoreboard_url =
            crate::nba_espn_games::render(crate::nba_espn_games::SCOREBOARD_TEMPLATE, &date, "");
        let discovery = match nba_espn_games_request(
            &site_client,
            options,
            recorder,
            state,
            capture_started,
            deadline,
            &mut shutdown,
            &scoreboard_url,
            cycle,
            index,
            "",
        )
        .await?
        {
            CrawlStep::Stop => break 'capture,
            CrawlStep::Done(body) => body,
        };
        let Some(body) = discovery else {
            // Discovery failed: this cycle has no request list at all. Back off
            // and re-discover on the next cycle rather than reusing a stale one.
            failure_streak = failure_streak.saturating_add(1);
            cycle = cycle.checked_add(1).ok_or(CaptureError::CounterExhausted)?;
            let delay = reconnect_delay(failure_streak);
            tokio::select! {
                result = &mut shutdown => {
                    result.map_err(CaptureError::Signal)?;
                    state.interrupted = true;
                    break;
                }
                () = sleep_until(deadline) => {
                    break;
                }
                () = sleep(delay) => {}
            }
            continue;
        };
        failure_streak = 0;

        // The event ids of today's slate. The ordered, hard-bounded set keeps the
        // journal byte-stable across runs that see the same slate and makes an
        // unbounded cycle impossible. This reuses the sibling crawl's discovery
        // walk unchanged (ONE authority for "read the ids out of an ESPN
        // collection"): the scoreboard's top-level `events[]` entries carry
        // scalar `id`s, which is exactly the shape that walk expects.
        let mut events: BTreeSet<u64> = BTreeSet::new();
        nba_espn_daily_collect_ids(
            &body,
            "events",
            &mut events,
            crate::nba_espn_games::MAX_EVENTS_PER_CYCLE,
        );

        let fetch_summary = cycle.is_multiple_of(crate::nba_espn_games::SUMMARY_CYCLE_MODULUS);
        for event in events {
            let event = event.to_string();
            let mut urls = vec![
                crate::nba_espn_games::render(crate::nba_espn_games::ODDS_TEMPLATE, "", &event),
                crate::nba_espn_games::render(
                    crate::nba_espn_games::PREDICTOR_TEMPLATE,
                    "",
                    &event,
                ),
            ];
            if fetch_summary {
                urls.push(crate::nba_espn_games::render(
                    crate::nba_espn_games::SUMMARY_TEMPLATE,
                    "",
                    &event,
                ));
            }
            for url in urls {
                index = index.checked_add(1).ok_or(CaptureError::CounterExhausted)?;
                let client = if crate::nba_espn_games::is_core_host(&url) {
                    &core_client
                } else {
                    &site_client
                };
                match nba_espn_games_request(
                    client,
                    options,
                    recorder,
                    state,
                    capture_started,
                    deadline,
                    &mut shutdown,
                    &url,
                    cycle,
                    index,
                    &event,
                )
                .await?
                {
                    CrawlStep::Stop => break 'capture,
                    CrawlStep::Done(_) => {}
                }
            }
        }

        cycle = cycle.checked_add(1).ok_or(CaptureError::CounterExhausted)?;
    }
    Ok(())
}

/// The UTC calendar date in ESPN's `YYYYMMDD` scoreboard form.
///
/// Reading the wall clock is a CAPTURE-side act: the capture host already owns
/// one (it stamps every record's `received_at` through `raw::now_timestamp`),
/// while adapters must stay clock-free so replay is deterministic. The instant is
/// an argument rather than read inside, so the formatting is testable.
fn utc_date_key(now: SystemTime) -> String {
    chrono::DateTime::<chrono::Utc>::from(now)
        .format("%Y%m%d")
        .to_string()
}

/// Record one game-day request's descriptor, issue the GET, and record the
/// verbatim response body. Paces before each request so a full summary cycle
/// stays inside ordinary browsing behaviour.
#[allow(clippy::too_many_arguments)]
async fn nba_espn_games_request<F>(
    client: &reqwest::Client,
    options: &CaptureOptions,
    recorder: &mut RawRecorder,
    state: &mut CaptureState,
    capture_started: Instant,
    deadline: Instant,
    shutdown: &mut Pin<Box<F>>,
    url: &str,
    cycle: u64,
    index: u64,
    entity_id: &str,
) -> Result<CrawlStep, CaptureError>
where
    F: Future<Output = std::io::Result<()>>,
{
    if state.ordinal >= options.max_records {
        return Ok(CrawlStep::Stop);
    }
    tokio::select! {
        result = &mut *shutdown => {
            result.map_err(CaptureError::Signal)?;
            state.interrupted = true;
            return Ok(CrawlStep::Stop);
        }
        () = sleep_until(deadline) => {
            return Ok(CrawlStep::Stop);
        }
        () = sleep(crate::nba_espn_games::REQUEST_PACING) => {}
    }

    // The descriptor is written BEFORE the GET, so a failed request still leaves
    // an honest request trace with no fabricated response -- and so the adapter's
    // pending-descriptor pairing has something to replace rather than a silent gap.
    let descriptor = crate::nba_espn_games::outbound_descriptor(url, cycle, index, entity_id);
    let sent_at = crate::raw::now_timestamp();
    let sent_elapsed_ns = elapsed_ns(capture_started)?;
    append_text_record(
        recorder,
        &mut state.ordinal,
        options.max_records,
        RawRecord::text_at(
            options.stream,
            state.connection_epoch,
            1,
            RecordKind::OutboundText,
            descriptor,
            sent_at,
            sent_elapsed_ns,
        ),
    )
    .await?;

    let poll = tokio::select! {
        result = &mut *shutdown => {
            result.map_err(CaptureError::Signal)?;
            state.interrupted = true;
            return Ok(CrawlStep::Stop);
        }
        () = sleep_until(deadline) => {
            return Ok(CrawlStep::Stop);
        }
        poll = nba_espn_games_get(client, url) => poll,
    };

    match poll {
        Some(body) => {
            if state.ordinal < options.max_records {
                let received_at = crate::raw::now_timestamp();
                let capture_elapsed_ns = elapsed_ns(capture_started)?;
                append_text_record(
                    recorder,
                    &mut state.ordinal,
                    options.max_records,
                    RawRecord::text_at(
                        options.stream,
                        state.connection_epoch,
                        1,
                        RecordKind::InboundText,
                        body.clone(),
                        received_at,
                        capture_elapsed_ns,
                    ),
                )
                .await?;
            }
            Ok(CrawlStep::Done(Some(body)))
        }
        None => {
            // A single failed request is not a cycle failure: count it honestly
            // and continue. No inbound record is fabricated.
            state.read_idle_timeouts = state
                .read_idle_timeouts
                .checked_add(1)
                .ok_or(CaptureError::CounterExhausted)?;
            Ok(CrawlStep::Done(None))
        }
    }
}

/// Execute one game-day GET. Returns the verbatim response body on a usable 2xx
/// UTF-8 response, or `None` for any transport, timeout, status, size, or
/// encoding failure. Sends the same allowlisted User-Agent as slices 1 and 2 --
/// see the `nba_espn` module header for the full ESPN allowlist probe matrix. The
/// caller has already selected the TLS profile the URL's host requires.
async fn nba_espn_games_get(client: &reqwest::Client, url: &str) -> Option<String> {
    let response = client
        .get(url)
        .header(
            reqwest::header::USER_AGENT,
            reqwest::header::HeaderValue::from_static(crate::nba_espn::CAPTURE_USER_AGENT),
        )
        .header(
            reqwest::header::ACCEPT,
            reqwest::header::HeaderValue::from_static("application/json"),
        )
        .timeout(crate::nba_espn_games::REQUEST_TIMEOUT)
        .send()
        .await
        .ok()?;
    let (status, bytes) = secure_http::bounded_body_with_status(response, MAX_MESSAGE_BYTES)
        .await
        .ok()?;
    if !(200..300).contains(&status) {
        return None;
    }
    String::from_utf8(bytes).ok()
}

async fn finalize_capture(
    recorder: RawRecorder,
    capture_result: Result<(), CaptureError>,
) -> Result<u64, CaptureError> {
    let finalize_result = recorder.finish().await;
    match (capture_result, finalize_result) {
        (Ok(()), Ok(bytes_written)) => Ok(bytes_written),
        (Ok(()), Err(error)) => Err(CaptureError::Raw(error)),
        (Err(error), Ok(_)) => Err(error),
        (Err(capture), Err(finalize)) => Err(CaptureError::FinalizeAfterFailure {
            capture: Box::new(capture),
            finalize,
        }),
    }
}

pub(crate) fn install_crypto_provider() -> Result<(), CaptureError> {
    if rustls::crypto::CryptoProvider::get_default().is_some() {
        return Ok(());
    }

    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| CaptureError::CryptoProvider)
}

pub(crate) const fn endpoint(stream: CaptureStream) -> &'static str {
    match stream {
        CaptureStream::BinanceSpot => BINANCE_URL,
        CaptureStream::BinanceUsdmPublic => BINANCE_USDM_PUBLIC_URL,
        // The USD-M REST pollers issue their own bounded GETs, not a WebSocket
        // handshake; the poll loop renders the endpoint paths itself and never
        // calls this accessor.
        CaptureStream::BinanceUsdmRest => crate::binance_usdm_rest::REST_HOST,
        CaptureStream::BybitSpot => BYBIT_SPOT_URL,
        CaptureStream::BybitLinear => BYBIT_LINEAR_URL,
        // The Bybit linear REST polls hit HTTP GET endpoints under this base;
        // the poll loop renders each path itself and never calls this accessor.
        CaptureStream::BybitLinearRest => BYBIT_REST_BASE,
        // Chainlink polls over HTTP JSON-RPC, not a WebSocket; the poll loop uses
        // the `chainlink` endpoint failover list directly and never calls this WS
        // accessor. This defensive arm returns the primary default only.
        CaptureStream::ChainlinkAggregator => crate::chainlink::primary_rpc_endpoint(),
        // NBA ESPN polls over HTTP GET, not a WebSocket; the poll loop iterates
        // `nba_espn::REQUEST_CYCLE` directly and never calls this WS accessor.
        CaptureStream::NbaEspnStatus => crate::nba_espn::SCOREBOARD_URL,
        // The NBA ESPN daily crawl walks compiled HTTP GET templates, not a
        // WebSocket; the crawl loop renders them and never calls this accessor.
        CaptureStream::NbaEspnDaily => crate::nba_espn_daily::TEAMS_URL,
        // The NBA ESPN game-day cycle renders its own templates per cycle (the
        // scoreboard URL carries a wall-clock date, so there is no constant
        // endpoint); the poll loop never calls this accessor.
        CaptureStream::NbaEspnGames => crate::nba_espn_games::SCOREBOARD_TEMPLATE,
        // Pyth Hermes streams over an HTTPS SSE / REST GET, not a WebSocket; the
        // SSE and REST loops render the pinned URLs themselves and never call this
        // accessor. This defensive arm returns the host only.
        CaptureStream::PythHermesStream | CaptureStream::PythHermesRest => {
            crate::pyth_hermes::HERMES_HOST
        }
        CaptureStream::CoinbaseCore
        | CaptureStream::CoinbaseLevel3
        | CaptureStream::CoinbaseMechanisms => COINBASE_URL,
        CaptureStream::KrakenCore | CaptureStream::KrakenUsdtReference => KRAKEN_URL,
        CaptureStream::KrakenLevel3 => KRAKEN_LEVEL3_URL,
        CaptureStream::KalshiMarket => kalshi_websocket_endpoint(),
        // The index socket's URL is built per run from the configured path, so
        // this accessor reports the host's adopted endpoint and is never dialled
        // for this stream.
        CaptureStream::KalshiIndex => kalshi_websocket_endpoint(),
        CaptureStream::PolymarketMarket => polymarket_websocket_endpoint(),
        CaptureStream::PolymarketChainlinkTwap => "wss://ws-live-data.polymarket.com",
        // SportsDataIO is an authenticated HTTP GET matrix, not a WebSocket; the
        // matrix loop renders one URL per entry from the compiled template
        // allowlist and never calls this WS accessor.
        CaptureStream::SportsDataIoNba => crate::sportsdataio_nba::API_ROOT,
    }
}

/// Refuse a run whose rotation override names a stream that has no rotation.
///
/// A real check rather than a `debug_assert!`: a release build must not
/// silently attempt to rotate a stream whose replacement would carry no
/// subscription.
fn rotation_supported(options: &CaptureOptions) -> Result<(), CaptureError> {
    if options.connection_lifetime_override.is_some()
        && connection_lifetime(options.stream).is_none()
    {
        return Err(CaptureError::RotationNotSupported {
            stream: options.stream.as_str(),
        });
    }
    Ok(())
}

/// How long one connection to `stream` may live before it is proactively
/// replaced, or `None` for a stream with no venue-imposed session limit.
///
/// Only the Binance USD-M `public` socket has one today. Returning `None`
/// everywhere else is what keeps rotation off every adopted stream's path.
pub(crate) const fn connection_lifetime(stream: CaptureStream) -> Option<Duration> {
    match stream {
        CaptureStream::BinanceUsdmPublic => Some(BINANCE_USDM_CONNECTION_LIFETIME),
        _ => None,
    }
}

/// Whether a connection that has been open for `connected_for` is due to be
/// replaced. A stream with no lifetime is never due.
pub(crate) fn rotation_due(lifetime: Option<Duration>, connected_for: Duration) -> bool {
    lifetime.is_some_and(|lifetime| connected_for >= lifetime)
}

pub(crate) const fn read_idle_timeout(stream: CaptureStream) -> Duration {
    match stream {
        CaptureStream::BinanceSpot => BINANCE_READ_IDLE_TIMEOUT,
        CaptureStream::BinanceUsdmPublic => BINANCE_USDM_READ_IDLE_TIMEOUT,
        // Unused by the poll loop (which enforces a per-request timeout).
        CaptureStream::BinanceUsdmRest => crate::binance_usdm_rest::REQUEST_TIMEOUT,
        CaptureStream::BybitSpot | CaptureStream::BybitLinear => BYBIT_READ_IDLE_TIMEOUT,
        // Unused by the REST poll loop (which enforces a per-request timeout).
        CaptureStream::BybitLinearRest => BYBIT_REST_REQUEST_TIMEOUT,
        // Unused by the poll loop (which enforces a per-request timeout); the
        // per-request timeout is the honest analogue of a read-idle bound here.
        CaptureStream::ChainlinkAggregator => crate::chainlink::REQUEST_TIMEOUT,
        // Unused by the poll loop (which enforces a per-request timeout).
        CaptureStream::NbaEspnStatus => crate::nba_espn::REQUEST_TIMEOUT,
        // Unused by the crawl loop (which enforces a per-request timeout).
        CaptureStream::NbaEspnDaily => crate::nba_espn_daily::REQUEST_TIMEOUT,
        // Unused by the game-day poll loop (which enforces a per-request timeout).
        CaptureStream::NbaEspnGames => crate::nba_espn_games::REQUEST_TIMEOUT,
        CaptureStream::CoinbaseCore
        | CaptureStream::CoinbaseLevel3
        | CaptureStream::CoinbaseMechanisms => COINBASE_READ_IDLE_TIMEOUT,
        CaptureStream::KrakenCore
        | CaptureStream::KrakenLevel3
        | CaptureStream::KrakenUsdtReference => KRAKEN_READ_IDLE_TIMEOUT,
        CaptureStream::KalshiMarket | CaptureStream::KalshiIndex => KALSHI_READ_IDLE_TIMEOUT,
        CaptureStream::PolymarketMarket | CaptureStream::PolymarketChainlinkTwap => {
            POLYMARKET_READ_IDLE_TIMEOUT
        }
        // Unused by the matrix loop (which enforces a per-request timeout).
        CaptureStream::SportsDataIoNba => crate::sportsdataio_nba::REQUEST_TIMEOUT,
        // Unused by the SSE/REST loops (they enforce their own timeouts); the
        // per-request timeout is the honest analogue of a read-idle bound here.
        CaptureStream::PythHermesStream | CaptureStream::PythHermesRest => {
            crate::pyth_hermes::REQUEST_TIMEOUT
        }
    }
}

pub(crate) fn websocket_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .read_buffer_size(64 * 1024)
        .write_buffer_size(16 * 1024)
        .max_write_buffer_size(256 * 1024)
        .max_message_size(Some(MAX_MESSAGE_BYTES))
        .max_frame_size(Some(MAX_MESSAGE_BYTES))
}

async fn append_connected_record(
    recorder: &mut RawRecorder,
    stream: CaptureStream,
    connection_epoch: u64,
    ordinal: &mut u64,
    maximum: u64,
    capture_started: Instant,
    connect_started_elapsed_ns: u64,
) -> Result<(), CaptureError> {
    let next = next_ordinal(*ordinal, maximum)?;
    let received_at = crate::raw::now_timestamp();
    let capture_elapsed_ns = elapsed_ns(capture_started)?;
    recorder
        .append(&RawRecord::connected_at(
            stream,
            connection_epoch,
            next,
            received_at,
            capture_elapsed_ns,
            connect_started_elapsed_ns,
        ))
        .await?;
    *ordinal = next;
    Ok(())
}

async fn append_configuration_record(
    recorder: &mut RawRecorder,
    stream: CaptureStream,
    ordinal: &mut u64,
    maximum: u64,
    capture_started: Instant,
    payload: String,
) -> Result<(), CaptureError> {
    let next = next_ordinal(*ordinal, maximum)?;
    let received_at = crate::raw::now_timestamp();
    let capture_elapsed_ns = elapsed_ns(capture_started)?;
    recorder
        .append(&RawRecord::configuration_at(
            stream,
            1,
            next,
            payload,
            received_at,
            capture_elapsed_ns,
        ))
        .await?;
    *ordinal = next;
    Ok(())
}

async fn append_disconnected_record(
    recorder: &mut RawRecorder,
    stream: CaptureStream,
    connection_epoch: u64,
    ordinal: &mut u64,
    maximum: u64,
    capture_started: Instant,
) -> Result<(), CaptureError> {
    let next = next_ordinal(*ordinal, maximum)?;
    let received_at = crate::raw::now_timestamp();
    let capture_elapsed_ns = elapsed_ns(capture_started)?;
    recorder
        .append(&RawRecord::disconnected_at(
            stream,
            connection_epoch,
            next,
            received_at,
            capture_elapsed_ns,
        ))
        .await?;
    *ordinal = next;
    Ok(())
}

async fn append_text_record(
    recorder: &mut RawRecorder,
    ordinal: &mut u64,
    maximum: u64,
    mut record: RawRecord,
) -> Result<(), CaptureError> {
    let next = next_ordinal(*ordinal, maximum)?;
    record.ordinal = next;
    recorder.append(&record).await?;
    *ordinal = next;
    Ok(())
}

fn elapsed_ns(started: Instant) -> Result<u64, CaptureError> {
    u64::try_from(started.elapsed().as_nanos()).map_err(|_| CaptureError::CounterExhausted)
}

fn next_ordinal(current: u64, maximum: u64) -> Result<u64, CaptureError> {
    let next = current
        .checked_add(1)
        .ok_or(CaptureError::CounterExhausted)?;
    if next > maximum {
        return Err(CaptureError::RecordLimitReached);
    }
    Ok(next)
}

pub(crate) struct Subscription {
    pub(crate) wire: Zeroizing<String>,
    pub(crate) recorded: String,
}

#[derive(Serialize)]
struct KrakenLevel3Subscription<'a> {
    method: &'static str,
    params: KrakenLevel3SubscriptionParams<'a>,
    req_id: u64,
}

#[derive(Serialize)]
struct KrakenLevel3SubscriptionParams<'a> {
    channel: &'static str,
    symbol: [&'static str; 1],
    depth: u16,
    snapshot: bool,
    token: &'a str,
}

/// Which Kalshi index-channel families this stream may subscribe.
///
/// Only `kalshi-index` is gated: it exists to dial the rights-gated index socket
/// and nothing else, so an unarmed run would open that connection and record
/// from it. The answer comes from `SIGNALS_ENABLED_VENUES` through
/// [`signals_core::venue_gate`], one token per family (audit A1/F6).
///
/// Every other stream — the adopted `kalshi-market` included — returns
/// [`ArmedIndexSockets::NONE`] **without reading the variable** (audit A1/N3).
/// An adopted capture path must not start failing on a malformed allow-list it
/// never consulted before; the index channels are carried live by the Kalshi
/// prediction lane, which does its own gating, and never by `btc-feeds capture
/// --stream kalshi-market`.
fn armed_index_sockets(stream: CaptureStream) -> Result<ArmedIndexSockets, CaptureError> {
    if stream != CaptureStream::KalshiIndex {
        // Not read at all for an ungated stream: see above.
        return Ok(ArmedIndexSockets::NONE);
    }
    let raw = match std::env::var(signals_core::VENUE_GATE_ENV) {
        Ok(raw) => Some(raw),
        Err(std::env::VarError::NotPresent) => None,
        Err(std::env::VarError::NotUnicode(_)) => Some("<non-utf8 value>".to_owned()),
    };
    armed_index_sockets_from(stream, raw.as_deref())
}

/// [`armed_index_sockets`] against an already-read variable.
///
/// Split out so the decision is testable without mutating the process
/// environment, which is `unsafe` and would race every other test in the binary.
fn armed_index_sockets_from(
    stream: CaptureStream,
    raw: Option<&str>,
) -> Result<ArmedIndexSockets, CaptureError> {
    if stream != CaptureStream::KalshiIndex {
        return Ok(ArmedIndexSockets::NONE);
    }
    let enabled = EnabledVenues::parse(raw)
        .map_err(|error| CaptureError::InvalidOptions(error.to_string()))?;
    let mut armed = ArmedIndexSockets::NONE;
    for token in INDEX_SOCKET_TOKENS {
        if enabled.is_enabled(token) {
            armed = armed.with(token);
        }
    }
    if !armed.any() {
        return Err(CaptureError::InvalidOptions(format!(
            "the Kalshi index capture stream requires {VENUE_GATE_ENV} to list {}; clear the venue's RIGHTS-LEDGER row before listing it",
            INDEX_SOCKET_TOKENS.join(" or ")
        )));
    }
    Ok(armed)
}

/// Refuse a capture whose venue is not in the runtime allow-list (ruling R7).
///
/// Only the rights-gated venues are checked here, and only the streams this
/// group added: the gate belongs at the point where a byte would first reach
/// disk, and re-gating a stream an operator is already recording would change
/// behaviour this group was not asked to change. The Binance spot capture stays
/// ungated for that reason and is a phase-2 item once the rights row clears.
fn require_enabled_venue(stream: CaptureStream) -> Result<(), CaptureError> {
    let Some(token) = gated_venue_token(stream) else {
        return Ok(());
    };
    let enabled = EnabledVenues::from_env()
        .map_err(|error| CaptureError::InvalidOptions(error.to_string()))?;
    require_enabled_venue_with(stream, token, &enabled)
}

/// The rights-gated venue token `stream` belongs to, if any.
const fn gated_venue_token(stream: CaptureStream) -> Option<&'static str> {
    match stream {
        // Every Binance surface, spot included. The spot stream was left ungated
        // in the first pass on the grounds that re-gating a running capture was
        // out of scope; that reasoning did not survive the same commit widening
        // what spot records (it gained `btcusdt@bookTicker`) while the venue's
        // RIGHTS-LEDGER row is still open. Its R1 hour has finished, so nothing
        // is recording, and one venue now means one gate.
        CaptureStream::BinanceSpot
        | CaptureStream::BinanceUsdmPublic
        | CaptureStream::BinanceUsdmRest => Some("binance"),
        // The Pyth Hermes streams are authenticated but their rights row is
        // still open, so the R7 allow-list must gate them too: the API key
        // refuses a missing credential, but only `SIGNALS_ENABLED_VENUES`
        // listing `pyth` admits the venue. Without this arm the key is the sole
        // gate and the exit-78 refusal the source contract advertises never
        // fires.
        CaptureStream::PythHermesStream | CaptureStream::PythHermesRest => Some("pyth"),
        _ => None,
    }
}

/// What one proactive rotation attempt produced.
enum RotationOutcome {
    /// A replacement socket that has already delivered one real data frame.
    /// Boxed: the socket dwarfs every other variant, and this enum is returned
    /// by value on a path that mostly does not rotate.
    Rotated(Box<Rotation>),
    /// The replacement could not be dialled, or never delivered a frame. The
    /// caller keeps the socket it already has.
    Failed,
    /// The operator interrupted the run.
    Interrupted,
    /// The capture deadline arrived.
    Deadline,
}

/// A proven-live replacement connection and the frame that proved it.
struct Rotation {
    socket: WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    first_message: String,
    connect_started_elapsed_ns: u64,
    /// Frames taken off the retiring socket after the replacement was proven
    /// live, minus any the replacement has already delivered. Recorded on the
    /// RETIRING epoch, before its `Disconnected`.
    drained: Vec<String>,
}

/// Dial a replacement connection and prove it live before the caller retires
/// the current one (spec §2.3, "make-before-break").
///
/// "Proven live" means one TEXT frame — a venue keepalive or an upgrade alone
/// does not show the stream is flowing, and the whole point of rotating early
/// is to avoid discovering a dead replacement after closing a live socket.
///
/// Only streams with a [`connection_lifetime`] reach this path — `run_capture`
/// refuses the override on any other stream, and the loop derives the lifetime
/// from the stream rather than from the override. Both of those connections are
/// keyless with no discovery and no subscribe frame: their whole subscription
/// is in the URL, so a replacement needs nothing but the dial. That is a
/// precondition of this function, not a hope: a stream needing a SUBSCRIBE
/// would get a silent socket.
async fn rotate_connection(
    options: &CaptureOptions,
    capture_started: Instant,
    shutdown: &mut Pin<Box<impl Future<Output = std::io::Result<()>>>>,
    deadline: Instant,
    retiring: &mut WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
) -> Result<RotationOutcome, CaptureError> {
    let connect_started_elapsed_ns = elapsed_ns(capture_started)?;
    let request = endpoint(options.stream)
        .into_client_request()
        .map_err(CaptureError::WebSocket)?;
    let connected = tokio::select! {
        result = &mut *shutdown => {
            result.map_err(CaptureError::Signal)?;
            return Ok(RotationOutcome::Interrupted);
        }
        () = sleep_until(deadline) => return Ok(RotationOutcome::Deadline),
        result = timeout(
            CONNECT_TIMEOUT,
            connect_async_tls_with_config(
                request,
                Some(websocket_config()),
                true,
                Some(Connector::Rustls(crate::websocket_tls::client_config_for(
                    options.stream,
                ))),
            ),
        ) => result,
    };
    let mut replacement = match connected {
        Ok(Ok((socket, _))) => socket,
        outcome @ (Ok(Err(_)) | Err(_)) => {
            report_failed_connection(
                options.stream,
                0,
                &match outcome {
                    Ok(Err(error)) => format!("rotation dial failed: {error}"),
                    Err(_) => format!("rotation dial found no response within {CONNECT_TIMEOUT:?}"),
                    Ok(Ok(_)) => unreachable!("the success arm returned above"),
                },
            );
            return Ok(RotationOutcome::Failed);
        }
    };
    // Wait for one text frame. Non-text frames (the venue's own ping, a pong)
    // prove the socket is open but not that the stream is flowing, so they are
    // skipped rather than accepted as proof.
    loop {
        let message = tokio::select! {
            result = &mut *shutdown => {
                result.map_err(CaptureError::Signal)?;
                return Ok(RotationOutcome::Interrupted);
            }
            () = sleep_until(deadline) => return Ok(RotationOutcome::Deadline),
            result = timeout(read_idle_timeout(options.stream), replacement.next()) => result,
        };
        match message {
            Ok(Some(Ok(Message::Text(text)))) => {
                let first_message = text.as_str().to_owned();
                // The retiring socket kept receiving while we dialled. Take what
                // it has before closing it, so the dial round-trip is not a
                // hole in the tape.
                let drained = drain_retiring(
                    retiring,
                    crate::binance_usdm::upstream_sequence(&first_message).as_ref(),
                )
                .await;
                // `received_at`/`capture_elapsed_ns` for the proving frame are
                // NOT stamped here: the frame is written on the new epoch only
                // after the drained frames, the `Disconnected` and the
                // `Connected` records, so its clock must be sampled at that
                // write time, not now — otherwise the record carries a stamp
                // earlier than the records that precede it in the file and
                // `latency::analyze` reports spurious clock regressions.
                return Ok(RotationOutcome::Rotated(Box::new(Rotation {
                    socket: replacement,
                    first_message,
                    connect_started_elapsed_ns,
                    drained,
                })));
            }
            Ok(Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_)))) => continue,
            _ => {
                report_failed_connection(
                    options.stream,
                    0,
                    "rotation replacement delivered no data frame; keeping the current connection",
                );
                return Ok(RotationOutcome::Failed);
            }
        }
    }
}

/// Take what the retiring socket has left, within a bounded deadline.
///
/// Called only after the replacement is proven live, so this covers exactly the
/// gap the dial round-trip opened. Frames the replacement has already delivered
/// — same stream, sequence at or past its first — are dropped rather than
/// recorded twice; frames whose stream carries no venue sequence are kept, and
/// the adapter's own cursors are what catch a duplicate among those.
///
/// The drain stops at the first non-text frame, close, error or silence: a
/// socket that has nothing more to say says nothing, and waiting the full
/// deadline on every rotation would make the switchover slower than the gap it
/// is closing.
async fn drain_retiring(
    retiring: &mut WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    replacement_first: Option<&(String, u64)>,
) -> Vec<String> {
    let drain_deadline = Instant::now() + ROTATION_DRAIN_TIMEOUT;
    let mut drained = Vec::new();
    while drained.len() < ROTATION_DRAIN_MAX_FRAMES {
        let Some(remaining) = drain_deadline.checked_duration_since(Instant::now()) else {
            break;
        };
        if remaining.is_zero() {
            break;
        }
        match timeout(remaining, retiring.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => {
                let text = text.as_str().to_owned();
                if !crate::binance_usdm::frame_is_superseded(&text, replacement_first) {
                    drained.push(text);
                }
            }
            // A keepalive proves nothing and carries nothing; keep draining.
            Ok(Some(Ok(Message::Ping(_) | Message::Pong(_)))) => {}
            _ => break,
        }
    }
    drained
}

/// Report a dial that never became a connection (ruling E27(c)).
///
/// The host is named, not just the stream: the whole point of E27 is that one
/// venue answers on one host and blackholes another, and an operator reading
/// this line needs to know which endpoint was refused.
fn report_failed_connection(stream: CaptureStream, attempt: u64, cause: &str) {
    eprintln!(
        "capture {}: connection attempt {attempt} to {} failed: {cause}",
        stream.as_str(),
        endpoint(stream)
    );
}

/// [`require_enabled_venue`] against an already-parsed allow-list.
///
/// Split out so the decision is testable without mutating the process
/// environment, which this crate cannot do: `unsafe_code` is forbidden and
/// `set_var` is unsafe. The same split `btc-signals::lane_gate` uses.
fn require_enabled_venue_with(
    stream: CaptureStream,
    token: &'static str,
    enabled: &EnabledVenues,
) -> Result<(), CaptureError> {
    if enabled.is_enabled(token) {
        return Ok(());
    }
    Err(CaptureError::VenueNotEnabled {
        stream: stream.as_str(),
        token,
        env: VENUE_GATE_ENV,
    })
}

/// The index-channel subscribe commands for the armed families, in a fixed
/// order. Each family is gated on its own token: `cfbrti` arms the CF
/// Benchmarks channels, `pyth` the relay (audit A1/F6).
///
/// `cfbenchmarks_value_5hz` rides the `cfbrti` token with the 1 Hz channel: both
/// are CF Benchmarks BRTI. It is requested on every connection even though the
/// venue often refuses it — the R1 hour refused it on three connections and
/// accepted it on the fourth (ruling E25), so a refusal is answered with a
/// notice and a retry on the next connection, never by dropping the subscribe.
fn kalshi_index_subscriptions(index_sockets: ArmedIndexSockets) -> Vec<Value> {
    let mut values = Vec::new();
    if index_sockets.is_armed("cfbrti") {
        values.push(json!({
            "id": crate::kalshi_index::VALUE_COMMAND_ID,
            "cmd": "subscribe",
            "params": {
                "channels": [crate::kalshi_index::VALUE_CHANNEL],
                "index_ids": [crate::kalshi_index::DEFAULT_INDEX_ID]
            }
        }));
        values.push(json!({
            "id": crate::kalshi_index::VALUE_5HZ_COMMAND_ID,
            "cmd": "subscribe",
            "params": {
                "channels": [crate::kalshi_index::VALUE_5HZ_CHANNEL],
                "index_ids": [crate::kalshi_index::DEFAULT_INDEX_ID]
            }
        }));
    }
    if index_sockets.is_armed("pyth") {
        values.push(json!({
            "id": crate::kalshi_index::PYTH_COMMAND_ID,
            "cmd": "subscribe",
            "params": {
                "channels": [crate::kalshi_index::PYTH_CHANNEL],
                "underlying_tickers": [crate::kalshi_index::DEFAULT_UNDERLYING_TICKER]
            }
        }));
    }
    values
}

/// Build the subscribe commands for one stream.
///
/// `index_sockets` names the Kalshi index-channel families the venue gate armed
/// (ruling E2, audit A1/F6). It is honoured on both Kalshi streams: the market
/// socket carries the index channels alongside its own when a family is armed,
/// and the dedicated index capture stream carries only the armed families.
pub(crate) fn subscriptions(
    stream: CaptureStream,
    kraken_btc_book_depth: u16,
    token: Option<&KrakenWebSocketToken>,
    polymarket_discovery: Option<&PolymarketDiscovery>,
    kalshi_discovery: Option<&KalshiDiscovery>,
    index_sockets: ArmedIndexSockets,
) -> Result<Vec<Subscription>, CaptureError> {
    let values = match stream {
        // Keyless combined spot streams: one full top-20 partial-book snapshot
        // every 100ms plus the aggregate-trade tape. The endpoint is the combined
        // `/stream` path, so every data message arrives wrapped as
        // `{"stream":<name>,"data":{..}}` and the adapter dispatches by name.
        CaptureStream::BinanceSpot => vec![json!({
            "method": "SUBSCRIBE",
            "params": [
                crate::binance::DEPTH_STREAM,
                crate::binance::AGG_TRADE_STREAM,
                crate::binance::BOOK_TICKER_STREAM
            ],
            "id": 1
        })],
        // Binance USD-M subscribes through the combined-stream URL, so there is
        // no SUBSCRIBE frame to send: the stream is named in the query string
        // and is live the moment the socket upgrades. Sending one anyway would
        // re-subscribe the stream the connection already carries.
        CaptureStream::BinanceUsdmPublic => vec![],
        // The USD-M REST pollers have no WebSocket subscription: the poll loop
        // issues its own GETs and never calls this function.
        CaptureStream::BinanceUsdmRest => vec![],
        // Pyth Hermes has no subscribe frame: the SSE query string IS the
        // subscription, and the REST anchor is a plain GET. Neither loop calls
        // this function.
        CaptureStream::PythHermesStream | CaptureStream::PythHermesRest => vec![],
        // Bybit v5 public subscribe: one request naming the stream's topics.
        // Spot carries the trade tape and both book depths; linear carries the
        // aggregated liquidation feed. Topics are the adapter's own constants.
        CaptureStream::BybitSpot => vec![json!({
            "req_id": "1",
            "op": "subscribe",
            "args": crate::bybit::SPOT_TOPICS
        })],
        CaptureStream::BybitLinear => vec![json!({
            "req_id": "1",
            "op": "subscribe",
            "args": crate::bybit::LINEAR_TOPICS
        })],
        // The Bybit linear REST polls have no WebSocket subscription: the poll
        // loop issues its own GET requests and never calls this function.
        CaptureStream::BybitLinearRest => vec![],
        // Chainlink has no WebSocket subscription: the poll loop issues its own
        // `eth_call` requests and never calls this function.
        CaptureStream::ChainlinkAggregator => vec![],
        // NBA ESPN has no WebSocket subscription: the poll loop issues its own
        // GET cycle and never calls this function.
        CaptureStream::NbaEspnStatus => vec![],
        // The NBA ESPN daily crawl has no WebSocket subscription either, and
        // neither does the game-day cycle.
        CaptureStream::NbaEspnDaily => vec![],
        CaptureStream::NbaEspnGames => vec![],
        // SportsDataIO is an authenticated HTTP GET matrix: the matrix loop issues
        // its own requests and never calls this function.
        CaptureStream::SportsDataIoNba => vec![],
        CaptureStream::CoinbaseCore => vec![json!({
            "type": "subscribe",
            "channels": [
                {"name": "heartbeat", "product_ids": ["BTC-USD"]},
                {"name": "status"},
                {"name": "level2_batch", "product_ids": ["BTC-USD"]},
                {"name": "matches", "product_ids": ["BTC-USD"]},
                {"name": "ticker", "product_ids": ["BTC-USD", "USDT-USD"]}
            ]
        })],
        CaptureStream::CoinbaseLevel3 => vec![json!({
            "type": "subscribe",
            "channels": [
                {"name": "heartbeat", "product_ids": ["BTC-USD"]},
                {"name": "full", "product_ids": ["BTC-USD"]}
            ]
        })],
        CaptureStream::CoinbaseMechanisms => vec![json!({
            "type": "subscribe",
            "channels": [
                {"name": "heartbeat", "product_ids": ["BTC-USD"]},
                {"name": "auctionfeed", "product_ids": ["BTC-USD"]},
                {"name": "rfq_matches", "product_ids": ["BTC-USD"]}
            ]
        })],
        CaptureStream::KrakenCore => vec![
            json!({
                "method": "subscribe",
                "params": {
                    "channel": "instrument",
                    "execution_venue": "international",
                    "include_tokenized_assets": false,
                    "snapshot": true
                },
                "req_id": 1
            }),
            json!({
                "method": "subscribe",
                "params": {"channel": "trade", "symbol": ["BTC/USD", "USDT/USD"], "snapshot": true},
                "req_id": 2
            }),
            json!({
                "method": "subscribe",
                "params": {
                    "channel": "ticker",
                    "symbol": ["BTC/USD", "USDT/USD"],
                    "event_trigger": "bbo",
                    "snapshot": true
                },
                "req_id": 3
            }),
            json!({
                "method": "subscribe",
                "params": {
                    "channel": "book",
                    "symbol": ["BTC/USD"],
                    "depth": kraken_btc_book_depth,
                    "snapshot": true
                },
                "req_id": 4
            }),
            json!({
                "method": "subscribe",
                    "params": {"channel": "book", "symbol": ["USDT/USD"], "depth": KRAKEN_USDT_BOOK_DEPTH, "snapshot": true},
                "req_id": 5
            }),
            json!({
                "method": "subscribe",
                "params": {"channel": "ohlc", "symbol": ["BTC/USD", "USDT/USD"], "interval": 1, "snapshot": true},
                "req_id": 6
            }),
        ],
        // E47(e): the dedicated USDT/USD reference socket subscribes ONLY the
        // `ticker` channel on `USDT/USD`, with the same `bbo` trigger and
        // snapshot flag the core socket uses, so the connection carries nothing
        // but the reference frames and the Kraken adapter emits only
        // `kraken.spot.usdt-usd.ticker`. No book channel, so no depth.
        CaptureStream::KrakenUsdtReference => vec![json!({
            "method": "subscribe",
            "params": {
                "channel": "ticker",
                "symbol": ["USDT/USD"],
                "event_trigger": "bbo",
                "snapshot": true
            },
            "req_id": 1
        })],
        CaptureStream::KrakenLevel3 => {
            let token = token.ok_or(CaptureError::MissingKrakenToken)?;
            let wire = KrakenLevel3Subscription {
                method: "subscribe",
                params: KrakenLevel3SubscriptionParams {
                    channel: "level3",
                    symbol: ["BTC/USD"],
                    depth: kraken_btc_book_depth,
                    snapshot: true,
                    token: token.expose(),
                },
                req_id: 1,
            };
            let recorded = json!({
                "method": "subscribe",
                "params": {
                    "channel": "level3",
                    "symbol": ["BTC/USD"],
                    "depth": kraken_btc_book_depth,
                    "snapshot": true,
                    "token": "[REDACTED]"
                },
                "req_id": 1
            });
            return Ok(vec![Subscription {
                wire: Zeroizing::new(serde_json::to_string(&wire).map_err(CaptureError::Json)?),
                recorded: serde_json::to_string(&recorded).map_err(CaptureError::Json)?,
            }]);
        }
        CaptureStream::PolymarketMarket => {
            let discovery = polymarket_discovery.ok_or(CaptureError::MissingPolymarketDiscovery)?;
            vec![json!({
                "assets_ids": discovery.asset_ids,
                "type": "market",
                "custom_feature_enabled": true
            })]
        }
        CaptureStream::PolymarketChainlinkTwap => vec![json!({
            "action": "subscribe",
            "subscriptions": [{
                "topic": crate::polymarket_twap::TOPIC,
                "type": "update",
                "filters": crate::polymarket_twap::SYMBOL_FILTER
            }]
        })],
        // The three index channels ride one socket, subscribed in a fixed order.
        // `["all"]` is never sent, and no `update_subscription`/`*_list` command
        // is emitted in wave A: `DATA-AVAILABILITY.md` records those paths as
        // unverified, and no request shape is documented for `pyth_value` at all.
        // The envelope shape is operator configuration, not a guess (ruling R1).
        // The index channels are subscribed under the market command envelope,
        // not the per-channel paths (observed, ruling E23); the live lane carries
        // them on a dedicated index socket (ruling E37d). The dedicated index
        // capture stream opens its own connection to the same endpoint and
        // subscribes only the index channels. `["all"]` is never sent, and no
        // `update_subscription` or `*_list` command is emitted.
        CaptureStream::KalshiIndex => kalshi_index_subscriptions(index_sockets),
        CaptureStream::KalshiMarket => {
            let discovery = kalshi_discovery.ok_or(CaptureError::MissingKalshiDiscovery)?;
            let mut values = vec![
                json!({
                    "id": 1,
                    "cmd": "subscribe",
                    "params": {
                        "channels": ["orderbook_delta"],
                        "market_ticker": discovery.market_ticker,
                        "use_yes_price": true
                    }
                }),
                json!({
                    "id": 2,
                    "cmd": "subscribe",
                    "params": {
                        "channels": ["ticker"],
                        "market_ticker": discovery.market_ticker
                    }
                }),
                json!({
                    "id": 3,
                    "cmd": "subscribe",
                    "params": {
                        "channels": ["trade"],
                        "market_ticker": discovery.market_ticker
                    }
                }),
            ];
            // One socket, market + index channels (E23). The index commands use
            // a reserved id band above the market commands', which is what lets
            // an `error` reply — it names only the id it answers — be attributed
            // to the channel it rejected.
            values.extend(kalshi_index_subscriptions(index_sockets));
            values
        }
    };
    values
        .into_iter()
        .map(|value| {
            let value = serde_json::to_string(&value).map_err(CaptureError::Json)?;
            Ok(Subscription {
                wire: Zeroizing::new(value.clone()),
                recorded: value,
            })
        })
        .collect()
}

/// Backoff before re-dialling after a connection that produced nothing.
///
/// One second doubling to a sixty-second cap. It is driven by the streak of
/// UNPRODUCTIVE connections — a dial that failed, or one that opened and
/// delivered no message before going idle — and reset by the first inbound
/// message of any connection, because a connection that delivered data was not
/// a failure however briefly it lived.
///
/// Without it, a venue that accepts the socket and then sends nothing (the
/// observed Binance USD-M `market` behaviour) makes the capture loop redial the
/// instant the read-idle timeout fires, for ever: roughly 360 connection epochs
/// an hour of pure churn, none of it data.
pub(crate) fn redial_backoff(unproductive_streak: u64) -> Duration {
    if unproductive_streak == 0 {
        return Duration::ZERO;
    }
    let exponent = u32::try_from(unproductive_streak.saturating_sub(1).min(6)).unwrap_or(6);
    Duration::from_secs(2_u64.saturating_pow(exponent)).min(REDIAL_MAX_BACKOFF)
}

/// Whether this streak is the first at its backoff level.
///
/// Reporting is per escalation STEP, not per attempt: a permanently unreachable
/// endpoint should leave a short, readable ladder in the log — seven lines and
/// then silence — not one line every second for an hour.
pub(crate) fn redial_backoff_stepped(unproductive_streak: u64) -> bool {
    unproductive_streak == 1
        || redial_backoff(unproductive_streak) != redial_backoff(unproductive_streak - 1)
}

pub(crate) fn reconnect_delay(attempt: u64) -> Duration {
    let exponent = u32::try_from(attempt.saturating_sub(1).min(4)).unwrap_or(4);
    let base_ms = 500_u64.saturating_mul(2_u64.saturating_pow(exponent));
    let jitter_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| u64::from(duration.subsec_millis()) % 250);
    Duration::from_millis(base_ms.saturating_add(jitter_ms)).min(MAX_BACKOFF)
}

/// Client text-heartbeat contract for a stream: how often to send, and the
/// exact payload to send and record byte-for-byte as an `OutboundText`. `None`
/// for streams without a client heartbeat. One function pairs the interval and
/// payload so the two can never disagree by construction.
pub(crate) fn text_heartbeat(stream: CaptureStream) -> Option<(Duration, &'static str)> {
    match stream {
        CaptureStream::PolymarketMarket => {
            Some((POLYMARKET_TEXT_HEARTBEAT_INTERVAL, POLYMARKET_PING_PAYLOAD))
        }
        CaptureStream::PolymarketChainlinkTwap => Some((
            POLYMARKET_RTDS_TEXT_HEARTBEAT_INTERVAL,
            POLYMARKET_PING_PAYLOAD,
        )),
        CaptureStream::BybitSpot | CaptureStream::BybitLinear => {
            Some((BYBIT_TEXT_HEARTBEAT_INTERVAL, BYBIT_PING_PAYLOAD))
        }
        _ => None,
    }
}

/// True when the time since the last successful read has reached the read-idle
/// timeout. Pure so it can be unit-tested without a live socket.
pub(crate) fn read_idle_exceeded(since_last_read: Duration, timeout: Duration) -> bool {
    since_last_read >= timeout
}

/// Every terminal failure a capture run can end with.
#[derive(Debug, Error)]
pub enum CaptureError {
    /// Returned when [`CaptureOptions`] validation rejects the requested run
    /// before any connection is opened. Carries the specific rule that failed.
    #[error("invalid capture options: {0}")]
    InvalidOptions(String),
    /// Returned when a rights-gated venue's capture stream is not enabled by
    /// `SIGNALS_ENABLED_VENUES` (R7). The binary maps this onto exit 78
    /// (`EX_CONFIG`); a lane binary (group A6) does the same.
    #[error("{0}")]
    VenueRefused(String),
    /// Returned when the append-only recorder fails to build, append, or flush a
    /// record. The byte-cap variant is handled as a normal stop, not an error.
    #[error(transparent)]
    Raw(#[from] RawRecordError),
    /// Returned when a subscription or heartbeat frame cannot be serialized to
    /// JSON, which means an unmodelled subscription shape rather than a fault.
    #[error("subscription JSON serialization failed: {0}")]
    Json(serde_json::Error),
    /// Returned when a WebSocket send fails or the socket errors outside the
    /// paths that reconnect. Transient read failures redial instead.
    #[error("WebSocket operation failed: {0}")]
    WebSocket(tokio_tungstenite::tungstenite::Error),
    /// Returned when awaiting the operating-system stop signal itself fails.
    #[error("shutdown signal failed: {0}")]
    Signal(std::io::Error),
    /// Returned when a u64 counter, ordinal, or elapsed-nanosecond read would
    /// overflow. The run fails closed rather than wrapping a sequence number.
    #[error("capture counters exhausted")]
    CounterExhausted,
    /// Returned when the next record would exceed `max_records`. Unlike the
    /// byte cap, reaching the record cap ends the run as a failure.
    #[error("capture record limit reached")]
    RecordLimitReached,
    /// Returned when the run reached its deadline with zero successful
    /// connections and no operator interrupt.
    #[error("no provider connection succeeded before the capture deadline")]
    NoConnection,
    /// Returned when the process-wide Rustls ring provider cannot be installed,
    /// so no TLS connection can be made.
    #[error("failed to install the Rustls ring crypto provider")]
    CryptoProvider,
    /// Returned when the Chainlink JSON-RPC HTTP client cannot be constructed
    /// from the shared TLS 1.3-only configuration.
    #[error("failed to construct the Chainlink JSON-RPC HTTP client")]
    ChainlinkClient,
    /// Returned when the Binance USD-M REST poller's HTTP client cannot be built.
    #[error("failed to construct the Binance USD-M REST HTTP client")]
    BinanceUsdmRestClient,
    /// Returned when `connection_lifetime_override` is set on a stream that has
    /// no proactive rotation of its own.
    ///
    /// Exit 78 like the venue refusal: both are operator configuration a blind
    /// restart must not paper over.
    #[error(
        "the {stream} capture stream has no connection lifetime, so --connection-lifetime-seconds cannot shorten one; rotation is wired only for the Binance USD-M sockets"
    )]
    RotationNotSupported {
        /// The refused capture stream's token.
        stream: &'static str,
    },
    /// Returned when the capture's venue is not in `SIGNALS_ENABLED_VENUES`
    /// (ruling R7).
    ///
    /// Its own variant rather than an `InvalidOptions` string because the
    /// binary maps it to exit 78 (`EX_CONFIG`) — the same exit the lane gate
    /// uses, so one venue refusal has one exit contract however it is reached,
    /// and a supervisor cannot restart around either of them.
    #[error(
        "the {stream} capture stream requires {env} to list {token}; clear the venue's RIGHTS-LEDGER row before listing it"
    )]
    VenueNotEnabled {
        /// The refused capture stream's token.
        stream: &'static str,
        /// The allow-list entry that would have admitted it.
        token: &'static str,
        /// The environment variable carrying the allow-list.
        env: &'static str,
    },
    /// Returned when the NBA ESPN scoreboard-status HTTP client cannot be built.
    #[error("failed to construct the NBA ESPN status HTTP client")]
    NbaEspnClient,
    /// Returned when the NBA ESPN daily-crawl HTTP client cannot be built. That
    /// host needs the TLS 1.2-compatible client.
    #[error("failed to construct the NBA ESPN daily HTTP client")]
    NbaEspnDailyClient,
    /// Returned when either NBA ESPN game-day HTTP client cannot be built.
    #[error("failed to construct the NBA ESPN game-day HTTP client")]
    NbaEspnGamesClient,
    /// Returned when the SportsDataIO NBA HTTP client cannot be built.
    #[error("failed to construct the SportsDataIO NBA HTTP client")]
    SportsDataIoClient,
    /// Returned when the SportsDataIO credential is missing or malformed in the
    /// environment. Rendered without the secret.
    #[error(transparent)]
    SportsDataIoAuth(#[from] SportsDataIoAuthError),
    /// Returned when the Pyth Hermes credential is missing or malformed in the
    /// environment. Rendered without the secret. The binary maps it to exit 78
    /// (`EX_CONFIG`): a refuse-closed feed with no key is a configuration
    /// decision a supervisor must not blind-restart, exactly like a venue gate.
    #[error(transparent)]
    PythHermesAuth(#[from] PythHermesAuthError),
    /// Returned when the Pyth Hermes venue rejects the key or the id/path
    /// (401/403/404): terminal, no retry. Mapped to exit 78 for the same reason
    /// as the missing key.
    #[error("{0}")]
    PythHermesTerminal(String),
    /// Returned when the Pyth Hermes HTTPS client cannot be built from the shared
    /// TLS 1.3-only configuration.
    #[error("failed to construct the Pyth Hermes HTTPS client")]
    PythHermesClient,
    /// Returned when a Pyth Hermes loop is reached without the environment-injected
    /// credential having been resolved.
    #[error("Pyth Hermes capture requires the environment-injected credential")]
    MissingPythHermesCredential,
    /// Returned when the request-matrix control file is rejected, or when a
    /// quota or response budget is exceeded during the matrix loop.
    #[error(transparent)]
    SportsDataIoControl(#[from] SportsDataIoNbaError),
    /// Returned when the SportsDataIO matrix loop is reached without the
    /// environment-injected credential having been resolved.
    #[error("SportsDataIO capture requires the environment-injected credential")]
    MissingSportsDataIoCredential,
    /// Returned when the SportsDataIO matrix loop is reached without a validated
    /// request matrix.
    #[error("SportsDataIO capture requires a validated request matrix")]
    MissingSportsDataIoMatrix,
    /// A SportsDataIO error whose rendered form carried the credential and was
    /// replaced by its redacted text at the capture path's choke point.
    #[error("SportsDataIO capture failed: {0}")]
    SportsDataIoRedacted(String),
    /// Returned when minting the Kraken authenticated-WebSocket token fails,
    /// which is required before a KrakenLevel3 subscription can be sent.
    #[error(transparent)]
    KrakenAuth(#[from] KrakenAuthError),
    /// Returned when signing the Kalshi WebSocket upgrade request fails.
    #[error(transparent)]
    KalshiAuth(#[from] crate::KalshiAuthError),
    /// Returned when keyless Kalshi REST discovery cannot resolve the requested
    /// market ticker to a subscribable market.
    #[error(transparent)]
    KalshiDiscovery(#[from] KalshiDiscoveryError),
    /// Returned when Polymarket Gamma discovery cannot resolve the requested
    /// slug to its asset ids.
    #[error(transparent)]
    PolymarketDiscovery(#[from] PolymarketDiscoveryError),
    /// Returned when the KrakenLevel3 subscription set is built without a token
    /// having been minted for this connection.
    #[error("Kraken level-three capture requires a fresh WebSocket token")]
    MissingKrakenToken,
    /// Returned when the Polymarket subscription set is built before discovery
    /// has produced a validated record.
    #[error("Polymarket capture requires a validated Gamma discovery record")]
    MissingPolymarketDiscovery,
    /// Returned when the Kalshi subscription set is built before discovery has
    /// produced a validated record.
    #[error("Kalshi capture requires a validated keyless REST discovery record")]
    MissingKalshiDiscovery,
    /// Returned when a capture failure and the closing durability flush both
    /// failed, so neither cause can be dropped in favour of the other.
    #[error("capture failed ({capture}) and its final durability flush also failed ({finalize})")]
    FinalizeAfterFailure {
        /// The failure that ended the capture loop, boxed to keep this enum small.
        capture: Box<CaptureError>,
        /// The recorder failure that then prevented a durable close, so the file
        /// on disk may end in a torn tail.
        finalize: RawRecordError,
    },
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::path::PathBuf;
    use std::time::Duration;

    use super::{
        BINANCE_URL, BYBIT_FUNDING_HISTORY_URL, BYBIT_FUNDING_POLL_CADENCE, BYBIT_LINEAR_URL,
        BYBIT_OPEN_INTEREST_POLL_CADENCE, BYBIT_OPEN_INTEREST_URL, BYBIT_PING_PAYLOAD,
        BYBIT_SPOT_URL, BYBIT_TEXT_HEARTBEAT_INTERVAL, BodyRecording, COINBASE_URL, CaptureError,
        CaptureOptions, EndpointRest, KRAKEN_LEVEL3_URL, KRAKEN_URL, MAX_BACKOFF,
        POLYMARKET_PING_PAYLOAD, POLYMARKET_RTDS_TEXT_HEARTBEAT_INTERVAL,
        POLYMARKET_TEXT_HEARTBEAT_INTERVAL, PollClass, RestPollOutcome, append_bounded,
        armed_index_sockets_from, body_recording, bybit_rest_backoff, bybit_retcode_backoff,
        bybit_served_error_detail, classify_bybit_rest_poll, endpoint,
        ensure_bybit_capture_enabled_from, ensure_live_venue_enabled_from, finalize_capture,
        nba_espn_daily_collect_ids, read_idle_exceeded, reconnect_delay, secure_http,
        subscriptions, text_heartbeat, utc_date_key,
    };
    use super::{BybitRestAction, BybitRestActionRecord, BybitRestCounter};
    use crate::kraken_auth::KrakenWebSocketToken;
    use crate::{CaptureStream, FailureKind, RawRecord};
    use signals_core::ArmedIndexSockets;
    use tokio::time::Instant;

    /// Audit A1/N3: only the gated `kalshi-index` stream consults
    /// `SIGNALS_ENABLED_VENUES`. The adopted capture paths — `kalshi-market`
    /// included — never did, and a malformed allow-list must not newly refuse
    /// them.
    #[test]
    fn only_the_index_capture_stream_reads_the_venue_allow_list() {
        for stream in [
            CaptureStream::KalshiMarket,
            CaptureStream::CoinbaseCore,
            CaptureStream::KrakenCore,
            CaptureStream::PolymarketMarket,
        ] {
            for raw in [
                None,
                Some("ftx,not-a-venue"),
                Some(""),
                Some("kalshi,cfbrti"),
            ] {
                assert_eq!(
                    armed_index_sockets_from(stream, raw).expect("an ungated stream never refuses"),
                    ArmedIndexSockets::NONE,
                    "{stream:?} with {raw:?}"
                );
            }
        }

        // The index stream does read it: it refuses an unarmed allow-list and a
        // malformed one, and arms only the families that are listed.
        assert!(matches!(
            armed_index_sockets_from(CaptureStream::KalshiIndex, Some("kalshi")),
            Err(CaptureError::InvalidOptions(_))
        ));
        assert!(matches!(
            armed_index_sockets_from(CaptureStream::KalshiIndex, Some("ftx")),
            Err(CaptureError::InvalidOptions(_))
        ));
        let armed = armed_index_sockets_from(CaptureStream::KalshiIndex, Some("kalshi,cfbrti"))
            .expect("a listed token arms its family");
        assert!(armed.is_armed("cfbrti") && !armed.is_armed("pyth"));
    }

    /// R7 capture-path gate: the three Bybit streams refuse to start unless
    /// `SIGNALS_ENABLED_VENUES` lists `bybit`; every other stream is ungated and
    /// never reads the variable (E2). A lane binary maps the refusal to
    /// EXIT_FATAL (78).
    #[test]
    fn bybit_capture_streams_refuse_unless_the_allow_list_lists_bybit() {
        for stream in [
            CaptureStream::BybitSpot,
            CaptureStream::BybitLinear,
            CaptureStream::BybitLinearRest,
        ] {
            // Unset (adopted-venue default), a list without bybit, and an empty
            // list each refuse.
            for raw in [None, Some("coinbase,kraken"), Some("")] {
                let refused = ensure_bybit_capture_enabled_from(stream, raw);
                assert!(
                    matches!(refused, Err(CaptureError::VenueRefused(_))),
                    "{stream:?} with {raw:?} must refuse with the typed venue error"
                );
                if let Err(CaptureError::VenueRefused(reason)) = refused {
                    assert!(reason.contains("RIGHTS-LEDGER"), "{reason}");
                }
            }
            // Listed: the stream starts.
            ensure_bybit_capture_enabled_from(stream, Some("bybit"))
                .expect("a listed bybit token admits the capture stream");
        }
        // A non-Bybit stream is never gated, even by a malformed allow-list.
        for raw in [None, Some("ftx,not-a-venue"), Some("")] {
            ensure_bybit_capture_enabled_from(CaptureStream::CoinbaseCore, raw)
                .expect("an ungated stream never refuses");
        }
    }

    /// The live-runner R7 gate (B2): a rights-gated venue stream refuses unless
    /// its token is listed, so `btc-feeds run` and the lane shell keep an unlisted
    /// venue off the wire; an ungated stream always admits.
    #[test]
    fn the_live_venue_gate_refuses_unlisted_venues_and_admits_listed_ones() {
        // Every Binance and Bybit surface the venue lanes drive refuses when its
        // venue is absent from the allow-list (unset, or a list without it).
        for stream in [
            CaptureStream::BinanceSpot,
            CaptureStream::BinanceUsdmPublic,
            CaptureStream::BinanceUsdmRest,
            CaptureStream::BybitSpot,
            CaptureStream::BybitLinear,
            CaptureStream::BybitLinearRest,
        ] {
            for raw in [None, Some("coinbase,kraken"), Some("")] {
                assert!(
                    ensure_live_venue_enabled_from(stream, raw).is_err(),
                    "{stream:?} with {raw:?} must refuse before any socket"
                );
            }
        }
        // Listed: the same streams admit and reach the live runner.
        for stream in [CaptureStream::BinanceSpot, CaptureStream::BinanceUsdmRest] {
            ensure_live_venue_enabled_from(stream, Some("binance,bybit"))
                .expect("a listed binance stream admits");
        }
        for stream in [CaptureStream::BybitSpot, CaptureStream::BybitLinearRest] {
            ensure_live_venue_enabled_from(stream, Some("binance,bybit"))
                .expect("a listed bybit stream admits");
        }
        // An ungated venue never consults the allow-list, even a malformed one.
        for raw in [None, Some("ftx,not-a-venue"), Some("")] {
            ensure_live_venue_enabled_from(CaptureStream::CoinbaseCore, raw)
                .expect("an ungated stream always admits");
        }
    }

    #[test]
    fn backoff_is_capped() {
        assert!(reconnect_delay(100) <= MAX_BACKOFF);
    }

    #[test]
    fn bybit_rest_backoff_ladders_fire_and_cap() {
        // Transport / non-success (§7): base 5 s, doubling, cap 300 s.
        assert_eq!(bybit_rest_backoff(0), Duration::from_secs(5));
        assert_eq!(bybit_rest_backoff(1), Duration::from_secs(5));
        assert_eq!(bybit_rest_backoff(2), Duration::from_secs(10));
        assert_eq!(bybit_rest_backoff(3), Duration::from_secs(20));
        assert_eq!(bybit_rest_backoff(100), Duration::from_secs(300));
        // The loop applies this by moving the endpoint's `next_poll_at`
        // (`EndpointRest::schedule_next`), so the delay genuinely spaces the
        // retry — see `per_endpoint_schedule_isolates_backoff_and_moves_the_ladder`.
        assert!(bybit_rest_backoff(4) >= Duration::from_secs(40));

        // Served-error (retCode/empty): base max(cadence, 60 s), doubling, cap
        // 3600 s. Open interest polls at 300 s, so its floor is the cadence.
        let cadence = BYBIT_OPEN_INTEREST_POLL_CADENCE;
        assert_eq!(bybit_retcode_backoff(1, cadence), cadence);
        assert_eq!(bybit_retcode_backoff(2, cadence), cadence * 2);
        assert_eq!(
            bybit_retcode_backoff(100, cadence),
            Duration::from_secs(3_600)
        );
        // A short cadence floors at 60 s.
        assert_eq!(
            bybit_retcode_backoff(1, Duration::from_secs(5)),
            Duration::from_secs(60)
        );
    }

    #[test]
    fn per_endpoint_schedule_isolates_backoff_and_moves_the_ladder() {
        let now = Instant::now();
        let mut oi = EndpointRest::new(
            BYBIT_OPEN_INTEREST_URL,
            BYBIT_OPEN_INTEREST_POLL_CADENCE,
            now,
        );
        let mut funding =
            EndpointRest::new(BYBIT_FUNDING_HISTORY_URL, BYBIT_FUNDING_POLL_CADENCE, now);

        // A served error on funding delays only funding (retCode ladder, base
        // max(3600, 60) = 3600 s); open interest's schedule is untouched.
        let funding_delay = funding.schedule_next(PollClass::ServedError, now);
        assert_eq!(funding_delay, Duration::from_secs(3600));
        assert_eq!(funding.next_poll_at, now + Duration::from_secs(3600));
        assert_eq!(oi.next_poll_at, now, "funding backoff must not touch OI");

        // Open interest keeps its 300 s cadence across the hour = 12 samples, no
        // matter what funding is doing.
        let hour = now + Duration::from_secs(3600);
        let mut samples = 0;
        let mut at = now;
        while oi.next_poll_at < hour {
            oi.schedule_next(PollClass::Success, at);
            samples += 1;
            at = oi.next_poll_at;
        }
        assert_eq!(
            samples, 12,
            "12 open-interest samples still taken in the hour"
        );

        // The transient ladder observably moves the retry (5, 10, 20 s …) and a
        // served success resets the streak.
        let mut ep = EndpointRest::new(
            BYBIT_OPEN_INTEREST_URL,
            BYBIT_OPEN_INTEREST_POLL_CADENCE,
            now,
        );
        assert_eq!(
            ep.schedule_next(PollClass::Transient, now),
            Duration::from_secs(5)
        );
        assert_eq!(
            ep.schedule_next(PollClass::Transient, now),
            Duration::from_secs(10)
        );
        assert_eq!(
            ep.schedule_next(PollClass::Transient, now),
            Duration::from_secs(20)
        );
        ep.schedule_next(PollClass::Success, now);
        assert_eq!(
            ep.schedule_next(PollClass::Transient, now),
            Duration::from_secs(5),
            "a served success resets the streak"
        );
    }

    #[test]
    fn bybit_served_success_only_on_retcode_zero_and_nonempty_list() {
        // A genuine success (retCode 0, non-empty list) yields no served-error.
        assert!(bybit_served_error_detail(
            r#"{"retCode":0,"retMsg":"OK","result":{"symbol":"BTCUSDT","list":[{"openInterest":"1","timestamp":"1739502300000"}]}}"#
        )
        .is_none());
        // retCode 10006 rate-limit, served with an empty list, is a served error
        // whose detail carries the retCode and retMsg.
        let retcode = bybit_served_error_detail(
            r#"{"retCode":10006,"retMsg":"Too many visits!","result":{"list":[]}}"#,
        )
        .expect("a non-zero retCode is a served error");
        assert!(retcode.contains("10006") && retcode.contains("Too many visits!"));
        // retCode 0 but empty list is a served error (funding between settlements).
        assert!(
            bybit_served_error_detail(r#"{"retCode":0,"retMsg":"OK","result":{"list":[]}}"#)
                .is_some()
        );
        // A malformed body is a served error (back off, don't reset the streak).
        assert!(bybit_served_error_detail("not json").is_some());
    }

    /// Ruling E32: every non-served Bybit REST outcome becomes exactly one
    /// schema-4 `Failed` record the adapter replays as a recovery notice. A 451
    /// (a US-host geo refusal) carries its status; a retCode 10006 served with
    /// HTTP 200 carries the retCode and retMsg in its detail.
    #[test]
    fn a_451_and_a_retcode_10006_each_produce_one_failed_record_that_replays() {
        use crate::adapter::FeedAdapter;
        use crate::bybit::BybitRestAdapter;
        use crate::raw::RecordKind;

        fn failed_record(action: &BybitRestAction, ordinal: u64) -> RawRecord {
            match &action.record {
                BybitRestActionRecord::Failed { kind, detail } => RawRecord::failed_at(
                    CaptureStream::BybitLinearRest,
                    1,
                    ordinal,
                    *kind,
                    detail,
                    "2026-09-03T04:00:02.250Z".to_owned(),
                    2_000,
                ),
                BybitRestActionRecord::Body(_) => panic!("expected a Failed action"),
            }
        }

        fn connected() -> RawRecord {
            RawRecord::connected_at(
                CaptureStream::BybitLinearRest,
                1,
                1,
                "2026-09-03T04:00:00.000Z".to_owned(),
                0,
                0,
            )
        }

        // A 451 classifies as a non-2xx Failed record on the transient ladder,
        // carrying the status.
        let non_success = classify_bybit_rest_poll(RestPollOutcome::NonSuccess(451));
        assert_eq!(non_success.class, PollClass::Transient);
        assert!(matches!(
            non_success.record,
            BybitRestActionRecord::Failed {
                kind: FailureKind::HttpStatus(451),
                ..
            }
        ));
        assert!(matches!(non_success.counter, BybitRestCounter::NonSuccess));
        let record = failed_record(&non_success, 2);
        record
            .validate()
            .expect("a 451 Failed record is well formed");
        assert_eq!(record.kind, RecordKind::Failed);
        let mut adapter = BybitRestAdapter::new("bybit-rest-test").expect("adapter builds");
        adapter.ingest(&connected()).expect("connects");
        let outcome = adapter.ingest(&record).expect("a 451 replays");
        assert!(outcome.events.is_empty());
        assert_eq!(outcome.notices.len(), 1);
        assert!(outcome.notices[0].recovery.is_some());
        assert!(outcome.notices[0].detail.contains("451"));

        // A retCode 10006 served with HTTP 200 classifies as a served-error
        // Failed record whose detail carries the retCode and retMsg.
        let served_error = classify_bybit_rest_poll(RestPollOutcome::Served(
            r#"{"retCode":10006,"retMsg":"Too many visits!","result":{"list":[]}}"#.to_owned(),
        ));
        assert_eq!(served_error.class, PollClass::ServedError);
        assert!(matches!(
            served_error.counter,
            BybitRestCounter::ServedError
        ));
        let detail = match &served_error.record {
            BybitRestActionRecord::Failed {
                kind: FailureKind::Served,
                detail,
            } => detail.clone(),
            other => panic!("expected a served-error Failed record, got {other:?}"),
        };
        assert!(detail.contains("10006") && detail.contains("Too many visits!"));
        let record = failed_record(&served_error, 2);
        record
            .validate()
            .expect("a retCode Failed record is well formed");
        let mut adapter = BybitRestAdapter::new("bybit-rest-test").expect("adapter builds");
        adapter.ingest(&connected()).expect("connects");
        let outcome = adapter.ingest(&record).expect("a retCode 10006 replays");
        assert!(outcome.events.is_empty());
        assert_eq!(outcome.notices.len(), 1);
        assert!(outcome.notices[0].recovery.is_some());

        // E39(c) m1: the discriminant tells the shapes apart — a served 2xx
        // error is `served` (never `transport`), a non-2xx is `http_status`, and
        // an unparseable 2xx body keeps a bounded slice of the venue bytes.
        let served_payload: serde_json::Value =
            serde_json::from_str(record.payload.as_deref().expect("payload")).expect("json");
        assert_eq!(served_payload["failure"], serde_json::json!("served"));
        let non_success_record = failed_record(&non_success, 3);
        let non_success_payload: serde_json::Value =
            serde_json::from_str(non_success_record.payload.as_deref().expect("payload"))
                .expect("json");
        assert_eq!(
            non_success_payload["failure"],
            serde_json::json!("http_status")
        );
        let unparseable = classify_bybit_rest_poll(RestPollOutcome::Served(
            r#"{"unexpected":"shape"}"#.to_owned(),
        ));
        let unparseable_detail = match &unparseable.record {
            BybitRestActionRecord::Failed {
                kind: FailureKind::Served,
                detail,
            } => detail.clone(),
            other => panic!("expected a served-error Failed record, got {other:?}"),
        };
        assert!(
            unparseable_detail.contains("unexpected"),
            "the unparseable body's venue bytes are retained: {unparseable_detail}"
        );
    }

    /// Two SportsDataIO recording rules that are easy to get wrong in opposite
    /// directions, pinned as one decision.
    ///
    ///  * a non-2xx body is TRUNCATED, never refused: reading it under the 8 MiB
    ///    payload cap would turn an oversized provider error page into a transport
    ///    failure and count a 401-or-429 answer as a read idle timeout;
    ///  * a 3xx body is not read at all: `Location` is off the header allowlist,
    ///    and ADR 0021 promises the redirect target is never recorded, which a
    ///    verbatim 301 HTML body would break by repeating the target inline.
    #[test]
    fn sportsdataio_body_recording_follows_status() {
        for status in [200, 201, 204, 299] {
            assert_eq!(body_recording(status), BodyRecording::Payload, "{status}");
        }
        for status in [300, 301, 302, 304, 307, 308, 399] {
            assert_eq!(body_recording(status), BodyRecording::Nothing, "{status}");
        }
        for status in [100, 400, 401, 403, 404, 429, 500, 503] {
            assert_eq!(
                body_recording(status),
                BodyRecording::StatusEvidence,
                "{status}"
            );
        }

        // The envelope a refused redirect produces carries zero body bytes.
        let redirect = crate::sportsdataio_nba::response_envelope(
            crate::sportsdataio_nba::Operation::Teams,
            1,
            0,
            "2026-09-01T00:00:00.000Z",
            302,
            crate::sportsdataio_nba::ResponseOutcome::RedirectRefused,
            std::collections::BTreeMap::new(),
            &[],
            true,
            false,
        );
        let parsed: crate::sportsdataio_nba::ResponseEnvelope =
            serde_json::from_str(&redirect).expect("round trip");
        assert_eq!(parsed.body_bytes, 0);
        assert_eq!(parsed.body, "");
        assert_eq!(parsed.outcome, "redirect_refused");
        assert!(parsed.body_truncated);
    }

    /// The per-response size ceilings, driven directly with synthetic bytes.
    ///
    /// ADR 0021 promises two DIFFERENT over-cap behaviours on the same path, and
    /// neither is reachable from an HTTPS-only client in a test: an over-cap 2xx
    /// payload is REFUSED by `secure_http::bounded_body_with_status` (nothing is
    /// recorded and the request counts as a transport failure), while an over-cap
    /// non-2xx error page is TRUNCATED at the small error cap and still recorded.
    /// `body_recording` is the switch between them and `append_bounded` is the
    /// truncating reader's whole boundary, so both are pinned here without a
    /// socket. The truncation flag matters twice over: it is also what makes the
    /// credential redactor drop its tail guard, so a bisected key cannot leave a
    /// prefix in the journal.
    #[test]
    fn sportsdataio_over_cap_bodies_truncate_or_refuse_by_status() {
        let cap = crate::sportsdataio_nba::MAX_ERROR_BODY_BYTES;

        // An oversized provider error page, delivered as several chunks. The read
        // stops at the cap and reports the cut.
        let mut body = Vec::new();
        let mut truncated = false;
        let chunk = vec![b'E'; 1_500];
        for _ in 0..8 {
            if append_bounded(&mut body, &chunk, cap) {
                truncated = true;
                break;
            }
        }
        assert!(truncated);
        assert_eq!(body.len(), cap);

        // A single chunk larger than the whole cap is cut at the cap, not dropped.
        let mut body = Vec::new();
        assert!(append_bounded(&mut body, &vec![b'E'; cap * 3], cap));
        assert_eq!(body.len(), cap);

        // An error page that fits is recorded whole and reports no cut.
        let mut body = Vec::new();
        assert!(!append_bounded(&mut body, b"unauthorized", cap));
        assert_eq!(body, b"unauthorized");
        assert!(!append_bounded(&mut body, b"", cap));
        assert_eq!(body.len(), "unauthorized".len());

        // A truncated body drops a tail guard, so a bisected occurrence cannot
        // leave a prefix of the credential behind.
        let credential =
            crate::SportsDataIoCredential::from_value("SENTINEL-KEY-DO-NOT-LEAK-7f3a".to_owned())
                .expect("the sentinel is a well-formed credential value");
        let mut body = Vec::new();
        let bisected = format!("{}{}", "x".repeat(cap - 8), "SENTINEL-KEY-DO-NOT-LEAK-7f3a");
        assert!(append_bounded(&mut body, bisected.as_bytes(), cap));
        let (recorded, replaced) = credential.redact_body(&body, true);
        assert!(!replaced);
        assert!(!String::from_utf8_lossy(&recorded).contains("SENTINEL"));

        // And the status switch that decides which reader sees the response at
        // all: only a 2xx reaches the refusing payload reader under the 8 MiB cap.
        assert_eq!(body_recording(200), BodyRecording::Payload);
        assert_eq!(body_recording(429), BodyRecording::StatusEvidence);
        assert!(crate::sportsdataio_nba::MAX_RESPONSE_BYTES > cap);
    }

    #[test]
    fn text_heartbeat_pairs_every_stream_variant_exactly() {
        // The interval and payload are paired in one function, so structural
        // disagreement is impossible by construction; this pins the contract for
        // every variant. Adding a CaptureStream variant forces a decision here.
        use CaptureStream::{
            BinanceSpot, BinanceUsdmPublic, BinanceUsdmRest, BybitLinear, BybitLinearRest,
            BybitSpot, ChainlinkAggregator, CoinbaseCore, CoinbaseLevel3, CoinbaseMechanisms,
            KalshiIndex, KalshiMarket, KrakenCore, KrakenLevel3, KrakenUsdtReference, NbaEspnDaily,
            NbaEspnGames, NbaEspnStatus, PolymarketChainlinkTwap, PolymarketMarket, PythHermesRest,
            PythHermesStream, SportsDataIoNba,
        };
        for stream in [
            BinanceSpot,
            BinanceUsdmPublic,
            BinanceUsdmRest,
            BybitSpot,
            BybitLinear,
            BybitLinearRest,
            ChainlinkAggregator,
            CoinbaseCore,
            CoinbaseLevel3,
            CoinbaseMechanisms,
            KrakenCore,
            KrakenLevel3,
            KrakenUsdtReference,
            KalshiMarket,
            KalshiIndex,
            NbaEspnDaily,
            NbaEspnGames,
            NbaEspnStatus,
            PolymarketMarket,
            PolymarketChainlinkTwap,
            PythHermesStream,
            PythHermesRest,
            SportsDataIoNba,
        ] {
            let expected = match stream {
                PolymarketMarket => {
                    Some((POLYMARKET_TEXT_HEARTBEAT_INTERVAL, POLYMARKET_PING_PAYLOAD))
                }
                PolymarketChainlinkTwap => Some((
                    POLYMARKET_RTDS_TEXT_HEARTBEAT_INTERVAL,
                    POLYMARKET_PING_PAYLOAD,
                )),
                BybitSpot | BybitLinear => {
                    Some((BYBIT_TEXT_HEARTBEAT_INTERVAL, BYBIT_PING_PAYLOAD))
                }
                // Chainlink and all three NBA ESPN streams run over HTTP and send
                // no WebSocket keepalive.
                // Binance answers a SERVER-initiated ping with the library's
                // automatic pong on the USD-M `public` socket, so there is no
                // client keepalive to pair here; the REST pollers (Binance USD-M
                // and Bybit linear) are HTTP.
                BinanceSpot | BinanceUsdmPublic | BinanceUsdmRest | BybitLinearRest
                | ChainlinkAggregator | NbaEspnDaily | NbaEspnGames | NbaEspnStatus
                | CoinbaseCore | CoinbaseLevel3 | CoinbaseMechanisms | KrakenCore
                | KrakenLevel3 | KrakenUsdtReference | KalshiMarket | KalshiIndex
                | SportsDataIoNba | PythHermesStream | PythHermesRest => None,
            };
            assert_eq!(text_heartbeat(stream), expected, "stream {stream:?}");
        }
        // Pin the concrete values the pairs resolve to.
        assert_eq!(POLYMARKET_TEXT_HEARTBEAT_INTERVAL, Duration::from_secs(10));
        assert_eq!(
            POLYMARKET_RTDS_TEXT_HEARTBEAT_INTERVAL,
            Duration::from_secs(5)
        );
        assert_eq!(BYBIT_TEXT_HEARTBEAT_INTERVAL, Duration::from_secs(20));
        assert_eq!(POLYMARKET_PING_PAYLOAD, "PING");
        assert_eq!(BYBIT_PING_PAYLOAD, r#"{"op":"ping"}"#);
    }

    #[test]
    fn the_binance_capture_streams_refuse_unless_binance_is_listed() {
        // Ruling R7. The gate is applied in `run_capture` before the output
        // file is created (`capture.rs`: `require_enabled_venue` precedes
        // `RawRecorder::create`), so a refused run leaves no zero-record
        // journal behind and records nothing the rights row has not cleared.
        use super::{EnabledVenues, gated_venue_token, require_enabled_venue_with};

        let gated = [
            CaptureStream::BinanceSpot,
            CaptureStream::BinanceUsdmPublic,
            CaptureStream::BinanceUsdmRest,
        ];
        // Unset means the four adopted venues, so every USD-M stream refuses.
        let default = EnabledVenues::parse(None).expect("the unset default parses");
        for stream in gated {
            let token = gated_venue_token(stream).expect("the stream is gated");
            assert_eq!(token, "binance");
            let refusal = require_enabled_venue_with(stream, token, &default)
                .expect_err("an unlisted venue refuses");
            let reason = refusal.to_string();
            assert!(reason.contains(stream.as_str()), "{reason}");
            assert!(reason.contains("RIGHTS-LEDGER"), "{reason}");
        }
        // Another venue's token never substitutes for this one.
        let other = EnabledVenues::parse(Some("bybit,kalshi")).expect("the value parses");
        for stream in gated {
            let token = gated_venue_token(stream).expect("the stream is gated");
            assert!(require_enabled_venue_with(stream, token, &other).is_err());
        }
        // Listed, and every USD-M stream is admitted.
        let listed = EnabledVenues::parse(Some("binance")).expect("the value parses");
        for stream in gated {
            let token = gated_venue_token(stream).expect("the stream is gated");
            assert!(require_enabled_venue_with(stream, token, &listed).is_ok());
        }
        // Every Binance surface takes the gate, spot included: one venue, one
        // rights row, one refusal. No adopted venue's stream is gated.
        for stream in [
            CaptureStream::CoinbaseCore,
            CaptureStream::KrakenCore,
            CaptureStream::KalshiMarket,
            CaptureStream::PolymarketMarket,
        ] {
            assert!(gated_venue_token(stream).is_none(), "{stream:?}");
        }
    }

    #[test]
    fn the_pyth_hermes_capture_streams_refuse_unless_pyth_is_listed() {
        // Ruling R7. The Pyth Hermes streams are authenticated, but the API key
        // is not the rights gate: the venue's rights row is still open, so
        // `SIGNALS_ENABLED_VENUES` must list `pyth` before either stream is
        // admitted, exactly as the source contract advertises. Without the
        // allow-list arm the key alone admits a run and the exit-78 refusal
        // never fires.
        use super::{EnabledVenues, gated_venue_token, require_enabled_venue_with};

        let gated = [
            CaptureStream::PythHermesStream,
            CaptureStream::PythHermesRest,
        ];
        // Unset means the four adopted venues; `pyth` is not among them.
        let default = EnabledVenues::parse(None).expect("the unset default parses");
        for stream in gated {
            let token = gated_venue_token(stream).expect("the stream is gated");
            assert_eq!(token, "pyth");
            let refusal = require_enabled_venue_with(stream, token, &default)
                .expect_err("an unlisted venue refuses");
            let reason = refusal.to_string();
            assert!(reason.contains(stream.as_str()), "{reason}");
            assert!(reason.contains("pyth"), "{reason}");
            assert!(reason.contains("RIGHTS-LEDGER"), "{reason}");
        }
        // Another venue's token never substitutes for this one.
        let other = EnabledVenues::parse(Some("binance,bybit")).expect("the value parses");
        for stream in gated {
            let token = gated_venue_token(stream).expect("the stream is gated");
            assert!(require_enabled_venue_with(stream, token, &other).is_err());
        }
        // Listed, and both Pyth Hermes streams are admitted.
        let listed = EnabledVenues::parse(Some("pyth")).expect("the value parses");
        for stream in gated {
            let token = gated_venue_token(stream).expect("the stream is gated");
            assert!(require_enabled_venue_with(stream, token, &listed).is_ok());
        }
    }

    /// The USD-M `public` connection subscribes through its URL, so the URL and
    /// the stream set the adapter compiles must not be able to drift apart.
    ///
    /// The `/public` path this group first shipped does not exist at Binance:
    /// the upgrade failed and, because nothing logged it, a 65-minute capture
    /// recorded zero bytes and said nothing (ruling E27).
    #[test]
    fn binance_usdm_urls_are_the_combined_form_of_their_streams() {
        assert_eq!(
            endpoint(CaptureStream::BinanceUsdmPublic),
            format!(
                "wss://fstream.binance.com/stream?streams={}",
                crate::binance_usdm::PUBLIC_STREAMS.join("/")
            ),
            "the public socket must dial exactly the streams its adapter compiles"
        );
        // The exact string, so a change to either side is visible in review.
        assert_eq!(
            endpoint(CaptureStream::BinanceUsdmPublic),
            "wss://fstream.binance.com/stream?streams=btcusdt@depth20@100ms"
        );
        // The path that was never real must not come back.
        let url = endpoint(CaptureStream::BinanceUsdmPublic);
        assert!(!url.contains("/public"), "{url}");
        assert!(
            url.starts_with("wss://fstream.binance.com/stream?streams="),
            "{url}"
        );
        // And it sends no SUBSCRIBE frame, because the URL already did.
        assert!(
            subscriptions(
                CaptureStream::BinanceUsdmPublic,
                10,
                None,
                None,
                None,
                ArmedIndexSockets::NONE
            )
            .expect("subscriptions build")
            .is_empty(),
        );
    }

    /// Proactive rotation is armed for exactly the streams with a venue session
    /// limit, and for nothing else.
    #[test]
    fn only_the_binance_usdm_sockets_rotate_and_they_rotate_an_hour_early() {
        use super::{connection_lifetime, rotation_due};

        assert_eq!(
            connection_lifetime(CaptureStream::BinanceUsdmPublic),
            Some(Duration::from_secs(23 * 60 * 60))
        );
        // Binance closes at 24 h; rotating at 23 h keeps the switchover on our
        // schedule with an hour of margin.
        assert_eq!(
            connection_lifetime(CaptureStream::BinanceUsdmPublic)
                .expect("the public socket has a lifetime")
                + Duration::from_secs(60 * 60),
            Duration::from_secs(24 * 60 * 60)
        );
        // The REST poll session is not a socket and every adopted stream is
        // left exactly as it was.
        for stream in [
            CaptureStream::BinanceUsdmRest,
            CaptureStream::BinanceSpot,
            CaptureStream::CoinbaseCore,
            CaptureStream::KrakenCore,
            CaptureStream::KalshiMarket,
            CaptureStream::KalshiIndex,
            CaptureStream::PolymarketMarket,
            CaptureStream::BybitSpot,
        ] {
            assert_eq!(connection_lifetime(stream), None, "{stream:?}");
        }

        // A stream with no lifetime is never due, however long it has been up.
        assert!(!rotation_due(None, Duration::from_secs(10 * 24 * 60 * 60)));
        let lifetime = Some(Duration::from_secs(60));
        assert!(!rotation_due(lifetime, Duration::from_secs(59)));
        // Due AT the lifetime, not only past it.
        assert!(rotation_due(lifetime, Duration::from_secs(60)));
        assert!(rotation_due(lifetime, Duration::from_secs(61)));
    }

    /// What a rotation writes down, and why it is written in that order.
    ///
    /// A capture file is a sequence: it cannot express two sockets open at once,
    /// and the record guard admits one connection epoch at a time. So the
    /// make-before-break switchover — replacement dialled and proven live
    /// first — is recorded as Disconnected on the retiring epoch, then Connected
    /// on the new one, then the very frame that proved the replacement live.
    /// This test drives that sequence through the real adapter to prove the
    /// order is replayable and that no frame is lost across the changeover.
    #[test]
    fn a_rotation_writes_a_replayable_changeover() {
        use crate::binance_usdm::{BinanceUsdmAdapter, BinanceUsdmConnection};
        use crate::{FeedAdapter, FeedNoticeCode, Provider, RawRecord, RecordKind};

        fn frame(epoch: u64, ordinal: u64, kind: RecordKind, payload: Option<&str>) -> RawRecord {
            RawRecord {
                schema_version: 1,
                provider: Provider::BinanceUsdm,
                stream: CaptureStream::BinanceUsdmPublic,
                connection_epoch: epoch,
                ordinal,
                received_at: "2026-09-04T11:00:00.000Z".to_owned(),
                capture_elapsed_ns: None,
                connect_started_elapsed_ns: None,
                kind,
                payload: payload.map(str::to_owned),
            }
        }
        let depth = |last_update_id: u64, previous_final: u64, millis: i64| {
            format!(
                r#"{{"stream":"btcusdt@depth20@100ms","data":{{"e":"depthUpdate","E":{millis},"T":{},"s":"BTCUSDT","ps":"BTCUSDT","U":{},"u":{last_update_id},"pu":{previous_final},"st":1,"b":[["66120.10","1.0"]],"a":[]}}}}"#,
                millis - 20,
                last_update_id.saturating_sub(4)
            )
        };

        let mut adapter = BinanceUsdmAdapter::new("rotation-test", BinanceUsdmConnection::Public)
            .expect("adapter constructs");
        adapter
            .ingest(&frame(1, 1, RecordKind::Connected, None))
            .expect("the first socket opens");
        let before = adapter
            .ingest(&frame(
                1,
                2,
                RecordKind::InboundText,
                Some(&depth(880_100, 880_096, 1_788_440_000_000)),
            ))
            .expect("the retiring socket delivers");
        assert_eq!(before.events.len(), 1);

        // What the retiring socket delivered while the replacement was being
        // dialled is recorded on the RETIRING epoch, before its Disconnected —
        // that is the gap the drain exists to close.
        let drained = adapter
            .ingest(&frame(
                1,
                3,
                RecordKind::InboundText,
                Some(&depth(880_104, 880_100, 1_788_440_000_500)),
            ))
            .expect("a drained frame replays on the retiring epoch");
        assert_eq!(drained.events.len(), 1);

        // The changeover, in the order the capture writes it.
        let closed = adapter
            .ingest(&frame(1, 4, RecordKind::Disconnected, None))
            .expect("the retiring socket closes");
        assert_eq!(closed.notices[0].code, FeedNoticeCode::Disconnected);
        let opened = adapter
            .ingest(&frame(2, 5, RecordKind::Connected, None))
            .expect("the replacement's epoch opens");
        assert_eq!(opened.notices[0].code, FeedNoticeCode::Connected);
        // The frame that proved the replacement live is recorded on the new
        // epoch rather than discarded to keep the bookkeeping tidy.
        let proof = adapter
            .ingest(&frame(
                2,
                6,
                RecordKind::InboundText,
                Some(&depth(880_108, 880_096, 1_788_440_001_000)),
            ))
            .expect("the proving frame replays on the new epoch");
        assert_eq!(proof.events.len(), 1);
        assert_eq!(proof.events[0].event_time, "2026-09-03T12:53:21.000Z");
        // Sequences are per-source and continuous across the rotation, so a
        // consumer sees one uninterrupted tape.
        assert_eq!(before.events[0].sequence, 0);
        assert_eq!(drained.events[0].sequence, 1);
        assert_eq!(proof.events[0].sequence, 2);
    }

    /// A venue that accepts the socket and then says nothing must not be
    /// re-dialled at the speed of the read-idle timeout.
    ///
    /// This is the observed Binance USD-M socket behaviour when a host cannot
    /// reach it: connect, ten seconds of silence, disconnect, redial — roughly
    /// 360 connection epochs an hour of pure churn, 720 records of it in the
    /// capture file, and no data.
    #[test]
    fn an_idle_venue_cannot_churn_hundreds_of_epochs_an_hour() {
        use super::{
            REDIAL_MAX_BACKOFF, read_idle_timeout, redial_backoff, redial_backoff_stepped,
        };

        // A productive connection costs nothing: the streak resets to zero.
        assert_eq!(redial_backoff(0), Duration::ZERO);
        // One second, doubling, capped at a minute.
        let ladder: Vec<Duration> = (1..=9).map(redial_backoff).collect();
        assert_eq!(
            ladder,
            vec![
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(4),
                Duration::from_secs(8),
                Duration::from_secs(16),
                Duration::from_secs(32),
                REDIAL_MAX_BACKOFF,
                REDIAL_MAX_BACKOFF,
                REDIAL_MAX_BACKOFF,
            ]
        );
        assert_eq!(REDIAL_MAX_BACKOFF, Duration::from_secs(60));

        // Simulate the churn: each epoch costs the read-idle timeout plus the
        // backoff owed before the next dial.
        let idle = read_idle_timeout(CaptureStream::BinanceUsdmPublic);
        assert_eq!(idle, Duration::from_secs(10));
        let mut elapsed = Duration::ZERO;
        let mut epochs = 0_u64;
        let hour = Duration::from_secs(3600);
        while elapsed < hour {
            epochs += 1;
            elapsed += idle + redial_backoff(epochs);
        }
        assert!(
            epochs < 60,
            "an hour of an idle venue must cost tens of epochs, not hundreds; got {epochs}"
        );
        // Without the ladder the same hour is 360 epochs, which is the state
        // this test exists to make impossible.
        assert_eq!(hour.as_secs() / idle.as_secs(), 360);

        // Reporting is per escalation step, so a permanently dead endpoint
        // leaves a short readable ladder rather than a line every second.
        let steps = (1..=200)
            .filter(|streak| redial_backoff_stepped(*streak))
            .count();
        assert_eq!(steps, 7, "one line per level: 1, 2, 4, 8, 16, 32, 60");
        assert!(
            redial_backoff_stepped(1),
            "the first failure always reports"
        );
        assert!(!redial_backoff_stepped(200));
    }

    /// Rotation is armed by the STREAM, never by the diagnostic override: the
    /// override can only shorten a lifetime a stream already has.
    #[test]
    fn the_rotation_override_is_refused_on_a_stream_with_no_lifetime() {
        use super::{connection_lifetime, rotation_supported};

        // What the capture loop computes. On a stream with a lifetime the
        // override shortens it; on every other stream it stays unarmed.
        let armed = |stream, override_: Option<Duration>| {
            connection_lifetime(stream)
                .map(|lifetime| override_.map_or(lifetime, |shortened| shortened.min(lifetime)))
        };
        let short = Duration::from_secs(5);
        assert_eq!(
            armed(CaptureStream::BinanceUsdmPublic, Some(short)),
            Some(short)
        );
        // It can only SHORTEN. A longer value would push the rotation past the
        // venue's own 24 h close — the drop rotation exists to prevent — so it
        // is taken as a minimum, not a replacement.
        assert_eq!(
            armed(
                CaptureStream::BinanceUsdmPublic,
                Some(Duration::from_secs(172_800))
            ),
            Some(Duration::from_secs(23 * 60 * 60)),
            "a 48-hour override must not outlive the venue's 24-hour close"
        );
        assert_eq!(
            armed(CaptureStream::BinanceUsdmPublic, None),
            Some(Duration::from_secs(23 * 60 * 60))
        );
        for stream in [
            CaptureStream::KalshiMarket,
            CaptureStream::CoinbaseCore,
            CaptureStream::KrakenCore,
            CaptureStream::PolymarketMarket,
            CaptureStream::BinanceSpot,
            CaptureStream::BinanceUsdmRest,
        ] {
            assert_eq!(
                armed(stream, Some(short)),
                None,
                "{stream:?} must not be armed by the override alone"
            );
        }

        // And the run is refused outright rather than silently ignoring the
        // flag: rotation dials a bare endpoint and sends no SUBSCRIBE, so on a
        // stream needing discovery, a credential or a subscribe frame it would
        // trade a live subscribed socket for a silent one.
        let options = |stream| CaptureOptions {
            stream,
            output: PathBuf::from("/dev/null"),
            duration: Duration::from_secs(1),
            max_records: 1,
            max_bytes: 1,
            kraken_btc_book_depth: 10,
            polymarket_slug: None,
            kalshi_market_ticker: None,
            sportsdataio_control_file: None,
            connection_lifetime_override: Some(short),
        };
        for stream in [CaptureStream::KalshiMarket, CaptureStream::CoinbaseCore] {
            let refusal = rotation_supported(&options(stream))
                .expect_err("a stream with no lifetime refuses the override");
            assert!(
                matches!(refusal, CaptureError::RotationNotSupported { .. }),
                "{stream:?}: {refusal}"
            );
            assert!(refusal.to_string().contains(stream.as_str()));
        }
        assert!(rotation_supported(&options(CaptureStream::BinanceUsdmPublic)).is_ok());
        // No override, no refusal, whatever the stream.
        let mut unset = options(CaptureStream::KalshiMarket);
        unset.connection_lifetime_override = None;
        assert!(rotation_supported(&unset).is_ok());
    }

    #[test]
    fn read_idle_exceeded_fires_only_at_or_past_the_timeout() {
        let timeout = Duration::from_secs(30);
        assert!(!read_idle_exceeded(Duration::from_secs(29), timeout));
        assert!(read_idle_exceeded(timeout, timeout));
        assert!(read_idle_exceeded(Duration::from_secs(31), timeout));
    }

    #[test]
    fn endpoints_and_subscriptions_are_compiled_not_user_supplied() {
        assert_eq!(endpoint(CaptureStream::CoinbaseCore), COINBASE_URL);
        assert_eq!(endpoint(CaptureStream::KrakenCore), KRAKEN_URL);
        assert_eq!(endpoint(CaptureStream::KrakenLevel3), KRAKEN_LEVEL3_URL);
        assert_eq!(endpoint(CaptureStream::BinanceSpot), BINANCE_URL);
        assert_eq!(endpoint(CaptureStream::BybitSpot), BYBIT_SPOT_URL);
        assert_eq!(endpoint(CaptureStream::BybitLinear), BYBIT_LINEAR_URL);
        assert_eq!(
            endpoint(CaptureStream::ChainlinkAggregator),
            crate::chainlink::primary_rpc_endpoint()
        );

        // Chainlink has no WebSocket subscription; the poll loop issues its own
        // eth_call requests.
        let chainlink = subscriptions(
            CaptureStream::ChainlinkAggregator,
            100,
            None,
            None,
            None,
            ArmedIndexSockets::NONE,
        )
        .expect("chainlink subscription list serializes");
        assert!(chainlink.is_empty());

        // Bybit spot subscribes to exactly the three spot topics named by the
        // adapter's own constants; linear subscribes to the liquidation topic.
        let bybit_spot = subscriptions(
            CaptureStream::BybitSpot,
            100,
            None,
            None,
            None,
            ArmedIndexSockets::NONE,
        )
        .expect("bybit spot subscription serializes");
        assert_eq!(bybit_spot.len(), 1);
        let spot_value: serde_json::Value =
            serde_json::from_str(&bybit_spot[0].recorded).expect("subscription is valid JSON");
        assert_eq!(
            spot_value.pointer("/op").and_then(|v| v.as_str()),
            Some("subscribe")
        );
        assert_eq!(
            spot_value.pointer("/args/0").and_then(|v| v.as_str()),
            Some(crate::bybit::PUBLIC_TRADE_TOPIC)
        );
        assert_eq!(
            spot_value.pointer("/args/1").and_then(|v| v.as_str()),
            Some(crate::bybit::ORDERBOOK_200_TOPIC)
        );
        assert_eq!(
            spot_value.pointer("/args/2").and_then(|v| v.as_str()),
            Some(crate::bybit::ORDERBOOK_1_TOPIC)
        );

        let bybit_linear = subscriptions(
            CaptureStream::BybitLinear,
            100,
            None,
            None,
            None,
            ArmedIndexSockets::NONE,
        )
        .expect("bybit linear subscription serializes");
        assert_eq!(bybit_linear.len(), 1);
        let linear_value: serde_json::Value =
            serde_json::from_str(&bybit_linear[0].recorded).expect("subscription is valid JSON");
        assert_eq!(
            linear_value.pointer("/args/0").and_then(|v| v.as_str()),
            Some(crate::bybit::ALL_LIQUIDATION_TOPIC)
        );
        assert_eq!(
            linear_value.pointer("/args/1").and_then(|v| v.as_str()),
            Some(crate::bybit::TICKERS_TOPIC)
        );
        assert_eq!(
            linear_value.pointer("/args/2").and_then(|v| v.as_str()),
            Some(crate::bybit::LINEAR_ORDERBOOK_50_TOPIC)
        );
        assert_eq!(
            linear_value.pointer("/args/3").and_then(|v| v.as_str()),
            Some(crate::bybit::PUBLIC_TRADE_TOPIC)
        );
        assert_eq!(
            linear_value.pointer("/args/4").and_then(|v| v.as_str()),
            None
        );

        // Binance subscribes to exactly the two combined spot streams named by
        // the adapter's own stream constants (single source of truth).
        let binance = subscriptions(
            CaptureStream::BinanceSpot,
            100,
            None,
            None,
            None,
            ArmedIndexSockets::NONE,
        )
        .expect("binance subscription serializes");
        assert_eq!(binance.len(), 1);
        let value: serde_json::Value =
            serde_json::from_str(&binance[0].recorded).expect("subscription is valid JSON");
        assert_eq!(
            value.pointer("/method").and_then(|v| v.as_str()),
            Some("SUBSCRIBE")
        );
        assert_eq!(
            value.pointer("/params/0").and_then(|v| v.as_str()),
            Some(crate::binance::DEPTH_STREAM)
        );
        assert_eq!(
            value.pointer("/params/1").and_then(|v| v.as_str()),
            Some(crate::binance::AGG_TRADE_STREAM)
        );

        let options = CaptureOptions {
            stream: CaptureStream::KrakenCore,
            output: PathBuf::from("unused"),
            duration: Duration::from_secs(1),
            max_records: 1,
            max_bytes: 1024,
            kraken_btc_book_depth: 100,
            polymarket_slug: None,
            kalshi_market_ticker: None,
            sportsdataio_control_file: None,
            connection_lifetime_override: None,
        };
        let messages = subscriptions(
            options.stream,
            options.kraken_btc_book_depth,
            None,
            None,
            None,
            ArmedIndexSockets::NONE,
        )
        .expect("static subscriptions serialize");
        assert_eq!(messages.len(), 6);
        assert!(messages.iter().all(|message| {
            let value: serde_json::Value =
                serde_json::from_str(&message.recorded).expect("subscription is valid JSON");
            value.pointer("/params/token").is_none()
        }));
    }

    #[test]
    fn authenticated_subscription_is_redacted_before_capture() {
        let options = CaptureOptions {
            stream: CaptureStream::KrakenLevel3,
            output: PathBuf::from("unused"),
            duration: Duration::from_secs(1),
            max_records: 1,
            max_bytes: 1024,
            kraken_btc_book_depth: 100,
            polymarket_slug: None,
            kalshi_market_ticker: None,
            sportsdataio_control_file: None,
            connection_lifetime_override: None,
        };
        let sentinel = "secret-token-that-must-never-be-recorded";
        let token = KrakenWebSocketToken::test_token(sentinel);
        let messages = subscriptions(
            options.stream,
            options.kraken_btc_book_depth,
            Some(&token),
            None,
            None,
            ArmedIndexSockets::NONE,
        )
        .expect("subscription serializes");
        assert_eq!(messages.len(), 1);
        assert!(messages[0].wire.contains(sentinel));
        assert!(!messages[0].recorded.contains(sentinel));
        assert!(messages[0].recorded.contains("[REDACTED]"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn error_exit_finalizes_buffered_capture_data() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "btc-feeds-capture-error-finalize-{}-{unique}.jsonl",
            std::process::id()
        ));
        let record = RawRecord::connected_at(
            CaptureStream::CoinbaseCore,
            1,
            1,
            "2026-08-13T12:00:00Z".to_owned(),
            2,
            1,
        );
        let mut expected = serde_json::to_vec(&record).expect("record serializes");
        expected.push(b'\n');
        let mut recorder = crate::raw::RawRecorder::create(&path, 1024 * 1024)
            .await
            .expect("capture file is created");
        recorder.append(&record).await.expect("record is buffered");

        let error = finalize_capture(recorder, Err(CaptureError::CounterExhausted))
            .await
            .expect_err("original capture failure is retained");
        assert!(matches!(error, CaptureError::CounterExhausted));
        assert_eq!(std::fs::read(&path).expect("capture is readable"), expected);
        std::fs::remove_file(path).expect("test capture is removed");
    }

    #[test]
    fn daily_discovery_reads_both_id_shapes_and_orders_them() {
        // The teams list nests each id one level deep (`teams[].team.id`); the
        // roster carries it directly (`athletes[].id`). Both are strings of
        // digits here and integers elsewhere in ESPN's surface, so both are read.
        // The result is ORDERED so the crawl issues requests in a stable sequence
        // regardless of the order ESPN happens to return.
        let teams_list = r#"{"sports":[{"leagues":[{"teams":[
            {"team":{"id":"30"}},{"team":{"id":"2"}},{"team":{"id":17}}]}]}]}"#;
        let mut teams = BTreeSet::new();
        nba_espn_daily_collect_ids(teams_list, "teams", &mut teams, 40);
        assert_eq!(teams.into_iter().collect::<Vec<_>>(), vec![2, 17, 30]);

        let roster = r#"{"team":{"id":"2"},"athletes":[{"id":"5142621"},{"id":"1966"}]}"#;
        let mut athletes = BTreeSet::new();
        nba_espn_daily_collect_ids(roster, "athletes", &mut athletes, 800);
        assert_eq!(
            athletes.into_iter().collect::<Vec<_>>(),
            vec![1966, 5142621]
        );
    }

    #[test]
    fn daily_discovery_is_bounded_and_never_fails_on_an_odd_body() {
        // The bound is the fail-closed guard that stops a malformed discovery
        // response from driving an unbounded crawl.
        let entries: String = (0..50)
            .map(|id| format!(r#"{{"team":{{"id":"{id}"}}}}"#))
            .collect::<Vec<_>>()
            .join(",");
        let body = format!(r#"{{"sports":[{{"leagues":[{{"teams":[{entries}]}}]}}]}}"#);
        let mut teams = BTreeSet::new();
        nba_espn_daily_collect_ids(&body, "teams", &mut teams, 40);
        assert_eq!(teams.len(), 40);

        // Discovery is best-effort: an unparseable or wrong-shaped body yields
        // nothing rather than aborting the capture.
        for body in ["<html>502</html>", "{}", r#"{"teams":"not an array"}"#] {
            let mut empty = BTreeSet::new();
            nba_espn_daily_collect_ids(body, "teams", &mut empty, 40);
            assert!(empty.is_empty(), "{body}");
        }
    }

    #[test]
    fn daily_crawl_uses_the_tls12_permitted_client_and_leaves_the_default_alone() {
        // Two of ESPN's three crawl hosts cap at TLS 1.2. The exception is a
        // SEPARATE constructor so the shared 1.3-only default that every other
        // provider (including slice 1's ESPN status poll) uses stays intact.
        secure_http::client().expect("the TLS 1.3-only default still builds");
        secure_http::tls12_compatible_client().expect("the TLS 1.2-permitted crawl client builds");
    }

    #[test]
    fn the_game_day_cycle_reads_todays_date_from_the_capture_clock() {
        // Reading the wall clock is a capture-side act; the date is baked into
        // the scoreboard URL and therefore into the recorded descriptor, so
        // replay stays clock-free. 1,775,779,200 s = 2026-04-10T00:00:00Z.
        let instant = std::time::UNIX_EPOCH + Duration::from_secs(1_775_779_200);
        assert_eq!(utc_date_key(instant), "20260410");
        // The date-key boundary is UTC, and a late-evening UTC instant on the
        // same day still keys that day.
        assert_eq!(
            utc_date_key(instant + Duration::from_secs(23 * 3_600 + 3_599)),
            "20260410"
        );
        assert_eq!(
            utc_date_key(instant + Duration::from_secs(24 * 3_600)),
            "20260411"
        );
        assert_eq!(
            crate::nba_espn_games::render(
                crate::nba_espn_games::SCOREBOARD_TEMPLATE,
                &utc_date_key(instant),
                ""
            ),
            "https://site.api.espn.com/apis/site/v2/sports/basketball/nba/scoreboard?dates=20260410"
        );
    }

    #[test]
    fn the_game_day_cycle_discovers_event_ids_from_the_scoreboard() {
        // The cycle's request list comes from the scoreboard's top-level
        // `events[]`, read through the same bounded, ordered discovery walk the
        // sibling crawl uses. Ordering keeps the journal byte-stable and the
        // bound keeps a malformed slate from driving an unbounded cycle.
        let scoreboard = r#"{"leagues":[{"calendar":["2026-04-10"]}],"events":[
            {"id":"401811026","competitions":[{"id":"401811026"}]},
            {"id":"401811020","competitions":[{"id":"401811020"}]}]}"#;
        let mut events = BTreeSet::new();
        nba_espn_daily_collect_ids(
            scoreboard,
            "events",
            &mut events,
            crate::nba_espn_games::MAX_EVENTS_PER_CYCLE,
        );
        assert_eq!(
            events.into_iter().collect::<Vec<_>>(),
            vec![401_811_020, 401_811_026]
        );

        let entries: String = (0..50)
            .map(|id| format!(r#"{{"id":"{id}"}}"#))
            .collect::<Vec<_>>()
            .join(",");
        let mut bounded = BTreeSet::new();
        nba_espn_daily_collect_ids(
            &format!(r#"{{"events":[{entries}]}}"#),
            "events",
            &mut bounded,
            crate::nba_espn_games::MAX_EVENTS_PER_CYCLE,
        );
        assert_eq!(bounded.len(), crate::nba_espn_games::MAX_EVENTS_PER_CYCLE);
    }

    #[test]
    fn the_game_day_cycle_fetches_summaries_hourly_and_routes_per_host() {
        // The summary predicate the poll loop applies, pinned: cycle 0 (the first
        // cycle) carries summaries, cycles 1-5 do not, cycle 6 does again.
        let modulus = crate::nba_espn_games::SUMMARY_CYCLE_MODULUS;
        let carries_summary: Vec<bool> = (0u64..13)
            .map(|cycle| cycle.is_multiple_of(modulus))
            .collect();
        assert_eq!(
            carries_summary,
            vec![
                true, false, false, false, false, false, true, false, false, false, false, false,
                true
            ]
        );
        // At the 600 s cadence that is one summary sweep per hour.
        assert_eq!(
            crate::nba_espn_games::POLL_CADENCE * u32::try_from(modulus).expect("small modulus"),
            Duration::from_secs(3_600)
        );

        // Host routing: the two core-API requests take the TLS-1.2-permitted
        // client, the two site-API requests take the shared 1.3-only default.
        // Both clients still build, and neither default is widened.
        secure_http::client().expect("the TLS 1.3-only default still builds");
        secure_http::tls12_compatible_client().expect("the TLS 1.2-permitted client builds");
        for template in [
            crate::nba_espn_games::ODDS_TEMPLATE,
            crate::nba_espn_games::PREDICTOR_TEMPLATE,
        ] {
            assert!(crate::nba_espn_games::is_core_host(
                &crate::nba_espn_games::render(template, "", "401811026")
            ));
        }
        assert!(!crate::nba_espn_games::is_core_host(
            &crate::nba_espn_games::render(
                crate::nba_espn_games::SUMMARY_TEMPLATE,
                "",
                "401811026"
            )
        ));
    }

    #[test]
    fn binance_usdm_ip_pause_floor_stays_five_seconds_with_the_premium_index_endpoint() {
        // Ruling H1: the IP-wide 429/418 pause floor is
        // `max(IP_PAUSE_FLOOR, fastest_cadence)`. The premium-index endpoint polls
        // at 1 s, but it must never lower the floor below 5 s, or an honoured
        // `Retry-After: 0` would resume in 1 s against an endpoint that just 429'd.
        let fastest = super::BinanceUsdmRestSession::ENDPOINTS
            .into_iter()
            .map(crate::binance_usdm_rest::BinanceUsdmRestEndpoint::cadence)
            .min()
            .expect("the four-endpoint set is non-empty");
        assert_eq!(
            fastest,
            Duration::from_secs(1),
            "premium-index is the fastest endpoint at 1 s"
        );
        let pause_floor = super::IP_PAUSE_FLOOR.max(fastest);
        assert_eq!(pause_floor, Duration::from_secs(5));
        // Fed to the backoff, an honoured `Retry-After: 0` pauses at the 5 s floor.
        let mut backoff = crate::binance_usdm_rest::RateLimitBackoff::new();
        assert_eq!(
            backoff.next_delay(Some("0"), super::refusal_clock(), pause_floor),
            Duration::from_secs(5)
        );
    }
}
