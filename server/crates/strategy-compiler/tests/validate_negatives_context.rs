//! Family C and G negatives: context restrictions and typing-lite, plus the
//! failed-guard cascade case.

mod common;

#[test]
fn context_negatives() {
    for (name, rendered) in common::rendered_cases(&common::context_cases()) {
        insta::assert_snapshot!(name, rendered);
    }
}

#[test]
fn failed_guard_suppresses_event_pin_checks() {
    // Cascade-suppression design case 3: the guard's parse failure is the ONE
    // diagnostic; the statement's `event.quarter` read must not be second-guessed as
    // unpinned — with no guard AST the pin is unknowable.
    let json = r#"{
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
      "body": [
        { "name": "note", "if": "crossed(signal.pbp.quarter > 2)",
          "do": ["set state.q = event.quarter"] }
      ] }
  ]
}"#;
    let diagnostics = common::run(json);
    assert_eq!(
        diagnostics.len(),
        1,
        "expected exactly the guard parse diagnostic:\n{}",
        common::render(&diagnostics)
    );
    insta::assert_snapshot!("neg_failed_guard_cascade", common::render(&diagnostics));
}

#[test]
fn probe_bare_identifier_in_cross_signal_trigger_does_not_cascade() {
    let json = r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": { "game": "nba.sim.h2h.current" },
  "signals": { "fair": "pinnacle.nba.sim.h2h.current.fair.v1", "book": "nba.sim.h2h.current.price.v1" },
  "state": [ { "name": "x", "kind": "scalar", "value_type": "decimal" } ],
  "triggers": [
    { "name": "spread", "condition": { "expr": "signal.fair.home - ask", "comparator": ">", "value": "0.05" },
      "fire": "if_true", "rearm": { "mode": "immediate" } }
  ],
  "legs": [
    { "name": "main", "resident": true, "arm": ["spread"],
      "body": [ { "name": "note", "if": "triggered(spread)", "do": ["set state.x = event.home"] } ] }
  ]
}"#;
    let diagnostics = common::run(json);
    assert_eq!(
        diagnostics.len(),
        1,
        "expected exactly the bare-identifier parse diagnostic:\n{}",
        common::render(&diagnostics)
    );
    insta::assert_snapshot!("probe_bare_identifier_no_cascade", common::render(&diagnostics));
}
