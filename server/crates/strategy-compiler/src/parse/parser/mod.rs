//! The recursive-descent parser: token stream to AST.
//!
//! One function per §9 EBNF production, named after it, so the grammar doc can be held
//! beside this module and checked line by line. Single-token lookahead throughout;
//! non-associativity of the comparison layers is structural — each production consumes
//! at most one operator and diagnoses a second.
//!
//! There is ONE expression grammar. Trigger leaf expressions (v0.3) are body
//! expressions restricted by the validator (`validate::context::trigger_expr`), not a
//! sub-grammar; the v0.2 leaf-expr dialect with bare field names is gone.

pub mod expr;
pub mod stmt;

use crate::parse::ast::expr::Expr;
use crate::parse::ast::stmt::Stmt;
use crate::parse::error::{ParseError, ParseErrorCode};
use crate::parse::lexer;
use crate::parse::lexer::token::{Token, TokenKind};
use crate::parse::span::{Span, Spanned};

/// Parse one expression string to a spanned AST, or the first error.
pub fn parse_expr(src: &str) -> Result<Spanned<Expr>, ParseError> {
    let mut parser = Parser::new(src)?;
    parser.check_nonempty("expression")?;
    let parsed = parser.expr()?;
    parser.expect_end()?;
    Ok(parsed)
}

/// Parse one statement string (`set ...` or `push ...`) to a spanned AST, or the first
/// error.
pub fn parse_stmt(src: &str) -> Result<Spanned<Stmt>, ParseError> {
    let mut parser = Parser::new(src)?;
    parser.check_nonempty("statement")?;
    let parsed = parser.stmt()?;
    parser.expect_end()?;
    Ok(parsed)
}

/// Parser state: the source, its token stream (always Eof-terminated), and a cursor.
pub(super) struct Parser<'src> {
    pub(super) src: &'src str,
    tokens: Vec<Token>,
    pos: usize,
}

impl<'src> Parser<'src> {
    fn new(src: &'src str) -> Result<Self, ParseError> {
        Ok(Parser {
            src,
            tokens: lexer::lex(src)?,
            pos: 0,
        })
    }

    fn check_nonempty(&self, what: &str) -> Result<(), ParseError> {
        if matches!(self.peek().kind, TokenKind::Eof) {
            Err(ParseError::new(
                ParseErrorCode::EmptyExpression,
                Span::point(0),
                format!("empty {what}"),
                self.src,
            ))
        } else {
            Ok(())
        }
    }

    /// The current token. The stream is Eof-terminated and the cursor never passes
    /// Eof, so this cannot go out of bounds.
    pub(super) fn peek(&self) -> Token {
        self.tokens[self.pos]
    }

    /// Consume and return the current token; the cursor stops at Eof.
    pub(super) fn bump(&mut self) -> Token {
        let token = self.tokens[self.pos];
        if !matches!(token.kind, TokenKind::Eof) {
            self.pos += 1;
        }
        token
    }

    /// Build an error carrying this parser's source.
    pub(super) fn error(
        &self,
        code: ParseErrorCode,
        span: Span,
        message: impl Into<String>,
    ) -> ParseError {
        ParseError::new(code, span, message, self.src)
    }

    /// An unexpected-token (or unexpected-EOF) error at the current token, with
    /// expected/found filled.
    pub(super) fn unexpected(&self, expected: &str) -> ParseError {
        let token = self.peek();
        let found = token.describe(self.src);
        let (code, message) = if matches!(token.kind, TokenKind::Eof) {
            (
                ParseErrorCode::UnexpectedEof,
                "expression ended unexpectedly".to_owned(),
            )
        } else {
            (
                ParseErrorCode::UnexpectedToken,
                format!("expected {expected}"),
            )
        };
        ParseError::new(code, token.span, message, self.src).with_expected_found(expected, found)
    }

    /// Consume a token of exactly `kind`, or fail with `expected` in the diagnostic.
    pub(super) fn expect(&mut self, kind: TokenKind, expected: &str) -> Result<Token, ParseError> {
        if self.peek().kind == kind {
            Ok(self.bump())
        } else {
            Err(self.unexpected(expected))
        }
    }

    /// Consume an identifier token, returning its text and span.
    pub(super) fn expect_ident(&mut self, expected: &str) -> Result<(Box<str>, Span), ParseError> {
        if matches!(self.peek().kind, TokenKind::Ident) {
            let token = self.bump();
            Ok((token.text(self.src).into(), token.span))
        } else {
            Err(self.unexpected(expected))
        }
    }

    fn expect_end(&self) -> Result<(), ParseError> {
        let token = self.peek();
        if matches!(token.kind, TokenKind::Eof) {
            Ok(())
        } else {
            Err(self
                .error(
                    ParseErrorCode::TrailingInput,
                    token.span,
                    "unexpected input after a complete expression",
                )
                .with_expected_found("end of input", token.describe(self.src)))
        }
    }
}
