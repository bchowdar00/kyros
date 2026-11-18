//! One deployment's connection to the platform: its subscriptions, trigger
//! registrations, delivery mask, params, armed timers, and the frame channel
//! the runtime reads.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use strategy_compiler::program::{TriggerDef, TriggerSource};
use tokio::sync::mpsc;

use crate::automation::value::Value;

use super::ConnId;
use super::row::RowKey;
use super::subscriber::TriggerSubscriber;

/// What a connection receives. Only activations: a subscribed signal that
/// publishes without firing an armed trigger is not delivered (the next
/// activation carries its latest snapshot anyway).
#[derive(Debug, Clone, PartialEq)]
pub enum Frame {
    /// One or more armed triggers fired on one event (or one timer tick).
    Activation(Activation),
}

impl Frame {
    /// The session the frame belongs to; the runtime drops frames from a
    /// session older than its own after a reset.
    pub fn session(&self) -> u64 {
        match self {
            Frame::Activation(a) => a.session,
        }
    }
}

/// Coalesced fires for one connection from one event.
#[derive(Debug, Clone, PartialEq)]
pub struct Activation {
    pub session: u64,
    pub fired: BTreeSet<String>,
    /// The publishing event's typed fields; `None` for timers and param changes.
    pub event: Option<BTreeMap<String, Value>>,
    pub event_signal: Option<String>,
    /// Latest snapshot of every subscribed signal (fields present only once
    /// published).
    pub snapshots: BTreeMap<String, BTreeMap<String, Value>>,
    pub ts_ms: u64,
}

/// A registered trigger.
#[derive(Debug)]
pub enum TriggerEntry {
    Condition { row: RowKey, subscriber: TriggerSubscriber },
    Timer { source: TriggerSource, subscriber: TriggerSubscriber, armed: Option<ArmedTimer> },
}

impl TriggerEntry {
    pub fn subscriber_mut(&mut self) -> &mut TriggerSubscriber {
        match self {
            TriggerEntry::Condition { subscriber, .. } | TriggerEntry::Timer { subscriber, .. } => subscriber,
        }
    }
}

/// One live arming of a timer.
#[derive(Debug)]
pub struct ArmedTimer {
    pub generation: u64,
    pub cancelled: Arc<AtomicBool>,
}

impl ArmedTimer {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

#[derive(Debug)]
pub struct Connection {
    pub id: ConnId,
    /// Deployment-supplied label for stats (the prototype's `strategy_owner_id`).
    pub owner: Option<String>,
    pub subscriptions: BTreeSet<String>,
    pub triggers: BTreeMap<String, TriggerEntry>,
    pub mask: BTreeSet<String>,
    pub params: BTreeMap<String, Value>,
    pub session: u64,
    pub closed: bool,
    pub(super) tx: mpsc::UnboundedSender<Frame>,
    pub(super) next_timer_generation: u64,
}

impl Connection {
    pub(super) fn new(id: ConnId, owner: Option<String>, tx: mpsc::UnboundedSender<Frame>) -> Self {
        Connection {
            id,
            owner,
            subscriptions: BTreeSet::new(),
            triggers: BTreeMap::new(),
            mask: BTreeSet::new(),
            params: BTreeMap::new(),
            session: 0,
            closed: false,
            tx,
            next_timer_generation: 0,
        }
    }

    pub(super) fn send(&mut self, frame: Frame) {
        if self.closed {
            return;
        }
        if self.tx.send(frame).is_err() {
            self.closed = true;
        }
    }

    pub fn timer_names(&self) -> impl Iterator<Item = &str> {
        self.triggers
            .iter()
            .filter(|(_, e)| matches!(e, TriggerEntry::Timer { .. }))
            .map(|(n, _)| n.as_str())
    }

    /// The trigger definition's source, for stats and re-arming.
    pub fn timer_source(&self, name: &str) -> Option<&TriggerSource> {
        match self.triggers.get(name) {
            Some(TriggerEntry::Timer { source, .. }) => Some(source),
            _ => None,
        }
    }

    /// Cancel the live arming of a timer, if any.
    pub(super) fn disarm_timer(&mut self, name: &str) {
        if let Some(TriggerEntry::Timer { armed, .. }) = self.triggers.get_mut(name)
            && let Some(a) = armed.take()
        {
            a.cancel();
        }
    }

    pub(super) fn subscriber_for(def: &TriggerDef, previous_truth: bool) -> TriggerSubscriber {
        TriggerSubscriber::new(def.fire, def.rearm, def.cooldown_ms, previous_truth)
    }
}
