//! `POST /api/accounts/{parent}/templates/{template_id}/deploy` — deploy one
//! side of a public strategy template.
//!
//! The template catalogue compiled every document at boot, which proves
//! each one clean. This route derives the envelope from the user's
//! allocation and the template's ratios, compiles the document once more
//! against that envelope (the compiler writes envelope reads into the
//! program, so the boot program carries the catalogue's $1,000 reference
//! and must not be run for anyone else), and runs the same fund-and-start
//! path as `POST /strategies`. The body is
//! `{ allocation_tenths, inverse, series? }`; `series` defaults to the
//! exchange's default series and must be one the template lists.
//!
//! An `Idempotency-Key` header makes a retry safe: a repeat of the same
//! key from the same parent answers with the deployment the first call
//! made, without moving money again.

use std::collections::BTreeMap;
use std::str::FromStr;
use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
};
use serde::Deserialize;

use super::compile::compile_for_venue;
use super::store::{StrategyDetail, fund_and_start, tenths_to_dollars};
use crate::api::{ApiError, AppState};
use crate::automation::runtime::next_deploy_epoch;
use crate::engine::DeployedStrategy;
use crate::pricing::feed::unix_now_ms;
use crate::templates::{Side, envelope_for};
use strategy_compiler::ir::Envelope;
use crate::types::{Address, SeriesId, Tenths};

pub const IDEMPOTENCY_HEADER: &str = "idempotency-key";

#[derive(Debug, Clone, Deserialize)]
pub struct DeployTemplateRequest {
    /// Cash to move from the parent into the new sub-account; also the
    /// envelope's allocation.
    pub allocation_tenths: i64,
    /// `true` deploys the template's Inverse document.
    #[serde(default)]
    pub inverse: bool,
    /// The series to deploy on; the exchange's default when absent.
    #[serde(default)]
    pub series: Option<SeriesId>,
}

/// Deploy `template_id`'s `side` on `series` for `parent`, funded with
/// `allocation`. The in-process path the HTTP handler and the boot
/// seeding share; the ledger is untouched until the template and series
/// have been checked.
pub async fn deploy_template_side(
    state: &AppState,
    parent: Address,
    template_id: &str,
    series: SeriesId,
    side: Side,
    allocation: Tenths,
) -> Result<StrategyDetail, ApiError> {
    let template = state.templates.get(template_id).filter(|t| t.lists(series)).ok_or(ApiError::TemplateNotFound)?;
    let envelope = envelope_for(&template.envelope, tenths_to_dollars(allocation.0));
    deploy_template_side_with(state, parent, template_id, series, side, allocation, envelope).await
}

/// [`deploy_template_side`] under a caller-supplied envelope, for a
/// caller that wants a tighter loss budget than the file's ratios give.
pub async fn deploy_template_side_with(
    state: &AppState,
    parent: Address,
    template_id: &str,
    series: SeriesId,
    side: Side,
    allocation: Tenths,
    envelope: Envelope,
) -> Result<StrategyDetail, ApiError> {
    if allocation.0 <= 0 {
        return Err(ApiError::BadRequest("allocation_tenths must be positive"));
    }
    let template = state.templates.get(template_id).filter(|t| t.lists(series)).ok_or(ApiError::TemplateNotFound)?;
    let compiled = template.compiled(series, side).ok_or(ApiError::TemplateNotFound)?;
    // The catalogue's program was compiled at boot against a $1,000
    // reference envelope, and the compiler writes every envelope read into
    // the program as a value. Run as it is, it would size each order and
    // gate each risk check as if the deployer had put in $1,000. So the
    // document is compiled again here against the deployer's own envelope;
    // the boot compile stays the proof that the document is clean.
    let (own, _) = compile_for_venue(&state.registry, &compiled.source, &envelope)?;
    let record = DeployedStrategy::new(
        None,
        compiled.source.clone(),
        envelope,
        BTreeMap::new(),
        Arc::new(own.artifacts.program),
        series,
        next_deploy_epoch(),
        unix_now_ms(),
    )
    .from_template(template_id, side.is_inverse());
    fund_and_start(state, parent, allocation, record, BTreeMap::new()).await
}

/// `POST /api/accounts/{parent}/templates/{template_id}/deploy`
pub async fn deploy_template(
    State(state): State<AppState>,
    Path((parent, template_id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(req): Json<DeployTemplateRequest>,
) -> Result<(StatusCode, Json<StrategyDetail>), ApiError> {
    let parent = Address::from_str(&parent)?;
    let series = state.series_or_default(req.series);
    let side = if req.inverse { Side::Inverse } else { Side::Primary };
    let key = headers.get(IDEMPOTENCY_HEADER).and_then(|v| v.to_str().ok()).map(str::trim).filter(|k| !k.is_empty()).map(str::to_string);

    if let Some(child) = key.as_deref().and_then(|k| state.deploys.get(parent, k)) {
        // The deployment may have been deleted since; then the key is
        // stale and the request is a fresh deploy.
        if let Some(detail) = super::store::detail_for(&state, parent, child).await {
            return Ok((StatusCode::CREATED, Json(detail)));
        }
    }

    let detail = deploy_template_side(&state, parent, &template_id, series, side, Tenths(req.allocation_tenths)).await?;
    if let Some(key) = key {
        state.deploys.insert(parent, key, detail.summary.id);
    }
    Ok((StatusCode::CREATED, Json(detail)))
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::api::strategy::store::tests::{MAKER, PARENT};
    use crate::api::strategy::store::{delete_strategy, get_strategy, list_strategies};
    use crate::automation::lane_bridge::tests::registry_with_spot_lane;
    use crate::automation::signal_bus::{Quality, SignalFrame};
    use crate::engine::{MIN_SUB_ACCOUNT_ALLOCATION, TradeSource};
    use crate::registry::PriceSource;
    use crate::series::SERIES;
    use crate::templates::TemplateCatalogue;
    use crate::ticks::PriceTick;
    use crate::types::{Qty, Side as BookSide};
    use serde_json::json;

    fn addr(s: &str) -> Address {
        s.parse().unwrap()
    }

    /// The store rig with every template compiled into the catalogue.
    fn state_with_templates() -> AppState {
        let registry = registry_with_spot_lane();
        let catalogue = TemplateCatalogue::load(&registry, PriceSource::GeminiIndex).unwrap_or_else(|e| panic!("{e}"));
        let (mut state, _) = crate::api::strategy::store::tests::app_state_with(registry);
        state.templates = Arc::new(catalogue);
        state
    }

    async fn deploy(state: &AppState, template: &str, body: serde_json::Value, key: Option<&str>) -> Result<StrategyDetail, ApiError> {
        let mut headers = HeaderMap::new();
        if let Some(k) = key {
            headers.insert(IDEMPOTENCY_HEADER, k.parse().unwrap());
        }
        let req: DeployTemplateRequest = serde_json::from_value(body).unwrap();
        deploy_template(State(state.clone()), Path((PARENT.into(), template.into())), headers, Json(req))
            .await
            .map(|(status, Json(d))| {
                assert_eq!(status, StatusCode::CREATED);
                d
            })
    }

    #[tokio::test]
    async fn deploys_both_sides_under_the_deployers_own_envelope_and_records_the_template() {
        let state = state_with_templates();
        let primary = deploy(&state, "late-closer", json!({ "allocation_tenths": 50_000, "inverse": false, "series": "btc-15m" }), None).await.unwrap();
        let inverse = deploy(&state, "late-closer", json!({ "allocation_tenths": 25_000, "inverse": true, "series": "btc-15m" }), None).await.unwrap();

        let template = state.templates.get("late-closer").unwrap();
        for (d, side, tenths) in [(&primary, Side::Primary, 50_000), (&inverse, Side::Inverse, 25_000)] {
            let compiled = template.compiled(SERIES[1].id, side).unwrap();
            assert_eq!(d.strategy.strategy_id, compiled.program.strategy_id, "the catalogue's document");
            assert_eq!(d.strategy.template_id.as_deref(), Some("late-closer"));
            assert_eq!(d.strategy.inverse, side.is_inverse());
            assert_eq!(d.strategy.series, SERIES[1].id);
            assert_eq!(d.summary.home_series, Some(SERIES[1].id));
            assert_eq!(d.summary.balance_tenths, tenths);
            assert_eq!(d.summary.name, compiled.program.name);
            assert_eq!(d.strategy.envelope.allocation.as_str(), tenths_to_dollars(tenths).normalize().to_string());
            assert!(d.runtime.as_ref().is_some_and(|r| !r.paused), "deployment resident");
        }
        // The two documents are two strategies, and each runs a program
        // compiled against its own envelope, not the catalogue's reference.
        assert_ne!(primary.strategy.strategy_id, inverse.strategy.strategy_id);
        {
            let e = state.exchange.lock().unwrap();
            let record = e.account(&primary.summary.id).unwrap().strategy.as_ref().unwrap();
            assert_eq!(record.program.envelope.allocation.as_str(), "50");
            assert_eq!(record.program.envelope.loss_budget.as_str(), "50");
            assert_eq!(template.compiled(SERIES[1].id, Side::Primary).unwrap().program.envelope.allocation.as_str(), "1000");
            assert_eq!(e.account(&addr(PARENT)).unwrap().usdc_balance, Tenths(1_000_000 - 75_000));
        }
        let Json(list) = list_strategies(State(state.clone()), Path(PARENT.into())).await.unwrap();
        assert_eq!(list.strategies.len(), 2);
        assert_eq!(state.runtime.ids().await.len(), 2);
    }

    #[tokio::test]
    async fn refuses_unknown_templates_off_series_listings_and_bad_allocations_without_moving_money() {
        let state = state_with_templates();
        let cases = [
            ("nope", json!({ "allocation_tenths": 50_000 })),
            // trend-rider is 15-minute only; the default series is btc-5m.
            ("trend-rider", json!({ "allocation_tenths": 50_000 })),
            ("trend-rider", json!({ "allocation_tenths": 50_000, "series": "btc-5m" })),
        ];
        for (id, body) in cases {
            let err = deploy(&state, id, body, None).await.unwrap_err();
            assert!(matches!(err, ApiError::TemplateNotFound), "{id}: {err:?}");
        }
        let err = deploy(&state, "late-closer", json!({ "allocation_tenths": 0 }), None).await.unwrap_err();
        assert!(matches!(err, ApiError::BadRequest(_)), "{err:?}");
        let err = deploy(&state, "late-closer", json!({ "allocation_tenths": MIN_SUB_ACCOUNT_ALLOCATION.0 - 1 }), None).await.unwrap_err();
        assert!(matches!(err, ApiError::AllocationTooSmall { .. }), "{err:?}");
        let err = deploy(&state, "late-closer", json!({ "allocation_tenths": 5_000_000 }), None).await.unwrap_err();
        assert!(matches!(err, ApiError::InsufficientBalance { .. }), "{err:?}");
        let e = state.exchange.lock().unwrap();
        assert_eq!(e.account(&addr(PARENT)).unwrap().usdc_balance, Tenths(1_000_000));
        assert!(e.sub_accounts_of(&addr(PARENT)).is_empty());
    }

    #[tokio::test]
    async fn an_idempotency_key_replays_the_first_deploy_until_it_is_gone() {
        let state = state_with_templates();
        let body = json!({ "allocation_tenths": 50_000, "series": "btc-5m" });
        let first = deploy(&state, "late-closer", body.clone(), Some("k1")).await.unwrap();
        let again = deploy(&state, "late-closer", body.clone(), Some("k1")).await.unwrap();
        assert_eq!(again.summary.id, first.summary.id, "same key, same deployment");
        assert_eq!(again.strategy.deployment_id, first.strategy.deployment_id);
        let other = deploy(&state, "late-closer", body.clone(), Some("k2")).await.unwrap();
        assert_ne!(other.summary.id, first.summary.id);
        {
            let e = state.exchange.lock().unwrap();
            assert_eq!(e.sub_accounts_of(&addr(PARENT)).len(), 2, "two keys, two deployments");
            assert_eq!(e.account(&addr(PARENT)).unwrap().usdc_balance, Tenths(900_000));
        }
        // A deleted deployment does not come back from its key.
        let Json(deleted) = delete_strategy(State(state.clone()), Path((PARENT.into(), first.summary.id.to_string()))).await.unwrap();
        assert!(deleted.removed);
        let fresh = deploy(&state, "late-closer", body, Some("k1")).await.unwrap();
        assert_ne!(fresh.summary.id, first.summary.id);
        assert_eq!(state.exchange.lock().unwrap().sub_accounts_of(&addr(PARENT)).len(), 2);
    }

    /// Deploy → bus frame → order on the real book, over the template
    /// route: late-closer's primary buys the side the round is leaning
    /// to once the clock is inside its window.
    #[tokio::test]
    async fn a_template_deployment_trades_on_a_bus_frame() {
        let state = state_with_templates();
        let series = SERIES[0].id;
        {
            let mut e = state.exchange.lock().unwrap();
            let maker = addr(MAKER);
            e.place_limit(series, maker, BookSide::Bid, PriceTick::from_tenths(300).unwrap(), Qty(500), TradeSource::Bot).unwrap();
            e.place_limit(series, maker, BookSide::Ask, PriceTick::from_tenths(400).unwrap(), Qty(500), TradeSource::Bot).unwrap();
        }
        let d = deploy(&state, "late-closer", json!({ "allocation_tenths": 100_000, "series": "btc-5m" }), None).await.unwrap();
        let child = d.summary.id;
        let publish = |id: &str, values: &[(&str, serde_json::Value)]| {
            state.bus.publish(SignalFrame {
                signal_id: id.to_string(),
                ts_ms: unix_now_ms(),
                quality: Quality::ready(),
                values: values.iter().map(|(k, v)| (k.to_string(), v.clone())).collect(),
            });
        };
        let slug = series.slug();
        publish(&format!("{slug}.current.price.v1"), &[("up", json!("0.35")), ("down", json!("0.65")), ("bid", json!("0.3")), ("ask", json!("0.4")), ("last", json!("0"))]);
        publish(&format!("{slug}.current.delta.v1"), &[("delta_bps", json!("25")), ("spot", json!("50125.00")), ("strike", json!("50000.00"))]);
        publish(&format!("{slug}.current.window.v1"), &[("phase", json!("open")), ("seq", json!(1)), ("remaining", json!(15_000))]);

        let mut position = 0;
        for _ in 0..200 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            position = state.exchange.lock().unwrap().engine(series).position(&child).position.0;
            if position != 0 {
                break;
            }
        }
        assert!(position > 0, "late-closer bought Up into the close: {position}");
        let Json(got) = get_strategy(State(state.clone()), Path((PARENT.into(), child.to_string()))).await.unwrap();
        assert_eq!(got.strategy.template_id.as_deref(), Some("late-closer"));
        assert!(got.runtime.unwrap().log.iter().any(|l| l.contains("[order]")));
    }
}
