//! Family A negatives: resolution, uniqueness, reservedness — one broken document per
//! code, snapshotting the full rendered diagnostic list.

mod common;

#[test]
fn resolution_negatives() {
    for (name, rendered) in common::rendered_cases(&common::resolution_cases()) {
        insta::assert_snapshot!(name, rendered);
    }
}

#[test]
fn base_document_is_clean() {
    // The mutation base itself must validate clean, or every case's snapshot would
    // carry unrelated noise.
    let diagnostics = common::run(common::BASE);
    assert!(
        diagnostics.is_empty(),
        "base document not clean:\n{}",
        common::render(&diagnostics)
    );
}

#[test]
fn poisoned_named_expr_reports_exactly_once() {
    // Cascade-suppression design case 1 (partial until the substitution pass lands):
    // a named expression that fails to parse is ONE parse diagnostic; the size
    // expression referencing it resolves fine (the name is declared) and must not
    // produce an unknown-reference echo.
    let json = r#"{
  "meta": { "name": "N", "description": "d", "dsl_version": "0.3" },
  "instruments": { "game": "nba.sim.h2h.current" },
  "signals": { "price": "nba.sim.h2h.current.price.v1" },
  "state": [],
  "expressions": { "named": { "sz": "clamp(envelope.max_order_size, 0," } },
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
                "size": "expr.sz",
                "max_size": "envelope.max_order_size", "tag": "entry" } }
          ],
          "then": "main" }
      ] }
  ]
}"#;
    let diagnostics = common::run(json);
    assert_eq!(
        diagnostics.len(),
        1,
        "expected exactly the parse diagnostic:\n{}",
        common::render(&diagnostics)
    );
    insta::assert_snapshot!("neg_poisoned_named_expr", common::render(&diagnostics));
}
