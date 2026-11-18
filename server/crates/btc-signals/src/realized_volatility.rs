//! Shared realized-volatility unit: the reference-price sample ring, the
//! short/long RMS volatility metrics, the regime classification, and the
//! `btc.external.realized_volatility@1.0.0` value projection.
//!
//! Extracted verbatim from the spot signal processor (SIG-037/038, ADR 0016) so
//! the composite prediction pipeline computes sigma through the identical
//! published code path rather than a fork. `SpotSignalProcessor` owns an instance
//! and delegates; `momentum_metrics` reads the same ring through the accessors
//! here. The ring is rolling market-relative state and is not checkpointed by
//! either pipeline; it re-warms after a restore.

use std::collections::{BTreeMap, VecDeque};

use rust_decimal::Decimal;
use serde_json::{Value, json};
use signals_core::{QualityState, SignalUpdate};

use crate::spot_signals::{
    MISSED_OBSERVATION_NS, SpotSignalError, checked_add, checked_div, checked_mul, decimal_value,
    optional_decimal, return_bps,
};

const MAX_REFERENCE_SAMPLES: usize = 61;

#[derive(Debug, Clone, Copy)]
struct ReferenceSample {
    logical_time_ns: u64,
    price: Decimal,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct VolatilityMetrics {
    pub short_bps: Decimal,
    pub long_bps: Decimal,
    pub short_long_ratio: Decimal,
    pub regime: &'static str,
    pub return_count: u16,
}

/// Rolling one-second reference-price ring plus the realized-volatility metrics
/// derived from it. Shared by the spot processor and the composite prediction
/// processor so both consume the same sigma without duplicating the math.
#[derive(Debug, Clone, Default)]
pub(crate) struct RealizedVolatility {
    samples: VecDeque<ReferenceSample>,
}

impl RealizedVolatility {
    pub(crate) fn new() -> Self {
        Self {
            samples: VecDeque::with_capacity(MAX_REFERENCE_SAMPLES),
        }
    }

    pub(crate) fn clear(&mut self) {
        self.samples.clear();
    }

    pub(crate) fn len(&self) -> usize {
        self.samples.len()
    }

    /// Reference price `offset` samples back from the newest (0 == newest).
    pub(crate) fn price_from_back(&self, offset: usize) -> Option<Decimal> {
        let index = self.samples.len().checked_sub(1 + offset)?;
        self.samples.get(index).map(|sample| sample.price)
    }

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

    /// Advance the ring from an already-extracted price observation.
    ///
    /// The blended reference reaches this through [`RealizedVolatility::update`];
    /// a per-venue lane passes that venue's fresh displayed midpoint directly
    /// (ADR 0024). `None` is an unusable observation and clears the ring, which
    /// is the same continuity rule the blended path has always applied to a
    /// non-ready reference.
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
        if self.samples.len() == MAX_REFERENCE_SAMPLES {
            self.samples.pop_front();
        }
        self.samples.push_back(ReferenceSample {
            logical_time_ns: now_ns,
            price,
        });
    }

    pub(crate) fn metrics(&self) -> Result<Option<VolatilityMetrics>, SpotSignalError> {
        if self.samples.len() < MAX_REFERENCE_SAMPLES {
            return Ok(None);
        }
        let mut returns = Vec::with_capacity(MAX_REFERENCE_SAMPLES - 1);
        for index in 1..self.samples.len() {
            returns.push(return_bps(
                self.samples[index].price,
                self.samples[index - 1].price,
            )?);
        }
        let long = root_mean_square(&returns)?;
        let short = root_mean_square(&returns[returns.len() - 15..])?;
        let ratio = if long.is_zero() {
            Decimal::ONE
        } else {
            checked_div(short, long)?
        };
        let regime = classify_volatility_ratio(ratio);
        Ok(Some(VolatilityMetrics {
            short_bps: short,
            long_bps: long,
            short_long_ratio: ratio,
            regime,
            return_count: u16::try_from(returns.len()).map_err(|_| SpotSignalError::Counter)?,
        }))
    }
}

/// Projects the realized-volatility metrics into the published
/// `btc.external.realized_volatility@1.0.0` value map.
pub(crate) fn realized_volatility_values(
    metrics: &VolatilityMetrics,
    produced_at: &str,
) -> BTreeMap<String, Value> {
    BTreeMap::from([
        (
            "volatility_bps_15s".to_owned(),
            decimal_value(metrics.short_bps),
        ),
        (
            "volatility_bps_60s".to_owned(),
            decimal_value(metrics.long_bps),
        ),
        (
            "short_long_ratio".to_owned(),
            decimal_value(metrics.short_long_ratio),
        ),
        ("regime".to_owned(), json!(metrics.regime)),
        ("return_count".to_owned(), json!(metrics.return_count)),
        ("window_close".to_owned(), json!(produced_at)),
    ])
}

pub(crate) fn classify_volatility_ratio(ratio: Decimal) -> &'static str {
    if ratio <= Decimal::new(8, 1) {
        "contracting"
    } else if ratio >= Decimal::new(12, 1) {
        "expanding"
    } else {
        "stable"
    }
}

pub(crate) fn root_mean_square(values: &[Decimal]) -> Result<Decimal, SpotSignalError> {
    if values.is_empty() {
        return Err(SpotSignalError::State(
            "root mean square requires at least one value".to_owned(),
        ));
    }
    let mut sum = Decimal::ZERO;
    for value in values {
        sum = checked_add(sum, checked_mul(*value, *value)?)?;
    }
    let mean = checked_div(
        sum,
        Decimal::from(u32::try_from(values.len()).map_err(|_| SpotSignalError::Counter)?),
    )?;
    decimal_sqrt(mean)
}

pub(crate) fn decimal_sqrt(value: Decimal) -> Result<Decimal, SpotSignalError> {
    if value < Decimal::ZERO {
        return Err(SpotSignalError::Arithmetic(
            "square root input cannot be negative".to_owned(),
        ));
    }
    if value.is_zero() {
        return Ok(Decimal::ZERO);
    }
    let two = Decimal::from(2_u8);
    let mut estimate = if value > Decimal::ONE {
        checked_add(checked_div(value, two)?, Decimal::ONE)?
    } else {
        Decimal::ONE
    };
    for _ in 0..96 {
        let next = checked_div(checked_add(estimate, checked_div(value, estimate)?)?, two)?;
        if next == estimate {
            return Ok(next);
        }
        estimate = next;
    }
    Ok(estimate)
}
