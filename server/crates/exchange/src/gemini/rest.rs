//! Public REST reads from Gemini, behind one shared limiter.
//!
//! Gemini allows 120 public requests a minute and asks for no more than
//! one a second. Every request in this process waits on [`limiter`], which
//! spaces them at least [`MIN_SPACING`] apart and stands down for
//! [`PARK_ON_429`] after a 429.
//!
//! This module can only read public data: [`check_public_path`] refuses
//! any path that names orders, positions, balances or terms before a
//! request is made.

use std::sync::OnceLock;
use std::time::Duration;

use tokio::sync::Mutex;
use tokio::time::Instant;

use super::env::GeminiEnv;
use super::types::{EventDetail, UpWindow, event_detail_from_json, events_pagination, up_windows_from_events};

/// The least time between two public requests.
pub const MIN_SPACING: Duration = Duration::from_millis(1300);
/// How long every request waits after Gemini answers 429.
pub const PARK_ON_429: Duration = Duration::from_secs(60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const EVENTS_PAGE: u64 = 100;
const MAX_EVENT_PAGES: u64 = 5;

/// Spaces requests. One per process: see [`limiter`].
#[derive(Debug)]
pub struct RestLimiter {
    next_allowed: Mutex<Option<Instant>>,
    spacing: Duration,
    park: Duration,
}

impl RestLimiter {
    pub fn new(spacing: Duration, park: Duration) -> Self {
        Self { next_allowed: Mutex::new(None), spacing, park }
    }

    /// Wait for this request's turn.
    pub async fn acquire(&self) {
        // The lock is held across the wait on purpose: callers queue here
        // and leave one at a time, each a full spacing after the last.
        let mut next_allowed = self.next_allowed.lock().await;
        if let Some(at) = *next_allowed {
            tokio::time::sleep_until(at).await;
        }
        *next_allowed = Some(Instant::now() + self.spacing);
    }

    /// Gemini said 429: nobody goes for [`PARK_ON_429`].
    pub async fn park(&self) {
        let until = Instant::now() + self.park;
        let mut next_allowed = self.next_allowed.lock().await;
        if next_allowed.is_none_or(|at| at < until) {
            *next_allowed = Some(until);
        }
    }
}

/// The process-wide limiter every public Gemini request goes through.
pub fn limiter() -> &'static RestLimiter {
    static LIMITER: OnceLock<RestLimiter> = OnceLock::new();
    LIMITER.get_or_init(|| RestLimiter::new(MIN_SPACING, PARK_ON_429))
}

/// Refuse any path that is not public market data.
pub fn check_public_path(path_and_query: &str) -> Result<(), String> {
    let lowered = path_and_query.to_ascii_lowercase();
    for word in ["order", "position", "balance", "terms", "account"] {
        if lowered.contains(word) {
            return Err(format!(
                "refused {path_and_query}: \"{word}\" is not public market data"
            ));
        }
    }
    Ok(())
}

fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .user_agent("kyros/0.1")
            .timeout(REQUEST_TIMEOUT)
            .build()
            .expect("reqwest client")
    })
}

/// One public GET, through the limiter. Errors on any status but 200.
pub async fn get_text(env: &GeminiEnv, path_and_query: &str) -> anyhow::Result<String> {
    check_public_path(path_and_query).map_err(anyhow::Error::msg)?;
    limiter().acquire().await;
    let url = format!("{}{}", env.rest_base, path_and_query);
    let response = client().get(&url).send().await?;
    let status = response.status();
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        limiter().park().await;
        anyhow::bail!("gemini answered 429 for {path_and_query}; standing down 60 s");
    }
    if !status.is_success() {
        anyhow::bail!("gemini answered {status} for {path_and_query}");
    }
    Ok(response.text().await?)
}

/// One event by its ticker: its window and, once settled, Gemini's own
/// result. An event Gemini does not have is an error.
pub async fn get_event(env: &GeminiEnv, event_ticker: &str) -> anyhow::Result<EventDetail> {
    let body = get_text(env, &format!("/v1/prediction-markets/events/{event_ticker}")).await?;
    event_detail_from_json(&body)
}

/// The listed Up windows of one series (`"BTC05M"`, `"BTC15M"`),
/// earliest expiry first.
pub async fn list_up_windows(env: &GeminiEnv, series: &str) -> anyhow::Result<Vec<UpWindow>> {
    let mut windows = Vec::new();
    let mut offset = 0;
    for _ in 0..MAX_EVENT_PAGES {
        let path = format!(
            "/v1/prediction-markets/events?status=active&category=crypto&limit={EVENTS_PAGE}&offset={offset}"
        );
        let body = get_text(env, &path).await?;
        windows.extend(up_windows_from_events(&body, series)?);
        match events_pagination(&body) {
            Some((page_offset, limit, total)) if page_offset + limit < total && limit > 0 => {
                offset = page_offset + limit;
            }
            _ => break,
        }
    }
    windows.sort_by_key(|w| w.expires_at_ms);
    windows.dedup_by(|a, b| a.event_ticker == b.event_ticker);
    Ok(windows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn two_requests_are_never_closer_than_the_spacing() {
        let limiter = RestLimiter::new(MIN_SPACING, PARK_ON_429);
        let mut at = Vec::new();
        for _ in 0..4 {
            limiter.acquire().await;
            at.push(Instant::now());
        }
        for pair in at.windows(2) {
            assert!(pair[1] - pair[0] >= Duration::from_millis(1300), "{:?}", pair[1] - pair[0]);
        }
        // The first one does not wait at all.
        assert!(at[3] - at[0] < Duration::from_millis(1300 * 3 + 50));
    }

    #[tokio::test(start_paused = true)]
    async fn concurrent_callers_are_spaced_too() {
        let limiter = std::sync::Arc::new(RestLimiter::new(MIN_SPACING, PARK_ON_429));
        let mut handles = Vec::new();
        for _ in 0..5 {
            let limiter = limiter.clone();
            handles.push(tokio::spawn(async move {
                limiter.acquire().await;
                Instant::now()
            }));
        }
        let mut at = Vec::new();
        for h in handles {
            at.push(h.await.unwrap());
        }
        at.sort();
        for pair in at.windows(2) {
            assert!(pair[1] - pair[0] >= Duration::from_millis(1300));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn an_idle_limiter_does_not_make_the_next_caller_wait() {
        let limiter = RestLimiter::new(MIN_SPACING, PARK_ON_429);
        limiter.acquire().await;
        tokio::time::sleep(Duration::from_secs(10)).await;
        let before = Instant::now();
        limiter.acquire().await;
        assert_eq!(Instant::now(), before);
    }

    #[tokio::test(start_paused = true)]
    async fn a_429_parks_everyone_for_sixty_seconds() {
        let limiter = RestLimiter::new(MIN_SPACING, PARK_ON_429);
        limiter.acquire().await;
        let parked_at = Instant::now();
        limiter.park().await;
        limiter.acquire().await;
        assert!(Instant::now() - parked_at >= Duration::from_secs(60));
        assert!(Instant::now() - parked_at < Duration::from_secs(62));
    }

    #[test]
    fn only_public_market_data_paths_pass() {
        assert!(check_public_path("/v1/prediction-markets/events?status=active").is_ok());
        assert!(check_public_path("/v1/book/GEMI-BTC05M2611181720-UP?limit_bids=0").is_ok());
        assert!(check_public_path("/v1/prediction-markets/events/BTC05M2611181720/strike").is_ok());
        for refused in [
            "/v1/prediction-markets/order",
            "/v1/prediction-markets/order/cancel",
            "/v1/prediction-markets/positions",
            "/v1/balances",
            "/v1/prediction-markets/terms",
            "/v1/Prediction-Markets/ORDER",
        ] {
            assert!(check_public_path(refused).is_err(), "{refused} passed");
        }
    }

    #[tokio::test]
    async fn a_refused_path_fails_before_any_request() {
        // A dead local address, never Gemini: if the guard were missing,
        // this test would fail on a refused connection, not reach out.
        let env = GeminiEnv { rest_base: "http://127.0.0.1:9", ws_url: "ws://127.0.0.1:9", name: "test" };
        let err = get_text(&env, "/v1/prediction-markets/order").await.unwrap_err();
        assert!(err.to_string().contains("is not public market data"), "{err}");
    }
}
