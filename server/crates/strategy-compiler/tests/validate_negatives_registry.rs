//! Family B negatives: registry cross-checks — one broken document per code, plus the
//! unresolved-signal cascade case.

mod common;

#[test]
fn registry_negatives() {
    for (name, rendered) in common::rendered_cases(&common::registry_cases()) {
        insta::assert_snapshot!(name, rendered);
    }
}

#[test]
fn unresolved_signal_suppresses_field_checks() {
    // Cascade-suppression design case 5: a bad signal id is ONE diagnostic. The field
    // leaf and the body read on that nickname must not fan out into phantom
    // unknown-field errors — the nickname is declared, only its registry entry is
    // missing.
    let json = r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": { "game": "nba.sim.h2h.current" },
  "signals": { "pbp": "nba.sim.h2h.current.pbp.v9" },
  "state": [],
  "triggers": [
    { "name": "shot", "condition": { "signal": "pbp", "field": "action", "comparator": "==", "value": "three_pointer" },
      "fire": "if_true", "rearm": { "mode": "immediate" } }
  ],
  "legs": [
    { "name": "main", "resident": true, "arm": ["shot"],
      "body": [ { "name": "note", "if": "signal.pbp.quarter >= 2", "do": [] } ] }
  ]
}"#;
    let diagnostics = common::run(json);
    assert_eq!(
        diagnostics.len(),
        1,
        "expected exactly the unknown-signal-id diagnostic:\n{}",
        common::render(&diagnostics)
    );
    insta::assert_snapshot!(
        "neg_unresolved_signal_cascade",
        common::render(&diagnostics)
    );
}
