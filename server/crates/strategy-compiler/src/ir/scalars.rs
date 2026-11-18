//! Shared string newtypes: identifiers, expression strings, and canonical value strings.
//!
//! The IR never parses expressions — it only marks which strings *are* expressions. These
//! newtypes exist so the compiler cannot confuse display text with expression text, and so
//! the generated JSON Schema carries the right `pattern` and `description` per field.
//!
//! Rust note: each newtype is a "wrapper struct" around `String`. `#[serde(transparent)]`
//! makes it serialize as the bare string; the wrapper exists purely for type-level
//! distinction, like a `NewType = NewType(str)` dataclass with no runtime cost.

use std::borrow::{Borrow, Cow};
use std::fmt;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Deserializer, Serialize};

/// JSON Schema `pattern` for identifiers (anchored).
pub const IDENT_PATTERN: &str = "^[a-zA-Z][a-zA-Z0-9_]*$";

/// JSON Schema `pattern` for canonical unsigned decimal strings (anchored).
pub const DECIMAL_PATTERN: &str = "^(0|[1-9][0-9]*)(\\.[0-9]*[1-9])?$";

/// JSON Schema `pattern` for canonical signed decimal strings (anchored). `-0` is
/// additionally forbidden via a `not` clause in the schema.
pub const SIGNED_DECIMAL_PATTERN: &str = "^-?(0|[1-9][0-9]*)(\\.[0-9]*[1-9])?$";

/// JSON Schema `pattern` for duration strings (anchored).
pub const DURATION_PATTERN: &str = "^[0-9]+(ms|s|m|h)$";

/// JSON Schema `pattern` for expression-text strings (anchored): printable ASCII with
/// `"` and `\` structurally excluded, so every legal string embeds in JSON verbatim.
pub const EXPR_TEXT_PATTERN: &str = "^[ !#-\\[\\]-~]+$";

fn is_ident(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() => chars.all(|c| c.is_ascii_alphanumeric() || c == '_'),
        _ => false,
    }
}

/// Canonical integer part: `0`, or a nonzero leading digit followed by digits.
fn is_canonical_int(part: &str) -> bool {
    match part.as_bytes() {
        [] => false,
        [b'0'] => true,
        [first, rest @ ..] => (b'1'..=b'9').contains(first) && rest.iter().all(u8::is_ascii_digit),
    }
}

/// Canonical unsigned decimal: no exponent, no sign, no bare dot, no leading zeros on the
/// integer part, no trailing zeros in the fraction. `0.4`, never `0.40` or `.4` or `05`.
fn is_canonical_decimal(s: &str) -> bool {
    match s.split_once('.') {
        Some((int, frac)) => {
            is_canonical_int(int)
                && !frac.is_empty()
                && frac.bytes().all(|b| b.is_ascii_digit())
                && !frac.ends_with('0')
        }
        None => is_canonical_int(s),
    }
}

/// Canonical signed decimal: an optional leading `-` on a canonical decimal, with the
/// spelling `-0` forbidden (`-0.5` is legal — the fraction carries the magnitude).
fn is_canonical_signed_decimal(s: &str) -> bool {
    if s == "-0" {
        return false;
    }
    match s.strip_prefix('-') {
        Some(rest) => is_canonical_decimal(rest),
        None => is_canonical_decimal(s),
    }
}

fn is_duration(s: &str) -> bool {
    // "ms" must be tried before "s": "500ms" would otherwise strip to "500m".
    let digits = s
        .strip_suffix("ms")
        .or_else(|| s.strip_suffix('s'))
        .or_else(|| s.strip_suffix('m'))
        .or_else(|| s.strip_suffix('h'));
    matches!(digits, Some(d) if !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
}

/// Expression text: nonempty printable ASCII with `"` and `\` excluded, so every legal
/// string embeds in a JSON string with zero escape sequences. Newlines are excluded by
/// the printable-ASCII bound.
fn is_expr_text(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| (0x20..=0x7E).contains(&b) && b != b'"' && b != b'\\')
}

/// A name in the document: section entry names, map keys, tags, and leg references.
///
/// Shape-checked at deserialization (`[a-zA-Z][a-zA-Z0-9_]*`). Reservedness (grammar
/// keywords, namespace heads, builtins, the leg name `stay`) is a validator concern — the
/// reserved set is grammar knowledge and lives with the parser, not with the data types.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct Ident(String);

impl Ident {
    /// Validate a string as an identifier. Used by custom deserializers that must
    /// distinguish an identifier from another string form (e.g. on-table destinations).
    pub fn parse(s: String) -> Result<Self, String> {
        if is_ident(&s) {
            Ok(Ident(s))
        } else {
            Err(format!(
                "`{s}` is not a valid identifier (expected [a-zA-Z][a-zA-Z0-9_]*)"
            ))
        }
    }

    /// The identifier as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Ident {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

// Lets `BTreeMap<Ident, T>` be queried with a plain `&str` key. Sound because `Ident`
// orders exactly like its inner `String`.
impl Borrow<str> for Ident {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for Ident {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Ident::parse(s).map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for Ident {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("Ident")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "pattern": IDENT_PATTERN,
            "description": "An identifier: [a-zA-Z][a-zA-Z0-9_]*. Must not be a reserved word (grammar keywords, namespace heads, builtin names, the leg name 'stay')."
        })
    }
}

/// A single-line body expression string. Parsed only by the compiler's expression parser;
/// the IR checks the character set (printable ASCII, no `"` or `\`) and nothing else.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct ExprStr(String);

impl ExprStr {
    /// The raw expression text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for ExprStr {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        if is_expr_text(&s) {
            Ok(ExprStr(s))
        } else {
            Err(serde::de::Error::custom(format_args!(
                "`{s}` is not expression text (nonempty printable ASCII without `\"` or `\\`)"
            )))
        }
    }
}

impl JsonSchema for ExprStr {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("ExprStr")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "pattern": EXPR_TEXT_PATTERN,
            "description": "A single-line expression string in the ratified body expression grammar. Never contains double quotes, backslashes, or newlines."
        })
    }
}

/// A statement string in a `do` list (`set state.x = ...`). Parsed only by the compiler's
/// statement parser; the IR checks the character set and nothing else.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct StmtStr(String);

impl StmtStr {
    /// The raw statement text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for StmtStr {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        if is_expr_text(&s) {
            Ok(StmtStr(s))
        } else {
            Err(serde::de::Error::custom(format_args!(
                "`{s}` is not statement text (nonempty printable ASCII without `\"` or `\\`)"
            )))
        }
    }
}

impl JsonSchema for StmtStr {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("StmtStr")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "pattern": EXPR_TEXT_PATTERN,
            "description": "A statement string: a set statement whose right-hand side is a body expression."
        })
    }
}

/// A canonical unsigned decimal string. No exponent, no sign, no bare dot, no leading
/// zeros on the integer part, no trailing zeros in the fraction: `0.4`, never `0.40`.
/// Numbers travel as strings throughout the document family to avoid float drift, and
/// the written form feeds content hashing — which is why canonical form is enforced at
/// the type boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct DecimalStr(String);

impl DecimalStr {
    /// The decimal as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for DecimalStr {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        if is_canonical_decimal(&s) {
            Ok(DecimalStr(s))
        } else {
            Err(serde::de::Error::custom(format_args!(
                "`{s}` is not a canonical decimal string (no sign, no exponent, no leading or trailing zeros: `0.4`, never `0.40` or `05`)"
            )))
        }
    }
}

impl JsonSchema for DecimalStr {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("DecimalStr")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "pattern": DECIMAL_PATTERN,
            "description": "A canonical unsigned decimal string, e.g. \"0.05\" or \"50\". No exponent, sign, bare dot, leading zeros, or trailing fraction zeros. Used where negatives are a correctness violation (price bands, table values)."
        })
    }
}

/// A canonical decimal string with an optional leading minus; `-0` is forbidden.
/// Ruled 2026-07-20, mirroring the `-1` unary-minus literal ruling: legal in trigger
/// leaf-value position and table keys (signed thresholds and signed curve domains),
/// nowhere else.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct SignedDecimalStr(String);

impl SignedDecimalStr {
    /// Validate a string as a canonical signed decimal. Used by `LeafValue`'s custom
    /// deserializer, which must distinguish a decimal from an enum variant.
    pub fn parse(s: String) -> Result<Self, String> {
        if is_canonical_signed_decimal(&s) {
            Ok(SignedDecimalStr(s))
        } else {
            Err(format!(
                "`{s}` is not a canonical signed decimal string (optional leading `-`, `-0` forbidden, no exponent or leading/trailing zeros)"
            ))
        }
    }

    /// The decimal as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for SignedDecimalStr {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        SignedDecimalStr::parse(s).map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for SignedDecimalStr {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("SignedDecimalStr")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "pattern": SIGNED_DECIMAL_PATTERN,
            "not": { "const": "-0" },
            "description": "A canonical decimal string with an optional leading minus; -0 is forbidden. Legal in trigger leaf values and table keys."
        })
    }
}

/// A duration string: integer count plus unit, e.g. `10s`, `500ms`, `5m`, `2h`.
/// The unit lives in the value, never in the field name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct DurationStr(String);

impl DurationStr {
    /// The duration as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The duration in milliseconds, the same unit table as the expression lexer.
    /// Saturates at `u64::MAX` — an astronomically large duration is representable in
    /// the string but meaningless past that bound.
    pub fn as_millis(&self) -> u64 {
        let unit_start = self
            .0
            .find(|c: char| !c.is_ascii_digit())
            .expect("DurationStr always carries a unit");
        let (count, unit) = self.0.split_at(unit_start);
        let multiplier: u128 = match unit {
            "ms" => 1,
            "s" => 1_000,
            "m" => 60_000,
            "h" => 3_600_000,
            _ => unreachable!("DurationStr admits only ms, s, m, h"),
        };
        let ms = count
            .parse::<u128>()
            .unwrap_or(u128::MAX)
            .saturating_mul(multiplier);
        u64::try_from(ms).unwrap_or(u64::MAX)
    }
}

impl<'de> Deserialize<'de> for DurationStr {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        if is_duration(&s) {
            Ok(DurationStr(s))
        } else {
            Err(serde::de::Error::custom(format_args!(
                "`{s}` is not a duration string (expected integer + ms|s|m|h, e.g. `10s`)"
            )))
        }
    }
}

impl JsonSchema for DurationStr {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("DurationStr")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "pattern": DURATION_PATTERN,
            "description": "A duration string: integer + unit (ms, s, m, h), e.g. \"10s\". Integer part only — write 1500ms, not 1.5s."
        })
    }
}
