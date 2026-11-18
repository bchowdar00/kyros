//! The reserved-identifier set: names a document may not declare.
//!
//! Ratified composition (ir-spec §3): the expression grammar's keywords, its nine
//! namespace heads, its builtin names, and the leg name `stay` (reserved because on
//! tables use it as a keyword). The validator checks every declared identifier —
//! instrument and signal nicknames, state slots, trigger/leg/block names, expression
//! and table names, tags — against this set.
//!
//! Nothing here duplicates a list that lives elsewhere: keywords come from the lexer's
//! own keyword table, namespace heads from [`Namespace::from_head`], builtin names from
//! the closed [`builtins`] inventory. Adding a word to any of those sources reserves it
//! automatically.

use crate::parse::ast::expr::Namespace;
use crate::parse::builtins;
use crate::parse::lexer::keyword_kind;

/// Whether `name` is reserved and therefore illegal as a declared identifier.
pub fn is_reserved(name: &str) -> bool {
    keyword_kind(name).is_some()
        || Namespace::from_head(name).is_some()
        || builtins::find(name).is_some()
        || name == "stay"
}

/// Which category reserves `name`, for diagnostic messages ("`min` is a builtin name").
/// `None` when the name is not reserved.
pub fn reserved_category(name: &str) -> Option<&'static str> {
    if keyword_kind(name).is_some() {
        Some("an expression keyword")
    } else if Namespace::from_head(name).is_some() {
        Some("a namespace head")
    } else if builtins::find(name).is_some() {
        Some("a builtin name")
    } else if name == "stay" {
        Some("the reserved leg destination")
    } else {
        None
    }
}
