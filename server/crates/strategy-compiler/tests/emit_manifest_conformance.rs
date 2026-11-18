//! Manifest wire-schema conformance, CI-enforced since the pin.
//!
//! Every emitted manifest must validate against the PINNED wire schemas — the
//! `tests/schemas/` at the commit recorded in [`strategy_compiler::emit::SCHEMA_PIN`]
//! (`strategy-schema.schema.json` v2, resolving into `common.schema.json`). A
//! failure here means the emitter and the wire contract have drifted; fix the
//! emitter or fork the schemas, never ignore the test.

use std::fs;
use std::path::{Path, PathBuf};

use jsonschema::{Retrieve, Uri};

use strategy_compiler::{emit, lower};

mod common;

fn schemas_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/schemas")
}

/// Resolves `$ref`s by mapping each `https://motionmarkets.dev/schemas/<file>`
/// URI onto the pinned submodule file of the same name — `$id`-relative
/// resolution intact, no network.
struct PinnedSchemas;

impl Retrieve for PinnedSchemas {
    fn retrieve(
        &self,
        uri: &Uri<String>,
    ) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
        let file = uri
            .path()
            .as_str()
            .rsplit('/')
            .next()
            .ok_or_else(|| format!("no file segment in schema uri {uri}"))?
            .to_string();
        let text = fs::read_to_string(schemas_dir().join(&file))
            .map_err(|e| format!("pinned schema {file} reads: {e}"))?;
        Ok(serde_json::from_str(&text)?)
    }
}

#[test]
fn manifests_conform_to_the_pinned_wire_schemas() {
    let schema_text = fs::read_to_string(schemas_dir().join("strategy-schema.schema.json"))
        .expect("pinned strategy-schema reads");
    let schema: serde_json::Value =
        serde_json::from_str(&schema_text).expect("pinned strategy-schema parses");
    let validator = jsonschema::options()
        .with_retriever(PinnedSchemas)
        .build(&schema)
        .expect("pinned strategy-schema compiles");

    for name in common::STRATEGIES {
        let lowered = lower::lower(&common::corpus_valid(name));
        let text = emit::emit_manifest(&lowered);

        // The emission round-trips through the manifest types byte-identically.
        let manifest: strategy_compiler::ir::Manifest =
            serde_json::from_str(&text).unwrap_or_else(|e| panic!("{name} round-trips: {e}"));
        assert_eq!(text, {
            let mut again = serde_json::to_string_pretty(&manifest).expect("serializes");
            again.push('\n');
            again
        });

        // And it validates against the pinned wire contract.
        let instance: serde_json::Value = serde_json::from_str(&text).expect("manifest is JSON");
        let errors: Vec<String> = validator
            .iter_errors(&instance)
            .map(|error| format!("  at {}: {}", error.instance_path(), error))
            .collect();
        assert!(
            errors.is_empty(),
            "{name} manifest violates the pinned wire schemas:\n{}",
            errors.join("\n")
        );
    }
}

// ---- verification probe: the tiny fixtures against the venue registry ---------------

const TINY_FIXTURES: &[&str] = &[
    "fair-discount",
    "entry-tracker",
    "leg-exit",
    "follow-whale",
    "periodic-requoter",
    "rolling-spread",
    "post-only-maker",
    "combined-everything",
    "momentum-flip",
    "scale-in",
    "two-sided-quoter",
    "re-entry",
    "every-time-cooldown",
];

fn venue_registry() -> strategy_compiler::ir::Registry {
    let text = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("registry/tiny.json"))
        .expect("venue registry reads");
    serde_json::from_str(&text).expect("venue registry parses")
}

fn tiny_fixture(name: &str) -> strategy_compiler::ir::Strategy {
    let text = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("tests/fixtures/tiny/{name}.json")),
    )
    .unwrap_or_else(|e| panic!("{name} fixture reads: {e}"));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{name} fixture parses: {e}"))
}

#[test]
fn tiny_manifests_conform_to_the_pinned_wire_schemas() {
    let schema_text = fs::read_to_string(schemas_dir().join("strategy-schema.schema.json"))
        .expect("pinned strategy-schema reads");
    let schema: serde_json::Value = serde_json::from_str(&schema_text).expect("parses");
    let validator = jsonschema::options()
        .with_retriever(PinnedSchemas)
        .build(&schema)
        .expect("compiles");

    for name in TINY_FIXTURES {
        let output = emit::build(&tiny_fixture(name), &venue_registry(), &common::envelope(), "0.1.0");
        assert!(
            !output.has_errors(),
            "{name} failed to build:\n{}",
            common::render(&output.diagnostics)
        );
        let text = output.artifacts.expect("artifacts").manifest_json;
        let instance: serde_json::Value = serde_json::from_str(&text).expect("manifest is JSON");
        let errors: Vec<String> = validator
            .iter_errors(&instance)
            .map(|error| format!("  at {}: {}", error.instance_path(), error))
            .collect();
        assert!(
            errors.is_empty(),
            "{name} manifest violates the pinned wire schemas:\n{}",
            errors.join("\n")
        );
    }
}
