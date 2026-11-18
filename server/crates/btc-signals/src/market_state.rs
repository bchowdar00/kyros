use std::collections::{BTreeMap, VecDeque};

use chrono::{DateTime, FixedOffset};
use rust_decimal::Decimal;
use serde_json::Value;
use signals_core::{SourceEvent, parse_decimal_text};

use crate::spot_event_join::AggressorSide;
use crate::{SpotSignalError, Venue};

pub(crate) const COINBASE_BOOK_SOURCE_ID: &str = "coinbase.exchange.btc-usd.book-levels";
pub(crate) const COINBASE_BOOK_SOURCE_VERSION: &str = "1.0.0";
pub(crate) const KRAKEN_BOOK_SOURCE_ID: &str = "kraken.spot.btc-usd.book-levels";
pub(crate) const KRAKEN_BOOK_SOURCE_VERSION: &str = "1.0.0";
pub(crate) const COINBASE_TRADE_SOURCE_ID: &str = "coinbase.exchange.btc-usd.trades";
pub(crate) const COINBASE_TRADE_SOURCE_VERSION: &str = "1.0.0";
pub(crate) const KRAKEN_TRADE_SOURCE_ID: &str = "kraken.spot.btc-usd.trades";
pub(crate) const KRAKEN_TRADE_SOURCE_VERSION: &str = "1.0.0";

pub(crate) const DEPTH_BAND_BPS: u16 = 5;
pub(crate) const ONE_SECOND_NS: u64 = 1_000_000_000;
pub(crate) const TEN_SECONDS_NS: u64 = 10 * ONE_SECOND_NS;
pub(crate) const SIXTY_SECONDS_NS: u64 = 60 * ONE_SECOND_NS;
pub(crate) const STALENESS_BOUND_NS: u64 = 10 * ONE_SECOND_NS;

const MAX_BOOK_LEVELS_PER_SIDE: usize = 100_000;
const MAX_ROLLING_EVENTS: usize = 1_000_000;

#[derive(Debug, Clone)]
struct OrderingCursor {
    publisher_epoch: String,
    sequence: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BookUpdateKind {
    Snapshot,
    Delta,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BookInvalidReason {
    InvalidBestQuote,
}

#[derive(Debug, Clone)]
pub(crate) enum BookIngestOutcome {
    Pending,
    Complete(BookCompletion),
    Invalid { transitioned: bool },
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Bbo {
    pub(crate) bid: Decimal,
    pub(crate) bid_size: Decimal,
    pub(crate) ask: Decimal,
    pub(crate) ask_size: Decimal,
}

#[derive(Debug, Clone)]
struct PendingBookGroup {
    id: String,
    kind: BookUpdateKind,
    count: u32,
    next_index: u32,
    previous_bbo: Option<Bbo>,
    event_time: String,
    parsed_event_time: DateTime<FixedOffset>,
    logical_receipt_ns: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct BookCompletion {
    pub(crate) kind: BookUpdateKind,
    pub(crate) previous_bbo: Option<Bbo>,
    pub(crate) current_bbo: Bbo,
    pub(crate) logical_receipt_ns: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct BookMetrics {
    pub(crate) midpoint: Decimal,
    pub(crate) microprice: Decimal,
    pub(crate) bid_depth: Decimal,
    pub(crate) ask_depth: Decimal,
    pub(crate) imbalance: Decimal,
    pub(crate) spread_bps: Decimal,
    pub(crate) logical_receipt_ns: u64,
    pub(crate) event_time: String,
    pub(crate) parsed_event_time: DateTime<FixedOffset>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct BookState {
    bids: BTreeMap<Decimal, Decimal>,
    asks: BTreeMap<Decimal, Decimal>,
    ordering: Option<OrderingCursor>,
    pending: Option<PendingBookGroup>,
    ready: bool,
    logical_receipt_ns: Option<u64>,
    event_time: Option<String>,
    parsed_event_time: Option<DateTime<FixedOffset>>,
    invalid_reason: Option<BookInvalidReason>,
}

impl BookState {
    pub(crate) fn clear(&mut self) {
        self.bids.clear();
        self.asks.clear();
        self.pending = None;
        self.ready = false;
        self.logical_receipt_ns = None;
        self.event_time = None;
        self.parsed_event_time = None;
        // Preserve a detected invalid-book marker across reconnect. Only a
        // complete, valid replacement snapshot is allowed to clear it.
    }

    pub(crate) fn invalid_reason(&self) -> Option<BookInvalidReason> {
        self.invalid_reason
    }

    /// Synthesize a complete book snapshot as source rows for a C5 rotation
    /// resync, rebuilt from the warm in-memory book. Admitted through the normal
    /// path as the first records of a rotation-opened generation, these let a
    /// cold replay rebuild the exact book the checkpoint does not carry.
    ///
    /// The rows are numbered to END at the current ordering cursor, so on the
    /// warm live process they are ordering-skipped (each sequence is at or below
    /// the cursor) and change nothing, while a cold replay (empty book, no
    /// cursor) seeds the cursor from them and rebuilds the book, after which the
    /// next real delta at `cursor + 1` is contiguous. Returns empty when the book
    /// is not ready or has no ordering cursor (nothing to resync).
    ///
    /// `extra_fields` are the source-specific static row fields the caller's
    /// definition requires (e.g. `instrument`, and for Kraken `configured_depth`,
    /// `change_origin`, `book_checksum`); the core group/side/price/size/
    /// event_time fields are filled here. The synthesized rows are a tagged
    /// resync, not received venue frames.
    pub(crate) fn resync_rows(
        &self,
        source_id: &str,
        source_version: &str,
        extra_fields: &[(&'static str, Value)],
    ) -> Vec<SourceEvent> {
        let cursor = match &self.ordering {
            Some(cursor) if self.ready => cursor,
            _ => return Vec::new(),
        };
        let Some(event_time) = self.event_time.clone() else {
            return Vec::new();
        };
        let levels: Vec<(&str, &Decimal, &Decimal)> = self
            .bids
            .iter()
            .map(|(price, size)| ("bid", price, size))
            .chain(self.asks.iter().map(|(price, size)| ("ask", price, size)))
            .collect();
        let count = match u32::try_from(levels.len()) {
            Ok(count) if count > 0 => count,
            _ => return Vec::new(),
        };
        // Number the group so its last row lands on the current cursor.
        let base_sequence = cursor
            .sequence
            .saturating_add(1)
            .saturating_sub(u64::from(count));
        let update_group = format!("resync-{}", cursor.sequence);
        let mut rows = Vec::with_capacity(levels.len());
        for (index, (side, price, size)) in levels.into_iter().enumerate() {
            let mut values: BTreeMap<String, Value> = extra_fields
                .iter()
                .map(|(key, value)| ((*key).to_owned(), value.clone()))
                .collect();
            values.insert("update_group".to_owned(), Value::from(update_group.clone()));
            values.insert("update_index".to_owned(), Value::from(index as u32));
            values.insert("update_count".to_owned(), Value::from(count));
            values.insert("update_kind".to_owned(), Value::from("snapshot"));
            values.insert("side".to_owned(), Value::from(side));
            values.insert("price".to_owned(), Value::from(price.to_string()));
            values.insert("size".to_owned(), Value::from(size.to_string()));
            values.insert("event_time".to_owned(), Value::from(event_time.clone()));
            rows.push(SourceEvent {
                schema_version: 1,
                source_id: source_id.to_owned(),
                source_version: source_version.to_owned(),
                publisher_epoch: cursor.publisher_epoch.clone(),
                sequence: base_sequence + index as u64,
                event_time: event_time.clone(),
                received_at: event_time.clone(),
                values,
            });
        }
        rows
    }

    pub(crate) fn ingest(
        &mut self,
        event: &SourceEvent,
        logical_receipt_ns: u64,
    ) -> Result<BookIngestOutcome, SpotSignalError> {
        if !accept_ordering(&mut self.ordering, event)? {
            return Ok(BookIngestOutcome::Pending);
        }

        let update_group = required_text(&event.values, "update_group")?;
        let update_index = required_u32(&event.values, "update_index")?;
        let update_count = required_u32(&event.values, "update_count")?;
        if update_count == 0 || update_index >= update_count {
            return Err(SpotSignalError::BookGroup(
                "book group count must be positive and index must be in range".to_owned(),
            ));
        }
        let kind = match required_text(&event.values, "update_kind")? {
            "snapshot" => BookUpdateKind::Snapshot,
            "delta" => BookUpdateKind::Delta,
            other => {
                return Err(SpotSignalError::BookGroup(format!(
                    "unsupported book update kind {other}"
                )));
            }
        };
        let parsed_event_time =
            DateTime::parse_from_rfc3339(&event.event_time).map_err(|error| {
                SpotSignalError::Timestamp {
                    field: "event_time",
                    detail: error.to_string(),
                }
            })?;

        if kind == BookUpdateKind::Delta && self.invalid_reason.is_some() {
            return Ok(BookIngestOutcome::Invalid {
                transitioned: false,
            });
        }

        if update_index == 0 {
            if self.pending.is_some() {
                return Err(SpotSignalError::BookGroup(
                    "new book group arrived before the prior group completed".to_owned(),
                ));
            }
            let previous_bbo = if kind == BookUpdateKind::Delta && self.ready {
                self.bbo()
            } else {
                None
            };
            if kind == BookUpdateKind::Snapshot {
                self.bids.clear();
                self.asks.clear();
                self.ready = false;
            } else if !self.ready {
                return Err(SpotSignalError::BookGroup(
                    "book delta arrived before a complete snapshot".to_owned(),
                ));
            }
            self.pending = Some(PendingBookGroup {
                id: update_group.to_owned(),
                kind,
                count: update_count,
                next_index: 0,
                previous_bbo,
                event_time: event.event_time.clone(),
                parsed_event_time,
                logical_receipt_ns,
            });
        }

        let pending = self.pending.as_mut().ok_or_else(|| {
            SpotSignalError::BookGroup("book group did not begin at index zero".to_owned())
        })?;
        if pending.id != update_group
            || pending.kind != kind
            || pending.count != update_count
            || pending.next_index != update_index
            || pending.logical_receipt_ns != logical_receipt_ns
            || pending.event_time != event.event_time
        {
            return Err(SpotSignalError::BookGroup(
                "book group metadata or row order changed before completion".to_owned(),
            ));
        }

        let side = required_text(&event.values, "side")?;
        let price = parse_decimal_text(required_text(&event.values, "price")?)?;
        let size = parse_decimal_text(required_text(&event.values, "size")?)?;
        if price <= Decimal::ZERO || size < Decimal::ZERO {
            return Err(SpotSignalError::MarketValue(
                "book price must be positive and size must be non-negative".to_owned(),
            ));
        }
        let levels = match side {
            "bid" => &mut self.bids,
            "ask" => &mut self.asks,
            other => {
                return Err(SpotSignalError::MarketValue(format!(
                    "unsupported book side {other}"
                )));
            }
        };
        if size.is_zero() {
            levels.remove(&price);
        } else {
            levels.insert(price, size);
            if levels.len() > MAX_BOOK_LEVELS_PER_SIDE {
                return Err(SpotSignalError::ResourceLimit(
                    "book side exceeded 100000 active levels".to_owned(),
                ));
            }
        }
        pending.next_index = pending
            .next_index
            .checked_add(1)
            .ok_or(SpotSignalError::Counter)?;

        if pending.next_index != pending.count {
            return Ok(BookIngestOutcome::Pending);
        }

        let pending = self.pending.take().ok_or_else(|| {
            SpotSignalError::BookGroup("completed book group disappeared".to_owned())
        })?;
        let Some(current_bbo) = self.bbo() else {
            let transitioned = self.invalid_reason.is_none();
            let reason = BookInvalidReason::InvalidBestQuote;
            self.bids.clear();
            self.asks.clear();
            self.ready = false;
            self.logical_receipt_ns = None;
            self.event_time = None;
            self.parsed_event_time = None;
            self.invalid_reason = Some(reason);
            return Ok(BookIngestOutcome::Invalid { transitioned });
        };
        self.ready = true;
        self.logical_receipt_ns = Some(pending.logical_receipt_ns);
        self.event_time = Some(pending.event_time);
        self.parsed_event_time = Some(pending.parsed_event_time);
        self.invalid_reason = None;
        Ok(BookIngestOutcome::Complete(BookCompletion {
            kind: pending.kind,
            previous_bbo: pending.previous_bbo,
            current_bbo,
            logical_receipt_ns,
        }))
    }

    pub(crate) fn metrics(
        &self,
        now_ns: u64,
        band_bps: u16,
    ) -> Result<Option<BookMetrics>, SpotSignalError> {
        // A provider message is atomic. Never expose the partially applied
        // state of a multi-row group, even if a caller advances the health
        // clock between rows.
        if !self.ready || self.pending.is_some() {
            return Ok(None);
        }
        let logical_receipt_ns = self.logical_receipt_ns.ok_or_else(|| {
            SpotSignalError::State("ready book is missing receipt time".to_owned())
        })?;
        let age = now_ns.checked_sub(logical_receipt_ns).ok_or(
            SpotSignalError::ReceiptAfterAdmission {
                receipt_ns: logical_receipt_ns,
                now_ns,
            },
        )?;
        if age > STALENESS_BOUND_NS {
            return Ok(None);
        }
        let bbo = self.bbo().ok_or_else(|| {
            SpotSignalError::MarketValue("ready book has no valid best quote".to_owned())
        })?;
        let midpoint = checked_mean(bbo.bid, bbo.ask).ok_or_else(|| {
            SpotSignalError::Arithmetic("book midpoint is not representable".to_owned())
        })?;
        let touch_size = bbo
            .bid_size
            .checked_add(bbo.ask_size)
            .ok_or_else(|| SpotSignalError::Arithmetic("touch size overflowed".to_owned()))?;
        if touch_size <= Decimal::ZERO {
            return Err(SpotSignalError::MarketValue(
                "touch size must be positive".to_owned(),
            ));
        }
        let microprice = bbo
            .ask
            .checked_mul(bbo.bid_size)
            .and_then(|left| {
                bbo.bid
                    .checked_mul(bbo.ask_size)
                    .and_then(|right| left.checked_add(right))
            })
            .and_then(|numerator| numerator.checked_div(touch_size))
            .ok_or_else(|| SpotSignalError::Arithmetic("microprice overflowed".to_owned()))?;
        let band = Decimal::from(band_bps)
            .checked_div(Decimal::from(10_000_u32))
            .ok_or_else(|| SpotSignalError::Arithmetic("depth band is invalid".to_owned()))?;
        let lower = midpoint
            .checked_mul(Decimal::ONE.checked_sub(band).ok_or_else(|| {
                SpotSignalError::Arithmetic("depth lower band overflowed".to_owned())
            })?)
            .ok_or_else(|| {
                SpotSignalError::Arithmetic("depth lower price overflowed".to_owned())
            })?;
        let upper = midpoint
            .checked_mul(Decimal::ONE.checked_add(band).ok_or_else(|| {
                SpotSignalError::Arithmetic("depth upper band overflowed".to_owned())
            })?)
            .ok_or_else(|| {
                SpotSignalError::Arithmetic("depth upper price overflowed".to_owned())
            })?;

        let mut bid_depth = Decimal::ZERO;
        for (_, size) in self.bids.range(lower..) {
            bid_depth = bid_depth
                .checked_add(*size)
                .ok_or_else(|| SpotSignalError::Arithmetic("bid depth overflowed".to_owned()))?;
        }
        let mut ask_depth = Decimal::ZERO;
        for (_, size) in self.asks.range(..=upper) {
            ask_depth = ask_depth
                .checked_add(*size)
                .ok_or_else(|| SpotSignalError::Arithmetic("ask depth overflowed".to_owned()))?;
        }
        let total_depth = bid_depth
            .checked_add(ask_depth)
            .ok_or_else(|| SpotSignalError::Arithmetic("total depth overflowed".to_owned()))?;
        if bid_depth <= Decimal::ZERO || ask_depth <= Decimal::ZERO || total_depth <= Decimal::ZERO
        {
            return Ok(None);
        }
        let imbalance = bid_depth
            .checked_sub(ask_depth)
            .and_then(|difference| difference.checked_div(total_depth))
            .ok_or_else(|| SpotSignalError::Arithmetic("depth imbalance overflowed".to_owned()))?;
        let spread_bps = bbo
            .ask
            .checked_sub(bbo.bid)
            .and_then(|spread| spread.checked_div(midpoint))
            .and_then(|ratio| ratio.checked_mul(Decimal::from(10_000_u32)))
            .ok_or_else(|| SpotSignalError::Arithmetic("book spread overflowed".to_owned()))?;
        Ok(Some(BookMetrics {
            midpoint,
            microprice,
            bid_depth,
            ask_depth,
            imbalance,
            spread_bps,
            logical_receipt_ns,
            event_time: self
                .event_time
                .clone()
                .ok_or_else(|| SpotSignalError::State("book event time is missing".to_owned()))?,
            parsed_event_time: self.parsed_event_time.ok_or_else(|| {
                SpotSignalError::State("parsed book event time is missing".to_owned())
            })?,
        }))
    }

    /// Whether the inclusive `band_bps` band around the current book midpoint is
    /// fully inside the captured levels (§6 Binance depth20 guard). `Some(true)`
    /// when both band edges fall within captured prices, `Some(false)` when the
    /// band reaches beyond the outermost captured level on either side, `None`
    /// when no fresh valid book exists yet.
    pub(crate) fn band_within_captured(
        &self,
        band_bps: u16,
    ) -> Result<Option<bool>, SpotSignalError> {
        if !self.ready || self.pending.is_some() {
            return Ok(None);
        }
        let Some(bbo) = self.bbo() else {
            return Ok(None);
        };
        let midpoint = checked_mean(bbo.bid, bbo.ask).ok_or_else(|| {
            SpotSignalError::Arithmetic("book midpoint is not representable".to_owned())
        })?;
        let band = Decimal::from(band_bps)
            .checked_div(Decimal::from(10_000_u32))
            .ok_or_else(|| SpotSignalError::Arithmetic("depth band is invalid".to_owned()))?;
        let lower = midpoint
            .checked_mul(Decimal::ONE.checked_sub(band).ok_or_else(|| {
                SpotSignalError::Arithmetic("depth lower band overflowed".to_owned())
            })?)
            .ok_or_else(|| {
                SpotSignalError::Arithmetic("depth lower price overflowed".to_owned())
            })?;
        let upper = midpoint
            .checked_mul(Decimal::ONE.checked_add(band).ok_or_else(|| {
                SpotSignalError::Arithmetic("depth upper band overflowed".to_owned())
            })?)
            .ok_or_else(|| {
                SpotSignalError::Arithmetic("depth upper price overflowed".to_owned())
            })?;
        let Some(lowest_bid) = self.bids.keys().next().copied() else {
            return Ok(None);
        };
        let Some(highest_ask) = self.asks.keys().next_back().copied() else {
            return Ok(None);
        };
        Ok(Some(lower >= lowest_bid && upper <= highest_ask))
    }

    /// This venue's best quote when the book is ready: the best bid/ask rows plus
    /// the book's event time and logical receipt time. Used by the Bybit
    /// one-level-book quote adapter (B1) and never by the USD spot lane.
    pub(crate) fn quote(&self) -> Option<(Bbo, String, u64)> {
        if !self.ready || self.pending.is_some() {
            return None;
        }
        let bbo = self.bbo()?;
        Some((bbo, self.event_time.clone()?, self.logical_receipt_ns?))
    }

    fn bbo(&self) -> Option<Bbo> {
        let (&bid, &bid_size) = self.bids.iter().next_back()?;
        let (&ask, &ask_size) = self.asks.iter().next()?;
        (bid > Decimal::ZERO
            && ask > Decimal::ZERO
            && bid <= ask
            && bid_size > Decimal::ZERO
            && ask_size > Decimal::ZERO)
            .then_some(Bbo {
                bid,
                bid_size,
                ask,
                ask_size,
            })
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct FlowTotals {
    pub(crate) signed_notional: Decimal,
    pub(crate) signed_base: Decimal,
    pub(crate) gross_notional: Decimal,
    pub(crate) base_volume: Decimal,
    pub(crate) count: u64,
}

impl FlowTotals {
    fn checked_add(self, other: Self) -> Result<Self, SpotSignalError> {
        Ok(Self {
            signed_notional: checked_decimal_add(self.signed_notional, other.signed_notional)?,
            signed_base: checked_decimal_add(self.signed_base, other.signed_base)?,
            gross_notional: checked_decimal_add(self.gross_notional, other.gross_notional)?,
            base_volume: checked_decimal_add(self.base_volume, other.base_volume)?,
            count: self
                .count
                .checked_add(other.count)
                .ok_or(SpotSignalError::Counter)?,
        })
    }

    fn checked_sub(self, other: Self) -> Result<Self, SpotSignalError> {
        Ok(Self {
            signed_notional: checked_decimal_sub(self.signed_notional, other.signed_notional)?,
            signed_base: checked_decimal_sub(self.signed_base, other.signed_base)?,
            gross_notional: checked_decimal_sub(self.gross_notional, other.gross_notional)?,
            base_volume: checked_decimal_sub(self.base_volume, other.base_volume)?,
            count: self
                .count
                .checked_sub(other.count)
                .ok_or(SpotSignalError::Counter)?,
        })
    }
}

#[derive(Debug, Clone, Copy)]
struct FlowPrefix {
    logical_receipt_ns: u64,
    totals: FlowTotals,
}

#[derive(Debug, Clone)]
struct VenueFlowSeries {
    entries: VecDeque<FlowPrefix>,
    cumulative: FlowTotals,
    observed_since_ns: Option<u64>,
    last_receipt_ns: Option<u64>,
}

impl Default for VenueFlowSeries {
    fn default() -> Self {
        Self {
            entries: VecDeque::with_capacity(2048),
            cumulative: FlowTotals::default(),
            observed_since_ns: None,
            last_receipt_ns: None,
        }
    }
}

impl VenueFlowSeries {
    fn clear(&mut self) {
        self.entries.clear();
        self.cumulative = FlowTotals::default();
        self.observed_since_ns = None;
        self.last_receipt_ns = None;
    }

    fn window(&self, now_ns: u64, window_ns: u64) -> Result<FlowTotals, SpotSignalError> {
        let Some(last) = self.entries.back() else {
            return Ok(FlowTotals::default());
        };
        let cutoff = now_ns.saturating_sub(window_ns);
        let baseline = last_prefix_at_or_before(&self.entries, cutoff).map(|entry| entry.totals);
        match baseline {
            Some(value) => last.totals.checked_sub(value),
            None => Ok(last.totals),
        }
    }

    fn prune(&mut self, now_ns: u64) {
        let cutoff = now_ns.saturating_sub(SIXTY_SECONDS_NS + ONE_SECOND_NS);
        while self.entries.len() > 1
            && self
                .entries
                .get(1)
                .is_some_and(|entry| entry.logical_receipt_ns < cutoff)
        {
            self.entries.pop_front();
        }
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct FlowSeries {
    coinbase: VenueFlowSeries,
    kraken: VenueFlowSeries,
    /// Per-venue rolling trade history for the rights-gated venues (B1). The
    /// blended `btc.external.*` reductions (`window`, `warmed`, `venues_fresh`)
    /// read only `coinbase`/`kraken`, so these entries never widen the blend; a
    /// single-venue lane owns its own `FlowSeries` and drives exactly one entry.
    extra: BTreeMap<Venue, VenueFlowSeries>,
}

impl FlowSeries {
    pub(crate) fn clear(&mut self, _now_ns: u64) {
        self.coinbase.clear();
        self.kraken.clear();
        for series in self.extra.values_mut() {
            series.clear();
        }
    }

    /// The mutable series for `venue`: a named blend field for the USD venues, or
    /// a per-venue `extra` entry (created on first write) for a rights-gated one.
    fn series_mut(&mut self, venue: Venue) -> &mut VenueFlowSeries {
        match venue {
            Venue::Coinbase => &mut self.coinbase,
            Venue::Kraken => &mut self.kraken,
            other => self.extra.entry(other).or_default(),
        }
    }

    pub(crate) fn ingest(
        &mut self,
        venue: Venue,
        event: &SourceEvent,
        logical_receipt_ns: u64,
    ) -> Result<(), SpotSignalError> {
        if is_replayed_trade_snapshot(venue, event)? {
            self.kraken
                .observed_since_ns
                .get_or_insert(logical_receipt_ns);
            return Ok(());
        }
        let mut current_len = self
            .coinbase
            .entries
            .len()
            .checked_add(self.kraken.entries.len())
            .ok_or(SpotSignalError::Counter)?;
        for series in self.extra.values() {
            current_len = current_len
                .checked_add(series.entries.len())
                .ok_or(SpotSignalError::Counter)?;
        }
        if current_len >= MAX_ROLLING_EVENTS {
            return Err(SpotSignalError::ResourceLimit(
                "rolling trade history exceeded 1000000 events".to_owned(),
            ));
        }
        let series = self.series_mut(venue);
        series.observed_since_ns.get_or_insert(logical_receipt_ns);
        if series
            .last_receipt_ns
            .is_some_and(|previous| logical_receipt_ns < previous)
        {
            return Err(SpotSignalError::TimeRegression);
        }
        let AdmittedTrade {
            price,
            size,
            aggressor,
        } = parse_admitted_trade(event)?;
        let notional = price
            .checked_mul(size)
            .ok_or_else(|| SpotSignalError::Arithmetic("trade notional overflowed".to_owned()))?;
        let sign = aggressor.sign();
        let contribution = FlowTotals {
            signed_notional: notional.checked_mul(sign).ok_or_else(|| {
                SpotSignalError::Arithmetic("signed trade notional overflowed".to_owned())
            })?,
            signed_base: size.checked_mul(sign).ok_or_else(|| {
                SpotSignalError::Arithmetic("signed trade size overflowed".to_owned())
            })?,
            gross_notional: notional,
            base_volume: size,
            count: 1,
        };
        series.cumulative = series.cumulative.checked_add(contribution)?;
        series.entries.push_back(FlowPrefix {
            logical_receipt_ns,
            totals: series.cumulative,
        });
        series.last_receipt_ns = Some(logical_receipt_ns);
        series.prune(logical_receipt_ns);
        Ok(())
    }

    /// Clear one venue's rolling trade history, leaving the other venue's
    /// untouched. Used by the per-venue identity lanes, whose readings must
    /// re-warm only for the venue that dropped (ADR 0024).
    pub(crate) fn clear_venue(&mut self, venue: Venue) {
        self.series_mut(venue).clear();
    }

    pub(crate) fn window(
        &self,
        now_ns: u64,
        window_ns: u64,
    ) -> Result<FlowTotals, SpotSignalError> {
        self.coinbase
            .window(now_ns, window_ns)?
            .checked_add(self.kraken.window(now_ns, window_ns)?)
    }

    pub(crate) fn venue_window(
        &self,
        venue: Venue,
        now_ns: u64,
        window_ns: u64,
    ) -> Result<FlowTotals, SpotSignalError> {
        match self.series(venue) {
            Some(series) => series.window(now_ns, window_ns),
            None => Ok(FlowTotals::default()),
        }
    }

    /// One venue's own 60-second observation warm-up, the per-venue half of the
    /// same rule [`FlowSeries::warmed`] applies across every venue.
    pub(crate) fn venue_warmed(&self, venue: Venue, now_ns: u64) -> bool {
        self.series(venue)
            .is_some_and(|series| observed_for_full_window(series.observed_since_ns, now_ns))
    }

    /// One venue's own trade-receipt freshness, the per-venue half of
    /// [`FlowSeries::venues_fresh`].
    pub(crate) fn venue_fresh(&self, venue: Venue, now_ns: u64) -> bool {
        self.series(venue)
            .is_some_and(|series| receipt_within_staleness_bound(series.last_receipt_ns, now_ns))
    }

    /// The series for `venue`, or `None` for a rights-gated venue not yet
    /// observed (treated as empty: not warmed, not fresh, a zero window).
    fn series(&self, venue: Venue) -> Option<&VenueFlowSeries> {
        match venue {
            Venue::Coinbase => Some(&self.coinbase),
            Venue::Kraken => Some(&self.kraken),
            other => self.extra.get(&other),
        }
    }

    pub(crate) fn warmed(&self, now_ns: u64) -> bool {
        [
            self.coinbase.observed_since_ns,
            self.kraken.observed_since_ns,
        ]
        .into_iter()
        .all(|value| observed_for_full_window(value, now_ns))
    }

    pub(crate) fn venues_fresh(&self, now_ns: u64) -> bool {
        [self.coinbase.last_receipt_ns, self.kraken.last_receipt_ns]
            .into_iter()
            .all(|value| receipt_within_staleness_bound(value, now_ns))
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct OfiTotals {
    pub(crate) value: Decimal,
    pub(crate) count: u64,
}

impl OfiTotals {
    fn checked_add(self, value: Decimal) -> Result<Self, SpotSignalError> {
        Ok(Self {
            value: checked_decimal_add(self.value, value)?,
            count: self.count.checked_add(1).ok_or(SpotSignalError::Counter)?,
        })
    }

    fn checked_sub(self, other: Self) -> Result<Self, SpotSignalError> {
        Ok(Self {
            value: checked_decimal_sub(self.value, other.value)?,
            count: self
                .count
                .checked_sub(other.count)
                .ok_or(SpotSignalError::Counter)?,
        })
    }

    fn checked_merge(self, other: Self) -> Result<Self, SpotSignalError> {
        Ok(Self {
            value: checked_decimal_add(self.value, other.value)?,
            count: self
                .count
                .checked_add(other.count)
                .ok_or(SpotSignalError::Counter)?,
        })
    }
}

#[derive(Debug, Clone, Copy)]
struct OfiPrefix {
    logical_receipt_ns: u64,
    totals: OfiTotals,
}

#[derive(Debug, Clone)]
struct VenueOfiSeries {
    entries: VecDeque<OfiPrefix>,
    cumulative: OfiTotals,
    observed_since_ns: Option<u64>,
    last_receipt_ns: Option<u64>,
}

impl Default for VenueOfiSeries {
    fn default() -> Self {
        Self {
            entries: VecDeque::with_capacity(2048),
            cumulative: OfiTotals::default(),
            observed_since_ns: None,
            last_receipt_ns: None,
        }
    }
}

impl VenueOfiSeries {
    fn clear(&mut self) {
        self.entries.clear();
        self.cumulative = OfiTotals::default();
        self.observed_since_ns = None;
        self.last_receipt_ns = None;
    }

    fn window(&self, now_ns: u64, window_ns: u64) -> Result<OfiTotals, SpotSignalError> {
        let Some(last) = self.entries.back() else {
            return Ok(OfiTotals::default());
        };
        let cutoff = now_ns.saturating_sub(window_ns);
        let baseline = last_prefix_at_or_before(&self.entries, cutoff).map(|entry| entry.totals);
        match baseline {
            Some(value) => last.totals.checked_sub(value),
            None => Ok(last.totals),
        }
    }

    fn prune(&mut self, now_ns: u64) {
        let cutoff = now_ns.saturating_sub(SIXTY_SECONDS_NS + ONE_SECOND_NS);
        while self.entries.len() > 1
            && self
                .entries
                .get(1)
                .is_some_and(|entry| entry.logical_receipt_ns < cutoff)
        {
            self.entries.pop_front();
        }
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct OfiSeries {
    coinbase: VenueOfiSeries,
    kraken: VenueOfiSeries,
    /// Per-venue rolling OFI history for the rights-gated venues (B1); see the
    /// note on [`FlowSeries`]. The blended reductions read only the named USD
    /// fields, so these entries never widen the `btc.external.*` blend.
    extra: BTreeMap<Venue, VenueOfiSeries>,
}

impl OfiSeries {
    pub(crate) fn clear(&mut self, _now_ns: u64) {
        self.coinbase.clear();
        self.kraken.clear();
        for series in self.extra.values_mut() {
            series.clear();
        }
    }

    pub(crate) fn clear_venue(&mut self, venue: Venue) {
        self.series_mut(venue).clear();
    }

    /// The mutable series for `venue`; see [`FlowSeries::series_mut`].
    fn series_mut(&mut self, venue: Venue) -> &mut VenueOfiSeries {
        match venue {
            Venue::Coinbase => &mut self.coinbase,
            Venue::Kraken => &mut self.kraken,
            other => self.extra.entry(other).or_default(),
        }
    }

    pub(crate) fn ingest(
        &mut self,
        venue: Venue,
        completion: &BookCompletion,
    ) -> Result<(), SpotSignalError> {
        if completion.kind == BookUpdateKind::Snapshot {
            let series = self.series_mut(venue);
            series
                .observed_since_ns
                .get_or_insert(completion.logical_receipt_ns);
            return Ok(());
        }
        let Some(previous) = completion.previous_bbo else {
            return Err(SpotSignalError::State(
                "book delta did not preserve its prior best quote".to_owned(),
            ));
        };
        let mut current_len = self
            .coinbase
            .entries
            .len()
            .checked_add(self.kraken.entries.len())
            .ok_or(SpotSignalError::Counter)?;
        for series in self.extra.values() {
            current_len = current_len
                .checked_add(series.entries.len())
                .ok_or(SpotSignalError::Counter)?;
        }
        if current_len >= MAX_ROLLING_EVENTS {
            return Err(SpotSignalError::ResourceLimit(
                "rolling OFI history exceeded 1000000 events".to_owned(),
            ));
        }
        let series = self.series_mut(venue);
        series
            .observed_since_ns
            .get_or_insert(completion.logical_receipt_ns);
        if series
            .last_receipt_ns
            .is_some_and(|previous| completion.logical_receipt_ns < previous)
        {
            return Err(SpotSignalError::TimeRegression);
        }
        let value = bbo_ofi(previous, completion.current_bbo)?;
        series.cumulative = series.cumulative.checked_add(value)?;
        series.entries.push_back(OfiPrefix {
            logical_receipt_ns: completion.logical_receipt_ns,
            totals: series.cumulative,
        });
        series.last_receipt_ns = Some(completion.logical_receipt_ns);
        series.prune(completion.logical_receipt_ns);
        Ok(())
    }

    /// Feed one full-image OFI observation for a venue whose displayed depth
    /// arrives as periodic top-of-book images rather than incremental deltas
    /// (Binance `depth20@100ms`, spec §6 row 4). Pass `previous_bbo: None` for a
    /// baseline image — the first image after a connect or an `image_kind`
    /// `reseed` — which is observed but contributes no flow; two consecutive
    /// `full_replace` images contribute the same BBO order-flow imbalance a delta
    /// would, computed between their best quotes.
    ///
    /// Additive by construction: [`OfiSeries::ingest`] and every path the USD
    /// spot lane drives are untouched, so a venue that streams real
    /// snapshot/delta books (Bybit `orderbook.200`) stays on `ingest` and cannot
    /// be counted twice.
    pub(crate) fn ingest_image(
        &mut self,
        venue: Venue,
        previous_bbo: Option<Bbo>,
        current_bbo: Bbo,
        logical_receipt_ns: u64,
    ) -> Result<(), SpotSignalError> {
        let Some(previous) = previous_bbo else {
            // A baseline image (a `reseed`, or the first image after a connect) is
            // a causal break: the prior image sequence is void, so the rolling OFI
            // history AND its warm-up window restart from this image rather than
            // continuing (spec §6 row 4). Retaining a stale `observed_since_ns`
            // across a reseed would let a post-reseed window claim a warm-up it
            // never had; a `get_or_insert` would do exactly that. It contributes
            // no flow.
            let series = self.series_mut(venue);
            series.clear();
            series.observed_since_ns = Some(logical_receipt_ns);
            return Ok(());
        };
        let mut current_len = self
            .coinbase
            .entries
            .len()
            .checked_add(self.kraken.entries.len())
            .ok_or(SpotSignalError::Counter)?;
        for series in self.extra.values() {
            current_len = current_len
                .checked_add(series.entries.len())
                .ok_or(SpotSignalError::Counter)?;
        }
        if current_len >= MAX_ROLLING_EVENTS {
            return Err(SpotSignalError::ResourceLimit(
                "rolling OFI history exceeded 1000000 events".to_owned(),
            ));
        }
        let series = self.series_mut(venue);
        series.observed_since_ns.get_or_insert(logical_receipt_ns);
        if series
            .last_receipt_ns
            .is_some_and(|last| logical_receipt_ns < last)
        {
            return Err(SpotSignalError::TimeRegression);
        }
        let value = bbo_ofi(previous, current_bbo)?;
        series.cumulative = series.cumulative.checked_add(value)?;
        series.entries.push_back(OfiPrefix {
            logical_receipt_ns,
            totals: series.cumulative,
        });
        series.last_receipt_ns = Some(logical_receipt_ns);
        series.prune(logical_receipt_ns);
        Ok(())
    }

    pub(crate) fn window(&self, now_ns: u64, window_ns: u64) -> Result<OfiTotals, SpotSignalError> {
        self.coinbase
            .window(now_ns, window_ns)?
            .checked_merge(self.kraken.window(now_ns, window_ns)?)
    }

    pub(crate) fn venue_window(
        &self,
        venue: Venue,
        now_ns: u64,
        window_ns: u64,
    ) -> Result<OfiTotals, SpotSignalError> {
        match self.series(venue) {
            Some(series) => series.window(now_ns, window_ns),
            None => Ok(OfiTotals::default()),
        }
    }

    pub(crate) fn warmed(&self, now_ns: u64) -> bool {
        [
            self.coinbase.observed_since_ns,
            self.kraken.observed_since_ns,
        ]
        .into_iter()
        .all(|value| observed_for_full_window(value, now_ns))
    }

    /// One venue's own 60-second observation warm-up, the per-venue half of the
    /// same rule [`OfiSeries::warmed`] applies across every venue.
    pub(crate) fn venue_warmed(&self, venue: Venue, now_ns: u64) -> bool {
        self.series(venue)
            .is_some_and(|series| observed_for_full_window(series.observed_since_ns, now_ns))
    }

    /// The series for `venue`, or `None` for a rights-gated venue not yet
    /// observed; see [`FlowSeries::series`].
    fn series(&self, venue: Venue) -> Option<&VenueOfiSeries> {
        match venue {
            Venue::Coinbase => Some(&self.coinbase),
            Venue::Kraken => Some(&self.kraken),
            other => self.extra.get(&other),
        }
    }
}

/// True when a series has been observed continuously for a full 60-second
/// window ending at `now_ns`. Sole authority for the warm-up rule that the
/// blended and per-venue readings both apply.
fn observed_for_full_window(observed_since_ns: Option<u64>, now_ns: u64) -> bool {
    observed_since_ns.is_some_and(|observed| {
        now_ns
            .checked_sub(observed)
            .is_some_and(|age| age >= SIXTY_SECONDS_NS)
    })
}

/// True when the most recent receipt is inside the staleness bound at `now_ns`.
/// Sole authority for the freshness rule the blended and per-venue readings
/// both apply.
fn receipt_within_staleness_bound(last_receipt_ns: Option<u64>, now_ns: u64) -> bool {
    last_receipt_ns.is_some_and(|receipt| {
        now_ns
            .checked_sub(receipt)
            .is_some_and(|age| age <= STALENESS_BOUND_NS)
    })
}

pub(crate) fn checked_mean(left: Decimal, right: Decimal) -> Option<Decimal> {
    let (lower, upper) = if left <= right {
        (left, right)
    } else {
        (right, left)
    };
    let difference = upper.checked_sub(lower)?;
    let half = difference.checked_div(Decimal::from(2_u8))?;
    lower.checked_add(half)
}

fn accept_ordering(
    ordering: &mut Option<OrderingCursor>,
    event: &SourceEvent,
) -> Result<bool, SpotSignalError> {
    match ordering {
        None => {
            *ordering = Some(OrderingCursor {
                publisher_epoch: event.publisher_epoch.clone(),
                sequence: event.sequence,
            });
            Ok(true)
        }
        Some(current) if current.publisher_epoch != event.publisher_epoch => {
            Err(SpotSignalError::Ordering(
                "source publisher epoch changed inside one signal process".to_owned(),
            ))
        }
        Some(current) if event.sequence <= current.sequence => Ok(false),
        Some(current) => {
            let expected = current
                .sequence
                .checked_add(1)
                .ok_or(SpotSignalError::Counter)?;
            if event.sequence != expected {
                return Err(SpotSignalError::Ordering(format!(
                    "source sequence jumped from {} to {}",
                    current.sequence, event.sequence
                )));
            }
            current.sequence = event.sequence;
            Ok(true)
        }
    }
}

fn bbo_ofi(previous: Bbo, current: Bbo) -> Result<Decimal, SpotSignalError> {
    let mut value = Decimal::ZERO;
    if current.bid >= previous.bid {
        value = checked_decimal_add(value, current.bid_size)?;
    }
    if current.bid <= previous.bid {
        value = checked_decimal_sub(value, previous.bid_size)?;
    }
    if current.ask <= previous.ask {
        value = checked_decimal_sub(value, current.ask_size)?;
    }
    if current.ask >= previous.ask {
        value = checked_decimal_add(value, previous.ask_size)?;
    }
    Ok(value)
}

fn last_prefix_at_or_before<T: PrefixTime>(entries: &VecDeque<T>, cutoff: u64) -> Option<&T> {
    let mut low = 0_usize;
    let mut high = entries.len();
    while low < high {
        let mid = low + (high - low) / 2;
        let entry = entries.get(mid)?;
        if entry.prefix_time() <= cutoff {
            low = mid + 1;
        } else {
            high = mid;
        }
    }
    low.checked_sub(1).and_then(|index| entries.get(index))
}

trait PrefixTime {
    fn prefix_time(&self) -> u64;
}

impl PrefixTime for FlowPrefix {
    fn prefix_time(&self) -> u64 {
        self.logical_receipt_ns
    }
}

impl PrefixTime for OfiPrefix {
    fn prefix_time(&self) -> u64 {
        self.logical_receipt_ns
    }
}

/// One admitted trade row, parsed once.
///
/// The rolling flow series and the per-venue microstructure join both read the
/// same three wire fields. Parsing them here keeps one authority for what an
/// admitted trade IS, so the two consumers cannot disagree about a price, a
/// size, or an aggressor side (STANDING-PRACTICES rule 6).
#[derive(Debug, Clone, Copy)]
pub(crate) struct AdmittedTrade {
    pub(crate) price: Decimal,
    pub(crate) size: Decimal,
    pub(crate) aggressor: AggressorSide,
}

/// Whether this trade row is a Kraken subscription snapshot — a replay of trade
/// history at subscribe time rather than live flow. Sole authority for that
/// classification: both the rolling flow series and the microstructure join
/// exclude such rows, and neither re-derives the test.
pub(crate) fn is_replayed_trade_snapshot(
    venue: Venue,
    event: &SourceEvent,
) -> Result<bool, SpotSignalError> {
    Ok(venue == Venue::Kraken && required_text(&event.values, "update_kind")? == "snapshot")
}

pub(crate) fn parse_admitted_trade(event: &SourceEvent) -> Result<AdmittedTrade, SpotSignalError> {
    let price = parse_decimal_text(required_text(&event.values, "price")?)?;
    // Binance agg-trades name the base quantity `quantity`; every other adopted
    // trade source uses `size`. Prefer `size`, fall back to `quantity`.
    let size_text = match required_text(&event.values, "size") {
        Ok(text) => text,
        Err(_) => required_text(&event.values, "quantity")?,
    };
    let size = parse_decimal_text(size_text)?;
    if price <= Decimal::ZERO || size <= Decimal::ZERO {
        return Err(SpotSignalError::MarketValue(
            "trade price and size must be positive".to_owned(),
        ));
    }
    let aggressor = match required_text(&event.values, "aggressor_side")? {
        "buy" => AggressorSide::Buy,
        "sell" => AggressorSide::Sell,
        other => {
            return Err(SpotSignalError::MarketValue(format!(
                "unsupported aggressor side {other}"
            )));
        }
    };
    Ok(AdmittedTrade {
        price,
        size,
        aggressor,
    })
}

fn required_text<'a>(
    values: &'a BTreeMap<String, Value>,
    name: &str,
) -> Result<&'a str, SpotSignalError> {
    values
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| SpotSignalError::MissingField(name.to_owned()))
}

fn required_u32(values: &BTreeMap<String, Value>, name: &str) -> Result<u32, SpotSignalError> {
    values
        .get(name)
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| SpotSignalError::MissingField(name.to_owned()))
}

fn checked_decimal_add(left: Decimal, right: Decimal) -> Result<Decimal, SpotSignalError> {
    left.checked_add(right)
        .ok_or_else(|| SpotSignalError::Arithmetic("decimal addition overflowed".to_owned()))
}

fn checked_decimal_sub(left: Decimal, right: Decimal) -> Result<Decimal, SpotSignalError> {
    left.checked_sub(right)
        .ok_or_else(|| SpotSignalError::Arithmetic("decimal subtraction overflowed".to_owned()))
}
