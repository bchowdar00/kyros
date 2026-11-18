//! The document walker: parse every embedded expression string in a deserialized
//! [`crate::ir::Strategy`], producing either a fully-parsed view or path-carrying diagnostics.
//!
//! This is the validator's front door. Collect-all across strings, first-error within
//! each string (the standing rule): a broken document reports every broken string in one
//! pass, each diagnostic anchored by the JSON path to the string
//! (`legs[0].body[2].if`, `triggers[1].condition.all[0].expr`) plus the span inside it.

use serde::Serialize;

use crate::parse::ast::expr::Expr;
use crate::parse::ast::stmt::Stmt;
use crate::parse::error::ParseError;
use crate::parse::parser::{parse_expr, parse_stmt};
use crate::parse::span::Spanned;

/// What kind of string a walked item came from — the validator branches on this (guards
/// get the `triggered()` rules, prices get the clamp rule, and so on).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StringRole {
    /// A block's `if` guard.
    Guard,
    /// A `do`-list statement.
    Statement,
    /// A named expression body.
    NamedExpr,
    /// An intent `price` expression.
    Price,
    /// An intent `size` expression.
    Size,
    /// An intent `max_size` expression.
    MaxSize,
    /// A trigger leaf `expr`: a body expression restricted to signal/param reads and
    /// arithmetic (`validate::context::trigger_expr`).
    TriggerExpr,
}

/// One parsed string: expression-shaped or statement-shaped.
#[derive(Debug, Clone, PartialEq)]
pub enum ParsedString {
    /// A body or trigger expression AST.
    Expr(Spanned<Expr>),
    /// A statement AST.
    Stmt(Spanned<Stmt>),
}

/// One successfully parsed embedded string, with its provenance.
#[derive(Debug, Clone, PartialEq)]
pub struct WalkedString {
    /// JSON path to the string in the document (`legs[0].body[2].if`).
    pub path: String,
    /// What the string is, for the validator's dispatch.
    pub role: StringRole,
    /// The parsed AST.
    pub parsed: ParsedString,
    /// The source string the AST's spans index into — carried so validator
    /// diagnostics can render source-and-caret lines without re-walking the document.
    pub source: String,
}

/// One parse failure, anchored by document path. Serializes for the LLM repair loop —
/// the embedded [`ParseError`] already carries its span, message, help, and source.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WalkDiagnostic {
    /// JSON path to the offending string.
    pub path: String,
    /// The parse failure inside that string.
    pub error: ParseError,
}

/// The walker's result: PARTIAL SUCCESS, never all-or-nothing. Everything that parsed
/// is available for validation even when siblings failed — the validator validates what
/// parsed, appends its own diagnostics, and the repair loop gets ONE combined list per
/// round instead of paying a round trip per error layer.
///
/// Validator caution (recorded here because this type is its input): suppress cascade
/// errors rooted at a failed string — do not also report "unknown reference" for a name
/// that only a failed string would have used or defined.
#[derive(Debug, Clone, PartialEq)]
pub struct WalkOutcome {
    /// Everything that parsed, in document order.
    pub walked: Vec<WalkedString>,
    /// Everything that didn't (collect-all across strings; first error within each).
    pub diagnostics: Vec<WalkDiagnostic>,
}

/// Parse every embedded expression string in the strategy.
///
/// Walk order is document order, so both lists read top-to-bottom. Path-scoped strings
/// are independent, so validating around a hole is safe.
pub fn walk_strategy(strategy: &crate::ir::Strategy) -> WalkOutcome {
    let mut walk = Walk {
        parsed: Vec::new(),
        diagnostics: Vec::new(),
    };

    if let Some(expressions) = &strategy.expressions
        && let Some(named) = &expressions.named
    {
        for (name, expr) in named.iter() {
            walk.expr(
                format!("expressions.named.{name}"),
                StringRole::NamedExpr,
                expr.as_str(),
            );
        }
    }

    for (t, trigger) in strategy.triggers.iter().enumerate() {
        if let crate::ir::TriggerSource::Condition(condition) = &trigger.source {
            let base = format!("triggers[{t}].condition");
            match condition {
                crate::ir::Condition::Leaf(leaf) => walk.leaf(base, leaf),
                crate::ir::Condition::All(group) => {
                    for (l, leaf) in group.all.iter().enumerate() {
                        walk.leaf(format!("{base}.all[{l}]"), leaf);
                    }
                }
                crate::ir::Condition::Any(group) => {
                    for (l, leaf) in group.any.iter().enumerate() {
                        walk.leaf(format!("{base}.any[{l}]"), leaf);
                    }
                }
            }
        }
    }

    for (l, leg) in strategy.legs.iter().enumerate() {
        for (b, block) in leg.body.iter().enumerate() {
            let base = format!("legs[{l}].body[{b}]");
            if let Some(guard) = &block.guard {
                walk.expr(format!("{base}.if"), StringRole::Guard, guard.as_str());
            }
            for (d, item) in block.do_items.iter().enumerate() {
                let item_base = format!("{base}.do[{d}]");
                match item {
                    crate::ir::DoItem::Statement(stmt) => walk.stmt(item_base, stmt.as_str()),
                    crate::ir::DoItem::Place(place_item) => {
                        walk.order_exprs(
                            format!("{item_base}.place"),
                            place_item.place.price.as_ref().map(crate::ir::ExprStr::as_str),
                            place_item.place.size.as_str(),
                            place_item.place.max_size.as_str(),
                        );
                    }
                    crate::ir::DoItem::Cancel(_) => {}
                    crate::ir::DoItem::TargetPosition(target_item) => {
                        let target = &target_item.target_position;
                        walk.order_exprs(
                            format!("{item_base}.target_position"),
                            None,
                            target.size.as_str(),
                            target.max_size.as_str(),
                        );
                    }
                    crate::ir::DoItem::CancelReplace(replace_item) => {
                        let replace = &replace_item.cancel_replace;
                        walk.order_exprs(
                            format!("{item_base}.cancel_replace"),
                            Some(replace.price.as_str()),
                            replace.size.as_str(),
                            replace.max_size.as_str(),
                        );
                    }
                }
            }
        }
    }

    WalkOutcome {
        walked: walk.parsed,
        diagnostics: walk.diagnostics,
    }
}

/// Accumulator: parsed strings and diagnostics, in document order.
struct Walk {
    parsed: Vec<WalkedString>,
    diagnostics: Vec<WalkDiagnostic>,
}

impl Walk {
    fn expr(&mut self, path: String, role: StringRole, src: &str) {
        match parse_expr(src) {
            Ok(ast) => self.parsed.push(WalkedString {
                path,
                role,
                parsed: ParsedString::Expr(ast),
                source: src.to_string(),
            }),
            Err(error) => self.diagnostics.push(WalkDiagnostic { path, error }),
        }
    }

    fn stmt(&mut self, path: String, src: &str) {
        match parse_stmt(src) {
            Ok(ast) => self.parsed.push(WalkedString {
                path,
                role: StringRole::Statement,
                parsed: ParsedString::Stmt(ast),
                source: src.to_string(),
            }),
            Err(error) => self.diagnostics.push(WalkDiagnostic { path, error }),
        }
    }

    fn leaf(&mut self, path: String, leaf: &crate::ir::ConditionLeaf) {
        if let crate::ir::ConditionLeaf::Expr(expr_leaf) = leaf {
            self.expr(
                format!("{path}.expr"),
                StringRole::TriggerExpr,
                expr_leaf.expr.as_str(),
            );
        }
    }

    fn order_exprs(&mut self, base: String, price: Option<&str>, size: &str, max_size: &str) {
        if let Some(price) = price {
            self.expr(format!("{base}.price"), StringRole::Price, price);
        }
        self.expr(format!("{base}.size"), StringRole::Size, size);
        self.expr(format!("{base}.max_size"), StringRole::MaxSize, max_size);
    }
}
