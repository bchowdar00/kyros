//! The expression parser: strings to typed AST plus structured diagnostics.
//!
//! This module is the ONLY string-to-AST boundary in the system. Nothing outside
//! `parse` may interpret an expression string; every downstream stage (validator,
//! lowerer, decompiler, interpreter, hasher) consumes the AST produced here.
//!
//! Implements expression-grammar.md, with the §9 EBNF as the authority. One grammar:
//! trigger leaf expressions are body expressions the validator restricts. Scope is
//! strictly syntax: context restrictions the grammar cannot express (namespace
//! placement rules, guard-position rules, `event.*` pinning) are deferred to the
//! validator — see the handoff list in the Phase 2 final report.

pub mod ast;
pub mod builtins;
pub mod error;
pub mod lexer;
pub mod parser;
pub mod reserved;
pub mod span;
pub mod walk;

pub use ast::expr::Expr;
pub use ast::stmt::Stmt;
pub use error::{ParseError, ParseErrorCode};
pub use parser::{parse_expr, parse_stmt};
pub use span::{Span, Spanned};
pub use walk::{
    ParsedString, StringRole, WalkDiagnostic, WalkOutcome, WalkedString, walk_strategy,
};
