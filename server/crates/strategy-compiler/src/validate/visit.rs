//! A shared preorder expression walker. Every per-string rule family traverses ASTs;
//! this is the one traversal, so an `Expr` variant added later breaks exactly one
//! exhaustive match (the workspace lints forbid catch-all arms over IR-shaped enums).

use crate::parse::ast::expr::Expr;
use crate::parse::ast::stmt::Stmt;
use crate::parse::span::Spanned;
use crate::parse::walk::{ParsedString, WalkedString};

/// Visit every expression node of a walked string, whatever its shape.
pub fn walk_string<'a>(walked: &'a WalkedString, visit: &mut impl FnMut(&'a Spanned<Expr>)) {
    match &walked.parsed {
        ParsedString::Expr(expr) => walk_expr(expr, visit),
        ParsedString::Stmt(stmt) => walk_stmt(stmt, visit),
    }
}

/// Visit `expr` and every sub-expression, preorder (parents before children,
/// document order among siblings).
pub fn walk_expr<'a>(expr: &'a Spanned<Expr>, visit: &mut impl FnMut(&'a Spanned<Expr>)) {
    visit(expr);
    match &expr.node {
        Expr::Literal(_) | Expr::Triggered { .. } => {}
        Expr::Ref(r) => {
            if let Some(index) = &r.index {
                for key in index {
                    walk_expr(key, visit);
                }
            }
        }
        Expr::Unary { operand, .. } => walk_expr(operand, visit),
        Expr::Arith { lhs, rhs, .. }
        | Expr::Cmp { lhs, rhs, .. }
        | Expr::Logic { lhs, rhs, .. } => {
            walk_expr(lhs, visit);
            walk_expr(rhs, visit);
        }
        Expr::If { arms, else_value } => {
            for arm in arms {
                walk_expr(&arm.condition, visit);
                walk_expr(&arm.value, visit);
            }
            walk_expr(else_value, visit);
        }
        Expr::Call { args, .. } => {
            for arg in args {
                walk_expr(arg, visit);
            }
        }
    }
}

/// Visit every expression embedded in a statement (index keys, then the right-hand
/// side), preorder within each.
pub fn walk_stmt<'a>(stmt: &'a Spanned<Stmt>, visit: &mut impl FnMut(&'a Spanned<Expr>)) {
    match &stmt.node {
        Stmt::Set { index, rhs, .. } => {
            if let Some(index) = index {
                for key in index {
                    walk_expr(key, visit);
                }
            }
            walk_expr(rhs, visit);
        }
        Stmt::Push { value, .. } => walk_expr(value, visit),
    }
}
