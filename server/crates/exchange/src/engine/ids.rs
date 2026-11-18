//! Global, lock-free id allocation shared by every market series.
//!
//! Order ids, market ids and the matching sequence number all come from
//! one allocator so they are globally unique and totally ordered across
//! series: `UserOrderRow.placed_ts` stays comparable when a user's rows
//! from two series are merged, and `Fill.ts` stays a valid per-series
//! high-water mark for the trades batcher. The series is deliberately
//! *not* packed into the id — order ids reach the browser as JS numbers
//! and must stay well below 2^53.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::types::{MarketId, OrderId};

#[derive(Debug)]
pub struct IdAllocator {
    order: AtomicU64,
    market: AtomicU32,
    seq: AtomicU64,
}

impl Default for IdAllocator {
    fn default() -> Self {
        Self::new()
    }
}

impl IdAllocator {
    /// All counters start at 1; 0 is never a valid id.
    pub fn new() -> Self {
        Self {
            order: AtomicU64::new(1),
            market: AtomicU32::new(1),
            seq: AtomicU64::new(1),
        }
    }

    pub fn next_order(&self) -> OrderId {
        OrderId(self.order.fetch_add(1, Ordering::Relaxed))
    }

    pub fn next_market(&self) -> MarketId {
        MarketId(self.market.fetch_add(1, Ordering::Relaxed))
    }

    pub fn next_seq(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_start_at_one_and_are_independent() {
        let ids = IdAllocator::new();
        assert_eq!(ids.next_order(), OrderId(1));
        assert_eq!(ids.next_order(), OrderId(2));
        assert_eq!(ids.next_market(), MarketId(1));
        assert_eq!(ids.next_seq(), 1);
        assert_eq!(ids.next_seq(), 2);
        assert_eq!(ids.next_market(), MarketId(2));
    }
}
