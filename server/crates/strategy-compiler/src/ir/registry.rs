//! Registry types: the venue-truth compiler input (markets, series, signal definitions).
//!
//! One file, one loader (`registry.json`). The IR declares typed references; the
//! validator cross-checks every one against this. Serde-only: not LLM-facing, so no
//! `JsonSchema`. Lives in `ir` (not the compiler) because the backtester and harness
//! will need these types and the dependency arrow must keep pointing at `ir`.

use serde::{Deserialize, Serialize};

use crate::ir::scalars::{DecimalStr, DurationStr, Ident};

/// The whole registry: venue truth, loaded from one file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Registry {
    /// Concrete markets.
    pub markets: Vec<Market>,
    /// Market series (rolling instruments like 2-minute up/down windows).
    pub series: Vec<Series>,
    /// Signal definitions, including the feed identity behind normalized signals.
    pub signals: Vec<SignalDef>,
}

/// A concrete market.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Market {
    /// The opaque venue market id.
    pub market_id: String,
    /// Outcome-structured (binary book) or linear.
    pub structure: MarketStructure,
    /// Ordered outcome labels; index 0 is the positive side of the unified book.
    /// Present iff outcome-structured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcomes: Option<Vec<String>>,
    /// Quote parameters.
    pub quote: Quote,
    /// The order features the venue implements for this market. Absent: none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<Capabilities>,
    /// Authoring context only: the LLM reads it, the decompiler may render English from
    /// it, nothing else touches it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

/// The optional order features a market or series supports. The compiler refuses an
/// order flag the instrument does not declare, so a strategy cannot compile an order
/// the venue would reject.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Capabilities {
    /// `post_only` limit orders: rest or reject, never take.
    pub post_only: bool,
    /// Good-till-time (`expire_after`) resting orders.
    pub gtd: bool,
    /// `reduce_only` orders that may only shrink the position.
    pub reduce_only: bool,
}

/// A market series: a rolling family of markets with a cadence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Series {
    /// The opaque venue series id; instruments may reference `<series_id>.current`.
    pub series_id: String,
    /// Outcome-structured or linear.
    pub structure: MarketStructure,
    /// Window cadence, e.g. `"2m"`.
    pub cadence: DurationStr,
    /// Ordered outcome labels, as on `Market`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcomes: Option<Vec<String>>,
    /// Quote parameters shared by every member.
    pub quote: Quote,
    /// The order features the venue implements for every member. Absent: none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<Capabilities>,
    /// Authoring context only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

/// Market structure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MarketStructure {
    /// A book over a closed outcome set; prices are implied probabilities.
    Outcome,
    /// A linear instrument (e.g. a perp).
    Linear,
}

/// Quote parameters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Quote {
    /// Price tick.
    pub tick: DecimalStr,
    /// `[lo, hi]` legal price range; present on outcome-structured quotes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<[DecimalStr; 2]>,
}

/// A signal definition: id, location, payload fields, and (for normalized signals) the
/// feed identity behind it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignalDef {
    /// The signal id, version-in-id form (`....vN`); must agree with `version`.
    pub signal_id: String,
    /// The signal's version, matching the id suffix.
    pub version: String,
    /// Human-readable name for catalogues and cards (`"Coinbase BTC/USD RSI(14)"`).
    /// Presentation only; never read by the compiler.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Version of the normalization applied to the underlying feed, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub normalization_version: Option<String>,
    /// The raw feed this entry normalizes, if any. Filtered signals name this feed
    /// directly; their `event.*` fields validate against `fields` here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feed: Option<String>,
    /// The feed's version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feed_version: Option<String>,
    /// Where the signal is computed.
    pub location: SignalLocation,
    /// Human description (authoring context).
    pub description: String,
    /// The payload fields, in declared order.
    pub fields: Vec<FieldDef>,
    /// Authoring context only (players map, outcome_fields bridge, etc.).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

/// Where a signal is computed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignalLocation {
    /// Computed by the platform (market state, TA indicators).
    Internal,
    /// Ingested from an external source.
    External,
}

/// One payload field of a signal.
///
/// Rust note: `#[serde(flatten)]` splices the tagged `FieldType` into this object, so
/// the JSON is flat: `{"name": ..., "type": ..., ["variants": ...]}`. Flatten and
/// `deny_unknown_fields` cannot combine (serde limitation), so unknown keys on a field
/// definition are ignored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FieldDef {
    /// The field name, read as `signal.<local>.<name>` / `event.<name>`.
    pub name: Ident,
    /// Human-readable label for a card showing this field (`"Round Volume"`).
    /// Presentation only; never read by the compiler.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Two sentences on what the number means, for a card's detail view.
    /// Presentation only; never read by the compiler.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Whether a catalogue shows this field by default; the rest are
    /// plumbing (timestamps, ages, parameters). Presentation only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headline: Option<bool>,
    /// The field's type, tagged on `type`.
    #[serde(flatten)]
    pub field_type: FieldType,
}

/// A payload field type.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FieldType {
    /// A decimal value.
    Decimal,
    /// An unsigned 8-bit integer.
    U8,
    /// An unsigned 16-bit integer.
    U16,
    /// An unsigned 32-bit integer.
    U32,
    /// An unsigned 64-bit integer.
    U64,
    /// A signed 64-bit integer.
    I64,
    /// A closed set of named variants.
    Enum {
        /// The legal variant names.
        variants: Vec<String>,
    },
    /// A duration in milliseconds.
    DurationMs,
    /// An availability time (epoch-nanos semantics); ordered like a number.
    Timestamp,
    /// A flag. Type-checked as the closed enum `true` / `false`, so a guard
    /// writes it plain like any variant.
    Bool,
    /// Free text: an address, a name. Compared with `==`/`!=` only.
    String,
}

/// How a field type behaves under comparison: the three classes the
/// validator distinguishes. Every [`FieldType`] maps to exactly one, so a
/// new type only needs a class here, never a new arm in the validator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldClass {
    /// Ordering comparators and arithmetic are legal.
    Numeric,
    /// `==` / `!=` / set-membership against one of the closed variants.
    Enum(Vec<String>),
    /// `==` / `!=` against a `{"str": ...}` literal.
    Str,
}

impl FieldType {
    /// Classify for type-checking.
    pub fn class(&self) -> FieldClass {
        match self {
            FieldType::Decimal
            | FieldType::U8
            | FieldType::U16
            | FieldType::U32
            | FieldType::U64
            | FieldType::I64
            | FieldType::DurationMs
            | FieldType::Timestamp => FieldClass::Numeric,
            FieldType::Enum { variants } => FieldClass::Enum(variants.clone()),
            FieldType::Bool => FieldClass::Enum(vec!["true".to_owned(), "false".to_owned()]),
            FieldType::String => FieldClass::Str,
        }
    }

    /// Whether ordering comparators and arithmetic are legal on this type.
    pub fn is_numeric(&self) -> bool {
        matches!(self.class(), FieldClass::Numeric)
    }
}
