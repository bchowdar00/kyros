//! REST handlers for the `/api/...` routes.
//!
//! Each handler takes the engine mutex, does its quick synchronous work,
//! drops the lock, and returns. Mutating handlers (`place_order`,
//! `cancel_order`) push a fresh book snapshot before dropping the lock
//! so subscribers see mutations in order.

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};

use crate::engine::PlaceOutcome;
use crate::engine::TradeSource;
use crate::engine::model::{Account, Fill, Order};
use crate::engine::{Exchange, Market, PositionState, RESOLVED_MARKETS_CAP};
use crate::ticks::PriceTick;
use crate::types::{Address, DEFAULT_SERIES, OrderId, Qty, SeriesId, Side, Tenths};

use super::{ApiError, AppState, BookLevel, broadcast_book};

// ────────────────────────────── Create account ──────────────────────────────

/// Curated palette of vibrant hex colors that read well on the dark theme.
const PROFILE_COLORS: &[&str] = &[
    "#ff6b6b", "#ffa94d", "#ffd43b", "#a9e34b", "#51cf66", "#20c997",
    "#22b8cf", "#4dabf7", "#748ffc", "#9775fa", "#da77f2", "#f06595",
];

/// Curated emoji set used as default profile avatars.
const PROFILE_EMOJIS: &[&str] = &[
    "🦊", "🐼", "🦄", "🐙", "🐳", "🐝", "🐢", "🦋", "🦖", "🐧",
    "🐯", "🦁", "🐸", "🐵", "🐶", "🐱", "🦉", "🦀", "🐠", "🦩",
    "🚀", "⚡", "🔥", "💎", "🌮", "🍕", "🎮", "🏆", "⭐", "🌈",
];

fn pick_profile_color() -> String {
    let idx = (rand::random::<u32>() as usize) % PROFILE_COLORS.len();
    PROFILE_COLORS[idx].to_string()
}

fn pick_profile_emoji() -> String {
    let idx = (rand::random::<u32>() as usize) % PROFILE_EMOJIS.len();
    PROFILE_EMOJIS[idx].to_string()
}

#[derive(Debug, Deserialize)]
pub struct CreateAccountRequest {
    pub username: String,
}

#[derive(Debug, Serialize)]
pub struct CreateAccountResponse {
    pub address: Address,
    pub username: String,
    pub color: String,
    pub emoji: String,
    pub usdc_balance: Tenths,
}

pub async fn create_account(
    State(state): State<AppState>,
    Json(req): Json<CreateAccountRequest>,
) -> Result<(StatusCode, Json<CreateAccountResponse>), ApiError> {
    let username = req.username.trim();
    if username.is_empty() {
        return Err(ApiError::BadRequest("username must not be empty"));
    }
    let username = username.to_string();
    let color = pick_profile_color();
    let emoji = pick_profile_emoji();

    let address = Address(rand::random::<[u8; 20]>());

    let mut exchange = state.exchange.lock().expect("exchange mutex poisoned");
    exchange.register_user(address);
    let acct = exchange.get_or_create_account(address);
    acct.username = Some(username.clone());
    acct.color = Some(color.clone());
    acct.emoji = Some(emoji.clone());
    let usdc_balance = acct.usdc_balance;

    Ok((
        StatusCode::CREATED,
        Json(CreateAccountResponse {
            address,
            color,
            emoji,
            usdc_balance,
            username,
        }),
    ))
}

// ─────────────────────────────── Place order ────────────────────────────────

/// `type: "limit"` requires `price` (raw tenths-of-cent); `type: "market"`
/// does not. Both require `address`, `side`, and `qty`.
///
/// TODO: gate this on an ETH-key signature once we add real wallets. For
/// now anyone may submit orders on any address's behalf — demo only.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum PlaceOrderRequest {
    Limit {
        address: Address,
        side: Side,
        price: u16,
        qty: u32,
        /// Target series; absent ⇒ the default series.
        #[serde(default)]
        series: Option<SeriesId>,
    },
    Market {
        address: Address,
        side: Side,
        qty: u32,
        #[serde(default)]
        series: Option<SeriesId>,
    },
}

impl PlaceOrderRequest {
    pub fn series(&self) -> SeriesId {
        match self {
            PlaceOrderRequest::Limit { series, .. } | PlaceOrderRequest::Market { series, .. } => {
                series.unwrap_or(DEFAULT_SERIES)
            }
        }
    }
}

pub async fn place_order(
    State(state): State<AppState>,
    Json(req): Json<PlaceOrderRequest>,
) -> Result<(StatusCode, Json<PlaceOutcome>), ApiError> {
    let series = req.series();
    let mut exchange = state.exchange.lock().expect("exchange mutex poisoned");
    let outcome = match req {
        PlaceOrderRequest::Limit { address, side, price, qty, .. } => {
            let pt = PriceTick::from_tenths(price).ok_or(ApiError::BadPrice(price))?;
            exchange.place_limit(series, address, side, pt, Qty(qty), TradeSource::Ui)?
        }
        PlaceOrderRequest::Market { address, side, qty, .. } => {
            exchange.place_market(series, address, side, Qty(qty), TradeSource::Ui)?
        }
    };
    // Push a fresh book snapshot under the lock so subscribers see this
    // mutation in order with everyone else's.
    broadcast_book(exchange.engine(series), state.channels_for(series));
    Ok((StatusCode::CREATED, Json(outcome)))
}

// ─────────────────────────────── Cancel order ───────────────────────────────

#[derive(Debug, Deserialize)]
pub struct CancelQuery {
    pub address: Address,
    /// Series to look in first; the other series are probed on a miss.
    #[serde(default)]
    pub series: Option<SeriesId>,
}

pub async fn cancel_order(
    State(state): State<AppState>,
    Path(id): Path<u64>,
    Query(q): Query<CancelQuery>,
) -> Result<Json<Order>, ApiError> {
    let mut exchange = state.exchange.lock().expect("exchange mutex poisoned");
    let preferred = q.series.unwrap_or(DEFAULT_SERIES);
    let (series, order) = exchange.cancel_any(preferred, q.address, OrderId(id))?;
    broadcast_book(exchange.engine(series), state.channels_for(series));
    Ok(Json(order))
}

// ────────────────────────────── Book snapshot ──────────────────────────────

#[derive(Debug, Deserialize)]
pub struct BookQuery {
    #[serde(default)]
    pub depth: Option<usize>,
    #[serde(default)]
    pub series: Option<SeriesId>,
}

#[derive(Debug, Serialize)]
pub struct BookSnapshot {
    pub bids: Vec<BookLevel>,
    pub asks: Vec<BookLevel>,
}

const DEFAULT_BOOK_DEPTH: usize = 20;
const MAX_BOOK_DEPTH: usize = 153;

pub async fn get_book(
    State(state): State<AppState>,
    Query(q): Query<BookQuery>,
) -> Json<BookSnapshot> {
    let depth = q.depth.unwrap_or(DEFAULT_BOOK_DEPTH).min(MAX_BOOK_DEPTH);
    let exchange = state.exchange.lock().expect("exchange mutex poisoned");
    let book = exchange.engine(q.series.unwrap_or(DEFAULT_SERIES)).book();
    let bids = book
        .iter_bids()
        .take(depth)
        .map(|(p, q)| BookLevel { price: p.tenths(), qty: q.0 })
        .collect();
    let asks = book
        .iter_asks()
        .take(depth)
        .map(|(p, q)| BookLevel { price: p.tenths(), qty: q.0 })
        .collect();
    Json(BookSnapshot { bids, asks })
}

// ──────────────────────────── Account lookup ────────────────────────────────

/// Wire shape of `GET /api/accounts/{address}`: every ledger field of the
/// account plus its position in the default series, flattened so the
/// JSON is exactly what it was when positions lived on the account.
#[derive(Debug, Clone, Serialize)]
pub struct AccountView {
    #[serde(flatten)]
    pub account: Account,
    #[serde(flatten)]
    pub position: PositionState,
}

pub fn build_account_view(exchange: &Exchange, address: &Address) -> Option<AccountView> {
    let account = exchange.account(address)?.clone();
    Some(AccountView {
        account,
        position: exchange.position(DEFAULT_SERIES, address),
    })
}

pub async fn get_account(
    State(state): State<AppState>,
    Path(address): Path<String>,
) -> Result<Json<AccountView>, ApiError> {
    let address: Address = address.parse()?;
    let exchange = state.exchange.lock().expect("exchange mutex poisoned");
    let view = build_account_view(&exchange, &address).ok_or(ApiError::UnknownAccount)?;
    Ok(Json(view))
}

// ────────────────────────────── Recent trades ──────────────────────────────

#[derive(Debug, Deserialize)]
pub struct TradesQuery {
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub series: Option<SeriesId>,
}

const DEFAULT_TRADES_LIMIT: usize = 50;
const MAX_TRADES_LIMIT: usize = 1000;

pub async fn get_trades(
    State(state): State<AppState>,
    Query(q): Query<TradesQuery>,
) -> Json<Vec<Fill>> {
    let limit = q.limit.unwrap_or(DEFAULT_TRADES_LIMIT).min(MAX_TRADES_LIMIT);
    let exchange = state.exchange.lock().expect("exchange mutex poisoned");
    Json(
        exchange
            .engine(q.series.unwrap_or(DEFAULT_SERIES))
            .recent_trades(limit)
            .copied()
            .collect(),
    )
}

// ─────────────────────────── Resolved-market history ───────────────────────

#[derive(Debug, Deserialize)]
pub struct MarketsHistoryQuery {
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub series: Option<SeriesId>,
}

/// Default page size for `GET /api/markets/history`. Matches the UI's
/// "last 15 rounds" indicator. Callers can request fewer or up to
/// [`RESOLVED_MARKETS_CAP`] more recent rounds.
pub const DEFAULT_MARKETS_HISTORY_LIMIT: usize = 15;

pub async fn get_markets_history(
    State(state): State<AppState>,
    Query(q): Query<MarketsHistoryQuery>,
) -> Json<Vec<Market>> {
    let limit = q
        .limit
        .unwrap_or(DEFAULT_MARKETS_HISTORY_LIMIT)
        .min(RESOLVED_MARKETS_CAP);
    let exchange = state.exchange.lock().expect("exchange mutex poisoned");
    Json(
        exchange
            .engine(q.series.unwrap_or(DEFAULT_SERIES))
            .resolved_markets(limit)
            .copied()
            .collect(),
    )
}

#[cfg(test)]
mod series_routing_tests {
    use super::*;
    use crate::api::SeriesChannels;
    use crate::engine::Exchange;
    use crate::pricing::feed::PriceFeed;
    use crate::series::SERIES;
    use crate::types::SeriesId;
    use axum::response::IntoResponse;
    use std::sync::{Arc, Mutex};

    const USER: &str = "0x1111111111111111111111111111111111111111";
    const BOT: &str = "0x2222222222222222222222222222222222222222";

    fn app_state() -> AppState {
        let mut x = Exchange::new(Tenths(1_000_000), SERIES);
        let user: Address = USER.parse().unwrap();
        let bot: Address = BOT.parse().unwrap();
        x.register_user(user);
        x.get_or_create_account(user);
        x.seed_bot_account_in(bot, Tenths(1_000_000), "bot".into(), None, None, SeriesId(1));
        for s in SeriesId::all() {
            x.open_first_market(s, 100_000.0, 0, s.duration_secs());
        }
        // No manager behind the handle: these tests never deploy.
        let (runtime, _rx) = crate::automation::runtime::runtime_channel();
        AppState {
            exchange: Arc::new(Mutex::new(x)),
            channels: SeriesChannels::for_all_series(),
            price: PriceFeed::new(),
            runtime,
            registry: crate::registry::for_tests(),
            bus: crate::automation::signal_bus::SignalBus::new(crate::registry::for_tests(), None),
            templates: Arc::new(crate::templates::TemplateCatalogue::default()),
            deploys: crate::api::DeployKeys::default(),
            crowd: false,
        }
    }

    #[tokio::test]
    async fn place_order_without_series_hits_default() {
        let state = app_state();
        let req: PlaceOrderRequest = serde_json::from_value(serde_json::json!({
            "type": "limit", "address": USER, "side": "bid", "price": 400, "qty": 2
        }))
        .unwrap();
        assert_eq!(req.series(), DEFAULT_SERIES);
        let (code, Json(out)) = place_order(State(state.clone()), Json(req)).await.unwrap();
        assert_eq!(code, StatusCode::CREATED);
        let x = state.exchange.lock().unwrap();
        assert!(x.engine(DEFAULT_SERIES).is_order_resting(out.order_id));
        assert!(x.engine(SeriesId(1)).book().is_empty());
    }

    #[tokio::test]
    async fn place_order_with_series_routes_to_that_book() {
        let state = app_state();
        let req: PlaceOrderRequest = serde_json::from_value(serde_json::json!({
            "type": "limit", "address": USER, "side": "bid", "price": 400, "qty": 2,
            "series": "btc-15m"
        }))
        .unwrap();
        let (_, Json(out)) = place_order(State(state.clone()), Json(req)).await.unwrap();
        let x = state.exchange.lock().unwrap();
        assert!(x.engine(SeriesId(1)).is_order_resting(out.order_id));
        assert!(x.engine(DEFAULT_SERIES).book().is_empty());
    }

    #[test]
    fn place_order_with_unknown_series_is_rejected_at_parse() {
        // The slug is validated by serde; axum turns the failure into a
        // 422 before the handler runs.
        let err = serde_json::from_value::<PlaceOrderRequest>(serde_json::json!({
            "type": "market", "address": USER, "side": "bid", "qty": 1, "series": "eth-5m"
        }))
        .unwrap_err();
        assert!(err.to_string().contains("unknown series"));
    }

    #[tokio::test]
    async fn bot_order_to_wrong_series_is_400_wrong_series() {
        let state = app_state();
        // The bot lives in series 1; an order without `series` targets 0.
        let req: PlaceOrderRequest = serde_json::from_value(serde_json::json!({
            "type": "limit", "address": BOT, "side": "bid", "price": 400, "qty": 1
        }))
        .unwrap();
        let err = place_order(State(state), Json(req)).await.unwrap_err();
        assert!(matches!(err, ApiError::SeriesMismatch { .. }));
        let resp = err.into_response();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"], "wrong_series");
        assert_eq!(json["detail"]["home"], "btc-15m");
        assert_eq!(json["detail"]["requested"], "btc-2m");
    }

    #[tokio::test]
    async fn cancel_probes_other_series() {
        let state = app_state();
        let user: Address = USER.parse().unwrap();
        let id = {
            let mut x = state.exchange.lock().unwrap();
            x.place_limit(SeriesId(1), user, Side::Bid, PriceTick::from_tenths(400).unwrap(), Qty(1), TradeSource::Ui)
                .unwrap()
                .order_id
        };
        let q = CancelQuery { address: user, series: None };
        let Json(order) = cancel_order(State(state.clone()), Path(id.0), Query(q)).await.unwrap();
        assert_eq!(order.id, id);
        assert!(state.exchange.lock().unwrap().engine(SeriesId(1)).book().is_empty());
    }
}

#[cfg(test)]
mod account_view_tests {
    use super::*;
    use crate::engine::{Engine, TradeSource};
    use crate::ticks::PriceTick;

    /// `GET /api/accounts/{address}` must keep the exact key set it had
    /// when positions lived on the account: the frontend is a regression
    /// test for this shape.
    #[test]
    fn account_view_json_matches_legacy_shape() {
        let user: Address = "0x1111111111111111111111111111111111111111".parse().unwrap();
        let bot: Address = "0x2222222222222222222222222222222222222222".parse().unwrap();
        let mut e = Engine::new(Tenths(1_000_000));
        e.register_user(user);
        e.get_or_create_account(user);
        e.get_or_create_account(bot);
        e.open_first_market(100_000.0, 0, 60);
        e.place_limit(bot, Side::Ask, PriceTick::from_tenths(500).unwrap(), Qty(3), TradeSource::Bot)
            .unwrap();
        e.place_market(user, Side::Bid, Qty(3), TradeSource::Ui).unwrap();
        e.resolve_and_open_next(100_500.0, 100_500.0, 60, 60);

        let view = build_account_view(e.exchange(), &user).unwrap();
        let json = serde_json::to_value(&view).unwrap();
        let obj = json.as_object().unwrap();
        let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "address",
                "color",
                "emoji",
                "entry_cost_tenths",
                "last_market",
                "lifetime_realized_pnl_tenths",
                "locked",
                "position",
                "realized_pnl_this_market",
                "usdc_balance",
                "username",
            ]
        );
        assert_eq!(obj["position"], 0);
        assert_eq!(obj["last_market"]["pnl_tenths"], 1500);
        assert_eq!(obj["last_market"]["series"], "btc-2m");
    }
}

#[cfg(test)]
mod markets_history_tests {
    use super::*;
    use crate::engine::{Engine, MarketStatus, Outcome};

    fn fresh_engine() -> Engine {
        Engine::new(Tenths(1_000_000))
    }

    fn resolve_n_rounds(engine: &mut Engine, n: usize) {
        // Open the first market at strike=100k, then resolve n times,
        // alternating yes/no by tweaking the close price relative to strike.
        engine.open_first_market(100_000.0, 0, 60);
        for i in 0..n {
            let close = if i % 2 == 0 { 100_500.0 } else { 99_500.0 };
            engine.resolve_and_open_next(close, 100_000.0, (i as u64 + 1) * 60, 60);
        }
    }

    #[test]
    fn default_limit_returns_up_to_fifteen_newest_first() {
        let mut e = fresh_engine();
        resolve_n_rounds(&mut e, 20);

        let collected: Vec<Market> = e
            .resolved_markets(DEFAULT_MARKETS_HISTORY_LIMIT)
            .copied()
            .collect();
        assert_eq!(collected.len(), DEFAULT_MARKETS_HISTORY_LIMIT);

        // Newest first — ids strictly decreasing.
        let ids: Vec<u32> = collected.iter().map(|m| m.id.0).collect();
        let mut sorted = ids.clone();
        sorted.sort_by(|a, b| b.cmp(a));
        assert_eq!(ids, sorted, "history should arrive newest-first");

        // Every market in the history is in the resolved state with a
        // numeric close price — exactly what the UI dot indicator needs.
        for m in &collected {
            match m.status {
                MarketStatus::Resolved { outcome, close } => {
                    assert!(matches!(close, crate::engine::RoundClose::Btc { close_price_btc_usd } if close_price_btc_usd.is_finite()));
                    assert!(matches!(outcome, Outcome::Yes | Outcome::No));
                }
                MarketStatus::Open => panic!("history must not contain open markets"),
            }
        }
    }

    #[test]
    fn limit_is_clamped_to_resolved_cap() {
        let mut e = fresh_engine();
        resolve_n_rounds(&mut e, RESOLVED_MARKETS_CAP + 10);
        // Even an oversized limit yields at most the cap, never more.
        let count = e.resolved_markets(usize::MAX).count();
        assert_eq!(count, RESOLVED_MARKETS_CAP);
    }

    #[test]
    fn empty_history_returns_empty_vec() {
        let e = fresh_engine();
        let collected: Vec<Market> = e
            .resolved_markets(DEFAULT_MARKETS_HISTORY_LIMIT)
            .copied()
            .collect();
        assert!(collected.is_empty());
    }
}

// ─────────────────────────────── Registry ───────────────────────────────

/// `GET /api/registry` — the venue registry generated at start-up: every
/// series and every signal, in the strategy compiler's format.
pub async fn get_registry(State(state): State<AppState>) -> Json<strategy_compiler::Registry> {
    Json((*state.registry).clone())
}
