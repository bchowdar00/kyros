//! B2 §11 integration surface for the derivatives tier-1 families, keyed on the
//! `open_interest_delta` warm-up boundary for both venues (the §5 test plan's
//! "not ready at t−1, ready at t" at the 900 s horizon). The exhaustive formula,
//! staleness and identity coverage lives in `venue_derivatives_signals.rs`; this
//! file is the boundary regression that the OI prune fix (diag-oi-delta) turns
//! green — the 900 s lookback must be satisfiable at any open-interest poll
//! cadence, so a warmed family is ready at 901 s on both a 5 s (Binance) and a
//! 1 s (Bybit) open-interest cadence, and warming at 899 s.

use std::collections::BTreeMap;

use btc_signals::Venue;
use btc_signals::usdt_reference::UsdtReferenceSnapshot;
use btc_signals::venue_derivatives_signals::VenueDerivativesEngine;
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use serde_json::{Value, json};
use signals_core::{QualityState, SignalUpdate, SourceEvent};

const ONE_SECOND_NS: u64 = 1_000_000_000;

fn reference() -> UsdtReferenceSnapshot {
    UsdtReferenceSnapshot::fresh(rust_decimal::Decimal::ONE)
}

fn wall_start() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-09-03T00:00:00Z")
        .expect("wall start parses")
        .with_timezone(&Utc)
}

fn ns(second: u64) -> u64 {
    second * ONE_SECOND_NS
}

fn timestamp(second: u64) -> String {
    wall_start()
        .checked_add_signed(Duration::seconds(i64::try_from(second).expect("fits")))
        .expect("representable")
        .to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn source_event(source_id: &str, second: u64, values: BTreeMap<String, Value>) -> SourceEvent {
    let event_time = timestamp(second);
    SourceEvent {
        schema_version: 1,
        source_id: source_id.to_owned(),
        source_version: "1.0.0".to_owned(),
        publisher_epoch: "deriv-boundary".to_owned(),
        sequence: second,
        event_time: event_time.clone(),
        received_at: event_time,
        values,
    }
}

// E62: Binance's mark/index/funding state is fed by the REST premium-index poll
// alone — the socket `mark-price` source was deleted with the `market` connection.
fn binance_mark(second: u64) -> SourceEvent {
    source_event(
        "binance.usdm.btcusdt.premium-index",
        second,
        BTreeMap::from([
            ("instrument".to_owned(), json!("BTCUSDT")),
            ("mark_price".to_owned(), json!("50000")),
            ("index_price".to_owned(), json!("50000")),
            ("estimated_settle_price".to_owned(), json!("50000")),
            ("last_funding_rate".to_owned(), json!("0.0001")),
            ("interest_rate".to_owned(), json!("0.0001")),
            ("next_funding_time".to_owned(), json!(timestamp(100_000))),
            ("event_time".to_owned(), json!(timestamp(second))),
        ]),
    )
}

fn binance_open_interest(second: u64, poll: u64) -> SourceEvent {
    source_event(
        "binance.usdm.btcusdt.open-interest",
        second,
        BTreeMap::from([
            ("instrument".to_owned(), json!("BTCUSDT")),
            ("open_interest_base".to_owned(), json!("1000")),
            ("poll_sequence".to_owned(), json!(poll)),
            ("event_time".to_owned(), json!(timestamp(second))),
        ]),
    )
}

fn bybit_ticker(second: u64, cross: u64) -> SourceEvent {
    source_event(
        "bybit.linear.btcusdt.tickers",
        second,
        BTreeMap::from([
            ("instrument".to_owned(), json!("BTCUSDT")),
            ("update_kind".to_owned(), json!("snapshot")),
            ("last_price".to_owned(), json!("50000")),
            ("mark_price".to_owned(), json!("50000")),
            ("index_price".to_owned(), json!("50000")),
            ("funding_rate".to_owned(), json!("0.0001")),
            ("next_funding_time".to_owned(), json!(timestamp(100_000))),
            ("open_interest_base".to_owned(), json!("1000")),
            ("open_interest_value_usd".to_owned(), json!("0")),
            ("best_bid_price".to_owned(), json!("50000")),
            ("best_bid_size".to_owned(), json!("1")),
            ("best_ask_price".to_owned(), json!("50000")),
            ("best_ask_size".to_owned(), json!("1")),
            ("cross_sequence".to_owned(), json!(cross)),
            ("event_time".to_owned(), json!(timestamp(second))),
        ]),
    )
}

fn find<'a>(updates: &'a [SignalUpdate], signal_id: &str) -> &'a SignalUpdate {
    updates
        .iter()
        .find(|u| u.signal_id == signal_id)
        .unwrap_or_else(|| panic!("missing {signal_id}"))
}

/// Binance open-interest polls at 5 s. Warming at 899 s, ready at 901 s — the
/// 900 s window-start reading survives prune at any poll cadence (diag-oi-delta).
#[test]
fn binance_open_interest_delta_warm_up_boundary_at_five_second_poll() {
    let mut engine = VenueDerivativesEngine::new(Venue::Binance, "epoch", wall_start()).unwrap();
    // Feed 0..=899 s: mark every second, open-interest every 5 s. Stop at 899 so
    // the warm-up assertion is a true boundary — taken before any 900/901 s
    // reading exists in the histories.
    for second in 0..=899 {
        engine.ingest(&binance_mark(second), ns(second)).unwrap();
        if second % 5 == 0 {
            engine
                .ingest(&binance_open_interest(second, second), ns(second))
                .unwrap();
        }
    }
    // 899 s: still warming (oi_clock warmed needs 900 s of observation).
    let at_899 = engine.emit(ns(899), reference()).unwrap();
    let oi_899 = find(&at_899, "btc.binance.open_interest_delta");
    assert_ne!(oi_899.quality.state, QualityState::Ready);
    assert!(
        oi_899
            .quality
            .reasons
            .contains(&"open_interest_history_warming_up".to_owned()),
        "reasons {:?}",
        oi_899.quality.reasons
    );
    // Now ingest the 900 and 901 s readings and confirm the boundary crosses.
    for second in 900..=901 {
        engine.ingest(&binance_mark(second), ns(second)).unwrap();
        if second % 5 == 0 {
            engine
                .ingest(&binance_open_interest(second, second), ns(second))
                .unwrap();
        }
    }
    // 901 s: ready — warmed and the 900 s lookback resolves to the t=0 reading.
    let at_901 = engine.emit(ns(901), reference()).unwrap();
    let oi_901 = find(&at_901, "btc.binance.open_interest_delta");
    assert_eq!(
        oi_901.quality.state,
        QualityState::Ready,
        "reasons {:?}",
        oi_901.quality.reasons
    );
}

/// Bybit folds open interest from the 1 s tickers. Same warm-up boundary.
#[test]
fn bybit_open_interest_delta_warm_up_boundary_at_one_second_cadence() {
    let mut engine = VenueDerivativesEngine::new(Venue::Bybit, "epoch", wall_start()).unwrap();
    // Feed 0..=899 s only, so the warm-up assertion is taken before the 900/901 s
    // ticker folds exist — a true boundary.
    for second in 0..=899 {
        engine
            .ingest(&bybit_ticker(second, 1_000 + second), ns(second))
            .unwrap();
    }
    let at_899 = engine.emit(ns(899), reference()).unwrap();
    let oi_899 = find(&at_899, "btc.bybit.open_interest_delta");
    assert_ne!(oi_899.quality.state, QualityState::Ready);
    assert!(
        oi_899
            .quality
            .reasons
            .contains(&"open_interest_history_warming_up".to_owned()),
        "reasons {:?}",
        oi_899.quality.reasons
    );
    // Now fold the 900 and 901 s tickers and confirm the boundary crosses.
    for second in 900..=901 {
        engine
            .ingest(&bybit_ticker(second, 1_000 + second), ns(second))
            .unwrap();
    }
    let at_901 = engine.emit(ns(901), reference()).unwrap();
    let oi_901 = find(&at_901, "btc.bybit.open_interest_delta");
    assert_eq!(
        oi_901.quality.state,
        QualityState::Ready,
        "reasons {:?}",
        oi_901.quality.reasons
    );
}

/// C3a / E59: the TA pack's one-second sample is the latest mark row held for up
/// to `PERP_MARK_HOLD_NS` (3 s); a poll stall longer than that hold is a missing
/// sample that breaks TA continuity exactly as a non-Ready quote does on the spot
/// lane, so the bar spanning the gap is invalid and every warmed `perp_*`
/// identity re-warms. The sub-3 s hold itself (a held mark keeps the ring ready
/// for at most two seconds) is exercised by the two E59 tests in
/// `venue_derivatives_signals.rs`. `perp_roc` is the cheapest to warm (301
/// one-second samples for its 300 s horizon), so it is the sentinel here for all
/// 14 identities.
#[test]
fn binance_mark_poll_stall_invalidates_the_bar_and_rewarms_perp_identities() {
    let mut engine = VenueDerivativesEngine::new(Venue::Binance, "epoch", wall_start()).unwrap();
    // Warm perp_roc to Ready on a steady 1 s mark poll: 0..=300 s is 301 samples,
    // enough for the 300 s (roc_bps_5m) horizon. Keep the 300 s decision.
    let mut ready = Vec::new();
    for second in 0..=300 {
        engine.ingest(&binance_mark(second), ns(second)).unwrap();
        ready = engine.emit(ns(second), reference()).unwrap();
    }
    assert_eq!(
        find(&ready, "btc.binance.perp_roc").quality.state,
        QualityState::Ready,
        "reasons {:?}",
        find(&ready, "btc.binance.perp_roc").quality.reasons
    );
    // Now the poll stalls: the last mark row stays at 300 s and no new row
    // arrives. Under E59 the sampler holds that row while it is younger than
    // `PERP_MARK_HOLD_NS` (3 s), so decisions 301 and 302 (age 1 s, 2 s) are held
    // repeats and perp_roc stays Ready. The row hits the 3 s bound at decision 303
    // and is no longer held: a missing sample breaks continuity, and by decision
    // 304 the stall is longer than the hold, so perp_roc re-warms rather than
    // publishing a fabricated bar.
    let mut stalled = Vec::new();
    for second in 301..=304 {
        stalled = engine.emit(ns(second), reference()).unwrap();
    }
    let roc = find(&stalled, "btc.binance.perp_roc");
    assert_ne!(
        roc.quality.state,
        QualityState::Ready,
        "a poll stall longer than the 3 s hold must break continuity, not hold Ready on a repeated mark"
    );
    assert!(
        roc.quality
            .reasons
            .contains(&"binance_warming_up".to_owned()),
        "reasons {:?}",
        roc.quality.reasons
    );
}

/// C3a control: an unbroken 1 s mark poll is unchanged — every second is a real
/// sample, so perp_roc warms to Ready and holds across the horizon where the
/// stalled poll above re-warmed.
#[test]
fn binance_steady_one_second_mark_poll_holds_perp_identities_ready() {
    let mut engine = VenueDerivativesEngine::new(Venue::Binance, "epoch", wall_start()).unwrap();
    let mut at_303 = Vec::new();
    for second in 0..=303 {
        engine.ingest(&binance_mark(second), ns(second)).unwrap();
        at_303 = engine.emit(ns(second), reference()).unwrap();
    }
    let roc = find(&at_303, "btc.binance.perp_roc");
    assert_eq!(
        roc.quality.state,
        QualityState::Ready,
        "reasons {:?}",
        roc.quality.reasons
    );
}

/// C3a control: the Bybit tickers path folds the mark at a 1 s cadence and is
/// likewise unchanged — each accepted ticker is a new mark row, so perp_roc
/// warms to Ready with no fabricated samples.
#[test]
fn bybit_one_second_tickers_hold_perp_identities_ready() {
    let mut engine = VenueDerivativesEngine::new(Venue::Bybit, "epoch", wall_start()).unwrap();
    let mut at_303 = Vec::new();
    for second in 0..=303 {
        engine
            .ingest(&bybit_ticker(second, 1_000 + second), ns(second))
            .unwrap();
        at_303 = engine.emit(ns(second), reference()).unwrap();
    }
    let roc = find(&at_303, "btc.bybit.perp_roc");
    assert_eq!(
        roc.quality.state,
        QualityState::Ready,
        "reasons {:?}",
        roc.quality.reasons
    );
}
