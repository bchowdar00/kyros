//! The missed-wake lint (signed-off formalization, 2026-07-20): block B warns when
//! ALL of —
//!
//! 1. B's guard tests `triggered(X)` where X is a condition trigger with
//!    `fire: on_crossing` (an `if_true` trigger wakes on every event while true and
//!    cannot miss);
//! 2. B's guard also reads some state slot S;
//! 3. another block in the same leg writes S without being gated on X.
//!
//! Then S can cross B's threshold on a wake where X (edge-fired) did not fire, and B
//! silently never runs. The fix is usually `fire: if_true` on X (ir-spec §9.4), or a
//! plain condition guard.
//!
//! Recorded blind spots (accepted with the formalization): writers in OTHER legs are
//! not considered (leg transitions re-enter through fresh activations, but a
//! cross-leg counter feeding an edge-gated block would escape this lint); `push`
//! counts as a write of its window; and the check is syntactic — it does not
//! prove the guard's threshold can actually be crossed by the writes, so a guard
//! reading S for reasons other than a threshold still warns.

use crate::validate::diag::{DiagCode, DiagPath, Diagnostic};
use crate::validate::tables::{Tables, TriggerKind};

use super::BlockInfo;

/// Run the missed-wake lint over every block digest.
pub(super) fn check(blocks: &[BlockInfo], tables: &Tables, diagnostics: &mut Vec<Diagnostic>) {
    for info in blocks {
        if info.guard_failed || info.guard_state_reads.is_empty() {
            continue;
        }
        for x in &info.triggered {
            let Some(entry) = tables.triggers.get(x) else {
                continue; // unknown trigger: family A reported it
            };
            if entry.kind != TriggerKind::Condition || entry.fire != crate::ir::Fire::OnCrossing {
                continue;
            }
            // Slots B reads that some other block in the leg writes while NOT gated
            // on X.
            let mut hazards: Vec<&str> = Vec::new();
            for other in blocks {
                if other.leg != info.leg || other.block == info.block {
                    continue;
                }
                if other.triggered.iter().any(|t| t == x) {
                    continue; // gated on X: fires together with B's cause
                }
                for slot in other.writes.intersection(&info.guard_state_reads) {
                    hazards.push(slot);
                }
            }
            hazards.sort_unstable();
            hazards.dedup();
            if hazards.is_empty() {
                continue;
            }
            diagnostics.push(
                Diagnostic::rule(
                    DiagCode::LintMissedWake,
                    DiagPath::root()
                        .key("legs")
                        .index(info.leg)
                        .key("body")
                        .index(info.block)
                        .key("if"),
                    format!(
                        "`{}` gates on edge-fired trigger `{x}` but reads state `{}` \
                         written by other blocks — the state can cross its threshold on \
                         a wake where the trigger did not fire, and this block silently \
                         never runs",
                        info.name,
                        hazards.join("`, `")
                    ),
                )
                .with_help(
                    "use fire: if_true on the trigger (the fire-mode idiom), or drop \
                     triggered() for a plain condition guard",
                ),
            );
        }
    }
}
