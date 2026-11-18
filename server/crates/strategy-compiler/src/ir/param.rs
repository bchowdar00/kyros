//! The `params` section: deploy-time knobs.
//!
//! A param is a typed value the user sets at deploy and may change on a running
//! strategy without recompiling: an address to follow, an edge threshold, a size
//! fraction. Readable as `param.<name>` in guards, statements, order expressions and
//! condition leaves. Params are NOT envelope fields: they carry no risk meaning, and
//! the magnitude rule still requires every size to derive from `envelope.*`. Their
//! values are runtime state, never compile-time constants, so they survive in the
//! program as reads — which is exactly what lets a live strategy be re-parameterized
//! without changing its id.

use std::borrow::Cow;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::ir::scalars::Ident;
use crate::ir::type_spec::TypeSpec;

/// One declared param.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Param {
    /// The name the body reads (`param.<name>`).
    pub name: Ident,
    /// The value type, in either `TypeSpec` spelling (`"decimal"`, `"string"`,
    /// `{"type": "enum", "variants": [...]}`, ...).
    #[serde(rename = "type")]
    pub type_spec: TypeSpec,
    /// The value the strategy deploys with unless the user overrides it. Checked
    /// against `type` by the validator.
    pub default: ParamValue,
}

/// A param's default: a JSON boolean for `bool` params, otherwise a string holding the
/// canonical decimal, the variant name, or the text — whichever the declared type
/// expects. The validator checks the pairing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParamValue {
    /// A boolean (JSON `true` / `false`).
    Bool(bool),
    /// A decimal, variant or string, as text.
    Text(String),
}

impl ParamValue {
    /// The value's source text, for messages.
    pub fn render(&self) -> String {
        match self {
            ParamValue::Bool(b) => b.to_string(),
            ParamValue::Text(t) => t.clone(),
        }
    }
}

impl Serialize for ParamValue {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            ParamValue::Bool(b) => serializer.serialize_bool(*b),
            ParamValue::Text(t) => serializer.serialize_str(t),
        }
    }
}

impl<'de> Deserialize<'de> for ParamValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Bool(bool),
            Text(String),
        }
        match Raw::deserialize(deserializer) {
            Ok(Raw::Bool(b)) => Ok(ParamValue::Bool(b)),
            Ok(Raw::Text(t)) => Ok(ParamValue::Text(t)),
            Err(_) => Err(serde::de::Error::custom(
                "a param default is a JSON boolean (bool params) or a string (the canonical \
                 decimal, the variant name, or the text)",
            )),
        }
    }
}

impl JsonSchema for ParamValue {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("ParamValue")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": ["string", "boolean"],
            "description": "The param's default: a JSON boolean for bool params; otherwise a string holding the canonical decimal, the enum variant name, or the text."
        })
    }
}
