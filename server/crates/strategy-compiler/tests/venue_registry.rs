//! The Tiny venue registry (`registry/tiny.json`): the contract the exchange
//! runtime publishes against and the compiler validates against. It must parse,
//! round-trip, and compile a strategy written for the real one-minute BTC market.

use std::fs;
use std::path::{Path, PathBuf};

use strategy_compiler::ir::{Registry, Strategy};

mod common;

const VERSION: &str = "0.1.0";

fn crate_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn venue_registry() -> Registry {
    let text = fs::read_to_string(crate_dir().join("registry/tiny.json"))
        .expect("venue registry reads");
    serde_json::from_str(&text).expect("venue registry parses")
}

fn fixture(name: &str) -> Strategy {
    let text = fs::read_to_string(crate_dir().join(format!("tests/fixtures/tiny/{name}.json")))
        .unwrap_or_else(|e| panic!("{name} fixture reads: {e}"));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{name} fixture parses: {e}"))
}

#[test]
fn venue_registry_round_trips() {
    let path = crate_dir().join("registry/tiny.json");
    let text = fs::read_to_string(&path).expect("venue registry reads");
    let typed: Registry = serde_json::from_str(&text).expect("venue registry parses");
    let reserialized = serde_json::to_value(&typed).expect("registry re-serializes");
    let original: serde_json::Value = serde_json::from_str(&text).expect("registry is JSON");
    assert_eq!(original, reserialized, "venue registry did not round-trip");
}

#[test]
fn venue_registry_declares_the_btc_market_family() {
    let registry = venue_registry();
    let series = registry
        .series
        .iter()
        .find(|s| s.series_id == "btc_usd.updown_1m")
        .expect("the one-minute up/down series");
    assert_eq!(series.outcomes.as_deref(), Some(&["up".to_string(), "down".to_string()][..]));
    for id in [
        "coinbase.btc_usd.spot.v1",
        "btc_usd.updown_1m.current.strike.v1",
        "btc_usd.updown_1m.current.price.v1",
        "btc_usd.updown_1m.current.window.v1",
        "btc_usd.updown_1m.current.trades.v1",
        "platform.btc_usd.window_delta.v1",
        "platform.btc_usd.updown_1m.current.fair.v1",
    ] {
        assert!(
            registry.signals.iter().any(|s| s.signal_id == id),
            "venue registry is missing {id}"
        );
    }
}

/// Every strategy written against the venue registry. Each compiles clean and its
/// program is snapshotted; a feature ships with at least one fixture here.
const TINY_FIXTURES: &[&str] = &[
    // The first acceptance strategy: resting limit, cancel-replace, window timer.
    "fair-discount",
    // Decimal and bool scalar state (v0.3 primitives).
    "entry-tracker",
    // `legs.*` fill-context reads (plain and tag-scoped) and the v0.3 platform reads.
    "leg-exit",
    // Deploy-time params (string, decimal, enum) in leaves and bodies, a string field
    // read through `event.*`, and a string state slot.
    "follow-whale",
    // `interval` and `after` timers and a rearm cooldown.
    "periodic-requoter",
    // A window slot with `push` and every reducer.
    "rolling-spread",
    // post_only, expire_after (GTD), reduce_only and a target_position rebalance.
    "post-only-maker",
    // Verification probe: every v0.3 feature together.
    "combined-everything",
    // Three holding modes, a reduce-only flatten plus an opposite-side take in one block.
    "momentum-flip",
    // A self-pacing add block: the guard reads the counter the block increments.
    "scale-in",
    // Two resting tags quoted and re-quoted together, pulled by the stop timer.
    "two-sided-quoter",
    // A flatten that returns to hunting, bounded by a per-round entry counter.
    "re-entry",
    // `if_true` + `immediate` paced only by a cooldown.
    "every-time-cooldown",
];

#[test]
fn tiny_fixtures_compile_against_the_venue() {
    for name in TINY_FIXTURES {
        let strategy = fixture(name);
        let output = strategy_compiler::emit::build(
            &strategy,
            &venue_registry(),
            &common::envelope(),
            VERSION,
        );
        assert!(
            !output.has_errors(),
            "{name} failed to build against the venue registry:\n{}",
            common::render(&output.diagnostics)
        );
        let artifacts = output.artifacts.expect("no errors yields artifacts");
        assert_eq!(
            artifacts.program_json,
            strategy_compiler::emit::serialize_program(&artifacts.program),
            "{name}: typed and serialized programs disagree"
        );
        let again: strategy_compiler::Program = serde_json::from_str(&artifacts.program_json)
            .unwrap_or_else(|e| panic!("{name}: program does not round-trip: {e}"));
        assert_eq!(again, artifacts.program, "{name}: program changed across a round trip");
        insta::assert_snapshot!(format!("program__tiny__{name}"), artifacts.program_json);
    }
}

#[test]
fn tiny_fixtures_round_trip_losslessly() {
    for name in TINY_FIXTURES {
        let text = fs::read_to_string(crate_dir().join(format!("tests/fixtures/tiny/{name}.json")))
            .unwrap_or_else(|e| panic!("{name} fixture reads: {e}"));
        let typed: Strategy = serde_json::from_str(&text).expect("fixture parses");
        let reserialized = serde_json::to_value(&typed).expect("re-serializes");
        let original: serde_json::Value = serde_json::from_str(&text).expect("fixture is JSON");
        assert_eq!(original, reserialized, "{name} did not round-trip through the IR types");
    }
}

/// The shape fixtures the authoring pipeline's derivations mirror. They are what the
/// writer learns from, so they must be lint-clean as well as error-free: a warning
/// here is a bad habit taught to every strategy.
const SHAPE_FIXTURES: &[&str] =
    &["momentum-flip", "scale-in", "two-sided-quoter", "re-entry", "every-time-cooldown"];

#[test]
fn shape_fixtures_compile_without_warnings() {
    for name in SHAPE_FIXTURES {
        let output = strategy_compiler::emit::build(
            &fixture(name),
            &venue_registry(),
            &common::envelope(),
            VERSION,
        );
        assert!(
            output.diagnostics.is_empty(),
            "{name} is not lint-clean:\n{}",
            common::render(&output.diagnostics)
        );
    }
}
