//! Manifest types: serde round-trip fidelity and the wire-trigger exactly-one
//! invariant. The manifest is compiler output, but its types must still reject
//! illegal states — a bundle edited by hand or produced by a broken build should
//! fail to parse, not silently half-load.

use strategy_compiler::ir::{Manifest, WireCondition, WireExpr, WireTrigger, WireTriggerSource};

const MANIFEST: &str = r#"{
  "schema_version": 2,
  "strategy_id": "Strategy_46ab2367683b",
  "name": "Favorite Panic",
  "description": "Buy the panicking favorite.",
  "dsl_version": "0.3",
  "source_hash": "46ab2367683b85493cd304e8199f87923e9bcbc3e13ecdd6a40ab57d85363f34",
  "signals": [
    { "signal_id": "nba.sim.h2h.current.pbp.v1", "staleness_policy": "proceed" }
  ],
  "watch_fields": [
    { "signal_id": "nba.sim.h2h.current.pbp.v1", "field": "action" }
  ],
  "triggers": [
    {
      "id": "made_three",
      "condition": { "all": [
        { "signal_id": "nba.sim.h2h.current.pbp.v1", "field": "action",
          "comparator": "==", "value": "three_pointer" },
        { "signal_id": "nba.sim.h2h.current.pbp.v1",
          "expr": { "op": "-", "lhs": { "field": "home_score" },
                    "rhs": { "op": "neg", "arg": { "const": "1" } } },
          "comparator": ">=", "value": "10" },
        { "signal_id": "nba.sim.h2h.current.pbp.v1", "comparator": "always" }
      ]},
      "fire": "if_true",
      "rearm": { "mode": "immediate" }
    },
    {
      "id": "deadline",
      "timer": {
        "kind": "window_clock",
        "window_clock": { "instrument_id": "btc_usd.updown_2m.current", "remaining_ms": 20000 }
      },
      "fire": "on_crossing",
      "rearm": { "mode": "never" }
    }
  ],
  "lifecycle_stages": [
    { "name": "hunting", "resident": false, "arm": ["made_three", "deadline"] },
    { "name": "done", "resident": true, "arm": [] }
  ],
  "intent_index": {
    "hunting.take.0": {
      "on": { "fill": "done", "partial": "stay", "reject": "hunting",
              "cancel_ack": "stay", "expiry": "stay" },
      "max_size": "100"
    }
  },
  "envelope": {
    "allocation": "1000",
    "loss_budget": "500",
    "max_order_size": "100"
  }
}"#;

#[test]
fn manifest_round_trips() {
    let manifest: Manifest = serde_json::from_str(MANIFEST).expect("manifest parses");
    let text = serde_json::to_string(&manifest).expect("manifest serializes");
    let again: Manifest = serde_json::from_str(&text).expect("re-parses");
    assert_eq!(manifest, again);

    // Serialization is deterministic: same value, same bytes.
    assert_eq!(text, serde_json::to_string(&again).expect("serializes"));
}

#[test]
fn round_trip_preserves_leaf_shapes() {
    let manifest: Manifest = serde_json::from_str(MANIFEST).expect("manifest parses");
    let trigger = &manifest.triggers[0];
    let WireTriggerSource::Condition(WireCondition::All { all }) = &trigger.source else {
        panic!("expected an all-group condition");
    };
    let [field, expr, bare] = all.as_slice() else {
        panic!("expected three leaves");
    };
    let (WireCondition::Leaf(field), WireCondition::Leaf(expr), WireCondition::Leaf(bare)) =
        (field, expr, bare)
    else {
        panic!("expected leaves");
    };
    assert!(field.field.is_some() && field.expr.is_none() && field.value.is_some());
    assert!(expr.field.is_none() && expr.expr.is_some() && expr.value.is_some());
    assert!(bare.field.is_none() && bare.expr.is_none() && bare.value.is_none());
    assert_eq!(bare.comparator, strategy_compiler::ir::WireComparator::Always);

    // The op-tree parsed into its exact node shapes, negation included.
    let Some(WireExpr::Bin { op, lhs, rhs }) = &expr.expr else {
        panic!("expected a binary op-tree root");
    };
    assert_eq!(*op, strategy_compiler::ir::WireArithOp::Sub);
    assert!(matches!(lhs.as_ref(), WireExpr::Field(f) if f == "home_score"));
    let WireExpr::Neg(arg) = rhs.as_ref() else {
        panic!("expected a neg node");
    };
    assert!(matches!(arg.as_ref(), WireExpr::Const(c) if c == "1"));
}

#[test]
fn wire_trigger_requires_exactly_one_source() {
    let both = r#"{
      "id": "t",
      "condition": { "signal_id": "s", "comparator": "always" },
      "timer": { "kind": "window_clock",
                 "window_clock": { "instrument_id": "i", "remaining_ms": 1 } },
      "fire": "if_true",
      "rearm": { "mode": "immediate" }
    }"#;
    assert!(serde_json::from_str::<WireTrigger>(both).is_err());

    let neither = r#"{ "id": "t", "fire": "if_true", "rearm": { "mode": "immediate" } }"#;
    assert!(serde_json::from_str::<WireTrigger>(neither).is_err());
}

#[test]
fn op_tree_rejects_malformed_nodes() {
    // A node must be exactly one of field / const / op.
    assert!(serde_json::from_str::<WireExpr>(r#"{ "field": "x", "const": "1" }"#).is_err());
    // A binary node needs both operands.
    assert!(serde_json::from_str::<WireExpr>(r#"{ "op": "+", "lhs": { "field": "x" } }"#).is_err());
    // neg takes arg, never lhs/rhs.
    assert!(
        serde_json::from_str::<WireExpr>(
            r#"{ "op": "neg", "lhs": { "field": "x" }, "rhs": { "field": "y" } }"#
        )
        .is_err()
    );
    // Unknown operators are rejected, not swallowed.
    assert!(
        serde_json::from_str::<WireExpr>(
            r#"{ "op": "%", "lhs": { "field": "x" }, "rhs": { "field": "y" } }"#
        )
        .is_err()
    );
    // Unknown keys are rejected (strict mirror, not serde untagged).
    assert!(serde_json::from_str::<WireExpr>(r#"{ "field": "x", "junk": 1 }"#).is_err());
}

#[test]
fn op_tree_source_node_round_trips_alone() {
    let node: WireExpr = serde_json::from_str(r#"{ "source": "signal.fair.up - signal.book.ask" }"#)
        .expect("source node parses");
    assert!(matches!(&node, WireExpr::Source(s) if s == "signal.fair.up - signal.book.ask"));
    assert_eq!(
        serde_json::to_string(&node).expect("serializes"),
        r#"{"source":"signal.fair.up - signal.book.ask"}"#
    );
    assert!(serde_json::from_str::<WireExpr>(r#"{ "source": "x", "field": "y" }"#).is_err());
}

#[test]
fn unknown_fields_are_rejected() {
    let extra = MANIFEST.replacen(
        "\"schema_version\": 2,",
        "\"schema_version\": 2, \"surprise\": true,",
        1,
    );
    assert!(serde_json::from_str::<Manifest>(&extra).is_err());
}
