//! Order book for a single market: one canonical YES book.
//!
//! Storage strategy. Because the price grid is small and fixed (153 ticks),
//! we sidestep `BTreeMap` and address each level by its dense tick index:
//!
//! * `levels: [Level; NUM_TICKS]`              direct-indexed price levels
//! * `nodes : Slab<OrderNode>`                 stable-index pool of FIFO nodes
//! * `by_id : HashMap<OrderId, NodeIdx>`       O(1) cancellation lookup
//! * `best_bid_idx`, `best_ask_idx`            cached top-of-book pointers
//!
//! Each level is a doubly-linked list of `OrderNode`s, head-to-tail in
//! arrival order, giving price-time priority for free. Insert and cancel
//! are both O(1) amortized; the only walk is the rare best-pointer update
//! when the top level empties.

use std::collections::HashMap;

use slab::Slab;

use super::model::Order;
use crate::ticks::{NUM_TICKS, PriceTick};
use crate::types::{OrderId, Qty, Side};

/// Tick-grid ceiling in tenths-of-a-cent (99.9¢). Used as the implied
/// mark when the ask side of the book is empty.
pub const MAX_MARK_TENTHS: u16 = 999;

/// Tick-grid floor in tenths-of-a-cent (0.1¢). Used as the implied
/// mark when the bid side of the book is empty.
pub const MIN_MARK_TENTHS: u16 = 1;

type NodeIdx = usize;

#[derive(Debug, Clone, Copy)]
struct Level {
    head: Option<NodeIdx>,
    tail: Option<NodeIdx>,
    total_qty: Qty,
}

impl Level {
    const EMPTY: Self = Self {
        head: None,
        tail: None,
        total_qty: Qty::ZERO,
    };

    fn is_empty(&self) -> bool {
        self.head.is_none()
    }
}

#[derive(Debug)]
struct OrderNode {
    order: Order,
    prev: Option<NodeIdx>,
    next: Option<NodeIdx>,
    level_idx: usize,
}

#[derive(Debug)]
pub struct Book {
    levels: [Level; NUM_TICKS],
    nodes: Slab<OrderNode>,
    by_id: HashMap<OrderId, NodeIdx>,
    best_bid_idx: Option<usize>,
    best_ask_idx: Option<usize>,
}

impl Book {
    pub fn new() -> Self {
        Self {
            levels: [Level::EMPTY; NUM_TICKS],
            nodes: Slab::new(),
            by_id: HashMap::new(),
            best_bid_idx: None,
            best_ask_idx: None,
        }
    }

    /// Number of resting orders across all levels. Cheap; useful for tests.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }
}

impl Default for Book {
    fn default() -> Self {
        Self::new()
    }
}

impl Book {
    /// Append an order to the tail of its price level.
    ///
    /// The order is treated as fully resting — no matching happens here. The
    /// engine is expected to have already crossed against the opposite side
    /// (if any) before handing the residual qty to the book.
    ///
    /// Panics if `order.qty_open` is zero or if `order.id` is already on the
    /// book; both are caller bugs.
    pub fn insert_resting(&mut self, order: Order) {
        assert!(!order.qty_open.is_zero(), "cannot rest a zero-qty order");
        assert!(
            !self.by_id.contains_key(&order.id),
            "duplicate order id: {:?}",
            order.id,
        );

        let level_idx = order.price.to_index();
        let side = order.side;
        let qty = order.qty_open;

        let prev_tail = self.levels[level_idx].tail;
        let node_idx = self.nodes.insert(OrderNode {
            order,
            prev: prev_tail,
            next: None,
            level_idx,
        });

        match prev_tail {
            Some(tail) => self.nodes[tail].next = Some(node_idx),
            None => self.levels[level_idx].head = Some(node_idx),
        }
        self.levels[level_idx].tail = Some(node_idx);
        self.levels[level_idx].total_qty += qty;
        self.by_id.insert(order.id, node_idx);

        self.update_best_on_insert(side, level_idx);
    }

    fn update_best_on_insert(&mut self, side: Side, level_idx: usize) {
        match side {
            // Bid: better = higher tick.
            Side::Bid => match self.best_bid_idx {
                Some(cur) if cur >= level_idx => {}
                _ => self.best_bid_idx = Some(level_idx),
            },
            // Ask: better = lower tick.
            Side::Ask => match self.best_ask_idx {
                Some(cur) if cur <= level_idx => {}
                _ => self.best_ask_idx = Some(level_idx),
            },
        }
    }

    /// Remove the resting order with the given id and return it. Returns
    /// `None` if no such order is on the book.
    ///
    /// O(1) on average: hash lookup → unlink from doubly-linked list →
    /// free slab slot. The only walk is the rare best-pointer refresh when
    /// the cancelled order leaves the top-of-book level empty.
    pub fn cancel(&mut self, id: OrderId) -> Option<Order> {
        let node_idx = self.by_id.remove(&id)?;
        let OrderNode {
            order,
            prev,
            next,
            level_idx,
        } = self.nodes.remove(node_idx);

        match prev {
            Some(p) => self.nodes[p].next = next,
            None => self.levels[level_idx].head = next,
        }
        match next {
            Some(n) => self.nodes[n].prev = prev,
            None => self.levels[level_idx].tail = prev,
        }
        self.levels[level_idx].total_qty -= order.qty_open;

        if self.levels[level_idx].is_empty() {
            match order.side {
                Side::Bid if self.best_bid_idx == Some(level_idx) => {
                    self.best_bid_idx = self.find_next_best_bid(level_idx);
                }
                Side::Ask if self.best_ask_idx == Some(level_idx) => {
                    self.best_ask_idx = self.find_next_best_ask(level_idx);
                }
                _ => {}
            }
        }

        Some(order)
    }

    fn find_next_best_bid(&self, from_exclusive: usize) -> Option<usize> {
        (0..from_exclusive)
            .rev()
            .find(|&i| !self.levels[i].is_empty())
    }

    fn find_next_best_ask(&self, from_exclusive: usize) -> Option<usize> {
        ((from_exclusive + 1)..NUM_TICKS).find(|&i| !self.levels[i].is_empty())
    }

    /// Highest bid currently resting on the book.
    pub fn best_bid(&self) -> Option<PriceTick> {
        self.best_bid_idx.and_then(PriceTick::from_index)
    }

    /// Lowest ask currently resting on the book.
    pub fn best_ask(&self) -> Option<PriceTick> {
        self.best_ask_idx.and_then(PriceTick::from_index)
    }

    /// Strict midpoint of best bid and best ask, in raw tenths.
    /// Returns `None` when either side is empty.
    ///
    /// Used by code paths that need a *transactable* reference price
    /// (e.g. `position_target_notional` for flip sizing): a one-sided
    /// book has no two-sided midpoint, and over-valuing the position
    /// against an implied-extreme would let `flip_position` open more
    /// than the close leg can fund.
    ///
    /// For display / strategy-trigger consumers that should reflect
    /// "one side is empty, so price is at the implied extreme,"
    /// use [`Book::implied_mark_yes_tenths`] instead.
    pub fn mid_yes_tenths(&self) -> Option<u16> {
        match (self.best_bid(), self.best_ask()) {
            (Some(bid), Some(ask)) => {
                let sum = bid.tenths() as u32 + ask.tenths() as u32;
                Some((sum / 2) as u16)
            }
            _ => None,
        }
    }

    /// Best-effort YES mark in raw tenths, with implied-extreme
    /// fallback for one-sided books:
    ///
    /// * Two-sided book → midpoint (same as [`mid_yes_tenths`]).
    /// * **Bid-only book** → [`MAX_MARK_TENTHS`] (99.9¢). No asks at
    ///   any price means every holder refuses to sell, implying a
    ///   near-certain YES outcome.
    /// * **Ask-only book** → [`MIN_MARK_TENTHS`] (0.1¢). Symmetric.
    /// * Empty book → `None`. Callers fall back to last trade / fair
    ///   value for true cold-start.
    ///
    /// Side effect: a one-sided book becomes a strong, possibly jumpy
    /// price signal. Edge-triggered (`PriceCrosses*`) strategies may
    /// see large step crossings when one side empties out — which is
    /// what they exist to detect. The per-block fire cap remains the
    /// runaway-safety net.
    pub fn implied_mark_yes_tenths(&self) -> Option<u16> {
        match (self.best_bid(), self.best_ask()) {
            (Some(bid), Some(ask)) => {
                let sum = bid.tenths() as u32 + ask.tenths() as u32;
                Some((sum / 2) as u16)
            }
            (Some(_), None) => Some(MAX_MARK_TENTHS),
            (None, Some(_)) => Some(MIN_MARK_TENTHS),
            (None, None) => None,
        }
    }

    /// The YES midpoint the probability chart plots, in raw tenths.
    ///
    /// Two-sided book → midpoint. One-sided book → the empty side is
    /// taken at its implied extreme ([`MAX_MARK_TENTHS`] for a missing
    /// ask, [`MIN_MARK_TENTHS`] for a missing bid) and the midpoint is
    /// taken against that, so the line drifts toward the extreme rather
    /// than stepping straight to it the moment a side empties. This is
    /// deliberately softer than [`implied_mark_yes_tenths`]: the header
    /// quotes the extreme because that is the price a taker would pay;
    /// the chart is a history and a hard step would read as a print
    /// that never happened. Empty book → `None`, and the chart holds
    /// its last value.
    pub fn chart_mid_yes_tenths(&self) -> Option<u16> {
        let bid = self.best_bid().map(|p| p.tenths());
        let ask = self.best_ask().map(|p| p.tenths());
        match (bid, ask) {
            (None, None) => None,
            (bid, ask) => {
                let bid = bid.unwrap_or(MIN_MARK_TENTHS) as u32;
                let ask = ask.unwrap_or(MAX_MARK_TENTHS) as u32;
                Some(((bid + ask) / 2) as u16)
            }
        }
    }

    /// Total open quantity resting at a given price.
    pub fn level_qty(&self, price: PriceTick) -> Qty {
        self.levels[price.to_index()].total_qty
    }

    /// Walks bids from best to worst (high → low), yielding non-empty levels.
    pub fn iter_bids(&self) -> impl Iterator<Item = (PriceTick, Qty)> + '_ {
        let end = self.best_bid_idx.map(|i| i + 1).unwrap_or(0);
        (0..end).rev().filter_map(move |i| {
            let lvl = &self.levels[i];
            if lvl.is_empty() {
                None
            } else {
                Some((
                    PriceTick::from_index(i).expect("level idx in range"),
                    lvl.total_qty,
                ))
            }
        })
    }

    /// Returns a copy of the resting order with the given id, if present.
    /// Used by the engine for ownership checks before cancel.
    pub fn get_order(&self, id: OrderId) -> Option<Order> {
        self.by_id.get(&id).map(|&idx| self.nodes[idx].order)
    }

    /// Returns a copy of the FIFO head of the best level on `side`.
    /// Used by the matching engine to inspect the next maker without
    /// borrowing the book mutably.
    pub fn peek_best_head(&self, side: Side) -> Option<Order> {
        let best_idx = match side {
            Side::Bid => self.best_bid_idx,
            Side::Ask => self.best_ask_idx,
        }?;
        let head_idx = self.levels[best_idx].head?;
        Some(self.nodes[head_idx].order)
    }

    /// Reduces the FIFO head of the best level on `side` by `fill_qty`.
    /// If `fill_qty` equals the head's remaining qty, the head is removed
    /// (best pointer refreshed if the level empties) and the consumed
    /// `Order` is returned. Otherwise returns `None` and the head stays
    /// resting with reduced `qty_open`.
    ///
    /// Panics if there is no head on the given side, if `fill_qty` is zero,
    /// or if `fill_qty` exceeds the head's open qty — all caller bugs.
    pub fn reduce_or_remove_head(&mut self, side: Side, fill_qty: Qty) -> Option<Order> {
        assert!(!fill_qty.is_zero(), "zero fill_qty");
        let best_idx = match side {
            Side::Bid => self.best_bid_idx,
            Side::Ask => self.best_ask_idx,
        }
        .expect("reduce_or_remove_head called with no head on side");
        let head_idx = self.levels[best_idx]
            .head
            .expect("best level must have a head");
        let head_qty = self.nodes[head_idx].order.qty_open;
        assert!(
            fill_qty <= head_qty,
            "fill_qty {:?} exceeds head qty {:?}",
            fill_qty,
            head_qty
        );

        if fill_qty == head_qty {
            let order_id = self.nodes[head_idx].order.id;
            Some(self.cancel(order_id).expect("head must be cancellable"))
        } else {
            self.nodes[head_idx].order.qty_open -= fill_qty;
            self.levels[best_idx].total_qty -= fill_qty;
            None
        }
    }

    /// Walks asks from best to worst (low → high), yielding non-empty levels.
    pub fn iter_asks(&self) -> impl Iterator<Item = (PriceTick, Qty)> + '_ {
        let start = self.best_ask_idx.unwrap_or(NUM_TICKS);
        (start..NUM_TICKS).filter_map(move |i| {
            let lvl = &self.levels[i];
            if lvl.is_empty() {
                None
            } else {
                Some((
                    PriceTick::from_index(i).expect("level idx in range"),
                    lvl.total_qty,
                ))
            }
        })
    }

    /// Walks resting bid orders in matching order: best price first, FIFO
    /// inside each level. Used by the engine to simulate market orders
    /// without mutating the book.
    pub fn iter_bid_orders(&self) -> impl Iterator<Item = Order> + '_ {
        let end = self.best_bid_idx.map(|i| i + 1).unwrap_or(0);
        (0..end)
            .rev()
            .flat_map(move |level_idx| self.iter_level_orders(level_idx))
    }

    /// Walks resting ask orders in matching order: best price first, FIFO
    /// inside each level. Used by the engine to simulate market orders
    /// without mutating the book.
    pub fn iter_ask_orders(&self) -> impl Iterator<Item = Order> + '_ {
        let start = self.best_ask_idx.unwrap_or(NUM_TICKS);
        (start..NUM_TICKS).flat_map(move |level_idx| self.iter_level_orders(level_idx))
    }

    fn iter_level_orders(&self, level_idx: usize) -> LevelOrderIter<'_> {
        LevelOrderIter {
            book: self,
            next: self.levels[level_idx].head,
        }
    }
}

struct LevelOrderIter<'a> {
    book: &'a Book,
    next: Option<NodeIdx>,
}

impl Iterator for LevelOrderIter<'_> {
    type Item = Order;

    fn next(&mut self) -> Option<Self::Item> {
        let idx = self.next?;
        let node = &self.book.nodes[idx];
        self.next = node.next;
        Some(node.order)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::model::TradeSource;
    use crate::types::Address;

    fn mk(id: u64, side: Side, tenths: u16, qty: u32) -> Order {
        // Deterministic test address derived from the order id.
        let mut bytes = [0u8; 20];
        bytes[12..].copy_from_slice(&id.to_be_bytes());
        Order {
            id: OrderId(id),
            account: Address(bytes),
            side,
            price: PriceTick::from_tenths(tenths).unwrap(),
            qty_open: Qty(qty),
            ts: id,
            source: TradeSource::Bot,
        }
    }

    #[test]
    fn empty_book_has_no_best() {
        let b = Book::new();
        assert!(b.best_bid().is_none());
        assert!(b.best_ask().is_none());
        assert!(b.is_empty());
        assert_eq!(b.iter_bids().count(), 0);
        assert_eq!(b.iter_asks().count(), 0);
    }

    #[test]
    fn single_bid_sets_best_bid_only() {
        let mut b = Book::new();
        b.insert_resting(mk(1, Side::Bid, 650, 10));
        assert_eq!(b.best_bid(), PriceTick::from_tenths(650));
        assert!(b.best_ask().is_none());
        assert_eq!(b.level_qty(PriceTick::from_tenths(650).unwrap()), Qty(10));
        assert_eq!(b.len(), 1);
    }

    #[test]
    fn single_ask_sets_best_ask_only() {
        let mut b = Book::new();
        b.insert_resting(mk(1, Side::Ask, 660, 5));
        assert_eq!(b.best_ask(), PriceTick::from_tenths(660));
        assert!(b.best_bid().is_none());
    }

    #[test]
    fn better_bid_replaces_best_worse_does_not() {
        let mut b = Book::new();
        b.insert_resting(mk(1, Side::Bid, 600, 1));
        b.insert_resting(mk(2, Side::Bid, 650, 1)); // better (higher)
        assert_eq!(b.best_bid(), PriceTick::from_tenths(650));
        b.insert_resting(mk(3, Side::Bid, 620, 1)); // worse
        assert_eq!(b.best_bid(), PriceTick::from_tenths(650));
    }

    #[test]
    fn better_ask_replaces_best_worse_does_not() {
        let mut b = Book::new();
        b.insert_resting(mk(1, Side::Ask, 700, 1));
        b.insert_resting(mk(2, Side::Ask, 660, 1)); // better (lower)
        assert_eq!(b.best_ask(), PriceTick::from_tenths(660));
        b.insert_resting(mk(3, Side::Ask, 680, 1)); // worse
        assert_eq!(b.best_ask(), PriceTick::from_tenths(660));
    }

    #[test]
    fn same_price_aggregates_qty_in_fifo() {
        let mut b = Book::new();
        b.insert_resting(mk(1, Side::Bid, 650, 3));
        b.insert_resting(mk(2, Side::Bid, 650, 7));
        b.insert_resting(mk(3, Side::Bid, 650, 5));
        let p = PriceTick::from_tenths(650).unwrap();
        assert_eq!(b.level_qty(p), Qty(15));
        // FIFO: cancelling id 1 first leaves id 2 at the head.
        let cancelled = b.cancel(OrderId(1)).unwrap();
        assert_eq!(cancelled.id, OrderId(1));
        assert_eq!(b.level_qty(p), Qty(12));
    }

    #[test]
    fn cancel_middle_keeps_endpoints_linked() {
        let mut b = Book::new();
        b.insert_resting(mk(1, Side::Bid, 650, 1));
        b.insert_resting(mk(2, Side::Bid, 650, 2));
        b.insert_resting(mk(3, Side::Bid, 650, 3));
        b.cancel(OrderId(2)).unwrap();
        let p = PriceTick::from_tenths(650).unwrap();
        assert_eq!(b.level_qty(p), Qty(4));
        // Now cancel the (formerly) head and tail; level should empty cleanly.
        b.cancel(OrderId(1)).unwrap();
        b.cancel(OrderId(3)).unwrap();
        assert_eq!(b.level_qty(p), Qty::ZERO);
        assert!(b.best_bid().is_none());
        assert!(b.is_empty());
    }

    #[test]
    fn cancel_tail_then_head_works() {
        let mut b = Book::new();
        b.insert_resting(mk(1, Side::Ask, 700, 1));
        b.insert_resting(mk(2, Side::Ask, 700, 2));
        b.cancel(OrderId(2)).unwrap(); // tail
        b.cancel(OrderId(1)).unwrap(); // now-sole, also head
        assert!(b.best_ask().is_none());
    }

    #[test]
    fn cancel_at_best_walks_to_next_best() {
        let mut b = Book::new();
        // Bids: 650 (best), 640, 600.
        b.insert_resting(mk(1, Side::Bid, 600, 1));
        b.insert_resting(mk(2, Side::Bid, 640, 1));
        b.insert_resting(mk(3, Side::Bid, 650, 1));
        assert_eq!(b.best_bid(), PriceTick::from_tenths(650));
        b.cancel(OrderId(3)).unwrap();
        assert_eq!(b.best_bid(), PriceTick::from_tenths(640));
        b.cancel(OrderId(2)).unwrap();
        assert_eq!(b.best_bid(), PriceTick::from_tenths(600));
        b.cancel(OrderId(1)).unwrap();
        assert!(b.best_bid().is_none());
    }

    #[test]
    fn cancel_below_best_does_not_disturb_top() {
        let mut b = Book::new();
        b.insert_resting(mk(1, Side::Bid, 650, 1));
        b.insert_resting(mk(2, Side::Bid, 600, 1));
        b.cancel(OrderId(2)).unwrap();
        assert_eq!(b.best_bid(), PriceTick::from_tenths(650));
    }

    #[test]
    fn cancel_missing_returns_none() {
        let mut b = Book::new();
        assert!(b.cancel(OrderId(42)).is_none());
        b.insert_resting(mk(1, Side::Bid, 650, 1));
        assert!(b.cancel(OrderId(42)).is_none());
        // Double-cancel: second call returns None.
        b.cancel(OrderId(1)).unwrap();
        assert!(b.cancel(OrderId(1)).is_none());
    }

    #[test]
    fn slab_reuses_slots_under_churn() {
        let mut b = Book::new();
        for i in 0..1000 {
            b.insert_resting(mk(i, Side::Bid, 500, 1));
            b.cancel(OrderId(i)).unwrap();
        }
        // After 1000 insert+cancel pairs, the slab should hold no live nodes.
        assert!(b.is_empty());
        assert_eq!(b.len(), 0);
    }

    #[test]
    fn iter_bids_descending_iter_asks_ascending() {
        let mut b = Book::new();
        for (id, tenths) in [(1, 600), (2, 650), (3, 620)].iter() {
            b.insert_resting(mk(*id, Side::Bid, *tenths, 1));
        }
        for (id, tenths) in [(11, 700), (12, 680), (13, 720)].iter() {
            b.insert_resting(mk(*id, Side::Ask, *tenths, 1));
        }
        let bids: Vec<u16> = b.iter_bids().map(|(p, _)| p.tenths()).collect();
        assert_eq!(bids, vec![650, 620, 600]);
        let asks: Vec<u16> = b.iter_asks().map(|(p, _)| p.tenths()).collect();
        assert_eq!(asks, vec![680, 700, 720]);
    }

    #[test]
    #[should_panic(expected = "zero-qty")]
    fn rejects_zero_qty() {
        let mut b = Book::new();
        b.insert_resting(mk(1, Side::Bid, 650, 0));
    }

    #[test]
    #[should_panic(expected = "duplicate order id")]
    fn rejects_duplicate_id() {
        let mut b = Book::new();
        b.insert_resting(mk(1, Side::Bid, 650, 1));
        b.insert_resting(mk(1, Side::Bid, 640, 1));
    }

    #[test]
    fn peek_best_head_empty_returns_none() {
        let b = Book::new();
        assert!(b.peek_best_head(Side::Bid).is_none());
        assert!(b.peek_best_head(Side::Ask).is_none());
    }

    #[test]
    fn peek_best_head_returns_fifo_head() {
        let mut b = Book::new();
        b.insert_resting(mk(1, Side::Bid, 650, 3));
        b.insert_resting(mk(2, Side::Bid, 650, 7));
        let head = b.peek_best_head(Side::Bid).unwrap();
        assert_eq!(head.id, OrderId(1));
        assert_eq!(head.qty_open, Qty(3));
        // Peeking is non-mutating: head is still there.
        assert_eq!(b.peek_best_head(Side::Bid).unwrap().id, OrderId(1));
    }

    #[test]
    fn partial_reduce_keeps_head() {
        let mut b = Book::new();
        b.insert_resting(mk(1, Side::Bid, 650, 10));
        let consumed = b.reduce_or_remove_head(Side::Bid, Qty(3));
        assert!(consumed.is_none());
        let head = b.peek_best_head(Side::Bid).unwrap();
        assert_eq!(head.id, OrderId(1));
        assert_eq!(head.qty_open, Qty(7));
        assert_eq!(b.level_qty(PriceTick::from_tenths(650).unwrap()), Qty(7));
    }

    #[test]
    fn full_reduce_removes_head_and_refreshes_best() {
        let mut b = Book::new();
        b.insert_resting(mk(1, Side::Ask, 660, 5)); // best
        b.insert_resting(mk(2, Side::Ask, 670, 5));
        let consumed = b.reduce_or_remove_head(Side::Ask, Qty(5)).unwrap();
        assert_eq!(consumed.id, OrderId(1));
        // Best ask should now be 670.
        assert_eq!(b.best_ask(), PriceTick::from_tenths(670));
        // 660 level should be empty.
        assert_eq!(b.level_qty(PriceTick::from_tenths(660).unwrap()), Qty::ZERO);
    }

    #[test]
    fn full_reduce_advances_within_same_level() {
        let mut b = Book::new();
        b.insert_resting(mk(1, Side::Bid, 650, 4));
        b.insert_resting(mk(2, Side::Bid, 650, 6));
        b.reduce_or_remove_head(Side::Bid, Qty(4)).unwrap();
        // FIFO: now id 2 is the head, level qty drops to 6, best unchanged.
        let head = b.peek_best_head(Side::Bid).unwrap();
        assert_eq!(head.id, OrderId(2));
        assert_eq!(b.best_bid(), PriceTick::from_tenths(650));
        assert_eq!(b.level_qty(PriceTick::from_tenths(650).unwrap()), Qty(6));
    }

    #[test]
    #[should_panic(expected = "exceeds head qty")]
    fn reduce_more_than_head_panics() {
        let mut b = Book::new();
        b.insert_resting(mk(1, Side::Bid, 650, 3));
        b.reduce_or_remove_head(Side::Bid, Qty(4));
    }

    #[test]
    #[should_panic(expected = "zero fill_qty")]
    fn reduce_zero_panics() {
        let mut b = Book::new();
        b.insert_resting(mk(1, Side::Bid, 650, 3));
        b.reduce_or_remove_head(Side::Bid, Qty::ZERO);
    }

    #[test]
    fn mid_requires_both_sides() {
        let mut b = Book::new();
        assert!(b.mid_yes_tenths().is_none());
        b.insert_resting(mk(1, Side::Bid, 600, 1));
        assert!(b.mid_yes_tenths().is_none(), "bid-only has no two-sided mid");
        b.insert_resting(mk(2, Side::Ask, 700, 1));
        assert_eq!(b.mid_yes_tenths(), Some(650));
    }

    #[test]
    fn mid_floors_odd_sum() {
        // Adjacent ticks in the fine-bottom 0.1¢ region produce an odd sum.
        let mut b = Book::new();
        b.insert_resting(mk(1, Side::Bid, 29, 1));
        b.insert_resting(mk(2, Side::Ask, 30, 1));
        // (29 + 30) / 2 = 29 (integer floor).
        assert_eq!(b.mid_yes_tenths(), Some(29));
    }

    // ────────────────────── implied_mark_yes_tenths ─────────────────────────

    #[test]
    fn implied_mark_empty_book_is_none() {
        let b = Book::new();
        assert_eq!(b.implied_mark_yes_tenths(), None);
    }

    #[test]
    fn implied_mark_two_sided_matches_mid() {
        let mut b = Book::new();
        b.insert_resting(mk(1, Side::Bid, 600, 1));
        b.insert_resting(mk(2, Side::Ask, 700, 1));
        assert_eq!(b.implied_mark_yes_tenths(), Some(650));
        assert_eq!(b.implied_mark_yes_tenths(), b.mid_yes_tenths());
    }

    #[test]
    fn implied_mark_bid_only_pins_to_ceiling() {
        // No asks at any price → implied mark is the tick-grid ceiling,
        // NOT the bid price and NOT some 50¢ midpoint.
        let mut b = Book::new();
        b.insert_resting(mk(1, Side::Bid, 600, 1));
        assert_eq!(
            b.implied_mark_yes_tenths(),
            Some(MAX_MARK_TENTHS),
            "bid-only book should mark at the tick-grid ceiling (99.9¢)",
        );
        // Holds for low bids too: the bid level doesn't pull the mark
        // back toward the middle.
        let mut b = Book::new();
        b.insert_resting(mk(1, Side::Bid, 100, 1));
        assert_eq!(b.implied_mark_yes_tenths(), Some(MAX_MARK_TENTHS));
    }

    #[test]
    fn implied_mark_ask_only_pins_to_floor() {
        let mut b = Book::new();
        b.insert_resting(mk(1, Side::Ask, 400, 1));
        assert_eq!(
            b.implied_mark_yes_tenths(),
            Some(MIN_MARK_TENTHS),
            "ask-only book should mark at the tick-grid floor (0.1¢)",
        );
        let mut b = Book::new();
        b.insert_resting(mk(1, Side::Ask, 900, 1));
        assert_eq!(b.implied_mark_yes_tenths(), Some(MIN_MARK_TENTHS));
    }

    // ────────────────────── chart_mid_yes_tenths ────────────────────────────

    #[test]
    fn chart_mid_empty_book_is_none() {
        assert_eq!(Book::new().chart_mid_yes_tenths(), None);
    }

    #[test]
    fn chart_mid_two_sided_matches_mid() {
        let mut b = Book::new();
        b.insert_resting(mk(1, Side::Bid, 600, 1));
        b.insert_resting(mk(2, Side::Ask, 700, 1));
        assert_eq!(b.chart_mid_yes_tenths(), Some(650));
        assert_eq!(b.chart_mid_yes_tenths(), b.mid_yes_tenths());
    }

    #[test]
    fn chart_mid_one_sided_averages_against_the_extreme() {
        // Bids only: the missing ask is taken at the ceiling, so the
        // line sits between the bid and 99.9¢ instead of jumping to it.
        let mut b = Book::new();
        b.insert_resting(mk(1, Side::Bid, 600, 1));
        assert_eq!(b.chart_mid_yes_tenths(), Some((600 + 999) / 2));
        // Asks only: symmetric against the floor.
        let mut b = Book::new();
        b.insert_resting(mk(1, Side::Ask, 400, 1));
        assert_eq!(b.chart_mid_yes_tenths(), Some(200), "(0.1¢ + 40¢) / 2, floored");
        // A bid at the ceiling itself is the one case that reads 99.9¢.
        let mut b = Book::new();
        b.insert_resting(mk(1, Side::Bid, 999, 1));
        assert_eq!(b.chart_mid_yes_tenths(), Some(MAX_MARK_TENTHS));
    }

    #[test]
    fn implied_extremes_are_valid_ticks() {
        // PriceTick::from_tenths must accept both endpoints — `mark_price()`
        // depends on this round-trip.
        assert!(PriceTick::from_tenths(MAX_MARK_TENTHS).is_some());
        assert!(PriceTick::from_tenths(MIN_MARK_TENTHS).is_some());
    }
}
