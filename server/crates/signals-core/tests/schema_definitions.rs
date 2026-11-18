use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    fs::File,
    io::BufReader,
    path::Path,
};

use serde_json::{Value, json};
use signals_core::{
    CONTRACT_SCHEMA_VERSION, ConflationPolicy, FeedHealthReport, FeedHealthStatus, FieldDefinition,
    FieldLocation, FieldType, INSTANCE_STATUS_REASONS, InputKind, InstanceStatusEntry,
    InstanceStatusState, LifecycleStatus, MarketTransitionPhase, MarketTransitionReason,
    PredictionMarketVenue, ProducerStatus, Quality, QualityState, SIGNAL_HEARTBEAT_INTERVAL_MS,
    SIGNAL_STREAM_SCHEMA_VERSION, SIGNAL_UPDATE_SCHEMA_VERSION, SignalDefinition, SignalInput,
    SignalStreamMessage, SignalUpdate, SourceDefinition, SourceEvent, StreamResetReason, Transport,
    Validate, ValuesSemantics,
};

fn read_json(path: &Path) -> Value {
    let file = File::open(path)
        .unwrap_or_else(|error| panic!("failed to open {}: {error}", path.display()));
    serde_json::from_reader(BufReader::new(file))
        .unwrap_or_else(|error| panic!("failed to parse {}: {error}", path.display()))
}

fn json_files(directory: &Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    let mut directories = vec![directory.to_path_buf()];

    while let Some(current) = directories.pop() {
        for entry in fs::read_dir(&current)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", current.display()))
        {
            let entry = entry
                .unwrap_or_else(|error| panic!("failed to inspect {}: {error}", current.display()));
            let path = entry.path();
            if path.is_dir() {
                directories.push(path);
            } else if path
                .extension()
                .is_some_and(|extension| extension == "json")
            {
                files.push(path);
            }
        }
    }

    files.sort();
    files
}

#[test]
fn schemas_compile_and_definitions_conform() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let schema_paths = [
        "schemas/source-definition.schema.json",
        "schemas/source-event.schema.json",
        "schemas/signal-definition.schema.json",
        "schemas/signal-update.schema.json",
        "schemas/instance-status.schema.json",
        "schemas/signal-stream-message.schema.json",
    ];

    for schema_path in schema_paths {
        let schema = read_json(&root.join(schema_path));
        jsonschema::validator_for(&schema)
            .unwrap_or_else(|error| panic!("invalid schema {schema_path}: {error}"));
    }

    let source_schema = read_json(&root.join("schemas/source-definition.schema.json"));
    let signal_schema = read_json(&root.join("schemas/signal-definition.schema.json"));
    let source_validator = jsonschema::validator_for(&source_schema)
        .unwrap_or_else(|error| panic!("invalid source-definition schema: {error}"));
    let signal_validator = jsonschema::validator_for(&signal_schema)
        .unwrap_or_else(|error| panic!("invalid signal-definition schema: {error}"));

    let definitions = json_files(&root.join("definitions"));
    for definition_path in definitions {
        let definition = read_json(&definition_path);
        let kind = definition_path
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str());
        let is_valid = match kind {
            Some("sources") => source_validator.is_valid(&definition),
            Some("signals") => signal_validator.is_valid(&definition),
            // Unadopted candidate drafts are complete V1 signal definitions and
            // must satisfy the same signal-definition conformance as adopted
            // ones, even though they are off the frozen surface (they are not
            // counted below, and `signals validate` never visits them).
            Some("candidates") => signal_validator.is_valid(&definition),
            _ => panic!(
                "definition must be inside a sources, signals, or candidates directory: {}",
                definition_path.display()
            ),
        };
        assert!(
            is_valid,
            "{} does not conform to its definition schema",
            definition_path.display()
        );
    }

    let source_definitions = json_files(&root.join("definitions/btc/sources"));
    // Fifty-two files, fifty-one defined stream sources: the odd one out,
    // `kalshi.cfbenchmarks.brti.history`, is the repair-row contract of the CF
    // Benchmarks REST passthrough. It is registered so the repair path has a
    // versioned field contract, and it emits no event, carries no capture
    // stream, and appears in no package closure or delivery catalogue (ruling
    // R2, OD-7).
    //
    // `kalshi.cfbenchmarks.brti.value-5hz` is back (ruling E25) after the
    // 65-minute R1 hour delivered 2,202 frames on it: the channel is
    // intermittently available, not absent, so it is an `approved` source
    // (built and ready to run) whose subscription tolerates a code-8 refusal;
    // its intermittent availability is stated in its description.
    //
    // `chainlink.ethereum.btc-usd.rounds` is an `approved` source (A5 phase 1);
    // its live `http_poll` driver is in `btc-feeds`, its lane is the generic
    // venue lane built in A6.
    //
    // Every file here carries `status: approved` — built, tested and ready to
    // run (ruling E45, superseding E10's single-`draft` model). Whether a
    // contract's wire shape has been observed in a capture or still rests on
    // documentation alone is stated per contract in its description, not in the
    // status field.
    //
    // The eight Binance contracts (group A3, plus the F2/E50 REST premium-index
    // poll) are three spot promotions of what `binance.rs` emits and five USD-M
    // perpetual sources — ruling E62 (2026-09-04) deleted the three
    // `market`-connection USD-M sources (`mark-price`, `agg-trades`,
    // `liquidations`) that no host we own can dial. The venue is refusal-gated at
    // runtime by `SIGNALS_ENABLED_VENUES` (ruling R7), so a published definition
    // is not a published feed.
    //
    // The nine Bybit contracts added the same day (group A4) — spot, linear
    // WebSocket and linear REST — are rights-gated behind the same
    // `SIGNALS_ENABLED_VENUES` allow-list (R7).
    //
    // `coinbase.exchange.usdt-usd.ticker` (ruling E47) is the second USDT/USD
    // reference alongside the adopted Kraken USDT/USD suite: the `ticker`
    // channel of the existing Coinbase socket, routed by product id. It is
    // `approved` and OBSERVED — corrected against the R1 coinbase-core capture
    // (2026-09-04, 2,612 ticker frames, zero adapter errors) before adoption
    // (its description says so).
    assert_eq!(
        source_definitions.len(),
        50,
        "the BTC source-definition set must contain exactly one V1 contract per source identity"
    );
    let signal_definitions = json_files(&root.join("definitions/btc/signals"));
    // B3 (2026-09-03) adds five signal identities: the Chainlink oracle reference
    // (built and published from the oracle lane, brief A), the CF oracle reference
    // and the two Kalshi settlement identities (published from the Kalshi
    // prediction lane, brief B), and the refuse-closed `btc.pyth.oracle_reference`
    // (its relay source is unobserved; ceded to A2, R3). Definitions live in the
    // public tree regardless of processor state (R7 refuse-closed precedent).
    // C3a (E51, 2026-09-04) adds the technical-analysis pack: `atr`, `rsi`,
    // `roc`, `ema_cross`, `macd`, `bollinger` and `zscore` on each of the five
    // spot scopes (external, coinbase, kraken, binance, bybit) and, under the
    // `perp_` prefix, on each of the two perpetual mark scopes — 49 identities.
    // The B2 base is 101 after ruling E62 (2026-09-04) deleted the two Binance
    // socket families `perp_cvd`/`liquidation_pressure` (8 B2, not 10), so
    // 101 + 49 = 150. C2 (E49, 2026-09-04) then adds 22 momentum/volatility
    // identities: the five spot scopes gain horizon momentum + volatility (10),
    // Kalshi and Polymarket gain probability momentum + volatility (4), Chainlink
    // gains price momentum + volatility (2), Pyth gains two refuse-closed price
    // identities (2, no processor — its Hermes lane input arrives in C4), and
    // Binance and Bybit gain reference-free perp momentum + volatility (4):
    // 150 + 22 = 172. C3b (ruling E51, 2026-09-04) then adds fifteen TA and
    // timing identities: `rsi`, `roc`, `zscore`, `tick_velocity` and
    // `expected_value` on each prediction venue (10), the Kalshi-only
    // `window_streak` (1), and `tick_velocity` on the four spot venues (4):
    // 172 + 15 = 187. C1 (wave3-c1) then adds the 6 multi/cross identities staged
    // ahead of their processors: 187 + 6 = 193.
    assert_eq!(
        signal_definitions.len(),
        193,
        "the BTC signal-definition set must contain exactly one V1 contract per signal identity"
    );
}

#[test]
fn rust_runtime_values_conform_to_runtime_schemas() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let source_definition_schema = read_json(&root.join("schemas/source-definition.schema.json"));
    let signal_definition_schema = read_json(&root.join("schemas/signal-definition.schema.json"));
    let source_schema = read_json(&root.join("schemas/source-event.schema.json"));
    let signal_schema = read_json(&root.join("schemas/signal-update.schema.json"));
    let instance_status_schema = read_json(&root.join("schemas/instance-status.schema.json"));
    let stream_schema = read_json(&root.join("schemas/signal-stream-message.schema.json"));
    let source_definition_validator = jsonschema::validator_for(&source_definition_schema)
        .expect("source-definition schema must compile");
    let signal_definition_validator = jsonschema::validator_for(&signal_definition_schema)
        .expect("signal-definition schema must compile");
    let source_validator =
        jsonschema::validator_for(&source_schema).expect("source-event schema must compile");
    let signal_validator =
        jsonschema::validator_for(&signal_schema).expect("signal-update schema must compile");
    let instance_status_validator = jsonschema::validator_for(&instance_status_schema)
        .expect("instance-status schema must compile");
    let stream_validator =
        jsonschema::validator_for(&stream_schema).expect("signal-stream schema must compile");

    let source_definition = source_definition();
    let source_event = source_event();
    source_definition
        .validate()
        .expect("source definition must be semantically valid");
    let source_definition_json =
        serde_json::to_value(&source_definition).expect("source definition serializes");
    assert!(
        source_definition_validator.is_valid(&source_definition_json),
        "Rust SourceDefinition must satisfy source-definition.schema.json"
    );
    let mut source_definition_unknown_field = source_definition_json.clone();
    source_definition_unknown_field
        .as_object_mut()
        .expect("source definition is an object")
        .insert("unexpected".to_owned(), json!(true));
    assert!(!source_definition_validator.is_valid(&source_definition_unknown_field));
    assert!(serde_json::from_value::<SourceDefinition>(source_definition_unknown_field).is_err());

    let mut source_definition_bad_status = source_definition_json.clone();
    source_definition_bad_status["status"] = json!("live");
    assert!(!source_definition_validator.is_valid(&source_definition_bad_status));
    assert!(serde_json::from_value::<SourceDefinition>(source_definition_bad_status).is_err());

    source_event
        .validate_against(&source_definition)
        .expect("source event must satisfy its definition");
    let source_json = serde_json::to_value(&source_event).expect("source event serializes");
    assert!(
        source_validator.is_valid(&source_json),
        "Rust SourceEvent must satisfy source-event.schema.json"
    );

    let signal_definition = signal_definition();
    let signal_update = signal_update();
    signal_definition
        .validate()
        .expect("signal definition must be semantically valid");
    let signal_definition_json =
        serde_json::to_value(&signal_definition).expect("signal definition serializes");
    assert!(
        signal_definition_validator.is_valid(&signal_definition_json),
        "Rust SignalDefinition must satisfy signal-definition.schema.json"
    );
    signal_update
        .validate_against(&signal_definition)
        .expect("signal update must satisfy its definition");
    let signal_json = serde_json::to_value(&signal_update).expect("signal update serializes");
    assert!(
        signal_validator.is_valid(&signal_json),
        "Rust SignalUpdate must satisfy signal-update.schema.json"
    );

    let stream_messages = stream_messages(signal_update.clone());
    for message in &stream_messages {
        message
            .validate()
            .expect("stream message must be semantically valid");
        let message_json = serde_json::to_value(message).expect("stream message serializes");
        assert!(
            stream_validator.is_valid(&message_json),
            "Rust SignalStreamMessage must satisfy signal-stream-message.schema.json: {message_json}"
        );
    }
    let instance_status_json =
        serde_json::to_value(&stream_messages[2]).expect("instance-status frame serializes");
    assert!(
        instance_status_validator.is_valid(&instance_status_json),
        "Rust instance-status frame must satisfy instance-status.schema.json"
    );
    let mut duplicate_reason = instance_status_json.clone();
    duplicate_reason["reasons"] = json!(["coinbase_stale", "coinbase_stale"]);
    assert!(!instance_status_validator.is_valid(&duplicate_reason));
    let mut unknown_reason = instance_status_json;
    unknown_reason["reasons"] = json!(["not_in_the_frozen_registry"]);
    assert!(!instance_status_validator.is_valid(&unknown_reason));

    let mut unsupported_reset_reason =
        serde_json::to_value(&stream_messages[0]).expect("reset serializes");
    unsupported_reset_reason["reason"] = json!("state_invalidated");
    assert!(!stream_validator.is_valid(&unsupported_reset_reason));
    assert!(serde_json::from_value::<SignalStreamMessage>(unsupported_reset_reason).is_err());

    let mut invalid_source = source_json;
    invalid_source
        .as_object_mut()
        .expect("source event is an object")
        .insert("unexpected".to_owned(), json!(true));
    assert!(!source_validator.is_valid(&invalid_source));
    assert!(serde_json::from_value::<SourceEvent>(invalid_source).is_err());

    let mut invalid_signal_definition = signal_definition_json;
    invalid_signal_definition
        .as_object_mut()
        .expect("signal definition is an object")
        .insert("unexpected".to_owned(), json!(true));
    assert!(!signal_definition_validator.is_valid(&invalid_signal_definition));
    assert!(serde_json::from_value::<SignalDefinition>(invalid_signal_definition).is_err());

    let mut invalid_signal = signal_json;
    invalid_signal["quality"]["reasons"] = json!(["not allowed while ready"]);
    assert!(!signal_validator.is_valid(&invalid_signal));
    let invalid_signal: SignalUpdate =
        serde_json::from_value(invalid_signal).expect("shape still deserializes");
    assert!(invalid_signal.validate_against(&signal_definition).is_err());
}

#[test]
fn rust_multi_market_scope_conforms_to_the_runtime_schemas() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let signal_validator =
        jsonschema::validator_for(&read_json(&root.join("schemas/signal-update.schema.json")))
            .expect("signal-update schema must compile");
    let instance_status_validator = jsonschema::validator_for(&read_json(
        &root.join("schemas/instance-status.schema.json"),
    ))
    .expect("instance-status schema must compile");
    let stream_validator = jsonschema::validator_for(&read_json(
        &root.join("schemas/signal-stream-message.schema.json"),
    ))
    .expect("signal-stream schema must compile");

    // The Rust `SignalScope::MultiMarket` wire shape against the published
    // multi_market schema branch — the only test that pins the two together, so a
    // renamed/added field or a differently spelled scope tag can no longer keep
    // the suite green while every multi_market frame fails a consumer's schema
    // validation. Legs ascending by (venue, market_id): kalshi before polymarket.
    let scope = signals_core::SignalScope::MultiMarket {
        legs: vec![
            signals_core::MultiMarketLeg {
                venue: "kalshi".to_owned(),
                market_id: "KXBTC15M-A".to_owned(),
                market_generation: 2,
            },
            signals_core::MultiMarketLeg {
                venue: "polymarket".to_owned(),
                market_id: "btc-updown-5m-1".to_owned(),
                market_generation: 4,
            },
        ],
        window: signals_core::MultiMarketWindow {
            open: "2026-09-03T12:00:00Z".to_owned(),
            close: "2026-09-03T12:15:00Z".to_owned(),
        },
    };

    let mut update = signal_update();
    update.signal_id = "btc.multi.prediction_dislocation".to_owned();
    update.scope = scope.clone();
    let update_json = serde_json::to_value(&update).expect("multi-market update serializes");
    assert!(
        signal_validator.is_valid(&update_json),
        "Rust MultiMarket SignalUpdate must satisfy signal-update.schema.json: {update_json}"
    );
    // The scope tag is spelled the same on both sides.
    assert_eq!(update_json["scope"]["type"], json!("multi_market"));

    // An instance_status frame carrying the same scope satisfies both the
    // standalone and the enveloped schema.
    let status = SignalStreamMessage::InstanceStatus {
        schema_version: SIGNAL_STREAM_SCHEMA_VERSION,
        publisher_epoch: "schema-test".to_owned(),
        sequence: 7,
        produced_at: "2026-09-03T12:01:00Z".to_owned(),
        signal_id: "btc.multi.prediction_dislocation".to_owned(),
        signal_version: "1.0.0".to_owned(),
        scope: scope.clone(),
        state: InstanceStatusState::Degraded,
        reasons: vec!["reference_constituents_unavailable".to_owned()],
        effective_time: "2026-09-03T12:01:00Z".to_owned(),
        last_decision_sequence: Some(6),
    };
    let status_json = serde_json::to_value(&status).expect("multi-market status serializes");
    assert!(
        instance_status_validator.is_valid(&status_json),
        "Rust MultiMarket instance-status must satisfy instance-status.schema.json: {status_json}"
    );
    assert!(
        stream_validator.is_valid(&status_json),
        "Rust MultiMarket instance-status must satisfy signal-stream-message.schema.json: {status_json}"
    );

    // A scope shape the schema forbids is rejected on both counts the wire
    // contract enforces: a venue outside the leg enum, and an extra leg field
    // under additionalProperties:false.
    let mut bad_venue = update_json.clone();
    bad_venue["scope"]["legs"][0]["venue"] = json!("coinbase");
    assert!(
        !signal_validator.is_valid(&bad_venue),
        "a leg venue outside [polymarket, kalshi] is rejected"
    );
    let mut extra_field = update_json;
    extra_field["scope"]["legs"][0]["unexpected"] = json!(true);
    assert!(
        !signal_validator.is_valid(&extra_field),
        "an extra leg field is rejected under additionalProperties:false"
    );
}

#[test]
fn frozen_instance_status_registry_matches_both_schema_surfaces() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let standalone = read_json(&root.join("schemas/instance-status.schema.json"));
    let envelope = read_json(&root.join("schemas/signal-stream-message.schema.json"));
    let schema_reasons = |schema: &Value| {
        schema
            .pointer("/$defs/reason/enum")
            .and_then(Value::as_array)
            .expect("schema has a reason enum")
            .iter()
            .map(|reason| {
                reason
                    .as_str()
                    .expect("reason enum entries are strings")
                    .to_owned()
            })
            .collect::<BTreeSet<_>>()
    };
    let runtime_reasons = INSTANCE_STATUS_REASONS
        .iter()
        .map(|reason| (*reason).to_owned())
        .collect::<BTreeSet<_>>();

    // ADR 0017 froze the registry at 63; ADR 0026 grew it once, by the 53
    // reasons the all-venues definitions declare, then E47 appended
    // usdt_reference_stale LAST (append-only, G2), to 117; E53 appended
    // no_qualifying_move LAST (append-only), to 118; E51 (C3a) appended
    // flat_window LAST, to 119; C2 (E49) appended chainlink_rounds_sparse LAST,
    // to 120.
    assert_eq!(INSTANCE_STATUS_REASONS.len(), 120);
    assert_eq!(
        runtime_reasons.len(),
        120,
        "runtime registry has no duplicates"
    );
    // Not sorted: append-only. The order is a wire contract because
    // `signals-package` persists and verifies the registry in array order. The
    // ADR-0026 batch [63..116] is byte-sorted; E47 appended usdt_reference_stale
    // (index 116), E53 appended no_qualifying_move (index 117), E51 appended
    // flat_window (index 118) and C2 (E49) appended chainlink_rounds_sparse
    // (index 119) LAST, out of sort order.
    assert_eq!(INSTANCE_STATUS_REASONS[116], "usdt_reference_stale");
    assert_eq!(INSTANCE_STATUS_REASONS[117], "no_qualifying_move");
    assert_eq!(INSTANCE_STATUS_REASONS[118], "flat_window");
    assert_eq!(INSTANCE_STATUS_REASONS[119], "chainlink_rounds_sparse");
    assert!(
        INSTANCE_STATUS_REASONS[63..116]
            .windows(2)
            .all(|pair| pair[0] < pair[1]),
        "the ADR 0026 block is byte-sorted among itself"
    );
    assert_eq!(schema_reasons(&standalone), runtime_reasons);
    assert_eq!(schema_reasons(&envelope), runtime_reasons);
    assert!(!runtime_reasons.contains("market_unavailable"));
    assert!(runtime_reasons.contains("reference_constituents_unavailable"));
    assert!(runtime_reasons.contains("reference_history_warming_up"));
}

fn stream_messages(update: SignalUpdate) -> Vec<SignalStreamMessage> {
    vec![
        SignalStreamMessage::Reset {
            schema_version: SIGNAL_STREAM_SCHEMA_VERSION,
            publisher_epoch: "schema-test".to_owned(),
            sequence: 0,
            produced_at: "2026-08-13T12:00:59Z".to_owned(),
            reason: StreamResetReason::PublisherEpochStarted,
        },
        SignalStreamMessage::SignalBatch {
            schema_version: SIGNAL_STREAM_SCHEMA_VERSION,
            publisher_epoch: "schema-test".to_owned(),
            sequence: 1,
            produced_at: update.produced_at.clone(),
            conflation_policy: ConflationPolicy::None,
            values_semantics: ValuesSemantics::FullReplace,
            updates: vec![update.clone()],
        },
        SignalStreamMessage::InstanceStatus {
            schema_version: SIGNAL_STREAM_SCHEMA_VERSION,
            publisher_epoch: "schema-test".to_owned(),
            sequence: 2,
            produced_at: "2026-08-13T12:01:00.500Z".to_owned(),
            signal_id: update.signal_id.clone(),
            signal_version: update.signal_version.clone(),
            scope: update.scope.clone(),
            state: InstanceStatusState::Degraded,
            reasons: vec!["reference_constituents_unavailable".to_owned()],
            effective_time: "2026-08-13T12:01:00.500Z".to_owned(),
            last_decision_sequence: Some(update.sequence),
        },
        SignalStreamMessage::Snapshot {
            schema_version: SIGNAL_STREAM_SCHEMA_VERSION,
            publisher_epoch: "schema-test".to_owned(),
            sequence: 3,
            produced_at: "2026-08-13T12:01:01Z".to_owned(),
            as_of_sequence: 2,
            conflation_policy: ConflationPolicy::None,
            values_semantics: ValuesSemantics::FullReplace,
            updates: vec![update],
            statuses: vec![InstanceStatusEntry {
                signal_id: "btc.external.example_return".to_owned(),
                signal_version: "1.0.0".to_owned(),
                scope: signals_core::SignalScope::Global,
                state: InstanceStatusState::Degraded,
                reasons: vec!["reference_constituents_unavailable".to_owned()],
                effective_time: "2026-08-13T12:01:00.500Z".to_owned(),
                last_decision_sequence: Some(1),
            }],
        },
        SignalStreamMessage::Heartbeat {
            schema_version: SIGNAL_STREAM_SCHEMA_VERSION,
            publisher_epoch: "schema-test".to_owned(),
            sequence: 4,
            produced_at: "2026-08-13T12:01:02Z".to_owned(),
            heartbeat_interval_ms: SIGNAL_HEARTBEAT_INTERVAL_MS,
            status: ProducerStatus::Ready,
            as_of_sequence: Some(1),
        },
        SignalStreamMessage::Status {
            schema_version: SIGNAL_STREAM_SCHEMA_VERSION,
            publisher_epoch: "schema-test".to_owned(),
            sequence: 5,
            produced_at: "2026-08-13T12:01:03Z".to_owned(),
            status: ProducerStatus::Ready,
            reasons: Vec::new(),
            as_of_sequence: Some(1),
        },
        SignalStreamMessage::MarketTransition {
            schema_version: SIGNAL_STREAM_SCHEMA_VERSION,
            publisher_epoch: "schema-test".to_owned(),
            sequence: 6,
            produced_at: "2026-08-13T12:01:04Z".to_owned(),
            venue: PredictionMarketVenue::Polymarket,
            market_generation: 2,
            previous_market_id: Some("btc-updown-5m-1786726800".to_owned()),
            next_market_id: Some("btc-updown-5m-1786727100".to_owned()),
            phase: MarketTransitionPhase::Bound,
            reason: MarketTransitionReason::ScheduledRollover,
        },
        SignalStreamMessage::FeedHealth {
            schema_version: SIGNAL_STREAM_SCHEMA_VERSION,
            publisher_epoch: "schema-test".to_owned(),
            sequence: 7,
            produced_at: "2026-08-13T12:01:05Z".to_owned(),
            feeds: vec![FeedHealthReport {
                source_id: "coinbase.exchange.btc-usd.ticker".to_owned(),
                venue: "coinbase".to_owned(),
                status: FeedHealthStatus::Live,
                last_event_age_ms: Some(100),
                down_since: None,
                disconnect_count: 0,
                reconnect_count: 0,
                anomaly_velocity: 0,
            }],
        },
    ]
}

fn source_definition() -> SourceDefinition {
    SourceDefinition {
        schema_version: CONTRACT_SCHEMA_VERSION,
        source_id: "example.btc-usd.trades".to_owned(),
        version: "1.0.0".to_owned(),
        name: "Example BTC/USD trades".to_owned(),
        description: "Synthetic schema-conformance source".to_owned(),
        status: LifecycleStatus::Draft,
        transport: Transport::Websocket,
        expected_heartbeat_ms: Some(1_000),
        fields: vec![
            FieldDefinition {
                name: "price".to_owned(),
                field_type: FieldType::Decimal,
                location: FieldLocation::External,
                variants: Vec::new(),
                label: None,
            },
            FieldDefinition {
                name: "event_time".to_owned(),
                field_type: FieldType::Timestamp,
                location: FieldLocation::External,
                variants: Vec::new(),
                label: None,
            },
        ],
        event_time_field: "event_time".to_owned(),
        sequence_field: None,
    }
}

fn source_event() -> SourceEvent {
    SourceEvent {
        schema_version: CONTRACT_SCHEMA_VERSION,
        source_id: "example.btc-usd.trades".to_owned(),
        source_version: "1.0.0".to_owned(),
        publisher_epoch: "schema-test".to_owned(),
        sequence: 1,
        event_time: "2026-08-13T12:00:00Z".to_owned(),
        received_at: "2026-08-13T12:00:00.001Z".to_owned(),
        values: BTreeMap::from([
            ("price".to_owned(), json!("100000.25")),
            ("event_time".to_owned(), json!("2026-08-13T12:00:00Z")),
        ]),
    }
}

fn signal_definition() -> SignalDefinition {
    SignalDefinition {
        schema_version: CONTRACT_SCHEMA_VERSION,
        signal_id: "btc.external.example_return".to_owned(),
        version: "1.0.0".to_owned(),
        name: "Example BTC/USD return".to_owned(),
        description: "Synthetic schema-conformance signal".to_owned(),
        status: LifecycleStatus::Experimental,
        approximate_interval_ms: 60_000,
        staleness_bound_ms: 180_000,
        inputs: vec![SignalInput {
            kind: InputKind::Source,
            id: "example.btc-usd.trades".to_owned(),
            version: "1.0.0".to_owned(),
        }],
        fields: vec![
            FieldDefinition {
                name: "return_1m".to_owned(),
                field_type: FieldType::Decimal,
                location: FieldLocation::Internal,
                variants: Vec::new(),
                label: None,
            },
            FieldDefinition {
                name: "window_close".to_owned(),
                field_type: FieldType::Timestamp,
                location: FieldLocation::Internal,
                variants: Vec::new(),
                label: None,
            },
        ],
        event_time_field: "window_close".to_owned(),
        scope_kind: None,
    }
}

fn signal_update() -> SignalUpdate {
    SignalUpdate {
        schema_version: SIGNAL_UPDATE_SCHEMA_VERSION,
        signal_id: "btc.external.example_return".to_owned(),
        signal_version: "1.0.0".to_owned(),
        publisher_epoch: "schema-test".to_owned(),
        sequence: 1,
        event_time: "2026-08-13T12:01:00Z".to_owned(),
        produced_at: "2026-08-13T12:01:00.001Z".to_owned(),
        valid_from: "2026-08-13T12:01:00.001Z".to_owned(),
        valid_until: "2026-08-13T12:04:00.001Z".to_owned(),
        quality: Quality {
            state: QualityState::Ready,
            reasons: Vec::new(),
        },
        values: BTreeMap::from([
            ("return_1m".to_owned(), json!("-0.00125")),
            ("window_close".to_owned(), json!("2026-08-13T12:01:00Z")),
        ]),
        scope: signals_core::SignalScope::Global,
    }
}
