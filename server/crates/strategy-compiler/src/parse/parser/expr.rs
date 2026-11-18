//! Expression productions (§9: `expr` through `primary`, `conditional`, `call`, `ref`).

use crate::parse::ast::expr::{
    ArithOp, CmpOp, Expr, IfArm, Literal, LogicOp, Namespace, Ref, UnaryOp,
};
use crate::parse::builtins::{self, Builtin};
use crate::parse::error::{ParseError, ParseErrorCode};
use crate::parse::lexer::token::{Token, TokenKind};
use crate::parse::span::Spanned;

use super::Parser;

impl Parser<'_> {
    /// §9 `expr = or_expr ;`
    pub(super) fn expr(&mut self) -> Result<Spanned<Expr>, ParseError> {
        self.or_expr()
    }

    /// §9 `or_expr = and_expr { "or" and_expr } ;`
    fn or_expr(&mut self) -> Result<Spanned<Expr>, ParseError> {
        let mut lhs = self.and_expr()?;
        while matches!(self.peek().kind, TokenKind::Or) {
            self.bump();
            let rhs = self.and_expr()?;
            let span = lhs.span.join(rhs.span);
            lhs = Spanned::new(
                Expr::Logic {
                    op: LogicOp::Or,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                },
                span,
            );
        }
        Ok(lhs)
    }

    /// §9 `and_expr = eq_expr { "and" eq_expr } ;`
    fn and_expr(&mut self) -> Result<Spanned<Expr>, ParseError> {
        let mut lhs = self.eq_expr()?;
        while matches!(self.peek().kind, TokenKind::And) {
            self.bump();
            let rhs = self.eq_expr()?;
            let span = lhs.span.join(rhs.span);
            lhs = Spanned::new(
                Expr::Logic {
                    op: LogicOp::And,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                },
                span,
            );
        }
        Ok(lhs)
    }

    /// §9 `eq_expr = cmp_expr [ ( "==" | "!=" ) cmp_expr ] ;` — non-associative: the
    /// production consumes at most one operator and diagnoses a second.
    fn eq_expr(&mut self) -> Result<Spanned<Expr>, ParseError> {
        let lhs = self.cmp_expr()?;
        let op = match self.peek().kind {
            TokenKind::EqEq => CmpOp::Eq,
            TokenKind::NotEq => CmpOp::Ne,
            _ => return Ok(lhs),
        };
        self.bump();
        let rhs = self.cmp_expr()?;
        if matches!(self.peek().kind, TokenKind::EqEq | TokenKind::NotEq) {
            return Err(self.chained_comparison());
        }
        let span = lhs.span.join(rhs.span);
        Ok(Spanned::new(
            Expr::Cmp {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            },
            span,
        ))
    }

    /// §9 `cmp_expr = add_expr [ ( "<" | "<=" | ">" | ">=" ) add_expr ] ;` —
    /// non-associative, same structure as `eq_expr`.
    fn cmp_expr(&mut self) -> Result<Spanned<Expr>, ParseError> {
        let lhs = self.add_expr()?;
        let op = match self.peek().kind {
            TokenKind::Lt => CmpOp::Lt,
            TokenKind::Le => CmpOp::Le,
            TokenKind::Gt => CmpOp::Gt,
            TokenKind::Ge => CmpOp::Ge,
            _ => return Ok(lhs),
        };
        self.bump();
        let rhs = self.add_expr()?;
        if matches!(
            self.peek().kind,
            TokenKind::Lt | TokenKind::Le | TokenKind::Gt | TokenKind::Ge
        ) {
            return Err(self.chained_comparison());
        }
        let span = lhs.span.join(rhs.span);
        Ok(Spanned::new(
            Expr::Cmp {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            },
            span,
        ))
    }

    fn chained_comparison(&self) -> ParseError {
        self.error(
            ParseErrorCode::ChainedComparison,
            self.peek().span,
            "comparison operators cannot be chained",
        )
        .with_help("split into two comparisons joined with 'and'")
    }

    /// §9 `add_expr = mul_expr { ( "+" | "-" ) mul_expr } ;`
    fn add_expr(&mut self) -> Result<Spanned<Expr>, ParseError> {
        let mut lhs = self.mul_expr()?;
        loop {
            let op = match self.peek().kind {
                TokenKind::Plus => ArithOp::Add,
                TokenKind::Minus => ArithOp::Sub,
                _ => return Ok(lhs),
            };
            self.bump();
            let rhs = self.mul_expr()?;
            let span = lhs.span.join(rhs.span);
            lhs = Spanned::new(
                Expr::Arith {
                    op,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                },
                span,
            );
        }
    }

    /// §9 `mul_expr = unary { ( "*" | "/" ) unary } ;`
    fn mul_expr(&mut self) -> Result<Spanned<Expr>, ParseError> {
        let mut lhs = self.unary()?;
        loop {
            let op = match self.peek().kind {
                TokenKind::Star => ArithOp::Mul,
                TokenKind::Slash => ArithOp::Div,
                _ => return Ok(lhs),
            };
            self.bump();
            let rhs = self.unary()?;
            let span = lhs.span.join(rhs.span);
            lhs = Spanned::new(
                Expr::Arith {
                    op,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                },
                span,
            );
        }
    }

    /// §9 `unary = [ "-" | "not" ] postfix ;` — single application by construction:
    /// the operand is `postfix`, not `unary`, so `- -1` and `not not a` fail to parse.
    fn unary(&mut self) -> Result<Spanned<Expr>, ParseError> {
        let op = match self.peek().kind {
            TokenKind::Minus => UnaryOp::Neg,
            TokenKind::Not => UnaryOp::Not,
            _ => return self.postfix(),
        };
        let op_token = self.bump();
        let operand = self.postfix()?;
        let span = op_token.span.join(operand.span);
        Ok(Spanned::new(
            Expr::Unary {
                op,
                operand: Box::new(operand),
            },
            span,
        ))
    }

    /// §9 `postfix = primary { "[" expr { "," expr } "]" | "." ident } ;`
    ///
    /// Field access and indexing fold into the base `Ref` node (the ratified AST
    /// shape). The EBNF permits postfix after *any* primary for parser simplicity, but
    /// the AST cannot represent (and the validator would always reject) postfix on
    /// non-references — those are diagnosed here. Recorded as a DEVIATION.
    fn postfix(&mut self) -> Result<Spanned<Expr>, ParseError> {
        let mut base = self.primary()?;
        loop {
            match self.peek().kind {
                TokenKind::Dot => {
                    let dot = self.bump();
                    let (segment, segment_span) = self.expect_ident("a field name after `.`")?;
                    match &mut base.node {
                        Expr::Ref(reference) if reference.index.is_none() => {
                            reference.path.push(Spanned::new(segment, segment_span));
                            base.span = base.span.join(segment_span);
                        }
                        Expr::Ref(_) => {
                            return Err(self
                                .error(
                                    ParseErrorCode::UnexpectedToken,
                                    dot.span,
                                    "field access after indexing is not supported",
                                )
                                .with_help("index the slot last: state.<slot>[keys]"));
                        }
                        _ => {
                            return Err(self.error(
                                ParseErrorCode::UnexpectedToken,
                                dot.span,
                                "field access is only valid on namespaced references",
                            ));
                        }
                    }
                }
                TokenKind::LBracket => {
                    let open = self.bump();
                    let mut keys = vec![self.expr()?];
                    while matches!(self.peek().kind, TokenKind::Comma) {
                        self.bump();
                        keys.push(self.expr()?);
                    }
                    let close = self.expect(TokenKind::RBracket, "a closing `]`")?;
                    match &mut base.node {
                        Expr::Ref(reference) if reference.index.is_none() => {
                            reference.index = Some(keys);
                            base.span = base.span.join(close.span);
                        }
                        Expr::Ref(_) => {
                            return Err(self.error(
                                ParseErrorCode::UnexpectedToken,
                                open.span,
                                "this reference is already indexed",
                            ));
                        }
                        _ => {
                            return Err(self.error(
                                ParseErrorCode::UnexpectedToken,
                                open.span,
                                "indexing is only valid on state references",
                            ));
                        }
                    }
                }
                _ => return Ok(base),
            }
        }
    }

    /// §9 `primary = literal | ref | call | conditional | "(" expr ")" ;`
    fn primary(&mut self) -> Result<Spanned<Expr>, ParseError> {
        let token = self.peek();
        match token.kind {
            TokenKind::Decimal => {
                self.bump();
                Ok(Spanned::new(
                    Expr::Literal(Literal::Decimal(token.text(self.src).into())),
                    token.span,
                ))
            }
            TokenKind::Duration(ms) => {
                self.bump();
                Ok(Spanned::new(
                    Expr::Literal(Literal::DurationMs(ms)),
                    token.span,
                ))
            }
            TokenKind::True => {
                self.bump();
                Ok(Spanned::new(Expr::Literal(Literal::Bool(true)), token.span))
            }
            TokenKind::False => {
                self.bump();
                Ok(Spanned::new(
                    Expr::Literal(Literal::Bool(false)),
                    token.span,
                ))
            }
            TokenKind::Variant => {
                self.bump();
                let inner = &self.src[token.span.start as usize + 1..token.span.end as usize - 1];
                Ok(Spanned::new(
                    Expr::Literal(Literal::Variant(inner.into())),
                    token.span,
                ))
            }
            TokenKind::If => self.conditional(),
            TokenKind::LParen => {
                let open = self.bump();
                let inner = self.expr()?;
                let close = self.expect(TokenKind::RParen, "a closing `)`")?;
                Ok(Spanned::new(inner.node, open.span.join(close.span)))
            }
            TokenKind::Ident => self.call_or_ref(),
            TokenKind::Plus => Err(self
                .error(
                    ParseErrorCode::NonCanonicalDecimal,
                    token.span,
                    "decimals cannot have a leading `+`",
                )
                .with_help("write `5`, not `+5`; negative values use unary minus")),
            TokenKind::And
            | TokenKind::Or
            | TokenKind::Not
            | TokenKind::Then
            | TokenKind::Else
            | TokenKind::Set
            | TokenKind::Push
            | TokenKind::Minus
            | TokenKind::Star
            | TokenKind::Slash
            | TokenKind::Lt
            | TokenKind::Le
            | TokenKind::Gt
            | TokenKind::Ge
            | TokenKind::EqEq
            | TokenKind::NotEq
            | TokenKind::Assign
            | TokenKind::RParen
            | TokenKind::LBracket
            | TokenKind::RBracket
            | TokenKind::Comma
            | TokenKind::Dot
            | TokenKind::Colon
            | TokenKind::Eof => Err(self.unexpected("a value")),
        }
    }

    /// Identifier dispatch: `name(` is a call, `name.` is a reference, anything else
    /// is the bare-identifier error.
    fn call_or_ref(&mut self) -> Result<Spanned<Expr>, ParseError> {
        let name_token = self.bump();
        match self.peek().kind {
            TokenKind::LParen => self.call(name_token),
            TokenKind::Dot => self.reference(name_token),
            _ => {
                let name = name_token.text(self.src);
                Err(self
                    .error(
                        ParseErrorCode::BareIdentifier,
                        name_token.span,
                        format!("`{name}` is not a reference — bare identifiers do not exist"),
                    )
                    .with_help(
                        "references must start with a namespace: event., signal., state., \
                         platform., legs., expr., table., entry., envelope., or param.",
                    ))
            }
        }
    }

    /// §9 `ref = namespace "." ident ;` — deeper fields and indexing attach in
    /// `postfix`.
    fn reference(&mut self, head: Token) -> Result<Spanned<Expr>, ParseError> {
        let head_text = head.text(self.src);
        if head_text == "timer" {
            // v0.1 steering: the timer namespace died with the timers section.
            return Err(self
                .error(
                    ParseErrorCode::SupersededTimerNamespace,
                    head.span,
                    "the `timer.` namespace was removed in v0.2 — timers are triggers now",
                )
                .with_help(
                    "declare a timer trigger in the triggers section and gate the block \
                     with triggered(<trigger name>)",
                ));
        }
        let Some(namespace) = Namespace::from_head(head_text) else {
            return Err(self
                .error(
                    ParseErrorCode::UnknownNamespace,
                    head.span,
                    format!("`{head_text}` is not a namespace"),
                )
                .with_help(
                    "references start with one of: event., signal., state., platform., \
                     legs., expr., table., entry., envelope., param.",
                ));
        };
        self.bump(); // the dot
        let (segment, segment_span) = self.expect_ident("an identifier after `.`")?;
        let span = head.span.join(segment_span);
        Ok(Spanned::new(
            Expr::Ref(Ref {
                namespace,
                path: vec![Spanned::new(segment, segment_span)],
                index: None,
            }),
            span,
        ))
    }

    /// §9 `call = builtin_name "(" [ expr { "," expr } ] ")" ;` — closed set, fixed
    /// arity, plus the two parser-enforced argument syntax rules (`lookup`'s `table.`
    /// first argument and `triggered`'s bare trigger name).
    fn call(&mut self, name_token: Token) -> Result<Spanned<Expr>, ParseError> {
        let name = name_token.text(self.src);
        let Some(info) = builtins::find(name) else {
            // v0.1 steering first: the two dead builtins are the highest-probability
            // LLM mistakes in the new vocabulary and must never read as unknown names.
            if name == "crossed" {
                return Err(self
                    .error(
                        ParseErrorCode::SupersededCrossed,
                        name_token.span,
                        "`crossed()` was removed in v0.2 — edge detection lives in \
                         trigger fire/rearm modes",
                    )
                    .with_help(
                        "declare a trigger with fire: on_crossing, rearm: fresh_crossing \
                         and gate the block with triggered(<trigger name>)",
                    ));
            }
            if name == "fired" {
                return Err(self
                    .error(
                        ParseErrorCode::SupersededFired,
                        name_token.span,
                        "`fired()` was removed in v0.2 — cause dispatch is `triggered(NAME)`",
                    )
                    .with_help(
                        "write triggered(<trigger name>) with the bare name of a trigger \
                         declared in the triggers section",
                    ));
            }
            let mut error = self.error(
                ParseErrorCode::UnknownFunction,
                name_token.span,
                format!("`{name}` is not a builtin function"),
            );
            if let Some(suggestion) = builtins::suggest(name) {
                error = error.with_help(format!("did you mean `{suggestion}`?"));
            }
            return Err(error);
        };
        if info.builtin == Builtin::Triggered {
            return self.triggered_call(name_token);
        }
        self.bump(); // (
        let mut args: Vec<Spanned<Expr>> = Vec::new();
        if !matches!(self.peek().kind, TokenKind::RParen) {
            loop {
                args.push(self.expr()?);
                if matches!(self.peek().kind, TokenKind::Comma) {
                    self.bump();
                } else {
                    break;
                }
            }
        }
        let close = self.expect(TokenKind::RParen, "a closing `)` or `,`")?;
        let call_span = name_token.span.join(close.span);

        if args.len() != info.arity {
            let plural = if info.arity == 1 { "" } else { "s" };
            let mut error = self.error(
                ParseErrorCode::WrongArity,
                call_span,
                format!(
                    "`{name}` expects {} argument{plural}, found {}",
                    info.arity,
                    args.len()
                ),
            );
            if matches!(info.builtin, Builtin::Min | Builtin::Max) && args.len() > 2 {
                error = error
                    .with_help("min and max are strictly binary; nest for more: min(a, min(b, c))");
            }
            return Err(error);
        }

        // The parser-enforced first-argument syntax rule for `lookup`. All other
        // namespace placement rules are validator concerns.
        if info.builtin == Builtin::Lookup {
            let first = &args[0];
            let is_table_ref =
                matches!(&first.node, Expr::Ref(r) if r.namespace == Namespace::Table);
            if !is_table_ref {
                return Err(self
                    .error(
                        ParseErrorCode::TableRefOutsideLookup,
                        first.span,
                        "the first argument of `lookup` must be a table reference",
                    )
                    .with_help("write `lookup(table.<name>, key)`"));
            }
        }

        Ok(Spanned::new(
            Expr::Call {
                builtin: info.builtin,
                name_span: name_token.span,
                args,
            },
            call_span,
        ))
    }

    /// `triggered(NAME)` — the argument is a single BARE trigger name, not an
    /// expression (the ratified carve-out from no-bare-identifiers). Anything else in
    /// argument position gets the dedicated steering diagnostic; the name resolves
    /// against the triggers section in the validator.
    fn triggered_call(&mut self, name_token: Token) -> Result<Spanned<Expr>, ParseError> {
        self.bump(); // (
        let bad_arg = |parser: &Parser<'_>, span| {
            parser
                .error(
                    ParseErrorCode::TriggeredArgNotBareName,
                    span,
                    "`triggered` takes a single bare trigger name",
                )
                .with_help(
                    "write triggered(<trigger name>) — the name is declared in the \
                     triggers section; namespaced references are not trigger names",
                )
        };
        if !matches!(self.peek().kind, TokenKind::Ident) {
            return Err(bad_arg(self, self.peek().span));
        }
        let ident_token = self.bump();
        let trigger_name: Box<str> = ident_token.text(self.src).into();
        if !matches!(self.peek().kind, TokenKind::RParen) {
            // A `.` (namespaced ref), `,` (extra arguments), or anything else.
            return Err(bad_arg(self, self.peek().span));
        }
        let close = self.bump();
        Ok(Spanned::new(
            Expr::Triggered {
                name: Spanned::new(trigger_name, ident_token.span),
            },
            name_token.span.join(close.span),
        ))
    }

    /// §9 `conditional = "if" expr "then" expr "else" expr ;` — else-if chains flatten
    /// into one arms list; the final `else` is mandatory.
    fn conditional(&mut self) -> Result<Spanned<Expr>, ParseError> {
        let if_token = self.bump();
        let mut arms = Vec::new();
        loop {
            let condition = self.expr()?;
            self.expect(TokenKind::Then, "`then` after the condition")?;
            let value = self.expr()?;
            arms.push(IfArm { condition, value });
            if !matches!(self.peek().kind, TokenKind::Else) {
                return Err(self
                    .error(
                        ParseErrorCode::MissingElse,
                        self.peek().span,
                        "this `if` expression is missing its `else` branch",
                    )
                    .with_help(
                        "'if ... then ...' expressions require an 'else' branch — every \
                         expression must produce a value",
                    ));
            }
            self.bump(); // else
            if matches!(self.peek().kind, TokenKind::If) {
                self.bump();
                continue;
            }
            let else_value = self.expr()?;
            let span = if_token.span.join(else_value.span);
            return Ok(Spanned::new(
                Expr::If {
                    arms,
                    else_value: Box::new(else_value),
                },
                span,
            ));
        }
    }
}
