//! The lock at close: on a series that follows an outside venue, a round
//! takes no orders from its `closes_at` until the next round opens. Off
//! by default, where every case here must behave as it always did.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use super::events::{ExecKind, ExecReject, Recorder};
use super::market::RoundClock;
use super::{Engine, MarketStatus, OrderError, Outcome, PlaceOpts, ReplaceError, RoundClose, TradeSource};
use crate::ticks::PriceTick;
use crate::types::{Address, DEFAULT_SERIES, Qty, Side, Tenths};

const START: Tenths = Tenths(1_000_000);
const A: Address = Address([0xaa; 20]);
const B: Address = Address([0xbb; 20]);
const OPENS: u64 = 1_900_000_000;
const CLOSES: u64 = OPENS + 300;

fn tick(t: u16) -> PriceTick {
    PriceTick::from_tenths(t).unwrap()
}

/// Two funded accounts and a round from `OPENS` to `CLOSES`, with a
/// clock the test moves. A rests an ask of 5 at 60¢ before anything else.
fn rig(lock: bool) -> (Engine, Arc<AtomicU64>, Recorder) {
    let mut e = Engine::new(START);
    for a in [A, B] {
        e.get_or_create_account(a);
        // Users, not bots: a bot's balance is reset at settlement.
        e.register_user(a);
    }
    e.open_first_market(100_000.0, OPENS, 300);
    let now = Arc::new(AtomicU64::new(OPENS + 10));
    let reads = now.clone();
    let rec = Recorder::new();
    let x = e.exchange_mut();
    x.set_execution_sink(rec.tx.clone());
    x.engine_mut(DEFAULT_SERIES).set_clock(RoundClock::new(move || reads.load(Ordering::SeqCst)));
    x.engine_mut(DEFAULT_SERIES).set_lock_at_close(lock);
    e.ctx().place_limit(A, Side::Ask, tick(600), Qty(5), TradeSource::Ui).unwrap();
    (e, now, rec)
}

/// Cash and locked collateral of both accounts, and the resting ask.
fn money(e: &Engine) -> (Tenths, Tenths, Tenths, Tenths, u32) {
    let a = e.account(&A).unwrap();
    let b = e.account(&B).unwrap();
    let resting: u32 = e.e().book().iter_ask_orders().map(|o| o.qty_open.0).sum();
    (a.usdc_balance, a.locked, b.usdc_balance, b.locked, resting)
}

#[test]
fn before_the_close_a_locked_series_trades_as_usual() {
    let (mut e, now, _rec) = rig(true);
    now.store(CLOSES - 1, Ordering::SeqCst);
    assert!(!e.e().is_locked());
    let out = e.ctx().place_market(B, Side::Bid, Qty(2), TradeSource::Ui).unwrap();
    assert_eq!(out.fills.iter().map(|f| f.qty.0).sum::<u32>(), 2);
}

#[test]
fn at_the_close_a_limit_order_is_refused_and_moves_no_money() {
    let (mut e, now, _rec) = rig(true);
    let before = money(&e);
    now.store(CLOSES, Ordering::SeqCst);
    assert!(e.e().is_locked());
    let got = e.ctx().place_limit(B, Side::Bid, tick(600), Qty(3), TradeSource::Ui);
    assert_eq!(got.unwrap_err(), OrderError::MarketClosed);
    // A limit that would only rest is refused too.
    let got = e.ctx().place_limit(B, Side::Bid, tick(100), Qty(3), TradeSource::Ui);
    assert_eq!(got.unwrap_err(), OrderError::MarketClosed);
    assert_eq!(money(&e), before);
}

#[test]
fn after_the_close_a_market_order_is_refused_and_moves_no_money() {
    let (mut e, now, _rec) = rig(true);
    let before = money(&e);
    now.store(CLOSES + 7, Ordering::SeqCst);
    let got = e.ctx().place_market(B, Side::Bid, Qty(3), TradeSource::Ui);
    assert_eq!(got.unwrap_err(), OrderError::MarketClosed);
    assert_eq!(money(&e), before);
}

#[test]
fn a_refused_order_with_a_client_id_is_reported_as_market_closed() {
    let (mut e, now, mut rec) = rig(true);
    now.store(CLOSES, Ordering::SeqCst);
    let got = e.ctx().place_limit_with(
        B,
        Side::Bid,
        tick(600),
        Qty(3),
        TradeSource::Strategy,
        PlaceOpts::with_client_id("late"),
    );
    assert_eq!(got.unwrap_err(), OrderError::MarketClosed);
    let evs = rec.take();
    assert_eq!(evs.len(), 1);
    assert_eq!(evs[0].kind, ExecKind::Reject(ExecReject::MarketClosed));
    assert_eq!(&*evs[0].client_order_id, "late");
}

#[test]
fn a_resting_order_can_still_be_cancelled_after_the_close() {
    let (mut e, now, _rec) = rig(true);
    let id = e.e().book().iter_ask_orders().next().unwrap().id;
    now.store(CLOSES + 1, Ordering::SeqCst);
    e.ctx().cancel(A, id).unwrap();
    let a = e.account(&A).unwrap();
    assert_eq!(a.locked, Tenths::ZERO);
    assert_eq!(a.usdc_balance, START);
}

#[test]
fn a_replace_after_the_close_takes_the_old_order_off_and_refuses_the_new() {
    let (mut e, now, _rec) = rig(true);
    let id = e.e().book().iter_ask_orders().next().unwrap().id;
    now.store(CLOSES, Ordering::SeqCst);
    let got = e.ctx().cancel_replace(A, id, Side::Ask, tick(610), Qty(5), TradeSource::Ui, PlaceOpts::default());
    assert_eq!(got.unwrap_err(), ReplaceError::Place(OrderError::MarketClosed));
    assert_eq!(e.e().book().iter_ask_orders().count(), 0);
    assert_eq!(e.account(&A).unwrap().locked, Tenths::ZERO);
}

#[test]
fn the_next_round_takes_orders_again() {
    let (mut e, now, _rec) = rig(true);
    now.store(CLOSES + 5, Ordering::SeqCst);
    assert!(e.e().is_locked());
    e.resolve_and_open_next(100_001.0, 100_001.0, CLOSES, 300);
    assert!(!e.e().is_locked());
    e.ctx().place_limit(B, Side::Bid, tick(400), Qty(1), TradeSource::Ui).unwrap();
}

#[test]
fn with_the_lock_off_a_late_order_is_taken_as_it_always_was() {
    let (mut e, now, _rec) = rig(false);
    now.store(CLOSES + 60, Ordering::SeqCst);
    assert!(!e.e().is_locked());
    let out = e.ctx().place_market(B, Side::Bid, Qty(2), TradeSource::Ui).unwrap();
    assert_eq!(out.fills.iter().map(|f| f.qty.0).sum::<u32>(), 2);
    e.ctx().place_limit(B, Side::Bid, tick(100), Qty(1), TradeSource::Ui).unwrap();
}

#[test]
fn a_new_engine_has_the_lock_off() {
    let mut e = Engine::new(START);
    e.get_or_create_account(A);
    // Opened in 1970 and long closed by the system clock.
    e.open_first_market(100_000.0, 0, 120);
    assert!(!e.e().is_locked());
    e.ctx().place_limit(A, Side::Ask, tick(600), Qty(1), TradeSource::Ui).unwrap();
}

// ── A round that ends before the next one can open ──

#[test]
fn a_round_can_be_settled_without_opening_the_next() {
    let (mut e, _now, _rec) = rig(false);
    // B buys 2 of A's 5 at 60c; 3 stay resting.
    e.ctx().place_market(B, Side::Bid, Qty(2), TradeSource::Ui).unwrap();
    let report = e.exchange_mut().resolve_only(DEFAULT_SERIES, Outcome::Yes, RoundClose::Btc { close_price_btc_usd: 100_001.0 });
    assert_eq!(report.outcome, Outcome::Yes);
    assert_eq!(report.cancelled_orders, 1);
    assert!(e.e().current_market().is_none(), "no round is open");
    let settled = e.e().resolved_markets(1).next().copied().unwrap();
    assert_eq!(settled.id, report.resolved_id);
    assert!(matches!(settled.status, MarketStatus::Resolved { outcome: Outcome::Yes, .. }));
    // Yes won: B paid 2 x 60c and is paid 2 x $1; A keeps 2 x 60c and loses 2 x $1.
    let (a, b) = (e.account(&A).unwrap(), e.account(&B).unwrap());
    assert_eq!(b.usdc_balance, Tenths(START.0 + 2 * 400));
    assert_eq!(a.usdc_balance, Tenths(START.0 - 2 * 400));
    assert_eq!((a.locked, b.locked), (Tenths::ZERO, Tenths::ZERO));
    assert_eq!(e.e().book().iter_ask_orders().count(), 0);
}

#[test]
fn after_a_settle_without_a_next_round_a_new_first_round_opens() {
    let (mut e, _now, _rec) = rig(false);
    e.exchange_mut().resolve_only(DEFAULT_SERIES, Outcome::No, RoundClose::Btc { close_price_btc_usd: 1.0 });
    e.open_first_market(100_500.0, CLOSES + 600, 300);
    let m = e.e().current_market().copied().unwrap();
    assert_eq!((m.opened_at, m.closes_at), (CLOSES + 600, CLOSES + 900));
    e.ctx().place_limit(A, Side::Ask, tick(600), Qty(1), TradeSource::Ui).unwrap();
}

#[test]
fn a_locked_series_with_no_open_round_refuses_orders() {
    let (mut e, now, _rec) = rig(true);
    now.store(OPENS + 10, Ordering::SeqCst);
    e.exchange_mut().resolve_only(DEFAULT_SERIES, Outcome::No, RoundClose::Btc { close_price_btc_usd: 1.0 });
    assert!(e.e().is_locked());
    let got = e.ctx().place_limit(B, Side::Bid, tick(400), Qty(1), TradeSource::Ui);
    assert_eq!(got.unwrap_err(), OrderError::MarketClosed);
}

#[test]
fn an_unlocked_series_with_no_open_round_behaves_as_it_always_did() {
    let (mut e, _now, _rec) = rig(false);
    e.exchange_mut().resolve_only(DEFAULT_SERIES, Outcome::No, RoundClose::Btc { close_price_btc_usd: 1.0 });
    assert!(!e.e().is_locked());
    e.ctx().place_limit(B, Side::Bid, tick(400), Qty(1), TradeSource::Ui).unwrap();
}
