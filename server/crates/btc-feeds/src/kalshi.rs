//! Kalshi BTC 15-minute market adapter: REST market discovery, the discovery
//! journal that binds an adapter to one exact market, and the authenticated
//! book, ticker and trade channels.

use chrono::{DateTime, SecondsFormat, TimeZone, Utc};
use reqwest::header::{ACCEPT, CACHE_CONTROL};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use signals_core::SourceEvent;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use thiserror::Error;

use crate::adapter::{
    GuardAction, RecordGuard, bounded_token, parse_timestamp, validate_publisher_epoch,
    validate_timestamp,
};
use crate::decimal::{decimal_string, validate_decimal};
use crate::secure_http::{self, SecureHttpError, sha256_hex};
use crate::{
    AdapterError, FeedAdapter, FeedNotice, FeedNoticeCode, IngestOutcome, Provider, RawRecord,
    RecoveryAction,
};

pub(crate) const KALSHI_MARKETS_URL: &str = "https://external-api.kalshi.com/trade-api/v2/markets";
const CONFIGURATION_KIND: &str = "kalshi_btc_market_v1";
pub(crate) const MAX_DISCOVERY_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const MAX_BOOK_ROWS: usize = 100_000;
const MAX_TICKER_STATES_PER_MILLISECOND: usize = 4_096;
const MAX_TRADE_IDS: usize = 100_000;
const MARKET_CONTRACT_SOURCE_ID: &str = "kalshi.btc.market-contracts";
const BOOK_SOURCE_ID: &str = "kalshi.btc.book-levels";
const QUOTE_SOURCE_ID: &str = "kalshi.btc.quotes";
const TRADE_SOURCE_ID: &str = "kalshi.btc.trades";
const SOURCE_VERSION: &str = "1.0.0";

/// One resolved Kalshi BTC market, ready to bind an adapter to.
#[derive(Debug, Clone)]
pub struct KalshiDiscovery {
    /// The Configuration [`RawRecord`] payload: a discovery journal that embeds
    /// the exact REST body, its request path, the retrieval time and the
    /// SHA-256 over the body. Feed it to a fresh [`KalshiAdapter`] as the
    /// stream's first record; see
    /// [`KalshiAdapter::configuration_payload`] for the format.
    pub journal_payload: String,
    /// The exact market ticker the journal resolved to.
    pub market_ticker: String,
}

/// Fixed-host, keyless REST discovery for one exact BTC 15-minute ticker.
pub(crate) async fn discover_market(ticker: &str) -> Result<KalshiDiscovery, KalshiDiscoveryError> {
    validate_market_ticker(ticker)
        .map_err(|error| KalshiDiscoveryError::InvalidMarket(error.to_string()))?;
    let response = secure_http::client()?
        .get(KALSHI_MARKETS_URL)
        .header(ACCEPT, "application/json")
        .header(CACHE_CONTROL, "no-cache")
        .query(&[("tickers", ticker)])
        .send()
        .await
        .map_err(|_| KalshiDiscoveryError::Http)?;
    let body = secure_http::bounded_body(response, MAX_DISCOVERY_RESPONSE_BYTES).await?;
    let retrieved_at = crate::raw::now_timestamp();
    let body_text = String::from_utf8(body).map_err(|_| {
        KalshiDiscoveryError::InvalidResponse("market response is not UTF-8".to_owned())
    })?;
    discovery_from_response(ticker, retrieved_at, body_text)
}

/// Build a discovery journal from a Kalshi markets REST body that was fetched
/// elsewhere.
///
/// `ticker` is the exact `KXBTC15M-*` ticker that was requested, `retrieved_at`
/// the RFC 3339 instant the response was received, and `body_text` the response
/// body verbatim. The body is validated against the full market contract before
/// a journal is produced, so an unusable market fails here rather than at
/// ingest.
pub fn discovery_from_response(
    ticker: &str,
    retrieved_at: String,
    body_text: String,
) -> Result<KalshiDiscovery, KalshiDiscoveryError> {
    validate_market_ticker(ticker)
        .map_err(|error| KalshiDiscoveryError::InvalidMarket(error.to_string()))?;
    let contract = MarketContract::from_rest(&body_text, ticker, &retrieved_at)
        .map_err(|error| KalshiDiscoveryError::InvalidMarket(error.to_string()))?;
    let request_path = format!("/trade-api/v2/markets?tickers={ticker}");
    let configuration = DiscoveryJournal {
        schema_version: 1,
        kind: CONFIGURATION_KIND.to_owned(),
        request_path,
        retrieved_at,
        response_body_sha256: sha256_hex(body_text.as_bytes()),
        response_body: body_text,
    };
    let journal_payload =
        serde_json::to_string(&configuration).map_err(KalshiDiscoveryError::Json)?;
    Ok(KalshiDiscovery {
        journal_payload,
        market_ticker: contract.market_ticker,
    })
}

/// Check that `ticker` is one exact Kalshi BTC 15-minute market ticker.
///
/// The crate never accepts a wildcard, prefix or list: `KXBTC15M-` plus
/// upper-case ASCII, digits and hyphens, 16 to 128 bytes.
pub fn validate_market_ticker(ticker: &str) -> Result<(), AdapterError> {
    if ticker.len() < 16
        || ticker.len() > 128
        || !ticker.starts_with("KXBTC15M-")
        || !ticker
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(AdapterError::InvalidPayload(
            "Kalshi ticker must be one exact KXBTC15M market ticker".to_owned(),
        ));
    }
    Ok(())
}

/// Why Kalshi market discovery could not produce a usable journal.
#[derive(Debug, Error)]
pub enum KalshiDiscoveryError {
    /// The bounded fixed-host REST request failed. Deliberately carries no
    /// transport detail.
    #[error("secure Kalshi REST request failed")]
    Http,
    /// The response was not bounded UTF-8 JSON of the expected shape.
    #[error("Kalshi returned an invalid bounded response: {0}")]
    InvalidResponse(String),
    /// The response parsed but the market it describes fails a contract rule
    /// (ticker, binary/greater-or-equal semantics, BRTI settlement rules, or the
    /// open/close/expiration relationship).
    #[error("Kalshi market contract validation failed: {0}")]
    InvalidMarket(String),
    /// The validated journal could not be serialized.
    #[error("Kalshi configuration serialization failed: {0}")]
    Json(serde_json::Error),
}

impl From<SecureHttpError> for KalshiDiscoveryError {
    fn from(_: SecureHttpError) -> Self {
        Self::Http
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DiscoveryJournal {
    schema_version: u16,
    kind: String,
    request_path: String,
    retrieved_at: String,
    response_body_sha256: String,
    response_body: String,
}

#[derive(Debug, Clone)]
struct MarketContract {
    market_ticker: String,
    event_ticker: String,
    title: String,
    status: String,
    open_time: String,
    close_time: String,
    expected_expiration_time: String,
    legal_expiration_time: String,
    reference_value: String,
    rules_sha256: String,
    event_time: String,
    received_at: String,
}

impl MarketContract {
    fn from_journal(payload: &str) -> Result<Self, AdapterError> {
        let journal: DiscoveryJournal = serde_json::from_str(payload).map_err(payload_error)?;
        if journal.schema_version != 1 || journal.kind != CONFIGURATION_KIND {
            return Err(AdapterError::InvalidPayload(
                "unsupported Kalshi discovery configuration".to_owned(),
            ));
        }
        validate_timestamp(&journal.retrieved_at, "retrieved_at")?;
        if sha256_hex(journal.response_body.as_bytes()) != journal.response_body_sha256 {
            return Err(AdapterError::InvalidPayload(
                "Kalshi REST body hash does not match configuration".to_owned(),
            ));
        }
        let ticker = journal
            .request_path
            .strip_prefix("/trade-api/v2/markets?tickers=")
            .ok_or_else(|| {
                AdapterError::InvalidPayload("invalid Kalshi discovery request path".to_owned())
            })?;
        validate_market_ticker(ticker)?;
        Self::from_rest(&journal.response_body, ticker, &journal.retrieved_at)
    }

    fn from_rest(
        body: &str,
        expected_ticker: &str,
        retrieved_at: &str,
    ) -> Result<Self, AdapterError> {
        validate_timestamp(retrieved_at, "retrieved_at")?;
        let response: MarketsResponse = serde_json::from_str(body).map_err(payload_error)?;
        if !response.cursor.is_empty() {
            return Err(AdapterError::InvalidPayload(
                "exact Kalshi ticker lookup unexpectedly returned a pagination cursor".to_owned(),
            ));
        }
        let [market] = response.markets.as_slice() else {
            return Err(AdapterError::InvalidPayload(
                "Kalshi exact ticker lookup must return one market".to_owned(),
            ));
        };
        validate_market_ticker(&market.ticker)?;
        if market.ticker != expected_ticker {
            return Err(AdapterError::InvalidPayload(
                "Kalshi REST market ticker does not match the request".to_owned(),
            ));
        }
        if market.market_type != "binary" || market.strike_type != "greater_or_equal" {
            return Err(AdapterError::InvalidPayload(
                "Kalshi BTC market must be binary with greater_or_equal strike semantics"
                    .to_owned(),
            ));
        }
        validate_identifier(&market.event_ticker, "event_ticker")?;
        validate_text(&market.title, "title", 1_024)?;
        validate_text(&market.status, "status", 64)?;
        validate_text(&market.rules_primary, "rules_primary", 32_768)?;
        validate_text(&market.rules_secondary, "rules_secondary", 32_768)?;
        let primary = market.rules_primary.to_ascii_lowercase();
        let secondary = market.rules_secondary.to_ascii_lowercase();
        if !primary.contains("cf benchmarks")
            || !primary.contains("brti")
            || !primary.contains("simple average")
            || !primary.contains("sixty seconds")
            || !primary.contains("at least")
            || !secondary.contains("60 rti prices")
        {
            return Err(AdapterError::InvalidPayload(
                "Kalshi rules do not prove CF Benchmarks BRTI sixty-print >= semantics".to_owned(),
            ));
        }
        let open = parse_timestamp(&market.open_time, "open_time")?;
        let close = parse_timestamp(&market.close_time, "close_time")?;
        let expected =
            parse_timestamp(&market.expected_expiration_time, "expected_expiration_time")?;
        let legal = parse_timestamp(&market.expiration_time, "expiration_time")?;
        if close - open != chrono::Duration::minutes(15)
            || expected < close
            || expected > close + chrono::Duration::hours(24)
            || legal < expected
        {
            return Err(AdapterError::InvalidPayload(
                "Kalshi open, close, expected-expiration, and legal-expiration times disagree"
                    .to_owned(),
            ));
        }
        let reference_value = canonical_raw_decimal(&market.floor_strike, "floor_strike")?;
        validate_timestamp(&market.updated_time, "updated_time")?;
        let mut rules = Sha256::new();
        rules.update(market.rules_primary.as_bytes());
        rules.update([0]);
        rules.update(market.rules_secondary.as_bytes());
        Ok(Self {
            market_ticker: market.ticker.clone(),
            event_ticker: market.event_ticker.clone(),
            title: market.title.clone(),
            status: market.status.clone(),
            open_time: canonical_timestamp(&market.open_time, "open_time")?,
            close_time: canonical_timestamp(&market.close_time, "close_time")?,
            expected_expiration_time: canonical_timestamp(
                &market.expected_expiration_time,
                "expected_expiration_time",
            )?,
            legal_expiration_time: canonical_timestamp(&market.expiration_time, "expiration_time")?,
            reference_value,
            rules_sha256: format!("{:x}", rules.finalize()),
            event_time: canonical_timestamp(&market.updated_time, "updated_time")?,
            received_at: canonical_timestamp(retrieved_at, "retrieved_at")?,
        })
    }

    fn values(&self) -> BTreeMap<String, Value> {
        BTreeMap::from([
            ("market_ticker".to_owned(), json!(self.market_ticker)),
            ("event_ticker".to_owned(), json!(self.event_ticker)),
            ("title".to_owned(), json!(self.title)),
            ("status".to_owned(), json!(self.status)),
            ("open_time".to_owned(), json!(self.open_time)),
            ("close_time".to_owned(), json!(self.close_time)),
            (
                "expected_expiration_time".to_owned(),
                json!(self.expected_expiration_time),
            ),
            (
                "legal_expiration_time".to_owned(),
                json!(self.legal_expiration_time),
            ),
            ("benchmark".to_owned(), json!("cf_benchmarks_brti")),
            ("averaging_window_seconds".to_owned(), json!(60)),
            ("comparator".to_owned(), json!("greater_than_or_equal")),
            ("tie_outcome".to_owned(), json!("yes")),
            ("reference_value".to_owned(), json!(self.reference_value)),
            ("rules_sha256".to_owned(), json!(self.rules_sha256)),
            ("event_time".to_owned(), json!(self.event_time)),
        ])
    }
}

#[derive(Default)]
struct BookState {
    bids: BTreeMap<Decimal, Decimal>,
    asks: BTreeMap<Decimal, Decimal>,
    ready: bool,
    sid: Option<u64>,
    last_sequence: Option<u64>,
}

impl BookState {
    fn clear(&mut self) {
        self.bids.clear();
        self.asks.clear();
        self.ready = false;
        self.sid = None;
        self.last_sequence = None;
    }

    fn is_uncrossed(&self) -> bool {
        match (
            self.bids.keys().next_back().copied(),
            self.asks.keys().next().copied(),
        ) {
            (Some(bid), Some(ask)) => bid <= ask,
            _ => true,
        }
    }
}

/// Deterministic Kalshi REST-contract + authenticated market-channel adapter.
pub struct KalshiAdapter {
    publisher_epoch: String,
    guard: RecordGuard,
    contract: Option<MarketContract>,
    book: BookState,
    current_book_subscription: bool,
    current_ticker_subscription: bool,
    current_trade_subscription: bool,
    saw_book_subscription: bool,
    saw_ticker_subscription: bool,
    saw_trade_subscription: bool,
    next_contract_sequence: u64,
    next_book_sequence: u64,
    next_quote_sequence: u64,
    next_trade_sequence: u64,
    last_ticker_event_time: Option<DateTime<Utc>>,
    ticker_fingerprints_at_frontier: BTreeSet<String>,
    last_trade_event_time: Option<DateTime<Utc>>,
    seen_trades: BTreeMap<String, String>,
    trade_id_order: VecDeque<String>,
}

impl KalshiAdapter {
    /// Construct a Kalshi adapter.
    ///
    /// `publisher_epoch` is copied verbatim into every emitted event; it must be
    /// non-blank and at most 128 bytes, or the call returns
    /// [`AdapterError::InvalidRecord`].
    ///
    /// The adapter is not usable until it has ingested a Configuration record
    /// binding it to one market — see [`KalshiAdapter::configuration_payload`].
    pub fn new(publisher_epoch: impl Into<String>) -> Result<Self, AdapterError> {
        let publisher_epoch = publisher_epoch.into();
        validate_publisher_epoch(&publisher_epoch)?;
        Ok(Self {
            publisher_epoch,
            guard: RecordGuard::new(Provider::Kalshi),
            contract: None,
            book: BookState::default(),
            current_book_subscription: false,
            current_ticker_subscription: false,
            current_trade_subscription: false,
            saw_book_subscription: false,
            saw_ticker_subscription: false,
            saw_trade_subscription: false,
            next_contract_sequence: 0,
            next_book_sequence: 0,
            next_quote_sequence: 0,
            next_trade_sequence: 0,
            last_ticker_event_time: None,
            ticker_fingerprints_at_frontier: BTreeSet::new(),
            last_trade_event_time: None,
            seen_trades: BTreeMap::new(),
            trade_id_order: VecDeque::new(),
        })
    }

    /// Build the Configuration record payload this adapter's `configure()` step
    /// accepts, from a Kalshi markets REST body fetched by the caller.
    ///
    /// `rest_body` is the response of
    /// `GET https://external-api.kalshi.com/trade-api/v2/markets?tickers=<ticker>`
    /// verbatim, `request_path` is that request's path and query
    /// (`/trade-api/v2/markets?tickers=<ticker>` — the ticker is read back out of
    /// it), and `retrieved_at` is the RFC 3339 instant the response was
    /// received.
    ///
    /// # Journal format
    ///
    /// The returned string is a JSON object with exactly these fields, and
    /// nothing else:
    ///
    /// ```text
    /// {
    ///   "schema_version": 1,
    ///   "kind": "kalshi_btc_market_v1",
    ///   "request_path": "/trade-api/v2/markets?tickers=KXBTC15M-...",
    ///   "retrieved_at": "<RFC 3339>",
    ///   "response_body_sha256": "<lower-case hex SHA-256 of response_body>",
    ///   "response_body": "<the REST body, verbatim>"
    /// }
    /// ```
    ///
    /// The self-describing hash is what makes a capture replayable without the
    /// network: `configure()` recomputes it and refuses a journal whose body was
    /// edited. Ingest it as the stream's first record, with connection epoch 1
    /// and schema version 3:
    ///
    /// ```text
    /// RawRecord::configuration_at(CaptureStream::KalshiMarket, 1, 1, payload, received_at, 0)
    /// ```
    ///
    /// Returns [`AdapterError::InvalidPayload`] if the path is not an exact
    /// single-ticker markets query, or the body does not describe exactly one
    /// market meeting the BTC 15-minute contract rules.
    pub fn configuration_payload(
        rest_body: &str,
        request_path: &str,
        retrieved_at: &str,
    ) -> Result<String, AdapterError> {
        let ticker = request_path
            .strip_prefix("/trade-api/v2/markets?tickers=")
            .ok_or_else(|| {
                AdapterError::InvalidPayload("invalid Kalshi discovery request path".to_owned())
            })?;
        validate_market_ticker(ticker)?;
        validate_timestamp(retrieved_at, "retrieved_at")?;
        discovery_from_response(ticker, retrieved_at.to_owned(), rest_body.to_owned())
            .map(|discovery| discovery.journal_payload)
            .map_err(|error| AdapterError::InvalidPayload(error.to_string()))
    }

    /// Assert the capture was complete. Call once, after the last record.
    ///
    /// Returns [`AdapterError::InvalidRecord`] if the stream carried no
    /// Configuration record, or if any of the book, ticker and trade
    /// subscriptions was never observed — a Kalshi capture missing one of the
    /// three is not evidence of that channel's absence, so it fails closed.
    pub fn finish(&self) -> Result<(), AdapterError> {
        if self.contract.is_none() {
            return Err(AdapterError::InvalidRecord(
                "Kalshi capture is missing its REST configuration record".to_owned(),
            ));
        }
        if !self.saw_book_subscription
            || !self.saw_ticker_subscription
            || !self.saw_trade_subscription
        {
            return Err(AdapterError::InvalidRecord(
                "Kalshi capture is missing a required recorded market-data subscription".to_owned(),
            ));
        }
        Ok(())
    }

    fn configure(&mut self, payload: &str) -> Result<IngestOutcome, AdapterError> {
        if self.contract.is_some() {
            return Err(AdapterError::InvalidRecord(
                "Kalshi capture contains multiple configuration records".to_owned(),
            ));
        }
        let contract = MarketContract::from_journal(payload)?;
        let event = self.source_event(
            MARKET_CONTRACT_SOURCE_ID,
            contract.event_time.clone(),
            contract.received_at.clone(),
            contract.values(),
        )?;
        self.contract = Some(contract);
        Ok(IngestOutcome {
            events: vec![event],
            notices: Vec::new(),
        })
    }

    fn outbound(&mut self, payload: &str) -> Result<IngestOutcome, AdapterError> {
        let subscription: OutboundSubscription =
            serde_json::from_str(payload).map_err(payload_error)?;
        if subscription.cmd != "subscribe" || subscription.id == 0 {
            return Err(AdapterError::InvalidPayload(
                "Kalshi outbound record must be a nonzero subscribe command".to_owned(),
            ));
        }
        let contract = self.contract.as_ref().ok_or_else(|| {
            AdapterError::InvalidRecord("Kalshi subscription precedes configuration".to_owned())
        })?;
        let tickers = subscription.params.market_tickers();
        if tickers != [contract.market_ticker.as_str()] {
            return Err(AdapterError::InvalidPayload(
                "Kalshi subscription ticker disagrees with REST discovery".to_owned(),
            ));
        }
        if subscription.params.channels.is_empty() || subscription.params.channels.len() > 3 {
            return Err(AdapterError::InvalidPayload(
                "Kalshi subscription must contain one to three bounded channels".to_owned(),
            ));
        }
        for channel in subscription.params.channels {
            match channel.as_str() {
                "orderbook_delta" => {
                    if subscription.params.use_yes_price != Some(true) {
                        return Err(AdapterError::InvalidPayload(
                            "Kalshi orderbook subscription must pin use_yes_price=true".to_owned(),
                        ));
                    }
                    self.current_book_subscription = true;
                    self.saw_book_subscription = true;
                }
                "ticker" => {
                    self.current_ticker_subscription = true;
                    self.saw_ticker_subscription = true;
                }
                "trade" => {
                    self.current_trade_subscription = true;
                    self.saw_trade_subscription = true;
                }
                _ => {
                    return Err(AdapterError::InvalidPayload(
                        "Kalshi capture contains an unexpected subscription channel".to_owned(),
                    ));
                }
            }
        }
        Ok(IngestOutcome::default())
    }

    fn inbound(
        &mut self,
        record: &RawRecord,
        payload: &str,
    ) -> Result<IngestOutcome, AdapterError> {
        let envelope: WebSocketEnvelope = serde_json::from_str(payload).map_err(payload_error)?;
        match envelope.message_type.as_str() {
            "subscribed" | "ok" => Ok(IngestOutcome::notice(FeedNotice::info(
                FeedNoticeCode::SubscriptionAccepted,
                "Kalshi market-data subscription acknowledged",
            ))),
            "error" => {
                let error: ErrorMessage =
                    serde_json::from_value(envelope.msg).map_err(payload_error)?;
                Err(AdapterError::InvalidPayload(format!(
                    "Kalshi rejected a market-data command with bounded code {}: {}",
                    error.code,
                    bounded_token(&error.message)
                )))
            }
            "orderbook_snapshot" => self.book_snapshot(record, envelope),
            "orderbook_delta" => self.book_delta(record, envelope),
            "ticker" => self.ticker(record, envelope),
            "trade" => self.trade(record, envelope),
            other => Ok(IngestOutcome::notice(FeedNotice::info(
                FeedNoticeCode::UnsupportedMessage,
                format!(
                    "retained unsupported Kalshi message type `{}`",
                    bounded_token(other)
                ),
            ))),
        }
    }

    fn book_snapshot(
        &mut self,
        record: &RawRecord,
        envelope: WebSocketEnvelope,
    ) -> Result<IngestOutcome, AdapterError> {
        if !self.current_book_subscription {
            return Err(AdapterError::InvalidRecord(
                "Kalshi book snapshot arrived before recorded subscription".to_owned(),
            ));
        }
        let sid = envelope.sid.ok_or_else(|| missing("sid"))?;
        let sequence = envelope.seq.ok_or_else(|| missing("seq"))?;
        let message: BookSnapshot = serde_json::from_value(envelope.msg).map_err(payload_error)?;
        self.check_ticker(&message.market_ticker)?;
        let total = message
            .yes_dollars_fp
            .len()
            .checked_add(message.no_dollars_fp.len())
            .ok_or_else(|| {
                AdapterError::InvalidPayload("Kalshi book row count overflowed".to_owned())
            })?;
        // Per ruling E61 a zero-level snapshot is a VALID empty book — a market
        // that just opened, or one whose sides are momentarily empty — not an
        // invalid payload. It routes through with no row events, resetting the
        // book to an empty-but-ready state so a matching delta still applies; the
        // downstream book identities go non-ready with their existing empty /
        // one-sided reasons rather than the stream failing closed. Only a snapshot
        // that overflows the level bound remains a refusal.
        if total > MAX_BOOK_ROWS {
            return Err(AdapterError::InvalidPayload(format!(
                "Kalshi book snapshot must contain at most {MAX_BOOK_ROWS} levels"
            )));
        }
        let update_count = u32::try_from(total).map_err(|_| {
            AdapterError::InvalidPayload("Kalshi book count exceeds u32".to_owned())
        })?;
        let mut bids = BTreeMap::new();
        let mut asks = BTreeMap::new();
        let mut rows = Vec::with_capacity(total);
        for (side, levels) in [
            ("bid", message.yes_dollars_fp),
            ("ask", message.no_dollars_fp),
        ] {
            for [price, size] in levels {
                let (price_text, price_value) = canonical_probability(&price, "price")?;
                let (size_text, size_value) = canonical_nonnegative(&size, "size")?;
                if size_value.is_zero() {
                    return Err(AdapterError::InvalidPayload(
                        "Kalshi snapshot contains a zero-size level".to_owned(),
                    ));
                }
                let map = if side == "bid" { &mut bids } else { &mut asks };
                if map.insert(price_value, size_value).is_some() {
                    return Err(AdapterError::InvalidPayload(
                        "Kalshi snapshot contains a duplicate side/price level".to_owned(),
                    ));
                }
                rows.push((side, price_text, size_text));
            }
        }
        let candidate = BookState {
            bids,
            asks,
            ready: true,
            sid: Some(sid),
            last_sequence: Some(sequence),
        };
        if !candidate.is_uncrossed() {
            return Err(AdapterError::InvalidPayload(
                "Kalshi use_yes_price book snapshot is crossed".to_owned(),
            ));
        }
        self.book = candidate;
        let event_time = canonical_timestamp(&record.received_at, "received_at")?;
        let update_group = format!("{sid}:{sequence}");
        let mut events = Vec::with_capacity(rows.len());
        for (index, (side, price, size)) in rows.into_iter().enumerate() {
            let update_index = u32::try_from(index).map_err(|_| {
                AdapterError::InvalidPayload("Kalshi book index exceeds u32".to_owned())
            })?;
            let values = self.book_values(
                sequence,
                &update_group,
                update_index,
                update_count,
                "snapshot",
                side,
                price,
                size,
                &event_time,
                "local_receipt",
            )?;
            events.push(self.source_event(
                BOOK_SOURCE_ID,
                event_time.clone(),
                record.received_at.clone(),
                values,
            )?);
        }
        Ok(IngestOutcome {
            events,
            notices: Vec::new(),
        })
    }

    fn book_delta(
        &mut self,
        record: &RawRecord,
        envelope: WebSocketEnvelope,
    ) -> Result<IngestOutcome, AdapterError> {
        if !self.current_book_subscription {
            return Err(AdapterError::InvalidRecord(
                "Kalshi book delta arrived before recorded subscription".to_owned(),
            ));
        }
        let sid = envelope.sid.ok_or_else(|| missing("sid"))?;
        let sequence = envelope.seq.ok_or_else(|| missing("seq"))?;
        let message: BookDelta = serde_json::from_value(envelope.msg).map_err(payload_error)?;
        self.check_ticker(&message.market_ticker)?;
        if !self.book.ready || self.book.sid != Some(sid) {
            return Ok(IngestOutcome::notice(FeedNotice::recovery(
                FeedNoticeCode::AwaitingSnapshot,
                "Kalshi book delta ignored until a matching fresh snapshot arrives",
                RecoveryAction::ResubscribeKalshiBook {
                    market_ticker: message.market_ticker,
                },
            )));
        }
        let expected = self
            .book
            .last_sequence
            .and_then(|value| value.checked_add(1))
            .ok_or(AdapterError::SequenceExhausted)?;
        if sequence < expected {
            return Ok(IngestOutcome::notice(FeedNotice::info(
                if sequence.checked_add(1) == Some(expected) {
                    FeedNoticeCode::Duplicate
                } else {
                    FeedNoticeCode::OutOfOrder
                },
                "Kalshi orderbook sequence was not newer than local state",
            )));
        }
        if sequence > expected {
            self.book.clear();
            return Ok(IngestOutcome::notice(FeedNotice::recovery(
                FeedNoticeCode::Gap,
                format!("Kalshi orderbook sequence gap: expected {expected}, received {sequence}"),
                RecoveryAction::ResubscribeKalshiBook {
                    market_ticker: message.market_ticker,
                },
            )));
        }
        let (price, price_value) = canonical_probability(&message.price_dollars, "price_dollars")?;
        let (_, delta_value) = canonical_signed(&message.delta_fp, "delta_fp")?;
        let side = match message.side.as_str() {
            "yes" => "bid",
            "no" => "ask",
            _ => {
                return Err(AdapterError::InvalidPayload(
                    "Kalshi orderbook side must be yes or no".to_owned(),
                ));
            }
        };
        let event_time = provider_timestamp(&message.ts_ms, message.ts.as_deref(), "book delta")?;
        let levels = if side == "bid" {
            &mut self.book.bids
        } else {
            &mut self.book.asks
        };
        let current = levels.get(&price_value).copied().unwrap_or(Decimal::ZERO);
        let size_value = current.checked_add(delta_value).ok_or_else(|| {
            AdapterError::InvalidPayload("Kalshi book size arithmetic overflowed".to_owned())
        })?;
        if size_value.is_sign_negative() {
            return Err(AdapterError::InvalidPayload(
                "Kalshi orderbook delta makes a level negative".to_owned(),
            ));
        }
        if size_value.is_zero() {
            levels.remove(&price_value);
        } else {
            levels.insert(price_value, size_value);
        }
        if !self.book.is_uncrossed() {
            self.book.clear();
            return Ok(IngestOutcome::notice(FeedNotice::recovery(
                FeedNoticeCode::StateInconsistent,
                "Kalshi use_yes_price book became crossed after a delta",
                RecoveryAction::ResubscribeKalshiBook {
                    market_ticker: message.market_ticker,
                },
            )));
        }
        self.book.last_sequence = Some(sequence);
        let size = size_value.normalize().to_string();
        let update_group = format!("{sid}:{sequence}");
        let values = self.book_values(
            sequence,
            &update_group,
            0,
            1,
            "delta",
            side,
            price,
            size,
            &event_time,
            "provider",
        )?;
        let event = self.source_event(
            BOOK_SOURCE_ID,
            event_time,
            record.received_at.clone(),
            values,
        )?;
        Ok(IngestOutcome {
            events: vec![event],
            notices: Vec::new(),
        })
    }

    fn ticker(
        &mut self,
        record: &RawRecord,
        envelope: WebSocketEnvelope,
    ) -> Result<IngestOutcome, AdapterError> {
        if !self.current_ticker_subscription {
            return Err(AdapterError::InvalidRecord(
                "Kalshi ticker arrived before recorded subscription".to_owned(),
            ));
        }
        let message: TickerMessage = serde_json::from_value(envelope.msg).map_err(payload_error)?;
        self.check_ticker(&message.market_ticker)?;
        let (last_price, _) = canonical_probability(&message.price_dollars, "price_dollars")?;
        let (best_bid, bid) = canonical_probability(&message.yes_bid_dollars, "yes_bid_dollars")?;
        let (best_ask, ask) = canonical_probability(&message.yes_ask_dollars, "yes_ask_dollars")?;
        if ask < bid {
            return Err(AdapterError::InvalidPayload(
                "Kalshi ticker YES ask is below YES bid".to_owned(),
            ));
        }
        let best_bid_size = canonical_nonnegative(&message.yes_bid_size_fp, "yes_bid_size_fp")?.0;
        let best_ask_size = canonical_nonnegative(&message.yes_ask_size_fp, "yes_ask_size_fp")?.0;
        let volume = canonical_nonnegative(&message.volume_fp, "volume_fp")?.0;
        let open_interest = canonical_nonnegative(&message.open_interest_fp, "open_interest_fp")?.0;
        let event_time = provider_timestamp(&message.ts_ms, message.time.as_deref(), "ticker")?;
        let parsed_event_time = parse_timestamp(&event_time, "ticker event_time")?;
        if self
            .last_ticker_event_time
            .is_some_and(|previous| parsed_event_time < previous)
        {
            return Ok(IngestOutcome::notice(FeedNotice::recovery(
                FeedNoticeCode::OutOfOrder,
                "ignored a Kalshi ticker older than the accepted ticker frontier",
                RecoveryAction::Reconnect,
            )));
        }
        let ticker_fingerprint = sha256_hex(
            [
                message.market_ticker.as_str(),
                event_time.as_str(),
                last_price.as_str(),
                best_bid.as_str(),
                best_ask.as_str(),
                best_bid_size.as_str(),
                best_ask_size.as_str(),
                volume.as_str(),
                open_interest.as_str(),
            ]
            .join("\0")
            .as_bytes(),
        );
        let advances_frontier = self
            .last_ticker_event_time
            .is_none_or(|previous| parsed_event_time > previous);
        if !advances_frontier
            && self
                .ticker_fingerprints_at_frontier
                .contains(&ticker_fingerprint)
        {
            return Ok(IngestOutcome::notice(FeedNotice::info(
                FeedNoticeCode::Duplicate,
                "ignored an exact Kalshi ticker redelivery",
            )));
        }
        if !advances_frontier
            && self.ticker_fingerprints_at_frontier.len() >= MAX_TICKER_STATES_PER_MILLISECOND
        {
            return Err(AdapterError::InvalidPayload(
                "Kalshi ticker states exceed the per-millisecond safety bound".to_owned(),
            ));
        }
        let values = BTreeMap::from([
            ("market_ticker".to_owned(), json!(message.market_ticker)),
            ("last_price".to_owned(), json!(last_price)),
            ("best_bid".to_owned(), json!(best_bid)),
            ("best_ask".to_owned(), json!(best_ask)),
            ("best_bid_size".to_owned(), json!(best_bid_size)),
            ("best_ask_size".to_owned(), json!(best_ask_size)),
            ("volume".to_owned(), json!(volume)),
            ("open_interest".to_owned(), json!(open_interest)),
            ("event_time".to_owned(), json!(event_time)),
        ]);
        let event = self.source_event(
            QUOTE_SOURCE_ID,
            event_time,
            record.received_at.clone(),
            values,
        )?;
        if advances_frontier {
            self.ticker_fingerprints_at_frontier.clear();
        }
        self.ticker_fingerprints_at_frontier
            .insert(ticker_fingerprint);
        self.last_ticker_event_time = Some(parsed_event_time);
        Ok(IngestOutcome {
            events: vec![event],
            notices: Vec::new(),
        })
    }

    fn trade(
        &mut self,
        record: &RawRecord,
        envelope: WebSocketEnvelope,
    ) -> Result<IngestOutcome, AdapterError> {
        if !self.current_trade_subscription {
            return Err(AdapterError::InvalidRecord(
                "Kalshi trade arrived before recorded subscription".to_owned(),
            ));
        }
        let message: TradeMessage = serde_json::from_value(envelope.msg).map_err(payload_error)?;
        self.check_ticker(&message.market_ticker)?;
        validate_identifier(&message.trade_id, "trade_id")?;
        let (yes_price, yes) =
            canonical_probability(&message.yes_price_dollars, "yes_price_dollars")?;
        let (no_price, no) = canonical_probability(&message.no_price_dollars, "no_price_dollars")?;
        if yes.checked_add(no) != Some(Decimal::ONE) {
            return Err(AdapterError::InvalidPayload(
                "Kalshi YES and NO trade prices must sum to 1".to_owned(),
            ));
        }
        let expected_book_side = match message.taker_outcome_side.as_str() {
            "yes" => "bid",
            "no" => "ask",
            _ => {
                return Err(AdapterError::InvalidPayload(
                    "Kalshi taker_outcome_side must be yes or no".to_owned(),
                ));
            }
        };
        if message.taker_book_side != expected_book_side {
            return Err(AdapterError::InvalidPayload(
                "Kalshi canonical taker outcome/book sides disagree".to_owned(),
            ));
        }
        let event_time = provider_timestamp(&message.ts_ms, None, "trade")?;
        let parsed_event_time = parse_timestamp(&event_time, "trade event_time")?;
        if self
            .last_trade_event_time
            .is_some_and(|previous| parsed_event_time < previous)
        {
            return Ok(IngestOutcome::notice(FeedNotice::recovery(
                FeedNoticeCode::OutOfOrder,
                "ignored a Kalshi trade older than the accepted trade frontier",
                RecoveryAction::Reconnect,
            )));
        }
        let count = canonical_nonnegative(&message.count_fp, "count_fp")?.0;
        let trade_fingerprint = sha256_hex(
            [
                message.market_ticker.as_str(),
                event_time.as_str(),
                yes_price.as_str(),
                no_price.as_str(),
                count.as_str(),
                message.taker_outcome_side.as_str(),
                message.taker_book_side.as_str(),
                if message.is_block_trade {
                    "true"
                } else {
                    "false"
                },
            ]
            .join("\0")
            .as_bytes(),
        );
        if let Some(existing) = self.seen_trades.get(&message.trade_id) {
            if existing == &trade_fingerprint {
                return Ok(IngestOutcome::notice(FeedNotice::info(
                    FeedNoticeCode::Duplicate,
                    "ignored an exact Kalshi trade redelivery",
                )));
            }
            return Err(AdapterError::InvalidPayload(
                "Kalshi trade_id was reused with conflicting contents".to_owned(),
            ));
        }
        let values = BTreeMap::from([
            ("trade_id".to_owned(), json!(message.trade_id)),
            ("market_ticker".to_owned(), json!(message.market_ticker)),
            ("yes_price".to_owned(), json!(yes_price)),
            ("no_price".to_owned(), json!(no_price)),
            ("count".to_owned(), json!(count)),
            (
                "taker_outcome_side".to_owned(),
                json!(message.taker_outcome_side),
            ),
            ("taker_book_side".to_owned(), json!(message.taker_book_side)),
            ("is_block_trade".to_owned(), json!(message.is_block_trade)),
            ("event_time".to_owned(), json!(event_time)),
        ]);
        let event = self.source_event(
            TRADE_SOURCE_ID,
            event_time,
            record.received_at.clone(),
            values,
        )?;
        self.remember_trade(message.trade_id, trade_fingerprint);
        self.last_trade_event_time = Some(parsed_event_time);
        Ok(IngestOutcome {
            events: vec![event],
            notices: Vec::new(),
        })
    }

    fn remember_trade(&mut self, trade_id: String, fingerprint: String) {
        if self
            .seen_trades
            .insert(trade_id.clone(), fingerprint)
            .is_none()
        {
            self.trade_id_order.push_back(trade_id);
        }
        while self.trade_id_order.len() > MAX_TRADE_IDS {
            if let Some(oldest) = self.trade_id_order.pop_front() {
                self.seen_trades.remove(&oldest);
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn book_values(
        &self,
        upstream_sequence: u64,
        update_group: &str,
        update_index: u32,
        update_count: u32,
        update_kind: &str,
        side: &str,
        price: String,
        size: String,
        event_time: &str,
        event_time_basis: &str,
    ) -> Result<BTreeMap<String, Value>, AdapterError> {
        let market_ticker = self
            .contract
            .as_ref()
            .ok_or_else(|| AdapterError::InvalidRecord("missing Kalshi contract".to_owned()))?
            .market_ticker
            .clone();
        Ok(BTreeMap::from([
            ("market_ticker".to_owned(), json!(market_ticker)),
            ("upstream_sequence".to_owned(), json!(upstream_sequence)),
            ("update_group".to_owned(), json!(update_group)),
            ("update_index".to_owned(), json!(update_index)),
            ("update_count".to_owned(), json!(update_count)),
            ("update_kind".to_owned(), json!(update_kind)),
            ("side".to_owned(), json!(side)),
            ("price".to_owned(), json!(price)),
            ("size".to_owned(), json!(size)),
            ("event_time".to_owned(), json!(event_time)),
            ("event_time_basis".to_owned(), json!(event_time_basis)),
        ]))
    }

    fn check_ticker(&self, ticker: &str) -> Result<(), AdapterError> {
        let expected = &self
            .contract
            .as_ref()
            .ok_or_else(|| AdapterError::InvalidRecord("missing Kalshi contract".to_owned()))?
            .market_ticker;
        if ticker != expected {
            return Err(AdapterError::InvalidPayload(
                "Kalshi WebSocket market ticker disagrees with REST discovery".to_owned(),
            ));
        }
        Ok(())
    }

    fn source_event(
        &mut self,
        source_id: &'static str,
        event_time: String,
        received_at: String,
        values: BTreeMap<String, Value>,
    ) -> Result<SourceEvent, AdapterError> {
        let sequence = match source_id {
            MARKET_CONTRACT_SOURCE_ID => &mut self.next_contract_sequence,
            BOOK_SOURCE_ID => &mut self.next_book_sequence,
            QUOTE_SOURCE_ID => &mut self.next_quote_sequence,
            TRADE_SOURCE_ID => &mut self.next_trade_sequence,
            _ => {
                return Err(AdapterError::InvalidRecord(
                    "unknown Kalshi source identity".to_owned(),
                ));
            }
        };
        let current = *sequence;
        *sequence = sequence
            .checked_add(1)
            .ok_or(AdapterError::SequenceExhausted)?;
        Ok(SourceEvent {
            schema_version: 1,
            source_id: source_id.to_owned(),
            source_version: SOURCE_VERSION.to_owned(),
            publisher_epoch: self.publisher_epoch.clone(),
            sequence: current,
            event_time,
            received_at,
            values,
        })
    }
}

impl FeedAdapter for KalshiAdapter {
    fn ingest(&mut self, record: &RawRecord) -> Result<IngestOutcome, AdapterError> {
        match self.guard.observe(record)? {
            GuardAction::Configuration(payload) => self.configure(payload),
            GuardAction::Connected => {
                if self.contract.is_none() {
                    return Err(AdapterError::InvalidRecord(
                        "Kalshi connection began before REST configuration".to_owned(),
                    ));
                }
                self.book.clear();
                self.current_book_subscription = false;
                self.current_ticker_subscription = false;
                self.current_trade_subscription = false;
                Ok(IngestOutcome::notice(FeedNotice::info(
                    FeedNoticeCode::Connected,
                    "Kalshi authenticated market WebSocket connected; book awaits snapshot",
                )))
            }
            GuardAction::Disconnected => {
                self.book.clear();
                self.current_book_subscription = false;
                self.current_ticker_subscription = false;
                self.current_trade_subscription = false;
                Ok(IngestOutcome::notice(FeedNotice::recovery(
                    FeedNoticeCode::Disconnected,
                    "Kalshi market WebSocket disconnected",
                    RecoveryAction::Reconnect,
                )))
            }
            GuardAction::Outbound => self.outbound(record.payload.as_deref().ok_or_else(|| {
                AdapterError::InvalidRecord("missing outbound payload".to_owned())
            })?),
            GuardAction::Failed(_) => Err(AdapterError::InvalidRecord(
                "Kalshi market captures do not accept failure records".to_owned(),
            )),
            GuardAction::Inbound(payload) => self.inbound(record, payload),
        }
    }
}

#[derive(Deserialize)]
struct MarketsResponse {
    markets: Vec<RestMarket>,
    cursor: String,
}

#[derive(Deserialize)]
struct RestMarket {
    ticker: String,
    event_ticker: String,
    market_type: String,
    title: String,
    status: String,
    open_time: String,
    close_time: String,
    expected_expiration_time: String,
    expiration_time: String,
    updated_time: String,
    rules_primary: String,
    rules_secondary: String,
    strike_type: String,
    floor_strike: Box<RawValue>,
}

#[derive(Deserialize)]
struct OutboundSubscription {
    id: u64,
    cmd: String,
    params: OutboundParams,
}

#[derive(Deserialize)]
struct OutboundParams {
    channels: Vec<String>,
    #[serde(default)]
    market_ticker: Option<String>,
    #[serde(default)]
    market_tickers: Vec<String>,
    #[serde(default)]
    use_yes_price: Option<bool>,
}

impl OutboundParams {
    fn market_tickers(&self) -> Vec<&str> {
        self.market_ticker
            .iter()
            .map(String::as_str)
            .chain(self.market_tickers.iter().map(String::as_str))
            .collect()
    }
}

#[derive(Deserialize)]
struct WebSocketEnvelope {
    #[serde(rename = "type")]
    message_type: String,
    #[serde(default)]
    sid: Option<u64>,
    #[serde(default)]
    seq: Option<u64>,
    msg: Value,
}

#[derive(Deserialize)]
struct ErrorMessage {
    code: u64,
    #[serde(rename = "msg")]
    message: String,
}

#[derive(Deserialize)]
struct BookSnapshot {
    market_ticker: String,
    #[serde(default)]
    yes_dollars_fp: Vec<[String; 2]>,
    #[serde(default)]
    no_dollars_fp: Vec<[String; 2]>,
}

#[derive(Deserialize)]
struct BookDelta {
    market_ticker: String,
    price_dollars: String,
    delta_fp: String,
    side: String,
    #[serde(default)]
    ts: Option<String>,
    ts_ms: UnixMillis,
}

#[derive(Deserialize)]
struct TickerMessage {
    market_ticker: String,
    price_dollars: String,
    yes_bid_dollars: String,
    yes_ask_dollars: String,
    volume_fp: String,
    open_interest_fp: String,
    yes_bid_size_fp: String,
    yes_ask_size_fp: String,
    ts_ms: UnixMillis,
    #[serde(default)]
    time: Option<String>,
}

#[derive(Deserialize)]
struct TradeMessage {
    trade_id: String,
    market_ticker: String,
    yes_price_dollars: String,
    no_price_dollars: String,
    count_fp: String,
    taker_outcome_side: String,
    taker_book_side: String,
    #[serde(default)]
    is_block_trade: bool,
    ts_ms: UnixMillis,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum UnixMillis {
    String(String),
    Number(serde_json::Number),
}

impl UnixMillis {
    fn text(&self) -> String {
        match self {
            Self::String(value) => value.clone(),
            Self::Number(value) => value.to_string(),
        }
    }
}

fn provider_timestamp(
    millis: &UnixMillis,
    corroborating_rfc3339: Option<&str>,
    field: &str,
) -> Result<String, AdapterError> {
    let text = millis.text();
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(AdapterError::InvalidPayload(format!(
            "Kalshi {field} timestamp must be Unix milliseconds"
        )));
    }
    let millis = text.parse::<i64>().map_err(|_| {
        AdapterError::InvalidPayload(format!("Kalshi {field} timestamp exceeds i64"))
    })?;
    let value = Utc.timestamp_millis_opt(millis).single().ok_or_else(|| {
        AdapterError::InvalidPayload(format!("Kalshi {field} timestamp is outside UTC range"))
    })?;
    if let Some(corroborating) = corroborating_rfc3339 {
        let corroborating = parse_timestamp(corroborating, field)?;
        if corroborating.timestamp_millis() != millis {
            return Err(AdapterError::InvalidPayload(format!(
                "Kalshi {field} ts_ms and RFC3339 time disagree"
            )));
        }
    }
    Ok(value.to_rfc3339_opts(SecondsFormat::Millis, true))
}

fn canonical_probability(value: &str, field: &str) -> Result<(String, Decimal), AdapterError> {
    let (text, decimal) = canonical_nonnegative(value, field)?;
    if decimal > Decimal::ONE {
        return Err(AdapterError::InvalidPayload(format!(
            "Kalshi {field} exceeds probability 1"
        )));
    }
    Ok((text, decimal))
}

fn canonical_nonnegative(value: &str, field: &str) -> Result<(String, Decimal), AdapterError> {
    let decimal = validate_decimal(value, field)?;
    Ok((decimal.normalize().to_string(), decimal))
}

fn canonical_signed(value: &str, field: &str) -> Result<(String, Decimal), AdapterError> {
    let decimal = signals_core::parse_decimal_text(value).map_err(|_| {
        AdapterError::InvalidPayload(format!("Kalshi {field} is not bounded exact decimal text"))
    })?;
    let canonical = decimal.normalize().to_string();
    Ok((canonical, decimal))
}

fn canonical_raw_decimal(raw: &RawValue, field: &str) -> Result<String, AdapterError> {
    let value = decimal_string(raw, field)?;
    canonical_nonnegative(&value, field).map(|(value, _)| value)
}

fn validate_identifier(value: &str, field: &str) -> Result<(), AdapterError> {
    if value.is_empty()
        || value.len() > 256
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(AdapterError::InvalidPayload(format!(
            "Kalshi {field} is invalid"
        )));
    }
    Ok(())
}

fn validate_text(value: &str, field: &str, maximum: usize) -> Result<(), AdapterError> {
    if value.trim().is_empty()
        || value.len() > maximum
        || value
            .chars()
            .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
    {
        return Err(AdapterError::InvalidPayload(format!(
            "Kalshi {field} is empty, oversized, or contains control bytes"
        )));
    }
    Ok(())
}

fn canonical_timestamp(value: &str, field: &str) -> Result<String, AdapterError> {
    Ok(parse_timestamp(value, field)?.to_rfc3339_opts(SecondsFormat::AutoSi, true))
}

fn missing(field: &str) -> AdapterError {
    AdapterError::InvalidPayload(format!("Kalshi message is missing {field}"))
}

fn payload_error(error: serde_json::Error) -> AdapterError {
    AdapterError::InvalidPayload(error.to_string())
}

#[cfg(test)]
mod tests {
    use sha2::{Digest, Sha256};

    use super::{KalshiAdapter, canonical_signed, validate_market_ticker};
    use crate::{AdapterError, CaptureStream, FeedAdapter, FeedNoticeCode, RawRecord, RecordKind};

    const TICKER: &str = "KXBTC15M-26AUG141315-15";

    fn configuration_payload() -> String {
        let rest_body = serde_json::json!({
            "markets": [{
                "ticker": TICKER,
                "event_ticker": "KXBTC15M-26AUG141315",
                "market_type": "binary",
                "title": "Bitcoin price at 1:15 PM EDT?",
                "status": "active",
                "open_time": "2026-08-14T17:00:00Z",
                "close_time": "2026-08-14T17:15:00Z",
                "expected_expiration_time": "2026-08-14T17:20:00Z",
                "expiration_time": "2026-08-21T17:15:00Z",
                "updated_time": "2026-08-14T17:00:01Z",
                "rules_primary": "CF Benchmarks BRTI simple average over sixty seconds; YES if the value is at least the strike.",
                "rules_secondary": "The exchange uses 60 RTI prices.",
                "strike_type": "greater_or_equal",
                "floor_strike": 63075.56
            }],
            "cursor": ""
        })
        .to_string();
        serde_json::json!({
            "schema_version": 1,
            "kind": "kalshi_btc_market_v1",
            "request_path": format!("/trade-api/v2/markets?tickers={TICKER}"),
            "retrieved_at": "2026-08-14T17:00:02Z",
            "response_body_sha256": format!("{:x}", Sha256::digest(rest_body.as_bytes())),
            "response_body": rest_body
        })
        .to_string()
    }

    fn text(ordinal: u64, kind: RecordKind, payload: &str) -> RawRecord {
        RawRecord::text_at(
            CaptureStream::KalshiMarket,
            1,
            ordinal,
            kind,
            payload.to_owned(),
            "2026-08-14T17:00:04Z".to_owned(),
            20,
        )
    }

    /// A subscribed adapter whose book is at sequence 2 after a snapshot.
    fn subscribed_adapter() -> KalshiAdapter {
        let mut adapter = KalshiAdapter::new("test").expect("adapter constructs");
        adapter
            .ingest(&RawRecord::configuration_at(
                CaptureStream::KalshiMarket,
                1,
                1,
                configuration_payload(),
                "2026-08-14T17:00:02Z".to_owned(),
                0,
            ))
            .expect("configuration is accepted");
        adapter
            .ingest(&RawRecord::connected_at(
                CaptureStream::KalshiMarket,
                1,
                2,
                "2026-08-14T17:00:03Z".to_owned(),
                10,
                5,
            ))
            .expect("connection opens");
        for (ordinal, channel) in [(3, "orderbook_delta"), (4, "ticker"), (5, "trade")] {
            adapter
                .ingest(&text(
                    ordinal,
                    RecordKind::OutboundText,
                    &serde_json::json!({
                        "id": ordinal,
                        "cmd": "subscribe",
                        "params": {
                            "channels": [channel],
                            "market_ticker": TICKER,
                            "use_yes_price": true
                        }
                    })
                    .to_string(),
                ))
                .expect("subscription is accepted");
        }
        adapter
            .ingest(&text(
                6,
                RecordKind::InboundText,
                &serde_json::json!({
                    "type": "orderbook_snapshot",
                    "sid": 11,
                    "seq": 2,
                    "msg": {
                        "market_ticker": TICKER,
                        "yes_dollars_fp": [["0.4500", "10.00"]],
                        "no_dollars_fp": [["0.5500", "12.00"]]
                    }
                })
                .to_string(),
            ))
            .expect("snapshot normalizes");
        adapter
    }

    #[test]
    fn malformed_json_is_refused_rather_than_skipped() {
        let mut adapter = subscribed_adapter();
        let error = adapter
            .ingest(&text(
                7,
                RecordKind::InboundText,
                r#"{"type":"orderbook_delta""#,
            ))
            .expect_err("a truncated frame must not be silently ignored");
        assert!(matches!(error, AdapterError::InvalidPayload(_)));
    }

    #[test]
    fn an_unknown_message_type_is_retained_as_a_notice_without_output() {
        let mut adapter = subscribed_adapter();
        let outcome = adapter
            .ingest(&text(
                7,
                RecordKind::InboundText,
                &serde_json::json!({"type": "market_positions", "sid": 11, "seq": 3, "msg": {}})
                    .to_string(),
            ))
            .expect("an unmodelled type is not an error");
        assert!(outcome.events.is_empty());
        assert_eq!(outcome.notices[0].code, FeedNoticeCode::UnsupportedMessage);
    }

    #[test]
    fn a_zero_level_snapshot_is_a_valid_empty_book() {
        let mut adapter = subscribed_adapter();
        // `subscribed_adapter` leaves the book populated from a two-level
        // snapshot; a following empty snapshot resets it to empty-but-ready
        // without emitting rows and without refusing the stream (ruling E61).
        let outcome = adapter
            .ingest(&text(
                7,
                RecordKind::InboundText,
                &serde_json::json!({
                    "type": "orderbook_snapshot",
                    "sid": 11,
                    "seq": 3,
                    "msg": {
                        "market_ticker": TICKER,
                        "yes_dollars_fp": [],
                        "no_dollars_fp": []
                    }
                })
                .to_string(),
            ))
            .expect("an empty book snapshot is a valid empty book, not a refusal");
        assert!(outcome.events.is_empty());
        assert!(outcome.notices.is_empty());
        assert!(adapter.book.ready);
        assert!(adapter.book.bids.is_empty());
        assert!(adapter.book.asks.is_empty());
        assert_eq!(adapter.book.sid, Some(11));
    }

    #[test]
    fn a_one_sided_snapshot_is_accepted() {
        let mut adapter = subscribed_adapter();
        let outcome = adapter
            .ingest(&text(
                7,
                RecordKind::InboundText,
                &serde_json::json!({
                    "type": "orderbook_snapshot",
                    "sid": 11,
                    "seq": 3,
                    "msg": {
                        "market_ticker": TICKER,
                        "yes_dollars_fp": [["0.4500", "10.00"]],
                        "no_dollars_fp": []
                    }
                })
                .to_string(),
            ))
            .expect("a one-sided book snapshot is accepted");
        assert_eq!(outcome.events.len(), 1);
        assert!(adapter.book.ready);
        assert!(adapter.book.asks.is_empty());
        assert_eq!(adapter.book.bids.len(), 1);
    }

    #[test]
    fn an_unsupported_shape_is_a_notice_but_an_invalid_modelled_value_refuses() {
        // Ruling E61: an unsupported message shape is a counted notice and the
        // stream continues, while a genuinely invalid value in a modelled
        // message stays fail-closed.
        let mut adapter = subscribed_adapter();
        let notice = adapter
            .ingest(&text(
                7,
                RecordKind::InboundText,
                &serde_json::json!({"type": "market_positions", "sid": 11, "seq": 3, "msg": {}})
                    .to_string(),
            ))
            .expect("an unmodelled message type is never an AdapterError");
        assert!(notice.events.is_empty());
        assert_eq!(notice.notices[0].code, FeedNoticeCode::UnsupportedMessage);

        let error = adapter
            .ingest(&text(
                8,
                RecordKind::InboundText,
                &serde_json::json!({
                    "type": "orderbook_snapshot",
                    "sid": 11,
                    "seq": 4,
                    "msg": {
                        "market_ticker": TICKER,
                        "yes_dollars_fp": [["0.4500", "0.00"]],
                        "no_dollars_fp": []
                    }
                })
                .to_string(),
            ))
            .expect_err("a zero-size level in a modelled snapshot still refuses");
        assert!(matches!(error, AdapterError::InvalidPayload(_)));
    }

    #[test]
    fn a_book_sequence_below_the_frontier_is_refused_output() {
        let mut adapter = subscribed_adapter();
        let outcome = adapter
            .ingest(&text(
                7,
                RecordKind::InboundText,
                &serde_json::json!({
                    "type": "orderbook_delta",
                    "sid": 11,
                    "seq": 2,
                    "msg": {
                        "market_ticker": TICKER,
                        "price_dollars": "0.4500",
                        "delta_fp": "-2.00",
                        "side": "yes",
                        "ts": "2026-08-14T17:00:03Z",
                        "ts_ms": 1_786_726_803_000_u64
                    }
                })
                .to_string(),
            ))
            .expect("a stale sequence is a notice, not an error");
        assert!(outcome.events.is_empty());
        assert!(matches!(
            outcome.notices[0].code,
            FeedNoticeCode::Duplicate | FeedNoticeCode::OutOfOrder
        ));
    }

    #[test]
    fn a_book_delta_whose_price_is_not_exact_decimal_is_refused() {
        let mut adapter = subscribed_adapter();
        let error = adapter
            .ingest(&text(
                7,
                RecordKind::InboundText,
                &serde_json::json!({
                    "type": "orderbook_delta",
                    "sid": 11,
                    "seq": 3,
                    "msg": {
                        "market_ticker": TICKER,
                        "price_dollars": "4.5e-1",
                        "delta_fp": "-2.00",
                        "side": "yes",
                        "ts": "2026-08-14T17:00:03Z",
                        "ts_ms": 1_786_726_803_000_u64
                    }
                })
                .to_string(),
            ))
            .expect_err("a scientific-notation price is never silently reinterpreted");
        assert!(matches!(error, AdapterError::InvalidPayload(_)));
    }

    #[test]
    fn market_ticker_is_exact_and_bounded() {
        assert!(validate_market_ticker("KXBTC15M-26AUG141315-15").is_ok());
        assert!(validate_market_ticker("KXBTCD-26AUG14").is_err());
        assert!(validate_market_ticker("KXBTC15M-lower").is_err());
    }

    #[test]
    fn signed_delta_uses_plain_exact_decimal() {
        assert_eq!(canonical_signed("-54.00", "delta").unwrap().0, "-54");
        assert!(canonical_signed("1e2", "delta").is_err());
    }

    #[test]
    fn remembered_trade_ids_are_deduplicated() {
        let mut adapter = KalshiAdapter::new("test").expect("adapter constructs");
        adapter.remember_trade("trade-1".into(), "fingerprint".into());
        adapter.remember_trade("trade-1".into(), "fingerprint".into());
        assert_eq!(adapter.seen_trades.len(), 1);
        assert_eq!(adapter.trade_id_order.len(), 1);
    }
}
