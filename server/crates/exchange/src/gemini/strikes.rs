//! What the round driver needs to know between REST reads, kept from
//! the public socket: each window's strike as soon as Gemini sets it, the
//! index price second by second, and how far this host's clock is from
//! Gemini's.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rust_decimal::Decimal;
use tokio::sync::{Notify, broadcast};
use tokio::time::Instant;

use super::types::WsEvent;

/// Strikes older than this are dropped.
const STRIKE_KEEP_MS: u64 = 6 * 60 * 60 * 1000;
/// Seconds of index history kept: two hours.
const TAPE_KEEP_SECS: u64 = 2 * 60 * 60;
/// Clock samples the offset is taken over.
const CLOCK_SAMPLES: usize = 32;
/// An offset beyond this is worth a warning and a line on the health route.
pub const CLOCK_WARN_MS: i64 = 2_000;

/// Event ticker to strike, with when it was first seen. Fed by the
/// socket's contract-status messages and by REST reads.
#[derive(Debug, Clone, Default)]
pub struct StrikeBook {
    inner: Arc<StrikeInner>,
}

#[derive(Debug, Default)]
struct StrikeInner {
    strikes: Mutex<HashMap<String, (Decimal, u64)>>,
    changed: Notify,
}

impl StrikeBook {
    pub fn new() -> Self {
        Self::default()
    }

    /// Remember a window's strike. The first value seen stands: Gemini
    /// sets a strike once.
    pub fn record(&self, event_ticker: &str, strike: Decimal, seen_ms: u64) {
        {
            let mut strikes = self.inner.strikes.lock().expect("strike book poisoned");
            match strikes.get(event_ticker) {
                Some((known, _)) if *known != strike => {
                    tracing::warn!(event_ticker, %known, %strike, "gemini gave a second, different strike; keeping the first");
                    return;
                }
                Some(_) => return,
                None => {}
            }
            strikes.retain(|_, (_, at)| at.saturating_add(STRIKE_KEEP_MS) >= seen_ms);
            strikes.insert(event_ticker.to_owned(), (strike, seen_ms));
        }
        self.inner.changed.notify_waiters();
    }

    pub fn get(&self, event_ticker: &str) -> Option<Decimal> {
        self.inner.strikes.lock().expect("strike book poisoned").get(event_ticker).map(|(s, _)| *s)
    }

    /// When the strike was first seen, in the caller's milliseconds.
    pub fn first_seen_ms(&self, event_ticker: &str) -> Option<u64> {
        self.inner.strikes.lock().expect("strike book poisoned").get(event_ticker).map(|(_, at)| *at)
    }

    /// The strike, waiting up to `timeout` for it to arrive.
    pub async fn wait_for(&self, event_ticker: &str, timeout: Duration) -> Option<Decimal> {
        let wait = async {
            loop {
                // Register before checking, so a strike recorded between
                // the check and the wait is not missed.
                let changed = self.inner.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if let Some(strike) = self.get(event_ticker) {
                    return strike;
                }
                changed.await;
            }
        };
        tokio::time::timeout(timeout, wait).await.ok()
    }
}

/// The index price at each second, as the socket reported it.
#[derive(Debug, Clone, Default)]
pub struct IndexTape {
    prices: Arc<Mutex<BTreeMap<u64, f64>>>,
}

impl IndexTape {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&self, unix_second: u64, price: f64) {
        let mut prices = self.prices.lock().expect("index tape poisoned");
        prices.insert(unix_second, price);
        let newest = prices.keys().next_back().copied().unwrap_or(unix_second);
        let oldest_kept = newest.saturating_sub(TAPE_KEEP_SECS);
        while prices.first_key_value().is_some_and(|(second, _)| *second < oldest_kept) {
            prices.pop_first();
        }
    }

    /// The price stamped with exactly this second, if one arrived.
    pub fn at_second(&self, unix_second: u64) -> Option<f64> {
        self.prices.lock().expect("index tape poisoned").get(&unix_second).copied()
    }

    pub fn latest(&self) -> Option<(u64, f64)> {
        self.prices.lock().expect("index tape poisoned").iter().next_back().map(|(s, p)| (*s, *p))
    }
}

/// Gemini's time, estimated from the timestamps on its messages.
///
/// Every observed message gives `gemini_stamp − local_receipt`, which is
/// the clock offset less the network delay. The largest of the recent
/// samples is the one with the least delay, so that is the offset.
/// Local time is a wall-clock reading taken once, advanced by Tokio's
/// monotonic clock, so tests under paused time are exact.
#[derive(Debug, Clone)]
pub struct GeminiClock {
    inner: Arc<ClockInner>,
}

#[derive(Debug)]
struct ClockInner {
    base_ms: u64,
    started: Instant,
    samples: Mutex<VecDeque<i64>>,
    offset_ms: AtomicI64,
}

impl GeminiClock {
    /// A clock whose local time reads `local_now_ms` at this instant.
    pub fn starting_at(local_now_ms: u64) -> Self {
        Self {
            inner: Arc::new(ClockInner {
                base_ms: local_now_ms,
                started: Instant::now(),
                samples: Mutex::new(VecDeque::with_capacity(CLOCK_SAMPLES)),
                offset_ms: AtomicI64::new(0),
            }),
        }
    }

    /// A clock started from the system clock.
    pub fn system() -> Self {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        Self::starting_at(now)
    }

    /// This host's time, uncorrected.
    pub fn local_ms(&self) -> u64 {
        self.inner.base_ms + self.inner.started.elapsed().as_millis() as u64
    }

    /// A message stamped `gemini_ms` by Gemini has just arrived.
    pub fn observe(&self, gemini_ms: u64) {
        let sample = gemini_ms as i64 - self.local_ms() as i64;
        let mut samples = self.inner.samples.lock().expect("clock samples poisoned");
        if samples.len() == CLOCK_SAMPLES {
            samples.pop_front();
        }
        samples.push_back(sample);
        let offset = samples.iter().copied().max().unwrap_or(0);
        let before = self.inner.offset_ms.swap(offset, Ordering::Relaxed);
        if offset.abs() > CLOCK_WARN_MS && before.abs() <= CLOCK_WARN_MS {
            tracing::warn!(offset_ms = offset, "this host's clock is more than 2 s from gemini's");
        }
    }

    /// Gemini's clock minus this host's, in milliseconds. Zero until a
    /// message has been observed.
    pub fn offset_ms(&self) -> i64 {
        self.inner.offset_ms.load(Ordering::Relaxed)
    }

    /// The time by Gemini's clock.
    pub fn now_ms(&self) -> u64 {
        self.local_ms().saturating_add_signed(self.offset_ms())
    }

    /// Sleep until Gemini's clock reads `gemini_ms`.
    pub async fn sleep_until_ms(&self, gemini_ms: u64) {
        // Re-read after each sleep: the offset may have moved meanwhile.
        loop {
            let now = self.now_ms();
            if now >= gemini_ms {
                return;
            }
            tokio::time::sleep(Duration::from_millis(gemini_ms - now)).await;
        }
    }
}

/// Feed the strike book, the index tape and the clock from the socket's
/// events until the socket is gone. `index` is Gemini's index id, e.g.
/// `LRRR_XBT`.
pub fn spawn_reference_listener(
    mut events: broadcast::Receiver<WsEvent>,
    strikes: StrikeBook,
    tape: IndexTape,
    clock: GeminiClock,
    index: &'static str,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(WsEvent::ContractStatus { symbol, event_ticker, strike, ts_ms, .. }) => {
                    // Stamped in milliseconds by Gemini as it happens: a
                    // clock sample. (Index prices are stamped in whole
                    // seconds and would bias the offset.)
                    clock.observe(ts_ms);
                    // Only an event's Up contract has the event's strike;
                    // a ladder's contracts each carry their own.
                    if let Some(strike) = strike
                        && symbol == format!("GEMI-{event_ticker}-UP")
                    {
                        strikes.record(&event_ticker, strike, ts_ms);
                    }
                }
                Ok(WsEvent::Index { index: name, price, ts_ms }) if name == index => {
                    tape.record(ts_ms / 1000, price);
                }
                Ok(_) => {}
                // Fell behind: the events are gone, carry on with the next.
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    tracing::warn!(missed, "gemini reference listener fell behind");
                }
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn d(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }

    #[test]
    fn a_recorded_strike_is_read_back_with_when_it_was_seen() {
        let book = StrikeBook::new();
        assert_eq!(book.get("BTC05M2611181720"), None);
        book.record("BTC05M2611181720", d("62597.88"), 1_000);
        assert_eq!(book.get("BTC05M2611181720"), Some(d("62597.88")));
        assert_eq!(book.first_seen_ms("BTC05M2611181720"), Some(1_000));
    }

    #[test]
    fn the_first_strike_seen_for_a_window_stands() {
        let book = StrikeBook::new();
        book.record("T", d("100.10"), 1_000);
        book.record("T", d("100.20"), 2_000);
        assert_eq!(book.get("T"), Some(d("100.10")));
        assert_eq!(book.first_seen_ms("T"), Some(1_000));
    }

    #[test]
    fn strikes_older_than_six_hours_are_dropped() {
        let book = StrikeBook::new();
        book.record("OLD", d("1"), 1_000);
        book.record("NEW", d("2"), 1_000 + STRIKE_KEEP_MS + 1);
        assert_eq!(book.get("OLD"), None);
        assert_eq!(book.get("NEW"), Some(d("2")));
    }

    #[tokio::test(start_paused = true)]
    async fn waiting_returns_at_once_when_the_strike_is_known() {
        let book = StrikeBook::new();
        book.record("T", d("5"), 0);
        let before = Instant::now();
        assert_eq!(book.wait_for("T", Duration::from_secs(20)).await, Some(d("5")));
        assert_eq!(Instant::now(), before);
    }

    #[tokio::test(start_paused = true)]
    async fn waiting_wakes_when_the_strike_arrives() {
        let book = StrikeBook::new();
        let writer = book.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(4)).await;
            writer.record("OTHER", d("1"), 0);
            tokio::time::sleep(Duration::from_secs(1)).await;
            writer.record("T", d("62801.74"), 0);
        });
        let before = Instant::now();
        assert_eq!(book.wait_for("T", Duration::from_secs(20)).await, Some(d("62801.74")));
        assert_eq!(Instant::now() - before, Duration::from_secs(5));
    }

    #[tokio::test(start_paused = true)]
    async fn waiting_gives_up_after_the_timeout() {
        let book = StrikeBook::new();
        let before = Instant::now();
        assert_eq!(book.wait_for("T", Duration::from_secs(20)).await, None);
        assert_eq!(Instant::now() - before, Duration::from_secs(20));
    }

    #[test]
    fn the_tape_answers_for_the_exact_second_only() {
        let tape = IndexTape::new();
        tape.record(1_795_023_000, 63_276.216);
        tape.record(1_795_023_001, 63_275.9);
        assert_eq!(tape.at_second(1_795_023_000), Some(63_276.216));
        assert_eq!(tape.at_second(1_795_022_999), None);
        assert_eq!(tape.latest(), Some((1_795_023_001, 63_275.9)));
    }

    #[test]
    fn the_tape_keeps_two_hours() {
        let tape = IndexTape::new();
        tape.record(1_000, 1.0);
        tape.record(1_000 + TAPE_KEEP_SECS, 2.0);
        assert_eq!(tape.at_second(1_000), Some(1.0), "exactly two hours back is kept");
        tape.record(1_001 + TAPE_KEEP_SECS, 3.0);
        assert_eq!(tape.at_second(1_000), None);
    }

    #[tokio::test(start_paused = true)]
    async fn with_nothing_observed_geminis_time_is_local_time() {
        let clock = GeminiClock::starting_at(1_000_000);
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(clock.local_ms(), 1_000_250);
        assert_eq!(clock.now_ms(), 1_000_250);
        assert_eq!(clock.offset_ms(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_host_five_seconds_fast_reads_geminis_time_five_seconds_back() {
        // Local says 1,000,000; Gemini's stamps say it is 5 s earlier.
        let clock = GeminiClock::starting_at(1_000_000);
        clock.observe(995_000 - 40); // 40 ms in transit
        clock.observe(995_000 - 12); // 12 ms in transit: the better sample
        clock.observe(995_000 - 90);
        assert_eq!(clock.offset_ms(), -5_012);
        assert_eq!(clock.now_ms(), 1_000_000 - 5_012);
    }

    #[tokio::test(start_paused = true)]
    async fn old_samples_age_out_so_the_offset_can_move() {
        let clock = GeminiClock::starting_at(1_000_000);
        clock.observe(1_000_000 + 3_000);
        for _ in 0..CLOCK_SAMPLES {
            clock.observe(1_000_000 - 20);
        }
        assert_eq!(clock.offset_ms(), -20);
    }

    #[tokio::test(start_paused = true)]
    async fn sleeping_until_a_gemini_time_allows_for_the_offset() {
        let clock = GeminiClock::starting_at(1_000_000);
        clock.observe(995_000); // host is 5 s fast
        let before = Instant::now();
        // Gemini's 1,000,000 is 5 s away by Gemini's clock.
        clock.sleep_until_ms(1_000_000).await;
        assert_eq!(Instant::now() - before, Duration::from_secs(5));
        // A time already past returns at once.
        let before = Instant::now();
        clock.sleep_until_ms(900_000).await;
        assert_eq!(Instant::now(), before);
    }

    #[tokio::test(start_paused = true)]
    async fn the_listener_files_strikes_index_prices_and_clock_samples() {
        let (tx, rx) = broadcast::channel(16);
        let (strikes, tape, clock) = (StrikeBook::new(), IndexTape::new(), GeminiClock::starting_at(2_000_000));
        let task = spawn_reference_listener(rx, strikes.clone(), tape.clone(), clock.clone(), "LRRR_XBT");
        tx.send(WsEvent::ContractStatus {
            symbol: "GEMI-BTC05M2611181710-UP".into(),
            event_ticker: "BTC05M2611181710".into(),
            strike: Some(d("62801.74")),
            new_status: "Active".into(),
            ts_ms: 2_000_000 - 3_000,
        })
        .unwrap();
        tx.send(WsEvent::ContractStatus {
            symbol: "GEMI-X".into(),
            event_ticker: "X".into(),
            strike: None,
            new_status: "Approved".into(),
            ts_ms: 2_000_000 - 3_050,
        })
        .unwrap();
        // An hourly ladder has many contracts under one event, each with
        // its own strike: none of them is an Up strike.
        for (contract, strike) in [("HI68", "68"), ("HI68D5", "68.5")] {
            tx.send(WsEvent::ContractStatus {
                symbol: format!("GEMI-HYPE2611181900-{contract}"),
                event_ticker: "HYPE2611181900".into(),
                strike: Some(d(strike)),
                new_status: "Active".into(),
                ts_ms: 2_000_000 - 3_000,
            })
            .unwrap();
        }
        tx.send(WsEvent::Index { index: "LRRR_XBT".into(), price: 62_801.736_5, ts_ms: 1_795_021_500_000 }).unwrap();
        tx.send(WsEvent::Index { index: "LRRR_ETH".into(), price: 3_000.0, ts_ms: 1_795_021_501_000 }).unwrap();
        drop(tx);
        task.await.unwrap();
        assert_eq!(strikes.get("BTC05M2611181710"), Some(d("62801.74")));
        assert_eq!(strikes.first_seen_ms("BTC05M2611181710"), Some(2_000_000 - 3_000));
        assert_eq!(strikes.get("X"), None);
        assert_eq!(strikes.get("HYPE2611181900"), None, "a ladder contract's strike is not an Up strike");
        assert_eq!(tape.at_second(1_795_021_500), Some(62_801.736_5));
        assert_eq!(tape.at_second(1_795_021_501), None, "another asset's index is not this tape's");
        assert_eq!(clock.offset_ms(), -3_000);
    }
}
