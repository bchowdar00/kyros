//! The band rule: a limit price's clamp bounds must be decimal literals falling
//! inside the declared `price_band` — a compile-time guarantee only; the band never
//! crosses the wire.
//!
//! Skips: no band (structural rules own presence), an inverted band (band-inverted
//! already fired — containment against a garbage interval would be noise), a price
//! that is not clamp-wrapped (not-clamp-wrapped already fired).

use std::cmp::Ordering;

use crate::ir::PriceBand;

use crate::parse::ast::expr::{Expr, Literal, UnaryOp};
use crate::parse::builtins::Builtin;
use crate::parse::span::Span;
use crate::validate::decimal::cmp_decimal;
use crate::validate::diag::{DiagCode, DiagPath, Diagnostic};

use super::{OrderExpr, OrderRole};

/// Check one substituted price expression against its band.
pub(super) fn check(
    order: &OrderExpr<'_>,
    role: OrderRole,
    band: Option<&PriceBand>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    if role != OrderRole::Price {
        return;
    }
    let Some(band) = band else { return };
    if cmp_decimal(band.lo().as_str(), band.hi().as_str()) == Ordering::Greater {
        return; // band-inverted already reported; containment would be noise
    }
    let Expr::Call {
        builtin: Builtin::Clamp,
        args,
        ..
    } = &order.ast.node
    else {
        return; // not-clamp-wrapped already reported
    };
    let [_, lo, hi] = args.as_slice() else {
        return; // arity is parser-enforced; nothing to check if it ever differs
    };

    let whole = Span::new(0, order.source.len() as u32);
    let (Some(lo), Some(hi)) = (literal_decimal(lo), literal_decimal(hi)) else {
        diagnostics.push(
            Diagnostic::rule(
                DiagCode::BandUnverifiable,
                DiagPath::from_walker(&order.path),
                "the price band cannot be verified: clamp bounds must be literal decimals",
            )
            .with_source(order.source.to_string(), whole)
            .with_help("write the clamp's lo and hi as canonical decimals inside the band"),
        );
        return;
    };

    let lo_ok = cmp_decimal(&lo, band.lo().as_str()) != Ordering::Less;
    let hi_ok = cmp_decimal(&hi, band.hi().as_str()) != Ordering::Greater;
    if !lo_ok || !hi_ok {
        diagnostics.push(
            Diagnostic::rule(
                DiagCode::BandViolation,
                DiagPath::from_walker(&order.path),
                format!(
                    "clamp bounds [{lo}, {hi}] exceed price_band [{}, {}]",
                    band.lo().as_str(),
                    band.hi().as_str()
                ),
            )
            .with_source(order.source.to_string(), whole)
            .with_help("tighten the clamp bounds to fall inside the declared band"),
        );
    }
}

/// A literal decimal clamp bound, allowing a single unary minus (`-0.5` then fails
/// containment against an unsigned band, which is the right diagnostic).
fn literal_decimal(expr: &crate::parse::span::Spanned<Expr>) -> Option<String> {
    match &expr.node {
        Expr::Literal(Literal::Decimal(text)) => Some(text.to_string()),
        Expr::Unary {
            op: UnaryOp::Neg,
            operand,
        } => match &operand.node {
            Expr::Literal(Literal::Decimal(text)) => Some(format!("-{text}")),
            Expr::Literal(Literal::Bool(_) | Literal::DurationMs(_) | Literal::Variant(_))
            | Expr::Ref(_)
            | Expr::Unary { .. }
            | Expr::Arith { .. }
            | Expr::Cmp { .. }
            | Expr::Logic { .. }
            | Expr::If { .. }
            | Expr::Call { .. }
            | Expr::Triggered { .. }
            => None,
        },
        Expr::Literal(Literal::Bool(_) | Literal::DurationMs(_) | Literal::Variant(_))
        | Expr::Ref(_)
        | Expr::Unary { .. }
        | Expr::Arith { .. }
        | Expr::Cmp { .. }
        | Expr::Logic { .. }
        | Expr::If { .. }
        | Expr::Call { .. }
        | Expr::Triggered { .. }
        => None,
    }
}
