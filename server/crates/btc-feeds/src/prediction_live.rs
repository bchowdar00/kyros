//! Live prediction-market runner: discover, bind, ride, and roll one venue.
//!
//! One process owns one venue. Each cycle rediscovers the currently open BTC
//! up/down market (Polymarket 5-minute slugs, Kalshi `KXBTC15M`), journals every
//! bounded discovery attempt through the admission callback *before* a selection
//! can affect signal state, binds the winner, streams its market socket until the
//! market closes, then rolls to the successor. Auto-roll builds a fresh adapter
//! per market, so adapter-local sequences are rebased onto one process-lifetime
//! source sequence to keep journal lineage and downstream deduplication intact.
//!
//! Socket-connectivity transitions are admitted only for a genuine mid-market
//! drop; every controlled close (scheduled rollover, shutdown, deadline, induced
//! diagnostic teardown) stays silent so it cannot register as feed sickness
//! (ADR 0012, T012b).

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use chrono::{DateTime, Utc};
use futures_util::{SinkExt, StreamExt};
use reqwest::header::{ACCEPT, CACHE_CONTROL};
use serde::{Deserialize, Serialize};
use signals_core::{ArmedIndexSockets, MarketTransitionReason, PredictionMarketVenue, SourceEvent};
use tokio::time::{Instant, sleep, sleep_until, timeout};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_tungstenite::{Connector, connect_async_tls_with_config};

use crate::capture::{
    CONNECT_TIMEOUT, CaptureError, endpoint, install_crypto_provider, read_idle_timeout,
    reconnect_delay, subscriptions, websocket_config,
};
use crate::counters::checked_add;
use crate::kalshi::{
    KALSHI_MARKETS_URL, KalshiDiscovery,
    MAX_DISCOVERY_RESPONSE_BYTES as MAX_KALSHI_DISCOVERY_RESPONSE_BYTES,
    discovery_from_response as kalshi_discovery_from_response,
};
use crate::kalshi_auth::{KALSHI_WEBSOCKET_HOST, KalshiAuthClient};
use crate::kalshi_index::INDEX_WEBSOCKET_PATH;
use crate::live::{LiveError, LiveShutdown, LiveShutdownReason, ProcessClock, ShutdownWait};
use crate::polymarket::{
    GAMMA_MARKETS_URL, MAX_GAMMA_RESPONSE_BYTES, PolymarketDiscovery,
    discovery_from_response as polymarket_discovery_from_response,
};
use crate::replay::{KrakenBookDepths, ReplayAdapter};
use crate::secure_http::{self, sha256_hex};
use crate::{CaptureStream, FeedAdapter, IngestOutcome, RawRecord, RecordKind, RecoveryAction};

const MAX_RUN_DURATION: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const DEFAULT_DISCOVERY_RETRY: Duration = Duration::from_secs(2);
const MAX_CONNECTION_ATTEMPTS: u64 = 100_000;
const KALSHI_SERIES: &str = "KXBTC15M";
const MAX_DISCOVERY_ERROR_EVIDENCE_BYTES: usize = 64 * 1024;

/// Which request of one discovery cycle produced an attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PredictionDiscoveryStage {
    /// The listing query for the market whose trading window contains now.
    CurrentMarket,
    /// The follow-up listing query for the next unopened market, issued only when
    /// no current market is listed.
    NextMarket,
    /// The single-market query whose body becomes the bound market's journaled
    /// configuration payload.
    ExactMarket,
}

/// The outcome a discovery attempt is journaled with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PredictionDiscoveryResult {
    /// The attempt yielded the one market this cycle binds.
    Selected,
    /// The venue answered successfully but listed no market for the requested window.
    Unlisted,
    /// The venue listed only a future market, so now falls in a close-to-open gap;
    /// that market's open time becomes the retry target, capped by `discovery_retry`.
    Gap,
    /// No HTTP response was reached at all (client build or request transport failure).
    Unreachable,
    /// A response was reached but was unparseable, ambiguous, non-2xx, or described a
    /// market that has already closed.
    Invalid,
}

/// How the retained discovery response body is represented in the record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PredictionDiscoveryBodyEncoding {
    /// No body was retained; the body field must be empty.
    None,
    /// The body is retained verbatim as UTF-8 text.
    Utf8,
    /// The body was not valid UTF-8 and is retained as standard base64.
    Base64,
}

/// One bounded, self-contained discovery observation admitted before selection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PredictionDiscoveryAttempt {
    /// Record layout version; validation rejects any value other than 1.
    pub schema_version: u16,
    /// The prediction venue the request was issued to.
    pub venue: PredictionMarketVenue,
    /// Which request of the cycle this attempt records.
    pub stage: PredictionDiscoveryStage,
    /// Request path with query string; must start with `/` and be at most 2,048 bytes.
    pub request_path: String,
    /// Response receipt as an RFC 3339 timestamp off the frozen process wall origin.
    pub retrieved_at: String,
    /// HTTP status when a response was reached, 100..=599; `None` when unreachable.
    pub http_status: Option<u16>,
    /// How the body below is represented: absent, UTF-8 text, or base64.
    pub response_body_encoding: PredictionDiscoveryBodyEncoding,
    /// Lowercase hex SHA-256 of the decoded body bytes; validation re-checks it.
    pub response_body_sha256: String,
    /// The retained body in the stated encoding. Invalid-result evidence is truncated
    /// to 64 KiB of decoded bytes and rehashed before admission.
    pub response_body: String,
    /// What this attempt concluded; validation ties it to the evidence carried.
    pub result: PredictionDiscoveryResult,
    /// Bound market identity, 1 to 256 bytes; present only for a selected result.
    pub selected_market_id: Option<String>,
}

fn bind_reason(previous_market_id: Option<&str>, recovery_pending: bool) -> MarketTransitionReason {
    if previous_market_id.is_none() {
        MarketTransitionReason::InitialBind
    } else if recovery_pending {
        MarketTransitionReason::RecoveryBind
    } else {
        MarketTransitionReason::ScheduledRollover
    }
}

fn is_departing_market_rediscovery(
    reason: MarketTransitionReason,
    previous_market_id: Option<&str>,
    discovered_market_id: &str,
) -> bool {
    reason == MarketTransitionReason::ScheduledRollover
        && previous_market_id == Some(discovered_market_id)
}

fn counts_completed_rollover(reason: MarketTransitionReason) -> bool {
    reason == MarketTransitionReason::ScheduledRollover
}

/// Reserve the single lifecycle departure that changes a bound venue into its
/// already-unavailable discovery state. Discovery attempts continue to be
/// journaled while unbound, but they must not emit a second `Unavailable`: the
/// prediction journal correctly rejects a departure without an active market.
fn begin_unavailability_if_bound(
    market_bound: &mut bool,
    last_failure_reason: &mut Option<MarketTransitionReason>,
    reason: MarketTransitionReason,
) -> bool {
    *last_failure_reason = Some(reason);
    if !*market_bound {
        return false;
    }
    *market_bound = false;
    true
}

/// Rebase the market-local adapter counters onto one process-lifetime source
/// sequence. Auto-roll constructs a fresh adapter for each discovered market,
/// but `(source_id, publisher_epoch, sequence)` must remain unique across those
/// adapters for journal lineage and downstream deduplication.
#[derive(Debug, Default)]
struct ContinuousSourceSequencer {
    next_by_source: BTreeMap<String, u64>,
}

impl ContinuousSourceSequencer {
    fn assign(&mut self, event: &mut SourceEvent) -> Result<(), PredictionLiveError> {
        let next = self
            .next_by_source
            .entry(event.source_id.clone())
            .or_insert(0);
        event.sequence = *next;
        *next = next.checked_add(1).ok_or(PredictionLiveError::Counter)?;
        Ok(())
    }

    fn assign_all(&mut self, events: &mut [SourceEvent]) -> Result<(), PredictionLiveError> {
        for event in events {
            self.assign(event)?;
        }
        Ok(())
    }
}

impl PredictionDiscoveryAttempt {
    /// Reject a discovery record that is not self-consistent: schema version, request
    /// path shape, RFC 3339 retrieval time, body size, encoding, SHA-256 agreement,
    /// HTTP status range, and the result-to-evidence and result-to-identity pairings.
    pub fn validate(&self) -> Result<(), PredictionLiveError> {
        if self.schema_version != 1 {
            return Err(PredictionLiveError::InvalidDiscovery(
                "discovery record schema_version must be 1".to_owned(),
            ));
        }
        if !self.request_path.starts_with('/') || self.request_path.len() > 2_048 {
            return Err(PredictionLiveError::InvalidDiscovery(
                "discovery request path is empty or too long".to_owned(),
            ));
        }
        DateTime::parse_from_rfc3339(&self.retrieved_at).map_err(|error| {
            PredictionLiveError::InvalidDiscovery(format!(
                "discovery retrieval time is invalid: {error}"
            ))
        })?;
        if self.response_body.len() > MAX_KALSHI_DISCOVERY_RESPONSE_BYTES * 2 {
            return Err(PredictionLiveError::InvalidDiscovery(
                "discovery response body representation is oversized".to_owned(),
            ));
        }
        let decoded = match self.response_body_encoding {
            PredictionDiscoveryBodyEncoding::None if self.response_body.is_empty() => Vec::new(),
            PredictionDiscoveryBodyEncoding::Utf8 => self.response_body.as_bytes().to_vec(),
            PredictionDiscoveryBodyEncoding::Base64 => BASE64_STANDARD
                .decode(self.response_body.as_bytes())
                .map_err(|_| {
                    PredictionLiveError::InvalidDiscovery(
                        "discovery response body is invalid base64".to_owned(),
                    )
                })?,
            PredictionDiscoveryBodyEncoding::None => {
                return Err(PredictionLiveError::InvalidDiscovery(
                    "none-encoded discovery body must be empty".to_owned(),
                ));
            }
        };
        if decoded.len() > MAX_KALSHI_DISCOVERY_RESPONSE_BYTES
            || sha256_hex(&decoded) != self.response_body_sha256
        {
            return Err(PredictionLiveError::InvalidDiscovery(
                "discovery response body is oversized or its SHA-256 disagrees".to_owned(),
            ));
        }
        if self
            .http_status
            .is_some_and(|status| !(100..=599).contains(&status))
        {
            return Err(PredictionLiveError::InvalidDiscovery(
                "discovery HTTP status is outside the valid range".to_owned(),
            ));
        }
        let successful_utf8_response = self
            .http_status
            .is_some_and(|status| (200..300).contains(&status))
            && self.response_body_encoding == PredictionDiscoveryBodyEncoding::Utf8;
        match self.result {
            PredictionDiscoveryResult::Selected
            | PredictionDiscoveryResult::Unlisted
            | PredictionDiscoveryResult::Gap
                if !successful_utf8_response =>
            {
                return Err(PredictionLiveError::InvalidDiscovery(
                    "selected, unlisted, and gap results require a successful UTF-8 response"
                        .to_owned(),
                ));
            }
            PredictionDiscoveryResult::Unreachable
                if self.http_status.is_some()
                    || self.response_body_encoding != PredictionDiscoveryBodyEncoding::None =>
            {
                return Err(PredictionLiveError::InvalidDiscovery(
                    "unreachable discovery cannot claim an HTTP response".to_owned(),
                ));
            }
            PredictionDiscoveryResult::Invalid if self.http_status.is_none() => {
                return Err(PredictionLiveError::InvalidDiscovery(
                    "invalid discovery requires a reached HTTP response".to_owned(),
                ));
            }
            _ => {}
        }
        match (self.result, self.selected_market_id.as_deref()) {
            (PredictionDiscoveryResult::Selected, Some(id))
                if !id.is_empty()
                    && id.len() <= 256
                    && self
                        .http_status
                        .is_some_and(|status| (200..300).contains(&status))
                    && self.response_body_encoding == PredictionDiscoveryBodyEncoding::Utf8 => {}
            (PredictionDiscoveryResult::Selected, _) => {
                return Err(PredictionLiveError::InvalidDiscovery(
                    "selected discovery record requires one bounded market identity".to_owned(),
                ));
            }
            (_, Some(_)) => {
                return Err(PredictionLiveError::InvalidDiscovery(
                    "non-selected discovery record cannot carry a selected identity".to_owned(),
                ));
            }
            (_, None) => {}
        }
        Ok(())
    }

    fn truncate_error_evidence(&mut self) -> Result<(), PredictionLiveError> {
        if self.result != PredictionDiscoveryResult::Invalid {
            return Ok(());
        }
        let mut retained = match self.response_body_encoding {
            PredictionDiscoveryBodyEncoding::Utf8 => self.response_body.as_bytes().to_vec(),
            PredictionDiscoveryBodyEncoding::Base64 => BASE64_STANDARD
                .decode(self.response_body.as_bytes())
                .map_err(|_| {
                    PredictionLiveError::InvalidDiscovery(
                        "discovery error evidence is invalid base64".to_owned(),
                    )
                })?,
            PredictionDiscoveryBodyEncoding::None => Vec::new(),
        };
        if retained.len() <= MAX_DISCOVERY_ERROR_EVIDENCE_BYTES {
            return Ok(());
        }
        retained.truncate(MAX_DISCOVERY_ERROR_EVIDENCE_BYTES);
        match self.response_body_encoding {
            PredictionDiscoveryBodyEncoding::Utf8 => {
                while std::str::from_utf8(&retained).is_err() {
                    retained.pop();
                }
                self.response_body = String::from_utf8(retained.clone()).map_err(|_| {
                    PredictionLiveError::InvalidDiscovery(
                        "truncated discovery error evidence is not UTF-8".to_owned(),
                    )
                })?;
            }
            PredictionDiscoveryBodyEncoding::Base64 => {
                self.response_body = BASE64_STANDARD.encode(&retained);
            }
            PredictionDiscoveryBodyEncoding::None => {}
        }
        self.response_body_sha256 = sha256_hex(&retained);
        Ok(())
    }
}

/// A socket-level connectivity transition observed by the live runner, mirroring
/// what the adapter's `Connected`/`Disconnected` notices carry plus the runner's
/// own reconnect attempt. It is deliberately minimal and homogeneous: the venue
/// is implicit (one venue per process) and the transition alone drives the
/// observe-only feed-health board. A scheduled market rollover is a healthy
/// handoff and never produces one of these, which is what keeps a rollover from
/// registering as socket disconnect-sickness (ADR 0012, T012b).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PredictionSocketTransition {
    /// The market socket was lost mid-market (a genuine drop, not a controlled
    /// rollover or shutdown close).
    Disconnected,
    /// The runner has begun a reconnect attempt for the same market.
    Reconnecting,
    /// A reconnect established a fresh market socket.
    Connected,
}

/// One value crossing the live runner's admission boundary, offered to the
/// callback in observation order. Kalshi signed request headers are consumed by
/// the WebSocket client and never appear in any admission value.
#[derive(Debug, Clone)]
pub enum PredictionLiveAdmission {
    /// One retained discovery attempt, admitted before the selection it records can
    /// affect signal state.
    Discovery {
        /// Market generation this cycle would bind, monotonic across the process.
        generation: u64,
        /// The validated, evidence-truncated discovery observation.
        attempt: PredictionDiscoveryAttempt,
        /// Admission instant in nanoseconds since the process logical origin (monotonic).
        now_ns: u64,
    },
    /// A socket-level connectivity transition, admitted so prediction feed-health
    /// sees genuine disconnects and recoveries rather than only lifecycle
    /// unavailability and data-flow age.
    Connectivity {
        /// Which half of a socket drop-and-recover this is.
        transition: PredictionSocketTransition,
        /// Observation instant, nanoseconds since the process logical origin (monotonic).
        now_ns: u64,
    },
    /// A market became active: the configuration frame's contract event, plus why the
    /// runner moved onto this market.
    Bind {
        /// Market generation being bound, monotonic across the process.
        generation: u64,
        /// Identity of the market being replaced; `None` only when nothing preceded it.
        previous_market_id: Option<String>,
        /// Initial bind, recovery bind after a resume, or scheduled rollover.
        reason: MarketTransitionReason,
        /// The single contract event the configuration record produced, resequenced.
        event: signals_core::SourceEvent,
        /// Configuration receipt in nanoseconds since the process logical origin.
        logical_receipt_ns: u64,
        /// Admission instant in nanoseconds since the process logical origin (monotonic).
        now_ns: u64,
    },
    /// Events and notices the adapter produced from one ingested market record.
    MarketOutcome {
        /// Market generation the record belongs to.
        generation: u64,
        /// The adapter's events and notices; admitted only when at least one is present.
        outcome: IngestOutcome,
        /// Record receipt in nanoseconds since the process logical origin.
        logical_receipt_ns: u64,
        /// Admission instant in nanoseconds since the process logical origin (monotonic).
        now_ns: u64,
    },
    /// The bound market departed and none replaced it yet. Emitted at most once per
    /// departure: while unbound, further discovery failures journal but stay silent.
    Unavailable {
        /// Market generation in flight when the departure was observed.
        generation: u64,
        /// Identity of the departing market; `None` if the run recorded none.
        previous_market_id: Option<String>,
        /// Why the market departed: rollover, unlisted successor, unreachable discovery.
        reason: MarketTransitionReason,
        /// Observation instant, nanoseconds since the process logical origin (monotonic).
        now_ns: u64,
    },
}

/// The shape of one live prediction run: venue, length, lineage, and fault injection.
#[derive(Debug, Clone)]
pub struct PredictionLiveOptions {
    /// Which prediction venue this run discovers and binds markets on.
    pub venue: PredictionMarketVenue,
    /// Total run length; validation rejects zero and anything over 7 days.
    pub duration: Duration,
    /// Process-lifetime publisher epoch stamped on emitted events; 1 to 128 bytes.
    pub publisher_epoch: String,
    /// Generation the first bind carries; must be positive, and increments per rollover.
    pub initial_generation: u64,
    /// Market a resumed run departed from, 1 to 256 bytes. Its presence makes the
    /// first bind a recovery bind rather than an initial bind.
    pub previous_market_id: Option<String>,
    /// Delay before retrying a discovery cycle that bound nothing, 1 ns to 60 s. It
    /// also caps the wait a close-to-open gap asks for.
    pub discovery_retry: Duration,
    /// Opt-in diagnostic: tear the socket down once, this far into the run, to prove
    /// reconnect. Must be positive and shorter than the run, and the run fails closed
    /// if exactly one induced disconnect and a following reconnect do not occur.
    pub induced_reconnect_interval: Option<Duration>,
    /// Coordinated shutdown receiver. When absent the run instead falls back to
    /// SIGINT/SIGTERM as its only operator stop.
    pub shutdown: Option<LiveShutdown>,
    /// Kalshi index-channel families the venue gate armed (ruling E2, audit
    /// A1/F6). Under ruling E37d the market runner no longer carries them: the
    /// armed families ride their own persistent connection
    /// ([`run_prediction_index_socket`]), independent of market discovery and
    /// roll, so a completing settlement frame that lands after the market socket
    /// tears down to roll is still received and derived. The market runner keeps
    /// this field only for lineage; it subscribes no index channel.
    pub index_sockets: ArmedIndexSockets,
}

impl PredictionLiveOptions {
    /// Build options for a run with the default 2-second discovery retry, no induced
    /// reconnect diagnostic, and no coordinated shutdown receiver.
    pub fn new(
        venue: PredictionMarketVenue,
        duration: Duration,
        publisher_epoch: impl Into<String>,
        initial_generation: u64,
        previous_market_id: Option<String>,
    ) -> Self {
        Self {
            venue,
            duration,
            publisher_epoch: publisher_epoch.into(),
            initial_generation,
            previous_market_id,
            discovery_retry: DEFAULT_DISCOVERY_RETRY,
            induced_reconnect_interval: None,
            shutdown: None,
            index_sockets: ArmedIndexSockets::NONE,
        }
    }

    fn validate(&self) -> Result<(), PredictionLiveError> {
        if self.venue != PredictionMarketVenue::Kalshi && self.index_sockets.any() {
            return Err(PredictionLiveError::InvalidOptions(
                "only the Kalshi lane carries index channels".to_owned(),
            ));
        }
        if self.duration.is_zero() || self.duration > MAX_RUN_DURATION {
            return Err(PredictionLiveError::InvalidOptions(format!(
                "duration must be between 1 second and {} seconds",
                MAX_RUN_DURATION.as_secs()
            )));
        }
        if self.publisher_epoch.trim().is_empty() || self.publisher_epoch.len() > 128 {
            return Err(PredictionLiveError::InvalidOptions(
                "publisher_epoch must contain 1 to 128 bytes".to_owned(),
            ));
        }
        if self.initial_generation == 0 {
            return Err(PredictionLiveError::InvalidOptions(
                "initial_generation must be positive".to_owned(),
            ));
        }
        if self.discovery_retry.is_zero() || self.discovery_retry > Duration::from_secs(60) {
            return Err(PredictionLiveError::InvalidOptions(
                "discovery_retry must be between 1 nanosecond and 60 seconds".to_owned(),
            ));
        }
        if self
            .previous_market_id
            .as_ref()
            .is_some_and(|id| id.is_empty() || id.len() > 256)
        {
            return Err(PredictionLiveError::InvalidOptions(
                "previous_market_id is empty or too long".to_owned(),
            ));
        }
        if self
            .induced_reconnect_interval
            .is_some_and(|interval| interval.is_zero() || interval >= self.duration)
        {
            return Err(PredictionLiveError::InvalidOptions(
                "induced reconnect interval must be positive and shorter than the run".to_owned(),
            ));
        }
        Ok(())
    }
}

/// What one completed run did. Every count is a whole-run total; a run that ends
/// early at the deadline or on an operator signal still returns its totals.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PredictionLiveReport {
    /// The venue the run bound markets on.
    pub venue: PredictionMarketVenue,
    /// Count of discovery attempts journaled across every cycle.
    pub discovery_attempts: u64,
    /// Count of markets bound, including the initial and any recovery bind.
    pub successful_bindings: u64,
    /// Count of binds that were scheduled rollovers from a live previous market.
    pub completed_rollovers: u64,
    /// Count of raw records fed to the market adapters, configuration frames included.
    pub records: u64,
    /// Count of source events the adapters emitted.
    pub events: u64,
    /// Count of feed notices the adapters emitted.
    pub notices: u64,
    /// Count of WebSocket connect attempts; the run fails closed at 100,000.
    pub connection_attempts: u64,
    /// Count of connect attempts that established a socket.
    pub successful_connections: u64,
    /// Count of mid-market socket teardowns that were followed by a reconnect wait.
    pub reconnects: u64,
    /// Count of read-idle timeouts that tore a socket down.
    pub read_idle_timeouts: u64,
    /// Count of operator-induced diagnostic teardowns; the gate requires exactly 1.
    pub induced_disconnects: u64,
    /// This worker's locally selected stop branch, not the authoritative
    /// process exit classification. The deadline coordinator owns that verdict.
    pub interrupted: bool,
}

#[derive(Default)]
struct PredictionLiveCounters {
    discovery_attempts: u64,
    successful_bindings: u64,
    completed_rollovers: u64,
    records: u64,
    events: u64,
    notices: u64,
    connection_attempts: u64,
    successful_connections: u64,
    reconnects: u64,
    read_idle_timeouts: u64,
    induced_disconnects: u64,
}

/// Discover, bind, ride, and roll one venue until the process deadline.
///
/// The callback is the admission boundary. It is invoked with each retained
/// discovery attempt before a selection affects signal state. Kalshi signed
/// request headers are consumed directly by tungstenite and are never present
/// in any callback value.
pub async fn run_prediction_live<H>(
    options: PredictionLiveOptions,
    clock: ProcessClock,
    mut handle: H,
) -> Result<PredictionLiveReport, PredictionLiveError>
where
    H: FnMut(PredictionLiveAdmission) -> Result<(), PredictionLiveError>,
{
    options.validate()?;
    install_crypto_provider()?;
    let auth = if options.venue == PredictionMarketVenue::Kalshi {
        Some(KalshiAuthClient::from_env()?)
    } else {
        None
    };
    let deadline = clock.deadline(options.duration);
    let mut shutdown = ShutdownWait::new(options.shutdown.clone());
    let mut pending_generation = options.initial_generation;
    let mut previous_market_id = options.previous_market_id.clone();
    let mut recovery_pending = previous_market_id.is_some();
    // A fresh or resumed runner starts unbound. A resumed checkpoint retains the
    // prior identity for a recovery bind, while the restored processor
    // deliberately clears its active market-relative state.
    let mut market_bound = false;
    let mut source_sequencer = ContinuousSourceSequencer::default();
    let mut last_failure_reason = None;
    let mut counters = PredictionLiveCounters::default();
    let mut interrupted = false;
    let induced_disconnect_at = options
        .induced_reconnect_interval
        .map(|interval| Instant::now() + interval)
        .filter(|instant| *instant < deadline);
    let mut induced_disconnect_done = false;

    'run: while Instant::now() < deadline {
        let discovery_now_ns = clock.elapsed_ns()?;
        let discovery_time = clock.wall_time_at(discovery_now_ns)?;
        let cycle = discover(options.venue, discovery_time, &clock).await?;
        for mut attempt in cycle.attempts {
            attempt.truncate_error_evidence()?;
            attempt.validate()?;
            counters.discovery_attempts = checked_add(counters.discovery_attempts, 1, || {
                PredictionLiveError::Counter
            })?;
            handle(PredictionLiveAdmission::Discovery {
                generation: pending_generation,
                attempt,
                now_ns: clock.admission_ns(clock.elapsed_ns()?)?,
            })?;
        }

        let discovered = match cycle.market {
            Some(market) => market,
            None => {
                let reason = cycle
                    .failure_reason
                    .unwrap_or(MarketTransitionReason::DiscoveryUnreachable);
                if begin_unavailability_if_bound(
                    &mut market_bound,
                    &mut last_failure_reason,
                    reason,
                ) {
                    handle(PredictionLiveAdmission::Unavailable {
                        generation: pending_generation,
                        previous_market_id: previous_market_id.clone(),
                        reason,
                        now_ns: clock.elapsed_ns()?,
                    })?;
                }
                match wait_for_retry(
                    cycle.retry_at,
                    options.discovery_retry,
                    deadline,
                    &mut shutdown,
                )
                .await?
                {
                    WaitResult::Retry => continue,
                    WaitResult::Deadline => break,
                    WaitResult::Interrupted => {
                        interrupted = true;
                        break;
                    }
                }
            }
        };

        let bind_reason = bind_reason(previous_market_id.as_deref(), recovery_pending);
        if is_departing_market_rediscovery(
            bind_reason,
            previous_market_id.as_deref(),
            discovered.market_id(),
        ) {
            let reason = MarketTransitionReason::NextMarketUnlisted;
            if begin_unavailability_if_bound(&mut market_bound, &mut last_failure_reason, reason) {
                handle(PredictionLiveAdmission::Unavailable {
                    generation: pending_generation,
                    previous_market_id: previous_market_id.clone(),
                    reason,
                    now_ns: clock.elapsed_ns()?,
                })?;
            }
            match wait_for_retry(None, options.discovery_retry, deadline, &mut shutdown).await? {
                WaitResult::Retry => continue,
                WaitResult::Deadline => break,
                WaitResult::Interrupted => {
                    interrupted = true;
                    break;
                }
            }
        }

        let stream = discovered.stream();
        let mut adapter = ReplayAdapter::new(
            stream,
            options.publisher_epoch.clone(),
            KrakenBookDepths::default(),
        )?;
        // Ruling E37d: the armed index channels no longer ride the market socket.
        // They now hold their own persistent connection ([`run_prediction_index_socket`]),
        // independent of market discovery and roll, so the completing settlement
        // frame that a venue delivers ~130 ms after a quarter close — after this
        // market socket has already torn down to roll — is still received and
        // derived. The market adapter therefore carries market channels only.
        let config_now_ns = clock.elapsed_ns()?;
        let configuration = RawRecord::configuration_at(
            stream,
            1,
            1,
            discovered.configuration_payload().to_owned(),
            clock.receipt_time_text(config_now_ns)?,
            config_now_ns,
        );
        counters.records = checked_add(counters.records, 1, || PredictionLiveError::Counter)?;
        let configured = adapter.ingest(&configuration)?;
        count_outcome(&mut counters, &configured)?;
        let [contract_event] = configured.events.as_slice() else {
            return Err(PredictionLiveError::InvalidDiscovery(
                "validated discovery did not emit exactly one contract event".to_owned(),
            ));
        };
        let mut contract_event = contract_event.clone();
        source_sequencer.assign(&mut contract_event)?;
        handle(PredictionLiveAdmission::Bind {
            generation: pending_generation,
            previous_market_id: previous_market_id.clone(),
            reason: bind_reason,
            event: contract_event,
            logical_receipt_ns: config_now_ns,
            now_ns: clock.admission_ns(config_now_ns)?,
        })?;
        market_bound = true;
        counters.successful_bindings = checked_add(counters.successful_bindings, 1, || {
            PredictionLiveError::Counter
        })?;
        if counts_completed_rollover(bind_reason) {
            counters.completed_rollovers = checked_add(counters.completed_rollovers, 1, || {
                PredictionLiveError::Counter
            })?;
        }
        recovery_pending = false;
        last_failure_reason = None;

        let close_ns = logical_ns(&clock, discovered.close_time())?;
        if clock.elapsed_ns()? >= close_ns {
            return Err(PredictionLiveError::InvalidDiscovery(
                "discovery selected a market whose close is not in the future".to_owned(),
            ));
        }

        let mut ordinal = 1_u64;
        let mut connection_epoch = 0_u64;
        let mut consecutive_connection_failures = 0_u64;
        // True once a genuine mid-market disconnect has admitted a reconnect
        // attempt, so the next successful connect for this market admits a
        // `Connected` recovery. Reset per market, so the initial bind's first
        // connect (handled by the bind admission) never counts as a recovery.
        let mut reconnecting_within_market = false;
        let mut market_rediscovery_required = false;
        'market: while Instant::now() < deadline && clock.elapsed_ns()? < close_ns {
            if counters.connection_attempts >= MAX_CONNECTION_ATTEMPTS {
                return Err(PredictionLiveError::ConnectionAttemptLimit);
            }
            counters.connection_attempts = checked_add(counters.connection_attempts, 1, || {
                PredictionLiveError::Counter
            })?;
            let connect_started_ns = clock.elapsed_ns()?;
            let request = if let Some(auth) = auth.as_ref() {
                auth.websocket_request()?
            } else {
                endpoint(stream)
                    .into_client_request()
                    .map_err(CaptureError::WebSocket)?
            };
            let market_deadline = logical_deadline(&clock, close_ns)?;
            let connected = tokio::select! {
                result = shutdown.wait() => {
                    interrupted = result
                        .map_err(PredictionLiveError::Signal)?
                        .interrupted();
                    break 'run;
                }
                () = sleep_until(deadline) => break 'run,
                () = sleep_until(market_deadline) => break 'market,
                result = timeout(
                    CONNECT_TIMEOUT,
                    connect_async_tls_with_config(
                        request,
                        Some(websocket_config()),
                        true,
                        Some(Connector::Rustls(crate::websocket_tls::client_config_for(
                            stream,
                        ))),
                    ),
                ) => result,
            };
            let (mut socket, _) = match connected {
                Ok(Ok(value)) => value,
                Ok(Err(_)) | Err(_) => {
                    consecutive_connection_failures =
                        checked_add(consecutive_connection_failures, 1, || {
                            PredictionLiveError::Counter
                        })?;
                    match wait_for_retry(
                        None,
                        reconnect_delay(consecutive_connection_failures),
                        market_deadline.min(deadline),
                        &mut shutdown,
                    )
                    .await?
                    {
                        WaitResult::Retry => continue,
                        WaitResult::Deadline => break 'market,
                        WaitResult::Interrupted => {
                            interrupted = true;
                            break 'run;
                        }
                    }
                }
            };
            consecutive_connection_failures = 0;
            counters.successful_connections =
                checked_add(counters.successful_connections, 1, || {
                    PredictionLiveError::Counter
                })?;
            connection_epoch = checked_add(connection_epoch, 1, || PredictionLiveError::Counter)?;
            ordinal = checked_add(ordinal, 1, || PredictionLiveError::Counter)?;
            let receipt_ns = clock.elapsed_ns()?;
            dispatch_record(
                &mut adapter,
                RawRecord::connected_at(
                    stream,
                    connection_epoch,
                    ordinal,
                    clock.receipt_time_text(receipt_ns)?,
                    receipt_ns,
                    connect_started_ns,
                ),
                pending_generation,
                receipt_ns,
                (&clock, &mut source_sequencer, &mut counters, &mut handle),
            )?;
            // A successful connect that follows an admitted disconnect for this
            // market is the recovery half of a socket transition; admit it so
            // feed-health returns the feed to healthy. The initial connect of a
            // market is not a recovery (the bind admission already promotes it),
            // so it is gated behind the per-market reconnect flag.
            if reconnecting_within_market {
                handle(PredictionLiveAdmission::Connectivity {
                    transition: PredictionSocketTransition::Connected,
                    now_ns: clock.elapsed_ns()?,
                })?;
                reconnecting_within_market = false;
            }

            let mut subscriptions_ready = true;
            let mut exit_run = false;
            let mut exit_market = false;
            // An operator-inflicted induced-reconnect diagnostic teardown is a
            // controlled close, not a genuine feed disconnect: it must admit no
            // socket-connectivity transitions, exactly like a rollover or shutdown.
            let mut induced_teardown = false;
            for subscription in subscriptions(
                stream,
                10,
                None,
                discovered.polymarket(),
                discovered.kalshi(),
                // Ruling E37d: the market socket subscribes no index channel; the
                // index families ride their own persistent connection.
                ArmedIndexSockets::NONE,
            )? {
                enum SubscriptionSend {
                    Sent(bool),
                    RunExit,
                    MarketExit,
                }
                let sent = tokio::select! {
                    result = shutdown.wait() => {
                        interrupted = result
                            .map_err(PredictionLiveError::Signal)?
                            .interrupted();
                        SubscriptionSend::RunExit
                    }
                    () = sleep_until(deadline) => SubscriptionSend::RunExit,
                    () = sleep_until(logical_deadline(&clock, close_ns)?) => {
                        SubscriptionSend::MarketExit
                    }
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
                    SubscriptionSend::RunExit => {
                        subscriptions_ready = false;
                        exit_run = true;
                        break;
                    }
                    SubscriptionSend::MarketExit => {
                        subscriptions_ready = false;
                        exit_market = true;
                        break;
                    }
                }
                ordinal = checked_add(ordinal, 1, || PredictionLiveError::Counter)?;
                let receipt_ns = clock.elapsed_ns()?;
                dispatch_record(
                    &mut adapter,
                    RawRecord::text_at(
                        stream,
                        connection_epoch,
                        ordinal,
                        RecordKind::OutboundText,
                        subscription.recorded,
                        clock.receipt_time_text(receipt_ns)?,
                        receipt_ns,
                    ),
                    pending_generation,
                    receipt_ns,
                    (&clock, &mut source_sequencer, &mut counters, &mut handle),
                )?;
            }

            // The ticker and its payload travel as one pair from the single
            // heartbeat authority so this runner cannot drift from capture.
            let mut heartbeat = if stream == CaptureStream::PolymarketMarket {
                crate::capture::text_heartbeat(stream).map(|(interval, payload)| {
                    (
                        tokio::time::interval_at(Instant::now() + interval, interval),
                        payload,
                    )
                })
            } else {
                None
            };
            while subscriptions_ready && !exit_run && !exit_market {
                enum SocketAction {
                    Message(
                        Result<
                            Option<Result<Message, tokio_tungstenite::tungstenite::Error>>,
                            tokio::time::error::Elapsed,
                        >,
                    ),
                    Heartbeat,
                    InducedDisconnect,
                    Exit,
                }
                let market_deadline = logical_deadline(&clock, close_ns)?;
                let action = tokio::select! {
                    result = shutdown.wait() => {
                        interrupted = result
                            .map_err(PredictionLiveError::Signal)?
                            .interrupted();
                        exit_run = true;
                        SocketAction::Exit
                    }
                    () = sleep_until(deadline) => {
                        exit_run = true;
                        SocketAction::Exit
                    },
                    () = sleep_until(market_deadline) => {
                        exit_market = true;
                        SocketAction::Exit
                    },
                    () = async {
                        if let Some(at) = induced_disconnect_at {
                            sleep_until(at).await;
                        }
                    }, if induced_disconnect_at.is_some() && !induced_disconnect_done => {
                        SocketAction::InducedDisconnect
                    }
                    _ = async {
                        heartbeat.as_mut().expect("guarded heartbeat exists").0.tick().await;
                    }, if heartbeat.is_some() => SocketAction::Heartbeat,
                    result = timeout(read_idle_timeout(stream), socket.next()) => {
                        SocketAction::Message(result)
                    }
                };
                match action {
                    SocketAction::Exit => break,
                    SocketAction::InducedDisconnect => {
                        counters.induced_disconnects =
                            checked_add(counters.induced_disconnects, 1, || {
                                PredictionLiveError::Counter
                            })?;
                        induced_disconnect_done = true;
                        induced_teardown = true;
                        break;
                    }
                    SocketAction::Heartbeat => {
                        let payload = heartbeat
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
                        ordinal = checked_add(ordinal, 1, || PredictionLiveError::Counter)?;
                        let receipt_ns = clock.elapsed_ns()?;
                        dispatch_record(
                            &mut adapter,
                            RawRecord::text_at(
                                stream,
                                connection_epoch,
                                ordinal,
                                RecordKind::OutboundText,
                                payload.to_owned(),
                                clock.receipt_time_text(receipt_ns)?,
                                receipt_ns,
                            ),
                            pending_generation,
                            receipt_ns,
                            (&clock, &mut source_sequencer, &mut counters, &mut handle),
                        )?;
                    }
                    SocketAction::Message(Ok(Some(Ok(Message::Text(payload))))) => {
                        ordinal = checked_add(ordinal, 1, || PredictionLiveError::Counter)?;
                        let receipt_ns = clock.elapsed_ns()?;
                        if dispatch_record(
                            &mut adapter,
                            RawRecord::text_at(
                                stream,
                                connection_epoch,
                                ordinal,
                                RecordKind::InboundText,
                                payload.to_string(),
                                clock.receipt_time_text(receipt_ns)?,
                                receipt_ns,
                            ),
                            pending_generation,
                            receipt_ns,
                            (&clock, &mut source_sequencer, &mut counters, &mut handle),
                        )? == PredictionDispatchControl::RediscoverMarket
                        {
                            market_rediscovery_required = true;
                            exit_market = true;
                        }
                    }
                    SocketAction::Message(Ok(Some(Ok(Message::Ping(bytes))))) => {
                        if socket.send(Message::Pong(bytes)).await.is_err() {
                            break;
                        }
                    }
                    SocketAction::Message(Ok(Some(Ok(Message::Pong(_))))) => {}
                    SocketAction::Message(Err(_)) => {
                        counters.read_idle_timeouts =
                            checked_add(counters.read_idle_timeouts, 1, || {
                                PredictionLiveError::Counter
                            })?;
                        break;
                    }
                    SocketAction::Message(Ok(Some(Ok(Message::Binary(_)))))
                    | SocketAction::Message(Ok(Some(Ok(Message::Frame(_))))) => {
                        // Ruling (global follow-up to E37d): an unexpected
                        // binary/continuation frame on the market socket is not
                        // fatal — a hard error would cancel the dedicated index
                        // socket and both spot runners through the shared
                        // `try_join!` and write no session record. Admit one
                        // counted `UnsupportedMessage` notice and drop; the
                        // disconnect record and the market socket's own reconnect
                        // ladder below take the stream honestly disconnected and
                        // re-dial. Only start-up refusals stay fatal.
                        let receipt_ns = clock.elapsed_ns()?;
                        admit_market_unsupported_message(
                            format!(
                                "unexpected binary/continuation frame on {}",
                                stream.as_str()
                            ),
                            pending_generation,
                            receipt_ns,
                            (&clock, &mut counters, &mut handle),
                        )?;
                        break;
                    }
                    SocketAction::Message(Ok(Some(Ok(Message::Close(_)))))
                    | SocketAction::Message(Ok(Some(Err(_))))
                    | SocketAction::Message(Ok(None)) => break,
                }
            }

            ordinal = checked_add(ordinal, 1, || PredictionLiveError::Counter)?;
            let receipt_ns = clock.elapsed_ns()?;
            dispatch_record(
                &mut adapter,
                RawRecord::disconnected_at(
                    stream,
                    connection_epoch,
                    ordinal,
                    clock.receipt_time_text(receipt_ns)?,
                    receipt_ns,
                ),
                pending_generation,
                receipt_ns,
                (&clock, &mut source_sequencer, &mut counters, &mut handle),
            )?;
            if exit_run {
                break 'run;
            }
            if exit_market {
                break 'market;
            }
            let will_reconnect = clock.elapsed_ns()? < close_ns && Instant::now() < deadline;
            if will_reconnect {
                // Admit socket-level transitions only for a genuine mid-market
                // disconnect, per `admits_socket_disconnect`: `Disconnected` marks
                // the feed down, `Reconnecting` marks the in-progress recovery.
                // Controlled closes stay silent — a rollover/shutdown already broke
                // out above, and an operator-inflicted induced teardown is excluded
                // here — so no controlled close registers as disconnect-sickness.
                // The reconnect itself still runs in every case.
                if admits_socket_disconnect(exit_run, exit_market, induced_teardown, will_reconnect)
                {
                    handle(PredictionLiveAdmission::Connectivity {
                        transition: PredictionSocketTransition::Disconnected,
                        now_ns: clock.elapsed_ns()?,
                    })?;
                    handle(PredictionLiveAdmission::Connectivity {
                        transition: PredictionSocketTransition::Reconnecting,
                        now_ns: clock.elapsed_ns()?,
                    })?;
                    reconnecting_within_market = true;
                }
                counters.reconnects =
                    checked_add(counters.reconnects, 1, || PredictionLiveError::Counter)?;
                match wait_for_retry(
                    None,
                    reconnect_delay(consecutive_connection_failures.max(1)),
                    logical_deadline(&clock, close_ns)?.min(deadline),
                    &mut shutdown,
                )
                .await?
                {
                    WaitResult::Retry => {}
                    WaitResult::Deadline => break 'market,
                    WaitResult::Interrupted => {
                        interrupted = true;
                        break 'run;
                    }
                }
            }
        }

        if interrupted || Instant::now() >= deadline {
            break;
        }
        previous_market_id = Some(discovered.market_id().to_owned());
        pending_generation = checked_add(pending_generation, 1, || PredictionLiveError::Counter)?;
        if market_rediscovery_required {
            recovery_pending = true;
            let reason = MarketTransitionReason::ProviderLifecycleChange;
            if begin_unavailability_if_bound(&mut market_bound, &mut last_failure_reason, reason) {
                handle(PredictionLiveAdmission::Unavailable {
                    generation: pending_generation,
                    previous_market_id: previous_market_id.clone(),
                    reason,
                    now_ns: clock.elapsed_ns()?,
                })?;
            }
            continue 'run;
        }
        let reason = MarketTransitionReason::ScheduledRollover;
        if begin_unavailability_if_bound(&mut market_bound, &mut last_failure_reason, reason) {
            handle(PredictionLiveAdmission::Unavailable {
                generation: pending_generation,
                previous_market_id: previous_market_id.clone(),
                reason,
                now_ns: clock.elapsed_ns()?,
            })?;
        }
    }

    // Keep the opt-in diagnostic gate distinct from its completion invariant.
    #[allow(clippy::collapsible_if)]
    if !interrupted && options.induced_reconnect_interval.is_some() {
        if counters.induced_disconnects != 1 || counters.successful_connections < 2 {
            return Err(PredictionLiveError::DiagnosticIncomplete(
                "the run did not complete one induced disconnect and reconnect".to_owned(),
            ));
        }
    }
    Ok(PredictionLiveReport {
        venue: options.venue,
        discovery_attempts: counters.discovery_attempts,
        successful_bindings: counters.successful_bindings,
        completed_rollovers: counters.completed_rollovers,
        records: counters.records,
        events: counters.events,
        notices: counters.notices,
        connection_attempts: counters.connection_attempts,
        successful_connections: counters.successful_connections,
        reconnects: counters.reconnects,
        read_idle_timeouts: counters.read_idle_timeouts,
        induced_disconnects: counters.induced_disconnects,
        interrupted,
    })
}

/// The shape of the dedicated Kalshi index-socket runner (ruling E37d).
///
/// This connection carries the armed CF Benchmarks BRTI value / 5 Hz value /
/// settlement-window channels and the Pyth relay, independent of market
/// discovery and roll. It runs for the whole process life against the venue's
/// index endpoint with the same reconnect/backoff ladder every other socket
/// uses, and its frames are admitted as auxiliary events under whatever
/// generation is current when they arrive.
#[derive(Debug, Clone)]
pub struct PredictionIndexSocketOptions {
    /// The venue whose index socket to open. Only Kalshi carries index channels;
    /// every other venue opens no socket and admits nothing.
    pub venue: PredictionMarketVenue,
    /// Total run length; validation rejects zero and anything over 7 days.
    pub duration: Duration,
    /// Process-lifetime publisher epoch stamped on emitted events; 1 to 128 bytes.
    pub publisher_epoch: String,
    /// Index-channel families the venue gate armed. No armed family opens no
    /// socket, so a disarmed lane is byte-identical to one with no index runner.
    pub index_sockets: ArmedIndexSockets,
    /// Coordinated shutdown receiver shared with every other live task.
    pub shutdown: Option<LiveShutdown>,
}

impl PredictionIndexSocketOptions {
    /// Build index-socket options with no coordinated shutdown receiver.
    pub fn new(
        venue: PredictionMarketVenue,
        duration: Duration,
        publisher_epoch: impl Into<String>,
        index_sockets: ArmedIndexSockets,
    ) -> Self {
        Self {
            venue,
            duration,
            publisher_epoch: publisher_epoch.into(),
            index_sockets,
            shutdown: None,
        }
    }

    fn validate(&self) -> Result<(), PredictionLiveError> {
        if self.duration.is_zero() || self.duration > MAX_RUN_DURATION {
            return Err(PredictionLiveError::InvalidOptions(
                "run duration must be between 1 second and 7 days".to_owned(),
            ));
        }
        let epoch_len = self.publisher_epoch.len();
        if epoch_len == 0 || epoch_len > 128 || self.publisher_epoch.trim().is_empty() {
            return Err(PredictionLiveError::InvalidOptions(
                "publisher epoch must be 1 to 128 non-blank bytes".to_owned(),
            ));
        }
        Ok(())
    }
}

fn index_socket_report(
    venue: PredictionMarketVenue,
    counters: &PredictionLiveCounters,
    interrupted: bool,
) -> PredictionLiveReport {
    PredictionLiveReport {
        venue,
        discovery_attempts: 0,
        successful_bindings: 0,
        completed_rollovers: 0,
        records: counters.records,
        events: counters.events,
        notices: counters.notices,
        connection_attempts: counters.connection_attempts,
        successful_connections: counters.successful_connections,
        reconnects: counters.reconnects,
        read_idle_timeouts: counters.read_idle_timeouts,
        induced_disconnects: 0,
        interrupted,
    }
}

/// Whether a message the auxiliary Kalshi index socket receives ends the
/// connection with a drop-and-reconnect rather than being processed inline.
///
/// Ruling E37d (MAJOR 1): the index socket shares the market lane's
/// `tokio::try_join!`, so nothing it receives may abort the run. A close,
/// transport error, or read EOF drops as on every socket; a binary or
/// continuation frame — since the global E37d follow-up, a counted-notice drop
/// on the market and spot sockets too — is a drop here as well, never a hard
/// error, so it cannot cancel the market runner. Text/Ping/Pong are handled
/// inline and return `false`. Pure so the policy is unit-tested without a live
/// socket (the end-to-end path is network-bound).
fn index_read_drops(
    message: &Option<Result<Message, tokio_tungstenite::tungstenite::Error>>,
) -> bool {
    matches!(
        message,
        Some(Ok(Message::Binary(_)
            | Message::Frame(_)
            | Message::Close(_)))
            | Some(Err(_))
            | None
    )
}

/// Run the dedicated Kalshi index socket for the whole process (ruling E37d).
///
/// The index families ride their own persistent connection so a market roll —
/// which tears the market socket down at the quarter close — never interrupts
/// the index stream. The completing settlement-window frame a venue delivers
/// ~130 ms after a close is therefore still received; the quarter-keyed adapter
/// derives the size-60 event and the admission boundary stamps it as an
/// auxiliary event under the current generation, so it reaches the departed
/// market's pending final-decision slot (E37c) before the slot's deadline.
///
/// The callback is the same admission boundary [`run_prediction_live`] uses. A
/// non-Kalshi venue or an unarmed lane opens no socket, admits nothing, and
/// returns an empty report, so a disarmed run is byte-identical to before E37d.
/// The `generation` on every [`PredictionLiveAdmission::MarketOutcome`] this
/// runner emits is a placeholder the caller MUST re-derive from the pipeline
/// (index frames are auxiliary and carry no market binding); the market runner
/// is the only source of a meaningful generation.
pub async fn run_prediction_index_socket<H>(
    options: PredictionIndexSocketOptions,
    clock: ProcessClock,
    mut handle: H,
) -> Result<PredictionLiveReport, PredictionLiveError>
where
    H: FnMut(PredictionLiveAdmission) -> Result<(), PredictionLiveError>,
{
    options.validate()?;
    let mut counters = PredictionLiveCounters::default();
    // A lane with no armed index family — every non-Kalshi venue, and a Kalshi
    // lane the gate did not open — opens no socket and admits nothing. This is
    // what keeps a disarmed run byte-identical to one built before E37d.
    if options.venue != PredictionMarketVenue::Kalshi || !options.index_sockets.any() {
        return Ok(index_socket_report(options.venue, &counters, false));
    }
    install_crypto_provider()?;
    let auth = KalshiAuthClient::from_env()?;
    let stream = CaptureStream::KalshiIndex;
    let deadline = clock.deadline(options.duration);
    let mut shutdown = ShutdownWait::new(options.shutdown.clone());
    // The dedicated index adapter derives the settlement window from the value
    // frame's quarter object, keyed to the quarter close on the 15-minute grid —
    // never to a market binding — so it completes the departed quarter even when
    // the frame arrives while the lane is unbound between roll and successor bind.
    let mut adapter = ReplayAdapter::new(
        stream,
        options.publisher_epoch.clone(),
        KrakenBookDepths::default(),
    )?;
    let mut source_sequencer = ContinuousSourceSequencer::default();
    let mut ordinal = 0_u64;
    let mut connection_epoch = 0_u64;
    let mut consecutive_connection_failures = 0_u64;
    let mut interrupted = false;
    // Once a genuine drop has admitted a `Disconnected`/`Reconnecting` pair, the
    // next successful connect admits the `Connected` half. The run's first
    // connect is not a recovery, so it is gated behind this flag exactly as the
    // market socket gates its own.
    let mut reconnecting = false;

    'run: while Instant::now() < deadline {
        if counters.connection_attempts >= MAX_CONNECTION_ATTEMPTS {
            return Err(PredictionLiveError::ConnectionAttemptLimit);
        }
        counters.connection_attempts = checked_add(counters.connection_attempts, 1, || {
            PredictionLiveError::Counter
        })?;
        let connect_started_ns = clock.elapsed_ns()?;
        // The index socket signs its own path so the signature and the request
        // can never describe different channels. It is a second connection to the
        // same host the market socket uses (observed, ruling E23).
        let request = auth.websocket_request_for(KALSHI_WEBSOCKET_HOST, INDEX_WEBSOCKET_PATH)?;
        let connected = tokio::select! {
            result = shutdown.wait() => {
                interrupted = result.map_err(PredictionLiveError::Signal)?.interrupted();
                break 'run;
            }
            () = sleep_until(deadline) => break 'run,
            result = timeout(
                CONNECT_TIMEOUT,
                connect_async_tls_with_config(
                    request,
                    Some(websocket_config()),
                    true,
                    Some(Connector::Rustls(crate::websocket_tls::client_config_for(stream))),
                ),
            ) => result,
        };
        let (mut socket, _) = match connected {
            Ok(Ok(value)) => value,
            Ok(Err(_)) | Err(_) => {
                consecutive_connection_failures =
                    checked_add(consecutive_connection_failures, 1, || {
                        PredictionLiveError::Counter
                    })?;
                match wait_for_retry(
                    None,
                    reconnect_delay(consecutive_connection_failures),
                    deadline,
                    &mut shutdown,
                )
                .await?
                {
                    WaitResult::Retry => continue,
                    WaitResult::Deadline => break 'run,
                    WaitResult::Interrupted => {
                        interrupted = true;
                        break 'run;
                    }
                }
            }
        };
        consecutive_connection_failures = 0;
        counters.successful_connections = checked_add(counters.successful_connections, 1, || {
            PredictionLiveError::Counter
        })?;
        connection_epoch = checked_add(connection_epoch, 1, || PredictionLiveError::Counter)?;
        ordinal = checked_add(ordinal, 1, || PredictionLiveError::Counter)?;
        let receipt_ns = clock.elapsed_ns()?;
        // Feed the connect marker through the adapter so its per-channel cursors
        // reset on the new connection: a reconnect re-seeds the sequence frontier
        // rather than reading the first post-reconnect frame as a gap.
        dispatch_record(
            &mut adapter,
            RawRecord::connected_at(
                stream,
                connection_epoch,
                ordinal,
                clock.receipt_time_text(receipt_ns)?,
                receipt_ns,
                connect_started_ns,
            ),
            0,
            receipt_ns,
            (&clock, &mut source_sequencer, &mut counters, &mut handle),
        )?;
        if reconnecting {
            handle(PredictionLiveAdmission::Connectivity {
                transition: PredictionSocketTransition::Connected,
                now_ns: clock.elapsed_ns()?,
            })?;
            reconnecting = false;
        }

        let mut subscriptions_ready = true;
        let mut exit_run = false;
        for subscription in subscriptions(stream, 10, None, None, None, options.index_sockets)? {
            let sent = tokio::select! {
                result = shutdown.wait() => {
                    interrupted = result.map_err(PredictionLiveError::Signal)?.interrupted();
                    None
                }
                () = sleep_until(deadline) => None,
                result = socket.send(Message::Text(subscription.wire.as_str().to_owned().into())) => {
                    Some(result.is_ok())
                }
            };
            match sent {
                Some(true) => {}
                Some(false) => {
                    subscriptions_ready = false;
                    break;
                }
                None => {
                    subscriptions_ready = false;
                    exit_run = true;
                    break;
                }
            }
            ordinal = checked_add(ordinal, 1, || PredictionLiveError::Counter)?;
            let receipt_ns = clock.elapsed_ns()?;
            dispatch_record(
                &mut adapter,
                RawRecord::text_at(
                    stream,
                    connection_epoch,
                    ordinal,
                    RecordKind::OutboundText,
                    subscription.recorded,
                    clock.receipt_time_text(receipt_ns)?,
                    receipt_ns,
                ),
                0,
                receipt_ns,
                (&clock, &mut source_sequencer, &mut counters, &mut handle),
            )?;
        }

        while subscriptions_ready && !exit_run {
            let action = tokio::select! {
                result = shutdown.wait() => {
                    interrupted = result.map_err(PredictionLiveError::Signal)?.interrupted();
                    exit_run = true;
                    None
                }
                () = sleep_until(deadline) => {
                    exit_run = true;
                    None
                }
                result = timeout(read_idle_timeout(stream), socket.next()) => Some(result),
            };
            let Some(result) = action else { break };
            let message = match result {
                Ok(message) => message,
                Err(_) => {
                    counters.read_idle_timeouts =
                        checked_add(counters.read_idle_timeouts, 1, || {
                            PredictionLiveError::Counter
                        })?;
                    break;
                }
            };
            // Ruling E37d (MAJOR 1): the auxiliary index socket shares the market
            // lane's `tokio::try_join!`, so nothing it receives may abort the run.
            // A close, transport error, or read EOF drops-and-reconnects as always;
            // an unexpected binary/continuation frame — a counted-notice drop on
            // the market and spot sockets too, since the global E37d follow-up — is
            // degraded to the same drop here, never a hard error, so it cannot
            // cancel the market runner and leave no session record. The reconnect
            // ladder below records index connectivity-down (families go honestly
            // unavailable/stale) and re-dials.
            if index_read_drops(&message) {
                break;
            }
            match message {
                Some(Ok(Message::Text(payload))) => {
                    ordinal = checked_add(ordinal, 1, || PredictionLiveError::Counter)?;
                    let receipt_ns = clock.elapsed_ns()?;
                    // Ruling E37d (MAJOR 1): an adapter-ingest or admission fault on
                    // an index frame is degraded to a drop for the same reason — it
                    // must not propagate into the shared `try_join!`. Only start-up
                    // refusals (auth, definitions, misconfiguration), which precede
                    // this loop, stay fatal.
                    if dispatch_record(
                        &mut adapter,
                        RawRecord::text_at(
                            stream,
                            connection_epoch,
                            ordinal,
                            RecordKind::InboundText,
                            payload.to_string(),
                            clock.receipt_time_text(receipt_ns)?,
                            receipt_ns,
                        ),
                        0,
                        receipt_ns,
                        (&clock, &mut source_sequencer, &mut counters, &mut handle),
                    )
                    .is_err()
                    {
                        break;
                    }
                }
                Some(Ok(Message::Ping(bytes))) => {
                    if socket.send(Message::Pong(bytes)).await.is_err() {
                        break;
                    }
                }
                Some(Ok(Message::Pong(_))) => {}
                // Every drop variant (close/error/EOF/binary/continuation) is
                // handled by `index_read_drops` above; this arm is unreachable but
                // keeps the match exhaustive without a fresh fatal path.
                _ => break,
            }
        }

        ordinal = checked_add(ordinal, 1, || PredictionLiveError::Counter)?;
        let receipt_ns = clock.elapsed_ns()?;
        dispatch_record(
            &mut adapter,
            RawRecord::disconnected_at(
                stream,
                connection_epoch,
                ordinal,
                clock.receipt_time_text(receipt_ns)?,
                receipt_ns,
            ),
            0,
            receipt_ns,
            (&clock, &mut source_sequencer, &mut counters, &mut handle),
        )?;
        if exit_run {
            break 'run;
        }
        // The index socket has no controlled close of its own: it does not roll,
        // so every break that is not the deadline or shutdown above is a genuine
        // drop. Admit the disconnect-and-recover pair and reconnect. A market
        // roll never reaches here — it lives on the other socket — so the index
        // socket's connection attempts are unchanged across a roll.
        if Instant::now() < deadline {
            handle(PredictionLiveAdmission::Connectivity {
                transition: PredictionSocketTransition::Disconnected,
                now_ns: clock.elapsed_ns()?,
            })?;
            handle(PredictionLiveAdmission::Connectivity {
                transition: PredictionSocketTransition::Reconnecting,
                now_ns: clock.elapsed_ns()?,
            })?;
            reconnecting = true;
            counters.reconnects =
                checked_add(counters.reconnects, 1, || PredictionLiveError::Counter)?;
            match wait_for_retry(
                None,
                reconnect_delay(consecutive_connection_failures.max(1)),
                deadline,
                &mut shutdown,
            )
            .await?
            {
                WaitResult::Retry => {}
                WaitResult::Deadline => break 'run,
                WaitResult::Interrupted => {
                    interrupted = true;
                    break 'run;
                }
            }
        }
    }

    Ok(index_socket_report(options.venue, &counters, interrupted))
}

struct DiscoveryCycle {
    attempts: Vec<PredictionDiscoveryAttempt>,
    market: Option<DiscoveredMarket>,
    failure_reason: Option<MarketTransitionReason>,
    retry_at: Option<DateTime<Utc>>,
}

enum DiscoveredMarket {
    Polymarket {
        market_id: String,
        close_time: DateTime<Utc>,
        discovery: PolymarketDiscovery,
    },
    Kalshi {
        market_id: String,
        close_time: DateTime<Utc>,
        discovery: KalshiDiscovery,
    },
}

impl DiscoveredMarket {
    fn stream(&self) -> CaptureStream {
        match self {
            Self::Polymarket { .. } => CaptureStream::PolymarketMarket,
            Self::Kalshi { .. } => CaptureStream::KalshiMarket,
        }
    }

    fn market_id(&self) -> &str {
        match self {
            Self::Polymarket { market_id, .. } | Self::Kalshi { market_id, .. } => market_id,
        }
    }

    fn close_time(&self) -> DateTime<Utc> {
        match self {
            Self::Polymarket { close_time, .. } | Self::Kalshi { close_time, .. } => *close_time,
        }
    }

    fn configuration_payload(&self) -> &str {
        match self {
            Self::Polymarket { discovery, .. } => &discovery.journal_payload,
            Self::Kalshi { discovery, .. } => &discovery.journal_payload,
        }
    }

    fn polymarket(&self) -> Option<&PolymarketDiscovery> {
        match self {
            Self::Polymarket { discovery, .. } => Some(discovery),
            Self::Kalshi { .. } => None,
        }
    }

    fn kalshi(&self) -> Option<&KalshiDiscovery> {
        match self {
            Self::Kalshi { discovery, .. } => Some(discovery),
            Self::Polymarket { .. } => None,
        }
    }
}

async fn discover(
    venue: PredictionMarketVenue,
    now: DateTime<Utc>,
    clock: &ProcessClock,
) -> Result<DiscoveryCycle, PredictionLiveError> {
    match venue {
        PredictionMarketVenue::Polymarket => discover_polymarket(now, clock).await,
        PredictionMarketVenue::Kalshi => discover_kalshi(now, clock).await,
    }
}

async fn discover_polymarket(
    now: DateTime<Utc>,
    clock: &ProcessClock,
) -> Result<DiscoveryCycle, PredictionLiveError> {
    let unix = now.timestamp();
    let slot = unix.div_euclid(300) * 300;
    let slug = format!("btc-updown-5m-{slot}");
    let path = format!("/markets?slug={slug}&closed=false");
    let fetched = fetch(
        GAMMA_MARKETS_URL,
        &[("slug", slug.as_str()), ("closed", "false")],
        path,
        MAX_GAMMA_RESPONSE_BYTES,
        clock,
    )
    .await?;
    let mut attempt = fetched.attempt(
        PredictionMarketVenue::Polymarket,
        PredictionDiscoveryStage::ExactMarket,
    );
    let fetched_failure = fetched.failure_reason();
    let body = match fetched.body {
        Some(body) => body,
        None => {
            return Ok(DiscoveryCycle {
                attempts: vec![attempt],
                market: None,
                failure_reason: Some(fetched_failure),
                retry_at: None,
            });
        }
    };
    let metadata = match parse_polymarket_metadata(&body, &slug) {
        Ok(Some(metadata)) => metadata,
        Ok(None) => {
            attempt.result = PredictionDiscoveryResult::Unlisted;
            return Ok(DiscoveryCycle {
                attempts: vec![attempt],
                market: None,
                failure_reason: Some(MarketTransitionReason::NextMarketUnlisted),
                retry_at: None,
            });
        }
        Err(_) => {
            attempt.result = PredictionDiscoveryResult::Invalid;
            return Ok(DiscoveryCycle {
                attempts: vec![attempt],
                market: None,
                failure_reason: Some(MarketTransitionReason::InvalidDiscovery),
                retry_at: None,
            });
        }
    };
    let discovery = match polymarket_discovery_from_response(
        &slug,
        false,
        attempt.retrieved_at.clone(),
        body,
    ) {
        Ok(discovery) => discovery,
        Err(_) => {
            attempt.result = PredictionDiscoveryResult::Invalid;
            return Ok(DiscoveryCycle {
                attempts: vec![attempt],
                market: None,
                failure_reason: Some(MarketTransitionReason::InvalidDiscovery),
                retry_at: None,
            });
        }
    };
    if metadata.close_time <= now {
        attempt.result = PredictionDiscoveryResult::Invalid;
        return Ok(DiscoveryCycle {
            attempts: vec![attempt],
            market: None,
            failure_reason: Some(MarketTransitionReason::InvalidDiscovery),
            retry_at: None,
        });
    }
    attempt.result = PredictionDiscoveryResult::Selected;
    attempt.selected_market_id = Some(metadata.market_id.clone());
    Ok(DiscoveryCycle {
        attempts: vec![attempt],
        market: Some(DiscoveredMarket::Polymarket {
            market_id: metadata.market_id,
            close_time: metadata.close_time,
            discovery,
        }),
        failure_reason: None,
        retry_at: None,
    })
}

async fn discover_kalshi(
    now: DateTime<Utc>,
    clock: &ProcessClock,
) -> Result<DiscoveryCycle, PredictionLiveError> {
    let current_path =
        format!("/trade-api/v2/markets?series_ticker={KALSHI_SERIES}&status=open&limit=1000");
    let current = fetch(
        KALSHI_MARKETS_URL,
        &[
            ("series_ticker", KALSHI_SERIES),
            ("status", "open"),
            ("limit", "1000"),
        ],
        current_path,
        MAX_KALSHI_DISCOVERY_RESPONSE_BYTES,
        clock,
    )
    .await?;
    let mut attempts = vec![current.attempt(
        PredictionMarketVenue::Kalshi,
        PredictionDiscoveryStage::CurrentMarket,
    )];
    let current_failure = current.failure_reason();
    let current_body = match current.body {
        Some(body) => body,
        None => {
            return Ok(DiscoveryCycle {
                attempts,
                market: None,
                failure_reason: Some(current_failure),
                retry_at: None,
            });
        }
    };
    let current_markets = match parse_kalshi_listing(&current_body) {
        Ok(markets) => markets,
        Err(_) => {
            attempts[0].result = PredictionDiscoveryResult::Invalid;
            return Ok(DiscoveryCycle {
                attempts,
                market: None,
                failure_reason: Some(MarketTransitionReason::InvalidDiscovery),
                retry_at: None,
            });
        }
    };
    let active = current_markets
        .into_iter()
        .filter(|market| market.open_time <= now && now < market.close_time)
        .collect::<Vec<_>>();
    let selected = match active.as_slice() {
        [market] => Some(market.clone()),
        [] => None,
        _ => {
            attempts[0].result = PredictionDiscoveryResult::Invalid;
            return Ok(DiscoveryCycle {
                attempts,
                market: None,
                failure_reason: Some(MarketTransitionReason::InvalidDiscovery),
                retry_at: None,
            });
        }
    };
    let selected = if let Some(selected) = selected {
        attempts[0].result = PredictionDiscoveryResult::Selected;
        attempts[0].selected_market_id = Some(selected.ticker.clone());
        selected
    } else {
        attempts[0].result = PredictionDiscoveryResult::Unlisted;
        let next_path = format!(
            "/trade-api/v2/markets?series_ticker={KALSHI_SERIES}&status=unopened&limit=1000"
        );
        let next = fetch(
            KALSHI_MARKETS_URL,
            &[
                ("series_ticker", KALSHI_SERIES),
                ("status", "unopened"),
                ("limit", "1000"),
            ],
            next_path,
            MAX_KALSHI_DISCOVERY_RESPONSE_BYTES,
            clock,
        )
        .await?;
        attempts.push(next.attempt(
            PredictionMarketVenue::Kalshi,
            PredictionDiscoveryStage::NextMarket,
        ));
        let next_failure = next.failure_reason();
        let next_body = match next.body {
            Some(body) => body,
            None => {
                return Ok(DiscoveryCycle {
                    attempts,
                    market: None,
                    failure_reason: Some(next_failure),
                    retry_at: None,
                });
            }
        };
        let mut future = match parse_kalshi_listing(&next_body) {
            Ok(markets) => markets
                .into_iter()
                .filter(|market| market.open_time > now)
                .collect::<Vec<_>>(),
            Err(_) => {
                attempts[1].result = PredictionDiscoveryResult::Invalid;
                return Ok(DiscoveryCycle {
                    attempts,
                    market: None,
                    failure_reason: Some(MarketTransitionReason::InvalidDiscovery),
                    retry_at: None,
                });
            }
        };
        future.sort_by(|left, right| {
            (&left.open_time, &left.close_time, &left.ticker).cmp(&(
                &right.open_time,
                &right.close_time,
                &right.ticker,
            ))
        });
        if let Some(next_market) = future.first() {
            attempts[1].result = PredictionDiscoveryResult::Gap;
            return Ok(DiscoveryCycle {
                attempts,
                market: None,
                failure_reason: Some(MarketTransitionReason::CloseToNextOpenGap),
                retry_at: Some(next_market.open_time),
            });
        }
        attempts[1].result = PredictionDiscoveryResult::Unlisted;
        return Ok(DiscoveryCycle {
            attempts,
            market: None,
            failure_reason: Some(MarketTransitionReason::NextMarketUnlisted),
            retry_at: None,
        });
    };

    let exact_path = format!("/trade-api/v2/markets?tickers={}", selected.ticker);
    let exact = fetch(
        KALSHI_MARKETS_URL,
        &[("tickers", selected.ticker.as_str())],
        exact_path,
        MAX_KALSHI_DISCOVERY_RESPONSE_BYTES,
        clock,
    )
    .await?;
    attempts.push(exact.attempt(
        PredictionMarketVenue::Kalshi,
        PredictionDiscoveryStage::ExactMarket,
    ));
    let exact_failure = exact.failure_reason();
    let exact_body = match exact.body {
        Some(body) => body,
        None => {
            return Ok(DiscoveryCycle {
                attempts,
                market: None,
                failure_reason: Some(exact_failure),
                retry_at: None,
            });
        }
    };
    let discovery = match kalshi_discovery_from_response(
        &selected.ticker,
        attempts
            .last()
            .expect("exact attempt exists")
            .retrieved_at
            .clone(),
        exact_body,
    ) {
        Ok(discovery) => discovery,
        Err(_) => {
            attempts.last_mut().expect("exact attempt exists").result =
                PredictionDiscoveryResult::Invalid;
            return Ok(DiscoveryCycle {
                attempts,
                market: None,
                failure_reason: Some(MarketTransitionReason::InvalidDiscovery),
                retry_at: None,
            });
        }
    };
    let exact_attempt = attempts.last_mut().expect("exact attempt exists");
    exact_attempt.result = PredictionDiscoveryResult::Selected;
    exact_attempt.selected_market_id = Some(selected.ticker.clone());
    Ok(DiscoveryCycle {
        attempts,
        market: Some(DiscoveredMarket::Kalshi {
            market_id: selected.ticker,
            close_time: selected.close_time,
            discovery,
        }),
        failure_reason: None,
        retry_at: None,
    })
}

struct FetchResult {
    request_path: String,
    retrieved_at: String,
    http_status: Option<u16>,
    response_body_encoding: PredictionDiscoveryBodyEncoding,
    response_body: String,
    response_body_sha256: String,
    body: Option<String>,
}

impl FetchResult {
    fn failure_reason(&self) -> MarketTransitionReason {
        if self.http_status.is_some() {
            MarketTransitionReason::InvalidDiscovery
        } else {
            MarketTransitionReason::DiscoveryUnreachable
        }
    }

    fn attempt(
        &self,
        venue: PredictionMarketVenue,
        stage: PredictionDiscoveryStage,
    ) -> PredictionDiscoveryAttempt {
        PredictionDiscoveryAttempt {
            schema_version: 1,
            venue,
            stage,
            request_path: self.request_path.clone(),
            retrieved_at: self.retrieved_at.clone(),
            http_status: self.http_status,
            response_body_encoding: self.response_body_encoding,
            response_body_sha256: self.response_body_sha256.clone(),
            response_body: self.response_body.clone(),
            result: if self.http_status.is_some() {
                PredictionDiscoveryResult::Invalid
            } else {
                PredictionDiscoveryResult::Unreachable
            },
            selected_market_id: None,
        }
    }
}

async fn fetch(
    url: &str,
    query: &[(&str, &str)],
    request_path: String,
    maximum_bytes: usize,
    clock: &ProcessClock,
) -> Result<FetchResult, PredictionLiveError> {
    let result = match secure_http::client() {
        Ok(client) => match client
            .get(url)
            .header(ACCEPT, "application/json")
            .header(CACHE_CONTROL, "no-cache")
            .query(query)
            .send()
            .await
        {
            Ok(response) => {
                let status = response.status().as_u16();
                match secure_http::bounded_body_with_status(response, maximum_bytes).await {
                    Ok((_, bytes)) => Some((status, bytes, true)),
                    Err(_) => Some((status, Vec::new(), false)),
                }
            }
            Err(_) => None,
        },
        Err(_) => None,
    };
    let receipt_ns = clock.elapsed_ns()?;
    let retrieved_at = clock.receipt_time_text(receipt_ns)?;
    let (http_status, bytes, body_available) = result
        .map(|(status, bytes, available)| (Some(status), bytes, available))
        .unwrap_or((None, Vec::new(), false));
    let response_body_sha256 = sha256_hex(&bytes);
    let success = http_status.is_some_and(|status| (200..300).contains(&status));
    let (response_body_encoding, response_body, body) = match String::from_utf8(bytes) {
        Ok(text) if body_available => (
            PredictionDiscoveryBodyEncoding::Utf8,
            text.clone(),
            success.then_some(text),
        ),
        Err(error) if body_available => (
            PredictionDiscoveryBodyEncoding::Base64,
            BASE64_STANDARD.encode(error.into_bytes()),
            None,
        ),
        _ => (PredictionDiscoveryBodyEncoding::None, String::new(), None),
    };
    Ok(FetchResult {
        request_path,
        retrieved_at,
        http_status,
        response_body_encoding,
        response_body_sha256,
        response_body,
        body,
    })
}

#[derive(Clone)]
struct KalshiListingMarket {
    ticker: String,
    open_time: DateTime<Utc>,
    close_time: DateTime<Utc>,
}

#[derive(Deserialize)]
struct KalshiListingResponse {
    markets: Vec<KalshiListingWireMarket>,
    cursor: String,
}

#[derive(Deserialize)]
struct KalshiListingWireMarket {
    ticker: String,
    open_time: String,
    close_time: String,
}

fn parse_kalshi_listing(body: &str) -> Result<Vec<KalshiListingMarket>, PredictionLiveError> {
    let response: KalshiListingResponse = serde_json::from_str(body).map_err(|error| {
        PredictionLiveError::InvalidDiscovery(format!("Kalshi listing is invalid JSON: {error}"))
    })?;
    if !response.cursor.is_empty() {
        return Err(PredictionLiveError::InvalidDiscovery(
            "Kalshi 1000-row listing unexpectedly requires pagination".to_owned(),
        ));
    }
    response
        .markets
        .into_iter()
        .map(|market| {
            crate::kalshi::validate_market_ticker(&market.ticker)
                .map_err(|error| PredictionLiveError::InvalidDiscovery(error.to_string()))?;
            let open_time = parse_time(&market.open_time, "Kalshi open_time")?;
            let close_time = parse_time(&market.close_time, "Kalshi close_time")?;
            if close_time <= open_time {
                return Err(PredictionLiveError::InvalidDiscovery(
                    "Kalshi listing close_time does not follow open_time".to_owned(),
                ));
            }
            Ok(KalshiListingMarket {
                ticker: market.ticker,
                open_time,
                close_time,
            })
        })
        .collect()
}

struct PolymarketMetadata {
    market_id: String,
    close_time: DateTime<Utc>,
}

#[derive(Deserialize)]
struct PolymarketWireMarket {
    id: String,
    slug: String,
    #[serde(rename = "endDate")]
    end_date: String,
}

fn parse_polymarket_metadata(
    body: &str,
    slug: &str,
) -> Result<Option<PolymarketMetadata>, PredictionLiveError> {
    let markets: Vec<PolymarketWireMarket> = serde_json::from_str(body).map_err(|error| {
        PredictionLiveError::InvalidDiscovery(format!("Gamma response is invalid JSON: {error}"))
    })?;
    match markets.as_slice() {
        [] => Ok(None),
        [market] if market.slug == slug && !market.id.is_empty() && market.id.len() <= 256 => {
            Ok(Some(PolymarketMetadata {
                market_id: market.id.clone(),
                close_time: parse_time(&market.end_date, "Polymarket endDate")?,
            }))
        }
        _ => Err(PredictionLiveError::InvalidDiscovery(
            "Gamma exact-slug result is not one matching market".to_owned(),
        )),
    }
}

/// Whether a mid-market socket teardown admits genuine socket-connectivity
/// transitions (`Disconnected` + `Reconnecting`, and later `Connected`). Only a
/// real disconnect that will attempt a reconnect does. Every controlled close is
/// silent: a scheduled rollover or shutdown/deadline breaks out before this
/// decision (`exit_run`/`exit_market`), a naturally-closed or past-deadline
/// market fails `will_reconnect`, and an operator-inflicted induced-reconnect
/// diagnostic teardown is excluded by `induced_teardown`. Pure so the gating rule
/// is unit-tested directly; the end-to-end socket path is network-bound.
const fn admits_socket_disconnect(
    exit_run: bool,
    exit_market: bool,
    induced_teardown: bool,
    will_reconnect: bool,
) -> bool {
    !exit_run && !exit_market && !induced_teardown && will_reconnect
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PredictionDispatchControl {
    Continue,
    RediscoverMarket,
}

fn dispatch_record<H>(
    adapter: &mut ReplayAdapter,
    record: RawRecord,
    generation: u64,
    logical_receipt_ns: u64,
    runtime: (
        &ProcessClock,
        &mut ContinuousSourceSequencer,
        &mut PredictionLiveCounters,
        &mut H,
    ),
) -> Result<PredictionDispatchControl, PredictionLiveError>
where
    H: FnMut(PredictionLiveAdmission) -> Result<(), PredictionLiveError>,
{
    let (clock, source_sequencer, counters, handle) = runtime;
    counters.records = checked_add(counters.records, 1, || PredictionLiveError::Counter)?;
    let mut outcome = adapter.ingest(&record)?;
    source_sequencer.assign_all(&mut outcome.events)?;
    let rediscover_market = requires_market_rediscovery(&outcome);
    count_outcome(counters, &outcome)?;
    if !outcome.events.is_empty() || !outcome.notices.is_empty() {
        handle(PredictionLiveAdmission::MarketOutcome {
            generation,
            outcome,
            logical_receipt_ns,
            now_ns: clock.admission_ns(logical_receipt_ns)?,
        })?;
    }
    Ok(if rediscover_market {
        PredictionDispatchControl::RediscoverMarket
    } else {
        PredictionDispatchControl::Continue
    })
}

fn requires_market_rediscovery(outcome: &IngestOutcome) -> bool {
    outcome
        .notices
        .iter()
        .any(|notice| notice.recovery == Some(RecoveryAction::RediscoverPredictionMarket))
}

fn count_outcome(
    counters: &mut PredictionLiveCounters,
    outcome: &IngestOutcome,
) -> Result<(), PredictionLiveError> {
    counters.events = checked_add(
        counters.events,
        u64::try_from(outcome.events.len()).map_err(|_| PredictionLiveError::Counter)?,
        || PredictionLiveError::Counter,
    )?;
    counters.notices = checked_add(
        counters.notices,
        u64::try_from(outcome.notices.len()).map_err(|_| PredictionLiveError::Counter)?,
        || PredictionLiveError::Counter,
    )?;
    Ok(())
}

/// Ruling (global follow-up to E37d): admit one counted `UnsupportedMessage`
/// notice for the market socket when it meets an unexpected binary/continuation
/// frame, so the lane survives it. The caller then drops the socket and the
/// market reconnect ladder re-dials — the dedicated index socket and both spot
/// runners sharing the `try_join!` keep running and a session record is still
/// written at the deadline, rather than a hard error cancelling every sibling.
/// The notice rides the current generation as an auxiliary `MarketOutcome`. This
/// path is for transport-frame faults only: an adapter/dispatch fault on a
/// decoded text frame stays fatal, as do start-up refusals. Mirrors the base
/// runner's `admit_unsupported_message`.
fn admit_market_unsupported_message<H>(
    detail: impl Into<String>,
    generation: u64,
    logical_receipt_ns: u64,
    runtime: (&ProcessClock, &mut PredictionLiveCounters, &mut H),
) -> Result<(), PredictionLiveError>
where
    H: FnMut(PredictionLiveAdmission) -> Result<(), PredictionLiveError>,
{
    let (clock, counters, handle) = runtime;
    let outcome = IngestOutcome::notice(crate::FeedNotice::info(
        crate::FeedNoticeCode::UnsupportedMessage,
        detail,
    ));
    count_outcome(counters, &outcome)?;
    handle(PredictionLiveAdmission::MarketOutcome {
        generation,
        outcome,
        logical_receipt_ns,
        now_ns: clock.admission_ns(logical_receipt_ns)?,
    })?;
    Ok(())
}

fn logical_ns(clock: &ProcessClock, timestamp: DateTime<Utc>) -> Result<u64, PredictionLiveError> {
    let nanos = timestamp
        .signed_duration_since(clock.wall_start())
        .num_nanoseconds()
        .ok_or(PredictionLiveError::ClockRange)?;
    u64::try_from(nanos).map_err(|_| PredictionLiveError::ClockRange)
}

fn logical_deadline(clock: &ProcessClock, target_ns: u64) -> Result<Instant, PredictionLiveError> {
    let now_ns = clock.elapsed_ns()?;
    Ok(Instant::now() + Duration::from_nanos(target_ns.saturating_sub(now_ns)))
}

enum WaitResult {
    Retry,
    Deadline,
    Interrupted,
}

async fn wait_for_retry(
    retry_at: Option<DateTime<Utc>>,
    retry: Duration,
    deadline: Instant,
    shutdown: &mut ShutdownWait,
) -> Result<WaitResult, PredictionLiveError> {
    let delay = retry_at.map_or(retry, |target| {
        let now = DateTime::<Utc>::from(SystemTime::now());
        target
            .signed_duration_since(now)
            .to_std()
            .unwrap_or_default()
            .min(retry)
    });
    tokio::select! {
        result = shutdown.wait() => {
            match result.map_err(PredictionLiveError::Signal)? {
                LiveShutdownReason::NaturalDeadline => Ok(WaitResult::Deadline),
                LiveShutdownReason::OperatorSignal => Ok(WaitResult::Interrupted),
            }
        }
        () = sleep_until(deadline) => Ok(WaitResult::Deadline),
        () = sleep(delay) => Ok(WaitResult::Retry),
    }
}

fn parse_time(value: &str, field: &str) -> Result<DateTime<Utc>, PredictionLiveError> {
    DateTime::parse_from_rfc3339(value)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|error| {
            PredictionLiveError::InvalidDiscovery(format!("{field} is invalid: {error}"))
        })
}

/// Every terminal failure a prediction live run can end with. All of them stop the run.
#[derive(Debug, thiserror::Error)]
pub enum PredictionLiveError {
    /// Returned when option validation rejects the run shape, before any request.
    #[error("invalid prediction live options: {0}")]
    InvalidOptions(String),
    /// Returned when a discovery response or record fails parsing, validation, or the
    /// selection rules, including a selected market whose close is not in the future.
    #[error("invalid prediction discovery: {0}")]
    InvalidDiscovery(String),
    /// Returned when shared capture setup or the WebSocket transport fails.
    #[error(transparent)]
    Capture(#[from] CaptureError),
    /// Returned when a market adapter rejects an ingested record.
    #[error(transparent)]
    Adapter(#[from] crate::AdapterError),
    /// Returned when the shared process clock or live shutdown scaffolding fails.
    #[error(transparent)]
    Live(#[from] LiveError),
    /// Returned when Kalshi credentials are unusable or a signed request cannot be built.
    #[error(transparent)]
    KalshiAuth(#[from] crate::KalshiAuthError),
    /// Returned when installing the operating-system shutdown signal handler fails.
    #[error("shutdown signal failed: {0}")]
    Signal(std::io::Error),
    /// Returned when the run reaches its 100,000 WebSocket connect-attempt limit.
    #[error("prediction live connection-attempt limit reached")]
    ConnectionAttemptLimit,
    /// Returned when the provider sends a binary or raw-frame WebSocket message.
    #[error("provider sent an unexpected binary WebSocket message")]
    UnexpectedBinary,
    /// Returned when an induced-reconnect run finished without exactly one induced
    /// disconnect and at least two successful connections.
    #[error("fault exercise did not prove its expected behavior: {0}")]
    DiagnosticIncomplete(String),
    /// Returned when a logical instant falls outside the u64 nanosecond range.
    #[error("prediction live logical clock is outside the supported range")]
    ClockRange,
    /// Returned when a run counter or source sequence would overflow `u64`.
    #[error("prediction live counter exhausted")]
    Counter,
    /// Returned when the admission callback rejects a value the runner offered it.
    #[error("prediction admission boundary rejected input: {0}")]
    Publication(String),
}

impl From<PredictionLiveError> for LiveError {
    fn from(error: PredictionLiveError) -> Self {
        LiveError::Publication(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_sequences_continue_across_market_local_adapter_resets() {
        fn event(source_id: &str, adapter_sequence: u64) -> SourceEvent {
            SourceEvent {
                schema_version: 1,
                source_id: source_id.to_owned(),
                source_version: "1.0.0".to_owned(),
                publisher_epoch: "process-epoch".to_owned(),
                sequence: adapter_sequence,
                event_time: "2026-08-27T12:00:00Z".to_owned(),
                received_at: "2026-08-27T12:00:00Z".to_owned(),
                values: BTreeMap::new(),
            }
        }

        let mut sequencer = ContinuousSourceSequencer::default();
        let mut first_market = vec![event("quotes", 0), event("books", 0), event("quotes", 1)];
        sequencer
            .assign_all(&mut first_market)
            .expect("first market sequences assign");
        assert_eq!(
            first_market
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            vec![0, 0, 1]
        );

        let mut successor_market = vec![event("quotes", 0), event("books", 0)];
        sequencer
            .assign_all(&mut successor_market)
            .expect("successor market sequences assign");
        assert_eq!(successor_market[0].sequence, 2);
        assert_eq!(successor_market[1].sequence, 1);
    }

    #[test]
    fn unbound_discovery_failures_do_not_duplicate_market_departure() {
        let mut market_bound = false;
        let mut last_failure_reason = None;

        assert!(!begin_unavailability_if_bound(
            &mut market_bound,
            &mut last_failure_reason,
            MarketTransitionReason::DiscoveryUnreachable,
        ));
        assert!(!begin_unavailability_if_bound(
            &mut market_bound,
            &mut last_failure_reason,
            MarketTransitionReason::NextMarketUnlisted,
        ));

        market_bound = true;
        last_failure_reason = None;
        assert!(begin_unavailability_if_bound(
            &mut market_bound,
            &mut last_failure_reason,
            MarketTransitionReason::ScheduledRollover,
        ));
        assert!(!market_bound);
        assert!(!begin_unavailability_if_bound(
            &mut market_bound,
            &mut last_failure_reason,
            MarketTransitionReason::NextMarketUnlisted,
        ));
    }

    #[test]
    fn discovery_record_hash_and_selection_are_closed() {
        let mut attempt = PredictionDiscoveryAttempt {
            schema_version: 1,
            venue: PredictionMarketVenue::Kalshi,
            stage: PredictionDiscoveryStage::CurrentMarket,
            request_path: "/trade-api/v2/markets?series_ticker=KXBTC15M".to_owned(),
            retrieved_at: "2026-08-15T17:00:00Z".to_owned(),
            http_status: Some(200),
            response_body_encoding: PredictionDiscoveryBodyEncoding::Utf8,
            response_body_sha256: sha256_hex(br#"{"markets":[]}"#),
            response_body: r#"{"markets":[]}"#.to_owned(),
            result: PredictionDiscoveryResult::Unlisted,
            selected_market_id: None,
        };
        attempt
            .validate()
            .expect("closed discovery record validates");
        attempt.selected_market_id = Some("KXBTC15M-26AUG151315-15".to_owned());
        assert!(attempt.validate().is_err());
    }

    #[test]
    fn invalid_discovery_evidence_is_capped_and_rehashed() {
        let mut attempt = PredictionDiscoveryAttempt {
            schema_version: 1,
            venue: PredictionMarketVenue::Kalshi,
            stage: PredictionDiscoveryStage::CurrentMarket,
            request_path: "/trade-api/v2/markets?series_ticker=KXBTC15M".to_owned(),
            retrieved_at: "2026-08-15T17:00:00Z".to_owned(),
            http_status: Some(500),
            response_body_encoding: PredictionDiscoveryBodyEncoding::Utf8,
            response_body_sha256: sha256_hex(&vec![b'x'; 100_000]),
            response_body: "x".repeat(100_000),
            result: PredictionDiscoveryResult::Invalid,
            selected_market_id: None,
        };

        attempt
            .truncate_error_evidence()
            .expect("invalid evidence truncates");

        assert_eq!(
            attempt.response_body.len(),
            MAX_DISCOVERY_ERROR_EVIDENCE_BYTES
        );
        assert_eq!(
            attempt.response_body_sha256,
            sha256_hex(attempt.response_body.as_bytes())
        );
        attempt.validate().expect("truncated evidence validates");
    }

    #[test]
    fn socket_disconnect_gating_admits_only_genuine_mid_market_drops() {
        // A genuine mid-market disconnect that will reconnect is the only case
        // that admits connectivity transitions.
        assert!(admits_socket_disconnect(false, false, false, true));

        // Every controlled close is silent. A run/market exit (rollover, shutdown,
        // deadline) — even if the reconnect condition still holds — admits nothing.
        assert!(!admits_socket_disconnect(true, false, false, true));
        assert!(!admits_socket_disconnect(false, true, false, true));
        // A naturally-closed or past-deadline market fails `will_reconnect`.
        assert!(!admits_socket_disconnect(false, false, false, false));
        // An operator-inflicted induced-reconnect teardown emits nothing, even
        // though it breaks the read loop with neither exit flag set and will
        // reconnect (the F2 case): the diagnostic must not fake socket-sickness.
        assert!(!admits_socket_disconnect(false, false, true, true));
        // The induced flag dominates regardless of the other flags.
        for will_reconnect in [true, false] {
            assert!(!admits_socket_disconnect(
                false,
                false,
                true,
                will_reconnect
            ));
        }
    }

    #[test]
    fn restart_successor_is_recovery_not_completed_rollover() {
        let reason = bind_reason(Some("departing"), true);
        assert_eq!(reason, MarketTransitionReason::RecoveryBind);
        assert!(!counts_completed_rollover(reason));

        let live_reason = bind_reason(Some("departing"), false);
        assert_eq!(live_reason, MarketTransitionReason::ScheduledRollover);
        assert!(counts_completed_rollover(live_reason));
    }

    #[test]
    fn provider_lifecycle_recovery_requests_market_rediscovery() {
        let outcome = IngestOutcome {
            events: Vec::new(),
            notices: vec![crate::FeedNotice {
                code: crate::FeedNoticeCode::StateInconsistent,
                detail: "lifecycle changed".to_owned(),
                recovery: Some(RecoveryAction::RediscoverPredictionMarket),
                subscription: None,
            }],
        };
        assert!(requires_market_rediscovery(&outcome));
        assert!(!requires_market_rediscovery(&IngestOutcome::default()));
    }

    #[test]
    fn departing_market_rediscovery_is_a_retry_condition() {
        assert!(is_departing_market_rediscovery(
            MarketTransitionReason::ScheduledRollover,
            Some("departing"),
            "departing",
        ));
        assert!(!is_departing_market_rediscovery(
            MarketTransitionReason::RecoveryBind,
            Some("departing"),
            "successor",
        ));
    }

    #[test]
    fn kalshi_listing_selection_uses_window_fields_not_ticker_text() {
        let body = r#"{
            "markets":[{
                "ticker":"KXBTC15M-26AUG151315-15",
                "open_time":"2026-08-15T17:00:00Z",
                "close_time":"2026-08-15T17:15:00Z"
            }],
            "cursor":""
        }"#;
        let markets = parse_kalshi_listing(body).expect("listing parses");
        assert_eq!(markets.len(), 1);
        assert_eq!(
            markets[0].open_time,
            parse_time("2026-08-15T17:00:00Z", "test").expect("time parses")
        );
    }

    #[test]
    fn polymarket_metadata_requires_the_exact_derived_slug() {
        let body =
            r#"[{"id":"42","slug":"btc-updown-5m-1786813200","endDate":"2026-08-15T17:05:00Z"}]"#;
        let metadata = parse_polymarket_metadata(body, "btc-updown-5m-1786813200")
            .expect("response parses")
            .expect("market is present");
        assert_eq!(metadata.market_id, "42");
        assert!(parse_polymarket_metadata(body, "btc-updown-5m-1786813500").is_err());
    }

    /// E37d — a lane with no armed index family, and every non-Kalshi venue,
    /// opens no index socket and admits nothing, so a disarmed run stays
    /// byte-identical to one built before the dedicated index socket existed.
    #[tokio::test(flavor = "current_thread")]
    async fn disarmed_index_socket_opens_nothing() {
        for (venue, index_sockets) in [
            // Kalshi, but the gate armed no family.
            (PredictionMarketVenue::Kalshi, ArmedIndexSockets::NONE),
            // A non-Kalshi venue never carries index channels, even if a family
            // were somehow armed.
            (
                PredictionMarketVenue::Polymarket,
                ArmedIndexSockets::NONE.with("cfbrti"),
            ),
        ] {
            let mut admissions = 0_u32;
            let options = PredictionIndexSocketOptions::new(
                venue,
                Duration::from_secs(3600),
                "process-epoch",
                index_sockets,
            );
            let report =
                run_prediction_index_socket(options, ProcessClock::start(), |_admission| {
                    admissions += 1;
                    Ok(())
                })
                .await
                .expect("a disarmed index socket returns cleanly");
            assert_eq!(
                admissions, 0,
                "{venue:?}: no admission crosses the boundary"
            );
            assert_eq!(
                report.connection_attempts, 0,
                "{venue:?}: no index socket is opened"
            );
            assert_eq!(report.events, 0, "{venue:?}: no index event is emitted");
            assert_eq!(report.venue, venue);
            assert!(!report.interrupted);
        }
    }

    /// E37d (MAJOR 1) — the auxiliary index socket shares the market lane's
    /// `try_join!`, so a binary/continuation frame must degrade to a drop (reconnect,
    /// families go stale) rather than a hard error. Since the global E37d follow-up
    /// the market and spot sockets degrade the same frame to a counted-notice drop
    /// too, so no socket cancels the lane. A close, transport error, or read EOF
    /// drops as always; text/ping/pong are processed inline. This proves the
    /// classification the read loop routes on; the socket path itself is network-bound.
    #[test]
    fn index_read_downgrades_binary_and_transport_faults_to_drops() {
        // A frame that aborts the primary lane is only a drop here.
        assert!(index_read_drops(&Some(Ok(Message::Binary(
            Vec::new().into()
        )))));
        // Benign end-of-connection variants drop as on every socket.
        assert!(index_read_drops(&Some(Ok(Message::Close(None)))));
        assert!(index_read_drops(&Some(Err(
            tokio_tungstenite::tungstenite::Error::ConnectionClosed
        ))));
        assert!(index_read_drops(&None));
        // Data and control frames the loop handles inline are never drops.
        assert!(!index_read_drops(&Some(Ok(Message::text("{}")))));
        assert!(!index_read_drops(&Some(Ok(Message::Ping(
            Vec::new().into()
        )))));
        assert!(!index_read_drops(&Some(Ok(Message::Pong(
            Vec::new().into()
        )))));
    }

    /// Ruling (global follow-up to E37d), market socket: an unexpected
    /// binary/continuation frame degrades to a counted `UnsupportedMessage` notice
    /// and drops the socket instead of returning `UnexpectedBinary`. The fatal
    /// return cancelled the dedicated index socket and both spot runners through
    /// the shared `try_join!` and wrote no session record; the counted-notice drop
    /// keeps every sibling arm running so the market runner reaches its own
    /// disconnect record and reconnect ladder, and the run still writes a session
    /// record at the deadline. The socket read loop is network-bound, so this pins
    /// the admission the loop routes the binary arm through: it admits (never
    /// errors) and counts exactly one notice on the current generation.
    #[test]
    fn a_market_binary_frame_degrades_to_a_counted_notice_not_a_fatal_cancel() {
        let clock = ProcessClock::start();
        let receipt = clock.elapsed_ns().expect("clock reads");
        let mut counters = PredictionLiveCounters::default();
        let mut admissions: Vec<(u64, usize, usize)> = Vec::new();
        let mut notice_codes: Vec<crate::FeedNoticeCode> = Vec::new();
        {
            let mut handle = |admission: PredictionLiveAdmission| {
                if let PredictionLiveAdmission::MarketOutcome {
                    generation,
                    outcome,
                    ..
                } = admission
                {
                    admissions.push((generation, outcome.events.len(), outcome.notices.len()));
                    notice_codes.extend(outcome.notices.iter().map(|notice| notice.code));
                }
                Ok(())
            };

            admit_market_unsupported_message(
                "unexpected binary/continuation frame on kalshi-market",
                7,
                receipt,
                (&clock, &mut counters, &mut handle),
            )
            .expect("a market binary frame degrades to a counted notice, never a fatal cancel");
        }

        // Exactly one auxiliary MarketOutcome, on the current generation, carrying
        // one UnsupportedMessage notice and no manufactured event.
        assert_eq!(admissions, vec![(7, 0, 1)]);
        assert_eq!(
            notice_codes,
            vec![crate::FeedNoticeCode::UnsupportedMessage]
        );
        assert_eq!(counters.notices, 1, "the drop is counted");
        assert_eq!(counters.events, 0, "no event is manufactured");
    }

    /// E37d (MAJOR 1) — start-up refusals stay fatal: an armed Kalshi lane whose
    /// credentials are absent refuses before any socket work, so the run fails
    /// closed rather than opening an unauthenticated index socket. (Only faults
    /// *after* start-up are degraded to drops.)
    #[tokio::test(flavor = "current_thread")]
    async fn armed_index_socket_without_credentials_refuses() {
        // The workspace forbids `unsafe_code`, so the environment cannot be
        // mutated here; assert the refusal only when the default test environment
        // actually lacks the credentials (the CI norm).
        if std::env::var_os("KALSHI_API_KEY_ID").is_some()
            || std::env::var_os("KALSHI_PRIVATE_KEY_PATH").is_some()
        {
            return;
        }
        let options = PredictionIndexSocketOptions::new(
            PredictionMarketVenue::Kalshi,
            Duration::from_secs(3600),
            "process-epoch",
            ArmedIndexSockets::NONE.with("cfbrti"),
        );
        let mut admissions = 0_u32;
        let error = run_prediction_index_socket(options, ProcessClock::start(), |_admission| {
            admissions += 1;
            Ok(())
        })
        .await
        .expect_err("an armed lane with no credentials refuses at start-up");
        assert!(
            matches!(error, PredictionLiveError::KalshiAuth(_)),
            "a missing credential is the fatal refusal, got {error:?}"
        );
        assert_eq!(
            admissions, 0,
            "no admission crosses the boundary on refusal"
        );
    }
}
