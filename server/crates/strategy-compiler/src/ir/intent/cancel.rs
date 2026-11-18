//! Cancel and cancel-replace intents.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ir::scalars::{DurationStr, ExprStr, Ident};

use super::place::{OrderType, PriceBand, Side, Tif};

/// A cancel-by-tag intent: cancels ALL live orders carrying the tag. No-match is a
/// logged no-op, never an error. The harness resolves the tag into wire
/// `target_client_order_id`s; tags never cross the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Cancel {
    /// The tag whose live orders are cancelled.
    pub tag: Ident,
}

/// A cancel-replace intent: cancel all live orders carrying `tag`, then place the given
/// order (which carries the same tag). The resolution rule matches cancel-by-tag;
/// replacing a tag with no live orders just places.
///
/// Corpus-shaped: exercised only as a limit-order replace, so `price` and `price_band`
/// are required; a market-order replace is a validator question for a future fixture
/// (`order_type` stays the full enum, validator enforces `limit`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CancelReplace {
    /// The tag whose live orders are replaced; the new order carries it too.
    pub tag: Ident,
    /// Local instrument nickname, declared in `instruments`.
    pub instrument: Ident,
    /// Which outcome to trade, using the market's outcome labels verbatim.
    pub outcome: Ident,
    /// Buy or sell.
    pub side: Side,
    /// Order type; the validator requires `limit` today.
    pub order_type: OrderType,
    /// Time in force.
    pub tif: Tif,
    /// Price expression; must be clamp-wrapped on the post-substitution AST.
    pub price: ExprStr,
    /// `[lo, hi]` price band; compile-time guarantee only.
    pub price_band: PriceBand,
    /// Size expression; clamp-wrapped, envelope-sourced magnitudes.
    pub size: ExprStr,
    /// Per-intent size bound.
    pub max_size: ExprStr,
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
}
