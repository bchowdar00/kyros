//! Exact comparison of canonical decimal strings. The validator's ONE numeric helper —
//! no float appears anywhere (a float rounding a band bound or a table key would be a
//! determinism bug).
//!
//! Inputs are canonical by construction (`DecimalStr`/`SignedDecimalStr` enforce the
//! form at deserialization): optional leading `-` (never on zero), no exponent, no
//! leading zeros on the integer part, no trailing zeros in the fraction, no bare or
//! trailing dot. Canonical form makes exact rational comparison a string exercise:
//! sign, then integer-part length, then integer part lexicographically, then fraction
//! parts lexicographically with implicit right-padding by `0`.

use std::cmp::Ordering;

/// Compare two canonical (optionally signed) decimal strings as exact rationals.
pub fn cmp_decimal(a: &str, b: &str) -> Ordering {
    let (a_neg, a_body) = split_sign(a);
    let (b_neg, b_body) = split_sign(b);
    match (a_neg, b_neg) {
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        (false, false) => cmp_magnitude(a_body, b_body),
        // Both negative: the larger magnitude is the smaller number.
        (true, true) => cmp_magnitude(b_body, a_body),
    }
}

fn split_sign(s: &str) -> (bool, &str) {
    match s.strip_prefix('-') {
        Some(body) => (true, body),
        None => (false, s),
    }
}

/// Compare two unsigned canonical decimal bodies.
fn cmp_magnitude(a: &str, b: &str) -> Ordering {
    let (a_int, a_frac) = split_point(a);
    let (b_int, b_frac) = split_point(b);
    // No leading zeros (canonical), so a longer integer part is a larger number.
    let ints = a_int.len().cmp(&b_int.len()).then_with(|| a_int.cmp(b_int));
    if ints != Ordering::Equal {
        return ints;
    }
    // Equal integer parts: compare fractions digit-by-digit; the shorter fraction is
    // implicitly right-padded with zeros.
    let max = a_frac.len().max(b_frac.len());
    for i in 0..max {
        let da = a_frac.as_bytes().get(i).copied().unwrap_or(b'0');
        let db = b_frac.as_bytes().get(i).copied().unwrap_or(b'0');
        match da.cmp(&db) {
            Ordering::Equal => {}
            other => return other,
        }
    }
    Ordering::Equal
}

fn split_point(s: &str) -> (&str, &str) {
    match s.split_once('.') {
        Some((int, frac)) => (int, frac),
        None => (s, ""),
    }
}

// The one sanctioned inline test site (per the validator design): everything else
// lives in `tests/`.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integer_length_dominates() {
        assert_eq!(cmp_decimal("100", "99"), Ordering::Greater);
        assert_eq!(cmp_decimal("9", "10"), Ordering::Less);
    }

    #[test]
    fn fractions_pad_with_zeros() {
        assert_eq!(cmp_decimal("0.4", "0.40000"), Ordering::Equal);
        assert_eq!(cmp_decimal("0.4", "0.41"), Ordering::Less);
        assert_eq!(cmp_decimal("0.35", "0.4"), Ordering::Less);
        assert_eq!(cmp_decimal("1.5", "1.05"), Ordering::Greater);
    }

    #[test]
    fn integers_against_fractions() {
        assert_eq!(cmp_decimal("1", "1.0"), Ordering::Equal);
        assert_eq!(cmp_decimal("1", "0.999"), Ordering::Greater);
        assert_eq!(cmp_decimal("2", "2.001"), Ordering::Less);
    }

    #[test]
    fn signs() {
        assert_eq!(cmp_decimal("-1", "1"), Ordering::Less);
        assert_eq!(cmp_decimal("1", "-1"), Ordering::Greater);
        assert_eq!(cmp_decimal("-2", "-10"), Ordering::Greater);
        assert_eq!(cmp_decimal("-0.5", "-0.4"), Ordering::Less);
        assert_eq!(cmp_decimal("0", "-1"), Ordering::Greater);
    }

    #[test]
    fn equality() {
        assert_eq!(cmp_decimal("0", "0"), Ordering::Equal);
        assert_eq!(cmp_decimal("-3.25", "-3.25"), Ordering::Equal);
    }
}
