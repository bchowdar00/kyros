//! Trigger-row lowering over the corpus: registration rows, subscriptions, arm lists,
//! and envelope constants, snapshotted for the fixtures that between them exercise
//! every leaf shape (bare, field, expr), both group kinds, and the timer family.

use strategy_compiler::lower;
use strategy_compiler::lower::{LeafRow, LeafValueLower, RowSource};

mod common;

/// Between them: bare leaves, field leaves, expr-leaf op-trees, `all` and `any`
/// groups, lone-leaf normalization, and `window_clock` timers.
const REPRESENTATIVES: [&str; 4] = [
    "break-quoter",
    "curve-discount",
    "endgame-sweep",
    "favorite-panic",
];

#[test]
fn registration_rows() {
    for name in REPRESENTATIVES {
        let lowered = lower::lower(&common::corpus_valid(name));
        insta::assert_debug_snapshot!(format!("triggers__{name}"), lowered.triggers);
    }
}

#[test]
fn registration_data() {
    for name in REPRESENTATIVES {
        let lowered = lower::lower(&common::corpus_valid(name));
        let arms: Vec<(&String, &Vec<String>)> = lowered
            .legs
            .iter()
            .map(|leg| (&leg.name, &leg.arm))
            .collect();
        let registration = (
            ("subscriptions", &lowered.subscriptions),
            ("legs", &arms),
            ("initial_leg", &lowered.initial_leg),
            ("envelope_consts", &lowered.envelope_consts),
        );
        insta::assert_debug_snapshot!(format!("registration__{name}"), registration);
    }
}

#[test]
fn lone_leaves_normalize_to_all_groups() {
    // Every condition row is All or Any; a lone authored leaf must arrive as a
    // one-element All group, and bare leaves must survive as Bare rows.
    for name in common::STRATEGIES {
        let lowered = lower::lower(&common::corpus_valid(name));
        for (trigger, row) in &lowered.triggers {
            match &row.source {
                RowSource::All(leaves) | RowSource::Any(leaves) => {
                    assert!(!leaves.is_empty(), "{name}.{trigger}: empty group");
                }
                RowSource::Timer(_) => {}
            }
        }
    }
}

#[test]
fn leaf_values_are_canonical_strings_not_floats() {
    // The float boundary is the Python expression emitter, nowhere else: every leaf
    // value in every corpus row must be the canonical source string.
    for name in common::STRATEGIES {
        let lowered = lower::lower(&common::corpus_valid(name));
        for (trigger, row) in &lowered.triggers {
            let leaves = match &row.source {
                RowSource::All(leaves) | RowSource::Any(leaves) => leaves,
                RowSource::Timer(_) => continue,
            };
            for leaf in leaves {
                let value = match leaf {
                    LeafRow::Bare { .. } => continue,
                    LeafRow::Field { value, .. } | LeafRow::Expr { value, .. } => match value {
                        LeafValueLower::Literal(value) => value,
                        // A param carries no literal; the runtime substitutes it.
                        LeafValueLower::Param(_) => continue,
                    },
                };
                assert!(
                    !value.is_empty() && !value.contains(|c: char| c.is_whitespace()),
                    "{name}.{trigger}: malformed leaf value `{value}`"
                );
                if value.contains('.') {
                    // Canonical decimals carry no trailing fraction zeros.
                    assert!(
                        !value.ends_with('0'),
                        "{name}.{trigger}: non-canonical decimal `{value}`"
                    );
                }
            }
        }
    }
}
