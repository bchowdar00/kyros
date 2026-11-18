use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

use btc_signals::{
    ABSORPTION_STATE_ID, ACTIONABLE_DEPTH_IMBALANCE_ID, AGGRESSOR_FLOW_ID,
    CROSS_VENUE_DISPERSION_ID, EARLY_MOVE_FADE_CONTEXT_ID, LIQUIDITY_SHOCK_ID,
    ORDER_FLOW_IMBALANCE_ID, REALIZED_VOLATILITY_ID, SPOT_MICROPRICE_ID, SPOT_SIGNAL_SET_COUNT,
    SpotSignalError, SpotSignalProcessor, UnavailableReason, VENUE_SPOT_SIGNAL_COUNT,
    VENUE_SPOT_VENUES, VWAP_DEVIATION_ID, Venue, prediction_signal_contract_documents,
    spot_signal_contract_documents, venue_spot_signal_ids,
};
use chrono::{Duration, SecondsFormat};
use serde_json::{Value, json};
use signals_core::{
    LifecycleStatus, QualityState, SignalDefinition, SignalUpdate, SourceEvent, Validate,
};

mod support;

use crate::support::wall_start;

const ONE_SECOND_NS: u64 = 1_000_000_000;

#[test]
fn every_active_definition_has_an_embedded_processor_contract() {
    let definitions_dir =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../definitions/btc/signals");
    let mut active = BTreeSet::new();
    for entry in fs::read_dir(definitions_dir).expect("read signal definitions") {
        let path = entry.expect("read definition entry").path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let definition: SignalDefinition = serde_json::from_slice(
            &fs::read(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display())),
        )
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        if definition.status != LifecycleStatus::Deprecated {
            active.insert((definition.signal_id, definition.version));
        }
    }

    // Definitions staged ahead of their processors: the CF oracle reference and
    // the two Kalshi settlement identities publish from the Kalshi prediction lane
    // in B3 brief (B); `btc.pyth.oracle_reference` left this list in push B once
    // the Hermes-fed processor landed (E56 as amended). They live in the public tree per
    // the SIG-039 / R7 refuse-closed precedent, and are listed here so this guard
    // stays exact rather than silently admitting an unimplemented identity.
    // `btc.kalshi.window_streak` (C3b, ruling E51) joins them: it has a
    // processor, but it is index-gated with the settlement twins whose final
    // decisions it folds, so like them it is in the ARMED contract set rather
    // than the disarmed baseline this guard walks.
    let pending_processor: BTreeSet<(String, String)> = [
        ("btc.cfbrti.oracle_reference", "1.0.0"),
        ("btc.kalshi.settlement_anchor_state", "1.0.0"),
        ("btc.kalshi.oracle_divergence", "1.0.0"),
        ("btc.kalshi.window_streak", "1.0.0"),
        // C2 (E49/E56): the two Pyth momentum/volatility identities ship
        // refuse-closed with NO processor (the oracle reference itself is now
        // Hermes-fed) — their processor arrives when Pyth Hermes drives a ring
        // input (slice C4). Listed here so this guard stays exact.
        ("btc.pyth.price_momentum", "1.0.0"),
        ("btc.pyth.price_volatility", "1.0.0"),
        // C1 (wave3-c1): the multi/cross identities were approved on disk staged
        // ahead of their embedded processors, and are removed as each lands, exactly
        // as B3 staged its oracle/settlement identities. The push-B cross-lane port
        // landed the three `btc.cross.*` processors; the multi runtime port (phase 3,
        // E57) then landed the two prediction blends with their embedded
        // MultiProcessor. `btc.multi.oracle_dispersion` alone stays staged: its
        // dispersion reading arrives when Pyth Hermes joins the oracle lane (slice C4).
        ("btc.multi.oracle_dispersion", "1.0.0"),
    ]
    .into_iter()
    .map(|(id, version)| (id.to_owned(), version.to_owned()))
    .collect();
    let active_implemented: BTreeSet<(String, String)> =
        active.difference(&pending_processor).cloned().collect();

    let embedded = spot_signal_contract_documents()
        .into_iter()
        .chain(prediction_signal_contract_documents())
        .chain(btc_signals::oracle_signals::oracle_signal_contract_documents())
        .chain(btc_signals::venue_spot_signals::venue_spot_signal_contract_documents())
        .chain(
            btc_signals::venue_derivatives_signals::venue_derivatives_signal_contract_documents(),
        )
        .chain(btc_signals::cross_signals::cross_signal_contract_documents())
        .chain(btc_signals::multi_signals::multi_signal_contract_documents())
        .filter_map(|(id, raw)| {
            let value: Value =
                serde_json::from_str(raw).unwrap_or_else(|error| panic!("{id}: {error}"));
            value.get("signal_id")?;
            let definition: SignalDefinition =
                serde_json::from_str(raw).unwrap_or_else(|error| panic!("{id}: {error}"));
            Some((definition.signal_id, definition.version))
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(active_implemented, embedded);
}

#[test]
fn embedded_definitions_cover_the_buildable_set_and_validate() {
    let processor = processor("definitions");
    let definitions: BTreeMap<_, _> = processor
        .definitions()
        .map(|definition| (definition.signal_id.as_str(), definition))
        .collect();
    let mut expected = BTreeSet::from([
        "btc.external.absorption_state",
        "btc.external.actionable_depth_imbalance",
        "btc.external.aggressor_flow",
        "btc.external.cross_venue_dispersion",
        "btc.external.early_move_fade_context",
        "btc.external.horizon_momentum",
        "btc.external.horizon_volatility",
        "btc.external.liquidity_shock",
        "btc.external.micro_momentum",
        "btc.external.order_flow_imbalance",
        "btc.external.realized_volatility",
        "btc.external.spot_microprice",
        "btc.external.spot_reference",
        "btc.external.vwap_deviation",
        // C3a (E51).
        "btc.external.atr",
        "btc.external.bollinger",
        "btc.external.ema_cross",
        "btc.external.macd",
        "btc.external.roc",
        "btc.external.rsi",
        "btc.external.zscore",
    ]);
    // The per-venue identity lanes (ADR 0024): nine single-venue readings for
    // each adopted spot venue, published alongside the blended set.
    for venue in VENUE_SPOT_VENUES {
        let ids = venue_spot_signal_ids(venue);
        assert_eq!(ids.len(), VENUE_SPOT_SIGNAL_COUNT);
        expected.extend(ids);
    }
    assert_eq!(expected.len(), SPOT_SIGNAL_SET_COUNT);
    assert_eq!(
        definitions.keys().copied().collect::<BTreeSet<_>>(),
        expected
    );
    for definition in definitions.values() {
        definition
            .validate()
            .expect("embedded definition validates");
        assert_eq!(definition.approximate_interval_ms, 1_000);
        assert_eq!(definition.staleness_bound_ms, 10_000);
    }
}

#[test]
fn sixty_one_ticks_make_every_buildable_signal_ready_and_contract_valid() {
    let mut processor = processor("complete-set");
    let mut final_tick = Vec::new();
    for second in 1..=61 {
        admit_second(&mut processor, second, true);
        final_tick = processor
            .advance_time(ns(second))
            .expect("health tick succeeds");
    }

    assert_eq!(final_tick.len(), SPOT_SIGNAL_SET_COUNT);
    let definitions: BTreeMap<String, SignalDefinition> = processor
        .definitions()
        .map(|definition| (definition.signal_id.clone(), definition.clone()))
        .collect();
    for update in &final_tick {
        let expected = fixture_state(&update.signal_id, QualityState::Ready);
        assert_eq!(update.quality.state, expected, "{}", update.signal_id);
        update
            .validate_against(
                definitions
                    .get(&update.signal_id)
                    .expect("every output has an embedded definition"),
            )
            .expect("ready output validates against its definition");
    }

    let flow = update(&final_tick, AGGRESSOR_FLOW_ID);
    assert_eq!(flow.values["trade_count_60s"], json!(120));
    assert_eq!(flow.values["cumulative_volume_delta_base_60s"], json!("30"));
    assert_eq!(flow.values["gross_notional_usd_60s"], json!("9000.9"));
    assert_eq!(flow.values["signed_notional_usd_60s"], json!("3000.3"));

    let volatility = update(&final_tick, REALIZED_VOLATILITY_ID);
    assert_eq!(volatility.values["return_count"], json!(60));
    assert_eq!(volatility.values["volatility_bps_15s"], json!("0"));
    assert_eq!(volatility.values["volatility_bps_60s"], json!("0"));
    assert_eq!(volatility.values["short_long_ratio"], json!("1"));
    assert_eq!(volatility.values["regime"], json!("stable"));

    assert_eq!(
        update(&final_tick, LIQUIDITY_SHOCK_ID).values["state"],
        json!("normal")
    );
    assert_eq!(
        update(&final_tick, CROSS_VENUE_DISPERSION_ID).values["leading_venue"],
        json!("tied")
    );
    assert_eq!(
        update(&final_tick, SPOT_MICROPRICE_ID).values["microprice"],
        json!("100.01")
    );
    assert_eq!(
        update(&final_tick, ACTIONABLE_DEPTH_IMBALANCE_ID).values["combined_imbalance"],
        json!("0")
    );
    assert_eq!(
        update(&final_tick, ORDER_FLOW_IMBALANCE_ID).values["event_count_60s"],
        json!(120)
    );

    // The adopted fade context folds this decision's momentum and absorption:
    // the fixture's price path is flat, so the thirty-second return that drives
    // the classification (E53) is below the twenty-basis-point threshold and the
    // reading is quiet, carrying the same absorption word the absorption signal
    // published on this decision; return_bps_1s is the (also zero) one-second
    // context.
    let fade = update(&final_tick, EARLY_MOVE_FADE_CONTEXT_ID);
    let absorption = update(&final_tick, ABSORPTION_STATE_ID);
    assert_eq!(fade.values["state"], json!("quiet"));
    assert_eq!(fade.values["return_bps_1s"], json!("0"));
    assert_eq!(fade.values["direction_10s"], json!("flat"));
    assert_eq!(fade.values["big_move_bps"], json!("20"));
    assert_eq!(fade.values["absorption_state"], absorption.values["state"]);
}

#[test]
fn price_and_book_pressure_signs_follow_the_definitions() {
    let mut processor = processor("formula-signs");
    for second in 1..=61 {
        admit_second(&mut processor, second, false);
        processor
            .advance_time(ns(second))
            .expect("warm-up health tick succeeds");
    }

    for mut event in [coinbase_ticker(62), kraken_ticker(62)] {
        event.values.insert("best_bid".to_owned(), json!("101"));
        event.values.insert("best_ask".to_owned(), json!("101.02"));
        processor
            .ingest(&event, ns(62), ns(62))
            .expect("higher reference quote is admitted");
    }
    for event in [
        coinbase_book_event(122, 0, 2, "delta", "bid", "100", "3", 62),
        coinbase_book_event(123, 1, 2, "delta", "ask", "100.02", "1", 62),
        kraken_book_event(122, 0, 2, "delta", "bid", "100", "3", 62),
        kraken_book_event(123, 1, 2, "delta", "ask", "100.02", "1", 62),
    ] {
        processor
            .ingest(&event, ns(62), ns(62))
            .expect("pressure book row is admitted");
    }
    processor
        .ingest(&coinbase_trade(62, "buy", "1"), ns(62), ns(62))
        .expect("Coinbase trade is admitted");
    processor
        .ingest(
            &kraken_trade(61, 62, "update", "sell", "0.5"),
            ns(62),
            ns(62),
        )
        .expect("Kraken trade is admitted");

    let updates = processor
        .advance_time(ns(62))
        .expect("formula observation is emitted");
    assert_eq!(
        update(&updates, VWAP_DEVIATION_ID).values["deviation_bps_10s"],
        json!("99.990001")
    );
    assert_eq!(
        update(&updates, SPOT_MICROPRICE_ID).values["microprice_deviation_bps"],
        json!("0.49995")
    );
    assert_eq!(
        update(&updates, ACTIONABLE_DEPTH_IMBALANCE_ID).values["combined_imbalance"],
        json!("0.5")
    );
    assert_eq!(
        update(&updates, ORDER_FLOW_IMBALANCE_ID).values["ofi_base_1s"],
        json!("4")
    );
}

#[test]
fn partial_atomic_book_group_is_never_visible_to_a_signal() {
    let mut processor = processor("atomic-book");
    admit_second(&mut processor, 1, true);
    let first = processor
        .advance_time(ns(1))
        .expect("first health tick succeeds");
    assert_eq!(
        update(&first, SPOT_MICROPRICE_ID).quality.state,
        QualityState::Ready
    );

    processor
        .ingest(
            &coinbase_book_event(2, 0, 2, "delta", "bid", "100", "3", 2),
            ns(2),
            ns(2),
        )
        .expect("first row of a valid atomic group is admitted");
    processor
        .ingest(&coinbase_ticker(2), ns(2), ns(2))
        .expect("Coinbase ticker remains fresh");
    processor
        .ingest(&kraken_ticker(2), ns(2), ns(2))
        .expect("Kraken ticker remains fresh");
    let partial = processor
        .advance_time(ns(2))
        .expect("health tick fails closed during partial group");
    let microprice = update(&partial, SPOT_MICROPRICE_ID);
    assert_eq!(microprice.quality.state, QualityState::Degraded);
    assert_eq!(
        microprice.quality.reasons,
        ["book_snapshot_stale_or_missing"]
    );
    assert!(microprice.values.is_empty());

    processor
        .ingest(
            &coinbase_book_event(3, 1, 2, "delta", "ask", "100.02", "1", 2),
            ns(2),
            ns(2),
        )
        .expect("final row completes the atomic group");
    let complete = processor
        .advance_time(ns(2))
        .expect("completed group becomes observable");
    assert_eq!(
        update(&complete, SPOT_MICROPRICE_ID).quality.state,
        QualityState::Ready
    );
}

#[test]
fn invalid_completed_book_is_contained_and_requires_a_fresh_snapshot() {
    let mut processor = processor("invalid-book");
    for second in 1..=61 {
        admit_second(&mut processor, second, true);
        processor
            .advance_time(ns(second))
            .expect("warm-up health tick succeeds");
    }

    let invalidated = processor
        .ingest(
            &coinbase_book_event(122, 0, 1, "delta", "bid", "101", "2", 62),
            ns(62),
            ns(62),
        )
        .expect("crossed completed book is contained");
    // The fade context folds over absorption, so a book invalidation carries it
    // to the same fail-closed restatement.
    // Only the venue whose book failed restates its own identities; Kraken's
    // lane is untouched by a Coinbase book fault (ADR 0024).
    let affected = BTreeSet::from([
        SPOT_MICROPRICE_ID,
        ACTIONABLE_DEPTH_IMBALANCE_ID,
        ORDER_FLOW_IMBALANCE_ID,
        ABSORPTION_STATE_ID,
        LIQUIDITY_SHOCK_ID,
        EARLY_MOVE_FADE_CONTEXT_ID,
        "btc.coinbase.spot_microprice",
        "btc.coinbase.actionable_depth_imbalance",
        "btc.coinbase.order_flow_imbalance",
        "btc.coinbase.liquidity_shock",
        // The four microstructure identities read the Coinbase book directly,
        // so they fail closed with the rest of that venue's book-derived set.
        "btc.coinbase.effective_spread",
        "btc.coinbase.kyle_lambda_drift",
        "btc.coinbase.sweep_detection",
        "btc.coinbase.book_resilience",
    ]);
    assert_eq!(
        invalidated
            .iter()
            .map(|update| update.signal_id.as_str())
            .collect::<BTreeSet<_>>(),
        affected
    );
    assert!(
        invalidated.iter().all(|update| update.quality.state
            == fixture_state(&update.signal_id, QualityState::Degraded))
    );
    assert_eq!(
        update(&invalidated, SPOT_MICROPRICE_ID).quality.reasons,
        ["coinbase_book_invalid"]
    );
    assert_eq!(
        update(&invalidated, ORDER_FLOW_IMBALANCE_ID)
            .quality
            .reasons,
        ["book_flow_history_warming_up", "coinbase_book_invalid"]
    );
    assert_eq!(
        processor.take_book_recovery_requests(),
        BTreeSet::from([Venue::Coinbase])
    );

    let repeated = processor
        .ingest(
            &coinbase_book_event(123, 0, 1, "delta", "ask", "102", "1", 62),
            ns(62),
            ns(62),
        )
        .expect("delta while invalid remains contained");
    assert!(repeated.is_empty());
    assert_eq!(
        processor.take_book_recovery_requests(),
        BTreeSet::from([Venue::Coinbase])
    );

    let while_invalid = processor
        .advance_time(ns(62))
        .expect("invalid venue degrades only dependent signals");
    assert_eq!(
        update(&while_invalid, SPOT_MICROPRICE_ID).quality.reasons,
        ["coinbase_book_invalid"]
    );
    assert_eq!(
        update(&while_invalid, AGGRESSOR_FLOW_ID).quality.state,
        QualityState::Ready
    );
    for signal_id in affected {
        assert_eq!(
            processor.quality_reason_entries()[&format!("{signal_id}:coinbase_book_invalid")],
            1,
            "invalidity is transition-counted once for {signal_id}"
        );
    }

    for event in [
        coinbase_book_event(124, 0, 2, "snapshot", "bid", "100", "2", 63),
        coinbase_book_event(125, 1, 2, "snapshot", "ask", "100.02", "1", 63),
    ] {
        processor
            .ingest(&event, ns(63), ns(63))
            .expect("fresh replacement snapshot is admitted");
    }
    assert!(processor.take_book_recovery_requests().is_empty());
    let recovered = processor
        .advance_time(ns(63))
        .expect("replacement snapshot restores book-derived measurements");
    assert_eq!(
        update(&recovered, SPOT_MICROPRICE_ID).quality.state,
        QualityState::Ready
    );
    assert_eq!(
        update(&recovered, ORDER_FLOW_IMBALANCE_ID).quality.reasons,
        ["book_flow_history_warming_up"]
    );
}

#[test]
fn one_sided_completed_snapshot_is_contained_without_process_failure() {
    let mut processor = processor("one-sided-book");
    let invalidated = processor
        .ingest(
            &coinbase_book_event(0, 0, 1, "snapshot", "bid", "100", "1", 1),
            ns(1),
            ns(1),
        )
        .expect("one-sided completed snapshot is a venue-local invalidity");
    // Six blended restatements plus the eight Coinbase book-derived identities.
    assert_eq!(invalidated.len(), 14);
    assert!(invalidated.iter().all(|update| {
        update.quality.state == QualityState::WarmingUp
            && update
                .quality
                .reasons
                .iter()
                .any(|reason| reason == "coinbase_book_invalid")
    }));
    assert_eq!(
        processor.take_book_recovery_requests(),
        BTreeSet::from([Venue::Coinbase])
    );
}

#[test]
fn kraken_invalid_book_requests_only_kraken_recovery() {
    let mut processor = processor("kraken-invalid-book");
    admit_second(&mut processor, 1, true);

    let invalidated = processor
        .ingest(
            &kraken_book_event(2, 0, 1, "delta", "bid", "101", "2", 2),
            ns(2),
            ns(2),
        )
        .expect("crossed Kraken book is contained");
    // Six blended restatements plus the eight Kraken book-derived identities.
    assert_eq!(invalidated.len(), 14);
    assert!(invalidated.iter().all(|update| {
        update
            .quality
            .reasons
            .iter()
            .any(|reason| reason == "kraken_book_invalid")
    }));
    assert_eq!(
        processor.take_book_recovery_requests(),
        BTreeSet::from([Venue::Kraken])
    );

    processor
        .ingest(
            &coinbase_book_event(2, 0, 1, "delta", "bid", "100", "3", 2),
            ns(2),
            ns(2),
        )
        .expect("the unaffected Coinbase book continues accepting deltas");
    assert!(processor.take_book_recovery_requests().is_empty());

    for event in [
        kraken_book_event(3, 0, 2, "snapshot", "bid", "100", "1", 2),
        kraken_book_event(4, 1, 2, "snapshot", "ask", "100.02", "2", 2),
    ] {
        processor
            .ingest(&event, ns(2), ns(2))
            .expect("fresh Kraken replacement snapshot is admitted");
    }
    assert!(processor.take_book_recovery_requests().is_empty());
    let recovered = processor
        .advance_time(ns(2))
        .expect("fresh Kraken snapshot restores paired book state");
    assert_eq!(
        update(&recovered, SPOT_MICROPRICE_ID).quality.state,
        QualityState::Ready
    );
}

#[test]
fn stale_inputs_fail_closed_after_the_ten_second_bound() {
    let mut processor = processor("stale-inputs");
    let mut ready = Vec::new();
    for second in 1..=61 {
        admit_second(&mut processor, second, false);
        ready = processor
            .advance_time(ns(second))
            .expect("health tick succeeds");
    }
    assert!(ready.iter().all(
        |update| update.quality.state == fixture_state(&update.signal_id, QualityState::Ready)
    ));

    let stale = processor
        .advance_time(ns(72))
        .expect("staleness health tick succeeds");
    assert_eq!(stale.len(), SPOT_SIGNAL_SET_COUNT);
    assert!(
        stale.iter().all(|update| update.quality.state
            == fixture_state(&update.signal_id, QualityState::Degraded))
    );
    assert_eq!(
        update(&stale, SPOT_MICROPRICE_ID).quality.reasons,
        ["book_snapshot_stale_or_missing"]
    );
    assert_eq!(
        update(&stale, AGGRESSOR_FLOW_ID).quality.reasons,
        ["trade_venue_stale_or_missing"]
    );
}

#[test]
fn trade_quiet_but_alive_venue_publishes_ready_zeros() {
    let mut processor = processor("trade-quiet-alive");
    for second in 1..=61 {
        admit_second(&mut processor, second, false);
        processor
            .advance_time(ns(second))
            .expect("warm-up health tick succeeds");
    }
    // Trades stop while books and tickers keep both venues demonstrably alive
    // well past the 60-second window.
    let mut quiet = Vec::new();
    for second in 62..=125 {
        admit_books_and_tickers(&mut processor, second);
        quiet = processor
            .advance_time(ns(second))
            .expect("quiet health tick succeeds");
    }
    let flow = update(&quiet, AGGRESSOR_FLOW_ID);
    assert_eq!(flow.quality.state, QualityState::Ready);
    assert_eq!(flow.signal_version, "1.0.0");
    assert_eq!(flow.values["trade_count_60s"], json!(0));
    assert_eq!(flow.values["signed_notional_usd_60s"], json!("0"));
    assert_eq!(flow.values["signed_notional_usd_10s"], json!("0"));
    assert_eq!(flow.values["cumulative_volume_delta_base_60s"], json!("0"));
    assert_eq!(flow.values["flow_direction_10s"], json!("flat"));
}

#[test]
fn silent_venue_degrades_even_after_a_quiet_alive_window() {
    let mut processor = processor("silent-after-quiet");
    for second in 1..=61 {
        admit_second(&mut processor, second, false);
        processor
            .advance_time(ns(second))
            .expect("warm-up health tick succeeds");
    }
    for second in 62..=70 {
        admit_books_and_tickers(&mut processor, second);
        let alive = processor
            .advance_time(ns(second))
            .expect("quiet health tick succeeds");
        assert_eq!(
            update(&alive, AGGRESSOR_FLOW_ID).quality.state,
            QualityState::Ready
        );
    }
    // Every venue stream stops: past the bound the venue is blind, not quiet, so
    // the existing degradation reason returns unchanged.
    let silent = processor
        .advance_time(ns(81))
        .expect("silent health tick succeeds");
    let flow = update(&silent, AGGRESSOR_FLOW_ID);
    assert_eq!(flow.quality.state, QualityState::Degraded);
    assert_eq!(flow.quality.reasons, ["trade_venue_stale_or_missing"]);
}

#[test]
fn absorption_reports_insufficient_flow_during_a_quiet_alive_window() {
    let mut processor = processor("absorption-quiet");
    for second in 1..=61 {
        admit_second(&mut processor, second, false);
        processor
            .advance_time(ns(second))
            .expect("warm-up health tick succeeds");
    }
    let mut quiet = Vec::new();
    for second in 62..=125 {
        admit_books_and_tickers(&mut processor, second);
        quiet = processor
            .advance_time(ns(second))
            .expect("quiet health tick succeeds");
    }
    let absorption = update(&quiet, ABSORPTION_STATE_ID);
    assert_eq!(absorption.quality.state, QualityState::Ready);
    assert_eq!(absorption.signal_version, "1.0.0");
    assert_eq!(absorption.values["state"], json!("insufficient_flow"));
    assert_eq!(absorption.values["flow_direction"], json!("flat"));
    assert_eq!(absorption.values["flow_to_depth_ratio"], json!("0"));
}

#[test]
fn order_flow_imbalance_publishes_zero_on_a_quiet_alive_book() {
    let mut processor = processor("ofi-quiet-alive");
    for second in 1..=61 {
        admit_second(&mut processor, second, false);
        processor
            .advance_time(ns(second))
            .expect("warm-up health tick succeeds");
    }
    // Book events stop while trades and tickers keep both venues alive past the
    // 60-second window; the OFI of an eventless window is a truthful zero.
    let mut quiet = Vec::new();
    for second in 62..=125 {
        admit_trades_and_tickers(&mut processor, second);
        quiet = processor
            .advance_time(ns(second))
            .expect("quiet health tick succeeds");
    }
    let ofi = update(&quiet, ORDER_FLOW_IMBALANCE_ID);
    assert_eq!(ofi.quality.state, QualityState::Ready);
    assert_eq!(ofi.signal_version, "1.0.0");
    assert_eq!(ofi.values["ofi_base_60s"], json!("0"));
    assert_eq!(ofi.values["ofi_base_10s"], json!("0"));
    assert_eq!(ofi.values["event_count_60s"], json!(0));
}

#[test]
fn aggressor_flow_recovers_nonzero_when_trades_resume() {
    let mut processor = processor("aggressor-recovery");
    for second in 1..=61 {
        admit_second(&mut processor, second, false);
        processor
            .advance_time(ns(second))
            .expect("warm-up health tick succeeds");
    }
    for second in 62..=125 {
        admit_books_and_tickers(&mut processor, second);
        let quiet = processor
            .advance_time(ns(second))
            .expect("quiet health tick succeeds");
        assert_eq!(
            update(&quiet, AGGRESSOR_FLOW_ID).quality.state,
            QualityState::Ready
        );
    }
    // Trades resume with contiguous per-venue sequences (Coinbase resumes at 62,
    // Kraken at 61): ready throughout, and the windows leave zero.
    let mut resumed = Vec::new();
    for offset in 0..5 {
        let second = 126 + offset;
        admit_books_and_tickers(&mut processor, second);
        processor
            .ingest(
                &coinbase_trade(62 + offset, "buy", "1"),
                ns(second),
                ns(second),
            )
            .expect("resumed Coinbase trade is admitted");
        processor
            .ingest(
                &kraken_trade(61 + offset, second, "update", "sell", "0.5"),
                ns(second),
                ns(second),
            )
            .expect("resumed Kraken trade is admitted");
        resumed = processor
            .advance_time(ns(second))
            .expect("resumed health tick succeeds");
        assert_eq!(
            update(&resumed, AGGRESSOR_FLOW_ID).quality.state,
            QualityState::Ready
        );
    }
    let flow = update(&resumed, AGGRESSOR_FLOW_ID);
    assert_ne!(flow.values["trade_count_60s"], json!(0));
    assert_ne!(flow.values["signed_notional_usd_10s"], json!("0"));
}

#[test]
fn missed_health_observation_restarts_one_second_history() {
    let mut processor = processor("missed-health-observation");
    for second in 1..=61 {
        admit_second(&mut processor, second, false);
        processor
            .advance_time(ns(second))
            .expect("health tick succeeds");
    }

    let after_gap = processor
        .advance_time(ns(63))
        .expect("post-gap health tick succeeds");
    assert_eq!(
        update(&after_gap, LIQUIDITY_SHOCK_ID).quality.reasons,
        ["book_history_warming_up"]
    );
    assert_eq!(
        update(&after_gap, REALIZED_VOLATILITY_ID).quality.reasons,
        ["reference_history_warming_up"]
    );
}

#[test]
fn rolling_windows_warm_from_observation_not_process_start() {
    let mut processor = processor("late-observation");
    let mut before_full_window = Vec::new();
    for second in 31..=90 {
        admit_offset_second(&mut processor, second, second - 31);
        before_full_window = processor
            .advance_time(ns(second))
            .expect("health tick succeeds");
    }

    assert_eq!(
        update(&before_full_window, AGGRESSOR_FLOW_ID)
            .quality
            .reasons,
        ["trade_history_warming_up"]
    );
    assert_eq!(
        update(&before_full_window, ORDER_FLOW_IMBALANCE_ID)
            .quality
            .reasons,
        ["book_flow_history_warming_up"]
    );

    admit_offset_second(&mut processor, 91, 60);
    let full_window = processor
        .advance_time(ns(91))
        .expect("sixty-second observation window completes");
    assert_eq!(
        update(&full_window, AGGRESSOR_FLOW_ID).quality.state,
        QualityState::Ready
    );
    assert_eq!(
        update(&full_window, ORDER_FLOW_IMBALANCE_ID).quality.state,
        QualityState::Ready
    );
}

#[test]
fn deltas_before_snapshots_and_source_sequence_gaps_stop_processing() {
    let mut before_snapshot = processor("delta-before-snapshot");
    assert!(matches!(
        before_snapshot.ingest(
            &coinbase_book_event(0, 0, 1, "delta", "bid", "100", "1", 1),
            ns(1),
            ns(1),
        ),
        Err(SpotSignalError::BookGroup(_))
    ));

    let mut book_gap = processor("book-gap");
    for event in [
        coinbase_book_event(0, 0, 2, "snapshot", "bid", "100", "1", 1),
        coinbase_book_event(1, 1, 2, "snapshot", "ask", "100.02", "1", 1),
    ] {
        book_gap
            .ingest(&event, ns(1), ns(1))
            .expect("snapshot row is admitted");
    }
    assert!(matches!(
        book_gap.ingest(
            &coinbase_book_event(3, 0, 1, "delta", "bid", "100", "2", 2),
            ns(2),
            ns(2),
        ),
        Err(SpotSignalError::Ordering(_))
    ));

    let mut trade_gap = processor("trade-gap");
    trade_gap
        .ingest(&coinbase_trade(1, "buy", "1"), ns(1), ns(1))
        .expect("first trade establishes ordering");
    assert!(matches!(
        trade_gap.ingest(&coinbase_trade(3, "buy", "1"), ns(3), ns(3)),
        Err(SpotSignalError::Ordering(_))
    ));

    let mut wrong_depth = kraken_book_event(0, 0, 1, "snapshot", "bid", "100", "1", 1);
    wrong_depth
        .values
        .insert("configured_depth".to_owned(), json!(10));
    assert!(matches!(
        processor("wrong-depth").ingest(&wrong_depth, ns(1), ns(1)),
        Err(SpotSignalError::MarketValue(_))
    ));
}

#[test]
fn cross_venue_receipt_inversion_is_valid_for_flow_and_ofi() {
    let mut processor = processor("cross-venue-receipts");
    processor
        .ingest(&coinbase_trade(0, "buy", "1"), ns(20), ns(20))
        .expect("Coinbase trade is admitted");
    processor
        .ingest(&kraken_trade(0, 10, "update", "sell", "1"), ns(10), ns(21))
        .expect("later-admitted Kraken trade may have an earlier receipt stamp");

    for event in [
        coinbase_book_event(0, 0, 2, "snapshot", "bid", "100", "1", 20),
        coinbase_book_event(1, 1, 2, "snapshot", "ask", "100.02", "1", 20),
    ] {
        processor
            .ingest(&event, ns(20), ns(22))
            .expect("Coinbase snapshot row is admitted");
    }
    for event in [
        kraken_book_event(0, 0, 2, "snapshot", "bid", "100", "1", 10),
        kraken_book_event(1, 1, 2, "snapshot", "ask", "100.02", "1", 10),
    ] {
        processor
            .ingest(&event, ns(10), ns(23))
            .expect("later-admitted Kraken snapshot may have an earlier receipt stamp");
    }
    for event in [
        coinbase_book_event(2, 0, 1, "delta", "bid", "100", "2", 30),
        kraken_book_event(2, 0, 1, "delta", "bid", "100", "2", 25),
    ] {
        let (receipt, now) = if event.source_id.starts_with("coinbase") {
            (ns(30), ns(30))
        } else {
            (ns(25), ns(31))
        };
        processor
            .ingest(&event, receipt, now)
            .expect("per-venue OFI histories accept cross-venue receipt inversion");
    }
}

#[test]
fn disconnect_and_restart_clear_rolling_state_and_rotate_identity() {
    let mut processor = processor("epoch-one");
    for second in 1..=61 {
        admit_second(&mut processor, second, false);
        processor
            .advance_time(ns(second))
            .expect("health tick succeeds");
    }
    let degraded = processor
        .mark_unavailable(Venue::Coinbase, UnavailableReason::Disconnected, ns(62))
        .expect("disconnect is a deterministic processor input");
    assert_eq!(degraded.len(), SPOT_SIGNAL_SET_COUNT);
    let coinbase_ids = venue_spot_signal_ids(Venue::Coinbase);
    let kraken_ids = venue_spot_signal_ids(Venue::Kraken);
    // The blended readings and the dropped venue's own identities withhold; the
    // peer venue keeps publishing from its own untouched history (ADR 0024).
    assert!(degraded.iter().all(|output| {
        let peer = kraken_ids.contains(&output.signal_id.as_str());
        let ready_expected = peer && !warms_beyond_the_fixture(&output.signal_id);
        ready_expected == (output.quality.state == QualityState::Ready)
    }));
    assert!(
        degraded
            .iter()
            .filter(|output| coinbase_ids.contains(&output.signal_id.as_str()))
            .all(|output| output
                .quality
                .reasons
                .iter()
                .any(|reason| reason == "coinbase_venue_unavailable"))
    );

    let checkpoint = processor.checkpoint_state();
    let mut restored = SpotSignalProcessor::restore("epoch-two", wall_start(), checkpoint.clone())
        .expect("new epoch restores checkpoint");
    let first_resumed = restored
        .advance_time(ns(62))
        .expect("resumed health tick succeeds");
    assert!(first_resumed.iter().all(|output| output.sequence == 0));
    // A restore clears every rolling history, so nothing derived from a window
    // can publish yet. The one exception is the surviving venue's stateless
    // quote projection: the restored Kraken quote is genuinely fresh and
    // carries no warm-up, so withholding it would be a lie about the input.
    assert_eq!(
        first_resumed
            .iter()
            .filter(|output| output.quality.state == QualityState::Ready)
            .map(|output| output.signal_id.as_str())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(["btc.kraken.spot_quote"])
    );

    let mut rewarmed = Vec::new();
    for offset in 0..=60 {
        let second = 63 + offset;
        admit_offset_second(&mut restored, second, offset);
        rewarmed = restored
            .advance_time(ns(second))
            .expect("resumed signal history warms from replacement inputs");
    }
    assert!(rewarmed.iter().all(
        |output| output.quality.state == fixture_state(&output.signal_id, QualityState::Ready)
    ));
    assert!(SpotSignalProcessor::restore("epoch-one", wall_start(), checkpoint).is_err());
}

#[test]
fn checkpoint_rejects_invalid_persisted_feature_quality() {
    let mut processor = processor("checkpoint-quality");
    admit_second(&mut processor, 1, false);
    processor
        .advance_time(ns(1))
        .expect("health tick establishes feature quality");
    let mut checkpoint = processor.checkpoint_state();
    let mut inconsistent_output_state = checkpoint.clone();
    inconsistent_output_state
        .features
        .get_mut(REALIZED_VOLATILITY_ID)
        .expect("volatility checkpoint exists")
        .lifetime_output_count = 0;
    assert!(
        SpotSignalProcessor::restore(
            "checkpoint-quality-inconsistent",
            wall_start(),
            inconsistent_output_state
        )
        .is_err()
    );

    checkpoint
        .features
        .get_mut(REALIZED_VOLATILITY_ID)
        .expect("volatility checkpoint exists")
        .last_quality
        .as_mut()
        .expect("quality was emitted")
        .reasons = vec!["duplicate".to_owned(), "duplicate".to_owned()];
    assert!(
        SpotSignalProcessor::restore("checkpoint-quality-next", wall_start(), checkpoint).is_err()
    );
}

#[test]
fn emitted_processor_can_checkpoint_immediately_after_restore() {
    let mut original = processor("checkpoint-before-restore");
    let emitted = original
        .advance_time(0)
        .expect("original processor emits feature quality");
    assert_eq!(emitted.len(), SPOT_SIGNAL_SET_COUNT);

    let before_restore = original.checkpoint_state();
    let restored =
        SpotSignalProcessor::restore("checkpoint-after-restore", wall_start(), before_restore)
            .expect("emitted processor restores under a new epoch");
    let after_restore = restored.checkpoint_state();
    after_restore
        .validate()
        .expect("post-restore checkpoint validates before any new input");
    assert!(after_restore.features.values().all(|feature| {
        feature.previous_next_sequence == 0
            && feature.lifetime_output_count == 1
            && feature.last_quality.is_some()
    }));
}

#[test]
fn checkpoint_accepts_a_reference_clock_lag_after_non_ticker_input() {
    let mut processor = processor("checkpoint-clock-lag");
    processor
        .ingest(&coinbase_ticker(1), ns(1), ns(1))
        .expect("ticker advances both clocks");
    processor
        .ingest(&coinbase_trade(2, "buy", "1"), ns(2), ns(2))
        .expect("trade advances only the global processor clock");
    processor
        .checkpoint_state()
        .validate()
        .expect("reference clock may safely lag global admission time");
}

#[test]
fn identical_inputs_replay_byte_identically() {
    let left = run_fixture("deterministic");
    let right = run_fixture("deterministic");
    assert_eq!(
        serde_json::to_vec(&left).expect("left output serializes"),
        serde_json::to_vec(&right).expect("right output serializes")
    );
}

fn run_fixture(epoch: &str) -> Vec<SignalUpdate> {
    let mut processor = processor(epoch);
    let mut output = Vec::new();
    for second in 1..=61 {
        admit_second(&mut processor, second, true);
        output.extend(
            processor
                .advance_time(ns(second))
                .expect("fixture health tick succeeds"),
        );
    }
    output
}

/// The per-venue identities the 61-tick fixture cannot warm.
///
/// `kyle_lambda_drift` needs `2N` completed per-trade pairs before its drift is
/// defined — 200 at the default window — and the fixture admits one trade per
/// venue per second. `horizon_momentum` / `horizon_volatility` (C2) warm over
/// the longest horizon, 15 minutes (901 contiguous one-second samples), so the
/// 61-tick fixture leaves them warming up; their ready path is exercised by the
/// horizon unit tests and the replay proof. That is a warm-up cost, not an
/// exemption from the readiness contract.
fn warms_beyond_the_fixture(signal_id: &str) -> bool {
    // The C3a technical-analysis pack warms on one-minute bars (fifteen to
    // thirty-four of them) and on one-second windows up to fifteen minutes long,
    // so a sixty-one-second fixture never brings one to ready. Their arithmetic
    // is proven by the known-answer and warm-up tests in `src/ta.rs`.
    signal_id.ends_with(".kyle_lambda_drift")
        || is_ta_identity(signal_id)
        || signal_id.ends_with(".horizon_momentum")
        || signal_id.ends_with(".horizon_volatility")
}

/// The state an identity carries once the fixture has run, where `warmed` is
/// the state an identity that DID warm would carry.
///
/// An identity the fixture never brought to ready has no degraded state to fall
/// to — a feature that was never ready is still warming up — so it stays
/// `WarmingUp` under every fault the fixture then injects.
fn fixture_state(signal_id: &str, warmed: QualityState) -> QualityState {
    if warms_beyond_the_fixture(signal_id) {
        QualityState::WarmingUp
    } else {
        warmed
    }
}

fn admit_second(processor: &mut SpotSignalProcessor, second: u64, snapshot_history: bool) {
    if snapshot_history && second == 1 {
        processor
            .ingest(&kraken_trade(0, 0, "snapshot", "buy", "999"), 0, 0)
            .expect("Kraken subscription history is admitted");
    }
    for event in [coinbase_ticker(second), kraken_ticker(second)] {
        processor
            .ingest(&event, ns(second), ns(second))
            .expect("ticker is admitted");
    }

    let kind = if second == 1 { "snapshot" } else { "delta" };
    let first_sequence = (second - 1) * 2;
    for event in [
        coinbase_book_event(first_sequence, 0, 2, kind, "bid", "100", "2", second),
        coinbase_book_event(first_sequence + 1, 1, 2, kind, "ask", "100.02", "1", second),
        kraken_book_event(first_sequence, 0, 2, kind, "bid", "100", "1", second),
        kraken_book_event(first_sequence + 1, 1, 2, kind, "ask", "100.02", "2", second),
    ] {
        processor
            .ingest(&event, ns(second), ns(second))
            .expect("book row is admitted");
    }

    processor
        .ingest(&coinbase_trade(second, "buy", "1"), ns(second), ns(second))
        .expect("Coinbase trade is admitted");
    let kraken_sequence = if snapshot_history { second } else { second - 1 };
    processor
        .ingest(
            &kraken_trade(kraken_sequence, second, "update", "sell", "0.5"),
            ns(second),
            ns(second),
        )
        .expect("Kraken trade is admitted");
}

fn admit_offset_second(processor: &mut SpotSignalProcessor, second: u64, offset: u64) {
    for event in [coinbase_ticker(second), kraken_ticker(second)] {
        processor
            .ingest(&event, ns(second), ns(second))
            .expect("ticker is admitted");
    }

    let kind = if offset == 0 { "snapshot" } else { "delta" };
    let first_sequence = offset * 2;
    for event in [
        coinbase_book_event(first_sequence, 0, 2, kind, "bid", "100", "2", second),
        coinbase_book_event(first_sequence + 1, 1, 2, kind, "ask", "100.02", "1", second),
        kraken_book_event(first_sequence, 0, 2, kind, "bid", "100", "1", second),
        kraken_book_event(first_sequence + 1, 1, 2, kind, "ask", "100.02", "2", second),
    ] {
        processor
            .ingest(&event, ns(second), ns(second))
            .expect("book row is admitted");
    }

    processor
        .ingest(&coinbase_trade(offset, "buy", "1"), ns(second), ns(second))
        .expect("Coinbase trade is admitted");
    processor
        .ingest(
            &kraken_trade(offset, second, "update", "sell", "0.5"),
            ns(second),
            ns(second),
        )
        .expect("Kraken trade is admitted");
}

// Admits both venues' tickers and a valid book delta for `second` but no
// trades: the venues stay demonstrably alive while their trade windows go quiet.
fn admit_books_and_tickers(processor: &mut SpotSignalProcessor, second: u64) {
    for event in [coinbase_ticker(second), kraken_ticker(second)] {
        processor
            .ingest(&event, ns(second), ns(second))
            .expect("ticker is admitted");
    }
    let first_sequence = (second - 1) * 2;
    for event in [
        coinbase_book_event(first_sequence, 0, 2, "delta", "bid", "100", "2", second),
        coinbase_book_event(
            first_sequence + 1,
            1,
            2,
            "delta",
            "ask",
            "100.02",
            "1",
            second,
        ),
        kraken_book_event(first_sequence, 0, 2, "delta", "bid", "100", "1", second),
        kraken_book_event(
            first_sequence + 1,
            1,
            2,
            "delta",
            "ask",
            "100.02",
            "2",
            second,
        ),
    ] {
        processor
            .ingest(&event, ns(second), ns(second))
            .expect("book row is admitted");
    }
}

// Admits both venues' tickers and trades for `second` but no book events: the
// venues stay alive while their book (OFI) windows go quiet.
fn admit_trades_and_tickers(processor: &mut SpotSignalProcessor, second: u64) {
    for event in [coinbase_ticker(second), kraken_ticker(second)] {
        processor
            .ingest(&event, ns(second), ns(second))
            .expect("ticker is admitted");
    }
    processor
        .ingest(&coinbase_trade(second, "buy", "1"), ns(second), ns(second))
        .expect("Coinbase trade is admitted");
    processor
        .ingest(
            &kraken_trade(second - 1, second, "update", "sell", "0.5"),
            ns(second),
            ns(second),
        )
        .expect("Kraken trade is admitted");
}

#[test]
fn sixty_one_ticks_make_every_per_venue_identity_ready_against_its_own_venue() {
    let mut processor = processor("per-venue-complete-set");
    let mut final_tick = Vec::new();
    for second in 1..=61 {
        admit_second(&mut processor, second, true);
        final_tick = processor
            .advance_time(ns(second))
            .expect("health tick succeeds");
    }

    assert_eq!(final_tick.len(), SPOT_SIGNAL_SET_COUNT);
    let definitions: BTreeMap<String, SignalDefinition> = processor
        .definitions()
        .map(|definition| (definition.signal_id.clone(), definition.clone()))
        .collect();
    for venue in VENUE_SPOT_VENUES {
        for signal_id in venue_spot_signal_ids(venue) {
            let published = update(&final_tick, signal_id);
            let expected = fixture_state(signal_id, QualityState::Ready);
            assert_eq!(published.quality.state, expected, "{signal_id}");
            published
                .validate_against(
                    definitions
                        .get(signal_id)
                        .expect("every per-venue output has an embedded definition"),
                )
                .expect("ready per-venue output validates against its definition");
        }
    }

    // The fixture prices both venues identically but sizes their books
    // differently, so the single-venue depth readings are the mirror image of
    // each other while the blended reading nets to zero.
    assert_eq!(
        update(&final_tick, "btc.coinbase.actionable_depth_imbalance").values["imbalance"],
        json!("0.33333333")
    );
    assert_eq!(
        update(&final_tick, "btc.kraken.actionable_depth_imbalance").values["imbalance"],
        json!("-0.33333333")
    );
    assert_eq!(
        update(&final_tick, ACTIONABLE_DEPTH_IMBALANCE_ID).values["combined_imbalance"],
        json!("0")
    );

    // Coinbase buys one unit a second, Kraken sells a half: the blended flow is
    // the sum of two single-venue flows that disagree in sign.
    assert_eq!(
        update(&final_tick, "btc.coinbase.aggressor_flow").values["cumulative_volume_delta_base_60s"],
        json!("60")
    );
    assert_eq!(
        update(&final_tick, "btc.kraken.aggressor_flow").values["cumulative_volume_delta_base_60s"],
        json!("-30")
    );
    assert_eq!(
        update(&final_tick, AGGRESSOR_FLOW_ID).values["cumulative_volume_delta_base_60s"],
        json!("30")
    );

    // Each quote projection reports only its own venue's displayed book.
    for venue_signal in ["btc.coinbase.spot_quote", "btc.kraken.spot_quote"] {
        let quote = update(&final_tick, venue_signal);
        assert_eq!(quote.values["bid"], json!("100"));
        assert_eq!(quote.values["ask"], json!("100.02"));
        assert_eq!(quote.values["midpoint"], json!("100.01"));
    }
}

#[test]
fn per_venue_identities_leave_the_blended_frames_byte_identical() {
    // The same fixture through the published surface and through the
    // determinism harness that suppresses the per-venue lanes. Every
    // `btc.external.*` frame — values, quality, sequence, timestamps — must be
    // identical, proving the lanes add output without perturbing the blend.
    let mut published = processor("byte-identity");
    let mut suppressed =
        SpotSignalProcessor::new_with_venue_identities_suppressed("byte-identity", wall_start())
            .expect("suppressed processor constructs");

    let mut published_blended = Vec::new();
    let mut suppressed_blended = Vec::new();
    for second in 1..=61 {
        for (target, sink) in [
            (&mut published, &mut published_blended),
            (&mut suppressed, &mut suppressed_blended),
        ] {
            admit_second(target, second, true);
            sink.extend(
                target
                    .advance_time(ns(second))
                    .expect("health tick succeeds"),
            );
        }
    }

    // A book fault and a venue drop both take transition paths of their own, so
    // the comparison covers them too.
    for (target, sink) in [
        (&mut published, &mut published_blended),
        (&mut suppressed, &mut suppressed_blended),
    ] {
        sink.extend(
            target
                .ingest(
                    &coinbase_book_event(122, 0, 1, "delta", "bid", "101", "2", 62),
                    ns(62),
                    ns(62),
                )
                .expect("crossed completed book is contained"),
        );
        sink.extend(
            target
                .mark_unavailable(Venue::Kraken, UnavailableReason::Disconnected, ns(63))
                .expect("disconnect is a deterministic processor input"),
        );
    }

    let blended = |updates: Vec<SignalUpdate>| -> Vec<String> {
        updates
            .into_iter()
            .filter(|update| update.signal_id.starts_with("btc.external."))
            .map(|update| serde_json::to_string(&update).expect("update serializes"))
            .collect()
    };
    let published_blended = blended(published_blended);
    let suppressed_blended = blended(suppressed_blended);
    assert!(!published_blended.is_empty());
    assert_eq!(published_blended, suppressed_blended);
}

#[test]
fn per_venue_identities_replay_deterministically() {
    let replay = |epoch: &str| -> Vec<String> {
        let mut processor = processor(epoch);
        let mut frames = Vec::new();
        for second in 1..=61 {
            admit_second(&mut processor, second, true);
            frames.extend(
                processor
                    .advance_time(ns(second))
                    .expect("health tick succeeds")
                    .into_iter()
                    .map(|update| serde_json::to_string(&update).expect("update serializes")),
            );
        }
        frames
    };
    let first = replay("per-venue-replay");
    let second = replay("per-venue-replay");
    assert_eq!(first.len(), 61 * SPOT_SIGNAL_SET_COUNT);
    assert_eq!(first, second);
}

#[test]
fn a_stale_kraken_leaves_the_coinbase_identities_ready() {
    let mut processor = processor("kraken-stale");
    for second in 1..=61 {
        admit_second(&mut processor, second, true);
        processor
            .advance_time(ns(second))
            .expect("health tick succeeds");
    }

    // Coinbase alone keeps feeding for eleven more seconds, so the Kraken quote
    // and trade stream both fall outside the ten-second staleness bound.
    let mut published = Vec::new();
    for second in 62..=72 {
        admit_coinbase_second(&mut processor, second);
        published = processor
            .advance_time(ns(second))
            .expect("health tick succeeds");
    }

    assert_eq!(published.len(), SPOT_SIGNAL_SET_COUNT);
    for signal_id in venue_spot_signal_ids(Venue::Coinbase) {
        assert_eq!(
            update(&published, signal_id).quality.state,
            fixture_state(signal_id, QualityState::Ready),
            "{signal_id}"
        );
    }
    for signal_id in venue_spot_signal_ids(Venue::Kraken) {
        assert_eq!(
            update(&published, signal_id).quality.state,
            fixture_state(signal_id, QualityState::Degraded),
            "{signal_id}"
        );
    }
    for published_update in &published {
        if published_update.signal_id.starts_with("btc.external.") {
            assert_eq!(
                published_update.quality.state,
                fixture_state(&published_update.signal_id, QualityState::Degraded),
                "{}",
                published_update.signal_id
            );
        }
    }
}

#[test]
fn a_restart_restores_every_per_venue_identity_and_rotates_its_epoch() {
    let mut processor = processor("venue-epoch-one");
    for second in 1..=61 {
        admit_second(&mut processor, second, true);
        processor
            .advance_time(ns(second))
            .expect("health tick succeeds");
    }
    let checkpoint = processor.checkpoint_state();
    for venue in VENUE_SPOT_VENUES {
        for signal_id in venue_spot_signal_ids(venue) {
            let feature = checkpoint
                .features
                .get(signal_id)
                .unwrap_or_else(|| panic!("checkpoint carries {signal_id}"));
            assert_eq!(
                feature.ever_ready,
                !warms_beyond_the_fixture(signal_id),
                "{signal_id}"
            );
            assert!(feature.lifetime_output_count > 0, "{signal_id}");
        }
    }

    let mut restored =
        SpotSignalProcessor::restore("venue-epoch-two", wall_start(), checkpoint.clone())
            .expect("new epoch restores checkpoint");
    let resumed = restored
        .advance_time(ns(61))
        .expect("resumed health tick succeeds");
    assert_eq!(resumed.len(), SPOT_SIGNAL_SET_COUNT);
    assert!(resumed.iter().all(|output| output.sequence == 0));
    assert!(
        resumed
            .iter()
            .all(|output| output.publisher_epoch == "venue-epoch-two")
    );

    // A checkpoint whose per-venue features are missing belongs to a different
    // processor build and cannot restore into this one.
    let mut foreign = checkpoint.clone();
    foreign.features.remove("btc.coinbase.spot_quote");
    assert!(SpotSignalProcessor::restore("venue-epoch-three", wall_start(), foreign).is_err());
}

/// The four measurements adopted per venue on 2026-09-02 (ADR 0024's
/// amendment). They are published ONLY per venue, so there is no
/// `btc.external.*` counterpart to check against.
const MICROSTRUCTURE_MEASUREMENTS: [&str; 4] = [
    "effective_spread",
    "kyle_lambda_drift",
    "sweep_detection",
    "book_resilience",
];

#[test]
fn the_microstructure_measurements_are_published_per_venue_only() {
    for venue in VENUE_SPOT_VENUES {
        let ids = venue_spot_signal_ids(venue);
        assert_eq!(ids.len(), VENUE_SPOT_SIGNAL_COUNT);
        let prefix = match venue {
            Venue::Coinbase => "btc.coinbase.",
            Venue::Kraken => "btc.kraken.",
            Venue::Binance | Venue::Bybit => {
                unreachable!("VENUE_SPOT_VENUES is the two USD venues")
            }
        };
        for measurement in MICROSTRUCTURE_MEASUREMENTS {
            assert!(
                ids.contains(&format!("{prefix}{measurement}").as_str()),
                "{prefix}{measurement} is published"
            );
        }
    }
    // The blended variants are deferred, not published: no processor embeds a
    // `btc.external.*` document for any of the four.
    let blended: BTreeSet<&str> = spot_signal_contract_documents()
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    for measurement in MICROSTRUCTURE_MEASUREMENTS {
        assert!(
            !blended.contains(format!("btc.external.{measurement}").as_str()),
            "btc.external.{measurement} must stay deferred"
        );
    }
}

#[test]
fn microstructure_identities_publish_from_their_own_venue() {
    let mut processor = processor("microstructure");
    let mut final_tick = Vec::new();
    for second in 1..=61 {
        admit_second(&mut processor, second, true);
        final_tick = processor
            .advance_time(ns(second))
            .expect("health tick succeeds");
    }

    // The fixture prints one Coinbase buy of size 1 and one Kraken sell of size
    // 0.5 per second against an identical 100 / 100.02 quote, so each venue's
    // effective spread is its own trade's, priced against its own midpoint.
    let coinbase = update(&final_tick, "btc.coinbase.effective_spread");
    assert_eq!(coinbase.quality.state, QualityState::Ready);
    assert_eq!(coinbase.values["aggressor_side"], json!("buy"));
    assert_eq!(coinbase.values["midpoint"], json!("100.01"));
    assert_eq!(coinbase.values["realized_horizon_ms"], json!(5000));
    let kraken = update(&final_tick, "btc.kraken.effective_spread");
    assert_eq!(kraken.quality.state, QualityState::Ready);
    assert_eq!(kraken.values["aggressor_side"], json!("sell"));

    // A single trade per batch can never span three price levels, so the sweep
    // verdict is a truthful quiet with the pre-first-sweep sentinel payload.
    for signal_id in ["btc.coinbase.sweep_detection", "btc.kraken.sweep_detection"] {
        let sweep = update(&final_tick, signal_id);
        assert_eq!(sweep.quality.state, QualityState::Ready, "{signal_id}");
        assert_eq!(sweep.values["detection_state"], json!("quiet"));
        assert_eq!(sweep.values["direction"], json!("none"));
        assert_eq!(sweep.values["swept_levels"], json!(0));
        assert_eq!(sweep.values["min_levels"], json!(3));
    }

    // The fixture never withdraws displayed size, so no depletion is tracked
    // and both presence flags carry their explicit sentinels.
    for signal_id in ["btc.coinbase.book_resilience", "btc.kraken.book_resilience"] {
        let resilience = update(&final_tick, signal_id);
        assert_eq!(resilience.quality.state, QualityState::Ready, "{signal_id}");
        assert_eq!(resilience.values["state"], json!("quiet"));
        assert_eq!(resilience.values["recovery_ms_present"], json!(false));
        assert_eq!(resilience.values["recovery_ms"], json!(0));
        assert_eq!(resilience.values["depletion_side"], json!("none"));
        assert_eq!(resilience.values["deplete_fraction"], json!("0.5"));
        assert_eq!(resilience.values["recover_fraction"], json!("0.75"));
    }

    // Every published frame still validates against its embedded definition,
    // which is what proves each declared field is present on a ready update.
    let definitions: BTreeMap<String, SignalDefinition> = processor
        .definitions()
        .map(|definition| (definition.signal_id.clone(), definition.clone()))
        .collect();
    for venue in VENUE_SPOT_VENUES {
        for signal_id in venue_spot_signal_ids(venue) {
            update(&final_tick, signal_id)
                .validate_against(
                    definitions
                        .get(signal_id)
                        .expect("every per-venue output has an embedded definition"),
                )
                .expect("per-venue output validates against its definition");
        }
    }
}

#[test]
fn the_effective_spread_publishes_the_resolved_measurement_once_one_exists() {
    // The definition ranks the two halves: "the published reading is the most
    // recent resolved measurement when one exists, otherwise the most recent
    // priced trade still awaiting its horizon". The fixture prints one trade per
    // venue per second against the default 5000 ms horizon, so running well past
    // twice the horizon leaves a resolution available at every late decision.
    let mut processor = processor("realized-precedence");
    let mut frames = Vec::new();
    for second in 1..=15 {
        admit_second(&mut processor, second, true);
        frames.push((
            second,
            processor
                .advance_time(ns(second))
                .expect("health tick succeeds"),
        ));
    }

    for venue_prefix in ["btc.coinbase.", "btc.kraken."] {
        let signal_id = format!("{venue_prefix}effective_spread");
        let mut first_resolved = None;
        for (second, published) in &frames {
            let reading = update(published, &signal_id);
            if reading.quality.state != QualityState::Ready {
                continue;
            }
            let resolved = reading.values["realized_state"] == json!("ready");
            assert_eq!(
                reading.values["realized_present"],
                json!(resolved),
                "{signal_id} at second {second} disagrees with itself"
            );
            match first_resolved {
                // Once a measurement has resolved, an awaiting trade never
                // supersedes it again: every later ready frame stays resolved.
                Some(first) => assert!(
                    resolved,
                    "{signal_id} fell back to awaiting_horizon at second {second} \
                     after resolving at second {first}"
                ),
                None if resolved => first_resolved = Some(*second),
                None => {}
            }
        }
        let first = first_resolved.unwrap_or_else(|| {
            panic!("{signal_id} never published a resolved realized measurement")
        });
        // The horizon is five seconds and resolution triggers on the first
        // observation strictly past it, so the fixture cannot resolve earlier
        // than the seventh second.
        assert!(
            first >= 7,
            "{signal_id} resolved implausibly early at {first}"
        );
    }

    // The last frame is the one a consumer would read, and it carries the
    // complete measurement — both halves, not the awaiting sentinel.
    let (_, last) = frames.last().expect("the fixture published frames");
    for venue_prefix in ["btc.coinbase.", "btc.kraken."] {
        let reading = update(last, &format!("{venue_prefix}effective_spread"));
        assert_eq!(reading.quality.state, QualityState::Ready);
        assert_eq!(reading.values["realized_state"], json!("ready"));
        assert_eq!(reading.values["realized_present"], json!(true));
        // The fixture holds the quote fixed, so the midpoint at the horizon is
        // the trade's own midpoint and the realized half is an exact zero.
        assert_eq!(reading.values["midpoint_at_horizon"], json!("100.01"));
        assert_eq!(reading.values["realized_spread_bps"], json!("0"));
    }
}

#[test]
fn kyle_lambda_drift_becomes_ready_on_its_own_venue() {
    // The estimator needs 2N completed pairs at the default window of 100, and
    // the fixture completes one pair per venue per second, so this is the
    // shortest run that reaches ready. It runs twice and compares the whole
    // frame sequence, so the warm path is proved deterministic as well as
    // reachable.
    let replay = |epoch: &str| -> Vec<String> {
        let mut processor = processor(epoch);
        let mut frames = Vec::new();
        for second in 1..=205 {
            admit_second(&mut processor, second, true);
            frames.extend(
                processor
                    .advance_time(ns(second))
                    .expect("health tick succeeds")
                    .into_iter()
                    .map(|update| serde_json::to_string(&update).expect("update serializes")),
            );
        }
        frames
    };
    assert_eq!(replay("kyle-warm"), replay("kyle-warm"));

    let mut processor = processor("kyle-warm");
    let mut final_tick = Vec::new();
    for second in 1..=205 {
        admit_second(&mut processor, second, true);
        final_tick = processor
            .advance_time(ns(second))
            .expect("health tick succeeds");
    }
    for signal_id in [
        "btc.coinbase.kyle_lambda_drift",
        "btc.kraken.kyle_lambda_drift",
    ] {
        let kyle = update(&final_tick, signal_id);
        assert_eq!(kyle.quality.state, QualityState::Ready, "{signal_id}");
        assert_eq!(kyle.values["window_trades"], json!(100), "{signal_id}");
        assert_eq!(kyle.values["pair_count"], json!(200), "{signal_id}");
        // The fixture holds the quote fixed, so every forward midpoint change
        // is zero and both windows measure a zero slope. The value being an
        // exact zero rather than absent is the point: it is a measurement.
        assert_eq!(kyle.values["kyle_lambda"], json!("0"), "{signal_id}");
        assert_eq!(kyle.values["lambda_drift"], json!("0"), "{signal_id}");
    }
}

#[test]
fn a_stale_kraken_leaves_the_coinbase_microstructure_identities_ready() {
    let mut processor = processor("microstructure-stale");
    for second in 1..=61 {
        admit_second(&mut processor, second, true);
        processor
            .advance_time(ns(second))
            .expect("health tick succeeds");
    }

    // Coinbase alone keeps feeding past the ten-second bound, so only the
    // Kraken join goes stale.
    let mut published = Vec::new();
    for second in 62..=72 {
        admit_coinbase_second(&mut processor, second);
        published = processor
            .advance_time(ns(second))
            .expect("health tick succeeds");
    }

    for measurement in MICROSTRUCTURE_MEASUREMENTS {
        let coinbase = update(&published, &format!("btc.coinbase.{measurement}"));
        assert_eq!(
            coinbase.quality.state,
            fixture_state(&coinbase.signal_id, QualityState::Ready),
            "btc.coinbase.{measurement}"
        );
        let kraken = update(&published, &format!("btc.kraken.{measurement}"));
        assert_ne!(
            kraken.quality.state,
            QualityState::Ready,
            "btc.kraken.{measurement}"
        );
        // The staleness gate publishes an existing stream-v3 term, not a new
        // one invented for this family.
        assert!(
            kraken
                .quality
                .reasons
                .contains(&"book_snapshot_stale_or_missing".to_owned()),
            "btc.kraken.{measurement} reasons {:?}",
            kraken.quality.reasons
        );
    }
}

#[test]
fn a_same_venue_receipt_regression_is_refused_by_the_microstructure_join() {
    // The microstructure join orders one venue's book and trade streams on a
    // single logical receipt clock, so it refuses a same-venue regression
    // instead of silently reordering it (join invariant 2). The journal's
    // admission gate already enforces the same rule per venue across every
    // stream, so this can only be reached by driving the processor directly.
    let mut processor = processor("receipt-regression");
    admit_second(&mut processor, 1, true);
    admit_second(&mut processor, 2, true);
    // A completed book group behind the venue's last admitted receipt. The book
    // state itself keeps no receipt cursor — only source sequence — so this
    // reaches the join, which is the component that refuses it.
    processor
        .ingest(
            &coinbase_book_event(4, 0, 2, "delta", "bid", "100", "2", 3),
            ns(1),
            ns(2),
        )
        .expect("the first row of the group is pending");
    let error = processor
        .ingest(
            &coinbase_book_event(5, 1, 2, "delta", "ask", "100.02", "1", 3),
            ns(1),
            ns(2),
        )
        .expect_err("a same-venue receipt regression is refused");
    assert!(
        matches!(error, SpotSignalError::TimeRegression),
        "{error:?}"
    );
}

#[test]
fn the_full_processor_checkpoint_round_trips_exactly() {
    let mut processor = processor("round-trip-one");
    for second in 1..=61 {
        admit_second(&mut processor, second, true);
        processor
            .advance_time(ns(second))
            .expect("health tick succeeds");
    }
    let checkpoint = processor.checkpoint_state();
    // Every published identity except the hosted reference, which checkpoints
    // under its own key rather than in the feature map.
    assert_eq!(checkpoint.features.len(), SPOT_SIGNAL_SET_COUNT - 1);
    for venue in VENUE_SPOT_VENUES {
        for measurement in MICROSTRUCTURE_MEASUREMENTS {
            let signal_id = match venue {
                Venue::Coinbase => format!("btc.coinbase.{measurement}"),
                Venue::Kraken => format!("btc.kraken.{measurement}"),
                Venue::Binance | Venue::Bybit => {
                    unreachable!("VENUE_SPOT_VENUES is the two USD venues")
                }
            };
            assert!(
                checkpoint.features.contains_key(&signal_id),
                "checkpoint carries {signal_id}"
            );
        }
    }

    let restored = SpotSignalProcessor::restore("round-trip-two", wall_start(), checkpoint.clone())
        .expect("new epoch restores checkpoint");
    let round_tripped = restored.checkpoint_state();
    // Output identity is never reused: a resumed process starts every sequence
    // at zero under its new epoch. That is the ONLY difference the feature map
    // is allowed to have — every blended feature and every per-venue feature,
    // the eight microstructure identities included, must come back byte for
    // byte otherwise.
    let mut expected = checkpoint.features.clone();
    for feature in expected.values_mut() {
        feature.previous_next_sequence = 0;
    }
    assert_eq!(round_tripped.features, expected);
    assert_eq!(round_tripped.last_now_ns, checkpoint.last_now_ns);
    // The hosted reference carries its own epoch lineage and sequence reset, so
    // it is compared on the logical time a restore is contracted to preserve
    // rather than on the fields the new epoch is contracted to move.
    assert_eq!(
        round_tripped.reference.last_now_ns(),
        checkpoint.reference.last_now_ns()
    );
}

// Admits only Coinbase inputs for `second`, continuing the sequences
// `admit_second` established, so Kraken goes quiet without disconnecting.
fn admit_coinbase_second(processor: &mut SpotSignalProcessor, second: u64) {
    processor
        .ingest(&coinbase_ticker(second), ns(second), ns(second))
        .expect("ticker is admitted");
    let first_sequence = (second - 1) * 2;
    for event in [
        coinbase_book_event(first_sequence, 0, 2, "delta", "bid", "100", "2", second),
        coinbase_book_event(
            first_sequence + 1,
            1,
            2,
            "delta",
            "ask",
            "100.02",
            "1",
            second,
        ),
    ] {
        processor
            .ingest(&event, ns(second), ns(second))
            .expect("book row is admitted");
    }
    processor
        .ingest(&coinbase_trade(second, "buy", "1"), ns(second), ns(second))
        .expect("Coinbase trade is admitted");
}

fn processor(epoch: &str) -> SpotSignalProcessor {
    SpotSignalProcessor::new(epoch, wall_start()).expect("processor constructs")
}

fn timestamp(second: u64) -> String {
    wall_start()
        .checked_add_signed(Duration::seconds(
            i64::try_from(second).expect("test second fits i64"),
        ))
        .expect("test timestamp is representable")
        .to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn ns(second: u64) -> u64 {
    second * ONE_SECOND_NS
}

fn update<'a>(updates: &'a [SignalUpdate], signal_id: &str) -> &'a SignalUpdate {
    updates
        .iter()
        .find(|update| update.signal_id == signal_id)
        .unwrap_or_else(|| panic!("missing output for {signal_id}"))
}

fn source_event(
    source_id: &str,
    source_version: &str,
    publisher_epoch: &str,
    sequence: u64,
    second: u64,
    values: BTreeMap<String, Value>,
) -> SourceEvent {
    let event_time = timestamp(second);
    SourceEvent {
        schema_version: 1,
        source_id: source_id.to_owned(),
        source_version: source_version.to_owned(),
        publisher_epoch: publisher_epoch.to_owned(),
        sequence,
        event_time: event_time.clone(),
        received_at: event_time,
        values,
    }
}

fn coinbase_ticker(second: u64) -> SourceEvent {
    let event_time = timestamp(second);
    source_event(
        "coinbase.exchange.btc-usd.ticker",
        "1.0.0",
        "coinbase-feed",
        second,
        second,
        BTreeMap::from([
            ("instrument".to_owned(), json!("BTC-USD")),
            ("trade_id".to_owned(), json!(10_000 + second)),
            ("price".to_owned(), json!("100.01")),
            ("last_size".to_owned(), json!("0.1")),
            ("best_bid".to_owned(), json!("100")),
            ("best_bid_size".to_owned(), json!("2")),
            ("best_ask".to_owned(), json!("100.02")),
            ("best_ask_size".to_owned(), json!("1")),
            ("open_24h".to_owned(), json!("100")),
            ("volume_24h".to_owned(), json!("1000")),
            ("low_24h".to_owned(), json!("90")),
            ("high_24h".to_owned(), json!("110")),
            ("volume_30d".to_owned(), json!("30000")),
            ("event_time".to_owned(), json!(event_time)),
            ("source_sequence".to_owned(), json!(20_000 + second)),
        ]),
    )
}

fn kraken_ticker(second: u64) -> SourceEvent {
    let event_time = timestamp(second);
    source_event(
        "kraken.spot.btc-usd.ticker",
        "1.0.0",
        "kraken-feed",
        second,
        second,
        BTreeMap::from([
            ("instrument".to_owned(), json!("BTC/USD")),
            ("best_bid".to_owned(), json!("100")),
            ("best_bid_size".to_owned(), json!("1")),
            ("best_ask".to_owned(), json!("100.02")),
            ("best_ask_size".to_owned(), json!("2")),
            ("last_price".to_owned(), json!("100.01")),
            ("volume_24h".to_owned(), json!("1000")),
            ("vwap_24h".to_owned(), json!("100.01")),
            ("low_24h".to_owned(), json!("90")),
            ("high_24h".to_owned(), json!("110")),
            ("change_24h".to_owned(), json!("1")),
            ("change_percent_24h".to_owned(), json!("1")),
            ("trade_count_24h".to_owned(), json!(1000)),
            ("update_kind".to_owned(), json!("update")),
            ("event_time".to_owned(), json!(event_time)),
        ]),
    )
}

#[allow(clippy::too_many_arguments)]
fn coinbase_book_event(
    sequence: u64,
    index: u32,
    count: u32,
    kind: &str,
    side: &str,
    price: &str,
    size: &str,
    second: u64,
) -> SourceEvent {
    let event_time = timestamp(second);
    source_event(
        "coinbase.exchange.btc-usd.book-levels",
        "1.0.0",
        "coinbase-book",
        sequence,
        second,
        BTreeMap::from([
            ("instrument".to_owned(), json!("BTC-USD")),
            ("update_group".to_owned(), json!(format!("cb-{second}"))),
            ("update_index".to_owned(), json!(index)),
            ("update_count".to_owned(), json!(count)),
            ("update_kind".to_owned(), json!(kind)),
            ("side".to_owned(), json!(side)),
            ("price".to_owned(), json!(price)),
            ("size".to_owned(), json!(size)),
            ("event_time_basis".to_owned(), json!("receipt")),
            ("event_time".to_owned(), json!(event_time)),
        ]),
    )
}

#[allow(clippy::too_many_arguments)]
fn kraken_book_event(
    sequence: u64,
    index: u32,
    count: u32,
    kind: &str,
    side: &str,
    price: &str,
    size: &str,
    second: u64,
) -> SourceEvent {
    let event_time = timestamp(second);
    source_event(
        "kraken.spot.btc-usd.book-levels",
        "1.0.0",
        "kraken-book",
        sequence,
        second,
        BTreeMap::from([
            ("instrument".to_owned(), json!("BTC/USD")),
            ("configured_depth".to_owned(), json!(100)),
            ("update_group".to_owned(), json!(format!("kr-{second}"))),
            ("update_index".to_owned(), json!(index)),
            ("update_count".to_owned(), json!(count)),
            ("update_kind".to_owned(), json!(kind)),
            ("side".to_owned(), json!(side)),
            ("price".to_owned(), json!(price)),
            ("size".to_owned(), json!(size)),
            ("change_origin".to_owned(), json!("provider")),
            ("book_checksum".to_owned(), json!(1234)),
            ("event_time".to_owned(), json!(event_time)),
        ]),
    )
}

fn coinbase_trade(sequence: u64, aggressor: &str, size: &str) -> SourceEvent {
    let event_time = timestamp(sequence);
    source_event(
        "coinbase.exchange.btc-usd.trades",
        "1.0.0",
        "coinbase-trades",
        sequence,
        sequence,
        BTreeMap::from([
            ("instrument".to_owned(), json!("BTC-USD")),
            ("trade_id".to_owned(), json!(sequence)),
            ("price".to_owned(), json!("100.01")),
            ("size".to_owned(), json!(size)),
            (
                "maker_side".to_owned(),
                json!(if aggressor == "buy" { "sell" } else { "buy" }),
            ),
            ("aggressor_side".to_owned(), json!(aggressor)),
            ("event_time".to_owned(), json!(event_time)),
        ]),
    )
}

fn kraken_trade(
    sequence: u64,
    second: u64,
    update_kind: &str,
    aggressor: &str,
    size: &str,
) -> SourceEvent {
    let event_time = timestamp(second);
    source_event(
        "kraken.spot.btc-usd.trades",
        "1.0.0",
        "kraken-trades",
        sequence,
        second,
        BTreeMap::from([
            ("instrument".to_owned(), json!("BTC/USD")),
            ("trade_id".to_owned(), json!(sequence)),
            ("price".to_owned(), json!("100.01")),
            ("size".to_owned(), json!(size)),
            ("aggressor_side".to_owned(), json!(aggressor)),
            ("order_type".to_owned(), json!("market")),
            ("update_kind".to_owned(), json!(update_kind)),
            ("event_time".to_owned(), json!(event_time)),
        ]),
    )
}

/// C3a byte-identity guard: the frames every pre-C3 identity publishes over the
/// 61-tick fixture, fingerprinted.
///
/// The TA pack (C3a) adds seven identities to each spot scope and C2 adds the
/// two horizon identities; both share the midpoint/reference rings the
/// volatility and momentum readings already drive, so the one thing that must be
/// proven is that they change nothing about the identities that shipped before
/// them. This hashes the canonical JSON of every pre-C3
/// update of a full fixture run — value maps, quality states, reasons,
/// sequences, timestamps and all — so any perturbation of an existing frame,
/// anywhere in the run, moves the fingerprint.
#[test]
fn the_pre_c3_frames_are_byte_identical() {
    let mut processor = processor("pre-c3-byte-identity");
    let mut serialized = String::new();
    for second in 1..=61 {
        admit_second(&mut processor, second, true);
        let updates = processor
            .advance_time(ns(second))
            .expect("health tick succeeds");
        for update in updates {
            // Skip the C3a TA pack (including tick_velocity) and the C2 horizon
            // identities: the guard proves neither slice perturbed the frames
            // that shipped before them.
            if is_ta_identity(&update.signal_id)
                || update.signal_id.ends_with(".tick_velocity")
                || update.signal_id.ends_with(".horizon_momentum")
                || update.signal_id.ends_with(".horizon_volatility")
            {
                continue;
            }
            serialized.push_str(&serde_json::to_string(&update).expect("update serializes"));
            serialized.push('\n');
        }
    }
    assert_eq!(
        (serialized.len(), support::crc32c(serialized.as_bytes())),
        (PRE_C3_FRAME_BYTES, PRE_C3_FRAME_CRC32C),
        "a pre-C3 identity's published frames changed"
    );
}

/// Recorded on the pre-C3a tree (definitions 53/103) and unchanged by the pack.
const PRE_C3_FRAME_BYTES: usize = 1_183_220;
const PRE_C3_FRAME_CRC32C: u32 = 481_864_516;

/// Whether a signal id is one of the seven C3a TA identities.
fn is_ta_identity(signal_id: &str) -> bool {
    [
        ".atr",
        ".rsi",
        ".roc",
        ".ema_cross",
        ".macd",
        ".bollinger",
        ".zscore",
    ]
    .iter()
    .any(|suffix| signal_id.ends_with(suffix))
}

/// The C3a pack reaches ready through the real processor, on the real decision
/// clock, for every scope the blended spot lane publishes.
///
/// The engine-level arithmetic is proven by the known-answer tests in
/// `src/ta.rs`; what this proves is the wiring — that the identities are driven
/// from the one-second reference and per-venue midpoints on the health clock,
/// that they validate against their embedded definitions, and that the
/// degenerate Z-score window publishes `flat_window` (registry entry 119)
/// rather than a value. The fixture's price path is flat, so the readings are
/// the zero-volatility ones: ATR zero, RSI neutral at fifty, Bollinger inside a
/// zero-width band.
#[test]
fn the_ta_pack_readies_on_the_decision_clock() {
    let mut processor = processor("ta-warm-up");
    let mut final_tick = Vec::new();
    // Thirty-four closed one-minute bars is the longest bar warm-up (MACD), and
    // the bar series starts one minute late because a bar begun mid-minute is
    // discarded, so thirty-six minutes of ticks clears every warm-up.
    for second in 1..=(36 * 60) {
        admit_second(&mut processor, second, second == 1);
        final_tick = processor
            .advance_time(ns(second))
            .expect("health tick succeeds");
    }
    let definitions: BTreeMap<String, SignalDefinition> = processor
        .definitions()
        .map(|definition| (definition.signal_id.clone(), definition.clone()))
        .collect();

    let mut seen = 0_usize;
    for update in &final_tick {
        if !is_ta_identity(&update.signal_id) {
            continue;
        }
        seen += 1;
        update
            .validate_against(&definitions[&update.signal_id])
            .expect("TA output validates against its definition");
        if update.signal_id.ends_with(".zscore") {
            // A perfectly flat fixture has zero standard deviation in every
            // window, so no score is defined.
            assert_eq!(update.quality.state, QualityState::WarmingUp);
            assert_eq!(update.quality.reasons, vec!["flat_window".to_owned()]);
            continue;
        }
        assert_eq!(
            update.quality.state,
            QualityState::Ready,
            "{}",
            update.signal_id
        );
    }
    // Seven identities on each of the blended, Coinbase and Kraken scopes.
    assert_eq!(seen, 21);

    let atr = update(&final_tick, "btc.external.atr");
    assert_eq!(atr.values["atr"], json!("0"));
    assert_eq!(atr.values["atr_bps"], json!("0"));
    assert_eq!(atr.values["bar_count"], json!(35));
    let rsi = update(&final_tick, "btc.external.rsi");
    assert_eq!(rsi.values["rsi_14"], json!("50"));
    assert_eq!(rsi.values["regime"], json!("neutral"));
    let bollinger = update(&final_tick, "btc.external.bollinger");
    assert_eq!(bollinger.values["percent_b"], json!("0.5"));
    assert_eq!(bollinger.values["bandwidth_bps"], json!("0"));
    assert_eq!(bollinger.values["state"], json!("inside"));
    let roc = update(&final_tick, "btc.external.roc");
    assert_eq!(roc.values["roc_bps_10s"], json!("0"));
    assert_eq!(roc.values["roc_bps_5m"], json!("0"));
    let ema = update(&final_tick, "btc.external.ema_cross");
    assert_eq!(ema.values["spread_bps"], json!("0"));
    assert_eq!(ema.values["state"], json!("flat"));
    assert_eq!(ema.values["crossed_present"], json!(false));
    let macd = update(&final_tick, "btc.external.macd");
    assert_eq!(macd.values["macd_bps"], json!("0"));
    assert_eq!(macd.values["histogram_bps"], json!("0"));
    // The Coinbase and Kraken lanes publish the same readings off their own
    // midpoints, proving the shared unit is driven per scope.
    for venue in ["coinbase", "kraken"] {
        assert_eq!(
            update(&final_tick, &format!("btc.{venue}.rsi")).values["rsi_14"],
            json!("50")
        );
    }
}

// --- C5 in-process file-generation rotation: value invisibility -----------

/// A run rotated mid-warm-up to a fresh output epoch at sequence 0 — keeping the
/// processor and every ring warm in memory (C5 Design B) — produces the same
/// signal *values and quality states* as an unrotated run at every decision
/// after the boundary. Only the envelope identity (publisher_epoch, sequence)
/// changes, and every identity that was Ready before the boundary is Ready in
/// the first decision after it.
///
/// This is the core C5 guarantee for the spot lane. A later generation's journal
/// begins with book deltas whose establishing snapshot was consumed in an earlier
/// generation and is not checkpointed; the rotation admits a book resync as the
/// new generation's first records (driven here, not just `rotate_output_epoch`)
/// so that journal replays standalone too — the warm rings still re-warm honestly
/// on that replay. See the C5 build report.
#[test]
fn rotation_is_invisible_to_the_values_spot() {
    type SecondSnapshot = BTreeMap<String, (String, QualityState, String, u64)>;

    fn drive(rotate_at: Option<u64>) -> BTreeMap<u64, SecondSnapshot> {
        let mut processor = processor("rotation-invisible-values");
        let mut per_second = BTreeMap::new();
        for second in 1..=61 {
            admit_second(&mut processor, second, true);
            if Some(second) == rotate_at {
                // Drive the REAL rotation path, not `rotate_output_epoch` alone:
                // rotate the output epoch AND admit the book resync (the same
                // rows `admit_book_resync` journals as the new generation's first
                // records). On this warm processor the synthesized snapshot rows
                // are ordering-skipped and value-neutral, so the values after the
                // boundary must be identical to an unrotated run — while a cold
                // replay of that journal would rebuild the books from them.
                processor
                    .rotate_output_epoch("rotation-invisible-values-2")
                    .expect("rotation keeps the warm processor");
                for batch in processor.book_resync_inputs() {
                    for event in &batch {
                        processor
                            .ingest_resync(event, ns(second), ns(second))
                            .expect("book resync row applies to the warm book only");
                    }
                }
            }
            let updates = processor
                .advance_time(ns(second))
                .expect("health tick succeeds");
            let snapshot: SecondSnapshot = updates
                .into_iter()
                .map(|update| {
                    (
                        update.signal_id.clone(),
                        (
                            serde_json::to_string(&update.values).expect("values serialize"),
                            update.quality.state,
                            update.publisher_epoch.clone(),
                            update.sequence,
                        ),
                    )
                })
                .collect();
            per_second.insert(second, snapshot);
        }
        per_second
    }

    let unrotated = drive(None);
    let rotated = drive(Some(30));

    for second in 31..=61 {
        let base = &unrotated[&second];
        let rolled = &rotated[&second];
        assert_eq!(
            base.keys().collect::<Vec<_>>(),
            rolled.keys().collect::<Vec<_>>(),
            "the same identities are published at second {second}"
        );
        for (signal_id, (values, state, _, _)) in base {
            let (rolled_values, rolled_state, rolled_epoch, _) = &rolled[signal_id];
            assert_eq!(
                values, rolled_values,
                "values unchanged by rotation: {signal_id} @ {second}"
            );
            assert_eq!(
                state, rolled_state,
                "quality state unchanged by rotation: {signal_id} @ {second}"
            );
            assert_eq!(
                rolled_epoch, "rotation-invisible-values-2",
                "the rotated run carries the new output epoch: {signal_id} @ {second}"
            );
        }
    }

    // Every identity Ready before the boundary is Ready in the first decision
    // after it (no ring re-warms across the in-process rotation).
    let before = &unrotated[&30];
    let after = &rotated[&31];
    let mut ready_before = 0;
    for (signal_id, (_, state, _, _)) in before {
        if *state == QualityState::Ready {
            ready_before += 1;
            assert_eq!(
                after[signal_id].1,
                QualityState::Ready,
                "{signal_id} stays Ready across the in-process boundary"
            );
        }
    }
    assert!(
        ready_before > 0,
        "the fixture warms at least one identity before the boundary"
    );

    // The rotated run reset its output sequence at the boundary: at the first
    // post-boundary decision its sequence is below the unrotated run's, which
    // never reset.
    let spot_before = &unrotated[&31]["btc.external.spot_reference"];
    let spot_after = &rotated[&31]["btc.external.spot_reference"];
    assert!(
        spot_after.3 < spot_before.3,
        "rotation restarts the output sequence at zero (rotated seq {} < unrotated seq {})",
        spot_after.3,
        spot_before.3
    );
}
