//! Round-trip fidelity over the corpus: every document parses into the IR types,
//! re-serializes to a JSON value identical to the source (lossless), and emits a
//! byte-deterministic canonical form (snapshotted).
//!
//! This is the whole contract of the `ir` crate at this stage — if a field is dropped,
//! renamed, defaulted, or reordered into a different *value*, the Value-equality check
//! fails; anything the types silently ignore goes missing on re-serialization. The
//! canonical-emission snapshots are the determinism witness: the same document must
//! produce identical bytes, forever.

use std::fs;
use std::path::{Path, PathBuf};

// `favorite-panic-empty-params` is deliberately absent: its `"params": []` is dropped
// on re-serialization (skip_serializing_if), which is what keeps its source_hash equal
// to favorite-panic's — see emit_program. It cannot round-trip losslessly by design.
const STRATEGIES: [&str; 8] = [
    "break-quoter",
    "capitulation-fade",
    "curve-discount",
    "endgame-sweep",
    "favorite-panic",
    "late-momentum",
    "momentum-flip",
    "pinnacle-bid",
];

fn corpus_dir() -> PathBuf {
    // A virtual workspace manifest has no build target, so tests live here and reach
    // the repo-root corpus relative to this crate's manifest.
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus")
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| panic!("{} could not be read: {e}", path.display()))
}

fn parse<T: serde::de::DeserializeOwned>(path: &Path, text: &str) -> T {
    serde_json::from_str(text).unwrap_or_else(|e| panic!("{} failed to parse: {e}", path.display()))
}

#[test]
fn strategies_round_trip_losslessly() {
    for name in STRATEGIES {
        let path = corpus_dir().join(format!("{name}.json"));
        let text = read(&path);
        let typed: strategy_compiler::ir::Strategy = parse(&path, &text);
        let reserialized = serde_json::to_value(&typed)
            .unwrap_or_else(|e| panic!("{name} failed to re-serialize: {e}"));
        let original: serde_json::Value = parse(&path, &text);
        assert_eq!(
            original, reserialized,
            "{name} did not round-trip through the IR types"
        );
    }
}

#[test]
fn canonical_emission_is_byte_deterministic_and_snapshotted() {
    for name in STRATEGIES {
        let path = corpus_dir().join(format!("{name}.json"));
        let text = read(&path);
        let first: strategy_compiler::ir::Strategy = parse(&path, &text);
        let second: strategy_compiler::ir::Strategy = parse(&path, &text);
        let first_emission =
            serde_json::to_string_pretty(&first).expect("canonical emission serializes");
        let second_emission =
            serde_json::to_string_pretty(&second).expect("canonical emission serializes");
        assert_eq!(
            first_emission, second_emission,
            "{name}: two parses emitted different bytes"
        );
        insta::assert_snapshot!(format!("canonical__{name}"), first_emission);
    }
}

#[test]
fn registry_round_trips() {
    let path = corpus_dir().join("registry/registry.json");
    let text = read(&path);
    let typed: strategy_compiler::ir::Registry = parse(&path, &text);
    let reserialized = serde_json::to_value(&typed).expect("registry re-serializes");
    let original: serde_json::Value = parse(&path, &text);
    assert_eq!(
        original, reserialized,
        "registry.json did not round-trip through the IR types"
    );
}
