//! The envelope: the submission-supplied bankroll document.
//!
//! NOT an IR section — never LLM-authored. The bankroll service generates it from the
//! user's USDC deposit; the compiler's signature is `compile(strategy, registry,
//! envelope)`, and both artifacts carry it verbatim. Not part of the LLM-facing
//! strategy schema; it derives `JsonSchema` only so the program schema is complete.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ir::scalars::DecimalStr;

/// The deployment's bankroll document. Schema ruled 2026-07-18 (ir-schema-notes.md):
/// three required survival-core fields, four optional gate parameters. All values are
/// decimal strings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Envelope {
    /// Escrowed sub-account capital (USDC); the structural floor.
    pub allocation: DecimalStr,
    /// Hard stop via worst-case accounting (USDC): realized loss + open cost basis +
    /// resting order notional may never exceed this. Binaries-only property.
    pub loss_budget: DecimalStr,
    /// Per-window budget (USDC): what the strategy is handed each market window and
    /// the most the risk gate lets it commit in one — this window's realized loss +
    /// open cost basis + opening notional of resting orders + the next order's
    /// opening cost. Closing orders are free. Resets with the window. Absent: no
    /// per-window cap. At most `allocation`. Readable from expressions, so a size
    /// written as a share of it scales with the money the user puts in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub round_budget: Option<DecimalStr>,
    /// Per-intent size cap (tokens); the primary magnitude strategies read.
    pub max_order_size: DecimalStr,
    /// In-flight worst-case exposure fraction of remaining budget. Absent: 1 (no
    /// proportional shrinkage). Gate parameter — not readable from expressions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_exposure_fraction: Option<DecimalStr>,
    /// Fraction of loss budget consumed at which the gate pauses and notifies. Absent:
    /// no checkpoint. Gate parameter — not readable from expressions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pause_at_consumed: Option<DecimalStr>,
    /// Per-instrument inventory/concentration cap (tokens).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_position_per_market: Option<DecimalStr>,
    /// Maker book-footprint cap (count). Absent: venue default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_open_orders: Option<DecimalStr>,
}
