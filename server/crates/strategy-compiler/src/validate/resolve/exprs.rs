//! Resolution of names written inside expression strings: namespace references and
//! `triggered()` arguments, plus statement lvalues.

use crate::parse::ast::expr::{Expr, Namespace};
use crate::parse::ast::stmt::Stmt;
use crate::parse::span::{Span, Spanned};
use crate::parse::walk::{ParsedString, WalkedString};
use crate::validate::diag::{DiagCode, DiagPath, Diagnostic};
use crate::validate::tables::Tables;
use crate::validate::visit::{walk_expr, walk_stmt};

/// Check every namespaced reference in every parsed string.
pub fn check(tables: &Tables, diagnostics: &mut Vec<Diagnostic>) {
    for walked in &tables.asts {
        match &walked.parsed {
            ParsedString::Expr(expr) => {
                walk_expr(expr, &mut |node| {
                    check_node(node, walked, tables, diagnostics);
                });
            }
            ParsedString::Stmt(stmt) => {
                lvalue(stmt, walked, tables, diagnostics);
                walk_stmt(stmt, &mut |node| {
                    check_node(node, walked, tables, diagnostics);
                });
            }
        }
    }
}

/// The statement's target slot. `walk_stmt` visits embedded expressions only; the
/// lvalue is a bare slot name outside any expression node.
fn lvalue(
    stmt: &Spanned<Stmt>,
    walked: &WalkedString,
    tables: &Tables,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let slot = match &stmt.node {
        Stmt::Set { slot, .. } | Stmt::Push { slot, .. } => slot,
    };
    if !tables.state.contains_key(slot.node.as_ref()) {
        diagnostics.push(unknown(
            DiagCode::UnknownStateSlot,
            "state slot",
            &slot.node,
            slot.span,
            walked,
            &declared(tables.state.keys()),
        ));
    }
}

fn check_node(
    node: &Spanned<Expr>,
    walked: &WalkedString,
    tables: &Tables,
    diagnostics: &mut Vec<Diagnostic>,
) {
    match &node.node {
        Expr::Triggered { name } => {
            if !tables.triggers.contains_key(name.node.as_ref()) {
                diagnostics.push(unknown(
                    DiagCode::UnknownTrigger,
                    "trigger",
                    &name.node,
                    name.span,
                    walked,
                    &declared(tables.triggers.keys()),
                ));
            }
        }
        Expr::Ref(r) => {
            // Every namespace's first path segment resolves against its section;
            // deeper segments and index legality belong to later families.
            let head = match r.path.first() {
                Some(head) => head,
                // The parser guarantees at least one segment; an empty path cannot
                // reach here, and emitting nothing is safe if it ever did.
                None => return,
            };
            match r.namespace {
                Namespace::Signal => {
                    if !tables.signals.contains_key(head.node.as_ref()) {
                        diagnostics.push(unknown(
                            DiagCode::UnknownSignal,
                            "signal",
                            &head.node,
                            head.span,
                            walked,
                            &declared(tables.signals.keys()),
                        ));
                    }
                }
                Namespace::State => {
                    if !tables.state.contains_key(head.node.as_ref()) {
                        diagnostics.push(unknown(
                            DiagCode::UnknownStateSlot,
                            "state slot",
                            &head.node,
                            head.span,
                            walked,
                            &declared(tables.state.keys()),
                        ));
                    }
                }
                Namespace::Expr => {
                    if !tables.named_exprs.contains(head.node.as_ref()) {
                        diagnostics.push(unknown(
                            DiagCode::UnknownNamedExpr,
                            "expression",
                            &head.node,
                            head.span,
                            walked,
                            &declared(tables.named_exprs.iter()),
                        ));
                    } else if r.path.len() > 1 {
                        let field = &r.path[1];
                        diagnostics.push(
                            Diagnostic::rule(
                                DiagCode::ExprRefHasField,
                                DiagPath::from_walker(&walked.path),
                                format!(
                                    "named expression `{}` has no fields — `expr.` references \
                                     are the bare name",
                                    head.node
                                ),
                            )
                            .with_source(walked.source.clone(), field.span)
                            .with_help(format!("write `expr.{}`", head.node)),
                        );
                    }
                }
                Namespace::Table => {
                    if !tables.const_tables.contains(head.node.as_ref()) {
                        diagnostics.push(unknown(
                            DiagCode::UnknownTable,
                            "table",
                            &head.node,
                            head.span,
                            walked,
                            &declared(tables.const_tables.iter()),
                        ));
                    }
                }
                Namespace::Legs => {
                    if !tables.legs.contains_key(head.node.as_ref()) {
                        diagnostics.push(unknown(
                            DiagCode::UnknownLeg,
                            "leg",
                            &head.node,
                            head.span,
                            walked,
                            &declared(tables.legs.keys()),
                        ));
                    }
                }
                Namespace::Param => {
                    if !tables.params.contains_key(head.node.as_ref()) {
                        diagnostics.push(unknown(
                            DiagCode::UnknownParam,
                            "param",
                            &head.node,
                            head.span,
                            walked,
                            &declared(tables.params.keys()),
                        ));
                    } else if r.path.len() > 1 {
                        let field = &r.path[1];
                        diagnostics.push(
                            Diagnostic::rule(
                                DiagCode::UnknownParam,
                                DiagPath::from_walker(&walked.path),
                                format!(
                                    "param `{}` has no fields — `param.` references are the bare name",
                                    head.node
                                ),
                            )
                            .with_source(walked.source.clone(), field.span)
                            .with_help(format!("write `param.{}`", head.node)),
                        );
                    }
                }
                // Fixed vocabularies (checked by the context family) or payload
                // fields (checked by the registry family once the pin is known).
                Namespace::Event | Namespace::Platform | Namespace::Entry | Namespace::Envelope => {
                }
            }
        }
        Expr::Literal(_)
        | Expr::Unary { .. }
        | Expr::Arith { .. }
        | Expr::Cmp { .. }
        | Expr::Logic { .. }
        | Expr::If { .. }
        | Expr::Call { .. }
        => {}
    }
}

fn unknown(
    code: DiagCode,
    construct: &str,
    name: &str,
    span: Span,
    walked: &WalkedString,
    declared: &str,
) -> Diagnostic {
    Diagnostic::rule(
        code,
        DiagPath::from_walker(&walked.path),
        format!("reference to undeclared {construct} `{name}`"),
    )
    .with_source(walked.source.clone(), span)
    .with_help(format!("declared {construct}s: {declared}"))
}

fn declared<'a>(names: impl Iterator<Item = &'a String>) -> String {
    let joined: Vec<&str> = names.map(String::as_str).collect();
    if joined.is_empty() {
        "(none)".to_string()
    } else {
        joined.join(", ")
    }
}
