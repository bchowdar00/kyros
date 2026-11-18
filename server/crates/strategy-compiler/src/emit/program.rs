//! The program projection: [`Lowered`] to [`Program`], the runtime's artifact.
//!
//! Mechanical and byte-deterministic. Registration rows, block plans and the intent
//! index cross over one-to-one; body expressions lower through
//! [`crate::lower::exprs`]; trigger expr leaves arrive already lowered to the same
//! [`ExprNode`] shape, so the runtime has one evaluator for both. Nothing here is
//! float: decimals stay canonical strings.

use std::collections::BTreeMap;

use crate::lower::exprs::{ExprCtx, lower_expr};
use crate::lower::{
    BlockPlan, DefaultValue, IntentEntry, IntentLower, LeafRow, LeafValueLower, Lowered,
    OrderLower, ParamDefaultLower, ParamKindLower, ParamLower, PlanItem, RowSource, SlotLower,
    TimerRow, TriggerRow,
};
use crate::program::{
    BlockDef, ExprNode, FORMAT, IntentDef, Item, Leaf, LeafValue, LegDef, OnTable, OrderDef,
    ParamDef, ParamType, Program, SignalRead, StateSlot, TableDef, TriggerDef, TriggerSource,
};

/// Serialize the program: pretty JSON plus a trailing newline, byte-identical for the
/// same lowering result and compiler version.
pub fn emit_program(lowered: &Lowered, compiler_version: &str) -> String {
    serialize_program(&project_program(lowered, compiler_version))
}

/// Serialize an already-projected program in the emitted form.
pub fn serialize_program(program: &Program) -> String {
    let mut text = serde_json::to_string_pretty(program)
        .expect("program types serialize infallibly: string-keyed, no floats");
    text.push('\n');
    text
}

/// Project the lowering result into the program types.
pub fn project_program(lowered: &Lowered, compiler_version: &str) -> Program {
    let signal_ids: BTreeMap<String, String> = lowered.signals.iter().cloned().collect();
    let ctx = ExprCtx {
        signal_ids: &signal_ids,
        envelope: &lowered.envelope,
    };

    Program {
        format: FORMAT.to_string(),
        compiler_version: compiler_version.to_string(),
        strategy_id: lowered.strategy_id.clone(),
        source_hash: lowered.source_hash.clone(),
        name: lowered.meta_name.clone(),
        description: lowered.meta_description.clone(),
        dsl_version: lowered.dsl_version.clone(),
        signals: signal_ids.clone(),
        instruments: lowered.instruments.iter().cloned().collect(),
        subscriptions: lowered.subscriptions.clone(),
        envelope: lowered.envelope.clone(),
        state: lowered.state.iter().map(state_slot).collect(),
        params: lowered.params.iter().map(param_def).collect(),
        triggers: lowered
            .triggers
            .iter()
            .map(|(name, row)| trigger(name, row))
            .collect(),
        legs: lowered
            .legs
            .iter()
            .map(|leg| LegDef {
                name: leg.name.clone(),
                resident: leg.resident,
                arm: leg.arm.clone(),
                blocks: leg
                    .blocks
                    .iter()
                    .map(|plan| block(plan, &signal_ids, &ctx))
                    .collect(),
            })
            .collect(),
        initial_leg: lowered.initial_leg.clone(),
        intents: lowered
            .intent_index
            .iter()
            .map(|(id, entry)| (id.clone(), intent(entry)))
            .collect(),
        tables: lowered
            .tables
            .iter()
            .map(|(name, table)| {
                (
                    name.clone(),
                    TableDef {
                        keys: table.keys.clone(),
                        values: table.values.clone(),
                        interpolation: table.interpolation,
                    },
                )
            })
            .collect(),
    }
}

fn state_slot(slot: &SlotLower) -> StateSlot {
    match slot {
        SlotLower::Scalar { name, default } => StateSlot::Scalar {
            name: name.clone(),
            default: default_node(default),
        },
        SlotLower::Window { name, capacity } => StateSlot::Window {
            name: name.clone(),
            capacity: *capacity,
        },
        SlotLower::Map {
            name,
            key_numeric,
            value_default,
            max_keys,
            overflow,
        } => StateSlot::Map {
            name: name.clone(),
            key_arity: key_numeric.len(),
            default: default_node(value_default),
            max_keys: *max_keys,
            overflow: *overflow,
        },
    }
}

/// A slot default as a literal node: zero for numerics, `false` for booleans, the
/// variant for enums.
fn default_node(default: &DefaultValue) -> ExprNode {
    match default {
        DefaultValue::Zero => ExprNode::Decimal {
            value: "0".to_string(),
        },
        DefaultValue::False => ExprNode::Bool { value: false },
        DefaultValue::EmptyStr => ExprNode::Str {
            value: String::new(),
        },
        DefaultValue::Variant(v) => ExprNode::Variant { value: v.clone() },
    }
}

fn param_def(param: &ParamLower) -> ParamDef {
    let param_type = match &param.kind {
        ParamKindLower::U8 => ParamType::U8,
        ParamKindLower::Decimal => ParamType::Decimal,
        ParamKindLower::Bool => ParamType::Bool,
        ParamKindLower::Str => ParamType::String,
        ParamKindLower::Enum(variants) => ParamType::Enum {
            variants: variants.clone(),
        },
    };
    let default = match &param.default {
        ParamDefaultLower::Decimal(d) => ExprNode::Decimal { value: d.clone() },
        ParamDefaultLower::Bool(b) => ExprNode::Bool { value: *b },
        ParamDefaultLower::Variant(v) => ExprNode::Variant { value: v.clone() },
        ParamDefaultLower::Str(s) => ExprNode::Str { value: s.clone() },
    };
    ParamDef {
        name: param.name.clone(),
        param_type,
        default,
    }
}

fn leaf_value(value: &LeafValueLower) -> LeafValue {
    match value {
        LeafValueLower::Literal(value) => LeafValue::Literal {
            value: value.clone(),
        },
        LeafValueLower::Param(name) => LeafValue::Param { name: name.clone() },
    }
}

fn trigger(name: &str, row: &TriggerRow) -> TriggerDef {
    let source = match &row.source {
        RowSource::All(leaves) => TriggerSource::All {
            leaves: leaves.iter().map(leaf).collect(),
        },
        RowSource::Any(leaves) => TriggerSource::Any {
            leaves: leaves.iter().map(leaf).collect(),
        },
        RowSource::Timer(TimerRow::WindowClock {
            instrument_id,
            remaining_ms,
        }) => TriggerSource::WindowClock {
            instrument_id: instrument_id.clone(),
            remaining_ms: *remaining_ms,
        },
        RowSource::Timer(TimerRow::After { elapsed_ms }) => TriggerSource::After {
            elapsed_ms: *elapsed_ms,
        },
        RowSource::Timer(TimerRow::Interval { every_ms }) => TriggerSource::Interval {
            every_ms: *every_ms,
        },
    };
    TriggerDef {
        name: name.to_string(),
        source,
        fire: row.fire,
        rearm: row.rearm,
        cooldown_ms: row.cooldown_ms,
    }
}

fn leaf(leaf: &LeafRow) -> Leaf {
    match leaf {
        LeafRow::Bare { signal_id } => Leaf::Always {
            signal_id: signal_id.clone(),
        },
        LeafRow::Field {
            signal_id,
            field,
            comparator,
            value,
        } => Leaf::Field {
            signal_id: signal_id.clone(),
            field: field.clone(),
            comparator: *comparator,
            value: leaf_value(value),
        },
        LeafRow::Expr {
            expr,
            comparator,
            value,
            ..
        } => Leaf::Expr {
            expr: expr.clone(),
            comparator: *comparator,
            value: leaf_value(value),
        },
    }
}

fn block(plan: &BlockPlan, signal_ids: &BTreeMap<String, String>, ctx: &ExprCtx) -> BlockDef {
    let mut reads: Vec<SignalRead> = plan
        .reads
        .iter()
        .map(|(nick, field)| SignalRead {
            signal_id: signal_ids
                .get(nick)
                .expect("every read nickname is a declared signal")
                .clone(),
            field: field.clone(),
        })
        .collect();
    // Two nicknames may resolve to one id; the read set is by id.
    reads.sort();
    reads.dedup();

    BlockDef {
        name: plan.name.clone(),
        guard: plan.guard.as_ref().map(|g| lower_expr(g, ctx)),
        reads,
        items: plan.items.iter().map(|item| plan_item(item, ctx)).collect(),
        then: plan.then.clone(),
    }
}

fn plan_item(item: &PlanItem, ctx: &ExprCtx) -> Item {
    match item {
        PlanItem::Set { slot, index, rhs } => Item::Set {
            slot: slot.clone(),
            keys: index
                .as_ref()
                .map(|keys| keys.iter().map(|k| lower_expr(k, ctx)).collect()),
            value: lower_expr(rhs, ctx),
        },
        PlanItem::Push { slot, value } => Item::Push {
            slot: slot.clone(),
            value: lower_expr(value, ctx),
        },
        PlanItem::Intent(IntentLower::Place { intent_id, order }) => Item::Place {
            intent_id: intent_id.clone(),
            order: order_def(order, ctx),
        },
        PlanItem::Intent(IntentLower::Cancel { tag }) => Item::Cancel { tag: tag.clone() },
        PlanItem::Intent(IntentLower::CancelReplace { intent_id, order }) => Item::CancelReplace {
            intent_id: intent_id.clone(),
            order: order_def(order, ctx),
        },
        PlanItem::Intent(IntentLower::TargetPosition { intent_id, order }) => Item::TargetPosition {
            intent_id: intent_id.clone(),
            order: order_def(order, ctx),
        },
    }
}

fn order_def(order: &OrderLower, ctx: &ExprCtx) -> OrderDef {
    OrderDef {
        instrument_id: order.instrument_id.clone(),
        outcome: order.outcome.clone(),
        side: order.side,
        order_type: order.order_type,
        tif: order.tif,
        tag: order.tag.clone(),
        size: lower_expr(&order.size, ctx),
        price: order.price.as_ref().map(|p| lower_expr(p, ctx)),
        post_only: order.post_only,
        expire_after_ms: order.expire_after_ms,
        reduce_only: order.reduce_only,
    }
}

fn intent(entry: &IntentEntry) -> IntentDef {
    IntentDef {
        on: OnTable {
            fill: entry.on.fill.clone(),
            partial: entry.on.partial.clone(),
            reject: entry.on.reject.clone(),
            cancel_ack: entry.on.cancel_ack.clone(),
            expiry: entry.on.expiry.clone(),
        },
        max_size: entry.max_size.clone(),
    }
}
