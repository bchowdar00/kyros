//! Negative deserialization cases, one per failure class the types enforce, plus
//! positive probes for every ruled-but-unfixtured spelling. The error-message
//! substrings asserted here are contract: they are what the LLM repair loop reads.

fn rejects<T: serde::de::DeserializeOwned + std::fmt::Debug>(json: &str, needle: &str) {
    let err = serde_json::from_str::<T>(json).expect_err("must fail to deserialize");
    let message = err.to_string();
    assert!(
        message.contains(needle),
        "error for {json:?} should mention {needle:?}, got: {message}"
    );
}

fn accepts<T: serde::de::DeserializeOwned>(json: &str) -> T {
    serde_json::from_str::<T>(json)
        .unwrap_or_else(|e| panic!("{json:?} must deserialize, got: {e}"))
}

// ---- trigger: exactly one of condition | timer ---------------------------------------

const FIRE_REARM: &str = r#""fire": "if_true", "rearm": { "mode": "immediate" }"#;

#[test]
fn trigger_with_condition_and_timer_is_rejected() {
    let json = format!(
        r#"{{"name": "t", "condition": {{"signal": "book"}},
            "timer": {{"kind": "window_clock", "instrument": "m", "remaining": "10s"}}, {FIRE_REARM}}}"#
    );
    rejects::<strategy_compiler::ir::Trigger>(&json, "exactly one");
}

#[test]
fn trigger_with_neither_source_is_rejected() {
    let json = format!(r#"{{"name": "t", {FIRE_REARM}}}"#);
    rejects::<strategy_compiler::ir::Trigger>(&json, "exactly one");
}

#[test]
fn rearm_cooldown_is_a_duration() {
    let json = r#"{"name": "t", "condition": {"signal": "book"},
                   "fire": "if_true", "rearm": { "mode": "immediate", "cooldown": "30s" }}"#;
    let trigger: strategy_compiler::ir::Trigger = accepts(json);
    assert_eq!(trigger.rearm.cooldown.as_ref().map(|c| c.as_millis()), Some(30_000));
    let bad = r#"{"name": "t", "condition": {"signal": "book"},
                  "fire": "if_true", "rearm": { "mode": "immediate", "cooldown": "1.5s" }}"#;
    rejects::<strategy_compiler::ir::Trigger>(bad, "duration");
}

#[test]
fn after_and_interval_timers_parse() {
    accepts::<strategy_compiler::ir::TimerSpec>(r#"{"kind": "after", "elapsed": "5s"}"#);
    accepts::<strategy_compiler::ir::TimerSpec>(r#"{"kind": "interval", "every": "500ms"}"#);
    assert!(serde_json::from_str::<strategy_compiler::ir::TimerSpec>(r#"{"kind": "after", "every": "5s"}"#).is_err());
}

// ---- condition leaves ----------------------------------------------------------------

#[test]
fn leaf_with_field_and_expr_is_rejected() {
    rejects::<strategy_compiler::ir::ConditionLeaf>(
        r#"{"signal": "s", "field": "f", "expr": "signal.s.a - signal.s.b", "comparator": ">", "value": "1"}"#,
        "never both",
    );
}

#[test]
fn bare_leaf_with_comparator_is_rejected() {
    rejects::<strategy_compiler::ir::ConditionLeaf>(
        r#"{"signal": "s", "comparator": ">", "value": "1"}"#,
        "bare leaf",
    );
}

#[test]
fn field_leaf_missing_value_is_rejected() {
    rejects::<strategy_compiler::ir::ConditionLeaf>(
        r#"{"signal": "s", "field": "f", "comparator": ">"}"#,
        "requires",
    );
}

#[test]
fn bare_and_comparison_leaves_parse() {
    accepts::<strategy_compiler::ir::ConditionLeaf>(r#"{"signal": "book"}"#);
    accepts::<strategy_compiler::ir::ConditionLeaf>(
        r#"{"signal": "book", "field": "up", "comparator": "!=", "value": "0.5"}"#,
    );
    accepts::<strategy_compiler::ir::ConditionLeaf>(
        r#"{"expr": "signal.pbp.home_score - signal.pbp.away_score", "comparator": ">=", "value": "10"}"#,
    );
}

#[test]
fn expr_leaf_carries_no_signal_key() {
    rejects::<strategy_compiler::ir::ConditionLeaf>(
        r#"{"signal": "pbp", "expr": "signal.pbp.home_score - 1", "comparator": ">=", "value": "10"}"#,
        "no `signal`",
    );
    rejects::<strategy_compiler::ir::ConditionLeaf>(
        r#"{"comparator": ">=", "value": "10"}"#,
        "names a `signal` or carries an `expr`",
    );
}

#[test]
fn leaf_value_object_forms_carry_exactly_one_key() {
    accepts::<strategy_compiler::ir::LeafValue>(r#"{"str": "0xabc"}"#);
    accepts::<strategy_compiler::ir::LeafValue>(r#"{"param": "target"}"#);
    rejects::<strategy_compiler::ir::LeafValue>(r#"{"str": "a", "param": "b"}"#, "exactly one");
    rejects::<strategy_compiler::ir::LeafValue>(r#"{}"#, "exactly one");
    rejects::<strategy_compiler::ir::LeafValue>(r#"{"param": "not an ident"}"#, "leaf value");
    rejects::<strategy_compiler::ir::LeafValue>(r#"{"text": "a"}"#, "leaf value");
}

#[test]
fn param_defaults_are_strings_or_booleans() {
    accepts::<strategy_compiler::ir::Param>(r#"{"name": "edge", "type": "decimal", "default": "0.04"}"#);
    accepts::<strategy_compiler::ir::Param>(r#"{"name": "on", "type": "bool", "default": true}"#);
    accepts::<strategy_compiler::ir::Param>(
        r#"{"name": "side", "type": {"type": "enum", "variants": ["buy", "sell"]}, "default": "buy"}"#,
    );
    rejects::<strategy_compiler::ir::Param>(r#"{"name": "edge", "type": "decimal", "default": 0.04}"#, "boolean");
    rejects::<strategy_compiler::ir::Param>(r#"{"name": "edge", "type": "decimal"}"#, "default");
    assert!(
        serde_json::from_str::<strategy_compiler::ir::Param>(
            r#"{"name": "edge", "type": "u16", "default": "1"}"#
        )
        .is_err()
    );
}

#[test]
fn signed_leaf_values_follow_the_canonical_ruling() {
    accepts::<strategy_compiler::ir::LeafValue>(r#""-0.0008""#);
    accepts::<strategy_compiler::ir::LeafValue>(r#""-12""#);
    accepts::<strategy_compiler::ir::LeafValue>(r#""three_pointer""#);
    rejects::<strategy_compiler::ir::LeafValue>(r#""-0""#, "canonical");
    rejects::<strategy_compiler::ir::LeafValue>(r#""-0.40""#, "canonical");
    rejects::<strategy_compiler::ir::LeafValue>(r#""0.40""#, "canonical");
}

// ---- decimals and durations ----------------------------------------------------------

#[test]
fn decimal_canonical_form_is_enforced() {
    accepts::<strategy_compiler::ir::DecimalStr>(r#""0.4""#);
    accepts::<strategy_compiler::ir::DecimalStr>(r#""1000""#);
    rejects::<strategy_compiler::ir::DecimalStr>(r#""0.40""#, "canonical");
    rejects::<strategy_compiler::ir::DecimalStr>(r#""05""#, "canonical");
    rejects::<strategy_compiler::ir::DecimalStr>(r#"".5""#, "canonical");
    rejects::<strategy_compiler::ir::DecimalStr>(r#""5.""#, "canonical");
    rejects::<strategy_compiler::ir::DecimalStr>(r#""1e5""#, "canonical");
    rejects::<strategy_compiler::ir::DecimalStr>(r#""-1""#, "canonical");
}

#[test]
fn price_band_is_unsigned_by_correctness() {
    rejects::<strategy_compiler::ir::PriceBand>(r#"["-0.1", "0.9"]"#, "canonical");
    accepts::<strategy_compiler::ir::PriceBand>(r#"["0.01", "0.99"]"#);
}

#[test]
fn expression_text_excludes_the_escaping_hazards() {
    accepts::<strategy_compiler::ir::ExprStr>(r#""signal.book.up >= 0.95""#);
    rejects::<strategy_compiler::ir::ExprStr>(r#""event.result == \"missed\"""#, "expression text");
    rejects::<strategy_compiler::ir::ExprStr>(r#""""#, "expression text");
}

// ---- typeSpec: one rule, both spellings ----------------------------------------------

#[test]
fn type_spec_accepts_both_spellings_everywhere() {
    accepts::<strategy_compiler::ir::TypeSpec>(r#""u8""#);
    accepts::<strategy_compiler::ir::TypeSpec>(r#"{"type": "u8"}"#);
    accepts::<strategy_compiler::ir::TypeSpec>(r#""decimal""#);
    accepts::<strategy_compiler::ir::TypeSpec>(r#"{"type": "decimal"}"#);
    accepts::<strategy_compiler::ir::TypeSpec>(r#""bool""#);
    accepts::<strategy_compiler::ir::TypeSpec>(r#"{"type": "bool"}"#);
    accepts::<strategy_compiler::ir::TypeSpec>(r#"{"type": "enum", "variants": ["home", "away"]}"#);
}

#[test]
fn unknown_primitive_types_are_rejected() {
    assert!(serde_json::from_str::<strategy_compiler::ir::TypeSpec>(r#""u16""#).is_err());
    assert!(serde_json::from_str::<strategy_compiler::ir::TypeSpec>(r#"{"type": "u16"}"#).is_err());
}

// ---- do items ------------------------------------------------------------------------

#[test]
fn ambiguous_multi_intent_object_is_rejected() {
    let json = r#"{"place": {"instrument": "m", "outcome": "up", "side": "buy",
                   "order_type": "market", "tif": "ioc",
                   "size": "clamp(envelope.max_order_size, 0, envelope.max_order_size)",
                   "max_size": "envelope.max_order_size", "tag": "t"},
                   "cancel": {"tag": "t"}}"#;
    assert!(serde_json::from_str::<strategy_compiler::ir::DoItem>(json).is_err());
}

#[test]
fn cancel_replace_requires_its_price() {
    let json = r#"{"cancel_replace": {"tag": "bid", "instrument": "g", "outcome": "home",
                   "side": "buy", "order_type": "limit", "tif": "gtc",
                   "price_band": ["0.01", "0.99"],
                   "size": "clamp(envelope.max_order_size, 0, envelope.max_order_size)",
                   "max_size": "envelope.max_order_size"}}"#;
    assert!(serde_json::from_str::<strategy_compiler::ir::CancelReplaceItem>(json).is_err());
}

// ---- on table ------------------------------------------------------------------------

#[test]
fn on_table_full_alphabet_and_stay_parse() {
    let table: strategy_compiler::ir::OnTable = accepts(
        r#"{"fill": "holding", "partial": "stay", "reject": "hunting",
            "cancel_ack": "stay", "expiry": "done"}"#,
    );
    assert_eq!(table.partial, Some(strategy_compiler::ir::OnDest::Stay));
    assert_eq!(
        table.fill,
        Some(strategy_compiler::ir::OnDest::Leg(accepts::<strategy_compiler::ir::Ident>(r#""holding""#)))
    );
}

#[test]
fn on_table_unknown_key_is_rejected() {
    assert!(serde_json::from_str::<strategy_compiler::ir::OnTable>(r#"{"bogus": "stay"}"#).is_err());
}

// ---- structure -----------------------------------------------------------------------

#[test]
fn if_less_block_always_runs() {
    let block: strategy_compiler::ir::Block = accepts(r#"{"name": "b", "do": []}"#);
    assert!(block.guard.is_none());
}

#[test]
fn block_unknown_field_is_rejected() {
    assert!(serde_json::from_str::<strategy_compiler::ir::Block>(r#"{"name": "b", "do": [], "when": "x"}"#).is_err());
}

// ---- verification probes: malformed documents at the IR boundary --------------------

#[test]
fn probe_expr_leaf_with_str_object_value_is_a_validator_job() {
    accepts::<strategy_compiler::ir::ConditionLeaf>(
        r#"{"expr": "signal.a.b", "comparator": ">", "value": {"str": "x"}}"#,
    );
}

#[test]
fn probe_expr_leaf_plus_signal_names_the_offending_key() {
    rejects::<strategy_compiler::ir::ConditionLeaf>(
        r#"{"signal": "a", "expr": "signal.a.b", "comparator": ">", "value": "1"}"#,
        "no `signal`",
    );
}

#[test]
fn probe_window_slot_with_map_only_field() {
    let json = r#"{"name": "w", "kind": "window", "value_type": "decimal", "capacity": 4, "max_keys": 2}"#;
    // Internally tagged enums cannot deny unknown fields: the stray `max_keys` is
    // silently dropped, and the round-trip echo test is the only guard. Documented here.
    accepts::<strategy_compiler::ir::StateSlot>(json);
}

#[test]
fn probe_param_without_default() {
    rejects::<strategy_compiler::ir::Param>(r#"{"name": "p", "type": "string"}"#, "default");
}

#[test]
fn probe_target_position_rejects_price() {
    let json = r#"{"target_position": {"instrument": "m", "size": "x", "max_size": "y", "tag": "t", "price": "0.5"}}"#;
    rejects::<strategy_compiler::ir::TargetPositionItem>(json, "unknown field `price`");
}

#[test]
fn probe_interval_timer_without_every() {
    rejects::<strategy_compiler::ir::TimerSpec>(r#"{"kind": "interval"}"#, "missing field `every`");
}

#[test]
fn duplicate_map_keys_are_rejected_not_last_wins() {
    let err = serde_json::from_str::<strategy_compiler::ir::NamedMap<String>>(r#"{"a": "1", "a": "2"}"#)
        .expect_err("duplicate key must be rejected");
    assert!(err.to_string().contains("duplicate key `a`"), "{err}");
}

#[test]
fn dsl_version_is_a_hard_cutover_gate() {
    rejects::<strategy_compiler::ir::DslVersion>(r#""0.1""#, "hard cutover");
    rejects::<strategy_compiler::ir::DslVersion>(r#""0.2""#, "hard cutover");
    accepts::<strategy_compiler::ir::DslVersion>(r#""0.3""#);
}
