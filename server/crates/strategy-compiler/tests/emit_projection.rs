//! Projection consistency, at the STRUCT level: the manifest projection must agree
//! with the lowering result the program is projected from — same identity, same
//! subscriptions, same legs and arm lists, same trigger table, same intent index.
//! Together with the program snapshots this pins the two artifacts to one source;
//! neither can drift alone.

use strategy_compiler::lower::{self, RowSource, TimerRow};
use strategy_compiler::ir::{WireCondition, WireTriggerSource};

mod common;

#[test]
fn manifest_agrees_with_the_lowering_result() {
    for name in common::STRATEGIES {
        let lowered = lower::lower(&common::corpus_valid(name));
        let manifest = strategy_compiler::emit::project_manifest(&lowered);

        assert_eq!(manifest.strategy_id, lowered.strategy_id, "{name}: identity");
        assert_eq!(manifest.name, lowered.meta_name, "{name}: display name");
        assert_eq!(
            manifest.source_hash, lowered.source_hash,
            "{name}: source hash"
        );
        assert_eq!(
            manifest.dsl_version, lowered.dsl_version,
            "{name}: dsl version"
        );

        let manifest_signals: Vec<&str> = manifest
            .signals
            .iter()
            .map(|row| row.signal_id.as_str())
            .collect();
        let subscriptions: Vec<&str> = lowered.subscriptions.iter().map(String::as_str).collect();
        assert_eq!(manifest_signals, subscriptions, "{name}: subscriptions");

        // The document-level trigger table: one wire row per lowered row, same order.
        assert_eq!(
            manifest.triggers.len(),
            lowered.triggers.len(),
            "{name}: trigger count"
        );
        for (wire, (trigger_name, row)) in manifest.triggers.iter().zip(&lowered.triggers) {
            assert_eq!(&wire.id, trigger_name, "{name}: trigger order");
            assert_eq!(wire.fire, row.fire, "{name}.{trigger_name}: fire");
            assert_eq!(wire.rearm.mode, row.rearm, "{name}.{trigger_name}: rearm");
            match (&wire.source, &row.source) {
                (WireTriggerSource::Condition(condition), RowSource::All(leaves)) => {
                    let wire_count = match condition {
                        WireCondition::Leaf(_) => 1,
                        WireCondition::All { all } => all.len(),
                        WireCondition::Any { .. } => {
                            panic!("{name}.{trigger_name}: group kind flipped")
                        }
                    };
                    assert_eq!(wire_count, leaves.len(), "{name}.{trigger_name}");
                }
                (
                    WireTriggerSource::Condition(WireCondition::Any { any }),
                    RowSource::Any(leaves),
                ) => {
                    assert_eq!(any.len(), leaves.len(), "{name}.{trigger_name}");
                }
                (
                    WireTriggerSource::Timer(timer),
                    RowSource::Timer(TimerRow::WindowClock {
                        instrument_id,
                        remaining_ms,
                    }),
                ) => {
                    let params = timer.window_clock.as_ref().expect("window_clock params");
                    assert_eq!(&params.instrument_id, instrument_id);
                    assert_eq!(params.remaining_ms, *remaining_ms);
                }
                (WireTriggerSource::Timer(timer), RowSource::Timer(TimerRow::After { elapsed_ms })) => {
                    assert_eq!(timer.delay_ms, Some(*elapsed_ms));
                }
                (WireTriggerSource::Timer(timer), RowSource::Timer(TimerRow::Interval { every_ms })) => {
                    assert_eq!(timer.period_ms, Some(*every_ms));
                }
                (wire, row) => {
                    panic!("{name}.{trigger_name}: source shapes diverged: {wire:?} vs {row:?}")
                }
            }
        }

        // The leg machine: stages are the legs' delivery masks, verbatim.
        assert_eq!(
            manifest.lifecycle_stages.len(),
            lowered.legs.len(),
            "{name}: leg count"
        );
        assert_eq!(
            manifest.lifecycle_stages[0].name, lowered.initial_leg,
            "{name}: the first stage is the initial leg"
        );
        for (stage, leg) in manifest.lifecycle_stages.iter().zip(&lowered.legs) {
            assert_eq!(stage.name, leg.name, "{name}: leg order");
            assert_eq!(
                stage.resident, leg.resident,
                "{name}.{}: resident",
                leg.name
            );
            assert_eq!(stage.arm, leg.arm, "{name}.{}: arm list", leg.name);
            for armed in &stage.arm {
                assert!(
                    manifest.triggers.iter().any(|wire| &wire.id == armed),
                    "{name}.{}: armed trigger {armed} missing from the table",
                    leg.name
                );
            }
        }

        // The intent index: same keys, same totalized tables, same bounds.
        assert_eq!(
            manifest.intent_index.len(),
            lowered.intent_index.len(),
            "{name}: intent count"
        );
        for ((wire_id, wire_entry), (id, entry)) in
            manifest.intent_index.iter().zip(&lowered.intent_index)
        {
            assert_eq!(wire_id, id, "{name}: intent id order");
            assert_eq!(wire_entry.on.fill, entry.on.fill, "{name}.{id}: fill");
            assert_eq!(
                wire_entry.on.partial, entry.on.partial,
                "{name}.{id}: partial"
            );
            assert_eq!(wire_entry.on.reject, entry.on.reject, "{name}.{id}: reject");
            assert_eq!(
                wire_entry.on.cancel_ack, entry.on.cancel_ack,
                "{name}.{id}: cancel_ack"
            );
            assert_eq!(wire_entry.on.expiry, entry.on.expiry, "{name}.{id}: expiry");
            assert_eq!(wire_entry.max_size, entry.max_size, "{name}.{id}: max_size");
        }

        assert_eq!(manifest.envelope, lowered.envelope, "{name}: envelope");
    }
}
