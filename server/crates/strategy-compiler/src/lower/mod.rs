//! The lowering stage: project a [`ValidStrategy`] into [`Lowered`], the one in-memory
//! struct every emitter reads from — the program and the manifest are two projections
//! of one lowering result; neither is derived from the other's serialized form.
//!
//! `Lowered` is FLOAT-FREE and string-canonical throughout: every numeric it carries is
//! a canonical decimal string or an integer (millisecond durations). Every body AST it
//! holds is POST-SUBSTITUTION (named expressions inlined), so emitters never see an
//! `expr.*` reference.

pub mod blocks;
pub mod exprs;
pub mod identity;
pub mod intents;
pub mod params;
pub mod state;
pub mod triggers;

use crate::ir::{Envelope, Interpolation};

use crate::validate::ValidStrategy;
use crate::validate::context::EnvelopeField;

pub use blocks::{BlockPlan, PlanItem};
pub use intents::{IntentEntry, IntentIndex, IntentLower, OnTotal, OrderLower};
pub use params::{ParamDefaultLower, ParamKindLower, ParamLower};
pub use state::{DefaultValue, SlotLower};
pub use triggers::{LeafRow, LeafValueLower, RowSource, TimerRow, TriggerRow};

/// The lowering result. Everything the emitters need, in emission-ready order:
/// document order for the array sections (state, triggers, legs), canonical
/// lexicographic order for the name-keyed data (signals, instruments, envelope reads).
#[derive(Debug, Clone, PartialEq)]
pub struct Lowered {
    /// sha256 over the canonical re-serialization of the validated document, 64 hex.
    pub source_hash: String,
    /// `Strategy_` + the first 12 hex of `source_hash`. Same document, same id —
    /// across compiler versions; any document change resets it.
    pub strategy_id: String,
    /// `meta.name`.
    pub meta_name: String,
    /// `meta.description` — the AUTHORED English, carried onto the artifacts.
    pub meta_description: String,
    /// `meta.dsl_version` — the IR version the source document conformed to,
    /// carried onto the artifacts as provenance.
    pub dsl_version: String,
    /// Signal nickname -> resolved id, lexicographic by nickname (`NamedMap` order).
    pub signals: Vec<(String, String)>,
    /// Instrument nickname -> venue id, lexicographic by nickname.
    pub instruments: Vec<(String, String)>,
    /// The resolved signal ids, lexicographic by nickname — the subscription list.
    pub subscriptions: Vec<String>,
    /// Every envelope field the program reads, with its resolved canonical decimal
    /// value, lexicographic by field spelling. The reads compile away entirely: the
    /// program carries the values, and has no runtime envelope-read concept.
    pub envelope_consts: Vec<(EnvelopeField, String)>,
    /// The submission's envelope, carried verbatim onto the artifacts.
    pub envelope: Envelope,
    /// Trigger name -> registration row, document order. ONE row per document-level
    /// trigger — no DNF expansion, no per-leg duplication.
    pub triggers: Vec<(String, TriggerRow)>,
    /// The state slots, document order, with defaults and key metadata precomputed.
    pub state: Vec<SlotLower>,
    /// The deploy-time params, document order, with types and defaults erased to
    /// their kinds.
    pub params: Vec<ParamLower>,
    /// Constant tables: name -> string-canonical columns, lexicographic.
    pub tables: Vec<(String, TableLower)>,
    /// The legs, document order. The first entry is the initial leg.
    pub legs: Vec<LegLower>,
    /// The document's first leg — the initial mode.
    pub initial_leg: String,
    /// `intent_id` -> totalized transition table and resolved bounds.
    pub intent_index: IntentIndex,
}

/// One lowered leg: its arm list and its block plans.
#[derive(Debug, Clone, PartialEq)]
pub struct LegLower {
    /// The leg name.
    pub name: String,
    /// Whether the leg may legitimately idle forever.
    pub resident: bool,
    /// The armed trigger names — a delivery mask, nothing more. Empty is terminal.
    pub arm: Vec<String>,
    /// The block plans, document order.
    pub blocks: Vec<BlockPlan>,
}

/// A constant table, columns as canonical decimal strings.
#[derive(Debug, Clone, PartialEq)]
pub struct TableLower {
    /// The key column (signed domain legal).
    pub keys: Vec<String>,
    /// The value column.
    pub values: Vec<String>,
    /// The interpolation mode.
    pub interpolation: Interpolation,
}

/// Lower a validated strategy. Total: every partial case was ruled out by the
/// validator, and the capability gate ran before this.
pub fn lower(valid: &ValidStrategy) -> Lowered {
    let strategy = valid.strategy();

    let source_hash = identity::source_hash(strategy);
    let strategy_id = identity::strategy_id(&source_hash);

    let signals: Vec<(String, String)> = strategy
        .signals
        .iter()
        .map(|(nick, id)| (nick.as_str().to_string(), id.clone()))
        .collect();
    let instruments: Vec<(String, String)> = strategy
        .instruments
        .iter()
        .map(|(nick, id)| (nick.as_str().to_string(), id.clone()))
        .collect();
    let subscriptions: Vec<String> = signals.iter().map(|(_, id)| id.clone()).collect();

    let envelope = valid.envelope().clone();
    let mut envelope_consts: Vec<(EnvelopeField, String)> = valid
        .envelope_reads()
        .iter()
        .map(|&field| (field, envelope_value(&envelope, field)))
        .collect();
    // `BTreeSet<EnvelopeField>` iterates in variant order; the emission doctrine is
    // canonical LEXICOGRAPHIC order (and `max_open_orders` < `max_order_size`
    // lexicographically, against variant order).
    envelope_consts.sort_by_key(|(field, _)| field.as_str());

    let (bodies, intent_index) = blocks::lower_blocks(valid);
    let legs: Vec<LegLower> = strategy
        .legs
        .iter()
        .zip(bodies)
        .map(|(leg, blocks)| LegLower {
            name: leg.name.as_str().to_string(),
            resident: leg.resident,
            arm: leg.arm.iter().map(|t| t.as_str().to_string()).collect(),
            blocks,
        })
        .collect();
    let initial_leg = legs
        .first()
        .expect("the validator rejects a document with no legs")
        .name
        .clone();

    let mut tables = Vec::new();
    if let Some(expressions) = &strategy.expressions
        && let Some(table_map) = &expressions.tables
    {
        for (name, table) in table_map.iter() {
            tables.push((
                name.as_str().to_string(),
                TableLower {
                    keys: table.keys.iter().map(|k| k.as_str().to_string()).collect(),
                    values: table
                        .values
                        .iter()
                        .map(|v| v.as_str().to_string())
                        .collect(),
                    interpolation: table.interpolation,
                },
            ));
        }
    }

    Lowered {
        source_hash,
        strategy_id,
        meta_name: strategy.meta.name.clone(),
        meta_description: strategy.meta.description.clone(),
        dsl_version: strategy.meta.dsl_version.as_str().to_string(),
        signals,
        instruments,
        subscriptions,
        envelope_consts,
        envelope,
        triggers: triggers::lower_triggers(valid),
        state: state::lower_state(&strategy.state),
        params: params::lower_params(&strategy.params),
        tables,
        legs,
        initial_leg,
        intent_index,
    }
}

/// The resolved decimal value of a readable envelope field.
///
/// The optional fields unwrap on the validator's guarantee: a program that reads an
/// absent field failed validation (`envelope-missing-read-field`), so a read set entry
/// implies presence.
fn envelope_value(envelope: &Envelope, field: EnvelopeField) -> String {
    let value = match field {
        EnvelopeField::Allocation => &envelope.allocation,
        EnvelopeField::LossBudget => &envelope.loss_budget,
        EnvelopeField::RoundBudget => envelope
            .round_budget
            .as_ref()
            .expect("validator guarantees read envelope fields are present"),
        EnvelopeField::MaxOrderSize => &envelope.max_order_size,
        EnvelopeField::MaxPositionPerMarket => envelope
            .max_position_per_market
            .as_ref()
            .expect("validator guarantees read envelope fields are present"),
        EnvelopeField::MaxOpenOrders => envelope
            .max_open_orders
            .as_ref()
            .expect("validator guarantees read envelope fields are present"),
    };
    value.as_str().to_string()
}
