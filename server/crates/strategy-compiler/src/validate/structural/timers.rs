//! Timer shape rules: an `interval` period has a floor (the SME is not a busy loop),
//! and an `interval` that never rearms is a one-shot wearing a repeating timer's name.

use crate::ir::{RearmMode, Strategy, TimerSpec, TriggerSource};

use crate::validate::diag::{DiagCode, DiagPath, Diagnostic};

/// The shortest legal `interval` period, milliseconds.
pub const INTERVAL_FLOOR_MS: u64 = 250;

/// Run the timer shape rules.
pub fn check(strategy: &Strategy, diagnostics: &mut Vec<Diagnostic>) {
    for (t, trigger) in strategy.triggers.iter().enumerate() {
        let TriggerSource::Timer(TimerSpec::Interval { every }) = &trigger.source else {
            continue;
        };
        let path = DiagPath::root().key("triggers").index(t).key("timer");
        if every.as_millis() < INTERVAL_FLOOR_MS {
            diagnostics.push(
                Diagnostic::rule(
                    DiagCode::IntervalTooFast,
                    path.clone().key("every"),
                    format!(
                        "interval `{}` fires every {} — the floor is {INTERVAL_FLOOR_MS}ms",
                        trigger.name,
                        every.as_str()
                    ),
                )
                .with_help("slow the interval down, or wake on the signal you are polling"),
            );
        }
        if trigger.rearm.mode == RearmMode::Never {
            diagnostics.push(
                Diagnostic::rule(
                    DiagCode::IntervalNeverRearms,
                    DiagPath::root().key("triggers").index(t).key("rearm"),
                    format!(
                        "interval `{}` has rearm: never — it fires once and is then spent",
                        trigger.name
                    ),
                )
                .with_help("use rearm: immediate for a repeating timer, or an `after` timer for a one-shot"),
            );
        }
    }
}
