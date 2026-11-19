//! ESPN's public NBA scoreboard, read into a small typed game state, with a
//! pure diff of what happened between two readings. Nothing here but
//! [`fetch_scoreboard`] touches the network; everything else is a pure
//! function over a response body and over [`GameState`]s.
//!
//! Three things about the wire the shape alone does not make obvious:
//!
//! * `date` is RFC 3339 but ESPN drops the seconds (`2026-11-19T02:30Z`), so
//!   both forms are accepted.
//! * `score` is a string, `status.clock` a number of seconds left in the
//!   period.
//! * ESPN's three-letter abbreviation differs from the code Gemini uses for a
//!   handful of teams; [`gemini_code`] reconciles them.

use rust_decimal::Decimal;
use serde::Deserialize;

/// ESPN's public NBA scoreboard endpoint.
pub const SCOREBOARD_URL: &str =
    "https://site.api.espn.com/apis/site/v2/sports/basketball/nba/scoreboard";

/// Where a game is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GamePhase {
    Pregame,
    InPlay,
    Final,
}

impl GamePhase {
    /// The lowercase wire form used elsewhere in the demo.
    pub fn as_str(self) -> &'static str {
        match self {
            GamePhase::Pregame => "pregame",
            GamePhase::InPlay => "in_play",
            GamePhase::Final => "final",
        }
    }
}

/// One side of a game.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Team {
    /// ESPN's team id (`team.id`).
    pub espn_id: String,
    /// The three-letter code Gemini uses, from ESPN's abbreviation through
    /// [`gemini_code`].
    pub code: String,
    /// ESPN's `team.displayName`.
    pub name: String,
}

/// The latest play ESPN reports for a live game.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastPlay {
    pub id: String,
    /// `type.text`.
    pub kind: String,
    pub text: String,
    pub score_value: u32,
    /// `true` when the play's team is the home side, `false` the away side,
    /// `None` when absent or matching neither competitor.
    pub by_home: Option<bool>,
    pub player: Option<String>,
}

/// A single game read from one scoreboard response.
#[derive(Debug, Clone, PartialEq)]
pub struct GameState {
    pub espn_id: String,
    /// Tipoff from `date`, as unix milliseconds.
    pub tipoff_ms: u64,
    pub phase: GamePhase,
    /// `status.period`: 0 before tipoff, 5+ is overtime.
    pub quarter: u8,
    /// `status.clock`, seconds left in the period.
    pub clock_secs: Decimal,
    pub home: Team,
    pub away: Team,
    pub home_score: u32,
    pub away_score: u32,
    pub last_play: Option<LastPlay>,
}

/// Gemini's code for an ESPN abbreviation. ESPN differs for exactly these:
/// `GS`→`GSW`, `NO`→`NOP`, `NY`→`NYK`, `SA`→`SAS`, `UTAH`→`UTA`,
/// `WSH`→`WAS`. Everything else is returned unchanged (upper-cased).
pub fn gemini_code(espn_abbreviation: &str) -> String {
    let up = espn_abbreviation.to_uppercase();
    match up.as_str() {
        "GS" => "GSW".to_string(),
        "NO" => "NOP".to_string(),
        "NY" => "NYK".to_string(),
        "SA" => "SAS".to_string(),
        "UTAH" => "UTA".to_string(),
        "WSH" => "WAS".to_string(),
        _ => up,
    }
}

// ---------------------------------------------------------------------------
// Wire types. Deserialized loosely: the whole body must be the expected JSON
// with an `events` array, but a single event that cannot be read is skipped,
// not fatal.
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct RawScoreboard {
    events: Vec<serde_json::Value>,
}

#[derive(Deserialize)]
struct RawEvent {
    id: String,
    date: String,
    status: RawStatus,
    competitions: Vec<RawCompetition>,
}

#[derive(Deserialize)]
struct RawStatus {
    #[serde(rename = "type")]
    type_: RawStatusType,
    period: u8,
    clock: f64,
}

#[derive(Deserialize)]
struct RawStatusType {
    state: String,
    /// False on a game filed as over that was never played out
    /// (postponed, cancelled, suspended).
    #[serde(default)]
    completed: Option<bool>,
}

#[derive(Deserialize)]
struct RawCompetition {
    competitors: Vec<RawCompetitor>,
    #[serde(default)]
    situation: Option<RawSituation>,
}

#[derive(Deserialize)]
struct RawCompetitor {
    #[serde(rename = "homeAway")]
    home_away: String,
    score: String,
    team: RawTeam,
}

#[derive(Deserialize)]
struct RawTeam {
    id: String,
    abbreviation: String,
    #[serde(rename = "displayName")]
    display_name: String,
}

#[derive(Deserialize)]
struct RawSituation {
    #[serde(default, rename = "lastPlay")]
    last_play: Option<RawLastPlay>,
}

#[derive(Deserialize)]
struct RawLastPlay {
    id: String,
    #[serde(default, rename = "type")]
    type_: Option<RawPlayType>,
    #[serde(default)]
    text: String,
    #[serde(default, rename = "scoreValue")]
    score_value: u32,
    #[serde(default)]
    team: Option<RawPlayTeam>,
    #[serde(default, rename = "athletesInvolved")]
    athletes_involved: Vec<RawAthlete>,
}

#[derive(Deserialize)]
struct RawPlayType {
    #[serde(default)]
    text: String,
}

#[derive(Deserialize)]
struct RawPlayTeam {
    id: String,
}

#[derive(Deserialize)]
struct RawAthlete {
    #[serde(default, rename = "displayName")]
    display_name: String,
}

/// `date` → unix milliseconds. ESPN writes RFC 3339 but sometimes drops the
/// seconds (`2026-11-19T02:30Z`); both forms parse.
fn date_to_ms(s: &str) -> Option<u64> {
    let parsed = chrono::DateTime::parse_from_rfc3339(s)
        .or_else(|_| chrono::DateTime::parse_from_rfc3339(&with_seconds(s)));
    parsed.ok().and_then(|t| u64::try_from(t.timestamp_millis()).ok())
}

/// Insert `:00` seconds into an RFC-3339 string whose time is only `HH:MM`.
/// Returns the input unchanged when it already has seconds or has no `T`.
fn with_seconds(s: &str) -> String {
    let Some(t_idx) = s.find('T') else {
        return s.to_string();
    };
    let rest = &s[t_idx + 1..];
    let (time_part, tz_part) = match rest.find(['Z', '+', '-']) {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    if time_part.matches(':').count() == 1 {
        format!("{}T{}:00{}", &s[..t_idx], time_part, tz_part)
    } else {
        s.to_string()
    }
}

fn phase_of(state: &str, period: u8) -> GamePhase {
    match state {
        "pre" => GamePhase::Pregame,
        "in" => GamePhase::InPlay,
        "post" => GamePhase::Final,
        _ if period == 0 => GamePhase::Pregame,
        _ => GamePhase::InPlay,
    }
}

fn team_of(c: &RawCompetitor) -> Team {
    Team {
        espn_id: c.team.id.clone(),
        code: gemini_code(&c.team.abbreviation),
        name: c.team.display_name.clone(),
    }
}

/// Convert one raw event into a [`GameState`], or `None` if it cannot be read
/// (missing competitor, unparsable date).
fn game_of(ev: RawEvent) -> Option<GameState> {
    if ev.status.type_.state == "post" && ev.status.type_.completed == Some(false) {
        // Over without a result. Left out rather than read as a final:
        // nothing downstream may take its 0-0 for a score.
        return None;
    }
    let tipoff_ms = date_to_ms(&ev.date)?;
    let comp = ev.competitions.into_iter().next()?;

    let mut home: Option<&RawCompetitor> = None;
    let mut away: Option<&RawCompetitor> = None;
    for c in &comp.competitors {
        match c.home_away.as_str() {
            "home" => home = Some(c),
            "away" => away = Some(c),
            _ => {}
        }
    }
    let (home, away) = (home?, away?);

    let home_team = team_of(home);
    let away_team = team_of(away);
    let home_id = home.team.id.clone();
    let away_id = away.team.id.clone();

    let last_play = comp
        .situation
        .and_then(|s| s.last_play)
        .map(|lp| last_play_of(lp, &home_id, &away_id));

    Some(GameState {
        espn_id: ev.id,
        tipoff_ms,
        phase: phase_of(&ev.status.type_.state, ev.status.period),
        quarter: ev.status.period,
        // The scoreboard counts in tenths; the float carries noise beyond them.
        clock_secs: Decimal::from_f64_retain(ev.status.clock).unwrap_or_default().round_dp(1).normalize(),
        home: home_team,
        away: away_team,
        home_score: home.score.trim().parse().unwrap_or(0),
        away_score: away.score.trim().parse().unwrap_or(0),
        last_play,
    })
}

fn last_play_of(lp: RawLastPlay, home_id: &str, away_id: &str) -> LastPlay {
    let by_home = lp.team.as_ref().and_then(|t| {
        if t.id == home_id {
            Some(true)
        } else if t.id == away_id {
            Some(false)
        } else {
            None
        }
    });
    let player = lp
        .athletes_involved
        .into_iter()
        .next()
        .map(|a| a.display_name)
        .filter(|n| !n.is_empty());
    LastPlay {
        id: lp.id,
        kind: lp.type_.map(|t| t.text).unwrap_or_default(),
        text: lp.text,
        score_value: lp.score_value,
        by_home,
        player,
    }
}

/// Every game in a scoreboard response. A game that cannot be read (missing
/// competitor, unparsable date) is skipped, not fatal; a body that is not the
/// expected JSON at the top level is an `Err`.
pub fn parse_scoreboard(body: &str) -> Result<Vec<GameState>, String> {
    let board: RawScoreboard =
        serde_json::from_str(body).map_err(|e| format!("scoreboard JSON: {e}"))?;
    Ok(board
        .events
        .into_iter()
        .filter_map(|v| serde_json::from_value::<RawEvent>(v).ok())
        .filter_map(game_of)
        .collect())
}

/// The game between `home_code` and `away_code` (Gemini's codes) whose tipoff
/// is within six hours of `tipoff_ms`.
pub fn find_game<'a>(
    games: &'a [GameState],
    home_code: &str,
    away_code: &str,
    tipoff_ms: u64,
) -> Option<&'a GameState> {
    const SIX_HOURS_MS: i64 = 6 * 60 * 60 * 1000;
    // Gemini's spelling goes through the same table as the scoreboard's,
    // so the two meet whichever one a team is written in.
    let (home_code, away_code) = (gemini_code(home_code), gemini_code(away_code));
    games.iter().find(|g| {
        g.home.code == home_code
            && g.away.code == away_code
            && (g.tipoff_ms as i64 - tipoff_ms as i64).abs() <= SIX_HOURS_MS
    })
}

/// One thing that happened in a game.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Play {
    pub action: &'static str,
    pub result: &'static str,
    pub by_home: Option<bool>,
    pub player: String,
    pub home_score_before: u32,
    pub away_score_before: u32,
}

fn scoring_play(
    by_home: bool,
    d: u32,
    next: &GameState,
    home_before: u32,
    away_before: u32,
) -> Play {
    let action = match d {
        1 => "free_throw",
        2 => "two_pointer",
        3 => "three_pointer",
        _ => "other",
    };
    let player = match &next.last_play {
        Some(lp) if lp.by_home == Some(by_home) && lp.score_value == d => {
            lp.player.clone().unwrap_or_default()
        }
        _ => String::new(),
    };
    Play {
        action,
        result: "made",
        by_home: Some(by_home),
        player,
        home_score_before: home_before,
        away_score_before: away_before,
    }
}

/// Classify a last-play `kind` into a non-scoring play (rules 4 of
/// [`plays_between`]). Comparison is case-insensitive.
fn classify_last_play(kind: &str) -> (&'static str, &'static str) {
    let k = kind.to_lowercase();
    if k.contains("rebound") {
        ("rebound", "none")
    } else if k.contains("turnover")
        || k.contains("traveling")
        || k.contains("bad pass")
        || k.contains("lost ball")
    {
        ("turnover", "none")
    } else if k.contains("foul") {
        ("foul", "none")
    } else if k.contains("free throw") {
        ("free_throw", "missed")
    } else if k.contains("three point") {
        ("three_pointer", "missed")
    } else if k.contains("shot")
        || k.contains("jumper")
        || k.contains("layup")
        || k.contains("dunk")
    {
        ("two_pointer", "missed")
    } else {
        ("other", "none")
    }
}

/// What happened between two readings of the same game, oldest first. `prev`
/// is `None` on the first reading. See the rules on the module's task brief.
pub fn plays_between(prev: Option<&GameState>, next: &GameState) -> Vec<Play> {
    let mut out = Vec::new();

    // Rule 1: first reading.
    let Some(prev) = prev else {
        if next.phase == GamePhase::Final {
            out.push(Play {
                action: "final",
                result: "none",
                by_home: None,
                player: String::new(),
                home_score_before: next.home_score,
                away_score_before: next.away_score,
            });
        }
        return out;
    };

    // Rule 2: tipoff on Pregame -> (InPlay | Final).
    if prev.phase == GamePhase::Pregame
        && matches!(next.phase, GamePhase::InPlay | GamePhase::Final)
    {
        out.push(Play {
            action: "tipoff",
            result: "none",
            by_home: None,
            player: String::new(),
            home_score_before: prev.home_score,
            away_score_before: prev.away_score,
        });
    }

    // Rule 3 (home before away) and Rule 6 (a decrease yields no play). The
    // befores chain: the away play sees home's new score and away's old.
    let mut scored = false;
    if let Some(d) = next.home_score.checked_sub(prev.home_score)
        && d > 0
    {
        scored = true;
        out.push(scoring_play(true, d, next, prev.home_score, prev.away_score));
    }
    if let Some(d) = next.away_score.checked_sub(prev.away_score)
        && d > 0
    {
        scored = true;
        out.push(scoring_play(false, d, next, next.home_score, prev.away_score));
    }

    // Rule 4: no scoring play, but a new last play adds detail.
    if !scored
        && let Some(lp) = &next.last_play
    {
        let is_new = match &prev.last_play {
            Some(plp) => plp.id != lp.id,
            None => true,
        };
        if is_new {
            let (action, result) = classify_last_play(&lp.kind);
            out.push(Play {
                action,
                result,
                by_home: lp.by_home,
                player: lp.player.clone().unwrap_or_default(),
                home_score_before: next.home_score,
                away_score_before: next.away_score,
            });
        }
    }

    // Rule 5: final last.
    if prev.phase != GamePhase::Final && next.phase == GamePhase::Final {
        out.push(Play {
            action: "final",
            result: "none",
            by_home: None,
            player: String::new(),
            home_score_before: next.home_score,
            away_score_before: next.away_score,
        });
    }

    out
}

/// One GET of the scoreboard (10 second timeout). Not called in tests.
pub async fn fetch_scoreboard(client: &reqwest::Client) -> Result<Vec<GameState>, String> {
    get_scoreboard(client, SCOREBOARD_URL.to_owned()).await
}

/// One GET of the scoreboard for one day, `YYYYMMDD` in New York: without
/// a day the scoreboard shows only the slate it considers current.
pub async fn fetch_scoreboard_on(client: &reqwest::Client, day: &str) -> Result<Vec<GameState>, String> {
    if day.len() != 8 || !day.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("not a scoreboard day: {day:?}"));
    }
    get_scoreboard(client, format!("{SCOREBOARD_URL}?dates={day}")).await
}

async fn get_scoreboard(client: &reqwest::Client, url: String) -> Result<Vec<GameState>, String> {
    let resp = client.get(url).timeout(std::time::Duration::from_secs(10)).send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("the scoreboard answered {}", resp.status()));
    }
    let body = resp.text().await.map_err(|e| e.to_string())?;
    parse_scoreboard(&body)
}

#[cfg(test)]
#[path = "espn_tests.rs"]
mod tests;
