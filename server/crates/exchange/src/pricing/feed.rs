//! Live BTC-USD price feed state, per-market history and real-time fanout
//! to subscribers. The feed is fed by the Gemini index listener
//! (`gemini::index_feed`); this module owns the shared state it writes to.
//!
//! [`PriceFeed`] is the single source of truth other parts of the server
//! consume:
//!
//! * [`PriceFeed::latest`] — lock-free read of the most recent price.
//! * [`PriceFeed::snapshot_since`] — one series' recent tick history
//!   (for new WS clients connecting mid-round): the open round plus a
//!   bounded run-up before it, so a chart never starts from nothing.
//!   Every series keeps its own window because rounds of different
//!   lengths hold different amounts of history.
//! * [`PriceFeed::subscribe`] — broadcast receiver for live `Tick`s and
//!   series-tagged `MarketReset` events.
//! * [`PriceFeed::reset_for_new_market_in`] — called by a series'
//!   scheduler at each round transition to notify subscribers. The
//!   window is kept: BTC's price before the open is still BTC's price.
//! * [`PriceFeed::recent_velocity_bps`] — short-horizon BTC velocity from
//!   a global, time-bounded buffer that no round reset ever clears.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::collections::VecDeque;

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::series::SERIES;
use crate::types::SeriesId;

/// Max number of ticks kept per minute of round length in a series'
/// history window. The oldest is dropped when the window's cap is
/// reached. ~2000 covers a full minute at >30 Hz.
pub const HISTORY_CAP: usize = 2000;

/// Longest run-up before a round's open that a snapshot carries, so a
/// client joining early in a round sees the move that led into it. A
/// series shorter than this gets one round's worth.
pub const PRE_OPEN_HISTORY_SECS_MAX: u64 = 300;

/// How far before `opens_at` a series' snapshot reaches, in ms.
pub fn pre_open_history_ms(duration_secs: u64) -> u64 {
    duration_secs.min(PRE_OPEN_HISTORY_SECS_MAX) * 1000
}

/// How far back the velocity buffer reaches. Longer than any bot's
/// lookback so a round boundary never blinds the surge detector.
pub const VELOCITY_WINDOW_SECS: u64 = 30;

/// Largest number of ticks handed to a client in a snapshot; longer
/// histories are thinned evenly. At one tick per
/// [`TICK_BROADCAST_INTERVAL_MS`] a 15-minute round is 4,500 ticks, so
/// this keeps every retained sample of the longest series.
pub const SNAPSHOT_MAX_TICKS: usize = 6000;

/// Tick capacity of a series' window: [`HISTORY_CAP`] per (started) minute.
pub fn window_cap(duration_secs: u64) -> usize {
    HISTORY_CAP * (duration_secs.div_ceil(60).max(1) as usize)
}

/// Capacity of the live broadcast channel. Subscribers slower than this
/// receive a `Lagged(n)` signal; we log and continue.
pub const BROADCAST_CAP: usize = 256;

/// Minimum spacing between two ticks handed to chart subscribers and
/// kept in the per-series history. A fast source can deliver tens of
/// updates a second; charts want a calm ~5 Hz tape. The engine, bots and round
/// resolution never see this gate: [`PriceFeed::latest`] and the
/// velocity buffer take every raw tick.
pub const TICK_BROADCAST_INTERVAL_MS: u64 = 200;

/// The coalescing gate between raw ticks and what subscribers see. When a
/// tick arrives inside the interval it replaces `pending`; the flusher
/// task emits that last price once the interval has elapsed, so a burst's
/// final value always reaches the chart.
#[derive(Debug, Default)]
struct EmitGate {
    last_emit_ms: u64,
    pending: Option<f64>,
}

/// A single observed BTC price sample. `ts_ms` is server-side unix time
/// in milliseconds (consistent across all clients on the same server).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Tick {
    pub ts_ms: u64,
    pub price: f64,
}

/// Live message pushed to WS subscribers.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FeedMessage {
    Tick {
        ts_ms: u64,
        price: f64,
    },
    /// A series opened a new round. Subscribers watching a different
    /// series must ignore it (see `api::ws::price_ws`).
    MarketReset {
        series: SeriesId,
        market_id: u32,
        strike: f64,
        opens_at_ms: u64,
        closes_at_ms: u64,
        duration_secs: u64,
    },
}

/// Shared price-feed state. Construct one with [`PriceFeed::new`], hand
/// `Arc<Self>` to the Gemini index listener that records ticks and to
/// every handler that reads from it.
#[derive(Debug)]
pub struct PriceFeed {
    /// Bit pattern of an `f64`; `0` means "no price yet".
    latest: AtomicU64,
    /// One current-round tick window per series, indexed by `SeriesId.0`.
    windows: Vec<Mutex<VecDeque<Tick>>>,
    /// Global, time-bounded ([`VELOCITY_WINDOW_SECS`]) tick buffer that
    /// no round reset clears.
    velocity: Mutex<VecDeque<Tick>>,
    /// Rate gate for history and broadcast; see [`TICK_BROADCAST_INTERVAL_MS`].
    gate: Mutex<EmitGate>,
    tx: broadcast::Sender<FeedMessage>,
}

impl PriceFeed {
    pub fn new() -> Arc<Self> {
        let (tx, _) = broadcast::channel(BROADCAST_CAP);
        Arc::new(Self {
            latest: AtomicU64::new(0),
            windows: SERIES
                .iter()
                .map(|s| Mutex::new(VecDeque::with_capacity(window_cap(s.duration_secs).min(4096))))
                .collect(),
            velocity: Mutex::new(VecDeque::new()),
            gate: Mutex::new(EmitGate::default()),
            tx,
        })
    }

    fn window(&self, series: SeriesId) -> &Mutex<VecDeque<Tick>> {
        &self.windows[series.0 as usize]
    }

    /// Most recent observed BTC-USD price, or `None` if the feed has not
    /// yet delivered one.
    pub fn latest(&self) -> Option<f64> {
        match self.latest.load(Ordering::Relaxed) {
            0 => None,
            b => Some(f64::from_bits(b)),
        }
    }

    /// Copy of `series`' current-round tick history, oldest first.
    pub fn snapshot_for(&self, series: SeriesId) -> Vec<Tick> {
        let h = self.window(series).lock().expect("price history mutex poisoned");
        h.iter().copied().collect()
    }

    /// The ticks in `series`' window at or after `since_ms`, oldest first.
    pub fn snapshot_since(&self, series: SeriesId, since_ms: u64) -> Vec<Tick> {
        let h = self.window(series).lock().expect("price history mutex poisoned");
        let skip = h.partition_point(|t| t.ts_ms < since_ms);
        h.iter().skip(skip).copied().collect()
    }

    /// [`PriceFeed::snapshot_since`] thinned evenly to at most `max` ticks,
    /// always keeping the first and the latest.
    pub fn snapshot_thinned_since(&self, series: SeriesId, since_ms: u64, max: usize) -> Vec<Tick> {
        thin_ticks(self.snapshot_since(series, since_ms), max)
    }

    /// Signed BTC velocity over the most recent `lookback_secs`, expressed
    /// in basis points: `(latest − oldest_in_window) / oldest_in_window ·
    /// 10_000`. Positive means BTC has risen during the window, negative
    /// means it has fallen.
    ///
    /// Used by the trader-bot population to detect *sudden* moves —
    /// distinct from cumulative drift vs. strike (`momentum_bps`) — so the
    /// book can flood with crossing flow when BTC swings fast.
    ///
    /// Returns `None` if the history has fewer than two ticks or no tick
    /// is older than `lookback_secs`.
    pub fn recent_velocity_bps(&self, lookback_secs: f64) -> Option<i32> {
        if !lookback_secs.is_finite() || lookback_secs <= 0.0 {
            return None;
        }
        let h = self.velocity.lock().expect("velocity mutex poisoned");
        if h.len() < 2 {
            return None;
        }
        let latest = h.back().copied()?;
        let cutoff_ms = latest.ts_ms.saturating_sub((lookback_secs * 1000.0) as u64);
        // First tick at or before the cutoff is our window anchor; if none
        // is old enough we use the oldest in history.
        let oldest = h
            .iter()
            .find(|t| t.ts_ms >= cutoff_ms)
            .copied()
            .unwrap_or_else(|| *h.front().expect("len >= 2"));
        if oldest.ts_ms == latest.ts_ms || oldest.price <= 0.0 {
            return None;
        }
        let bps = ((latest.price - oldest.price) / oldest.price) * 10_000.0;
        if !bps.is_finite() {
            return None;
        }
        Some(bps.round() as i32)
    }

    /// Subscribe to live price events. A subscriber that falls behind by
    /// more than [`BROADCAST_CAP`] messages will receive a `Lagged` error
    /// — handlers should log and continue, not disconnect.
    pub fn subscribe(&self) -> broadcast::Receiver<FeedMessage> {
        self.tx.subscribe()
    }

    /// Record a new raw tick. The atomic latest and the velocity buffer
    /// take it immediately (the engine and bots read those). History and
    /// the broadcast are rate-limited to one tick per
    /// [`TICK_BROADCAST_INTERVAL_MS`]: a tick inside the interval only
    /// becomes `pending`, and [`Self::flush_pending`] emits it once the
    /// interval has elapsed. Send failures are ignored (no subscribers).
    pub fn record(&self, price: f64) {
        let ts_ms = unix_now_ms();
        self.record_at(ts_ms, price);
    }

    fn record_at(&self, ts_ms: u64, price: f64) {
        self.observe(ts_ms, price);
        let mut gate = self.gate.lock().expect("emit gate mutex poisoned");
        if ts_ms.saturating_sub(gate.last_emit_ms) >= TICK_BROADCAST_INTERVAL_MS {
            gate.last_emit_ms = ts_ms;
            gate.pending = None;
            drop(gate);
            self.emit(ts_ms, price);
        } else {
            gate.pending = Some(price);
        }
    }

    /// Emit a tick held back by the rate gate once its interval is up.
    /// Driven by [`spawn_tick_flusher`]; a no-op when nothing is pending
    /// or the last emit is still fresh.
    pub fn flush_pending(&self) {
        self.flush_pending_at(unix_now_ms());
    }

    fn flush_pending_at(&self, now_ms: u64) {
        let mut gate = self.gate.lock().expect("emit gate mutex poisoned");
        if gate.pending.is_none()
            || now_ms.saturating_sub(gate.last_emit_ms) < TICK_BROADCAST_INTERVAL_MS
        {
            return;
        }
        let price = gate.pending.take().expect("checked above");
        gate.last_emit_ms = now_ms;
        drop(gate);
        self.emit(now_ms, price);
    }

    /// Inject a tick at an explicit timestamp. Test-only — production
    /// ticks come through [`Self::record`] which timestamps with the
    /// system clock. Bypasses the rate gate and the broadcast so tests
    /// can build deterministic histories without flooding subscribers.
    #[cfg(test)]
    pub fn record_with_ts(&self, ts_ms: u64, price: f64) {
        self.observe(ts_ms, price);
        self.push_history(Tick { ts_ms, price });
    }

    /// The raw path: what `latest()` and the surge detector see.
    fn observe(&self, ts_ms: u64, price: f64) {
        self.latest.store(price.to_bits(), Ordering::Relaxed);
        let tick = Tick { ts_ms, price };
        let mut v = self.velocity.lock().expect("velocity mutex poisoned");
        let cutoff = ts_ms.saturating_sub(VELOCITY_WINDOW_SECS * 1000);
        while v.front().is_some_and(|t| t.ts_ms < cutoff) {
            v.pop_front();
        }
        v.push_back(tick);
    }

    /// The gated path: what charts see, live and in snapshots.
    fn emit(&self, ts_ms: u64, price: f64) {
        self.push_history(Tick { ts_ms, price });
        let _ = self.tx.send(FeedMessage::Tick { ts_ms, price });
    }

    fn push_history(&self, tick: Tick) {
        for (i, w) in self.windows.iter().enumerate() {
            let cap = window_cap(SERIES[i].duration_secs);
            let mut h = w.lock().expect("price history mutex poisoned");
            if h.len() >= cap {
                h.pop_front();
            }
            h.push_back(tick);
        }
    }

    /// Notify subscribers that `series` opened a new round. The window is
    /// deliberately kept: the price line is continuous across rounds and
    /// only the strike changes, so clients draw the run-up into the open
    /// rather than an empty plot. Other series and the velocity buffer are
    /// untouched.
    pub fn reset_for_new_market_in(
        &self,
        series: SeriesId,
        market_id: u32,
        strike: f64,
        opens_at_ms: u64,
        closes_at_ms: u64,
    ) {
        let _ = self.tx.send(FeedMessage::MarketReset {
            series,
            market_id,
            strike,
            opens_at_ms,
            closes_at_ms,
            duration_secs: series.duration_secs(),
        });
    }
}

/// Drive the rate gate's trailing edge: every
/// [`TICK_BROADCAST_INTERVAL_MS`] emit whatever tick a burst left
/// pending, so the chart ends on the true last price even when the feed
/// goes quiet. Idempotent and cheap when nothing is pending.
pub fn spawn_tick_flusher(feed: Arc<PriceFeed>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(TICK_BROADCAST_INTERVAL_MS));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            feed.flush_pending();
        }
    })
}

/// Evenly thin `ticks` to at most `max` entries, keeping the first and
/// the last so the chart's window edges stay anchored.
pub fn thin_ticks(ticks: Vec<Tick>, max: usize) -> Vec<Tick> {
    thin_evenly(ticks, max)
}

/// Evenly thin `items` to at most `max` entries by stride, keeping the
/// first and the last. Shared by the price snapshot and signal history.
pub fn thin_evenly<T: Clone>(items: Vec<T>, max: usize) -> Vec<T> {
    if max == 0 || items.len() <= max {
        return items;
    }
    let n = items.len();
    let mut out = Vec::with_capacity(max);
    for i in 0..max {
        // Map i ∈ [0, max) onto [0, n) with the last slot pinned to n−1.
        let idx = if i == max - 1 { n - 1 } else { i * n / max };
        out.push(items[idx].clone());
    }
    out
}

pub(crate) fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::DEFAULT_SERIES;

    #[test]
    fn latest_starts_empty_then_holds() {
        let f = PriceFeed::new();
        assert!(f.latest().is_none());
        f.record(99_000.0);
        assert_eq!(f.latest(), Some(99_000.0));
        f.record(100_000.5);
        assert_eq!(f.latest(), Some(100_000.5));
    }

    #[test]
    fn rate_gate_coalesces_bursts_and_flushes_the_last_price() {
        let f = PriceFeed::new();
        let mut rx = f.subscribe();
        // A burst of ten ticks inside one interval: only the first is
        // emitted, the newest waits as pending.
        for i in 0..10u64 {
            f.record_at(1_000 + i * 10, 100_000.0 + i as f64);
        }
        assert_eq!(f.latest(), Some(100_009.0), "latest is never gated");
        assert_eq!(f.snapshot_for(DEFAULT_SERIES).len(), 1);
        assert!(matches!(rx.try_recv(), Ok(FeedMessage::Tick { price, .. }) if price == 100_000.0));
        assert!(rx.try_recv().is_err());
        // Too early to flush; nothing happens.
        f.flush_pending_at(1_150);
        assert_eq!(f.snapshot_for(DEFAULT_SERIES).len(), 1);
        // Interval elapsed: the burst's last price is emitted with the flush time.
        f.flush_pending_at(1_200);
        let snap = f.snapshot_for(DEFAULT_SERIES);
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[1], Tick { ts_ms: 1_200, price: 100_009.0 });
        assert!(matches!(rx.try_recv(), Ok(FeedMessage::Tick { ts_ms: 1_200, price }) if price == 100_009.0));
        // Flushing again with nothing pending is a no-op.
        f.flush_pending_at(2_000);
        assert_eq!(f.snapshot_for(DEFAULT_SERIES).len(), 2);
        // A tick after the interval goes straight through.
        f.record_at(1_400, 100_020.0);
        assert_eq!(f.snapshot_for(DEFAULT_SERIES).len(), 3);
    }

    #[test]
    fn record_appends_to_history_and_caps() {
        let f = PriceFeed::new();
        let cap = window_cap(DEFAULT_SERIES.duration_secs());
        for i in 0..(cap + 50) {
            f.record_with_ts(i as u64 * TICK_BROADCAST_INTERVAL_MS, 100_000.0 + i as f64);
        }
        let snap = f.snapshot_for(DEFAULT_SERIES);
        assert_eq!(snap.len(), cap);
        // Oldest entries were dropped: first surviving tick is index 50.
        assert_eq!(snap.first().unwrap().price, 100_000.0 + 50.0);
        assert_eq!(snap.last().unwrap().price, 100_000.0 + (cap as f64 + 49.0));
    }

    #[test]
    fn reset_keeps_history_and_broadcasts() {
        let f = PriceFeed::new();
        let mut rx = f.subscribe();
        // Spaced past the rate gate so both ticks reach history and subscribers.
        f.record_at(1_000, 99_000.0);
        f.record_at(1_000 + TICK_BROADCAST_INTERVAL_MS, 99_100.0);
        assert_eq!(f.snapshot_for(DEFAULT_SERIES).len(), 2);
        // Drain the two Tick events first.
        let _ = rx.try_recv();
        let _ = rx.try_recv();

        let duration = DEFAULT_SERIES.duration_secs();
        f.reset_for_new_market_in(
            DEFAULT_SERIES,
            7,
            99_500.0,
            1_700_000_000_000,
            1_700_000_000_000 + duration * 1000,
        );
        assert_eq!(f.snapshot_for(DEFAULT_SERIES).len(), 2, "the run-up survives the open");
        match rx.try_recv().expect("reset broadcast") {
            FeedMessage::MarketReset { series, market_id, strike, opens_at_ms, closes_at_ms, duration_secs } => {
                assert_eq!(series, DEFAULT_SERIES);
                assert_eq!(market_id, 7);
                assert_eq!(strike, 99_500.0);
                assert_eq!(opens_at_ms, 1_700_000_000_000);
                assert_eq!(duration_secs, DEFAULT_SERIES.duration_secs());
                assert_eq!(closes_at_ms, 1_700_000_000_000 + duration_secs * 1000);
            }
            other => panic!("expected MarketReset, got {:?}", other),
        }
    }

    #[test]
    fn snapshot_since_bounds_the_run_up_before_an_open() {
        let f = PriceFeed::new();
        for i in 0..10u64 {
            f.record_with_ts(1_000 * i, 99_000.0 + i as f64);
        }
        let since = f.snapshot_since(DEFAULT_SERIES, 6_000);
        assert_eq!(since.len(), 4);
        assert_eq!(since[0].ts_ms, 6_000, "the cutoff tick itself is included");
        assert_eq!(since.last().unwrap().ts_ms, 9_000);
        assert!(f.snapshot_since(DEFAULT_SERIES, 20_000).is_empty());
        assert_eq!(f.snapshot_since(DEFAULT_SERIES, 0).len(), 10);
        assert_eq!(f.snapshot_thinned_since(DEFAULT_SERIES, 0, 5).len(), 5);
    }

    #[test]
    fn pre_open_history_is_one_round_capped_at_five_minutes() {
        assert_eq!(pre_open_history_ms(120), 120_000);
        assert_eq!(pre_open_history_ms(900), 300_000);
    }

    #[test]
    fn velocity_survives_reset() {
        let f = PriceFeed::new();
        f.record_with_ts(0, 100_000.0);
        f.record_with_ts(1_000, 100_100.0);
        f.reset_for_new_market_in(SeriesId(0), 2, 100_100.0, 1_000, 61_000);
        f.reset_for_new_market_in(SeriesId(1), 3, 100_100.0, 1_000, 901_000);
        // The surge detector still sees the move that straddled the resets.
        assert_eq!(f.recent_velocity_bps(2.0), Some(10));
    }

    #[test]
    fn velocity_buffer_is_time_bounded() {
        let f = PriceFeed::new();
        f.record_with_ts(0, 100_000.0);
        f.record_with_ts(VELOCITY_WINDOW_SECS * 1000 + 5_000, 100_500.0);
        // The first tick fell out of the 30 s window; only one remains.
        assert_eq!(f.recent_velocity_bps(60.0), None);
    }

    #[test]
    fn window_caps_scale_with_duration() {
        assert_eq!(window_cap(60), HISTORY_CAP);
        assert_eq!(window_cap(120), 2 * HISTORY_CAP);
        assert_eq!(window_cap(900), 15 * HISTORY_CAP);
    }

    #[test]
    fn snapshot_thinning_keeps_ends_and_respects_max() {
        let ticks: Vec<Tick> = (0..100).map(|i| Tick { ts_ms: i, price: i as f64 }).collect();
        let thin = thin_ticks(ticks.clone(), 10);
        assert_eq!(thin.len(), 10);
        assert_eq!(thin[0].ts_ms, 0);
        assert_eq!(thin[9].ts_ms, 99);
        assert!(thin.windows(2).all(|w| w[0].ts_ms < w[1].ts_ms));
        assert_eq!(thin_ticks(ticks.clone(), 1000).len(), 100);
    }

    #[test]
    fn recent_velocity_returns_none_with_fewer_than_two_ticks() {
        let f = PriceFeed::new();
        assert_eq!(f.recent_velocity_bps(2.0), None);
        f.record_with_ts(1_000, 100_000.0);
        assert_eq!(f.recent_velocity_bps(2.0), None);
    }

    #[test]
    fn recent_velocity_measures_rise() {
        let f = PriceFeed::new();
        f.record_with_ts(0, 100_000.0);
        f.record_with_ts(1_000, 100_100.0);
        // +$100 / $100_000 = +10 bps over the window.
        assert_eq!(f.recent_velocity_bps(2.0), Some(10));
    }

    #[test]
    fn recent_velocity_measures_fall() {
        let f = PriceFeed::new();
        f.record_with_ts(0, 100_000.0);
        f.record_with_ts(1_000, 99_700.0);
        assert_eq!(f.recent_velocity_bps(2.0), Some(-30));
    }

    #[test]
    fn recent_velocity_uses_only_window() {
        let f = PriceFeed::new();
        // Old tick well outside the 2s window.
        f.record_with_ts(0, 100_000.0);
        // Two ticks inside the window measured from the latest (10_000ms).
        f.record_with_ts(9_000, 100_500.0);
        f.record_with_ts(10_000, 100_600.0);
        // Anchor should be 9_000ms (first tick >= 8_000ms cutoff), not the
        // 100_000.0 tick at t=0.
        // (100_600 - 100_500) / 100_500 ≈ +9.95 bps → rounds to +10.
        assert_eq!(f.recent_velocity_bps(2.0), Some(10));
    }

    #[test]
    fn recent_velocity_rejects_bad_lookback() {
        let f = PriceFeed::new();
        f.record_with_ts(0, 100_000.0);
        f.record_with_ts(1_000, 100_100.0);
        assert_eq!(f.recent_velocity_bps(0.0), None);
        assert_eq!(f.recent_velocity_bps(-1.0), None);
        assert_eq!(f.recent_velocity_bps(f64::NAN), None);
    }

    #[test]
    fn recent_velocity_falls_back_to_oldest_tick_when_history_short() {
        // If history is younger than the lookback window, the velocity
        // collapses to (latest − oldest) — we still surface a signal
        // instead of returning None, since the trader bots need *some*
        // reading to act on early in a round.
        let f = PriceFeed::new();
        f.record_with_ts(100, 100_000.0);
        f.record_with_ts(500, 100_200.0);
        // 30-second window, but history only spans 400ms.
        // (100_200 - 100_000) / 100_000 = 20 bps.
        assert_eq!(f.recent_velocity_bps(30.0), Some(20));
    }

    #[test]
    fn subscribe_receives_live_ticks() {
        let f = PriceFeed::new();
        let mut rx = f.subscribe();
        f.record(100_000.0);
        match rx.try_recv().expect("tick broadcast") {
            FeedMessage::Tick { price, .. } => assert_eq!(price, 100_000.0),
            other => panic!("expected Tick, got {:?}", other),
        }
    }
}
