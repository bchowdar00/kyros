//! Family C: context restrictions — constructs the parser accepts anywhere but the
//! language only allows somewhere.
//!
//! Split by concern: `pin.rs` (the per-block pin decision), `guard.rs` (`triggered()`
//! placement, `event.*` pinning), `refs.rs` (table/window/entry positions),
//! `namespaces.rs` (envelope/platform/legs closed sets), `stmt.rs` (statement lvalue
//! shape). The typing-lite lattice (`super::typing`) is family G but rides in the same
//! pass — several lvalue rules consume it.

mod guard;
pub mod namespaces;
pub mod pin;
mod refs;
mod stmt;
mod trigger_expr;

use crate::ir::{Registry, Strategy};

use super::diag::Diagnostic;
use super::poison::Poisons;
use super::tables::Tables;

pub use namespaces::EnvelopeField;
pub(crate) use refs::is_reducer;
pub use pin::{PinStatus, Pins, UnpinReason};

/// Run every context rule. Returns the computed pins — the lint family reads them.
pub fn check(
    strategy: &Strategy,
    registry: &Registry,
    tables: &Tables,
    poisons: &Poisons,
    diagnostics: &mut Vec<Diagnostic>,
) -> Pins {
    let pins = Pins::compute(strategy, tables, poisons);
    guard::check(strategy, registry, tables, &pins, diagnostics);
    refs::check(strategy, tables, diagnostics);
    trigger_expr::check(tables, diagnostics);
    namespaces::check(tables, diagnostics);
    stmt::check(strategy, registry, tables, &pins, diagnostics);
    pins
}
