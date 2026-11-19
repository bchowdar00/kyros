//! The engine's execution-event stream.
//!
//! Emitted only for orders that carry a `client_order_id` (see
//! [`super::PlaceOpts`]), synchronously from inside the engine call that
//! caused them, so events are totally ordered with respect to the mutations
//! they describe. Bots and the UI never carry a client id and never cost an
//! allocation here.
//!
//! Everything is in the engine's integer units (`Qty`, `PriceTick` on the
//! YES book, `OrderId`, `MarketId`). The automation venue adapter converts
//! to the runtime's decimal, outcome-framed shape; the engine knows nothing
//! about `up`/`down` or `rust_decimal`.
//!
//! One event per fate change:
//!
//! | Path | Event |
//! |---|---|
//! | taker fully filled at placement | `Fill` |
//! | taker partly filled, residual rests | `Partial` with `remaining > 0` |
//! | taker partly filled, residual killed (market / IOC / affordability clamp) | `Partial` with `remaining 0` |
//! | taker nothing filled, residual killed | `Expiry` |
//! | taker nothing filled, residual rests | *no event* (the placement reply carries the id) |
//! | resting order hit | `Partial` (still resting) or `Fill` (consumed) |
//! | `cancel`, `cancel_all_resting`, self-trade prevention | `CancelAck` |
//! | `expire` | `Expiry` |
//! | `cancel_replace`, old order | `Replaced` |
//! | any `OrderError` at placement | `Reject` |

use std::sync::Arc;

use tokio::sync::mpsc;

use super::OrderError;
use crate::ticks::PriceTick;
use crate::types::{Address, MarketId, OrderId, Qty, SeriesId};

/// Why the engine refused an order. A closed set the venue adapter maps
/// onto the runtime's reject vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecReject {
    ZeroQuantity,
    UnknownAccount,
    InsufficientBalance,
    NoLiquidity,
    SeriesMismatch,
    /// `post_only` and the order would have crossed.
    PostOnlyWouldCross,
    /// `reduce_only` and the order would not reduce the position.
    ReduceOnlyNoPosition,
    /// The round had reached its close.
    MarketClosed,
    /// An outside venue refused the order (real orders only).
    VenueRefused,
    /// An outside venue never confirmed the order and it could not be found
    /// there afterwards (real orders only).
    VenueUnreachable,
}

impl From<OrderError> for ExecReject {
    fn from(e: OrderError) -> Self {
        match e {
            OrderError::ZeroQuantity => ExecReject::ZeroQuantity,
            OrderError::UnknownAccount => ExecReject::UnknownAccount,
            OrderError::InsufficientBalance { .. } => ExecReject::InsufficientBalance,
            OrderError::NoLiquidity => ExecReject::NoLiquidity,
            OrderError::SeriesMismatch { .. } => ExecReject::SeriesMismatch,
            OrderError::PostOnlyWouldCross => ExecReject::PostOnlyWouldCross,
            OrderError::ReduceOnlyNoPosition => ExecReject::ReduceOnlyNoPosition,
            OrderError::MarketClosed => ExecReject::MarketClosed,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecKind {
    Fill,
    Partial,
    Reject(ExecReject),
    CancelAck,
    Expiry,
    Replaced,
}

/// One order-fate event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionEvent {
    pub series: SeriesId,
    /// The market the order was placed in; `None` only for rejects raised
    /// before a market was consulted.
    pub market: Option<MarketId>,
    pub account: Address,
    pub client_order_id: Arc<str>,
    /// `None` for rejects (no engine id was allocated).
    pub venue_order_id: Option<OrderId>,
    pub kind: ExecKind,
    /// Quantity this event filled (fills only; zero otherwise).
    pub filled_qty: Qty,
    /// Quantity still live after this event; zero when the order is dead.
    pub remaining_qty: Qty,
    /// Fill price on the YES book (fills only).
    pub price: Option<PriceTick>,
}

/// Where the engine sends events: a synchronous callback invoked inside
/// the engine call, under whatever lock the caller holds. It must not
/// block; the venue adapter's callback converts the event and pushes it
/// onto an unbounded channel, so a settlement's cancel acks land in the
/// runtime's inbox before the scheduler (which still holds the lock) can
/// send the settlement notification after them.
#[derive(Clone)]
pub struct ExecutionSink(Arc<dyn Fn(ExecutionEvent) + Send + Sync>);

impl ExecutionSink {
    pub fn new(f: impl Fn(ExecutionEvent) + Send + Sync + 'static) -> ExecutionSink {
        ExecutionSink(Arc::new(f))
    }

    /// A sink that forwards onto a channel.
    pub fn channel(tx: mpsc::UnboundedSender<ExecutionEvent>) -> ExecutionSink {
        ExecutionSink::new(move |ev| {
            let _ = tx.send(ev);
        })
    }

    pub fn send(&self, event: ExecutionEvent) {
        (self.0)(event)
    }
}

impl std::fmt::Debug for ExecutionSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ExecutionSink")
    }
}

/// A sink plus a receiver, for tests: drain with [`Recorder::take`].
#[cfg(test)]
pub struct Recorder {
    pub tx: ExecutionSink,
    rx: mpsc::UnboundedReceiver<ExecutionEvent>,
}

#[cfg(test)]
impl Default for Recorder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
impl Recorder {
    pub fn new() -> Recorder {
        let (tx, rx) = mpsc::unbounded_channel();
        Recorder { tx: ExecutionSink::channel(tx), rx }
    }

    /// Every event emitted since the last call.
    pub fn take(&mut self) -> Vec<ExecutionEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = self.rx.try_recv() {
            out.push(ev);
        }
        out
    }
}
