//! Polymarket's price for the game a round trades, as a second line.
//!
//! The round's book is Gemini's. Polymarket lists the same game, and what
//! it pays for the home side is a second opinion a strategy can trade
//! against. Two public reads, no key: the game's listing once (which of
//! its two outcome tokens is the home side), then that token's midpoint
//! on the order book while the round is open.
//!
//! Fails closed. A game Polymarket does not list under the expected
//! address, a listing that is not a moneyline, is closed, or does not name
//! both teams, and a midpoint outside (0, 1) all publish nothing: the
//! signal stays unready and the strategies that read it stay hidden.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use rust_decimal::Decimal;
use serde_json::{Value as Json, json};

use crate::automation::signal_bus::{Quality, SignalBus, SignalFrame};
use crate::automation::sme::task::PlatformHandle;
use crate::gemini::nba::{GameCell, NbaGame};
use crate::registry::NBA_POLYMARKET_SIGNAL;

/// Between midpoint reads while the round is open.
pub const POLL: Duration = Duration::from_secs(3);
/// Between tries for a listing that could not be read or does not fit.
pub const LISTING_RETRY: Duration = Duration::from_secs(60);
/// With no round open; nothing is read.
const NO_GAME_LOOK: Duration = Duration::from_secs(2);

const LISTING_URL: &str = "https://gamma-api.polymarket.com/markets";
const MIDPOINT_URL: &str = "https://clob.polymarket.com/midpoint";

/// The name Polymarket's outcomes use for a team, by its three-letter code.
pub fn nickname(code: &str) -> Option<&'static str> {
    Some(match code {
        "ATL" => "Hawks",
        "BOS" => "Celtics",
        "BKN" => "Nets",
        "CHA" => "Hornets",
        "CHI" => "Bulls",
        "CLE" => "Cavaliers",
        "DAL" => "Mavericks",
        "DEN" => "Nuggets",
        "DET" => "Pistons",
        "GSW" => "Warriors",
        "HOU" => "Rockets",
        "IND" => "Pacers",
        "LAC" => "Clippers",
        "LAL" => "Lakers",
        "MEM" => "Grizzlies",
        "MIA" => "Heat",
        "MIL" => "Bucks",
        "MIN" => "Timberwolves",
        "NOP" => "Pelicans",
        "NYK" => "Knicks",
        "OKC" => "Thunder",
        "ORL" => "Magic",
        "PHI" => "76ers",
        "PHX" => "Suns",
        "POR" => "Trail Blazers",
        "SAC" => "Kings",
        "SAS" => "Spurs",
        "TOR" => "Raptors",
        "UTA" => "Jazz",
        "WAS" => "Wizards",
        _ => return None,
    })
}

/// The address Polymarket files a game under: `nba-<away>-<home>-<date>`,
/// the date being the game's in New York.
pub fn market_slug(away: &str, home: &str, tipoff_ms: u64) -> String {
    // Standard time, as the scoreboard's day is reckoned (`nba_live::scoreboard_day`).
    const NEW_YORK_BEHIND_MS: i64 = 5 * 3_600_000;
    let day = chrono::DateTime::from_timestamp_millis(tipoff_ms as i64 - NEW_YORK_BEHIND_MS).unwrap_or_default().format("%Y-%m-%d");
    format!("nba-{}-{}-{day}", away.to_ascii_lowercase(), home.to_ascii_lowercase())
}

/// Which of a listing's two outcome tokens is which side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listing {
    pub home_token: String,
    pub away_token: String,
}

/// How far a listing's stated start may be from the round's tipoff and
/// still be the same game. Venues disagree on the minute; the same two
/// teams never meet twice in an evening.
const SAME_GAME_WITHIN_MS: u64 = 6 * 3_600_000;

/// Read a listing for the game `away` at `home` that tips off at
/// `tipoff_ms`. `None` unless it is filed under that game's address and is
/// an open moneyline for that start whose two outcomes are exactly the
/// two teams.
pub fn parse_listing(body: &str, away: &str, home: &str, tipoff_ms: u64) -> Option<Listing> {
    let (away_name, home_name) = (nickname(away)?, nickname(home)?);
    let listed: Vec<Json> = serde_json::from_str(body).ok()?;
    listed.iter().find_map(|market| {
        // The address is what says which team is at home.
        if market["slug"].as_str() != Some(market_slug(away, home, tipoff_ms).as_str()) {
            return None;
        }
        let open = market["active"] == json!(true) && market["closed"] == json!(false);
        if !open || market["sportsMarketType"] != json!("moneyline") {
            return None;
        }
        let starts = listed_start_ms(market["gameStartTime"].as_str()?)?;
        if starts.abs_diff(tipoff_ms) > SAME_GAME_WITHIN_MS {
            return None;
        }
        // Both lists travel as JSON inside a string, in the same order.
        let names: Vec<String> = serde_json::from_str(market["outcomes"].as_str()?).ok()?;
        let tokens: Vec<String> = serde_json::from_str(market["clobTokenIds"].as_str()?).ok()?;
        if names.len() != 2 || tokens.len() != 2 || tokens.iter().any(|t| t.is_empty() || !t.bytes().all(|b| b.is_ascii_digit())) {
            return None;
        }
        let side = |name: &str| names.iter().position(|n| n == name);
        let (h, a) = (side(home_name)?, side(away_name)?);
        (h != a).then(|| Listing { home_token: tokens[h].clone(), away_token: tokens[a].clone() })
    })
}

/// `2026-11-19 02:30:00+00`, the one form the listing uses, as unix ms.
fn listed_start_ms(text: &str) -> Option<u64> {
    let utc = text.strip_suffix("+00")?;
    let at = chrono::NaiveDateTime::parse_from_str(utc, "%Y-%m-%d %H:%M:%S").ok()?;
    u64::try_from(at.and_utc().timestamp_millis()).ok()
}

/// Read a midpoint answer, `{"mid":"0.225"}`. `None` unless it is a
/// price strictly between 0 and 1.
pub fn parse_midpoint(body: &str) -> Option<Decimal> {
    let answer: Json = serde_json::from_str(body).ok()?;
    let mid: Decimal = answer["mid"].as_str()?.parse().ok()?;
    (mid > Decimal::ZERO && mid < Decimal::ONE).then_some(mid)
}

/// The frame for one reading of the home side's price.
pub fn line_frame(home: &str, away: &str, home_prob: Decimal, ts_ms: u64) -> SignalFrame {
    let values: BTreeMap<String, Json> = [
        ("home_team", json!(home)),
        ("away_team", json!(away)),
        ("home_prob", json!(home_prob.normalize().to_string())),
        ("away_prob", json!((Decimal::ONE - home_prob).normalize().to_string())),
    ]
    .into_iter()
    .map(|(name, value)| (name.to_owned(), value))
    .collect();
    SignalFrame { signal_id: NBA_POLYMARKET_SIGNAL.to_owned(), ts_ms, quality: Quality::ready(), values }
}

/// How long one read of the price may take before it counts as a miss.
pub const READ_TIMEOUT: Duration = Duration::from_secs(4);
/// How long the game's listing may take; it is looked up once per game, not every [`POLL`].
const LISTING_TIMEOUT: Duration = Duration::from_secs(10);

/// Consecutive readings that may fail before the line is withdrawn: thirty
/// seconds of them at [`POLL`] when they fail at once, seventy when every
/// one hangs until [`READ_TIMEOUT`]. A withdrawal clears the snapshot a
/// running strategy trades on, so one slow answer must not cause it.
pub const STALE_AFTER: u32 = 10;

/// What one reading means for the published line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Publish(Decimal),
    /// Mark the signal not ready and take its last value away from
    /// running strategies.
    Withdraw,
    Nothing,
}

/// Decides when a line is published and when it is too old to stand.
#[derive(Debug, Default)]
pub struct LineWatch {
    misses: u32,
    standing: bool,
}

impl LineWatch {
    /// One midpoint reading; `None` when it could not be read or used.
    pub fn read(&mut self, mid: Option<Decimal>) -> Step {
        match mid {
            Some(mid) => {
                self.misses = 0;
                self.standing = true;
                Step::Publish(mid)
            }
            None => {
                self.misses += 1;
                if self.standing && self.misses >= STALE_AFTER { self.end() } else { Step::Nothing }
            }
        }
    }

    /// The game the line was for is no longer the open round.
    pub fn end(&mut self) -> Step {
        self.misses = 0;
        if std::mem::take(&mut self.standing) { Step::Withdraw } else { Step::Nothing }
    }
}

/// Publish Polymarket's price for the home side of whatever game is the
/// open round, every [`POLL`], and withdraw it when it stops arriving or
/// its game is over.
pub fn spawn_polymarket_line(cell: GameCell, bus: Arc<SignalBus>, platform: PlatformHandle) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let client = reqwest::Client::new();
        let mut watch = LineWatch::default();
        // The game whose listing is known, and the game last complained about.
        let mut known: Option<(String, Listing)> = None;
        let mut complained: Option<String> = None;
        loop {
            let game = cell.open().map(|(game, _)| game);
            let same_game = matches!((&game, &known), (Some(g), Some((ticker, _))) if *ticker == g.event_ticker);
            if !same_game {
                known = None;
                apply(watch.end(), None, &bus, &platform);
            }
            let Some(game) = game else {
                tokio::time::sleep(NO_GAME_LOOK).await;
                continue;
            };
            if known.is_none() {
                let slug = market_slug(&game.away.code, &game.home.code, game.tipoff_ms);
                let found = get_text(&client, LISTING_URL, &[("slug", slug.as_str())], LISTING_TIMEOUT)
                    .await
                    .map(|body| parse_listing(&body, &game.away.code, &game.home.code, game.tipoff_ms));
                match found {
                    Ok(Some(listing)) => {
                        tracing::info!(game = %game.event_ticker, %slug, "polymarket: following its price for this game");
                        known = Some((game.event_ticker.clone(), listing));
                    }
                    other => {
                        if complained.as_deref() != Some(game.event_ticker.as_str()) {
                            complained = Some(game.event_ticker.clone());
                            tracing::warn!(game = %game.event_ticker, %slug, error = ?other.err(), "polymarket: no usable listing for this game; its line is not published");
                        }
                        tokio::time::sleep(LISTING_RETRY).await;
                        continue;
                    }
                }
            }
            if let Some((_, listing)) = &known {
                let mid = get_text(&client, MIDPOINT_URL, &[("token_id", listing.home_token.as_str())], READ_TIMEOUT).await.ok().and_then(|body| parse_midpoint(&body));
                // The read took a while; publish only if this is still the
                // round's game. A withdrawal goes through either way.
                match watch.read(mid) {
                    step @ Step::Publish(_) => {
                        cell.while_open(&game.event_ticker, || apply(step, Some(&game), &bus, &platform));
                    }
                    step => apply(step, Some(&game), &bus, &platform),
                }
            }
            tokio::time::sleep(POLL).await;
        }
    })
}

fn apply(step: Step, game: Option<&NbaGame>, bus: &SignalBus, platform: &PlatformHandle) {
    let now = crate::pricing::feed::unix_now_ms();
    match (step, game) {
        (Step::Publish(mid), Some(game)) => bus.publish(line_frame(&game.home.code, &game.away.code, mid, now)),
        (Step::Withdraw, _) => {
            tracing::warn!("polymarket: the line is withdrawn until a fresh price arrives");
            bus.set_quality(NBA_POLYMARKET_SIGNAL, now, Quality { state: "unavailable".into(), reasons: vec!["no fresh price from Polymarket".into()] });
            platform.clear_snapshot(NBA_POLYMARKET_SIGNAL);
        }
        _ => {}
    }
}

async fn get_text(client: &reqwest::Client, url: &str, query: &[(&str, &str)], timeout: Duration) -> Result<String, String> {
    let resp = client.get(url).query(query).timeout(timeout).send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("polymarket answered {}", resp.status()));
    }
    resp.text().await.map_err(|e| e.to_string())
}

#[cfg(test)]
#[path = "polymarket_tests.rs"]
mod tests;
