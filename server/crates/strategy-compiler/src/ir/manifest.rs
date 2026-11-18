//! Manifest types: the wire strategy-schema dialect the compiler emits.
//!
//! Serde-only — the manifest is compiler OUTPUT, never part of the LLM-facing schema,
//! so nothing here derives `JsonSchema`. Shapes follow the
//! PINNED wire schemas (`tests/schemas/`: `strategy-schema.schema.json` v2 and
//! `common.schema.json`; the pin commit is recorded in `emit::manifest::SCHEMA_PIN`).
//! The conformance test validates every emitted manifest against exactly those files.
//!
//! Not carried here (deliberately): `state_layout`, `payload_layouts`, `layout_hash`,
//! `scratch_bytes`, and the cost stamps — the layout stage's output, which does not
//! exist yet; the schema reserves their fields. Price bands never cross the wire
//! (compile-time guarantee only), and the authored expr-leaf string is display-side
//! only — the wire carries the pre-parsed op-tree.

use std::collections::BTreeMap;

use serde::ser::{SerializeMap, SerializeStruct};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::ir::envelope::Envelope;
use crate::ir::trigger::{Fire, RearmMode};

/// The manifest document: the venue-facing half of the bundle.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// Wire schema version. 2 = the pinned shape: document-level triggers,
    /// arm-list lifecycle stages, intent index, v0.2 envelope.
    pub schema_version: u32,
    /// The artifact's stable id (`Strategy_<12hex>`), derived from the source hash.
    pub strategy_id: String,
    /// Display name from `meta.name` — audit/UI context, not an identifier.
    pub name: String,
    /// The AUTHORED description from `meta.description` — the user-confirmed English.
    pub description: String,
    /// The IR dsl_version the source document conformed to (`"0.3"`).
    pub dsl_version: String,
    /// sha256 (lowercase hex) over the canonical serialization of the validated
    /// source document; `strategy_id` embeds its first 12 hex.
    pub source_hash: String,
    /// Per-signal subscription rows, lexicographic by resolved id.
    pub signals: Vec<SubscriptionRow>,
    /// Every (signal, field) the strategy's triggers and body read, deduped, sorted.
    pub watch_fields: Vec<WatchField>,
    /// The document-level trigger table: one row per document trigger, document
    /// order — one edge/rearm state each, registered once, evaluated continuously.
    pub triggers: Vec<WireTrigger>,
    /// The leg machine: named delivery masks over the trigger table. The first
    /// entry is the initial leg.
    pub lifecycle_stages: Vec<LifecycleStage>,
    /// `intent_id` -> totalized transition table and resolved bounds, executed
    /// mechanically by the runner (no strategy code runs on order events).
    pub intent_index: BTreeMap<String, WireIntentEntry>,
    /// The submission envelope, verbatim in its v0.2 shape (ir-spec §12): enforced
    /// by the risk gate reading this, never the compiled code.
    pub envelope: Envelope,
}

/// One subscription row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubscriptionRow {
    /// The resolved signal id.
    pub signal_id: String,
    /// v0.2 killed staleness policies (ir-spec §15); `proceed` (deliver the latest
    /// value regardless of age) is the one semantic the activation snapshot model
    /// has, so it is the only variant emitted.
    pub staleness_policy: StalenessPolicy,
}

/// The staleness policy — v0.2 emits only `proceed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StalenessPolicy {
    /// Deliver the last value regardless of age.
    Proceed,
}

/// One watched (signal, field) pair.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WatchField {
    /// The resolved signal id.
    pub signal_id: String,
    /// The field name, per the signal's registry entry.
    pub field: String,
}

/// One leg of the machine: a named delivery mask over the document-level triggers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LifecycleStage {
    /// The leg name (`stay` is reserved).
    pub name: String,
    /// Whether the leg may legitimately idle forever.
    pub resident: bool,
    /// Ids of the document-level triggers delivered while this leg is active —
    /// a mask, nothing more; arming never registers, resets, or rewinds trigger
    /// bookkeeping (timers excepted, per-arming by ratified IR semantics). Empty
    /// is a terminal leg.
    pub arm: Vec<String>,
}

/// One intent index entry: what the runner snapshots onto the order at placement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireIntentEntry {
    /// The totalized transition table.
    pub on: WireOnTable,
    /// The resolved per-intent size bound, canonical decimal string — the risk
    /// path enforces this without trusting the code's arithmetic.
    pub max_size: String,
}

/// The five-key totalized transition table: every key present, absent authored
/// keys materialized as `stay`. Each value is `stay` or a declared leg name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireOnTable {
    /// Destination on full fill.
    pub fill: String,
    /// Destination on partial fill.
    pub partial: String,
    /// Destination on venue rejection.
    pub reject: String,
    /// Destination on cancel acknowledgement.
    pub cancel_ack: String,
    /// Destination on expiry.
    pub expiry: String,
}

/// A wire trigger: exactly one of a condition tree or a timer, plus edge semantics.
///
/// Rust note: the exactly-one invariant cannot be expressed by derived serde on a
/// flat struct, so like [`crate::ir::Trigger`] this deserializes through a strict mirror
/// and serializes by hand.
#[derive(Debug, Clone, PartialEq)]
pub struct WireTrigger {
    /// The trigger's id — the document-level trigger name.
    pub id: String,
    /// The wake source.
    pub source: WireTriggerSource,
    /// The fire mode (wire spelling matches the IR's).
    pub fire: Fire,
    /// The rearm policy.
    pub rearm: WireRearm,
}

/// The wire rearm policy: the mode plus an optional cooldown in milliseconds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireRearm {
    /// The rearm mode.
    pub mode: RearmMode,
    /// Minimum gap between fires, milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_ms: Option<u64>,
}

/// The two wire trigger families.
#[derive(Debug, Clone, PartialEq)]
pub enum WireTriggerSource {
    /// An author-shaped condition tree over resolved signal ids.
    Condition(WireCondition),
    /// A timer spec.
    Timer(WireTimer),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireTriggerMirror {
    id: String,
    #[serde(default)]
    condition: Option<WireCondition>,
    #[serde(default)]
    timer: Option<WireTimer>,
    fire: Fire,
    rearm: WireRearm,
}

impl<'de> Deserialize<'de> for WireTrigger {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mirror = WireTriggerMirror::deserialize(deserializer)?;
        let source = match (mirror.condition, mirror.timer) {
            (Some(condition), None) => WireTriggerSource::Condition(condition),
            (None, Some(timer)) => WireTriggerSource::Timer(timer),
            _ => {
                return Err(serde::de::Error::custom(
                    "a wire trigger carries exactly one of `condition` and `timer`",
                ));
            }
        };
        Ok(WireTrigger {
            id: mirror.id,
            source,
            fire: mirror.fire,
            rearm: mirror.rearm,
        })
    }
}

impl Serialize for WireTrigger {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut s = serializer.serialize_struct("WireTrigger", 4)?;
        s.serialize_field("id", &self.id)?;
        match &self.source {
            WireTriggerSource::Condition(condition) => s.serialize_field("condition", condition)?,
            WireTriggerSource::Timer(timer) => s.serialize_field("timer", timer)?,
        }
        s.serialize_field("fire", &self.fire)?;
        s.serialize_field("rearm", &self.rearm)?;
        s.end()
    }
}

/// An author-shaped condition tree: a leaf, or one `all`/`any` group. The wire
/// permits unbounded nesting; v0.2 documents only produce depth-1 trees.
///
/// Rust note: the three shapes' JSON key sets are disjoint, so `untagged` round-trips
/// unambiguously.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum WireCondition {
    /// A single leaf.
    Leaf(WireLeaf),
    /// AND: all children hold at the same moment.
    All {
        /// The conjuncts.
        all: Vec<WireCondition>,
    },
    /// OR: stateless disjunction.
    Any {
        /// The disjuncts.
        any: Vec<WireCondition>,
    },
}

/// One condition leaf, signal nickname resolved to its id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireLeaf {
    /// The resolved signal id.
    pub signal_id: String,
    /// The field name; absent on bare and expr leaves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    /// The pre-parsed op-tree of an expr leaf; absent on bare and field leaves.
    /// The authored expression string never crosses the wire — it lives in the
    /// source document and its decompilation only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expr: Option<WireExpr>,
    /// The comparator; `always` for bare leaves.
    pub comparator: WireComparator,
    /// The literal, string-canonical (wire SignalValue: decimals as strings);
    /// absent on bare leaves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
}

/// The pre-parsed expr-leaf op-tree (`common.schema.json#/$defs/ExprOpTree`):
/// arithmetic over the leaf's own signal fields. The platform never parses strings.
///
/// Rust note: the four shapes' key sets are disjoint, but serde's `untagged` cannot
/// reject unknown keys, so deserialization goes through a strict mirror (matching
/// this file's exactly-one style) and serialization is by hand.
#[derive(Debug, Clone, PartialEq)]
pub enum WireExpr {
    /// `{"field": "<name>"}` — a field of the leaf's own signal.
    Field(String),
    /// `{"const": "<canonical decimal>"}`.
    Const(String),
    /// `{"op": "+|-|*|/", "lhs": ..., "rhs": ...}`.
    Bin {
        /// The arithmetic operator.
        op: WireArithOp,
        /// Left operand.
        lhs: Box<WireExpr>,
        /// Right operand.
        rhs: Box<WireExpr>,
    },
    /// `{"op": "neg", "arg": ...}` — unary minus.
    Neg(Box<WireExpr>),
    /// `{"source": "<authored expression>"}` — a v0.3 expression the op-tree cannot
    /// express (several signals, builtins, params). The program carries the real
    /// tree; the manifest keeps the authored text for display.
    Source(String),
}

/// The wire arithmetic operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WireArithOp {
    /// `+`
    #[serde(rename = "+")]
    Add,
    /// `-`
    #[serde(rename = "-")]
    Sub,
    /// `*`
    #[serde(rename = "*")]
    Mul,
    /// `/`
    #[serde(rename = "/")]
    Div,
}

impl Serialize for WireExpr {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            WireExpr::Field(field) => {
                let mut m = serializer.serialize_map(Some(1))?;
                m.serialize_entry("field", field)?;
                m.end()
            }
            WireExpr::Const(value) => {
                let mut m = serializer.serialize_map(Some(1))?;
                m.serialize_entry("const", value)?;
                m.end()
            }
            WireExpr::Bin { op, lhs, rhs } => {
                let mut m = serializer.serialize_map(Some(3))?;
                m.serialize_entry("op", op)?;
                m.serialize_entry("lhs", lhs)?;
                m.serialize_entry("rhs", rhs)?;
                m.end()
            }
            WireExpr::Neg(arg) => {
                let mut m = serializer.serialize_map(Some(2))?;
                m.serialize_entry("op", "neg")?;
                m.serialize_entry("arg", arg)?;
                m.end()
            }
            WireExpr::Source(source) => {
                let mut m = serializer.serialize_map(Some(1))?;
                m.serialize_entry("source", source)?;
                m.end()
            }
        }
    }
}

/// Strict deserialization mirror for op-tree dispatch: every shape-discriminating
/// field collected first, then one precise error or one precise node.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireExprMirror {
    #[serde(default)]
    field: Option<String>,
    #[serde(default, rename = "const")]
    constant: Option<String>,
    #[serde(default)]
    op: Option<String>,
    #[serde(default)]
    lhs: Option<Box<WireExpr>>,
    #[serde(default)]
    rhs: Option<Box<WireExpr>>,
    #[serde(default)]
    arg: Option<Box<WireExpr>>,
    #[serde(default)]
    source: Option<String>,
}

impl<'de> Deserialize<'de> for WireExpr {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let m = WireExprMirror::deserialize(deserializer)?;
        if let Some(source) = m.source {
            if m.field.is_some() || m.constant.is_some() || m.op.is_some()
                || m.lhs.is_some() || m.rhs.is_some() || m.arg.is_some()
            {
                return Err(serde::de::Error::custom(
                    "a source node is `{\"source\": ...}` alone",
                ));
            }
            return Ok(WireExpr::Source(source));
        }
        match (m.field, m.constant, m.op) {
            (Some(field), None, None) => {
                if m.lhs.is_some() || m.rhs.is_some() || m.arg.is_some() {
                    return Err(serde::de::Error::custom(
                        "a field node is `{\"field\": ...}` alone",
                    ));
                }
                Ok(WireExpr::Field(field))
            }
            (None, Some(constant), None) => {
                if m.lhs.is_some() || m.rhs.is_some() || m.arg.is_some() {
                    return Err(serde::de::Error::custom(
                        "a const node is `{\"const\": ...}` alone",
                    ));
                }
                Ok(WireExpr::Const(constant))
            }
            (None, None, Some(op)) => match op.as_str() {
                "neg" => match (m.arg, m.lhs, m.rhs) {
                    (Some(arg), None, None) => Ok(WireExpr::Neg(arg)),
                    _ => Err(serde::de::Error::custom(
                        "a neg node carries `arg` and nothing else",
                    )),
                },
                "+" | "-" | "*" | "/" => {
                    let arith = match op.as_str() {
                        "+" => WireArithOp::Add,
                        "-" => WireArithOp::Sub,
                        "*" => WireArithOp::Mul,
                        _ => WireArithOp::Div,
                    };
                    match (m.lhs, m.rhs, m.arg) {
                        (Some(lhs), Some(rhs), None) => Ok(WireExpr::Bin {
                            op: arith,
                            lhs,
                            rhs,
                        }),
                        _ => Err(serde::de::Error::custom(
                            "a binary node carries `lhs` and `rhs` and no `arg`",
                        )),
                    }
                }
                other => Err(serde::de::Error::custom(format_args!(
                    "unknown op-tree operator `{other}`"
                ))),
            },
            _ => Err(serde::de::Error::custom(
                "an op-tree node is exactly one of field, const, or op",
            )),
        }
    }
}

/// The wire comparator set, including `always` (the bare leaf's lowering).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WireComparator {
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
    /// True whenever the signal publishes (bare leaves).
    #[serde(rename = "always")]
    Always,
}

/// A wire timer: discriminant plus the matching per-kind parameters (exactly the
/// kind's own, per the schema's per-kind coupling).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireTimer {
    /// The timer kind.
    pub kind: WireTimerKind,
    /// `kind=after`: milliseconds from arming.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delay_ms: Option<u64>,
    /// `kind=interval`: the period, milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub period_ms: Option<u64>,
    /// `kind=window_clock` parameters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_clock: Option<WindowClockParams>,
}

/// The timer kinds the compiler emits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireTimerKind {
    /// Fires when the instrument's window clock reaches the remaining threshold.
    WindowClock,
    /// Fires once, `delay_ms` after arming.
    After,
    /// Fires every `period_ms` while armed.
    Interval,
}

/// `window_clock` parameters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowClockParams {
    /// The resolved venue instrument id.
    pub instrument_id: String,
    /// The threshold before window close, milliseconds.
    pub remaining_ms: u64,
}
