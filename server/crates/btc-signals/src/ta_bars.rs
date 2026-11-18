//! Prediction-venue bar/indicator toolkit for the C3b pack (ruling E51).
//!
//! FUTURE-BUILDS: in this tree only the prediction scopes use this module. The
//! spot and perp price scopes compute their technical analysis through C3a's
//! `TaEngine` in `ta.rs`, a separate 60-second-bar implementation that shares no
//! types with this one (its `Bar`/`ZScoreOutcome` differ in shape). The two were
//! kept apart rather than force-unified during the wave-3 port because a single
//! `BarSeries`/`TaEngine` API is not a mechanical merge; folding them into one
//! module is deferred. `BarSeries` here is written to fold any `bar_seconds`, so
//! it is ready to host the price scopes when that unification happens.
//!
//! One place owns the bar construction and the textbook indicator arithmetic so
//! the prediction processors compute the same number from the same inputs.
//! Nothing here knows about venues, identities or readiness: a
//! caller feeds it one-second samples and reads back either a value or a
//! declared not-ready disposition, then maps that onto the frozen instance-status
//! vocabulary itself.
//!
//! Two sampling shapes, both causal and both driven only by admitted event time:
//!
//! * [`SampleRing`] — one sample per whole second of lane time (last write in the
//!   second wins, which is that second's close). Windowed reads require the whole
//!   window to be present: there is no gap bridging, so a feed silence makes the
//!   affected window unavailable rather than quietly widening it.
//! * [`BarSeries`] — bars folded from those one-second samples over a
//!   caller-chosen bar length (open = first sample, high/low = extrema, close =
//!   last). The prediction scopes fold ten-second bars so a Wilder RSI(14) can
//!   warm inside a single market generation (ruling E58); [`BAR_SECONDS`] (60) is
//!   the length the price scopes would pass once they move onto this type. A bar
//!   missing any of its seconds is invalid, and an invalid bar restarts the
//!   bar-based warm-ups rather than being bridged (spec §2).
//!
//! Arithmetic is exact `rust_decimal` throughout; only the caller's published
//! output rounds, to eight places half-even. No binary floating point enters any
//! path.

use std::collections::VecDeque;

use rust_decimal::Decimal;

use crate::decimal_math::{DecimalMathError, sqrt_fixed};

/// Nanoseconds in one second: the sampling quantum of every C3 identity.
pub const SECOND_NS: u64 = 1_000_000_000;
/// Seconds in one minute bar. Each [`BarSeries`] carries its own bar length, so
/// this is the value the price scopes would pass when they move onto this type,
/// not a global the type reads. The prediction scopes pass ten (E58).
pub const BAR_SECONDS: u64 = 60;
/// Wilder's default RSI period, a research default stated in every description.
pub const RSI_PERIOD: usize = 14;
/// Bars of history a Wilder RSI(14) needs before it publishes: fourteen deltas
/// take fifteen closes.
pub const RSI_WARMUP_BARS: usize = RSI_PERIOD + 1;

/// A failure of the shared indicator arithmetic. Overflow only: every
/// insufficient-history or degenerate-window case is a declared outcome, not an
/// error.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TaError {
    #[error("technical-indicator arithmetic overflowed: {0}")]
    Arithmetic(String),
}

impl From<DecimalMathError> for TaError {
    fn from(error: DecimalMathError) -> Self {
        Self::Arithmetic(error.to_string())
    }
}

fn add(left: Decimal, right: Decimal) -> Result<Decimal, TaError> {
    left.checked_add(right)
        .ok_or_else(|| TaError::Arithmetic("addition overflowed".to_owned()))
}

fn sub(left: Decimal, right: Decimal) -> Result<Decimal, TaError> {
    left.checked_sub(right)
        .ok_or_else(|| TaError::Arithmetic("subtraction overflowed".to_owned()))
}

fn mul(left: Decimal, right: Decimal) -> Result<Decimal, TaError> {
    left.checked_mul(right)
        .ok_or_else(|| TaError::Arithmetic("multiplication overflowed".to_owned()))
}

fn div(left: Decimal, right: Decimal) -> Result<Decimal, TaError> {
    left.checked_div(right)
        .ok_or_else(|| TaError::Arithmetic("division overflowed or divided by zero".to_owned()))
}

/// A rolling ring of one-second closes.
///
/// `observe` is idempotent within a second: the last value admitted in a second
/// is that second's close, exactly as a bar close is the last sample of its
/// minute. Samples older than the retention are evicted, so the ring is bounded
/// by construction. Time must not regress; an out-of-order sample older than the
/// newest second is ignored rather than reordered, so a replay of the same
/// ordered admissions reproduces the ring.
#[derive(Debug, Clone)]
pub struct SampleRing {
    retention_seconds: u64,
    samples: VecDeque<(u64, Decimal)>,
}

impl SampleRing {
    /// A ring retaining `retention_seconds` of one-second closes.
    #[must_use]
    pub fn new(retention_seconds: u64) -> Self {
        Self {
            retention_seconds,
            samples: VecDeque::new(),
        }
    }

    /// Fold one observation into its second bucket, then evict beyond retention.
    pub fn observe(&mut self, now_ns: u64, value: Decimal) {
        let second = now_ns / SECOND_NS;
        match self.samples.back_mut() {
            Some((last_second, last_value)) if *last_second == second => *last_value = value,
            // A sample older than the newest retained second is out of order.
            // Ignoring it keeps the ring a function of the admitted order.
            Some((last_second, _)) if *last_second > second => return,
            _ => self.samples.push_back((second, value)),
        }
        let cutoff = second.saturating_sub(self.retention_seconds);
        while self
            .samples
            .front()
            .is_some_and(|(sample_second, _)| *sample_second < cutoff)
        {
            self.samples.pop_front();
        }
    }

    /// The newest retained `(second, close)`.
    #[must_use]
    pub fn latest(&self) -> Option<(u64, Decimal)> {
        self.samples.back().copied()
    }

    /// The close `seconds_back` whole seconds before the newest sample, or
    /// `None` when that exact second was never observed (no gap bridging).
    #[must_use]
    pub fn at_offset(&self, seconds_back: u64) -> Option<Decimal> {
        let (latest_second, _) = self.latest()?;
        let target = latest_second.checked_sub(seconds_back)?;
        self.samples
            .iter()
            .rev()
            .find(|(second, _)| *second == target)
            .map(|(_, value)| *value)
    }

    /// The complete `seconds`-long window ending at the newest sample, oldest
    /// first, or `None` when any second in it is missing.
    #[must_use]
    pub fn window(&self, seconds: u64) -> Option<Vec<Decimal>> {
        let (latest_second, _) = self.latest()?;
        let first = latest_second.checked_sub(seconds.checked_sub(1)?)?;
        let mut window = Vec::with_capacity(usize::try_from(seconds).ok()?);
        let mut expected = first;
        for (second, value) in self
            .samples
            .iter()
            .skip_while(|(second, _)| *second < first)
        {
            if *second != expected {
                return None;
            }
            window.push(*value);
            expected = expected.checked_add(1)?;
        }
        if window.len() == usize::try_from(seconds).ok()? {
            Some(window)
        } else {
            None
        }
    }

    /// Drop every retained sample (a market roll or a venue drop).
    pub fn clear(&mut self) {
        self.samples.clear();
    }

    /// Retained sample count.
    #[must_use]
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    /// Whether the ring holds no sample.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }
}

/// One closed bar of the series' bar length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bar {
    /// The bar's first second (Unix seconds), so a caller can stamp it.
    pub start_second: u64,
    pub open: Decimal,
    pub high: Decimal,
    pub low: Decimal,
    pub close: Decimal,
}

#[derive(Debug, Clone, Copy)]
struct PartialBar {
    start_second: u64,
    open: Decimal,
    high: Decimal,
    low: Decimal,
    close: Decimal,
}

/// Fixed-length bars folded from one-second samples, with the continuity rule.
///
/// The bar length is `bar_seconds`, chosen by the caller: 60 for the spot and
/// perp scopes, 10 for the prediction scopes (ruling E58). A bar is valid only
/// when every one of its `bar_seconds` seconds was observed. A missing second
/// invalidates the bar in progress and clears the closed history, so a bar-based
/// warm-up restarts from the next complete bar rather than bridging the gap
/// (spec §2). A series that starts mid-bar waits for the next bar boundary rather
/// than closing a short first bar.
#[derive(Debug, Clone)]
pub struct BarSeries {
    capacity: usize,
    bar_seconds: u64,
    last_second: Option<u64>,
    current: Option<PartialBar>,
    closed: VecDeque<Bar>,
}

impl BarSeries {
    /// A series retaining at most `capacity` closed `bar_seconds`-long bars.
    #[must_use]
    pub fn new(capacity: usize, bar_seconds: u64) -> Self {
        Self {
            capacity,
            bar_seconds,
            last_second: None,
            current: None,
            closed: VecDeque::new(),
        }
    }

    /// Fold one second's close into the bar in progress.
    ///
    /// Returns the bar this sample closed, if any. A sample that is not the
    /// immediate successor of the previous one breaks continuity: the partial
    /// bar and the closed history are both discarded, and bar-based warm-ups
    /// restart from the next bar boundary.
    pub fn observe(&mut self, second: u64, value: Decimal) -> Option<Bar> {
        let contiguous = self
            .last_second
            .is_some_and(|last| second.checked_sub(last) == Some(1));
        if !contiguous {
            self.current = None;
            self.closed.clear();
        }
        self.last_second = Some(second);
        let start = second - second % self.bar_seconds;
        match self.current.as_mut() {
            Some(current) => {
                current.high = current.high.max(value);
                current.low = current.low.min(value);
                current.close = value;
            }
            None => {
                // A bar only counts from its own boundary; a first sample inside
                // a minute waits for the next one.
                if start != second {
                    return None;
                }
                self.current = Some(PartialBar {
                    start_second: start,
                    open: value,
                    high: value,
                    low: value,
                    close: value,
                });
            }
        }
        if second % self.bar_seconds != self.bar_seconds - 1 {
            return None;
        }
        let finished = self.current.take()?;
        let bar = Bar {
            start_second: finished.start_second,
            open: finished.open,
            high: finished.high,
            low: finished.low,
            close: finished.close,
        };
        self.closed.push_back(bar);
        while self.closed.len() > self.capacity {
            self.closed.pop_front();
        }
        Some(bar)
    }

    /// The retained closed bars, oldest first.
    #[must_use]
    pub fn closed(&self) -> &VecDeque<Bar> {
        &self.closed
    }

    /// The retained closed-bar closes, oldest first.
    #[must_use]
    pub fn closes(&self) -> Vec<Decimal> {
        self.closed.iter().map(|bar| bar.close).collect()
    }

    /// Drop the bar in progress and every closed bar.
    pub fn clear(&mut self) {
        self.last_second = None;
        self.current = None;
        self.closed.clear();
    }
}

/// Wilder's RSI over the trailing `period` deltas of `closes` (oldest first).
///
/// `None` while fewer than `period + 1` closes are retained. A window with no
/// downward delta is RSI 100 by the standard convention (the denominator is the
/// average loss, and zero average loss is unbounded relative strength); a window
/// with no upward delta is 0.
pub fn wilder_rsi(closes: &[Decimal], period: usize) -> Result<Option<Decimal>, TaError> {
    if period == 0 || closes.len() < period + 1 {
        return Ok(None);
    }
    let period_decimal = Decimal::from(u64::try_from(period).unwrap_or(u64::MAX));
    // Seed the two Wilder averages on the first `period` deltas, then smooth
    // across every remaining delta: avg = (avg * (period - 1) + delta) / period.
    let mut gain_sum = Decimal::ZERO;
    let mut loss_sum = Decimal::ZERO;
    for index in 1..=period {
        let delta = sub(closes[index], closes[index - 1])?;
        if delta > Decimal::ZERO {
            gain_sum = add(gain_sum, delta)?;
        } else {
            loss_sum = add(loss_sum, -delta)?;
        }
    }
    let mut average_gain = div(gain_sum, period_decimal)?;
    let mut average_loss = div(loss_sum, period_decimal)?;
    let smoothing = sub(period_decimal, Decimal::ONE)?;
    for index in period + 1..closes.len() {
        let delta = sub(closes[index], closes[index - 1])?;
        let (gain, loss) = if delta > Decimal::ZERO {
            (delta, Decimal::ZERO)
        } else {
            (Decimal::ZERO, -delta)
        };
        average_gain = div(add(mul(average_gain, smoothing)?, gain)?, period_decimal)?;
        average_loss = div(add(mul(average_loss, smoothing)?, loss)?, period_decimal)?;
    }
    let hundred = Decimal::from(100);
    if average_loss.is_zero() {
        return Ok(Some(if average_gain.is_zero() {
            // A perfectly flat window has no relative strength either way; the
            // textbook midpoint is the only non-arbitrary answer.
            Decimal::from(50)
        } else {
            hundred
        }));
    }
    let relative_strength = div(average_gain, average_loss)?;
    let denominator = add(Decimal::ONE, relative_strength)?;
    Ok(Some(sub(hundred, div(hundred, denominator)?)?))
}

/// Wilder's RSI regime bands. Thresholds are research defaults (30 / 70).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RsiRegime {
    Oversold,
    Neutral,
    Overbought,
}

impl RsiRegime {
    /// The published `regime` enum variant.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Oversold => "oversold",
            Self::Neutral => "neutral",
            Self::Overbought => "overbought",
        }
    }

    /// Classify an RSI reading against the research-default bands: oversold at
    /// or below 30, overbought at or above 70.
    #[must_use]
    pub fn classify(rsi: Decimal, oversold: Decimal, overbought: Decimal) -> Self {
        if rsi <= oversold {
            Self::Oversold
        } else if rsi >= overbought {
            Self::Overbought
        } else {
            Self::Neutral
        }
    }
}

/// Rate of change as a percent of the prior value: `(now / prior - 1) * 100`.
///
/// `None` when the prior value is zero, which has no percent change.
pub fn roc_percent(now: Decimal, prior: Decimal) -> Result<Option<Decimal>, TaError> {
    if prior.is_zero() {
        return Ok(None);
    }
    let change = sub(now, prior)?;
    Ok(Some(mul(div(change, prior)?, Decimal::from(100))?))
}

/// Rate of change in basis points of the prior value.
pub fn roc_bps(now: Decimal, prior: Decimal) -> Result<Option<Decimal>, TaError> {
    if prior.is_zero() {
        return Ok(None);
    }
    let change = sub(now, prior)?;
    Ok(Some(mul(div(change, prior)?, Decimal::from(10_000))?))
}

/// The arithmetic mean of a non-empty sample window.
pub fn mean(values: &[Decimal]) -> Result<Option<Decimal>, TaError> {
    if values.is_empty() {
        return Ok(None);
    }
    let mut total = Decimal::ZERO;
    for value in values {
        total = add(total, *value)?;
    }
    let count = Decimal::from(u64::try_from(values.len()).unwrap_or(u64::MAX));
    Ok(Some(div(total, count)?))
}

/// The population standard deviation of a sample window about `mean_value`.
///
/// Population rather than sample: the window is the whole observed window, not a
/// draw from a larger one, and the population form is what a fixed-length
/// z-score band conventionally uses.
pub fn population_sigma(
    values: &[Decimal],
    mean_value: Decimal,
) -> Result<Option<Decimal>, TaError> {
    if values.is_empty() {
        return Ok(None);
    }
    let mut total = Decimal::ZERO;
    for value in values {
        let deviation = sub(*value, mean_value)?;
        total = add(total, mul(deviation, deviation)?)?;
    }
    let count = Decimal::from(u64::try_from(values.len()).unwrap_or(u64::MAX));
    Ok(Some(sqrt_fixed(div(total, count)?)?))
}

/// One z-score evaluation: a value, or the reason there is none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ZScoreOutcome {
    Ready {
        z: Decimal,
        mean: Decimal,
        sigma: Decimal,
    },
    /// The window is complete but has zero dispersion, so the z-score is
    /// undefined rather than large (spec §1 row 7).
    FlatWindow,
    /// The window is incomplete: fewer samples than it declares, or a gap in it.
    Insufficient,
}

/// The z-score of `current` against a complete sample window.
///
/// The window is the trailing window *including* the current sample, which is
/// what makes the statistic a causal fold over exactly the samples published so
/// far.
pub fn zscore(window: &[Decimal], current: Decimal) -> Result<ZScoreOutcome, TaError> {
    let Some(mean_value) = mean(window)? else {
        return Ok(ZScoreOutcome::Insufficient);
    };
    let Some(sigma) = population_sigma(window, mean_value)? else {
        return Ok(ZScoreOutcome::Insufficient);
    };
    if sigma.is_zero() {
        return Ok(ZScoreOutcome::FlatWindow);
    }
    Ok(ZScoreOutcome::Ready {
        z: div(sub(current, mean_value)?, sigma)?,
        mean: mean_value,
        sigma,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dec(text: &str) -> Decimal {
        text.parse().expect("decimal literal")
    }

    fn ns(second: u64) -> u64 {
        second * SECOND_NS
    }

    #[test]
    fn a_ring_keeps_one_close_per_second_and_evicts_beyond_retention() {
        let mut ring = SampleRing::new(3);
        ring.observe(ns(10), dec("1"));
        // A second write inside the same second is that second's close.
        ring.observe(ns(10) + 999, dec("2"));
        assert_eq!(ring.len(), 1);
        assert_eq!(ring.latest(), Some((10, dec("2"))));
        for second in 11..=14 {
            ring.observe(ns(second), Decimal::from(second));
        }
        // Retention 3 keeps seconds 11..=14 (cutoff = 14 - 3).
        assert_eq!(ring.len(), 4);
        assert_eq!(ring.at_offset(3), Some(dec("11")));
        assert_eq!(ring.at_offset(4), None);
    }

    #[test]
    fn an_out_of_order_sample_is_ignored_rather_than_reordered() {
        let mut ring = SampleRing::new(60);
        ring.observe(ns(10), dec("1"));
        ring.observe(ns(12), dec("3"));
        ring.observe(ns(11), dec("2"));
        assert_eq!(ring.latest(), Some((12, dec("3"))));
        assert_eq!(ring.len(), 2);
    }

    #[test]
    fn a_window_with_a_missing_second_is_unavailable_rather_than_bridged() {
        let mut ring = SampleRing::new(60);
        ring.observe(ns(1), dec("1"));
        ring.observe(ns(2), dec("2"));
        // Second 3 is missing.
        ring.observe(ns(4), dec("4"));
        assert_eq!(ring.window(2), None);
        assert_eq!(ring.window(3), None);
        assert_eq!(ring.window(1), Some(vec![dec("4")]));
    }

    #[test]
    fn a_bar_closes_only_on_sixty_complete_seconds() {
        let mut series = BarSeries::new(4, BAR_SECONDS);
        let mut closed = None;
        for second in 60..120 {
            closed = series.observe(second, Decimal::from(second));
        }
        let bar = closed.expect("the sixtieth sample closes the bar");
        assert_eq!(bar.start_second, 60);
        assert_eq!(bar.open, dec("60"));
        assert_eq!(bar.high, dec("119"));
        assert_eq!(bar.low, dec("60"));
        assert_eq!(bar.close, dec("119"));
        assert_eq!(series.closed().len(), 1);
    }

    #[test]
    fn a_missing_second_invalidates_the_bar_and_restarts_the_warm_up() {
        let mut series = BarSeries::new(4, BAR_SECONDS);
        for second in 60..120 {
            series.observe(second, Decimal::from(second));
        }
        assert_eq!(series.closed().len(), 1);
        // A gap inside the next bar clears the closed history.
        series.observe(120, dec("1"));
        series.observe(122, dec("1"));
        assert_eq!(series.closed().len(), 0);
        assert!(series.closes().is_empty());
    }

    #[test]
    fn wilder_rsi_matches_a_hand_computed_synthetic_series() {
        // A hand-computed synthetic series, not a published one: fifteen closes
        // built from eleven +0.6 moves and three -0.7 moves (step 3, 7 and 11 are
        // the losses), so exactly the seed window of fourteen deltas is exercised
        // with no smoothing beyond it. The expected value below is derived by hand
        // from those deltas, not quoted from a source.
        let mut closes = vec![dec("100")];
        for step in 0..14 {
            let previous = *closes.last().expect("seeded");
            let next = if step % 4 == 3 {
                previous - dec("0.7")
            } else {
                previous + dec("0.6")
            };
            closes.push(next);
        }
        let rsi = wilder_rsi(&closes, RSI_PERIOD)
            .expect("no overflow")
            .expect("fifteen closes");
        // sum gains = 11 * 0.6 = 6.6, sum losses = 3 * 0.7 = 2.1
        // RS = (6.6/14) / (2.1/14) = 6.6 / 2.1 = 3.142857142857...
        // RSI = 100 - 100/(1+RS) = 75.86206896551724...
        assert_eq!(rsi.round_dp(8), dec("75.86206897"));
        assert_eq!(
            RsiRegime::classify(rsi, dec("30"), dec("70")),
            RsiRegime::Overbought
        );
    }

    #[test]
    fn a_ten_second_bar_closes_on_ten_complete_seconds() {
        // The prediction scopes fold ten-second bars (ruling E58): a bar starts
        // on a multiple of ten and closes on the second ending in nine.
        let mut series = BarSeries::new(4, 10);
        let mut closed = None;
        for second in 20..30 {
            closed = series.observe(second, Decimal::from(second));
        }
        let bar = closed.expect("the tenth sample closes the bar");
        assert_eq!(bar.start_second, 20);
        assert_eq!(bar.open, dec("20"));
        assert_eq!(bar.close, dec("29"));
        assert_eq!(series.closed().len(), 1);
    }

    #[test]
    fn rsi_is_unavailable_below_the_warm_up_and_saturates_on_one_sided_windows() {
        let closes: Vec<Decimal> = (0..RSI_PERIOD).map(Decimal::from).collect();
        assert_eq!(wilder_rsi(&closes, RSI_PERIOD).expect("no overflow"), None);
        let rising: Vec<Decimal> = (0..=RSI_PERIOD).map(Decimal::from).collect();
        assert_eq!(
            wilder_rsi(&rising, RSI_PERIOD).expect("no overflow"),
            Some(Decimal::from(100))
        );
        let falling: Vec<Decimal> = (0..=RSI_PERIOD)
            .map(|step| Decimal::from(100 - step))
            .collect();
        assert_eq!(
            wilder_rsi(&falling, RSI_PERIOD).expect("no overflow"),
            Some(Decimal::ZERO)
        );
        let flat = vec![dec("0.55"); RSI_PERIOD + 1];
        assert_eq!(
            wilder_rsi(&flat, RSI_PERIOD).expect("no overflow"),
            Some(Decimal::from(50))
        );
    }

    #[test]
    fn roc_is_percent_and_bps_of_the_prior_value() {
        assert_eq!(
            roc_percent(dec("0.55"), dec("0.50")).expect("no overflow"),
            Some(dec("10"))
        );
        assert_eq!(
            roc_bps(dec("0.55"), dec("0.50")).expect("no overflow"),
            Some(dec("1000"))
        );
        assert_eq!(
            roc_percent(dec("0.55"), Decimal::ZERO).expect("no overflow"),
            None
        );
    }

    #[test]
    fn zscore_is_the_population_form_and_a_flat_window_is_declared() {
        // Window 1..5: mean 3, population sigma sqrt(2) = 1.41421356...
        let window = vec![dec("1"), dec("2"), dec("3"), dec("4"), dec("5")];
        let ZScoreOutcome::Ready { z, mean, sigma } =
            zscore(&window, dec("5")).expect("no overflow")
        else {
            panic!("a dispersed window is ready");
        };
        assert_eq!(mean, dec("3"));
        assert_eq!(sigma.round_dp(8), dec("1.41421356"));
        assert_eq!(z.round_dp(8), dec("1.41421356"));
        assert_eq!(
            zscore(&[dec("0.55"); 4], dec("0.55")).expect("no overflow"),
            ZScoreOutcome::FlatWindow
        );
        assert_eq!(
            zscore(&[], dec("1")).expect("no overflow"),
            ZScoreOutcome::Insufficient
        );
    }
}
