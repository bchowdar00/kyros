//! Account-scoped strategy CRUD, one sub-account per strategy.
//!
//! Every strategy lives on its own sub-account (see
//! [`crate::engine::sub_accounts`]). The strategy `id` on the wire is
//! the sub-account's address. The engine stores the compiled record; the
//! runtime manager (`AppState.runtime`) runs it. Handlers take the engine
//! lock synchronously for engine work (microseconds) and talk to the
//! runtime outside the lock.
//!
//! Routes (all under `/api/accounts/{parent}`):
//!
//! * `GET    /strategies`            — list summaries
//! * `POST   /strategies`            — compile, create sub-account, deploy
//! * `GET    /strategies/{id}`       — full detail, with runtime status
//! * `PATCH  /strategies/{id}`       — rename / pause / resume
//! * `DELETE /strategies/{id}`       — stop the deployment, close, sweep cash home
//! * `POST   /strategies/{id}/fund`  — move cash parent ↔ child

use std::collections::BTreeMap;
use std::str::FromStr;
use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::Value as Json_;
use strategy_compiler::ir::{Envelope, Strategy};

use super::compile::{compile_for_venue, params_to_values};
use crate::api::accounts::{AccountKind, AccountRow, build_account_row};
use crate::api::{ApiError, AppState};
use crate::automation::runtime::{DeploymentSpec, DeploymentStatus, next_deploy_epoch};
use crate::automation::value::parse_decimal;
use crate::engine::{DeployedStrategy, Exchange, StrategyPatch, StrategyStatus};
use crate::pricing::feed::unix_now_ms;
use crate::types::{Address, DEFAULT_SERIES, SeriesId, Tenths};

// ───────────────────────────── Wire shapes ────────────────────────────────

/// Per-strategy row for lists and the accounts stream: the shared
/// [`AccountRow`] restricted to `kind == Strategy`.
pub type StrategySummary = AccountRow;

/// The stored record as the API shows it: everything but the compiled
/// program (which is derivable from `source` and `envelope`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StrategyRecordView {
    pub name: String,
    pub status: StrategyStatus,
    pub created_at_ms: u64,
    pub series: SeriesId,
    pub strategy_id: String,
    pub deploy_epoch: u64,
    pub deployment_id: String,
    pub envelope: Envelope,
    pub params: BTreeMap<String, Json_>,
    pub source: Strategy,
    /// The public template this was deployed from; absent for an
    /// authored document.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub template_id: Option<String>,
    /// `true` when the deployment runs the template's Inverse document.
    pub inverse: bool,
}

impl From<&DeployedStrategy> for StrategyRecordView {
    fn from(r: &DeployedStrategy) -> Self {
        StrategyRecordView {
            name: r.name.clone(),
            status: r.status,
            created_at_ms: r.created_at_ms,
            series: r.series,
            strategy_id: r.strategy_id.clone(),
            deploy_epoch: r.deploy_epoch,
            deployment_id: r.deployment_id.clone(),
            envelope: r.envelope.clone(),
            params: r.params.clone(),
            source: r.source.clone(),
            template_id: r.template_id.clone(),
            inverse: r.inverse,
        }
    }
}

/// One live order the runtime placed, decimals as strings.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LiveOrderView {
    pub client_order_id: String,
    pub intent_id: String,
    pub tag: String,
    pub leg: String,
    pub outcome: Option<String>,
    pub side: strategy_compiler::ir::Side,
    pub order_type: strategy_compiler::ir::OrderType,
    pub tif: strategy_compiler::ir::Tif,
    pub price: Option<String>,
    pub requested: String,
    pub remaining: String,
    pub filled: String,
    pub venue_order_id: Option<u64>,
    pub market_id: Option<u32>,
}

/// The deployment as the runtime reports it. Absent when the runtime has
/// no resident deployment for the record (it failed and was reaped).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RuntimeView {
    pub current_leg: String,
    pub armed_triggers: Vec<String>,
    pub paused: bool,
    pub stopping: bool,
    pub failure_reason: Option<String>,
    pub activations: u64,
    /// Queued activations absorbed into a later one with the same cause.
    pub coalesced: u64,
    /// Submissions refused by the per-second order-rate gate.
    pub rate_rejects: u64,
    pub live_orders: Vec<LiveOrderView>,
    /// Scalar state slots, canonical string form.
    pub state: BTreeMap<String, String>,
    /// Effective params (defaults merged with the deploy-time overrides).
    pub params: BTreeMap<String, String>,
    pub log: Vec<String>,
}

impl From<DeploymentStatus> for RuntimeView {
    fn from(s: DeploymentStatus) -> Self {
        RuntimeView {
            current_leg: s.current_leg,
            armed_triggers: s.armed_triggers,
            paused: s.paused,
            stopping: s.stopping,
            failure_reason: s.failure_reason,
            activations: s.activations,
            coalesced: s.coalesced,
            rate_rejects: s.rate_rejects,
            live_orders: s
                .live_orders
                .into_iter()
                .map(|o| LiveOrderView {
                    client_order_id: o.client_order_id.to_string(),
                    intent_id: o.intent_id,
                    tag: o.tag,
                    leg: o.leg,
                    outcome: o.outcome,
                    side: o.side,
                    order_type: o.order_type,
                    tif: o.tif,
                    price: o.price.map(|p| p.normalize().to_string()),
                    requested: o.requested.normalize().to_string(),
                    remaining: o.remaining.normalize().to_string(),
                    filled: o.filled.normalize().to_string(),
                    venue_order_id: o.venue_order_id,
                    market_id: o.market_id,
                })
                .collect(),
            state: s.state.iter().map(|(k, v)| (k.clone(), v.canonical())).collect(),
            params: s.params.iter().map(|(k, v)| (k.clone(), v.canonical())).collect(),
            log: s.log,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StrategyDetail {
    #[serde(flatten)]
    pub summary: StrategySummary,
    pub strategy: StrategyRecordView,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime: Option<RuntimeView>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StrategyListResponse {
    pub strategies: Vec<StrategySummary>,
}

#[derive(Debug, Deserialize)]
pub struct CreateStrategyRequest {
    /// Cash to move from the parent into the new sub-account.
    pub allocation_tenths: i64,
    #[serde(default)]
    pub name: Option<String>,
    /// The authored IR document.
    pub strategy: Strategy,
    /// The bankroll envelope the program is compiled and risk-gated
    /// against. Its `allocation` may not exceed `allocation_tenths`.
    pub envelope: Envelope,
    /// Overrides for the document's declared params.
    #[serde(default)]
    pub params: BTreeMap<String, Json_>,
}

/// Re-use `StrategyPatch` directly as the wire shape — both fields are
/// `Option`, default-`None` semantics match a partial-update PATCH.
pub type UpdateStrategyRequest = StrategyPatch;

#[derive(Debug, Deserialize)]
pub struct FundStrategyRequest {
    /// Positive: parent → sub-account. Negative: sub-account → parent.
    pub amount_tenths: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DeleteStrategyResponse {
    /// Cash returned to the parent now. Zero if the position could not
    /// be flattened; the remainder is swept at market resolution.
    pub swept_tenths: i64,
    pub cancelled_orders: u32,
    pub residual_position: i64,
    /// `true` when the sub-account is gone immediately.
    pub removed: bool,
}

// ─────────────────────────────── Builders ─────────────────────────────────

/// Build a summary for the sub-account at `child`. `None` if the
/// account is missing, is not a sub-account, or carries no strategy
/// (an orphan awaiting the resolution sweep).
pub fn build_strategy_summary(engine: &Exchange, child: Address) -> Option<StrategySummary> {
    let row = build_account_row(engine, child)?;
    (row.kind == AccountKind::Strategy).then_some(row)
}

/// Every live strategy under `parent`, in creation order.
pub fn build_strategy_list(engine: &Exchange, parent: Address) -> Vec<StrategySummary> {
    engine
        .sub_accounts_of(&parent)
        .iter()
        .filter_map(|c| build_strategy_summary(engine, *c))
        .collect()
}

/// The stored half of a detail, read under the lock. The runtime half is
/// fetched afterwards by [`detail_with_runtime`].
fn build_stored_detail(engine: &Exchange, child: Address) -> Option<(StrategyDetail, String)> {
    let summary = build_strategy_summary(engine, child)?;
    let record = engine.account(&child)?.strategy.as_ref()?;
    let deployment_id = record.deployment_id.clone();
    Some((StrategyDetail { summary, strategy: record.into(), runtime: None }, deployment_id))
}

/// Read the stored detail under the lock, then attach the runtime status
/// outside it.
async fn detail_with_runtime(state: &AppState, child: Address) -> Option<StrategyDetail> {
    let (mut detail, deployment_id) = {
        let engine = state.exchange.lock().expect("exchange mutex poisoned");
        build_stored_detail(&engine, child)?
    };
    detail.runtime = state.runtime.status(&deployment_id).await.map(RuntimeView::from);
    Some(detail)
}

/// The detail of `child` if `parent` still owns it as a live strategy.
pub(super) async fn detail_for(state: &AppState, parent: Address, child: Address) -> Option<StrategyDetail> {
    {
        let engine = state.exchange.lock().expect("exchange mutex poisoned");
        resolve_child(&engine, parent, child).ok()?;
    }
    detail_with_runtime(state, child).await
}

/// Resolve `{parent, id}` to a child the parent owns and that still
/// carries a strategy. Maps every miss to `strategy_not_found` so the
/// route does not leak whether an unrelated address exists.
fn resolve_child(engine: &Exchange, parent: Address, id: Address) -> Result<Address, ApiError> {
    let acct = engine.account(&id).ok_or(ApiError::StrategyNotFound)?;
    if acct.parent != Some(parent) || acct.strategy.is_none() {
        return Err(ApiError::StrategyNotFound);
    }
    Ok(id)
}

/// The deployment id of the strategy on `child`, or `strategy_not_found`.
fn deployment_id_of(engine: &Exchange, child: Address) -> Result<String, ApiError> {
    engine
        .account(&child)
        .and_then(|a| a.strategy.as_ref())
        .map(|s| s.deployment_id.clone())
        .ok_or(ApiError::StrategyNotFound)
}

/// Tenths of a cent as dollars.
pub(super) fn tenths_to_dollars(t: i64) -> Decimal {
    Decimal::new(t, 3)
}

/// The money-moving half of a deploy, shared by the authored route and
/// the template route: create a sub-account funded with `allocation`
/// from `parent`, store `record` on it, and start the deployment. A
/// runtime start failure closes the sub-account again so nothing is
/// left funded without a running strategy. Everything that can fail
/// without touching the ledger (compile, envelope checks, param typing)
/// happens before this is called.
pub(super) async fn fund_and_start(
    state: &AppState,
    parent: Address,
    allocation: Tenths,
    record: DeployedStrategy,
    params: BTreeMap<String, crate::automation::value::Value>,
) -> Result<StrategyDetail, ApiError> {
    let series = record.series;
    let child = {
        let mut engine = state.exchange.lock().expect("exchange mutex poisoned");
        let child = engine.create_sub_account_in(parent, allocation, series)?;
        if let Err(e) = engine.deploy_strategy_in(child, record.clone()) {
            // Cannot happen after a fresh child, but never leave money
            // stranded if it somehow does.
            let _ = engine.close_sub_account(parent, child);
            return Err(e.into());
        }
        child
    };

    let spec = DeploymentSpec {
        program: record.program.clone(),
        deploy_epoch: record.deploy_epoch,
        sub_account: child,
        series,
        params,
    };
    if let Err(e) = state.runtime.start(spec).await {
        tracing::error!(?child, error = %e, "strategy deployment failed to start; closing sub-account");
        let mut engine = state.exchange.lock().expect("exchange mutex poisoned");
        let _ = engine.close_sub_account(parent, child);
        return Err(ApiError::InvalidStrategy(format!("deployment failed to start: {e}")));
    }

    Ok(detail_with_runtime(state, child).await.expect("just deployed"))
}

// ─────────────────────────────── Handlers ─────────────────────────────────

/// `GET /api/accounts/{parent}/strategies`
pub async fn list_strategies(
    State(state): State<AppState>,
    Path(parent): Path<String>,
) -> Result<Json<StrategyListResponse>, ApiError> {
    let parent = Address::from_str(&parent)?;
    let engine = state.exchange.lock().expect("exchange mutex poisoned");
    if engine.account(&parent).is_none() {
        return Err(ApiError::UnknownAccount);
    }
    Ok(Json(StrategyListResponse {
        strategies: build_strategy_list(&engine, parent),
    }))
}

/// `POST /api/accounts/{parent}/strategies` — compile the document,
/// create a funded sub-account carrying the record, then start the
/// deployment. A compile failure moves no money; a runtime start failure
/// closes the sub-account again.
pub async fn create_strategy(
    State(state): State<AppState>,
    Path(parent): Path<String>,
    Json(req): Json<CreateStrategyRequest>,
) -> Result<(StatusCode, Json<StrategyDetail>), ApiError> {
    let parent = Address::from_str(&parent)?;
    if req.allocation_tenths <= 0 {
        return Err(ApiError::BadRequest("allocation_tenths must be positive"));
    }
    // Everything that can fail without touching the ledger goes first.
    let (compiled, series) = compile_for_venue(&state.registry, &req.strategy, &req.envelope)?;
    let envelope_allocation = parse_decimal(req.envelope.allocation.as_str())
        .ok_or(ApiError::BadRequest("envelope.allocation is not a decimal"))?;
    if envelope_allocation > tenths_to_dollars(req.allocation_tenths) {
        return Err(ApiError::BadRequest("envelope.allocation exceeds the funded allocation_tenths"));
    }
    let params = params_to_values(&compiled.artifacts.program, &req.params)?;
    let record = DeployedStrategy::new(
        req.name,
        req.strategy,
        req.envelope,
        req.params,
        Arc::new(compiled.artifacts.program),
        series,
        next_deploy_epoch(),
        unix_now_ms(),
    );
    let detail = fund_and_start(&state, parent, Tenths(req.allocation_tenths), record, params).await?;
    Ok((StatusCode::CREATED, Json(detail)))
}

/// `GET /api/accounts/{parent}/strategies/{id}`
pub async fn get_strategy(
    State(state): State<AppState>,
    Path((parent, id)): Path<(String, String)>,
) -> Result<Json<StrategyDetail>, ApiError> {
    let parent = Address::from_str(&parent)?;
    let id = Address::from_str(&id)?;
    let child = {
        let engine = state.exchange.lock().expect("exchange mutex poisoned");
        if engine.account(&parent).is_none() {
            return Err(ApiError::UnknownAccount);
        }
        resolve_child(&engine, parent, id)?
    };
    let detail = detail_with_runtime(&state, child).await.ok_or(ApiError::StrategyNotFound)?;
    Ok(Json(detail))
}

/// `PATCH /api/accounts/{parent}/strategies/{id}` — update `name`
/// and/or `status`. A status change pauses or resumes the deployment.
pub async fn update_strategy(
    State(state): State<AppState>,
    Path((parent, id)): Path<(String, String)>,
    Json(req): Json<UpdateStrategyRequest>,
) -> Result<Json<StrategyDetail>, ApiError> {
    let parent = Address::from_str(&parent)?;
    let id = Address::from_str(&id)?;
    let (child, deployment_id, status) = {
        let mut engine = state.exchange.lock().expect("exchange mutex poisoned");
        if engine.account(&parent).is_none() {
            return Err(ApiError::UnknownAccount);
        }
        let child = resolve_child(&engine, parent, id)?;
        let record = engine.update_strategy(&child, req)?;
        (child, record.deployment_id, record.status)
    };
    match status {
        StrategyStatus::Active => state.runtime.resume(&deployment_id),
        StrategyStatus::Paused | StrategyStatus::Archived => state.runtime.pause(&deployment_id),
    }
    let detail = detail_with_runtime(&state, child).await.ok_or(ApiError::StrategyNotFound)?;
    Ok(Json(detail))
}

/// `DELETE /api/accounts/{parent}/strategies/{id}` — stop the deployment
/// (which cancels its live orders), then flatten and sweep cash back to
/// the parent.
pub async fn delete_strategy(
    State(state): State<AppState>,
    Path((parent, id)): Path<(String, String)>,
) -> Result<Json<DeleteStrategyResponse>, ApiError> {
    let parent = Address::from_str(&parent)?;
    let id = Address::from_str(&id)?;
    let (child, deployment_id) = {
        let engine = state.exchange.lock().expect("exchange mutex poisoned");
        if engine.account(&parent).is_none() {
            return Err(ApiError::UnknownAccount);
        }
        let child = resolve_child(&engine, parent, id)?;
        (child, deployment_id_of(&engine, child)?)
    };
    // Stop first so the deployment cannot place a new order between our
    // cancel and our close. `stop` waits for the task to wind down.
    state.runtime.stop(&deployment_id).await;
    let report = {
        let mut engine = state.exchange.lock().expect("exchange mutex poisoned");
        // Re-resolve: the strategy may have been deleted while we waited.
        let child = resolve_child(&engine, parent, child)?;
        let home = engine.ledger().home_series(&child).unwrap_or(DEFAULT_SERIES);
        let report = engine.close_sub_account(parent, child)?;
        crate::api::broadcast_book(engine.engine(home), state.channels_for(home));
        report
    };
    Ok(Json(DeleteStrategyResponse {
        swept_tenths: report.swept.0,
        cancelled_orders: report.cancelled_orders,
        residual_position: report.residual_position,
        removed: report.removed,
    }))
}

/// `POST /api/accounts/{parent}/strategies/{id}/fund`
pub async fn fund_strategy(
    State(state): State<AppState>,
    Path((parent, id)): Path<(String, String)>,
    Json(req): Json<FundStrategyRequest>,
) -> Result<Json<StrategyDetail>, ApiError> {
    let parent = Address::from_str(&parent)?;
    let id = Address::from_str(&id)?;
    if req.amount_tenths == 0 {
        return Err(ApiError::BadRequest("amount_tenths must be non-zero"));
    }
    let child = {
        let mut engine = state.exchange.lock().expect("exchange mutex poisoned");
        if engine.account(&parent).is_none() {
            return Err(ApiError::UnknownAccount);
        }
        let child = resolve_child(&engine, parent, id)?;
        engine.fund_sub_account(parent, child, Tenths(req.amount_tenths))?;
        child
    };
    let detail = detail_with_runtime(&state, child).await.ok_or(ApiError::StrategyNotFound)?;
    Ok(Json(detail))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::Mutex;

    use crate::api::SeriesChannels;
    use crate::automation::runtime::{runtime_channel, spawn_manager};
    use crate::automation::signal_bus::SignalBus;
    use crate::automation::signals::SignalStore;
    use crate::automation::sme::task::spawn_platform;
    use crate::automation::venue::{EngineClock, EngineVenue, SeriesMap};
    use crate::engine::strategies::test_support::{idle_document, test_envelope};
    use crate::engine::{Exchange, MIN_SUB_ACCOUNT_ALLOCATION, TradeSource};
    use crate::pricing::feed::PriceFeed;
    use crate::series::SERIES;
    use crate::ticks::PriceTick;
    use crate::types::{Qty, Side};

    pub(crate) const PARENT: &str = "0x1111111111111111111111111111111111111111";
    const OTHER: &str = "0x2222222222222222222222222222222222222222";
    pub(crate) const MAKER: &str = "0x3333333333333333333333333333333333333333";

    fn addr(s: &str) -> Address {
        s.parse().unwrap()
    }

    /// AppState with a real engine, a real signals platform, and a real
    /// runtime manager over `EngineVenue`, so the handlers exercise the
    /// same path production does. Markets are open in every series.
    fn app_state() -> AppState {
        app_state_with(crate::registry::for_tests()).0
    }

    /// [`app_state`] plus the platform handle, for sibling route tests.
    pub(crate) fn app_state_pair() -> (AppState, crate::automation::sme::task::PlatformHandle) {
        app_state_with(crate::registry::for_tests())
    }

    /// [`app_state`] over a chosen registry, also returning the platform
    /// handle so a test can wire more publishers (the lane bridge).
    pub(crate) fn app_state_with(registry: Arc<strategy_compiler::Registry>) -> (AppState, crate::automation::sme::task::PlatformHandle) {
        let mut x = Exchange::new(Tenths(1_000_000), SERIES);
        for a in [PARENT, OTHER] {
            x.register_user(addr(a));
            x.get_or_create_account(addr(a));
        }
        x.get_or_create_account(addr(MAKER));
        let now_s = unix_now_ms() / 1000;
        for s in SeriesId::all() {
            x.open_first_market(s, 50_000.0, now_s, s.duration_secs());
        }
        let exchange = Arc::new(Mutex::new(x));
        let series_map = SeriesMap::new(SERIES.iter().map(|s| (s.slug, s.id)));
        let clock = EngineClock::new(exchange.clone(), series_map.clone());
        let platform = spawn_platform(SignalStore::from_registry(&registry), clock.clone());
        let (runtime, rx) = runtime_channel();
        EngineVenue::install(&exchange, runtime.clone());
        let venue = EngineVenue::new(exchange.clone(), series_map);
        spawn_manager(rx, platform.clone(), venue, clock);
        let bus = SignalBus::new(registry.clone(), Some(platform.clone()));
        let state = AppState {
            exchange,
            channels: SeriesChannels::for_all_series(),
            price: PriceFeed::new(),
            runtime,
            registry,
            bus,
            templates: Arc::new(crate::templates::TemplateCatalogue::default()),
            deploys: crate::api::DeployKeys::default(),
            crowd: false,
            gemini: None,
        };
        (state, platform)
    }

    fn create_req(allocation: i64, series: SeriesId) -> CreateStrategyRequest {
        CreateStrategyRequest {
            allocation_tenths: allocation,
            name: Some("Test".into()),
            strategy: idle_document(series),
            envelope: test_envelope(),
            params: BTreeMap::new(),
        }
    }

    async fn create(state: &AppState, allocation: i64) -> StrategyDetail {
        let (status, Json(detail)) =
            create_strategy(State(state.clone()), Path(PARENT.into()), Json(create_req(allocation, DEFAULT_SERIES)))
                .await
                .unwrap();
        assert_eq!(status, StatusCode::CREATED);
        detail
    }

    #[tokio::test]
    async fn create_derives_the_series_from_the_document() {
        let state = app_state();
        let (_, Json(d)) =
            create_strategy(State(state.clone()), Path(PARENT.into()), Json(create_req(100_000, SeriesId(1))))
                .await
                .unwrap();
        assert_eq!(d.strategy.series, SeriesId(1));
        assert_eq!(d.summary.home_series, Some(SeriesId(1)));
        let rt = d.runtime.expect("deployment resident");
        assert_eq!(rt.current_leg, "idle");
        assert_eq!(rt.armed_triggers, vec!["tick".to_string()]);
        assert!(!rt.paused);
        assert!(state.runtime.ids().await.contains(&d.strategy.deployment_id));
    }

    #[tokio::test]
    async fn create_then_list_then_get() {
        let state = app_state();
        let d = create(&state, 100_000).await;
        assert_eq!(d.summary.kind, AccountKind::Strategy);
        assert_eq!(d.summary.name, "Test");
        assert_eq!(d.summary.status, Some(StrategyStatus::Active));
        assert_eq!(d.summary.balance_tenths, 100_000);
        assert_eq!(d.strategy.deployment_id, format!("{}:{}", d.strategy.strategy_id, d.strategy.deploy_epoch));

        let Json(list) = list_strategies(State(state.clone()), Path(PARENT.into())).await.unwrap();
        assert_eq!(list.strategies.len(), 1);
        assert_eq!(list.strategies[0].id, d.summary.id);

        let Json(got) = get_strategy(State(state.clone()), Path((PARENT.into(), d.summary.id.to_string())))
            .await
            .unwrap();
        assert_eq!(got.strategy, d.strategy);
        assert!(got.runtime.is_some());

        // Two deployments of the same document get distinct epochs.
        let d2 = create(&state, 100_000).await;
        assert_eq!(d2.strategy.strategy_id, d.strategy.strategy_id);
        assert_ne!(d2.strategy.deploy_epoch, d.strategy.deploy_epoch);
        assert_eq!(state.runtime.ids().await.len(), 2);
    }

    #[tokio::test]
    async fn compile_failure_moves_no_money() {
        let state = app_state();
        let mut req = create_req(100_000, DEFAULT_SERIES);
        req.strategy.legs[0].arm = serde_json::from_value(serde_json::json!(["missing"])).unwrap();
        let err = create_strategy(State(state.clone()), Path(PARENT.into()), Json(req)).await.unwrap_err();
        assert!(matches!(err, ApiError::CompileFailed(_)), "{err:?}");
        let e = state.exchange.lock().unwrap();
        assert_eq!(e.account(&addr(PARENT)).unwrap().usdc_balance, Tenths(1_000_000));
        assert!(e.sub_accounts_of(&addr(PARENT)).is_empty());
    }

    #[tokio::test]
    async fn create_rejects_bad_allocations_and_oversized_envelopes() {
        let state = app_state();
        let err = create_strategy(State(state.clone()), Path(PARENT.into()), Json(create_req(0, DEFAULT_SERIES)))
            .await
            .unwrap_err();
        assert!(matches!(err, ApiError::BadRequest(_)));

        // Envelope allocation ($100) above the funded $50.
        let err = create_strategy(State(state.clone()), Path(PARENT.into()), Json(create_req(50_000, DEFAULT_SERIES)))
            .await
            .unwrap_err();
        assert!(matches!(err, ApiError::BadRequest(m) if m.contains("envelope.allocation")), "{err:?}");

        // Below the sub-account minimum, with a matching tiny envelope.
        let mut req = create_req(MIN_SUB_ACCOUNT_ALLOCATION.0 - 1, DEFAULT_SERIES);
        req.envelope = serde_json::from_str(r#"{"allocation": "1", "loss_budget": "1", "max_order_size": "1"}"#).unwrap();
        let err = create_strategy(State(state.clone()), Path(PARENT.into()), Json(req)).await.unwrap_err();
        assert!(matches!(err, ApiError::AllocationTooSmall { .. }), "{err:?}");

        // More than the parent has.
        let mut req = create_req(2_000_000, DEFAULT_SERIES);
        req.envelope = test_envelope();
        let err = create_strategy(State(state.clone()), Path(PARENT.into()), Json(req)).await.unwrap_err();
        assert!(matches!(err, ApiError::InsufficientBalance { .. }), "{err:?}");
        assert!(state.runtime.ids().await.is_empty());
    }

    #[tokio::test]
    async fn get_hides_other_users_strategies() {
        let state = app_state();
        let d = create(&state, 100_000).await;
        let err = get_strategy(State(state.clone()), Path((OTHER.into(), d.summary.id.to_string())))
            .await
            .unwrap_err();
        assert!(matches!(err, ApiError::StrategyNotFound));
    }

    #[tokio::test]
    async fn patch_pause_and_resume_drive_the_runtime() {
        let state = app_state();
        let d = create(&state, 100_000).await;
        let id = d.summary.id.to_string();
        let Json(paused) = update_strategy(
            State(state.clone()),
            Path((PARENT.into(), id.clone())),
            Json(StrategyPatch { name: Some("Renamed".into()), status: Some(StrategyStatus::Paused) }),
        )
        .await
        .unwrap();
        assert_eq!(paused.strategy.name, "Renamed");
        assert_eq!(paused.strategy.status, StrategyStatus::Paused);
        let rt = paused.runtime.expect("still resident while paused");
        assert!(rt.paused);
        assert!(rt.armed_triggers.is_empty(), "a paused deployment arms nothing");

        let Json(resumed) = update_strategy(
            State(state.clone()),
            Path((PARENT.into(), id)),
            Json(StrategyPatch { name: None, status: Some(StrategyStatus::Active) }),
        )
        .await
        .unwrap();
        assert_eq!(resumed.strategy.name, "Renamed");
        let rt = resumed.runtime.unwrap();
        assert!(!rt.paused);
        assert_eq!(rt.armed_triggers, vec!["tick".to_string()]);
    }

    #[tokio::test]
    async fn fund_moves_cash_and_rejects_zero() {
        let state = app_state();
        let d = create(&state, 100_000).await;
        let id = d.summary.id.to_string();
        let Json(after) = fund_strategy(
            State(state.clone()),
            Path((PARENT.into(), id.clone())),
            Json(FundStrategyRequest { amount_tenths: 25_000 }),
        )
        .await
        .unwrap();
        assert_eq!(after.summary.balance_tenths, 125_000);
        assert_eq!(after.summary.net_contributed_tenths, 125_000);
        let err = fund_strategy(
            State(state.clone()),
            Path((PARENT.into(), id)),
            Json(FundStrategyRequest { amount_tenths: 0 }),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ApiError::BadRequest(_)));
    }

    #[tokio::test]
    async fn delete_stops_the_deployment_closes_the_position_and_sweeps() {
        let state = app_state();
        let d = create(&state, 100_000).await;
        let child = d.summary.id;
        {
            let mut e = state.exchange.lock().unwrap();
            e.place_limit(DEFAULT_SERIES, addr(MAKER), Side::Ask, PriceTick::from_tenths(500).unwrap(), Qty(10), TradeSource::Bot).unwrap();
            e.place_market(DEFAULT_SERIES, child, Side::Bid, Qty(10), TradeSource::Strategy).unwrap();
            e.place_limit(DEFAULT_SERIES, addr(MAKER), Side::Bid, PriceTick::from_tenths(500).unwrap(), Qty(10), TradeSource::Bot).unwrap();
        }
        let Json(list) = list_strategies(State(state.clone()), Path(PARENT.into())).await.unwrap();
        assert_eq!(list.strategies[0].signed_position, 10);
        assert_eq!(list.strategies[0].balance_tenths, 95_000);

        let Json(resp) = delete_strategy(State(state.clone()), Path((PARENT.into(), child.to_string())))
            .await
            .unwrap();
        assert!(resp.removed);
        assert_eq!(resp.residual_position, 0);
        assert_eq!(resp.swept_tenths, 100_000);
        assert!(state.runtime.ids().await.is_empty(), "deployment stopped");
        assert!(state.runtime.status(&d.strategy.deployment_id).await.is_none());
        {
            let e = state.exchange.lock().unwrap();
            assert!(e.account(&child).is_none());
            assert_eq!(e.account(&addr(PARENT)).unwrap().usdc_balance, Tenths(1_000_000));
        }
        // Second delete is a 404.
        let err = delete_strategy(State(state.clone()), Path((PARENT.into(), child.to_string())))
            .await
            .unwrap_err();
        assert!(matches!(err, ApiError::StrategyNotFound));
    }

    #[tokio::test]
    async fn orphaned_child_is_hidden_from_list() {
        let state = app_state();
        let d = create(&state, 100_000).await;
        let child = d.summary.id;
        {
            let mut e = state.exchange.lock().unwrap();
            e.place_limit(DEFAULT_SERIES, addr(MAKER), Side::Ask, PriceTick::from_tenths(500).unwrap(), Qty(10), TradeSource::Bot).unwrap();
            e.place_market(DEFAULT_SERIES, child, Side::Bid, Qty(10), TradeSource::Strategy).unwrap();
            // No bid to close into → orphan.
        }
        let Json(resp) = delete_strategy(State(state.clone()), Path((PARENT.into(), child.to_string())))
            .await
            .unwrap();
        assert!(!resp.removed);
        assert_eq!(resp.residual_position, 10);
        let Json(list) = list_strategies(State(state.clone()), Path(PARENT.into())).await.unwrap();
        assert!(list.strategies.is_empty(), "orphans are not strategies");
        assert!(state.runtime.ids().await.is_empty());
    }

    /// A one-shot bidder against the exchange's own price signal: when the
    /// up ask is at or under 0.60, rest a bid five cents under it.
    fn cheap_bidder_document() -> Strategy {
        let slug = DEFAULT_SERIES.slug();
        serde_json::from_value(serde_json::json!({
            "meta": {
                "name": "Cheap Bidder",
                "description": "When the up ask is at or under 0.60, rest a bid five cents under it once per round.",
                "dsl_version": "0.3"
            },
            "instruments": { "market": format!("{slug}.current") },
            "signals": { "book": format!("{slug}.current.price.v1") },
            "state": [],
            "triggers": [
                { "name": "cheap",
                  "condition": { "signal": "book", "field": "ask", "comparator": "<=", "value": "0.6" },
                  "fire": "if_true", "rearm": { "mode": "never" } }
            ],
            "legs": [
                { "name": "hunting", "resident": true, "arm": ["cheap"],
                  "body": [
                    { "name": "bid", "if": "triggered(cheap)",
                      "do": [ { "place": {
                          "instrument": "market", "outcome": "up", "side": "buy",
                          "order_type": "limit", "tif": "gtc",
                          "price": "clamp(signal.book.ask - 0.05, 0.01, 0.99)",
                          "price_band": ["0.01", "0.99"],
                          "size": "clamp(envelope.max_order_size * 0.5, 0, envelope.max_order_size)",
                          "max_size": "envelope.max_order_size",
                          "tag": "bid",
                          "on": { "fill": "done", "reject": "hunting" }
                      } } ],
                      "then": "done" }
                  ] },
                { "name": "done", "resident": true, "arm": [], "body": [] }
            ]
        }))
        .expect("cheap bidder parses")
    }

    /// The whole production path: deploy over the handler, publish a price
    /// frame on the bus, and find the strategy's bid on the real book.
    #[tokio::test]
    async fn deployed_strategy_trades_on_a_bus_frame() {
        use crate::automation::signal_bus::{Quality, SignalFrame};

        let state = app_state();
        let req = CreateStrategyRequest {
            allocation_tenths: 100_000,
            name: None,
            strategy: cheap_bidder_document(),
            envelope: test_envelope(),
            params: BTreeMap::new(),
        };
        let (_, Json(d)) = create_strategy(State(state.clone()), Path(PARENT.into()), Json(req)).await.unwrap();
        let child = d.summary.id;
        assert_eq!(d.summary.name, "Cheap Bidder", "name defaults to the document");
        assert_eq!(d.runtime.as_ref().unwrap().current_leg, "hunting");
        assert_eq!(state.exchange.lock().unwrap().engine(DEFAULT_SERIES).book().best_bid(), None);

        // The exchange's price publisher would emit this frame; publish it
        // by hand so the test controls the ask.
        let values: BTreeMap<String, Json_> = [
            ("up", "0.550"), ("down", "0.450"), ("bid", "0.000"), ("ask", "0.550"), ("last", "0.000"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), serde_json::json!(v)))
        .collect();
        state.bus.publish(SignalFrame {
            signal_id: format!("{}.current.price.v1", DEFAULT_SERIES.slug()),
            ts_ms: 1,
            quality: Quality::ready(),
            values,
        });

        // Bus → platform task → activation → deployment task → EngineVenue.
        let mut best_bid = None;
        for _ in 0..200 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            best_bid = state.exchange.lock().unwrap().engine(DEFAULT_SERIES).book().best_bid();
            if best_bid.is_some() {
                break;
            }
        }
        assert_eq!(best_bid, Some(PriceTick::from_tenths(500).unwrap()), "bid rests at ask - 0.05");
        {
            let e = state.exchange.lock().unwrap();
            assert_eq!(e.account(&child).unwrap().locked, Tenths(5 * 500), "five lots at 50c locked");
        }
        let Json(got) = get_strategy(State(state.clone()), Path((PARENT.into(), child.to_string()))).await.unwrap();
        let rt = got.runtime.unwrap();
        assert_eq!(rt.current_leg, "done");
        assert_eq!(rt.live_orders.len(), 1);
        assert_eq!(rt.live_orders[0].price.as_deref(), Some("0.5"));
        assert_eq!(rt.live_orders[0].tag, "bid");
        assert!(rt.log.iter().any(|l| l.contains("[order]")), "{:#?}", rt.log);

        // Delete pulls the resting bid and sweeps the cash home.
        let Json(resp) = delete_strategy(State(state.clone()), Path((PARENT.into(), child.to_string()))).await.unwrap();
        assert!(resp.removed);
        assert_eq!(resp.swept_tenths, 100_000);
        assert_eq!(state.exchange.lock().unwrap().engine(DEFAULT_SERIES).book().best_bid(), None);
    }

    /// A strategy on a lane signal: when Coinbase RSI reads oversold, rest
    /// a bid at 40c once per round.
    fn rsi_dip_document() -> Strategy {
        let slug = DEFAULT_SERIES.slug();
        serde_json::from_value(serde_json::json!({
            "meta": {
                "name": "RSI Dip",
                "description": "When the Coinbase 14-bar RSI is at or under 30, rest a bid on up at 40c once per round.",
                "dsl_version": "0.3"
            },
            "instruments": { "market": format!("{slug}.current") },
            "signals": { "rsi": "btc.coinbase.rsi.v1" },
            "state": [],
            "triggers": [
                { "name": "oversold",
                  "condition": { "signal": "rsi", "field": "rsi_14", "comparator": "<=", "value": "30" },
                  "fire": "if_true", "rearm": { "mode": "never" } }
            ],
            "legs": [
                { "name": "hunting", "resident": true, "arm": ["oversold"],
                  "body": [
                    { "name": "bid", "if": "triggered(oversold)",
                      "do": [ { "place": {
                          "instrument": "market", "outcome": "up", "side": "buy",
                          "order_type": "limit", "tif": "gtc",
                          "price": "clamp(0.40, 0.01, 0.99)",
                          "price_band": ["0.01", "0.99"],
                          "size": "clamp(envelope.max_order_size * 0.5, 0, envelope.max_order_size)",
                          "max_size": "envelope.max_order_size",
                          "tag": "bid",
                          "on": { "fill": "done", "reject": "hunting" }
                      } } ],
                      "then": "done" }
                  ] },
                { "name": "done", "resident": true, "arm": [], "body": [] }
            ]
        }))
        .expect("rsi dip parses")
    }

    /// The whole ingest path, from a lane to the book: a strategy deployed
    /// over the handler against a registry that carries the checkout's
    /// signal definitions; a stream message on the supervisor's channel,
    /// exactly as a lane's hook would send it; the bridge, the bus, the
    /// platform, the deployment and `EngineVenue`; and the strategy's bid
    /// resting on the real book. The catalogue reports the ingest.
    #[tokio::test]
    async fn deployed_strategy_trades_on_a_lane_message() {
        use crate::automation::lane_bridge::tests::{batch, ready_update, registry_with_spot_lane};
        use crate::automation::lane_bridge::spawn_lane_bridge;
        use crate::signals::LaneMessage;

        let (state, platform) = app_state_with(registry_with_spot_lane());
        let (lane_tx, lane_rx) = tokio::sync::mpsc::unbounded_channel();
        spawn_lane_bridge(state.bus.clone(), platform, lane_rx);

        let req = CreateStrategyRequest {
            allocation_tenths: 100_000,
            name: None,
            strategy: rsi_dip_document(),
            envelope: test_envelope(),
            params: BTreeMap::new(),
        };
        let (_, Json(d)) = create_strategy(State(state.clone()), Path(PARENT.into()), Json(req)).await.unwrap();
        let child = d.summary.id;
        assert_eq!(d.runtime.as_ref().unwrap().current_leg, "hunting");
        assert_eq!(d.runtime.as_ref().unwrap().armed_triggers, vec!["oversold"]);
        assert_eq!(state.exchange.lock().unwrap().engine(DEFAULT_SERIES).book().best_bid(), None);

        // A neutral reading first: typed, ingested, no fire.
        let neutral = ready_update(
            "btc.coinbase.rsi",
            "2026-09-06T00:00:01Z",
            &[("rsi_14", Json_::from("55.0")), ("regime", Json_::from("neutral")), ("bar_count", Json_::from(20)), ("window_close", Json_::from("2026-09-06T00:00:00Z"))],
        );
        lane_tx.send(LaneMessage { lane: "spot", message: batch(vec![neutral]) }).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(state.exchange.lock().unwrap().engine(DEFAULT_SERIES).book().best_bid(), None, "55 is not oversold");

        // Oversold: lane channel → bridge → bus → platform → deployment → book.
        let oversold = ready_update(
            "btc.coinbase.rsi",
            "2026-09-06T00:00:02Z",
            &[("rsi_14", Json_::from("25.0")), ("regime", Json_::from("oversold")), ("bar_count", Json_::from(21)), ("window_close", Json_::from("2026-09-06T00:00:00Z"))],
        );
        lane_tx.send(LaneMessage { lane: "spot", message: batch(vec![oversold]) }).unwrap();
        let mut best_bid = None;
        for _ in 0..200 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            best_bid = state.exchange.lock().unwrap().engine(DEFAULT_SERIES).book().best_bid();
            if best_bid.is_some() {
                break;
            }
        }
        assert_eq!(best_bid, Some(PriceTick::from_tenths(400).unwrap()), "bid rests at 40c");
        {
            let e = state.exchange.lock().unwrap();
            assert_eq!(e.account(&child).unwrap().locked, Tenths(5 * 400), "five lots at 40c locked");
        }
        let Json(got) = get_strategy(State(state.clone()), Path((PARENT.into(), child.to_string()))).await.unwrap();
        let rt = got.runtime.unwrap();
        assert_eq!(rt.current_leg, "done");
        assert_eq!(rt.live_orders.len(), 1);
        assert_eq!(rt.live_orders[0].price.as_deref(), Some("0.4"));

        // The catalogue shows the signal live, ready, and ingested twice.
        let cat = crate::api::signals::catalogue(&state.bus, DEFAULT_SERIES);
        let rsi = cat.signals.iter().find(|s| s.signal_id == "btc.coinbase.rsi.v1").expect("rsi is catalogued for the series");
        assert!(rsi.live);
        assert_eq!(rsi.quality.state, "ready");
        assert_eq!((rsi.ingest.published, rsi.ingest.rejected), (2, 0));
        assert_eq!(rsi.fields.iter().find(|f| f.name == "rsi_14").unwrap().value, Some(Json_::from("25.0")));

        let Json(resp) = delete_strategy(State(state.clone()), Path((PARENT.into(), child.to_string()))).await.unwrap();
        assert!(resp.removed);
        assert_eq!(state.exchange.lock().unwrap().engine(DEFAULT_SERIES).book().best_bid(), None);
    }
}
