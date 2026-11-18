//! Expression nodes.
//!
//! Decimal literals preserve their exact written form (`Box<str>`) — written form is
//! canonical form and feeds content hashing; no float type appears anywhere. Durations
//! were lowered to milliseconds by the lexer; the AST has no duration-token concept.

use crate::parse::builtins::Builtin;
use crate::parse::span::{Span, Spanned};

/// An expression node. Operator categories are separate variants (arithmetic,
/// comparison, logic) so downstream typing can match exhaustively per category instead
/// of re-dispatching on a shared operator enum.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// A literal value.
    Literal(Literal),
    /// A namespaced reference, with any field path and index folded in.
    Ref(Ref),
    /// Unary minus or `not`. Single application by construction (§9: `unary = [ "-" |
    /// "not" ] postfix`) — `- -1` and `not not a` do not parse.
    Unary {
        /// The operator.
        op: UnaryOp,
        /// The operand.
        operand: Box<Spanned<Expr>>,
    },
    /// `+ - * /`.
    Arith {
        /// The operator.
        op: ArithOp,
        /// Left operand.
        lhs: Box<Spanned<Expr>>,
        /// Right operand.
        rhs: Box<Spanned<Expr>>,
    },
    /// `< <= > >= == !=`. Non-associative: chains are parse errors, so a `Cmp` never
    /// has a `Cmp` of the same layer as a direct operand from chaining.
    Cmp {
        /// The comparator.
        op: CmpOp,
        /// Left operand.
        lhs: Box<Spanned<Expr>>,
        /// Right operand.
        rhs: Box<Spanned<Expr>>,
    },
    /// `and` / `or`.
    Logic {
        /// The connective.
        op: LogicOp,
        /// Left operand.
        lhs: Box<Spanned<Expr>>,
        /// Right operand.
        rhs: Box<Spanned<Expr>>,
    },
    /// `if c then v` arms (else-if chains flattened by the parser) plus the mandatory
    /// final `else`.
    If {
        /// The condition/value arms, in source order.
        arms: Vec<IfArm>,
        /// The mandatory `else` value.
        else_value: Box<Spanned<Expr>>,
    },
    /// A builtin call. Arity was checked at parse time.
    Call {
        /// Which builtin.
        builtin: Builtin,
        /// The span of the function name alone (diagnostics anchor).
        name_span: Span,
        /// The arguments, in source order.
        args: Vec<Spanned<Expr>>,
    },
    /// `triggered(NAME)` — the one cause primitive. Its argument is a BARE trigger name,
    /// not an expression (the ratified carve-out from no-bare-identifiers), so it gets a
    /// dedicated node instead of riding through `Call`'s expression arguments. The name
    /// resolves against the document's triggers section in the validator.
    Triggered {
        /// The bare trigger name.
        name: Spanned<Box<str>>,
    },
}

/// A literal value.
#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    /// A canonical decimal, exact source text preserved. Never a float.
    Decimal(Box<str>),
    /// `true` / `false`.
    Bool(bool),
    /// A duration literal, already lowered to milliseconds by the lexer.
    DurationMs(u64),
    /// A single-quoted enum variant, quotes stripped.
    Variant(Box<str>),
}

/// A namespaced reference: `signal.book.ask`, `state.miss_count[event.team, 2]`.
#[derive(Debug, Clone, PartialEq)]
pub struct Ref {
    /// The namespace head.
    pub namespace: Namespace,
    /// Path segments after the namespace, at least one. `signal.book.ask` has path
    /// `["book", "ask"]`.
    pub path: Vec<Spanned<Box<str>>>,
    /// Index expressions, when the reference is indexed (`state.m[k1, k2]`). Where
    /// indexing is *meaningful* (map-typed state slots) is the validator's concern.
    pub index: Option<Vec<Spanned<Expr>>>,
}

/// The ten namespace heads (v0.2: `timer` died with the timers section — timers are
/// triggers now; v0.3 added `param`). Where each is *legal* (e.g. `entry` only inside reducer arguments) is
/// the validator's concern; the parser records what was written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Namespace {
    /// Fields of the triggering event.
    Event,
    /// The activation payload snapshot.
    Signal,
    /// The strategy's declared slots.
    State,
    /// Harness-written platform state.
    Platform,
    /// Prior-leg outputs.
    Legs,
    /// Named expressions.
    Expr,
    /// Constant tables.
    Table,
    /// The element under a window reducer.
    Entry,
    /// The deployment's bankroll document.
    Envelope,
    /// Deploy-time knobs, set by the user and changeable on a running strategy.
    Param,
}

impl Namespace {
    /// Resolve a namespace head from its source spelling.
    pub fn from_head(head: &str) -> Option<Namespace> {
        match head {
            "event" => Some(Namespace::Event),
            "signal" => Some(Namespace::Signal),
            "state" => Some(Namespace::State),
            "platform" => Some(Namespace::Platform),
            "legs" => Some(Namespace::Legs),
            "expr" => Some(Namespace::Expr),
            "table" => Some(Namespace::Table),
            "entry" => Some(Namespace::Entry),
            "envelope" => Some(Namespace::Envelope),
            "param" => Some(Namespace::Param),
            _ => None,
        }
    }

    /// The source spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Namespace::Event => "event",
            Namespace::Signal => "signal",
            Namespace::State => "state",
            Namespace::Platform => "platform",
            Namespace::Legs => "legs",
            Namespace::Expr => "expr",
            Namespace::Table => "table",
            Namespace::Entry => "entry",
            Namespace::Envelope => "envelope",
            Namespace::Param => "param",
        }
    }
}

/// One `if c then v` arm of a conditional.
#[derive(Debug, Clone, PartialEq)]
pub struct IfArm {
    /// The boolean condition.
    pub condition: Spanned<Expr>,
    /// The value when the condition holds.
    pub value: Spanned<Expr>,
}

/// Unary operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    /// Numeric negation (`-`).
    Neg,
    /// Boolean negation (`not`).
    Not,
}

/// Arithmetic operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArithOp {
    /// `+`
    Add,
    /// `-`
    Sub,
    /// `*`
    Mul,
    /// `/` (totality policy for division by zero is pinned in the AST spec).
    Div,
}

/// Comparison operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    /// `<`
    Lt,
    /// `<=`
    Le,
    /// `>`
    Gt,
    /// `>=`
    Ge,
    /// `==`
    Eq,
    /// `!=`
    Ne,
}

/// Boolean connectives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogicOp {
    /// `and`
    And,
    /// `or`
    Or,
}
