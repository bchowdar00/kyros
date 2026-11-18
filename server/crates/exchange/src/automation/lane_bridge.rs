//! Bridges the signal lanes' in-process output onto the [`SignalBus`].
//!
//! Every lane the supervisor (`crate::signals`) runs is started with a
//! stream hook that sends each stream-v3 message it writes, before the
//! bytes reach its output file, as a [`LaneMessage`] on one unbounded
//! channel. This task drains that channel and republishes:
//!
//! * `signal_batch` / `snapshot`: every update becomes a [`SignalFrame`]
//!   under the registry id (`<wire id>.v<major>`), with the update's
//!   `quality` and `values` verbatim, and goes on the bus, whose fast
//!   path types it and hands it to the signals platform;
//! * `instance_status`: a quality-only update (`warming_up`, `degraded`,
//!   `unavailable`, `invalidated`) for subscribers and the catalogue, plus
//!   a snapshot clear on the platform, so a strategy reading a signal
//!   that has just gone bad sees it as unpublished until the next ready
//!   value rather than triggering on a stale reading;
//! * everything else (heartbeats, producer status, resets, market
//!   transitions, feed health) is dropped here.
//!
//! The lanes' `signal_batch` carries only Ready updates by construction
//! (non-ready evaluations become `instance_status` lines), so a value the
//! platform ingests is always one the lane vouched for.
//!
//! Instances: a market-scoped update (Kalshi, Polymarket) is published
//! under its signal id like any other, so the newest bound market wins.
//! Binding an instance to the exchange series whose round it matches is
//! the next step here and changes nothing upstream.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::Value as Json;
use signals_core::{InstanceStatusEntry, SignalStreamMessage, SignalUpdate};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::automation::signal_bus::{Quality, SignalBus, SignalFrame, rfc3339_to_ms};
use crate::automation::sme::task::PlatformHandle;
use crate::signals::LaneMessage;

/// Spawn the bridge over the supervisor's message stream. Runs until the
/// senders are gone (process shutdown).
pub fn spawn_lane_bridge(bus: Arc<SignalBus>, platform: PlatformHandle, mut rx: UnboundedReceiver<LaneMessage>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut published: u64 = 0;
        while let Some(LaneMessage { lane, message }) = rx.recv().await {
            let n = publish_message(&bus, &platform, &message);
            if n > 0 && published == 0 {
                tracing::info!(lane, "lane bridge: first frames published");
            }
            published += n;
        }
        tracing::info!(published, "lane bridge stopped");
    })
}

/// Publish the frames in one stream message. Returns how many were published.
pub fn publish_message(bus: &SignalBus, platform: &PlatformHandle, message: &SignalStreamMessage) -> u64 {
    match message {
        SignalStreamMessage::SignalBatch { updates, .. } => publish_updates(bus, updates),
        SignalStreamMessage::Snapshot { updates, statuses, .. } => {
            // A snapshot restates the current world: values for the ready
            // instances, statuses for the rest.
            let n = publish_updates(bus, updates);
            for status in statuses {
                apply_status(bus, platform, status);
            }
            n
        }
        SignalStreamMessage::InstanceStatus { signal_id, signal_version, scope, state, reasons, effective_time, last_decision_sequence, .. } => {
            apply_status(
                bus,
                platform,
                &InstanceStatusEntry {
                    signal_id: signal_id.clone(),
                    signal_version: signal_version.clone(),
                    scope: scope.clone(),
                    state: *state,
                    reasons: reasons.clone(),
                    effective_time: effective_time.clone(),
                    last_decision_sequence: *last_decision_sequence,
                },
            );
            0
        }
        _ => 0,
    }
}

fn publish_updates(bus: &SignalBus, updates: &[SignalUpdate]) -> u64 {
    let mut n = 0;
    for update in updates {
        let frame = frame_from_update(update);
        if bus.knows(&frame.signal_id) {
            bus.publish(frame);
            n += 1;
        }
    }
    n
}

/// A not-ready instance: update the catalogue's readiness and forget the
/// platform's snapshot for it. Unknown ids are ignored.
fn apply_status(bus: &SignalBus, platform: &PlatformHandle, status: &InstanceStatusEntry) {
    let id = registry_id(&status.signal_id, &status.signal_version);
    if !bus.knows(&id) {
        return;
    }
    let ts_ms = rfc3339_to_ms(&status.effective_time).unwrap_or(0);
    let quality = Quality { state: quality_state(status), reasons: status.reasons.clone() };
    bus.set_quality(&id, ts_ms, quality);
    platform.clear_snapshot(&id);
}

/// The wire form of a status state, matching the lanes' JSON.
fn quality_state(status: &InstanceStatusEntry) -> String {
    serde_json::to_value(status.state)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".into())
}

/// `btc.coinbase.rsi` + `1.0.0` → `btc.coinbase.rsi.v1`.
pub fn registry_id(signal_id: &str, signal_version: &str) -> String {
    let major = signal_version.split('.').next().unwrap_or("1");
    format!("{signal_id}.v{major}")
}

/// The bus frame for one lane update: the registry id, the update's
/// `produced_at`, and its quality and values verbatim.
pub fn frame_from_update(update: &SignalUpdate) -> SignalFrame {
    let quality = serde_json::to_value(&update.quality)
        .ok()
        .and_then(|v| serde_json::from_value::<Quality>(v).ok())
        .unwrap_or_default();
    let values: BTreeMap<String, Json> = update.values.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    SignalFrame {
        signal_id: registry_id(&update.signal_id, &update.signal_version),
        ts_ms: rfc3339_to_ms(&update.produced_at).unwrap_or(0),
        quality,
        values,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::automation::signals::SignalValue;
    use crate::automation::sme::task::PlatformCommand;
    use crate::registry::{self, Inputs, PriceSource};
    use signals_core::{
        ConflationPolicy, InstanceStatusState, QualityState, SIGNAL_STREAM_SCHEMA_VERSION, SignalScope, ValuesSemantics,
    };
    use std::path::PathBuf;
    use strategy_compiler::Registry;

    /// The registry with the checkout's definition files and the spot lane live.
    pub(crate) fn registry_with_spot_lane() -> Arc<Registry> {
        let defs = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../definitions"));
        Arc::new(
            registry::build(&Inputs { price_source: PriceSource::GeminiIndex, live_lanes: &["spot"], definitions_dir: Some(defs), copy_dir: None })
                .expect("registry with definitions builds"),
        )
    }

    /// A ready update for a lane signal with the given values.
    pub(crate) fn ready_update(signal_id: &str, produced_at: &str, values: &[(&str, Json)]) -> SignalUpdate {
        SignalUpdate {
            schema_version: SIGNAL_STREAM_SCHEMA_VERSION,
            signal_id: signal_id.into(),
            signal_version: "1.0.0".into(),
            publisher_epoch: "test".into(),
            sequence: 0,
            event_time: produced_at.into(),
            produced_at: produced_at.into(),
            valid_from: produced_at.into(),
            valid_until: produced_at.into(),
            quality: signals_core::Quality { state: QualityState::Ready, reasons: Vec::new() },
            values: values.iter().map(|(k, v)| (k.to_string(), v.clone())).collect(),
            scope: SignalScope::Global,
        }
    }

    pub(crate) fn batch(updates: Vec<SignalUpdate>) -> SignalStreamMessage {
        SignalStreamMessage::SignalBatch {
            schema_version: SIGNAL_STREAM_SCHEMA_VERSION,
            publisher_epoch: "test".into(),
            sequence: 1,
            produced_at: "2026-09-06T00:00:01Z".into(),
            conflation_policy: ConflationPolicy::None,
            values_semantics: ValuesSemantics::FullReplace,
            updates,
        }
    }

    fn status(signal_id: &str, state: InstanceStatusState) -> SignalStreamMessage {
        SignalStreamMessage::InstanceStatus {
            schema_version: SIGNAL_STREAM_SCHEMA_VERSION,
            publisher_epoch: "test".into(),
            sequence: 2,
            produced_at: "2026-09-06T00:00:02Z".into(),
            signal_id: signal_id.into(),
            signal_version: "1.0.0".into(),
            scope: SignalScope::Global,
            state,
            reasons: vec!["coinbase_stale".into()],
            effective_time: "2026-09-06T00:00:02Z".into(),
            last_decision_sequence: Some(0),
        }
    }

    #[test]
    fn update_becomes_a_frame_under_the_registry_id() {
        let u = ready_update("btc.coinbase.spot_quote", "1970-01-01T00:00:01Z", &[("bid", Json::from("1.0")), ("input_age_ms", Json::from(5))]);
        let f = frame_from_update(&u);
        assert_eq!(f.signal_id, "btc.coinbase.spot_quote.v1");
        assert_eq!(f.ts_ms, 1000);
        assert_eq!(f.quality.state, "ready");
        assert_eq!(f.values["input_age_ms"], 5);
        assert_eq!(registry_id("x.y", "2.3.4"), "x.y.v2");
    }

    /// A batch for a registry signal reaches the bus's latest store and the
    /// platform as a typed event; an unknown signal is dropped.
    #[test]
    fn batch_publishes_known_signals_to_the_bus_and_platform() {
        let reg = registry_with_spot_lane();
        let def = reg.signals.iter().find(|s| s.signal_id == "btc.coinbase.rsi.v1").expect("rsi is defined");
        let (platform, mut inbox) = PlatformHandle::channel();
        let bus = SignalBus::new(reg.clone(), Some(platform.clone()));
        // Every declared field, typed by its declaration.
        let values: Vec<(&str, Json)> = def
            .fields
            .iter()
            .map(|f| {
                let v = match &f.field_type {
                    strategy_compiler::ir::FieldType::Decimal => Json::from("55.5"),
                    strategy_compiler::ir::FieldType::Enum { variants } => Json::from(variants[0].clone()),
                    strategy_compiler::ir::FieldType::Timestamp => Json::from("2026-09-06T00:00:01Z"),
                    strategy_compiler::ir::FieldType::Bool => Json::from(true),
                    strategy_compiler::ir::FieldType::String => Json::from("s"),
                    _ => Json::from(7),
                };
                (f.name.as_str(), v)
            })
            .collect();
        let known = ready_update("btc.coinbase.rsi", "2026-09-06T00:00:01Z", &values);
        let unknown = ready_update("btc.nowhere.nothing", "2026-09-06T00:00:01Z", &[("x", Json::from(1))]);
        assert_eq!(publish_message(&bus, &platform, &batch(vec![unknown, known])), 1);
        assert_eq!(bus.latest("btc.coinbase.rsi.v1").unwrap().quality.state, "ready");
        assert!(bus.latest("btc.nowhere.nothing.v1").is_none());
        assert!(matches!(inbox.try_recv(), Ok(PlatformCommand::Enable(ref s)) if s == "btc.coinbase.rsi.v1"));
        match inbox.try_recv() {
            Ok(PlatformCommand::Ingest(ev)) => {
                assert_eq!(ev.signal_id, "btc.coinbase.rsi.v1");
                assert_eq!(ev.fields.len(), def.fields.len());
                assert!(ev.fields.values().any(|v| matches!(v, SignalValue::Decimal(_))));
            }
            other => panic!("expected an ingest, got {:?}", other.map(|_| ())),
        }
        assert_eq!(bus.ingest_stats("btc.coinbase.rsi.v1").published, 1);
    }

    /// An instance status updates the catalogue's readiness and clears the
    /// platform's snapshot; unknown ids do neither.
    #[test]
    fn instance_status_updates_quality_and_clears_the_snapshot() {
        let reg = registry_with_spot_lane();
        let (platform, mut inbox) = PlatformHandle::channel();
        let bus = SignalBus::new(reg, Some(platform.clone()));
        publish_message(&bus, &platform, &status("btc.coinbase.rsi", InstanceStatusState::Degraded));
        let f = bus.latest("btc.coinbase.rsi.v1").unwrap();
        assert_eq!(f.quality.state, "degraded");
        assert_eq!(f.quality.reasons, vec!["coinbase_stale"]);
        assert!(matches!(inbox.try_recv(), Ok(PlatformCommand::ClearSnapshot(ref s)) if s == "btc.coinbase.rsi.v1"));
        assert!(inbox.try_recv().is_err());

        publish_message(&bus, &platform, &status("btc.nowhere.nothing", InstanceStatusState::WarmingUp));
        assert!(inbox.try_recv().is_err(), "unknown ids reach nothing");
    }

    /// Control messages are not signals.
    #[test]
    fn other_message_kinds_are_dropped() {
        let reg = registry_with_spot_lane();
        let (platform, mut inbox) = PlatformHandle::channel();
        let bus = SignalBus::new(reg, Some(platform.clone()));
        let reset = SignalStreamMessage::Reset {
            schema_version: SIGNAL_STREAM_SCHEMA_VERSION,
            publisher_epoch: "test".into(),
            sequence: 0,
            produced_at: "2026-09-06T00:00:00Z".into(),
            reason: signals_core::StreamResetReason::PublisherEpochStarted,
        };
        assert_eq!(publish_message(&bus, &platform, &reset), 0);
        assert!(inbox.try_recv().is_err());
    }

    /// The task end to end: a message on the lane channel lands on the bus.
    #[tokio::test]
    async fn bridge_task_drains_the_lane_channel() {
        let reg = registry_with_spot_lane();
        let (platform, _inbox) = PlatformHandle::channel();
        let bus = SignalBus::new(reg, Some(platform.clone()));
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let task = spawn_lane_bridge(bus.clone(), platform, rx);
        tx.send(LaneMessage { lane: "spot", message: status("btc.coinbase.rsi", InstanceStatusState::WarmingUp) }).unwrap();
        drop(tx);
        task.await.unwrap();
        assert_eq!(bus.latest("btc.coinbase.rsi.v1").unwrap().quality.state, "warming_up");
    }
}
