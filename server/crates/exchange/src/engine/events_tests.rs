//! Execution-event coverage: every taker / maker / cancel / lock path emits
//! exactly the expected events for client-id orders and nothing for orders
//! without one.

use std::sync::Arc;

use super::events::{ExecKind, ExecReject, ExecutionEvent, Recorder};
use super::{Engine, OrderStatus, PlaceOpts, TradeSource};
use crate::ticks::PriceTick;
use crate::types::{Address, Qty, Side, Tenths};

const START: Tenths = Tenths(1_000_000);
const A: Address = Address([0xaa; 20]);
const B: Address = Address([0xbb; 20]);
const C: Address = Address([0xcc; 20]);

fn tick(t: u16) -> PriceTick {
    PriceTick::from_tenths(t).unwrap()
}

fn cid(s: &str) -> PlaceOpts {
    PlaceOpts::with_client_id(s)
}

/// An engine with three funded accounts, a recorder, and an open market.
fn rig() -> (Engine, Recorder) {
    let mut e = Engine::new(START);
    for a in [A, B, C] {
        e.get_or_create_account(a);
    }
    e.open_first_market(100_000.0, 0, 120);
    let rec = Recorder::new();
    e.exchange_mut().set_execution_sink(rec.tx.clone());
    (e, rec)
}

fn kinds(evs: &[ExecutionEvent]) -> Vec<(String, ExecKind, u32, u32)> {
    evs.iter()
        .map(|e| (e.client_order_id.to_string(), e.kind, e.filled_qty.0, e.remaining_qty.0))
        .collect()
}

#[test]
fn orders_without_a_client_id_emit_nothing() {
    let (mut e, mut rec) = rig();
    let out = e.ctx().place_limit(A, Side::Ask, tick(600), Qty(5), TradeSource::Bot).unwrap();
    e.ctx().place_market(B, Side::Bid, Qty(2), TradeSource::Ui).unwrap();
    e.ctx().cancel(A, out.order_id).unwrap();
    e.resolve_and_open_next(100_001.0, 100_001.0, 120, 120);
    assert!(rec.take().is_empty());
}

#[test]
fn maker_and_taker_full_fill() {
    let (mut e, mut rec) = rig();
    let maker = e.ctx().place_limit_with(A, Side::Ask, tick(600), Qty(5), TradeSource::Strategy, cid("m")).unwrap();
    assert!(rec.take().is_empty(), "a pure rest is silent");
    let taker = e.ctx().place_limit_with(B, Side::Bid, tick(600), Qty(5), TradeSource::Strategy, cid("t")).unwrap();
    let evs = rec.take();
    assert_eq!(
        kinds(&evs),
        vec![("m".into(), ExecKind::Fill, 5, 0), ("t".into(), ExecKind::Fill, 5, 0)],
        "maker first (inside the match), then the taker summary"
    );
    assert_eq!(evs[0].venue_order_id, Some(maker.order_id));
    assert_eq!(evs[1].venue_order_id, Some(taker.order_id));
    assert!(evs.iter().all(|e| e.price == Some(tick(600))));
    assert!(evs.iter().all(|e| e.market.is_some()));
    assert_eq!(evs[0].account, A);
    assert_eq!(evs[1].account, B);
}

#[test]
fn taker_partial_rests_then_cancel_acks() {
    let (mut e, mut rec) = rig();
    e.ctx().place_limit_with(A, Side::Ask, tick(600), Qty(4), TradeSource::Strategy, cid("m")).unwrap();
    let taker = e.ctx().place_limit_with(B, Side::Bid, tick(600), Qty(10), TradeSource::Strategy, cid("t")).unwrap();
    assert_eq!(taker.status, OrderStatus::PartiallyFilled);
    assert_eq!(
        kinds(&rec.take()),
        vec![("m".into(), ExecKind::Fill, 4, 0), ("t".into(), ExecKind::Partial, 4, 6)]
    );
    e.ctx().cancel(B, taker.order_id).unwrap();
    let evs = rec.take();
    assert_eq!(kinds(&evs), vec![("t".into(), ExecKind::CancelAck, 0, 0)]);
    assert_eq!(evs[0].venue_order_id, Some(taker.order_id));
}

#[test]
fn maker_partial_stays_resting() {
    let (mut e, mut rec) = rig();
    e.ctx().place_limit_with(A, Side::Ask, tick(600), Qty(10), TradeSource::Strategy, cid("m")).unwrap();
    e.ctx().place_market_with(B, Side::Bid, Qty(3), TradeSource::Strategy, cid("t")).unwrap();
    assert_eq!(
        kinds(&rec.take()),
        vec![("m".into(), ExecKind::Partial, 3, 7), ("t".into(), ExecKind::Fill, 3, 0)]
    );
    e.ctx().place_market_with(C, Side::Bid, Qty(7), TradeSource::Strategy, cid("u")).unwrap();
    assert_eq!(
        kinds(&rec.take()),
        vec![("m".into(), ExecKind::Fill, 7, 0), ("u".into(), ExecKind::Fill, 7, 0)]
    );
}

#[test]
fn taker_across_two_prices_reports_one_event_per_price() {
    let (mut e, mut rec) = rig();
    e.ctx().place_limit(A, Side::Ask, tick(600), Qty(2), TradeSource::Bot).unwrap();
    e.ctx().place_limit(A, Side::Ask, tick(700), Qty(3), TradeSource::Bot).unwrap();
    e.ctx().place_limit_with(B, Side::Bid, tick(700), Qty(5), TradeSource::Strategy, cid("t")).unwrap();
    let evs = rec.take();
    assert_eq!(
        kinds(&evs),
        vec![("t".into(), ExecKind::Partial, 2, 3), ("t".into(), ExecKind::Fill, 3, 0)]
    );
    assert_eq!(evs[0].price, Some(tick(600)));
    assert_eq!(evs[1].price, Some(tick(700)));
}

#[test]
fn market_order_killed_remainder_is_a_partial_with_zero_remaining() {
    let (mut e, mut rec) = rig();
    e.ctx().place_limit(A, Side::Ask, tick(600), Qty(2), TradeSource::Bot).unwrap();
    let out = e.ctx().place_market_with(B, Side::Bid, Qty(10), TradeSource::Strategy, cid("t")).unwrap();
    assert_eq!(out.status, OrderStatus::Killed);
    assert_eq!(kinds(&rec.take()), vec![("t".into(), ExecKind::Partial, 2, 0)]);
}

#[test]
fn engine_errors_are_rejects() {
    let (mut e, mut rec) = rig();
    let err = e.ctx().place_market_with(B, Side::Bid, Qty(10), TradeSource::Strategy, cid("t")).unwrap_err();
    assert_eq!(err, super::OrderError::NoLiquidity);
    let evs = rec.take();
    assert_eq!(evs.len(), 1);
    assert_eq!(evs[0].kind, ExecKind::Reject(ExecReject::NoLiquidity));
    assert_eq!(evs[0].venue_order_id, None);
    assert_eq!(evs[0].remaining_qty, Qty(10));

    e.ctx().place_limit_with(B, Side::Bid, tick(500), Qty(0), TradeSource::Strategy, cid("z")).unwrap_err();
    assert_eq!(rec.take()[0].kind, ExecKind::Reject(ExecReject::ZeroQuantity));

    // 10,000 × 99.9¢ = far more than the $1,000 balance.
    e.ctx().place_limit_with(B, Side::Bid, tick(999), Qty(10_000), TradeSource::Strategy, cid("big")).unwrap_err();
    assert_eq!(rec.take()[0].kind, ExecKind::Reject(ExecReject::InsufficientBalance));
}

#[test]
fn self_trade_prevention_acks_the_resting_order() {
    let (mut e, mut rec) = rig();
    let ask = e.ctx().place_limit_with(A, Side::Ask, tick(600), Qty(5), TradeSource::Strategy, cid("ask")).unwrap();
    let bid = e.ctx().place_limit_with(A, Side::Bid, tick(600), Qty(5), TradeSource::Strategy, cid("bid")).unwrap();
    assert_eq!(bid.status, OrderStatus::Rested, "own ask was cancelled, bid rests");
    let evs = rec.take();
    assert_eq!(kinds(&evs), vec![("ask".into(), ExecKind::CancelAck, 0, 0)]);
    assert_eq!(evs[0].venue_order_id, Some(ask.order_id));
}

#[test]
fn ioc_limit_kills_the_residual_without_an_ack() {
    let (mut e, mut rec) = rig();
    e.ctx().place_limit(A, Side::Ask, tick(600), Qty(2), TradeSource::Bot).unwrap();
    let ioc = PlaceOpts { ioc: true, ..cid("t") };
    let out = e.ctx().place_limit_with(B, Side::Bid, tick(600), Qty(5), TradeSource::Strategy, ioc.clone()).unwrap();
    assert_eq!(out.status, OrderStatus::Killed);
    assert_eq!(out.resting_qty, Qty::ZERO);
    assert!(e.market().book().is_empty(), "nothing rests");
    assert_eq!(kinds(&rec.take()), vec![("t".into(), ExecKind::Partial, 2, 0)]);
    let b = e.account(&B).unwrap();
    assert_eq!(b.locked, Tenths::ZERO, "residual lock refunded");
    assert_eq!(b.usdc_balance, Tenths(START.0 - 2 * 600));

    // Nothing to hit at all: Expiry, still no rest.
    let out = e.ctx().place_limit_with(B, Side::Bid, tick(500), Qty(5), TradeSource::Strategy, PlaceOpts { ioc: true, ..cid("u") }).unwrap();
    assert_eq!(out.status, OrderStatus::Killed);
    assert_eq!(kinds(&rec.take()), vec![("u".into(), ExecKind::Expiry, 0, 0)]);
    assert!(e.market().book().is_empty());
    e.exchange().debug_assert_invariants();
}

#[test]
fn post_only_refuses_to_cross_and_rests_otherwise() {
    let (mut e, mut rec) = rig();
    e.ctx().place_limit(A, Side::Ask, tick(600), Qty(2), TradeSource::Bot).unwrap();
    let po = PlaceOpts { post_only: true, ..cid("t") };
    let err = e.ctx().place_limit_with(B, Side::Bid, tick(600), Qty(5), TradeSource::Strategy, po.clone()).unwrap_err();
    assert_eq!(err, super::OrderError::PostOnlyWouldCross);
    assert_eq!(rec.take()[0].kind, ExecKind::Reject(ExecReject::PostOnlyWouldCross));
    let out = e.ctx().place_limit_with(B, Side::Bid, tick(590), Qty(5), TradeSource::Strategy, po).unwrap();
    assert_eq!(out.status, OrderStatus::Rested);
    assert!(rec.take().is_empty());
    e.exchange().debug_assert_invariants();
}

#[test]
fn reduce_only_clamps_to_the_position_or_rejects() {
    let (mut e, mut rec) = rig();
    // B goes long 3 YES.
    e.ctx().place_limit(A, Side::Ask, tick(600), Qty(3), TradeSource::Bot).unwrap();
    e.ctx().place_market(B, Side::Bid, Qty(3), TradeSource::Ui).unwrap();
    assert_eq!(e.position(&B).position.0, 3);
    let ro = PlaceOpts { reduce_only: true, ..cid("x") };
    // Buying more would extend the long: refused.
    let err = e.ctx().place_limit_with(B, Side::Bid, tick(500), Qty(1), TradeSource::Strategy, ro.clone()).unwrap_err();
    assert_eq!(err, super::OrderError::ReduceOnlyNoPosition);
    assert_eq!(rec.take()[0].kind, ExecKind::Reject(ExecReject::ReduceOnlyNoPosition));
    // Selling 10 is clamped to the 3 held.
    e.ctx().place_limit(A, Side::Bid, tick(500), Qty(10), TradeSource::Bot).unwrap();
    let out = e.ctx().place_market_with(B, Side::Ask, Qty(10), TradeSource::Strategy, PlaceOpts { reduce_only: true, ..cid("y") }).unwrap();
    assert_eq!(out.status, OrderStatus::Filled);
    assert_eq!(kinds(&rec.take()), vec![("y".into(), ExecKind::Fill, 3, 0)]);
    assert_eq!(e.position(&B).position.0, 0);
    e.exchange().debug_assert_invariants();
}

#[test]
fn expire_reports_expiry_instead_of_cancel_ack() {
    let (mut e, mut rec) = rig();
    let out = e.ctx().place_limit_with(A, Side::Ask, tick(600), Qty(2), TradeSource::Strategy, cid("gtd")).unwrap();
    e.ctx().expire(A, out.order_id).unwrap();
    let evs = rec.take();
    assert_eq!(kinds(&evs), vec![("gtd".into(), ExecKind::Expiry, 0, 0)]);
    assert!(e.market().book().is_empty());
    assert_eq!(e.account(&A).unwrap().locked, Tenths::ZERO);
    assert!(e.ctx().expire(A, out.order_id).is_err(), "already gone");
}

#[test]
fn cancel_replace_reports_replaced_then_the_new_order() {
    let (mut e, mut rec) = rig();
    let old = e.ctx().place_limit_with(A, Side::Bid, tick(500), Qty(5), TradeSource::Strategy, cid("v1")).unwrap();
    let new = e.ctx().cancel_replace(A, old.order_id, Side::Bid, tick(520), Qty(6), TradeSource::Strategy, cid("v2")).unwrap();
    assert_eq!(new.status, OrderStatus::Rested);
    let evs = rec.take();
    assert_eq!(kinds(&evs), vec![("v1".into(), ExecKind::Replaced, 0, 0)], "a pure rest of v2 is silent");
    assert_eq!(evs[0].venue_order_id, Some(old.order_id));
    assert_eq!(e.market().book().len(), 1);
    assert_eq!(e.account(&A).unwrap().locked, Tenths(6 * 520));

    // The replacement can take: v2 replaced by a crossing v3.
    e.ctx().place_limit(B, Side::Ask, tick(530), Qty(2), TradeSource::Bot).unwrap();
    rec.take();
    let v3 = e.ctx().cancel_replace(A, new.order_id, Side::Bid, tick(530), Qty(6), TradeSource::Strategy, cid("v3")).unwrap();
    assert_eq!(v3.status, OrderStatus::PartiallyFilled);
    assert_eq!(
        kinds(&rec.take()),
        vec![("v2".into(), ExecKind::Replaced, 0, 0), ("v3".into(), ExecKind::Partial, 2, 4)]
    );

    // Unknown or foreign old id: nothing happens, no event.
    let err = e.ctx().cancel_replace(B, v3.order_id, Side::Bid, tick(500), Qty(1), TradeSource::Strategy, cid("v4")).unwrap_err();
    assert_eq!(err, super::ReplaceError::Cancel(super::CancelError::NotOwner));
    assert!(rec.take().is_empty());
    e.exchange().debug_assert_invariants();
}

#[test]
fn settlement_acks_client_orders_in_submission_order() {
    let (mut e, mut rec) = rig();
    // Interleave prices and sides so book order differs from submission order.
    e.ctx().place_limit_with(A, Side::Bid, tick(300), Qty(1), TradeSource::Strategy, cid("first")).unwrap();
    e.ctx().place_limit(B, Side::Ask, tick(800), Qty(1), TradeSource::Bot).unwrap();
    e.ctx().place_limit_with(A, Side::Ask, tick(700), Qty(1), TradeSource::Strategy, cid("second")).unwrap();
    e.ctx().place_limit_with(B, Side::Bid, tick(400), Qty(1), TradeSource::Strategy, cid("third")).unwrap();
    let report = e.resolve_and_open_next(100_001.0, 100_001.0, 120, 120);
    assert_eq!(report.cancelled_orders, 4);
    let evs = rec.take();
    let ids: Vec<Arc<str>> = evs.iter().map(|e| e.client_order_id.clone()).collect();
    assert_eq!(ids.iter().map(|s| s.as_ref()).collect::<Vec<_>>(), vec!["first", "second", "third"]);
    assert!(evs.iter().all(|e| e.kind == ExecKind::CancelAck));
    // A cancelled client id is forgotten: re-using the engine later emits nothing for it.
    assert!(e.market().book().is_empty());
}
