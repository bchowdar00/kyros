//! Plan §4 step 5 against the real engine: platform task, manager,
//! `fair-discount`, [`EngineVenue`] over a real `Exchange` with a seeded
//! book, [`EngineClock`] for timers, paused Tokio clock.
//!
//! The registry still names one series (`btc_usd.updown_1m`, plan §8.3), so
//! each world pins that registry id to whichever engine series the test
//! wants the deployment homed in.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rust_decimal::Decimal;

use super::deployment::DeploymentSpec;
use super::manager::{RuntimeHandle, runtime_channel, spawn_manager};
use super::test_support::*;
use crate::automation::signals::{SignalEvent, SignalStore, SignalValue};
use crate::automation::sme::task::{PlatformHandle, spawn_platform};
use crate::automation::venue::{EngineClock, EngineVenue, SeriesMap};
use crate::engine::{Exchange, TradeSource};
use crate::pricing::feed::unix_now_ms;
use crate::series::SERIES;
use crate::ticks::PriceTick;
use crate::types::{Address, Qty, SeriesId, Side, Tenths};

const REGISTRY_SERIES: &str = "btc_usd.updown_1m";
const INSTRUMENT: &str = "btc_usd.updown_1m.current";
const PARENT: Address = Address([0x11; 20]);
const STRAT: Address = Address([0x12; 20]);
const BOT: Address = Address([0xb0; 20]);

struct World {
    exchange: Arc<Mutex<Exchange>>,
    platform: PlatformHandle,
    runtime: RuntimeHandle,
    home: SeriesId,
    ts: u64,
}

fn tick(t: u16) -> PriceTick {
    PriceTick::from_tenths(t).unwrap()
}

impl World {
    /// Both series open now; the strategy sub-account homed in `home`,
    /// which is where the registry series id points.
    async fn new(home: SeriesId) -> World {
        let exchange = Arc::new(Mutex::new(Exchange::new(Tenths(1_000_000), SERIES)));
        let now_s = unix_now_ms() / 1000;
        {
            let mut x = exchange.lock().unwrap();
            x.get_or_create_account(BOT);
            x.get_or_create_account(PARENT);
            x.register_user(PARENT);
            x.create_sub_account_at(PARENT, STRAT, Tenths(500_000), home).unwrap();
            // Settlement sweeps a sub-account whose `strategy` is `None` as
            // an orphan. This test drives the runtime directly rather than
            // through the deploy route, so mark the account occupied.
            x.deploy_strategy_in(STRAT, crate::engine::sub_accounts::test_support::named("fair-discount", home)).unwrap();
            for s in SeriesId::all() {
                x.open_first_market(s, 100_000.0, now_s, s.duration_secs());
            }
        }
        let map = SeriesMap::new([(REGISTRY_SERIES, home)]);
        let clock = EngineClock::new(exchange.clone(), map.clone());
        let platform = spawn_platform(SignalStore::from_registry(&tiny_registry()), clock.clone());
        platform.enable(FAIR);
        platform.enable(BOOK);
        let (runtime, rx) = runtime_channel();
        EngineVenue::install(&exchange, runtime.clone());
        let venue = EngineVenue::new(exchange.clone(), map);
        spawn_manager(rx, platform.clone(), venue, clock);
        World { exchange, platform, runtime, home, ts: 0 }
    }

    async fn deploy(&self) -> String {
        let program = Arc::new(compile_fixture("fair-discount"));
        let spec = DeploymentSpec { program, deploy_epoch: 0, sub_account: STRAT, series: self.home, params: BTreeMap::new() };
        let id = self.runtime.start(spec).await.unwrap();
        settle().await;
        id
    }

    fn book(&mut self, ask: &str) {
        self.ts += 1;
        self.platform.ingest(SignalEvent::new(
            BOOK,
            self.ts,
            [("up", SignalValue::decimal("0.5")), ("down", SignalValue::decimal("0.5")), ("bid", SignalValue::decimal("0.49")), ("ask", SignalValue::decimal(ask)), ("last", SignalValue::decimal("0.5"))],
        ));
    }

    fn fair(&mut self, up: &str) {
        self.ts += 1;
        let down = (Decimal::ONE - d(up)).to_string();
        self.platform.ingest(SignalEvent::new(FAIR, self.ts, [("up", SignalValue::decimal(up)), ("down", SignalValue::decimal(&down))]));
    }

    async fn leg(&self, id: &str) -> String {
        self.runtime.status(id).await.expect("deployment resident").current_leg
    }

    fn best_bid(&self, series: SeriesId) -> Option<PriceTick> {
        self.exchange.lock().unwrap().engine(series).book().best_bid()
    }

    fn strat_locked(&self) -> Tenths {
        self.exchange.lock().unwrap().account(&STRAT).unwrap().locked
    }

    fn strat_position(&self) -> i64 {
        self.exchange.lock().unwrap().engine(self.home).position(&STRAT).position.0
    }

    /// A bot rests an ask so the book has a mark and something to hit.
    fn bot_ask(&self, price: u16, qty: u32) {
        self.exchange.lock().unwrap().place_limit(self.home, BOT, Side::Ask, tick(price), Qty(qty), TradeSource::Bot).unwrap();
    }

    /// The scheduler's round transition: resolve under the lock (acks are
    /// pushed into the manager inbox synchronously), then notify.
    fn roll(&self, series: SeriesId) -> u32 {
        let now_s = unix_now_ms() / 1000;
        let report = self.exchange.lock().unwrap().resolve_and_open_next(series, 100_001.0, 100_001.0, now_s, series.duration_secs());
        self.runtime.window_settled(series, report.resolved_id.0);
        report.new_market_id.0
    }
}

async fn settle() {
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test(start_paused = true)]
async fn fair_discount_on_the_real_engine() {
    let mut w = World::new(SeriesId(0)).await;
    w.bot_ask(700, 100);
    let id = w.deploy().await;
    assert_eq!(w.leg(&id).await, "hunting");

    // Book then fair 5c above ask: bid 100 @ 0.59 rests on the real book.
    w.book("0.55");
    w.fair("0.6");
    settle().await;
    assert_eq!(w.leg(&id).await, "quoting");
    assert_eq!(w.best_bid(SeriesId(0)), Some(tick(590)));
    assert_eq!(w.strat_locked(), Tenths(100 * 590));
    let status = w.runtime.status(&id).await.unwrap();
    assert_eq!(status.live_orders.len(), 1);
    let first_venue_id = status.live_orders[0].venue_order_id.unwrap();

    // Fair moves: atomic cancel-replace to 0.61, `replaced` leaves the leg alone.
    w.fair("0.62");
    settle().await;
    assert_eq!(w.best_bid(SeriesId(0)), Some(tick(610)));
    assert_eq!(w.strat_locked(), Tenths(100 * 610));
    assert_eq!(w.leg(&id).await, "quoting");
    let status = w.runtime.status(&id).await.unwrap();
    assert_eq!(status.live_orders.len(), 1, "the replacement is the only live order");
    assert_ne!(status.live_orders[0].venue_order_id.unwrap(), first_venue_id);
    assert!(status.log.iter().any(|l| l.contains("replaced -> stay")), "{:#?}", status.log);

    // The window_clock timer (10s before the round's close) pulls the bid: CancelAck -> done.
    tokio::time::advance(Duration::from_secs(SeriesId(0).duration_secs() - 9)).await;
    settle().await;
    assert_eq!(w.leg(&id).await, "done");
    assert_eq!(w.best_bid(SeriesId(0)), None, "bid pulled from the real book");
    assert_eq!(w.strat_locked(), Tenths::ZERO);
    assert!(w.runtime.status(&id).await.unwrap().live_orders.is_empty());

    // Settlement resets the session; the next window re-arms hunting.
    let next = w.roll(SeriesId(0));
    settle().await;
    assert_eq!(w.leg(&id).await, "hunting");
    w.runtime.window_opened(SeriesId(0), next, vec![INSTRUMENT.into()]);
    settle().await;
    w.fair("0.6");
    settle().await;
    assert_eq!(w.leg(&id).await, "quoting");
    assert_eq!(w.best_bid(SeriesId(0)), Some(tick(590)));

    // A bot sells into the bid: maker Fill -> done, position 100 @ 0.59.
    w.exchange.lock().unwrap().place_market(SeriesId(0), BOT, Side::Ask, Qty(100), TradeSource::Bot).unwrap();
    settle().await;
    assert_eq!(w.leg(&id).await, "done");
    assert_eq!(w.strat_position(), 100);
    assert_eq!(w.strat_locked(), Tenths::ZERO);
    let status = w.runtime.status(&id).await.unwrap();
    assert!(status.log.iter().any(|l| l.contains("hunting.bid_up.1 fill -> done")), "{:#?}", status.log);
    w.exchange.lock().unwrap().debug_assert_invariants();
}

#[tokio::test(start_paused = true)]
async fn lock_phase_acks_precede_the_settlement_reset() {
    let mut w = World::new(SeriesId(0)).await;
    w.bot_ask(700, 10);
    let id = w.deploy().await;
    w.book("0.55");
    w.fair("0.6");
    settle().await;
    assert_eq!(w.best_bid(SeriesId(0)), Some(tick(590)));

    // Resolve with the bid resting: the engine emits the ack under the
    // lock, the notification follows; the runtime applies the ack first.
    w.roll(SeriesId(0));
    settle().await;
    assert_eq!(w.leg(&id).await, "hunting");
    let log = w.runtime.status(&id).await.unwrap().log;
    let ack = log.iter().position(|l| l.contains("cancel_ack -> stay")).expect("ack applied: {log:#?}");
    let reset = log.iter().position(|l| l.contains("[reset]")).expect("reset logged");
    assert!(ack < reset, "{log:#?}");
    assert_eq!(w.strat_locked(), Tenths::ZERO);
    assert_eq!(w.best_bid(SeriesId(0)), None);
    assert!(w.runtime.status(&id).await.unwrap().live_orders.is_empty());
}

#[tokio::test(start_paused = true)]
async fn settling_another_series_leaves_the_deployment_untouched() {
    // Homed in the 15-minute series; the 2-minute series rolls underneath.
    let mut w = World::new(SeriesId(1)).await;
    w.bot_ask(700, 10);
    let id = w.deploy().await;
    w.book("0.55");
    w.fair("0.6");
    settle().await;
    assert_eq!(w.leg(&id).await, "quoting");
    assert_eq!(w.best_bid(SeriesId(1)), Some(tick(590)));

    w.roll(SeriesId(0));
    settle().await;
    assert_eq!(w.leg(&id).await, "quoting", "2m settlement is not ours");
    assert_eq!(w.best_bid(SeriesId(1)), Some(tick(590)), "bid still resting in the 15m book");
    let status = w.runtime.status(&id).await.unwrap();
    assert_eq!(status.live_orders.len(), 1);
    assert!(!status.log.iter().any(|l| l.contains("[reset]")), "{:#?}", status.log);
    assert_eq!(status.armed_triggers, vec!["fair_moved".to_string(), "last_10s".to_string()]);

    // Our own series settling does reset us.
    w.roll(SeriesId(1));
    settle().await;
    assert_eq!(w.leg(&id).await, "hunting");
    assert_eq!(w.best_bid(SeriesId(1)), None);
    w.exchange.lock().unwrap().debug_assert_invariants();
}
