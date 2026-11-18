//! A series whose book mirrors an outside venue: the mirror account is
//! a counterparty, never a trader. The public tape and the round's
//! volume are the venue's prints; pretend fills show only in the
//! history of the person who made them.

use super::{Engine, TradeSource};
use crate::ticks::PriceTick;
use crate::types::{Address, DEFAULT_SERIES, Qty, Side, Tenths};

const START: Tenths = Tenths(1_000_000);
const A: Address = Address([0xaa; 20]);
const B: Address = Address([0xbb; 20]);
const MIRROR: Address = Address([0x6e; 20]);

fn tick(t: u16) -> PriceTick {
    PriceTick::from_tenths(t).unwrap()
}

/// Two users and a mirror resting an ask of 10 at 60¢ and a bid of 10
/// at 50¢. `mirrored` says whether the engine has been told.
fn rig(mirrored: bool) -> Engine {
    let mut e = Engine::new(START);
    for a in [A, B] {
        e.get_or_create_account(a);
        e.register_user(a);
    }
    e.seed_bot_account_in(MIRROR, Tenths(1_000_000_000), "gemini-book".into(), None, None, DEFAULT_SERIES);
    if mirrored {
        e.exchange_mut().engine_mut(DEFAULT_SERIES).set_mirror(MIRROR);
    }
    e.open_first_market(100_000.0, 1_900_000_000, 300);
    e.ctx().place_limit(MIRROR, Side::Ask, tick(600), Qty(10), TradeSource::Bot).unwrap();
    e.ctx().place_limit(MIRROR, Side::Bid, tick(500), Qty(10), TradeSource::Bot).unwrap();
    e
}

fn stats(e: &Engine) -> (i64, u64, u64, Option<u32>, usize) {
    let m = e.e();
    (
        m.market_volume_tenths(),
        m.market_trade_count(),
        m.market_unique_traders(),
        m.market_largest_trade().map(|f| f.qty.0),
        m.recent_trades(100).count(),
    )
}

#[test]
fn without_a_mirror_every_fill_is_on_the_tape_as_before() {
    let mut e = rig(false);
    assert_eq!(e.e().mirror(), None);
    e.ctx().place_market(A, Side::Bid, Qty(3), TradeSource::Ui).unwrap();
    assert_eq!(stats(&e), (3_000, 1, 2, Some(3), 1));
}

#[test]
fn a_pretend_fill_against_the_mirror_is_on_no_tape_and_in_no_count_but_the_traders() {
    let mut e = rig(true);
    assert_eq!(e.e().mirror(), Some(MIRROR));
    let out = e.ctx().place_market(A, Side::Bid, Qty(3), TradeSource::Ui).unwrap();
    assert_eq!(out.fills.len(), 1, "the order itself still reports its fill");
    // No volume, no trade, nothing on the tape; one trader, and it is A.
    assert_eq!(stats(&e), (0, 0, 1, None, 0));
    // A's own history has it, and A holds the position.
    let rows = e.e().user_orders(&A);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].filled_qty, Qty(3));
    assert_eq!(e.e().position(&A).position.0, 3);
    assert!(e.e().recent_trades(1).next().is_none(), "a pretend fill is not a last trade");
}

#[test]
fn a_pretend_fill_between_two_people_is_on_no_tape_either() {
    let mut e = rig(true);
    e.ctx().place_limit(A, Side::Bid, tick(550), Qty(4), TradeSource::Ui).unwrap();
    e.ctx().place_limit(B, Side::Ask, tick(550), Qty(4), TradeSource::Ui).unwrap();
    assert_eq!(stats(&e), (0, 0, 2, None, 0));
}

#[test]
fn a_print_from_the_venue_is_the_tape_and_the_volume() {
    let mut e = rig(true);
    let x = e.exchange_mut().engine_mut(DEFAULT_SERIES);
    x.record_print(Side::Bid, tick(600), Qty(4));
    x.record_print(Side::Ask, tick(500), Qty(9));
    x.record_print(Side::Bid, tick(590), Qty(9));
    // $1 a contract of volume, three trades, the largest is the latest
    // of the two nines, and nobody is counted as a trader.
    assert_eq!(stats(&e), (22_000, 3, 0, Some(9), 3));
    let tape: Vec<_> = e.e().recent_trades(10).map(|f| (f.taker_side, f.price.tenths(), f.qty.0)).collect();
    assert_eq!(tape, vec![(Side::Bid, 590, 9), (Side::Ask, 500, 9), (Side::Bid, 600, 4)], "newest first");
    let stamps: Vec<u64> = e.e().recent_trades(10).map(|f| f.ts).collect();
    assert!(stamps[0] > stamps[1] && stamps[1] > stamps[2], "each print has its own, rising stamp: {stamps:?}");
    assert_eq!(e.e().market_largest_trade().map(|f| f.price.tenths()), Some(590));
    assert!(e.e().recent_trades(10).all(|f| f.taker == MIRROR && f.maker == MIRROR));
    // No money moved and no position exists for it.
    assert_eq!(e.e().position(&MIRROR).position.0, 0);
    assert_eq!(e.e().open_interest(), 0);
}

#[test]
fn a_print_on_a_series_with_no_mirror_is_ignored() {
    let mut e = rig(false);
    e.exchange_mut().engine_mut(DEFAULT_SERIES).record_print(Side::Bid, tick(600), Qty(4));
    assert_eq!(stats(&e), (0, 0, 0, None, 0));
}

#[test]
fn the_rounds_numbers_start_again_at_settlement() {
    let mut e = rig(true);
    e.exchange_mut().engine_mut(DEFAULT_SERIES).record_print(Side::Bid, tick(600), Qty(4));
    e.ctx().place_market(A, Side::Bid, Qty(3), TradeSource::Ui).unwrap();
    assert_eq!(stats(&e).2, 1);
    e.resolve_and_open_next(100_001.0, 100_001.0, 1_900_000_300, 300);
    let (volume, trades, traders, largest, _) = stats(&e);
    assert_eq!((volume, trades, traders, largest), (0, 0, 0, None));
    assert_eq!(e.e().mirror(), Some(MIRROR), "the series is still mirrored");
}
