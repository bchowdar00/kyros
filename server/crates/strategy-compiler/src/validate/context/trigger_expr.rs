//! The trigger-expression purity rule (v0.3 cross-signal triggers).
//!
//! A trigger leaf's `expr` is a body expression, but the SME evaluates it with zero
//! strategy state against the snapshot store, so it may read only `signal.*` and
//! `param.*` (and `envelope.*`, which is a compile-time constant), use decimal and
//! duration literals, arithmetic, unary minus, and the math builtins. Everything that
//! makes an expression depend on the activation — `state.*`, `event.*`, `platform.*`,
//! `legs.*`, `expr.*`, tables, `triggered()`, comparisons, booleans and conditionals
//! (the leaf's own comparator is the one comparison) — is refused. A leaf expression
//! must also read at least one signal, or it is not a trigger.

use crate::parse::ast::expr::{Expr, Literal, Namespace, UnaryOp};
use crate::parse::builtins::{Builtin, BuiltinCategory, BUILTINS};
use crate::parse::span::Span;
use crate::parse::walk::StringRole;
use crate::validate::diag::{DiagCode, DiagPath, Diagnostic};
use crate::validate::tables::Tables;
use crate::validate::visit::walk_string;

/// Run the purity rule over every trigger expression.
pub fn check(tables: &Tables, diagnostics: &mut Vec<Diagnostic>) {
    for walked in &tables.asts {
        if walked.role != StringRole::TriggerExpr {
            continue;
        }
        let mut reads_signal = false;
        walk_string(walked, &mut |node| {
            let refused: Option<&str> = match &node.node {
                Expr::Ref(r) => match r.namespace {
                    Namespace::Signal => {
                        reads_signal = true;
                        None
                    }
                    Namespace::Param | Namespace::Envelope => None,
                    Namespace::State => Some("state is strategy-private; the SME cannot read it"),
                    Namespace::Event => Some("there is no triggering event inside a trigger"),
                    Namespace::Platform => Some("platform reads are activation-time values"),
                    Namespace::Legs => Some("fill context is activation-time state"),
                    Namespace::Expr => Some("named expressions are body-side only"),
                    Namespace::Table => Some("tables are body-side only"),
                    Namespace::Entry => Some("`entry.*` means nothing outside a reducer"),
                },
                Expr::Literal(Literal::Decimal(_) | Literal::DurationMs(_)) => None,
                Expr::Literal(Literal::Bool(_)) => Some("a trigger expression is numeric"),
                Expr::Literal(Literal::Variant(_)) => {
                    Some("compare enum fields with a field leaf, not an expr leaf")
                }
                Expr::Unary { op: UnaryOp::Neg, .. } | Expr::Arith { .. } => None,
                Expr::Unary { op: UnaryOp::Not, .. } | Expr::Logic { .. } => {
                    Some("a multi-condition trigger is an all/any group of leaves")
                }
                Expr::Cmp { .. } => Some("the comparator and value live in the leaf's own JSON fields"),
                Expr::If { .. } => Some("piecewise logic belongs in a body expression"),
                Expr::Triggered { .. } => Some("`triggered()` is legal only in a block guard"),
                Expr::Call { builtin, .. } => match category(*builtin) {
                    BuiltinCategory::Math | BuiltinCategory::Analytic => None,
                    BuiltinCategory::Table => Some("tables are body-side only"),
                    BuiltinCategory::Reducer => Some("windows are strategy-private state"),
                    BuiltinCategory::Trigger => Some("`triggered()` is legal only in a block guard"),
                },
            };
            if let Some(why) = refused {
                diagnostics.push(impure(walked, node.span, why));
            }
        });
        if !reads_signal {
            diagnostics.push(impure(
                walked,
                Span::new(0, walked.source.len() as u32),
                "a trigger expression must read at least one signal field",
            ));
        }
    }
}

fn category(builtin: Builtin) -> BuiltinCategory {
    BUILTINS
        .iter()
        .find(|info| info.builtin == builtin)
        .expect("every builtin has a table row")
        .category
}

fn impure(walked: &crate::parse::walk::WalkedString, span: Span, why: &str) -> Diagnostic {
    Diagnostic::rule(
        DiagCode::TriggerExprImpure,
        DiagPath::from_walker(&walked.path),
        format!("not allowed in a trigger expression: {why}"),
    )
    .with_source(walked.source.clone(), span)
    .with_help(
        "a trigger expression is arithmetic over signal.<nick>.<field>, param.* and \
         envelope.* reads with the math builtins; anything else belongs in a block guard",
    )
}
