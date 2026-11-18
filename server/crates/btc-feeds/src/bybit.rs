//! Bybit adapter for spot BTCUSDT (trades, level-two book, BBO), the USDT
//! perpetual (all-liquidations, ticker, `orderbook.50`, trades) and the two
//! linear REST poll identities (open interest, funding history). Nine A4 source
//! definitions cover these; the streams are rights-gated (R7) — the capture path
//! refuses to start unless `SIGNALS_ENABLED_VENUES` lists `bybit`.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::value::RawValue;
use serde_json::{Value, json};
use signals_core::SourceEvent;

use crate::adapter::{GuardAction, RecordGuard, bounded_token, validate_publisher_epoch};
use crate::decimal::{validate_decimal, validate_signed_decimal};
use crate::event_window::millis_to_rfc3339;
use crate::{
    AdapterError, FeedAdapter, FeedNotice, FeedNoticeCode, IngestOutcome, Provider, RawRecord,
    RecoveryAction,
};

const INSTRUMENT: &str = "BTCUSDT";

const TRADES_SOURCE_ID: &str = "bybit.spot.btcusdt.trades";
const TRADES_SOURCE_VERSION: &str = "1.0.0";
const BOOK_LEVELS_SOURCE_ID: &str = "bybit.spot.btcusdt.book-levels";
const BOOK_LEVELS_SOURCE_VERSION: &str = "1.0.0";
const BBO_LEVELS_SOURCE_ID: &str = "bybit.spot.btcusdt.bbo-levels";
const BBO_LEVELS_SOURCE_VERSION: &str = "1.0.0";
const LIQUIDATIONS_SOURCE_ID: &str = "bybit.linear.btcusdt.liquidations";
const LIQUIDATIONS_SOURCE_VERSION: &str = "1.0.0";
const LINEAR_TRADES_SOURCE_ID: &str = "bybit.linear.btcusdt.trades";
const LINEAR_TRADES_SOURCE_VERSION: &str = "1.0.0";
const LINEAR_BOOK_LEVELS_SOURCE_ID: &str = "bybit.linear.btcusdt.book-levels";
const LINEAR_BOOK_LEVELS_SOURCE_VERSION: &str = "1.0.0";
const TICKERS_SOURCE_ID: &str = "bybit.linear.btcusdt.tickers";
const TICKERS_SOURCE_VERSION: &str = "1.0.0";

// Subscription topics compiled into the capture binary (single source of truth
// shared with capture.rs). Spot carries the public trade tape and two book
// depths; the linear stream carries the aggregated liquidation feed. The `.200`
// book is the deep book-levels source; the `.1` book is the best-bid/offer
// source. Every data message names one of these topics and the adapter
// dispatches by name.
pub(crate) const PUBLIC_TRADE_TOPIC: &str = "publicTrade.BTCUSDT";
pub(crate) const ORDERBOOK_200_TOPIC: &str = "orderbook.200.BTCUSDT";
pub(crate) const ORDERBOOK_1_TOPIC: &str = "orderbook.1.BTCUSDT";
pub(crate) const ALL_LIQUIDATION_TOPIC: &str = "allLiquidation.BTCUSDT";
// Linear-only topics. `tickers.BTCUSDT` is the folded mark/index/funding/open-
// interest state; `orderbook.50.BTCUSDT` is the perp's 50-level book. The linear
// tape reuses the shared `publicTrade.BTCUSDT` topic string, dispatched by market.
pub(crate) const TICKERS_TOPIC: &str = "tickers.BTCUSDT";
pub(crate) const LINEAR_ORDERBOOK_50_TOPIC: &str = "orderbook.50.BTCUSDT";

pub(crate) const SPOT_TOPICS: [&str; 3] =
    [PUBLIC_TRADE_TOPIC, ORDERBOOK_200_TOPIC, ORDERBOOK_1_TOPIC];
pub(crate) const LINEAR_TOPICS: [&str; 4] = [
    ALL_LIQUIDATION_TOPIC,
    TICKERS_TOPIC,
    LINEAR_ORDERBOOK_50_TOPIC,
    PUBLIC_TRADE_TOPIC,
];

// Depth pushes are bounded by the deepest subscribed book (200 levels per side);
// the array-shaped trade and liquidation pushes are bounded generously above any
// observed batch. A message beyond these bounds fails closed rather than
// allocating without limit.
const MAX_DEPTH_ROWS: usize = 4_096;
const MAX_ARRAY_ROWS: usize = 8_192;

/// Per-topic book cursor. `Some(u)` is the last applied Bybit update id for the
/// topic; `None` means no snapshot has seeded the book yet, so the next delta is
/// premature and a snapshot is required first.
type BookCursor = Option<u64>;

/// Deterministic Bybit v5 parser and ordering state for one public stream. One
/// adapter serves exactly one stream's compiled topic set: the spot topics
/// (trades, `orderbook.200`, `orderbook.1`) or the linear liquidation topic. A
/// message naming a topic outside this instance's set is refused, never
/// normalized under a foreign source id.
pub struct BybitAdapter {
    publisher_epoch: String,
    guard: RecordGuard,
    market: BybitMarket,
    topics: &'static [&'static str],
    next_trade_sequence: u64,
    next_book_levels_sequence: u64,
    next_bbo_levels_sequence: u64,
    next_liquidation_sequence: u64,
    next_linear_trade_sequence: u64,
    next_linear_book_levels_sequence: u64,
    next_tickers_sequence: u64,
    last_trade_seq: Option<u64>,
    book_200_cursor: BookCursor,
    book_1_cursor: BookCursor,
    book_50_cursor: BookCursor,
    last_ticker_cs: Option<u64>,
    ticker_state: Option<Box<LinearTickerState>>,
}

/// Which Bybit product line an adapter is bound to.
///
/// Bybit's spot and USDT-perpetual (linear) sockets carry different topic sets
/// and different source identities, so the binding is fixed at construction.
/// This is the only Bybit market split the crate captures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BybitMarket {
    /// Bybit spot BTCUSDT: public trades, level-two book and BBO.
    Spot,
    /// Bybit USDT-perpetual BTCUSDT: all-liquidations only.
    Linear,
}

impl From<BybitMarket> for Provider {
    fn from(value: BybitMarket) -> Self {
        match value {
            BybitMarket::Spot => Self::BybitSpot,
            BybitMarket::Linear => Self::BybitLinear,
        }
    }
}

impl BybitAdapter {
    /// Construct a Bybit adapter for one product line.
    ///
    /// `publisher_epoch` is copied verbatim into every emitted event; it must be
    /// non-blank and at most 128 bytes, or the call returns
    /// [`AdapterError::InvalidRecord`]. `market` selects the topic set and the
    /// source identities the adapter will emit, and must match the stream the
    /// records come from — a record whose provider disagrees is refused at
    /// [`FeedAdapter::ingest`].
    pub fn new(
        publisher_epoch: impl Into<String>,
        market: BybitMarket,
    ) -> Result<Self, AdapterError> {
        let publisher_epoch = publisher_epoch.into();
        validate_publisher_epoch(&publisher_epoch)?;
        let topics: &'static [&'static str] = match market {
            BybitMarket::Spot => &SPOT_TOPICS,
            BybitMarket::Linear => &LINEAR_TOPICS,
        };
        Ok(Self {
            publisher_epoch,
            guard: RecordGuard::new(Provider::from(market)),
            market,
            topics,
            next_trade_sequence: 0,
            next_book_levels_sequence: 0,
            next_bbo_levels_sequence: 0,
            next_liquidation_sequence: 0,
            next_linear_trade_sequence: 0,
            next_linear_book_levels_sequence: 0,
            next_tickers_sequence: 0,
            last_trade_seq: None,
            book_200_cursor: None,
            book_1_cursor: None,
            book_50_cursor: None,
            last_ticker_cs: None,
            ticker_state: None,
        })
    }

    fn reset_stream_state(&mut self) {
        // A fresh connection restarts every ordering cursor. Bybit's cross
        // sequence and per-book update ids are scoped to the live connection and
        // begin anew after a reconnect, so a lower value on the first
        // post-reconnect message is a new epoch, not a regression. The folded
        // ticker state is voided too: its next post-reconnect message must be a
        // fresh snapshot, so a delta arriving first is premature.
        self.last_trade_seq = None;
        self.book_200_cursor = None;
        self.book_1_cursor = None;
        self.book_50_cursor = None;
        self.last_ticker_cs = None;
        self.ticker_state = None;
    }

    fn ingest_payload(
        &mut self,
        record: &RawRecord,
        payload: &str,
    ) -> Result<IngestOutcome, AdapterError> {
        let envelope: BybitEnvelope = serde_json::from_str(payload).map_err(payload_error)?;
        // Operation responses (`subscribe`/`ping` acknowledgements) carry an `op`
        // field and no market data: they are operational state, never events.
        if let Some(op) = envelope.op.as_deref() {
            return Ok(op_response(
                op,
                envelope.success,
                envelope.ret_msg.as_deref(),
                envelope.args.as_deref(),
                self.topics,
            ));
        }
        let (Some(topic), Some(data)) = (envelope.topic.as_deref(), envelope.data.as_deref())
        else {
            return Ok(IngestOutcome::notice(FeedNotice::info(
                FeedNoticeCode::UnsupportedMessage,
                "retained unrecognized Bybit message without normalized output",
            )));
        };
        // A topic outside this stream's compiled set (e.g. `publicTrade` on the
        // linear adapter) is refused here, so it can never be normalized under a
        // source id that belongs to the other stream.
        if !self.topics.contains(&topic) {
            return Ok(IngestOutcome::notice(FeedNotice::info(
                FeedNoticeCode::UnsupportedMessage,
                format!(
                    "retained unsupported Bybit topic `{}` for this stream without normalized output",
                    bounded_token(topic)
                ),
            )));
        }
        match topic {
            // `publicTrade.BTCUSDT` is shared by both sockets; the market decides
            // the source identity and whether the futures-only `L` tick direction
            // is required and projected.
            PUBLIC_TRADE_TOPIC => match self.market {
                BybitMarket::Spot => {
                    self.public_trade(record, TRADES_SOURCE_ID, TRADES_SOURCE_VERSION, false, data)
                }
                BybitMarket::Linear => self.public_trade(
                    record,
                    LINEAR_TRADES_SOURCE_ID,
                    LINEAR_TRADES_SOURCE_VERSION,
                    true,
                    data,
                ),
            },
            ORDERBOOK_200_TOPIC => self.orderbook(
                record,
                BOOK_LEVELS_SOURCE_ID,
                BOOK_LEVELS_SOURCE_VERSION,
                BookTopic::Deep,
                envelope.message_type.as_deref(),
                envelope.ts,
                envelope.cts,
                data,
            ),
            ORDERBOOK_1_TOPIC => self.orderbook(
                record,
                BBO_LEVELS_SOURCE_ID,
                BBO_LEVELS_SOURCE_VERSION,
                BookTopic::Bbo,
                envelope.message_type.as_deref(),
                envelope.ts,
                envelope.cts,
                data,
            ),
            LINEAR_ORDERBOOK_50_TOPIC => self.orderbook(
                record,
                LINEAR_BOOK_LEVELS_SOURCE_ID,
                LINEAR_BOOK_LEVELS_SOURCE_VERSION,
                BookTopic::LinearDeep,
                envelope.message_type.as_deref(),
                envelope.ts,
                envelope.cts,
                data,
            ),
            TICKERS_TOPIC => self.ticker(
                record,
                envelope.message_type.as_deref(),
                envelope.ts,
                envelope.cs,
                data,
            ),
            ALL_LIQUIDATION_TOPIC => self.liquidation(record, data),
            other => Ok(IngestOutcome::notice(FeedNotice::info(
                FeedNoticeCode::UnsupportedMessage,
                format!(
                    "retained unsupported Bybit topic `{}` without normalized output",
                    bounded_token(other)
                ),
            ))),
        }
    }

    fn public_trade(
        &mut self,
        record: &RawRecord,
        source_id: &'static str,
        source_version: &'static str,
        expect_tick_direction: bool,
        data: &RawValue,
    ) -> Result<IngestOutcome, AdapterError> {
        let trades: Vec<BybitPublicTrade> =
            serde_json::from_str(data.get()).map_err(payload_error)?;
        if trades.len() > MAX_ARRAY_ROWS {
            return Err(AdapterError::InvalidPayload(format!(
                "Bybit publicTrade message has {} entries; maximum is {MAX_ARRAY_ROWS}",
                trades.len()
            )));
        }
        if trades.is_empty() {
            // An empty trade batch is a degenerate venue state, not a parse
            // failure: note it and emit nothing. The cross-sequence cursor is
            // left untouched.
            return Ok(IngestOutcome::notice(FeedNotice::info(
                FeedNoticeCode::StateInconsistent,
                "Bybit publicTrade message carried no trades; skipped without normalized output",
            )));
        }
        // Validate every entry (symbol, side, decimals, event time) BEFORE the
        // ordering check, so a malformed trade fails closed even when its `seq`
        // would place the message out of order.
        let mut notices = Vec::new();
        let mut prepared = Vec::with_capacity(trades.len());
        let mut min_seq = u64::MAX;
        let mut max_seq = 0_u64;
        for trade in &trades {
            require_symbol(&trade.symbol)?;
            let aggressor_side = taker_side(&trade.side)?;
            validate_decimal(&trade.price, "price")?;
            validate_decimal(&trade.size, "size")?;
            // `L` (tick direction) is a futures-only key: the linear tape requires
            // it and projects it as `tick_direction`; the spot tape never carries
            // or projects it. A linear fill without `L` is an anomalous entry (a
            // block/RPI print, a venue field rename), not a parse failure of the
            // whole stream: skip THIS fill with a notice and keep ingesting,
            // rather than aborting replay/live at this record and dropping every
            // later trade, book, ticker and liquidation event in the file.
            let tick_direction = if expect_tick_direction {
                match trade.tick_direction.as_deref() {
                    Some(raw) => Some(tick_direction(raw)?),
                    None => {
                        notices.push(FeedNotice::info(
                            FeedNoticeCode::StateInconsistent,
                            "Bybit linear publicTrade entry is missing the required L tick direction; skipped without normalized output",
                        ));
                        continue;
                    }
                }
            } else {
                None
            };
            let event_time = millis_to_rfc3339(trade.trade_time, "T")?;
            min_seq = min_seq.min(trade.seq);
            max_seq = max_seq.max(trade.seq);
            prepared.push((trade, aggressor_side, tick_direction, event_time));
        }

        // Ordering guard: `seq` is a relative-ordering token only (no gap
        // semantics documented). A message carrying a `seq` below the running
        // maximum is delivered out of order and noted, but its trades are still
        // valid and are always emitted -- valid data is never discarded. Equal
        // `seq` is legal: one match event fans out to several fills that share a
        // sequence.
        if !prepared.is_empty() {
            if let Some(previous) = self.last_trade_seq {
                if min_seq < previous {
                    notices.push(FeedNotice::info(
                        FeedNoticeCode::OutOfOrder,
                        format!(
                            "out-of-order Bybit trade cross sequence {min_seq} after {previous}; trades still emitted"
                        ),
                    ));
                }
                self.last_trade_seq = Some(previous.max(max_seq));
            } else {
                self.last_trade_seq = Some(max_seq);
            }
        }

        let mut events = Vec::with_capacity(prepared.len());
        for (trade, aggressor_side, tick_direction, event_time) in prepared {
            // `i` is the provider trade id: an opaque string (observed as a long
            // numeric string, not necessarily a UUID). Its contiguity is not
            // documented, so no gap or continuity logic is applied to it.
            let mut values = BTreeMap::from([
                ("instrument".to_owned(), json!(INSTRUMENT)),
                ("trade_id".to_owned(), json!(trade.trade_id)),
                ("price".to_owned(), json!(trade.price)),
                ("size".to_owned(), json!(trade.size)),
                ("aggressor_side".to_owned(), json!(aggressor_side)),
                ("block_trade".to_owned(), json!(trade.block_trade)),
                // `RPI`: retail-price-improvement order flag. Absent entries are
                // recorded as false.
                ("rpi".to_owned(), json!(trade.rpi.unwrap_or(false))),
                ("cross_sequence".to_owned(), json!(trade.seq)),
                ("event_time_basis".to_owned(), json!("source")),
                ("event_time".to_owned(), json!(event_time)),
            ]);
            if let Some(tick_direction) = tick_direction {
                values.insert("tick_direction".to_owned(), json!(tick_direction));
            }
            events.push(self.source_event(
                source_id,
                source_version,
                event_time.clone(),
                record.received_at.clone(),
                values,
            )?);
        }
        Ok(IngestOutcome { events, notices })
    }

    #[allow(clippy::too_many_arguments)]
    fn orderbook(
        &mut self,
        record: &RawRecord,
        source_id: &'static str,
        source_version: &'static str,
        topic: BookTopic,
        message_type: Option<&str>,
        ts: Option<i64>,
        cts: Option<i64>,
        data: &RawValue,
    ) -> Result<IngestOutcome, AdapterError> {
        let book: BybitOrderbook = serde_json::from_str(data.get()).map_err(payload_error)?;
        require_symbol(&book.symbol)?;
        let row_count = book
            .bids
            .len()
            .checked_add(book.asks.len())
            .ok_or_else(|| {
                AdapterError::InvalidPayload("Bybit orderbook row count overflowed".to_owned())
            })?;
        if row_count > MAX_DEPTH_ROWS {
            return Err(AdapterError::InvalidPayload(format!(
                "Bybit orderbook message has {row_count} levels; maximum is {MAX_DEPTH_ROWS}"
            )));
        }
        // Validate shape, decimals, and event time BEFORE any cursor
        // short-circuit. A size of "0" is a level deletion and is emitted with
        // that size; book reconstruction from deletions is the adopting
        // consumer's job, mirroring the other book-levels sources.
        let mut rows = Vec::with_capacity(row_count);
        for level in &book.bids {
            validate_decimal(&level[0], "b.price")?;
            validate_decimal(&level[1], "b.size")?;
            rows.push(("bid", level[0].clone(), level[1].clone()));
        }
        for level in &book.asks {
            validate_decimal(&level[0], "a.price")?;
            validate_decimal(&level[1], "a.size")?;
            rows.push(("ask", level[0].clone(), level[1].clone()));
        }
        // Event time prefers `cts` (the cross/matching-engine timestamp) and
        // labels it "source". When `cts` is absent the record's local receipt
        // time is used, labeled "receipt" to say so honestly; the envelope `ts`
        // (venue send time) is not our receipt time and is not used in that role.
        // A message carrying neither `cts` nor `ts` is malformed.
        let (event_time, event_time_basis) = match cts {
            Some(cts) => (millis_to_rfc3339(cts, "cts")?, "source"),
            None => {
                if ts.is_none() {
                    return Err(AdapterError::InvalidPayload(
                        "Bybit orderbook message carries neither cts nor ts".to_owned(),
                    ));
                }
                (record.received_at.clone(), "receipt")
            }
        };

        // The type is validated before the empty-message short-circuit so a
        // malformed frame that happens to carry no levels still fails closed.
        let book_kind = match message_type {
            Some(kind @ ("snapshot" | "delta")) => kind,
            other => {
                return Err(AdapterError::InvalidPayload(format!(
                    "Bybit orderbook type must be snapshot or delta, received `{}`",
                    bounded_token(other.unwrap_or("<missing>"))
                )));
            }
        };
        if row_count == 0 {
            // An empty book message is a degenerate venue state, not a parse
            // failure: note it and emit nothing. Critically, the cursor is NOT
            // seeded or advanced from an empty message, so a later populated
            // snapshot still seeds the book.
            return Ok(IngestOutcome::notice(FeedNotice::info(
                FeedNoticeCode::StateInconsistent,
                "Bybit orderbook message carried no levels; skipped without normalized output",
            )));
        }

        let cursor = self.book_cursor(topic);
        let mut restart_notices = Vec::new();

        match book_kind {
            "snapshot" => {
                // A snapshot is the fresh seed: it ALWAYS resets this topic's
                // cursor and applies its levels, regardless of `u`. This covers
                // the initial seed, a mid-stream re-seed, and the same-`u`
                // snapshot repeats the venue sends on the level-1 book. A `u == 1`
                // snapshot additionally signals that a book-service restart
                // produced this seed, so a recovery notice accompanies the levels.
                *cursor = Some(book.u);
                if book.u == 1 {
                    restart_notices.push(FeedNotice::recovery(
                        FeedNoticeCode::StateInconsistent,
                        "Bybit orderbook update id 1 signals a book-service restart; the prior local book is void and reseeded from this snapshot",
                        RecoveryAction::Reconnect,
                    ));
                }
            }
            _ => {
                if book.u == 1 {
                    // A delta is never the restart seed; a `u == 1` delta is
                    // malformed relative to the contract. Discard the local book
                    // and await a fresh snapshot rather than applying it.
                    *cursor = None;
                    return Ok(IngestOutcome::notice(FeedNotice::recovery(
                        FeedNoticeCode::AwaitingSnapshot,
                        "Bybit orderbook delta carried update id 1; awaiting a fresh snapshot",
                        RecoveryAction::Reconnect,
                    )));
                }
                match *cursor {
                    None => {
                        // A delta with no seeding snapshot is transient, never
                        // terminal: note it and await a snapshot without touching
                        // state.
                        return Ok(IngestOutcome::notice(FeedNotice::recovery(
                            FeedNoticeCode::AwaitingSnapshot,
                            "Bybit orderbook delta arrived before a snapshot; awaiting a fresh snapshot",
                            RecoveryAction::Reconnect,
                        )));
                    }
                    Some(previous) if book.u == previous => {
                        // A repeated update id is a duplicate carrying no newer
                        // state.
                        return Ok(IngestOutcome::notice(FeedNotice::info(
                            FeedNoticeCode::Duplicate,
                            format!(
                                "Bybit orderbook delta update id {} repeats {previous}; skipped",
                                book.u
                            ),
                        )));
                    }
                    Some(previous) if book.u < previous => {
                        // A lower update id is out of order and carries no newer
                        // state.
                        return Ok(IngestOutcome::notice(FeedNotice::info(
                            FeedNoticeCode::OutOfOrder,
                            format!(
                                "out-of-order Bybit orderbook delta update id {} after {previous}; skipped",
                                book.u
                            ),
                        )));
                    }
                    Some(previous) => {
                        // The update id advances. F4: a forward jump past the next
                        // id is a resync condition, not a benign advance. `u` is
                        // contiguous in every observed hour (bybit-spot-r1's 36,952
                        // orderbook.200 frames advance by exactly one, zero
                        // exceptions), so a gap means a dropped delta and a local
                        // book that has silently diverged from the venue's. Drop
                        // the book and await a fresh snapshot rather than applying
                        // the jumped delta and publishing a diverged book as ready.
                        let expected = previous.saturating_add(1);
                        if book.u > expected {
                            *cursor = None;
                            return Ok(IngestOutcome::notice(FeedNotice::recovery(
                                FeedNoticeCode::Gap,
                                format!(
                                    "Bybit orderbook delta update id {} jumped past the expected {expected} (gap after {previous}); awaiting a fresh snapshot",
                                    book.u
                                ),
                                RecoveryAction::Reconnect,
                            )));
                        }
                        *cursor = Some(book.u);
                    }
                }
            }
        }

        let update_kind = book_kind;
        let mut outcome = self.book_events(
            record,
            source_id,
            source_version,
            update_kind,
            book.u,
            book.seq,
            event_time,
            event_time_basis,
            rows,
        )?;
        outcome.notices.append(&mut restart_notices);
        Ok(outcome)
    }

    #[allow(clippy::too_many_arguments)]
    fn book_events(
        &mut self,
        record: &RawRecord,
        source_id: &'static str,
        source_version: &'static str,
        update_kind: &str,
        update_id: u64,
        cross_sequence: u64,
        event_time: String,
        event_time_basis: &str,
        rows: Vec<(&str, String, String)>,
    ) -> Result<IngestOutcome, AdapterError> {
        let update_count = u32::try_from(rows.len()).map_err(|_| {
            AdapterError::InvalidPayload("Bybit orderbook update_count exceeds u32".to_owned())
        })?;
        // One capture message's levels are bound together by `update_group`;
        // `update_count` states the complete set so a downstream consumer can
        // reconstruct the message boundary during book application.
        let update_group = format!(
            "{}:{}:{}",
            record.stream.as_str(),
            record.connection_epoch,
            record.ordinal
        );
        let mut events = Vec::with_capacity(rows.len());
        for (index, (side, price, size)) in rows.into_iter().enumerate() {
            let update_index = u32::try_from(index).map_err(|_| {
                AdapterError::InvalidPayload("Bybit orderbook update_index exceeds u32".to_owned())
            })?;
            let values = BTreeMap::from([
                ("instrument".to_owned(), json!(INSTRUMENT)),
                ("update_group".to_owned(), json!(update_group)),
                ("update_index".to_owned(), json!(update_index)),
                ("update_count".to_owned(), json!(update_count)),
                ("update_kind".to_owned(), json!(update_kind)),
                ("side".to_owned(), json!(side)),
                ("price".to_owned(), json!(price)),
                ("size".to_owned(), json!(size)),
                ("update_id".to_owned(), json!(update_id)),
                ("cross_sequence".to_owned(), json!(cross_sequence)),
                ("event_time_basis".to_owned(), json!(event_time_basis)),
                ("event_time".to_owned(), json!(event_time)),
            ]);
            events.push(self.source_event(
                source_id,
                source_version,
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

    fn liquidation(
        &mut self,
        record: &RawRecord,
        data: &RawValue,
    ) -> Result<IngestOutcome, AdapterError> {
        let liquidations: Vec<BybitLiquidation> =
            serde_json::from_str(data.get()).map_err(payload_error)?;
        if liquidations.len() > MAX_ARRAY_ROWS {
            return Err(AdapterError::InvalidPayload(format!(
                "Bybit allLiquidation message has {} entries; maximum is {MAX_ARRAY_ROWS}",
                liquidations.len()
            )));
        }
        if liquidations.is_empty() {
            // An empty liquidation batch is a degenerate venue state, not a parse
            // failure: note it and emit nothing.
            return Ok(IngestOutcome::notice(FeedNotice::info(
                FeedNoticeCode::StateInconsistent,
                "Bybit allLiquidation message carried no liquidations; skipped without normalized output",
            )));
        }
        // The liquidation feed documents no cross-message ordering guarantee, so
        // there is no ordering guard: each entry is validated and emitted.
        let mut events = Vec::with_capacity(liquidations.len());
        for liquidation in &liquidations {
            require_symbol(&liquidation.symbol)?;
            let side = taker_side(&liquidation.side)?;
            validate_decimal(&liquidation.size, "v")?;
            // `p` is the bankruptcy price of the liquidated position, not the
            // last-traded price; it is named accordingly.
            validate_decimal(&liquidation.bankruptcy_price, "p")?;
            let event_time = millis_to_rfc3339(liquidation.event_time, "T")?;
            let values = BTreeMap::from([
                ("instrument".to_owned(), json!(INSTRUMENT)),
                ("side".to_owned(), json!(side)),
                ("size".to_owned(), json!(liquidation.size)),
                (
                    "bankruptcy_price".to_owned(),
                    json!(liquidation.bankruptcy_price),
                ),
                ("event_time_basis".to_owned(), json!("source")),
                ("event_time".to_owned(), json!(event_time)),
            ]);
            events.push(self.source_event(
                LIQUIDATIONS_SOURCE_ID,
                LIQUIDATIONS_SOURCE_VERSION,
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

    fn ticker(
        &mut self,
        record: &RawRecord,
        message_type: Option<&str>,
        ts: Option<i64>,
        cs: Option<u64>,
        data: &RawValue,
    ) -> Result<IngestOutcome, AdapterError> {
        let ticker: BybitLinearTicker = serde_json::from_str(data.get()).map_err(payload_error)?;
        // event_time is the envelope ts; a ticker without it is malformed. cs is
        // the declared ordering key and must be present on every ticker frame.
        let ts = ts.ok_or_else(|| {
            AdapterError::InvalidPayload("Bybit ticker message carries no ts".to_owned())
        })?;
        let event_time = millis_to_rfc3339(ts, "ts")?;
        let cs = cs.ok_or_else(|| {
            AdapterError::InvalidPayload(
                "Bybit ticker message carries no cs cross sequence".to_owned(),
            )
        })?;
        let kind = match message_type {
            Some(kind @ ("snapshot" | "delta")) => kind,
            other => {
                return Err(AdapterError::InvalidPayload(format!(
                    "Bybit ticker type must be snapshot or delta, received `{}`",
                    bounded_token(other.unwrap_or("<missing>"))
                )));
            }
        };

        match kind {
            "snapshot" => {
                // A snapshot missing any projected key is not a snapshot: refuse
                // it rather than folding a partial state. It replaces the folded
                // state wholesale and reseeds the cross-sequence frontier.
                let state = LinearTickerState::from_snapshot(&ticker)?;
                self.ticker_state = Some(Box::new(state));
                self.last_ticker_cs = Some(cs);
            }
            _ => {
                if self.ticker_state.is_none() {
                    return Ok(IngestOutcome::notice(FeedNotice::recovery(
                        FeedNoticeCode::AwaitingSnapshot,
                        "Bybit ticker delta arrived before a snapshot; awaiting a fresh snapshot",
                        RecoveryAction::Reconnect,
                    )));
                }
                let last = self
                    .last_ticker_cs
                    .expect("ticker cross sequence is set with the state");
                if cs == last {
                    return Ok(IngestOutcome::notice(FeedNotice::info(
                        FeedNoticeCode::Duplicate,
                        format!("Bybit ticker cross sequence {cs} repeats {last}; skipped"),
                    )));
                }
                if cs < last {
                    return Ok(IngestOutcome::notice(FeedNotice::info(
                        FeedNoticeCode::OutOfOrder,
                        format!(
                            "out-of-order Bybit ticker cross sequence {cs} after {last}; skipped"
                        ),
                    )));
                }
                // cs advances: merge only the present keys, retain the rest.
                let state = self
                    .ticker_state
                    .as_mut()
                    .expect("ticker state present after the None check");
                state.merge_delta(&ticker)?;
                self.last_ticker_cs = Some(cs);
            }
        }

        let state = self
            .ticker_state
            .as_ref()
            .expect("ticker state is present after a snapshot or an applied delta");
        let values = state.row(kind, cs, &event_time);
        let event = self.source_event(
            TICKERS_SOURCE_ID,
            TICKERS_SOURCE_VERSION,
            event_time.clone(),
            record.received_at.clone(),
            values,
        )?;
        Ok(IngestOutcome {
            events: vec![event],
            notices: Vec::new(),
        })
    }

    fn book_cursor(&mut self, topic: BookTopic) -> &mut BookCursor {
        // The two book depths keep fully separate cursors: an `orderbook.1`
        // update never advances or reseeds the `orderbook.200` book, and the
        // reverse.
        match topic {
            BookTopic::Deep => &mut self.book_200_cursor,
            BookTopic::Bbo => &mut self.book_1_cursor,
            BookTopic::LinearDeep => &mut self.book_50_cursor,
        }
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
            TRADES_SOURCE_ID => &mut self.next_trade_sequence,
            BOOK_LEVELS_SOURCE_ID => &mut self.next_book_levels_sequence,
            BBO_LEVELS_SOURCE_ID => &mut self.next_bbo_levels_sequence,
            LIQUIDATIONS_SOURCE_ID => &mut self.next_liquidation_sequence,
            LINEAR_TRADES_SOURCE_ID => &mut self.next_linear_trade_sequence,
            LINEAR_BOOK_LEVELS_SOURCE_ID => &mut self.next_linear_book_levels_sequence,
            TICKERS_SOURCE_ID => &mut self.next_tickers_sequence,
            _ => {
                return Err(AdapterError::InvalidRecord(
                    "unknown Bybit source identity".to_owned(),
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

impl FeedAdapter for BybitAdapter {
    fn ingest(&mut self, record: &RawRecord) -> Result<IngestOutcome, AdapterError> {
        match self.guard.observe(record)? {
            GuardAction::Configuration(_) => Err(AdapterError::InvalidRecord(
                "Bybit captures do not accept configuration records".to_owned(),
            )),
            GuardAction::Connected => {
                self.reset_stream_state();
                Ok(IngestOutcome::notice(FeedNotice::info(
                    FeedNoticeCode::Connected,
                    "Bybit WebSocket connected; books await fresh snapshots",
                )))
            }
            GuardAction::Disconnected => {
                self.reset_stream_state();
                Ok(IngestOutcome::notice(FeedNotice::recovery(
                    FeedNoticeCode::Disconnected,
                    "Bybit WebSocket disconnected",
                    RecoveryAction::Reconnect,
                )))
            }
            GuardAction::Outbound => Ok(IngestOutcome::default()),
            GuardAction::Failed(_) => Err(AdapterError::InvalidRecord(
                "Bybit captures do not accept failure records".to_owned(),
            )),
            GuardAction::Inbound(payload) => self.ingest_payload(record, payload),
        }
    }
}

/// Which of the subscribed book depths a message belongs to. The three depths
/// (`orderbook.200` and `orderbook.1` on spot, `orderbook.50` on the perp) keep
/// fully independent cursors.
#[derive(Debug, Clone, Copy)]
enum BookTopic {
    Deep,
    Bbo,
    LinearDeep,
}

/// Classify a Bybit operation response. A `success:false` acknowledgement is a
/// rejection notice; a `ping`/`pong` acknowledgement is a heartbeat; a subscribe
/// acknowledgement is accepted, and a partial topic echo is reported without
/// tearing down the connection.
fn op_response(
    op: &str,
    success: Option<bool>,
    ret_msg: Option<&str>,
    args: Option<&[String]>,
    expected_topics: &[&str],
) -> IngestOutcome {
    // A rejected operation is a failed connection, not a degraded-but-live lane
    // (spec §4): recover by reconnecting rather than continuing on a partial
    // subscription.
    if success == Some(false) {
        return IngestOutcome::notice(FeedNotice::recovery(
            FeedNoticeCode::SubscriptionRejected,
            bounded_detail(
                "Bybit operation rejected",
                ret_msg.unwrap_or("no ret_msg provided"),
            ),
            RecoveryAction::Reconnect,
        ));
    }
    // The heartbeat ack is `op:ping` on spot but `op:pong` on linear/inverse
    // (the latter carrying `args:["<ts ms>"]`), so both are heartbeats — never a
    // subscribe whose args must echo the topic set.
    if op == "ping" || op == "pong" {
        return IngestOutcome::notice(FeedNotice::info(
            FeedNoticeCode::Heartbeat,
            "Bybit heartbeat acknowledged",
        ));
    }
    // The topic echo lives only on a subscribe ack, and only there is a subset
    // even potentially meaningful. Bybit does NOT document that a subscribe ack
    // echoes ALL accepted topics (echo-all), so a subset echo is not proof that
    // a topic was rejected — one subscribe frame may be acked in pieces, and an
    // ack may omit the echo entirely (the observed shape). A genuinely dropped
    // topic delivers no data and is caught by the connection's silence/read-idle
    // bound, which tears the socket down and reconnects. So a partial echo is
    // reported as informational and the live subscription is kept, rather than
    // forcing a reconnect loop on every heartbeat or per-topic ack.
    if op == "subscribe" {
        if let Some(accepted) = args {
            let missing: Vec<&str> = expected_topics
                .iter()
                .copied()
                .filter(|topic| !accepted.iter().any(|arg| arg == topic))
                .collect();
            if !missing.is_empty() {
                return IngestOutcome::notice(FeedNotice::info(
                    FeedNoticeCode::SubscriptionAccepted,
                    bounded_detail(
                        "Bybit subscribe ack echoed a subset; awaiting the remaining topics within the connection silence bound",
                        &missing.join(","),
                    ),
                ));
            }
        }
        return IngestOutcome::notice(FeedNotice::info(
            FeedNoticeCode::SubscriptionAccepted,
            "Bybit subscription acknowledged",
        ));
    }
    // Any other acknowledged operation carries no market data and no topic
    // contract to check: accept it without inspecting args.
    IngestOutcome::notice(FeedNotice::info(
        FeedNoticeCode::SubscriptionAccepted,
        "Bybit operation acknowledged",
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BybitEnvelope {
    #[serde(default)]
    topic: Option<String>,
    #[serde(rename = "type", default)]
    message_type: Option<String>,
    #[serde(default)]
    ts: Option<i64>,
    #[serde(default)]
    data: Option<Box<RawValue>>,
    #[serde(default)]
    cts: Option<i64>,
    // `cs` (cross sequence) rides the linear ticker envelope only. Modelled so
    // `deny_unknown_fields` admits the ticker frame; the ticker fold reads it as
    // the declared ordering key, and it is absent (None) on every other topic.
    #[serde(default)]
    cs: Option<u64>,
    // Operation-response fields (`subscribe`/`ping` acknowledgements). Captured
    // so the envelope denies genuinely unknown top-level fields without
    // rejecting acknowledgements.
    #[serde(default)]
    op: Option<String>,
    #[serde(default)]
    success: Option<bool>,
    #[serde(default)]
    ret_msg: Option<String>,
    // Some subscribe acks echo the accepted topics. Modelled optional so a
    // partial acceptance (a subset of the frame) is detectable and an ack
    // without the echo (the observed shape) still parses under
    // `deny_unknown_fields`.
    #[serde(default)]
    args: Option<Vec<String>>,
    // `conn_id` and `req_id` are captured but never read, so that
    // `deny_unknown_fields` still rejects genuinely unknown fields instead of
    // tripping on an acknowledgement Bybit already documents.
    #[serde(default)]
    #[allow(dead_code)]
    conn_id: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    req_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BybitPublicTrade {
    #[serde(rename = "T")]
    trade_time: i64,
    #[serde(rename = "s")]
    symbol: String,
    #[serde(rename = "S")]
    side: String,
    #[serde(rename = "v")]
    size: String,
    #[serde(rename = "p")]
    price: String,
    #[serde(rename = "i")]
    trade_id: String,
    #[serde(rename = "BT")]
    block_trade: bool,
    // Retail-price-improvement order flag, present on spot trades.
    #[serde(rename = "RPI", default)]
    rpi: Option<bool>,
    // `L` is the futures-only tick direction. Absent on the spot tape (which is
    // why the spot capture parses without it); required on the linear tape and
    // projected as `tick_direction`. Modelled `Option` so one struct denies
    // unknown fields on both tapes.
    #[serde(rename = "L", default)]
    tick_direction: Option<String>,
    seq: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BybitOrderbook {
    #[serde(rename = "s")]
    symbol: String,
    #[serde(rename = "b")]
    bids: Vec<[String; 2]>,
    #[serde(rename = "a")]
    asks: Vec<[String; 2]>,
    u: u64,
    seq: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BybitLiquidation {
    #[serde(rename = "T")]
    event_time: i64,
    #[serde(rename = "s")]
    symbol: String,
    #[serde(rename = "S")]
    side: String,
    #[serde(rename = "v")]
    size: String,
    // `p` is the bankruptcy price of the liquidated position, not last price.
    #[serde(rename = "p")]
    bankruptcy_price: String,
}

/// The folded linear ticker: exactly the projected subset, held between
/// messages so a delta carrying only changed keys still emits a complete row.
#[derive(Clone)]
struct LinearTickerState {
    instrument: String,
    last_price: String,
    mark_price: String,
    index_price: String,
    funding_rate: String,
    next_funding_time: String,
    open_interest_base: String,
    open_interest_value_usd: String,
    best_bid_price: String,
    best_bid_size: String,
    best_ask_price: String,
    best_ask_size: String,
}

impl LinearTickerState {
    /// Build a fresh state from a snapshot. Every projected key must be present
    /// and valid, or the snapshot is refused (a partial snapshot is not a
    /// snapshot). `funding_rate` is the one signed decimal on this identity; the
    /// non-negative guard is not applied to it.
    fn from_snapshot(ticker: &BybitLinearTicker) -> Result<Self, AdapterError> {
        require_symbol(require_present(&ticker.symbol, "symbol")?)?;
        Ok(Self {
            instrument: INSTRUMENT.to_owned(),
            last_price: require_decimal(&ticker.last_price, "lastPrice")?,
            mark_price: require_decimal(&ticker.mark_price, "markPrice")?,
            index_price: require_decimal(&ticker.index_price, "indexPrice")?,
            funding_rate: require_signed_decimal(&ticker.funding_rate, "fundingRate")?,
            next_funding_time: require_next_funding_time(&ticker.next_funding_time)?,
            open_interest_base: require_decimal(&ticker.open_interest, "openInterest")?,
            open_interest_value_usd: require_decimal(
                &ticker.open_interest_value,
                "openInterestValue",
            )?,
            best_bid_price: require_decimal(&ticker.bid1_price, "bid1Price")?,
            best_bid_size: require_decimal(&ticker.bid1_size, "bid1Size")?,
            best_ask_price: require_decimal(&ticker.ask1_price, "ask1Price")?,
            best_ask_size: require_decimal(&ticker.ask1_size, "ask1Size")?,
        })
    }

    /// Merge a delta's present keys onto the retained state; absent keys keep
    /// their prior value. Every present decimal is validated before it is folded.
    ///
    /// The merge runs on a SCRATCH copy and is committed to `self` only once
    /// every field has validated, so a delta that fails partway (a good
    /// `lastPrice` but a malformed `bid1Price`, say) leaves the retained state
    /// untouched rather than folding some of the new cross-sequence's values
    /// while the frontier still claims the previous one.
    fn merge_delta(&mut self, ticker: &BybitLinearTicker) -> Result<(), AdapterError> {
        if let Some(symbol) = &ticker.symbol {
            require_symbol(symbol)?;
        }
        let mut next = self.clone();
        merge_decimal(&mut next.last_price, &ticker.last_price, "lastPrice")?;
        merge_decimal(&mut next.mark_price, &ticker.mark_price, "markPrice")?;
        merge_decimal(&mut next.index_price, &ticker.index_price, "indexPrice")?;
        merge_signed_decimal(&mut next.funding_rate, &ticker.funding_rate, "fundingRate")?;
        if let Some(raw) = &ticker.next_funding_time {
            next.next_funding_time = next_funding_time(raw)?;
        }
        merge_decimal(
            &mut next.open_interest_base,
            &ticker.open_interest,
            "openInterest",
        )?;
        merge_decimal(
            &mut next.open_interest_value_usd,
            &ticker.open_interest_value,
            "openInterestValue",
        )?;
        merge_decimal(&mut next.best_bid_price, &ticker.bid1_price, "bid1Price")?;
        merge_decimal(&mut next.best_bid_size, &ticker.bid1_size, "bid1Size")?;
        merge_decimal(&mut next.best_ask_price, &ticker.ask1_price, "ask1Price")?;
        merge_decimal(&mut next.best_ask_size, &ticker.ask1_size, "ask1Size")?;
        *self = next;
        Ok(())
    }

    /// The complete normalized row for one admitted ticker message.
    fn row(
        &self,
        update_kind: &str,
        cross_sequence: u64,
        event_time: &str,
    ) -> BTreeMap<String, Value> {
        BTreeMap::from([
            ("instrument".to_owned(), json!(self.instrument)),
            ("update_kind".to_owned(), json!(update_kind)),
            ("last_price".to_owned(), json!(self.last_price)),
            ("mark_price".to_owned(), json!(self.mark_price)),
            ("index_price".to_owned(), json!(self.index_price)),
            ("funding_rate".to_owned(), json!(self.funding_rate)),
            (
                "next_funding_time".to_owned(),
                json!(self.next_funding_time),
            ),
            (
                "open_interest_base".to_owned(),
                json!(self.open_interest_base),
            ),
            (
                "open_interest_value_usd".to_owned(),
                json!(self.open_interest_value_usd),
            ),
            ("best_bid_price".to_owned(), json!(self.best_bid_price)),
            ("best_bid_size".to_owned(), json!(self.best_bid_size)),
            ("best_ask_price".to_owned(), json!(self.best_ask_price)),
            ("best_ask_size".to_owned(), json!(self.best_ask_size)),
            ("cross_sequence".to_owned(), json!(cross_sequence)),
            ("event_time".to_owned(), json!(event_time)),
        ])
    }
}

fn require_present<'a>(value: &'a Option<String>, field: &str) -> Result<&'a str, AdapterError> {
    value.as_deref().ok_or_else(|| {
        AdapterError::InvalidPayload(format!(
            "Bybit ticker snapshot is missing the required `{field}` key"
        ))
    })
}

fn require_decimal(value: &Option<String>, field: &str) -> Result<String, AdapterError> {
    let text = require_present(value, field)?;
    validate_decimal(text, field)?;
    Ok(text.to_owned())
}

fn require_signed_decimal(value: &Option<String>, field: &str) -> Result<String, AdapterError> {
    let text = require_present(value, field)?;
    validate_signed_decimal(text, field)?;
    Ok(text.to_owned())
}

fn require_next_funding_time(value: &Option<String>) -> Result<String, AdapterError> {
    let text = require_present(value, "nextFundingTime")?;
    next_funding_time(text)
}

fn merge_decimal(
    slot: &mut String,
    value: &Option<String>,
    field: &str,
) -> Result<(), AdapterError> {
    if let Some(text) = value {
        validate_decimal(text, field)?;
        *slot = text.clone();
    }
    Ok(())
}

fn merge_signed_decimal(
    slot: &mut String,
    value: &Option<String>,
    field: &str,
) -> Result<(), AdapterError> {
    if let Some(text) = value {
        validate_signed_decimal(text, field)?;
        *slot = text.clone();
    }
    Ok(())
}

/// `nextFundingTime` is a millisecond epoch carried as a string; normalize it to
/// RFC 3339 through the same window guard as every other Bybit timestamp.
fn next_funding_time(raw: &str) -> Result<String, AdapterError> {
    let millis: i64 = raw.trim().parse().map_err(|_| {
        AdapterError::InvalidPayload(format!(
            "Bybit nextFundingTime `{}` is not a millisecond epoch",
            bounded_token(raw)
        ))
    })?;
    millis_to_rfc3339(millis, "nextFundingTime")
}

/// Every documented `category=linear` ticker key. The projected subset is read;
/// the rest are modelled so `deny_unknown_fields` still catches genuine drift
/// but are never projected. Every field is `Option` because a delta carries only
/// the keys that changed.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BybitLinearTicker {
    symbol: Option<String>,
    #[serde(rename = "lastPrice")]
    last_price: Option<String>,
    #[serde(rename = "markPrice")]
    mark_price: Option<String>,
    #[serde(rename = "indexPrice")]
    index_price: Option<String>,
    #[serde(rename = "fundingRate")]
    funding_rate: Option<String>,
    #[serde(rename = "nextFundingTime")]
    next_funding_time: Option<String>,
    #[serde(rename = "openInterest")]
    open_interest: Option<String>,
    #[serde(rename = "openInterestValue")]
    open_interest_value: Option<String>,
    #[serde(rename = "bid1Price")]
    bid1_price: Option<String>,
    #[serde(rename = "bid1Size")]
    bid1_size: Option<String>,
    #[serde(rename = "ask1Price")]
    ask1_price: Option<String>,
    #[serde(rename = "ask1Size")]
    ask1_size: Option<String>,
    // Modelled-but-not-projected keys. Held as raw JSON so any documented value
    // type is admitted; `deny_unknown_fields` still rejects a key outside this
    // set as drift a human must reconcile.
    #[serde(rename = "tickDirection", default)]
    #[allow(dead_code)]
    tick_direction: Option<Box<RawValue>>,
    #[serde(rename = "price24hPcnt", default)]
    #[allow(dead_code)]
    price_24h_pcnt: Option<Box<RawValue>>,
    #[serde(rename = "prevPrice24h", default)]
    #[allow(dead_code)]
    prev_price_24h: Option<Box<RawValue>>,
    #[serde(rename = "prevPrice1h", default)]
    #[allow(dead_code)]
    prev_price_1h: Option<Box<RawValue>>,
    #[serde(rename = "highPrice24h", default)]
    #[allow(dead_code)]
    high_price_24h: Option<Box<RawValue>>,
    #[serde(rename = "lowPrice24h", default)]
    #[allow(dead_code)]
    low_price_24h: Option<Box<RawValue>>,
    #[serde(rename = "volume24h", default)]
    #[allow(dead_code)]
    volume_24h: Option<Box<RawValue>>,
    #[serde(rename = "turnover24h", default)]
    #[allow(dead_code)]
    turnover_24h: Option<Box<RawValue>>,
    #[serde(rename = "basisRate", default)]
    #[allow(dead_code)]
    basis_rate: Option<Box<RawValue>>,
    #[serde(rename = "basis", default)]
    #[allow(dead_code)]
    basis: Option<Box<RawValue>>,
    #[serde(rename = "deliveryTime", default)]
    #[allow(dead_code)]
    delivery_time: Option<Box<RawValue>>,
    #[serde(rename = "deliveryFeeRate", default)]
    #[allow(dead_code)]
    delivery_fee_rate: Option<Box<RawValue>>,
    #[serde(rename = "predictedDeliveryPrice", default)]
    #[allow(dead_code)]
    predicted_delivery_price: Option<Box<RawValue>>,
    #[serde(rename = "preOpenPrice", default)]
    #[allow(dead_code)]
    pre_open_price: Option<Box<RawValue>>,
    #[serde(rename = "preQty", default)]
    #[allow(dead_code)]
    pre_qty: Option<Box<RawValue>>,
    #[serde(rename = "curPreListingPhase", default)]
    #[allow(dead_code)]
    cur_pre_listing_phase: Option<Box<RawValue>>,
    #[serde(rename = "singleOpenInterest", default)]
    #[allow(dead_code)]
    single_open_interest: Option<Box<RawValue>>,
    // Observed in the R2 linear capture (2026-09-03) but not in the docs table:
    // the perpetual's funding interval and cap, and the single-market open
    // interest value. Modelled so `deny_unknown_fields` admits them; not
    // projected.
    #[serde(rename = "singleOpenInterestValue", default)]
    #[allow(dead_code)]
    single_open_interest_value: Option<Box<RawValue>>,
    #[serde(rename = "fundingIntervalHour", default)]
    #[allow(dead_code)]
    funding_interval_hour: Option<Box<RawValue>>,
    #[serde(rename = "fundingCap", default)]
    #[allow(dead_code)]
    funding_cap: Option<Box<RawValue>>,
}

fn require_symbol(symbol: &str) -> Result<(), AdapterError> {
    if symbol == INSTRUMENT {
        Ok(())
    } else {
        Err(AdapterError::InvalidPayload(format!(
            "unexpected Bybit symbol `{}`",
            bounded_token(symbol)
        )))
    }
}

// Bybit `S` names a side as `Buy`/`Sell`. For a trade it is the taker/aggressor
// side; for a liquidation it is the side of the position being liquidated (Buy =
// a long liquidated, Sell = a short liquidated). Both map to the same normalized
// `buy`/`sell` tokens.
fn taker_side(side: &str) -> Result<&'static str, AdapterError> {
    match side {
        "Buy" => Ok("buy"),
        "Sell" => Ok("sell"),
        other => Err(AdapterError::InvalidPayload(format!(
            "unexpected Bybit side `{}`",
            bounded_token(other)
        ))),
    }
}

// Bybit `L` names the price change direction of a fill relative to the prior
// trade. It is present only on the futures (linear) tape. The four documented
// values map to snake_case variants.
fn tick_direction(value: &str) -> Result<&'static str, AdapterError> {
    match value {
        "PlusTick" => Ok("plus_tick"),
        "ZeroPlusTick" => Ok("zero_plus_tick"),
        "MinusTick" => Ok("minus_tick"),
        "ZeroMinusTick" => Ok("zero_minus_tick"),
        other => Err(AdapterError::InvalidPayload(format!(
            "unexpected Bybit tick direction `{}`",
            bounded_token(other)
        ))),
    }
}

fn payload_error(error: serde_json::Error) -> AdapterError {
    AdapterError::InvalidPayload(error.to_string())
}

fn bounded_detail(prefix: &str, value: &str) -> String {
    format!("{prefix}: {}", bounded_token(value))
}

// A REST response body is read under a bounded cap; a body above it is refused,
// never partially parsed.
const MAX_REST_BODY_BYTES: usize = 64 * 1024;
const OPEN_INTEREST_SOURCE_ID: &str = "bybit.linear.btcusdt.open-interest";
const OPEN_INTEREST_SOURCE_VERSION: &str = "1.0.0";
const FUNDING_HISTORY_SOURCE_ID: &str = "bybit.linear.btcusdt.funding-history";
const FUNDING_HISTORY_SOURCE_VERSION: &str = "1.0.0";
// Steady-state open-interest is polled at the 5-minute bucket (§7). The response
// does not echo the request's `intervalTime`, so the bucket is stamped from the
// steady-state query; a coarser backfill bucket is out of scope here.
const OPEN_INTEREST_STEADY_INTERVAL: &str = "5min";

/// Deterministic normalizer for the two Bybit linear REST poll identities
/// (`bybit.linear.btcusdt.open-interest`, `bybit.linear.btcusdt.funding-history`).
///
/// Both ride one capture stream ([`CaptureStream::BybitLinearRest`]); a response
/// is classified by the shape of its list rows. Each identity dedups on
/// `event_time`: the venue republishes the current bucket until it closes, so a
/// repeat of an already-admitted `event_time` is a `Duplicate` notice with no
/// event. Dedup state is not reset on reconnect — the bucket close is a wall
/// instant, not a connection-scoped sequence.
pub struct BybitRestAdapter {
    publisher_epoch: String,
    guard: RecordGuard,
    next_open_interest_sequence: u64,
    next_funding_history_sequence: u64,
    last_open_interest_event_time: Option<String>,
    last_funding_event_time: Option<String>,
}

impl BybitRestAdapter {
    /// Construct the Bybit linear REST normalizer.
    ///
    /// `publisher_epoch` is copied verbatim into every emitted event; it must be
    /// non-blank and at most 128 bytes, or the call returns
    /// [`AdapterError::InvalidRecord`].
    pub fn new(publisher_epoch: impl Into<String>) -> Result<Self, AdapterError> {
        let publisher_epoch = publisher_epoch.into();
        validate_publisher_epoch(&publisher_epoch)?;
        Ok(Self {
            publisher_epoch,
            guard: RecordGuard::new(Provider::BybitLinear),
            next_open_interest_sequence: 0,
            next_funding_history_sequence: 0,
            last_open_interest_event_time: None,
            last_funding_event_time: None,
        })
    }

    fn ingest_body(
        &mut self,
        record: &RawRecord,
        payload: &str,
    ) -> Result<IngestOutcome, AdapterError> {
        if payload.len() > MAX_REST_BODY_BYTES {
            return Err(AdapterError::InvalidPayload(format!(
                "Bybit REST body is {} bytes; maximum is {MAX_REST_BODY_BYTES}",
                payload.len()
            )));
        }
        let envelope: BybitRestEnvelope = serde_json::from_str(payload).map_err(payload_error)?;
        // A non-zero retCode (e.g. 10006 "Too many visits!", served with HTTP
        // 200) is a non-fatal poll outcome, not a stream-ending error: emit no
        // events and note it, so replay never aborts on it and a valid row in a
        // later poll still normalizes. The capture-side backoff ladder is the
        // poller's job.
        if envelope.ret_code != 0 {
            return Ok(IngestOutcome::notice(FeedNotice::info(
                FeedNoticeCode::StateInconsistent,
                format!(
                    "Bybit REST response retCode {} is non-zero: {}; skipped without normalized output",
                    envelope.ret_code,
                    bounded_token(envelope.ret_msg.as_deref().unwrap_or("")),
                ),
            )));
        }
        // An empty list is likewise non-fatal: funding/history legitimately
        // returns none between settlements.
        if envelope.result.list.is_empty() {
            return Ok(IngestOutcome::notice(FeedNotice::info(
                FeedNoticeCode::StateInconsistent,
                "Bybit REST response result.list is empty; skipped without normalized output"
                    .to_owned(),
            )));
        }
        if envelope.result.list.len() > MAX_ARRAY_ROWS {
            return Err(AdapterError::InvalidPayload(format!(
                "Bybit REST response has {} rows; maximum is {MAX_ARRAY_ROWS}",
                envelope.result.list.len()
            )));
        }
        // Classify by the first row's shape: an open-interest row carries
        // `openInterest`, a funding row carries `fundingRate`.
        let probe: BybitRestRowProbe =
            serde_json::from_str(envelope.result.list[0].get()).map_err(payload_error)?;
        match (probe.open_interest.is_some(), probe.funding_rate.is_some()) {
            (true, false) => self.open_interest(record, &envelope.result),
            (false, true) => self.funding_history(record, &envelope.result),
            _ => Err(AdapterError::InvalidPayload(
                "Bybit REST row is neither an open-interest nor a funding-history row".to_owned(),
            )),
        }
    }

    fn open_interest(
        &mut self,
        record: &RawRecord,
        result: &BybitRestResult,
    ) -> Result<IngestOutcome, AdapterError> {
        // The open-interest response carries `result.symbol`.
        require_symbol(require_present(&result.symbol, "result.symbol")?)?;
        let mut events = Vec::new();
        let mut notices = Vec::new();
        // Dedup against the NEWEST bucket admitted in a PRIOR poll (a high-water
        // mark snapshotted here, before the loop), not a single slot rewritten
        // per row. The list is newest-first and may carry several rows: a per-row
        // slot would end holding the OLDEST row and re-emit every newer bucket on
        // the next poll, while snapshotting the mark lets several genuinely-new
        // rows in one response all admit. The event_time is a fixed-width
        // canonical RFC3339 `…Z`-millis string, so a lexicographic compare is
        // chronological.
        let previous_newest = self.last_open_interest_event_time.clone();
        let mut newest_admitted = previous_newest.clone();
        for raw in &result.list {
            let row: OpenInterestRow = serde_json::from_str(raw.get()).map_err(payload_error)?;
            validate_decimal(&row.open_interest, "openInterest")?;
            let event_time = millis_from_text(&row.timestamp, "timestamp")?;
            if previous_newest
                .as_deref()
                .is_some_and(|newest| event_time.as_str() <= newest)
            {
                notices.push(FeedNotice::info(
                    FeedNoticeCode::Duplicate,
                    format!("Bybit open-interest bucket {event_time} already admitted; skipped"),
                ));
                continue;
            }
            let values = BTreeMap::from([
                ("instrument".to_owned(), json!(INSTRUMENT)),
                ("interval".to_owned(), json!(OPEN_INTEREST_STEADY_INTERVAL)),
                ("open_interest".to_owned(), json!(row.open_interest)),
                ("event_time".to_owned(), json!(event_time)),
                ("retrieved_at".to_owned(), json!(record.received_at)),
            ]);
            events.push(self.rest_event(
                OPEN_INTEREST_SOURCE_ID,
                OPEN_INTEREST_SOURCE_VERSION,
                event_time.clone(),
                record.received_at.clone(),
                values,
            )?);
            if newest_admitted
                .as_deref()
                .is_none_or(|newest| event_time.as_str() > newest)
            {
                newest_admitted = Some(event_time);
            }
        }
        self.last_open_interest_event_time = newest_admitted;
        Ok(IngestOutcome { events, notices })
    }

    fn funding_history(
        &mut self,
        record: &RawRecord,
        result: &BybitRestResult,
    ) -> Result<IngestOutcome, AdapterError> {
        let mut events = Vec::new();
        let mut notices = Vec::new();
        // High-water mark snapshotted before the loop (see `open_interest`): the
        // funding list is newest-first and can legitimately return several
        // settlements, so a single per-row slot would hold the OLDEST and
        // re-emit the newer settlements on the next poll.
        let previous_newest = self.last_funding_event_time.clone();
        let mut newest_admitted = previous_newest.clone();
        for raw in &result.list {
            let row: FundingRow = serde_json::from_str(raw.get()).map_err(payload_error)?;
            // The funding row carries its own symbol; the response object does not.
            require_symbol(&row.symbol)?;
            validate_signed_decimal(&row.funding_rate, "fundingRate")?;
            let event_time = millis_from_text(&row.funding_rate_timestamp, "fundingRateTimestamp")?;
            if previous_newest
                .as_deref()
                .is_some_and(|newest| event_time.as_str() <= newest)
            {
                notices.push(FeedNotice::info(
                    FeedNoticeCode::Duplicate,
                    format!("Bybit funding settlement {event_time} already admitted; skipped"),
                ));
                continue;
            }
            let values = BTreeMap::from([
                ("instrument".to_owned(), json!(INSTRUMENT)),
                ("funding_rate".to_owned(), json!(row.funding_rate)),
                ("event_time".to_owned(), json!(event_time)),
                ("retrieved_at".to_owned(), json!(record.received_at)),
            ]);
            events.push(self.rest_event(
                FUNDING_HISTORY_SOURCE_ID,
                FUNDING_HISTORY_SOURCE_VERSION,
                event_time.clone(),
                record.received_at.clone(),
                values,
            )?);
            if newest_admitted
                .as_deref()
                .is_none_or(|newest| event_time.as_str() > newest)
            {
                newest_admitted = Some(event_time);
            }
        }
        self.last_funding_event_time = newest_admitted;
        Ok(IngestOutcome { events, notices })
    }

    fn rest_event(
        &mut self,
        source_id: &'static str,
        source_version: &'static str,
        event_time: String,
        received_at: String,
        values: BTreeMap<String, Value>,
    ) -> Result<SourceEvent, AdapterError> {
        let sequence = match source_id {
            OPEN_INTEREST_SOURCE_ID => &mut self.next_open_interest_sequence,
            FUNDING_HISTORY_SOURCE_ID => &mut self.next_funding_history_sequence,
            _ => {
                return Err(AdapterError::InvalidRecord(
                    "unknown Bybit REST source identity".to_owned(),
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

impl FeedAdapter for BybitRestAdapter {
    fn ingest(&mut self, record: &RawRecord) -> Result<IngestOutcome, AdapterError> {
        match self.guard.observe(record)? {
            GuardAction::Configuration(_) => Err(AdapterError::InvalidRecord(
                "Bybit REST captures do not accept configuration records".to_owned(),
            )),
            GuardAction::Connected => Ok(IngestOutcome::notice(FeedNotice::info(
                FeedNoticeCode::Connected,
                "Bybit REST poll session opened",
            ))),
            GuardAction::Disconnected => Ok(IngestOutcome::notice(FeedNotice::recovery(
                FeedNoticeCode::Disconnected,
                "Bybit REST poll session ended",
                RecoveryAction::Reconnect,
            ))),
            GuardAction::Outbound => Ok(IngestOutcome::default()),
            GuardAction::Inbound(payload) => self.ingest_body(record, payload),
            // A schema-4 `Failed` record is a poll that produced no usable
            // response (ruling E32; A3's `RawRecord::failed_at`). Only the REST
            // connection can carry one, and it is operational state, not market
            // data: a recovery notice, no event. The live capture loop writes one
            // for every non-served outcome — a transport/decode failure, a
            // non-2xx status, or a served-error body (retCode != 0 / empty list) —
            // uniformly with the Binance USD-M REST adapter, so a refused or
            // rate-limited poll leaves a durable, replayable trace rather than
            // only a counter and a stderr line.
            GuardAction::Failed(detail) => Ok(IngestOutcome::notice(FeedNotice::recovery(
                FeedNoticeCode::StateInconsistent,
                bounded_detail("Bybit REST request failed", detail),
                RecoveryAction::Reconnect,
            ))),
        }
    }
}

/// A millisecond epoch carried as a JSON string (Bybit REST rows), normalized to
/// RFC 3339 through the shared window guard.
fn millis_from_text(raw: &str, field: &str) -> Result<String, AdapterError> {
    let millis: i64 = raw.trim().parse().map_err(|_| {
        AdapterError::InvalidPayload(format!(
            "Bybit {field} `{}` is not a millisecond epoch",
            bounded_token(raw)
        ))
    })?;
    millis_to_rfc3339(millis, field)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BybitRestEnvelope {
    #[serde(rename = "retCode")]
    ret_code: i64,
    #[serde(rename = "retMsg", default)]
    ret_msg: Option<String>,
    result: BybitRestResult,
    #[serde(rename = "retExtInfo", default)]
    #[allow(dead_code)]
    ret_ext_info: Option<Box<RawValue>>,
    #[serde(default)]
    #[allow(dead_code)]
    time: Option<i64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BybitRestResult {
    #[serde(default)]
    #[allow(dead_code)]
    category: Option<String>,
    #[serde(default)]
    symbol: Option<String>,
    #[serde(default)]
    list: Vec<Box<RawValue>>,
    #[serde(rename = "nextPageCursor", default)]
    #[allow(dead_code)]
    next_page_cursor: Option<String>,
}

#[derive(Deserialize)]
struct BybitRestRowProbe {
    #[serde(rename = "openInterest", default)]
    open_interest: Option<Box<RawValue>>,
    #[serde(rename = "fundingRate", default)]
    funding_rate: Option<Box<RawValue>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenInterestRow {
    #[serde(rename = "openInterest")]
    open_interest: String,
    timestamp: String,
    // Observed in the R2 REST capture (2026-09-03) but not projected: the
    // single-market open interest. Modelled so `deny_unknown_fields` admits it.
    #[serde(rename = "singleOpenInterest", default)]
    #[allow(dead_code)]
    single_open_interest: Option<Box<RawValue>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FundingRow {
    symbol: String,
    #[serde(rename = "fundingRate")]
    funding_rate: String,
    #[serde(rename = "fundingRateTimestamp")]
    funding_rate_timestamp: String,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{BybitAdapter, BybitMarket, BybitRestAdapter};
    use crate::{
        CaptureStream, FeedAdapter, FeedNoticeCode, IngestOutcome, RawRecord, RecordKind,
        RecoveryAction,
    };

    fn spot() -> BybitAdapter {
        BybitAdapter::new("test-epoch", BybitMarket::Spot).expect("spot adapter constructs")
    }

    fn linear() -> BybitAdapter {
        BybitAdapter::new("test-epoch", BybitMarket::Linear).expect("linear adapter constructs")
    }

    fn record(stream: CaptureStream, ordinal: u64, payload: &str) -> RawRecord {
        RawRecord {
            schema_version: 1,
            provider: stream.provider(),
            stream,
            connection_epoch: 1,
            ordinal,
            received_at: "2026-08-28T12:00:01Z".to_owned(),
            capture_elapsed_ns: None,
            connect_started_elapsed_ns: None,
            kind: RecordKind::InboundText,
            payload: Some(payload.to_owned()),
        }
    }

    fn connected(stream: CaptureStream) -> RawRecord {
        RawRecord {
            kind: RecordKind::Connected,
            ordinal: 1,
            received_at: "2026-08-28T12:00:00Z".to_owned(),
            payload: None,
            ..record(stream, 1, "")
        }
    }

    fn trade_message(ordinal: u64, id: &str, side: &str, price: &str, seq: u64) -> RawRecord {
        record(
            CaptureStream::BybitSpot,
            ordinal,
            &format!(
                r#"{{"topic":"publicTrade.BTCUSDT","type":"snapshot","ts":1787918400123,"data":[{{"T":1787918400100,"s":"BTCUSDT","S":"{side}","v":"0.001","p":"{price}","i":"{id}","BT":false,"RPI":false,"seq":{seq}}}]}}"#
            ),
        )
    }

    fn book_message(
        ordinal: u64,
        topic: &str,
        kind: &str,
        u: u64,
        bids: &str,
        asks: &str,
    ) -> RawRecord {
        record(
            CaptureStream::BybitSpot,
            ordinal,
            &format!(
                r#"{{"topic":"{topic}","type":"{kind}","ts":1787918400123,"data":{{"s":"BTCUSDT","b":{bids},"a":{asks},"u":{u},"seq":7961638724}},"cts":1787918400120}}"#
            ),
        )
    }

    #[test]
    fn wrapper_edges_are_classified_without_events() {
        let mut adapter = spot();
        adapter
            .ingest(&connected(CaptureStream::BybitSpot))
            .expect("connects");
        // Subscribe acknowledgement.
        let ack = adapter
            .ingest(&record(
                CaptureStream::BybitSpot,
                2,
                r#"{"success":true,"ret_msg":"subscribe","conn_id":"c1","req_id":"1","op":"subscribe"}"#,
            ))
            .expect("subscribe ack is operational");
        assert!(ack.events.is_empty());
        assert_eq!(ack.notices[0].code, FeedNoticeCode::SubscriptionAccepted);
        // Ping/pong acknowledgement.
        let pong = adapter
            .ingest(&record(
                CaptureStream::BybitSpot,
                3,
                r#"{"success":true,"ret_msg":"pong","conn_id":"c1","req_id":"","op":"ping"}"#,
            ))
            .expect("pong is operational");
        assert!(pong.events.is_empty());
        assert_eq!(pong.notices[0].code, FeedNoticeCode::Heartbeat);
        // Rejection.
        let rejected = adapter
            .ingest(&record(
                CaptureStream::BybitSpot,
                4,
                r#"{"success":false,"ret_msg":"Invalid topic","conn_id":"c1","op":"subscribe"}"#,
            ))
            .expect("rejection is operational");
        assert!(rejected.events.is_empty());
        assert_eq!(
            rejected.notices[0].code,
            FeedNoticeCode::SubscriptionRejected
        );
        // Unknown topic.
        let unknown = adapter
            .ingest(&record(
                CaptureStream::BybitSpot,
                5,
                r#"{"topic":"tickers.BTCUSDT","type":"snapshot","ts":1787918400123,"data":{"x":1}}"#,
            ))
            .expect("unknown topic is operational");
        assert!(unknown.events.is_empty());
        assert_eq!(unknown.notices[0].code, FeedNoticeCode::UnsupportedMessage);
        // A message with neither op nor topic.
        let stray = adapter
            .ingest(&record(
                CaptureStream::BybitSpot,
                6,
                r#"{"ts":1787918400123}"#,
            ))
            .expect("stray message is operational");
        assert!(stray.events.is_empty());
        assert_eq!(stray.notices[0].code, FeedNoticeCode::UnsupportedMessage);
        // Malformed JSON fails closed.
        assert!(
            adapter
                .ingest(&record(CaptureStream::BybitSpot, 7, r#"{not json"#))
                .is_err()
        );
    }

    #[test]
    fn public_trade_emits_with_aggressor_side_and_opaque_id() {
        let mut adapter = spot();
        adapter
            .ingest(&connected(CaptureStream::BybitSpot))
            .expect("connects");
        let buy = adapter
            .ingest(&trade_message(2, "uuid-a", "Buy", "60000.50", 100))
            .expect("first trade");
        assert_eq!(buy.events.len(), 1);
        assert_eq!(buy.events[0].source_id, "bybit.spot.btcusdt.trades");
        assert_eq!(
            buy.events[0].values.get("aggressor_side"),
            Some(&json!("buy"))
        );
        assert_eq!(buy.events[0].values.get("trade_id"), Some(&json!("uuid-a")));
        assert_eq!(buy.events[0].values.get("price"), Some(&json!("60000.50")));
        assert_eq!(buy.events[0].values.get("rpi"), Some(&json!(false)));
        assert_eq!(
            buy.events[0].values.get("event_time"),
            Some(&json!("2026-08-28T12:00:00.100Z"))
        );
        let sell = adapter
            .ingest(&trade_message(3, "uuid-b", "Sell", "60001.00", 101))
            .expect("second trade");
        assert_eq!(
            sell.events[0].values.get("aggressor_side"),
            Some(&json!("sell"))
        );
    }

    #[test]
    fn trade_rpi_flag_is_emitted_and_defaults_false_when_absent() {
        let mut adapter = spot();
        adapter
            .ingest(&connected(CaptureStream::BybitSpot))
            .expect("connects");
        // RPI true is carried through.
        let rpi_true = adapter
            .ingest(&record(
                CaptureStream::BybitSpot,
                2,
                r#"{"topic":"publicTrade.BTCUSDT","type":"snapshot","ts":1787918400123,"data":[{"T":1787918400100,"s":"BTCUSDT","S":"Buy","v":"0.001","p":"60000.50","i":"2290000001200569874","BT":false,"RPI":true,"seq":10}]}"#,
            ))
            .expect("rpi-true trade");
        assert_eq!(rpi_true.events[0].values.get("rpi"), Some(&json!(true)));
        assert_eq!(
            rpi_true.events[0].values.get("trade_id"),
            Some(&json!("2290000001200569874"))
        );
        // An absent RPI defaults to false.
        let rpi_absent = adapter
            .ingest(&record(
                CaptureStream::BybitSpot,
                3,
                r#"{"topic":"publicTrade.BTCUSDT","type":"snapshot","ts":1787918400123,"data":[{"T":1787918400100,"s":"BTCUSDT","S":"Buy","v":"0.001","p":"60000.50","i":"2290000001200569875","BT":false,"seq":11}]}"#,
            ))
            .expect("rpi-absent trade");
        assert_eq!(rpi_absent.events[0].values.get("rpi"), Some(&json!(false)));
    }

    #[test]
    fn cross_stream_topic_is_refused_without_normalized_output() {
        // A topic outside the instance's compiled set is refused, never
        // normalized under a foreign source id. `orderbook.200` is a spot-only
        // depth: the linear adapter (which subscribes `orderbook.50`) must not
        // normalize it. `publicTrade` is shared by both sockets, so it is no
        // longer the cross-stream witness.
        let mut adapter = linear();
        adapter
            .ingest(&connected(CaptureStream::BybitLinear))
            .expect("connects");
        let refused = adapter
            .ingest(&record(
                CaptureStream::BybitLinear,
                2,
                r#"{"topic":"orderbook.200.BTCUSDT","type":"snapshot","ts":1787918400123,"data":{"s":"BTCUSDT","b":[["60000.10","1.0"]],"a":[],"u":100,"seq":1},"cts":1787918400120}"#,
            ))
            .expect("cross-stream topic is operational");
        assert!(refused.events.is_empty());
        assert_eq!(refused.notices[0].code, FeedNoticeCode::UnsupportedMessage);
    }

    #[test]
    fn empty_data_arrays_are_state_inconsistent_without_events_or_cursor_change() {
        // Empty trade batch.
        let mut spot_adapter = spot();
        spot_adapter
            .ingest(&connected(CaptureStream::BybitSpot))
            .expect("connects");
        let empty_trades = spot_adapter
            .ingest(&record(
                CaptureStream::BybitSpot,
                2,
                r#"{"topic":"publicTrade.BTCUSDT","type":"snapshot","ts":1787918400123,"data":[]}"#,
            ))
            .expect("empty trades operational");
        assert!(empty_trades.events.is_empty());
        assert_eq!(
            empty_trades.notices[0].code,
            FeedNoticeCode::StateInconsistent
        );
        // An empty book message does not seed the cursor: a later populated
        // snapshot still seeds, and its following delta applies.
        let empty_book = spot_adapter
            .ingest(&book_message(
                3,
                "orderbook.200.BTCUSDT",
                "snapshot",
                100,
                r#"[]"#,
                r#"[]"#,
            ))
            .expect("empty book operational");
        assert!(empty_book.events.is_empty());
        assert_eq!(
            empty_book.notices[0].code,
            FeedNoticeCode::StateInconsistent
        );
        // The empty snapshot did NOT seed: a delta now is still premature.
        let premature = spot_adapter
            .ingest(&book_message(
                4,
                "orderbook.200.BTCUSDT",
                "delta",
                101,
                r#"[["60000.10","1.0"]]"#,
                r#"[]"#,
            ))
            .expect("delta operational");
        assert!(premature.events.is_empty());
        assert_eq!(premature.notices[0].code, FeedNoticeCode::AwaitingSnapshot);

        // Empty liquidation batch.
        let mut linear_adapter = linear();
        linear_adapter
            .ingest(&connected(CaptureStream::BybitLinear))
            .expect("connects");
        let empty_liquidations = linear_adapter
            .ingest(&record(
                CaptureStream::BybitLinear,
                2,
                r#"{"topic":"allLiquidation.BTCUSDT","type":"snapshot","ts":1739502303204,"data":[]}"#,
            ))
            .expect("empty liquidations operational");
        assert!(empty_liquidations.events.is_empty());
        assert_eq!(
            empty_liquidations.notices[0].code,
            FeedNoticeCode::StateInconsistent
        );
    }

    #[test]
    fn malformed_empty_book_frames_fail_closed_before_the_empty_short_circuit() {
        // Validation precedes the empty-message short-circuit: a frame that
        // carries no levels but a garbage type or an implausible cts must
        // error, not be retained as a benign StateInconsistent notice.
        let mut adapter = spot();
        adapter
            .ingest(&connected(CaptureStream::BybitSpot))
            .expect("connects");
        let garbage_type = adapter.ingest(&book_message(
            2,
            "orderbook.200.BTCUSDT",
            "resync",
            100,
            r#"[]"#,
            r#"[]"#,
        ));
        assert!(garbage_type.is_err(), "garbage type must fail closed");
        let microsecond_cts = adapter.ingest(&record(
            CaptureStream::BybitSpot,
            3,
            r#"{"topic":"orderbook.200.BTCUSDT","type":"snapshot","ts":1787918400123,"cts":1787918400123000,"data":{"s":"BTCUSDT","b":[],"a":[],"u":100,"seq":7}}"#,
        ));
        assert!(
            microsecond_cts.is_err(),
            "microsecond-scale cts must fail closed even on an empty frame"
        );
    }

    #[test]
    fn trade_seq_regression_notes_but_still_emits_and_equal_seq_is_legal() {
        let mut adapter = spot();
        adapter
            .ingest(&connected(CaptureStream::BybitSpot))
            .expect("connects");
        adapter
            .ingest(&trade_message(2, "uuid-a", "Buy", "60000.00", 200))
            .expect("first trade");
        // Equal seq: one match, several fills -> legal, no notice.
        let equal = adapter
            .ingest(&trade_message(3, "uuid-b", "Buy", "60000.00", 200))
            .expect("equal-seq trade");
        assert_eq!(equal.events.len(), 1);
        assert!(equal.notices.is_empty());
        // Lower seq: out of order -> noted, but the trade is still emitted.
        let regressed = adapter
            .ingest(&trade_message(4, "uuid-c", "Sell", "59999.00", 199))
            .expect("regressed trade");
        assert_eq!(regressed.events.len(), 1);
        assert_eq!(regressed.notices[0].code, FeedNoticeCode::OutOfOrder);
    }

    #[test]
    fn trade_validates_before_the_ordering_check() {
        let mut adapter = spot();
        adapter
            .ingest(&connected(CaptureStream::BybitSpot))
            .expect("connects");
        adapter
            .ingest(&trade_message(2, "uuid-a", "Buy", "60000.00", 200))
            .expect("first trade");
        // A lower-seq message carrying a non-decimal price must error, not be
        // swallowed as an out-of-order skip (there is no skip here, but the
        // decimal must fail closed regardless of sequence).
        assert!(
            adapter
                .ingest(&trade_message(3, "uuid-b", "Buy", "1e5", 1))
                .is_err()
        );
    }

    #[test]
    fn orderbook_snapshot_resets_and_emits_levels() {
        let mut adapter = spot();
        adapter
            .ingest(&connected(CaptureStream::BybitSpot))
            .expect("connects");
        let snapshot = adapter
            .ingest(&book_message(
                2,
                "orderbook.200.BTCUSDT",
                "snapshot",
                100,
                r#"[["60000.10","1.25"],["59999.00","2.0"]]"#,
                r#"[["60001.20","3.0"]]"#,
            ))
            .expect("snapshot applies");
        assert_eq!(snapshot.events.len(), 3);
        assert_eq!(
            snapshot.events[0].source_id,
            "bybit.spot.btcusdt.book-levels"
        );
        for event in &snapshot.events {
            assert_eq!(event.values.get("update_kind"), Some(&json!("snapshot")));
            assert_eq!(event.values.get("update_count"), Some(&json!(3)));
            assert_eq!(event.values.get("event_time_basis"), Some(&json!("source")));
            assert_eq!(
                event.values.get("event_time"),
                Some(&json!("2026-08-28T12:00:00.120Z"))
            );
        }
        // A following delta with a higher u applies.
        let delta = adapter
            .ingest(&book_message(
                3,
                "orderbook.200.BTCUSDT",
                "delta",
                101,
                r#"[["60000.10","1.5"]]"#,
                r#"[]"#,
            ))
            .expect("delta applies");
        assert_eq!(delta.events.len(), 1);
        assert_eq!(
            delta.events[0].values.get("update_kind"),
            Some(&json!("delta"))
        );
    }

    #[test]
    fn orderbook_delta_before_snapshot_recovers_without_events() {
        let mut adapter = spot();
        adapter
            .ingest(&connected(CaptureStream::BybitSpot))
            .expect("connects");
        let premature = adapter
            .ingest(&book_message(
                2,
                "orderbook.200.BTCUSDT",
                "delta",
                50,
                r#"[["60000.10","1.5"]]"#,
                r#"[]"#,
            ))
            .expect("delta before snapshot is operational");
        assert!(premature.events.is_empty());
        assert_eq!(premature.notices[0].code, FeedNoticeCode::AwaitingSnapshot);
        assert!(premature.notices[0].recovery.is_some());
    }

    #[test]
    fn orderbook_non_advancing_update_id_is_skipped_with_a_notice() {
        let mut adapter = spot();
        adapter
            .ingest(&connected(CaptureStream::BybitSpot))
            .expect("connects");
        adapter
            .ingest(&book_message(
                2,
                "orderbook.200.BTCUSDT",
                "snapshot",
                100,
                r#"[["60000.10","1.0"]]"#,
                r#"[["60001.20","1.0"]]"#,
            ))
            .expect("snapshot");
        // Equal u -> duplicate, skipped.
        let duplicate = adapter
            .ingest(&book_message(
                3,
                "orderbook.200.BTCUSDT",
                "delta",
                100,
                r#"[["60000.10","9.0"]]"#,
                r#"[]"#,
            ))
            .expect("duplicate is operational");
        assert!(duplicate.events.is_empty());
        assert_eq!(duplicate.notices[0].code, FeedNoticeCode::Duplicate);
        // Lower u -> out of order, skipped, with the sibling OutOfOrder code.
        let stale = adapter
            .ingest(&book_message(
                4,
                "orderbook.200.BTCUSDT",
                "delta",
                99,
                r#"[["60000.10","9.0"]]"#,
                r#"[]"#,
            ))
            .expect("out of order is operational");
        assert!(stale.events.is_empty());
        assert_eq!(stale.notices[0].code, FeedNoticeCode::OutOfOrder);
        // F4: a forward jump past the next id is a resync condition — Gap notice,
        // no event, book dropped — not a silent apply. `u` is contiguous in every
        // observed hour, so a gap means a dropped delta and a diverged book.
        let jumped = adapter
            .ingest(&book_message(
                5,
                "orderbook.200.BTCUSDT",
                "delta",
                500,
                r#"[["60000.10","2.0"]]"#,
                r#"[]"#,
            ))
            .expect("gap is operational");
        assert!(jumped.events.is_empty());
        assert_eq!(jumped.notices[0].code, FeedNoticeCode::Gap);
        assert!(jumped.notices[0].recovery.is_some());
    }

    #[test]
    fn orderbook_forward_u_gap_drops_the_book_and_awaits_a_snapshot() {
        // F4: the full resync cycle. A clean +1 delta applies; a forward gap emits
        // a Gap recovery notice and drops the book; a following delta then awaits a
        // snapshot; a fresh snapshot reseeds and publishes again.
        let mut adapter = spot();
        adapter
            .ingest(&connected(CaptureStream::BybitSpot))
            .expect("connects");
        adapter
            .ingest(&book_message(
                2,
                "orderbook.200.BTCUSDT",
                "snapshot",
                100,
                r#"[["60000.10","1.0"]]"#,
                r#"[["60001.20","1.0"]]"#,
            ))
            .expect("snapshot");
        // u == previous + 1 applies cleanly.
        let clean = adapter
            .ingest(&book_message(
                3,
                "orderbook.200.BTCUSDT",
                "delta",
                101,
                r#"[["60000.10","1.5"]]"#,
                r#"[]"#,
            ))
            .expect("contiguous delta applies");
        assert_eq!(clean.events.len(), 1);
        assert!(clean.notices.is_empty());
        // u == previous + 2 is a forward gap: Gap notice, no event, book dropped.
        let gap = adapter
            .ingest(&book_message(
                4,
                "orderbook.200.BTCUSDT",
                "delta",
                103,
                r#"[["60000.10","2.0"]]"#,
                r#"[]"#,
            ))
            .expect("gap is operational");
        assert!(gap.events.is_empty());
        assert_eq!(gap.notices[0].code, FeedNoticeCode::Gap);
        assert!(gap.notices[0].recovery.is_some());
        // The book is dropped: the next delta awaits a fresh snapshot.
        let after = adapter
            .ingest(&book_message(
                5,
                "orderbook.200.BTCUSDT",
                "delta",
                104,
                r#"[["60000.10","3.0"]]"#,
                r#"[]"#,
            ))
            .expect("post-gap delta is operational");
        assert!(after.events.is_empty());
        assert_eq!(after.notices[0].code, FeedNoticeCode::AwaitingSnapshot);
        // A fresh snapshot reseeds and publishes again.
        let reseed = adapter
            .ingest(&book_message(
                6,
                "orderbook.200.BTCUSDT",
                "snapshot",
                200,
                r#"[["60000.10","1.0"]]"#,
                r#"[["60001.20","1.0"]]"#,
            ))
            .expect("reseed snapshot");
        assert_eq!(reseed.events.len(), 2);
    }

    #[test]
    fn snapshot_always_seeds_regardless_of_update_id() {
        let mut adapter = spot();
        adapter
            .ingest(&connected(CaptureStream::BybitSpot))
            .expect("connects");
        adapter
            .ingest(&book_message(
                2,
                "orderbook.200.BTCUSDT",
                "snapshot",
                100,
                r#"[["60000.10","1.0"]]"#,
                r#"[["60001.20","1.0"]]"#,
            ))
            .expect("first snapshot seeds at u=100");
        // A same-u snapshot re-applies (no Duplicate skip): the venue repeats the
        // level-1 snapshot with an unchanged u, and a snapshot always seeds.
        let same_u = adapter
            .ingest(&book_message(
                3,
                "orderbook.200.BTCUSDT",
                "snapshot",
                100,
                r#"[["60000.10","2.0"]]"#,
                r#"[["60001.20","2.0"]]"#,
            ))
            .expect("same-u snapshot re-applies");
        assert_eq!(same_u.events.len(), 2);
        assert!(same_u.notices.is_empty());
        // A later snapshot with a LOWER u re-seeds (snapshot always resets the
        // cursor), and the delta that follows applies against the new base.
        let lower_u = adapter
            .ingest(&book_message(
                4,
                "orderbook.200.BTCUSDT",
                "snapshot",
                50,
                r#"[["60000.10","3.0"]]"#,
                r#"[["60001.20","3.0"]]"#,
            ))
            .expect("lower-u snapshot re-seeds");
        assert_eq!(lower_u.events.len(), 2);
        let delta = adapter
            .ingest(&book_message(
                5,
                "orderbook.200.BTCUSDT",
                "delta",
                51,
                r#"[["60000.10","4.0"]]"#,
                r#"[]"#,
            ))
            .expect("delta applies against the re-seeded base");
        assert_eq!(delta.events.len(), 1);
        assert!(delta.notices.is_empty());
    }

    #[test]
    fn mid_stream_u1_snapshot_seeds_and_notes_the_restart() {
        let mut adapter = spot();
        adapter
            .ingest(&connected(CaptureStream::BybitSpot))
            .expect("connects");
        adapter
            .ingest(&book_message(
                2,
                "orderbook.200.BTCUSDT",
                "snapshot",
                100,
                r#"[["60000.10","1.0"]]"#,
                r#"[["60001.20","1.0"]]"#,
            ))
            .expect("initial snapshot seeds the book");
        // A mid-stream u=1 snapshot IS the fresh seed after a service restart: it
        // emits its levels AND carries a restart recovery notice.
        let restart = adapter
            .ingest(&book_message(
                3,
                "orderbook.200.BTCUSDT",
                "snapshot",
                1,
                r#"[["60000.10","9.0"]]"#,
                r#"[["60001.20","9.0"]]"#,
            ))
            .expect("u=1 snapshot seeds");
        assert_eq!(restart.events.len(), 2);
        assert_eq!(restart.notices.len(), 1);
        assert_eq!(restart.notices[0].code, FeedNoticeCode::StateInconsistent);
        assert!(restart.notices[0].recovery.is_some());
        // The book was reseeded at u=1, so a following delta u=2 applies.
        let delta = adapter
            .ingest(&book_message(
                4,
                "orderbook.200.BTCUSDT",
                "delta",
                2,
                r#"[["60000.10","10.0"]]"#,
                r#"[]"#,
            ))
            .expect("delta u=2 applies after the u=1 reseed");
        assert_eq!(delta.events.len(), 1);
        assert!(delta.notices.is_empty());
    }

    #[test]
    fn u1_delta_is_skipped_awaiting_a_snapshot() {
        let mut adapter = spot();
        adapter
            .ingest(&connected(CaptureStream::BybitSpot))
            .expect("connects");
        adapter
            .ingest(&book_message(
                2,
                "orderbook.200.BTCUSDT",
                "snapshot",
                100,
                r#"[["60000.10","1.0"]]"#,
                r#"[["60001.20","1.0"]]"#,
            ))
            .expect("snapshot seeds the book");
        // A u=1 typed as a DELTA is malformed relative to the contract (a delta is
        // never the restart seed): skip, discard the book, await a snapshot.
        let restart = adapter
            .ingest(&book_message(
                3,
                "orderbook.200.BTCUSDT",
                "delta",
                1,
                r#"[["60000.10","9.0"]]"#,
                r#"[]"#,
            ))
            .expect("u=1 delta is operational");
        assert!(restart.events.is_empty());
        assert_eq!(restart.notices[0].code, FeedNoticeCode::AwaitingSnapshot);
        assert!(restart.notices[0].recovery.is_some());
        // The cursor was cleared, so the next delta is premature until a snapshot.
        let premature = adapter
            .ingest(&book_message(
                4,
                "orderbook.200.BTCUSDT",
                "delta",
                200,
                r#"[["60000.10","9.0"]]"#,
                r#"[]"#,
            ))
            .expect("premature delta is operational");
        assert!(premature.events.is_empty());
        assert_eq!(premature.notices[0].code, FeedNoticeCode::AwaitingSnapshot);
    }

    #[test]
    fn orderbook_receipt_time_fallback_when_cts_is_absent() {
        let mut adapter = spot();
        adapter
            .ingest(&connected(CaptureStream::BybitSpot))
            .expect("connects");
        // A snapshot with ts but no cts falls back to the record's receipt time,
        // labeled "receipt" honestly.
        let outcome = adapter
            .ingest(&record(
                CaptureStream::BybitSpot,
                2,
                r#"{"topic":"orderbook.200.BTCUSDT","type":"snapshot","ts":1787918400123,"data":{"s":"BTCUSDT","b":[["60000.10","1.0"]],"a":[],"u":100,"seq":1}}"#,
            ))
            .expect("cts-absent snapshot applies");
        assert_eq!(outcome.events.len(), 1);
        assert_eq!(
            outcome.events[0].values.get("event_time_basis"),
            Some(&json!("receipt"))
        );
        // The record helper stamps received_at = 2026-08-28T12:00:01Z.
        assert_eq!(
            outcome.events[0].values.get("event_time"),
            Some(&json!("2026-08-28T12:00:01Z"))
        );
        assert_eq!(outcome.events[0].event_time, "2026-08-28T12:00:01Z");
    }

    #[test]
    fn orderbook_without_cts_or_ts_is_rejected() {
        let mut adapter = spot();
        adapter
            .ingest(&connected(CaptureStream::BybitSpot))
            .expect("connects");
        let malformed = record(
            CaptureStream::BybitSpot,
            2,
            r#"{"topic":"orderbook.200.BTCUSDT","type":"snapshot","data":{"s":"BTCUSDT","b":[["60000.10","1.0"]],"a":[],"u":100,"seq":1}}"#,
        );
        assert!(adapter.ingest(&malformed).is_err());
    }

    #[test]
    fn orderbook_level_deletion_emits_size_zero() {
        let mut adapter = spot();
        adapter
            .ingest(&connected(CaptureStream::BybitSpot))
            .expect("connects");
        adapter
            .ingest(&book_message(
                2,
                "orderbook.200.BTCUSDT",
                "snapshot",
                100,
                r#"[["60000.10","1.0"]]"#,
                r#"[["60001.20","1.0"]]"#,
            ))
            .expect("snapshot");
        let deletion = adapter
            .ingest(&book_message(
                3,
                "orderbook.200.BTCUSDT",
                "delta",
                101,
                r#"[["60000.10","0"]]"#,
                r#"[]"#,
            ))
            .expect("deletion applies");
        assert_eq!(deletion.events.len(), 1);
        assert_eq!(deletion.events[0].values.get("size"), Some(&json!("0")));
    }

    #[test]
    fn orderbook_cursors_are_per_topic_independent() {
        let mut adapter = spot();
        adapter
            .ingest(&connected(CaptureStream::BybitSpot))
            .expect("connects");
        // Seed only the deep book.
        adapter
            .ingest(&book_message(
                2,
                "orderbook.200.BTCUSDT",
                "snapshot",
                100,
                r#"[["60000.10","1.0"]]"#,
                r#"[["60001.20","1.0"]]"#,
            ))
            .expect("deep snapshot");
        // A bbo delta is still premature: the deep-book snapshot did not seed it.
        let bbo_premature = adapter
            .ingest(&book_message(
                3,
                "orderbook.1.BTCUSDT",
                "delta",
                100,
                r#"[["60000.10","1.0"]]"#,
                r#"[["60001.20","1.0"]]"#,
            ))
            .expect("bbo delta is operational");
        assert!(bbo_premature.events.is_empty());
        assert_eq!(
            bbo_premature.notices[0].code,
            FeedNoticeCode::AwaitingSnapshot
        );
        // The bbo snapshot seeds its own cursor and emits to the bbo source.
        let bbo_snapshot = adapter
            .ingest(&book_message(
                4,
                "orderbook.1.BTCUSDT",
                "snapshot",
                40,
                r#"[["60000.10","1.0"]]"#,
                r#"[["60001.20","1.0"]]"#,
            ))
            .expect("bbo snapshot");
        assert_eq!(bbo_snapshot.events.len(), 2);
        assert_eq!(
            bbo_snapshot.events[0].source_id,
            "bybit.spot.btcusdt.bbo-levels"
        );
        // The deep book's cursor was untouched by the bbo traffic: its next delta
        // still applies against u=100.
        let deep_delta = adapter
            .ingest(&book_message(
                5,
                "orderbook.200.BTCUSDT",
                "delta",
                101,
                r#"[["60000.10","2.0"]]"#,
                r#"[]"#,
            ))
            .expect("deep delta applies");
        assert_eq!(deep_delta.events.len(), 1);
        assert_eq!(
            deep_delta.events[0].source_id,
            "bybit.spot.btcusdt.book-levels"
        );
    }

    #[test]
    fn connect_resets_per_epoch_ordering_state() {
        let mut adapter = spot();
        adapter
            .ingest(&connected(CaptureStream::BybitSpot))
            .expect("connects");
        adapter
            .ingest(&trade_message(2, "uuid-a", "Buy", "60000.00", 500))
            .expect("trade before reconnect");
        adapter
            .ingest(&book_message(
                3,
                "orderbook.200.BTCUSDT",
                "snapshot",
                100,
                r#"[["60000.10","1.0"]]"#,
                r#"[["60001.20","1.0"]]"#,
            ))
            .expect("snapshot before reconnect");
        let disconnected = RawRecord {
            connection_epoch: 1,
            ordinal: 4,
            kind: RecordKind::Disconnected,
            payload: None,
            ..connected(CaptureStream::BybitSpot)
        };
        adapter.ingest(&disconnected).expect("disconnects");
        let reconnect = RawRecord {
            connection_epoch: 2,
            ordinal: 5,
            ..connected(CaptureStream::BybitSpot)
        };
        adapter.ingest(&reconnect).expect("reconnects");
        // A lower trade seq after the reset is a new epoch, not a regression.
        let after_trade = RawRecord {
            connection_epoch: 2,
            ..trade_message(6, "uuid-b", "Buy", "60000.00", 1)
        };
        let trade = adapter.ingest(&after_trade).expect("post-reset trade");
        assert_eq!(trade.events.len(), 1);
        assert!(trade.notices.is_empty());
        // The book cursor was cleared, so a delta is premature until a snapshot.
        let after_delta = RawRecord {
            connection_epoch: 2,
            ..book_message(
                7,
                "orderbook.200.BTCUSDT",
                "delta",
                200,
                r#"[["60000.10","1.0"]]"#,
                r#"[]"#,
            )
        };
        let delta = adapter.ingest(&after_delta).expect("post-reset delta");
        assert!(delta.events.is_empty());
        assert_eq!(delta.notices[0].code, FeedNoticeCode::AwaitingSnapshot);
    }

    #[test]
    fn liquidation_emits_with_bankruptcy_price_field() {
        let mut adapter = linear();
        adapter
            .ingest(&connected(CaptureStream::BybitLinear))
            .expect("connects");
        let outcome = adapter
            .ingest(&record(
                CaptureStream::BybitLinear,
                2,
                r#"{"topic":"allLiquidation.BTCUSDT","type":"snapshot","ts":1739502303204,"data":[{"T":1739502302929,"s":"BTCUSDT","S":"Sell","v":"20000","p":"0.04499"}]}"#,
            ))
            .expect("liquidation applies");
        assert_eq!(outcome.events.len(), 1);
        assert_eq!(
            outcome.events[0].source_id,
            "bybit.linear.btcusdt.liquidations"
        );
        assert_eq!(outcome.events[0].values.get("side"), Some(&json!("sell")));
        assert_eq!(
            outcome.events[0].values.get("bankruptcy_price"),
            Some(&json!("0.04499"))
        );
        assert!(!outcome.events[0].values.contains_key("price"));
    }

    #[test]
    fn liquidation_event_time_outside_the_window_is_rejected() {
        let mut adapter = linear();
        adapter
            .ingest(&connected(CaptureStream::BybitLinear))
            .expect("connects");
        // A liquidation T read as microseconds is ~1000x too large.
        let micros = record(
            CaptureStream::BybitLinear,
            2,
            r#"{"topic":"allLiquidation.BTCUSDT","type":"snapshot","ts":1739502303204,"data":[{"T":1739502302929000,"s":"BTCUSDT","S":"Sell","v":"20000","p":"0.04499"}]}"#,
        );
        assert!(adapter.ingest(&micros).is_err());
    }

    #[test]
    fn microsecond_scale_timestamps_are_rejected() {
        let mut adapter = spot();
        adapter
            .ingest(&connected(CaptureStream::BybitSpot))
            .expect("connects");
        // A trade T read as microseconds is ~1000x too large.
        let trade = record(
            CaptureStream::BybitSpot,
            2,
            r#"{"topic":"publicTrade.BTCUSDT","type":"snapshot","ts":1787918400123,"data":[{"T":1787918400100000,"s":"BTCUSDT","S":"Buy","v":"0.001","p":"60000.50","i":"uuid","BT":false,"seq":100}]}"#,
        );
        assert!(adapter.ingest(&trade).is_err());
        // An orderbook cts read as microseconds is rejected before cursor logic.
        let book = record(
            CaptureStream::BybitSpot,
            3,
            r#"{"topic":"orderbook.200.BTCUSDT","type":"snapshot","ts":1787918400123,"data":{"s":"BTCUSDT","b":[["60000.10","1.0"]],"a":[],"u":100,"seq":1},"cts":1787918400120000}"#,
        );
        assert!(adapter.ingest(&book).is_err());
    }

    #[test]
    fn replay_is_deterministic_over_a_mixed_spot_sequence() {
        let fixture = vec![
            connected(CaptureStream::BybitSpot),
            book_message(
                2,
                "orderbook.200.BTCUSDT",
                "snapshot",
                100,
                r#"[["60000.10","1.0"]]"#,
                r#"[["60001.20","2.0"]]"#,
            ),
            trade_message(3, "uuid-a", "Buy", "60000.50", 100),
            book_message(
                4,
                "orderbook.1.BTCUSDT",
                "snapshot",
                40,
                r#"[["60000.10","1.0"]]"#,
                r#"[["60001.20","1.0"]]"#,
            ),
            book_message(
                5,
                "orderbook.200.BTCUSDT",
                "delta",
                101,
                r#"[["60000.10","0"]]"#,
                r#"[]"#,
            ),
            trade_message(6, "uuid-b", "Sell", "60001.00", 100),
        ];

        let run = || {
            let mut adapter = spot();
            let mut events = Vec::new();
            for record in &fixture {
                let IngestOutcome { events: batch, .. } =
                    adapter.ingest(record).expect("record ingests");
                events.extend(batch);
            }
            serde_json::to_string(&events).expect("events serialize")
        };
        assert_eq!(run(), run());
    }

    // ---- Linear ticker (id 5) ----

    const TICKER_SNAPSHOT_DATA: &str = r#"{"symbol":"BTCUSDT","tickDirection":"ZeroPlusTick","price24hPcnt":"0.005","lastPrice":"60000.00","prevPrice24h":"59600.00","highPrice24h":"60500.00","lowPrice24h":"59000.00","prevPrice1h":"60100.00","markPrice":"60000.50","indexPrice":"60000.20","openInterest":"12345.67","openInterestValue":"740740200.00","turnover24h":"1234567.0","volume24h":"20.5","fundingIntervalHour":"8","fundingCap":"0.02","fundingRate":"-0.00012","nextFundingTime":"1739505600000","bid1Price":"59999.90","bid1Size":"1.5","ask1Price":"60000.10","ask1Size":"2.0","curPreListingPhase":"Finished","preOpenPrice":"0","preQty":"0","singleOpenInterest":"6172.83","singleOpenInterestValue":"370370100.00"}"#;

    fn linear_record(ordinal: u64, payload: &str) -> RawRecord {
        record(CaptureStream::BybitLinear, ordinal, payload)
    }

    fn ticker(ordinal: u64, kind: &str, cs: u64, ts: i64, data: &str) -> RawRecord {
        linear_record(
            ordinal,
            &format!(
                r#"{{"topic":"tickers.BTCUSDT","type":"{kind}","ts":{ts},"cs":{cs},"data":{data}}}"#
            ),
        )
    }

    fn linear_connected() -> RawRecord {
        connected(CaptureStream::BybitLinear)
    }

    #[test]
    fn ticker_snapshot_seeds_state_and_emits_every_declared_field() {
        let mut adapter = linear();
        adapter.ingest(&linear_connected()).expect("connects");
        let out = adapter
            .ingest(&ticker(
                2,
                "snapshot",
                100,
                1_739_502_303_804,
                TICKER_SNAPSHOT_DATA,
            ))
            .expect("snapshot applies");
        assert_eq!(out.events.len(), 1);
        let values = &out.events[0].values;
        assert_eq!(out.events[0].source_id, "bybit.linear.btcusdt.tickers");
        for field in [
            "instrument",
            "update_kind",
            "last_price",
            "mark_price",
            "index_price",
            "funding_rate",
            "next_funding_time",
            "open_interest_base",
            "open_interest_value_usd",
            "best_bid_price",
            "best_bid_size",
            "best_ask_price",
            "best_ask_size",
            "cross_sequence",
            "event_time",
        ] {
            assert!(values.contains_key(field), "missing {field}");
        }
        assert_eq!(values.len(), 15, "exactly the declared fields");
        assert_eq!(values.get("update_kind"), Some(&json!("snapshot")));
        assert_eq!(values.get("mark_price"), Some(&json!("60000.50")));
        assert_eq!(values.get("open_interest_base"), Some(&json!("12345.67")));
        assert_eq!(values.get("cross_sequence"), Some(&json!(100)));
        assert_eq!(
            values.get("next_funding_time"),
            Some(&json!("2025-02-14T04:00:00.000Z"))
        );
    }

    #[test]
    fn ticker_delta_merges_present_keys_and_retains_others() {
        let mut adapter = linear();
        adapter.ingest(&linear_connected()).expect("connects");
        adapter
            .ingest(&ticker(
                2,
                "snapshot",
                100,
                1_739_502_303_804,
                TICKER_SNAPSHOT_DATA,
            ))
            .expect("snapshot");
        let out = adapter
            .ingest(&ticker(
                3,
                "delta",
                101,
                1_739_502_303_904,
                r#"{"symbol":"BTCUSDT","markPrice":"60001.00"}"#,
            ))
            .expect("delta merges");
        assert_eq!(out.events.len(), 1);
        let values = &out.events[0].values;
        // The changed key is folded; every other key retains its snapshot value.
        assert_eq!(values.get("mark_price"), Some(&json!("60001.00")));
        assert_eq!(values.get("index_price"), Some(&json!("60000.20")));
        assert_eq!(values.get("last_price"), Some(&json!("60000.00")));
        assert_eq!(values.get("update_kind"), Some(&json!("delta")));
        assert_eq!(values.get("cross_sequence"), Some(&json!(101)));
    }

    #[test]
    fn ticker_cs_equal_is_duplicate_lower_is_out_of_order_state_unchanged() {
        let mut adapter = linear();
        adapter.ingest(&linear_connected()).expect("connects");
        adapter
            .ingest(&ticker(
                2,
                "snapshot",
                100,
                1_739_502_303_804,
                TICKER_SNAPSHOT_DATA,
            ))
            .expect("snapshot");
        let duplicate = adapter
            .ingest(&ticker(
                3,
                "delta",
                100,
                1_739_502_303_904,
                r#"{"markPrice":"99999.00"}"#,
            ))
            .expect("equal cs is operational");
        assert!(duplicate.events.is_empty());
        assert_eq!(duplicate.notices[0].code, FeedNoticeCode::Duplicate);
        let stale = adapter
            .ingest(&ticker(
                4,
                "delta",
                99,
                1_739_502_303_914,
                r#"{"markPrice":"88888.00"}"#,
            ))
            .expect("lower cs is operational");
        assert!(stale.events.is_empty());
        assert_eq!(stale.notices[0].code, FeedNoticeCode::OutOfOrder);
        // Neither skipped delta changed the state: a following higher cs still
        // carries the untouched snapshot mark price for every retained key.
        let advance = adapter
            .ingest(&ticker(
                5,
                "delta",
                101,
                1_739_502_303_924,
                r#"{"lastPrice":"60002.00"}"#,
            ))
            .expect("advance applies");
        assert_eq!(
            advance.events[0].values.get("mark_price"),
            Some(&json!("60000.50"))
        );
    }

    #[test]
    fn ticker_delta_before_snapshot_awaits_snapshot_no_event() {
        let mut adapter = linear();
        adapter.ingest(&linear_connected()).expect("connects");
        let out = adapter
            .ingest(&ticker(
                2,
                "delta",
                100,
                1_739_502_303_804,
                r#"{"markPrice":"60001.00"}"#,
            ))
            .expect("premature delta is operational");
        assert!(out.events.is_empty());
        assert_eq!(out.notices[0].code, FeedNoticeCode::AwaitingSnapshot);
        assert!(out.notices[0].recovery.is_some());
    }

    #[test]
    fn ticker_reconnect_voids_state_next_delta_is_premature() {
        let mut adapter = linear();
        adapter.ingest(&linear_connected()).expect("connects");
        adapter
            .ingest(&ticker(
                2,
                "snapshot",
                100,
                1_739_502_303_804,
                TICKER_SNAPSHOT_DATA,
            ))
            .expect("snapshot");
        let disconnected = RawRecord {
            connection_epoch: 1,
            ordinal: 3,
            kind: RecordKind::Disconnected,
            payload: None,
            ..linear_connected()
        };
        adapter.ingest(&disconnected).expect("disconnects");
        let reconnect = RawRecord {
            connection_epoch: 2,
            ordinal: 4,
            ..linear_connected()
        };
        adapter.ingest(&reconnect).expect("reconnects");
        let premature = RawRecord {
            connection_epoch: 2,
            ..ticker(
                5,
                "delta",
                101,
                1_739_502_303_904,
                r#"{"markPrice":"60001.00"}"#,
            )
        };
        let out = adapter.ingest(&premature).expect("post-reset delta");
        assert!(out.events.is_empty());
        assert_eq!(out.notices[0].code, FeedNoticeCode::AwaitingSnapshot);
    }

    #[test]
    fn ticker_negative_funding_rate_admitted_negative_mark_price_refused() {
        let mut adapter = linear();
        adapter.ingest(&linear_connected()).expect("connects");
        // The snapshot data carries fundingRate -0.00012: admitted and projected
        // with its sign, because funding is a signed period rate.
        let out = adapter
            .ingest(&ticker(
                2,
                "snapshot",
                100,
                1_739_502_303_804,
                TICKER_SNAPSHOT_DATA,
            ))
            .expect("negative funding admitted");
        assert_eq!(
            out.events[0].values.get("funding_rate"),
            Some(&json!("-0.00012"))
        );
        // A negative mark price is refused: the non-negative guard applies to
        // every decimal except funding_rate.
        let bad = ticker(
            3,
            "snapshot",
            101,
            1_739_502_303_904,
            &TICKER_SNAPSHOT_DATA.replace(r#""markPrice":"60000.50""#, r#""markPrice":"-1.00""#),
        );
        assert!(adapter.ingest(&bad).is_err());
    }

    #[test]
    fn ticker_partial_snapshot_is_invalid_payload() {
        let mut adapter = linear();
        adapter.ingest(&linear_connected()).expect("connects");
        // A snapshot missing a projected key (indexPrice) is not a snapshot.
        let partial = ticker(
            2,
            "snapshot",
            100,
            1_739_502_303_804,
            &TICKER_SNAPSHOT_DATA.replace(r#","indexPrice":"60000.20""#, ""),
        );
        assert!(adapter.ingest(&partial).is_err());
    }

    #[test]
    fn ticker_unknown_key_is_refused() {
        let mut adapter = linear();
        adapter.ingest(&linear_connected()).expect("connects");
        let drift = ticker(
            2,
            "snapshot",
            100,
            1_739_502_303_804,
            &TICKER_SNAPSHOT_DATA.replace('{', r#"{"unknownKey":"x","#),
        );
        assert!(adapter.ingest(&drift).is_err());
    }

    // ---- Linear orderbook.50 (id 6) ----

    fn linear_book(
        ordinal: u64,
        kind: &str,
        u: u64,
        bids: &str,
        asks: &str,
        cts: Option<i64>,
    ) -> RawRecord {
        let cts = cts
            .map(|value| format!(r#","cts":{value}"#))
            .unwrap_or_default();
        linear_record(
            ordinal,
            &format!(
                r#"{{"topic":"orderbook.50.BTCUSDT","type":"{kind}","ts":1739502304004,"data":{{"s":"BTCUSDT","b":{bids},"a":{asks},"u":{u},"seq":7961700000}}{cts}}}"#
            ),
        )
    }

    #[test]
    fn orderbook_50_cursor_independent_of_200_and_1() {
        // The perp adapter only subscribes orderbook.50; seed it and confirm the
        // spot depths are refused as unsupported topics for this stream (their
        // cursors do not exist here), while the 50 cursor advances on its own.
        let mut adapter = linear();
        adapter.ingest(&linear_connected()).expect("connects");
        let snapshot = adapter
            .ingest(&linear_book(
                2,
                "snapshot",
                200,
                r#"[["59999.90","1.0"]]"#,
                r#"[["60000.10","2.0"]]"#,
                Some(1_739_502_304_000),
            ))
            .expect("book50 snapshot");
        assert_eq!(snapshot.events.len(), 2);
        assert_eq!(
            snapshot.events[0].source_id,
            "bybit.linear.btcusdt.book-levels"
        );
        // A spot depth topic is not in the linear set: unsupported, no reseed.
        let foreign = adapter
            .ingest(&record(
                CaptureStream::BybitLinear,
                3,
                r#"{"topic":"orderbook.200.BTCUSDT","type":"delta","ts":1739502304104,"data":{"s":"BTCUSDT","b":[["1.0","1.0"]],"a":[],"u":300,"seq":1},"cts":1739502304100}"#,
            ))
            .expect("foreign topic operational");
        assert_eq!(foreign.notices[0].code, FeedNoticeCode::UnsupportedMessage);
        // The 50 cursor is untouched by that foreign frame: its delta applies.
        let delta = adapter
            .ingest(&linear_book(
                4,
                "delta",
                201,
                r#"[["59999.80","0"]]"#,
                r#"[]"#,
                Some(1_739_502_304_100),
            ))
            .expect("book50 delta applies");
        assert_eq!(delta.events.len(), 1);
    }

    #[test]
    fn orderbook_50_event_time_falls_back_to_receipt_when_cts_absent() {
        let mut adapter = linear();
        adapter.ingest(&linear_connected()).expect("connects");
        let out = adapter
            .ingest(&linear_book(
                2,
                "snapshot",
                200,
                r#"[["59999.90","1.0"]]"#,
                r#"[]"#,
                None,
            ))
            .expect("cts-absent snapshot applies");
        assert_eq!(
            out.events[0].values.get("event_time_basis"),
            Some(&json!("receipt"))
        );
        assert_eq!(
            out.events[0].values.get("event_time"),
            Some(&json!("2026-08-28T12:00:01Z"))
        );
    }

    // ---- Linear trades (id 7) ----

    fn linear_trade(ordinal: u64, extra: &str) -> RawRecord {
        linear_record(
            ordinal,
            &format!(
                r#"{{"topic":"publicTrade.BTCUSDT","type":"snapshot","ts":1739502304204,"data":[{{"T":1739502304200,"s":"BTCUSDT","S":"Buy","v":"0.5","p":"60000.00","i":"2290000001203031348","BT":false,"RPI":false{extra},"seq":600}}]}}"#
            ),
        )
    }

    #[test]
    fn linear_trade_with_l_emits_tick_direction() {
        let mut adapter = linear();
        adapter.ingest(&linear_connected()).expect("connects");
        let out = adapter
            .ingest(&linear_trade(2, r#","L":"PlusTick""#))
            .expect("linear trade with L");
        assert_eq!(out.events.len(), 1);
        assert_eq!(out.events[0].source_id, "bybit.linear.btcusdt.trades");
        assert_eq!(
            out.events[0].values.get("tick_direction"),
            Some(&json!("plus_tick"))
        );
        assert_eq!(
            out.events[0].values.get("aggressor_side"),
            Some(&json!("buy"))
        );
    }

    #[test]
    fn linear_trade_without_l_skips_with_a_notice_and_keeps_ingesting() {
        let mut adapter = linear();
        adapter.ingest(&linear_connected()).expect("connects");
        // No L on the linear tape: the anomalous fill is skipped with a notice
        // rather than aborting the whole stream, so ingest does NOT error and
        // emits no normalized output for that fill.
        let out = adapter
            .ingest(&linear_trade(2, ""))
            .expect("an L-less linear fill is skipped, not a hard error");
        assert!(out.events.is_empty());
        assert_eq!(out.notices[0].code, FeedNoticeCode::StateInconsistent);
        // A subsequent well-formed linear trade still applies: the earlier skip
        // did not tear down the adapter.
        let recovered = adapter
            .ingest(&linear_trade(3, r#","L":"PlusTick""#))
            .expect("a later valid fill still applies");
        assert_eq!(recovered.events.len(), 1);
        assert_eq!(
            recovered.events[0].values.get("tick_direction"),
            Some(&json!("plus_tick"))
        );
    }

    #[test]
    fn spot_trade_without_l_still_admitted() {
        let mut adapter = spot();
        adapter
            .ingest(&connected(CaptureStream::BybitSpot))
            .expect("connects");
        // The spot tape never carries L; the same struct parses it fine and the
        // spot path never projects a tick_direction.
        let out = adapter
            .ingest(&trade_message(2, "uuid-a", "Buy", "60000.50", 100))
            .expect("spot trade without L");
        assert_eq!(out.events.len(), 1);
        assert!(!out.events[0].values.contains_key("tick_direction"));
    }

    #[test]
    fn partial_subscription_ack_is_kept_not_reconnected() {
        let mut adapter = linear();
        adapter.ingest(&linear_connected()).expect("connects");
        // An ack echoing only 3 of the 4 linear args is NOT proof of rejection:
        // Bybit does not document echo-all, so the subset is reported as
        // informational and the live subscription is kept — no Reconnect. A
        // genuinely dropped topic is caught by the connection silence bound.
        let partial = adapter
            .ingest(&linear_record(
                2,
                r#"{"success":true,"ret_msg":"subscribe","conn_id":"c1","req_id":"1","op":"subscribe","args":["allLiquidation.BTCUSDT","tickers.BTCUSDT","orderbook.50.BTCUSDT"]}"#,
            ))
            .expect("partial ack is operational");
        assert!(partial.events.is_empty());
        assert_eq!(
            partial.notices[0].code,
            FeedNoticeCode::SubscriptionAccepted
        );
        assert!(
            partial.notices[0].recovery.is_none(),
            "a subset echo must not force a reconnect"
        );
        // The full 4-arg echo is a whole-frame acceptance.
        let full = adapter
            .ingest(&linear_record(
                3,
                r#"{"success":true,"ret_msg":"subscribe","op":"subscribe","args":["allLiquidation.BTCUSDT","tickers.BTCUSDT","orderbook.50.BTCUSDT","publicTrade.BTCUSDT"]}"#,
            ))
            .expect("full ack");
        assert_eq!(full.notices[0].code, FeedNoticeCode::SubscriptionAccepted);
        assert!(full.notices[0].recovery.is_none());
        // An ack without an args echo (the observed shape) is accepted.
        let observed = adapter
            .ingest(&linear_record(
                4,
                r#"{"success":true,"ret_msg":"subscribe","conn_id":"c1","req_id":"1","op":"subscribe"}"#,
            ))
            .expect("observed ack");
        assert_eq!(
            observed.notices[0].code,
            FeedNoticeCode::SubscriptionAccepted
        );
        // success:false is still a failed connection.
        let rejected = adapter
            .ingest(&linear_record(
                5,
                r#"{"success":false,"ret_msg":"Invalid topic","op":"subscribe"}"#,
            ))
            .expect("rejection");
        assert_eq!(
            rejected.notices[0].code,
            FeedNoticeCode::SubscriptionRejected
        );
        assert!(matches!(
            rejected.notices[0].recovery,
            Some(RecoveryAction::Reconnect)
        ));
    }

    #[test]
    fn linear_pong_frame_is_a_heartbeat_not_a_subscription_rejection() {
        // Bybit answers the client ping on linear/inverse with `op:pong` carrying
        // `args:["<ts ms>"]`. That args echo names no topic, so the old
        // unconditional subset gate turned every 20 s heartbeat into a
        // SubscriptionRejected + Reconnect on a fully-subscribed socket.
        let mut adapter = linear();
        adapter.ingest(&linear_connected()).expect("connects");
        let pong = adapter
            .ingest(&linear_record(
                2,
                r#"{"success":true,"ret_msg":"pong","op":"pong","conn_id":"c1","args":["1739502303204"]}"#,
            ))
            .expect("a pong is operational");
        assert!(pong.events.is_empty());
        assert_eq!(pong.notices[0].code, FeedNoticeCode::Heartbeat);
        assert!(pong.notices[0].recovery.is_none());
    }

    #[test]
    fn liquidation_side_mapping_pinned_to_v1_outcome() {
        // The shipped adapter reads `S` as the POSITION side: Buy = a long was
        // liquidated, Sell = a short was liquidated (bybit.rs `taker_side` doc).
        // This reading is settled by the R1 capture (V1); until it passes,
        // `btc.bybit.liquidation_pressure` side skew is non-ready and no signal
        // may key on this field. This test pins the mapping the definition
        // states, not the (still non-ready) semantic.
        let mut adapter = linear();
        adapter.ingest(&linear_connected()).expect("connects");
        let out = adapter
            .ingest(&record(
                CaptureStream::BybitLinear,
                2,
                r#"{"topic":"allLiquidation.BTCUSDT","type":"snapshot","ts":1739502303204,"data":[{"T":1739502302929,"s":"BTCUSDT","S":"Buy","v":"0.007","p":"77510.40"},{"T":1739502302930,"s":"BTCUSDT","S":"Sell","v":"0.008","p":"77500.00"}]}"#,
            ))
            .expect("liquidations apply");
        assert_eq!(out.events.len(), 2);
        assert_eq!(out.events[0].values.get("side"), Some(&json!("buy")));
        assert_eq!(out.events[1].values.get("side"), Some(&json!("sell")));
    }

    // ---- REST poll identities (ids 8, 9) ----

    fn rest() -> BybitRestAdapter {
        BybitRestAdapter::new("test-epoch").expect("rest adapter")
    }

    fn rest_record(ordinal: u64, payload: &str) -> RawRecord {
        record(CaptureStream::BybitLinearRest, ordinal, payload)
    }

    fn oi_body(oi: &str, ts: &str) -> String {
        format!(
            r#"{{"retCode":0,"retMsg":"OK","result":{{"symbol":"BTCUSDT","category":"linear","list":[{{"openInterest":"{oi}","singleOpenInterest":"6172.83","timestamp":"{ts}"}}],"nextPageCursor":""}},"retExtInfo":{{}},"time":1739502303204}}"#
        )
    }

    #[test]
    fn rest_retcode_and_empty_list_are_nonfatal_wrong_symbol_and_oversize_refused() {
        let mut adapter = rest();
        adapter
            .ingest(&connected(CaptureStream::BybitLinearRest))
            .expect("connects");
        // Non-zero retCode (10006 rate-limit, served with HTTP 200) is non-fatal:
        // no events, a StateInconsistent notice, and replay does not abort.
        let retcode = adapter
            .ingest(&rest_record(
                2,
                r#"{"retCode":10006,"retMsg":"Too many visits!","result":{"symbol":"BTCUSDT","list":[]}}"#,
            ))
            .expect("a non-zero retCode is non-fatal");
        assert!(retcode.events.is_empty());
        assert_eq!(retcode.notices[0].code, FeedNoticeCode::StateInconsistent);
        // An empty list is non-fatal too (funding legitimately returns none).
        let empty = adapter
            .ingest(&rest_record(
                3,
                r#"{"retCode":0,"retMsg":"OK","result":{"symbol":"BTCUSDT","list":[]}}"#,
            ))
            .expect("an empty list is non-fatal");
        assert!(empty.events.is_empty());
        assert_eq!(empty.notices[0].code, FeedNoticeCode::StateInconsistent);
        // A valid open-interest row after the retCode body still normalizes.
        let served = adapter
            .ingest(&rest_record(4, &oi_body("12345.67", "1739502300000")))
            .expect("the served row after a retCode still normalizes");
        assert_eq!(served.events.len(), 1);
        // Wrong symbol on an open-interest response is a genuine contract
        // violation: refused.
        assert!(
            adapter
                .ingest(&rest_record(
                    5,
                    r#"{"retCode":0,"retMsg":"OK","result":{"symbol":"ETHUSDT","list":[{"openInterest":"1","timestamp":"1739502300000"}]}}"#,
                ))
                .is_err()
        );
        // Oversize body is refused, never partially parsed.
        let huge = format!(
            r#"{{"retCode":0,"retMsg":"{}","result":{{"symbol":"BTCUSDT","list":[{{"openInterest":"1","timestamp":"1739502300000"}}]}}}}"#,
            "x".repeat(70 * 1024)
        );
        assert!(adapter.ingest(&rest_record(6, &huge)).is_err());
    }

    #[test]
    fn rest_repeated_event_time_is_duplicate_new_bucket_emits() {
        let mut adapter = rest();
        adapter
            .ingest(&connected(CaptureStream::BybitLinearRest))
            .expect("connects");
        let first = adapter
            .ingest(&rest_record(2, &oi_body("12345.67", "1739502300000")))
            .expect("first bucket");
        assert_eq!(first.events.len(), 1);
        assert_eq!(
            first.events[0].source_id,
            "bybit.linear.btcusdt.open-interest"
        );
        assert_eq!(first.events[0].values.get("interval"), Some(&json!("5min")));
        // The venue republishes the current bucket until it closes: a repeat of
        // the admitted event_time is a Duplicate with no event.
        let repeat = adapter
            .ingest(&rest_record(3, &oi_body("12346.00", "1739502300000")))
            .expect("repeat bucket");
        assert!(repeat.events.is_empty());
        assert_eq!(repeat.notices[0].code, FeedNoticeCode::Duplicate);
        // A new bucket close emits.
        let next = adapter
            .ingest(&rest_record(4, &oi_body("12350.00", "1739502600000")))
            .expect("new bucket");
        assert_eq!(next.events.len(), 1);
        // Funding rows dedup independently and carry a signed rate.
        let funding = adapter
            .ingest(&rest_record(
                5,
                r#"{"retCode":0,"retMsg":"OK","result":{"category":"linear","list":[{"symbol":"BTCUSDT","fundingRate":"-0.00012","fundingRateTimestamp":"1739502000000"}]}}"#,
            ))
            .expect("funding row");
        assert_eq!(funding.events.len(), 1);
        assert_eq!(
            funding.events[0].source_id,
            "bybit.linear.btcusdt.funding-history"
        );
        assert_eq!(
            funding.events[0].values.get("funding_rate"),
            Some(&json!("-0.00012"))
        );
    }

    #[test]
    fn rest_multi_row_open_interest_dedups_against_the_newest_high_water_mark() {
        let mut adapter = rest();
        adapter
            .ingest(&connected(CaptureStream::BybitLinearRest))
            .expect("connects");
        // A newest-first list of three distinct buckets: all are new, all emit.
        let poll1 = r#"{"retCode":0,"retMsg":"OK","result":{"symbol":"BTCUSDT","category":"linear","list":[{"openInterest":"30","singleOpenInterest":"6","timestamp":"1739502900000"},{"openInterest":"20","singleOpenInterest":"6","timestamp":"1739502600000"},{"openInterest":"10","singleOpenInterest":"6","timestamp":"1739502300000"}],"nextPageCursor":""},"retExtInfo":{},"time":1739502903204}"#;
        let first = adapter.ingest(&rest_record(2, poll1)).expect("first poll");
        assert_eq!(first.events.len(), 3);
        // The next poll re-lists the three seen buckets plus one newer bucket.
        // Only the new one emits; the three already admitted are duplicates. The
        // old single-slot dedup would have re-emitted the two buckets newer than
        // the oldest.
        let poll2 = r#"{"retCode":0,"retMsg":"OK","result":{"symbol":"BTCUSDT","category":"linear","list":[{"openInterest":"40","singleOpenInterest":"6","timestamp":"1739503200000"},{"openInterest":"30","singleOpenInterest":"6","timestamp":"1739502900000"},{"openInterest":"20","singleOpenInterest":"6","timestamp":"1739502600000"},{"openInterest":"10","singleOpenInterest":"6","timestamp":"1739502300000"}],"nextPageCursor":""},"retExtInfo":{},"time":1739503203204}"#;
        let second = adapter.ingest(&rest_record(3, poll2)).expect("second poll");
        assert_eq!(second.events.len(), 1);
        assert_eq!(second.notices.len(), 3);
        assert!(
            second
                .notices
                .iter()
                .all(|notice| notice.code == FeedNoticeCode::Duplicate)
        );
    }
}
