//! Binance spot BTCUSDT adapter: partial-depth book snapshots and aggregate
//! trades from the combined-stream socket. Capture-only — no source definition.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::value::RawValue;
use serde_json::{Value, json};
use signals_core::SourceEvent;

use crate::adapter::{GuardAction, RecordGuard, bounded_token, validate_publisher_epoch};
use crate::decimal::validate_decimal;
use crate::event_window::millis_to_rfc3339;
use crate::{
    AdapterError, FeedAdapter, FeedNotice, FeedNoticeCode, IngestOutcome, Provider, RawRecord,
    RecoveryAction,
};

const INSTRUMENT: &str = "BTCUSDT";
const DEPTH_SOURCE_ID: &str = "binance.spot.btcusdt.book-levels";
const DEPTH_SOURCE_VERSION: &str = "1.0.0";
const AGG_TRADE_SOURCE_ID: &str = "binance.spot.btcusdt.agg-trades";
const AGG_TRADE_SOURCE_VERSION: &str = "1.0.0";
const BOOK_TICKER_SOURCE_ID: &str = "binance.spot.btcusdt.book-ticker";
const BOOK_TICKER_SOURCE_VERSION: &str = "1.0.0";

// Combined-stream discriminators. The capture connects to Binance's combined
// endpoint and subscribes to exactly these two spot streams, so every data
// message is wrapped as `{"stream":<name>,"data":{..}}` and dispatched by name.
// These names are the single source of truth for both the capture subscription
// and this adapter's dispatch.
//
// `btcusdt@depth20@100ms` is the PARTIAL book-depth stream: each message is a
// COMPLETE top-20 bid/ask snapshot (full-replace book state), not a diff. It
// carries no `U`/`u` first/final update ids and therefore no per-message gap
// semantics -- the only ordering guarantee is that `lastUpdateId` is strictly
// increasing across messages, which the adapter enforces (a duplicate or
// out-of-order id is dropped, never applied). Book seeding needs no REST
// snapshot and no diff continuity machinery, and staleness is receipt-time
// based via the existing freshness conventions, not a stream-side gap signal.
//
// The full-depth `@depth@100ms` diff stream would give sub-top-20 depth, but a
// diff stream must be seeded and re-seeded from a REST snapshot after every
// connection, which this crate's capture layer (WebSocket only) does not record
// today. Switch to it only if a production signal needs depth below the top 20,
// and only alongside a capture-layer snapshot record to seed the diff book.
pub(crate) const DEPTH_STREAM: &str = "btcusdt@depth20@100ms";
pub(crate) const AGG_TRADE_STREAM: &str = "btcusdt@aggTrade";
// `btcusdt@bookTicker` is the venue's DISPLAYED BEST QUOTE, republished on
// every best-quote change. It is the Binance analogue of the Coinbase and
// Kraken ticker, and it is a separate stream on purpose: the `depth20` top of
// book is conflated to 100ms, so deriving a quote from it would sample a
// materially different object than the other venues do and would give one
// identity two book-shaped inputs.
pub(crate) const BOOK_TICKER_STREAM: &str = "btcusdt@bookTicker";

const MAX_DEPTH_ROWS: usize = 4_096;

/// Deterministic Binance spot parser and ordering state for the combined
/// partial-depth + aggregate-trade streams.
pub struct BinanceAdapter {
    publisher_epoch: String,
    guard: RecordGuard,
    next_depth_sequence: u64,
    next_agg_trade_sequence: u64,
    next_book_ticker_sequence: u64,
    last_depth_update_id: Option<u64>,
    last_agg_trade_id: Option<u64>,
    last_book_ticker_update_id: Option<u64>,
}

impl BinanceAdapter {
    /// Construct a Binance spot adapter.
    ///
    /// `publisher_epoch` is copied verbatim into every emitted event; it must be
    /// non-blank and at most 128 bytes, or the call returns
    /// [`AdapterError::InvalidRecord`].
    pub fn new(publisher_epoch: impl Into<String>) -> Result<Self, AdapterError> {
        let publisher_epoch = publisher_epoch.into();
        validate_publisher_epoch(&publisher_epoch)?;
        Ok(Self {
            publisher_epoch,
            guard: RecordGuard::new(Provider::BinanceSpot),
            next_depth_sequence: 0,
            next_agg_trade_sequence: 0,
            next_book_ticker_sequence: 0,
            last_depth_update_id: None,
            last_agg_trade_id: None,
            last_book_ticker_update_id: None,
        })
    }

    fn reset_book_state(&mut self) {
        // A fresh connection restarts the depth cursor: the partial-depth stream
        // is full-replace, so the next snapshot rebuilds the book from scratch and
        // its `lastUpdateId` sequence begins anew. The aggregate-trade cursor
        // deliberately PERSISTS across reconnects: aggregate trade ids are a
        // monotonic per-symbol counter, so a hole spanning a reconnect is caught
        // by the same id-jump check on the first post-reconnect trade. The
        // book-ticker cursor is connection-scoped like the depth cursor: the
        // venue's order-book update id is not guaranteed to continue across a
        // fresh socket, and the first quote of a new epoch reseeds it.
        self.last_depth_update_id = None;
        self.last_book_ticker_update_id = None;
    }

    fn ingest_payload(
        &mut self,
        record: &RawRecord,
        payload: &str,
    ) -> Result<IngestOutcome, AdapterError> {
        let envelope: BinanceEnvelope = serde_json::from_str(payload).map_err(payload_error)?;
        if let Some(error) = envelope.error.as_deref() {
            return Ok(IngestOutcome::notice(FeedNotice::info(
                FeedNoticeCode::SubscriptionRejected,
                bounded_detail("Binance subscription rejected", error.get()),
            )));
        }
        match (envelope.stream.as_deref(), envelope.data.as_deref()) {
            (Some(DEPTH_STREAM), Some(data)) => self.depth(record, data),
            (Some(AGG_TRADE_STREAM), Some(data)) => self.agg_trade(record, data),
            (Some(BOOK_TICKER_STREAM), Some(data)) => self.book_ticker(record, data),
            (Some(other), Some(_)) => Ok(IngestOutcome::notice(FeedNotice::info(
                FeedNoticeCode::UnsupportedMessage,
                format!(
                    "retained unsupported Binance stream `{}` without normalized output",
                    bounded_token(other)
                ),
            ))),
            _ => {
                if envelope.id.is_some() {
                    Ok(IngestOutcome::notice(FeedNotice::info(
                        FeedNoticeCode::SubscriptionAccepted,
                        "Binance subscription acknowledged",
                    )))
                } else {
                    Ok(IngestOutcome::notice(FeedNotice::info(
                        FeedNoticeCode::UnsupportedMessage,
                        "retained unrecognized Binance message without normalized output",
                    )))
                }
            }
        }
    }

    fn depth(
        &mut self,
        record: &RawRecord,
        data: &RawValue,
    ) -> Result<IngestOutcome, AdapterError> {
        let depth: BinanceDepth = serde_json::from_str(data.get()).map_err(payload_error)?;
        // Validate shape and decimals BEFORE the ordering short-circuit, matching
        // the aggTrade path: a malformed message fails closed even when its
        // `lastUpdateId` would otherwise have been dropped as a duplicate.
        let row_count = depth
            .bids
            .len()
            .checked_add(depth.asks.len())
            .ok_or_else(|| AdapterError::InvalidPayload("depth row count overflowed".to_owned()))?;
        if row_count > MAX_DEPTH_ROWS {
            return Err(AdapterError::InvalidPayload(format!(
                "Binance depth message has {row_count} levels; maximum is {MAX_DEPTH_ROWS}"
            )));
        }
        if row_count == 0 {
            // An empty top-20 book is a degenerate venue state, not a parse
            // failure: note it and skip application so replay does not abort. The
            // depth cursor is left untouched so a later populated snapshot with a
            // higher id still applies.
            return Ok(IngestOutcome::notice(FeedNotice::info(
                FeedNoticeCode::StateInconsistent,
                "Binance depth message carried no levels; skipped without normalized output",
            )));
        }
        let mut rows = Vec::with_capacity(row_count);
        for level in &depth.bids {
            validate_decimal(&level[0], "bids.price")?;
            validate_decimal(&level[1], "bids.size")?;
            rows.push(("bid", level[0].clone(), level[1].clone()));
        }
        for level in &depth.asks {
            validate_decimal(&level[0], "asks.price")?;
            validate_decimal(&level[1], "asks.size")?;
            rows.push(("ask", level[0].clone(), level[1].clone()));
        }
        // Strictly-increasing `lastUpdateId` is the only ordering guarantee on
        // the partial-depth stream. A duplicate or an out-of-order id carries no
        // book state we can trust to be newer, so it is dropped, never applied.
        if let Some(previous) = self.last_depth_update_id {
            if depth.last_update_id == previous {
                return Ok(IngestOutcome::notice(FeedNotice::info(
                    FeedNoticeCode::Duplicate,
                    format!("duplicate Binance depth lastUpdateId {previous} ignored"),
                )));
            }
            if depth.last_update_id < previous {
                return Ok(IngestOutcome::notice(FeedNotice::info(
                    FeedNoticeCode::OutOfOrder,
                    format!(
                        "out-of-order Binance depth lastUpdateId {} after {previous} ignored",
                        depth.last_update_id
                    ),
                )));
            }
        }
        // `image_kind` discriminates a continuous image from a reseed: the
        // first image of a connection epoch (or after any ordering reset)
        // rebuilds the book from nothing, while every later image replaces a
        // book we were already carrying. Binance spot partial depth has no
        // first/final update ids, so this — not a gap guard — is the whole
        // continuity contract the order-flow and book-resilience identities can
        // read.
        let image_kind = if self.last_depth_update_id.is_some() {
            "full_replace"
        } else {
            "reseed"
        };
        self.last_depth_update_id = Some(depth.last_update_id);
        self.depth_events(record, depth.last_update_id, image_kind, rows)
    }

    fn depth_events(
        &mut self,
        record: &RawRecord,
        last_update_id: u64,
        image_kind: &'static str,
        rows: Vec<(&str, String, String)>,
    ) -> Result<IngestOutcome, AdapterError> {
        let update_count = u32::try_from(rows.len()).map_err(|_| {
            AdapterError::InvalidPayload("depth update_count exceeds u32".to_owned())
        })?;
        // Each partial-depth message is a self-contained full replacement of the
        // top-of-book: the `update_group` binds one message's levels together and
        // `update_count` states the complete set, so a level absent from this
        // message is gone from the book (top-20 truncation is authoritative).
        let update_group = format!(
            "{}:{}:{}",
            record.stream.as_str(),
            record.connection_epoch,
            record.ordinal
        );
        let mut events = Vec::with_capacity(rows.len());
        for (index, (side, price, size)) in rows.into_iter().enumerate() {
            let update_index = u32::try_from(index).map_err(|_| {
                AdapterError::InvalidPayload("depth update_index exceeds u32".to_owned())
            })?;
            let values = BTreeMap::from([
                ("instrument".to_owned(), json!(INSTRUMENT)),
                ("update_group".to_owned(), json!(update_group)),
                ("update_index".to_owned(), json!(update_index)),
                ("update_count".to_owned(), json!(update_count)),
                ("update_kind".to_owned(), json!("snapshot")),
                ("image_kind".to_owned(), json!(image_kind)),
                ("side".to_owned(), json!(side)),
                ("price".to_owned(), json!(price)),
                ("size".to_owned(), json!(size)),
                ("last_update_id".to_owned(), json!(last_update_id)),
                ("event_time_basis".to_owned(), json!("receipt")),
                ("event_time".to_owned(), json!(record.received_at)),
            ]);
            events.push(self.source_event(
                DEPTH_SOURCE_ID,
                DEPTH_SOURCE_VERSION,
                record.received_at.clone(),
                record.received_at.clone(),
                values,
            )?);
        }
        Ok(IngestOutcome {
            events,
            notices: Vec::new(),
        })
    }

    fn agg_trade(
        &mut self,
        record: &RawRecord,
        data: &RawValue,
    ) -> Result<IngestOutcome, AdapterError> {
        let trade: BinanceAggTrade = serde_json::from_str(data.get()).map_err(payload_error)?;
        require_symbol(&trade.symbol)?;
        if trade.event_type != "aggTrade" {
            return Err(AdapterError::InvalidPayload(format!(
                "unexpected Binance aggregate-trade event type `{}`",
                bounded_token(&trade.event_type)
            )));
        }
        validate_decimal(&trade.price, "price")?;
        validate_decimal(&trade.quantity, "quantity")?;
        let event_time = millis_to_rfc3339(trade.event_time, "event_time")?;
        let trade_time = millis_to_rfc3339(trade.trade_time, "trade_time")?;

        let mut notices = Vec::new();
        if let Some(previous) = self.last_agg_trade_id {
            if trade.agg_trade_id == previous {
                return Ok(IngestOutcome::notice(FeedNotice::info(
                    FeedNoticeCode::Duplicate,
                    format!(
                        "duplicate Binance aggregate trade id {} ignored",
                        trade.agg_trade_id
                    ),
                )));
            }
            if trade.agg_trade_id < previous {
                return Ok(IngestOutcome::notice(FeedNotice::info(
                    FeedNoticeCode::OutOfOrder,
                    format!(
                        "out-of-order Binance aggregate trade id {} after {previous} ignored",
                        trade.agg_trade_id
                    ),
                )));
            }
            if trade.agg_trade_id > previous.saturating_add(1) {
                // The aggregate trade id must advance by exactly one. A jump means
                // trades between `previous` and this id were missed. The jump is
                // SIGNALED, not repaired: a Binance WebSocket reconnect does not
                // backfill the missed span and REST backfill is deferred to the
                // adoption slice, so the recovery notice is an observability
                // marker for a downstream consumer. The in-hand trade is valid
                // and is never discarded -- it is emitted alongside the notice and
                // the cursor advances to it, so the tape resumes contiguously.
                notices.push(FeedNotice::recovery(
                    FeedNoticeCode::Gap,
                    format!(
                        "Binance aggregate trade id jumped from {previous} to {}",
                        trade.agg_trade_id
                    ),
                    RecoveryAction::Reconnect,
                ));
            }
        }

        self.last_agg_trade_id = Some(trade.agg_trade_id);
        let event =
            self.agg_trade_event(&trade, event_time, trade_time, record.received_at.clone())?;
        Ok(IngestOutcome {
            events: vec![event],
            notices,
        })
    }

    fn agg_trade_event(
        &mut self,
        trade: &BinanceAggTrade,
        event_time: String,
        trade_time: String,
        received_at: String,
    ) -> Result<SourceEvent, AdapterError> {
        // `m` is the buyer-is-maker flag. When true the buyer's order was resting
        // (the maker), so the aggressor/taker is the SELLER; when false the seller
        // was the maker, so the aggressor is the BUYER.
        let aggressor_side = if trade.buyer_is_maker { "sell" } else { "buy" };
        let values = BTreeMap::from([
            ("instrument".to_owned(), json!(INSTRUMENT)),
            ("agg_trade_id".to_owned(), json!(trade.agg_trade_id)),
            ("first_trade_id".to_owned(), json!(trade.first_trade_id)),
            ("last_trade_id".to_owned(), json!(trade.last_trade_id)),
            ("price".to_owned(), json!(trade.price)),
            ("quantity".to_owned(), json!(trade.quantity)),
            ("buyer_is_maker".to_owned(), json!(trade.buyer_is_maker)),
            ("aggressor_side".to_owned(), json!(aggressor_side)),
            ("event_time".to_owned(), json!(event_time)),
            ("trade_time".to_owned(), json!(trade_time)),
        ]);
        self.source_event(
            AGG_TRADE_SOURCE_ID,
            AGG_TRADE_SOURCE_VERSION,
            event_time.clone(),
            received_at,
            values,
        )
    }

    /// `bookTicker`: the displayed best bid and ask.
    ///
    /// The spot payload carries no venue clock, so `event_time_basis` is
    /// `receipt` and `event_time` is our own receipt of the frame. A CROSSED
    /// quote (bid at or above ask) is emitted verbatim rather than repaired:
    /// the venue said it, and a consumer's venue-local invalidity path is where
    /// that judgement belongs.
    fn book_ticker(
        &mut self,
        record: &RawRecord,
        data: &RawValue,
    ) -> Result<IngestOutcome, AdapterError> {
        let quote: BinanceBookTicker = serde_json::from_str(data.get()).map_err(payload_error)?;
        require_symbol(&quote.symbol)?;
        // Validated before the ordering short-circuit, as everywhere else here.
        for (value, field) in [
            (&quote.best_bid, "best_bid"),
            (&quote.best_bid_size, "best_bid_size"),
            (&quote.best_ask, "best_ask"),
            (&quote.best_ask_size, "best_ask_size"),
        ] {
            let decimal = validate_decimal(value, field)?;
            if decimal <= rust_decimal::Decimal::ZERO {
                return Err(AdapterError::InvalidPayload(format!(
                    "Binance book ticker {field} must be positive"
                )));
            }
        }
        if let Some(previous) = self.last_book_ticker_update_id {
            if quote.order_book_update_id == previous {
                return Ok(IngestOutcome::notice(FeedNotice::info(
                    FeedNoticeCode::Duplicate,
                    format!("duplicate Binance book ticker update id {previous} ignored"),
                )));
            }
            if quote.order_book_update_id < previous {
                return Ok(IngestOutcome::notice(FeedNotice::info(
                    FeedNoticeCode::OutOfOrder,
                    format!(
                        "out-of-order Binance book ticker update id {} after {previous} ignored",
                        quote.order_book_update_id
                    ),
                )));
            }
        }
        self.last_book_ticker_update_id = Some(quote.order_book_update_id);

        let values = BTreeMap::from([
            ("instrument".to_owned(), json!(INSTRUMENT)),
            (
                "order_book_update_id".to_owned(),
                json!(quote.order_book_update_id),
            ),
            ("best_bid".to_owned(), json!(quote.best_bid)),
            ("best_bid_size".to_owned(), json!(quote.best_bid_size)),
            ("best_ask".to_owned(), json!(quote.best_ask)),
            ("best_ask_size".to_owned(), json!(quote.best_ask_size)),
            ("event_time_basis".to_owned(), json!("receipt")),
            ("event_time".to_owned(), json!(record.received_at)),
        ]);
        let event = self.source_event(
            BOOK_TICKER_SOURCE_ID,
            BOOK_TICKER_SOURCE_VERSION,
            record.received_at.clone(),
            record.received_at.clone(),
            values,
        )?;
        Ok(IngestOutcome {
            events: vec![event],
            notices: Vec::new(),
        })
    }

    fn source_event(
        &mut self,
        source_id: &'static str,
        source_version: &'static str,
        event_time: String,
        received_at: String,
        values: BTreeMap<String, Value>,
    ) -> Result<SourceEvent, AdapterError> {
        let sequence = match source_id {
            DEPTH_SOURCE_ID => &mut self.next_depth_sequence,
            AGG_TRADE_SOURCE_ID => &mut self.next_agg_trade_sequence,
            BOOK_TICKER_SOURCE_ID => &mut self.next_book_ticker_sequence,
            _ => {
                return Err(AdapterError::InvalidRecord(
                    "unknown Binance source identity".to_owned(),
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
            source_version: source_version.to_owned(),
            publisher_epoch: self.publisher_epoch.clone(),
            sequence: current,
            event_time,
            received_at,
            values,
        })
    }
}

impl FeedAdapter for BinanceAdapter {
    fn ingest(&mut self, record: &RawRecord) -> Result<IngestOutcome, AdapterError> {
        match self.guard.observe(record)? {
            GuardAction::Configuration(_) => Err(AdapterError::InvalidRecord(
                "Binance captures do not accept configuration records".to_owned(),
            )),
            GuardAction::Connected => {
                self.reset_book_state();
                Ok(IngestOutcome::notice(FeedNotice::info(
                    FeedNoticeCode::Connected,
                    "Binance WebSocket connected; book awaits a fresh partial-depth snapshot",
                )))
            }
            GuardAction::Disconnected => {
                self.reset_book_state();
                Ok(IngestOutcome::notice(FeedNotice::recovery(
                    FeedNoticeCode::Disconnected,
                    "Binance WebSocket disconnected",
                    RecoveryAction::Reconnect,
                )))
            }
            GuardAction::Outbound => Ok(IngestOutcome::default()),
            GuardAction::Failed(_) => Err(AdapterError::InvalidRecord(
                "Binance captures do not accept failure records".to_owned(),
            )),
            GuardAction::Inbound(payload) => self.ingest_payload(record, payload),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BinanceEnvelope {
    #[serde(default)]
    stream: Option<String>,
    #[serde(default)]
    data: Option<Box<RawValue>>,
    #[serde(default)]
    id: Option<u64>,
    // A subscription ack is `{"result":null,"id":1}`; capture `result` so the
    // envelope denies genuinely unknown top-level fields without rejecting acks.
    #[serde(default)]
    #[allow(dead_code)]
    result: Option<Box<RawValue>>,
    #[serde(default)]
    error: Option<Box<RawValue>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BinanceDepth {
    #[serde(rename = "lastUpdateId")]
    last_update_id: u64,
    bids: Vec<[String; 2]>,
    asks: Vec<[String; 2]>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BinanceAggTrade {
    #[serde(rename = "e")]
    event_type: String,
    #[serde(rename = "E")]
    event_time: i64,
    #[serde(rename = "s")]
    symbol: String,
    #[serde(rename = "a")]
    agg_trade_id: u64,
    #[serde(rename = "p")]
    price: String,
    #[serde(rename = "q")]
    quantity: String,
    #[serde(rename = "f")]
    first_trade_id: u64,
    #[serde(rename = "l")]
    last_trade_id: u64,
    #[serde(rename = "T")]
    trade_time: i64,
    #[serde(rename = "m")]
    buyer_is_maker: bool,
    // Deprecated Binance "M" ("was the trade the best price match"); captured
    // only so the wire struct denies genuinely unknown fields. Never emitted.
    #[serde(rename = "M")]
    #[allow(dead_code)]
    best_match: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BinanceBookTicker {
    #[serde(rename = "u")]
    order_book_update_id: u64,
    #[serde(rename = "s")]
    symbol: String,
    #[serde(rename = "b")]
    best_bid: String,
    #[serde(rename = "B")]
    best_bid_size: String,
    #[serde(rename = "a")]
    best_ask: String,
    #[serde(rename = "A")]
    best_ask_size: String,
}

fn require_symbol(symbol: &str) -> Result<(), AdapterError> {
    if symbol == INSTRUMENT {
        Ok(())
    } else {
        Err(AdapterError::InvalidPayload(format!(
            "unexpected Binance symbol `{}`",
            bounded_token(symbol)
        )))
    }
}

fn payload_error(error: serde_json::Error) -> AdapterError {
    AdapterError::InvalidPayload(error.to_string())
}

fn bounded_detail(prefix: &str, value: &str) -> String {
    format!("{prefix}: {}", bounded_token(value))
}

#[cfg(test)]
mod tests {
    use crate::{
        CaptureStream, FeedAdapter, FeedNoticeCode, Provider, RawRecord, RecordKind, RecoveryAction,
    };

    use super::BinanceAdapter;

    fn record(ordinal: u64, payload: &str) -> RawRecord {
        RawRecord {
            schema_version: 1,
            provider: Provider::BinanceSpot,
            stream: CaptureStream::BinanceSpot,
            connection_epoch: 1,
            ordinal,
            received_at: "2026-08-28T12:00:01Z".to_owned(),
            capture_elapsed_ns: None,
            connect_started_elapsed_ns: None,
            kind: RecordKind::InboundText,
            payload: Some(payload.to_owned()),
        }
    }

    fn connected() -> RawRecord {
        RawRecord {
            schema_version: 1,
            provider: Provider::BinanceSpot,
            stream: CaptureStream::BinanceSpot,
            connection_epoch: 1,
            ordinal: 1,
            received_at: "2026-08-28T12:00:00Z".to_owned(),
            capture_elapsed_ns: None,
            connect_started_elapsed_ns: None,
            kind: RecordKind::Connected,
            payload: None,
        }
    }

    fn depth(ordinal: u64, last_update_id: u64, bids: &str, asks: &str) -> RawRecord {
        record(
            ordinal,
            &format!(
                r#"{{"stream":"btcusdt@depth20@100ms","data":{{"lastUpdateId":{last_update_id},"bids":{bids},"asks":{asks}}}}}"#
            ),
        )
    }

    fn agg_trade(ordinal: u64, id: u64, price: &str, qty: &str, maker: bool) -> RawRecord {
        record(
            ordinal,
            &format!(
                r#"{{"stream":"btcusdt@aggTrade","data":{{"e":"aggTrade","E":1787918400123,"s":"BTCUSDT","a":{id},"p":"{price}","q":"{qty}","f":1,"l":2,"T":1787918400100,"m":{maker},"M":true}}}}"#
            ),
        )
    }

    #[test]
    fn subscription_ack_is_operational_not_an_event() {
        let mut adapter = BinanceAdapter::new("test-epoch").expect("adapter constructs");
        adapter.ingest(&connected()).expect("connects");
        let ack = adapter
            .ingest(&record(2, r#"{"result":null,"id":1}"#))
            .expect("ack is operational state");
        assert!(ack.events.is_empty());
        assert_eq!(ack.notices[0].code, FeedNoticeCode::SubscriptionAccepted);
    }

    #[test]
    fn partial_depth_message_fully_replaces_the_book_per_message() {
        let mut adapter = BinanceAdapter::new("test-epoch").expect("adapter constructs");
        adapter.ingest(&connected()).expect("connects");
        let first = adapter
            .ingest(&depth(
                2,
                100,
                r#"[["60000.00","1.0"],["59999.00","2.0"]]"#,
                r#"[["60001.00","3.0"]]"#,
            ))
            .expect("first snapshot applies");
        // Every level of the message is emitted as one full-replace snapshot row,
        // bound by a shared update_group and a complete update_count.
        assert_eq!(first.events.len(), 3);
        assert!(first.notices.is_empty());
        for event in &first.events {
            assert_eq!(
                event.values.get("update_kind"),
                Some(&serde_json::json!("snapshot"))
            );
            assert_eq!(
                event.values.get("update_count"),
                Some(&serde_json::json!(3))
            );
            assert_eq!(
                event.values.get("last_update_id"),
                Some(&serde_json::json!(100))
            );
        }
        assert_eq!(
            first.events[0].values.get("side"),
            Some(&serde_json::json!("bid"))
        );
        assert_eq!(
            first.events[2].values.get("side"),
            Some(&serde_json::json!("ask"))
        );

        // A later message with a higher id fully replaces the book: a price
        // present before but absent now (59999.00) simply does not appear -- the
        // top-N set is authoritative and a dropped level is gone.
        let second = adapter
            .ingest(&depth(
                3,
                101,
                r#"[["60000.50","1.5"]]"#,
                r#"[["60001.00","3.0"]]"#,
            ))
            .expect("second snapshot applies");
        assert_eq!(second.events.len(), 2);
        let prices: Vec<_> = second
            .events
            .iter()
            .map(|event| event.values.get("price").cloned())
            .collect();
        assert_eq!(
            prices,
            vec![
                Some(serde_json::json!("60000.50")),
                Some(serde_json::json!("60001.00")),
            ]
        );
        assert!(!prices.contains(&Some(serde_json::json!("59999.00"))));
    }

    #[test]
    fn depth_reorder_and_duplicate_are_dropped_with_a_notice() {
        let mut adapter = BinanceAdapter::new("test-epoch").expect("adapter constructs");
        adapter.ingest(&connected()).expect("connects");
        assert_eq!(
            adapter
                .ingest(&depth(
                    2,
                    100,
                    r#"[["60000.00","1.0"]]"#,
                    r#"[["60001.00","1.0"]]"#
                ))
                .expect("first snapshot")
                .events
                .len(),
            2
        );
        // Equal id: duplicate, never applied.
        let duplicate = adapter
            .ingest(&depth(
                3,
                100,
                r#"[["70000.00","9.0"]]"#,
                r#"[["70001.00","9.0"]]"#,
            ))
            .expect("duplicate is operational");
        assert!(duplicate.events.is_empty());
        assert_eq!(duplicate.notices[0].code, FeedNoticeCode::Duplicate);
        // Lower id: out of order, never applied.
        let stale = adapter
            .ingest(&depth(
                4,
                99,
                r#"[["70000.00","9.0"]]"#,
                r#"[["70001.00","9.0"]]"#,
            ))
            .expect("out-of-order is operational");
        assert!(stale.events.is_empty());
        assert_eq!(stale.notices[0].code, FeedNoticeCode::OutOfOrder);
        // Strictly greater id applies again.
        assert_eq!(
            adapter
                .ingest(&depth(
                    5,
                    101,
                    r#"[["60002.00","1.0"]]"#,
                    r#"[["60003.00","1.0"]]"#
                ))
                .expect("newer snapshot")
                .events
                .len(),
            2
        );
    }

    #[test]
    fn depth_preserves_high_precision_decimals_and_rejects_malformed() {
        let mut adapter = BinanceAdapter::new("test-epoch").expect("adapter constructs");
        adapter.ingest(&connected()).expect("connects");
        let outcome = adapter
            .ingest(&depth(
                2,
                100,
                r#"[["60000.00000001","0.00000010"]]"#,
                r#"[["60001.00","1.0"]]"#,
            ))
            .expect("exact decimals apply");
        // The exact provider text is preserved verbatim (no float round-trip).
        assert_eq!(
            outcome.events[0].values.get("price"),
            Some(&serde_json::json!("60000.00000001"))
        );
        assert_eq!(
            outcome.events[0].values.get("size"),
            Some(&serde_json::json!("0.00000010"))
        );
        // A non-decimal price fails closed.
        assert!(
            adapter
                .ingest(&depth(
                    3,
                    101,
                    r#"[["1e5","1.0"]]"#,
                    r#"[["60001.00","1.0"]]"#
                ))
                .is_err()
        );
    }

    #[test]
    fn agg_trade_emits_with_aggressor_side_mapping() {
        let mut adapter = BinanceAdapter::new("test-epoch").expect("adapter constructs");
        adapter.ingest(&connected()).expect("connects");
        // m=true -> buyer is maker -> aggressor is the seller.
        let sell = adapter
            .ingest(&agg_trade(2, 10, "60000.00000001", "0.5", true))
            .expect("first aggregate trade");
        assert_eq!(sell.events.len(), 1);
        assert_eq!(
            sell.events[0].values.get("aggressor_side"),
            Some(&serde_json::json!("sell"))
        );
        assert_eq!(
            sell.events[0].values.get("price"),
            Some(&serde_json::json!("60000.00000001"))
        );
        assert_eq!(
            sell.events[0].values.get("event_time"),
            Some(&serde_json::json!("2026-08-28T12:00:00.123Z"))
        );
        assert_eq!(
            sell.events[0].values.get("trade_time"),
            Some(&serde_json::json!("2026-08-28T12:00:00.100Z"))
        );
        // m=false -> seller is maker -> aggressor is the buyer.
        let buy = adapter
            .ingest(&agg_trade(3, 11, "60001.00", "0.25", false))
            .expect("second aggregate trade");
        assert_eq!(
            buy.events[0].values.get("aggressor_side"),
            Some(&serde_json::json!("buy"))
        );
    }

    #[test]
    fn agg_trade_dedup_and_reorder_are_dropped() {
        let mut adapter = BinanceAdapter::new("test-epoch").expect("adapter constructs");
        adapter.ingest(&connected()).expect("connects");
        adapter
            .ingest(&agg_trade(2, 10, "60000.00", "0.5", true))
            .expect("first trade");
        let duplicate = adapter
            .ingest(&agg_trade(3, 10, "60000.00", "0.5", true))
            .expect("duplicate is operational");
        assert!(duplicate.events.is_empty());
        assert_eq!(duplicate.notices[0].code, FeedNoticeCode::Duplicate);
        let stale = adapter
            .ingest(&agg_trade(4, 9, "60000.00", "0.5", true))
            .expect("out-of-order is operational");
        assert!(stale.events.is_empty());
        assert_eq!(stale.notices[0].code, FeedNoticeCode::OutOfOrder);
    }

    #[test]
    fn agg_trade_id_gap_signals_recovery_and_still_emits_the_trade() {
        let mut adapter = BinanceAdapter::new("test-epoch").expect("adapter constructs");
        adapter.ingest(&connected()).expect("connects");
        adapter
            .ingest(&agg_trade(2, 10, "60000.00", "0.5", true))
            .expect("first trade");
        // A jump from 10 to 13 means ids 11 and 12 were missed. The gap is
        // signaled with a recovery notice, but the in-hand trade is valid and is
        // emitted -- valid data is never discarded.
        let gap = adapter
            .ingest(&agg_trade(3, 13, "60003.00", "0.5", false))
            .expect("gap is operational state");
        assert_eq!(gap.events.len(), 1);
        assert_eq!(
            gap.events[0].values.get("agg_trade_id"),
            Some(&serde_json::json!(13))
        );
        assert_eq!(gap.notices[0].code, FeedNoticeCode::Gap);
        assert_eq!(gap.notices[0].recovery, Some(RecoveryAction::Reconnect));
        // The cursor advanced to the observed id, so a contiguous id no longer
        // signals a gap.
        let resumed = adapter
            .ingest(&agg_trade(4, 14, "60004.00", "0.5", true))
            .expect("contiguous trade resumes");
        assert_eq!(resumed.events.len(), 1);
        assert!(resumed.notices.is_empty());
    }

    #[test]
    fn agg_trade_cursor_persists_across_reconnect_and_catches_a_boundary_hole() {
        let mut adapter = BinanceAdapter::new("test-epoch").expect("adapter constructs");
        adapter.ingest(&connected()).expect("connects");
        adapter
            .ingest(&agg_trade(2, 10, "60000.00", "0.5", true))
            .expect("trade before reconnect");
        // Capture records a Disconnected then a fresh Connected epoch.
        let disconnected = RawRecord {
            connection_epoch: 1,
            ordinal: 3,
            kind: RecordKind::Disconnected,
            payload: None,
            ..connected()
        };
        adapter.ingest(&disconnected).expect("disconnects");
        let reconnect = RawRecord {
            connection_epoch: 2,
            ordinal: 4,
            ..connected()
        };
        adapter.ingest(&reconnect).expect("reconnects");
        // The aggregate-trade cursor survived the reconnect: a jumped id on the
        // first post-reconnect trade is still caught, and the trade is emitted.
        let jumped = RawRecord {
            connection_epoch: 2,
            ..agg_trade(5, 13, "60003.00", "0.5", false)
        };
        let gap = adapter
            .ingest(&jumped)
            .expect("boundary hole is operational");
        assert_eq!(gap.events.len(), 1);
        assert_eq!(gap.notices[0].code, FeedNoticeCode::Gap);
        // A contiguous id after the reconnect emits with no notice.
        let contiguous = RawRecord {
            connection_epoch: 2,
            ..agg_trade(6, 14, "60004.00", "0.5", true)
        };
        let resumed = adapter.ingest(&contiguous).expect("contiguous resumes");
        assert_eq!(resumed.events.len(), 1);
        assert!(resumed.notices.is_empty());
    }

    #[test]
    fn connect_resets_ordering_state() {
        let mut adapter = BinanceAdapter::new("test-epoch").expect("adapter constructs");
        adapter.ingest(&connected()).expect("connects");
        adapter
            .ingest(&depth(
                2,
                100,
                r#"[["60000.00","1.0"]]"#,
                r#"[["60001.00","1.0"]]"#,
            ))
            .expect("first snapshot");
        // A capture records a Disconnected before the next Connected; the new
        // connection begins a fresh epoch that restarts the book. A lower id
        // after the reset is not treated as stale because the cursor was cleared.
        let disconnected = RawRecord {
            connection_epoch: 1,
            ordinal: 3,
            kind: RecordKind::Disconnected,
            payload: None,
            ..connected()
        };
        adapter.ingest(&disconnected).expect("disconnects");
        let reconnect = RawRecord {
            connection_epoch: 2,
            ordinal: 4,
            ..connected()
        };
        adapter.ingest(&reconnect).expect("reconnects");
        let after = RawRecord {
            connection_epoch: 2,
            ..record(
                5,
                r#"{"stream":"btcusdt@depth20@100ms","data":{"lastUpdateId":50,"bids":[["60000.00","1.0"]],"asks":[["60001.00","1.0"]]}}"#,
            )
        };
        assert_eq!(
            adapter
                .ingest(&after)
                .expect("post-reset snapshot")
                .events
                .len(),
            2
        );
    }

    #[test]
    fn replay_is_deterministic_over_a_mixed_fixture_sequence() {
        let fixture = vec![
            connected(),
            depth(2, 100, r#"[["60000.00","1.0"]]"#, r#"[["60001.00","2.0"]]"#),
            agg_trade(3, 10, "60000.50", "0.5", true),
            depth(4, 100, r#"[["70000.00","9.0"]]"#, r#"[["70001.00","9.0"]]"#),
            agg_trade(5, 10, "60000.50", "0.5", true),
            depth(6, 101, r#"[["60002.00","1.0"]]"#, r#"[["60003.00","1.0"]]"#),
            agg_trade(7, 11, "60002.00", "0.25", false),
        ];

        let run = |records: &[RawRecord]| {
            let mut adapter = BinanceAdapter::new("test-epoch").expect("adapter constructs");
            let mut events = Vec::new();
            for record in records {
                events.extend(adapter.ingest(record).expect("record ingests").events);
            }
            serde_json::to_string(&events).expect("events serialize")
        };

        assert_eq!(run(&fixture), run(&fixture));
    }

    #[test]
    fn millis_bounds_reject_zero_negative_and_microsecond_scale() {
        use crate::event_window::{MAX_EVENT_MILLIS, MIN_EVENT_MILLIS, millis_to_rfc3339};
        // Plausible boundary values are accepted.
        assert!(millis_to_rfc3339(MIN_EVENT_MILLIS, "event_time").is_ok());
        assert!(millis_to_rfc3339(MAX_EVENT_MILLIS, "event_time").is_ok());
        // Zero, negative, sub-window, and microsecond-scale values fail closed.
        assert!(millis_to_rfc3339(0, "event_time").is_err());
        assert!(millis_to_rfc3339(-1, "event_time").is_err());
        assert!(millis_to_rfc3339(MIN_EVENT_MILLIS - 1, "event_time").is_err());
        // A millisecond timestamp read as microseconds is ~1000x too large.
        assert!(millis_to_rfc3339(1_787_918_400_123_000, "event_time").is_err());
        // The same trap surfaces end-to-end through the aggTrade path.
        let mut adapter = BinanceAdapter::new("test-epoch").expect("adapter constructs");
        adapter.ingest(&connected()).expect("connects");
        let micros = record(
            2,
            r#"{"stream":"btcusdt@aggTrade","data":{"e":"aggTrade","E":1787918400123000,"s":"BTCUSDT","a":10,"p":"60000.00","q":"0.5","f":1,"l":2,"T":1787918400100000,"m":true,"M":true}}"#,
        );
        assert!(adapter.ingest(&micros).is_err());
    }

    #[test]
    fn depth_validates_before_the_duplicate_short_circuit() {
        let mut adapter = BinanceAdapter::new("test-epoch").expect("adapter constructs");
        adapter.ingest(&connected()).expect("connects");
        adapter
            .ingest(&depth(
                2,
                100,
                r#"[["60000.00","1.0"]]"#,
                r#"[["60001.00","1.0"]]"#,
            ))
            .expect("first snapshot");
        // A message repeating the last id but carrying a malformed decimal must
        // error, not be silently swallowed as a duplicate.
        assert!(
            adapter
                .ingest(&depth(
                    3,
                    100,
                    r#"[["1e5","1.0"]]"#,
                    r#"[["60001.00","1.0"]]"#
                ))
                .is_err()
        );
    }

    #[test]
    fn empty_book_message_is_skipped_without_error_or_events() {
        let mut adapter = BinanceAdapter::new("test-epoch").expect("adapter constructs");
        adapter.ingest(&connected()).expect("connects");
        adapter
            .ingest(&depth(
                2,
                100,
                r#"[["60000.00","1.0"]]"#,
                r#"[["60001.00","1.0"]]"#,
            ))
            .expect("first snapshot");
        // An empty book carries no levels: skipped with a notice, no events, and
        // the depth cursor is untouched so a later higher-id snapshot still applies.
        let empty = adapter
            .ingest(&depth(3, 101, r#"[]"#, r#"[]"#))
            .expect("empty book is operational, not an error");
        assert!(empty.events.is_empty());
        assert_eq!(empty.notices[0].code, FeedNoticeCode::StateInconsistent);
        let after = adapter
            .ingest(&depth(
                4,
                102,
                r#"[["60002.00","1.0"]]"#,
                r#"[["60003.00","1.0"]]"#,
            ))
            .expect("later snapshot still applies");
        assert_eq!(after.events.len(), 2);
    }

    #[test]
    fn wrapper_edges_are_classified_without_events() {
        let mut adapter = BinanceAdapter::new("test-epoch").expect("adapter constructs");
        adapter.ingest(&connected()).expect("connects");
        // Error frame -> subscription-rejected notice.
        let error = adapter
            .ingest(&record(
                2,
                r#"{"error":{"code":2,"msg":"Invalid request"},"id":1}"#,
            ))
            .expect("error frame is operational");
        assert!(error.events.is_empty());
        assert_eq!(error.notices[0].code, FeedNoticeCode::SubscriptionRejected);
        // Unknown stream name -> unsupported message.
        let unknown = adapter
            .ingest(&record(3, r#"{"stream":"btcusdt@ticker","data":{"x":1}}"#))
            .expect("unknown stream is operational");
        assert!(unknown.events.is_empty());
        assert_eq!(unknown.notices[0].code, FeedNoticeCode::UnsupportedMessage);
        // A message with neither stream, error, nor id -> unsupported message.
        let stray = adapter
            .ingest(&record(4, r#"{"result":"pong"}"#))
            .expect("stray message is operational");
        assert!(stray.events.is_empty());
        assert_eq!(stray.notices[0].code, FeedNoticeCode::UnsupportedMessage);
        // Malformed JSON fails closed.
        assert!(adapter.ingest(&record(5, r#"{not json"#)).is_err());
    }
}
