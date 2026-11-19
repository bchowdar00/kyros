//! The Gemini venue against a real `Exchange`, with Gemini played by the
//! test: first message by message, then once over a real socket.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use gemini_private::rest::{RestOrder, RestOrderStatus};
use serde_json::{Value, json};
use strategy_compiler::ir::{Side as IrSide, Tif};

use super::*;
use crate::engine::events::Recorder;
use crate::engine::{ExecutionEvent, Outcome, RoundClock, RoundClose};
use crate::series::SERIES;
use crate::types::Tenths;

const S: SeriesId = SeriesId(0);
const STRAT: Address = Address([0x51; 20]);
const OTHER: Address = Address([0x52; 20]);
const OUTSIDE: Address = Address([0x0e; 20]);
const SYMBOL: &str = "GEMI-BTC05M2611190300-UP";
const OPENS: u64 = 1_900_000_000;
const GID: u64 = 73_797_746_498_585_286;

struct Rig {
    exchange: Arc<Mutex<Exchange>>,
    venue: Arc<GeminiVenue>,
    driver: Driver,
    rec: Recorder,
    now: Arc<AtomicU64>,
    has_symbol: Arc<std::sync::atomic::AtomicBool>,
}

/// A funded strategy account on an open round, an outside counterparty,
/// and a venue that is connected with the terms accepted.
fn rig() -> Rig {
    let exchange = Arc::new(Mutex::new(Exchange::new(Tenths(1_000_000), SERIES)));
    let rec = Recorder::new();
    let now = Arc::new(AtomicU64::new(OPENS + 10));
    {
        let mut x = exchange.lock().unwrap();
        x.get_or_create_account(STRAT);
        x.get_or_create_account(OTHER);
        x.seed_bot_account_in(OUTSIDE, Tenths(1_000_000_000), "gemini-fills".into(), None, None, S);
        x.set_outside(S, OUTSIDE);
        x.open_first_market(S, 100_000.0, OPENS, 300);
        x.set_execution_sink(rec.tx.clone());
        let reads = now.clone();
        x.engine_mut(S).set_clock(RoundClock::new(move || reads.load(Ordering::SeqCst)));
        x.engine_mut(S).set_lock_at_close(true);
    }
    let has_symbol = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let flag = has_symbol.clone();
    let symbols: SymbolFn = Arc::new(move |series| (series == S && flag.load(Ordering::SeqCst)).then(|| SYMBOL.to_string()));
    let (venue, driver) = GeminiVenue::new(exchange.clone(), SeriesMap::new([("a", S), ("b", SeriesId(1))]), symbols, "kt");
    venue.set_terms(true);
    venue.on_connected();
    Rig { exchange, venue, driver, rec, now, has_symbol }
}

impl Rig {
    /// Frames the venue queued for the socket since the last call.
    fn frames(&mut self) -> Vec<Value> {
        let mut out = Vec::new();
        while let Ok(stamped) = self.driver.frames.try_recv() {
            // As the socket task does: a frame queued for a socket that has
            // since dropped is never sent.
            if let Some(frame) = self.venue.still_current(stamped) {
                out.push(serde_json::from_str(&frame).unwrap());
            }
        }
        out
    }

    fn events(&mut self) -> Vec<(String, ExecKind, u32, u32, Option<u16>)> {
        self.raw_events()
            .iter()
            .map(|e| (e.client_order_id.to_string(), e.kind, e.filled_qty.0, e.remaining_qty.0, e.price.map(|p| p.tenths())))
            .collect()
    }

    fn raw_events(&mut self) -> Vec<ExecutionEvent> {
        self.rec.take()
    }

    fn position(&self, account: Address) -> i64 {
        self.exchange.lock().unwrap().engine(S).position(&account).position.0
    }

    /// Gemini says something; returns what the venue answers.
    fn gemini(&self, message: Value) -> Vec<Value> {
        self.venue.on_inbound(&message.to_string()).iter().map(|f| serde_json::from_str(f).unwrap()).collect()
    }
}

fn order(cid: &str, outcome: &str, side: IrSide, size: &str, price: &str) -> VenueOrder {
    VenueOrder {
        client_order_id: Arc::from(cid),
        instrument_id: "a.current".into(),
        outcome: Some(outcome.into()),
        side,
        order_type: OrderType::Limit,
        tif: Tif::Gtc,
        size: size.parse().unwrap(),
        price: Some(price.parse().unwrap()),
        post_only: false,
        expire_after_ms: None,
        reduce_only: false,
    }
}

fn buy_up(cid: &str, size: &str, price: &str) -> VenueOrder {
    order(cid, "up", IrSide::Buy, size, price)
}

/// An `orderUpdate` for the order Gemini knows as `kt-<n>`.
fn update(n: u64, status: &str, extra: Value) -> Value {
    let mut v = json!({"e": "orderUpdate", "s": SYMBOL, "i": GID + n, "c": format!("kt-{n}"), "X": status});
    for (k, x) in extra.as_object().unwrap() {
        v[k] = x.clone();
    }
    v
}

fn response(id: &Value, status: u16) -> Value {
    if status == 200 { json!({"id": id, "status": 200, "result": {}}) } else { json!({"id": id, "status": status, "error": {"code": -2010, "msg": "Order rejected"}}) }
}

fn rest_order(n: u64, status: RestOrderStatus, quantity: u32, filled: u32, avg: Option<&str>) -> RestOrder {
    RestOrder {
        order_id: GID + n,
        client_order_id: Some(format!("kt-{n}")),
        status,
        symbol: SYMBOL.into(),
        quantity: quantity.into(),
        filled_quantity: filled.into(),
        remaining_quantity: (quantity - filled).into(),
        price: Some("0.48".parse().unwrap()),
        avg_execution_price: avg.map(|a| a.parse().unwrap()),
    }
}

// ───────────────────────── what goes to Gemini ─────────────────────────

#[test]
fn every_order_goes_out_as_a_buy_of_yes_or_of_no() {
    let mut r = rig();
    let a = r.venue.submit(S, STRAT, &buy_up("c1", "5", "0.48")).unwrap();
    let b = r.venue.submit(S, STRAT, &order("c2", "down", IrSide::Buy, "3", "0.40")).unwrap();
    assert_eq!(a.market_id, Some(1));
    assert_ne!(a.venue_order_id, b.venue_order_id);
    let frames = r.frames();
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0]["method"], "order.place");
    assert_eq!(
        frames[0]["params"],
        json!({"symbol": SYMBOL, "side": "BUY", "type": "LIMIT", "timeInForce": "GTC", "price": "0.48", "quantity": "5", "clientOrderId": "kt-1", "eventOutcome": "YES"})
    );
    assert_eq!(frames[1]["params"]["eventOutcome"], "NO");
    assert_eq!(frames[1]["params"]["price"], "0.4");
    assert_eq!(frames[1]["params"]["clientOrderId"], "kt-2");
    assert!(r.events().is_empty(), "a placement Gemini has not answered reports nothing");
    assert_eq!(r.venue.status().live_orders, 2);
}

#[test]
fn selling_an_outcome_is_sent_as_buying_the_other_at_the_complement() {
    let mut r = rig();
    r.venue.submit(S, STRAT, &order("c1", "up", IrSide::Sell, "2", "0.60")).unwrap();
    r.venue.submit(S, OTHER, &order("c2", "down", IrSide::Sell, "2", "0.30")).unwrap();
    let frames = r.frames();
    assert_eq!((&frames[0]["params"]["side"], &frames[0]["params"]["eventOutcome"], &frames[0]["params"]["price"]), (&json!("BUY"), &json!("NO"), &json!("0.4")));
    assert_eq!((&frames[1]["params"]["side"], &frames[1]["params"]["eventOutcome"], &frames[1]["params"]["price"]), (&json!("BUY"), &json!("YES"), &json!("0.7")));
}

#[test]
fn time_in_force_follows_the_order() {
    let mut r = rig();
    let mut ioc = buy_up("c1", "1", "0.48");
    ioc.tif = Tif::Ioc;
    let mut post = buy_up("c2", "1", "0.48");
    post.post_only = true;
    r.venue.submit(S, STRAT, &ioc).unwrap();
    r.venue.submit(S, OTHER, &post).unwrap();
    let frames = r.frames();
    assert_eq!(frames[0]["params"]["timeInForce"], "IOC");
    assert_eq!(frames[1]["params"]["timeInForce"], "MOC");
}

// ───────────────────────── accept, then fill ─────────────────────────

#[test]
fn an_accepted_order_that_fills_is_booked_fill_by_fill() {
    let mut r = rig();
    let acc = r.venue.submit(S, STRAT, &buy_up("c1", "5", "0.48")).unwrap();
    let place = r.frames().remove(0);
    assert!(r.gemini(response(&place["id"], 200)).is_empty());
    assert!(r.gemini(update(1, "NEW", json!({"S": "BUY", "O": "YES", "p": "0.48000", "q": "5", "z": "5"}))).is_empty());
    assert!(r.events().is_empty(), "accepted is not filled");

    r.gemini(update(1, "PARTIALLY_FILLED", json!({"Z": "2", "L": "0.47", "t": 901, "z": "3"})));
    r.gemini(update(1, "FILLED", json!({"Z": "3", "L": "0.48", "t": 902, "z": "0", "n": "0.01"})));

    let evs = r.raw_events();
    assert!(evs.iter().all(|e| e.venue_order_id == Some(OrderId(acc.venue_order_id)) && e.account == STRAT));
    assert_eq!(
        evs.iter().map(|e| (e.client_order_id.to_string(), e.kind, e.filled_qty.0, e.remaining_qty.0, e.price.map(|p| p.tenths()))).collect::<Vec<_>>(),
        vec![("c1".to_string(), ExecKind::Partial, 2, 3, Some(470)), ("c1".to_string(), ExecKind::Fill, 3, 0, Some(480))]
    );
    assert_eq!(r.position(STRAT), 5);
    assert_eq!(r.position(OUTSIDE), -5);
    let status = r.venue.status();
    assert_eq!((status.live_orders, status.fills, status.divergences), (0, 2, 0));
    let view = r.venue.account_view(S, STRAT);
    assert_eq!(view.position_size, Decimal::from(5));
    assert_eq!(view.cash, Decimal::from(1000) - "0.94".parse::<Decimal>().unwrap() - "1.44".parse::<Decimal>().unwrap());
}

#[test]
fn the_same_trade_reported_twice_is_booked_once() {
    let mut r = rig();
    r.venue.submit(S, STRAT, &buy_up("c1", "5", "0.48")).unwrap();
    for _ in 0..2 {
        r.gemini(update(1, "PARTIALLY_FILLED", json!({"Z": "2", "L": "0.47", "t": 901})));
    }
    assert_eq!(r.events().len(), 1);
    assert_eq!(r.position(STRAT), 2);
}

#[test]
fn a_fill_of_a_no_order_lands_on_the_quoted_book_at_the_complement() {
    let mut r = rig();
    r.venue.submit(S, STRAT, &order("c1", "down", IrSide::Buy, "4", "0.40")).unwrap();
    // Gemini reports the price of the No contract that was bought.
    r.gemini(update(1, "FILLED", json!({"Z": "4", "L": "0.39", "t": 1, "O": "NO"})));
    assert_eq!(r.events(), vec![("c1".to_string(), ExecKind::Fill, 4, 0, Some(610))]);
    assert_eq!(r.position(STRAT), -4, "long No is short the quoted outcome");
}

#[test]
fn an_update_for_an_order_that_is_not_ours_is_ignored() {
    let mut r = rig();
    r.gemini(json!({"e": "orderUpdate", "s": SYMBOL, "i": 5, "c": "placed-by-hand", "X": "FILLED", "Z": "9", "L": "0.5"}));
    assert!(r.events().is_empty());
    assert_eq!(r.position(STRAT), 0);
    assert_eq!(r.venue.status().divergences, 0);
}

// ───────────────────────── accept, then refuse ─────────────────────────

#[test]
fn a_placement_gemini_answers_with_an_error_is_reported_refused() {
    let mut r = rig();
    r.venue.submit(S, STRAT, &buy_up("c1", "5", "0.48")).unwrap();
    let place = r.frames().remove(0);
    assert!(r.gemini(response(&place["id"], 400)).is_empty());
    assert_eq!(r.events(), vec![("c1".to_string(), ExecKind::Reject(ExecReject::VenueRefused), 0, 0, None)]);
    assert_eq!(r.venue.status().live_orders, 0);
    assert_eq!(r.position(STRAT), 0);
}

#[test]
fn an_order_gemini_accepts_and_then_rejects_is_reported_with_its_reason() {
    let mut r = rig();
    r.venue.submit(S, STRAT, &buy_up("c1", "5", "0.48")).unwrap();
    r.venue.submit(S, OTHER, &buy_up("c2", "5", "0.48")).unwrap();
    r.gemini(update(1, "NEW", json!({})));
    r.gemini(update(1, "REJECTED", json!({"r": "InsufficientFunds"})));
    r.gemini(update(2, "REJECTED", json!({"r": "TERMS_NOT_ACCEPTED"})));
    assert_eq!(
        r.events(),
        vec![
            ("c1".to_string(), ExecKind::Reject(ExecReject::InsufficientBalance), 0, 0, None),
            ("c2".to_string(), ExecKind::Reject(ExecReject::VenueRefused), 0, 0, None),
        ]
    );
    assert_eq!(r.venue.status().live_orders, 0);
}

#[test]
fn a_post_only_order_that_would_have_taken_is_a_refusal_not_a_cancel() {
    let mut r = rig();
    let mut post = buy_up("c1", "5", "0.48");
    post.post_only = true;
    r.venue.submit(S, STRAT, &post).unwrap();
    r.gemini(update(1, "NEW", json!({})));
    r.gemini(update(1, "CANCELED", json!({"r": "MakerOrCancelWouldTake"})));
    assert_eq!(r.events(), vec![("c1".to_string(), ExecKind::Reject(ExecReject::PostOnlyWouldCross), 0, 0, None)]);
}

// ───────────────────────── cancels ─────────────────────────

#[test]
fn a_cancel_is_sent_with_geminis_id_and_acked_when_gemini_says_so() {
    let mut r = rig();
    let acc = r.venue.submit(S, STRAT, &buy_up("c1", "5", "0.48")).unwrap();
    r.gemini(update(1, "NEW", json!({})));
    r.frames();
    r.venue.cancel(S, STRAT, acc.venue_order_id).unwrap();
    let frames = r.frames();
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0]["method"], "order.cancel");
    assert_eq!(frames[0]["params"]["orderId"], json!(GID + 1));
    assert!(r.events().is_empty(), "asked is not cancelled");
    r.venue.cancel(S, STRAT, acc.venue_order_id).unwrap();
    assert!(r.frames().is_empty(), "a second ask sends nothing new");

    r.gemini(update(1, "CANCELED", json!({})));
    assert_eq!(r.events(), vec![("c1".to_string(), ExecKind::CancelAck, 0, 0, None)]);
    assert!(r.venue.cancel(S, STRAT, acc.venue_order_id).is_err(), "it is gone now");
}

#[test]
fn a_cancel_asked_before_gemini_named_the_order_goes_out_once_it_has() {
    let mut r = rig();
    let acc = r.venue.submit(S, STRAT, &buy_up("c1", "5", "0.48")).unwrap();
    r.frames();
    r.venue.cancel(S, STRAT, acc.venue_order_id).unwrap();
    assert!(r.frames().is_empty(), "there is no id to cancel by yet");
    let answer = r.gemini(update(1, "NEW", json!({})));
    assert_eq!(answer.len(), 1);
    assert_eq!(answer[0]["method"], "order.cancel");
    assert_eq!(answer[0]["params"]["orderId"], json!(GID + 1));
}

#[test]
fn a_fill_that_arrives_after_a_cancel_was_asked_for_is_a_fill() {
    let mut r = rig();
    let acc = r.venue.submit(S, STRAT, &buy_up("c1", "5", "0.48")).unwrap();
    r.gemini(update(1, "NEW", json!({})));
    r.frames();
    r.venue.cancel(S, STRAT, acc.venue_order_id).unwrap();
    let cancel = r.frames().remove(0);

    // Gemini had already matched it.
    r.gemini(update(1, "FILLED", json!({"Z": "5", "L": "0.48", "t": 7})));
    assert!(r.gemini(response(&cancel["id"], 422)).is_empty());
    r.gemini(update(1, "CANCELED", json!({"Z": "5"})));

    assert_eq!(r.events(), vec![("c1".to_string(), ExecKind::Fill, 5, 0, Some(480))], "no cancel ack follows a fill");
    assert_eq!(r.position(STRAT), 5);
    assert_eq!(r.venue.status().divergences, 0);
}

#[test]
fn a_cancel_that_carries_fills_nobody_reported_books_them_first() {
    let mut r = rig();
    let acc = r.venue.submit(S, STRAT, &buy_up("c1", "5", "0.48")).unwrap();
    r.gemini(update(1, "NEW", json!({})));
    r.venue.cancel(S, STRAT, acc.venue_order_id).unwrap();
    // On a cancel, Z is everything the order ever filled.
    r.gemini(update(1, "CANCELED", json!({"Z": "2", "L": "0.47"})));
    assert_eq!(
        r.events(),
        vec![("c1".to_string(), ExecKind::Partial, 2, 3, Some(470)), ("c1".to_string(), ExecKind::CancelAck, 0, 0, None)]
    );
    assert_eq!(r.position(STRAT), 2);
}

#[test]
fn an_immediate_order_ends_as_expired_whatever_it_filled() {
    let mut r = rig();
    let mut ioc = buy_up("c1", "5", "0.48");
    ioc.tif = Tif::Ioc;
    r.venue.submit(S, STRAT, &ioc).unwrap();
    r.gemini(update(1, "PARTIALLY_FILLED", json!({"Z": "2", "L": "0.48", "t": 1})));
    r.gemini(update(1, "CANCELED", json!({"Z": "2", "r": "ImmediateOrCancelWouldPost"})));
    assert_eq!(
        r.events(),
        vec![("c1".to_string(), ExecKind::Partial, 2, 3, Some(480)), ("c1".to_string(), ExecKind::Expiry, 0, 0, None)]
    );
}

#[test]
fn only_the_owner_can_cancel_and_only_a_live_order() {
    let mut r = rig();
    let acc = r.venue.submit(S, STRAT, &buy_up("c1", "5", "0.48")).unwrap();
    r.frames();
    assert!(r.venue.cancel(S, OTHER, acc.venue_order_id).is_err());
    assert!(r.venue.cancel(S, STRAT, 424_242).is_err());
    assert!(r.frames().is_empty());
}

// ───────────────────────── nothing is retried blind ─────────────────────────

#[test]
fn an_unanswered_placement_is_looked_up_twice_and_never_sent_again() {
    let mut r = rig();
    let acc = r.venue.submit(S, STRAT, &buy_up("c1", "5", "0.48")).unwrap();
    assert_eq!(r.frames().len(), 1);
    let id = OrderId(acc.venue_order_id);

    assert!(r.venue.due_lookups(Instant::now()).is_empty(), "not before the timeout");
    let later = Instant::now() + PLACE_TIMEOUT + Duration::from_millis(1);
    assert_eq!(r.venue.due_lookups(later), vec![LookupRequest { order: id, symbol: SYMBOL.into() }]);
    assert!(r.venue.due_lookups(later).is_empty(), "one lookup at a time");

    // Gemini has no order under that client id. It may only be slow: one
    // miss decides nothing.
    let others = vec![rest_order(99, RestOrderStatus::Open, 5, 0, None)];
    assert!(r.venue.on_lookup_at(id, Ok(others.clone()), later).is_empty());
    assert!(r.events().is_empty(), "one miss is not a refusal");
    assert_eq!(r.venue.status().live_orders, 1);
    assert!(r.venue.due_lookups(later + LOOKUP_RETRY / 2).is_empty(), "the second look waits");
    assert_eq!(r.venue.due_lookups(later + LOOKUP_RETRY).len(), 1);

    // Still nothing: it never took it.
    assert!(r.venue.on_lookup_at(id, Ok(others), later + LOOKUP_RETRY).is_empty());
    assert_eq!(r.events(), vec![("c1".to_string(), ExecKind::Reject(ExecReject::VenueUnreachable), 0, 0, None)]);
    assert!(r.frames().is_empty(), "the placement was not repeated");
    assert_eq!(r.venue.status().live_orders, 0);
}

/// Submit `c1` and have two lookups miss it, so it is reported refused.
fn refused_unanswered(r: &mut Rig) {
    let acc = r.venue.submit(S, STRAT, &buy_up("c1", "5", "0.48")).unwrap();
    r.frames();
    let id = OrderId(acc.venue_order_id);
    let later = Instant::now() + PLACE_TIMEOUT + Duration::from_millis(1);
    r.venue.due_lookups(later);
    r.venue.on_lookup_at(id, Ok(Vec::new()), later);
    r.venue.due_lookups(later + LOOKUP_RETRY);
    r.venue.on_lookup_at(id, Ok(Vec::new()), later + LOOKUP_RETRY);
    assert_eq!(r.events(), vec![("c1".to_string(), ExecKind::Reject(ExecReject::VenueUnreachable), 0, 0, None)]);
}

#[test]
fn an_order_reported_refused_that_gemini_took_after_all_is_pulled_and_said() {
    let mut r = rig();
    refused_unanswered(&mut r);

    // Gemini took it after all. It is cancelled there at once, and the
    // server says its books may be off.
    let answer = r.gemini(update(1, "NEW", json!({})));
    assert_eq!(answer.len(), 1);
    assert_eq!(answer[0]["method"], "order.cancel");
    assert_eq!(answer[0]["params"]["orderId"], json!(GID + 1));
    let status = r.venue.status();
    assert_eq!(status.divergences, 1);
    assert!(status.problem.unwrap().contains("reported refused"));
    // Said once, cancelled once.
    assert!(r.gemini(update(1, "OPEN", json!({}))).is_empty());
    assert_eq!(r.venue.status().divergences, 1);
    // Its cancel changes nothing more.
    assert!(r.gemini(update(1, "CANCELED", json!({}))).is_empty());
    assert_eq!(r.position(STRAT), 0);
    assert!(r.events().is_empty());
}

#[test]
fn a_fill_on_an_order_reported_refused_is_still_booked_once() {
    let mut r = rig();
    refused_unanswered(&mut r);

    // Two of five filled before the cancel could land: the ledger must
    // hold what the Gemini account holds.
    let fill = update(1, "PARTIALLY_FILLED", json!({"q": "5", "z": "3", "Z": "2", "L": "0.47", "t": 4}));
    let answer = r.gemini(fill.clone());
    assert_eq!(answer.len(), 1, "and it is pulled");
    assert_eq!(answer[0]["method"], "order.cancel");
    assert_eq!(r.position(STRAT), 2);
    assert!(r.gemini(fill).is_empty());
    assert_eq!(r.position(STRAT), 2, "the same report books nothing more");
    // The cancel says three were filled in all.
    r.gemini(update(1, "CANCELED", json!({"Z": "3", "L": "0.48"})));
    assert_eq!(r.position(STRAT), 3);
    assert_eq!(r.venue.status().divergences, 1);
}

#[test]
fn a_fill_on_a_refused_order_whose_round_is_over_is_not_booked_into_the_next_round() {
    let mut r = rig();
    refused_unanswered(&mut r);
    r.now.store(OPENS + 300, Ordering::SeqCst);
    r.exchange.lock().unwrap().resolve_and_open_next(S, 100_050.0, 100_050.0, OPENS + 300, 300);
    r.events();

    // The order was for the round that has settled. Its fill belongs to a
    // contract that has paid out, not to the round now open.
    r.gemini(update(1, "FILLED", json!({"q": "5", "z": "0", "Z": "5", "L": "0.48", "t": 7})));
    assert_eq!(r.position(STRAT), 0);
    let status = r.venue.status();
    assert_eq!(status.divergences, 1);
    assert!(status.problem.unwrap().contains("after its round settled"));
}

#[test]
fn a_refused_order_is_still_known_when_gemini_names_it_only_by_its_own_id() {
    let mut r = rig();
    refused_unanswered(&mut r);
    r.gemini(update(1, "NEW", json!({})));
    // No client order id on this one.
    r.gemini(json!({"e": "orderUpdate", "s": SYMBOL, "i": GID + 1, "X": "PARTIALLY_FILLED", "q": "5", "z": "3", "Z": "2", "L": "0.47", "t": 4}));
    assert_eq!(r.position(STRAT), 2);
}

#[test]
fn a_refused_order_never_books_more_than_it_was_for() {
    let mut r = rig();
    refused_unanswered(&mut r);
    // A fill report with no totals and no trade id, twice.
    let filled = update(1, "FILLED", json!({"Z": "5", "L": "0.48"}));
    r.gemini(filled.clone());
    r.gemini(filled);
    assert_eq!(r.position(STRAT), 5);
}

#[test]
fn gemini_saying_a_refused_order_was_rejected_or_pulled_unfilled_agrees_with_us() {
    let mut r = rig();
    refused_unanswered(&mut r);
    assert!(r.gemini(update(1, "REJECTED", json!({"r": "InsufficientFunds"}))).is_empty());
    assert_eq!((r.venue.status().divergences, r.venue.status().problem), (0, None));

    let mut r = rig();
    refused_unanswered(&mut r);
    assert!(r.gemini(update(1, "CANCELED", json!({"Z": "0"}))).is_empty());
    assert_eq!((r.venue.status().divergences, r.venue.status().problem), (0, None));
}

#[test]
fn a_second_look_is_never_sooner_than_the_wait_whoever_asks_for_it() {
    let mut r = rig();
    let acc = r.venue.submit(S, STRAT, &buy_up("c1", "5", "0.48")).unwrap();
    r.frames();
    let id = OrderId(acc.venue_order_id);
    let later = Instant::now() + PLACE_TIMEOUT + Duration::from_millis(1);
    r.venue.due_lookups(later);
    r.venue.on_lookup_at(id, Ok(Vec::new()), later);

    // The socket drops a second later, which looks every order up at once.
    r.venue.on_disconnected();
    let soon = later + Duration::from_secs(1);
    assert_eq!(r.venue.due_lookups(soon).len(), 1);
    r.venue.on_lookup_at(id, Ok(Vec::new()), soon);
    assert!(r.events().is_empty(), "a second miss a second after the first decides nothing");
    assert_eq!(r.venue.status().live_orders, 1);

    assert!(r.venue.due_lookups(later + LOOKUP_RETRY - Duration::from_millis(1)).is_empty());
    assert_eq!(r.venue.due_lookups(later + LOOKUP_RETRY).len(), 1);
    r.venue.on_lookup_at(id, Ok(Vec::new()), later + LOOKUP_RETRY);
    assert_eq!(r.events(), vec![("c1".to_string(), ExecKind::Reject(ExecReject::VenueUnreachable), 0, 0, None)]);
}

#[test]
fn a_cancel_gemini_refuses_is_looked_up_after_a_wait_not_in_a_loop() {
    let mut r = rig();
    let acc = r.venue.submit(S, STRAT, &buy_up("c1", "5", "0.48")).unwrap();
    r.gemini(update(1, "NEW", json!({})));
    r.frames();
    r.venue.cancel(S, STRAT, acc.venue_order_id).unwrap();
    let cancel = r.frames().remove(0);
    r.gemini(response(&cancel["id"], 400));

    let now = Instant::now();
    assert!(r.venue.due_lookups(now).is_empty(), "not at once: Gemini would refuse the next cancel just as fast");
    assert_eq!(r.venue.due_lookups(now + LOOKUP_RETRY + Duration::from_millis(50)).len(), 1);
}

#[test]
fn a_frame_queued_before_the_socket_dropped_is_not_sent_on_the_next_one() {
    let mut r = rig();
    r.venue.submit(S, STRAT, &buy_up("c1", "5", "0.48")).unwrap();
    // The socket drops with the placement still in the queue, then is back.
    r.venue.on_disconnected();
    r.venue.on_connected();
    assert!(r.frames().is_empty(), "a placement made for the old socket must not reach the new one");
    // The order is looked up like any other that was open at the drop.
    assert_eq!(r.venue.due_lookups(Instant::now()).len(), 1);
}

#[test]
fn a_cancel_asked_while_the_socket_is_down_goes_out_when_it_is_back() {
    let mut r = rig();
    let acc = r.venue.submit(S, STRAT, &buy_up("c1", "5", "0.48")).unwrap();
    r.gemini(update(1, "NEW", json!({})));
    r.frames();
    r.venue.on_disconnected();
    r.venue.cancel(S, STRAT, acc.venue_order_id).unwrap();
    assert!(r.frames().is_empty(), "there is no socket to send it on");

    let frames: Vec<Value> = r.venue.on_connected().iter().map(|f| serde_json::from_str(f).unwrap()).collect();
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0]["method"], "SUBSCRIBE");
    assert_eq!(frames[1]["method"], "order.cancel");
    assert_eq!(frames[1]["params"]["orderId"], json!(GID + 1));
}

#[test]
fn an_unanswered_placement_gemini_did_take_is_adopted_with_its_fills() {
    let mut r = rig();
    let acc = r.venue.submit(S, STRAT, &buy_up("c1", "5", "0.48")).unwrap();
    r.frames();
    let id = OrderId(acc.venue_order_id);
    r.venue.due_lookups(Instant::now() + PLACE_TIMEOUT + Duration::from_millis(1));

    assert!(r.venue.on_lookup(id, Ok(vec![rest_order(1, RestOrderStatus::Open, 5, 2, Some("0.47"))])).is_empty());
    assert_eq!(r.events(), vec![("c1".to_string(), ExecKind::Partial, 2, 3, Some(470))]);
    assert!(r.frames().is_empty());
    assert_eq!(r.venue.status().live_orders, 1, "still open on Gemini");
    // It is answered now, so it is not looked up again.
    assert!(r.venue.due_lookups(Instant::now() + PLACE_TIMEOUT * 3).is_empty());
    // And the stream can finish it by Gemini's id alone.
    r.gemini(json!({"e": "orderUpdate", "s": SYMBOL, "i": GID + 1, "X": "FILLED", "Z": "3", "L": "0.48", "t": 5}));
    assert_eq!(r.events(), vec![("c1".to_string(), ExecKind::Fill, 3, 0, Some(480))]);
}

#[test]
fn a_fill_adopted_from_a_lookup_is_not_booked_again_when_the_stream_reports_it_late() {
    let mut r = rig();
    let acc = r.venue.submit(S, STRAT, &buy_up("c1", "5", "0.48")).unwrap();
    r.frames();
    let id = OrderId(acc.venue_order_id);
    r.venue.due_lookups(Instant::now() + PLACE_TIMEOUT + Duration::from_millis(1));
    r.venue.on_lookup(id, Ok(vec![rest_order(1, RestOrderStatus::Open, 5, 2, Some("0.47"))]));
    assert_eq!(r.events(), vec![("c1".to_string(), ExecKind::Partial, 2, 3, Some(470))]);

    // The stream now delivers that same fill: two filled, three remaining.
    r.gemini(update(1, "PARTIALLY_FILLED", json!({"q": "5", "z": "3", "Z": "2", "L": "0.47", "t": 5})));
    assert!(r.events().is_empty(), "already booked from the lookup");
    assert_eq!(r.position(STRAT), 2);

    // And then the rest.
    r.gemini(update(1, "FILLED", json!({"q": "5", "z": "0", "Z": "3", "L": "0.48", "t": 6})));
    assert_eq!(r.events(), vec![("c1".to_string(), ExecKind::Fill, 3, 0, Some(480))]);
    assert_eq!(r.position(STRAT), 5);
}

#[test]
fn a_fill_report_with_totals_also_books_a_fill_that_was_never_reported() {
    let mut r = rig();
    r.venue.submit(S, STRAT, &buy_up("c1", "5", "0.48")).unwrap();
    r.gemini(update(1, "NEW", json!({})));
    r.events();
    // One contract in this report, but Gemini says four are filled in all.
    r.gemini(update(1, "PARTIALLY_FILLED", json!({"q": "5", "z": "1", "Z": "1", "L": "0.48", "t": 9})));
    assert_eq!(r.events(), vec![("c1".to_string(), ExecKind::Partial, 4, 1, Some(480))]);
    assert_eq!(r.position(STRAT), 4);
}

#[test]
fn a_lookup_that_fails_is_tried_again_later_and_changes_nothing() {
    let mut r = rig();
    let acc = r.venue.submit(S, STRAT, &buy_up("c1", "5", "0.48")).unwrap();
    r.frames();
    let id = OrderId(acc.venue_order_id);
    let later = Instant::now() + PLACE_TIMEOUT + Duration::from_millis(1);
    r.venue.due_lookups(later);
    assert!(r.venue.on_lookup_at(id, Err("timed out".into()), later).is_empty());
    assert!(r.events().is_empty());
    assert_eq!(r.venue.status().live_orders, 1);
    assert!(r.venue.due_lookups(later + LOOKUP_RETRY / 2).is_empty(), "not at once");
    assert_eq!(r.venue.due_lookups(later + LOOKUP_RETRY).len(), 1);
}

#[test]
fn losing_the_socket_looks_every_sent_order_up() {
    let mut r = rig();
    let a = r.venue.submit(S, STRAT, &buy_up("c1", "5", "0.48")).unwrap();
    let b = r.venue.submit(S, OTHER, &buy_up("c2", "4", "0.48")).unwrap();
    r.gemini(update(1, "NEW", json!({})));
    r.gemini(update(2, "NEW", json!({})));
    r.frames();

    r.venue.on_disconnected();
    assert!(!r.venue.status().connected);
    assert_eq!(r.venue.submit(S, STRAT, &buy_up("c3", "1", "0.48")), Err(RejectReason::InternalError), "no orders without the socket");
    let due = r.venue.due_lookups(Instant::now());
    assert_eq!(due.iter().map(|l| l.order.0).collect::<Vec<_>>(), vec![a.venue_order_id, b.venue_order_id]);

    // While we were away: the first filled one and was pulled, the second filled whole.
    let found = vec![rest_order(1, RestOrderStatus::Cancelled, 5, 1, Some("0.48")), rest_order(2, RestOrderStatus::Filled, 4, 4, Some("0.46"))];
    r.venue.on_lookup(OrderId(a.venue_order_id), Ok(found.clone()));
    r.venue.on_lookup(OrderId(b.venue_order_id), Ok(found));
    assert_eq!(
        r.events(),
        vec![
            ("c1".to_string(), ExecKind::Partial, 1, 4, Some(480)),
            ("c1".to_string(), ExecKind::CancelAck, 0, 0, None),
            ("c2".to_string(), ExecKind::Fill, 4, 0, Some(460)),
        ]
    );
    assert_eq!((r.position(STRAT), r.position(OTHER)), (1, 4));
    assert_eq!(r.venue.status().live_orders, 0);
}

// ───────────────────────── replace ─────────────────────────

#[test]
fn a_replacement_waits_for_the_old_order_to_be_cancelled() {
    let mut r = rig();
    let old = r.venue.submit(S, STRAT, &buy_up("c1", "5", "0.48")).unwrap();
    r.gemini(update(1, "NEW", json!({})));
    r.frames();

    let new = r.venue.cancel_replace(S, STRAT, old.venue_order_id, &buy_up("c2", "5", "0.50")).unwrap();
    assert_ne!(new.venue_order_id, old.venue_order_id);
    let frames = r.frames();
    assert_eq!(frames.len(), 1, "only the cancel goes out");
    assert_eq!(frames[0]["method"], "order.cancel");
    assert!(r.events().is_empty());

    let answer = r.gemini(update(1, "CANCELED", json!({})));
    assert_eq!(r.events(), vec![("c1".to_string(), ExecKind::Replaced, 0, 0, None)]);
    assert_eq!(answer.len(), 1);
    assert_eq!(answer[0]["method"], "order.place");
    assert_eq!(answer[0]["params"]["price"], "0.5");
    assert_eq!(answer[0]["params"]["clientOrderId"], "kt-2");

    r.gemini(update(2, "FILLED", json!({"Z": "5", "L": "0.50", "t": 3})));
    assert_eq!(r.events(), vec![("c2".to_string(), ExecKind::Fill, 5, 0, Some(500))]);
    assert_eq!(r.position(STRAT), 5);
}

#[test]
fn a_replacement_is_refused_when_the_old_order_fills_first() {
    let mut r = rig();
    let old = r.venue.submit(S, STRAT, &buy_up("c1", "5", "0.48")).unwrap();
    r.gemini(update(1, "NEW", json!({})));
    r.venue.cancel_replace(S, STRAT, old.venue_order_id, &buy_up("c2", "5", "0.50")).unwrap();
    r.frames();

    let answer = r.gemini(update(1, "FILLED", json!({"Z": "5", "L": "0.48", "t": 3})));
    assert!(answer.is_empty(), "the new order is never sent");
    assert_eq!(
        r.events(),
        vec![("c1".to_string(), ExecKind::Fill, 5, 0, Some(480)), ("c2".to_string(), ExecKind::Reject(ExecReject::VenueRefused), 0, 0, None)]
    );
    assert_eq!(r.position(STRAT), 5, "five, not ten");
    assert_eq!(r.venue.status().live_orders, 0);
}

#[test]
fn a_replacement_is_refused_when_its_old_order_was_never_answered() {
    let mut r = rig();
    let old = r.venue.submit(S, STRAT, &buy_up("c1", "5", "0.48")).unwrap();
    r.venue.cancel_replace(S, STRAT, old.venue_order_id, &buy_up("c2", "5", "0.50")).unwrap();
    assert_eq!(r.frames().len(), 1, "only the first placement; there is no id to cancel it by");

    // Gemini never answers for the old order and two lookups miss it. It
    // may still turn up there, so the new one must not go out beside it.
    let id = OrderId(old.venue_order_id);
    let later = Instant::now() + PLACE_TIMEOUT + Duration::from_millis(1);
    r.venue.due_lookups(later);
    r.venue.on_lookup_at(id, Ok(Vec::new()), later);
    r.venue.due_lookups(later + LOOKUP_RETRY);
    let answer = r.venue.on_lookup_at(id, Ok(Vec::new()), later + LOOKUP_RETRY);

    assert!(answer.is_empty() && r.frames().is_empty(), "the replacement is never sent");
    assert_eq!(
        r.events(),
        vec![
            ("c1".to_string(), ExecKind::Reject(ExecReject::VenueUnreachable), 0, 0, None),
            ("c2".to_string(), ExecKind::Reject(ExecReject::VenueRefused), 0, 0, None),
        ]
    );
    assert_eq!(r.venue.status().live_orders, 0);
}

#[test]
fn a_placement_gemini_answers_with_a_server_error_is_looked_up_not_ended() {
    let mut r = rig();
    let old = r.venue.submit(S, STRAT, &buy_up("c1", "5", "0.48")).unwrap();
    let place = r.frames().remove(0);
    r.venue.cancel_replace(S, STRAT, old.venue_order_id, &buy_up("c2", "5", "0.50")).unwrap();
    r.frames();

    // A fault on Gemini's side says nothing about whether it took the
    // order: ending it here would send the replacement beside it.
    assert!(r.gemini(response(&place["id"], 503)).is_empty());
    assert!(r.events().is_empty(), "a server error is not a refusal");
    assert!(r.frames().is_empty(), "the replacement stays held");
    assert_eq!(r.venue.status().live_orders, 2);

    // It is looked up at once, and Gemini did take it.
    let id = OrderId(old.venue_order_id);
    let now = Instant::now() + Duration::from_millis(1);
    assert_eq!(r.venue.due_lookups(now), vec![LookupRequest { order: id, symbol: SYMBOL.into() }]);
    let answer = r.venue.on_lookup_at(id, Ok(vec![rest_order(1, RestOrderStatus::Open, 5, 0, None)]), now);
    let sent: Vec<Value> = answer.iter().map(|f| serde_json::from_str(f).unwrap()).collect();
    assert_eq!(sent.len(), 1, "the cancel that waited for its id");
    assert_eq!(sent[0]["method"], "order.cancel");
    assert_eq!(sent[0]["params"]["orderId"], json!(GID + 1));
    assert!(r.events().is_empty());
}

#[test]
fn a_server_error_for_an_order_gemini_never_took_ends_it_after_two_looks() {
    let mut r = rig();
    let acc = r.venue.submit(S, STRAT, &buy_up("c1", "5", "0.48")).unwrap();
    let place = r.frames().remove(0);
    r.gemini(response(&place["id"], 500));
    let id = OrderId(acc.venue_order_id);
    let now = Instant::now() + Duration::from_millis(1);
    r.venue.due_lookups(now);
    r.venue.on_lookup_at(id, Ok(Vec::new()), now);
    assert!(r.events().is_empty(), "one miss decides nothing");
    r.venue.due_lookups(now + LOOKUP_RETRY);
    r.venue.on_lookup_at(id, Ok(Vec::new()), now + LOOKUP_RETRY);
    assert_eq!(r.events(), vec![("c1".to_string(), ExecKind::Reject(ExecReject::VenueUnreachable), 0, 0, None)]);
}

#[test]
fn a_refusal_for_an_order_gemini_already_reported_live_is_looked_up() {
    let mut r = rig();
    let acc = r.venue.submit(S, STRAT, &buy_up("c1", "5", "0.48")).unwrap();
    let place = r.frames().remove(0);
    r.gemini(update(1, "NEW", json!({})));

    // The stream said it is live; a reply that says otherwise does not
    // end it. Gemini's own list settles which is right.
    assert!(r.gemini(response(&place["id"], 400)).is_empty());
    assert!(r.events().is_empty());
    assert_eq!(r.venue.status().live_orders, 1);
    let now = Instant::now() + Duration::from_millis(1);
    assert_eq!(r.venue.due_lookups(now), vec![LookupRequest { order: OrderId(acc.venue_order_id), symbol: SYMBOL.into() }]);
}

#[test]
fn a_cancel_for_an_order_gemini_acknowledged_without_naming_it_looks_it_up() {
    let mut r = rig();
    let acc = r.venue.submit(S, STRAT, &buy_up("c1", "5", "0.48")).unwrap();
    let place = r.frames().remove(0);
    // Accepted, but no report ever names the order: there is no id to
    // cancel it by, and nothing else would ever ask for one.
    r.gemini(response(&place["id"], 200));
    r.venue.cancel(S, STRAT, acc.venue_order_id).unwrap();
    assert!(r.frames().is_empty());

    let id = OrderId(acc.venue_order_id);
    let now = Instant::now();
    assert!(r.venue.due_lookups(now).is_empty(), "the report may only be late");
    assert_eq!(r.venue.due_lookups(now + LOOKUP_RETRY + Duration::from_millis(1)).len(), 1);
    let answer = r.venue.on_lookup_at(id, Ok(vec![rest_order(1, RestOrderStatus::Open, 5, 0, None)]), now + LOOKUP_RETRY);
    let sent: Vec<Value> = answer.iter().map(|f| serde_json::from_str(f).unwrap()).collect();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["method"], "order.cancel");
    assert_eq!(sent[0]["params"]["orderId"], json!(GID + 1));
}

#[test]
fn replacing_an_order_that_is_not_live_is_refused() {
    let mut r = rig();
    assert_eq!(r.venue.cancel_replace(S, STRAT, 77, &buy_up("c2", "5", "0.50")), Err(RejectReason::InvalidOrder));
    let old = r.venue.submit(S, STRAT, &buy_up("c1", "5", "0.48")).unwrap();
    assert_eq!(r.venue.cancel_replace(S, OTHER, old.venue_order_id, &buy_up("c2", "5", "0.50")), Err(RejectReason::InvalidOrder));
    r.frames();
    assert_eq!(r.venue.status().live_orders, 1);
}

// ───────────────────────── refused here ─────────────────────────

#[test]
fn orders_gemini_would_not_take_are_refused_before_they_leave() {
    let mut r = rig();
    let mut market = buy_up("c1", "5", "0.48");
    market.order_type = OrderType::Market;
    market.price = None;
    assert_eq!(r.venue.submit(S, STRAT, &market), Err(RejectReason::InvalidOrder));
    assert_eq!(r.venue.submit(S, STRAT, &buy_up("c1", "5", "0.005")), Err(RejectReason::PriceBand), "half a cent is not on Gemini's grid");
    assert_eq!(r.venue.submit(S, STRAT, &buy_up("c1", "5", "0.485")), Err(RejectReason::PriceBand), "a limit is never rounded to a cent it did not name");
    assert_eq!(r.venue.submit(S, STRAT, &order("c1", "down", IrSide::Buy, "5", "0.305")), Err(RejectReason::PriceBand));
    assert_eq!(r.venue.submit(S, STRAT, &buy_up("c1", "0.4", "0.48")), Err(RejectReason::InvalidOrder), "no whole contract");
    let mut foreign = buy_up("c1", "5", "0.48");
    foreign.instrument_id = "b.current".into();
    assert_eq!(r.venue.submit(S, STRAT, &foreign), Err(RejectReason::InvalidOrder));
    assert!(r.frames().is_empty());
    assert!(r.events().is_empty());
    assert_eq!(r.venue.status().live_orders, 0);
}

#[test]
fn a_third_order_in_one_second_from_one_account_is_refused() {
    let mut r = rig();
    assert_eq!(MAX_REAL_ORDERS_PER_SEC, 2);
    r.venue.submit(S, STRAT, &buy_up("c1", "1", "0.48")).unwrap();
    r.venue.submit(S, STRAT, &buy_up("c2", "1", "0.48")).unwrap();
    assert_eq!(r.venue.submit(S, STRAT, &buy_up("c3", "1", "0.48")), Err(RejectReason::EnvelopeViolation));
    r.venue.submit(S, OTHER, &buy_up("c4", "1", "0.48")).unwrap();
    assert_eq!(r.frames().len(), 3);
}

#[test]
fn nothing_is_sent_while_the_terms_are_not_accepted() {
    let mut r = rig();
    r.venue.set_terms(false);
    assert_eq!(r.venue.submit(S, STRAT, &buy_up("c1", "1", "0.48")), Err(RejectReason::InternalError));
    assert!(r.frames().is_empty());
    assert!(r.venue.status().problem.unwrap().contains("terms"));
}

#[test]
fn nothing_is_sent_without_a_contract_or_after_the_round_stops() {
    let mut r = rig();
    r.has_symbol.store(false, Ordering::SeqCst);
    assert_eq!(r.venue.submit(S, STRAT, &buy_up("c1", "1", "0.48")), Err(RejectReason::MarketClosed));
    r.has_symbol.store(true, Ordering::SeqCst);
    r.now.store(OPENS + 300, Ordering::SeqCst);
    assert_eq!(r.venue.submit(S, STRAT, &buy_up("c1", "1", "0.48")), Err(RejectReason::MarketClosed));
    assert!(r.frames().is_empty());
    assert_eq!(r.venue.status().live_orders, 0);
}

#[test]
fn a_reduce_only_order_is_cut_to_the_position_or_refused() {
    let mut r = rig();
    r.exchange.lock().unwrap().apply_external_fill(S, STRAT, Side::Bid, PriceTick::from_tenths(480).unwrap(), Qty(3)).unwrap();
    let mut close = order("c1", "up", IrSide::Sell, "5", "0.50");
    close.reduce_only = true;
    r.venue.submit(S, STRAT, &close).unwrap();
    assert_eq!(r.frames()[0]["params"]["quantity"], "3");
    let mut open = buy_up("c2", "5", "0.48");
    open.reduce_only = true;
    assert_eq!(r.venue.submit(S, STRAT, &open), Err(RejectReason::InvalidOrder), "a buy reduces nothing when long");
}

#[test]
fn an_order_the_account_cannot_pay_for_is_refused_before_it_leaves() {
    let mut r = rig();
    const POOR: Address = Address([0x53; 20]);
    r.exchange.lock().unwrap().seed_bot_account_in(POOR, Tenths(5_000), "poor".into(), None, None, S);
    // Five at 48 cents is $2.40 of the $5.
    r.venue.submit(S, POOR, &buy_up("c1", "5", "0.48")).unwrap();
    // Five more at 60 cents would make $5.40 if both filled.
    assert_eq!(r.venue.submit(S, POOR, &buy_up("c2", "5", "0.60")), Err(RejectReason::InsufficientFunds));
    // Selling Up at 60 is buying Down at 40: $2.00, which fits.
    r.venue.submit(S, POOR, &order("c3", "up", IrSide::Sell, "5", "0.60")).unwrap();
    assert_eq!(r.frames().len(), 2);
    assert_eq!(r.venue.status().live_orders, 2);
}

#[test]
fn an_order_that_only_closes_a_position_needs_no_cash() {
    let r = rig();
    const POOR: Address = Address([0x53; 20]);
    r.exchange.lock().unwrap().seed_bot_account_in(POOR, Tenths(2_400), "poor".into(), None, None, S);
    r.exchange.lock().unwrap().apply_external_fill(S, POOR, Side::Bid, PriceTick::from_tenths(480).unwrap(), Qty(5)).unwrap();
    assert_eq!(r.exchange.lock().unwrap().account(&POOR).unwrap().usdc_balance, Tenths(0));
    // Long five with nothing left: selling the five is free, a sixth is not.
    assert_eq!(r.venue.submit(S, POOR, &order("c1", "up", IrSide::Sell, "6", "0.50")), Err(RejectReason::InsufficientFunds));
    r.venue.submit(S, POOR, &order("c2", "up", IrSide::Sell, "5", "0.50")).unwrap();
    // And those five are spoken for: a second close has nothing left to close.
    assert_eq!(r.venue.submit(S, POOR, &order("c3", "up", IrSide::Sell, "1", "0.50")), Err(RejectReason::InsufficientFunds));
}

// ───────────────────────── the round ends ─────────────────────────

#[test]
fn settlement_ends_open_orders_and_a_fill_after_it_is_a_divergence() {
    let mut r = rig();
    r.venue.submit(S, STRAT, &buy_up("c1", "5", "0.48")).unwrap();
    r.gemini(update(1, "NEW", json!({})));
    r.exchange.lock().unwrap().resolve_only(S, Outcome::Yes, RoundClose::Btc { close_price_btc_usd: 100_050.0 });
    assert_eq!(r.events(), vec![("c1".to_string(), ExecKind::CancelAck, 0, 0, None)]);

    // Gemini's own cancel for the closed contract changes nothing more.
    r.gemini(update(1, "CANCELED", json!({})));
    assert!(r.events().is_empty());
    assert_eq!(r.venue.status().divergences, 0);
    assert_eq!(r.venue.status().live_orders, 0);
}

#[test]
fn a_fill_the_ledger_can_no_longer_book_is_counted_and_said() {
    let mut r = rig();
    r.venue.submit(S, STRAT, &buy_up("c1", "5", "0.48")).unwrap();
    r.gemini(update(1, "NEW", json!({})));
    r.exchange.lock().unwrap().resolve_only(S, Outcome::Yes, RoundClose::Btc { close_price_btc_usd: 100_050.0 });
    r.events();

    r.gemini(update(1, "FILLED", json!({"Z": "5", "L": "0.48", "t": 9})));
    assert!(r.events().is_empty());
    let status = r.venue.status();
    assert_eq!(status.divergences, 1);
    assert!(status.problem.unwrap().contains("could not be booked"));
    assert_eq!(status.live_orders, 0);

    // Reading the terms again, as every reconnect does, must not unsay it.
    r.venue.set_terms(true);
    assert!(r.venue.status().problem.unwrap().contains("could not be booked"));
    r.venue.set_terms(false);
    assert!(r.venue.status().problem.unwrap().contains("could not be booked"));
}

#[test]
fn accepting_the_terms_clears_the_terms_problem() {
    let r = rig();
    r.venue.set_terms(false);
    assert!(r.venue.status().problem.unwrap().contains("terms"));
    r.venue.set_terms(true);
    assert_eq!(r.venue.status().problem, None);
}

#[test]
fn orders_on_one_contract_are_looked_up_in_one_read() {
    let due = vec![
        LookupRequest { order: OrderId(3), symbol: "B".into() },
        LookupRequest { order: OrderId(1), symbol: "A".into() },
        LookupRequest { order: OrderId(2), symbol: "A".into() },
    ];
    assert_eq!(by_symbol(due), vec![("A".to_string(), vec![OrderId(1), OrderId(2)]), ("B".to_string(), vec![OrderId(3)])]);
}

#[test]
fn an_order_whose_round_settled_is_pulled_from_gemini_and_forgotten() {
    let mut r = rig();
    let acc = r.venue.submit(S, STRAT, &buy_up("c1", "5", "0.48")).unwrap();
    r.gemini(update(1, "NEW", json!({})));
    r.frames();
    r.exchange.lock().unwrap().resolve_only(S, Outcome::Yes, RoundClose::Btc { close_price_btc_usd: 100_050.0 });
    r.events();

    // The next tick notices: the order is cancelled on Gemini and looked up.
    let due = r.venue.due_lookups(Instant::now());
    assert_eq!(due, vec![LookupRequest { order: OrderId(acc.venue_order_id), symbol: SYMBOL.into() }]);
    let frames = r.frames();
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0]["method"], "order.cancel");
    assert_eq!(frames[0]["params"]["orderId"], GID + 1);
    // Asked once, however many ticks pass.
    r.venue.on_lookup(OrderId(acc.venue_order_id), Err("timed out".into()));
    r.venue.due_lookups(Instant::now() + LOOKUP_RETRY * 2);
    assert!(r.frames().is_empty());

    r.venue.on_lookup(OrderId(acc.venue_order_id), Ok(vec![rest_order(1, RestOrderStatus::Cancelled, 5, 0, None)]));
    assert!(r.events().is_empty(), "the engine already said it was cancelled");
    assert_eq!((r.venue.status().live_orders, r.venue.status().divergences), (0, 0));
}

#[test]
fn a_settled_order_gemini_no_longer_lists_is_forgotten() {
    let r = rig();
    let acc = r.venue.submit(S, STRAT, &buy_up("c1", "5", "0.48")).unwrap();
    r.gemini(update(1, "NEW", json!({})));
    r.exchange.lock().unwrap().resolve_only(S, Outcome::Yes, RoundClose::Btc { close_price_btc_usd: 100_050.0 });
    r.venue.due_lookups(Instant::now());
    r.venue.on_lookup(OrderId(acc.venue_order_id), Ok(vec![]));
    assert_eq!(r.venue.status().live_orders, 0);
}

#[test]
fn a_held_replacement_is_dropped_when_its_round_settles_first() {
    let mut r = rig();
    let old = r.venue.submit(S, STRAT, &buy_up("c1", "5", "0.48")).unwrap();
    r.gemini(update(1, "NEW", json!({})));
    r.venue.cancel_replace(S, STRAT, old.venue_order_id, &buy_up("c2", "5", "0.50")).unwrap();
    r.frames();
    r.exchange.lock().unwrap().resolve_only(S, Outcome::Yes, RoundClose::Btc { close_price_btc_usd: 100_050.0 });
    r.events();

    let answer = r.gemini(update(1, "CANCELED", json!({})));
    assert!(answer.is_empty(), "no order is sent for a round that is over");
    assert!(r.frames().is_empty());
    assert!(r.events().is_empty());
    assert_eq!(r.venue.status().live_orders, 0);
}

#[test]
fn a_filled_report_that_leaves_the_order_short_is_looked_up() {
    let mut r = rig();
    let acc = r.venue.submit(S, STRAT, &buy_up("c1", "5", "0.48")).unwrap();
    r.gemini(update(1, "NEW", json!({})));
    r.events();
    // Gemini says the order is filled, but only three were ever reported
    // here: an earlier fill of two was missed.
    r.gemini(update(1, "FILLED", json!({"Z": "3", "L": "0.48", "t": 8})));
    assert_eq!(r.events(), vec![("c1".to_string(), ExecKind::Partial, 3, 2, Some(480))]);
    let id = OrderId(acc.venue_order_id);
    assert_eq!(r.venue.due_lookups(Instant::now()), vec![LookupRequest { order: id, symbol: SYMBOL.into() }]);
    r.venue.on_lookup(id, Ok(vec![rest_order(1, RestOrderStatus::Filled, 5, 5, Some("0.47"))]));
    assert_eq!(r.events(), vec![("c1".to_string(), ExecKind::Fill, 2, 0, Some(470))]);
    assert_eq!(r.position(STRAT), 5);
    assert_eq!(r.venue.status().live_orders, 0);
}

#[test]
fn an_order_larger_than_the_cap_is_refused_before_it_leaves() {
    let mut r = rig();
    assert_eq!(DEFAULT_MAX_ORDER_QTY, 25);
    r.venue.submit(S, STRAT, &buy_up("c1", "25", "0.48")).unwrap();
    assert_eq!(r.venue.submit(S, STRAT, &buy_up("c2", "26", "0.48")), Err(RejectReason::EnvelopeViolation));
    assert_eq!(r.frames().len(), 1);
    r.venue.set_max_order_qty(100);
    r.venue.submit(S, OTHER, &buy_up("c3", "26", "0.48")).unwrap();
    assert_eq!(r.frames().len(), 1);
}

// ───────────────────────── turning it on ─────────────────────────

#[test]
fn only_the_operators_strategies_reach_gemini() {
    let mut r = rig();
    let operator = Address([0x70; 20]);
    let (mine, theirs) = {
        let mut x = r.exchange.lock().unwrap();
        x.register_user(operator);
        x.get_or_create_account(operator);
        x.register_user(OTHER);
        (x.create_sub_account_in(operator, Tenths(100_000), S).unwrap(), x.create_sub_account_in(OTHER, Tenths(100_000), S).unwrap())
    };
    r.venue.set_operator(Some(operator));
    r.venue.submit(S, mine, &buy_up("c1", "1", "0.48")).unwrap();
    assert_eq!(r.venue.submit(S, theirs, &buy_up("c2", "1", "0.48")), Err(RejectReason::EnvelopeViolation), "another visitor's strategy");
    assert_eq!(r.venue.submit(S, operator, &buy_up("c3", "1", "0.48")), Err(RejectReason::EnvelopeViolation), "not a strategy account");
    assert_eq!(r.frames().len(), 1);
    assert_eq!(r.venue.status().live_orders, 1);
}

#[test]
fn every_series_gets_its_own_outside_account() {
    let mut x = Exchange::new(Tenths(1_000_000), SERIES);
    install_outside_accounts(&mut x, SeriesId::all());
    for s in SeriesId::all() {
        assert_eq!(x.engine(s).outside(), Some(outside_address(s)));
        assert!(x.account(&outside_address(s)).is_some());
    }
    assert_ne!(outside_address(SeriesId(0)), outside_address(SeriesId(1)));
    assert_ne!(outside_address(SeriesId(0)), crate::gemini::mirror_task::mirror_address(SeriesId(0)));
}

#[test]
fn the_operator_is_one_whole_address_or_the_boot_stops() {
    assert_eq!(parse_operator(Some("0x7070707070707070707070707070707070707070")), Ok(Address([0x70; 20])));
    assert_eq!(parse_operator(Some(" 0x7070707070707070707070707070707070707070 ")), Ok(Address([0x70; 20])));
    assert!(parse_operator(None).unwrap_err().contains("KYROS_REAL_OPERATOR"));
    assert!(parse_operator(Some("")).unwrap_err().contains("KYROS_REAL_OPERATOR"));
    assert!(parse_operator(Some("0x1234")).is_err());
}

#[test]
fn the_order_cap_is_a_whole_number_of_contracts_or_the_boot_stops() {
    assert_eq!(parse_max_order(None), Ok(DEFAULT_MAX_ORDER_QTY));
    assert_eq!(parse_max_order(Some("")), Ok(DEFAULT_MAX_ORDER_QTY));
    assert_eq!(parse_max_order(Some("5")), Ok(5));
    assert!(parse_max_order(Some("0")).is_err());
    assert!(parse_max_order(Some("lots")).unwrap_err().contains("KYROS_REAL_MAX_ORDER"));
}

#[test]
fn real_orders_need_the_flag_a_named_environment_and_a_key() {
    let key = || Ok(Some(ApiKey::new("account-test", "not-a-real-secret").unwrap()));
    let none = || Ok(None);
    // Off, however much else is set.
    assert!(real_orders_key(None, Some("sandbox"), key()).unwrap().is_none());
    assert!(real_orders_key(Some("0"), Some("production"), key()).unwrap().is_none());
    assert!(real_orders_key(Some(""), None, none()).unwrap().is_none());
    // On.
    assert!(real_orders_key(Some("1"), Some("sandbox"), key()).unwrap().is_some());
    assert!(real_orders_key(Some("1"), Some("production"), key()).unwrap().is_some());
    // Half-set stops the boot.
    assert!(real_orders_key(Some("1"), None, key()).unwrap_err().contains("GEMINI_ENV"), "the default environment is not consent");
    assert!(real_orders_key(Some("1"), Some("sandbox"), none()).unwrap_err().contains("GEMINI_API_KEY"));
    assert!(real_orders_key(Some("yes"), Some("sandbox"), key()).unwrap_err().contains("KYROS_REAL_ORDERS"));
    assert!(real_orders_key(Some("1"), Some("sandbox"), Err("one of the two is missing".into())).is_err());
}

// ───────────────────────── over a real socket ─────────────────────────

mod socket {
    use super::*;
    use tokio::net::TcpListener;
    use tokio_tungstenite::tungstenite::handshake::server::{Request as HttpRequest, Response as HttpResponse};

    const WAIT: Duration = Duration::from_secs(3);

    struct Seen {
        uri: String,
        apikey: Option<String>,
        nonce: Option<String>,
        payload: Option<String>,
        signature: Option<String>,
    }

    struct Conn {
        ws: tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
        seen: Seen,
    }

    impl Conn {
        async fn next_request(&mut self) -> Value {
            loop {
                let msg = tokio::time::timeout(WAIT, self.ws.next()).await.expect("no request in time").expect("client closed").expect("read");
                if let Message::Text(text) = msg {
                    return serde_json::from_str(text.as_str()).unwrap();
                }
            }
        }

        async fn send(&mut self, v: Value) {
            self.ws.send(Message::Text(v.to_string().into())).await.unwrap();
        }
    }

    async fn accept(listener: &TcpListener) -> Conn {
        let (stream, _) = tokio::time::timeout(WAIT, listener.accept()).await.expect("the venue never connected").unwrap();
        let seen = Arc::new(Mutex::new(None));
        let slot = seen.clone();
        let ws = tokio_tungstenite::accept_hdr_async(stream, move |req: &HttpRequest, resp: HttpResponse| {
            let header = |name: &str| req.headers().get(name).and_then(|v| v.to_str().ok()).map(str::to_owned);
            *slot.lock().unwrap() = Some(Seen {
                uri: req.uri().to_string(),
                apikey: header("X-GEMINI-APIKEY"),
                nonce: header("X-GEMINI-NONCE"),
                payload: header("X-GEMINI-PAYLOAD"),
                signature: header("X-GEMINI-SIGNATURE"),
            });
            Ok(resp)
        })
        .await
        .unwrap();
        let seen = seen.lock().unwrap().take().unwrap();
        Conn { ws, seen }
    }

    /// REST, played by the test: counts calls and answers from a script.
    struct FakeLookup {
        calls: AtomicUsize,
        found: Mutex<Vec<RestOrder>>,
    }

    impl OrderLookup for Arc<FakeLookup> {
        fn orders(&self, _symbol: String) -> BoxFuture<'static, Result<Vec<RestOrder>, String>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let found = self.found.lock().unwrap().clone();
            Box::pin(async move { Ok(found) })
        }

        fn terms_accepted(&self) -> BoxFuture<'static, Result<bool, String>> {
            Box::pin(async { Ok(true) })
        }
    }

    async fn until(mut done: impl FnMut() -> bool, what: &str) {
        let deadline = Instant::now() + WAIT;
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn session(url: String, idle_limit: Duration) -> SessionConfig {
        SessionConfig {
            url,
            backoff_base: Duration::from_millis(20),
            backoff_max: Duration::from_millis(80),
            ping_every: Duration::from_millis(50),
            idle_limit,
            nonce: gemini_private::auth::Nonce::new(),
        }
    }

    #[tokio::test]
    async fn a_socket_that_goes_silent_is_dropped_and_opened_again() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let mut r = rig();
        r.venue.on_disconnected();
        let lookup = Arc::new(FakeLookup { calls: AtomicUsize::new(0), found: Mutex::new(Vec::new()) });
        let key = ApiKey::new("account-test", "not-a-real-secret").unwrap();
        let venue = r.venue.clone();
        let driver = std::mem::replace(&mut r.driver, GeminiVenue::new(r.exchange.clone(), SeriesMap::default(), Arc::new(|_| None), "unused").1);
        tokio::spawn(run(driver, session(url, Duration::from_millis(300)), key, Arc::new(lookup.clone())));

        let mut first = accept(&listener).await;
        assert_eq!(first.next_request().await["method"], "SUBSCRIBE");
        until(|| venue.status().connected, "the venue to see the socket").await;
        // Gemini stops reading and writing but keeps the connection open:
        // nothing answers the venue's pings.
        let second = accept(&listener).await;
        assert!(second.seen.nonce.unwrap().parse::<u64>().unwrap() > first.seen.nonce.clone().unwrap().parse::<u64>().unwrap(), "every sign-in carries a new nonce");
        drop(first);
    }

    #[tokio::test]
    async fn the_task_signs_in_subscribes_places_books_and_looks_up_after_a_drop() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let mut r = rig();
        // The rig marked the venue connected by hand; the task must earn it.
        r.venue.on_disconnected();
        r.venue.set_terms(false);
        let lookup = Arc::new(FakeLookup { calls: AtomicUsize::new(0), found: Mutex::new(Vec::new()) });
        let key = ApiKey::new("account-test", "not-a-real-secret").unwrap();
        let config = session(url, Duration::from_secs(60));
        let venue = r.venue.clone();
        let driver = std::mem::replace(&mut r.driver, GeminiVenue::new(r.exchange.clone(), SeriesMap::default(), Arc::new(|_| None), "unused").1);
        tokio::spawn(run(driver, config, key, Arc::new(lookup.clone())));

        // The key is in the handshake, and a dropped socket pulls our orders.
        let mut conn = accept(&listener).await;
        assert!(conn.seen.uri.contains("cancelOnDisconnect=true"), "{}", conn.seen.uri);
        assert_eq!(conn.seen.apikey.as_deref(), Some("account-test"));
        let nonce = conn.seen.nonce.clone().unwrap();
        assert!(nonce.parse::<u64>().is_ok());
        let payload = conn.seen.payload.clone().unwrap();
        assert_eq!(conn.seen.signature.as_deref(), Some(gemini_private::auth::sign(b"not-a-real-secret", &payload).as_str()));
        let subscribe = conn.next_request().await;
        assert_eq!(subscribe["method"], "SUBSCRIBE");
        assert_eq!(subscribe["params"], json!([ORDER_STREAM]));
        until(|| venue.status().connected, "the venue to see the socket").await;

        // An order goes out, is accepted, and fills.
        venue.submit(S, STRAT, &buy_up("c1", "5", "0.48")).unwrap();
        let place = conn.next_request().await;
        assert_eq!(place["method"], "order.place");
        assert_eq!(place["params"]["clientOrderId"], "kt-1");
        conn.send(response(&place["id"], 200)).await;
        conn.send(update(1, "PARTIALLY_FILLED", json!({"Z": "2", "L": "0.48", "t": 1}))).await;
        until(|| r.position(STRAT) == 2, "the fill to be booked").await;

        // The socket dies with the order open; Gemini pulled it having filled one more.
        *lookup.found.lock().unwrap() = vec![rest_order(1, RestOrderStatus::Cancelled, 5, 3, Some("0.48"))];
        drop(conn);
        let mut again = accept(&listener).await;
        assert_eq!(again.next_request().await["method"], "SUBSCRIBE");
        until(|| venue.status().live_orders == 0, "the open order to be looked up").await;
        assert!(lookup.calls.load(Ordering::SeqCst) >= 1);
        assert_eq!(r.position(STRAT), 3);
        assert_eq!(
            r.events(),
            vec![
                ("c1".to_string(), ExecKind::Partial, 2, 3, Some(480)),
                ("c1".to_string(), ExecKind::Partial, 1, 2, Some(480)),
                ("c1".to_string(), ExecKind::CancelAck, 0, 0, None),
            ]
        );
    }
}
