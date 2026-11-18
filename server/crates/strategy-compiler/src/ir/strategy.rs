//! The document root: a complete IR strategy document.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ir::expressions::Expressions;
use crate::ir::leg::Leg;
use crate::ir::meta::Meta;
use crate::ir::named_map::NamedMap;
use crate::ir::param::Param;
use crate::ir::state::StateSlot;
use crate::ir::trigger::Trigger;

/// A complete IR strategy document — what the LLM authors and the compiler consumes.
///
/// Section order is canonical (field order here IS the emission order): meta,
/// instruments, signals, state, params, expressions, triggers, legs. Sparse-in
/// doctrine: `expressions` is `Option` and `params` is skipped when empty, so a
/// document without them emits (and hashes) exactly as it did before they existed;
/// `state` is required but may be empty.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Strategy {
    /// Display name, authored description, DSL version.
    pub meta: Meta,
    /// Local nickname -> opaque venue instrument id. Authors never write raw ids
    /// anywhere else; intents and timers reference the nickname.
    pub instruments: NamedMap<String>,
    /// Local nickname -> signal id. Identity only — no kinds, no conditions; filtering
    /// lives in triggers. Own-order-event signals share this surface with a special
    /// resolution rule (per-deployment, never content-hashed).
    pub signals: NamedMap<String>,
    /// Declared state slots. Required, may be empty.
    pub state: Vec<StateSlot>,
    /// Deploy-time knobs, readable as `param.<name>`. Optional: omitted when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub params: Vec<Param>,
    /// Named expressions and constant tables. Optional: emitted only when used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expressions: Option<Expressions>,
    /// The authored wake conditions. Nothing wakes the strategy except a fired trigger.
    pub triggers: Vec<Trigger>,
    /// The legs, in declaration order. The first leg is the initial mode.
    pub legs: Vec<Leg>,
}
