//! Unit tests for the `btc.multi.*` cross-venue prediction blends (C1, E57).
//!
//! Every reading is driven by synthetic Kalshi/Polymarket published updates fed
//! through [`MultiProcessor::observe`], so the tests exercise the same
//! harvest-then-evaluate path the runtime uses. Values are hand-computed.

use super::*;
use signals_core::{
    Quality, QualityState, ScopeKind, SignalScope, validate_signal_scope_binding_for_definition,
};

const EPOCH: &str = "epoch-multi-test";
const PRODUCED_AT: &str = "2026-09-03T11:10:05.000000000Z";
const CLOSE: &str = "2026-09-03T11:15:00.000000000Z";
const KALSHI_OPEN: &str = "2026-09-03T11:00:00.000000000Z";
const POLYMARKET_OPEN: &str = "2026-09-03T11:10:00.000000000Z";

fn clock() -> LaneClock {
    LaneClock {
        now_ns: 305_000_000_000,
        produced_at: PRODUCED_AT.to_owned(),
    }
}

fn ready(signal_id: &str, values: Vec<(&str, Value)>) -> SignalUpdate {
    SignalUpdate {
        schema_version: SIGNAL_UPDATE_SCHEMA_VERSION,
        signal_id: signal_id.to_owned(),
        signal_version: "1.0.0".to_owned(),
        publisher_epoch: EPOCH.to_owned(),
        sequence: 0,
        event_time: PRODUCED_AT.to_owned(),
        produced_at: PRODUCED_AT.to_owned(),
        valid_from: PRODUCED_AT.to_owned(),
        valid_until: "2026-09-03T11:10:15.000000000Z".to_owned(),
        quality: Quality {
            state: QualityState::Ready,
            reasons: Vec::new(),
        },
        values: values.into_iter().map(|(k, v)| (k.to_owned(), v)).collect(),
        scope: SignalScope::Global,
    }
}

fn non_ready(signal_id: &str, reasons: &[&str]) -> SignalUpdate {
    let mut update = ready(signal_id, Vec::new());
    update.quality = Quality {
        state: QualityState::WarmingUp,
        reasons: reasons.iter().map(|r| (*r).to_owned()).collect(),
    };
    update
}

fn contract(
    signal_id: &str,
    market_id: &str,
    generation: u64,
    open: &str,
    strike: &str,
    benchmark: &str,
) -> SignalUpdate {
    ready(
        signal_id,
        vec![
            ("market_id", json!(market_id)),
            ("market_generation", json!(generation)),
            ("window_open", json!(open)),
            ("window_close", json!(CLOSE)),
            ("strike", json!(strike)),
            ("settlement_benchmark", json!(benchmark)),
            ("comparator", json!("greater_or_equal")),
            ("tie_outcome", json!("affirmative")),
        ],
    )
}

fn quote(signal_id: &str, bid: &str, ask: &str, mid: &str, neg_ask: &str) -> SignalUpdate {
    ready(
        signal_id,
        vec![
            ("affirmative_bid", json!(bid)),
            ("affirmative_ask", json!(ask)),
            ("affirmative_midpoint", json!(mid)),
            ("negative_ask", json!(neg_ask)),
            ("quote_time", json!(PRODUCED_AT)),
            ("max_quote_age_ms", json!(0)),
        ],
    )
}

fn fair(signal_id: &str, probability: &str) -> SignalUpdate {
    ready(signal_id, vec![("probability", json!(probability))])
}

fn kalshi_pair_cost(pair: &str, fee_adj: &str, yes: &str, no: &str) -> SignalUpdate {
    ready(
        KALSHI_PAIR_COST_ID,
        vec![
            ("pair_cost", json!(pair)),
            ("fee_adjusted_pair_cost", json!(fee_adj)),
            ("yes_leg_fee", json!(yes)),
            ("no_leg_fee", json!(no)),
            ("premium_threshold", json!("0.01")),
        ],
    )
}

fn polymarket_pair_cost(pair: &str, fee_adj: &str, up: &str, down: &str) -> SignalUpdate {
    ready(
        POLYMARKET_PAIR_COST_ID,
        vec![
            ("pair_cost", json!(pair)),
            ("fee_adjusted_pair_cost", json!(fee_adj)),
            ("up_leg_fee", json!(up)),
            ("down_leg_fee", json!(down)),
            ("premium_threshold", json!("0.01")),
        ],
    )
}

/// Bind both venues to the canonical aligned pair. `kalshi_fair`/`poly_fair` set
/// the two fair-probability legs; the quotes and pair-costs are the canonical
/// values used by the arithmetic tests.
fn bind_canonical(processor: &mut MultiProcessor, kalshi_fair: &str, poly_fair: &str) {
    processor
        .observe(
            MultiVenue::Kalshi,
            &[
                contract(
                    KALSHI_CONTRACT_ID,
                    "KXBTC15M-A",
                    3,
                    KALSHI_OPEN,
                    "111500",
                    "cf_benchmarks_brti_sixty_print",
                ),
                quote(KALSHI_QUOTE_ID, "0.40", "0.42", "0.41", "0.60"),
                fair(KALSHI_FAIR_ID, kalshi_fair),
                kalshi_pair_cost("0.98", "0.99", "0.01", "0.01"),
            ],
        )
        .expect("kalshi observe");
    processor
        .observe(
            MultiVenue::Polymarket,
            &[
                contract(
                    POLYMARKET_CONTRACT_ID,
                    "0xpoly",
                    7,
                    POLYMARKET_OPEN,
                    "111600",
                    "chainlink_btc_usd_twap_60s",
                ),
                quote(POLYMARKET_QUOTE_ID, "0.44", "0.46", "0.45", "0.56"),
                fair(POLYMARKET_FAIR_ID, poly_fair),
                polymarket_pair_cost("0.97", "0.985", "0.005", "0.005"),
            ],
        )
        .expect("polymarket observe");
}

fn find<'a>(updates: &'a [SignalUpdate], id: &str) -> &'a SignalUpdate {
    updates
        .iter()
        .find(|u| u.signal_id == id)
        .unwrap_or_else(|| panic!("{id} not published; got {updates:?}"))
}

fn dec(update: &SignalUpdate, field: &str) -> String {
    update
        .values
        .get(field)
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("{field} absent/not-a-string"))
        .to_owned()
}

/// Assert a decimal field equals `expected` by value (robust to trailing-zero
/// rendering: `round_dp` does not pad to eight places).
fn assert_dec(update: &SignalUpdate, field: &str, expected: &str) {
    let got = Decimal::from_str(&dec(update, field))
        .unwrap_or_else(|_| panic!("{field} is not a decimal"));
    let want = Decimal::from_str(expected).expect("expected decimal");
    assert_eq!(got, want, "field {field}");
}

#[test]
fn both_venues_bound_publishes_both_identities_with_multi_market_scope() {
    let mut processor = MultiProcessor::new(EPOCH).expect("processor");
    bind_canonical(&mut processor, "0.410", "0.470");
    let out = processor.evaluate(&clock()).expect("evaluate");
    assert_eq!(out.len(), 2, "both identities publish fresh");

    for update in &out {
        assert_eq!(update.quality.state, QualityState::Ready);
        match &update.scope {
            SignalScope::MultiMarket { legs, window } => {
                assert_eq!(legs.len(), 2);
                assert_eq!(legs[0].venue, "kalshi");
                assert_eq!(legs[0].market_id, "KXBTC15M-A");
                assert_eq!(legs[0].market_generation, 3);
                assert_eq!(legs[1].venue, "polymarket");
                assert_eq!(legs[1].market_id, "0xpoly");
                assert_eq!(legs[1].market_generation, 7);
                assert_eq!(window.close, CLOSE);
                // Overlapping window opens at the later of the two opens.
                assert_eq!(window.open, POLYMARKET_OPEN);
            }
            other => panic!("expected multi_market scope, got {other:?}"),
        }
    }
}

#[test]
fn prediction_dislocation_formulas_exact() {
    let mut processor = MultiProcessor::new(EPOCH).expect("processor");
    bind_canonical(&mut processor, "0.410", "0.470");
    let out = processor.evaluate(&clock()).expect("evaluate");
    let d = find(&out, PREDICTION_DISLOCATION_ID);

    assert_dec(d, "implied_up_kalshi", "0.41000000");
    assert_dec(d, "implied_up_polymarket", "0.45000000");
    // dislocation = 0.45 - 0.41 = 0.04; bps = 400.
    assert_dec(d, "dislocation", "0.04000000");
    assert_dec(d, "dislocation_bps", "400.00000000");
    // model_adjustment = 0.470 - 0.410 = 0.06; strike-adjusted = 0.04 - 0.06 = -0.02.
    assert_dec(d, "model_adjustment", "0.06000000");
    assert_dec(d, "strike_adjusted_dislocation", "-0.02000000");
    // strike diff = 111600 - 111500 = 100; bps = 100/111500*10000.
    assert_dec(d, "strike_difference_absolute", "100.00000000");
    assert_dec(d, "strike_difference_bps", "8.96860987");
    // abs(-0.02) is not > threshold 0.02 -> aligned; sign < 0 -> kalshi_rich.
    assert_eq!(d.values["state"], json!("aligned"));
    assert_eq!(d.values["direction"], json!("kalshi_rich"));
    assert_eq!(d.values["dislocation_threshold"], json!("0.02"));
    assert_eq!(d.values["window_alignment"], json!("exact_close"));
    // Different benchmarks today -> underlying_mismatch true; same tie rule -> false.
    assert_eq!(d.values["underlying_mismatch"], json!(true));
    assert_eq!(d.values["tie_rule_mismatch"], json!(false));
    // close 11:15:00 - produced 11:10:05 = 295000 ms.
    assert_eq!(d.values["time_to_close_ms"], json!(295_000));
    assert_eq!(d.values["observation_time"], json!(PRODUCED_AT));
}

#[test]
fn prediction_dislocation_state_and_direction_flip_when_model_adjustment_small() {
    // poly fair 0.40, kalshi fair 0.41 -> model_adjustment = -0.01;
    // strike-adjusted = 0.04 - (-0.01) = 0.05 > 0.02 -> dislocated, polymarket_rich.
    let mut processor = MultiProcessor::new(EPOCH).expect("processor");
    bind_canonical(&mut processor, "0.41", "0.40");
    let out = processor.evaluate(&clock()).expect("evaluate");
    let d = find(&out, PREDICTION_DISLOCATION_ID);
    assert_dec(d, "strike_adjusted_dislocation", "0.05000000");
    assert_eq!(d.values["state"], json!("dislocated"));
    assert_eq!(d.values["direction"], json!("polymarket_rich"));
}

#[test]
fn pair_cost_spread_formulas_exact() {
    let mut processor = MultiProcessor::new(EPOCH).expect("processor");
    bind_canonical(&mut processor, "0.410", "0.470");
    let out = processor.evaluate(&clock()).expect("evaluate");
    let p = find(&out, PAIR_COST_SPREAD_ID);

    assert_dec(p, "kalshi_fee_adjusted_pair_cost", "0.99000000");
    assert_dec(p, "polymarket_fee_adjusted_pair_cost", "0.98500000");
    // spread = 0.985 - 0.99 = -0.005.
    assert_dec(p, "pair_cost_spread", "-0.00500000");
    assert_eq!(p.values["cheaper_venue"], json!("polymarket"));
    // cross_up_kalshi = (0.42 + 0.01) + (0.56 + 0.005) - 1 = -0.005.
    assert_dec(p, "cross_pair_cost_up_kalshi", "-0.00500000");
    // cross_up_polymarket = (0.46 + 0.005) + (0.60 + 0.01) - 1 = 0.075.
    assert_dec(p, "cross_pair_cost_up_polymarket", "0.07500000");
    assert_dec(p, "best_cross_pair_cost", "-0.00500000");
    assert_eq!(
        p.values["best_cross_direction"],
        json!("up_kalshi_down_polymarket")
    );
    // best cross < 0 -> locked_profit.
    assert_eq!(p.values["state"], json!("locked_profit"));
    assert_eq!(p.values["premium_threshold"], json!("0.01"));
    // benchmarks and strikes differ -> conditional lock.
    assert_eq!(p.values["lock_is_conditional"], json!(true));
}

#[test]
fn pair_cost_spread_state_boundaries_fair_and_premium() {
    // Drive best_cross into the `premium` band (> 0.01) by widening the asks.
    let mut processor = MultiProcessor::new(EPOCH).expect("processor");
    processor
        .observe(
            MultiVenue::Kalshi,
            &[
                contract(
                    KALSHI_CONTRACT_ID,
                    "KXBTC15M-A",
                    1,
                    KALSHI_OPEN,
                    "111500",
                    "cf_benchmarks_brti_sixty_print",
                ),
                quote(KALSHI_QUOTE_ID, "0.50", "0.55", "0.525", "0.55"),
                fair(KALSHI_FAIR_ID, "0.5"),
                kalshi_pair_cost("1.0", "1.0", "0.02", "0.02"),
            ],
        )
        .expect("kalshi");
    processor
        .observe(
            MultiVenue::Polymarket,
            &[
                contract(
                    POLYMARKET_CONTRACT_ID,
                    "0xpoly",
                    1,
                    POLYMARKET_OPEN,
                    "111500",
                    "cf_benchmarks_brti_sixty_print",
                ),
                quote(POLYMARKET_QUOTE_ID, "0.50", "0.55", "0.525", "0.55"),
                fair(POLYMARKET_FAIR_ID, "0.5"),
                polymarket_pair_cost("1.0", "1.0", "0.02", "0.02"),
            ],
        )
        .expect("poly");
    let out = processor.evaluate(&clock()).expect("evaluate");
    let p = find(&out, PAIR_COST_SPREAD_ID);
    // cross = (0.55 + 0.02) + (0.55 + 0.02) - 1 = 0.14 > 0.01 -> premium.
    assert_dec(p, "best_cross_pair_cost", "0.14000000");
    assert_eq!(p.values["state"], json!("premium"));
    // Same benchmark/comparator/tie/strike -> not conditional.
    assert_eq!(p.values["lock_is_conditional"], json!(false));
    assert_eq!(p.values["cheaper_venue"], json!("tied"));
}

#[test]
fn one_venue_unbound_withholds_both_blends() {
    // One leg unbound => no valid two-leg multi_market instance (V2), so the lane
    // withholds entirely rather than publish a Global/partial-leg scope the
    // verifier rejects (E20). The terminal invalidation is the runtime's job.
    let mut processor = MultiProcessor::new(EPOCH).expect("processor");
    bind_canonical(&mut processor, "0.41", "0.47");
    let warm = processor.evaluate(&clock()).expect("warm");
    assert_eq!(warm.len(), 2, "the aligned pair publishes both blends");
    processor.mark_leg_unavailable(MultiVenue::Polymarket);
    let out = processor.evaluate(&clock()).expect("evaluate");
    assert!(
        out.is_empty(),
        "an unbound leg withholds both blends, got {out:?}"
    );
}

#[test]
fn absent_counterparty_withholds_both_blends() {
    // Only Kalshi observed: no two-leg instance, so the lane withholds (never a
    // Global scope, E20).
    let mut processor = MultiProcessor::new(EPOCH).expect("processor");
    processor
        .observe(
            MultiVenue::Kalshi,
            &[contract(
                KALSHI_CONTRACT_ID,
                "KXBTC15M-A",
                1,
                KALSHI_OPEN,
                "111500",
                "cf_benchmarks_brti_sixty_print",
            )],
        )
        .expect("kalshi");
    let out = processor.evaluate(&clock()).expect("evaluate");
    assert!(
        out.is_empty(),
        "an absent counterparty withholds, got {out:?}"
    );
}

#[test]
fn window_alignment_mismatch_withholds_both_blends() {
    // Misaligned closes bind no instance (W2), so the lane withholds.
    let mut processor = MultiProcessor::new(EPOCH).expect("processor");
    processor
        .observe(
            MultiVenue::Kalshi,
            &[contract(
                KALSHI_CONTRACT_ID,
                "KXBTC15M-A",
                1,
                KALSHI_OPEN,
                "111500",
                "cf_benchmarks_brti_sixty_print",
            )],
        )
        .expect("kalshi");
    let mut poly = contract(
        POLYMARKET_CONTRACT_ID,
        "0xpoly",
        1,
        POLYMARKET_OPEN,
        "111500",
        "chainlink_btc_usd_twap_60s",
    );
    poly.values.insert(
        "window_close".to_owned(),
        json!("2026-09-03T11:20:00.000000000Z"),
    );
    processor
        .observe(MultiVenue::Polymarket, &[poly])
        .expect("poly");
    let out = processor.evaluate(&clock()).expect("evaluate");
    assert!(
        out.is_empty(),
        "misaligned windows bind no instance (W2); the lane withholds, got {out:?}"
    );
    // The aligned scope helper reports None while unaligned.
    assert!(
        processor
            .aligned_scope(PRODUCED_AT)
            .expect("align")
            .is_none()
    );
}

#[test]
fn rollover_on_one_venue_changes_the_scope_generation() {
    let mut processor = MultiProcessor::new(EPOCH).expect("processor");
    bind_canonical(&mut processor, "0.41", "0.47");
    let first = processor.evaluate(&clock()).expect("first");
    let g1 = match &find(&first, PREDICTION_DISLOCATION_ID).scope {
        SignalScope::MultiMarket { legs, .. } => legs[0].market_generation,
        other => panic!("{other:?}"),
    };
    assert_eq!(g1, 3);

    // Kalshi rolls to generation 4 with a new market id; re-observe its bind.
    processor
        .observe(
            MultiVenue::Kalshi,
            &[contract(
                KALSHI_CONTRACT_ID,
                "KXBTC15M-B",
                4,
                KALSHI_OPEN,
                "111500",
                "cf_benchmarks_brti_sixty_print",
            )],
        )
        .expect("kalshi roll");
    let second = processor.evaluate(&clock()).expect("second");
    let d = find(&second, PREDICTION_DISLOCATION_ID);
    match &d.scope {
        SignalScope::MultiMarket { legs, .. } => {
            assert_eq!(legs[0].market_id, "KXBTC15M-B");
            assert_eq!(legs[0].market_generation, 4);
            assert_eq!(legs[1].market_generation, 7);
        }
        other => panic!("{other:?}"),
    }
    // The new instance publishes fresh (a different scope key, V10).
    assert_eq!(d.sequence, 1, "the new instance consumes the next sequence");
}

#[test]
fn stale_quote_is_not_ready() {
    let mut processor = MultiProcessor::new(EPOCH).expect("processor");
    bind_canonical(&mut processor, "0.41", "0.47");
    // Overwrite the kalshi quote with one 20 s old (> 10 s bound).
    let mut stale = quote(KALSHI_QUOTE_ID, "0.40", "0.42", "0.41", "0.60");
    stale.values.insert(
        "quote_time".to_owned(),
        json!("2026-09-03T11:09:45.000000000Z"),
    );
    processor
        .observe(MultiVenue::Kalshi, &[stale])
        .expect("stale");
    let out = processor.evaluate(&clock()).expect("evaluate");
    let d = find(&out, PREDICTION_DISLOCATION_ID);
    assert!(
        d.quality
            .reasons
            .contains(&"affirmative_quote_stale".to_owned()),
        "got {:?}",
        d.quality.reasons
    );
}

#[test]
fn non_ready_fair_probability_propagates_its_reason() {
    let mut processor = MultiProcessor::new(EPOCH).expect("processor");
    bind_canonical(&mut processor, "0.41", "0.47");
    // Replace the kalshi fair probability with a non-ready warm-up.
    processor
        .observe(
            MultiVenue::Kalshi,
            &[non_ready(KALSHI_FAIR_ID, &["volatility_warming_up"])],
        )
        .expect("warmup");
    let out = processor.evaluate(&clock()).expect("evaluate");
    let d = find(&out, PREDICTION_DISLOCATION_ID);
    assert!(
        d.quality
            .reasons
            .contains(&"volatility_warming_up".to_owned()),
        "got {:?}",
        d.quality.reasons
    );
    // The pair-cost identity does not consume fair probability, so it stays ready.
    let p = find(&out, PAIR_COST_SPREAD_ID);
    assert_eq!(p.quality.state, QualityState::Ready);
}

#[test]
fn coalescing_suppresses_an_unchanged_ready_restatement() {
    let mut processor = MultiProcessor::new(EPOCH).expect("processor");
    bind_canonical(&mut processor, "0.41", "0.47");
    let first = processor.evaluate(&clock()).expect("first");
    assert_eq!(first.len(), 2);
    // A second evaluation at the same logical time with identical inputs is
    // suppressed by the interval-and-change-key rule.
    let mut same = clock();
    same.now_ns += 1;
    let second = processor.evaluate(&same).expect("second");
    assert!(second.is_empty(), "unchanged restatement is coalesced");
}

#[test]
fn checkpoint_round_trip_preserves_emitter_memory() {
    let mut processor = MultiProcessor::new(EPOCH).expect("processor");
    bind_canonical(&mut processor, "0.41", "0.47");
    let _ = processor.evaluate(&clock()).expect("warm");
    let state = processor.checkpoint_state();
    let json = serde_json::to_string(&state).expect("serialize");
    let restored: MultiCheckpointState = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(state, restored);

    let mut resumed = MultiProcessor::new(EPOCH).expect("processor");
    resumed.restore_state(restored);
    // The restored emitter memory (ever_ready, lifetime output count) round-trips.
    assert_eq!(resumed.checkpoint_state(), state);
}

#[test]
fn ready_multi_frame_is_verifier_accepted_and_global_is_rejected() {
    // A READY blend on the aligned pair carries a `multi_market` scope that the
    // package verifier's per-frame binding check accepts (E20), while the same
    // identity under a Global scope is rejected — the exact rule the BLOCKER fix
    // satisfies by withholding rather than ever stamping Global.
    let mut processor = MultiProcessor::new(EPOCH).expect("processor");
    bind_canonical(&mut processor, "0.410", "0.470");
    let out = processor.evaluate(&clock()).expect("evaluate");
    for update in &out {
        assert!(matches!(update.scope, SignalScope::MultiMarket { .. }));
        validate_signal_scope_binding_for_definition(
            &update.signal_id,
            &update.scope,
            Some(ScopeKind::MultiMarket),
        )
        .expect("a multi_market frame is verifier-accepted");
        assert!(
            validate_signal_scope_binding_for_definition(
                &update.signal_id,
                &SignalScope::Global,
                Some(ScopeKind::MultiMarket),
            )
            .is_err(),
            "a Global scope on a multi_market identity must be rejected (E20)"
        );
    }
    // The aligned scope helper reports the bound instance's scope.
    assert!(
        processor
            .aligned_scope(PRODUCED_AT)
            .expect("align")
            .is_some()
    );
}

#[test]
fn embedded_definitions_match_the_published_identities() {
    let processor = MultiProcessor::new(EPOCH).expect("processor");
    let ids: Vec<String> = processor
        .definitions()
        .into_iter()
        .map(|d| d.signal_id)
        .collect();
    assert_eq!(ids, vec![PREDICTION_DISLOCATION_ID, PAIR_COST_SPREAD_ID]);
    assert_eq!(multi_signal_ids().len(), MULTI_SIGNAL_COUNT);
}
