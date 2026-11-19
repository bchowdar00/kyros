//! HTTP / WebSocket surface.
//!
//! Engine access goes through `Arc<Mutex<Exchange>>` — every handler
//! takes the lock, does its quick synchronous work on one series' engine,
//! and drops it. Engine operations are microsecond-scale, so this is fine.
//!
//! Layout:
//!
//! * `mod` (this file) — [`AppState`], [`ApiError`], shared message
//!   types ([`BookSnapshotMessage`], [`TradesMessage`]), and the
//!   broadcast helpers.
//! * [`handlers`] — REST endpoints (`/api/...`).
//! * [`ws`] — WebSocket upgrade handlers (`/ws/...`).
//! * [`trades`] — background batcher that polls the engine's fill
//!   buffer and broadcasts per-taker-order trade rows.
//!
//! All error responses share a consistent JSON shape:
//! `{"error": "<code>", "message": "<human>", "detail": <optional object>}`.

use std::sync::{Arc, Mutex};

use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc};

use crate::automation::runtime::RuntimeHandle;
use crate::engine::{CancelError, Exchange, MarketEngine, OrderError, StrategyError, SubAccountError};
use crate::pricing::feed::PriceFeed;
use crate::series::SERIES;
use crate::types::{Address, AddressParseError, DEFAULT_SERIES, SeriesId, Tenths};
use mid_history::{MidHistory, MidPoint};

pub mod accounts;
pub mod accounts_ws;
pub mod handlers;
pub mod health;
pub mod mid_history;
pub mod positions;
pub mod rounds;
pub mod series;
pub mod signals;
pub mod signals_ws;
pub mod stats;
pub mod strategy;
pub mod templates;
pub mod trades;
pub mod user_trades;
pub mod writer_meter;
pub mod ws;

pub use handlers::{
    cancel_order, create_account, get_account, get_book, get_markets_history, get_registry,
    get_trades, place_order,
};
pub use mid_history::get_mid_history;
pub use stats::get_market_stats;
pub use positions::{close_position, flip_position, get_positions, positions_ws};
pub use rounds::get_rounds_audit;
pub use series::list_series;
pub use signals::{get_signals_history, list_signals};
pub use signals_ws::signals_ws;
pub use strategy::{
    compile_strategy, create_strategy, delete_strategy, deploy_template, fund_strategy,
    get_strategy, get_strategy_schema, list_strategies, update_strategy,
};
pub use templates::{get_template, list_templates};
pub use accounts_ws::accounts_ws;
pub use trades::spawn_trades_batcher;
pub use user_trades::get_user_trades;
pub use ws::{book_ws, price_history, price_ws, trades_ws};

// ────────────────────────────── App state ──────────────────────────────────

/// The broadcast channels of one series. Per series rather than one
/// tagged global channel: the positions and accounts sockets rebuild
/// their snapshot on every book broadcast, and each market maker
/// requotes ~10×/s, so a shared channel would double their wakeups for
/// frames they would discard.
#[derive(Clone)]
pub struct SeriesChannels {
    pub series: SeriesId,
    pub book_tx: broadcast::Sender<BookSnapshotMessage>,
    pub trades_tx: broadcast::Sender<TradesMessage>,
    /// The round's YES-midpoint history, fed by [`broadcast_book`] and
    /// served to `/ws/book` joiners and `GET /api/book/mid-history`.
    /// Shared across clones so every broadcaster appends to one buffer.
    pub mid_history: Arc<Mutex<MidHistory>>,
}

impl SeriesChannels {
    /// One channel pair per entry of the series table, indexed by
    /// `SeriesId.0`.
    pub fn for_all_series() -> Arc<Vec<SeriesChannels>> {
        Arc::new(
            SERIES
                .iter()
                .map(|spec| SeriesChannels {
                    series: spec.id,
                    book_tx: broadcast::channel(BOOK_BROADCAST_CAP).0,
                    trades_tx: broadcast::channel(TRADES_BROADCAST_CAP).0,
                    mid_history: Arc::new(Mutex::new(MidHistory::default())),
                })
                .collect(),
        )
    }
}

/// Shared state handed to every handler. Cheap to clone — everything is
/// an `Arc` or a `broadcast::Sender`.
#[derive(Clone)]
pub struct AppState {
    pub exchange: Arc<Mutex<Exchange>>,
    /// Broadcast channels per series, indexed by `SeriesId.0`.
    pub channels: Arc<Vec<SeriesChannels>>,
    pub price: Arc<PriceFeed>,
    /// The strategy runtime manager. The strategy HTTP handlers start,
    /// pause, resume, stop and query deployments through it after every
    /// engine mutation, so running deployments stay in sync with the
    /// stored records.
    pub runtime: RuntimeHandle,
    /// The venue registry generated at start-up (`registry.rs`).
    pub registry: Arc<strategy_compiler::Registry>,
    /// The signal bus: latest frame and quality per signal.
    pub bus: Arc<crate::automation::signal_bus::SignalBus>,
    /// The public strategy templates, compiled at boot (`templates.rs`).
    pub templates: Arc<crate::templates::TemplateCatalogue>,
    /// Idempotency keys the template deploy route has honoured.
    pub deploys: DeployKeys,
    /// The Gemini link, when the rounds follow Gemini's windows; `None`
    /// in tests that run rounds on their own clock.
    pub gemini: Option<crate::gemini::GeminiState>,
}

/// The `Idempotency-Key` headers the template deploy route has seen,
/// keyed by parent account so one user's key cannot replay another's:
/// `(parent, key)` → the sub-account that deploy created. A repeat with
/// the same key answers with that deployment instead of funding a second
/// one, which is what makes a client retry of a money-moving POST safe.
/// Process-local, like every other piece of exchange state.
#[derive(Clone, Default)]
pub struct DeployKeys(Arc<Mutex<std::collections::HashMap<(Address, String), Address>>>);

impl DeployKeys {
    pub fn get(&self, parent: Address, key: &str) -> Option<Address> {
        self.0.lock().expect("deploy keys poisoned").get(&(parent, key.to_string())).copied()
    }

    pub fn insert(&self, parent: Address, key: String, child: Address) {
        self.0.lock().expect("deploy keys poisoned").insert((parent, key), child);
    }
}

impl AppState {
    pub fn channels_for(&self, series: SeriesId) -> &SeriesChannels {
        &self.channels[series.0 as usize]
    }

    /// Resolve an optional wire `series` to a running series, rejecting
    /// slugs the exchange does not run.
    pub fn series_or_default(&self, series: Option<SeriesId>) -> SeriesId {
        series.unwrap_or(DEFAULT_SERIES)
    }
}

/// One wake-up stream over every series' book and trades broadcasts, for
/// the per-account sockets whose snapshot spans all series. Each event is
/// a unit; the receiver rebuilds and dedups. Forwarders exit when the
/// returned receiver is dropped.
pub fn subscribe_all_series(channels: &[SeriesChannels]) -> mpsc::Receiver<()> {
    let (tx, rx) = mpsc::channel::<()>(64);
    for ch in channels {
        forward_wakeups(ch.book_tx.subscribe(), tx.clone());
        forward_wakeups(ch.trades_tx.subscribe(), tx.clone());
    }
    rx
}

fn forward_wakeups<T: Clone + Send + 'static>(
    mut rx: broadcast::Receiver<T>,
    tx: mpsc::Sender<()>,
) {
    tokio::spawn(async move {
        // A lagged receiver is still a wake-up: the socket rebuilds from
        // live state, so dropped frames cost nothing.
        while !matches!(rx.recv().await, Err(broadcast::error::RecvError::Closed)) {
            if tx.send(()).await.is_err() {
                break;
            }
        }
    });
}

/// `?series=` query parameter shared by the series-scoped routes.
#[derive(Debug, Clone, Copy, Default, serde::Deserialize)]
pub struct SeriesQuery {
    #[serde(default)]
    pub series: Option<SeriesId>,
}

// ─────────────────────────── Broadcast constants ───────────────────────────

/// Capacity of the book broadcast channel. Lagged subscribers receive a
/// `Lagged(n)` and we recover by pushing them a fresh snapshot.
pub const BOOK_BROADCAST_CAP: usize = 256;

/// Depth used both for the WS snapshot on connect and for every broadcast.
/// Same depth on the wire keeps the frontend's data model uniform.
pub const BOOK_WS_DEPTH: usize = 20;

/// Capacity of the trades broadcast channel. Lagged subscribers get a
/// `Lagged(n)` and we recover by pushing them a fresh snapshot.
pub const TRADES_BROADCAST_CAP: usize = 256;

// ────────────────────────── Shared wire types ──────────────────────────────

/// Price-tick level on the book (tenths-of-cent + open contract qty).
#[derive(Debug, Clone, Serialize)]
pub struct BookLevel {
    pub price: u16,
    pub qty: u32,
}

/// Wire shape sent over `/ws/book`. Mirrors the REST `GET /api/book`
/// payload with a `type` discriminator so clients can dispatch
/// consistently with the price feed.
#[derive(Debug, Clone, Serialize)]
pub struct BookSnapshotMessage {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub series: SeriesId,
    /// The open round, so a client can tell a new round's first frame
    /// from a requote and restart its midpoint line. `None` before the
    /// first round opens.
    pub market_id: Option<u32>,
    /// The YES midpoint this book implies for the probability chart
    /// (`Book::chart_mid_yes_tenths`); `None` on an empty book.
    pub mid: Option<u16>,
    /// The round's midpoint history, oldest first. Only on the frame a
    /// `/ws/book` client receives when it joins (and after a lag
    /// recovery); broadcast frames leave it out and carry `mid` alone.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mid_history: Option<Vec<MidPoint>>,
    pub bids: Vec<BookLevel>,
    pub asks: Vec<BookLevel>,
}

pub fn build_book_snapshot_message(engine: &MarketEngine, depth: usize) -> BookSnapshotMessage {
    let book = engine.book();
    BookSnapshotMessage {
        kind: "snapshot",
        series: engine.series(),
        market_id: engine.current_market().map(|m| m.id.0),
        mid: book.chart_mid_yes_tenths(),
        mid_history: None,
        bids: book
            .iter_bids()
            .take(depth)
            .map(|(p, q)| BookLevel {
                price: p.tenths(),
                qty: q.0,
            })
            .collect(),
        asks: book
            .iter_asks()
            .take(depth)
            .map(|(p, q)| BookLevel {
                price: p.tenths(),
                qty: q.0,
            })
            .collect(),
    }
}

/// Build a snapshot from current engine state and push it to subscribers.
/// Callers should hold the engine mutex while invoking this so snapshots
/// observed by subscribers are linearized with respect to mutations.
/// Also the one place the series' midpoint history is recorded, so the
/// chart sees exactly the books the sockets see.
pub fn broadcast_book(engine: &MarketEngine, channels: &SeriesChannels) {
    channels
        .mid_history
        .lock()
        .expect("mid history mutex poisoned")
        .record(engine, unix_now_ms());
    let snapshot = build_book_snapshot_message(engine, BOOK_WS_DEPTH);
    let _ = channels.book_tx.send(snapshot);
}

fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_millis() as u64
}

/// Identity payload included with each trade so the activity feed can
/// render an avatar (color + emoji) and display name without a separate
/// account lookup round-trip.
#[derive(Debug, Clone, Serialize)]
pub struct TradeIdentity {
    /// The address that actually traded (may be a strategy sub-account).
    pub address: Address,
    /// The top-level user behind `address`. Equals `address` for
    /// top-level accounts and bots. Clients should match "is this me"
    /// against this field.
    pub root_address: Address,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub emoji: Option<String>,
}

/// Direction the taker (the order that crossed) was on, projected onto
/// the YES contract. Buying YES = "yes"; selling YES (≡ buying NO) = "no".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TradeSide {
    Yes,
    No,
}

/// One taker order's entire fill, collapsed across however many makers
/// it walked through. `price` is the volume-weighted average across all
/// underlying fills so the row reflects what the taker actually paid.
#[derive(Debug, Clone, Serialize)]
pub struct TradeRow {
    pub series: SeriesId,
    pub price: u16,
    pub qty: u32,
    pub ts: u64,
    pub side: TradeSide,
    pub taker: TradeIdentity,
    pub maker: TradeIdentity,
}

#[derive(Debug, Clone, Serialize)]
pub struct TradesMessage {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub trades: Vec<TradeRow>,
}

// ────────────────────────────── Error type ─────────────────────────────────

/// Error type returned by handlers. Implements `IntoResponse` directly so
/// handlers can `?` engine errors after a `From` conversion.
#[derive(Debug)]
pub enum ApiError {
    BadRequest(&'static str),
    BadAddress,
    BadPrice(u16),
    UnknownAccount,
    UnknownOrder,
    NotOwner,
    ZeroQuantity,
    InsufficientBalance { required: Tenths, available: Tenths },
    NoLiquidity,
    /// The round has reached its close and is waiting on its result.
    MarketClosed,
    StrategyNotFound,
    /// No public template with that id is listed on the series.
    TemplateNotFound,
    InvalidStrategy(String),
    AllocationTooSmall { min: Tenths },
    TooManySubAccounts { max: usize },
    NotSubAccount,
    /// A bot / strategy account tried to trade outside its home series.
    SeriesMismatch { home: SeriesId, requested: SeriesId },
    UnknownSeries(String),
    /// The strategy compiler refused the document. Carries every
    /// diagnostic (errors and warnings) so an authoring loop can repair
    /// the document path by path.
    CompileFailed(Vec<strategy_compiler::Diagnostic>),
    /// Signal ids that do not apply to the requested series.
    UnknownSignals { series: SeriesId, ids: Vec<String> },
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code, message, detail): (_, &str, String, Option<Value>) = match self {
            ApiError::BadRequest(m) => {
                (StatusCode::BAD_REQUEST, "bad_request", m.to_string(), None)
            }
            ApiError::BadAddress => (
                StatusCode::BAD_REQUEST,
                "bad_address",
                "could not parse address".into(),
                None,
            ),
            ApiError::BadPrice(p) => (
                StatusCode::BAD_REQUEST,
                "bad_price",
                format!("price {} is not on the tick grid", p),
                None,
            ),
            ApiError::UnknownAccount => (
                StatusCode::NOT_FOUND,
                "unknown_account",
                "account does not exist".into(),
                None,
            ),
            ApiError::UnknownOrder => (
                StatusCode::NOT_FOUND,
                "unknown_order",
                "order not on the book".into(),
                None,
            ),
            ApiError::NotOwner => (
                StatusCode::FORBIDDEN,
                "not_owner",
                "order belongs to a different account".into(),
                None,
            ),
            ApiError::ZeroQuantity => (
                StatusCode::BAD_REQUEST,
                "zero_quantity",
                "order qty must be positive".into(),
                None,
            ),
            ApiError::InsufficientBalance {
                required,
                available,
            } => (
                StatusCode::BAD_REQUEST,
                "insufficient_balance",
                "insufficient balance".into(),
                Some(json!({"required": required.0, "available": available.0})),
            ),
            ApiError::NoLiquidity => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "no_liquidity",
                "no liquidity on opposite side".into(),
                None,
            ),
            ApiError::MarketClosed => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "market_closed",
                "this round is closed to new orders; the next one opens shortly".into(),
                None,
            ),
            ApiError::StrategyNotFound => (
                StatusCode::NOT_FOUND,
                "strategy_not_found",
                "strategy not found on this account".into(),
                None,
            ),
            ApiError::TemplateNotFound => (
                StatusCode::NOT_FOUND,
                "template_not_found",
                "no strategy template with that id is listed on this series".into(),
                None,
            ),
            ApiError::InvalidStrategy(m) => {
                (StatusCode::BAD_REQUEST, "invalid_strategy", m, None)
            }
            ApiError::AllocationTooSmall { min } => (
                StatusCode::BAD_REQUEST,
                "allocation_too_small",
                format!("allocation must be at least {} tenths", min.0),
                Some(json!({"min": min.0})),
            ),
            ApiError::TooManySubAccounts { max } => (
                StatusCode::BAD_REQUEST,
                "too_many_strategies",
                format!("at most {max} strategies per account"),
                Some(json!({"max": max})),
            ),
            ApiError::NotSubAccount => (
                StatusCode::BAD_REQUEST,
                "not_sub_account",
                "address is not a strategy sub-account of this account".into(),
                None,
            ),
            ApiError::SeriesMismatch { home, requested } => (
                StatusCode::BAD_REQUEST,
                "wrong_series",
                format!("account is bound to series {home} but the request targets {requested}"),
                Some(json!({"home": home, "requested": requested})),
            ),
            ApiError::UnknownSeries(slug) => (
                StatusCode::BAD_REQUEST,
                "unknown_series",
                format!("unknown series `{slug}`"),
                None,
            ),
            ApiError::CompileFailed(diagnostics) => {
                let errors = diagnostics
                    .iter()
                    .filter(|d| d.severity == strategy_compiler::validate::Severity::Error)
                    .count();
                (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "compile_failed",
                    format!("strategy did not compile: {errors} error(s)"),
                    Some(json!({
                        "diagnostics": diagnostics,
                        "rendered": diagnostics.iter().map(ToString::to_string).collect::<Vec<_>>(),
                    })),
                )
            }
            ApiError::UnknownSignals { series, ids } => (
                StatusCode::BAD_REQUEST,
                "unknown_signals",
                format!("not signals of {}: {}", series.slug(), ids.join(", ")),
                Some(json!({"series": series, "ids": ids})),
            ),
        };
        let mut body = json!({"error": code, "message": message});
        if let Some(d) = detail {
            body.as_object_mut()
                .expect("object")
                .insert("detail".to_string(), d);
        }
        (status, Json(body)).into_response()
    }
}

impl From<OrderError> for ApiError {
    fn from(e: OrderError) -> Self {
        match e {
            OrderError::ZeroQuantity => ApiError::ZeroQuantity,
            OrderError::UnknownAccount => ApiError::UnknownAccount,
            OrderError::InsufficientBalance {
                required,
                available,
            } => ApiError::InsufficientBalance {
                required,
                available,
            },
            OrderError::NoLiquidity => ApiError::NoLiquidity,
            OrderError::MarketClosed => ApiError::MarketClosed,
            OrderError::SeriesMismatch { home, requested } => {
                ApiError::SeriesMismatch { home, requested }
            }
            // Only strategy orders set these flags; UI orders never see them.
            OrderError::PostOnlyWouldCross => {
                ApiError::BadRequest("post-only order would cross the book")
            }
            OrderError::ReduceOnlyNoPosition => {
                ApiError::BadRequest("reduce-only order has no position to reduce")
            }
        }
    }
}

impl From<CancelError> for ApiError {
    fn from(e: CancelError) -> Self {
        match e {
            CancelError::UnknownOrder => ApiError::UnknownOrder,
            CancelError::NotOwner => ApiError::NotOwner,
        }
    }
}

impl From<AddressParseError> for ApiError {
    fn from(_: AddressParseError) -> Self {
        ApiError::BadAddress
    }
}

impl From<SubAccountError> for ApiError {
    fn from(e: SubAccountError) -> Self {
        match e {
            SubAccountError::UnknownParent => ApiError::UnknownAccount,
            SubAccountError::ParentNotUser | SubAccountError::ParentIsSubAccount => {
                ApiError::BadRequest("strategies can only be created on a top-level user account")
            }
            SubAccountError::AllocationTooSmall { min } => ApiError::AllocationTooSmall { min },
            SubAccountError::InsufficientBalance { required, available } => {
                ApiError::InsufficientBalance { required, available }
            }
            SubAccountError::TooManySubAccounts { max } => ApiError::TooManySubAccounts { max },
            SubAccountError::UnknownSubAccount => ApiError::StrategyNotFound,
            SubAccountError::NotOwner => ApiError::NotSubAccount,
        }
    }
}

impl From<StrategyError> for ApiError {
    fn from(e: StrategyError) -> Self {
        match e {
            StrategyError::AccountNotFound => ApiError::UnknownAccount,
            StrategyError::StrategyNotFound => ApiError::StrategyNotFound,
            StrategyError::InvalidStrategy(msg) => ApiError::InvalidStrategy(msg),
        }
    }
}

#[cfg(test)]
mod order_error_tests {
    use super::*;

    #[tokio::test]
    async fn an_order_into_a_closed_round_answers_422_market_closed() {
        let response = ApiError::from(OrderError::MarketClosed).into_response();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let bytes = axum::body::to_bytes(response.into_body(), 4096).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"], "market_closed");
        assert!(body["message"].as_str().unwrap().contains("closed"));
    }
}
