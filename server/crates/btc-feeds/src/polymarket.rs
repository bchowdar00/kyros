//! Polymarket BTC Up/Down CLOB market adapter: Gamma market discovery, the
//! discovery journal that binds an adapter to one exact market, and the market
//! socket's book, quote and trade messages.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::str::FromStr;

use chrono::{SecondsFormat, TimeZone, Utc};
use reqwest::header::{ACCEPT, CACHE_CONTROL};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use serde_json::{Value, json};
use signals_core::SourceEvent;
use thiserror::Error;

use crate::adapter::{
    GuardAction, RecordGuard, bounded_token, parse_timestamp, validate_publisher_epoch,
    validate_timestamp,
};
use crate::decimal::{decimal_string, validate_decimal, value_is_exact};
use crate::secure_http::{self, SecureHttpError, sha256_hex};
use crate::{
    AdapterError, FeedAdapter, FeedNotice, FeedNoticeCode, IngestOutcome, Provider, RawRecord,
    RecoveryAction,
};

pub(crate) const GAMMA_MARKETS_URL: &str = "https://gamma-api.polymarket.com/markets";
const POLYMARKET_MARKET_WS_URL: &str = "wss://ws-subscriptions-clob.polymarket.com/ws/market";
const CHAINLINK_BTC_TWAP_URL: &str = "https://data.chain.link/streams/btc-usd-twap-60s-streams";
const CONFIGURATION_KIND: &str = "polymarket_gamma_market_v1";
pub(crate) const MAX_GAMMA_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const MAX_BOOK_ROWS: usize = 100_000;
const MAX_TRADE_FINGERPRINTS: usize = 100_000;
const MARKET_CONTRACT_SOURCE_ID: &str = "polymarket.btc.market-contracts";
const BOOK_SOURCE_ID: &str = "polymarket.btc.book-levels";
const QUOTE_SOURCE_ID: &str = "polymarket.btc.quotes";
const TRADE_SOURCE_ID: &str = "polymarket.btc.trades";
const SOURCE_VERSION: &str = "1.0.0";

pub(crate) const fn websocket_endpoint() -> &'static str {
    POLYMARKET_MARKET_WS_URL
}

/// One resolved Polymarket BTC Up/Down market, ready to bind an adapter to.
#[derive(Debug, Clone)]
pub struct PolymarketDiscovery {
    /// The Configuration [`RawRecord`] payload: a discovery journal that embeds
    /// the exact Gamma body, its request path, the retrieval time and the
    /// SHA-256 over the body. Feed it to a fresh [`PolymarketAdapter`] as the
    /// stream's first record; see
    /// [`PolymarketAdapter::configuration_payload`] for the format.
    pub journal_payload: String,
    /// The CLOB asset ids the journal resolved to, `[up, down]`.
    pub asset_ids: [String; 2],
}

/// Fixed-host, keyless Gamma discovery for one BTC Up/Down market slug.
pub(crate) async fn discover_market(
    slug: &str,
) -> Result<PolymarketDiscovery, PolymarketDiscoveryError> {
    validate_slug(slug)
        .map_err(|error| PolymarketDiscoveryError::InvalidMarket(error.to_string()))?;
    let client = secure_http::client()?;

    for closed in [false, true] {
        let response = client
            .get(GAMMA_MARKETS_URL)
            .header(ACCEPT, "application/json")
            .header(CACHE_CONTROL, "no-cache")
            .query(&[
                ("slug", slug),
                ("closed", if closed { "true" } else { "false" }),
            ])
            .send()
            .await
            .map_err(|_| PolymarketDiscoveryError::Http)?;
        let body = secure_http::bounded_body(response, MAX_GAMMA_RESPONSE_BYTES).await?;
        let retrieved_at = crate::raw::now_timestamp();
        let body_text = String::from_utf8(body).map_err(|_| {
            PolymarketDiscoveryError::InvalidResponse("Gamma body is not UTF-8".to_owned())
        })?;
        let markets: Vec<&RawValue> = serde_json::from_str(&body_text).map_err(|error| {
            PolymarketDiscoveryError::InvalidResponse(format!(
                "Gamma body is not a market array: {error}"
            ))
        })?;
        if markets.is_empty() {
            continue;
        }
        if markets.len() != 1 {
            return Err(PolymarketDiscoveryError::InvalidResponse(format!(
                "Gamma returned {} markets for exact slug {slug}",
                markets.len()
            )));
        }

        return discovery_from_response(slug, closed, retrieved_at, body_text);
    }

    Err(PolymarketDiscoveryError::NotFound(slug.to_owned()))
}

/// Build a discovery journal from a Gamma markets body that was fetched
/// elsewhere.
///
/// `slug` is the exact `btc-updown-{5m,15m}-<unix>` slug that was requested,
/// `closed` the value of the `closed` query parameter used, `retrieved_at` the
/// RFC 3339 instant the response was received, and `body_text` the response
/// body verbatim. The body must be a one-element market array and is validated
/// against the full market contract before a journal is produced.
pub fn discovery_from_response(
    slug: &str,
    closed: bool,
    retrieved_at: String,
    body_text: String,
) -> Result<PolymarketDiscovery, PolymarketDiscoveryError> {
    validate_slug(slug)
        .map_err(|error| PolymarketDiscoveryError::InvalidMarket(error.to_string()))?;
    let markets: Vec<&RawValue> = serde_json::from_str(&body_text).map_err(|error| {
        PolymarketDiscoveryError::InvalidResponse(format!(
            "Gamma body is not a market array: {error}"
        ))
    })?;
    if markets.len() != 1 {
        return Err(PolymarketDiscoveryError::InvalidResponse(format!(
            "Gamma returned {} markets for exact slug {slug}",
            markets.len()
        )));
    }
    let request_path = format!("/markets?slug={slug}&closed={closed}");
    let contract = MarketContract::from_gamma(&body_text, slug, &retrieved_at)
        .map_err(|error| PolymarketDiscoveryError::InvalidMarket(error.to_string()))?;
    let configuration = DiscoveryJournal {
        schema_version: 1,
        kind: CONFIGURATION_KIND.to_owned(),
        request_path,
        retrieved_at,
        gamma_body_sha256: sha256_hex(body_text.as_bytes()),
        gamma_body: body_text,
    };
    let journal_payload =
        serde_json::to_string(&configuration).map_err(PolymarketDiscoveryError::Json)?;
    Ok(PolymarketDiscovery {
        journal_payload,
        asset_ids: [contract.up_token_id, contract.down_token_id],
    })
}

/// Check that `slug` names one exact Polymarket BTC 5-minute or 15-minute
/// Up/Down market: the `btc-updown-5m-` or `btc-updown-15m-` prefix plus a Unix
/// timestamp, at most 128 bytes.
pub fn validate_slug(slug: &str) -> Result<(), AdapterError> {
    if slug.len() > 128 {
        return Err(AdapterError::InvalidPayload(
            "Polymarket slug exceeds 128 bytes".to_owned(),
        ));
    }
    let timestamp = slug
        .strip_prefix("btc-updown-5m-")
        .or_else(|| slug.strip_prefix("btc-updown-15m-"))
        .ok_or_else(|| {
            AdapterError::InvalidPayload(
                "Polymarket slug must select a BTC 5m or 15m Up/Down market".to_owned(),
            )
        })?;
    if timestamp.is_empty() || !timestamp.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(AdapterError::InvalidPayload(
            "Polymarket slug must end in a Unix timestamp".to_owned(),
        ));
    }
    timestamp.parse::<i64>().map_err(|_| {
        AdapterError::InvalidPayload("Polymarket slug timestamp is outside i64".to_owned())
    })?;
    Ok(())
}

/// Why Polymarket market discovery could not produce a usable journal.
#[derive(Debug, Error)]
pub enum PolymarketDiscoveryError {
    /// The bounded fixed-host Gamma request failed. Deliberately carries no
    /// transport detail.
    #[error("secure Polymarket HTTP request failed")]
    Http,
    /// The slug matched no market in either the open or the closed lookup.
    #[error("Polymarket market `{0}` was absent from both closed=false and closed=true lookups")]
    NotFound(String),
    /// The response was not a bounded UTF-8 one-element market array.
    #[error("Polymarket returned an invalid bounded response: {0}")]
    InvalidResponse(String),
    /// The response parsed but the market it describes fails a contract rule
    /// (slug, token pair, window, tick size, or resolution source).
    #[error("Polymarket market contract validation failed: {0}")]
    InvalidMarket(String),
    /// The validated journal could not be serialized.
    #[error("Polymarket configuration serialization failed: {0}")]
    Json(serde_json::Error),
}

impl From<SecureHttpError> for PolymarketDiscoveryError {
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
    gamma_body_sha256: String,
    gamma_body: String,
}

#[derive(Debug, Clone)]
struct MarketContract {
    market_id: String,
    condition_id: String,
    question_id: String,
    slug: String,
    question: String,
    up_token_id: String,
    down_token_id: String,
    window_open: String,
    window_close: String,
    resolution_source: String,
    minimum_tick_size: String,
    minimum_order_size: String,
    order_book_enabled: bool,
    active: bool,
    closed: bool,
    rules_sha256: String,
    event_time: String,
    received_at: String,
}

impl MarketContract {
    fn from_journal(payload: &str) -> Result<Self, AdapterError> {
        let journal: DiscoveryJournal = serde_json::from_str(payload).map_err(payload_error)?;
        if journal.schema_version != 1 || journal.kind != CONFIGURATION_KIND {
            return Err(AdapterError::InvalidPayload(
                "unsupported Polymarket discovery configuration".to_owned(),
            ));
        }
        validate_timestamp(&journal.retrieved_at, "retrieved_at")?;
        if sha256_hex(journal.gamma_body.as_bytes()) != journal.gamma_body_sha256 {
            return Err(AdapterError::InvalidPayload(
                "Polymarket Gamma body hash does not match configuration".to_owned(),
            ));
        }
        let (slug, closed) = parse_request_path(&journal.request_path)?;
        let contract = Self::from_gamma(&journal.gamma_body, slug, &journal.retrieved_at)?;
        if contract.closed != closed {
            return Err(AdapterError::InvalidPayload(
                "Polymarket Gamma closed filter disagrees with returned market".to_owned(),
            ));
        }
        Ok(contract)
    }

    fn from_gamma(
        body: &str,
        expected_slug: &str,
        retrieved_at: &str,
    ) -> Result<Self, AdapterError> {
        validate_timestamp(retrieved_at, "retrieved_at")?;
        let markets: Vec<GammaMarket> = serde_json::from_str(body).map_err(payload_error)?;
        let [market] = markets.as_slice() else {
            return Err(AdapterError::InvalidPayload(
                "Polymarket Gamma response must contain exactly one market".to_owned(),
            ));
        };
        validate_slug(&market.slug)?;
        if market.slug != expected_slug {
            return Err(AdapterError::InvalidPayload(
                "Polymarket Gamma market slug does not match the request".to_owned(),
            ));
        }
        validate_identifier(&market.id, "market id", false)?;
        validate_identifier(&market.condition_id, "condition id", true)?;
        validate_identifier(&market.question_id, "question id", true)?;
        validate_text(&market.question, "question", 1_024)?;
        validate_text(&market.description, "description", 16_384)?;
        if market.resolution_source != CHAINLINK_BTC_TWAP_URL {
            return Err(AdapterError::InvalidPayload(
                "Polymarket BTC market does not use the verified Chainlink TWAP source".to_owned(),
            ));
        }
        let description = market.description.to_ascii_lowercase();
        if !description.contains("greater than or equal to")
            || !description.contains("otherwise, it will resolve to \"down\"")
            || !description.contains("time-weighted average price")
        {
            return Err(AdapterError::InvalidPayload(
                "Polymarket BTC market rules do not prove >= Up / otherwise Down TWAP semantics"
                    .to_owned(),
            ));
        }

        let outcomes = market.outcomes.values("outcomes")?;
        let token_ids = market.clob_token_ids.values("clobTokenIds")?;
        if outcomes != ["Up", "Down"] || token_ids.len() != 2 {
            return Err(AdapterError::InvalidPayload(
                "Polymarket market must map exactly Up then Down to two CLOB token ids".to_owned(),
            ));
        }
        validate_identifier(&token_ids[0], "Up token id", false)?;
        validate_identifier(&token_ids[1], "Down token id", false)?;
        if token_ids[0] == token_ids[1] {
            return Err(AdapterError::InvalidPayload(
                "Polymarket outcome token ids must differ".to_owned(),
            ));
        }

        let matching_events = market
            .events
            .iter()
            .filter(|event| event.slug == market.slug || event.ticker == market.slug)
            .collect::<Vec<_>>();
        let [event] = matching_events.as_slice() else {
            return Err(AdapterError::InvalidPayload(
                "Polymarket Gamma market must contain one matching eventStartTime".to_owned(),
            ));
        };
        let window_open = validate_window(&market.slug, &event.start_time, &market.end_date)?;
        validate_timestamp(&market.updated_at, "updatedAt")?;
        let minimum_tick_size =
            canonical_raw_decimal(&market.minimum_tick_size, "orderPriceMinTickSize")?;
        let minimum_order_size = canonical_raw_decimal(&market.minimum_order_size, "orderMinSize")?;

        Ok(Self {
            market_id: market.id.clone(),
            condition_id: market.condition_id.clone(),
            question_id: market.question_id.clone(),
            slug: market.slug.clone(),
            question: market.question.clone(),
            up_token_id: token_ids[0].clone(),
            down_token_id: token_ids[1].clone(),
            window_open,
            window_close: canonical_timestamp(&market.end_date, "endDate")?,
            resolution_source: market.resolution_source.clone(),
            minimum_tick_size,
            minimum_order_size,
            order_book_enabled: market.enable_order_book,
            active: market.active,
            closed: market.closed,
            rules_sha256: sha256_hex(market.description.as_bytes()),
            event_time: canonical_timestamp(&market.updated_at, "updatedAt")?,
            received_at: canonical_timestamp(retrieved_at, "retrieved_at")?,
        })
    }

    fn outcome(&self, asset_id: &str) -> Result<&'static str, AdapterError> {
        if asset_id == self.up_token_id {
            Ok("up")
        } else if asset_id == self.down_token_id {
            Ok("down")
        } else {
            Err(AdapterError::InvalidPayload(
                "Polymarket message references an undiscovered token id".to_owned(),
            ))
        }
    }

    fn contract_values(&self) -> BTreeMap<String, Value> {
        BTreeMap::from([
            ("market_id".to_owned(), json!(self.market_id)),
            ("condition_id".to_owned(), json!(self.condition_id)),
            ("question_id".to_owned(), json!(self.question_id)),
            ("slug".to_owned(), json!(self.slug)),
            ("question".to_owned(), json!(self.question)),
            ("up_token_id".to_owned(), json!(self.up_token_id)),
            ("down_token_id".to_owned(), json!(self.down_token_id)),
            ("window_open".to_owned(), json!(self.window_open)),
            ("window_close".to_owned(), json!(self.window_close)),
            (
                "resolution_source".to_owned(),
                json!(self.resolution_source),
            ),
            ("comparator".to_owned(), json!("greater_than_or_equal")),
            ("tie_outcome".to_owned(), json!("up")),
            (
                "minimum_tick_size".to_owned(),
                json!(self.minimum_tick_size),
            ),
            (
                "minimum_order_size".to_owned(),
                json!(self.minimum_order_size),
            ),
            (
                "order_book_enabled".to_owned(),
                json!(self.order_book_enabled),
            ),
            ("active".to_owned(), json!(self.active)),
            ("closed".to_owned(), json!(self.closed)),
            ("rules_sha256".to_owned(), json!(self.rules_sha256)),
            ("event_time".to_owned(), json!(self.event_time)),
        ])
    }
}

/// Deterministic Polymarket Gamma + market-channel adapter.
pub struct PolymarketAdapter {
    publisher_epoch: String,
    guard: RecordGuard,
    contract: Option<MarketContract>,
    next_contract_sequence: u64,
    next_book_sequence: u64,
    next_quote_sequence: u64,
    next_trade_sequence: u64,
    seen_trade_fingerprints: BTreeSet<String>,
    trade_fingerprint_order: VecDeque<String>,
    rediscovery_required: bool,
}

impl PolymarketAdapter {
    /// Construct a Polymarket market adapter.
    ///
    /// `publisher_epoch` is copied verbatim into every emitted event; it must be
    /// non-blank and at most 128 bytes, or the call returns
    /// [`AdapterError::InvalidRecord`].
    ///
    /// The adapter is not usable until it has ingested a Configuration record
    /// binding it to one market — see
    /// [`PolymarketAdapter::configuration_payload`].
    pub fn new(publisher_epoch: impl Into<String>) -> Result<Self, AdapterError> {
        let publisher_epoch = publisher_epoch.into();
        validate_publisher_epoch(&publisher_epoch)?;
        Ok(Self {
            publisher_epoch,
            guard: RecordGuard::new(Provider::Polymarket),
            contract: None,
            next_contract_sequence: 0,
            next_book_sequence: 0,
            next_quote_sequence: 0,
            next_trade_sequence: 0,
            seen_trade_fingerprints: BTreeSet::new(),
            trade_fingerprint_order: VecDeque::new(),
            rediscovery_required: false,
        })
    }

    /// Build the Configuration record payload this adapter's `configure()` step
    /// accepts, from a Gamma markets body fetched by the caller.
    ///
    /// `gamma_body` is the response of
    /// `GET https://gamma-api.polymarket.com/markets?slug=<slug>&closed=<bool>`
    /// verbatim, `request_path` is that request's path and query
    /// (`/markets?slug=<slug>&closed=<true|false>` — the slug is read back out of
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
    ///   "kind": "polymarket_gamma_market_v1",
    ///   "request_path": "/markets?slug=btc-updown-15m-...&closed=false",
    ///   "retrieved_at": "<RFC 3339>",
    ///   "gamma_body_sha256": "<lower-case hex SHA-256 of gamma_body>",
    ///   "gamma_body": "<the Gamma body, verbatim>"
    /// }
    /// ```
    ///
    /// The self-describing hash is what makes a capture replayable without the
    /// network: `configure()` recomputes it and refuses a journal whose body was
    /// edited. Ingest it as the stream's first record, with connection epoch 1
    /// and schema version 3:
    ///
    /// ```text
    /// RawRecord::configuration_at(CaptureStream::PolymarketMarket, 1, 1, payload, received_at, 0)
    /// ```
    ///
    /// Returns [`AdapterError::InvalidPayload`] if the path is not an exact
    /// single-slug Gamma query, or the body does not describe exactly one market
    /// meeting the BTC Up/Down contract rules.
    pub fn configuration_payload(
        gamma_body: &str,
        request_path: &str,
        retrieved_at: &str,
    ) -> Result<String, AdapterError> {
        let query = request_path.strip_prefix("/markets?slug=").ok_or_else(|| {
            AdapterError::InvalidPayload("invalid Polymarket discovery request path".to_owned())
        })?;
        let (slug, closed) = match query.split_once("&closed=") {
            Some((slug, "false")) => (slug, false),
            Some((slug, "true")) => (slug, true),
            _ => {
                return Err(AdapterError::InvalidPayload(
                    "invalid Polymarket discovery request path".to_owned(),
                ));
            }
        };
        validate_slug(slug)?;
        validate_timestamp(retrieved_at, "retrieved_at")?;
        discovery_from_response(slug, closed, retrieved_at.to_owned(), gamma_body.to_owned())
            .map(|discovery| discovery.journal_payload)
            .map_err(|error| AdapterError::InvalidPayload(error.to_string()))
    }

    /// Assert the capture was complete. Call once, after the last record.
    ///
    /// Returns [`AdapterError::InvalidRecord`] if the stream carried no Gamma
    /// Configuration record, without which no market binding — and so no
    /// contract event — exists.
    pub fn finish(&self) -> Result<(), AdapterError> {
        if self.contract.is_some() {
            Ok(())
        } else {
            Err(AdapterError::InvalidRecord(
                "Polymarket capture is missing its Gamma configuration record".to_owned(),
            ))
        }
    }

    fn configure(&mut self, payload: &str) -> Result<IngestOutcome, AdapterError> {
        if self.contract.is_some() {
            return Err(AdapterError::InvalidRecord(
                "Polymarket capture contains multiple configuration records".to_owned(),
            ));
        }
        let contract = MarketContract::from_journal(payload)?;
        let event = self.source_event(
            MARKET_CONTRACT_SOURCE_ID,
            contract.event_time.clone(),
            contract.received_at.clone(),
            contract.contract_values(),
        )?;
        self.contract = Some(contract);
        Ok(IngestOutcome {
            events: vec![event],
            notices: Vec::new(),
        })
    }

    fn ingest_payload(
        &mut self,
        record: &RawRecord,
        payload: &str,
    ) -> Result<IngestOutcome, AdapterError> {
        if payload == "PONG" {
            return Ok(IngestOutcome::notice(FeedNotice::info(
                FeedNoticeCode::Heartbeat,
                "Polymarket literal-text PONG observed",
            )));
        }
        if self.rediscovery_required {
            return Err(AdapterError::InvalidPayload(
                "Polymarket market input followed a lifecycle change before rediscovery".to_owned(),
            ));
        }
        let value: Value = serde_json::from_str(payload).map_err(payload_error)?;
        let values = match value {
            Value::Array(values) => values,
            Value::Object(_) => vec![value],
            _ => {
                return Err(AdapterError::InvalidPayload(
                    "Polymarket market message must be an object, object array, or literal PONG"
                        .to_owned(),
                ));
            }
        };
        if values.len() > MAX_BOOK_ROWS {
            return Err(AdapterError::InvalidPayload(format!(
                "Polymarket message array exceeds {MAX_BOOK_ROWS} entries"
            )));
        }
        let mut combined = IngestOutcome::default();
        for value in values {
            // Ruling E61: a frame carrying no string `event_type` discriminator is
            // an unsupported message shape — an auxiliary or unmodelled frame the
            // adapter does not model — retained as a counted notice so the stream
            // continues, rather than failing closed. A frame that names a
            // discriminator still routes strictly, and a modelled message with an
            // invalid value still refuses.
            let Some(event_type) = value.get("event_type").and_then(Value::as_str) else {
                combined.notices.push(FeedNotice::info(
                    FeedNoticeCode::UnsupportedMessage,
                    "retained Polymarket message without an event_type discriminator".to_owned(),
                ));
                continue;
            };
            let event_type = event_type.to_owned();
            let outcome = match event_type.as_str() {
                "book" => self.book(record, value)?,
                "price_change" => self.price_change(record, value, payload)?,
                "last_trade_price" => self.last_trade(record, value)?,
                "best_bid_ask" => self.best_bid_ask(record, value)?,
                "tick_size_change" | "new_market" | "market_resolved" => {
                    self.lifecycle_change(&event_type, value)?
                }
                other => IngestOutcome::notice(FeedNotice::info(
                    FeedNoticeCode::UnsupportedMessage,
                    format!(
                        "retained unsupported Polymarket message type `{}`",
                        bounded_token(other)
                    ),
                )),
            };
            combined.events.extend(outcome.events);
            combined.notices.extend(outcome.notices);
            if self.rediscovery_required {
                break;
            }
        }
        Ok(combined)
    }

    fn lifecycle_change(
        &mut self,
        event_type: &str,
        value: Value,
    ) -> Result<IngestOutcome, AdapterError> {
        // `new_market` is a global custom-channel announcement, not a mutation of
        // the subscribed contract. Retain it as provider evidence while leaving
        // the current binding intact. The two mutation events below are checked
        // against the configured condition and tokens before they can invalidate.
        if event_type == "new_market" {
            return Ok(IngestOutcome::notice(FeedNotice::info(
                FeedNoticeCode::UnsupportedMessage,
                "retained Polymarket `new_market` announcement; current binding unchanged",
            )));
        }

        let contract = self.contract.as_ref().ok_or_else(|| {
            AdapterError::InvalidRecord(
                "Polymarket lifecycle message arrived before Gamma configuration".to_owned(),
            )
        })?;
        let affects_current_market = match event_type {
            "tick_size_change" => {
                let message: TickSizeChangeMessage =
                    serde_json::from_value(value).map_err(payload_error)?;
                validate_identifier(&message.market, "condition id", true)?;
                validate_identifier(&message.asset_id, "asset id", false)?;
                canonical_decimal(&message.old_tick_size, "old_tick_size")?;
                canonical_decimal(&message.new_tick_size, "new_tick_size")?;
                provider_timestamp(&message.timestamp, "timestamp")?;
                if message.market == contract.condition_id {
                    contract.outcome(&message.asset_id)?;
                    true
                } else {
                    false
                }
            }
            "market_resolved" => {
                let message: MarketResolvedMessage =
                    serde_json::from_value(value).map_err(payload_error)?;
                validate_identifier(&message.market, "condition id", true)?;
                validate_identifier(&message.winning_asset_id, "winning asset id", false)?;
                provider_timestamp(&message.timestamp, "timestamp")?;
                if message.market == contract.condition_id {
                    let expected = BTreeSet::from([
                        contract.up_token_id.as_str(),
                        contract.down_token_id.as_str(),
                    ]);
                    let actual = message
                        .assets_ids
                        .iter()
                        .map(String::as_str)
                        .collect::<BTreeSet<_>>();
                    if message.assets_ids.len() != 2 || actual != expected {
                        return Err(AdapterError::InvalidPayload(
                            "Polymarket resolution assets do not match the configured market"
                                .to_owned(),
                        ));
                    }
                    contract.outcome(&message.winning_asset_id)?;
                    true
                } else {
                    false
                }
            }
            _ => {
                return Err(AdapterError::InvalidRecord(
                    "unknown Polymarket lifecycle event".to_owned(),
                ));
            }
        };

        if !affects_current_market {
            return Ok(IngestOutcome::notice(FeedNotice::info(
                FeedNoticeCode::UnsupportedMessage,
                format!(
                    "retained unrelated Polymarket `{}` event; current binding unchanged",
                    bounded_token(event_type)
                ),
            )));
        }

        self.rediscovery_required = true;
        Ok(IngestOutcome::notice(FeedNotice::recovery(
            FeedNoticeCode::StateInconsistent,
            format!("Polymarket `{event_type}` invalidated the current market binding"),
            RecoveryAction::RediscoverPredictionMarket,
        )))
    }

    fn book(&mut self, record: &RawRecord, value: Value) -> Result<IngestOutcome, AdapterError> {
        let message: BookMessage = serde_json::from_value(value).map_err(payload_error)?;
        let contract = self
            .checked_contract(&message.market, &message.asset_id)?
            .clone();
        let event_time = provider_timestamp(&message.timestamp, "timestamp")?;
        validate_text(&message.hash, "book hash", 256)?;
        let update_group = format!("{}:{}", message.asset_id, message.hash);
        let total = message
            .bids
            .len()
            .checked_add(message.asks.len())
            .ok_or_else(|| AdapterError::InvalidPayload("book row count overflowed".to_owned()))?;
        if total == 0 || total > MAX_BOOK_ROWS {
            return Err(AdapterError::InvalidPayload(format!(
                "Polymarket book snapshot must contain 1 to {MAX_BOOK_ROWS} levels"
            )));
        }
        let update_count = u32::try_from(total).map_err(|_| {
            AdapterError::InvalidPayload("Polymarket book row count exceeds u32".to_owned())
        })?;
        let mut events = Vec::with_capacity(total.saturating_add(1));
        let mut bids = Vec::with_capacity(message.bids.len());
        let mut asks = Vec::with_capacity(message.asks.len());
        for (side, levels) in [("bid", message.bids), ("ask", message.asks)] {
            for level in levels {
                let (price, price_decimal) = canonical_decimal(&level.price, "price")?;
                let (size, _) = canonical_decimal(&level.size, "size")?;
                if side == "bid" {
                    bids.push((price.clone(), price_decimal));
                } else {
                    asks.push((price.clone(), price_decimal));
                }
                let update_index = u32::try_from(events.len()).map_err(|_| {
                    AdapterError::InvalidPayload("Polymarket book index exceeds u32".to_owned())
                })?;
                let values = book_values(
                    &contract,
                    BookValueInput {
                        asset_id: &message.asset_id,
                        update_group: &update_group,
                        update_index,
                        update_count,
                        update_kind: "snapshot",
                        side,
                        price,
                        size,
                        event_time: &event_time,
                    },
                )?;
                events.push(self.source_event(
                    BOOK_SOURCE_ID,
                    event_time.clone(),
                    record.received_at.clone(),
                    values,
                )?);
            }
        }
        if let (Some(best_bid), Some(best_ask)) = (
            bids.into_iter().max_by_key(|(_, value)| *value),
            asks.into_iter().min_by_key(|(_, value)| *value),
        ) {
            events.push(self.quote_event(
                &contract,
                &message.asset_id,
                best_bid.0,
                best_ask.0,
                event_time,
                record.received_at.clone(),
            )?);
        }
        Ok(IngestOutcome {
            events,
            notices: Vec::new(),
        })
    }

    fn price_change(
        &mut self,
        record: &RawRecord,
        value: Value,
        raw_payload: &str,
    ) -> Result<IngestOutcome, AdapterError> {
        let message: PriceChangeMessage = serde_json::from_value(value).map_err(payload_error)?;
        let event_time = provider_timestamp(&message.timestamp, "timestamp")?;
        if message.price_changes.is_empty() || message.price_changes.len() > MAX_BOOK_ROWS {
            return Err(AdapterError::InvalidPayload(format!(
                "Polymarket price_change must contain 1 to {MAX_BOOK_ROWS} rows"
            )));
        }
        let update_count = u32::try_from(message.price_changes.len()).map_err(|_| {
            AdapterError::InvalidPayload("Polymarket price_change count exceeds u32".to_owned())
        })?;
        let update_group = format!(
            "{}:{}",
            message.timestamp.text(),
            sha256_hex(raw_payload.as_bytes())
        );
        let mut events = Vec::with_capacity(message.price_changes.len().saturating_mul(2));
        for (index, change) in message.price_changes.into_iter().enumerate() {
            let contract = self
                .checked_contract(&message.market, &change.asset_id)?
                .clone();
            let (price, _) = canonical_decimal(&change.price, "price")?;
            let (size, _) = canonical_decimal(&change.size, "size")?;
            let side = match change.side.as_str() {
                "BUY" => "bid",
                "SELL" => "ask",
                _ => {
                    return Err(AdapterError::InvalidPayload(
                        "Polymarket price_change side must be BUY or SELL".to_owned(),
                    ));
                }
            };
            let update_index = u32::try_from(index).map_err(|_| {
                AdapterError::InvalidPayload("Polymarket price_change index exceeds u32".to_owned())
            })?;
            let values = book_values(
                &contract,
                BookValueInput {
                    asset_id: &change.asset_id,
                    update_group: &update_group,
                    update_index,
                    update_count,
                    update_kind: "delta",
                    side,
                    price,
                    size,
                    event_time: &event_time,
                },
            )?;
            events.push(self.source_event(
                BOOK_SOURCE_ID,
                event_time.clone(),
                record.received_at.clone(),
                values,
            )?);
            if let (Some(best_bid), Some(best_ask)) = (change.best_bid, change.best_ask) {
                events.push(self.quote_event(
                    &contract,
                    &change.asset_id,
                    canonical_decimal(&best_bid, "best_bid")?.0,
                    canonical_decimal(&best_ask, "best_ask")?.0,
                    event_time.clone(),
                    record.received_at.clone(),
                )?);
            }
        }
        Ok(IngestOutcome {
            events,
            notices: Vec::new(),
        })
    }

    fn last_trade(
        &mut self,
        record: &RawRecord,
        value: Value,
    ) -> Result<IngestOutcome, AdapterError> {
        let message: LastTradeMessage = serde_json::from_value(value).map_err(payload_error)?;
        let contract = self
            .checked_contract(&message.market, &message.asset_id)?
            .clone();
        let event_time = provider_timestamp(&message.timestamp, "timestamp")?;
        let reported_side = match message.side.as_str() {
            "BUY" => "buy",
            "SELL" => "sell",
            _ => {
                return Err(AdapterError::InvalidPayload(
                    "Polymarket trade side must be BUY or SELL".to_owned(),
                ));
            }
        };
        let price = canonical_decimal(&message.price, "price")?.0;
        let size = canonical_decimal(&message.size, "size")?.0;
        let fee_rate_bps = canonical_decimal(&message.fee_rate_bps, "fee_rate_bps")?.0;
        validate_identifier(&message.transaction_hash, "transaction_hash", true)?;
        let transaction_hash = message.transaction_hash.to_ascii_lowercase();
        let fingerprint = sha256_hex(
            [
                transaction_hash.as_str(),
                message.market.as_str(),
                message.asset_id.as_str(),
                event_time.as_str(),
                reported_side,
                price.as_str(),
                size.as_str(),
                fee_rate_bps.as_str(),
            ]
            .join("\0")
            .as_bytes(),
        );
        if self.seen_trade_fingerprints.contains(&fingerprint) {
            return Ok(IngestOutcome::notice(FeedNotice::info(
                FeedNoticeCode::Duplicate,
                "ignored an exact Polymarket trade redelivery",
            )));
        }
        let values = BTreeMap::from([
            ("condition_id".to_owned(), json!(contract.condition_id)),
            ("asset_id".to_owned(), json!(message.asset_id)),
            (
                "outcome".to_owned(),
                json!(contract.outcome(&message.asset_id)?),
            ),
            ("price".to_owned(), json!(price)),
            ("size".to_owned(), json!(size)),
            ("fee_rate_bps".to_owned(), json!(fee_rate_bps)),
            ("reported_side".to_owned(), json!(reported_side)),
            ("event_time".to_owned(), json!(event_time)),
        ]);
        let event = self.source_event(
            TRADE_SOURCE_ID,
            event_time,
            record.received_at.clone(),
            values,
        )?;
        self.remember_trade(fingerprint);
        Ok(IngestOutcome {
            events: vec![event],
            notices: Vec::new(),
        })
    }

    fn remember_trade(&mut self, fingerprint: String) {
        if self.seen_trade_fingerprints.insert(fingerprint.clone()) {
            self.trade_fingerprint_order.push_back(fingerprint);
        }
        while self.trade_fingerprint_order.len() > MAX_TRADE_FINGERPRINTS {
            if let Some(oldest) = self.trade_fingerprint_order.pop_front() {
                self.seen_trade_fingerprints.remove(&oldest);
            }
        }
    }

    fn best_bid_ask(
        &mut self,
        record: &RawRecord,
        value: Value,
    ) -> Result<IngestOutcome, AdapterError> {
        let message: BestBidAskMessage = serde_json::from_value(value).map_err(payload_error)?;
        let contract = self
            .checked_contract(&message.market, &message.asset_id)?
            .clone();
        let event_time = provider_timestamp(&message.timestamp, "timestamp")?;
        let event = self.quote_event(
            &contract,
            &message.asset_id,
            canonical_decimal(&message.best_bid, "best_bid")?.0,
            canonical_decimal(&message.best_ask, "best_ask")?.0,
            event_time,
            record.received_at.clone(),
        )?;
        Ok(IngestOutcome {
            events: vec![event],
            notices: Vec::new(),
        })
    }

    fn quote_event(
        &mut self,
        contract: &MarketContract,
        asset_id: &str,
        best_bid: String,
        best_ask: String,
        event_time: String,
        received_at: String,
    ) -> Result<SourceEvent, AdapterError> {
        let bid = validate_decimal(&best_bid, "best_bid")?;
        let ask = validate_decimal(&best_ask, "best_ask")?;
        if ask < bid {
            return Err(AdapterError::InvalidPayload(
                "Polymarket best ask is below best bid".to_owned(),
            ));
        }
        let spread = (ask - bid).normalize().to_string();
        let values = BTreeMap::from([
            ("condition_id".to_owned(), json!(contract.condition_id)),
            ("asset_id".to_owned(), json!(asset_id)),
            ("outcome".to_owned(), json!(contract.outcome(asset_id)?)),
            ("best_bid".to_owned(), json!(best_bid)),
            ("best_ask".to_owned(), json!(best_ask)),
            ("spread".to_owned(), json!(spread)),
            ("event_time".to_owned(), json!(event_time)),
        ]);
        self.source_event(QUOTE_SOURCE_ID, event_time, received_at, values)
    }

    fn checked_contract(
        &self,
        condition_id: &str,
        asset_id: &str,
    ) -> Result<&MarketContract, AdapterError> {
        let contract = self.contract.as_ref().ok_or_else(|| {
            AdapterError::InvalidRecord(
                "Polymarket message arrived before Gamma configuration".to_owned(),
            )
        })?;
        if condition_id != contract.condition_id {
            return Err(AdapterError::InvalidPayload(
                "Polymarket message condition id does not match configured market".to_owned(),
            ));
        }
        contract.outcome(asset_id)?;
        Ok(contract)
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
                    "unknown Polymarket source identity".to_owned(),
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

impl FeedAdapter for PolymarketAdapter {
    fn ingest(&mut self, record: &RawRecord) -> Result<IngestOutcome, AdapterError> {
        match self.guard.observe(record)? {
            GuardAction::Configuration(payload) => self.configure(payload),
            GuardAction::Connected => {
                if self.contract.is_none() {
                    return Err(AdapterError::InvalidRecord(
                        "Polymarket connection began before Gamma configuration".to_owned(),
                    ));
                }
                Ok(IngestOutcome::notice(FeedNotice::info(
                    FeedNoticeCode::Connected,
                    "Polymarket market WebSocket connected; each token awaits a fresh book snapshot",
                )))
            }
            GuardAction::Disconnected => Ok(IngestOutcome::notice(FeedNotice::recovery(
                FeedNoticeCode::Disconnected,
                "Polymarket market WebSocket disconnected",
                RecoveryAction::Reconnect,
            ))),
            GuardAction::Outbound => Ok(IngestOutcome::default()),
            GuardAction::Failed(_) => Err(AdapterError::InvalidRecord(
                "Polymarket market captures do not accept failure records".to_owned(),
            )),
            GuardAction::Inbound(payload) => self.ingest_payload(record, payload),
        }
    }
}

struct BookValueInput<'a> {
    asset_id: &'a str,
    update_group: &'a str,
    update_index: u32,
    update_count: u32,
    update_kind: &'static str,
    side: &'a str,
    price: String,
    size: String,
    event_time: &'a str,
}

fn book_values(
    contract: &MarketContract,
    input: BookValueInput<'_>,
) -> Result<BTreeMap<String, Value>, AdapterError> {
    Ok(BTreeMap::from([
        ("condition_id".to_owned(), json!(contract.condition_id)),
        ("asset_id".to_owned(), json!(input.asset_id)),
        (
            "outcome".to_owned(),
            json!(contract.outcome(input.asset_id)?),
        ),
        ("update_group".to_owned(), json!(input.update_group)),
        ("update_index".to_owned(), json!(input.update_index)),
        ("update_count".to_owned(), json!(input.update_count)),
        ("update_kind".to_owned(), json!(input.update_kind)),
        ("side".to_owned(), json!(input.side)),
        ("price".to_owned(), json!(input.price)),
        ("size".to_owned(), json!(input.size)),
        ("event_time".to_owned(), json!(input.event_time)),
    ]))
}

#[derive(Deserialize)]
struct GammaMarket {
    id: String,
    question: String,
    #[serde(rename = "conditionId")]
    condition_id: String,
    slug: String,
    #[serde(rename = "resolutionSource")]
    resolution_source: String,
    #[serde(rename = "endDate")]
    end_date: String,
    description: String,
    outcomes: StringArray,
    active: bool,
    closed: bool,
    #[serde(rename = "questionID")]
    question_id: String,
    #[serde(rename = "enableOrderBook")]
    enable_order_book: bool,
    #[serde(rename = "orderPriceMinTickSize")]
    minimum_tick_size: Box<RawValue>,
    #[serde(rename = "orderMinSize")]
    minimum_order_size: Box<RawValue>,
    #[serde(rename = "clobTokenIds")]
    clob_token_ids: StringArray,
    #[serde(rename = "updatedAt")]
    updated_at: String,
    events: Vec<GammaEvent>,
}

#[derive(Deserialize)]
struct GammaEvent {
    ticker: String,
    slug: String,
    #[serde(rename = "startTime")]
    start_time: String,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum StringArray {
    Encoded(String),
    Direct(Vec<String>),
}

impl StringArray {
    fn values(&self, field: &str) -> Result<Vec<String>, AdapterError> {
        match self {
            Self::Encoded(value) => serde_json::from_str(value).map_err(|error| {
                AdapterError::InvalidPayload(format!(
                    "Polymarket {field} is not an encoded string array: {error}"
                ))
            }),
            Self::Direct(values) => Ok(values.clone()),
        }
    }
}

#[derive(Deserialize)]
struct BookMessage {
    asset_id: String,
    market: String,
    bids: Vec<BookLevel>,
    asks: Vec<BookLevel>,
    timestamp: StringOrNumber,
    hash: String,
}

#[derive(Deserialize)]
struct BookLevel {
    price: String,
    size: String,
}

#[derive(Deserialize)]
struct PriceChangeMessage {
    market: String,
    price_changes: Vec<PriceChange>,
    timestamp: StringOrNumber,
}

#[derive(Deserialize)]
struct PriceChange {
    asset_id: String,
    price: String,
    size: String,
    side: String,
    #[serde(default)]
    best_bid: Option<String>,
    #[serde(default)]
    best_ask: Option<String>,
}

#[derive(Deserialize)]
struct LastTradeMessage {
    asset_id: String,
    market: String,
    transaction_hash: String,
    price: String,
    size: String,
    side: String,
    fee_rate_bps: String,
    timestamp: StringOrNumber,
}

#[derive(Deserialize)]
struct BestBidAskMessage {
    asset_id: String,
    market: String,
    best_bid: String,
    best_ask: String,
    timestamp: StringOrNumber,
}

#[derive(Deserialize)]
struct TickSizeChangeMessage {
    asset_id: String,
    market: String,
    old_tick_size: String,
    new_tick_size: String,
    timestamp: StringOrNumber,
}

#[derive(Deserialize)]
struct MarketResolvedMessage {
    market: String,
    assets_ids: Vec<String>,
    winning_asset_id: String,
    timestamp: StringOrNumber,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum StringOrNumber {
    String(String),
    Number(serde_json::Number),
}

impl StringOrNumber {
    fn text(&self) -> Cow<'_, str> {
        match self {
            Self::String(value) => Cow::Borrowed(value),
            Self::Number(value) => Cow::Owned(value.to_string()),
        }
    }
}

fn parse_request_path(path: &str) -> Result<(&str, bool), AdapterError> {
    let prefix = "/markets?slug=";
    let rest = path.strip_prefix(prefix).ok_or_else(|| {
        AdapterError::InvalidPayload("invalid Polymarket Gamma request path".to_owned())
    })?;
    let (slug, closed) = rest.split_once("&closed=").ok_or_else(|| {
        AdapterError::InvalidPayload("invalid Polymarket Gamma request query".to_owned())
    })?;
    validate_slug(slug)?;
    let closed = match closed {
        "true" => true,
        "false" => false,
        _ => {
            return Err(AdapterError::InvalidPayload(
                "Polymarket Gamma closed query must be true or false".to_owned(),
            ));
        }
    };
    Ok((slug, closed))
}

fn validate_window(slug: &str, start: &str, end: &str) -> Result<String, AdapterError> {
    let (duration_minutes, timestamp) = if let Some(timestamp) = slug.strip_prefix("btc-updown-5m-")
    {
        (5_i64, timestamp)
    } else if let Some(timestamp) = slug.strip_prefix("btc-updown-15m-") {
        (15_i64, timestamp)
    } else {
        return Err(AdapterError::InvalidPayload(
            "unsupported Polymarket BTC slug".to_owned(),
        ));
    };
    let unix = timestamp.parse::<i64>().map_err(|_| {
        AdapterError::InvalidPayload("Polymarket slug timestamp is invalid".to_owned())
    })?;
    let expected_start = Utc.timestamp_opt(unix, 0).single().ok_or_else(|| {
        AdapterError::InvalidPayload("Polymarket slug timestamp is outside UTC range".to_owned())
    })?;
    let observed_start = parse_timestamp(start, "eventStartTime")?;
    let observed_end = parse_timestamp(end, "endDate")?;
    if observed_start != expected_start
        || observed_end != expected_start + chrono::Duration::minutes(duration_minutes)
    {
        return Err(AdapterError::InvalidPayload(
            "Polymarket slug, eventStartTime, and endDate window disagree".to_owned(),
        ));
    }
    Ok(expected_start.to_rfc3339_opts(SecondsFormat::Secs, true))
}

fn provider_timestamp(value: &StringOrNumber, field: &str) -> Result<String, AdapterError> {
    let text = value.text();
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(AdapterError::InvalidPayload(format!(
            "Polymarket {field} must be Unix milliseconds"
        )));
    }
    let millis = text
        .parse::<i64>()
        .map_err(|_| AdapterError::InvalidPayload(format!("Polymarket {field} exceeds i64")))?;
    Utc.timestamp_millis_opt(millis)
        .single()
        .ok_or_else(|| {
            AdapterError::InvalidPayload(format!("Polymarket {field} is outside UTC range"))
        })
        .map(|timestamp| timestamp.to_rfc3339_opts(SecondsFormat::Millis, true))
}

fn canonical_raw_decimal(raw: &RawValue, field: &str) -> Result<String, AdapterError> {
    let value = decimal_string(raw, field)?;
    canonical_decimal(&value, field).map(|(value, _)| value)
}

fn canonical_decimal(value: &str, field: &str) -> Result<(String, Decimal), AdapterError> {
    if value.is_empty() || value.len() > signals_core::MAX_DECIMAL_TEXT_BYTES {
        return Err(AdapterError::InvalidPayload(format!(
            "Polymarket {field} has invalid decimal length"
        )));
    }
    if value.starts_with('+') || value.contains(['e', 'E']) {
        return Err(AdapterError::InvalidPayload(format!(
            "Polymarket {field} must be plain decimal text"
        )));
    }
    let expanded;
    let value = if value.starts_with('.') {
        expanded = format!("0{value}");
        expanded.as_str()
    } else {
        value
    };
    let decimal = Decimal::from_str(value).map_err(|error| {
        AdapterError::InvalidPayload(format!(
            "Polymarket {field} is not an exact decimal: {error}"
        ))
    })?;
    if !value_is_exact(value, &decimal) {
        return Err(AdapterError::InvalidPayload(format!(
            "Polymarket {field} is not an exact decimal"
        )));
    }
    if decimal.is_sign_negative() {
        return Err(AdapterError::InvalidPayload(format!(
            "Polymarket {field} must be non-negative"
        )));
    }
    let canonical = decimal.normalize().to_string();
    validate_decimal(&canonical, field)?;
    Ok((canonical, decimal))
}

fn canonical_timestamp(value: &str, field: &str) -> Result<String, AdapterError> {
    Ok(parse_timestamp(value, field)?.to_rfc3339_opts(SecondsFormat::AutoSi, true))
}

fn validate_identifier(value: &str, field: &str, hex: bool) -> Result<(), AdapterError> {
    if value.is_empty() || value.len() > 256 {
        return Err(AdapterError::InvalidPayload(format!(
            "Polymarket {field} must contain 1 to 256 bytes"
        )));
    }
    if hex
        && (value.len() != 66
            || !value.starts_with("0x")
            || !value[2..].bytes().all(|byte| byte.is_ascii_hexdigit()))
    {
        return Err(AdapterError::InvalidPayload(format!(
            "Polymarket {field} must be a 32-byte 0x-prefixed hex value"
        )));
    }
    if !hex && !value.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        return Err(AdapterError::InvalidPayload(format!(
            "Polymarket {field} contains unexpected characters"
        )));
    }
    Ok(())
}

fn validate_text(value: &str, field: &str, maximum: usize) -> Result<(), AdapterError> {
    if value.trim().is_empty() || value.len() > maximum {
        return Err(AdapterError::InvalidPayload(format!(
            "Polymarket {field} must contain 1 to {maximum} bytes"
        )));
    }
    Ok(())
}

fn payload_error(error: impl std::fmt::Display) -> AdapterError {
    AdapterError::InvalidPayload(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(payload: &str) -> RawRecord {
        RawRecord {
            schema_version: 1,
            provider: Provider::Polymarket,
            stream: crate::CaptureStream::PolymarketMarket,
            connection_epoch: 1,
            ordinal: 1,
            received_at: "2026-08-26T00:00:00Z".into(),
            capture_elapsed_ns: None,
            connect_started_elapsed_ns: None,
            kind: crate::RecordKind::InboundText,
            payload: Some(payload.into()),
        }
    }

    fn configured_adapter() -> PolymarketAdapter {
        let mut adapter = PolymarketAdapter::new("test").expect("adapter constructs");
        adapter.contract = Some(MarketContract {
            market_id: "1".to_owned(),
            condition_id: format!("0x{}", "1".repeat(64)),
            question_id: format!("0x{}", "2".repeat(64)),
            slug: "btc-updown-5m-1786726500".to_owned(),
            question: "BTC Up or Down?".to_owned(),
            up_token_id: "11".to_owned(),
            down_token_id: "22".to_owned(),
            window_open: "2026-08-14T16:55:00Z".to_owned(),
            window_close: "2026-08-14T17:00:00Z".to_owned(),
            resolution_source: CHAINLINK_BTC_TWAP_URL.to_owned(),
            minimum_tick_size: "0.01".to_owned(),
            minimum_order_size: "1".to_owned(),
            order_book_enabled: true,
            active: true,
            closed: false,
            rules_sha256: "0".repeat(64),
            event_time: "2026-08-14T16:55:00Z".to_owned(),
            received_at: "2026-08-14T16:55:00Z".to_owned(),
        });
        adapter
    }

    #[test]
    fn literal_text_ping_contract_and_closed_lookup_are_explicit() {
        assert_eq!(POLYMARKET_MARKET_WS_URL, websocket_endpoint());
        assert!(
            parse_request_path("/markets?slug=btc-updown-5m-1786726500&closed=true")
                .expect("valid query")
                .1
        );
        assert!(parse_request_path("/markets?slug=btc-updown-5m-1786726500").is_err());
    }

    #[test]
    fn provider_decimal_forms_are_canonicalized_without_float() {
        assert_eq!(
            canonical_decimal(".48", "price")
                .expect("leading decimal")
                .0,
            "0.48"
        );
        assert_eq!(
            canonical_decimal("1.2300", "price")
                .expect("trailing zeros")
                .0,
            "1.23"
        );
        assert!(canonical_decimal("1e-3", "price").is_err());
    }

    #[test]
    fn provider_decimal_rejects_precision_loss() {
        // Exact boundary and leading-zero forms remain accepted.
        assert_eq!(
            canonical_decimal("79228162514264337593543950335", "price")
                .expect("2^96 - 1 literal")
                .0,
            "79228162514264337593543950335"
        );
        assert_eq!(
            canonical_decimal("007", "price").expect("leading zeros").0,
            "7"
        );
        // 29 significant integer digits exceed the 96-bit coefficient.
        assert!(canonical_decimal("99999999999999999999999999999", "price").is_err());
        // A nonzero digit past scale 28 cannot survive without rounding.
        assert!(canonical_decimal("0.12345678901234567890123456789", "price").is_err());
        assert!(canonical_decimal("1.000000000000000000000000000001", "price").is_err());
    }

    #[test]
    fn slug_window_is_checked_against_event_time() {
        assert_eq!(
            validate_window(
                "btc-updown-5m-1786726500",
                "2026-08-14T16:55:00Z",
                "2026-08-14T17:00:00Z"
            )
            .expect("window matches"),
            "2026-08-14T16:55:00Z"
        );
        assert!(
            validate_window(
                "btc-updown-5m-1786726500",
                "2026-08-14T16:54:00Z",
                "2026-08-14T17:00:00Z"
            )
            .is_err()
        );
    }

    #[test]
    fn global_new_market_announcement_does_not_invalidate_the_binding() {
        let mut adapter = configured_adapter();
        let payload = r#"{"event_type":"new_market","market":"unrelated"}"#;
        let outcome = adapter
            .ingest_payload(&record(payload), payload)
            .expect("global market announcement is retained");
        assert_eq!(outcome.notices.len(), 1);
        assert_eq!(outcome.notices[0].code, FeedNoticeCode::UnsupportedMessage);
        assert_eq!(outcome.notices[0].recovery, None);
        assert!(!adapter.rediscovery_required);
    }

    #[test]
    fn an_unsupported_shape_is_a_notice_but_an_invalid_modelled_value_refuses() {
        // Ruling E61: a frame with no `event_type` discriminator is an
        // unsupported shape retained as a counted notice, never an AdapterError,
        // while a modelled `book` message with an invalid value stays fail-closed.
        let mut adapter = configured_adapter();
        let discriminatorless = r#"{"connection_id":"c-1","ping":true}"#;
        let outcome = adapter
            .ingest_payload(&record(discriminatorless), discriminatorless)
            .expect("a frame without an event_type discriminator is not an AdapterError");
        assert!(outcome.events.is_empty());
        assert_eq!(outcome.notices.len(), 1);
        assert_eq!(outcome.notices[0].code, FeedNoticeCode::UnsupportedMessage);

        let condition_id = format!("0x{}", "1".repeat(64));
        let invalid_book = json!({
            "event_type": "book",
            "asset_id": "11",
            "market": condition_id,
            "bids": [{"price": "1e-3", "size": "5"}],
            "asks": [{"price": "0.52", "size": "5"}],
            "timestamp": "1786726500000",
            "hash": "h"
        })
        .to_string();
        let error = adapter.ingest_payload(&record(&invalid_book), &invalid_book);
        assert!(matches!(error, Err(AdapterError::InvalidPayload(_))));
    }

    #[test]
    fn current_market_mutations_fail_closed_for_rediscovery() {
        let condition_id = format!("0x{}", "1".repeat(64));
        let cases = [
            format!(
                r#"{{"event_type":"tick_size_change","asset_id":"11","market":"{condition_id}","old_tick_size":"0.01","new_tick_size":"0.001","timestamp":"1786726500000"}}"#
            ),
            format!(
                r#"{{"event_type":"market_resolved","market":"{condition_id}","assets_ids":["11","22"],"winning_asset_id":"11","timestamp":"1786726500000"}}"#
            ),
        ];
        for payload in cases {
            let mut adapter = configured_adapter();
            let outcome = adapter
                .ingest_payload(&record(&payload), &payload)
                .expect("current-market mutation becomes a recovery notice");
            assert_eq!(outcome.notices.len(), 1);
            assert_eq!(outcome.notices[0].code, FeedNoticeCode::StateInconsistent);
            assert_eq!(
                outcome.notices[0].recovery,
                Some(RecoveryAction::RediscoverPredictionMarket)
            );
            assert!(adapter.rediscovery_required);
        }
    }

    #[test]
    fn unrelated_resolution_does_not_invalidate_the_binding() {
        let mut adapter = configured_adapter();
        let unrelated = format!("0x{}", "3".repeat(64));
        let payload = format!(
            r#"{{"event_type":"market_resolved","market":"{unrelated}","assets_ids":["33","44"],"winning_asset_id":"33","timestamp":"1786726500000"}}"#
        );
        let outcome = adapter
            .ingest_payload(&record(&payload), &payload)
            .expect("unrelated resolution is retained");
        assert_eq!(outcome.notices[0].recovery, None);
        assert!(!adapter.rediscovery_required);
    }

    #[test]
    fn trade_identity_retention_is_bounded_and_deduplicated() {
        let mut adapter = PolymarketAdapter::new("test").expect("adapter constructs");
        adapter.remember_trade("fingerprint".into());
        adapter.remember_trade("fingerprint".into());
        assert_eq!(adapter.seen_trade_fingerprints.len(), 1);
        assert_eq!(adapter.trade_fingerprint_order.len(), 1);
    }

    #[test]
    fn composite_trade_redelivery_is_idempotent_and_same_transaction_fills_are_distinct() {
        let mut adapter = configured_adapter();
        let hash = format!("0x{}", "a".repeat(64));
        let payload = |price: &str| {
            json!({
                "event_type": "last_trade_price",
                "asset_id": "11",
                "market": format!("0x{}", "1".repeat(64)),
                "transaction_hash": hash,
                "price": price,
                "size": "2",
                "side": "BUY",
                "fee_rate_bps": "0",
                "timestamp": "1786726502000"
            })
            .to_string()
        };
        let first = payload("0.51");
        assert_eq!(
            adapter
                .ingest_payload(&record(&first), &first)
                .expect("first trade admits")
                .events
                .len(),
            1
        );
        assert_eq!(
            adapter
                .ingest_payload(&record(&first), &first)
                .expect("exact redelivery is idempotent")
                .notices[0]
                .code,
            FeedNoticeCode::Duplicate
        );
        let second_fill = payload("0.52");
        assert_eq!(
            adapter
                .ingest_payload(&record(&second_fill), &second_fill)
                .expect("a distinct fill in the same transaction admits")
                .events
                .len(),
            1
        );
        assert_eq!(adapter.seen_trade_fingerprints.len(), 2);
    }
}
