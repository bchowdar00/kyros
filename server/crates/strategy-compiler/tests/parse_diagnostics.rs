//! Layer (c): negatives. Each case asserts the exact `ParseErrorCode` AND snapshots the
//! full rendered diagnostic — the rendered output (code + message + caret + help) is
//! part of the contract, consumed by humans and the LLM repair loop alike.

use strategy_compiler::parse::{ParseErrorCode, parse_expr, parse_stmt};

fn check_expr(name: &str, src: &str, code: ParseErrorCode) {
    let err = parse_expr(src).expect_err("must fail to parse");
    assert_eq!(err.code, code, "wrong code for {src:?}: got {:?}", err.code);
    insta::assert_snapshot!(name, err.to_string());
}

fn check_stmt(name: &str, src: &str, code: ParseErrorCode) {
    let err = parse_stmt(src).expect_err("must fail to parse");
    assert_eq!(err.code, code, "wrong code for {src:?}: got {:?}", err.code);
    insta::assert_snapshot!(name, err.to_string());
}

#[test]
fn chained_comparison() {
    check_expr(
        "neg_chained_comparison",
        "0 < signal.a.b < 1",
        ParseErrorCode::ChainedComparison,
    );
}

#[test]
fn missing_else() {
    check_expr(
        "neg_missing_else",
        "if signal.a.b > 0 then 1",
        ParseErrorCode::MissingElse,
    );
}

#[test]
fn bare_identifier() {
    check_expr(
        "neg_bare_identifier",
        "high_water",
        ParseErrorCode::BareIdentifier,
    );
}

#[test]
fn unknown_namespace() {
    check_expr(
        "neg_unknown_namespace",
        "foo.bar",
        ParseErrorCode::UnknownNamespace,
    );
}

#[test]
fn unknown_function_near_a_real_one_suggests() {
    // The rendered snapshot must contain the did-you-mean help.
    check_expr(
        "neg_unknown_function_near",
        "clamq(1, 2, 3)",
        ParseErrorCode::UnknownFunction,
    );
}

#[test]
fn unknown_function_far_from_everything_stays_silent() {
    // The rendered snapshot must NOT contain a did-you-mean help.
    let err = parse_expr("zzzzzz(1)").expect_err("must fail");
    assert_eq!(err.code, ParseErrorCode::UnknownFunction);
    assert!(
        err.help.is_none(),
        "no suggestion should fire at distance > 2"
    );
    insta::assert_snapshot!("neg_unknown_function_far", err.to_string());
}

#[test]
fn wrong_arity_both_directions() {
    check_expr(
        "neg_wrong_arity_too_few",
        "min(1)",
        ParseErrorCode::WrongArity,
    );
    check_expr(
        "neg_wrong_arity_too_many",
        "min(1, 2, 3)",
        ParseErrorCode::WrongArity,
    );
}

#[test]
fn superseded_crossed_steers_to_fire_rearm() {
    check_expr(
        "neg_superseded_crossed",
        "crossed(signal.a.b > 0)",
        ParseErrorCode::SupersededCrossed,
    );
}

#[test]
fn superseded_fired_steers_to_triggered() {
    check_expr(
        "neg_superseded_fired",
        "fired(signal.missed_threes)",
        ParseErrorCode::SupersededFired,
    );
}

#[test]
fn superseded_timer_namespace_steers_to_timer_triggers() {
    check_expr(
        "neg_superseded_timer_namespace",
        "timer.analyze",
        ParseErrorCode::SupersededTimerNamespace,
    );
}

#[test]
fn non_canonical_decimals() {
    check_expr(
        "neg_decimal_exponent",
        "1e5",
        ParseErrorCode::NonCanonicalDecimal,
    );
    check_expr(
        "neg_decimal_leading_dot",
        ".5",
        ParseErrorCode::NonCanonicalDecimal,
    );
    check_expr(
        "neg_decimal_trailing_dot",
        "5.",
        ParseErrorCode::NonCanonicalDecimal,
    );
    check_expr(
        "neg_decimal_leading_plus",
        "+5",
        ParseErrorCode::NonCanonicalDecimal,
    );
    check_expr(
        "neg_decimal_leading_zeros",
        "05",
        ParseErrorCode::NonCanonicalDecimal,
    );
    check_expr(
        "neg_decimal_separators",
        "1_000",
        ParseErrorCode::NonCanonicalDecimal,
    );
}

#[test]
fn alphabet_property_rejections() {
    check_expr(
        "neg_double_quote",
        "event.result == \"missed\"",
        ParseErrorCode::ForbiddenCharacter,
    );
    check_expr(
        "neg_backslash",
        "1 \\ 2",
        ParseErrorCode::ForbiddenCharacter,
    );
    check_expr("neg_newline", "1 +\n2", ParseErrorCode::ForbiddenCharacter);
    check_expr(
        "neg_non_ascii",
        "signal.\u{e9}",
        ParseErrorCode::NonAsciiCharacter,
    );
}

#[test]
fn unary_is_single_application() {
    check_expr("neg_double_minus", "- -1", ParseErrorCode::UnexpectedToken);
    check_expr(
        "neg_double_not",
        "not not signal.a.b",
        ParseErrorCode::UnexpectedToken,
    );
}

#[test]
fn empty_and_whitespace_only() {
    check_expr("neg_empty", "", ParseErrorCode::EmptyExpression);
    check_expr(
        "neg_whitespace_only",
        "   ",
        ParseErrorCode::EmptyExpression,
    );
}

#[test]
fn eof_mid_expression_points_at_the_end() {
    check_expr(
        "neg_eof_after_operator",
        "1 + ",
        ParseErrorCode::UnexpectedEof,
    );
}

#[test]
fn trailing_input_points_at_the_first_leftover_token() {
    check_expr(
        "neg_trailing_input",
        "1 + 2 3",
        ParseErrorCode::TrailingInput,
    );
}

#[test]
fn invalid_lvalues() {
    check_stmt(
        "neg_lvalue_platform",
        "set platform.x = 1",
        ParseErrorCode::InvalidLvalue,
    );
    check_stmt(
        "neg_lvalue_event",
        "set event.y = 1",
        ParseErrorCode::InvalidLvalue,
    );
}

#[test]
fn lookup_first_arg_must_be_a_table_ref() {
    check_expr(
        "neg_lookup_non_table",
        "lookup(state.x, 1)",
        ParseErrorCode::TableRefOutsideLookup,
    );
}

#[test]
fn triggered_arg_must_be_a_bare_name() {
    check_expr(
        "neg_triggered_namespaced_arg",
        "triggered(signal.book)",
        ParseErrorCode::TriggeredArgNotBareName,
    );
    check_expr(
        "neg_triggered_missing_arg",
        "triggered()",
        ParseErrorCode::TriggeredArgNotBareName,
    );
    check_expr(
        "neg_triggered_two_args",
        "triggered(a, b)",
        ParseErrorCode::TriggeredArgNotBareName,
    );
}
