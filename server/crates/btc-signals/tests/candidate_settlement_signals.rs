//! Settlement-anchor computations from the candidate module, both of which the
//! Polymarket prediction processor publishes.
//!
//! Ready assertions carry their arithmetic in comments so the exact expected
//! value is auditable. Prices divide into exact basis-point integers.

use std::collections::BTreeSet;
use std::path::Path;

use btc_signals::candidate_settlement_signals::{
    AnchorOutcome, AnchorSettlementState, CANDIDATE_SETTLEMENT_SIGNAL_VERSION, Comparator,
    DEFAULT_DIVERGED_THRESHOLD_BPS, DEFAULT_PIN_THRESHOLD_BPS, DEFAULT_PIN_WINDOW_MS,
    DivergenceOutcome, DivergenceState, KALSHI_ORACLE_DIVERGENCE_ID,
    KALSHI_SETTLEMENT_ANCHOR_STATE_ID, OracleDivergenceCandidate, POLYMARKET_ORACLE_DIVERGENCE_ID,
    POLYMARKET_SETTLEMENT_ANCHOR_STATE_ID, SettledSide, SettlementAnchorStateCandidate,
};
use rust_decimal::Decimal;

fn ns(seconds: u64) -> u64 {
    seconds * 1_000_000_000
}

// --- 1. settlement_anchor_state --------------------------------------------

/// Runs a contract + anchor pair with a 30s-to-close window and returns the
/// ready anchor state.
fn settled(anchor: i64, threshold: i64, tie_outcome: SettledSide) -> AnchorOutcome {
    let mut candidate = SettlementAnchorStateCandidate::with_defaults();
    candidate
        .observe_contract(
            ns(0),
            Decimal::from(threshold),
            Comparator::GreaterThanOrEqual,
            tie_outcome,
            ns(30),
        )
        .unwrap();
    candidate
        .observe_anchor(ns(0), Decimal::from(anchor))
        .unwrap()
}

#[test]
fn settlement_pinned_matches_hand_computed_distance() {
    let mut candidate = SettlementAnchorStateCandidate::with_defaults(); // pin 10 bps / 60000 ms
    // Contract first: anchor not seen yet -> not ready.
    assert_eq!(
        candidate
            .observe_contract(
                ns(0),
                Decimal::from(60000),
                Comparator::GreaterThanOrEqual,
                SettledSide::Up,
                ns(30),
            )
            .unwrap(),
        AnchorOutcome::NotReady {
            reason: "inputs_warming_up",
        }
    );
    // distance = (60030 - 60000) / 60000 * 1e4 = (30/60000) * 1e4 = 5 bps.
    // time_to_close = 30s - 0s = 30000 ms. |5| <= 10 and 30000 <= 60000 -> pinned.
    match candidate
        .observe_anchor(ns(0), Decimal::from(60030))
        .unwrap()
    {
        AnchorOutcome::Ready(state) => {
            assert_eq!(state.anchor_price, Decimal::from(60030));
            assert_eq!(state.threshold, Decimal::from(60000));
            assert_eq!(state.distance_bps, Decimal::from(5));
            assert_eq!(state.time_to_close_ms, 30000);
            assert_eq!(state.side_if_settled_now, SettledSide::Up);
            assert_eq!(state.state, AnchorSettlementState::Pinned);
        }
        other => panic!("expected ready anchor state, got {other:?}"),
    }
}

#[test]
fn settlement_pin_boundary_is_inclusive_on_both_conditions() {
    let mut candidate = SettlementAnchorStateCandidate::with_defaults();
    candidate
        .observe_contract(
            ns(0),
            Decimal::from(60000),
            Comparator::GreaterThanOrEqual,
            SettledSide::Up,
            ns(60), // exactly 60000 ms to close
        )
        .unwrap();
    // distance = (60060 - 60000)/60000 * 1e4 = 10 bps exactly; time = 60000 ms exactly.
    // Both bounds inclusive -> pinned.
    match candidate
        .observe_anchor(ns(0), Decimal::from(60060))
        .unwrap()
    {
        AnchorOutcome::Ready(state) => {
            assert_eq!(state.distance_bps, Decimal::from(10));
            assert_eq!(state.time_to_close_ms, 60000);
            assert_eq!(state.state, AnchorSettlementState::Pinned);
        }
        other => panic!("expected ready anchor state, got {other:?}"),
    }
}

#[test]
fn settlement_tracks_when_distance_or_window_exceeds_the_pin() {
    // Distance too far (20 bps) even though the window is near.
    let mut far_distance = SettlementAnchorStateCandidate::with_defaults();
    far_distance
        .observe_contract(
            ns(0),
            Decimal::from(60000),
            Comparator::GreaterThanOrEqual,
            SettledSide::Up,
            ns(30),
        )
        .unwrap();
    match far_distance
        .observe_anchor(ns(0), Decimal::from(60120))
        .unwrap()
    {
        AnchorOutcome::Ready(state) => {
            assert_eq!(state.distance_bps, Decimal::from(20));
            assert_eq!(state.state, AnchorSettlementState::Tracking);
        }
        other => panic!("got {other:?}"),
    }

    // Window too far (120000 ms) even though the distance is inside the band.
    let mut far_window = SettlementAnchorStateCandidate::with_defaults();
    far_window
        .observe_contract(
            ns(0),
            Decimal::from(60000),
            Comparator::GreaterThanOrEqual,
            SettledSide::Up,
            ns(120),
        )
        .unwrap();
    match far_window
        .observe_anchor(ns(0), Decimal::from(60030))
        .unwrap()
    {
        AnchorOutcome::Ready(state) => {
            assert_eq!(state.time_to_close_ms, 120000);
            assert_eq!(state.state, AnchorSettlementState::Tracking);
        }
        other => panic!("got {other:?}"),
    }
}

#[test]
fn settlement_comparator_decides_side_and_exact_ties_defer_to_tie_outcome() {
    // Strict comparison: above -> up, below -> down.
    match settled(60060, 60000, SettledSide::Up) {
        AnchorOutcome::Ready(state) => {
            assert_eq!(state.side_if_settled_now, SettledSide::Up);
            assert_eq!(state.distance_bps, Decimal::from(10));
        }
        other => panic!("got {other:?}"),
    }
    match settled(59940, 60000, SettledSide::Up) {
        AnchorOutcome::Ready(state) => {
            assert_eq!(state.side_if_settled_now, SettledSide::Down);
            assert_eq!(state.distance_bps, Decimal::from(-10));
        }
        other => panic!("got {other:?}"),
    }
    // Exact tie defers to the contract's tie outcome (up / down / void).
    for tie in [SettledSide::Up, SettledSide::Down, SettledSide::Void] {
        match settled(60000, 60000, tie) {
            AnchorOutcome::Ready(state) => {
                assert_eq!(state.distance_bps, Decimal::ZERO);
                assert_eq!(state.side_if_settled_now, tie);
            }
            other => panic!("got {other:?}"),
        }
    }
}

#[test]
fn settlement_is_not_ready_when_the_window_is_closed() {
    // Close strictly before now.
    let mut past = SettlementAnchorStateCandidate::with_defaults();
    past.observe_contract(
        ns(100),
        Decimal::from(60000),
        Comparator::GreaterThanOrEqual,
        SettledSide::Up,
        ns(60),
    )
    .unwrap();
    assert_eq!(
        past.observe_anchor(ns(100), Decimal::from(60030)).unwrap(),
        AnchorOutcome::NotReady {
            reason: "window_closed",
        }
    );

    // Close exactly at now is also closed (no time remaining).
    let mut at_close = SettlementAnchorStateCandidate::with_defaults();
    at_close
        .observe_contract(
            ns(100),
            Decimal::from(60000),
            Comparator::GreaterThanOrEqual,
            SettledSide::Up,
            ns(100),
        )
        .unwrap();
    assert_eq!(
        at_close
            .observe_anchor(ns(100), Decimal::from(60030))
            .unwrap(),
        AnchorOutcome::NotReady {
            reason: "window_closed",
        }
    );
}

#[test]
fn settlement_is_not_ready_until_both_inputs_are_seen() {
    let mut anchor_only = SettlementAnchorStateCandidate::with_defaults();
    assert_eq!(
        anchor_only
            .observe_anchor(ns(0), Decimal::from(60030))
            .unwrap(),
        AnchorOutcome::NotReady {
            reason: "inputs_warming_up",
        }
    );

    let mut contract_only = SettlementAnchorStateCandidate::with_defaults();
    assert_eq!(
        contract_only
            .observe_contract(
                ns(0),
                Decimal::from(60000),
                Comparator::GreaterThanOrEqual,
                SettledSide::Up,
                ns(30),
            )
            .unwrap(),
        AnchorOutcome::NotReady {
            reason: "inputs_warming_up",
        }
    );
}

// --- 2. oracle_divergence --------------------------------------------------

#[test]
fn oracle_aligned_matches_hand_computed_divergence() {
    let mut candidate = OracleDivergenceCandidate::with_default_threshold(); // 10 bps
    // Anchor first: spot not seen yet -> not ready.
    assert_eq!(
        candidate.observe_anchor(Decimal::from(60030)),
        DivergenceOutcome::NotReady {
            reason: "inputs_warming_up",
        }
    );
    // divergence = (60030 - 60000)/60000 * 1e4 = 5 bps; |5| < 10 -> aligned.
    match candidate.observe_spot_reference(Decimal::from(60000)) {
        DivergenceOutcome::Ready(context) => {
            assert_eq!(context.divergence_bps, Decimal::from(5));
            assert_eq!(context.anchor_price, Decimal::from(60030));
            assert_eq!(context.spot_reference_mid, Decimal::from(60000));
            assert_eq!(context.state, DivergenceState::Aligned);
        }
        other => panic!("expected ready divergence, got {other:?}"),
    }
}

#[test]
fn oracle_diverged_below_spot_carries_a_negative_sign() {
    let mut candidate = OracleDivergenceCandidate::with_default_threshold();
    candidate.observe_anchor(Decimal::from(59940));
    // divergence = (59940 - 60000)/60000 * 1e4 = -10 bps; |−10| is NOT < 10 -> diverged.
    match candidate.observe_spot_reference(Decimal::from(60000)) {
        DivergenceOutcome::Ready(context) => {
            assert_eq!(context.divergence_bps, Decimal::from(-10));
            assert_eq!(context.state, DivergenceState::Diverged);
        }
        other => panic!("expected ready divergence, got {other:?}"),
    }
}

#[test]
fn oracle_alignment_threshold_is_strict() {
    // 9 bps is strictly below 10 -> aligned.
    let mut aligned = OracleDivergenceCandidate::with_default_threshold();
    aligned.observe_anchor(Decimal::from(60054));
    match aligned.observe_spot_reference(Decimal::from(60000)) {
        DivergenceOutcome::Ready(context) => {
            assert_eq!(context.divergence_bps, Decimal::from(9));
            assert_eq!(context.state, DivergenceState::Aligned);
        }
        other => panic!("got {other:?}"),
    }
    // Exactly 10 bps is not below the threshold -> diverged.
    let mut boundary = OracleDivergenceCandidate::with_default_threshold();
    boundary.observe_anchor(Decimal::from(60060));
    match boundary.observe_spot_reference(Decimal::from(60000)) {
        DivergenceOutcome::Ready(context) => {
            assert_eq!(context.divergence_bps, Decimal::from(10));
            assert_eq!(context.state, DivergenceState::Diverged);
        }
        other => panic!("got {other:?}"),
    }
}

#[test]
fn oracle_zero_or_negative_spot_mid_is_not_ready() {
    let mut zero = OracleDivergenceCandidate::with_default_threshold();
    zero.observe_anchor(Decimal::from(60030));
    assert_eq!(
        zero.observe_spot_reference(Decimal::ZERO),
        DivergenceOutcome::NotReady {
            reason: "invalid_spot_mid",
        }
    );

    let mut negative = OracleDivergenceCandidate::with_default_threshold();
    negative.observe_anchor(Decimal::from(60030));
    assert_eq!(
        negative.observe_spot_reference(Decimal::from(-1)),
        DivergenceOutcome::NotReady {
            reason: "invalid_spot_mid",
        }
    );
}

#[test]
fn oracle_zero_or_negative_anchor_price_is_not_ready() {
    // Both inputs get the same invalid-observation disposition: a transient
    // bad anchor print pairs to not-ready, never a fabricated divergence.
    let mut zero = OracleDivergenceCandidate::with_default_threshold();
    zero.observe_spot_reference(Decimal::from(60000));
    assert_eq!(
        zero.observe_anchor(Decimal::ZERO),
        DivergenceOutcome::NotReady {
            reason: "invalid_anchor_price",
        }
    );

    let mut negative = OracleDivergenceCandidate::with_default_threshold();
    negative.observe_spot_reference(Decimal::from(60000));
    assert_eq!(
        negative.observe_anchor(Decimal::from(-1)),
        DivergenceOutcome::NotReady {
            reason: "invalid_anchor_price",
        }
    );
}

#[test]
fn oracle_is_not_ready_until_both_inputs_are_seen() {
    let mut anchor_only = OracleDivergenceCandidate::with_default_threshold();
    assert_eq!(
        anchor_only.observe_anchor(Decimal::from(60030)),
        DivergenceOutcome::NotReady {
            reason: "inputs_warming_up",
        }
    );
}

// --- drift guard: consts, defaults, and enum variants match the definitions --

fn definition_field_variants(base: &Path, file: &str, field_name: &str) -> BTreeSet<String> {
    let bytes =
        std::fs::read(base.join(file)).unwrap_or_else(|error| panic!("read {file}: {error}"));
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).unwrap_or_else(|error| panic!("parse {file}: {error}"));
    value["fields"]
        .as_array()
        .unwrap_or_else(|| panic!("{file} fields is an array"))
        .iter()
        .find(|field| field["name"].as_str() == Some(field_name))
        .unwrap_or_else(|| panic!("{file} has no field {field_name}"))["variants"]
        .as_array()
        .unwrap_or_else(|| panic!("{file} field {field_name} has no variants"))
        .iter()
        .map(|variant| {
            variant
                .as_str()
                .unwrap_or_else(|| panic!("{file} {field_name} variant is a string"))
                .to_owned()
        })
        .collect()
}

#[test]
fn candidate_settlement_ids_version_and_defaults_match_the_definitions() {
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../definitions/btc/signals");
    let cases = [
        (
            POLYMARKET_SETTLEMENT_ANCHOR_STATE_ID,
            "btc.polymarket.settlement_anchor_state@1.0.0.json",
            vec![
                format!("pin_threshold_bps defaults to {DEFAULT_PIN_THRESHOLD_BPS}"),
                format!("pin_window_ms defaults to {DEFAULT_PIN_WINDOW_MS}"),
            ],
        ),
        (
            POLYMARKET_ORACLE_DIVERGENCE_ID,
            "btc.polymarket.oracle_divergence@1.0.0.json",
            vec![format!(
                "diverged_threshold_bps defaults to {DEFAULT_DIVERGED_THRESHOLD_BPS}"
            )],
        ),
    ];
    for (id, file, expected_defaults) in cases {
        let bytes =
            std::fs::read(base.join(file)).unwrap_or_else(|error| panic!("read {file}: {error}"));
        let value: serde_json::Value =
            serde_json::from_slice(&bytes).unwrap_or_else(|error| panic!("parse {file}: {error}"));
        assert_eq!(
            value["signal_id"],
            serde_json::json!(id),
            "{file} signal_id"
        );
        assert_eq!(
            value["version"],
            serde_json::json!(CANDIDATE_SETTLEMENT_SIGNAL_VERSION),
            "{file} version"
        );
        let description = value["description"]
            .as_str()
            .unwrap_or_else(|| panic!("{file} description is a string"));
        for expected in &expected_defaults {
            assert!(
                description.contains(expected.as_str()),
                "{file} description must state the default: `{expected}`"
            );
        }
    }
}

#[test]
fn candidate_settlement_enum_variant_strings_match_the_definitions() {
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../definitions/btc/signals");
    let file = "btc.polymarket.settlement_anchor_state@1.0.0.json";

    let sides: BTreeSet<String> = [SettledSide::Up, SettledSide::Down, SettledSide::Void]
        .into_iter()
        .map(|value| value.as_str().to_owned())
        .collect();
    assert_eq!(
        definition_field_variants(&base, file, "side_if_settled_now"),
        sides,
        "side_if_settled_now variants"
    );

    let comparators: BTreeSet<String> = [Comparator::GreaterThanOrEqual]
        .into_iter()
        .map(|value| value.as_str().to_owned())
        .collect();
    assert_eq!(
        definition_field_variants(&base, file, "comparator"),
        comparators,
        "comparator variants"
    );
}

// --- determinism per family ------------------------------------------------

#[test]
fn settlement_replays_identically() {
    let run = || {
        let mut candidate = SettlementAnchorStateCandidate::with_defaults();
        vec![
            candidate
                .observe_contract(
                    ns(0),
                    Decimal::from(60000),
                    Comparator::GreaterThanOrEqual,
                    SettledSide::Up,
                    ns(30),
                )
                .unwrap(),
            candidate
                .observe_anchor(ns(0), Decimal::from(60030))
                .unwrap(),
        ]
    };
    assert_eq!(run(), run());
}

#[test]
fn oracle_replays_identically() {
    let run = || {
        let mut candidate = OracleDivergenceCandidate::with_default_threshold();
        vec![
            candidate.observe_anchor(Decimal::from(60030)),
            candidate.observe_spot_reference(Decimal::from(60000)),
        ]
    };
    assert_eq!(run(), run());
}

// --- 3. Kalshi twins (B3-B) --------------------------------------------------
//
// The Kalshi settlement identities are formula twins of the Polymarket originals
// (§5.4), reusing `SettlementAnchorStateCandidate` / `OracleDivergenceCandidate`
// verbatim. The only difference is the anchor binding (CF `avg_60s_value` rather
// than the RTDS TWAP), which the processor supplies. These assert the published
// contracts are byte-for-byte twins in field shape, so the same computation feeds
// both.

/// The `fields`, `event_time_field` and enum variants of two signal definitions,
/// everything a consumer keys on, ignoring the id/name/description prose.
fn definition_shape(base: &Path, file: &str) -> serde_json::Value {
    let bytes =
        std::fs::read(base.join(file)).unwrap_or_else(|error| panic!("read {file}: {error}"));
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).unwrap_or_else(|error| panic!("parse {file}: {error}"));
    serde_json::json!({
        "fields": value["fields"],
        "event_time_field": value["event_time_field"],
    })
}

/// The definition object for one internal settlement-window field (F1/E37): the
/// three fields the Kalshi twins add on top of the shared Polymarket shape.
fn settlement_window_field(name: &str, field_type: &str) -> serde_json::Value {
    serde_json::json!({ "name": name, "type": field_type, "location": "internal" })
}

/// The Polymarket twin's shape with the three Kalshi settlement-window fields
/// appended, in the order the Kalshi definition declares them. The Polymarket-only
/// `opening_strike_source` field (E11: the RTDS-TWAP opening-strike fallback's
/// provenance) is dropped first — Kalshi binds its reference value directly, so it
/// has no opening-TWAP fallback and does not declare that field.
fn polymarket_shape_plus_settlement_window(base: &Path, file: &str) -> serde_json::Value {
    let mut shape = definition_shape(base, file);
    let fields = shape["fields"]
        .as_array_mut()
        .expect("the twin shape has a fields array");
    fields.retain(|field| field["name"] != "opening_strike_source");
    fields.push(settlement_window_field("quarter_close_time", "timestamp"));
    fields.push(settlement_window_field("window_as_of", "timestamp"));
    fields.push(settlement_window_field(
        "settlement_window_complete",
        "bool",
    ));
    shape
}

#[test]
fn kalshi_settlement_anchor_matches_polymarket_twin_vectors() {
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../definitions/btc/signals");
    // The Kalshi twin shares the Polymarket original's formula field shape and
    // adds the three settlement-window fields KXBTC15M locks on (F1/E37): the
    // derived close, the running as-of and the completeness flag. Polymarket
    // resolves on the Chainlink TWAP and declares none of them.
    assert_eq!(
        definition_shape(&base, "btc.kalshi.settlement_anchor_state@1.0.0.json"),
        polymarket_shape_plus_settlement_window(
            &base,
            "btc.polymarket.settlement_anchor_state@1.0.0.json"
        ),
        "the Kalshi settlement anchor is the Polymarket field-shape twin plus the settlement-window fields"
    );
    assert_eq!(
        KALSHI_SETTLEMENT_ANCHOR_STATE_ID,
        "btc.kalshi.settlement_anchor_state"
    );

    // The shared candidate produces identical state for identical inputs; the
    // venue only changes where the anchor comes from, never the arithmetic.
    let mut candidate = SettlementAnchorStateCandidate::with_defaults();
    candidate
        .observe_contract(
            ns(0),
            Decimal::from(60_000),
            Comparator::GreaterThanOrEqual,
            SettledSide::Up,
            ns(30),
        )
        .unwrap();
    let AnchorOutcome::Ready(state) = candidate
        .observe_anchor(ns(0), Decimal::from(60_060))
        .unwrap()
    else {
        panic!("a bound contract with an anchor is ready");
    };
    // (60060 - 60000) / 60000 * 10000 = 10 bps.
    assert_eq!(state.distance_bps, Decimal::from(10));
    assert_eq!(state.side_if_settled_now, SettledSide::Up);
}

#[test]
fn kalshi_oracle_divergence_matches_polymarket_twin_vectors() {
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../definitions/btc/signals");
    // Same twin-plus-settlement-window relationship as the anchor (F1/E37).
    assert_eq!(
        definition_shape(&base, "btc.kalshi.oracle_divergence@1.0.0.json"),
        polymarket_shape_plus_settlement_window(
            &base,
            "btc.polymarket.oracle_divergence@1.0.0.json"
        ),
        "the Kalshi oracle divergence is the Polymarket field-shape twin plus the settlement-window fields"
    );
    assert_eq!(KALSHI_ORACLE_DIVERGENCE_ID, "btc.kalshi.oracle_divergence");

    let mut candidate = OracleDivergenceCandidate::with_default_threshold();
    candidate.observe_anchor(Decimal::from(60_060));
    let DivergenceOutcome::Ready(context) = candidate.observe_spot_reference(Decimal::from(60_000))
    else {
        panic!("an anchor and a spot reference are ready");
    };
    // (60060 - 60000) / 60000 * 10000 = 10 bps; |10| is not < 10 -> diverged.
    assert_eq!(context.divergence_bps, Decimal::from(10));
    assert_eq!(context.state, DivergenceState::Diverged);
}
