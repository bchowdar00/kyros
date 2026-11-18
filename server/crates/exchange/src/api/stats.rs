//! `GET /api/market/stats` — aggregate stats for the currently-active market.
//!
//! All counters are tracked per-market on [`Engine`] and reset alongside
//! `open_interest` at resolution. Money is reported in tenths-of-cent
//! (`$1 = 1000`) to match the rest of the wire surface.
//!
//! Definitions (justified inline so they are visible at the call site):
//!
//! * `total_volume_tenths` — notional cash committed by both sides combined.
//!   For every fill of `qty` contracts the buyer puts up `qty × price` of
//!   YES-side collateral and the seller `qty × (1$ − price)` of NO-side
//!   collateral, summing to `qty × $1`. Aggregated as `Σ qty × $1` so the
//!   number is symmetric and matches how the engine itself sizes open
//!   interest ("total locked = OI × $1").
//! * `open_interest_contracts` — outstanding YES contracts (equivalently
//!   outstanding NO contracts). Sourced directly from the engine.
//! * `total_locked_tenths` — `open_interest_contracts × $1`, the maximum
//!   payout the engine is on the hook for, in tenths.
//! * `total_trades` — number of `Fill` records produced in the current
//!   market.
//! * `unique_traders` — distinct addresses appearing as taker OR maker in
//!   the current market.
//! * `largest_trade` — the single fill with the largest `qty`. Ties favor
//!   the more recent fill. Chosen by qty (not by cash) because every
//!   contract has identical $1 face value, so qty is the truest measure of
//!   "size"; `qty × price` would bias toward extreme-priced trades.

use axum::{Json, extract::{Query, State}};
use serde::Serialize;

use crate::engine::{MarketEngine, PAYOUT_PER_CONTRACT};
use crate::engine::model::Fill;
use crate::types::{DEFAULT_SERIES, SeriesId};

use super::{AppState, SeriesQuery};

#[derive(Debug, Clone, Serialize)]
pub struct MarketStats {
    pub series: SeriesId,
    pub duration_secs: u64,
    /// `None` before the first market is opened.
    pub market_id: Option<u32>,
    pub opened_at_ms: Option<u64>,
    pub closes_at_ms: Option<u64>,
    pub total_volume_tenths: i64,
    pub open_interest_contracts: u64,
    pub total_locked_tenths: i64,
    pub total_trades: u64,
    pub unique_traders: u64,
    pub largest_trade: Option<Fill>,
}

impl MarketStats {
    pub fn from_engine(engine: &MarketEngine) -> Self {
        let market = engine.current_market();
        let oi = engine.open_interest();
        Self {
            series: engine.series(),
            duration_secs: engine.series().duration_secs(),
            market_id: market.map(|m| m.id.0),
            // Engine stores `opened_at`/`closes_at` in unix seconds; the wire
            // contract is named `_ms`, so we upconvert here. Mirrors the
            // conversion in `/ws/price` (api/ws.rs).
            opened_at_ms: market.map(|m| m.opened_at * 1000),
            closes_at_ms: market.map(|m| m.closes_at * 1000),
            total_volume_tenths: engine.market_volume_tenths(),
            open_interest_contracts: oi,
            total_locked_tenths: oi as i64 * PAYOUT_PER_CONTRACT,
            total_trades: engine.market_trade_count(),
            unique_traders: engine.market_unique_traders(),
            largest_trade: engine.market_largest_trade().copied(),
        }
    }
}

pub async fn get_market_stats(
    State(state): State<AppState>,
    Query(q): Query<SeriesQuery>,
) -> Json<MarketStats> {
    let exchange = state.exchange.lock().expect("exchange mutex poisoned");
    Json(MarketStats::from_engine(
        exchange.engine(q.series.unwrap_or(DEFAULT_SERIES)),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Engine, TradeSource};
    use crate::ticks::PriceTick;
    use crate::types::{Address, Qty, Side, Tenths};

    const START: Tenths = Tenths(1_000_000);

    fn addr(s: &str) -> Address {
        s.parse().unwrap()
    }

    fn tick(t: u16) -> PriceTick {
        PriceTick::from_tenths(t).unwrap()
    }

    #[test]
    fn stats_empty_market_serializes_zeros() {
        let engine = Engine::new(START);
        let stats = MarketStats::from_engine(engine.market());

        assert!(stats.market_id.is_none());
        assert!(stats.opened_at_ms.is_none());
        assert!(stats.closes_at_ms.is_none());
        assert_eq!(stats.total_volume_tenths, 0);
        assert_eq!(stats.open_interest_contracts, 0);
        assert_eq!(stats.total_locked_tenths, 0);
        assert_eq!(stats.total_trades, 0);
        assert_eq!(stats.unique_traders, 0);
        assert!(stats.largest_trade.is_none());

        // Valid JSON.
        let json = serde_json::to_value(&stats).unwrap();
        assert_eq!(json["market_id"], serde_json::Value::Null);
        assert_eq!(json["total_trades"], 0);
        assert_eq!(json["largest_trade"], serde_json::Value::Null);
    }

    #[test]
    fn stats_reflect_current_market_after_trades() {
        let mut engine = Engine::new(START);
        let a = addr("0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let b = addr("0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        engine.get_or_create_account(a);
        engine.get_or_create_account(b);
        engine.open_first_market(50_000.0, 1_000, 60);

        engine.place_limit(b, Side::Ask, tick(500), Qty(12), TradeSource::Bot).unwrap();
        engine.place_market(a, Side::Bid, Qty(4), TradeSource::Bot).unwrap();
        engine.place_market(a, Side::Bid, Qty(8), TradeSource::Bot).unwrap();

        let stats = MarketStats::from_engine(engine.market());
        assert_eq!(stats.market_id, Some(1));
        assert_eq!(stats.opened_at_ms, Some(1_000_000));
        assert_eq!(stats.closes_at_ms, Some(1_060_000));
        assert_eq!(stats.total_trades, 2);
        assert_eq!(stats.unique_traders, 2);
        assert_eq!(stats.total_volume_tenths, 12 * PAYOUT_PER_CONTRACT);
        assert_eq!(stats.open_interest_contracts, 12);
        assert_eq!(stats.total_locked_tenths, 12 * PAYOUT_PER_CONTRACT);
        let largest = stats.largest_trade.expect("trades occurred");
        assert_eq!(largest.qty.0, 8);
    }

    #[test]
    fn stats_largest_trade_serializes_with_address_and_side() {
        let mut engine = Engine::new(START);
        let a = addr("0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let b = addr("0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        engine.get_or_create_account(a);
        engine.get_or_create_account(b);
        engine.open_first_market(50_000.0, 0, 60);
        engine.place_limit(b, Side::Ask, tick(500), Qty(5), TradeSource::Bot).unwrap();
        engine.place_market(a, Side::Bid, Qty(5), TradeSource::Bot).unwrap();

        let stats = MarketStats::from_engine(engine.market());
        let json = serde_json::to_value(&stats).unwrap();
        let trade = &json["largest_trade"];
        assert_eq!(trade["qty"], 5);
        assert_eq!(trade["price"], 500);
        assert_eq!(trade["taker_side"], "bid");
        assert_eq!(trade["kind"], "open");
        assert!(trade["taker"].is_string());
        assert!(trade["maker"].is_string());
    }
}
