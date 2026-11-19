//! The one win-probability model for an NBA game.
//!
//! Used by the derived win-probability signal, fed by the real score,
//! clock and pregame line, so every reader of it agrees.
//!
//! The score margin is modelled as a Brownian motion with a full-game
//! standard deviation of [`SIGMA_FINAL`] points and a drift equal to the
//! expected final margin scaled by the fraction of the game left. P(home)
//! is the probability the diffusion ends positive, shrunk toward even by
//! a share of doubt that scales with the square root of the time left
//! ([`HUMILITY`]): a fourteen-point lead with nine minutes to play reads
//! 88, not 98, while a one-point game barely moves and the final still
//! snaps to the boundary. At zero time it is a step function, with a tie
//! at 0.5 (overtime pending). [`prior_drift`] inverts the model at full
//! time so a pregame probability (the market's own price before tipoff)
//! can be turned into a drift: that is how the model uses what the market
//! knew before the game without knowing the team ratings.

use crate::pricing::fair_value::standard_normal_cdf;

/// Four twelve-minute quarters.
pub const REGULATION_SECONDS: f64 = 2880.0;
/// One period of regulation.
pub const PERIOD_SECONDS: f64 = 720.0;
/// One overtime period.
pub const OVERTIME_SECONDS: f64 = 300.0;
/// Standard deviation of the final margin over a full game, in points.
/// Measured from the possession engine (about 15.8 across every matchup)
/// rather than taken from the real league, so the model is calibrated to
/// the games that settle the rounds; the engine's calibration test pins
/// it. Wider than a real NBA game, which is what keeps a mid-game lead
/// from reading as a lock.
pub const SIGMA_FINAL: f64 = 16.0;

const P_MIN: f64 = 0.01;
const P_MAX: f64 = 0.99;
/// The share of the model's conviction given back to even at full time;
/// it fades with the square root of the time left and is gone at the
/// final. Keeps a decided game from reading as a lock while minutes
/// remain, without touching a close one.
pub const HUMILITY: f64 = 0.5;

/// How much of the diffusion's conviction the model keeps with fraction
/// `t` of the game left.
fn conviction(t: f64) -> f64 {
    1.0 - HUMILITY * t.clamp(0.0, 1.0).sqrt()
}

/// P(home wins) given the current margin (home minus away), the game
/// seconds left and the expected full-game margin in the home side's
/// favour.
pub fn win_probability(margin: f64, seconds_remaining: f64, drift_per_game: f64) -> f64 {
    let t = seconds_remaining.max(0.0) / REGULATION_SECONDS;
    if t <= 0.0 {
        return if margin > 0.0 {
            P_MAX
        } else if margin < 0.0 {
            P_MIN
        } else {
            0.5
        };
    }
    let z = (margin + drift_per_game * t) / (SIGMA_FINAL * t.sqrt());
    let p = 0.5 + (standard_normal_cdf(z) - 0.5) * conviction(t);
    p.clamp(P_MIN, P_MAX)
}

/// The full-game drift that makes [`win_probability`] equal `prior` at
/// tipoff with a level score: the humility at full time is undone first.
pub fn prior_drift(prior: f64) -> f64 {
    let raw = 0.5 + (prior.clamp(P_MIN, P_MAX) - 0.5) / conviction(1.0);
    inverse_normal_cdf(raw.clamp(P_MIN, P_MAX)) * SIGMA_FINAL
}

/// The expected full-game margin from two net ratings and home court:
/// the world's own truth for a matchup.
pub fn expected_margin(home_rating: f64, away_rating: f64, home_court: f64) -> f64 {
    home_rating + home_court - away_rating
}

/// Game seconds left: the rest of this period plus every later regulation
/// period. In overtime only the current period counts, because later
/// periods are conditional on a tie.
pub fn game_seconds_remaining(quarter: u8, clock_remaining: f64) -> f64 {
    if quarter <= 4 {
        clock_remaining + f64::from(4 - quarter) * PERIOD_SECONDS
    } else {
        clock_remaining
    }
}

/// Acklam's rational approximation to the inverse standard-normal CDF
/// (relative error under 1.2e-9). Plenty for turning a line into a drift.
pub fn inverse_normal_cdf(p: f64) -> f64 {
    const A: [f64; 6] = [
        -3.969683028665376e+01,
        2.209460984245205e+02,
        -2.759285104469687e+02,
        1.383_577_518_672_69e+02,
        -3.066479806614716e+01,
        2.506628277459239e+00,
    ];
    const B: [f64; 5] = [
        -5.447609879822406e+01,
        1.615858368580409e+02,
        -1.556989798598866e+02,
        6.680131188771972e+01,
        -1.328068155288572e+01,
    ];
    const C: [f64; 6] = [
        -7.784894002430293e-03,
        -3.223964580411365e-01,
        -2.400758277161838e+00,
        -2.549732539343734e+00,
        4.374664141464968e+00,
        2.938163982698783e+00,
    ];
    const D: [f64; 4] = [
        7.784695709041462e-03,
        3.224671290700398e-01,
        2.445134137142996e+00,
        3.754408661907416e+00,
    ];
    const P_LOW: f64 = 0.02425;
    let p = p.clamp(f64::MIN_POSITIVE, 1.0 - f64::EPSILON);
    if p < P_LOW {
        let q = (-2.0 * p.ln()).sqrt();
        (((((C[0] * q + C[1]) * q + C[2]) * q + C[3]) * q + C[4]) * q + C[5])
            / ((((D[0] * q + D[1]) * q + D[2]) * q + D[3]) * q + 1.0)
    } else if p <= 1.0 - P_LOW {
        let q = p - 0.5;
        let r = q * q;
        (((((A[0] * r + A[1]) * r + A[2]) * r + A[3]) * r + A[4]) * r + A[5]) * q
            / (((((B[0] * r + B[1]) * r + B[2]) * r + B[3]) * r + B[4]) * r + 1.0)
    } else {
        let q = (-2.0 * (1.0 - p).ln()).sqrt();
        -(((((C[0] * q + C[1]) * q + C[2]) * q + C[3]) * q + C[4]) * q + C[5])
            / ((((D[0] * q + D[1]) * q + D[2]) * q + D[3]) * q + 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64, eps: f64) -> bool {
        (a - b).abs() < eps
    }

    #[test]
    fn level_game_at_tipoff_is_a_coin_flip() {
        assert!(approx(win_probability(0.0, REGULATION_SECONDS, 0.0), 0.5, 1e-9));
    }

    #[test]
    fn zero_time_is_a_step_with_a_tie_pending() {
        assert_eq!(win_probability(1.0, 0.0, 0.0), 0.99);
        assert_eq!(win_probability(-1.0, 0.0, 0.0), 0.01);
        assert_eq!(win_probability(0.0, 0.0, 0.0), 0.5);
        assert_eq!(win_probability(0.0, -5.0, 0.0), 0.5, "negative time is zero time");
    }

    #[test]
    fn leads_matter_more_late() {
        let early = win_probability(6.0, REGULATION_SECONDS * 0.75, 0.0);
        let late = win_probability(6.0, 120.0, 0.0);
        assert!(late > early && late > 0.9, "early={early} late={late}");
        assert!(win_probability(-6.0, 120.0, 0.0) < 0.1);
    }

    /// Inverting holds across every line the league can open at (and
    /// well beyond); a prior past what full-time humility can express
    /// saturates rather than failing.
    #[test]
    fn prior_drift_inverts_the_model_at_tipoff() {
        for prior in [0.3, 0.45, 0.5, 0.55, 0.61, 0.7] {
            let p = win_probability(0.0, REGULATION_SECONDS, prior_drift(prior));
            assert!(approx(p, prior, 1e-6), "prior {prior} → {p}");
        }
        assert!(approx(prior_drift(0.5), 0.0, 1e-9));
        let saturated = win_probability(0.0, REGULATION_SECONDS, prior_drift(0.9));
        assert!(saturated > 0.7 && saturated < 0.9, "{saturated}");
        assert!(prior_drift(0.05) < prior_drift(0.3));
    }

    #[test]
    fn inverse_normal_matches_known_quantiles() {
        assert!(approx(inverse_normal_cdf(0.975), 1.959964, 1e-5));
        assert!(approx(inverse_normal_cdf(0.5), 0.0, 1e-9));
        assert!(approx(inverse_normal_cdf(0.01), -2.326348, 1e-5));
        for p in [0.001, 0.02, 0.2, 0.5, 0.8, 0.98, 0.999] {
            assert!(approx(standard_normal_cdf(inverse_normal_cdf(p)), p, 1e-6), "{p}");
        }
    }

    #[test]
    fn game_seconds_count_later_regulation_periods_but_not_later_overtimes() {
        assert_eq!(game_seconds_remaining(1, 720.0), REGULATION_SECONDS);
        assert_eq!(game_seconds_remaining(4, 30.0), 30.0);
        assert_eq!(game_seconds_remaining(2, 100.0), 100.0 + 2.0 * PERIOD_SECONDS);
        assert_eq!(game_seconds_remaining(5, 200.0), 200.0);
        assert_eq!(game_seconds_remaining(6, 10.0), 10.0);
    }

    #[test]
    fn expected_margin_adds_home_court() {
        assert_eq!(expected_margin(4.0, 3.0, 2.5), 3.5);
        let p = win_probability(0.0, REGULATION_SECONDS, expected_margin(4.0, 3.0, 2.5));
        assert!(p > 0.53 && p < 0.56, "a 3.5-point favourite, humbled at full time: {p}");
    }

    /// The worked examples: a 55% favourite up three with forty minutes
    /// left is in the high fifties; down ten at the half is a third; a
    /// fourteen-point lead with nine minutes left is high eighties, not
    /// high nineties; down ten with four minutes left is under ten.
    #[test]
    fn leads_price_sanely_and_blowouts_are_not_locks() {
        let drift = prior_drift(0.55);
        let up3_early = win_probability(3.0, 2400.0, drift);
        assert!(up3_early > 0.55 && up3_early < 0.60, "{up3_early}");
        let down10_half = win_probability(-10.0, 1440.0, drift);
        assert!(down10_half > 0.28 && down10_half < 0.38, "{down10_half}");
        let one_point_late = win_probability(1.0, 525.0, drift);
        assert!(one_point_late > 0.53 && one_point_late < 0.58, "{one_point_late}");
        let blowout_late = win_probability(14.0, 525.0, drift);
        assert!(blowout_late > 0.85 && blowout_late < 0.91, "{blowout_late}");
        let down10_late = win_probability(-10.0, 240.0, drift);
        assert!(down10_late > 0.05 && down10_late < 0.12, "{down10_late}");
        // The final is still the step function.
        assert_eq!(win_probability(1.0, 0.0, drift), 0.99);
    }
}
