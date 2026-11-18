//! `btc.pyth.oracle_reference` — the value-carrying Pyth Hermes oracle reference
//! (S-RULINGS E48/E56, C1 phase 1). These tests drive [`PythOracleProcessor`]
//! directly: a byte-identical replay of the §20 observed Hermes fixture, the
//! embedded-contract match, and the readiness / staleness behaviour.
//!
//! Fixture provenance (STANDING-PRACTICES §20 — contiguous, unedited slice):
//! `crates/btc-feeds/tests/fixtures/observed/pyth_hermes_stream.jsonl` is
//! byte-identical to `head -200` (ordinals 1–200) of the real authenticated
//! capture `pyth-hermes-stream-r1-20260904T012850Z.jsonl` (Tokyo box,
//! 2026-09-04 01:28:50Z; 3 660 stream frames total). The excerpt carries one
//! `connected`, one `outbound` subscription pin, and 198 `inbound` SSE price
//! frames.

use std::collections::BTreeMap;

use btc_feeds::{CaptureStream, RawRecord, ReplaySession};
use btc_signals::lane::{LaneClock, LaneProcessor};
use btc_signals::oracle_signals::{PYTH_SOURCE_ID, PYTH_STREAM_LABEL, PythOracleProcessor};
use serde_json::{Value, json};
use signals_core::{QualityState, SignalDefinition, SignalUpdate, SourceEvent};

const FIXTURE: &str =
    include_str!("../../btc-feeds/tests/fixtures/observed/pyth_hermes_stream.jsonl");
const PYTH_SIGNAL_DOCUMENT: &str =
    include_str!("../../../definitions/btc/signals/btc.pyth.oracle_reference@1.0.0.json");

fn clock(now_ns: u64, produced_at: &str) -> LaneClock {
    LaneClock {
        now_ns,
        produced_at: produced_at.to_owned(),
    }
}

/// Replay the observed Hermes fixture through the real adapter path, exactly as
/// the lane runtime would, and return the normalized `SourceEvent`s.
fn fixture_events() -> Vec<SourceEvent> {
    let mut session = ReplaySession::new(CaptureStream::PythHermesStream, "fixture-epoch", None)
        .expect("replay session builds");
    let mut events = Vec::new();
    for (index, line) in FIXTURE.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let record: RawRecord = serde_json::from_str(line)
            .unwrap_or_else(|error| panic!("fixture record {index} parses: {error}"));
        let outcome = session
            .ingest(&record)
            .unwrap_or_else(|error| panic!("fixture record {index} replays: {error}"));
        events.extend(outcome.events);
    }
    events
}

/// A synthetic Hermes price event, for the readiness/staleness unit tests.
fn price_event(
    price: &str,
    conf: &str,
    ema_price: &str,
    ema_conf: &str,
    update_kind: &str,
    event_time: &str,
) -> SourceEvent {
    SourceEvent {
        schema_version: 1,
        source_id: PYTH_SOURCE_ID.to_owned(),
        source_version: "1.0.0".to_owned(),
        publisher_epoch: "pyth-test".to_owned(),
        sequence: 0,
        event_time: event_time.to_owned(),
        received_at: event_time.to_owned(),
        values: BTreeMap::from([
            ("instrument".to_owned(), json!("BTC-USD")),
            ("update_kind".to_owned(), json!(update_kind)),
            ("price".to_owned(), json!(price)),
            ("conf".to_owned(), json!(conf)),
            ("ema_price".to_owned(), json!(ema_price)),
            ("ema_conf".to_owned(), json!(ema_conf)),
            ("expo".to_owned(), json!(-8)),
        ]),
    }
}

fn field<'a>(update: &'a SignalUpdate, name: &str) -> &'a Value {
    update
        .values
        .get(name)
        .unwrap_or_else(|| panic!("update carries the {name} field"))
}

// --- Embedded contract ------------------------------------------------------

/// The embedded document is the on-disk 1.0.0 contract, it parses, and it
/// declares exactly the shared oracle_reference core plus the Pyth-specific
/// additive fields. Construction of the processor also parses (and validates) it.
#[test]
fn embedded_contract_matches_the_definition_file() {
    let definition: SignalDefinition =
        serde_json::from_str(PYTH_SIGNAL_DOCUMENT).expect("the embedded document parses");
    assert_eq!(definition.signal_id, "btc.pyth.oracle_reference");
    assert_eq!(definition.version, "1.0.0");
    assert_eq!(definition.event_time_field, "observation_time");
    assert_eq!(definition.inputs.len(), 1);
    assert_eq!(definition.inputs[0].id, "pyth.hermes.btc-usd.price");

    let fields: std::collections::BTreeSet<&str> = definition
        .fields
        .iter()
        .map(|field| field.name.as_str())
        .collect();
    let expected: std::collections::BTreeSet<&str> = [
        "value",
        "confidence",
        "ema_price",
        "ema_conf",
        "update_kind",
        "publish_time",
        "staleness_ms",
        "update_interval_ms",
        "freshness",
        "staleness_threshold_ms",
        "observation_time",
    ]
    .into_iter()
    .collect();
    assert_eq!(fields, expected, "the 1.1.0 correction's field set");

    // The processor construction parses the same document without panicking.
    let _ = PythOracleProcessor::new("pyth-test");
}

// --- Byte-identical replay --------------------------------------------------

/// Feed each observed frame in its own decision (as the runtime would per stream
/// read), collecting every ready decision, and prove the run reproduces
/// identically. The first ready decision pins the decoded first aggregate.
#[test]
fn replays_the_hermes_fixture_byte_identically() {
    let events = fixture_events();
    assert!(
        events.len() >= 190,
        "the head-200 excerpt decodes its ~198 inbound price frames (got {})",
        events.len()
    );

    let run = |events: &[SourceEvent]| -> Vec<SignalUpdate> {
        let mut processor = PythOracleProcessor::new("pyth-replay");
        let mut updates = Vec::new();
        for (index, event) in events.iter().enumerate() {
            let now_ns = (index as u64 + 1) * 1_000_000_000;
            // observation_time == the aggregate publish time: staleness is zero,
            // so freshness is `fresh` and the reading is deterministic.
            let clock = clock(now_ns, &event.event_time);
            updates.extend(
                processor
                    .on_events(PYTH_STREAM_LABEL, std::slice::from_ref(event), &clock)
                    .expect("event folds"),
            );
        }
        updates
    };

    let first = run(&events);
    let second = run(&events);
    assert_eq!(
        serde_json::to_string(&first).unwrap(),
        serde_json::to_string(&second).unwrap(),
        "the replay is byte-identical across runs"
    );
    assert!(!first.is_empty(), "the replay publishes ready decisions");

    // Every published update is the ready reference, numbered contiguously from 0.
    for (expected_sequence, update) in first.iter().enumerate() {
        assert_eq!(update.signal_id, "btc.pyth.oracle_reference");
        assert_eq!(update.signal_version, "1.0.0");
        assert_eq!(update.quality.state, QualityState::Ready);
        assert_eq!(update.sequence, expected_sequence as u64);
    }

    // The first ready decision is the first decoded aggregate (ordinal 3 of the
    // capture): price 80884.59499997, conf 6.07231946, ema 81107.16430000.
    let first_ready = &first[0];
    assert_eq!(field(first_ready, "value"), &json!("80884.59499997"));
    assert_eq!(field(first_ready, "confidence"), &json!("6.07231946"));
    assert_eq!(field(first_ready, "ema_price"), &json!("81107.16430000"));
    assert_eq!(field(first_ready, "ema_conf"), &json!("8.14001912"));
    assert_eq!(field(first_ready, "update_kind"), &json!("stream"));
    assert_eq!(field(first_ready, "freshness"), &json!("fresh"));
    assert_eq!(field(first_ready, "staleness_ms"), &json!(0));
    assert_eq!(field(first_ready, "update_interval_ms"), &json!(0));
    assert_eq!(
        field(first_ready, "publish_time"),
        &json!(events[0].event_time)
    );
}

// --- Readiness / staleness --------------------------------------------------

/// A fresh processor warms up: an interval tick before any aggregate publishes a
/// non-ready `pyth_warming_up`.
#[test]
fn warms_up_until_the_first_aggregate() {
    let mut processor = PythOracleProcessor::new("pyth-test");
    let updates = processor
        .on_interval(&clock(1_000_000_000, "2026-09-04T01:28:51.000000000Z"))
        .expect("interval evaluates");
    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0].quality.state, QualityState::WarmingUp);
    assert_eq!(
        updates[0].quality.reasons,
        vec!["pyth_warming_up".to_owned()]
    );
    assert!(
        updates[0].values.is_empty(),
        "a warming update carries no values"
    );
}

/// After the first aggregate the reference publishes ready, mirroring the
/// Chainlink core's shared fields.
#[test]
fn publishes_ready_after_the_first_aggregate() {
    let mut processor = PythOracleProcessor::new("pyth-test");
    let event = price_event(
        "80884.59499997",
        "6.07231946",
        "81107.16430000",
        "8.14001912",
        "stream",
        "2026-09-04T01:28:51.000000000Z",
    );
    let updates = processor
        .on_events(
            PYTH_STREAM_LABEL,
            std::slice::from_ref(&event),
            &clock(2_000_000_000, "2026-09-04T01:28:51.000000000Z"),
        )
        .expect("event folds");
    assert_eq!(updates.len(), 1);
    let update = &updates[0];
    assert_eq!(update.quality.state, QualityState::Ready);
    assert_eq!(field(update, "value"), &json!("80884.59499997"));
    assert_eq!(field(update, "confidence"), &json!("6.07231946"));
    assert_eq!(field(update, "freshness"), &json!("fresh"));
    assert_eq!(field(update, "staleness_threshold_ms"), &json!(5000));
}

/// An aggregate older than the staleness threshold still publishes ready — a
/// stale freshness is a published measurement, not a quality state (E56/§5.1).
#[test]
fn a_stale_aggregate_still_publishes_ready() {
    let mut processor = PythOracleProcessor::new("pyth-test");
    let event = price_event(
        "80884.59499997",
        "6.07231946",
        "81107.16430000",
        "8.14001912",
        "stream",
        "2026-09-04T01:28:51.000000000Z",
    );
    processor
        .on_events(
            PYTH_STREAM_LABEL,
            std::slice::from_ref(&event),
            &clock(2_000_000_000, "2026-09-04T01:28:51.000000000Z"),
        )
        .expect("event folds");
    // Ten seconds later, no new aggregate: staleness 10 000 ms > 5 000 threshold.
    let updates = processor
        .on_interval(&clock(12_000_000_000, "2026-09-04T01:29:01.000000000Z"))
        .expect("interval evaluates");
    assert_eq!(updates.len(), 1);
    let update = &updates[0];
    assert_eq!(
        update.quality.state,
        QualityState::Ready,
        "stale is still ready"
    );
    assert_eq!(field(update, "freshness"), &json!("stale"));
    assert_eq!(field(update, "staleness_ms"), &json!(10_000));
}

/// The update interval chains between accepted aggregates by their publish times.
#[test]
fn update_interval_chains_across_aggregates() {
    let mut processor = PythOracleProcessor::new("pyth-test");
    let first = price_event(
        "80000.00000000",
        "5.00000000",
        "80010.00000000",
        "6.00000000",
        "stream",
        "2026-09-04T01:28:51.000000000Z",
    );
    processor
        .on_events(
            PYTH_STREAM_LABEL,
            std::slice::from_ref(&first),
            &clock(1_000_000_000, "2026-09-04T01:28:51.000000000Z"),
        )
        .expect("first folds");
    let second = price_event(
        "80100.00000000",
        "5.00000000",
        "80020.00000000",
        "6.00000000",
        "stream",
        "2026-09-04T01:28:52.500000000Z",
    );
    let updates = processor
        .on_events(
            PYTH_STREAM_LABEL,
            std::slice::from_ref(&second),
            &clock(2_000_000_000, "2026-09-04T01:28:52.500000000Z"),
        )
        .expect("second folds");
    assert_eq!(updates.len(), 1);
    // 1 500 ms between the two publish times.
    assert_eq!(field(&updates[0], "update_interval_ms"), &json!(1500));
}

/// A lost stream makes the reference unavailable (`pyth_disconnected`), and it is
/// Degraded rather than WarmingUp once it has been ready.
#[test]
fn a_disconnect_makes_the_reference_unavailable() {
    let mut processor = PythOracleProcessor::new("pyth-test");
    let event = price_event(
        "80884.59499997",
        "6.07231946",
        "81107.16430000",
        "8.14001912",
        "stream",
        "2026-09-04T01:28:51.000000000Z",
    );
    processor
        .on_events(
            PYTH_STREAM_LABEL,
            std::slice::from_ref(&event),
            &clock(1_000_000_000, "2026-09-04T01:28:51.000000000Z"),
        )
        .expect("event folds");
    let updates = processor
        .on_connectivity(
            PYTH_STREAM_LABEL,
            false,
            &clock(2_000_000_000, "2026-09-04T01:28:52.000000000Z"),
        )
        .expect("disconnect evaluates");
    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0].quality.state, QualityState::Degraded);
    assert_eq!(
        updates[0].quality.reasons,
        vec!["pyth_disconnected".to_owned()]
    );
}
