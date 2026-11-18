//! `TypeSpec` — the unified value/key type spelling (ruled 2026-07-20).
//!
//! One rule in every position (`state` map `key_types`, map `value_type`, scalar
//! `value_type`, window `value_type`): a bare type-name string for primitives, or the
//! tagged `{"type": ..., "variants": ...}` object. Both spellings of a primitive are
//! legal; round-trip fidelity requires preserving whichever the author wrote, which is
//! why this is an untagged enum over the two forms rather than a normalizing parser.
//!
//! v0.3 primitives: `u8` (small counters), `decimal` (prices, sizes, PnL — the type
//! most signal fields and every platform read carry), `bool` (flags), and `string`
//! (addresses, names — compared with `==`/`!=` only, never ordered).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ir::scalars::Ident;

/// A value or key type, in either ratified spelling.
///
/// Rust note: `#[serde(untagged)]` tries each variant against the JSON value; the two
/// forms are disjoint JSON types (string vs object), so matching is unambiguous.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum TypeSpec {
    /// The bare-string spelling of a primitive: `"u8"`, `"decimal"`, `"bool"`.
    Name(PrimitiveType),
    /// The tagged-object spelling: `{"type": "u8"}` or `{"type": "enum", "variants": [...]}`.
    Object(TypeSpecObject),
}

impl TypeSpec {
    /// The type's kind, independent of which spelling the author used. Every consumer
    /// branches on this rather than on the two spellings.
    pub fn kind(&self) -> TypeKind<'_> {
        match self {
            TypeSpec::Name(PrimitiveType::U8) | TypeSpec::Object(TypeSpecObject::U8) => {
                TypeKind::U8
            }
            TypeSpec::Name(PrimitiveType::Decimal)
            | TypeSpec::Object(TypeSpecObject::Decimal) => TypeKind::Decimal,
            TypeSpec::Name(PrimitiveType::Bool) | TypeSpec::Object(TypeSpecObject::Bool) => {
                TypeKind::Bool
            }
            TypeSpec::Name(PrimitiveType::String)
            | TypeSpec::Object(TypeSpecObject::String) => TypeKind::Str,
            TypeSpec::Object(TypeSpecObject::Enum { variants }) => TypeKind::Enum(variants),
        }
    }
}

/// A `TypeSpec` with the spelling erased.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeKind<'a> {
    /// An unsigned 8-bit integer.
    U8,
    /// An exact decimal.
    Decimal,
    /// A boolean.
    Bool,
    /// A string.
    Str,
    /// A closed variant set, in declared order.
    Enum(&'a [Ident]),
}

/// A primitive type name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PrimitiveType {
    /// An unsigned 8-bit integer. Defaults to `0`.
    U8,
    /// An exact decimal (prices, sizes, PnL). Defaults to `0`.
    Decimal,
    /// A boolean. Defaults to `false`.
    Bool,
    /// A string. Defaults to the empty string.
    String,
}

/// The tagged-object spelling of a type, discriminated on `type`.
///
/// Rust note: internally-tagged enums cannot carry `deny_unknown_fields` (a serde
/// limitation); strictness comes from the round-trip echo test.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TypeSpecObject {
    /// An unsigned 8-bit integer.
    U8,
    /// An exact decimal.
    Decimal,
    /// A boolean.
    Bool,
    /// A string.
    String,
    /// A closed set of named variants. When the variants mirror a market's outcomes they
    /// must use the market's outcome labels verbatim.
    Enum {
        /// The legal variant names, e.g. `["home", "away"]`.
        variants: Vec<Ident>,
    },
}
