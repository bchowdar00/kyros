//! The validator: every rule between a parsed document and a lowerable one.
//!
//! `validate(strategy, registry, envelope)` runs a fixed pass pipeline and returns
//! every diagnostic it can find (never bailing on the first) plus, iff zero errors, the
//! [`ValidStrategy`] typestate that lowering takes — the type's existence is the proof
//! of validation; nothing else can construct one.
//!
//! The passes, in order:
//!
//! - **P0 walk** — parse every embedded expression string ([`crate::parse::walk`]);
//!   adopt its diagnostics; seed the poison set (cascade suppression: a rule whose
//!   input failed upstream emits nothing — the root cause was already reported once).
//! - **P1 declarations** — build the resolution tables from the raw document and the
//!   registry; duplicate/reserved/unresolved-id diagnostics.
//! - **P2 substitution** — the named-expression DAG: cycles, poison propagation, the
//!   memoized substitution environment.
//! - **P3 per-string rules** — resolution, registry cross-checks, context
//!   restrictions, and typing-lite over each parsed string, dispatched on its role.
//! - **P4 structural** — shape rules over the raw document; envelope sanity (split
//!   around P5 — see `structural::envelope`).
//! - **P5 orders** — clamp/band/magnitude on post-substitution ASTs; records the
//!   envelope-read set.
//! - **P6 leg graph** — non-resident exit paths.
//! - **P7 lints** — the three idiom lints and arm-set warnings (warnings only).
//! - **P8 assemble** — merge, sort in document order, gate.
//!
//! Worst-case activation cost is NOT stamped here: it is the layout stage's job
//! (ruled 2026-07-20, Q9) — the validator has nothing to price until scratch layout
//! exists.

pub mod context;
pub mod decimal;
pub mod declarations;
pub mod diag;
pub mod lints;
pub mod orders;
pub mod poison;
pub mod registry;
pub mod resolve;
pub mod structural;
pub mod subst;
pub mod tables;
pub mod typing;
pub mod valid;
pub mod visit;

pub use diag::{DiagCode, DiagDetail, DiagPath, Diagnostic, PathSeg, Severity};
pub use valid::ValidStrategy;

use crate::ir::{Envelope, Registry, Strategy};

/// Run the full validation pipeline.
///
/// Returns every diagnostic found (assembled in document order, warnings included)
/// plus, iff zero diagnostics are errors, the [`ValidStrategy`] typestate that
/// lowering takes. Warnings never block the gate.
pub fn validate(
    strategy: &Strategy,
    registry: &Registry,
    envelope: &Envelope,
) -> (Vec<Diagnostic>, Option<ValidStrategy>) {
    // P0: parse every embedded string; adopt the walker's diagnostics.
    let walk = crate::parse::walk_strategy(strategy);
    let mut diagnostics: Vec<Diagnostic> = walk
        .diagnostics
        .iter()
        .map(|d| Diagnostic::parse(&d.path, d.error.clone()))
        .collect();

    // P1: declarations and resolution tables.
    let tables = tables::Tables::build(strategy, registry, walk.walked);
    let mut poisons = poison::Poisons::seed(strategy, &walk.diagnostics, &tables);
    declarations::check(strategy, registry, &tables, &mut diagnostics);

    // P2: the named-expression DAG — cycles, poison propagation, substitution memo.
    let subst = subst::Subst::build(&tables, &mut poisons, &mut diagnostics);

    // P3: names inside JSON references and parsed strings, then registry
    // cross-checks over the resolved names.
    resolve::check(strategy, &tables, &mut diagnostics);
    registry::check(strategy, registry, &tables, &poisons, &mut diagnostics);
    let pins = context::check(strategy, registry, &tables, &poisons, &mut diagnostics);
    typing::check(strategy, registry, &tables, &pins, &mut diagnostics);

    // P4: structural rules over the raw document (never suppressed — JSON only).
    // The envelope read-set check is NOT here: it runs after P5 (see `structural`).
    structural::check(strategy, envelope, &mut diagnostics);

    // P5: clamp/band/magnitude over post-substitution order expressions, then the
    // envelope-read set — which is why the envelope pass is split (see `structural`).
    orders::check(strategy, &tables, &poisons, &subst, &mut diagnostics);
    let envelope_reads = orders::reads::envelope_reads(&tables);
    structural::envelope::check_read_set(envelope, &envelope_reads, &mut diagnostics);

    // P7: the idiom lints and arm-set warnings (warnings only).
    lints::check(strategy, registry, &tables, &poisons, &mut diagnostics);

    // P8: document order (stable, so equal paths keep emission order), then the
    // typestate gate — produced iff zero ERRORS; warnings ride along.
    diagnostics.sort_by(|a, b| a.path.cmp(&b.path));
    let valid = if diagnostics.iter().any(Diagnostic::is_error) {
        None
    } else {
        Some(ValidStrategy::new(
            strategy.clone(),
            tables,
            subst,
            pins,
            envelope.clone(),
            envelope_reads,
        ))
    };
    (diagnostics, valid)
}
