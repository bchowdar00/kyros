//! Token kinds and the token type. The closed vocabulary of the expression grammar —
//! nothing outside expression-grammar.md's alphabet lexes.

use crate::parse::span::Span;

/// The kind of one token. Payload-free except `Duration`, whose milliseconds value is
/// computed at lex time (durations are pure syntax sugar; the parser never sees a unit).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    /// An identifier that is not a keyword (namespace heads and builtin names lex as
    /// `Ident`; their meaning is positional and resolved by the parser).
    Ident,
    /// A canonical decimal literal; the exact source text is the value.
    Decimal,
    /// A duration literal, already lowered to milliseconds.
    Duration(u64),
    /// A single-quoted enum variant, quotes included in the span.
    Variant,

    /// Keyword `and`.
    And,
    /// Keyword `or`.
    Or,
    /// Keyword `not`.
    Not,
    /// Keyword `if`.
    If,
    /// Keyword `then`.
    Then,
    /// Keyword `else`.
    Else,
    /// Keyword `true`.
    True,
    /// Keyword `false`.
    False,
    /// Keyword `set` (statement head).
    Set,
    /// Keyword `push` (statement head).
    Push,

    /// `+`
    Plus,
    /// `-`
    Minus,
    /// `*`
    Star,
    /// `/`
    Slash,
    /// `<`
    Lt,
    /// `<=`
    Le,
    /// `>`
    Gt,
    /// `>=`
    Ge,
    /// `==`
    EqEq,
    /// `!=`
    NotEq,
    /// `=` — legal only in statements.
    Assign,
    /// `(`
    LParen,
    /// `)`
    RParen,
    /// `[`
    LBracket,
    /// `]`
    RBracket,
    /// `,`
    Comma,
    /// `.`
    Dot,
    /// `:` — legal nowhere in the grammar; lexed as a token (rather than a forbidden
    /// character) so diagnostics can name it precisely where it appears.
    Colon,

    /// End of input. Span is `(len, len)` per the convention in `span.rs`.
    Eof,
}

/// One token: a kind and where it sits in the source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token {
    /// What the token is.
    pub kind: TokenKind,
    /// The bytes it covers, half-open.
    pub span: Span,
}

impl Token {
    /// The token's source text.
    pub fn text<'src>(&self, src: &'src str) -> &'src str {
        &src[self.span.start as usize..self.span.end as usize]
    }

    /// A human description for expected/found diagnostics: `` identifier `foo` ``,
    /// `` `and` ``, `end of input`.
    pub fn describe(&self, src: &str) -> String {
        match self.kind {
            TokenKind::Ident => format!("identifier `{}`", self.text(src)),
            TokenKind::Decimal => format!("number `{}`", self.text(src)),
            TokenKind::Duration(_) => format!("duration `{}`", self.text(src)),
            TokenKind::Variant => format!("variant {}", self.text(src)),
            TokenKind::And
            | TokenKind::Or
            | TokenKind::Not
            | TokenKind::If
            | TokenKind::Then
            | TokenKind::Else
            | TokenKind::True
            | TokenKind::False
            | TokenKind::Set
            | TokenKind::Push
            | TokenKind::Plus
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
            | TokenKind::LParen
            | TokenKind::RParen
            | TokenKind::LBracket
            | TokenKind::RBracket
            | TokenKind::Comma
            | TokenKind::Dot
            | TokenKind::Colon => format!("`{}`", self.text(src)),
            TokenKind::Eof => "end of input".to_owned(),
        }
    }
}
