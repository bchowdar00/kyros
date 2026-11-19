//! Orders that live on an outside venue. The engine does not match them;
//! it keeps their names, books each fill the venue reports against the
//! series' outside-counterparty account, and reports their fates through
//! the same execution sink as its own orders.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use super::events::{ExecKind, ExecReject, ExecutionEvent, Recorder};
use super::{Engine, ExternalError, OrderError, Outcome, RoundClock, RoundClose, TradeSource};
use crate::ticks::PriceTick;
use crate::types::{Address, DEFAULT_SERIES, OrderId, Qty, SeriesId, Side, Tenths};

const S: SeriesId = DEFAULT_SERIES;
const START: Tenths = Tenths(1_000_000);
const BANK: Tenths = Tenths(1_000_000_000);
const A: Address = Address([0xaa; 20]);
const B: Address = Address([0xbb; 20]);
const MIRROR: Address = Address([0x6e; 20]);
const OUTSIDE: Address = Address([0x0e; 20]);
const OPENS: u64 = 1_900_000_000;
const CLOSES: u64 = OPENS + 300;

fn tick(t: u16) -> PriceTick {
    PriceTick::from_tenths(t).unwrap()
}

/// Two people, a mirror with a resting book, an outside counterparty, a
/// recorder and a clock the test moves. The round locks at its close.
fn rig() -> (Engine, Arc<AtomicU64>, Recorder) {
    let mut e = Engine::new(START);
    for a in [A, B] {
        e.get_or_create_account(a);
        e.register_user(a);
    }
    e.seed_bot_account_in(MIRROR, BANK, "gemini-book".into(), None, None, S);
    e.seed_bot_account_in(OUTSIDE, BANK, "gemini-fills".into(), None, None, S);
    e.open_first_market(100_000.0, OPENS, 300);
    e.ctx().place_limit(MIRROR, Side::Ask, tick(600), Qty(10), TradeSource::Bot).unwrap();
    e.ctx().place_limit(MIRROR, Side::Bid, tick(500), Qty(10), TradeSource::Bot).unwrap();
    let now = Arc::new(AtomicU64::new(OPENS + 10));
    let reads = now.clone();
    let rec = Recorder::new();
    let x = e.exchange_mut();
    x.set_execution_sink(rec.tx.clone());
    x.engine_mut(S).set_mirror(MIRROR);
    x.set_outside(S, OUTSIDE);
    x.engine_mut(S).set_clock(RoundClock::new(move || reads.load(Ordering::SeqCst)));
    x.engine_mut(S).set_lock_at_close(true);
    (e, now, rec)
}

fn cash(e: &Engine, a: Address) -> i64 {
    e.account(&a).unwrap().usdc_balance.0
}

fn kinds(evs: &[ExecutionEvent]) -> Vec<(String, ExecKind, u32, u32, Option<u16>)> {
    evs.iter()
        .map(|e| (e.client_order_id.to_string(), e.kind, e.filled_qty.0, e.remaining_qty.0, e.price.map(|p| p.tenths())))
        .collect()
}

// ───────────────────────── the ledger step ─────────────────────────

#[test]
fn an_outside_fill_moves_cash_and_position_on_both_sides_and_touches_no_book() {
    let (mut e, _, mut rec) = rig();
    e.exchange_mut().apply_external_fill(S, A, Side::Bid, tick(550), Qty(4)).unwrap();

    assert_eq!(e.e().position(&A).position.0, 4);
    assert_eq!(e.e().position(&OUTSIDE).position.0, -4);
    assert_eq!(cash(&e, A), START.0 - 4 * 550);
    assert_eq!(cash(&e, OUTSIDE), BANK.0 - 4 * 450);
    assert_eq!(e.e().open_interest(), 4);
    // The mirrored book is as it was: 10 offered at 60, 10 bid at 50.
    assert_eq!(e.e().book().best_ask().map(|p| (p.tenths(), e.e().book().level_qty(p).0)), Some((600, 10)));
    assert_eq!(e.e().book().best_bid().map(|p| (p.tenths(), e.e().book().level_qty(p).0)), Some((500, 10)));
    // A bare ledger step names no order, so it reports nothing.
    assert!(rec.take().is_empty());
    e.debug_assert_invariants();
}

#[test]
fn an_outside_fill_is_on_no_tape_and_counts_one_trader() {
    let (mut e, _, _) = rig();
    e.exchange_mut().apply_external_fill(S, A, Side::Bid, tick(550), Qty(4)).unwrap();
    let m = e.e();
    assert_eq!(m.market_volume_tenths(), 0, "the venue's own print is the tape");
    assert_eq!(m.market_trade_count(), 0);
    assert_eq!(m.market_unique_traders(), 1, "the counterparty is not a trader");
    assert_eq!(m.recent_trades(10).count(), 0);
    // The person's own history has it.
    let rows = m.user_orders(&A);
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0].filled_qty, rows[0].limit_price, rows[0].side), (Qty(4), Some(550), Side::Bid));
}

#[test]
fn an_outside_fill_closes_a_position_and_realizes_its_profit() {
    let (mut e, _, _) = rig();
    e.exchange_mut().apply_external_fill(S, A, Side::Bid, tick(550), Qty(4)).unwrap();
    e.exchange_mut().apply_external_fill(S, A, Side::Ask, tick(700), Qty(4)).unwrap();
    assert_eq!(e.e().position(&A).position.0, 0);
    assert_eq!(e.e().position(&OUTSIDE).position.0, 0);
    assert_eq!(e.e().open_interest(), 0);
    assert_eq!(cash(&e, A), START.0 + 4 * 150);
    assert_eq!(e.e().position(&A).realized_pnl_this_market.0, 4 * 150);
    e.debug_assert_invariants();
}

#[test]
fn an_outside_fill_still_books_after_the_round_has_stopped_taking_orders() {
    let (mut e, now, _) = rig();
    now.store(CLOSES + 1, Ordering::SeqCst);
    assert!(e.e().is_locked());
    e.exchange_mut().apply_external_fill(S, A, Side::Bid, tick(550), Qty(1)).unwrap();
    assert_eq!(e.e().position(&A).position.0, 1);
}

#[test]
fn an_outside_fill_is_refused_when_it_cannot_be_booked() {
    let (mut e, _, _) = rig();
    let x = e.exchange_mut();
    assert_eq!(x.apply_external_fill(S, A, Side::Bid, tick(550), Qty(0)), Err(OrderError::ZeroQuantity));
    assert_eq!(x.apply_external_fill(S, Address([0x77; 20]), Side::Bid, tick(550), Qty(1)), Err(OrderError::UnknownAccount));
    assert_eq!(x.apply_external_fill(S, OUTSIDE, Side::Bid, tick(550), Qty(1)), Err(OrderError::UnknownAccount), "the counterparty cannot trade itself");
    // A series with no counterparty named has nobody to book against.
    let mut bare = Engine::new(START);
    bare.get_or_create_account(A);
    bare.open_first_market(100_000.0, OPENS, 300);
    assert_eq!(bare.exchange_mut().apply_external_fill(S, A, Side::Bid, tick(550), Qty(1)), Err(OrderError::UnknownAccount));
    assert_eq!(bare.e().position(&A).position.0, 0);
}

// ───────────────────────── named orders ─────────────────────────

#[test]
fn opening_an_outside_order_is_silent_and_locks_nothing() {
    let (mut e, _, mut rec) = rig();
    let id = e.exchange_mut().external_open(S, A, Arc::from("c1"), Qty(5)).unwrap();
    assert!(e.exchange_mut().external_is_live(S, id));
    assert!(rec.take().is_empty(), "the placement reply carries the id");
    assert_eq!(cash(&e, A), START.0);
    assert_eq!(e.account(&A).unwrap().locked.0, 0);
    e.debug_assert_invariants();
}

#[test]
fn an_outside_order_is_refused_once_the_round_has_stopped_taking_orders() {
    let (mut e, now, _) = rig();
    now.store(CLOSES, Ordering::SeqCst);
    assert_eq!(e.exchange_mut().external_open(S, A, Arc::from("c1"), Qty(5)), Err(OrderError::MarketClosed));
    assert_eq!(e.exchange_mut().external_open(S, A, Arc::from("c1"), Qty(0)), Err(OrderError::MarketClosed));
}

#[test]
fn an_outside_order_needs_a_size_and_a_known_account() {
    let (mut e, _, _) = rig();
    let x = e.exchange_mut();
    assert_eq!(x.external_open(S, A, Arc::from("c1"), Qty(0)), Err(OrderError::ZeroQuantity));
    assert_eq!(x.external_open(S, Address([0x77; 20]), Arc::from("c1"), Qty(1)), Err(OrderError::UnknownAccount));
}

#[test]
fn fills_on_an_outside_order_report_partial_then_fill_and_book_each_one() {
    let (mut e, _, mut rec) = rig();
    let id = e.exchange_mut().external_open(S, A, Arc::from("c1"), Qty(5)).unwrap();
    e.exchange_mut().external_fill(S, id, Side::Bid, tick(550), Qty(2)).unwrap();
    e.exchange_mut().external_fill(S, id, Side::Bid, tick(540), Qty(3)).unwrap();

    let evs = rec.take();
    assert_eq!(
        kinds(&evs),
        vec![
            ("c1".to_string(), ExecKind::Partial, 2, 3, Some(550)),
            ("c1".to_string(), ExecKind::Fill, 3, 0, Some(540)),
        ]
    );
    assert!(evs.iter().all(|ev| ev.venue_order_id == Some(id) && ev.account == A && ev.series == S));
    assert_eq!(e.e().position(&A).position.0, 5);
    assert_eq!(cash(&e, A), START.0 - 2 * 550 - 3 * 540);
    assert!(!e.exchange_mut().external_is_live(S, id), "a filled order is gone");
    e.debug_assert_invariants();
}

#[test]
fn a_fill_for_an_order_the_engine_no_longer_holds_changes_nothing() {
    let (mut e, _, mut rec) = rig();
    let id = e.exchange_mut().external_open(S, A, Arc::from("c1"), Qty(2)).unwrap();
    e.exchange_mut().external_fill(S, id, Side::Bid, tick(550), Qty(2)).unwrap();
    rec.take();
    assert_eq!(e.exchange_mut().external_fill(S, id, Side::Bid, tick(550), Qty(1)), Err(ExternalError::UnknownOrder));
    assert_eq!(e.exchange_mut().external_fill(S, OrderId(999_999), Side::Bid, tick(550), Qty(1)), Err(ExternalError::UnknownOrder));
    assert_eq!(e.e().position(&A).position.0, 2);
    assert!(rec.take().is_empty());
}

#[test]
fn a_fill_larger_than_what_was_left_is_booked_whole_and_ends_the_order() {
    // The venue's report is what happened; the ledger follows it.
    let (mut e, _, mut rec) = rig();
    let id = e.exchange_mut().external_open(S, A, Arc::from("c1"), Qty(2)).unwrap();
    e.exchange_mut().external_fill(S, id, Side::Bid, tick(550), Qty(3)).unwrap();
    assert_eq!(kinds(&rec.take()), vec![("c1".to_string(), ExecKind::Fill, 3, 0, Some(550))]);
    assert_eq!(e.e().position(&A).position.0, 3);
}

#[test]
fn closing_an_outside_order_reports_its_fate_once() {
    let (mut e, _, mut rec) = rig();
    let x = e.exchange_mut();
    let cancelled = x.external_open(S, A, Arc::from("c1"), Qty(5)).unwrap();
    let refused = x.external_open(S, A, Arc::from("c2"), Qty(5)).unwrap();
    let lapsed = x.external_open(S, B, Arc::from("c3"), Qty(5)).unwrap();
    let replaced = x.external_open(S, B, Arc::from("c4"), Qty(5)).unwrap();
    assert!(x.external_close(S, cancelled, ExecKind::CancelAck));
    assert!(x.external_close(S, refused, ExecKind::Reject(ExecReject::InsufficientBalance)));
    assert!(x.external_close(S, lapsed, ExecKind::Expiry));
    assert!(x.external_close(S, replaced, ExecKind::Replaced));
    assert!(!x.external_close(S, cancelled, ExecKind::CancelAck), "already gone");
    assert_eq!(
        kinds(&rec.take()),
        vec![
            ("c1".to_string(), ExecKind::CancelAck, 0, 0, None),
            ("c2".to_string(), ExecKind::Reject(ExecReject::InsufficientBalance), 0, 0, None),
            ("c3".to_string(), ExecKind::Expiry, 0, 0, None),
            ("c4".to_string(), ExecKind::Replaced, 0, 0, None),
        ]
    );
}

#[test]
fn a_partly_filled_outside_order_can_still_be_cancelled() {
    let (mut e, _, mut rec) = rig();
    let id = e.exchange_mut().external_open(S, A, Arc::from("c1"), Qty(5)).unwrap();
    e.exchange_mut().external_fill(S, id, Side::Ask, tick(520), Qty(2)).unwrap();
    assert!(e.exchange_mut().external_close(S, id, ExecKind::CancelAck));
    assert_eq!(
        kinds(&rec.take()),
        vec![("c1".to_string(), ExecKind::Partial, 2, 3, Some(520)), ("c1".to_string(), ExecKind::CancelAck, 0, 0, None)]
    );
    assert_eq!(e.e().position(&A).position.0, -2);
}

// ───────────────────────── settlement and sweeps ─────────────────────────

#[test]
fn settlement_acks_every_live_outside_order_and_pays_the_booked_position() {
    let (mut e, _, mut rec) = rig();
    let filled = e.exchange_mut().external_open(S, A, Arc::from("c1"), Qty(4)).unwrap();
    e.exchange_mut().external_fill(S, filled, Side::Bid, tick(550), Qty(4)).unwrap();
    let first = e.exchange_mut().external_open(S, A, Arc::from("c2"), Qty(1)).unwrap();
    let second = e.exchange_mut().external_open(S, B, Arc::from("c3"), Qty(1)).unwrap();
    rec.take();

    e.exchange_mut().resolve_only(S, Outcome::Yes, RoundClose::Btc { close_price_btc_usd: 100_050.0 });

    let evs = rec.take();
    assert_eq!(
        kinds(&evs),
        vec![("c2".to_string(), ExecKind::CancelAck, 0, 0, None), ("c3".to_string(), ExecKind::CancelAck, 0, 0, None)],
        "in the order they were opened"
    );
    assert_eq!(evs[0].venue_order_id, Some(first));
    assert_eq!(evs[1].venue_order_id, Some(second));
    assert!(!e.exchange_mut().external_is_live(S, first));
    // Four contracts bought at 55 cents pay a dollar each.
    assert_eq!(cash(&e, A), START.0 - 4 * 550 + 4 * 1000);
    assert_eq!(e.e().position(&A).position.0, 0);
}

#[test]
fn sweeping_an_account_acks_its_outside_orders_and_nobody_elses() {
    let (mut e, _, mut rec) = rig();
    let mine = e.exchange_mut().external_open(S, A, Arc::from("c1"), Qty(1)).unwrap();
    let theirs = e.exchange_mut().external_open(S, B, Arc::from("c2"), Qty(1)).unwrap();
    assert!(e.exchange_mut().engine(S).has_resting_orders_for(&A), "an outside order is an order");
    let (engine, ledger) = e.parts();
    engine.cancel_resting_for(ledger, A);
    assert_eq!(kinds(&rec.take()), vec![("c1".to_string(), ExecKind::CancelAck, 0, 0, None)]);
    assert!(!e.exchange_mut().external_is_live(S, mine));
    assert!(e.exchange_mut().external_is_live(S, theirs));
    assert!(!e.exchange_mut().engine(S).has_resting_orders_for(&A));
}
