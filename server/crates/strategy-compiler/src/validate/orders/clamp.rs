//! The clamp rule: price and size expressions must be clamp-wrapped on the
//! POST-SUBSTITUTION AST — naming a formula never changes validation. `max_size` is
//! exempt (it is a bound, not an emitted value).

use crate::parse::ast::expr::Expr;
use crate::parse::builtins::Builtin;
use crate::validate::diag::{DiagCode, DiagPath, Diagnostic};

use super::{OrderExpr, OrderRole};

/// Check one substituted order expression's clamp shape.
pub(super) fn check(
    order: &OrderExpr<'_>,
    role: OrderRole,
    _band: Option<&crate::ir::PriceBand>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let what = match role {
        OrderRole::Price => "price",
        OrderRole::Size => "size",
        OrderRole::MaxSize => return, // clamp-exempt
    };
    if matches!(
        &order.ast.node,
        Expr::Call {
            builtin: Builtin::Clamp,
            ..
        }
    ) {
        return;
    }
    let mut diag = Diagnostic::rule(
        DiagCode::NotClampWrapped,
        DiagPath::from_walker(&order.path),
        format!("a {what} expression must be clamp-wrapped"),
    );
    diag = match &order.via_named {
        Some(name) => diag.with_help(format!(
            "`expr.{name}` expands to an unclamped expression — wrap the named \
             expression's body (or the reference) in clamp(<expr>, <lo>, <hi>)"
        )),
        None => diag.with_help("write clamp(<expr>, <lo>, <hi>)"),
    };
    // Spans inside a substituted AST index the NAMED expression's source, not this
    // string — underline the whole original expression instead.
    let whole = crate::parse::span::Span::new(0, order.source.len() as u32);
    diagnostics.push(diag.with_source(order.source.to_string(), whole));
}
