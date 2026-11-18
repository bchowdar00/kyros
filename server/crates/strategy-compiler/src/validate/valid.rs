//! The typestate gate: [`ValidStrategy`] exists iff validation found zero errors.
//!
//! Nothing downstream of the validator ever accepts an unvalidated strategy —
//! lowering takes this type, and this type has ONE constructor, private to the
//! `validate` module tree. No `From`, no `Default`, no field exposure: the type's
//! existence is the proof of validation (the typestate pattern — like Python code
//! that only hands you an object through the factory that checked it, but enforced
//! by the compiler instead of convention).

use std::collections::BTreeSet;

use crate::ir::{Envelope, Strategy};

use crate::parse::span::Spanned;

use super::context::{EnvelopeField, Pins};
use super::subst::Subst;
use super::tables::Tables;

/// A strategy that passed validation, together with everything the validator built
/// that lowering wants: the resolution tables (with every parsed AST), the
/// substitution environment, the per-block pins, and the envelope with its read set.
#[derive(Debug, Clone, PartialEq)]
pub struct ValidStrategy {
    strategy: Strategy,
    tables: Tables,
    subst: Subst,
    pins: Pins,
    envelope: Envelope,
    envelope_reads: BTreeSet<EnvelopeField>,
}

impl ValidStrategy {
    /// The one constructor. `pub(super)` — only `validate::validate` can call it,
    /// and only after counting zero errors.
    pub(super) fn new(
        strategy: Strategy,
        tables: Tables,
        subst: Subst,
        pins: Pins,
        envelope: Envelope,
        envelope_reads: BTreeSet<EnvelopeField>,
    ) -> ValidStrategy {
        ValidStrategy {
            strategy,
            tables,
            subst,
            pins,
            envelope,
            envelope_reads,
        }
    }

    /// The validated document.
    pub fn strategy(&self) -> &Strategy {
        &self.strategy
    }

    /// The resolution tables, including every parsed AST in document order.
    pub fn tables(&self) -> &Tables {
        &self.tables
    }

    /// Fully inline an expression through the substitution environment.
    pub fn substituted(
        &self,
        expr: &Spanned<crate::parse::ast::expr::Expr>,
    ) -> Option<Spanned<crate::parse::ast::expr::Expr>> {
        self.subst.substituted(expr)
    }

    /// The per-block pin decisions.
    pub fn pins(&self) -> &Pins {
        &self.pins
    }

    /// The submission's envelope, carried verbatim onto the manifest.
    pub fn envelope(&self) -> &Envelope {
        &self.envelope
    }

    /// The envelope fields the program reads (the manifest records this).
    pub fn envelope_reads(&self) -> &BTreeSet<EnvelopeField> {
        &self.envelope_reads
    }
}
