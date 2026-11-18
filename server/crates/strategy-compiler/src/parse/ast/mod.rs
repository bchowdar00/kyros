//! The expression and statement AST: what the parser produces and everything downstream
//! (validator, lowerer, decompiler, interpreter, hasher) consumes.
//!
//! Every node is spanned. Decimal literals preserve their exact written form — the
//! written form is the canonical form and feeds content hashing; no float type appears
//! anywhere in this module tree.

pub mod expr;
pub mod stmt;
