//! Statement productions (§9: `set_stmt`, `push_stmt`, `lvalue`).

use crate::parse::ast::expr::Expr;
use crate::parse::ast::stmt::Stmt;
use crate::parse::error::{ParseError, ParseErrorCode};
use crate::parse::lexer::token::TokenKind;
use crate::parse::span::Spanned;

use super::Parser;

impl Parser<'_> {
    /// Statement dispatch: `set_stmt | push_stmt`.
    pub(super) fn stmt(&mut self) -> Result<Spanned<Stmt>, ParseError> {
        match self.peek().kind {
            TokenKind::Set => self.set_stmt(),
            TokenKind::Push => self.push_stmt(),
            _ => Err(self.unexpected("`set` or `push`")),
        }
    }

    /// §9 `set_stmt = "set" lvalue "=" expr ;`
    fn set_stmt(&mut self) -> Result<Spanned<Stmt>, ParseError> {
        let set_token = self.bump();
        let (slot, index) = self.lvalue(true)?;
        self.expect_assign()?;
        let rhs = self.expr()?;
        let span = set_token.span.join(rhs.span);
        Ok(Spanned::new(Stmt::Set { slot, index, rhs }, span))
    }

    /// §9 `push_stmt = "push" "state" "." ident "=" expr ;` — the lvalue shape minus
    /// indexing: a window is appended as a whole.
    fn push_stmt(&mut self) -> Result<Spanned<Stmt>, ParseError> {
        let push_token = self.bump();
        let (slot, _none) = self.lvalue(false)?;
        self.expect_assign()?;
        let value = self.expr()?;
        let span = push_token.span.join(value.span);
        Ok(Spanned::new(Stmt::Push { slot, value }, span))
    }

    /// §9 `lvalue = "state" "." ident [ "[" expr { "," expr } "]" ] ;` — a state slot
    /// and nothing else. `allow_index` is false for `push` targets.
    #[allow(clippy::type_complexity)]
    fn lvalue(
        &mut self,
        allow_index: bool,
    ) -> Result<(Spanned<Box<str>>, Option<Vec<Spanned<Expr>>>), ParseError> {
        let head = self.peek();
        match head.kind {
            TokenKind::Ident if head.text(self.src) == "state" => {
                self.bump();
            }
            TokenKind::Ident => {
                return Err(self
                    .error(
                        ParseErrorCode::InvalidLvalue,
                        head.span,
                        format!("`{}` cannot be assigned", head.text(self.src)),
                    )
                    .with_help("only state slots can be assigned: set state.<slot> = ..."));
            }
            _ => {
                return Err(self
                    .error(
                        ParseErrorCode::InvalidLvalue,
                        head.span,
                        "assignment targets must be state slots",
                    )
                    .with_help("only state slots can be assigned: set state.<slot> = ..."));
            }
        }
        self.expect(TokenKind::Dot, "`.` after `state`")?;
        let (slot, slot_span) = self.expect_ident("a state slot name")?;

        let mut index = None;
        if matches!(self.peek().kind, TokenKind::LBracket) {
            if !allow_index {
                return Err(self
                    .error(
                        ParseErrorCode::InvalidLvalue,
                        self.peek().span,
                        "push targets cannot be indexed",
                    )
                    .with_help("a window is appended as a whole: push state.<window> = <value>"));
            }
            self.bump();
            let mut keys = vec![self.expr()?];
            while matches!(self.peek().kind, TokenKind::Comma) {
                self.bump();
                keys.push(self.expr()?);
            }
            self.expect(TokenKind::RBracket, "a closing `]`")?;
            index = Some(keys);
        }

        if matches!(self.peek().kind, TokenKind::Dot) {
            return Err(self
                .error(
                    ParseErrorCode::InvalidLvalue,
                    self.peek().span,
                    "assignment targets cannot have nested paths",
                )
                .with_help(
                    "write whole slots: set state.<slot> = ... (field-granular writes are \
                     not part of the grammar)",
                ));
        }

        Ok((Spanned::new(slot, slot_span), index))
    }

    /// Consume the single `=` of a statement, with a targeted help when `==` was
    /// written instead.
    fn expect_assign(&mut self) -> Result<(), ParseError> {
        match self.peek().kind {
            TokenKind::Assign => {
                self.bump();
                Ok(())
            }
            TokenKind::EqEq => Err(self
                .unexpected("`=`")
                .with_help("statements assign with a single `=`; `==` is comparison")),
            _ => Err(self.unexpected("`=`")),
        }
    }
}
