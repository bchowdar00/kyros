//! `NamedMap<T>` — the name-keyed map shape shared by `instruments`, `expressions.named`,
//! and `expressions.tables`.
//!
//! Doctrine (ir-schema-notes.md, document conventions): name-keyed maps are legal only for
//! sections whose values have no object body to lose, and the parser must reject duplicate
//! keys — serde's default map handling is last-wins, which would make a duplicated name
//! silently shadow an earlier one. The custom `Deserialize` below makes that illegal state
//! unrepresentable at the type level: a duplicate key is a deserialization error, not a
//! quiet overwrite. (The "collected diagnostics" refinement needs a streaming pre-scan and
//! belongs to the compiler's parse stage; here the first duplicate fails fast.)

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt;
use std::marker::PhantomData;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::de::{MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

use crate::ir::scalars::{IDENT_PATTERN, Ident};

/// An ordered name-keyed map with identifier keys and duplicate-key rejection.
///
/// Backed by a `BTreeMap` so iteration order is deterministic (lexicographic by name) —
/// per the determinism invariant, no map whose iteration order can reach an artifact may
/// be hash-based.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(transparent)]
pub struct NamedMap<T>(BTreeMap<Ident, T>);

impl<T> NamedMap<T> {
    /// Iterate entries in deterministic (lexicographic) name order.
    pub fn iter(&self) -> impl Iterator<Item = (&Ident, &T)> {
        self.0.iter()
    }

    /// Look up an entry by name.
    pub fn get(&self, name: &str) -> Option<&T> {
        self.0.get(name)
    }

    /// Whether an entry with this name exists.
    pub fn contains_key(&self, name: &str) -> bool {
        self.0.contains_key(name)
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the map has no entries.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for NamedMap<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct MapVisitor<T>(PhantomData<T>);

        impl<'de, T: Deserialize<'de>> Visitor<'de> for MapVisitor<T> {
            type Value = NamedMap<T>;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a name-keyed map with unique identifier keys")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
                let mut entries = BTreeMap::new();
                // serde_json streams map entries without deduplicating, so every key
                // occurrence — including duplicates — reaches this loop.
                while let Some(key) = access.next_key::<Ident>()? {
                    let value = access.next_value::<T>()?;
                    if entries.insert(key.clone(), value).is_some() {
                        return Err(serde::de::Error::custom(format_args!(
                            "duplicate key `{key}` in name-keyed map"
                        )));
                    }
                }
                Ok(NamedMap(entries))
            }
        }

        deserializer.deserialize_map(MapVisitor(PhantomData))
    }
}

impl<T: JsonSchema> JsonSchema for NamedMap<T> {
    fn schema_name() -> Cow<'static, str> {
        Cow::Owned(format!("NamedMapOf{}", T::schema_name()))
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let value_schema = generator.subschema_for::<T>();
        let mut schema = json_schema!({
            "type": "object",
            "additionalProperties": false,
            "description": "Name-keyed map with unique identifier keys. Duplicate keys are a parse error, never last-wins."
        });
        schema.ensure_object().insert(
            "patternProperties".to_owned(),
            serde_json::json!({ IDENT_PATTERN: value_schema }),
        );
        schema
    }
}
