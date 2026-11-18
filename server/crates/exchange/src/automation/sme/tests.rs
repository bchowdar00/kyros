//! Ports of the prototype's `test_generated_triggers.py`, plus coverage for
//! the v0.3 additions (params, `after`/`interval`, cooldown, sessions).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use rust_decimal::Decimal;
use strategy_compiler::program::TriggerDef;
use tokio::sync::mpsc;

use super::*;
use crate::automation::signals::tests::ab_store;
use crate::automation::signals::{SignalEvent, SignalValue};

/// A clock whose windows and time are set by the test.
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

/// The recording sink shared with the test so requests can be inspected.
#[derive(Default, Clone)]
struct SharedSink(Arc<Mutex<Vec<TimerRequest>>>);

impl TimerSink for SharedSink {
    fn schedule(&mut self, request: TimerRequest) {
        self.0.lock().unwrap().push(request);
    }
}

struct Rig {
    platform: SignalsPlatform,
    clock: Arc<TestClock>,
    sink: SharedSink,
}

fn rig() -> Rig {
    let clock = Arc::new(TestClock::default());
    let sink = SharedSink::default();
    let platform = SignalsPlatform::new(ab_store(), clock.clone(), Box::new(sink.clone()));
    Rig { platform, clock, sink }
}

fn trigger(json: serde_json::Value) -> TriggerDef {
    serde_json::from_value(json).unwrap()
}

fn field_leaf(signal: &str, field: &str, cmp: &str, value: &str) -> serde_json::Value {
    serde_json::json!({"kind": "field", "signal_id": signal, "field": field, "comparator": cmp,
        "value": {"kind": "literal", "value": value}})
}

fn cond(name: &str, shape: &str, leaves: Vec<serde_json::Value>, fire: &str, rearm: &str) -> TriggerDef {
    trigger(serde_json::json!({"name": name, "source": {"kind": shape, "leaves": leaves}, "fire": fire, "rearm": rearm}))
}

fn timer(name: &str, source: serde_json::Value, fire: &str, rearm: &str) -> TriggerDef {
    trigger(serde_json::json!({"name": name, "source": source, "fire": fire, "rearm": rearm}))
}

fn connect(
    platform: &mut SignalsPlatform,
    owner: &str,
    subs: &[&str],
    triggers: Vec<TriggerDef>,
    mask: &[&str],
) -> (ConnId, mpsc::UnboundedReceiver<Frame>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let id = platform
        .connect(ConnectSpec {
            owner: Some(owner.to_string()),
            subscriptions: subs.iter().map(|s| s.to_string()).collect(),
            triggers,
            initial_mask: mask.iter().map(|s| s.to_string()).collect(),
            params: BTreeMap::new(),
            tx,
        })
        .unwrap();
    (id, rx)
}

fn a(x: &str, y: &str, ts: u64) -> SignalEvent {
    SignalEvent::new("a", ts, [("x", SignalValue::decimal(x)), ("y", SignalValue::decimal(y))])
}

fn b(kind: &str, ts: u64) -> SignalEvent {
    SignalEvent::new("b", ts, [("kind", SignalValue::text(kind))])
}

fn activations(rx: &mut mpsc::UnboundedReceiver<Frame>) -> Vec<Activation> {
    let mut out = Vec::new();
    while let Ok(Frame::Activation(act)) = rx.try_recv() {
        out.push(act);
    }
    out
}

fn names(v: &[&str]) -> BTreeSet<String> {
    v.iter().map(|s| s.to_string()).collect()
}

fn d(s: &str) -> Value {
    Value::Decimal(s.parse::<Decimal>().unwrap())
}

#[test]
fn crossing_while_disarmed_is_lost() {
    let mut r = rig();
    let edge = cond("edge", "all", vec![field_leaf("a", "x", ">", "0.3")], "on_crossing", "fresh_crossing");
    let (id, mut rx) = connect(&mut r.platform, "s", &["a"], vec![edge], &[]);
    r.platform.ingest(&a("0.4", "1", 1)).unwrap();
    r.platform.set_mask(id, names(&["edge"]));
    r.platform.ingest(&a("0.5", "2", 2)).unwrap();
    assert!(activations(&mut rx).is_empty());
    r.platform.ingest(&a("0.2", "2", 3)).unwrap();
    r.platform.ingest(&a("0.4", "2", 4)).unwrap();
    assert_eq!(activations(&mut rx)[0].fired, names(&["edge"]));
}

/// A cleared snapshot makes `always` and field rows false at once, and the
/// next ready event is a fresh crossing; clearing a never-published signal
/// evaluates nothing.
#[test]
fn clear_snapshot_falsifies_rows_until_the_next_event() {
    let mut r = rig();
    let seen = cond("seen", "all", vec![serde_json::json!({"kind": "always", "signal_id": "a"})], "if_true", "immediate");
    let high = cond("high", "all", vec![field_leaf("a", "x", ">", "0.3")], "on_crossing", "fresh_crossing");
    let (_, mut rx) = connect(&mut r.platform, "s", &["a"], vec![seen, high], &["seen", "high"]);
    r.platform.clear_snapshot("a");
    assert!(activations(&mut rx).is_empty(), "never published: nothing to clear");
    assert_eq!(r.platform.stats().rows.iter().map(|row| row.evaluations).sum::<u64>(), 0);

    r.platform.ingest(&a("0.5", "1", 1)).unwrap();
    assert_eq!(activations(&mut rx)[0].fired, names(&["high", "seen"]));
    let evaluated_before = r.platform.stats().rows.iter().map(|row| row.evaluations).sum::<u64>();

    r.platform.clear_snapshot("a");
    assert!(!r.platform.store().has_published("a"));
    assert!(activations(&mut rx).is_empty(), "false rows never fire");
    let evaluated_after = r.platform.stats().rows.iter().map(|row| row.evaluations).sum::<u64>();
    assert_eq!(evaluated_after, evaluated_before + 2, "both rows depending on `a` re-evaluated");

    // Still high on the next event: a crossing again, because the clear
    // took the row through false.
    r.platform.ingest(&a("0.5", "1", 2)).unwrap();
    assert_eq!(activations(&mut rx)[0].fired, names(&["high", "seen"]));
}

/// The per-signal row index behind `clear_snapshot` follows registration:
/// shared rows count once, a row reading two signals sits under both, and
/// the last unregistration removes the entry.
#[test]
fn rows_are_indexed_by_signal_for_clears() {
    let mut r = rig();
    assert_eq!(r.platform.rows_reading("a"), 0);
    let high = || cond("high", "all", vec![field_leaf("a", "x", ">", "0.3")], "if_true", "immediate");
    let both = cond("both", "all", vec![field_leaf("a", "y", ">", "0"), field_leaf("b", "kind", "==", "yes")], "if_true", "immediate");
    let (c1, _rx1) = connect(&mut r.platform, "s1", &["a", "b"], vec![high(), both], &["high", "both"]);
    let (c2, _rx2) = connect(&mut r.platform, "s2", &["a"], vec![high()], &["high"]);
    assert_eq!(r.platform.rows_reading("a"), 2, "`high` is shared by both connections; `both` reads a too");
    assert_eq!(r.platform.rows_reading("b"), 1);

    r.platform.disconnect(c2);
    assert_eq!(r.platform.rows_reading("a"), 2, "`high` still has a subscriber");
    r.platform.disconnect(c1);
    assert_eq!(r.platform.rows_reading("a"), 0);
    assert_eq!(r.platform.rows_reading("b"), 0);
    assert_eq!(r.platform.stats().unique_rows, 0);
}

#[test]
fn suppressed_never_is_spent() {
    let mut r = rig();
    let once = cond("once", "all", vec![serde_json::json!({"kind": "always", "signal_id": "a"})], "if_true", "never");
    let (id, mut rx) = connect(&mut r.platform, "s", &["a"], vec![once], &[]);
    r.platform.ingest(&a("1", "2", 1)).unwrap();
    r.platform.set_mask(id, names(&["once"]));
    r.platform.ingest(&a("2", "3", 2)).unwrap();
    assert!(activations(&mut rx).is_empty());
}

#[test]
fn exact_expr_and_atomic_typed_payload() {
    let mut r = rig();
    let sum = cond(
        "sum",
        "all",
        vec![
            serde_json::json!({"kind": "expr", "expr": {"op": "+", "lhs": {"op": "signal", "signal_id": "a", "field": "x"}, "rhs": {"op": "signal", "signal_id": "a", "field": "y"}},
                "comparator": "==", "value": {"kind": "literal", "value": "0.3"}}),
            field_leaf("b", "kind", "==", "yes"),
        ],
        "if_true",
        "immediate",
    );
    let (_id, mut rx) = connect(&mut r.platform, "s", &["a", "b"], vec![sum], &["sum"]);
    r.platform.ingest(&a("0.1", "0.2", 1)).unwrap();
    r.platform.ingest(&b("yes", 2)).unwrap();
    let acts = activations(&mut rx);
    assert_eq!(acts.len(), 1);
    let act = &acts[0];
    assert_eq!(act.event.as_ref().unwrap().get("kind"), Some(&Value::Text("yes".into())));
    assert_eq!(act.event_signal.as_deref(), Some("b"));
    assert_eq!(act.snapshots["a"], [("x".to_string(), d("0.1")), ("y".to_string(), d("0.2"))].into_iter().collect());
    assert_eq!(act.ts_ms, 2);
}

#[test]
fn one_event_delivers_every_matching_cause() {
    let mut r = rig();
    let row = |name: &str, field: &str| cond(name, "any", vec![field_leaf("a", field, ">", "0")], "if_true", "immediate");
    let (_id, mut rx) = connect(&mut r.platform, "s", &["a"], vec![row("x_positive", "x"), row("y_positive", "y")], &["x_positive", "y_positive"]);
    r.platform.ingest(&a("1", "2", 1)).unwrap();
    let acts = activations(&mut rx);
    assert_eq!(acts.len(), 1);
    assert_eq!(acts[0].fired, names(&["x_positive", "y_positive"]));
}

#[test]
fn shared_predicate_evaluates_once_with_independent_subscribers() {
    let mut r = rig();
    let leaf = field_leaf("a", "x", ">", "0.3");
    let (first, mut rx1) = connect(&mut r.platform, "Strategy_x:1", &["a"], vec![cond("edge", "all", vec![leaf.clone()], "on_crossing", "fresh_crossing")], &["edge"]);
    let (second, mut rx2) = connect(&mut r.platform, "Strategy_x:2", &["a"], vec![cond("one_shot", "all", vec![leaf], "if_true", "never")], &[]);

    let stats = r.platform.stats();
    assert_eq!(stats.logical_registrations, 2);
    assert_eq!(stats.unique_rows, 1);
    assert_eq!(stats.dedup_ratio, 2.0);
    assert_eq!(stats.subscriber_strategies, 2);

    r.platform.ingest(&a("0.4", "1", 1)).unwrap();
    assert_eq!(r.platform.stats().rows[0].evaluations, 1);
    assert_eq!(activations(&mut rx1)[0].fired, names(&["edge"]));
    assert!(activations(&mut rx2).is_empty());
    r.platform.set_mask(second, names(&["one_shot"]));
    r.platform.ingest(&a("0.5", "2", 2)).unwrap();
    assert!(activations(&mut rx1).is_empty());
    assert!(activations(&mut rx2).is_empty(), "spent while masked");

    r.platform.ingest(&a("0.2", "2", 3)).unwrap();
    r.platform.ingest(&a("0.4", "2", 4)).unwrap();
    assert_eq!(activations(&mut rx1)[0].fired, names(&["edge"]));
    assert!(activations(&mut rx2).is_empty());

    r.platform.disconnect(first);
    assert_eq!(r.platform.stats().logical_registrations, 1);
    r.platform.disconnect(second);
    assert_eq!(r.platform.stats().unique_rows, 0);
}

#[test]
fn timers_are_deployment_owned_and_excluded_from_rows() {
    let mut r = rig();
    r.clock.remaining.lock().unwrap().insert("m".into(), 1000);
    let clock = timer("clock", serde_json::json!({"kind": "window_clock", "instrument_id": "m", "remaining_ms": 10}), "on_crossing", "never");
    let (id, _rx) = connect(&mut r.platform, "Strategy_timer:3", &["a"], vec![clock], &["clock"]);
    let stats = r.platform.stats();
    assert_eq!(stats.timers, 1);
    assert_eq!(stats.unique_rows, 0);
    assert_eq!(stats.logical_registrations, 0);
    let reqs = r.sink.0.lock().unwrap();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].delay_ms, 990);
    drop(reqs);
    r.platform.disconnect(id);
    assert_eq!(r.platform.stats().timers, 0);
}

#[test]
fn timer_is_leg_scoped_and_fires_with_no_event() {
    let mut r = rig();
    r.clock.remaining.lock().unwrap().insert("m".into(), 5);
    let clock = timer("clock", serde_json::json!({"kind": "window_clock", "instrument_id": "m", "remaining_ms": 10}), "on_crossing", "never");
    let (id, mut rx) = connect(&mut r.platform, "s", &["a"], vec![clock], &[]);
    assert!(r.sink.0.lock().unwrap().is_empty(), "not armed until in the mask");
    r.platform.set_mask(id, names(&["clock"]));
    let req = r.sink.0.lock().unwrap().last().cloned().unwrap();
    assert_eq!(req.delay_ms, 0, "already inside the threshold fires immediately");
    *r.clock.now.lock().unwrap() = 77;
    r.platform.timer_fired(id, "clock", req.generation);
    let acts = activations(&mut rx);
    assert_eq!(acts.len(), 1);
    assert_eq!(acts[0].event, None);
    assert_eq!(acts[0].fired, names(&["clock"]));
    assert_eq!(acts[0].ts_ms, 77);
    // Spent: a second tick of the same arming is ignored.
    r.platform.timer_fired(id, "clock", req.generation);
    assert!(activations(&mut rx).is_empty());
}

#[test]
fn timer_restarts_when_next_leg_arms_the_same_name() {
    let mut r = rig();
    r.clock.remaining.lock().unwrap().insert("m".into(), 1000);
    let clock = timer("clock", serde_json::json!({"kind": "window_clock", "instrument_id": "m", "remaining_ms": 10}), "on_crossing", "immediate");
    let (id, mut rx) = connect(&mut r.platform, "s", &["a"], vec![clock], &["clock"]);
    r.platform.set_mask(id, names(&["clock"]));
    let reqs = r.sink.0.lock().unwrap().clone();
    assert_eq!(reqs.len(), 2);
    assert!(reqs[0].cancelled.load(std::sync::atomic::Ordering::Relaxed), "old arming cancelled");
    assert!(!reqs[1].cancelled.load(std::sync::atomic::Ordering::Relaxed));
    // A late fire from the first arming is ignored; the second delivers.
    r.platform.timer_fired(id, "clock", reqs[0].generation);
    assert!(activations(&mut rx).is_empty());
    r.platform.timer_fired(id, "clock", reqs[1].generation);
    assert_eq!(activations(&mut rx).len(), 1);
}

#[test]
fn timer_waits_for_first_window_and_rearms_each_window() {
    let mut r = rig();
    let clock = timer("clock", serde_json::json!({"kind": "window_clock", "instrument_id": "series.current", "remaining_ms": 10}), "on_crossing", "immediate");
    let (id, mut rx) = connect(&mut r.platform, "s", &["a"], vec![clock], &["clock"]);
    assert!(r.sink.0.lock().unwrap().is_empty(), "no window yet");
    r.clock.remaining.lock().unwrap().insert("series.current".into(), 60_000);
    r.platform.window_opened(&["series.w0", "series.current"]);
    let req = r.sink.0.lock().unwrap().last().cloned().unwrap();
    assert_eq!(req.delay_ms, 59_990);
    r.platform.timer_fired(id, "clock", req.generation);
    assert_eq!(activations(&mut rx).len(), 1);
    r.platform.window_opened(&["series.w1", "series.current"]);
    let req2 = r.sink.0.lock().unwrap().last().cloned().unwrap();
    assert_ne!(req.generation, req2.generation);
    r.platform.timer_fired(id, "clock", req2.generation);
    assert_eq!(activations(&mut rx).len(), 1);
}

#[test]
fn after_and_interval_timers_resolve_without_windows() {
    let mut r = rig();
    let after = timer("give_up", serde_json::json!({"kind": "after", "elapsed_ms": 20_000}), "on_crossing", "never");
    let every = timer("every_2s", serde_json::json!({"kind": "interval", "every_ms": 2_000}), "on_crossing", "immediate");
    let (id, mut rx) = connect(&mut r.platform, "s", &["a"], vec![after, every], &["give_up", "every_2s"]);
    let reqs = r.sink.0.lock().unwrap().clone();
    assert_eq!(reqs.len(), 2);
    let every_req = reqs.iter().find(|q| q.name == "every_2s").unwrap();
    let after_req = reqs.iter().find(|q| q.name == "give_up").unwrap();
    assert_eq!((every_req.delay_ms, every_req.repeat_ms), (2_000, Some(2_000)));
    assert_eq!((after_req.delay_ms, after_req.repeat_ms), (20_000, None));
    for t in [2_000, 4_000, 6_000] {
        *r.clock.now.lock().unwrap() = t;
        r.platform.timer_fired(id, "every_2s", every_req.generation);
    }
    assert_eq!(activations(&mut rx).len(), 3);
}

#[test]
fn cooldown_paces_a_condition_trigger() {
    let mut r = rig();
    let cheap = trigger(serde_json::json!({"name": "cheap", "source": {"kind": "all", "leaves": [field_leaf("a", "x", ">", "0")]},
        "fire": "if_true", "rearm": "immediate", "cooldown_ms": 2000}));
    let (_id, mut rx) = connect(&mut r.platform, "s", &["a"], vec![cheap], &["cheap"]);
    r.platform.ingest(&a("1", "1", 0)).unwrap();
    r.platform.ingest(&a("1", "1", 500)).unwrap();
    r.platform.ingest(&a("1", "1", 1999)).unwrap();
    r.platform.ingest(&a("1", "1", 2000)).unwrap();
    assert_eq!(activations(&mut rx).iter().map(|a| a.ts_ms).collect::<Vec<_>>(), vec![0, 2000]);
}

#[test]
fn reset_session_rewinds_and_bumps_session() {
    let mut r = rig();
    let once = cond("once", "all", vec![field_leaf("a", "x", ">", "0")], "if_true", "never");
    let (id, mut rx) = connect(&mut r.platform, "s", &["a"], vec![once], &["once"]);
    r.platform.ingest(&a("1", "1", 1)).unwrap();
    assert_eq!(activations(&mut rx).len(), 1);
    r.platform.ingest(&a("1", "1", 2)).unwrap();
    assert!(activations(&mut rx).is_empty(), "spent");
    r.platform.reset_session(id);
    assert!(r.platform.connection(id).unwrap().mask.is_empty());
    r.platform.set_mask(id, names(&["once"]));
    r.platform.ingest(&a("1", "1", 3)).unwrap();
    let acts = activations(&mut rx);
    assert_eq!(acts.len(), 1);
    assert_eq!(acts[0].session, 1);
}

#[test]
fn params_reevaluate_only_their_connection() {
    let mut r = rig();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let follow = trigger(serde_json::json!({"name": "follow", "source": {"kind": "all", "leaves": [
        {"kind": "field", "signal_id": "b", "field": "kind", "comparator": "==", "value": {"kind": "param", "name": "target"}}]},
        "fire": "if_true", "rearm": "immediate"}));
    let mut params = BTreeMap::new();
    params.insert("target".to_string(), Value::Text("no".into()));
    let id = r
        .platform
        .connect(ConnectSpec { owner: None, subscriptions: vec!["b".into()], triggers: vec![follow], initial_mask: vec!["follow".into()], params, tx })
        .unwrap();
    r.platform.ingest(&b("yes", 1)).unwrap();
    assert!(activations(&mut rx).is_empty());
    let mut params = BTreeMap::new();
    params.insert("target".to_string(), Value::Text("yes".into()));
    r.platform.set_params(id, params);
    let acts = activations(&mut rx);
    assert_eq!(acts.len(), 1);
    assert_eq!(acts[0].event, None, "a param change is not an event");
    assert_eq!(acts[0].fired, names(&["follow"]));
    assert_eq!(r.platform.stats().rows[0].logical_registrations, 1);
}

#[test]
fn connect_rejects_bad_specs_without_leaking_rows() {
    let mut r = rig();
    let (tx, _rx) = mpsc::unbounded_channel();
    let err = r
        .platform
        .connect(ConnectSpec {
            owner: None,
            subscriptions: vec!["a".into()],
            triggers: vec![cond("ok", "all", vec![field_leaf("a", "x", ">", "0")], "if_true", "immediate"), cond("bad", "all", vec![field_leaf("b", "kind", "==", "yes")], "if_true", "immediate")],
            initial_mask: vec![],
            params: BTreeMap::new(),
            tx,
        })
        .unwrap_err();
    assert_eq!(err, ConnectError::UnsubscribedSignal { trigger: "bad".into(), signal_id: "b".into() });
    assert_eq!(r.platform.stats().unique_rows, 0);
    let (tx, _rx) = mpsc::unbounded_channel();
    let err = r
        .platform
        .connect(ConnectSpec { owner: None, subscriptions: vec!["zzz".into()], triggers: vec![], initial_mask: vec![], params: BTreeMap::new(), tx })
        .unwrap_err();
    assert_eq!(err, ConnectError::UnknownSignal("zzz".into()));
}

/// A publish that fires nothing is not delivered, subscribed or not: the
/// only frame a connection ever receives is an activation.
#[test]
fn a_publish_without_a_fire_delivers_nothing() {
    let mut r = rig();
    let (_a, mut rx_a) = connect(&mut r.platform, "s", &["a"], vec![], &[]);
    let (_b, mut rx_b) = connect(&mut r.platform, "s", &["b"], vec![], &[]);
    r.platform.ingest(&a("1", "2", 1)).unwrap();
    assert!(rx_a.try_recv().is_err(), "subscribed, but no armed trigger fired");
    assert!(rx_b.try_recv().is_err());
}

#[tokio::test(start_paused = true)]
async fn platform_task_round_trips_and_fires_tokio_timers() {
    use super::task::{PlatformCommand, spawn_platform};
    let clock = Arc::new(TestClock::default());
    clock.remaining.lock().unwrap().insert("m".into(), 1_000);
    let handle = spawn_platform(ab_store(), clock.clone());
    let (tx, mut rx) = mpsc::unbounded_channel();
    let clock_t = timer("clock", serde_json::json!({"kind": "window_clock", "instrument_id": "m", "remaining_ms": 100}), "on_crossing", "never");
    let edge = cond("edge", "all", vec![field_leaf("a", "x", ">", "0")], "if_true", "immediate");
    let id = handle
        .connect(ConnectSpec { owner: None, subscriptions: vec!["a".into()], triggers: vec![clock_t, edge], initial_mask: vec!["clock".into(), "edge".into()], params: BTreeMap::new(), tx })
        .await
        .unwrap();
    handle.ingest(a("1", "1", 5));
    let Frame::Activation(act) = rx.recv().await.unwrap();
    assert_eq!(act.fired, names(&["edge"]));
    // The window_clock timer sleeps 900 ms on the paused Tokio clock.
    tokio::time::advance(std::time::Duration::from_millis(950)).await;
    let Frame::Activation(act) = rx.recv().await.unwrap();
    assert_eq!(act.fired, names(&["clock"]));
    assert_eq!(handle.stats().await.unwrap().timers, 1);
    handle.send(PlatformCommand::Disconnect { conn: id });
    assert_eq!(handle.stats().await.unwrap().timers, 0);
}
