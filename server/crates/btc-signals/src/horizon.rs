//! Shared longer-horizon momentum and volatility unit (C2, ruling E49).
//!
//! One implementation of the 1 m / 5 m / 15 m causal simple-return momentum and
//! realized volatility over a one-second price ring, reused by every spot
//! processor: the blended `btc.external.*` reading (`SpotSignalProcessor`) and
//! the four per-venue readings that share [`crate::spot_signals::VenueLane`]
//! (Coinbase/Kraken and, through `VenueSpotProcessor`, Binance/Bybit). One
//! [`HorizonRing`] per identity source is fed the same one-second midpoint
//! samples the [`crate::realized_volatility::RealizedVolatility`] micro ring is
//! fed, so the micro identities keep their contract byte-for-byte while the
//! longer horizons publish from an independent 901-sample ring.
//!
//! Semantics (spec §2, §3): causal, one-second samples, no gap bridging — a
//! missing or stale one-second sample restarts the warm-up; warm-up is the
//! longest horizon (901 contiguous samples). Returns are simple basis-point
//! returns rounded to eight places round-half-to-even on output; the 0.8 / 1.2
//! regime boundaries are labelled research defaults, identical to
//! `btc.external.realized_volatility`.

use std::collections::{BTreeMap, VecDeque};

use rust_decimal::Decimal;
use serde_json::{Value, json};
use signals_core::{QualityState, SignalUpdate};

use crate::realized_volatility::{classify_volatility_ratio, root_mean_square};
use crate::spot_signals::{
    MISSED_OBSERVATION_NS, SpotSignalError, decimal_value, direction_word, optional_decimal,
    return_bps,
};

/// The longest horizon in one-second samples (15 minutes). One extra sample over
/// the 900 one-second returns the longest window needs, so the 15 m return has a
/// `now − 900 s` reference.
pub(crate) const HORIZON_MAX_SAMPLES: usize = 901;

/// Horizon offsets, in one-second samples: 1 m / 5 m / 15 m.
const OFFSET_1M: usize = 60;
const OFFSET_5M: usize = 300;
const OFFSET_15M: usize = 900;

#[derive(Debug, Clone, Copy)]
struct HorizonSample {
    logical_time_ns: u64,
    price: Decimal,
}

/// The 1 m / 5 m / 15 m causal simple basis-point returns.
#[derive(Debug, Clone, Copy)]
pub(crate) struct HorizonMomentumMetrics {
    pub return_1m: Decimal,
    pub return_5m: Decimal,
    pub return_15m: Decimal,
}

/// The 1 m / 5 m / 15 m realized-volatility metrics.
#[derive(Debug, Clone, Copy)]
pub(crate) struct HorizonVolatilityMetrics {
    pub volatility_1m: Decimal,
    pub volatility_5m: Decimal,
    pub volatility_15m: Decimal,
    pub short_long_ratio: Decimal,
    pub regime: &'static str,
}

/// Rolling one-second price ring holding up to [`HORIZON_MAX_SAMPLES`] samples.
///
/// Identical continuity rule to [`crate::realized_volatility::RealizedVolatility`]:
/// `None`, a non-monotonic timestamp, or a gap of at least
/// [`MISSED_OBSERVATION_NS`] clears the ring and restarts the warm-up.
#[derive(Debug, Clone, Default)]
pub(crate) struct HorizonRing {
    samples: VecDeque<HorizonSample>,
}

impl HorizonRing {
    pub(crate) fn new() -> Self {
        Self {
            samples: VecDeque::with_capacity(HORIZON_MAX_SAMPLES),
        }
    }

    pub(crate) fn clear(&mut self) {
        self.samples.clear();
    }

    fn price_from_back(&self, offset: usize) -> Option<Decimal> {
        let index = self.samples.len().checked_sub(1 + offset)?;
        self.samples.get(index).map(|sample| sample.price)
    }

    /// Advance the ring from the blended `btc.external.spot_reference` update,
    /// exactly as [`crate::realized_volatility::RealizedVolatility::update`]
    /// does: a non-ready reference is an unusable observation that clears the
    /// ring (continuity break).
    pub(crate) fn update(
        &mut self,
        reference: &SignalUpdate,
        now_ns: u64,
    ) -> Result<(), SpotSignalError> {
        if reference.quality.state != QualityState::Ready {
            self.update_price(None, now_ns);
            return Ok(());
        }
        let price = optional_decimal(&reference.values, "reference_price")?;
        self.update_price(price, now_ns);
        Ok(())
    }

    /// Advance the ring from a one-second midpoint observation. `None` is an
    /// unusable observation and clears the ring (continuity break), exactly as
    /// the micro ring treats a non-ready reference.
    pub(crate) fn update_price(&mut self, price: Option<Decimal>, now_ns: u64) {
        let Some(price) = price else {
            self.samples.clear();
            return;
        };
        if self.samples.back().is_some_and(|sample| {
            now_ns <= sample.logical_time_ns
                || now_ns.saturating_sub(sample.logical_time_ns) >= MISSED_OBSERVATION_NS
        }) {
            self.samples.clear();
        }
        if self.samples.len() == HORIZON_MAX_SAMPLES {
            self.samples.pop_front();
        }
        self.samples.push_back(HorizonSample {
            logical_time_ns: now_ns,
            price,
        });
    }

    /// The 1 m / 5 m / 15 m momentum, or `None` while the ring is still warming
    /// up to the longest horizon (fewer than [`HORIZON_MAX_SAMPLES`] samples).
    pub(crate) fn momentum(&self) -> Result<Option<HorizonMomentumMetrics>, SpotSignalError> {
        if self.samples.len() < HORIZON_MAX_SAMPLES {
            return Ok(None);
        }
        let current = self
            .price_from_back(0)
            .ok_or_else(|| SpotSignalError::State("warm horizon ring has no newest".to_owned()))?;
        let one_minute = self.price_from_back(OFFSET_1M).ok_or_else(missing)?;
        let five_minute = self.price_from_back(OFFSET_5M).ok_or_else(missing)?;
        let fifteen_minute = self.price_from_back(OFFSET_15M).ok_or_else(missing)?;
        Ok(Some(HorizonMomentumMetrics {
            return_1m: return_bps(current, one_minute)?,
            return_5m: return_bps(current, five_minute)?,
            return_15m: return_bps(current, fifteen_minute)?,
        }))
    }

    /// The 1 m / 5 m / 15 m realized volatility, or `None` during warm-up.
    pub(crate) fn volatility(&self) -> Result<Option<HorizonVolatilityMetrics>, SpotSignalError> {
        if self.samples.len() < HORIZON_MAX_SAMPLES {
            return Ok(None);
        }
        let mut returns = Vec::with_capacity(self.samples.len() - 1);
        for index in 1..self.samples.len() {
            returns.push(return_bps(
                self.samples[index].price,
                self.samples[index - 1].price,
            )?);
        }
        // The one-second returns inside (now − h, now]: the last h returns.
        let volatility_15m = root_mean_square(&returns[returns.len() - OFFSET_15M..])?;
        let volatility_5m = root_mean_square(&returns[returns.len() - OFFSET_5M..])?;
        let volatility_1m = root_mean_square(&returns[returns.len() - OFFSET_1M..])?;
        // short / long with the micro convention: both zero → 1, regime stable.
        let short_long_ratio = if volatility_15m.is_zero() {
            Decimal::ONE
        } else {
            crate::spot_signals::checked_div(volatility_1m, volatility_15m)?
        };
        let regime = classify_volatility_ratio(short_long_ratio);
        Ok(Some(HorizonVolatilityMetrics {
            volatility_1m,
            volatility_5m,
            volatility_15m,
            short_long_ratio,
            regime,
        }))
    }
}

fn missing() -> SpotSignalError {
    SpotSignalError::State("warm horizon ring is missing a horizon sample".to_owned())
}

#[cfg(test)]
impl HorizonRing {
    /// Test-only: the 1 m simple bps return from whatever samples exist, so the
    /// momentum math can be hand-checked on a 61-sample ring independent of the
    /// 15 m readiness gate.
    pub(crate) fn one_minute_return_for_test(&self) -> Decimal {
        let current = self.price_from_back(0).expect("newest sample");
        let prior = self
            .price_from_back(OFFSET_1M)
            .expect("one-minute-back sample");
        return_bps(current, prior).expect("return computes")
    }

    /// Test-only: the RMS of the one-second returns inside the trailing minute.
    pub(crate) fn one_minute_rms_for_test(&self) -> Decimal {
        let len = self.samples.len();
        let mut returns = Vec::with_capacity(OFFSET_1M);
        for index in (len - OFFSET_1M)..len {
            returns.push(
                return_bps(self.samples[index].price, self.samples[index - 1].price)
                    .expect("return computes"),
            );
        }
        root_mean_square(&returns).expect("rms computes")
    }
}

/// Projects the horizon momentum into the published
/// `btc.<scope>.horizon_momentum@1.0.0` value map.
pub(crate) fn horizon_momentum_values(
    metrics: HorizonMomentumMetrics,
    produced_at: &str,
) -> BTreeMap<String, Value> {
    BTreeMap::from([
        ("return_bps_1m".to_owned(), decimal_value(metrics.return_1m)),
        ("return_bps_5m".to_owned(), decimal_value(metrics.return_5m)),
        (
            "return_bps_15m".to_owned(),
            decimal_value(metrics.return_15m),
        ),
        (
            "direction_5m".to_owned(),
            json!(direction_word(metrics.return_5m)),
        ),
        ("window_close".to_owned(), json!(produced_at)),
    ])
}

/// Projects the horizon volatility into the published
/// `btc.<scope>.horizon_volatility@1.0.0` value map.
pub(crate) fn horizon_volatility_values(
    metrics: HorizonVolatilityMetrics,
    produced_at: &str,
) -> BTreeMap<String, Value> {
    BTreeMap::from([
        (
            "volatility_bps_1m".to_owned(),
            decimal_value(metrics.volatility_1m),
        ),
        (
            "volatility_bps_5m".to_owned(),
            decimal_value(metrics.volatility_5m),
        ),
        (
            "volatility_bps_15m".to_owned(),
            decimal_value(metrics.volatility_15m),
        ),
        (
            "short_long_ratio".to_owned(),
            decimal_value(metrics.short_long_ratio),
        ),
        ("regime".to_owned(), json!(metrics.regime)),
        ("window_close".to_owned(), json!(produced_at)),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::prelude::ToPrimitive;

    const SEC: u64 = 1_000_000_000;

    fn d(mantissa: i64, scale: u32) -> Decimal {
        Decimal::new(mantissa, scale)
    }

    /// Push one price per second starting at t = 1 s.
    fn fill(ring: &mut HorizonRing, prices: &[Decimal]) {
        for (index, price) in prices.iter().enumerate() {
            let now_ns = (index as u64 + 1) * SEC;
            ring.update_price(Some(*price), now_ns);
        }
    }

    /// Spec §6: hand-computed 1 m return and RMS on a 61-sample synthetic ring.
    /// Path is flat at 100 for the first 60 samples, then steps to 100.10, so the
    /// one-minute return is exactly 10 bps and the trailing-minute returns are 59
    /// zeros and a single +10 bps.
    #[test]
    fn one_minute_return_and_rms_are_hand_computable() {
        let mut prices = vec![d(100, 0); 60];
        prices.push(d(10010, 2));
        assert_eq!(prices.len(), 61);
        let mut ring = HorizonRing::new();
        fill(&mut ring, &prices);

        // return = 10_000 * (100.10 - 100) / 100 = 10 bps, exactly.
        assert_eq!(ring.one_minute_return_for_test(), d(10, 0));

        // RMS of [0 x 59, 10] over 60 returns = sqrt(100 / 60).
        let expected = (100.0_f64 / 60.0).sqrt();
        let got = ring
            .one_minute_rms_for_test()
            .to_f64()
            .expect("rms is finite");
        assert!(
            (got - expected).abs() < 1e-8,
            "rms {got} within 1e-8 of hand value {expected}"
        );
    }

    /// Spec §6: 0/0 → ratio 1. A flat 901-sample ring has zero volatility in every
    /// window, so the short/long ratio is defined as 1 and the regime is stable.
    #[test]
    fn flat_prices_give_zero_volatility_ratio_one_stable() {
        let prices = vec![d(100, 0); HORIZON_MAX_SAMPLES];
        let mut ring = HorizonRing::new();
        fill(&mut ring, &prices);
        let vol = ring.volatility().expect("no error").expect("warm");
        assert_eq!(vol.volatility_1m, Decimal::ZERO);
        assert_eq!(vol.volatility_5m, Decimal::ZERO);
        assert_eq!(vol.volatility_15m, Decimal::ZERO);
        assert_eq!(vol.short_long_ratio, Decimal::ONE);
        assert_eq!(vol.regime, "stable");
    }

    /// Spec §6: warm-up boundary — n − 1 samples not ready, n ready. The longest
    /// horizon is 901 samples (15 minutes).
    #[test]
    fn warmup_boundary_is_the_longest_horizon() {
        let mut ring = HorizonRing::new();
        fill(&mut ring, &vec![d(100, 0); HORIZON_MAX_SAMPLES - 1]);
        assert!(ring.momentum().expect("no error").is_none());
        assert!(ring.volatility().expect("no error").is_none());

        ring.update_price(Some(d(100, 0)), HORIZON_MAX_SAMPLES as u64 * SEC);
        assert!(ring.momentum().expect("no error").is_some());
        assert!(ring.volatility().expect("no error").is_some());
    }

    /// Spec §6: a continuity break (a missing/stale one-second sample) restarts
    /// the warm-up — the ring is cleared and must refill from scratch.
    #[test]
    fn a_gap_restarts_the_warmup() {
        let mut ring = HorizonRing::new();
        fill(&mut ring, &vec![d(100, 0); HORIZON_MAX_SAMPLES]);
        assert!(ring.momentum().expect("no error").is_some());

        // A sample more than one second after the last observation is a gap: the
        // ring clears and the single fresh sample cannot satisfy the warm-up.
        let last_ns = HORIZON_MAX_SAMPLES as u64 * SEC;
        ring.update_price(Some(d(100, 0)), last_ns + 3 * SEC);
        assert!(ring.momentum().expect("no error").is_none());
        assert!(ring.volatility().expect("no error").is_none());
    }

    /// A non-ready / missing observation (None) also clears the ring.
    #[test]
    fn a_missing_observation_clears_the_ring() {
        let mut ring = HorizonRing::new();
        fill(&mut ring, &vec![d(100, 0); HORIZON_MAX_SAMPLES]);
        ring.update_price(None, (HORIZON_MAX_SAMPLES as u64 + 1) * SEC);
        assert!(ring.momentum().expect("no error").is_none());
    }

    /// A rising step gives a positive 1 m / 5 m / 15 m return and an "up"
    /// direction on the middle horizon.
    #[test]
    fn a_step_gives_positive_returns_and_up_direction() {
        let mut prices = vec![d(100, 0); HORIZON_MAX_SAMPLES - 1];
        prices.push(d(10020, 2));
        let mut ring = HorizonRing::new();
        fill(&mut ring, &prices);
        let mom = ring.momentum().expect("no error").expect("warm");
        // back(60) = back(300) = back(900) = 100, current = 100.20 -> 20 bps.
        assert_eq!(mom.return_1m, d(20, 0));
        assert_eq!(mom.return_5m, d(20, 0));
        assert_eq!(mom.return_15m, d(20, 0));
        let values = horizon_momentum_values(mom, "2026-09-04T00:00:00Z");
        assert_eq!(values["direction_5m"], serde_json::json!("up"));
    }
}
