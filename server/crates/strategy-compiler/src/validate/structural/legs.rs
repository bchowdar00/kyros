//! Non-resident legs need a way out.
//!
//! The SIMPLE rule, deliberately: an exit EDGE must exist — a block `then` naming a
//! different leg, or an on-table destination naming a different leg. Whether the edge
//! is reachable under the guards, or whether the destination chain ever reaches a
//! resident leg, is a flow analysis this rule does not attempt (recorded limitation;
//! revisit when a fixture demands it).

use crate::ir::{DoItem, OnDest, Strategy};

use crate::validate::diag::{DiagCode, DiagPath, Diagnostic};

/// Check every non-resident leg for an exit edge.
pub fn check(strategy: &Strategy, diagnostics: &mut Vec<Diagnostic>) {
    for (l, leg) in strategy.legs.iter().enumerate() {
        if leg.resident || has_exit_edge(leg) {
            continue;
        }
        diagnostics.push(
            Diagnostic::rule(
                DiagCode::NonResidentNoExit,
                DiagPath::root().key("legs").index(l),
                format!(
                    "leg `{}` is not resident but has no transition out",
                    leg.name
                ),
            )
            .with_help(
                "add a `then` or an on-table route to another leg, or declare the leg \
                 resident",
            ),
        );
    }
}

fn has_exit_edge(leg: &crate::ir::Leg) -> bool {
    for block in &leg.body {
        if let Some(then) = &block.then
            && *then != leg.name
        {
            return true;
        }
        for item in &block.do_items {
            let on = match item {
                DoItem::Place(place_item) => place_item.place.on.as_ref(),
                DoItem::TargetPosition(target_item) => target_item.target_position.on.as_ref(),
                DoItem::Statement(_) | DoItem::Cancel(_) | DoItem::CancelReplace(_) => None,
            };
            let Some(on) = on else { continue };
            let dests = [
                &on.fill,
                &on.partial,
                &on.reject,
                &on.cancel_ack,
                &on.expiry,
            ];
            if dests
                .into_iter()
                .flatten()
                .any(|dest| matches!(dest, OnDest::Leg(name) if *name != leg.name))
            {
                return true;
            }
        }
    }
    false
}
