//! Coinbase Exchange BTC-USD adapter: trades, ticker, level-two book and the
//! product status channel, plus the fail-closed trade-gap recovery loop that
//! [`CoinbaseAdapter::apply_trade_backfill`] closes. The `ticker` channel is
//! also carried for USDT-USD — the USDT/USD reference stream of ruling E47 —
//! routed by product id; every other channel stays BTC-USD only.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::{Value, json};
use signals_core::SourceEvent;

use crate::adapter::{
    GuardAction, RecordGuard, bounded_token, validate_publisher_epoch, validate_timestamp,
};
use crate::decimal::validate_decimal;
use crate::{
    AdapterError, CaptureStream, FeedAdapter, FeedNotice, FeedNoticeCode, IngestOutcome, Provider,
    RawRecord, RecoveryAction,
};

const PRODUCT_ID: &str = "BTC-USD";
const USDT_PRODUCT_ID: &str = "USDT-USD";
const TRADE_SOURCE_VERSION: &str = "1.0.0";
const TICKER_SOURCE_VERSION: &str = "1.0.0";
const BOOK_SOURCE_VERSION: &str = "1.0.0";
const PRODUCT_SOURCE_VERSION: &str = "1.0.0";
const TRADE_SOURCE_ID: &str = "coinbase.exchange.btc-usd.trades";
const TICKER_SOURCE_ID: &str = "coinbase.exchange.btc-usd.ticker";
const USDT_TICKER_SOURCE_ID: &str = "coinbase.exchange.usdt-usd.ticker";
const BOOK_SOURCE_ID: &str = "coinbase.exchange.btc-usd.book-levels";
const PRODUCT_SOURCE_ID: &str = "coinbase.exchange.btc-usd.product";
const MAX_BOOK_ROWS: usize = 100_000;
const MAX_STATUS_PRODUCTS: usize = 10_000;
const MAX_TRADE_BACKFILL: usize = 100_000;
const MAX_HELD_TRADES: usize = 100_000;

/// A validated live trade observed while trade publication is paused pending a
/// REST backfill, whose id is beyond the pending recovery range. It is held in
/// hand (never dropped) and emitted after the backfilled range completes.
struct HeldTrade {
    trade_id: u64,
    price: String,
    size: String,
    maker_side: String,
    event_time: String,
    received_at: String,
}

/// A trade range the venue has proven exists but the WebSocket tape never
/// delivered. While one is pending, trade publication is paused and every trade
/// message returns a [`RecoveryAction::BackfillCoinbaseTrades`] notice naming
/// exactly this range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PendingBackfill {
    /// Exclusive lower bound: the last trade id the adapter admitted.
    after_trade_id: u64,
    /// Inclusive upper bound: the first trade id proven to exist beyond the gap.
    through_trade_id: u64,
}

/// Which of the four Coinbase source identities an emitted event belongs to.
///
/// Each carries its own `sequence` counter, so the dispatch is a closed enum
/// rather than a match over source-id strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CoinbaseSource {
    Trade,
    Ticker,
    UsdtTicker,
    Book,
    Product,
}

impl CoinbaseSource {
    const fn source_id(self) -> &'static str {
        match self {
            Self::Trade => TRADE_SOURCE_ID,
            Self::Ticker => TICKER_SOURCE_ID,
            Self::UsdtTicker => USDT_TICKER_SOURCE_ID,
            Self::Book => BOOK_SOURCE_ID,
            Self::Product => PRODUCT_SOURCE_ID,
        }
    }

    const fn source_version(self) -> &'static str {
        match self {
            Self::Trade => TRADE_SOURCE_VERSION,
            Self::Ticker | Self::UsdtTicker => TICKER_SOURCE_VERSION,
            Self::Book => BOOK_SOURCE_VERSION,
            Self::Product => PRODUCT_SOURCE_VERSION,
        }
    }
}

/// Deterministic Coinbase parser and ordering state for the selected BTC-USD streams.
pub struct CoinbaseAdapter {
    publisher_epoch: String,
    guard: RecordGuard,
    next_trade_sequence: u64,
    next_btc_ticker_sequence: u64,
    next_usdt_ticker_sequence: u64,
    next_book_sequence: u64,
    next_product_sequence: u64,
    last_trade_id: Option<u64>,
    trade_gap: Option<PendingBackfill>,
    /// Trades observed beyond the pending recovery frontier while paused. Bounded
    /// by `MAX_HELD_TRADES`; overflow fails closed by widening the pending range.
    held_trades: Vec<HeldTrade>,
    /// Last accepted venue `sequence` per ticker product; the two ticker products
    /// carry independent Coinbase sequence spaces, so each has its own cursor.
    last_btc_ticker_sequence: Option<u64>,
    last_usdt_ticker_sequence: Option<u64>,
    book_snapshot_seen: bool,
}

impl CoinbaseAdapter {
    /// Construct a Coinbase Exchange adapter.
    ///
    /// `publisher_epoch` is copied verbatim into every emitted event; it must be
    /// non-blank and at most 128 bytes, or the call returns
    /// [`AdapterError::InvalidRecord`]. One adapter serves all three Coinbase
    /// streams; it normalizes only the message types its record's stream
    /// selects.
    pub fn new(publisher_epoch: impl Into<String>) -> Result<Self, AdapterError> {
        let publisher_epoch = publisher_epoch.into();
        validate_publisher_epoch(&publisher_epoch)?;
        Ok(Self {
            publisher_epoch,
            guard: RecordGuard::new(Provider::CoinbaseExchange),
            next_trade_sequence: 0,
            next_btc_ticker_sequence: 0,
            next_usdt_ticker_sequence: 0,
            next_book_sequence: 0,
            next_product_sequence: 0,
            last_trade_id: None,
            trade_gap: None,
            held_trades: Vec::new(),
            last_btc_ticker_sequence: None,
            last_usdt_ticker_sequence: None,
            book_snapshot_seen: false,
        })
    }

    /// Close a pending trade gap with a complete REST-recovered range.
    ///
    /// Call this — and only this — after an [`IngestOutcome`] carried a
    /// [`FeedNoticeCode::Gap`] notice whose recovery is
    /// [`RecoveryAction::BackfillCoinbaseTrades`]. Pass that notice's
    /// `after_trade_id` and `through_trade_id` back unchanged, `received_at` as
    /// the RFC 3339 instant the REST response was received, and every trade in
    /// the half-open range `(after_trade_id, through_trade_id]`.
    ///
    /// Until this succeeds the adapter publishes no trades at all: every live
    /// trade message returns the same recovery notice, and trades beyond the
    /// frontier are held in hand (never dropped) and emitted, in order, by this
    /// call. Ticker, book and product output are unaffected.
    ///
    /// If the range is wrong, nothing is published and the pending gap is left
    /// exactly as it was — the caller may simply retry with the right range:
    ///
    /// * a range that is not the pending one, or no gap pending at all, is
    ///   [`AdapterError::InvalidRecord`];
    /// * a set of trades that is short, non-contiguous, contains an unparseable
    ///   price/size/side/time, or exceeds the 100,000-trade recovery bound is
    ///   [`AdapterError::InvalidPayload`].
    ///
    /// Extra trades outside the range are discarded rather than rejected, so a
    /// REST page that overshoots the boundary is accepted.
    ///
    /// The returned outcome carries the recovered trades followed by any held
    /// trades that are contiguous with them; a hole inside the held run emits
    /// what is proven and raises a further [`RecoveryAction::BackfillCoinbaseTrades`]
    /// for the remainder rather than publishing a non-contiguous tape.
    ///
    /// This is a live-runner path: replay of a raw capture never calls it, and
    /// therefore reproduces the recorded tape without re-performing recovery.
    pub fn apply_trade_backfill(
        &mut self,
        after_trade_id: u64,
        through_trade_id: u64,
        received_at: &str,
        mut trades: Vec<CoinbaseRestTrade>,
    ) -> Result<IngestOutcome, AdapterError> {
        let Some(pending) = self.trade_gap else {
            return Err(AdapterError::InvalidRecord(
                "no Coinbase trade backfill is pending".to_owned(),
            ));
        };
        if (after_trade_id, through_trade_id) != (pending.after_trade_id, pending.through_trade_id)
        {
            return Err(AdapterError::InvalidRecord(format!(
                "backfill range {after_trade_id}..={through_trade_id} does not match required range {}..={}",
                pending.after_trade_id, pending.through_trade_id
            )));
        }
        validate_timestamp(received_at, "received_at")?;
        let expected_count = through_trade_id
            .checked_sub(after_trade_id)
            .and_then(|count| usize::try_from(count).ok())
            .ok_or_else(|| {
                AdapterError::InvalidPayload("Coinbase backfill range is invalid".to_owned())
            })?;
        if expected_count > MAX_TRADE_BACKFILL {
            return Err(AdapterError::InvalidPayload(format!(
                "Coinbase backfill requires {expected_count} trades; maximum is {MAX_TRADE_BACKFILL}"
            )));
        }
        trades
            .retain(|trade| trade.trade_id > after_trade_id && trade.trade_id <= through_trade_id);
        trades.sort_unstable_by_key(|trade| trade.trade_id);
        if trades.len() != expected_count {
            return Err(AdapterError::InvalidPayload(format!(
                "Coinbase backfill returned {} of {expected_count} required trades",
                trades.len()
            )));
        }
        for (offset, trade) in trades.iter().enumerate() {
            let expected_id = after_trade_id
                .checked_add(u64::try_from(offset).map_err(|_| {
                    AdapterError::InvalidPayload("Coinbase backfill offset overflowed".to_owned())
                })?)
                .and_then(|id| id.checked_add(1))
                .ok_or(AdapterError::SequenceExhausted)?;
            if trade.trade_id != expected_id {
                return Err(AdapterError::InvalidPayload(format!(
                    "Coinbase backfill is not contiguous at trade_id {expected_id}"
                )));
            }
            validate_rest_trade(trade)?;
        }
        // Pre-flight overflow check: prove the per-source sequence counter can
        // absorb the whole recovered range BEFORE any event is emitted, so a
        // backfill either publishes completely or not at all. The sum itself is
        // discarded; `source_event` performs the real increments.
        self.next_trade_sequence
            .checked_add(u64::try_from(trades.len()).map_err(|_| {
                AdapterError::InvalidPayload("Coinbase backfill length overflowed".to_owned())
            })?)
            .ok_or(AdapterError::SequenceExhausted)?;

        let mut events = Vec::with_capacity(trades.len());
        for trade in trades {
            events.push(self.trade_event(
                trade.trade_id,
                trade.price,
                trade.size,
                trade.side,
                trade.time,
                received_at.to_owned(),
            )?);
        }
        self.last_trade_id = Some(through_trade_id);
        self.trade_gap = None;
        self.drain_held_trades(through_trade_id, events)
    }

    /// Emit trades held beyond the recovery frontier during the pause, in order,
    /// after the backfilled range. Held ids at or below `through_trade_id` are
    /// deduplicated against the backfill and dropped. A discontinuity within the
    /// held run fails closed without discarding what is in hand: the unprocessed
    /// tail is re-held and a bounded REST recovery reopens over the ids strictly
    /// below the first re-held trade, so on completion the drain resumes the tail
    /// contiguously rather than emitting a non-contiguous tape.
    fn drain_held_trades(
        &mut self,
        through_trade_id: u64,
        mut events: Vec<SourceEvent>,
    ) -> Result<IngestOutcome, AdapterError> {
        if self.held_trades.is_empty() {
            return Ok(IngestOutcome {
                events,
                notices: Vec::new(),
            });
        }
        let mut held = std::mem::take(&mut self.held_trades);
        held.sort_unstable_by_key(|trade| trade.trade_id);
        held.dedup_by_key(|trade| trade.trade_id);
        // Trades the backfill already covered are duplicates of the recovered range.
        held.retain(|trade| trade.trade_id > through_trade_id);

        let mut last_trade_id = through_trade_id;
        let mut notices = Vec::new();
        let mut index = 0;
        while index < held.len() {
            let trade = &held[index];
            let expected = last_trade_id
                .checked_add(1)
                .ok_or(AdapterError::SequenceExhausted)?;
            if trade.trade_id != expected {
                // A hole inside the held run: nothing on hand proves the
                // intervening ids. What is in hand is never discarded, so put
                // the unprocessed tail back and reopen recovery only over the
                // ids strictly below the first re-held trade. On completion the
                // drain re-runs from `expected = through + 1`, resuming the tail
                // contiguously; a further hole recurses through this same path.
                let resume_from = trade.trade_id;
                // `resume_from > expected = last_trade_id + 1` (sorted, contiguous
                // to here), so `resume_from - 1 >= last_trade_id + 1`: non-empty.
                let through = resume_from - 1;
                self.trade_gap = Some(PendingBackfill {
                    after_trade_id: last_trade_id,
                    through_trade_id: through,
                });
                self.held_trades = held.split_off(index);
                notices.push(FeedNotice::recovery(
                    FeedNoticeCode::Gap,
                    format!("Coinbase held trades jumped from {last_trade_id} to {resume_from}"),
                    RecoveryAction::BackfillCoinbaseTrades {
                        product_id: PRODUCT_ID.to_owned(),
                        after_trade_id: last_trade_id,
                        through_trade_id: through,
                    },
                ));
                return Ok(IngestOutcome { events, notices });
            }
            let event = self.trade_event(
                trade.trade_id,
                trade.price.clone(),
                trade.size.clone(),
                trade.maker_side.clone(),
                trade.event_time.clone(),
                trade.received_at.clone(),
            )?;
            events.push(event);
            last_trade_id = trade.trade_id;
            self.last_trade_id = Some(last_trade_id);
            index += 1;
        }
        Ok(IngestOutcome { events, notices })
    }

    fn ingest_payload(
        &mut self,
        record: &RawRecord,
        payload: &str,
    ) -> Result<IngestOutcome, AdapterError> {
        let envelope: CoinbaseEnvelope = serde_json::from_str(payload).map_err(payload_error)?;
        match envelope.message_type.as_str() {
            "subscriptions" => Ok(IngestOutcome::notice(FeedNotice::info(
                FeedNoticeCode::SubscriptionAccepted,
                "Coinbase subscription acknowledged",
            ))),
            "error" => {
                let error: CoinbaseError = serde_json::from_str(payload).map_err(payload_error)?;
                Ok(IngestOutcome::notice(FeedNotice::info(
                    FeedNoticeCode::SubscriptionRejected,
                    bounded_detail("Coinbase subscription rejected", &error.message),
                )))
            }
            "heartbeat" => self.heartbeat(payload),
            "status" => self.product(record, payload),
            "match" | "last_match" if record.stream == CaptureStream::CoinbaseCore => {
                self.trade(record, payload)
            }
            "ticker" if record.stream == CaptureStream::CoinbaseCore => {
                self.ticker(record, payload)
            }
            "snapshot" if record.stream == CaptureStream::CoinbaseCore => {
                self.book_snapshot(record, payload)
            }
            "l2update" if record.stream == CaptureStream::CoinbaseCore => {
                self.book_update(record, payload)
            }
            other => Ok(IngestOutcome::notice(FeedNotice::info(
                FeedNoticeCode::UnsupportedMessage,
                format!(
                    "retained unsupported Coinbase message type `{}` without normalized output",
                    bounded_token(other)
                ),
            ))),
        }
    }

    fn heartbeat(&mut self, payload: &str) -> Result<IngestOutcome, AdapterError> {
        let heartbeat: CoinbaseHeartbeat = serde_json::from_str(payload).map_err(payload_error)?;
        require_product(&heartbeat.product_id)?;
        validate_timestamp(&heartbeat.time, "time")?;
        if self.trade_gap.is_none()
            && let Some(last_trade_id) = self.last_trade_id
            && heartbeat.last_trade_id > last_trade_id.saturating_add(1)
        {
            let through_trade_id = heartbeat.last_trade_id;
            self.trade_gap = Some(PendingBackfill {
                after_trade_id: last_trade_id,
                through_trade_id,
            });
            return Ok(IngestOutcome::notice(FeedNotice::recovery(
                FeedNoticeCode::Gap,
                format!(
                    "Coinbase heartbeat advanced from trade {last_trade_id} to {through_trade_id}"
                ),
                RecoveryAction::BackfillCoinbaseTrades {
                    product_id: PRODUCT_ID.to_owned(),
                    after_trade_id: last_trade_id,
                    through_trade_id,
                },
            )));
        }
        Ok(IngestOutcome::notice(FeedNotice::info(
            FeedNoticeCode::Heartbeat,
            "Coinbase heartbeat observed",
        )))
    }

    fn trade(&mut self, record: &RawRecord, payload: &str) -> Result<IngestOutcome, AdapterError> {
        let trade: CoinbaseTrade = serde_json::from_str(payload).map_err(payload_error)?;
        require_product(&trade.product_id)?;
        validate_timestamp(&trade.time, "time")?;
        validate_decimal(&trade.price, "price")?;
        validate_decimal(&trade.size, "size")?;
        let maker_side = side(&trade.side)?;

        if let Some(PendingBackfill {
            after_trade_id,
            through_trade_id,
        }) = self.trade_gap
        {
            // A trade at or below the pending frontier is covered by the REST
            // backfill and stays paused. A trade beyond the frontier is in hand:
            // dropping it would lose a jump no later match reveals, so it is held
            // (bounded) and emitted once the backfilled range completes.
            if trade.trade_id > through_trade_id {
                if self.held_trades.len() >= MAX_HELD_TRADES {
                    // Fail closed: clear the buffer, but the in-hand trade is
                    // still never dropped -- hold it (the cleared buffer has
                    // room) and widen the pending range only up to the id below
                    // it. `id > through >= after + 1` gives `id - 1 > after`, so
                    // the widened range is non-empty and cannot underflow.
                    self.held_trades.clear();
                    let widened_through = trade.trade_id - 1;
                    self.trade_gap = Some(PendingBackfill {
                        after_trade_id,
                        through_trade_id: widened_through,
                    });
                    self.held_trades.push(HeldTrade {
                        trade_id: trade.trade_id,
                        price: trade.price,
                        size: trade.size,
                        maker_side: maker_side.to_owned(),
                        event_time: trade.time,
                        received_at: record.received_at.clone(),
                    });
                    return Ok(IngestOutcome::notice(FeedNotice::recovery(
                        FeedNoticeCode::Gap,
                        format!(
                            "Coinbase held-trade buffer exhausted; recovery widened to trade {widened_through}"
                        ),
                        RecoveryAction::BackfillCoinbaseTrades {
                            product_id: PRODUCT_ID.to_owned(),
                            after_trade_id,
                            through_trade_id: widened_through,
                        },
                    )));
                }
                self.held_trades.push(HeldTrade {
                    trade_id: trade.trade_id,
                    price: trade.price,
                    size: trade.size,
                    maker_side: maker_side.to_owned(),
                    event_time: trade.time,
                    received_at: record.received_at.clone(),
                });
            }
            return Ok(IngestOutcome::notice(FeedNotice::recovery(
                FeedNoticeCode::Gap,
                "Coinbase trade publication remains paused until backfill completes",
                RecoveryAction::BackfillCoinbaseTrades {
                    product_id: PRODUCT_ID.to_owned(),
                    after_trade_id,
                    through_trade_id,
                },
            )));
        }
        if let Some(previous) = self.last_trade_id {
            if trade.trade_id == previous {
                return Ok(IngestOutcome::notice(FeedNotice::info(
                    FeedNoticeCode::Duplicate,
                    format!("duplicate Coinbase trade_id {} ignored", trade.trade_id),
                )));
            }
            if trade.trade_id < previous {
                return Ok(IngestOutcome::notice(FeedNotice::info(
                    FeedNoticeCode::OutOfOrder,
                    format!(
                        "out-of-order Coinbase trade_id {} after {previous} ignored",
                        trade.trade_id
                    ),
                )));
            }
            if trade.trade_id > previous.saturating_add(1) {
                // The in-hand trade is never surrendered to REST: hold it and
                // ask the backfill only for ids strictly below it. A jump implies
                // `id >= previous + 2`, so `id - 1 >= previous + 1`: the range is
                // non-empty and cannot underflow.
                let through_trade_id = trade.trade_id - 1;
                self.trade_gap = Some(PendingBackfill {
                    after_trade_id: previous,
                    through_trade_id,
                });
                self.held_trades.push(HeldTrade {
                    trade_id: trade.trade_id,
                    price: trade.price,
                    size: trade.size,
                    maker_side: maker_side.to_owned(),
                    event_time: trade.time,
                    received_at: record.received_at.clone(),
                });
                return Ok(IngestOutcome::notice(FeedNotice::recovery(
                    FeedNoticeCode::Gap,
                    format!(
                        "Coinbase trade_id jumped from {previous} to {}",
                        through_trade_id + 1
                    ),
                    RecoveryAction::BackfillCoinbaseTrades {
                        product_id: PRODUCT_ID.to_owned(),
                        after_trade_id: previous,
                        through_trade_id,
                    },
                )));
            }
        }

        self.last_trade_id = Some(trade.trade_id);
        let event = self.trade_event(
            trade.trade_id,
            trade.price,
            trade.size,
            maker_side.to_owned(),
            trade.time,
            record.received_at.clone(),
        )?;
        Ok(IngestOutcome {
            events: vec![event],
            notices: Vec::new(),
        })
    }

    fn trade_event(
        &mut self,
        trade_id: u64,
        price: String,
        size: String,
        maker_side: String,
        event_time: String,
        received_at: String,
    ) -> Result<SourceEvent, AdapterError> {
        let aggressor_side = if maker_side == "buy" { "sell" } else { "buy" };
        let values = BTreeMap::from([
            ("instrument".to_owned(), json!(PRODUCT_ID)),
            ("trade_id".to_owned(), json!(trade_id)),
            ("price".to_owned(), json!(price)),
            ("size".to_owned(), json!(size)),
            ("maker_side".to_owned(), json!(maker_side)),
            ("aggressor_side".to_owned(), json!(aggressor_side)),
            ("event_time".to_owned(), json!(event_time)),
        ]);
        self.source_event(CoinbaseSource::Trade, event_time, received_at, values)
    }

    fn ticker(&mut self, record: &RawRecord, payload: &str) -> Result<IngestOutcome, AdapterError> {
        let ticker: CoinbaseTicker = serde_json::from_str(payload).map_err(payload_error)?;
        // Route by product id: BTC-USD is the adopted ticker, USDT-USD the E47
        // reference ticker carried on the same socket. Any other product is a
        // foreign symbol and is refused rather than emitted under a Coinbase id.
        let source = ticker_source(&ticker.product_id)?;
        validate_timestamp(&ticker.time, "time")?;
        for (name, value) in [
            ("price", &ticker.price),
            ("last_size", &ticker.last_size),
            ("best_bid", &ticker.best_bid),
            ("best_bid_size", &ticker.best_bid_size),
            ("best_ask", &ticker.best_ask),
            ("best_ask_size", &ticker.best_ask_size),
            ("open_24h", &ticker.open_24h),
            ("volume_24h", &ticker.volume_24h),
            ("low_24h", &ticker.low_24h),
            ("high_24h", &ticker.high_24h),
            ("volume_30d", &ticker.volume_30d),
        ] {
            validate_decimal(value, name)?;
        }
        let last_sequence = match source {
            CoinbaseSource::UsdtTicker => &mut self.last_usdt_ticker_sequence,
            _ => &mut self.last_btc_ticker_sequence,
        };
        if let Some(previous) = *last_sequence {
            if ticker.sequence == previous {
                return Ok(IngestOutcome::notice(FeedNotice::info(
                    FeedNoticeCode::Duplicate,
                    format!(
                        "duplicate Coinbase {} ticker sequence {previous} ignored",
                        ticker.product_id
                    ),
                )));
            }
            if ticker.sequence < previous {
                return Ok(IngestOutcome::notice(FeedNotice::info(
                    FeedNoticeCode::OutOfOrder,
                    format!(
                        "out-of-order Coinbase {} ticker sequence {} after {previous} ignored",
                        ticker.product_id, ticker.sequence
                    ),
                )));
            }
        }
        *last_sequence = Some(ticker.sequence);
        let values = BTreeMap::from([
            ("instrument".to_owned(), json!(ticker.product_id)),
            ("trade_id".to_owned(), json!(ticker.trade_id)),
            ("price".to_owned(), json!(ticker.price)),
            ("last_size".to_owned(), json!(ticker.last_size)),
            ("best_bid".to_owned(), json!(ticker.best_bid)),
            ("best_bid_size".to_owned(), json!(ticker.best_bid_size)),
            ("best_ask".to_owned(), json!(ticker.best_ask)),
            ("best_ask_size".to_owned(), json!(ticker.best_ask_size)),
            ("open_24h".to_owned(), json!(ticker.open_24h)),
            ("volume_24h".to_owned(), json!(ticker.volume_24h)),
            ("low_24h".to_owned(), json!(ticker.low_24h)),
            ("high_24h".to_owned(), json!(ticker.high_24h)),
            ("volume_30d".to_owned(), json!(ticker.volume_30d)),
            ("event_time".to_owned(), json!(ticker.time)),
            ("source_sequence".to_owned(), json!(ticker.sequence)),
        ]);
        let event = self.source_event(source, ticker.time, record.received_at.clone(), values)?;
        Ok(IngestOutcome {
            events: vec![event],
            notices: Vec::new(),
        })
    }

    fn product(
        &mut self,
        record: &RawRecord,
        payload: &str,
    ) -> Result<IngestOutcome, AdapterError> {
        let status: CoinbaseStatus = serde_json::from_str(payload).map_err(payload_error)?;
        if status.products.len() > MAX_STATUS_PRODUCTS {
            return Err(AdapterError::InvalidPayload(format!(
                "Coinbase status has {} products; maximum is {MAX_STATUS_PRODUCTS}",
                status.products.len()
            )));
        }
        let product = status
            .products
            .into_iter()
            .find(|product| product.id == PRODUCT_ID)
            .ok_or_else(|| {
                AdapterError::InvalidPayload("Coinbase status does not contain BTC-USD".to_owned())
            })?;
        validate_product(&product)?;
        let values = BTreeMap::from([
            ("instrument".to_owned(), json!(product.id)),
            ("base_asset".to_owned(), json!(product.base_currency)),
            ("quote_asset".to_owned(), json!(product.quote_currency)),
            ("status".to_owned(), json!(product.status)),
            ("base_increment".to_owned(), json!(product.base_increment)),
            ("quote_increment".to_owned(), json!(product.quote_increment)),
            (
                "minimum_notional".to_owned(),
                json!(product.min_market_funds),
            ),
            ("post_only".to_owned(), json!(product.post_only)),
            ("limit_only".to_owned(), json!(product.limit_only)),
            ("cancel_only".to_owned(), json!(product.cancel_only)),
            ("auction_mode".to_owned(), json!(product.auction_mode)),
            ("event_time".to_owned(), json!(record.received_at)),
        ]);
        let event = self.source_event(
            CoinbaseSource::Product,
            record.received_at.clone(),
            record.received_at.clone(),
            values,
        )?;
        Ok(IngestOutcome {
            events: vec![event],
            notices: vec![FeedNotice::info(
                FeedNoticeCode::VenueStatus,
                "Coinbase BTC-USD status/reference snapshot observed",
            )],
        })
    }

    fn book_snapshot(
        &mut self,
        record: &RawRecord,
        payload: &str,
    ) -> Result<IngestOutcome, AdapterError> {
        let snapshot: CoinbaseBookSnapshot =
            serde_json::from_str(payload).map_err(payload_error)?;
        require_product(&snapshot.product_id)?;
        let row_count = snapshot
            .bids
            .len()
            .checked_add(snapshot.asks.len())
            .ok_or_else(|| AdapterError::InvalidPayload("book row count overflowed".to_owned()))?;
        if row_count == 0 {
            self.book_snapshot_seen = false;
            return Err(AdapterError::InvalidPayload(
                "Coinbase book snapshot must contain at least one level".to_owned(),
            ));
        }
        require_book_row_count(row_count)?;
        let mut rows = Vec::with_capacity(row_count);
        for level in snapshot.bids {
            validate_decimal(&level[0], "bids.price")?;
            validate_decimal(&level[1], "bids.size")?;
            rows.push(("bid", level[0].clone(), level[1].clone()));
        }
        for level in snapshot.asks {
            validate_decimal(&level[0], "asks.price")?;
            validate_decimal(&level[1], "asks.size")?;
            rows.push(("ask", level[0].clone(), level[1].clone()));
        }
        self.book_snapshot_seen = true;
        self.book_events(record, "snapshot", "receipt", &record.received_at, rows)
    }

    fn book_update(
        &mut self,
        record: &RawRecord,
        payload: &str,
    ) -> Result<IngestOutcome, AdapterError> {
        if !self.book_snapshot_seen {
            return Ok(IngestOutcome::notice(FeedNotice::recovery(
                FeedNoticeCode::AwaitingSnapshot,
                "Coinbase level2 update ignored before a connection-local snapshot",
                RecoveryAction::Reconnect,
            )));
        }
        let update: CoinbaseBookUpdate = serde_json::from_str(payload).map_err(payload_error)?;
        require_product(&update.product_id)?;
        validate_timestamp(&update.time, "time")?;
        require_book_row_count(update.changes.len())?;
        let mut rows = Vec::with_capacity(update.changes.len());
        for change in update.changes {
            let normalized_side = side_to_book_side(&change[0])?;
            validate_decimal(&change[1], "changes.price")?;
            validate_decimal(&change[2], "changes.size")?;
            rows.push((normalized_side, change[1].clone(), change[2].clone()));
        }
        self.book_events(record, "delta", "source", &update.time, rows)
    }

    fn book_events(
        &mut self,
        record: &RawRecord,
        update_kind: &str,
        event_time_basis: &str,
        event_time: &str,
        rows: Vec<(&str, String, String)>,
    ) -> Result<IngestOutcome, AdapterError> {
        let update_count = u32::try_from(rows.len()).map_err(|_| {
            AdapterError::InvalidPayload("book update_count exceeds u32".to_owned())
        })?;
        let update_group = format!(
            "{}:{}:{}",
            record.stream.as_str(),
            record.connection_epoch,
            record.ordinal
        );
        let mut events = Vec::with_capacity(rows.len());
        for (index, (side, price, size)) in rows.into_iter().enumerate() {
            let update_index = u32::try_from(index).map_err(|_| {
                AdapterError::InvalidPayload("book update_index exceeds u32".to_owned())
            })?;
            let values = BTreeMap::from([
                ("instrument".to_owned(), json!(PRODUCT_ID)),
                ("update_group".to_owned(), json!(update_group)),
                ("update_index".to_owned(), json!(update_index)),
                ("update_count".to_owned(), json!(update_count)),
                ("update_kind".to_owned(), json!(update_kind)),
                ("side".to_owned(), json!(side)),
                ("price".to_owned(), json!(price)),
                ("size".to_owned(), json!(size)),
                ("event_time_basis".to_owned(), json!(event_time_basis)),
                ("event_time".to_owned(), json!(event_time)),
            ]);
            events.push(self.source_event(
                CoinbaseSource::Book,
                event_time.to_owned(),
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
        source: CoinbaseSource,
        event_time: String,
        received_at: String,
        values: BTreeMap<String, Value>,
    ) -> Result<SourceEvent, AdapterError> {
        let sequence = match source {
            CoinbaseSource::Trade => &mut self.next_trade_sequence,
            CoinbaseSource::Ticker => &mut self.next_btc_ticker_sequence,
            CoinbaseSource::UsdtTicker => &mut self.next_usdt_ticker_sequence,
            CoinbaseSource::Book => &mut self.next_book_sequence,
            CoinbaseSource::Product => &mut self.next_product_sequence,
        };
        let current = *sequence;
        *sequence = sequence
            .checked_add(1)
            .ok_or(AdapterError::SequenceExhausted)?;
        Ok(SourceEvent {
            schema_version: 1,
            source_id: source.source_id().to_owned(),
            source_version: source.source_version().to_owned(),
            publisher_epoch: self.publisher_epoch.clone(),
            sequence: current,
            event_time,
            received_at,
            values,
        })
    }
}

impl FeedAdapter for CoinbaseAdapter {
    fn ingest(&mut self, record: &RawRecord) -> Result<IngestOutcome, AdapterError> {
        match self.guard.observe(record)? {
            GuardAction::Configuration(_) => Err(AdapterError::InvalidRecord(
                "Coinbase captures do not accept configuration records".to_owned(),
            )),
            GuardAction::Connected => {
                self.book_snapshot_seen = false;
                Ok(IngestOutcome::notice(FeedNotice::info(
                    FeedNoticeCode::Connected,
                    "Coinbase WebSocket connected; book state awaits a fresh snapshot",
                )))
            }
            GuardAction::Disconnected => {
                self.book_snapshot_seen = false;
                Ok(IngestOutcome::notice(FeedNotice::recovery(
                    FeedNoticeCode::Disconnected,
                    "Coinbase WebSocket disconnected",
                    RecoveryAction::Reconnect,
                )))
            }
            GuardAction::Outbound => Ok(IngestOutcome::default()),
            GuardAction::Failed(_) => Err(AdapterError::InvalidRecord(
                "Coinbase captures do not accept failure records".to_owned(),
            )),
            GuardAction::Inbound(payload) => self.ingest_payload(record, payload),
        }
    }
}

#[derive(Deserialize)]
struct CoinbaseEnvelope {
    #[serde(rename = "type")]
    message_type: String,
}

#[derive(Deserialize)]
struct CoinbaseError {
    message: String,
}

#[derive(Deserialize)]
struct CoinbaseHeartbeat {
    product_id: String,
    last_trade_id: u64,
    time: String,
}

#[derive(Deserialize)]
struct CoinbaseTrade {
    product_id: String,
    trade_id: u64,
    price: String,
    size: String,
    side: String,
    time: String,
}

#[derive(Deserialize)]
struct CoinbaseTicker {
    product_id: String,
    trade_id: u64,
    sequence: u64,
    price: String,
    last_size: String,
    best_bid: String,
    best_bid_size: String,
    best_ask: String,
    best_ask_size: String,
    open_24h: String,
    volume_24h: String,
    low_24h: String,
    high_24h: String,
    volume_30d: String,
    time: String,
}

#[derive(Deserialize)]
struct CoinbaseStatus {
    products: Vec<CoinbaseProduct>,
}

#[derive(Deserialize)]
struct CoinbaseProduct {
    id: String,
    base_currency: String,
    quote_currency: String,
    base_increment: String,
    quote_increment: String,
    status: String,
    min_market_funds: String,
    post_only: bool,
    limit_only: bool,
    cancel_only: bool,
    auction_mode: bool,
}

/// One Coinbase Exchange REST trade row, as returned by
/// `GET /products/BTC-USD/trades`, for
/// [`CoinbaseAdapter::apply_trade_backfill`].
///
/// Field names and shapes match the venue response exactly, so a caller can
/// deserialize the REST body straight into `Vec<CoinbaseRestTrade>`. Values are
/// validated by `apply_trade_backfill`, not on construction.
#[derive(Debug, Deserialize)]
pub struct CoinbaseRestTrade {
    /// Venue trade id. Contiguous and increasing across the recovered range.
    pub trade_id: u64,
    /// Exact decimal price text. Preserved verbatim; never rounded.
    pub price: String,
    /// Exact decimal base size text. Preserved verbatim; never rounded.
    pub size: String,
    /// Maker side, the venue's `buy` or `sell`. Any other value is refused.
    pub side: String,
    /// Venue trade time as an RFC 3339 instant.
    pub time: String,
}

#[derive(Deserialize)]
struct CoinbaseBookSnapshot {
    product_id: String,
    bids: Vec<[String; 2]>,
    asks: Vec<[String; 2]>,
}

#[derive(Deserialize)]
struct CoinbaseBookUpdate {
    product_id: String,
    time: String,
    changes: Vec<[String; 3]>,
}

fn require_product(product_id: &str) -> Result<(), AdapterError> {
    if product_id == PRODUCT_ID {
        Ok(())
    } else {
        Err(AdapterError::InvalidPayload(format!(
            "unexpected Coinbase product_id `{}`",
            bounded_token(product_id)
        )))
    }
}

/// Map a `ticker` product id to its source identity. Only BTC-USD and the E47
/// USDT-USD reference are carried; any other product is a foreign symbol and is
/// refused, so the adapter never emits a foreign source id.
fn ticker_source(product_id: &str) -> Result<CoinbaseSource, AdapterError> {
    match product_id {
        PRODUCT_ID => Ok(CoinbaseSource::Ticker),
        USDT_PRODUCT_ID => Ok(CoinbaseSource::UsdtTicker),
        other => Err(AdapterError::InvalidPayload(format!(
            "unexpected Coinbase product_id `{}`",
            bounded_token(other)
        ))),
    }
}

fn validate_product(product: &CoinbaseProduct) -> Result<(), AdapterError> {
    require_product(&product.id)?;
    if product.base_currency != "BTC" || product.quote_currency != "USD" {
        return Err(AdapterError::InvalidPayload(
            "Coinbase BTC-USD product has unexpected base or quote asset".to_owned(),
        ));
    }
    for (name, value) in [
        ("base_increment", &product.base_increment),
        ("quote_increment", &product.quote_increment),
        ("min_market_funds", &product.min_market_funds),
    ] {
        validate_decimal(value, name)?;
    }
    if product.status.is_empty() || product.status.len() > 64 {
        return Err(AdapterError::InvalidPayload(
            "Coinbase product status must contain 1 to 64 bytes".to_owned(),
        ));
    }
    Ok(())
}

fn validate_rest_trade(trade: &CoinbaseRestTrade) -> Result<(), AdapterError> {
    validate_decimal(&trade.price, "price")?;
    validate_decimal(&trade.size, "size")?;
    side(&trade.side)?;
    validate_timestamp(&trade.time, "time")
}

fn side(value: &str) -> Result<&str, AdapterError> {
    match value {
        "buy" | "sell" => Ok(value),
        _ => Err(AdapterError::InvalidPayload(format!(
            "unknown Coinbase side `{}`",
            bounded_token(value)
        ))),
    }
}

fn side_to_book_side(value: &str) -> Result<&'static str, AdapterError> {
    match value {
        "buy" => Ok("bid"),
        "sell" => Ok("ask"),
        _ => Err(AdapterError::InvalidPayload(format!(
            "unknown Coinbase side `{}`",
            bounded_token(value)
        ))),
    }
}

fn require_book_row_count(count: usize) -> Result<(), AdapterError> {
    if count > MAX_BOOK_ROWS {
        Err(AdapterError::InvalidPayload(format!(
            "Coinbase book message has {count} rows; maximum is {MAX_BOOK_ROWS}"
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
    use crate::{
        CaptureStream, FeedAdapter, FeedNoticeCode, RawRecord, RecordKind, RecoveryAction,
    };

    use super::{CoinbaseAdapter, CoinbaseRestTrade};

    fn record(ordinal: u64, payload: &str) -> RawRecord {
        RawRecord {
            schema_version: 1,
            provider: crate::Provider::CoinbaseExchange,
            stream: CaptureStream::CoinbaseCore,
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
        RawRecord {
            schema_version: 1,
            provider: crate::Provider::CoinbaseExchange,
            stream: CaptureStream::CoinbaseCore,
            connection_epoch: 1,
            ordinal: 1,
            received_at: "2026-08-12T12:00:00Z".to_owned(),
            capture_elapsed_ns: None,
            connect_started_elapsed_ns: None,
            kind: RecordKind::Connected,
            payload: None,
        }
    }

    #[test]
    fn trade_gap_fails_closed_until_backfill_is_confirmed() {
        let mut adapter = CoinbaseAdapter::new("test-epoch").expect("adapter constructs");
        adapter.ingest(&connected()).expect("connects");
        let first = record(
            2,
            r#"{"type":"match","trade_id":10,"sequence":100,"product_id":"BTC-USD","price":"60000.10","size":"0.01","side":"sell","time":"2026-08-12T12:00:00Z"}"#,
        );
        assert_eq!(adapter.ingest(&first).expect("first trade").events.len(), 1);

        let gap = record(
            3,
            r#"{"type":"match","trade_id":12,"sequence":102,"product_id":"BTC-USD","price":"60001.00","size":"0.02","side":"buy","time":"2026-08-12T12:00:01Z"}"#,
        );
        let outcome = adapter.ingest(&gap).expect("gap is operational state");
        assert!(outcome.events.is_empty());
        assert_eq!(outcome.notices[0].code, FeedNoticeCode::Gap);
        // The in-hand trade 12 is held, not surrendered: recovery asks REST only
        // for ids strictly below it, so the range ends at id - 1 (11).
        assert_eq!(
            outcome.notices[0].recovery,
            Some(RecoveryAction::BackfillCoinbaseTrades {
                product_id: "BTC-USD".to_owned(),
                after_trade_id: 10,
                through_trade_id: 11,
            })
        );

        // Required range is now (10, 11]: the held trade 12 is not demanded from
        // REST. A backfill missing its one required trade fails closed.
        let incomplete = adapter.apply_trade_backfill(10, 11, "2026-08-12T12:00:01.250Z", vec![]);
        assert!(incomplete.is_err());
        // Trade 13 arrives while paused, beyond the pending frontier (11). It is
        // held in hand (not dropped): publication stays paused, but the trade is
        // now on record and will be emitted once the backfilled range completes.
        let still_paused = adapter
            .ingest(&record(
                4,
                r#"{"type":"match","trade_id":13,"sequence":103,"product_id":"BTC-USD","price":"60002.00","size":"0.03","side":"sell","time":"2026-08-12T12:00:02Z"}"#,
            ))
            .expect("an incomplete backfill leaves publication paused");
        assert!(still_paused.events.is_empty());
        assert_eq!(still_paused.notices[0].code, FeedNoticeCode::Gap);

        let recovered = adapter
            .apply_trade_backfill(
                10,
                11,
                "2026-08-12T12:00:01.500Z",
                vec![CoinbaseRestTrade {
                    trade_id: 11,
                    price: "60000.75".to_owned(),
                    size: "0.01".to_owned(),
                    side: "sell".to_owned(),
                    time: "2026-08-12T12:00:00.500Z".to_owned(),
                }],
            )
            .expect("complete backfill restores cursor");
        // Backfilled 11 then the held trades 12, 13 in order: the in-hand trades
        // are recovered rather than lost.
        assert_eq!(recovered.events.len(), 3);
        assert_eq!(
            recovered.events[2].values.get("trade_id"),
            Some(&serde_json::json!(13))
        );
        // The venue's later re-broadcast of trade 13 is now a duplicate of the
        // recovered trade and is ignored; a fresh contiguous trade still emits.
        let duplicate = record(
            5,
            r#"{"type":"match","trade_id":13,"sequence":103,"product_id":"BTC-USD","price":"60002.00","size":"0.03","side":"sell","time":"2026-08-12T12:00:02Z"}"#,
        );
        let duplicate = adapter
            .ingest(&duplicate)
            .expect("duplicate is operational");
        assert!(duplicate.events.is_empty());
        assert_eq!(duplicate.notices[0].code, FeedNoticeCode::Duplicate);
        let next = record(
            6,
            r#"{"type":"match","trade_id":14,"sequence":104,"product_id":"BTC-USD","price":"60003.00","size":"0.01","side":"buy","time":"2026-08-12T12:00:03Z"}"#,
        );
        assert_eq!(adapter.ingest(&next).expect("next trade").events.len(), 1);
    }

    #[test]
    fn level2_update_waits_for_snapshot() {
        let mut adapter = CoinbaseAdapter::new("test-epoch").expect("adapter constructs");
        adapter.ingest(&connected()).expect("connects");
        let update = record(
            2,
            r#"{"type":"l2update","product_id":"BTC-USD","time":"2026-08-12T12:00:00Z","changes":[["buy","60000.00","1.0"]]}"#,
        );
        let outcome = adapter
            .ingest(&update)
            .expect("awaiting snapshot is handled");
        assert!(outcome.events.is_empty());
        assert_eq!(outcome.notices[0].code, FeedNoticeCode::AwaitingSnapshot);
    }

    #[test]
    fn empty_level2_snapshot_is_rejected_and_does_not_open_the_delta_gate() {
        let mut adapter = CoinbaseAdapter::new("test-epoch").expect("adapter constructs");
        adapter.ingest(&connected()).expect("connects");
        let empty = record(
            2,
            r#"{"type":"snapshot","product_id":"BTC-USD","bids":[],"asks":[]}"#,
        );
        assert!(adapter.ingest(&empty).is_err());
        let update = record(
            3,
            r#"{"type":"l2update","product_id":"BTC-USD","time":"2026-08-12T12:00:00Z","changes":[["buy","60000.00","1.0"]]}"#,
        );
        let outcome = adapter.ingest(&update).expect("delta remains gated");
        assert_eq!(outcome.notices[0].code, FeedNoticeCode::AwaitingSnapshot);
    }

    fn heartbeat(ordinal: u64, last_trade_id: u64) -> RawRecord {
        record(
            ordinal,
            &format!(
                r#"{{"type":"heartbeat","product_id":"BTC-USD","last_trade_id":{last_trade_id},"sequence":{last_trade_id},"time":"2026-08-12T12:00:05Z"}}"#
            ),
        )
    }

    // Heartbeat completeness cross-check (SIG-041): the heartbeat channel's
    // declared `last_trade_id` is compared against the observed trade tape as a
    // second detector independent of per-message sequence tracking.

    #[test]
    fn heartbeat_consistent_with_observed_tape_does_not_trigger_recovery() {
        let mut adapter = CoinbaseAdapter::new("test-epoch").expect("adapter constructs");
        adapter.ingest(&connected()).expect("connects");
        let trade = record(
            2,
            r#"{"type":"match","trade_id":10,"sequence":100,"product_id":"BTC-USD","price":"60000.10","size":"0.01","side":"sell","time":"2026-08-12T12:00:00Z"}"#,
        );
        assert_eq!(adapter.ingest(&trade).expect("first trade").events.len(), 1);

        // Declared progress equal to the observed tape is consistent: info only.
        let level = adapter
            .ingest(&heartbeat(3, 10))
            .expect("heartbeat is operational state");
        assert!(level.events.is_empty());
        assert_eq!(level.notices[0].code, FeedNoticeCode::Heartbeat);
        assert!(level.notices[0].recovery.is_none());

        // Exactly one trade ahead is a single in-flight trade, not silent loss:
        // the strict `> last + 1` threshold avoids that false positive.
        let ahead = adapter
            .ingest(&heartbeat(4, 11))
            .expect("heartbeat is operational state");
        assert!(ahead.events.is_empty());
        assert_eq!(ahead.notices[0].code, FeedNoticeCode::Heartbeat);
        assert!(ahead.notices[0].recovery.is_none());

        // Publication is not paused: the next contiguous trade still emits.
        let next = record(
            5,
            r#"{"type":"match","trade_id":11,"sequence":101,"product_id":"BTC-USD","price":"60000.20","size":"0.01","side":"buy","time":"2026-08-12T12:00:06Z"}"#,
        );
        assert_eq!(adapter.ingest(&next).expect("next trade").events.len(), 1);
    }

    #[test]
    fn heartbeat_declaring_progress_beyond_observed_tape_routes_existing_recovery() {
        let mut adapter = CoinbaseAdapter::new("test-epoch").expect("adapter constructs");
        adapter.ingest(&connected()).expect("connects");
        let trade = record(
            2,
            r#"{"type":"match","trade_id":10,"sequence":100,"product_id":"BTC-USD","price":"60000.10","size":"0.01","side":"sell","time":"2026-08-12T12:00:00Z"}"#,
        );
        assert_eq!(adapter.ingest(&trade).expect("first trade").events.len(), 1);

        // The venue's heartbeat declares trade 13 while the tape only reached 10:
        // trades 11 and 12 were silently lost with no later match to reveal them.
        let outcome = adapter
            .ingest(&heartbeat(3, 13))
            .expect("heartbeat is operational state");
        assert!(outcome.events.is_empty());
        assert_eq!(outcome.notices[0].code, FeedNoticeCode::Gap);
        assert_eq!(
            outcome.notices[0].recovery,
            Some(RecoveryAction::BackfillCoinbaseTrades {
                product_id: "BTC-USD".to_owned(),
                after_trade_id: 10,
                through_trade_id: 13,
            })
        );

        // The detector routes through the same fail-closed backfill machinery a
        // per-message gap uses: publication stays paused until the exact range
        // is recovered. Trade 14 arrives while paused, beyond the pending
        // frontier (13). It is the final trade before a quiet period, so no later
        // match would ever reveal it; rather than dropping it, the adapter holds
        // it and emits it once the backfilled range completes.
        let paused = adapter
            .ingest(&record(
                4,
                r#"{"type":"match","trade_id":14,"sequence":104,"product_id":"BTC-USD","price":"60003.00","size":"0.01","side":"sell","time":"2026-08-12T12:00:07Z"}"#,
            ))
            .expect("a pending backfill leaves publication paused");
        assert!(paused.events.is_empty());
        assert_eq!(paused.notices[0].code, FeedNoticeCode::Gap);

        let recovered = adapter
            .apply_trade_backfill(
                10,
                13,
                "2026-08-12T12:00:07.500Z",
                vec![
                    CoinbaseRestTrade {
                        trade_id: 11,
                        price: "60000.75".to_owned(),
                        size: "0.01".to_owned(),
                        side: "sell".to_owned(),
                        time: "2026-08-12T12:00:05.500Z".to_owned(),
                    },
                    CoinbaseRestTrade {
                        trade_id: 12,
                        price: "60001.00".to_owned(),
                        size: "0.02".to_owned(),
                        side: "buy".to_owned(),
                        time: "2026-08-12T12:00:06Z".to_owned(),
                    },
                    CoinbaseRestTrade {
                        trade_id: 13,
                        price: "60002.00".to_owned(),
                        size: "0.03".to_owned(),
                        side: "sell".to_owned(),
                        time: "2026-08-12T12:00:06.500Z".to_owned(),
                    },
                ],
            )
            .expect("complete backfill restores the cursor");
        // Backfilled 11, 12, 13 then the held trade 14: the final-before-quiet
        // trade that per-message tracking could never reveal is now recovered.
        assert_eq!(recovered.events.len(), 4);
        assert_eq!(
            recovered.events[3].values.get("trade_id"),
            Some(&serde_json::json!(14))
        );
    }

    #[test]
    fn multiple_held_trades_beyond_the_frontier_emit_in_order_after_backfill() {
        let mut adapter = CoinbaseAdapter::new("test-epoch").expect("adapter constructs");
        adapter.ingest(&connected()).expect("connects");
        assert_eq!(
            adapter
                .ingest(&record(
                    2,
                    r#"{"type":"match","trade_id":10,"sequence":100,"product_id":"BTC-USD","price":"60000.10","size":"0.01","side":"sell","time":"2026-08-12T12:00:00Z"}"#,
                ))
                .expect("first trade")
                .events
                .len(),
            1
        );
        // A per-message gap holds the in-hand trade 12 and opens the pending
        // range (10, 11]: REST is asked only for ids strictly below what is held.
        let gap = adapter
            .ingest(&record(
                3,
                r#"{"type":"match","trade_id":12,"sequence":102,"product_id":"BTC-USD","price":"60001.00","size":"0.02","side":"buy","time":"2026-08-12T12:00:01Z"}"#,
            ))
            .expect("gap is operational");
        assert!(gap.events.is_empty());

        // Three more trades arrive while paused, all beyond the frontier (11),
        // out of wire order. Each is held; publication stays paused.
        for (ordinal, id, seconds) in [(4u64, 15u64, 4u64), (5, 13, 2), (6, 14, 3)] {
            let held = adapter
                .ingest(&record(
                    ordinal,
                    &format!(
                        r#"{{"type":"match","trade_id":{id},"sequence":{id},"product_id":"BTC-USD","price":"6000{seconds}.00","size":"0.01","side":"sell","time":"2026-08-12T12:00:0{seconds}Z"}}"#
                    ),
                ))
                .expect("held while paused");
            assert!(held.events.is_empty());
            assert_eq!(held.notices[0].code, FeedNoticeCode::Gap);
        }

        let recovered = adapter
            .apply_trade_backfill(
                10,
                11,
                "2026-08-12T12:00:05Z",
                vec![CoinbaseRestTrade {
                    trade_id: 11,
                    price: "60000.75".to_owned(),
                    size: "0.01".to_owned(),
                    side: "sell".to_owned(),
                    time: "2026-08-12T12:00:00.500Z".to_owned(),
                }],
            )
            .expect("complete backfill restores the cursor");
        // Backfilled 11 then held 12, 13, 14, 15 sorted into contiguous order.
        let ids: Vec<_> = recovered
            .events
            .iter()
            .map(|event| event.values.get("trade_id").cloned())
            .collect();
        assert_eq!(
            ids,
            vec![
                Some(serde_json::json!(11)),
                Some(serde_json::json!(12)),
                Some(serde_json::json!(13)),
                Some(serde_json::json!(14)),
                Some(serde_json::json!(15)),
            ]
        );
        assert!(recovered.notices.is_empty());
        // The cursor advanced to the held frontier; the next contiguous trade emits.
        assert_eq!(
            adapter
                .ingest(&record(
                    7,
                    r#"{"type":"match","trade_id":16,"sequence":116,"product_id":"BTC-USD","price":"60006.00","size":"0.01","side":"buy","time":"2026-08-12T12:00:06Z"}"#,
                ))
                .expect("next trade")
                .events
                .len(),
            1
        );
    }

    #[test]
    fn held_trade_within_the_backfill_range_is_deduplicated() {
        let mut adapter = CoinbaseAdapter::new("test-epoch").expect("adapter constructs");
        adapter.ingest(&connected()).expect("connects");
        adapter
            .ingest(&record(
                2,
                r#"{"type":"match","trade_id":10,"sequence":100,"product_id":"BTC-USD","price":"60000.10","size":"0.01","side":"sell","time":"2026-08-12T12:00:00Z"}"#,
            ))
            .expect("first trade");
        // The in-hand trade 13 is held; the pending range opens at (10, 12].
        adapter
            .ingest(&record(
                3,
                r#"{"type":"match","trade_id":13,"sequence":103,"product_id":"BTC-USD","price":"60003.00","size":"0.03","side":"sell","time":"2026-08-12T12:00:03Z"}"#,
            ))
            .expect("gap is operational");
        // The venue re-broadcasts trade 13 while paused: beyond the frontier (12)
        // it is held again, a duplicate that the drain deduplicates.
        let duplicate_held = adapter
            .ingest(&record(
                4,
                r#"{"type":"match","trade_id":13,"sequence":103,"product_id":"BTC-USD","price":"60003.00","size":"0.03","side":"sell","time":"2026-08-12T12:00:03Z"}"#,
            ))
            .expect("duplicate held trade stays paused");
        assert!(duplicate_held.events.is_empty());
        // A beyond-frontier trade 14 is held.
        adapter
            .ingest(&record(
                5,
                r#"{"type":"match","trade_id":14,"sequence":104,"product_id":"BTC-USD","price":"60004.00","size":"0.01","side":"buy","time":"2026-08-12T12:00:04Z"}"#,
            ))
            .expect("beyond-frontier trade is held");

        let recovered = adapter
            .apply_trade_backfill(
                10,
                12,
                "2026-08-12T12:00:04.500Z",
                vec![
                    CoinbaseRestTrade {
                        trade_id: 11,
                        price: "60001.00".to_owned(),
                        size: "0.01".to_owned(),
                        side: "sell".to_owned(),
                        time: "2026-08-12T12:00:01Z".to_owned(),
                    },
                    CoinbaseRestTrade {
                        trade_id: 12,
                        price: "60002.00".to_owned(),
                        size: "0.01".to_owned(),
                        side: "buy".to_owned(),
                        time: "2026-08-12T12:00:02Z".to_owned(),
                    },
                ],
            )
            .expect("complete backfill restores the cursor");
        // 11, 12 from the backfill then held 13 (once, the duplicate collapsed)
        // and held 14: no duplicate 13 despite two held copies.
        let ids: Vec<_> = recovered
            .events
            .iter()
            .map(|event| event.values.get("trade_id").cloned())
            .collect();
        assert_eq!(
            ids,
            vec![
                Some(serde_json::json!(11)),
                Some(serde_json::json!(12)),
                Some(serde_json::json!(13)),
                Some(serde_json::json!(14)),
            ]
        );
    }

    #[test]
    fn a_hole_within_the_held_run_reopens_recovery_and_fails_closed() {
        let mut adapter = CoinbaseAdapter::new("test-epoch").expect("adapter constructs");
        adapter.ingest(&connected()).expect("connects");
        adapter
            .ingest(&record(
                2,
                r#"{"type":"match","trade_id":10,"sequence":100,"product_id":"BTC-USD","price":"60000.10","size":"0.01","side":"sell","time":"2026-08-12T12:00:00Z"}"#,
            ))
            .expect("first trade");
        // The in-hand trade 12 is held; the pending range opens at (10, 11].
        adapter
            .ingest(&record(
                3,
                r#"{"type":"match","trade_id":12,"sequence":102,"product_id":"BTC-USD","price":"60001.00","size":"0.02","side":"buy","time":"2026-08-12T12:00:01Z"}"#,
            ))
            .expect("gap is operational");
        // Held run becomes 12, 13, then 15 -- trade 14 was never seen, a hole in hand.
        for (ordinal, id, time) in [
            (4u64, 13u64, "2026-08-12T12:00:03Z"),
            (5, 15, "2026-08-12T12:00:05Z"),
        ] {
            adapter
                .ingest(&record(
                    ordinal,
                    &format!(
                        r#"{{"type":"match","trade_id":{id},"sequence":{id},"product_id":"BTC-USD","price":"600{id}.00","size":"0.01","side":"sell","time":"{time}"}}"#
                    ),
                ))
                .expect("held while paused");
        }

        let recovered = adapter
            .apply_trade_backfill(
                10,
                11,
                "2026-08-12T12:00:05Z",
                vec![CoinbaseRestTrade {
                    trade_id: 11,
                    price: "60000.75".to_owned(),
                    size: "0.01".to_owned(),
                    side: "sell".to_owned(),
                    time: "2026-08-12T12:00:00.500Z".to_owned(),
                }],
            )
            .expect("complete backfill restores the cursor");
        // 11 emits, then held 12, 13 (contiguous). Trade 15 is not contiguous with
        // 13, so recovery reopens over the ids strictly below 15 -- (13, 14] --
        // instead of emitting a torn tape.
        let ids: Vec<_> = recovered
            .events
            .iter()
            .map(|event| event.values.get("trade_id").cloned())
            .collect();
        assert_eq!(
            ids,
            vec![
                Some(serde_json::json!(11)),
                Some(serde_json::json!(12)),
                Some(serde_json::json!(13)),
            ]
        );
        assert_eq!(recovered.notices[0].code, FeedNoticeCode::Gap);
        assert_eq!(
            recovered.notices[0].recovery,
            Some(RecoveryAction::BackfillCoinbaseTrades {
                product_id: "BTC-USD".to_owned(),
                after_trade_id: 13,
                through_trade_id: 14,
            })
        );

        // The tail (trade 15) was re-held, not lost. Completing the reopened
        // range emits the missing 14 and then resumes the held tail contiguously.
        let resumed = adapter
            .apply_trade_backfill(
                13,
                14,
                "2026-08-12T12:00:05.500Z",
                vec![CoinbaseRestTrade {
                    trade_id: 14,
                    price: "60014.00".to_owned(),
                    size: "0.01".to_owned(),
                    side: "buy".to_owned(),
                    time: "2026-08-12T12:00:04Z".to_owned(),
                }],
            )
            .expect("reopened range completes");
        let resumed_ids: Vec<_> = resumed
            .events
            .iter()
            .map(|event| event.values.get("trade_id").cloned())
            .collect();
        assert_eq!(
            resumed_ids,
            vec![Some(serde_json::json!(14)), Some(serde_json::json!(15))]
        );
        assert!(resumed.notices.is_empty());
    }

    #[test]
    fn held_buffer_overflow_fails_closed_by_widening_recovery() {
        use super::{HeldTrade, MAX_HELD_TRADES};

        let mut adapter = CoinbaseAdapter::new("test-epoch").expect("adapter constructs");
        adapter.ingest(&connected()).expect("connects");
        adapter
            .ingest(&record(
                2,
                r#"{"type":"match","trade_id":10,"sequence":100,"product_id":"BTC-USD","price":"60000.10","size":"0.01","side":"sell","time":"2026-08-12T12:00:00Z"}"#,
            ))
            .expect("first trade");
        // The in-hand trade 12 is held; the pending range opens at (10, 11].
        adapter
            .ingest(&record(
                3,
                r#"{"type":"match","trade_id":12,"sequence":102,"product_id":"BTC-USD","price":"60001.00","size":"0.02","side":"buy","time":"2026-08-12T12:00:01Z"}"#,
            ))
            .expect("gap is operational");
        // Saturate the held buffer to its bound (overwriting the held trade 12).
        adapter.held_trades = (0..MAX_HELD_TRADES)
            .map(|offset| HeldTrade {
                trade_id: 1_000 + offset as u64,
                price: "60000.00".to_owned(),
                size: "0.01".to_owned(),
                maker_side: "buy".to_owned(),
                event_time: "2026-08-12T12:00:02Z".to_owned(),
                received_at: "2026-08-12T12:00:02Z".to_owned(),
            })
            .collect();

        // One more beyond-frontier trade overflows the buffer: the buffer is
        // cleared, but the in-hand trade 13 is still never dropped -- it is held
        // in the emptied buffer and the pending range widens only up to 12 (id-1).
        let overflow = adapter
            .ingest(&record(
                4,
                r#"{"type":"match","trade_id":13,"sequence":103,"product_id":"BTC-USD","price":"60002.00","size":"0.03","side":"sell","time":"2026-08-12T12:00:03Z"}"#,
            ))
            .expect("overflow is operational");
        assert!(overflow.events.is_empty());
        assert_eq!(overflow.notices[0].code, FeedNoticeCode::Gap);
        assert_eq!(
            overflow.notices[0].recovery,
            Some(RecoveryAction::BackfillCoinbaseTrades {
                product_id: "BTC-USD".to_owned(),
                after_trade_id: 10,
                through_trade_id: 12,
            })
        );
        // The saturating buffer was cleared, leaving only the newly held trade 13.
        assert_eq!(adapter.held_trades.len(), 1);
        assert_eq!(adapter.held_trades[0].trade_id, 13);
    }

    #[test]
    fn gap_open_holds_the_in_hand_trade_and_recovers_below_it() {
        let mut adapter = CoinbaseAdapter::new("test-epoch").expect("adapter constructs");
        adapter.ingest(&connected()).expect("connects");
        adapter
            .ingest(&record(
                2,
                r#"{"type":"match","trade_id":10,"sequence":100,"product_id":"BTC-USD","price":"60000.10","size":"0.01","side":"sell","time":"2026-08-12T12:00:00Z"}"#,
            ))
            .expect("first trade");

        // A jump to 12 holds the in-hand trade and asks REST only for ids strictly
        // below it: the recovery range ends at id - 1 (11), not id.
        let gap = adapter
            .ingest(&record(
                3,
                r#"{"type":"match","trade_id":12,"sequence":102,"product_id":"BTC-USD","price":"60001.00","size":"0.02","side":"buy","time":"2026-08-12T12:00:01Z"}"#,
            ))
            .expect("gap is operational");
        assert!(gap.events.is_empty());
        assert_eq!(
            gap.notices[0].recovery,
            Some(RecoveryAction::BackfillCoinbaseTrades {
                product_id: "BTC-USD".to_owned(),
                after_trade_id: 10,
                through_trade_id: 11,
            })
        );

        // A complete backfill of exactly (10, 11] emits 11, then the held trade 12
        // last; the cursor advances to 12 (the in-hand id), not 11.
        let recovered = adapter
            .apply_trade_backfill(
                10,
                11,
                "2026-08-12T12:00:01.500Z",
                vec![CoinbaseRestTrade {
                    trade_id: 11,
                    price: "60000.75".to_owned(),
                    size: "0.01".to_owned(),
                    side: "sell".to_owned(),
                    time: "2026-08-12T12:00:00.500Z".to_owned(),
                }],
            )
            .expect("complete backfill restores the cursor");
        let ids: Vec<_> = recovered
            .events
            .iter()
            .map(|event| event.values.get("trade_id").cloned())
            .collect();
        assert_eq!(
            ids,
            vec![Some(serde_json::json!(11)), Some(serde_json::json!(12))]
        );
        assert!(recovered.notices.is_empty());
        // last_trade_id == 12: the next contiguous trade 13 emits without a gap.
        assert_eq!(
            adapter
                .ingest(&record(
                    4,
                    r#"{"type":"match","trade_id":13,"sequence":103,"product_id":"BTC-USD","price":"60002.00","size":"0.01","side":"sell","time":"2026-08-12T12:00:02Z"}"#,
                ))
                .expect("next trade")
                .events
                .len(),
            1
        );
    }

    #[test]
    fn a_second_hole_inside_the_re_held_tail_recurses() {
        let mut adapter = CoinbaseAdapter::new("test-epoch").expect("adapter constructs");
        adapter.ingest(&connected()).expect("connects");
        adapter
            .ingest(&record(
                2,
                r#"{"type":"match","trade_id":10,"sequence":100,"product_id":"BTC-USD","price":"60000.10","size":"0.01","side":"sell","time":"2026-08-12T12:00:00Z"}"#,
            ))
            .expect("first trade");
        // The in-hand trade 13 is held; the pending range opens at (10, 12].
        adapter
            .ingest(&record(
                3,
                r#"{"type":"match","trade_id":13,"sequence":103,"product_id":"BTC-USD","price":"60003.00","size":"0.03","side":"sell","time":"2026-08-12T12:00:03Z"}"#,
            ))
            .expect("gap is operational");
        // Two more holes in hand: 15 skips 14, then 18 skips 16 and 17.
        for (ordinal, id, time) in [
            (4u64, 15u64, "2026-08-12T12:00:05Z"),
            (5, 18, "2026-08-12T12:00:08Z"),
        ] {
            adapter
                .ingest(&record(
                    ordinal,
                    &format!(
                        r#"{{"type":"match","trade_id":{id},"sequence":{id},"product_id":"BTC-USD","price":"600{id}.00","size":"0.01","side":"sell","time":"{time}"}}"#
                    ),
                ))
                .expect("held while paused");
        }

        // First backfill (10, 12] emits 11, 12, then held 13; the first hole (15
        // after 13) reopens over (13, 14] and re-holds the tail 15, 18.
        let first = adapter
            .apply_trade_backfill(
                10,
                12,
                "2026-08-12T12:00:08.100Z",
                vec![
                    CoinbaseRestTrade {
                        trade_id: 11,
                        price: "60011.00".to_owned(),
                        size: "0.01".to_owned(),
                        side: "sell".to_owned(),
                        time: "2026-08-12T12:00:01Z".to_owned(),
                    },
                    CoinbaseRestTrade {
                        trade_id: 12,
                        price: "60012.00".to_owned(),
                        size: "0.01".to_owned(),
                        side: "buy".to_owned(),
                        time: "2026-08-12T12:00:02Z".to_owned(),
                    },
                ],
            )
            .expect("first backfill restores the cursor");
        let first_ids: Vec<_> = first
            .events
            .iter()
            .map(|event| event.values.get("trade_id").cloned())
            .collect();
        assert_eq!(
            first_ids,
            vec![
                Some(serde_json::json!(11)),
                Some(serde_json::json!(12)),
                Some(serde_json::json!(13)),
            ]
        );
        assert_eq!(
            first.notices[0].recovery,
            Some(RecoveryAction::BackfillCoinbaseTrades {
                product_id: "BTC-USD".to_owned(),
                after_trade_id: 13,
                through_trade_id: 14,
            })
        );

        // Completing (13, 14] emits 14, resumes held 15, then the SECOND hole (18
        // after 15) recurses through the same path, reopening over (15, 17].
        let second = adapter
            .apply_trade_backfill(
                13,
                14,
                "2026-08-12T12:00:08.200Z",
                vec![CoinbaseRestTrade {
                    trade_id: 14,
                    price: "60014.00".to_owned(),
                    size: "0.01".to_owned(),
                    side: "buy".to_owned(),
                    time: "2026-08-12T12:00:04Z".to_owned(),
                }],
            )
            .expect("reopened range completes");
        let second_ids: Vec<_> = second
            .events
            .iter()
            .map(|event| event.values.get("trade_id").cloned())
            .collect();
        assert_eq!(
            second_ids,
            vec![Some(serde_json::json!(14)), Some(serde_json::json!(15))]
        );
        assert_eq!(
            second.notices[0].recovery,
            Some(RecoveryAction::BackfillCoinbaseTrades {
                product_id: "BTC-USD".to_owned(),
                after_trade_id: 15,
                through_trade_id: 17,
            })
        );

        // Completing (15, 17] emits 16, 17, then the final held tail 18 contiguously.
        let third = adapter
            .apply_trade_backfill(
                15,
                17,
                "2026-08-12T12:00:08.300Z",
                vec![
                    CoinbaseRestTrade {
                        trade_id: 16,
                        price: "60016.00".to_owned(),
                        size: "0.01".to_owned(),
                        side: "sell".to_owned(),
                        time: "2026-08-12T12:00:06Z".to_owned(),
                    },
                    CoinbaseRestTrade {
                        trade_id: 17,
                        price: "60017.00".to_owned(),
                        size: "0.01".to_owned(),
                        side: "buy".to_owned(),
                        time: "2026-08-12T12:00:07Z".to_owned(),
                    },
                ],
            )
            .expect("second reopened range completes");
        let third_ids: Vec<_> = third
            .events
            .iter()
            .map(|event| event.values.get("trade_id").cloned())
            .collect();
        assert_eq!(
            third_ids,
            vec![
                Some(serde_json::json!(16)),
                Some(serde_json::json!(17)),
                Some(serde_json::json!(18)),
            ]
        );
        assert!(third.notices.is_empty());
    }

    fn ticker(ordinal: u64, product_id: &str, sequence: u64) -> RawRecord {
        record(
            ordinal,
            &format!(
                r#"{{"type":"ticker","trade_id":{sequence},"sequence":{sequence},"product_id":"{product_id}","price":"1.0001","last_size":"100.0","best_bid":"1.0000","best_bid_size":"5000.0","best_ask":"1.0002","best_ask_size":"4000.0","open_24h":"0.9999","volume_24h":"1000000.0","low_24h":"0.9990","high_24h":"1.0010","volume_30d":"30000000.0","time":"2026-08-12T12:00:01Z"}}"#
            ),
        )
    }

    #[test]
    fn usdt_ticker_routes_to_its_own_reference_source_id() {
        let mut adapter = CoinbaseAdapter::new("test-epoch").expect("adapter constructs");
        adapter.ingest(&connected()).expect("connects");
        let outcome = adapter
            .ingest(&ticker(2, "USDT-USD", 500))
            .expect("USDT-USD ticker parses");
        assert_eq!(outcome.events.len(), 1);
        let event = &outcome.events[0];
        assert_eq!(event.source_id, "coinbase.exchange.usdt-usd.ticker");
        assert_eq!(event.sequence, 0);
        assert_eq!(
            event.values.get("instrument"),
            Some(&serde_json::json!("USDT-USD"))
        );
        assert_eq!(
            event.values.get("source_sequence"),
            Some(&serde_json::json!(500))
        );
    }

    #[test]
    fn btc_and_usdt_tickers_carry_independent_sequences() {
        let mut adapter = CoinbaseAdapter::new("test-epoch").expect("adapter constructs");
        adapter.ingest(&connected()).expect("connects");
        // BTC ticker at a high venue sequence advances only the BTC cursor.
        let btc = adapter
            .ingest(&ticker(2, "BTC-USD", 901))
            .expect("BTC-USD ticker parses");
        assert_eq!(btc.events[0].source_id, "coinbase.exchange.btc-usd.ticker");
        assert_eq!(btc.events[0].sequence, 0);
        // A USDT venue sequence below the BTC one still emits: the cursors are
        // independent, so there is no cross-product out-of-order suppression.
        let usdt = adapter
            .ingest(&ticker(3, "USDT-USD", 500))
            .expect("USDT-USD ticker parses");
        assert_eq!(
            usdt.events[0].source_id,
            "coinbase.exchange.usdt-usd.ticker"
        );
        assert_eq!(usdt.events[0].sequence, 0);
        // Each product's output sequence counter advances on its own.
        let usdt_next = adapter
            .ingest(&ticker(4, "USDT-USD", 501))
            .expect("second USDT-USD ticker parses");
        assert_eq!(usdt_next.events[0].sequence, 1);
        let btc_next = adapter
            .ingest(&ticker(5, "BTC-USD", 902))
            .expect("second BTC-USD ticker parses");
        assert_eq!(btc_next.events[0].sequence, 1);
        // Per-product dedup still holds: a repeated USDT sequence is a duplicate.
        let dup = adapter
            .ingest(&ticker(6, "USDT-USD", 501))
            .expect("duplicate is operational");
        assert!(dup.events.is_empty());
        assert_eq!(dup.notices[0].code, FeedNoticeCode::Duplicate);
    }

    #[test]
    fn foreign_ticker_product_is_refused_without_emitting() {
        let mut adapter = CoinbaseAdapter::new("test-epoch").expect("adapter constructs");
        adapter.ingest(&connected()).expect("connects");
        // A product the socket never subscribes is a foreign symbol: it is
        // refused rather than emitted under any Coinbase source id.
        assert!(adapter.ingest(&ticker(2, "ETH-USD", 700)).is_err());
    }
}
