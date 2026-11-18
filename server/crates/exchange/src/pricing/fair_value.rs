//! Digital-call fair-value math for the YES book.
//!
//! Shared by the market-maker bot and the trader-bot population so they
//! agree on the underlying probability model and disagree only on the
//! per-bot perception noise / behavior layered on top.
//!
//! The model treats each market as a digital call on BTC: the YES contract
//! pays $1 iff BTC closes strictly above the strike. Returns are modeled as
//! lognormal with a fixed annualized volatility, so the fair YES probability
//! is `Φ(d)` where `d = (btc − strike) / (strike · σ_period)`.

/// BTC's assumed annualized volatility, used to scale moves over the
/// round's duration into a probability. Tuned *below* realized BTC vol
/// (~0.70) on purpose: this is a demo, and a less-volatile model treats
/// any given BTC move as more informative, so a modest drift from strike
/// swings YES decisively (e.g. +$50 with 30s left → ~80¢ instead of ~70¢).
/// Bump up for chillier quotes (more 50/50), down for even sharper
/// conviction on small moves.
pub const BTC_VOL_ANNUAL: f64 = 0.40;

/// 365.25 days × 24h × 3600s.
pub const SECONDS_PER_YEAR: f64 = 31_557_600.0;

/// Abramowitz & Stegun 7.1.26 approximation to the error function. Max
/// absolute error ~1.5e-7 — overkill for picking quote ticks.
pub fn erf(x: f64) -> f64 {
    let sign = if x < 0.0 { -1.0 } else { 1.0 };
    let x = x.abs();
    let p = 0.3275911_f64;
    let a1 = 0.254829592_f64;
    let a2 = -0.284496736_f64;
    let a3 = 1.421413741_f64;
    let a4 = -1.453152027_f64;
    let a5 = 1.061405429_f64;
    let t = 1.0 / (1.0 + p * x);
    let y = 1.0 - (((((a5 * t + a4) * t) + a3) * t + a2) * t + a1) * t * (-x * x).exp();
    sign * y
}

/// Standard-normal cumulative distribution function.
pub fn standard_normal_cdf(x: f64) -> f64 {
    0.5 * (1.0 + erf(x / std::f64::consts::SQRT_2))
}

/// Fair YES probability — the chance that BTC ends above `strike` in
/// `secs_remaining` seconds given current price `btc` — converted to tenths
/// of a cent on the [1, 999] grid. This is the standard digital-call price
/// using a normal-distribution model of returns:
///
/// ```text
///   σ_period = σ_annual · √(secs / year)
///   d        = (btc − strike) / (strike · σ_period)
///   p        = Φ(d)
/// ```
///
/// At market open `btc ≈ strike` so `d ≈ 0` → fair value ≈ 50¢. As BTC
/// drifts the value tracks it; as `secs_remaining → 0` the value pins
/// toward 99.9¢ (if above strike) or 0.1¢ (if below).
///
/// Returns `None` for non-finite or non-positive inputs.
pub fn compute_fair_value_tenths(btc: f64, strike: f64, secs_remaining: f64) -> Option<u16> {
    if !btc.is_finite() || !strike.is_finite() || !secs_remaining.is_finite() {
        return None;
    }
    if btc <= 0.0 || strike <= 0.0 || secs_remaining <= 0.0 {
        return None;
    }
    let sigma_period = BTC_VOL_ANNUAL * (secs_remaining / SECONDS_PER_YEAR).sqrt();
    if sigma_period <= 0.0 {
        return None;
    }
    let d = (btc - strike) / (strike * sigma_period);
    let prob = standard_normal_cdf(d);
    let raw = (prob * 1000.0).round();
    let clamped = raw.clamp(1.0, 999.0);
    Some(clamped as u16)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64, eps: f64) -> bool {
        (a - b).abs() < eps
    }

    #[test]
    fn erf_zero_is_zero() {
        assert!(approx(erf(0.0), 0.0, 1e-9));
    }

    #[test]
    fn erf_one_known_value() {
        assert!(approx(erf(1.0), 0.8427007929, 1e-6));
    }

    #[test]
    fn erf_is_odd() {
        for &x in &[0.1_f64, 0.5, 1.0, 1.7, 3.0] {
            assert!(approx(erf(-x), -erf(x), 1e-9));
        }
    }

    #[test]
    fn cdf_zero_is_half() {
        assert!(approx(standard_normal_cdf(0.0), 0.5, 1e-9));
    }

    #[test]
    fn cdf_1_96_is_approx_975() {
        assert!(approx(standard_normal_cdf(1.96), 0.975, 1e-3));
    }

    #[test]
    fn cdf_neg_1_96_is_approx_025() {
        assert!(approx(standard_normal_cdf(-1.96), 0.025, 1e-3));
    }

    #[test]
    fn cdf_large_positive_saturates_to_one() {
        assert!(standard_normal_cdf(8.0) > 0.9999999);
    }

    #[test]
    fn cdf_large_negative_saturates_to_zero() {
        assert!(standard_normal_cdf(-8.0) < 1e-7);
    }

    #[test]
    fn fair_value_at_strike_is_near_half() {
        let v = compute_fair_value_tenths(100_000.0, 100_000.0, 60.0).unwrap();
        assert_eq!(v, 500);
    }

    #[test]
    fn fair_value_above_strike_drifts_toward_yes() {
        // With BTC_VOL_ANNUAL = 0.40, a $100 move with 60s left is a
        // ~1.8-sigma event → FV lands deep in the 90s.
        let v = compute_fair_value_tenths(100_100.0, 100_000.0, 60.0).unwrap();
        assert!(v > 800, "got {}", v);
    }

    #[test]
    fn fair_value_below_strike_drifts_toward_no() {
        let v = compute_fair_value_tenths(99_900.0, 100_000.0, 60.0).unwrap();
        assert!(v < 200, "got {}", v);
    }

    #[test]
    fn fair_value_pins_to_top_when_far_above_with_little_time() {
        let v = compute_fair_value_tenths(101_000.0, 100_000.0, 1.0).unwrap();
        assert_eq!(v, 999);
    }

    #[test]
    fn fair_value_pins_to_bottom_when_far_below_with_little_time() {
        let v = compute_fair_value_tenths(99_000.0, 100_000.0, 1.0).unwrap();
        assert_eq!(v, 1);
    }

    #[test]
    fn fair_value_more_time_means_less_certainty() {
        let short = compute_fair_value_tenths(100_050.0, 100_000.0, 5.0).unwrap();
        let long = compute_fair_value_tenths(100_050.0, 100_000.0, 60.0).unwrap();
        assert!(
            (short as i32 - 500).abs() > (long as i32 - 500).abs(),
            "short={} long={}",
            short,
            long,
        );
    }

    #[test]
    fn fair_value_rejects_bad_inputs() {
        assert!(compute_fair_value_tenths(0.0, 100_000.0, 60.0).is_none());
        assert!(compute_fair_value_tenths(100_000.0, 0.0, 60.0).is_none());
        assert!(compute_fair_value_tenths(100_000.0, 100_000.0, 0.0).is_none());
        assert!(compute_fair_value_tenths(100_000.0, 100_000.0, -1.0).is_none());
        assert!(compute_fair_value_tenths(f64::NAN, 100_000.0, 60.0).is_none());
        assert!(compute_fair_value_tenths(100_000.0, 100_000.0, f64::INFINITY).is_none());
    }
}
