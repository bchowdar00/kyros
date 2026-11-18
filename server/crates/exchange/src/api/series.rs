//! `GET /api/series` — the series the server runs and where each one's
//! current round stands. Clients use it to build the series switcher
//! and to count down the rounds they are not currently watching.

use axum::{Json, extract::State};
use serde::Serialize;

use crate::engine::{Exchange, RoundReference};
use crate::gemini::{GeminiSeriesView, GeminiState};
use crate::types::SeriesId;

use super::AppState;

#[derive(Debug, Clone, Serialize)]
pub struct SeriesInfo {
    pub series: SeriesId,
    pub duration_secs: u64,
    /// What the rounds are about (`btc`, `nba`).
    pub underlying: &'static str,
    /// Outcome labels, positive side first (`up`/`down`, `home`/`away`).
    pub outcomes: [&'static str; 2],
    /// `None` until the series' first round opens.
    pub current_market_id: Option<u32>,
    pub opens_at_ms: Option<u64>,
    pub closes_at_ms: Option<u64>,
    /// What the open round opened on: `{"strike_btc_usd"}` for BTC,
    /// `{"home_team", "away_team", "pregame_home_prob", "game_seq"}` for NBA.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reference: Option<RoundReference>,
    /// The Gemini series and window this series follows, when it does.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gemini: Option<GeminiSeriesView>,
}

pub fn build_series_list(exchange: &Exchange, gemini: Option<&GeminiState>) -> Vec<SeriesInfo> {
    exchange
        .engines()
        .map(|e| {
            let market = e.current_market();
            SeriesInfo {
                series: e.series(),
                duration_secs: e.series().duration_secs(),
                underlying: e.series().underlying(),
                outcomes: e.series().outcomes(),
                current_market_id: market.map(|m| m.id.0),
                opens_at_ms: market.map(|m| m.opened_at * 1000),
                closes_at_ms: market.map(|m| m.closes_at * 1000),
                reference: market.map(|m| m.reference),
                gemini: gemini.and_then(|g| g.view(e.series())),
            }
        })
        .collect()
}

pub async fn list_series(State(state): State<AppState>) -> Json<Vec<SeriesInfo>> {
    let exchange = state.exchange.lock().expect("exchange mutex poisoned");
    Json(build_series_list(&exchange, state.gemini.as_ref()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::series::SERIES;
    use crate::types::Tenths;

    #[test]
    fn lists_every_series_with_its_round() {
        let mut x = Exchange::new(Tenths(1_000_000), SERIES);
        x.open_first_market(SeriesId(1), 100_000.0, 10, 900);
        let list = build_series_list(&x, None);
        assert_eq!(list.len(), SERIES.len());
        assert_eq!(list[0].series, SeriesId(0));
        assert_eq!(list[0].current_market_id, None);
        assert_eq!(list[1].series, SeriesId(1));
        assert_eq!(list[1].duration_secs, 900);
        assert_eq!(list[1].current_market_id, Some(1));
        assert_eq!(list[1].opens_at_ms, Some(10_000));
        assert_eq!(list[1].closes_at_ms, Some(910_000));
        let json = serde_json::to_value(&list).unwrap();
        assert_eq!(json[1]["series"], "btc-15m");
        assert_eq!(json[1]["underlying"], "btc");
        assert_eq!(json[1]["outcomes"], serde_json::json!(["up", "down"]));
        assert_eq!(json[1]["reference"]["strike_btc_usd"], 100_000.0);
        assert!(json[0].get("reference").is_none(), "no round, no reference");
        assert!(json[0].get("gemini").is_none(), "not running on Gemini");
    }

    #[test]
    fn a_series_that_follows_gemini_says_which_window() {
        use crate::gemini::audit::RoundAudit;
        use crate::gemini::driver::SeriesLink;
        use crate::gemini::env::SANDBOX;
        use crate::gemini::strikes::GeminiClock;

        let x = Exchange::new(Tenths(1_000_000), SERIES);
        let mut gemini = GeminiState::new(SANDBOX, GeminiClock::starting_at(0), RoundAudit::new());
        gemini.follow(SeriesId(1), "BTC15M", SeriesLink::new());
        let json = serde_json::to_value(build_series_list(&x, Some(&gemini))).unwrap();
        assert_eq!(json[1]["gemini"], serde_json::json!({ "env": "sandbox", "series": "BTC15M", "phase": "waiting_for_window" }));
        assert!(json[0].get("gemini").is_none(), "series 0 was not registered");
    }
}
