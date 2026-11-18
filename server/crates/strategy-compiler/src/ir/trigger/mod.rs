//! The `triggers` section: authored, named wake conditions.
//!
//! Nothing wakes a strategy except a fired trigger. Each entry carries exactly one of a
//! `condition` (a ConditionTree the SME evaluates) or a `timer` (a TimerSpec), plus the
//! `fire`/`rearm` edge semantics — which is where ALL edge detection lives; the v0.1
//! `crossed()` builtin's prior-truth machinery is now the SME's per-trigger bookkeeping.

pub mod condition;
pub mod timer;

use std::borrow::Cow;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::ser::SerializeStruct;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::ir::scalars::{DurationStr, Ident};
use condition::Condition;
use timer::TimerSpec;

/// One authored trigger: a named wake source with edge semantics.
///
/// The document spelling is flat — `{"name", "condition" | "timer", "fire", "rearm"}` —
/// with exactly one of `condition`/`timer` present. That exactly-one invariant cannot be
/// expressed by derived serde on a flat struct, so this type deserializes through a
/// strict mirror (see `TriggerMirror`) and serializes by hand.
#[derive(Debug, Clone, PartialEq)]
pub struct Trigger {
    /// The trigger's unique name, referenced by `arm` lists and `triggered(NAME)`.
    pub name: Ident,
    /// The wake source: an SME-evaluable condition or a timer.
    pub source: TriggerSource,
    /// When the trigger fires relative to its condition's truth.
    pub fire: Fire,
    /// When the trigger may fire again.
    pub rearm: Rearm,
}

/// The two wake-source families.
#[derive(Debug, Clone, PartialEq)]
pub enum TriggerSource {
    /// A ConditionTree over declared signals, evaluated by the SME with zero strategy
    /// state.
    Condition(Condition),
    /// A timer, armed at leg entry and disarmed at leg exit.
    Timer(TimerSpec),
}

/// Strict deserialization mirror: both sources optional, unknown fields rejected; the
/// exactly-one check happens after field collection so the error can say precisely what
/// is wrong.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TriggerMirror {
    name: Ident,
    #[serde(default)]
    condition: Option<Condition>,
    #[serde(default)]
    timer: Option<TimerSpec>,
    fire: Fire,
    rearm: Rearm,
}

impl<'de> Deserialize<'de> for Trigger {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mirror = TriggerMirror::deserialize(deserializer)?;
        let source = match (mirror.condition, mirror.timer) {
            (Some(condition), None) => TriggerSource::Condition(condition),
            (None, Some(timer)) => TriggerSource::Timer(timer),
            (Some(_), Some(_)) => {
                return Err(serde::de::Error::custom(format_args!(
                    "trigger `{}` has both `condition` and `timer` — a trigger carries exactly one",
                    mirror.name
                )));
            }
            (None, None) => {
                return Err(serde::de::Error::custom(format_args!(
                    "trigger `{}` has neither `condition` nor `timer` — a trigger carries exactly one",
                    mirror.name
                )));
            }
        };
        Ok(Trigger {
            name: mirror.name,
            source,
            fire: mirror.fire,
            rearm: mirror.rearm,
        })
    }
}

impl Serialize for Trigger {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut s = serializer.serialize_struct("Trigger", 4)?;
        s.serialize_field("name", &self.name)?;
        match &self.source {
            TriggerSource::Condition(condition) => s.serialize_field("condition", condition)?,
            TriggerSource::Timer(timer) => s.serialize_field("timer", timer)?,
        }
        s.serialize_field("fire", &self.fire)?;
        s.serialize_field("rearm", &self.rearm)?;
        s.end()
    }
}

impl JsonSchema for Trigger {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("Trigger")
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let name = generator.subschema_for::<Ident>();
        let condition = generator.subschema_for::<Condition>();
        let timer = generator.subschema_for::<TimerSpec>();
        let fire = generator.subschema_for::<Fire>();
        let rearm = generator.subschema_for::<Rearm>();
        json_schema!({
            "oneOf": [
                {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["name", "condition", "fire", "rearm"],
                    "properties": {
                        "name": name.clone(),
                        "condition": condition,
                        "fire": fire.clone(),
                        "rearm": rearm.clone()
                    }
                },
                {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["name", "timer", "fire", "rearm"],
                    "properties": {
                        "name": name,
                        "timer": timer,
                        "fire": fire,
                        "rearm": rearm
                    }
                }
            ],
            "description": "An authored wake source: exactly one of a condition or a timer, plus fire/rearm edge semantics."
        })
    }
}

/// When a trigger fires relative to its condition's truth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Fire {
    /// Only on the rising edge: the condition's overall truth went false -> true. A
    /// condition already true when the trigger comes live does not fire until it
    /// falsifies and crosses again.
    OnCrossing,
    /// On every evaluation while the condition is true, including the first evaluation
    /// after coming live. The fire-mode idiom pairs this with state-refined guards.
    IfTrue,
}

/// The rearm policy: when a fired trigger may fire again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Rearm {
    /// The rearm mode.
    pub mode: RearmMode,
    /// Optional minimum gap between fires, on top of the mode. Evaluated lazily by the
    /// SME against event timestamps — never a scheduled timer. A fire suppressed by the
    /// cooldown still advances the trigger's bookkeeping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown: Option<DurationStr>,
}

/// Rearm modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RearmMode {
    /// May not re-fire until the condition has gone false and then true again.
    FreshCrossing,
    /// May re-fire while still true.
    Immediate,
    /// Fires at most once.
    Never,
}
