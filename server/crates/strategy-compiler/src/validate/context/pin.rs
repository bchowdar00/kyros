//! The per-block pin: which single-signal condition trigger a block's guard names,
//! deciding whether `event.*` is well-typed inside the block (ir-spec §11.3).
//!
//! A block is PINNED iff its guard contains exactly one `triggered(NAME)` node and
//! NAME is a condition trigger over exactly one signal; the block's `event.*` fields
//! are that signal's. Computed once here; the guard rules, statement rules, and lints
//! all read it.
//!
//! Recorded corner (kept deliberately simple): the count rule ignores polarity — a
//! guard of `not triggered(x)` still pins x. A negated pin reading `event.*` is
//! nonsense no honest strategy writes; the simple rule keeps the pin decidable without
//! a polarity analysis.

use std::collections::BTreeMap;

use crate::ir::Strategy;

use crate::parse::ast::expr::Expr;
use crate::parse::walk::ParsedString;
use crate::validate::poison::Poisons;
use crate::validate::tables::{Tables, TriggerKind};
use crate::validate::visit::walk_expr;

/// One block's pin decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PinStatus {
    /// Exactly one single-signal condition trigger: `event.*` reads its fields.
    Pinned {
        /// The pinned signal nickname.
        signal: String,
    },
    /// The guard does not pin: `event.*` is an error here, with this reason.
    Unpinned(UnpinReason),
    /// The pin is unknowable — the guard string failed to parse, or the pinned
    /// trigger's expression did — and every pin-dependent rule suppresses.
    GuardFailed,
}

/// Why a block provides no `event.*`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnpinReason {
    /// No guard, or a guard with no `triggered()` — nothing names the firing event.
    NoTriggered,
    /// More than one `triggered()` — the firing event would be ambiguous.
    MultipleTriggered,
    /// The pinned trigger is a timer — timers carry no payload.
    Timer,
    /// The pinned trigger's condition spans several signals — the firing event would
    /// be ambiguous.
    MultiSignal,
    /// The pinned name resolves to no trigger — family A reported it; suppress
    /// rather than second-guess.
    UnknownTrigger,
}

/// Every block's pin, keyed `(leg index, block index)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pins(BTreeMap<(usize, usize), PinStatus>);

impl Pins {
    /// Compute the pin for every block.
    pub fn compute(strategy: &Strategy, tables: &Tables, poisons: &Poisons) -> Pins {
        let mut pins = BTreeMap::new();
        for (l, leg) in strategy.legs.iter().enumerate() {
            for (b, block) in leg.body.iter().enumerate() {
                pins.insert((l, b), pin_of(l, b, block, tables, poisons));
            }
        }
        Pins(pins)
    }

    /// The pin for a block. Every block has an entry; the `expect` documents that
    /// invariant (the map is total over `(leg, block)` by construction).
    pub fn at(&self, leg: usize, block: usize) -> &PinStatus {
        self.0
            .get(&(leg, block))
            .expect("Pins is total over every (leg, block) by construction")
    }
}

fn pin_of(l: usize, b: usize, block: &crate::ir::Block, tables: &Tables, poisons: &Poisons) -> PinStatus {
    if block.guard.is_none() {
        return PinStatus::Unpinned(UnpinReason::NoTriggered);
    }
    if poisons.failed_guards.contains(&(l, b)) {
        return PinStatus::GuardFailed;
    }
    let path = format!("legs[{l}].body[{b}].if");
    let Some(walked) = tables.ast_at(&path) else {
        // A guard that neither parsed nor failed cannot happen; treat as failed so
        // pin-dependent rules stay quiet rather than guessing.
        return PinStatus::GuardFailed;
    };
    let ParsedString::Expr(guard) = &walked.parsed else {
        return PinStatus::GuardFailed;
    };

    let mut names: Vec<&str> = Vec::new();
    walk_expr(guard, &mut |node| {
        if let Expr::Triggered { name } = &node.node {
            names.push(&name.node);
        }
    });
    match names.as_slice() {
        [] => PinStatus::Unpinned(UnpinReason::NoTriggered),
        [name] => match tables.triggers.get(*name) {
            None => PinStatus::Unpinned(UnpinReason::UnknownTrigger),
            Some(entry) => match entry.kind {
                TriggerKind::Timer(_) => PinStatus::Unpinned(UnpinReason::Timer),
                TriggerKind::Condition if !entry.signal_set_known => PinStatus::GuardFailed,
                TriggerKind::Condition => {
                    let mut signals = entry.signal_set.iter();
                    match (signals.next(), signals.next()) {
                        (Some(signal), None) => PinStatus::Pinned {
                            signal: signal.clone(),
                        },
                        _ => PinStatus::Unpinned(UnpinReason::MultiSignal),
                    }
                }
            },
        },
        _ => PinStatus::Unpinned(UnpinReason::MultipleTriggered),
    }
}
