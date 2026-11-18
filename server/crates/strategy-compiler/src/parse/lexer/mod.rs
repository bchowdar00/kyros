//! The lexer: one expression string to a token stream.
//!
//! Single pass, no backtracking. Enforces the grammar's alphabet at the character level:
//! `"`, `\`, and newlines are not tokens-that-fail-later, they are rejected here with
//! dedicated codes, preserving the zero-escaping JSON embedding property. Duration
//! literals are lowered to milliseconds here (integer arithmetic only; overflow is a lex
//! error, never a panic). Canonical-decimal rejections (`1e5`, `.5`, `5.`, `05`,
//! `1_000`) also happen here, where the offending characters are visible.

pub mod token;

use self::token::{Token, TokenKind};
use crate::parse::error::{ParseError, ParseErrorCode};
use crate::parse::span::Span;

/// Lex a full expression string. Returns the token stream (always terminated by an
/// [`TokenKind::Eof`] token spanning `(len, len)`) or the first lexical error.
pub fn lex(src: &str) -> Result<Vec<Token>, ParseError> {
    let mut lexer = Lexer {
        src,
        bytes: src.as_bytes(),
        pos: 0,
        tokens: Vec::new(),
    };
    lexer.run()?;
    Ok(lexer.tokens)
}

struct Lexer<'src> {
    src: &'src str,
    bytes: &'src [u8],
    pos: usize,
    tokens: Vec<Token>,
}

impl Lexer<'_> {
    fn run(&mut self) -> Result<(), ParseError> {
        while self.pos < self.bytes.len() {
            let b = self.bytes[self.pos];
            if !b.is_ascii() {
                return Err(self
                    .err_at(
                        ParseErrorCode::NonAsciiCharacter,
                        Span::new(self.pos as u32, self.pos as u32 + 1),
                        "expressions are ASCII-only",
                    )
                    .with_help("replace the non-ASCII character; identifiers and variants use [a-zA-Z0-9_]"));
            }
            match b {
                b' ' | b'\t' => self.pos += 1,
                b'"' | b'\\' | b'\n' | b'\r' => return Err(self.forbidden(b)),
                b'a'..=b'z' | b'A'..=b'Z' => self.word(),
                b'0'..=b'9' => self.number()?,
                b'\'' => self.variant()?,
                _ => self.punct()?,
            }
        }
        let end = self.bytes.len() as u32;
        self.tokens.push(Token {
            kind: TokenKind::Eof,
            span: Span::point(end),
        });
        Ok(())
    }

    fn is_ident_byte(b: u8) -> bool {
        b.is_ascii_alphanumeric() || b == b'_'
    }

    fn peek_byte(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn peek_byte_at(&self, offset: usize) -> Option<u8> {
        self.bytes.get(self.pos + offset).copied()
    }

    fn push(&mut self, kind: TokenKind, start: usize) {
        self.tokens.push(Token {
            kind,
            span: Span::new(start as u32, self.pos as u32),
        });
    }

    fn err_at(&self, code: ParseErrorCode, span: Span, message: impl Into<String>) -> ParseError {
        ParseError::new(code, span, message, self.src)
    }

    fn forbidden(&self, b: u8) -> ParseError {
        let what = match b {
            b'"' => "a double quote",
            b'\\' => "a backslash",
            _ => "a newline",
        };
        self.err_at(
            ParseErrorCode::ForbiddenCharacter,
            Span::new(self.pos as u32, self.pos as u32 + 1),
            format!("{what} cannot appear in an expression"),
        )
        .with_help(
            "double quotes, backslashes, and newlines are outside the grammar's alphabet; \
             enum variants use single quotes ('missed') and expressions are single-line",
        )
    }

    fn word(&mut self) {
        let start = self.pos;
        while self.peek_byte().is_some_and(Self::is_ident_byte) {
            self.pos += 1;
        }
        let kind = keyword_kind(&self.src[start..self.pos]).unwrap_or(TokenKind::Ident);
        self.push(kind, start);
    }

    /// Numbers: canonical decimals and duration literals. All the canonical-form
    /// rejections live here because this is where the characters are visible.
    fn number(&mut self) -> Result<(), ParseError> {
        let start = self.pos;
        while self.peek_byte().is_some_and(|b| b.is_ascii_digit()) {
            self.pos += 1;
        }
        let int_end = self.pos;
        let mut has_fraction = false;
        if self.peek_byte() == Some(b'.') {
            if self.peek_byte_at(1).is_some_and(|b| b.is_ascii_digit()) {
                self.pos += 1;
                while self.peek_byte().is_some_and(|b| b.is_ascii_digit()) {
                    self.pos += 1;
                }
                has_fraction = true;
            } else {
                self.pos += 1; // include the dot in the span
                return Err(self
                    .err_at(
                        ParseErrorCode::NonCanonicalDecimal,
                        Span::new(start as u32, self.pos as u32),
                        "decimals cannot end with a dot",
                    )
                    .with_help("write `5`, not `5.`"));
            }
        }

        // A letter tail makes this either a duration literal or a malformed number.
        if self.peek_byte().is_some_and(Self::is_ident_byte) {
            let unit_start = self.pos;
            while self.peek_byte().is_some_and(Self::is_ident_byte) {
                self.pos += 1;
            }
            let span = Span::new(start as u32, self.pos as u32);
            let multiplier: u64 = match &self.src[unit_start..self.pos] {
                "ms" => 1,
                "s" => 1_000,
                "m" => 60_000,
                "h" => 3_600_000,
                _ => {
                    return Err(self
                        .err_at(
                            ParseErrorCode::NonCanonicalDecimal,
                            span,
                            "numbers must be canonical decimals",
                        )
                        .with_help(
                            "no exponents (write 100000, not 1e5), no digit separators \
                             (write 500000, not 500_000); the only letters allowed after \
                             a number are the duration units ms, s, m, h",
                        ));
                }
            };
            if has_fraction {
                return Err(self
                    .err_at(
                        ParseErrorCode::InvalidDuration,
                        span,
                        "duration literals take an integer count",
                    )
                    .with_help("write `1500ms`, not `1.5s`"));
            }
            let overflow = || {
                self.err_at(
                    ParseErrorCode::DurationOverflow,
                    span,
                    "duration is too large to represent in milliseconds",
                )
            };
            let count: u64 = self.src[start..unit_start]
                .parse()
                .map_err(|_| overflow())?;
            let ms = count.checked_mul(multiplier).ok_or_else(overflow)?;
            self.tokens.push(Token {
                kind: TokenKind::Duration(ms),
                span,
            });
            return Ok(());
        }

        // Canonical decimal: no leading zeros on the integer part (signal-id.md rule,
        // ratified into the parser 2026-07-18).
        let int_part = &self.src[start..int_end];
        if int_part.len() > 1 && int_part.starts_with('0') {
            return Err(self
                .err_at(
                    ParseErrorCode::NonCanonicalDecimal,
                    Span::new(start as u32, self.pos as u32),
                    "decimals cannot have leading zeros on the integer part",
                )
                .with_help("write `5`, not `05`"));
        }
        self.push(TokenKind::Decimal, start);
        Ok(())
    }

    fn variant(&mut self) -> Result<(), ParseError> {
        let start = self.pos;
        self.pos += 1; // opening quote
        if !self.peek_byte().is_some_and(|b| b.is_ascii_alphabetic()) {
            let end = (self.pos + 1).min(self.bytes.len());
            return Err(self
                .err_at(
                    ParseErrorCode::InvalidEnumVariant,
                    Span::new(start as u32, end as u32),
                    "enum variants are single-quoted identifiers",
                )
                .with_help(
                    "write 'missed': a quote, an identifier starting with a letter, a quote",
                ));
        }
        while self.peek_byte().is_some_and(Self::is_ident_byte) {
            self.pos += 1;
        }
        if self.peek_byte() == Some(b'\'') {
            self.pos += 1;
            self.push(TokenKind::Variant, start);
            Ok(())
        } else {
            Err(self
                .err_at(
                    ParseErrorCode::InvalidEnumVariant,
                    Span::new(start as u32, self.pos as u32),
                    "unterminated enum variant",
                )
                .with_help("add the closing single quote: 'missed'"))
        }
    }

    fn punct(&mut self) -> Result<(), ParseError> {
        let start = self.pos;
        let b = self.bytes[self.pos];
        let kind = match b {
            b'+' => {
                self.pos += 1;
                TokenKind::Plus
            }
            b'-' => {
                self.pos += 1;
                TokenKind::Minus
            }
            b'*' => {
                self.pos += 1;
                TokenKind::Star
            }
            b'/' => {
                self.pos += 1;
                TokenKind::Slash
            }
            b'(' => {
                self.pos += 1;
                TokenKind::LParen
            }
            b')' => {
                self.pos += 1;
                TokenKind::RParen
            }
            b'[' => {
                self.pos += 1;
                TokenKind::LBracket
            }
            b']' => {
                self.pos += 1;
                TokenKind::RBracket
            }
            b',' => {
                self.pos += 1;
                TokenKind::Comma
            }
            b':' => {
                self.pos += 1;
                TokenKind::Colon
            }
            b'.' => {
                if self
                    .peek_byte_at(1)
                    .is_some_and(|next| next.is_ascii_digit())
                {
                    self.pos += 1;
                    while self.peek_byte().is_some_and(|next| next.is_ascii_digit()) {
                        self.pos += 1;
                    }
                    return Err(self
                        .err_at(
                            ParseErrorCode::NonCanonicalDecimal,
                            Span::new(start as u32, self.pos as u32),
                            "decimals need a leading digit",
                        )
                        .with_help("write `0.5`, not `.5`"));
                }
                self.pos += 1;
                TokenKind::Dot
            }
            b'<' => {
                self.pos += 1;
                if self.peek_byte() == Some(b'=') {
                    self.pos += 1;
                    TokenKind::Le
                } else {
                    TokenKind::Lt
                }
            }
            b'>' => {
                self.pos += 1;
                if self.peek_byte() == Some(b'=') {
                    self.pos += 1;
                    TokenKind::Ge
                } else {
                    TokenKind::Gt
                }
            }
            b'=' => {
                self.pos += 1;
                if self.peek_byte() == Some(b'=') {
                    self.pos += 1;
                    TokenKind::EqEq
                } else {
                    TokenKind::Assign
                }
            }
            b'!' => {
                if self.peek_byte_at(1) == Some(b'=') {
                    self.pos += 2;
                    TokenKind::NotEq
                } else {
                    return Err(self
                        .err_at(
                            ParseErrorCode::UnexpectedToken,
                            Span::new(start as u32, start as u32 + 1),
                            "unexpected character `!`",
                        )
                        .with_help(
                            "write `!=` for not-equal; logical negation is the word `not`",
                        ));
                }
            }
            other => {
                return Err(self.err_at(
                    ParseErrorCode::UnexpectedToken,
                    Span::new(start as u32, start as u32 + 1),
                    format!("unexpected character `{}`", other as char),
                ));
            }
        };
        self.tokens.push(Token {
            kind,
            span: Span::new(start as u32, self.pos as u32),
        });
        Ok(())
    }
}

/// The grammar's keyword spellings: the token kind a word lexes to when it is not a
/// plain identifier. This is the single source of the keyword list — `parse::reserved`
/// builds the reserved-identifier set from it, so a keyword added here is automatically
/// reserved.
pub(in crate::parse) fn keyword_kind(word: &str) -> Option<TokenKind> {
    match word {
        "and" => Some(TokenKind::And),
        "or" => Some(TokenKind::Or),
        "not" => Some(TokenKind::Not),
        "if" => Some(TokenKind::If),
        "then" => Some(TokenKind::Then),
        "else" => Some(TokenKind::Else),
        "true" => Some(TokenKind::True),
        "false" => Some(TokenKind::False),
        "set" => Some(TokenKind::Set),
        "push" => Some(TokenKind::Push),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(src: &str) -> Vec<TokenKind> {
        lex(src)
            .expect("lexes")
            .into_iter()
            .map(|t| t.kind)
            .collect()
    }

    fn err_code(src: &str) -> ParseErrorCode {
        lex(src).expect_err("must fail to lex").code
    }

    #[test]
    fn lexes_every_token_kind() {
        assert_eq!(
            kinds("a 5 0.5 10s 'v' and or not if then else true false set push"),
            vec![
                TokenKind::Ident,
                TokenKind::Decimal,
                TokenKind::Decimal,
                TokenKind::Duration(10_000),
                TokenKind::Variant,
                TokenKind::And,
                TokenKind::Or,
                TokenKind::Not,
                TokenKind::If,
                TokenKind::Then,
                TokenKind::Else,
                TokenKind::True,
                TokenKind::False,
                TokenKind::Set,
                TokenKind::Push,
                TokenKind::Eof,
            ]
        );
        assert_eq!(
            kinds("+ - * / < <= > >= == != = ( ) [ ] , . :"),
            vec![
                TokenKind::Plus,
                TokenKind::Minus,
                TokenKind::Star,
                TokenKind::Slash,
                TokenKind::Lt,
                TokenKind::Le,
                TokenKind::Gt,
                TokenKind::Ge,
                TokenKind::EqEq,
                TokenKind::NotEq,
                TokenKind::Assign,
                TokenKind::LParen,
                TokenKind::RParen,
                TokenKind::LBracket,
                TokenKind::RBracket,
                TokenKind::Comma,
                TokenKind::Dot,
                TokenKind::Colon,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn durations_lower_per_unit() {
        assert_eq!(kinds("500ms")[0], TokenKind::Duration(500));
        assert_eq!(kinds("10s")[0], TokenKind::Duration(10_000));
        assert_eq!(kinds("5m")[0], TokenKind::Duration(300_000));
        assert_eq!(kinds("2h")[0], TokenKind::Duration(7_200_000));
    }

    #[test]
    fn duration_overflow_is_an_error_not_a_panic() {
        assert_eq!(
            err_code("99999999999999999999h"),
            ParseErrorCode::DurationOverflow
        );
        assert_eq!(
            err_code("9999999999999999999h"),
            ParseErrorCode::DurationOverflow
        );
    }

    #[test]
    fn fractional_durations_are_invalid() {
        assert_eq!(err_code("1.5s"), ParseErrorCode::InvalidDuration);
    }

    #[test]
    fn forbidden_characters_reject_at_char_level() {
        assert_eq!(err_code("a\"b"), ParseErrorCode::ForbiddenCharacter);
        assert_eq!(err_code("a\\b"), ParseErrorCode::ForbiddenCharacter);
        assert_eq!(err_code("a\nb"), ParseErrorCode::ForbiddenCharacter);
        assert_eq!(err_code("é"), ParseErrorCode::NonAsciiCharacter);
    }

    #[test]
    fn non_canonical_decimals_reject() {
        assert_eq!(err_code("1e5"), ParseErrorCode::NonCanonicalDecimal);
        assert_eq!(err_code(".5"), ParseErrorCode::NonCanonicalDecimal);
        assert_eq!(err_code("5."), ParseErrorCode::NonCanonicalDecimal);
        assert_eq!(err_code("05"), ParseErrorCode::NonCanonicalDecimal);
        assert_eq!(err_code("1_000"), ParseErrorCode::NonCanonicalDecimal);
    }

    #[test]
    fn eof_token_spans_one_past_the_end() {
        let tokens = lex("ab ").expect("lexes");
        let eof = tokens.last().expect("has eof");
        assert_eq!(eof.kind, TokenKind::Eof);
        assert_eq!((eof.span.start, eof.span.end), (3, 3));
    }

    #[test]
    fn variants_must_be_identifiers() {
        assert_eq!(err_code("'123'"), ParseErrorCode::InvalidEnumVariant);
        assert_eq!(err_code("'missed"), ParseErrorCode::InvalidEnumVariant);
    }
}
