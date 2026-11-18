//! The venue seam between the runtime and the matching engine.
//!
//! The runtime speaks `Program` vocabulary: instruments as `<series>.current`,
//! outcomes `up`/`down`, sizes and prices as exact decimals in the strategy's
//! outcome frame. A [`Venue`] implementation translates that to engine calls
//! (snapping prices to the tick grid, complementing `down` orders onto the
//! YES book, flooring sizes) and reports order fates back as
//! [`ExecutionEvent`]s on a channel, keyed by `client_order_id`.
//!
//! Two implementations: [`EngineVenue`] (`engine.rs`) over the real
//! exchange, built from [`translate`] (the §3.5 table), [`SeriesMap`] and
//! [`EngineClock`]; and [`FakeVenue`], which records requests and lets tests
//! script fills. Both report events into the runtime manager's inbox through
//! a [`RuntimeHandle`], never a separate channel, so a settlement's cancel
//! acks are always ordered before the settlement notification.
//!
//! Prices in execution events are in the *quoted* outcome frame
//! (the series' quoted outcome, `series::is_quoted_outcome`); the runtime complements them
//! back for `down` orders before they reach fill ledgers and logs.

pub mod clock;
pub mod engine;
pub mod series_map;
pub mod translate;

pub use clock::EngineClock;
pub use engine::EngineVenue;
pub use series_map::SeriesMap;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use rust_decimal::Decimal;
use strategy_compiler::ir::{OrderType, Side, Tif};

use crate::automation::runtime::RuntimeHandle;
use crate::types::{Address, SeriesId};

pub type ClientOrderId = Arc<str>;

/// An order as the runtime hands it to the venue.
#[derive(Debug, Clone, PartialEq)]
pub struct VenueOrder {
    pub client_order_id: ClientOrderId,
    /// `<series>.current`; resolved to the open market at placement.
    pub instrument_id: String,
    pub outcome: Option<String>,
    pub side: Side,
    pub order_type: OrderType,
    pub tif: Tif,
    /// Tokens, in the strategy's outcome frame.
    pub size: Decimal,
    /// Probability price in the strategy's outcome frame; `None` for market.
    pub price: Option<Decimal>,
    pub post_only: bool,
    pub expire_after_ms: Option<u64>,
    pub reduce_only: bool,
}

impl VenueOrder {
    /// Whether the order can leave a resting remainder on the book.
    pub fn may_rest(&self) -> bool {
        self.order_type == OrderType::Limit && self.tif == Tif::Gtc
    }
}

/// The closed reject vocabulary (`venue/translate.rs` in the plan).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    MarketClosed,
    InsufficientFunds,
    InvalidOrder,
    NoLiquidity,
    EnvelopeViolation,
    PriceBand,
    InternalError,
}

impl RejectReason {
    pub fn as_str(self) -> &'static str {
        match self {
            RejectReason::MarketClosed => "market_closed",
            RejectReason::InsufficientFunds => "insufficient_funds",
            RejectReason::InvalidOrder => "invalid_order",
            RejectReason::NoLiquidity => "no_liquidity",
            RejectReason::EnvelopeViolation => "envelope_violation",
            RejectReason::PriceBand => "price_band",
            RejectReason::InternalError => "internal_error",
        }
    }
}

/// A submission the venue took.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Accepted {
    pub venue_order_id: u64,
    pub market_id: Option<u32>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ExecutionKind {
    Fill,
    Partial,
    Reject { reason: RejectReason, detail: String },
    CancelAck,
    Expiry,
    Replaced,
}

impl ExecutionKind {
    /// The `on` table key.
    pub fn table_key(&self) -> &'static str {
        match self {
            ExecutionKind::Fill => "fill",
            ExecutionKind::Partial => "partial",
            ExecutionKind::Reject { .. } => "reject",
            ExecutionKind::CancelAck => "cancel_ack",
            ExecutionKind::Expiry => "expiry",
            ExecutionKind::Replaced => "replaced",
        }
    }
}

/// One order-fate event. Emitted only for orders carrying a client id.
#[derive(Debug, Clone, PartialEq)]
pub struct ExecutionEvent {
    pub series: SeriesId,
    pub account: Address,
    pub client_order_id: ClientOrderId,
    pub venue_order_id: Option<u64>,
    pub kind: ExecutionKind,
    /// Quantity filled by this event.
    pub filled_qty: Decimal,
    /// Quantity still live after this event (0 when the order is dead).
    pub remaining_qty: Decimal,
    /// Fill price in the quoted (`up`) frame.
    pub price: Option<Decimal>,
    pub market_id: Option<u32>,
}

/// The sub-account as the venue sees it, quoted-outcome frame, decimals.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AccountView {
    /// Free cash (not counting collateral locked under resting orders).
    pub cash: Decimal,
    pub position_size: Decimal,
    pub position_avg_cost: Decimal,
    /// Realized PnL in the current window. What `platform.realized_pnl`
    /// reads; resets with the window like the rest of the session.
    pub realized_pnl: Decimal,
    /// Realized PnL over the sub-account's whole life, settled windows
    /// included. What the envelope's `loss_budget` is measured against:
    /// the budget is a hard stop on the deployment, not on one window.
    pub realized_pnl_lifetime: Decimal,
    pub unrealized_pnl: Decimal,
}

pub trait Venue: Send + Sync {
    fn submit(&self, series: SeriesId, account: Address, order: &VenueOrder) -> Result<Accepted, RejectReason>;
    fn cancel(&self, series: SeriesId, account: Address, venue_order_id: u64) -> Result<(), String>;
    /// Atomic cancel-then-place. `Replaced` is reported on the old order.
    fn cancel_replace(&self, series: SeriesId, account: Address, old_venue_order_id: u64, order: &VenueOrder) -> Result<Accepted, RejectReason>;
    fn account_view(&self, series: SeriesId, account: Address) -> AccountView;
}

// ───────────────────────────────── fake ─────────────────────────────────

#[derive(Debug, Clone)]
pub struct FakeLiveOrder {
    pub series: SeriesId,
    pub account: Address,
    pub order: VenueOrder,
    pub remaining: Decimal,
}

#[derive(Debug, Default)]
pub struct FakeState {
    next_id: u64,
    /// Every submission, in order, including replacements.
    pub submitted: Vec<(SeriesId, Address, VenueOrder)>,
    pub cancelled: Vec<u64>,
    pub replaced: Vec<(u64, VenueOrder)>,
    pub live: HashMap<u64, FakeLiveOrder>,
    pub views: HashMap<(SeriesId, Address), AccountView>,
    /// How many times `account_view` was read (each is an engine lock on
    /// the real venue).
    pub view_reads: u64,
    /// Reject the next submission with this reason.
    pub reject_next: Option<RejectReason>,
    pub market_id: Option<u32>,
}

/// A venue that records everything and fills only when told to.
pub struct FakeVenue {
    pub state: Mutex<FakeState>,
    runtime: RuntimeHandle,
}

impl FakeVenue {
    /// `runtime` receives the execution events.
    pub fn new(runtime: RuntimeHandle) -> Arc<FakeVenue> {
        Arc::new(FakeVenue { state: Mutex::new(FakeState { next_id: 1, market_id: Some(1), ..Default::default() }), runtime })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FakeState> {
        self.state.lock().expect("fake venue mutex poisoned")
    }

    pub fn set_view(&self, series: SeriesId, account: Address, view: AccountView) {
        self.lock().views.insert((series, account), view);
    }

    pub fn set_market(&self, market_id: Option<u32>) {
        self.lock().market_id = market_id;
    }

    pub fn reject_next(&self, reason: RejectReason) {
        self.lock().reject_next = Some(reason);
    }

    pub fn live_ids(&self) -> Vec<u64> {
        let mut ids: Vec<u64> = self.lock().live.keys().copied().collect();
        ids.sort();
        ids
    }

    pub fn submitted(&self) -> Vec<VenueOrder> {
        self.lock().submitted.iter().map(|(_, _, o)| o.clone()).collect()
    }

    fn emit(&self, ev: ExecutionEvent) {
        self.runtime.execution(ev);
    }

    /// Script a fill against a live order. A full fill removes it.
    pub fn fill(&self, venue_order_id: u64, qty: Decimal, price: Decimal) {
        let ev = {
            let mut s = self.lock();
            let Some(live) = s.live.get_mut(&venue_order_id) else { return };
            let qty = qty.min(live.remaining);
            live.remaining -= qty;
            let done = live.remaining.is_zero();
            let ev = ExecutionEvent {
                series: live.series,
                account: live.account,
                client_order_id: live.order.client_order_id.clone(),
                venue_order_id: Some(venue_order_id),
                kind: if done { ExecutionKind::Fill } else { ExecutionKind::Partial },
                filled_qty: qty,
                remaining_qty: live.remaining,
                price: Some(price),
                market_id: s.market_id,
            };
            if done {
                s.live.remove(&venue_order_id);
            }
            ev
        };
        self.emit(ev);
    }

    /// Script an expiry of a live order.
    pub fn expire(&self, venue_order_id: u64) {
        let ev = {
            let mut s = self.lock();
            let Some(live) = s.live.remove(&venue_order_id) else { return };
            ExecutionEvent {
                series: live.series,
                account: live.account,
                client_order_id: live.order.client_order_id,
                venue_order_id: Some(venue_order_id),
                kind: ExecutionKind::Expiry,
                filled_qty: Decimal::ZERO,
                remaining_qty: Decimal::ZERO,
                price: None,
                market_id: s.market_id,
            }
        };
        self.emit(ev);
    }

    /// Settlement: cancel every live order for `series` (emitting acks in
    /// submission order) and return the affected accounts.
    pub fn settle_series(&self, series: SeriesId) -> Vec<Address> {
        let (evs, accounts) = {
            let mut s = self.lock();
            let mut ids: Vec<u64> = s.live.iter().filter(|(_, l)| l.series == series).map(|(id, _)| *id).collect();
            ids.sort();
            let mut evs = Vec::new();
            let mut accounts = Vec::new();
            for id in ids {
                let live = s.live.remove(&id).expect("present");
                accounts.push(live.account);
                evs.push(ExecutionEvent {
                    series,
                    account: live.account,
                    client_order_id: live.order.client_order_id,
                    venue_order_id: Some(id),
                    kind: ExecutionKind::CancelAck,
                    filled_qty: Decimal::ZERO,
                    remaining_qty: Decimal::ZERO,
                    price: None,
                    market_id: s.market_id,
                });
            }
            (evs, accounts)
        };
        for ev in evs {
            self.emit(ev);
        }
        accounts
    }

    fn accept(&self, s: &mut FakeState, series: SeriesId, account: Address, order: &VenueOrder) -> Result<Accepted, RejectReason> {
        if let Some(reason) = s.reject_next.take() {
            return Err(reason);
        }
        if s.market_id.is_none() {
            return Err(RejectReason::MarketClosed);
        }
        let id = s.next_id;
        s.next_id += 1;
        s.submitted.push((series, account, order.clone()));
        s.live.insert(id, FakeLiveOrder { series, account, order: order.clone(), remaining: order.size });
        Ok(Accepted { venue_order_id: id, market_id: s.market_id })
    }
}

impl Venue for FakeVenue {
    fn submit(&self, series: SeriesId, account: Address, order: &VenueOrder) -> Result<Accepted, RejectReason> {
        let mut s = self.lock();
        self.accept(&mut s, series, account, order)
    }

    fn cancel(&self, series: SeriesId, account: Address, venue_order_id: u64) -> Result<(), String> {
        let ev = {
            let mut s = self.lock();
            let live = s.live.remove(&venue_order_id).ok_or_else(|| format!("order {venue_order_id} is not live"))?;
            s.cancelled.push(venue_order_id);
            ExecutionEvent {
                series,
                account,
                client_order_id: live.order.client_order_id,
                venue_order_id: Some(venue_order_id),
                kind: ExecutionKind::CancelAck,
                filled_qty: Decimal::ZERO,
                remaining_qty: Decimal::ZERO,
                price: None,
                market_id: s.market_id,
            }
        };
        self.emit(ev);
        Ok(())
    }

    fn cancel_replace(&self, series: SeriesId, account: Address, old: u64, order: &VenueOrder) -> Result<Accepted, RejectReason> {
        let (ev, accepted) = {
            let mut s = self.lock();
            let live = s.live.remove(&old).ok_or(RejectReason::InvalidOrder)?;
            s.replaced.push((old, order.clone()));
            let ev = ExecutionEvent {
                series,
                account,
                client_order_id: live.order.client_order_id,
                venue_order_id: Some(old),
                kind: ExecutionKind::Replaced,
                filled_qty: Decimal::ZERO,
                remaining_qty: Decimal::ZERO,
                price: None,
                market_id: s.market_id,
            };
            let accepted = self.accept(&mut s, series, account, order);
            (ev, accepted)
        };
        self.emit(ev);
        accepted
    }

    fn account_view(&self, series: SeriesId, account: Address) -> AccountView {
        let mut s = self.lock();
        s.view_reads += 1;
        s.views.get(&(series, account)).cloned().unwrap_or_default()
    }
}
