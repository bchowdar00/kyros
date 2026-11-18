//! Family D: structural rules over the raw document, plus envelope sanity.
//!
//! Everything here reads typed JSON only — no ASTs — so this family runs at full
//! fidelity on documents whose every expression string is broken. Nothing in it
//! consults the poison set.
//!
//! THE ENVELOPE PASS IS SPLIT AROUND P5 — DO NOT "SIMPLIFY" IT BACK INTO ONE CALL.
//! `envelope::check_sanity` runs with the rest of family D (it reads only the
//! envelope document), but `envelope::check_read_set` cannot run until the orders
//! pass (P5) has collected the program's envelope-read set from the substituted
//! ASTs — "an envelope missing a read field does not load" is a judgment about
//! WHICH fields the program reads, and that set does not exist yet when P4 runs.
//! `validate::mod` calls the two halves at their respective points in the pipeline.

pub mod envelope;
mod intents;
mod legs;
mod sections;
mod timers;

use crate::ir::Strategy;

use super::diag::Diagnostic;

/// Run the AST-free half of family D (everything except the read-set envelope check).
pub fn check(strategy: &Strategy, env: &crate::ir::Envelope, diagnostics: &mut Vec<Diagnostic>) {
    sections::check(strategy, diagnostics);
    intents::check(strategy, diagnostics);
    legs::check(strategy, diagnostics);
    timers::check(strategy, diagnostics);
    envelope::check_sanity(env, diagnostics);
}
