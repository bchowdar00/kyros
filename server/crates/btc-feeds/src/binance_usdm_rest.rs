//! Binance USD-M bounded REST pollers: open interest, its five-minute history,
//! the venue-declared funding interval, and the premium index.
//!
//! # Documented, not observed (ruling R1)
//!
//! No USD-M REST body has been captured yet. Every field spelling and every
//! bound below comes from `A3-binance-usdm-feeds.md` §5, and each is corrected
//! against the wave-A capture before the sources are adopted. In particular
//! probe **P5** — whether BTCUSDT appears in `/fapi/v1/fundingInfo` at all — is
//! open, and this module fails closed on its absence: no event, an informational
//! notice, and no invented default interval, cap or floor.
//!
//! # Why replay needs no network
//!
//! Each poll writes two capture records: an `OutboundText` request descriptor
//! naming the endpoint, then the verbatim response body as `InboundText`. The
//! descriptor is what lets a replay dispatch a body to the right parser without
//! guessing from its shape, and it is the same idiom the NBA ESPN poll capture
//! uses. A non-2xx response is counted by the capture loop and recorded as
//! nothing at all (the Chainlink rule): a body we never received is never
//! fabricated, and the dependent identity ages toward its staleness bound.

use std::collections::BTreeMap;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::json;

use crate::adapter::bounded_token;
use crate::binance_usdm::{BinanceUsdmAdapter, payload_error, require_symbol};
use crate::decimal::{validate_decimal, validate_signed_decimal};
use crate::event_window::millis_to_rfc3339;
use crate::{AdapterError, FeedNotice, FeedNoticeCode, IngestOutcome, RecoveryAction};

/// USD-M REST host. Distinct from the spot market-data mirror: there is no
/// documented `.vision` mirror for USD-M (probe P3 covers reachability).
pub(crate) const REST_HOST: &str = "https://fapi.binance.com";

pub(crate) const OPEN_INTEREST_PATH: &str = "/fapi/v1/openInterest?symbol=BTCUSDT";
pub(crate) const OPEN_INTEREST_HIST_PATH: &str =
    "/futures/data/openInterestHist?symbol=BTCUSDT&period=5m&limit=30";
pub(crate) const FUNDING_INFO_PATH: &str = "/fapi/v1/fundingInfo";
pub(crate) const PREMIUM_INDEX_PATH: &str = "/fapi/v1/premiumIndex?symbol=BTCUSDT";

/// Poll cadences (ruling R5 for the five-second open-interest poll).
pub(crate) const OPEN_INTEREST_CADENCE: Duration = Duration::from_secs(5);
pub(crate) const OPEN_INTEREST_HIST_CADENCE: Duration = Duration::from_secs(300);
pub(crate) const FUNDING_INFO_CADENCE: Duration = Duration::from_secs(3600);
pub(crate) const PREMIUM_INDEX_CADENCE: Duration = Duration::from_secs(1);

/// Per-request HTTP timeout and the bounded body read.
///
/// One total-request bound rather than a separate connect bound: the shared
/// `secure_http` client is constructed once for every provider, and the whole
/// poll — connect, TLS, request, bounded body — is covered by this ceiling.
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

/// Per-session request budget across all four USD-M REST pollers. Binance's
/// documented USD-M REST ceiling is 2 400 request-weight per minute per IP, and
/// each of these four endpoints is weight 1 (A3 §2.3), so 120 req/min is a
/// deliberate 5 % share of that ceiling. Steady state spends far less —
/// premium-index 60/min + open-interest ≤ 12/min + open-interest-hist ≤ 2/min +
/// funding-info ≤ 1/min ≈ 75/min — leaving headroom for startup bursts while the
/// 1 s premium-index poll gets its full cadence. The earlier shared 15/min
/// self-budget starved that poll after ~15 s; S-RULINGS F2/E50 flagged it for
/// exactly this raise.
pub(crate) const MAX_REQUESTS_PER_MINUTE: u32 = 120;

/// Used-weight header, and the level at which a poller pauses itself.
pub(crate) const USED_WEIGHT_HEADER: &str = "x-mbx-used-weight-1m";
pub(crate) const USED_WEIGHT_PAUSE_THRESHOLD: u32 = 1000;

const OPEN_INTEREST_SOURCE_ID: &str = "binance.usdm.btcusdt.open-interest";
const OPEN_INTEREST_HIST_SOURCE_ID: &str = "binance.usdm.btcusdt.open-interest-hist";
const FUNDING_INFO_SOURCE_ID: &str = "binance.usdm.btcusdt.funding-info";
const PREMIUM_INDEX_SOURCE_ID: &str = "binance.usdm.btcusdt.premium-index";

/// Maximum accepted rows in one `openInterestHist` body.
pub(crate) const MAX_OI_HIST_ROWS: usize = 1_024;

/// Declared `period` of the history series. A constant, because the request
/// pins it; it is emitted so a consumer never has to know the request.
const OI_HIST_PERIOD: &str = "5m";

/// Which endpoint a recorded request descriptor named.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinanceUsdmRestEndpoint {
    /// `/fapi/v1/openInterest`.
    OpenInterest,
    /// `/futures/data/openInterestHist`.
    OpenInterestHist,
    /// `/fapi/v1/fundingInfo`.
    FundingInfo,
    /// `/fapi/v1/premiumIndex`.
    PremiumIndex,
}

impl BinanceUsdmRestEndpoint {
    /// Stable token written into the request descriptor.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::OpenInterest => "open_interest",
            Self::OpenInterestHist => "open_interest_hist",
            Self::FundingInfo => "funding_info",
            Self::PremiumIndex => "premium_index",
        }
    }

    /// Path and query this endpoint is polled at.
    #[must_use]
    pub const fn path(self) -> &'static str {
        match self {
            Self::OpenInterest => OPEN_INTEREST_PATH,
            Self::OpenInterestHist => OPEN_INTEREST_HIST_PATH,
            Self::FundingInfo => FUNDING_INFO_PATH,
            Self::PremiumIndex => PREMIUM_INDEX_PATH,
        }
    }

    /// Cadence this endpoint is polled on.
    #[must_use]
    pub const fn cadence(self) -> Duration {
        match self {
            Self::OpenInterest => OPEN_INTEREST_CADENCE,
            Self::OpenInterestHist => OPEN_INTEREST_HIST_CADENCE,
            Self::FundingInfo => FUNDING_INFO_CADENCE,
            Self::PremiumIndex => PREMIUM_INDEX_CADENCE,
        }
    }

    fn from_token(token: &str) -> Option<Self> {
        match token {
            "open_interest" => Some(Self::OpenInterest),
            "open_interest_hist" => Some(Self::OpenInterestHist),
            "funding_info" => Some(Self::FundingInfo),
            "premium_index" => Some(Self::PremiumIndex),
            _ => None,
        }
    }
}

/// The exact `OutboundText` payload recorded before a poll.
///
/// It is compiled here and nowhere else, so the capture and the replay dispatch
/// cannot drift apart.
#[must_use]
pub fn request_descriptor(endpoint: BinanceUsdmRestEndpoint, poll: u64) -> String {
    json!({
        "request": endpoint.token(),
        "method": "GET",
        "url": format!("{REST_HOST}{}", endpoint.path()),
        "poll": poll
    })
    .to_string()
}

/// Ordering cursors and per-source sequence counters for the REST identities.
#[derive(Debug, Default)]
pub struct BinanceUsdmRestState {
    next_open_interest_sequence: u64,
    next_open_interest_hist_sequence: u64,
    next_funding_info_sequence: u64,
    next_premium_index_sequence: u64,
    /// Endpoint named by the most recent request descriptor. A response with no
    /// preceding descriptor cannot be attributed and is refused.
    pending: Option<BinanceUsdmRestEndpoint>,
    /// `time` of the last admitted open-interest snapshot.
    last_open_interest_millis: Option<i64>,
    /// Ruling E7: a strictly increasing ordinal over admitted open-interest
    /// polls, so two polls reporting an identical open interest are still
    /// distinguishable. Derived from the record stream alone, so replay
    /// reproduces it exactly.
    open_interest_poll_sequence: u64,
    /// Highest `timestamp` admitted from the history series. The endpoint is
    /// polled with deliberate overlap, so only strictly newer rows are emitted.
    open_interest_hist_watermark: Option<i64>,
    /// `time` of the last admitted premium-index snapshot.
    last_premium_index_millis: Option<i64>,
}

impl BinanceUsdmRestState {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Reset the connection-local state of a poll session.
    ///
    /// The history watermark is connection-local like every other cursor: a
    /// fresh session re-reads the overlapping window and re-admits nothing it
    /// has not already passed, because the watermark starts empty and the first
    /// body seeds it.
    pub(crate) fn reset(&mut self) {
        self.pending = None;
        self.last_open_interest_millis = None;
        self.open_interest_hist_watermark = None;
        self.last_premium_index_millis = None;
    }

    pub(crate) fn sequence_slot(&mut self, source_id: &str) -> Option<&mut u64> {
        match source_id {
            OPEN_INTEREST_SOURCE_ID => Some(&mut self.next_open_interest_sequence),
            OPEN_INTEREST_HIST_SOURCE_ID => Some(&mut self.next_open_interest_hist_sequence),
            FUNDING_INFO_SOURCE_ID => Some(&mut self.next_funding_info_sequence),
            PREMIUM_INDEX_SOURCE_ID => Some(&mut self.next_premium_index_sequence),
            _ => None,
        }
    }
}

/// Remember which endpoint the request descriptor named.
pub(crate) fn observe_request(
    adapter: &mut BinanceUsdmAdapter,
    payload: &str,
) -> Result<IngestOutcome, AdapterError> {
    let descriptor: RequestDescriptor = serde_json::from_str(payload).map_err(payload_error)?;
    let endpoint = BinanceUsdmRestEndpoint::from_token(&descriptor.request).ok_or_else(|| {
        AdapterError::InvalidPayload(format!(
            "unknown Binance USD-M REST request `{}`",
            bounded_token(&descriptor.request)
        ))
    })?;
    adapter.rest_state().pending = Some(endpoint);
    Ok(IngestOutcome::default())
}

/// Parse one recorded response body against the endpoint its descriptor named.
pub(crate) fn ingest_response(
    adapter: &mut BinanceUsdmAdapter,
    record: &crate::RawRecord,
    payload: &str,
) -> Result<IngestOutcome, AdapterError> {
    if payload.len() > MAX_RESPONSE_BYTES {
        return Err(AdapterError::InvalidPayload(format!(
            "Binance USD-M REST body is {} bytes; maximum is {MAX_RESPONSE_BYTES}",
            payload.len()
        )));
    }
    let endpoint = adapter.rest_state().pending.take().ok_or_else(|| {
        AdapterError::InvalidRecord(
            "Binance USD-M REST response has no preceding request descriptor".to_owned(),
        )
    })?;
    match endpoint {
        BinanceUsdmRestEndpoint::OpenInterest => open_interest(adapter, record, payload),
        BinanceUsdmRestEndpoint::OpenInterestHist => open_interest_hist(adapter, record, payload),
        BinanceUsdmRestEndpoint::FundingInfo => funding_info(adapter, record, payload),
        BinanceUsdmRestEndpoint::PremiumIndex => premium_index(adapter, record, payload),
    }
}

fn open_interest(
    adapter: &mut BinanceUsdmAdapter,
    record: &crate::RawRecord,
    payload: &str,
) -> Result<IngestOutcome, AdapterError> {
    let body: OpenInterestBody = match serde_json::from_str(payload) {
        Ok(body) => body,
        Err(error) => {
            // A malformed body is a venue-side condition on a poll feed, not a
            // capture we can refuse to replay: it is a recovery notice and the
            // dependent identity ages rather than reading a stale value as fresh.
            return Ok(IngestOutcome::notice(FeedNotice::recovery(
                FeedNoticeCode::StateInconsistent,
                format!(
                    "Binance USD-M open-interest body did not parse: {}",
                    bounded_token(&error.to_string())
                ),
                RecoveryAction::Reconnect,
            )));
        }
    };
    if body.symbol != "BTCUSDT" {
        return Ok(IngestOutcome::notice(FeedNotice::recovery(
            FeedNoticeCode::StateInconsistent,
            format!(
                "Binance USD-M open-interest body named symbol `{}`",
                bounded_token(&body.symbol)
            ),
            RecoveryAction::Reconnect,
        )));
    }
    let open_interest = validate_decimal(&body.open_interest, "open_interest_base")?;
    if open_interest.is_sign_negative() {
        return Err(AdapterError::InvalidPayload(
            "Binance USD-M open interest must not be negative".to_owned(),
        ));
    }
    let event_time = millis_to_rfc3339(body.time, "event_time")?;

    if let Some(previous) = adapter.rest_state().last_open_interest_millis {
        if body.time == previous {
            // The venue republishes an unchanged snapshot between its own
            // updates, so an equal timestamp is a duplicate poll, not new state.
            return Ok(IngestOutcome::notice(FeedNotice::info(
                FeedNoticeCode::Duplicate,
                format!("duplicate Binance USD-M open-interest timestamp {previous} ignored"),
            )));
        }
        if body.time < previous {
            return Ok(IngestOutcome::notice(FeedNotice::info(
                FeedNoticeCode::OutOfOrder,
                format!(
                    "out-of-order Binance USD-M open-interest timestamp {} after {previous} ignored",
                    body.time
                ),
            )));
        }
    }
    let poll_sequence = {
        let state = adapter.rest_state();
        state.last_open_interest_millis = Some(body.time);
        let current = state.open_interest_poll_sequence;
        state.open_interest_poll_sequence = current
            .checked_add(1)
            .ok_or(AdapterError::SequenceExhausted)?;
        current
    };

    let values = BTreeMap::from([
        ("instrument".to_owned(), json!("BTCUSDT")),
        ("open_interest_base".to_owned(), json!(body.open_interest)),
        ("poll_sequence".to_owned(), json!(poll_sequence)),
        ("event_time".to_owned(), json!(event_time)),
    ]);
    let event = adapter.source_event(
        OPEN_INTEREST_SOURCE_ID,
        event_time,
        record.received_at.clone(),
        values,
    )?;
    Ok(IngestOutcome {
        events: vec![event],
        notices: Vec::new(),
    })
}

fn open_interest_hist(
    adapter: &mut BinanceUsdmAdapter,
    record: &crate::RawRecord,
    payload: &str,
) -> Result<IngestOutcome, AdapterError> {
    let rows: Vec<OpenInterestHistRow> = serde_json::from_str(payload).map_err(payload_error)?;
    if rows.len() > MAX_OI_HIST_ROWS {
        return Err(AdapterError::InvalidPayload(format!(
            "Binance USD-M open-interest history has {} rows; maximum is {MAX_OI_HIST_ROWS}",
            rows.len()
        )));
    }
    if rows.is_empty() {
        return Ok(IngestOutcome::notice(FeedNotice::info(
            FeedNoticeCode::StateInconsistent,
            "Binance USD-M open-interest history carried no rows; skipped without normalized output",
        )));
    }
    // Every row is validated before any is admitted, so a malformed tail cannot
    // leave a half-applied watermark behind.
    let mut admitted = Vec::new();
    for row in &rows {
        require_symbol(&row.symbol)?;
        validate_decimal(&row.sum_open_interest, "sum_open_interest_base")?;
        validate_decimal(&row.sum_open_interest_value, "sum_open_interest_value_usd")?;
        let event_time = millis_to_rfc3339(row.timestamp, "event_time")?;
        // Overlap is deliberate: the poll asks for thirty rows every five
        // minutes, so only rows strictly newer than the watermark are new facts.
        if adapter
            .rest_state()
            .open_interest_hist_watermark
            .is_some_and(|watermark| row.timestamp <= watermark)
        {
            continue;
        }
        admitted.push((row, event_time));
    }
    if admitted.is_empty() {
        return Ok(IngestOutcome::notice(FeedNotice::info(
            FeedNoticeCode::Duplicate,
            "Binance USD-M open-interest history repeated only rows already admitted",
        )));
    }
    let row_count = u32::try_from(admitted.len()).map_err(|_| {
        AdapterError::InvalidPayload("open-interest history row_count exceeds u32".to_owned())
    })?;
    let row_group = format!(
        "{}:{}:{}",
        record.stream.as_str(),
        record.connection_epoch,
        record.ordinal
    );
    let newest = admitted
        .iter()
        .map(|(row, _)| row.timestamp)
        .max()
        .unwrap_or_default();
    let mut events = Vec::with_capacity(admitted.len());
    for (index, (row, event_time)) in admitted.into_iter().enumerate() {
        let row_index = u32::try_from(index).map_err(|_| {
            AdapterError::InvalidPayload("open-interest history row_index exceeds u32".to_owned())
        })?;
        let values = BTreeMap::from([
            ("instrument".to_owned(), json!("BTCUSDT")),
            ("row_group".to_owned(), json!(row_group)),
            ("row_index".to_owned(), json!(row_index)),
            ("row_count".to_owned(), json!(row_count)),
            ("period".to_owned(), json!(OI_HIST_PERIOD)),
            (
                "sum_open_interest_base".to_owned(),
                json!(row.sum_open_interest),
            ),
            (
                "sum_open_interest_value_usd".to_owned(),
                json!(row.sum_open_interest_value),
            ),
            ("event_time".to_owned(), json!(event_time)),
        ]);
        events.push(adapter.source_event(
            OPEN_INTEREST_HIST_SOURCE_ID,
            event_time.clone(),
            record.received_at.clone(),
            values,
        )?);
    }
    adapter.rest_state().open_interest_hist_watermark = Some(newest);
    Ok(IngestOutcome {
        events,
        notices: Vec::new(),
    })
}

fn funding_info(
    adapter: &mut BinanceUsdmAdapter,
    record: &crate::RawRecord,
    payload: &str,
) -> Result<IngestOutcome, AdapterError> {
    let rows: Vec<FundingInfoRow> = serde_json::from_str(payload).map_err(payload_error)?;
    let Some(row) = rows.iter().find(|row| row.symbol == "BTCUSDT") else {
        // Probe P5, fail-closed: the endpoint documents only symbols with
        // non-default settings, so BTCUSDT may legitimately be absent. NO
        // default interval, cap or floor is ever substituted — unknown stays
        // unknown, and `btc.binance.funding_state` stays non-ready.
        return Ok(IngestOutcome::notice(FeedNotice::info(
            FeedNoticeCode::StateInconsistent,
            "Binance USD-M funding info carried no BTCUSDT element; no funding interval is assumed",
        )));
    };
    if !(1..=24).contains(&row.funding_interval_hours) {
        return Err(AdapterError::InvalidPayload(format!(
            "Binance USD-M funding interval {} hours is outside 1..=24",
            row.funding_interval_hours
        )));
    }
    // Cap and floor are SIGNED fractions per funding interval — the floor is
    // normally negative — so they keep their sign verbatim.
    validate_signed_decimal(&row.adjusted_funding_rate_cap, "adjusted_funding_rate_cap")?;
    validate_signed_decimal(
        &row.adjusted_funding_rate_floor,
        "adjusted_funding_rate_floor",
    )?;
    // The body carries no venue clock, so the event time is our own receipt of
    // the polled response and the definition declares it internal.
    let event_time = record.received_at.clone();
    let values = BTreeMap::from([
        ("instrument".to_owned(), json!("BTCUSDT")),
        (
            "funding_interval_hours".to_owned(),
            json!(row.funding_interval_hours),
        ),
        (
            "adjusted_funding_rate_cap".to_owned(),
            json!(row.adjusted_funding_rate_cap),
        ),
        (
            "adjusted_funding_rate_floor".to_owned(),
            json!(row.adjusted_funding_rate_floor),
        ),
        ("event_time".to_owned(), json!(event_time)),
    ]);
    let event = adapter.source_event(
        FUNDING_INFO_SOURCE_ID,
        event_time,
        record.received_at.clone(),
        values,
    )?;
    Ok(IngestOutcome {
        events: vec![event],
        notices: Vec::new(),
    })
}

fn premium_index(
    adapter: &mut BinanceUsdmAdapter,
    record: &crate::RawRecord,
    payload: &str,
) -> Result<IngestOutcome, AdapterError> {
    let body: PremiumIndexBody = match serde_json::from_str(payload) {
        Ok(body) => body,
        Err(error) => {
            // A malformed body is a venue-side condition on a poll feed, not a
            // capture we can refuse to replay: it is a recovery notice and the
            // dependent identity ages rather than reading a stale value as fresh.
            return Ok(IngestOutcome::notice(FeedNotice::recovery(
                FeedNoticeCode::StateInconsistent,
                format!(
                    "Binance USD-M premium-index body did not parse: {}",
                    bounded_token(&error.to_string())
                ),
                RecoveryAction::Reconnect,
            )));
        }
    };
    if body.symbol != "BTCUSDT" {
        return Ok(IngestOutcome::notice(FeedNotice::recovery(
            FeedNoticeCode::StateInconsistent,
            format!(
                "Binance USD-M premium-index body named symbol `{}`",
                bounded_token(&body.symbol)
            ),
            RecoveryAction::Reconnect,
        )));
    }
    // Mark and index prices are strictly positive levels: validated like the
    // open-interest decimal, non-negative and canonical.
    let mark_price = validate_decimal(&body.mark_price, "mark_price")?;
    if mark_price.is_sign_negative() {
        return Err(AdapterError::InvalidPayload(
            "Binance USD-M mark price must not be negative".to_owned(),
        ));
    }
    let index_price = validate_decimal(&body.index_price, "index_price")?;
    if index_price.is_sign_negative() {
        return Err(AdapterError::InvalidPayload(
            "Binance USD-M index price must not be negative".to_owned(),
        ));
    }
    validate_decimal(&body.estimated_settle_price, "estimated_settle_price")?;
    // Funding and interest rates are SIGNED fractions and keep their sign.
    validate_signed_decimal(&body.last_funding_rate, "last_funding_rate")?;
    validate_signed_decimal(&body.interest_rate, "interest_rate")?;
    let next_funding_time = millis_to_rfc3339(body.next_funding_time, "next_funding_time")?;
    let event_time = millis_to_rfc3339(body.time, "event_time")?;

    if let Some(previous) = adapter.rest_state().last_premium_index_millis {
        if body.time == previous {
            // The venue republishes an unchanged snapshot between its own
            // updates, so an equal timestamp is a duplicate poll, not new state.
            return Ok(IngestOutcome::notice(FeedNotice::info(
                FeedNoticeCode::Duplicate,
                format!("duplicate Binance USD-M premium-index timestamp {previous} ignored"),
            )));
        }
        if body.time < previous {
            return Ok(IngestOutcome::notice(FeedNotice::info(
                FeedNoticeCode::OutOfOrder,
                format!(
                    "out-of-order Binance USD-M premium-index timestamp {} after {previous} ignored",
                    body.time
                ),
            )));
        }
    }
    adapter.rest_state().last_premium_index_millis = Some(body.time);

    let values = BTreeMap::from([
        ("instrument".to_owned(), json!("BTCUSDT")),
        ("mark_price".to_owned(), json!(body.mark_price)),
        ("index_price".to_owned(), json!(body.index_price)),
        (
            "estimated_settle_price".to_owned(),
            json!(body.estimated_settle_price),
        ),
        (
            "last_funding_rate".to_owned(),
            json!(body.last_funding_rate),
        ),
        ("interest_rate".to_owned(), json!(body.interest_rate)),
        ("next_funding_time".to_owned(), json!(next_funding_time)),
        ("event_time".to_owned(), json!(event_time)),
    ]);
    let event = adapter.source_event(
        PREMIUM_INDEX_SOURCE_ID,
        event_time,
        record.received_at.clone(),
        values,
    )?;
    Ok(IngestOutcome {
        events: vec![event],
        notices: Vec::new(),
    })
}

/// A whole-minute request budget shared by the three pollers.
///
/// Deterministic and clock-free: the caller passes the elapsed milliseconds it
/// already tracks, so the budget is unit-testable without sleeping and without
/// reading a clock on the parse path.
#[derive(Debug, Default)]
pub struct RestBudget {
    window_start_ms: u64,
    requests_in_window: u32,
}

impl RestBudget {
    /// A fresh budget.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            window_start_ms: 0,
            requests_in_window: 0,
        }
    }

    /// Claim one request at `elapsed_ms`. `false` means the caller must skip
    /// this tick: the minute's allowance is spent.
    pub fn try_claim(&mut self, elapsed_ms: u64) -> bool {
        // The window is anchored at its FIRST claim, not at zero: anchoring at
        // process start would make the opening window arbitrarily short.
        if self.requests_in_window == 0 || elapsed_ms.saturating_sub(self.window_start_ms) >= 60_000
        {
            self.window_start_ms = elapsed_ms;
            self.requests_in_window = 0;
        }
        if self.requests_in_window >= MAX_REQUESTS_PER_MINUTE {
            return false;
        }
        self.requests_in_window = self.requests_in_window.saturating_add(1);
        true
    }

    /// Requests already claimed in the current window.
    #[must_use]
    pub const fn claimed(&self) -> u32 {
        self.requests_in_window
    }
}

/// The ban-avoidance floor: the pause when the venue told us nothing usable, and
/// the ceiling on the sub-floor `Retry-After` escalation ([`next_delay`] rule 2).
///
/// A poll cadence is not a rate-limit backoff. Resuming five seconds after a
/// 418 — which is what falling back to the open-interest cadence did — puts up
/// to fifteen requests a minute against an endpoint that has just banned us,
/// which is precisely the outcome the escalation ladder exists to prevent. It is
/// deliberately distinct from the smaller IP-wide pause floor (`IP_PAUSE_FLOOR`
/// in `capture.rs`, ruling H1) at which an honoured `Retry-After` starts.
///
/// [`next_delay`]: RateLimitBackoff::next_delay
pub const RATE_LIMIT_FLOOR: Duration = Duration::from_secs(60);

/// Upper bound on any rate-limit pause, honoured header or not. An hour of
/// silence is already a page-worthy outage; waiting longer buys nothing.
pub const RATE_LIMIT_CEILING: Duration = Duration::from_secs(3600);

/// The escalating pause a poller takes after a `429` or `418`.
///
/// `floor` is the caller's pause floor — never below the poll cadence, so a
/// refused poller never resumes faster than it was already going. For the
/// IP-wide sanction it is `max(IP_PAUSE_FLOOR, fastest_cadence)` (see
/// `capture.rs`, ruling H1); for a per-endpoint backoff it is that endpoint's
/// cadence.
///
/// Three rules, in order:
///
/// 1. **A `Retry-After` at or above the floor is honoured verbatim**, in either
///    RFC 7231 form — delta-seconds or an HTTP-date — capped at
///    [`RATE_LIMIT_CEILING`], and it *resets* the escalation: the venue asked
///    for a wait at least as long as our floor, so its number is trusted.
/// 2. **A `Retry-After` below the floor escalates.** A `Retry-After: 0` — which
///    an edge will send — must not resume the poll immediately against an
///    endpoint that has just refused it, so the pause starts at the floor and
///    doubles on each consecutive sub-floor refusal, capped at
///    `max(floor, `[`RATE_LIMIT_FLOOR`]`)` — the ban-avoidance floor, 60 s for
///    the 5 s IP pause.
/// 3. **No usable header escalates from the ban-avoidance floor.** Absent,
///    malformed, an obsolete date format, or a date already in the past: start
///    at `max(floor, `[`RATE_LIMIT_FLOOR`]`)` and double up to the ceiling.
///
/// [`reset`](Self::reset) on a served poll, and an honoured at-or-above-floor
/// header, are what make the ladder consecutive rather than cumulative.
#[derive(Debug, Default)]
pub struct RateLimitBackoff {
    consecutive: u32,
}

impl RateLimitBackoff {
    /// A fresh ladder.
    #[must_use]
    pub const fn new() -> Self {
        Self { consecutive: 0 }
    }

    /// Forget the refusal history after a poll the venue served.
    pub const fn reset(&mut self) {
        self.consecutive = 0;
    }

    /// Consecutive refusals since the last served poll.
    #[must_use]
    pub const fn consecutive(&self) -> u32 {
        self.consecutive
    }

    /// How long to pause after one rate-limit refusal.
    ///
    /// `now` is passed rather than read so an HTTP-date header resolves
    /// deterministically and the whole ladder is testable without a clock.
    pub fn next_delay(
        &mut self,
        header: Option<&str>,
        now: DateTime<Utc>,
        floor: Duration,
    ) -> Duration {
        if let Some(honoured) = parse_retry_after(header, now) {
            if honoured >= floor {
                // At or above the floor: the venue's own number is trusted and
                // resets the escalation, ceiling-clamped so an absurd header
                // cannot pause longer than a page-worthy outage.
                self.consecutive = 0;
                return honoured.min(RATE_LIMIT_CEILING);
            }
            // Below the floor (e.g. `Retry-After: 0`): never resume faster than
            // the floor, and a *repeated* sub-floor header escalates — the floor
            // doubling per consecutive refusal, capped at the ban-avoidance
            // floor (60 s for the 5 s IP pause).
            self.consecutive = self.consecutive.saturating_add(1);
            let cap = floor.max(RATE_LIMIT_FLOOR).min(RATE_LIMIT_CEILING);
            let doubling = 1_u32
                .checked_shl(self.consecutive.saturating_sub(1))
                .unwrap_or(u32::MAX);
            return floor.saturating_mul(doubling).min(cap);
        }
        // No usable instruction: escalate from the ban-avoidance floor. Guessing
        // a shorter pause than the venue would ask for is how a limit becomes a
        // ban.
        self.consecutive = self.consecutive.saturating_add(1);
        let base = floor.max(RATE_LIMIT_FLOOR);
        let doubling = 1_u32
            .checked_shl(self.consecutive.saturating_sub(1))
            .unwrap_or(u32::MAX);
        base.saturating_mul(doubling).min(RATE_LIMIT_CEILING)
    }
}

/// Parse a `Retry-After` value in either RFC 7231 form, relative to `now`.
///
/// `None` means "nothing usable here" — absent, malformed, a date format the
/// venue is not expected to send (the obsolete RFC 850 and asctime forms), or a
/// date already past. Every one of those escalates instead of shortening the
/// wait, because guessing a shorter pause than the venue asked for is how a
/// rate limit becomes a ban.
fn parse_retry_after(header: Option<&str>, now: DateTime<Utc>) -> Option<Duration> {
    let header = header?.trim();
    if header.is_empty() {
        return None;
    }
    if header.bytes().all(|byte| byte.is_ascii_digit()) {
        return header.parse::<u64>().ok().map(Duration::from_secs);
    }
    // IMF-fixdate, the form RFC 7231 requires senders to use:
    // `Sun, 06 Nov 1994 08:49:37 GMT`.
    let deadline = DateTime::parse_from_rfc2822(header)
        .ok()?
        .with_timezone(&Utc);
    let remaining = deadline.signed_duration_since(now);
    if remaining <= chrono::TimeDelta::zero() {
        return None;
    }
    remaining.to_std().ok()
}

/// Whether the venue's used-weight header says a poller must pause itself.
#[must_use]
pub fn used_weight_exhausted(header: Option<&str>) -> bool {
    header
        .and_then(|value| value.trim().parse::<u32>().ok())
        .is_some_and(|weight| weight >= USED_WEIGHT_PAUSE_THRESHOLD)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestDescriptor {
    request: String,
    #[allow(dead_code)]
    method: String,
    #[allow(dead_code)]
    url: String,
    #[allow(dead_code)]
    poll: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenInterestBody {
    symbol: String,
    #[serde(rename = "openInterest")]
    open_interest: String,
    time: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenInterestHistRow {
    symbol: String,
    #[serde(rename = "sumOpenInterest")]
    sum_open_interest: String,
    #[serde(rename = "sumOpenInterestValue")]
    sum_open_interest_value: String,
    // Documented on the row and deliberately NOT carried: circulating supply is
    // not a market fact about BTCUSDT and no signal reads it. Captured only so
    // the wire struct denies genuinely unknown fields.
    #[serde(rename = "CMCCirculatingSupply", default)]
    #[allow(dead_code)]
    cmc_circulating_supply: Option<String>,
    timestamp: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FundingInfoRow {
    symbol: String,
    #[serde(rename = "adjustedFundingRateCap")]
    adjusted_funding_rate_cap: String,
    #[serde(rename = "adjustedFundingRateFloor")]
    adjusted_funding_rate_floor: String,
    #[serde(rename = "fundingIntervalHours")]
    funding_interval_hours: u32,
    // OBSERVED 2026-09-03: present on every row, and `true` on exactly the rows
    // whose `updateTime` is null (114 of 1,554, BTCUSDT among them). Captured so
    // the struct denies genuinely unknown fields; never emitted.
    #[serde(default)]
    #[allow(dead_code)]
    disclaimer: Option<bool>,
    // OBSERVED 2026-09-03, and NOT in this spec's documented table: the venue's
    // own last-change time for that symbol's funding settings. It is NULLABLE
    // and is null for BTCUSDT, so it is accepted and deliberately not carried —
    // a declared field that is always null on our instrument would be a field
    // contract promising nothing. If a later capture shows it populated for
    // BTCUSDT, it becomes a declared timestamp then, on evidence.
    #[serde(rename = "updateTime", default)]
    #[allow(dead_code)]
    update_time: Option<i64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PremiumIndexBody {
    symbol: String,
    #[serde(rename = "markPrice")]
    mark_price: String,
    #[serde(rename = "indexPrice")]
    index_price: String,
    #[serde(rename = "estimatedSettlePrice")]
    estimated_settle_price: String,
    #[serde(rename = "lastFundingRate")]
    last_funding_rate: String,
    #[serde(rename = "interestRate")]
    interest_rate: String,
    #[serde(rename = "nextFundingTime")]
    next_funding_time: i64,
    time: i64,
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::binance_usdm::{BinanceUsdmAdapter, BinanceUsdmConnection};
    use crate::{
        CaptureStream, FailureKind, FeedAdapter, FeedNoticeCode, Provider, RawRecord, RecordKind,
    };

    use chrono::{DateTime, Utc};

    use super::{
        BinanceUsdmRestEndpoint, FUNDING_INFO_CADENCE, OPEN_INTEREST_CADENCE,
        OPEN_INTEREST_HIST_CADENCE, PREMIUM_INDEX_CADENCE, RATE_LIMIT_CEILING, RATE_LIMIT_FLOOR,
        RateLimitBackoff, RestBudget, request_descriptor, used_weight_exhausted,
    };

    // SYNTHETIC fixtures. No Binance USD-M REST body has been observed (ruling
    // R1); these assert the contract built from `A3-binance-usdm-feeds.md` §5,
    // and are replaced by capture-derived bodies at finalization.

    fn record(ordinal: u64, kind: RecordKind, payload: Option<&str>) -> RawRecord {
        RawRecord {
            schema_version: 1,
            provider: Provider::BinanceUsdm,
            stream: CaptureStream::BinanceUsdmRest,
            connection_epoch: 1,
            ordinal,
            received_at: "2026-09-03T04:00:02.250Z".to_owned(),
            capture_elapsed_ns: None,
            connect_started_elapsed_ns: None,
            kind,
            payload: payload.map(str::to_owned),
        }
    }

    fn adapter() -> BinanceUsdmAdapter {
        let mut adapter = BinanceUsdmAdapter::new("test-epoch", BinanceUsdmConnection::Rest)
            .expect("adapter constructs");
        adapter
            .ingest(&RawRecord {
                schema_version: 1,
                provider: Provider::BinanceUsdm,
                stream: CaptureStream::BinanceUsdmRest,
                connection_epoch: 1,
                ordinal: 1,
                received_at: "2026-09-03T04:00:00Z".to_owned(),
                capture_elapsed_ns: None,
                connect_started_elapsed_ns: None,
                kind: RecordKind::Connected,
                payload: None,
            })
            .expect("the poll session opens");
        adapter
    }

    fn poll(
        adapter: &mut BinanceUsdmAdapter,
        ordinal: u64,
        endpoint: BinanceUsdmRestEndpoint,
        body: &str,
    ) -> crate::IngestOutcome {
        adapter
            .ingest(&record(
                ordinal,
                RecordKind::OutboundText,
                Some(&request_descriptor(endpoint, 1)),
            ))
            .expect("the request descriptor is recorded");
        adapter
            .ingest(&record(ordinal + 1, RecordKind::InboundText, Some(body)))
            .expect("the response parses")
    }

    fn open_interest_body(open_interest: &str, time: i64) -> String {
        format!(r#"{{"openInterest":"{open_interest}","symbol":"BTCUSDT","time":{time}}}"#)
    }

    fn oi_hist_row(sum: &str, value: &str, timestamp: i64) -> String {
        format!(
            r#"{{"symbol":"BTCUSDT","sumOpenInterest":"{sum}","sumOpenInterestValue":"{value}","CMCCirculatingSupply":"19800000","timestamp":{timestamp}}}"#
        )
    }

    #[test]
    fn request_descriptor_is_the_exact_recorded_shape() {
        // The descriptor is the ONE authority the capture and the replay
        // dispatch share, so its bytes are pinned here.
        assert_eq!(
            request_descriptor(BinanceUsdmRestEndpoint::OpenInterest, 7),
            r#"{"method":"GET","poll":7,"request":"open_interest","url":"https://fapi.binance.com/fapi/v1/openInterest?symbol=BTCUSDT"}"#
        );
        assert_eq!(
            BinanceUsdmRestEndpoint::OpenInterestHist.cadence(),
            Duration::from_secs(300)
        );
        assert_eq!(
            BinanceUsdmRestEndpoint::FundingInfo.cadence(),
            Duration::from_secs(3600)
        );
    }

    #[test]
    fn open_interest_poll_happy_path() {
        let mut adapter = adapter();
        let outcome = poll(
            &mut adapter,
            2,
            BinanceUsdmRestEndpoint::OpenInterest,
            &open_interest_body("10659.50900000", 1_788_440_000_000),
        );
        assert_eq!(outcome.events.len(), 1);
        let event = &outcome.events[0];
        assert_eq!(event.source_id, "binance.usdm.btcusdt.open-interest");
        assert_eq!(event.values["instrument"], serde_json::json!("BTCUSDT"));
        assert_eq!(
            event.values["open_interest_base"],
            serde_json::json!("10659.50900000")
        );
        // Ruling E7: the poll ordinal starts at zero and is derived from the
        // record stream alone, so replay reproduces it exactly.
        assert_eq!(event.values["poll_sequence"], serde_json::json!(0_u64));
        assert_eq!(event.event_time, "2026-09-03T12:53:20.000Z");

        let second = poll(
            &mut adapter,
            4,
            BinanceUsdmRestEndpoint::OpenInterest,
            &open_interest_body("10660.00000000", 1_788_440_005_000),
        );
        assert_eq!(
            second.events[0].values["poll_sequence"],
            serde_json::json!(1_u64)
        );
        assert_eq!(second.events[0].sequence, 1);
    }

    #[test]
    fn open_interest_equal_timestamp_is_duplicate() {
        let mut adapter = adapter();
        poll(
            &mut adapter,
            2,
            BinanceUsdmRestEndpoint::OpenInterest,
            &open_interest_body("10659.50900000", 1_788_440_000_000),
        );
        // The venue republishes an unchanged snapshot between its own updates,
        // so an equal timestamp is a duplicate poll, not new state.
        let duplicate = poll(
            &mut adapter,
            4,
            BinanceUsdmRestEndpoint::OpenInterest,
            &open_interest_body("10659.50900000", 1_788_440_000_000),
        );
        assert!(duplicate.events.is_empty());
        assert_eq!(duplicate.notices[0].code, FeedNoticeCode::Duplicate);

        let stale = poll(
            &mut adapter,
            6,
            BinanceUsdmRestEndpoint::OpenInterest,
            &open_interest_body("10659.00000000", 1_788_439_999_000),
        );
        assert!(stale.events.is_empty());
        assert_eq!(stale.notices[0].code, FeedNoticeCode::OutOfOrder);
    }

    #[test]
    fn open_interest_non_200_emits_no_event() {
        // A non-2xx response records NOTHING (the Chainlink rule): the capture
        // counts it and writes no inbound record, so the replayed stream is a
        // request descriptor with no response — and no event is fabricated for
        // a body we never received.
        let mut adapter = adapter();
        let failed = adapter
            .ingest(&record(
                2,
                RecordKind::OutboundText,
                Some(&request_descriptor(
                    BinanceUsdmRestEndpoint::OpenInterest,
                    1,
                )),
            ))
            .expect("the request descriptor is recorded");
        assert!(failed.events.is_empty());
        assert!(failed.notices.is_empty());

        // A malformed or foreign-symbol body IS recorded, and it degrades the
        // identity with a recovery notice rather than emitting a value.
        let malformed = poll(
            &mut adapter,
            3,
            BinanceUsdmRestEndpoint::OpenInterest,
            r#"{"code":-1121,"msg":"Invalid symbol."}"#,
        );
        assert!(malformed.events.is_empty());
        assert_eq!(malformed.notices[0].code, FeedNoticeCode::StateInconsistent);
        assert!(malformed.notices[0].recovery.is_some());

        let foreign = poll(
            &mut adapter,
            5,
            BinanceUsdmRestEndpoint::OpenInterest,
            &open_interest_body("10659.50900000", 1_788_440_000_000).replace("BTCUSDT", "ETHUSDT"),
        );
        assert!(foreign.events.is_empty());
        assert_eq!(foreign.notices[0].code, FeedNoticeCode::StateInconsistent);
    }

    /// The observed US-host result for `fapi.binance.com` is HTTP 451
    /// (ruling E27). A capture from such a host must not poll it at cadence for
    /// its whole duration and write a file that says nothing about why.
    #[test]
    fn a_451_is_recorded_with_its_status_and_backs_the_poller_off() {
        // The capture writes the refusal down. It is the one record kind whose
        // payload we author, and it carries the status rather than a body we
        // never received.
        let record = RawRecord::failed_at(
            CaptureStream::BinanceUsdmRest,
            1,
            2,
            FailureKind::HttpStatus(451),
            BinanceUsdmRestEndpoint::OpenInterest.token(),
            "2026-09-03T04:00:02.250Z".to_owned(),
            2_000,
        );
        record.validate().expect("a failure record is well formed");
        assert_eq!(record.kind, RecordKind::Failed);
        let payload: serde_json::Value =
            serde_json::from_str(record.payload.as_deref().expect("payload"))
                .expect("the payload is our own JSON");
        assert_eq!(payload["status"], serde_json::json!(451));
        assert_eq!(payload["failure"], serde_json::json!("http_status"));
        assert_eq!(payload["detail"], serde_json::json!("open_interest"));

        // Replaying it is operational state, never an event, and it names the
        // status so a consumer can tell a refused venue from a quiet one.
        let mut adapter = adapter();
        let outcome = adapter.ingest(&record).expect("a refusal replays");
        assert!(outcome.events.is_empty());
        assert_eq!(outcome.notices[0].code, FeedNoticeCode::StateInconsistent);
        assert!(outcome.notices[0].recovery.is_some());
        assert!(
            outcome.notices[0].detail.contains("451"),
            "{}",
            outcome.notices[0].detail
        );

        // And the poller backs off the same ladder a rate limit uses, so it
        // cannot keep polling a refusing endpoint at its five-second cadence.
        let now = at("2026-09-03T12:00:00Z");
        let mut backoff = RateLimitBackoff::new();
        let ladder: Vec<Duration> = (0..4)
            .map(|_| backoff.next_delay(None, now, OPEN_INTEREST_CADENCE))
            .collect();
        assert_eq!(
            ladder,
            vec![
                Duration::from_secs(60),
                Duration::from_secs(120),
                Duration::from_secs(240),
                Duration::from_secs(480),
            ]
        );
        for delay in &ladder {
            assert!(*delay > OPEN_INTEREST_CADENCE);
        }
        // An hour against a 451 host costs a handful of requests, not 900.
        let mut budget = RateLimitBackoff::new();
        let mut elapsed = Duration::ZERO;
        let mut polls = 0_u32;
        while elapsed < Duration::from_secs(3600) {
            polls += 1;
            elapsed += budget.next_delay(None, now, OPEN_INTEREST_CADENCE);
        }
        assert!(
            polls <= 8,
            "an hour of refusals must cost single-digit polls; got {polls}"
        );
    }

    /// The E27(c) shape: a host that never answers at all. It must cost the
    /// same as a host that answers 451 — a record, a line per escalation step,
    /// and the same ladder — not twelve polls a minute into a black hole.
    #[test]
    fn a_transport_failure_is_recorded_with_its_cause_and_backs_the_poller_off() {
        let record = RawRecord::failed_at(
            CaptureStream::BinanceUsdmRest,
            1,
            2,
            FailureKind::Transport,
            "open_interest: error sending request",
            "2026-09-03T04:00:02.250Z".to_owned(),
            2_000,
        );
        record
            .validate()
            .expect("a transport failure is well formed");
        let payload: serde_json::Value =
            serde_json::from_str(record.payload.as_deref().expect("payload"))
                .expect("the payload is our own JSON");
        // No status is invented for a request that never got one.
        assert_eq!(payload["failure"], serde_json::json!("transport"));
        assert!(payload.get("status").is_none());
        assert!(
            payload["detail"]
                .as_str()
                .expect("detail")
                .contains("open_interest")
        );

        let mut adapter = adapter();
        let outcome = adapter
            .ingest(&record)
            .expect("a transport failure replays");
        assert!(outcome.events.is_empty());
        assert_eq!(outcome.notices[0].code, FeedNoticeCode::StateInconsistent);
        assert!(outcome.notices[0].recovery.is_some());

        // The same ladder as an answered refusal, so an hour against a
        // blackholed host is single-digit polls rather than 720.
        let now = at("2026-09-03T12:00:00Z");
        let mut backoff = RateLimitBackoff::new();
        let mut elapsed = Duration::ZERO;
        let mut polls = 0_u32;
        while elapsed < Duration::from_secs(3600) {
            polls += 1;
            elapsed += backoff.next_delay(None, now, OPEN_INTEREST_CADENCE);
        }
        assert!(
            polls <= 8,
            "an hour of a dead host must cost ≤8 polls; got {polls}"
        );
    }

    /// A failure record belongs to a poll session. The USD-M `public` socket has
    /// no request to fail, so one arriving there is a malformed capture.
    #[test]
    fn the_usdm_websocket_connections_refuse_a_failure_record() {
        use crate::binance_usdm::{BinanceUsdmAdapter, BinanceUsdmConnection};

        // E62 left one USD-M socket connection, `public` (depth); the `market`
        // connection and its sources are gone.
        let connection = BinanceUsdmConnection::Public;
        let stream = CaptureStream::BinanceUsdmPublic;
        let mut adapter =
            BinanceUsdmAdapter::new("test-epoch", connection).expect("adapter constructs");
        adapter
            .ingest(&RawRecord::connected_at(
                stream,
                1,
                1,
                "2026-09-03T04:00:00Z".to_owned(),
                1_000,
                0,
            ))
            .expect("the socket opens");
        let refusal = adapter
            .ingest(&RawRecord::failed_at(
                stream,
                1,
                2,
                FailureKind::HttpStatus(451),
                "open_interest",
                "2026-09-03T04:00:02.250Z".to_owned(),
                2_000,
            ))
            .expect_err("a socket has no request to fail");
        assert!(
            matches!(refusal, crate::AdapterError::InvalidRecord(_)),
            "{connection:?}: {refusal}"
        );
    }

    #[test]
    fn a_response_without_a_request_descriptor_is_refused() {
        // Without the descriptor a body cannot be attributed to an endpoint, and
        // guessing from its shape is exactly the ambiguity the descriptor exists
        // to remove.
        let mut adapter = adapter();
        assert!(
            adapter
                .ingest(&record(
                    2,
                    RecordKind::InboundText,
                    Some(&open_interest_body("10659.50900000", 1_788_440_000_000)),
                ))
                .is_err()
        );
    }

    #[test]
    fn oi_hist_watermark_skips_overlap() {
        let mut adapter = adapter();
        let first = format!(
            "[{},{}]",
            oi_hist_row("10600.000", "701000000.00", 1_788_439_700_000),
            oi_hist_row("10650.000", "704000000.00", 1_788_440_000_000)
        );
        let outcome = poll(
            &mut adapter,
            2,
            BinanceUsdmRestEndpoint::OpenInterestHist,
            &first,
        );
        assert_eq!(outcome.events.len(), 2);
        let event = &outcome.events[0];
        assert_eq!(event.source_id, "binance.usdm.btcusdt.open-interest-hist");
        assert_eq!(event.values["period"], serde_json::json!("5m"));
        assert_eq!(event.values["row_count"], serde_json::json!(2_u32));
        assert_eq!(event.values["row_index"], serde_json::json!(0_u32));
        assert_eq!(
            event.values["sum_open_interest_base"],
            serde_json::json!("10600.000")
        );
        // Circulating supply is deliberately not carried: it is not a market
        // fact about BTCUSDT.
        assert!(!event.values.contains_key("cmc_circulating_supply"));

        // The poll asks for thirty rows every five minutes, so the next body
        // overlaps. Only the strictly newer row is a new fact.
        let second = format!(
            "[{},{}]",
            oi_hist_row("10650.000", "704000000.00", 1_788_440_000_000),
            oi_hist_row("10700.000", "707000000.00", 1_788_440_300_000)
        );
        let overlapped = poll(
            &mut adapter,
            4,
            BinanceUsdmRestEndpoint::OpenInterestHist,
            &second,
        );
        assert_eq!(overlapped.events.len(), 1);
        assert_eq!(
            overlapped.events[0].values["row_count"],
            serde_json::json!(1_u32)
        );
        assert_eq!(overlapped.events[0].event_time, "2026-09-03T12:58:20.000Z");

        // A body carrying nothing new at all is a duplicate, not an error.
        let repeated = poll(
            &mut adapter,
            6,
            BinanceUsdmRestEndpoint::OpenInterestHist,
            &second,
        );
        assert!(repeated.events.is_empty());
        assert_eq!(repeated.notices[0].code, FeedNoticeCode::Duplicate);
    }

    #[test]
    fn oi_hist_over_1024_rows_is_invalid_payload() {
        let mut adapter = adapter();
        let row = oi_hist_row("10600.000", "701000000.00", 1_788_439_700_000);
        let rows = vec![row.as_str(); 1_025].join(",");
        adapter
            .ingest(&record(
                2,
                RecordKind::OutboundText,
                Some(&request_descriptor(
                    BinanceUsdmRestEndpoint::OpenInterestHist,
                    1,
                )),
            ))
            .expect("descriptor recorded");
        assert!(
            adapter
                .ingest(&record(
                    3,
                    RecordKind::InboundText,
                    Some(&format!("[{rows}]"))
                ))
                .is_err()
        );
    }

    #[test]
    fn oi_hist_empty_body_is_noted_without_events() {
        let mut adapter = adapter();
        let empty = poll(
            &mut adapter,
            2,
            BinanceUsdmRestEndpoint::OpenInterestHist,
            "[]",
        );
        assert!(empty.events.is_empty());
        assert_eq!(empty.notices[0].code, FeedNoticeCode::StateInconsistent);
    }

    #[test]
    fn funding_info_parses_the_btcusdt_element() {
        let mut adapter = adapter();
        let body = r#"[{"symbol":"ETHUSDT","adjustedFundingRateCap":"0.02000000","adjustedFundingRateFloor":"-0.02000000","fundingIntervalHours":8},{"symbol":"BTCUSDT","adjustedFundingRateCap":"0.02000000","adjustedFundingRateFloor":"-0.02000000","fundingIntervalHours":4}]"#;
        let outcome = poll(&mut adapter, 2, BinanceUsdmRestEndpoint::FundingInfo, body);
        let event = &outcome.events[0];
        assert_eq!(event.source_id, "binance.usdm.btcusdt.funding-info");
        // The interval is the VENUE-DECLARED value, never a constant of ours.
        assert_eq!(
            event.values["funding_interval_hours"],
            serde_json::json!(4_u32)
        );
        assert_eq!(
            event.values["adjusted_funding_rate_floor"],
            serde_json::json!("-0.02000000")
        );
        // The body carries no venue clock, so the event time is our receipt.
        assert_eq!(event.event_time, "2026-09-03T04:00:02.250Z");
    }

    #[test]
    fn funding_info_absent_btcusdt_emits_no_event() {
        // Probe P5, fail-closed: the endpoint documents only symbols with
        // non-default settings. If BTCUSDT is absent, NO default interval, cap
        // or floor is substituted — unknown stays unknown and the dependent
        // funding identity stays non-ready.
        let mut adapter = adapter();
        let body = r#"[{"symbol":"ETHUSDT","adjustedFundingRateCap":"0.02000000","adjustedFundingRateFloor":"-0.02000000","fundingIntervalHours":8}]"#;
        let outcome = poll(&mut adapter, 2, BinanceUsdmRestEndpoint::FundingInfo, body);
        assert!(outcome.events.is_empty());
        assert_eq!(outcome.notices[0].code, FeedNoticeCode::StateInconsistent);

        let empty = poll(&mut adapter, 4, BinanceUsdmRestEndpoint::FundingInfo, "[]");
        assert!(empty.events.is_empty());
        assert_eq!(empty.notices[0].code, FeedNoticeCode::StateInconsistent);
    }

    #[test]
    fn funding_info_interval_out_of_range_rejected() {
        for hours in ["0", "25"] {
            let mut adapter = adapter();
            let body = format!(
                r#"[{{"symbol":"BTCUSDT","adjustedFundingRateCap":"0.02000000","adjustedFundingRateFloor":"-0.02000000","fundingIntervalHours":{hours}}}]"#
            );
            adapter
                .ingest(&record(
                    2,
                    RecordKind::OutboundText,
                    Some(&request_descriptor(BinanceUsdmRestEndpoint::FundingInfo, 1)),
                ))
                .expect("descriptor recorded");
            assert!(
                adapter
                    .ingest(&record(3, RecordKind::InboundText, Some(&body)))
                    .is_err(),
                "{hours} hours must fail closed"
            );
        }
    }

    fn premium_index_body(
        mark: &str,
        index: &str,
        last_funding_rate: &str,
        next_funding_time: i64,
        time: i64,
    ) -> String {
        format!(
            r#"{{"symbol":"BTCUSDT","markPrice":"{mark}","indexPrice":"{index}","estimatedSettlePrice":"{index}","lastFundingRate":"{last_funding_rate}","interestRate":"0.00010000","nextFundingTime":{next_funding_time},"time":{time}}}"#
        )
    }

    #[test]
    fn premium_index_poll_happy_path_and_duplicate() {
        let mut adapter = adapter();
        let outcome = poll(
            &mut adapter,
            2,
            BinanceUsdmRestEndpoint::PremiumIndex,
            &premium_index_body(
                "108250.30000000",
                "108240.00000000",
                "-0.00012345",
                1_788_451_200_000,
                1_788_440_000_000,
            ),
        );
        assert_eq!(outcome.events.len(), 1);
        let event = &outcome.events[0];
        assert_eq!(event.source_id, "binance.usdm.btcusdt.premium-index");
        assert_eq!(event.values["instrument"], serde_json::json!("BTCUSDT"));
        assert_eq!(
            event.values["mark_price"],
            serde_json::json!("108250.30000000")
        );
        assert_eq!(
            event.values["index_price"],
            serde_json::json!("108240.00000000")
        );
        assert_eq!(
            event.values["estimated_settle_price"],
            serde_json::json!("108240.00000000")
        );
        // Signed funding and interest rates keep their sign verbatim.
        assert_eq!(
            event.values["last_funding_rate"],
            serde_json::json!("-0.00012345")
        );
        assert_eq!(
            event.values["interest_rate"],
            serde_json::json!("0.00010000")
        );
        // Both venue millis timestamps are rendered as RFC 3339.
        assert_eq!(
            event.values["next_funding_time"],
            serde_json::json!("2026-09-03T16:00:00.000Z")
        );
        assert_eq!(
            event.values["event_time"],
            serde_json::json!("2026-09-03T12:53:20.000Z")
        );
        assert_eq!(event.event_time, "2026-09-03T12:53:20.000Z");

        // The venue republishes an unchanged snapshot between its own updates, so
        // an equal `time` is a duplicate poll and emits no second event.
        let duplicate = poll(
            &mut adapter,
            4,
            BinanceUsdmRestEndpoint::PremiumIndex,
            &premium_index_body(
                "108251.00000000",
                "108241.00000000",
                "-0.00012345",
                1_788_451_200_000,
                1_788_440_000_000,
            ),
        );
        assert!(duplicate.events.is_empty());
        assert_eq!(duplicate.notices[0].code, FeedNoticeCode::Duplicate);
    }

    fn at(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .expect("fixture instant parses")
            .with_timezone(&Utc)
    }

    #[test]
    fn rest_429_honours_retry_after_in_both_rfc7231_forms() {
        let now = at("2026-09-03T12:00:00Z");
        let cadence = OPEN_INTEREST_CADENCE;

        // Delta-seconds: honoured exactly.
        let mut backoff = RateLimitBackoff::new();
        assert_eq!(
            backoff.next_delay(Some("30"), now, cadence),
            Duration::from_secs(30)
        );
        let mut backoff = RateLimitBackoff::new();
        assert_eq!(
            backoff.next_delay(Some(" 120 "), now, cadence),
            Duration::from_secs(120)
        );

        // An HTTP-date is the other form RFC 7231 allows, and Binance's edge
        // sends it. Honoured exactly, as the interval from now to that instant.
        let mut backoff = RateLimitBackoff::new();
        assert_eq!(
            backoff.next_delay(Some("Thu, 03 Sep 2026 12:03:20 GMT"), now, cadence),
            Duration::from_secs(200),
            "an HTTP-date must be honoured, not fall through to the ladder"
        );

        // A header SHORTER than the poll cadence cannot make the poller come
        // back faster than it was already going. An edge that answers `0` — or
        // an HTTP-date one second out — must not resume the five-second
        // open-interest poll against an endpoint that has just refused it.
        assert_eq!(cadence, Duration::from_secs(5), "the production cadence");
        for header in ["0", "00", " 0 ", "1", "2", "4"] {
            let mut backoff = RateLimitBackoff::new();
            let delay = backoff.next_delay(Some(header), now, cadence);
            assert_eq!(delay, cadence, "{header:?} must floor at the cadence");
        }
        // One second past `now`, in the other RFC 7231 form.
        let mut backoff = RateLimitBackoff::new();
        assert_eq!(
            backoff.next_delay(Some("Thu, 03 Sep 2026 12:00:01 GMT"), now, cadence),
            cadence
        );
        // At and above the cadence the venue's own number is used verbatim.
        let mut backoff = RateLimitBackoff::new();
        assert_eq!(
            backoff.next_delay(Some("5"), now, cadence),
            Duration::from_secs(5)
        );
        let mut backoff = RateLimitBackoff::new();
        assert_eq!(
            backoff.next_delay(Some("6"), now, cadence),
            Duration::from_secs(6)
        );
        // A date already in the past is not a short pause: it is no usable
        // instruction at all, so it escalates rather than flooring.
        let mut backoff = RateLimitBackoff::new();
        assert_eq!(
            backoff.next_delay(Some("Thu, 03 Sep 2026 11:59:59 GMT"), now, cadence),
            RATE_LIMIT_FLOOR
        );

        // Either form is clamped to the ceiling, never beyond it.
        let mut backoff = RateLimitBackoff::new();
        assert_eq!(
            backoff.next_delay(Some("99999"), now, cadence),
            RATE_LIMIT_CEILING
        );
        let mut backoff = RateLimitBackoff::new();
        assert_eq!(
            backoff.next_delay(Some("Fri, 04 Sep 2026 12:00:00 GMT"), now, cadence),
            RATE_LIMIT_CEILING
        );
    }

    #[test]
    fn rest_rate_limit_without_a_usable_header_escalates_and_never_resumes_at_cadence() {
        let now = at("2026-09-03T12:00:00Z");
        // The PRODUCTION cadence, not a hand-picked one: the five-second
        // open-interest poll is exactly the value that made the old fallback
        // dangerous.
        let cadence = OPEN_INTEREST_CADENCE;
        assert_eq!(cadence, Duration::from_secs(5));

        for header in [
            None,
            Some(""),
            Some("   "),
            Some("soon"),
            Some("-1"),
            Some("30s"),
            Some("1.5"),
            // An obsolete RFC 850 date: not a form the venue is expected to
            // send, so it escalates rather than being half-parsed.
            Some("Thursday, 03-Sep-26 12:03:20 GMT"),
            // A date already past means "retry now", which against an endpoint
            // that just refused us is exactly what must not happen.
            Some("Thu, 03 Sep 2026 11:00:00 GMT"),
        ] {
            let mut backoff = RateLimitBackoff::new();
            let delay = backoff.next_delay(header, now, cadence);
            assert_eq!(delay, RATE_LIMIT_FLOOR, "{header:?}");
            assert!(delay > cadence, "{header:?} must never resume at cadence");
        }

        // Consecutive refusals double, up to the ceiling and no further.
        let mut backoff = RateLimitBackoff::new();
        let ladder: Vec<Duration> = (0..8)
            .map(|_| backoff.next_delay(None, now, cadence))
            .collect();
        assert_eq!(
            ladder,
            vec![
                Duration::from_secs(60),
                Duration::from_secs(120),
                Duration::from_secs(240),
                Duration::from_secs(480),
                Duration::from_secs(960),
                Duration::from_secs(1920),
                RATE_LIMIT_CEILING,
                RATE_LIMIT_CEILING,
            ]
        );
        assert_eq!(backoff.consecutive(), 8);

        // A served poll resets the ladder.
        backoff.reset();
        assert_eq!(backoff.consecutive(), 0);
        assert_eq!(backoff.next_delay(None, now, cadence), RATE_LIMIT_FLOOR);

        // A slower poller's own cadence is the floor when it is the larger of
        // the two: the hourly funding poll never retries faster than an hour.
        let mut hourly = RateLimitBackoff::new();
        assert_eq!(
            hourly.next_delay(None, now, FUNDING_INFO_CADENCE),
            RATE_LIMIT_CEILING
        );
        // That cadence sits exactly ON the ceiling, so the floor-then-ceiling
        // pair meets at a point. `Ord::clamp` panics when its bounds cross;
        // this must return a value for a header on either side of it.
        assert_eq!(FUNDING_INFO_CADENCE, RATE_LIMIT_CEILING);
        for header in ["1", "3600", "99999"] {
            let mut hourly = RateLimitBackoff::new();
            assert_eq!(
                hourly.next_delay(Some(header), now, FUNDING_INFO_CADENCE),
                RATE_LIMIT_CEILING,
                "{header:?} at the hourly cadence"
            );
        }
    }

    #[test]
    fn rest_429_ip_pause_floor_escalates_sub_floor_and_resets_at_or_above_it() {
        // Ruling H1: the IP-wide pause floor is 5 s (`max(IP_PAUSE_FLOOR,
        // fastest_cadence)` for the four-endpoint set), well below the 60 s
        // ban-avoidance floor. A sub-floor `Retry-After` escalates from 5 s; an
        // at-or-above-floor one is honoured verbatim and resets the ladder.
        let now = at("2026-09-03T12:00:00Z");
        let floor = Duration::from_secs(5);

        // A single `Retry-After: 0` pauses at the 5 s floor, not the 1 s premium
        // cadence that lives below it.
        let mut backoff = RateLimitBackoff::new();
        assert_eq!(
            backoff.next_delay(Some("0"), now, floor),
            Duration::from_secs(5)
        );

        // Three consecutive sub-floor headers escalate 5 -> 10 -> 20 s.
        let mut backoff = RateLimitBackoff::new();
        let ladder: Vec<Duration> = (0..3)
            .map(|_| backoff.next_delay(Some("0"), now, floor))
            .collect();
        assert_eq!(
            ladder,
            vec![
                Duration::from_secs(5),
                Duration::from_secs(10),
                Duration::from_secs(20),
            ]
        );
        // The escalation saturates at the 60 s ban-avoidance floor, never above.
        for _ in 0..8 {
            let capped = backoff.next_delay(Some("0"), now, floor);
            assert!(
                capped <= RATE_LIMIT_FLOOR,
                "sub-floor escalation caps at 60 s"
            );
        }
        assert_eq!(backoff.next_delay(Some("0"), now, floor), RATE_LIMIT_FLOOR);

        // A `Retry-After` at or above the floor is honoured verbatim and RESETS
        // the escalation, so the next sub-floor header starts again at the floor.
        let mut backoff = RateLimitBackoff::new();
        let _ = backoff.next_delay(Some("0"), now, floor);
        let _ = backoff.next_delay(Some("0"), now, floor);
        assert_eq!(
            backoff.next_delay(Some("30"), now, floor),
            Duration::from_secs(30)
        );
        assert_eq!(
            backoff.consecutive(),
            0,
            "an at-or-above-floor header resets the ladder"
        );
        assert_eq!(
            backoff.next_delay(Some("0"), now, floor),
            Duration::from_secs(5),
            "the sub-floor pause starts again at the floor after a reset"
        );
    }

    #[test]
    fn rest_self_budget_pauses_poller() {
        let mut budget = RestBudget::new();
        for claim in 0..super::MAX_REQUESTS_PER_MINUTE {
            assert!(
                budget.try_claim(1_000),
                "claim {claim} is inside the budget"
            );
        }
        assert_eq!(budget.claimed(), super::MAX_REQUESTS_PER_MINUTE);
        // The first request past the budget in the same minute is refused.
        assert!(!budget.try_claim(1_500));
        assert!(!budget.try_claim(60_000));
        // A full minute past the window start reopens the allowance.
        assert!(budget.try_claim(61_001));
        assert_eq!(budget.claimed(), 1);
    }

    #[test]
    fn steady_state_cadences_never_exhaust_the_budget() {
        // The four endpoints at their configured cadences over a simulated 60 s:
        // premium-index 60/min + open-interest 12/min + open-interest-hist 1 +
        // funding-info 1 = 74 claims, comfortably inside the 120/min budget, so
        // the 1 s premium-index poll is served its full 60 marks a minute.
        let mut budget = RestBudget::new();
        let mut premium_index_served = 0u32;
        let mut refusals = 0u32;
        for sec in 0..60u64 {
            let elapsed_ms = sec * 1_000;
            // Endpoints due this second, at their configured cadences. The
            // premium index is due every second; the others when the whole
            // second is a multiple of their cadence.
            let mut due = vec![PREMIUM_INDEX_CADENCE];
            if sec % OPEN_INTEREST_CADENCE.as_secs() == 0 {
                due.push(OPEN_INTEREST_CADENCE);
            }
            if sec % OPEN_INTEREST_HIST_CADENCE.as_secs() == 0 {
                due.push(OPEN_INTEREST_HIST_CADENCE);
            }
            if sec % FUNDING_INFO_CADENCE.as_secs() == 0 {
                due.push(FUNDING_INFO_CADENCE);
            }
            for cadence in due {
                if budget.try_claim(elapsed_ms) {
                    if cadence == PREMIUM_INDEX_CADENCE {
                        premium_index_served += 1;
                    }
                } else {
                    refusals += 1;
                }
            }
        }
        assert_eq!(refusals, 0, "no budget refusal in steady state");
        assert_eq!(
            premium_index_served, 60,
            "premium-index served once per second for a full minute"
        );
    }

    #[test]
    fn used_weight_header_pauses_at_the_threshold() {
        assert!(!used_weight_exhausted(None));
        assert!(!used_weight_exhausted(Some("999")));
        assert!(used_weight_exhausted(Some("1000")));
        assert!(used_weight_exhausted(Some(" 2400 ")));
        // A header we cannot read is not evidence of exhaustion.
        assert!(!used_weight_exhausted(Some("many")));
    }
}
