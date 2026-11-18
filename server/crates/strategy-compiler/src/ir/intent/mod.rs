//! Do-list items: statement strings and order intents.

pub mod cancel;
pub mod on_table;
pub mod place;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ir::scalars::StmtStr;

pub use cancel::{Cancel, CancelReplace};
pub use on_table::{OnDest, OnTable};
pub use place::{OrderType, Place, PriceBand, Side, TargetPosition, Tif};

/// One entry in a block's `do` list: either a statement string or an externally-tagged
/// intent object.
///
/// Rust note: `#[serde(untagged)]` tries each variant in order against the JSON value —
/// a string becomes `Statement`, an object must match exactly one of the strict wrapper
/// structs. The wrappers exist (instead of inline struct variants) because
/// `deny_unknown_fields` works on real structs but not on untagged variants, so an
/// object carrying two intent keys fails every wrapper instead of silently matching one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum DoItem {
    /// A statement string (`set state.x = ...`); parsed by the compiler's statement
    /// parser, opaque here.
    Statement(StmtStr),
    /// A `{"place": {...}}` intent.
    Place(PlaceItem),
    /// A `{"cancel": {...}}` intent.
    Cancel(CancelItem),
    /// A `{"cancel_replace": {...}}` intent.
    CancelReplace(CancelReplaceItem),
    /// A `{"target_position": {...}}` intent.
    TargetPosition(TargetPositionItem),
}

/// The external tag wrapper for a target-position intent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TargetPositionItem {
    /// The position to reach.
    pub target_position: TargetPosition,
}

/// The external tag wrapper for a place intent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlaceItem {
    /// The order to place.
    pub place: Place,
}

/// The external tag wrapper for a cancel intent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CancelItem {
    /// The cancellation to perform.
    pub cancel: Cancel,
}

/// The external tag wrapper for a cancel-replace intent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CancelReplaceItem {
    /// The replacement to perform.
    pub cancel_replace: CancelReplace,
}
