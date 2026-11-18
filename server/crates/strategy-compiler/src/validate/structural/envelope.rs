//! Envelope sanity — TWO HALVES, CALLED AT DIFFERENT PIPELINE POINTS.
//!
//! `check_sanity` reads only the envelope document and runs with the rest of family D
//! (P4). `check_read_set` runs AFTER the orders pass (P5) because its input — the set
//! of envelope fields the program actually reads — is collected from post-substitution
//! ASTs and does not exist earlier. Merging the halves into one call would either move
//! this check before its input exists or drag all of family D behind P5; the split is
//! load-bearing, not an accident (see also the module doc on `structural`).
//!
//! Envelope diagnostics use the pseudo-path `envelope.<field>` — the envelope is a
//! compiler input, not a document section; the path contract still holds and the
//! section ranks it last.

use std::collections::BTreeSet;

use crate::ir::Envelope;

use crate::validate::context::EnvelopeField;
use crate::validate::decimal::cmp_decimal;
use crate::validate::diag::{DiagCode, DiagPath, Diagnostic};

/// The P4 half: internal consistency of the envelope document itself.
pub fn check_sanity(envelope: &Envelope, diagnostics: &mut Vec<Diagnostic>) {
    for (field, value) in [
        ("max_exposure_fraction", &envelope.max_exposure_fraction),
        ("pause_at_consumed", &envelope.pause_at_consumed),
    ] {
        if let Some(fraction) = value
            && cmp_decimal(fraction.as_str(), "1") == std::cmp::Ordering::Greater
        {
            diagnostics.push(
                Diagnostic::rule(
                    DiagCode::EnvelopeFractionOutOfRange,
                    DiagPath::root().key("envelope").key(field),
                    format!(
                        "`{field}` is a fraction of 1 — `{}` is out of range",
                        fraction.as_str()
                    ),
                )
                .with_help("write the fraction in [0, 1]; 0.5 means half"),
            );
        }
    }

    if cmp_decimal(envelope.loss_budget.as_str(), envelope.allocation.as_str())
        == std::cmp::Ordering::Greater
    {
        diagnostics.push(
            Diagnostic::rule(
                DiagCode::EnvelopeBudgetExceedsAllocation,
                DiagPath::root().key("envelope").key("loss_budget"),
                format!(
                    "loss budget {} exceeds the allocation {} — the strategy could lose \
                     more than it escrows",
                    envelope.loss_budget.as_str(),
                    envelope.allocation.as_str()
                ),
            )
            .with_help("loss_budget must be at most allocation"),
        );
    }

    if let Some(round_budget) = &envelope.round_budget
        && cmp_decimal(round_budget.as_str(), envelope.allocation.as_str())
            == std::cmp::Ordering::Greater
    {
        diagnostics.push(
            Diagnostic::rule(
                DiagCode::EnvelopeBudgetExceedsAllocation,
                DiagPath::root().key("envelope").key("round_budget"),
                format!(
                    "round budget {} exceeds the allocation {} — a window could commit \
                     more than the strategy escrows",
                    round_budget.as_str(),
                    envelope.allocation.as_str()
                ),
            )
            .with_help("round_budget must be at most allocation"),
        );
    }
}

/// The post-P5 half: every envelope field the program reads must be present on the
/// supplied envelope ("an envelope missing a read field does not load").
pub fn check_read_set(
    envelope: &Envelope,
    reads: &BTreeSet<EnvelopeField>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    for field in reads {
        let present = match field {
            // The survival core is required by the type; only the optional fields
            // can be missing.
            EnvelopeField::Allocation | EnvelopeField::LossBudget | EnvelopeField::MaxOrderSize => {
                true
            }
            EnvelopeField::RoundBudget => envelope.round_budget.is_some(),
            EnvelopeField::MaxPositionPerMarket => envelope.max_position_per_market.is_some(),
            EnvelopeField::MaxOpenOrders => envelope.max_open_orders.is_some(),
        };
        if !present {
            diagnostics.push(
                Diagnostic::rule(
                    DiagCode::EnvelopeMissingReadField,
                    DiagPath::root().key("envelope").key(field.as_str()),
                    format!(
                        "the program reads envelope.{} but the supplied envelope omits it",
                        field.as_str()
                    ),
                )
                .with_help("supply the field, or drop the read — an envelope missing a read field does not load"),
            );
        }
    }
}
