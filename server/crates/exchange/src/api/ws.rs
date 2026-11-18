//! WebSocket upgrade handlers for `/ws/price`, `/ws/book`, and
//! `/ws/trades`.
//!
//! Each handler sends a one-time snapshot on connect, then forwards
//! broadcast messages until the client disconnects. Lagged subscribers
//! get a fresh snapshot instead of being dropped.

use axum::{
    extract::{
        Query, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::error::RecvError;
use tokio::time::{Duration, Instant, sleep_until};

/// Minimum spacing between two book frames on one `/ws/book` connection.
/// The market maker requotes ~10×/s and every requote broadcasts a full
/// snapshot; since each frame is absolute, a subscriber only ever needs
/// the newest one, so intermediate frames are dropped per connection and
/// the latest is sent once this interval has passed.
pub const BOOK_WS_MIN_INTERVAL_MS: u64 = 150;

use crate::pricing::feed::{pre_open_history_ms, FeedMessage, SNAPSHOT_MAX_TICKS, Tick};

use super::trades::{TRADES_SNAPSHOT_LIMIT, build_trades_snapshot};
use super::mid_history::build_book_join_message;
use super::{ApiError, AppState, BookSnapshotMessage, SeriesQuery, TradesMessage};
use crate::types::{DEFAULT_SERIES, SeriesId};

// ─────────────────────────── /ws/price ────────────────────────────────────

/// Initial message sent to every client right after upgrade. Carries the
/// active market metadata plus all price ticks observed since that market
/// opened, so a client joining mid-round can render the full chart. The
/// same shape answers `GET /api/price/history` for callers without a
/// socket, such as the chat agent's read tool.
#[derive(Debug, Serialize)]
pub struct PriceSnapshot {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub series: SeriesId,
    pub duration_secs: u64,
    pub market_id: Option<u32>,
    pub strike: Option<f64>,
    pub opens_at_ms: Option<u64>,
    pub closes_at_ms: Option<u64>,
    pub ticks: Vec<Tick>,
}

/// The open round plus a bounded run-up before it, thinned to at most
/// `max_ticks`. Both locks are held only briefly. The line is continuous
/// across rounds, so a client joining early sees the move into the open.
pub fn build_price_snapshot(state: &AppState, series: SeriesId, max_ticks: usize) -> PriceSnapshot {
    let exchange = state.exchange.lock().expect("exchange mutex poisoned");
    let market = exchange.engine(series).current_market();
    let since_ms = market.map_or(0, |m| {
        (m.opened_at * 1000).saturating_sub(pre_open_history_ms(series.duration_secs()))
    });
    PriceSnapshot {
        kind: "snapshot",
        series,
        duration_secs: series.duration_secs(),
        market_id: market.map(|m| m.id.0),
        strike: market.and_then(|m| m.strike_btc_usd()),
        opens_at_ms: market.map(|m| m.opened_at * 1000),
        closes_at_ms: market.map(|m| m.closes_at * 1000),
        ticks: state.price.snapshot_thinned_since(series, since_ms, max_ticks),
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub struct PriceHistoryQuery {
    #[serde(default)]
    pub series: Option<SeriesId>,
    /// Ticks after thinning; capped at the socket snapshot's limit.
    #[serde(default)]
    pub max_points: Option<usize>,
}

/// `GET /api/price/history?series=&max_points=`: the price socket's
/// snapshot over REST.
pub async fn price_history(
    State(state): State<AppState>,
    Query(q): Query<PriceHistoryQuery>,
) -> axum::Json<PriceSnapshot> {
    let series = q.series.unwrap_or(DEFAULT_SERIES);
    let max_ticks = q.max_points.unwrap_or(SNAPSHOT_MAX_TICKS).clamp(1, SNAPSHOT_MAX_TICKS);
    axum::Json(build_price_snapshot(&state, series, max_ticks))
}

pub async fn price_ws(
    State(state): State<AppState>,
    Query(q): Query<SeriesQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    let series = q.series.unwrap_or(DEFAULT_SERIES);
    ws.on_upgrade(move |socket| handle_price_ws(state, series, socket))
}

async fn handle_price_ws(state: AppState, series: SeriesId, mut socket: WebSocket) {
    let snapshot = build_price_snapshot(&state, series, SNAPSHOT_MAX_TICKS);

    let json = match serde_json::to_string(&snapshot) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "failed to serialize price snapshot");
            return;
        }
    };
    if socket.send(Message::Text(json.into())).await.is_err() {
        return;
    }

    let mut rx = state.price.subscribe();
    loop {
        tokio::select! {
            broadcast = rx.recv() => match broadcast {
                Ok(msg) => {
                    if !forward_to_series(&msg, series) {
                        continue;
                    }
                    let json = match serde_json::to_string(&msg) {
                        Ok(s) => s,
                        Err(e) => {
                            tracing::warn!(error = %e, "failed to serialize feed message");
                            continue;
                        }
                    };
                    if socket.send(Message::Text(json.into())).await.is_err() {
                        break;
                    }
                }
                Err(RecvError::Lagged(n)) => {
                    tracing::warn!(dropped = n, "price ws subscriber lagged");
                }
                Err(RecvError::Closed) => break,
            },
            client_msg = socket.recv() => match client_msg {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                _ => {}
            },
        }
    }
}

/// Ticks are shared by every series; a round reset belongs to one series
/// and must not wipe the chart of a subscriber watching another.
pub fn forward_to_series(msg: &FeedMessage, subscriber: SeriesId) -> bool {
    match msg {
        FeedMessage::Tick { .. } => true,
        FeedMessage::MarketReset { series, .. } => *series == subscriber,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::SeriesChannels;
    use crate::engine::Exchange;
    use crate::pricing::feed::PriceFeed;
    use crate::series::SERIES;
    use crate::types::Tenths;
    use std::sync::{Arc, Mutex};

    fn app_state() -> AppState {
        let mut x = Exchange::new(Tenths(1_000_000), SERIES);
        // Round 1 opened at t = 300 s with ticks from t = 0 s, so the pre-open run-up
        // (two minutes for the 2m series) starts well inside the recorded history.
        x.open_first_market(SeriesId(0), 100_000.0, 300, SeriesId(0).duration_secs());
        let price = PriceFeed::new();
        for i in 0..400u64 {
            price.record_with_ts(i * 1000, 100_000.0 + i as f64);
        }
        // No manager behind the handle: these tests never deploy.
        let (runtime, _rx) = crate::automation::runtime::runtime_channel();
        AppState {
            exchange: Arc::new(Mutex::new(x)),
            channels: SeriesChannels::for_all_series(),
            price,
            runtime,
            registry: crate::registry::for_tests(),
            bus: crate::automation::signal_bus::SignalBus::new(crate::registry::for_tests(), None),
            templates: Arc::new(crate::templates::TemplateCatalogue::default()),
            deploys: crate::api::DeployKeys::default(),
            crowd: false,
        }
    }

    #[tokio::test]
    async fn price_history_serves_the_socket_snapshot_thinned() {
        let state = app_state();
        let axum::Json(out) =
            price_history(State(state.clone()), Query(PriceHistoryQuery { series: None, max_points: Some(10) })).await;
        assert_eq!(out.kind, "snapshot");
        assert_eq!(out.series, SeriesId(0));
        assert_eq!(out.market_id, Some(1));
        assert_eq!(out.strike, Some(100_000.0));
        assert_eq!(out.opens_at_ms, Some(300_000));
        assert_eq!(out.ticks.len(), 10);
        let since = 300_000 - pre_open_history_ms(SeriesId(0).duration_secs());
        assert_eq!(out.ticks[0].ts_ms, since, "first tick is the start of the run-up");
        assert_eq!(out.ticks[9].ts_ms, 399_000, "last tick is the newest");

        let axum::Json(all) =
            price_history(State(state), Query(PriceHistoryQuery { series: None, max_points: Some(0) })).await;
        assert_eq!(all.ticks.len(), 1, "max_points is clamped to at least one tick");
    }

    #[test]
    fn price_ws_filters_resets_by_series() {
        let tick = FeedMessage::Tick { ts_ms: 1, price: 100.0 };
        let reset0 = FeedMessage::MarketReset {
            series: SeriesId(0),
            market_id: 1,
            strike: 100.0,
            opens_at_ms: 0,
            closes_at_ms: 60_000,
            duration_secs: 60,
        };
        let reset1 = FeedMessage::MarketReset {
            series: SeriesId(1),
            market_id: 2,
            strike: 100.0,
            opens_at_ms: 0,
            closes_at_ms: 900_000,
            duration_secs: 900,
        };
        assert!(forward_to_series(&tick, SeriesId(0)));
        assert!(forward_to_series(&tick, SeriesId(1)));
        assert!(forward_to_series(&reset0, SeriesId(0)));
        assert!(!forward_to_series(&reset0, SeriesId(1)));
        assert!(forward_to_series(&reset1, SeriesId(1)));
        assert!(!forward_to_series(&reset1, SeriesId(0)));
    }
}

// ─────────────────────────── /ws/book ───────────────────────────────────────

pub async fn book_ws(
    State(state): State<AppState>,
    Query(q): Query<SeriesQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    let series = q.series.unwrap_or(DEFAULT_SERIES);
    ws.on_upgrade(move |socket| handle_book_ws(state, series, socket))
}

async fn handle_book_ws(state: AppState, series: SeriesId, mut socket: WebSocket) {
    // Initial snapshot from current engine state, with the round's
    // midpoint history so the probability chart starts complete.
    let initial = build_book_join_message(&state, series);
    if !send_book_message(&mut socket, &initial).await {
        return;
    }

    let mut rx = state.channels_for(series).book_tx.subscribe();
    let interval = Duration::from_millis(BOOK_WS_MIN_INTERVAL_MS);
    let mut last_sent = Instant::now();
    // The newest frame received since `last_sent`; older ones are superseded.
    let mut pending: Option<BookSnapshotMessage> = None;
    loop {
        tokio::select! {
            broadcast = rx.recv() => match broadcast {
                Ok(msg) => pending = Some(msg),
                Err(RecvError::Lagged(n)) => {
                    tracing::warn!(dropped = n, "book ws subscriber lagged");
                    // Recover by queueing a fresh snapshot so the client's view
                    // is consistent again; the history covers the frames it missed.
                    pending = Some(build_book_join_message(&state, series));
                }
                Err(RecvError::Closed) => break,
            },
            // Fires immediately when the interval has already elapsed, so the
            // first frame after a quiet spell goes out with no added delay.
            _ = sleep_until(last_sent + interval), if pending.is_some() => {
                let msg = pending.take().expect("guarded by precondition");
                if !send_book_message(&mut socket, &msg).await {
                    break;
                }
                last_sent = Instant::now();
            }
            client_msg = socket.recv() => match client_msg {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                _ => {}
            },
        }
    }
}

async fn send_book_message(socket: &mut WebSocket, msg: &BookSnapshotMessage) -> bool {
    let json = match serde_json::to_string(msg) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "failed to serialize book snapshot");
            return true;
        }
    };
    socket.send(Message::Text(json.into())).await.is_ok()
}

// ─────────────────────────── /ws/trades ───────────────────────────────────

/// `?series=<slug>` for one series' tape, `?series=all` for every
/// series merged (rows carry their `series`). Absent ⇒ the default.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TradesWsQuery {
    #[serde(default)]
    pub series: Option<String>,
}

/// Which tapes a `/ws/trades` subscriber wants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TradesScope {
    One(SeriesId),
    All,
}

impl TradesScope {
    pub fn parse(raw: Option<&str>) -> Result<Self, ApiError> {
        match raw {
            None => Ok(TradesScope::One(DEFAULT_SERIES)),
            Some("all") => Ok(TradesScope::All),
            Some(slug) => slug
                .parse::<SeriesId>()
                .map(TradesScope::One)
                .map_err(|e| ApiError::UnknownSeries(e.0)),
        }
    }
}

pub async fn trades_ws(
    State(state): State<AppState>,
    Query(q): Query<TradesWsQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    match TradesScope::parse(q.series.as_deref()) {
        Ok(scope) => ws.on_upgrade(move |socket| handle_trades_ws(state, scope, socket)),
        Err(e) => e.into_response(),
    }
}

async fn handle_trades_ws(state: AppState, scope: TradesScope, mut socket: WebSocket) {
    let series_list: Vec<SeriesId> = match scope {
        TradesScope::One(s) => vec![s],
        TradesScope::All => state.channels.iter().map(|c| c.series).collect(),
    };
    let snapshot = |state: &AppState| {
        let exchange = state.exchange.lock().expect("exchange mutex poisoned");
        let mut merged = TradesMessage { kind: "snapshot", trades: Vec::new() };
        for s in &series_list {
            if exchange.has_series(*s) {
                merged
                    .trades
                    .extend(build_trades_snapshot(&exchange, *s, TRADES_SNAPSHOT_LIMIT).trades);
            }
        }
        merged.trades.sort_by(|a, b| b.ts.cmp(&a.ts));
        merged.trades.truncate(TRADES_SNAPSHOT_LIMIT);
        merged
    };
    if !send_trades_message(&mut socket, &snapshot(&state)).await {
        return;
    }

    match scope {
        TradesScope::One(series) => {
            let mut rx = state.channels_for(series).trades_tx.subscribe();
            loop {
                tokio::select! {
                    broadcast = rx.recv() => match broadcast {
                        Ok(msg) => {
                            if !send_trades_message(&mut socket, &msg).await {
                                break;
                            }
                        }
                        Err(RecvError::Lagged(n)) => {
                            tracing::warn!(dropped = n, "trades ws subscriber lagged");
                            if !send_trades_message(&mut socket, &snapshot(&state)).await {
                                break;
                            }
                        }
                        Err(RecvError::Closed) => break,
                    },
                    client_msg = socket.recv() => match client_msg {
                        None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                        _ => {}
                    },
                }
            }
        }
        TradesScope::All => {
            // Merge every series' batches onto one socket.
            let (tx, mut rx) = tokio::sync::mpsc::channel::<TradesMessage>(64);
            for ch in state.channels.iter() {
                let mut sub = ch.trades_tx.subscribe();
                let tx = tx.clone();
                tokio::spawn(async move {
                    loop {
                        match sub.recv().await {
                            Ok(msg) => {
                                if tx.send(msg).await.is_err() {
                                    break;
                                }
                            }
                            Err(RecvError::Lagged(_)) => continue,
                            Err(RecvError::Closed) => break,
                        }
                    }
                });
            }
            drop(tx);
            loop {
                tokio::select! {
                    msg = rx.recv() => match msg {
                        Some(msg) => {
                            if !send_trades_message(&mut socket, &msg).await {
                                break;
                            }
                        }
                        None => break,
                    },
                    client_msg = socket.recv() => match client_msg {
                        None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                        _ => {}
                    },
                }
            }
        }
    }
}

async fn send_trades_message(socket: &mut WebSocket, msg: &TradesMessage) -> bool {
    let json = match serde_json::to_string(msg) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "failed to serialize trades message");
            return true;
        }
    };
    socket.send(Message::Text(json.into())).await.is_ok()
}
