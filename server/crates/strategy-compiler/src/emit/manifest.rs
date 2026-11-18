//! The manifest projection: [`Lowered`] to the wire dialect (`crate::ir::Manifest`),
//! shaped by the PINNED wire schemas (`tests/schemas/`, vendored at
//! [`SCHEMA_PIN`]; `strategy-schema.schema.json` v2). The conformance test
//! validates every emitted manifest against exactly those files.
//!
//! Byte-deterministic and string-canonical: no float exists on this path — leaf
//! values and envelope decimals ride through as their canonical source strings, and
//! serialization order is struct declaration order plus already-sorted vectors.
//!
//! Author shape is recovered exactly where normalization erased it: an authored
//! `all`/`any` group has at least two leaves (`group-too-small` is a validator
//! error), so a one-element `All` row IS an authored lone leaf, un-normalized here.
//! Expr leaves cross the wire as the pre-parsed op-tree — the platform never parses
//! strings; the authored expression string is display-side only.

use crate::ir::{
    LifecycleStage, Manifest, StalenessPolicy, SubscriptionRow, WatchField, WindowClockParams,
    WireArithOp, WireComparator, WireCondition, WireExpr, WireIntentEntry, WireLeaf, WireOnTable,
    WireRearm, WireTimer, WireTimerKind, WireTrigger, WireTriggerSource,
};

use crate::lower::{IntentEntry, LeafRow, LeafValueLower, Lowered, RowSource, TimerRow, TriggerRow};
use crate::program::ExprNode;

/// The pinned wire-schema commit: `tiny-schemas` branch `jet/compiler-demo-changes`.
/// The files under `tests/schemas/` are that commit's; the conformance test
/// validates every emitted manifest against exactly them.
pub const SCHEMA_PIN: &str = "1b8377f8224e9a7f66beaa9784e6a2e77c8e838b";

/// Wire schema version stamped on every manifest: 2 = the pinned shape
/// (document-level triggers, arm-list lifecycle stages, intent index, v0.2
/// envelope).
const SCHEMA_VERSION: u32 = 2;

/// Serialize the manifest projection: pretty JSON plus a trailing newline, byte-
/// identical for the same lowering result.
pub fn emit_manifest(lowered: &Lowered) -> String {
    let manifest = project_manifest(lowered);
    let mut text = serde_json::to_string_pretty(&manifest)
        .expect("manifest types serialize infallibly: string-keyed, no floats");
    text.push('\n');
    text
}

/// Project the lowering result into the wire manifest types.
pub fn project_manifest(lowered: &Lowered) -> Manifest {
    Manifest {
        schema_version: SCHEMA_VERSION,
        strategy_id: lowered.strategy_id.clone(),
        name: lowered.meta_name.clone(),
        description: lowered.meta_description.clone(),
        dsl_version: lowered.dsl_version.clone(),
        source_hash: lowered.source_hash.clone(),
        signals: lowered
            .subscriptions
            .iter()
            .map(|id| SubscriptionRow {
                signal_id: id.clone(),
                staleness_policy: StalenessPolicy::Proceed,
            })
            .collect(),
        watch_fields: watch_fields(lowered),
        triggers: lowered
            .triggers
            .iter()
            .map(|(name, row)| wire_trigger(name, row))
            .collect(),
        lifecycle_stages: lowered
            .legs
            .iter()
            .map(|leg| LifecycleStage {
                name: leg.name.clone(),
                resident: leg.resident,
                arm: leg.arm.clone(),
            })
            .collect(),
        intent_index: lowered
            .intent_index
            .iter()
            .map(|(id, entry)| (id.clone(), wire_intent_entry(entry)))
            .collect(),
        envelope: lowered.envelope.clone(),
    }
}

/// Every (signal_id, field) the strategy reads: trigger field leaves, expr-leaf
/// op-tree fields, and body snapshot reads — deduped, sorted.
fn watch_fields(lowered: &Lowered) -> Vec<WatchField> {
    let mut pairs = std::collections::BTreeSet::new();
    for (_, row) in &lowered.triggers {
        let leaves = match &row.source {
            RowSource::All(leaves) | RowSource::Any(leaves) => leaves,
            RowSource::Timer(_) => continue,
        };
        for leaf in leaves {
            match leaf {
                LeafRow::Bare { .. } => {}
                LeafRow::Field {
                    signal_id, field, ..
                } => {
                    pairs.insert((signal_id.clone(), field.clone()));
                }
                LeafRow::Expr { expr, .. } => collect_expr_fields(expr, &mut pairs),
            }
        }
    }
    let nick_to_id: std::collections::BTreeMap<&str, &str> = lowered
        .signals
        .iter()
        .map(|(nick, id)| (nick.as_str(), id.as_str()))
        .collect();
    for leg in &lowered.legs {
        for block in &leg.blocks {
            for (nick, field) in &block.reads {
                let id = nick_to_id
                    .get(nick.as_str())
                    .expect("every read nickname is a declared signal");
                pairs.insert(((*id).to_string(), field.clone()));
            }
        }
    }
    pairs
        .into_iter()
        .map(|(signal_id, field)| WatchField { signal_id, field })
        .collect()
}

/// Every `(signal_id, field)` a lowered expression reads.
fn collect_expr_fields(expr: &ExprNode, pairs: &mut std::collections::BTreeSet<(String, String)>) {
    for_each_node(expr, &mut |node| {
        if let ExprNode::Signal { signal_id, field } = node {
            pairs.insert((signal_id.clone(), field.clone()));
        }
    });
}

/// Preorder walk over an [`ExprNode`] tree.
fn for_each_node<'a>(expr: &'a ExprNode, visit: &mut impl FnMut(&'a ExprNode)) {
    visit(expr);
    match expr {
        ExprNode::Decimal { .. }
        | ExprNode::Bool { .. }
        | ExprNode::DurationMs { .. }
        | ExprNode::Variant { .. }
        | ExprNode::Str { .. }
        | ExprNode::Param { .. }
        | ExprNode::Signal { .. }
        | ExprNode::Event { .. }
        | ExprNode::State { .. }
        | ExprNode::Envelope { .. }
        | ExprNode::Platform { .. }
        | ExprNode::LegFill { .. }
        | ExprNode::Entry
        | ExprNode::Triggered { .. } => {}
        ExprNode::Reduce { predicate, .. } => {
            if let Some(predicate) = predicate {
                for_each_node(predicate, visit);
            }
        }
        ExprNode::StateMap { keys, .. } => keys.iter().for_each(|k| for_each_node(k, visit)),
        ExprNode::Neg { arg } | ExprNode::Not { arg } => for_each_node(arg, visit),
        ExprNode::Add { lhs, rhs }
        | ExprNode::Sub { lhs, rhs }
        | ExprNode::Mul { lhs, rhs }
        | ExprNode::Div { lhs, rhs }
        | ExprNode::Lt { lhs, rhs }
        | ExprNode::Le { lhs, rhs }
        | ExprNode::Gt { lhs, rhs }
        | ExprNode::Ge { lhs, rhs }
        | ExprNode::Eq { lhs, rhs }
        | ExprNode::Ne { lhs, rhs }
        | ExprNode::And { lhs, rhs }
        | ExprNode::Or { lhs, rhs } => {
            for_each_node(lhs, visit);
            for_each_node(rhs, visit);
        }
        ExprNode::If { arms, else_value } => {
            for arm in arms {
                for_each_node(&arm.condition, visit);
                for_each_node(&arm.value, visit);
            }
            for_each_node(else_value, visit);
        }
        ExprNode::Call { args, .. } => args.iter().for_each(|a| for_each_node(a, visit)),
        ExprNode::Lookup { key, .. } => for_each_node(key, visit),
    }
}

/// The first signal a lowered expression reads, preorder — the wire leaf's
/// `signal_id`. Every trigger expression reads at least one (validator rule).
fn primary_signal(expr: &ExprNode) -> String {
    let mut first = None;
    for_each_node(expr, &mut |node| {
        if first.is_none()
            && let ExprNode::Signal { signal_id, .. } = node
        {
            first = Some(signal_id.clone());
        }
    });
    first.expect("a trigger expression reads at least one signal")
}

/// One document-level trigger row in wire shape.
fn wire_trigger(name: &str, row: &TriggerRow) -> WireTrigger {
    WireTrigger {
        id: name.to_string(),
        source: source(row),
        fire: row.fire,
        rearm: WireRearm {
            mode: row.rearm,
            cooldown_ms: row.cooldown_ms,
        },
    }
}

/// One intent index entry in wire shape: the totalized five-key table plus the
/// resolved size bound.
fn wire_intent_entry(entry: &IntentEntry) -> WireIntentEntry {
    WireIntentEntry {
        on: WireOnTable {
            fill: entry.on.fill.clone(),
            partial: entry.on.partial.clone(),
            reject: entry.on.reject.clone(),
            cancel_ack: entry.on.cancel_ack.clone(),
            expiry: entry.on.expiry.clone(),
        },
        max_size: entry.max_size.clone(),
    }
}

/// The wire source: one-element `All` un-normalizes to the authored lone leaf.
fn source(row: &TriggerRow) -> WireTriggerSource {
    match &row.source {
        RowSource::All(leaves) => match leaves.as_slice() {
            [lone] => WireTriggerSource::Condition(WireCondition::Leaf(wire_leaf(lone))),
            _ => WireTriggerSource::Condition(WireCondition::All {
                all: leaves
                    .iter()
                    .map(|leaf| WireCondition::Leaf(wire_leaf(leaf)))
                    .collect(),
            }),
        },
        RowSource::Any(leaves) => WireTriggerSource::Condition(WireCondition::Any {
            any: leaves
                .iter()
                .map(|leaf| WireCondition::Leaf(wire_leaf(leaf)))
                .collect(),
        }),
        RowSource::Timer(TimerRow::WindowClock {
            instrument_id,
            remaining_ms,
        }) => WireTriggerSource::Timer(WireTimer {
            kind: WireTimerKind::WindowClock,
            delay_ms: None,
            period_ms: None,
            window_clock: Some(WindowClockParams {
                instrument_id: instrument_id.clone(),
                remaining_ms: *remaining_ms,
            }),
        }),
        RowSource::Timer(TimerRow::After { elapsed_ms }) => WireTriggerSource::Timer(WireTimer {
            kind: WireTimerKind::After,
            delay_ms: Some(*elapsed_ms),
            period_ms: None,
            window_clock: None,
        }),
        RowSource::Timer(TimerRow::Interval { every_ms }) => {
            WireTriggerSource::Timer(WireTimer {
                kind: WireTimerKind::Interval,
                delay_ms: None,
                period_ms: Some(*every_ms),
                window_clock: None,
            })
        }
    }
}

fn wire_leaf(leaf: &LeafRow) -> WireLeaf {
    match leaf {
        LeafRow::Bare { signal_id } => WireLeaf {
            signal_id: signal_id.clone(),
            field: None,
            expr: None,
            comparator: WireComparator::Always,
            value: None,
        },
        LeafRow::Field {
            signal_id,
            field,
            comparator,
            value,
        } => WireLeaf {
            signal_id: signal_id.clone(),
            field: Some(field.clone()),
            expr: None,
            comparator: wire_comparator(*comparator),
            value: Some(wire_value(value)),
        },
        LeafRow::Expr {
            expr,
            source,
            comparator,
            value,
        } => {
            let signal_id = primary_signal(expr);
            WireLeaf {
                expr: Some(
                    wire_expr(expr, &signal_id).unwrap_or_else(|| WireExpr::Source(source.clone())),
                ),
                signal_id,
                field: None,
                comparator: wire_comparator(*comparator),
                value: Some(wire_value(value)),
            }
        }
    }
}

/// A leaf value on the wire. The pinned schema types `value` as a `SignalValue`
/// string and has no param concept; a param-valued leaf projects as its read
/// spelling (`param.<name>`), which is display-side truth — the program is the
/// executable artifact.
fn wire_value(value: &LeafValueLower) -> String {
    match value {
        LeafValueLower::Literal(value) => value.clone(),
        LeafValueLower::Param(name) => format!("param.{name}"),
    }
}

/// The lowered expression in the pinned op-tree shape, when it fits: reads of the
/// leaf's one signal, unsigned decimal constants, `+ - * /` and negation. Anything
/// else (a second signal, a param, an envelope constant, a builtin) yields `None` and
/// the caller falls back to the authored source.
fn wire_expr(expr: &ExprNode, signal_id: &str) -> Option<WireExpr> {
    let bin = |op: WireArithOp, lhs: &ExprNode, rhs: &ExprNode| {
        Some(WireExpr::Bin {
            op,
            lhs: Box::new(wire_expr(lhs, signal_id)?),
            rhs: Box::new(wire_expr(rhs, signal_id)?),
        })
    };
    match expr {
        ExprNode::Signal {
            signal_id: read,
            field,
        } if read == signal_id => Some(WireExpr::Field(field.clone())),
        ExprNode::Decimal { value } if !value.starts_with('-') => {
            Some(WireExpr::Const(value.clone()))
        }
        ExprNode::Add { lhs, rhs } => bin(WireArithOp::Add, lhs, rhs),
        ExprNode::Sub { lhs, rhs } => bin(WireArithOp::Sub, lhs, rhs),
        ExprNode::Mul { lhs, rhs } => bin(WireArithOp::Mul, lhs, rhs),
        ExprNode::Div { lhs, rhs } => bin(WireArithOp::Div, lhs, rhs),
        ExprNode::Neg { arg } => Some(WireExpr::Neg(Box::new(wire_expr(arg, signal_id)?))),
        ExprNode::Signal { .. }
        | ExprNode::Decimal { .. }
        | ExprNode::Bool { .. }
        | ExprNode::DurationMs { .. }
        | ExprNode::Variant { .. }
        | ExprNode::Str { .. }
        | ExprNode::Param { .. }
        | ExprNode::Event { .. }
        | ExprNode::State { .. }
        | ExprNode::StateMap { .. }
        | ExprNode::Envelope { .. }
        | ExprNode::Platform { .. }
        | ExprNode::LegFill { .. }
        | ExprNode::Not { .. }
        | ExprNode::Lt { .. }
        | ExprNode::Le { .. }
        | ExprNode::Gt { .. }
        | ExprNode::Ge { .. }
        | ExprNode::Eq { .. }
        | ExprNode::Ne { .. }
        | ExprNode::And { .. }
        | ExprNode::Or { .. }
        | ExprNode::If { .. }
        | ExprNode::Call { .. }
        | ExprNode::Lookup { .. }
        | ExprNode::Reduce { .. }
        | ExprNode::Entry
        | ExprNode::Triggered { .. } => None,
    }
}

fn wire_comparator(comparator: crate::ir::Comparator) -> WireComparator {
    match comparator {
        crate::ir::Comparator::Eq => WireComparator::Eq,
        crate::ir::Comparator::Ne => WireComparator::Ne,
        crate::ir::Comparator::Lt => WireComparator::Lt,
        crate::ir::Comparator::Le => WireComparator::Le,
        crate::ir::Comparator::Gt => WireComparator::Gt,
        crate::ir::Comparator::Ge => WireComparator::Ge,
    }
}
