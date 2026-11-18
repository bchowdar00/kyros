//! The interpreter against the compiler's real fixture programs.

use std::collections::BTreeMap;

use strategy_compiler::ir::{OrderType, Side, Tif};

use super::interpreter::{Action, FillLedger, PlatformView, interpret};
use super::state::StateStore;
use super::test_support::*;
use crate::automation::value::Value;

struct Harness {
    program: strategy_compiler::program::Program,
    state: StateStore,
    params: BTreeMap<String, Value>,
    platform: PlatformView,
    fills: FillLedger,
}

impl Harness {
    fn new(fixture: &str) -> Harness {
        let program = compile_fixture(fixture);
        let state = StateStore::new(&program.state).unwrap();
        Harness { program, state, params: BTreeMap::new(), platform: PlatformView::default(), fills: FillLedger::default() }
    }

    fn run(&mut self, leg: &str, act: &crate::automation::sme::Activation) -> Vec<Action> {
        interpret(&self.program, leg, act, &mut self.state, &self.params, &self.platform, &self.fills).unwrap()
    }
}

#[test]
fn fair_discount_hunting_places_bid_and_moves_to_quoting() {
    let mut h = Harness::new("fair-discount");
    let act = activation(&["cheap"], &[(FAIR, &[("up", "0.6"), ("down", "0.4")]), (BOOK, &[("ask", "0.55"), ("bid", "0.5"), ("last", "0.52"), ("up", "0.525"), ("down", "0.475")])], 1);
    let actions = h.run("hunting", &act);
    assert_eq!(actions.len(), 2);
    let Action::Place { intent_id, order } = &actions[0] else { panic!("{actions:?}") };
    assert_eq!(intent_id, "hunting.bid_up.1");
    assert_eq!(order.size, d("100"));
    assert_eq!(order.price, Some(d("0.59")));
    assert_eq!(order.def.side, Side::Buy);
    assert_eq!(order.def.order_type, OrderType::Limit);
    assert_eq!(order.def.tif, Tif::Gtc);
    assert_eq!(order.def.tag, "bid");
    assert_eq!(order.def.outcome.as_deref(), Some("up"));
    assert_eq!(actions[1], Action::SwitchLeg("quoting".into()));
    assert_eq!(h.state.scalar("quotes").unwrap(), Value::Decimal(d("1")));
}

#[test]
fn fair_discount_reads_gate_skips_unpublished_blocks() {
    let mut h = Harness::new("fair-discount");
    // `cheap` fired but fair has never published: bid_up reads fair.up.
    let act = activation(&["cheap"], &[(BOOK, &[("ask", "0.55")])], 1);
    assert!(h.run("hunting", &act).is_empty());
    assert_eq!(h.state.scalar("quotes").unwrap(), Value::Decimal(d("0")));
}

#[test]
fn fair_discount_timer_stops_hunting() {
    let mut h = Harness::new("fair-discount");
    let act = activation(&["last_10s"], &[], 1);
    assert_eq!(h.run("hunting", &act), vec![Action::SwitchLeg("done".into())]);
}

#[test]
fn fair_discount_quoting_requotes_and_pulls() {
    let mut h = Harness::new("fair-discount");
    let moved = activation(&["fair_moved"], &[(FAIR, &[("up", "0.7"), ("down", "0.3")])], 2);
    let actions = h.run("quoting", &moved);
    assert_eq!(actions.len(), 1);
    let Action::CancelReplace { intent_id, order } = &actions[0] else { panic!("{actions:?}") };
    assert_eq!(intent_id, "quoting.requote.0");
    assert_eq!(order.price, Some(d("0.69")));
    assert_eq!(order.def.tag, "bid");

    let pull = activation(&["last_10s"], &[(FAIR, &[("up", "0.7"), ("down", "0.3")])], 3);
    assert_eq!(h.run("quoting", &pull), vec![Action::Cancel { tag: "bid".into() }, Action::SwitchLeg("done".into())]);
}

#[test]
fn fair_discount_price_is_clamped() {
    let mut h = Harness::new("fair-discount");
    let act = activation(&["cheap"], &[(FAIR, &[("up", "0.005"), ("down", "0.995")])], 1);
    let actions = h.run("hunting", &act);
    let Action::Place { order, .. } = &actions[0] else { panic!() };
    assert_eq!(order.price, Some(d("0.01")));
}

#[test]
fn rolling_spread_pushes_then_buys_once_window_qualifies() {
    let mut h = Harness::new("rolling-spread");
    let tick = |ts: u64| activation(&["book_tick"], &[(FAIR, &[("up", "0.61"), ("down", "0.39")]), (BOOK, &[("ask", "0.5"), ("bid", "0.48"), ("last", "0.49"), ("up", "0.49"), ("down", "0.51")])], ts);
    for i in 1..10 {
        assert!(h.run("sampling", &tick(i)).is_empty(), "tick {i} only records");
        assert_eq!(h.state.window("spreads").unwrap().len(), i as usize);
    }
    let actions = h.run("sampling", &tick(10));
    assert_eq!(h.state.window("spreads").unwrap().len(), 10);
    assert_eq!(actions.len(), 2);
    let Action::Place { intent_id, order } = &actions[0] else { panic!("{actions:?}") };
    assert_eq!(intent_id, "sampling.buy_up.0");
    assert_eq!(order.def.order_type, OrderType::Market);
    assert_eq!(order.size, d("100"));
    assert_eq!(actions[1], Action::SwitchLeg("done".into()));
}

#[test]
fn rolling_spread_param_override_changes_count_where() {
    let mut h = Harness::new("rolling-spread");
    // With mid = 0.11 every reading is within 0.05 of it, so the
    // `count_where(abs(entry - mid) > 0.05) >= 3` clause fails.
    h.params.insert("mid".into(), Value::Decimal(d("0.11")));
    let tick = |ts: u64| activation(&["book_tick"], &[(FAIR, &[("up", "0.61"), ("down", "0.39")]), (BOOK, &[("ask", "0.5"), ("bid", "0.48"), ("last", "0.49"), ("up", "0.49"), ("down", "0.51")])], ts);
    for i in 1..=12 {
        assert!(h.run("sampling", &tick(i)).is_empty());
    }
}

#[test]
fn leg_exit_reads_fill_ledger_and_platform_view() {
    let mut h = Harness::new("leg-exit");
    h.platform.position_size = d("40");
    h.platform.position_avg_cost = d("0.5");
    let book = |bid: &str, ts: u64| activation(&["book_tick"], &[(FAIR, &[("up", "0.6"), ("down", "0.4")]), (BOOK, &[("ask", "0.62"), ("bid", bid), ("last", "0.6"), ("up", "0.61"), ("down", "0.39")])], ts);
    // No fills recorded yet: take_profit needs legs.hunting.fills >= 1 and
    // cut_loss needs bid <= avg_cost - 0.1.
    assert!(h.run("holding", &book("0.58", 1)).is_empty());
    h.fills.record("hunting", "bid", d("40"), d("0.5"));
    assert!(h.run("holding", &book("0.54", 2)).is_empty(), "5 cents above 0.5 not reached");
    let actions = h.run("holding", &book("0.55", 3));
    let Action::Place { intent_id, order } = &actions[0] else { panic!("{actions:?}") };
    assert_eq!(intent_id, "holding.take_profit.0");
    assert_eq!(order.def.side, Side::Sell);
    assert_eq!(order.size, d("40"));
    assert_eq!(actions[1], Action::SwitchLeg("done".into()));
    // Cut loss path.
    let actions = h.run("holding", &book("0.39", 4));
    let Action::Place { intent_id, .. } = &actions[0] else { panic!("{actions:?}") };
    assert_eq!(intent_id, "holding.cut_loss.0", "intent ids are numbered per block");
}

#[test]
fn leg_exit_hunting_guard_uses_open_orders_and_window_remaining() {
    let mut h = Harness::new("leg-exit");
    let act = activation(&["cheap"], &[(FAIR, &[("up", "0.6"), ("down", "0.4")]), (BOOK, &[("ask", "0.55"), ("bid", "0.5"), ("last", "0.52"), ("up", "0.525"), ("down", "0.475")])], 1);
    h.platform.window_remaining_ms = 30_000;
    h.platform.open_orders = 1;
    assert!(h.run("hunting", &act).is_empty(), "an open order blocks a new bid");
    h.platform.open_orders = 0;
    h.platform.window_remaining_ms = 10_000;
    assert!(h.run("hunting", &act).is_empty(), "too late in the window");
    h.platform.window_remaining_ms = 30_000;
    let actions = h.run("hunting", &act);
    assert!(matches!(&actions[0], Action::Place { intent_id, .. } if intent_id == "hunting.bid_up.0"));
    assert_eq!(actions[1], Action::SwitchLeg("resting".into()));
}

#[test]
fn every_tiny_fixture_seeds_state_and_interprets_its_initial_leg() {
    for name in ["fair-discount", "entry-tracker", "leg-exit", "follow-whale", "periodic-requoter", "rolling-spread", "post-only-maker", "combined-everything", "momentum-flip", "scale-in", "two-sided-quoter", "re-entry", "every-time-cooldown"] {
        let mut h = Harness::new(name);
        // An activation with nothing fired and nothing published must be a
        // no-op for every fixture: blocks are gated on reads and guards.
        let actions = h.run(&h.program.initial_leg.clone(), &activation(&[], &[], 0));
        assert!(actions.is_empty(), "{name}: {actions:?}");
    }
}
