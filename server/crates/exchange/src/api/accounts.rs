//! One row per account a user controls — their main account and each
//! strategy sub-account — with the same shape for both so the UI can
//! render a single "My Accounts" list.
//!
//! Every money field is tenths of a cent. Three PnL horizons:
//!
//! * `market_pnl_tenths` — this round so far: realized fills plus the
//!   open position marked at the current book. Equals "what I'd have if
//!   I closed now, minus what I put into this round", however many
//!   entries and exits happened.
//! * `last_market` — the previous round's frozen result, including
//!   settlement, so the panel can show the outcome while the next round
//!   runs.
//! * `lifetime_pnl_tenths` — `equity − net_contributed`, i.e. everything
//!   ever made on the cash put in, live. For a strategy the cash put in
//!   is its allocation plus top-ups minus withdrawals; for the main
//!   account it is the starting balance minus what is currently parked
//!   in strategies, so the rows sum to a true total.

use serde::Serialize;

use crate::engine::{Exchange, MarketResult, StrategyStatus};
use crate::types::{Address, DEFAULT_SERIES, SeriesId};

use super::positions::{FlipQuote, PositionSide, SeriesPositionRow, build_position_snapshot};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AccountKind {
    Main,
    Strategy,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AccountRow {
    /// Account address. For strategies this doubles as the strategy id.
    pub id: Address,
    pub kind: AccountKind,
    /// Strategy name, or the user's username (fallback "Main") on the
    /// main row.
    pub name: String,
    /// Strategy lifecycle; `None` on the main row.
    pub status: Option<StrategyStatus>,
    pub created_at_ms: Option<u64>,
    /// The one series a strategy trades; `None` on the main row, which
    /// may hold positions in every series.
    pub home_series: Option<SeriesId>,

    // ── Open position (see `PositionSnapshotMessage`) ──
    pub side: PositionSide,
    pub size_shares: u32,
    pub signed_position: i64,
    pub avg_entry_yes_tenths: Option<u16>,
    pub cost_basis_tenths: i64,
    pub mark_yes_tenths: Option<u16>,
    pub mark_value_tenths: Option<i64>,
    pub unrealized_pnl_tenths: Option<i64>,
    pub return_pct: Option<f64>,
    pub flip_quote: Option<FlipQuote>,

    // ── This round (default series) ──
    pub realized_pnl_this_market_tenths: i64,
    /// `realized + unrealized` — the round's net so far.
    pub market_pnl_tenths: i64,
    /// Orders this round that received at least one fill.
    pub trades_this_market: u32,

    // ── Every series ──
    /// One row per series the exchange runs, marked to that series' book.
    pub series_positions: Vec<SeriesPositionRow>,
    /// Σ unrealized over every series' open position.
    pub unrealized_total_tenths: i64,
    /// Σ (realized this round + unrealized) over every series.
    pub market_pnl_total_tenths: i64,

    // ── Cash ──
    pub balance_tenths: i64,
    pub locked_tenths: i64,
    /// `balance + locked + Σ_series mark_value` — spans every series so
    /// lifetime PnL stays right while a position is open elsewhere.
    pub equity_tenths: i64,

    // ── Lifetime ──
    pub net_contributed_tenths: i64,
    /// `equity − net_contributed`, live.
    pub lifetime_pnl_tenths: i64,
    /// Σ settled round results; excludes the in-progress round.
    pub lifetime_realized_pnl_tenths: i64,
    pub last_market: Option<MarketResult>,
}

/// Build the row for `address`. `None` if there is no such account, or if
/// it is a strategy-less sub-account (an orphan awaiting the resolution
/// sweep, which must not be shown).
pub fn build_account_row(exchange: &Exchange, address: Address) -> Option<AccountRow> {
    let acct = exchange.account(&address)?;
    let snap = build_position_snapshot(exchange, address, DEFAULT_SERIES)?;
    let engine = exchange.engine(DEFAULT_SERIES);

    let (kind, name, status, created_at_ms, net_contributed) = if acct.is_sub_account() {
        let strategy = acct.strategy.as_ref()?;
        (
            AccountKind::Strategy,
            strategy.name.clone(),
            Some(strategy.status),
            Some(strategy.created_at_ms),
            acct.net_contributed.map(|t| t.0).unwrap_or(0),
        )
    } else {
        let parked_in_children: i64 = exchange
            .sub_accounts_of(&address)
            .iter()
            .filter_map(|c| exchange.account(c))
            .filter_map(|c| c.net_contributed)
            .map(|t| t.0)
            .sum();
        (
            AccountKind::Main,
            acct.username.clone().unwrap_or_else(|| "Main".to_string()),
            None,
            None,
            exchange.starting_balance().0 - parked_in_children,
        )
    };

    let ps = engine.position(&address);
    let mark_value_total: i64 = snap
        .series_positions
        .iter()
        .map(|r| r.mark_value_tenths.unwrap_or(0))
        .sum();
    let unrealized_total: i64 = snap
        .series_positions
        .iter()
        .map(|r| r.unrealized_pnl_tenths.unwrap_or(0))
        .sum();
    let market_pnl_total: i64 = snap.series_positions.iter().map(|r| r.market_pnl_tenths).sum();
    let equity = acct.usdc_balance.0 + acct.locked.0 + mark_value_total;
    let realized = ps.realized_pnl_this_market.0;

    Some(AccountRow {
        id: address,
        kind,
        name,
        status,
        created_at_ms,
        home_series: acct.home_series,
        side: snap.side,
        size_shares: snap.size_shares,
        signed_position: snap.signed_position,
        avg_entry_yes_tenths: snap.avg_entry_yes_tenths,
        cost_basis_tenths: snap.cost_basis_tenths,
        mark_yes_tenths: snap.mark_yes_tenths,
        mark_value_tenths: snap.mark_value_tenths,
        unrealized_pnl_tenths: snap.unrealized_pnl_tenths,
        return_pct: snap.return_pct,
        flip_quote: snap.flip_quote,
        realized_pnl_this_market_tenths: realized,
        market_pnl_tenths: realized + snap.unrealized_pnl_tenths.unwrap_or(0),
        trades_this_market: engine.trades_this_market(&address),
        series_positions: snap.series_positions,
        unrealized_total_tenths: unrealized_total,
        market_pnl_total_tenths: market_pnl_total,
        balance_tenths: acct.usdc_balance.0,
        locked_tenths: acct.locked.0,
        equity_tenths: equity,
        net_contributed_tenths: net_contributed,
        lifetime_pnl_tenths: equity - net_contributed,
        lifetime_realized_pnl_tenths: acct.lifetime_realized_pnl_tenths,
        last_market: ps.last_market,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Engine, TradeSource};
    use crate::engine::sub_accounts::test_support::named;
    use crate::ticks::PriceTick;
    use crate::types::{Qty, Side, Tenths};

    const START: Tenths = Tenths(1_000_000);

    fn addr(b: u8) -> Address {
        Address([b; 20])
    }
    fn tick(t: u16) -> PriceTick {
        PriceTick::from_tenths(t).unwrap()
    }

    #[test]
    fn account_row_equity_spans_series() {
        use crate::engine::Exchange;
        use crate::series::SERIES;
        use crate::types::SeriesId;
        let (user, mm) = (addr(1), addr(2));
        let mut x = Exchange::new(START, SERIES);
        x.register_user(user);
        x.get_or_create_account(user);
        x.seed_bot_account_in(mm, Tenths(50_000_000), "mm".into(), None, None, SeriesId(1));
        x.open_first_market(SeriesId(0), 50_000.0, 0, 60);
        x.open_first_market(SeriesId(1), 50_000.0, 0, 900);
        // Buy 10 @ 400 in the 15m series, then the maker requotes 500/600
        // so the position marks at 550.
        x.place_limit(SeriesId(1), mm, Side::Ask, tick(400), Qty(10), TradeSource::Bot).unwrap();
        x.place_market(SeriesId(1), user, Side::Bid, Qty(10), TradeSource::Ui).unwrap();
        x.place_limit(SeriesId(1), mm, Side::Bid, tick(500), Qty(1), TradeSource::Bot).unwrap();
        x.place_limit(SeriesId(1), mm, Side::Ask, tick(600), Qty(1), TradeSource::Bot).unwrap();

        let row = build_account_row(&x, user).unwrap();
        // The flat (default-series) fields are flat …
        assert_eq!(row.signed_position, 0);
        assert_eq!(row.market_pnl_tenths, 0);
        // … but equity and the totals see the 15m position.
        assert_eq!(row.balance_tenths, START.0 - 4000);
        assert_eq!(row.equity_tenths, START.0 - 4000 + 10 * 550);
        assert_eq!(row.unrealized_total_tenths, 10 * 150);
        assert_eq!(row.market_pnl_total_tenths, 10 * 150);
        assert_eq!(row.lifetime_pnl_tenths, 10 * 150);
        assert_eq!(row.series_positions.len(), SERIES.len());
        assert_eq!(row.series_positions[1].series, SeriesId(1));
        assert_eq!(row.series_positions[1].signed_position, 10);
    }

    #[test]
    fn main_and_strategy_rows_split_contributed_cash_and_sum_to_seed() {
        let (user, mm) = (addr(1), addr(2));
        let mut e = Engine::new(START);
        e.register_user(user);
        e.get_or_create_account(user).username = Some("jet".into());
        e.get_or_create_account(mm);
        e.open_first_market(50_000.0, 0, 60);
        let child = e.create_sub_account(user, Tenths(300_000)).unwrap();
        e.deploy_strategy(child, named("Momentum", e.series())).unwrap();

        let main = build_account_row(e.exchange(), user).unwrap();
        assert_eq!(main.kind, AccountKind::Main);
        assert_eq!(main.name, "jet");
        assert_eq!(main.status, None);
        assert_eq!(main.net_contributed_tenths, 700_000, "seed minus cash parked in the child");
        assert_eq!(main.equity_tenths, 700_000);
        assert_eq!(main.lifetime_pnl_tenths, 0, "allocating is not a loss");

        let strat = build_account_row(e.exchange(), child).unwrap();
        assert_eq!(strat.kind, AccountKind::Strategy);
        assert_eq!(strat.name, "Momentum");
        assert_eq!(strat.status, Some(StrategyStatus::Active));
        assert_eq!(strat.net_contributed_tenths, 300_000);
        assert_eq!(strat.lifetime_pnl_tenths, 0);

        assert_eq!(
            main.net_contributed_tenths + strat.net_contributed_tenths,
            START.0,
            "rows telescope to the single real deposit"
        );
    }

    #[test]
    fn market_pnl_combines_realized_and_unrealized_and_counts_trades() {
        let (user, mm) = (addr(1), addr(2));
        let mut e = Engine::new(START);
        e.register_user(user);
        e.get_or_create_account(user);
        e.get_or_create_account(mm);
        e.open_first_market(50_000.0, 0, 60);

        // Buy 10 @ 500, sell 4 @ 700 (realized +800), hold 6 with the
        // book marking at 700/800 mid = 750 (unrealized 6 × 250 = 1500).
        e.place_limit(mm, Side::Ask, tick(500), Qty(10), TradeSource::Bot).unwrap();
        e.place_market(user, Side::Bid, Qty(10), TradeSource::Ui).unwrap();
        e.place_limit(mm, Side::Bid, tick(700), Qty(4), TradeSource::Bot).unwrap();
        e.place_market(user, Side::Ask, Qty(4), TradeSource::Ui).unwrap();
        e.place_limit(mm, Side::Bid, tick(700), Qty(1), TradeSource::Bot).unwrap();
        e.place_limit(mm, Side::Ask, tick(800), Qty(1), TradeSource::Bot).unwrap();

        let row = build_account_row(e.exchange(), user).unwrap();
        assert_eq!(row.signed_position, 6);
        assert_eq!(row.realized_pnl_this_market_tenths, 800);
        assert_eq!(row.unrealized_pnl_tenths, Some(1500));
        assert_eq!(row.market_pnl_tenths, 2300);
        assert_eq!(row.trades_this_market, 2);
        // Lifetime is live: equity − seed.
        assert_eq!(row.lifetime_pnl_tenths, 2300);
        assert_eq!(row.lifetime_realized_pnl_tenths, 0);
        assert!(row.last_market.is_none());
    }

    #[test]
    fn orphan_sub_account_and_unknown_address_yield_no_row() {
        let user = addr(1);
        let mut e = Engine::new(START);
        e.register_user(user);
        e.get_or_create_account(user);
        let orphan = e.create_sub_account(user, Tenths(50_000)).unwrap();
        assert!(build_account_row(e.exchange(), orphan).is_none());
        assert!(build_account_row(e.exchange(), addr(9)).is_none());
    }
}
