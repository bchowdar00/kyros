//! The machine-gun lint (signed-off formalization, 2026-07-20): an intent-bearing
//! block warns when NOTHING stops consecutive re-fire — none of:
//!
//! 1. a `then` (the transition leaves the leg, ending re-fire);
//! 2. a PACING `triggered(X)` in the guard — pacing means `fire: on_crossing`, OR
//!    `rearm` in {fresh_crossing, never}, OR a rearm `cooldown` (v0.3); the `if_true` +
//!    `immediate` firehose with no cooldown is exactly what does not pace;
//! 3. a self-flipping state term: the guard reads a slot the block's own do-list
//!    sets.
//!
//! An on table alone does NOT satisfy the lint — it stops re-fire only at the order
//! outcome, not per-wake (ratified).
//!
//! THE INTENT-BEARING SET IS `place` ONLY (ruled 2026-07-20). The lint exists
//! because consecutive `place` fires STACK exposure — each wake adds another live
//! order into the ack gap. `cancel_replace` is structurally self-limiting (it
//! targets its own tag; the book footprint is one order regardless of fire rate),
//! and plain `cancel` is idempotent (no-match is a logged no-op). Those are
//! different failure modes; warning on them would teach the repair loop to add
//! spurious pacing to every maker strategy, and an unpaced re-quote tracking the
//! fair line is the CANONICAL maker idiom, not a bug. The exemption is PER-INTENT:
//! a block carrying both a cancel_replace and a bare place still trips on the place.
//!
//! Recorded blind spots (accepted with the formalization): the self-flip check is
//! syntactic (same slot read and written; it does not prove the write falsifies the
//! guard); a timer trigger's own cadence is treated as pacing only through its
//! fire/rearm modes and cooldown;
//! and the cancel_replace exemption accepts VENUE CHURN (a cancel/replace per
//! constituent tick while the condition holds) as a cost question, not a
//! correctness question; `target_position` (v0.3) is exempt for the same reason —
//! the runtime places nothing when already at target — exactly where ir-spec §9.4 put it, picked up venue-side
//! by the activation-to-trade admission metric. If churn ever needs a compiler-side
//! signal, that is a future rate-shaped lint with its own threshold, not this one.

use crate::validate::diag::{DiagCode, DiagPath, Diagnostic};
use crate::validate::tables::Tables;

use super::BlockInfo;

/// Run the machine-gun lint over every block digest.
pub(super) fn check(blocks: &[BlockInfo], tables: &Tables, diagnostics: &mut Vec<Diagnostic>) {
    for info in blocks {
        if !info.has_place || info.guard_failed || info.has_then {
            continue;
        }
        let paced = info.triggered.iter().any(|x| {
            tables.triggers.get(x).is_some_and(|entry| {
                entry.fire == crate::ir::Fire::OnCrossing
                    || entry.cooldown_ms.is_some()
                    || matches!(
                        entry.rearm_mode,
                        crate::ir::RearmMode::FreshCrossing | crate::ir::RearmMode::Never
                    )
            })
        });
        if paced {
            continue;
        }
        let self_flips = info
            .guard_state_reads
            .intersection(&info.writes)
            .next()
            .is_some();
        if self_flips {
            continue;
        }
        diagnostics.push(
            Diagnostic::rule(
                DiagCode::LintMachineGun,
                DiagPath::root()
                    .key("legs")
                    .index(info.leg)
                    .key("body")
                    .index(info.block),
                format!(
                    "block `{}` places an order but nothing stops it re-firing on \
                     consecutive wakes",
                    info.name
                ),
            )
            .with_help(
                "add a `then` out of the leg, gate on an edge-fired, rearm-limited or \
                 cooled-down trigger, or flip a state slot the guard reads (an on table \
                 does not pace — it acts only at the order outcome)",
            ),
        );
    }
}
