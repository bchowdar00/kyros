//! The per-(connection, trigger) fire/rearm state machine. A direct port of
//! the prototype's `TriggerSubscriber.on_truth`, plus the IR v0.3 cooldown.
//!
//! ```text
//! crossing  = truth && !previous_truth
//! qualifies = fire == IfTrue ? truth : crossing
//! if needs_false && !truth { needs_false = false }
//! fires     = qualifies && !spent && !needs_false
//! if fires { match rearm { Never => spent = true, FreshCrossing => needs_false = true, Immediate => {} } }
//! previous_truth = truth
//! ```
//!
//! Three rulings carried over verbatim:
//! 1. Bookkeeping advances whether or not the trigger is in the mask. A
//!    `rearm: never` trigger that fires while disarmed is spent forever.
//! 2. Crossings observed while disarmed are lost, not queued.
//! 3. Leg changes never reset anything. Only a session reset rewinds.
//!
//! Cooldown is checked *after* the state machine decides a fire; a fire the
//! cooldown suppresses still advanced the bookkeeping (consistent with the
//! mask ruling).

use strategy_compiler::ir::{Fire, RearmMode};

#[derive(Debug, Clone)]
pub struct TriggerSubscriber {
    pub fire: Fire,
    pub rearm: RearmMode,
    pub cooldown_ms: Option<u64>,
    pub previous_truth: bool,
    pub spent: bool,
    pub needs_false: bool,
    /// Earliest `ts_ms` at which the next fire may be delivered.
    pub next_allowed_at_ms: Option<u64>,
}

impl TriggerSubscriber {
    /// `previous_truth` is the row's truth at registration time, so a
    /// condition that is already true when a strategy connects does not
    /// register as a crossing.
    pub fn new(fire: Fire, rearm: RearmMode, cooldown_ms: Option<u64>, previous_truth: bool) -> Self {
        TriggerSubscriber { fire, rearm, cooldown_ms, previous_truth, spent: false, needs_false: false, next_allowed_at_ms: None }
    }

    /// Advance on a fresh truth value at time `ts_ms`. Returns whether the
    /// trigger logically fires (before the mask is consulted).
    pub fn on_truth(&mut self, truth: bool, ts_ms: u64) -> bool {
        let crossing = truth && !self.previous_truth;
        let qualifies = match self.fire {
            Fire::IfTrue => truth,
            Fire::OnCrossing => crossing,
        };
        if self.needs_false && !truth {
            self.needs_false = false;
        }
        let logical_fire = qualifies && !self.spent && !self.needs_false;
        if logical_fire {
            match self.rearm {
                RearmMode::Never => self.spent = true,
                RearmMode::FreshCrossing => self.needs_false = true,
                RearmMode::Immediate => {}
            }
        }
        self.previous_truth = truth;
        if !logical_fire {
            return false;
        }
        self.apply_cooldown(ts_ms)
    }

    /// A timer tick: timers have no truth to cross, every tick is a logical
    /// fire subject to `spent` and cooldown. `rearm: never` spends the timer.
    pub fn on_timer(&mut self, ts_ms: u64) -> bool {
        if self.spent {
            return false;
        }
        if self.rearm == RearmMode::Never {
            self.spent = true;
        }
        self.apply_cooldown(ts_ms)
    }

    fn apply_cooldown(&mut self, ts_ms: u64) -> bool {
        if self.next_allowed_at_ms.is_some_and(|next| ts_ms < next) {
            return false;
        }
        if let Some(cd) = self.cooldown_ms {
            self.next_allowed_at_ms = Some(ts_ms.saturating_add(cd));
        }
        true
    }

    /// Session reset: rewind every piece of bookkeeping.
    pub fn reset(&mut self) {
        self.previous_truth = false;
        self.spent = false;
        self.needs_false = false;
        self.next_allowed_at_ms = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sub(fire: Fire, rearm: RearmMode) -> TriggerSubscriber {
        TriggerSubscriber::new(fire, rearm, None, false)
    }

    #[test]
    fn if_true_immediate_fires_every_true() {
        let mut s = sub(Fire::IfTrue, RearmMode::Immediate);
        assert!(s.on_truth(true, 0));
        assert!(s.on_truth(true, 1));
        assert!(!s.on_truth(false, 2));
        assert!(s.on_truth(true, 3));
    }

    #[test]
    fn on_crossing_fresh_crossing_needs_a_false_between_fires() {
        let mut s = sub(Fire::OnCrossing, RearmMode::FreshCrossing);
        assert!(s.on_truth(true, 0));
        assert!(!s.on_truth(true, 1), "no second crossing while true");
        assert!(!s.on_truth(false, 2));
        assert!(s.on_truth(true, 3));
    }

    #[test]
    fn if_true_fresh_crossing_fires_once_per_true_run() {
        let mut s = sub(Fire::IfTrue, RearmMode::FreshCrossing);
        assert!(s.on_truth(true, 0));
        assert!(!s.on_truth(true, 1));
        assert!(!s.on_truth(false, 2));
        assert!(s.on_truth(true, 3));
    }

    #[test]
    fn never_is_spent_after_one_fire_even_when_masked() {
        // The mask is applied by the caller; the machine itself does not know.
        let mut s = sub(Fire::IfTrue, RearmMode::Never);
        assert!(s.on_truth(true, 0));
        assert!(!s.on_truth(true, 1));
        assert!(!s.on_truth(false, 2));
        assert!(!s.on_truth(true, 3));
        s.reset();
        assert!(s.on_truth(true, 4));
    }

    #[test]
    fn already_true_at_registration_is_not_a_crossing() {
        let mut s = TriggerSubscriber::new(Fire::OnCrossing, RearmMode::Immediate, None, true);
        assert!(!s.on_truth(true, 0));
        assert!(!s.on_truth(false, 1));
        assert!(s.on_truth(true, 2));
    }

    #[test]
    fn cooldown_suppresses_but_still_advances() {
        let mut s = TriggerSubscriber::new(Fire::OnCrossing, RearmMode::FreshCrossing, Some(1000), false);
        assert!(s.on_truth(true, 0));
        assert!(!s.on_truth(false, 100));
        // A fresh crossing inside the cooldown is suppressed, and it consumed
        // the fresh-crossing rearm: the run of trues that follows cannot fire.
        assert!(!s.on_truth(true, 500));
        assert!(s.needs_false);
        assert!(!s.on_truth(true, 1500));
        assert!(!s.on_truth(false, 1600));
        assert!(s.on_truth(true, 1700));
    }

    #[test]
    fn timer_ticks_respect_never_and_cooldown() {
        let mut once = sub(Fire::OnCrossing, RearmMode::Never);
        assert!(once.on_timer(0));
        assert!(!once.on_timer(1));
        let mut every = TriggerSubscriber::new(Fire::OnCrossing, RearmMode::Immediate, Some(2000), false);
        assert!(every.on_timer(0));
        assert!(!every.on_timer(1000));
        assert!(every.on_timer(2000));
    }
}
