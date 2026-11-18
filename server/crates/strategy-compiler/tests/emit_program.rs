//! The program backend over the corpus: every fixture's emitted program snapshotted
//! byte-for-byte, round-tripped through the typed `Program`, and validated against
//! the schema generated from the program types.

use strategy_compiler::program::Program;
use strategy_compiler::{emit, lower};

mod common;

/// The version stamped into test snapshots. Fixed (not `CARGO_PKG_VERSION`) so a
/// workspace version bump is a reviewed snapshot diff, not a silent one.
const VERSION: &str = "0.1.0";

#[test]
fn corpus_programs() {
    for name in common::STRATEGIES {
        let lowered = lower::lower(&common::corpus_valid(name));
        let program = emit::emit_program(&lowered, VERSION);
        insta::assert_snapshot!(format!("program__{name}"), program);
    }
}

#[test]
fn programs_round_trip_through_the_typed_form() {
    for name in common::STRATEGIES {
        let lowered = lower::lower(&common::corpus_valid(name));
        let typed = emit::project_program(&lowered, VERSION);
        let text = emit::serialize_program(&typed);
        let again: Program = serde_json::from_str(&text)
            .unwrap_or_else(|e| panic!("{name}: emitted program does not parse: {e}"));
        assert_eq!(typed, again, "{name}: program changed across a round trip");
        assert_eq!(
            text,
            emit::serialize_program(&again),
            "{name}: re-serialization drifted"
        );
    }
}

#[test]
fn programs_conform_to_the_generated_schema() {
    let schema = serde_json::to_value(schemars::schema_for!(Program)).expect("schema serializes");
    let validator = jsonschema::validator_for(&schema).expect("program schema compiles");
    for name in common::STRATEGIES {
        let lowered = lower::lower(&common::corpus_valid(name));
        let text = emit::emit_program(&lowered, VERSION);
        let instance: serde_json::Value = serde_json::from_str(&text).expect("program is JSON");
        let errors: Vec<String> = validator
            .iter_errors(&instance)
            .map(|error| format!("  at {}: {}", error.instance_path(), error))
            .collect();
        assert!(
            errors.is_empty(),
            "{name}: program violates its schema:\n{}",
            errors.join("\n")
        );
    }
}

// ---- verification probes: tiny fixtures against the generated Program schema --------

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

fn tiny_build(name: &str) -> strategy_compiler::emit::Artifacts {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let registry: strategy_compiler::ir::Registry = serde_json::from_str(
        &std::fs::read_to_string(dir.join("registry/tiny.json")).expect("registry reads"),
    )
    .expect("registry parses");
    let strategy: strategy_compiler::ir::Strategy = serde_json::from_str(
        &std::fs::read_to_string(dir.join(format!("tests/fixtures/tiny/{name}.json")))
            .expect("fixture reads"),
    )
    .expect("fixture parses");
    let output = emit::build(&strategy, &registry, &common::envelope(), VERSION);
    assert!(!output.has_errors(), "{name}:\n{}", common::render(&output.diagnostics));
    output.artifacts.expect("artifacts")
}

#[test]
fn tiny_programs_conform_to_the_generated_schema() {
    let schema = serde_json::to_value(schemars::schema_for!(Program)).expect("schema serializes");
    let validator = jsonschema::validator_for(&schema).expect("program schema compiles");
    for name in TINY_FIXTURES {
        let text = tiny_build(name).program_json;
        let instance: serde_json::Value = serde_json::from_str(&text).expect("program is JSON");
        let errors: Vec<String> = validator
            .iter_errors(&instance)
            .map(|error| format!("  at {}: {}", error.instance_path(), error))
            .collect();
        assert!(errors.is_empty(), "{name}: program violates its schema:\n{}", errors.join("\n"));
    }
}

#[test]
fn tiny_combined_emits_twice_byte_identical() {
    let a = tiny_build("combined-everything");
    let b = tiny_build("combined-everything");
    assert_eq!(a.program_json, b.program_json, "program drifted");
    assert_eq!(a.manifest_json, b.manifest_json, "manifest drifted");
}

#[test]
fn program_identity_and_shape() {
    for name in common::STRATEGIES {
        let valid = common::corpus_valid(name);
        let lowered = lower::lower(&valid);
        let program = emit::project_program(&lowered, VERSION);

        assert_eq!(program.format, strategy_compiler::program::FORMAT);
        assert_eq!(program.compiler_version, VERSION);
        assert_eq!(program.strategy_id, lowered.strategy_id, "{name}");
        assert_eq!(
            program.strategy_id,
            format!("Strategy_{}", &program.source_hash[..12]),
            "{name}: id derives from the source hash"
        );
        assert_eq!(program.subscriptions, lowered.subscriptions, "{name}");
        assert_eq!(program.initial_leg, program.legs[0].name, "{name}");

        // Every armed trigger and every `then` target exists.
        for leg in &program.legs {
            for armed in &leg.arm {
                assert!(
                    program.triggers.iter().any(|t| &t.name == armed),
                    "{name}.{}: armed trigger {armed} missing",
                    leg.name
                );
            }
            for block in &leg.blocks {
                if let Some(target) = &block.then {
                    assert!(
                        program.legs.iter().any(|l| &l.name == target),
                        "{name}.{}.{}: then target {target} missing",
                        leg.name,
                        block.name
                    );
                }
                for item in &block.items {
                    let intent_id = match item {
                        strategy_compiler::program::Item::Place { intent_id, .. }
                        | strategy_compiler::program::Item::CancelReplace { intent_id, .. } => {
                            intent_id
                        }
                        _ => continue,
                    };
                    assert!(
                        program.intents.contains_key(intent_id),
                        "{name}: intent {intent_id} missing from the index"
                    );
                }
            }
        }
        // Every on-table destination is `stay` or a leg.
        for (id, intent) in &program.intents {
            for dest in [
                &intent.on.fill,
                &intent.on.partial,
                &intent.on.reject,
                &intent.on.cancel_ack,
                &intent.on.expiry,
            ] {
                assert!(
                    dest == "stay" || program.legs.iter().any(|l| &l.name == dest),
                    "{name}.{id}: destination {dest} is not a leg"
                );
            }
        }
    }
}
