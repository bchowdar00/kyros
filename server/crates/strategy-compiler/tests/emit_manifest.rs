//! The manifest projection over the corpus: all eight fixtures' manifest.json,
//! snapshotted byte-for-byte.

use strategy_compiler::{emit, lower};

mod common;

#[test]
fn corpus_manifests() {
    for name in common::STRATEGIES {
        let lowered = lower::lower(&common::corpus_valid(name));
        let manifest = emit::emit_manifest(&lowered);
        insta::assert_snapshot!(format!("manifest__{name}"), manifest);
    }
}
