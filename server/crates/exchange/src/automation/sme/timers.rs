//! Timer triggers are connection-owned, not condition rows.
//!
//! The platform decides *when* a timer should fire (a delay from now, and for
//! `interval` a period) and hands that to a [`TimerSink`]. The production sink
//! spawns a Tokio sleep that sends `TimerFired` back into the platform task;
//! tests use a recording sink and fire timers by hand. A timer is identified
//! by its arming generation, so a fire from a cancelled arming is ignored, and
//! every request carries a `cancelled` flag the sink's task checks so it can
//! stop early instead of ticking forever after a re-arm.
//!
//! `window_clock` resolves against the [`VenueClock`]: the remaining time of
//! the instrument's current window. No open window means the timer stays
//! logically armed and is measured when `WindowOpened` arrives.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use strategy_compiler::program::TriggerSource;

use super::ConnId;

/// The venue's view of window time, per instrument.
pub trait VenueClock: Send + Sync {
    /// Milliseconds until the instrument's current window closes, or `None`
    /// when no window is open for it.
    fn window_remaining_ms(&self, instrument_id: &str) -> Option<u64>;
    /// The venue's wall clock, unix milliseconds. Used to stamp timer
    /// activations.
    fn now_ms(&self) -> u64;
}

/// A clock with no windows and a fixed time; the default for tests that do
/// not exercise window timers.
#[derive(Debug, Default)]
pub struct NoWindows;

impl VenueClock for NoWindows {
    fn window_remaining_ms(&self, _instrument_id: &str) -> Option<u64> {
        None
    }
    fn now_ms(&self) -> u64 {
        0
    }
}

/// What the platform asks the sink to schedule.
#[derive(Debug, Clone)]
pub struct TimerRequest {
    pub conn: ConnId,
    pub name: String,
    /// The arming generation the fire must quote.
    pub generation: u64,
    /// Delay to the first fire.
    pub delay_ms: u64,
    /// Period for repeating timers (`interval`); `None` fires once.
    pub repeat_ms: Option<u64>,
    /// Set by the platform when the arming is cancelled; the sink's task
    /// should stop.
    pub cancelled: Arc<AtomicBool>,
}

/// Where timer requests go.
pub trait TimerSink: Send {
    fn schedule(&mut self, request: TimerRequest);
}

/// Records requests instead of scheduling them; tests fire them by hand.
#[derive(Debug, Default)]
pub struct RecordingSink {
    pub requests: Vec<TimerRequest>,
}

impl TimerSink for RecordingSink {
    fn schedule(&mut self, request: TimerRequest) {
        self.requests.push(request);
    }
}

/// How a timer source resolves right now: `Some((delay, repeat))`, or `None`
/// when a `window_clock` has no open window to measure against.
pub fn resolve(source: &TriggerSource, clock: &dyn VenueClock) -> Option<(u64, Option<u64>)> {
    match source {
        TriggerSource::WindowClock { instrument_id, remaining_ms } => {
            let remaining = clock.window_remaining_ms(instrument_id)?;
            Some((remaining.saturating_sub(*remaining_ms), None))
        }
        TriggerSource::After { elapsed_ms } => Some((*elapsed_ms, None)),
        TriggerSource::Interval { every_ms } => Some((*every_ms, Some(*every_ms))),
        TriggerSource::All { .. } | TriggerSource::Any { .. } => None,
    }
}

/// Whether a source is a timer at all.
pub fn is_timer(source: &TriggerSource) -> bool {
    !matches!(source, TriggerSource::All { .. } | TriggerSource::Any { .. })
}

/// The instrument a `window_clock` timer measures, if any.
pub fn window_instrument(source: &TriggerSource) -> Option<&str> {
    match source {
        TriggerSource::WindowClock { instrument_id, .. } => Some(instrument_id),
        _ => None,
    }
}
