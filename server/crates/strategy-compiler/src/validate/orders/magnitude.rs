//! The magnitude rule: token- and USDC-denominated magnitudes come from `envelope.*`
//! references, never literals — an all-literal size is the ratified error case.
//! Extended to `max_size` (ruled 2026-07-20, Q5): it is the same token-denominated
//! trap, with an adjusted message.
//!
//! The scaling half (`size-not-envelope-scaled`): mentioning the envelope in a clamp
//! bound is not enough — `clamp(5 / signal.book.ask, 0, envelope.max_order_size)`
//! smuggles a literal five dollars past the first rule and would not grow with the
//! user's money. The VALUE of a size (the clamp's first argument) must itself derive
//! from the envelope, or from the position (already envelope-bounded), or be the
//! literal zero (a flatten target).

use crate::parse::ast::expr::{Expr, Literal, Namespace};
use crate::parse::builtins::Builtin;
use crate::parse::span::{Span, Spanned};
use crate::validate::diag::{DiagCode, DiagPath, Diagnostic};
use crate::validate::visit::walk_expr;

use super::{OrderExpr, OrderRole};

/// Check one substituted size or max_size expression for an envelope reference.
pub(super) fn check(order: &OrderExpr<'_>, role: OrderRole, diagnostics: &mut Vec<Diagnostic>) {
    let what = match role {
        OrderRole::Price => return, // prices are venue-scaled, not envelope-scaled
        OrderRole::Size => "size",
        OrderRole::MaxSize => "max_size",
    };
    let whole = Span::new(0, order.source.len() as u32);
    if !reads_envelope(&order.ast) {
        diagnostics.push(
            Diagnostic::rule(
                DiagCode::SizeWithoutEnvelope,
                DiagPath::from_walker(&order.path),
                format!(
                    "a {what} must be sourced from envelope.* — a literal magnitude is not allowed"
                ),
            )
            .with_source(order.source.to_string(), whole)
            .with_help(
                "scale from an envelope field (e.g. envelope.max_order_size); only \
                 dimensionless shape constants may be literals",
            ),
        );
        return;
    }
    if role != OrderRole::Size {
        return;
    }
    // The clamp rule reports a non-clamp size itself; the value is the first argument.
    let Expr::Call {
        builtin: Builtin::Clamp,
        args,
        ..
    } = &order.ast.node
    else {
        return;
    };
    let Some(value) = args.first() else { return };
    if reads_envelope(value) || reads_position(value) || is_literal_zero(value) {
        return;
    }
    diagnostics.push(
        Diagnostic::rule(
            DiagCode::SizeNotEnvelopeScaled,
            DiagPath::from_walker(&order.path),
            "a size's value (the clamp's first argument) must scale with envelope.* — \
             an envelope read in the bounds alone leaves a literal magnitude in charge",
        )
        .with_source(order.source.to_string(), whole)
        .with_help(
            "derive the value from an envelope field (e.g. \
             floor(envelope.round_budget * 0.25 / signal.book.ask)), from \
             platform.position.*, or write 0 to flatten",
        ),
    );
}

fn reads_envelope(expr: &Spanned<Expr>) -> bool {
    let mut found = false;
    walk_expr(expr, &mut |node| {
        if let Expr::Ref(r) = &node.node
            && r.namespace == Namespace::Envelope
        {
            found = true;
        }
    });
    found
}

fn reads_position(expr: &Spanned<Expr>) -> bool {
    let mut found = false;
    walk_expr(expr, &mut |node| {
        if let Expr::Ref(r) = &node.node
            && r.namespace == Namespace::Platform
            && r.path.first().is_some_and(|head| head.node.as_ref() == "position")
        {
            found = true;
        }
    });
    found
}

fn is_literal_zero(expr: &Spanned<Expr>) -> bool {
    matches!(&expr.node, Expr::Literal(Literal::Decimal(d)) if d.as_ref() == "0")
}
