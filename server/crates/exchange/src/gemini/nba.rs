//! One NBA game at a time, from Gemini's listed moneyline events.
//!
//! Gemini lists each game as an event, `NBA-<tipoff>-<AWAY>-<HOME>-M`,
//! with one contract per team; each contract says whether it is the home
//! or the away side. The series' book is the **home** contract's book, so
//! Yes is "home wins" and No is "away wins", as the series table says.
//!
//! A round is one game. It opens as soon as the game is chosen (so the
//! market trades before tipoff), states its close as tipoff plus
//! [`GAME_SPAN`], and resolves on Gemini's own result for the contract.
//! If the public scoreboard has shown a final score for
//! [`SCORE_FINAL_GRACE`] and Gemini has still said nothing, the round
//! resolves on that score and says so in the log.

use std::collections::HashSet;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use serde::Deserialize;

use super::driver::{Phase, SeriesLink};
use super::env::GeminiEnv;
use super::strikes::GeminiClock;
use super::types::UpWindow;
use crate::engine::{Market, Outcome, RoundClose, RoundReference};
use crate::scheduler::{RoundDriver, RoundPlan, RoundResolution};

/// A round states its close as this long after tipoff. No NBA game the
/// scoreboard has shown ran longer, overtime included.
pub const GAME_SPAN: Duration = Duration::from_secs(4 * 3600);
/// A game is taken up to this long before its tipoff.
pub const LOOK_AHEAD: Duration = Duration::from_secs(24 * 3600);
/// How often Gemini's list is read while no game is open.
pub const IDLE_LOOK_EVERY: Duration = Duration::from_secs(300);
/// No result is asked for sooner than this after tipoff.
pub const EARLIEST_FINAL: Duration = Duration::from_secs(100 * 60);
/// How often Gemini is asked for the result once a game could be over.
pub const RESULT_READ_EVERY: Duration = Duration::from_secs(30);
/// How long a final score waits for Gemini's result before it decides.
pub const SCORE_FINAL_GRACE: Duration = Duration::from_secs(600);

/// One team's contract in a game.
#[derive(Debug, Clone, PartialEq)]
pub struct NbaSide {
    /// Gemini's three-letter code, e.g. `DEN`.
    pub code: String,
    /// The team's full name, e.g. `Denver Nuggets`.
    pub name: String,
    /// The contract's instrument symbol.
    pub symbol: String,
    /// Gemini's own word: `open`, `closed`, …
    pub market_state: String,
    pub best_bid: Option<Decimal>,
    pub best_ask: Option<Decimal>,
    /// `Some(true)` once Gemini has resolved this contract Yes (the team
    /// won), `Some(false)` for No.
    pub resolution: Option<bool>,
}

/// One listed NBA game.
#[derive(Debug, Clone, PartialEq)]
pub struct NbaGame {
    pub event_ticker: String,
    pub home: NbaSide,
    pub away: NbaSide,
    pub tipoff_ms: u64,
    /// What Gemini has traded on the event; its own figure, unit unstated.
    pub volume: Decimal,
}

impl NbaGame {
    /// Gemini's result, from whichever contract carries one.
    pub fn home_won(&self) -> Option<bool> {
        self.home.resolution.or(self.away.resolution.map(|away_won| !away_won))
    }

    /// The middle of the home contract's best bid and ask.
    pub fn home_mid(&self) -> Option<Decimal> {
        Some((self.home.best_bid? + self.home.best_ask?) / Decimal::TWO)
    }
}

/// The NBA moneyline games in a page of `GET /v1/prediction-markets/events`.
/// An event that cannot be read is skipped; the next look sees it again.
pub fn nba_games_from_events(body: &str) -> anyhow::Result<Vec<NbaGame>> {
    #[derive(Deserialize)]
    struct Page {
        data: Vec<serde_json::Value>,
    }
    let page: Page = serde_json::from_str(body)?;
    let mut games = Vec::new();
    for event in page.data {
        if !event.get("ticker").and_then(|t| t.as_str()).is_some_and(is_nba_moneyline) {
            continue;
        }
        match serde_json::from_value::<RawEvent>(event).map_err(anyhow::Error::from).and_then(game_from_raw) {
            Ok(game) => games.push(game),
            Err(e) => tracing::debug!(error = %e, "an NBA event in gemini's list could not be read; skipped"),
        }
    }
    Ok(games)
}

/// One game from `GET /v1/prediction-markets/events/{ticker}`.
pub fn nba_game_from_event(body: &str) -> anyhow::Result<NbaGame> {
    game_from_raw(serde_json::from_str::<RawEvent>(body)?)
}

fn is_nba_moneyline(ticker: &str) -> bool {
    ticker.starts_with("NBA-") && ticker.ends_with("-M")
}

#[derive(Deserialize)]
struct RawEvent {
    ticker: String,
    #[serde(rename = "startTime")]
    start_time: String,
    #[serde(default)]
    volume: Option<serde_json::Value>,
    #[serde(default)]
    contracts: Vec<RawContract>,
}

#[derive(Deserialize)]
struct RawContract {
    ticker: String,
    #[serde(rename = "instrumentSymbol")]
    symbol: String,
    #[serde(rename = "sportsSide", default)]
    side: Option<String>,
    #[serde(rename = "marketState", default)]
    market_state: Option<String>,
    #[serde(default)]
    prices: Option<RawPrices>,
    #[serde(rename = "resolutionSide", default)]
    resolution: Option<String>,
    #[serde(default)]
    target: Option<RawTarget>,
    #[serde(default)]
    label: Option<String>,
}

#[derive(Deserialize)]
struct RawPrices {
    #[serde(rename = "bestBid", default)]
    best_bid: Option<String>,
    #[serde(rename = "bestAsk", default)]
    best_ask: Option<String>,
}

#[derive(Deserialize)]
struct RawTarget {
    #[serde(default)]
    name: Option<String>,
}

fn game_from_raw(raw: RawEvent) -> anyhow::Result<NbaGame> {
    anyhow::ensure!(is_nba_moneyline(&raw.ticker), "{} is not an NBA moneyline event", raw.ticker);
    let tipoff_ms = chrono::DateTime::parse_from_rfc3339(&raw.start_time)
        .ok()
        .and_then(|at| u64::try_from(at.timestamp_millis()).ok())
        .ok_or_else(|| anyhow::anyhow!("{}: start time {:?} is not a time", raw.ticker, raw.start_time))?;
    let volume = match &raw.volume {
        Some(serde_json::Value::String(text)) => text.parse().unwrap_or(Decimal::ZERO),
        Some(serde_json::Value::Number(n)) => n.to_string().parse().unwrap_or(Decimal::ZERO),
        _ => Decimal::ZERO,
    };
    let (mut home, mut away) = (None, None);
    for contract in raw.contracts {
        let slot = match contract.side.as_deref() {
            Some("home") => &mut home,
            Some("away") => &mut away,
            _ => continue,
        };
        let price = |text: Option<String>| text.filter(|t| !t.is_empty()).and_then(|t| t.parse::<Decimal>().ok());
        let (best_bid, best_ask) = match contract.prices {
            Some(prices) => (price(prices.best_bid), price(prices.best_ask)),
            None => (None, None),
        };
        *slot = Some(NbaSide {
            name: contract.target.and_then(|t| t.name).or(contract.label).unwrap_or_else(|| contract.ticker.clone()),
            code: contract.ticker,
            symbol: contract.symbol,
            market_state: contract.market_state.unwrap_or_default(),
            best_bid,
            best_ask,
            resolution: match contract.resolution.as_deref() {
                Some("yes") => Some(true),
                Some("no") => Some(false),
                _ => None,
            },
        });
    }
    match (home, away) {
        (Some(home), Some(away)) => Ok(NbaGame { event_ticker: raw.ticker, home, away, tipoff_ms, volume }),
        _ => anyhow::bail!("{} does not list a home and an away contract", raw.ticker),
    }
}

/// The game a round should be about at `now_ms`: of the games not in
/// `done`, not yet resolved, within [`LOOK_AHEAD`] of tipoff and not past
/// their [`GAME_SPAN`], whose home contract is open with a bid and an ask
/// no more than ten cents apart and not at either end of the scale (a
/// game that is over but not settled yet), the most traded; then the
/// earliest.
pub fn choose_game<'a>(games: &'a [NbaGame], now_ms: u64, done: &HashSet<String>) -> Option<&'a NbaGame> {
    let (span, ahead) = (GAME_SPAN.as_millis() as u64, LOOK_AHEAD.as_millis() as u64);
    games
        .iter()
        .filter(|g| !done.contains(&g.event_ticker) && g.home_won().is_none())
        .filter(|g| now_ms <= g.tipoff_ms + span && g.tipoff_ms <= now_ms + ahead)
        .filter(|g| g.home.market_state == "open")
        .filter(|g| matches!((g.home.best_bid, g.home.best_ask), (Some(bid), Some(ask)) if ask - bid <= MAX_SPREAD))
        .filter(|g| g.home_mid().is_some_and(|mid| mid > DECIDED_BELOW && mid < Decimal::ONE - DECIDED_BELOW))
        .min_by(|a, b| b.volume.cmp(&a.volume).then(a.tipoff_ms.cmp(&b.tipoff_ms)).then_with(|| a.event_ticker.cmp(&b.event_ticker)))
}

/// A home price this close to either end means the game is decided:
/// three cents.
const DECIDED_BELOW: Decimal = Decimal::from_parts(3, 0, 0, false, 2);
/// The widest home book a game is taken with: ten cents.
const MAX_SPREAD: Decimal = Decimal::from_parts(10, 0, 0, false, 2);
/// The path of Gemini's list of active NBA moneyline events, one page.
const LIST_PATH: &str = "/v1/prediction-markets/events?status=active&category=sports&sport=basketball&sports_market_type=moneyline&limit=100";
/// No more pages than this are read in one look: 500 events.
const MAX_PAGES: usize = 5;

/// Where the driver reads games. The real one is Gemini's REST API.
pub trait NbaSource: Send + Sync + 'static {
    fn games(&self) -> impl Future<Output = anyhow::Result<Vec<NbaGame>>> + Send;
    fn game(&self, event_ticker: &str) -> impl Future<Output = anyhow::Result<NbaGame>> + Send;
}

/// Gemini's public REST API as an [`NbaSource`].
#[derive(Debug, Clone, Copy)]
pub struct RestNbaSource(pub GeminiEnv);

impl NbaSource for RestNbaSource {
    async fn games(&self) -> anyhow::Result<Vec<NbaGame>> {
        // The list is every basketball moneyline, college included, a
        // hundred to a page; the NBA's games are spread through it.
        let mut games = Vec::new();
        for page in 0..MAX_PAGES {
            let body = super::rest::get_text(&self.0, &format!("{LIST_PATH}&offset={}", page * 100)).await?;
            games.extend(nba_games_from_events(&body)?);
            if !has_more(&body, (page + 1) * 100) {
                break;
            }
        }
        Ok(games)
    }

    async fn game(&self, event_ticker: &str) -> anyhow::Result<NbaGame> {
        anyhow::ensure!(is_nba_moneyline(event_ticker) && event_ticker.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'), "not an NBA event ticker: {event_ticker:?}");
        let body = super::rest::get_text(&self.0, &format!("/v1/prediction-markets/events/{event_ticker}")).await?;
        nba_game_from_event(&body)
    }
}

/// Whether Gemini's list has events past the first `read`.
fn has_more(body: &str, read: usize) -> bool {
    #[derive(Deserialize)]
    struct Page {
        pagination: Option<Pagination>,
    }
    #[derive(Deserialize)]
    struct Pagination {
        total: Option<usize>,
    }
    serde_json::from_str::<Page>(body).ok().and_then(|p| p.pagination).and_then(|p| p.total).is_some_and(|total| total > read)
}

/// A team code that lives as long as the process. The engine's round
/// reference holds `&'static str`; the league has thirty teams.
fn intern(code: &str) -> &'static str {
    static CODES: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
    let mut codes = CODES.lock().expect("team codes poisoned");
    if let Some(known) = codes.iter().find(|known| **known == code) {
        return known;
    }
    let leaked: &'static str = Box::leak(code.to_owned().into_boxed_str());
    codes.push(leaked);
    leaked
}

/// The score of a game as the public scoreboard last showed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Score {
    pub home: u32,
    pub away: u32,
    pub is_final: bool,
    /// The game has tipped off and is not over.
    pub in_play: bool,
}

/// The scoreboard's latest reading of a game, if it has one.
pub type ScoreFn = Arc<dyn Fn(&NbaGame) -> Option<Score> + Send + Sync>;

/// The game the open round is about, for the signal publishers and the
/// API. It is also the one gate between games: what was published for a
/// game is withdrawn the moment the cell moves to another, and a
/// publisher publishes only through [`GameCell::while_open`], so a
/// reading that comes back late can never land in the next game's round.
#[derive(Clone, Default)]
pub struct GameCell {
    open: Arc<Mutex<Option<(NbaGame, f64)>>>,
    withdraw: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl std::fmt::Debug for GameCell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GameCell").field("open", &self.get().map(|game| game.event_ticker)).finish()
    }
}

impl GameCell {
    pub fn get(&self) -> Option<NbaGame> {
        self.open().map(|(game, _)| game)
    }

    /// The open game and the round's pregame view of its home side.
    pub fn open(&self) -> Option<(NbaGame, f64)> {
        self.open.lock().expect("game cell poisoned").clone()
    }

    fn set(&self, open: Option<(NbaGame, f64)>) {
        let ticker = |held: &Option<(NbaGame, f64)>| held.as_ref().map(|(game, _)| game.event_ticker.clone());
        let mut held = self.open.lock().expect("game cell poisoned");
        let changed = ticker(&held) != ticker(&open);
        *held = open;
        // Under the lock, so no publisher can slip a frame in between.
        if changed && let Some(withdraw) = &self.withdraw {
            withdraw();
        }
    }

    /// A cell that runs `withdraw` whenever the open game changes.
    pub fn on_change(withdraw: impl Fn() + Send + Sync + 'static) -> GameCell {
        GameCell { open: Arc::default(), withdraw: Some(Arc::new(withdraw)) }
    }

    /// Run `publish` if `event_ticker` is still the open game, holding the
    /// cell so the game cannot change under it. Returns whether it ran.
    pub fn while_open(&self, event_ticker: &str, publish: impl FnOnce()) -> bool {
        let held = self.open.lock().expect("game cell poisoned");
        let still = held.as_ref().is_some_and(|(game, _)| game.event_ticker == event_ticker);
        if still {
            publish();
        }
        still
    }
}

/// Everything a [`GeminiNbaDriver`] is built from.
pub struct NbaParts<S> {
    pub source: S,
    pub clock: GeminiClock,
    pub link: SeriesLink,
    pub game: GameCell,
    pub score: ScoreFn,
}

pub struct GeminiNbaDriver<S> {
    parts: NbaParts<S>,
    open: Option<NbaGame>,
    /// What the series link says about the open round.
    window: Option<UpWindow>,
    /// Games this driver has resolved; never opened twice.
    done: HashSet<String>,
    seq: u64,
}

impl<S: NbaSource> GeminiNbaDriver<S> {
    pub fn new(parts: NbaParts<S>) -> Self {
        Self { parts, open: None, window: None, done: HashSet::new(), seq: 0 }
    }
}

impl<S: NbaSource> GeminiNbaDriver<S> {
    /// One look at Gemini's list for the game to open next.
    async fn look(&self) -> Option<NbaGame> {
        match self.parts.source.games().await {
            Ok(games) => choose_game(&games, self.parts.clock.now_ms(), &self.done).cloned(),
            Err(e) => {
                tracing::warn!(error = %e, "could not read gemini's NBA games");
                None
            }
        }
    }

    /// Open a round on `game`: the series follows its home contract.
    fn plan(&mut self, game: NbaGame) -> RoundPlan {
        let now = self.parts.clock.now_ms();
        // A game taken up in its last minutes still gets a minute on the clock.
        let closes_ms = (game.tipoff_ms + GAME_SPAN.as_millis() as u64).max(now + 60_000);
        self.seq += 1;
        // The pregame view is Gemini's own price for the home side, and
        // only if the round opens before tipoff. A price read once the
        // game is under way already contains the score, so a round that
        // opens late has no pregame view: even.
        let pregame_home_prob = if now < game.tipoff_ms { game.home_mid().and_then(|mid| mid.to_f64()).unwrap_or(0.5) } else { 0.5 };
        let reference = RoundReference::Nba { home_team: intern(&game.home.code), away_team: intern(&game.away.code), pregame_home_prob, game_seq: self.seq };
        let window = UpWindow {
            event_ticker: game.event_ticker.clone(),
            symbol: game.home.symbol.clone(),
            starts_at_ms: game.tipoff_ms,
            expires_at_ms: closes_ms,
            strike: None,
            market_state: game.home.market_state.clone(),
        };
        self.parts.link.set(Phase::Open, Some(window.clone()));
        self.window = Some(window);
        tracing::info!(game = %game.event_ticker, home = %game.home.code, away = %game.away.code, "nba: round opens on this game");
        self.parts.game.set(Some((game.clone(), pregame_home_prob)));
        self.open = Some(game);
        RoundPlan { reference, opens_at: now / 1000, closes_at: closes_ms / 1000 }
    }

    fn idle(&mut self) {
        self.open = None;
        self.window = None;
        self.parts.link.set(Phase::WaitingForWindow, None);
        self.parts.game.set(None);
    }
}

impl<S: NbaSource> RoundDriver for GeminiNbaDriver<S> {
    async fn first_plan(&mut self) -> RoundPlan {
        loop {
            if let Some(game) = self.look().await {
                return self.plan(game);
            }
            self.idle();
            let now = self.parts.clock.now_ms();
            self.parts.clock.sleep_until_ms(now + IDLE_LOOK_EVERY.as_millis() as u64).await;
        }
    }

    async fn await_resolution(&mut self, _current: &Market) -> RoundResolution {
        let Some(game) = self.open.clone() else {
            // The scheduler only asks about a round this driver opened.
            tracing::error!("nba: asked to resolve a round with no game open");
            return std::future::pending().await;
        };
        let clock = self.parts.clock.clone();
        clock.sleep_until_ms(game.tipoff_ms + EARLIEST_FINAL.as_millis() as u64).await;
        let mut final_since: Option<u64> = None;
        let home_won = loop {
            match self.parts.source.game(&game.event_ticker).await {
                Ok(latest) => {
                    if let Some(home_won) = latest.home_won() {
                        break home_won;
                    }
                }
                Err(e) => tracing::warn!(game = %game.event_ticker, error = %e, "could not read gemini's result for the game"),
            }
            let now = clock.now_ms();
            // Past the stated close with no result: the round is not open
            // any more, it is waiting. A postponed game waits here for
            // good; `/health` says so once it is hours late.
            // A game still being played (overtimes, a delay) keeps its
            // book: only a game the scoreboard no longer shows in play.
            if let Some(window) = &self.window
                && now >= window.expires_at_ms
                && self.parts.link.phase() == Phase::Open
                && !(self.parts.score)(&game).is_some_and(|score| score.in_play)
            {
                tracing::warn!(game = %game.event_ticker, "nba: the stated close has passed with no result; waiting for one");
                self.parts.link.set(Phase::WaitingForResult, Some(window.clone()));
            }
            match (self.parts.score)(&game) {
                Some(score) if score.is_final && score.home != score.away => {
                    let since = *final_since.get_or_insert(now);
                    // The clock is corrected against Gemini's and may step back.
                    if now.saturating_sub(since) >= SCORE_FINAL_GRACE.as_millis() as u64 {
                        tracing::warn!(game = %game.event_ticker, home = score.home, away = score.away, "nba: gemini has published no result; resolving on the scoreboard's final score");
                        break score.home > score.away;
                    }
                }
                _ => final_since = None,
            }
            clock.sleep_until_ms(now + RESULT_READ_EVERY.as_millis() as u64).await;
        };
        let close = match (self.parts.score)(&game) {
            Some(score) => RoundClose::Nba { home_score: score.home, away_score: score.away },
            None => {
                tracing::warn!(game = %game.event_ticker, "nba: no scoreboard reading for the game; its score is recorded as 0-0");
                RoundClose::Nba { home_score: 0, away_score: 0 }
            }
        };
        tracing::info!(game = %game.event_ticker, home_won, "nba: round resolved");
        self.done.insert(game.event_ticker);
        self.open = None;
        let next = match self.look().await {
            Some(next) => Some(self.plan(next)),
            None => {
                self.idle();
                None
            }
        };
        RoundResolution { outcome: if home_won { Outcome::Yes } else { Outcome::No }, close, next }
    }
}

#[cfg(test)]
#[path = "nba_tests.rs"]
mod tests;
