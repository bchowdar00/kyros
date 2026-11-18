//! The `expressions` section: document-level named expressions and constant tables.
//!
//! Body-side only, by doctrine: trigger conditions must be SME-evaluable with zero
//! strategy state, so `expr.` and `table.` names never appear in a trigger.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ir::named_map::NamedMap;
use crate::ir::scalars::{DecimalStr, ExprStr, SignedDecimalStr};

/// Named expressions and constant tables. Both members are optional (sparse-in
/// doctrine); an empty map is legal and distinct from an absent one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Expressions {
    /// Named expressions, referenced as `expr.NAME`. Pure substitution, not storage;
    /// the reference graph must be a DAG (validator-enforced); each evaluates at most
    /// once per activation, memoized.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub named: Option<NamedMap<ExprStr>>,
    /// Constant tables, consumed only via `lookup(table.NAME, key)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tables: Option<NamedMap<Table>>,
}

/// A constant lookup table. `keys` and `values` must be the same length and `keys`
/// strictly increasing (validator checks); out-of-range lookups clamp to the end values
/// (platform-fixed).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Table {
    /// The key column. Signed domain is legal (ruled 2026-07-20): curves keyed on
    /// signed deltas.
    pub keys: Vec<SignedDecimalStr>,
    /// The value column, one per key. Unsigned: table outputs are prices/sizes.
    pub values: Vec<DecimalStr>,
    /// How lookups between keys resolve.
    pub interpolation: Interpolation,
}

/// Table interpolation modes. `step` is spec'd but unexercised; it arrives with its
/// first fixture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Interpolation {
    /// Linear interpolation between neighboring keys.
    Linear,
}
