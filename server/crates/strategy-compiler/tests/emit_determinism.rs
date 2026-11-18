//! Determinism over the whole emit path: two independent runs from fresh parses
//! produce byte-identical program and manifest for every corpus fixture. Same
//! (document, compiler version) -> same bytes, always.

use strategy_compiler::{emit, lower};

mod common;

const VERSION: &str = "0.1.0";

fn artifacts(name: &str) -> (String, String) {
    let lowered = lower::lower(&common::corpus_valid(name));
    let program = emit::emit_program(&lowered, VERSION);
    let manifest = emit::emit_manifest(&lowered);
    (program, manifest)
}

#[test]
fn emit_twice_is_byte_identical() {
    for name in common::STRATEGIES {
        let first = artifacts(name);
        let second = artifacts(name);
        assert_eq!(first.0, second.0, "{name}: program drifted");
        assert_eq!(first.1, second.1, "{name}: manifest drifted");
    }
}

#[test]
fn both_artifacts_agree_on_identity() {
    for name in common::STRATEGIES {
        let (program, manifest) = artifacts(name);
        let program: serde_json::Value = serde_json::from_str(&program).expect("program parses");
        let manifest: serde_json::Value =
            serde_json::from_str(&manifest).expect("manifest parses");
        assert_eq!(program["strategy_id"], manifest["strategy_id"], "{name}");
        assert_eq!(program["source_hash"], manifest["source_hash"], "{name}");
        let source = program["source_hash"].as_str().expect("source hash");
        assert_eq!(
            program["strategy_id"],
            format!("Strategy_{}", &source[..12]).as_str(),
            "{name}: id derives from the source hash"
        );
    }
}
