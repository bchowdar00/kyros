use std::collections::BTreeMap;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use btc_signals::{
    COINBASE_TICKER_SOURCE_ID, COINBASE_TICKER_SOURCE_VERSION, KRAKEN_TICKER_SOURCE_ID,
    KRAKEN_TICKER_SOURCE_VERSION, SPOT_REFERENCE_ID, SPOT_REFERENCE_VERSION, SpotReferenceError,
    SpotReferenceProcessor, UnavailableReason, Venue,
};
use serde_json::{Value, json};
use signals_core::{
    FieldLocation, InputKind, LifecycleStatus, QualityState, SignalDefinition, SignalUpdate,
    SourceEvent, Validate,
};

mod support;

use crate::support::wall_start;

const T0: &str = "2026-08-13T12:00:00.000Z";
const T0_RECEIVED: &str = "2026-08-13T12:00:00.010Z";
const T1: &str = "2026-08-13T12:00:00.015Z";
const T1_RECEIVED: &str = "2026-08-13T12:00:00.025Z";
const T0_RECEIPT_NS: u64 = 10_000_000;
const T0_NOW_NS: u64 = 20_000_000;
const T1_RECEIPT_NS: u64 = 25_000_000;
const T1_NOW_NS: u64 = 30_000_000;

#[test]
fn embedded_definition_pins_inputs_and_methodology_contract() {
    let processor = processor("definition-test");
    let definition = processor.definition();
    definition.validate().expect("definition is valid");
    assert_eq!(definition.signal_id, SPOT_REFERENCE_ID);
    assert_eq!(definition.version, SPOT_REFERENCE_VERSION);
    assert_eq!(definition.status, LifecycleStatus::Approved);
    assert_eq!(definition.approximate_interval_ms, 1_000);
    assert_eq!(definition.staleness_bound_ms, 10_000);
    assert_eq!(definition.inputs.len(), 2);
    assert_eq!(definition.inputs[0].kind, InputKind::Source);
    assert_eq!(definition.inputs[0].id, COINBASE_TICKER_SOURCE_ID);
    assert_eq!(definition.inputs[0].version, COINBASE_TICKER_SOURCE_VERSION);
    assert_eq!(definition.inputs[1].kind, InputKind::Source);
    assert_eq!(definition.inputs[1].id, KRAKEN_TICKER_SOURCE_ID);
    assert_eq!(definition.inputs[1].version, KRAKEN_TICKER_SOURCE_VERSION);
    assert!(definition.description.contains("10 basis points"));
    assert!(definition.description.contains("round-half-to-even"));
    let reference_time = definition
        .fields
        .iter()
        .find(|field| field.name == "reference_time")
        .expect("reference_time field is declared");
    assert_eq!(reference_time.location, FieldLocation::External);
}

#[test]
fn warms_up_then_emits_exact_ready_reference() {
    let mut processor = processor("run-1");
    let first = processor
        .ingest(
            &coinbase_event(0, "60000", "60002", T0, T0_RECEIVED),
            T0_RECEIPT_NS,
            T0_NOW_NS,
        )
        .expect("Coinbase input is accepted")
        .expect("first input emits warm-up");
    assert_eq!(first.sequence, 0);
    assert_eq!(first.quality.state, QualityState::WarmingUp);
    assert_eq!(first.quality.reasons, ["kraken_warming_up"]);
    assert_eq!(first.values.get("coinbase_mid"), Some(&json!("60001")));
    assert_eq!(first.values.get("healthy_venue_count"), Some(&json!(1)));
    assert_eq!(first.values.get("max_input_age_ms"), Some(&json!(10)));
    assert!(!first.values.contains_key("reference_price"));

    let ready = processor
        .ingest(
            &kraken_event(0, "60001", "60003", T1, T1_RECEIVED),
            T1_RECEIPT_NS,
            T1_NOW_NS,
        )
        .expect("Kraken input is accepted")
        .expect("second venue emits ready output");
    assert_ready(&ready, processor.definition());
    assert_eq!(
        ready.sequence, 0,
        "the warm-up evaluation must not consume a decision sequence"
    );
    assert_eq!(ready.event_time, T1);
    assert_eq!(ready.produced_at, "2026-08-13T12:00:00.030000000Z");
    assert_eq!(ready.values.get("coinbase_mid"), Some(&json!("60001")));
    assert_eq!(ready.values.get("kraken_mid"), Some(&json!("60002")));
    assert_eq!(ready.values.get("reference_price"), Some(&json!("60001.5")));
    assert_eq!(
        ready.values.get("dispersion_bps"),
        Some(&json!("0.1666625"))
    );
    assert_eq!(ready.values.get("healthy_venue_count"), Some(&json!(2)));
    assert_eq!(ready.values.get("max_input_age_ms"), Some(&json!(20)));
    assert_eq!(ready.values.get("reference_time"), Some(&json!(T1)));
}

#[test]
fn unchanged_quotes_and_nonincreasing_sequences_do_not_emit() {
    let mut processor = ready_processor();
    let unchanged = coinbase_event(
        1,
        "60000",
        "60002",
        "2026-08-13T12:00:00.040Z",
        "2026-08-13T12:00:00.050Z",
    );
    assert!(
        processor
            .ingest(&unchanged, 40_000_000, 60_000_000)
            .expect("unchanged quote is accepted")
            .is_none()
    );
    assert!(
        processor
            .ingest(&unchanged, 40_000_000, 70_000_000)
            .expect("duplicate is ignored")
            .is_none()
    );

    let older = coinbase_event(
        0,
        "60000",
        "60004",
        "2026-08-13T12:00:00.050Z",
        "2026-08-13T12:00:00.060Z",
    );
    assert!(
        processor
            .ingest(&older, 50_000_000, 70_000_000)
            .expect("out-of-order input is ignored")
            .is_none()
    );
}

#[test]
fn monotonic_staleness_boundary_is_inclusive_and_recovers() {
    let mut processor = ready_processor();
    let at_boundary = processor
        .advance_time(T0_RECEIPT_NS + 10_000_000_000)
        .expect("boundary clock is accepted")
        .expect("health publication emits");
    assert_eq!(at_boundary.quality.state, QualityState::Ready);
    assert_eq!(
        at_boundary.values.get("max_input_age_ms"),
        Some(&json!(10_000))
    );

    let beyond = processor
        .advance_time(T0_RECEIPT_NS + 10_000_000_001)
        .expect("post-boundary clock is accepted")
        .expect("staleness transition emits");
    assert_eq!(beyond.quality.state, QualityState::Degraded);
    assert_eq!(beyond.quality.reasons, ["coinbase_stale"]);
    assert!(!beyond.values.contains_key("reference_price"));

    let recovered = processor
        .ingest(
            &coinbase_event(
                1,
                "60002",
                "60004",
                "2026-08-13T12:00:10.020Z",
                "2026-08-13T12:00:10.025Z",
            ),
            10_020_000_000,
            10_020_000_000,
        )
        .expect("fresh quote is accepted")
        .expect("fresh quote restores readiness");
    assert_eq!(recovered.quality.state, QualityState::Ready);
}

#[test]
fn health_tick_degrades_without_a_market_event() {
    let mut processor = ready_processor();
    let degraded = processor
        .advance_time(10_025_000_001)
        .expect("clock advance succeeds")
        .expect("staleness transition emits");
    assert_eq!(degraded.quality.state, QualityState::Degraded);
    assert_eq!(degraded.quality.reasons, ["coinbase_stale", "kraken_stale"]);
    assert_eq!(degraded.event_time, T1);
    assert_eq!(degraded.values.get("healthy_venue_count"), Some(&json!(0)));
    assert!(!degraded.values.contains_key("reference_price"));
    let heartbeat = processor
        .advance_time(11_025_000_001)
        .expect("unchanged degraded clock succeeds")
        .expect("health clock publishes current state");
    assert_eq!(
        heartbeat.sequence, degraded.sequence,
        "non-ready restatements must not consume decision sequences"
    );
}

#[test]
fn provider_wall_skew_does_not_control_freshness() {
    let mut processor = processor("wall-skew");
    let far_future = coinbase_event(
        0,
        "60000",
        "60002",
        "2099-01-01T00:00:00Z",
        "2099-01-01T00:00:01Z",
    );
    let far_past = kraken_event(
        0,
        "60001",
        "60003",
        "2001-01-01T00:00:00Z",
        "2001-01-01T00:00:01Z",
    );
    processor
        .ingest(&far_future, 1, 2)
        .expect("future provider wall time is metadata only");
    let ready = processor
        .ingest(&far_past, 3, 4)
        .expect("past provider wall time is metadata only")
        .expect("fresh monotonic receipts are ready");
    assert_eq!(ready.quality.state, QualityState::Ready);
    assert_eq!(ready.event_time, "2099-01-01T00:00:00Z");
}

#[test]
fn reference_uses_exact_midpoints_before_output_rounding() {
    let mut processor = processor("precision");
    let warming = processor
        .ingest(
            &coinbase_event(0, "1.000000006", "1.000000006", T0, T0_RECEIVED),
            T0_RECEIPT_NS,
            T0_NOW_NS,
        )
        .expect("Coinbase input is accepted")
        .expect("first input emits warming up");
    assert_eq!(
        warming.values.get("coinbase_mid"),
        Some(&json!("1.00000001"))
    );

    let ready = processor
        .ingest(
            &kraken_event(0, "1.000000016", "1.000000016", T1, T1_RECEIVED),
            T1_RECEIPT_NS,
            T1_NOW_NS,
        )
        .expect("Kraken input is accepted")
        .expect("second input emits ready");
    assert_eq!(ready.values.get("kraken_mid"), Some(&json!("1.00000002")));
    assert_eq!(
        ready.values.get("reference_price"),
        Some(&json!("1.00000001"))
    );
}

#[test]
fn crossed_and_nonpositive_quotes_are_degraded_not_reused() {
    for (bid, ask) in [("60005", "60004"), ("0", "60004"), ("-1", "60004")] {
        let mut processor = ready_processor();
        let degraded = processor
            .ingest(
                &kraken_event(
                    1,
                    bid,
                    ask,
                    "2026-08-13T12:00:00.040Z",
                    "2026-08-13T12:00:00.050Z",
                ),
                40_000_000,
                60_000_000,
            )
            .expect("contract-valid quote reaches semantic guard")
            .expect("invalid quote emits degradation");
        assert_eq!(degraded.quality.state, QualityState::Degraded);
        assert_eq!(degraded.quality.reasons, ["kraken_invalid_quote"]);
        assert!(!degraded.values.contains_key("kraken_mid"));
        assert!(!degraded.values.contains_key("reference_price"));
    }
}

#[test]
fn excessive_dispersion_exposes_constituents_but_omits_reference() {
    let mut processor = processor("dispersion");
    processor
        .ingest(
            &coinbase_event(0, "59999", "60001", T0, T0_RECEIVED),
            T0_RECEIPT_NS,
            T0_NOW_NS,
        )
        .expect("Coinbase accepted");
    let update = processor
        .ingest(
            &kraken_event(0, "60099", "60101", T1, T1_RECEIVED),
            T1_RECEIPT_NS,
            T1_NOW_NS,
        )
        .expect("Kraken accepted")
        .expect("dispersion emits degradation");
    assert_eq!(update.quality.state, QualityState::Degraded);
    assert_eq!(update.quality.reasons, ["cross_venue_dispersion_exceeded"]);
    assert_eq!(update.values.get("coinbase_mid"), Some(&json!("60000")));
    assert_eq!(update.values.get("kraken_mid"), Some(&json!("60100")));
    assert_eq!(update.values.get("healthy_venue_count"), Some(&json!(2)));
    assert!(update.values.contains_key("dispersion_bps"));
    assert!(!update.values.contains_key("reference_price"));
    update
        .validate_against(processor.definition())
        .expect("degraded subset remains contract valid");
}

#[test]
fn dispersion_guard_uses_full_precision_before_publication_rounding() {
    let mut at_limit = processor("at-limit");
    at_limit
        .ingest(
            &coinbase_event(0, "0.9995", "0.9995", T0, T0_RECEIVED),
            T0_RECEIPT_NS,
            T0_NOW_NS,
        )
        .expect("first constituent accepted");
    let ready = at_limit
        .ingest(
            &kraken_event(0, "1.0005", "1.0005", T1, T1_RECEIVED),
            T1_RECEIPT_NS,
            T1_NOW_NS,
        )
        .expect("second constituent accepted")
        .expect("exact limit emits");
    assert_eq!(ready.quality.state, QualityState::Ready);
    assert_eq!(ready.values.get("dispersion_bps"), Some(&json!("10")));

    let mut above = processor("above-limit");
    above
        .ingest(
            &coinbase_event(0, "0.9995", "0.9995", T0, T0_RECEIVED),
            T0_RECEIPT_NS,
            T0_NOW_NS,
        )
        .expect("first constituent accepted");
    let degraded = above
        .ingest(
            &kraken_event(
                0,
                "1.000500000000000000000001",
                "1.000500000000000000000001",
                T1,
                T1_RECEIVED,
            ),
            T1_RECEIPT_NS,
            T1_NOW_NS,
        )
        .expect("second constituent accepted")
        .expect("above-limit value emits");
    assert_eq!(degraded.quality.state, QualityState::Degraded);
    assert_eq!(
        degraded.quality.reasons,
        ["cross_venue_dispersion_exceeded"]
    );
    assert_eq!(degraded.values.get("dispersion_bps"), Some(&json!("10")));
    assert!(!degraded.values.contains_key("reference_price"));
}

#[test]
fn disconnect_and_recovery_invalidate_one_venue() {
    for (reason, expected) in [
        (UnavailableReason::Disconnected, "kraken_disconnected"),
        (UnavailableReason::Recovery, "kraken_recovery_required"),
        (
            UnavailableReason::SubscriptionRejected,
            "kraken_subscription_rejected",
        ),
    ] {
        let mut processor = ready_processor();
        let update = processor
            .mark_unavailable(Venue::Kraken, reason, 50_000_000)
            .expect("availability transition is accepted")
            .expect("availability transition emits");
        assert_eq!(update.quality.state, QualityState::Degraded);
        assert_eq!(update.quality.reasons, [expected]);
        assert!(!update.values.contains_key("reference_price"));
    }
}

#[test]
fn source_epoch_change_allows_sequence_restart() {
    let mut processor = ready_processor();
    let mut restarted = coinbase_event(
        0,
        "60002",
        "60004",
        "2026-08-13T12:00:00.040Z",
        "2026-08-13T12:00:00.050Z",
    );
    restarted.publisher_epoch = "feed-restart".to_owned();
    let update = processor
        .ingest(&restarted, 40_000_000, 60_000_000)
        .expect("new source epoch is accepted")
        .expect("new quote emits");
    assert_eq!(update.quality.state, QualityState::Ready);
    assert_eq!(update.values.get("coinbase_mid"), Some(&json!("60003")));
}

#[test]
fn contract_and_logical_clock_invariants_fail_closed() {
    let mut processor = processor("guards");
    let mut wrong_version = coinbase_event(0, "60000", "60002", T0, T0_RECEIVED);
    wrong_version.source_version = "9.9.9".to_owned();
    assert!(matches!(
        processor.ingest(&wrong_version, T0_RECEIPT_NS, T0_NOW_NS),
        Err(SpotReferenceError::InputVersion { .. })
    ));

    let mut wrong_instrument = coinbase_event(0, "60000", "60002", T0, T0_RECEIVED);
    wrong_instrument
        .values
        .insert("instrument".to_owned(), json!("ETH-USD"));
    assert!(matches!(
        processor.ingest(&wrong_instrument, T0_RECEIPT_NS, T0_NOW_NS),
        Err(SpotReferenceError::Instrument { .. })
    ));

    let valid = coinbase_event(0, "60000", "60002", T0, T0_RECEIVED);
    assert!(matches!(
        processor.ingest(&valid, T0_NOW_NS + 1, T0_NOW_NS),
        Err(SpotReferenceError::ReceiptAfterAdmission { .. })
    ));
    assert!(
        processor
            .ingest(&valid, T0_RECEIPT_NS, T0_NOW_NS)
            .expect("failed receipt input did not consume ordering state")
            .is_some()
    );
    assert!(matches!(
        processor.advance_time(T0_NOW_NS - 1),
        Err(SpotReferenceError::ClockRegression { .. })
    ));
}

#[test]
fn venue_midpoint_overflow_is_contained_and_next_clean_quote_recovers() {
    let maximum = "79228162514264337593543950335";
    let adjacent = "79228162514264337593543950334";
    let mut processor = processor("venue-overflow");
    let degraded = processor
        .ingest(
            &coinbase_event(0, adjacent, maximum, T0, T0_RECEIVED),
            T0_RECEIPT_NS,
            T0_NOW_NS,
        )
        .expect("unrepresentable midpoint is contained")
        .expect("quality degradation emits");
    assert_eq!(degraded.quality.state, QualityState::Degraded);
    assert_eq!(
        degraded.quality.reasons,
        ["coinbase_midpoint_unrepresentable", "kraken_warming_up"]
    );
    assert!(!degraded.values.contains_key("coinbase_mid"));

    let recovered = processor
        .ingest(
            &coinbase_event(1, "60000", "60002", T0, T0_RECEIVED),
            T0_RECEIPT_NS + 1,
            T0_NOW_NS + 1,
        )
        .expect("next clean event is accepted")
        .expect("clean event clears the venue failure");
    assert_eq!(recovered.quality.state, QualityState::WarmingUp);
    assert_eq!(recovered.quality.reasons, ["kraken_warming_up"]);
    assert_eq!(recovered.values.get("coinbase_mid"), Some(&json!("60001")));
}

#[test]
fn reference_overflow_keeps_both_constituents_but_omits_reference() {
    let maximum = "79228162514264337593543950335";
    let adjacent = "79228162514264337593543950334";
    let mut processor = processor("reference-overflow");
    processor
        .ingest(
            &coinbase_event(0, maximum, maximum, T0, T0_RECEIVED),
            T0_RECEIPT_NS,
            T0_NOW_NS,
        )
        .expect("maximum constituent is representable");
    let degraded = processor
        .ingest(
            &kraken_event(0, adjacent, adjacent, T1, T1_RECEIVED),
            T1_RECEIPT_NS,
            T1_NOW_NS,
        )
        .expect("reference arithmetic failure is contained")
        .expect("quality degradation emits");
    assert_eq!(degraded.quality.state, QualityState::Degraded);
    assert_eq!(
        degraded.quality.reasons,
        ["reference_arithmetic_unrepresentable"]
    );
    assert_eq!(degraded.values.get("coinbase_mid"), Some(&json!(maximum)));
    assert_eq!(degraded.values.get("kraken_mid"), Some(&json!(adjacent)));
    assert!(!degraded.values.contains_key("reference_price"));
    assert!(!degraded.values.contains_key("dispersion_bps"));
}

#[test]
fn equal_decimal_maximum_is_ready_without_panic() {
    let maximum = "79228162514264337593543950335";
    let mut processor = processor("maximum");
    processor
        .ingest(
            &coinbase_event(0, maximum, maximum, T0, T0_RECEIVED),
            T0_RECEIPT_NS,
            T0_NOW_NS,
        )
        .expect("maximum Coinbase midpoint is representable");
    let ready = processor
        .ingest(
            &kraken_event(0, maximum, maximum, T1, T1_RECEIVED),
            T1_RECEIPT_NS,
            T1_NOW_NS,
        )
        .expect("maximum reference is representable")
        .expect("ready output emits");
    assert_eq!(ready.quality.state, QualityState::Ready);
    assert_eq!(ready.values.get("reference_price"), Some(&json!(maximum)));
}

#[test]
fn replayed_logical_inputs_are_deterministic() {
    let inputs = [
        (
            coinbase_event(0, "60000", "60002", T0, T0_RECEIVED),
            T0_RECEIPT_NS,
            T0_NOW_NS,
        ),
        (
            kraken_event(0, "60001", "60003", T1, T1_RECEIVED),
            T1_RECEIPT_NS,
            T1_NOW_NS,
        ),
        (
            coinbase_event(
                1,
                "60001",
                "60003",
                "2026-08-13T12:00:00.040Z",
                "2026-08-13T12:00:00.050Z",
            ),
            40_000_000,
            60_000_000,
        ),
    ];
    let first = process_all("replay", &inputs);
    let second = process_all("replay", &inputs);
    assert_eq!(first, second);

    let restarted = process_all("restart", &inputs);
    assert_eq!(restarted[0].sequence, 0);
    assert_eq!(restarted[0].publisher_epoch, "restart");
    assert_eq!(first[0].publisher_epoch, "replay");
}

#[test]
fn ready_output_conforms_to_generic_json_schema() {
    let mut processor = ready_processor();
    let update = processor
        .ingest(
            &coinbase_event(
                1,
                "60001",
                "60003",
                "2026-08-13T12:00:00.040Z",
                "2026-08-13T12:00:00.050Z",
            ),
            40_000_000,
            60_000_000,
        )
        .expect("input accepted")
        .expect("changed quote emits");
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let schema: Value = serde_json::from_reader(BufReader::new(
        File::open(root.join("schemas/signal-update.schema.json")).expect("schema opens"),
    ))
    .expect("schema parses");
    let validator = jsonschema::validator_for(&schema).expect("schema compiles");
    let instance = serde_json::to_value(update).expect("update serializes");
    validator
        .validate(&instance)
        .expect("output conforms to generic signal-update schema");
}

fn processor(epoch: &str) -> SpotReferenceProcessor {
    SpotReferenceProcessor::new(epoch, wall_start()).expect("processor constructs")
}

fn ready_processor() -> SpotReferenceProcessor {
    let mut processor = processor("ready-test");
    processor
        .ingest(
            &coinbase_event(0, "60000", "60002", T0, T0_RECEIVED),
            T0_RECEIPT_NS,
            T0_NOW_NS,
        )
        .expect("Coinbase input accepted");
    let ready = processor
        .ingest(
            &kraken_event(0, "60001", "60003", T1, T1_RECEIVED),
            T1_RECEIPT_NS,
            T1_NOW_NS,
        )
        .expect("Kraken input accepted")
        .expect("ready update emitted");
    assert_eq!(ready.quality.state, QualityState::Ready);
    processor
}

fn process_all(publisher_epoch: &str, inputs: &[(SourceEvent, u64, u64)]) -> Vec<SignalUpdate> {
    let mut processor = processor(publisher_epoch);
    inputs
        .iter()
        .filter_map(|(event, receipt, now)| {
            processor
                .ingest(event, *receipt, *now)
                .expect("replay input succeeds")
        })
        .collect()
}

fn assert_ready(update: &SignalUpdate, definition: &SignalDefinition) {
    assert_eq!(update.signal_id, SPOT_REFERENCE_ID);
    assert_eq!(update.signal_version, SPOT_REFERENCE_VERSION);
    assert_eq!(update.quality.state, QualityState::Ready);
    assert!(update.quality.reasons.is_empty());
    update
        .validate_against(definition)
        .expect("ready update is definition-valid");
}

fn coinbase_event(
    sequence: u64,
    bid: &str,
    ask: &str,
    event_time: &str,
    received_at: &str,
) -> SourceEvent {
    SourceEvent {
        schema_version: 1,
        source_id: COINBASE_TICKER_SOURCE_ID.to_owned(),
        source_version: COINBASE_TICKER_SOURCE_VERSION.to_owned(),
        publisher_epoch: "coinbase-feed".to_owned(),
        sequence,
        event_time: event_time.to_owned(),
        received_at: received_at.to_owned(),
        values: BTreeMap::from([
            ("instrument".to_owned(), json!("BTC-USD")),
            ("trade_id".to_owned(), json!(100 + sequence)),
            ("price".to_owned(), json!("60001")),
            ("last_size".to_owned(), json!("0.01")),
            ("best_bid".to_owned(), json!(bid)),
            ("best_bid_size".to_owned(), json!("1.25")),
            ("best_ask".to_owned(), json!(ask)),
            ("best_ask_size".to_owned(), json!("2.5")),
            ("open_24h".to_owned(), json!("59800")),
            ("volume_24h".to_owned(), json!("1250.5")),
            ("low_24h".to_owned(), json!("59000")),
            ("high_24h".to_owned(), json!("61000")),
            ("volume_30d".to_owned(), json!("30000")),
            ("event_time".to_owned(), json!(event_time)),
            ("source_sequence".to_owned(), json!(900 + sequence)),
        ]),
    }
}

fn kraken_event(
    sequence: u64,
    bid: &str,
    ask: &str,
    event_time: &str,
    received_at: &str,
) -> SourceEvent {
    SourceEvent {
        schema_version: 1,
        source_id: KRAKEN_TICKER_SOURCE_ID.to_owned(),
        source_version: KRAKEN_TICKER_SOURCE_VERSION.to_owned(),
        publisher_epoch: "kraken-feed".to_owned(),
        sequence,
        event_time: event_time.to_owned(),
        received_at: received_at.to_owned(),
        values: BTreeMap::from([
            ("instrument".to_owned(), json!("BTC/USD")),
            ("best_bid".to_owned(), json!(bid)),
            ("best_bid_size".to_owned(), json!("0.02")),
            ("best_ask".to_owned(), json!(ask)),
            ("best_ask_size".to_owned(), json!("0.17")),
            ("last_price".to_owned(), json!("60001")),
            ("volume_24h".to_owned(), json!("973.7")),
            ("vwap_24h".to_owned(), json!("60010")),
            ("low_24h".to_owned(), json!("59000")),
            ("high_24h".to_owned(), json!("61000")),
            ("change_24h".to_owned(), json!("-72.9")),
            ("change_percent_24h".to_owned(), json!("-0.11")),
            ("trade_count_24h".to_owned(), json!(46574)),
            ("update_kind".to_owned(), json!("update")),
            ("event_time".to_owned(), json!(event_time)),
        ]),
    }
}
