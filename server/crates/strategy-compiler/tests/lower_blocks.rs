//! Block-plan lowering: per-block snapshot read sets (transitive through named
//! expressions) and the intent index (totalized on-tables, resolved max_size).

use strategy_compiler::lower;

mod common;

/// Between them: on-tables (favorite-panic), a named expression and constant table
/// feeding order expressions (curve-discount), and cancel-replace (pinnacle-bid).
const REPRESENTATIVES: [&str; 3] = ["curve-discount", "favorite-panic", "pinnacle-bid"];

#[test]
fn block_read_sets() {
    for name in REPRESENTATIVES {
        let lowered = lower::lower(&common::corpus_valid(name));
        let reads: Vec<(String, Vec<String>)> = lowered
            .legs
            .iter()
            .flat_map(|leg| {
                leg.blocks.iter().map(|block| {
                    (
                        format!("{}.{}", leg.name, block.name),
                        block
                            .reads
                            .iter()
                            .map(|(nick, field)| format!("{nick}.{field}"))
                            .collect(),
                    )
                })
            })
            .collect();
        insta::assert_debug_snapshot!(format!("reads__{name}"), reads);
    }
}

#[test]
fn intent_index() {
    for name in REPRESENTATIVES {
        let lowered = lower::lower(&common::corpus_valid(name));
        insta::assert_debug_snapshot!(format!("intent_index__{name}"), lowered.intent_index);
    }
}

#[test]
fn intent_ids_use_the_diagnostic_path_vocabulary() {
    // "<leg>.<block>.<do-index>", every segment a declared name, for every corpus
    // fixture — and every place/cancel_replace intent has exactly one index entry.
    for name in common::STRATEGIES {
        let lowered = lower::lower(&common::corpus_valid(name));
        let mut expected = 0;
        for leg in &lowered.legs {
            for block in &leg.blocks {
                for (d, item) in block.items.iter().enumerate() {
                    let intent_id = match item {
                        lower::PlanItem::Intent(lower::IntentLower::Place {
                            intent_id, ..
                        })
                        | lower::PlanItem::Intent(lower::IntentLower::CancelReplace {
                            intent_id,
                            ..
                        }) => intent_id,
                        _ => continue,
                    };
                    expected += 1;
                    assert_eq!(
                        *intent_id,
                        format!("{}.{}.{d}", leg.name, block.name),
                        "{name}: intent id must be <leg>.<block>.<do-index>"
                    );
                    assert!(
                        lowered.intent_index.contains_key(intent_id),
                        "{name}: {intent_id} missing from the intent index"
                    );
                }
            }
        }
        assert_eq!(
            lowered.intent_index.len(),
            expected,
            "{name}: index entries must map one-to-one onto id-bearing intents"
        );
    }
}

#[test]
fn resolved_max_size_is_the_envelope_decimal() {
    // Every corpus max_size is `envelope.max_order_size`; the test envelope says 100.
    for name in common::STRATEGIES {
        let lowered = lower::lower(&common::corpus_valid(name));
        for (intent_id, entry) in &lowered.intent_index {
            assert_eq!(
                entry.max_size, "100",
                "{name}.{intent_id}: max_size must resolve through the envelope"
            );
        }
    }
}
