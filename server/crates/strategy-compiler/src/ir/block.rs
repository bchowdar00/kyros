//! A guarded block: one entry in a leg's body.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ir::intent::DoItem;
use crate::ir::scalars::{ExprStr, Ident};

/// One block: `{ name, if?, do, then? }`. Blocks run in listed order, top to bottom, on
/// every activation of the leg; statement effects are immediately visible to later
/// blocks in the same activation (load-bearing: a counting block's increment is seen by
/// the entry block on the same wake).
///
/// Rust note: `if` and `do` are Rust keywords, so the fields are named `guard` and
/// `do_items` with `#[serde(rename)]` mapping them to the JSON spelling — schemars
/// follows the serde renames, so the generated schema says `if`/`do`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Block {
    /// The block's identity; stamps order provenance into `client_order_id` and
    /// backtest attribution. Unique within the leg.
    pub name: Ident,
    /// The guard expression (JSON field `if`). ABSENT MEANS THE BLOCK ALWAYS RUNS.
    /// Guards are re-checked on every wake regardless of cause; `triggered(NAME)` — the
    /// one cause primitive — is legal only here.
    #[serde(default, rename = "if", skip_serializing_if = "Option::is_none")]
    pub guard: Option<ExprStr>,
    /// Statements and intents (JSON field `do`), executed in listed order. A flat list
    /// of leaf items; blocks never nest. May be empty on a pure-transition block.
    #[serde(rename = "do")]
    pub do_items: Vec<DoItem>,
    /// Transition to a named leg after this block runs; immediate and TERMINAL —
    /// nothing after it in the body executes this activation. Absent means stay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub then: Option<Ident>,
}
