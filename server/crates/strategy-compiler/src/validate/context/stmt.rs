//! `set` lvalue shape and typing: indexing against the slot kind, index arity and
//! types against `key_types`, right-hand side against `value_type`.
//!
//! `push` types its value against the window's value type; the slot-kind rules
//! (`push` to a non-window, `set` on a window) live in refs.rs.

use crate::ir::{StateSlot, Strategy, TypeSpec};

use crate::parse::ast::expr::{Expr, Literal};
use crate::parse::span::Spanned;
use crate::parse::walk::{ParsedString, StringRole};
use crate::validate::diag::{DiagCode, DiagPath, Diagnostic};
use crate::validate::tables::Tables;
use crate::validate::typing::{self, TypeCtx, ValueType};

use super::pin::Pins;

/// Check every `set` statement.
pub fn check(
    strategy: &Strategy,
    registry: &crate::ir::Registry,
    tables: &Tables,
    pins: &Pins,
    diagnostics: &mut Vec<Diagnostic>,
) {
    for (l, leg) in strategy.legs.iter().enumerate() {
        for (b, block) in leg.body.iter().enumerate() {
            for (d, _) in block.do_items.iter().enumerate() {
                let path = format!("legs[{l}].body[{b}].do[{d}]");
                let Some(walked) = tables.ast_at(&path) else {
                    continue;
                };
                if walked.role != StringRole::Statement {
                    continue;
                }
                let ParsedString::Stmt(stmt) = &walked.parsed else {
                    continue;
                };
                let ctx = TypeCtx {
                    registry,
                    tables,
                    strategy,
                    pin: Some(pins.at(l, b)),
                    walked,
                };
                match &stmt.node {
                    crate::parse::ast::stmt::Stmt::Set { slot, index, rhs } => {
                        check_set(slot, index.as_deref(), rhs, &ctx, diagnostics);
                    }
                    crate::parse::ast::stmt::Stmt::Push { slot, value } => {
                        if let Some(&i) = tables.state.get(slot.node.as_ref())
                            && let StateSlot::Window { value_type, .. } = &strategy.state[i]
                        {
                            rhs_check(value, value_type, &ctx, diagnostics);
                        }
                    }
                }
            }
        }
    }
}

fn check_set(
    slot: &Spanned<Box<str>>,
    index: Option<&[Spanned<Expr>]>,
    rhs: &Spanned<Expr>,
    ctx: &TypeCtx<'_>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    // Unknown slot: family A reported it; nothing here can be judged.
    let Some(&slot_index) = ctx.tables.state.get(slot.node.as_ref()) else {
        return;
    };
    match &ctx.strategy.state[slot_index] {
        // refs.rs reports `set` on a window.
        StateSlot::Window { .. } => {}
        StateSlot::Scalar { value_type, .. } => {
            if index.is_some() {
                diagnostics.push(
                    Diagnostic::rule(
                        DiagCode::SetScalarIndexed,
                        DiagPath::from_walker(&ctx.walked.path),
                        format!("state slot `{}` is a scalar — it takes no index", slot.node),
                    )
                    .with_source(ctx.walked.source.clone(), slot.span)
                    .with_help(format!("write `set state.{} = ...`", slot.node)),
                );
            }
            rhs_check(rhs, value_type, ctx, diagnostics);
        }
        StateSlot::Map {
            key_types,
            value_type,
            ..
        } => {
            let Some(keys) = index else {
                diagnostics.push(
                    Diagnostic::rule(
                        DiagCode::SetMapUnindexed,
                        DiagPath::from_walker(&ctx.walked.path),
                        format!("state slot `{}` is a map — a set must index it", slot.node),
                    )
                    .with_source(ctx.walked.source.clone(), slot.span)
                    .with_help(format!(
                        "write `set state.{}[{}] = ...`",
                        slot.node,
                        vec!["<key>"; key_types.len()].join(", ")
                    )),
                );
                rhs_check(rhs, value_type, ctx, diagnostics);
                return;
            };
            if keys.len() != key_types.len() {
                diagnostics.push(
                    Diagnostic::rule(
                        DiagCode::IndexArityMismatch,
                        DiagPath::from_walker(&ctx.walked.path),
                        format!(
                            "state slot `{}` is keyed by {} key(s); this set writes {}",
                            slot.node,
                            key_types.len(),
                            keys.len()
                        ),
                    )
                    .with_source(ctx.walked.source.clone(), slot.span),
                );
            }
            // Type-check the keys that do line up, even under an arity mismatch.
            for (key, spec) in keys.iter().zip(key_types.iter()) {
                value_against_spec(
                    key,
                    spec,
                    DiagCode::IndexTypeMismatch,
                    "key",
                    ctx,
                    diagnostics,
                );
            }
            rhs_check(rhs, value_type, ctx, diagnostics);
        }
    }
}

fn rhs_check(
    rhs: &Spanned<Expr>,
    value_type: &TypeSpec,
    ctx: &TypeCtx<'_>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    value_against_spec(
        rhs,
        value_type,
        DiagCode::SetRhsTypeMismatch,
        "value",
        ctx,
        diagnostics,
    );
}

/// Judge one expression against a declared `TypeSpec`. Definite mismatches only:
/// `Unknown` on either side stays silent.
fn value_against_spec(
    expr: &Spanned<Expr>,
    spec: &TypeSpec,
    code: DiagCode,
    what: &str,
    ctx: &TypeCtx<'_>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let target = typing::spec_type(spec);

    // A bare variant literal types by context: membership in the target's set.
    if let Expr::Literal(Literal::Variant(variant)) = &expr.node {
        match &target {
            ValueType::Enum(variants) => {
                if !variants.contains(variant.as_ref()) {
                    let known: Vec<&str> = variants.iter().map(String::as_str).collect();
                    diagnostics.push(
                        Diagnostic::rule(
                            code,
                            DiagPath::from_walker(&ctx.walked.path),
                            format!(
                                "'{variant}' is not a variant of the declared {what} type \
                                 (variants: {})",
                                known.join(", ")
                            ),
                        )
                        .with_source(ctx.walked.source.clone(), expr.span),
                    );
                }
            }
            ValueType::Numeric | ValueType::Bool | ValueType::Str => {
                diagnostics.push(
                    Diagnostic::rule(
                        code,
                        DiagPath::from_walker(&ctx.walked.path),
                        format!(
                            "'{variant}' is a variant — the declared {what} type is not an enum"
                        ),
                    )
                    .with_source(ctx.walked.source.clone(), expr.span),
                );
            }
            ValueType::Unknown => {}
        }
        return;
    }

    // Quiet query: the typing pass already emitted this subtree's internal
    // mismatches; only the against-the-slot judgment is new information.
    let found = typing::type_of(expr, ctx, &mut |_| {});
    let incompatible = match (&target, &found) {
        (ValueType::Numeric, ValueType::Bool | ValueType::Enum(_) | ValueType::Str)
        | (ValueType::Bool, ValueType::Numeric | ValueType::Enum(_) | ValueType::Str)
        | (ValueType::Enum(_), ValueType::Numeric | ValueType::Bool | ValueType::Str)
        | (ValueType::Str, ValueType::Numeric | ValueType::Bool | ValueType::Enum(_)) => true,
        // An enum-typed expression fits an enum-typed slot when every variant it can
        // produce is representable: subset, not equality.
        (ValueType::Enum(declared), ValueType::Enum(produced)) => !produced.is_subset(declared),
        (ValueType::Unknown, _)
        | (_, ValueType::Unknown)
        | (ValueType::Numeric, ValueType::Numeric)
        | (ValueType::Bool, ValueType::Bool)
        | (ValueType::Str, ValueType::Str) => false,
    };
    if incompatible {
        diagnostics.push(
            Diagnostic::rule(
                code,
                DiagPath::from_walker(&ctx.walked.path),
                format!("this {what} does not fit the slot's declared type"),
            )
            .with_source(ctx.walked.source.clone(), expr.span),
        );
    }
}
