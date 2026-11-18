//! `/ws/signals?series=<slug>` — live signal frames for one market.
//!
//! The socket carries values only; the catalogue (`GET /api/signals`)
//! carries the metadata. A client says what it wants with a `subscribe`
//! message and may send another at any time to replace the set:
//!
//! ```json
//! {"kind": "subscribe", "ids": ["btc-5m.current.stats.v1", "pyth.btc_usd.spot.v1"]}
//! {"kind": "subscribe", "set": "series"}
//! {"kind": "subscribe", "ids": ["btc.coinbase.rsi.v1"], "history_ms": 240000, "max_points": 500}
//! ```
//!
//! `ids` is the pinned set; `set: "series"` is every signal that applies
//! to the market ([`signals_for`]), for Browse All. Each subscribe is
//! answered with a `snapshot` of the latest frame per subscribed signal,
//! then a `batch` every [`SIGNALS_WS_INTERVAL_MS`] carrying only the
//! signals that published since the last send, newest frame each. Ids
//! outside the market's set are refused in an `error` message and the
//! rest of the request still applies, so a stale pin from another market
//! cannot leak through.
//!
//! A subscribe carrying `history_ms` is answered first with a `history`
//! message: for every subscribed signal, the frames the bus retained over
//! that many milliseconds before now, oldest first, evenly thinned to
//! `max_points` (default [`SIGNALS_WS_HISTORY_MAX_POINTS`]). A chart opens
//! with its window filled instead of accumulating from the moment it
//! subscribed. The bus keeps at most
//! [`crate::automation::signal_bus::HISTORY_WINDOW_MS`] regardless.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use axum::{
    extract::{
        Query, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    response::Response,
};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::error::RecvError;
use tokio::time::{Duration, MissedTickBehavior, interval};

use super::{AppState, SeriesQuery};
use crate::automation::signal_bus::{SignalBus, SignalFrame};
use crate::registry::signals_for;
use crate::types::SeriesId;

/// How often a connection flushes its pending frames.
pub const SIGNALS_WS_INTERVAL_MS: u64 = 200;

/// Frames per signal in a `history` answer unless the subscribe says
/// otherwise: a few per pixel of the widest chart the rail can hold.
pub const SIGNALS_WS_HISTORY_MAX_POINTS: usize = 2_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SubscribeSet {
    /// Every signal that applies to the market.
    Series,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ClientMessage {
    Subscribe {
        #[serde(default)]
        ids: Vec<String>,
        #[serde(default)]
        set: Option<SubscribeSet>,
        /// Answer with the retained frames of the last this-many milliseconds.
        #[serde(default)]
        history_ms: Option<u64>,
        /// Thin each signal's history to at most this many frames.
        #[serde(default)]
        max_points: Option<usize>,
    },
}

/// One signal's retained frames, oldest first.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SignalHistory {
    pub signal_id: String,
    pub frames: Vec<Arc<SignalFrame>>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ServerMessage {
    Snapshot { frames: Vec<Arc<SignalFrame>> },
    History { signals: Vec<SignalHistory> },
    Batch { frames: Vec<Arc<SignalFrame>> },
    Error { message: String },
}

/// What one connection is subscribed to, resolved against the market.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Subscription {
    ids: BTreeSet<String>,
}

impl Subscription {
    /// Resolve a subscribe request. Returns the subscription and the ids
    /// that were refused because they do not apply to `series`.
    pub fn resolve(bus: &SignalBus, series: SeriesId, ids: &[String], whole_series: bool) -> (Subscription, Vec<String>) {
        let applicable: BTreeSet<String> = signals_for(bus.registry(), series.slug()).map(|d| d.signal_id.clone()).collect();
        if whole_series {
            return (Subscription { ids: applicable }, Vec::new());
        }
        let (known, refused): (Vec<String>, Vec<String>) = ids.iter().cloned().partition(|id| applicable.contains(id));
        (Subscription { ids: known.into_iter().collect() }, refused)
    }

    pub fn contains(&self, signal_id: &str) -> bool {
        self.ids.contains(signal_id)
    }

    /// The latest frame of every subscribed signal that has published.
    pub fn snapshot(&self, bus: &SignalBus) -> Vec<Arc<SignalFrame>> {
        self.ids.iter().filter_map(|id| bus.latest(id)).collect()
    }

    /// The retained frames of every subscribed signal since `since_ms`,
    /// thinned to `max_points` each; signals with nothing retained are
    /// omitted, like the snapshot omits signals that never published.
    pub fn history(&self, bus: &SignalBus, since_ms: u64, max_points: usize) -> Vec<SignalHistory> {
        self.ids
            .iter()
            .filter_map(|id| {
                let frames = bus.history(id, since_ms, max_points);
                (!frames.is_empty()).then(|| SignalHistory { signal_id: id.clone(), frames })
            })
            .collect()
    }
}

pub async fn signals_ws(State(state): State<AppState>, Query(q): Query<SeriesQuery>, ws: WebSocketUpgrade) -> Response {
    let series = state.series_or_default(q.series);
    ws.on_upgrade(move |socket| handle_signals_ws(state.bus.clone(), series, socket))
}

async fn send(socket: &mut WebSocket, msg: &ServerMessage) -> bool {
    match serde_json::to_string(msg) {
        Ok(json) => socket.send(Message::Text(json.into())).await.is_ok(),
        Err(e) => {
            tracing::warn!(error = %e, "failed to serialize signals message");
            true
        }
    }
}

async fn handle_signals_ws(bus: Arc<SignalBus>, series: SeriesId, mut socket: WebSocket) {
    let mut rx = bus.subscribe();
    let mut subscription = Subscription::default();
    let mut pending: BTreeMap<String, Arc<SignalFrame>> = BTreeMap::new();
    let mut ticker = interval(Duration::from_millis(SIGNALS_WS_INTERVAL_MS));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            frame = rx.recv() => match frame {
                Ok(frame) => {
                    if subscription.contains(&frame.signal_id) {
                        pending.insert(frame.signal_id.clone(), frame);
                    }
                }
                Err(RecvError::Lagged(n)) => {
                    tracing::debug!(dropped = n, "signals ws subscriber lagged; resnapshotting");
                    for frame in subscription.snapshot(&bus) {
                        pending.insert(frame.signal_id.clone(), frame);
                    }
                }
                Err(RecvError::Closed) => break,
            },
            _ = ticker.tick() => {
                if pending.is_empty() {
                    continue;
                }
                let frames = std::mem::take(&mut pending).into_values().collect();
                if !send(&mut socket, &ServerMessage::Batch { frames }).await {
                    break;
                }
            },
            client = socket.recv() => match client {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                Some(Ok(Message::Text(text))) => {
                    let request: ClientMessage = match serde_json::from_str(&text) {
                        Ok(r) => r,
                        Err(e) => {
                            if !send(&mut socket, &ServerMessage::Error { message: format!("bad request: {e}") }).await {
                                break;
                            }
                            continue;
                        }
                    };
                    let ClientMessage::Subscribe { ids, set, history_ms, max_points } = request;
                    let (next, refused) = Subscription::resolve(&bus, series, &ids, set == Some(SubscribeSet::Series));
                    if !refused.is_empty() {
                        let message = format!("not signals of {}: {}", series.slug(), refused.join(", "));
                        if !send(&mut socket, &ServerMessage::Error { message }).await {
                            break;
                        }
                    }
                    subscription = next;
                    pending.clear();
                    if let Some(history_ms) = history_ms {
                        let since_ms = crate::pricing::feed::unix_now_ms().saturating_sub(history_ms);
                        let signals = subscription.history(&bus, since_ms, max_points.unwrap_or(SIGNALS_WS_HISTORY_MAX_POINTS));
                        if !send(&mut socket, &ServerMessage::History { signals }).await {
                            break;
                        }
                    }
                    let frames = subscription.snapshot(&bus);
                    if !send(&mut socket, &ServerMessage::Snapshot { frames }).await {
                        break;
                    }
                }
                Some(Ok(_)) => {}
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::automation::signal_bus::Quality;
    use crate::registry::{self, stats_signal_id};
    use crate::series::SERIES;
    use serde_json::json;

    #[test]
    fn subscribe_resolves_against_the_market() {
        let bus = SignalBus::new(registry::for_tests(), None);
        let a = SERIES[0].id;
        let mine = stats_signal_id(a.slug());
        let theirs = stats_signal_id(SERIES[1].slug);
        let (sub, refused) = Subscription::resolve(&bus, a, &[mine.clone(), theirs.clone(), "nope".into()], false);
        assert!(sub.contains(&mine));
        assert!(!sub.contains(&theirs));
        assert_eq!(refused, vec![theirs.clone(), "nope".to_string()]);

        let (all, refused) = Subscription::resolve(&bus, a, &[], true);
        assert!(refused.is_empty());
        assert!(all.contains(&mine));
        assert!(!all.contains(&theirs));
        assert!(all.contains("coinbase.btc_usd.spot.v1"));

        assert!(sub.snapshot(&bus).is_empty(), "nothing published yet");
        bus.publish(SignalFrame { signal_id: mine.clone(), ts_ms: 5, quality: Quality::ready(), values: [("trades".to_string(), json!(1))].into_iter().collect() });
        let snap = sub.snapshot(&bus);
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].ts_ms, 5);
    }

    #[test]
    fn history_answers_per_subscribed_signal_and_omits_the_unpublished() {
        let bus = SignalBus::new(registry::for_tests(), None);
        let series = SERIES[0].id;
        let stats = stats_signal_id(series.slug());
        let spot = "coinbase.btc_usd.spot.v1".to_string();
        for ts in [10, 20, 30, 40] {
            bus.publish(SignalFrame { signal_id: stats.clone(), ts_ms: ts, quality: Quality::ready(), values: [("trades".to_string(), json!(ts))].into_iter().collect() });
        }
        let (sub, _) = Subscription::resolve(&bus, series, &[stats.clone(), spot], false);
        let history = sub.history(&bus, 20, 2);
        assert_eq!(history.len(), 1, "the spot feed never published");
        assert_eq!(history[0].signal_id, stats);
        assert_eq!(history[0].frames.iter().map(|f| f.ts_ms).collect::<Vec<_>>(), vec![20, 40]);
    }

    #[test]
    fn wire_shapes() {
        let req: ClientMessage = serde_json::from_str(r#"{"kind":"subscribe","set":"series"}"#).unwrap();
        assert!(matches!(req, ClientMessage::Subscribe { set: Some(SubscribeSet::Series), history_ms: None, max_points: None, .. }));
        let req: ClientMessage = serde_json::from_str(r#"{"kind":"subscribe","ids":["a"]}"#).unwrap();
        assert!(matches!(req, ClientMessage::Subscribe { set: None, ref ids, .. } if ids == &["a".to_string()]));
        let req: ClientMessage =
            serde_json::from_str(r#"{"kind":"subscribe","ids":["a"],"history_ms":240000,"max_points":500}"#).unwrap();
        assert!(matches!(req, ClientMessage::Subscribe { history_ms: Some(240_000), max_points: Some(500), .. }));
        let out = serde_json::to_value(ServerMessage::Batch { frames: vec![] }).unwrap();
        assert_eq!(out, json!({"kind": "batch", "frames": []}));
        let frame = Arc::new(SignalFrame { signal_id: "a".into(), ts_ms: 7, quality: Quality::ready(), values: [("x".to_string(), json!("1.5"))].into_iter().collect() });
        let out = serde_json::to_value(ServerMessage::History { signals: vec![SignalHistory { signal_id: "a".into(), frames: vec![frame] }] }).unwrap();
        assert_eq!(
            out,
            json!({"kind": "history", "signals": [{"signal_id": "a", "frames": [{"signal_id": "a", "ts_ms": 7, "quality": {"state": "ready", "reasons": []}, "values": {"x": "1.5"}}]}]})
        );
    }
}
