//! `GET /health`: whether this server is fit to be shown, and why not.
//!
//! It answers `200` only when the index price is arriving, every series
//! has a round open (or closed one within the last minute and is waiting
//! on Gemini for the result or the next window), and every series has
//! strategy templates. Otherwise `503` with the reasons. The body is the
//! same shape either way. The offset between this host's clock and
//! Gemini's is always in it and is flagged over two seconds; it does not
//! fail the check, because round times are Gemini's own.
//!
//! `GET /live` is the plain "the process is serving" answer. A host that
//! restarts on a failed probe must be pointed at `/live`: a restart
//! wipes every account, and Gemini being late is no reason for that.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

use super::AppState;
use crate::gemini::GeminiState;
use crate::series::SeriesId;
use crate::templates::TemplateCatalogue;
use crate::gemini::driver::Phase;
use crate::gemini::mirror_task::BookState;
use crate::gemini::strikes::CLOCK_WARN_MS;
use crate::gemini::venue::VenueStatus;

/// The index price arrives once a second; older than this, it is not arriving.
pub const FEED_STALE_MS: u64 = 10_000;
/// How long a series may sit between rounds. Gemini publishes a result
/// within about ten seconds and the next window is already listed.
pub const BETWEEN_ROUNDS_MS: u64 = 60_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeriesInput {
    pub series: &'static str,
    pub phase: Phase,
    /// Expiry of the window the series last followed, if it has followed one.
    pub expires_at_ms: Option<u64>,
    pub book: BookState,
    pub templates: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthInputs {
    /// Now, on Gemini's clock.
    pub now_ms: u64,
    /// The newest second the index price was received for.
    pub last_index_second: Option<u64>,
    pub clock_offset_ms: i64,
    pub series: Vec<SeriesInput>,
    /// The signed order link; `None` on a paper server.
    pub orders: Option<VenueStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SeriesHealth {
    pub series: &'static str,
    pub phase: Phase,
    pub book: BookState,
    pub templates: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HealthReport {
    pub ok: bool,
    /// Why not, in words; empty when `ok`.
    pub problems: Vec<String>,
    pub feed_age_ms: Option<u64>,
    pub clock_offset_ms: i64,
    /// This host's clock is more than two seconds from Gemini's.
    pub clock_off: bool,
    pub series: Vec<SeriesHealth>,
    /// The signed order link; absent on a paper server.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub orders: Option<VenueStatus>,
}

pub fn assess(inputs: &HealthInputs) -> HealthReport {
    let mut problems = Vec::new();
    let feed_age_ms = inputs.last_index_second.map(|second| inputs.now_ms.saturating_sub(second * 1000));
    match feed_age_ms {
        None => problems.push("no index price has arrived".to_owned()),
        Some(age) if age > FEED_STALE_MS => problems.push(format!("the index price is {} s old", age / 1000)),
        Some(_) => {}
    }
    if inputs.series.is_empty() {
        problems.push("no series follows gemini".to_owned());
    }
    for series in &inputs.series {
        if series.phase != Phase::Open {
            match series.expires_at_ms {
                None => problems.push(format!("{}: no round yet", series.series)),
                Some(expiry) => {
                    let since = inputs.now_ms.saturating_sub(expiry);
                    if since > BETWEEN_ROUNDS_MS {
                        problems.push(format!("{}: no round open for {} s", series.series, since / 1000));
                    }
                }
            }
        }
        if series.templates == 0 {
            problems.push(format!("{}: no strategy templates", series.series));
        }
    }
    if let Some(orders) = &inputs.orders {
        if !orders.connected {
            problems.push("real orders: not connected to Gemini's order socket".to_owned());
        }
        if let Some(problem) = &orders.problem {
            problems.push(format!("real orders: {problem}"));
        }
    }
    HealthReport {
        ok: problems.is_empty(),
        problems,
        feed_age_ms,
        clock_offset_ms: inputs.clock_offset_ms,
        clock_off: inputs.clock_offset_ms.abs() > CLOCK_WARN_MS,
        series: inputs
            .series
            .iter()
            .map(|s| SeriesHealth { series: s.series, phase: s.phase, book: s.book, templates: s.templates })
            .collect(),
        orders: inputs.orders.clone(),
    }
}

/// What the check reads from the running server: Gemini's clock, the
/// newest index second, and one row per series in the table. A series
/// with no Gemini link reads as never having had a round.
pub fn gather(gemini: &GeminiState, templates: &TemplateCatalogue) -> HealthInputs {
    let series = SeriesId::all()
        .map(|id| {
            let view = gemini.view(id);
            SeriesInput {
                series: id.slug(),
                phase: view.as_ref().map_or(Phase::WaitingForWindow, |v| v.phase),
                expires_at_ms: view.as_ref().and_then(|v| v.expires_at_ms),
                book: view.as_ref().map_or(BookState::Waiting, |v| v.book),
                templates: templates.for_series(id).count(),
            }
        })
        .collect();
    HealthInputs {
        now_ms: gemini.clock.now_ms(),
        last_index_second: gemini.last_index_second(),
        clock_offset_ms: gemini.clock.offset_ms(),
        series,
        orders: gemini.orders_status(),
    }
}

/// `200` with the report when fit, `503` with the same shape when not.
pub async fn get_health(State(state): State<AppState>) -> Response {
    let report = match &state.gemini {
        Some(gemini) => assess(&gather(gemini, &state.templates)),
        None => HealthReport {
            ok: false,
            problems: vec!["this server is not following Gemini".into()],
            feed_age_ms: None,
            clock_offset_ms: 0,
            clock_off: false,
            series: Vec::new(),
            orders: None,
        },
    };
    let status = if report.ok { StatusCode::OK } else { StatusCode::SERVICE_UNAVAILABLE };
    (status, Json(report)).into_response()
}

pub async fn get_live() -> &'static str {
    "ok"
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_795_030_000_000;

    fn open(series: &'static str) -> SeriesInput {
        SeriesInput { series, phase: Phase::Open, expires_at_ms: Some(NOW + 120_000), book: BookState::Live, templates: 11 }
    }

    fn good() -> HealthInputs {
        HealthInputs { now_ms: NOW, last_index_second: Some(NOW / 1000 - 1), clock_offset_ms: 140, series: vec![open("btc-5m"), open("btc-15m")], orders: None }
    }

    #[test]
    fn real_orders_without_a_working_order_link_are_not_ok() {
        let mut inputs = good();
        inputs.orders = Some(VenueStatus { connected: true, ..VenueStatus::default() });
        let fit = assess(&inputs);
        assert!(fit.ok);
        assert_eq!(fit.orders, inputs.orders);
        inputs.orders = Some(VenueStatus::default());
        assert_eq!(assess(&inputs).problems, vec!["real orders: not connected to Gemini's order socket".to_owned()]);
        inputs.orders = Some(VenueStatus { connected: true, problem: Some("a Gemini fill of 2 could not be booked".into()), ..VenueStatus::default() });
        assert_eq!(assess(&inputs).problems, vec!["real orders: a Gemini fill of 2 could not be booked".to_owned()]);
    }

    #[test]
    fn a_server_with_the_feed_arriving_and_every_round_open_is_ok() {
        let report = assess(&good());
        assert!(report.ok);
        assert_eq!(report.problems, Vec::<String>::new());
        assert_eq!(report.feed_age_ms, Some(1_000));
        assert_eq!((report.clock_offset_ms, report.clock_off), (140, false));
        assert_eq!(report.series.len(), 2);
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["series"][0], serde_json::json!({ "series": "btc-5m", "phase": "open", "book": "live", "templates": 11 }));
    }

    #[test]
    fn no_index_price_or_an_old_one_is_not_ok() {
        let mut never = good();
        never.last_index_second = None;
        let report = assess(&never);
        assert!(!report.ok);
        assert_eq!(report.problems, vec!["no index price has arrived".to_owned()]);
        assert_eq!(report.feed_age_ms, None);

        let mut old = good();
        old.last_index_second = Some(NOW / 1000 - 11);
        assert_eq!(assess(&old).problems, vec!["the index price is 11 s old".to_owned()]);
        let mut fresh_enough = good();
        fresh_enough.last_index_second = Some(NOW / 1000 - 10);
        assert!(assess(&fresh_enough).ok);
    }

    #[test]
    fn a_series_between_rounds_is_ok_for_a_minute_and_then_is_not() {
        let waiting = |phase, ago_ms: u64| {
            let mut inputs = good();
            inputs.series[0] = SeriesInput { phase, expires_at_ms: Some(NOW - ago_ms), book: BookState::Waiting, ..open("btc-5m") };
            assess(&inputs)
        };
        assert!(waiting(Phase::WaitingForResult, 8_000).ok);
        assert!(waiting(Phase::WaitingForWindow, 60_000).ok);
        let late = waiting(Phase::WaitingForResult, 61_000);
        assert!(!late.ok);
        assert_eq!(late.problems, vec!["btc-5m: no round open for 61 s".to_owned()]);
        // A window that is listed and has not started yet is not late.
        let mut upcoming = good();
        upcoming.series[1] = SeriesInput { phase: Phase::WaitingForWindow, ..open("btc-15m") };
        assert!(assess(&upcoming).ok);
    }

    #[test]
    fn a_series_that_has_never_had_a_round_is_not_ok() {
        let mut inputs = good();
        inputs.series[1] = SeriesInput { phase: Phase::WaitingForWindow, expires_at_ms: None, book: BookState::Waiting, ..open("btc-15m") };
        let report = assess(&inputs);
        assert!(!report.ok);
        assert_eq!(report.problems, vec!["btc-15m: no round yet".to_owned()]);
    }

    #[test]
    fn a_series_with_no_templates_and_a_server_with_no_series_are_not_ok() {
        let mut inputs = good();
        inputs.series[0].templates = 0;
        assert_eq!(assess(&inputs).problems, vec!["btc-5m: no strategy templates".to_owned()]);
        let mut none = good();
        none.series.clear();
        assert_eq!(assess(&none).problems, vec!["no series follows gemini".to_owned()]);
    }

    #[test]
    fn every_problem_is_listed_together() {
        let mut inputs = good();
        inputs.last_index_second = None;
        inputs.series[0].templates = 0;
        inputs.series[1] = SeriesInput { phase: Phase::WaitingForWindow, expires_at_ms: None, ..open("btc-15m") };
        assert_eq!(assess(&inputs).problems.len(), 3);
    }

    #[test]
    fn a_clock_two_seconds_out_is_flagged_and_does_not_fail_the_check() {
        let mut inputs = good();
        inputs.clock_offset_ms = -2_001;
        let report = assess(&inputs);
        assert!(report.ok);
        assert!(report.clock_off);
        inputs.clock_offset_ms = 2_000;
        assert!(!assess(&inputs).clock_off);
    }

    use crate::gemini::audit::RoundAudit;
    use crate::gemini::driver::SeriesLink;
    use crate::gemini::env::PRODUCTION;
    use crate::gemini::strikes::GeminiClock;
    use crate::gemini::types::UpWindow;
    use crate::registry::PriceSource;
    use std::sync::Arc;

    fn window(expires_at_ms: u64) -> UpWindow {
        UpWindow {
            event_ticker: "BTC05M2611181730".into(),
            symbol: "GEMI-BTC05M2611181730-UP".into(),
            starts_at_ms: expires_at_ms - 300_000,
            expires_at_ms,
            strike: None,
            market_state: "open".into(),
        }
    }

    /// A Gemini state whose clock reads `NOW`, following the first series only.
    fn gemini() -> (GeminiState, SeriesLink) {
        let mut state = GeminiState::new(PRODUCTION, GeminiClock::starting_at(NOW), RoundAudit::new());
        let link = SeriesLink::new();
        state.follow(SeriesId(0), "BTC05M", link.clone());
        (state, link)
    }

    fn catalogue() -> TemplateCatalogue {
        let registry = crate::automation::lane_bridge::tests::registry_with_spot_lane();
        TemplateCatalogue::load(&registry, PriceSource::GeminiIndex).unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn gather_reads_the_clock_the_index_and_one_row_per_series() {
        let (gemini, link) = gemini();
        gemini.index.record(NOW / 1000 - 1, 62_000.0);
        link.set(Phase::Open, Some(window(NOW + 120_000)));
        link.set_book(BookState::Live);
        let catalogue = catalogue();

        let inputs = gather(&gemini, &catalogue);

        assert!((NOW..NOW + 5_000).contains(&inputs.now_ms), "{}", inputs.now_ms);
        assert_eq!(inputs.last_index_second, Some(NOW / 1000 - 1));
        assert_eq!(inputs.clock_offset_ms, 0);
        let all: Vec<SeriesId> = SeriesId::all().collect();
        assert_eq!(inputs.series.len(), all.len());
        let first = &inputs.series[0];
        assert_eq!((first.series, first.phase, first.expires_at_ms, first.book), (all[0].slug(), Phase::Open, Some(NOW + 120_000), BookState::Live));
        assert_eq!(first.templates, catalogue.for_series(all[0]).count());
        assert!(first.templates > 0);
        // The second series has no link here: it reads as never having had a round.
        let second = &inputs.series[1];
        assert_eq!((second.series, second.phase, second.expires_at_ms, second.book), (all[1].slug(), Phase::WaitingForWindow, None, BookState::Waiting));
    }

    async fn body(response: Response) -> (StatusCode, serde_json::Value) {
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    fn app_state(gemini: Option<GeminiState>, templates: TemplateCatalogue) -> AppState {
        let (mut state, _) = crate::api::strategy::store::tests::app_state_pair();
        state.gemini = gemini;
        state.templates = Arc::new(templates);
        state
    }

    #[tokio::test]
    async fn the_route_answers_200_when_every_series_is_fit() {
        let mut gemini = GeminiState::new(PRODUCTION, GeminiClock::starting_at(NOW), RoundAudit::new());
        gemini.index.record(NOW / 1000, 62_000.0);
        for (id, name) in SeriesId::all().zip(["BTC05M", "BTC15M"]) {
            let link = SeriesLink::new();
            link.set(Phase::Open, Some(window(NOW + 120_000)));
            link.set_book(BookState::Live);
            gemini.follow(id, name, link);
        }
        let (status, json) = body(get_health(State(app_state(Some(gemini), catalogue()))).await).await;
        assert_eq!(status, StatusCode::OK, "{json}");
        assert_eq!(json["ok"], true);
        assert_eq!(json["problems"], serde_json::json!([]));
        assert_eq!(json["series"].as_array().unwrap().len(), SeriesId::all().count());
        assert_eq!(json["series"][0]["phase"], "open");
        assert_eq!(json["series"][0]["book"], "live");
    }

    #[tokio::test]
    async fn the_route_answers_503_with_the_reasons_when_not() {
        // The feed has never delivered and no series has had a round.
        let (gemini, _) = gemini();
        let (status, json) = body(get_health(State(app_state(Some(gemini), catalogue()))).await).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(json["ok"], false);
        assert!(json["problems"].as_array().unwrap().len() >= 3, "{json}");
    }

    #[tokio::test]
    async fn a_server_that_does_not_follow_gemini_answers_503() {
        let (status, json) = body(get_health(State(app_state(None, TemplateCatalogue::default()))).await).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(json["ok"], false);
        assert_eq!(json["problems"], serde_json::json!(["this server is not following Gemini"]));
    }

    #[tokio::test]
    async fn live_answers_whatever_the_state() {
        assert_eq!(get_live().await, "ok");
    }
}
