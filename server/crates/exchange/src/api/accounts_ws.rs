//! `/ws/accounts?address=<user>` — per-user stream of every account the
//! user controls: the main account plus one row per live strategy
//! sub-account, totals across all of them, every resting order those
//! accounts have on any book, and their merged order log.
//!
//! Mirrors `/ws/positions`: one snapshot on connect, then a fresh
//! snapshot whenever the trades or book broadcasts fire *and* the rows
//! actually changed, at most once per [`ACCOUNTS_WS_MIN_INTERVAL_MS`]
//! per connection (the book broadcasts ~10×/s per series; wake-ups in
//! between are coalesced into one rebuild against the newest state).
//! Strategy create/patch/delete/fund go through HTTP and are picked up
//! here on the next broadcast (the market maker requotes every 500 ms,
//! so the lag is bounded even on a quiet book).

use std::time::{SystemTime, UNIX_EPOCH};

use axum::{
    extract::{
        Query, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    response::Response,
};
use serde::{Deserialize, Serialize};
use tokio::time::{Duration, Instant, sleep_until};

use crate::engine::Exchange;
use crate::types::{Address, SeriesId, Side};

use super::accounts::{AccountKind, AccountRow, build_account_row};
use super::user_trades::{TradeScope, UserTradeRow, build_user_trades};
use super::{AppState, subscribe_all_series};
use super::strategy::build_strategy_list;

/// Minimum spacing between two rebuilds (and so two frames) on one
/// `/ws/accounts` connection. The tabs behind it are read by a person,
/// not a strategy, so twice a second is plenty and keeps the rows from
/// flickering under the market maker's requotes.
pub const ACCOUNTS_WS_MIN_INTERVAL_MS: u64 = 500;

/// One resting order owned by the user or one of their strategies, as it
/// sits on a series' book right now. Cancel with `account` as the
/// `address` query, not the parent's: the engine checks ownership.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OpenOrderRow {
    pub series: SeriesId,
    pub order_id: u64,
    pub market_id: u32,
    /// Owning account: the parent for click trades, the sub-account for
    /// strategy orders.
    pub account: Address,
    pub account_kind: AccountKind,
    /// The owning row's display name (see [`AccountRow::name`]).
    pub account_name: String,
    /// `bid` buys YES, `ask` sells YES / buys NO.
    pub side: Side,
    /// Limit price in tenths-of-cent on the YES book.
    pub price: u16,
    /// Unfilled size still on the book.
    pub qty_open: u32,
    /// Engine acceptance sequence; newest first on read.
    pub placed_ts: u64,
}

/// Every resting order across every series for `main` and each strategy
/// row, main first, newest first within an account.
pub fn build_open_orders(engine: &Exchange, main: &AccountRow, strategies: &[AccountRow]) -> Vec<OpenOrderRow> {
    let mut out = Vec::new();
    for row in std::iter::once(main).chain(strategies) {
        let mut mine: Vec<OpenOrderRow> = engine
            .engines()
            .flat_map(|market| {
                let Some(market_id) = market.current_market().map(|m| m.id.0) else {
                    return Vec::new();
                };
                let book = market.book();
                book.iter_bid_orders()
                    .chain(book.iter_ask_orders())
                    .filter(|o| o.account == row.id)
                    .map(|o| OpenOrderRow {
                        series: market.series(),
                        order_id: o.id.0,
                        market_id,
                        account: row.id,
                        account_kind: row.kind,
                        account_name: row.name.clone(),
                        side: o.side,
                        price: o.price.tenths(),
                        qty_open: o.qty_open.0,
                        placed_ts: o.ts,
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        mine.sort_by_key(|o| std::cmp::Reverse(o.placed_ts));
        out.extend(mine);
    }
    out
}

/// Sums across the main row and every strategy row. Money in tenths.
/// `net_contributed` telescopes to the user's single real deposit (the
/// starting balance), so `lifetime_pnl = equity − starting balance`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct AccountTotals {
    pub equity_tenths: i64,
    pub net_contributed_tenths: i64,
    pub lifetime_pnl_tenths: i64,
    pub market_pnl_tenths: i64,
    pub trades_this_market: u32,
    pub last_market_pnl_tenths: i64,
    /// Σ unrealized across every row and every series.
    pub unrealized_total_tenths: i64,
    /// Σ this-round PnL across every row and every series.
    pub market_pnl_total_tenths: i64,
}

/// What actually changes between frames — everything but the timestamp.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AccountsBody {
    pub main: AccountRow,
    pub strategies: Vec<AccountRow>,
    pub totals: AccountTotals,
    /// Resting orders across every account and series; see
    /// [`build_open_orders`]. Empties at settlement, which cancels them.
    pub open_orders: Vec<OpenOrderRow>,
    /// The user's and every strategy's order log, newest first, the
    /// same rows `GET /api/accounts/{address}/trades` serves. Survives
    /// settlement: resolved rows stay, flagged `settled`.
    pub trades: Vec<UserTradeRow>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AccountsSnapshotMessage {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub as_of_unix_ms: u64,
    pub address: Address,
    #[serde(flatten)]
    pub body: AccountsBody,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AccountsQuery {
    pub address: Address,
}

fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn totals(main: &AccountRow, strategies: &[AccountRow]) -> AccountTotals {
    let mut t = AccountTotals {
        equity_tenths: 0,
        net_contributed_tenths: 0,
        lifetime_pnl_tenths: 0,
        market_pnl_tenths: 0,
        trades_this_market: 0,
        last_market_pnl_tenths: 0,
        unrealized_total_tenths: 0,
        market_pnl_total_tenths: 0,
    };
    for row in std::iter::once(main).chain(strategies) {
        t.unrealized_total_tenths += row.unrealized_total_tenths;
        t.market_pnl_total_tenths += row.market_pnl_total_tenths;
        t.equity_tenths += row.equity_tenths;
        t.net_contributed_tenths += row.net_contributed_tenths;
        t.lifetime_pnl_tenths += row.lifetime_pnl_tenths;
        t.market_pnl_tenths += row.market_pnl_tenths;
        t.trades_this_market += row.trades_this_market;
        t.last_market_pnl_tenths += row.last_market.map(|m| m.pnl_tenths).unwrap_or(0);
    }
    t
}

/// `None` when `address` is not a top-level account (unknown, or a
/// strategy sub-account — clients subscribe with the parent).
pub fn build_accounts_body(engine: &Exchange, address: Address) -> Option<AccountsBody> {
    if engine.account(&address)?.is_sub_account() {
        return None;
    }
    let main = build_account_row(engine, address)?;
    let strategies = build_strategy_list(engine, address);
    let totals = totals(&main, &strategies);
    let open_orders = build_open_orders(engine, &main, &strategies);
    let trades = build_user_trades(engine, address, TradeScope::All);
    Some(AccountsBody { main, strategies, totals, open_orders, trades })
}

pub fn build_accounts_snapshot(
    engine: &Exchange,
    address: Address,
) -> Option<AccountsSnapshotMessage> {
    Some(AccountsSnapshotMessage {
        kind: "accounts_snapshot",
        as_of_unix_ms: unix_now_ms(),
        address,
        body: build_accounts_body(engine, address)?,
    })
}

pub async fn accounts_ws(
    State(state): State<AppState>,
    Query(q): Query<AccountsQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    ws.on_upgrade(move |socket| handle_accounts_ws(state, q.address, socket))
}

async fn handle_accounts_ws(state: AppState, address: Address, mut socket: WebSocket) {
    let initial = {
        let exchange = state.exchange.lock().expect("exchange mutex poisoned");
        build_accounts_snapshot(&exchange, address)
    };
    let Some(initial) = initial else {
        let err = serde_json::json!({
            "type": "error",
            "error": "unknown_account",
            "message": "no top-level account at that address",
        });
        let _ = socket.send(Message::Text(err.to_string().into())).await;
        return;
    };

    // Dedup on the body only; the timestamp changes every rebuild.
    let mut last = initial.body.clone();
    if !send(&mut socket, &initial).await {
        return;
    }

    let mut wakeups = subscribe_all_series(&state.channels);
    let interval = Duration::from_millis(ACCOUNTS_WS_MIN_INTERVAL_MS);
    let mut last_built = Instant::now();
    // Wake-ups arrived since `last_built`; they are coalesced into one rebuild.
    let mut dirty = false;

    loop {
        tokio::select! {
            wake = wakeups.recv() => match wake {
                Some(()) => dirty = true,
                None => break,
            },
            // Fires at once when the interval has already elapsed, so the first change
            // after a quiet spell goes out with no added delay.
            _ = sleep_until(last_built + interval), if dirty => {
                dirty = false;
                last_built = Instant::now();
                if !push_if_changed(&state, address, &mut last, &mut socket).await {
                    break;
                }
            }
            client_msg = socket.recv() => match client_msg {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                _ => {}
            },
        }
    }
}

async fn push_if_changed(
    state: &AppState,
    address: Address,
    last: &mut AccountsBody,
    socket: &mut WebSocket,
) -> bool {
    let snapshot = {
        let exchange = state.exchange.lock().expect("exchange mutex poisoned");
        build_accounts_snapshot(&exchange, address)
    };
    let Some(snapshot) = snapshot else {
        return false;
    };
    if snapshot.body == *last {
        return true;
    }
    *last = snapshot.body.clone();
    send(socket, &snapshot).await
}

async fn send(socket: &mut WebSocket, msg: &AccountsSnapshotMessage) -> bool {
    let json = match serde_json::to_string(msg) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "failed to serialize accounts snapshot");
            return true;
        }
    };
    socket.send(Message::Text(json.into())).await.is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::accounts::AccountKind;
    use crate::engine::{Engine, TradeSource};
    use crate::engine::sub_accounts::test_support::named;
    use crate::ticks::PriceTick;
    use crate::types::{Qty, Side, Tenths};

    #[test]
    fn snapshot_lists_main_then_live_strategies_only() {
        let mut e = Engine::new(Tenths(1_000_000));
        let parent = Address([1; 20]);
        e.register_user(parent);
        e.get_or_create_account(parent);
        let a = e.create_sub_account(parent, Tenths(50_000)).unwrap();
        let b = e.create_sub_account(parent, Tenths(50_000)).unwrap();
        e.deploy_strategy(a, named("A", e.series())).unwrap();
        // `b` has no strategy (orphan-like) → hidden, but its cash still
        // counts as parked, so the main row's contributed shrinks by it.
        let _ = b;

        let snap = build_accounts_snapshot(e.exchange(), parent).unwrap();
        assert_eq!(snap.kind, "accounts_snapshot");
        assert_eq!(snap.address, parent);
        assert_eq!(snap.body.main.kind, AccountKind::Main);
        assert_eq!(snap.body.strategies.len(), 1);
        assert_eq!(snap.body.strategies[0].id, a);
        assert_eq!(snap.body.strategies[0].name, "A");

        assert!(build_accounts_snapshot(e.exchange(), Address([9; 20])).is_none());
        assert!(build_accounts_snapshot(e.exchange(), a).is_none(), "children are not roots");
    }

    #[test]
    fn totals_sum_rows_and_lifetime_is_equity_minus_seed() {
        let (parent, mm) = (Address([1; 20]), Address([2; 20]));
        let mut e = Engine::new(Tenths(1_000_000));
        e.register_user(parent);
        e.get_or_create_account(parent);
        e.get_or_create_account(mm);
        e.open_first_market(50_000.0, 0, 60);
        let child = e.create_sub_account(parent, Tenths(200_000)).unwrap();
        e.deploy_strategy(child, named("Child", e.series())).unwrap();

        // Child buys 10 @ 500; a two-sided 550/650 book marks it at 600.
        let tick = |t: u16| PriceTick::from_tenths(t).unwrap();
        e.place_limit(mm, Side::Ask, tick(500), Qty(10), TradeSource::Bot).unwrap();
        e.place_market(child, Side::Bid, Qty(10), TradeSource::Strategy).unwrap();
        e.place_limit(mm, Side::Bid, tick(550), Qty(1), TradeSource::Bot).unwrap();
        e.place_limit(mm, Side::Ask, tick(650), Qty(1), TradeSource::Bot).unwrap();
        // Parent sells 1 @ 550 (short, taker) — one trade on the main row.
        e.place_market(parent, Side::Ask, Qty(1), TradeSource::Ui).unwrap();

        let body = build_accounts_body(e.exchange(), parent).unwrap();
        let t = body.totals;
        let rows: Vec<&AccountRow> = std::iter::once(&body.main).chain(&body.strategies).collect();
        assert_eq!(t.equity_tenths, rows.iter().map(|r| r.equity_tenths).sum::<i64>());
        assert_eq!(t.market_pnl_tenths, rows.iter().map(|r| r.market_pnl_tenths).sum::<i64>());
        assert_eq!(t.trades_this_market, 2);
        assert_eq!(t.net_contributed_tenths, 1_000_000, "telescopes to the seed");
        assert_eq!(t.lifetime_pnl_tenths, t.equity_tenths - 1_000_000);
        assert_eq!(t.last_market_pnl_tenths, 0);
    }

    #[test]
    fn open_orders_and_trades_span_main_and_strategies_and_survive_settlement() {
        use crate::engine::UserOrderStatus;
        let (parent, mm) = (Address([1; 20]), Address([2; 20]));
        let mut e = Engine::new(Tenths(1_000_000));
        e.register_user(parent);
        e.get_or_create_account(parent).username = Some("jet".into());
        e.get_or_create_account(mm);
        e.open_first_market(50_000.0, 0, 60);
        let child = e.create_sub_account(parent, Tenths(200_000)).unwrap();
        e.deploy_strategy(child, named("Child", e.series())).unwrap();
        let tick = |t: u16| PriceTick::from_tenths(t).unwrap();

        // Child rests a bid, then the parent rests one above it; neither
        // fills. The parent also lifts a maker offer: one click-trade fill.
        e.place_limit(child, Side::Bid, tick(300), Qty(5), TradeSource::Strategy).unwrap();
        e.place_limit(parent, Side::Bid, tick(310), Qty(2), TradeSource::Ui).unwrap();
        e.place_limit(mm, Side::Ask, tick(500), Qty(10), TradeSource::Bot).unwrap();
        e.place_market(child, Side::Bid, Qty(3), TradeSource::Strategy).unwrap();
        e.place_market(parent, Side::Bid, Qty(1), TradeSource::Ui).unwrap();

        let body = build_accounts_body(e.exchange(), parent).unwrap();
        let orders = &body.open_orders;
        assert_eq!(orders.len(), 2, "one resting bid each; the maker's offer is not ours");
        assert_eq!(orders[0].account, parent, "main account rows come first");
        assert_eq!(orders[0].account_kind, AccountKind::Main);
        assert_eq!(orders[0].account_name, "jet");
        assert_eq!((orders[0].side, orders[0].price, orders[0].qty_open), (Side::Bid, 310, 2));
        assert_eq!(orders[1].account, child);
        assert_eq!(orders[1].account_kind, AccountKind::Strategy);
        assert_eq!(orders[1].account_name, "Child");
        assert_eq!((orders[1].side, orders[1].price, orders[1].qty_open), (Side::Bid, 300, 5));

        let trades = &body.trades;
        assert_eq!(trades.len(), 4, "every order the user or a strategy placed is a row");
        assert!(trades.windows(2).all(|w| w[0].ts >= w[1].ts), "newest first");
        let child_fill = trades
            .iter()
            .find(|t| t.strategy_id == Some(child) && t.status == UserOrderStatus::Filled)
            .expect("the strategy's market buy is a filled row");
        assert_eq!(child_fill.strategy_name.as_deref(), Some("Child"));
        assert_eq!((child_fill.filled_qty, child_fill.avg_price), (3, Some(500)));
        let main_fill = trades
            .iter()
            .find(|t| t.strategy_id.is_none() && t.status == UserOrderStatus::Filled)
            .expect("the click trade is a filled row without a strategy");
        assert_eq!((main_fill.filled_qty, main_fill.avg_price), (1, Some(500)));
        assert!(trades.iter().all(|t| !t.settled));

        // Settlement cancels every resting order and archives the log.
        e.resolve_and_open_next(51_000.0, 51_000.0, 60, 60);
        let after = build_accounts_body(e.exchange(), parent).unwrap();
        assert!(after.open_orders.is_empty(), "the flip clears the book");
        assert_eq!(after.trades.len(), 4, "history survives the flip");
        assert!(after.trades.iter().all(|t| t.settled));
        assert!(
            after.trades.iter().any(|t| t.strategy_id == Some(child)),
            "strategy rows survive too"
        );
    }
}
