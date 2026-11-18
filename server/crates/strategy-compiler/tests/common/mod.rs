//! Shared harness for the validator's negative suite.
//!
//! The negative documents live here AS DATA, split by rule family: each family's test
//! file snapshots its own slice, and the coverage contract test
//! (`validate_code_coverage.rs`) runs the ENTIRE catalog to diff exercised codes
//! against the full roster. A new rule means a new case here — the contract test
//! fails until it exists.

// Each integration-test binary compiles this module independently and uses only its
// slice of it, so per-binary dead-code analysis would false-positive on the rest.
#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};

use strategy_compiler::validate::{self, Diagnostic};

/// One negative document: a name (the snapshot key) and the document text.
pub struct Case {
    /// Snapshot name, `neg_<code-ish>`.
    pub name: &'static str,
    /// The strategy document, shape-valid JSON (deserializes into `strategy_compiler::ir::Strategy`).
    pub json: &'static str,
}

fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus")
}

/// The eight corpus fixtures, the emit stage's real test suite.
pub const STRATEGIES: [&str; 9] = [
    "break-quoter",
    "capitulation-fade",
    "curve-discount",
    "endgame-sweep",
    "favorite-panic",
    "favorite-panic-empty-params",
    "late-momentum",
    "momentum-flip",
    "pinnacle-bid",
];

/// The raw text of one corpus fixture.
pub fn corpus_text(name: &str) -> String {
    fs::read_to_string(corpus_dir().join(format!("{name}.json")))
        .unwrap_or_else(|e| panic!("{name} fixture reads: {e}"))
}

/// Load and validate one corpus fixture, panicking on any error diagnostic — the
/// emit-stage tests all start from the typestate, and a fixture that stopped
/// validating is `validate_corpus.rs`'s failure to report, not theirs.
pub fn corpus_valid(name: &str) -> validate::ValidStrategy {
    let (diagnostics, valid) = run_full(&corpus_text(name), &envelope());
    let errors: Vec<String> = diagnostics
        .iter()
        .filter(|d| d.is_error())
        .map(|d| d.to_string())
        .collect();
    assert!(
        errors.is_empty(),
        "{name} has validation errors:\n{}",
        errors.join("\n")
    );
    valid.expect("zero errors produces the typestate")
}

/// The one registry every test validates against.
pub fn registry() -> strategy_compiler::ir::Registry {
    let text = fs::read_to_string(corpus_dir().join("registry/registry.json"))
        .expect("registry fixture reads");
    serde_json::from_str(&text).expect("registry fixture parses")
}

/// The test envelope: generous, all optional fields present, internally consistent.
pub fn envelope() -> strategy_compiler::ir::Envelope {
    serde_json::from_str(
        r#"{
  "allocation": "1000",
  "loss_budget": "500",
  "round_budget": "400",
  "max_order_size": "100",
  "max_position_per_market": "200",
  "max_open_orders": "8"
}"#,
    )
    .expect("test envelope parses")
}

/// Deserialize and validate one document, returning the diagnostics alone.
pub fn run(json: &str) -> Vec<Diagnostic> {
    run_with_envelope(json, &envelope())
}

/// Deserialize and validate one document against a specific envelope.
pub fn run_with_envelope(json: &str, envelope: &strategy_compiler::ir::Envelope) -> Vec<Diagnostic> {
    run_full(json, envelope).0
}

/// The full validator result, typestate included.
pub fn run_full(
    json: &str,
    envelope: &strategy_compiler::ir::Envelope,
) -> (Vec<Diagnostic>, Option<validate::ValidStrategy>) {
    let strategy: strategy_compiler::ir::Strategy = serde_json::from_str(json).expect("shape-valid document");
    validate::validate(&strategy, &registry(), envelope)
}

/// Render a diagnostic list the way the snapshots record it.
pub fn render(diagnostics: &[Diagnostic]) -> String {
    if diagnostics.is_empty() {
        return "(no diagnostics)".to_string();
    }
    diagnostics
        .iter()
        .map(|d| d.to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Render a family's cases for snapshotting. The `insta` assertion itself lives in
/// each test file (not here) so snapshot files land in `tests/snapshots/`, named by
/// the test binary — insta keys the location to the assertion site's module.
pub fn rendered_cases(cases: &[Case]) -> Vec<(&'static str, String)> {
    cases
        .iter()
        .map(|case| (case.name, render(&run(case.json))))
        .collect()
}

/// A minimal valid document the negative cases mutate. Kept valid on purpose: every
/// case's diagnostics should come from its one deliberate breakage.
pub const BASE: &str = r#"{
  "meta": { "name": "Base", "description": "negative-suite base", "dsl_version": "0.3" },
  "instruments": { "game": "nba.sim.h2h.current" },
  "signals": { "pbp": "nba.sim.h2h.current.pbp.v1", "price": "nba.sim.h2h.current.price.v1" },
  "state": [ { "name": "tally", "kind": "scalar", "value_type": "u8" } ],
  "triggers": [
    { "name": "tick", "condition": { "signal": "price" },
      "fire": "if_true", "rearm": { "mode": "immediate" } }
  ],
  "legs": [
    { "name": "main", "resident": true, "arm": ["tick"],
      "body": [
        { "name": "note", "if": "triggered(tick)",
          "do": ["set state.tally = state.tally + 1"] }
      ] }
  ]
}"#;

/// Every negative case, for the coverage contract sweep.
pub fn all_cases() -> Vec<Case> {
    let mut cases = resolution_cases();
    cases.extend(registry_cases());
    cases.extend(context_cases());
    cases.extend(structural_cases());
    cases.extend(orders_cases());
    cases.extend(lints_cases());
    cases
}

/// The cycle document (design cascade case 2): `a -> b -> a`, with `c` a poisoned
/// dependent feeding a size. Shared by the scenario test and the coverage sweep.
pub const CYCLE_DOC: &str = r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": { "game": "nba.sim.h2h.current" },
  "signals": { "price": "nba.sim.h2h.current.price.v1" },
  "state": [],
  "expressions": { "named": {
    "a": "expr.b + 1",
    "b": "expr.a + 1",
    "c": "expr.b * 2"
  } },
  "triggers": [
    { "name": "tick", "condition": { "signal": "price" },
      "fire": "if_true", "rearm": { "mode": "immediate" } }
  ],
  "legs": [
    { "name": "main", "resident": true, "arm": ["tick"],
      "body": [
        { "name": "buy", "if": "triggered(tick)",
          "do": [
            { "place": {
                "instrument": "game", "outcome": "home", "side": "buy",
                "order_type": "market", "tif": "ioc",
                "size": "expr.c",
                "max_size": "envelope.max_order_size", "tag": "open" } }
          ],
          "then": "main" }
      ] }
  ]
}"#;

/// A document reading `envelope.max_position_per_market` (pair with
/// [`sparse_envelope`] for the missing-read-field case).
pub const MISSING_READ_DOC: &str = r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": { "game": "nba.sim.h2h.current" },
  "signals": { "price": "nba.sim.h2h.current.price.v1" },
  "state": [],
  "triggers": [
    { "name": "tick", "condition": { "signal": "price" },
      "fire": "if_true", "rearm": { "mode": "immediate" } }
  ],
  "legs": [
    { "name": "main", "resident": true, "arm": ["tick"],
      "body": [
        { "name": "buy", "if": "triggered(tick) and platform.position.size < envelope.max_position_per_market",
          "do": [
            { "place": {
                "instrument": "game", "outcome": "home", "side": "buy",
                "order_type": "market", "tif": "ioc",
                "size": "clamp(envelope.max_order_size, 0, envelope.max_order_size)",
                "max_size": "envelope.max_order_size", "tag": "open" } }
          ],
          "then": "main" }
      ] }
  ]
}"#;

/// The survival-core-only envelope (no optional fields).
pub fn sparse_envelope() -> strategy_compiler::ir::Envelope {
    serde_json::from_str(
        r#"{ "allocation": "1000", "loss_budget": "500", "max_order_size": "100" }"#,
    )
    .expect("test envelope parses")
}

/// An internally inconsistent envelope (fraction above one, loss and round budgets
/// above allocation).
pub fn bad_envelope() -> strategy_compiler::ir::Envelope {
    serde_json::from_str(
        r#"{
  "allocation": "1000",
  "loss_budget": "2000",
  "round_budget": "1500",
  "max_order_size": "100",
  "max_exposure_fraction": "1.5"
}"#,
    )
    .expect("test envelope parses")
}

/// Family F: the idiom lints — each firing case paired with a near-miss that must
/// stay clean (the lint's boundary is part of the contract).
pub fn lints_cases() -> Vec<Case> {
    // The missed-wake pair: an edge-fired entry trigger whose guard reads a counter
    // that a different trigger's block maintains. `fire` is the only difference
    // between the firing case and the near-miss.
    const MISSED_WAKE: &str = r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": { "game": "nba.sim.h2h.current" },
  "signals": { "pbp": "nba.sim.h2h.current.pbp.v1", "price": "nba.sim.h2h.current.price.v1" },
  "state": [ { "name": "n", "kind": "scalar", "value_type": "u8" } ],
  "triggers": [
    { "name": "made_shot",
      "condition": { "all": [
        { "signal": "pbp", "field": "action", "comparator": "==", "value": "three_pointer" },
        { "signal": "pbp", "field": "result", "comparator": "==", "value": "made" }
      ] },
      "fire": "if_true", "rearm": { "mode": "immediate" } },
    { "name": "cheap",
      "condition": { "signal": "price", "field": "home", "comparator": "<", "value": "0.4" },
      "fire": "FIRE_MODE", "rearm": { "mode": "REARM_MODE" } }
  ],
  "legs": [
    { "name": "hunting", "resident": true, "arm": ["made_shot", "cheap"],
      "body": [
        { "name": "tally_up", "if": "triggered(made_shot)",
          "do": ["set state.n = state.n + 1"] },
        { "name": "buy", "if": "triggered(cheap) and state.n >= 5",
          "do": [
            { "place": {
                "instrument": "game", "outcome": "home", "side": "buy",
                "order_type": "market", "tif": "ioc",
                "size": "clamp(envelope.max_order_size, 0, envelope.max_order_size)",
                "max_size": "envelope.max_order_size", "tag": "open" } }
          ],
          "then": "done" }
      ] },
    { "name": "done", "resident": true, "arm": [], "body": [] }
  ]
}"#;

    // The coverage pair: the guard's threshold vs the trigger's. 0.35 is TIGHTER
    // than the trigger's 0.4 (drift: warn); 0.45 is looser (implied: clean).
    const COVERAGE: &str = r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": { "game": "nba.sim.h2h.current" },
  "signals": { "price": "nba.sim.h2h.current.price.v1" },
  "state": [],
  "triggers": [
    { "name": "cheap",
      "condition": { "signal": "price", "field": "home", "comparator": "<", "value": "0.4" },
      "fire": "if_true", "rearm": { "mode": "immediate" } }
  ],
  "legs": [
    { "name": "main", "resident": true, "arm": ["cheap"],
      "body": [
        { "name": "buy", "if": "triggered(cheap) and signal.price.home < THRESHOLD",
          "do": [
            { "place": {
                "instrument": "game", "outcome": "home", "side": "buy",
                "order_type": "market", "tif": "ioc",
                "size": "clamp(envelope.max_order_size, 0, envelope.max_order_size)",
                "max_size": "envelope.max_order_size", "tag": "open" } }
          ],
          "then": "done" }
      ] },
    { "name": "done", "resident": true, "arm": [], "body": [] }
  ]
}"#;

    const REQUOTE_RATE: &str = r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": { "game": "nba.sim.h2h.current" },
  "signals": { "ticks": "nba.sim.h2h.current.book_ticks.v1" },
  "state": [],
  "triggers": [
    { "name": "tick", "condition": { "signal": "ticks" },
      "fire": "if_true", "rearm": REARM }
  ],
  "legs": [
    { "name": "quoting", "resident": true, "arm": ["tick"],
      "body": [
        { "name": "requote", "if": "triggered(tick)",
          "do": [
            { "cancel_replace": {
                "tag": "quote", "instrument": "game", "outcome": "home", "side": "buy",
                "order_type": "limit", "tif": "gtc",
                "price": "clamp(signal.ticks.bid, 0.01, 0.99)", "price_band": ["0.01", "0.99"],
                "size": "clamp(envelope.max_order_size, 0, envelope.max_order_size)",
                "max_size": "envelope.max_order_size" } }
          ] }
      ] }
  ]
}"#;

    vec![
        Case {
            name: "neg_lint_missed_wake",
            json: leak(
                MISSED_WAKE
                    .replace("FIRE_MODE", "on_crossing")
                    .replace("REARM_MODE", "fresh_crossing"),
            ),
        },
        Case {
            name: "neg_lint_missed_wake_near_miss",
            json: leak(
                MISSED_WAKE
                    .replace("FIRE_MODE", "if_true")
                    .replace("REARM_MODE", "immediate"),
            ),
        },
        // The requote-rate pair: a cancel_replace on every wake of an unpaced
        // `if_true` trigger over the corpus's one sub-second signal. The
        // cooldown is the only difference between the firing case and the
        // near-miss.
        Case {
            name: "neg_lint_requote_rate",
            json: leak(REQUOTE_RATE.replace("REARM", r#"{ "mode": "immediate" }"#)),
        },
        Case {
            name: "neg_lint_requote_rate_near_miss",
            json: leak(REQUOTE_RATE.replace("REARM", r#"{ "mode": "immediate", "cooldown": "250ms" }"#)),
        },
        Case {
            name: "neg_lint_machine_gun",
            json: r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": { "game": "nba.sim.h2h.current" },
  "signals": { "price": "nba.sim.h2h.current.price.v1" },
  "state": [],
  "triggers": [
    { "name": "tick", "condition": { "signal": "price" },
      "fire": "if_true", "rearm": { "mode": "immediate" } }
  ],
  "legs": [
    { "name": "main", "resident": true, "arm": ["tick"],
      "body": [
        { "name": "spray",
          "do": [
            { "place": {
                "instrument": "game", "outcome": "home", "side": "buy",
                "order_type": "market", "tif": "ioc",
                "size": "clamp(envelope.max_order_size, 0, envelope.max_order_size)",
                "max_size": "envelope.max_order_size", "tag": "open",
                "on": { "fill": "done" } } }
          ] }
      ] },
    { "name": "done", "resident": true, "arm": [], "body": [] }
  ]
}"#,
        },
        Case {
            name: "neg_lint_machine_gun_near_miss",
            json: r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": { "game": "nba.sim.h2h.current" },
  "signals": { "price": "nba.sim.h2h.current.price.v1" },
  "state": [],
  "triggers": [
    { "name": "tick", "condition": { "signal": "price" },
      "fire": "if_true", "rearm": { "mode": "immediate" } }
  ],
  "legs": [
    { "name": "main", "resident": true, "arm": ["tick"],
      "body": [
        { "name": "once",
          "do": [
            { "place": {
                "instrument": "game", "outcome": "home", "side": "buy",
                "order_type": "market", "tif": "ioc",
                "size": "clamp(envelope.max_order_size, 0, envelope.max_order_size)",
                "max_size": "envelope.max_order_size", "tag": "open" } }
          ],
          "then": "done" }
      ] },
    { "name": "done", "resident": true, "arm": [], "body": [] }
  ]
}"#,
        },
        Case {
            name: "neg_lint_coverage",
            json: leak(COVERAGE.replace("THRESHOLD", "0.35")),
        },
        Case {
            name: "neg_lint_coverage_near_miss",
            json: leak(COVERAGE.replace("THRESHOLD", "0.45")),
        },
        Case {
            name: "neg_triggered_not_armed",
            json: r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": { "game": "nba.sim.h2h.current" },
  "signals": { "price": "nba.sim.h2h.current.price.v1" },
  "state": [],
  "triggers": [
    { "name": "tick", "condition": { "signal": "price" },
      "fire": "if_true", "rearm": { "mode": "immediate" } },
    { "name": "other", "condition": { "signal": "price", "field": "home", "comparator": ">", "value": "0.7" },
      "fire": "on_crossing", "rearm": { "mode": "fresh_crossing" } }
  ],
  "legs": [
    { "name": "main", "resident": true, "arm": ["tick"],
      "body": [ { "name": "dead", "if": "triggered(other)", "do": [] } ] },
    { "name": "second", "resident": true, "arm": ["other"], "body": [] }
  ]
}"#,
        },
        Case {
            name: "neg_trigger_unarmed",
            json: r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": { "game": "nba.sim.h2h.current" },
  "signals": { "price": "nba.sim.h2h.current.price.v1" },
  "state": [],
  "triggers": [
    { "name": "tick", "condition": { "signal": "price" },
      "fire": "if_true", "rearm": { "mode": "immediate" } },
    { "name": "orphan", "condition": { "signal": "price", "field": "home", "comparator": ">", "value": "0.7" },
      "fire": "on_crossing", "rearm": { "mode": "fresh_crossing" } }
  ],
  "legs": [ { "name": "main", "resident": true, "arm": ["tick"], "body": [] } ]
}"#,
        },
    ]
}

/// Leak a built JSON string to `'static` — negative cases are process-lifetime test
/// data, and `Case` deliberately stores `&'static str` so the catalog is plain data.
fn leak(json: String) -> &'static str {
    Box::leak(json.into_boxed_str())
}

/// Family E: substitution and order-expression rules.
pub fn orders_cases() -> Vec<Case> {
    vec![
        Case {
            name: "neg_not_clamp_wrapped",
            json: r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": { "game": "nba.sim.h2h.current" },
  "signals": { "price": "nba.sim.h2h.current.price.v1" },
  "state": [],
  "triggers": [
    { "name": "tick", "condition": { "signal": "price" },
      "fire": "if_true", "rearm": { "mode": "immediate" } }
  ],
  "legs": [
    { "name": "main", "resident": true, "arm": ["tick"],
      "body": [
        { "name": "buy", "if": "triggered(tick)",
          "do": [
            { "place": {
                "instrument": "game", "outcome": "home", "side": "buy",
                "order_type": "market", "tif": "ioc",
                "size": "envelope.max_order_size * 0.5",
                "max_size": "envelope.max_order_size", "tag": "open" } }
          ],
          "then": "main" }
      ] }
  ]
}"#,
        },
        Case {
            name: "neg_band_unverifiable",
            json: r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": { "game": "nba.sim.h2h.current" },
  "signals": { "price": "nba.sim.h2h.current.price.v1" },
  "state": [],
  "triggers": [
    { "name": "tick", "condition": { "signal": "price" },
      "fire": "if_true", "rearm": { "mode": "immediate" } }
  ],
  "legs": [
    { "name": "main", "resident": true, "arm": ["tick"],
      "body": [
        { "name": "quote", "if": "triggered(tick)",
          "do": [
            { "place": {
                "instrument": "game", "outcome": "home", "side": "buy",
                "order_type": "limit", "tif": "gtc",
                "price": "clamp(signal.price.home, envelope.allocation, 0.9)",
                "price_band": ["0.1", "0.9"],
                "size": "clamp(envelope.max_order_size, 0, envelope.max_order_size)",
                "max_size": "envelope.max_order_size", "tag": "open" } }
          ],
          "then": "main" }
      ] }
  ]
}"#,
        },
        Case {
            name: "neg_band_violation",
            json: r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": { "game": "nba.sim.h2h.current" },
  "signals": { "price": "nba.sim.h2h.current.price.v1" },
  "state": [],
  "triggers": [
    { "name": "tick", "condition": { "signal": "price" },
      "fire": "if_true", "rearm": { "mode": "immediate" } }
  ],
  "legs": [
    { "name": "main", "resident": true, "arm": ["tick"],
      "body": [
        { "name": "quote", "if": "triggered(tick)",
          "do": [
            { "place": {
                "instrument": "game", "outcome": "home", "side": "buy",
                "order_type": "limit", "tif": "gtc",
                "price": "clamp(signal.price.home, 0.05, 0.95)",
                "price_band": ["0.1", "0.9"],
                "size": "clamp(envelope.max_order_size, 0, envelope.max_order_size)",
                "max_size": "envelope.max_order_size", "tag": "open" } }
          ],
          "then": "main" }
      ] }
  ]
}"#,
        },
        Case {
            name: "neg_param_as_size",
            json: r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": { "game": "nba.sim.h2h.current" },
  "signals": { "price": "nba.sim.h2h.current.price.v1" },
  "state": [],
  "params": [ { "name": "qty", "type": "decimal", "default": "10" } ],
  "triggers": [
    { "name": "tick", "condition": { "signal": "price" },
      "fire": "if_true", "rearm": { "mode": "immediate" } }
  ],
  "legs": [
    { "name": "main", "resident": true, "arm": ["tick"],
      "body": [
        { "name": "buy", "if": "triggered(tick)",
          "do": [
            { "place": {
                "instrument": "game", "outcome": "home", "side": "buy",
                "order_type": "market", "tif": "ioc",
                "size": "clamp(param.qty, 0, param.qty)",
                "max_size": "param.qty", "tag": "open" } }
          ],
          "then": "main" }
      ] }
  ]
}"#,
        },
        Case {
            name: "neg_size_without_envelope",
            json: r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": { "game": "nba.sim.h2h.current" },
  "signals": { "price": "nba.sim.h2h.current.price.v1" },
  "state": [],
  "triggers": [
    { "name": "tick", "condition": { "signal": "price" },
      "fire": "if_true", "rearm": { "mode": "immediate" } }
  ],
  "legs": [
    { "name": "main", "resident": true, "arm": ["tick"],
      "body": [
        { "name": "buy", "if": "triggered(tick)",
          "do": [
            { "place": {
                "instrument": "game", "outcome": "home", "side": "buy",
                "order_type": "market", "tif": "ioc",
                "size": "clamp(50, 0, 100)",
                "max_size": "100", "tag": "open" } }
          ],
          "then": "main" }
      ] }
  ]
}"#,
        },
        Case {
            // The bound reads the envelope, so the first rule passes; the VALUE is a
            // literal five dollars that would not grow with the user's money.
            name: "neg_size_literal_dollars",
            json: r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": { "game": "nba.sim.h2h.current" },
  "signals": { "price": "nba.sim.h2h.current.price.v1" },
  "state": [],
  "triggers": [
    { "name": "tick", "condition": { "signal": "price" },
      "fire": "if_true", "rearm": { "mode": "immediate" } }
  ],
  "legs": [
    { "name": "main", "resident": true, "arm": ["tick"],
      "body": [
        { "name": "buy", "if": "triggered(tick)",
          "do": [
            { "place": {
                "instrument": "game", "outcome": "home", "side": "buy",
                "order_type": "market", "tif": "ioc",
                "size": "clamp(floor(5 / signal.price.home), 0, envelope.max_order_size)",
                "max_size": "envelope.max_order_size", "tag": "open" } }
          ],
          "then": "main" }
      ] }
  ]
}"#,
        },
    ]
}

/// Family D: structural rules.
pub fn structural_cases() -> Vec<Case> {
    vec![
        Case {
            name: "neg_group_too_small",
            json: r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": { "game": "nba.sim.h2h.current" },
  "signals": { "price": "nba.sim.h2h.current.price.v1" },
  "state": [],
  "triggers": [
    { "name": "tick",
      "condition": { "all": [ { "signal": "price", "field": "home", "comparator": "<", "value": "0.4" } ] },
      "fire": "if_true", "rearm": { "mode": "immediate" } }
  ],
  "legs": [ { "name": "main", "resident": true, "arm": ["tick"], "body": [] } ]
}"#,
        },
        Case {
            name: "neg_table_shape",
            json: r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": { "game": "nba.sim.h2h.current" },
  "signals": { "price": "nba.sim.h2h.current.price.v1" },
  "state": [],
  "expressions": { "tables": {
    "short": { "keys": ["0"], "values": ["0.1", "0.2"], "interpolation": "linear" },
    "unsorted": { "keys": ["1", "1", "0"], "values": ["0.1", "0.2", "0.3"], "interpolation": "linear" }
  } },
  "triggers": [
    { "name": "tick", "condition": { "signal": "price" },
      "fire": "if_true", "rearm": { "mode": "immediate" } }
  ],
  "legs": [
    { "name": "main", "resident": true, "arm": ["tick"],
      "body": [
        { "name": "note", "if": "lookup(table.short, 1) > lookup(table.unsorted, 1)", "do": [] }
      ] }
  ]
}"#,
        },
        Case {
            name: "neg_window_shape",
            json: r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": { "game": "nba.sim.h2h.current" },
  "signals": { "price": "nba.sim.h2h.current.price.v1" },
  "state": [
    { "name": "a", "kind": "window", "value_type": "decimal", "capacity": 0 },
    { "name": "b", "kind": "window", "value_type": "u8", "capacity": 5000 },
    { "name": "c", "kind": "window", "value_type": { "type": "enum", "variants": ["home", "away"] }, "capacity": 4 }
  ],
  "triggers": [
    { "name": "tick", "condition": { "signal": "price" },
      "fire": "if_true", "rearm": { "mode": "immediate" } }
  ],
  "legs": [ { "name": "main", "resident": true, "arm": ["tick"], "body": [] } ]
}"#,
        },
        Case {
            name: "neg_enum_shape",
            json: r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": { "game": "nba.sim.h2h.current" },
  "signals": { "price": "nba.sim.h2h.current.price.v1" },
  "state": [
    { "name": "m", "kind": "map",
      "key_types": [ { "type": "enum", "variants": ["home", "home"] } ],
      "value_type": { "type": "enum", "variants": ["only"] },
      "max_keys": 0, "overflow": "refuse_update" }
  ],
  "triggers": [
    { "name": "tick", "condition": { "signal": "price" },
      "fire": "if_true", "rearm": { "mode": "immediate" } }
  ],
  "legs": [ { "name": "main", "resident": true, "arm": ["tick"], "body": [] } ]
}"#,
        },
        Case {
            name: "neg_param_default_mismatch",
            json: r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": { "game": "nba.sim.h2h.current" },
  "signals": { "price": "nba.sim.h2h.current.price.v1" },
  "state": [],
  "params": [
    { "name": "a", "type": "decimal", "default": "abc" },
    { "name": "b", "type": "decimal", "default": "0.40" },
    { "name": "c", "type": "u8", "default": "300" },
    { "name": "d", "type": "u8", "default": "1.5" },
    { "name": "e", "type": "bool", "default": "yes" },
    { "name": "f", "type": { "type": "enum", "variants": ["home", "away"] }, "default": "blue" },
    { "name": "g", "type": "string", "default": true },
    { "name": "h", "type": "decimal", "default": "-0.5" },
    { "name": "i", "type": "u8", "default": "255" }
  ],
  "triggers": [
    { "name": "tick", "condition": { "signal": "price" },
      "fire": "if_true", "rearm": { "mode": "immediate" } }
  ],
  "legs": [ { "name": "main", "resident": true, "arm": ["tick"], "body": [] } ]
}"#,
        },
        Case {
            name: "neg_empty_section",
            json: r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": {},
  "signals": { "price": "nba.sim.h2h.current.price.v1" },
  "state": [],
  "expressions": { "named": {} },
  "triggers": [],
  "legs": [ { "name": "main", "resident": true, "arm": [], "body": [] } ]
}"#,
        },
        Case {
            name: "neg_limit_requires_price",
            json: r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": { "game": "nba.sim.h2h.current" },
  "signals": { "price": "nba.sim.h2h.current.price.v1" },
  "state": [],
  "triggers": [
    { "name": "tick", "condition": { "signal": "price" },
      "fire": "if_true", "rearm": { "mode": "immediate" } }
  ],
  "legs": [
    { "name": "main", "resident": true, "arm": ["tick"],
      "body": [
        { "name": "buy", "if": "triggered(tick)",
          "do": [
            { "place": {
                "instrument": "game", "outcome": "home", "side": "buy",
                "order_type": "limit", "tif": "gtc",
                "size": "clamp(envelope.max_order_size, 0, envelope.max_order_size)",
                "max_size": "envelope.max_order_size", "tag": "open" } }
          ],
          "then": "main" }
      ] }
  ]
}"#,
        },
        Case {
            name: "neg_market_forbids_price",
            json: r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": { "game": "nba.sim.h2h.current" },
  "signals": { "price": "nba.sim.h2h.current.price.v1" },
  "state": [],
  "triggers": [
    { "name": "tick", "condition": { "signal": "price" },
      "fire": "if_true", "rearm": { "mode": "immediate" } }
  ],
  "legs": [
    { "name": "main", "resident": true, "arm": ["tick"],
      "body": [
        { "name": "buy", "if": "triggered(tick)",
          "do": [
            { "place": {
                "instrument": "game", "outcome": "home", "side": "buy",
                "order_type": "market", "tif": "ioc",
                "price": "clamp(0.5, 0.4, 0.6)", "price_band": ["0.4", "0.6"],
                "size": "clamp(envelope.max_order_size, 0, envelope.max_order_size)",
                "max_size": "envelope.max_order_size", "tag": "open" } }
          ],
          "then": "main" }
      ] }
  ]
}"#,
        },
        Case {
            name: "neg_cancel_replace_not_limit",
            json: r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": { "game": "nba.sim.h2h.current" },
  "signals": { "price": "nba.sim.h2h.current.price.v1" },
  "state": [],
  "triggers": [
    { "name": "tick", "condition": { "signal": "price" },
      "fire": "if_true", "rearm": { "mode": "immediate" } }
  ],
  "legs": [
    { "name": "main", "resident": true, "arm": ["tick"],
      "body": [
        { "name": "requote", "if": "triggered(tick)",
          "do": [
            { "cancel_replace": {
                "tag": "quote", "instrument": "game", "outcome": "home", "side": "buy",
                "order_type": "market", "tif": "gtc",
                "price": "clamp(0.5, 0.4, 0.6)", "price_band": ["0.4", "0.6"],
                "size": "clamp(envelope.max_order_size, 0, envelope.max_order_size)",
                "max_size": "envelope.max_order_size" } }
          ],
          "then": "main" }
      ] }
  ]
}"#,
        },
        Case {
            name: "neg_order_flags",
            json: r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": { "game": "nba.sim.h2h.current", "btc": "btc_usd.updown_2m.current" },
  "signals": { "price": "nba.sim.h2h.current.price.v1" },
  "state": [],
  "triggers": [
    { "name": "tick", "condition": { "signal": "price" },
      "fire": "if_true", "rearm": { "mode": "immediate" } }
  ],
  "legs": [
    { "name": "main", "resident": true, "arm": ["tick"],
      "body": [
        { "name": "a", "if": "triggered(tick)",
          "do": [
            { "place": {
                "instrument": "game", "outcome": "home", "side": "buy",
                "order_type": "market", "tif": "ioc", "post_only": true, "expire_after": "5s",
                "size": "clamp(envelope.max_order_size, 0, envelope.max_order_size)",
                "max_size": "envelope.max_order_size", "tag": "open" } },
            { "place": {
                "instrument": "game", "outcome": "home", "side": "buy",
                "order_type": "limit", "tif": "gtc", "expire_after": "5s", "post_only": true, "reduce_only": true,
                "price": "clamp(0.5, 0.4, 0.6)", "price_band": ["0.4", "0.6"],
                "size": "clamp(envelope.max_order_size, 0, envelope.max_order_size)",
                "max_size": "envelope.max_order_size", "tag": "rest" } },
            { "place": {
                "instrument": "btc", "outcome": "up", "side": "buy",
                "order_type": "limit", "tif": "gtc", "post_only": true, "reduce_only": true,
                "price": "clamp(0.5, 0.4, 0.6)", "price_band": ["0.4", "0.6"],
                "size": "clamp(envelope.max_order_size, 0, envelope.max_order_size)",
                "max_size": "envelope.max_order_size", "tag": "rest2" } },
            { "target_position": {
                "instrument": "btc", "outcome": "up", "reduce_only": true,
                "size": "clamp(0 - envelope.max_order_size, 0 - envelope.max_order_size, envelope.max_order_size)",
                "max_size": "envelope.max_order_size", "tag": "flat" } }
          ],
          "then": "done" }
      ] },
    { "name": "done", "resident": true, "arm": [], "body": [] }
  ]
}"#,
        },
        Case {
            name: "neg_band_inverted",
            json: r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": { "game": "nba.sim.h2h.current" },
  "signals": { "price": "nba.sim.h2h.current.price.v1" },
  "state": [],
  "triggers": [
    { "name": "tick", "condition": { "signal": "price" },
      "fire": "if_true", "rearm": { "mode": "immediate" } }
  ],
  "legs": [
    { "name": "main", "resident": true, "arm": ["tick"],
      "body": [
        { "name": "buy", "if": "triggered(tick)",
          "do": [
            { "place": {
                "instrument": "game", "outcome": "home", "side": "buy",
                "order_type": "limit", "tif": "gtc",
                "price": "clamp(0.5, 0.4, 0.6)", "price_band": ["0.6", "0.4"],
                "size": "clamp(envelope.max_order_size, 0, envelope.max_order_size)",
                "max_size": "envelope.max_order_size", "tag": "open" } }
          ],
          "then": "main" }
      ] }
  ]
}"#,
        },
        Case {
            name: "neg_empty_on_table",
            json: r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": { "game": "nba.sim.h2h.current" },
  "signals": { "price": "nba.sim.h2h.current.price.v1" },
  "state": [],
  "triggers": [
    { "name": "tick", "condition": { "signal": "price" },
      "fire": "if_true", "rearm": { "mode": "immediate" } }
  ],
  "legs": [
    { "name": "main", "resident": true, "arm": ["tick"],
      "body": [
        { "name": "buy", "if": "triggered(tick)",
          "do": [
            { "place": {
                "instrument": "game", "outcome": "home", "side": "buy",
                "order_type": "market", "tif": "ioc",
                "size": "clamp(envelope.max_order_size, 0, envelope.max_order_size)",
                "max_size": "envelope.max_order_size", "tag": "open",
                "on": {} } }
          ],
          "then": "main" }
      ] }
  ]
}"#,
        },
        Case {
            name: "neg_explicit_stay",
            json: r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": { "game": "nba.sim.h2h.current" },
  "signals": { "price": "nba.sim.h2h.current.price.v1" },
  "state": [],
  "triggers": [
    { "name": "tick", "condition": { "signal": "price" },
      "fire": "if_true", "rearm": { "mode": "immediate" } }
  ],
  "legs": [
    { "name": "main", "resident": true, "arm": ["tick"],
      "body": [
        { "name": "buy", "if": "triggered(tick)",
          "do": [
            { "place": {
                "instrument": "game", "outcome": "home", "side": "buy",
                "order_type": "market", "tif": "ioc",
                "size": "clamp(envelope.max_order_size, 0, envelope.max_order_size)",
                "max_size": "envelope.max_order_size", "tag": "open",
                "on": { "reject": "stay" } } }
          ],
          "then": "main" }
      ] }
  ]
}"#,
        },
        Case {
            name: "neg_interval_rules",
            json: r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": { "game": "nba.sim.h2h.current" },
  "signals": { "price": "nba.sim.h2h.current.price.v1" },
  "state": [],
  "triggers": [
    { "name": "spin", "timer": { "kind": "interval", "every": "10ms" },
      "fire": "on_crossing", "rearm": { "mode": "immediate" } },
    { "name": "once", "timer": { "kind": "interval", "every": "2s" },
      "fire": "on_crossing", "rearm": { "mode": "never" } },
    { "name": "fine", "timer": { "kind": "interval", "every": "250ms" },
      "fire": "on_crossing", "rearm": { "mode": "immediate", "cooldown": "1s" } }
  ],
  "legs": [ { "name": "main", "resident": true, "arm": ["spin", "once", "fine"], "body": [] } ]
}"#,
        },
        Case {
            name: "neg_non_resident_no_exit",
            json: r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": { "game": "nba.sim.h2h.current" },
  "signals": { "price": "nba.sim.h2h.current.price.v1" },
  "state": [],
  "triggers": [
    { "name": "tick", "condition": { "signal": "price" },
      "fire": "if_true", "rearm": { "mode": "immediate" } }
  ],
  "legs": [
    { "name": "probing", "resident": false, "arm": ["tick"],
      "body": [ { "name": "note", "if": "triggered(tick)", "do": [] } ] }
  ]
}"#,
        },
    ]
}

/// Families C and G: context restrictions and typing-lite.
pub fn context_cases() -> Vec<Case> {
    vec![
        Case {
            name: "neg_triggered_outside_guard",
            json: r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": { "game": "nba.sim.h2h.current" },
  "signals": { "price": "nba.sim.h2h.current.price.v1" },
  "state": [],
  "triggers": [
    { "name": "tick", "condition": { "signal": "price" },
      "fire": "if_true", "rearm": { "mode": "immediate" } }
  ],
  "legs": [
    { "name": "main", "resident": true, "arm": ["tick"],
      "body": [
        { "name": "buy", "if": "triggered(tick)",
          "do": [
            { "place": {
                "instrument": "game", "outcome": "home", "side": "buy",
                "order_type": "market", "tif": "ioc",
                "size": "clamp(if triggered(tick) then envelope.max_order_size else 0, 0, envelope.max_order_size)",
                "max_size": "envelope.max_order_size", "tag": "open" } }
          ],
          "then": "main" }
      ] }
  ]
}"#,
        },
        Case {
            name: "neg_event_unpinned",
            json: r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": { "game": "nba.sim.h2h.current" },
  "signals": { "pbp": "nba.sim.h2h.current.pbp.v1" },
  "state": [ { "name": "q", "kind": "scalar", "value_type": "u8" } ],
  "triggers": [
    { "name": "play", "condition": { "signal": "pbp" },
      "fire": "if_true", "rearm": { "mode": "immediate" } }
  ],
  "legs": [
    { "name": "main", "resident": true, "arm": ["play"],
      "body": [ { "name": "note", "do": ["set state.q = event.quarter"] } ] }
  ]
}"#,
        },
        Case {
            name: "neg_event_in_named_expr",
            json: r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": { "game": "nba.sim.h2h.current" },
  "signals": { "pbp": "nba.sim.h2h.current.pbp.v1" },
  "state": [],
  "expressions": { "named": { "next_q": "event.quarter + 1" } },
  "triggers": [
    { "name": "play", "condition": { "signal": "pbp" },
      "fire": "if_true", "rearm": { "mode": "immediate" } }
  ],
  "legs": [ { "name": "main", "resident": true, "arm": ["play"], "body": [] } ]
}"#,
        },
        Case {
            name: "neg_table_outside_lookup",
            json: r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": { "game": "nba.sim.h2h.current" },
  "signals": { "price": "nba.sim.h2h.current.price.v1" },
  "state": [],
  "expressions": { "tables": { "curve": { "keys": ["0", "1"], "values": ["0.1", "0.2"], "interpolation": "linear" } } },
  "triggers": [
    { "name": "tick", "condition": { "signal": "price" },
      "fire": "if_true", "rearm": { "mode": "immediate" } }
  ],
  "legs": [
    { "name": "main", "resident": true, "arm": ["tick"],
      "body": [ { "name": "note", "if": "lookup(table.curve, signal.price.home) > table.curve", "do": [] } ] }
  ]
}"#,
        },
        Case {
            name: "neg_trigger_expr_impure",
            json: r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": { "game": "nba.sim.h2h.current" },
  "signals": { "price": "nba.sim.h2h.current.price.v1", "pbp": "nba.sim.h2h.current.pbp.v1" },
  "state": [ { "name": "n", "kind": "scalar", "value_type": "u8" } ],
  "params": [ { "name": "edge", "type": "decimal", "default": "0.04" } ],
  "expressions": { "named": { "mid": "(signal.price.home + signal.price.away) / 2" } },
  "triggers": [
    { "name": "state_read", "condition": { "expr": "signal.price.home - state.n", "comparator": ">", "value": "0" },
      "fire": "if_true", "rearm": { "mode": "immediate" } },
    { "name": "compared", "condition": { "expr": "signal.price.home > 0.5", "comparator": "==", "value": "1" },
      "fire": "if_true", "rearm": { "mode": "immediate" } },
    { "name": "cause", "condition": { "expr": "if triggered(compared) then signal.price.home else 0", "comparator": ">", "value": "0" },
      "fire": "if_true", "rearm": { "mode": "immediate" } },
    { "name": "named", "condition": { "expr": "expr.mid - platform.position.size", "comparator": ">", "value": "0" },
      "fire": "if_true", "rearm": { "mode": "immediate" } },
    { "name": "no_signal", "condition": { "expr": "param.edge * 2", "comparator": ">", "value": "0" },
      "fire": "if_true", "rearm": { "mode": "immediate" } },
    { "name": "fine", "condition": { "expr": "abs(signal.price.home - signal.price.away) - param.edge", "comparator": ">=", "value": "0" },
      "fire": "if_true", "rearm": { "mode": "immediate" } }
  ],
  "legs": [ { "name": "main", "resident": true, "arm": ["state_read", "compared", "cause", "named", "no_signal", "fine"], "body": [] } ]
}"#,
        },
        Case {
            name: "neg_window_misuse",
            json: r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": { "game": "nba.sim.h2h.current" },
  "signals": { "price": "nba.sim.h2h.current.price.v1" },
  "state": [
    { "name": "w", "kind": "window", "value_type": "decimal", "capacity": 8 },
    { "name": "n", "kind": "scalar", "value_type": "u8" }
  ],
  "params": [ { "name": "lo", "type": "decimal", "default": "0.4" } ],
  "triggers": [
    { "name": "tick", "condition": { "signal": "price" },
      "fire": "if_true", "rearm": { "mode": "immediate" } }
  ],
  "legs": [
    { "name": "main", "resident": true, "arm": ["tick"],
      "body": [
        { "name": "a", "do": ["push state.n = signal.price.home"] },
        { "name": "b", "do": ["set state.w = 1"] },
        { "name": "c", "if": "count(state.n) > 3", "do": [] },
        { "name": "d", "if": "state.w > 3", "do": [] },
        { "name": "e", "if": "count_where(state.w, entry.value > signal.price.home) > 1", "do": [] },
        { "name": "f", "if": "count_where(state.w, entry.price > state.n) > 1", "do": [] },
        { "name": "g", "if": "count_where(state.w, count(state.w) > 1) > 1", "do": [] },
        { "name": "h", "if": "count_where(state.w, entry.value > param.lo) > 1 and sum(state.w) > 0", "do": ["push state.w = 'home'"] },
        { "name": "i", "if": "sum(signal.price.home) > 0", "do": [] }
      ] }
  ]
}"#,
        },
        Case {
            name: "neg_entry_outside_reducer",
            json: r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": { "game": "nba.sim.h2h.current" },
  "signals": { "price": "nba.sim.h2h.current.price.v1" },
  "state": [],
  "triggers": [
    { "name": "tick", "condition": { "signal": "price" },
      "fire": "if_true", "rearm": { "mode": "immediate" } }
  ],
  "legs": [
    { "name": "main", "resident": true, "arm": ["tick"],
      "body": [ { "name": "note", "if": "entry.price > 0", "do": [] } ] }
  ]
}"#,
        },
        Case {
            name: "neg_envelope_reads",
            json: r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": { "game": "nba.sim.h2h.current" },
  "signals": { "price": "nba.sim.h2h.current.price.v1" },
  "state": [],
  "triggers": [
    { "name": "tick", "condition": { "signal": "price" },
      "fire": "if_true", "rearm": { "mode": "immediate" } }
  ],
  "legs": [
    { "name": "main", "resident": true, "arm": ["tick"],
      "body": [
        { "name": "gate", "if": "envelope.max_exposure_fraction > 0.5", "do": [] },
        { "name": "typo", "if": "envelope.max_order > 1", "do": [] }
      ] }
  ]
}"#,
        },
        Case {
            name: "neg_unknown_platform_path",
            json: r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": { "game": "nba.sim.h2h.current" },
  "signals": { "price": "nba.sim.h2h.current.price.v1" },
  "state": [],
  "triggers": [
    { "name": "tick", "condition": { "signal": "price" },
      "fire": "if_true", "rearm": { "mode": "immediate" } }
  ],
  "legs": [
    { "name": "main", "resident": true, "arm": ["tick"],
      "body": [ { "name": "note", "if": "platform.position > 0", "do": [] } ] }
  ]
}"#,
        },
        Case {
            name: "neg_legs_read_invalid",
            json: r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": { "game": "nba.sim.h2h.current" },
  "signals": { "price": "nba.sim.h2h.current.price.v1" },
  "state": [],
  "triggers": [
    { "name": "tick", "condition": { "signal": "price" },
      "fire": "if_true", "rearm": { "mode": "immediate" } }
  ],
  "legs": [
    { "name": "main", "resident": true, "arm": ["tick"],
      "body": [
        { "name": "a", "if": "legs.main.fill_qty > 0", "do": [] },
        { "name": "b", "if": "legs.main.tag > 0", "do": [] }
      ] }
  ]
}"#,
        },
        Case {
            name: "neg_legs_unknown_tag",
            json: r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": { "game": "nba.sim.h2h.current" },
  "signals": { "price": "nba.sim.h2h.current.price.v1" },
  "state": [],
  "triggers": [
    { "name": "tick", "condition": { "signal": "price" },
      "fire": "if_true", "rearm": { "mode": "immediate" } }
  ],
  "legs": [
    { "name": "main", "resident": true, "arm": ["tick"],
      "body": [
        { "name": "buy", "if": "triggered(tick)",
          "do": [
            { "place": {
                "instrument": "game", "outcome": "home", "side": "buy",
                "order_type": "market", "tif": "ioc",
                "size": "clamp(envelope.max_order_size, 0, envelope.max_order_size)",
                "max_size": "envelope.max_order_size", "tag": "open" } }
          ],
          "then": "exit" } ] },
    { "name": "exit", "resident": true, "arm": ["tick"],
      "body": [ { "name": "note", "if": "legs.main.tag.nope.fill_price > legs.main.tag.open.fill_price", "do": [] } ] }
  ]
}"#,
        },
        Case {
            name: "neg_set_lvalues",
            json: r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": { "game": "nba.sim.h2h.current" },
  "signals": { "pbp": "nba.sim.h2h.current.pbp.v1" },
  "state": [
    { "name": "s", "kind": "scalar", "value_type": "u8" },
    { "name": "m", "kind": "map",
      "key_types": [ { "type": "enum", "variants": ["home", "away"] } ],
      "value_type": "u8", "max_keys": 4, "overflow": "refuse_update" }
  ],
  "triggers": [
    { "name": "play", "condition": { "signal": "pbp" },
      "fire": "if_true", "rearm": { "mode": "immediate" } }
  ],
  "legs": [
    { "name": "main", "resident": true, "arm": ["play"],
      "body": [
        { "name": "a", "do": ["set state.s[1] = 1"] },
        { "name": "b", "do": ["set state.m = 1"] },
        { "name": "c", "do": ["set state.m['home', 2] = 1"] },
        { "name": "d", "do": ["set state.m['blue'] = 1"] },
        { "name": "e", "do": ["set state.s = 'home'"] }
      ] }
  ]
}"#,
        },
        Case {
            name: "neg_string_typing",
            json: r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": { "game": "nba.sim.h2h.current" },
  "signals": { "bets": "nba.sim.h2h.current.bets.v1" },
  "state": [ { "name": "who", "kind": "scalar", "value_type": "string" }, { "name": "n", "kind": "scalar", "value_type": "u8" } ],
  "params": [ { "name": "target", "type": "string", "default": "0xabc" } ],
  "triggers": [
    { "name": "bet", "condition": { "signal": "bets" },
      "fire": "if_true", "rearm": { "mode": "immediate" } }
  ],
  "legs": [
    { "name": "main", "resident": true, "arm": ["bet"],
      "body": [
        { "name": "ok", "if": "triggered(bet) and event.bettor == param.target and state.who != signal.bets.bettor",
          "do": ["set state.who = event.bettor"] },
        { "name": "ordered", "if": "signal.bets.bettor > param.target", "do": [] },
        { "name": "arith", "if": "signal.bets.bettor + 1 > 2", "do": [] },
        { "name": "mixed", "if": "signal.bets.bettor == signal.bets.stake", "do": [] },
        { "name": "bare", "if": "param.target", "do": [] },
        { "name": "slot", "do": ["set state.n = param.target", "set state.who = signal.bets.stake"] }
      ] }
  ]
}"#,
        },
        Case {
            name: "neg_guard_not_boolean",
            json: r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": { "game": "nba.sim.h2h.current" },
  "signals": { "price": "nba.sim.h2h.current.price.v1" },
  "state": [],
  "triggers": [
    { "name": "tick", "condition": { "signal": "price" },
      "fire": "if_true", "rearm": { "mode": "immediate" } }
  ],
  "legs": [
    { "name": "main", "resident": true, "arm": ["tick"],
      "body": [ { "name": "note", "if": "signal.price.home + 1", "do": [] } ] }
  ]
}"#,
        },
        Case {
            name: "neg_type_mismatch",
            json: r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": { "game": "nba.sim.h2h.current" },
  "signals": { "price": "nba.sim.h2h.current.price.v1" },
  "state": [],
  "triggers": [
    { "name": "tick", "condition": { "signal": "price" },
      "fire": "if_true", "rearm": { "mode": "immediate" } }
  ],
  "legs": [
    { "name": "main", "resident": true, "arm": ["tick"],
      "body": [ { "name": "note", "if": "signal.price.home == true", "do": [] } ] }
  ]
}"#,
        },
        Case {
            name: "neg_unknown_variant",
            json: r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": { "game": "nba.sim.h2h.current" },
  "signals": { "pbp": "nba.sim.h2h.current.pbp.v1" },
  "state": [],
  "triggers": [
    { "name": "play", "condition": { "signal": "pbp" },
      "fire": "if_true", "rearm": { "mode": "immediate" } }
  ],
  "legs": [
    { "name": "main", "resident": true, "arm": ["play"],
      "body": [ { "name": "note", "if": "signal.pbp.action == 'dunk'", "do": [] } ] }
  ]
}"#,
        },
        Case {
            name: "neg_event_unpinned_cross_signal_trigger",
            json: r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": { "game": "nba.sim.h2h.current" },
  "signals": { "fair": "pinnacle.nba.sim.h2h.current.fair.v1", "book": "nba.sim.h2h.current.price.v1" },
  "state": [ { "name": "x", "kind": "scalar", "value_type": "decimal" } ],
  "triggers": [
    { "name": "spread", "condition": { "expr": "signal.fair.home - signal.book.home", "comparator": ">", "value": "0.05" },
      "fire": "if_true", "rearm": { "mode": "immediate" } }
  ],
  "legs": [
    { "name": "main", "resident": true, "arm": ["spread"],
      "body": [ { "name": "note", "if": "triggered(spread)", "do": ["set state.x = event.home"] } ] }
  ]
}"#,
        },
    ]
}

/// Family B: registry cross-checks.
pub fn registry_cases() -> Vec<Case> {
    vec![
        Case {
            name: "neg_unknown_field",
            json: r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": { "game": "nba.sim.h2h.current" },
  "signals": { "price": "nba.sim.h2h.current.price.v1" },
  "state": [],
  "triggers": [
    { "name": "tick", "condition": { "signal": "price", "field": "hom", "comparator": "<", "value": "0.4" },
      "fire": "if_true", "rearm": { "mode": "immediate" } }
  ],
  "legs": [
    { "name": "main", "resident": true, "arm": ["tick"],
      "body": [ { "name": "note", "if": "signal.price.hom > 0.5", "do": [] } ] }
  ]
}"#,
        },
        Case {
            name: "neg_comparator_type_mismatch",
            json: r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": { "game": "nba.sim.h2h.current" },
  "signals": { "pbp": "nba.sim.h2h.current.pbp.v1" },
  "state": [],
  "triggers": [
    { "name": "shot", "condition": { "signal": "pbp", "field": "action", "comparator": "<", "value": "three_pointer" },
      "fire": "if_true", "rearm": { "mode": "immediate" } }
  ],
  "legs": [ { "name": "main", "resident": true, "arm": ["shot"], "body": [] } ]
}"#,
        },
        Case {
            name: "neg_value_type_mismatch",
            json: r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": { "game": "nba.sim.h2h.current" },
  "signals": { "pbp": "nba.sim.h2h.current.pbp.v1" },
  "state": [],
  "triggers": [
    { "name": "shot", "condition": { "all": [
        { "signal": "pbp", "field": "action", "comparator": "==", "value": "dunk" },
        { "signal": "pbp", "field": "quarter", "comparator": "==", "value": "made" }
      ] },
      "fire": "if_true", "rearm": { "mode": "immediate" } }
  ],
  "legs": [ { "name": "main", "resident": true, "arm": ["shot"], "body": [] } ]
}"#,
        },
        Case {
            name: "neg_leaf_value_forms",
            json: r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": { "game": "nba.sim.h2h.current" },
  "signals": { "price": "nba.sim.h2h.current.price.v1", "bets": "nba.sim.h2h.current.bets.v1", "pbp": "nba.sim.h2h.current.pbp.v1" },
  "state": [],
  "params": [
    { "name": "who", "type": "string", "default": "0xabc" },
    { "name": "flag", "type": "bool", "default": true },
    { "name": "team", "type": { "type": "enum", "variants": ["home", "away", "draw"] }, "default": "home" }
  ],
  "triggers": [
    { "name": "bad", "condition": { "all": [
        { "signal": "price", "field": "home", "comparator": "<", "value": { "str": "0.4" } },
        { "signal": "price", "field": "home", "comparator": "<", "value": { "param": "who" } },
        { "signal": "bets", "field": "bettor", "comparator": "==", "value": "whale" },
        { "signal": "bets", "field": "bettor", "comparator": "==", "value": "0" },
        { "signal": "bets", "field": "bettor", "comparator": ">", "value": { "str": "0xabc" } },
        { "signal": "bets", "field": "team", "comparator": "==", "value": { "str": "home" } },
        { "signal": "bets", "field": "team", "comparator": "==", "value": { "param": "team" } },
        { "expr": "signal.pbp.home_score - signal.pbp.away_score", "comparator": ">=", "value": { "param": "flag" } },
        { "expr": "signal.pbp.home_score - signal.pbp.away_score", "comparator": ">=", "value": { "str": "10" } },
        { "signal": "bets", "field": "bettor", "comparator": "==", "value": { "param": "who" } }
      ] },
      "fire": "if_true", "rearm": { "mode": "immediate" } }
  ],
  "legs": [ { "name": "main", "resident": true, "arm": ["bad"], "body": [] } ]
}"#,
        },
        Case {
            name: "neg_leaf_field_not_numeric",
            json: r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": { "game": "nba.sim.h2h.current" },
  "signals": { "pbp": "nba.sim.h2h.current.pbp.v1" },
  "state": [],
  "triggers": [
    { "name": "gap", "condition": { "expr": "signal.pbp.action", "comparator": ">=", "value": "10" },
      "fire": "if_true", "rearm": { "mode": "immediate" } }
  ],
  "legs": [ { "name": "main", "resident": true, "arm": ["gap"], "body": [] } ]
}"#,
        },
        Case {
            name: "neg_outcome_required",
            json: r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": { "game": "nba.sim.h2h.current" },
  "signals": { "price": "nba.sim.h2h.current.price.v1" },
  "state": [],
  "triggers": [
    { "name": "tick", "condition": { "signal": "price" },
      "fire": "if_true", "rearm": { "mode": "immediate" } }
  ],
  "legs": [
    { "name": "main", "resident": true, "arm": ["tick"],
      "body": [
        { "name": "buy", "if": "triggered(tick)",
          "do": [
            { "place": {
                "instrument": "game", "side": "buy",
                "order_type": "market", "tif": "ioc",
                "size": "clamp(envelope.max_order_size, 0, envelope.max_order_size)",
                "max_size": "envelope.max_order_size", "tag": "open" } }
          ],
          "then": "main" }
      ] }
  ]
}"#,
        },
        Case {
            name: "neg_outcome_forbidden",
            json: r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": { "perp": "btc_usd.perp" },
  "signals": { "ticks": "binance.btc_usdt.ticks.v1" },
  "state": [],
  "triggers": [
    { "name": "tick", "condition": { "signal": "ticks" },
      "fire": "if_true", "rearm": { "mode": "immediate" } }
  ],
  "legs": [
    { "name": "main", "resident": true, "arm": ["tick"],
      "body": [
        { "name": "buy", "if": "triggered(tick)",
          "do": [
            { "place": {
                "instrument": "perp", "outcome": "up", "side": "buy",
                "order_type": "market", "tif": "ioc",
                "size": "clamp(envelope.max_order_size, 0, envelope.max_order_size)",
                "max_size": "envelope.max_order_size", "tag": "open" } }
          ],
          "then": "main" }
      ] }
  ]
}"#,
        },
        Case {
            name: "neg_unknown_outcome",
            json: r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": { "game": "nba.sim.h2h.current" },
  "signals": { "price": "nba.sim.h2h.current.price.v1" },
  "state": [],
  "triggers": [
    { "name": "tick", "condition": { "signal": "price" },
      "fire": "if_true", "rearm": { "mode": "immediate" } }
  ],
  "legs": [
    { "name": "main", "resident": true, "arm": ["tick"],
      "body": [
        { "name": "buy", "if": "triggered(tick)",
          "do": [
            { "place": {
                "instrument": "game", "outcome": "hom", "side": "buy",
                "order_type": "market", "tif": "ioc",
                "size": "clamp(envelope.max_order_size, 0, envelope.max_order_size)",
                "max_size": "envelope.max_order_size", "tag": "open" } }
          ],
          "then": "main" }
      ] }
  ]
}"#,
        },
    ]
}

/// Family A: resolution, uniqueness, reservedness.
pub fn resolution_cases() -> Vec<Case> {
    vec![
        Case {
            name: "neg_duplicate_name",
            json: r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": { "game": "nba.sim.h2h.current" },
  "signals": { "price": "nba.sim.h2h.current.price.v1" },
  "state": [],
  "triggers": [
    { "name": "tick", "condition": { "signal": "price" },
      "fire": "if_true", "rearm": { "mode": "immediate" } },
    { "name": "tick", "condition": { "signal": "price" },
      "fire": "on_crossing", "rearm": { "mode": "never" } }
  ],
  "legs": [ { "name": "main", "resident": true, "arm": ["tick"], "body": [] } ]
}"#,
        },
        Case {
            name: "neg_reserved_name",
            json: r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": { "game": "nba.sim.h2h.current" },
  "signals": { "price": "nba.sim.h2h.current.price.v1" },
  "state": [ { "name": "min", "kind": "scalar", "value_type": "u8" } ],
  "triggers": [
    { "name": "tick", "condition": { "signal": "price" },
      "fire": "if_true", "rearm": { "mode": "immediate" } }
  ],
  "legs": [ { "name": "stay", "resident": true, "arm": ["tick"], "body": [] } ]
}"#,
        },
        Case {
            name: "neg_unknown_signal_id",
            json: r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": { "game": "nba.sim.h2h.current" },
  "signals": { "pbp": "nba.sim.h2h.current.pbp.v9" },
  "state": [],
  "triggers": [
    { "name": "tick", "condition": { "signal": "pbp" },
      "fire": "if_true", "rearm": { "mode": "immediate" } }
  ],
  "legs": [ { "name": "main", "resident": true, "arm": ["tick"], "body": [] } ]
}"#,
        },
        Case {
            name: "neg_unknown_instrument_id",
            json: r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": { "game": "nba.sim.h2h" },
  "signals": { "price": "nba.sim.h2h.current.price.v1" },
  "state": [],
  "triggers": [
    { "name": "tick", "condition": { "signal": "price" },
      "fire": "if_true", "rearm": { "mode": "immediate" } }
  ],
  "legs": [ { "name": "main", "resident": true, "arm": ["tick"], "body": [] } ]
}"#,
        },
        Case {
            name: "neg_unknown_signal",
            json: r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": { "game": "nba.sim.h2h.current" },
  "signals": { "price": "nba.sim.h2h.current.price.v1" },
  "state": [],
  "triggers": [
    { "name": "tick", "condition": { "signal": "prices", "field": "home", "comparator": "<", "value": "0.4" },
      "fire": "if_true", "rearm": { "mode": "immediate" } }
  ],
  "legs": [ { "name": "main", "resident": true, "arm": ["tick"], "body": [] } ]
}"#,
        },
        Case {
            name: "neg_unknown_instrument",
            json: r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": { "game": "nba.sim.h2h.current" },
  "signals": { "price": "nba.sim.h2h.current.price.v1" },
  "state": [],
  "triggers": [
    { "name": "tick", "condition": { "signal": "price" },
      "fire": "if_true", "rearm": { "mode": "immediate" } }
  ],
  "legs": [
    { "name": "main", "resident": true, "arm": ["tick"],
      "body": [
        { "name": "buy", "if": "triggered(tick)",
          "do": [
            { "place": {
                "instrument": "gme", "outcome": "home", "side": "buy",
                "order_type": "market", "tif": "ioc",
                "size": "clamp(envelope.max_order_size, 0, envelope.max_order_size)",
                "max_size": "envelope.max_order_size", "tag": "entry" } }
          ],
          "then": "main" }
      ] }
  ]
}"#,
        },
        Case {
            name: "neg_unknown_trigger",
            json: r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": { "game": "nba.sim.h2h.current" },
  "signals": { "price": "nba.sim.h2h.current.price.v1" },
  "state": [],
  "triggers": [
    { "name": "tick", "condition": { "signal": "price" },
      "fire": "if_true", "rearm": { "mode": "immediate" } }
  ],
  "legs": [
    { "name": "main", "resident": true, "arm": ["tick", "nope"],
      "body": [ { "name": "note", "if": "triggered(gone)", "do": [] } ] }
  ]
}"#,
        },
        Case {
            name: "neg_unknown_leg",
            json: r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": { "game": "nba.sim.h2h.current" },
  "signals": { "price": "nba.sim.h2h.current.price.v1" },
  "state": [],
  "triggers": [
    { "name": "tick", "condition": { "signal": "price" },
      "fire": "if_true", "rearm": { "mode": "immediate" } }
  ],
  "legs": [
    { "name": "main", "resident": true, "arm": ["tick"],
      "body": [ { "name": "note", "if": "triggered(tick)", "do": [], "then": "gone" } ] }
  ]
}"#,
        },
        Case {
            name: "neg_unknown_state_slot",
            json: r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": { "game": "nba.sim.h2h.current" },
  "signals": { "price": "nba.sim.h2h.current.price.v1" },
  "state": [ { "name": "count", "kind": "scalar", "value_type": "u8" } ],
  "triggers": [
    { "name": "tick", "condition": { "signal": "price" },
      "fire": "if_true", "rearm": { "mode": "immediate" } }
  ],
  "legs": [
    { "name": "main", "resident": true, "arm": ["tick"],
      "body": [ { "name": "note", "do": ["set state.cuont = state.count + 1"] } ] }
  ]
}"#,
        },
        Case {
            name: "neg_unknown_named_expr",
            json: r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": { "game": "nba.sim.h2h.current" },
  "signals": { "price": "nba.sim.h2h.current.price.v1" },
  "state": [],
  "triggers": [
    { "name": "tick", "condition": { "signal": "price" },
      "fire": "if_true", "rearm": { "mode": "immediate" } }
  ],
  "legs": [
    { "name": "main", "resident": true, "arm": ["tick"],
      "body": [ { "name": "note", "if": "expr.edge > 0", "do": [] } ] }
  ]
}"#,
        },
        Case {
            name: "neg_expr_ref_has_field",
            json: r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": { "game": "nba.sim.h2h.current" },
  "signals": { "price": "nba.sim.h2h.current.price.v1" },
  "state": [],
  "expressions": { "named": { "edge": "signal.price.home - 0.5" } },
  "triggers": [
    { "name": "tick", "condition": { "signal": "price" },
      "fire": "if_true", "rearm": { "mode": "immediate" } }
  ],
  "legs": [
    { "name": "main", "resident": true, "arm": ["tick"],
      "body": [ { "name": "note", "if": "expr.edge.raw > 0", "do": [] } ] }
  ]
}"#,
        },
        Case {
            name: "neg_unknown_table",
            json: r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": { "game": "nba.sim.h2h.current" },
  "signals": { "price": "nba.sim.h2h.current.price.v1" },
  "state": [],
  "triggers": [
    { "name": "tick", "condition": { "signal": "price" },
      "fire": "if_true", "rearm": { "mode": "immediate" } }
  ],
  "legs": [
    { "name": "main", "resident": true, "arm": ["tick"],
      "body": [ { "name": "note", "if": "lookup(table.curve, signal.price.home) > 0", "do": [] } ] }
  ]
}"#,
        },
        Case {
            name: "neg_unknown_param",
            json: r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": { "game": "nba.sim.h2h.current" },
  "signals": { "price": "nba.sim.h2h.current.price.v1" },
  "state": [],
  "params": [ { "name": "edge", "type": "decimal", "default": "0.04" } ],
  "triggers": [
    { "name": "cheap", "condition": { "signal": "price", "field": "home", "comparator": "<", "value": { "param": "gone" } },
      "fire": "if_true", "rearm": { "mode": "immediate" } }
  ],
  "legs": [
    { "name": "main", "resident": true, "arm": ["cheap"],
      "body": [
        { "name": "a", "if": "signal.price.home < param.nope", "do": [] },
        { "name": "b", "if": "signal.price.home < param.edge.raw", "do": [] }
      ] }
  ]
}"#,
        },
        Case {
            name: "neg_unknown_tag",
            json: r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": { "game": "nba.sim.h2h.current" },
  "signals": { "price": "nba.sim.h2h.current.price.v1" },
  "state": [],
  "triggers": [
    { "name": "tick", "condition": { "signal": "price" },
      "fire": "if_true", "rearm": { "mode": "immediate" } }
  ],
  "legs": [
    { "name": "main", "resident": true, "arm": ["tick"],
      "body": [
        { "name": "buy", "if": "triggered(tick)",
          "do": [
            { "place": {
                "instrument": "game", "outcome": "home", "side": "buy",
                "order_type": "market", "tif": "ioc",
                "size": "clamp(envelope.max_order_size, 0, envelope.max_order_size)",
                "max_size": "envelope.max_order_size", "tag": "entry" } },
            { "cancel": { "tag": "nada" } }
          ],
          "then": "main" }
      ] }
  ]
}"#,
        },
        Case {
            name: "neg_reserved_and_duplicate_in_new_sections",
            json: r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": { "game": "nba.sim.h2h.current" },
  "signals": { "price": "nba.sim.h2h.current.price.v1" },
  "state": [ { "name": "entry", "kind": "window", "value_type": "decimal", "capacity": 4 } ],
  "params": [
    { "name": "param", "type": "decimal", "default": "1" },
    { "name": "stay", "type": "decimal", "default": "1" },
    { "name": "edge", "type": "decimal", "default": "1" },
    { "name": "edge", "type": "decimal", "default": "2" }
  ],
  "triggers": [
    { "name": "tick", "condition": { "signal": "price" },
      "fire": "if_true", "rearm": { "mode": "immediate" } }
  ],
  "legs": [ { "name": "main", "resident": true, "arm": ["tick"], "body": [] } ]
}"#,
        },
    ]
}
