//! Family E (P5): the clamp/band/magnitude rules over POST-SUBSTITUTION order
//! expressions, plus the envelope-read set.
//!
//! Everything here consumes the substitution environment: a price or size whose
//! string failed to parse, or whose substitution touches a poisoned named
//! expression, is silently skipped — the root cause is already reported once.

mod band;
mod clamp;
mod magnitude;
pub mod reads;

use crate::ir::{DoItem, Strategy};

use crate::parse::ast::expr::Expr;
use crate::parse::span::Spanned;
use crate::parse::walk::ParsedString;
use crate::validate::diag::Diagnostic;
use crate::validate::poison::Poisons;
use crate::validate::subst::Subst;
use crate::validate::tables::Tables;

/// One order expression, substituted and ready for the rule files.
pub(super) struct OrderExpr<'a> {
    /// Walker path of the original string (`legs[0].body[1].do[0].place.size`).
    pub path: String,
    /// The original source string (diagnostics quote it even though spans into the
    /// substituted AST are meaningless — the message carries the specifics).
    pub source: &'a str,
    /// Whether the original root was an `expr.` reference (help text names it).
    pub via_named: Option<String>,
    /// The fully-inlined AST.
    pub ast: Spanned<Expr>,
}

/// Which field of the intent an order expression came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum OrderRole {
    /// A limit price: clamp-wrapped, band-checked.
    Price,
    /// A size: clamp-wrapped, envelope-sourced.
    Size,
    /// A max size: envelope-sourced only (clamp-exempt).
    MaxSize,
}

/// Run family E over every intent.
pub fn check(
    strategy: &Strategy,
    tables: &Tables,
    poisons: &Poisons,
    subst: &Subst,
    diagnostics: &mut Vec<Diagnostic>,
) {
    for (l, leg) in strategy.legs.iter().enumerate() {
        for (b, block) in leg.body.iter().enumerate() {
            for (d, item) in block.do_items.iter().enumerate() {
                let base = format!("legs[{l}].body[{b}].do[{d}]");
                match item {
                    DoItem::Place(place_item) => {
                        let place = &place_item.place;
                        let base = format!("{base}.place");
                        check_order_exprs(
                            &base,
                            place.price.is_some(),
                            place.price_band.as_ref(),
                            tables,
                            poisons,
                            subst,
                            diagnostics,
                        );
                    }
                    DoItem::CancelReplace(replace_item) => {
                        let replace = &replace_item.cancel_replace;
                        let base = format!("{base}.cancel_replace");
                        check_order_exprs(
                            &base,
                            true,
                            Some(&replace.price_band),
                            tables,
                            poisons,
                            subst,
                            diagnostics,
                        );
                    }
                    DoItem::TargetPosition(_) => {
                        let base = format!("{base}.target_position");
                        check_order_exprs(&base, false, None, tables, poisons, subst, diagnostics);
                    }
                    DoItem::Statement(_) | DoItem::Cancel(_) => {}
                }
            }
        }
    }
}

fn check_order_exprs(
    base: &str,
    has_price: bool,
    band: Option<&crate::ir::PriceBand>,
    tables: &Tables,
    poisons: &Poisons,
    subst: &Subst,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let fields: [(&str, OrderRole); 3] = [
        ("price", OrderRole::Price),
        ("size", OrderRole::Size),
        ("max_size", OrderRole::MaxSize),
    ];
    for (field, role) in fields {
        if field == "price" && !has_price {
            continue;
        }
        let path = format!("{base}.{field}");
        let Some(order_expr) = substituted_at(&path, tables, poisons, subst) else {
            continue;
        };
        clamp::check(&order_expr, role, band, diagnostics);
        band::check(&order_expr, role, band, diagnostics);
        magnitude::check(&order_expr, role, diagnostics);
    }
}

/// Fetch and substitute one order expression; `None` suppresses (parse failure or
/// poisoned substitution — already reported).
fn substituted_at<'a>(
    path: &str,
    tables: &'a Tables,
    poisons: &Poisons,
    subst: &Subst,
) -> Option<OrderExpr<'a>> {
    if poisons.failed_strings.contains(path) {
        return None;
    }
    let walked = tables.ast_at(path)?;
    let ParsedString::Expr(original) = &walked.parsed else {
        return None;
    };
    let via_named = match &original.node {
        Expr::Ref(r) if r.namespace == crate::parse::ast::expr::Namespace::Expr => {
            r.path.first().map(|head| head.node.to_string())
        }
        Expr::Literal(_)
        | Expr::Ref(_)
        | Expr::Unary { .. }
        | Expr::Arith { .. }
        | Expr::Cmp { .. }
        | Expr::Logic { .. }
        | Expr::If { .. }
        | Expr::Call { .. }
        | Expr::Triggered { .. }
        => None,
    };
    let ast = subst.substituted(original)?;
    Some(OrderExpr {
        path: path.to_string(),
        source: &walked.source,
        via_named,
        ast,
    })
}
