//! Family A, expression and reference side: every name written anywhere in the
//! document resolves against its declaring section.
//!
//! Split by reference surface: `document.rs` checks names living in typed JSON (leaf
//! signals, arm entries, transition destinations, intent instruments, cancel tags);
//! `exprs.rs` checks names living inside parsed expression strings. Declarations
//! themselves (duplicates, reserved words, registry ids) are `declarations.rs`.

mod document;
mod exprs;

use crate::ir::Strategy;

use super::diag::Diagnostic;
use super::tables::Tables;

/// Run every resolution rule.
pub fn check(strategy: &Strategy, tables: &Tables, diagnostics: &mut Vec<Diagnostic>) {
    document::check(strategy, tables, diagnostics);
    exprs::check(tables, diagnostics);
}
