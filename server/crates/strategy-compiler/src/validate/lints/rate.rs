//! The requote-rate lint (2026-09-06): the rate-shaped lint the machine-gun
//! formalization deferred. A block warns when ALL of —
//!
//! 1. its do-list submits to the venue on every wake — `place`,
//!    `cancel_replace` or `target_position` — and it has no `then` (a
//!    transition leaves the leg, ending the re-fire);
//! 2. its guard tests `triggered(X)` where X is a condition trigger that is
//!    UNPACED: `fire: if_true`, `rearm: immediate`, no cooldown (an edge-fired
//!    or rearm-limited trigger fires at most once per crossing, and a cooldown
//!    is the throttle);
//! 3. X reads a registry signal that publishes faster than once a second —
//!    metadata `approximate_interval_ms` under [`SUB_SECOND_MS`]; the exchange
//!    marks its per-event internals (the book, the tape) with `0`.
//!
//! Such a block requotes on every frame of a fast signal. The runtime survives
//! it — activations coalesce and the order-rate gate rejects the excess — but
//! the strategy then behaves as the gate decides, not as written. The fix is a
//! `rearm.cooldown` on X, which is exactly "at most every N ms, with the latest
//! state" for an `if_true` trigger.
//!
//! Recorded blind spots: a signal with no declared interval is assumed slow;
//! the interval is the publisher's estimate, not a bound; and a state-slot
//! guard (`state.n < 3`) that limits the requotes is not recognised — the
//! machine-gun lint's self-flip check is not applied here because a requote
//! block rarely flips state.

use crate::ir::Fire;
use crate::ir::RearmMode;
use crate::ir::Registry;
use crate::validate::diag::{DiagCode, DiagPath, Diagnostic};
use crate::validate::tables::{SignalEntry, Tables, TriggerKind};

use super::BlockInfo;

/// Publish intervals under this are "fast" for the lint.
pub const SUB_SECOND_MS: u64 = 1000;

/// The declared publish interval of a registry signal, if any.
fn interval_ms(registry: &Registry, index: usize) -> Option<u64> {
    registry.signals.get(index)?.metadata.as_ref()?.get("approximate_interval_ms")?.as_u64()
}

/// Run the requote-rate lint over every block digest.
pub(super) fn check(blocks: &[BlockInfo], registry: &Registry, tables: &Tables, diagnostics: &mut Vec<Diagnostic>) {
    for info in blocks {
        if !info.has_requote || info.guard_failed || info.has_then {
            continue;
        }
        for x in &info.triggered {
            let Some(entry) = tables.triggers.get(x) else {
                continue; // unknown trigger: family A reported it
            };
            let unpaced = entry.kind == TriggerKind::Condition
                && entry.fire == Fire::IfTrue
                && entry.rearm_mode == RearmMode::Immediate
                && entry.cooldown_ms.is_none();
            if !unpaced || !entry.signal_set_known {
                continue;
            }
            // The fastest signal the trigger reads, by nickname.
            let mut fast: Vec<(&str, u64)> = entry
                .signal_set
                .iter()
                .filter_map(|nick| match tables.signals.get(nick) {
                    Some(SignalEntry::Resolved(i)) => interval_ms(registry, *i).map(|ms| (nick.as_str(), ms)),
                    _ => None,
                })
                .filter(|(_, ms)| *ms < SUB_SECOND_MS)
                .collect();
            fast.sort_by_key(|(_, ms)| *ms);
            let Some((nick, ms)) = fast.first() else {
                continue;
            };
            let cadence = if *ms == 0 { "on every event".to_string() } else { format!("about every {ms} ms") };
            diagnostics.push(
                Diagnostic::rule(
                    DiagCode::LintRequoteRate,
                    DiagPath::root().key("legs").index(info.leg).key("body").index(info.block),
                    format!(
                        "block `{}` submits an order on every wake of `{x}`, which fires on \
                         every frame of `{nick}` ({cadence}); the runtime will coalesce and \
                         rate-limit it, so the strategy trades at the gate's pace, not yours",
                        info.name
                    ),
                )
                .with_help(
                    "add a `cooldown` to the trigger's rearm (an `if_true` trigger with a \
                     cooldown fires at most once per cooldown, with the latest snapshot), or \
                     gate on a crossing",
                ),
            );
        }
    }
}
