//! Update-rate counters behind the `tick_velocity` identity (C3 row 8).
//!
//! Three event streams are counted per scope — admitted quote updates, admitted
//! trades, and changes of the quoted midpoint — over a 1 s and a 10 s trailing
//! window, plus a trailing five-minute baseline rate the 10 s rate is compared
//! against. Nothing here reads a wall clock: every count is a fold over admitted
//! event times, so a replay of the same ordered admissions reproduces every
//! published number.
//!
//! The counters are rolling market-relative state, like the realized-volatility
//! ring: they are deliberately not checkpointed and re-warm after a restart, and
//! they are cleared whenever the scope they describe is torn down (a venue drop
//! on the spot lanes, a market roll on the prediction lanes).

use std::collections::VecDeque;

use rust_decimal::Decimal;

/// One second in nanoseconds.
const SECOND_NS: u64 = 1_000_000_000;
/// The fast window: one second.
pub const FAST_WINDOW_NS: u64 = SECOND_NS;
/// The published window: ten seconds.
pub const TEN_SECOND_WINDOW_NS: u64 = 10 * SECOND_NS;
/// The trailing baseline the 10 s rate is compared against: five minutes. A
/// research default, stated in every `tick_velocity` description.
pub const BASELINE_WINDOW_NS: u64 = 300 * SECOND_NS;
/// How much observation the identity needs before it publishes: one full 10 s
/// window, so the headline counts are complete rather than partial.
pub const WARMUP_NS: u64 = TEN_SECOND_WINDOW_NS;
/// Hard bound on retained event times per stream. Five minutes of a 2 kHz feed;
/// past it the oldest events are evicted, which can only shrink the baseline
/// denominator, never the 1 s or 10 s counts.
const MAX_RETAINED_EVENTS: usize = 600_000;

/// One counted event stream: the times of the events still inside the baseline
/// window.
#[derive(Debug, Clone, Default)]
struct EventStream {
    times: VecDeque<u64>,
}

impl EventStream {
    fn observe(&mut self, now_ns: u64) {
        // Out-of-order admissions are ignored rather than reordered, so the
        // deque stays sorted and the windowed counts stay a function of the
        // admitted order.
        if self.times.back().is_some_and(|last| *last > now_ns) {
            return;
        }
        self.times.push_back(now_ns);
        while self.times.len() > MAX_RETAINED_EVENTS {
            self.times.pop_front();
        }
    }

    fn prune(&mut self, now_ns: u64) {
        let cutoff = now_ns.saturating_sub(BASELINE_WINDOW_NS);
        while self.times.front().is_some_and(|time| *time < cutoff) {
            self.times.pop_front();
        }
    }

    fn count_within(&self, now_ns: u64, window_ns: u64) -> u32 {
        // Half-open window (now - window, now]: the lower edge is excluded so a
        // window of length w over a steady rate r counts exactly r*w events, not
        // r*w + 1. Counting both edges would bias every rate high (a constant 1/s
        // stream would report a velocity_ratio above one).
        let cutoff = now_ns.saturating_sub(window_ns);
        let count = self
            .times
            .iter()
            .rev()
            .take_while(|time| **time > cutoff)
            .count();
        u32::try_from(count).unwrap_or(u32::MAX)
    }

    fn clear(&mut self) {
        self.times.clear();
    }
}

/// One `tick_velocity` reading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TickVelocityReading {
    pub quote_updates_1s: u32,
    pub quote_updates_10s: u32,
    pub trades_1s: u32,
    pub trades_10s: u32,
    pub midpoint_changes_10s: u32,
    /// Whether a baseline rate existed to divide by. False leaves
    /// `velocity_ratio` at zero rather than fabricating one, exactly as
    /// `book_resilience` carries `recovery_ms_present`.
    pub velocity_ratio_present: bool,
    /// The 10 s quote-update rate divided by the trailing five-minute mean
    /// quote-update rate. One means the venue is updating at its recent average.
    pub velocity_ratio: Decimal,
    /// Quote updates inside the trailing baseline window, published so a
    /// consumer can see the denominator the ratio was formed from.
    pub baseline_quote_updates: u32,
    /// The observed span of the baseline window in milliseconds, capped at the
    /// baseline length. Below the full five minutes the ratio is against a
    /// shorter observed history, and this field says so.
    pub baseline_window_ms: u64,
}

/// The three counted streams of one scope.
#[derive(Debug, Clone, Default)]
pub struct TickVelocity {
    quotes: EventStream,
    trades: EventStream,
    midpoint_changes: EventStream,
    last_midpoint: Option<Decimal>,
    first_event_ns: Option<u64>,
}

impl TickVelocity {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Count one admitted quote update, and a midpoint change when the quoted
    /// midpoint differs from the previous admitted one.
    pub fn observe_quote(&mut self, now_ns: u64, midpoint: Decimal) {
        self.note_time(now_ns);
        self.quotes.observe(now_ns);
        if self
            .last_midpoint
            .is_some_and(|previous| previous != midpoint)
        {
            self.midpoint_changes.observe(now_ns);
        }
        self.last_midpoint = Some(midpoint);
    }

    /// Count one admitted trade.
    pub fn observe_trade(&mut self, now_ns: u64) {
        self.note_time(now_ns);
        self.trades.observe(now_ns);
    }

    fn note_time(&mut self, now_ns: u64) {
        if self.first_event_ns.is_none() {
            self.first_event_ns = Some(now_ns);
        }
    }

    /// Whether enough lane time has been observed for the 10 s counts to be
    /// complete. Measured from the first admitted event to `now_ns`, so a scope
    /// that has seen nothing is never warmed.
    #[must_use]
    pub fn warmed(&self, now_ns: u64) -> bool {
        self.first_event_ns
            .is_some_and(|first| now_ns.saturating_sub(first) >= WARMUP_NS)
    }

    /// Evict events past the baseline window and read the counts at `now_ns`.
    pub fn reading(&mut self, now_ns: u64) -> TickVelocityReading {
        self.quotes.prune(now_ns);
        self.trades.prune(now_ns);
        self.midpoint_changes.prune(now_ns);
        let quote_updates_10s = self.quotes.count_within(now_ns, TEN_SECOND_WINDOW_NS);
        // The baseline window is the observed span, capped at five minutes, taken
        // half-open (first_event, now] exactly like the numerator's 10 s window
        // (ruling E58). Counting the baseline over the fixed five-minute window
        // while measuring the span from the first event counted one extra event
        // (the span's lower endpoint) over that span, so a steady rate divided to
        // just under one before the cap; taking both over the same half-open span
        // makes a steady stream read exactly one at every span.
        let observed_ns = self
            .first_event_ns
            .map_or(0, |first| now_ns.saturating_sub(first))
            .min(BASELINE_WINDOW_NS);
        let baseline_quote_updates = self.quotes.count_within(now_ns, observed_ns);
        // rate_10s / rate_baseline = (n_10s / 10 s) / (n_base / observed span).
        // Formed as one exact ratio rather than two divisions so nothing is
        // rounded before the comparison.
        let (velocity_ratio_present, velocity_ratio) =
            if baseline_quote_updates == 0 || observed_ns == 0 {
                (false, Decimal::ZERO)
            } else {
                let numerator = Decimal::from(quote_updates_10s)
                    .checked_mul(Decimal::from(observed_ns))
                    .unwrap_or(Decimal::MAX);
                let denominator = Decimal::from(baseline_quote_updates)
                    .checked_mul(Decimal::from(TEN_SECOND_WINDOW_NS))
                    .unwrap_or(Decimal::MAX);
                numerator
                    .checked_div(denominator)
                    .map_or((false, Decimal::ZERO), |ratio| (true, ratio))
            };
        TickVelocityReading {
            quote_updates_1s: self.quotes.count_within(now_ns, FAST_WINDOW_NS),
            quote_updates_10s,
            trades_1s: self.trades.count_within(now_ns, FAST_WINDOW_NS),
            trades_10s: self.trades.count_within(now_ns, TEN_SECOND_WINDOW_NS),
            midpoint_changes_10s: self
                .midpoint_changes
                .count_within(now_ns, TEN_SECOND_WINDOW_NS),
            velocity_ratio_present,
            velocity_ratio,
            baseline_quote_updates,
            baseline_window_ms: observed_ns / 1_000_000,
        }
    }

    /// Drop every counted event and the midpoint baseline.
    pub fn clear(&mut self) {
        self.quotes.clear();
        self.trades.clear();
        self.midpoint_changes.clear();
        self.last_midpoint = None;
        self.first_event_ns = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dec(text: &str) -> Decimal {
        text.parse().expect("decimal literal")
    }

    fn ns(seconds: u64) -> u64 {
        seconds * SECOND_NS
    }

    #[test]
    fn counts_are_windowed_and_midpoint_changes_need_a_different_midpoint() {
        let mut velocity = TickVelocity::new();
        // Eleven quotes at seconds 0..=10; the midpoint alternates every second,
        // so each quote after the first differs from its predecessor and all ten
        // of those are midpoint changes. Windows are half-open (now - w, now], so
        // the 1 s window at now = 10 s holds only the second-10 quote and the 10 s
        // window holds seconds 1..=10.
        for second in 0..11 {
            let midpoint = if second % 2 == 0 {
                dec("0.50")
            } else {
                dec("0.51")
            };
            velocity.observe_quote(ns(second), midpoint);
        }
        velocity.observe_trade(ns(10));
        velocity.observe_trade(ns(10));
        let reading = velocity.reading(ns(10));
        assert_eq!(reading.quote_updates_1s, 1);
        assert_eq!(reading.quote_updates_10s, 10);
        assert_eq!(reading.trades_1s, 2);
        assert_eq!(reading.trades_10s, 2);
        assert_eq!(reading.midpoint_changes_10s, 10);
        assert!(velocity.warmed(ns(10)));
    }

    #[test]
    fn the_velocity_ratio_is_the_ten_second_rate_over_the_observed_baseline_rate() {
        let mut velocity = TickVelocity::new();
        // 301 quotes, one per second at seconds 0..=300. With half-open windows
        // the 10 s window holds seconds 291..=300 (ten) and the 300 s baseline
        // window holds seconds 1..=300 (three hundred): a steady 1/s stream, so
        // the ten-second rate equals the baseline rate exactly.
        for second in 0..=300 {
            velocity.observe_quote(ns(second), dec("0.5"));
        }
        let reading = velocity.reading(ns(300));
        assert_eq!(reading.quote_updates_10s, 10);
        assert_eq!(reading.baseline_quote_updates, 300);
        assert!(reading.velocity_ratio_present);
        // (10 / 10 s) / (300 / 300 s) = 1 exactly.
        assert_eq!(reading.velocity_ratio, dec("1"));
    }

    #[test]
    fn a_steady_stream_reads_exactly_one_at_a_sixty_second_span() {
        // Ruling E58: before the five-minute cap the baseline window is the
        // observed span taken half-open, so a steady 1 quote/s stream divides to
        // exactly one at a sixty-second span rather than the 60/61 a closed lower
        // edge gave. Quotes at seconds 1..=61, read at second 61: the 10 s window
        // holds seconds 52..=61 (ten) and the baseline the half-open span
        // (1, 61] = seconds 2..=61 (sixty) over sixty seconds.
        let mut velocity = TickVelocity::new();
        for second in 1..=61 {
            velocity.observe_quote(ns(second), dec("0.5"));
        }
        let reading = velocity.reading(ns(61));
        assert_eq!(reading.quote_updates_10s, 10);
        assert_eq!(reading.baseline_quote_updates, 60);
        assert_eq!(reading.baseline_window_ms, 60_000);
        assert!(reading.velocity_ratio_present);
        assert_eq!(reading.velocity_ratio, dec("1"));
    }

    #[test]
    fn no_baseline_leaves_the_ratio_absent_rather_than_fabricated() {
        let mut velocity = TickVelocity::new();
        velocity.observe_trade(ns(1));
        let reading = velocity.reading(ns(20));
        assert!(!reading.velocity_ratio_present);
        assert_eq!(reading.velocity_ratio, Decimal::ZERO);
        assert_eq!(reading.quote_updates_10s, 0);
    }

    #[test]
    fn warm_up_needs_ten_seconds_of_observation_and_clear_restarts_it() {
        let mut velocity = TickVelocity::new();
        assert!(!velocity.warmed(ns(100)));
        velocity.observe_quote(ns(0), dec("0.5"));
        assert!(!velocity.warmed(ns(9)));
        assert!(velocity.warmed(ns(10)));
        velocity.clear();
        assert!(!velocity.warmed(ns(100)));
    }
}
