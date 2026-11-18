//! Rounds that follow Gemini's rolling Up windows.
//!
//! A round here is one Gemini window: it opens and closes at Gemini's
//! times, on Gemini's strike, and resolves on Gemini's own published
//! result. The driver never decides any of the three.
//!
//! * **Which window.** Windows sit on fixed boundaries (every 5 or 15
//!   minutes, UTC) and a window's ticker follows from its series and
//!   expiry, so the driver computes the live window's ticker from
//!   Gemini's time and asks for that event.
//! * **Opening.** A round opens once its strike is known: from the
//!   socket (about five seconds into the window) or from a REST read.
//!   Until then, and whenever Gemini lists no such window, the series
//!   has no open round and says it is waiting on Gemini.
//! * **Resolving.** At the close the round takes no more orders. Five
//!   seconds on, the driver starts asking for Gemini's result and
//!   resolves on it. If none has come twenty seconds after the close,
//!   it resolves on the contract's stated rule (the index price at the
//!   expiry second, at or above the strike) and marks the round
//!   provisional, for the audit to check against Gemini later. With no
//!   index price for that second either, it keeps waiting: it never
//!   resolves on a guess.

use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use serde::Serialize;

use super::audit::{AuditRow, RoundAudit};
use super::env::GeminiEnv;
use super::strikes::{GeminiClock, IndexTape, StrikeBook};
use super::types::{EventDetail, UpWindow, event_ticker};
use crate::engine::{Market, Outcome, RoundClose, RoundReference};
use crate::pricing::feed::PriceFeed;
use crate::scheduler::{RoundDriver, RoundPlan, RoundResolution};

/// A round is not opened with less than this left to run.
pub const MIN_REMAINING: Duration = Duration::from_secs(20);
/// How long after the close the first read of Gemini's result is made.
pub const RESULT_FIRST_READ: Duration = Duration::from_secs(5);
/// The gap between reads of the result after that.
pub const RESULT_READ_EVERY: Duration = Duration::from_secs(2);
/// With no result this long after the close, the round resolves
/// provisionally if the index price at expiry is known.
pub const PROVISIONAL_AFTER: Duration = Duration::from_secs(20);
/// The gap between reads once the provisional deadline has passed.
pub const LATE_READ_EVERY: Duration = Duration::from_secs(5);
/// The gap between looks for a window's strike in its first minute, and
/// after that.
pub const WINDOW_LOOK_EVERY: Duration = Duration::from_secs(5);
pub const WINDOW_LOOK_EVERY_LATE: Duration = Duration::from_secs(15);

/// Where the driver reads events. The real one is Gemini's REST API;
/// tests script one.
pub trait UpSource: Send + Sync + 'static {
    /// The event with this ticker. An error when Gemini has none, or
    /// cannot be reached: either way, ask again later.
    fn event(&self, event_ticker: &str) -> impl Future<Output = anyhow::Result<EventDetail>> + Send;
}

/// Gemini's public REST API as an [`UpSource`].
#[derive(Debug, Clone, Copy)]
pub struct RestSource(pub GeminiEnv);

impl UpSource for RestSource {
    async fn event(&self, event_ticker: &str) -> anyhow::Result<EventDetail> {
        super::rest::get_event(&self.0, event_ticker).await
    }
}

/// What a series is doing, for the API and the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// No round is open: waiting on Gemini for a window or its strike.
    WaitingForWindow,
    /// A round is open and trading.
    Open,
    /// The round has closed: waiting on Gemini for its result.
    WaitingForResult,
}

/// The driver's view of its series, shared with the API and the mirror.
#[derive(Debug, Clone)]
pub struct SeriesLink {
    inner: Arc<Mutex<(Phase, Option<UpWindow>)>>,
}

impl Default for SeriesLink {
    fn default() -> Self {
        Self { inner: Arc::new(Mutex::new((Phase::WaitingForWindow, None))) }
    }
}

impl SeriesLink {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn phase(&self) -> Phase {
        self.inner.lock().expect("series link poisoned").0
    }

    /// The Gemini window the open (or just closed) round follows.
    pub fn window(&self) -> Option<UpWindow> {
        self.inner.lock().expect("series link poisoned").1.clone()
    }

    pub(super) fn set(&self, phase: Phase, window: Option<UpWindow>) {
        *self.inner.lock().expect("series link poisoned") = (phase, window);
    }
}

/// Everything a [`GeminiUpDriver`] is built from.
pub struct DriverParts<S> {
    /// Gemini's series name: `"BTC05M"`, `"BTC15M"`.
    pub series: &'static str,
    /// The window length in seconds: 300, 900.
    pub window_secs: u64,
    pub source: S,
    pub strikes: StrikeBook,
    pub tape: IndexTape,
    pub clock: GeminiClock,
    pub link: SeriesLink,
    pub audit: RoundAudit,
    /// Told of each open, so the chart starts a new window. `None` in tests.
    pub feed: Option<Arc<PriceFeed>>,
}

pub struct GeminiUpDriver<S> {
    parts: DriverParts<S>,
    /// The window of the round that is open, with its exact strike.
    open: Option<(UpWindow, Decimal)>,
}

impl<S: UpSource> GeminiUpDriver<S> {
    pub fn new(parts: DriverParts<S>) -> Self {
        Self { parts, open: None }
    }

    fn window_ms(&self) -> u64 {
        self.parts.window_secs * 1000
    }

    /// The strike of the window expiring at `expires_at_ms`, and the
    /// window itself: from the socket's book if it is there, else from
    /// one read of the event. `None` when Gemini has no such window, has
    /// not set its strike, or describes a different window under that
    /// ticker.
    async fn strike_now(&self, expires_at_ms: u64) -> Option<(UpWindow, Decimal)> {
        let ticker = event_ticker(self.parts.series, expires_at_ms);
        let starts_at_ms = expires_at_ms - self.window_ms();
        if let Some(strike) = self.parts.strikes.get(&ticker) {
            let window = UpWindow {
                symbol: format!("GEMI-{ticker}-UP"),
                event_ticker: ticker,
                starts_at_ms,
                expires_at_ms,
                strike: Some(strike),
                market_state: "open".into(),
            };
            return Some((window, strike));
        }
        let detail = match self.parts.source.event(&ticker).await {
            Ok(detail) => detail,
            Err(error) => {
                tracing::debug!(%ticker, %error, "gemini has no such window yet");
                return None;
            }
        };
        let window = detail.window;
        if (window.starts_at_ms, window.expires_at_ms) != (starts_at_ms, expires_at_ms) {
            tracing::error!(
                %ticker,
                starts_at_ms = window.starts_at_ms,
                expires_at_ms = window.expires_at_ms,
                "gemini's event is not the window its ticker implies; not opening it",
            );
            return None;
        }
        let strike = window.strike?;
        self.parts.strikes.record(&ticker, strike, self.parts.clock.now_ms());
        Some((window, strike))
    }
}

/// `Up` iff the price is at or above the strike: the contract's rule.
fn at_or_above(price: f64, strike: Decimal) -> Option<bool> {
    Decimal::try_from(price).ok().map(|price| price >= strike)
}

impl<S: UpSource> RoundDriver for GeminiUpDriver<S> {
    async fn first_plan(&mut self) -> RoundPlan {
        self.open = None;
        self.parts.link.set(Phase::WaitingForWindow, None);
        let window_ms = self.window_ms();
        loop {
            let now = self.parts.clock.now_ms();
            let starts_at_ms = now - now % window_ms;
            let expires_at_ms = starts_at_ms + window_ms;
            if expires_at_ms - now < MIN_REMAINING.as_millis() as u64 {
                // Too little of this window is left; take the next.
                self.parts.clock.sleep_until_ms(expires_at_ms).await;
                continue;
            }
            if let Some((window, strike)) = self.strike_now(expires_at_ms).await {
                let Some(strike_btc_usd) = strike.to_f64() else {
                    tracing::error!(%strike, "gemini's strike does not fit a float; not opening");
                    self.parts.clock.sleep_until_ms(expires_at_ms).await;
                    continue;
                };
                self.parts.link.set(Phase::Open, Some(window.clone()));
                self.open = Some((window, strike));
                return RoundPlan {
                    reference: RoundReference::Btc { strike_btc_usd },
                    opens_at: starts_at_ms / 1000,
                    closes_at: expires_at_ms / 1000,
                };
            }
            // Wait for the socket to deliver the strike, or look again:
            // often in the window's first minute, then less so.
            let look_every =
                if now - starts_at_ms < 60_000 { WINDOW_LOOK_EVERY } else { WINDOW_LOOK_EVERY_LATE };
            let ticker = event_ticker(self.parts.series, expires_at_ms);
            self.parts.strikes.wait_for(&ticker, look_every).await;
        }
    }

    async fn await_resolution(&mut self, current: &Market) -> RoundResolution {
        let (window, strike) = self.open.clone().expect("await_resolution with no round opened by this driver");
        let expires_at_ms = window.expires_at_ms;
        if current.closes_at * 1000 != expires_at_ms {
            tracing::error!(
                market_closes_at = current.closes_at,
                window_expires_at_ms = expires_at_ms,
                "the engine's round is not the driver's window; resolving the driver's window",
            );
        }
        let clock = self.parts.clock.clone();
        clock.sleep_until_ms(expires_at_ms).await;
        self.parts.link.set(Phase::WaitingForResult, Some(window.clone()));

        let provisional_at_ms = expires_at_ms + PROVISIONAL_AFTER.as_millis() as u64;
        let mut next_read_ms = expires_at_ms + RESULT_FIRST_READ.as_millis() as u64;
        let mut warned = false;
        let (resolved_up, close_price, provisional, gemini_up) = loop {
            clock.sleep_until_ms(next_read_ms).await;
            match self.parts.source.event(&window.event_ticker).await {
                Ok(EventDetail { settled: Some(result), .. }) => {
                    let value = result.settlement_value.to_f64().unwrap_or(f64::NAN);
                    break (result.up_won, value, false, Some(result.up_won));
                }
                Ok(_) => {}
                Err(error) => tracing::warn!(ticker = %window.event_ticker, %error, "could not read gemini's result"),
            }
            let now = clock.now_ms();
            if now + RESULT_READ_EVERY.as_millis() as u64 <= provisional_at_ms {
                next_read_ms = now + RESULT_READ_EVERY.as_millis() as u64;
                continue;
            }
            // Gemini is late. Resolve on the contract's own rule if the
            // index price at the expiry second is known; else keep waiting.
            clock.sleep_until_ms(provisional_at_ms).await;
            let at_expiry = self.parts.tape.at_second(expires_at_ms / 1000);
            if let Some((price, up)) = at_expiry.and_then(|p| at_or_above(p, strike).map(|up| (p, up))) {
                tracing::warn!(
                    ticker = %window.event_ticker,
                    price,
                    %strike,
                    "gemini has published no result 20 s after the close; resolving provisionally on the index at expiry",
                );
                break (up, price, true, None);
            }
            if !warned {
                tracing::warn!(
                    ticker = %window.event_ticker,
                    "no result from gemini and no index price for the expiry second; waiting on gemini",
                );
                warned = true;
            }
            next_read_ms = clock.now_ms() + LATE_READ_EVERY.as_millis() as u64;
        };

        self.parts.audit.record(AuditRow {
            event_ticker: window.event_ticker.clone(),
            strike,
            resolved_up,
            provisional,
            close_price,
            gemini_up,
            index_at_expiry: self.parts.tape.at_second(expires_at_ms / 1000),
        });
        self.open = None;
        self.parts.link.set(Phase::WaitingForWindow, None);
        RoundResolution {
            outcome: if resolved_up { Outcome::Yes } else { Outcome::No },
            close: RoundClose::Gemini { close_price_btc_usd: close_price, provisional },
            // The next window's strike is seconds away; `first_plan`
            // waits for it, and for Gemini to list a window at all.
            next: None,
        }
    }

    fn on_opened(&mut self, market: &Market) {
        if let (Some(feed), Some(strike)) = (&self.parts.feed, market.strike_btc_usd()) {
            feed.reset_for_new_market_in(
                market.series,
                market.id.0,
                strike,
                market.opened_at * 1000,
                market.closes_at * 1000,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{MarketStatus, RoundReference};
    use crate::types::{MarketId, SeriesId};
    use std::collections::HashMap;
    use std::str::FromStr;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::time::Instant;

    /// 2026-11-18T17:25:00Z, a five-minute boundary.
    const S: u64 = 1_795_022_700_000;
    const W: u64 = 300_000;
    const E: u64 = S + W;

    fn d(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }

    fn window(expiry_ms: u64, strike: Option<&str>) -> UpWindow {
        let ticker = event_ticker("BTC05M", expiry_ms);
        UpWindow {
            symbol: format!("GEMI-{ticker}-UP"),
            event_ticker: ticker,
            starts_at_ms: expiry_ms - W,
            expires_at_ms: expiry_ms,
            strike: strike.map(d),
            market_state: "open".into(),
        }
    }

    fn live(expiry_ms: u64, strike: Option<&str>) -> EventDetail {
        EventDetail { window: window(expiry_ms, strike), settled: None }
    }

    fn settled(expiry_ms: u64, strike: &str, up_won: bool, value: &str) -> EventDetail {
        EventDetail {
            window: window(expiry_ms, Some(strike)),
            settled: Some(super::super::types::Settled {
                up_won,
                settlement_value: d(value),
                resolved_at_ms: expiry_ms + 5_600,
            }),
        }
    }

    /// Each ticker's versions of its event, with the Gemini time each
    /// becomes readable.
    type Script = HashMap<String, Vec<(u64, EventDetail)>>;

    /// A stand-in for Gemini: each ticker has versions of its event that
    /// become readable at given Gemini times. Unknown tickers and times
    /// before the first version are errors, as a 404 would be.
    #[derive(Clone)]
    struct Scripted {
        clock: GeminiClock,
        events: Arc<Mutex<Script>>,
        reads: Arc<AtomicUsize>,
        down: Arc<Mutex<Option<(u64, u64)>>>,
    }

    impl Scripted {
        fn new(clock: &GeminiClock) -> Self {
            Self {
                clock: clock.clone(),
                events: Arc::default(),
                reads: Arc::default(),
                down: Arc::default(),
            }
        }

        fn put(&self, readable_from_ms: u64, detail: EventDetail) -> &Self {
            self.events
                .lock()
                .unwrap()
                .entry(detail.window.event_ticker.clone())
                .or_default()
                .push((readable_from_ms, detail));
            self
        }

        /// Every read fails between these Gemini times.
        fn down_between(&self, from_ms: u64, to_ms: u64) {
            *self.down.lock().unwrap() = Some((from_ms, to_ms));
        }

        fn reads(&self) -> usize {
            self.reads.load(Ordering::SeqCst)
        }
    }

    impl UpSource for Scripted {
        async fn event(&self, event_ticker: &str) -> anyhow::Result<EventDetail> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            let now = self.clock.now_ms();
            if let Some((from, to)) = *self.down.lock().unwrap()
                && (from..to).contains(&now)
            {
                anyhow::bail!("gemini is unreachable");
            }
            self.events
                .lock()
                .unwrap()
                .get(event_ticker)
                .and_then(|versions| versions.iter().rev().find(|(from, _)| *from <= now))
                .map(|(_, detail)| detail.clone())
                .ok_or_else(|| anyhow::anyhow!("no event {event_ticker}"))
        }
    }

    struct Rig {
        driver: GeminiUpDriver<Scripted>,
        gemini: Scripted,
        clock: GeminiClock,
        strikes: StrikeBook,
        tape: IndexTape,
        link: SeriesLink,
        audit: RoundAudit,
    }

    /// A driver whose host clock reads `local_now_ms` now.
    fn rig(local_now_ms: u64) -> Rig {
        let clock = GeminiClock::starting_at(local_now_ms);
        let gemini = Scripted::new(&clock);
        let (strikes, tape, link, audit) = (StrikeBook::new(), IndexTape::new(), SeriesLink::new(), RoundAudit::new());
        let driver = GeminiUpDriver::new(DriverParts {
            series: "BTC05M",
            window_secs: 300,
            source: gemini.clone(),
            strikes: strikes.clone(),
            tape: tape.clone(),
            clock: clock.clone(),
            link: link.clone(),
            audit: audit.clone(),
            feed: None,
        });
        Rig { driver, gemini, clock, strikes, tape, link, audit }
    }

    /// The engine's market for a plan, as the scheduler would hand it back.
    fn market(plan: &RoundPlan) -> Market {
        Market {
            id: MarketId(1),
            series: SeriesId(0),
            reference: plan.reference,
            opened_at: plan.opens_at,
            closes_at: plan.closes_at,
            duration_secs: plan.duration_secs(),
            status: MarketStatus::Open,
        }
    }

    async fn within<T>(secs: u64, f: impl Future<Output = T>) -> Option<T> {
        tokio::time::timeout(Duration::from_secs(secs), f).await.ok()
    }

    fn strike_of(plan: &RoundPlan) -> f64 {
        let RoundReference::Btc { strike_btc_usd } = plan.reference else { panic!("not a BTC round") };
        strike_btc_usd
    }

    // ── Opening ──

    #[tokio::test(start_paused = true)]
    async fn a_round_opens_on_geminis_window_times_and_the_strike_from_the_socket() {
        let mut r = rig(S + 6_000);
        r.strikes.record(&event_ticker("BTC05M", E), d("63147.36"), S + 4_200);
        let plan = within(1, r.driver.first_plan()).await.expect("a plan at once");
        assert_eq!((plan.opens_at, plan.closes_at), (S / 1000, E / 1000));
        assert_eq!(strike_of(&plan), 63_147.36);
        assert_eq!(r.gemini.reads(), 0, "the socket had the strike; no REST read");
        assert_eq!(r.link.phase(), Phase::Open);
        assert_eq!(r.link.window().unwrap().symbol, "GEMI-BTC05M2611181730-UP");
    }

    #[tokio::test(start_paused = true)]
    async fn with_no_strike_from_the_socket_it_is_read_from_gemini() {
        let mut r = rig(S + 60_000);
        r.gemini.put(S + 5_000, live(E, Some("63147.36")));
        let plan = within(1, r.driver.first_plan()).await.expect("a plan at once");
        assert_eq!((plan.opens_at, plan.closes_at), (S / 1000, E / 1000));
        assert_eq!(strike_of(&plan), 63_147.36);
        assert_eq!(r.strikes.get("BTC05M2611181730"), Some(d("63147.36")), "and remembered");
    }

    #[tokio::test(start_paused = true)]
    async fn a_host_clock_five_seconds_fast_does_not_move_the_round() {
        // The host thinks it is S + 11 s; Gemini's stamps say S + 6 s.
        let mut r = rig(S + 11_000);
        r.clock.observe(S + 6_000);
        r.strikes.record(&event_ticker("BTC05M", E), d("63147.36"), S + 4_200);
        let plan = within(1, r.driver.first_plan()).await.unwrap();
        assert_eq!((plan.opens_at, plan.closes_at), (S / 1000, E / 1000));
        // And the round is not resolved before Gemini's close: Gemini's
        // result is readable from E + 5.6 s by Gemini's clock.
        r.gemini.put(E + 5_600, settled(E, "63147.36", true, "63276.216"));
        let started = Instant::now();
        let market = market(&plan);
        let resolution = within(400, r.driver.await_resolution(&market)).await.unwrap();
        let took = Instant::now() - started;
        // From Gemini's S + 6 s to at least E + 5.6 s.
        assert!(took >= Duration::from_millis(W - 6_000 + 5_600), "resolved after {took:?}");
        assert_eq!(resolution.outcome, Outcome::Yes);
    }

    #[tokio::test(start_paused = true)]
    async fn while_gemini_lists_no_window_no_round_opens() {
        let mut r = rig(S + 6_000);
        // Nothing for this window; the next one appears 10 s into it.
        r.gemini.put(E + 10_000, live(E + W, Some("63276.22")));
        assert!(within(200, r.driver.first_plan()).await.is_none(), "no plan in this window");
        assert_eq!(r.link.phase(), Phase::WaitingForWindow);
        assert_eq!(r.link.window(), None);
        let plan = within(200, r.driver.first_plan()).await.expect("the next window opens");
        assert_eq!((plan.opens_at, plan.closes_at), (E / 1000, (E + W) / 1000));
        assert_eq!(strike_of(&plan), 63_276.22);
    }

    #[tokio::test(start_paused = true)]
    async fn a_window_listed_without_a_strike_waits_for_it() {
        let mut r = rig(S + 1_000);
        r.gemini.put(S, live(E, None));
        let strikes = r.strikes.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(3_700)).await;
            strikes.record("BTC05M2611181730", d("63147.36"), S + 4_700);
        });
        let started = Instant::now();
        let plan = within(60, r.driver.first_plan()).await.expect("opens when the strike lands");
        assert_eq!(Instant::now() - started, Duration::from_millis(3_700), "woken by the socket, not the next poll");
        assert_eq!(strike_of(&plan), 63_147.36);
    }

    #[tokio::test(start_paused = true)]
    async fn a_window_with_under_twenty_seconds_left_is_skipped() {
        let mut r = rig(E - 15_000);
        r.strikes.record(&event_ticker("BTC05M", E), d("63147.36"), S + 4_200);
        r.strikes.record(&event_ticker("BTC05M", E + W), d("63276.22"), E + 4_000);
        let plan = within(60, r.driver.first_plan()).await.unwrap();
        assert_eq!((plan.opens_at, plan.closes_at), (E / 1000, (E + W) / 1000));
        assert!(r.clock.now_ms() >= E, "it waited for the next window to start");
    }

    #[tokio::test(start_paused = true)]
    async fn an_event_whose_times_are_not_the_expected_window_is_not_opened() {
        let mut r = rig(S + 6_000);
        let mut odd = live(E, Some("63147.36"));
        odd.window.starts_at_ms -= 60_000;
        r.gemini.put(S, odd);
        assert!(within(120, r.driver.first_plan()).await.is_none());
        assert_eq!(r.link.phase(), Phase::WaitingForWindow);
    }

    // ── Resolving ──

    /// A rig with the S..E round already open on `strike`.
    async fn open_round(strike: &str) -> (Rig, Market) {
        let mut r = rig(S + 6_000);
        r.strikes.record(&event_ticker("BTC05M", E), d(strike), S + 4_200);
        let plan = within(1, r.driver.first_plan()).await.expect("the round opens at once");
        let market = market(&plan);
        (r, market)
    }

    #[tokio::test(start_paused = true)]
    async fn a_round_resolves_on_geminis_own_result_and_not_before_the_close() {
        let (mut r, market) = open_round("63147.36").await;
        r.gemini.put(E + 5_600, settled(E, "63147.36", true, "63276.216"));
        let resolution = within(400, r.driver.await_resolution(&market)).await.unwrap();
        assert!(r.clock.now_ms() >= E + 5_600);
        assert!(r.clock.now_ms() <= E + 9_000, "soon after Gemini published");
        assert_eq!(resolution.outcome, Outcome::Yes);
        assert_eq!(resolution.close, RoundClose::Gemini { close_price_btc_usd: 63_276.216, provisional: false });
        assert_eq!(resolution.next, None, "the next round comes from first_plan");
        assert_eq!(r.link.phase(), Phase::WaitingForWindow);
    }

    #[tokio::test(start_paused = true)]
    async fn the_series_says_it_is_waiting_for_the_result_after_the_close() {
        let (mut r, market) = open_round("63147.36").await;
        assert!(within(301, r.driver.await_resolution(&market)).await.is_none());
        assert_eq!(r.link.phase(), Phase::WaitingForResult);
        assert_eq!(r.link.window().unwrap().event_ticker, "BTC05M2611181730");
    }

    #[tokio::test(start_paused = true)]
    async fn down_wins_when_gemini_says_no() {
        let (mut r, market) = open_round("63147.36").await;
        r.gemini.put(E + 5_600, settled(E, "63147.36", false, "63100.5"));
        let resolution = within(400, r.driver.await_resolution(&market)).await.unwrap();
        assert_eq!(resolution.outcome, Outcome::No);
    }

    #[tokio::test(start_paused = true)]
    async fn geminis_side_is_taken_even_where_a_rounded_comparison_would_differ() {
        // Settled half a cent under the strike: No, though the value
        // rounds to the strike.
        let (mut r, market) = open_round("62801.74").await;
        r.gemini.put(E + 5_600, settled(E, "62801.7365", false, "62801.7365"));
        let resolution = within(400, r.driver.await_resolution(&market)).await.unwrap();
        assert_eq!(resolution.outcome, Outcome::No);
    }

    #[tokio::test(start_paused = true)]
    async fn with_no_result_in_twenty_seconds_it_resolves_provisionally_on_the_index_at_expiry() {
        let (mut r, market) = open_round("63147.36").await;
        r.tape.record(E / 1000 - 1, 63_200.0);
        r.tape.record(E / 1000, 63_100.25);
        r.tape.record(E / 1000 + 1, 63_300.0);
        let resolution = within(400, r.driver.await_resolution(&market)).await.unwrap();
        assert!(r.clock.now_ms() >= E + 20_000);
        assert!(r.clock.now_ms() < E + 23_000);
        assert_eq!(resolution.outcome, Outcome::No, "the expiry second's price, not its neighbours'");
        assert_eq!(resolution.close, RoundClose::Gemini { close_price_btc_usd: 63_100.25, provisional: true });
    }

    #[tokio::test(start_paused = true)]
    async fn provisionally_up_wins_when_the_index_equals_the_strike_exactly() {
        let (mut r, market) = open_round("63147.36").await;
        r.tape.record(E / 1000, 63_147.36);
        let resolution = within(400, r.driver.await_resolution(&market)).await.unwrap();
        assert_eq!(resolution.outcome, Outcome::Yes);
    }

    #[tokio::test(start_paused = true)]
    async fn with_no_result_and_no_index_price_it_waits_and_never_guesses() {
        let (mut r, market) = open_round("63147.36").await;
        // Prices around the expiry second, none for it.
        r.tape.record(E / 1000 - 1, 63_200.0);
        r.tape.record(E / 1000 + 1, 63_300.0);
        // Gemini publishes two and a half hours late, as on 2026-11-18.
        r.gemini.put(E + 9_000_000, settled(E, "63147.36", true, "63276.216"));
        assert!(within(3_600, r.driver.await_resolution(&market)).await.is_none(), "still waiting an hour on");
        assert_eq!(r.link.phase(), Phase::WaitingForResult);
        let resolution = within(9_000, r.driver.await_resolution(&market)).await.expect("resolves when Gemini does");
        assert_eq!(resolution.close, RoundClose::Gemini { close_price_btc_usd: 63_276.216, provisional: false });
    }

    #[tokio::test(start_paused = true)]
    async fn gemini_being_unreachable_does_not_stop_a_provisional_result() {
        let (mut r, market) = open_round("63147.36").await;
        r.gemini.down_between(E, E + 600_000);
        r.tape.record(E / 1000, 63_500.0);
        let resolution = within(400, r.driver.await_resolution(&market)).await.unwrap();
        assert_eq!(resolution.outcome, Outcome::Yes);
        assert_eq!(resolution.close, RoundClose::Gemini { close_price_btc_usd: 63_500.0, provisional: true });
    }

    #[tokio::test(start_paused = true)]
    async fn reads_of_the_result_are_spaced_not_hammered() {
        let (mut r, market) = open_round("63147.36").await;
        r.tape.record(E / 1000, 63_500.0);
        let before = r.gemini.reads();
        within(400, r.driver.await_resolution(&market)).await.unwrap();
        // At 5, 7, 9 … 19 s after the close, then the provisional result.
        assert!((7..=9).contains(&(r.gemini.reads() - before)), "{} reads", r.gemini.reads() - before);
    }

    // ── The audit trail ──

    #[tokio::test(start_paused = true)]
    async fn every_resolved_round_is_written_to_the_audit() {
        let (mut r, market) = open_round("63147.36").await;
        r.tape.record(E / 1000, 63_276.216);
        r.gemini.put(E + 5_600, settled(E, "63147.36", true, "63276.216"));
        within(400, r.driver.await_resolution(&market)).await.unwrap();
        let rows = r.audit.rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0],
            AuditRow {
                event_ticker: "BTC05M2611181730".into(),
                strike: d("63147.36"),
                resolved_up: true,
                provisional: false,
                close_price: 63_276.216,
                gemini_up: Some(true),
                index_at_expiry: Some(63_276.216),
            }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_provisional_round_is_written_as_awaiting_geminis_result() {
        let (mut r, market) = open_round("63147.36").await;
        r.tape.record(E / 1000, 63_100.25);
        within(400, r.driver.await_resolution(&market)).await.unwrap();
        let rows = r.audit.rows();
        assert_eq!((rows[0].provisional, rows[0].resolved_up, rows[0].gemini_up), (true, false, None));
    }
}
