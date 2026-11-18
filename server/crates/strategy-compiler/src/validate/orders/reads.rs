//! The envelope-read set: which `envelope.*` fields the program reads. Recorded on
//! `ValidStrategy` (the manifest wants it) and fed to the post-P5 envelope check.
//!
//! Collected from the RAW walked ASTs rather than the substituted ones: named
//! expression bodies are themselves walked strings, so a read reached only through
//! substitution is still seen — with one deliberate over-approximation, recorded
//! here: a named expression that nothing references still contributes its reads.
//! Over-approximating keeps the read set independent of the poison set (a broken
//! sibling string cannot shrink it), and an unused named expression is a smell the
//! author will notice anyway.

use std::collections::BTreeSet;

use crate::parse::ast::expr::{Expr, Namespace};
use crate::validate::context::EnvelopeField;
use crate::validate::tables::Tables;
use crate::validate::visit::walk_string;

/// Collect every readable envelope field the program references. Unknown or
/// unreadable fields are not here — `namespaces.rs` already errored on them.
pub fn envelope_reads(tables: &Tables) -> BTreeSet<EnvelopeField> {
    let mut reads = BTreeSet::new();
    for walked in &tables.asts {
        walk_string(walked, &mut |node| {
            if let Expr::Ref(r) = &node.node
                && r.namespace == Namespace::Envelope
                && let [field] = r.path.as_slice()
                && let Some(field) = EnvelopeField::from_name(&field.node)
            {
                reads.insert(field);
            }
        });
    }
    reads
}
