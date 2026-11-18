//! Parse diagnostics: machine-readable codes, spans, and human-and-LLM-facing rendering.
//!
//! Every error carries a stable code, a byte span, a standalone message, and optional
//! expected/found/help. The struct serializes to JSON for the LLM repair loop; `Display`
//! renders the human form:
//!
//! ```text
//! error[chained-comparison]: comparison operators cannot be chained
//!   0 < x < 1
//!         ^
//!   = help: split into two comparisons joined with 'and'
//! ```
//!
//! Message style rules (enforced by review): messages start lowercase, name the
//! construct rather than parser internals, and stand alone without the source visible.

use std::fmt;

use serde::Serialize;

use super::span::Span;

/// One parse failure. Single-expression parsing stops at the first error; collect-all
/// is the document level's job (many expressions, each parsed independently).
///
/// Rust note: the payload lives behind a `Box` so `Result<_, ParseError>` stays
/// pointer-sized on the happy path (clippy's `result_large_err`); `Deref` makes the
/// fields read as if they were inline (`err.code`, `err.span`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct ParseError(Box<ParseErrorDetail>);

impl std::ops::Deref for ParseError {
    type Target = ParseErrorDetail;

    fn deref(&self) -> &ParseErrorDetail {
        &self.0
    }
}

/// The fields of one parse failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ParseErrorDetail {
    /// The machine-readable failure class.
    pub code: ParseErrorCode,
    /// Where in the source string the failure is anchored (half-open byte range).
    pub span: Span,
    /// Human-readable description; stands alone without the source visible.
    pub message: String,
    /// What the parser was looking for, when the failure is an unexpected token.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected: Option<String>,
    /// What the parser actually found, when the failure is an unexpected token.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub found: Option<String>,
    /// Concrete, imperative guidance for fixing the expression — written for a human
    /// and a repair-loop LLM at once.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
    /// The full source string the span indexes into. Included so a serialized error is
    /// self-contained for the repair loop.
    pub source: String,
}

impl ParseError {
    /// Construct an error with the required fields; attach the optional ones with the
    /// builder methods.
    pub fn new(
        code: ParseErrorCode,
        span: Span,
        message: impl Into<String>,
        source: impl Into<String>,
    ) -> Self {
        ParseError(Box::new(ParseErrorDetail {
            code,
            span,
            message: message.into(),
            expected: None,
            found: None,
            help: None,
            source: source.into(),
        }))
    }

    /// Attach expected/found descriptions.
    pub fn with_expected_found(
        mut self,
        expected: impl Into<String>,
        found: impl Into<String>,
    ) -> Self {
        self.0.expected = Some(expected.into());
        self.0.found = Some(found.into());
        self
    }

    /// Attach help text.
    pub fn with_help(mut self, help: impl Into<String>) -> Self {
        self.0.help = Some(help.into());
        self
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "error[{}]: {}", self.code.as_str(), self.message)?;
        writeln!(f, "  {}", self.source)?;
        // Caret line: at least one caret, even for zero-width (EOF) spans.
        let start = self.span.start as usize;
        let width = (self.span.end.saturating_sub(self.span.start) as usize).max(1);
        writeln!(f, "  {}{}", " ".repeat(start), "^".repeat(width))?;
        if let (Some(expected), Some(found)) = (&self.expected, &self.found) {
            writeln!(f, "  = expected {expected}, found {found}")?;
        }
        if let Some(help) = &self.help {
            writeln!(f, "  = help: {help}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ParseError {}

/// The closed set of parse failure classes. Tooling branches on these; messages are
/// never parsed. Serialized in kebab-case (`chained-comparison`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ParseErrorCode {
    /// A token that cannot appear here. Example: `1 + * 2`.
    UnexpectedToken,
    /// The expression ended where more input was required. Example: `1 + `.
    UnexpectedEof,
    /// A character outside the grammar's alphabet: `"`, `\`, or a newline.
    /// Example: `event.result == "missed"`.
    ForbiddenCharacter,
    /// A non-ASCII byte anywhere in the expression. Example: `signal.é`.
    NonAsciiCharacter,
    /// An identifier in reference position without a namespace. Example: `high_water`.
    BareIdentifier,
    /// A dotted reference whose head is not one of the namespaces.
    /// Example: `foo.bar`.
    UnknownNamespace,
    /// A call to a name outside the closed builtin set. Example: `sqrtt(x)`.
    UnknownFunction,
    /// A builtin called with the wrong number of arguments. Example: `min(1, 2, 3)`.
    WrongArity,
    /// A second comparison operator in a non-associative layer. Example: `0 < x < 1`.
    ChainedComparison,
    /// An `if ... then ...` conditional with no `else` branch. Example: `if a then 1`.
    MissingElse,
    /// v0.1's `crossed()` — recognized and steered, never "unknown function": edge
    /// detection lives in trigger fire/rearm modes now. Example: `crossed(a > 1)`.
    SupersededCrossed,
    /// v0.1's `fired()` — recognized and steered: cause dispatch is `triggered(NAME)`.
    /// Example: `fired(signal.pbp)`.
    SupersededFired,
    /// v0.1's `timer.` namespace — recognized and steered: timers are triggers now.
    /// Example: `fired(timer.analyze)` after the `fired` steer, or `timer.x` alone.
    SupersededTimerNamespace,
    /// `triggered` with anything but a single bare trigger name as its argument.
    /// Example: `triggered(signal.book)`.
    TriggeredArgNotBareName,
    /// A number not in canonical decimal form. Example: `1e5`, `.5`, `5.`, `05`.
    NonCanonicalDecimal,
    /// A duration literal with a fractional count or malformed unit. Example: `1.5s`.
    InvalidDuration,
    /// A duration whose millisecond value overflows. Example: `99999999999999999999h`.
    DurationOverflow,
    /// A malformed enum variant literal. Example: `'123'` or an unterminated `'missed`.
    InvalidEnumVariant,
    /// A statement assigning to anything but a state slot. Example: `set platform.x = 1`.
    InvalidLvalue,
    /// `lookup` whose first argument is not a `table.` reference.
    /// Example: `lookup(state.x, 1)`.
    TableRefOutsideLookup,
    /// An empty or whitespace-only source string.
    EmptyExpression,
    /// A complete expression followed by leftover tokens. Example: `1 + 2 3`.
    TrailingInput,
}

impl ParseErrorCode {
    /// The stable kebab-case slug, identical to the serde serialization.
    pub fn as_str(self) -> &'static str {
        match self {
            ParseErrorCode::UnexpectedToken => "unexpected-token",
            ParseErrorCode::UnexpectedEof => "unexpected-eof",
            ParseErrorCode::ForbiddenCharacter => "forbidden-character",
            ParseErrorCode::NonAsciiCharacter => "non-ascii-character",
            ParseErrorCode::BareIdentifier => "bare-identifier",
            ParseErrorCode::UnknownNamespace => "unknown-namespace",
            ParseErrorCode::UnknownFunction => "unknown-function",
            ParseErrorCode::WrongArity => "wrong-arity",
            ParseErrorCode::ChainedComparison => "chained-comparison",
            ParseErrorCode::MissingElse => "missing-else",
            ParseErrorCode::SupersededCrossed => "superseded-crossed",
            ParseErrorCode::SupersededFired => "superseded-fired",
            ParseErrorCode::SupersededTimerNamespace => "superseded-timer-namespace",
            ParseErrorCode::TriggeredArgNotBareName => "triggered-arg-not-bare-name",
            ParseErrorCode::NonCanonicalDecimal => "non-canonical-decimal",
            ParseErrorCode::InvalidDuration => "invalid-duration",
            ParseErrorCode::DurationOverflow => "duration-overflow",
            ParseErrorCode::InvalidEnumVariant => "invalid-enum-variant",
            ParseErrorCode::InvalidLvalue => "invalid-lvalue",
            ParseErrorCode::TableRefOutsideLookup => "table-ref-outside-lookup",
            ParseErrorCode::EmptyExpression => "empty-expression",
            ParseErrorCode::TrailingInput => "trailing-input",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_code_source_caret_and_help() {
        let err = ParseError::new(
            ParseErrorCode::ChainedComparison,
            Span::new(6, 7),
            "comparison operators cannot be chained",
            "0 < x < 1",
        )
        .with_help("split into two comparisons joined with 'and'");
        let rendered = err.to_string();
        let expected = concat!(
            "error[chained-comparison]: comparison operators cannot be chained\n",
            "  0 < x < 1\n",
            "        ^\n",
            "  = help: split into two comparisons joined with 'and'\n",
        );
        assert_eq!(rendered, expected);
    }

    #[test]
    fn eof_span_renders_one_caret_past_the_end() {
        let err = ParseError::new(
            ParseErrorCode::UnexpectedEof,
            Span::point(4),
            "expression ended unexpectedly",
            "1 + ",
        )
        .with_expected_found("a value", "end of input");
        let rendered = err.to_string();
        assert!(rendered.contains("  1 + \n      ^\n"), "{rendered}");
        assert!(rendered.contains("= expected a value, found end of input"));
    }

    #[test]
    fn serializes_kebab_case_codes() {
        let err = ParseError::new(ParseErrorCode::SupersededCrossed, Span::new(0, 1), "m", "s");
        let json = serde_json::to_value(&err).expect("serializes");
        assert_eq!(json["code"], "superseded-crossed");
    }
}
