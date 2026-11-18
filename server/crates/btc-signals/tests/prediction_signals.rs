use std::collections::{BTreeMap, BTreeSet};

use btc_signals::{
    COINBASE_TICKER_SOURCE_ID, COINBASE_TICKER_SOURCE_VERSION, KALSHI_EXECUTABLE_EDGE_ID,
    KALSHI_EXPECTED_VALUE_ID, KALSHI_FAIR_UP_PROBABILITY_ID, KALSHI_MARKET_CONTRACT_ID,
    KALSHI_PAIR_COST_STATE_ID, KALSHI_PREDICTION_LIQUIDITY_ID, KALSHI_PREDICTION_QUOTE_ID,
    KALSHI_PREDICTION_TRADE_FLOW_ID, KALSHI_ROC_ID, KALSHI_RSI_ID,
    KALSHI_STANDARDIZED_MONEYNESS_ID, KALSHI_TICK_VELOCITY_ID, KALSHI_TIE_RULE_CONTEXT_ID,
    KALSHI_VENUE_REPRICING_LAG_ID, KALSHI_WINDOW_DELTA_ID, KALSHI_ZSCORE_ID,
    KRAKEN_TICKER_SOURCE_ID, KRAKEN_TICKER_SOURCE_VERSION, POLYMARKET_EXECUTABLE_EDGE_ID,
    POLYMARKET_EXPECTED_VALUE_ID, POLYMARKET_FAIR_UP_PROBABILITY_ID, POLYMARKET_MARKET_CONTRACT_ID,
    POLYMARKET_ORACLE_DIVERGENCE_ID, POLYMARKET_PAIR_COST_STATE_ID,
    POLYMARKET_PREDICTION_LIQUIDITY_ID, POLYMARKET_PREDICTION_QUOTE_ID,
    POLYMARKET_PREDICTION_TRADE_FLOW_ID, POLYMARKET_ROC_ID, POLYMARKET_RSI_ID,
    POLYMARKET_SETTLEMENT_ANCHOR_STATE_ID, POLYMARKET_STANDARDIZED_MONEYNESS_ID,
    POLYMARKET_TICK_VELOCITY_ID, POLYMARKET_TIE_RULE_CONTEXT_ID, POLYMARKET_VENUE_REPRICING_LAG_ID,
    POLYMARKET_WINDOW_DELTA_ID, POLYMARKET_ZSCORE_ID, PredictionBatch, PredictionSignalError,
    PredictionSignalProcessor, prediction_signal_contract_documents, prediction_signal_count,
    prediction_signal_ids,
};
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use serde_json::{Value, json};
use signals_core::{
    PredictionMarketVenue, QualityState, SignalDefinition, SignalUpdate, SourceEvent, Validate,
};

const ONE_SECOND_NS: u64 = 1_000_000_000;

#[test]
fn embedded_prediction_definitions_cover_both_venue_scoped_sets() {
    let documents = prediction_signal_contract_documents();
    let ids = documents.iter().map(|(id, _)| *id).collect::<BTreeSet<_>>();
    assert_eq!(
        ids,
        BTreeSet::from([
            POLYMARKET_MARKET_CONTRACT_ID,
            POLYMARKET_PREDICTION_QUOTE_ID,
            POLYMARKET_PREDICTION_LIQUIDITY_ID,
            POLYMARKET_PREDICTION_TRADE_FLOW_ID,
            KALSHI_MARKET_CONTRACT_ID,
            KALSHI_PREDICTION_QUOTE_ID,
            KALSHI_PREDICTION_LIQUIDITY_ID,
            KALSHI_PREDICTION_TRADE_FLOW_ID,
            POLYMARKET_WINDOW_DELTA_ID,
            POLYMARKET_STANDARDIZED_MONEYNESS_ID,
            POLYMARKET_FAIR_UP_PROBABILITY_ID,
            POLYMARKET_EXECUTABLE_EDGE_ID,
            KALSHI_WINDOW_DELTA_ID,
            KALSHI_STANDARDIZED_MONEYNESS_ID,
            KALSHI_FAIR_UP_PROBABILITY_ID,
            KALSHI_EXECUTABLE_EDGE_ID,
            POLYMARKET_PAIR_COST_STATE_ID,
            POLYMARKET_VENUE_REPRICING_LAG_ID,
            POLYMARKET_TIE_RULE_CONTEXT_ID,
            KALSHI_PAIR_COST_STATE_ID,
            KALSHI_VENUE_REPRICING_LAG_ID,
            KALSHI_TIE_RULE_CONTEXT_ID,
            POLYMARKET_SETTLEMENT_ANCHOR_STATE_ID,
            POLYMARKET_ORACLE_DIVERGENCE_ID,
            // C2 (E49): the two probability identities per venue. Referenced by
            // string because the spot C2 lane's convention is not to re-export the
            // new identity id consts from the crate root.
            "btc.polymarket.probability_momentum",
            "btc.polymarket.probability_volatility",
            "btc.kalshi.probability_momentum",
            "btc.kalshi.probability_volatility",
            // C3b (ruling E51): five always-on TA/timing identities per
            // prediction venue. `btc.kalshi.window_streak` is index-gated with
            // the settlement twins it folds, so it is not in the disarmed set.
            POLYMARKET_RSI_ID,
            POLYMARKET_ROC_ID,
            POLYMARKET_ZSCORE_ID,
            POLYMARKET_TICK_VELOCITY_ID,
            POLYMARKET_EXPECTED_VALUE_ID,
            KALSHI_RSI_ID,
            KALSHI_ROC_ID,
            KALSHI_ZSCORE_ID,
            KALSHI_TICK_VELOCITY_ID,
            KALSHI_EXPECTED_VALUE_ID,
        ])
    );
    for (id, raw) in documents {
        let definition: SignalDefinition =
            serde_json::from_str(raw).unwrap_or_else(|error| panic!("{id}: {error}"));
        definition
            .validate()
            .unwrap_or_else(|error| panic!("{id}: {error}"));
        assert_eq!(definition.version, "1.0.0", "{id}");
        assert_eq!(definition.approximate_interval_ms, 1_000);
        assert_eq!(definition.staleness_bound_ms, 10_000);
    }
}

#[test]
fn polymarket_rules_quotes_liquidity_and_flow_are_exact() {
    let (processor, output) = ready_polymarket("poly-exact");
    assert_eq!(processor.active_market_id(), Some("pm-1"));
    assert_eq!(processor.active_market_generation(), Some(1));
    assert_ready_and_valid(&processor, &output);

    let contract = update(&output, POLYMARKET_MARKET_CONTRACT_ID);
    assert_eq!(contract.values["strike"], json!("60000"));
    assert_eq!(
        contract.values["settlement_benchmark"],
        json!("chainlink_btc_usd_twap_60s")
    );
    assert_eq!(
        contract.values["comparator"],
        json!("greater_than_or_equal")
    );
    assert_eq!(contract.values["tie_outcome"], json!("up"));

    let quote = update(&output, POLYMARKET_PREDICTION_QUOTE_ID);
    assert_eq!(quote.values["affirmative_midpoint"], json!("0.46"));
    assert_eq!(quote.values["negative_midpoint"], json!("0.54"));
    assert_eq!(quote.values["last_trade_outcome"], json!("up"));

    let liquidity = update(&output, POLYMARKET_PREDICTION_LIQUIDITY_ID);
    assert_eq!(liquidity.values["probe_units"], json!("10"));
    assert_eq!(liquidity.values["affirmative_buy_cost"], json!("4.74"));
    assert_eq!(liquidity.values["affirmative_sell_proceeds"], json!("4.50"));
    assert_eq!(liquidity.values["negative_buy_cost"], json!("5.50"));
    assert_eq!(liquidity.values["negative_sell_proceeds"], json!("5.30"));

    let flow = update(&output, POLYMARKET_PREDICTION_TRADE_FLOW_ID);
    assert_eq!(
        flow.values["flow_semantics"],
        json!("provider_reported_side")
    );
    assert_eq!(flow.values["signed_notional_60s"], json!("0.92"));
    assert_eq!(flow.values["signed_units_60s"], json!("2"));
    assert_eq!(flow.values["trade_count_60s"], json!(1));
}

#[test]
fn kalshi_complements_yes_axis_and_signs_no_taker_flow() {
    let (processor, output) = ready_kalshi("kalshi-exact");
    assert_eq!(processor.active_market_id(), Some("KXBTC15M-1"));
    assert_ready_and_valid(&processor, &output);

    let contract = update(&output, KALSHI_MARKET_CONTRACT_ID);
    assert_eq!(contract.values["strike"], json!("60100"));
    assert_eq!(
        contract.values["settlement_benchmark"],
        json!("cf_benchmarks_brti_sixty_print")
    );
    assert_eq!(contract.values["tie_outcome"], json!("yes"));

    let quote = update(&output, KALSHI_PREDICTION_QUOTE_ID);
    assert_eq!(quote.values["affirmative_bid"], json!("0.40"));
    assert_eq!(quote.values["affirmative_ask"], json!("0.42"));
    assert_eq!(quote.values["negative_bid"], json!("0.58"));
    assert_eq!(quote.values["negative_ask"], json!("0.60"));
    assert_eq!(quote.values["last_trade_outcome"], json!("no"));
    assert_eq!(quote.values["last_trade_price"], json!("0.59"));

    let liquidity = update(&output, KALSHI_PREDICTION_LIQUIDITY_ID);
    assert_eq!(liquidity.values["affirmative_buy_cost"], json!("4.24"));
    assert_eq!(liquidity.values["affirmative_sell_proceeds"], json!("4.00"));
    assert_eq!(liquidity.values["negative_buy_cost"], json!("6.00"));
    assert_eq!(liquidity.values["negative_sell_proceeds"], json!("5.76"));
    assert_eq!(liquidity.values["negative_total_bid_depth"], json!("16"));
    assert_eq!(liquidity.values["negative_total_ask_depth"], json!("22"));

    let flow = update(&output, KALSHI_PREDICTION_TRADE_FLOW_ID);
    assert_eq!(
        flow.values["flow_semantics"],
        json!("verified_taker_outcome")
    );
    assert_eq!(flow.values["signed_notional_60s"], json!("-1.77"));
    assert_eq!(flow.values["signed_units_60s"], json!("-3"));
    assert_eq!(flow.values["flow_direction_10s"], json!("negative"));
}

#[test]
fn zero_net_flow_is_published_as_flat() {
    let (mut processor, _) = ready_kalshi("kalshi-flat-flow");
    let mut balancing_trade = kalshi_trade(1, 1, 61);
    balancing_trade
        .values
        .insert("yes_price".to_owned(), json!("0.59"));
    balancing_trade
        .values
        .insert("no_price".to_owned(), json!("0.41"));
    balancing_trade
        .values
        .insert("taker_outcome_side".to_owned(), json!("yes"));
    balancing_trade
        .values
        .insert("taker_book_side".to_owned(), json!("bid"));
    processor
        .ingest(&balancing_trade, ns(61), ns(61))
        .expect("balancing YES trade is admitted");

    let output = processor.advance_time(ns(61)).expect("flat batch emits");
    let flow = update(&output, KALSHI_PREDICTION_TRADE_FLOW_ID);
    assert_eq!(flow.values["signed_notional_10s"], json!("0.00"));
    assert_eq!(flow.values["flow_direction_10s"], json!("flat"));
}

#[test]
fn legal_one_dollar_yes_ask_degrades_invalid_kalshi_complement() {
    let mut processor = PredictionSignalProcessor::new(
        PredictionMarketVenue::Kalshi,
        "kalshi-complement-edge",
        wall_start(),
    )
    .expect("Kalshi processor constructs");
    processor
        .bind_market(1, &kalshi_contract(0, 1, 0), 0, 0)
        .expect("Kalshi market binds");
    for event in [
        kalshi_book(0, "edge-snapshot", 0, 2, "bid", "0.99", "20", 1),
        kalshi_book(1, "edge-snapshot", 1, 2, "ask", "1.00", "20", 1),
    ] {
        processor
            .ingest(&event, ns(1), ns(1))
            .expect("legal YES book row is admitted");
    }

    let output = processor
        .advance_time(ns(1))
        .expect("complement edge is contained instead of failing the run");
    let liquidity = update(&output, KALSHI_PREDICTION_LIQUIDITY_ID);
    assert_eq!(liquidity.quality.state, QualityState::Degraded);
    assert!(liquidity.values.is_empty());
    assert!(
        liquidity
            .quality
            .reasons
            .iter()
            .any(|reason| reason == "kalshi_complement_invalid")
    );
}

#[test]
fn prediction_inputs_are_fresh_through_exactly_ten_seconds() {
    let (mut processor, _) = ready_kalshi("kalshi-staleness-boundary");
    let exact_boundary = processor
        .advance_time(ns(70))
        .expect("exact staleness boundary emits");
    for signal_id in [
        KALSHI_PREDICTION_QUOTE_ID,
        KALSHI_PREDICTION_LIQUIDITY_ID,
        KALSHI_PREDICTION_TRADE_FLOW_ID,
    ] {
        assert_eq!(
            update(&exact_boundary, signal_id).quality.state,
            QualityState::Ready,
            "{signal_id} must remain ready at exactly ten seconds"
        );
    }

    let after_boundary = processor
        .advance_time(ns(70) + 1)
        .expect("post-boundary batch emits");
    for signal_id in [
        KALSHI_PREDICTION_QUOTE_ID,
        KALSHI_PREDICTION_LIQUIDITY_ID,
        KALSHI_PREDICTION_TRADE_FLOW_ID,
    ] {
        assert_eq!(
            update(&after_boundary, signal_id).quality.state,
            QualityState::Degraded,
            "{signal_id} must be stale one nanosecond past the boundary"
        );
    }
}

#[test]
fn rollover_clears_mid_quote_inflight_book_and_trade_state() {
    let (mut processor, _) = ready_polymarket("poly-rollover");
    processor
        .ingest(
            &poly_book(8, "inflight", 0, 2, "up", "bid", "0.46", "20", 100),
            ns(100),
            ns(100),
        )
        .expect("first row of an in-flight group is admitted");
    processor
        .ingest(
            &poly_trade(1, "up", "sell", "0.45", "4", 100),
            ns(100),
            ns(100),
        )
        .expect("trade immediately before handoff is admitted");

    processor
        .ingest(&poly_twap(1, 300, "60100"), ns(300), ns(300))
        .expect("next opening TWAP is retained");
    assert!(
        processor
            .mark_unavailable(ns(300), "scheduled_rollover")
            .expect("departing generation clears")
            .is_empty(),
        "an unbound venue has no instance evaluations"
    );
    processor
        // A fresh per-market adapter restarts its market-source sequence at zero.
        .bind_market(2, &poly_contract(0, 2, 300), ns(300), ns(300))
        .expect("next generation binds");
    let boundary = processor
        .advance_time(ns(301))
        .expect("post-boundary batch emits");

    let contract = update(&boundary, POLYMARKET_MARKET_CONTRACT_ID);
    assert_eq!(contract.quality.state, QualityState::Ready);
    assert_eq!(contract.values["market_id"], json!("pm-2"));
    assert_eq!(contract.values["market_generation"], json!(2));
    for signal_id in [
        POLYMARKET_PREDICTION_QUOTE_ID,
        POLYMARKET_PREDICTION_LIQUIDITY_ID,
        POLYMARKET_PREDICTION_TRADE_FLOW_ID,
    ] {
        let output = update(&boundary, signal_id);
        assert_eq!(output.quality.state, QualityState::Degraded);
        assert!(output.values.is_empty());
    }

    assert!(matches!(
        processor.ingest(
            &poly_quote(2, 1, "up", "0.4", "0.42", 302),
            ns(302),
            ns(302)
        ),
        Err(PredictionSignalError::MarketIdentity { .. })
    ));
}

#[test]
fn close_emits_bound_fail_closed_states_then_departure_clears_the_instance_set() {
    let (mut processor, _) = ready_polymarket("poly-gap");
    let closed = processor
        .advance_time(ns(300))
        .expect("market close is evaluated");
    assert!(closed.iter().all(|output| {
        // Base signals were ready before close and degrade; the composites and
        // the probability pair never warmed (no ticker fixture / no full ring) and
        // stay WarmingUp. Both carry market_closed and omit values.
        let expected_state =
            if is_composite_signal(&output.signal_id) || is_probability_signal(&output.signal_id) {
                QualityState::WarmingUp
            } else {
                QualityState::Degraded
            };
        output.quality.state == expected_state
            && output
                .quality
                .reasons
                .iter()
                .any(|reason| reason == "market_closed")
            && output.values.is_empty()
    }));

    let unavailable = processor
        .mark_unavailable(ns(300), "next_market_not_open")
        .expect("discovery gap is an admitted processor transition");
    assert!(
        unavailable.is_empty(),
        "the unbound gap is market-transition-owned"
    );
    assert_eq!(processor.active_market_id(), None);
}

#[test]
fn departure_before_any_ready_state_still_clears_without_unbound_evaluations() {
    let mut processor = PredictionSignalProcessor::new(
        PredictionMarketVenue::Polymarket,
        "poly-never-ready",
        wall_start(),
    )
    .expect("Polymarket processor constructs");
    processor
        .bind_market(1, &poly_contract(0, 1, 0), 0, 0)
        .expect("market binds without a retained opening TWAP");

    let warming = processor
        .advance_time(ns(1))
        .expect("pre-boundary warming batch emits");
    assert!(
        warming
            .iter()
            .all(|output| output.quality.state == QualityState::WarmingUp)
    );

    let unavailable = processor
        .mark_unavailable(ns(2), "scheduled_rollover")
        .expect("unavailable boundary clears the bound state");
    assert!(unavailable.is_empty());
    assert_eq!(processor.active_market_id(), None);
}

#[test]
fn one_sided_and_crossed_books_degrade_without_terminating_admission() {
    let mut one_sided = PredictionSignalProcessor::new(
        PredictionMarketVenue::Polymarket,
        "poly-one-sided",
        wall_start(),
    )
    .expect("Polymarket processor constructs");
    one_sided
        .ingest(&poly_twap(0, 0, "60000"), 0, 0)
        .expect("opening TWAP is admitted");
    one_sided
        .bind_market(1, &poly_contract(0, 1, 0), 0, 0)
        .expect("market binds");
    one_sided
        .ingest(
            &poly_book(0, "one-sided", 0, 1, "up", "bid", "0.45", "12", 1),
            ns(1),
            ns(1),
        )
        .expect("one-sided provider snapshot fails closed without aborting");
    let one_sided_output = one_sided.advance_time(ns(1)).expect("batch emits");
    assert!(
        update(&one_sided_output, POLYMARKET_PREDICTION_LIQUIDITY_ID)
            .quality
            .reasons
            .iter()
            .any(|reason| reason == "book_one_sided")
    );

    let (mut crossed, _) = ready_polymarket("poly-crossed");
    let mut crossing_delta = poly_book(8, "cross", 0, 1, "up", "bid", "0.60", "1", 62);
    crossing_delta
        .values
        .insert("update_kind".to_owned(), json!("delta"));
    crossed
        .ingest(&crossing_delta, ns(62), ns(62))
        .expect("crossing provider delta fails closed without aborting");
    let invalid = crossed.advance_time(ns(62)).expect("invalid batch emits");
    assert!(
        update(&invalid, POLYMARKET_PREDICTION_LIQUIDITY_ID)
            .quality
            .reasons
            .iter()
            .any(|reason| reason == "book_crossed_or_invalid")
    );

    let mut repair_delta = poly_book(9, "repair", 0, 1, "up", "bid", "0.60", "0", 63);
    repair_delta
        .values
        .insert("update_kind".to_owned(), json!("delta"));
    crossed
        .ingest(&repair_delta, ns(63), ns(63))
        .expect("subsequent absolute delta repairs the book");
    let repaired = crossed.advance_time(ns(63)).expect("repaired batch emits");
    assert_eq!(
        update(&repaired, POLYMARKET_PREDICTION_LIQUIDITY_ID)
            .quality
            .state,
        QualityState::Ready
    );
}

#[test]
fn crossed_quote_degrades_and_recovers_without_terminating_admission() {
    let (mut processor, _) = ready_polymarket("poly-crossed-quote");
    processor
        .ingest(&poly_quote(2, 1, "up", "0.50", "0.40", 62), ns(62), ns(62))
        .expect("crossed provider quote fails closed without aborting");
    let invalid = processor.advance_time(ns(62)).expect("invalid batch emits");
    let quote = update(&invalid, POLYMARKET_PREDICTION_QUOTE_ID);
    assert_eq!(quote.quality.state, QualityState::Degraded);
    assert!(quote.values.is_empty());
    assert!(
        quote
            .quality
            .reasons
            .iter()
            .any(|reason| reason == "affirmative_quote_crossed_or_invalid")
    );

    processor
        .ingest(&poly_quote(3, 1, "up", "0.45", "0.47", 63), ns(63), ns(63))
        .expect("later uncrossed quote repairs the axis");
    let repaired = processor
        .advance_time(ns(63))
        .expect("repaired batch emits");
    assert_eq!(
        update(&repaired, POLYMARKET_PREDICTION_QUOTE_ID)
            .quality
            .state,
        QualityState::Ready
    );
}

#[test]
fn restart_preserves_generation_history_but_not_market_relative_state() {
    let (processor, _) = ready_kalshi("kalshi-before-restart");
    let checkpoint = processor.checkpoint_state();
    let mut restored =
        PredictionSignalProcessor::restore("kalshi-after-restart", wall_start(), checkpoint)
            .expect("checkpoint restores");

    assert_eq!(restored.active_market_id(), None);
    assert_eq!(
        restored
            .reserve_next_generation()
            .expect("generation advances"),
        2
    );
    let first = restored
        .advance_time(ns(61))
        .expect("restart emits unavailable state");
    assert!(first.iter().all(|output| {
        // Base signals restore ever_ready and degrade; composites and the
        // probability pair never warmed and stay WarmingUp. All restart their
        // output sequence at zero with no values.
        let expected_state =
            if is_composite_signal(&output.signal_id) || is_probability_signal(&output.signal_id) {
                QualityState::WarmingUp
            } else {
                QualityState::Degraded
            };
        output.sequence == 0 && output.quality.state == expected_state && output.values.is_empty()
    }));

    restored
        .bind_market(2, &kalshi_contract(0, 1, 0), ns(62), ns(62))
        .expect("restart rebind uses a new generation even for the same ticker");
    let rebound = restored.advance_time(ns(62)).expect("rebound batch emits");
    assert_eq!(
        update(&rebound, KALSHI_MARKET_CONTRACT_ID).values["market_generation"],
        json!(2)
    );
    for signal_id in [
        KALSHI_PREDICTION_QUOTE_ID,
        KALSHI_PREDICTION_LIQUIDITY_ID,
        KALSHI_PREDICTION_TRADE_FLOW_ID,
    ] {
        assert!(update(&rebound, signal_id).values.is_empty());
    }
}

#[test]
fn checkpoint_rejects_inconsistent_persisted_quality() {
    let (processor, _) = ready_kalshi("kalshi-checkpoint-validation");
    let mut checkpoint = processor.checkpoint_state();
    checkpoint
        .features
        .get_mut(KALSHI_PREDICTION_QUOTE_ID)
        .expect("quote checkpoint exists")
        .last_quality
        .as_mut()
        .expect("quote quality exists")
        .reasons = vec!["duplicate".to_owned(), "duplicate".to_owned()];
    assert!(
        PredictionSignalProcessor::restore("kalshi-corrupt-checkpoint", wall_start(), checkpoint)
            .is_err()
    );
}

#[test]
fn identical_prediction_inputs_produce_identical_updates() {
    let (_, left) = ready_polymarket("prediction-replay");
    let (_, right) = ready_polymarket("prediction-replay");
    assert_eq!(
        serde_json::to_vec(left.evaluations()).expect("left output serializes"),
        serde_json::to_vec(right.evaluations()).expect("right output serializes")
    );
    // The coalescing decision is a fold over journaled inputs and the journaled
    // decision clock alone, so the published subset repeats as exactly as the
    // evaluations do.
    assert_eq!(
        serde_json::to_vec(&left.into_published()).expect("left publications serialize"),
        serde_json::to_vec(&right.into_published()).expect("right publications serialize")
    );
}

// SIG-046 quiet-window semantics, consolidated into the single V1 contract.

#[test]
fn kalshi_trade_quiet_window_is_ready_with_zeros_while_alive() {
    let (mut processor, _) = ready_kalshi("kalshi-trade-quiet-alive");
    // No trade since second 60; a fresh quote keeps the venue demonstrably alive
    // well past the ten-second trade horizon and drains the sixty-second window.
    processor
        .ingest(&kalshi_quote(1, 1, 125), ns(125), ns(125))
        .expect("late quote keeps the venue alive");
    let output = processor
        .advance_time(ns(130))
        .expect("quiet-but-alive batch emits");
    let flow = update(&output, KALSHI_PREDICTION_TRADE_FLOW_ID);
    assert_eq!(flow.quality.state, QualityState::Ready);
    assert!(
        !flow
            .quality
            .reasons
            .iter()
            .any(|reason| reason == "trade_stale_or_missing")
    );
    assert_eq!(flow.values["trade_count_60s"], json!(0));
    assert_eq!(flow.values["flow_direction_10s"], json!("flat"));
    assert_eq!(
        flow.values["signed_notional_10s"]
            .as_str()
            .expect("decimal string")
            .parse::<f64>()
            .expect("parses"),
        0.0
    );
}

#[test]
fn polymarket_trade_quiet_window_is_ready_with_zeros_while_alive() {
    let (mut processor, _) = ready_polymarket("poly-trade-quiet-alive");
    processor
        .ingest(
            &poly_quote(2, 1, "up", "0.45", "0.47", 125),
            ns(125),
            ns(125),
        )
        .expect("late quote keeps the venue alive");
    let output = processor
        .advance_time(ns(130))
        .expect("quiet-but-alive batch emits");
    let flow = update(&output, POLYMARKET_PREDICTION_TRADE_FLOW_ID);
    assert_eq!(flow.quality.state, QualityState::Ready);
    assert!(
        !flow
            .quality
            .reasons
            .iter()
            .any(|reason| reason == "trade_stale_or_missing")
    );
    assert_eq!(flow.values["trade_count_60s"], json!(0));
    assert_eq!(flow.values["flow_direction_10s"], json!("flat"));
}

#[test]
fn kalshi_resting_book_republishes_quote_and_liquidity_while_alive() {
    let (mut processor, ready) = ready_kalshi("kalshi-resting-book-alive");
    let quote_bid = update(&ready, KALSHI_PREDICTION_QUOTE_ID).values["affirmative_bid"].clone();
    let book_size =
        update(&ready, KALSHI_PREDICTION_LIQUIDITY_ID).values["affirmative_bid_size"].clone();
    // Only a trade advances; the book and quotes rest fifteen seconds past their
    // own staleness horizon but the venue stays alive.
    processor
        .ingest(&kalshi_trade(1, 1, 75), ns(75), ns(75))
        .expect("late trade keeps the venue alive");
    let output = processor.advance_time(ns(75)).expect("resting batch emits");

    let quote = update(&output, KALSHI_PREDICTION_QUOTE_ID);
    assert_eq!(quote.quality.state, QualityState::Ready);
    assert_eq!(quote.values["affirmative_bid"], quote_bid);
    assert_eq!(quote.values["max_quote_age_ms"], json!(15000));

    let liquidity = update(&output, KALSHI_PREDICTION_LIQUIDITY_ID);
    assert_eq!(liquidity.quality.state, QualityState::Ready);
    assert_eq!(liquidity.values["affirmative_bid_size"], book_size);
    assert_eq!(liquidity.values["max_book_age_ms"], json!(15000));
}

#[test]
fn polymarket_resting_book_republishes_quote_and_liquidity_while_alive() {
    let (mut processor, ready) = ready_polymarket("poly-resting-book-alive");
    let quote_bid =
        update(&ready, POLYMARKET_PREDICTION_QUOTE_ID).values["affirmative_bid"].clone();
    let book_size =
        update(&ready, POLYMARKET_PREDICTION_LIQUIDITY_ID).values["affirmative_bid_size"].clone();
    processor
        .ingest(&poly_trade(1, "up", "buy", "0.46", "2", 75), ns(75), ns(75))
        .expect("late trade keeps the venue alive");
    let output = processor.advance_time(ns(75)).expect("resting batch emits");

    let quote = update(&output, POLYMARKET_PREDICTION_QUOTE_ID);
    assert_eq!(quote.quality.state, QualityState::Ready);
    assert_eq!(quote.values["affirmative_bid"], quote_bid);
    assert_eq!(quote.values["max_quote_age_ms"], json!(15000));

    let liquidity = update(&output, POLYMARKET_PREDICTION_LIQUIDITY_ID);
    assert_eq!(liquidity.quality.state, QualityState::Ready);
    assert_eq!(liquidity.values["affirmative_bid_size"], book_size);
    assert_eq!(liquidity.values["max_book_age_ms"], json!(15000));
}

#[test]
fn silent_kalshi_venue_still_degrades_with_todays_reasons() {
    let (mut processor, _) = ready_kalshi("kalshi-silent-venue");
    // No input after second 60; one nanosecond past the aliveness bound.
    let output = processor
        .advance_time(ns(70) + 1)
        .expect("silent-venue batch emits");
    for signal_id in [
        KALSHI_PREDICTION_QUOTE_ID,
        KALSHI_PREDICTION_LIQUIDITY_ID,
        KALSHI_PREDICTION_TRADE_FLOW_ID,
    ] {
        assert_eq!(
            update(&output, signal_id).quality.state,
            QualityState::Degraded
        );
    }
    assert!(
        update(&output, KALSHI_PREDICTION_TRADE_FLOW_ID)
            .quality
            .reasons
            .iter()
            .any(|reason| reason == "trade_stale_or_missing")
    );
    assert!(
        update(&output, KALSHI_PREDICTION_QUOTE_ID)
            .quality
            .reasons
            .iter()
            .any(|reason| reason == "affirmative_quote_stale")
    );
    assert!(
        update(&output, KALSHI_PREDICTION_LIQUIDITY_ID)
            .quality
            .reasons
            .iter()
            .any(|reason| reason == "book_stale")
    );
}

#[test]
fn silent_polymarket_venue_still_degrades_with_todays_reasons() {
    let (mut processor, _) = ready_polymarket("poly-silent-venue");
    let output = processor
        .advance_time(ns(70) + 1)
        .expect("silent-venue batch emits");
    for signal_id in [
        POLYMARKET_PREDICTION_QUOTE_ID,
        POLYMARKET_PREDICTION_LIQUIDITY_ID,
        POLYMARKET_PREDICTION_TRADE_FLOW_ID,
    ] {
        assert_eq!(
            update(&output, signal_id).quality.state,
            QualityState::Degraded
        );
    }
    assert!(
        update(&output, POLYMARKET_PREDICTION_TRADE_FLOW_ID)
            .quality
            .reasons
            .iter()
            .any(|reason| reason == "trade_stale_or_missing")
    );
    assert!(
        update(&output, POLYMARKET_PREDICTION_LIQUIDITY_ID)
            .quality
            .reasons
            .iter()
            .any(|reason| reason == "book_stale")
    );
}

#[test]
fn twap_admissions_do_not_vouch_for_a_silent_polymarket_venue() {
    let (mut processor, _) = ready_polymarket("poly-twap-no-vouch");
    // The CLOB is silent after second 60 while the settlement relay keeps
    // publishing; a benchmark feed must not hold the order book alive (SIG-046).
    processor
        .ingest(&poly_twap(9, 69, "60050"), ns(69), ns(69))
        .expect("twap admission");
    let output = processor
        .advance_time(ns(70) + 1)
        .expect("silent-venue batch emits");
    for signal_id in [
        POLYMARKET_PREDICTION_QUOTE_ID,
        POLYMARKET_PREDICTION_LIQUIDITY_ID,
        POLYMARKET_PREDICTION_TRADE_FLOW_ID,
    ] {
        assert_eq!(
            update(&output, signal_id).quality.state,
            QualityState::Degraded
        );
    }
}

#[test]
fn a_marked_unavailable_venue_emits_no_unbound_instance_evaluations() {
    let (mut processor, _) = ready_polymarket("poly-down-immediate");
    // Availability is authoritative: a fresh pre-disconnect admission must not
    // let quiet-window publication ride into a known-down venue (SIG-046).
    let output = processor
        .mark_unavailable(ns(61), "socket_disconnected")
        .expect("unavailability clears the bound state");
    assert!(output.is_empty());
    // The unbound gap is market-transition-owned; no fabricated market instance
    // exists one second later either.
    let later = processor
        .advance_time(ns(62))
        .expect("post-down evaluation succeeds");
    assert!(later.is_empty());
}

#[test]
fn aliveness_does_not_mask_a_crossed_book_integrity_fault() {
    let (mut processor, _) = ready_polymarket("poly-crossed-while-alive");
    // The crossing delta is itself an admitted venue input, so the venue is alive
    // at decision time; the integrity fault must still degrade the signal.
    let mut crossing_delta = poly_book(8, "cross", 0, 1, "up", "bid", "0.60", "1", 62);
    crossing_delta
        .values
        .insert("update_kind".to_owned(), json!("delta"));
    processor
        .ingest(&crossing_delta, ns(62), ns(62))
        .expect("crossing delta is admitted");
    let output = processor.advance_time(ns(62)).expect("invalid batch emits");
    let liquidity = update(&output, POLYMARKET_PREDICTION_LIQUIDITY_ID);
    assert_eq!(liquidity.quality.state, QualityState::Degraded);
    assert!(
        liquidity
            .quality
            .reasons
            .iter()
            .any(|reason| reason == "book_crossed_or_invalid")
    );
}

#[test]
fn resumed_activity_updates_values_and_stays_ready_across_the_quiet_period() {
    let (mut processor, ready) = ready_polymarket("poly-recovery");
    let initial_bid =
        update(&ready, POLYMARKET_PREDICTION_QUOTE_ID).values["affirmative_bid"].clone();
    // Activity resumes on the Up axis; the venue stays alive and the value updates.
    processor
        .ingest(&poly_quote(2, 1, "up", "0.50", "0.52", 70), ns(70), ns(70))
        .expect("resumed quote is admitted");
    let resumed = processor.advance_time(ns(70)).expect("resumed batch emits");
    let quote = update(&resumed, POLYMARKET_PREDICTION_QUOTE_ID);
    assert_eq!(quote.quality.state, QualityState::Ready);
    assert_ne!(quote.values["affirmative_bid"], initial_bid);

    // One second later, no new input, still within the aliveness bound.
    let quiet = processor.advance_time(ns(71)).expect("quiet batch emits");
    assert_eq!(
        update(&quiet, POLYMARKET_PREDICTION_QUOTE_ID).quality.state,
        QualityState::Ready
    );
}

/// The two venues publish different identity sets since the 2026-09-02
/// adoption: the settlement-anchor readings are Polymarket's alone because
/// Kalshi resolves on the rights-gated CF Benchmarks BRTI. Every count in the
/// pipeline is derived from these sets, so this is the drift guard for all of
/// them.
#[test]
fn per_venue_expected_sets_are_asymmetric() {
    let polymarket = prediction_signal_ids(PredictionMarketVenue::Polymarket);
    let kalshi = prediction_signal_ids(PredictionMarketVenue::Kalshi);
    // C2 (E49) added the two probability momentum/volatility identities on both
    // venues (Polymarket 13 -> 15, Kalshi 11 -> 13); C3b (ruling E51) then added
    // five to each always-on set: Polymarket 15 -> 20, Kalshi 13 -> 18.
    assert_eq!(polymarket.len(), 20);
    assert_eq!(kalshi.len(), 18);
    assert_eq!(
        polymarket.len(),
        prediction_signal_count(PredictionMarketVenue::Polymarket)
    );
    assert_eq!(
        kalshi.len(),
        prediction_signal_count(PredictionMarketVenue::Kalshi)
    );

    let polymarket_extra = polymarket
        .iter()
        .filter(|id| !kalshi.contains(&id.replace("btc.polymarket.", "btc.kalshi.").as_str()))
        .copied()
        .collect::<BTreeSet<_>>();
    assert_eq!(
        polymarket_extra,
        BTreeSet::from([
            // The C3b always-on TA/timing identities are symmetric across both
            // venues, so they are not in the asymmetry; only Polymarket's
            // always-on settlement twins (index-gated on Kalshi) are extra.
            POLYMARKET_SETTLEMENT_ANCHOR_STATE_ID,
            POLYMARKET_ORACLE_DIVERGENCE_ID,
        ])
    );

    // A processor builds exactly its venue's set, and neither leaks into the
    // other.
    for (venue, ids) in [
        (PredictionMarketVenue::Polymarket, &polymarket),
        (PredictionMarketVenue::Kalshi, &kalshi),
    ] {
        let processor = PredictionSignalProcessor::new(venue, "venue-set", wall_start())
            .expect("processor constructs");
        assert_eq!(
            processor
                .definitions()
                .map(|definition| definition.signal_id.as_str())
                .collect::<BTreeSet<_>>(),
            ids.iter().copied().collect::<BTreeSet<_>>(),
            "{venue:?}"
        );
    }
}

/// The settlement-anchor reading on the ready Polymarket fixture: the opening
/// TWAP is both the anchor and the rules-card threshold, so the distance is
/// exactly zero and the tie resolves to the contract's `up` outcome. The window
/// closes at second 300 and the decision is at second 61, so 239_000 ms remain
/// — outside the 60_000 ms pin window, which keeps the state `tracking` even at
/// zero distance.
#[test]
fn polymarket_publishes_the_settlement_anchor_state() {
    let (_, output) = ready_polymarket("poly-settlement-anchor");
    let anchor = update(&output, POLYMARKET_SETTLEMENT_ANCHOR_STATE_ID);
    assert_eq!(anchor.quality.state, QualityState::Ready);
    assert_eq!(anchor.values["anchor_price"], json!("60000"));
    assert_eq!(anchor.values["threshold"], json!("60000"));
    assert_eq!(anchor.values["distance_bps"], json!("0"));
    assert_eq!(anchor.values["time_to_close_ms"], json!(239_000));
    assert_eq!(anchor.values["comparator"], json!("greater_than_or_equal"));
    assert_eq!(anchor.values["side_if_settled_now"], json!("up"));
    assert_eq!(anchor.values["state"], json!("tracking"));
    assert_eq!(anchor.values["pin_threshold_bps"], json!("10"));
    assert_eq!(anchor.values["pin_window_ms"], json!(60_000));
}

/// Inside the pin window and inside the distance band, the same reading becomes
/// `pinned`: the market is 30 seconds from close with the anchor exactly on the
/// threshold.
#[test]
fn settlement_anchor_reports_pinned_inside_the_resolution_window() {
    let (mut processor, _) = ready_polymarket("poly-settlement-pinned");
    let batch = processor
        .advance_time(ns(270))
        .expect("late-window batch emits");
    let anchor = update(&batch, POLYMARKET_SETTLEMENT_ANCHOR_STATE_ID);
    assert_eq!(anchor.quality.state, QualityState::Ready);
    assert_eq!(anchor.values["time_to_close_ms"], json!(30_000));
    assert_eq!(anchor.values["state"], json!("pinned"));
}

/// The closed window has no resolution-sensitivity context left, so the reading
/// fails closed rather than restating the last distance.
#[test]
fn settlement_anchor_fails_closed_once_the_window_closes() {
    let (mut processor, _) = ready_polymarket("poly-settlement-closed");
    let batch = processor.advance_time(ns(300)).expect("closed batch emits");
    let anchor = update(&batch, POLYMARKET_SETTLEMENT_ANCHOR_STATE_ID);
    assert_ne!(anchor.quality.state, QualityState::Ready);
    assert!(anchor.values.is_empty());
    assert!(anchor.quality.reasons.contains(&"market_closed".to_owned()));
}

/// Oracle divergence needs the hosted spot reference, so it fails closed on the
/// market-only fixture and becomes ready once the reference warms. The warm run
/// holds spot at the 60_000 anchor, so the divergence is exactly zero and the
/// state is `aligned`.
#[test]
fn polymarket_publishes_the_oracle_divergence_once_spot_is_warm() {
    let (_, cold) = ready_polymarket("poly-oracle-divergence-cold");
    let divergence = update(&cold, POLYMARKET_ORACLE_DIVERGENCE_ID);
    assert_ne!(divergence.quality.state, QualityState::Ready);
    assert!(divergence.values.is_empty());
    assert!(
        divergence
            .quality
            .reasons
            .contains(&"reference_unavailable".to_owned())
    );

    let warm = warm_polymarket_anchor("poly-oracle-divergence-warm", "60000");
    let divergence = update(&warm, POLYMARKET_ORACLE_DIVERGENCE_ID);
    assert_eq!(divergence.quality.state, QualityState::Ready);
    assert_eq!(divergence.values["anchor_price"], json!("60000"));
    assert_eq!(divergence.values["spot_reference_mid"], json!("60000"));
    assert_eq!(divergence.values["divergence_bps"], json!("0"));
    assert_eq!(divergence.values["state"], json!("aligned"));
    assert_eq!(divergence.values["diverged_threshold_bps"], json!("10"));
}

/// A spot reference far from the anchor crosses the 10 bps band: 60_000 against
/// a 60_120 reference is -20 bps, so the state is `diverged`.
#[test]
fn oracle_divergence_reports_diverged_beyond_the_threshold() {
    let warm = warm_polymarket_anchor("poly-oracle-diverged", "60120");
    let divergence = update(&warm, POLYMARKET_ORACLE_DIVERGENCE_ID);
    assert_eq!(divergence.quality.state, QualityState::Ready);
    assert_eq!(divergence.values["spot_reference_mid"], json!("60120"));
    // (60000 - 60120) / 60120 * 10^4 = -19.96007984 bps at eight places.
    assert_eq!(divergence.values["divergence_bps"], json!("-19.96007984"));
    assert_eq!(divergence.values["state"], json!("diverged"));
}

/// A Kalshi processor never carries either settlement-anchor identity, so its
/// batch and its checkpoint stay at eleven features.
#[test]
fn kalshi_publishes_neither_settlement_anchor_identity() {
    let (processor, output) = ready_kalshi("kalshi-no-settlement-anchor");
    assert_eq!(
        output.len(),
        prediction_signal_count(PredictionMarketVenue::Kalshi)
    );
    assert!(output.iter().all(
        |update| !update.signal_id.ends_with(".settlement_anchor_state")
            && !update.signal_id.ends_with(".oracle_divergence")
    ));
    assert_eq!(
        processor.checkpoint_state().features.len(),
        prediction_signal_count(PredictionMarketVenue::Kalshi)
    );
}

/// A Polymarket checkpoint carries all thirteen features, restores, and the
/// restored processor keeps publishing its own venue set.
#[test]
fn polymarket_restart_restores_the_settlement_anchor_features() {
    let (processor, _) = ready_polymarket("poly-settlement-before-restart");
    let checkpoint = processor.checkpoint_state();
    assert_eq!(
        checkpoint.features.len(),
        prediction_signal_count(PredictionMarketVenue::Polymarket)
    );
    for id in [
        POLYMARKET_SETTLEMENT_ANCHOR_STATE_ID,
        POLYMARKET_ORACLE_DIVERGENCE_ID,
    ] {
        assert!(checkpoint.features.contains_key(id), "{id}");
    }
    let mut restored = PredictionSignalProcessor::restore(
        "poly-settlement-after-restart",
        wall_start(),
        checkpoint,
    )
    .expect("checkpoint restores");
    restored
        .bind_market(2, &poly_contract(0, 2, 300), ns(300), ns(300))
        .expect("restart rebind uses a new generation");
    let rebound = restored.advance_time(ns(301)).expect("rebound batch emits");
    assert_eq!(
        rebound.len(),
        prediction_signal_count(PredictionMarketVenue::Polymarket)
    );
    // The anchor retained across the handoff is the opening TWAP of the first
    // market; the second market's rules card has no strike yet, so the reading
    // fails closed on the contract rather than on the anchor.
    let anchor = update(&rebound, POLYMARKET_SETTLEMENT_ANCHOR_STATE_ID);
    assert_ne!(anchor.quality.state, QualityState::Ready);
    assert!(anchor.values.is_empty());
}

/// A checkpoint written by one venue cannot restore into the other: the feature
/// identity set is the venue's expected set, not a shared count.
#[test]
fn a_kalshi_checkpoint_never_restores_a_polymarket_feature_set() {
    let (kalshi, _) = ready_kalshi("kalshi-cross-venue");
    let mut checkpoint = kalshi.checkpoint_state();
    checkpoint.venue = PredictionMarketVenue::Polymarket;
    assert!(
        PredictionSignalProcessor::restore("cross-venue", wall_start(), checkpoint).is_err(),
        "an eleven-feature checkpoint must not restore as a thirteen-feature venue"
    );
}

/// A bound Polymarket market whose settlement anchor is the 60_000 opening TWAP,
/// with the hosted spot reference warmed to `spot` at second 75. Returns the
/// batch emitted on that tick.
fn warm_polymarket_anchor(epoch: &str, spot: &str) -> PredictionBatch {
    let mut processor =
        PredictionSignalProcessor::new(PredictionMarketVenue::Polymarket, epoch, wall_start())
            .expect("Polymarket processor constructs");
    processor
        .ingest(&poly_twap(0, 0, "60000"), 0, 0)
        .expect("opening TWAP is admitted");
    processor
        .bind_market(1, &poly_contract(0, 1, 0), 0, 0)
        .expect("Polymarket market binds");
    warm_spot(&mut processor, "60000", spot, 75)
}

fn ready_polymarket(epoch: &str) -> (PredictionSignalProcessor, PredictionBatch) {
    let mut processor =
        PredictionSignalProcessor::new(PredictionMarketVenue::Polymarket, epoch, wall_start())
            .expect("Polymarket processor constructs");
    processor
        .ingest(&poly_twap(0, 0, "60000"), 0, 0)
        .expect("opening TWAP is admitted");
    processor
        .bind_market(1, &poly_contract(0, 1, 0), 0, 0)
        .expect("Polymarket market binds");
    for event in [
        poly_quote(0, 1, "up", "0.45", "0.47", 60),
        poly_quote(1, 1, "down", "0.53", "0.55", 60),
    ] {
        processor
            .ingest(&event, ns(60), ns(60))
            .expect("native quote is admitted");
    }
    for event in [
        poly_book(0, "up-snapshot", 0, 4, "up", "bid", "0.45", "12", 60),
        poly_book(1, "up-snapshot", 1, 4, "up", "bid", "0.44", "8", 60),
        poly_book(2, "up-snapshot", 2, 4, "up", "ask", "0.47", "6", 60),
        poly_book(3, "up-snapshot", 3, 4, "up", "ask", "0.48", "10", 60),
        poly_book(4, "down-snapshot", 0, 4, "down", "bid", "0.53", "12", 60),
        poly_book(5, "down-snapshot", 1, 4, "down", "bid", "0.52", "8", 60),
        poly_book(6, "down-snapshot", 2, 4, "down", "ask", "0.55", "12", 60),
        poly_book(7, "down-snapshot", 3, 4, "down", "ask", "0.56", "8", 60),
    ] {
        processor
            .ingest(&event, ns(60), ns(60))
            .expect("native book row is admitted");
    }
    processor
        .ingest(&poly_trade(0, "up", "buy", "0.46", "2", 60), ns(60), ns(60))
        .expect("Polymarket trade is admitted");
    let output = processor
        .advance_time(ns(61))
        .expect("Polymarket ready batch emits");
    (processor, output)
}

fn ready_kalshi(epoch: &str) -> (PredictionSignalProcessor, PredictionBatch) {
    let mut processor =
        PredictionSignalProcessor::new(PredictionMarketVenue::Kalshi, epoch, wall_start())
            .expect("Kalshi processor constructs");
    processor
        .bind_market(1, &kalshi_contract(0, 1, 0), 0, 0)
        .expect("Kalshi market binds");
    processor
        .ingest(&kalshi_quote(0, 1, 60), ns(60), ns(60))
        .expect("Kalshi quote is admitted");
    for event in [
        kalshi_book(0, "yes-snapshot", 0, 4, "bid", "0.40", "12", 60),
        kalshi_book(1, "yes-snapshot", 1, 4, "bid", "0.39", "10", 60),
        kalshi_book(2, "yes-snapshot", 2, 4, "ask", "0.42", "6", 60),
        kalshi_book(3, "yes-snapshot", 3, 4, "ask", "0.43", "10", 60),
    ] {
        processor
            .ingest(&event, ns(60), ns(60))
            .expect("Kalshi book row is admitted");
    }
    processor
        .ingest(&kalshi_trade(0, 1, 60), ns(60), ns(60))
        .expect("Kalshi trade is admitted");
    let output = processor
        .advance_time(ns(61))
        .expect("Kalshi ready batch emits");
    (processor, output)
}

/// A composite signal (ADR 0016) requires warmed embedded spot state (a ready
/// reference and 61 volatility samples). The market-only fixtures here never feed
/// ticker events, so the four composites are correctly non-ready (fail closed with
/// empty values) while the four base signals are Ready.
/// A signal whose readiness needs warmed spot state (the hosted reference and
/// its volatility ring), so it fails closed while only venue inputs exist.
/// `venue_repricing_lag` joins them: its lag is measured from a spot move, so it
/// cannot complete a measurement without the reference either.
/// Identities these fixtures can never bring to ready.
///
/// Two shapes, both "not ready in this fixture" rather than a fault: the
/// composites need a warmed hosted spot reference the fixtures do not feed, and
/// the C3b TA and timing identities need more history than a fixture spanning a
/// few seconds provides (RSI fifteen closed ten-second bars, the z-score a
/// complete one-minute window, ROC a ten-second-old sample, tick velocity ten
/// seconds of observation).
fn is_composite_signal(signal_id: &str) -> bool {
    signal_id.ends_with(".rsi")
        || signal_id.ends_with(".roc")
        || signal_id.ends_with(".zscore")
        || signal_id.ends_with(".tick_velocity")
        || signal_id.ends_with(".expected_value")
        || signal_id.ends_with(".window_delta")
        || signal_id.ends_with(".standardized_moneyness")
        || signal_id.ends_with(".fair_up_probability")
        || signal_id.ends_with(".executable_edge")
        || signal_id.ends_with(".tie_rule_context")
        || signal_id.ends_with(".venue_repricing_lag")
        // `oracle_divergence` compares the settlement anchor against the hosted
        // spot reference, so it fails closed for the same reason.
        || signal_id.ends_with(".oracle_divergence")
}

/// C2 (E49): the venue's probability momentum/volatility pair. Its ring needs a
/// full warm-up of contiguous one-second `affirmative_midpoint` samples — 301 on
/// Kalshi (10 s / 1 m / 5 m), 121 on Polymarket (10 s / 30 s / 2 m) — which the
/// market-only fixtures never drive, so it is warming (no values) there. When a
/// fixture DOES warm it, `assert_ready_and_valid` validates the populated update
/// against its definition rather than asserting it empty.
fn is_probability_signal(signal_id: &str) -> bool {
    signal_id.ends_with(".probability_momentum") || signal_id.ends_with(".probability_volatility")
}

fn assert_ready_and_valid(processor: &PredictionSignalProcessor, output: &[SignalUpdate]) {
    let definitions = processor
        .definitions()
        .map(|definition| (definition.signal_id.clone(), definition.clone()))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(output.len(), prediction_signal_count(processor.venue()));
    for update in output {
        if is_probability_signal(&update.signal_id) {
            // Ready once the ring is warm, warming otherwise. Either way the
            // update is validated against its definition below; a populated
            // update (warm ring) exercises the field-name/type/8-dp contract.
            if update.quality.state == QualityState::Ready {
                assert!(!update.values.is_empty(), "{}", update.signal_id);
            } else {
                assert!(update.values.is_empty(), "{}", update.signal_id);
            }
        } else if is_composite_signal(&update.signal_id) {
            // Fail closed without warmed spot state: never Ready, no values.
            assert_ne!(
                update.quality.state,
                QualityState::Ready,
                "{}",
                update.signal_id
            );
            assert!(update.values.is_empty(), "{}", update.signal_id);
        } else {
            assert_eq!(
                update.quality.state,
                QualityState::Ready,
                "{}",
                update.signal_id
            );
        }
        update
            .validate_against(
                definitions
                    .get(&update.signal_id)
                    .expect("output has an embedded definition"),
            )
            .expect("output validates against its definition");
    }
}

/// The processor's embedded definitions, keyed by signal id.
fn definitions_of(processor: &PredictionSignalProcessor) -> BTreeMap<String, SignalDefinition> {
    processor
        .definitions()
        .map(|definition| (definition.signal_id.clone(), definition.clone()))
        .collect()
}

/// Assert the named probability updates are Ready with populated values and that
/// each validates against its definition JSON via the same `validate_against`
/// path `assert_ready_and_valid` uses — the field-name/type/8-dp contract, now
/// exercised on a populated probability update rather than an empty one.
fn assert_probability_updates_valid(
    definitions: &BTreeMap<String, SignalDefinition>,
    updates: &[SignalUpdate],
    ids: [&str; 2],
) {
    for id in ids {
        let update = update(updates, id);
        assert_eq!(update.quality.state, QualityState::Ready, "{id}");
        assert!(!update.values.is_empty(), "{id}");
        update
            .validate_against(definitions.get(id).expect("definition is present"))
            .expect("populated probability update validates against its definition");
    }
}

fn update<'a>(updates: &'a [SignalUpdate], signal_id: &str) -> &'a SignalUpdate {
    updates
        .iter()
        .find(|update| update.signal_id == signal_id)
        .unwrap_or_else(|| panic!("missing output for {signal_id}"))
}

fn wall_start() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-08-15T12:00:00Z")
        .expect("wall start parses")
        .with_timezone(&Utc)
}

fn timestamp(second: u64) -> String {
    wall_start()
        .checked_add_signed(Duration::seconds(
            i64::try_from(second).expect("test second fits i64"),
        ))
        .expect("timestamp is representable")
        .to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn ns(second: u64) -> u64 {
    second * ONE_SECOND_NS
}

fn coinbase_ticker(sequence: u64, price: &str, second: u64) -> SourceEvent {
    SourceEvent {
        schema_version: 1,
        source_id: COINBASE_TICKER_SOURCE_ID.to_owned(),
        source_version: COINBASE_TICKER_SOURCE_VERSION.to_owned(),
        publisher_epoch: "coinbase-feed".to_owned(),
        sequence,
        event_time: timestamp(second),
        received_at: timestamp(second),
        values: BTreeMap::from([
            ("instrument".to_owned(), json!("BTC-USD")),
            ("trade_id".to_owned(), json!(100 + sequence)),
            ("price".to_owned(), json!(price)),
            ("last_size".to_owned(), json!("0.01")),
            ("best_bid".to_owned(), json!(price)),
            ("best_bid_size".to_owned(), json!("1.25")),
            ("best_ask".to_owned(), json!(price)),
            ("best_ask_size".to_owned(), json!("2.5")),
            ("open_24h".to_owned(), json!("59800")),
            ("volume_24h".to_owned(), json!("1250.5")),
            ("low_24h".to_owned(), json!("59000")),
            ("high_24h".to_owned(), json!("61000")),
            ("volume_30d".to_owned(), json!("30000")),
            ("event_time".to_owned(), json!(timestamp(second))),
            ("source_sequence".to_owned(), json!(900 + sequence)),
        ]),
    }
}

fn kraken_ticker(sequence: u64, price: &str, second: u64) -> SourceEvent {
    SourceEvent {
        schema_version: 1,
        source_id: KRAKEN_TICKER_SOURCE_ID.to_owned(),
        source_version: KRAKEN_TICKER_SOURCE_VERSION.to_owned(),
        publisher_epoch: "kraken-feed".to_owned(),
        sequence,
        event_time: timestamp(second),
        received_at: timestamp(second),
        values: BTreeMap::from([
            ("instrument".to_owned(), json!("BTC/USD")),
            ("best_bid".to_owned(), json!(price)),
            ("best_bid_size".to_owned(), json!("0.02")),
            ("best_ask".to_owned(), json!(price)),
            ("best_ask_size".to_owned(), json!("0.17")),
            ("last_price".to_owned(), json!(price)),
            ("volume_24h".to_owned(), json!("973.7")),
            ("vwap_24h".to_owned(), json!("60010")),
            ("low_24h".to_owned(), json!("59000")),
            ("high_24h".to_owned(), json!("61000")),
            ("change_24h".to_owned(), json!("-72.9")),
            ("change_percent_24h".to_owned(), json!("-0.11")),
            ("trade_count_24h".to_owned(), json!(46574)),
            ("update_kind".to_owned(), json!("update")),
            ("event_time".to_owned(), json!(timestamp(second))),
        ]),
    }
}

/// Round-A smoke test: a composite emits. With warm embedded spot state (a ready
/// reference and a full volatility ring) and the current reference exactly on the
/// strike, the standardized moneyness d is zero and fair_up_probability is exactly
/// 0.5 (phi_cdf(0)) on a bound Kalshi market.
#[test]
fn strike_equal_reference_yields_fair_up_probability_one_half() {
    let mut processor =
        PredictionSignalProcessor::new(PredictionMarketVenue::Kalshi, "kalshi-smoke", wall_start())
            .expect("Kalshi processor constructs");
    // Strike is the rules card reference_value of 60100.
    processor
        .bind_market(1, &kalshi_contract(0, 1, 0), 0, 0)
        .expect("Kalshi market binds");

    // Warm the internally hosted spot pipeline over 65 one-second health ticks.
    // Prices alternate so realized volatility is strictly positive, but the final
    // reference lands exactly on the 60100 strike so d collapses to zero.
    let mut batch = PredictionBatch::default();
    for second in 1..=65 {
        let price = if second % 2 == 0 { "60110" } else { "60100" };
        processor
            .ingest(
                &coinbase_ticker(second, price, second),
                ns(second),
                ns(second),
            )
            .expect("coinbase ticker admits");
        processor
            .ingest(
                &kraken_ticker(second, price, second),
                ns(second),
                ns(second),
            )
            .expect("kraken ticker admits");
        batch = processor
            .health_tick(ns(second))
            .expect("health tick emits a batch");
    }

    // The final tick (second 65, odd) carries reference 60100 == strike.
    let window_delta = update(&batch, KALSHI_WINDOW_DELTA_ID);
    assert_eq!(window_delta.quality.state, QualityState::Ready);
    assert_eq!(window_delta.values["reference_price"], json!("60100"));
    assert_eq!(window_delta.values["strike"], json!("60100"));
    assert_eq!(window_delta.values["delta_absolute"], json!("0"));

    let moneyness = update(&batch, KALSHI_STANDARDIZED_MONEYNESS_ID);
    assert_eq!(moneyness.quality.state, QualityState::Ready);
    assert_eq!(moneyness.values["moneyness"], json!("0"));

    let fair_up = update(&batch, KALSHI_FAIR_UP_PROBABILITY_ID);
    assert_eq!(fair_up.quality.state, QualityState::Ready);
    assert_eq!(fair_up.values["moneyness"], json!("0"));
    assert_eq!(fair_up.values["probability"], json!("0.5"));
    assert_eq!(fair_up.values["reference_price"], json!("60100"));
    assert_eq!(fair_up.values["strike"], json!("60100"));

    // Tie-rule context on the same decision: |d| = 0 is inside the 0.1 band, so
    // the reading is near a tie and the Kalshi `yes` tie outcome normalizes to
    // the canonical `up` advantage side.
    let tie_rule = update(&batch, KALSHI_TIE_RULE_CONTEXT_ID);
    assert_eq!(tie_rule.quality.state, QualityState::Ready);
    assert_eq!(tie_rule.values["tie_outcome"], json!("up"));
    assert_eq!(tie_rule.values["near_tie"], json!(true));
    assert_eq!(tie_rule.values["tie_advantage_side"], json!("up"));
    assert_eq!(tie_rule.values["near_tie_threshold"], json!("0.1"));
    assert_eq!(
        tie_rule.values["observation_time"],
        moneyness.values["observation_time"]
    );
}

/// Warm the internally hosted spot pipeline on a bound processor. Feed both venue
/// tickers at `base` for seconds 1..`jump`, then `spot` at second `jump`, running a
/// one-second health tick after each (the cadence that samples the realized-
/// volatility ring). Returns the batch emitted at the jump tick.
///
/// With `jump >= 75` the ring holds its full 61 samples, so realized volatility is
/// ready; the last fifteen one-second returns are then fourteen zeros (the constant
/// `base` run) plus the single `base -> spot` jump, which makes the short-window
/// RMS exactly `|jump_return_bps| / sqrt(15)` — the sigma the reference cases pin.
fn warm_spot(
    processor: &mut PredictionSignalProcessor,
    base: &str,
    spot: &str,
    jump: u64,
) -> PredictionBatch {
    let mut batch = PredictionBatch::default();
    for second in 1..=jump {
        let price = if second == jump { spot } else { base };
        processor
            .ingest(
                &coinbase_ticker(second, price, second),
                ns(second),
                ns(second),
            )
            .expect("coinbase ticker admits");
        processor
            .ingest(
                &kraken_ticker(second, price, second),
                ns(second),
                ns(second),
            )
            .expect("kraken ticker admits");
        batch = processor
            .health_tick(ns(second))
            .expect("health tick emits a batch");
    }
    batch
}

fn as_decimal(value: &Value) -> rust_decimal::Decimal {
    value
        .as_str()
        .expect("decimal string")
        .parse()
        .expect("decimal parses")
}

// ADR 0016 composite reference case (Kalshi). A hand-computed instance of the full
// four-signal chain, derived with Python mpmath at 60 digits replicating the pinned
// kernel formulas (atanh-series ln, Newton sqrt, Abramowitz-Stegun 7.1.26 phi) and
// rounded half-even to eight places, per the decimal_math test convention.
//
// Construction: strike K = 60100 (rules card reference_value). The reference ramps
// from 60000 to S = 60600 on the final one-second tick, so the last fifteen returns
// are fourteen zeros and one +100 bps step. Therefore
//   short_bps            = sqrt(100^2 / 15) = 100 / sqrt(15) = 25.819888974716...
//   sigma_per_second     = short_bps / 10^4 = 0.0025819888974716...   -> 0.00258199
//   tau                  = 900 - 75 = 825 s
//   S/K                  = 60600 / 60100 = 1.00831946755...
//   d = ln(S/K)/(sigma*sqrt(tau)) = 0.0082853.../(0.00258199*28.72281...) = 0.11171561...
//   phi_cdf(d) [A-S 7.1.26]        = 0.54447560...
//   delta_absolute       = 60600 - 60100 = 500
//   delta_bps            = 500/60100 * 10^4 = 83.194675540...          -> 83.19467554
#[test]
fn kalshi_composite_chain_matches_mpmath_reference() {
    let mut processor = PredictionSignalProcessor::new(
        PredictionMarketVenue::Kalshi,
        "kalshi-reference",
        wall_start(),
    )
    .expect("Kalshi processor constructs");
    processor
        .bind_market(1, &kalshi_contract(0, 1, 0), 0, 0)
        .expect("Kalshi market binds");
    let batch = warm_spot(&mut processor, "60000", "60600", 75);

    let window_delta = update(&batch, KALSHI_WINDOW_DELTA_ID);
    assert_eq!(window_delta.quality.state, QualityState::Ready);
    assert_eq!(window_delta.values["reference_price"], json!("60600"));
    assert_eq!(window_delta.values["strike"], json!("60100"));
    assert_eq!(window_delta.values["delta_absolute"], json!("500"));
    assert_eq!(window_delta.values["delta_bps"], json!("83.19467554"));

    let moneyness = update(&batch, KALSHI_STANDARDIZED_MONEYNESS_ID);
    assert_eq!(moneyness.quality.state, QualityState::Ready);
    assert_eq!(moneyness.values["moneyness"], json!("0.11171561"));
    assert_eq!(moneyness.values["sigma_per_second"], json!("0.00258199"));
    assert_eq!(moneyness.values["tau_seconds"], json!("825"));

    let fair_up = update(&batch, KALSHI_FAIR_UP_PROBABILITY_ID);
    assert_eq!(fair_up.quality.state, QualityState::Ready);
    assert_eq!(fair_up.values["probability"], json!("0.54447560"));
    assert_eq!(fair_up.values["moneyness"], json!("0.11171561"));
    assert_eq!(fair_up.values["reference_price"], json!("60600"));
    assert_eq!(fair_up.values["strike"], json!("60100"));

    // The complementary tie-rule branch: |d| = 0.11171561 is outside the 0.1
    // band, so the reading is not near a tie and declares no advantage side.
    let tie_rule = update(&batch, KALSHI_TIE_RULE_CONTEXT_ID);
    assert_eq!(tie_rule.quality.state, QualityState::Ready);
    assert_eq!(tie_rule.values["tie_outcome"], json!("up"));
    assert_eq!(tie_rule.values["near_tie"], json!(false));
    assert_eq!(tie_rule.values["tie_advantage_side"], json!("none"));
}

// ADR 0016 composite reference case (Polymarket). Same derivation method and sigma
// as the Kalshi case, but K = 60000 (opening TWAP strike), a 300 s window (tau =
// 300 - 75 = 225 s), and S = 60600 so S/K = 1.01 exactly.
//   d = ln(1.01)/(0.0025819888974716*sqrt(225)) = 0.25691644...
//   phi_cdf(d)          = 0.60137831...
//   delta_absolute       = 600 ; delta_bps = 600/60000*10^4 = 100
#[test]
fn polymarket_composite_chain_matches_mpmath_reference() {
    let mut processor = PredictionSignalProcessor::new(
        PredictionMarketVenue::Polymarket,
        "poly-reference",
        wall_start(),
    )
    .expect("Polymarket processor constructs");
    processor
        .ingest(&poly_twap(0, 0, "60000"), 0, 0)
        .expect("opening TWAP is admitted");
    processor
        .bind_market(1, &poly_contract(0, 1, 0), 0, 0)
        .expect("Polymarket market binds");
    let batch = warm_spot(&mut processor, "60000", "60600", 75);

    let window_delta = update(&batch, POLYMARKET_WINDOW_DELTA_ID);
    assert_eq!(window_delta.quality.state, QualityState::Ready);
    assert_eq!(window_delta.values["reference_price"], json!("60600"));
    assert_eq!(window_delta.values["strike"], json!("60000"));
    assert_eq!(window_delta.values["delta_absolute"], json!("600"));
    assert_eq!(window_delta.values["delta_bps"], json!("100.00"));

    let moneyness = update(&batch, POLYMARKET_STANDARDIZED_MONEYNESS_ID);
    assert_eq!(moneyness.quality.state, QualityState::Ready);
    assert_eq!(moneyness.values["moneyness"], json!("0.25691644"));
    assert_eq!(moneyness.values["sigma_per_second"], json!("0.00258199"));
    assert_eq!(moneyness.values["tau_seconds"], json!("225"));

    let fair_up = update(&batch, POLYMARKET_FAIR_UP_PROBABILITY_ID);
    assert_eq!(fair_up.quality.state, QualityState::Ready);
    assert_eq!(fair_up.values["probability"], json!("0.60137831"));
    assert_eq!(fair_up.values["moneyness"], json!("0.25691644"));
}

/// Warm a bound processor to ready composites and then admit one fresh venue quote
/// at the final tick, so `executable_edge` has both a warmed model probability and a
/// live executable price on each side. Returns the batch that prices the edge.
fn warm_with_fresh_quote(
    processor: &mut PredictionSignalProcessor,
    quote: &SourceEvent,
    jump: u64,
) -> PredictionBatch {
    for second in 1..jump {
        processor
            .ingest(
                &coinbase_ticker(second, "60000", second),
                ns(second),
                ns(second),
            )
            .expect("coinbase ticker admits");
        processor
            .ingest(
                &kraken_ticker(second, "60000", second),
                ns(second),
                ns(second),
            )
            .expect("kraken ticker admits");
        processor
            .health_tick(ns(second))
            .expect("health tick emits");
    }
    processor
        .ingest(&coinbase_ticker(jump, "60600", jump), ns(jump), ns(jump))
        .expect("coinbase jump ticker admits");
    processor
        .ingest(&kraken_ticker(jump, "60600", jump), ns(jump), ns(jump))
        .expect("kraken jump ticker admits");
    processor
        .ingest(quote, ns(jump), ns(jump))
        .expect("fresh venue quote admits");
    processor
        .health_tick(ns(jump))
        .expect("final health tick emits")
}

// ADR 0016 both-sides gross edge (Kalshi complement). With the reference case's
// warmed model probability (0.54447560, S = 60600, K = 60100, tau = 825) and the
// standard Kalshi quote (YES bid 0.40 / ask 0.42, so the NO complement is bid 0.58 /
// ask 0.60):
//   executable_up_price   = affirmative (YES) ask   = 0.42
//   executable_down_price = negative (NO) ask        = 1 - 0.40 = 0.60
//   edge_up   = probability - 0.42               = 0.54447560 - 0.42 =  0.12447560
//   edge_down = (1 - probability) - 0.60         = 0.45552440 - 0.60 = -0.14447560
#[test]
fn kalshi_executable_edge_prices_both_sides_with_the_no_complement() {
    let mut processor =
        PredictionSignalProcessor::new(PredictionMarketVenue::Kalshi, "kalshi-edge", wall_start())
            .expect("Kalshi processor constructs");
    processor
        .bind_market(1, &kalshi_contract(0, 1, 0), 0, 0)
        .expect("Kalshi market binds");
    let batch = warm_with_fresh_quote(&mut processor, &kalshi_quote(0, 1, 75), 75);

    let edge = update(&batch, KALSHI_EXECUTABLE_EDGE_ID);
    assert_eq!(edge.quality.state, QualityState::Ready);
    assert_eq!(edge.values["executable_up_price"], json!("0.42"));
    assert_eq!(edge.values["executable_down_price"], json!("0.60"));
    assert_eq!(edge.values["probability"], json!("0.54447560"));
    assert_eq!(edge.values["edge_up"], json!("0.12447560"));
    assert_eq!(edge.values["edge_down"], json!("-0.14447560"));
    assert_eq!(edge.values["cost_model"], json!("none"));

    // Pair cost on the same YES book. Locking a dollar means buying YES at the
    // ask and NO at 1 - the YES bid, so the gross cost is the YES spread:
    //   pair_cost   = 0.42 + 0.60 - 1                                   = 0.02
    //   yes_leg_fee = ceil(0.07 * 0.42 * 0.58 = 0.017052 -> 0.01 step)  = 0.02
    //   no_leg_fee  = ceil(0.07 * 0.60 * 0.40 = 0.0168   -> 0.01 step)  = 0.02
    //   fee_adjusted = 0.02 + 0.02 + 0.02                               = 0.06
    // 0.06 exceeds the 0.01 premium band, so the state is premium.
    let pair_cost = update(&batch, KALSHI_PAIR_COST_STATE_ID);
    assert_eq!(pair_cost.quality.state, QualityState::Ready);
    assert_eq!(pair_cost.values["pair_cost"], json!("0.02"));
    assert_eq!(pair_cost.values["yes_leg_fee"], json!("0.02"));
    assert_eq!(pair_cost.values["no_leg_fee"], json!("0.02"));
    assert_eq!(pair_cost.values["fee_adjusted_pair_cost"], json!("0.06"));
    assert_eq!(pair_cost.values["fee_rate"], json!("0.07"));
    assert_eq!(pair_cost.values["fee_alignment"], json!("0.01"));
    assert_eq!(pair_cost.values["premium_threshold"], json!("0.01"));
    assert_eq!(pair_cost.values["state"], json!("premium"));
}

// ADR 0016 both-sides gross edge (Polymarket down token). Warmed probability
// 0.60137831 (S = 60600, K = 60000, tau = 225); the up token asks 0.47 and the
// separate down token asks 0.55:
//   edge_up   = 0.60137831 - 0.47 =  0.13137831
//   edge_down = (1 - 0.60137831) - 0.55 = 0.39862169 - 0.55 = -0.15137831
#[test]
fn polymarket_executable_edge_prices_both_sides_with_the_down_token() {
    let mut processor = PredictionSignalProcessor::new(
        PredictionMarketVenue::Polymarket,
        "poly-edge",
        wall_start(),
    )
    .expect("Polymarket processor constructs");
    processor
        .ingest(&poly_twap(0, 0, "60000"), 0, 0)
        .expect("opening TWAP is admitted");
    processor
        .bind_market(1, &poly_contract(0, 1, 0), 0, 0)
        .expect("Polymarket market binds");
    // Warm and admit the up token quote at the final tick; the down token quote is
    // admitted alongside it so both executable prices are live.
    for second in 1..75 {
        processor
            .ingest(
                &coinbase_ticker(second, "60000", second),
                ns(second),
                ns(second),
            )
            .expect("coinbase ticker admits");
        processor
            .ingest(
                &kraken_ticker(second, "60000", second),
                ns(second),
                ns(second),
            )
            .expect("kraken ticker admits");
        processor
            .health_tick(ns(second))
            .expect("health tick emits");
    }
    processor
        .ingest(&coinbase_ticker(75, "60600", 75), ns(75), ns(75))
        .expect("coinbase jump admits");
    processor
        .ingest(&kraken_ticker(75, "60600", 75), ns(75), ns(75))
        .expect("kraken jump admits");
    processor
        .ingest(&poly_quote(2, 1, "up", "0.45", "0.47", 75), ns(75), ns(75))
        .expect("up token quote admits");
    processor
        .ingest(
            &poly_quote(3, 1, "down", "0.53", "0.55", 75),
            ns(75),
            ns(75),
        )
        .expect("down token quote admits");
    let batch = processor.health_tick(ns(75)).expect("final batch emits");

    let edge = update(&batch, POLYMARKET_EXECUTABLE_EDGE_ID);
    assert_eq!(edge.quality.state, QualityState::Ready);
    assert_eq!(edge.values["executable_up_price"], json!("0.47"));
    assert_eq!(edge.values["executable_down_price"], json!("0.55"));
    assert_eq!(edge.values["edge_up"], json!("0.13137831"));
    assert_eq!(edge.values["edge_down"], json!("-0.15137831"));
    assert_eq!(edge.values["cost_model"], json!("none"));
}

#[test]
fn composites_fail_closed_when_the_reference_is_missing() {
    // A bound market with no ticker feed: the embedded reference never becomes
    // ready, so every composite fails closed on the reference gate.
    let mut processor = PredictionSignalProcessor::new(
        PredictionMarketVenue::Kalshi,
        "kalshi-no-reference",
        wall_start(),
    )
    .expect("Kalshi processor constructs");
    processor
        .bind_market(1, &kalshi_contract(0, 1, 0), 0, 0)
        .expect("Kalshi market binds");
    let batch = processor.advance_time(ns(1)).expect("batch emits");
    for id in [
        KALSHI_STANDARDIZED_MONEYNESS_ID,
        KALSHI_FAIR_UP_PROBABILITY_ID,
    ] {
        let output = update(&batch, id);
        assert!(
            output
                .quality
                .reasons
                .iter()
                .any(|r| r == "reference_unavailable"),
            "{id}: {:?}",
            output.quality.reasons
        );
    }
}

#[test]
fn composites_fail_closed_when_volatility_is_warming() {
    // Fewer than sixty-one samples: the reference can be ready but realized
    // volatility is not, so the moneyness chain fails closed while warming.
    let mut processor = PredictionSignalProcessor::new(
        PredictionMarketVenue::Kalshi,
        "kalshi-vol-warming",
        wall_start(),
    )
    .expect("Kalshi processor constructs");
    processor
        .bind_market(1, &kalshi_contract(0, 1, 0), 0, 0)
        .expect("Kalshi market binds");
    let batch = warm_spot(&mut processor, "60000", "60600", 30);
    for id in [
        KALSHI_STANDARDIZED_MONEYNESS_ID,
        KALSHI_FAIR_UP_PROBABILITY_ID,
    ] {
        let output = update(&batch, id);
        assert_ne!(output.quality.state, QualityState::Ready, "{id}");
        assert!(
            output
                .quality
                .reasons
                .iter()
                .any(|r| r == "volatility_warming_up"),
            "{id}: {:?}",
            output.quality.reasons
        );
    }
}

#[test]
fn composites_fail_closed_when_volatility_is_zero() {
    // A perfectly flat reference: sixty-one samples but every return is zero, so the
    // short-window RMS is exactly zero and the moneyness chain fails closed.
    let mut processor = PredictionSignalProcessor::new(
        PredictionMarketVenue::Kalshi,
        "kalshi-vol-zero",
        wall_start(),
    )
    .expect("Kalshi processor constructs");
    processor
        .bind_market(1, &kalshi_contract(0, 1, 0), 0, 0)
        .expect("Kalshi market binds");
    let batch = warm_spot(&mut processor, "60100", "60100", 75);
    for id in [
        KALSHI_STANDARDIZED_MONEYNESS_ID,
        KALSHI_FAIR_UP_PROBABILITY_ID,
    ] {
        let output = update(&batch, id);
        assert_ne!(output.quality.state, QualityState::Ready, "{id}");
        assert!(
            output
                .quality
                .reasons
                .iter()
                .any(|r| r == "volatility_zero"),
            "{id}: {:?}",
            output.quality.reasons
        );
    }
}

#[test]
fn composites_fail_closed_when_moneyness_leaves_the_model_domain() {
    // A reference far above the strike (S/K = 250000/60100 = 4.16 > 4) leaves the
    // pinned ln domain; the moneyness chain fails closed with model_domain_exceeded
    // rather than extrapolating.
    let mut processor = PredictionSignalProcessor::new(
        PredictionMarketVenue::Kalshi,
        "kalshi-domain",
        wall_start(),
    )
    .expect("Kalshi processor constructs");
    processor
        .bind_market(1, &kalshi_contract(0, 1, 0), 0, 0)
        .expect("Kalshi market binds");
    let batch = warm_spot(&mut processor, "60000", "250000", 75);
    for id in [
        KALSHI_STANDARDIZED_MONEYNESS_ID,
        KALSHI_FAIR_UP_PROBABILITY_ID,
    ] {
        let output = update(&batch, id);
        assert_ne!(output.quality.state, QualityState::Ready, "{id}");
        assert!(
            output
                .quality
                .reasons
                .iter()
                .any(|r| r == "model_domain_exceeded"),
            "{id}: {:?}",
            output.quality.reasons
        );
    }
}

#[test]
fn composites_fail_closed_exactly_at_the_tau_zero_boundary() {
    // tau is strictly positive only while now < close. One nanosecond before the
    // close the market is still open; exactly at the close the market_closed gate
    // fires (tau <= 0) and the composites fail closed.
    let mut processor = PredictionSignalProcessor::new(
        PredictionMarketVenue::Kalshi,
        "kalshi-tau-boundary",
        wall_start(),
    )
    .expect("Kalshi processor constructs");
    processor
        .bind_market(1, &kalshi_contract(0, 1, 0), 0, 0)
        .expect("Kalshi market binds");
    // The Kalshi window closes 900 s after open.
    let just_before = processor
        .advance_time(ns(900) - 1)
        .expect("pre-close batch emits");
    assert!(
        !update(&just_before, KALSHI_STANDARDIZED_MONEYNESS_ID)
            .quality
            .reasons
            .iter()
            .any(|r| r == "market_closed"),
        "one nanosecond before the close the market is still open"
    );
    let at_close = processor
        .advance_time(ns(900))
        .expect("boundary batch emits");
    assert!(
        update(&at_close, KALSHI_STANDARDIZED_MONEYNESS_ID)
            .quality
            .reasons
            .iter()
            .any(|r| r == "market_closed"),
        "exactly at the close the tau gate fires"
    );
}

#[test]
fn composites_fail_closed_without_a_bound_rules_card() {
    // No market has been bound, so there is no rules card (strike/comparator/close).
    // Every composite fails closed on the market gate rather than inventing a strike.
    let mut processor = PredictionSignalProcessor::new(
        PredictionMarketVenue::Kalshi,
        "kalshi-no-card",
        wall_start(),
    )
    .expect("Kalshi processor constructs");
    let batch = processor
        .advance_time(ns(1))
        .expect("unbound evaluation succeeds");
    assert!(batch.is_empty(), "unbound state has no market instance key");
}

#[test]
fn executable_edge_fails_closed_on_a_crossed_quote() {
    // The model probability is warm and ready, but a crossed executable quote must
    // fail the edge closed rather than pricing against an invalid book.
    let mut processor = PredictionSignalProcessor::new(
        PredictionMarketVenue::Kalshi,
        "kalshi-edge-crossed",
        wall_start(),
    )
    .expect("Kalshi processor constructs");
    processor
        .bind_market(1, &kalshi_contract(0, 1, 0), 0, 0)
        .expect("Kalshi market binds");
    // A crossed Kalshi quote (bid 0.60 > ask 0.42) faults both YES and NO axes.
    let mut crossed = kalshi_quote(0, 1, 75);
    crossed.values.insert("best_bid".to_owned(), json!("0.60"));
    crossed.values.insert("best_ask".to_owned(), json!("0.42"));
    let batch = warm_with_fresh_quote(&mut processor, &crossed, 75);
    let edge = update(&batch, KALSHI_EXECUTABLE_EDGE_ID);
    // The crossed quote keeps the edge from ever pricing, so it fails closed (never
    // Ready, empty values) carrying the crossed-quote reason.
    assert_ne!(edge.quality.state, QualityState::Ready);
    assert!(edge.values.is_empty());
    assert!(
        edge.quality
            .reasons
            .iter()
            .any(|r| r == "affirmative_quote_crossed_or_invalid"),
        "{:?}",
        edge.quality.reasons
    );
}

#[test]
fn rollover_clears_composite_state_and_the_fresh_market_re_derives() {
    // The embedded spot state (reference and volatility) survives a market
    // transition, but the composites are market-relative: a rollover blanks them,
    // and the fresh market re-derives the chain against its own strike.
    let mut processor = PredictionSignalProcessor::new(
        PredictionMarketVenue::Kalshi,
        "kalshi-rollover",
        wall_start(),
    )
    .expect("Kalshi processor constructs");
    processor
        .bind_market(1, &kalshi_contract(0, 1, 0), 0, 0)
        .expect("first market binds");
    let ready = warm_spot(&mut processor, "60000", "60600", 75);
    assert_eq!(
        update(&ready, KALSHI_FAIR_UP_PROBABILITY_ID).values["probability"],
        json!("0.54447560")
    );

    // The scheduled rollover marks the venue unavailable: the composites blank.
    let unavailable = processor
        .mark_unavailable(ns(75), "scheduled_rollover")
        .expect("rollover clears the departing generation");
    assert!(unavailable.is_empty());

    // The successor market re-binds with a strike equal to the surviving reference
    // (60600). The chain re-derives against the new card: S == K, so d == 0 and the
    // fair probability is exactly 0.5 — a value the first market never produced.
    let mut successor = kalshi_contract(0, 2, 0);
    successor
        .values
        .insert("reference_value".to_owned(), json!("60600"));
    processor
        .bind_market(2, &successor, ns(75), ns(75))
        .expect("successor market binds");
    let re_derived = processor
        .advance_time(ns(75))
        .expect("fresh-market batch emits");
    let fair_up = update(&re_derived, KALSHI_FAIR_UP_PROBABILITY_ID);
    assert_eq!(fair_up.quality.state, QualityState::Ready);
    assert_eq!(fair_up.values["strike"], json!("60600"));
    assert_eq!(fair_up.values["reference_price"], json!("60600"));
    assert_eq!(fair_up.values["probability"], json!("0.5"));
}

#[test]
fn checkpoint_resume_restores_composite_feature_state() {
    // Warm the composites to ready, then checkpoint and restore. The realized-
    // volatility ring is market-relative rolling state and is deliberately not
    // checkpointed (ADR 0016), so it re-warms cold; but the composite emitters'
    // feature state (ever_ready, quality history) is restored, so a restored
    // composite degrades rather than reverting to a never-warmed WarmingUp.
    let mut processor = PredictionSignalProcessor::new(
        PredictionMarketVenue::Kalshi,
        "kalshi-before-ckpt",
        wall_start(),
    )
    .expect("Kalshi processor constructs");
    processor
        .bind_market(1, &kalshi_contract(0, 1, 0), 0, 0)
        .expect("Kalshi market binds");
    let ready = warm_spot(&mut processor, "60000", "60600", 75);
    assert_eq!(
        update(&ready, KALSHI_FAIR_UP_PROBABILITY_ID).quality.state,
        QualityState::Ready
    );

    let checkpoint = processor.checkpoint_state();
    let mut restored =
        PredictionSignalProcessor::restore("kalshi-after-ckpt", wall_start(), checkpoint)
            .expect("checkpoint restores");
    // The market is market-relative and does not survive; the volatility ring is
    // cold. The composite fails closed, but on its restored ever_ready it is
    // Degraded (not WarmingUp) with an empty value map.
    let after = restored
        .advance_time(ns(76))
        .expect("post-restore evaluation succeeds");
    assert!(
        after.is_empty(),
        "restored unbound state has no market instance key"
    );
}

#[test]
fn resting_quote_still_prices_the_edge_while_the_venue_is_alive() {
    // SIG-046 quiet-window interplay with ADR 0016: a V1 executable quote
    // rests fifteen seconds past its own staleness horizon, but a fresh trade keeps
    // the venue demonstrably alive, so the edge still prices against the resting ask.
    let mut processor = PredictionSignalProcessor::new(
        PredictionMarketVenue::Kalshi,
        "kalshi-quiet-edge",
        wall_start(),
    )
    .expect("Kalshi processor constructs");
    processor
        .bind_market(1, &kalshi_contract(0, 1, 0), 0, 0)
        .expect("Kalshi market binds");
    // Warm the model through second 60 with a fresh quote at second 60.
    for second in 1..=60 {
        processor
            .ingest(
                &coinbase_ticker(second, "60000", second),
                ns(second),
                ns(second),
            )
            .expect("coinbase ticker admits");
        processor
            .ingest(
                &kraken_ticker(second, "60000", second),
                ns(second),
                ns(second),
            )
            .expect("kraken ticker admits");
        if second == 60 {
            processor
                .ingest(&kalshi_quote(0, 1, 60), ns(60), ns(60))
                .expect("quote admits");
        }
        processor
            .health_tick(ns(second))
            .expect("health tick emits");
    }
    // Keep the ring warm to second 75 while the quote rests untouched; a fresh trade
    // at second 75 keeps the venue alive fifteen seconds past the quote.
    for second in 61..=75 {
        let price = if second == 75 { "60600" } else { "60000" };
        processor
            .ingest(
                &coinbase_ticker(second, price, second),
                ns(second),
                ns(second),
            )
            .expect("coinbase ticker admits");
        processor
            .ingest(
                &kraken_ticker(second, price, second),
                ns(second),
                ns(second),
            )
            .expect("kraken ticker admits");
        if second == 75 {
            processor
                .ingest(&kalshi_trade(1, 1, 75), ns(75), ns(75))
                .expect("late trade keeps the venue alive");
        }
        processor
            .health_tick(ns(second))
            .expect("health tick emits");
    }
    let batch = processor.advance_time(ns(75)).expect("resting batch emits");
    let edge = update(&batch, KALSHI_EXECUTABLE_EDGE_ID);
    assert_eq!(edge.quality.state, QualityState::Ready);
    assert_eq!(edge.values["executable_up_price"], json!("0.42"));
    assert_eq!(edge.values["executable_down_price"], json!("0.60"));
    assert_eq!(edge.values["cost_model"], json!("none"));
    // The published edge is model minus resting ask, computed from the ready
    // probability; both sides are present decimals.
    assert!(as_decimal(&edge.values["edge_up"]) != as_decimal(&edge.values["edge_down"]));
}

/// A Kalshi quote at explicit prices, so a test can move the book between two
/// admissions. `kalshi_quote` pins the standard 0.40/0.42 fixture.
fn kalshi_quote_at(sequence: u64, market: u64, second: u64, bid: &str, ask: &str) -> SourceEvent {
    source_event(
        "kalshi.btc.quotes",
        "kalshi-quotes",
        sequence,
        second,
        BTreeMap::from([
            (
                "market_ticker".to_owned(),
                json!(format!("KXBTC15M-{market}")),
            ),
            ("last_price".to_owned(), json!(bid)),
            ("best_bid".to_owned(), json!(bid)),
            ("best_ask".to_owned(), json!(ask)),
            ("best_bid_size".to_owned(), json!("12")),
            ("best_ask_size".to_owned(), json!("6")),
            ("volume".to_owned(), json!("100")),
            ("open_interest".to_owned(), json!("50")),
        ]),
    )
}

/// The repricing-lag fold, end to end through the processor.
///
/// The book rests at 0.40/0.42 while the reference is flat, so no qualifying
/// move exists and the reading stays not-ready (its `repricing_lag_ms` is a
/// required field, and no lag has completed). The reference then jumps 100 bps in
/// one second — past the 10 bps threshold — while the venue is silent, which
/// leaves the move unanswered; the next second admits a genuinely different
/// quote, which answers it. The lag is measured on the processor's decision
/// clock, so the answer one decision later is exactly 1000 ms.
#[test]
fn venue_repricing_lag_times_the_first_answering_quote_change() {
    let mut processor =
        PredictionSignalProcessor::new(PredictionMarketVenue::Kalshi, "kalshi-lag", wall_start())
            .expect("Kalshi processor constructs");
    processor
        .bind_market(1, &kalshi_contract(0, 1, 0), 0, 0)
        .expect("Kalshi market binds");

    let mut batch = PredictionBatch::default();
    for second in 1..=70 {
        processor
            .ingest(
                &coinbase_ticker(second, "60000", second),
                ns(second),
                ns(second),
            )
            .expect("coinbase ticker admits");
        processor
            .ingest(
                &kraken_ticker(second, "60000", second),
                ns(second),
                ns(second),
            )
            .expect("kraken ticker admits");
        if second == 10 {
            processor
                .ingest(&kalshi_quote(0, 1, second), ns(second), ns(second))
                .expect("resting quote admits");
        }
        batch = processor
            .health_tick(ns(second))
            .expect("health tick emits a batch");
    }

    // Flat reference, so nothing has ever been answered: not ready, no values.
    let quiet = update(&batch, KALSHI_VENUE_REPRICING_LAG_ID);
    assert_ne!(quiet.quality.state, QualityState::Ready);
    assert!(quiet.values.is_empty());

    // A 100 bps reference move with a silent venue: the move is now unanswered.
    processor
        .ingest(&coinbase_ticker(71, "60600", 71), ns(71), ns(71))
        .expect("coinbase jump admits");
    processor
        .ingest(&kraken_ticker(71, "60600", 71), ns(71), ns(71))
        .expect("kraken jump admits");
    let lagging = processor.health_tick(ns(71)).expect("jump tick emits");
    let lagging = update(&lagging, KALSHI_VENUE_REPRICING_LAG_ID);
    assert_ne!(lagging.quality.state, QualityState::Ready);
    assert!(lagging.values.is_empty());

    // One decision later the book moves, which answers the pending move.
    processor
        .ingest(&coinbase_ticker(72, "60600", 72), ns(72), ns(72))
        .expect("coinbase hold admits");
    processor
        .ingest(&kraken_ticker(72, "60600", 72), ns(72), ns(72))
        .expect("kraken hold admits");
    processor
        .ingest(&kalshi_quote_at(1, 1, 72, "0.44", "0.46"), ns(72), ns(72))
        .expect("repriced quote admits");
    let answered = processor.health_tick(ns(72)).expect("answering tick emits");
    let answered = update(&answered, KALSHI_VENUE_REPRICING_LAG_ID);
    assert_eq!(answered.quality.state, QualityState::Ready);
    assert_eq!(answered.values["state"], json!("current"));
    assert_eq!(answered.values["repricing_lag_ms"], json!(1_000));
    assert_eq!(answered.values["staleness_ms"], json!(0));
    assert_eq!(answered.values["move_threshold_bps"], json!("10"));
}

fn source_event(
    source_id: &str,
    publisher_epoch: &str,
    sequence: u64,
    second: u64,
    mut values: BTreeMap<String, Value>,
) -> SourceEvent {
    let event_time = timestamp(second);
    values.insert("event_time".to_owned(), json!(event_time));
    SourceEvent {
        schema_version: 1,
        source_id: source_id.to_owned(),
        source_version: "1.0.0".to_owned(),
        publisher_epoch: publisher_epoch.to_owned(),
        sequence,
        event_time: event_time.clone(),
        received_at: event_time,
        values,
    }
}

fn poly_twap(sequence: u64, second: u64, price: &str) -> SourceEvent {
    source_event(
        "polymarket.rtds.btc-usd.twap-60s",
        "poly-twap",
        sequence,
        second,
        BTreeMap::from([
            ("symbol".to_owned(), json!("btc/usd")),
            ("window_seconds".to_owned(), json!(60)),
            ("twap_price".to_owned(), json!(price)),
            ("published_at".to_owned(), json!(timestamp(second))),
        ]),
    )
}

fn poly_contract(sequence: u64, market: u64, open_second: u64) -> SourceEvent {
    source_event(
        "polymarket.btc.market-contracts",
        "poly-contracts",
        sequence,
        open_second,
        BTreeMap::from([
            ("market_id".to_owned(), json!(format!("pm-{market}"))),
            ("condition_id".to_owned(), json!(format!("cond-{market}"))),
            (
                "question_id".to_owned(),
                json!(format!("question-{market}")),
            ),
            ("slug".to_owned(), json!(format!("btc-updown-{market}"))),
            (
                "question".to_owned(),
                json!("Will BTC be at or above the opening value?"),
            ),
            ("up_token_id".to_owned(), json!(format!("up-{market}"))),
            ("down_token_id".to_owned(), json!(format!("down-{market}"))),
            ("window_open".to_owned(), json!(timestamp(open_second))),
            (
                "window_close".to_owned(),
                json!(timestamp(open_second + 300)),
            ),
            (
                "resolution_source".to_owned(),
                json!("https://data.chain.link/streams/btc-usd-twap-60s-streams"),
            ),
            ("comparator".to_owned(), json!("greater_than_or_equal")),
            ("tie_outcome".to_owned(), json!("up")),
            ("minimum_tick_size".to_owned(), json!("0.01")),
            ("minimum_order_size".to_owned(), json!("5")),
            ("order_book_enabled".to_owned(), json!(true)),
            ("active".to_owned(), json!(true)),
            ("closed".to_owned(), json!(false)),
            ("rules_sha256".to_owned(), json!("a".repeat(64))),
        ]),
    )
}

fn poly_quote(
    sequence: u64,
    market: u64,
    outcome: &str,
    bid: &str,
    ask: &str,
    second: u64,
) -> SourceEvent {
    let bid_decimal = bid.parse::<rust_decimal::Decimal>().expect("bid parses");
    let ask_decimal = ask.parse::<rust_decimal::Decimal>().expect("ask parses");
    source_event(
        "polymarket.btc.quotes",
        "poly-quotes",
        sequence,
        second,
        BTreeMap::from([
            ("condition_id".to_owned(), json!(format!("cond-{market}"))),
            ("asset_id".to_owned(), json!(format!("{outcome}-{market}"))),
            ("outcome".to_owned(), json!(outcome)),
            ("best_bid".to_owned(), json!(bid)),
            ("best_ask".to_owned(), json!(ask)),
            (
                "spread".to_owned(),
                json!((ask_decimal - bid_decimal).to_string()),
            ),
        ]),
    )
}

#[allow(clippy::too_many_arguments)]
fn poly_book(
    sequence: u64,
    group: &str,
    index: u32,
    count: u32,
    outcome: &str,
    side: &str,
    price: &str,
    size: &str,
    second: u64,
) -> SourceEvent {
    source_event(
        "polymarket.btc.book-levels",
        "poly-books",
        sequence,
        second,
        BTreeMap::from([
            ("condition_id".to_owned(), json!("cond-1")),
            ("asset_id".to_owned(), json!(format!("{outcome}-1"))),
            ("outcome".to_owned(), json!(outcome)),
            ("update_group".to_owned(), json!(group)),
            ("update_index".to_owned(), json!(index)),
            ("update_count".to_owned(), json!(count)),
            ("update_kind".to_owned(), json!("snapshot")),
            ("side".to_owned(), json!(side)),
            ("price".to_owned(), json!(price)),
            ("size".to_owned(), json!(size)),
        ]),
    )
}

fn poly_trade(
    sequence: u64,
    outcome: &str,
    side: &str,
    price: &str,
    size: &str,
    second: u64,
) -> SourceEvent {
    source_event(
        "polymarket.btc.trades",
        "poly-trades",
        sequence,
        second,
        BTreeMap::from([
            ("condition_id".to_owned(), json!("cond-1")),
            ("asset_id".to_owned(), json!(format!("{outcome}-1"))),
            ("outcome".to_owned(), json!(outcome)),
            ("price".to_owned(), json!(price)),
            ("size".to_owned(), json!(size)),
            ("fee_rate_bps".to_owned(), json!("0")),
            ("reported_side".to_owned(), json!(side)),
        ]),
    )
}

fn kalshi_contract(sequence: u64, market: u64, open_second: u64) -> SourceEvent {
    source_event(
        "kalshi.btc.market-contracts",
        "kalshi-contracts",
        sequence,
        open_second,
        BTreeMap::from([
            (
                "market_ticker".to_owned(),
                json!(format!("KXBTC15M-{market}")),
            ),
            (
                "event_ticker".to_owned(),
                json!(format!("KXBTC15M-E{market}")),
            ),
            ("title".to_owned(), json!("BTC price in fifteen minutes")),
            ("status".to_owned(), json!("active")),
            ("open_time".to_owned(), json!(timestamp(open_second))),
            ("close_time".to_owned(), json!(timestamp(open_second + 900))),
            (
                "expected_expiration_time".to_owned(),
                json!(timestamp(open_second + 900)),
            ),
            (
                "legal_expiration_time".to_owned(),
                json!(timestamp(open_second + 1_800)),
            ),
            ("benchmark".to_owned(), json!("cf_benchmarks_brti")),
            ("averaging_window_seconds".to_owned(), json!(60)),
            ("comparator".to_owned(), json!("greater_than_or_equal")),
            ("tie_outcome".to_owned(), json!("yes")),
            ("reference_value".to_owned(), json!("60100")),
            ("rules_sha256".to_owned(), json!("b".repeat(64))),
        ]),
    )
}

fn kalshi_quote(sequence: u64, market: u64, second: u64) -> SourceEvent {
    source_event(
        "kalshi.btc.quotes",
        "kalshi-quotes",
        sequence,
        second,
        BTreeMap::from([
            (
                "market_ticker".to_owned(),
                json!(format!("KXBTC15M-{market}")),
            ),
            ("last_price".to_owned(), json!("0.41")),
            ("best_bid".to_owned(), json!("0.40")),
            ("best_ask".to_owned(), json!("0.42")),
            ("best_bid_size".to_owned(), json!("12")),
            ("best_ask_size".to_owned(), json!("6")),
            ("volume".to_owned(), json!("100")),
            ("open_interest".to_owned(), json!("50")),
        ]),
    )
}

#[allow(clippy::too_many_arguments)]
fn kalshi_book(
    sequence: u64,
    group: &str,
    index: u32,
    count: u32,
    side: &str,
    price: &str,
    size: &str,
    second: u64,
) -> SourceEvent {
    source_event(
        "kalshi.btc.book-levels",
        "kalshi-books",
        sequence,
        second,
        BTreeMap::from([
            ("market_ticker".to_owned(), json!("KXBTC15M-1")),
            ("upstream_sequence".to_owned(), json!(sequence + 100)),
            ("update_group".to_owned(), json!(group)),
            ("update_index".to_owned(), json!(index)),
            ("update_count".to_owned(), json!(count)),
            ("update_kind".to_owned(), json!("snapshot")),
            ("side".to_owned(), json!(side)),
            ("price".to_owned(), json!(price)),
            ("size".to_owned(), json!(size)),
            ("event_time_basis".to_owned(), json!("provider")),
        ]),
    )
}

fn kalshi_trade(sequence: u64, market: u64, second: u64) -> SourceEvent {
    source_event(
        "kalshi.btc.trades",
        "kalshi-trades",
        sequence,
        second,
        BTreeMap::from([
            ("trade_id".to_owned(), json!(format!("trade-{sequence}"))),
            (
                "market_ticker".to_owned(),
                json!(format!("KXBTC15M-{market}")),
            ),
            ("yes_price".to_owned(), json!("0.41")),
            ("no_price".to_owned(), json!("0.59")),
            ("count".to_owned(), json!("3")),
            ("taker_outcome_side".to_owned(), json!("no")),
            ("taker_book_side".to_owned(), json!("ask")),
            ("is_block_trade".to_owned(), json!(false)),
        ]),
    )
}

/// Ruling E2's applied form. The Kalshi lane's four adopted market sources do
/// not depend on the index socket, so a missing `cfbrti`/`pyth` token disables
/// that socket and leaves the market channels running; a literal refusal would
/// take working, un-gated surfaces down to gate a subscription they do not need.
/// A lane that *requests* the socket without its token is the case the binary
/// turns into exit 78.
#[test]
fn index_socket_refuses_without_enabled_venue() {
    use signals_core::{ArmedIndexSockets, EnabledVenues, KALSHI_LANE, LaneStart, lane_start};

    let default_set = EnabledVenues::parse(None).expect("the unset default parses");
    assert_eq!(
        lane_start(&KALSHI_LANE, &default_set),
        LaneStart::Start {
            index_sockets: ArmedIndexSockets::NONE
        },
        "the market channels run with the index socket off"
    );
    // F6: one token arms one channel family, never both, even though the two
    // families ride the same socket.
    for token in ["cfbrti", "pyth"] {
        let enabled =
            EnabledVenues::parse(Some(&format!("kalshi,{token}"))).expect("the value parses");
        let LaneStart::Start { index_sockets } = lane_start(&KALSHI_LANE, &enabled) else {
            panic!("the Kalshi lane starts with {token}");
        };
        assert!(index_sockets.is_armed(token), "{token} arms its own family");
        assert_eq!(
            index_sockets.tokens().collect::<Vec<_>>(),
            vec![token],
            "{token} arms nothing else"
        );
    }
    // The index tokens never substitute for the lane's own token.
    let index_only = EnabledVenues::parse(Some("cfbrti,pyth")).expect("the value parses");
    let LaneStart::Refuse { reason } = lane_start(&KALSHI_LANE, &index_only) else {
        panic!("the Kalshi lane still requires its own token");
    };
    assert!(reason.contains("kalshi"), "{reason}");
    assert!(reason.contains("RIGHTS-LEDGER"), "{reason}");
}

// --- C3b: the TA and timing pack on the prediction venues (ruling E51) -----

/// Feed one Kalshi quote per second on a ramping probability and take the
/// decision batch of the last second.
///
/// The affirmative midpoint is 0.30 + 0.001 * second exactly, so the sampled
/// probability path is 30 + 0.1 * second in probability points — a series whose
/// mean, population sigma, z-score and rate of change are all hand-computable.
fn kalshi_probability_ramp(
    epoch: &str,
    seconds: u64,
) -> (PredictionSignalProcessor, PredictionBatch) {
    let mut processor =
        PredictionSignalProcessor::new(PredictionMarketVenue::Kalshi, epoch, wall_start())
            .expect("Kalshi processor constructs");
    processor
        .bind_market(1, &kalshi_contract(0, 1, 0), 0, 0)
        .expect("Kalshi market binds");
    let mut batch = PredictionBatch::default();
    for second in 1..=seconds {
        let bid = format!("0.{:03}", 290 + second);
        let ask = format!("0.{:03}", 310 + second);
        processor
            .ingest(
                &kalshi_quote_at(second, 1, second, &bid, &ask),
                ns(second),
                ns(second),
            )
            .expect("Kalshi quote is admitted");
        batch = processor
            .advance_time(ns(second))
            .expect("the decision batch emits");
    }
    (processor, batch)
}

/// A Polymarket analogue of [`kalshi_probability_ramp`]: one native up/down quote
/// pair per second over a rising up-token midpoint, so the affirmative-midpoint
/// bar series warms while the market stays bound to one generation.
fn polymarket_probability_ramp(
    epoch: &str,
    seconds: u64,
) -> (PredictionSignalProcessor, PredictionBatch) {
    let mut processor =
        PredictionSignalProcessor::new(PredictionMarketVenue::Polymarket, epoch, wall_start())
            .expect("Polymarket processor constructs");
    processor
        .ingest(&poly_twap(0, 0, "60000"), 0, 0)
        .expect("opening TWAP is admitted");
    processor
        .bind_market(1, &poly_contract(0, 1, 0), 0, 0)
        .expect("Polymarket market binds");
    let mut batch = PredictionBatch::default();
    for second in 1..=seconds {
        let up_bid = format!("0.{:03}", 200 + second);
        let up_ask = format!("0.{:03}", 210 + second);
        let down_bid = format!("0.{:03}", 780 - second);
        let down_ask = format!("0.{:03}", 790 - second);
        for event in [
            poly_quote(2 * second, 1, "up", &up_bid, &up_ask, second),
            poly_quote(2 * second + 1, 1, "down", &down_bid, &down_ask, second),
        ] {
            processor
                .ingest(&event, ns(second), ns(second))
                .expect("native quote is admitted");
        }
        batch = processor
            .advance_time(ns(second))
            .expect("the decision batch emits");
    }
    (processor, batch)
}

/// Known-answer test for the three probability-series identities.
///
/// Sixty-one one-second samples of 30 + 0.1 * s probability points:
///   * the complete one-minute z-score window is seconds 2..=61, whose mean is
///     33.15 and population sigma sqrt(179.95 / 60) = 1.73181023, so the current
///     36.1 sits (36.1 - 33.15) / 1.73181023 = 1.7034199 sigma above it;
///   * ROC over ten seconds is 36.1 - 35.1 = 1 probability point and
///     (36.1 / 35.1 - 1) * 100 = 2.84900285 percent; over one minute it is
///     6 points and 19.93355482 percent; the five-minute horizon has no sample
///     yet and is reported absent rather than measured over a shorter span;
///   * the update rate counts ten quote updates and ten midpoint changes in the
///     half-open trailing ten-second window (51 s, 61 s] against sixty seconds of
///     observed baseline (sixty-one quotes), a velocity ratio of
///     (10 / 10) / (61 / 60) = 60 / 61 = 0.98360656.
///
/// RSI stays warming: fifteen complete ten-second bars are the declared warm-up
/// (ruling E58) and only six have closed after sixty-one seconds.
#[test]
fn the_probability_series_identities_match_hand_computed_values() {
    let (_processor, batch) = kalshi_probability_ramp("kalshi-ta", 61);

    let zscore = update(&batch, KALSHI_ZSCORE_ID);
    assert_eq!(zscore.quality.state, QualityState::Ready);
    assert_eq!(zscore.values["probability_points"], json!("36.100"));
    assert_eq!(zscore.values["mean_1m"], json!("33.150"));
    assert_eq!(zscore.values["sigma_1m"], json!("1.73181023"));
    assert_eq!(zscore.values["z_1m"], json!("1.70341990"));
    assert_eq!(zscore.values["z_5m_present"], json!(false));
    assert_eq!(zscore.values["z_5m"], json!("0"));
    assert_eq!(zscore.values["z_15m_present"], json!(false));

    let roc = update(&batch, KALSHI_ROC_ID);
    assert_eq!(roc.quality.state, QualityState::Ready);
    assert_eq!(roc.values["roc_10s_present"], json!(true));
    assert_eq!(roc.values["roc_points_10s"], json!("1.000"));
    assert_eq!(roc.values["roc_percent_10s"], json!("2.84900285"));
    assert_eq!(roc.values["roc_1m_present"], json!(true));
    assert_eq!(roc.values["roc_points_1m"], json!("6.000"));
    assert_eq!(roc.values["roc_percent_1m"], json!("19.93355482"));
    assert_eq!(roc.values["roc_5m_present"], json!(false));
    assert_eq!(roc.values["roc_points_5m"], json!("0"));

    let velocity = update(&batch, KALSHI_TICK_VELOCITY_ID);
    assert_eq!(velocity.quality.state, QualityState::Ready);
    assert_eq!(velocity.values["quote_updates_10s"], json!(10));
    assert_eq!(velocity.values["midpoint_changes_10s"], json!(10));
    assert_eq!(velocity.values["trades_1s"], json!(0));
    assert_eq!(velocity.values["baseline_quote_updates"], json!(60));
    assert_eq!(velocity.values["baseline_window_ms"], json!(60_000));
    assert_eq!(velocity.values["velocity_ratio_present"], json!(true));
    // Ruling E58: the baseline is the half-open observed span, so a steady 1/s
    // stream divides to exactly one.
    assert_eq!(velocity.values["velocity_ratio"], json!("1"));

    // The declared RSI warm-up is fifteen closed ten-second bars (ruling E58).
    let rsi = update(&batch, KALSHI_RSI_ID);
    assert_eq!(rsi.quality.state, QualityState::WarmingUp);
    assert_eq!(
        rsi.quality.reasons,
        vec!["market_contract_warming_up".to_owned()]
    );
}

/// Kalshi RSI reaches Ready inside a single 900 s generation (ruling E58).
///
/// Fifteen closed ten-second bars warm the indicator; over a rising midpoint they
/// close by ~160 s, far inside the fifteen-minute market window that sixty-second
/// bars could never fill. The ramp is monotone up, so every delta is a gain and
/// the reading saturates at 100 — a Ready value, which is all this asserts.
#[test]
fn kalshi_rsi_reaches_ready_inside_a_generation() {
    // 200 s < the 900 s generation, and past the ~150 s fifteen-bar warm-up.
    const RAMP_SECONDS: u64 = 200;
    let (_processor, batch) = kalshi_probability_ramp("kalshi-rsi-ready", RAMP_SECONDS);
    let rsi = update(&batch, KALSHI_RSI_ID);
    assert_eq!(rsi.quality.state, QualityState::Ready, "{:?}", rsi.quality);
    assert!(rsi.values.contains_key("rsi_14"));
    assert!(
        rsi.values["bars_observed"]
            .as_u64()
            .expect("bars_observed is a count")
            >= 15,
        "at least the fifteen-bar warm-up has closed: {:?}",
        rsi.values["bars_observed"]
    );
}

/// Polymarket RSI reaches Ready inside a 300 s window (ruling E58): the same
/// ten-second bars warm the indicator by ~160 s, inside the five-minute window.
#[test]
fn polymarket_rsi_reaches_ready_inside_a_window() {
    // 200 s < the 300 s window, and past the ~150 s fifteen-bar warm-up.
    const RAMP_SECONDS: u64 = 200;
    let (_processor, batch) = polymarket_probability_ramp("poly-rsi-ready", RAMP_SECONDS);
    let rsi = update(&batch, POLYMARKET_RSI_ID);
    assert_eq!(rsi.quality.state, QualityState::Ready, "{:?}", rsi.quality);
    assert!(rsi.values.contains_key("rsi_14"));
    assert!(
        rsi.values["bars_observed"]
            .as_u64()
            .expect("bars_observed is a count")
            >= 15,
        "at least the fifteen-bar warm-up has closed: {:?}",
        rsi.values["bars_observed"]
    );
}

/// A complete one-minute window with zero dispersion has no z-score: the
/// identity is not ready with the E51 reason `flat_window` rather than dividing
/// by zero or publishing a fabricated zero. The other two series identities are
/// unaffected — a flat market has a well-defined zero rate of change.
#[test]
fn a_flat_probability_window_is_not_ready_with_flat_window() {
    let mut processor =
        PredictionSignalProcessor::new(PredictionMarketVenue::Kalshi, "kalshi-flat", wall_start())
            .expect("Kalshi processor constructs");
    processor
        .bind_market(1, &kalshi_contract(0, 1, 0), 0, 0)
        .expect("Kalshi market binds");
    let mut batch = PredictionBatch::default();
    for second in 1..=61 {
        processor
            .ingest(
                &kalshi_quote_at(second, 1, second, "0.40", "0.42"),
                ns(second),
                ns(second),
            )
            .expect("Kalshi quote is admitted");
        batch = processor
            .advance_time(ns(second))
            .expect("the decision batch emits");
    }
    let zscore = update(&batch, KALSHI_ZSCORE_ID);
    assert_eq!(zscore.quality.state, QualityState::WarmingUp);
    assert_eq!(zscore.quality.reasons, vec!["flat_window".to_owned()]);
    assert!(zscore.values.is_empty());

    let roc = update(&batch, KALSHI_ROC_ID);
    assert_eq!(roc.quality.state, QualityState::Ready);
    assert_eq!(roc.values["roc_points_10s"], json!("0.00"));
    assert_eq!(roc.values["roc_percent_10s"], json!("0"));

    // A flat quote is still a quote update; only the midpoint never changes.
    let velocity = update(&batch, KALSHI_TICK_VELOCITY_ID);
    assert_eq!(velocity.quality.state, QualityState::Ready);
    assert_eq!(velocity.values["quote_updates_10s"], json!(10));
    assert_eq!(velocity.values["midpoint_changes_10s"], json!(0));
}

/// Expected value at the ask, hand-computed with the official Kalshi fee.
///
/// The spot fixture puts the reference exactly on the 60100 strike, so the fair
/// probability is exactly 0.5. The book rests at 0.40 / 0.42, so the YES ask is
/// 0.42 and the NO ask is 1 - 0.40 = 0.60. Kalshi charges
/// round_up(0.07 * P * (1 - P)) onto the retail 0.01 increment:
///   yes: 0.07 * 0.42 * 0.58 = 0.017052 -> 0.02
///   no:  0.07 * 0.60 * 0.40 = 0.0168   -> 0.02
///   ev_yes = 0.5 - 0.42 - 0.02 =  0.06
///   ev_no  = 0.5 - 0.60 - 0.02 = -0.12
/// so the best side is yes, staking 0.42 + 0.02 = 0.44 for
/// 0.06 / 0.44 * 10000 = 1363.63636364 bps of stake.
#[test]
fn expected_value_at_the_ask_is_hand_computable_with_the_kalshi_fee() {
    let mut processor =
        PredictionSignalProcessor::new(PredictionMarketVenue::Kalshi, "kalshi-ev", wall_start())
            .expect("Kalshi processor constructs");
    processor
        .bind_market(1, &kalshi_contract(0, 1, 0), 0, 0)
        .expect("Kalshi market binds");
    let mut batch = PredictionBatch::default();
    for second in 1..=65 {
        let price = if second % 2 == 0 { "60110" } else { "60100" };
        processor
            .ingest(
                &coinbase_ticker(second, price, second),
                ns(second),
                ns(second),
            )
            .expect("coinbase ticker admits");
        processor
            .ingest(
                &kraken_ticker(second, price, second),
                ns(second),
                ns(second),
            )
            .expect("kraken ticker admits");
        processor
            .ingest(
                &kalshi_quote_at(second, 1, second, "0.40", "0.42"),
                ns(second),
                ns(second),
            )
            .expect("Kalshi quote is admitted");
        batch = processor
            .health_tick(ns(second))
            .expect("health tick emits a batch");
    }

    // The fair probability the expected value reads is the published one.
    let fair_up = update(&batch, KALSHI_FAIR_UP_PROBABILITY_ID);
    assert_eq!(fair_up.quality.state, QualityState::Ready);
    assert_eq!(fair_up.values["probability"], json!("0.5"));

    let ev = update(&batch, KALSHI_EXPECTED_VALUE_ID);
    assert_eq!(ev.quality.state, QualityState::Ready);
    assert_eq!(ev.values["fair_probability"], json!("0.5"));
    assert_eq!(ev.values["payout"], json!("1"));
    assert_eq!(ev.values["yes_ask"], json!("0.42"));
    assert_eq!(ev.values["no_ask"], json!("0.60"));
    assert_eq!(ev.values["yes_fee"], json!("0.02"));
    assert_eq!(ev.values["no_fee"], json!("0.02"));
    assert_eq!(ev.values["ev_buy_yes_at_ask"], json!("0.06"));
    assert_eq!(ev.values["ev_buy_no_at_ask"], json!("-0.12"));
    assert_eq!(ev.values["best_side"], json!("yes"));
    assert_eq!(ev.values["ev_bps_of_stake"], json!("1363.63636364"));
}

/// The prediction series are per generation (spec C3 §2): a market roll clears
/// the probability ring, the bars and the update-rate counters, so the next
/// contract's readings never fold the departed contract's path.
#[test]
fn a_market_roll_restarts_the_prediction_series() {
    let (mut processor, batch) = kalshi_probability_ramp("kalshi-roll", 61);
    assert_eq!(
        update(&batch, KALSHI_ZSCORE_ID).quality.state,
        QualityState::Ready
    );
    processor
        .mark_unavailable(ns(62), "scheduled_rollover")
        .expect("the market rolls off");
    processor
        .bind_market(2, &kalshi_contract(1, 2, 63), ns(63), ns(63))
        .expect("the successor market binds");
    processor
        .ingest(&kalshi_quote_at(100, 2, 64, "0.40", "0.42"), ns(64), ns(64))
        .expect("Kalshi quote is admitted");
    let after = processor
        .advance_time(ns(64))
        .expect("the successor decision emits");
    // Degraded, not warming: these instances were ready under the departed
    // generation, and an identity that has ever been ready degrades rather than
    // re-warming (ADR 0017). What matters here is that no value survives.
    for id in [KALSHI_ZSCORE_ID, KALSHI_ROC_ID, KALSHI_TICK_VELOCITY_ID] {
        let published = update(&after, id);
        assert_eq!(published.quality.state, QualityState::Degraded, "{id}");
        assert!(published.values.is_empty(), "{id}");
    }
}

// ---------------------------------------------------------------------------
// C2 (E49): probability momentum and volatility on the venue's own
// prediction_quote affirmative_midpoint. The ring samples one midpoint per
// second on the health tick, so these fixtures drive one-second health ticks
// on a bound Kalshi market, feeding a fresh quote each second (which keeps the
// venue alive and republishes the single up-front trade) so the prediction
// quote — and therefore the sampled affirmative_midpoint — is ready every tick.
// ---------------------------------------------------------------------------

const PROBABILITY_MOMENTUM_ID: &str = "btc.kalshi.probability_momentum";
const PROBABILITY_VOLATILITY_ID: &str = "btc.kalshi.probability_volatility";

/// Bind a fresh Kalshi generation-1 market and drive one-second health ticks for
/// seconds `1..=last`, feeding a quote each second whose YES bid/ask are chosen
/// by `midpoint`. A single trade is fed up front and republishes while the venue
/// stays alive, so the prediction quote is ready every tick. Returns the
/// processor (so a test can keep driving it) and the batch emitted at `last`.
fn warm_probability(
    epoch: &str,
    last: u64,
    midpoint: impl Fn(u64) -> (&'static str, &'static str),
) -> (PredictionSignalProcessor, PredictionBatch) {
    let mut processor =
        PredictionSignalProcessor::new(PredictionMarketVenue::Kalshi, epoch, wall_start())
            .expect("Kalshi processor constructs");
    processor
        .bind_market(1, &kalshi_contract(0, 1, 0), 0, 0)
        .expect("Kalshi market binds");
    processor
        .ingest(&kalshi_trade(0, 1, 1), ns(1), ns(1))
        .expect("Kalshi trade admits");
    let mut batch = PredictionBatch::default();
    for second in 1..=last {
        let (bid, ask) = midpoint(second);
        processor
            .ingest(
                &kalshi_quote_at(second, 1, second, bid, ask),
                ns(second),
                ns(second),
            )
            .expect("Kalshi quote admits");
        batch = processor
            .health_tick(ns(second))
            .expect("health tick emits a batch");
    }
    (processor, batch)
}

/// Hand-computed momentum on a synthetic ring: the affirmative_midpoint holds at
/// 0.50 for 300 seconds and jumps to 0.55 on the final tick, so every horizon's
/// prior reference is 0.50 and each probability-point return is
/// 100 * (0.55 - 0.50) = 5 pp. direction_1m is up.
#[test]
fn probability_momentum_is_the_hand_computed_pp_return() {
    let (_, batch) = warm_probability("kalshi-prob-momentum", 301, |second| {
        if second == 301 {
            ("0.54", "0.56") // midpoint 0.55
        } else {
            ("0.49", "0.51") // midpoint 0.50
        }
    });
    let momentum = update(&batch, PROBABILITY_MOMENTUM_ID);
    assert_eq!(momentum.quality.state, QualityState::Ready);
    assert_eq!(
        as_decimal(&momentum.values["return_pp_10s"]),
        "5".parse().unwrap()
    );
    assert_eq!(
        as_decimal(&momentum.values["return_pp_1m"]),
        "5".parse().unwrap()
    );
    assert_eq!(
        as_decimal(&momentum.values["return_pp_5m"]),
        "5".parse().unwrap()
    );
    assert_eq!(momentum.values["direction_1m"], json!("up"));
}

/// Hand-computed 1 m RMS on a synthetic ring: the affirmative_midpoint alternates
/// 0.50 / 0.51 every second, so every one-second return is +/- 1 pp and its
/// square is 1. The root mean square over each window is therefore exactly 1 pp,
/// the shortest/longest ratio is 1, and the regime is stable.
#[test]
fn probability_volatility_is_the_hand_computed_rms() {
    let (_, batch) = warm_probability("kalshi-prob-volatility", 301, |second| {
        if second % 2 == 0 {
            ("0.49", "0.51") // midpoint 0.50
        } else {
            ("0.50", "0.52") // midpoint 0.51
        }
    });
    let volatility = update(&batch, PROBABILITY_VOLATILITY_ID);
    assert_eq!(volatility.quality.state, QualityState::Ready);
    assert_eq!(
        as_decimal(&volatility.values["volatility_pp_10s"]),
        "1".parse().unwrap()
    );
    assert_eq!(
        as_decimal(&volatility.values["volatility_pp_1m"]),
        "1".parse().unwrap()
    );
    assert_eq!(
        as_decimal(&volatility.values["volatility_pp_5m"]),
        "1".parse().unwrap()
    );
    assert_eq!(
        as_decimal(&volatility.values["short_long_ratio"]),
        "1".parse().unwrap()
    );
    assert_eq!(volatility.values["regime"], json!("stable"));
}

/// The 0/0 convention: a perfectly flat affirmative_midpoint makes every
/// one-second return zero, so both the shortest and longest RMS are zero. The
/// ratio is then defined as 1 and the regime is stable (never a division by
/// zero). Momentum over a flat series is zero and direction_1m is flat.
#[test]
fn probability_volatility_flat_series_ratio_is_one() {
    let (_, batch) = warm_probability("kalshi-prob-flat", 301, |_| ("0.49", "0.51"));
    let volatility = update(&batch, PROBABILITY_VOLATILITY_ID);
    assert_eq!(volatility.quality.state, QualityState::Ready);
    assert_eq!(
        as_decimal(&volatility.values["volatility_pp_10s"]),
        "0".parse().unwrap()
    );
    assert_eq!(
        as_decimal(&volatility.values["volatility_pp_5m"]),
        "0".parse().unwrap()
    );
    assert_eq!(
        as_decimal(&volatility.values["short_long_ratio"]),
        "1".parse().unwrap()
    );
    assert_eq!(volatility.values["regime"], json!("stable"));

    let momentum = update(&batch, PROBABILITY_MOMENTUM_ID);
    assert_eq!(momentum.quality.state, QualityState::Ready);
    assert_eq!(
        as_decimal(&momentum.values["return_pp_1m"]),
        "0".parse().unwrap()
    );
    assert_eq!(momentum.values["direction_1m"], json!("flat"));
}

/// Warm-up boundary: the five-minute longest horizon needs 301 contiguous
/// one-second samples. At 300 samples both identities are still warming (not
/// ready, `volatility_warming_up`); the 301st contiguous sample makes them ready.
#[test]
fn probability_ring_is_not_ready_until_the_full_warm_up() {
    let (mut processor, batch_300) =
        warm_probability("kalshi-prob-warmup", 300, |_| ("0.49", "0.51"));
    for id in [PROBABILITY_MOMENTUM_ID, PROBABILITY_VOLATILITY_ID] {
        let warming = update(&batch_300, id);
        assert_ne!(warming.quality.state, QualityState::Ready, "{id}");
        assert!(warming.values.is_empty(), "{id}");
        assert!(
            warming
                .quality
                .reasons
                .contains(&"volatility_warming_up".to_owned()),
            "{id}"
        );
    }

    processor
        .ingest(
            &kalshi_quote_at(301, 1, 301, "0.49", "0.51"),
            ns(301),
            ns(301),
        )
        .expect("the 301st quote admits");
    let batch_301 = processor
        .health_tick(ns(301))
        .expect("the 301st health tick emits");
    for id in [PROBABILITY_MOMENTUM_ID, PROBABILITY_VOLATILITY_ID] {
        let ready = update(&batch_301, id);
        assert_eq!(ready.quality.state, QualityState::Ready, "{id}");
    }
}

/// Continuity break: once ready, a missing one-second observation (a two-second
/// gap between health ticks) breaks the ring and restarts the warm-up, so the
/// next decision is no longer ready.
#[test]
fn probability_ring_continuity_break_restarts_the_warm_up() {
    let (mut processor, batch_301) =
        warm_probability("kalshi-prob-continuity", 301, |_| ("0.49", "0.51"));
    assert_eq!(
        update(&batch_301, PROBABILITY_MOMENTUM_ID).quality.state,
        QualityState::Ready
    );

    // Skip second 302: the next tick at second 303 is a two-second gap, which is
    // a missing observation that clears the ring (spec §3, no gap bridging).
    processor
        .ingest(
            &kalshi_quote_at(303, 1, 303, "0.49", "0.51"),
            ns(303),
            ns(303),
        )
        .expect("the post-gap quote admits");
    let batch_303 = processor
        .health_tick(ns(303))
        .expect("the post-gap health tick emits");
    for id in [PROBABILITY_MOMENTUM_ID, PROBABILITY_VOLATILITY_ID] {
        let broken = update(&batch_303, id);
        assert_ne!(broken.quality.state, QualityState::Ready, "{id}");
        assert!(
            broken
                .quality
                .reasons
                .contains(&"volatility_warming_up".to_owned()),
            "{id}"
        );
    }
}

/// A generation roll resets the ring. Generation 1 warms to 200 contiguous
/// samples, then generation 2 binds and is fed 101 more contiguous samples
/// (seconds 201..=301). The timestamps never gap, so were the ring not
/// per-generation it would hold 301 contiguous samples and be ready; because the
/// roll discards generation 1's samples, generation 2 has only 101 and is still
/// warming.
#[test]
fn probability_ring_resets_on_a_generation_roll() {
    let (mut processor, _) = warm_probability("kalshi-prob-roll", 200, |_| ("0.49", "0.51"));

    // Roll to generation 2 with contiguous timestamps (second 200 -> 201).
    processor
        .mark_unavailable(ns(201), "scheduled_rollover")
        .expect("generation 1 rolls off");
    processor
        .bind_market(2, &kalshi_contract(0, 2, 0), 0, ns(201))
        .expect("generation 2 binds");
    processor
        .ingest(&kalshi_trade(0, 2, 201), ns(201), ns(201))
        .expect("generation 2 trade admits");

    let mut batch = PredictionBatch::default();
    for second in 201..=301 {
        processor
            .ingest(
                &kalshi_quote_at(second, 2, second, "0.49", "0.51"),
                ns(second),
                ns(second),
            )
            .expect("generation 2 quote admits");
        batch = processor
            .health_tick(ns(second))
            .expect("generation 2 health tick emits");
    }
    for id in [PROBABILITY_MOMENTUM_ID, PROBABILITY_VOLATILITY_ID] {
        let after_roll = update(&batch, id);
        assert_ne!(
            after_roll.quality.state,
            QualityState::Ready,
            "the roll discarded generation 1's samples, so generation 2 still warms: {id}"
        );
    }
}

// ---------------------------------------------------------------------------
// C2 (ruling E49) — per-venue probability horizons. Polymarket's 300 s markets
// can never fill a five-minute ring, so its probability momentum/volatility pair
// runs on the shorter 10 s / 30 s / 2 m horizons (ready at 121 contiguous
// samples) while Kalshi keeps 10 s / 1 m / 5 m (ready at 301). Both rings are
// sampled on the one-second health tick.
// ---------------------------------------------------------------------------

const POLY_PROBABILITY_MOMENTUM_ID: &str = "btc.polymarket.probability_momentum";
const POLY_PROBABILITY_VOLATILITY_ID: &str = "btc.polymarket.probability_volatility";

/// Non-flat hand-computed guard: a deterministic Polymarket midpoint path — flat
/// at 0.15 through second 91, then a +1 pp/s ramp (seconds 92..=111) and a
/// +5 pp/s ramp (seconds 112..=121) — pins every horizon to a value hand-derived
/// from the 10 s / 30 s / 2 m offsets, so a wrong offset changes at least one
/// asserted number and fails. At the ready tick (second 121, the ring holds the
/// samples for seconds 1..=121) the newest is mid(121)=0.85, and the offsets read
/// mid(111)=0.35 (10 s back), mid(91)=0.15 (30 s back) and mid(1)=0.15 (2 m back):
///   return_pp_10s = 100*(0.85-0.35) = 50
///   return_pp_30s = 100*(0.85-0.15) = 70  (direction_30s up)
///   return_pp_2m  = 100*(0.85-0.15) = 70
/// The 120 one-second returns are 90 zeros, then 20 of +1 pp, then 10 of +5 pp,
/// so each realized-volatility window is the RMS of exactly its offset's returns:
///   volatility_pp_10s = sqrt((10*5^2)/10)            = 5
///   volatility_pp_30s = sqrt((20*1^2 + 10*5^2)/30)   = sqrt(270/30)  = 3
///   volatility_pp_2m  = sqrt((90*0 + 20*1^2 + 10*5^2)/120) = sqrt(270/120) = 1.5
///   short_long_ratio  = 5 / 1.5 = 3.33333333  (>= 1.2, regime expanding)
/// Keeps the 120-warming / 121-ready boundary, proves the horizons are per venue
/// (Kalshi is still warming at 121, needing 301), and validates the populated
/// Polymarket and Kalshi probability updates against their definition JSON.
#[test]
fn polymarket_probability_is_hand_computed_and_offset_pinned() {
    // Midpoint in hundredths for a given second: flat 0.15 through 91, a +0.01/s
    // ramp to 0.35 at 111, then a +0.05/s ramp to 0.85 at 121.
    fn mid_hundredths(second: u64) -> u64 {
        if second <= 91 {
            15
        } else if second <= 111 {
            15 + (second - 91)
        } else {
            35 + 5 * (second - 111)
        }
    }
    // A one-cent spread around the midpoint, both legs two-decimal probabilities.
    fn poly_leg(second: u64) -> (String, String) {
        let mid = mid_hundredths(second);
        (format!("0.{:02}", mid - 1), format!("0.{:02}", mid + 1))
    }

    let mut processor = PredictionSignalProcessor::new(
        PredictionMarketVenue::Polymarket,
        "poly-prob-hand",
        wall_start(),
    )
    .expect("Polymarket processor constructs");
    processor
        .ingest(&poly_twap(0, 0, "60000"), 0, 0)
        .expect("opening TWAP is admitted");
    processor
        .bind_market(1, &poly_contract(0, 1, 0), 0, 0)
        .expect("Polymarket market binds");
    processor
        .ingest(&poly_trade(0, "up", "buy", "0.50", "1", 1), ns(1), ns(1))
        .expect("Polymarket trade admits");

    // Drive seconds 1..=121 along the path; capture the batch at 120 (warming,
    // one short of the two-minute horizon) and at 121 (ready).
    let mut batch_120 = PredictionBatch::default();
    let mut batch_121 = PredictionBatch::default();
    for second in 1..=121 {
        let (bid, ask) = poly_leg(second);
        for event in [
            poly_quote(2 * second, 1, "up", &bid, &ask, second),
            poly_quote(2 * second + 1, 1, "down", &bid, &ask, second),
        ] {
            processor
                .ingest(&event, ns(second), ns(second))
                .expect("native quote is admitted");
        }
        let batch = processor
            .health_tick(ns(second))
            .expect("health tick emits a batch");
        if second == 120 {
            batch_120 = batch;
        } else if second == 121 {
            batch_121 = batch;
        }
    }

    // Boundary: 120 contiguous samples is one short of the two-minute horizon.
    for id in [POLY_PROBABILITY_MOMENTUM_ID, POLY_PROBABILITY_VOLATILITY_ID] {
        let warming = update(&batch_120, id);
        assert_ne!(warming.quality.state, QualityState::Ready, "{id}");
        assert!(warming.values.is_empty(), "{id}");
    }

    // The 121st contiguous sample makes both identities ready with the
    // shorter-horizon field names (10 s / 30 s / 2 m) and the hand-computed values.
    let momentum = update(&batch_121, POLY_PROBABILITY_MOMENTUM_ID);
    assert_eq!(momentum.quality.state, QualityState::Ready);
    assert_eq!(
        as_decimal(&momentum.values["return_pp_10s"]),
        "50".parse().unwrap()
    );
    assert_eq!(
        as_decimal(&momentum.values["return_pp_30s"]),
        "70".parse().unwrap()
    );
    assert_eq!(
        as_decimal(&momentum.values["return_pp_2m"]),
        "70".parse().unwrap()
    );
    assert_eq!(momentum.values["direction_30s"], json!("up"));

    let volatility = update(&batch_121, POLY_PROBABILITY_VOLATILITY_ID);
    assert_eq!(volatility.quality.state, QualityState::Ready);
    assert_eq!(
        as_decimal(&volatility.values["volatility_pp_10s"]),
        "5".parse().unwrap()
    );
    assert_eq!(
        as_decimal(&volatility.values["volatility_pp_30s"]),
        "3".parse().unwrap()
    );
    assert_eq!(
        as_decimal(&volatility.values["volatility_pp_2m"]),
        "1.5".parse().unwrap()
    );
    assert_eq!(
        as_decimal(&volatility.values["short_long_ratio"]),
        "3.33333333".parse().unwrap()
    );
    assert_eq!(volatility.values["regime"], json!("expanding"));

    // The populated Polymarket updates validate against their definition JSON
    // (field names, types, 8 dp).
    let poly_definitions = definitions_of(&processor);
    assert_probability_updates_valid(
        &poly_definitions,
        &batch_121,
        [POLY_PROBABILITY_MOMENTUM_ID, POLY_PROBABILITY_VOLATILITY_ID],
    );

    // Kalshi at the same 121 contiguous samples is still warming: its longest
    // horizon needs 301.
    let (_, kalshi_121) = warm_probability("kalshi-prob-121", 121, |_| ("0.49", "0.51"));
    for id in [PROBABILITY_MOMENTUM_ID, PROBABILITY_VOLATILITY_ID] {
        let warming = update(&kalshi_121, id);
        assert_ne!(
            warming.quality.state,
            QualityState::Ready,
            "Kalshi still needs 301 samples: {id}"
        );
    }

    // Warm Kalshi fully (301) and validate its populated probability updates too,
    // so the definition contract is exercised on a populated update for BOTH venues.
    let (kalshi_processor, kalshi_301) =
        warm_probability("kalshi-prob-301", 301, |_| ("0.49", "0.51"));
    let kalshi_definitions = definitions_of(&kalshi_processor);
    assert_probability_updates_valid(
        &kalshi_definitions,
        &kalshi_301,
        [PROBABILITY_MOMENTUM_ID, PROBABILITY_VOLATILITY_ID],
    );
}
