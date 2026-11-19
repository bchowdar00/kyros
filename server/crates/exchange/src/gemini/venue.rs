//! Real orders: the [`Venue`] that sends a strategy's orders to Gemini.
//!
//! The runtime's four calls answer at once. `submit` checks the order,
//! names it in the engine ([`Exchange::external_open`]) and hands a signed
//! socket the `order.place` frame; what Gemini then says about the order
//! arrives on the account's order stream and is applied through the engine
//! ([`Exchange::external_fill`], [`Exchange::external_close`]), so every
//! fill lands in the same ledger, journal and runtime inbox as a paper
//! fill, in the same order.
//!
//! **Every order is a buy.** An order on the engine's bid side buys Yes at
//! its price; an order on the ask side buys No at one minus its price.
//! Nothing is ever sold, so no order depends on what the Gemini account
//! happens to hold. The engine's single book nets the two: long five Yes
//! and then five No is flat here. Whether Gemini nets them is **not
//! established**: its positions example lists Yes and No on one contract
//! side by side. If it does not, a strategy that is flat here holds both
//! legs there, which pays exactly one dollar a pair at settlement whatever
//! happens, with the cash tied up until then; the comparison to make
//! against Gemini's positions is Yes minus No. A sandbox run settles it.
//!
//! **Nothing is retried blind.** A placement Gemini has not answered within
//! [`PLACE_TIMEOUT`], and every order that was open when the socket
//! dropped, is looked up over REST ([`OrderLookup`]) by its client order
//! id. So is one Gemini answered with a fault of its own (a 5xx), which
//! says nothing about whether it took the order; only a 4xx for an order
//! its stream has not reported is a refusal. Found: its fills are booked
//! and its state adopted. Missed by two lookups [`LOOKUP_RETRY`] apart:
//! it is reported refused. It is never
//! sent a second time, and a frame queued for a socket that has since
//! dropped is thrown away, never sent on the next one. Should Gemini
//! report on an order after it was reported refused, the order is
//! cancelled there, whatever it filled is booked while its round is still
//! the open one, and the server reports itself unfit until someone has
//! looked. What this cannot see: a fill on such an order that Gemini
//! reports while the socket is down. The order stream is not replayed, so
//! after any refusal of this kind the positions are to be compared with
//! Gemini's by hand.
//!
//! **A replace is a cancel, then a place.** The new order is held here
//! until Gemini confirms the old one cancelled; if the old one fills
//! completely first, or was never answered and is reported refused, the
//! new one is refused too, so a replace can never leave both live. An old
//! order cancelled after filling in part is replaced in full, as on the
//! paper book.
//!
//! **An order is paid for before it leaves.** The account's cash must
//! cover this order and its other open real orders, each counted for the
//! part that would open a position rather than close one.
//!
//! Not modelled: Gemini's fees (logged, not booked), market orders
//! (refused: Gemini takes limit orders), prices that are not a whole cent
//! (refused, never rounded), and what a fill reported after its round has
//! settled would mean (said on the health page and logged; the count of
//! divergences goes up once per order, not once per late report). An
//! acknowledged order that Gemini's list no longer shows, and that no
//! report has ended, stays open here until its round settles: its cash
//! stays reserved and a replacement held behind it waits.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::future::BoxFuture;
use futures::{SinkExt, StreamExt};
use gemini_private::auth::{ApiKey, Nonce};
use gemini_private::rest::{PrivateRest, RestOrder, RestOrderStatus};
use gemini_private::wire::{
    self, Inbound, OrderStatus, OrderUpdate, PlaceParams, Response, WireOutcome, WireSide, WireTif,
};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use strategy_compiler::ir::OrderType;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use crate::automation::venue::translate::{self, snap_price, tick_to_decimal};
use crate::automation::venue::{Accepted, AccountView, RejectReason, SeriesMap, Venue, VenueOrder, engine::account_view};
use crate::engine::{ExecKind, ExecReject, Exchange, ExternalError, MarketStatus};
use crate::ticks::PriceTick;
use crate::types::{Address, OrderId, Qty, SeriesId, Side, Tenths};

/// Placements one account may send to Gemini in any one second. Gemini
/// allows 600 private requests a minute for the whole key.
pub const MAX_REAL_ORDERS_PER_SEC: usize = 2;
/// How long a placement may go unanswered before it is looked up.
pub const PLACE_TIMEOUT: Duration = Duration::from_secs(5);
/// How long to wait before looking an order up again after a failed lookup.
pub const LOOKUP_RETRY: Duration = Duration::from_secs(3);
/// The most contracts one order may carry unless the host raises it. A
/// contract costs at most a dollar, so this is also a dollar ceiling.
pub const DEFAULT_MAX_ORDER_QTY: u32 = 25;
/// The order stream this venue reads: every order of the key's account.
pub const ORDER_STREAM: &str = "orders@account";
const TICK_EVERY: Duration = Duration::from_millis(250);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const TERMS_RETRY: Duration = Duration::from_secs(30);
/// How many orders reported refused are remembered, in case Gemini turns
/// out to have taken one.
const REFUSED_KEPT: usize = 256;
const OUTSIDE_USERNAME: &str = "gemini-fills";
const OUTSIDE_BANKROLL: Tenths = Tenths(1_000_000_000);

/// Gemini's contract for a series' open round, if it has one.
pub type SymbolFn = Arc<dyn Fn(SeriesId) -> Option<String> + Send + Sync>;

/// The REST reads the venue falls back on. A trait so tests need no HTTP.
pub trait OrderLookup: Send + Sync + 'static {
    /// Every order of this account Gemini knows on `symbol`: the open
    /// ones and the recently closed ones.
    fn orders(&self, symbol: String) -> BoxFuture<'static, Result<Vec<RestOrder>, String>>;
    /// Whether the account has accepted Gemini's current prediction-market
    /// terms. Orders are refused by Gemini until it has; this venue never
    /// accepts them itself.
    fn terms_accepted(&self) -> BoxFuture<'static, Result<bool, String>>;
}

impl OrderLookup for Arc<PrivateRest> {
    fn orders(&self, symbol: String) -> BoxFuture<'static, Result<Vec<RestOrder>, String>> {
        let rest = self.clone();
        Box::pin(async move {
            let mut all = rest.active_orders(&symbol).await.map_err(|e| format!("{e:?}"))?;
            all.extend(rest.order_history(&symbol, 200).await.map_err(|e| format!("{e:?}"))?);
            Ok(all)
        })
    }

    fn terms_accepted(&self) -> BoxFuture<'static, Result<bool, String>> {
        let rest = self.clone();
        Box::pin(async move { rest.terms_accepted().await.map_err(|e| format!("{e:?}")) })
    }
}

/// Decide from the host's variables whether this server sends real orders.
///
/// `Ok(None)` is paper. Real orders need all three: `KYROS_REAL_ORDERS=1`,
/// `GEMINI_ENV` set in so many words (the default environment is never
/// enough to spend money in), and a key. Anything half-set is an error,
/// so a typo stops the boot instead of quietly choosing.
pub fn real_orders_key(flag: Option<&str>, env_name: Option<&str>, key: Result<Option<ApiKey>, String>) -> Result<Option<ApiKey>, String> {
    match flag.map(str::trim) {
        None | Some("") | Some("0") => return Ok(None),
        Some("1") => {}
        Some(other) => return Err(format!("KYROS_REAL_ORDERS is {other:?}; it must be \"1\" or \"0\"")),
    }
    match env_name.map(str::trim) {
        Some("production") | Some("sandbox") => {}
        _ => return Err("KYROS_REAL_ORDERS=1 needs GEMINI_ENV set to \"sandbox\" or \"production\" in so many words".into()),
    }
    match key? {
        Some(key) => Ok(Some(key)),
        None => Err("KYROS_REAL_ORDERS=1 needs GEMINI_API_KEY and GEMINI_API_SECRET on the host".into()),
    }
}

/// The account that takes the other side of every fill Gemini reports in
/// `series`. Not a trader: it stands for everyone on Gemini's book.
pub const fn outside_address(series: SeriesId) -> Address {
    Address([0x6e, 0x02, series.0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0])
}

/// Give each of `series` its outside account. Before anything trades.
pub fn install_outside_accounts(exchange: &mut Exchange, series: impl IntoIterator<Item = SeriesId>) {
    for series in series {
        let address = outside_address(series);
        exchange.seed_bot_account_in(address, OUTSIDE_BANKROLL, OUTSIDE_USERNAME.to_string(), None, None, series);
        exchange.set_outside(series, address);
    }
}

/// The one account whose strategies may send real orders, from
/// `KYROS_REAL_OPERATOR`.
pub fn parse_operator(raw: Option<&str>) -> Result<Address, String> {
    let raw = raw.map(str::trim).unwrap_or("");
    if raw.is_empty() {
        return Err("KYROS_REAL_ORDERS=1 needs KYROS_REAL_OPERATOR: the address of the one account whose strategies may send real orders".into());
    }
    raw.parse::<Address>().map_err(|e| format!("KYROS_REAL_OPERATOR is not an account address: {e}"))
}

/// The most contracts one real order may carry, from `KYROS_REAL_MAX_ORDER`.
pub fn parse_max_order(raw: Option<&str>) -> Result<u32, String> {
    match raw.map(str::trim) {
        None | Some("") => Ok(DEFAULT_MAX_ORDER_QTY),
        Some(text) => match text.parse::<u32>() {
            Ok(n) if n > 0 => Ok(n),
            _ => Err(format!("KYROS_REAL_MAX_ORDER is {text:?}; it must be a whole number of contracts, 1 or more")),
        },
    }
}

/// What the health check and `/api/series` say about the order link.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct VenueStatus {
    pub connected: bool,
    /// Why orders are being refused, when they are.
    pub problem: Option<String>,
    pub live_orders: usize,
    pub placed: u64,
    pub fills: u64,
    /// Reports from Gemini the ledger could not follow. Should be zero.
    pub divergences: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// A replacement, waiting for the order it replaces to end.
    Held,
    /// Sent; Gemini has not said anything about it yet.
    Sent(Instant),
    /// Gemini has acknowledged it or reported on it.
    Acked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CancelState {
    No,
    /// Asked for, but Gemini's id for the order is not known yet.
    Wanted,
    Sent,
}

#[derive(Debug)]
struct Live {
    series: SeriesId,
    account: Address,
    client_id: String,
    symbol: String,
    /// Side on the engine's book of the quoted outcome.
    side: Side,
    /// Limit, in the quoted outcome's frame.
    limit: PriceTick,
    qty: Qty,
    booked: Qty,
    tif: WireTif,
    gemini_id: Option<u64>,
    stage: Stage,
    cancel: CancelState,
    /// A cancel we asked for because the order's time ran out.
    lapsed: bool,
    replaced_by: Option<OrderId>,
    trades: HashSet<u64>,
    lookup_due: Option<Instant>,
    lookup_running: bool,
    /// When a lookup first failed to find the order while it was unanswered.
    first_miss: Option<Instant>,
    /// The engine's id of the round the order was placed in.
    market: u32,
    /// The engine let go of the order (its round settled, or its account
    /// closed) while Gemini may still hold it.
    orphaned: bool,
}

/// An order reported refused because Gemini had no record of it. Kept so
/// that a late report on it is neither ignored nor left unhedged.
#[derive(Debug)]
struct Refused {
    series: SeriesId,
    account: Address,
    side: Side,
    limit: PriceTick,
    /// The engine's id of the round it was placed in. A fill is booked
    /// only while that round is still the open one.
    market: u32,
    qty: Qty,
    /// What Gemini has reported filled, booked or not.
    booked: Qty,
    trades: HashSet<u64>,
    /// Gemini has shown it took the order: said once.
    seen: bool,
    cancel_sent: bool,
}

#[derive(Debug, Clone, Copy)]
enum Request {
    Place(OrderId),
    Cancel(OrderId),
    Other,
}

#[derive(Default)]
struct State {
    connected: bool,
    terms_ok: bool,
    /// Why no order can be sent: the terms. Cleared when they are accepted.
    terms_problem: Option<String>,
    /// A report from Gemini the ledger could not follow. Stays until the
    /// server is restarted: someone has to look.
    divergence_problem: Option<String>,
    live: HashMap<OrderId, Live>,
    refused: HashMap<String, Refused>,
    refused_order: VecDeque<String>,
    /// Gemini's ids of refused orders it has reported on, to their client ids.
    refused_by_gemini: HashMap<u64, String>,
    by_client: HashMap<String, OrderId>,
    by_gemini: HashMap<u64, OrderId>,
    requests: HashMap<u64, Request>,
    next_request: u64,
    next_client: u64,
    submissions: HashMap<Address, VecDeque<Instant>>,
    placed: u64,
    fills: u64,
    divergences: u64,
    max_order_qty: u32,
    /// When set, only this account's strategy sub-accounts trade for real.
    operator: Option<Address>,
}

/// A lookup the socket task should start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LookupRequest {
    pub order: OrderId,
    pub symbol: String,
}

/// The venue. Cheap to share; build with [`GeminiVenue::new`] and give
/// its [`Driver`] to [`run`].
pub struct GeminiVenue {
    exchange: Arc<Mutex<Exchange>>,
    map: SeriesMap,
    symbols: SymbolFn,
    state: Mutex<State>,
    frames: mpsc::UnboundedSender<(u64, String)>,
    /// Counts sockets. A queued frame carries the number it was made
    /// under and is thrown away once that socket has dropped.
    generation: AtomicU64,
    /// Starts every client order id; differs on every boot so a restart
    /// can never reuse one.
    boot: String,
}

impl std::fmt::Debug for GeminiVenue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GeminiVenue").field("boot", &self.boot).finish_non_exhaustive()
    }
}

/// The receiving end the socket task needs.
pub struct Driver {
    pub venue: Arc<GeminiVenue>,
    frames: mpsc::UnboundedReceiver<(u64, String)>,
}

impl GeminiVenue {
    pub fn new(exchange: Arc<Mutex<Exchange>>, map: SeriesMap, symbols: SymbolFn, boot: impl Into<String>) -> (Arc<GeminiVenue>, Driver) {
        let (frames, rx) = mpsc::unbounded_channel();
        let venue = Arc::new(GeminiVenue { exchange, map, symbols, state: Mutex::new(State { max_order_qty: DEFAULT_MAX_ORDER_QTY, ..State::default() }), frames, generation: AtomicU64::new(0), boot: boot.into() });
        (venue.clone(), Driver { venue, frames: rx })
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("gemini venue mutex poisoned")
    }

    fn exchange(&self) -> std::sync::MutexGuard<'_, Exchange> {
        self.exchange.lock().expect("exchange mutex poisoned")
    }

    pub fn status(&self) -> VenueStatus {
        let s = self.state();
        VenueStatus {
            connected: s.connected,
            problem: s.divergence_problem.clone().or_else(|| s.terms_problem.clone()),
            live_orders: s.live.len(),
            placed: s.placed,
            fills: s.fills,
            divergences: s.divergences,
        }
    }

    fn send(&self, frame: String) {
        let _ = self.frames.send((self.generation.load(Ordering::SeqCst), frame));
    }

    /// The frame, unless it was queued for a socket that has since dropped.
    pub fn still_current(&self, (generation, frame): (u64, String)) -> Option<String> {
        if generation == self.generation.load(Ordering::SeqCst) {
            Some(frame)
        } else {
            tracing::warn!("a frame for gemini was thrown away: the socket it was made for has dropped");
            None
        }
    }

    // ───────────────────────── the runtime's side ─────────────────────────

    /// The order in engine terms and as Gemini wants it, or the local
    /// refusal. Reads the account's position only for `reduce_only`.
    fn translate(&self, series: SeriesId, account: Address, order: &VenueOrder) -> Result<(Side, PriceTick, Qty, WireTif), RejectReason> {
        if order.order_type != OrderType::Limit {
            return Err(RejectReason::InvalidOrder);
        }
        if self.map.instrument_series(&order.instrument_id) != Some(series) {
            return Err(RejectReason::InvalidOrder);
        }
        let eo = translate::to_engine(order)?;
        let tick = eo.price.ok_or(RejectReason::InvalidOrder)?;
        // The engine snaps a price to its nearest tick. Real money is
        // never rounded: the order must name the cent itself.
        let stated = translate::quoted_price(order).ok_or(RejectReason::InvalidOrder)?;
        if !on_cent_grid(tick) || tick_to_decimal(tick) != stated {
            return Err(RejectReason::PriceBand);
        }
        let mut qty = eo.qty;
        if eo.opts.reduce_only {
            let position = self.exchange().engine(series).position(&account).position.0;
            let closable = match eo.side {
                Side::Bid => (-position).max(0),
                Side::Ask => position.max(0),
            };
            qty = Qty(qty.0.min(u32::try_from(closable).unwrap_or(u32::MAX)));
            if qty.is_zero() {
                return Err(RejectReason::InvalidOrder);
            }
        }
        if qty.0 > self.state().max_order_qty {
            return Err(RejectReason::EnvelopeViolation);
        }
        let tif = if eo.opts.post_only {
            WireTif::Moc
        } else if eo.opts.ioc {
            WireTif::Ioc
        } else {
            WireTif::Gtc
        };
        Ok((eo.side, tick, qty, tif))
    }

    /// Name the order in the engine and remember it. Under the state lock.
    #[allow(clippy::too_many_arguments)]
    fn open(&self, s: &mut State, series: SeriesId, account: Address, order: &VenueOrder, side: Side, limit: PriceTick, qty: Qty, tif: WireTif, stage: Stage) -> Result<(OrderId, Option<u32>), RejectReason> {
        let now = Instant::now();
        let recent = s.submissions.entry(account).or_default();
        while recent.front().is_some_and(|t| now.duration_since(*t) >= Duration::from_secs(1)) {
            recent.pop_front();
        }
        if recent.len() >= MAX_REAL_ORDERS_PER_SEC {
            return Err(RejectReason::EnvelopeViolation);
        }
        if let Some(operator) = s.operator {
            // Real money: only the operator's own strategies, never a
            // visitor's and never a manual account.
            let parent = self.exchange().account(&account).and_then(|a| a.parent);
            if parent != Some(operator) {
                return Err(RejectReason::EnvelopeViolation);
            }
        }
        if !s.connected || !s.terms_ok {
            return Err(RejectReason::InternalError);
        }
        let symbol = (self.symbols)(series).ok_or(RejectReason::MarketClosed)?;
        let (id, market_id) = {
            let mut x = self.exchange();
            let market_id = match x.engine(series).current_market() {
                Some(m) if m.status == MarketStatus::Open => m.id.0,
                _ => return Err(RejectReason::MarketClosed),
            };
            // Nothing is reserved in the ledger for a real order, and a
            // fill Gemini reports is booked whatever it costs. So the
            // order is paid for here, together with the account's other
            // open real orders in this series.
            let cash = x.account(&account).map(|a| a.usdc_balance.0).unwrap_or(0);
            let position = x.engine(series).position(&account).position.0;
            let open_orders = s.live.values().filter(|l| l.account == account && l.series == series).map(|l| (l.side, l.limit, Qty(l.qty.0.saturating_sub(l.booked.0))));
            if cost_to_open(position, open_orders.chain([(side, limit, qty)])) > cash {
                return Err(RejectReason::InsufficientFunds);
            }
            let id = x.external_open(series, account, order.client_order_id.clone(), qty).map_err(translate::reject_reason)?;
            (id, market_id)
        };
        s.submissions.entry(account).or_default().push_back(now);
        s.next_client += 1;
        let client_id = format!("{}-{}", self.boot, s.next_client);
        s.by_client.insert(client_id.clone(), id);
        s.live.insert(
            id,
            Live {
                series,
                account,
                client_id,
                symbol,
                side,
                limit,
                qty,
                booked: Qty::ZERO,
                tif,
                gemini_id: None,
                stage,
                cancel: CancelState::No,
                lapsed: false,
                replaced_by: None,
                trades: HashSet::new(),
                lookup_due: None,
                lookup_running: false,
                first_miss: None,
                market: market_id,
                orphaned: false,
            },
        );
        Ok((id, Some(market_id)))
    }

    /// The `order.place` frame for a remembered order, marking it sent.
    fn place_frame(s: &mut State, id: OrderId, now: Instant) -> Option<String> {
        s.next_request += 1;
        let request = s.next_request;
        let live = s.live.get_mut(&id)?;
        live.stage = Stage::Sent(now);
        let (outcome, price) = match live.side {
            Side::Bid => (WireOutcome::Yes, tick_to_decimal(live.limit)),
            Side::Ask => (WireOutcome::No, Decimal::ONE - tick_to_decimal(live.limit)),
        };
        let params = PlaceParams {
            symbol: live.symbol.clone(),
            side: WireSide::Buy,
            tif: live.tif,
            price: price.normalize(),
            quantity: Decimal::from(live.qty.0),
            client_order_id: live.client_id.clone(),
            outcome,
        };
        s.requests.insert(request, Request::Place(id));
        s.placed += 1;
        Some(wire::place_request(request, &params))
    }

    /// Ask Gemini to cancel a remembered order, now or once its id is known.
    fn cancel_frame(s: &mut State, id: OrderId) -> Option<String> {
        let live = s.live.get_mut(&id)?;
        if live.cancel == CancelState::Sent {
            return None;
        }
        let (Some(gemini_id), true) = (live.gemini_id, s.connected) else {
            // No id to cancel by yet, or no socket to send it on: it goes
            // out when there is.
            live.cancel = CancelState::Wanted;
            if s.connected && live.stage == Stage::Acked && live.lookup_due.is_none() {
                // Accepted, but no report has named it. One may only be
                // late; if none comes, Gemini's list has the id.
                live.lookup_due = Some(Instant::now() + LOOKUP_RETRY);
            }
            return None;
        };
        live.cancel = CancelState::Sent;
        s.next_request += 1;
        let request = s.next_request;
        s.requests.insert(request, Request::Cancel(id));
        Some(wire::cancel_request(request, gemini_id))
    }

    /// A cancel from the runtime, or from the order's own timer.
    fn request_cancel(&self, id: OrderId, lapsed: bool) -> Result<(), String> {
        let mut s = self.state();
        let Some(live) = s.live.get_mut(&id) else {
            return Err(format!("order {} is not live", id.0));
        };
        live.lapsed |= lapsed;
        if live.stage == Stage::Held {
            // Never sent: it ends here.
            let kind = if lapsed { ExecKind::Expiry } else { ExecKind::CancelAck };
            for other in s.live.values_mut() {
                if other.replaced_by == Some(id) {
                    other.replaced_by = None;
                }
            }
            self.end(&mut s, id, kind);
            return Ok(());
        }
        if let Some(frame) = Self::cancel_frame(&mut s, id) {
            self.send(frame);
        }
        Ok(())
    }

    fn arm_expiry(self: &Arc<Self>, id: OrderId, after_ms: u64) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            tracing::warn!(order = id.0, "expire_after outside a Tokio runtime: not armed");
            return;
        };
        let venue = self.clone();
        handle.spawn(async move {
            tokio::time::sleep(Duration::from_millis(after_ms)).await;
            let _ = venue.request_cancel(id, true);
        });
    }

    // ───────────────────────── Gemini's side ─────────────────────────

    /// Forget an order and report its end through the engine. Returns the
    /// frame that sends its replacement, when it has one and `kind` lets
    /// the replacement go ahead.
    fn end(&self, s: &mut State, id: OrderId, kind: ExecKind) -> Option<String> {
        let live = s.live.remove(&id)?;
        s.by_client.remove(&live.client_id);
        if let Some(g) = live.gemini_id {
            s.by_gemini.remove(&g);
        }
        let kind = match (kind, live.replaced_by) {
            (ExecKind::CancelAck, Some(_)) => ExecKind::Replaced,
            (kind, _) => kind,
        };
        if kind != ExecKind::Fill {
            // A fill already went out from `external_fill`.
            self.exchange().external_close(live.series, id, kind);
        }
        let next = live.replaced_by?;
        if matches!(kind, ExecKind::Fill | ExecKind::Reject(ExecReject::VenueUnreachable)) {
            // The old order filled before its cancel landed, or Gemini
            // never answered for it and may yet turn out to hold it:
            // sending the new one as well could fill both.
            self.end(s, next, ExecKind::Reject(ExecReject::VenueRefused));
            return None;
        }
        if !self.exchange().external_is_live(live.series, next) {
            // The round the replacement was for is over; the engine has
            // already reported it cancelled. It is never sent.
            self.end(s, next, ExecKind::CancelAck);
            return None;
        }
        Self::place_frame(s, next, Instant::now())
    }

    /// Book `qty` of a fill at `price` (in the order's own frame; the
    /// limit when Gemini gave none). Returns whether the order is done.
    fn book(&self, s: &mut State, id: OrderId, qty: Decimal, price: Option<Decimal>) -> bool {
        let Some(live) = s.live.get_mut(&id) else { return false };
        let whole = Qty(qty.floor().to_u32().unwrap_or(0));
        if whole.is_zero() {
            return false;
        }
        let tick = price
            .map(|p| if live.side == Side::Bid { p } else { Decimal::ONE - p })
            .and_then(snap_price)
            .unwrap_or(live.limit);
        let better_than_limit = match live.side {
            Side::Bid => tick.tenths() <= live.limit.tenths(),
            Side::Ask => tick.tenths() >= live.limit.tenths(),
        };
        if !better_than_limit {
            tracing::warn!(order = id.0, fill = tick.tenths(), limit = live.limit.tenths(), "gemini reported a fill outside the order's limit; booked as reported");
        }
        let (series, side) = (live.series, live.side);
        match self.exchange().external_fill(series, id, side, tick, whole) {
            Ok(()) => {
                live.booked = Qty(live.booked.0 + whole.0);
                s.fills += 1;
                live.booked.0 >= live.qty.0
            }
            Err(e) => {
                // The engine has let go of the order (its round settled, or
                // its account was closed) and Gemini filled it anyway.
                s.divergences += 1;
                let detail = match e {
                    ExternalError::UnknownOrder => "the ledger no longer holds the order".to_string(),
                    ExternalError::Ledger(e) => e.to_string(),
                };
                tracing::error!(order = id.0, qty = whole.0, price = tick.tenths(), %detail, "a gemini fill could not be booked");
                s.divergence_problem = Some(format!("a Gemini fill of {} could not be booked: {detail}", whole.0));
                true
            }
        }
    }

    /// One message from Gemini's socket. Returns the frames to send back.
    pub fn on_inbound(&self, text: &str) -> Vec<String> {
        let mut out = Vec::new();
        let inbound = match wire::parse_inbound(text) {
            Ok(inbound) => inbound,
            Err(e) => {
                tracing::warn!(error = %e, "unreadable message on gemini's order socket");
                return out;
            }
        };
        let mut s = self.state();
        match inbound {
            Inbound::Other => {}
            Inbound::Response(r) => self.on_response(&mut s, r, &mut out),
            Inbound::OrderUpdate(u) => self.on_update(&mut s, u, &mut out),
        }
        out
    }

    fn on_response(&self, s: &mut State, r: Response, out: &mut Vec<String>) {
        match s.requests.remove(&r.id) {
            Some(Request::Place(id)) => {
                let Some(live) = s.live.get_mut(&id) else { return };
                let unanswered = matches!(live.stage, Stage::Sent(_));
                if r.status == 200 {
                    if unanswered {
                        live.stage = Stage::Acked;
                    }
                } else if unanswered && (400..500).contains(&r.status) {
                    tracing::warn!(order = id.0, status = r.status, code = ?r.error_code, msg = ?r.error_msg, "gemini refused an order");
                    out.extend(self.end(s, id, ExecKind::Reject(refusal(r.error_msg.as_deref()))));
                } else {
                    // A fault on Gemini's side says nothing about whether it
                    // took the order, and a refusal for an order its stream
                    // already reported cannot be believed over the stream.
                    // Its own list settles it; ending the order here could
                    // send a replacement beside one that is live.
                    tracing::warn!(order = id.0, status = r.status, code = ?r.error_code, msg = ?r.error_msg, "gemini's answer to an order settles nothing; looking it up");
                    live.lookup_due = Some(Instant::now());
                }
            }
            Some(Request::Cancel(id)) => {
                if r.status != 200
                    && let Some(live) = s.live.get_mut(&id)
                {
                    // Usually the order filled first and its fill is on the
                    // way. Look it up rather than guess.
                    tracing::warn!(order = id.0, status = r.status, msg = ?r.error_msg, "gemini refused a cancel; looking the order up");
                    live.cancel = CancelState::Wanted;
                    // After a wait: looked up at once, an order still open
                    // would be cancelled again at once, and refused again.
                    live.lookup_due = Some(Instant::now() + LOOKUP_RETRY);
                }
            }
            Some(Request::Other) | None => {
                if r.status != 200 {
                    tracing::warn!(status = r.status, code = ?r.error_code, msg = ?r.error_msg, "gemini refused a request on the order socket");
                }
            }
        }
    }

    fn on_update(&self, s: &mut State, u: OrderUpdate, out: &mut Vec<String>) {
        let known = u.client_order_id.as_deref().and_then(|c| s.by_client.get(c).copied()).or_else(|| s.by_gemini.get(&u.order_id).copied());
        let Some(id) = known else {
            self.on_refused_update(s, &u, out);
            return;
        };
        {
            let Some(live) = s.live.get_mut(&id) else { return };
            if live.gemini_id.is_none() {
                live.gemini_id = Some(u.order_id);
            }
            if matches!(live.stage, Stage::Sent(_)) {
                live.stage = Stage::Acked;
            }
        }
        s.by_gemini.insert(u.order_id, id);
        let says_filled = u.status == OrderStatus::Filled;
        match u.status {
            OrderStatus::New | OrderStatus::Open | OrderStatus::Modified => {
                if s.live.get(&id).is_some_and(|l| l.cancel == CancelState::Wanted) {
                    out.extend(Self::cancel_frame(s, id));
                }
            }
            OrderStatus::PartiallyFilled | OrderStatus::Filled => {
                let done = match (u.original_qty, u.remaining_qty) {
                    // Gemini's own totals for the order: book whatever part
                    // of them is not booked yet. The same report twice, or a
                    // fill already adopted from a lookup, then books nothing,
                    // and a fill that was never reported is caught up.
                    (Some(original), Some(remaining)) => self.book_up_to(s, id, Some(original - remaining), u.last_price),
                    // No totals: `Z` is this execution alone, once per trade.
                    _ => {
                        let fresh = match (u.trade_id, s.live.get_mut(&id)) {
                            (Some(t), Some(live)) => live.trades.insert(t),
                            _ => true,
                        };
                        fresh && self.book(s, id, u.executed_qty.unwrap_or(Decimal::ZERO), u.last_price)
                    }
                };
                if done {
                    out.extend(self.end(s, id, ExecKind::Fill));
                    return;
                }
                if says_filled && let Some(live) = s.live.get_mut(&id) {
                    // Gemini says it is all filled and we have booked less:
                    // a fill was missed. REST has the total.
                    live.lookup_due = Some(Instant::now());
                }
                if s.live.get(&id).is_some_and(|l| l.cancel == CancelState::Wanted) {
                    out.extend(Self::cancel_frame(s, id));
                }
            }
            OrderStatus::Canceled => {
                // On a cancel `Z` is everything the order ever filled.
                if self.book_up_to(s, id, u.executed_qty, u.last_price) {
                    out.extend(self.end(s, id, ExecKind::Fill));
                    return;
                }
                let Some(live) = s.live.get(&id) else { return };
                let kind = if u.reason.as_deref() == Some("MakerOrCancelWouldTake") {
                    ExecKind::Reject(ExecReject::PostOnlyWouldCross)
                } else if live.lapsed || matches!(live.tif, WireTif::Ioc | WireTif::Fok) {
                    ExecKind::Expiry
                } else {
                    ExecKind::CancelAck
                };
                out.extend(self.end(s, id, kind));
            }
            OrderStatus::Rejected => {
                tracing::warn!(order = id.0, reason = ?u.reason, "gemini rejected an order");
                out.extend(self.end(s, id, ExecKind::Reject(refusal(u.reason.as_deref()))));
            }
            OrderStatus::Other(_) => {}
        }
    }

    /// Gemini reports on an order this venue does not hold. If it is one
    /// that was reported refused, Gemini took it after all: it is cancelled
    /// there, whatever it filled is booked on the account, and the server
    /// says so until someone has looked.
    fn on_refused_update(&self, s: &mut State, u: &OrderUpdate, out: &mut Vec<String>) {
        let client_id = match u.client_order_id.as_deref() {
            Some(c) if s.refused.contains_key(c) => c.to_owned(),
            // Gemini does not always repeat the client id.
            _ => match s.refused_by_gemini.get(&u.order_id) {
                Some(c) => c.clone(),
                None => return,
            },
        };
        s.refused_by_gemini.insert(u.order_id, client_id.clone());
        let Some(refused) = s.refused.get_mut(&client_id) else { return };
        let booked = Decimal::from(refused.booked.0);
        let unbooked = match u.status {
            OrderStatus::PartiallyFilled | OrderStatus::Filled => match (u.original_qty, u.remaining_qty) {
                (Some(original), Some(remaining)) => original - remaining - booked,
                _ if u.trade_id.is_none_or(|t| refused.trades.insert(t)) => u.executed_qty.unwrap_or(Decimal::ZERO),
                _ => Decimal::ZERO,
            },
            OrderStatus::Canceled => u.executed_qty.unwrap_or(Decimal::ZERO) - booked,
            _ => Decimal::ZERO,
        };
        // Never more than the order was for, whatever the reports say.
        let room = refused.qty.0.saturating_sub(refused.booked.0);
        let qty = Qty(unbooked.max(Decimal::ZERO).floor().to_u32().unwrap_or(0).min(room));
        let (series, account, side, limit, market) = (refused.series, refused.account, refused.side, refused.limit, refused.market);
        let mut late = false;
        if !qty.is_zero() {
            refused.booked = Qty(refused.booked.0 + qty.0);
            let tick = u.last_price.map(|p| if side == Side::Bid { p } else { Decimal::ONE - p }).and_then(snap_price).unwrap_or(limit);
            let mut x = self.exchange();
            // Once its round has settled, the contract it filled has paid
            // out or expired, and the round now open is another one.
            let in_its_round = x.engine(series).current_market().map(|m| m.id.0) == Some(market);
            match in_its_round.then(|| x.apply_external_fill(series, account, side, tick, qty)) {
                Some(Ok(())) => s.fills += 1,
                refused_here => {
                    tracing::error!(%client_id, qty = qty.0, error = ?refused_here, "a fill on an order reported refused is in no round here");
                    late = true;
                }
            }
        }
        let live_there = matches!(u.status, OrderStatus::New | OrderStatus::Open | OrderStatus::Modified | OrderStatus::PartiallyFilled);
        // Gemini saying it rejected the order, or pulled it unfilled,
        // agrees with what was reported. Anything else does not.
        let took_it = live_there || !qty.is_zero() || u.status == OrderStatus::Filled;
        if took_it && !refused.seen {
            refused.seen = true;
            s.divergences += 1;
            tracing::error!(%client_id, gemini_id = u.order_id, status = ?u.status, "gemini reports on an order that was reported refused");
            s.divergence_problem = Some(format!("Gemini took order {client_id} after it was reported refused; it was cancelled there and any fill booked. Compare positions with Gemini before trusting them"));
        }
        if late {
            s.divergence_problem = Some(format!("Gemini filled {} of order {client_id} after its round settled; it had been reported refused and the fill is in no round here. Compare positions with Gemini", qty.0));
        }
        if live_there && !refused.cancel_sent && s.connected {
            refused.cancel_sent = true;
            s.next_request += 1;
            let request = s.next_request;
            s.requests.insert(request, Request::Other);
            out.push(wire::cancel_request(request, u.order_id));
        }
    }

    /// Book whatever part of a cumulative filled quantity is not booked
    /// yet. Returns whether that completed the order.
    fn book_up_to(&self, s: &mut State, id: OrderId, cumulative: Option<Decimal>, price: Option<Decimal>) -> bool {
        let Some(live) = s.live.get(&id) else { return false };
        let missing = cumulative.unwrap_or(Decimal::ZERO) - Decimal::from(live.booked.0);
        missing > Decimal::ZERO && self.book(s, id, missing, price)
    }

    /// The socket is up and subscribed. Returns the frames to send.
    pub fn on_connected(&self) -> Vec<String> {
        let mut s = self.state();
        s.connected = true;
        s.next_request += 1;
        let request = s.next_request;
        s.requests.insert(request, Request::Other);
        let mut frames = vec![wire::subscribe_request(request, &[ORDER_STREAM])];
        // Cancels asked for while there was no socket to send them on.
        let mut wanted: Vec<OrderId> = s.live.iter().filter(|(_, l)| l.cancel == CancelState::Wanted && l.gemini_id.is_some()).map(|(id, _)| *id).collect();
        wanted.sort();
        for id in wanted {
            frames.extend(Self::cancel_frame(&mut s, id));
        }
        frames
    }

    /// The socket is gone. Every order Gemini may know about is looked up.
    pub fn on_disconnected(&self) {
        let mut s = self.state();
        s.connected = false;
        // Whatever is still queued was made for the socket that just went.
        self.generation.fetch_add(1, Ordering::SeqCst);
        s.requests.clear();
        let now = Instant::now();
        for live in s.live.values_mut() {
            if live.stage != Stage::Held {
                live.lookup_due = Some(now);
                if live.cancel == CancelState::Sent {
                    live.cancel = CancelState::Wanted;
                }
            }
        }
    }

    /// Limit real orders to the strategies of one parent account.
    pub fn set_operator(&self, operator: Option<Address>) {
        self.state().operator = operator;
    }

    /// Change the most contracts one order may carry.
    pub fn set_max_order_qty(&self, contracts: u32) {
        self.state().max_order_qty = contracts;
    }

    pub fn set_terms(&self, accepted: bool) {
        let mut s = self.state();
        s.terms_ok = accepted;
        s.terms_problem = (!accepted).then(|| "Gemini's prediction-market terms are not accepted on this account; accept them on Gemini, by hand".to_string());
    }

    /// The lookups that are due at `now`: unanswered placements and
    /// orders marked for one.
    pub fn due_lookups(&self, now: Instant) -> Vec<LookupRequest> {
        let mut s = self.state();
        // Orders the engine has let go of: a held one ends here; one Gemini
        // may still hold is cancelled there and looked up until it is gone.
        let let_go: Vec<OrderId> = {
            let x = self.exchange();
            s.live.iter().filter(|(id, l)| !l.orphaned && !x.external_is_live(l.series, **id)).map(|(id, _)| *id).collect()
        };
        for id in let_go {
            let Some(live) = s.live.get_mut(&id) else { continue };
            if live.stage == Stage::Held {
                self.end(&mut s, id, ExecKind::CancelAck);
                continue;
            }
            live.orphaned = true;
            live.lookup_due = Some(now);
            if let Some(frame) = Self::cancel_frame(&mut s, id) {
                self.send(frame);
            }
        }
        let mut due = Vec::new();
        for (id, live) in s.live.iter_mut() {
            if live.lookup_running {
                continue;
            }
            // An unanswered order with a lookup already set for later waits for it.
            let unanswered = live.lookup_due.is_none() && matches!(live.stage, Stage::Sent(at) if now.duration_since(at) >= PLACE_TIMEOUT);
            let marked = live.lookup_due.is_some_and(|at| now >= at);
            if unanswered || marked {
                live.lookup_running = true;
                live.lookup_due = None;
                due.push(LookupRequest { order: *id, symbol: live.symbol.clone() });
            }
        }
        due.sort_by_key(|l| l.order);
        due
    }

    /// What REST said about one order. Returns the frames to send.
    pub fn on_lookup(&self, id: OrderId, result: Result<Vec<RestOrder>, String>) -> Vec<String> {
        self.on_lookup_at(id, result, Instant::now())
    }

    /// [`GeminiVenue::on_lookup`] with the time it is.
    pub fn on_lookup_at(&self, id: OrderId, result: Result<Vec<RestOrder>, String>, now: Instant) -> Vec<String> {
        let mut out = Vec::new();
        let mut s = self.state();
        let Some(live) = s.live.get_mut(&id) else { return out };
        live.lookup_running = false;
        let orders = match result {
            Ok(orders) => orders,
            Err(e) => {
                tracing::warn!(order = id.0, error = %e, "could not look an order up on gemini; will try again");
                live.lookup_due = Some(now + LOOKUP_RETRY);
                return out;
            }
        };
        let Some(found) = orders.iter().find(|o| o.client_order_id.as_deref() == Some(live.client_id.as_str())) else {
            if matches!(live.stage, Stage::Sent(_)) {
                // Gemini may only be slow to list it: one miss decides
                // nothing, and nor does a second one that comes sooner
                // than the wait, whatever asked for it.
                let first_miss = *live.first_miss.get_or_insert(now);
                if now.duration_since(first_miss) < LOOKUP_RETRY {
                    live.lookup_due = Some(first_miss + LOOKUP_RETRY);
                    return out;
                }
                // Gemini never took it. It is refused, not sent again; and
                // remembered, in case Gemini says otherwise later.
                tracing::warn!(order = id.0, "gemini has no record of an unanswered order; reporting it refused");
                let refused = Refused { series: live.series, account: live.account, side: live.side, limit: live.limit, market: live.market, qty: live.qty, booked: live.booked, trades: HashSet::new(), seen: false, cancel_sent: false };
                let client_id = live.client_id.clone();
                s.refused.insert(client_id.clone(), refused);
                s.refused_order.push_back(client_id);
                while s.refused_order.len() > REFUSED_KEPT {
                    if let Some(old) = s.refused_order.pop_front() {
                        s.refused.remove(&old);
                        s.refused_by_gemini.retain(|_, client_id| *client_id != old);
                    }
                }
                out.extend(self.end(&mut s, id, ExecKind::Reject(ExecReject::VenueUnreachable)));
            } else if live.orphaned {
                // Nothing left of it on either side.
                out.extend(self.end(&mut s, id, ExecKind::CancelAck));
            }
            return out;
        };
        live.gemini_id = Some(found.order_id);
        live.stage = Stage::Acked;
        s.by_gemini.insert(found.order_id, id);
        let price = found.avg_execution_price.or(found.price);
        if self.book_up_to(&mut s, id, Some(found.filled_quantity), price) {
            out.extend(self.end(&mut s, id, ExecKind::Fill));
            return out;
        }
        match found.status {
            RestOrderStatus::Cancelled => {
                let lapsed = s.live.get(&id).is_some_and(|l| l.lapsed || matches!(l.tif, WireTif::Ioc | WireTif::Fok));
                out.extend(self.end(&mut s, id, if lapsed { ExecKind::Expiry } else { ExecKind::CancelAck }));
            }
            RestOrderStatus::Filled => {
                // Filled, but for less than we hold it for: the rest will
                // never come.
                out.extend(self.end(&mut s, id, ExecKind::CancelAck));
            }
            RestOrderStatus::Open | RestOrderStatus::Other(_) => {
                if s.connected && s.live.get(&id).is_some_and(|l| l.cancel == CancelState::Wanted) {
                    out.extend(Self::cancel_frame(&mut s, id));
                }
                if let Some(live) = s.live.get_mut(&id)
                    && live.orphaned
                {
                    live.lookup_due = Some(now + LOOKUP_RETRY);
                }
            }
        }
        out
    }
}

/// Gemini's prediction markets move in whole cents from 1 to 99.
fn on_cent_grid(tick: PriceTick) -> bool {
    let t = tick.tenths();
    t.is_multiple_of(10) && (10..=990).contains(&t)
}

/// The most cash, in tenths of a cent, that `orders` could take from an
/// account holding `position` contracts of the quoted outcome, if every
/// one filled at its limit. An order on the bid side buys the quoted
/// outcome at its limit; one on the ask side buys the other at the
/// complement. Contracts that close what the account already holds cost
/// nothing, and each held contract is counted for one order only.
fn cost_to_open(position: i64, orders: impl Iterator<Item = (Side, PriceTick, Qty)>) -> i64 {
    let mut closable_by_asks = position.max(0);
    let mut closable_by_bids = (-position).max(0);
    let mut cost = 0i64;
    for (side, limit, qty) in orders {
        let (closable, each) = match side {
            Side::Bid => (&mut closable_by_bids, i64::from(limit.tenths())),
            Side::Ask => (&mut closable_by_asks, 1000 - i64::from(limit.tenths())),
        };
        let closing = (*closable).min(i64::from(qty.0));
        *closable -= closing;
        cost += (i64::from(qty.0) - closing) * each;
    }
    cost
}

/// Gemini's reason for refusing an order, in the engine's vocabulary.
fn refusal(reason: Option<&str>) -> ExecReject {
    match reason.unwrap_or("") {
        "MarketNotOpen" => ExecReject::MarketClosed,
        "InsufficientFunds" => ExecReject::InsufficientBalance,
        "InsufficientLiquidity" => ExecReject::NoLiquidity,
        "MakerOrCancelWouldTake" => ExecReject::PostOnlyWouldCross,
        _ => ExecReject::VenueRefused,
    }
}

impl Venue for Arc<GeminiVenue> {
    fn submit(&self, series: SeriesId, account: Address, order: &VenueOrder) -> Result<Accepted, RejectReason> {
        let (side, limit, qty, tif) = self.translate(series, account, order)?;
        let (id, market_id) = {
            let mut s = self.state();
            let (id, market_id) = self.open(&mut s, series, account, order, side, limit, qty, tif, Stage::Held)?;
            if let Some(frame) = GeminiVenue::place_frame(&mut s, id, Instant::now()) {
                self.send(frame);
            }
            (id, market_id)
        };
        if let (WireTif::Gtc, Some(ms)) = (tif, order.expire_after_ms) {
            self.arm_expiry(id, ms);
        }
        Ok(Accepted { venue_order_id: id.0, market_id })
    }

    fn cancel(&self, _series: SeriesId, account: Address, venue_order_id: u64) -> Result<(), String> {
        let id = OrderId(venue_order_id);
        if self.state().live.get(&id).is_none_or(|l| l.account != account) {
            return Err(format!("order {venue_order_id} is not live"));
        }
        self.request_cancel(id, false)
    }

    fn cancel_replace(&self, series: SeriesId, account: Address, old: u64, order: &VenueOrder) -> Result<Accepted, RejectReason> {
        let (side, limit, qty, tif) = self.translate(series, account, order)?;
        let old = OrderId(old);
        let (id, market_id) = {
            let mut s = self.state();
            match s.live.get(&old) {
                Some(l) if l.account == account && l.replaced_by.is_none() => {}
                _ => return Err(RejectReason::InvalidOrder),
            }
            let (id, market_id) = self.open(&mut s, series, account, order, side, limit, qty, tif, Stage::Held)?;
            let old_live = s.live.get_mut(&old).expect("checked above");
            old_live.replaced_by = Some(id);
            if old_live.stage == Stage::Held {
                // The old one was itself waiting: it is replaced here and
                // the new one waits in its place.
                let waiting_on = s.live.iter().find(|(_, l)| l.replaced_by == Some(old)).map(|(k, _)| *k);
                s.live.get_mut(&old).expect("present").replaced_by = None;
                self.end(&mut s, old, ExecKind::Replaced);
                match waiting_on.and_then(|k| s.live.get_mut(&k)) {
                    Some(first) => first.replaced_by = Some(id),
                    None => {
                        if let Some(frame) = GeminiVenue::place_frame(&mut s, id, Instant::now()) {
                            self.send(frame);
                        }
                    }
                }
            } else if let Some(frame) = GeminiVenue::cancel_frame(&mut s, old) {
                self.send(frame);
            }
            (id, market_id)
        };
        if let (WireTif::Gtc, Some(ms)) = (tif, order.expire_after_ms) {
            self.arm_expiry(id, ms);
        }
        Ok(Accepted { venue_order_id: id.0, market_id })
    }

    fn account_view(&self, series: SeriesId, account: Address) -> AccountView {
        account_view(&self.exchange(), series, account)
    }
}

// ───────────────────────── the socket task ─────────────────────────

/// Where and how the task connects.
#[derive(Debug, Clone)]
pub struct SessionConfig {
    /// Without a path or query, e.g. `wss://ws.sandbox.gemini.com`.
    pub url: String,
    pub backoff_base: Duration,
    pub backoff_max: Duration,
    /// How often the socket is pinged.
    pub ping_every: Duration,
    /// A socket that has delivered nothing for this long is treated as
    /// dropped: a half-open connection would otherwise look healthy while
    /// fills go unseen.
    pub idle_limit: Duration,
    /// The nonces of the key; shared with the REST client.
    pub nonce: Nonce,
}

/// The lookups to run, one read per contract.
fn by_symbol(due: Vec<LookupRequest>) -> Vec<(String, Vec<OrderId>)> {
    let mut grouped: std::collections::BTreeMap<String, Vec<OrderId>> = std::collections::BTreeMap::new();
    for lookup in due {
        grouped.entry(lookup.symbol).or_default().push(lookup.order);
    }
    grouped.into_iter().map(|(symbol, mut orders)| (symbol, { orders.sort(); orders })).collect()
}

/// Keep the signed socket up forever: check the terms, connect with the
/// key in the handshake, subscribe to the order stream, apply what
/// arrives, send what the venue queues, and run the lookups that fall due.
pub async fn run(mut driver: Driver, config: SessionConfig, key: ApiKey, lookup: Arc<dyn OrderLookup>) {
    let venue = driver.venue.clone();
    let (results_tx, mut results) = mpsc::unbounded_channel::<(OrderId, Result<Vec<RestOrder>, String>)>();
    // `cancelOnDisconnect`: if this server loses the socket, Gemini pulls
    // the orders it placed rather than leave them resting unwatched.
    let url = format!("{}/?cancelOnDisconnect=true", config.url.trim_end_matches('/'));
    let mut attempt: u32 = 0;
    let mut tick = tokio::time::interval(TICK_EVERY);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        match lookup.terms_accepted().await {
            Ok(accepted) => venue.set_terms(accepted),
            Err(e) => {
                tracing::warn!(error = %e, "could not read gemini's terms status");
                venue.set_terms(false);
            }
        }
        let mut socket = if venue.state().terms_ok { connect(&url, &key, &config.nonce).await } else { None };
        // What the venue wants said first on a new socket. It is sent by
        // the loop below like any other frame, so a send that fails is a
        // dropped socket like any other and the venue is told so.
        let mut greeting = if socket.is_some() {
            attempt = 0;
            tracing::info!("gemini order socket connected");
            venue.on_connected()
        } else {
            Vec::new()
        };
        let wait = if venue.state().terms_ok {
            super::ws::backoff_delay(attempt, config.backoff_base, config.backoff_max)
        } else {
            TERMS_RETRY
        };
        let retry_at = tokio::time::Instant::now() + wait;
        let mut heard = tokio::time::Instant::now();
        let mut pinged = heard;
        // One loop for both states: with a socket it runs until the socket
        // drops; without one it runs until it is time to try again. The
        // lookups run in both, since REST does not need the socket.
        loop {
            let mut outgoing: Vec<String> = std::mem::take(&mut greeting);
            if outgoing.is_empty() {
                tokio::select! {
                    message = async { socket.as_mut().expect("guarded").next().await }, if socket.is_some() => {
                        heard = tokio::time::Instant::now();
                        match message {
                            Some(Ok(Message::Text(text))) => outgoing = venue.on_inbound(text.as_str()),
                            Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                            // A pong, or anything else: the socket is alive.
                            Some(Ok(_)) => {}
                        }
                    },
                    frame = driver.frames.recv() => match frame {
                        Some(stamped) => outgoing.extend(venue.still_current(stamped)),
                        None => return,
                    },
                    Some((id, result)) = results.recv() => outgoing = venue.on_lookup(id, result),
                    _ = tick.tick() => {
                        for (symbol, orders) in by_symbol(venue.due_lookups(Instant::now())) {
                            let (lookup, tx) = (lookup.clone(), results_tx.clone());
                            tokio::spawn(async move {
                                let result = lookup.orders(symbol).await;
                                for order in orders {
                                    let _ = tx.send((order, result.clone()));
                                }
                            });
                        }
                        if let Some(ws) = socket.as_mut() {
                            let now = tokio::time::Instant::now();
                            if now.duration_since(heard) >= config.idle_limit {
                                tracing::warn!(silent_for = ?now.duration_since(heard), "gemini order socket has gone silent; dropping it");
                                break;
                            }
                            if now.duration_since(pinged) >= config.ping_every {
                                pinged = now;
                                if ws.send(Message::Ping(Vec::new().into())).await.is_err() {
                                    break;
                                }
                            }
                        }
                    },
                    _ = tokio::time::sleep_until(retry_at), if socket.is_none() => break,
                }
            }
            let mut dropped = false;
            for frame in outgoing {
                match socket.as_mut() {
                    Some(ws) => dropped |= ws.send(Message::Text(frame.into())).await.is_err(),
                    // No socket: the order stays unanswered and its lookup
                    // will report it refused.
                    None => tracing::warn!("a frame for gemini was dropped: the order socket is down"),
                }
            }
            if dropped {
                break;
            }
        }
        if socket.is_some() {
            tracing::warn!("gemini order socket dropped");
            venue.on_disconnected();
            tokio::time::sleep(super::ws::backoff_delay(attempt, config.backoff_base, config.backoff_max)).await;
        }
        attempt = attempt.saturating_add(1);
    }
}

type Socket = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Open the socket with the key in the handshake. `None` on any failure;
/// the reason is logged without a header value.
async fn connect(url: &str, key: &ApiKey, nonce: &Nonce) -> Option<Socket> {
    let mut request = url.into_client_request().ok()?;
    let headers = key.ws_headers(nonce.next());
    for (name, value) in [
        ("X-GEMINI-APIKEY", headers.apikey),
        ("X-GEMINI-NONCE", headers.nonce),
        ("X-GEMINI-PAYLOAD", headers.payload),
        ("X-GEMINI-SIGNATURE", headers.signature),
    ] {
        request.headers_mut().insert(name, value.parse().ok()?);
    }
    match tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(request)).await {
        Ok(Ok((socket, _))) => Some(socket),
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "gemini order socket did not connect");
            None
        }
        Err(_) => {
            tracing::warn!("gemini order socket timed out connecting");
            None
        }
    }
}

#[cfg(test)]
#[path = "venue_tests.rs"]
mod tests;
