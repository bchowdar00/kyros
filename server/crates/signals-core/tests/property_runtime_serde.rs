use std::collections::BTreeMap;

use proptest::prelude::*;
use serde_json::Value;
use signals_core::{
    ConflationPolicy, InstanceStatusState, MarketTransitionPhase, MarketTransitionReason,
    PredictionMarketVenue, ProducerStatus, Quality, QualityState, SignalScope, SignalStreamMessage,
    SignalUpdate, SourceEvent, StreamResetReason, ValuesSemantics,
};

fn json_value() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::from),
        any::<i64>().prop_map(Value::from),
        any::<u64>().prop_map(Value::from),
        ".*".prop_map(Value::from),
    ];
    leaf.prop_recursive(3, 16, 4, |inner| {
        prop_oneof![
            proptest::collection::vec(inner.clone(), 0..4).prop_map(Value::from),
            proptest::collection::btree_map("[a-z_]{1,6}", inner, 0..4)
                .prop_map(|pairs| Value::Object(pairs.into_iter().collect())),
        ]
    })
}

fn values_map() -> impl Strategy<Value = BTreeMap<String, Value>> {
    proptest::collection::btree_map("[a-z_]{1,8}", json_value(), 0..4)
}

fn producer_status() -> impl Strategy<Value = ProducerStatus> {
    prop_oneof![
        Just(ProducerStatus::Starting),
        Just(ProducerStatus::Ready),
        Just(ProducerStatus::Degraded),
    ]
}

fn signal_scope() -> impl Strategy<Value = SignalScope> {
    prop_oneof![
        Just(SignalScope::Global),
        (".*", any::<u64>()).prop_map(|(market_id, market_generation)| SignalScope::Market {
            market_id,
            market_generation,
        }),
    ]
}

fn instance_status_state() -> impl Strategy<Value = InstanceStatusState> {
    prop_oneof![
        Just(InstanceStatusState::WarmingUp),
        Just(InstanceStatusState::Degraded),
        Just(InstanceStatusState::Unavailable),
        Just(InstanceStatusState::Invalidated),
    ]
}

/// Generate a quality that honors the runtime rule: `Ready` carries no reasons
/// while `WarmingUp`/`Degraded` carry at least one non-empty reason.
fn quality() -> impl Strategy<Value = Quality> {
    prop_oneof![
        Just(Quality {
            state: QualityState::Ready,
            reasons: Vec::new(),
        }),
        (
            prop_oneof![Just(QualityState::WarmingUp), Just(QualityState::Degraded)],
            proptest::collection::vec("[a-z]{1,12}", 1..4),
        )
            .prop_map(|(state, reasons)| Quality { state, reasons }),
    ]
}

/// Generate a producer status paired with reasons under the same runtime rule
/// enforced for stream `Status` messages: only non-`Ready` may carry reasons.
fn status_and_reasons() -> impl Strategy<Value = (ProducerStatus, Vec<String>)> {
    prop_oneof![
        Just((ProducerStatus::Ready, Vec::new())),
        (
            prop_oneof![
                Just(ProducerStatus::Starting),
                Just(ProducerStatus::Degraded)
            ],
            proptest::collection::vec("[a-z]{1,12}", 1..4),
        ),
    ]
}

prop_compose! {
    fn source_event()(
        schema_version in any::<u16>(),
        source_id in ".*",
        source_version in ".*",
        publisher_epoch in ".*",
        sequence in any::<u64>(),
        event_time in ".*",
        received_at in ".*",
        values in values_map(),
    ) -> SourceEvent {
        SourceEvent {
            schema_version,
            source_id,
            source_version,
            publisher_epoch,
            sequence,
            event_time,
            received_at,
            values,
        }
    }
}

prop_compose! {
    fn signal_update()(
        schema_version in any::<u16>(),
        signal_id in ".*",
        signal_version in ".*",
        publisher_epoch in ".*",
        sequence in any::<u64>(),
        event_time in ".*",
        produced_at in ".*",
        valid_from in ".*",
        valid_until in ".*",
        quality in quality(),
        values in values_map(),
    ) -> SignalUpdate {
        SignalUpdate {
            schema_version,
            signal_id,
            signal_version,
            publisher_epoch,
            sequence,
            event_time,
            produced_at,
            valid_from,
            valid_until,
            quality,
            values,
            scope: SignalScope::Global,
        }
    }
}

fn signal_batch() -> impl Strategy<Value = SignalStreamMessage> {
    (
        any::<u16>(),
        ".*",
        any::<u64>(),
        ".*",
        proptest::collection::vec(signal_update(), 0..3),
    )
        .prop_map(
            |(schema_version, publisher_epoch, sequence, produced_at, updates)| {
                SignalStreamMessage::SignalBatch {
                    schema_version,
                    publisher_epoch,
                    sequence,
                    produced_at,
                    conflation_policy: ConflationPolicy::None,
                    values_semantics: ValuesSemantics::FullReplace,
                    updates,
                }
            },
        )
}

fn snapshot() -> impl Strategy<Value = SignalStreamMessage> {
    (
        any::<u16>(),
        ".*",
        any::<u64>(),
        ".*",
        any::<u64>(),
        proptest::collection::vec(signal_update(), 0..3),
    )
        .prop_map(
            |(schema_version, publisher_epoch, sequence, produced_at, as_of_sequence, updates)| {
                SignalStreamMessage::Snapshot {
                    schema_version,
                    publisher_epoch,
                    sequence,
                    produced_at,
                    as_of_sequence,
                    conflation_policy: ConflationPolicy::None,
                    values_semantics: ValuesSemantics::FullReplace,
                    updates,
                    statuses: Vec::new(),
                }
            },
        )
}

fn instance_status() -> impl Strategy<Value = SignalStreamMessage> {
    (
        any::<u16>(),
        ".*",
        any::<u64>(),
        ".*",
        ".*",
        ".*",
        signal_scope(),
        instance_status_state(),
        proptest::collection::vec(".*", 0..4),
        ".*",
        proptest::option::of(any::<u64>()),
    )
        .prop_map(
            |(
                schema_version,
                publisher_epoch,
                sequence,
                produced_at,
                signal_id,
                signal_version,
                scope,
                state,
                reasons,
                effective_time,
                last_decision_sequence,
            )| SignalStreamMessage::InstanceStatus {
                schema_version,
                publisher_epoch,
                sequence,
                produced_at,
                signal_id,
                signal_version,
                scope,
                state,
                reasons,
                effective_time,
                last_decision_sequence,
            },
        )
}

fn heartbeat() -> impl Strategy<Value = SignalStreamMessage> {
    (
        any::<u16>(),
        ".*",
        any::<u64>(),
        ".*",
        any::<u64>(),
        producer_status(),
        proptest::option::of(any::<u64>()),
    )
        .prop_map(
            |(
                schema_version,
                publisher_epoch,
                sequence,
                produced_at,
                heartbeat_interval_ms,
                status,
                as_of_sequence,
            )| {
                SignalStreamMessage::Heartbeat {
                    schema_version,
                    publisher_epoch,
                    sequence,
                    produced_at,
                    heartbeat_interval_ms,
                    status,
                    as_of_sequence,
                }
            },
        )
}

fn status() -> impl Strategy<Value = SignalStreamMessage> {
    (
        any::<u16>(),
        ".*",
        any::<u64>(),
        ".*",
        status_and_reasons(),
        proptest::option::of(any::<u64>()),
    )
        .prop_map(
            |(
                schema_version,
                publisher_epoch,
                sequence,
                produced_at,
                (status, reasons),
                as_of_sequence,
            )| {
                SignalStreamMessage::Status {
                    schema_version,
                    publisher_epoch,
                    sequence,
                    produced_at,
                    status,
                    reasons,
                    as_of_sequence,
                }
            },
        )
}

fn reset() -> impl Strategy<Value = SignalStreamMessage> {
    (any::<u16>(), ".*", any::<u64>(), ".*").prop_map(
        |(schema_version, publisher_epoch, sequence, produced_at)| SignalStreamMessage::Reset {
            schema_version,
            publisher_epoch,
            sequence,
            produced_at,
            reason: StreamResetReason::PublisherEpochStarted,
        },
    )
}

fn market_transition() -> impl Strategy<Value = SignalStreamMessage> {
    let venue = prop_oneof![
        Just(PredictionMarketVenue::Polymarket),
        Just(PredictionMarketVenue::Kalshi),
    ];
    let phase = prop_oneof![
        Just(MarketTransitionPhase::Unavailable),
        Just(MarketTransitionPhase::Bound),
    ];
    let reason = prop_oneof![
        Just(MarketTransitionReason::InitialBind),
        Just(MarketTransitionReason::ScheduledRollover),
        Just(MarketTransitionReason::DiscoveryUnreachable),
        Just(MarketTransitionReason::NextMarketUnlisted),
        Just(MarketTransitionReason::InvalidDiscovery),
        Just(MarketTransitionReason::CloseToNextOpenGap),
        Just(MarketTransitionReason::ProviderLifecycleChange),
        Just(MarketTransitionReason::RecoveryBind),
    ];
    (
        any::<u16>(),
        ".*",
        any::<u64>(),
        ".*",
        venue,
        any::<u64>(),
        proptest::option::of(".*"),
        proptest::option::of(".*"),
        phase,
        reason,
    )
        .prop_map(
            |(
                schema_version,
                publisher_epoch,
                sequence,
                produced_at,
                venue,
                market_generation,
                previous_market_id,
                next_market_id,
                phase,
                reason,
            )| {
                SignalStreamMessage::MarketTransition {
                    schema_version,
                    publisher_epoch,
                    sequence,
                    produced_at,
                    venue,
                    market_generation,
                    previous_market_id,
                    next_market_id,
                    phase,
                    reason,
                }
            },
        )
}

fn stream_message() -> impl Strategy<Value = SignalStreamMessage> {
    prop_oneof![
        signal_batch(),
        snapshot(),
        instance_status(),
        heartbeat(),
        status(),
        reset(),
        market_transition(),
    ]
}

proptest! {
    #[test]
    fn source_event_serde_round_trips(event in source_event()) {
        let text = serde_json::to_string(&event).expect("SourceEvent serializes");
        let from_text: SourceEvent = serde_json::from_str(&text).expect("SourceEvent parses");
        prop_assert_eq!(&event, &from_text);
        let value = serde_json::to_value(&event).expect("SourceEvent to Value");
        let from_value: SourceEvent = serde_json::from_value(value).expect("SourceEvent from Value");
        prop_assert_eq!(event, from_value);
    }

    #[test]
    fn signal_update_serde_round_trips(update in signal_update()) {
        let text = serde_json::to_string(&update).expect("SignalUpdate serializes");
        let from_text: SignalUpdate = serde_json::from_str(&text).expect("SignalUpdate parses");
        prop_assert_eq!(&update, &from_text);
        let value = serde_json::to_value(&update).expect("SignalUpdate to Value");
        let from_value: SignalUpdate = serde_json::from_value(value).expect("SignalUpdate from Value");
        prop_assert_eq!(update, from_value);
    }

    #[test]
    fn stream_message_serde_round_trips(message in stream_message()) {
        let text = serde_json::to_string(&message).expect("SignalStreamMessage serializes");
        let from_text: SignalStreamMessage =
            serde_json::from_str(&text).expect("SignalStreamMessage parses");
        prop_assert_eq!(&message, &from_text);
        let value = serde_json::to_value(&message).expect("SignalStreamMessage to Value");
        let from_value: SignalStreamMessage =
            serde_json::from_value(value).expect("SignalStreamMessage from Value");
        prop_assert_eq!(message, from_value);
    }
}
