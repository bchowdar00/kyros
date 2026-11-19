//! The NBA game list and round driver, against a trimmed copy of a real
//! response from Gemini (2026-11-19T03:19Z: fourteen NBA games and one
//! college game) and a scripted Gemini for the driver.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;
use crate::engine::MarketStatus;
use crate::types::{MarketId, SeriesId};

const LISTED: &str = include_str!("fixtures/nba-moneyline-active.json");
/// 2026-11-19T03:10:00Z, when the fixture was read: three games in play.
const NOW: u64 = 1_795_057_800_000;
/// 2026-11-19T02:30:00Z.
const BOS_DEN_TIPOFF: u64 = 1_795_055_400_000;
const BOS_DEN: &str = "NBA-2611190230-BOS-DEN-M";
const HOUR: u64 = 3_600_000;
const MINUTE: u64 = 60_000;

fn listed() -> Vec<NbaGame> {
    nba_games_from_events(LISTED).unwrap()
}

fn game(ticker: &str) -> NbaGame {
    listed().into_iter().find(|g| g.event_ticker == ticker).unwrap_or_else(|| panic!("no {ticker}"))
}

fn dec(s: &str) -> Decimal {
    s.parse().unwrap()
}

// ───────────────────────── reading Gemini's list ─────────────────────────

#[test]
fn the_list_holds_the_nba_moneyline_games_and_nothing_else() {
    let games = listed();
    assert_eq!(games.len(), 14, "the college game is not an NBA game");
    assert!(games.iter().all(|g| g.event_ticker.starts_with("NBA-") && g.event_ticker.ends_with("-M")));
}

#[test]
fn a_game_names_both_sides_by_gemini_s_own_home_and_away() {
    let g = game(BOS_DEN);
    assert_eq!(g.tipoff_ms, BOS_DEN_TIPOFF);
    assert_eq!(g.volume, dec("10398.24"));
    assert_eq!((g.home.code.as_str(), g.home.name.as_str()), ("DEN", "Denver Nuggets"));
    assert_eq!((g.away.code.as_str(), g.away.name.as_str()), ("BOS", "Boston Celtics"));
    assert_eq!(g.home.symbol, "GEMI-NBA-2611190230-BOS-DEN-M-DEN");
    assert_eq!(g.away.symbol, "GEMI-NBA-2611190230-BOS-DEN-M-BOS");
    assert_eq!((g.home.best_bid, g.home.best_ask), (Some(dec("0.31")), Some(dec("0.34"))));
    assert_eq!(g.home.market_state, "open");
    assert_eq!(g.home_won(), None);
    assert_eq!(g.home_mid(), Some(dec("0.325")));
}

#[test]
fn a_one_sided_book_reads_as_no_quote_on_that_side() {
    let g = game("NBA-2611190100-DET-NOP-M");
    assert_eq!((g.home.code.as_str(), g.home.best_bid, g.home.best_ask), ("NOP", None, Some(dec("0.02"))));
    assert_eq!(g.home_mid(), None);
}

#[test]
fn gemini_s_result_is_read_from_either_contract() {
    let mut body: serde_json::Value = serde_json::from_str(LISTED).unwrap();
    let event = body["data"].as_array_mut().unwrap().iter_mut().find(|e| e["ticker"] == BOS_DEN).unwrap();
    let single = |event: &serde_json::Value| nba_game_from_event(&event.to_string()).unwrap();
    assert_eq!(single(event).home_won(), None);

    let contracts = event["contracts"].as_array_mut().unwrap();
    let home = contracts.iter().position(|c| c["sportsSide"] == "home").unwrap();
    contracts[home]["resolutionSide"] = "no".into();
    assert_eq!(single(event).home_won(), Some(false));
    let contracts = event["contracts"].as_array_mut().unwrap();
    contracts[home]["resolutionSide"] = serde_json::Value::Null;
    contracts[1 - home]["resolutionSide"] = "no".into();
    assert_eq!(single(event).home_won(), Some(true), "the away side lost");
}

#[test]
fn a_body_that_is_not_the_list_is_an_error_and_a_broken_event_is_skipped() {
    assert!(nba_games_from_events("not json").is_err());
    assert!(nba_game_from_event(r#"{"ticker":"NBA-2611190230-BOS-DEN-M","contracts":[]}"#).is_err());
    let mut body: serde_json::Value = serde_json::from_str(LISTED).unwrap();
    let event = body["data"].as_array_mut().unwrap().iter_mut().find(|e| e["ticker"] == BOS_DEN).unwrap();
    event["contracts"].as_array_mut().unwrap().pop();
    assert_eq!(nba_games_from_events(&body.to_string()).unwrap().len(), 13);
}

// ───────────────────────── choosing the game ─────────────────────────

#[test]
fn the_game_chosen_is_the_most_traded_one_with_a_two_sided_book() {
    let games = listed();
    let none = HashSet::new();
    // Detroit at New Orleans has traded more, but its book is one-sided.
    assert_eq!(choose_game(&games, NOW, &none).unwrap().event_ticker, BOS_DEN);
}

#[test]
fn a_game_already_resolved_here_is_not_chosen_again() {
    let games = listed();
    let done = HashSet::from([BOS_DEN.to_string()]);
    assert_eq!(choose_game(&games, NOW, &done).unwrap().event_ticker, "NBA-2611190300-UTA-LAL-M");
}

#[test]
fn a_game_long_over_or_more_than_a_day_away_is_not_chosen() {
    let games = listed();
    let none = HashSet::new();
    // Twenty hours on: the games of the 19th are over, the four at 01:00Z on the 20th are next.
    let later = NOW + 20 * HOUR;
    assert_eq!(choose_game(&games, later, &none).unwrap().event_ticker, "NBA-2611200100-POR-MIL-M");
    // Three days on, every listed game is past its span.
    assert!(choose_game(&games, NOW + 72 * HOUR, &none).is_none());
    // A game is only taken within a day of its tipoff.
    let only_far: Vec<NbaGame> = games.iter().filter(|g| g.tipoff_ms > NOW + LOOK_AHEAD.as_millis() as u64).cloned().collect();
    assert!(!only_far.is_empty());
    assert!(choose_game(&only_far, NOW, &none).is_none());
}

#[test]
fn a_wide_or_closed_book_is_not_chosen() {
    let mut g = game(BOS_DEN);
    let none = HashSet::new();
    g.home.best_ask = Some(dec("0.45"));
    assert!(choose_game(std::slice::from_ref(&g), NOW, &none).is_none(), "fourteen cents wide");
    let mut g = game(BOS_DEN);
    g.home.market_state = "closed".into();
    assert!(choose_game(std::slice::from_ref(&g), NOW, &none).is_none());
    let mut g = game(BOS_DEN);
    g.home.resolution = Some(true);
    assert!(choose_game(std::slice::from_ref(&g), NOW, &none).is_none(), "already settled on Gemini");
}

#[test]
fn a_game_that_is_all_but_decided_is_not_chosen() {
    // A finished game Gemini has not settled yet still shows an open book,
    // a cent wide, at one end. A round opened on it would be over at once.
    let none = HashSet::new();
    let mut g = game(BOS_DEN);
    (g.home.best_bid, g.home.best_ask) = (Some(dec("0.98")), Some(dec("0.99")));
    assert!(choose_game(std::slice::from_ref(&g), NOW, &none).is_none(), "home has all but won");
    (g.home.best_bid, g.home.best_ask) = (Some(dec("0.01")), Some(dec("0.02")));
    assert!(choose_game(std::slice::from_ref(&g), NOW, &none).is_none(), "home has all but lost");
    (g.home.best_bid, g.home.best_ask) = (Some(dec("0.94")), Some(dec("0.96")));
    assert!(choose_game(std::slice::from_ref(&g), NOW, &none).is_some(), "a heavy favourite is still a game");
}

#[test]
fn a_change_of_game_withdraws_what_was_published_for_the_old_one() {
    let withdrawn = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count = withdrawn.clone();
    let cell = GameCell::on_change(move || {
        count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    });
    let times = || withdrawn.load(std::sync::atomic::Ordering::SeqCst);
    cell.set(Some((game(BOS_DEN), 0.5)));
    let after_first = times();
    cell.set(Some((game(BOS_DEN), 0.4)));
    assert_eq!(times(), after_first, "the same game again is not a change");
    cell.set(Some((game("NBA-2611190300-UTA-LAL-M"), 0.5)));
    assert_eq!(times(), after_first + 1, "another game: the old one's signals go");
    cell.set(None);
    assert_eq!(times(), after_first + 2, "no game: they go too");
    cell.set(None);
    assert_eq!(times(), after_first + 2);
}

#[test]
fn nothing_is_published_for_a_game_that_is_no_longer_the_round() {
    let cell = GameCell::default();
    let mut published = 0;
    assert!(!cell.while_open(BOS_DEN, || published += 1), "no round open");
    cell.set(Some((game(BOS_DEN), 0.5)));
    assert!(cell.while_open(BOS_DEN, || published += 1));
    cell.set(Some((game("NBA-2611190300-UTA-LAL-M"), 0.5)));
    assert!(!cell.while_open(BOS_DEN, || published += 1), "a reading that came back after the round moved on");
    assert_eq!(published, 1);
}

// ───────────────────────── the round driver ─────────────────────────

/// Per ticker: `(readable from, game)`, newest last.
type Versions = HashMap<String, Vec<(u64, NbaGame)>>;

#[derive(Clone)]
struct Scripted {
    clock: GeminiClock,
    list: Arc<Mutex<Vec<NbaGame>>>,
    singles: Arc<Mutex<Versions>>,
    list_reads: Arc<AtomicUsize>,
    single_reads: Arc<AtomicUsize>,
}

impl Scripted {
    fn new(clock: &GeminiClock, list: Vec<NbaGame>) -> Self {
        Self { clock: clock.clone(), list: Arc::new(Mutex::new(list)), singles: Arc::default(), list_reads: Arc::default(), single_reads: Arc::default() }
    }

    fn put(&self, readable_from_ms: u64, game: NbaGame) {
        self.singles.lock().unwrap().entry(game.event_ticker.clone()).or_default().push((readable_from_ms, game));
    }
}

impl NbaSource for Scripted {
    async fn games(&self) -> anyhow::Result<Vec<NbaGame>> {
        self.list_reads.fetch_add(1, Ordering::SeqCst);
        Ok(self.list.lock().unwrap().clone())
    }

    async fn game(&self, event_ticker: &str) -> anyhow::Result<NbaGame> {
        self.single_reads.fetch_add(1, Ordering::SeqCst);
        let now = self.clock.now_ms();
        self.singles
            .lock()
            .unwrap()
            .get(event_ticker)
            .and_then(|versions| versions.iter().rev().find(|(from, _)| *from <= now))
            .map(|(_, game)| game.clone())
            .ok_or_else(|| anyhow::anyhow!("no event {event_ticker}"))
    }
}

struct Rig {
    driver: GeminiNbaDriver<Scripted>,
    gemini: Scripted,
    clock: GeminiClock,
    link: SeriesLink,
    cell: GameCell,
    score: Arc<Mutex<Option<Score>>>,
}

fn rig(now_ms: u64, list: Vec<NbaGame>) -> Rig {
    let clock = GeminiClock::starting_at(now_ms);
    let gemini = Scripted::new(&clock, list);
    let (link, cell) = (SeriesLink::new(), GameCell::default());
    let score: Arc<Mutex<Option<Score>>> = Arc::default();
    let reads = score.clone();
    let driver = GeminiNbaDriver::new(NbaParts {
        source: gemini.clone(),
        clock: clock.clone(),
        link: link.clone(),
        game: cell.clone(),
        score: Arc::new(move |_| *reads.lock().unwrap()),
    });
    Rig { driver, gemini, clock, link, cell, score }
}

fn market(plan: &RoundPlan) -> Market {
    Market {
        id: MarketId(1),
        series: SeriesId(2),
        reference: plan.reference,
        opened_at: plan.opens_at,
        closes_at: plan.closes_at,
        duration_secs: plan.duration_secs(),
        status: MarketStatus::Open,
    }
}

async fn within<T>(ms: u64, f: impl Future<Output = T>) -> Option<T> {
    tokio::time::timeout(Duration::from_millis(ms), f).await.ok()
}

fn resolved(ticker: &str, home_won: bool) -> NbaGame {
    let mut g = game(ticker);
    g.home.resolution = Some(home_won);
    g.away.resolution = Some(!home_won);
    g
}

#[tokio::test(start_paused = true)]
async fn the_first_round_is_the_chosen_game_and_the_book_followed_is_the_home_side() {
    let mut r = rig(NOW, listed());
    let plan = within(1_000, r.driver.first_plan()).await.expect("a game is listed");

    assert_eq!(plan.opens_at, NOW / 1000);
    assert_eq!(plan.closes_at, (BOS_DEN_TIPOFF + GAME_SPAN.as_millis() as u64) / 1000);
    match plan.reference {
        RoundReference::Nba { home_team, away_team, pregame_home_prob, game_seq } => {
            assert_eq!((home_team, away_team, game_seq), ("DEN", "BOS", 1));
            assert_eq!(pregame_home_prob, 0.5, "forty minutes in, Gemini's price is not a pregame view");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(r.link.phase(), Phase::Open);
    let window = r.link.window().unwrap();
    assert_eq!(window.symbol, "GEMI-NBA-2611190230-BOS-DEN-M-DEN");
    assert_eq!(window.event_ticker, BOS_DEN);
    assert_eq!((window.starts_at_ms, window.expires_at_ms), (BOS_DEN_TIPOFF, BOS_DEN_TIPOFF + GAME_SPAN.as_millis() as u64));
    assert_eq!(r.cell.get().unwrap().event_ticker, BOS_DEN);
}

#[tokio::test(start_paused = true)]
async fn a_round_opened_before_tipoff_takes_gemini_s_price_as_the_pregame_view() {
    let mut r = rig(BOS_DEN_TIPOFF - 30 * MINUTE, listed());
    let plan = r.driver.first_plan().await;
    match plan.reference {
        RoundReference::Nba { home_team: "DEN", pregame_home_prob, .. } => assert!((pregame_home_prob - 0.325).abs() < 1e-9, "{pregame_home_prob}"),
        other => panic!("{other:?}"),
    }
    let (game, prior) = r.cell.open().unwrap();
    assert_eq!(game.event_ticker, BOS_DEN);
    assert!((prior - 0.325).abs() < 1e-9);
    assert_eq!(plan.opens_at, (BOS_DEN_TIPOFF - 30 * MINUTE) / 1000);
}

#[tokio::test(start_paused = true)]
async fn with_no_game_listed_it_waits_and_looks_again() {
    let mut r = rig(NOW, Vec::new());
    assert!(within(IDLE_LOOK_EVERY.as_millis() as u64 - 1_000, r.driver.first_plan()).await.is_none());
    assert_eq!(r.link.phase(), Phase::WaitingForWindow);
    assert!(r.cell.get().is_none());
    *r.gemini.list.lock().unwrap() = listed();
    let plan = within(2_000, r.driver.first_plan()).await.expect("found on the next look");
    assert!(matches!(plan.reference, RoundReference::Nba { home_team: "DEN", .. }));
}

#[tokio::test(start_paused = true)]
async fn the_round_resolves_on_gemini_s_own_result_with_the_final_score() {
    // Five minutes before Gemini publishes its result. The scoreboard already
    // shows a final, whose ten-minute grace would end five minutes after it.
    let mut r = rig(BOS_DEN_TIPOFF + 145 * MINUTE, listed());
    let plan = r.driver.first_plan().await;
    let m = market(&plan);
    let over = BOS_DEN_TIPOFF + 150 * MINUTE;
    r.gemini.put(0, game(BOS_DEN));
    r.gemini.put(over, resolved(BOS_DEN, false));
    *r.score.lock().unwrap() = Some(Score { home: 101, away: 112, is_final: true, in_play: false });
    // Gemini lists nothing else afterwards.
    *r.gemini.list.lock().unwrap() = Vec::new();

    let resolution = within(4 * HOUR, r.driver.await_resolution(&m)).await.expect("resolved");

    assert_eq!(resolution.outcome, Outcome::No, "the home side lost");
    assert_eq!(resolution.close, RoundClose::Nba { home_score: 101, away_score: 112 });
    assert!(resolution.next.is_none());
    assert!(r.clock.now_ms() >= over && r.clock.now_ms() < over + 2 * RESULT_READ_EVERY.as_millis() as u64);
    assert_eq!(r.link.phase(), Phase::WaitingForWindow);
    assert!(r.cell.get().is_none());
}

#[tokio::test(start_paused = true)]
async fn gemini_is_not_asked_for_a_result_before_a_game_could_be_over() {
    // One minute before tipoff.
    let mut r = rig(BOS_DEN_TIPOFF - MINUTE, listed());
    let plan = r.driver.first_plan().await;
    let m = market(&plan);
    r.gemini.put(0, game(BOS_DEN));
    assert!(within(EARLIEST_FINAL.as_millis() as u64, r.driver.await_resolution(&m)).await.is_none());
    assert_eq!(r.gemini.single_reads.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn a_final_score_gemini_has_not_confirmed_resolves_the_round_after_the_grace() {
    let mut r = rig(BOS_DEN_TIPOFF + 140 * MINUTE, listed());
    let plan = r.driver.first_plan().await;
    let m = market(&plan);
    r.gemini.put(0, game(BOS_DEN));
    *r.score.lock().unwrap() = Some(Score { home: 120, away: 118, is_final: true, in_play: false });
    let started = r.clock.now_ms();

    let resolution = within(HOUR, r.driver.await_resolution(&m)).await.expect("resolved on the score");

    assert_eq!(resolution.outcome, Outcome::Yes);
    assert_eq!(resolution.close, RoundClose::Nba { home_score: 120, away_score: 118 });
    let took = r.clock.now_ms() - started;
    let grace = SCORE_FINAL_GRACE.as_millis() as u64;
    assert!(took >= grace && took < grace + 2 * RESULT_READ_EVERY.as_millis() as u64, "{took}");
}

#[tokio::test(start_paused = true)]
async fn a_score_that_is_not_final_or_is_level_never_resolves_a_round() {
    let mut r = rig(BOS_DEN_TIPOFF + 140 * MINUTE, listed());
    let plan = r.driver.first_plan().await;
    let m = market(&plan);
    r.gemini.put(0, game(BOS_DEN));
    *r.score.lock().unwrap() = Some(Score { home: 120, away: 118, is_final: false, in_play: true });
    assert!(within(HOUR, r.driver.await_resolution(&m)).await.is_none());
    *r.score.lock().unwrap() = Some(Score { home: 118, away: 118, is_final: true, in_play: false });
    assert!(within(HOUR, r.driver.await_resolution(&m)).await.is_none());
}

#[tokio::test(start_paused = true)]
async fn the_next_game_opens_straight_after_a_result_and_is_never_the_one_just_played() {
    let mut r = rig(BOS_DEN_TIPOFF + 150 * MINUTE, listed());
    let plan = r.driver.first_plan().await;
    let m = market(&plan);
    // Gemini still lists the finished game as active for a while.
    r.gemini.put(0, resolved(BOS_DEN, true));

    let resolution = within(HOUR, r.driver.await_resolution(&m)).await.expect("resolved");

    assert_eq!(resolution.outcome, Outcome::Yes);
    assert_eq!(resolution.close, RoundClose::Nba { home_score: 0, away_score: 0 }, "no scoreboard reading: the score is unknown");
    let next = resolution.next.expect("another game is in play");
    assert!(matches!(next.reference, RoundReference::Nba { home_team: "LAL", away_team: "UTA", game_seq: 2, .. }), "{:?}", next.reference);
    assert_eq!(r.link.window().unwrap().symbol, "GEMI-NBA-2611190300-UTA-LAL-M-LAL");
    assert_eq!(r.cell.get().unwrap().event_ticker, "NBA-2611190300-UTA-LAL-M");
}

#[tokio::test(start_paused = true)]
async fn a_game_with_no_result_at_its_stated_close_says_it_is_waiting_for_one() {
    let mut r = rig(BOS_DEN_TIPOFF + 140 * MINUTE, listed());
    let plan = r.driver.first_plan().await;
    let m = market(&plan);
    // Gemini lists the game and never says who won; no final score either.
    r.gemini.put(0, game(BOS_DEN));
    assert!(within(HOUR, r.driver.await_resolution(&m)).await.is_none());
    assert_eq!(r.link.phase(), Phase::Open, "three hours twenty after tipoff the round is still open");
    assert!(within(HOUR, r.driver.await_resolution(&m)).await.is_none());
    assert_eq!(r.link.phase(), Phase::WaitingForResult, "past the stated close it says what it is doing");
    assert!(r.cell.open().is_some(), "the game is still the round's game");
}

#[tokio::test(start_paused = true)]
async fn a_game_still_being_played_past_its_stated_close_stays_open() {
    let mut r = rig(BOS_DEN_TIPOFF + 230 * MINUTE, listed());
    let plan = r.driver.first_plan().await;
    let m = market(&plan);
    r.gemini.put(0, game(BOS_DEN));
    // Triple overtime: four hours after tipoff they are still playing.
    *r.score.lock().unwrap() = Some(Score { home: 141, away: 141, is_final: false, in_play: true });
    assert!(within(HOUR, r.driver.await_resolution(&m)).await.is_none());
    assert_eq!(r.link.phase(), Phase::Open, "its book is still followed");
    // The scoreboard stops showing it in play and no result comes.
    *r.score.lock().unwrap() = None;
    assert!(within(HOUR, r.driver.await_resolution(&m)).await.is_none());
    assert_eq!(r.link.phase(), Phase::WaitingForResult);
}
