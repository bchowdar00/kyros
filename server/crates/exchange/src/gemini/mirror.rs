//! Gemini's live book, copied into the pretend-money engine.
//!
//! One account per series, the mirror, keeps resting orders equal to
//! Gemini's book level by level, so a pretend order fills only against
//! a price and a size that are really on Gemini. Three rules keep that
//! honest:
//!
//! * When a pretend order takes from a level, the mirror does not put
//!   the size back until Gemini's own size at that level changes: the
//!   same displayed contracts are never taken twice.
//! * A pretend order resting inside Gemini's spread fills when Gemini's
//!   book moves through it, or when Gemini prints a trade at or through
//!   its price, for no more than the print's size. A pretend order at
//!   or behind a Gemini level waits behind Gemini's own queue and does
//!   not fill on a print.
//! * The mirror is not a trader: it is in no count, and the public tape
//!   and the round's volume are Gemini's prints (see the engine's
//!   `set_mirror` and `record_print`).
//!
//! This file holds the pure part: what the mirror remembers
//! ([`MirrorState`]) and what it must do next ([`reconcile`]).

use std::collections::BTreeMap;

use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;

use crate::ticks::PriceTick;
use crate::types::{OrderId, Qty, Side};

/// One price level: Gemini's, or the mirror's target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Level {
    pub tick: PriceTick,
    pub qty: Qty,
}

/// One thing for the mirror to do on the engine's book.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MirrorOp {
    Place { side: Side, tick: PriceTick, qty: Qty },
    Cancel { order: OrderId },
}

/// Gemini's price as a tick of the engine's grid. `None` for a price
/// the grid does not have; every whole cent from 1 to 99 is on it.
pub fn tick_for(price: Decimal) -> Option<PriceTick> {
    let tenths = price * Decimal::ONE_THOUSAND;
    if !tenths.fract().is_zero() {
        return None;
    }
    PriceTick::from_tenths(tenths.to_u16()?)
}

/// One side of Gemini's book as mirror levels, in the order given.
/// Sizes round down to whole contracts; a level under one contract, or
/// at a price off the grid, is left out.
pub fn levels(side: &[(Decimal, Decimal)]) -> Vec<Level> {
    side.iter()
        .filter_map(|&(price, qty)| {
            let qty = qty.floor().to_u32().filter(|q| *q >= 1)?;
            Some(Level { tick: tick_for(price)?, qty: Qty(qty) })
        })
        .collect()
}

/// The mirror's own orders at one price.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Slot {
    /// Gemini's size here when the mirror last matched it.
    seen: Qty,
    /// Taken by pretend orders since then.
    taken: u32,
    /// The mirror's resting orders, oldest first, with what is left.
    orders: Vec<(OrderId, Qty)>,
}

impl Slot {
    fn resting(&self) -> u32 {
        self.orders.iter().map(|(_, q)| q.0).sum()
    }

    /// Nothing rests here and Gemini shows nothing here.
    fn is_spent(&self) -> bool {
        self.orders.is_empty() && self.seen.is_zero()
    }

    /// What should rest here when Gemini shows `gemini`: all of it if
    /// that is a size the mirror has not matched yet, else what pretend
    /// orders have left of it.
    fn target(&self, gemini: Qty) -> u32 {
        if self.seen == gemini { gemini.0.saturating_sub(self.taken) } else { gemini.0 }
    }
}

/// What the mirror has resting and what pretend orders took from it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MirrorState {
    bids: BTreeMap<u16, Slot>,
    asks: BTreeMap<u16, Slot>,
}

impl MirrorState {
    pub fn new() -> Self {
        Self::default()
    }

    fn side(&self, side: Side) -> &BTreeMap<u16, Slot> {
        match side {
            Side::Bid => &self.bids,
            Side::Ask => &self.asks,
        }
    }

    fn side_mut(&mut self, side: Side) -> &mut BTreeMap<u16, Slot> {
        match side {
            Side::Bid => &mut self.bids,
            Side::Ask => &mut self.asks,
        }
    }

    /// What the mirror has resting at a price.
    pub fn resting(&self, side: Side, tick: PriceTick) -> Qty {
        Qty(self.side(side).get(&tick.tenths()).map_or(0, Slot::resting))
    }

    /// Every resting mirror order: id, side, price, what is left.
    pub fn orders(&self) -> Vec<(OrderId, Side, PriceTick, Qty)> {
        let mut all = Vec::new();
        for side in [Side::Bid, Side::Ask] {
            for (tenths, slot) in self.side(side) {
                let tick = PriceTick::from_tenths(*tenths).expect("slots are keyed by ticks");
                all.extend(slot.orders.iter().map(|(id, qty)| (*id, side, tick, *qty)));
            }
        }
        all
    }

    /// Drop the prices where nothing rests and Gemini shows nothing.
    fn prune(&mut self) {
        self.bids.retain(|_, slot| !slot.is_spent());
        self.asks.retain(|_, slot| !slot.is_spent());
    }

    /// An order of the mirror's is resting with `resting` left.
    pub fn placed(&mut self, side: Side, tick: PriceTick, order: OrderId, resting: Qty) {
        if !resting.is_zero() {
            self.side_mut(side).entry(tick.tenths()).or_default().orders.push((order, resting));
        }
    }

    /// The mirror cancelled this order (or the round's end did).
    pub fn cancelled(&mut self, order: OrderId) {
        for slot in self.bids.values_mut().chain(self.asks.values_mut()) {
            slot.orders.retain(|(id, _)| *id != order);
        }
        self.prune();
    }

    /// Pretend orders took `qty` at this price.
    pub fn took(&mut self, side: Side, tick: PriceTick, qty: Qty) {
        let slot = self.side_mut(side).entry(tick.tenths()).or_default();
        slot.taken = slot.taken.saturating_add(qty.0);
    }

    /// The engine says this much of an order is left. A drop since the
    /// last look was taken by a pretend order; nothing left drops the
    /// order.
    pub fn remaining(&mut self, order: OrderId, remaining: Qty) {
        for slot in self.bids.values_mut().chain(self.asks.values_mut()) {
            let Some(at) = slot.orders.iter().position(|(id, _)| *id == order) else {
                continue;
            };
            let before = slot.orders[at].1.0;
            slot.taken = slot.taken.saturating_add(before.saturating_sub(remaining.0));
            if remaining.is_zero() {
                slot.orders.remove(at);
            } else {
                slot.orders[at].1 = remaining;
            }
        }
    }

    /// The mirror is about to match Gemini's size at this price (call
    /// it before the orders that do so). When that size differs from
    /// the last one matched, what was taken before no longer counts; a
    /// size of zero forgets the price once nothing rests there.
    pub fn saw(&mut self, side: Side, tick: PriceTick, gemini: Qty) {
        let slot = self.side_mut(side).entry(tick.tenths()).or_default();
        if slot.seen != gemini {
            slot.seen = gemini;
            slot.taken = 0;
        }
        self.prune();
    }

    /// [`Self::saw`] for a whole book: every level Gemini shows, and a
    /// size of zero at every price the mirror remembers that Gemini no
    /// longer shows.
    pub fn observe(&mut self, gemini_bids: &[Level], gemini_asks: &[Level]) {
        for (side, gemini) in [(Side::Bid, gemini_bids), (Side::Ask, gemini_asks)] {
            let gone: Vec<u16> = self
                .side(side)
                .keys()
                .copied()
                .filter(|tenths| !gemini.iter().any(|l| l.tick.tenths() == *tenths))
                .collect();
            for tenths in gone {
                if let Some(tick) = PriceTick::from_tenths(tenths) {
                    self.saw(side, tick, Qty::ZERO);
                }
            }
            for level in gemini {
                self.saw(side, level.tick, level.qty);
            }
        }
    }

    /// Forget everything: the round ended or the book is gone.
    pub fn clear(&mut self) {
        self.bids.clear();
        self.asks.clear();
    }
}

/// What to do so the mirror's resting orders equal Gemini's book less
/// what pretend orders already took. Pure. Cancels come first, so a
/// new order never meets one of the mirror's own that is on its way
/// out. A level grows by one more order (the older ones keep their
/// place in the queue) and shrinks by cancelling the newest.
pub fn reconcile(gemini_bids: &[Level], gemini_asks: &[Level], mine: &MirrorState) -> Vec<MirrorOp> {
    let mut cancels = Vec::new();
    let mut places = Vec::new();
    for (side, gemini) in [(Side::Bid, gemini_bids), (Side::Ask, gemini_asks)] {
        let slots = mine.side(side);
        let shown = |tenths: u16| gemini.iter().find(|l| l.tick.tenths() == tenths).map(|l| l.qty);
        // What is left resting at each of the mirror's prices once the
        // surplus (newest first) is cancelled.
        let mut left: BTreeMap<u16, u32> = BTreeMap::new();
        for (tenths, slot) in slots {
            let target = shown(*tenths).map_or(0, |qty| slot.target(qty));
            let mut resting = slot.resting();
            for (order, qty) in slot.orders.iter().rev() {
                if resting <= target {
                    break;
                }
                cancels.push(MirrorOp::Cancel { order: *order });
                resting -= qty.0;
            }
            left.insert(*tenths, resting);
        }
        for level in gemini {
            let tenths = level.tick.tenths();
            let target = slots.get(&tenths).map_or(level.qty.0, |slot| slot.target(level.qty));
            let resting = left.get(&tenths).copied().unwrap_or(0);
            if resting < target {
                places.push(MirrorOp::Place { side, tick: level.tick, qty: Qty(target - resting) });
            }
        }
    }
    cancels.extend(places);
    cancels
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn d(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }

    fn tick(tenths: u16) -> PriceTick {
        PriceTick::from_tenths(tenths).unwrap()
    }

    fn level(tenths: u16, qty: u32) -> Level {
        Level { tick: tick(tenths), qty: Qty(qty) }
    }

    fn place(side: Side, tenths: u16, qty: u32) -> MirrorOp {
        MirrorOp::Place { side, tick: tick(tenths), qty: Qty(qty) }
    }

    fn cancel(order: u64) -> MirrorOp {
        MirrorOp::Cancel { order: OrderId(order) }
    }

    /// A mirror that has matched Gemini with one order per level.
    fn matched(bids: &[(u16, u32, u64)], asks: &[(u16, u32, u64)]) -> MirrorState {
        let mut mine = MirrorState::new();
        for (side, levels) in [(Side::Bid, bids), (Side::Ask, asks)] {
            for &(tenths, qty, order) in levels {
                mine.placed(side, tick(tenths), OrderId(order), Qty(qty));
                mine.saw(side, tick(tenths), Qty(qty));
            }
        }
        mine
    }

    #[test]
    fn geminis_whole_cents_are_ticks_of_the_grid() {
        assert_eq!(tick_for(d("0.01")), Some(tick(10)));
        assert_eq!(tick_for(d("0.5500")), Some(tick(550)));
        assert_eq!(tick_for(d("0.99")), Some(tick(990)));
        for cents in 1..=99u32 {
            let price = Decimal::new(i64::from(cents), 2);
            assert_eq!(tick_for(price).map(PriceTick::tenths), Some((cents * 10) as u16), "{cents} cents");
        }
        // Half a cent is a tick only in the fine tails of the grid.
        assert_eq!(tick_for(d("0.005")), Some(tick(5)));
        assert_eq!(tick_for(d("0.555")), None);
        assert_eq!(tick_for(d("0.5501")), None, "finer than a tenth of a cent");
        assert_eq!(tick_for(d("0")), None);
        assert_eq!(tick_for(d("1")), None);
        assert_eq!(tick_for(d("-0.5")), None);
    }

    #[test]
    fn sizes_round_down_and_a_level_under_one_contract_is_not_mirrored() {
        let side = [(d("0.55"), d("10.99")), (d("0.54"), d("0.99")), (d("0.53"), d("3")), (d("0.555"), d("8"))];
        assert_eq!(levels(&side), vec![level(550, 10), level(530, 3)]);
        assert_eq!(levels(&[]), vec![]);
    }

    #[test]
    fn equal_books_need_nothing() {
        let mine = matched(&[(540, 10, 1), (530, 25, 2)], &[(560, 7, 3)]);
        let ops = reconcile(&[level(540, 10), level(530, 25)], &[level(560, 7)], &mine);
        assert_eq!(ops, vec![]);
        assert_eq!(reconcile(&[], &[], &MirrorState::new()), vec![]);
    }

    #[test]
    fn a_level_that_appears_is_placed() {
        let mine = matched(&[(540, 10, 1)], &[]);
        let ops = reconcile(&[level(540, 10), level(530, 4)], &[level(560, 7)], &mine);
        assert_eq!(ops, vec![place(Side::Bid, 530, 4), place(Side::Ask, 560, 7)]);
    }

    #[test]
    fn a_level_that_grows_gets_one_more_order_for_the_difference() {
        let mine = matched(&[(540, 10, 1)], &[]);
        assert_eq!(reconcile(&[level(540, 15)], &[], &mine), vec![place(Side::Bid, 540, 5)]);
    }

    #[test]
    fn a_level_that_shrinks_loses_its_newest_orders_first() {
        // One order of 10: nothing smaller to cancel, so replace it.
        let one = matched(&[], &[(560, 10, 1)]);
        assert_eq!(reconcile(&[], &[level(560, 4)], &one), vec![cancel(1), place(Side::Ask, 560, 4)]);

        // Two orders, 10 then 5 (the level grew earlier).
        let mut two = matched(&[], &[(560, 10, 1)]);
        two.placed(Side::Ask, tick(560), OrderId(2), Qty(5));
        two.saw(Side::Ask, tick(560), Qty(15));
        // Down to 10: the newer one goes, the older keeps its place.
        assert_eq!(reconcile(&[], &[level(560, 10)], &two), vec![cancel(2)]);
        // Down to 12: the newer one goes and 2 come back behind.
        assert_eq!(reconcile(&[], &[level(560, 12)], &two), vec![cancel(2), place(Side::Ask, 560, 2)]);
        // Down to 3: both go.
        assert_eq!(reconcile(&[], &[level(560, 3)], &two), vec![cancel(2), cancel(1), place(Side::Ask, 560, 3)]);
    }

    #[test]
    fn a_level_that_vanishes_is_cancelled() {
        let mut mine = matched(&[(540, 10, 1), (530, 25, 2)], &[(560, 7, 3)]);
        mine.placed(Side::Bid, tick(530), OrderId(4), Qty(5));
        let ops = reconcile(&[level(540, 10)], &[], &mine);
        assert_eq!(ops, vec![cancel(4), cancel(2), cancel(3)]);
    }

    #[test]
    fn cancels_come_before_places() {
        // Gemini's book moved down a cent: the old bid must be gone
        // before the new ask lands on its price.
        let mine = matched(&[(550, 10, 1)], &[(560, 7, 2)]);
        let ops = reconcile(&[level(540, 10)], &[level(550, 7)], &mine);
        assert_eq!(ops, vec![cancel(1), cancel(2), place(Side::Bid, 540, 10), place(Side::Ask, 550, 7)]);
    }

    #[test]
    fn what_a_pretend_order_took_stays_gone_until_geminis_size_there_changes() {
        let mut mine = matched(&[], &[(560, 10, 1)]);
        // A pretend buyer lifts 7 of the 10: the engine reports 3 left.
        mine.remaining(OrderId(1), Qty(3));
        assert_eq!(mine.resting(Side::Ask, tick(560)), Qty(3));
        // Gemini still shows 10. The mirror does not top the level up.
        assert_eq!(reconcile(&[], &[level(560, 10)], &mine), vec![]);
        // Gemini's size changes: the level is Gemini's new size, whole.
        assert_eq!(reconcile(&[], &[level(560, 12)], &mine), vec![place(Side::Ask, 560, 9)]);
        assert_eq!(reconcile(&[], &[level(560, 2)], &mine), vec![cancel(1), place(Side::Ask, 560, 2)]);
        // Once the mirror has matched the new size, taking starts from zero.
        mine.placed(Side::Ask, tick(560), OrderId(2), Qty(9));
        mine.saw(Side::Ask, tick(560), Qty(12));
        assert_eq!(reconcile(&[], &[level(560, 12)], &mine), vec![]);
    }

    #[test]
    fn an_order_taken_whole_is_not_replaced_while_gemini_shows_the_same_size() {
        let mut mine = matched(&[(540, 10, 1)], &[]);
        mine.remaining(OrderId(1), Qty::ZERO);
        assert_eq!(mine.resting(Side::Bid, tick(540)), Qty::ZERO);
        assert!(mine.orders().is_empty());
        assert_eq!(reconcile(&[level(540, 10)], &[], &mine), vec![]);
        assert_eq!(reconcile(&[level(540, 11)], &[], &mine), vec![place(Side::Bid, 540, 11)]);
        // The level vanishes on Gemini and comes back at the old size:
        // that is a new level, mirrored whole.
        mine.saw(Side::Bid, tick(540), Qty::ZERO);
        assert_eq!(reconcile(&[level(540, 10)], &[], &mine), vec![place(Side::Bid, 540, 10)]);
    }

    #[test]
    fn a_take_while_placing_counts_against_the_level() {
        // Gemini's ask came down to 53 with 10; a pretend bid above it
        // took 4 as the mirror's order went in, so 6 rest.
        let mut mine = MirrorState::new();
        mine.saw(Side::Ask, tick(530), Qty(10));
        mine.placed(Side::Ask, tick(530), OrderId(1), Qty(6));
        mine.took(Side::Ask, tick(530), Qty(4));
        assert_eq!(reconcile(&[], &[level(530, 10)], &mine), vec![]);
        assert_eq!(reconcile(&[], &[level(530, 11)], &mine), vec![place(Side::Ask, 530, 5)]);
    }

    #[test]
    fn observing_a_book_resets_what_was_taken_wherever_geminis_size_moved() {
        let mut mine = matched(&[(540, 10, 1)], &[(560, 10, 2), (570, 4, 3)]);
        mine.remaining(OrderId(1), Qty(4)); // 6 taken at 54
        mine.remaining(OrderId(2), Qty::ZERO); // all 10 taken at 56
        mine.remaining(OrderId(3), Qty(1)); // 3 taken at 57
        // Gemini now shows: 54 unchanged, 56 gone, 57 at a new size.
        mine.observe(&[level(540, 10)], &[level(570, 5)]);
        assert_eq!(reconcile(&[level(540, 10)], &[level(570, 5)], &mine), vec![place(Side::Ask, 570, 4)]);
        // 56 comes back at its old size: a new level, mirrored whole.
        let again = reconcile(&[level(540, 10)], &[level(560, 10), level(570, 5)], &mine);
        assert_eq!(again, vec![place(Side::Ask, 560, 10), place(Side::Ask, 570, 4)]);
    }

    #[test]
    fn the_state_lists_and_forgets_its_orders() {
        let mut mine = matched(&[(540, 10, 1)], &[(560, 7, 2)]);
        mine.placed(Side::Ask, tick(560), OrderId(3), Qty(2));
        assert_eq!(
            mine.orders(),
            vec![
                (OrderId(1), Side::Bid, tick(540), Qty(10)),
                (OrderId(2), Side::Ask, tick(560), Qty(7)),
                (OrderId(3), Side::Ask, tick(560), Qty(2)),
            ]
        );
        assert_eq!(mine.resting(Side::Ask, tick(560)), Qty(9));
        mine.cancelled(OrderId(2));
        assert_eq!(mine.resting(Side::Ask, tick(560)), Qty(2));
        mine.cancelled(OrderId(99));
        mine.clear();
        assert!(mine.orders().is_empty());
        assert_eq!(mine, MirrorState::new());
    }
}
