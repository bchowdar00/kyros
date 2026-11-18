//! Statement nodes: `set` and `push`.

use crate::parse::ast::expr::Expr;
use crate::parse::span::Spanned;

/// A `do`-list statement. The lvalue grammar is deliberately tiny: a `state` slot,
/// optionally indexed for `set`, never indexed for `push`.
#[derive(Debug, Clone, PartialEq)]
pub enum Stmt {
    /// `set state.<slot>[keys] = <expr>`. The RHS reads the pre-assignment value.
    Set {
        /// The state slot name (the segment after `state.`).
        slot: Spanned<Box<str>>,
        /// Index key expressions, when the slot is map-typed.
        index: Option<Vec<Spanned<Expr>>>,
        /// The value expression.
        rhs: Spanned<Expr>,
    },
    /// `push state.<window> = <expr>`. Appends to a window-typed slot; eviction follows
    /// the slot's declared rule.
    Push {
        /// The window slot name.
        slot: Spanned<Box<str>>,
        /// The appended value expression.
        value: Spanned<Expr>,
    },
}
