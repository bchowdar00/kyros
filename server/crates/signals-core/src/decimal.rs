use std::str::FromStr;

use rust_decimal::Decimal;
use thiserror::Error;

/// Maximum accepted decimal text length at the shared contract boundary.
pub const MAX_DECIMAL_TEXT_BYTES: usize = 64;

/// Maximum accepted fractional-digit count, i.e. the exact-decimal scale bound.
const MAX_DECIMAL_SCALE_DIGITS: usize = 28;

/// Largest representable 96-bit coefficient, `2^96 - 1`, as a digit string.
const MAX_DECIMAL_COEFFICIENT: &str = "79228162514264337593543950335";

/// Parse the one decimal-text grammar accepted by source and signal contracts.
///
/// Accepted values are an optional `-`, one or more ASCII integer digits, and
/// an optional fractional part containing a dot followed by one or more ASCII
/// digits. Exponents, separators, a leading `+`, `.5`, and `1.` are rejected.
///
/// The contract requires literal exact representability (CONTRACTS.md
/// field-types table, SIG-018): the value is rejected unless its fractional
/// part holds at most 28 digits and its literal coefficient fits a 96-bit
/// integer, so no in-range-but-unrepresentable input is silently rounded.
pub fn parse_decimal_text(value: &str) -> Result<Decimal, DecimalTextError> {
    if value.is_empty() || value.len() > MAX_DECIMAL_TEXT_BYTES {
        return Err(DecimalTextError::Length);
    }

    let unsigned = value.strip_prefix('-').unwrap_or(value);
    let mut parts = unsigned.split('.');
    let integer = parts.next().unwrap_or_default();
    let fraction = parts.next();
    if parts.next().is_some()
        || integer.is_empty()
        || !integer.bytes().all(|byte| byte.is_ascii_digit())
        || fraction.is_some_and(|digits| {
            digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit())
        })
    {
        return Err(DecimalTextError::Syntax);
    }

    if fraction.is_some_and(|digits| digits.len() > MAX_DECIMAL_SCALE_DIGITS)
        || !coefficient_within_96_bits(integer, fraction.unwrap_or(""))
    {
        return Err(DecimalTextError::Range);
    }

    Decimal::from_str(value).map_err(|_| DecimalTextError::Range)
}

/// Whether the literal coefficient (integer digits followed by fraction digits,
/// leading zeros dropped) is at most `2^96 - 1`, compared by length then
/// lexically across the two segments so no big-integer type is required.
fn coefficient_within_96_bits(integer: &str, fraction: &str) -> bool {
    let integer = integer.as_bytes();
    let fraction = fraction.as_bytes();
    let integer_leading = integer.iter().take_while(|byte| **byte == b'0').count();
    let (integer_significant, fraction_significant) = if integer_leading == integer.len() {
        let fraction_leading = fraction.iter().take_while(|byte| **byte == b'0').count();
        (&integer[integer.len()..], &fraction[fraction_leading..])
    } else {
        (&integer[integer_leading..], fraction)
    };
    let significant = integer_significant.len() + fraction_significant.len();
    let limit = MAX_DECIMAL_COEFFICIENT.as_bytes();
    if significant != limit.len() {
        return significant < limit.len();
    }
    for (index, limit_digit) in limit.iter().enumerate() {
        let digit = if index < integer_significant.len() {
            integer_significant[index]
        } else {
            fraction_significant[index - integer_significant.len()]
        };
        if digit != *limit_digit {
            return digit < *limit_digit;
        }
    }
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum DecimalTextError {
    #[error("decimal text must contain 1 to {MAX_DECIMAL_TEXT_BYTES} bytes")]
    Length,
    #[error("decimal text must match -?[0-9]+(?:\\.[0-9]+)?")]
    Syntax,
    #[error("decimal text is outside the exact decimal range")]
    Range,
}

#[cfg(test)]
mod tests {
    use super::parse_decimal_text;

    #[test]
    fn accepts_plain_signed_base_ten_text() {
        for value in ["0", "001", "1.0", "-0.25", "123456789.000100"] {
            assert!(
                parse_decimal_text(value).is_ok(),
                "expected {value} to pass"
            );
        }
    }

    #[test]
    fn rejects_non_contract_float_shortcuts() {
        for value in ["", "-", "+1", ".5", "1.", "1e-8", "1E-8", "1_0", "1..0"] {
            assert!(
                parse_decimal_text(value).is_err(),
                "expected {value:?} to fail"
            );
        }
    }
}
