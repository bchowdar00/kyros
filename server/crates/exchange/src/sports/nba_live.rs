//! The live NBA game as signals: play-by-play, score margin and the
//! model's win probability, from the public scoreboard.
//!
//! The round driver ([`crate::gemini::nba`]) chooses the game; this module
//! follows that game on the scoreboard. Scoring plays come from the
//! change in score between two readings, never from the play text, so a
//! reading that skips a play still accounts for every point.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rust_decimal::prelude::ToPrimitive;
use serde_json::{Value as Json, json};

use crate::automation::signal_bus::{Quality, SignalBus, SignalFrame};
use crate::automation::sme::task::PlatformHandle;
use crate::gemini::nba::{GameCell, Score, ScoreFn};
use crate::registry::{NBA_MARGIN_SIGNAL, NBA_PBP_SIGNAL, NBA_POLYMARKET_SIGNAL, NBA_WINPROB_SIGNAL};
use crate::sports::espn::{self, GamePhase, GameState, Play};
use crate::sports::winprob;

/// How often the scoreboard is read while the game is in play.
pub const POLL_IN_PLAY: Duration = Duration::from_secs(5);
/// How often it is read before tipoff and after the final.
pub const POLL_IDLE: Duration = Duration::from_secs(30);
/// How often the publisher checks whether a round has opened on a game.
const NO_GAME_LOOK: Duration = Duration::from_secs(2);
/// How many readings in a row must agree on a step backwards (a lower
/// score, an earlier quarter, a game no longer final) before it is
/// believed. The scoreboard is served from caches of different ages, so
/// one old copy between two current ones is common; an official's
/// correction is in every copy from then on.
pub const BACKSTEP_CONFIRM: u32 = 3;

/// The scoreboard's latest reading of each game, by Gemini's event ticker.
#[derive(Debug, Clone, Default)]
pub struct ScoreBoard(Arc<Mutex<HashMap<String, Score>>>);

impl ScoreBoard {
    pub fn put(&self, event_ticker: &str, state: &GameState) {
        let score = Score { home: state.home_score, away: state.away_score, is_final: state.phase == GamePhase::Final, in_play: state.phase == GamePhase::InPlay };
        self.0.lock().expect("score board poisoned").insert(event_ticker.to_owned(), score);
    }

    /// The round driver's view of the board.
    pub fn score_fn(&self) -> ScoreFn {
        let board = self.0.clone();
        Arc::new(move |game| board.lock().expect("score board poisoned").get(&game.event_ticker).copied())
    }
}

/// The day to ask the scoreboard for, `YYYYMMDD`: the scoreboard files a
/// game under its date in New York. The NBA season runs on standard time
/// but for its first days and its playoffs, and no game tips off in the
/// hour where the difference would change the date.
pub fn scoreboard_day(tipoff_ms: u64) -> String {
    const NEW_YORK_BEHIND_MS: i64 = 5 * 3_600_000;
    chrono::DateTime::from_timestamp_millis(tipoff_ms as i64 - NEW_YORK_BEHIND_MS).unwrap_or_default().format("%Y%m%d").to_string()
}

/// Turns successive readings of one game into signal frames.
#[derive(Debug, Default)]
pub struct Tracker {
    event_ticker: Option<String>,
    prev: Option<GameState>,
    seq: u32,
    /// The model's last published value, as published.
    model: Option<String>,
    /// The margin last published.
    margin: Option<i64>,
    /// A step backwards seen but not yet believed, and how many readings
    /// in a row have shown it.
    doubted: Option<((u8, u8, u32), u32)>,
    /// The newest play seen, by the scoreboard's own numbering.
    latest_play: Option<u64>,
}

/// Where a game stands, ordered so that a later reading is never smaller:
/// phase, quarter, points scored.
fn progress(state: &GameState) -> (u8, u8, u32) {
    let phase = match state.phase {
        GamePhase::Pregame => 0,
        GamePhase::InPlay => 1,
        GamePhase::Final => 2,
    };
    (phase, state.quarter, state.home_score + state.away_score)
}

impl Tracker {
    /// The latest reading this tracker took as true.
    pub fn believed(&self) -> Option<&GameState> {
        self.prev.as_ref()
    }

    /// The frames for one reading of the game Gemini lists as
    /// `event_ticker`: its plays in order, then the margin if anything
    /// happened, then the model if it moved. `prior_home` is the pregame
    /// view of the home side.
    pub fn step(&mut self, event_ticker: &str, prior_home: f64, state: &GameState, ts_ms: u64) -> Vec<SignalFrame> {
        if self.event_ticker.as_deref() != Some(event_ticker) {
            *self = Tracker { event_ticker: Some(event_ticker.to_owned()), ..Tracker::default() };
        }
        // The scoreboard numbers its plays upwards.
        let play = state.last_play.as_ref().and_then(|play| play.id.parse::<u64>().ok());
        let older_play = matches!((play, self.latest_play), (Some(play), Some(latest)) if play < latest);
        let mut corrected = false;
        if let Some(prev) = &self.prev {
            let (was, is) = (progress(prev), progress(state));
            let backwards = is.0 < was.0 || is.1 < was.1 || is.2 < was.2;
            if backwards {
                // Usually an old copy of the scoreboard. Believed only
                // once enough readings in a row have said the same; then
                // it is a correction, and its last play is the newest.
                let seen = match self.doubted {
                    Some((what, seen)) if what == is => seen + 1,
                    _ => 1,
                };
                if seen < BACKSTEP_CONFIRM {
                    self.doubted = Some((is, seen));
                    return Vec::new();
                }
                corrected = true;
            } else if older_play && is == was {
                // The game stands where it stood but the last play is one
                // already passed: an old copy, and nothing in it is news.
                // A reading that is further on is believed for its score,
                // however its last play is numbered.
                return Vec::new();
            }
            self.doubted = None;
        }
        let mut plays = espn::plays_between(self.prev.as_ref(), state);
        if self.prev.is_none() && plays.is_empty() {
            // The first reading says where the game stands, so nothing
            // downstream waits for the next basket to learn the score.
            plays.push(Play { action: "other", result: "none", by_home: None, player: String::new(), home_score_before: state.home_score, away_score_before: state.away_score });
        }
        let mut frames = Vec::with_capacity(plays.len() + 2);
        for (i, play) in plays.iter().enumerate() {
            // A play ends where the next one starts; the last ends on the reading.
            let (home_after, away_after) = plays.get(i + 1).map_or((state.home_score, state.away_score), |next| (next.home_score_before, next.away_score_before));
            self.seq += 1;
            let team = match play.by_home {
                Some(true) => state.home.code.as_str(),
                Some(false) => state.away.code.as_str(),
                None => "",
            };
            frames.push(frame(
                NBA_PBP_SIGNAL,
                ts_ms,
                vec![
                    ("seq", json!(self.seq)),
                    ("action", json!(play.action)),
                    ("result", json!(play.result)),
                    ("team", json!(team)),
                    ("player", json!(play.player)),
                    ("phase", json!(state.phase.as_str())),
                    ("home_team", json!(state.home.code)),
                    ("away_team", json!(state.away.code)),
                    ("quarter", json!(state.quarter)),
                    ("home_score_before", json!(play.home_score_before)),
                    ("away_score_before", json!(play.away_score_before)),
                    ("home_score", json!(home_after)),
                    ("away_score", json!(away_after)),
                    ("clock_remaining", json!(state.clock_secs.normalize().to_string())),
                ],
            ));
        }
        let margin = i64::from(state.home_score) - i64::from(state.away_score);
        // On every play, and when the score changed without one (a correction).
        if !plays.is_empty() || self.margin != Some(margin) {
            frames.push(frame(NBA_MARGIN_SIGNAL, ts_ms, vec![("value", json!(margin.to_string()))]));
            self.margin = Some(margin);
        }
        let model = format!("{:.4}", model_home(prior_home, state));
        if self.model.as_deref() != Some(model.as_str()) {
            frames.push(frame(NBA_WINPROB_SIGNAL, ts_ms, vec![("home", json!(model))]));
            self.model = Some(model);
        }
        // A copy that comes without its last play does not make the play
        // before it news again when it reappears.
        let mut kept = state.clone();
        if kept.last_play.is_none() {
            kept.last_play = self.prev.take().and_then(|prev| prev.last_play);
        }
        self.prev = Some(kept);
        self.latest_play = if corrected { play.or(self.latest_play) } else { play.max(self.latest_play) };
        frames
    }
}

/// One reading of the open game: the frames it makes, with `board`
/// brought up to the latest reading the tracker took as true. The round
/// driver never sees a reading the tracker doubted.
pub fn take_reading(tracker: &mut Tracker, board: &ScoreBoard, event_ticker: &str, prior_home: f64, state: &GameState, ts_ms: u64) -> Vec<SignalFrame> {
    let frames = tracker.step(event_ticker, prior_home, state, ts_ms);
    if let Some(believed) = tracker.believed() {
        board.put(event_ticker, believed);
    }
    frames
}

/// The model's P(home wins): the pregame view before tipoff, then the
/// margin-and-clock model with that view as its prior.
fn model_home(prior_home: f64, state: &GameState) -> f64 {
    let margin = f64::from(state.home_score) - f64::from(state.away_score);
    match state.phase {
        GamePhase::Pregame => prior_home,
        GamePhase::InPlay => {
            let left = winprob::game_seconds_remaining(state.quarter.max(1), state.clock_secs.to_f64().unwrap_or(0.0));
            winprob::win_probability(margin, left, winprob::prior_drift(prior_home))
        }
        GamePhase::Final => winprob::win_probability(margin, 0.0, 0.0),
    }
}

fn frame(signal_id: &str, ts_ms: u64, values: Vec<(&str, Json)>) -> SignalFrame {
    let values: BTreeMap<String, Json> = values.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
    SignalFrame { signal_id: signal_id.to_owned(), ts_ms, quality: Quality::ready(), values }
}

/// Follow whichever game the round driver has open (`cell`): read the
/// scoreboard, keep `board` current for the driver, and publish the
/// game's signals on `bus`. Runs for the life of the process.
pub fn spawn_nba_live(cell: GameCell, board: ScoreBoard, bus: Arc<SignalBus>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let client = reqwest::Client::new();
        let mut tracker = Tracker::default();
        let mut failing = false;
        loop {
            let Some((game, prior_home)) = cell.open() else {
                // No round is open. Nothing is read; look again shortly so
                // a game's signals start with its round.
                tokio::time::sleep(NO_GAME_LOOK).await;
                continue;
            };
            let found = espn::fetch_scoreboard_on(&client, &scoreboard_day(game.tipoff_ms)).await.map(|games| espn::find_game(&games, &game.home.code, &game.away.code, game.tipoff_ms).cloned());
            let mut wait = POLL_IDLE;
            match found {
                Ok(Some(state)) => {
                    if failing {
                        tracing::info!(game = %game.event_ticker, "nba: the scoreboard is readable again");
                        failing = false;
                    }
                    let frames = take_reading(&mut tracker, &board, &game.event_ticker, prior_home, &state, crate::pricing::feed::unix_now_ms());
                    // The request took a while; the round may have moved
                    // on to another game since it was sent.
                    cell.while_open(&game.event_ticker, || {
                        for frame in frames {
                            bus.publish(frame);
                        }
                    });
                    if state.phase == GamePhase::InPlay {
                        wait = POLL_IN_PLAY;
                    }
                }
                Ok(None) if !failing => {
                    failing = true;
                    tracing::warn!(game = %game.event_ticker, "nba: the scoreboard does not list this game");
                }
                Err(e) if !failing => {
                    failing = true;
                    tracing::warn!(game = %game.event_ticker, error = %e, "nba: could not read the scoreboard");
                }
                _ => {}
            }
            tokio::time::sleep(wait).await;
        }
    })
}

#[cfg(test)]
#[path = "nba_live_tests.rs"]
mod tests;

/// Take every `nba.live.*` signal away: not ready on the bus, no value on
/// the platform. Called when the open game changes, so nothing read for
/// one game is ever traded on in the next.
pub fn withdraw_game_signals(bus: &SignalBus, platform: &PlatformHandle) {
    let now = crate::pricing::feed::unix_now_ms();
    for id in [NBA_PBP_SIGNAL, NBA_MARGIN_SIGNAL, NBA_WINPROB_SIGNAL, NBA_POLYMARKET_SIGNAL] {
        bus.set_quality(id, now, Quality { state: "unavailable".into(), reasons: vec!["the game changed".into()] });
        platform.clear_snapshot(id);
    }
}
