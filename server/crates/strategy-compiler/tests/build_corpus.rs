//! The success criterion, end to end: all eight corpus fixtures compile through the
//! library build path (validate -> gate -> lower -> emit) to a program and a
//! manifest that agree on identity, and the typed program matches its serialized
//! form.

use strategy_compiler::emit;

mod common;

const VERSION: &str = "0.1.0";

#[test]
fn corpus_builds_to_programs() {
    for name in common::STRATEGIES {
        let strategy: strategy_compiler::ir::Strategy =
            serde_json::from_str(&common::corpus_text(name)).expect("fixture parses");
        let output = emit::build(&strategy, &common::registry(), &common::envelope(), VERSION);
        assert!(
            !output.has_errors(),
            "{name} failed to build:\n{}",
            common::render(&output.diagnostics)
        );
        let artifacts = output.artifacts.expect("no errors yields artifacts");

        assert!(!artifacts.program_json.is_empty(), "{name}: empty program");
        assert!(
            !artifacts.manifest_json.is_empty(),
            "{name}: empty manifest"
        );
        assert_eq!(
            artifacts.program_json,
            emit::serialize_program(&artifacts.program),
            "{name}: typed and serialized programs disagree"
        );

        let manifest: serde_json::Value =
            serde_json::from_str(&artifacts.manifest_json).expect("manifest parses");
        assert_eq!(
            manifest["strategy_id"],
            artifacts.program.strategy_id.as_str(),
            "{name}: strategy_id"
        );
        assert_eq!(
            artifacts.program.strategy_id,
            format!("Strategy_{}", &artifacts.program.source_hash[..12]),
            "{name}: id derives from the source hash"
        );
    }
}

#[test]
fn compile_entry_point_matches_build() {
    for name in common::STRATEGIES {
        let strategy: strategy_compiler::ir::Strategy =
            serde_json::from_str(&common::corpus_text(name)).expect("fixture parses");
        let compiled = strategy_compiler::compile(&strategy, &common::registry(), &common::envelope())
            .unwrap_or_else(|diagnostics| {
                panic!("{name} failed to compile:\n{}", common::render(&diagnostics))
            });
        assert_eq!(
            compiled.artifacts.program.compiler_version,
            strategy_compiler::VERSION
        );
        assert!(compiled.warnings.iter().all(|d| !d.is_error()));
    }
}
