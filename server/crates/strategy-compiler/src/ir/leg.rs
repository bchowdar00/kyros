//! A leg: a mode the strategy is in.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ir::block::Block;
use crate::ir::scalars::Ident;

/// A leg: an explicit wake-set plus an ordered, flat list of guarded blocks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Leg {
    /// The leg's name, referenced by `then` transitions, on-table destinations, and
    /// `legs.*` reads. Unique among legs; `stay` is reserved (on tables use it as a
    /// keyword).
    pub name: Ident,
    /// `true`: the leg may legitimately idle forever. `false`: the validator requires a
    /// reachable exit path (a `then` or an intent on-route out).
    pub resident: bool,
    /// Trigger names live while this leg is active — the explicit per-leg wake-set,
    /// mapped one-to-one onto the harness's triggersets and swapped on transition. An
    /// empty list is a terminal leg that wakes for nothing. Arm lists control DELIVERY
    /// only: condition-trigger bookkeeping (prior truth, rearm state) lives at the SME
    /// and is never rewound by leg transitions; timers, by contrast, restart at leg
    /// entry.
    pub arm: Vec<Ident>,
    /// The guarded blocks, executed top to bottom on every activation. May be empty (a
    /// terminal leg like `done`).
    pub body: Vec<Block>,
}
