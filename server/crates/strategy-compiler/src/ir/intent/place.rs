//! The place intent and its order-shape vocabulary.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ir::scalars::{DecimalStr, DurationStr, ExprStr, Ident};

use super::on_table::OnTable;

/// An order intent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Place {
    /// Local instrument nickname, declared in `instruments`.
    pub instrument: Ident,
    /// Which outcome to trade, using the market's outcome labels verbatim. Required iff
    /// the instrument's market is outcome-structured (validator-enforced against the
    /// registry); linear markets omit it. Registry outcomes are venue data; this is an
    /// authored identifier — the validator bridges them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<Ident>,
    /// Buy or sell.
    pub side: Side,
    /// Market or limit.
    pub order_type: OrderType,
    /// Time in force.
    pub tif: Tif,
    /// Price expression. Present iff `order_type` is `limit` (validator-enforced); must
    /// be clamp-wrapped on the post-substitution AST.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price: Option<ExprStr>,
    /// `[lo, hi]` price band, limit orders only. A compile-time-only guarantee: the
    /// validator checks the price expression's clamp bounds fall inside it; it never
    /// crosses the wire.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_band: Option<PriceBand>,
    /// Size expression; must be clamp-wrapped post-substitution, with magnitudes sourced
    /// from `envelope.*`.
    pub size: ExprStr,
    /// Per-intent size bound (envelope-symbolic expression).
    pub max_size: ExprStr,
    /// Per-strategy order tag; unique among simultaneously-live orders.
    pub tag: Ident,
    /// Rest or reject, never take (limit orders only; the instrument must declare
    /// `post_only`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub post_only: Option<bool>,
    /// Good-till-time: the resting order expires this long after placement, reported
    /// as `expiry` (resting orders only; the instrument must declare `gtd`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expire_after: Option<DurationStr>,
    /// The order may only shrink the position; the runtime clamps its size to the
    /// live position at placement (the instrument must declare `reduce_only`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reduce_only: Option<bool>,
    /// Order-outcome transitions, executed mechanically by the runner (never an
    /// activation). Absent keys default to `stay`; the compiler totalizes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on: Option<OnTable>,
}

/// A target-position intent: the runtime computes the delta between `size` (the
/// desired signed position on the outcome, in tokens) and the live position, and
/// takes it with one market IOC order — or places nothing when already there. Carries
/// an intent id and an on table like `place`; the magnitude and clamp rules apply to
/// `size` exactly as for `place`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TargetPosition {
    /// Local instrument nickname, declared in `instruments`.
    pub instrument: Ident,
    /// Which outcome the position is on; required iff the market is outcome-structured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<Ident>,
    /// The desired signed position, clamp-wrapped and envelope-sourced.
    pub size: ExprStr,
    /// Per-intent bound on the delta order's size.
    pub max_size: ExprStr,
    /// The logical order tag the delta order carries.
    pub tag: Ident,
    /// The delta order may only shrink the position (the instrument must declare
    /// `reduce_only`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reduce_only: Option<bool>,
    /// Order-outcome transitions for the delta order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on: Option<OnTable>,
}

/// Order side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    /// Buy the outcome / go long.
    Buy,
    /// Sell the outcome / reduce or go short.
    Sell,
}

/// Order type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OrderType {
    /// Take whatever the book gives; carries no price and no price band. A market order
    /// needing a slippage cap is written as a limit at a clamped price with `tif: ioc`
    /// (the limit-ify pattern).
    Market,
    /// Priced by the `price` expression; carries a `price_band`.
    Limit,
}

/// Time in force.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Tif {
    /// Immediate-or-cancel: cross now, unfilled remainder dies.
    Ioc,
    /// Good-till-cancelled: rests on the book.
    Gtc,
}

/// `[lo, hi]` — exactly two canonical unsigned decimal strings (prices are non-negative
/// by construction; unsigned here is a correctness constraint).
///
/// Rust note: a two-field tuple struct serializes as a two-element JSON array, so the
/// arity constraint is enforced by the type itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PriceBand(pub DecimalStr, pub DecimalStr);

impl PriceBand {
    /// The lower bound.
    pub fn lo(&self) -> &DecimalStr {
        &self.0
    }

    /// The upper bound.
    pub fn hi(&self) -> &DecimalStr {
        &self.1
    }
}
