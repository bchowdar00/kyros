use proptest::prelude::*;
use signals_core::parse_decimal_text;

/// Independent oracle: re-derives acceptance from the documented grammar and
/// bounds directly, never consulting `parse_decimal_text` to define truth.
fn oracle_accepts(s: &str) -> bool {
    if s.is_empty() || s.len() > 64 {
        return false;
    }
    let body = s.strip_prefix('-').unwrap_or(s);
    let (integer, fraction) = match body.split_once('.') {
        Some((integer, fraction)) => (integer, Some(fraction)),
        None => (body, None),
    };
    let is_digits = |text: &str| !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit());
    if !is_digits(integer) {
        return false;
    }
    let scale = match fraction {
        None => 0,
        Some(fraction) => {
            if !is_digits(fraction) {
                return false;
            }
            fraction.len()
        }
    };
    if scale > 28 {
        return false;
    }
    let mut coefficient = String::with_capacity(integer.len() + scale);
    coefficient.push_str(integer);
    if let Some(fraction) = fraction {
        coefficient.push_str(fraction);
    }
    coefficient_fits_96_bits(&coefficient)
}

/// Independent oracle helper: compares the coefficient digits against 2^96 by
/// length and lexical order, avoiding any 128-bit type the code might share.
fn coefficient_fits_96_bits(digits: &str) -> bool {
    const LIMIT: &str = "79228162514264337593543950336"; // 2^96
    let trimmed = digits.trim_start_matches('0');
    trimmed.len() < LIMIT.len() || (trimmed.len() == LIMIT.len() && trimmed < LIMIT)
}

prop_compose! {
    fn canonical_decimal()(
        negative in any::<bool>(),
        integer in "0|[1-9][0-9]{0,13}",
        fraction in proptest::option::of("[0-9]{1,14}"),
    ) -> String {
        let is_zero = integer.bytes().all(|byte| byte == b'0')
            && fraction
                .as_deref()
                .is_none_or(|f| f.bytes().all(|byte| byte == b'0'));
        let mut text = String::new();
        if negative && !is_zero {
            text.push('-');
        }
        text.push_str(&integer);
        if let Some(fraction) = &fraction {
            text.push('.');
            text.push_str(fraction);
        }
        text
    }
}

fn maybe_decimal_text() -> impl Strategy<Value = String> {
    prop_oneof![
        ".*",
        "-?[0-9]{1,40}(\\.[0-9]{1,40})?",
        "[+.\\-0-9eE_]{0,20}",
        proptest::collection::vec(any::<u8>(), 0..48)
            .prop_map(|bytes| String::from_utf8_lossy(&bytes).into_owned()),
    ]
}

proptest! {
    #[test]
    fn valid_decimal_text_round_trips(text in canonical_decimal()) {
        let first = parse_decimal_text(&text).expect("canonical decimal must be accepted");
        let formatted = first.to_string();
        prop_assert_eq!(&text, &formatted);
        let reparsed = parse_decimal_text(&formatted).expect("re-formatted decimal must parse");
        prop_assert_eq!(first, reparsed);
        prop_assert_eq!(formatted, reparsed.to_string());
    }

    #[test]
    fn parse_never_panics_on_arbitrary_unicode(s in maybe_decimal_text()) {
        let outcome = parse_decimal_text(&s);
        prop_assert!(outcome.is_ok() || outcome.is_err());
    }

    #[test]
    fn parse_never_panics_on_arbitrary_bytes(
        bytes in proptest::collection::vec(any::<u8>(), 0..64)
    ) {
        let text = String::from_utf8_lossy(&bytes);
        let outcome = parse_decimal_text(&text);
        prop_assert!(outcome.is_ok() || outcome.is_err());
    }

    // Everything the strict oracle accepts, the code also accepts: the literal
    // coefficient fits 96 bits at scale <= 28, so `Decimal::from_str` never
    // rounds. This sound direction holds and stays live.
    #[test]
    fn oracle_acceptance_implies_code_acceptance(s in maybe_decimal_text()) {
        if oracle_accepts(&s) {
            prop_assert!(parse_decimal_text(&s).is_ok(), "input = {:?}", s);
        }
    }

    // `parse_decimal_text` now enforces literal exact representability (a 96-bit
    // coefficient at scale <= 28) before `Decimal::from_str`, so acceptance
    // equals the independent oracle. Regression seed
    // "100000000000000000000.000000000" is retained to guard the fix.
    #[test]
    fn acceptance_matches_independent_oracle(s in maybe_decimal_text()) {
        let accepted = parse_decimal_text(&s).is_ok();
        prop_assert_eq!(accepted, oracle_accepts(&s), "input = {:?}", s);
    }
}

#[test]
fn coefficient_boundary_at_two_pow_ninety_six() {
    // 2^96 - 1 is the largest exactly representable coefficient; 2^96 must reject.
    assert!(parse_decimal_text("79228162514264337593543950335").is_ok());
    assert!(parse_decimal_text("79228162514264337593543950336").is_err());
    // Same boundary split across the integer/fraction join.
    assert!(parse_decimal_text("7922816251426433759354395033.5").is_ok());
    assert!(parse_decimal_text("7922816251426433759354395033.6").is_err());
    // Leading zeros do not change the magnitude comparison.
    assert!(parse_decimal_text("0079228162514264337593543950335").is_ok());
    // Scale bounds: 28 fraction digits accept, 29 reject.
    assert!(parse_decimal_text("0.0000000000000000000000000001").is_ok());
    assert!(parse_decimal_text("0.00000000000000000000000000001").is_err());
}
