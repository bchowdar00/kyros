//! The exchange: one shared [`Ledger`] plus one [`MarketEngine`] per
//! market series.
//!
//! [`Exchange`] is the single writer for all trading state. Every entry
//! point takes a [`SeriesId`] and routes to that series' engine, handing
//! it the shared ledger for the money side. [`Engine`] is a permanent
//! single-series facade over an `Exchange` with exactly the API the engine
//! had before series existed; the unit tests and single-series callers use
//! it unchanged.
//!
//! Money is in `Tenths` (1/10 of a cent). One contract pays $1 at
//! settlement, i.e. `Tenths(1000)`. Prices are in tenths of a cent as well,
//! so a single share at price `p` costs exactly `p` tenths — clean integer
//! math with no rounding.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

pub mod accounts;
pub mod book;
pub mod events;
#[cfg(test)]
mod events_tests;
pub mod ids;
pub mod ledger;
pub mod market;
pub mod model;
pub mod positions;
pub mod strategies;
pub mod sub_accounts;

use book::Book;
pub use ids::IdAllocator;
pub use ledger::Ledger;
pub use events::{ExecKind, ExecReject, ExecutionEvent, ExecutionSink};
pub use market::{MarketEngine, PlaceOpts};
use model::{Account, Fill, Order};

pub use model::{
    LeaderboardStanding, MarketResult, OrderType, TradeSource, UserOrderRow, UserOrderStatus,
};
pub use positions::PositionState;
pub use sub_accounts::{CloseSubAccountReport, SubAccountError};
pub use strategies::{DeployedStrategy, StrategyError, StrategyPatch, StrategyStatus};

use crate::ticks::PriceTick;
use crate::series::SeriesSpec;
use crate::types::{Address, DEFAULT_SERIES, MarketId, OrderId, Qty, SeriesId, Side, Tenths};

/// Payout per contract at settlement, in tenths of a cent. $1.
pub const PAYOUT_PER_CONTRACT: i64 = 1000;

/// Maximum number of fills retained in the engine's recent-trades buffer.
/// Older fills are dropped as new ones arrive.
pub const RECENT_FILLS_CAP: usize = 1000;

/// Maximum number of resolved markets kept in the rolling history.
pub const RESOLVED_MARKETS_CAP: usize = 60;

/// How many resolved markets' worth of per-user order rows stay readable
/// after resolution (see [`Engine::settled_user_orders`]). Rows from
/// older markets are dropped at each settlement.
pub const SETTLED_ORDERS_MARKETS_CAP: u32 = 10;

/// Smallest cash allocation a parent may place into a strategy
/// sub-account. $10.
pub const MIN_SUB_ACCOUNT_ALLOCATION: Tenths = Tenths(10_000);

/// Soft cap on live sub-accounts per parent. Each one is an executor
/// task polling the engine lock, so this is the single knob to tighten
/// if that ever becomes a problem.
pub const MAX_SUB_ACCOUNTS_PER_USER: usize = 200;

/// What a round is *about*, captured when it opens. The engine never
/// interprets it: the series' round driver (`scheduler.rs`) decides the
/// outcome and hands it in with a [`RoundClose`]. Untagged and flattened
/// into [`Market`], so a BTC round serializes exactly as it always has
/// (`strike_btc_usd` at the top level) and an NBA round carries its
/// matchup at the top level instead.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(untagged)]
pub enum RoundReference {
    /// BTC up/down: the spot price when the round opened.
    Btc { strike_btc_usd: f64 },
    /// NBA head-to-head: the matchup and the sharp line's pregame home win
    /// probability, both captured at open; `game_seq` is the world's game
    /// counter so a stale final from the previous game can be told apart.
    Nba {
        home_team: &'static str,
        away_team: &'static str,
        pregame_home_prob: f64,
        game_seq: u64,
    },
}

impl RoundReference {
    /// The BTC strike, if this is a BTC round.
    pub fn strike_btc_usd(&self) -> Option<f64> {
        match self {
            RoundReference::Btc { strike_btc_usd } => Some(*strike_btc_usd),
            RoundReference::Nba { .. } => None,
        }
    }
}

/// How a round closed, alongside its [`Outcome`]. Untagged and flattened
/// into [`MarketStatus::Resolved`] for the same wire compatibility as
/// [`RoundReference`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(untagged)]
pub enum RoundClose {
    Btc { close_price_btc_usd: f64 },
    Nba { home_score: u32, away_score: u32 },
}

/// A single market round: opens with a reference (a BTC strike, an NBA
/// matchup), closes after a fixed duration, resolves Yes/No when the
/// series' driver reports the outcome.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Market {
    pub id: MarketId,
    /// Series this round belongs to.
    #[serde(default)]
    pub series: SeriesId,
    #[serde(flatten)]
    pub reference: RoundReference,
    pub opened_at: u64,
    pub closes_at: u64,
    /// Round length in seconds (`closes_at − opened_at`).
    #[serde(default)]
    pub duration_secs: u64,
    pub status: MarketStatus,
}

impl Market {
    /// The BTC strike of a BTC round; `None` for other underlyings.
    pub fn strike_btc_usd(&self) -> Option<f64> {
        self.reference.strike_btc_usd()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MarketStatus {
    Open,
    Resolved {
        outcome: Outcome,
        #[serde(flatten)]
        close: RoundClose,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Yes,
    No,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ResolutionReport {
    pub resolved_id: MarketId,
    pub outcome: Outcome,
    pub cancelled_orders: u32,
    pub new_market_id: MarketId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderError {
    ZeroQuantity,
    UnknownAccount,
    InsufficientBalance { required: Tenths, available: Tenths },
    NoLiquidity,
    /// The account is bound to `home` and tried to trade `requested`.
    SeriesMismatch { home: SeriesId, requested: SeriesId },
    /// A `post_only` limit would have crossed the book.
    PostOnlyWouldCross,
    /// A `reduce_only` order found no position to reduce on that side.
    ReduceOnlyNoPosition,
}

impl std::fmt::Display for OrderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OrderError::ZeroQuantity => f.write_str("order qty must be positive"),
            OrderError::UnknownAccount => f.write_str("account does not exist"),
            OrderError::InsufficientBalance {
                required,
                available,
            } => write!(
                f,
                "insufficient balance: required {} tenths, available {} tenths",
                required.0, available.0,
            ),
            OrderError::NoLiquidity => f.write_str("no liquidity on opposite side"),
            OrderError::SeriesMismatch { home, requested } => write!(
                f,
                "account is bound to series {home} but the order targets {requested}",
            ),
            OrderError::PostOnlyWouldCross => f.write_str("post-only order would cross the book"),
            OrderError::ReduceOnlyNoPosition => {
                f.write_str("reduce-only order has no position to reduce")
            }
        }
    }
}

impl std::error::Error for OrderError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelError {
    UnknownOrder,
    NotOwner,
}

impl std::fmt::Display for CancelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CancelError::UnknownOrder => f.write_str("no such order on the book"),
            CancelError::NotOwner => f.write_str("order belongs to a different account"),
        }
    }
}

impl std::error::Error for CancelError {}

/// Why a [`MarketEngine::cancel_replace`] failed. `Cancel` means nothing
/// changed; `Place` means the old order is gone and the new one was
/// refused (its `Reject` was already reported on the execution stream).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplaceError {
    Cancel(CancelError),
    Place(OrderError),
}

impl std::fmt::Display for ReplaceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReplaceError::Cancel(e) => write!(f, "cancel_replace: {e}"),
            ReplaceError::Place(e) => write!(f, "cancel_replace: old order cancelled, new refused: {e}"),
        }
    }
}

impl std::error::Error for ReplaceError {}

/// Result of a successful place. `order_id` is the engine-assigned id;
/// `fills` lists every match generated during the cross, classified into
/// `Open` / `TransferYes` / `TransferNo` / `Close` sub-fills; `resting_qty`
/// is the portion of the order that remained on the book after the cross
/// (zero for market orders); `status` summarizes the outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlaceOutcome {
    pub order_id: OrderId,
    pub fills: Vec<Fill>,
    pub status: OrderStatus,
    pub resting_qty: crate::types::Qty,
}

/// Result of a true position flip. The engine first closes the current
/// position to flat, then opens the opposite side using the position's
/// current marked notional as the target budget.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FlipOutcome {
    pub target_notional_tenths: Tenths,
    pub close: PlaceOutcome,
    pub open: PlaceOutcome,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderStatus {
    /// Order fully filled by crossing; nothing rests.
    Filled,
    /// Some qty filled, the rest is resting (limit orders only).
    PartiallyFilled,
    /// No qty filled, all qty rests on the book.
    Rested,
    /// Market order: some or no qty filled, remainder discarded.
    Killed,
}


// ─────────────────────────────── Exchange ──────────────────────────────────

/// Every series' engine plus the one ledger they share.
#[derive(Debug)]
pub struct Exchange {
    ledger: Ledger,
    ids: Arc<IdAllocator>,
    /// One engine per entry of the series table handed to [`Exchange::new`].
    engines: Vec<MarketEngine>,
}

impl Exchange {
    pub fn new(starting_balance: Tenths, series: &[SeriesSpec]) -> Self {
        let ids = Arc::new(IdAllocator::new());
        let engines = series
            .iter()
            .map(|s| MarketEngine::new(s.id, ids.clone()))
            .collect();
        Self {
            ledger: Ledger::new(starting_balance),
            ids,
            engines,
        }
    }

    fn index(&self, series: SeriesId) -> usize {
        self.engines
            .iter()
            .position(|e| e.series() == series)
            .unwrap_or_else(|| panic!("series {series} is not running on this exchange"))
    }

    pub fn has_series(&self, series: SeriesId) -> bool {
        self.engines.iter().any(|e| e.series() == series)
    }

    pub fn engine(&self, series: SeriesId) -> &MarketEngine {
        &self.engines[self.index(series)]
    }

    pub fn engine_mut(&mut self, series: SeriesId) -> &mut MarketEngine {
        let i = self.index(series);
        &mut self.engines[i]
    }

    pub fn engines(&self) -> impl Iterator<Item = &MarketEngine> {
        self.engines.iter()
    }

    /// Series in table order.
    pub fn series(&self) -> impl Iterator<Item = SeriesId> + '_ {
        self.engines.iter().map(|e| e.series())
    }

    pub fn ledger(&self) -> &Ledger {
        &self.ledger
    }

    pub fn ledger_mut(&mut self) -> &mut Ledger {
        &mut self.ledger
    }

    pub fn ids(&self) -> &Arc<IdAllocator> {
        &self.ids
    }

    /// Install the execution-event sink on every series' engine. Events
    /// are emitted only for orders carrying a client id
    /// (`PlaceOpts::client_order_id`); without a sink they are dropped.
    pub fn set_execution_sink(&mut self, sink: ExecutionSink) {
        for e in &mut self.engines {
            e.set_execution_sink(sink.clone());
        }
    }

    /// One series' engine together with the ledger, for callers that
    /// drive the engine directly (bots, the scheduler).
    pub fn engine_and_ledger(&mut self, series: SeriesId) -> (&mut MarketEngine, &mut Ledger) {
        let i = self.index(series);
        (&mut self.engines[i], &mut self.ledger)
    }

    /// [`Exchange::engine_and_ledger`] bundled as a [`SeriesCtx`].
    pub fn ctx(&mut self, series: SeriesId) -> SeriesCtx<'_> {
        let (e, l) = self.engine_and_ledger(series);
        SeriesCtx::new(e, l)
    }

    // ── series-scoped trading entry points ──

    pub fn place_limit(
        &mut self,
        series: SeriesId,
        account: Address,
        side: Side,
        price: PriceTick,
        qty: Qty,
        source: TradeSource,
    ) -> Result<PlaceOutcome, OrderError> {
        let (e, l) = self.engine_and_ledger(series);
        e.place_limit(l, account, side, price, qty, source)
    }

    pub fn place_market(
        &mut self,
        series: SeriesId,
        account: Address,
        side: Side,
        qty: Qty,
        source: TradeSource,
    ) -> Result<PlaceOutcome, OrderError> {
        let (e, l) = self.engine_and_ledger(series);
        e.place_market(l, account, side, qty, source)
    }

    /// [`Exchange::place_limit`] with per-order [`PlaceOpts`].
    #[allow(clippy::too_many_arguments)]
    pub fn place_limit_with(
        &mut self,
        series: SeriesId,
        account: Address,
        side: Side,
        price: PriceTick,
        qty: Qty,
        source: TradeSource,
        opts: PlaceOpts,
    ) -> Result<PlaceOutcome, OrderError> {
        let (e, l) = self.engine_and_ledger(series);
        e.place_limit_with(l, account, side, price, qty, source, opts)
    }

    /// [`Exchange::place_market`] with per-order [`PlaceOpts`].
    pub fn place_market_with(
        &mut self,
        series: SeriesId,
        account: Address,
        side: Side,
        qty: Qty,
        source: TradeSource,
        opts: PlaceOpts,
    ) -> Result<PlaceOutcome, OrderError> {
        let (e, l) = self.engine_and_ledger(series);
        e.place_market_with(l, account, side, qty, source, opts)
    }

    pub fn flip_position(
        &mut self,
        series: SeriesId,
        account: Address,
        source: TradeSource,
    ) -> Result<FlipOutcome, OrderError> {
        let (e, l) = self.engine_and_ledger(series);
        e.flip_position(l, account, source)
    }

    pub fn cancel(
        &mut self,
        series: SeriesId,
        account: Address,
        id: OrderId,
    ) -> Result<Order, CancelError> {
        let (e, l) = self.engine_and_ledger(series);
        e.cancel(l, account, id)
    }

    /// [`MarketEngine::cancel_replace`] routed to `series`.
    #[allow(clippy::too_many_arguments)]
    pub fn cancel_replace(
        &mut self,
        series: SeriesId,
        account: Address,
        old_id: OrderId,
        side: Side,
        price: PriceTick,
        qty: Qty,
        source: TradeSource,
        opts: PlaceOpts,
    ) -> Result<PlaceOutcome, ReplaceError> {
        let (e, l) = self.engine_and_ledger(series);
        e.cancel_replace(l, account, old_id, side, price, qty, source, opts)
    }

    /// [`Exchange::cancel`] reported as an `Expiry` (good-till-time sweep).
    pub fn expire(
        &mut self,
        series: SeriesId,
        account: Address,
        id: OrderId,
    ) -> Result<Order, CancelError> {
        let (e, l) = self.engine_and_ledger(series);
        e.expire(l, account, id)
    }

    /// Cancel `id` wherever it rests: `preferred` first, then every other
    /// series in table order. Returns the series it was found in.
    pub fn cancel_any(
        &mut self,
        preferred: SeriesId,
        account: Address,
        id: OrderId,
    ) -> Result<(SeriesId, Order), CancelError> {
        let order: Vec<SeriesId> = std::iter::once(preferred)
            .chain(self.series().filter(|s| *s != preferred))
            .collect();
        for s in order {
            match self.cancel(s, account, id) {
                Err(CancelError::UnknownOrder) => continue,
                Err(e) => return Err(e),
                Ok(o) => return Ok((s, o)),
            }
        }
        Err(CancelError::UnknownOrder)
    }

    pub fn position(&self, series: SeriesId, address: &Address) -> PositionState {
        self.engine(series).position(address)
    }

    /// BTC convenience over [`Exchange::open_first_market_with`].
    pub fn open_first_market(
        &mut self,
        series: SeriesId,
        strike_btc_usd: f64,
        opens_at: u64,
        duration_secs: u64,
    ) {
        self.engine_mut(series)
            .open_first_market(strike_btc_usd, opens_at, duration_secs);
    }

    pub fn open_first_market_with(
        &mut self,
        series: SeriesId,
        reference: RoundReference,
        opens_at: u64,
        duration_secs: u64,
    ) {
        self.engine_mut(series)
            .open_first_market_with(reference, opens_at, duration_secs);
    }

    /// BTC convenience over [`Exchange::resolve_with`]: the outcome is
    /// close-above-strike.
    pub fn resolve_and_open_next(
        &mut self,
        series: SeriesId,
        close_price_btc_usd: f64,
        new_strike_btc_usd: f64,
        new_opens_at: u64,
        duration_secs: u64,
    ) -> ResolutionReport {
        let (e, l) = self.engine_and_ledger(series);
        e.resolve_and_open_next(l, close_price_btc_usd, new_strike_btc_usd, new_opens_at, duration_secs)
    }

    /// Resolve the current round of `series` with a driver-decided
    /// outcome and open the next one on `next`.
    pub fn resolve_with(
        &mut self,
        series: SeriesId,
        outcome: Outcome,
        close: RoundClose,
        next: RoundReference,
        new_opens_at: u64,
        duration_secs: u64,
    ) -> ResolutionReport {
        let (e, l) = self.engine_and_ledger(series);
        e.resolve_with(l, outcome, close, next, new_opens_at, duration_secs)
    }

    // ── ledger delegates ──

    pub fn root_of(&self, address: &Address) -> Address {
        self.ledger.root_of(address)
    }

    pub fn sub_accounts_of(&self, parent: &Address) -> &[Address] {
        self.ledger.sub_accounts_of(parent)
    }

    pub fn is_user_account(&self, address: &Address) -> bool {
        self.ledger.is_user_account(address)
    }

    pub fn register_user(&mut self, address: Address) {
        self.ledger.register_user(address);
    }

    pub fn starting_balance(&self) -> Tenths {
        self.ledger.starting_balance()
    }

    pub fn account(&self, address: &Address) -> Option<&Account> {
        self.ledger.account(address)
    }

    pub fn accounts_iter(&self) -> impl Iterator<Item = &Account> {
        self.ledger.accounts_iter()
    }

    pub fn leaderboard_standings(
        &self,
    ) -> impl Iterator<Item = (&Address, &LeaderboardStanding)> {
        self.ledger.leaderboard_standings()
    }

    /// Returns the account at `address`, creating it (seeded with the
    /// starting balance, no home series) if it doesn't yet exist.
    pub fn get_or_create_account(&mut self, address: Address) -> &mut Account {
        self.ledger.get_or_create(address)
    }

    /// Startup-only: seed a bot account bound to `home`. See
    /// [`Ledger::seed_bot_account`].
    pub fn seed_bot_account_in(
        &mut self,
        address: Address,
        balance: Tenths,
        username: String,
        color: Option<String>,
        emoji: Option<String>,
        home: SeriesId,
    ) {
        self.ledger
            .seed_bot_account(address, balance, username, color, emoji, home);
    }

    /// Walk every invariant that must hold between calls. Debug/test only.
    #[cfg(any(test, debug_assertions))]
    pub fn debug_assert_invariants(&self) {
        // 2. Per-series locked.
        let mut locked_sum = Tenths::ZERO;
        for e in &self.engines {
            assert_eq!(
                e.locked_total(),
                e.resting_lock_sum(),
                "series {}: locked_total != Σ resting locks",
                e.series()
            );
            locked_sum += e.locked_total();
        }
        let ledger_locked: i64 = self.ledger.accounts_iter().map(|a| a.locked.0).sum();
        assert_eq!(locked_sum.0, ledger_locked, "Σ locked_total != Σ Account.locked");
        for a in self.ledger.accounts_iter() {
            assert!(a.usdc_balance.0 >= 0, "negative balance on {}", a.address);
            assert!(a.locked.0 >= 0, "negative locked on {}", a.address);
        }

        // 3. Per-series positions.
        for e in &self.engines {
            let (mut sum, mut longs) = (0i64, 0i64);
            for (_, ps) in e.positions_iter() {
                sum += ps.position.0;
                longs += ps.position.0.max(0);
            }
            assert_eq!(sum, 0, "series {}: positions not zero-sum", e.series());
            assert_eq!(
                e.open_interest() as i64,
                longs,
                "series {}: open_interest != Σ long positions",
                e.series()
            );
        }

        // 4. Home series.
        for a in self.ledger.accounts_iter() {
            let Some(home) = a.home_series else { continue };
            for e in &self.engines {
                if e.series() == home {
                    continue;
                }
                assert!(
                    e.position(&a.address).is_flat(),
                    "{} (home {home}) holds a position in {}",
                    a.address,
                    e.series()
                );
                assert!(
                    !e.has_resting_orders_for(&a.address),
                    "{} (home {home}) has resting orders in {}",
                    a.address,
                    e.series()
                );
            }
        }
    }
}


// ─────────────────────────────── SeriesCtx ─────────────────────────────────

/// One series' engine borrowed together with the shared ledger: the unit
/// of work for anything that trades a single series (bots, the market
/// maker, a strategy executor). Offers the trading calls with the ledger
/// already threaded, plus the read accessors those callers need.
pub struct SeriesCtx<'a> {
    engine: &'a mut MarketEngine,
    ledger: &'a mut Ledger,
}

impl<'a> SeriesCtx<'a> {
    pub fn new(engine: &'a mut MarketEngine, ledger: &'a mut Ledger) -> Self {
        Self { engine, ledger }
    }

    pub fn engine(&self) -> &MarketEngine {
        self.engine
    }

    pub fn ledger(&self) -> &Ledger {
        self.ledger
    }

    pub fn series(&self) -> SeriesId {
        self.engine.series()
    }

    pub fn book(&self) -> &Book {
        self.engine.book()
    }

    pub fn current_market(&self) -> Option<&Market> {
        self.engine.current_market()
    }

    pub fn position(&self, address: &Address) -> PositionState {
        self.engine.position(address)
    }

    pub fn recent_trades(&self, limit: usize) -> impl Iterator<Item = &Fill> {
        self.engine.recent_trades(limit)
    }

    pub fn is_order_resting(&self, id: OrderId) -> bool {
        self.engine.is_order_resting(id)
    }

    pub fn account(&self, address: &Address) -> Option<&Account> {
        self.ledger.account(address)
    }

    pub fn place_limit(
        &mut self,
        account: Address,
        side: Side,
        price: PriceTick,
        qty: Qty,
        source: TradeSource,
    ) -> Result<PlaceOutcome, OrderError> {
        self.engine
            .place_limit(self.ledger, account, side, price, qty, source)
    }

    pub fn place_market(
        &mut self,
        account: Address,
        side: Side,
        qty: Qty,
        source: TradeSource,
    ) -> Result<PlaceOutcome, OrderError> {
        self.engine.place_market(self.ledger, account, side, qty, source)
    }

    pub fn place_limit_with(
        &mut self,
        account: Address,
        side: Side,
        price: PriceTick,
        qty: Qty,
        source: TradeSource,
        opts: PlaceOpts,
    ) -> Result<PlaceOutcome, OrderError> {
        self.engine
            .place_limit_with(self.ledger, account, side, price, qty, source, opts)
    }

    pub fn place_market_with(
        &mut self,
        account: Address,
        side: Side,
        qty: Qty,
        source: TradeSource,
        opts: PlaceOpts,
    ) -> Result<PlaceOutcome, OrderError> {
        self.engine
            .place_market_with(self.ledger, account, side, qty, source, opts)
    }

    pub fn flip_position(
        &mut self,
        account: Address,
        source: TradeSource,
    ) -> Result<FlipOutcome, OrderError> {
        self.engine.flip_position(self.ledger, account, source)
    }

    pub fn cancel(&mut self, account: Address, id: OrderId) -> Result<Order, CancelError> {
        self.engine.cancel(self.ledger, account, id)
    }

    pub fn expire(&mut self, account: Address, id: OrderId) -> Result<Order, CancelError> {
        self.engine.expire(self.ledger, account, id)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn cancel_replace(
        &mut self,
        account: Address,
        old_id: OrderId,
        side: Side,
        price: PriceTick,
        qty: Qty,
        source: TradeSource,
        opts: PlaceOpts,
    ) -> Result<PlaceOutcome, ReplaceError> {
        self.engine
            .cancel_replace(self.ledger, account, old_id, side, price, qty, source, opts)
    }
}

// ─────────────────────────── Single-series facade ──────────────────────────

/// Exactly the pre-series `Engine` API over a one-series [`Exchange`].
/// Permanent: it is what the unit tests and every single-series caller
/// use. Every method forwards to the exchange's only engine.
#[derive(Debug)]
pub struct Engine(Exchange);

impl Engine {
    pub fn new(starting_balance: Tenths) -> Self {
        Self::new_for_series(starting_balance, DEFAULT_SERIES)
    }

    pub fn new_for_series(starting_balance: Tenths, series: SeriesId) -> Self {
        let spec = series.spec();
        Engine(Exchange::new(starting_balance, std::slice::from_ref(spec)))
    }

    pub fn into_exchange(self) -> Exchange {
        self.0
    }

    pub fn exchange(&self) -> &Exchange {
        &self.0
    }

    pub fn exchange_mut(&mut self) -> &mut Exchange {
        &mut self.0
    }

    fn e(&self) -> &MarketEngine {
        &self.0.engines[0]
    }

    /// The one series engine behind the facade.
    pub fn market(&self) -> &MarketEngine {
        self.e()
    }

    fn parts(&mut self) -> (&mut MarketEngine, &mut Ledger) {
        (&mut self.0.engines[0], &mut self.0.ledger)
    }

    /// Engine + ledger bundled for callers written against [`SeriesCtx`].
    pub fn ctx(&mut self) -> SeriesCtx<'_> {
        let (e, l) = self.parts();
        SeriesCtx::new(e, l)
    }

    pub fn series(&self) -> SeriesId {
        self.e().series()
    }

    pub fn position(&self, address: &Address) -> PositionState {
        self.e().position(address)
    }

    pub fn positions_iter(&self) -> impl Iterator<Item = (&Address, &PositionState)> {
        self.e().positions_iter()
    }

    #[cfg(test)]
    pub fn set_position(&mut self, address: Address, state: PositionState) {
        self.0.engines[0].set_position(address, state);
    }

    pub fn root_of(&self, address: &Address) -> Address {
        self.0.root_of(address)
    }

    pub fn ledger(&self) -> &Ledger {
        self.0.ledger()
    }

    pub fn ledger_mut(&mut self) -> &mut Ledger {
        self.0.ledger_mut()
    }

    pub fn sub_accounts_of(&self, parent: &Address) -> &[Address] {
        self.0.sub_accounts_of(parent)
    }

    pub fn user_orders(&self, address: &Address) -> &[UserOrderRow] {
        self.e().user_orders(address)
    }

    pub fn settled_user_orders(&self, address: &Address) -> &[UserOrderRow] {
        self.e().settled_user_orders(address)
    }

    pub fn trades_this_market(&self, address: &Address) -> u32 {
        self.e().trades_this_market(address)
    }

    pub fn is_user_account(&self, address: &Address) -> bool {
        self.0.is_user_account(address)
    }

    pub fn register_user(&mut self, address: Address) {
        self.0.register_user(address);
    }

    pub fn book(&self) -> &Book {
        self.e().book()
    }

    pub fn starting_balance(&self) -> Tenths {
        self.0.starting_balance()
    }

    pub fn account(&self, address: &Address) -> Option<&Account> {
        self.0.account(address)
    }

    pub fn accounts_iter(&self) -> impl Iterator<Item = &Account> {
        self.0.accounts_iter()
    }

    pub fn leaderboard_standings(
        &self,
    ) -> impl Iterator<Item = (&Address, &LeaderboardStanding)> {
        self.0.leaderboard_standings()
    }

    pub fn get_or_create_account(&mut self, address: Address) -> &mut Account {
        self.0.get_or_create_account(address)
    }

    /// Startup-only: seed a bot account bound to this engine's series.
    pub fn seed_bot_account(
        &mut self,
        address: Address,
        balance: Tenths,
        username: String,
        color: Option<String>,
        emoji: Option<String>,
    ) {
        let home = self.series();
        self.seed_bot_account_in(address, balance, username, color, emoji, home);
    }

    pub fn seed_bot_account_in(
        &mut self,
        address: Address,
        balance: Tenths,
        username: String,
        color: Option<String>,
        emoji: Option<String>,
        home: SeriesId,
    ) {
        self.0.seed_bot_account_in(address, balance, username, color, emoji, home);
    }

    pub fn open_interest(&self) -> u64 {
        self.e().open_interest()
    }

    pub fn locked_total(&self) -> Tenths {
        self.e().locked_total()
    }

    pub fn market_volume_tenths(&self) -> i64 {
        self.e().market_volume_tenths()
    }

    pub fn market_trade_count(&self) -> u64 {
        self.e().market_trade_count()
    }

    pub fn market_unique_traders(&self) -> u64 {
        self.e().market_unique_traders()
    }

    pub fn market_largest_trade(&self) -> Option<&Fill> {
        self.e().market_largest_trade()
    }

    pub fn mark_price(&self) -> Option<PriceTick> {
        self.e().mark_price()
    }

    pub fn is_order_resting(&self, id: OrderId) -> bool {
        self.e().is_order_resting(id)
    }

    pub fn recent_trades(&self, limit: usize) -> impl Iterator<Item = &Fill> {
        self.e().recent_trades(limit)
    }

    pub fn current_market(&self) -> Option<&Market> {
        self.e().current_market()
    }

    pub fn resolved_markets(&self, limit: usize) -> impl Iterator<Item = &Market> {
        self.e().resolved_markets(limit)
    }

    pub fn open_first_market(&mut self, strike_btc_usd: f64, opens_at: u64, duration_secs: u64) {
        self.0.engines[0].open_first_market(strike_btc_usd, opens_at, duration_secs);
    }

    pub fn open_first_market_with(&mut self, reference: RoundReference, opens_at: u64, duration_secs: u64) {
        self.0.engines[0].open_first_market_with(reference, opens_at, duration_secs);
    }

    pub fn resolve_and_open_next(
        &mut self,
        close_price_btc_usd: f64,
        new_strike_btc_usd: f64,
        new_opens_at: u64,
        duration_secs: u64,
    ) -> ResolutionReport {
        let (e, l) = self.parts();
        e.resolve_and_open_next(l, close_price_btc_usd, new_strike_btc_usd, new_opens_at, duration_secs)
    }

    pub fn resolve_with(
        &mut self,
        outcome: Outcome,
        close: RoundClose,
        next: RoundReference,
        new_opens_at: u64,
        duration_secs: u64,
    ) -> ResolutionReport {
        let (e, l) = self.parts();
        e.resolve_with(l, outcome, close, next, new_opens_at, duration_secs)
    }

    pub fn place_limit(
        &mut self,
        account: Address,
        side: Side,
        price: PriceTick,
        qty: Qty,
        source: TradeSource,
    ) -> Result<PlaceOutcome, OrderError> {
        let (e, l) = self.parts();
        e.place_limit(l, account, side, price, qty, source)
    }

    pub fn place_market(
        &mut self,
        account: Address,
        side: Side,
        qty: Qty,
        source: TradeSource,
    ) -> Result<PlaceOutcome, OrderError> {
        let (e, l) = self.parts();
        e.place_market(l, account, side, qty, source)
    }

    pub fn flip_position(
        &mut self,
        account: Address,
        source: TradeSource,
    ) -> Result<FlipOutcome, OrderError> {
        let (e, l) = self.parts();
        e.flip_position(l, account, source)
    }

    pub fn cancel(&mut self, account: Address, id: OrderId) -> Result<Order, CancelError> {
        let (e, l) = self.parts();
        e.cancel(l, account, id)
    }

    /// Store `record` on `account`; the record's series must be this
    /// engine's series.
    pub fn deploy_strategy(
        &mut self,
        account: Address,
        record: DeployedStrategy,
    ) -> Result<DeployedStrategy, StrategyError> {
        self.0.deploy_strategy_in(account, record)
    }

    pub fn get_strategy(
        &self,
        account: &Address,
    ) -> Result<Option<DeployedStrategy>, StrategyError> {
        self.0.get_strategy(account)
    }

    pub fn update_strategy(
        &mut self,
        account: &Address,
        patch: StrategyPatch,
    ) -> Result<DeployedStrategy, StrategyError> {
        self.0.update_strategy(account, patch)
    }

    pub fn delete_strategy(&mut self, account: &Address) -> Result<(), StrategyError> {
        self.0.delete_strategy(account)
    }

    /// Create a sub-account bound to this engine's series.
    pub fn create_sub_account(
        &mut self,
        parent: Address,
        allocation: Tenths,
    ) -> Result<Address, SubAccountError> {
        let home = self.series();
        self.0.create_sub_account_in(parent, allocation, home)
    }

    pub fn create_sub_account_in(
        &mut self,
        parent: Address,
        allocation: Tenths,
        home: SeriesId,
    ) -> Result<Address, SubAccountError> {
        self.0.create_sub_account_in(parent, allocation, home)
    }

    pub fn create_sub_account_at(
        &mut self,
        parent: Address,
        child: Address,
        allocation: Tenths,
        home: SeriesId,
    ) -> Result<(), SubAccountError> {
        self.0.create_sub_account_at(parent, child, allocation, home)
    }

    pub fn fund_sub_account(
        &mut self,
        parent: Address,
        child: Address,
        amount: Tenths,
    ) -> Result<(), SubAccountError> {
        self.0.fund_sub_account(parent, child, amount)
    }

    pub fn close_sub_account(
        &mut self,
        parent: Address,
        child: Address,
    ) -> Result<CloseSubAccountReport, SubAccountError> {
        self.0.close_sub_account(parent, child)
    }

    #[cfg(any(test, debug_assertions))]
    pub fn debug_assert_invariants(&self) {
        self.0.debug_assert_invariants()
    }
}

// ──────────────────────────── Strategy storage ─────────────────────────────

impl Exchange {
    /// Persist a compiled strategy record on `account`, replacing any
    /// existing one. The record's series must be the sub-account's home
    /// series. Compilation and the runtime start happen in the API layer;
    /// the engine only keeps the record.
    pub fn deploy_strategy_in(
        &mut self,
        account: Address,
        record: DeployedStrategy,
    ) -> Result<DeployedStrategy, StrategyError> {
        let acct = self
            .ledger
            .account_mut(&account)
            .ok_or(StrategyError::AccountNotFound)?;
        if !acct.is_sub_account() {
            return Err(StrategyError::InvalidStrategy(
                "strategies must be deployed on a sub-account".into(),
            ));
        }
        if acct.home_series != Some(record.series) {
            return Err(StrategyError::InvalidStrategy(
                "strategy series must match the sub-account's series".into(),
            ));
        }
        acct.strategy = Some(record.clone());
        Ok(record)
    }

    /// Fetch the account's strategy, or `None` if none is stored.
    /// Returns `AccountNotFound` only when the account itself is missing.
    pub fn get_strategy(
        &self,
        account: &Address,
    ) -> Result<Option<DeployedStrategy>, StrategyError> {
        let acct = self
            .ledger
            .account(account)
            .ok_or(StrategyError::AccountNotFound)?;
        Ok(acct.strategy.clone())
    }

    /// Apply a partial update. Only `Some` fields are written. An empty
    /// patch is a no-op (returns the strategy unchanged).
    pub fn update_strategy(
        &mut self,
        account: &Address,
        patch: StrategyPatch,
    ) -> Result<DeployedStrategy, StrategyError> {
        let acct = self
            .ledger
            .account_mut(account)
            .ok_or(StrategyError::AccountNotFound)?;
        let strategy = acct
            .strategy
            .as_mut()
            .ok_or(StrategyError::StrategyNotFound)?;
        if let Some(name) = patch.name {
            let trimmed = name.trim();
            if trimmed.is_empty() {
                return Err(StrategyError::InvalidStrategy(
                    "name must not be empty".into(),
                ));
            }
            strategy.name = trimmed.to_string();
        }
        if let Some(status) = patch.status {
            strategy.status = status;
        }
        Ok(strategy.clone())
    }

    /// Clear the account's strategy. Errors `StrategyNotFound` when there
    /// was nothing to delete (so DELETE is not idempotent — callers can
    /// treat 404 as "already gone").
    pub fn delete_strategy(&mut self, account: &Address) -> Result<(), StrategyError> {
        let acct = self
            .ledger
            .account_mut(account)
            .ok_or(StrategyError::AccountNotFound)?;
        if acct.strategy.take().is_none() {
            return Err(StrategyError::StrategyNotFound);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::model::FillKind;
    use crate::types::Position;

    const START: Tenths = Tenths(1_000_000); // $1000

    fn addr(s: &str) -> Address {
        s.parse().unwrap()
    }
    fn tick(t: u16) -> PriceTick {
        PriceTick::from_tenths(t).unwrap()
    }

    const A: &str = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn engine_with_accounts() -> (Engine, Address, Address) {
        let mut e = Engine::new(START);
        e.get_or_create_account(addr(A));
        e.get_or_create_account(addr(B));
        (e, addr(A), addr(B))
    }

    #[test]
    fn seed_bot_account_uses_custom_balance_and_identity() {
        let mut e = Engine::new(START);
        e.seed_bot_account(
            addr(A),
            Tenths(5_000_000),
            "ralph".to_string(),
            Some("#ff0000".to_string()),
            Some("🦊".to_string()),
        );
        let acct = e.account(&addr(A)).expect("seeded account exists");
        let acct_ps = e.position(&addr(A));
        assert_eq!(acct.usdc_balance, Tenths(5_000_000));
        assert_eq!(acct.username.as_deref(), Some("ralph"));
        assert_eq!(acct.color.as_deref(), Some("#ff0000"));
        assert_eq!(acct.emoji.as_deref(), Some("🦊"));
        assert_eq!(acct.locked, Tenths::ZERO);
        assert_eq!(acct_ps.position, Position::FLAT);
    }

    #[test]
    fn bot_cannot_trade_outside_home_series() {
        // A bot seeded into series 0 is rejected by a series-1 engine.
        let mut e = Engine::new_for_series(START, SeriesId(1));
        e.seed_bot_account_in(addr(A), START, "bot".into(), None, None, SeriesId(0));
        e.open_first_market(100_000.0, 0, 900);
        let err = e
            .place_limit(addr(A), Side::Bid, tick(500), Qty(1), TradeSource::Bot)
            .unwrap_err();
        assert_eq!(
            err,
            OrderError::SeriesMismatch {
                home: SeriesId(0),
                requested: SeriesId(1),
            }
        );
        let err = e
            .place_market(addr(A), Side::Bid, Qty(1), TradeSource::Bot)
            .unwrap_err();
        assert!(matches!(err, OrderError::SeriesMismatch { .. }));
        assert!(matches!(
            e.flip_position(addr(A), TradeSource::Bot),
            Err(OrderError::SeriesMismatch { .. })
        ));
        // Nothing moved.
        let acct = e.account(&addr(A)).unwrap();
        assert_eq!(acct.usdc_balance, START);
        assert_eq!(acct.locked, Tenths::ZERO);
        assert!(e.book().is_empty());
    }

    #[test]
    fn sub_account_cannot_trade_outside_home_series() {
        let mut e = Engine::new_for_series(START, SeriesId(1));
        e.register_user(addr(A));
        e.get_or_create_account(addr(A));
        let child = e
            .create_sub_account_in(addr(A), Tenths(100_000), SeriesId(0))
            .unwrap();
        e.open_first_market(100_000.0, 0, 900);
        assert!(matches!(
            e.place_limit(child, Side::Bid, tick(500), Qty(1), TradeSource::Strategy),
            Err(OrderError::SeriesMismatch { home: SeriesId(0), requested: SeriesId(1) })
        ));
    }

    #[test]
    fn main_account_may_trade_any_series() {
        // A human's main account has no home series: both engines accept it.
        for series in SeriesId::all() {
            let mut e = Engine::new_for_series(START, series);
            e.register_user(addr(A));
            e.get_or_create_account(addr(A));
            e.open_first_market(100_000.0, 0, series.duration_secs());
            assert!(e.account(&addr(A)).unwrap().home_series.is_none());
            e.place_limit(addr(A), Side::Bid, tick(500), Qty(1), TradeSource::Ui)
                .unwrap();
        }
    }

    #[test]
    fn seed_bot_account_overrides_existing_balance() {
        let mut e = Engine::new(START);
        // Touch via get_or_create_account first: gets the default START balance.
        e.get_or_create_account(addr(A));
        assert_eq!(e.account(&addr(A)).unwrap().usdc_balance, START);
        // Re-seeding overwrites the balance and stamps a username.
        e.seed_bot_account(addr(A), Tenths(7_500_000), "bot".to_string(), None, None);
        let acct = e.account(&addr(A)).unwrap();
        assert_eq!(acct.usdc_balance, Tenths(7_500_000));
        assert_eq!(acct.username.as_deref(), Some("bot"));
    }

    #[test]
    fn place_limit_bid_locks_qty_times_price() {
        let (mut e, a, _) = engine_with_accounts();
        let out = e.place_limit(a, Side::Bid, tick(650), Qty(10), TradeSource::Bot).unwrap();
        assert_eq!(out.status, OrderStatus::Rested);
        assert_eq!(out.resting_qty, Qty(10));
        let acct = e.account(&a).unwrap();
        assert_eq!(acct.locked, Tenths(6500));
        assert_eq!(acct.usdc_balance, Tenths(1_000_000 - 6500));
    }

    #[test]
    fn place_limit_ask_locks_qty_times_complement() {
        let (mut e, a, _) = engine_with_accounts();
        e.place_limit(a, Side::Ask, tick(650), Qty(10), TradeSource::Bot).unwrap();
        let acct = e.account(&a).unwrap();
        // Ask at 65¢ for 10: worst case opens short, locks (1000-650) * 10 = 3500.
        assert_eq!(acct.locked, Tenths(3500));
        assert_eq!(acct.usdc_balance, Tenths(1_000_000 - 3500));
    }

    #[test]
    fn mark_price_is_none_on_empty_book() {
        let (e, _, _) = engine_with_accounts();
        assert_eq!(e.mark_price(), None);
    }

    #[test]
    fn mark_price_pins_to_ceiling_when_only_bids() {
        // Bid-only book → asks vanished → mark pinned to the tick-grid
        // ceiling (99.9¢), NOT None and NOT the bid price.
        let (mut e, a, _) = engine_with_accounts();
        e.place_limit(a, Side::Bid, tick(400), Qty(1), TradeSource::Bot).unwrap();
        assert_eq!(
            e.mark_price(),
            PriceTick::from_tenths(999),
            "bid-only book should mark at 99.9¢, not None",
        );
    }

    #[test]
    fn mark_price_pins_to_floor_when_only_asks() {
        let (mut e, a, _) = engine_with_accounts();
        e.place_limit(a, Side::Ask, tick(600), Qty(1), TradeSource::Bot).unwrap();
        assert_eq!(
            e.mark_price(),
            PriceTick::from_tenths(1),
            "ask-only book should mark at 0.1¢, not None",
        );
    }

    #[test]
    fn is_order_resting_tracks_lifecycle() {
        let (mut e, a, _) = engine_with_accounts();
        let out = e.place_limit(a, Side::Bid, tick(400), Qty(1), TradeSource::Bot).unwrap();
        let id = out.order_id;
        assert!(e.is_order_resting(id), "freshly-rested limit must be live");

        // Bogus id is not on the book.
        assert!(!e.is_order_resting(OrderId(u64::MAX)));

        // After cancellation, the order is no longer resting.
        e.cancel(a, id).expect("cancel");
        assert!(!e.is_order_resting(id), "cancelled order must not be live");
    }

    #[test]
    fn mark_price_returns_mid_of_best_bid_and_ask() {
        let (mut e, a, b) = engine_with_accounts();
        e.place_limit(a, Side::Bid, tick(400), Qty(1), TradeSource::Bot).unwrap();
        e.place_limit(b, Side::Ask, tick(600), Qty(1), TradeSource::Bot).unwrap();
        // mid of 400 and 600 in tenths = 500 → 50¢, which is a valid tick.
        assert_eq!(e.mark_price(), Some(tick(500)));
    }

    #[test]
    fn cancel_restores_full_balance() {
        let (mut e, a, _) = engine_with_accounts();
        let out = e.place_limit(a, Side::Bid, tick(650), Qty(10), TradeSource::Bot).unwrap();
        let cancelled = e.cancel(a, out.order_id).unwrap();
        assert_eq!(cancelled.qty_open, Qty(10));
        let acct = e.account(&a).unwrap();
        assert_eq!(acct.locked, Tenths::ZERO);
        assert_eq!(acct.usdc_balance, START);
        // Book has no resting order; no stale lock entry.
        assert!(e.book().is_empty());
    }

    #[test]
    fn cancel_unknown_order_errors() {
        let (mut e, a, _) = engine_with_accounts();
        assert_eq!(e.cancel(a, OrderId(999)), Err(CancelError::UnknownOrder));
    }

    #[test]
    fn cancel_not_owner_errors() {
        let (mut e, a, b) = engine_with_accounts();
        let out = e.place_limit(a, Side::Bid, tick(650), Qty(1), TradeSource::Bot).unwrap();
        assert_eq!(e.cancel(b, out.order_id), Err(CancelError::NotOwner));
        // Order is still on the book; A's lock is intact.
        assert_eq!(e.account(&a).unwrap().locked, Tenths(650));
        assert!(e.book().get_order(out.order_id).is_some());
    }

    #[test]
    fn place_zero_qty_errors() {
        let (mut e, a, _) = engine_with_accounts();
        assert_eq!(
            e.place_limit(a, Side::Bid, tick(650), Qty(0), TradeSource::Bot),
            Err(OrderError::ZeroQuantity),
        );
    }

    #[test]
    fn place_unknown_account_errors() {
        let mut e = Engine::new(START);
        let ghost = addr("0xcccccccccccccccccccccccccccccccccccccccc");
        assert_eq!(
            e.place_limit(ghost, Side::Bid, tick(650), Qty(1), TradeSource::Bot),
            Err(OrderError::UnknownAccount),
        );
    }

    #[test]
    fn place_insufficient_balance_errors() {
        let mut e = Engine::new(Tenths(500)); // 50¢ starting balance — tiny
        let a = addr(A);
        e.get_or_create_account(a);
        let err = e.place_limit(a, Side::Bid, tick(650), Qty(1), TradeSource::Bot).unwrap_err();
        assert!(
            matches!(err, OrderError::InsufficientBalance { required, available }
            if required == Tenths(650) && available == Tenths(500))
        );
        // Nothing committed: balance untouched, no order on the book.
        assert_eq!(e.account(&a).unwrap().usdc_balance, Tenths(500));
        assert!(e.book().is_empty());
    }

    // ─────────────────────────── matching scenarios ────────────────────────────

    const C: &str = "0xcccccccccccccccccccccccccccccccccccccccc";

    fn engine_with(n: usize) -> (Engine, Vec<Address>) {
        let mut e = Engine::new(START);
        let mut addrs = Vec::new();
        for byte in 0xa0u8..(0xa0 + n as u8) {
            let bytes = [byte; 20];
            let a = Address(bytes);
            e.get_or_create_account(a);
            addrs.push(a);
        }
        (e, addrs)
    }

    /// Asserts conservation: sum of (free + locked) across all participants
    /// plus the system-held escrow (OI × $1) equals the sum of seeded
    /// starting balances. Catches accounting drift anywhere in the engine.
    fn assert_conservation(e: &Engine, addrs: &[Address]) {
        let user_total: i64 = addrs
            .iter()
            .map(|a| {
                let acct = e.account(a).unwrap();
                acct.usdc_balance.0 + acct.locked.0
            })
            .sum();
        let escrow = (e.open_interest() as i64) * PAYOUT_PER_CONTRACT;
        let expected = (addrs.len() as i64) * START.0;
        assert_eq!(
            user_total + escrow,
            expected,
            "cash conservation broken: users={} escrow={} expected={}",
            user_total,
            escrow,
            expected,
        );
        let pos_sum: i64 = addrs.iter().map(|a| e.position(a).position.0).sum();
        assert_eq!(pos_sum, 0, "positions not zero-sum");
        let locked_sum: i64 = addrs.iter().map(|a| e.account(a).unwrap().locked.0).sum();
        assert_eq!(e.locked_total().0, locked_sum, "locked_total drifted from Σ locked");
    }

    #[test]
    fn market_order_reserves_peak_then_releases_remainder() {
        let (mut e, addrs) = engine_with(2);
        let (maker, taker) = (addrs[0], addrs[1]);
        e.place_limit(maker, Side::Ask, tick(600), Qty(5), TradeSource::Bot).unwrap();
        e.place_limit(maker, Side::Ask, tick(700), Qty(5), TradeSource::Bot).unwrap();
        let before = e.account(&taker).unwrap().usdc_balance;

        let out = e.place_market(taker, Side::Bid, Qty(10), TradeSource::Bot).unwrap();
        assert_eq!(out.status, OrderStatus::Filled);

        // Nothing stays reserved after the call and the balance is exactly
        // what an unreserved cross would have left: 5×600 + 5×700 drawn.
        let after = e.account(&taker).unwrap();
        assert_eq!(after.locked, Tenths::ZERO);
        assert_eq!(after.usdc_balance, Tenths(before.0 - 6500));
        assert_eq!(e.locked_total(), Tenths::ZERO);
        assert_conservation(&e, &addrs);
    }

    #[test]
    fn open_fill_between_fresh_accounts() {
        let (mut e, addrs) = engine_with(2);
        let (a, b) = (addrs[0], addrs[1]);
        // A rests an ask at 65¢ for 1; B takes it with a bid.
        e.place_limit(a, Side::Ask, tick(650), Qty(1), TradeSource::Bot).unwrap();
        let out = e.place_limit(b, Side::Bid, tick(700), Qty(1), TradeSource::Bot).unwrap();
        assert_eq!(out.status, OrderStatus::Filled);
        assert_eq!(out.fills.len(), 1);
        assert_eq!(out.fills[0].kind, FillKind::Open);
        assert_eq!(out.fills[0].price, tick(650));
        assert_eq!(out.fills[0].qty, Qty(1));

        // Positions: B long 1, A short 1.
        assert_eq!(e.position(&b).position, Position(1));
        assert_eq!(e.position(&a).position, Position(-1));
        // OI = 1.
        assert_eq!(e.open_interest(), 1);
        // Cash: B paid 650 to open long; A paid 350 to open short (collateral).
        // B placed a bid at 700 (locked 700), filled at 650 → kept 50 back.
        assert_eq!(e.account(&b).unwrap().usdc_balance, Tenths(1_000_000 - 650));
        assert_eq!(e.account(&a).unwrap().usdc_balance, Tenths(1_000_000 - 350));
        assert_eq!(e.account(&a).unwrap().locked, Tenths::ZERO);
        assert_eq!(e.account(&b).unwrap().locked, Tenths::ZERO);
        assert_conservation(&e, &addrs);
    }

    #[test]
    fn transfer_yes_long_to_long() {
        let (mut e, addrs) = engine_with(3);
        let (a, b, c) = (addrs[0], addrs[1], addrs[2]);
        // Open 1 between A (short) and B (long) at 50¢.
        e.place_limit(a, Side::Ask, tick(500), Qty(1), TradeSource::Bot).unwrap();
        e.place_limit(b, Side::Bid, tick(700), Qty(1), TradeSource::Bot).unwrap();
        let b_after_open = e.account(&b).unwrap().usdc_balance;
        // Now B sells their long to C at 60¢ (TransferYes).
        e.place_limit(b, Side::Ask, tick(600), Qty(1), TradeSource::Bot).unwrap();
        let out = e.place_limit(c, Side::Bid, tick(700), Qty(1), TradeSource::Bot).unwrap();
        assert_eq!(out.fills.len(), 1);
        assert_eq!(out.fills[0].kind, FillKind::TransferYes);

        // B went from +1 to 0; C from 0 to +1; A still -1; OI unchanged at 1.
        assert_eq!(e.position(&b).position, Position::FLAT);
        assert_eq!(e.position(&c).position, Position(1));
        assert_eq!(e.position(&a).position, Position(-1));
        assert_eq!(e.open_interest(), 1);
        // B's cash: had b_after_open (after buying at 500). Sold at 600 → +600 (TransferYes seller cash flow).
        // The ask placement reserved (1000-600)=400, refunded on fill, so net just +600.
        assert_eq!(
            e.account(&b).unwrap().usdc_balance,
            b_after_open + Tenths(600),
        );
        assert_eq!(e.account(&b).unwrap().locked, Tenths::ZERO);
        assert_conservation(&e, &addrs);
    }

    #[test]
    fn transfer_no_short_to_short() {
        let (mut e, addrs) = engine_with(3);
        let (a, b, c) = (addrs[0], addrs[1], addrs[2]);
        // Open 1 between A (short) and B (long) at 60¢.
        e.place_limit(a, Side::Ask, tick(600), Qty(1), TradeSource::Bot).unwrap();
        e.place_limit(b, Side::Bid, tick(700), Qty(1), TradeSource::Bot).unwrap();
        let a_after_open = e.account(&a).unwrap().usdc_balance;
        // A covers (buys YES); C takes the other side (opens short).
        // A places bid that crosses C's ask. So A is taker bid; C must rest the ask first.
        e.place_limit(c, Side::Ask, tick(650), Qty(1), TradeSource::Bot).unwrap();
        let out = e.place_limit(a, Side::Bid, tick(700), Qty(1), TradeSource::Bot).unwrap();
        assert_eq!(out.fills.len(), 1);
        assert_eq!(out.fills[0].kind, FillKind::TransferNo);
        // A: -1 → 0; C: 0 → -1; B still +1; OI unchanged.
        assert_eq!(e.position(&a).position, Position::FLAT);
        assert_eq!(e.position(&c).position, Position(-1));
        assert_eq!(e.position(&b).position, Position(1));
        assert_eq!(e.open_interest(), 1);
        // A's cash: bought to cover short at 650. Cash flow for "exit short" = +(1000-650)=350.
        // A placed bid lock 700, refunded on fill, net cash gain = 350.
        assert_eq!(
            e.account(&a).unwrap().usdc_balance,
            a_after_open + Tenths(350),
        );
        assert_eq!(e.account(&a).unwrap().locked, Tenths::ZERO);
        assert_conservation(&e, &addrs);
    }

    #[test]
    fn close_long_and_short() {
        let (mut e, addrs) = engine_with(2);
        let (a, b) = (addrs[0], addrs[1]);
        // Open 1 between A (short) and B (long) at 60¢.
        e.place_limit(a, Side::Ask, tick(600), Qty(1), TradeSource::Bot).unwrap();
        e.place_limit(b, Side::Bid, tick(700), Qty(1), TradeSource::Bot).unwrap();
        assert_eq!(e.open_interest(), 1);
        // Now both close: A bids (covers short), B asks (exits long).
        e.place_limit(b, Side::Ask, tick(650), Qty(1), TradeSource::Bot).unwrap();
        let out = e.place_limit(a, Side::Bid, tick(700), Qty(1), TradeSource::Bot).unwrap();
        assert_eq!(out.fills.len(), 1);
        assert_eq!(out.fills[0].kind, FillKind::Close);
        assert_eq!(e.position(&a).position, Position::FLAT);
        assert_eq!(e.position(&b).position, Position::FLAT);
        assert_eq!(e.open_interest(), 0);
        assert_eq!(e.account(&a).unwrap().locked, Tenths::ZERO);
        assert_eq!(e.account(&b).unwrap().locked, Tenths::ZERO);
        // A opened short at 60 (collateral 400), covered at 65 (refund 350): −50 PnL.
        // B opened long at 60 (cost 600), closed at 65 (refund 650): +50 PnL.
        assert_eq!(e.account(&a).unwrap().usdc_balance, Tenths(1_000_000 - 50),);
        assert_eq!(e.account(&b).unwrap().usdc_balance, Tenths(1_000_000 + 50),);
        assert_conservation(&e, &addrs);
    }

    #[test]
    fn position_flipping_fill() {
        let (mut e, addrs) = engine_with(2);
        let (a, b) = (addrs[0], addrs[1]);
        // Build A's short of 2 via two Opens.
        e.place_limit(a, Side::Ask, tick(500), Qty(2), TradeSource::Bot).unwrap();
        e.place_limit(b, Side::Bid, tick(700), Qty(2), TradeSource::Bot).unwrap();
        assert_eq!(e.position(&a).position, Position(-2));
        assert_eq!(e.open_interest(), 2);

        // Now A takes a huge bid that covers their short AND opens long.
        // First B rests two asks: one cheap (closes), one expensive (opens for A).
        // Simpler: B rests one ask at 60¢ for 5. A bids 5 at 70¢.
        // A's pre-trade pos: -2. q=5. a=2 (close shorts), b=3 (open long).
        // B's pre-trade pos: 0 (after closing their long via this very ask). Wait no, before A bids.
        // Reset by closing first: let's just open a new pair.
        // Actually for this test: A pos=-2, B pos=+2. B asks 5 at 60¢ — but B only has +2 long,
        // so b's ask opens shorts for the 3 extra. To keep B's account math simple, use a third.
        // Reset approach: skip this test variant and do a focused one.

        // Have a third counterparty open with B/C so the multi-leg below is clean.
        let _ = e; // throwaway; re-engineer below

        let (mut e, addrs) = engine_with(3);
        let (a, b, c) = (addrs[0], addrs[1], addrs[2]);
        // Build A short -2 via fill with B.
        e.place_limit(a, Side::Ask, tick(500), Qty(2), TradeSource::Bot).unwrap();
        e.place_limit(b, Side::Bid, tick(700), Qty(2), TradeSource::Bot).unwrap();
        assert_eq!(e.position(&a).position, Position(-2));
        assert_eq!(e.open_interest(), 2);

        // C rests an ask at 60¢ for 5; A bids 5 at 70¢ → flips −2 to +3.
        e.place_limit(c, Side::Ask, tick(600), Qty(5), TradeSource::Bot).unwrap();
        let out = e.place_limit(a, Side::Bid, tick(700), Qty(5), TradeSource::Bot).unwrap();
        // Classification: a=2 (close A's short), b=3 (open A's long); c=0 (C is flat), d=5 (open C's short).
        // close=0, transfer_no = 2 - 0 = 2, transfer_yes = 0, open = 5 - max(2,0) = 3.
        let kinds: Vec<_> = out.fills.iter().map(|f| (f.kind, f.qty)).collect();
        assert!(kinds.contains(&(FillKind::Open, Qty(3))));
        assert!(kinds.contains(&(FillKind::TransferNo, Qty(2))));
        assert_eq!(out.fills.iter().map(|f| f.qty.0).sum::<u32>(), 5);

        assert_eq!(e.position(&a).position, Position(3));
        assert_eq!(e.position(&c).position, Position(-5));
        assert_eq!(e.position(&b).position, Position(2));
        // OI: was 2, +open 3, -close 0 = 5.
        assert_eq!(e.open_interest(), 5);
        assert_conservation(&e, &addrs);
    }

    #[test]
    fn sweep_across_multiple_price_levels() {
        let (mut e, addrs) = engine_with(3);
        let (a, b, c) = (addrs[0], addrs[1], addrs[2]);
        // Two ask levels: A rests 2 at 60¢, B rests 3 at 65¢.
        e.place_limit(a, Side::Ask, tick(600), Qty(2), TradeSource::Bot).unwrap();
        e.place_limit(b, Side::Ask, tick(650), Qty(3), TradeSource::Bot).unwrap();
        // C bids 5 at 70¢ → sweeps both levels in price order.
        let out = e.place_limit(c, Side::Bid, tick(700), Qty(5), TradeSource::Bot).unwrap();
        assert_eq!(out.status, OrderStatus::Filled);
        assert_eq!(out.fills.len(), 2);
        // First match at 60¢, then at 65¢ (best-first).
        assert_eq!(out.fills[0].price, tick(600));
        assert_eq!(out.fills[0].qty, Qty(2));
        assert_eq!(out.fills[1].price, tick(650));
        assert_eq!(out.fills[1].qty, Qty(3));
        assert_conservation(&e, &addrs);
    }

    #[test]
    fn partial_cross_then_rest_residual() {
        let (mut e, addrs) = engine_with(2);
        let (a, b) = (addrs[0], addrs[1]);
        // A rests 2 asks at 65¢.
        e.place_limit(a, Side::Ask, tick(650), Qty(2), TradeSource::Bot).unwrap();
        // B bids 5 at 70¢ → fills 2, rests 3 at 70¢ as the new best bid.
        let out = e.place_limit(b, Side::Bid, tick(700), Qty(5), TradeSource::Bot).unwrap();
        assert_eq!(out.status, OrderStatus::PartiallyFilled);
        assert_eq!(out.resting_qty, Qty(3));
        assert_eq!(e.book().best_bid(), Some(tick(700)));
        assert_eq!(e.book().level_qty(tick(700)), Qty(3));
        // B's locked = remaining 3 × 700.
        assert_eq!(e.account(&b).unwrap().locked, Tenths(2100));
        assert_conservation(&e, &addrs);
    }

    #[test]
    fn self_trade_cancels_resting_and_continues() {
        let (mut e, addrs) = engine_with(2);
        let (a, b) = (addrs[0], addrs[1]);
        // A rests an ask at 65¢, B rests an ask at 66¢, A rests another ask at 67¢.
        let a_first = e
            .place_limit(a, Side::Ask, tick(650), Qty(1), TradeSource::Bot)
            .unwrap()
            .order_id;
        e.place_limit(b, Side::Ask, tick(660), Qty(1), TradeSource::Bot).unwrap();
        let a_second = e
            .place_limit(a, Side::Ask, tick(670), Qty(1), TradeSource::Bot)
            .unwrap()
            .order_id;
        let a_balance_before = e.account(&a).unwrap().usdc_balance;
        let a_locked_before = e.account(&a).unwrap().locked;

        // A places a bid at 70¢ for 3 — would match own asks at 650 and 670, plus B at 660.
        // Expected: self-trade cancels A's two asks; only matches B's 1 ask.
        let out = e.place_limit(a, Side::Bid, tick(700), Qty(3), TradeSource::Bot).unwrap();
        assert_eq!(out.fills.len(), 1);
        assert_eq!(out.fills[0].maker, b);
        assert_eq!(out.fills[0].price, tick(660));
        // Both of A's prior asks are cancelled and refunded.
        assert!(e.book().get_order(a_first).is_none());
        assert!(e.book().get_order(a_second).is_none());
        // After cancels: A's locked from those asks (350 + 330 = 680) should be back as free.
        // After the bid: A took position +1 at 660 (entered long). Cash flow −660.
        // Remaining bid qty: 3 − 1 = 2, rests at 700 → lock 1400.
        let a_after = e.account(&a).unwrap();
        let a_after_ps = e.position(&a);
        assert_eq!(a_after_ps.position, Position(1));
        assert_eq!(a_after.locked, Tenths(1400));
        // Cash: started at a_balance_before with a_locked_before in lock.
        // The two asks refunded a_locked_before to free. The bid: lock 2100, fill 1 → cash −660,
        // refund of 1 × 700 lock on fill (so net of that fill: +700 − 660 = +40), residual 2 stays locked.
        // Net cash change: +a_locked_before − 2100 + 40 = a_locked_before − 2060.
        let expected = a_balance_before + a_locked_before - Tenths(2060);
        assert_eq!(a_after.usdc_balance, expected);
        assert_conservation(&e, &addrs);
    }

    #[test]
    fn market_order_fills_and_kills_remainder() {
        let (mut e, addrs) = engine_with(2);
        let (a, b) = (addrs[0], addrs[1]);
        // A rests 1 ask at 65¢.
        e.place_limit(a, Side::Ask, tick(650), Qty(1), TradeSource::Bot).unwrap();
        // B places market bid for 3 → fills 1, kills 2.
        let out = e.place_market(b, Side::Bid, Qty(3), TradeSource::Bot).unwrap();
        assert_eq!(out.status, OrderStatus::Killed);
        assert_eq!(out.fills.len(), 1);
        assert_eq!(out.fills[0].price, tick(650));
        assert_eq!(out.fills[0].qty, Qty(1));
        assert_eq!(out.resting_qty, Qty::ZERO);
        // B's locked should be fully released (no resting market order).
        assert_eq!(e.account(&b).unwrap().locked, Tenths::ZERO);
        assert_conservation(&e, &addrs);
    }

    #[test]
    fn market_buy_can_spend_full_cash_at_actual_price() {
        let (mut e, addrs) = engine_with(2);
        let (maker, taker) = (addrs[0], addrs[1]);
        e.place_limit(maker, Side::Ask, tick(500), Qty(2_000), TradeSource::Bot)
            .unwrap();

        let out = e.place_market(taker, Side::Bid, Qty(2_000), TradeSource::Bot).unwrap();

        assert_eq!(out.status, OrderStatus::Filled);
        assert_eq!(out.fills.iter().map(|f| f.qty.0).sum::<u32>(), 2_000);
        assert_eq!(e.account(&taker).unwrap().usdc_balance, Tenths::ZERO);
        assert_eq!(e.account(&taker).unwrap().locked, Tenths::ZERO);
        assert_eq!(e.position(&taker).position, Position(2_000));
        assert_conservation(&e, &addrs);
    }

    #[test]
    fn market_sell_can_close_full_long_with_low_cash() {
        let (mut e, addrs) = engine_with(3);
        let (ask_maker, bid_maker, user) = (addrs[0], addrs[1], addrs[2]);

        e.place_limit(ask_maker, Side::Ask, tick(900), Qty(1_000), TradeSource::Bot)
            .unwrap();
        e.place_market(user, Side::Bid, Qty(1_000), TradeSource::Bot).unwrap();
        assert_eq!(e.position(&user).position, Position(1_000));
        assert_eq!(e.account(&user).unwrap().usdc_balance, Tenths(100_000));

        e.place_limit(bid_maker, Side::Bid, tick(900), Qty(1_000), TradeSource::Bot)
            .unwrap();
        let out = e.place_market(user, Side::Ask, Qty(1_000), TradeSource::Bot).unwrap();

        assert_eq!(out.status, OrderStatus::Filled);
        assert_eq!(e.position(&user).position, Position::FLAT);
        assert_eq!(e.account(&user).unwrap().usdc_balance, START);
        assert_eq!(e.account(&user).unwrap().locked, Tenths::ZERO);
        assert_conservation(&e, &addrs);
    }

    #[test]
    fn market_buy_can_close_full_short_with_low_cash() {
        let (mut e, addrs) = engine_with(3);
        let (bid_maker, ask_maker, user) = (addrs[0], addrs[1], addrs[2]);

        e.place_limit(bid_maker, Side::Bid, tick(100), Qty(1_000), TradeSource::Bot)
            .unwrap();
        e.place_market(user, Side::Ask, Qty(1_000), TradeSource::Bot).unwrap();
        assert_eq!(e.position(&user).position, Position(-1_000));
        assert_eq!(e.account(&user).unwrap().usdc_balance, Tenths(100_000));

        e.place_limit(ask_maker, Side::Ask, tick(100), Qty(1_000), TradeSource::Bot)
            .unwrap();
        let out = e.place_market(user, Side::Bid, Qty(1_000), TradeSource::Bot).unwrap();

        assert_eq!(out.status, OrderStatus::Filled);
        assert_eq!(e.position(&user).position, Position::FLAT);
        assert_eq!(e.account(&user).unwrap().usdc_balance, START);
        assert_eq!(e.account(&user).unwrap().locked, Tenths::ZERO);
        assert_conservation(&e, &addrs);
    }

    #[test]
    fn market_order_over_balance_fills_max_affordable_qty() {
        let (mut e, addrs) = engine_with(2);
        let (maker, taker) = (addrs[0], addrs[1]);
        e.seed_bot_account(maker, Tenths(2_000_000), "maker".into(), None, None);
        e.place_limit(maker, Side::Ask, tick(500), Qty(2_001), TradeSource::Bot)
            .unwrap();

        let out = e.place_market(taker, Side::Bid, Qty(2_001), TradeSource::Bot).unwrap();

        assert_eq!(out.status, OrderStatus::Filled);
        assert_eq!(out.fills.iter().map(|f| f.qty.0).sum::<u32>(), 2_000);
        assert_eq!(e.account(&taker).unwrap().usdc_balance, Tenths::ZERO);
        assert_eq!(e.position(&taker).position, Position(2_000));
    }

    #[test]
    fn market_on_empty_book_returns_no_liquidity() {
        let (mut e, addrs) = engine_with(1);
        let a = addrs[0];
        let bal_before = e.account(&a).unwrap().usdc_balance;
        let err = e.place_market(a, Side::Bid, Qty(1), TradeSource::Bot).unwrap_err();
        assert_eq!(err, OrderError::NoLiquidity);
        // Lock fully refunded.
        assert_eq!(e.account(&a).unwrap().usdc_balance, bal_before);
        assert_eq!(e.account(&a).unwrap().locked, Tenths::ZERO);
    }

    #[test]
    fn market_zero_qty_errors() {
        let (mut e, addrs) = engine_with(1);
        assert_eq!(
            e.place_market(addrs[0], Side::Bid, Qty(0), TradeSource::Bot),
            Err(OrderError::ZeroQuantity),
        );
    }

    #[test]
    fn cash_conservation_under_churn() {
        let (mut e, addrs) = engine_with(3);
        let (a, b, c) = (addrs[0], addrs[1], addrs[2]);
        // A bunch of mixed activity.
        e.place_limit(a, Side::Ask, tick(550), Qty(4), TradeSource::Bot).unwrap();
        e.place_limit(b, Side::Bid, tick(600), Qty(2), TradeSource::Bot).unwrap();
        e.place_limit(c, Side::Bid, tick(700), Qty(3), TradeSource::Bot).unwrap();
        let cancel = e
            .place_limit(a, Side::Bid, tick(400), Qty(5), TradeSource::Bot)
            .unwrap()
            .order_id;
        e.cancel(a, cancel).unwrap();
        e.place_limit(b, Side::Ask, tick(800), Qty(1), TradeSource::Bot).unwrap();
        let _ = e.place_market(c, Side::Ask, Qty(1), TradeSource::Bot);
        assert_conservation(&e, &addrs);
    }

    #[test]
    fn recent_trades_returns_newest_first() {
        let (mut e, addrs) = engine_with(2);
        let (a, b) = (addrs[0], addrs[1]);
        e.place_limit(a, Side::Ask, tick(600), Qty(1), TradeSource::Bot).unwrap();
        e.place_limit(b, Side::Bid, tick(700), Qty(1), TradeSource::Bot).unwrap();
        e.place_limit(a, Side::Ask, tick(650), Qty(1), TradeSource::Bot).unwrap();
        e.place_limit(b, Side::Bid, tick(700), Qty(1), TradeSource::Bot).unwrap();
        let trades: Vec<_> = e.recent_trades(10).collect();
        assert_eq!(trades.len(), 2);
        // Newest first: the 650 fill is more recent than the 600 fill.
        assert_eq!(trades[0].price, tick(650));
        assert_eq!(trades[1].price, tick(600));
    }

    #[test]
    fn recent_trades_caps_at_buffer_size() {
        let (mut e, addrs) = engine_with(2);
        let (a, b) = (addrs[0], addrs[1]);
        // Generate slightly more than the cap by repeated Opens.
        let cap = RECENT_FILLS_CAP;
        for _ in 0..(cap + 5) {
            e.place_limit(a, Side::Ask, tick(500), Qty(1), TradeSource::Bot).unwrap();
            e.place_limit(b, Side::Bid, tick(700), Qty(1), TradeSource::Bot).unwrap();
        }
        let count = e.recent_trades(usize::MAX).count();
        assert_eq!(count, cap, "buffer should be exactly at cap");
    }

    #[test]
    fn recent_trades_limit_truncates_response() {
        let (mut e, addrs) = engine_with(2);
        let (a, b) = (addrs[0], addrs[1]);
        for _ in 0..5 {
            e.place_limit(a, Side::Ask, tick(500), Qty(1), TradeSource::Bot).unwrap();
            e.place_limit(b, Side::Bid, tick(700), Qty(1), TradeSource::Bot).unwrap();
        }
        assert_eq!(e.recent_trades(3).count(), 3);
        assert_eq!(e.recent_trades(0).count(), 0);
    }

    #[test]
    fn cancel_does_not_disturb_balance_after_partial_fills() {
        let (mut e, addrs) = engine_with(2);
        let (a, b) = (addrs[0], addrs[1]);
        // A rests an ask 10 @ 65¢.
        let a_order = e
            .place_limit(a, Side::Ask, tick(650), Qty(10), TradeSource::Bot)
            .unwrap()
            .order_id;
        // B takes 3 of them.
        e.place_limit(b, Side::Bid, tick(700), Qty(3), TradeSource::Bot).unwrap();
        // A still has 7 resting; cancel.
        e.cancel(a, a_order).unwrap();
        // A's locked should be 0 (no resting order); position −3 (sold 3).
        assert_eq!(e.position(&a).position, Position(-3));
        assert_eq!(e.account(&a).unwrap().locked, Tenths::ZERO);
        assert!(e.book().is_empty());
        assert_conservation(&e, &addrs);
    }

    // Suppress unused-variable warning for unused `C` constant in some configs.
    #[allow(dead_code)]
    const _SUPPRESS: &str = C;

    // ─────────────────────────── market lifecycle ────────────────────────────

    #[test]
    fn open_first_market_sets_current() {
        let mut e = Engine::new(START);
        assert!(e.current_market().is_none());
        e.open_first_market(100_000.0, 1_700_000_000, 60);
        let m = e.current_market().unwrap();
        assert_eq!(m.id, MarketId(1));
        assert_eq!(m.strike_btc_usd(), Some(100_000.0));
        assert_eq!(m.opened_at, 1_700_000_000);
        assert_eq!(m.closes_at, 1_700_000_060);
        assert_eq!(m.status, MarketStatus::Open);
    }

    #[test]
    fn resolve_yes_when_close_above_strike() {
        let mut e = Engine::new(START);
        e.open_first_market(100_000.0, 0, 60);
        let report = e.resolve_and_open_next(100_500.0, 100_500.0, 60, 60);
        assert_eq!(report.outcome, Outcome::Yes);
        assert_eq!(report.resolved_id, MarketId(1));
        assert_eq!(report.new_market_id, MarketId(2));
        let history: Vec<_> = e.resolved_markets(10).collect();
        assert_eq!(history.len(), 1);
        assert!(matches!(
            history[0].status,
            MarketStatus::Resolved {
                outcome: Outcome::Yes,
                close: RoundClose::Btc { close_price_btc_usd: p },
            } if p == 100_500.0,
        ));
        assert_eq!(e.current_market().unwrap().strike_btc_usd(), Some(100_500.0));
    }

    #[test]
    fn resolve_no_when_close_below_or_equal_strike() {
        let mut e = Engine::new(START);
        e.open_first_market(100_000.0, 0, 60);
        // Strictly below.
        let r = e.resolve_and_open_next(99_500.0, 99_500.0, 60, 60);
        assert_eq!(r.outcome, Outcome::No);
        // Tie → No (conservative).
        let r2 = e.resolve_and_open_next(99_500.0, 99_500.0, 120, 60);
        assert_eq!(r2.outcome, Outcome::No);
    }

    #[test]
    fn resolution_cancels_all_resting_and_refunds_locks() {
        let (mut e, addrs) = engine_with(3);
        let (a, b, c) = (addrs[0], addrs[1], addrs[2]);
        e.open_first_market(100_000.0, 0, 60);

        // Three resting orders across two accounts.
        e.place_limit(a, Side::Bid, tick(550), Qty(2), TradeSource::Bot).unwrap();
        e.place_limit(a, Side::Ask, tick(750), Qty(1), TradeSource::Bot).unwrap();
        e.place_limit(b, Side::Bid, tick(540), Qty(4), TradeSource::Bot).unwrap();
        // C has no orders — should stay untouched.
        let c_balance_before = e.account(&c).unwrap().usdc_balance;

        assert_ne!(e.account(&a).unwrap().locked, Tenths::ZERO);
        assert_ne!(e.account(&b).unwrap().locked, Tenths::ZERO);

        let report = e.resolve_and_open_next(100_500.0, 100_500.0, 60, 60);
        assert_eq!(report.cancelled_orders, 3);
        assert!(e.book().is_empty());
        assert_eq!(e.account(&a).unwrap().locked, Tenths::ZERO);
        assert_eq!(e.account(&a).unwrap().usdc_balance, START);
        assert_eq!(e.account(&b).unwrap().locked, Tenths::ZERO);
        assert_eq!(e.account(&b).unwrap().usdc_balance, START);
        assert_eq!(e.account(&c).unwrap().usdc_balance, c_balance_before);
    }

    #[test]
    fn market_ids_are_monotonic_across_rounds() {
        let mut e = Engine::new(START);
        e.open_first_market(100_000.0, 0, 60);
        assert_eq!(e.current_market().unwrap().id, MarketId(1));
        e.resolve_and_open_next(100_500.0, 100_500.0, 60, 60);
        assert_eq!(e.current_market().unwrap().id, MarketId(2));
        e.resolve_and_open_next(100_400.0, 100_400.0, 120, 60);
        assert_eq!(e.current_market().unwrap().id, MarketId(3));
    }

    #[test]
    fn resolved_history_is_capped() {
        let mut e = Engine::new(START);
        e.open_first_market(100_000.0, 0, 60);
        for i in 0..(RESOLVED_MARKETS_CAP + 5) {
            let t = 60 + (i as u64) * 60;
            e.resolve_and_open_next(100_000.0 + i as f64, 100_000.0, t, 60);
        }
        let count = e.resolved_markets(usize::MAX).count();
        assert_eq!(count, RESOLVED_MARKETS_CAP);
    }

    #[test]
    fn leaderboard_snapshots_cumulative_pnl_per_participated_market() {
        let mut e = Engine::new(START);
        let user = addr(A);
        let bot = addr(B);
        e.get_or_create_account(user);
        e.get_or_create_account(bot);
        e.register_user(user); // `bot` stays unregistered → excluded.
        e.open_first_market(50_000.0, 0, 60);

        let standing = |e: &Engine, who: Address| {
            e.leaderboard_standings()
                .find(|(a, _)| **a == who)
                .map(|(_, s)| *s)
        };

        // ── Round 1 (market 1): user buys 5 YES @ 50¢; resolves YES → +$2.50.
        e.place_limit(bot, Side::Ask, tick(500), Qty(5), TradeSource::Bot).unwrap();
        e.place_market(user, Side::Bid, Qty(5), TradeSource::Ui).unwrap();
        e.resolve_and_open_next(50_500.0, 50_500.0, 60, 60); // YES → market 2 @ 50_500

        let st = standing(&e, user).expect("standing exists after participating");
        assert_eq!(st.cumulative_pnl_tenths, 2500);
        assert_eq!(st.last_market_pnl_tenths, 2500);
        assert_eq!(st.markets_played, 1);
        assert_eq!(st.markets_won, 1);
        assert_eq!(st.last_market_id, 1);
        // Bots never earn a standing, regardless of their activity.
        assert!(standing(&e, bot).is_none());

        // ── Round 2 (market 2): user sits out. Standing must freeze (Codex P2).
        e.resolve_and_open_next(50_000.0, 50_000.0, 120, 60); // NO → market 3 @ 50_000
        let st = standing(&e, user).unwrap();
        assert_eq!(st.markets_played, 1, "a skipped round must not count as played");
        assert_eq!(st.last_market_id, 1, "standing frozen across a skipped round");
        assert_eq!(st.cumulative_pnl_tenths, 2500);

        // ── Round 3 (market 3 @ 50_000): user buys 5 YES @ 50¢; resolves NO → −$2.50.
        e.place_limit(bot, Side::Ask, tick(500), Qty(5), TradeSource::Bot).unwrap();
        e.place_market(user, Side::Bid, Qty(5), TradeSource::Ui).unwrap();
        e.resolve_and_open_next(49_500.0, 49_500.0, 180, 60); // NO

        let st = standing(&e, user).unwrap();
        assert_eq!(st.cumulative_pnl_tenths, 0, "round-3 loss erases the round-1 gain");
        assert_eq!(st.last_market_pnl_tenths, -2500);
        assert_eq!(st.markets_played, 2);
        assert_eq!(st.markets_won, 1, "a losing round does not add a win");
        assert_eq!(st.last_market_id, 3);
    }

    #[test]
    #[should_panic(expected = "no market open")]
    fn resolve_without_open_panics() {
        let mut e = Engine::new(START);
        e.resolve_and_open_next(100_000.0, 100_500.0, 60, 60);
    }

    #[test]
    #[should_panic(expected = "already open")]
    fn open_first_twice_panics() {
        let mut e = Engine::new(START);
        e.open_first_market(100_000.0, 0, 60);
        e.open_first_market(100_000.0, 0, 60);
    }

    // ───────────────────── Settlement / resolution tests ─────────────────────

    #[test]
    fn user_long_yes_wins_gets_payout() {
        let mut e = Engine::new(START);
        let user = addr(A);
        let bot = addr(B);
        e.register_user(user);
        e.get_or_create_account(user);
        e.get_or_create_account(bot);
        e.open_first_market(100_000.0, 0, 60);

        // User buys 5 YES at 600 tenths from bot's ask.
        e.place_limit(bot, Side::Ask, tick(600), Qty(5), TradeSource::Bot).unwrap();
        e.place_limit(user, Side::Bid, tick(600), Qty(5), TradeSource::Bot).unwrap();

        assert_eq!(e.position(&user).position, Position(5));
        assert_eq!(e.position(&bot).position, Position(-5));

        // Price went up → YES wins.
        let report = e.resolve_and_open_next(100_500.0, 100_500.0, 60, 60);
        assert_eq!(report.outcome, Outcome::Yes);

        let user_acct = e.account(&user).unwrap();
        let user_acct_ps = e.position(&user);
        // User paid 5 * 600 = 3000 tenths, then receives 5 * 1000 = 5000 tenths.
        // Net balance = START - 3000 + 5000 = START + 2000.
        assert_eq!(user_acct.usdc_balance, Tenths(START.0 + 2000));
        assert_eq!(user_acct_ps.position, Position::FLAT);
        assert_eq!(user_acct.locked, Tenths::ZERO);

        // Bot resets to starting balance.
        let bot_acct = e.account(&bot).unwrap();
        let bot_acct_ps = e.position(&bot);
        assert_eq!(bot_acct.usdc_balance, START);
        assert_eq!(bot_acct_ps.position, Position::FLAT);
    }

    #[test]
    fn user_long_yes_loses_gets_nothing() {
        let mut e = Engine::new(START);
        let user = addr(A);
        let bot = addr(B);
        e.register_user(user);
        e.get_or_create_account(user);
        e.get_or_create_account(bot);
        e.open_first_market(100_000.0, 0, 60);

        e.place_limit(bot, Side::Ask, tick(700), Qty(3), TradeSource::Bot).unwrap();
        e.place_limit(user, Side::Bid, tick(700), Qty(3), TradeSource::Bot).unwrap();

        assert_eq!(e.position(&user).position, Position(3));

        // Price went down → NO wins. User loses.
        let report = e.resolve_and_open_next(99_500.0, 99_500.0, 60, 60);
        assert_eq!(report.outcome, Outcome::No);

        let user_acct = e.account(&user).unwrap();
        let user_acct_ps = e.position(&user);
        // User paid 3 * 700 = 2100, gets no payout. Net = START - 2100.
        assert_eq!(user_acct.usdc_balance, Tenths(START.0 - 2100));
        assert_eq!(user_acct_ps.position, Position::FLAT);
    }

    #[test]
    fn user_short_yes_wins_on_no_resolution() {
        let mut e = Engine::new(START);
        let user = addr(A);
        let bot = addr(B);
        e.register_user(user);
        e.get_or_create_account(user);
        e.get_or_create_account(bot);
        e.open_first_market(100_000.0, 0, 60);

        // User sells YES (goes short) at 800 tenths.
        e.place_limit(bot, Side::Bid, tick(800), Qty(4), TradeSource::Bot).unwrap();
        e.place_limit(user, Side::Ask, tick(800), Qty(4), TradeSource::Bot).unwrap();

        assert_eq!(e.position(&user).position, Position(-4));

        // NO wins → short YES wins.
        let report = e.resolve_and_open_next(99_000.0, 99_000.0, 60, 60);
        assert_eq!(report.outcome, Outcome::No);

        let user_acct = e.account(&user).unwrap();
        let user_acct_ps = e.position(&user);
        // User locked 4 * (1000-800) = 800 tenths, receives 4 * 1000 = 4000.
        // Net = START - 800 + 4000 = START + 3200.
        assert_eq!(user_acct.usdc_balance, Tenths(START.0 + 3200));
        assert_eq!(user_acct_ps.position, Position::FLAT);
    }

    #[test]
    fn user_short_yes_loses_on_yes_resolution() {
        let mut e = Engine::new(START);
        let user = addr(A);
        let bot = addr(B);
        e.register_user(user);
        e.get_or_create_account(user);
        e.get_or_create_account(bot);
        e.open_first_market(100_000.0, 0, 60);

        e.place_limit(bot, Side::Bid, tick(800), Qty(4), TradeSource::Bot).unwrap();
        e.place_limit(user, Side::Ask, tick(800), Qty(4), TradeSource::Bot).unwrap();

        assert_eq!(e.position(&user).position, Position(-4));

        // YES wins → short loses.
        let report = e.resolve_and_open_next(100_500.0, 100_500.0, 60, 60);
        assert_eq!(report.outcome, Outcome::Yes);

        let user_acct = e.account(&user).unwrap();
        let user_acct_ps = e.position(&user);
        // User locked 4 * 200 = 800, gets nothing. Net = START - 800.
        assert_eq!(user_acct.usdc_balance, Tenths(START.0 - 800));
        assert_eq!(user_acct_ps.position, Position::FLAT);
    }

    #[test]
    fn bot_accounts_reset_balance_on_resolution() {
        let mut e = Engine::new(START);
        let bot1 = addr(A);
        let bot2 = addr(B);
        e.get_or_create_account(bot1);
        e.get_or_create_account(bot2);
        e.open_first_market(100_000.0, 0, 60);

        // Bots trade with each other.
        e.place_limit(bot1, Side::Bid, tick(500), Qty(10), TradeSource::Bot).unwrap();
        e.place_limit(bot2, Side::Ask, tick(500), Qty(10), TradeSource::Bot).unwrap();

        assert_ne!(e.account(&bot1).unwrap().usdc_balance, START);
        assert_ne!(e.account(&bot2).unwrap().usdc_balance, START);

        e.resolve_and_open_next(100_500.0, 100_500.0, 60, 60);

        assert_eq!(e.account(&bot1).unwrap().usdc_balance, START);
        assert_eq!(e.account(&bot2).unwrap().usdc_balance, START);
        assert_eq!(e.position(&bot1).position, Position::FLAT);
        assert_eq!(e.position(&bot2).position, Position::FLAT);
    }

    #[test]
    fn oi_is_zero_after_resolution() {
        let mut e = Engine::new(START);
        let a = addr(A);
        let b = addr(B);
        e.get_or_create_account(a);
        e.get_or_create_account(b);
        e.open_first_market(100_000.0, 0, 60);

        e.place_limit(a, Side::Bid, tick(500), Qty(10), TradeSource::Bot).unwrap();
        e.place_limit(b, Side::Ask, tick(500), Qty(10), TradeSource::Bot).unwrap();

        assert_eq!(e.open_interest(), 10);

        e.resolve_and_open_next(100_500.0, 100_500.0, 60, 60);

        assert_eq!(e.open_interest(), 0);
    }

    #[test]
    fn user_vs_user_settlement_is_zero_sum() {
        let mut e = Engine::new(START);
        let user_a = addr(A);
        let user_b = addr(B);
        e.register_user(user_a);
        e.register_user(user_b);
        e.get_or_create_account(user_a);
        e.get_or_create_account(user_b);
        e.open_first_market(100_000.0, 0, 60);

        // A buys YES from B at 600.
        e.place_limit(user_b, Side::Ask, tick(600), Qty(8), TradeSource::Bot).unwrap();
        e.place_limit(user_a, Side::Bid, tick(600), Qty(8), TradeSource::Bot).unwrap();

        let total_before = e.account(&user_a).unwrap().usdc_balance.0
            + e.account(&user_a).unwrap().locked.0
            + e.account(&user_b).unwrap().usdc_balance.0
            + e.account(&user_b).unwrap().locked.0
            + (e.open_interest() as i64) * PAYOUT_PER_CONTRACT;
        assert_eq!(total_before, 2 * START.0);

        e.resolve_and_open_next(100_500.0, 100_500.0, 60, 60);

        // After resolution: OI=0, locked=0 for both, positions flat.
        let total_after =
            e.account(&user_a).unwrap().usdc_balance.0 + e.account(&user_b).unwrap().usdc_balance.0;
        assert_eq!(total_after, 2 * START.0, "user-vs-user should be zero-sum");
    }

    #[test]
    fn flat_position_user_unaffected_by_resolution() {
        let mut e = Engine::new(START);
        let user = addr(A);
        e.register_user(user);
        e.get_or_create_account(user);
        e.open_first_market(100_000.0, 0, 60);

        // User does nothing, just sits flat.
        e.resolve_and_open_next(100_500.0, 100_500.0, 60, 60);

        let acct = e.account(&user).unwrap();
        let acct_ps = e.position(&user);
        assert_eq!(acct.usdc_balance, START);
        assert_eq!(acct_ps.position, Position::FLAT);
    }

    #[test]
    fn user_balance_rolls_over_across_rounds() {
        let mut e = Engine::new(START);
        let user = addr(A);
        let bot = addr(B);
        e.register_user(user);
        e.get_or_create_account(user);
        e.get_or_create_account(bot);
        e.open_first_market(100_000.0, 0, 60);

        // Round 1: user goes long 5 at 400, YES wins.
        e.place_limit(bot, Side::Ask, tick(400), Qty(5), TradeSource::Bot).unwrap();
        e.place_limit(user, Side::Bid, tick(400), Qty(5), TradeSource::Bot).unwrap();
        e.resolve_and_open_next(100_500.0, 100_500.0, 60, 60);

        // Profit = 5 * (1000-400) = 3000.
        let after_r1 = e.account(&user).unwrap().usdc_balance;
        assert_eq!(after_r1, Tenths(START.0 + 3000));

        // Round 2: user goes long 5 at 400 again, YES wins again.
        e.place_limit(bot, Side::Ask, tick(400), Qty(5), TradeSource::Bot).unwrap();
        e.place_limit(user, Side::Bid, tick(400), Qty(5), TradeSource::Bot).unwrap();
        e.resolve_and_open_next(100_600.0, 100_600.0, 120, 60);

        // Profit compounds on top of round 1 winnings.
        let after_r2 = e.account(&user).unwrap().usdc_balance;
        assert_eq!(after_r2, Tenths(after_r1.0 + 3000));
    }

    // ──────────────── Avg entry / realized PnL accounting ────────────────

    #[test]
    fn two_opens_at_different_prices_give_vwap_avg() {
        let (mut e, addrs) = engine_with(2);
        let (maker, taker) = (addrs[0], addrs[1]);
        // Maker rests two ask levels: 5@600 and 5@700.
        e.place_limit(maker, Side::Ask, tick(600), Qty(5), TradeSource::Bot).unwrap();
        e.place_limit(maker, Side::Ask, tick(700), Qty(5), TradeSource::Bot).unwrap();
        // Taker bids 10 at 700 — sweeps both levels.
        e.place_limit(taker, Side::Bid, tick(700), Qty(10), TradeSource::Bot).unwrap();

        let acct_ps = e.position(&taker);
        assert_eq!(acct_ps.position, Position(10));
        // entry_cost = 5×600 + 5×700 = 6500; avg = 650.
        assert_eq!(acct_ps.entry_cost_tenths, Tenths(6500));
        assert_eq!(acct_ps.realized_pnl_this_market, Tenths::ZERO);
    }

    #[test]
    fn partial_close_realizes_pnl_and_keeps_avg() {
        let (mut e, addrs) = engine_with(3);
        let (maker, taker, exit) = (addrs[0], addrs[1], addrs[2]);
        // Open: maker asks 10@600; taker bids 10@600 → taker long 10 @ avg 600.
        e.place_limit(maker, Side::Ask, tick(600), Qty(10), TradeSource::Bot).unwrap();
        e.place_limit(taker, Side::Bid, tick(600), Qty(10), TradeSource::Bot).unwrap();
        assert_eq!(e.position(&taker).entry_cost_tenths, Tenths(6000));

        // Exit-bidder posts a bid at 800 for 3. Taker hits it (sells 3 at 800).
        e.place_limit(exit, Side::Bid, tick(800), Qty(3), TradeSource::Bot).unwrap();
        e.place_limit(taker, Side::Ask, tick(800), Qty(3), TradeSource::Bot).unwrap();

        let acct_ps = e.position(&taker);
        assert_eq!(acct_ps.position, Position(7));
        // qty_closed=3 at avg 600. entry_cost_released = 3×600 = 1800.
        // entry_cost remaining = 6000 − 1800 = 4200 → avg 4200/7 = 600. ✓
        assert_eq!(acct_ps.entry_cost_tenths, Tenths(4200));
        // realized = +(800−600)×3 = +600.
        assert_eq!(acct_ps.realized_pnl_this_market, Tenths(600));
    }

    #[test]
    fn full_close_to_flat_zeros_entry_cost() {
        let (mut e, addrs) = engine_with(3);
        let (maker, taker, exit) = (addrs[0], addrs[1], addrs[2]);
        e.place_limit(maker, Side::Ask, tick(600), Qty(5), TradeSource::Bot).unwrap();
        e.place_limit(taker, Side::Bid, tick(600), Qty(5), TradeSource::Bot).unwrap();

        e.place_limit(exit, Side::Bid, tick(700), Qty(5), TradeSource::Bot).unwrap();
        e.place_limit(taker, Side::Ask, tick(700), Qty(5), TradeSource::Bot).unwrap();

        let acct_ps = e.position(&taker);
        assert_eq!(acct_ps.position, Position::FLAT);
        assert_eq!(acct_ps.entry_cost_tenths, Tenths::ZERO);
        assert_eq!(acct_ps.realized_pnl_this_market, Tenths(500));
    }

    #[test]
    fn flip_in_single_fill_re_anchors_entry_cost() {
        let (mut e, addrs) = engine_with(3);
        let (maker, taker, flip_maker) = (addrs[0], addrs[1], addrs[2]);
        // Build taker long 5 @ 600.
        e.place_limit(maker, Side::Ask, tick(600), Qty(5), TradeSource::Bot).unwrap();
        e.place_limit(taker, Side::Bid, tick(600), Qty(5), TradeSource::Bot).unwrap();

        // flip_maker posts a bid 10@500. Taker sells 10 at 500 → flips +5 to −5.
        e.place_limit(flip_maker, Side::Bid, tick(500), Qty(10), TradeSource::Bot)
            .unwrap();
        e.place_limit(taker, Side::Ask, tick(500), Qty(10), TradeSource::Bot).unwrap();

        let acct_ps = e.position(&taker);
        assert_eq!(acct_ps.position, Position(-5));
        // Close leg of 5 long @ 600 covered at 500 → realized = (500−600)×5 = −500.
        assert_eq!(acct_ps.realized_pnl_this_market, Tenths(-500));
        // New short 5 anchored at fill price 500 → entry_cost = 500×5 = 2500.
        assert_eq!(acct_ps.entry_cost_tenths, Tenths(2500));
    }

    #[test]
    fn short_open_then_cover_mirrors_long_path() {
        let (mut e, addrs) = engine_with(3);
        let (bid_maker, taker, ask_maker) = (addrs[0], addrs[1], addrs[2]);
        // Taker sells 5 at 700 (goes short 5). YES-side avg entry = 700.
        e.place_limit(bid_maker, Side::Bid, tick(700), Qty(5), TradeSource::Bot)
            .unwrap();
        e.place_limit(taker, Side::Ask, tick(700), Qty(5), TradeSource::Bot).unwrap();
        let after_open_ps = e.position(&taker);
        assert_eq!(after_open_ps.position, Position(-5));
        assert_eq!(after_open_ps.entry_cost_tenths, Tenths(3500));

        // Cover at 500.
        e.place_limit(ask_maker, Side::Ask, tick(500), Qty(5), TradeSource::Bot)
            .unwrap();
        e.place_limit(taker, Side::Bid, tick(500), Qty(5), TradeSource::Bot).unwrap();

        let acct_ps = e.position(&taker);
        assert_eq!(acct_ps.position, Position::FLAT);
        assert_eq!(acct_ps.entry_cost_tenths, Tenths::ZERO);
        // Short profited (700 − 500)×5 = +1000.
        assert_eq!(acct_ps.realized_pnl_this_market, Tenths(1000));
    }

    /// Registered user + bot with an open market, so user order rows are
    /// recorded.
    #[test]
    fn position_defaults_flat_for_unknown_address() {
        let e = Engine::new(START);
        let ps = e.position(&addr(A));
        assert_eq!(ps, PositionState::default());
        assert!(ps.is_flat());
        assert_eq!(e.positions_iter().count(), 0);
    }

    #[test]
    fn settle_clears_position_map() {
        let (mut e, user, bot) = engine_with_user_market();
        e.place_limit(bot, Side::Ask, tick(400), Qty(5), TradeSource::Bot).unwrap();
        e.place_market(user, Side::Bid, Qty(5), TradeSource::Ui).unwrap();
        assert_eq!(e.positions_iter().count(), 2);

        e.resolve_and_open_next(100_500.0, 100_500.0, 60, 60);

        // Bot entry dropped outright; the user's stays only as the
        // carrier of `last_market`, flat.
        let remaining: Vec<_> = e.positions_iter().collect();
        assert_eq!(remaining.len(), 1);
        let (who, ps) = remaining[0];
        assert_eq!(*who, user);
        assert!(ps.is_flat());
        assert_eq!(ps.entry_cost_tenths, Tenths::ZERO);
        assert_eq!(ps.realized_pnl_this_market, Tenths::ZERO);
        assert!(ps.last_market.is_some());
        assert_eq!(e.position(&bot), PositionState::default());
    }

    // ───────────────────────── Exchange: two series ─────────────────────────

    const S0: SeriesId = SeriesId(0);
    const S1: SeriesId = SeriesId(1);

    /// Two-series exchange with a human main account (no home) and one
    /// bot per series, both markets open.
    fn two_series_exchange() -> (Exchange, Address, Address, Address) {
        let mut x = Exchange::new(START, crate::series::SERIES);
        let (user, bot0, bot1) = (addr(A), addr(B), addr(C));
        x.register_user(user);
        x.get_or_create_account(user);
        x.seed_bot_account_in(bot0, START, "bot0".into(), None, None, S0);
        x.seed_bot_account_in(bot1, START, "bot1".into(), None, None, S1);
        x.open_first_market(S0, 100_000.0, 0, 60);
        x.open_first_market(S1, 100_000.0, 0, 900);
        (x, user, bot0, bot1)
    }

    #[test]
    fn exchange_two_series_independent_books() {
        let (mut x, user, bot0, _) = two_series_exchange();
        x.place_limit(S0, bot0, Side::Ask, tick(500), Qty(5), TradeSource::Bot).unwrap();
        x.place_limit(S0, user, Side::Bid, tick(400), Qty(2), TradeSource::Ui).unwrap();
        assert_eq!(x.engine(S0).book().iter_asks().count(), 1);
        assert_eq!(x.engine(S0).book().iter_bids().count(), 1);
        assert!(x.engine(S1).book().is_empty());
        // A market order on series 1 finds nothing to hit.
        assert_eq!(
            x.place_market(S1, user, Side::Bid, Qty(1), TradeSource::Ui),
            Err(OrderError::NoLiquidity)
        );
        x.debug_assert_invariants();
    }

    #[test]
    fn cancel_any_probes_other_series() {
        let (mut x, user, _, _) = two_series_exchange();
        let out = x
            .place_limit(S1, user, Side::Bid, tick(400), Qty(2), TradeSource::Ui)
            .unwrap();
        // Preferred series is wrong; the probe finds it in series 1.
        let (found_in, order) = x.cancel_any(S0, user, out.order_id).unwrap();
        assert_eq!(found_in, S1);
        assert_eq!(order.id, out.order_id);
        assert_eq!(x.account(&user).unwrap().locked, Tenths::ZERO);
        assert_eq!(x.cancel_any(S0, user, out.order_id), Err(CancelError::UnknownOrder));
        x.debug_assert_invariants();
    }

    #[test]
    fn settle_series0_leaves_series1_positions_and_oi_intact() {
        let (mut x, user, bot0, bot1) = two_series_exchange();
        // Position in both series plus a resting order in series 1.
        x.place_limit(S0, bot0, Side::Ask, tick(600), Qty(5), TradeSource::Bot).unwrap();
        x.place_market(S0, user, Side::Bid, Qty(5), TradeSource::Ui).unwrap();
        x.place_limit(S1, bot1, Side::Ask, tick(300), Qty(4), TradeSource::Bot).unwrap();
        x.place_market(S1, user, Side::Bid, Qty(4), TradeSource::Ui).unwrap();
        x.place_limit(S1, user, Side::Bid, tick(200), Qty(10), TradeSource::Ui).unwrap();
        let locked_before = x.account(&user).unwrap().locked;
        assert_eq!(locked_before, Tenths(2000));
        x.debug_assert_invariants();

        // Series 0 resolves YES; series 1 is untouched.
        let report = x.resolve_and_open_next(S0, 100_500.0, 100_500.0, 60, 60);
        assert_eq!(report.outcome, Outcome::Yes);
        assert!(x.position(S0, &user).is_flat());
        assert_eq!(x.engine(S0).open_interest(), 0);
        assert_eq!(x.position(S1, &user).position.0, 4);
        assert_eq!(x.engine(S1).open_interest(), 4);
        assert_eq!(x.engine(S1).book().iter_bids().count(), 1);
        // The series-1 reservation survives series-0 settlement.
        assert_eq!(x.account(&user).unwrap().locked, locked_before);
        // Cash: −5×600 + 5×1000 (s0) − 4×300 (s1) − 2000 reserved.
        assert_eq!(
            x.account(&user).unwrap().usdc_balance,
            Tenths(START.0 - 3000 + 5000 - 1200 - 2000)
        );
        let last = x.position(S0, &user).last_market.unwrap();
        assert_eq!(last.series, S0);
        assert_eq!(last.pnl_tenths, 2000);
        assert!(x.position(S1, &user).last_market.is_none());
        x.debug_assert_invariants();
    }

    #[test]
    fn bot_reset_only_at_home_series_settlement() {
        let (mut x, user, bot0, bot1) = two_series_exchange();
        x.place_limit(S0, bot0, Side::Ask, tick(600), Qty(5), TradeSource::Bot).unwrap();
        x.place_market(S0, user, Side::Bid, Qty(5), TradeSource::Ui).unwrap();
        x.place_limit(S1, bot1, Side::Ask, tick(300), Qty(4), TradeSource::Bot).unwrap();
        x.place_market(S1, user, Side::Bid, Qty(4), TradeSource::Ui).unwrap();
        let bot1_cash = x.account(&bot1).unwrap().usdc_balance;
        assert_ne!(bot1_cash, START);

        // Settling series 0 resets bot0 but leaves bot1 (home = series 1)
        // exactly as it was, position included.
        x.resolve_and_open_next(S0, 100_500.0, 100_500.0, 60, 60);
        assert_eq!(x.account(&bot0).unwrap().usdc_balance, START);
        assert_eq!(x.account(&bot1).unwrap().usdc_balance, bot1_cash);
        assert_eq!(x.position(S1, &bot1).position.0, -4);

        x.resolve_and_open_next(S1, 100_500.0, 100_500.0, 900, 900);
        assert_eq!(x.account(&bot1).unwrap().usdc_balance, START);
        assert!(x.position(S1, &bot1).is_flat());
        x.debug_assert_invariants();
    }

    fn engine_with_user_market() -> (Engine, Address, Address) {
        let mut e = Engine::new(START);
        let user = addr(A);
        let bot = addr(B);
        e.register_user(user);
        e.get_or_create_account(user);
        e.get_or_create_account(bot);
        e.open_first_market(100_000.0, 0, 60);
        (e, user, bot)
    }

    fn row_realized(e: &Engine, user: Address, order_id: OrderId) -> i64 {
        e.user_orders(&user)
            .iter()
            .find(|r| r.order_id == order_id)
            .expect("row recorded")
            .realized_pnl_tenths
    }

    #[test]
    fn taker_partial_close_realized_lands_on_closing_row_only() {
        let (mut e, user, bot) = engine_with_user_market();
        e.place_limit(bot, Side::Ask, tick(600), Qty(10), TradeSource::Bot).unwrap();
        let open = e.place_market(user, Side::Bid, Qty(10), TradeSource::Ui).unwrap();
        e.place_limit(bot, Side::Bid, tick(800), Qty(3), TradeSource::Bot).unwrap();
        let close = e.place_market(user, Side::Ask, Qty(3), TradeSource::Ui).unwrap();

        assert_eq!(row_realized(&e, user, open.order_id), 0);
        // (800 − 600) × 3
        assert_eq!(row_realized(&e, user, close.order_id), 600);
        assert_eq!(e.position(&user).realized_pnl_this_market, Tenths(600));
    }

    #[test]
    fn maker_resting_order_accrues_realized_when_hit() {
        let (mut e, user, bot) = engine_with_user_market();
        // User short 5 @ 700 (sells into bot bid).
        e.place_limit(bot, Side::Bid, tick(700), Qty(5), TradeSource::Bot).unwrap();
        e.place_market(user, Side::Ask, Qty(5), TradeSource::Ui).unwrap();
        // User rests a covering bid at 500; bot sells into it in two matches.
        let cover = e.place_limit(user, Side::Bid, tick(500), Qty(5), TradeSource::Ui).unwrap();
        assert_eq!(row_realized(&e, user, cover.order_id), 0);
        e.place_market(bot, Side::Ask, Qty(2), TradeSource::Bot).unwrap();
        // Short covered 2 @ 500 vs entry 700 → +400.
        assert_eq!(row_realized(&e, user, cover.order_id), 400);
        e.place_market(bot, Side::Ask, Qty(3), TradeSource::Bot).unwrap();
        assert_eq!(row_realized(&e, user, cover.order_id), 1000);
        let row = e.user_orders(&user).iter().find(|r| r.order_id == cover.order_id).unwrap();
        assert_eq!(row.status, UserOrderStatus::Filled);
        assert_eq!(row.filled_qty, Qty(5));
    }

    #[test]
    fn flip_attributes_realized_to_close_leg_row() {
        let (mut e, user, bot) = engine_with_user_market();
        e.place_limit(bot, Side::Ask, tick(600), Qty(5), TradeSource::Bot).unwrap();
        e.place_market(user, Side::Bid, Qty(5), TradeSource::Ui).unwrap();
        // Deep bid so both flip legs fill at 500.
        e.place_limit(bot, Side::Bid, tick(500), Qty(50), TradeSource::Bot).unwrap();
        let flip = e.flip_position(user, TradeSource::Ui).unwrap();

        // Long 5 @ 600 closed at 500 → −500 on the close leg; open leg is a
        // fresh short, no realization.
        assert_eq!(row_realized(&e, user, flip.close.order_id), -500);
        assert_eq!(row_realized(&e, user, flip.open.order_id), 0);
        assert_eq!(e.position(&user).realized_pnl_this_market, Tenths(-500));
    }

    #[test]
    fn resolution_freezes_round_pnl_for_every_direction() {
        // (user side, price, outcome close, expected pnl)
        //  long 5 @ 600, YES  → 5 × (1000 − 600) = +2000
        //  long 3 @ 700, NO   → −3 × 700          = −2100
        //  short 4 @ 800, NO  → 4 × 800           = +3200
        //  short 4 @ 800, YES → −4 × (1000 − 800) = −800
        let cases = [
            (Side::Bid, 600u16, 5u32, 100_500.0, 2000i64),
            (Side::Bid, 700, 3, 99_500.0, -2100),
            (Side::Ask, 800, 4, 99_000.0, 3200),
            (Side::Ask, 800, 4, 100_500.0, -800),
        ];
        for (side, price, qty, close, expected) in cases {
            let (mut e, user, bot) = engine_with_user_market();
            e.place_limit(bot, side.opposite(), tick(price), Qty(qty), TradeSource::Bot)
                .unwrap();
            e.place_market(user, side, Qty(qty), TradeSource::Ui).unwrap();
            e.resolve_and_open_next(close, close, 60, 60);
            let acct = e.account(&user).unwrap();
            let acct_ps = e.position(&user);
            let last = acct_ps.last_market.expect("frozen at resolve");
            assert_eq!(last.pnl_tenths, expected, "{side:?} {price} close {close}");
            assert_eq!(last.trades, 1);
            assert_eq!(last.market_id, MarketId(1));
            assert_eq!(acct.lifetime_realized_pnl_tenths, expected);
            // Round PnL equals the cash the account actually gained.
            assert_eq!(acct.usdc_balance.0 - START.0, expected);
            assert_eq!(acct_ps.realized_pnl_this_market, Tenths::ZERO);
        }
    }

    #[test]
    fn round_pnl_equals_cash_change_with_partial_close_and_lifetime_accumulates() {
        let (mut e, user, bot) = engine_with_user_market();
        // Long 5 @ 400, sell 2 @ 700, hold 3 into a YES resolution.
        e.place_limit(bot, Side::Ask, tick(400), Qty(5), TradeSource::Bot).unwrap();
        e.place_market(user, Side::Bid, Qty(5), TradeSource::Ui).unwrap();
        e.place_limit(bot, Side::Bid, tick(700), Qty(2), TradeSource::Bot).unwrap();
        e.place_market(user, Side::Ask, Qty(2), TradeSource::Ui).unwrap();
        e.resolve_and_open_next(100_500.0, 100_500.0, 60, 60);

        let acct = e.account(&user).unwrap();
        let acct_ps = e.position(&user);
        // 2 × (700 − 400) + 3 × (1000 − 400) = 600 + 1800.
        let round1 = acct_ps.last_market.unwrap();
        assert_eq!(round1.pnl_tenths, 2400);
        assert_eq!(round1.trades, 2);
        assert_eq!(round1.outcome, Outcome::Yes);
        assert_eq!(acct.usdc_balance.0 - START.0, 2400);
        let after_round1 = acct.usdc_balance.0;

        // Round 2: short 4 @ 800, NO resolves → +3200.
        e.place_limit(bot, Side::Bid, tick(800), Qty(4), TradeSource::Bot).unwrap();
        e.place_market(user, Side::Ask, Qty(4), TradeSource::Ui).unwrap();
        e.resolve_and_open_next(99_000.0, 99_000.0, 120, 60);

        let acct = e.account(&user).unwrap();
        let acct_ps = e.position(&user);
        let round2 = acct_ps.last_market.unwrap();
        assert_eq!(round2.market_id, MarketId(2));
        assert_eq!(round2.pnl_tenths, 3200);
        assert_eq!(acct.usdc_balance.0 - after_round1, 3200);
        assert_eq!(acct.lifetime_realized_pnl_tenths, 5600);

        // Round 3: no activity → zero result, lifetime unchanged, bots untouched.
        e.resolve_and_open_next(99_000.0, 99_000.0, 180, 60);
        let acct = e.account(&user).unwrap();
        let acct_ps = e.position(&user);
        let round3 = acct_ps.last_market.unwrap();
        assert_eq!((round3.pnl_tenths, round3.trades), (0, 0));
        assert_eq!(acct.lifetime_realized_pnl_tenths, 5600);
        assert!(e.position(&bot).last_market.is_none());
    }

    #[test]
    fn resolution_clears_entry_cost_and_realized() {
        let mut e = Engine::new(START);
        let user = addr(A);
        let bot = addr(B);
        e.register_user(user);
        e.get_or_create_account(user);
        e.get_or_create_account(bot);
        e.open_first_market(100_000.0, 0, 60);

        // User goes long 5 at 400, then partially closes 2 at 700 → realized PnL.
        e.place_limit(bot, Side::Ask, tick(400), Qty(5), TradeSource::Bot).unwrap();
        e.place_limit(user, Side::Bid, tick(400), Qty(5), TradeSource::Bot).unwrap();
        e.place_limit(bot, Side::Bid, tick(700), Qty(2), TradeSource::Bot).unwrap();
        e.place_limit(user, Side::Ask, tick(700), Qty(2), TradeSource::Bot).unwrap();

        let before_ps = e.position(&user);
        assert_ne!(before_ps.entry_cost_tenths, Tenths::ZERO);
        assert_ne!(before_ps.realized_pnl_this_market, Tenths::ZERO);

        e.resolve_and_open_next(100_500.0, 100_500.0, 60, 60);

        let after_ps = e.position(&user);
        assert_eq!(after_ps.entry_cost_tenths, Tenths::ZERO);
        assert_eq!(after_ps.realized_pnl_this_market, Tenths::ZERO);
        // Bot should also be zeroed.
        let bot_acct_ps = e.position(&bot);
        assert_eq!(bot_acct_ps.entry_cost_tenths, Tenths::ZERO);
        assert_eq!(bot_acct_ps.realized_pnl_this_market, Tenths::ZERO);
    }

    #[test]
    fn flip_long_yes_to_same_notional_no() {
        let (mut e, addrs) = engine_with(3);
        let (ask_maker, bid_maker, user) = (addrs[0], addrs[1], addrs[2]);

        // User opens $5 of YES: 10 shares at 50c.
        e.place_limit(ask_maker, Side::Ask, tick(500), Qty(10), TradeSource::Bot)
            .unwrap();
        e.place_market(user, Side::Bid, Qty(10), TradeSource::Bot).unwrap();
        assert_eq!(e.position(&user).position, Position(10));

        // Bid liquidity at 50c can close 10 YES and open $5 of NO
        // (10 more sells, each locking the 50c complement).
        e.place_limit(bid_maker, Side::Bid, tick(500), Qty(20), TradeSource::Bot)
            .unwrap();
        let out = e.flip_position(user, TradeSource::Bot).unwrap();

        assert_eq!(out.target_notional_tenths, Tenths(5000));
        assert_eq!(out.close.status, OrderStatus::Filled);
        assert_eq!(out.open.status, OrderStatus::Filled);
        let acct_ps = e.position(&user);
        assert_eq!(acct_ps.position, Position(-10));
        assert_eq!(acct_ps.entry_cost_tenths, Tenths(5000));
        assert_conservation(&e, &addrs);
    }

    #[test]
    fn flip_no_to_same_notional_yes() {
        let (mut e, addrs) = engine_with(3);
        let (bid_maker, ask_maker, user) = (addrs[0], addrs[1], addrs[2]);

        // User opens $5 of NO by selling 10 YES at 50c.
        e.place_limit(bid_maker, Side::Bid, tick(500), Qty(10), TradeSource::Bot)
            .unwrap();
        e.place_market(user, Side::Ask, Qty(10), TradeSource::Bot).unwrap();
        assert_eq!(e.position(&user).position, Position(-10));

        // Ask liquidity at 50c can close the NO and open $5 of YES.
        e.place_limit(ask_maker, Side::Ask, tick(500), Qty(20), TradeSource::Bot)
            .unwrap();
        let out = e.flip_position(user, TradeSource::Bot).unwrap();

        assert_eq!(out.target_notional_tenths, Tenths(5000));
        assert_eq!(out.close.status, OrderStatus::Filled);
        assert_eq!(out.open.status, OrderStatus::Filled);
        let acct_ps = e.position(&user);
        assert_eq!(acct_ps.position, Position(10));
        assert_eq!(acct_ps.entry_cost_tenths, Tenths(5000));
        assert_conservation(&e, &addrs);
    }

    #[test]
    fn flip_caps_open_leg_to_affordable_qty_after_close() {
        let mut e = Engine::new(Tenths(5_000));
        let ask_maker = addr(A);
        let bid_maker = addr(B);
        let user = addr(C);
        e.seed_bot_account(ask_maker, Tenths(20_000), "ask".into(), None, None);
        e.seed_bot_account(bid_maker, Tenths(20_000), "bid".into(), None, None);
        e.get_or_create_account(user);

        e.place_limit(ask_maker, Side::Ask, tick(500), Qty(10), TradeSource::Bot)
            .unwrap();
        e.place_market(user, Side::Bid, Qty(10), TradeSource::Bot).unwrap();
        assert_eq!(e.position(&user).position, Position(10));
        assert_eq!(e.account(&user).unwrap().usdc_balance, Tenths::ZERO);

        // The position's cost-basis target is $5, but closing at 40c only
        // frees $4. The flip should open the largest affordable NO size,
        // not close and then fail on the second leg.
        e.place_limit(bid_maker, Side::Bid, tick(400), Qty(18), TradeSource::Bot)
            .unwrap();
        let out = e.flip_position(user, TradeSource::Bot).unwrap();

        assert_eq!(out.target_notional_tenths, Tenths(5_000));
        assert_eq!(out.close.fills.iter().map(|f| f.qty.0).sum::<u32>(), 10);
        assert_eq!(out.open.fills.iter().map(|f| f.qty.0).sum::<u32>(), 6);
        let acct = e.account(&user).unwrap();
        let acct_ps = e.position(&user);
        assert_eq!(acct_ps.position, Position(-6));
        assert_eq!(acct_ps.entry_cost_tenths, Tenths(2_400));
        assert_eq!(acct.usdc_balance, Tenths(400));
    }

    // ───────────────────────── Per-market stat counters ─────────────────────

    #[test]
    fn stats_empty_on_fresh_engine() {
        let e = Engine::new(START);
        assert_eq!(e.market_volume_tenths(), 0);
        assert_eq!(e.market_trade_count(), 0);
        assert_eq!(e.market_unique_traders(), 0);
        assert!(e.market_largest_trade().is_none());
        assert_eq!(e.open_interest(), 0);
    }

    #[test]
    fn stats_volume_and_count_after_trades() {
        let (mut e, a, b) = engine_with_accounts();
        // Open fills: A buys 5 YES from B, then 3 YES from B at the same price.
        e.place_limit(b, Side::Ask, tick(500), Qty(8), TradeSource::Bot).unwrap();
        e.place_market(a, Side::Bid, Qty(5), TradeSource::Bot).unwrap();
        e.place_market(a, Side::Bid, Qty(3), TradeSource::Bot).unwrap();

        // 2 fills, total qty 8. Notional = 8 × $1 = 8000 tenths.
        assert_eq!(e.market_trade_count(), 2);
        assert_eq!(e.market_volume_tenths(), 8 * PAYOUT_PER_CONTRACT);
        // OI is the contracts outstanding (8), locked = OI × $1.
        assert_eq!(e.open_interest(), 8);
    }

    #[test]
    fn stats_unique_traders_counts_taker_and_maker_dedup() {
        let mut e = Engine::new(START);
        let a = addr("0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let b = addr("0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        let c = addr("0xcccccccccccccccccccccccccccccccccccccccc");
        e.get_or_create_account(a);
        e.get_or_create_account(b);
        e.get_or_create_account(c);

        // Two fills total, three distinct participants.
        e.place_limit(b, Side::Ask, tick(500), Qty(5), TradeSource::Bot).unwrap();
        e.place_market(a, Side::Bid, Qty(5), TradeSource::Bot).unwrap(); // fill: taker=a, maker=b
        e.place_limit(c, Side::Ask, tick(500), Qty(2), TradeSource::Bot).unwrap();
        e.place_market(a, Side::Bid, Qty(2), TradeSource::Bot).unwrap(); // fill: taker=a, maker=c

        // `a` appears as taker in both fills; counted once. 3 distinct addresses.
        assert_eq!(e.market_unique_traders(), 3);
        assert_eq!(e.market_trade_count(), 2);
    }

    #[test]
    fn stats_largest_trade_is_by_qty() {
        let (mut e, a, b) = engine_with_accounts();
        e.place_limit(b, Side::Ask, tick(500), Qty(50), TradeSource::Bot).unwrap();

        // Three fills of qty 3, 10, 7. Largest = 10.
        e.place_market(a, Side::Bid, Qty(3), TradeSource::Bot).unwrap();
        e.place_market(a, Side::Bid, Qty(10), TradeSource::Bot).unwrap();
        e.place_market(a, Side::Bid, Qty(7), TradeSource::Bot).unwrap();

        let largest = e.market_largest_trade().expect("trades occurred");
        assert_eq!(largest.qty.0, 10);
        assert_eq!(e.market_trade_count(), 3);
    }

    #[test]
    fn stats_largest_trade_ties_favor_most_recent() {
        let (mut e, a, b) = engine_with_accounts();
        e.place_limit(b, Side::Ask, tick(500), Qty(30), TradeSource::Bot).unwrap();

        // Two fills both of qty 5; second wins by recency.
        e.place_market(a, Side::Bid, Qty(5), TradeSource::Bot).unwrap();
        let first = e.market_largest_trade().copied().unwrap();
        e.place_market(a, Side::Bid, Qty(5), TradeSource::Bot).unwrap();
        let second = e.market_largest_trade().copied().unwrap();

        assert_eq!(first.qty.0, 5);
        assert_eq!(second.qty.0, 5);
        // Different ts proves it's the later fill.
        assert!(second.ts > first.ts);
    }

    #[test]
    fn stats_reset_on_market_resolve() {
        let (mut e, a, b) = engine_with_accounts();
        e.open_first_market(50_000.0, 1_000, 60);
        e.place_limit(b, Side::Ask, tick(500), Qty(5), TradeSource::Bot).unwrap();
        e.place_market(a, Side::Bid, Qty(5), TradeSource::Bot).unwrap();

        assert_eq!(e.market_trade_count(), 1);
        assert!(e.market_volume_tenths() > 0);
        assert_eq!(e.market_unique_traders(), 2);
        assert!(e.market_largest_trade().is_some());

        // Resolve and open the next market.
        e.resolve_and_open_next(60_000.0, 60_000.0, 2_000, 60);

        assert_eq!(e.market_volume_tenths(), 0);
        assert_eq!(e.market_trade_count(), 0);
        assert_eq!(e.market_unique_traders(), 0);
        assert!(e.market_largest_trade().is_none());
        assert_eq!(e.open_interest(), 0);
    }

    #[test]
    fn stats_track_new_fills_after_resolve() {
        // Stats reset on resolve and then accumulate fresh for the next market.
        let (mut e, a, b) = engine_with_accounts();
        e.open_first_market(50_000.0, 1_000, 60);

        // Market 1: one fill of qty 4.
        e.place_limit(b, Side::Ask, tick(500), Qty(4), TradeSource::Bot).unwrap();
        e.place_market(a, Side::Bid, Qty(4), TradeSource::Bot).unwrap();
        assert_eq!(e.market_trade_count(), 1);

        e.resolve_and_open_next(60_000.0, 60_000.0, 2_000, 60);
        assert_eq!(e.market_trade_count(), 0);

        // Market 2: two fills of qty 2 and 6. Largest = 6, count = 2,
        // volume = 8 × $1.
        e.place_limit(b, Side::Ask, tick(500), Qty(8), TradeSource::Bot).unwrap();
        e.place_market(a, Side::Bid, Qty(2), TradeSource::Bot).unwrap();
        e.place_market(a, Side::Bid, Qty(6), TradeSource::Bot).unwrap();

        assert_eq!(e.market_trade_count(), 2);
        assert_eq!(e.market_volume_tenths(), 8 * PAYOUT_PER_CONTRACT);
        assert_eq!(e.market_unique_traders(), 2);
        assert_eq!(e.market_largest_trade().unwrap().qty.0, 6);
    }

    // ──────────────────── Per-user order log recording ──────────────────

    /// Helper: engine with an open market and two pre-funded accounts.
    /// Caller chooses which (if any) to mark as a registered user.
    fn engine_with_market() -> (Engine, Address, Address) {
        let mut e = Engine::new(START);
        let a = addr(A);
        let b = addr(B);
        e.get_or_create_account(a);
        e.get_or_create_account(b);
        e.open_first_market(50_000.0, 0, 60);
        (e, a, b)
    }

    #[test]
    fn taker_market_records_one_row_with_filled_status() {
        let (mut e, user, bot) = engine_with_market();
        e.register_user(user);
        e.place_limit(bot, Side::Ask, tick(500), Qty(5), TradeSource::Bot)
            .unwrap();
        e.place_market(user, Side::Bid, Qty(5), TradeSource::Ui)
            .unwrap();

        let rows = e.user_orders(&user);
        assert_eq!(rows.len(), 1);
        let row = rows[0];
        assert_eq!(row.source, TradeSource::Ui);
        assert_eq!(row.order_type, OrderType::Market);
        assert_eq!(row.qty_requested, Qty(5));
        assert_eq!(row.filled_qty, Qty(5));
        assert_eq!(row.notional_tenths, 5 * 500);
        assert_eq!(row.status, UserOrderStatus::Filled);
        assert_eq!(row.market_id, MarketId(1));
        assert_eq!(row.limit_price, None);
        // Bot doesn't get a row regardless of activity.
        assert!(e.user_orders(&bot).is_empty());
    }

    #[test]
    fn taker_sweep_collapses_into_one_row() {
        let (mut e, user, bot) = engine_with_market();
        e.register_user(user);
        let c = addr(C);
        e.get_or_create_account(c);
        e.place_limit(bot, Side::Ask, tick(500), Qty(3), TradeSource::Bot)
            .unwrap();
        e.place_limit(c, Side::Ask, tick(550), Qty(2), TradeSource::Bot)
            .unwrap();
        e.place_market(user, Side::Bid, Qty(5), TradeSource::Ui)
            .unwrap();

        let rows = e.user_orders(&user);
        assert_eq!(rows.len(), 1, "one place call = one row");
        assert_eq!(rows[0].filled_qty, Qty(5));
        // VWAP encoded in notional_tenths: 3*500 + 2*550 = 2600
        assert_eq!(rows[0].notional_tenths, 2600);
        assert_eq!(rows[0].status, UserOrderStatus::Filled);
    }

    #[test]
    fn maker_partial_fill_transitions_status_in_place() {
        let (mut e, user, bot) = engine_with_market();
        e.register_user(user);
        e.place_limit(user, Side::Ask, tick(600), Qty(10), TradeSource::Ui)
            .unwrap();
        assert_eq!(e.user_orders(&user).len(), 1);
        assert_eq!(
            e.user_orders(&user)[0].status,
            UserOrderStatus::Rested
        );

        // First taker hits 4.
        e.place_market(bot, Side::Bid, Qty(4), TradeSource::Bot)
            .unwrap();
        let row = e.user_orders(&user)[0];
        assert_eq!(row.filled_qty, Qty(4));
        assert_eq!(row.notional_tenths, 4 * 600);
        assert_eq!(row.status, UserOrderStatus::PartiallyFilled);

        // Second taker finishes it.
        e.place_market(bot, Side::Bid, Qty(6), TradeSource::Bot)
            .unwrap();
        let row = e.user_orders(&user)[0];
        assert_eq!(row.filled_qty, Qty(10));
        assert_eq!(row.notional_tenths, 10 * 600);
        assert_eq!(row.status, UserOrderStatus::Filled);
        assert_eq!(e.user_orders(&user).len(), 1, "still one row");
    }

    #[test]
    fn limit_partial_at_placement_records_partial_status() {
        let (mut e, user, bot) = engine_with_market();
        e.register_user(user);
        // Bot rests 3 asks at 50; user's bid wants 5, crosses for 3 and rests 2.
        e.place_limit(bot, Side::Ask, tick(500), Qty(3), TradeSource::Bot)
            .unwrap();
        e.place_limit(user, Side::Bid, tick(550), Qty(5), TradeSource::Ui)
            .unwrap();
        let row = e.user_orders(&user)[0];
        assert_eq!(row.qty_requested, Qty(5));
        assert_eq!(row.filled_qty, Qty(3));
        assert_eq!(row.status, UserOrderStatus::PartiallyFilled);
        assert_eq!(row.limit_price, Some(550));
    }

    #[test]
    fn cancel_marks_row_cancelled() {
        let (mut e, user, _bot) = engine_with_market();
        e.register_user(user);
        let outcome = e
            .place_limit(user, Side::Bid, tick(400), Qty(2), TradeSource::Ui)
            .unwrap();
        e.cancel(user, outcome.order_id).unwrap();
        let row = e.user_orders(&user)[0];
        assert_eq!(row.status, UserOrderStatus::Cancelled);
        assert_eq!(row.filled_qty, Qty(0));
    }

    #[test]
    fn strategy_source_carries_through_to_row() {
        let (mut e, user, bot) = engine_with_market();
        e.register_user(user);
        e.place_limit(bot, Side::Ask, tick(500), Qty(3), TradeSource::Bot)
            .unwrap();
        e.place_market(user, Side::Bid, Qty(3), TradeSource::Strategy)
            .unwrap();
        assert_eq!(e.user_orders(&user)[0].source, TradeSource::Strategy);
    }

    #[test]
    fn bot_only_activity_writes_no_rows() {
        let (mut e, bot1, bot2) = engine_with_market();
        e.place_limit(bot1, Side::Ask, tick(500), Qty(3), TradeSource::Bot)
            .unwrap();
        e.place_market(bot2, Side::Bid, Qty(3), TradeSource::Bot)
            .unwrap();
        assert!(e.user_orders(&bot1).is_empty());
        assert!(e.user_orders(&bot2).is_empty());
    }

    #[test]
    fn user_vs_user_updates_both_owners_rows() {
        let (mut e, a, b) = engine_with_market();
        e.register_user(a);
        e.register_user(b);
        // A rests via UI; B takes via strategy.
        e.place_limit(a, Side::Ask, tick(500), Qty(2), TradeSource::Ui)
            .unwrap();
        e.place_market(b, Side::Bid, Qty(2), TradeSource::Strategy)
            .unwrap();

        // A: their resting limit got filled.
        let a_row = e.user_orders(&a)[0];
        assert_eq!(a_row.source, TradeSource::Ui);
        assert_eq!(a_row.order_type, OrderType::Limit);
        assert_eq!(a_row.filled_qty, Qty(2));
        assert_eq!(a_row.status, UserOrderStatus::Filled);

        // B: their market order filled fully.
        let b_row = e.user_orders(&b)[0];
        assert_eq!(b_row.source, TradeSource::Strategy);
        assert_eq!(b_row.order_type, OrderType::Market);
        assert_eq!(b_row.filled_qty, Qty(2));
        assert_eq!(b_row.status, UserOrderStatus::Filled);
    }

    #[test]
    fn user_orders_cleared_on_market_resolve() {
        let (mut e, user, bot) = engine_with_market();
        e.register_user(user);
        e.place_limit(bot, Side::Ask, tick(500), Qty(2), TradeSource::Bot)
            .unwrap();
        e.place_market(user, Side::Bid, Qty(2), TradeSource::Ui)
            .unwrap();
        assert_eq!(e.user_orders(&user).len(), 1);
        e.resolve_and_open_next(60_000.0, 60_000.0, 60, 60);
        assert!(e.user_orders(&user).is_empty());
        // ...but the row is still readable from the settled archive.
        let settled = e.settled_user_orders(&user);
        assert_eq!(settled.len(), 1);
        assert_eq!(settled[0].market_id, MarketId(1));
        assert_eq!(settled[0].filled_qty, Qty(2));
    }

    #[test]
    fn settled_archive_marks_resting_cancelled_and_prunes_old_markets() {
        let (mut e, user, bot) = engine_with_market();
        e.register_user(user);
        // Round 1: one fill and one resting limit that the lock cancels.
        e.place_limit(bot, Side::Ask, tick(500), Qty(2), TradeSource::Bot).unwrap();
        e.place_market(user, Side::Bid, Qty(2), TradeSource::Ui).unwrap();
        e.place_limit(user, Side::Bid, tick(100), Qty(1), TradeSource::Ui).unwrap();
        e.resolve_and_open_next(60_000.0, 60_000.0, 60, 60);

        let settled = e.settled_user_orders(&user);
        assert_eq!(settled.len(), 2);
        assert_eq!(settled[0].status, UserOrderStatus::Filled);
        assert_eq!(settled[1].status, UserOrderStatus::Cancelled);
        assert!(e.user_orders(&bot).is_empty() && e.settled_user_orders(&bot).is_empty());

        // One trade per round for CAP more rounds: market 1 must fall out
        // once market CAP+1 settles, and the oldest kept is market 2.
        for i in 0..SETTLED_ORDERS_MARKETS_CAP {
            e.place_limit(bot, Side::Ask, tick(500), Qty(1), TradeSource::Bot).unwrap();
            e.place_market(user, Side::Bid, Qty(1), TradeSource::Ui).unwrap();
            let t = 120 + 60 * i as u64;
            e.resolve_and_open_next(60_000.0, 60_000.0, t, 60);
        }
        let settled = e.settled_user_orders(&user);
        assert_eq!(settled.len() as u32, SETTLED_ORDERS_MARKETS_CAP);
        assert_eq!(settled[0].market_id, MarketId(2));
        assert_eq!(
            settled.last().unwrap().market_id,
            MarketId(SETTLED_ORDERS_MARKETS_CAP + 1)
        );
        assert!(settled.windows(2).all(|w| w[0].market_id <= w[1].market_id));
    }

    #[test]
    fn killed_market_order_records_killed_status() {
        let (mut e, user, bot) = engine_with_market();
        e.register_user(user);
        // Only 2 contracts of liquidity, user wants 5.
        e.place_limit(bot, Side::Ask, tick(500), Qty(2), TradeSource::Bot)
            .unwrap();
        e.place_market(user, Side::Bid, Qty(5), TradeSource::Ui)
            .unwrap();
        let row = e.user_orders(&user)[0];
        assert_eq!(row.qty_requested, Qty(5));
        assert_eq!(row.filled_qty, Qty(2));
        assert_eq!(row.status, UserOrderStatus::Killed);
    }

    #[test]
    fn unregistered_address_returns_empty_slice() {
        let (mut e, a, b) = engine_with_market();
        e.place_limit(a, Side::Ask, tick(500), Qty(1), TradeSource::Bot)
            .unwrap();
        e.place_market(b, Side::Bid, Qty(1), TradeSource::Bot)
            .unwrap();
        assert!(e.user_orders(&a).is_empty());
        assert!(e.user_orders(&b).is_empty());
        assert!(!e.is_user_account(&a));
        e.register_user(a);
        assert!(e.is_user_account(&a));
    }
}
