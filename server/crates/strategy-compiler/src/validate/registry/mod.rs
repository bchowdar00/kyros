//! Family B: registry cross-checks — every typed reference to venue truth agrees with
//! the registry's declarations.
//!
//! Split by reference surface: `leaves.rs` (trigger condition leaves), `reads.rs`
//! (`signal.*` body reads), `outcomes.rs` (intent outcomes vs market structure).
//! `event.*` field checks live with the pin rule in the context family — the pinned
//! signal must be known before the field can be looked up.

mod capabilities;
mod leaves;
mod outcomes;
mod reads;

use crate::ir::{FieldDef, Registry, Strategy};

use super::diag::Diagnostic;
use super::poison::Poisons;
use super::tables::{SignalEntry, Tables};

/// Run every registry cross-check.
pub fn check(
    strategy: &Strategy,
    registry: &Registry,
    tables: &Tables,
    poisons: &Poisons,
    diagnostics: &mut Vec<Diagnostic>,
) {
    leaves::check(strategy, registry, tables, poisons, diagnostics);
    reads::check(registry, tables, poisons, diagnostics);
    outcomes::check(strategy, registry, tables, poisons, diagnostics);
    capabilities::check(strategy, registry, tables, poisons, diagnostics);
}

/// Look up a signal nickname's registry fields. `None` when the nickname is
/// undeclared or unresolved — the caller suppresses (family A or `unknown-signal-id`
/// already reported the root cause).
pub(super) fn signal_fields<'r>(
    nick: &str,
    registry: &'r Registry,
    tables: &Tables,
) -> Option<&'r [FieldDef]> {
    match tables.signals.get(nick)? {
        SignalEntry::Resolved(index) => Some(&registry.signals[*index].fields),
        SignalEntry::Unresolved => None,
    }
}

/// Look up a declared param's type spec. `None` when undeclared (family A reported).
pub(super) fn param_spec<'s>(
    name: &str,
    strategy: &'s Strategy,
    tables: &Tables,
) -> Option<&'s crate::ir::TypeSpec> {
    tables
        .params
        .get(name)
        .map(|&index| &strategy.params[index].type_spec)
}

/// Whether a registry field type is numeric (ordering comparators and arithmetic are
/// legal on it).
pub(super) fn is_numeric(field_type: &crate::ir::FieldType) -> bool {
    field_type.is_numeric()
}
