//! TimerSpecs: the timer family of trigger sources.
//!
//! Timers are armed at leg entry and disarmed at leg exit — `after` and `interval`
//! measure from the moment their leg became active, which is deterministic because leg
//! entry is itself a sequenced event; `window_clock` measures against the instrument's
//! window close. Timers carry no payload: a block gated on a timer trigger has no
//! `event.*`. Time-based conditions are ALWAYS timers, never threshold comparisons on a
//! "time signal".

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ir::scalars::{DurationStr, Ident};

/// A timer specification, tagged on `kind`.
///
/// Rust note: internally-tagged enums cannot carry `deny_unknown_fields` (a serde
/// limitation); strictness comes from the round-trip echo test.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TimerSpec {
    /// Fires when the named instrument's window clock reaches the `remaining` threshold.
    WindowClock {
        /// Local instrument nickname (declared in `instruments`) whose window clock is
        /// watched.
        instrument: Ident,
        /// Threshold before window close, as a duration string (`"10s"`). The unit lives
        /// in the value, never the field name.
        remaining: DurationStr,
    },
    /// Fires once, `elapsed` after the leg that arms it is entered. The order-timeout
    /// primitive: "cancel if unfilled after 30s" is an `after` timer on the resting leg.
    After {
        /// Time from leg entry to the firing, as a duration string.
        elapsed: DurationStr,
    },
    /// Fires every `every` while the arming leg is active, first firing one period after
    /// entry. Pairs naturally with `rearm: immediate`.
    Interval {
        /// The period, as a duration string. At least 250 ms (validator rule).
        every: DurationStr,
    },
}
