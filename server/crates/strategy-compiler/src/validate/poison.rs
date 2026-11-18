//! Cascade suppression: the poison set.
//!
//! The universal principle, made mechanical: a rule consults this set for every input
//! it depends on, and if any input is poisoned the rule emits NOTHING — the root cause
//! was already reported once (a parse error, an unresolved id, a cycle). Without this,
//! one broken string fans out into phantom diagnostics for every rule downstream of it.
//!
//! What is never suppressed: structural rules (they read only JSON), and resolution of
//! names declared in typed JSON (declarations cannot fail to parse).

use std::collections::BTreeSet;

use crate::ir::Strategy;

use crate::parse::walk::WalkDiagnostic;

use super::tables::{InstrumentEntry, SignalEntry, Tables};

/// Everything known-broken upstream, consulted by every rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Poisons {
    /// Walker paths of strings that failed to parse.
    pub failed_strings: BTreeSet<String>,
    /// `(leg index, block index)` of blocks whose guard failed to parse — the
    /// event-pin rule and the per-block lints are suppressed there.
    pub failed_guards: BTreeSet<(usize, usize)>,
    /// Signal nicknames whose registry resolution failed — reads of these suppress
    /// their registry cross-checks.
    pub unresolved_signals: BTreeSet<String>,
    /// Instrument nicknames whose registry resolution failed.
    pub unresolved_instruments: BTreeSet<String>,
    /// Named expressions unusable for substitution: parse-failed body, cycle member,
    /// or transitively referencing one. Filled by the substitution pass (P2); empty
    /// until then.
    pub poisoned_exprs: BTreeSet<String>,
}

impl Poisons {
    /// Seed the poison set from the walk diagnostics and the resolution tables
    /// (P0/P1). The substitution pass adds `poisoned_exprs` afterward.
    pub fn seed(strategy: &Strategy, walk: &[WalkDiagnostic], tables: &Tables) -> Poisons {
        let failed_strings: BTreeSet<String> = walk.iter().map(|diag| diag.path.clone()).collect();

        // Guard paths are reconstructed from the document (not parsed back out of the
        // path strings) so the mapping to (leg, block) stays index-true.
        let mut failed_guards = BTreeSet::new();
        for (l, leg) in strategy.legs.iter().enumerate() {
            for (b, block) in leg.body.iter().enumerate() {
                if block.guard.is_some()
                    && failed_strings.contains(&format!("legs[{l}].body[{b}].if"))
                {
                    failed_guards.insert((l, b));
                }
            }
        }

        let unresolved_signals = tables
            .signals
            .iter()
            .filter(|(_, entry)| **entry == SignalEntry::Unresolved)
            .map(|(nick, _)| nick.clone())
            .collect();
        let unresolved_instruments = tables
            .instruments
            .iter()
            .filter(|(_, entry)| **entry == InstrumentEntry::Unresolved)
            .map(|(nick, _)| nick.clone())
            .collect();

        Poisons {
            failed_strings,
            failed_guards,
            unresolved_signals,
            unresolved_instruments,
            poisoned_exprs: BTreeSet::new(),
        }
    }
}
