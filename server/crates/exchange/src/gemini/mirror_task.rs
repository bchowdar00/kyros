//! The mirror at work: Gemini's depth and trade messages in, orders on
//! the engine's book out. The rules are in [`super::mirror`]; this file
//! runs them against a real [`Exchange`].

use std::sync::{Arc, Mutex};
use std::time::Duration;

use rust_decimal::prelude::ToPrimitive;
use serde::Serialize;
use tokio::sync::broadcast::error::RecvError;

use super::driver::{Phase, SeriesLink};
use super::mirror::{Level, MirrorOp, MirrorState, levels, reconcile, tick_for};
use super::types::{LocalBook, WsEvent};
use super::ws::PublicSocket;
use crate::engine::{Exchange, MarketEngine, PlaceOpts, TradeSource};
use crate::ticks::PriceTick;
use crate::types::{Address, MarketId, Qty, SeriesId, Side, Tenths};

/// The mirror's name wherever an account's name shows.
pub const MIRROR_USERNAME: &str = "gemini-book";
/// A bankroll the mirror's orders can never run into: $50 million.
const MIRROR_BANKROLL: Tenths = Tenths(50_000_000_000);
/// How often the mirror checks which contract its series' round follows.
const FOLLOW_EVERY: Duration = Duration::from_millis(250);

/// The mirror account of a series: `0x6e 0x01 <series> 0…`.
pub const fn mirror_address(series: SeriesId) -> Address {
    Address([0x6e, 0x01, series.0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0])
}

/// Whether a series' book is Gemini's book right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BookState {
    /// No contract is being mirrored (no round is open).
    #[default]
    Waiting,
    /// The book is Gemini's, as of its last message.
    Live,
    /// The link to Gemini dropped, or no book has arrived yet: the
    /// mirror's orders are off the book until one does.
    Stale,
}

/// One series' mirror.
pub struct Mirror {
    series: SeriesId,
    address: Address,
    state: MirrorState,
    book: LocalBook,
    /// The contract being mirrored.
    symbol: Option<String>,
    /// A whole book has arrived for `symbol` since it was chosen or
    /// since the link last dropped.
    live: bool,
    /// The round the mirror's orders were placed in.
    market: Option<MarketId>,
}

impl Mirror {
    /// Seed the mirror's account and tell the series' engine it is one.
    pub fn new(exchange: &mut Exchange, series: SeriesId) -> Self {
        let address = mirror_address(series);
        // A bot account: pinned to its series, and its balance starts
        // again after every round, so it never runs out.
        exchange.seed_bot_account_in(address, MIRROR_BANKROLL, MIRROR_USERNAME.to_string(), None, None, series);
        exchange.engine_mut(series).set_mirror(address);
        Self {
            series,
            address,
            state: MirrorState::new(),
            book: LocalBook::new(),
            symbol: None,
            live: false,
            market: None,
        }
    }

    pub fn address(&self) -> Address {
        self.address
    }

    pub fn symbol(&self) -> Option<&str> {
        self.symbol.as_deref()
    }

    pub fn book_state(&self) -> BookState {
        match (&self.symbol, self.live) {
            (None, _) => BookState::Waiting,
            (Some(_), true) => BookState::Live,
            (Some(_), false) => BookState::Stale,
        }
    }

    /// Mirror another contract, or none. Every order of the mirror's
    /// comes off the book first.
    pub fn switch_to(&mut self, exchange: &mut Exchange, symbol: Option<String>) {
        self.drop_book(exchange);
        self.symbol = symbol;
    }

    /// One socket event. `true` when the engine's book changed.
    pub fn on_event(&mut self, exchange: &mut Exchange, event: &WsEvent) -> bool {
        match event {
            WsEvent::Depth { symbol, bids, asks, snapshot, .. } if self.follows(symbol) => {
                // Changes mean nothing without the whole book before them.
                if !*snapshot && !self.live {
                    return false;
                }
                self.live = true;
                self.book.apply(bids, asks, *snapshot);
                self.sync(exchange)
            }
            WsEvent::Trade { symbol, price, qty, buyer_is_maker, .. } if self.follows(symbol) => {
                let (Some(tick), Some(qty)) = (tick_for(*price), qty.floor().to_u32().filter(|q| *q >= 1)) else {
                    return false;
                };
                // Gemini's `m`: the buyer made, so the seller took.
                let taker_side = if *buyer_is_maker == Some(true) { Side::Ask } else { Side::Bid };
                exchange.engine_mut(self.series).record_print(taker_side, tick, Qty(qty));
                self.fill_inside_the_spread(exchange, tick, qty)
            }
            WsEvent::Disconnected => self.drop_book(exchange),
            _ => false,
        }
    }

    fn follows(&self, symbol: &str) -> bool {
        self.symbol.as_deref() == Some(symbol)
    }

    /// Whether the series has a round that is taking orders. A new round
    /// means the engine cancelled everything the mirror had resting.
    fn round_is_open(&mut self, exchange: &Exchange) -> bool {
        let engine = exchange.engine(self.series);
        let market = engine.current_market().map(|m| m.id);
        if market != self.market {
            self.state.clear();
            self.market = market;
        }
        market.is_some() && !engine.is_locked()
    }

    /// Take every order of the mirror's off the book and forget
    /// Gemini's: there is no book until a whole one arrives.
    fn drop_book(&mut self, exchange: &mut Exchange) -> bool {
        self.round_is_open(exchange);
        let orders = self.state.orders();
        for (order, ..) in &orders {
            let _ = exchange.cancel(self.series, self.address, *order);
        }
        self.state.clear();
        self.book.clear();
        self.live = false;
        !orders.is_empty()
    }

    /// Read back what is left of each resting order: what is missing was
    /// taken by pretend orders.
    fn note_takes(&mut self, exchange: &Exchange) {
        let book = exchange.engine(self.series).book();
        for (order, ..) in self.state.orders() {
            let left = book.get_order(order).map_or(Qty::ZERO, |o| o.qty_open);
            self.state.remaining(order, left);
        }
    }

    /// Bring the mirror's orders level with Gemini's book.
    fn sync(&mut self, exchange: &mut Exchange) -> bool {
        if !self.round_is_open(exchange) {
            return false;
        }
        self.note_takes(exchange);
        let bids = levels(&self.book.bids());
        let asks = levels(&self.book.asks());
        let ops = reconcile(&bids, &asks, &self.state);
        self.state.observe(&bids, &asks);
        let mut changed = false;
        for op in ops {
            match op {
                MirrorOp::Cancel { order } => {
                    let _ = exchange.cancel(self.series, self.address, order);
                    self.state.cancelled(order);
                    changed = true;
                }
                MirrorOp::Place { side, tick, qty } => {
                    match exchange.place_limit(self.series, self.address, side, tick, qty, TradeSource::Bot) {
                        Ok(placed) => {
                            // Gemini's book came through a pretend order
                            // resting inside the old spread: that order
                            // filled at its own price, out of this level.
                            let crossed: u32 = placed.fills.iter().map(|f| f.qty.0).sum();
                            if crossed > 0 {
                                self.state.took(side, tick, Qty(crossed));
                                tracing::debug!(
                                    series = %self.series,
                                    ?side,
                                    tick = tick.tenths(),
                                    gemini_qty = qty.0,
                                    filled = crossed,
                                    "mirror: gemini's book came through a pretend order",
                                );
                            }
                            self.state.placed(side, tick, placed.order_id, placed.resting_qty);
                            changed = true;
                        }
                        Err(error) => {
                            tracing::debug!(series = %self.series, ?side, tick = tick.tenths(), %error, "mirror order refused");
                        }
                    }
                }
            }
        }
        changed
    }

    /// The best price the mirror shows on a side: Gemini's, or an order
    /// of the mirror's own that has not caught up with Gemini yet.
    fn mirrored_best(&self, side: Side) -> Option<PriceTick> {
        let gemini = match side {
            Side::Bid => levels(&self.book.bids()),
            Side::Ask => levels(&self.book.asks()),
        };
        let mine = self.state.orders().into_iter().filter(|(_, s, ..)| *s == side).map(|(_, _, tick, _)| tick);
        let all = gemini.into_iter().map(|l: Level| l.tick).chain(mine);
        match side {
            Side::Bid => all.max(),
            Side::Ask => all.min(),
        }
    }

    /// Gemini printed `qty` at `tick`. A real order resting inside
    /// Gemini's spread, at that price or better, would have traded
    /// first: fill pretend orders there, at their own prices, for no
    /// more than the print. An order at or behind a Gemini level waits
    /// behind Gemini's queue and is left alone.
    fn fill_inside_the_spread(&mut self, exchange: &mut Exchange, tick: PriceTick, qty: u32) -> bool {
        if !self.live || !self.round_is_open(exchange) {
            return false;
        }
        self.note_takes(exchange);
        let mut left = qty;
        // Pretend bids above the mirror's best bid, at or over the print.
        let floor = match self.mirrored_best(Side::Bid) {
            Some(best) => PriceTick::from_index(best.to_index() + 1).map(|above| above.max(tick)),
            None => Some(tick),
        };
        if let Some(floor) = floor
            && exchange.engine(self.series).book().best_bid().is_some_and(|bid| bid >= floor)
        {
            left -= self.take(exchange, Side::Ask, floor, left);
        }
        // Pretend asks below the mirror's best ask, at or under the print.
        let cap = match self.mirrored_best(Side::Ask) {
            Some(best) => best.to_index().checked_sub(1).and_then(PriceTick::from_index).map(|below| below.min(tick)),
            None => Some(tick),
        };
        if left > 0
            && let Some(cap) = cap
            && exchange.engine(self.series).book().best_ask().is_some_and(|ask| ask <= cap)
        {
            left -= self.take(exchange, Side::Bid, cap, left);
        }
        if left < qty {
            tracing::debug!(
                series = %self.series,
                tick = tick.tenths(),
                print_qty = qty,
                filled = qty - left,
                "mirror: a gemini print filled a pretend order",
            );
        }
        left < qty
    }

    /// The mirror takes up to `qty` at `limit` or better and rests
    /// nothing. Returns what it took.
    fn take(&mut self, exchange: &mut Exchange, side: Side, limit: PriceTick, qty: u32) -> u32 {
        let opts = PlaceOpts { ioc: true, ..PlaceOpts::default() };
        match exchange.place_limit_with(self.series, self.address, side, limit, Qty(qty), TradeSource::Bot, opts) {
            Ok(placed) => placed.fills.iter().map(|f| f.qty.0).sum(),
            Err(error) => {
                tracing::debug!(series = %self.series, ?side, %error, "mirror could not fill a pretend order on a print");
                0
            }
        }
    }
}

/// The public streams of one contract: its book, ten times a second,
/// and its trades.
fn streams_for(symbol: &str) -> Vec<String> {
    vec![format!("{symbol}@depth@100ms"), format!("{symbol}@trade")]
}

/// Run a series' mirror for the life of the process: it follows the
/// contract of the round the driver has open (`link`), subscribes to
/// that contract's depth and trades, and calls `on_book_change` after
/// every change to the engine's book.
pub fn spawn_mirror<F>(
    exchange: Arc<Mutex<Exchange>>,
    series: SeriesId,
    socket: PublicSocket,
    link: SeriesLink,
    on_book_change: F,
) -> tokio::task::JoinHandle<()>
where
    F: Fn(&MarketEngine) + Send + 'static,
{
    tokio::spawn(async move {
        let mut events = socket.events();
        let mut mirror = Mirror::new(&mut exchange.lock().expect("exchange mutex poisoned"), series);
        let mut follow = tokio::time::interval(FOLLOW_EVERY);
        follow.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                event = events.recv() => {
                    let changed = match event {
                        Ok(event) => {
                            let mut x = exchange.lock().expect("exchange mutex poisoned");
                            mirror.on_event(&mut x, &event)
                        }
                        // Messages were missed: the book cannot be trusted.
                        // Drop it and ask for the contract's book again.
                        Err(RecvError::Lagged(missed)) => {
                            tracing::warn!(%series, missed, "mirror fell behind gemini's socket; reloading the book");
                            let symbol = mirror.symbol().map(str::to_owned);
                            let changed = {
                                let mut x = exchange.lock().expect("exchange mutex poisoned");
                                mirror.on_event(&mut x, &WsEvent::Disconnected)
                            };
                            if let Some(symbol) = symbol {
                                socket.unsubscribe(streams_for(&symbol));
                                socket.subscribe(streams_for(&symbol));
                            }
                            changed
                        }
                        Err(RecvError::Closed) => return,
                    };
                    link.set_book(mirror.book_state());
                    if changed {
                        on_book_change(exchange.lock().expect("exchange mutex poisoned").engine(series));
                    }
                }
                _ = follow.tick() => {
                    // Mirror the contract of the round the driver has open.
                    let wanted = match link.phase() {
                        Phase::Open => link.window().map(|w| w.symbol),
                        Phase::WaitingForWindow | Phase::WaitingForResult => None,
                    };
                    if wanted.as_deref() != mirror.symbol() {
                        if let Some(old) = mirror.symbol() {
                            socket.unsubscribe(streams_for(old));
                        }
                        {
                            let mut x = exchange.lock().expect("exchange mutex poisoned");
                            mirror.switch_to(&mut x, wanted.clone());
                            on_book_change(x.engine(series));
                        }
                        if let Some(new) = &wanted {
                            socket.subscribe(streams_for(new));
                        }
                        tracing::info!(%series, contract = ?wanted, "mirror: following");
                    }
                    link.set_book(mirror.book_state());
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{OrderError, RoundClock};
    use crate::series::SERIES;
    use rust_decimal::Decimal;
    use std::str::FromStr;
    use std::sync::atomic::{AtomicU64, Ordering};

    const S: SeriesId = SeriesId(0);
    const A: Address = Address([0xaa; 20]);
    const B: Address = Address([0xbb; 20]);
    const SYMBOL: &str = "GEMI-BTC05M2611181730-UP";
    const OTHER: &str = "GEMI-BTC05M2611181735-UP";
    const OPENS: u64 = 1_900_000_000;

    fn d(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }

    fn tick(t: u16) -> PriceTick {
        PriceTick::from_tenths(t).unwrap()
    }

    fn pairs(levels: &[(&str, &str)]) -> Vec<(Decimal, Decimal)> {
        levels.iter().map(|(p, q)| (d(p), d(q))).collect()
    }

    fn depth(symbol: &str, bids: &[(&str, &str)], asks: &[(&str, &str)], snapshot: bool) -> WsEvent {
        WsEvent::Depth { symbol: symbol.into(), bids: pairs(bids), asks: pairs(asks), snapshot, ts_ms: 0 }
    }

    /// Bids 10 at 54¢ and 25 at 53¢; asks 7 at 56¢ and 20 at 58¢.
    fn snapshot() -> WsEvent {
        depth(SYMBOL, &[("0.54", "10"), ("0.53", "25.9")], &[("0.56", "7"), ("0.58", "20"), ("0.59", "0.5")], true)
    }

    fn print(symbol: &str, price: &str, qty: &str, buyer_is_maker: bool) -> WsEvent {
        WsEvent::Trade { symbol: symbol.into(), price: d(price), qty: d(qty), buyer_is_maker: Some(buyer_is_maker), ts_ms: 0 }
    }

    /// Two people, an open round, and a mirror following `SYMBOL` with
    /// Gemini's book in place.
    fn rig() -> (Exchange, Mirror) {
        let mut x = Exchange::new(Tenths(1_000_000), SERIES);
        for a in [A, B] {
            x.get_or_create_account(a);
            x.register_user(a);
        }
        x.open_first_market(S, 100_000.0, OPENS, 300);
        let mut m = Mirror::new(&mut x, S);
        m.switch_to(&mut x, Some(SYMBOL.into()));
        assert!(m.on_event(&mut x, &snapshot()), "the snapshot changed the book");
        (x, m)
    }

    /// The engine's book as `(tenths, quantity)`, best first.
    /// Bids and asks as `(tick, quantity)`.
    type Sides = (Vec<(u16, u32)>, Vec<(u16, u32)>);

    fn book(x: &Exchange) -> Sides {
        let b = x.engine(S).book();
        (
            b.iter_bids().map(|(p, q)| (p.tenths(), q.0)).collect(),
            b.iter_asks().map(|(p, q)| (p.tenths(), q.0)).collect(),
        )
    }

    /// What the mirror itself has resting, as `(tenths, quantity)` summed
    /// per price, best first.
    fn mirrors(x: &Exchange, m: &Mirror) -> Sides {
        let b = x.engine(S).book();
        let sum = |orders: Vec<(Address, u16, u32)>| {
            let mut out: Vec<(u16, u32)> = Vec::new();
            for (_, price, qty) in orders.into_iter().filter(|(account, _, _)| *account == m.address()) {
                match out.last_mut() {
                    Some((p, q)) if *p == price => *q += qty,
                    _ => out.push((price, qty)),
                }
            }
            out
        };
        (
            sum(b.iter_bid_orders().map(|o| (o.account, o.price.tenths(), o.qty_open.0)).collect()),
            sum(b.iter_ask_orders().map(|o| (o.account, o.price.tenths(), o.qty_open.0)).collect()),
        )
    }

    /// What an account has had filled this round: contracts and the sum
    /// of quantity times price in tenths.
    fn filled(x: &Exchange, who: Address) -> (u32, u64) {
        let rows = x.engine(S).user_orders(&who);
        (rows.iter().map(|r| r.filled_qty.0).sum(), rows.iter().map(|r| r.notional_tenths).sum())
    }

    #[test]
    fn the_mirror_is_an_account_of_its_series_and_not_a_person() {
        let (x, m) = rig();
        assert_eq!(m.address(), mirror_address(S));
        assert_eq!((m.address().0[0], m.address().0[1], m.address().0[2]), (0x6e, 0x01, 0));
        assert_ne!(mirror_address(SeriesId(0)), mirror_address(SeriesId(1)));
        let account = x.account(&m.address()).expect("seeded");
        assert_eq!(account.username.as_deref(), Some(MIRROR_USERNAME));
        assert_eq!(account.home_series, Some(S));
        assert!(!x.is_user_account(&m.address()));
        assert_eq!(x.engine(S).mirror(), Some(m.address()));
        assert_eq!(m.symbol(), Some(SYMBOL));
    }

    #[test]
    fn a_snapshot_puts_geminis_book_on_the_engine_in_whole_contracts() {
        let (x, m) = rig();
        // 25.9 rounds down to 25; the half contract at 59¢ is not mirrored.
        assert_eq!(book(&x), (vec![(540, 10), (530, 25)], vec![(560, 7), (580, 20)]));
        assert_eq!(mirrors(&x, &m), book(&x), "every order is the mirror's");
        assert_eq!(m.book_state(), BookState::Live);
    }

    #[test]
    fn a_change_moves_only_what_changed() {
        let (mut x, mut m) = rig();
        let untouched = x.engine(S).book().iter_bid_orders().find(|o| o.price == tick(530)).unwrap().id;
        assert!(m.on_event(&mut x, &depth(SYMBOL, &[("0.54", "0"), ("0.52", "3")], &[("0.56", "9")], false)));
        assert_eq!(book(&x), (vec![(530, 25), (520, 3)], vec![(560, 9), (580, 20)]));
        assert!(x.engine(S).book().get_order(untouched).is_some(), "the 53¢ order kept its place");
        // The same book again changes nothing.
        assert!(!m.on_event(&mut x, &depth(SYMBOL, &[("0.52", "3")], &[], false)));
    }

    #[test]
    fn another_contracts_messages_are_not_this_book() {
        let (mut x, mut m) = rig();
        assert!(!m.on_event(&mut x, &depth(OTHER, &[("0.10", "99")], &[("0.11", "99")], true)));
        assert!(!m.on_event(&mut x, &print(OTHER, "0.55", "5", false)));
        assert_eq!(book(&x), (vec![(540, 10), (530, 25)], vec![(560, 7), (580, 20)]));
        assert_eq!(x.engine(S).market_trade_count(), 0);
    }

    #[test]
    fn what_a_pretend_order_takes_is_not_put_back_until_gemini_changes_that_level() {
        let (mut x, mut m) = rig();
        let out = x.place_market(S, A, Side::Bid, Qty(5), TradeSource::Ui).unwrap();
        assert_eq!(out.fills.iter().map(|f| (f.price.tenths(), f.qty.0)).collect::<Vec<_>>(), vec![(560, 5)]);
        assert_eq!(book(&x).1, vec![(560, 2), (580, 20)]);
        // Gemini's book moves elsewhere; 56¢ still shows 7 there.
        m.on_event(&mut x, &depth(SYMBOL, &[], &[("0.58", "21")], false));
        assert_eq!(book(&x).1, vec![(560, 2), (580, 21)], "the 5 taken are not offered twice");
        // Gemini's size at 56¢ changes: the level is Gemini's again.
        m.on_event(&mut x, &depth(SYMBOL, &[], &[("0.56", "8")], false));
        assert_eq!(book(&x).1, vec![(560, 8), (580, 21)]);
    }

    #[test]
    fn a_level_taken_whole_stays_empty_until_gemini_changes_it() {
        let (mut x, mut m) = rig();
        x.place_market(S, A, Side::Bid, Qty(7), TradeSource::Ui).unwrap();
        assert_eq!(book(&x).1, vec![(580, 20)]);
        m.on_event(&mut x, &depth(SYMBOL, &[("0.53", "26")], &[], false));
        assert_eq!(book(&x).1, vec![(580, 20)]);
        m.on_event(&mut x, &depth(SYMBOL, &[], &[("0.56", "6")], false));
        assert_eq!(book(&x).1, vec![(560, 6), (580, 20)]);
    }

    #[test]
    fn a_market_order_into_a_side_gemini_does_not_have_finds_no_liquidity() {
        let (mut x, mut m) = rig();
        // Gemini's asks all go; only bids are left.
        m.on_event(&mut x, &depth(SYMBOL, &[], &[("0.56", "0"), ("0.58", "0"), ("0.59", "0")], false));
        assert_eq!(book(&x).1, vec![]);
        let refused = x.place_market(S, A, Side::Bid, Qty(1), TradeSource::Ui);
        assert_eq!(refused.unwrap_err(), crate::engine::OrderError::NoLiquidity);
        // The other side is still there to sell into.
        assert_eq!(x.place_market(S, A, Side::Ask, Qty(1), TradeSource::Ui).unwrap().fills.len(), 1);
    }

    #[test]
    fn a_pretend_bid_inside_the_spread_fills_at_its_own_price_when_geminis_ask_comes_down_through_it() {
        let (mut x, mut m) = rig();
        let out = x.place_limit(S, A, Side::Bid, tick(550), Qty(4), TradeSource::Ui).unwrap();
        assert!(out.fills.is_empty(), "inside the spread: it rests");
        // Gemini's whole book steps down: best bid 52¢, best ask 53¢.
        m.on_event(
            &mut x,
            &depth(SYMBOL, &[("0.54", "0"), ("0.53", "0"), ("0.52", "10")], &[("0.56", "0"), ("0.53", "7")], false),
        );
        // A bought 4 at 55¢, the price A asked for, not at 53¢.
        assert_eq!(filled(&x, A), (4, 4 * 550));
        assert_eq!(x.engine(S).position(&A).position.0, 4);
        // Those 4 came out of the 7 Gemini showed at 53¢.
        assert_eq!(book(&x), (vec![(520, 10)], vec![(530, 3), (580, 20)]));
        // And it is nobody's public trade.
        assert_eq!(x.engine(S).market_trade_count(), 0);
        assert_eq!(x.engine(S).market_unique_traders(), 1);
    }

    #[test]
    fn a_print_at_or_through_a_pretend_bid_inside_the_spread_fills_it_for_no_more_than_the_print() {
        let (mut x, mut m) = rig();
        x.place_limit(S, A, Side::Bid, tick(550), Qty(10), TradeSource::Ui).unwrap();
        // Someone on Gemini sells 3 at 55¢: a real bid there would have had them.
        assert!(m.on_event(&mut x, &print(SYMBOL, "0.55", "3", true)));
        assert_eq!(filled(&x, A), (3, 3 * 550));
        // A sale at 54¢ went through A's price: 4 more, still at 55¢.
        m.on_event(&mut x, &print(SYMBOL, "0.54", "4.5", true));
        assert_eq!(filled(&x, A), (7, 7 * 550));
        // A buy at 56¢ is on the other side of A's bid.
        m.on_event(&mut x, &print(SYMBOL, "0.56", "5", false));
        assert_eq!(filled(&x, A), (7, 7 * 550));
        // The mirror's own levels were not touched by any of it.
        assert_eq!(mirrors(&x, &m), (vec![(540, 10), (530, 25)], vec![(560, 7), (580, 20)]));
        // Gemini's three prints are the tape and the volume: 3 + 4 + 5.
        assert_eq!(x.engine(S).market_trade_count(), 3);
        assert_eq!(x.engine(S).market_volume_tenths(), 12_000);
    }

    #[test]
    fn a_pretend_ask_inside_the_spread_fills_on_a_print_through_it() {
        let (mut x, mut m) = rig();
        x.place_limit(S, A, Side::Ask, tick(550), Qty(5), TradeSource::Ui).unwrap();
        m.on_event(&mut x, &print(SYMBOL, "0.56", "2", false));
        assert_eq!(filled(&x, A), (2, 2 * 550));
        m.on_event(&mut x, &print(SYMBOL, "0.54", "9", true));
        assert_eq!(filled(&x, A), (2, 2 * 550), "a sale below A's ask does not lift it");
    }

    #[test]
    fn a_pretend_order_at_or_behind_a_gemini_level_does_not_fill_on_a_print() {
        let (mut x, mut m) = rig();
        x.place_limit(S, A, Side::Bid, tick(540), Qty(5), TradeSource::Ui).unwrap();
        x.place_limit(S, B, Side::Bid, tick(530), Qty(5), TradeSource::Ui).unwrap();
        m.on_event(&mut x, &print(SYMBOL, "0.54", "10", true));
        m.on_event(&mut x, &print(SYMBOL, "0.53", "40", true));
        assert_eq!(filled(&x, A), (0, 0));
        assert_eq!(filled(&x, B), (0, 0));
        assert_eq!(mirrors(&x, &m).0, vec![(540, 10), (530, 25)], "the mirror never trades with itself");
    }

    #[test]
    fn geminis_prints_are_the_tape_with_the_side_that_took() {
        let (mut x, mut m) = rig();
        m.on_event(&mut x, &print(SYMBOL, "0.56", "2", false));
        m.on_event(&mut x, &print(SYMBOL, "0.54", "9.7", true));
        m.on_event(&mut x, &print(SYMBOL, "0.55", "0.4", true));
        let tape: Vec<_> = x.engine(S).recent_trades(10).map(|f| (f.taker_side, f.price.tenths(), f.qty.0)).collect();
        // Newest first; under one contract is not a row.
        assert_eq!(tape, vec![(Side::Ask, 540, 9), (Side::Bid, 560, 2)]);
        assert_eq!(x.engine(S).market_unique_traders(), 0);
    }

    #[test]
    fn when_the_link_drops_every_mirror_order_goes_at_once_and_the_book_is_stale() {
        let (mut x, mut m) = rig();
        x.place_limit(S, A, Side::Bid, tick(550), Qty(4), TradeSource::Ui).unwrap();
        assert!(m.on_event(&mut x, &WsEvent::Disconnected));
        assert_eq!(m.book_state(), BookState::Stale);
        assert_eq!(mirrors(&x, &m), (vec![], vec![]));
        assert_eq!(book(&x), (vec![(550, 4)], vec![]), "a person's order stays");
        // With no book there is nothing to take.
        assert_eq!(x.place_market(S, B, Side::Bid, Qty(1), TradeSource::Ui).unwrap_err(), OrderError::NoLiquidity);
        // A change with no book before it is not a book.
        m.on_event(&mut x, &WsEvent::Connected);
        assert!(!m.on_event(&mut x, &depth(SYMBOL, &[("0.50", "5")], &[], false)));
        assert_eq!(m.book_state(), BookState::Stale);
        // The next whole book brings it back.
        assert!(m.on_event(&mut x, &snapshot()));
        assert_eq!(m.book_state(), BookState::Live);
        assert_eq!(mirrors(&x, &m), (vec![(540, 10), (530, 25)], vec![(560, 7), (580, 20)]));
    }

    #[test]
    fn switching_contract_clears_the_book_and_waits_for_the_new_one() {
        let (mut x, mut m) = rig();
        m.switch_to(&mut x, Some(OTHER.into()));
        assert_eq!(mirrors(&x, &m), (vec![], vec![]));
        assert_eq!(m.book_state(), BookState::Stale);
        // The old contract's late messages are ignored.
        assert!(!m.on_event(&mut x, &snapshot()));
        assert!(m.on_event(&mut x, &depth(OTHER, &[("0.40", "3")], &[("0.45", "3")], true)));
        assert_eq!(book(&x), (vec![(400, 3)], vec![(450, 3)]));
        m.switch_to(&mut x, None);
        assert_eq!(m.book_state(), BookState::Waiting);
        assert_eq!(book(&x), (vec![], vec![]));
    }

    #[test]
    fn a_settled_round_leaves_the_mirror_nothing_to_undo() {
        let (mut x, mut m) = rig();
        x.place_market(S, A, Side::Bid, Qty(5), TradeSource::Ui).unwrap();
        // The round settles under the mirror: the engine cancels every order.
        x.resolve_and_open_next(S, 100_001.0, 100_001.0, OPENS + 300, 300);
        assert_eq!(book(&x), (vec![], vec![]));
        // The same contract's next message (the driver has not switched
        // it yet) rebuilds the whole book in the new round, the level A
        // took from included.
        assert!(m.on_event(&mut x, &depth(SYMBOL, &[], &[("0.58", "21")], false)));
        assert_eq!(book(&x), (vec![(540, 10), (530, 25)], vec![(560, 7), (580, 21)]));
        assert_eq!(mirrors(&x, &m), book(&x));
    }

    #[test]
    fn after_the_close_the_mirror_leaves_the_locked_book_alone() {
        let (mut x, mut m) = rig();
        let now = Arc::new(AtomicU64::new(OPENS + 10));
        let reads = now.clone();
        x.engine_mut(S).set_clock(RoundClock::new(move || reads.load(Ordering::SeqCst)));
        x.engine_mut(S).set_lock_at_close(true);
        now.store(OPENS + 300, Ordering::SeqCst);
        assert!(!m.on_event(&mut x, &depth(SYMBOL, &[("0.54", "0")], &[("0.56", "9")], false)));
        assert!(!m.on_event(&mut x, &print(SYMBOL, "0.55", "3", true)), "no fill after the close");
        assert_eq!(book(&x), (vec![(540, 10), (530, 25)], vec![(560, 7), (580, 20)]));
        assert_eq!(x.engine(S).market_trade_count(), 1, "the print itself is still Gemini's tape");
    }
}
