//! ConditionTrees and their leaves: the SME-evaluable half of a trigger.
//!
//! Conditions reference declared signals only — never state, platform, or expression
//! names: a trigger must be evaluable by the Signal Matching Engine with zero strategy
//! state. There is deliberately no `not` node; negate a comparison by flipping its
//! comparator.

use std::borrow::Cow;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::ir::scalars::{ExprStr, Ident, SignedDecimalStr};

/// A ConditionTree: a leaf, or one `all`/`any` group of leaves.
///
/// Children are leaves (the corpus-ratified depth); `all` is a same-moment conjunction —
/// the SME tracks both edges of every child — and `any` is a stateless disjunction.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum Condition {
    /// A single leaf.
    Leaf(ConditionLeaf),
    /// `{"all": [leaves]}` — AND.
    All(AllGroup),
    /// `{"any": [leaves]}` — OR.
    Any(AnyGroup),
}

/// The AND group: all children hold at the same moment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AllGroup {
    /// The conjuncts.
    pub all: Vec<ConditionLeaf>,
}

/// The OR group: stateless disjunction.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AnyGroup {
    /// The disjuncts.
    pub any: Vec<ConditionLeaf>,
}

/// One condition leaf, in one of three ratified shapes. Bare and field leaves name one
/// signal; an expr leaf (v0.3) is arithmetic over any subscribed signals' fields,
/// compared against a constant or a param — the SME evaluates it against the latest
/// snapshot of every referenced signal whenever any of them publishes.
///
/// Rust note: `Serialize`/`JsonSchema` derive through `untagged` (the shapes' key sets
/// are disjoint), but `Deserialize` is manual — untagged's "did not match any variant"
/// is unacceptable at the highest-traffic LLM repair site, so deserialization dispatches
/// on key presence and reports exactly which combination is malformed.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(untagged)]
pub enum ConditionLeaf {
    /// `{"signal": s}` — true whenever the signal publishes.
    Bare(BareLeaf),
    /// `{"signal", "field", "comparator", "value"}`.
    Field(FieldLeaf),
    /// `{"expr", "comparator", "value"}`.
    Expr(ExprLeaf),
}

/// The bare leaf: true whenever the named signal publishes. The first-class form of
/// "wake on every event of this signal"; lowers onto the wire's `always` comparator.
/// Idiomatic pairing: `fire: if_true`, `rearm: immediate`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BareLeaf {
    /// A local signal nickname declared in `signals`.
    pub signal: Ident,
}

/// A field leaf: one field of one signal compared against a literal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FieldLeaf {
    /// A local signal nickname declared in `signals`.
    pub signal: Ident,
    /// A field of that signal, per its registry entry.
    pub field: Ident,
    /// The comparison operator.
    pub comparator: Comparator,
    /// The literal compared against, type-checked against the field's registry type.
    pub value: LeafValue,
}

/// An expr leaf: arithmetic over subscribed signals' fields, compared against a value.
/// The expression is a body expression restricted to `signal.<nick>.<field>` and
/// `param.*` reads, `envelope.*` constants, decimal literals, arithmetic and the math
/// builtins — a pure function of the snapshot store. No `signal` key: every read is
/// qualified.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExprLeaf {
    /// The expression, e.g. `signal.fair.up - signal.book.ask`.
    pub expr: ExprStr,
    /// The comparison operator.
    pub comparator: Comparator,
    /// The literal compared against, type-checked against the expression's type.
    pub value: LeafValue,
}

/// Strict deserialization mirror for leaf dispatch: every optional shape-discriminating
/// field collected first, then one precise error or one precise leaf.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LeafMirror {
    #[serde(default)]
    signal: Option<Ident>,
    #[serde(default)]
    field: Option<Ident>,
    #[serde(default)]
    expr: Option<ExprStr>,
    #[serde(default)]
    comparator: Option<Comparator>,
    #[serde(default)]
    value: Option<LeafValue>,
}

impl<'de> Deserialize<'de> for ConditionLeaf {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let m = LeafMirror::deserialize(deserializer)?;
        match (m.field, m.expr) {
            (Some(_), Some(_)) => Err(serde::de::Error::custom(
                "a condition leaf carries `field` or `expr`, never both",
            )),
            (Some(field), None) => {
                let Some(signal) = m.signal else {
                    return Err(serde::de::Error::custom(
                        "a field leaf requires `signal`",
                    ));
                };
                match (m.comparator, m.value) {
                    (Some(comparator), Some(value)) => Ok(ConditionLeaf::Field(FieldLeaf {
                        signal,
                        field,
                        comparator,
                        value,
                    })),
                    _ => Err(serde::de::Error::custom(
                        "a field leaf requires `comparator` and `value`",
                    )),
                }
            }
            (None, Some(expr)) => {
                if m.signal.is_some() {
                    return Err(serde::de::Error::custom(
                        "an expr leaf carries no `signal` — its reads are qualified \
                         `signal.<nick>.<field>` inside `expr`",
                    ));
                }
                match (m.comparator, m.value) {
                    (Some(comparator), Some(value)) => Ok(ConditionLeaf::Expr(ExprLeaf {
                        expr,
                        comparator,
                        value,
                    })),
                    _ => Err(serde::de::Error::custom(
                        "an expr leaf requires `comparator` and `value`",
                    )),
                }
            }
            (None, None) => {
                let Some(signal) = m.signal else {
                    return Err(serde::de::Error::custom(
                        "a condition leaf names a `signal` or carries an `expr`",
                    ));
                };
                if m.comparator.is_some() || m.value.is_some() {
                    Err(serde::de::Error::custom(
                        "a bare leaf is `{\"signal\": ...}` alone — add `field` or `expr` to compare",
                    ))
                } else {
                    Ok(ConditionLeaf::Bare(BareLeaf { signal }))
                }
            }
        }
    }
}

/// The comparison operators. `!=` is carried per the 2026-07-20 widening ruling.
/// Ordering comparators require numeric types (validator concern).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum Comparator {
    /// `==`
    #[serde(rename = "==")]
    Eq,
    /// `!=`
    #[serde(rename = "!=")]
    Ne,
    /// `<`
    #[serde(rename = "<")]
    Lt,
    /// `<=`
    #[serde(rename = "<=")]
    Le,
    /// `>`
    #[serde(rename = ">")]
    Gt,
    /// `>=`
    #[serde(rename = ">=")]
    Ge,
}

/// What a leaf compares against: a canonical (optionally signed) decimal or an enum
/// variant written plain (both as bare strings — the leaf's `field` pins the type via
/// the registry, so there is nothing to disambiguate), a string literal in the
/// `{"str": "..."}` object form (bare text would be ambiguous with a variant or a
/// decimal), or a deploy-time param in the `{"param": "<name>"}` object form.
///
/// Rust note: bare strings dispatch on the first character (digit or `-` means decimal)
/// so each side reports its own canonical error instead of untagged's generic mismatch;
/// the two object forms dispatch on their one key.
#[derive(Debug, Clone, PartialEq)]
pub enum LeafValue {
    /// A canonical signed decimal.
    Decimal(SignedDecimalStr),
    /// An enum variant, plain (no quotes).
    Variant(Ident),
    /// A string literal: `{"str": "0xabc..."}`.
    Str(String),
    /// A deploy-time param: `{"param": "target"}`; its declared type must match the
    /// field's.
    Param(Ident),
}

impl Serialize for LeafValue {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        match self {
            LeafValue::Decimal(d) => d.serialize(serializer),
            LeafValue::Variant(v) => v.serialize(serializer),
            LeafValue::Str(s) => {
                let mut m = serializer.serialize_map(Some(1))?;
                m.serialize_entry("str", s)?;
                m.end()
            }
            LeafValue::Param(p) => {
                let mut m = serializer.serialize_map(Some(1))?;
                m.serialize_entry("param", p)?;
                m.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for LeafValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct ObjectForm {
            #[serde(default)]
            str: Option<String>,
            #[serde(default)]
            param: Option<Ident>,
        }
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Bare(String),
            Object(ObjectForm),
        }
        let raw = Raw::deserialize(deserializer).map_err(|_| {
            serde::de::Error::custom(
                "a leaf value is a bare string (canonical decimal or variant), \
                 {\"str\": \"...\"}, or {\"param\": \"<name>\"}",
            )
        })?;
        match raw {
            Raw::Bare(s) => {
                let looks_numeric =
                    s.starts_with('-') || s.bytes().next().is_some_and(|b| b.is_ascii_digit());
                if looks_numeric {
                    SignedDecimalStr::parse(s)
                        .map(LeafValue::Decimal)
                        .map_err(serde::de::Error::custom)
                } else {
                    Ident::parse(s)
                        .map(LeafValue::Variant)
                        .map_err(serde::de::Error::custom)
                }
            }
            Raw::Object(ObjectForm {
                str: Some(s),
                param: None,
            }) => Ok(LeafValue::Str(s)),
            Raw::Object(ObjectForm {
                str: None,
                param: Some(p),
            }) => Ok(LeafValue::Param(p)),
            Raw::Object(_) => Err(serde::de::Error::custom(
                "a leaf value object carries exactly one of `str` or `param`",
            )),
        }
    }
}

impl JsonSchema for LeafValue {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("LeafValue")
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let decimal = generator.subschema_for::<SignedDecimalStr>();
        let variant = generator.subschema_for::<Ident>();
        let param = generator.subschema_for::<Ident>();
        json_schema!({
            "anyOf": [
                decimal,
                variant,
                {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["str"],
                    "properties": { "str": { "type": "string" } },
                    "description": "A string literal, for string-typed fields."
                },
                {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["param"],
                    "properties": { "param": param },
                    "description": "A deploy-time param whose declared type matches the field's."
                }
            ],
            "description": "What the leaf compares against, type-checked against the field/expression type via the registry: a canonical (optionally signed) decimal or an enum variant written plain (bare strings), {\"str\": ...} for string fields, or {\"param\": name}."
        })
    }
}
