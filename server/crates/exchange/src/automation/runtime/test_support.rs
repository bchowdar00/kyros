//! Test helpers: compile the compiler crate's Tiny fixtures into programs.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use rust_decimal::Decimal;
use strategy_compiler::ir::{Envelope, Registry, Strategy};
use strategy_compiler::program::Program;

use crate::automation::sme::Activation;
use crate::automation::value::Value;

fn compiler_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../strategy-compiler")
}

pub(crate) fn tiny_registry() -> Registry {
    let text = std::fs::read_to_string(compiler_dir().join("registry/tiny.json")).expect("tiny registry reads");
    serde_json::from_str(&text).expect("tiny registry parses")
}

pub(crate) fn test_envelope() -> Envelope {
    serde_json::from_str(
        r#"{"allocation": "1000", "loss_budget": "500", "round_budget": "400", "max_order_size": "100",
            "max_position_per_market": "200", "max_open_orders": "8"}"#,
    )
    .expect("envelope parses")
}

/// Compile `tests/fixtures/tiny/<name>.json` against the venue registry.
pub(crate) fn compile_fixture(name: &str) -> Program {
    compile_fixture_with(name, &test_envelope())
}

/// Compile a Tiny fixture against a specific envelope (the values are baked
/// into the program, so a test of the risk gate picks its own budget).
pub(crate) fn compile_fixture_with(name: &str, envelope: &Envelope) -> Program {
    let path = compiler_dir().join(format!("tests/fixtures/tiny/{name}.json"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{name} fixture reads: {e}"));
    let strategy: Strategy = serde_json::from_str(&text).unwrap_or_else(|e| panic!("{name} parses: {e}"));
    let compiled = strategy_compiler::compile(&strategy, &tiny_registry(), envelope)
        .unwrap_or_else(|diags| panic!("{name} compiles: {diags:#?}"));
    compiled.artifacts.program
}

pub(crate) fn d(s: &str) -> Decimal {
    s.parse().expect("decimal literal")
}

/// Build an activation with the given fired triggers and per-signal decimal
/// fields.
pub(crate) fn activation(fired: &[&str], snapshots: &[(&str, &[(&str, &str)])], ts_ms: u64) -> Activation {
    let mut snaps: BTreeMap<String, BTreeMap<String, Value>> = BTreeMap::new();
    for (sid, fields) in snapshots {
        let m = snaps.entry(sid.to_string()).or_default();
        for (f, v) in *fields {
            m.insert(f.to_string(), Value::Decimal(d(v)));
        }
    }
    Activation {
        session: 0,
        fired: fired.iter().map(|s| s.to_string()).collect::<BTreeSet<_>>(),
        event: None,
        event_signal: None,
        snapshots: snaps,
        ts_ms,
    }
}

pub(crate) const BOOK: &str = "btc_usd.updown_1m.current.price.v1";
pub(crate) const FAIR: &str = "platform.btc_usd.updown_1m.current.fair.v1";
