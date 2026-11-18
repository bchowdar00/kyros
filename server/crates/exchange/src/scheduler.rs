//! Drives one market series' round lifecycle.
//!
//! One task per series, generic over a [`RoundDriver`]: the thing that
//! knows what a round of this series is *about*. The driver supplies the
//! reference the first round opens on, waits until the current round is
//! final and reports its outcome together with the next round's
//! reference. The scheduler owns everything else: opening and resolving
//! on the engine, broadcasting the book, and telling the strategy runtime.
//!
//! [`BtcDriver`] is the BTC up/down driver: it polls the price feed until
//! a first tick arrives (that price is the strike), sleeps to the round's
//! close, reads the latest price, and resolves Yes iff it is strictly
//! above the strike; the close becomes the next strike. After each open
//! it resets the feed's window for the series so tick #1 belongs to the
//! new round. The NBA driver lives in `sim::nba`.
//!
//! After every transition the scheduler tells the strategy runtime, in
//! this order: `window_settled` for the round that just closed, then
//! `window_opened` for the new one. Both are sent *after* the engine lock
//! is released. Settlement cancels resting orders under the lock and the
//! engine's execution sink pushes those cancel acks into the runtime's
//! inbox synchronously, so the acks always precede the settlement frame —
//! the ordering the deployment's session reset relies on.

use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::automation::runtime::RuntimeHandle;
use crate::engine::{Exchange, Market, MarketEngine, Outcome, RoundClose, RoundReference};
use crate::pricing::feed::PriceFeed;
use crate::types::SeriesId;

/// Duration of the default series. Kept under its historical name; the
/// per-series value lives in [`crate::series::SERIES`].
pub const MARKET_DURATION_SECS: u64 = crate::series::SERIES[crate::types::DEFAULT_SERIES.0 as usize].duration_secs;
const PRICE_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// How a driver closes a round: the outcome, how it closed, and what the
/// next round opens on.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RoundResolution {
    pub outcome: Outcome,
    pub close: RoundClose,
    pub next: RoundReference,
}

/// What a series' rounds are about. One implementation per underlying.
pub trait RoundDriver: Send + 'static {
    /// Wait until the series can open its first round, and return what it
    /// opens on.
    fn first_reference(&mut self) -> impl Future<Output = RoundReference> + Send;

    /// Wait until `current` is final and report how it resolved. Called
    /// once per round; must not return before `current.closes_at`.
    fn await_resolution(&mut self, current: &Market) -> impl Future<Output = RoundResolution> + Send;

    /// A round just opened (called outside the engine lock).
    fn on_opened(&mut self, _market: &Market) {}
}

/// The BTC up/down driver over the live price feed.
pub struct BtcDriver {
    feed: Arc<PriceFeed>,
}

impl BtcDriver {
    pub fn new(feed: Arc<PriceFeed>) -> Self {
        Self { feed }
    }
}

impl RoundDriver for BtcDriver {
    async fn first_reference(&mut self) -> RoundReference {
        // Wait for the first BTC tick before opening any market — the
        // strike has to come from somewhere.
        RoundReference::Btc { strike_btc_usd: wait_for_first_price(&self.feed).await }
    }

    async fn await_resolution(&mut self, current: &Market) -> RoundResolution {
        sleep_until_unix(current.closes_at).await;
        let strike = current.strike_btc_usd().expect("BTC driver on a BTC round");
        // A missing price at the boundary defers the transition until one
        // arrives; the round simply runs long.
        let mut warned = false;
        let close = loop {
            if let Some(p) = self.feed.latest() {
                break p;
            }
            if !warned {
                tracing::warn!("no BTC price available at tick; deferring round transition");
                warned = true;
            }
            tokio::time::sleep(PRICE_POLL_INTERVAL).await;
        };
        let outcome = if close > strike { Outcome::Yes } else { Outcome::No };
        RoundResolution {
            outcome,
            close: RoundClose::Btc { close_price_btc_usd: close },
            next: RoundReference::Btc { strike_btc_usd: close },
        }
    }

    fn on_opened(&mut self, market: &Market) {
        let strike = market.strike_btc_usd().expect("BTC driver on a BTC round");
        self.feed.reset_for_new_market_in(
            market.series,
            market.id.0,
            strike,
            market.opened_at * 1000,
            market.closes_at * 1000,
        );
    }
}

/// The instrument ids a `window_clock` timer may name for a round: the
/// concrete market id and the series' `<slug>.current` alias (what
/// compiled programs use).
pub fn window_instruments(series: SeriesId, market_id: u32) -> Vec<String> {
    vec![market_id.to_string(), format!("{}.current", series.slug())]
}

/// Spawn the market scheduler. The task runs until the program exits.
///
/// `book_broadcast` is invoked under the engine lock after every lifecycle
/// transition (initial open and every round resolution) so subscribers on
/// `/ws/book` see the post-mutation snapshot in lockstep with the engine.
/// `runtime` receives the window notifications after the lock is released.
pub fn spawn<F>(
    exchange: Arc<Mutex<Exchange>>,
    series: SeriesId,
    feed: Arc<PriceFeed>,
    runtime: RuntimeHandle,
    book_broadcast: F,
) -> tokio::task::JoinHandle<()>
where
    F: Fn(&MarketEngine) + Send + 'static,
{
    spawn_with_duration(exchange, series, series.duration_secs(), feed, runtime, book_broadcast)
}

/// [`spawn`] with an explicit round length instead of the series table's.
/// Tests use it to run whole rounds in seconds.
pub fn spawn_with_duration<F>(
    exchange: Arc<Mutex<Exchange>>,
    series: SeriesId,
    duration_secs: u64,
    feed: Arc<PriceFeed>,
    runtime: RuntimeHandle,
    book_broadcast: F,
) -> tokio::task::JoinHandle<()>
where
    F: Fn(&MarketEngine) + Send + 'static,
{
    spawn_with_driver(exchange, series, duration_secs, BtcDriver::new(feed), runtime, book_broadcast)
}

/// The scheduler over any [`RoundDriver`].
pub fn spawn_with_driver<D, F>(
    exchange: Arc<Mutex<Exchange>>,
    series: SeriesId,
    duration_secs: u64,
    mut driver: D,
    runtime: RuntimeHandle,
    book_broadcast: F,
) -> tokio::task::JoinHandle<()>
where
    D: RoundDriver,
    F: Fn(&MarketEngine) + Send + 'static,
{
    tokio::spawn(async move {
        let initial = driver.first_reference().await;
        let now = unix_now();
        let market = {
            let mut x = exchange.lock().expect("exchange mutex poisoned");
            x.open_first_market_with(series, initial, now, duration_secs);
            let e = x.engine(series);
            book_broadcast(e);
            *e.current_market().expect("just opened")
        };
        driver.on_opened(&market);
        runtime.window_opened(series, market.id.0, window_instruments(series, market.id.0));
        tracing::info!(
            %series,
            market_id = market.id.0,
            reference = ?market.reference,
            duration_secs,
            "first market opened",
        );

        let mut current = market;
        loop {
            let resolution = driver.await_resolution(&current).await;
            let now = unix_now();
            let (report, next) = {
                let mut x = exchange.lock().expect("exchange mutex poisoned");
                let report = x.resolve_with(
                    series,
                    resolution.outcome,
                    resolution.close,
                    resolution.next,
                    now,
                    duration_secs,
                );
                let e = x.engine(series);
                book_broadcast(e);
                (report, *e.current_market().expect("just opened"))
            };
            driver.on_opened(&next);
            // Settled first, then opened: the deployment rewinds its
            // session on the former and re-arms on the latter.
            runtime.window_settled(series, report.resolved_id.0);
            runtime.window_opened(
                series,
                report.new_market_id.0,
                window_instruments(series, report.new_market_id.0),
            );
            tracing::info!(
                %series,
                resolved = report.resolved_id.0,
                outcome = ?report.outcome,
                close = ?resolution.close,
                cancelled = report.cancelled_orders,
                new = report.new_market_id.0,
                reference = ?next.reference,
                "round resolved",
            );
            current = next;
        }
    })
}

async fn wait_for_first_price(feed: &PriceFeed) -> f64 {
    loop {
        if let Some(p) = feed.latest() {
            return p;
        }
        tokio::time::sleep(PRICE_POLL_INTERVAL).await;
    }
}

/// Sleep until the unix second `at` (no-op if it has passed).
pub async fn sleep_until_unix(at: u64) {
    let now_ms = unix_now_ms();
    let at_ms = at * 1000;
    if at_ms > now_ms {
        tokio::time::sleep(Duration::from_millis(at_ms - now_ms)).await;
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_secs()
}

fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::automation::runtime::{RuntimeCommand, runtime_channel};
    use crate::series::SERIES;
    use crate::types::Tenths;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn scheduler_two_series_open_with_own_durations() {
        let exchange = Arc::new(Mutex::new(Exchange::new(Tenths(1_000_000), SERIES)));
        let feed = PriceFeed::new();
        feed.record(100_000.0);
        let (runtime, mut rx) = runtime_channel();
        // Series 0 rolls every second; series 1 every three.
        spawn_with_duration(exchange.clone(), SeriesId(0), 1, feed.clone(), runtime.clone(), |_| {});
        spawn_with_duration(exchange.clone(), SeriesId(1), 3, feed.clone(), runtime, |_| {});

        tokio::time::sleep(Duration::from_millis(1_700)).await;
        let x = exchange.lock().unwrap();
        let m0 = x.engine(SeriesId(0)).current_market().expect("s0 open");
        let m1 = x.engine(SeriesId(1)).current_market().expect("s1 open");
        assert_eq!(m0.series, SeriesId(0));
        assert_eq!(m1.series, SeriesId(1));
        assert_eq!(m0.duration_secs, 1);
        assert_eq!(m1.duration_secs, 3);
        // s0 has rolled at least once; s1 is still on its first round.
        assert!(x.engine(SeriesId(0)).resolved_markets(10).count() >= 1);
        assert_eq!(x.engine(SeriesId(1)).resolved_markets(10).count(), 0);
        drop(x);

        // The runtime saw both first opens, then s0's settle/open pairs,
        // each open naming the market id and the `.current` alias.
        let mut opened: Vec<(SeriesId, u32, Vec<String>)> = Vec::new();
        let mut settled: Vec<(SeriesId, u32)> = Vec::new();
        let mut s0_events: Vec<&'static str> = Vec::new();
        while let Ok(cmd) = rx.try_recv() {
            match cmd {
                RuntimeCommand::WindowOpened { series, market_id, instrument_ids } => {
                    if series == SeriesId(0) {
                        s0_events.push("opened");
                    }
                    opened.push((series, market_id, instrument_ids));
                }
                RuntimeCommand::WindowSettled { series, market_id } => {
                    if series == SeriesId(0) {
                        s0_events.push("settled");
                    }
                    settled.push((series, market_id));
                }
                _ => {}
            }
        }
        assert!(opened.iter().any(|(s, _, _)| *s == SeriesId(1)), "s1 first open notified");
        assert!(!settled.iter().any(|(s, _)| *s == SeriesId(1)), "s1 never settled");
        assert!(settled.iter().any(|(s, _)| *s == SeriesId(0)), "s0 settled at least once");
        assert_eq!(s0_events[0], "opened");
        for pair in s0_events[1..].chunks(2) {
            assert_eq!(pair, &["settled", "opened"][..pair.len()], "settled precedes the next open");
        }
        let (_, id, ids) = opened.iter().find(|(s, _, _)| *s == SeriesId(0)).unwrap();
        assert_eq!(ids, &window_instruments(SeriesId(0), *id));
        assert!(ids.contains(&format!("{}.current", SeriesId(0).slug())));
    }
}
