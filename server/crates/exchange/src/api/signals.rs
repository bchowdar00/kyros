//! `GET /api/signals?series=<slug>` — the signal catalogue for one market.
//!
//! The registry answers "which signals apply to this series"
//! ([`crate::registry::signals_for`]); the bus answers "what is each one
//! saying right now" ([`SignalBus::latest`]). This endpoint joins the
//! two into what the market page needs to draw its signal cards and its
//! Browse All list: every applicable signal with its readable name,
//! description, scope, lane, readiness and its latest published values,
//! field by field, each field carrying the label a card shows.
//!
//! The pin unit is a field, so every field is addressable on its own:
//! `signal_id` + `name` is the key a pinned card is stored under.
//!
//! Values keep the bus's wire convention (decimals and timestamps as
//! strings, integers as numbers, flags as booleans) and are absent until
//! the signal first publishes.
//!
//! `GET /api/signals/history?series=&ids=a,b&history_ms=&max_points=`
//! answers with the frames the bus retained for each id over the window,
//! oldest first and evenly thinned, in the same shape as the socket's
//! `history` message. It is the read path for a caller that wants recent
//! values without holding a socket, such as the chat agent's read tool.

use std::collections::BTreeMap;

use axum::{
    Json,
    extract::{Query, State},
};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json_;
use strategy_compiler::ir::registry::{FieldType, SignalDef, SignalLocation};

use crate::api::signals_ws::{SIGNALS_WS_HISTORY_MAX_POINTS, SignalHistory, Subscription};
use crate::api::{ApiError, AppState, SeriesQuery};
use crate::automation::signal_bus::{HISTORY_WINDOW_MS, IngestStats, Quality, SignalBus};
use crate::registry::{Scope, signals_for};
use crate::types::SeriesId;

/// One field of a catalogued signal, with its current value if published.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CatalogueField {
    pub name: String,
    pub label: String,
    /// The registry type tag: `decimal`, `u32`, `enum`, `duration_ms`, …
    #[serde(rename = "type")]
    pub field_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variants: Option<Vec<String>>,
    /// Two sentences on what the number means, from `copy/signals`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Shown by default; the rest are plumbing behind "Show all fields".
    pub headline: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<Json_>,
}

/// One catalogued signal for the requested series.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CatalogueSignal {
    pub signal_id: String,
    pub name: String,
    pub description: String,
    pub location: SignalLocation,
    pub scope: Scope,
    /// The lane that publishes it, for definition-file signals.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lane: Option<String>,
    /// Whether something in this process publishes it.
    pub live: bool,
    /// Readiness as last reported; `unknown` until the first frame.
    pub quality: Quality,
    /// When the latest values were published, unix milliseconds. Absent
    /// until the first frame.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ts_ms: Option<u64>,
    /// When the signal last became ready, unix milliseconds. Absent while
    /// it is warming up, degraded or unavailable, or has never published,
    /// so a card can tell "waiting on this signal" from "dead".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ready_since_ms: Option<u64>,
    /// How the signal's frames have fared at the registry typing step:
    /// frames handed to the strategy platform, frames refused, and why the
    /// last one was refused. A signal that is publishing but never
    /// `published` here is exactly one whose values do not match its
    /// definition, which is the failure to look for first.
    pub ingest: IngestStats,
    pub fields: Vec<CatalogueField>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SignalCatalogue {
    pub series: String,
    pub underlying: String,
    pub signals: Vec<CatalogueSignal>,
}

fn type_tag(ty: &FieldType) -> (String, Option<Vec<String>>) {
    match ty {
        FieldType::Decimal => ("decimal".into(), None),
        FieldType::U8 => ("u8".into(), None),
        FieldType::U16 => ("u16".into(), None),
        FieldType::U32 => ("u32".into(), None),
        FieldType::U64 => ("u64".into(), None),
        FieldType::I64 => ("i64".into(), None),
        FieldType::Enum { variants } => ("enum".into(), Some(variants.clone())),
        FieldType::DurationMs => ("duration_ms".into(), None),
        FieldType::Timestamp => ("timestamp".into(), None),
        FieldType::Bool => ("bool".into(), None),
        FieldType::String => ("string".into(), None),
    }
}

/// Join one registry entry with its latest bus frame.
pub fn catalogue_signal(def: &SignalDef, bus: &SignalBus) -> Option<CatalogueSignal> {
    let scope = Scope::of(def)?;
    let meta = def.metadata.as_ref();
    let lane = meta.and_then(|m| m.get("lane")).and_then(Json_::as_str).map(str::to_string);
    let latest = bus.latest(&def.signal_id);
    let values: BTreeMap<String, Json_> = latest.as_ref().map(|f| f.values.clone()).unwrap_or_default();
    // Exchange internals are published by this process by construction;
    // definition-file signals only when their lane runs.
    let live = match meta.and_then(|m| m.get("live")).and_then(Json_::as_bool) {
        Some(live) => live,
        None => matches!(scope, Scope::Series { .. }) || def.location == SignalLocation::External,
    };
    let fields = def
        .fields
        .iter()
        .map(|f| {
            let (field_type, variants) = type_tag(&f.field_type);
            let name = f.name.as_str().to_string();
            CatalogueField {
                label: f.label.clone().unwrap_or_else(|| crate::registry::field_label(&name)),
                description: f.description.clone(),
                headline: f.headline.unwrap_or(false),
                value: values.get(&name).cloned(),
                name,
                field_type,
                variants,
            }
        })
        .collect();
    Some(CatalogueSignal {
        signal_id: def.signal_id.clone(),
        name: def.name.clone().unwrap_or_else(|| def.signal_id.clone()),
        description: def.description.clone(),
        location: def.location,
        scope,
        lane,
        live,
        quality: latest
            .as_ref()
            .map(|f| f.quality.clone())
            .unwrap_or_else(|| Quality { state: "unknown".into(), reasons: Vec::new() }),
        ts_ms: latest.as_ref().map(|f| f.ts_ms),
        ready_since_ms: bus.ready_since(&def.signal_id),
        ingest: bus.ingest_stats(&def.signal_id),
        fields,
    })
}

/// The catalogue for `series`, in registry order.
pub fn catalogue(bus: &SignalBus, series: SeriesId) -> SignalCatalogue {
    let slug = series.slug();
    SignalCatalogue {
        series: slug.to_string(),
        underlying: series.underlying().to_string(),
        signals: signals_for(bus.registry(), slug).filter_map(|d| catalogue_signal(d, bus)).collect(),
    }
}

pub async fn list_signals(State(state): State<AppState>, Query(q): Query<SeriesQuery>) -> Json<SignalCatalogue> {
    let series = state.series_or_default(q.series);
    Json(catalogue(&state.bus, series))
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SignalsHistoryQuery {
    #[serde(default)]
    pub series: Option<SeriesId>,
    /// Comma-separated signal ids. Absent means every signal of the series.
    #[serde(default)]
    pub ids: Option<String>,
    /// How far back from now, in milliseconds; bounded by the bus's window.
    #[serde(default)]
    pub history_ms: Option<u64>,
    /// Frames per signal after thinning.
    #[serde(default)]
    pub max_points: Option<usize>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SignalsHistory {
    pub series: String,
    pub since_ms: u64,
    pub signals: Vec<SignalHistory>,
}

/// The retained frames of the requested signals, refusing ids that do
/// not apply to the series so a pin from another market cannot read
/// through. `now_ms` is a parameter so tests can pin the clock.
pub fn signals_history(bus: &SignalBus, series: SeriesId, q: &SignalsHistoryQuery, now_ms: u64) -> Result<SignalsHistory, ApiError> {
    let ids: Vec<String> = q
        .ids
        .as_deref()
        .map(|list| list.split(',').map(str::trim).filter(|id| !id.is_empty()).map(str::to_string).collect())
        .unwrap_or_default();
    let whole_series = q.ids.is_none();
    let (subscription, refused) = Subscription::resolve(bus, series, &ids, whole_series);
    if !refused.is_empty() {
        return Err(ApiError::UnknownSignals { series, ids: refused });
    }
    let window = q.history_ms.unwrap_or(HISTORY_WINDOW_MS).min(HISTORY_WINDOW_MS);
    let since_ms = now_ms.saturating_sub(window);
    let max_points = q.max_points.unwrap_or(SIGNALS_WS_HISTORY_MAX_POINTS);
    Ok(SignalsHistory { series: series.slug().to_string(), since_ms, signals: subscription.history(bus, since_ms, max_points) })
}

pub async fn get_signals_history(
    State(state): State<AppState>,
    Query(q): Query<SignalsHistoryQuery>,
) -> Result<Json<SignalsHistory>, ApiError> {
    let series = state.series_or_default(q.series);
    let now_ms = crate::pricing::feed::unix_now_ms();
    signals_history(&state.bus, series, &q, now_ms).map(Json)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::automation::signal_bus::SignalFrame;
    use crate::registry::{self, stats_signal_id};
    use crate::series::SERIES;
    use serde_json::json;

    #[test]
    fn catalogue_lists_only_the_series_signals_with_labels_and_values() {
        let bus = SignalBus::new(registry::for_tests(), None);
        let a = SERIES[0].id;
        let b = SERIES[1].id;
        let stats_a = stats_signal_id(a.slug());
        bus.publish(SignalFrame {
            signal_id: stats_a.clone(),
            ts_ms: 42,
            quality: Quality::ready(),
            values: [("volume".to_string(), json!("2365.000")), ("trades".to_string(), json!(29))].into_iter().collect(),
        });

        let cat = catalogue(&bus, a);
        assert_eq!(cat.series, a.slug());
        assert_eq!(cat.underlying, "btc");
        let ids: Vec<&str> = cat.signals.iter().map(|s| s.signal_id.as_str()).collect();
        assert!(ids.contains(&stats_a.as_str()));
        assert!(!ids.contains(&stats_signal_id(b.slug()).as_str()), "other series' internals excluded");
        assert!(ids.contains(&"coinbase.btc_usd.spot.v1"), "underlying-scoped feed included");

        let stats = cat.signals.iter().find(|s| s.signal_id == stats_a).unwrap();
        assert_eq!(stats.name, "Round Stats");
        assert_eq!(stats.scope, Scope::Series { series_id: a.slug().to_string() });
        assert!(stats.live);
        assert_eq!(stats.quality.state, "ready");
        assert_eq!(stats.ts_ms, Some(42));
        assert_eq!(stats.ready_since_ms, Some(42));
        let volume = stats.fields.iter().find(|f| f.name == "volume").unwrap();
        assert_eq!(volume.label, "Round Volume");
        assert_eq!(volume.field_type, "decimal");
        assert_eq!(volume.value, Some(json!("2365.000")));
        let traders = stats.fields.iter().find(|f| f.name == "traders").unwrap();
        assert_eq!(traders.value, None, "unpublished fields carry no value");

        // The stats frame above carried two of the declared fields, so it
        // was fanned out but refused for the platform; the catalogue says so.
        assert_eq!((stats.ingest.published, stats.ingest.rejected), (0, 1));
        assert!(stats.ingest.last_reject.as_deref().unwrap().contains("missing field"), "{:?}", stats.ingest);

        let window = cat.signals.iter().find(|s| s.signal_id.ends_with(".current.window.v1")).unwrap();
        assert_eq!(window.quality.state, "unknown");
        assert_eq!(window.ts_ms, None);
        assert_eq!(window.ready_since_ms, None);
        assert_eq!(window.ingest, IngestStats::default());
        let phase = window.fields.iter().find(|f| f.name == "phase").unwrap();
        assert_eq!(phase.field_type, "enum");
        assert_eq!(phase.variants.as_deref(), Some(&["open".to_string(), "locked".into(), "settled".into()][..]));

        // Both series see the same feed set.
        let feeds = |c: &SignalCatalogue| -> Vec<String> {
            c.signals.iter().filter(|s| !matches!(s.scope, Scope::Series { .. })).map(|s| s.signal_id.clone()).collect()
        };
        assert_eq!(feeds(&cat), feeds(&catalogue(&bus, b)));
    }

    #[test]
    fn history_route_reads_the_ring_and_refuses_foreign_ids() {
        let bus = SignalBus::new(registry::for_tests(), None);
        let a = SERIES[0].id;
        let stats = stats_signal_id(a.slug());
        for ts in [1_000, 2_000, 3_000] {
            bus.publish(SignalFrame {
                signal_id: stats.clone(),
                ts_ms: ts,
                quality: Quality::ready(),
                values: [("trades".to_string(), json!(ts))].into_iter().collect(),
            });
        }
        let query = SignalsHistoryQuery { ids: Some(format!("{stats}, coinbase.btc_usd.spot.v1")), history_ms: Some(1_500), ..Default::default() };
        let out = signals_history(&bus, a, &query, 3_000).unwrap();
        assert_eq!(out.series, a.slug());
        assert_eq!(out.since_ms, 1_500);
        assert_eq!(out.signals.len(), 1, "the unpublished feed is omitted");
        assert_eq!(out.signals[0].frames.iter().map(|f| f.ts_ms).collect::<Vec<_>>(), vec![2_000, 3_000]);

        // No ids means the whole series; the window is capped at the ring's.
        let everything = signals_history(&bus, a, &SignalsHistoryQuery { history_ms: Some(u64::MAX), ..Default::default() }, 3_000).unwrap();
        assert_eq!(everything.since_ms, 0);
        assert_eq!(everything.signals.len(), 1);

        let foreign = SignalsHistoryQuery { ids: Some(format!("{},nope", stats_signal_id(SERIES[1].slug))), ..Default::default() };
        match signals_history(&bus, a, &foreign, 3_000) {
            Err(ApiError::UnknownSignals { series, ids }) => {
                assert_eq!(series, a);
                assert_eq!(ids, vec![stats_signal_id(SERIES[1].slug), "nope".to_string()]);
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn serializes_with_wire_names() {
        let bus = SignalBus::new(registry::for_tests(), None);
        let cat = catalogue(&bus, SERIES[0].id);
        let v = serde_json::to_value(&cat).unwrap();
        let first = &v["signals"][0];
        assert!(first["fields"][0]["type"].is_string());
        assert!(first["fields"][0]["headline"].is_boolean());
        assert!(first["scope"]["kind"].is_string());
        assert!(first.get("ts_ms").is_none());
        assert!(first.get("ready_since_ms").is_none());
        assert_eq!(first["ingest"], json!({"published": 0, "rejected": 0}), "counters always present, reason only when set");
    }
}
