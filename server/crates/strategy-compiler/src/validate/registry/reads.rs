//! `signal.<nick>.<field>` body reads against the registry: the field exists and the
//! path has exactly two segments (nickname, field).
//!
//! `event.*` reads are NOT here: their signal is the block's pinned trigger, so the
//! check lives with the pin rule in the context family.

use crate::ir::Registry;

use crate::parse::ast::expr::{Expr, Namespace};
use crate::parse::walk::{ParsedString, WalkedString};
use crate::validate::diag::{DiagCode, DiagPath, Diagnostic};
use crate::validate::poison::Poisons;
use crate::validate::tables::Tables;
use crate::validate::visit::{walk_expr, walk_stmt};

/// Check every `signal.*` read in every parsed string.
pub fn check(
    registry: &Registry,
    tables: &Tables,
    poisons: &Poisons,
    diagnostics: &mut Vec<Diagnostic>,
) {
    for walked in &tables.asts {
        let mut visit = |node: &crate::parse::span::Spanned<Expr>| {
            signal_read(node, walked, registry, tables, poisons, diagnostics);
        };
        match &walked.parsed {
            ParsedString::Expr(expr) => walk_expr(expr, &mut visit),
            ParsedString::Stmt(stmt) => walk_stmt(stmt, &mut visit),
        }
    }
}

fn signal_read(
    node: &crate::parse::span::Spanned<Expr>,
    walked: &WalkedString,
    registry: &Registry,
    tables: &Tables,
    poisons: &Poisons,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let Expr::Ref(r) = &node.node else { return };
    if r.namespace != Namespace::Signal {
        return;
    }
    let Some(nick) = r.path.first() else { return };
    // Undeclared (family A reported) or unresolved (`unknown-signal-id` reported).
    if poisons.unresolved_signals.contains(nick.node.as_ref()) {
        return;
    }
    let Some(fields) = super::signal_fields(nick.node.as_ref(), registry, tables) else {
        return;
    };

    let path = DiagPath::from_walker(&walked.path);
    match r.path.len() {
        // `signal.price` alone is not a value; the payload field is required.
        1 => diagnostics.push(
            Diagnostic::rule(
                DiagCode::UnknownField,
                path,
                format!(
                    "`signal.{}` reads no field — a snapshot read is `signal.{}.<field>`",
                    nick.node, nick.node
                ),
            )
            .with_source(walked.source.clone(), node.span)
            .with_help(format!("declared fields: {}", field_list(fields))),
        ),
        2 => {
            let field = &r.path[1];
            if !fields
                .iter()
                .any(|f| f.name.as_str() == field.node.as_ref())
            {
                diagnostics.push(
                    Diagnostic::rule(
                        DiagCode::UnknownField,
                        path,
                        format!("signal `{}` has no field `{}`", nick.node, field.node),
                    )
                    .with_source(walked.source.clone(), field.span)
                    .with_help(format!("declared fields: {}", field_list(fields))),
                );
            }
        }
        // Payload fields are flat scalars; a third segment reads into nothing.
        _ => {
            let extra = &r.path[2];
            diagnostics.push(
                Diagnostic::rule(
                    DiagCode::UnknownField,
                    path,
                    format!(
                        "signal fields are flat — `signal.{}.{}` has no `{}`",
                        nick.node, r.path[1].node, extra.node
                    ),
                )
                .with_source(walked.source.clone(), extra.span),
            );
        }
    }
}

fn field_list(fields: &[crate::ir::FieldDef]) -> String {
    fields
        .iter()
        .map(|f| f.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}
