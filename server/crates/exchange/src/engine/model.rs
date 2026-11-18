//! Core domain types: orders, fills, accounts.
//!
//! These are pure data carriers. They do not mutate the book or the ledger —
//! the book module is responsible for level state, and the (future) engine is
//! responsible for collateral and position accounting.

use serde::{Deserialize, Serialize};

use super::Outcome;
use super::strategies::DeployedStrategy;
use crate::ticks::PriceTick;
use crate::types::{Address, MarketId, OrderId, Qty, SeriesId, Side, Tenths};

/// How an order was placed: taking whatever the book offers, or resting at
/// a limit price.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderType {
    Market,
    Limit,
}

/// Origin of an order placement, captured at the engine entry point.
///
/// Carried on resting orders so maker-side trade history can attribute
/// each fill back to how the resting limit was originally placed. Lives
/// entirely inside the server; HTTP clients never send this — handlers
/// stamp it themselves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TradeSource {
    /// Direct HTTP API call from a logged-in user (place / close / flip).
    Ui,
    /// Issued by the strategy executor on behalf of a registered user.
    Strategy,
    /// Originated by an internal bot (market-maker or trader population).
    Bot,
}

/// Lifecycle state of one user-placed order.
///
/// Maps engine outcomes onto a stable wire shape:
/// - `Filled` / `PartiallyFilled` / `Rested` / `Killed` come from
///   [`super::OrderStatus`] at placement time, possibly upgraded as
///   later fills land against a resting residual.
/// - `Cancelled` is set when the engine cancels a resting order (user
///   action, lock phase, or `cancel_all_resting`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserOrderStatus {
    /// Fully filled (taker side: at placement; maker side: over time).
    Filled,
    /// Some qty filled, more still resting on the book.
    PartiallyFilled,
    /// On the book, no fills yet.
    Rested,
    /// Market order partially or wholly killed (no resting residual).
    Killed,
    /// Resting order cancelled before fully filling.
    Cancelled,
}

/// One user order placement recorded for the per-user, single-market
/// trade log. Mutated in place as fills land against the order; status
/// transitions reflect the order's lifecycle from the user's point of
/// view, not the engine's matching unit. Cleared on
/// [`super::Engine::settle_all`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UserOrderRow {
    pub order_id: OrderId,
    pub market_id: MarketId,
    /// `next_seq` value captured at placement; used for newest-first
    /// ordering on read.
    pub placed_ts: u64,
    /// The user's order side. `Bid` = bought YES; `Ask` = sold YES.
    pub side: Side,
    pub qty_requested: Qty,
    /// Limit price in tenths-of-cent, or `None` for a market order.
    pub limit_price: Option<u16>,
    pub source: TradeSource,
    pub order_type: OrderType,
    pub filled_qty: Qty,
    /// Sum of `qty × price` (in their native integer units) across all
    /// fills landed against this order; divided by `filled_qty` on read
    /// to produce the VWAP fill price.
    pub notional_tenths: u64,
    pub status: UserOrderStatus,
    /// Realized PnL (tenths) attributed to this order: the sum of every
    /// close / partial-close / flip-close realization triggered by its
    /// fills. Zero for orders that only opened exposure. Signed.
    pub realized_pnl_tenths: i64,
}

/// A resting or in-flight order on the canonical YES book.
///
/// `qty_open` is the remaining unfilled size; matching reduces it in place.
/// `ts` is a strictly monotonic sequence number assigned at acceptance, used
/// to enforce price-time priority within a level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Order {
    pub id: OrderId,
    pub account: Address,
    pub side: Side,
    pub price: PriceTick,
    pub qty_open: Qty,
    pub ts: u64,
    /// Where the original placement came from. Carried on the resting
    /// order so maker-side user-trade rows can be attributed correctly
    /// when the limit eventually fills. Internal-only; not on the wire.
    #[serde(default = "default_order_source")]
    pub source: TradeSource,
}

fn default_order_source() -> TradeSource {
    TradeSource::Bot
}

/// Classification of a fill against the two parties' prior positions.
///
/// See the protocol doc: every fill is normalized as "buyer buys YES from
/// seller at price p", then bucketed into one of these four cases to drive
/// the collateral / open-interest update.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FillKind {
    /// Both sides take on new exposure. Open interest and locked collateral increase.
    Open,
    /// An existing YES position transfers from seller to buyer. OI unchanged.
    TransferYes,
    /// An existing NO (short) position transfers from buyer to seller. OI unchanged.
    TransferNo,
    /// A YES and NO position cancel out. OI and locked collateral decrease.
    Close,
}

/// A single match between a taker and a maker.
///
/// `taker_side` carries which side the taker's order was on so downstream
/// surfaces (activity feed, analytics) can label each fill as a YES or NO
/// trade from the taker's perspective without having to inspect position
/// deltas. `Bid` taker = bought YES; `Ask` taker = sold YES (≡ bought NO).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fill {
    pub taker: Address,
    pub maker: Address,
    pub taker_side: Side,
    pub price: PriceTick,
    pub qty: Qty,
    pub ts: u64,
    pub kind: FillKind,
}

/// One account's result for a resolved market, frozen in
/// [`super::Engine::settle_all`]. `pnl_tenths` is the full-round realized
/// PnL including settlement (every fill's cash flow plus the payout), so
/// it equals the account's cash change over the round net of transfers.
/// `trades` counts orders that got at least one fill.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarketResult {
    /// Series the resolved round belonged to. Defaults to the first series
    /// for payloads that predate multi-series.
    #[serde(default)]
    pub series: SeriesId,
    pub market_id: MarketId,
    pub outcome: Outcome,
    pub pnl_tenths: i64,
    pub trades: u32,
}

/// A trader's ledger row: cash, identity and lineage. Shared across every
/// market series; per-series exposure lives in
/// [`super::positions::PositionState`] inside each series' engine.
///
/// `usdc_balance` is freely spendable cash in tenths. `locked` is the
/// worst-case reservation held against the account's *resting* orders in
/// every series (refunded per unit as they fill or cancel).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Account {
    pub address: Address,
    pub username: Option<String>,
    pub color: Option<String>,
    pub emoji: Option<String>,
    pub usdc_balance: Tenths,
    pub locked: Tenths,
    /// The user's deployed strategy, if any. At most one per account;
    /// `deploy_strategy` replaces it. Read by the (future) executor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strategy: Option<DeployedStrategy>,
    /// Parent address when this account is a strategy sub-account. `None`
    /// for top-level users and bots. Sub-accounts never carry their own
    /// username/color/emoji; identity is resolved through the parent via
    /// [`super::Engine::root_of`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<Address>,
    /// Net cash the parent has put into this sub-account: the creation
    /// allocation plus every later top-up, minus every withdrawal. Only
    /// set on sub-accounts. Lifetime PnL of a strategy is
    /// `equity − net_contributed`, so this must move with every
    /// parent ↔ child transfer (see [`super::Engine::fund_sub_account`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub net_contributed: Option<Tenths>,
    /// Σ `MarketResult::pnl_tenths` over every resolved market in every
    /// series. Excludes in-progress rounds; add each series'
    /// `realized_pnl_this_market` + unrealized for a live figure.
    #[serde(default)]
    pub lifetime_realized_pnl_tenths: i64,
    /// Bankroll a bot account is reset to after every market. `None` for
    /// user accounts (their balances persist). Set by
    /// [`super::Engine::seed_bot_account`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_balance: Option<Tenths>,
    /// The one series this account is allowed to trade. `None` for a
    /// human's main account, which may hold positions in every series;
    /// `Some` for every bot, market maker and strategy sub-account. The
    /// engine rejects orders outside the home series.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home_series: Option<SeriesId>,
}

impl Account {
    /// New account at `address`, seeded with `starting_balance` of free USDC.
    pub fn new(address: Address, starting_balance: Tenths) -> Self {
        Self {
            address,
            username: None,
            color: None,
            emoji: None,
            usdc_balance: starting_balance,
            locked: Tenths::ZERO,
            strategy: None,
            parent: None,
            net_contributed: None,
            lifetime_realized_pnl_tenths: 0,
            reset_balance: None,
            home_series: None,
        }
    }

    /// `true` iff this account is a strategy sub-account.
    pub fn is_sub_account(&self) -> bool {
        self.parent.is_some()
    }
}

/// A registered user's cumulative trading record, snapshotted once per
/// market at resolution (never mid-round). Bots are excluded — only
/// accounts that placed at least one order in a round get a standing, and
/// it is updated solely inside [`super::Engine::settle_all`].
///
/// `cumulative_pnl_tenths` is Σ realized round PnL (settlement included)
/// over every participated round in every series, for the root and its
/// strategy sub-accounts together. `last_market_pnl_tenths` is the most
/// recently *participated* round's PnL.
#[derive(Debug, Clone, Copy, Default)]
pub struct LeaderboardStanding {
    pub cumulative_pnl_tenths: i64,
    pub last_market_pnl_tenths: i64,
    pub markets_played: u32,
    pub markets_won: u32,
    pub last_market_id: u32,
    /// Series of the round `last_market_id` belongs to.
    pub last_series: SeriesId,
}
