//! The `meta` section: display name, authored description, and DSL version.

use std::borrow::Cow;
use std::fmt;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Deserializer, Serialize};

/// Document metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Meta {
    /// Display English, not an identifier — nothing references it.
    pub name: String,
    /// AUTHORED English, distinct from the decompiler's DERIVED English; comparing the
    /// two is how the user confirms the compiler understood them.
    pub description: String,
    /// Which IR schema version the document was written against. Must be `"0.3"`.
    pub dsl_version: DslVersion,
}

/// The DSL version marker. Exactly `"0.3"` — the version gates the parser at the door
/// (hard cutover, no compatibility shims), so any other value is a deserialization
/// error, not a validator diagnostic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct DslVersion(String);

/// The one version this crate speaks.
const DSL_VERSION: &str = "0.3";

impl DslVersion {
    /// The version as a string slice (always `"0.3"`).
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DslVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for DslVersion {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        if s == DSL_VERSION {
            Ok(DslVersion(s))
        } else {
            Err(serde::de::Error::custom(format_args!(
                "unsupported dsl_version `{s}` (this compiler speaks `{DSL_VERSION}`; hard cutover, no compatibility shims)"
            )))
        }
    }
}

impl JsonSchema for DslVersion {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("DslVersion")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "const": DSL_VERSION,
            "description": "The IR schema version this document is written against."
        })
    }
}
