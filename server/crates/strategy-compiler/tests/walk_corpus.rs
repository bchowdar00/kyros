//! The document walker over the corpus: all eight fixtures walk clean, and a
//! deliberately-broken document produces the multi-diagnostic, path-anchored output the
//! repair loop consumes (snapshotted as contract).

use std::fs;
use std::path::{Path, PathBuf};

use strategy_compiler::parse::walk_strategy;

const STRATEGIES: [&str; 9] = [
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

fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus")
}

#[test]
fn all_corpus_fixtures_walk_clean() {
    for name in STRATEGIES {
        let path = corpus_dir().join(format!("{name}.json"));
        let text = fs::read_to_string(&path).expect("fixture reads");
        let strategy: strategy_compiler::ir::Strategy = serde_json::from_str(&text).expect("fixture parses");
        let outcome = walk_strategy(&strategy);
        let rendered: Vec<String> = outcome
            .diagnostics
            .iter()
            .map(|d| format!("{}: {}", d.path, d.error))
            .collect();
        assert!(
            outcome.diagnostics.is_empty(),
            "{name} failed to walk:\n{}",
            rendered.join("\n")
        );
        assert!(
            !outcome.walked.is_empty(),
            "{name}: a strategy with no embedded expressions is not a strategy"
        );
    }
}

#[test]
fn broken_document_reports_failures_and_keeps_what_parsed() {
    // Four distinct failure classes in one document — a superseded builtin in a guard,
    // a v0.1 timer namespace in a second guard, an invalid statement lvalue, and a
    // bare (unqualified) field inside a trigger leaf expression — COEXISTING with healthy
    // strings. Partial success is the contract: the validator validates what parsed
    // while the repair loop fixes the rest in the same round.
    let json = r#"{
      "meta": { "name": "Broken", "description": "walker negative case", "dsl_version": "0.3" },
      "instruments": { "market": "btc_usd.updown_2m.current" },
      "signals": { "book": "btc_usd.updown_2m.current.price.v1" },
      "state": [],
      "triggers": [
        { "name": "cheap",
          "condition": { "all": [
            { "signal": "book", "field": "up", "comparator": "<", "value": "0.3" },
            { "expr": "signal.book.bid - ask", "comparator": ">", "value": "0" }
          ]},
          "fire": "if_true", "rearm": { "mode": "immediate" } }
      ],
      "legs": [
        { "name": "main", "resident": true, "arm": ["cheap"],
          "body": [
            { "name": "old_crossed",
              "if": "crossed(signal.book.up > 0.75)",
              "do": ["set platform.position = 1"] },
            { "name": "old_timer",
              "if": "fired(timer.analyze)",
              "do": [] },
            { "name": "healthy_entry",
              "if": "triggered(cheap) and signal.book.up < 0.3",
              "do": [
                "set state.entries = state.entries + 1",
                { "place": {
                    "instrument": "market", "outcome": "up", "side": "buy",
                    "order_type": "market", "tif": "ioc",
                    "size": "clamp(envelope.max_order_size, 0, envelope.max_order_size)",
                    "max_size": "envelope.max_order_size", "tag": "entry"
                }}
              ] }
          ] }
      ]
    }"#;
    let strategy: strategy_compiler::ir::Strategy = serde_json::from_str(json).expect("shape-valid document");
    let outcome = walk_strategy(&strategy);
    let mut rendered = vec![format!("PARSED ({}):", outcome.walked.len())];
    rendered.extend(
        outcome
            .walked
            .iter()
            .map(|w| format!("  {} ({:?})", w.path, w.role)),
    );
    rendered.push(format!("\nDIAGNOSTICS ({}):", outcome.diagnostics.len()));
    rendered.extend(
        outcome
            .diagnostics
            .iter()
            .map(|d| format!("--- {}\n{}", d.path, d.error)),
    );
    insta::assert_snapshot!("walk_broken_document", rendered.join("\n"));
}
