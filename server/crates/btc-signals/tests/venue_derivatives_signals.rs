//! B2 + C2 tests: the Binance USD-M and Bybit linear derivatives families (five
//! B2 tier-1 plus the two C2 perp momentum/volatility identities, E49)
//! driven through the public [`VenueDerivativesEngine`] — warm-up and staleness
//! boundaries, the formula rows with hand-computed decimals, the `interval_basis`
//! transitions, the honest `binance_recovery_required` on a missing Binance mark
//! row (E50), the Bybit confirmed liquidated-position side and its 60 s skew (R8,
//! R1 step 2), ticker ordering, the C2 perp momentum/volatility ring (warm-up
//! boundary, hand-checked returns and RMS, continuity break, mark-missing reason),
//! family-identity of the shared definitions, checkpoint round-trip and replay
//! determinism (spec §5). Since ruling E62, Binance produces twelve families and
//! Bybit fourteen; `liquidation_pressure`/`perp_cvd` are Bybit-only.
//!
//! The liquidation and perp-CVD formula rows themselves are unit-tested in the
//! module's own `#[cfg(test)]` block against the Bybit spec.

use std::collections::BTreeMap;

use btc_signals::Venue;
use btc_signals::usdt_reference::UsdtReferenceSnapshot;
use btc_signals::venue_derivatives_signals::VenueDerivativesEngine;
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use serde_json::{Value, json};
use signals_core::{QualityState, SignalDefinition, SignalUpdate, SourceEvent};

const ONE_SECOND_NS: u64 = 1_000_000_000;

/// A fresh USDT/USD reference at parity (rate 1.0), so the E47 conversion leaves
/// every quote-denominated value numerically unchanged and these B2 formula
/// assertions read the same figures; each converting family additionally carries
/// `usdt_usd_rate` "1" and `usdt_usd_rate_age_ms` 0. Conversion arithmetic at a
/// non-unit rate is covered by the `usdt_reference` unit tests and the venue
/// conversion tests.
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

fn source_event(
    source_id: &str,
    second: u64,
    cross: u64,
    values: BTreeMap<String, Value>,
) -> SourceEvent {
    let event_time = timestamp(second);
    SourceEvent {
        schema_version: 1,
        source_id: source_id.to_owned(),
        source_version: "1.0.0".to_owned(),
        publisher_epoch: "deriv-test".to_owned(),
        sequence: cross,
        event_time: event_time.clone(),
        received_at: event_time,
        values,
    }
}

/// A Bybit `tickers` snapshot carrying every declared field.
#[allow(clippy::too_many_arguments)]
fn bybit_ticker(
    second: u64,
    cross: u64,
    kind: &str,
    mark: &str,
    index: &str,
    funding_rate: &str,
    next_funding: &str,
    open_interest: &str,
) -> SourceEvent {
    source_event(
        "bybit.linear.btcusdt.tickers",
        second,
        cross,
        BTreeMap::from([
            ("instrument".to_owned(), json!("BTCUSDT")),
            ("update_kind".to_owned(), json!(kind)),
            ("last_price".to_owned(), json!(mark)),
            ("mark_price".to_owned(), json!(mark)),
            ("index_price".to_owned(), json!(index)),
            ("funding_rate".to_owned(), json!(funding_rate)),
            ("next_funding_time".to_owned(), json!(next_funding)),
            ("open_interest_base".to_owned(), json!(open_interest)),
            ("open_interest_value_usd".to_owned(), json!("0")),
            ("best_bid_price".to_owned(), json!(index)),
            ("best_bid_size".to_owned(), json!("1")),
            ("best_ask_price".to_owned(), json!(mark)),
            ("best_ask_size".to_owned(), json!("1")),
            ("cross_sequence".to_owned(), json!(cross)),
            ("event_time".to_owned(), json!(timestamp(second))),
        ]),
    )
}

// E62: Binance's mark/index/funding state is fed by the REST premium-index poll
// alone — the socket `mark-price` source was deleted with the `market` connection.
// `last_funding_rate` carries the funding rate the socket used to publish as
// `funding_rate` (F2/E50).
fn binance_mark(
    second: u64,
    mark: &str,
    index: &str,
    funding_rate: &str,
    next_funding: &str,
) -> SourceEvent {
    source_event(
        "binance.usdm.btcusdt.premium-index",
        second,
        second,
        BTreeMap::from([
            ("instrument".to_owned(), json!("BTCUSDT")),
            ("mark_price".to_owned(), json!(mark)),
            ("index_price".to_owned(), json!(index)),
            ("estimated_settle_price".to_owned(), json!(mark)),
            ("last_funding_rate".to_owned(), json!(funding_rate)),
            ("interest_rate".to_owned(), json!("0.0001")),
            ("next_funding_time".to_owned(), json!(next_funding)),
            ("event_time".to_owned(), json!(timestamp(second))),
        ]),
    )
}

fn binance_funding_info(second: u64, hours: u64) -> SourceEvent {
    source_event(
        "binance.usdm.btcusdt.funding-info",
        second,
        second,
        BTreeMap::from([
            ("instrument".to_owned(), json!("BTCUSDT")),
            ("funding_interval_hours".to_owned(), json!(hours)),
            ("adjusted_funding_rate_cap".to_owned(), json!("0.02")),
            ("adjusted_funding_rate_floor".to_owned(), json!("-0.02")),
            ("event_time".to_owned(), json!(timestamp(second))),
        ]),
    )
}

fn binance_open_interest(second: u64, oi: &str, poll: u64) -> SourceEvent {
    source_event(
        "binance.usdm.btcusdt.open-interest",
        second,
        second,
        BTreeMap::from([
            ("instrument".to_owned(), json!("BTCUSDT")),
            ("open_interest_base".to_owned(), json!(oi)),
            ("poll_sequence".to_owned(), json!(poll)),
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

// --- Bybit basis, funding, open interest -----------------------------------

#[test]
fn bybit_basis_warms_to_a_ready_frame_with_hand_checked_values() {
    let mut engine = VenueDerivativesEngine::new(Venue::Bybit, "epoch", wall_start()).unwrap();
    let next = timestamp(100_000);
    for second in 0..=60 {
        engine
            .ingest(
                &bybit_ticker(
                    second,
                    1_000 + second,
                    "snapshot",
                    "100.05",
                    "100",
                    "0.0001",
                    &next,
                    "1000",
                ),
                ns(second),
            )
            .unwrap();
    }
    let updates = engine.emit(ns(60), reference()).unwrap();
    let basis = find(&updates, "btc.bybit.mark_index_basis_state");
    assert_eq!(basis.quality.state, QualityState::Ready);
    // (100.05 - 100)/100*10000 = 5; constant, so both changes are 0.
    assert_eq!(basis.values["basis_bps"], json!("5"));
    assert_eq!(basis.values["basis_bps_change_10s"], json!("0"));
    assert_eq!(basis.values["basis_bps_change_60s"], json!("0"));
    assert_eq!(basis.values["regime"], json!("premium"));
    assert_eq!(basis.values["basis_stress"], json!(false));
    assert_eq!(basis.values["quote_currency"], json!("usd"));
    assert_eq!(basis.values["neutral_band_bps"], json!("2"));
}

#[test]
fn bybit_basis_is_not_ready_one_second_before_the_warmup_and_ready_at_it() {
    let mut engine = VenueDerivativesEngine::new(Venue::Bybit, "epoch", wall_start()).unwrap();
    let next = timestamp(100_000);
    for second in 0..=60 {
        engine
            .ingest(
                &bybit_ticker(
                    second,
                    1_000 + second,
                    "snapshot",
                    "100.05",
                    "100",
                    "0.0001",
                    &next,
                    "1000",
                ),
                ns(second),
            )
            .unwrap();
    }
    // 59 s of observation: still warming.
    let at_59 = engine.emit(ns(59), reference()).unwrap();
    let basis_59 = find(&at_59, "btc.bybit.mark_index_basis_state");
    assert_ne!(basis_59.quality.state, QualityState::Ready);
    assert!(
        basis_59
            .quality
            .reasons
            .contains(&"bybit_warming_up".to_owned())
    );
    // Exactly 60 s: ready.
    let at_60 = engine.emit(ns(60), reference()).unwrap();
    assert_eq!(
        find(&at_60, "btc.bybit.mark_index_basis_state")
            .quality
            .state,
        QualityState::Ready
    );
}

#[test]
fn bybit_basis_degrades_out_of_ready_past_the_staleness_bound() {
    let mut engine = VenueDerivativesEngine::new(Venue::Bybit, "epoch", wall_start()).unwrap();
    let next = timestamp(100_000);
    for second in 0..=60 {
        engine
            .ingest(
                &bybit_ticker(
                    second,
                    1_000 + second,
                    "snapshot",
                    "100.05",
                    "100",
                    "0.0001",
                    &next,
                    "1000",
                ),
                ns(second),
            )
            .unwrap();
    }
    assert_eq!(
        find(
            &engine.emit(ns(60), reference()).unwrap(),
            "btc.bybit.mark_index_basis_state"
        )
        .quality
        .state,
        QualityState::Ready
    );
    // 11 s later with no input: past the 10 s staleness bound.
    let stale = engine.emit(ns(71), reference()).unwrap();
    let basis = find(&stale, "btc.bybit.mark_index_basis_state");
    assert_ne!(basis.quality.state, QualityState::Ready);
    assert!(basis.quality.reasons.contains(&"bybit_stale".to_owned()));
}

#[test]
fn bybit_funding_annualises_with_the_configured_interval_then_the_observed_one() {
    let mut engine = VenueDerivativesEngine::new(Venue::Bybit, "epoch", wall_start()).unwrap();
    let next_a = timestamp(100_000);
    for second in 0..=60 {
        engine
            .ingest(
                &bybit_ticker(
                    second,
                    1_000 + second,
                    "snapshot",
                    "100",
                    "100",
                    "0.0001",
                    &next_a,
                    "1000",
                ),
                ns(second),
            )
            .unwrap();
    }
    let funding = find(
        &engine.emit(ns(60), reference()).unwrap(),
        "btc.bybit.funding_state",
    )
    .clone();
    assert_eq!(funding.quality.state, QualityState::Ready);
    // 0.0001 * 10000 = 1 bps; annualised = 1 * 8760/8 = 1095; longs pay (1 > 0.1).
    assert_eq!(funding.values["funding_rate_bps"], json!("1"));
    assert_eq!(funding.values["annualised_rate_bps"], json!("1095"));
    assert_eq!(funding.values["sign_regime"], json!("longs_pay"));
    assert_eq!(funding.values["interval_basis"], json!("configured"));
    assert_eq!(funding.values["funding_interval_hours"], json!("8"));

    // A second distinct next_funding_time 8 h later makes the interval observed.
    let next_b = timestamp(100_000 + 8 * 3_600);
    for second in 61..=70 {
        engine
            .ingest(
                &bybit_ticker(
                    second,
                    1_000 + second,
                    "snapshot",
                    "100",
                    "100",
                    "0.0001",
                    &next_b,
                    "1000",
                ),
                ns(second),
            )
            .unwrap();
    }
    let observed = find(
        &engine.emit(ns(70), reference()).unwrap(),
        "btc.bybit.funding_state",
    )
    .clone();
    assert_eq!(observed.values["interval_basis"], json!("observed"));
    assert_eq!(observed.values["funding_interval_hours"], json!("8"));
}

#[test]
fn bybit_open_interest_delta_is_ready_after_fifteen_minutes_with_hand_checked_values() {
    let mut engine = VenueDerivativesEngine::new(Venue::Bybit, "epoch", wall_start()).unwrap();
    let next = timestamp(100_000);
    // A reading at t=0 and t=901 s spans the 900 s window; the lookbacks resolve
    // to the t=0 reading for every horizon.
    engine
        .ingest(
            &bybit_ticker(0, 1, "snapshot", "50000", "50000", "0.0001", &next, "1000"),
            ns(0),
        )
        .unwrap();
    engine
        .ingest(
            &bybit_ticker(
                901, 2, "snapshot", "50500", "50500", "0.0001", &next, "1100",
            ),
            ns(901),
        )
        .unwrap();
    let oi = find(
        &engine.emit(ns(901), reference()).unwrap(),
        "btc.bybit.open_interest_delta",
    )
    .clone();
    assert_eq!(
        oi.quality.state,
        QualityState::Ready,
        "reasons {:?}",
        oi.quality.reasons
    );
    assert_eq!(oi.values["open_interest_base"], json!("1100"));
    // 1100 * 50500 = 55550000.
    assert_eq!(oi.values["open_interest_notional_usd"], json!("55550000"));
    // 1100 - 1000 = 100 for every horizon; 100/1000*100 = 10 %.
    assert_eq!(oi.values["delta_base_15m"], json!("100"));
    assert_eq!(oi.values["delta_pct_5m"], json!("10"));
    // (50500/50000 - 1) * 10000 = 100 bps.
    assert_eq!(oi.values["price_return_bps_5m"], json!("100"));
    assert_eq!(oi.values["quadrant"], json!("oi_up_price_up"));
    assert_eq!(oi.values["divergence"], json!(false));
}

// --- Binance: E34 unavailability, funding, open interest -------------------

/// F2/E50: the REST premium-index row that feeds the same mark state as the
/// socket mark-price event.
fn binance_premium_index(
    second: u64,
    mark: &str,
    index: &str,
    last_funding_rate: &str,
    next_funding: &str,
) -> SourceEvent {
    source_event(
        "binance.usdm.btcusdt.premium-index",
        second,
        second,
        BTreeMap::from([
            ("instrument".to_owned(), json!("BTCUSDT")),
            ("mark_price".to_owned(), json!(mark)),
            ("index_price".to_owned(), json!(index)),
            ("estimated_settle_price".to_owned(), json!(mark)),
            ("last_funding_rate".to_owned(), json!(last_funding_rate)),
            ("interest_rate".to_owned(), json!("0.0001")),
            ("next_funding_time".to_owned(), json!(next_funding)),
            ("event_time".to_owned(), json!(timestamp(second))),
        ]),
    )
}

#[test]
fn binance_does_not_publish_the_deleted_socket_families() {
    // E62: `perp_cvd` and `liquidation_pressure` were deleted with the Binance
    // `market`-socket sources (aggTrade/forceOrder) that fed them alone, so the
    // Binance engine never emits them at all — not even as unavailable. The
    // mark-fed `mark_index_basis_state` remains reachable over the REST
    // premium-index poll (F2/E50).
    let mut engine = VenueDerivativesEngine::new(Venue::Binance, "epoch", wall_start()).unwrap();
    let next = timestamp(100_000);
    for second in 0..=60 {
        engine
            .ingest(
                &binance_mark(second, "100.05", "100", "0.0001", &next),
                ns(second),
            )
            .unwrap();
    }
    let updates = engine.emit(ns(60), reference()).unwrap();
    for family in ["perp_cvd", "liquidation_pressure"] {
        let id = format!("btc.binance.{family}");
        assert!(
            updates.iter().all(|update| update.signal_id != id),
            "{id} must never be published by Binance (E62)"
        );
    }
    let basis = find(&updates, "btc.binance.mark_index_basis_state");
    assert_eq!(
        basis.quality.state,
        QualityState::Ready,
        "mark_index_basis_state is reachable from the mark state, reasons {:?}",
        basis.quality.reasons
    );
}

#[test]
fn binance_mark_families_are_unavailable_until_the_first_mark_row() {
    // F2/E50: before any mark row arrives (socket or premium-index), the mark-fed
    // families publish an honest binance_recovery_required Unavailable, NOT a
    // warming/stale that would never resolve if the mark plane is unreachable.
    // Feed open-interest and funding-info (the non-mark REST inputs) but no mark.
    let mut engine = VenueDerivativesEngine::new(Venue::Binance, "epoch", wall_start()).unwrap();
    engine.ingest(&binance_funding_info(0, 8), ns(0)).unwrap();
    for second in 0..=60 {
        engine
            .ingest(&binance_open_interest(second, "1000", second), ns(second))
            .unwrap();
    }
    let updates = engine.emit(ns(60), reference()).unwrap();
    for family in [
        "mark_index_basis_state",
        "funding_state",
        "open_interest_delta",
    ] {
        let id = format!("btc.binance.{family}");
        let update = find(&updates, &id);
        assert_eq!(
            update.quality.reasons,
            vec!["binance_recovery_required".to_owned()],
            "{id} is honestly unavailable until the first mark row, not warming/stale"
        );
    }
}

#[test]
fn binance_mark_families_reach_ready_over_the_rest_premium_index() {
    // F2/E50: the REST premium-index poll feeds the mark state, so the mark-fed
    // families are reachable from Tokyo without the socket mark-price stream.
    let mut engine = VenueDerivativesEngine::new(Venue::Binance, "epoch", wall_start()).unwrap();
    let next = timestamp(100_000);
    engine.ingest(&binance_funding_info(0, 8), ns(0)).unwrap();
    for second in 0..=60 {
        engine
            .ingest(
                &binance_premium_index(second, "100.05", "100", "0.0001", &next),
                ns(second),
            )
            .unwrap();
    }
    let updates = engine.emit(ns(60), reference()).unwrap();
    for family in ["mark_index_basis_state", "funding_state"] {
        let id = format!("btc.binance.{family}");
        let update = find(&updates, &id);
        assert_eq!(
            update.quality.state,
            QualityState::Ready,
            "{id} reaches ready over the premium-index poll, reasons {:?}",
            update.quality.reasons
        );
    }
    // The mark state read from the premium-index row is the same one the socket
    // would set: the basis identity carries the fed mark/index.
    let basis = find(&updates, "btc.binance.mark_index_basis_state");
    assert_eq!(basis.values["mark_price"], json!("100.05"));
}

/// F2/E50: the socket mark-price and the REST premium-index feed one mark state,
/// so a premium-index row that arrives late but is timestamped before the last
/// accepted mark must not overwrite the fresher fields. One event-time cursor
/// spans both sources: an out-of-order — or equal-timestamp — row is dropped and
/// counted, never folded. A regression that took every mark row in receipt order
/// would ship the stale price; this pins that it does not.
#[test]
fn binance_mark_state_orders_socket_and_rest_by_event_time() {
    let mut engine = VenueDerivativesEngine::new(Venue::Binance, "epoch", wall_start()).unwrap();
    let next = timestamp(100_000);
    engine.ingest(&binance_funding_info(0, 8), ns(0)).unwrap();
    // Warm the mark state over the REST premium-index poll.
    for second in 0..=60 {
        engine
            .ingest(
                &binance_premium_index(second, "100.05", "100", "0.0001", &next),
                ns(second),
            )
            .unwrap();
    }
    // A socket mark at t=61 advances the cursor to the fresher instant.
    engine
        .ingest(&binance_mark(61, "101.00", "100", "0.0001", &next), ns(61))
        .unwrap();
    // A REST premium-index row timestamped at t=60 arrives afterwards (received at
    // t=62): it is behind the last accepted mark, so it is dropped, not applied.
    engine
        .ingest(
            &binance_premium_index(60, "999.99", "100", "0.0001", &next),
            ns(62),
        )
        .unwrap();
    assert_eq!(engine.mark_out_of_order_drops(), 1);
    // A forward premium-index row at t=63 is accepted and becomes the mark.
    engine
        .ingest(
            &binance_premium_index(63, "102.55", "100", "0.0001", &next),
            ns(63),
        )
        .unwrap();
    // An equal-timestamp row (t=63 again) is the same instant's duplicate and is
    // dropped too (`<=`), never folded over the accepted value.
    engine
        .ingest(
            &binance_premium_index(63, "888.88", "100", "0.0001", &next),
            ns(64),
        )
        .unwrap();
    assert_eq!(engine.mark_out_of_order_drops(), 2);

    let updates = engine.emit(ns(64), reference()).unwrap();
    let basis = find(&updates, "btc.binance.mark_index_basis_state");
    assert_eq!(
        basis.quality.state,
        QualityState::Ready,
        "basis stays ready over the ordered mark stream, reasons {:?}",
        basis.quality.reasons
    );
    assert_eq!(
        basis.values["mark_price"],
        json!("102.55"),
        "the stale (t=60) and duplicate (t=63) rows must not overwrite the t=63 mark"
    );
}

#[test]
fn binance_funding_is_missing_until_a_funding_info_reading_then_venue_declared() {
    let mut engine = VenueDerivativesEngine::new(Venue::Binance, "epoch", wall_start()).unwrap();
    let next = timestamp(100_000);
    // 61 s of mark price, no funding-info: degrade as funding_info_missing.
    for second in 0..=60 {
        engine
            .ingest(
                &binance_mark(second, "100", "100", "0.0001", &next),
                ns(second),
            )
            .unwrap();
    }
    let missing = find(
        &engine.emit(ns(60), reference()).unwrap(),
        "btc.binance.funding_state",
    )
    .clone();
    assert_ne!(missing.quality.state, QualityState::Ready);
    assert!(
        missing
            .quality
            .reasons
            .contains(&"funding_info_missing".to_owned())
    );

    // A funding-info reading lifts it to venue_declared and ready.
    engine.ingest(&binance_funding_info(61, 8), ns(61)).unwrap();
    engine
        .ingest(&binance_mark(61, "100", "100", "0.0001", &next), ns(61))
        .unwrap();
    let declared = find(
        &engine.emit(ns(61), reference()).unwrap(),
        "btc.binance.funding_state",
    )
    .clone();
    assert_eq!(
        declared.quality.state,
        QualityState::Ready,
        "reasons {:?}",
        declared.quality.reasons
    );
    assert_eq!(declared.values["interval_basis"], json!("venue_declared"));
    assert_eq!(declared.values["annualised_rate_bps"], json!("1095"));
}

#[test]
fn binance_open_interest_delta_is_ready_from_the_rest_poll_and_mark_price() {
    let mut engine = VenueDerivativesEngine::new(Venue::Binance, "epoch", wall_start()).unwrap();
    let next = timestamp(100_000);
    engine
        .ingest(&binance_mark(0, "50000", "50000", "0.0001", &next), ns(0))
        .unwrap();
    engine
        .ingest(&binance_open_interest(0, "1000", 1), ns(0))
        .unwrap();
    engine
        .ingest(
            &binance_mark(901, "50500", "50500", "0.0001", &next),
            ns(901),
        )
        .unwrap();
    engine
        .ingest(&binance_open_interest(901, "1100", 2), ns(901))
        .unwrap();
    let oi = find(
        &engine.emit(ns(901), reference()).unwrap(),
        "btc.binance.open_interest_delta",
    )
    .clone();
    assert_eq!(
        oi.quality.state,
        QualityState::Ready,
        "reasons {:?}",
        oi.quality.reasons
    );
    assert_eq!(oi.values["delta_base_15m"], json!("100"));
    assert_eq!(oi.values["quadrant"], json!("oi_up_price_up"));
}

/// Regression (diag-oi-delta): with a 5 s open-interest poll (Binance's real
/// cadence) the family must stay ready on *every* signal tick once warmed. The
/// prune margin used to be ~1 s wide, so between polls the 900 s lookback lost
/// its window-start reading and the family flapped ready <-> warming_up. Prune
/// now retains the newest reading at or before the cutoff, so 60 consecutive
/// one-second ticks past warm-up are all ready.
#[test]
fn binance_open_interest_delta_does_not_flap_with_a_five_second_poll() {
    let mut engine = VenueDerivativesEngine::new(Venue::Binance, "epoch", wall_start()).unwrap();
    let next = timestamp(100_000);
    // Ingest mark every second and OI every 5 s, emitting a decision at each
    // second from 901 s on — so prune runs at the same `now` the lookback reads,
    // exactly as the runtime interleaves ingest and decisions.
    for second in 0..=960 {
        engine
            .ingest(
                &binance_mark(second, "50000", "50000", "0.0001", &next),
                ns(second),
            )
            .unwrap();
        if second % 5 == 0 {
            engine
                .ingest(&binance_open_interest(second, "1000", second), ns(second))
                .unwrap();
        }
        if second >= 901 {
            let updates = engine.emit(ns(second), reference()).unwrap();
            let oi = find(&updates, "btc.binance.open_interest_delta");
            assert_eq!(
                oi.quality.state,
                QualityState::Ready,
                "tick {second}s must be ready, reasons {:?}",
                oi.quality.reasons
            );
        }
    }
}

/// The same guarantee at a 1 s open-interest cadence (Bybit folds OI from the
/// 1 s tickers): no flapping over 60 consecutive ticks once warmed.
#[test]
fn bybit_open_interest_delta_does_not_flap_with_a_one_second_cadence() {
    let mut engine = VenueDerivativesEngine::new(Venue::Bybit, "epoch", wall_start()).unwrap();
    let next = timestamp(100_000);
    for second in 0..=960 {
        engine
            .ingest(
                &bybit_ticker(
                    second,
                    1_000 + second,
                    "snapshot",
                    "50000",
                    "50000",
                    "0.0001",
                    &next,
                    "1000",
                ),
                ns(second),
            )
            .unwrap();
        if second >= 901 {
            let updates = engine.emit(ns(second), reference()).unwrap();
            let oi = find(&updates, "btc.bybit.open_interest_delta");
            assert_eq!(
                oi.quality.state,
                QualityState::Ready,
                "tick {second}s must be ready, reasons {:?}",
                oi.quality.reasons
            );
        }
    }
}

// --- Bybit liquidation side (R8 confirmed, R1 step 2) ----------------------

/// A Bybit `allLiquidation` entry as the capture-only adapter emits it: `side`
/// is the liquidated POSITION side (buy = a long was liquidated, sell = a short),
/// `size` is the executed base size, `bankruptcy_price` the position's bankruptcy
/// price. Notional is `size * bankruptcy_price`.
fn bybit_liquidation(second: u64, side: &str, size: &str, bankruptcy_price: &str) -> SourceEvent {
    source_event(
        "bybit.linear.btcusdt.liquidations",
        second,
        second,
        BTreeMap::from([
            ("instrument".to_owned(), json!("BTCUSDT")),
            ("side".to_owned(), json!(side)),
            ("size".to_owned(), json!(size)),
            ("bankruptcy_price".to_owned(), json!(bankruptcy_price)),
            ("event_time_basis".to_owned(), json!("source")),
            ("event_time".to_owned(), json!(timestamp(second))),
        ]),
    )
}

#[test]
fn bybit_liquidation_pressure_confirms_side_and_computes_skew() {
    // R1 step 2: the Bybit side convention is confirmed (buy = long liquidated),
    // so the family is ready and splits the 60 s notional by liquidated position
    // side. Two Buy (long) liquidations and one Sell (short) inside the 60 s
    // window, at reference rate 1.0 so USD == USDT:
    //   long  = 2*50000 + 1*50000 = 150000 ; short = 1*50000 = 50000
    //   side_skew = (150000 - 50000) / (150000 + 50000) = 0.5
    let mut engine = VenueDerivativesEngine::new(Venue::Bybit, "epoch", wall_start()).unwrap();
    let next = timestamp(100_000);
    for second in 0..=360 {
        engine
            .ingest(
                &bybit_ticker(
                    second,
                    1_000 + second,
                    "snapshot",
                    "100",
                    "100",
                    "0.0001",
                    &next,
                    "1000",
                ),
                ns(second),
            )
            .unwrap();
        match second {
            350 => engine
                .ingest(&bybit_liquidation(350, "buy", "2", "50000"), ns(350))
                .unwrap(),
            355 => engine
                .ingest(&bybit_liquidation(355, "buy", "1", "50000"), ns(355))
                .unwrap(),
            358 => engine
                .ingest(&bybit_liquidation(358, "sell", "1", "50000"), ns(358))
                .unwrap(),
            _ => {}
        }
    }
    let liq = find(
        &engine.emit(ns(360), reference()).unwrap(),
        "btc.bybit.liquidation_pressure",
    )
    .clone();
    assert_eq!(
        liq.quality.state,
        QualityState::Ready,
        "reasons {:?}",
        liq.quality.reasons
    );
    assert_eq!(liq.values["count_60s"], json!(3));
    assert_eq!(liq.values["long_notional_usd_60s"], json!("150000"));
    assert_eq!(liq.values["short_notional_usd_60s"], json!("50000"));
    assert_eq!(liq.values["side_skew_60s"], json!("0.5"));
}

// --- ticker ordering (spec §3) ---------------------------------------------

#[test]
fn a_ticker_delta_before_a_snapshot_is_not_applied() {
    let mut engine = VenueDerivativesEngine::new(Venue::Bybit, "epoch", wall_start()).unwrap();
    let next = timestamp(100_000);
    // A delta before any snapshot voids the fold: no mark is folded, so basis
    // stays warming even after 60 s of (dropped) deltas.
    for second in 0..=60 {
        engine
            .ingest(
                &bybit_ticker(
                    second,
                    1_000 + second,
                    "delta",
                    "100.05",
                    "100",
                    "0.0001",
                    &next,
                    "1000",
                ),
                ns(second),
            )
            .unwrap();
    }
    let basis = find(
        &engine.emit(ns(60), reference()).unwrap(),
        "btc.bybit.mark_index_basis_state",
    )
    .clone();
    assert_ne!(basis.quality.state, QualityState::Ready);
}

#[test]
fn an_out_of_order_ticker_delta_is_dropped() {
    let mut engine = VenueDerivativesEngine::new(Venue::Bybit, "epoch", wall_start()).unwrap();
    let next = timestamp(100_000);
    // Seed with a snapshot at a high cross_sequence, then a lower-sequence delta
    // that must be dropped (its mark of 200 never folds).
    engine
        .ingest(
            &bybit_ticker(
                0, 5_000, "snapshot", "100.05", "100", "0.0001", &next, "1000",
            ),
            ns(0),
        )
        .unwrap();
    engine
        .ingest(
            &bybit_ticker(1, 4_000, "delta", "200", "100", "0.0001", &next, "1000"),
            ns(1),
        )
        .unwrap();
    // Keep folding at the seeded price for the rest of the warm-up.
    for second in 2..=60 {
        engine
            .ingest(
                &bybit_ticker(
                    second,
                    5_000 + second,
                    "delta",
                    "100.05",
                    "100",
                    "0.0001",
                    &next,
                    "1000",
                ),
                ns(second),
            )
            .unwrap();
    }
    let basis = find(
        &engine.emit(ns(60), reference()).unwrap(),
        "btc.bybit.mark_index_basis_state",
    )
    .clone();
    assert_eq!(basis.quality.state, QualityState::Ready);
    // The dropped 200 mark never moved the basis off 5.
    assert_eq!(basis.values["basis_bps"], json!("5"));
}

// --- C2 perp momentum / volatility (E49) -----------------------------------

/// Drive a Bybit engine with a per-second mark price (index == mark, so basis is
/// irrelevant), ingesting a ticker and emitting a decision at each second
/// `1..=last`, and return the final decision's updates. The mark ring samples the
/// merged mark once per second off the decision clock, so `last` emits build a
/// `last`-sample contiguous ring.
fn drive_bybit_perp<F: Fn(u64) -> &'static str>(price: F, last: u64) -> Vec<SignalUpdate> {
    let mut engine = VenueDerivativesEngine::new(Venue::Bybit, "epoch", wall_start()).unwrap();
    let next = timestamp(100_000);
    let mut updates = Vec::new();
    for second in 1..=last {
        let mark = price(second);
        engine
            .ingest(
                &bybit_ticker(
                    second,
                    1_000 + second,
                    "snapshot",
                    mark,
                    mark,
                    "0.0001",
                    &next,
                    "1000",
                ),
                ns(second),
            )
            .unwrap();
        updates = engine.emit(ns(second), reference()).unwrap();
    }
    updates
}

#[test]
fn bybit_perp_is_not_ready_at_900_samples_and_ready_at_901() {
    // Warm-up boundary (spec §3): 901 contiguous one-second samples. The 900th
    // emit leaves the ring one sample short; the 901st fills it.
    let mut engine = VenueDerivativesEngine::new(Venue::Bybit, "epoch", wall_start()).unwrap();
    let next = timestamp(100_000);
    for second in 1..=900 {
        engine
            .ingest(
                &bybit_ticker(
                    second,
                    1_000 + second,
                    "snapshot",
                    "100",
                    "100",
                    "0.0001",
                    &next,
                    "1000",
                ),
                ns(second),
            )
            .unwrap();
        let updates = engine.emit(ns(second), reference()).unwrap();
        if second == 900 {
            for id in ["btc.bybit.perp_momentum", "btc.bybit.perp_volatility"] {
                let update = find(&updates, id);
                assert_ne!(
                    update.quality.state,
                    QualityState::Ready,
                    "{id}: 900 samples is one short of the warm-up"
                );
                assert!(
                    update
                        .quality
                        .reasons
                        .contains(&"bybit_warming_up".to_owned()),
                    "{id} warms up, reasons {:?}",
                    update.quality.reasons
                );
            }
        }
    }
    // The 901st contiguous sample lifts both identities to ready.
    engine
        .ingest(
            &bybit_ticker(
                901,
                1_000 + 901,
                "snapshot",
                "100",
                "100",
                "0.0001",
                &next,
                "1000",
            ),
            ns(901),
        )
        .unwrap();
    let updates = engine.emit(ns(901), reference()).unwrap();
    let mom = find(&updates, "btc.bybit.perp_momentum");
    assert_eq!(
        mom.quality.state,
        QualityState::Ready,
        "reasons {:?}",
        mom.quality.reasons
    );
    // Constant mark → every horizon return is zero and the direction is flat.
    assert_eq!(mom.values["return_bps_10s"], json!("0"));
    assert_eq!(mom.values["return_bps_1m"], json!("0"));
    assert_eq!(mom.values["return_bps_5m"], json!("0"));
    assert_eq!(mom.values["return_bps_15m"], json!("0"));
    assert_eq!(mom.values["direction_5m"], json!("flat"));
    assert_eq!(mom.values["quote_currency"], json!("usd"));
    // 0/0 volatility → ratio 1, regime stable (the micro convention).
    let vol = find(&updates, "btc.bybit.perp_volatility");
    assert_eq!(vol.quality.state, QualityState::Ready);
    assert_eq!(vol.values["volatility_bps_10s"], json!("0"));
    assert_eq!(vol.values["volatility_bps_15m"], json!("0"));
    assert_eq!(vol.values["short_long_ratio"], json!("1"));
    assert_eq!(vol.values["regime"], json!("stable"));
}

#[test]
fn bybit_perp_15m_return_and_rms_are_hand_checked() {
    // A single 90 bps upward step at second 401 (100 → 100.9), inside the 15 m
    // window but outside the 5 m / 1 m / 10 s windows: only the 15 m horizon sees
    // it, so the shorter horizons stay zero.
    let updates = drive_bybit_perp(|s| if s >= 401 { "100.9" } else { "100" }, 901);
    let mom = find(&updates, "btc.bybit.perp_momentum");
    assert_eq!(mom.quality.state, QualityState::Ready);
    // 10000 * (100.9 - 100) / 100 = 90 bps over 15 m; the shorter horizons see a
    // constant 100.9 and return zero. direction_5m is flat (five-minute return 0).
    assert_eq!(mom.values["return_bps_15m"], json!("90"));
    assert_eq!(mom.values["return_bps_5m"], json!("0"));
    assert_eq!(mom.values["return_bps_1m"], json!("0"));
    assert_eq!(mom.values["return_bps_10s"], json!("0"));
    assert_eq!(mom.values["direction_5m"], json!("flat"));
    let vol = find(&updates, "btc.bybit.perp_volatility");
    assert_eq!(vol.quality.state, QualityState::Ready);
    // One 90 bps one-second return among the 900 in the 15 m window:
    // RMS = sqrt(90^2 / 900) = sqrt(9) = 3. The shorter windows contain only zeros.
    assert_eq!(vol.values["volatility_bps_15m"], json!("3"));
    assert_eq!(vol.values["volatility_bps_5m"], json!("0"));
    assert_eq!(vol.values["volatility_bps_1m"], json!("0"));
    assert_eq!(vol.values["volatility_bps_10s"], json!("0"));
    // short/long = 10 s (0) / 15 m (3) = 0 → contracting.
    assert_eq!(vol.values["short_long_ratio"], json!("0"));
    assert_eq!(vol.values["regime"], json!("contracting"));
}

#[test]
fn bybit_perp_one_minute_return_is_hand_checked() {
    // A 100 bps upward step at second 871 (100 → 101), inside the 1 m window at
    // T = 901: the 1 m / 5 m / 15 m returns all see 100 bps, the 10 s window does
    // not (the step is more than 10 s old), and the direction is up.
    let updates = drive_bybit_perp(|s| if s >= 871 { "101" } else { "100" }, 901);
    let mom = find(&updates, "btc.bybit.perp_momentum");
    assert_eq!(mom.quality.state, QualityState::Ready);
    // 10000 * (101 - 100) / 100 = 100 bps.
    assert_eq!(mom.values["return_bps_1m"], json!("100"));
    assert_eq!(mom.values["return_bps_5m"], json!("100"));
    assert_eq!(mom.values["return_bps_15m"], json!("100"));
    assert_eq!(mom.values["return_bps_10s"], json!("0"));
    assert_eq!(mom.values["direction_5m"], json!("up"));
}

#[test]
fn bybit_perp_continuity_break_restarts_the_warmup() {
    // Warm to ready over 901 contiguous samples, then a 12 s gap with no mark makes
    // the merged mark stale: the ring is a continuity break and clears, so the
    // identities leave ready and must re-warm from scratch (no gap bridging).
    let mut engine = VenueDerivativesEngine::new(Venue::Bybit, "epoch", wall_start()).unwrap();
    let next = timestamp(100_000);
    for second in 1..=901 {
        engine
            .ingest(
                &bybit_ticker(
                    second,
                    1_000 + second,
                    "snapshot",
                    "100",
                    "100",
                    "0.0001",
                    &next,
                    "1000",
                ),
                ns(second),
            )
            .unwrap();
        engine.emit(ns(second), reference()).unwrap();
    }
    // A decision 12 s later with no fresh mark: stale, and the ring is cleared.
    let stale = engine.emit(ns(913), reference()).unwrap();
    let mom_stale = find(&stale, "btc.bybit.perp_momentum");
    assert_ne!(mom_stale.quality.state, QualityState::Ready);
    assert!(
        mom_stale
            .quality
            .reasons
            .contains(&"bybit_stale".to_owned()),
        "reasons {:?}",
        mom_stale.quality.reasons
    );
    // Marks resume at second 914: the ring restarted, so the identity is warming
    // again (a single fresh sample), not instantly ready from the pre-gap history.
    engine
        .ingest(
            &bybit_ticker(
                914,
                1_000 + 914,
                "snapshot",
                "100",
                "100",
                "0.0001",
                &next,
                "1000",
            ),
            ns(914),
        )
        .unwrap();
    let resumed = engine.emit(ns(914), reference()).unwrap();
    let mom_resumed = find(&resumed, "btc.bybit.perp_momentum");
    assert_ne!(
        mom_resumed.quality.state,
        QualityState::Ready,
        "the warm-up restarts after a continuity break"
    );
    assert!(
        mom_resumed
            .quality
            .reasons
            .contains(&"bybit_warming_up".to_owned()),
        "reasons {:?}",
        mom_resumed.quality.reasons
    );
}

#[test]
fn binance_perp_mark_missing_is_recovery_required_and_never_reference_stale() {
    // A mark-missing decision (no socket mark-price and no REST premium-index) on
    // the REST-mark-fed Binance venue publishes the existing binance_recovery_required
    // reason (E34/E50), NOT a new reason. These two identities are reference-free,
    // so usdt_reference_stale never appears — even with an absent USDT reference.
    let mut engine = VenueDerivativesEngine::new(Venue::Binance, "epoch", wall_start()).unwrap();
    let updates = engine
        .emit(ns(10), UsdtReferenceSnapshot::absent())
        .unwrap();
    for id in ["btc.binance.perp_momentum", "btc.binance.perp_volatility"] {
        let update = find(&updates, id);
        assert_eq!(
            update.quality.reasons,
            vec!["binance_recovery_required".to_owned()],
            "{id} is honestly unavailable on a missing mark"
        );
        assert!(
            !update
                .quality
                .reasons
                .iter()
                .any(|reason| reason == "usdt_reference_stale"),
            "{id} is reference-free and never carries usdt_reference_stale"
        );
    }
}

#[test]
fn binance_perp_reaches_ready_over_the_rest_premium_index() {
    // The REST premium-index poll feeds the mark ring, so Binance perp
    // momentum/volatility are buildable and reach ready from Tokyo (E50/F2). Even
    // with an absent USDT reference the reference-free identities stay ready.
    let mut engine = VenueDerivativesEngine::new(Venue::Binance, "epoch", wall_start()).unwrap();
    let next = timestamp(100_000);
    let mut updates = Vec::new();
    for second in 1..=901 {
        engine
            .ingest(
                &binance_premium_index(second, "100", "100", "0.0001", &next),
                ns(second),
            )
            .unwrap();
        updates = engine
            .emit(ns(second), UsdtReferenceSnapshot::absent())
            .unwrap();
    }
    for id in ["btc.binance.perp_momentum", "btc.binance.perp_volatility"] {
        let update = find(&updates, id);
        assert_eq!(
            update.quality.state,
            QualityState::Ready,
            "{id} reaches ready over the premium-index poll, reasons {:?}",
            update.quality.reasons
        );
        assert!(
            !update.values.contains_key("usdt_usd_rate"),
            "{id} publishes no usdt_usd_rate (reference-free)"
        );
        assert_eq!(update.values["quote_currency"], json!("usd"));
    }
}

// --- family identity + definition validation (spec §5) ---------------------

macro_rules! embed {
    ($token:literal, $slug:literal) => {
        include_str!(concat!(
            "../../../definitions/btc/signals/btc.",
            $token,
            ".",
            $slug,
            "@1.0.0.json"
        ))
    };
}

// E62: the twelve families Binance produces — the shared set both venues carry.
// `liquidation_pressure` and `perp_cvd` are Bybit-only now, so they are excluded
// from the pairwise field-identity comparison below.
const FAMILY_SLUGS: [&str; 12] = [
    "mark_index_basis_state",
    "funding_state",
    "open_interest_delta",
    // C3a (E51): the technical-analysis pack on the perpetual mark series.
    "perp_atr",
    "perp_rsi",
    "perp_roc",
    "perp_ema_cross",
    "perp_macd",
    "perp_bollinger",
    "perp_zscore",
    "perp_momentum",
    "perp_volatility",
];

fn definition(token: &str, slug: &str) -> SignalDefinition {
    let raw = match (token, slug) {
        ("binance", "mark_index_basis_state") => embed!("binance", "mark_index_basis_state"),
        ("binance", "funding_state") => embed!("binance", "funding_state"),
        ("binance", "open_interest_delta") => embed!("binance", "open_interest_delta"),
        // E62: Binance produces no liquidation_pressure/perp_cvd (definitions deleted).
        ("bybit", "mark_index_basis_state") => embed!("bybit", "mark_index_basis_state"),
        ("bybit", "funding_state") => embed!("bybit", "funding_state"),
        ("bybit", "open_interest_delta") => embed!("bybit", "open_interest_delta"),
        ("bybit", "liquidation_pressure") => embed!("bybit", "liquidation_pressure"),
        ("bybit", "perp_cvd") => embed!("bybit", "perp_cvd"),
        ("binance", "perp_atr") => embed!("binance", "perp_atr"),
        ("binance", "perp_rsi") => embed!("binance", "perp_rsi"),
        ("binance", "perp_roc") => embed!("binance", "perp_roc"),
        ("binance", "perp_ema_cross") => embed!("binance", "perp_ema_cross"),
        ("binance", "perp_macd") => embed!("binance", "perp_macd"),
        ("binance", "perp_bollinger") => embed!("binance", "perp_bollinger"),
        ("binance", "perp_zscore") => embed!("binance", "perp_zscore"),
        ("bybit", "perp_atr") => embed!("bybit", "perp_atr"),
        ("bybit", "perp_rsi") => embed!("bybit", "perp_rsi"),
        ("bybit", "perp_roc") => embed!("bybit", "perp_roc"),
        ("bybit", "perp_ema_cross") => embed!("bybit", "perp_ema_cross"),
        ("bybit", "perp_macd") => embed!("bybit", "perp_macd"),
        ("bybit", "perp_bollinger") => embed!("bybit", "perp_bollinger"),
        ("bybit", "perp_zscore") => embed!("bybit", "perp_zscore"),
        ("binance", "perp_momentum") => embed!("binance", "perp_momentum"),
        ("binance", "perp_volatility") => embed!("binance", "perp_volatility"),
        ("bybit", "perp_momentum") => embed!("bybit", "perp_momentum"),
        ("bybit", "perp_volatility") => embed!("bybit", "perp_volatility"),
        other => panic!("no embed for {other:?}"),
    };
    serde_json::from_str(raw).expect("definition parses")
}

#[test]
fn the_definitions_are_pairwise_field_identical_per_family() {
    for slug in FAMILY_SLUGS {
        let binance = definition("binance", slug);
        let bybit = definition("bybit", slug);
        assert_eq!(
            binance.event_time_field, bybit.event_time_field,
            "{slug}: event_time_field identical"
        );
        let binance_fields: Vec<_> = binance
            .fields
            .iter()
            .map(|f| (&f.name, &f.field_type, &f.location, &f.variants))
            .collect();
        let bybit_fields: Vec<_> = bybit
            .fields
            .iter()
            .map(|f| (&f.name, &f.field_type, &f.location, &f.variants))
            .collect();
        assert_eq!(
            binance_fields, bybit_fields,
            "{slug}: field lists identical"
        );
    }
}

#[test]
fn every_ready_frame_validates_against_its_embedded_definition() {
    let mut engine = VenueDerivativesEngine::new(Venue::Bybit, "epoch", wall_start()).unwrap();
    let next = timestamp(100_000);
    for second in 0..=901 {
        engine
            .ingest(
                &bybit_ticker(
                    second,
                    1_000 + second,
                    "snapshot",
                    "100.05",
                    "100",
                    "0.0001",
                    &next,
                    "1000",
                ),
                ns(second),
            )
            .unwrap();
    }
    let updates = engine.emit(ns(901), reference()).unwrap();
    for update in &updates {
        let slug = update.signal_id.rsplit('.').next().unwrap();
        let def = definition("bybit", slug);
        update
            .validate_against(&def)
            .unwrap_or_else(|e| panic!("{} validates: {e}", update.signal_id));
    }
}

// --- checkpoint round-trip + replay determinism ----------------------------

#[test]
fn checkpoint_round_trips_and_restarts_sequences() {
    let mut engine = VenueDerivativesEngine::new(Venue::Bybit, "epoch", wall_start()).unwrap();
    let next = timestamp(100_000);
    for second in 0..=60 {
        engine
            .ingest(
                &bybit_ticker(
                    second,
                    1_000 + second,
                    "snapshot",
                    "100.05",
                    "100",
                    "0.0001",
                    &next,
                    "1000",
                ),
                ns(second),
            )
            .unwrap();
    }
    engine.emit(ns(60), reference()).unwrap();
    let features = engine.checkpoint_features();
    // Bybit produces all fourteen derivatives families (E62).
    assert_eq!(features.len(), 14);

    let mut resumed = VenueDerivativesEngine::new(Venue::Bybit, "epoch-2", wall_start()).unwrap();
    resumed.restore_features(&features).unwrap();
    let resumed_features = resumed.checkpoint_features();
    for (id, feature) in &features {
        let after = &resumed_features[id];
        assert_eq!(
            after.lifetime_output_count, feature.lifetime_output_count,
            "{id}"
        );
        assert_eq!(after.last_quality, feature.last_quality, "{id}");
        // A resumed engine restarts its per-run sequence at zero.
        assert_eq!(after.previous_next_sequence, 0, "{id}");
    }
}

#[test]
fn replay_is_byte_identical_across_two_independent_runs() {
    let run = || -> Vec<String> {
        let mut engine = VenueDerivativesEngine::new(Venue::Bybit, "replay", wall_start()).unwrap();
        let next = timestamp(100_000);
        let mut frames = Vec::new();
        for second in 0..=120 {
            engine
                .ingest(
                    &bybit_ticker(
                        second,
                        1_000 + second,
                        "snapshot",
                        "100.05",
                        "100",
                        "0.0001",
                        &next,
                        "1000",
                    ),
                    ns(second),
                )
                .unwrap();
            for update in engine.emit(ns(second), reference()).unwrap() {
                frames.push(serde_json::to_string(&update).unwrap());
            }
        }
        frames
    };
    assert_eq!(
        run(),
        run(),
        "the derivatives engine replays deterministically"
    );
}

// --- C3a technical analysis on the perpetual mark series --------------------

/// Long enough for every C3a technical-analysis window to close on the mark
/// series: MACD needs 26 + 9 one-minute bars (2 100 s) and the Z-score needs its
/// fifteen-minute sample ring.
const TA_WARM_SECONDS: u64 = 2_160;

/// Each perp technical-analysis identity and the price-level fields E47 converts
/// USDT→USD on it (`deriv_quote_fields`). An empty list means the identity
/// publishes no quote-denominated field, so its whole value map must be
/// byte-identical at any reference rate.
const PERP_TA_PRICE_FIELDS: [(&str, &[&str]); 7] = [
    ("perp_atr", &["atr", "close"]),
    ("perp_rsi", &[]),
    ("perp_roc", &[]),
    ("perp_ema_cross", &["ema_9", "ema_21"]),
    ("perp_macd", &[]),
    ("perp_bollinger", &[]),
    ("perp_zscore", &[]),
];

/// Drive the Bybit derivatives engine through a full TA warm-up on a moving mark
/// price — a flat series publishes `flat_window` rather than a value and would
/// never reach the ready path — and emit at the given USDT/USD rate.
fn bybit_perp_ta_run(rate: rust_decimal::Decimal) -> Vec<SignalUpdate> {
    let mut engine = VenueDerivativesEngine::new(Venue::Bybit, "epoch", wall_start()).unwrap();
    let next = timestamp(100_000);
    let mut last = Vec::new();
    for second in 0..=TA_WARM_SECONDS {
        let mark = format!("100.{:02}", second % 97 + 1);
        engine
            .ingest(
                &bybit_ticker(
                    second,
                    1_000 + second,
                    "snapshot",
                    &mark,
                    "100",
                    "0.0001",
                    &next,
                    "1000",
                ),
                ns(second),
            )
            .unwrap();
        // The mark bar series is folded on the decision, so the engine is
        // emitted every second exactly as the lane shell drives it.
        last = engine
            .emit(ns(second), UsdtReferenceSnapshot::fresh(rate))
            .unwrap();
    }
    last
}

/// C3a + E47 on the perpetual mark scope: every technical-analysis identity
/// reaches Ready after a full warm-up, validates against its embedded
/// definition, and converts exactly the price levels the E47 table declares —
/// checked against a parity replay of the identical mark series, so a rename in
/// `deriv_quote_fields` cannot pass silently.
#[test]
fn bybit_perp_ta_reaches_ready_and_converts_mark_price_levels() {
    let rate = rust_decimal::Decimal::new(105, 2);
    let parity = bybit_perp_ta_run(rust_decimal::Decimal::ONE);
    let scaled = bybit_perp_ta_run(rate);
    for (slug, price_fields) in PERP_TA_PRICE_FIELDS {
        let signal_id = format!("btc.bybit.{slug}");
        let before = find(&parity, &signal_id);
        let after = find(&scaled, &signal_id);
        after
            .validate_against(&definition("bybit", slug))
            .unwrap_or_else(|error| panic!("{signal_id} validates: {error}"));
        assert_eq!(
            before.quality.state,
            QualityState::Ready,
            "{signal_id}: ready on the parity run, reasons {:?}",
            before.quality.reasons
        );
        assert_eq!(
            after.quality.state,
            QualityState::Ready,
            "{signal_id}: ready on the non-parity run, reasons {:?}",
            after.quality.reasons
        );
        for field in price_fields {
            let level = decimal_field(before, field);
            assert_ne!(
                level,
                rust_decimal::Decimal::ZERO,
                "{signal_id}.{field}: a zero level would make the conversion unobservable"
            );
            assert_eq!(
                decimal_field(after, field),
                (level * rate)
                    .round_dp_with_strategy(8, rust_decimal::RoundingStrategy::MidpointNearestEven),
                "{signal_id}.{field}: converts USDT→USD at the reference rate"
            );
        }
        for (key, value) in &before.values {
            if price_fields.contains(&key.as_str())
                || key == "usdt_usd_rate"
                || key == "usdt_usd_rate_age_ms"
            {
                continue;
            }
            assert_eq!(
                after.values.get(key),
                Some(value),
                "{signal_id}.{key}: scale-free, must not move with the reference rate"
            );
        }
        assert_eq!(
            before.values.len(),
            after.values.len(),
            "{signal_id}: both runs publish the same field set"
        );
        if price_fields.is_empty() {
            assert!(
                !after.values.contains_key("usdt_usd_rate"),
                "{signal_id}: a non-converting identity publishes no rate"
            );
        } else {
            assert_eq!(
                after.values.get("usdt_usd_rate"),
                Some(&json!("1.05")),
                "{signal_id}: a converting identity publishes the rate it used"
            );
        }
    }
}

fn decimal_field(update: &SignalUpdate, field: &str) -> rust_decimal::Decimal {
    update
        .values
        .get(field)
        .unwrap_or_else(|| {
            panic!(
                "{}: declared price field {field} is published",
                update.signal_id
            )
        })
        .as_str()
        .unwrap_or_else(|| panic!("{}.{field} is a decimal string", update.signal_id))
        .parse()
        .unwrap_or_else(|_| panic!("{}.{field} parses as a decimal", update.signal_id))
}
#[test]
fn bybit_perp_holds_a_sub_hertz_mark_and_reaches_ready() {
    // E59: the one-second sampler holds the latest mark row for up to 3 s, so a
    // sub-1 Hz cadence — the ~1.3 s median premium-index poll, or a Bybit ticker
    // whose markPrice updates slower than once a second — still supplies a sample
    // every decision. Here mark rows land every 1.3 s while decisions tick every
    // second; the strictly-new-row rule this replaces would clear the ring on
    // every second without a fresh row and never warm. The first row lands at
    // 1.3 s, so the first sample is at decision 2 and the 901st at decision 902.
    const ROW_SPACING_NS: u64 = 1_300_000_000;
    let mut engine = VenueDerivativesEngine::new(Venue::Bybit, "epoch", wall_start()).unwrap();
    let next = timestamp(100_000);
    let mut row_index: u64 = 0;
    let mut next_row_ns: u64 = ROW_SPACING_NS;
    let mut updates = Vec::new();
    for decision in 1..=902 {
        let decision_ns = decision * ONE_SECOND_NS;
        while next_row_ns <= decision_ns {
            row_index += 1;
            engine
                .ingest(
                    &bybit_ticker(
                        row_index,
                        1_000 + row_index,
                        "snapshot",
                        "100",
                        "100",
                        "0.0001",
                        &next,
                        "1000",
                    ),
                    next_row_ns,
                )
                .unwrap();
            next_row_ns += ROW_SPACING_NS;
        }
        updates = engine.emit(decision_ns, reference()).unwrap();
        if decision == 901 {
            for id in ["btc.bybit.perp_momentum", "btc.bybit.perp_volatility"] {
                assert_ne!(
                    find(&updates, id).quality.state,
                    QualityState::Ready,
                    "{id}: 900 held samples is one short of the warm-up"
                );
            }
        }
    }
    for id in ["btc.bybit.perp_momentum", "btc.bybit.perp_volatility"] {
        assert_eq!(
            find(&updates, id).quality.state,
            QualityState::Ready,
            "{id}: 901 contiguous held samples over a 1.3 s mark cadence warms it"
        );
    }
}

#[test]
fn bybit_perp_holds_a_mark_two_seconds_then_rewarms_after_a_five_second_gap() {
    // E59: with a fresh mark row on each of 901 decision seconds the ring is ready.
    // If rows then stop, the last row is held for two more decisions (age 1 s and
    // 2 s, both inside the 3 s bound) and dropped at 3 s, so a held mark is
    // repeated for at most two seconds and never fabricates zero returns across a
    // real gap. A drought past the bound clears the ring, and a five-second gap
    // leaves the identities warming again from scratch.
    let mut engine = VenueDerivativesEngine::new(Venue::Bybit, "epoch", wall_start()).unwrap();
    let next = timestamp(100_000);
    let ticker = |second: u64| {
        bybit_ticker(
            second,
            1_000 + second,
            "snapshot",
            "100",
            "100",
            "0.0001",
            &next,
            "1000",
        )
    };
    for second in 1..=901 {
        engine.ingest(&ticker(second), ns(second)).unwrap();
        engine.emit(ns(second), reference()).unwrap();
    }
    // The last row is at 901 s. Decision 902: mark 1 s old, held → still ready.
    let held_one = engine.emit(ns(902), reference()).unwrap();
    // Decision 903: mark 2 s old, held → still ready.
    let held_two = engine.emit(ns(903), reference()).unwrap();
    // Decision 904: mark exactly 3 s old — no longer held, the sample is missing
    // and the ring clears, so the identities leave ready (fabricated repeats are
    // bounded to two seconds).
    let dropped = engine.emit(ns(904), reference()).unwrap();
    for id in ["btc.bybit.perp_momentum", "btc.bybit.perp_volatility"] {
        assert_eq!(
            find(&held_one, id).quality.state,
            QualityState::Ready,
            "{id}: held one second"
        );
        assert_eq!(
            find(&held_two, id).quality.state,
            QualityState::Ready,
            "{id}: held two seconds"
        );
        assert_ne!(
            find(&dropped, id).quality.state,
            QualityState::Ready,
            "{id}: dropped at the 3 s bound"
        );
    }
    // Continue the drought to a full five-second gap; still not ready.
    for second in 905..=906 {
        let updates = engine.emit(ns(second), reference()).unwrap();
        for id in ["btc.bybit.perp_momentum", "btc.bybit.perp_volatility"] {
            assert_ne!(find(&updates, id).quality.state, QualityState::Ready);
        }
    }
    // Rows resume at second 907: the ring restarted, so a single fresh sample is
    // warming, not instantly ready from the pre-gap history.
    engine.ingest(&ticker(907), ns(907)).unwrap();
    let resumed = engine.emit(ns(907), reference()).unwrap();
    for id in ["btc.bybit.perp_momentum", "btc.bybit.perp_volatility"] {
        let update = find(&resumed, id);
        assert_ne!(
            update.quality.state,
            QualityState::Ready,
            "{id}: the warm-up restarts after the gap"
        );
        assert!(
            update
                .quality
                .reasons
                .contains(&"bybit_warming_up".to_owned()),
            "{id} reasons {:?}",
            update.quality.reasons
        );
    }
}

// ===== E62 Bybit derivatives byte-identity guard =====

/// A Bybit `publicTrade` event carrying every declared field, for the perp_cvd
/// path in the byte-identity guard below.
fn bybit_trade(second: u64, aggressor: &str, price: &str, size: &str) -> SourceEvent {
    source_event(
        "bybit.linear.btcusdt.trades",
        second,
        second,
        BTreeMap::from([
            ("instrument".to_owned(), json!("BTCUSDT")),
            ("trade_id".to_owned(), json!(format!("t{second}"))),
            ("price".to_owned(), json!(price)),
            ("size".to_owned(), json!(size)),
            ("aggressor_side".to_owned(), json!(aggressor)),
            ("event_time_basis".to_owned(), json!("source")),
            ("block_trade".to_owned(), json!(false)),
            ("tick_direction".to_owned(), json!("plus_tick")),
            ("rpi".to_owned(), json!(false)),
            ("cross_sequence".to_owned(), json!(second)),
            ("event_time".to_owned(), json!(timestamp(second))),
        ]),
    )
}

/// The deterministic Bybit derivatives sequence whose emitted frames are pinned
/// by the golden below. It drives one-second tickers to 2160 s (a mark that drifts
/// up each minute against a flat index, so the basis, momentum, volatility and TA
/// families see real variance), a trade every second (perp CVD), a per-second
/// open-interest reading, three liquidations inside the final 60 s window, and a
/// decision emit on every tick (the perp mark-series ring samples on each
/// decision). By 2160 s every warm-up is satisfied — open interest (900 s),
/// liquidation (300 s) and the slowest TA family, MACD's 26-bar slow EMA plus its
/// 9-bar signal EMA over one-minute bars — so all fourteen families emit READY
/// frames carrying computed values, not just warming envelopes.
fn bybit_derivatives_golden_frames() -> Vec<String> {
    let mut engine = VenueDerivativesEngine::new(Venue::Bybit, "e62-golden", wall_start()).unwrap();
    let next = timestamp(100_000);
    // Run long enough for the slowest warm-up (MACD's 26-bar slow EMA plus its
    // 9-bar signal EMA over one-minute bars) to complete.
    const END: u64 = 2_160;
    let mut last = Vec::new();
    for second in 0..=END {
        // A per-minute upward drift ((second / 60) * 10) gives each closed bar a
        // different close, while the intra-minute term (second % 20) gives the
        // bars a real high/low range, so the TA families compute non-trivial
        // values rather than degenerate zeros.
        let mark = format!("{}", 50_000 + (second / 60) * 10 + second % 20);
        let open_interest = format!("{}", 1_000 + second % 10);
        engine
            .ingest(
                &bybit_ticker(
                    second,
                    1_000 + second,
                    "snapshot",
                    &mark,
                    "50000",
                    "0.0001",
                    &next,
                    &open_interest,
                ),
                ns(second),
            )
            .unwrap();
        engine
            .ingest(
                &bybit_trade(
                    second,
                    if second % 2 == 0 { "buy" } else { "sell" },
                    &mark,
                    "1",
                ),
                ns(second),
            )
            .unwrap();
        // Three liquidations inside the final 60 s window so liquidation_pressure
        // is ready with a non-empty notional split.
        match second {
            s if s == END - 55 => engine
                .ingest(&bybit_liquidation(second, "buy", "2", "50000"), ns(second))
                .unwrap(),
            s if s == END - 30 => engine
                .ingest(&bybit_liquidation(second, "buy", "1", "50000"), ns(second))
                .unwrap(),
            s if s == END - 5 => engine
                .ingest(&bybit_liquidation(second, "sell", "1", "50000"), ns(second))
                .unwrap(),
            _ => {}
        }
        // Emit on every one-second tick: the perp mark-series ring (momentum,
        // volatility and the TA families) samples the mark on each decision, so a
        // single terminal emit would leave them warming. The final batch is the
        // golden.
        last = engine.emit(ns(second), reference()).unwrap();
    }
    let mut frames: Vec<String> = last
        .iter()
        .map(|u| serde_json::to_string(u).expect("update serializes"))
        .collect();
    frames.sort();
    frames
}

/// Bybit's derivatives frames are byte-identical to the pre-E62 tree
/// (`signals-prototype-wave3`): the E62 refactor removed only Binance's
/// `market`-connection families and left every Bybit code path untouched. The
/// golden was regenerated from THIS worktree over the exact sequence in
/// [`bybit_derivatives_golden_frames`], and because that Bybit code is unchanged
/// from pre-E62 the frames are byte-identical to what the pre-edit tree emits. It
/// pins READY frames with computed values across all fourteen families, so any
/// drift in a Bybit derivatives value — not just its envelope — fails here.
#[test]
fn bybit_derivatives_frames_are_byte_identical_to_the_pre_e62_golden() {
    let golden: Vec<String> = include_str!("support/bybit_derivatives_golden.jsonl")
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(
        golden.len(),
        14,
        "the golden pins all fourteen Bybit families"
    );
    let frames = bybit_derivatives_golden_frames();
    assert_eq!(
        frames, golden,
        "Bybit derivatives frames must be byte-identical to the pre-E62 tree"
    );
}
