//! Per-user trade history: the active market plus the last few
//! resolved ones.
//!
//! `GET /api/accounts/{address}/trades[?strategy=main|<id>]` returns one
//! row per order the user (and every strategy sub-account) placed. Rows
//! accumulate fills as they land — a resting limit hit by multiple
//! takers is still one row, showing as `partially_filled` with a running
//! `filled_qty` and VWAP until it's fully filled or cancelled. Rows from
//! resolved markets come from the engine's settled archive and are
//! flagged `settled`. The engine stores rows directly; this handler does
//! no aggregation, only projection + filter + sort + serialise.
//!
//! `strategy=main` keeps only the parent's own orders; `strategy=<id>`
//! keeps only that sub-account's orders (404 `strategy_not_found` when
//! the id is not one of the caller's live strategies).
//!
//! Unregistered addresses (bots, never-seen-before keys) return an
//! empty list rather than 404 so external callers can ask uniformly
//! without leaking the user-vs-bot distinction. Truly nonexistent
//! accounts (the engine has no record at all) still return 404.
//!
//! Each row carries `at_ms`, the wall-clock time the order was placed
//! in unix milliseconds, for placing the order on a time axis. `ts`
//! remains the engine's sequence number used only for newest-first
//! ordering.

use axum::{
    Json,
    extract::{Path, Query, State},
};
use serde::{Deserialize, Serialize};

use crate::engine::{Exchange, OrderType, TradeSource, UserOrderRow, UserOrderStatus};
use crate::types::{Address, SeriesId, Side};

use super::{ApiError, AppState, TradeSide};

/// One row in the user's trade log. Describes the user's order from
/// the user's own perspective: what they asked for, what came back,
/// and where it came from. `avg_price` is the volume-weighted average
/// across all fills landed against this order, in tenths-of-cent;
/// `None` while `filled_qty` is zero.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UserTradeRow {
    pub series: SeriesId,
    pub order_id: u64,
    pub ts: u64,
    /// Wall-clock time the order was placed, in unix milliseconds. Fills
    /// that land later do not change it. Use this to place the order on a
    /// time axis; `ts` is a sequence number, not a clock time.
    pub at_ms: u64,
    /// User's order direction. `Yes` = bought YES (Bid). `No` = sold
    /// YES / bought NO (Ask).
    pub side: TradeSide,
    pub qty_requested: u32,
    pub filled_qty: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avg_price: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit_price: Option<u16>,
    pub source: TradeSource,
    pub order_type: OrderType,
    pub status: UserOrderStatus,
    pub market_id: u32,
    /// Set when the order was placed by one of the user's strategy
    /// sub-accounts: the sub-account address (= strategy id) and the
    /// strategy's display name. Absent for manual (parent) orders.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strategy_id: Option<Address>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strategy_name: Option<String>,
    /// Realized PnL this order produced by closing exposure (partial
    /// close, close to flat, or the close leg of a flip), in tenths.
    /// Zero for pure opens. Settlement PnL on a position held to
    /// resolution is *not* attributed to any order — read it from the
    /// account's `last_market` instead.
    pub realized_pnl_tenths: i64,
    /// `true` when the row's market has resolved.
    pub settled: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct UserTradesResponse {
    pub trades: Vec<UserTradeRow>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct UserTradesQuery {
    /// `main`, or a strategy id (sub-account address). Absent = all.
    #[serde(default)]
    pub strategy: Option<String>,
}

/// Which of the caller's accounts to include.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TradeScope {
    All,
    MainOnly,
    Strategy(Address),
}

/// `GET /api/accounts/{address}/trades`. See module docs.
pub async fn get_user_trades(
    State(state): State<AppState>,
    Path(address): Path<String>,
    Query(q): Query<UserTradesQuery>,
) -> Result<Json<UserTradesResponse>, ApiError> {
    let address: Address = address.parse()?;
    let engine = state.exchange.lock().expect("exchange mutex poisoned");
    // True 404 only for accounts the engine has no record of at all.
    if engine.account(&address).is_none() {
        return Err(ApiError::UnknownAccount);
    }
    // Bot or otherwise unregistered: no recordable activity by design.
    if !engine.is_user_account(&address) {
        return Ok(Json(UserTradesResponse { trades: Vec::new() }));
    }
    let scope = match q.strategy.as_deref() {
        None => TradeScope::All,
        Some("main") => TradeScope::MainOnly,
        Some(id) => {
            let id: Address = id.parse()?;
            let is_live_child = engine
                .account(&id)
                .is_some_and(|a| a.parent == Some(address) && a.strategy.is_some());
            if !is_live_child {
                return Err(ApiError::StrategyNotFound);
            }
            TradeScope::Strategy(id)
        }
    };
    let trades = build_user_trades(&engine, address, scope);
    Ok(Json(UserTradesResponse { trades }))
}

/// The user's own orders plus every strategy sub-account's orders,
/// active market and settled archive alike, newest first. Child rows
/// carry `strategy_id` / `strategy_name`.
pub fn build_user_trades(
    engine: &Exchange,
    address: Address,
    scope: TradeScope,
) -> Vec<UserTradeRow> {
    let mut trades: Vec<UserTradeRow> = Vec::new();
    if matches!(scope, TradeScope::All | TradeScope::MainOnly) {
        trades.extend(rows_for(engine, address, None));
    }
    for child in engine.sub_accounts_of(&address) {
        let included = match scope {
            TradeScope::All => true,
            TradeScope::MainOnly => false,
            TradeScope::Strategy(id) => id == *child,
        };
        if !included {
            continue;
        }
        let name = engine
            .account(child)
            .and_then(|a| a.strategy.as_ref())
            .map(|s| s.name.clone());
        trades.extend(rows_for(engine, *child, Some((*child, name))));
    }
    // `placed_ts` is the engine's global sequence, monotonic across
    // markets, so this also puts settled rows after live ones.
    trades.sort_by(|a, b| b.ts.cmp(&a.ts));
    trades
}

fn rows_for(
    exchange: &Exchange,
    account: Address,
    strategy: Option<(Address, Option<String>)>,
) -> Vec<UserTradeRow> {
    exchange
        .engines()
        .flat_map(|engine| {
            engine
                .user_orders(&account)
                .iter()
                .map(|r| project_row(engine.series(), r, strategy.clone(), false))
                .chain(
                    engine
                        .settled_user_orders(&account)
                        .iter()
                        .map(|r| project_row(engine.series(), r, strategy.clone(), true)),
                )
                .collect::<Vec<_>>()
        })
        .collect()
}

fn project_row(
    series: SeriesId,
    row: &UserOrderRow,
    strategy: Option<(Address, Option<String>)>,
    settled: bool,
) -> UserTradeRow {
    let (strategy_id, strategy_name) = match strategy {
        Some((id, name)) => (Some(id), name),
        None => (None, None),
    };
    let avg_price = if row.filled_qty.0 > 0 {
        Some((row.notional_tenths / row.filled_qty.0 as u64) as u16)
    } else {
        None
    };
    UserTradeRow {
        series,
        order_id: row.order_id.0,
        ts: row.placed_ts,
        at_ms: row.placed_at_ms,
        side: match row.side {
            Side::Bid => TradeSide::Yes,
            Side::Ask => TradeSide::No,
        },
        qty_requested: row.qty_requested.0,
        filled_qty: row.filled_qty.0,
        avg_price,
        limit_price: row.limit_price,
        source: row.source,
        order_type: row.order_type,
        status: row.status,
        market_id: row.market_id.0,
        strategy_id,
        strategy_name,
        realized_pnl_tenths: row.realized_pnl_tenths,
        settled,
    }
}

// ───────────────────────────── tests ────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Engine, TradeSource};
    use crate::ticks::PriceTick;
    use crate::types::{Address, Qty, Tenths};

    const START: Tenths = Tenths(1_000_000);

    fn addr(s: &str) -> Address {
        s.parse().unwrap()
    }
    fn tick(t: u16) -> PriceTick {
        PriceTick::from_tenths(t).unwrap()
    }

    fn engine_with_user_and_bot() -> (Engine, Address, Address) {
        let mut e = Engine::new(START);
        let user = addr("0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let bot = addr("0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        e.seed_bot_account(bot, Tenths(50_000_000), "bot".into(), None, None);
        e.register_user(user);
        e.get_or_create_account(user);
        e.open_first_market(50_000.0, 0, 60);
        (e, user, bot)
    }

    #[test]
    fn user_trades_merge_two_series_by_seq() {
        use crate::engine::Exchange;
        use crate::series::SERIES;
        use crate::types::SeriesId;
        let user = addr("0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let mut x = Exchange::new(START, SERIES);
        x.register_user(user);
        x.get_or_create_account(user);
        x.open_first_market(SeriesId(0), 50_000.0, 0, 60);
        x.open_first_market(SeriesId(1), 50_000.0, 0, 900);
        x.place_limit(SeriesId(0), user, Side::Bid, tick(300), Qty(1), TradeSource::Ui).unwrap();
        x.place_limit(SeriesId(1), user, Side::Bid, tick(300), Qty(2), TradeSource::Ui).unwrap();
        x.place_limit(SeriesId(0), user, Side::Bid, tick(300), Qty(3), TradeSource::Ui).unwrap();

        let rows = build_user_trades(&x, user, TradeScope::All);
        assert_eq!(rows.len(), 3);
        // Newest first across both series, by the global sequence.
        assert_eq!(rows.iter().map(|r| r.qty_requested).collect::<Vec<_>>(), vec![3, 2, 1]);
        assert_eq!(rows[0].series, SeriesId(0));
        assert_eq!(rows[1].series, SeriesId(1));
        assert!(rows[0].ts > rows[1].ts && rows[1].ts > rows[2].ts);
        assert_eq!(serde_json::to_value(&rows[1]).unwrap()["series"], "btc-15m");
    }

    #[test]
    fn sweep_across_makers_yields_one_row() {
        let (mut e, user, bot) = engine_with_user_and_bot();
        // Two ask levels, one taker market sweep across both.
        e.place_limit(bot, Side::Ask, tick(500), Qty(3), TradeSource::Bot)
            .unwrap();
        e.place_limit(bot, Side::Ask, tick(550), Qty(2), TradeSource::Bot)
            .unwrap();
        e.place_market(user, Side::Bid, Qty(5), TradeSource::Ui)
            .unwrap();

        let rows: Vec<_> = e.user_orders(&user).iter().map(|r| project_row(crate::types::DEFAULT_SERIES, r, None, false)).collect();
        assert_eq!(rows.len(), 1, "one place call = one row");
        let row = &rows[0];
        assert_eq!(row.qty_requested, 5);
        assert_eq!(row.filled_qty, 5);
        // VWAP: (3*500 + 2*550) / 5 = 520
        assert_eq!(row.avg_price, Some(520));
        assert!(matches!(row.side, TradeSide::Yes));
        assert_eq!(row.source, TradeSource::Ui);
        assert_eq!(row.order_type, OrderType::Market);
        assert_eq!(row.status, UserOrderStatus::Filled);
        assert!(row.limit_price.is_none());
    }

    #[test]
    fn resting_limit_partial_fills_track_status_transitions() {
        let (mut e, user, bot) = engine_with_user_and_bot();
        // User rests a limit ask for 10 @ 60.
        e.place_limit(user, Side::Ask, tick(600), Qty(10), TradeSource::Ui)
            .unwrap();

        let row = e.user_orders(&user)[0];
        assert_eq!(row.qty_requested, Qty(10));
        assert_eq!(row.filled_qty, Qty(0));
        assert_eq!(row.status, UserOrderStatus::Rested);
        assert!(row.limit_price.is_some());
        assert_eq!(row.limit_price, Some(600));

        // Bot takes 4 of them.
        e.place_market(bot, Side::Bid, Qty(4), TradeSource::Bot)
            .unwrap();
        let row = e.user_orders(&user)[0];
        assert_eq!(row.filled_qty, Qty(4));
        assert_eq!(row.status, UserOrderStatus::PartiallyFilled);

        // Bot takes the remaining 6.
        e.place_market(bot, Side::Bid, Qty(6), TradeSource::Bot)
            .unwrap();
        let row = e.user_orders(&user)[0];
        assert_eq!(row.filled_qty, Qty(10));
        assert_eq!(row.status, UserOrderStatus::Filled);
        // Still ONE row in the user's log, not three.
        assert_eq!(e.user_orders(&user).len(), 1);
    }

    #[test]
    fn projects_market_order_avg_price_and_status() {
        let (mut e, user, bot) = engine_with_user_and_bot();
        e.place_limit(bot, Side::Ask, tick(500), Qty(2), TradeSource::Bot)
            .unwrap();
        e.place_market(user, Side::Bid, Qty(2), TradeSource::Ui)
            .unwrap();
        let row = project_row(crate::types::DEFAULT_SERIES, &e.user_orders(&user)[0], None, false);
        assert_eq!(row.avg_price, Some(500));
        assert_eq!(row.filled_qty, 2);
        assert_eq!(row.status, UserOrderStatus::Filled);
        assert_eq!(row.order_type, OrderType::Market);
    }

    #[test]
    fn cancelled_resting_order_is_marked_cancelled() {
        let (mut e, user, _bot) = engine_with_user_and_bot();
        let outcome = e
            .place_limit(user, Side::Bid, tick(400), Qty(3), TradeSource::Ui)
            .unwrap();
        assert_eq!(
            e.user_orders(&user)[0].status,
            UserOrderStatus::Rested
        );
        e.cancel(user, outcome.order_id).unwrap();
        let row = e.user_orders(&user)[0];
        assert_eq!(row.status, UserOrderStatus::Cancelled);
        assert_eq!(row.filled_qty, Qty(0));
    }

    #[test]
    fn unregistered_account_yields_no_rows() {
        // Pure-bot activity must not surface anywhere.
        let mut e = Engine::new(START);
        let b1 = addr("0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let b2 = addr("0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        e.seed_bot_account(b1, Tenths(50_000_000), "m".into(), None, None);
        e.seed_bot_account(b2, Tenths(50_000_000), "t".into(), None, None);
        e.open_first_market(50_000.0, 0, 60);
        e.place_limit(b1, Side::Ask, tick(500), Qty(2), TradeSource::Bot)
            .unwrap();
        e.place_market(b2, Side::Bid, Qty(2), TradeSource::Bot)
            .unwrap();
        assert!(e.user_orders(&b1).is_empty());
        assert!(e.user_orders(&b2).is_empty());
    }

    #[test]
    fn user_vs_user_writes_two_rows_one_per_owner() {
        let mut e = Engine::new(START);
        let a = addr("0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let b = addr("0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        e.register_user(a);
        e.register_user(b);
        e.get_or_create_account(a);
        e.get_or_create_account(b);
        e.open_first_market(50_000.0, 0, 60);

        // A rests a limit via UI.
        e.place_limit(a, Side::Ask, tick(500), Qty(2), TradeSource::Ui)
            .unwrap();
        // B takes it via strategy.
        e.place_market(b, Side::Bid, Qty(2), TradeSource::Strategy)
            .unwrap();

        // A's row updated to Filled.
        let a_row = project_row(crate::types::DEFAULT_SERIES, &e.user_orders(&a)[0], None, false);
        assert_eq!(a_row.status, UserOrderStatus::Filled);
        assert_eq!(a_row.source, TradeSource::Ui);
        assert_eq!(a_row.order_type, OrderType::Limit);
        assert_eq!(a_row.filled_qty, 2);

        // B's row is the taker placement.
        let b_row = project_row(crate::types::DEFAULT_SERIES, &e.user_orders(&b)[0], None, false);
        assert_eq!(b_row.status, UserOrderStatus::Filled);
        assert_eq!(b_row.source, TradeSource::Strategy);
        assert_eq!(b_row.order_type, OrderType::Market);
        assert_eq!(b_row.filled_qty, 2);
    }

    #[test]
    fn newest_first_by_placed_ts() {
        let (mut e, user, bot) = engine_with_user_and_bot();
        e.place_limit(bot, Side::Ask, tick(500), Qty(1), TradeSource::Bot)
            .unwrap();
        e.place_market(user, Side::Bid, Qty(1), TradeSource::Ui)
            .unwrap();
        e.place_limit(bot, Side::Bid, tick(500), Qty(1), TradeSource::Bot)
            .unwrap();
        e.place_market(user, Side::Ask, Qty(1), TradeSource::Ui)
            .unwrap();

        let mut rows: Vec<UserTradeRow> =
            e.user_orders(&user).iter().map(|r| project_row(crate::types::DEFAULT_SERIES, r, None, false)).collect();
        rows.sort_by(|a, b| b.ts.cmp(&a.ts));
        assert_eq!(rows.len(), 2);
        assert!(rows[0].ts > rows[1].ts);
        // Latest action was Ask → side No.
        assert!(matches!(rows[0].side, TradeSide::No));
    }

    #[test]
    fn rows_cleared_after_market_resolution() {
        let (mut e, user, bot) = engine_with_user_and_bot();
        e.place_limit(bot, Side::Ask, tick(500), Qty(1), TradeSource::Bot)
            .unwrap();
        e.place_market(user, Side::Bid, Qty(1), TradeSource::Ui)
            .unwrap();
        assert!(!e.user_orders(&user).is_empty());
        e.resolve_and_open_next(60_000.0, 60_000.0, 60, 60);
        assert!(e.user_orders(&user).is_empty());

        // The endpoint still shows it, flagged settled, under the old
        // market id; a new live order sorts ahead of it.
        e.place_limit(bot, Side::Ask, tick(500), Qty(1), TradeSource::Bot).unwrap();
        e.place_market(user, Side::Bid, Qty(1), TradeSource::Ui).unwrap();
        let rows = build_user_trades(e.exchange(), user, TradeScope::All);
        assert_eq!(rows.len(), 2);
        assert!(!rows[0].settled);
        assert_eq!(rows[0].market_id, 2);
        assert!(rows[1].settled);
        assert_eq!(rows[1].market_id, 1);
    }

    #[test]
    fn closing_order_carries_its_realized_pnl() {
        let (mut e, user, bot) = engine_with_user_and_bot();
        e.place_limit(bot, Side::Ask, tick(400), Qty(5), TradeSource::Bot).unwrap();
        e.place_market(user, Side::Bid, Qty(5), TradeSource::Ui).unwrap();
        e.place_limit(bot, Side::Bid, tick(700), Qty(2), TradeSource::Bot).unwrap();
        e.place_market(user, Side::Ask, Qty(2), TradeSource::Ui).unwrap();

        let rows = build_user_trades(e.exchange(), user, TradeScope::All);
        // Newest first: the sell (close) then the buy (open).
        assert_eq!(rows[0].realized_pnl_tenths, 600);
        assert_eq!(rows[1].realized_pnl_tenths, 0);
    }

    #[test]
    fn unknown_address_branch_semantics() {
        // Mirrors the handler's 404 vs empty distinction.
        let mut e = Engine::new(START);
        let ghost = addr("0xcccccccccccccccccccccccccccccccccccccccc");
        assert!(e.account(&ghost).is_none());
        let bot = addr("0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        e.seed_bot_account(bot, Tenths(1_000_000), "m".into(), None, None);
        assert!(e.account(&bot).is_some());
        assert!(!e.is_user_account(&bot));
        let user = addr("0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        e.register_user(user);
        e.get_or_create_account(user);
        assert!(e.is_user_account(&user));
        assert!(e.user_orders(&user).is_empty());
    }

    #[test]
    fn rested_row_serializes_with_null_avg_price() {
        let (mut e, user, _bot) = engine_with_user_and_bot();
        e.place_limit(user, Side::Bid, tick(400), Qty(2), TradeSource::Ui)
            .unwrap();
        let row = project_row(crate::types::DEFAULT_SERIES, &e.user_orders(&user)[0], None, false);
        let json = serde_json::to_value(&row).unwrap();
        assert_eq!(json["status"], "rested");
        assert_eq!(json["filled_qty"], 0);
        assert!(json.get("avg_price").is_none(), "skipped when None");
        assert_eq!(json["limit_price"], 400);
        assert_eq!(json["order_type"], "limit");
        assert_eq!(json["side"], "yes");
        assert_eq!(json["market_id"], 1);
    }

    #[test]
    fn project_row_copies_placed_at_ms_to_at_ms_and_serializes() {
        let (mut e, user, bot) = engine_with_user_and_bot();
        e.place_limit(bot, Side::Ask, tick(500), Qty(2), TradeSource::Bot)
            .unwrap();
        e.place_market(user, Side::Bid, Qty(2), TradeSource::Ui)
            .unwrap();
        let order_row = e.user_orders(&user)[0];
        let row = project_row(crate::types::DEFAULT_SERIES, &order_row, None, false);
        assert_eq!(row.at_ms, order_row.placed_at_ms);
        let json = serde_json::to_value(&row).unwrap();
        // Always serialized, as a number equal to the placement time.
        assert_eq!(json["at_ms"].as_u64(), Some(order_row.placed_at_ms));
    }

    #[test]
    fn flip_position_records_two_rows_close_then_open() {
        // A flip is two market orders → two distinct entries in the
        // user's log, in placement order.
        let (mut e, user, bot) = engine_with_user_and_bot();
        // Seed enough liquidity to open and flip.
        e.place_limit(bot, Side::Ask, tick(500), Qty(10), TradeSource::Bot)
            .unwrap();
        e.place_market(user, Side::Bid, Qty(10), TradeSource::Ui)
            .unwrap();
        e.place_limit(bot, Side::Bid, tick(500), Qty(30), TradeSource::Bot)
            .unwrap();
        e.flip_position(user, TradeSource::Ui).unwrap();
        // Three rows: the open buy, the flip-close sell, the flip-open sell.
        assert_eq!(e.user_orders(&user).len(), 3);
    }

    #[test]
    fn strategy_sub_account_orders_roll_into_parent_log() {
        let (mut e, user, bot) = engine_with_user_and_bot();
        let child = e.create_sub_account(user, Tenths(100_000)).unwrap();
        e.deploy_strategy(child, crate::engine::sub_accounts::test_support::named("Momentum", e.series()))
            .unwrap();

        e.place_limit(bot, Side::Ask, tick(500), Qty(10), TradeSource::Bot).unwrap();
        e.place_market(user, Side::Bid, Qty(2), TradeSource::Ui).unwrap();
        e.place_market(child, Side::Bid, Qty(3), TradeSource::Strategy).unwrap();

        let rows = build_user_trades(e.exchange(), user, TradeScope::All);
        assert_eq!(rows.len(), 2);
        // Newest first: the child's order was placed last.
        assert_eq!(rows[0].strategy_id, Some(child));
        assert_eq!(rows[0].strategy_name.as_deref(), Some("Momentum"));
        assert_eq!(rows[0].qty_requested, 3);
        assert_eq!(rows[0].source, TradeSource::Strategy);
        assert_eq!(rows[1].strategy_id, None);
        assert_eq!(rows[1].strategy_name, None);
        assert_eq!(rows[1].qty_requested, 2);

        // The child's own log is unchanged and unlabelled from its view.
        let child_rows = build_user_trades(e.exchange(), child, TradeScope::All);
        assert_eq!(child_rows.len(), 1);
        assert_eq!(child_rows[0].strategy_id, None);

        // Scopes.
        let main_only = build_user_trades(e.exchange(), user, TradeScope::MainOnly);
        assert_eq!(main_only.len(), 1);
        assert_eq!(main_only[0].strategy_id, None);
        let one = build_user_trades(e.exchange(), user, TradeScope::Strategy(child));
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].strategy_id, Some(child));
        assert!(build_user_trades(e.exchange(), user, TradeScope::Strategy(bot)).is_empty());
    }
}
