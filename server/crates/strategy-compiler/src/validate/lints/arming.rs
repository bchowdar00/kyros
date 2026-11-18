//! Arm-set warnings: a `triggered()` test its leg never arms (the block can never
//! run there), and a trigger no leg arms (it can never fire). Warnings (ruled
//! 2026-07-20, Q8): dead code is a smell, not a broken artifact.

use std::collections::BTreeSet;

use crate::ir::Strategy;

use crate::validate::diag::{DiagCode, DiagPath, Diagnostic};
use crate::validate::tables::Tables;

use super::BlockInfo;

/// Run the arm-set warnings.
pub(super) fn check(
    strategy: &Strategy,
    tables: &Tables,
    blocks: &[BlockInfo],
    diagnostics: &mut Vec<Diagnostic>,
) {
    // triggered(X) in a leg that does not arm X.
    for info in blocks {
        let armed: BTreeSet<&str> = strategy.legs[info.leg]
            .arm
            .iter()
            .map(|t| t.as_str())
            .collect();
        for x in &info.triggered {
            if !tables.triggers.contains_key(x) {
                continue; // unknown trigger: family A reported it
            }
            if !armed.contains(x.as_str()) {
                diagnostics.push(
                    Diagnostic::rule(
                        DiagCode::TriggeredNotArmed,
                        DiagPath::root()
                            .key("legs")
                            .index(info.leg)
                            .key("body")
                            .index(info.block)
                            .key("if"),
                        format!(
                            "`triggered({x})` can never be true in leg `{}` — the leg does \
                             not arm it",
                            strategy.legs[info.leg].name
                        ),
                    )
                    .with_help("add the trigger to the leg's arm list, or drop the test"),
                );
            }
        }
    }

    // A trigger no leg arms.
    let armed_anywhere: BTreeSet<&str> = strategy
        .legs
        .iter()
        .flat_map(|leg| leg.arm.iter().map(|t| t.as_str()))
        .collect();
    for (t, trigger) in strategy.triggers.iter().enumerate() {
        if !armed_anywhere.contains(trigger.name.as_str()) {
            diagnostics.push(
                Diagnostic::rule(
                    DiagCode::TriggerUnarmed,
                    DiagPath::root().key("triggers").index(t),
                    format!(
                        "trigger `{}` is armed by no leg and can never fire",
                        trigger.name
                    ),
                )
                .with_help("arm it in some leg, or delete it"),
            );
        }
    }
}
