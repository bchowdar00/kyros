//! Resolution tables: everything the rule passes look names up in, built once from the
//! raw document and the registry (P1). Construction only — the diagnostics that
//! accompany table-building (duplicates, reserved names, unresolved ids) are emitted by
//! `declarations.rs`, which drives this module.
//!
//! Determinism: every map here is a `BTreeMap` and every set a `BTreeSet`; iteration
//! order reaches diagnostic output.

use std::collections::{BTreeMap, BTreeSet};

use crate::ir::{
    Condition, ConditionLeaf, Fire, RearmMode, Registry, Strategy, TimerSpec, TriggerSource,
};

use crate::parse::ast::expr::{Expr, Namespace};
use crate::parse::walk::{ParsedString, WalkedString};
use crate::validate::visit::walk_expr;

/// A declared signal nickname's resolution against the registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalEntry {
    /// Index into `Registry::signals`.
    Resolved(usize),
    /// The id matched nothing; reads of this nickname suppress their registry checks.
    Unresolved,
}

/// A declared instrument nickname's resolution against the registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstrumentEntry {
    /// Index into `Registry::markets`.
    Market(usize),
    /// Index into `Registry::series` (the id was `<series_id>.current`).
    Series(usize),
    /// The id matched nothing; reads of this nickname suppress their registry checks.
    Unresolved,
}

/// A trigger's rule-relevant shape, precomputed once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriggerEntry {
    /// Index into `Strategy::triggers`.
    pub index: usize,
    /// Condition or timer.
    pub kind: TriggerKind,
    /// The distinct signal nicknames the condition names: bare and field leaves from
    /// the JSON, expr leaves from their parsed reads. Empty for timers.
    pub signal_set: BTreeSet<String>,
    /// Whether `signal_set` is complete. `false` when an expr leaf failed to parse:
    /// the pin rule and the lints that depend on the set stay quiet (the parse error
    /// is already reported).
    pub signal_set_known: bool,
    /// The fire mode.
    pub fire: Fire,
    /// The rearm mode.
    pub rearm_mode: RearmMode,
    /// The rearm cooldown, milliseconds, when declared.
    pub cooldown_ms: Option<u64>,
}

/// The two trigger families, as the rules need them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerKind {
    /// An SME condition trigger.
    Condition,
    /// A timer trigger, of this kind.
    Timer(TimerKind),
}

/// The timer kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimerKind {
    /// Against the instrument's window close.
    WindowClock,
    /// Once, measured from leg entry.
    After,
    /// Repeating, measured from leg entry.
    Interval,
}

/// A leg's rule-relevant shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegEntry {
    /// Index into `Strategy::legs`.
    pub index: usize,
    /// Block names declared in this leg (uniqueness is checked at build time).
    pub block_names: BTreeSet<String>,
    /// Tags carried by orders this leg places (`place` and `cancel_replace`) — the
    /// tags a `legs.<leg>.tag.<tag>.*` read may name.
    pub tags: BTreeSet<String>,
}

/// Every lookup surface the rule passes share. Borrows nothing — indices into the
/// document and registry keep it plain data, and `ValidStrategy` owns it afterward.
#[derive(Debug, Clone, PartialEq)]
pub struct Tables {
    /// Signal nickname -> registry resolution.
    pub signals: BTreeMap<String, SignalEntry>,
    /// Instrument nickname -> registry resolution.
    pub instruments: BTreeMap<String, InstrumentEntry>,
    /// State slot name -> index into `Strategy::state`.
    pub state: BTreeMap<String, usize>,
    /// Param name -> index into `Strategy::params`.
    pub params: BTreeMap<String, usize>,
    /// Trigger name -> precomputed shape.
    pub triggers: BTreeMap<String, TriggerEntry>,
    /// Leg name -> shape.
    pub legs: BTreeMap<String, LegEntry>,
    /// Declared named-expression names (bodies live in the walked ASTs; substitution
    /// status lives in `Poisons`/`subst`).
    pub named_exprs: BTreeSet<String>,
    /// Declared constant-table names.
    pub const_tables: BTreeSet<String>,
    /// Every tag declared by a place or cancel-replace intent.
    pub tags: BTreeSet<String>,
    /// Everything that parsed, in document order (moved out of the walk outcome).
    pub asts: Vec<WalkedString>,
    /// Walker path -> index into `asts`.
    pub ast_index: BTreeMap<String, usize>,
}

impl Tables {
    /// Build every table. Infallible: unresolvable entries are recorded as
    /// `Unresolved`, duplicate declarations keep the FIRST occurrence (the diagnostic
    /// for the second is `declarations.rs`'s job — first-wins here keeps every
    /// downstream reference resolvable while the author repairs the duplicate).
    pub fn build(strategy: &Strategy, registry: &Registry, walked: Vec<WalkedString>) -> Tables {
        let mut signals = BTreeMap::new();
        for (nick, id) in strategy.signals.iter() {
            let entry = registry
                .signals
                .iter()
                .position(|def| def.signal_id == *id)
                .map_or(SignalEntry::Unresolved, SignalEntry::Resolved);
            signals.insert(nick.as_str().to_string(), entry);
        }

        let mut instruments = BTreeMap::new();
        for (nick, id) in strategy.instruments.iter() {
            instruments.insert(nick.as_str().to_string(), resolve_instrument(registry, id));
        }

        let mut state = BTreeMap::new();
        for (index, slot) in strategy.state.iter().enumerate() {
            state
                .entry(slot.name().as_str().to_string())
                .or_insert(index);
        }

        let mut params = BTreeMap::new();
        for (index, param) in strategy.params.iter().enumerate() {
            params
                .entry(param.name.as_str().to_string())
                .or_insert(index);
        }

        let asts = walked;
        let ast_index: BTreeMap<String, usize> = asts
            .iter()
            .enumerate()
            .map(|(i, w)| (w.path.clone(), i))
            .collect();

        let mut triggers = BTreeMap::new();
        for (index, trigger) in strategy.triggers.iter().enumerate() {
            let (kind, signal_set, signal_set_known) = match &trigger.source {
                TriggerSource::Condition(condition) => {
                    let (set, known) = condition_signals(index, condition, &asts, &ast_index);
                    (TriggerKind::Condition, set, known)
                }
                TriggerSource::Timer(timer) => {
                    let timer_kind = match timer {
                        TimerSpec::WindowClock { .. } => TimerKind::WindowClock,
                        TimerSpec::After { .. } => TimerKind::After,
                        TimerSpec::Interval { .. } => TimerKind::Interval,
                    };
                    (TriggerKind::Timer(timer_kind), BTreeSet::new(), true)
                }
            };
            triggers
                .entry(trigger.name.as_str().to_string())
                .or_insert(TriggerEntry {
                    index,
                    kind,
                    signal_set,
                    signal_set_known,
                    fire: trigger.fire,
                    rearm_mode: trigger.rearm.mode,
                    cooldown_ms: trigger.rearm.cooldown.as_ref().map(|c| c.as_millis()),
                });
        }

        let mut legs = BTreeMap::new();
        for (index, leg) in strategy.legs.iter().enumerate() {
            let block_names = leg
                .body
                .iter()
                .map(|block| block.name.as_str().to_string())
                .collect();
            let tags = leg
                .body
                .iter()
                .flat_map(|block| block.do_items.iter())
                .filter_map(|item| match item {
                    crate::ir::DoItem::Place(place) => Some(place.place.tag.as_str().to_string()),
                    crate::ir::DoItem::CancelReplace(replace) => {
                        Some(replace.cancel_replace.tag.as_str().to_string())
                    }
                    crate::ir::DoItem::TargetPosition(target) => {
                        Some(target.target_position.tag.as_str().to_string())
                    }
                    crate::ir::DoItem::Statement(_) | crate::ir::DoItem::Cancel(_) => None,
                })
                .collect();
            legs.entry(leg.name.as_str().to_string()).or_insert(LegEntry {
                index,
                block_names,
                tags,
            });
        }

        let mut named_exprs = BTreeSet::new();
        let mut const_tables = BTreeSet::new();
        if let Some(expressions) = &strategy.expressions {
            if let Some(named) = &expressions.named {
                named_exprs.extend(named.iter().map(|(name, _)| name.as_str().to_string()));
            }
            if let Some(tables) = &expressions.tables {
                const_tables.extend(tables.iter().map(|(name, _)| name.as_str().to_string()));
            }
        }

        let mut tags = BTreeSet::new();
        for leg in &strategy.legs {
            for block in &leg.body {
                for item in &block.do_items {
                    match item {
                        crate::ir::DoItem::Place(place) => {
                            tags.insert(place.place.tag.as_str().to_string());
                        }
                        crate::ir::DoItem::CancelReplace(replace) => {
                            tags.insert(replace.cancel_replace.tag.as_str().to_string());
                        }
                        crate::ir::DoItem::TargetPosition(target) => {
                            tags.insert(target.target_position.tag.as_str().to_string());
                        }
                        crate::ir::DoItem::Statement(_) | crate::ir::DoItem::Cancel(_) => {}
                    }
                }
            }
        }

        Tables {
            signals,
            instruments,
            state,
            params,
            triggers,
            legs,
            named_exprs,
            const_tables,
            tags,
            asts,
            ast_index,
        }
    }

    /// The parsed AST at a walker path, if that string parsed.
    pub fn ast_at(&self, path: &str) -> Option<&WalkedString> {
        self.ast_index.get(path).map(|&i| &self.asts[i])
    }
}

/// Resolve an instrument id: an exact market id, or `<series_id>.current`.
fn resolve_instrument(registry: &Registry, id: &str) -> InstrumentEntry {
    if let Some(index) = registry.markets.iter().position(|m| m.market_id == id) {
        return InstrumentEntry::Market(index);
    }
    if let Some(series_id) = id.strip_suffix(".current")
        && let Some(index) = registry
            .series
            .iter()
            .position(|s| s.series_id == series_id)
    {
        return InstrumentEntry::Series(index);
    }
    InstrumentEntry::Unresolved
}

/// The distinct signal nicknames named by a condition's leaves, and whether the set
/// is complete (an expr leaf whose string failed to parse contributes nothing and
/// marks the set unknown).
fn condition_signals(
    trigger_index: usize,
    condition: &Condition,
    asts: &[WalkedString],
    ast_index: &BTreeMap<String, usize>,
) -> (BTreeSet<String>, bool) {
    let base = format!("triggers[{trigger_index}].condition");
    let (leaves, paths): (&[ConditionLeaf], Vec<String>) = match condition {
        Condition::Leaf(leaf) => (std::slice::from_ref(leaf), vec![base]),
        Condition::All(group) => (
            &group.all,
            (0..group.all.len()).map(|l| format!("{base}.all[{l}]")).collect(),
        ),
        Condition::Any(group) => (
            &group.any,
            (0..group.any.len()).map(|l| format!("{base}.any[{l}]")).collect(),
        ),
    };
    let mut set = BTreeSet::new();
    let mut known = true;
    for (leaf, path) in leaves.iter().zip(paths) {
        match leaf {
            ConditionLeaf::Bare(bare) => {
                set.insert(bare.signal.as_str().to_string());
            }
            ConditionLeaf::Field(field) => {
                set.insert(field.signal.as_str().to_string());
            }
            ConditionLeaf::Expr(_) => {
                let Some(walked) = ast_index.get(&format!("{path}.expr")).map(|&i| &asts[i])
                else {
                    known = false;
                    continue;
                };
                if let ParsedString::Expr(ast) = &walked.parsed {
                    walk_expr(ast, &mut |node| {
                        if let Expr::Ref(r) = &node.node
                            && r.namespace == Namespace::Signal
                            && let Some(nick) = r.path.first()
                        {
                            set.insert(nick.node.to_string());
                        }
                    });
                }
            }
        }
    }
    (set, known)
}
