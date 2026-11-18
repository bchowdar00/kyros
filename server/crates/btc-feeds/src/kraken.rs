//! Kraken spot adapter for the aggregated `book` channel plus the `ticker`,
//! `trade`, `ohlc`, `instrument` and `status` channels, over BTC/USD and
//! USDT/USD, with the venue's CRC32 book checksum enforced.

use std::collections::BTreeMap;

use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::value::RawValue;
use serde_json::{Value, json};
use signals_core::SourceEvent;

use crate::adapter::{
    GuardAction, RecordGuard, bounded_token, validate_publisher_epoch, validate_timestamp,
};
use crate::decimal::{decimal_string, signed_decimal_string, update_checksum, validate_decimal};
use crate::{
    AdapterError, FeedAdapter, FeedNotice, FeedNoticeCode, IngestOutcome, Provider, RawRecord,
    RecoveryAction,
};

const BTC_SYMBOL: &str = "BTC/USD";
const USDT_SYMBOL: &str = "USDT/USD";
const TRADE_SOURCE_VERSION: &str = "1.0.0";
const BOOK_SOURCE_VERSION: &str = "1.0.0";
const BTC_TRADE_SOURCE_ID: &str = "kraken.spot.btc-usd.trades";
const USDT_TRADE_SOURCE_ID: &str = "kraken.spot.usdt-usd.trades";
const BTC_BOOK_SOURCE_ID: &str = "kraken.spot.btc-usd.book-levels";
const USDT_BOOK_SOURCE_ID: &str = "kraken.spot.usdt-usd.book-levels";
const BTC_INSTRUMENT_SOURCE_ID: &str = "kraken.spot.btc-usd.instrument";
const USDT_INSTRUMENT_SOURCE_ID: &str = "kraken.spot.usdt-usd.instrument";
const BTC_TICKER_SOURCE_ID: &str = "kraken.spot.btc-usd.ticker";
const USDT_TICKER_SOURCE_ID: &str = "kraken.spot.usdt-usd.ticker";
const BTC_OHLC_SOURCE_ID: &str = "kraken.spot.btc-usd.ohlc-1m";
const USDT_OHLC_SOURCE_ID: &str = "kraken.spot.usdt-usd.ohlc-1m";
const STATUS_SOURCE_ID: &str = "kraken.spot.status";
const REFERENCE_SOURCE_VERSION: &str = "1.0.0";
const MAX_MESSAGE_ROWS: usize = 10_000;
const MAX_INSTRUMENT_PAIRS: usize = 10_000;

/// Deterministic Kraken parser, trade deduplicator, and checksum-gated L2 state.
pub struct KrakenAdapter {
    publisher_epoch: String,
    guard: RecordGuard,
    next_btc_trade_sequence: u64,
    next_usdt_trade_sequence: u64,
    next_btc_book_sequence: u64,
    next_usdt_book_sequence: u64,
    next_btc_instrument_sequence: u64,
    next_usdt_instrument_sequence: u64,
    next_btc_ticker_sequence: u64,
    next_usdt_ticker_sequence: u64,
    next_btc_ohlc_sequence: u64,
    next_usdt_ohlc_sequence: u64,
    next_status_sequence: u64,
    last_btc_trade_id: Option<u64>,
    last_usdt_trade_id: Option<u64>,
    btc_book: BookState,
    usdt_book: BookState,
}

impl KrakenAdapter {
    /// Construct a Kraken spot adapter.
    ///
    /// `publisher_epoch` is copied verbatim into every emitted event; it must be
    /// non-blank and at most 128 bytes. The two depths must equal the depths the
    /// `book` subscriptions were made at, and each must be one of Kraken's
    /// 10, 25, 100, 500 or 1000 — the book checksum is depth-sensitive, so a
    /// wrong depth would invalidate every checksum. Any violation returns
    /// [`AdapterError::InvalidRecord`].
    pub fn new(
        publisher_epoch: impl Into<String>,
        btc_book_depth: u16,
        usdt_book_depth: u16,
    ) -> Result<Self, AdapterError> {
        let publisher_epoch = publisher_epoch.into();
        validate_publisher_epoch(&publisher_epoch)?;
        if !matches!(btc_book_depth, 10 | 25 | 100 | 500 | 1000) {
            return Err(AdapterError::InvalidRecord(
                "Kraken BTC book depth must be one of 10, 25, 100, 500, or 1000".to_owned(),
            ));
        }
        if !matches!(usdt_book_depth, 10 | 25 | 100 | 500 | 1000) {
            return Err(AdapterError::InvalidRecord(
                "Kraken USDT book depth must be one of 10, 25, 100, 500, or 1000".to_owned(),
            ));
        }
        Ok(Self {
            publisher_epoch,
            guard: RecordGuard::new(Provider::KrakenSpot),
            next_btc_trade_sequence: 0,
            next_usdt_trade_sequence: 0,
            next_btc_book_sequence: 0,
            next_usdt_book_sequence: 0,
            next_btc_instrument_sequence: 0,
            next_usdt_instrument_sequence: 0,
            next_btc_ticker_sequence: 0,
            next_usdt_ticker_sequence: 0,
            next_btc_ohlc_sequence: 0,
            next_usdt_ohlc_sequence: 0,
            next_status_sequence: 0,
            last_btc_trade_id: None,
            last_usdt_trade_id: None,
            btc_book: BookState::new(btc_book_depth),
            usdt_book: BookState::new(usdt_book_depth),
        })
    }

    fn ingest_payload(
        &mut self,
        record: &RawRecord,
        payload: &str,
    ) -> Result<IngestOutcome, AdapterError> {
        let envelope: KrakenEnvelope = serde_json::from_str(payload).map_err(payload_error)?;
        if envelope.method.as_deref() == Some("subscribe") {
            let accepted = envelope.success.unwrap_or(false);
            return if accepted {
                Ok(IngestOutcome::notice(FeedNotice::info(
                    FeedNoticeCode::SubscriptionAccepted,
                    "Kraken subscription acknowledged",
                )))
            } else {
                Ok(IngestOutcome::notice(FeedNotice::info(
                    FeedNoticeCode::SubscriptionRejected,
                    bounded_detail(
                        "Kraken subscription rejected",
                        envelope.error.as_deref().unwrap_or("missing error detail"),
                    ),
                )))
            };
        }

        match envelope.channel.as_deref() {
            Some("heartbeat") => Ok(IngestOutcome::notice(FeedNotice::info(
                FeedNoticeCode::Heartbeat,
                "Kraken heartbeat observed",
            ))),
            Some("status") => self.status(record, envelope),
            Some("trade") => self.trades(record, envelope),
            Some("book") => self.book(record, envelope),
            Some("instrument") => self.instruments(record, envelope),
            Some("ticker") => self.tickers(record, envelope),
            Some("ohlc") => self.ohlc(record, envelope),
            Some(other) => Ok(IngestOutcome::notice(FeedNotice::info(
                FeedNoticeCode::UnsupportedMessage,
                format!("retained unknown Kraken channel `{}`", bounded_token(other)),
            ))),
            None => Ok(IngestOutcome::notice(FeedNotice::info(
                FeedNoticeCode::UnsupportedMessage,
                format!(
                    "retained Kraken control frame with no channel (method `{}`)",
                    bounded_token(envelope.method.as_deref().unwrap_or("none"))
                ),
            ))),
        }
    }

    fn status(
        &mut self,
        record: &RawRecord,
        envelope: KrakenEnvelope,
    ) -> Result<IngestOutcome, AdapterError> {
        let data = required_data(envelope)?;
        let mut rows: Vec<KrakenStatus> =
            serde_json::from_str(data.get()).map_err(payload_error)?;
        if rows.len() != 1 {
            return Err(AdapterError::InvalidPayload(format!(
                "Kraken status must contain exactly one row; received {}",
                rows.len()
            )));
        }
        let row = rows
            .pop()
            .ok_or_else(|| AdapterError::InvalidPayload("Kraken status is empty".to_owned()))?;
        for (name, value) in [
            ("version", row.version.as_str()),
            ("system", row.system.as_str()),
            ("api_version", row.api_version.as_str()),
        ] {
            validate_bounded_text(value, name, 64)?;
        }
        let Some(connection_id) = row.connection_id else {
            return Ok(IngestOutcome::notice(FeedNotice::info(
                FeedNoticeCode::SubscriptionAccepted,
                "Kraken status without connection_id accepted; no status event emitted",
            )));
        };
        let values = BTreeMap::from([
            ("system_status".to_owned(), json!(row.system)),
            ("service_version".to_owned(), json!(row.version)),
            ("api_version".to_owned(), json!(row.api_version)),
            ("connection_id".to_owned(), json!(connection_id)),
            ("event_time".to_owned(), json!(record.received_at)),
        ]);
        let event = self.source_event(
            STATUS_SOURCE_ID,
            REFERENCE_SOURCE_VERSION,
            record.received_at.clone(),
            record.received_at.clone(),
            values,
        )?;
        Ok(IngestOutcome {
            events: vec![event],
            notices: vec![FeedNotice::info(
                FeedNoticeCode::VenueStatus,
                "Kraken engine status observed",
            )],
        })
    }

    fn instruments(
        &mut self,
        record: &RawRecord,
        envelope: KrakenEnvelope,
    ) -> Result<IngestOutcome, AdapterError> {
        let update_kind = message_type(&envelope)?.to_owned();
        let data = required_data(envelope)?;
        let payload: KrakenInstrumentData =
            serde_json::from_str(data.get()).map_err(payload_error)?;
        if payload.pairs.len() > MAX_INSTRUMENT_PAIRS {
            return Err(AdapterError::InvalidPayload(format!(
                "Kraken instrument message has {} pairs; maximum is {MAX_INSTRUMENT_PAIRS}",
                payload.pairs.len()
            )));
        }
        let mut events = Vec::with_capacity(2);
        for pair in payload.pairs {
            if pair.symbol != BTC_SYMBOL && pair.symbol != USDT_SYMBOL {
                continue;
            }
            let symbol = known_symbol(&pair.symbol)?;
            let expected_base = if symbol == BTC_SYMBOL { "BTC" } else { "USDT" };
            if pair.base != expected_base || pair.quote != "USD" {
                return Err(AdapterError::InvalidPayload(format!(
                    "Kraken {symbol} instrument has unexpected base or quote asset"
                )));
            }
            validate_bounded_text(&pair.status, "status", 64)?;
            let values = BTreeMap::from([
                ("instrument".to_owned(), json!(symbol)),
                ("base_asset".to_owned(), json!(pair.base)),
                ("quote_asset".to_owned(), json!(pair.quote)),
                ("status".to_owned(), json!(pair.status)),
                (
                    "quantity_increment".to_owned(),
                    json!(decimal_string(&pair.qty_increment, "qty_increment")?),
                ),
                (
                    "price_increment".to_owned(),
                    json!(decimal_string(&pair.price_increment, "price_increment")?),
                ),
                (
                    "minimum_quantity".to_owned(),
                    json!(decimal_string(&pair.qty_min, "qty_min")?),
                ),
                (
                    "minimum_notional".to_owned(),
                    json!(decimal_string(&pair.cost_min, "cost_min")?),
                ),
                ("update_kind".to_owned(), json!(&update_kind)),
                ("event_time".to_owned(), json!(record.received_at)),
            ]);
            events.push(self.source_event(
                instrument_source_id(symbol),
                REFERENCE_SOURCE_VERSION,
                record.received_at.clone(),
                record.received_at.clone(),
                values,
            )?);
        }
        if update_kind == "snapshot" && events.len() != 2 {
            return Err(AdapterError::InvalidPayload(
                "Kraken instrument message does not contain both selected pairs".to_owned(),
            ));
        }
        Ok(IngestOutcome {
            events,
            notices: Vec::new(),
        })
    }

    fn tickers(
        &mut self,
        record: &RawRecord,
        envelope: KrakenEnvelope,
    ) -> Result<IngestOutcome, AdapterError> {
        let update_kind = message_type(&envelope)?.to_owned();
        let data = required_data(envelope)?;
        let rows: Vec<KrakenTicker> = serde_json::from_str(data.get()).map_err(payload_error)?;
        require_row_count("ticker", rows.len())?;
        let mut events = Vec::with_capacity(rows.len());
        for ticker in rows {
            let symbol = known_symbol(&ticker.symbol)?;
            validate_timestamp(&ticker.timestamp, "timestamp")?;
            let values = BTreeMap::from([
                ("instrument".to_owned(), json!(symbol)),
                (
                    "best_bid".to_owned(),
                    json!(decimal_string(&ticker.bid, "bid")?),
                ),
                (
                    "best_bid_size".to_owned(),
                    json!(decimal_string(&ticker.bid_qty, "bid_qty")?),
                ),
                (
                    "best_ask".to_owned(),
                    json!(decimal_string(&ticker.ask, "ask")?),
                ),
                (
                    "best_ask_size".to_owned(),
                    json!(decimal_string(&ticker.ask_qty, "ask_qty")?),
                ),
                (
                    "last_price".to_owned(),
                    json!(decimal_string(&ticker.last, "last")?),
                ),
                (
                    "volume_24h".to_owned(),
                    json!(decimal_string(&ticker.volume, "volume")?),
                ),
                (
                    "vwap_24h".to_owned(),
                    json!(decimal_string(&ticker.vwap, "vwap")?),
                ),
                (
                    "low_24h".to_owned(),
                    json!(decimal_string(&ticker.low, "low")?),
                ),
                (
                    "high_24h".to_owned(),
                    json!(decimal_string(&ticker.high, "high")?),
                ),
                (
                    "change_24h".to_owned(),
                    json!(signed_decimal_string(&ticker.change, "change")?),
                ),
                (
                    "change_percent_24h".to_owned(),
                    json!(signed_decimal_string(&ticker.change_pct, "change_pct")?),
                ),
                ("trade_count_24h".to_owned(), json!(ticker.trades)),
                ("update_kind".to_owned(), json!(&update_kind)),
                ("event_time".to_owned(), json!(ticker.timestamp)),
            ]);
            events.push(self.source_event(
                ticker_source_id(symbol),
                REFERENCE_SOURCE_VERSION,
                ticker.timestamp,
                record.received_at.clone(),
                values,
            )?);
        }
        Ok(IngestOutcome {
            events,
            notices: Vec::new(),
        })
    }

    fn ohlc(
        &mut self,
        record: &RawRecord,
        envelope: KrakenEnvelope,
    ) -> Result<IngestOutcome, AdapterError> {
        let update_kind = message_type(&envelope)?.to_owned();
        let event_time = envelope.timestamp.clone().ok_or_else(|| {
            AdapterError::InvalidPayload("Kraken OHLC message is missing timestamp".to_owned())
        })?;
        validate_timestamp(&event_time, "timestamp")?;
        let data = required_data(envelope)?;
        let rows: Vec<KrakenOhlc> = serde_json::from_str(data.get()).map_err(payload_error)?;
        require_row_count("OHLC", rows.len())?;
        let mut events = Vec::with_capacity(rows.len());
        for candle in rows {
            let symbol = known_symbol(&candle.symbol)?;
            if candle.interval != 1 {
                return Err(AdapterError::InvalidPayload(format!(
                    "Kraken OHLC interval {} does not match the selected one-minute stream",
                    candle.interval
                )));
            }
            validate_timestamp(&candle.interval_begin, "interval_begin")?;
            validate_timestamp(&candle.timestamp, "window timestamp")?;
            let values = BTreeMap::from([
                ("instrument".to_owned(), json!(symbol)),
                ("interval_minutes".to_owned(), json!(candle.interval)),
                ("window_open".to_owned(), json!(candle.interval_begin)),
                ("window_close".to_owned(), json!(candle.timestamp)),
                (
                    "open".to_owned(),
                    json!(decimal_string(&candle.open, "open")?),
                ),
                (
                    "high".to_owned(),
                    json!(decimal_string(&candle.high, "high")?),
                ),
                ("low".to_owned(), json!(decimal_string(&candle.low, "low")?)),
                (
                    "close".to_owned(),
                    json!(decimal_string(&candle.close, "close")?),
                ),
                ("trade_count".to_owned(), json!(candle.trades)),
                (
                    "volume".to_owned(),
                    json!(decimal_string(&candle.volume, "volume")?),
                ),
                (
                    "vwap".to_owned(),
                    json!(decimal_string(&candle.vwap, "vwap")?),
                ),
                ("update_kind".to_owned(), json!(&update_kind)),
                ("event_time".to_owned(), json!(event_time)),
            ]);
            events.push(self.source_event(
                ohlc_source_id(symbol),
                REFERENCE_SOURCE_VERSION,
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

    fn trades(
        &mut self,
        record: &RawRecord,
        envelope: KrakenEnvelope,
    ) -> Result<IngestOutcome, AdapterError> {
        let is_snapshot = message_type(&envelope)? == "snapshot";
        let update_kind = if is_snapshot { "snapshot" } else { "update" };
        let data = required_data(envelope)?;
        let rows: Vec<KrakenTrade> = serde_json::from_str(data.get()).map_err(payload_error)?;
        if rows.len() > MAX_MESSAGE_ROWS {
            return Err(AdapterError::InvalidPayload(format!(
                "Kraken trade message has {} rows; maximum is {MAX_MESSAGE_ROWS}",
                rows.len()
            )));
        }

        let mut validated = rows
            .into_iter()
            .map(validate_trade)
            .collect::<Result<Vec<_>, _>>()?;
        if is_snapshot {
            validated.sort_by_key(|trade| (trade.symbol, trade.trade_id));
        }

        let mut outcome = IngestOutcome::default();
        for trade in validated {
            let last_trade_id = if trade.symbol == BTC_SYMBOL {
                &mut self.last_btc_trade_id
            } else {
                &mut self.last_usdt_trade_id
            };
            let last = *last_trade_id;
            if last == Some(trade.trade_id) {
                outcome.notices.push(FeedNotice::info(
                    FeedNoticeCode::Duplicate,
                    format!(
                        "duplicate Kraken {} trade_id {} ignored",
                        trade.symbol, trade.trade_id
                    ),
                ));
                continue;
            }
            if last.is_some_and(|previous| trade.trade_id < previous) {
                outcome.notices.push(FeedNotice::info(
                    FeedNoticeCode::OutOfOrder,
                    format!(
                        "out-of-order Kraken {} trade_id {} ignored",
                        trade.symbol, trade.trade_id
                    ),
                ));
                continue;
            }
            *last_trade_id = Some(trade.trade_id);
            let source_id = trade_source_id(trade.symbol);
            let values = BTreeMap::from([
                ("instrument".to_owned(), json!(trade.symbol)),
                ("trade_id".to_owned(), json!(trade.trade_id)),
                ("price".to_owned(), json!(trade.price)),
                ("size".to_owned(), json!(trade.quantity)),
                ("aggressor_side".to_owned(), json!(trade.side)),
                ("order_type".to_owned(), json!(trade.order_type)),
                ("update_kind".to_owned(), json!(update_kind)),
                ("event_time".to_owned(), json!(trade.timestamp)),
            ]);
            outcome.events.push(self.source_event(
                source_id,
                TRADE_SOURCE_VERSION,
                trade.timestamp,
                record.received_at.clone(),
                values,
            )?);
        }
        Ok(outcome)
    }

    fn book(
        &mut self,
        record: &RawRecord,
        envelope: KrakenEnvelope,
    ) -> Result<IngestOutcome, AdapterError> {
        let is_snapshot = message_type(&envelope)? == "snapshot";
        let data = required_data(envelope)?;
        let mut rows: Vec<KrakenBookMessage> =
            serde_json::from_str(data.get()).map_err(payload_error)?;
        if rows.len() != 1 {
            return Err(AdapterError::InvalidPayload(format!(
                "Kraken book message must contain exactly one symbol; received {}",
                rows.len()
            )));
        }
        let message = rows.pop().ok_or_else(|| {
            AdapterError::InvalidPayload("Kraken book message is empty".to_owned())
        })?;
        let symbol = known_symbol(&message.symbol)?;
        validate_timestamp(&message.timestamp, "timestamp")?;
        let provider_row_count = message
            .bids
            .len()
            .checked_add(message.asks.len())
            .ok_or_else(|| AdapterError::InvalidPayload("book row count overflowed".to_owned()))?;
        if is_snapshot && provider_row_count == 0 {
            let book = if symbol == BTC_SYMBOL {
                &mut self.btc_book
            } else {
                &mut self.usdt_book
            };
            book.clear();
            return Err(AdapterError::InvalidPayload(
                "Kraken book snapshot must contain at least one level".to_owned(),
            ));
        }
        if provider_row_count > MAX_MESSAGE_ROWS {
            return Err(AdapterError::InvalidPayload(format!(
                "Kraken book message has {provider_row_count} rows; maximum is {MAX_MESSAGE_ROWS}"
            )));
        }

        let updates = validate_book_rows(message.bids, message.asks)?;
        let book = if symbol == BTC_SYMBOL {
            &mut self.btc_book
        } else {
            &mut self.usdt_book
        };
        if !is_snapshot && !book.ready {
            return Ok(IngestOutcome::notice(FeedNotice::recovery(
                FeedNoticeCode::AwaitingSnapshot,
                format!("Kraken {symbol} update ignored before a valid snapshot"),
                RecoveryAction::ResubscribeKrakenBook {
                    symbol: symbol.to_owned(),
                    depth: book.depth,
                },
            )));
        }
        if is_snapshot {
            book.clear();
        }

        let mut changes = Vec::with_capacity(updates.len());
        for update in updates {
            book.apply(&update);
            changes.push(BookChange {
                side: update.side,
                price: update.price_text,
                quantity: update.quantity_text,
                origin: "provider",
            });
        }
        changes.extend(book.truncate());

        let actual_checksum = book.checksum();
        if actual_checksum != message.checksum {
            let depth = book.depth;
            book.clear();
            return Ok(IngestOutcome::notice(FeedNotice::recovery(
                FeedNoticeCode::ChecksumMismatch,
                format!(
                    "Kraken {symbol} checksum mismatch: expected {}, calculated {actual_checksum}",
                    message.checksum
                ),
                RecoveryAction::ResubscribeKrakenBook {
                    symbol: symbol.to_owned(),
                    depth,
                },
            )));
        }
        book.ready = true;
        let depth = book.depth;

        let update_count = u32::try_from(changes.len()).map_err(|_| {
            AdapterError::InvalidPayload("book update_count exceeds u32".to_owned())
        })?;
        let update_group = format!(
            "{}:{}:{}",
            record.stream.as_str(),
            record.connection_epoch,
            record.ordinal
        );
        let update_kind = if is_snapshot { "snapshot" } else { "delta" };
        let source_id = book_source_id(symbol);
        let mut events = Vec::with_capacity(changes.len());
        for (index, change) in changes.into_iter().enumerate() {
            let update_index = u32::try_from(index).map_err(|_| {
                AdapterError::InvalidPayload("book update_index exceeds u32".to_owned())
            })?;
            let values = BTreeMap::from([
                ("instrument".to_owned(), json!(symbol)),
                ("configured_depth".to_owned(), json!(depth)),
                ("update_group".to_owned(), json!(update_group)),
                ("update_index".to_owned(), json!(update_index)),
                ("update_count".to_owned(), json!(update_count)),
                ("update_kind".to_owned(), json!(update_kind)),
                ("side".to_owned(), json!(change.side.as_str())),
                ("price".to_owned(), json!(change.price)),
                ("size".to_owned(), json!(change.quantity)),
                ("change_origin".to_owned(), json!(change.origin)),
                ("book_checksum".to_owned(), json!(message.checksum)),
                ("event_time".to_owned(), json!(message.timestamp)),
            ]);
            events.push(self.source_event(
                source_id,
                BOOK_SOURCE_VERSION,
                message.timestamp.clone(),
                record.received_at.clone(),
                values,
            )?);
        }
        Ok(IngestOutcome {
            events,
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
            BTC_TRADE_SOURCE_ID => &mut self.next_btc_trade_sequence,
            USDT_TRADE_SOURCE_ID => &mut self.next_usdt_trade_sequence,
            BTC_BOOK_SOURCE_ID => &mut self.next_btc_book_sequence,
            USDT_BOOK_SOURCE_ID => &mut self.next_usdt_book_sequence,
            BTC_INSTRUMENT_SOURCE_ID => &mut self.next_btc_instrument_sequence,
            USDT_INSTRUMENT_SOURCE_ID => &mut self.next_usdt_instrument_sequence,
            BTC_TICKER_SOURCE_ID => &mut self.next_btc_ticker_sequence,
            USDT_TICKER_SOURCE_ID => &mut self.next_usdt_ticker_sequence,
            BTC_OHLC_SOURCE_ID => &mut self.next_btc_ohlc_sequence,
            USDT_OHLC_SOURCE_ID => &mut self.next_usdt_ohlc_sequence,
            STATUS_SOURCE_ID => &mut self.next_status_sequence,
            _ => {
                return Err(AdapterError::InvalidRecord(
                    "unknown Kraken source identity".to_owned(),
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

impl FeedAdapter for KrakenAdapter {
    fn ingest(&mut self, record: &RawRecord) -> Result<IngestOutcome, AdapterError> {
        match self.guard.observe(record)? {
            GuardAction::Configuration(_) => Err(AdapterError::InvalidRecord(
                "Kraken captures do not accept configuration records".to_owned(),
            )),
            GuardAction::Connected => {
                self.btc_book.clear();
                self.usdt_book.clear();
                Ok(IngestOutcome::notice(FeedNotice::info(
                    FeedNoticeCode::Connected,
                    "Kraken WebSocket connected; books await checksum-valid snapshots",
                )))
            }
            GuardAction::Disconnected => {
                self.btc_book.clear();
                self.usdt_book.clear();
                Ok(IngestOutcome::notice(FeedNotice::recovery(
                    FeedNoticeCode::Disconnected,
                    "Kraken WebSocket disconnected",
                    RecoveryAction::Reconnect,
                )))
            }
            GuardAction::Outbound => Ok(IngestOutcome::default()),
            GuardAction::Failed(_) => Err(AdapterError::InvalidRecord(
                "Kraken captures do not accept failure records".to_owned(),
            )),
            GuardAction::Inbound(payload) => self.ingest_payload(record, payload),
        }
    }
}

#[derive(Deserialize)]
struct KrakenEnvelope {
    channel: Option<String>,
    #[serde(rename = "type")]
    message_type: Option<String>,
    data: Option<Box<RawValue>>,
    method: Option<String>,
    success: Option<bool>,
    error: Option<String>,
    timestamp: Option<String>,
}

#[derive(Deserialize)]
struct KrakenStatus {
    version: String,
    system: String,
    api_version: String,
    // Kraken omitted this field fleet-wide during the 2026-08-20 v2.0.10
    // deploy window; a status message without it is accepted (with a notice)
    // rather than failing the run, and no status event is emitted because the
    // contract declares the field.
    connection_id: Option<u64>,
}

#[derive(Deserialize)]
struct KrakenInstrumentData {
    pairs: Vec<KrakenInstrumentPair>,
}

#[derive(Deserialize)]
struct KrakenInstrumentPair {
    symbol: String,
    base: String,
    quote: String,
    status: String,
    qty_increment: Box<RawValue>,
    price_increment: Box<RawValue>,
    qty_min: Box<RawValue>,
    cost_min: Box<RawValue>,
}

#[derive(Deserialize)]
struct KrakenTicker {
    symbol: String,
    bid: Box<RawValue>,
    bid_qty: Box<RawValue>,
    ask: Box<RawValue>,
    ask_qty: Box<RawValue>,
    last: Box<RawValue>,
    volume: Box<RawValue>,
    vwap: Box<RawValue>,
    low: Box<RawValue>,
    high: Box<RawValue>,
    change: Box<RawValue>,
    change_pct: Box<RawValue>,
    trades: u64,
    timestamp: String,
}

#[derive(Deserialize)]
struct KrakenOhlc {
    symbol: String,
    open: Box<RawValue>,
    high: Box<RawValue>,
    low: Box<RawValue>,
    close: Box<RawValue>,
    trades: u64,
    volume: Box<RawValue>,
    vwap: Box<RawValue>,
    interval_begin: String,
    interval: u16,
    timestamp: String,
}

#[derive(Deserialize)]
struct KrakenTrade {
    symbol: String,
    side: String,
    qty: Box<RawValue>,
    price: Box<RawValue>,
    ord_type: String,
    trade_id: u64,
    timestamp: String,
}

struct ValidatedTrade {
    symbol: &'static str,
    side: &'static str,
    quantity: String,
    price: String,
    order_type: &'static str,
    trade_id: u64,
    timestamp: String,
}

#[derive(Deserialize)]
struct KrakenBookMessage {
    symbol: String,
    #[serde(default)]
    bids: Vec<KrakenBookLevel>,
    #[serde(default)]
    asks: Vec<KrakenBookLevel>,
    checksum: u32,
    timestamp: String,
}

#[derive(Deserialize)]
struct KrakenBookLevel {
    price: Box<RawValue>,
    qty: Box<RawValue>,
}

#[derive(Debug, Clone, Copy)]
enum BookSide {
    Bid,
    Ask,
}

impl BookSide {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Bid => "bid",
            Self::Ask => "ask",
        }
    }
}

struct ValidatedBookUpdate {
    side: BookSide,
    price: Decimal,
    price_text: String,
    quantity: Decimal,
    quantity_text: String,
}

struct BookChange {
    side: BookSide,
    price: String,
    quantity: String,
    origin: &'static str,
}

#[derive(Clone)]
struct StoredLevel {
    price_text: String,
    quantity_text: String,
}

struct BookState {
    depth: u16,
    ready: bool,
    bids: BTreeMap<Decimal, StoredLevel>,
    asks: BTreeMap<Decimal, StoredLevel>,
}

impl BookState {
    fn new(depth: u16) -> Self {
        Self {
            depth,
            ready: false,
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
        }
    }

    fn clear(&mut self) {
        self.ready = false;
        self.bids.clear();
        self.asks.clear();
    }

    fn apply(&mut self, update: &ValidatedBookUpdate) {
        let levels = match update.side {
            BookSide::Bid => &mut self.bids,
            BookSide::Ask => &mut self.asks,
        };
        if update.quantity == Decimal::ZERO {
            levels.remove(&update.price);
        } else {
            levels.insert(
                update.price,
                StoredLevel {
                    price_text: update.price_text.clone(),
                    quantity_text: update.quantity_text.clone(),
                },
            );
        }
    }

    fn truncate(&mut self) -> Vec<BookChange> {
        let depth = usize::from(self.depth);
        let mut evictions = Vec::new();
        while self.asks.len() > depth {
            let Some((price, level)) = self.asks.pop_last() else {
                break;
            };
            let _ = price;
            evictions.push(BookChange {
                side: BookSide::Ask,
                price: level.price_text,
                quantity: "0".to_owned(),
                origin: "depth_eviction",
            });
        }
        while self.bids.len() > depth {
            let Some((price, level)) = self.bids.pop_first() else {
                break;
            };
            let _ = price;
            evictions.push(BookChange {
                side: BookSide::Bid,
                price: level.price_text,
                quantity: "0".to_owned(),
                origin: "depth_eviction",
            });
        }
        evictions
    }

    fn checksum(&self) -> u32 {
        let mut hasher = crc32fast::Hasher::new();
        for level in self.asks.values().take(10) {
            update_checksum(&mut hasher, &level.price_text);
            update_checksum(&mut hasher, &level.quantity_text);
        }
        for level in self.bids.values().rev().take(10) {
            update_checksum(&mut hasher, &level.price_text);
            update_checksum(&mut hasher, &level.quantity_text);
        }
        hasher.finalize()
    }
}

fn validate_trade(trade: KrakenTrade) -> Result<ValidatedTrade, AdapterError> {
    let symbol = known_symbol(&trade.symbol)?;
    let side = match trade.side.as_str() {
        "buy" => "buy",
        "sell" => "sell",
        value => {
            return Err(AdapterError::InvalidPayload(format!(
                "unknown Kraken side `{}`",
                bounded_token(value)
            )));
        }
    };
    let order_type = match trade.ord_type.as_str() {
        "limit" => "limit",
        "market" => "market",
        value => {
            return Err(AdapterError::InvalidPayload(format!(
                "unknown Kraken ord_type `{}`",
                bounded_token(value)
            )));
        }
    };
    validate_timestamp(&trade.timestamp, "timestamp")?;
    Ok(ValidatedTrade {
        symbol,
        side,
        quantity: decimal_string(&trade.qty, "qty")?,
        price: decimal_string(&trade.price, "price")?,
        order_type,
        trade_id: trade.trade_id,
        timestamp: trade.timestamp,
    })
}

fn validate_book_rows(
    bids: Vec<KrakenBookLevel>,
    asks: Vec<KrakenBookLevel>,
) -> Result<Vec<ValidatedBookUpdate>, AdapterError> {
    let capacity = bids
        .len()
        .checked_add(asks.len())
        .ok_or_else(|| AdapterError::InvalidPayload("book row count overflowed".to_owned()))?;
    let mut updates = Vec::with_capacity(capacity);
    for (side, rows) in [(BookSide::Bid, bids), (BookSide::Ask, asks)] {
        for row in rows {
            let price_text = decimal_string(&row.price, "book.price")?;
            let quantity_text = decimal_string(&row.qty, "book.qty")?;
            let price = validate_decimal(&price_text, "book.price")?;
            let quantity = validate_decimal(&quantity_text, "book.qty")?;
            if price == Decimal::ZERO {
                return Err(AdapterError::InvalidPayload(
                    "Kraken book price must be greater than zero".to_owned(),
                ));
            }
            updates.push(ValidatedBookUpdate {
                side,
                price,
                price_text,
                quantity,
                quantity_text,
            });
        }
    }
    Ok(updates)
}

fn required_data(envelope: KrakenEnvelope) -> Result<Box<RawValue>, AdapterError> {
    envelope
        .data
        .ok_or_else(|| AdapterError::InvalidPayload("Kraken message is missing data".to_owned()))
}

fn message_type(envelope: &KrakenEnvelope) -> Result<&str, AdapterError> {
    match envelope.message_type.as_deref() {
        Some(value @ ("snapshot" | "update")) => Ok(value),
        Some(value) => Err(AdapterError::InvalidPayload(format!(
            "unsupported Kraken message type `{}`",
            bounded_token(value)
        ))),
        None => Err(AdapterError::InvalidPayload(
            "Kraken data message is missing type".to_owned(),
        )),
    }
}

fn known_symbol(symbol: &str) -> Result<&'static str, AdapterError> {
    match symbol {
        BTC_SYMBOL => Ok(BTC_SYMBOL),
        USDT_SYMBOL => Ok(USDT_SYMBOL),
        value => Err(AdapterError::InvalidPayload(format!(
            "unexpected Kraken symbol `{}`",
            bounded_token(value)
        ))),
    }
}

fn trade_source_id(symbol: &str) -> &'static str {
    if symbol == BTC_SYMBOL {
        BTC_TRADE_SOURCE_ID
    } else {
        USDT_TRADE_SOURCE_ID
    }
}

fn book_source_id(symbol: &str) -> &'static str {
    if symbol == BTC_SYMBOL {
        BTC_BOOK_SOURCE_ID
    } else {
        USDT_BOOK_SOURCE_ID
    }
}

fn instrument_source_id(symbol: &str) -> &'static str {
    if symbol == BTC_SYMBOL {
        BTC_INSTRUMENT_SOURCE_ID
    } else {
        USDT_INSTRUMENT_SOURCE_ID
    }
}

fn ticker_source_id(symbol: &str) -> &'static str {
    if symbol == BTC_SYMBOL {
        BTC_TICKER_SOURCE_ID
    } else {
        USDT_TICKER_SOURCE_ID
    }
}

fn ohlc_source_id(symbol: &str) -> &'static str {
    if symbol == BTC_SYMBOL {
        BTC_OHLC_SOURCE_ID
    } else {
        USDT_OHLC_SOURCE_ID
    }
}

fn require_row_count(channel: &str, count: usize) -> Result<(), AdapterError> {
    if count > MAX_MESSAGE_ROWS {
        Err(AdapterError::InvalidPayload(format!(
            "Kraken {channel} message has {count} rows; maximum is {MAX_MESSAGE_ROWS}"
        )))
    } else {
        Ok(())
    }
}

fn validate_bounded_text(value: &str, field: &str, maximum: usize) -> Result<(), AdapterError> {
    if value.is_empty() || value.len() > maximum {
        Err(AdapterError::InvalidPayload(format!(
            "Kraken {field} must contain 1 to {maximum} bytes"
        )))
    } else {
        Ok(())
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
    use rust_decimal::Decimal;

    use crate::{AdapterError, CaptureStream, FeedAdapter, FeedNoticeCode, RawRecord, RecordKind};

    use super::{BookSide, BookState, KrakenAdapter, StoredLevel};

    fn record(ordinal: u64, payload: &str) -> RawRecord {
        RawRecord {
            schema_version: 1,
            provider: crate::Provider::KrakenSpot,
            stream: CaptureStream::KrakenCore,
            connection_epoch: 1,
            ordinal,
            received_at: "2026-08-12T12:00:01Z".to_owned(),
            capture_elapsed_ns: None,
            connect_started_elapsed_ns: None,
            kind: RecordKind::InboundText,
            payload: Some(payload.to_owned()),
        }
    }

    fn connected() -> RawRecord {
        let mut record = record(1, "");
        record.kind = RecordKind::Connected;
        record.payload = None;
        record
    }

    #[test]
    fn official_top_ten_checksum_vector_matches() {
        let mut book = BookState::new(10);
        for (price, quantity) in [
            ("45283.5", "0.10000000"),
            ("45283.4", "1.54582015"),
            ("45282.1", "0.10000000"),
            ("45281.0", "0.10000000"),
            ("45280.3", "1.54592586"),
            ("45279.0", "0.07990000"),
            ("45277.6", "0.03310103"),
            ("45277.5", "0.30000000"),
            ("45277.3", "1.54602737"),
            ("45276.6", "0.15445238"),
        ] {
            insert(&mut book, BookSide::Bid, price, quantity);
        }
        for (price, quantity) in [
            ("45285.2", "0.00100000"),
            ("45286.4", "1.54571953"),
            ("45286.6", "1.54571109"),
            ("45289.6", "1.54560911"),
            ("45290.2", "0.15890660"),
            ("45291.8", "1.54553491"),
            ("45294.7", "0.04454749"),
            ("45296.1", "0.35380000"),
            ("45297.5", "0.09945542"),
            ("45299.5", "0.18772827"),
        ] {
            insert(&mut book, BookSide::Ask, price, quantity);
        }
        assert_eq!(book.checksum(), 3_310_070_434);
    }

    #[test]
    fn empty_book_snapshot_is_rejected() {
        let mut adapter = KrakenAdapter::new("test", 10, 10).expect("adapter constructs");
        adapter.ingest(&connected()).expect("connects");
        let empty = record(
            2,
            r#"{"channel":"book","type":"snapshot","data":[{"symbol":"BTC/USD","bids":[],"asks":[],"checksum":0,"timestamp":"2026-08-12T12:00:01Z"}]}"#,
        );
        assert!(adapter.ingest(&empty).is_err());
    }

    fn adapter_after_connect() -> KrakenAdapter {
        let mut adapter = KrakenAdapter::new("test", 10, 10).expect("adapter constructs");
        adapter.ingest(&connected()).expect("connects");
        adapter
    }

    #[test]
    fn malformed_json_is_refused_rather_than_skipped() {
        let mut adapter = adapter_after_connect();
        let error = adapter
            .ingest(&record(2, r#"{"channel":"trade","type":"update","data":"#))
            .expect_err("a truncated frame must not be silently ignored");
        assert!(matches!(error, AdapterError::InvalidPayload(_)));
    }

    #[test]
    fn a_channel_whose_type_is_wrong_for_the_channel_is_refused() {
        let mut adapter = adapter_after_connect();
        // `type` is the update kind, not a channel: `book` has no `instrument`
        // update kind, and the row shape does not match either.
        let error = adapter
            .ingest(&record(
                2,
                r#"{"channel":"book","type":"instrument","data":[{"symbol":"BTC/USD","bids":[],"asks":[],"checksum":0,"timestamp":"2026-08-12T12:00:01Z"}]}"#,
            ))
            .expect_err("an unmodelled update kind on a known channel must fail closed");
        assert!(matches!(error, AdapterError::InvalidPayload(_)));
    }

    #[test]
    fn a_repeated_trade_id_is_reported_as_a_duplicate_and_emits_nothing() {
        let mut adapter = adapter_after_connect();
        let trade = r#"{"channel":"trade","type":"update","data":[{"symbol":"BTC/USD","side":"buy","price":45284.0,"qty":0.01000000,"ord_type":"limit","trade_id":1000,"timestamp":"2026-08-12T12:00:01Z"}]}"#;
        assert_eq!(
            adapter
                .ingest(&record(2, trade))
                .expect("first trade")
                .events
                .len(),
            1
        );
        let repeat = adapter
            .ingest(&record(3, trade))
            .expect("repeat is a notice");
        assert!(repeat.events.is_empty());
        assert_eq!(repeat.notices[0].code, FeedNoticeCode::Duplicate);
    }

    #[test]
    fn a_trade_id_below_the_frontier_is_reported_as_out_of_order_and_emits_nothing() {
        let mut adapter = adapter_after_connect();
        adapter
            .ingest(&record(
                2,
                r#"{"channel":"trade","type":"update","data":[{"symbol":"BTC/USD","side":"buy","price":45284.0,"qty":0.01000000,"ord_type":"limit","trade_id":1001,"timestamp":"2026-08-12T12:00:01Z"}]}"#,
            ))
            .expect("first trade");
        let stale = adapter
            .ingest(&record(
                3,
                r#"{"channel":"trade","type":"update","data":[{"symbol":"BTC/USD","side":"buy","price":45284.0,"qty":0.01000000,"ord_type":"limit","trade_id":1000,"timestamp":"2026-08-12T12:00:02Z"}]}"#,
            ))
            .expect("stale trade is a notice");
        assert!(stale.events.is_empty());
        assert_eq!(stale.notices[0].code, FeedNoticeCode::OutOfOrder);
    }

    #[test]
    fn an_ohlc_interval_other_than_one_minute_is_refused() {
        let mut adapter = adapter_after_connect();
        let error = adapter
            .ingest(&record(
                2,
                r#"{"channel":"ohlc","type":"update","timestamp":"2026-08-12T12:00:02Z","data":[{"symbol":"BTC/USD","open":63542.6,"high":63628.0,"low":63542.6,"close":63628.0,"trades":87,"volume":13.92440212,"vwap":63596.7,"interval_begin":"2026-08-12T11:59:00Z","interval":5,"timestamp":"2026-08-12T12:00:00Z"}]}"#,
            ))
            .expect_err("a five-minute candle is not the selected one-minute stream");
        assert!(matches!(error, AdapterError::InvalidPayload(_)));
    }

    #[test]
    fn an_instrument_snapshot_missing_a_selected_pair_is_refused() {
        let mut adapter = adapter_after_connect();
        let error = adapter
            .ingest(&record(
                2,
                r#"{"channel":"instrument","type":"snapshot","data":{"pairs":[{"symbol":"BTC/USD","base":"BTC","quote":"USD","status":"online","qty_increment":1E-8,"price_increment":0.1,"cost_min":0.50,"qty_min":0.00005000}]}}"#,
            ))
            .expect_err("a snapshot must carry both selected pairs");
        assert!(matches!(error, AdapterError::InvalidPayload(_)));
    }

    #[test]
    fn a_status_message_with_more_than_one_row_is_refused() {
        let mut adapter = adapter_after_connect();
        let error = adapter
            .ingest(&record(
                2,
                r#"{"channel":"status","type":"update","data":[{"version":"2.0.10","system":"online","api_version":"v2","connection_id":1},{"version":"2.0.10","system":"online","api_version":"v2","connection_id":2}]}"#,
            ))
            .expect_err("status is one row per message");
        assert!(matches!(error, AdapterError::InvalidPayload(_)));
    }

    #[test]
    fn a_channel_less_non_subscribe_frame_is_a_counted_notice_not_an_error() {
        // S1 shape: a frame with no `channel` and a method other than
        // `subscribe` (here a keepalive) must be a counted notice, not a
        // lane-killing error.
        let mut adapter = adapter_after_connect();
        let outcome = adapter
            .ingest(&record(2, r#"{"method":"pong"}"#))
            .expect("a channel-less control frame must be retained, not refused");
        assert!(outcome.events.is_empty());
        assert_eq!(outcome.notices.len(), 1);
        assert_eq!(outcome.notices[0].code, FeedNoticeCode::UnsupportedMessage);
    }

    #[test]
    fn a_frame_on_an_unsubscribed_channel_is_a_counted_notice_not_an_error() {
        let mut adapter = adapter_after_connect();
        let outcome = adapter
            .ingest(&record(2, r#"{"channel":"level3","data":[]}"#))
            .expect("an unsubscribed channel must be retained, not refused");
        assert!(outcome.events.is_empty());
        assert_eq!(outcome.notices.len(), 1);
        assert_eq!(outcome.notices[0].code, FeedNoticeCode::UnsupportedMessage);
    }

    fn insert(book: &mut BookState, side: BookSide, price: &str, quantity: &str) {
        let levels = match side {
            BookSide::Bid => &mut book.bids,
            BookSide::Ask => &mut book.asks,
        };
        levels.insert(
            price.parse::<Decimal>().expect("valid test price"),
            StoredLevel {
                price_text: price.to_owned(),
                quantity_text: quantity.to_owned(),
            },
        );
    }
}
