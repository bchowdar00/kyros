//! Kalshi's price for the game a round trades, as a third line.
//!
//! The round's book is Gemini's. Kalshi lists the same game as a pair of
//! event-contract markets, one per team, and what it pays for the home
//! side is a second opinion a strategy can trade against. One public read,
//! no key: the game's markets carry both identity (which ticker is the
//! home side) and price (the home market's bid and ask) at once, so there
//! is no separate listing step.
//!
//! Fails closed. A game Kalshi does not list under the expected event
//! ticker, a home market that is not an active binary, one whose stated
//! expiration is not this game's, and a bid/ask outside (0, 1) or the
//! wrong way round all publish nothing: the signal stays unready and the
//! strategies that read it stay hidden.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use rust_decimal::Decimal;
use serde_json::{Value as Json, json};

use crate::automation::signal_bus::{Quality, SignalBus, SignalFrame};
use crate::automation::sme::task::PlatformHandle;
use crate::gemini::nba::{GameCell, NbaGame};
use crate::registry::NBA_KALSHI_SIGNAL;

// `LineWatch`, `Step` and the staleness count are the Polymarket line's;
// one reading means the same thing for either venue.
use super::polymarket::{LineWatch, READ_TIMEOUT, Step};

/// Between reads while the round is open.
pub const POLL: Duration = Duration::from_secs(3);
/// Between tries for a game Kalshi does not list in a usable form.
pub const LISTING_RETRY: Duration = Duration::from_secs(60);
/// With no round open; nothing is read.
const NO_GAME_LOOK: Duration = Duration::from_secs(2);

const MARKETS_URL: &str = "https://api.elections.kalshi.com/trade-api/v2/markets";

/// How far a home market's stated expiration may be from the round's
/// tipoff and still be the same game: no earlier than tipoff, no later
/// than six hours after. The same two teams never meet twice in an
/// evening.
const SAME_GAME_WITHIN_MS: u64 = 6 * 3_600_000;

/// The event ticker Kalshi files a game under:
/// `KXNBAGAME-<YY><MON><DD><AWAY><HOME>`, the date being the game's in New
/// York (the same standard-time rule as [`super::polymarket::market_slug`]),
/// month as three upper-case letters.
pub fn event_ticker(away: &str, home: &str, tipoff_ms: u64) -> String {
    // Standard time, as the scoreboard's day is reckoned (`nba_live::scoreboard_day`).
    const NEW_YORK_BEHIND_MS: i64 = 5 * 3_600_000;
    let date = chrono::DateTime::from_timestamp_millis(tipoff_ms as i64 - NEW_YORK_BEHIND_MS)
        .unwrap_or_default()
        .format("%y%b%d")
        .to_string()
        .to_uppercase();
    format!("KXNBAGAME-{date}{}{}", away.to_ascii_uppercase(), home.to_ascii_uppercase())
}

/// The midpoint of Kalshi's bid and ask for the home team winning, or
/// `None` when the read is not a usable market for this game.
pub fn parse_home_mid(body: &str, away: &str, home: &str, tipoff_ms: u64) -> Option<Decimal> {
    let ticker = event_ticker(away, home, tipoff_ms);
    let doc: Json = serde_json::from_str(body).ok()?;
    let markets = doc["markets"].as_array()?;
    // A market of this event filed under `<event_ticker>-<suffix>`.
    let market = |suffix: &str| {
        let want = format!("{ticker}-{suffix}");
        markets.iter().find(|m| m["ticker"].as_str() == Some(want.as_str()) && m["event_ticker"].as_str() == Some(ticker.as_str()))
    };
    let home_market = market(&home.to_ascii_uppercase())?;
    // Both teams must be listed, so a one-sided event is not a game.
    market(&away.to_ascii_uppercase())?;
    if home_market["status"].as_str() != Some("active") || home_market["market_type"].as_str() != Some("binary") {
        return None;
    }
    // The stated expiration says this is the same meeting, not another
    // between the two teams filed under the same ticker shape.
    let expires = expiration_ms(home_market["expected_expiration_time"].as_str()?)?;
    if expires < tipoff_ms || expires > tipoff_ms + SAME_GAME_WITHIN_MS {
        return None;
    }
    let bid: Decimal = home_market["yes_bid_dollars"].as_str()?.parse().ok()?;
    let ask: Decimal = home_market["yes_ask_dollars"].as_str()?.parse().ok()?;
    (Decimal::ZERO < bid && bid < ask && ask < Decimal::ONE).then(|| (bid + ask) / Decimal::TWO)
}

/// `2026-11-21T03:30:00Z`, the RFC 3339 form Kalshi uses, as unix ms.
fn expiration_ms(text: &str) -> Option<u64> {
    let at = chrono::DateTime::parse_from_rfc3339(text).ok()?;
    u64::try_from(at.timestamp_millis()).ok()
}

/// The frame for one reading of the home side's price. Same four fields as
/// the Polymarket line.
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
    SignalFrame { signal_id: NBA_KALSHI_SIGNAL.to_owned(), ts_ms, quality: Quality::ready(), values }
}

/// What one read of Kalshi came to.
#[derive(Debug, Clone, PartialEq)]
pub enum Read {
    /// The home side's midpoint.
    Price(Decimal),
    /// Kalshi answered, and has no usable market for this game.
    Unlisted,
    /// No answer: a timeout, a refusal, a server error.
    Failed,
}

/// How long to wait before the next read. Only a game Kalshi does not list,
/// with no line standing, waits [`LISTING_RETRY`]. A read that did not
/// arrive says nothing about the listing and is tried again at [`POLL`],
/// and a standing line is always read at [`POLL`] so that it ages out
/// through [`LineWatch`] and not through one long sleep.
pub fn wait_after(read: &Read, standing: bool) -> Duration {
    if *read == Read::Unlisted && !standing { LISTING_RETRY } else { POLL }
}

/// Publish Kalshi's price for the home side of whatever game is the open
/// round, every [`POLL`], and withdraw it when it stops arriving or its
/// game is over. One read gives both identity and price, so a game Kalshi
/// does not yet list usably is complained about once and then retried
/// after [`LISTING_RETRY`] until a line stands ([`wait_after`]); from then
/// on a missing read ages the line out through [`LineWatch`] at the normal
/// [`POLL`].
pub fn spawn_kalshi_line(cell: GameCell, bus: Arc<SignalBus>, platform: PlatformHandle) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let client = reqwest::Client::new();
        let mut watch = LineWatch::default();
        // The game a line currently stands for, the game whose first price
        // was announced, and the game last complained about.
        let mut standing: Option<String> = None;
        let mut announced: Option<String> = None;
        let mut complained: Option<String> = None;
        // Whether the last read failed to arrive, so a run of them is said once.
        let mut failing = false;
        loop {
            let game = cell.open().map(|(game, _)| game);
            // A standing line whose game is no longer the open round is
            // withdrawn; the read below never publishes for another game.
            let same_game = matches!((&game, &standing), (Some(g), Some(ticker)) if *ticker == g.event_ticker);
            if !same_game && standing.is_some() {
                apply(watch.end(), None, &bus, &platform);
                standing = None;
            }
            let Some(game) = game else {
                tokio::time::sleep(NO_GAME_LOOK).await;
                continue;
            };
            let ticker = event_ticker(&game.away.code, &game.home.code, game.tipoff_ms);
            let read = match get_text(&client, MARKETS_URL, &[("event_ticker", ticker.as_str())]).await {
                Ok(body) => parse_home_mid(&body, &game.away.code, &game.home.code, game.tipoff_ms).map_or(Read::Unlisted, Read::Price),
                Err(why) => {
                    if !failing {
                        tracing::warn!(game = %game.event_ticker, %ticker, %why, "kalshi: the read did not arrive; trying again");
                    }
                    Read::Failed
                }
            };
            failing = read == Read::Failed;
            // Kalshi answered, has no usable market for this game and no
            // line stands: say so once and wait before trying again, so a
            // game Kalshi never lists is not polled every POLL.
            let wait = wait_after(&read, standing.as_deref() == Some(game.event_ticker.as_str()));
            if wait == LISTING_RETRY {
                if complained.as_deref() != Some(game.event_ticker.as_str()) {
                    complained = Some(game.event_ticker.clone());
                    tracing::warn!(game = %game.event_ticker, %ticker, "kalshi: no usable market for this game; its line is not published");
                }
                tokio::time::sleep(wait).await;
                continue;
            }
            let mid = match read {
                Read::Price(mid) => Some(mid),
                Read::Unlisted | Read::Failed => None,
            };
            // The read took a while; publish only if this is still the
            // round's game. A withdrawal goes through either way.
            match watch.read(mid) {
                step @ Step::Publish(_) => {
                    if announced.as_deref() != Some(game.event_ticker.as_str()) {
                        announced = Some(game.event_ticker.clone());
                        tracing::info!(game = %game.event_ticker, %ticker, "kalshi: following its price for this game");
                    }
                    standing = Some(game.event_ticker.clone());
                    cell.while_open(&game.event_ticker, || apply(step, Some(&game), &bus, &platform));
                }
                step => {
                    if step == Step::Withdraw {
                        standing = None;
                    }
                    apply(step, Some(&game), &bus, &platform);
                }
            }
            tokio::time::sleep(wait).await;
        }
    })
}

fn apply(step: Step, game: Option<&NbaGame>, bus: &SignalBus, platform: &PlatformHandle) {
    let now = crate::pricing::feed::unix_now_ms();
    match (step, game) {
        (Step::Publish(mid), Some(game)) => bus.publish(line_frame(&game.home.code, &game.away.code, mid, now)),
        (Step::Withdraw, _) => {
            tracing::warn!("kalshi: the line is withdrawn until a fresh price arrives");
            bus.set_quality(NBA_KALSHI_SIGNAL, now, Quality { state: "unavailable".into(), reasons: vec!["no fresh price from Kalshi".into()] });
            platform.clear_snapshot(NBA_KALSHI_SIGNAL);
        }
        _ => {}
    }
}

async fn get_text(client: &reqwest::Client, url: &str, query: &[(&str, &str)]) -> Result<String, String> {
    let resp = client.get(url).query(query).timeout(READ_TIMEOUT).send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("kalshi answered {}", resp.status()));
    }
    resp.text().await.map_err(|e| e.to_string())
}

#[cfg(test)]
#[path = "kalshi_tests.rs"]
mod tests;
