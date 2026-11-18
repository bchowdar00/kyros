//! Artifact identity: the canonical serialization is deterministic, and the
//! strategy id derives from the source hash exactly as spec'd.

use strategy_compiler::lower;

mod common;

#[test]
fn source_hash_is_deterministic_across_parses() {
    for name in common::STRATEGIES {
        let text = common::corpus_text(name);
        let a: strategy_compiler::ir::Strategy = serde_json::from_str(&text).expect("fixture parses");
        let b: strategy_compiler::ir::Strategy = serde_json::from_str(&text).expect("fixture parses");
        assert_eq!(
            lower::identity::canonical_document(&a),
            lower::identity::canonical_document(&b),
            "{name}: canonical serialization must not vary across parses"
        );
        assert_eq!(
            lower::identity::source_hash(&a),
            lower::identity::source_hash(&b)
        );
    }
}

#[test]
fn strategy_id_is_strategy_plus_twelve_hex() {
    let valid = common::corpus_valid("favorite-panic");
    let lowered = lower::lower(&valid);
    assert_eq!(lowered.source_hash.len(), 64);
    assert!(lowered.source_hash.bytes().all(|b| b.is_ascii_hexdigit()));
    assert_eq!(
        lowered.strategy_id,
        format!("Strategy_{}", &lowered.source_hash[..12])
    );
}

#[test]
fn key_order_and_whitespace_do_not_change_the_hash() {
    // Same document, reordered name-keyed map entries and different whitespace:
    // the canonical re-serialization must erase both.
    let text = common::corpus_text("favorite-panic");
    let value: serde_json::Value = serde_json::from_str(&text).expect("fixture parses");
    let reserialized = serde_json::to_string_pretty(&value).expect("value serializes");
    let a: strategy_compiler::ir::Strategy = serde_json::from_str(&text).expect("fixture parses");
    let b: strategy_compiler::ir::Strategy = serde_json::from_str(&reserialized).expect("reserialized parses");
    assert_eq!(
        lower::identity::source_hash(&a),
        lower::identity::source_hash(&b)
    );
}
