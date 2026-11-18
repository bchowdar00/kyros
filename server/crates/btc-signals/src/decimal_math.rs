//! Pinned exact-decimal transcendental kernel for the fair-probability model
//! (ADR 0010 / SIG-038).
//!
//! `rust_decimal` provides no `ln`/`exp`/`sqrt`, and the `maths` feature is
//! deliberately not enabled: its approximation internals are third-party and not
//! pinnable across versions. This module implements the four transcendentals the
//! processor needs with fixed algorithms, fixed iteration counts, and documented
//! error bounds, so every value is deterministic to the byte and replay-safe. No
//! binary floating point enters any path.
//!
//! # Precision model
//!
//! `Decimal` carries at most 28 significant digits (scale <= 28), giving an
//! absolute floor near 1e-28 for the magnitudes handled here. Every stated error
//! bound sits well above that floor and is reachable within the type.
//!
//! # Arithmetic safety
//!
//! Intermediate magnitudes on the documented domains are bounded far inside
//! `Decimal`'s range (|value| < ~1e3 everywhere: roots <= 1e3, ln outputs
//! <= ~1.39, exponentials <= 1, CDF terms <= 1), and every divisor is provably
//! nonzero, so the plain `+ - * /` operators cannot overflow or divide by zero on
//! these inputs. Arithmetic is therefore explicitly-reasoned unchecked rather than
//! `checked_*`; the error type carries domain violations only, per the SIG-038
//! brief. Off-domain inputs are rejected before any arithmetic runs.

// The fair-probability processor that consumes this kernel is wired in a later
// round (per the SIG-038 scope split); until then every item is reached only from
// the in-module tests, so the non-test build sees them as unused.

use rust_decimal::Decimal;
use thiserror::Error;

/// Domain violations for the pinned decimal kernel. Arithmetic cannot fail on the
/// documented domains (see module docs), so no arithmetic variant exists.
#[derive(Debug, Error, PartialEq, Eq)]
pub(crate) enum DecimalMathError {
    #[error("sqrt_fixed requires x >= 0, got {0}")]
    SqrtNegative(Decimal),
    #[error("ln_fixed domain is [0.25, 4.0], got {0}")]
    LnOutOfDomain(Decimal),
    #[error("exp_neg_fixed domain is [0, 40], got {0}")]
    ExpNegOutOfDomain(Decimal),
}

// --- pinned constants (byte-for-byte; see ADR 0010 and the SIG-038 brief) ---

/// e^-1 to 28 significant digits (true value ...701614608..., rounded half-even).
/// Range-reduction base for `exp_neg_fixed`.
fn e_neg_1() -> Decimal {
    Decimal::from_i128_with_scale(3_678_794_411_714_423_215_955_237_702, 28)
}

/// sqrt(2*pi) to 28 significant digits; pinned so `phi_cdf` needs no runtime sqrt.
fn sqrt_2pi() -> Decimal {
    Decimal::from_i128_with_scale(2_506_628_274_631_000_502_415_765_285, 27)
}

// Abramowitz-Stegun 7.1.26 coefficients, copied from ADR 0010.
fn as_p() -> Decimal {
    Decimal::from_i128_with_scale(2_316_419, 7)
}
fn as_b1() -> Decimal {
    Decimal::from_i128_with_scale(319_381_530, 9)
}
fn as_b2() -> Decimal {
    Decimal::from_i128_with_scale(-356_563_782, 9)
}
fn as_b3() -> Decimal {
    Decimal::from_i128_with_scale(1_781_477_937, 9)
}
fn as_b4() -> Decimal {
    Decimal::from_i128_with_scale(-1_821_255_978, 9)
}
fn as_b5() -> Decimal {
    Decimal::from_i128_with_scale(1_330_274_429, 9)
}

/// `phi_cdf` returns exactly 1 at or beyond this abscissa: the A-S 7.1.26 value at
/// 8.3 is 0.99999999999999994734...,  which rounds to 1 at the eight half-even
/// decimals ADR 0010 publishes, so evaluating the tail adds no information.
fn saturation() -> Decimal {
    Decimal::from_i128_with_scale(83, 1)
}

// --- fixed iteration/term counts (see per-function error analysis) ---

/// Newton-Raphson steps for `sqrt_fixed`. The scale-halving seed lands within a
/// factor of 10 of the root; quadratic convergence then reaches the `Decimal`
/// floor by step ~8, so 10 steps hold the error <= 1e-12 on [1e-9, 1e6] with
/// margin. Fixed count (no data-dependent early exit) preserves byte determinism.
const SQRT_ITERS: u32 = 10;

/// atanh-series terms for `ln_fixed`. With |z| <= 0.6 on [0.25, 4.0] the first
/// omitted term is 2*z^81/81 <= 2.6e-20, below the documented 1e-18 bound.
const LN_TERMS: u32 = 40;

/// Taylor terms for `exp(-r)`, r in [0, 1). The first omitted term is below
/// 1/22! ~= 9e-22.
const EXP_TERMS: u32 = 22;

/// Square root by Newton-Raphson in exact decimal arithmetic.
///
/// `x == 0` returns 0; `x < 0` is a domain error. The seed is a pure power of ten
/// obtained by halving the decimal magnitude of `x` (floor(log10 x) via digit
/// count, no floats), placing it within a factor of 10 of the true root. Error is
/// <= 1e-12 on [1e-9, 1e6]; outside that band the function still converges and
/// never panics, but the bound is not guaranteed.
pub(crate) fn sqrt_fixed(x: Decimal) -> Result<Decimal, DecimalMathError> {
    if x.is_sign_negative() && !x.is_zero() {
        return Err(DecimalMathError::SqrtNegative(x));
    }
    if x.is_zero() {
        return Ok(Decimal::ZERO);
    }

    let mut guess = sqrt_seed(x);
    let two = Decimal::from(2);
    // Fixed iteration count: x_{n+1} = (x_n + x/x_n) / 2. Divisor stays positive.
    for _ in 0..SQRT_ITERS {
        guess = (guess + x / guess) / two;
    }
    Ok(guess)
}

/// Power-of-ten seed within a factor of 10 of sqrt(x), for x > 0. Computes
/// e = floor(log10 x) from the mantissa digit count and scale, then returns
/// 10^(e div_euclid 2). No floating point.
fn sqrt_seed(x: Decimal) -> Decimal {
    let mantissa = x.mantissa().unsigned_abs();
    let digits = decimal_digits(mantissa); // number of base-10 digits of the mantissa
    // x = mantissa * 10^(-scale); its order of magnitude is digits - 1 - scale.
    let exponent = digits as i64 - 1 - x.scale() as i64;
    let half = exponent.div_euclid(2);
    if half >= 0 {
        // 10^half; half <= 3 on-domain, so 10^half fits an i128 exactly.
        Decimal::from(10_i128.pow(half as u32))
    } else {
        // 10^half = 1 * 10^(-half) as a scaled Decimal.
        Decimal::from_i128_with_scale(1, (-half) as u32)
    }
}

fn decimal_digits(mut m: u128) -> u32 {
    if m == 0 {
        return 1;
    }
    let mut count = 0;
    while m > 0 {
        m /= 10;
        count += 1;
    }
    count
}

/// Natural logarithm on the model domain [0.25, 4.0] via the atanh series
/// ln(x) = 2 * sum_{k=0..LN_TERMS-1} z^(2k+1)/(2k+1), z = (x-1)/(x+1).
///
/// S/K beyond +/-75% is out-of-model, so inputs outside [0.25, 4.0] are domain
/// errors rather than extrapolated. On-domain |z| <= 0.6 bounds the truncation
/// error below 1e-18 (see `LN_TERMS`). `ln(1) == 0` exactly; the divisor `x + 1`
/// is >= 1.25.
pub(crate) fn ln_fixed(x: Decimal) -> Result<Decimal, DecimalMathError> {
    let lo = Decimal::from_i128_with_scale(25, 2); // 0.25
    let hi = Decimal::from(4);
    if x < lo || x > hi {
        return Err(DecimalMathError::LnOutOfDomain(x));
    }

    let one = Decimal::ONE;
    let z = (x - one) / (x + one);
    let z_sq = z * z;

    let mut term = z; // z^(2k+1), starting at k = 0
    let mut sum = Decimal::ZERO;
    for k in 0..LN_TERMS {
        let denom = Decimal::from(2 * k + 1);
        sum += term / denom;
        term *= z_sq; // advance to z^(2(k+1)+1)
    }
    Ok(sum * Decimal::from(2))
}

/// e^(-y) for y in [0, 40] via range reduction y = k + r (integer k, r in [0,1))
/// and (e^-1)^k * exp(-r).
///
/// `exp(-r)` is the alternating Taylor series with EXP_TERMS fixed terms
/// (truncation < 1/22! ~= 9e-22); `e^-1` is the pinned 28-digit constant.
/// `y == 0` returns exactly 1. Inputs outside [0, 40] are domain errors.
pub(crate) fn exp_neg_fixed(y: Decimal) -> Result<Decimal, DecimalMathError> {
    let upper = Decimal::from(40);
    if y.is_sign_negative() && !y.is_zero() || y > upper {
        return Err(DecimalMathError::ExpNegOutOfDomain(y));
    }

    let k = y.floor(); // integer part as Decimal; 0 <= k <= 40
    let r = y - k;

    // (e^-1)^k by repeated multiplication (k <= 40 fixed by the loop bound).
    let e_neg_1 = e_neg_1();
    let mut pow = Decimal::ONE;
    let mut i = Decimal::ZERO;
    while i < k {
        pow *= e_neg_1;
        i += Decimal::ONE;
    }

    // exp(-r) = sum_{n=0..EXP_TERMS-1} (-r)^n / n! ; term_n = term_{n-1} * (-r)/n.
    let neg_r = -r;
    let mut term = Decimal::ONE; // n = 0
    let mut sum = Decimal::ONE;
    for n in 1..EXP_TERMS {
        term *= neg_r / Decimal::from(n);
        sum += term;
    }

    Ok(pow * sum)
}

/// Standard normal CDF via the Abramowitz-Stegun 7.1.26 rational approximation
/// pinned in ADR 0010.
///
/// - `x == 0` returns exactly 0.5 (the axis of symmetry). The A-S formula itself
///   evaluates to 0.500000000524... at 0; returning the exact 0.5 is what makes
///   `phi_cdf(0) == 0.5` and `phi_cdf(-x) + phi_cdf(x) == 1` hold exactly, as
///   SIG-038 pins. This is the only abscissa given a special case.
/// - `x < 0`: `1 - phi_cdf(-x)` (exact symmetry).
/// - `x >= 8.3` (`saturation`): returns exactly 1.
/// - otherwise: `t = 1/(1 + p*x)`, `pdf = exp_neg_fixed(x^2/2) / sqrt(2*pi)`,
///   polynomial by Horner, `1 - pdf * poly`.
///
/// The A-S approximation's own absolute error (~7.5e-8 vs the true normal CDF) is
/// the model's documented error per SIG-038; the decimal arithmetic here adds only
/// rounding far below it.
pub(crate) fn phi_cdf(x: Decimal) -> Result<Decimal, DecimalMathError> {
    if x.is_zero() {
        return Ok(Decimal::from_i128_with_scale(5, 1)); // exactly 0.5
    }
    if x.is_sign_negative() {
        return Ok(Decimal::ONE - phi_cdf(-x)?);
    }
    if x >= saturation() {
        return Ok(Decimal::ONE);
    }

    let one = Decimal::ONE;
    let t = one / (one + as_p() * x); // 1 + p*x >= 1, divisor nonzero

    let half = Decimal::from_i128_with_scale(5, 1);
    let pdf = exp_neg_fixed(x * x * half)? / sqrt_2pi(); // x^2/2 in [0, 40)

    // Horner: t*(b1 + t*(b2 + t*(b3 + t*(b4 + t*b5))))
    let poly = t * (as_b1() + t * (as_b2() + t * (as_b3() + t * (as_b4() + t * as_b5()))));

    Ok(one - pdf * poly)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::str::FromStr;

    fn dec(s: &str) -> Decimal {
        Decimal::from_str(s).expect("valid decimal literal")
    }

    fn abs_diff(a: Decimal, b: Decimal) -> Decimal {
        if a >= b { a - b } else { b - a }
    }

    // Reference values for the A-S 7.1.26 FORMULA ITSELF (not the true normal CDF),
    // evaluated with Python mpmath at 60 digits and rounded half-even to 28
    // significant digits. Asserting against the true CDF at 8 decimals would fail
    // spuriously because A-S 7.1.26 differs from it by up to ~7.5e-8.
    // Points: x in {0, 0.5, 1, 1.6448536270, 2, 3, 5}.
    const AS_0: &str = "0.5000000005248086700855302440";
    const AS_0_5: &str = "0.6914624677873249590533620366";
    const AS_1: &str = "0.8413447404368684110670563617";
    const AS_1_645: &str = "0.9500000156603774840619453956";
    const AS_2: &str = "0.9772499379857480715530171114";
    const AS_3: &str = "0.9986500327777648093977619009";
    const AS_5: &str = "0.9999997128950004976477760335";

    // The kernel reproduces the A-S formula to within its own decimal rounding,
    // far tighter than the ~7.5e-8 model error. Tolerance covers exp/division
    // rounding accumulation with wide margin.
    fn as_tol() -> Decimal {
        dec("0.0000000000000000001") // 1e-19
    }

    #[test]
    fn phi_cdf_matches_abramowitz_stegun_formula() {
        let cases = [
            ("0.5", AS_0_5),
            ("1", AS_1),
            ("1.6448536270", AS_1_645),
            ("2", AS_2),
            ("3", AS_3),
            ("5", AS_5),
        ];
        for (x, reference) in cases {
            let got = phi_cdf(dec(x)).expect("on-domain");
            assert!(
                abs_diff(got, dec(reference)) <= as_tol(),
                "phi_cdf({x}) = {got}, A-S reference {reference}"
            );
        }
        // x = 0 is the exact special case, checked separately below; confirm the
        // A-S formula reference itself is the near-0.5 value it should be.
        assert!(abs_diff(dec(AS_0), dec("0.5")) <= dec("0.0000000006"));
    }

    #[test]
    fn phi_cdf_sanity_band_vs_true_cdf_six_decimals() {
        // True normal CDF (mpmath, 10 digits); A-S agrees to within ~7.5e-8, well
        // inside this band (e.g. Phi(1) ~= 0.841345, Phi(2) ~= 0.977250).
        let band = dec("0.0000005"); // 5e-7
        let cases = [
            ("0.5", "0.6914624613"),
            ("1", "0.8413447461"),
            ("1.6448536270", "0.9500000000"),
            ("2", "0.9772498681"),
            ("3", "0.9986501020"),
            ("5", "0.9999997133"),
        ];
        for (x, true_cdf) in cases {
            let got = phi_cdf(dec(x)).expect("on-domain");
            assert!(
                abs_diff(got, dec(true_cdf)) <= band,
                "phi_cdf({x}) = {got} outside 6-decimal band of {true_cdf}"
            );
        }
    }

    #[test]
    fn phi_cdf_zero_is_exactly_half() {
        assert_eq!(phi_cdf(Decimal::ZERO).expect("zero"), dec("0.5"));
    }

    #[test]
    fn phi_cdf_symmetry_holds_to_full_precision() {
        for x in ["0", "0.5", "1", "1.6448536270", "2", "3", "5"] {
            let pos = phi_cdf(dec(x)).expect("on-domain");
            let neg = phi_cdf(-dec(x)).expect("on-domain");
            assert_eq!(pos + neg, Decimal::ONE, "symmetry failed at x = {x}");
        }
    }

    #[test]
    fn phi_cdf_saturates_to_exactly_one() {
        assert_eq!(phi_cdf(dec("8.3")).expect("sat"), Decimal::ONE);
        assert_eq!(phi_cdf(dec("9")).expect("sat"), Decimal::ONE);
        assert_eq!(phi_cdf(dec("40")).expect("sat"), Decimal::ONE);
        // Negative saturation via symmetry -> exactly 0.
        assert_eq!(phi_cdf(dec("-8.3")).expect("sat"), Decimal::ZERO);
    }

    #[test]
    fn ln_fixed_matches_pinned_constants() {
        // ln(2) to 28 significant digits (mpmath, 60 dps).
        let ln2 = dec("0.6931471805599453094172321215");
        let got = ln_fixed(dec("2")).expect("on-domain");
        assert!(abs_diff(got, ln2) <= dec("0.0000000000000000001")); // 1e-19

        // ln(0.5) == -ln(2), exact to full precision (odd-power series symmetry).
        assert_eq!(ln_fixed(dec("0.5")).expect("on-domain"), -got);

        // ln(1) == 0 exactly.
        assert_eq!(ln_fixed(Decimal::ONE).expect("on-domain"), Decimal::ZERO);
    }

    #[test]
    fn ln_fixed_domain_edges() {
        // Endpoints are in-domain and succeed.
        assert!(ln_fixed(dec("0.25")).is_ok());
        assert!(ln_fixed(dec("4")).is_ok());
        // Just outside the domain errors, no extrapolation.
        assert_eq!(
            ln_fixed(dec("0.2")),
            Err(DecimalMathError::LnOutOfDomain(dec("0.2")))
        );
        assert_eq!(
            ln_fixed(dec("4.5")),
            Err(DecimalMathError::LnOutOfDomain(dec("4.5")))
        );
    }

    #[test]
    fn sqrt_fixed_reference_values() {
        // sqrt(2) to 28 significant digits.
        let sqrt2 = dec("1.414213562373095048801688724");
        let got = sqrt_fixed(dec("2")).expect("nonneg");
        assert!(abs_diff(got, sqrt2) <= dec("0.000000000001")); // 1e-12

        // sqrt(900) == 30 exactly.
        assert_eq!(sqrt_fixed(dec("900")).expect("nonneg"), Decimal::from(30));

        // sqrt(0) == 0.
        assert_eq!(sqrt_fixed(Decimal::ZERO).expect("nonneg"), Decimal::ZERO);
    }

    #[test]
    fn sqrt_fixed_error_bound_on_domain() {
        // Spot-check the <= 1e-12 claim near both ends of [1e-9, 1e6].
        for (x, root) in [
            ("0.000000001", "0.0000316227766016837933"), // 1e-9
            ("1000000", "1000"),                         // 1e6
            ("2", "1.4142135623730950488016887242"),
        ] {
            let got = sqrt_fixed(dec(x)).expect("nonneg");
            let sq = got * got;
            assert!(
                abs_diff(got, dec(root)) <= dec("0.000000000001"),
                "sqrt({x}) = {got} (sq {sq})"
            );
        }
    }

    #[test]
    fn sqrt_fixed_rejects_negative() {
        assert_eq!(
            sqrt_fixed(dec("-1")),
            Err(DecimalMathError::SqrtNegative(dec("-1")))
        );
    }

    #[test]
    fn exp_neg_fixed_reference_values() {
        // 20+ digit references (mpmath).
        let e_neg_1 = dec("0.3678794411714423215955237702");
        let e_neg_half = dec("0.6065306597126334236037995350");
        let e_neg_10 = dec("0.00004539992976248485153559151556");

        assert!(
            abs_diff(exp_neg_fixed(dec("1")).expect("on-domain"), e_neg_1)
                <= dec("0.00000000000000000001")
        );
        assert!(
            abs_diff(exp_neg_fixed(dec("0.5")).expect("on-domain"), e_neg_half)
                <= dec("0.00000000000000000001")
        );
        assert!(
            abs_diff(exp_neg_fixed(dec("10")).expect("on-domain"), e_neg_10)
                <= dec("0.00000000000000000001")
        );

        // y == 0 -> exactly 1.
        assert_eq!(
            exp_neg_fixed(Decimal::ZERO).expect("on-domain"),
            Decimal::ONE
        );
    }

    #[test]
    fn exp_neg_fixed_domain() {
        assert!(exp_neg_fixed(dec("40")).is_ok());
        assert_eq!(
            exp_neg_fixed(dec("-0.1")),
            Err(DecimalMathError::ExpNegOutOfDomain(dec("-0.1")))
        );
        assert_eq!(
            exp_neg_fixed(dec("40.1")),
            Err(DecimalMathError::ExpNegOutOfDomain(dec("40.1")))
        );
    }

    proptest! {
        // phi_cdf is monotone nondecreasing and never panics over the model band.
        #[test]
        fn phi_cdf_monotone_no_panic(
            a in -8_300_000i64..=8_300_000,
            b in -8_300_000i64..=8_300_000,
        ) {
            let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
            let xl = Decimal::from_i128_with_scale(lo as i128, 6);
            let xh = Decimal::from_i128_with_scale(hi as i128, 6);
            let fl = phi_cdf(xl).expect("on-band");
            let fh = phi_cdf(xh).expect("on-band");
            prop_assert!(fl <= fh, "phi_cdf not monotone: f({xl})={fl} > f({xh})={fh}");
        }

        // ln_fixed is monotone nondecreasing on its domain and never panics.
        #[test]
        fn ln_fixed_monotone_no_panic(
            a in 250_000i64..=4_000_000,
            b in 250_000i64..=4_000_000,
        ) {
            let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
            let xl = Decimal::from_i128_with_scale(lo as i128, 6);
            let xh = Decimal::from_i128_with_scale(hi as i128, 6);
            let fl = ln_fixed(xl).expect("on-domain");
            let fh = ln_fixed(xh).expect("on-domain");
            prop_assert!(fl <= fh, "ln_fixed not monotone: f({xl})={fl} > f({xh})={fh}");
        }
    }
}
