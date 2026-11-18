//! Behavioural tests for the shape fixtures the authoring pipeline teaches
//! from (`tests/fixtures/tiny/{momentum-flip,scale-in,two-sided-quoter,
//! re-entry,every-time-cooldown}.json`). Each drives the compiled program
//! through the real platform task, the interpreter and a fake venue on a
//! paused Tokio clock and asserts what the strategy DID, not that it compiled:
//! fire counts, flip direction, flatten side, cancels on the stop timer.
//!
//! The fake venue does not track positions, so the account view is set by
//! hand after each scripted fill, the way the engine would have moved it.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rust_decimal::Decimal;
use strategy_compiler::ir::{OrderType, Side, Tif};

use super::deployment::DeploymentSpec;
use super::manager::{RuntimeHandle, runtime_channel, spawn_manager};
use super::test_support::*;
use crate::automation::signals::{SignalEvent, SignalStore, SignalValue};
use crate::automation::sme::VenueClock;
use crate::automation::sme::task::{PlatformHandle, spawn_platform};
use crate::automation::venue::{AccountView, FakeVenue, VenueOrder};
use crate::types::{Address, SeriesId};

const INSTRUMENT: &str = "btc_usd.updown_1m.current";
const ACCT: Address = Address([0x33; 20]);
const SERIES: SeriesId = SeriesId(0);

#[derive(Default)]
struct TestClock {
    remaining: Mutex<BTreeMap<String, u64>>,
    now: Mutex<u64>,
}

impl VenueClock for TestClock {
    fn window_remaining_ms(&self, instrument_id: &str) -> Option<u64> {
        self.remaining.lock().unwrap().get(instrument_id).copied()
    }
    fn now_ms(&self) -> u64 {
        *self.now.lock().unwrap()
    }
}

struct World {
    platform: PlatformHandle,
    runtime: RuntimeHandle,
    venue: Arc<FakeVenue>,
    id: String,
    ts: u64,
}

/// Let every task on the current-thread runtime process its queues.
async fn settle() {
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
}

impl World {
    /// A deployed `fixture` with a 60-second window open.
    async fn new(fixture: &str) -> World {
        let clock = Arc::new(TestClock::default());
        clock.remaining.lock().unwrap().insert(INSTRUMENT.into(), 60_000);
        let store = SignalStore::from_registry(&tiny_registry());
        let platform = spawn_platform(store, clock.clone());
        platform.enable(FAIR);
        platform.enable(BOOK);
        let (runtime, rx) = runtime_channel();
        let venue = FakeVenue::new(runtime.clone());
        spawn_manager(rx, platform.clone(), venue.clone(), clock.clone());
        let program = Arc::new(compile_fixture(fixture));
        let spec = DeploymentSpec { program, deploy_epoch: 0, sub_account: ACCT, series: SERIES, params: BTreeMap::new() };
        let id = runtime.start(spec).await.unwrap();
        settle().await;
        World { platform, runtime, venue, id, ts: 0 }
    }

    /// Publish a book with `up` as the mark; the spread is one cent either side.
    async fn up(&mut self, up: &str) {
        self.book_at(up, self.ts + 1).await;
    }

    async fn book_at(&mut self, up: &str, ts: u64) {
        self.ts = ts;
        let up = d(up);
        let cent = d("0.01");
        let fields = [
            ("up", SignalValue::decimal(&up.to_string())),
            ("down", SignalValue::decimal(&(Decimal::ONE - up).to_string())),
            ("bid", SignalValue::decimal(&(up - cent).to_string())),
            ("ask", SignalValue::decimal(&(up + cent).to_string())),
            ("last", SignalValue::decimal(&up.to_string())),
        ];
        self.platform.ingest(SignalEvent::new(BOOK, ts, fields));
        settle().await;
    }

    async fn fair_at(&mut self, up: &str, ts: u64) {
        self.ts = ts;
        let down = (Decimal::ONE - d(up)).to_string();
        self.platform.ingest(SignalEvent::new(FAIR, ts, [("up", SignalValue::decimal(up)), ("down", SignalValue::decimal(&down))]));
        settle().await;
    }

    async fn leg(&self) -> String {
        self.runtime.status(&self.id).await.expect("deployment resident").current_leg
    }

    async fn log(&self) -> Vec<String> {
        self.runtime.status(&self.id).await.expect("deployment resident").log
    }

    /// Fill the newest live order in full at `price` and move the account view
    /// to `position` / `avg_cost` the way the engine would.
    async fn fill_last(&self, price: &str, position: &str, avg_cost: &str) {
        let id = *self.venue.live_ids().last().expect("a live order to fill");
        let size = self.venue.state.lock().unwrap().live[&id].remaining;
        self.venue.fill(id, size, d(price));
        self.position(position, avg_cost);
        settle().await;
    }

    fn position(&self, position: &str, avg_cost: &str) {
        self.venue.set_view(SERIES, ACCT, AccountView { position_size: d(position), position_avg_cost: d(avg_cost), ..Default::default() });
    }

    fn submitted(&self) -> Vec<VenueOrder> {
        self.venue.submitted()
    }

    async fn assert_no_rejects(&self) {
        let log = self.log().await;
        assert!(!log.iter().any(|l| l.contains("[reject]")), "a rejected order: {log:#?}");
    }
}

fn is_flatten(order: &VenueOrder, side: Side) -> bool {
    order.reduce_only && order.order_type == OrderType::Market && order.side == side && order.outcome.as_deref() == Some("up")
}

fn is_take(order: &VenueOrder, outcome: &str) -> bool {
    !order.reduce_only && order.order_type == OrderType::Limit && order.tif == Tif::Ioc && order.side == Side::Buy && order.outcome.as_deref() == Some(outcome)
}

#[tokio::test(start_paused = true)]
async fn flip_enters_once_flips_on_the_way_down_and_never_on_the_way_up() {
    let mut w = World::new("momentum-flip").await;
    assert_eq!(w.leg().await, "hunting");

    // Up crosses 70 cents: one take of up a cent through the ask (0.76 + 0.01).
    w.up("0.75").await;
    let orders = w.submitted();
    assert_eq!(orders.len(), 1, "{:#?}", w.log().await);
    assert!(is_take(&orders[0], "up"));
    assert_eq!(orders[0].price, Some(d("0.77")));
    assert_eq!(orders[0].size, d("100"), "floor(400 * 0.5 / 0.77) = 259, capped at max_order_size 100");
    assert_eq!(w.leg().await, "holding_up", "a take moves at placement");
    w.fill_last("0.76", "100", "0.76").await;

    // Rising while holding up: up_rich is no longer armed, nothing happens.
    w.up("0.8").await;
    w.up("0.85").await;
    assert_eq!(w.submitted().len(), 1, "no second entry on the way up");

    // Falling below 50 cents: flatten the long, then buy down with the released dollars.
    w.up("0.45").await;
    let orders = w.submitted();
    assert_eq!(orders.len(), 3, "{:#?}", w.log().await);
    assert!(is_flatten(&orders[1], Side::Sell), "a long is flattened with a reduce-only sell: {:?}", orders[1]);
    assert_eq!(orders[1].size, d("100"));
    assert!(is_take(&orders[2], "down"), "{:?}", orders[2]);
    // 100 contracts at 0.76 released $76; down costs 1 - 0.44 + 0.01 = 0.57 -> 133, capped at 100.
    assert_eq!(orders[2].size, d("100"));
    assert_eq!(w.leg().await, "holding_down");
    w.venue.fill(2, d("100"), d("0.44"));
    w.fill_last("0.44", "-100", "0.44").await;

    // Falling further while holding down: down_cheap (down < 0.5) is false, no flip.
    w.up("0.4").await;
    w.up("0.35").await;
    assert_eq!(w.submitted().len(), 3, "holding down rides the move down");

    // Up back above 50 cents: down is cheap, flip back with a reduce-only BUY.
    w.up("0.55").await;
    let orders = w.submitted();
    assert_eq!(orders.len(), 5, "{:#?}", w.log().await);
    assert!(is_flatten(&orders[3], Side::Buy), "a short is flattened with a reduce-only buy: {:?}", orders[3]);
    assert!(is_take(&orders[4], "up"));
    assert_eq!(w.leg().await, "holding_up");
    w.assert_no_rejects().await;
}

#[tokio::test(start_paused = true)]
async fn flip_stop_timer_ends_the_round_in_any_holding_leg() {
    let mut w = World::new("momentum-flip").await;
    w.up("0.75").await;
    w.fill_last("0.76", "100", "0.76").await;
    assert_eq!(w.leg().await, "holding_up");
    tokio::time::advance(Duration::from_millis(50_100)).await;
    settle().await;
    assert_eq!(w.leg().await, "done");
    w.up("0.45").await;
    assert_eq!(w.submitted().len(), 1, "done arms nothing: no flip after the stop");
}

#[tokio::test(start_paused = true)]
async fn scale_in_adds_on_the_way_down_at_most_three_times() {
    let mut w = World::new("scale-in").await;
    w.up("0.56").await;
    let orders = w.submitted();
    assert_eq!(orders.len(), 1, "{:#?}", w.log().await);
    assert_eq!(orders[0].price, Some(d("0.58")), "a cent through the 0.57 ask");
    assert_eq!(w.leg().await, "holding");
    w.fill_last("0.57", "100", "0.57").await;

    // Above the last entry: nothing. At 5 cents under it (ask 0.52 <= 0.57 - 0.05): add.
    w.up("0.6").await;
    assert_eq!(w.submitted().len(), 1);
    w.up("0.51").await;
    assert_eq!(w.submitted().len(), 2, "{:#?}", w.log().await);
    assert_eq!(w.submitted()[1].price, Some(d("0.53")), "the add takes a cent through the new ask");
    // The same level again is not 5 cents under the new last entry (0.52).
    w.up("0.51").await;
    assert_eq!(w.submitted().len(), 2, "no add without a fresh 5 cent drop");
    w.up("0.46").await;
    w.up("0.41").await;
    assert_eq!(w.submitted().len(), 4, "{:#?}", w.log().await);
    // A fourth drop is refused by the counter, not the market.
    w.up("0.36").await;
    w.up("0.31").await;
    assert_eq!(w.submitted().len(), 4, "at most three adds");
    assert_eq!(w.leg().await, "holding", "hold to settlement");
    let status = w.runtime.status(&w.id).await.unwrap();
    assert_eq!(status.state["adds"], crate::automation::value::Value::Decimal(d("3")));
    w.assert_no_rejects().await;
}

#[tokio::test(start_paused = true)]
async fn quoter_rests_both_sides_requotes_and_pulls_on_the_stop_timer() {
    let mut w = World::new("two-sided-quoter").await;
    // A 2 cent spread is not wide enough.
    w.book_at("0.5", 1).await;
    assert!(w.submitted().is_empty());
    // Publish a wide book: bid 0.5, ask 0.55.
    w.platform.ingest(SignalEvent::new(
        BOOK,
        2,
        [("up", SignalValue::decimal("0.525")), ("down", SignalValue::decimal("0.475")), ("bid", SignalValue::decimal("0.5")), ("ask", SignalValue::decimal("0.55")), ("last", SignalValue::decimal("0.52"))],
    ));
    settle().await;
    let orders = w.submitted();
    assert_eq!(orders.len(), 2, "{:#?}", w.log().await);
    assert_eq!((orders[0].side, orders[0].price, orders[0].tif), (Side::Buy, Some(d("0.51")), Tif::Gtc));
    assert_eq!((orders[1].side, orders[1].price, orders[1].tif), (Side::Sell, Some(d("0.54")), Tif::Gtc));
    assert_eq!(w.leg().await, "quoting");
    assert_eq!(w.venue.live_ids(), vec![1, 2]);

    // Every 2 seconds both tags are re-quoted in place.
    tokio::time::advance(Duration::from_millis(2_050)).await;
    settle().await;
    assert_eq!(w.venue.state.lock().unwrap().replaced.len(), 2);
    assert_eq!(w.venue.live_ids(), vec![3, 4]);

    // The bid fills: only the ask is re-quoted from then on.
    w.venue.fill(3, d("100"), d("0.51"));
    w.position("100", "0.51");
    settle().await;
    assert_eq!(w.leg().await, "quoting", "a fill does not leave the quoting leg");
    tokio::time::advance(Duration::from_millis(2_000)).await;
    settle().await;
    assert_eq!(w.venue.state.lock().unwrap().replaced.len(), 3, "a filled tag is not replaced");
    assert_eq!(w.venue.live_ids(), vec![5]);

    // Ten seconds before the close: pull everything and finish.
    tokio::time::advance(Duration::from_millis(50_000)).await;
    settle().await;
    assert_eq!(w.leg().await, "done");
    assert!(w.venue.live_ids().is_empty(), "both tags pulled");
    w.assert_no_rejects().await;
}

#[tokio::test(start_paused = true)]
async fn re_entry_hunts_again_after_an_exit_up_to_three_times() {
    let mut w = World::new("re-entry").await;
    let mut entries = 0;
    for _ in 0..3 {
        // A fresh crossing above 60 cents: the trigger needs a false in between.
        w.up("0.55").await;
        w.up("0.65").await;
        entries += 1;
        let orders = w.submitted();
        assert_eq!(orders.len(), entries * 2 - 1, "entry {entries}: {:#?}", w.log().await);
        assert!(is_take(orders.last().unwrap(), "up"));
        assert_eq!(w.leg().await, "holding");
        w.fill_last("0.66", "100", "0.66").await;
        // Take profit at bid >= avg + 0.05: up 0.72 makes bid 0.71.
        w.up("0.72").await;
        let orders = w.submitted();
        assert_eq!(orders.len(), entries * 2, "exit {entries}: {:#?}", w.log().await);
        assert!(is_flatten(orders.last().unwrap(), Side::Sell));
        assert_eq!(w.leg().await, "hunting", "an exit returns to hunting");
        w.fill_last("0.71", "0", "0").await;
    }
    // A fourth crossing is refused by the entry counter.
    w.up("0.55").await;
    w.up("0.65").await;
    assert_eq!(w.submitted().len(), 6, "at most three entries per round");
    assert_eq!(w.leg().await, "hunting");
    w.assert_no_rejects().await;
}

#[tokio::test(start_paused = true)]
async fn re_entry_cut_loss_also_returns_to_hunting() {
    let mut w = World::new("re-entry").await;
    w.up("0.65").await;
    w.fill_last("0.66", "100", "0.66").await;
    w.up("0.6").await;
    assert_eq!(w.submitted().len(), 1, "9 cents down is not a cut");
    w.up("0.55").await;
    let orders = w.submitted();
    assert_eq!(orders.len(), 2, "{:#?}", w.log().await);
    assert!(is_flatten(&orders[1], Side::Sell));
    assert_eq!(w.leg().await, "hunting");
}

#[tokio::test(start_paused = true)]
async fn every_time_cooldown_fires_at_most_once_per_five_seconds() {
    let mut w = World::new("every-time-cooldown").await;
    w.book_at("0.5", 0).await;
    assert!(w.submitted().is_empty(), "fair has not published: the leaf is false");
    // Fair 9 cents over the ask: fires, and keeps being true on every publish.
    w.fair_at("0.6", 1).await;
    assert_eq!(w.submitted().len(), 1, "{:#?}", w.log().await);
    assert_eq!(w.submitted()[0].size, d("76"), "floor(400 * 0.1 / 0.52): a tenth of the round budget at a cent through the ask");
    w.fair_at("0.61", 1_000).await;
    w.book_at("0.5", 2_000).await;
    w.fair_at("0.62", 4_999).await;
    assert_eq!(w.submitted().len(), 1, "inside the 5 second cooldown");
    w.fair_at("0.62", 6_001).await;
    assert_eq!(w.submitted().len(), 2, "{:#?}", w.log().await);
    assert_eq!(w.leg().await, "trading", "no then: the leg keeps trading");
    // The stop timer ends the round.
    tokio::time::advance(Duration::from_millis(50_100)).await;
    settle().await;
    assert_eq!(w.leg().await, "done");
    w.fair_at("0.7", 60_000).await;
    assert_eq!(w.submitted().len(), 2);
    w.assert_no_rejects().await;
}
