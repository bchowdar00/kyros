//! The on table: order-outcome transitions, executed mechanically by the runner.
//!
//! An on-table transition is NOT an activation — the runner flips the leg pointer when
//! the order event arrives; no strategy code runs. The table belongs to the ORDER and
//! survives leg changes made after placement. It moves the leg pointer, never trigger
//! state: a walk-back destination whose re-entry depends on a rearm-parked trigger is a
//! one-shot, not a retry.

use std::borrow::Cow;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::ir::scalars::Ident;

/// Transitions over the order-event alphabet. Absent keys default to `stay` (sparse-in:
/// never write a default); the compiler totalizes over the full alphabet and the
/// decompilation discloses all of it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OnTable {
    /// Destination when the order fully fills.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fill: Option<OnDest>,
    /// Destination on a partial fill.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partial: Option<OnDest>,
    /// Destination on venue rejection. Note: an unfilled IOC dies as an
    /// expiry/cancel-ack, not a reject — a walk-back idiom meaning "the attempt failed"
    /// must list every key it intends to cover.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reject: Option<OnDest>,
    /// Destination when a cancellation is acknowledged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancel_ack: Option<OnDest>,
    /// Destination when the order expires.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expiry: Option<OnDest>,
}

/// An on-table destination: a declared leg name, or `stay` — no transition, including
/// when the outcome lands after the strategy has already moved legs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OnDest {
    /// No transition.
    Stay,
    /// Flip the leg pointer to the named leg.
    Leg(Ident),
}

impl Serialize for OnDest {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            OnDest::Stay => serializer.serialize_str("stay"),
            OnDest::Leg(leg) => leg.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for OnDest {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        // `stay` is a reserved leg name precisely so this dispatch cannot collide.
        if s == "stay" {
            Ok(OnDest::Stay)
        } else {
            Ident::parse(s)
                .map(OnDest::Leg)
                .map_err(serde::de::Error::custom)
        }
    }
}

impl JsonSchema for OnDest {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("OnDest")
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let leg = generator.subschema_for::<Ident>();
        json_schema!({
            "anyOf": [
                { "const": "stay" },
                leg
            ],
            "description": "A declared leg name, or 'stay' (no transition — the default for every key; sparse-in says omit it)."
        })
    }
}
