//! One market series' matching engine.
//!
//! A [`MarketEngine`] owns everything that is *per series*: the order
//! book, the current and resolved rounds, open interest, per-account
//! positions, the per-round stats and the per-user order log. It owns no
//! money: every method that moves cash takes the shared [`Ledger`] as a
//! parameter and performs single atomic operations on it, so several
//! engines can draw on one balance without racing each other.
//!
//! Money is in `Tenths` (1/10 of a cent). One contract pays $1 at
//! settlement, i.e. `Tenths(1000)`. Prices are in tenths of a cent as well,
//! so a single share at price `p` costs exactly `p` tenths — clean integer
//! math with no rounding.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use super::book::Book;
use super::events::{ExecKind, ExecutionEvent, ExecutionSink};
use super::ids::IdAllocator;
use super::ledger::Ledger;
use super::model::{Fill, FillKind, Order};
use super::positions::PositionState;
use super::model::OrderType;
use super::{
    CancelError, FlipOutcome, Market, MarketResult, MarketStatus, OrderError, OrderStatus,
    Outcome, PAYOUT_PER_CONTRACT, PlaceOutcome, RECENT_FILLS_CAP, RESOLVED_MARKETS_CAP,
    ReplaceError, ResolutionReport, RoundClose, RoundReference, SETTLED_ORDERS_MARKETS_CAP,
    SettleReport,
    TradeSource, UserOrderRow, UserOrderStatus,
};
use crate::ticks::PriceTick;
use crate::types::{Address, MarketId, OrderId, Position, Qty, SeriesId, Side, Tenths};

/// Per-order options the automation stack sets and everybody else leaves
/// at the default. `client_order_id` is what makes an order *observable*:
/// execution events are emitted only for orders that carry one, so bots
/// and the UI cost nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlaceOpts {
    /// The strategy runtime's id for this order. Bots and UI: `None`.
    pub client_order_id: Option<Arc<str>>,
    /// Limit orders only: immediate-or-cancel. The unfilled residual is
    /// killed instead of resting, reported as one `Partial` / `Expiry`.
    pub ioc: bool,
    /// Limit orders only: reject instead of taking when the order would
    /// cross the book.
    pub post_only: bool,
    /// Clamp the quantity to what closes the account's current position;
    /// an order that would open or extend exposure is rejected.
    pub reduce_only: bool,
}

impl PlaceOpts {
    pub fn with_client_id(id: impl Into<Arc<str>>) -> Self {
        PlaceOpts { client_order_id: Some(id.into()), ..Default::default() }
    }
}

/// Where a series reads "now" (unix seconds) when its lock is on. The
/// system clock unless a driver that knows the venue's time replaces it.
#[derive(Clone)]
pub struct RoundClock(Arc<dyn Fn() -> u64 + Send + Sync>);

impl RoundClock {
    pub fn new(now: impl Fn() -> u64 + Send + Sync + 'static) -> Self {
        Self(Arc::new(now))
    }

    pub fn system() -> Self {
        Self::new(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs())
        })
    }

    pub fn now(&self) -> u64 {
        (self.0)()
    }
}

impl std::fmt::Debug for RoundClock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RoundClock")
    }
}

#[derive(Debug)]
pub struct MarketEngine {
    /// When set, an order arriving at or after the round's `closes_at`
    /// is refused with [`OrderError::MarketClosed`]. Off by default: a
    /// simulated series resolves at its close, so nothing can arrive
    /// late; a series that follows an outside venue waits for that
    /// venue's result and must not trade while it waits.
    lock_at_close: bool,
    clock: RoundClock,
    /// Which series this engine runs. Orders from accounts with a
    /// different home series are rejected.
    series: SeriesId,
    book: Book,
    open_interest: u64,
    /// Σ per-unit lock × open qty over every resting order in this
    /// series, plus any market-order peak reservation while its cross is
    /// in flight (zero between calls). Invariant: Σ over series equals
    /// Σ `Account.locked` on the ledger.
    locked_total: Tenths,
    /// Global id source shared with every other series' engine.
    ids: Arc<IdAllocator>,
    /// Per-order reservation, in tenths-per-unit. Multiplying by an order's
    /// remaining `qty_open` gives the amount currently in `Account.locked`
    /// for that order, which must be refunded on cancel or per-unit fill.
    order_locks: HashMap<OrderId, Tenths>,
    /// Client order id of every *resting* order that carries one, keyed by
    /// engine id. `Order` stays `Copy` for the book; the maker's client id
    /// is looked up here inside `execute_match`. Removed when the order
    /// leaves the book (fill, cancel, settlement).
    client_ids: HashMap<OrderId, Arc<str>>,
    /// Where execution events for client-id orders go. `None` drops them.
    executions: Option<ExecutionSink>,
    /// Rolling buffer of fills generated by the engine. New fills are
    /// pushed to the back; the oldest is dropped when capacity is reached.
    recent_fills: VecDeque<Fill>,
    /// The active market round, or `None` before the first round is opened.
    current_market: Option<Market>,
    /// History of resolved markets, newest at the back, capped at
    /// [`RESOLVED_MARKETS_CAP`].
    resolved_markets: VecDeque<Market>,
    /// Sum of fill notional in the current market, in tenths-of-cent. Each
    /// fill contributes `qty × PAYOUT_PER_CONTRACT` — the symmetric cash
    /// committed by both sides (buyer pays `qty × price`, seller posts
    /// `qty × (1$ − price)` of NO-side collateral, summing to `qty × $1`).
    /// Resets to zero in [`Engine::settle_all`].
    market_volume_tenths: i64,
    /// Number of fills recorded in the current market. Resets in
    /// [`Engine::settle_all`].
    market_trade_count: u64,
    /// Distinct participant addresses (taker or maker) seen in the current
    /// market. Resets in [`Engine::settle_all`].
    market_unique_traders: HashSet<Address>,
    /// Largest single fill in the current market, by `qty`. On ties the more
    /// recent fill wins. Resets in [`Engine::settle_all`].
    market_largest_trade: Option<Fill>,
    /// The account that mirrors an outside venue's book into this one,
    /// if this series has one. See [`Self::set_mirror`].
    mirror: Option<Address>,
    /// Per-user trade log scoped to the current market. One entry per
    /// order placement by a registered user; mutated in place as fills
    /// land against it. Cleared in [`Engine::settle_all`]. Bot accounts
    /// never write here. Both sides of a user-vs-user fill update the
    /// respective resting/aggressing rows of their owners
    /// independently.
    user_orders: HashMap<Address, Vec<UserOrderRow>>,
    /// Rows moved out of `user_orders` at settlement, oldest market
    /// first, kept for the last [`SETTLED_ORDERS_MARKETS_CAP`] markets so
    /// trade history survives the round boundary. Resting rows are marked
    /// cancelled on the way in (the lock phase cancels them).
    settled_orders: HashMap<Address, Vec<UserOrderRow>>,
    /// Per-account exposure in this engine's series. Absent ⇒ flat with no
    /// history. Cleared at settlement (every position is flat then).
    positions: HashMap<Address, PositionState>,
}

impl MarketEngine {
    pub fn new(series: SeriesId, ids: Arc<IdAllocator>) -> Self {
        Self {
            lock_at_close: false,
            clock: RoundClock::system(),
            series,
            book: Book::new(),
            open_interest: 0,
            locked_total: Tenths::ZERO,
            ids,
            order_locks: HashMap::new(),
            client_ids: HashMap::new(),
            executions: None,
            recent_fills: VecDeque::with_capacity(RECENT_FILLS_CAP),
            current_market: None,
            resolved_markets: VecDeque::with_capacity(RESOLVED_MARKETS_CAP),
            market_volume_tenths: 0,
            market_trade_count: 0,
            market_unique_traders: HashSet::new(),
            market_largest_trade: None,
            mirror: None,
            user_orders: HashMap::new(),
            settled_orders: HashMap::new(),
            positions: HashMap::new(),
        }
    }

    /// Refuse orders from the round's close until the next round opens.
    pub fn set_lock_at_close(&mut self, on: bool) {
        self.lock_at_close = on;
    }

    /// Replace the clock the lock reads.
    pub fn set_clock(&mut self, clock: RoundClock) {
        self.clock = clock;
    }

    /// Make `address` this series' book mirror: the account that copies an
    /// outside venue's book into this one. From then on the public tape,
    /// the round's volume, trade count and largest trade are the venue's
    /// prints ([`Self::record_print`]), not fills made here, and the
    /// mirror is never counted as a trader.
    pub fn set_mirror(&mut self, address: Address) {
        self.mirror = Some(address);
    }

    pub fn mirror(&self) -> Option<Address> {
        self.mirror
    }

    /// A trade printed on the mirrored venue: `taker_side` is the side
    /// that took. Goes on the tape and into the round's numbers; moves
    /// no money. Ignored on a series with no mirror.
    pub fn record_print(&mut self, taker_side: Side, price: PriceTick, qty: Qty) {
        let Some(mirror) = self.mirror else {
            return;
        };
        let ts = self.next_seq();
        // The venue's traders are unknown here: the print carries the
        // mirror's address on both sides and counts no trader.
        let print = Fill { taker: mirror, maker: mirror, taker_side, price, qty, ts, kind: FillKind::Open };
        self.push_to_tape(print);
    }

    /// Whether the series is refusing orders: its round has reached its
    /// close, or no round is open. Always false with the lock off.
    pub fn is_locked(&self) -> bool {
        self.lock_at_close
            && self.current_market.as_ref().is_none_or(|m| self.clock.now() >= m.closes_at)
    }

    /// `address`'s exposure in this series. Flat/default for addresses
    /// that never traded here.
    pub fn position(&self, address: &Address) -> PositionState {
        self.positions.get(address).copied().unwrap_or_default()
    }

    fn position_mut(&mut self, address: Address) -> &mut PositionState {
        self.positions.entry(address).or_default()
    }

    /// Every address with a recorded position state (possibly flat, if a
    /// round result is being carried) in this series.
    pub fn positions_iter(&self) -> impl Iterator<Item = (&Address, &PositionState)> {
        self.positions.iter()
    }

    #[cfg(test)]
    pub fn set_position(&mut self, address: Address, state: PositionState) {
        self.positions.insert(address, state);
    }

    pub fn series(&self) -> SeriesId {
        self.series
    }

    /// Install the execution-event sink (see [`super::events`]).
    pub fn set_execution_sink(&mut self, sink: ExecutionSink) {
        self.executions = Some(sink);
    }

    /// Emit one execution event for a client-id order. No sink, no work.
    #[allow(clippy::too_many_arguments)]
    fn emit(
        &self,
        client_order_id: Arc<str>,
        account: Address,
        venue_order_id: Option<OrderId>,
        kind: ExecKind,
        filled_qty: Qty,
        remaining_qty: Qty,
        price: Option<PriceTick>,
    ) {
        let Some(sink) = &self.executions else { return };
        sink.send(ExecutionEvent {
            series: self.series,
            market: self.current_market.as_ref().map(|m| m.id),
            account,
            client_order_id,
            venue_order_id,
            kind,
            filled_qty,
            remaining_qty,
            price,
        });
    }

    /// Per-user order log for the current market. Empty slice for any
    /// address that hasn't placed an order yet (including unregistered
    /// /bot addresses, which are never recorded). Cleared on
    /// [`Engine::settle_all`].
    pub fn user_orders(&self, address: &Address) -> &[UserOrderRow] {
        self.user_orders
            .get(address)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// `address`'s order rows from resolved markets, oldest market first
    /// (rows within a market keep placement order). Covers at most the
    /// last [`SETTLED_ORDERS_MARKETS_CAP`] markets; every row carries its
    /// `market_id`. Empty for bots and never-traded addresses.
    pub fn settled_user_orders(&self, address: &Address) -> &[UserOrderRow] {
        self.settled_orders
            .get(address)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Number of `address`'s orders in the current market that received
    /// at least one fill. A market sweep across several levels is one
    /// trade; a resting limit hit by several takers is one trade.
    pub fn trades_this_market(&self, address: &Address) -> u32 {
        count_filled_orders(self.user_orders(address))
    }

    pub fn book(&self) -> &Book {
        &self.book
    }

    pub fn open_interest(&self) -> u64 {
        self.open_interest
    }

    /// Reserved cash attributable to this series' resting orders. See the
    /// field doc for the cross-series invariant.
    pub fn locked_total(&self) -> Tenths {
        self.locked_total
    }

    /// Notional volume of fills in the current market, in tenths-of-cent.
    /// See [`Engine`]'s `market_volume_tenths` field for the definition.
    pub fn market_volume_tenths(&self) -> i64 {
        self.market_volume_tenths
    }

    /// Number of fills recorded in the current market.
    pub fn market_trade_count(&self) -> u64 {
        self.market_trade_count
    }

    /// Number of distinct participant addresses (taker or maker) seen in the
    /// current market.
    pub fn market_unique_traders(&self) -> u64 {
        self.market_unique_traders.len() as u64
    }

    /// Largest single fill in the current market, by qty. `None` until the
    /// first fill of the round.
    pub fn market_largest_trade(&self) -> Option<&Fill> {
        self.market_largest_trade.as_ref()
    }

    /// Best-effort YES mark price. Two-sided book → strict mid; one-
    /// sided book → implied extreme (99.9¢ if bids only, 0.1¢ if asks
    /// only); empty book → `None`.
    ///
    /// Thin passthrough over [`Book::implied_mark_yes_tenths`] that
    /// re-wraps the tenths integer into a typed [`PriceTick`]. Provided
    /// here so callers (notably the strategy executor and the position
    /// snapshot) can read a single canonical mark without reaching
    /// through `book()`.
    ///
    /// Internal engine paths that need a *transactable* reference
    /// price (e.g. flip notional sizing) continue to call
    /// [`Book::mid_yes_tenths`] directly — they must not over-value
    /// against an implied extreme that no order will actually clear at.
    pub fn mark_price(&self) -> Option<PriceTick> {
        self.book
            .implied_mark_yes_tenths()
            .and_then(PriceTick::from_tenths)
    }

    /// Returns `true` iff `id` is currently a live, resting order on the
    /// book (i.e. has any remaining open quantity).
    ///
    /// The strategy executor uses this to detect when a limit order it
    /// placed has either fully filled or been cancelled out-of-band:
    /// either way, the order is no longer resting and the block can
    /// transition out of `WaitingFill`.
    pub fn is_order_resting(&self, id: OrderId) -> bool {
        self.book.get_order(id).is_some()
    }

    /// Returns up to `limit` of the most recent fills, newest first.
    pub fn recent_trades(&self, limit: usize) -> impl Iterator<Item = &Fill> {
        self.recent_fills.iter().rev().take(limit)
    }

    pub fn current_market(&self) -> Option<&Market> {
        self.current_market.as_ref()
    }

    /// Returns up to `limit` of the most recently resolved markets, newest first.
    pub fn resolved_markets(&self, limit: usize) -> impl Iterator<Item = &Market> {
        self.resolved_markets.iter().rev().take(limit)
    }

    /// Open the first market round. Must be called once before
    /// [`resolve_and_open_next`]. Panics if a market is already open —
    /// caller bug.
    pub fn open_first_market(&mut self, strike_btc_usd: f64, opens_at: u64, duration_secs: u64) {
        self.open_first_market_with(RoundReference::Btc { strike_btc_usd }, opens_at, duration_secs);
    }

    /// [`open_first_market`](Self::open_first_market) for any underlying.
    pub fn open_first_market_with(&mut self, reference: RoundReference, opens_at: u64, duration_secs: u64) {
        assert!(
            self.current_market.is_none(),
            "open_first_market called when a market is already open",
        );
        self.current_market = Some(self.build_market(reference, opens_at, duration_secs));
    }

    /// BTC convenience over [`resolve_with`](Self::resolve_with): Yes iff
    /// the close is strictly above the strike, and the next round opens
    /// on `new_strike_btc_usd`. Panics on a non-BTC round.
    pub fn resolve_and_open_next(
        &mut self,
        ledger: &mut Ledger,
        close_price_btc_usd: f64,
        new_strike_btc_usd: f64,
        new_opens_at: u64,
        duration_secs: u64,
    ) -> ResolutionReport {
        let strike = self
            .current_market
            .as_ref()
            .expect("resolve_and_open_next called with no market open")
            .strike_btc_usd()
            .expect("resolve_and_open_next on a non-BTC round");
        let outcome = if close_price_btc_usd > strike { Outcome::Yes } else { Outcome::No };
        self.resolve_with(
            ledger,
            outcome,
            RoundClose::Btc { close_price_btc_usd },
            RoundReference::Btc { strike_btc_usd: new_strike_btc_usd },
            new_opens_at,
            duration_secs,
        )
    }

    /// Resolve the current market with a driver-decided `outcome`, settle
    /// all positions, and open the next round on `next`.
    ///
    /// Settlement:
    /// 1. Cancel all resting orders (refunds their locked collateral).
    /// 2. For user accounts: pay out winners ($1 per contract on the winning
    ///    side), zero positions and locked.
    /// 3. For bot accounts: zero positions, locked, and reset balance to
    ///    their seeded bankroll so they never run dry across rounds.
    /// 4. Zero open interest.
    ///
    /// Panics if no market is currently open — call [`open_first_market`] first.
    pub fn resolve_with(
        &mut self,
        ledger: &mut Ledger,
        outcome: Outcome,
        close: RoundClose,
        next: RoundReference,
        new_opens_at: u64,
        duration_secs: u64,
    ) -> ResolutionReport {
        let SettleReport { resolved_id, outcome, cancelled_orders } = self.resolve_only(ledger, outcome, close);

        let next = self.build_market(next, new_opens_at, duration_secs);
        let new_market_id = next.id;
        self.current_market = Some(next);

        ResolutionReport {
            resolved_id,
            outcome,
            cancelled_orders,
            new_market_id,
        }
    }

    /// [`resolve_with`](Self::resolve_with) without the next round: the
    /// series is left with no open round.
    ///
    /// Panics if no market is currently open.
    pub fn resolve_only(&mut self, ledger: &mut Ledger, outcome: Outcome, close: RoundClose) -> SettleReport {
        let mut current = self
            .current_market
            .take()
            .expect("resolve called with no market open");

        let cancelled_orders = self.cancel_all_resting(ledger);

        self.settle_all(ledger, outcome, current.id);

        current.status = MarketStatus::Resolved { outcome, close };
        let resolved_id = current.id;

        if self.resolved_markets.len() == RESOLVED_MARKETS_CAP {
            self.resolved_markets.pop_front();
        }
        self.resolved_markets.push_back(current);

        SettleReport { resolved_id, outcome, cancelled_orders }
    }

    fn settle_all(&mut self, ledger: &mut Ledger, outcome: Outcome, resolved_id: MarketId) {
        // Settlement is a forced close of every position at the terminal
        // YES price: 1$ on Yes, 0 on No. Running it through the same PnL
        // path as a fill makes `realized_pnl_this_market` the complete
        // round result before it is frozen into `last_market`.
        let settle_price = match outcome {
            Outcome::Yes => PAYOUT_PER_CONTRACT,
            Outcome::No => 0,
        };
        let trade_counts: HashMap<Address, u32> = self
            .user_orders
            .iter()
            .map(|(a, rows)| (*a, count_filled_orders(rows)))
            .collect();

        // 1. Settle every position: pay out registered user accounts
        //    (top-level users and strategy sub-accounts alike) and freeze
        //    their round result. Bot exposure just vanishes — their
        //    bankroll is reset below. Locked collateral is untouched:
        //    `cancel_all_resting` already refunded this series' orders and
        //    other series' reservations are not ours to release.
        let addresses: Vec<Address> = self.positions.keys().copied().collect();
        // Round PnL per user address, settlement included; feeds the
        // leaderboard below.
        let mut realized_by: HashMap<Address, i64> = HashMap::new();
        for address in addresses {
            let is_user = ledger.is_user_account(&address);
            let ps = self.positions.get_mut(&address).expect("just listed");
            let pos = ps.position.0;
            if is_user {
                let payout = match outcome {
                    Outcome::Yes => pos.max(0) * PAYOUT_PER_CONTRACT,
                    Outcome::No => (-pos).max(0) * PAYOUT_PER_CONTRACT,
                };
                apply_pnl_for_fill(ps, pos, -pos, settle_price);
                let pnl_tenths = ps.realized_pnl_this_market.0;
                ps.last_market = Some(MarketResult {
                    series: self.series,
                    market_id: resolved_id,
                    outcome,
                    pnl_tenths,
                    trades: trade_counts.get(&address).copied().unwrap_or(0),
                });
                realized_by.insert(address, pnl_tenths);
                ledger.credit(address, Tenths(payout));
                if let Some(acct) = ledger.account_mut(&address) {
                    acct.lifetime_realized_pnl_tenths += pnl_tenths;
                }
            }
            ps.position = Position::FLAT;
            ps.entry_cost_tenths = Tenths::ZERO;
            ps.realized_pnl_this_market = Tenths::ZERO;
        }
        // Bots never lose money: back to their seeded bankroll (or the
        // engine default for unseeded bot accounts). Only *this* series'
        // bots — a bot homed elsewhere is mid-round there and its balance
        // is that engine's to reset.
        let series = self.series;
        let bots: Vec<Address> = ledger
            .accounts_iter()
            .filter(|a| !ledger.is_user_account(&a.address))
            .filter(|a| a.home_series.is_none_or(|h| h == series))
            .map(|a| a.address)
            .collect();
        for bot in bots {
            ledger.reset_bot(bot);
        }

        // 2. Leaderboard, keyed by *root* user: the round's realized PnL
        //    (settlement included) of the root plus every child. Cash
        //    parked in strategies or committed to another series never
        //    shows up as a loss because only realized PnL is folded in.
        //    A round counts as participated if the root or any of its
        //    children placed an order.
        let roots: Vec<Address> = ledger.accounts_iter()
            .filter(|a| ledger.is_user_account(&a.address) && !a.is_sub_account())
            .map(|a| a.address)
            .collect();
        for root in roots {
            let children = ledger.sub_accounts_of(&root);
            let participated = self.user_orders.contains_key(&root)
                || children.iter().any(|c| self.user_orders.contains_key(c));
            let family_realized: i64 = std::iter::once(&root)
                .chain(children.iter())
                .filter_map(|a| realized_by.get(a))
                .sum();
            ledger.record_round(
                root,
                self.series,
                resolved_id,
                family_realized,
                participated,
            );
        }

        // 3. Orphan sweep. `close_sub_account` leaves a child behind when it
        //    cannot flatten the position mid-round, clearing its strategy
        //    as the marker. Positions are flat and locks are zero now, so
        //    sweep the cash home and delete the account.
        let orphans: Vec<Address> = ledger
            .accounts_iter()
            .filter(|a| a.is_sub_account() && a.strategy.is_none())
            .filter(|a| a.home_series == Some(series))
            .map(|a| a.address)
            .collect();
        for child in orphans {
            self.forget_account(child);
            ledger.remove_sub_account(child);
        }

        // Positions are flat now. Drop bot entries outright; keep user
        // entries only as carriers of `last_market` (still flat).
        self.positions.retain(|_, ps| ps.last_market.is_some());

        self.open_interest = 0;

        // Per-market stats reset alongside open_interest — same lifecycle
        // boundary, see `market_volume_tenths` field doc.
        self.market_volume_tenths = 0;
        self.market_trade_count = 0;
        self.market_unique_traders.clear();
        self.market_largest_trade = None;
        // Per-user order log is single-market scoped: move this round's
        // rows into the settled archive and prune markets that fell out
        // of the window. Orphans removed above already had their rows
        // dropped by `remove_sub_account`.
        let oldest_kept = resolved_id.0.saturating_sub(SETTLED_ORDERS_MARKETS_CAP - 1);
        for (account, mut rows) in self.user_orders.drain() {
            for row in &mut rows {
                if row.status == UserOrderStatus::Rested {
                    row.status = UserOrderStatus::Cancelled;
                }
            }
            self.settled_orders.entry(account).or_default().extend(rows);
        }
        self.settled_orders.retain(|_, rows| {
            rows.retain(|r| r.market_id.0 >= oldest_kept);
            !rows.is_empty()
        });
    }

    fn build_market(&mut self, reference: RoundReference, opens_at: u64, duration_secs: u64) -> Market {
        let id = self.ids.next_market();
        Market {
            id,
            series: self.series,
            reference,
            opened_at: opens_at,
            closes_at: opens_at + duration_secs,
            duration_secs,
            status: MarketStatus::Open,
        }
    }

    /// Cancel every resting order on the book and refund each owner's
    /// locked collateral. Returns the number of orders cancelled.
    ///
    /// Runs in submission order (`Order.ts`) so client-id orders receive
    /// their `CancelAck`s in the order they were placed; the runtime's
    /// snapshotted transition tables depend on that at the lock phase.
    fn cancel_all_resting(&mut self, ledger: &mut Ledger) -> u32 {
        let mut orders: Vec<Order> = self
            .book
            .iter_bid_orders()
            .chain(self.book.iter_ask_orders())
            .collect();
        orders.sort_by_key(|o| o.ts);
        let count = orders.len() as u32;
        for order in orders {
            let id = order.id;
            let per_unit = self
                .order_locks
                .remove(&id)
                .expect("resting order must have a lock entry");
            let refund = Tenths(per_unit.0 * order.qty_open.0 as i64);
            ledger.release(order.account, refund);
            self.locked_total -= refund;
            if let Some(cid) = self.client_ids.remove(&id) {
                self.emit(cid, order.account, Some(id), ExecKind::CancelAck, Qty::ZERO, Qty::ZERO, None);
            }
            self.book.cancel(id).expect("book had it");
        }
        count
    }

    /// Realized PnL `account` accrued since `before` (a prior reading of
    /// `realized_pnl_this_market`). Used to attribute a taker's
    /// close-side realization to the order that caused it: a taker never
    /// matches itself (self-trades are cancelled in `cross`), so the
    /// counter delta across one `cross` belongs entirely to that order.
    fn realized_since(&self, account: Address, before: i64) -> i64 {
        self.position(&account).realized_pnl_this_market.0 - before
    }

    /// Append a fresh `UserOrderRow` for a registered user's placement.
    /// No-op for bot/unregistered accounts. Called from `place_limit` /
    /// `place_market` after `cross` returns; the row captures both the
    /// requested params and any taker-side fills that happened
    /// immediately. Future maker-side fills against this order are
    /// applied via `record_user_order_fill` inside `execute_match`.
    #[allow(clippy::too_many_arguments)]
    fn record_user_order_placement(
        &mut self,
        ledger: &Ledger,
        account: Address,
        order_id: OrderId,
        side: Side,
        qty_requested: Qty,
        limit_price: Option<u16>,
        source: TradeSource,
        order_type: OrderType,
        filled_qty: Qty,
        notional_tenths: u64,
        status: UserOrderStatus,
        realized_pnl_tenths: i64,
    ) {
        if !ledger.is_user_account(&account) {
            return;
        }
        let Some(market) = self.current_market.as_ref() else {
            return;
        };
        let market_id = market.id;
        let placed_ts = self.next_seq();
        self.user_orders
            .entry(account)
            .or_default()
            .push(UserOrderRow {
                order_id,
                market_id,
                placed_ts,
                side,
                qty_requested,
                limit_price,
                source,
                order_type,
                filled_qty,
                notional_tenths,
                status,
                realized_pnl_tenths,
            });
    }

    /// Bump filled_qty / notional / realized PnL and transition status on
    /// the row for `(account, order_id)`. Called once per match from
    /// `execute_match`'s maker-side path. Silently does nothing if the
    /// row isn't tracked (bot accounts, or rows that pre-dated user
    /// registration).
    fn record_user_order_fill(
        &mut self,
        account: Address,
        order_id: OrderId,
        fill_qty: Qty,
        fill_price: PriceTick,
        realized_pnl_tenths: i64,
    ) {
        let Some(rows) = self.user_orders.get_mut(&account) else {
            return;
        };
        let Some(row) = rows.iter_mut().find(|r| r.order_id == order_id) else {
            return;
        };
        row.filled_qty = Qty(row.filled_qty.0 + fill_qty.0);
        row.notional_tenths += (fill_qty.0 as u64) * (fill_price.tenths() as u64);
        row.realized_pnl_tenths += realized_pnl_tenths;
        row.status = if row.filled_qty.0 >= row.qty_requested.0 {
            UserOrderStatus::Filled
        } else {
            UserOrderStatus::PartiallyFilled
        };
    }

    /// Mark a user's order as cancelled. Called from `cancel`. No-op
    /// for non-users or untracked ids.
    fn record_user_order_cancellation(&mut self, account: Address, order_id: OrderId) {
        let Some(rows) = self.user_orders.get_mut(&account) else {
            return;
        };
        if let Some(row) = rows.iter_mut().find(|r| r.order_id == order_id) {
            row.status = UserOrderStatus::Cancelled;
        }
    }

    fn record_fills(&mut self, ledger: &Ledger, fills: &[Fill]) {
        for f in fills {
            // On a mirrored series the tape and the round's numbers are
            // the venue's prints (`record_print`); a fill made here only
            // counts its traders, and the mirror is not one.
            if self.mirror.is_none() {
                self.push_to_tape(*f);
            }
            // Count by root identity so a user's strategy sub-accounts
            // don't inflate the unique-trader figure.
            for party in [f.taker, f.maker] {
                if Some(party) != self.mirror {
                    self.market_unique_traders.insert(ledger.root_of(&party));
                }
            }
        }
    }

    /// One trade onto the public tape and into the round's volume, trade
    /// count and largest trade.
    fn push_to_tape(&mut self, f: Fill) {
        if self.recent_fills.len() == RECENT_FILLS_CAP {
            self.recent_fills.pop_front();
        }
        self.recent_fills.push_back(f);

        // Per-market stat counters. `recent_fills` is a rolling 1000-cap
        // buffer shared across markets, so totals must be tracked here
        // rather than re-derived on read.
        self.market_volume_tenths += f.qty.0 as i64 * PAYOUT_PER_CONTRACT;
        self.market_trade_count += 1;
        match self.market_largest_trade {
            // Tie → most recent wins (≥, not >). Tests rely on this.
            Some(prev) if prev.qty.0 > f.qty.0 => {}
            _ => self.market_largest_trade = Some(f),
        }
    }

    /// Places a limit order. Crosses against opposing liquidity at prices
    /// equal to or better than `price`, then rests any unfilled remainder
    /// at `price`.
    ///
    /// `source` is the internal origin tag (UI / strategy / bot). It's
    /// carried through to user-trade recording for any fills this call
    /// produces, and stamped on the resting residual so maker-side fills
    /// can be attributed correctly later.
    pub fn place_limit(
        &mut self,
        ledger: &mut Ledger,
        account: Address,
        side: Side,
        price: PriceTick,
        qty: Qty,
        source: TradeSource,
    ) -> Result<PlaceOutcome, OrderError> {
        self.place_limit_with(ledger, account, side, price, qty, source, PlaceOpts::default())
    }

    /// [`place_limit`](Self::place_limit) with per-order [`PlaceOpts`].
    /// For a client-id order every outcome is also reported on the
    /// execution stream (see [`super::events`]).
    #[allow(clippy::too_many_arguments)]
    pub fn place_limit_with(
        &mut self,
        ledger: &mut Ledger,
        account: Address,
        side: Side,
        price: PriceTick,
        qty: Qty,
        source: TradeSource,
        opts: PlaceOpts,
    ) -> Result<PlaceOutcome, OrderError> {
        let result = self.place_limit_inner(ledger, account, side, price, qty, source, &opts);
        self.report_taker(&opts, account, qty, &result);
        result
    }

    #[allow(clippy::too_many_arguments)]
    fn place_limit_inner(
        &mut self,
        ledger: &mut Ledger,
        account: Address,
        side: Side,
        price: PriceTick,
        qty: Qty,
        source: TradeSource,
        opts: &PlaceOpts,
    ) -> Result<PlaceOutcome, OrderError> {
        if self.is_locked() {
            return Err(OrderError::MarketClosed);
        }
        if qty.is_zero() {
            return Err(OrderError::ZeroQuantity);
        }
        ledger.account(&account)
            .ok_or(OrderError::UnknownAccount)?;
        ledger.assert_may_trade(&account, self.series)?;
        let qty = self.apply_reduce_only(opts, account, side, qty)?;
        if opts.post_only
            && let Some(head) = self.book.peek_best_head(side.opposite())
        {
            let crosses = match side {
                Side::Bid => head.price.tenths() <= price.tenths(),
                Side::Ask => head.price.tenths() >= price.tenths(),
            };
            if crosses {
                return Err(OrderError::PostOnlyWouldCross);
            }
        }

        let per_unit_lock = per_unit_lock_for_limit(side, price);
        let lock_total = Tenths(per_unit_lock.0 * qty.0 as i64);

        // Reserve the full worst-case lock up front — one atomic check-and-
        // move on the shared ledger; cross() refunds it per filled unit and
        // any residual stays reserved for the resting order.
        ledger.reserve(account, lock_total)?;
        self.locked_total += lock_total;
        let realized_before = self.position(&account).realized_pnl_this_market.0;

        let fills = self.cross(ledger, account, side, Some(price), per_unit_lock, qty);
        self.record_fills(ledger, &fills);
        let realized = self.realized_since(account, realized_before);
        let filled: u32 = fills.iter().map(|f| f.qty.0).sum();
        let remaining = Qty(qty.0 - filled);
        let notional: u64 = fills
            .iter()
            .map(|f| (f.qty.0 as u64) * (f.price.tenths() as u64))
            .sum();

        let order_id = self.next_id();
        let status = if remaining.is_zero() {
            OrderStatus::Filled
        } else if opts.ioc {
            // Immediate-or-cancel: kill the residual and refund its lock
            // instead of resting it. No cancel ack; the taker report
            // carries the outcome as one Partial / Expiry.
            let refund = Tenths(per_unit_lock.0 * remaining.0 as i64);
            ledger.release(account, refund);
            self.locked_total -= refund;
            OrderStatus::Killed
        } else if filled > 0 {
            self.rest_residual(order_id, account, side, price, remaining, per_unit_lock, source, opts.client_order_id.clone());
            OrderStatus::PartiallyFilled
        } else {
            self.rest_residual(order_id, account, side, price, remaining, per_unit_lock, source, opts.client_order_id.clone());
            OrderStatus::Rested
        };
        let resting_qty = if status == OrderStatus::Killed { Qty::ZERO } else { remaining };
        self.record_user_order_placement(ledger, 
            account,
            order_id,
            side,
            qty,
            Some(price.tenths()),
            source,
            OrderType::Limit,
            Qty(filled),
            notional,
            user_order_status_from(status),
            realized,
        );

        Ok(PlaceOutcome {
            order_id,
            fills,
            status,
            resting_qty,
        })
    }

    /// `reduce_only`: clamp `qty` to the position the order closes, or
    /// refuse when the order would open or extend exposure. A bid closes
    /// a short (negative position), an ask closes a long.
    fn apply_reduce_only(&self, opts: &PlaceOpts, account: Address, side: Side, qty: Qty) -> Result<Qty, OrderError> {
        if !opts.reduce_only {
            return Ok(qty);
        }
        let pos = self.position(&account).position.0;
        let closable = match side {
            Side::Bid => (-pos).max(0),
            Side::Ask => pos.max(0),
        };
        if closable == 0 {
            return Err(OrderError::ReduceOnlyNoPosition);
        }
        Ok(Qty(qty.0.min(closable as u32)))
    }

    /// Places an immediate-or-cancel market order. Crosses against any
    /// opposing liquidity; unfilled qty is discarded (status `Killed`).
    /// Errors with `NoLiquidity` if nothing on the opposite side at all.
    ///
    /// `source` is the internal origin tag (UI / strategy / bot); see
    /// [`Engine::place_limit`].
    pub fn place_market(
        &mut self,
        ledger: &mut Ledger,
        account: Address,
        side: Side,
        qty: Qty,
        source: TradeSource,
    ) -> Result<PlaceOutcome, OrderError> {
        self.place_market_with(ledger, account, side, qty, source, PlaceOpts::default())
    }

    /// [`place_market`](Self::place_market) with per-order [`PlaceOpts`].
    /// For a client-id order every outcome is also reported on the
    /// execution stream (see [`super::events`]).
    pub fn place_market_with(
        &mut self,
        ledger: &mut Ledger,
        account: Address,
        side: Side,
        qty: Qty,
        source: TradeSource,
        opts: PlaceOpts,
    ) -> Result<PlaceOutcome, OrderError> {
        let result = self.place_market_inner(ledger, account, side, qty, source, &opts);
        self.report_taker(&opts, account, qty, &result);
        result
    }

    fn place_market_inner(
        &mut self,
        ledger: &mut Ledger,
        account: Address,
        side: Side,
        mut qty: Qty,
        source: TradeSource,
        opts: &PlaceOpts,
    ) -> Result<PlaceOutcome, OrderError> {
        if self.is_locked() {
            return Err(OrderError::MarketClosed);
        }
        if qty.is_zero() {
            return Err(OrderError::ZeroQuantity);
        }
        let acct = ledger.account(&account)
            .ok_or(OrderError::UnknownAccount)?;
        ledger.assert_may_trade(&account, self.series)?;
        qty = self.apply_reduce_only(opts, account, side, qty)?;

        let (mut required, mut expected_fill) =
            self.simulate_market_required_cash(ledger, account, side, qty);
        if expected_fill.is_zero() {
            return Err(OrderError::NoLiquidity);
        }
        if acct.usdc_balance < required {
            qty = self.max_affordable_market_qty(ledger, account, side, qty);
            if qty.is_zero() {
                return Err(OrderError::InsufficientBalance {
                    required,
                    available: acct.usdc_balance,
                });
            }
            (required, expected_fill) = self.simulate_market_required_cash(ledger, account, side, qty);
            if expected_fill.is_zero() {
                return Err(OrderError::NoLiquidity);
            }
            if acct.usdc_balance < required {
                return Err(OrderError::InsufficientBalance {
                    required,
                    available: acct.usdc_balance,
                });
            }
        }

        // The precheck above walks the exact same opposing orders the cross
        // will see and proves the peak cash draw is affordable — but the
        // balance is shared with other series, so the peak is *reserved*
        // atomically for the duration of the cross and released in full
        // afterwards (per-fill cash lands on the balance directly). Net
        // effect on the balance is identical to an unreserved cross; the
        // reservation only exists to make two series racing for the same
        // dollar fail loudly instead of overdrawing.
        ledger.reserve(account, required)?;
        self.locked_total += required;
        let realized_before = self.position(&account).realized_pnl_this_market.0;
        let fills = self.cross(ledger, account, side, None, Tenths::ZERO, qty);
        ledger.release(account, required);
        self.locked_total -= required;
        self.record_fills(ledger, &fills);
        let realized = self.realized_since(account, realized_before);
        let filled: u32 = fills.iter().map(|f| f.qty.0).sum();
        let remaining = Qty(qty.0 - filled);
        let notional: u64 = fills
            .iter()
            .map(|f| (f.qty.0 as u64) * (f.price.tenths() as u64))
            .sum();

        let order_id = self.next_id();
        let status = if remaining.is_zero() {
            OrderStatus::Filled
        } else {
            OrderStatus::Killed
        };
        self.record_user_order_placement(ledger, 
            account,
            order_id,
            side,
            qty,
            None,
            source,
            OrderType::Market,
            Qty(filled),
            notional,
            user_order_status_from(status),
            realized,
        );

        Ok(PlaceOutcome {
            order_id,
            fills,
            status,
            resting_qty: Qty::ZERO,
        })
    }

    /// Report a taker-side placement on the execution stream: a `Reject`
    /// on error; otherwise one `Partial` per fill price in book order, the
    /// last upgraded to `Fill` when nothing remains, with `remaining` set
    /// to zero when the residual was killed; `Expiry` when nothing filled
    /// and nothing rests; nothing at all for a pure rest.
    fn report_taker(&self, opts: &PlaceOpts, account: Address, requested: Qty, result: &Result<PlaceOutcome, OrderError>) {
        let Some(cid) = opts.client_order_id.clone() else { return };
        if self.executions.is_none() {
            return;
        }
        let out = match result {
            Ok(out) => out,
            Err(e) => {
                self.emit(cid, account, None, ExecKind::Reject((*e).into()), Qty::ZERO, requested, None);
                return;
            }
        };
        let rests = out.status == OrderStatus::Rested || out.status == OrderStatus::PartiallyFilled;
        if out.fills.is_empty() {
            if !rests {
                self.emit(cid, account, Some(out.order_id), ExecKind::Expiry, Qty::ZERO, Qty::ZERO, None);
            }
            return;
        }
        // Group consecutive fills at one price (a single match splits into
        // up to four classified sub-fills at the same price).
        let mut groups: Vec<(PriceTick, u32)> = Vec::new();
        for f in &out.fills {
            match groups.last_mut() {
                Some((p, q)) if *p == f.price => *q += f.qty.0,
                _ => groups.push((f.price, f.qty.0)),
            }
        }
        let filled_total: u32 = groups.iter().map(|(_, q)| q).sum();
        let mut filled_so_far = 0u32;
        let n = groups.len();
        for (i, (price, q)) in groups.into_iter().enumerate() {
            filled_so_far += q;
            let last = i + 1 == n;
            let remaining = if last {
                if rests { out.resting_qty } else { Qty::ZERO }
            } else {
                Qty(filled_total - filled_so_far + out.resting_qty.0)
            };
            let kind = if last && out.resting_qty.is_zero() && out.status == OrderStatus::Filled {
                ExecKind::Fill
            } else {
                ExecKind::Partial
            };
            self.emit(cid.clone(), account, Some(out.order_id), kind, Qty(q), remaining, Some(price));
        }
    }

    /// Returns the maximum positive cash draw needed to execute this IOC
    /// market order against the current book, plus the expected fill size.
    /// Self orders are skipped because the real matcher cancels them before
    /// continuing. Closing exposure can generate cash before new exposure is
    /// opened, so we track the peak required cash rather than a crude
    /// worst-case $1/share reservation.
    fn simulate_market_required_cash(
        &self,
        ledger: &Ledger,
        account: Address,
        side: Side,
        qty: Qty,
    ) -> (Tenths, Qty) {
        if ledger.account(&account).is_none() {
            return (Tenths::ZERO, Qty::ZERO);
        }

        let mut pos = self.position(&account).position.0;
        let mut remaining = qty.0 as i64;
        let mut running_cash_delta = 0_i64;
        let mut min_cash_delta = 0_i64;
        let mut filled = 0_u32;

        let orders: Box<dyn Iterator<Item = Order> + '_> = match side {
            Side::Bid => Box::new(self.book.iter_ask_orders()),
            Side::Ask => Box::new(self.book.iter_bid_orders()),
        };

        for maker in orders {
            if remaining == 0 {
                break;
            }
            if maker.account == account {
                continue;
            }

            let q = remaining.min(maker.qty_open.0 as i64);
            let p = maker.price.tenths() as i64;

            match side {
                Side::Bid => {
                    let close_short = q.min((-pos).max(0));
                    let open_long = q - close_short;
                    running_cash_delta += close_short * (PAYOUT_PER_CONTRACT - p) - open_long * p;
                    pos += q;
                }
                Side::Ask => {
                    let close_long = q.min(pos.max(0));
                    let open_short = q - close_long;
                    running_cash_delta += close_long * p - open_short * (PAYOUT_PER_CONTRACT - p);
                    pos -= q;
                }
            }

            min_cash_delta = min_cash_delta.min(running_cash_delta);
            remaining -= q;
            filled += q as u32;
        }

        (Tenths(-min_cash_delta), Qty(filled))
    }

    fn max_affordable_market_qty(&self, ledger: &Ledger, account: Address, side: Side, desired: Qty) -> Qty {
        let Some(acct) = ledger.account(&account) else {
            return Qty::ZERO;
        };
        if desired.is_zero() {
            return Qty::ZERO;
        }

        let mut lo = 0_u32;
        let mut hi = desired.0;
        let mut best = 0_u32;

        while lo <= hi {
            let mid = lo + (hi - lo) / 2;
            let probe = Qty(mid);
            let (required, expected_fill) =
                self.simulate_market_required_cash(ledger, account, side, probe);
            let can_execute = expected_fill == probe && acct.usdc_balance >= required;

            if can_execute {
                best = mid;
                lo = mid.saturating_add(1);
            } else if mid == 0 {
                break;
            } else {
                hi = mid - 1;
            }
        }

        Qty(best)
    }

    /// Current USD value of an open position in tenths. Uses the live book
    /// mid when available, falling back to exact open cost basis when the
    /// book is one-sided/empty. This is the target notional for flips.
    fn position_target_notional(&self, ledger: &Ledger, account: Address) -> Option<Tenths> {
        ledger.account(&account)?;
        let ps = self.position(&account);
        let signed = ps.position.0;
        if signed == 0 {
            return Some(Tenths::ZERO);
        }

        if let Some(mark) = self.book.mid_yes_tenths() {
            let mark = mark as i64;
            let value = if signed > 0 {
                signed * mark
            } else {
                (-signed) * (PAYOUT_PER_CONTRACT - mark)
            };
            return Some(Tenths(value));
        }

        Some(Tenths(ps.cost_basis()))
    }

    /// Maximum opening qty that can be bought/sold with `budget` after
    /// ignoring the first `skip_qty` executable contracts on this same side.
    /// The skip models the close leg of a flip, so the returned qty is sized
    /// against the liquidity left for the new opposite exposure.
    fn market_open_qty_for_budget_after_skip(
        &self,
        account: Address,
        side: Side,
        budget: Tenths,
        skip_qty: Qty,
    ) -> Qty {
        if budget.0 <= 0 {
            return Qty::ZERO;
        }

        let mut budget_left = budget.0;
        let mut skip_left = skip_qty.0 as i64;
        let mut qty = 0_u32;
        let orders: Box<dyn Iterator<Item = Order> + '_> = match side {
            Side::Bid => Box::new(self.book.iter_ask_orders()),
            Side::Ask => Box::new(self.book.iter_bid_orders()),
        };

        for maker in orders {
            if maker.account == account {
                continue;
            }

            let mut available = maker.qty_open.0 as i64;
            if skip_left > 0 {
                let skipped = available.min(skip_left);
                available -= skipped;
                skip_left -= skipped;
            }
            if available <= 0 {
                continue;
            }

            let p = maker.price.tenths() as i64;
            let unit_cost = match side {
                Side::Bid => p,
                Side::Ask => PAYOUT_PER_CONTRACT - p,
            };
            if unit_cost <= 0 {
                continue;
            }

            let affordable = (budget_left / unit_cost).min(available);
            if affordable <= 0 {
                break;
            }
            qty = qty.saturating_add(affordable as u32);
            budget_left -= affordable * unit_cost;
            if affordable < available {
                break;
            }
        }

        Qty(qty)
    }

    /// Flip the caller's current position into the opposite side using the
    /// current marked USD value as the target opening notional. Example: a
    /// $100 NO position closes, then opens roughly $100 of YES exposure.
    ///
    /// `source` is forwarded to both internal market orders (close +
    /// open legs).
    pub fn flip_position(
        &mut self,
        ledger: &mut Ledger,
        account: Address,
        source: TradeSource,
    ) -> Result<FlipOutcome, OrderError> {
        ledger.account(&account)
            .ok_or(OrderError::UnknownAccount)?;
        ledger.assert_may_trade(&account, self.series)?;
        let signed = self.position(&account).position.0;
        if signed == 0 {
            return Err(OrderError::ZeroQuantity);
        }

        let close_qty = Qty(signed.unsigned_abs() as u32);
        let side = if signed > 0 { Side::Ask } else { Side::Bid };
        let target_notional = self
            .position_target_notional(ledger, account)
            .expect("account checked above");

        // Avoid mutating into a half-closed state when there is not enough
        // liquidity to flatten the old leg.
        let (_, close_expected) = self.simulate_market_required_cash(ledger, account, side, close_qty);
        if close_expected < close_qty {
            return Err(OrderError::NoLiquidity);
        }

        let desired_open_qty =
            self.market_open_qty_for_budget_after_skip(account, side, target_notional, close_qty);
        if desired_open_qty.is_zero() {
            return Err(OrderError::NoLiquidity);
        }

        let desired_total_qty = close_qty + desired_open_qty;
        let affordable_total_qty =
            self.max_affordable_market_qty(ledger, account, side, desired_total_qty);
        if affordable_total_qty <= close_qty {
            return Err(OrderError::NoLiquidity);
        }
        let open_qty = affordable_total_qty - close_qty;

        let close = self.place_market(ledger, account, side, close_qty, source)?;
        let open = self.place_market(ledger, account, side, open_qty, source)?;

        Ok(FlipOutcome {
            target_notional_tenths: target_notional,
            close,
            open,
        })
    }

    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_arguments)]
    fn rest_residual(
        &mut self,
        order_id: OrderId,
        account: Address,
        side: Side,
        price: PriceTick,
        qty: Qty,
        per_unit_lock: Tenths,
        source: TradeSource,
        client_order_id: Option<Arc<str>>,
    ) {
        if let Some(cid) = client_order_id {
            self.client_ids.insert(order_id, cid);
        }
        let ts = self.next_seq();
        self.book.insert_resting(Order {
            id: order_id,
            account,
            side,
            price,
            qty_open: qty,
            ts,
            source,
        });
        self.order_locks.insert(order_id, per_unit_lock);
    }

    /// Walks the opposite side of the book, matching until the taker's qty
    /// is exhausted or the price condition (for limits) is no longer met.
    /// Releases the taker's reserved lock per filled unit and applies all
    /// per-side cash flow and position updates.
    ///
    /// Per-user order rows for the taker side are written by the caller
    /// (`place_limit` / `place_market`) once the fills are summed;
    /// per-user maker rows are updated inline in `execute_match` so the
    /// resting order id is still in scope.
    fn cross(
        &mut self,
        ledger: &mut Ledger,
        taker: Address,
        side: Side,
        limit_price: Option<PriceTick>,
        taker_per_unit_lock: Tenths,
        mut remaining: Qty,
    ) -> Vec<Fill> {
        let opposite = side.opposite();
        let mut fills = Vec::new();

        while !remaining.is_zero() {
            let Some(head) = self.book.peek_best_head(opposite) else {
                break;
            };

            if let Some(limit) = limit_price {
                let crosses = match side {
                    Side::Bid => head.price.tenths() <= limit.tenths(),
                    Side::Ask => head.price.tenths() >= limit.tenths(),
                };
                if !crosses {
                    break;
                }
            }

            // Self-trade prevention: cancel the resting maker and continue.
            if head.account == taker {
                let _ = self.cancel(ledger, taker, head.id);
                continue;
            }

            let match_qty = Qty(remaining.0.min(head.qty_open.0));
            self.execute_match(ledger, 
                taker,
                side,
                head,
                match_qty,
                taker_per_unit_lock,
                &mut fills,
            );
            remaining = Qty(remaining.0 - match_qty.0);
        }

        fills
    }

    #[allow(clippy::too_many_arguments)]
    fn execute_match(
        &mut self,
        ledger: &mut Ledger,
        taker: Address,
        taker_side: Side,
        maker_head: Order,
        qty: Qty,
        taker_per_unit_lock: Tenths,
        fills: &mut Vec<Fill>,
    ) {
        let price = maker_head.price;
        let p = price.tenths() as i64;
        let q = qty.0 as i64;

        let maker_lock = *self
            .order_locks
            .get(&maker_head.id)
            .expect("resting order must have a lock entry");

        let (buyer_addr, seller_addr, buyer_lock, seller_lock) = match taker_side {
            Side::Bid => (taker, maker_head.account, taker_per_unit_lock, maker_lock),
            Side::Ask => (maker_head.account, taker, maker_lock, taker_per_unit_lock),
        };

        let buyer_pos_pre = self.position(&buyer_addr).position.0;
        let seller_pos_pre = self.position(&seller_addr).position.0;

        // Per-side close/open split (in units of the fill).
        let a = q.min((-buyer_pos_pre).max(0)); // buyer's exit-short qty
        let b = q - a; // buyer's enter-long qty
        let c = q.min(seller_pos_pre.max(0)); // seller's exit-long qty
        let d = q - c; // seller's enter-short qty

        // Cross-tabulation into the four classified sub-fills.
        let close = a.min(c);
        let transfer_no = a - close;
        let transfer_yes = c - close;
        let open = q - a.max(c);

        // Buyer side: refund full reserved lock for these q units, then apply
        // cash flow (a units exit short, b units enter long), then position.
        // Seller side mirrors it: c units exit long, d units enter short.
        let buyer_refund = Tenths(buyer_lock.0 * q);
        let seller_refund = Tenths(seller_lock.0 * q);
        let buyer_cash = Tenths(a * (PAYOUT_PER_CONTRACT - p) - b * p);
        let seller_cash = Tenths(c * p - d * (PAYOUT_PER_CONTRACT - p));
        ledger.apply_fill_cash(
            buyer_addr,
            buyer_refund,
            buyer_cash,
            seller_addr,
            seller_refund,
            seller_cash,
        );
        self.locked_total -= buyer_refund + seller_refund;

        let buyer_realized = {
            let ps = self.position_mut(buyer_addr);
            let r = apply_pnl_for_fill(ps, buyer_pos_pre, q, p);
            ps.position += Position(q);
            r
        };
        let seller_realized = {
            let ps = self.position_mut(seller_addr);
            let r = apply_pnl_for_fill(ps, seller_pos_pre, -q, p);
            ps.position += Position(-q);
            r
        };
        let maker_realized = match taker_side {
            Side::Bid => seller_realized,
            Side::Ask => buyer_realized,
        };

        // OI moves by Open count minus Close count = q − a − c.
        let oi_delta = q - a - c;
        self.open_interest = (self.open_interest as i64 + oi_delta) as u64;

        // Emit one Fill per non-zero classification, in OI-impact order.
        let maker_addr = maker_head.account;
        for (kind, k) in [
            (FillKind::Open, open),
            (FillKind::TransferYes, transfer_yes),
            (FillKind::TransferNo, transfer_no),
            (FillKind::Close, close),
        ] {
            if k > 0 {
                let ts = self.next_seq();
                fills.push(Fill {
                    taker,
                    maker: maker_addr,
                    taker_side,
                    price,
                    qty: Qty(k as u32),
                    ts,
                    kind,
                });
            }
        }

        // Maker-side user-order row: one update for the whole match (all
        // `q` units at one price, plus whatever this match realized) while
        // the resting order id is still in scope. Taker-side rows are
        // written once at the end of `place_limit` / `place_market` from
        // the aggregate fill list.
        if ledger.is_user_account(&maker_addr) {
            self.record_user_order_fill(maker_addr, maker_head.id, qty, price, maker_realized);
        }

        // Maker-side execution event, while the resting id is in scope.
        if let Some(cid) = self.client_ids.get(&maker_head.id).cloned() {
            let remaining = Qty(maker_head.qty_open.0 - qty.0);
            let kind = if remaining.is_zero() { ExecKind::Fill } else { ExecKind::Partial };
            self.emit(cid, maker_addr, Some(maker_head.id), kind, qty, remaining, Some(price));
        }

        // Reduce the head; if fully consumed, drop its lock entry.
        if self
            .book
            .reduce_or_remove_head(taker_side.opposite(), qty)
            .is_some()
        {
            self.order_locks.remove(&maker_head.id);
            self.client_ids.remove(&maker_head.id);
        }
    }

    /// Cancels a resting order owned by `account`. Refunds the order's
    /// remaining locked collateral. A client-id order gets a `CancelAck`.
    pub fn cancel(&mut self, ledger: &mut Ledger, account: Address, id: OrderId) -> Result<Order, CancelError> {
        self.remove_resting(ledger, account, id, ExecKind::CancelAck)
    }

    /// [`cancel`](Self::cancel) reported as an `Expiry` instead of a
    /// `CancelAck`: the venue adapter's good-till-time sweep uses it so
    /// the runtime follows the intent's `on.expiry` transition.
    pub fn expire(&mut self, ledger: &mut Ledger, account: Address, id: OrderId) -> Result<Order, CancelError> {
        self.remove_resting(ledger, account, id, ExecKind::Expiry)
    }

    /// Atomic cancel-then-place: the resting order `old_id` (owned by
    /// `account`) comes off the book reported as `Replaced`, then the new
    /// limit is placed under `opts` and reported like any other placement.
    /// Both halves run under one `&mut self`, so nothing can interleave.
    /// An unknown or foreign `old_id` fails before anything changes.
    #[allow(clippy::too_many_arguments)]
    pub fn cancel_replace(
        &mut self,
        ledger: &mut Ledger,
        account: Address,
        old_id: OrderId,
        side: Side,
        price: PriceTick,
        qty: Qty,
        source: TradeSource,
        opts: PlaceOpts,
    ) -> Result<PlaceOutcome, ReplaceError> {
        self.remove_resting(ledger, account, old_id, ExecKind::Replaced)
            .map_err(ReplaceError::Cancel)?;
        self.place_limit_with(ledger, account, side, price, qty, source, opts)
            .map_err(ReplaceError::Place)
    }

    /// Take a resting order off the book, refund its lock, and report it
    /// as `kind` if it carries a client id.
    pub(super) fn remove_resting(&mut self, ledger: &mut Ledger, account: Address, id: OrderId, kind: ExecKind) -> Result<Order, CancelError> {
        let resting = self.book.get_order(id).ok_or(CancelError::UnknownOrder)?;
        if resting.account != account {
            return Err(CancelError::NotOwner);
        }

        let per_unit_lock = self
            .order_locks
            .remove(&id)
            .expect("resting order must have a lock entry");
        let refund = Tenths(per_unit_lock.0 * resting.qty_open.0 as i64);
        ledger.release(account, refund);
        self.locked_total -= refund;

        if ledger.is_user_account(&account) {
            self.record_user_order_cancellation(account, id);
        }
        if let Some(cid) = self.client_ids.remove(&id) {
            self.emit(cid, account, Some(id), kind, Qty::ZERO, Qty::ZERO, None);
        }

        Ok(self.book.cancel(id).expect("book had the order"))
    }

    fn next_id(&mut self) -> OrderId {
        self.ids.next_order()
    }

    fn next_seq(&mut self) -> u64 {
        self.ids.next_seq()
    }
}


/// Project the engine's [`OrderStatus`] at placement time onto the
/// user-log [`UserOrderStatus`]. The two enums are intentionally
/// distinct: `OrderStatus` describes the matching outcome (taker side)
/// in one snapshot, while `UserOrderStatus` is a lifecycle that can
/// later transition (e.g. `Rested` → `PartiallyFilled` → `Filled`) as
/// maker-side fills arrive.
fn user_order_status_from(s: OrderStatus) -> UserOrderStatus {
    match s {
        OrderStatus::Filled => UserOrderStatus::Filled,
        OrderStatus::PartiallyFilled => UserOrderStatus::PartiallyFilled,
        OrderStatus::Rested => UserOrderStatus::Rested,
        OrderStatus::Killed => UserOrderStatus::Killed,
    }
}

fn count_filled_orders(rows: &[UserOrderRow]) -> u32 {
    rows.iter().filter(|r| r.filled_qty.0 > 0).count() as u32
}

/// Worst-case reservation for one unit of a limit order. A bid for one
/// share could end up Opening a long for the full price `p`; an ask could
/// end up Opening a short for `(1 - p) = 1000 - p` tenths. We do not apply
/// a close-discount here — the engine intentionally over-locks closing
/// orders to keep the model simple. With starting balances of $1000 this
/// is well within budget.
fn per_unit_lock_for_limit(side: Side, price: PriceTick) -> Tenths {
    match side {
        Side::Bid => Tenths(price.tenths() as i64),
        Side::Ask => Tenths(PAYOUT_PER_CONTRACT - price.tenths() as i64),
    }
}

/// Update `entry_cost_tenths` / `realized_pnl_this_market` for one party's
/// position change in a single fill.
///
/// `pos_pre` is the account's signed position before this fill; `delta` is
/// the signed share change (`+q` for the buyer, `-q` for the seller); `price`
/// is the YES-side fill price in tenths. Handles open-from-flat, open-more,
/// partial close, close-to-flat, and one-fill flip. Avg entry is derived from
/// `entry_cost_tenths / |pos|` so per-fill rounding stays under 1 tenth and
/// close-to-flat rolls any leftover into realized so `entry_cost` is exactly
/// zero whenever position is.
///
/// Returns the realized PnL (tenths, signed) this fill produced — non-zero
/// only on partial close, close-to-flat and flip — so callers can attribute
/// it to the order that triggered it.
fn apply_pnl_for_fill(acct: &mut PositionState, pos_pre: i64, delta: i64, price: i64) -> i64 {
    if delta == 0 {
        return 0;
    }
    let pos_post = pos_pre + delta;

    if pos_pre == 0 {
        // Pure open from flat.
        acct.entry_cost_tenths += Tenths(price * delta.abs());
        return 0;
    }

    if pos_post == 0 {
        // Close fully to flat. Roll the entire remaining entry_cost into
        // realized so the flat invariant (entry_cost == 0) holds exactly.
        let qty_closed = pos_pre.abs();
        let realized = pos_pre.signum() * (price * qty_closed - acct.entry_cost_tenths.0);
        acct.realized_pnl_this_market += Tenths(realized);
        acct.entry_cost_tenths = Tenths::ZERO;
        return realized;
    }

    if pos_pre.signum() == pos_post.signum() {
        if pos_post.abs() > pos_pre.abs() {
            // Open more, same direction.
            acct.entry_cost_tenths += Tenths(price * delta.abs());
            return 0;
        }
        // Partial close, same direction.
        let qty_closed = pos_pre.abs() - pos_post.abs();
        let avg = acct.entry_cost_tenths.0 / pos_pre.abs();
        let entry_released = avg * qty_closed;
        let realized = pos_pre.signum() * (price * qty_closed - entry_released);
        acct.realized_pnl_this_market += Tenths(realized);
        acct.entry_cost_tenths -= Tenths(entry_released);
        return realized;
    }

    // Flip: close the entire old leg, then open the new opposite leg at
    // this fill price. Rolling the full old entry_cost into realized keeps
    // the post-flip entry_cost exactly aligned with the new leg.
    let qty_closed = pos_pre.abs();
    let qty_opened = pos_post.abs();
    let realized = pos_pre.signum() * (price * qty_closed - acct.entry_cost_tenths.0);
    acct.realized_pnl_this_market += Tenths(realized);
    acct.entry_cost_tenths = Tenths(price * qty_opened);
    realized
}

impl MarketEngine {
    /// Cancel every resting order owned by `account`, refunding locks.
    pub fn cancel_resting_for(&mut self, ledger: &mut Ledger, account: Address) -> u32 {
        let ids: Vec<OrderId> = self
            .book
            .iter_bid_orders()
            .chain(self.book.iter_ask_orders())
            .filter(|o| o.account == account)
            .map(|o| o.id)
            .collect();
        let count = ids.len() as u32;
        for id in ids {
            self.cancel(ledger, account, id).expect("order listed on book");
        }
        count
    }

    /// Drop every per-series record this engine holds for `account`:
    /// its position entry and its order rows. The account must be flat.
    pub fn forget_account(&mut self, account: Address) {
        debug_assert_eq!(
            self.position(&account).position.0,
            0,
            "forget_account on open position"
        );
        self.positions.remove(&account);
        self.user_orders.remove(&account);
        self.settled_orders.remove(&account);
    }

    /// `true` iff `account` has any order resting in this series.
    pub fn has_resting_orders_for(&self, account: &Address) -> bool {
        self.book
            .iter_bid_orders()
            .chain(self.book.iter_ask_orders())
            .any(|o| o.account == *account)
    }

    /// Σ per-unit lock × open qty over the resting book — what
    /// `locked_total` must equal between calls.
    pub fn resting_lock_sum(&self) -> Tenths {
        let mut sum = 0i64;
        for o in self.book.iter_bid_orders().chain(self.book.iter_ask_orders()) {
            let per_unit = self.order_locks.get(&o.id).copied().unwrap_or(Tenths::ZERO);
            sum += per_unit.0 * o.qty_open.0 as i64;
        }
        Tenths(sum)
    }
}
