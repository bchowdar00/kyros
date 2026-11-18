//! Layer (b): targeted positive cases with stable snapshot names.
//!
//! The two erratum cases lead the file (written before the unary implementation, per
//! the brief): unary minus on a literal must parse, standalone and after `<=`.

use strategy_compiler::parse::{parse_expr, parse_stmt};

#[test]
fn erratum_unary_minus_on_a_literal() {
    let ast = parse_expr("-1").expect("parses");
    insta::assert_debug_snapshot!("erratum_neg_one", ast);
}

#[test]
fn erratum_comparison_against_negative_decimal() {
    let ast = parse_expr("(signal.spot.last - signal.ptb.price) / signal.ptb.price <= -0.0008")
        .expect("parses");
    insta::assert_debug_snapshot!("erratum_le_negative_decimal", ast);
}

#[test]
fn duration_literals_in_every_unit() {
    for (name, src) in [
        ("duration_ms", "signal.book.age < 500ms"),
        ("duration_s", "signal.book.age < 10s"),
        ("duration_m", "signal.book.age < 5m"),
        ("duration_h", "signal.book.age < 2h"),
    ] {
        let ast = parse_expr(src).expect("parses");
        insta::assert_debug_snapshot!(name, ast);
    }
}

#[test]
fn nested_else_if_chains_flatten() {
    let ast = parse_expr(
        "if signal.d.pct > 0 then 1 else if signal.d.pct < 0 then -1 else if signal.d.vol > 1 then 2 else 0",
    )
    .expect("parses");
    insta::assert_debug_snapshot!("else_if_chain_flattens_to_three_arms", ast);
}

#[test]
fn triggered_takes_a_bare_trigger_name() {
    let ast = parse_expr("triggered(up_rich)").expect("parses");
    insta::assert_debug_snapshot!("triggered_bare_name", ast);
}

#[test]
fn triggered_in_a_state_refined_guard() {
    // The recommended entry-block shape: the trigger holds the signal-side thresholds,
    // the guard adds only state refinement.
    let ast = parse_expr(
        "triggered(home_panic) and state.favorite == 'home' and state.made_threes['away', signal.pbp.quarter] >= 5",
    )
    .expect("parses");
    insta::assert_debug_snapshot!("triggered_state_refined_guard", ast);
}

#[test]
fn deep_clamp_nesting() {
    let ast = parse_expr(
        "clamp(clamp(signal.a.b, 0, 1) + clamp(signal.c.d, 0, 1), 0, clamp(signal.e.f, 0, 1))",
    )
    .expect("parses");
    insta::assert_debug_snapshot!("deep_clamp_nesting", ast);
}

#[test]
fn set_with_two_key_index() {
    let ast = parse_stmt(
        "set state.m[event.team, event.quarter] = state.m[event.team, event.quarter] + 1",
    )
    .expect("parses");
    insta::assert_debug_snapshot!("set_two_key_index", ast);
}

#[test]
fn push_statement_synthetic() {
    // SYNTHETIC: no corpus fixture uses push yet; the spelling is §9's push_stmt
    // production. Replace with a corpus-derived case when a window strategy lands.
    let ast = parse_stmt("push state.big_trades = event.notional").expect("parses");
    insta::assert_debug_snapshot!("push_synthetic", ast);
}

#[test]
fn enum_variants_in_comparison_and_index_position() {
    let comparison = parse_expr("event.result == 'missed'").expect("parses");
    insta::assert_debug_snapshot!("variant_in_comparison", comparison);
    let index = parse_expr("state.miss_count['BOS', signal.pbp.quarter] >= 6").expect("parses");
    insta::assert_debug_snapshot!("variant_in_index_position", index);
}
