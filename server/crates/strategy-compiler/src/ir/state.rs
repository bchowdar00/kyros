//! The `state` section: the strategy's private, fixed-size memory slots.
//!
//! State is PRIVATE: only the strategy's own body writes it, and only inside
//! activations. That fact is load-bearing for the trigger model — state can never change
//! while the strategy sleeps, so every state change coincides with an armed trigger
//! firing (the fire-mode idiom depends on this).
//!
//! Window slots (v0.3) are bounded ring buffers of numeric values: `push` appends and
//! evicts the oldest at capacity; the reducers (`count`, `sum`, `max_of`, `min_of`,
//! `oldest`, `newest`, `count_where`) read them. A window is never a value itself.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ir::scalars::Ident;
use crate::ir::type_spec::TypeSpec;

/// A declared state slot, tagged on `kind`.
///
/// Rust note: internally-tagged enums don't support `deny_unknown_fields` (a serde
/// limitation); strictness comes from the round-trip echo test.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StateSlot {
    /// A bounded map: keys of declared types, values of a declared type, fixed capacity.
    /// Design instinct, ratified: prefer richer keys over resets — a counter keyed
    /// `[team, quarter]` makes reset logic unrepresentable and unnecessary.
    Map {
        /// Local name the body uses (`state.<name>[...]`).
        name: Ident,
        /// Key types, in index order; index expressions must match this arity and typing.
        key_types: Vec<TypeSpec>,
        /// The value type of every entry.
        value_type: TypeSpec,
        /// Capacity: the maximum number of live keys. Everything is bounded at compile
        /// time; this is the map's bound. The document's only bare JSON number.
        max_keys: u32,
        /// What happens when an insert would exceed `max_keys`.
        overflow: OverflowPolicy,
    },
    /// A single value. Documented defaults when uninitialized: zero for numerics,
    /// `false` for booleans, the empty string for strings, the first declared variant
    /// for enums.
    Scalar {
        /// Local name the body uses (`state.<name>`).
        name: Ident,
        /// The slot's value type.
        value_type: TypeSpec,
    },
    /// A bounded ring buffer of the last `capacity` pushed values, oldest evicted first.
    /// Starts empty; reducers over an empty window return zero.
    Window {
        /// Local name the body uses (`push state.<name> = ...`, `count(state.<name>)`).
        name: Ident,
        /// The element type: `u8` or `decimal` (numeric only).
        value_type: TypeSpec,
        /// The capacity, 1 to 4096.
        capacity: u32,
    },
}

impl StateSlot {
    /// The slot's local name.
    pub fn name(&self) -> &Ident {
        match self {
            StateSlot::Map { name, .. }
            | StateSlot::Scalar { name, .. }
            | StateSlot::Window { name, .. } => name,
        }
    }
}

/// What a full map does with an insert of a new key. `evict` is spec'd but unexercised;
/// it arrives with its first fixture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OverflowPolicy {
    /// Drop the update; existing entries are untouched.
    RefuseUpdate,
}
