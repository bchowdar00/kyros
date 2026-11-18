//! End-to-end: real platform task, real manager and deployment tasks, the
//! compiled `fair-discount` program, a fake venue, and a paused Tokio clock.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rust_decimal::Decimal;
use strategy_compiler::ir::{OrderType, Side};

use super::deployment::DeploymentSpec;
use super::manager::{RuntimeHandle, runtime_channel, spawn_manager};
use super::test_support::*;
use crate::automation::signals::{SignalEvent, SignalStore, SignalValue};
use crate::automation::sme::VenueClock;
use crate::automation::sme::task::{PlatformHandle, spawn_platform};
use crate::automation::venue::{AccountView, FakeVenue, RejectReason};
use crate::types::{Address, SeriesId};

const INSTRUMENT: &str = "btc_usd.updown_1m.current";

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
    clock: Arc<TestClock>,
    program: Arc<strategy_compiler::program::Program>,
    ts: u64,
}

impl World {
    async fn new() -> World {
        let clock = Arc::new(TestClock::default());
        clock.remaining.lock().unwrap().insert(INSTRUMENT.into(), 60_000);
        let store = SignalStore::from_registry(&tiny_registry());
        let platform = spawn_platform(store, clock.clone());
        platform.enable(FAIR);
        platform.enable(BOOK);
        let (runtime, rx) = runtime_channel();
        let venue = FakeVenue::new(runtime.clone());
        spawn_manager(rx, platform.clone(), venue.clone(), clock.clone());
        let program = Arc::new(compile_fixture("fair-discount"));
        World { platform, runtime, venue, clock, program, ts: 0 }
    }

    /// The same world, running `fair-discount` compiled against `envelope`.
    async fn with_envelope(envelope: &str) -> World {
        let mut w = World::new().await;
        let envelope: strategy_compiler::ir::Envelope = serde_json::from_str(envelope).expect("envelope parses");
        w.program = Arc::new(compile_fixture_with("fair-discount", &envelope));
        w
    }

    async fn deploy(&self, epoch: u64, account: Address, series: SeriesId) -> String {
        let spec = DeploymentSpec { program: self.program.clone(), deploy_epoch: epoch, sub_account: account, series, params: BTreeMap::new() };
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
}

/// Let every task on the current-thread runtime process its queues.
async fn settle() {
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
}

const ACCT: Address = Address([0x11; 20]);
const ACCT2: Address = Address([0x22; 20]);

#[tokio::test(start_paused = true)]
async fn fair_discount_hunts_requotes_pulls_and_resets() {
    let mut w = World::new().await;
    let id = w.deploy(0, ACCT, SeriesId(0)).await;
    assert_eq!(w.leg(&id).await, "hunting");

    // Book first (no trigger reads it alone), then fair 5 cents above ask.
    w.book("0.55");
    w.fair("0.6");
    settle().await;
    let orders = w.venue.submitted();
    assert_eq!(orders.len(), 1, "one bid submitted");
    assert_eq!(w.venue.state.lock().unwrap().view_reads, 1, "one account read serves the interpreter and the risk gate");
    assert_eq!(orders[0].side, Side::Buy);
    assert_eq!(orders[0].order_type, OrderType::Limit);
    assert_eq!(orders[0].price, Some(d("0.59")));
    assert_eq!(orders[0].size, d("100"));
    assert_eq!(orders[0].outcome.as_deref(), Some("up"));
    assert!(orders[0].client_order_id.starts_with(&format!("{}/0/hunting.bid_up.1/1/0", w.program.strategy_id)));
    assert_eq!(w.leg(&id).await, "quoting");
    let status = w.runtime.status(&id).await.unwrap();
    assert_eq!(status.live_orders.len(), 1);
    assert_eq!(status.state["quotes"], crate::automation::value::Value::Decimal(d("1")));

    // Fair moves: cancel-replace the bid one cent under the new fair.
    w.fair("0.62");
    settle().await;
    let replaced = w.venue.state.lock().unwrap().replaced.clone();
    assert_eq!(replaced.len(), 1);
    assert_eq!(replaced[0].0, 1, "the original bid was replaced");
    assert_eq!(replaced[0].1.price, Some(d("0.61")));
    assert_eq!(w.venue.live_ids(), vec![2]);
    assert_eq!(w.leg(&id).await, "quoting", "`replaced` never moves the leg");

    // `cheap` is not armed in quoting: a cheaper book does not place again.
    w.book("0.4");
    settle().await;
    assert_eq!(w.venue.submitted().len(), 2, "no third submission");

    // The window_clock timer (60s - 10s) pulls the bid and finishes.
    tokio::time::advance(Duration::from_millis(50_100)).await;
    settle().await;
    assert_eq!(w.venue.state.lock().unwrap().cancelled, vec![2]);
    assert_eq!(w.leg(&id).await, "done");
    assert!(w.venue.live_ids().is_empty());
    let status = w.runtime.status(&id).await.unwrap();
    assert!(status.live_orders.is_empty());
    assert!(status.armed_triggers.is_empty(), "done arms nothing");

    // Settlement resets the session: back to hunting, masked until a window opens.
    w.runtime.window_settled(SeriesId(0), 1);
    settle().await;
    assert_eq!(w.leg(&id).await, "hunting");
    let status = w.runtime.status(&id).await.unwrap();
    assert_eq!(status.state["quotes"], crate::automation::value::Value::Decimal(d("0")), "state wiped");
    assert!(status.armed_triggers.is_empty(), "awaiting the next window");
    w.fair("0.7");
    settle().await;
    assert_eq!(w.venue.submitted().len(), 2, "masked: no order while awaiting the window");

    w.clock.remaining.lock().unwrap().insert(INSTRUMENT.into(), 60_000);
    w.runtime.window_opened(SeriesId(0), 2, vec![INSTRUMENT.into()]);
    settle().await;
    w.fair("0.7");
    settle().await;
    assert_eq!(w.venue.submitted().len(), 3, "re-armed: hunting bids again");
    assert_eq!(w.leg(&id).await, "quoting");
}

/// A burst of fair-value frames while quoting collapses into one requote
/// at the last price; the absorbed activations are counted.
#[tokio::test(start_paused = true)]
async fn burst_of_same_cause_activations_coalesces_into_one_requote() {
    let mut w = World::new().await;
    let id = w.deploy(0, ACCT, SeriesId(0)).await;
    w.book("0.55");
    w.fair("0.6");
    settle().await;
    assert_eq!(w.leg(&id).await, "quoting");
    assert_eq!(w.runtime.status(&id).await.unwrap().coalesced, 0);

    // Twenty fair moves queued before the deployment task gets to run.
    for i in 1..=20 {
        w.fair(&format!("0.{}", 60 + i));
    }
    settle().await;
    let replaced = w.venue.state.lock().unwrap().replaced.clone();
    assert_eq!(replaced.len(), 1, "one requote for the whole burst");
    assert_eq!(replaced[0].1.price, Some(d("0.79")), "at the latest fair (0.80 - 0.01)");
    let status = w.runtime.status(&id).await.unwrap();
    assert_eq!(status.coalesced, 19);
    assert_eq!(status.activations, 2, "the entry bid and one requote");
}

/// Requotes past the per-second cap are refused locally through the
/// intent's `on.reject` (a `cancel_replace` has none, so the leg stays),
/// counted, and admitted again once the second has passed.
#[tokio::test(start_paused = true)]
async fn order_rate_gate_rejects_the_excess_and_recovers() {
    use super::deployment::MAX_ORDERS_PER_SEC;

    let mut w = World::new().await;
    let id = w.deploy(0, ACCT, SeriesId(0)).await;
    w.book("0.55");
    w.fair("0.6");
    settle().await;
    assert_eq!(w.leg(&id).await, "quoting");
    // The entry bid used one submission of this second's budget.
    let budget = MAX_ORDERS_PER_SEC - 1;

    for i in 0..budget + 5 {
        w.fair(&format!("0.{}", 61 + (i % 30)));
        settle().await;
    }
    let replaced = w.venue.state.lock().unwrap().replaced.len();
    assert_eq!(replaced, budget, "exactly the budget reached the venue");
    let status = w.runtime.status(&id).await.unwrap();
    assert_eq!(status.rate_rejects, 5);
    assert_eq!(status.current_leg, "quoting", "a refused requote leaves the resting bid alone");
    assert_eq!(status.live_orders.len(), 1);
    assert!(status.log.iter().any(|l| l.contains("envelope_violation") && l.contains("order rate exceeds")), "{:#?}", status.log);

    // A second later the gate admits again.
    tokio::time::advance(Duration::from_millis(1_001)).await;
    w.fair("0.7");
    settle().await;
    assert_eq!(w.venue.state.lock().unwrap().replaced.len(), budget + 1);
    assert_eq!(w.runtime.status(&id).await.unwrap().rate_rejects, 5);
}

/// `coalesce` absorbs only a run of same-cause activations: a frame from
/// another trigger or signal ends the run and is handed back to be
/// processed next, in order, and a later same-cause run starts fresh.
#[test]
fn coalesce_stops_at_a_frame_with_a_different_cause() {
    use crate::automation::sme::Frame;
    use super::deployment::coalesce;

    fn act(signal: Option<&str>, fired: &[&str], ts: u64) -> crate::automation::sme::Activation {
        let mut a = activation(fired, &[], ts);
        a.event_signal = signal.map(str::to_string);
        a
    }
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    for a in [act(Some(FAIR), &["fair_moved"], 2), act(Some(FAIR), &["fair_moved"], 3), act(None, &["last_10s"], 4), act(Some(FAIR), &["fair_moved"], 5), act(Some(BOOK), &["fair_moved"], 6)] {
        tx.send(Frame::Activation(a)).unwrap();
    }
    let (run, absorbed, rest) = coalesce(act(Some(FAIR), &["fair_moved"], 1), &mut rx);
    assert_eq!((run.ts_ms, absorbed), (3, 2), "the run of three fair fires collapses to the last");
    let Some(Frame::Activation(timer)) = rest else { panic!("the timer fire ends the run") };
    assert_eq!(timer.ts_ms, 4);

    let (run, absorbed, rest) = coalesce(timer, &mut rx);
    assert_eq!((run.ts_ms, absorbed), (4, 0), "a timer fire is never merged with a signal fire");
    let Some(Frame::Activation(fair)) = rest else { panic!("the next fair fire is handed back") };

    let (run, absorbed, rest) = coalesce(fair, &mut rx);
    assert_eq!((run.ts_ms, absorbed), (5, 0), "same trigger, different signal: not the same cause");
    let Some(Frame::Activation(book)) = rest else { panic!() };
    let (run, absorbed, rest) = coalesce(book, &mut rx);
    assert_eq!((run.ts_ms, absorbed), (6, 0));
    assert!(rest.is_none(), "channel drained");

    // A newer session is never absorbed into an older one.
    let mut newer = act(Some(FAIR), &["fair_moved"], 8);
    newer.session = 1;
    tx.send(Frame::Activation(newer)).unwrap();
    let (run, absorbed, rest) = coalesce(act(Some(FAIR), &["fair_moved"], 7), &mut rx);
    assert_eq!((run.ts_ms, absorbed), (7, 0));
    assert!(matches!(rest, Some(Frame::Activation(a)) if a.session == 1));
}

#[tokio::test(start_paused = true)]
async fn fill_moves_leg_and_records_fill_ledger() {
    let mut w = World::new().await;
    let id = w.deploy(0, ACCT, SeriesId(0)).await;
    w.book("0.55");
    w.fair("0.6");
    settle().await;
    assert_eq!(w.leg(&id).await, "quoting");
    w.venue.fill(1, d("100"), d("0.59"));
    settle().await;
    assert_eq!(w.leg(&id).await, "done", "on.fill = done");
    let status = w.runtime.status(&id).await.unwrap();
    assert!(status.log.iter().any(|l| l.contains("[lifecycle] hunting.bid_up.1 fill -> done")), "{:#?}", status.log);
    assert!(status.live_orders.is_empty());
}

#[tokio::test(start_paused = true)]
async fn venue_reject_follows_on_reject_transition() {
    let mut w = World::new().await;
    let id = w.deploy(0, ACCT, SeriesId(0)).await;
    w.venue.reject_next(RejectReason::InsufficientFunds);
    w.book("0.55");
    w.fair("0.6");
    settle().await;
    // Batch: place (refused -> queued reject), switch to quoting; drain: reject -> hunting.
    assert_eq!(w.leg(&id).await, "hunting");
    let status = w.runtime.status(&id).await.unwrap();
    assert!(status.log.iter().any(|l| l.contains("[reject] hunting.bid_up.1: insufficient_funds")), "{:#?}", status.log);
    assert!(status.live_orders.is_empty());
    assert!(w.venue.live_ids().is_empty());
}

#[tokio::test(start_paused = true)]
async fn envelope_gate_rejects_locally_before_the_venue() {
    let mut w = World::new().await;
    let id = w.deploy(0, ACCT, SeriesId(0)).await;
    // Position already at the cap: another 100 long breaks max_position_per_market = 200.
    w.venue.set_view(SeriesId(0), ACCT, AccountView { position_size: d("150"), ..Default::default() });
    w.book("0.55");
    w.fair("0.6");
    settle().await;
    assert!(w.venue.submitted().is_empty(), "never reached the venue");
    let status = w.runtime.status(&id).await.unwrap();
    assert!(status.log.iter().any(|l| l.contains("envelope_violation") && l.contains("max_position_per_market")), "{:#?}", status.log);
    assert_eq!(status.current_leg, "hunting");
}

#[tokio::test(start_paused = true)]
async fn round_budget_rejects_this_window_and_rearms_after_settlement() {
    // A $50 round budget: the bid (100 × 0.59 = $59) is refused outright, not
    // shrunk, while this window's loss counts against the budget.
    let mut w = World::with_envelope(
        r#"{"allocation": "1000", "loss_budget": "500", "round_budget": "50", "max_order_size": "100", "max_position_per_market": "200", "max_open_orders": "8"}"#,
    )
    .await;
    let id = w.deploy(0, ACCT, SeriesId(0)).await;
    w.venue.set_view(SeriesId(0), ACCT, AccountView { realized_pnl: d("-10"), realized_pnl_lifetime: d("-10"), ..Default::default() });
    w.book("0.55");
    w.fair("0.6");
    settle().await;
    assert!(w.venue.submitted().is_empty(), "never reached the venue");
    let status = w.runtime.status(&id).await.unwrap();
    assert!(status.log.iter().any(|l| l.contains("envelope_violation") && l.contains("round_budget")), "{:#?}", status.log);
    assert_eq!(status.current_leg, "hunting", "on.reject keeps hunting");

    // Settlement clears the window: position, resting orders and round PnL
    // are gone (the engine zeroes them; the fake view is reset by hand), and
    // the same bid now fits a fresh $50.
    w.venue.settle_series(SeriesId(0));
    w.runtime.window_settled(SeriesId(0), 1);
    settle().await;
    w.venue.set_view(SeriesId(0), ACCT, AccountView { realized_pnl_lifetime: d("-10"), ..Default::default() });
    w.runtime.window_opened(SeriesId(0), 2, vec![INSTRUMENT.into()]);
    settle().await;
    w.fair("0.5");
    w.book("0.45");
    settle().await;
    assert_eq!(w.venue.submitted().len(), 1, "{:#?}", w.runtime.status(&id).await.unwrap().log);
    assert_eq!(w.leg(&id).await, "quoting");
}

#[tokio::test(start_paused = true)]
async fn loss_budget_breach_is_fatal_and_stops_the_deployment() {
    let mut w = World::new().await;
    let id = w.deploy(0, ACCT, SeriesId(0)).await;
    w.book("0.55");
    w.fair("0.6");
    settle().await;
    assert_eq!(w.venue.live_ids(), vec![1]);
    w.venue.set_view(SeriesId(0), ACCT, AccountView { realized_pnl: d("-600"), realized_pnl_lifetime: d("-600"), ..Default::default() });
    w.venue.fill(1, d("10"), d("0.59"));
    settle().await;
    // The deployment task ended; the manager reaps it on its next command.
    assert!(w.runtime.status(&id).await.is_none() || w.runtime.status(&id).await.is_none());
    assert!(w.venue.live_ids().is_empty(), "remaining order cancelled at stop");
    assert!(w.runtime.ids().await.is_empty());
}

#[tokio::test(start_paused = true)]
async fn pause_drops_activations_and_resume_rearms() {
    let mut w = World::new().await;
    let id = w.deploy(0, ACCT, SeriesId(0)).await;
    w.runtime.send(super::manager::RuntimeCommand::Pause { id: id.clone() });
    settle().await;
    w.book("0.55");
    w.fair("0.6");
    settle().await;
    assert!(w.venue.submitted().is_empty());
    assert!(w.runtime.status(&id).await.unwrap().paused);
    w.runtime.send(super::manager::RuntimeCommand::Resume { id: id.clone() });
    settle().await;
    w.fair("0.61");
    settle().await;
    assert_eq!(w.venue.submitted().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn settlement_in_one_series_leaves_the_other_untouched() {
    let mut w = World::new().await;
    let a = w.deploy(0, ACCT, SeriesId(0)).await;
    let b = w.deploy(1, ACCT2, SeriesId(1)).await;
    w.book("0.55");
    w.fair("0.6");
    settle().await;
    assert_eq!(w.leg(&a).await, "quoting");
    assert_eq!(w.leg(&b).await, "quoting");
    assert_eq!(w.venue.submitted().len(), 2);

    // The engine cancels every resting order in the settling series (acks
    // first), then reports the settlement.
    let touched = w.venue.settle_series(SeriesId(0));
    assert_eq!(touched, vec![ACCT]);
    w.runtime.window_settled(SeriesId(0), 1);
    settle().await;
    assert_eq!(w.leg(&a).await, "hunting", "2m deployment reset");
    assert_eq!(w.leg(&b).await, "quoting", "15m deployment untouched");
    assert_eq!(w.venue.live_ids(), vec![2], "only b's bid survives");
    let a_log = w.runtime.status(&a).await.unwrap().log;
    assert!(a_log.iter().any(|l| l.contains("cancel_ack -> stay")), "lock-phase ack applied before reset: {a_log:#?}");
    let b_status = w.runtime.status(&b).await.unwrap();
    assert_eq!(b_status.live_orders.len(), 1);
    assert_eq!(b_status.armed_triggers, vec!["fair_moved".to_string(), "last_10s".to_string()]);
    let a_status = w.runtime.status(&a).await.unwrap();
    assert!(a_status.armed_triggers.is_empty());

    // Stopping one deployment cancels only its orders.
    let stopped = w.runtime.stop(&b).await.unwrap();
    assert!(stopped.stopping);
    assert!(w.venue.live_ids().is_empty(), "b's order cancelled; a's was already dead at reset");
    assert_eq!(w.runtime.ids().await, vec![a.clone()]);
    // Duplicate (strategy, epoch) is refused while resident.
    let dup = DeploymentSpec { program: w.program.clone(), deploy_epoch: 0, sub_account: ACCT, series: SeriesId(0), params: BTreeMap::new() };
    assert!(matches!(w.runtime.start(dup).await, Err(super::manager::StartError::Duplicate(_))));
}
