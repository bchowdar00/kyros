//! Intent outcomes against the registry: required iff the instrument's market is
//! outcome-structured, and the label must match a registry outcome verbatim.
//!
//! Registry outcomes are venue data (`String`); IR outcomes are authored identifiers
//! (`Ident`) — plain string equality is the bridge. Note `CancelReplace.outcome` is a
//! required field in the IR types, so a linear-market cancel-replace is unrepresentable
//! there (flagged on the ir follow-up list, ruled out of scope here); its outcome still
//! label-checks like any other.

use crate::ir::{DoItem, MarketStructure, Registry, Strategy};

use crate::validate::diag::{DiagCode, DiagPath, Diagnostic};
use crate::validate::poison::Poisons;
use crate::validate::tables::{InstrumentEntry, Tables};

/// Check every place and cancel-replace outcome.
pub fn check(
    strategy: &Strategy,
    registry: &Registry,
    tables: &Tables,
    poisons: &Poisons,
    diagnostics: &mut Vec<Diagnostic>,
) {
    for (l, leg) in strategy.legs.iter().enumerate() {
        for (b, block) in leg.body.iter().enumerate() {
            for (d, item) in block.do_items.iter().enumerate() {
                let base = DiagPath::root()
                    .key("legs")
                    .index(l)
                    .key("body")
                    .index(b)
                    .key("do")
                    .index(d);
                match item {
                    DoItem::Place(place_item) => {
                        let place = &place_item.place;
                        check_outcome(
                            place.instrument.as_str(),
                            place.outcome.as_ref().map(|o| o.as_str()),
                            base.key("place"),
                            registry,
                            tables,
                            poisons,
                            diagnostics,
                        );
                    }
                    DoItem::CancelReplace(replace_item) => {
                        let replace = &replace_item.cancel_replace;
                        check_outcome(
                            replace.instrument.as_str(),
                            Some(replace.outcome.as_str()),
                            base.key("cancel_replace"),
                            registry,
                            tables,
                            poisons,
                            diagnostics,
                        );
                    }
                    DoItem::TargetPosition(target_item) => {
                        let target = &target_item.target_position;
                        check_outcome(
                            target.instrument.as_str(),
                            target.outcome.as_ref().map(|o| o.as_str()),
                            base.key("target_position"),
                            registry,
                            tables,
                            poisons,
                            diagnostics,
                        );
                    }
                    DoItem::Statement(_) | DoItem::Cancel(_) => {}
                }
            }
        }
    }
}

fn check_outcome(
    instrument: &str,
    outcome: Option<&str>,
    base: DiagPath,
    registry: &Registry,
    tables: &Tables,
    poisons: &Poisons,
    diagnostics: &mut Vec<Diagnostic>,
) {
    // Undeclared (family A) or unresolved (`unknown-instrument-id`): suppressed.
    if poisons.unresolved_instruments.contains(instrument) {
        return;
    }
    let (structure, outcomes) = match tables.instruments.get(instrument) {
        Some(InstrumentEntry::Market(index)) => {
            let market = &registry.markets[*index];
            (market.structure, market.outcomes.as_deref())
        }
        Some(InstrumentEntry::Series(index)) => {
            let series = &registry.series[*index];
            (series.structure, series.outcomes.as_deref())
        }
        Some(InstrumentEntry::Unresolved) | None => return,
    };

    match (structure, outcome) {
        (MarketStructure::Outcome, None) => {
            let labels = outcomes.unwrap_or_default().join(", ");
            diagnostics.push(
                Diagnostic::rule(
                    DiagCode::OutcomeRequired,
                    base.key("outcome"),
                    format!(
                        "instrument `{instrument}` is outcome-structured — the intent must name an outcome"
                    ),
                )
                .with_help(format!("outcomes: {labels}")),
            );
        }
        (MarketStructure::Outcome, Some(label)) => {
            let known = outcomes.unwrap_or_default();
            if !known.iter().any(|o| o == label) {
                diagnostics.push(
                    Diagnostic::rule(
                        DiagCode::UnknownOutcome,
                        base.key("outcome"),
                        format!(
                            "`{label}` is not an outcome of `{instrument}` (outcomes: {})",
                            known.join(", ")
                        ),
                    )
                    .with_help("outcome labels are registry venue data, matched verbatim"),
                );
            }
        }
        (MarketStructure::Linear, Some(label)) => {
            diagnostics.push(
                Diagnostic::rule(
                    DiagCode::OutcomeForbidden,
                    base.key("outcome"),
                    format!(
                        "instrument `{instrument}` is linear — `{label}` names an outcome it does not have"
                    ),
                )
                .with_help("omit `outcome` on linear markets"),
            );
        }
        (MarketStructure::Linear, None) => {}
    }
}
