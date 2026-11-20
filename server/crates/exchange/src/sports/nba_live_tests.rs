//! The NBA signal tracker, reading by reading, against the generated
//! registry.

use rust_decimal::Decimal;

use super::*;
use crate::automation::signal_bus::{field_types, to_signal_event};
use crate::automation::signals::SignalStore;
use crate::registry::{self, NBA_MARGIN_SIGNAL, NBA_PBP_SIGNAL, NBA_WINPROB_SIGNAL};
use crate::sports::espn::{GamePhase, GameState, LastPlay, Team};

const TICKER: &str = "NBA-2611190230-BOS-DEN-M";
const TIPOFF: u64 = 1_795_055_400_000;

fn team(id: &str, code: &str, name: &str) -> Team {
    Team { espn_id: id.into(), code: code.into(), name: name.into() }
}

fn reading(phase: GamePhase, quarter: u8, clock: u32, home: u32, away: u32) -> GameState {
    GameState {
        espn_id: "401910015".into(),
        tipoff_ms: TIPOFF,
        phase,
        quarter,
        clock_secs: Decimal::from(clock),
        home: team("7", "DEN", "Denver Nuggets"),
        away: team("2", "BOS", "Boston Celtics"),
        home_score: home,
        away_score: away,
        last_play: None,
    }
}

fn ids(frames: &[SignalFrame]) -> Vec<&str> {
    frames.iter().map(|f| f.signal_id.as_str()).collect()
}

fn text<'a>(frame: &'a SignalFrame, field: &str) -> &'a str {
    frame.values[field].as_str().unwrap_or_else(|| panic!("{field} is not text: {:?}", frame.values[field]))
}

fn number(frame: &SignalFrame, field: &str) -> u64 {
    frame.values[field].as_u64().unwrap_or_else(|| panic!("{field} is not a number: {:?}", frame.values[field]))
}

fn prob(frame: &SignalFrame) -> f64 {
    text(frame, "home").parse().unwrap()
}

#[test]
fn the_first_reading_of_a_game_in_play_states_where_the_game_is() {
    let mut t = Tracker::default();
    let frames = t.step(TICKER, 0.5, &reading(GamePhase::InPlay, 2, 720, 24, 30), 1_000);

    assert_eq!(ids(&frames), vec![NBA_PBP_SIGNAL, NBA_MARGIN_SIGNAL, NBA_WINPROB_SIGNAL]);
    let pbp = &frames[0];
    assert_eq!(number(pbp, "seq"), 1);
    assert_eq!((text(pbp, "action"), text(pbp, "result"), text(pbp, "phase")), ("other", "none", "in_play"));
    assert_eq!((text(pbp, "home_team"), text(pbp, "away_team")), ("DEN", "BOS"));
    assert_eq!((text(pbp, "team"), text(pbp, "player")), ("", ""));
    assert_eq!(number(pbp, "quarter"), 2);
    assert_eq!(text(pbp, "clock_remaining"), "720");
    assert_eq!((number(pbp, "home_score_before"), number(pbp, "away_score_before")), (24, 30));
    assert_eq!((number(pbp, "home_score"), number(pbp, "away_score")), (24, 30));
    assert_eq!(text(&frames[1], "value"), "-6");
    let p = prob(&frames[2]);
    assert!(p > 0.2 && p < 0.5, "six down early in the second with no pregame view: {p}");
    assert!(frames.iter().all(|f| f.ts_ms == 1_000));
}

#[test]
fn a_made_three_is_a_play_with_its_scores_then_the_margin_then_the_model() {
    let mut t = Tracker::default();
    let before = t.step(TICKER, 0.5, &reading(GamePhase::InPlay, 2, 720, 24, 30), 1_000);
    let mut next = reading(GamePhase::InPlay, 2, 700, 27, 30);
    next.last_play = Some(LastPlay { id: "p9".into(), kind: "Jump Shot".into(), text: "makes a three".into(), score_value: 3, by_home: Some(true), player: Some("Jamal Murray".into()) });
    let frames = t.step(TICKER, 0.5, &next, 6_000);

    assert_eq!(ids(&frames), vec![NBA_PBP_SIGNAL, NBA_MARGIN_SIGNAL, NBA_WINPROB_SIGNAL]);
    let pbp = &frames[0];
    assert_eq!(number(pbp, "seq"), 2);
    assert_eq!((text(pbp, "action"), text(pbp, "result")), ("three_pointer", "made"));
    assert_eq!((text(pbp, "team"), text(pbp, "player")), ("DEN", "Jamal Murray"));
    assert_eq!((number(pbp, "home_score_before"), number(pbp, "away_score_before")), (24, 30));
    assert_eq!((number(pbp, "home_score"), number(pbp, "away_score")), (27, 30));
    assert_eq!(text(pbp, "clock_remaining"), "700");
    assert_eq!(text(&frames[1], "value"), "-3");
    assert!(prob(&frames[2]) > prob(&before[2]), "the home side is closer than it was");
}

#[test]
fn both_sides_scoring_between_two_readings_is_two_plays_in_order() {
    let mut t = Tracker::default();
    t.step(TICKER, 0.5, &reading(GamePhase::InPlay, 2, 720, 24, 30), 1_000);
    let frames = t.step(TICKER, 0.5, &reading(GamePhase::InPlay, 2, 690, 26, 31), 6_000);

    assert_eq!(ids(&frames), vec![NBA_PBP_SIGNAL, NBA_PBP_SIGNAL, NBA_MARGIN_SIGNAL, NBA_WINPROB_SIGNAL]);
    let (home, away) = (&frames[0], &frames[1]);
    assert_eq!((number(home, "seq"), text(home, "team"), text(home, "action")), (2, "DEN", "two_pointer"));
    assert_eq!((number(home, "home_score"), number(home, "away_score")), (26, 30), "the away basket has not happened yet");
    assert_eq!((number(away, "seq"), text(away, "team"), text(away, "action")), (3, "BOS", "free_throw"));
    assert_eq!((number(away, "home_score_before"), number(away, "away_score_before")), (26, 30));
    assert_eq!((number(away, "home_score"), number(away, "away_score")), (26, 31));
    assert_eq!(text(&frames[2], "value"), "-5");
}

#[test]
fn a_reading_with_nothing_new_publishes_nothing_but_a_moved_model() {
    let mut t = Tracker::default();
    let state = reading(GamePhase::InPlay, 2, 720, 24, 30);
    t.step(TICKER, 0.5, &state, 1_000);
    assert!(t.step(TICKER, 0.5, &state, 6_000).is_empty(), "the same reading twice");
    // Five minutes run off the clock with no score: only the model moves.
    let frames = t.step(TICKER, 0.5, &reading(GamePhase::InPlay, 2, 420, 24, 30), 300_000);
    assert_eq!(ids(&frames), vec![NBA_WINPROB_SIGNAL]);
}

#[test]
fn before_tipoff_the_model_is_the_pregame_view_and_tipoff_is_a_play() {
    let mut t = Tracker::default();
    let frames = t.step(TICKER, 0.325, &reading(GamePhase::Pregame, 0, 0, 0, 0), 1_000);
    assert_eq!(ids(&frames), vec![NBA_PBP_SIGNAL, NBA_MARGIN_SIGNAL, NBA_WINPROB_SIGNAL]);
    assert_eq!((text(&frames[0], "phase"), text(&frames[0], "action")), ("pregame", "other"));
    assert_eq!(text(&frames[1], "value"), "0");
    assert_eq!(text(&frames[2], "home"), "0.3250");
    assert!(t.step(TICKER, 0.325, &reading(GamePhase::Pregame, 0, 0, 0, 0), 31_000).is_empty());

    let frames = t.step(TICKER, 0.325, &reading(GamePhase::InPlay, 1, 720, 0, 0), 61_000);
    assert_eq!((text(&frames[0], "action"), text(&frames[0], "phase")), ("tipoff", "in_play"));
    assert_eq!(ids(&frames), vec![NBA_PBP_SIGNAL, NBA_MARGIN_SIGNAL], "level at tipoff the model is still the pregame view, so it is not republished");
    // The first basket moves it, from the pregame view.
    let frames = t.step(TICKER, 0.325, &reading(GamePhase::InPlay, 1, 700, 2, 0), 66_000);
    let p = prob(frames.last().unwrap());
    assert!(p > 0.325 && p < 0.40, "{p}");
}

#[test]
fn a_final_is_the_last_play_and_the_model_is_certain() {
    let mut t = Tracker::default();
    let before = t.step(TICKER, 0.5, &reading(GamePhase::InPlay, 4, 90, 110, 112), 1_000);
    assert!(prob(before.last().unwrap()) > 0.05, "two down with ninety seconds left is not over");
    let frames = t.step(TICKER, 0.5, &reading(GamePhase::Final, 4, 0, 110, 112), 6_000);
    assert_eq!(ids(&frames), vec![NBA_PBP_SIGNAL, NBA_MARGIN_SIGNAL, NBA_WINPROB_SIGNAL]);
    assert_eq!((text(&frames[0], "action"), text(&frames[0], "phase")), ("final", "final"));
    assert_eq!(text(&frames[1], "value"), "-2");
    assert!(prob(&frames[2]) < 0.02);
    assert!(t.step(TICKER, 0.5, &reading(GamePhase::Final, 4, 0, 110, 112), 11_000).is_empty());
}

#[test]
fn a_new_game_starts_its_plays_from_one() {
    let mut t = Tracker::default();
    t.step(TICKER, 0.5, &reading(GamePhase::InPlay, 2, 720, 24, 30), 1_000);
    t.step(TICKER, 0.5, &reading(GamePhase::InPlay, 2, 700, 27, 30), 6_000);
    let frames = t.step("NBA-2611190300-UTA-LAL-M", 0.6, &reading(GamePhase::InPlay, 1, 300, 10, 8), 11_000);
    assert_eq!(ids(&frames), vec![NBA_PBP_SIGNAL, NBA_MARGIN_SIGNAL, NBA_WINPROB_SIGNAL]);
    assert_eq!(number(&frames[0], "seq"), 1);
    assert_eq!(text(&frames[1], "value"), "2");
}

/// Every frame the tracker emits must type against the registry entry for
/// its signal, or the platform drops it and no strategy ever sees it.
#[test]
fn every_frame_types_against_the_generated_registry() {
    let reg = registry::for_tests();
    let mut store = SignalStore::from_registry(&reg);
    let mut t = Tracker::default();
    let mut frames = t.step(TICKER, 0.325, &reading(GamePhase::Pregame, 0, 0, 0, 0), 1_000);
    frames.extend(t.step(TICKER, 0.325, &reading(GamePhase::InPlay, 1, 700, 3, 0), 2_000));
    let mut foul = reading(GamePhase::InPlay, 1, 690, 3, 0);
    foul.last_play = Some(LastPlay { id: "p2".into(), kind: "Personal Foul".into(), text: "foul".into(), score_value: 0, by_home: Some(false), player: None });
    frames.extend(t.step(TICKER, 0.325, &foul, 3_000));
    frames.extend(t.step(TICKER, 0.325, &reading(GamePhase::Final, 4, 0, 99, 101), 4_000));
    assert!(frames.len() >= 10, "{}", frames.len());
    for f in frames {
        assert!(store.enable(&f.signal_id), "{} in registry", f.signal_id);
        let def = reg.signals.iter().find(|s| s.signal_id == f.signal_id).unwrap();
        let event = to_signal_event(&f, &field_types(def)).unwrap_or_else(|e| panic!("{} does not type: {e}: {:?}", f.signal_id, f.values));
        let typed = store.ingest(&event).unwrap_or_else(|e| panic!("{}: {e}", f.signal_id));
        assert_eq!(typed.len(), def.fields.len(), "{}", f.signal_id);
    }
}

#[test]
fn the_scoreboard_is_asked_for_the_game_s_day_in_new_york() {
    // 01:00Z on the 20th is eight in the evening of the 19th in New York.
    assert_eq!(scoreboard_day(1_795_136_400_000), "20261119");
    // 17:30Z on the 21st is half past noon the same day.
    assert_eq!(scoreboard_day(1_795_282_200_000), "20261121");
}

#[test]
fn the_round_driver_reads_a_game_s_score_by_its_gemini_ticker() {
    let board = ScoreBoard::default();
    let score = board.score_fn();
    let game = crate::gemini::nba::nba_games_from_events(include_str!("../gemini/fixtures/nba-moneyline-active.json")).unwrap().into_iter().find(|g| g.event_ticker == TICKER).unwrap();
    assert_eq!(score(&game), None, "no reading yet");
    board.put(TICKER, &reading(GamePhase::InPlay, 2, 720, 24, 30));
    assert_eq!(score(&game), Some(Score { home: 24, away: 30, is_final: false, in_play: true }));
    board.put(TICKER, &reading(GamePhase::Final, 4, 0, 101, 112));
    assert_eq!(score(&game), Some(Score { home: 101, away: 112, is_final: true, in_play: false }));
}

// --- findings of the 2026-11-19 review --------------------------------------

fn count(frames: &[SignalFrame], id: &str) -> usize {
    frames.iter().filter(|f| f.signal_id == id).count()
}

#[test]
fn a_reading_that_goes_backwards_once_is_not_believed() {
    let mut t = Tracker::default();
    t.step(TICKER, 0.5, &reading(GamePhase::InPlay, 2, 300, 48, 48), 1);
    let basket = t.step(TICKER, 0.5, &reading(GamePhase::InPlay, 2, 290, 50, 48), 2);
    assert_eq!(count(&basket, NBA_PBP_SIGNAL), 1);
    // An older copy of the scoreboard, then the current one again.
    assert!(t.step(TICKER, 0.5, &reading(GamePhase::InPlay, 2, 300, 48, 48), 3).is_empty(), "a stale reading publishes nothing");
    let again = t.step(TICKER, 0.5, &reading(GamePhase::InPlay, 2, 290, 50, 48), 4);
    assert!(again.is_empty(), "the same basket is not published twice: {again:?}");
}

#[test]
fn a_final_that_flickers_back_into_play_is_final_once() {
    let finals = |frames: &[SignalFrame]| frames.iter().filter(|f| f.signal_id == NBA_PBP_SIGNAL && text(f, "action") == "final").count();
    let mut t = Tracker::default();
    t.step(TICKER, 0.5, &reading(GamePhase::InPlay, 4, 1, 110, 100), 1);
    assert_eq!(finals(&t.step(TICKER, 0.5, &reading(GamePhase::Final, 4, 0, 110, 100), 2)), 1);
    assert!(t.step(TICKER, 0.5, &reading(GamePhase::InPlay, 4, 1, 110, 100), 3).is_empty());
    assert!(t.step(TICKER, 0.5, &reading(GamePhase::Final, 4, 0, 110, 100), 4).is_empty(), "no second final");
}

#[test]
fn a_score_that_stays_lower_is_a_correction_and_the_margin_follows_it() {
    let mut t = Tracker::default();
    t.step(TICKER, 0.5, &reading(GamePhase::InPlay, 2, 300, 50, 48), 1);
    // The officials took a basket off the board: every reading from here
    // on says so.
    let corrected = reading(GamePhase::InPlay, 2, 280, 48, 48);
    for ts in 0..u64::from(BACKSTEP_CONFIRM) - 1 {
        assert!(t.step(TICKER, 0.5, &corrected, 2 + ts).is_empty(), "not believed yet");
    }
    let believed = t.step(TICKER, 0.5, &corrected, 9);
    assert_eq!(count(&believed, NBA_PBP_SIGNAL), 0, "a correction is not a play");
    let margin = believed.iter().find(|f| f.signal_id == NBA_MARGIN_SIGNAL).expect("the margin is said again");
    assert_eq!(text(margin, "value"), "0");
    assert_eq!(count(&believed, NBA_WINPROB_SIGNAL), 1);
    // And the game goes on from the corrected score.
    let next = t.step(TICKER, 0.5, &reading(GamePhase::InPlay, 2, 270, 48, 51), 10);
    assert_eq!(count(&next, NBA_PBP_SIGNAL), 1);
}

fn with_play(mut state: GameState, id: &str, kind: &str) -> GameState {
    state.last_play = Some(LastPlay { id: id.into(), kind: kind.into(), text: String::new(), score_value: 0, by_home: Some(true), player: None });
    state
}

#[test]
fn an_older_copy_with_the_same_score_does_not_replay_its_last_play() {
    let mut t = Tracker::default();
    let older = with_play(reading(GamePhase::InPlay, 2, 300, 50, 48), "401910015100", "Defensive Rebound");
    let newer = with_play(reading(GamePhase::InPlay, 2, 290, 50, 48), "401910015104", "Personal Foul");
    t.step(TICKER, 0.5, &older, 1);
    assert_eq!(count(&t.step(TICKER, 0.5, &newer, 2), NBA_PBP_SIGNAL), 1, "the foul is news once");
    // The two copies keep alternating: neither is news again, and the
    // older clock does not move the model back.
    for ts in [3, 5, 7] {
        assert!(t.step(TICKER, 0.5, &older, ts).is_empty(), "an older copy publishes nothing");
        assert!(t.step(TICKER, 0.5, &newer, ts + 1).is_empty(), "nor does the copy already seen");
    }
}

#[test]
fn only_a_reading_that_was_believed_is_kept_as_the_score() {
    let mut t = Tracker::default();
    let board = ScoreBoard::default();
    let score = board.score_fn();
    let game = crate::gemini::nba::nba_games_from_events(include_str!("../gemini/fixtures/nba-moneyline-active.json")).unwrap().into_iter().find(|g| g.event_ticker == TICKER).unwrap();
    assert!(t.believed().is_none());
    take_reading(&mut t, &board, TICKER, 0.5, &reading(GamePhase::InPlay, 4, 1, 110, 100), 1);
    take_reading(&mut t, &board, TICKER, 0.5, &reading(GamePhase::Final, 4, 0, 110, 100), 2);
    // An old in-play copy after the final must not reach the round
    // driver: it would restart the wait before a final score is trusted.
    take_reading(&mut t, &board, TICKER, 0.5, &reading(GamePhase::InPlay, 4, 1, 110, 100), 3);
    assert_eq!(t.believed().map(|s| s.phase), Some(GamePhase::Final));
    assert_eq!(score(&game), Some(Score { home: 110, away: 100, is_final: true, in_play: false }), "the board holds the final, not the old copy");
}

#[test]
fn a_reading_without_a_last_play_does_not_make_the_one_before_it_news_again() {
    let mut t = Tracker::default();
    let foul = with_play(reading(GamePhase::InPlay, 2, 290, 50, 48), "401910015104", "Personal Foul");
    let blank = reading(GamePhase::InPlay, 2, 290, 50, 48);
    t.step(TICKER, 0.5, &with_play(reading(GamePhase::InPlay, 2, 300, 50, 48), "401910015100", "Defensive Rebound"), 1);
    assert_eq!(count(&t.step(TICKER, 0.5, &foul, 2), NBA_PBP_SIGNAL), 1, "the foul is news once");
    // One copy of the scoreboard comes without its last play, then the
    // play is back: it was already published.
    assert_eq!(count(&t.step(TICKER, 0.5, &blank, 3), NBA_PBP_SIGNAL), 0);
    assert_eq!(count(&t.step(TICKER, 0.5, &foul, 4), NBA_PBP_SIGNAL), 0, "the same foul is not a second play");
}

#[test]
fn a_correction_that_takes_back_the_last_play_is_believed_once_it_stands() {
    let mut t = Tracker::default();
    t.step(TICKER, 0.5, &with_play(reading(GamePhase::InPlay, 2, 300, 48, 48), "401910015100", "Defensive Rebound"), 1);
    t.step(TICKER, 0.5, &with_play(reading(GamePhase::InPlay, 2, 290, 50, 48), "401910015104", "Jump Shot"), 2);
    // The basket is waved off: the score steps back and so does the last play.
    let corrected = with_play(reading(GamePhase::InPlay, 2, 288, 48, 48), "401910015100", "Defensive Rebound");
    assert!(t.step(TICKER, 0.5, &corrected, 3).is_empty());
    assert!(t.step(TICKER, 0.5, &corrected, 4).is_empty());
    let frames = t.step(TICKER, 0.5, &corrected, 5);
    let margin = frames.iter().find(|f| f.signal_id == NBA_MARGIN_SIGNAL).expect("the third reading in a row is believed");
    assert_eq!(text(margin, "value"), "0");
    assert_eq!(t.believed().map(|s| (s.home_score, s.away_score)), Some((48, 48)));
    // A play numbered after the corrected one is news again.
    let next = with_play(reading(GamePhase::InPlay, 2, 280, 48, 48), "401910015102", "Personal Foul");
    assert_eq!(count(&t.step(TICKER, 0.5, &next, 6), NBA_PBP_SIGNAL), 1);
}

#[test]
fn withdrawing_takes_every_game_signal_off_the_bus_and_the_platform() {
    use crate::automation::sme::task::{PlatformCommand, PlatformHandle};
    let (platform, mut inbox) = PlatformHandle::channel();
    let bus = SignalBus::new(registry::for_tests(), Some(platform.clone()));
    let mut t = Tracker::default();
    for frame in t.step(TICKER, 0.5, &reading(GamePhase::InPlay, 2, 300, 50, 48), 1) {
        bus.publish(frame);
    }
    while inbox.try_recv().is_ok() {}
    assert_eq!(bus.latest(NBA_MARGIN_SIGNAL).unwrap().quality.state, "ready");

    withdraw_game_signals(&bus, &platform);

    for id in [NBA_PBP_SIGNAL, NBA_MARGIN_SIGNAL, NBA_WINPROB_SIGNAL, crate::registry::NBA_POLYMARKET_SIGNAL, crate::registry::NBA_KALSHI_SIGNAL] {
        assert_ne!(bus.latest(id).map(|f| f.quality.state.clone()).unwrap_or_default(), "ready", "{id}");
    }
    let mut cleared = Vec::new();
    while let Ok(command) = inbox.try_recv() {
        if let PlatformCommand::ClearSnapshot(id) = command {
            cleared.push(id);
        }
    }
    cleared.sort();
    assert_eq!(cleared, vec![crate::registry::NBA_KALSHI_SIGNAL, NBA_MARGIN_SIGNAL, NBA_PBP_SIGNAL, crate::registry::NBA_POLYMARKET_SIGNAL, NBA_WINPROB_SIGNAL]);
}

#[test]
fn a_reading_with_a_newer_score_is_believed_whatever_its_last_play_is_numbered() {
    let mut t = Tracker::default();
    t.step(TICKER, 0.5, &with_play(reading(GamePhase::InPlay, 2, 300, 50, 48), "401910015104", "Personal Foul"), 1);
    // The score has moved on; the play it names carries a lower number
    // than one already seen. The score is what the game says: an old copy
    // cannot be ahead.
    let ahead = with_play(reading(GamePhase::InPlay, 2, 280, 52, 48), "401910015090", "Jump Shot");
    let frames = t.step(TICKER, 0.5, &ahead, 2);
    let margin = frames.iter().find(|f| f.signal_id == NBA_MARGIN_SIGNAL).expect("the new score is published");
    assert_eq!(text(margin, "value"), "4");
    assert_eq!(t.believed().map(|s| (s.home_score, s.away_score)), Some((52, 48)));
}
