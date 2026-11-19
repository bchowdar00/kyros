//! The round's book-midpoint history, one per series.
//!
//! The probability chart plots what the market thinks, not a formula over
//! the BTC price, so it needs the YES midpoint over the round. The engine
//! keeps no history of its own; this buffer sits beside the series'
//! broadcast channel and is fed by [`super::broadcast_book`], which every
//! book mutation already calls under the engine lock. A client joining
//! mid-round receives the buffer in its first `/ws/book` frame and then
//! appends from the `mid` on every later frame.
//!
//! A point is recorded only when the midpoint moves, so a market maker
//! requoting the same spread ten times a second costs nothing; a new
//! round clears the buffer because the previous round's prices say
//! nothing about this one.

use std::collections::VecDeque;

use axum::Json;
use axum::extract::{Query, State};
use serde::Serialize;

use super::{AppState, BOOK_WS_DEPTH, BookSnapshotMessage, SeriesQuery, build_book_snapshot_message};
use crate::engine::MarketEngine;
use crate::engine::book::{MAX_MARK_TENTHS, MIN_MARK_TENTHS};
use crate::types::{DEFAULT_SERIES, MarketId, SeriesId};

/// Upper bound on points kept for one round. The market maker refreshes
/// on every price tick (5/s) and a 15-minute round is 4,500 ticks; twice
/// that leaves room for taker flow between requotes. Beyond the cap the
/// oldest point goes, which only ever trims the start of a very busy
/// round.
pub const MID_HISTORY_CAP: usize = 10_000;

/// A spread wider than this (25 cents) is a gap, not a market: on a thin
/// book a maker pulling one quote for a fraction of a second leaves the
/// next level tens of cents away, and the midpoint across it is a price
/// nobody quoted.
pub const WIDE_SPREAD_TENTHS: u16 = 250;

/// How long a gap has to stand before its midpoint is charted. Shorter
/// than this, the chart holds its last value.
pub const WIDE_SETTLE_MS: u64 = 3_000;

/// One sample of the YES midpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct MidPoint {
    pub ts_ms: u64,
    /// YES midpoint in tenths of a cent; see `Book::chart_mid_yes_tenths`.
    pub mid: u16,
}

#[derive(Debug, Default)]
pub struct MidHistory {
    market_id: Option<MarketId>,
    points: VecDeque<MidPoint>,
    /// When the book's spread last became wider than
    /// [`WIDE_SPREAD_TENTHS`]; `None` while it is not.
    wide_since_ms: Option<u64>,
}

impl MidHistory {
    /// Bring the buffer up to date with the engine's book. Clears on a
    /// new round, then records the midpoint if it moved. Returns the
    /// charted midpoint, which is what the broadcast frame carries.
    ///
    /// While the spread is a gap that has not yet stood for
    /// [`WIDE_SETTLE_MS`], the charted midpoint is the last one recorded
    /// (the first point of a round has nothing to hold and is recorded
    /// as it is).
    pub fn record(&mut self, engine: &MarketEngine, now_ms: u64) -> Option<u16> {
        let market_id = engine.current_market().map(|m| m.id);
        if market_id != self.market_id {
            self.market_id = market_id;
            self.points.clear();
            self.wide_since_ms = None;
        }
        let book = engine.book();
        let mid = book.chart_mid_yes_tenths()?;
        let bid = book.best_bid().map_or(MIN_MARK_TENTHS, |p| p.tenths());
        let ask = book.best_ask().map_or(MAX_MARK_TENTHS, |p| p.tenths());
        if ask.saturating_sub(bid) > WIDE_SPREAD_TENTHS {
            let since = *self.wide_since_ms.get_or_insert(now_ms);
            if now_ms.saturating_sub(since) < WIDE_SETTLE_MS
                && let Some(held) = self.latest()
            {
                return Some(held);
            }
        } else {
            self.wide_since_ms = None;
        }
        if self.points.back().map(|p| p.mid) != Some(mid) {
            if self.points.len() == MID_HISTORY_CAP {
                self.points.pop_front();
            }
            self.points.push_back(MidPoint { ts_ms: now_ms, mid });
        }
        Some(mid)
    }

    pub fn market_id(&self) -> Option<MarketId> {
        self.market_id
    }

    /// The round's points, oldest first.
    pub fn points(&self) -> Vec<MidPoint> {
        self.points.iter().copied().collect()
    }

    /// The most recent midpoint, if the round has printed one.
    pub fn latest(&self) -> Option<u16> {
        self.points.back().map(|p| p.mid)
    }
}

/// The frame a `/ws/book` client receives on join: the current book plus
/// the round's midpoint history, so the probability chart is complete
/// from the first frame. Takes the engine lock, then the history lock,
/// the same order [`super::broadcast_book`] uses.
pub fn build_book_join_message(state: &AppState, series: SeriesId) -> BookSnapshotMessage {
    let exchange = state.exchange.lock().expect("exchange mutex poisoned");
    let mut message = build_book_snapshot_message(exchange.engine(series), BOOK_WS_DEPTH);
    let history = state.channels_for(series).mid_history.lock().expect("mid history mutex poisoned");
    message.mid_history = Some(history.points());
    message
}

/// `GET /api/book/mid-history?series=`.
#[derive(Debug, Clone, Serialize)]
pub struct MidHistoryResponse {
    pub series: SeriesId,
    pub market_id: Option<u32>,
    /// The round's YES midpoints, oldest first.
    pub points: Vec<MidPoint>,
}

/// The round's midpoint history for callers without a socket; the same
/// points the `/ws/book` join frame carries.
pub async fn get_mid_history(
    State(state): State<AppState>,
    Query(q): Query<SeriesQuery>,
) -> Json<MidHistoryResponse> {
    let series = q.series.unwrap_or(DEFAULT_SERIES);
    let history = state.channels_for(series).mid_history.lock().expect("mid history mutex poisoned");
    Json(MidHistoryResponse {
        series,
        market_id: history.market_id().map(|id| id.0),
        points: history.points(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Exchange, TradeSource};
    use crate::series::SERIES;
    use crate::ticks::PriceTick;
    use crate::types::{Address, Qty, SeriesId, Side, Tenths};

    const S: SeriesId = SeriesId(0);

    fn exchange_with_round() -> (Exchange, Address, Address) {
        let mut x = Exchange::new(Tenths(1_000_000), SERIES);
        x.open_first_market(S, 100_000.0, 0, S.duration_secs());
        let buyer = Address([1u8; 20]);
        let seller = Address([2u8; 20]);
        x.get_or_create_account(buyer);
        x.get_or_create_account(seller);
        (x, buyer, seller)
    }

    fn rest(x: &mut Exchange, account: Address, side: Side, tenths: u16) {
        let price = PriceTick::from_tenths(tenths).expect("test price on the grid");
        x.place_limit(S, account, side, price, Qty(1), TradeSource::Ui).expect("order rests");
    }

    fn quote(x: &mut Exchange, buyer: Address, seller: Address, bid: u16, ask: u16) {
        let (engine, ledger) = x.engine_and_ledger(S);
        engine.cancel_resting_for(ledger, buyer);
        engine.cancel_resting_for(ledger, seller);
        rest(x, buyer, Side::Bid, bid);
        rest(x, seller, Side::Ask, ask);
    }

    #[test]
    fn records_only_when_the_midpoint_moves() {
        let (mut x, buyer, seller) = exchange_with_round();
        let mut history = MidHistory::default();
        assert_eq!(history.record(x.engine(S), 1), None, "empty book prints nothing");
        assert!(history.points().is_empty());

        quote(&mut x, buyer, seller, 400, 500);
        assert_eq!(history.record(x.engine(S), 2), Some(450));
        // Same book again: no new point.
        assert_eq!(history.record(x.engine(S), 3), Some(450));
        assert_eq!(history.points().len(), 1);

        quote(&mut x, buyer, seller, 420, 500);
        assert_eq!(history.record(x.engine(S), 4), Some(460));
        assert_eq!(
            history.points(),
            vec![MidPoint { ts_ms: 2, mid: 450 }, MidPoint { ts_ms: 4, mid: 460 }]
        );
        assert_eq!(history.latest(), Some(460));
    }

    /// A quote pulled for a moment leaves the next level a long way off.
    /// The midpoint across that gap is a price nobody saw, so the chart
    /// holds its last value and the frame carries the held value too.
    #[test]
    fn a_gap_that_opens_for_a_moment_is_not_charted() {
        let (mut x, buyer, seller) = exchange_with_round();
        let mut history = MidHistory::default();
        quote(&mut x, buyer, seller, 290, 310);
        assert_eq!(history.record(x.engine(S), 1_000), Some(300));

        // The near ask is pulled; the next one rests at 90 cents.
        quote(&mut x, buyer, seller, 290, 900);
        assert_eq!(history.record(x.engine(S), 1_200), Some(300), "the gap is not a price");

        quote(&mut x, buyer, seller, 300, 320);
        assert_eq!(history.record(x.engine(S), 1_500), Some(310));
        assert_eq!(
            history.points(),
            vec![MidPoint { ts_ms: 1_000, mid: 300 }, MidPoint { ts_ms: 1_500, mid: 310 }]
        );
    }

    /// A gap that stands is the market: once it has lasted
    /// [`WIDE_SETTLE_MS`] its midpoint is charted like any other.
    #[test]
    fn a_gap_that_stands_is_charted() {
        let (mut x, buyer, seller) = exchange_with_round();
        let mut history = MidHistory::default();
        quote(&mut x, buyer, seller, 290, 310);
        history.record(x.engine(S), 1_000);
        quote(&mut x, buyer, seller, 290, 900);
        assert_eq!(history.record(x.engine(S), 1_200), Some(300));
        assert_eq!(history.record(x.engine(S), 1_200 + WIDE_SETTLE_MS - 1), Some(300));
        assert_eq!(history.record(x.engine(S), 1_200 + WIDE_SETTLE_MS), Some(595));
        assert_eq!(history.latest(), Some(595));

        // A second gap is timed from its own start, not from the first.
        quote(&mut x, buyer, seller, 290, 310);
        assert_eq!(history.record(x.engine(S), 10_000), Some(300));
        quote(&mut x, buyer, seller, 290, 900);
        assert_eq!(history.record(x.engine(S), 10_100), Some(300));
    }

    /// With nothing charted yet there is no last value to hold, so a
    /// round that opens on a wide book gets its first point at once.
    #[test]
    fn a_round_that_opens_on_a_wide_book_still_gets_its_first_point() {
        let (mut x, buyer, seller) = exchange_with_round();
        let mut history = MidHistory::default();
        quote(&mut x, buyer, seller, 100, 900);
        assert_eq!(history.record(x.engine(S), 1), Some(500));
        assert_eq!(history.points().len(), 1);
    }

    #[test]
    fn a_new_round_clears_the_buffer() {
        let (mut x, buyer, seller) = exchange_with_round();
        let mut history = MidHistory::default();
        quote(&mut x, buyer, seller, 400, 500);
        history.record(x.engine(S), 1);
        let first = history.market_id();
        assert!(first.is_some());

        x.resolve_and_open_next(S, 100_100.0, 100_100.0, S.duration_secs(), S.duration_secs());
        // Settlement cancelled the resting quotes: the new round starts empty.
        assert_eq!(history.record(x.engine(S), 2), None);
        assert_ne!(history.market_id(), first);
        assert!(history.points().is_empty());
        assert_eq!(history.latest(), None);
    }

    /// The join frame and the REST route both serve what `broadcast_book`
    /// recorded, and a plain broadcast frame carries the mid alone.
    #[tokio::test]
    async fn join_frame_and_route_carry_the_recorded_history() {
        let (x, buyer, seller) = exchange_with_round();
        let (runtime, _rx) = crate::automation::runtime::runtime_channel();
        let state = AppState {
            exchange: std::sync::Arc::new(std::sync::Mutex::new(x)),
            channels: crate::api::SeriesChannels::for_all_series(),
            price: crate::pricing::feed::PriceFeed::new(),
            runtime,
            registry: crate::registry::for_tests(),
            bus: crate::automation::signal_bus::SignalBus::new(crate::registry::for_tests(), None),
            templates: std::sync::Arc::new(crate::templates::TemplateCatalogue::default()),
            deploys: crate::api::DeployKeys::default(),
            gemini: None,
        };
        let mut rx = state.channels_for(S).book_tx.subscribe();
        {
            let mut x = state.exchange.lock().unwrap();
            quote(&mut x, buyer, seller, 400, 500);
            crate::api::broadcast_book(x.engine(S), state.channels_for(S));
            quote(&mut x, buyer, seller, 400, 600);
            crate::api::broadcast_book(x.engine(S), state.channels_for(S));
        }
        let frame = rx.try_recv().expect("first broadcast");
        assert_eq!(frame.market_id, Some(1));
        assert_eq!(frame.mid, Some(450));
        assert!(frame.mid_history.is_none(), "broadcast frames carry the mid alone");
        assert!(!serde_json::to_string(&frame).unwrap().contains("mid_history"));

        let join = build_book_join_message(&state, S);
        assert_eq!(join.mid, Some(500));
        let history = join.mid_history.expect("join frame carries the history");
        assert_eq!(history.iter().map(|p| p.mid).collect::<Vec<_>>(), vec![450, 500]);

        let Json(out) = get_mid_history(State(state.clone()), Query(SeriesQuery { series: Some(S) })).await;
        assert_eq!(out.series, S);
        assert_eq!(out.market_id, Some(1));
        assert_eq!(out.points, history);
    }

    #[test]
    fn the_cap_drops_the_oldest_point() {
        let (mut x, buyer, seller) = exchange_with_round();
        let mut history = MidHistory::default();
        // Alternate two bids under one ask so every record lands a point.
        rest(&mut x, seller, Side::Ask, 500);
        for i in 0..(MID_HISTORY_CAP as u64 + 5) {
            let bid = if i % 2 == 0 { 400 } else { 300 };
            let (engine, ledger) = x.engine_and_ledger(S);
            engine.cancel_resting_for(ledger, buyer);
            rest(&mut x, buyer, Side::Bid, bid);
            history.record(x.engine(S), i);
        }
        let points = history.points();
        assert_eq!(points.len(), MID_HISTORY_CAP);
        assert_eq!(points[0].ts_ms, 5);
    }
}
