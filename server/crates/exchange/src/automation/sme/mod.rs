//! The Signals Matching Engine.
//!
//! [`SignalsPlatform`] is the synchronous core: the signal store, the
//! deduplicated condition rows and their dependency index, every connection
//! with its subscriber state machines, masks, params and timers. It is owned
//! by exactly one task ([`task::run_platform`]) in production so that
//! evaluation is synchronous with ingest and every row sees a consistent
//! snapshot; tests drive the core directly.
//!
//! Direct port of the prototype's `signals/platform.py`, with IR v0.3's
//! params, `after`/`interval` timers and cooldowns added.

pub mod connection;
pub mod row;
pub mod subscriber;
pub mod task;
pub mod timers;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use strategy_compiler::ir::Registry;
use strategy_compiler::program::TriggerDef;
use tokio::sync::mpsc;

use super::signals::{IngestError, SignalEvent, SignalStore};
use super::value::Value;
pub use connection::{Activation, Connection, Frame, TriggerEntry};
use connection::ArmedTimer;
use row::{ConditionRow, Dependency, RowKey};
pub use timers::{NoWindows, RecordingSink, TimerRequest, TimerSink, VenueClock};

/// A connection id, allocated by the platform.
pub type ConnId = u64;

/// What a deployment supplies when it connects.
#[derive(Debug)]
pub struct ConnectSpec {
    pub owner: Option<String>,
    pub subscriptions: Vec<String>,
    pub triggers: Vec<TriggerDef>,
    pub initial_mask: Vec<String>,
    pub params: BTreeMap<String, Value>,
    pub tx: mpsc::UnboundedSender<Frame>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectError {
    UnknownSignal(String),
    DuplicateTrigger(String),
    /// A trigger reads a signal the connection did not subscribe to.
    UnsubscribedSignal { trigger: String, signal_id: String },
    UnknownMaskTrigger(String),
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectError::UnknownSignal(s) => write!(f, "unknown_signal: {s}"),
            ConnectError::DuplicateTrigger(t) => write!(f, "trigger id re-used: {t}"),
            ConnectError::UnsubscribedSignal { trigger, signal_id } => {
                write!(f, "trigger `{trigger}` references unsubscribed signal `{signal_id}`")
            }
            ConnectError::UnknownMaskTrigger(t) => write!(f, "unknown active trigger: {t}"),
        }
    }
}

impl std::error::Error for ConnectError {}

/// Per-row statistics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowStats {
    pub predicate: String,
    pub logical_registrations: usize,
    pub strategies: usize,
    pub evaluations: u64,
}

/// SME-wide statistics.
#[derive(Debug, Clone, PartialEq)]
pub struct Stats {
    pub logical_registrations: usize,
    pub unique_rows: usize,
    pub dedup_ratio: f64,
    pub timers: usize,
    pub subscriber_strategies: usize,
    pub rows: Vec<RowStats>,
}

struct RowEntry {
    row: ConditionRow,
    /// `(connection, trigger name)` pairs registered on this row.
    subscribers: BTreeSet<(ConnId, String)>,
}

pub struct SignalsPlatform {
    store: SignalStore,
    rows: HashMap<RowKey, RowEntry>,
    /// Rows to re-evaluate when a field (or a whole signal) publishes.
    index: HashMap<Dependency, HashSet<RowKey>>,
    /// Every row that reads a signal at all, for snapshot clears.
    by_signal: HashMap<String, HashSet<RowKey>>,
    connections: BTreeMap<ConnId, Connection>,
    next_conn: ConnId,
    clock: Arc<dyn VenueClock>,
    timers: Box<dyn TimerSink>,
}

impl SignalsPlatform {
    pub fn new(store: SignalStore, clock: Arc<dyn VenueClock>, timers: Box<dyn TimerSink>) -> Self {
        SignalsPlatform { store, rows: HashMap::new(), index: HashMap::new(), by_signal: HashMap::new(), connections: BTreeMap::new(), next_conn: 1, clock, timers }
    }

    /// Catalogue from a registry, every signal disabled.
    pub fn from_registry(registry: &Registry, clock: Arc<dyn VenueClock>, timers: Box<dyn TimerSink>) -> Self {
        Self::new(SignalStore::from_registry(registry), clock, timers)
    }

    pub fn store(&self) -> &SignalStore {
        &self.store
    }

    pub fn store_mut(&mut self) -> &mut SignalStore {
        &mut self.store
    }

    pub fn enable(&mut self, signal_id: &str) -> bool {
        self.store.enable(signal_id)
    }

    pub fn connection(&self, id: ConnId) -> Option<&Connection> {
        self.connections.get(&id)
    }

    // ───────────────────────────── ingest ─────────────────────────────

    /// Atomically record one whole event and evaluate every affected row.
    /// A connection hears about the event only through an activation: the
    /// runtime has no use for a bare update, and sending one per subscriber
    /// per frame was the hot path's largest cost. Unknown or disabled ids
    /// are a no-op reported as `Err` for the caller to log or ignore.
    pub fn ingest(&mut self, event: &SignalEvent) -> Result<(), IngestError> {
        let typed = self.store.ingest(event)?;
        let mut affected: HashSet<RowKey> = HashSet::new();
        if let Some(keys) = self.index.get(&Dependency { signal_id: event.signal_id.clone(), field: None }) {
            affected.extend(keys.iter().cloned());
        }
        for field in typed.keys() {
            if let Some(keys) = self.index.get(&Dependency { signal_id: event.signal_id.clone(), field: Some(field.clone()) }) {
                affected.extend(keys.iter().cloned());
            }
        }
        self.evaluate_rows(affected, Some((&event.signal_id, &typed)), event.ts_ms);
        Ok(())
    }

    /// A signal is no longer trustworthy (its publisher reported it
    /// warming up, degraded or unavailable): forget its snapshot and
    /// re-evaluate every row that reads it, so `always` leaves and field
    /// reads are false until the next ready event. Subscriber bookkeeping
    /// advances as on any evaluation, so a trigger that is true again
    /// afterwards is a fresh crossing. A signal that never published is a
    /// no-op.
    pub fn clear_snapshot(&mut self, signal_id: &str) {
        if !self.store.clear(signal_id) {
            return;
        }
        let affected: HashSet<RowKey> = self.by_signal.get(signal_id).cloned().unwrap_or_default();
        let ts_ms = self.clock.now_ms();
        self.evaluate_rows(affected, None, ts_ms);
    }

    /// Evaluate a set of rows once each, advance every subscriber, and deliver
    /// one coalesced activation per connection with in-mask fires.
    fn evaluate_rows(&mut self, keys: HashSet<RowKey>, event: Option<(&str, &BTreeMap<String, Value>)>, ts_ms: u64) {
        // Deterministic order across connections.
        let mut keys: Vec<RowKey> = keys.into_iter().collect();
        keys.sort();
        let mut deliveries: BTreeMap<ConnId, BTreeSet<String>> = BTreeMap::new();
        for key in keys {
            let Some(entry) = self.rows.get_mut(&key) else { continue };
            // Param rows are per connection; shared rows use empty params.
            let params: BTreeMap<String, Value> = match key.conn {
                Some(c) => self.connections.get(&c).map(|c| c.params.clone()).unwrap_or_default(),
                None => BTreeMap::new(),
            };
            let truth = entry.row.evaluate(&self.store, &params);
            entry.row.evaluations += 1;
            for (conn_id, name) in &entry.subscribers {
                let Some(conn) = self.connections.get_mut(conn_id) else { continue };
                let Some(TriggerEntry::Condition { subscriber, .. }) = conn.triggers.get_mut(name) else { continue };
                let fires = subscriber.on_truth(truth, ts_ms);
                if fires && conn.mask.contains(name) && !conn.closed {
                    deliveries.entry(*conn_id).or_default().insert(name.clone());
                }
            }
        }
        for (conn_id, fired) in deliveries {
            self.enqueue_activation(conn_id, fired, event, ts_ms);
        }
    }

    fn enqueue_activation(&mut self, conn_id: ConnId, fired: BTreeSet<String>, event: Option<(&str, &BTreeMap<String, Value>)>, ts_ms: u64) {
        let Some(conn) = self.connections.get_mut(&conn_id) else { return };
        let snapshots = conn
            .subscriptions
            .iter()
            .map(|sid| {
                let fields = self
                    .store
                    .snapshot(sid)
                    .map(|s| s.iter().map(|(f, r)| (f.clone(), r.value.clone())).collect())
                    .unwrap_or_default();
                (sid.clone(), fields)
            })
            .collect();
        let activation = Activation {
            session: conn.session,
            fired,
            event: event.map(|(_, f)| f.clone()),
            event_signal: event.map(|(s, _)| s.to_string()),
            snapshots,
            ts_ms,
        };
        conn.send(Frame::Activation(activation));
    }

    // ─────────────────────────── connections ───────────────────────────

    pub fn connect(&mut self, spec: ConnectSpec) -> Result<ConnId, ConnectError> {
        for sid in &spec.subscriptions {
            if !self.store.contains(sid) {
                return Err(ConnectError::UnknownSignal(sid.clone()));
            }
        }
        let id = self.next_conn;
        let mut conn = Connection::new(id, spec.owner, spec.tx);
        conn.subscriptions = spec.subscriptions.iter().cloned().collect();
        conn.params = spec.params;
        // Register rows first so a failure leaves nothing behind.
        let mut registered: Vec<(RowKey, String)> = Vec::new();
        let mut result = Ok(());
        for def in &spec.triggers {
            if conn.triggers.contains_key(&def.name) {
                result = Err(ConnectError::DuplicateTrigger(def.name.clone()));
                break;
            }
            match ConditionRow::from_source(&def.source, id) {
                Some(row) => {
                    if let Some(dep) = row.dependencies.iter().find(|d| !conn.subscriptions.contains(&d.signal_id)) {
                        result = Err(ConnectError::UnsubscribedSignal { trigger: def.name.clone(), signal_id: dep.signal_id.clone() });
                        break;
                    }
                    let key = row.key.clone();
                    let previous_truth = self.register_row(row, id, &def.name, &conn.params);
                    registered.push((key.clone(), def.name.clone()));
                    conn.triggers.insert(def.name.clone(), TriggerEntry::Condition { row: key, subscriber: Connection::subscriber_for(def, previous_truth) });
                }
                None => {
                    conn.triggers.insert(
                        def.name.clone(),
                        TriggerEntry::Timer { source: def.source.clone(), subscriber: Connection::subscriber_for(def, false), armed: None },
                    );
                }
            }
        }
        if result.is_ok() {
            for name in &spec.initial_mask {
                if !conn.triggers.contains_key(name) {
                    result = Err(ConnectError::UnknownMaskTrigger(name.clone()));
                    break;
                }
            }
        }
        if let Err(e) = result {
            for (key, name) in registered {
                self.unregister_row(&key, id, &name);
            }
            return Err(e);
        }
        self.next_conn += 1;
        self.connections.insert(id, conn);
        self.set_mask(id, spec.initial_mask.iter().cloned().collect());
        Ok(id)
    }

    /// Insert or share a row; returns its current truth for the subscriber's
    /// `previous_truth`.
    fn register_row(&mut self, row: ConditionRow, conn: ConnId, name: &str, params: &BTreeMap<String, Value>) -> bool {
        let key = row.key.clone();
        let entry = self.rows.entry(key.clone()).or_insert_with(|| {
            for dep in &row.dependencies {
                self.index.entry(dep.clone()).or_default().insert(key.clone());
                self.by_signal.entry(dep.signal_id.clone()).or_default().insert(key.clone());
            }
            RowEntry { row, subscribers: BTreeSet::new() }
        });
        entry.subscribers.insert((conn, name.to_string()));
        entry.row.evaluate(&self.store, params)
    }

    fn unregister_row(&mut self, key: &RowKey, conn: ConnId, name: &str) {
        let Some(entry) = self.rows.get_mut(key) else { return };
        entry.subscribers.remove(&(conn, name.to_string()));
        if !entry.subscribers.is_empty() {
            return;
        }
        let entry = self.rows.remove(key).expect("present");
        for dep in &entry.row.dependencies {
            if let Some(keys) = self.index.get_mut(dep) {
                keys.remove(key);
                if keys.is_empty() {
                    self.index.remove(dep);
                }
            }
            if let Some(keys) = self.by_signal.get_mut(&dep.signal_id) {
                keys.remove(key);
                if keys.is_empty() {
                    self.by_signal.remove(&dep.signal_id);
                }
            }
        }
    }

    /// The rows that read `signal_id`, for tests of the index.
    #[cfg(test)]
    pub(crate) fn rows_reading(&self, signal_id: &str) -> usize {
        self.by_signal.get(signal_id).map_or(0, HashSet::len)
    }

    /// Replace the delivery mask. Condition triggers keep their bookkeeping;
    /// every timer in the old mask is cancelled and every timer in the new
    /// mask is freshly armed, even one present in both (each mask change is a
    /// leg boundary).
    pub fn set_mask(&mut self, conn_id: ConnId, names: BTreeSet<String>) {
        let Some(conn) = self.connections.get_mut(&conn_id) else { return };
        let names: BTreeSet<String> = names.into_iter().filter(|n| conn.triggers.contains_key(n)).collect();
        let old_timers: Vec<String> = conn.mask.iter().filter(|n| conn.timer_source(n).is_some()).cloned().collect();
        for name in old_timers {
            conn.disarm_timer(&name);
        }
        conn.mask = names;
        let new_timers: Vec<String> = conn.mask.iter().filter(|n| conn.timer_source(n).is_some()).cloned().collect();
        for name in new_timers {
            self.arm_timer(conn_id, &name);
        }
    }

    fn arm_timer(&mut self, conn_id: ConnId, name: &str) {
        let Some(conn) = self.connections.get_mut(&conn_id) else { return };
        let Some(source) = conn.timer_source(name).cloned() else { return };
        conn.disarm_timer(name);
        let Some((delay_ms, repeat_ms)) = timers::resolve(&source, self.clock.as_ref()) else {
            // No open window yet: logically armed, measured on WindowOpened.
            return;
        };
        conn.next_timer_generation += 1;
        let generation = conn.next_timer_generation;
        let cancelled = Arc::new(AtomicBool::new(false));
        if let Some(TriggerEntry::Timer { armed, .. }) = conn.triggers.get_mut(name) {
            *armed = Some(ArmedTimer { generation, cancelled: cancelled.clone() });
        }
        self.timers.schedule(TimerRequest { conn: conn_id, name: name.to_string(), generation, delay_ms, repeat_ms, cancelled });
    }

    /// A scheduled timer fired. Delivered only if the arming is current and
    /// the name is still in the mask.
    pub fn timer_fired(&mut self, conn_id: ConnId, name: &str, generation: u64) {
        let ts_ms = self.clock.now_ms();
        let Some(conn) = self.connections.get_mut(&conn_id) else { return };
        if conn.closed || !conn.mask.contains(name) {
            return;
        }
        let Some(TriggerEntry::Timer { subscriber, armed, .. }) = conn.triggers.get_mut(name) else { return };
        if armed.as_ref().map(|a| a.generation) != Some(generation) {
            return;
        }
        if !subscriber.on_timer(ts_ms) {
            return;
        }
        let fired = [name.to_string()].into_iter().collect();
        self.enqueue_activation(conn_id, fired, None, ts_ms);
    }

    /// A window opened for `instrument_ids` (the concrete market id and its
    /// `<series>.current` alias): re-measure every armed `window_clock`
    /// timer that names one of them.
    pub fn window_opened(&mut self, instrument_ids: &[&str]) {
        let targets: Vec<(ConnId, String)> = self
            .connections
            .values()
            .flat_map(|c| {
                c.mask
                    .iter()
                    .filter(|n| c.timer_source(n).and_then(timers::window_instrument).is_some_and(|i| instrument_ids.contains(&i)))
                    .map(|n| (c.id, n.clone()))
                    .collect::<Vec<_>>()
            })
            .collect();
        for (conn, name) in targets {
            self.arm_timer(conn, &name);
        }
    }

    /// Settlement boundary: empty mask, every subscriber rewound, timers
    /// cancelled, and a new session so the runtime can drop queued frames.
    pub fn reset_session(&mut self, conn_id: ConnId) {
        self.set_mask(conn_id, BTreeSet::new());
        let Some(conn) = self.connections.get_mut(&conn_id) else { return };
        conn.session += 1;
        for entry in conn.triggers.values_mut() {
            entry.subscriber_mut().reset();
        }
    }

    /// Replace the connection's params and re-evaluate its param-dependent
    /// rows (fires as an activation with no event).
    pub fn set_params(&mut self, conn_id: ConnId, params: BTreeMap<String, Value>) {
        let Some(conn) = self.connections.get_mut(&conn_id) else { return };
        conn.params = params;
        let keys: HashSet<RowKey> = conn
            .triggers
            .values()
            .filter_map(|e| match e {
                TriggerEntry::Condition { row, .. } if row.conn == Some(conn_id) => Some(row.clone()),
                _ => None,
            })
            .collect();
        let ts_ms = self.clock.now_ms();
        self.evaluate_rows(keys, None, ts_ms);
    }

    pub fn disconnect(&mut self, conn_id: ConnId) {
        let Some(mut conn) = self.connections.remove(&conn_id) else { return };
        conn.closed = true;
        let names: Vec<String> = conn.triggers.keys().cloned().collect();
        for name in names {
            conn.disarm_timer(&name);
            if let Some(TriggerEntry::Condition { row, .. }) = conn.triggers.get(&name) {
                let row = row.clone();
                self.unregister_row(&row, conn_id, &name);
            }
        }
    }

    // ────────────────────────────── stats ──────────────────────────────

    pub fn stats(&self) -> Stats {
        let logical: usize = self.rows.values().map(|r| r.subscribers.len()).sum();
        let unique = self.rows.len();
        let timers = self.connections.values().map(|c| c.timer_names().count()).sum();
        let owner = |conn: &ConnId| self.connections.get(conn).and_then(|c| c.owner.clone());
        let strategies: HashSet<String> = self.rows.values().flat_map(|r| r.subscribers.iter().filter_map(|(c, _)| owner(c))).collect();
        let mut rows: Vec<RowStats> = self
            .rows
            .values()
            .map(|r| RowStats {
                predicate: r.row.key.canonical.clone(),
                logical_registrations: r.subscribers.len(),
                strategies: r.subscribers.iter().filter_map(|(c, _)| owner(c)).collect::<HashSet<_>>().len(),
                evaluations: r.row.evaluations,
            })
            .collect();
        rows.sort_by(|a, b| a.predicate.cmp(&b.predicate));
        Stats {
            logical_registrations: logical,
            unique_rows: unique,
            dedup_ratio: if unique == 0 { 1.0 } else { logical as f64 / unique as f64 },
            timers,
            subscriber_strategies: strategies.len(),
            rows,
        }
    }
}

#[cfg(test)]
mod tests;
