//! The shared technical-analysis unit behind the C3a pack (ruling E51): one
//! one-second sample ring, the one-minute bar series closed on the lane decision
//! clock, and the seven indicators every price scope publishes.
//!
//! Every scope — the blended reference, the four spot venues and the two
//! perpetual mark series — drives one [`TaEngine`] with its own one-second
//! sample, so the arithmetic is byte-identical across scopes and only the sample
//! series differs. The engine holds no identity, no emitter and no clock text:
//! it takes `(price, now_ns)` and returns readings, which each lane projects
//! into its own value maps. That is what keeps `spot_signals`,
//! `venue_spot_signals` and `venue_derivatives_signals` free of duplicated
//! indicator math.
//!
//! Continuity is the same rule the volatility ring applies (ADR 0016): a missing
//! or unusable sample, or a gap of two seconds or more between samples, is a
//! causal break — every retained sample, bar and smoothed state is discarded and
//! the warm-ups restart. Nothing is bridged across a gap, so a published reading
//! is always derived from one contiguous run of observations.

use std::collections::{BTreeMap, VecDeque};

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde_json::{Value, json};

use crate::market_state::ONE_SECOND_NS;
use crate::realized_volatility::decimal_sqrt;
use crate::spot_signals::{
    MISSED_OBSERVATION_NS, SpotSignalError, checked_add, checked_div, checked_mul, checked_sub,
    decimal_value, logical_timestamp_text,
};

/// One-minute bars, closed on the lane decision clock at minute boundaries.
const BAR_INTERVAL_NS: u64 = 60 * ONE_SECOND_NS;

/// Wilder's period for both ATR and RSI (research default).
const WILDER_PERIOD: usize = 14;
/// The EMA cross lengths (research defaults).
const EMA_FAST_BARS: usize = 9;
const EMA_SLOW_BARS: usize = 21;
/// The MACD lengths (research defaults, the standard Appel parameters).
const MACD_FAST_BARS: usize = 12;
const MACD_SLOW_BARS: usize = 26;
const MACD_SIGNAL_BARS: usize = 9;
/// The Bollinger window and band width in standard deviations (research defaults).
const BOLLINGER_BARS: usize = 20;
const BOLLINGER_SIGMA: u32 = 2;

/// The rate-of-change horizons, in one-second samples (research defaults).
const ROC_HORIZONS: [usize; 3] = [10, 60, 300];
/// The Z-score windows, in one-second samples (research defaults).
const Z_WINDOWS: [usize; 3] = [60, 300, 900];

/// The sample ring holds the longest horizon any one-second indicator reads.
const MAX_SAMPLES: usize = 900;
/// The closed-bar ring holds the longest bar window any indicator reads.
const MAX_BARS: usize = BOLLINGER_BARS;

/// Basis-point scale factor.
fn ten_thousand() -> Decimal {
    Decimal::from(10_000_u32)
}

/// One closed one-minute bar. `open` is carried for completeness of the bar
/// contract even though no adopted indicator reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Bar {
    pub(crate) open: Decimal,
    pub(crate) high: Decimal,
    pub(crate) low: Decimal,
    pub(crate) close: Decimal,
    /// The logical instant the bar closed: the end of its minute.
    pub(crate) close_ns: u64,
}

/// The bar being accumulated. `aligned` records whether it began at the top of
/// its minute — a bar started mid-minute (the first after a continuity break)
/// is incomplete by construction and is discarded rather than closed.
#[derive(Debug, Clone, Copy)]
struct PartialBar {
    minute: u64,
    open: Decimal,
    high: Decimal,
    low: Decimal,
    close: Decimal,
    aligned: bool,
}

/// Wilder's smoothed average: the arithmetic mean of the first `period` inputs,
/// then `previous + (input - previous) / period`.
#[derive(Debug, Clone)]
struct WilderAverage {
    period: usize,
    seed: Vec<Decimal>,
    value: Option<Decimal>,
}

impl WilderAverage {
    fn new(period: usize) -> Self {
        Self {
            period,
            seed: Vec::with_capacity(period),
            value: None,
        }
    }

    fn reset(&mut self) {
        self.seed.clear();
        self.value = None;
    }

    fn update(&mut self, input: Decimal) -> Result<(), SpotSignalError> {
        match self.value {
            Some(previous) => {
                let period = decimal_count(self.period)?;
                let step = checked_div(checked_sub(input, previous)?, period)?;
                self.value = Some(checked_add(previous, step)?);
            }
            None => {
                self.seed.push(input);
                if self.seed.len() == self.period {
                    self.value = Some(mean(&self.seed)?);
                }
            }
        }
        Ok(())
    }

    const fn value(&self) -> Option<Decimal> {
        self.value
    }
}

/// An exponential moving average with alpha = 2 / (period + 1), seeded with the
/// arithmetic mean of its first `period` inputs.
#[derive(Debug, Clone)]
struct Ema {
    period: usize,
    alpha: Decimal,
    seed: Vec<Decimal>,
    value: Option<Decimal>,
}

impl Ema {
    fn new(period: usize) -> Result<Self, SpotSignalError> {
        let alpha = checked_div(
            Decimal::from(2_u8),
            checked_add(decimal_count(period)?, Decimal::ONE)?,
        )?;
        Ok(Self {
            period,
            alpha,
            seed: Vec::with_capacity(period),
            value: None,
        })
    }

    fn reset(&mut self) {
        self.seed.clear();
        self.value = None;
    }

    fn update(&mut self, input: Decimal) -> Result<(), SpotSignalError> {
        match self.value {
            Some(previous) => {
                let step = checked_mul(self.alpha, checked_sub(input, previous)?)?;
                self.value = Some(checked_add(previous, step)?);
            }
            None => {
                self.seed.push(input);
                if self.seed.len() == self.period {
                    self.value = Some(mean(&self.seed)?);
                }
            }
        }
        Ok(())
    }

    const fn value(&self) -> Option<Decimal> {
        self.value
    }
}

/// The ATR reading: the average true range in price units plus its scale-free
/// basis-point form and the bar close it was measured against.
#[derive(Debug, Clone, Copy)]
pub(crate) struct AtrReading {
    pub(crate) atr: Decimal,
    pub(crate) atr_bps: Decimal,
    pub(crate) close: Decimal,
    pub(crate) bar_count: u16,
}

/// The RSI reading and its research-threshold regime word.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RsiReading {
    pub(crate) rsi_14: Decimal,
    pub(crate) regime: &'static str,
    pub(crate) bar_count: u16,
}

/// The rate-of-change reading over the three research horizons.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RocReading {
    pub(crate) roc_bps_10s: Decimal,
    pub(crate) roc_bps_1m: Decimal,
    pub(crate) roc_bps_5m: Decimal,
    pub(crate) sample_count: u16,
}

/// The EMA cross reading. `crossed_close_ns` is `None` until a sign change of
/// the spread has been observed since continuity started.
#[derive(Debug, Clone, Copy)]
pub(crate) struct EmaCrossReading {
    pub(crate) ema_9: Decimal,
    pub(crate) ema_21: Decimal,
    pub(crate) spread_bps: Decimal,
    pub(crate) state: &'static str,
    pub(crate) crossed_close_ns: Option<u64>,
    pub(crate) bar_count: u16,
}

/// The MACD reading, published scale-free as basis points of the latest close.
#[derive(Debug, Clone, Copy)]
pub(crate) struct MacdReading {
    pub(crate) macd_bps: Decimal,
    pub(crate) signal_bps: Decimal,
    pub(crate) histogram_bps: Decimal,
    pub(crate) bar_count: u16,
}

/// The Bollinger band position reading.
#[derive(Debug, Clone, Copy)]
pub(crate) struct BollingerReading {
    pub(crate) percent_b: Decimal,
    pub(crate) bandwidth_bps: Decimal,
    pub(crate) state: &'static str,
    pub(crate) bar_count: u16,
}

/// The Z-score reading over the three research windows.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ZScoreReading {
    pub(crate) z_1m: Decimal,
    pub(crate) z_5m: Decimal,
    pub(crate) z_15m: Decimal,
    pub(crate) sample_count: u16,
}

/// What the Z-score has to say on one decision: a reading, a flat window (at
/// least one of the three has zero standard deviation, so no score is defined),
/// or not enough contiguous samples yet.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ZScoreOutcome {
    Ready(ZScoreReading),
    FlatWindow,
    Warming,
}

/// The one-second sample ring, the one-minute bar series and the seven
/// indicators derived from them, for exactly one price scope.
#[derive(Debug, Clone)]
pub(crate) struct TaEngine {
    /// The contiguous one-second samples, newest last.
    samples: VecDeque<Decimal>,
    /// The contiguous closed one-minute bars, newest last.
    bars: VecDeque<Bar>,
    partial: Option<PartialBar>,
    last_sample_ns: Option<u64>,
    /// The previous closed bar's close, the baseline for true range and for the
    /// RSI's bar-to-bar change. `None` until the first bar closes.
    previous_close: Option<Decimal>,
    /// Closed bars behind the current state since the last continuity break,
    /// saturating at `u16::MAX` (about forty-five days of bars).
    bar_count: u16,
    atr: WilderAverage,
    gains: WilderAverage,
    losses: WilderAverage,
    ema_fast: Ema,
    ema_slow: Ema,
    macd_fast: Ema,
    macd_slow: Ema,
    macd_signal: Ema,
    /// The sign of the EMA spread at the previous closed bar, for cross
    /// detection, and the close instant of the most recent sign change.
    previous_spread_sign: Option<i8>,
    crossed_close_ns: Option<u64>,
}

impl TaEngine {
    pub(crate) fn new() -> Result<Self, SpotSignalError> {
        Ok(Self {
            samples: VecDeque::with_capacity(MAX_SAMPLES),
            bars: VecDeque::with_capacity(MAX_BARS),
            partial: None,
            last_sample_ns: None,
            previous_close: None,
            bar_count: 0,
            atr: WilderAverage::new(WILDER_PERIOD),
            gains: WilderAverage::new(WILDER_PERIOD),
            losses: WilderAverage::new(WILDER_PERIOD),
            ema_fast: Ema::new(EMA_FAST_BARS)?,
            ema_slow: Ema::new(EMA_SLOW_BARS)?,
            macd_fast: Ema::new(MACD_FAST_BARS)?,
            macd_slow: Ema::new(MACD_SLOW_BARS)?,
            macd_signal: Ema::new(MACD_SIGNAL_BARS)?,
            previous_spread_sign: None,
            crossed_close_ns: None,
        })
    }

    /// Discard every retained sample, bar and smoothed state: the observations
    /// before a break are not continuous with those after it.
    pub(crate) fn clear(&mut self) {
        self.samples.clear();
        self.bars.clear();
        self.partial = None;
        self.last_sample_ns = None;
        self.previous_close = None;
        self.bar_count = 0;
        self.atr.reset();
        self.gains.reset();
        self.losses.reset();
        self.ema_fast.reset();
        self.ema_slow.reset();
        self.macd_fast.reset();
        self.macd_slow.reset();
        self.macd_signal.reset();
        self.previous_spread_sign = None;
        self.crossed_close_ns = None;
    }

    /// Advance the engine by one decision-clock sample.
    ///
    /// `None` is an unusable observation — no fresh price for this scope — and
    /// is a continuity break, exactly as it is for the volatility ring. So is a
    /// non-advancing or two-second-or-wider gap between samples.
    pub(crate) fn update(
        &mut self,
        price: Option<Decimal>,
        now_ns: u64,
    ) -> Result<(), SpotSignalError> {
        let Some(price) = price else {
            self.clear();
            return Ok(());
        };
        if self.last_sample_ns.is_some_and(|last| {
            now_ns <= last || now_ns.saturating_sub(last) >= MISSED_OBSERVATION_NS
        }) {
            self.clear();
        }
        if self.samples.len() == MAX_SAMPLES {
            self.samples.pop_front();
        }
        self.samples.push_back(price);
        self.fold_bar(price, now_ns)?;
        self.last_sample_ns = Some(now_ns);
        Ok(())
    }

    /// Fold one sample into the bar series, closing the previous bar when the
    /// sample crosses a minute boundary.
    fn fold_bar(&mut self, price: Decimal, now_ns: u64) -> Result<(), SpotSignalError> {
        let minute = now_ns / BAR_INTERVAL_NS;
        match self.partial {
            Some(mut partial) if partial.minute == minute => {
                partial.high = partial.high.max(price);
                partial.low = partial.low.min(price);
                partial.close = price;
                self.partial = Some(partial);
            }
            Some(partial) => {
                // A bar that began mid-minute never saw its whole minute, so it
                // is discarded rather than closed (it is only ever the first bar
                // after a continuity break).
                if partial.aligned {
                    let close_ns = minute_end_ns(partial.minute)?;
                    self.close_bar(
                        Bar {
                            open: partial.open,
                            high: partial.high,
                            low: partial.low,
                            close: partial.close,
                            close_ns,
                        },
                        close_ns,
                    )?;
                }
                self.partial = Some(PartialBar {
                    minute,
                    open: price,
                    high: price,
                    low: price,
                    close: price,
                    // The new bar begins at the first sample after the previous
                    // minute's last, with no gap (a gap already cleared the
                    // engine), so it covers its whole minute.
                    aligned: minute == partial.minute.saturating_add(1),
                });
            }
            None => {
                self.partial = Some(PartialBar {
                    minute,
                    open: price,
                    high: price,
                    low: price,
                    close: price,
                    aligned: false,
                });
            }
        }
        Ok(())
    }

    /// Advance every bar-driven indicator by one closed bar.
    fn close_bar(&mut self, bar: Bar, close_ns: u64) -> Result<(), SpotSignalError> {
        if self.bars.len() == MAX_BARS {
            self.bars.pop_front();
        }
        self.bars.push_back(bar);
        self.bar_count = self.bar_count.saturating_add(1);

        // True range and the RSI's change are defined against the previous
        // close, so the first closed bar only establishes that baseline.
        if let Some(previous_close) = self.previous_close {
            let true_range = checked_sub(bar.high, bar.low)?
                .max(checked_sub(bar.high, previous_close)?.abs())
                .max(checked_sub(bar.low, previous_close)?.abs());
            self.atr.update(true_range)?;
            let change = checked_sub(bar.close, previous_close)?;
            self.gains.update(change.max(Decimal::ZERO))?;
            self.losses.update((-change).max(Decimal::ZERO))?;
        }
        self.previous_close = Some(bar.close);

        self.ema_fast.update(bar.close)?;
        self.ema_slow.update(bar.close)?;
        if let (Some(fast), Some(slow)) = (self.ema_fast.value(), self.ema_slow.value()) {
            let spread = checked_sub(fast, slow)?;
            let sign = sign_of(spread);
            if self
                .previous_spread_sign
                .is_some_and(|previous| previous != sign)
            {
                self.crossed_close_ns = Some(close_ns);
            }
            self.previous_spread_sign = Some(sign);
        }

        self.macd_fast.update(bar.close)?;
        self.macd_slow.update(bar.close)?;
        if let (Some(fast), Some(slow)) = (self.macd_fast.value(), self.macd_slow.value()) {
            self.macd_signal.update(checked_sub(fast, slow)?)?;
        }
        Ok(())
    }

    /// Closed contiguous bars behind the current state. The published readings
    /// carry it as `bar_count`; this accessor exists for the continuity and
    /// warm-up tests, which assert on the state rather than on a value map.
    #[cfg(test)]
    pub(crate) const fn bar_count(&self) -> u16 {
        self.bar_count
    }

    /// Contiguous one-second samples behind the current state.
    pub(crate) fn sample_count(&self) -> u16 {
        u16::try_from(self.samples.len()).unwrap_or(u16::MAX)
    }

    pub(crate) fn atr(&self) -> Result<Option<AtrReading>, SpotSignalError> {
        let (Some(atr), Some(close)) = (self.atr.value(), self.previous_close) else {
            return Ok(None);
        };
        if close <= Decimal::ZERO {
            return Ok(None);
        }
        Ok(Some(AtrReading {
            atr,
            atr_bps: checked_mul(checked_div(atr, close)?, ten_thousand())?,
            close,
            bar_count: self.bar_count,
        }))
    }

    pub(crate) fn rsi(&self) -> Result<Option<RsiReading>, SpotSignalError> {
        let (Some(gain), Some(loss)) = (self.gains.value(), self.losses.value()) else {
            return Ok(None);
        };
        let hundred = Decimal::from(100_u8);
        // A window with no losses has no defined relative strength; the standard
        // convention is an index of 100 (and of 0 when there are no gains).
        let rsi = if loss.is_zero() {
            if gain.is_zero() {
                // Every close identical: the standard convention reads neutral.
                Decimal::from(50_u8)
            } else {
                hundred
            }
        } else {
            let rs = checked_div(gain, loss)?;
            checked_sub(
                hundred,
                checked_div(hundred, checked_add(Decimal::ONE, rs)?)?,
            )?
        };
        Ok(Some(RsiReading {
            rsi_14: rsi,
            regime: rsi_regime(rsi),
            bar_count: self.bar_count,
        }))
    }

    pub(crate) fn roc(&self) -> Result<Option<RocReading>, SpotSignalError> {
        let mut values = [Decimal::ZERO; 3];
        for (slot, horizon) in values.iter_mut().zip(ROC_HORIZONS) {
            let Some(reading) = self.roc_over(horizon)? else {
                return Ok(None);
            };
            *slot = reading;
        }
        Ok(Some(RocReading {
            roc_bps_10s: values[0],
            roc_bps_1m: values[1],
            roc_bps_5m: values[2],
            sample_count: self.sample_count(),
        }))
    }

    /// The rate of change over `horizon` one-second samples, in basis points of
    /// the earlier sample, or `None` while the ring is shorter than the horizon.
    fn roc_over(&self, horizon: usize) -> Result<Option<Decimal>, SpotSignalError> {
        let Some(index) = self.samples.len().checked_sub(horizon + 1) else {
            return Ok(None);
        };
        let (Some(prior), Some(current)) = (self.samples.get(index), self.samples.back()) else {
            return Ok(None);
        };
        if *prior <= Decimal::ZERO {
            return Err(SpotSignalError::MarketValue(
                "rate-of-change denominator must be positive".to_owned(),
            ));
        }
        Ok(Some(checked_mul(
            checked_div(checked_sub(*current, *prior)?, *prior)?,
            ten_thousand(),
        )?))
    }

    pub(crate) fn ema_cross(&self) -> Result<Option<EmaCrossReading>, SpotSignalError> {
        let (Some(fast), Some(slow)) = (self.ema_fast.value(), self.ema_slow.value()) else {
            return Ok(None);
        };
        if slow <= Decimal::ZERO {
            return Ok(None);
        }
        let spread_bps = checked_mul(checked_div(checked_sub(fast, slow)?, slow)?, ten_thousand())?;
        Ok(Some(EmaCrossReading {
            ema_9: fast,
            ema_21: slow,
            spread_bps,
            state: cross_state(spread_bps),
            crossed_close_ns: self.crossed_close_ns,
            bar_count: self.bar_count,
        }))
    }

    pub(crate) fn macd(&self) -> Result<Option<MacdReading>, SpotSignalError> {
        let (Some(fast), Some(slow), Some(signal), Some(close)) = (
            self.macd_fast.value(),
            self.macd_slow.value(),
            self.macd_signal.value(),
            self.previous_close,
        ) else {
            return Ok(None);
        };
        if close <= Decimal::ZERO {
            return Ok(None);
        }
        let macd = checked_sub(fast, slow)?;
        let histogram = checked_sub(macd, signal)?;
        Ok(Some(MacdReading {
            macd_bps: to_bps(macd, close)?,
            signal_bps: to_bps(signal, close)?,
            histogram_bps: to_bps(histogram, close)?,
            bar_count: self.bar_count,
        }))
    }

    pub(crate) fn bollinger(&self) -> Result<Option<BollingerReading>, SpotSignalError> {
        if self.bars.len() < BOLLINGER_BARS {
            return Ok(None);
        }
        let closes: Vec<Decimal> = self.bars.iter().map(|bar| bar.close).collect();
        let middle = mean(&closes)?;
        if middle <= Decimal::ZERO {
            return Ok(None);
        }
        let sigma = population_standard_deviation(&closes, middle)?;
        let width = checked_mul(Decimal::from(BOLLINGER_SIGMA), sigma)?;
        let upper = checked_add(middle, width)?;
        let lower = checked_sub(middle, width)?;
        let close = *closes.last().ok_or_else(|| {
            SpotSignalError::State("a full Bollinger window has a latest close".to_owned())
        })?;
        // A zero-width band (twenty identical closes) puts the close exactly on
        // both bands; the conventional reading of percent_b there is the middle.
        let percent_b = if width.is_zero() {
            Decimal::new(5, 1)
        } else {
            checked_div(checked_sub(close, lower)?, checked_sub(upper, lower)?)?
        };
        let state = if close > upper {
            "above"
        } else if close < lower {
            "below"
        } else {
            "inside"
        };
        Ok(Some(BollingerReading {
            percent_b,
            bandwidth_bps: checked_mul(
                checked_div(checked_sub(upper, lower)?, middle)?,
                ten_thousand(),
            )?,
            state,
            bar_count: self.bar_count,
        }))
    }

    pub(crate) fn zscore(&self) -> Result<ZScoreOutcome, SpotSignalError> {
        // Warm-up is decided before flatness: a shorter window that happens to
        // be degenerate must not report `flat_window` while a longer one is
        // still filling, or the identity would claim a decided state it has not
        // reached.
        if self.samples.len() < MAX_SAMPLES {
            return Ok(ZScoreOutcome::Warming);
        }
        let mut values = [Decimal::ZERO; 3];
        for (slot, window) in values.iter_mut().zip(Z_WINDOWS) {
            match self.zscore_over(window)? {
                Some(Some(score)) => *slot = score,
                // The window is long enough but perfectly flat: no score exists.
                Some(None) => return Ok(ZScoreOutcome::FlatWindow),
                None => return Ok(ZScoreOutcome::Warming),
            }
        }
        Ok(ZScoreOutcome::Ready(ZScoreReading {
            z_1m: values[0],
            z_5m: values[1],
            z_15m: values[2],
            sample_count: self.sample_count(),
        }))
    }

    /// `Ok(None)` when the ring is shorter than the window, `Ok(Some(None))`
    /// when the window is flat (zero standard deviation).
    fn zscore_over(&self, window: usize) -> Result<Option<Option<Decimal>>, SpotSignalError> {
        if self.samples.len() < window {
            return Ok(None);
        }
        let values: Vec<Decimal> = self.samples.iter().rev().take(window).copied().collect();
        let mean_value = mean(&values)?;
        let sigma = population_standard_deviation(&values, mean_value)?;
        if sigma.is_zero() {
            return Ok(Some(None));
        }
        let current = *values.first().ok_or_else(|| {
            SpotSignalError::State("a full Z-score window has a latest sample".to_owned())
        })?;
        Ok(Some(Some(checked_div(
            checked_sub(current, mean_value)?,
            sigma,
        )?)))
    }
}

/// The logical instant one minute index ends: the start of the next minute.
fn minute_end_ns(minute: u64) -> Result<u64, SpotSignalError> {
    minute
        .checked_add(1)
        .and_then(|next| next.checked_mul(BAR_INTERVAL_NS))
        .ok_or_else(|| SpotSignalError::Arithmetic("bar close instant overflowed".to_owned()))
}

/// The RSI regime word for the research-default thirty / seventy boundaries.
fn rsi_regime(rsi: Decimal) -> &'static str {
    if rsi <= Decimal::from(30_u8) {
        "oversold"
    } else if rsi >= Decimal::from(70_u8) {
        "overbought"
    } else {
        "neutral"
    }
}

/// The EMA cross state word from the signed spread.
fn cross_state(spread_bps: Decimal) -> &'static str {
    if spread_bps.is_zero() {
        "flat"
    } else if spread_bps > Decimal::ZERO {
        "bullish"
    } else {
        "bearish"
    }
}

fn sign_of(value: Decimal) -> i8 {
    if value.is_zero() {
        0
    } else if value.is_sign_negative() {
        -1
    } else {
        1
    }
}

/// A price-unit value as basis points of `close`.
fn to_bps(value: Decimal, close: Decimal) -> Result<Decimal, SpotSignalError> {
    checked_mul(checked_div(value, close)?, ten_thousand())
}

fn decimal_count(count: usize) -> Result<Decimal, SpotSignalError> {
    Ok(Decimal::from(
        u32::try_from(count).map_err(|_| SpotSignalError::Counter)?,
    ))
}

fn mean(values: &[Decimal]) -> Result<Decimal, SpotSignalError> {
    if values.is_empty() {
        return Err(SpotSignalError::State(
            "mean requires at least one value".to_owned(),
        ));
    }
    let mut sum = Decimal::ZERO;
    for value in values {
        sum = checked_add(sum, *value)?;
    }
    checked_div(sum, decimal_count(values.len())?)
}

/// The population standard deviation of `values` about `mean_value` — the same
/// divisor (n, not n-1) the volatility unit uses for its root mean square.
fn population_standard_deviation(
    values: &[Decimal],
    mean_value: Decimal,
) -> Result<Decimal, SpotSignalError> {
    if values.is_empty() {
        return Err(SpotSignalError::State(
            "standard deviation requires at least one value".to_owned(),
        ));
    }
    let mut sum = Decimal::ZERO;
    for value in values {
        let deviation = checked_sub(*value, mean_value)?;
        sum = checked_add(sum, checked_mul(deviation, deviation)?)?;
    }
    decimal_sqrt(checked_div(sum, decimal_count(values.len())?)?)
}

// --- publication -----------------------------------------------------------

/// The seven TA identities, in publication order. One enum shared by every
/// scope so a scope cannot publish a different set or a different order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum TaKind {
    Atr,
    Rsi,
    Roc,
    EmaCross,
    Macd,
    Bollinger,
    ZScore,
}

impl TaKind {
    pub(crate) const ALL: [Self; 7] = [
        Self::Atr,
        Self::Rsi,
        Self::Roc,
        Self::EmaCross,
        Self::Macd,
        Self::Bollinger,
        Self::ZScore,
    ];
}

/// What one TA identity has to publish on a decision: a value map, or the
/// reason it has none. The caller supplies its own scope's warm-up reason word,
/// so the shared unit names no scope-specific vocabulary; `FlatWindow` is the
/// one reason the arithmetic itself decides (registry entry 119).
#[derive(Debug, Clone)]
pub(crate) enum TaOutcome {
    Ready(BTreeMap<String, Value>),
    Warming,
    FlatWindow,
}

/// The readiness reason a degenerate Z-score window publishes (E51, registry
/// entry 119): the window's standard deviation is exactly zero, so no score is
/// defined.
pub(crate) const FLAT_WINDOW_REASON: &str = "flat_window";

impl TaEngine {
    /// The published outcome of one TA identity on this decision.
    ///
    /// `wall_start` renders the EMA cross instant through the one shared
    /// [`logical_timestamp_text`], never a second offset calculation.
    pub(crate) fn outcome(
        &self,
        kind: TaKind,
        produced_at: &str,
        wall_start: DateTime<Utc>,
    ) -> Result<TaOutcome, SpotSignalError> {
        let values = match kind {
            TaKind::Atr => self.atr()?.map(|reading| {
                BTreeMap::from([
                    ("atr".to_owned(), decimal_value(reading.atr)),
                    ("atr_bps".to_owned(), decimal_value(reading.atr_bps)),
                    ("close".to_owned(), decimal_value(reading.close)),
                    ("bar_count".to_owned(), json!(reading.bar_count)),
                ])
            }),
            TaKind::Rsi => self.rsi()?.map(|reading| {
                BTreeMap::from([
                    ("rsi_14".to_owned(), decimal_value(reading.rsi_14)),
                    ("regime".to_owned(), json!(reading.regime)),
                    ("bar_count".to_owned(), json!(reading.bar_count)),
                ])
            }),
            TaKind::Roc => self.roc()?.map(|reading| {
                BTreeMap::from([
                    ("roc_bps_10s".to_owned(), decimal_value(reading.roc_bps_10s)),
                    ("roc_bps_1m".to_owned(), decimal_value(reading.roc_bps_1m)),
                    ("roc_bps_5m".to_owned(), decimal_value(reading.roc_bps_5m)),
                    ("sample_count".to_owned(), json!(reading.sample_count)),
                ])
            }),
            TaKind::EmaCross => {
                let reading = self.ema_cross()?;
                match reading {
                    Some(reading) => {
                        // No cross observed yet: `crossed_present` is false and
                        // `crossed_at` repeats the decision instant, carrying no
                        // cross information (the `_present` convention the
                        // resilience reading already uses).
                        let crossed_at = match reading.crossed_close_ns {
                            Some(close_ns) => logical_timestamp_text(wall_start, close_ns)?,
                            None => produced_at.to_owned(),
                        };
                        Some(BTreeMap::from([
                            ("ema_9".to_owned(), decimal_value(reading.ema_9)),
                            ("ema_21".to_owned(), decimal_value(reading.ema_21)),
                            ("spread_bps".to_owned(), decimal_value(reading.spread_bps)),
                            ("state".to_owned(), json!(reading.state)),
                            (
                                "crossed_present".to_owned(),
                                json!(reading.crossed_close_ns.is_some()),
                            ),
                            ("crossed_at".to_owned(), json!(crossed_at)),
                            ("bar_count".to_owned(), json!(reading.bar_count)),
                        ]))
                    }
                    None => None,
                }
            }
            TaKind::Macd => self.macd()?.map(|reading| {
                BTreeMap::from([
                    ("macd_bps".to_owned(), decimal_value(reading.macd_bps)),
                    ("signal_bps".to_owned(), decimal_value(reading.signal_bps)),
                    (
                        "histogram_bps".to_owned(),
                        decimal_value(reading.histogram_bps),
                    ),
                    ("bar_count".to_owned(), json!(reading.bar_count)),
                ])
            }),
            TaKind::Bollinger => self.bollinger()?.map(|reading| {
                BTreeMap::from([
                    ("percent_b".to_owned(), decimal_value(reading.percent_b)),
                    (
                        "bandwidth_bps".to_owned(),
                        decimal_value(reading.bandwidth_bps),
                    ),
                    ("state".to_owned(), json!(reading.state)),
                    ("bar_count".to_owned(), json!(reading.bar_count)),
                ])
            }),
            TaKind::ZScore => match self.zscore()? {
                ZScoreOutcome::Ready(reading) => Some(BTreeMap::from([
                    ("z_1m".to_owned(), decimal_value(reading.z_1m)),
                    ("z_5m".to_owned(), decimal_value(reading.z_5m)),
                    ("z_15m".to_owned(), decimal_value(reading.z_15m)),
                    ("sample_count".to_owned(), json!(reading.sample_count)),
                ])),
                ZScoreOutcome::FlatWindow => return Ok(TaOutcome::FlatWindow),
                ZScoreOutcome::Warming => None,
            },
        };
        Ok(match values {
            Some(mut values) => {
                values.insert("window_close".to_owned(), json!(produced_at));
                TaOutcome::Ready(values)
            }
            None => TaOutcome::Warming,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use signals_core::parse_decimal_text;

    fn dec(text: &str) -> Decimal {
        parse_decimal_text(text).expect("decimal parses")
    }

    fn ns(second: u64) -> u64 {
        second * ONE_SECOND_NS
    }

    const PRODUCED_AT: &str = "2026-08-13T12:40:00.000000000Z";

    fn wall_start() -> DateTime<Utc> {
        DateTime::UNIX_EPOCH
    }

    /// The known-answer bar path: forty closes of `100 + ((i * 7) mod 11) - 5`
    /// for i = 1..=40, i.e. 102, 98, 105, 101, 97, 104, 100, 96, 103, 99, 95,
    /// 102, ... — a repeating twenty-two-bar saw with both up and down closes,
    /// so no indicator is exercised on a degenerate monotone path.
    fn known_answer_closes(bars: usize) -> Vec<Decimal> {
        (1..=bars)
            .map(|index| Decimal::from(100_i64 + i64::try_from((index * 7) % 11).unwrap() - 5))
            .collect()
    }

    /// Drive `closes` through the engine as one-minute bars.
    ///
    /// Every second of a minute carries that bar's close, so open = high = low =
    /// close and the true range reduces to the absolute close-to-close change —
    /// which is what makes the ATR expectations hand-checkable. A priming minute
    /// runs first because a bar started mid-minute is discarded by construction,
    /// and one sample of the following minute closes the last bar.
    fn drive_bars(engine: &mut TaEngine, closes: &[Decimal]) {
        for second in 0..60 {
            engine
                .update(Some(closes[0]), ns(second))
                .expect("priming sample");
        }
        for (index, close) in closes.iter().enumerate() {
            let minute = u64::try_from(index + 1).expect("minute fits");
            for second in 0..60 {
                engine
                    .update(Some(*close), ns(minute * 60 + second))
                    .expect("bar sample");
            }
        }
        let next_minute = u64::try_from(closes.len() + 1).expect("minute fits");
        engine
            .update(Some(*closes.last().expect("a close")), ns(next_minute * 60))
            .expect("closing sample");
    }

    fn engine_over(bars: usize) -> TaEngine {
        let mut engine = TaEngine::new().expect("engine builds");
        drive_bars(&mut engine, &known_answer_closes(bars));
        engine
    }

    fn ready(engine: &TaEngine, kind: TaKind) -> BTreeMap<String, Value> {
        match engine
            .outcome(kind, PRODUCED_AT, wall_start())
            .expect("outcome computes")
        {
            TaOutcome::Ready(values) => values,
            other => panic!("{kind:?} is not ready: {other:?}"),
        }
    }

    fn is_warming(engine: &TaEngine, kind: TaKind) -> bool {
        matches!(
            engine
                .outcome(kind, PRODUCED_AT, wall_start())
                .expect("outcome computes"),
            TaOutcome::Warming
        )
    }

    /// Wilder's ATR(14) and RSI(14) (Wilder, *New Concepts in Technical Trading
    /// Systems*, 1978) over the fifteen- and twenty-bar prefixes of the saw.
    ///
    /// Expected values are computed independently of this implementation, in
    /// exact rational arithmetic over the same textbook recursions (the first
    /// average is the mean of the first fourteen inputs, then `previous +
    /// (input - previous) / 14`), and rounded to eight places half-to-even:
    ///
    /// | bars | ATR | ATR bps | RSI |
    /// | --- | --- | --- | --- |
    /// | 15 | 5.07142857 | 502.12164074 | 49.29577465 |
    /// | 20 | 5.12552792 | 497.62407 | 51.23826962 |
    /// | 40 | — | — | 49.63031434 |
    #[test]
    fn wilder_atr_and_rsi_match_the_hand_computed_recursions() {
        let fifteen = engine_over(15);
        let atr = ready(&fifteen, TaKind::Atr);
        assert_eq!(atr["atr"], json!("5.07142857"));
        assert_eq!(atr["atr_bps"], json!("502.12164074"));
        assert_eq!(atr["close"], json!("101"));
        assert_eq!(atr["bar_count"], json!(15));
        let rsi = ready(&fifteen, TaKind::Rsi);
        assert_eq!(rsi["rsi_14"], json!("49.29577465"));
        assert_eq!(rsi["regime"], json!("neutral"));

        let twenty = engine_over(20);
        assert_eq!(ready(&twenty, TaKind::Atr)["atr"], json!("5.12552792"));
        assert_eq!(ready(&twenty, TaKind::Atr)["atr_bps"], json!("497.62407"));
        assert_eq!(ready(&twenty, TaKind::Rsi)["rsi_14"], json!("51.23826962"));

        assert_eq!(
            ready(&engine_over(40), TaKind::Rsi)["rsi_14"],
            json!("49.63031434")
        );
    }

    /// EMA(9)/EMA(21) with alpha = 2 / (n + 1), each seeded with the mean of its
    /// first n closes, over the same saw. Independently computed in exact
    /// rational arithmetic:
    ///
    /// | bars | EMA(9) | EMA(21) | spread bps | state |
    /// | --- | --- | --- | --- | --- |
    /// | 21 | 100.06905316 | 100.23809524 | -16.86405529 | bearish |
    /// | 25 | 100.45228417 | 100.26063468 | 19.11512857 | bullish |
    #[test]
    fn the_ema_cross_matches_the_hand_computed_averages() {
        let twenty_one = engine_over(21);
        let values = ready(&twenty_one, TaKind::EmaCross);
        assert_eq!(values["ema_9"], json!("100.06905316"));
        assert_eq!(values["ema_21"], json!("100.23809524"));
        assert_eq!(values["spread_bps"], json!("-16.86405529"));
        assert_eq!(values["state"], json!("bearish"));
        // The first bar at which both averages exist cannot also be a cross:
        // there is no previous spread to change sign against.
        assert_eq!(values["crossed_present"], json!(false));
        assert_eq!(values["crossed_at"], json!(PRODUCED_AT));

        let twenty_five = engine_over(25);
        let values = ready(&twenty_five, TaKind::EmaCross);
        assert_eq!(values["spread_bps"], json!("19.11512857"));
        assert_eq!(values["state"], json!("bullish"));
        // The spread turned positive between bars 21 and 25, so the cross is
        // recorded at the close instant of the bar it was observed on.
        assert_eq!(values["crossed_present"], json!(true));
        assert_ne!(values["crossed_at"], json!(PRODUCED_AT));
    }

    /// MACD(12, 26, 9) published as basis points of the latest close (Appel's
    /// standard lengths). Independently computed in exact rational arithmetic:
    ///
    /// | bars | MACD bps | signal bps | histogram bps |
    /// | --- | --- | --- | --- |
    /// | 34 | -16.78554177 | -1.009252 | -15.77628977 |
    /// | 40 | 19.84840654 | 8.52278113 | 11.32562542 |
    #[test]
    fn the_macd_matches_the_hand_computed_averages() {
        let values = ready(&engine_over(34), TaKind::Macd);
        assert_eq!(values["macd_bps"], json!("-16.78554177"));
        assert_eq!(values["signal_bps"], json!("-1.009252"));
        assert_eq!(values["histogram_bps"], json!("-15.77628977"));

        let values = ready(&engine_over(40), TaKind::Macd);
        assert_eq!(values["macd_bps"], json!("19.84840654"));
        assert_eq!(values["signal_bps"], json!("8.52278113"));
        assert_eq!(values["histogram_bps"], json!("11.32562542"));
    }

    /// Bollinger's 20-bar, 2-sigma bands. At twenty bars the window is the first
    /// twenty closes of the saw, whose mean is exactly 100.3 and whose
    /// population standard deviation is exactly 3.1 — so the bands are 94.1 and
    /// 106.5, the latest close is 99, percent_b is (99 - 94.1) / 12.4 =
    /// 0.71774194 (8 dp) and bandwidth is 12.4 / 100.3 x 10 000 = 1236.29112662
    /// bps. The forty-bar row is the same computation over the last twenty
    /// closes, independently computed.
    #[test]
    fn the_bollinger_bands_match_the_hand_computed_window() {
        let values = ready(&engine_over(20), TaKind::Bollinger);
        assert_eq!(values["percent_b"], json!("0.71774194"));
        assert_eq!(values["bandwidth_bps"], json!("1236.29112662"));
        assert_eq!(values["state"], json!("inside"));
        assert_eq!(values["bar_count"], json!(20));

        let values = ready(&engine_over(40), TaKind::Bollinger);
        assert_eq!(values["percent_b"], json!("0.49599628"));
        assert_eq!(values["bandwidth_bps"], json!("1248.21535358"));
    }

    /// Every bar-driven indicator becomes ready on exactly the bar its warm-up
    /// names, and not one bar earlier: ATR and RSI at fifteen closed bars
    /// (fourteen changes), the EMA cross at twenty-one (the longer seed),
    /// Bollinger at twenty, and MACD at thirty-four (26 + 9 - 1).
    #[test]
    fn each_indicator_readies_on_its_warm_up_boundary() {
        for (kind, boundary) in [
            (TaKind::Atr, 15_usize),
            (TaKind::Rsi, 15),
            (TaKind::Bollinger, 20),
            (TaKind::EmaCross, 21),
            (TaKind::Macd, 34),
        ] {
            let before = engine_over(boundary - 1);
            assert!(
                is_warming(&before, kind),
                "{kind:?} must still warm at {} bars",
                boundary - 1
            );
            let at = engine_over(boundary);
            assert!(
                matches!(
                    at.outcome(kind, PRODUCED_AT, wall_start())
                        .expect("outcome computes"),
                    TaOutcome::Ready(_)
                ),
                "{kind:?} must be ready at {boundary} bars"
            );
        }
    }

    /// The one-second indicators over a linear ramp of 900 samples, price
    /// 100 + t/100 at second t. Independently computed in exact rational
    /// arithmetic: the rates of change are 9.18273646, 55.35055351 and
    /// 283.01886792 bps, and the Z-scores 1.7034199, 1.7262869 and 1.73012737.
    #[test]
    fn the_one_second_indicators_match_the_hand_computed_ramp() {
        let mut engine = TaEngine::new().expect("engine builds");
        for second in 1..=900_u64 {
            let price = checked_add(
                Decimal::from(100_u8),
                checked_div(Decimal::from(second), Decimal::from(100_u8)).expect("price"),
            )
            .expect("price");
            engine.update(Some(price), ns(second)).expect("sample");
        }
        let roc = ready(&engine, TaKind::Roc);
        assert_eq!(roc["roc_bps_10s"], json!("9.18273646"));
        assert_eq!(roc["roc_bps_1m"], json!("55.35055351"));
        assert_eq!(roc["roc_bps_5m"], json!("283.01886792"));
        assert_eq!(roc["sample_count"], json!(900));

        let zscore = ready(&engine, TaKind::ZScore);
        assert_eq!(zscore["z_1m"], json!("1.7034199"));
        assert_eq!(zscore["z_5m"], json!("1.7262869"));
        assert_eq!(zscore["z_15m"], json!("1.73012737"));
    }

    /// The one-second warm-up boundaries: the rate of change needs 301 samples
    /// (the five-minute horizon plus the current one) and the Z-score 900.
    #[test]
    fn the_one_second_indicators_ready_on_their_sample_boundaries() {
        let mut engine = TaEngine::new().expect("engine builds");
        let price = dec("100");
        for second in 1..=300_u64 {
            engine.update(Some(price), ns(second)).expect("sample");
        }
        assert!(is_warming(&engine, TaKind::Roc));
        engine.update(Some(price), ns(301)).expect("sample");
        assert!(matches!(
            engine
                .outcome(TaKind::Roc, PRODUCED_AT, wall_start())
                .expect("outcome computes"),
            TaOutcome::Ready(_)
        ));

        for second in 302..=899_u64 {
            engine.update(Some(price), ns(second)).expect("sample");
        }
        assert!(is_warming(&engine, TaKind::ZScore));
        engine.update(Some(price), ns(900)).expect("sample");
        // A perfectly flat ramp has zero standard deviation in every window, so
        // the boundary is reached as `flat_window`, not as a value.
        assert!(matches!(
            engine
                .outcome(TaKind::ZScore, PRODUCED_AT, wall_start())
                .expect("outcome computes"),
            TaOutcome::FlatWindow
        ));
    }

    /// A window with any movement is scored; a perfectly flat one is not.
    #[test]
    fn a_flat_window_has_no_z_score() {
        let mut engine = TaEngine::new().expect("engine builds");
        for second in 1..=900_u64 {
            engine.update(Some(dec("100")), ns(second)).expect("sample");
        }
        assert!(matches!(
            engine
                .outcome(TaKind::ZScore, PRODUCED_AT, wall_start())
                .expect("outcome computes"),
            TaOutcome::FlatWindow
        ));
        // Every window is trailing and includes the current sample, so one
        // moved sample makes all three non-degenerate at once and the identity
        // is scored on the very next decision.
        engine.update(Some(dec("100.01")), ns(901)).expect("sample");
        assert!(matches!(
            engine
                .outcome(TaKind::ZScore, PRODUCED_AT, wall_start())
                .expect("outcome computes"),
            TaOutcome::Ready(_)
        ));
    }

    /// An unusable sample and a two-second gap are both causal breaks: every
    /// retained bar, sample and smoothed state is discarded, and the indicators
    /// re-warm from scratch rather than bridging the gap.
    #[test]
    fn a_continuity_break_discards_every_retained_state() {
        for break_at in [BreakKind::MissingSample, BreakKind::Gap] {
            let closes = known_answer_closes(20);
            let mut engine = TaEngine::new().expect("engine builds");
            drive_bars(&mut engine, &closes);
            assert_eq!(engine.bar_count(), 20);
            assert!(matches!(
                engine
                    .outcome(TaKind::Atr, PRODUCED_AT, wall_start())
                    .expect("outcome computes"),
                TaOutcome::Ready(_)
            ));

            let resume_at = ns(21 * 60 + 5);
            match break_at {
                BreakKind::MissingSample => {
                    engine.update(None, resume_at).expect("missing sample");
                }
                BreakKind::Gap => {
                    // Two seconds after the last sample: a missed observation.
                    engine
                        .update(Some(closes[19]), ns(21 * 60 + 7))
                        .expect("late sample");
                }
            }
            assert_eq!(engine.bar_count(), 0);
            assert_eq!(engine.sample_count(), u16::from(break_at == BreakKind::Gap));
            for kind in TaKind::ALL {
                assert!(
                    is_warming(&engine, kind),
                    "{kind:?} must re-warm after a {break_at:?}"
                );
            }
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum BreakKind {
        MissingSample,
        Gap,
    }

    /// A bar that began mid-minute never saw its whole minute and is discarded
    /// rather than closed, so a partial first bar cannot enter any warm-up.
    #[test]
    fn a_bar_started_mid_minute_is_never_closed() {
        let mut engine = TaEngine::new().expect("engine builds");
        let price = dec("100");
        // Start at second 30 of minute 0 and run to the end of minute 1.
        for second in 30..120_u64 {
            engine.update(Some(price), ns(second)).expect("sample");
        }
        // Minute 0's bar was discarded; minute 1's closed.
        engine.update(Some(price), ns(120)).expect("sample");
        assert_eq!(engine.bar_count(), 1);
    }

    /// The pack is seven identities in one fixed order, shared by every scope:
    /// each lane maps this array onto its own identity strings, so a scope
    /// cannot publish a different set or a different publication order.
    #[test]
    fn the_pack_is_seven_identities_in_a_fixed_order() {
        assert_eq!(
            TaKind::ALL,
            [
                TaKind::Atr,
                TaKind::Rsi,
                TaKind::Roc,
                TaKind::EmaCross,
                TaKind::Macd,
                TaKind::Bollinger,
                TaKind::ZScore,
            ]
        );
    }
}
