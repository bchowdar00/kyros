//! Exact base-10 decimal admission for provider JSON tokens.
//!
//! Provider numbers reach the crate as raw JSON text. This module keeps them in
//! text form wherever it can and never routes a value through floating point: a
//! bare token is size-checked and syntax-validated as-is, a quoted token is
//! unquoted first, and only scientific notation is expanded through `Decimal`,
//! and then only after a textual exactness check proves the parser rounded
//! nothing away (SIG-045). Every failure is an `AdapterError::InvalidPayload`
//! naming the offending field, so a lossy provider value fails closed instead
//! of entering a normalized record.

use rust_decimal::Decimal;
use serde_json::value::RawValue;
use signals_core::{MAX_DECIMAL_TEXT_BYTES, parse_decimal_text};

use crate::AdapterError;

/// Admit a non-negative decimal field, returning its exact text.
///
/// Rejects a negative value, an over-long token, a malformed JSON string, and
/// any scientific token that cannot be expanded without precision loss.
pub(crate) fn decimal_string(raw: &RawValue, field: &str) -> Result<String, AdapterError> {
    decimal_string_with_sign(raw, field, false)
}

/// Admit a decimal field that may carry a leading `-`, returning its exact
/// text. Identical to [`decimal_string`] except that a negative value passes.
pub(crate) fn signed_decimal_string(raw: &RawValue, field: &str) -> Result<String, AdapterError> {
    decimal_string_with_sign(raw, field, true)
}

/// Upper bound on textual point-shift padding. Any exactly representable value
/// has at most 29 integer and 28 fraction digits, so an expansion that would
/// pad beyond this cannot be exact and is treated as a mismatch.
const MAX_EXPANSION_DIGITS: usize = 1024;

/// Verify that `produced` preserves the exact value written in `raw_token`.
///
/// The provider token (plain or scientific) is expanded to exact integer and
/// fraction digit strings by a textual decimal-point shift — no floating point
/// and no `Decimal` arithmetic — then both sides are reduced to a sign-free
/// canonical form (leading integer zeros and trailing fraction zeros removed)
/// and compared as text. Any silent rounding by `Decimal::from_scientific` or
/// `Decimal::from_str` changes the produced digits and fails the comparison, so
/// adapters reject precision loss rather than accepting a rounded value.
/// (SIG-045.)
pub(crate) fn value_is_exact(raw_token: &str, produced: &Decimal) -> bool {
    match (
        canonical_magnitude(raw_token),
        canonical_magnitude(&produced.to_string()),
    ) {
        (Some(raw), Some(produced)) => raw == produced,
        _ => false,
    }
}

/// Reduce a decimal or scientific token to its sign-free canonical magnitude:
/// exact integer and fraction digit strings with insignificant zeros removed.
/// Returns `None` when the token cannot be expanded within the exact envelope.
fn canonical_magnitude(token: &str) -> Option<String> {
    let (integer, fraction) = expanded_digits(token)?;
    let integer = integer.trim_start_matches('0');
    let fraction = fraction.trim_end_matches('0');
    Some(format!("{integer}.{fraction}"))
}

/// Textually expand a provider decimal token (plain or scientific) into exact
/// integer and fraction digit strings, dropping the sign. Returns `None` when
/// the exponent cannot be parsed or the expansion would exceed the exact
/// decimal envelope, both of which mean the token is not exactly representable.
fn expanded_digits(token: &str) -> Option<(String, String)> {
    let unsigned = token
        .strip_prefix('-')
        .or_else(|| token.strip_prefix('+'))
        .unwrap_or(token);
    let (mantissa, exponent) = match unsigned.split_once(['e', 'E']) {
        Some((mantissa, exponent)) => (mantissa, exponent.parse::<i32>().ok()?),
        None => (unsigned, 0),
    };
    let (integer_digits, fraction_digits) = match mantissa.split_once('.') {
        Some((integer, fraction)) => (integer, fraction),
        None => (mantissa, ""),
    };
    let digits: String = [integer_digits, fraction_digits].concat();
    let point = i64::try_from(integer_digits.len()).ok()? + i64::from(exponent);
    let padding = if point <= 0 {
        point.unsigned_abs() as usize
    } else {
        (point as usize).saturating_sub(digits.len())
    };
    if padding > MAX_EXPANSION_DIGITS {
        return None;
    }
    if point <= 0 {
        Some((String::new(), format!("{}{digits}", "0".repeat(padding))))
    } else if point as usize >= digits.len() {
        Some((format!("{digits}{}", "0".repeat(padding)), String::new()))
    } else {
        let (integer, fraction) = digits.split_at(point as usize);
        Some((integer.to_owned(), fraction.to_owned()))
    }
}

/// Shared admission path for [`decimal_string`] and [`signed_decimal_string`].
///
/// Bounds the encoded token at `MAX_DECIMAL_TEXT_BYTES` (plus the two quote
/// bytes when the token is a JSON string), unquotes a string token, expands a
/// scientific token through `Decimal` only after [`value_is_exact`] confirms no
/// rounding occurred, and passes any other token through untouched so its
/// original digits survive verbatim. `allow_negative` selects the signed or
/// non-negative syntax check applied to the result.
fn decimal_string_with_sign(
    raw: &RawValue,
    field: &str,
    allow_negative: bool,
) -> Result<String, AdapterError> {
    let token = raw.get();
    let maximum_token_bytes = if token.starts_with('"') {
        MAX_DECIMAL_TEXT_BYTES.saturating_add(2)
    } else {
        MAX_DECIMAL_TEXT_BYTES
    };
    if token.len() > maximum_token_bytes {
        return Err(AdapterError::InvalidPayload(format!(
            "{field} encoded value exceeds {maximum_token_bytes} bytes"
        )));
    }
    let value = if token.starts_with('"') {
        serde_json::from_str::<String>(token).map_err(|error| {
            AdapterError::InvalidPayload(format!("{field} is not a valid string: {error}"))
        })?
    } else if token.contains(['e', 'E']) {
        let value = Decimal::from_scientific(token).map_err(|error| {
            AdapterError::InvalidPayload(format!(
                "{field} is not representable as an exact decimal: {error}"
            ))
        })?;
        if !value_is_exact(token, &value) {
            return Err(AdapterError::InvalidPayload(format!(
                "{field} is not representable as an exact decimal"
            )));
        }
        if !allow_negative && value.is_sign_negative() {
            return Err(AdapterError::InvalidPayload(format!(
                "{field} is not a non-negative base-10 decimal"
            )));
        }
        value.to_string()
    } else {
        token.to_owned()
    };
    if allow_negative {
        validate_signed_decimal(&value, field)?;
    } else {
        validate_decimal(&value, field)?;
    }
    Ok(value)
}

/// Validate `value` as non-negative base-10 decimal text and return the parsed
/// `Decimal`. Errors on an over-long, negative, or non-canonical token (a
/// leading `+`, a bare `.5` or `1.`, an exponent, or digit separators).
pub(crate) fn validate_decimal(value: &str, field: &str) -> Result<Decimal, AdapterError> {
    let parsed = validate_decimal_syntax(value, field, false)?;
    if parsed.is_sign_negative() {
        return Err(AdapterError::InvalidPayload(format!(
            "{field} is not a non-negative base-10 decimal"
        )));
    }
    Ok(parsed)
}

/// Validate `value` as base-10 decimal text that may be negative, returning the
/// parsed `Decimal`.
pub(crate) fn validate_signed_decimal(value: &str, field: &str) -> Result<Decimal, AdapterError> {
    validate_decimal_syntax(value, field, true)
}

/// Shared syntax check: bound the text at `MAX_DECIMAL_TEXT_BYTES`, reject a
/// leading `-` unless `allow_negative`, then delegate to
/// `signals_core::parse_decimal_text` for canonical base-10 form.
fn validate_decimal_syntax(
    value: &str,
    field: &str,
    allow_negative: bool,
) -> Result<Decimal, AdapterError> {
    if value.len() > MAX_DECIMAL_TEXT_BYTES {
        return Err(AdapterError::InvalidPayload(format!(
            "{field} exceeds {MAX_DECIMAL_TEXT_BYTES} bytes"
        )));
    }
    if !allow_negative && value.starts_with('-') {
        return Err(AdapterError::InvalidPayload(format!(
            "{field} is not a non-negative base-10 decimal"
        )));
    }
    parse_decimal_text(value).map_err(|error| {
        AdapterError::InvalidPayload(format!("{field} is not valid decimal text: {error}"))
    })
}

/// Fold a decimal's significant digits into `hasher` for a scale-independent
/// checksum.
///
/// Leading zeros and the decimal point are dropped and the remaining digit
/// segments are hashed in order, so `0.00000100` and `123.4500` contribute the
/// same bytes as the concatenation `1001234500`. Trailing zeros are significant
/// and are retained; a token with no significant digit contributes nothing.
pub(crate) fn update_checksum(hasher: &mut crc32fast::Hasher, value: &str) {
    let bytes = value.as_bytes();
    let first_significant = bytes.iter().position(|byte| *byte != b'0' && *byte != b'.');
    let Some(first_significant) = first_significant else {
        return;
    };

    for segment in bytes[first_significant..].split(|byte| *byte == b'.') {
        hasher.update(segment);
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use proptest::prelude::*;
    use rust_decimal::Decimal;
    use serde_json::value::RawValue;

    use super::{
        decimal_string, signed_decimal_string, update_checksum, validate_decimal, value_is_exact,
    };

    #[test]
    fn checksum_stream_preserves_trailing_zeros() {
        let mut expected = crc32fast::Hasher::new();
        expected.update(b"1001234500");
        let mut actual = crc32fast::Hasher::new();
        update_checksum(&mut actual, "0.00000100");
        update_checksum(&mut actual, "123.4500");
        assert_eq!(actual.finalize(), expected.finalize());
    }

    #[test]
    fn decimal_validation_rejects_float_shortcuts() {
        for invalid in ["+1", ".5", "1.", "1e-8", "1E-8", "1_0"] {
            assert!(validate_decimal(invalid, "price").is_err());
        }
        assert!(validate_decimal("-1", "price").is_err());
        assert!(validate_decimal("1.25", "price").is_ok());
    }

    #[test]
    fn provider_scientific_notation_expands_before_contract_output() {
        let raw = RawValue::from_string("1E-8".to_owned()).expect("valid JSON number");
        assert_eq!(
            decimal_string(&raw, "quantity_increment").expect("exact expansion"),
            "0.00000001"
        );
    }

    #[test]
    fn oversized_scientific_token_is_rejected_before_conversion() {
        let raw =
            RawValue::from_string(format!("1E{}", "9".repeat(64))).expect("valid JSON number");
        assert!(decimal_string(&raw, "quantity_increment").is_err());
    }

    #[test]
    fn signed_market_change_accepts_negative_exact_decimal() {
        let raw = RawValue::from_string("-72.9".to_owned()).expect("valid JSON number");
        assert_eq!(
            signed_decimal_string(&raw, "change").expect("valid signed decimal"),
            "-72.9"
        );
        assert!(decimal_string(&raw, "price").is_err());
    }

    #[test]
    fn value_exact_check_accepts_lossless_forms() {
        // Scientific normal, trailing zeros, leading zeros, and the exact 96-bit
        // boundary coefficient all preserve their value.
        assert!(value_is_exact(
            "1E-8",
            &Decimal::from_scientific("1E-8").expect("valid scientific")
        ));
        assert!(value_is_exact("1.50", &Decimal::from_str("1.5").unwrap()));
        assert!(value_is_exact("007", &Decimal::from_str("7").unwrap()));
        assert!(value_is_exact(
            "0.0000000000000000000000000001",
            &Decimal::from_str("0.0000000000000000000000000001").unwrap()
        ));
        assert!(value_is_exact(
            "79228162514264337593543950335",
            &Decimal::from_str("79228162514264337593543950335").unwrap()
        ));
    }

    #[test]
    fn value_exact_check_rejects_precision_loss() {
        // A produced value that dropped digits from the raw token is not exact.
        assert!(!value_is_exact(
            "1.00000000000000000000000000001",
            &Decimal::from_str("1").unwrap()
        ));
        // An exponent shifting a nonzero digit outside scale 28 is not exact.
        assert!(!value_is_exact("1E-30", &Decimal::ZERO));
    }

    #[test]
    fn scientific_path_accepts_exact_values() {
        for (token, expected) in [
            ("1E-8", "0.00000001"),
            ("1E-28", "0.0000000000000000000000000001"),
        ] {
            let raw = RawValue::from_string(token.to_owned()).expect("valid JSON number");
            assert_eq!(
                decimal_string(&raw, "quantity_increment").expect("exact scientific"),
                expected
            );
        }
        // A scientific form with trailing mantissa zeros is exact.
        let raw = RawValue::from_string("1.50E1".to_owned()).expect("valid JSON number");
        let accepted = decimal_string(&raw, "quantity_increment").expect("exact scientific");
        assert_eq!(
            Decimal::from_str(&accepted).unwrap(),
            Decimal::from_str("15").unwrap()
        );
    }

    #[test]
    fn scientific_path_rejects_inexact_values() {
        for token in [
            // 29 significant digits via exponent: coefficient exceeds 2^96 - 1.
            "9.9999999999999999999999999999E0",
            // Scale beyond 28 forces the parser to round away digits.
            "1.000000000000000000000000000001E0",
            // Exponent underflow rounds a nonzero value to zero.
            "1E-30",
            // Scale 29 is one past the exact bound.
            "1E-29",
            // Exponent overflow beyond the representable range.
            "1E40",
        ] {
            let raw = RawValue::from_string(token.to_owned()).expect("valid JSON number");
            assert!(
                decimal_string(&raw, "quantity_increment").is_err(),
                "expected {token} to be rejected"
            );
        }
    }

    proptest! {
        // In-range decimals accept and round-trip exactly, in both plain and
        // scientific renderings of the same value.
        #[test]
        fn in_range_decimals_accept_and_round_trip(
            coefficient in 0i128..=79_228_162_514_264_337_593_543_950_335i128,
            scale in 0u32..=28,
        ) {
            let value = Decimal::try_from_i128_with_scale(coefficient, scale)
                .expect("coefficient within 96 bits and scale <= 28");

            let plain = value.to_string();
            prop_assert!(value_is_exact(&plain, &value));
            let raw = RawValue::from_string(plain).expect("valid JSON number");
            let accepted = decimal_string(&raw, "price").expect("in-range plain decimal");
            prop_assert_eq!(Decimal::from_str(&accepted).unwrap(), value);

            let scientific = if value.scale() == 0 {
                format!("{}E0", value.mantissa())
            } else {
                format!("{}E-{}", value.mantissa(), value.scale())
            };
            prop_assert!(value_is_exact(&scientific, &value));
            let raw = RawValue::from_string(scientific).expect("valid JSON number");
            let accepted = decimal_string(&raw, "price").expect("in-range scientific decimal");
            prop_assert_eq!(Decimal::from_str(&accepted).unwrap(), value);
        }

        // Over-precision tokens (a nonzero digit at scale 29) are never accepted,
        // and the exact check reports the loss whenever the parser rounds.
        #[test]
        fn over_precision_scientific_tokens_reject(
            leading in 1u8..=9,
            excess in "[0-9]{28}[1-9]",
        ) {
            let token = format!("{leading}.{excess}E0");
            let raw = RawValue::from_string(token.clone()).expect("valid JSON number");
            prop_assert!(decimal_string(&raw, "price").is_err());
            if let Ok(rounded) = Decimal::from_scientific(&token) {
                prop_assert!(!value_is_exact(&token, &rounded));
            }
        }
    }
}
