//! Per-user position stream and close-position action.
//!
//! `/ws/positions?address=…` — streams a `PositionSnapshot` whenever the
//! caller's account state changes. The handler subscribes to both the
//! trades and book broadcasts; on each event it rebuilds the snapshot
//! under the engine lock and forwards it only if it differs from the
//! last one sent (trivial dedup against the noisy book broadcast).
//!
//! `GET /api/accounts/{address}/positions?series=` — the same snapshot
//! once, over REST, for callers without a socket such as the chat agent.
//!
//! `POST /api/positions/close` — issues a market order on the closing
//! side at the caller's current size, ending at FLAT. Atomically reads
//! position size under the engine lock so a stale UI doesn't race.

use std::time::{SystemTime, UNIX_EPOCH};

use axum::{
    Json,
    extract::{
        Path, Query, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::StatusCode,
    response::Response,
};
use serde::{Deserialize, Serialize};

use crate::engine::{Exchange, FlipOutcome, MarketResult, PAYOUT_PER_CONTRACT, PlaceOutcome, TradeSource};
use crate::types::{Address, DEFAULT_SERIES, Qty, SeriesId, Side};

use super::{ApiError, AppState, broadcast_book, subscribe_all_series};

// ─────────────────────────── Wire types ────────────────────────────────────

/// Direction projected onto the YES contract for display.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PositionSide {
    Yes,
    No,
    Flat,
}

/// Pre-computed quote for the "close to flat" action. `qty_needed` is the
/// market-order size the close-position endpoint will submit, and `side`
/// is the side that order will rest on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct FlipQuote {
    pub target_signed_position: i64,
    pub qty_needed: u32,
    pub side: Side,
}

/// One series' slice of an account's exposure: the flat position fields
/// of [`PositionSnapshotMessage`] plus this-round and last-round PnL.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SeriesPositionRow {
    pub series: SeriesId,
    pub market_id: Option<u32>,
    pub side: PositionSide,
    pub size_shares: u32,
    pub signed_position: i64,
    pub avg_entry_yes_tenths: Option<u16>,
    pub cost_basis_tenths: i64,
    pub mark_yes_tenths: Option<u16>,
    pub mark_value_tenths: Option<i64>,
    pub unrealized_pnl_tenths: Option<i64>,
    pub return_pct: Option<f64>,
    pub flip_quote: Option<FlipQuote>,
    pub realized_pnl_this_market_tenths: i64,
    /// Realized this round plus the open position marked to the book.
    pub market_pnl_tenths: i64,
    pub trades_this_market: u32,
    pub last_market: Option<MarketResult>,
}

/// Streamed over `/ws/positions`. All money fields are in tenths of a cent.
/// `*_pct` is a percentage (multiplied by 100 already), not a fraction.
///
/// The flat fields describe the requested series (default: the first);
/// `series_positions` carries the same figures for every series.
#[derive(Debug, Clone, Serialize)]
pub struct PositionSnapshotMessage {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub as_of_unix_ms: u64,
    pub series: SeriesId,
    pub market_id: Option<u32>,
    pub address: Address,
    pub side: PositionSide,
    pub size_shares: u32,
    pub signed_position: i64,
    pub avg_entry_yes_tenths: Option<u16>,
    pub cost_basis_tenths: i64,
    pub mark_yes_tenths: Option<u16>,
    pub mark_value_tenths: Option<i64>,
    pub unrealized_pnl_tenths: Option<i64>,
    pub realized_pnl_this_market_tenths: i64,
    pub return_pct: Option<f64>,
    pub balance_tenths: i64,
    pub locked_tenths: i64,
    pub flip_quote: Option<FlipQuote>,
    pub series_positions: Vec<SeriesPositionRow>,
}

/// Stripped-down view used to dedup snapshots — every field that should
/// trigger a push, with the wall-clock timestamp omitted so identical
/// state observed at two instants does not produce a redundant frame.
#[derive(Debug, Clone, PartialEq)]
struct SnapshotKey {
    market_id: Option<u32>,
    side: PositionSide,
    size_shares: u32,
    signed_position: i64,
    avg_entry_yes_tenths: Option<u16>,
    cost_basis_tenths: i64,
    mark_yes_tenths: Option<u16>,
    mark_value_tenths: Option<i64>,
    unrealized_pnl_tenths: Option<i64>,
    realized_pnl_this_market_tenths: i64,
    return_pct_bits: Option<u64>,
    balance_tenths: i64,
    locked_tenths: i64,
    flip_quote: Option<FlipQuote>,
    series_positions: Vec<SeriesPositionRow>,
}

impl SnapshotKey {
    fn from(msg: &PositionSnapshotMessage) -> Self {
        Self {
            market_id: msg.market_id,
            side: msg.side,
            size_shares: msg.size_shares,
            signed_position: msg.signed_position,
            avg_entry_yes_tenths: msg.avg_entry_yes_tenths,
            cost_basis_tenths: msg.cost_basis_tenths,
            mark_yes_tenths: msg.mark_yes_tenths,
            mark_value_tenths: msg.mark_value_tenths,
            unrealized_pnl_tenths: msg.unrealized_pnl_tenths,
            realized_pnl_this_market_tenths: msg.realized_pnl_this_market_tenths,
            return_pct_bits: msg.return_pct.map(f64::to_bits),
            balance_tenths: msg.balance_tenths,
            locked_tenths: msg.locked_tenths,
            flip_quote: msg.flip_quote,
            series_positions: msg.series_positions.clone(),
        }
    }
}

// ─────────────────────────── Snapshot builder ──────────────────────────────

fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Compute the live position snapshot for `address`. Returns `None` if no
/// such account exists. The flat fields describe `series`; the vector
/// covers every series the exchange runs.
pub fn build_position_snapshot(
    exchange: &Exchange,
    address: Address,
    series: SeriesId,
) -> Option<PositionSnapshotMessage> {
    let acct = exchange.account(&address)?;
    let series_positions: Vec<SeriesPositionRow> = exchange
        .series()
        .map(|s| build_series_position_row(exchange, address, s))
        .collect();
    let row = series_positions
        .iter()
        .find(|r| r.series == series)
        .cloned()
        .unwrap_or_else(|| build_series_position_row(exchange, address, series));

    Some(PositionSnapshotMessage {
        kind: "position_snapshot",
        as_of_unix_ms: unix_now_ms(),
        series,
        market_id: row.market_id,
        address,
        side: row.side,
        size_shares: row.size_shares,
        signed_position: row.signed_position,
        avg_entry_yes_tenths: row.avg_entry_yes_tenths,
        cost_basis_tenths: row.cost_basis_tenths,
        mark_yes_tenths: row.mark_yes_tenths,
        mark_value_tenths: row.mark_value_tenths,
        unrealized_pnl_tenths: row.unrealized_pnl_tenths,
        realized_pnl_this_market_tenths: row.realized_pnl_this_market_tenths,
        return_pct: row.return_pct,
        balance_tenths: acct.usdc_balance.0,
        locked_tenths: acct.locked.0,
        flip_quote: row.flip_quote,
        series_positions,
    })
}

/// `address`'s exposure in one series, marked to that series' book.
pub fn build_series_position_row(
    exchange: &Exchange,
    address: Address,
    series: SeriesId,
) -> SeriesPositionRow {
    let engine = exchange.engine(series);
    let ps = engine.position(&address);

    let signed = ps.position.0;
    let abs = signed.unsigned_abs();

    let side = if signed > 0 {
        PositionSide::Yes
    } else if signed < 0 {
        PositionSide::No
    } else {
        PositionSide::Flat
    };

    let (avg_entry, cost_basis) = (ps.avg_entry(), ps.cost_basis());

    // Display mark: use implied-extreme (one-sided book → 99.9¢ / 0.1¢)
    // rather than strict mid, so the user's PnL reflects the price
    // signal a vanished side gives off instead of falling through to
    // a stale last trade or a 50¢-ish fair-value default.
    let live_mark = engine.book().implied_mark_yes_tenths();
    let fallback_mark = if signed == 0 {
        live_mark
    } else {
        live_mark
            .or_else(|| engine.recent_trades(1).next().map(|fill| fill.price.tenths()))
            .or(avg_entry)
    };

    let (mark_value, unrealized, return_pct) = match (fallback_mark, signed) {
        (_, 0) => (None, None, None),
        (Some(mark_t), _) => {
            let mark_i = mark_t as i64;
            let avg_i = avg_entry.expect("non-flat has avg") as i64;
            let unrealized_v = signed * (mark_i - avg_i);
            // Close-at-mark proceeds, in tenths. Long: pos × mark.
            // Short: |pos| × (1$ − mark) when buying back at mark.
            let mark_val = if signed > 0 {
                signed * mark_i
            } else {
                (-signed) * (PAYOUT_PER_CONTRACT - mark_i)
            };
            let ret = if cost_basis > 0 {
                Some(unrealized_v as f64 / cost_basis as f64 * 100.0)
            } else {
                None
            };
            (Some(mark_val), Some(unrealized_v), ret)
        }
        (None, _) => (None, None, None),
    };

    let flip_quote = if signed != 0 {
        let close_side = if signed > 0 { Side::Ask } else { Side::Bid };
        Some(FlipQuote {
            target_signed_position: 0,
            qty_needed: abs as u32,
            side: close_side,
        })
    } else {
        None
    };

    let market_id = engine.current_market().map(|m| m.id.0);
    let realized = ps.realized_pnl_this_market.0;

    SeriesPositionRow {
        series,
        market_id,
        side,
        size_shares: abs as u32,
        signed_position: signed,
        avg_entry_yes_tenths: avg_entry,
        cost_basis_tenths: cost_basis,
        mark_yes_tenths: fallback_mark,
        mark_value_tenths: mark_value,
        unrealized_pnl_tenths: unrealized,
        return_pct,
        flip_quote,
        realized_pnl_this_market_tenths: realized,
        market_pnl_tenths: realized + unrealized.unwrap_or(0),
        trades_this_market: engine.trades_this_market(&address),
        last_market: ps.last_market,
    }
}

/// Address query for `/ws/positions?address=…`.
#[derive(Debug, Clone, Deserialize)]
pub struct PositionQuery {
    pub address: Address,
    /// Series the flat fields describe; absent ⇒ the default series.
    #[serde(default)]
    pub series: Option<SeriesId>,
}

/// Body for `POST /api/positions/close` and `/api/positions/flip`.
#[derive(Debug, Clone, Deserialize)]
pub struct ClosePositionRequest {
    pub address: Address,
    #[serde(default)]
    pub series: Option<SeriesId>,
}

// ───────────────────────── REST snapshot ───────────────────────────────────

/// `GET /api/accounts/{address}/positions?series=`: one position snapshot,
/// every series included, the flat fields describing `series`.
pub async fn get_positions(
    State(state): State<AppState>,
    Path(address): Path<String>,
    Query(q): Query<super::SeriesQuery>,
) -> Result<Json<PositionSnapshotMessage>, ApiError> {
    let address: Address = address.parse()?;
    let series = state.series_or_default(q.series);
    let exchange = state.exchange.lock().expect("exchange mutex poisoned");
    build_position_snapshot(&exchange, address, series).map(Json).ok_or(ApiError::UnknownAccount)
}

// ───────────────────────── WebSocket handler ───────────────────────────────

pub async fn positions_ws(
    State(state): State<AppState>,
    Query(q): Query<PositionQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    let series = q.series.unwrap_or(DEFAULT_SERIES);
    ws.on_upgrade(move |socket| handle_positions_ws(state, q.address, series, socket))
}

async fn handle_positions_ws(
    state: AppState,
    address: Address,
    series: SeriesId,
    mut socket: WebSocket,
) {
    // Build the initial snapshot. If the account doesn't exist we send one
    // error frame and close — the WS isn't useful for unknown addresses.
    let initial = {
        let exchange = state.exchange.lock().expect("exchange mutex poisoned");
        build_position_snapshot(&exchange, address, series)
    };
    let Some(initial) = initial else {
        let err = serde_json::json!({
            "type": "error",
            "error": "unknown_account",
            "message": "no account at that address",
        });
        let _ = socket.send(Message::Text(err.to_string().into())).await;
        return;
    };

    let mut last_key = SnapshotKey::from(&initial);
    if !send_snapshot(&mut socket, &initial).await {
        return;
    }

    // Any series' activity can change this account's snapshot (shared
    // cash, per-series rows), so wake on every series' broadcasts.
    let mut wakeups = subscribe_all_series(&state.channels);

    loop {
        tokio::select! {
            wake = wakeups.recv() => match wake {
                Some(()) => {
                    if !push_if_changed(&state, address, series, &mut last_key, &mut socket).await {
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

async fn push_if_changed(
    state: &AppState,
    address: Address,
    series: SeriesId,
    last_key: &mut SnapshotKey,
    socket: &mut WebSocket,
) -> bool {
    let snapshot = {
        let exchange = state.exchange.lock().expect("exchange mutex poisoned");
        build_position_snapshot(&exchange, address, series)
    };
    let Some(snapshot) = snapshot else {
        // Account vanished mid-stream — unreachable in practice (accounts
        // are never deleted) but bail cleanly if so.
        return false;
    };
    let key = SnapshotKey::from(&snapshot);
    if key == *last_key {
        return true;
    }
    *last_key = key;
    send_snapshot(socket, &snapshot).await
}

async fn send_snapshot(socket: &mut WebSocket, msg: &PositionSnapshotMessage) -> bool {
    let json = match serde_json::to_string(msg) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "failed to serialize position snapshot");
            return true;
        }
    };
    socket.send(Message::Text(json.into())).await.is_ok()
}

// ──────────────────────── Close-position HTTP ──────────────────────────────

/// POST `/api/positions/close` — market-orders out of the caller's current
/// position back to flat. Reads position size under the engine lock so the
/// computed qty cannot race against an in-flight fill.
pub async fn close_position(
    State(state): State<AppState>,
    Json(req): Json<ClosePositionRequest>,
) -> Result<(StatusCode, Json<PlaceOutcome>), ApiError> {
    let series = req.series.unwrap_or(DEFAULT_SERIES);
    let mut exchange = state.exchange.lock().expect("exchange mutex poisoned");
    exchange
        .account(&req.address)
        .ok_or(ApiError::UnknownAccount)?;
    let signed = exchange.position(series, &req.address).position.0;
    if signed == 0 {
        return Err(ApiError::BadRequest("no open position to close"));
    }
    let qty = Qty(signed.unsigned_abs() as u32);
    // Long → sell to flatten; short → buy to flatten.
    let close_side = if signed > 0 { Side::Ask } else { Side::Bid };
    let outcome = exchange.place_market(series, req.address, close_side, qty, TradeSource::Ui)?;
    broadcast_book(exchange.engine(series), state.channels_for(series));
    Ok((StatusCode::OK, Json(outcome)))
}

/// POST `/api/positions/flip` — closes the caller's current position and
/// opens the opposite side using the current marked USD value as the target
/// notional. All sizing happens under the engine lock.
pub async fn flip_position(
    State(state): State<AppState>,
    Json(req): Json<ClosePositionRequest>,
) -> Result<(StatusCode, Json<FlipOutcome>), ApiError> {
    let series = req.series.unwrap_or(DEFAULT_SERIES);
    let mut exchange = state.exchange.lock().expect("exchange mutex poisoned");
    exchange
        .account(&req.address)
        .ok_or(ApiError::UnknownAccount)?;
    if exchange.position(series, &req.address).position.0 == 0 {
        return Err(ApiError::BadRequest("no open position to flip"));
    }

    let outcome = exchange.flip_position(series, req.address, TradeSource::Ui)?;
    broadcast_book(exchange.engine(series), state.channels_for(series));
    Ok((StatusCode::OK, Json(outcome)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Engine;
    use crate::ticks::PriceTick;
    use crate::types::Tenths;

    const START: Tenths = Tenths(1_000_000);

    fn addr(s: &str) -> Address {
        s.parse().unwrap()
    }

    fn tick(tenths: u16) -> PriceTick {
        PriceTick::from_tenths(tenths).unwrap()
    }

    #[tokio::test]
    async fn positions_route_serves_the_snapshot_and_404s_unknown_accounts() {
        use crate::api::SeriesChannels;
        use crate::pricing::feed::PriceFeed;
        use crate::series::SERIES;
        use std::sync::{Arc, Mutex};

        let user = addr("0xcccccccccccccccccccccccccccccccccccccccc");
        let mut x = Exchange::new(START, SERIES);
        x.register_user(user);
        x.get_or_create_account(user);
        // No manager behind the handle: this test never deploys.
        let (runtime, _rx) = crate::automation::runtime::runtime_channel();
        let state = AppState {
            exchange: Arc::new(Mutex::new(x)),
            channels: SeriesChannels::for_all_series(),
            price: PriceFeed::new(),
            runtime,
            registry: crate::registry::for_tests(),
            bus: crate::automation::signal_bus::SignalBus::new(crate::registry::for_tests(), None),
            templates: Arc::new(crate::templates::TemplateCatalogue::default()),
            deploys: crate::api::DeployKeys::default(),
            crowd: false,
            gemini: None,
        };
        let Json(snapshot) = get_positions(
            State(state.clone()),
            Path(user.to_string()),
            Query(super::super::SeriesQuery { series: Some(SeriesId(1)) }),
        )
        .await
        .unwrap();
        assert_eq!(snapshot.series, SeriesId(1));
        assert_eq!(snapshot.address, user);
        assert_eq!(snapshot.balance_tenths, START.0);
        assert_eq!(snapshot.series_positions.len(), SERIES.len());

        let missing = get_positions(
            State(state.clone()),
            Path("0xdddddddddddddddddddddddddddddddddddddddd".into()),
            Query(super::super::SeriesQuery::default()),
        )
        .await;
        assert!(matches!(missing, Err(ApiError::UnknownAccount)));
        let bad = get_positions(State(state), Path("nope".into()), Query(super::super::SeriesQuery::default())).await;
        assert!(matches!(bad, Err(ApiError::BadAddress)));
    }

    #[test]
    fn position_snapshot_json_is_superset_of_legacy() {
        let user = addr("0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        let mut engine = Engine::new(START);
        engine.get_or_create_account(user);
        let snapshot = build_position_snapshot(engine.exchange(), user, DEFAULT_SERIES).unwrap();
        let json = serde_json::to_value(&snapshot).unwrap();
        let obj = json.as_object().unwrap();
        for key in [
            "type", "as_of_unix_ms", "market_id", "address", "side", "size_shares",
            "signed_position", "avg_entry_yes_tenths", "cost_basis_tenths", "mark_yes_tenths",
            "mark_value_tenths", "unrealized_pnl_tenths", "realized_pnl_this_market_tenths",
            "return_pct", "balance_tenths", "locked_tenths", "flip_quote",
        ] {
            assert!(obj.contains_key(key), "legacy key `{key}` missing");
        }
        assert_eq!(obj["series"], "btc-5m");
        let rows = obj["series_positions"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["series"], "btc-5m");
        assert_eq!(rows[0]["signed_position"], 0);
    }

    #[test]
    fn open_position_uses_last_trade_mark_when_book_has_no_mid() {
        let maker = addr("0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let user = addr("0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        let other = addr("0xcccccccccccccccccccccccccccccccccccccccc");
        let mut engine = Engine::new(START);
        engine.get_or_create_account(maker);
        engine.get_or_create_account(user);
        engine.get_or_create_account(other);

        engine.place_limit(maker, Side::Ask, tick(500), Qty(10), TradeSource::Bot).unwrap();
        engine.place_market(user, Side::Bid, Qty(10), TradeSource::Ui).unwrap();
        engine.place_limit(maker, Side::Ask, tick(600), Qty(1), TradeSource::Bot).unwrap();
        engine.place_market(other, Side::Bid, Qty(1), TradeSource::Bot).unwrap();

        assert!(engine.book().mid_yes_tenths().is_none());
        let snapshot = build_position_snapshot(engine.exchange(), user, DEFAULT_SERIES).unwrap();

        assert_eq!(snapshot.mark_yes_tenths, Some(600));
        assert_eq!(snapshot.mark_value_tenths, Some(6000));
        assert_eq!(snapshot.unrealized_pnl_tenths, Some(1000));
        assert_eq!(snapshot.return_pct, Some(20.0));
    }

    #[test]
    fn open_position_uses_entry_mark_when_no_mid_or_recent_trade_exists() {
        let user = addr("0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        let mut engine = Engine::new(START);
        engine.get_or_create_account(user).usdc_balance = Tenths(995000);
        engine.set_position(
            user,
            crate::engine::PositionState {
                position: crate::types::Position(10),
                entry_cost_tenths: Tenths(5000),
                ..Default::default()
            },
        );

        assert!(engine.book().mid_yes_tenths().is_none());
        assert_eq!(engine.recent_trades(1).count(), 0);
        let snapshot = build_position_snapshot(engine.exchange(), user, DEFAULT_SERIES).unwrap();

        assert_eq!(snapshot.mark_yes_tenths, Some(500));
        assert_eq!(snapshot.mark_value_tenths, Some(5000));
        assert_eq!(snapshot.unrealized_pnl_tenths, Some(0));
        assert_eq!(snapshot.return_pct, Some(0.0));
    }
}
