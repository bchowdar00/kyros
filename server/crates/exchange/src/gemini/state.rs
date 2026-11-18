//! What the rest of the server knows about the Gemini link: which
//! environment, which Gemini series each of our series follows, what
//! each driver is doing, and the audit of results.

use rust_decimal::Decimal;
use serde::Serialize;

use super::audit::{AuditSummary, RoundAudit};
use super::driver::{Phase, SeriesLink};
use super::env::GeminiEnv;
use super::mirror_task::BookState;
use super::strikes::GeminiClock;
use super::ws::PublicSocket;
use crate::types::SeriesId;

/// Rounds returned by `GET /api/rounds/audit`, newest first.
pub const AUDIT_RECENT: usize = 50;

/// The Gemini series one of ours follows, by our slug.
pub fn gemini_series_for(slug: &str) -> Option<&'static str> {
    match slug {
        "btc-5m" => Some("BTC05M"),
        "btc-15m" => Some("BTC15M"),
        _ => None,
    }
}

/// One series' Gemini link, as `GET /api/series` shows it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GeminiSeriesView {
    /// `production` or `sandbox`.
    pub env: &'static str,
    /// Gemini's series name, e.g. `BTC05M`.
    pub series: &'static str,
    pub phase: Phase,
    /// Whether the series' book is Gemini's live book right now.
    pub book: BookState,
    /// The window the open (or just closed) round follows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_ticker: Option<String>,
    /// Gemini's instrument symbol for that window's Up contract.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    /// Gemini's strike, exactly as published.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strike: Option<Decimal>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub starts_at_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<u64>,
}

/// Shared with the API. Cheap to clone.
#[derive(Debug, Clone)]
pub struct GeminiState {
    pub env: GeminiEnv,
    pub clock: GeminiClock,
    pub audit: RoundAudit,
    /// The public socket the strikes, the index and (later) the books
    /// come from. Held here so it lives as long as the server; `None`
    /// in tests.
    pub socket: Option<PublicSocket>,
    links: Vec<(SeriesId, &'static str, SeriesLink)>,
}

impl GeminiState {
    pub fn new(env: GeminiEnv, clock: GeminiClock, audit: RoundAudit) -> Self {
        Self { env, clock, audit, socket: None, links: Vec::new() }
    }

    /// Register the link of a series that follows `gemini_series`.
    pub fn follow(&mut self, series: SeriesId, gemini_series: &'static str, link: SeriesLink) {
        self.links.push((series, gemini_series, link));
    }

    /// `None` for a series that does not follow Gemini.
    pub fn view(&self, series: SeriesId) -> Option<GeminiSeriesView> {
        let (_, gemini_series, link) = self.links.iter().find(|(id, _, _)| *id == series)?;
        let window = link.window();
        Some(GeminiSeriesView {
            env: self.env.name,
            series: gemini_series,
            phase: link.phase(),
            book: link.book(),
            strike: window.as_ref().and_then(|w| w.strike),
            starts_at_ms: window.as_ref().map(|w| w.starts_at_ms),
            expires_at_ms: window.as_ref().map(|w| w.expires_at_ms),
            symbol: window.as_ref().map(|w| w.symbol.clone()),
            event_ticker: window.map(|w| w.event_ticker),
        })
    }

    pub fn audit_summary(&self) -> AuditSummary {
        self.audit.summary(AUDIT_RECENT)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gemini::audit::AuditRow;
    use crate::gemini::env::PRODUCTION;
    use crate::gemini::types::UpWindow;
    use std::str::FromStr;

    #[test]
    fn our_btc_series_follow_geminis_five_and_fifteen_minute_series() {
        assert_eq!(gemini_series_for("btc-5m"), Some("BTC05M"));
        assert_eq!(gemini_series_for("btc-15m"), Some("BTC15M"));
        assert_eq!(gemini_series_for("nba-h2h"), None);
        // Every BTC series in the table follows one.
        for spec in crate::series::SERIES.iter().filter(|s| s.id.is_btc()) {
            assert!(gemini_series_for(spec.slug).is_some(), "{}", spec.slug);
        }
    }

    fn state() -> (GeminiState, SeriesLink) {
        let mut state = GeminiState::new(PRODUCTION, GeminiClock::starting_at(0), RoundAudit::new());
        let link = SeriesLink::new();
        state.follow(SeriesId(0), "BTC05M", link.clone());
        (state, link)
    }

    #[test]
    fn a_series_says_whether_its_book_is_geminis_live_book() {
        let (state, link) = state();
        let book = |state: &GeminiState| serde_json::to_value(state.view(SeriesId(0)).unwrap()).unwrap()["book"].clone();
        assert_eq!(book(&state), "waiting");
        link.set_book(crate::gemini::mirror_task::BookState::Live);
        assert_eq!(book(&state), "live");
        link.set_book(crate::gemini::mirror_task::BookState::Stale);
        assert_eq!(book(&state), "stale");
    }

    #[test]
    fn a_series_shows_its_phase_and_the_window_it_follows() {
        let (state, link) = state();
        let waiting = state.view(SeriesId(0)).expect("series 0 follows Gemini");
        assert_eq!((waiting.env, waiting.series, waiting.phase), ("production", "BTC05M", Phase::WaitingForWindow));
        assert_eq!(waiting.event_ticker, None);
        let json = serde_json::to_value(&waiting).unwrap();
        assert_eq!(
            json,
            serde_json::json!({ "env": "production", "series": "BTC05M", "phase": "waiting_for_window", "book": "waiting" })
        );

        link.set(
            Phase::Open,
            Some(UpWindow {
                event_ticker: "BTC05M2611181730".into(),
                symbol: "GEMI-BTC05M2611181730-UP".into(),
                starts_at_ms: 1_795_023_900_000,
                expires_at_ms: 1_795_024_200_000,
                strike: Some(Decimal::from_str("62766.29").unwrap()),
                market_state: "open".into(),
            }),
        );
        let open = state.view(SeriesId(0)).unwrap();
        assert_eq!(open.phase, Phase::Open);
        assert_eq!(open.event_ticker.as_deref(), Some("BTC05M2611181730"));
        assert_eq!(open.symbol.as_deref(), Some("GEMI-BTC05M2611181730-UP"));
        assert_eq!(open.strike, Some(Decimal::from_str("62766.29").unwrap()));
        assert_eq!((open.starts_at_ms, open.expires_at_ms), (Some(1_795_023_900_000), Some(1_795_024_200_000)));
        assert_eq!(serde_json::to_value(&open).unwrap()["strike"], "62766.29");

        assert_eq!(state.view(SeriesId(1)), None, "a series nobody registered");
    }

    #[test]
    fn the_audit_summary_carries_the_newest_rounds() {
        let (state, _) = state();
        for i in 0..AUDIT_RECENT + 3 {
            state.audit.record(AuditRow {
                event_ticker: format!("T{i}"),
                strike: Decimal::ONE,
                resolved_up: true,
                provisional: false,
                close_price: 1.0,
                gemini_up: Some(true),
                index_at_expiry: None,
            });
        }
        let summary = state.audit_summary();
        assert_eq!(summary.rounds, AUDIT_RECENT + 3);
        assert_eq!(summary.recent.len(), AUDIT_RECENT);
        assert_eq!(summary.recent[0].event_ticker, format!("T{}", AUDIT_RECENT + 2));
    }
}
