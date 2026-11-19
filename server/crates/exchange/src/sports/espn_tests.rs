//! Tests for the ESPN NBA scoreboard reader. Parsing is exercised against the
//! real responses saved on 2026-11-19; `plays_between` against hand-built
//! game states.

use super::*;
use rust_decimal::Decimal;

const LIVE: &str = include_str!("fixtures/scoreboard-live.json");
const PRE: &str = include_str!("fixtures/scoreboard-pregame.json");

// --- parsing against the real live fixture ---------------------------------

#[test]
fn live_parses_twelve_and_den_bos_reads_in_full() {
    let games = parse_scoreboard(LIVE).unwrap();
    assert_eq!(games.len(), 12);

    let g = games.iter().find(|g| g.espn_id == "401910015").unwrap();
    assert_eq!(g.phase, GamePhase::InPlay);
    assert_eq!(g.quarter, 2);
    assert_eq!(g.clock_secs, Decimal::from(720));
    assert_eq!(g.home.code, "DEN");
    assert_eq!(g.home.name, "Denver Nuggets");
    assert_eq!(g.home_score, 24);
    assert_eq!(g.away.code, "BOS");
    assert_eq!(g.away.name, "Boston Celtics");
    assert_eq!(g.away_score, 30);
    // "2026-11-19T02:30Z" (no seconds) as unix ms.
    assert_eq!(g.tipoff_ms, 1_795_055_400_000);

    let lp = g.last_play.as_ref().unwrap();
    assert_eq!(lp.kind, "Substitution");
    // lastPlay.team.id is 2, which is Boston, the away side.
    assert_eq!(lp.by_home, Some(false));
    assert_eq!(lp.player.as_deref(), Some("Paul George"));
}

#[test]
fn gemini_code_differences() {
    let games = parse_scoreboard(LIVE).unwrap();
    // 401910016 is DAL (home) vs GS (away).
    let gs = games.iter().find(|g| g.espn_id == "401910016").unwrap();
    assert_eq!(gs.away.code, "GSW");
    // 401910013 is NO (home) vs DET (away).
    let no = games.iter().find(|g| g.espn_id == "401910013").unwrap();
    assert_eq!(no.home.code, "NOP");
    // 401910017 is LAL (home) vs UTAH (away).
    let utah = games.iter().find(|g| g.espn_id == "401910017").unwrap();
    assert_eq!(utah.away.code, "UTA");

    assert_eq!(gemini_code("NY"), "NYK");
    assert_eq!(gemini_code("SA"), "SAS");
    assert_eq!(gemini_code("WSH"), "WAS");
    assert_eq!(gemini_code("bos"), "BOS");
    // An unmapped code comes back upper-cased, unchanged.
    assert_eq!(gemini_code("den"), "DEN");
}

#[test]
fn final_game_and_pregame_fixture() {
    let games = parse_scoreboard(LIVE).unwrap();
    // 401910007 (CLE vs MEM) is a finished game.
    let fin = games.iter().find(|g| g.espn_id == "401910007").unwrap();
    assert_eq!(fin.phase, GamePhase::Final);

    let pre = parse_scoreboard(PRE).unwrap();
    assert_eq!(pre.len(), 4);
    for g in &pre {
        assert_eq!(g.phase, GamePhase::Pregame);
        assert_eq!(g.quarter, 0);
        assert_eq!(g.home_score, 0);
        assert_eq!(g.away_score, 0);
    }
}

#[test]
fn find_game_within_six_hours() {
    let games = parse_scoreboard(LIVE).unwrap();
    let tip = 1_795_055_400_000u64; // DEN/BOS tipoff.

    let at_exact = find_game(&games, "DEN", "BOS", tip).unwrap();
    assert_eq!(at_exact.espn_id, "401910015");

    let two_h = tip + 2 * 3_600_000;
    assert!(find_game(&games, "DEN", "BOS", two_h).is_some());

    let seven_h = tip + 7 * 3_600_000;
    assert!(find_game(&games, "DEN", "BOS", seven_h).is_none());

    // Home and away swapped must not match.
    assert!(find_game(&games, "BOS", "DEN", tip).is_none());
}

#[test]
fn bad_bodies_err_and_unreadable_events_skip() {
    assert!(parse_scoreboard("this is not json").is_err());
    assert!(parse_scoreboard(r#"{"leagues":[]}"#).is_err());

    // The first event has only one competitor and is skipped; the second
    // parses.
    let body = r#"{"events":[
      {"id":"BAD","date":"2026-11-19T02:30Z",
       "status":{"type":{"state":"in"},"period":1,"clock":100.0},
       "competitions":[{"competitors":[
         {"homeAway":"home","score":"5","team":{"id":"1","abbreviation":"DEN","displayName":"Denver Nuggets"}}
       ]}]},
      {"id":"GOOD","date":"2026-11-19T02:30Z",
       "status":{"type":{"state":"in"},"period":1,"clock":100.0},
       "competitions":[{"competitors":[
         {"homeAway":"home","score":"5","team":{"id":"1","abbreviation":"DEN","displayName":"Denver Nuggets"}},
         {"homeAway":"away","score":"7","team":{"id":"2","abbreviation":"BOS","displayName":"Boston Celtics"}}
       ]}]}
    ]}"#;
    let games = parse_scoreboard(body).unwrap();
    assert_eq!(games.len(), 1);
    assert_eq!(games[0].espn_id, "GOOD");
}

// --- plays_between: hand-built game states ---------------------------------

fn team(code: &str) -> Team {
    Team {
        espn_id: format!("id-{code}"),
        code: code.to_string(),
        name: code.to_string(),
    }
}

fn game(phase: GamePhase, home_score: u32, away_score: u32) -> GameState {
    GameState {
        espn_id: "g".to_string(),
        tipoff_ms: 0,
        phase,
        quarter: 1,
        clock_secs: Decimal::ZERO,
        home: team("DEN"),
        away: team("BOS"),
        home_score,
        away_score,
        last_play: None,
    }
}

fn last(id: &str, kind: &str, by_home: Option<bool>, sv: u32, player: Option<&str>) -> LastPlay {
    LastPlay {
        id: id.to_string(),
        kind: kind.to_string(),
        text: String::new(),
        score_value: sv,
        by_home,
        player: player.map(|p| p.to_string()),
    }
}

fn play(
    action: &'static str,
    result: &'static str,
    by_home: Option<bool>,
    player: &str,
    hb: u32,
    ab: u32,
) -> Play {
    Play {
        action,
        result,
        by_home,
        player: player.to_string(),
        home_score_before: hb,
        away_score_before: ab,
    }
}

#[test]
fn rule1_first_reading() {
    // Non-final first reading: nothing.
    assert!(plays_between(None, &game(GamePhase::InPlay, 10, 8)).is_empty());
    // Final first reading: one `final`, scores before = next's scores.
    let next = game(GamePhase::Final, 100, 99);
    assert_eq!(
        plays_between(None, &next),
        vec![play("final", "none", None, "", 100, 99)]
    );
}

#[test]
fn rule2_tipoff() {
    let prev = game(GamePhase::Pregame, 0, 0);
    let next = game(GamePhase::InPlay, 0, 0);
    assert_eq!(
        plays_between(Some(&prev), &next),
        vec![play("tipoff", "none", None, "", 0, 0)]
    );
}

#[test]
fn rule3_both_sides_score_chains_befores_and_attributes_player() {
    let prev = game(GamePhase::InPlay, 10, 8);
    let mut next = game(GamePhase::InPlay, 12, 11); // home +2, away +3
    // Last play is the away three-pointer: attributes to away only.
    next.last_play = Some(last("p1", "Jump Shot", Some(false), 3, Some("Jayson Tatum")));

    let ps = plays_between(Some(&prev), &next);
    assert_eq!(ps.len(), 2);
    // Home first, before = prev's scores, no player (last play is the away one).
    assert_eq!(ps[0], play("two_pointer", "made", Some(true), "", 10, 8));
    // Away second, before = home's NEW score (12) and away's old (8); player
    // attributed because side and value both match.
    assert_eq!(
        ps[1],
        play("three_pointer", "made", Some(false), "Jayson Tatum", 12, 8)
    );
}

#[test]
fn rule3_free_throw_and_player_not_attributed_on_value_mismatch() {
    let prev = game(GamePhase::InPlay, 10, 8);
    let mut next = game(GamePhase::InPlay, 11, 8); // home +1 -> free throw
    // Side matches (home) but score_value 2 != delta 1, so player is "".
    next.last_play = Some(last("p2", "Free Throw", Some(true), 2, Some("Nikola Jokic")));
    assert_eq!(
        plays_between(Some(&prev), &next),
        vec![play("free_throw", "made", Some(true), "", 10, 8)]
    );
}

#[test]
fn rule3_large_delta_is_other() {
    let prev = game(GamePhase::InPlay, 10, 8);
    let next = game(GamePhase::InPlay, 10, 13); // away +5
    assert_eq!(
        plays_between(Some(&prev), &next),
        vec![play("other", "made", Some(false), "", 10, 8)]
    );
}

#[test]
fn rule4_new_last_play_without_scoring() {
    // Rebound -> rebound/none, by_home and player from the last play.
    let prev = game(GamePhase::InPlay, 10, 8);
    let mut next = game(GamePhase::InPlay, 10, 8);
    next.last_play = Some(last("r1", "Defensive Rebound", Some(true), 0, Some("Aaron Gordon")));
    assert_eq!(
        plays_between(Some(&prev), &next),
        vec![play("rebound", "none", Some(true), "Aaron Gordon", 10, 8)]
    );

    // A missed jump shot classifies as two_pointer/missed.
    let mut miss = game(GamePhase::InPlay, 10, 8);
    miss.last_play = Some(last("s1", "Jump Shot", Some(false), 2, None));
    let ps = plays_between(Some(&prev), &miss);
    assert_eq!(ps.len(), 1);
    assert_eq!(ps[0].action, "two_pointer");
    assert_eq!(ps[0].result, "missed");
    assert_eq!(ps[0].by_home, Some(false));

    // A turnover keyword classifies as turnover/none.
    let mut to = game(GamePhase::InPlay, 10, 8);
    to.last_play = Some(last("t1", "Bad Pass Turnover", Some(true), 0, None));
    assert_eq!(plays_between(Some(&prev), &to)[0].action, "turnover");

    // Same last-play id as prev -> nothing.
    let mut prev_same = game(GamePhase::InPlay, 10, 8);
    prev_same.last_play = Some(last("r1", "Defensive Rebound", Some(true), 0, None));
    let mut next_same = game(GamePhase::InPlay, 10, 8);
    next_same.last_play = Some(last("r1", "Defensive Rebound", Some(true), 0, None));
    assert!(plays_between(Some(&prev_same), &next_same).is_empty());
}

#[test]
fn rule5_final_comes_last_after_scoring() {
    let prev = game(GamePhase::InPlay, 98, 97);
    let next = game(GamePhase::Final, 100, 97); // home +2, then final
    let ps = plays_between(Some(&prev), &next);
    assert_eq!(ps.len(), 2);
    assert_eq!(ps[0], play("two_pointer", "made", Some(true), "", 98, 97));
    assert_eq!(ps[1], play("final", "none", None, "", 100, 97));

    // Pure final transition with no scoring.
    let prev2 = game(GamePhase::InPlay, 100, 97);
    let next2 = game(GamePhase::Final, 100, 97);
    assert_eq!(
        plays_between(Some(&prev2), &next2),
        vec![play("final", "none", None, "", 100, 97)]
    );
}

#[test]
fn rule6_score_decrease_yields_no_play() {
    let prev = game(GamePhase::InPlay, 12, 10);
    let next = game(GamePhase::InPlay, 11, 10); // home corrected down, no last play
    assert!(plays_between(Some(&prev), &next).is_empty());
}

// --- findings of the 2026-11-19 review --------------------------------------

fn live_with(espn_id: &str, edit: impl Fn(&mut serde_json::Value)) -> String {
    let mut v: serde_json::Value = serde_json::from_str(LIVE).unwrap();
    let event = v["events"].as_array_mut().unwrap().iter_mut().find(|e| e["id"] == espn_id).expect("the event is in the fixture");
    edit(event);
    v.to_string()
}

#[test]
fn a_game_that_is_over_without_being_played_is_not_a_final() {
    // A postponed game is filed as "post", 0-0. It never ended; it must
    // not read as a final score.
    let postponed = live_with("401910015", |e| {
        e["status"]["type"]["state"] = serde_json::json!("post");
        e["status"]["type"]["completed"] = serde_json::json!(false);
        e["status"]["type"]["name"] = serde_json::json!("STATUS_POSTPONED");
    });
    let games = parse_scoreboard(&postponed).unwrap();
    assert_eq!(games.len(), 11, "the postponed game is left out");
    assert!(games.iter().all(|g| g.espn_id != "401910015"));
    // A game that did finish still reads as final.
    assert!(parse_scoreboard(LIVE).unwrap().iter().any(|g| g.phase == GamePhase::Final));
}

#[test]
fn the_clock_is_read_to_the_tenth_without_float_noise() {
    let body = live_with("401910015", |e| e["status"]["clock"] = serde_json::json!(12.3));
    let games = parse_scoreboard(&body).unwrap();
    let g = games.iter().find(|g| g.espn_id == "401910015").unwrap();
    assert_eq!(g.clock_secs.to_string(), "12.3");
}

#[test]
fn a_game_is_found_whichever_spelling_of_a_team_gemini_uses() {
    let games = parse_scoreboard(LIVE).unwrap();
    let was_tor = games.iter().find(|g| g.home.code == "TOR" && g.away.code == "WAS").expect("Washington at Toronto is in the fixture");
    let tipoff = was_tor.tipoff_ms;
    assert!(find_game(&games, "TOR", "WAS", tipoff).is_some());
    assert_eq!(find_game(&games, "TOR", "WSH", tipoff).map(|g| g.espn_id.as_str()), Some(was_tor.espn_id.as_str()));
    assert_eq!(find_game(&games, "tor", "wsh", tipoff).map(|g| g.espn_id.as_str()), Some(was_tor.espn_id.as_str()));
}
