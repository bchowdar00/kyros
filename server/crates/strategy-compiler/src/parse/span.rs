//! Byte spans into a single expression source string.
//!
//! EOF convention: the EOF token's span is `(len, len)` — zero-width, pointing one past
//! the last byte — so expected-X-found-end-of-input diagnostics underline the end of the
//! string, never a dummy position.

use serde::Serialize;

/// A half-open byte range `[start, end)` into the source string of one expression.
///
/// Expressions are single-line ASCII, so byte offsets are column numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Span {
    /// Byte offset of the first byte, inclusive.
    pub start: u32,
    /// Byte offset one past the last byte, exclusive.
    pub end: u32,
}

impl Span {
    /// A span covering `[start, end)`.
    pub fn new(start: u32, end: u32) -> Self {
        Span { start, end }
    }

    /// A zero-width span at `at` — used for EOF and insertion points.
    pub fn point(at: u32) -> Self {
        Span { start: at, end: at }
    }

    /// The smallest span covering both `self` and `other`.
    pub fn join(self, other: Span) -> Span {
        Span {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }
}

/// A value paired with the source span it came from. Every AST node is `Spanned`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Spanned<T> {
    /// The value.
    pub node: T,
    /// Where in the source string it was written.
    pub span: Span,
}

impl<T> Spanned<T> {
    /// Pair a value with its span.
    pub fn new(node: T, span: Span) -> Self {
        Spanned { node, span }
    }
}
