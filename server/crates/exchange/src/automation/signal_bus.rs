//! The in-process signal bus: one path every signal publisher takes.
//!
//! A publisher (the spot, series and stats publishers, the lane bridge,
//! anything future) hands the bus a [`SignalFrame`]: a registry signal id,
//! a timestamp, a quality state and the field values as JSON. The bus then
//! does two things with it, in this order:
//!
//! 1. **The fast path.** Types the values against the registry and hands a
//!    typed `SignalEvent` to the signals platform, so strategies can
//!    trigger on it. This happens first and is one unbounded channel send;
//!    nothing the frontend needs sits in front of it.
//! 2. **The fan-out.** Keeps the frame as the signal's latest value (for
//!    the catalogue endpoint and new subscribers), appends a value-carrying
//!    frame to a bounded ring of recent frames per signal (so a chart can
//!    open with history rather than accumulate it from the moment it
//!    subscribed), and broadcasts it to every live subscriber
//!    (`/ws/signals`). Subscribers coalesce and resnapshot on lag, so
//!    falling behind here never costs a strategy.
//!
//! A frame the registry cannot type (a missing or malformed field) is
//! counted and reported per signal ([`SignalBus::ingest_stats`]), warned
//! about at most once a minute per signal, and still kept and fanned out;
//! it is only skipped for the platform.
//!
//! Values keep the wire convention on the way out: decimals and
//! timestamps are strings, integers are numbers, flags are booleans.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use strategy_compiler::Registry;
use strategy_compiler::ir::FieldType;
use strategy_compiler::ir::registry::SignalDef;
use tokio::sync::broadcast;

use crate::automation::signals::{SignalEvent, SignalValue};
use crate::automation::sme::task::PlatformHandle;

/// Capacity of the broadcast channel behind `/ws/signals`. Subscribers
/// coalesce, so a lagging one only loses intermediate frames.
pub const BUS_BROADCAST_CAP: usize = 4096;

/// How often a signal whose frames keep failing to type is warned about.
const REJECT_WARN_INTERVAL: Duration = Duration::from_secs(60);

/// How far back the bus keeps frames per signal: the longest series
/// (15 minutes) plus its pre-open run-up, with margin, so a chart of a
/// whole round never asks for more than the ring holds.
pub const HISTORY_WINDOW_MS: u64 = 20 * 60 * 1000;

/// Hard cap on frames kept per signal. The spot feed is the chattiest
/// publisher at one coalesced frame per 200 ms, which is 6,000 in the
/// window; lane signals publish on change and stay far below it.
pub const HISTORY_CAP: usize = 6_000;

/// A signal's readiness, as the lanes report it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Quality {
    /// `ready`, `warming_up`, `degraded`, `unavailable`, …
    pub state: String,
    #[serde(default)]
    pub reasons: Vec<String>,
}

impl Quality {
    pub fn ready() -> Self {
        Self { state: "ready".into(), reasons: Vec::new() }
    }
}

/// One published value set for one signal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SignalFrame {
    pub signal_id: String,
    pub ts_ms: u64,
    pub quality: Quality,
    /// Field name → JSON value. Empty for a quality-only update.
    pub values: BTreeMap<String, Json>,
}

/// How a signal's frames have fared at the registry typing step.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct IngestStats {
    /// Frames typed and handed to the platform.
    pub published: u64,
    /// Frames the registry could not type (kept and broadcast, not ingested).
    pub rejected: u64,
    /// Why the most recent rejected frame was refused.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_reject: Option<String>,
}

#[derive(Debug, Default)]
struct Counters {
    stats: IngestStats,
    last_warned: Option<Instant>,
}

/// A signal's declared fields with their names interned for the process.
/// `SignalEvent` wants `&'static str` names; interning once per registry
/// field (bounded by the registry size) keeps the publish path
/// allocation-free for names.
pub type FieldTypes = Vec<(&'static str, FieldType)>;

/// The interned field declarations of one registry signal.
pub fn field_types(def: &SignalDef) -> FieldTypes {
    def.fields
        .iter()
        .map(|f| {
            let name: &'static str = Box::leak(f.name.as_str().to_string().into_boxed_str());
            (name, f.field_type.clone())
        })
        .collect()
}

pub struct SignalBus {
    registry: Arc<Registry>,
    /// Field declarations per registry signal id, for the platform hand-off.
    types: HashMap<String, FieldTypes>,
    latest: RwLock<HashMap<String, Arc<SignalFrame>>>,
    /// Recent value-carrying frames per signal, oldest first, sharing the
    /// same `Arc`s as `latest` and the broadcast so nothing is copied.
    history: RwLock<HashMap<String, VecDeque<Arc<SignalFrame>>>>,
    tx: broadcast::Sender<Arc<SignalFrame>>,
    platform: Option<PlatformHandle>,
    enabled: Mutex<HashSet<String>>,
    counters: Mutex<HashMap<String, Counters>>,
    /// When each signal's quality last became `ready` (the `ts_ms` of
    /// that frame); absent while it is anything else. What the catalogue
    /// reports as `ready_since_ms`, so a card can say how long a
    /// warming-up signal has been unusable, or how fresh a ready one is.
    ready_since: Mutex<HashMap<String, u64>>,
}

impl SignalBus {
    pub fn new(registry: Arc<Registry>, platform: Option<PlatformHandle>) -> Arc<Self> {
        let types = registry.signals.iter().map(|s| (s.signal_id.clone(), field_types(s))).collect();
        let (tx, _) = broadcast::channel(BUS_BROADCAST_CAP);
        Arc::new(Self {
            registry,
            types,
            latest: RwLock::new(HashMap::new()),
            history: RwLock::new(HashMap::new()),
            tx,
            platform,
            enabled: Mutex::new(HashSet::new()),
            counters: Mutex::new(HashMap::new()),
            ready_since: Mutex::new(HashMap::new()),
        })
    }

    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    pub fn knows(&self, signal_id: &str) -> bool {
        self.types.contains_key(signal_id)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Arc<SignalFrame>> {
        self.tx.subscribe()
    }

    pub fn latest(&self, signal_id: &str) -> Option<Arc<SignalFrame>> {
        self.latest.read().expect("bus latest poisoned").get(signal_id).cloned()
    }

    /// When the signal's quality last became `ready`; `None` while it is
    /// not ready or has never published.
    pub fn ready_since(&self, signal_id: &str) -> Option<u64> {
        self.ready_since.lock().expect("bus ready poisoned").get(signal_id).copied()
    }

    /// Track the ready transition: the first ready frame after a
    /// not-ready one (or the first ever) stamps `ready_since`; a not-ready
    /// frame clears it.
    fn note_quality(&self, frame: &SignalFrame) {
        let mut ready = self.ready_since.lock().expect("bus ready poisoned");
        if frame.quality.state == "ready" {
            ready.entry(frame.signal_id.clone()).or_insert(frame.ts_ms);
        } else {
            ready.remove(&frame.signal_id);
        }
    }

    /// The typing counters of a signal; zeros until it first publishes.
    pub fn ingest_stats(&self, signal_id: &str) -> IngestStats {
        self.counters
            .lock()
            .expect("bus counters poisoned")
            .get(signal_id)
            .map(|c| c.stats.clone())
            .unwrap_or_default()
    }

    /// The frames of `signal_id` published at or after `since_ms`, oldest
    /// first, evenly thinned to at most `max_points` (0 means no limit).
    /// Bounded by [`HISTORY_WINDOW_MS`] and [`HISTORY_CAP`] regardless.
    pub fn history(&self, signal_id: &str, since_ms: u64, max_points: usize) -> Vec<Arc<SignalFrame>> {
        let guard = self.history.read().expect("bus history poisoned");
        let Some(ring) = guard.get(signal_id) else {
            return Vec::new();
        };
        let start = ring.partition_point(|f| f.ts_ms < since_ms);
        let frames: Vec<Arc<SignalFrame>> = ring.range(start..).cloned().collect();
        drop(guard);
        crate::pricing::feed::thin_evenly(frames, max_points)
    }

    /// Frames kept for `signal_id`, for tests and metrics.
    pub fn history_len(&self, signal_id: &str) -> usize {
        self.history.read().expect("bus history poisoned").get(signal_id).map_or(0, VecDeque::len)
    }

    /// Append a value-carrying frame to its signal's ring and evict what
    /// falls outside the window or the cap. Frames arrive in publish
    /// order; a clock that steps backwards is kept in arrival order so a
    /// chart still sees it, and the window is measured from the newest.
    fn retain(&self, frame: &Arc<SignalFrame>) {
        let mut guard = self.history.write().expect("bus history poisoned");
        let ring = guard.entry(frame.signal_id.clone()).or_default();
        ring.push_back(frame.clone());
        let oldest_kept = frame.ts_ms.saturating_sub(HISTORY_WINDOW_MS);
        while ring.len() > HISTORY_CAP || ring.front().is_some_and(|f| f.ts_ms < oldest_kept) {
            ring.pop_front();
        }
    }

    /// Publish a frame: platform first, then the latest store, the history
    /// ring and the broadcast. Unknown signal ids are dropped (debug log).
    pub fn publish(&self, frame: SignalFrame) {
        let Some(types) = self.types.get(&frame.signal_id) else {
            tracing::debug!(signal = %frame.signal_id, "signal bus: unknown signal dropped");
            return;
        };
        let frame = Arc::new(frame);
        self.note_quality(&frame);
        // A quality-only frame restates old values; it is not a sample for
        // the platform or the history ring.
        if !frame.values.is_empty() {
            self.ingest(&frame, types);
        }
        self.latest
            .write()
            .expect("bus latest poisoned")
            .insert(frame.signal_id.clone(), frame.clone());
        if !frame.values.is_empty() {
            self.retain(&frame);
        }
        let _ = self.tx.send(frame);
    }

    /// The fast path: type the frame and hand it to the platform, counting
    /// the outcome either way.
    fn ingest(&self, frame: &SignalFrame, types: &FieldTypes) {
        let typed = to_signal_event(frame, types);
        let mut counters = self.counters.lock().expect("bus counters poisoned");
        let c = counters.entry(frame.signal_id.clone()).or_default();
        match typed {
            Ok(event) => {
                c.stats.published += 1;
                drop(counters);
                if let Some(platform) = &self.platform {
                    let first = self.enabled.lock().expect("enabled poisoned").insert(frame.signal_id.clone());
                    if first {
                        platform.enable(&frame.signal_id);
                    }
                    platform.ingest(event);
                }
            }
            Err(reason) => {
                c.stats.rejected += 1;
                c.stats.last_reject = Some(reason.clone());
                let warn = c.last_warned.is_none_or(|t| t.elapsed() >= REJECT_WARN_INTERVAL);
                if warn {
                    c.last_warned = Some(Instant::now());
                }
                let rejected = c.stats.rejected;
                drop(counters);
                if warn {
                    tracing::warn!(signal = %frame.signal_id, rejected, %reason, "signal bus: frame does not type against the registry; not ingested");
                } else {
                    tracing::debug!(signal = %frame.signal_id, %reason, "signal bus: frame not typed for the platform");
                }
            }
        }
    }

    /// Update only the quality of a signal, keeping its last values. Fan-out
    /// only; the platform learns about readiness from the bridge.
    pub fn set_quality(&self, signal_id: &str, ts_ms: u64, quality: Quality) {
        if !self.knows(signal_id) {
            return;
        }
        let values = self.latest(signal_id).map(|f| f.values.clone()).unwrap_or_default();
        let frame = Arc::new(SignalFrame { signal_id: signal_id.to_string(), ts_ms, quality, values });
        self.note_quality(&frame);
        self.latest
            .write()
            .expect("bus latest poisoned")
            .insert(signal_id.to_string(), frame.clone());
        let _ = self.tx.send(frame);
    }
}

/// Type a frame's JSON values against the registry's field declarations.
pub fn to_signal_event(frame: &SignalFrame, types: &FieldTypes) -> Result<SignalEvent, String> {
    let mut fields: Vec<(&'static str, SignalValue)> = Vec::with_capacity(types.len());
    for (name, ty) in types {
        let raw = frame.values.get(*name).ok_or_else(|| format!("missing field `{name}`"))?;
        let value = json_to_signal_value(ty, raw).ok_or_else(|| format!("field `{name}` is not a {ty:?}: {raw}"))?;
        fields.push((name, value));
    }
    Ok(SignalEvent::new(&frame.signal_id, frame.ts_ms, fields))
}

fn json_to_signal_value(ty: &FieldType, raw: &Json) -> Option<SignalValue> {
    match ty {
        FieldType::Decimal => match raw {
            Json::String(s) => s.parse().ok().map(SignalValue::Decimal),
            Json::Number(n) => n.to_string().parse().ok().map(SignalValue::Decimal),
            _ => None,
        },
        FieldType::U8 | FieldType::U16 | FieldType::U32 | FieldType::U64 | FieldType::DurationMs => match raw {
            Json::Number(n) => n.as_u64().map(SignalValue::UInt),
            Json::String(s) => s.parse::<u64>().ok().map(SignalValue::UInt),
            _ => None,
        },
        FieldType::I64 => match raw {
            Json::Number(n) => n.as_i64().map(|i| SignalValue::Decimal(i.into())),
            Json::String(s) => s.parse::<i64>().ok().map(|i| SignalValue::Decimal(i.into())),
            _ => None,
        },
        FieldType::Timestamp => match raw {
            Json::Number(n) => n.as_u64().map(SignalValue::UInt),
            Json::String(s) => rfc3339_to_ms(s).map(SignalValue::UInt),
            _ => None,
        },
        FieldType::Bool => raw.as_bool().map(SignalValue::Bool),
        FieldType::Enum { .. } | FieldType::String => match raw {
            Json::String(s) => Some(SignalValue::Text(s.clone())),
            Json::Bool(b) => Some(SignalValue::Text(b.to_string())),
            Json::Number(n) => Some(SignalValue::Text(n.to_string())),
            _ => None,
        },
    }
}

/// `2026-09-06T02:54:08.885943Z` → unix milliseconds.
pub fn rfc3339_to_ms(s: &str) -> Option<u64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .and_then(|t| u64::try_from(t.timestamp_millis()).ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::automation::sme::task::PlatformCommand;
    use serde_json::json;

    fn types() -> FieldTypes {
        vec![
            ("price", FieldType::Decimal),
            ("count", FieldType::U32),
            ("at", FieldType::Timestamp),
            ("flag", FieldType::Bool),
            ("regime", FieldType::Enum { variants: vec!["a".into()] }),
        ]
    }

    fn frame(signal_id: &str, ts_ms: u64, values: &[(&str, Json)]) -> SignalFrame {
        SignalFrame {
            signal_id: signal_id.into(),
            ts_ms,
            quality: Quality::ready(),
            values: values.iter().map(|(k, v)| (k.to_string(), v.clone())).collect(),
        }
    }

    #[test]
    fn rfc3339_parses_to_millis() {
        assert_eq!(rfc3339_to_ms("1970-01-01T00:00:01.5Z"), Some(1500));
        assert_eq!(rfc3339_to_ms("nope"), None);
    }

    #[test]
    fn frame_types_against_registry_field_types() {
        let frame = frame(
            "x",
            7,
            &[
                ("price", json!("79895.235")),
                ("count", json!(12)),
                ("at", json!("1970-01-01T00:00:02Z")),
                ("flag", json!(true)),
                ("regime", json!("a")),
            ],
        );
        let ev = to_signal_event(&frame, &types()).unwrap();
        assert_eq!(ev.ts_ms, 7);
        assert!(matches!(ev.fields["count"], SignalValue::UInt(12)));
        assert!(matches!(ev.fields["at"], SignalValue::UInt(2000)));
        assert!(matches!(ev.fields["flag"], SignalValue::Bool(true)));

        let mut missing = frame.clone();
        missing.values.remove("flag");
        assert!(to_signal_event(&missing, &types()).unwrap_err().contains("missing field `flag`"));

        let mut bad = frame;
        bad.values.insert("count".into(), json!("twelve"));
        assert!(to_signal_event(&bad, &types()).is_err());
    }

    #[test]
    fn bus_keeps_latest_and_broadcasts_known_signals_only() {
        let reg = crate::registry::for_tests();
        let bus = SignalBus::new(reg, None);
        let mut rx = bus.subscribe();
        let id = crate::registry::stats_signal_id(crate::series::SERIES[0].slug);
        bus.publish(frame(&id, 1, &[]));
        assert_eq!(bus.latest(&id).unwrap().ts_ms, 1);
        assert_eq!(rx.try_recv().unwrap().signal_id, id);
        bus.publish(frame("nope.v1", 2, &[]));
        assert!(bus.latest("nope.v1").is_none());
        assert!(rx.try_recv().is_err());
        bus.set_quality(&id, 3, Quality { state: "degraded".into(), reasons: vec!["x".into()] });
        assert_eq!(bus.latest(&id).unwrap().quality.state, "degraded");
    }

    /// `ready_since` is the timestamp of the frame that made the signal
    /// ready, holds across later ready frames, clears on a not-ready
    /// quality and restamps on the next ready frame.
    #[test]
    fn ready_since_follows_the_quality_transitions() {
        let reg = crate::registry::for_tests();
        let bus = SignalBus::new(reg, None);
        let id = crate::registry::stats_signal_id(crate::series::SERIES[0].slug);
        assert_eq!(bus.ready_since(&id), None, "never published");
        bus.publish(frame(&id, 10, &[]));
        bus.publish(frame(&id, 20, &[]));
        assert_eq!(bus.ready_since(&id), Some(10), "the first ready frame, not the latest");
        bus.set_quality(&id, 30, Quality { state: "warming_up".into(), reasons: Vec::new() });
        assert_eq!(bus.ready_since(&id), None);
        bus.publish(frame(&id, 40, &[]));
        assert_eq!(bus.ready_since(&id), Some(40));
        let mut degraded = frame(&id, 50, &[]);
        degraded.quality = Quality { state: "degraded".into(), reasons: vec!["stale".into()] };
        bus.publish(degraded);
        assert_eq!(bus.ready_since(&id), None, "a published frame's own quality counts too");
        assert_eq!(bus.ready_since("nope.v1"), None);
    }

    /// The platform gets the typed event (enable on first publish, then
    /// ingest) and the fan-out gets the frame; a second publish only ingests.
    #[test]
    fn publish_reaches_the_platform_and_the_fan_out() {
        let reg = crate::registry::for_tests();
        let (platform, mut commands) = PlatformHandle::channel();
        let bus = SignalBus::new(reg, Some(platform));
        let mut rx = bus.subscribe();
        // The strike signal declares exactly one field, `price`.
        let id = format!("{}.current.strike.v1", crate::series::SERIES[0].slug);
        let f = frame(&id, 5, &[("price", json!("79845.74"))]);
        bus.publish(f.clone());
        assert!(matches!(commands.try_recv(), Ok(PlatformCommand::Enable(ref s)) if *s == id));
        match commands.try_recv() {
            Ok(PlatformCommand::Ingest(ev)) => {
                assert_eq!(ev.signal_id, id);
                assert_eq!(ev.ts_ms, 5);
                assert!(matches!(ev.fields["price"], SignalValue::Decimal(_)));
            }
            other => panic!("expected an ingest, got {:?}", other.map(|_| ())),
        }
        assert_eq!(rx.try_recv().unwrap().ts_ms, 5);
        assert_eq!(bus.ingest_stats(&id), IngestStats { published: 1, rejected: 0, last_reject: None });

        bus.publish(f);
        assert!(matches!(commands.try_recv(), Ok(PlatformCommand::Ingest(_))), "enabled only once");
        assert!(commands.try_recv().is_err());
        assert_eq!(bus.ingest_stats(&id).published, 2);
    }

    /// A frame that does not type is counted with its reason, never reaches
    /// the platform, and is still kept and broadcast for the frontend.
    #[test]
    fn untyped_frames_are_counted_and_fanned_out_but_not_ingested() {
        let reg = crate::registry::for_tests();
        let (platform, mut commands) = PlatformHandle::channel();
        let bus = SignalBus::new(reg, Some(platform));
        let mut rx = bus.subscribe();
        let id = format!("{}.current.strike.v1", crate::series::SERIES[0].slug);
        bus.publish(frame(&id, 9, &[("price", json!("not a number"))]));
        assert!(commands.try_recv().is_err(), "nothing reached the platform");
        assert_eq!(rx.try_recv().unwrap().ts_ms, 9);
        assert_eq!(bus.latest(&id).unwrap().ts_ms, 9);
        let stats = bus.ingest_stats(&id);
        assert_eq!((stats.published, stats.rejected), (0, 1));
        assert!(stats.last_reject.as_deref().unwrap().contains("field `price` is not a Decimal"), "{stats:?}");
        bus.publish(frame(&id, 10, &[]));
        assert_eq!(bus.ingest_stats(&id).rejected, 1, "a quality-only frame is not a typing failure");
        bus.publish(frame(&id, 11, &[("strike", json!("1"))]));
        assert!(bus.ingest_stats(&id).last_reject.as_deref().unwrap().contains("missing field `price`"));
        assert_eq!(bus.ingest_stats("never.v1"), IngestStats::default());
    }

    fn sample(id: &str, ts_ms: u64) -> SignalFrame {
        SignalFrame {
            signal_id: id.to_string(),
            ts_ms,
            quality: Quality::ready(),
            values: [("trades".to_string(), json!(ts_ms))].into_iter().collect(),
        }
    }

    #[test]
    fn history_keeps_value_frames_oldest_first_and_shares_the_latest_arc() {
        let bus = SignalBus::new(crate::registry::for_tests(), None);
        let id = crate::registry::stats_signal_id(crate::series::SERIES[0].slug);
        assert!(bus.history(&id, 0, 0).is_empty());
        for ts in [10, 20, 30] {
            bus.publish(sample(&id, ts));
        }
        // Quality-only updates restate values and are not samples.
        bus.set_quality(&id, 40, Quality { state: "degraded".into(), reasons: vec![] });
        bus.publish(SignalFrame { signal_id: id.clone(), ts_ms: 50, quality: Quality::ready(), values: BTreeMap::new() });
        let frames = bus.history(&id, 0, 0);
        assert_eq!(frames.iter().map(|f| f.ts_ms).collect::<Vec<_>>(), vec![10, 20, 30]);
        assert!(Arc::ptr_eq(&bus.history(&id, 30, 0)[0], &bus.history(&id, 0, 0)[2]));
        // `since_ms` is inclusive and thinning pins both ends.
        assert_eq!(bus.history(&id, 20, 0).iter().map(|f| f.ts_ms).collect::<Vec<_>>(), vec![20, 30]);
        assert_eq!(bus.history(&id, 0, 2).iter().map(|f| f.ts_ms).collect::<Vec<_>>(), vec![10, 30]);
        assert!(bus.history("nope.v1", 0, 0).is_empty());
    }

    #[test]
    fn history_evicts_by_window_and_by_cap() {
        let bus = SignalBus::new(crate::registry::for_tests(), None);
        let id = crate::registry::stats_signal_id(crate::series::SERIES[0].slug);
        bus.publish(sample(&id, 1_000));
        bus.publish(sample(&id, 1_000 + HISTORY_WINDOW_MS));
        assert_eq!(bus.history(&id, 0, 0).iter().map(|f| f.ts_ms).collect::<Vec<_>>(), vec![1_000, 1_000 + HISTORY_WINDOW_MS]);
        bus.publish(sample(&id, 1_001 + HISTORY_WINDOW_MS));
        assert_eq!(bus.history(&id, 0, 0)[0].ts_ms, 1_000 + HISTORY_WINDOW_MS);

        let bus = SignalBus::new(crate::registry::for_tests(), None);
        for ts in 0..(HISTORY_CAP as u64 + 5) {
            bus.publish(sample(&id, ts));
        }
        assert_eq!(bus.history_len(&id), HISTORY_CAP);
        assert_eq!(bus.history(&id, 0, 0)[0].ts_ms, 5);
    }
}
