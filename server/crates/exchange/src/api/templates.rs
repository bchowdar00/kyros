//! `GET /api/strategies/templates?series=<slug>` and
//! `GET /api/strategies/templates/{id}?series=<slug>` — the public strategy
//! cards for one market.
//!
//! The catalogue (`crate::templates`) is fixed at boot; this endpoint is
//! the read side the market page draws its strategy column from and the
//! Run / Inverse widget refetches on open. Each entry carries both sides
//! (`primary` for Run, `inverse` for Inverse) with their own prose and
//! counts, plus the combined counts the card shows.
//!
//! Counts come from one walk over every account under the engine lock
//! ([`TemplateTally::collect`]): a deployment of a template is a
//! sub-account whose record carries the template id, and it counts on the
//! series it is homed in. `running` is the number of active ones,
//! `deployed_tenths` the cash their parents have put in. Runner portraits
//! are the creator's when the creator's own account runs the template;
//! user accounts carry an emoji and a colour, not a portrait file.
//!
//! Readiness comes from the bus ([`Readiness::from_bus`]): a side is
//! `ready` when every signal its compiled program subscribes to is usable,
//! and `waiting_on` lists the ones that are not. A deployment made while a
//! signal is missing never fires and never errors (a row reading an
//! unpublished field is false), so the card says so up front instead of
//! letting a user fund a sub-account that sits idle.

use std::collections::{BTreeSet, HashMap};

use axum::{
    Json,
    extract::{Path, Query, State},
};
use serde::Serialize;

use crate::api::{ApiError, AppState, SeriesQuery};
use crate::automation::signal_bus::SignalBus;
use crate::engine::{Exchange, StrategyStatus};
use crate::templates::{Creator, EnvelopeRatios, Side, Template, TemplateCatalogue};
use crate::types::SeriesId;

/// A signal a template side reads that the platform cannot act on yet.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct WaitingSignal {
    pub signal_id: String,
    /// The registry's readable name, for the card's copy.
    pub name: String,
}

/// Which signals are usable right now, read once per request from the
/// bus. A signal is usable when its quality is `ready` *and* at least one
/// of its frames has typed against the registry and reached the platform:
/// the bus stamps `ready_since` from the frame's quality before typing it,
/// so a lane signal whose frames all fail typing reads as ready while the
/// platform has never seen a value. That is the definition mismatch the
/// catalogue's `ingest` counters exist to expose, and a card must not call
/// it deployable.
#[derive(Debug, Clone, Default)]
pub struct Readiness {
    usable: BTreeSet<String>,
    names: HashMap<String, String>,
    /// Everything counts as usable; the view of a catalogue with no bus.
    all_ready: bool,
}

impl Readiness {
    /// Every registry signal, checked against the bus.
    pub fn from_bus(bus: &SignalBus) -> Self {
        let mut readiness = Readiness::default();
        for def in &bus.registry().signals {
            readiness.names.insert(def.signal_id.clone(), def.name.clone().unwrap_or_else(|| def.signal_id.clone()));
            if bus.ready_since(&def.signal_id).is_some() && bus.ingest_stats(&def.signal_id).published > 0 {
                readiness.usable.insert(def.signal_id.clone());
            }
        }
        readiness
    }

    /// Every signal usable; for views that carry no readiness (tests).
    pub fn all_ready() -> Self {
        Readiness { all_ready: true, ..Readiness::default() }
    }

    pub fn is_usable(&self, signal_id: &str) -> bool {
        self.all_ready || self.usable.contains(signal_id)
    }

    /// The subscriptions in `signal_ids` that are not usable, sorted by id.
    pub fn waiting_on<'a>(&self, signal_ids: impl IntoIterator<Item = &'a String>) -> Vec<WaitingSignal> {
        let mut waiting: Vec<WaitingSignal> = signal_ids
            .into_iter()
            .filter(|id| !self.is_usable(id))
            .map(|id| WaitingSignal { signal_id: id.clone(), name: self.names.get(id).cloned().unwrap_or_else(|| id.clone()) })
            .collect();
        waiting.sort();
        waiting.dedup();
        waiting
    }
}

/// One side of a template: what Run or Inverse deploys.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TemplateSideView {
    /// The document's `meta.name`.
    pub name: String,
    /// The card prose for this side.
    pub logic: String,
    /// Active deployments of this side on the series.
    pub running: u32,
    /// Σ `net_contributed` over the real deployments, tenths of a cent.
    pub deployed_tenths: i64,
    /// Every signal this side reads is usable, so a deployment can fire.
    pub ready: bool,
    /// The signals this side reads that are not usable yet.
    pub waiting_on: Vec<WaitingSignal>,
}

/// One template as the market page sees it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TemplateView {
    pub id: String,
    pub title: String,
    /// Every series the template is listed on, as slugs.
    pub series: Vec<String>,
    pub creator: Creator,
    pub envelope: EnvelopeRatios,
    pub primary: TemplateSideView,
    pub inverse: TemplateSideView,
    /// Both sides combined.
    pub running: u32,
    pub deployed_tenths: i64,
    /// Portrait filenames of a few accounts running it.
    pub runners: Vec<String>,
    /// Both sides ready: the card offers Run and Inverse, so it is
    /// deployable only when each can fire.
    pub ready: bool,
    /// The union of both sides' waiting signals.
    pub waiting_on: Vec<WaitingSignal>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TemplateCatalogueResponse {
    pub series: String,
    pub templates: Vec<TemplateView>,
}

/// The active deployments of one template side on one series.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SideTally {
    pub running: u32,
    pub deployed_tenths: i64,
}

/// The deployment counts of every template on one series, from one walk
/// over the accounts.
#[derive(Debug, Clone, Default)]
pub struct TemplateTally {
    sides: HashMap<(String, Side), SideTally>,
    /// Usernames of the root accounts running each template, in walk order.
    runners: HashMap<String, Vec<String>>,
}

impl TemplateTally {
    /// Walk every account once and count the active template deployments
    /// homed in `series`. Call under the engine lock.
    pub fn collect(exchange: &Exchange, series: SeriesId) -> Self {
        let mut tally = TemplateTally::default();
        for acct in exchange.accounts_iter() {
            let Some(record) = acct.strategy.as_ref() else { continue };
            let Some(template_id) = record.template_id.as_deref() else { continue };
            if record.series != series || record.status != StrategyStatus::Active {
                continue;
            }
            let side = if record.inverse { Side::Inverse } else { Side::Primary };
            let entry = tally.sides.entry((template_id.to_string(), side)).or_default();
            entry.running += 1;
            entry.deployed_tenths += acct.net_contributed.map_or(0, |t| t.0);
            if let Some(root) = acct.parent.and_then(|p| exchange.account(&p))
                && let Some(username) = &root.username
            {
                let names = tally.runners.entry(template_id.to_string()).or_default();
                if !names.contains(username) {
                    names.push(username.clone());
                }
            }
        }
        tally
    }

    pub fn side(&self, template_id: &str, side: Side) -> SideTally {
        self.sides.get(&(template_id.to_string(), side)).copied().unwrap_or_default()
    }

    /// Portraits of the accounts running `t`: the creator's portrait when
    /// the creator's account is among them.
    fn runner_portraits(&self, t: &Template) -> Vec<String> {
        let Some(portrait) = t.creator.avatar.as_ref() else { return Vec::new() };
        let creator_runs = self.runners.get(&t.id).is_some_and(|names| names.contains(&t.creator.username));
        if creator_runs { vec![portrait.clone()] } else { Vec::new() }
    }
}

fn side_view(t: &Template, series: SeriesId, side: Side, tally: &TemplateTally, readiness: &Readiness) -> TemplateSideView {
    let compiled = t.compiled(series, side);
    let name = compiled.map(|c| c.program.name.clone()).unwrap_or_default();
    let waiting_on = readiness.waiting_on(compiled.map(|c| c.program.subscriptions.iter()).into_iter().flatten());
    let counts = tally.side(&t.id, side);
    TemplateSideView {
        name,
        logic: t.prose(side).to_string(),
        running: counts.running,
        deployed_tenths: counts.deployed_tenths,
        ready: waiting_on.is_empty(),
        waiting_on,
    }
}

/// The view of `t` on `series`. The caller has checked the listing.
pub fn template_view(t: &Template, series: SeriesId, tally: &TemplateTally, readiness: &Readiness) -> TemplateView {
    let primary = side_view(t, series, Side::Primary, tally, readiness);
    let inverse = side_view(t, series, Side::Inverse, tally, readiness);
    let mut waiting_on: Vec<WaitingSignal> = primary.waiting_on.iter().chain(&inverse.waiting_on).cloned().collect();
    waiting_on.sort();
    waiting_on.dedup();
    TemplateView {
        id: t.id.clone(),
        title: t.title.clone(),
        series: t.series.iter().map(|s| s.slug().to_string()).collect(),
        creator: t.creator.clone(),
        envelope: t.envelope.clone(),
        running: primary.running + inverse.running,
        deployed_tenths: primary.deployed_tenths + inverse.deployed_tenths,
        ready: primary.ready && inverse.ready,
        primary,
        inverse,
        runners: tally.runner_portraits(t),
        waiting_on,
    }
}

/// Every template listed on `series`, catalogue order.
pub fn catalogue(templates: &TemplateCatalogue, series: SeriesId, tally: &TemplateTally, readiness: &Readiness) -> TemplateCatalogueResponse {
    TemplateCatalogueResponse {
        series: series.slug().to_string(),
        templates: templates.for_series(series).map(|t| template_view(t, series, tally, readiness)).collect(),
    }
}

/// The counts for `series`, read under the engine lock.
fn tally_for(state: &AppState, series: SeriesId) -> TemplateTally {
    let engine = state.exchange.lock().expect("exchange mutex poisoned");
    TemplateTally::collect(&engine, series)
}

/// `GET /api/strategies/templates?series=`
pub async fn list_templates(State(state): State<AppState>, Query(q): Query<SeriesQuery>) -> Json<TemplateCatalogueResponse> {
    let series = state.series_or_default(q.series);
    let tally = tally_for(&state, series);
    let readiness = Readiness::from_bus(&state.bus);
    Json(catalogue(&state.templates, series, &tally, &readiness))
}

/// `GET /api/strategies/templates/{id}?series=` — 404 unless the template
/// exists and is listed on the series.
pub async fn get_template(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<SeriesQuery>,
) -> Result<Json<TemplateView>, ApiError> {
    let series = state.series_or_default(q.series);
    let t = state.templates.get(&id).filter(|t| t.lists(series)).ok_or(ApiError::TemplateNotFound)?;
    let tally = tally_for(&state, series);
    let readiness = Readiness::from_bus(&state.bus);
    Ok(Json(template_view(t, series, &tally, &readiness)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::strategy::deploy_template_side;
    use crate::api::strategy::store::tests::PARENT;
    use crate::automation::signal_bus::{Quality, SignalFrame};
    use crate::engine::StrategyPatch;
    use crate::registry::{self, Inputs, PriceSource};
    use crate::series::SERIES;
    use crate::types::{Address, Tenths};
    use std::sync::Arc;

    fn full_catalogue() -> TemplateCatalogue {
        let registry = registry::build(&Inputs { price_source: PriceSource::GeminiIndex, live_lanes: &[], definitions_dir: None, copy_dir: None }).unwrap();
        TemplateCatalogue::load(&registry, PriceSource::GeminiIndex).unwrap_or_else(|e| panic!("{e}"))
    }

    fn empty() -> TemplateTally {
        TemplateTally::default()
    }

    fn all_ready() -> Readiness {
        Readiness::all_ready()
    }

    #[test]
    fn list_is_per_series_and_carries_both_sides() {
        let templates = full_catalogue();
        let two = catalogue(&templates, SERIES[0].id, &empty(), &all_ready());
        let fifteen = catalogue(&templates, SERIES[1].id, &empty(), &all_ready());
        assert_eq!(two.series, "btc-5m");
        assert_eq!((two.templates.len(), fifteen.templates.len()), (8, 9));
        let ids: Vec<&str> = two.templates.iter().map(|t| t.id.as_str()).collect();
        assert!(ids.contains(&"sweep-follower") && !ids.contains(&"trend-rider"));

        let fair = two.templates.iter().find(|t| t.id == "fair-value-discount").unwrap();
        assert_eq!(fair.title, "Fair Price Finder");
        assert_eq!(fair.series, vec!["btc-5m", "btc-15m"]);
        assert_eq!(fair.creator.handle, "@tiny");
        assert_eq!(fair.creator.username, "Tiny");
        assert_eq!(fair.creator.avatar.as_deref(), Some("strategy-01.jpg"));
        assert_eq!(fair.primary.name, "Buy the Bargain");
        assert_eq!(fair.inverse.name, "Ride the Odds");
        assert!(fair.primary.logic.contains("hunts for the next bargain"));
        assert!(fair.inverse.logic.contains("closes and switches sides"));
        assert_eq!(fair.envelope.max_order_size.as_str(), "200");
        assert_eq!(fair.envelope.order_fraction.as_str(), "0.2");
        assert_eq!((fair.running, fair.deployed_tenths), (0, 0));
        assert!(fair.runners.is_empty());

        let poly = fifteen.templates.iter().find(|t| t.id == "polymarket-discount").unwrap();
        assert_eq!(poly.creator.avatar, None, "no portrait assigned yet");
    }

    #[test]
    fn json_has_the_wire_keys() {
        let templates = full_catalogue();
        let view = template_view(templates.get("late-closer").unwrap(), SERIES[1].id, &empty(), &all_ready());
        let json = serde_json::to_value(&view).unwrap();
        for key in ["id", "title", "series", "creator", "envelope", "primary", "inverse", "running", "deployed_tenths", "runners", "ready", "waiting_on"] {
            assert!(json.get(key).is_some(), "missing {key}");
        }
        assert_eq!(json["creator"]["avatar"], "strategy-03.jpg");
        assert_eq!(json["primary"]["running"], 0);
        assert_eq!(json["primary"]["ready"], true);
        assert_eq!(json["primary"]["waiting_on"], serde_json::json!([]));
        assert_eq!(json["ready"], true);
        assert_eq!(json["envelope"]["loss_budget_fraction"], "1");
        assert_eq!(json["envelope"]["order_fraction"], "0.2");
    }

    #[tokio::test]
    async fn single_template_is_404_off_its_series_or_unknown() {
        let templates = full_catalogue();
        let (mut state, _) = crate::api::strategy::store::tests::app_state_pair();
        state.templates = std::sync::Arc::new(templates);

        let Json(view) = get_template(State(state.clone()), Path("trend-rider".into()), Query(SeriesQuery { series: Some(SERIES[1].id) }))
            .await
            .unwrap();
        assert_eq!(view.id, "trend-rider");

        let err = get_template(State(state.clone()), Path("trend-rider".into()), Query(SeriesQuery { series: Some(SERIES[0].id) }))
            .await
            .unwrap_err();
        assert!(matches!(err, ApiError::TemplateNotFound));
        let err = get_template(State(state.clone()), Path("nope".into()), Query(SeriesQuery::default())).await.unwrap_err();
        assert!(matches!(err, ApiError::TemplateNotFound));

        let Json(list) = list_templates(State(state), Query(SeriesQuery::default())).await;
        assert_eq!(list.series, "btc-5m");
        assert_eq!(list.templates.len(), 8);
    }

    /// A ready frame carrying every declared field of `signal_id`, typed
    /// by its registry declaration.
    fn typed_frame(bus: &SignalBus, signal_id: &str, ts_ms: u64) -> SignalFrame {
        use strategy_compiler::ir::FieldType;
        let def = bus.registry().signals.iter().find(|s| s.signal_id == signal_id).unwrap_or_else(|| panic!("{signal_id} is defined"));
        let values = def
            .fields
            .iter()
            .map(|f| {
                let v = match &f.field_type {
                    FieldType::Decimal => serde_json::json!("1.5"),
                    FieldType::Enum { variants } => serde_json::json!(variants[0]),
                    FieldType::Timestamp => serde_json::json!("2026-09-06T00:00:01Z"),
                    FieldType::Bool => serde_json::json!(true),
                    FieldType::String => serde_json::json!("s"),
                    _ => serde_json::json!(7),
                };
                (f.name.as_str().to_string(), v)
            })
            .collect();
        SignalFrame { signal_id: signal_id.into(), ts_ms, quality: Quality::ready(), values }
    }

    fn waiting_ids(view: &TemplateSideView) -> Vec<&str> {
        view.waiting_on.iter().map(|w| w.signal_id.as_str()).collect()
    }

    /// Readiness is per side from each side's own subscriptions, the
    /// card is the conjunction with the union of waiting signals, and a
    /// side reading only usable signals is ready.
    #[test]
    fn readiness_is_per_side_and_the_card_needs_both() {
        let registry = crate::automation::lane_bridge::tests::registry_with_spot_lane();
        let templates = TemplateCatalogue::load(&registry, PriceSource::GeminiIndex).unwrap_or_else(|e| panic!("{e}"));
        let bus = SignalBus::new(registry, None);
        let fifteen = SERIES[1].id;
        let late = templates.get("late-closer").unwrap();

        // Nothing has published: every side waits on everything it reads.
        let view = template_view(late, fifteen, &empty(), &Readiness::from_bus(&bus));
        assert!(!view.ready && !view.primary.ready && !view.inverse.ready);
        assert_eq!(waiting_ids(&view.primary), ["btc-15m.current.delta.v1", "btc-15m.current.price.v1", "btc-15m.current.window.v1"]);
        assert_eq!(waiting_ids(&view.inverse), ["btc-15m.current.price.v1", "btc-15m.current.window.v1"]);
        assert_eq!(view.waiting_on, view.primary.waiting_on, "the union, without duplicates");
        assert_eq!(view.waiting_on[0].name, "Distance from strike", "the copy title, not the spec name");

        // The inverse reads no delta, so it is ready before the primary.
        bus.publish(typed_frame(&bus, "btc-15m.current.price.v1", 1));
        bus.publish(typed_frame(&bus, "btc-15m.current.window.v1", 2));
        let view = template_view(late, fifteen, &empty(), &Readiness::from_bus(&bus));
        assert!(view.inverse.ready && view.inverse.waiting_on.is_empty());
        assert_eq!(waiting_ids(&view.primary), ["btc-15m.current.delta.v1"]);
        assert!(!view.ready, "one side waiting keeps the card waiting");

        bus.publish(typed_frame(&bus, "btc-15m.current.delta.v1", 3));
        let readiness = Readiness::from_bus(&bus);
        let view = template_view(late, fifteen, &empty(), &readiness);
        assert!(view.ready && view.primary.ready && view.waiting_on.is_empty());

        // The 2-minute internals never published, so the same template on
        // the other series still waits: readiness is per stamped document.
        let two = template_view(late, SERIES[0].id, &empty(), &readiness);
        assert!(!two.ready);
        assert_eq!(waiting_ids(&two.inverse), ["btc-5m.current.price.v1", "btc-5m.current.window.v1"]);

        // The list carries the same readiness as the single route.
        let list = catalogue(&templates, fifteen, &empty(), &readiness);
        let trend = list.templates.iter().find(|t| t.id == "trend-rider").unwrap();
        assert!(!trend.ready);
        assert_eq!(
            trend.waiting_on.iter().map(|w| (w.signal_id.as_str(), w.name.as_str())).collect::<Vec<_>>(),
            // Names come from the copy book, which titles both momentum
            // indicators the same way; the id tells them apart.
            [("btc.external.horizon_momentum.v1", "Momentum"), ("btc.external.micro_momentum.v1", "Momentum")]
        );
    }

    /// A lane signal is usable only once a frame has typed against the
    /// registry and reached the platform: a ready quality over frames the
    /// registry refuses is the definition mismatch, not readiness. Degraded
    /// clears readiness until the next ready frame.
    #[test]
    fn readiness_needs_a_typed_frame_and_follows_quality() {
        let registry = crate::automation::lane_bridge::tests::registry_with_spot_lane();
        let templates = TemplateCatalogue::load(&registry, PriceSource::GeminiIndex).unwrap_or_else(|e| panic!("{e}"));
        let bus = SignalBus::new(registry, None);
        let fifteen = SERIES[1].id;
        let trend = templates.get("trend-rider").unwrap();
        for id in ["btc-15m.current.price.v1", "btc.external.micro_momentum.v1"] {
            bus.publish(typed_frame(&bus, id, 1));
        }
        let horizon = "btc.external.horizon_momentum.v1";

        // Ready quality, but every field is wrong: refused for the platform.
        let mut bad = typed_frame(&bus, horizon, 2);
        bad.values.insert("return_bps_1m".into(), serde_json::json!("not a number"));
        bus.publish(bad);
        assert!(bus.ready_since(horizon).is_some(), "the bus calls it ready");
        assert_eq!(bus.ingest_stats(horizon).published, 0);
        let view = template_view(trend, fifteen, &empty(), &Readiness::from_bus(&bus));
        assert!(!view.ready);
        assert_eq!(waiting_ids(&view.primary), [horizon]);

        bus.publish(typed_frame(&bus, horizon, 3));
        let view = template_view(trend, fifteen, &empty(), &Readiness::from_bus(&bus));
        assert!(view.ready, "{:?}", view.waiting_on);

        // The lane reports it warming up: not usable until it is ready again.
        bus.set_quality(horizon, 4, Quality { state: "warming_up".into(), reasons: vec!["insufficient_history".into()] });
        let view = template_view(trend, fifteen, &empty(), &Readiness::from_bus(&bus));
        assert!(!view.ready && !view.inverse.ready);
        assert_eq!(waiting_ids(&view.inverse), [horizon]);

        bus.publish(typed_frame(&bus, horizon, 5));
        assert!(template_view(trend, fifteen, &empty(), &Readiness::from_bus(&bus)).ready);

        assert!(Readiness::all_ready().is_usable("anything.v1"));
        assert!(Readiness::from_bus(&bus).waiting_on(&[]).is_empty());
    }

    /// The routes read readiness from the state's bus.
    #[tokio::test]
    async fn routes_report_readiness_from_the_bus() {
        let registry = crate::automation::lane_bridge::tests::registry_with_spot_lane();
        let templates = TemplateCatalogue::load(&registry, PriceSource::GeminiIndex).unwrap_or_else(|e| panic!("{e}"));
        let (mut state, _) = crate::api::strategy::store::tests::app_state_with(registry);
        state.templates = Arc::new(templates);
        let series = SERIES[0].id;
        let Json(list) = list_templates(State(state.clone()), Query(SeriesQuery { series: Some(series) })).await;
        assert!(list.templates.iter().all(|t| !t.ready), "nothing published yet");

        for id in ["btc-5m.current.price.v1", "btc-5m.current.delta.v1", "btc-5m.current.window.v1"] {
            state.bus.publish(typed_frame(&state.bus, id, 1));
        }
        let Json(late) = get_template(State(state.clone()), Path("late-closer".into()), Query(SeriesQuery { series: Some(series) })).await.unwrap();
        assert!(late.ready, "{:?}", late.waiting_on);
        let Json(list) = list_templates(State(state), Query(SeriesQuery { series: Some(series) })).await;
        let by_id = |id: &str| list.templates.iter().find(|t| t.id == id).unwrap();
        assert!(by_id("fair-value-discount").ready, "internals only");
        assert!(!by_id("momentum-rider").ready, "external signals have not published");
    }

    /// Deployments through the template route show up on the cards: per
    /// side, per series, active ones only, with the parent's cash summed,
    /// and the creator's portrait once the creator's account runs it.
    #[tokio::test]
    async fn counts_follow_the_deployments_on_each_series() {
        // The platform must know the lane signals the templates subscribe to.
        let registry = crate::automation::lane_bridge::tests::registry_with_spot_lane();
        let templates = TemplateCatalogue::load(&registry, PriceSource::GeminiIndex).unwrap_or_else(|e| panic!("{e}"));
        let (mut state, _) = crate::api::strategy::store::tests::app_state_with(registry);
        state.templates = Arc::new(templates);
        let parent: Address = PARENT.parse().unwrap();
        let (two, fifteen) = (SERIES[0].id, SERIES[1].id);

        let a = deploy_template_side(&state, parent, "late-closer", two, Side::Primary, Tenths(50_000)).await.unwrap();
        deploy_template_side(&state, parent, "late-closer", two, Side::Primary, Tenths(30_000)).await.unwrap();
        let c = deploy_template_side(&state, parent, "late-closer", two, Side::Inverse, Tenths(20_000)).await.unwrap();
        deploy_template_side(&state, parent, "late-closer", fifteen, Side::Inverse, Tenths(10_000)).await.unwrap();
        deploy_template_side(&state, parent, "momentum-rider", two, Side::Primary, Tenths(10_000)).await.unwrap();

        let Json(late) = get_template(State(state.clone()), Path("late-closer".into()), Query(SeriesQuery { series: Some(two) })).await.unwrap();
        assert_eq!((late.primary.running, late.primary.deployed_tenths), (2, 80_000));
        assert_eq!((late.inverse.running, late.inverse.deployed_tenths), (1, 20_000));
        assert_eq!((late.running, late.deployed_tenths), (3, 100_000));
        assert!(late.runners.is_empty(), "the parent is not the creator");

        let Json(list) = list_templates(State(state.clone()), Query(SeriesQuery { series: Some(fifteen) })).await;
        let late15 = list.templates.iter().find(|t| t.id == "late-closer").unwrap();
        assert_eq!((late15.primary.running, late15.inverse.running, late15.deployed_tenths), (0, 1, 10_000));
        let mom15 = list.templates.iter().find(|t| t.id == "momentum-rider").unwrap();
        assert_eq!(mom15.running, 0, "the 2-minute deployment does not count on the 15-minute card");

        // A paused deployment stops counting; a deleted one is gone.
        let Json(_) = crate::api::strategy::update_strategy(
            State(state.clone()),
            Path((PARENT.into(), a.summary.id.to_string())),
            Json(StrategyPatch { name: None, status: Some(crate::engine::StrategyStatus::Paused) }),
        )
        .await
        .unwrap();
        let Json(_) = crate::api::strategy::delete_strategy(State(state.clone()), Path((PARENT.into(), c.summary.id.to_string()))).await.unwrap();
        let Json(late) = get_template(State(state.clone()), Path("late-closer".into()), Query(SeriesQuery { series: Some(two) })).await.unwrap();
        assert_eq!((late.primary.running, late.primary.deployed_tenths), (1, 30_000));
        assert_eq!((late.inverse.running, late.inverse.deployed_tenths), (0, 0));

        // The creator's own account running it puts the portrait on the card.
        state.exchange.lock().unwrap().get_or_create_account(parent).username = Some("Tiny".into());
        let Json(late) = get_template(State(state), Path("late-closer".into()), Query(SeriesQuery { series: Some(two) })).await.unwrap();
        assert_eq!(late.runners, vec!["strategy-03.jpg".to_string()]);
    }
}
