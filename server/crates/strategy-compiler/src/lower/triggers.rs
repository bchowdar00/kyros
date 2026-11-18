//! Trigger lowering: one normalized registration row per document-level trigger.
//!
//! This is the registration-row dialect (python-target-spec §2.2), NOT the wire
//! manifest dialect — both project from this one lowering result. No DNF expansion, no
//! per-leg duplication: v0.2 conditions cannot nest, so the platform evaluates
//! `all`/`any` natively over one row and one logical trigger keeps one edge/rearm
//! state. A lone condition leaf normalizes to `All([leaf])`.
//!
//! Everything stays string-canonical: leaf values are the canonical decimal or variant
//! strings verbatim (the trigger engine compares exact decimals — only the Python
//! activation ABI is float-lowered), and durations are the already-lowered integer
//! milliseconds.

use std::collections::BTreeMap;

use crate::ir::{Comparator, Condition, ConditionLeaf, Fire, LeafValue, RearmMode, TriggerSource};

use crate::lower::exprs::{ExprCtx, lower_expr};
use crate::program::ExprNode;
use crate::validate::ValidStrategy;

/// One registration row: exactly one source, plus the edge semantics.
#[derive(Debug, Clone, PartialEq)]
pub struct TriggerRow {
    /// The wake source, normalized.
    pub source: RowSource,
    /// Fire mode, rendered by the emitters as `"if_true"` / `"on_crossing"`.
    pub fire: Fire,
    /// Rearm mode.
    pub rearm: RearmMode,
    /// The rearm cooldown, milliseconds, when declared.
    pub cooldown_ms: Option<u64>,
}

/// The row's source: a native conjunction, disjunction, or timer.
#[derive(Debug, Clone, PartialEq)]
pub enum RowSource {
    /// `"all"`: same-moment conjunction. A lone leaf lands here as a one-element group.
    All(Vec<LeafRow>),
    /// `"any"`: stateless disjunction.
    Any(Vec<LeafRow>),
    /// `"timer"`: the timer families.
    Timer(TimerRow),
}

/// One condition leaf, signal nickname resolved to its id.
#[derive(Debug, Clone, PartialEq)]
pub enum LeafRow {
    /// True whenever the signal publishes; lowers onto the wire's `always` comparator.
    Bare {
        /// The resolved signal id.
        signal_id: String,
    },
    /// One field compared against a value.
    Field {
        /// The resolved signal id.
        signal_id: String,
        /// The field name, per the signal's registry entry.
        field: String,
        /// The comparison operator.
        comparator: Comparator,
        /// What the field is compared against.
        value: LeafValueLower,
    },
    /// Arithmetic over subscribed signals' fields compared against a value (v0.3
    /// cross-signal leaf).
    Expr {
        /// The lowered expression: signal reads carry resolved ids, envelope reads
        /// their values. The program embeds it verbatim; the manifest projects it.
        expr: ExprNode,
        /// The authored expression string (display-side, and the manifest's fallback
        /// when the wire op-tree cannot express the tree).
        source: String,
        /// The comparison operator.
        comparator: Comparator,
        /// What the arithmetic is compared against.
        value: LeafValueLower,
    },
}

/// What a comparing leaf compares against: a string-canonical literal (a canonical
/// signed decimal, a plain variant, or text — never a float) or a param by name.
#[derive(Debug, Clone, PartialEq)]
pub enum LeafValueLower {
    /// The literal's canonical string.
    Literal(String),
    /// The param name; the runtime substitutes its current value.
    Param(String),
}

/// A timer row.
#[derive(Debug, Clone, PartialEq)]
pub enum TimerRow {
    /// Fires when the instrument's window clock reaches the remaining threshold.
    WindowClock {
        /// The resolved venue instrument id.
        instrument_id: String,
        /// The threshold, in milliseconds.
        remaining_ms: u64,
    },
    /// Fires once, this long after leg entry.
    After {
        /// Milliseconds from arming.
        elapsed_ms: u64,
    },
    /// Fires every period while armed.
    Interval {
        /// The period, milliseconds.
        every_ms: u64,
    },
}

/// Lower every document trigger to its registration row, document order.
pub fn lower_triggers(valid: &ValidStrategy) -> Vec<(String, TriggerRow)> {
    let strategy = valid.strategy();
    let signal_ids: BTreeMap<String, String> = strategy
        .signals
        .iter()
        .map(|(nick, id)| (nick.as_str().to_string(), id.clone()))
        .collect();
    let ctx = ExprCtx {
        signal_ids: &signal_ids,
        envelope: valid.envelope(),
    };
    strategy
        .triggers
        .iter()
        .enumerate()
        .map(|(t, trigger)| {
            let source = match &trigger.source {
                TriggerSource::Condition(condition) => lower_condition(valid, &ctx, t, condition),
                TriggerSource::Timer(timer) => RowSource::Timer(timer_row(valid, timer)),
            };
            (
                trigger.name.as_str().to_string(),
                TriggerRow {
                    source,
                    fire: trigger.fire,
                    rearm: trigger.rearm.mode,
                    cooldown_ms: trigger.rearm.cooldown.as_ref().map(|c| c.as_millis()),
                },
            )
        })
        .collect()
}

/// Lower a ConditionTree: a lone leaf normalizes to `All([leaf])`.
fn lower_condition(
    valid: &ValidStrategy,
    ctx: &ExprCtx,
    trigger_index: usize,
    condition: &Condition,
) -> RowSource {
    let base = format!("triggers[{trigger_index}].condition");
    match condition {
        Condition::Leaf(leaf) => RowSource::All(vec![lower_leaf(valid, ctx, &base, leaf)]),
        Condition::All(group) => RowSource::All(
            group
                .all
                .iter()
                .enumerate()
                .map(|(l, leaf)| lower_leaf(valid, ctx, &format!("{base}.all[{l}]"), leaf))
                .collect(),
        ),
        Condition::Any(group) => RowSource::Any(
            group
                .any
                .iter()
                .enumerate()
                .map(|(l, leaf)| lower_leaf(valid, ctx, &format!("{base}.any[{l}]"), leaf))
                .collect(),
        ),
    }
}

/// Lower one leaf, resolving its signal nickname and carrying values verbatim.
fn lower_leaf(valid: &ValidStrategy, ctx: &ExprCtx, path: &str, leaf: &ConditionLeaf) -> LeafRow {
    match leaf {
        ConditionLeaf::Bare(bare) => LeafRow::Bare {
            signal_id: signal_id(valid, bare.signal.as_str()),
        },
        ConditionLeaf::Field(field) => LeafRow::Field {
            signal_id: signal_id(valid, field.signal.as_str()),
            field: field.field.as_str().to_string(),
            comparator: field.comparator,
            value: leaf_value(&field.value),
        },
        ConditionLeaf::Expr(expr) => {
            let walked = valid
                .tables()
                .ast_at(&format!("{path}.expr"))
                .expect("validated documents have every leaf-expr parsed");
            let ast = match &walked.parsed {
                crate::parse::walk::ParsedString::Expr(ast) => ast,
                crate::parse::walk::ParsedString::Stmt(_) => {
                    unreachable!("a leaf-expr path never parses as a statement")
                }
            };
            // Trigger expressions never reference named expressions (purity rule),
            // so the raw AST is already substitution-free.
            LeafRow::Expr {
                expr: lower_expr(ast, ctx),
                source: expr.expr.as_str().to_string(),
                comparator: expr.comparator,
                value: leaf_value(&expr.value),
            }
        }
    }
}

/// Resolve a declared signal nickname to its id. Total on validated documents.
fn signal_id(valid: &ValidStrategy, nick: &str) -> String {
    valid
        .strategy()
        .signals
        .iter()
        .find(|(name, _)| name.as_str() == nick)
        .map(|(_, id)| id.clone())
        .expect("the validator resolves every leaf signal nickname")
}

/// A leaf value's lowered form: literals verbatim (the canonical signed decimal, the
/// variant spelled plain, or the text), params by name.
fn leaf_value(value: &LeafValue) -> LeafValueLower {
    match value {
        LeafValue::Decimal(d) => LeafValueLower::Literal(d.as_str().to_string()),
        LeafValue::Variant(v) => LeafValueLower::Literal(v.as_str().to_string()),
        LeafValue::Str(s) => LeafValueLower::Literal(s.clone()),
        LeafValue::Param(p) => LeafValueLower::Param(p.as_str().to_string()),
    }
}

/// Lower a timer spec to its row, resolving the instrument nickname and the duration.
fn timer_row(valid: &ValidStrategy, timer: &crate::ir::TimerSpec) -> TimerRow {
    match timer {
        crate::ir::TimerSpec::WindowClock {
            instrument,
            remaining,
        } => TimerRow::WindowClock {
            instrument_id: instrument_id(valid, instrument.as_str()),
            remaining_ms: remaining.as_millis(),
        },
        crate::ir::TimerSpec::After { elapsed } => TimerRow::After {
            elapsed_ms: elapsed.as_millis(),
        },
        crate::ir::TimerSpec::Interval { every } => TimerRow::Interval {
            every_ms: every.as_millis(),
        },
    }
}

/// Resolve a declared instrument nickname to its venue id. Total on validated documents.
fn instrument_id(valid: &ValidStrategy, nick: &str) -> String {
    valid
        .strategy()
        .instruments
        .iter()
        .find(|(name, _)| name.as_str() == nick)
        .map(|(_, id)| id.clone())
        .expect("the validator resolves every timer instrument nickname")
}
