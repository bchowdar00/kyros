//! Metered, authenticated SportsDataIO NBA capture (gate SDIO-1).
//!
//! Capture-only: each recorded response becomes a per-operation EVIDENCE event
//! carrying identity, status, size and digest, and the provider payload is kept
//! raw. Nothing here defines a signal or extracts a provider field.
//!
//! Three properties separate this module from every other provider in the crate
//! and shape most of its API: the subscription key rides one header, so no URL
//! built here may carry a query string; the request list is operator-supplied
//! but closed, compiled from a control file against the `Operation` allowlist,
//! so an unknown operation is a refusal rather than a passthrough; and every
//! request is metered against an explicit request quota and response-byte
//! budget, so a finite trial credential ends a run cleanly instead of hitting a
//! wall of 429s.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use chrono::NaiveDate;
use reqwest::header::HeaderMap;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use signals_core::SourceEvent;
use thiserror::Error;

use crate::adapter::{GuardAction, RecordGuard};
use crate::sportsdataio_auth::{SportsDataIoCredential, redact_secret};
use crate::{AdapterError, FeedAdapter, FeedNotice, FeedNoticeCode, IngestOutcome, RawRecord};

// --- Compiled SportsDataIO NBA capture constants and the closed operation
// allowlist. `pub(crate)` where the capture matrix loop and the replay adapter
// must share one source of truth; `pub` where the gate's tests and the control
// file are the consumers.
//
// This is a CAPTURE-ONLY candidate slice (gate SDIO-1). Nothing here defines a
// signal, extracts a provider field, or gives SportsDataIO a production source
// role: the adapter turns each recorded response into a per-operation EVIDENCE
// event (identity, status, size, digest) and leaves the provider payload raw.
// Gate SDIO-2 compares those raw bodies against an independent authoritative
// record; that comparison is deliberately not code in this slice.
//
// Three properties separate this stream from every other provider in the crate:
//
//   1. IT IS AUTHENTICATED. The subscription key rides one header and nothing
//      else -- see `sportsdataio_auth`. No URL this module builds may EVER carry
//      a query string, because a query string is where a `?key=` would hide;
//      `request_url` refuses one structurally rather than by convention.
//   2. THE REQUEST LIST IS OPERATOR-SUPPLIED BUT CLOSED. The control file names
//      operations from the compiled `Operation` allowlist and the parameters they
//      take; it can never name a URL. An unknown operation is a refusal, not a
//      passthrough.
//   3. EVERY REQUEST IS METERED. A trial credential has a finite quota, so the
//      run carries an explicit request budget and ends CLEANLY when it is spent,
//      rather than discovering the limit as a wall of 429s.
//
// The endpoint set is the SDIO-2 minimum matrix (gate lines 122-141) resolved
// against the Aug. 11 audit's `sportsdataio-nba-audit/ENDPOINTS.tsv` column 7
// (`sanitized_url`); each operation carries that file's documented call interval
// as evidence, NOT as a cadence this capture-only slice is entitled to poll at.

/// The only host and path prefix this module may ever request. Enforced as a
/// structural check on the built URL, not as a naming convention.
pub const API_ROOT: &str = "https://api.sportsdata.io/v3/nba/";

/// Series prefix for the normalization analyzer: one series per operation.
pub(crate) const SERIES_PREFIX: &str = "sportsdataio_nba";

/// Grouping label for a recorded response whose envelope does not parse. Like the
/// ESPN and Chainlink precedents, an odd body groups here instead of aborting a
/// normalization run that replay of the same journal completes.
pub(crate) const UNKNOWN_SERIES: &str = "sportsdataio_nba:unknown";

const SOURCE_ID_PREFIX: &str = "nba.sportsdataio.";
const SOURCE_VERSION: &str = "1.0.0";

/// Per-request HTTP timeout. Sized for the largest matrix body (a full-date box
/// score), matching the ESPN daily/game-day crawls rather than slice 1's 10 s.
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// Matrix cycle cadence. The matrix is a BOUNDED FIDELITY SWEEP over completed
/// games, not a live poll: 300 s re-executes it slowly enough that a multi-cycle
/// run stays far inside any documented interval in the table below, and the
/// request budget -- not the cadence -- is what actually bounds a run.
pub(crate) const POLL_CADENCE: Duration = Duration::from_secs(300);

/// Delay between consecutive requests inside one cycle (~4 requests/second).
pub(crate) const REQUEST_PACING: Duration = Duration::from_millis(250);

/// Per-response body cap for a 2xx response. Matches the crate-wide message cap.
pub(crate) const MAX_RESPONSE_BYTES: usize = crate::MAX_MESSAGE_BYTES;

/// Per-response body cap for a non-2xx response. A provider error body is status
/// EVIDENCE, not data: enough is kept to identify the refusal, no more.
pub(crate) const MAX_ERROR_BODY_BYTES: usize = 4 * 1024;

/// Aggregate response-byte budget for one capture run, enforced across cycles on
/// top of the recorder's own `max_bytes`. Reaching it ends the run cleanly.
pub(crate) const MAX_RUN_RESPONSE_BYTES: u64 = 512 * 1024 * 1024;

/// Default request budget when the control file does not name one. Deliberately
/// conservative against an unverified trial quota (gate SDIO-0 is unexecuted, so
/// the real daily limit is unknown; assuming a large one is exactly the mistake).
pub const DEFAULT_MAX_REQUESTS: u32 = 200;

/// Ceiling on the request budget a control file may ask for.
pub const MAXIMUM_MAX_REQUESTS: u32 = 5_000;

/// Longest `Retry-After` this capture will honour before treating the wait as a
/// cycle failure instead. Bounds a provider-controlled sleep.
pub(crate) const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);

/// Bounded control-file size. The file names operations and parameters only.
pub const MAX_CONTROL_FILE_BYTES: u64 = 64 * 1024;

// Fail-closed bounds on what one control file may request.
const MAX_OPERATIONS: usize = 32;
const MAX_DATES: usize = 31;
const MAX_GAME_IDS: usize = 64;
const MAX_DELTA_MINUTES: usize = 8;
/// Longest look-back one delta window may request: one day.
const MAX_DELTA_WINDOW_MINUTES: u32 = 1_440;
const MAX_GAME_ID: u64 = 99_999_999;
const MIN_SEASON_YEAR: u32 = 1946;
const MAX_SEASON_YEAR: u32 = 2100;
/// Longest operator-supplied value one refusal message may echo. A longer value is
/// reported by length only; see `bounded_name`.
const MAX_ECHOED_NAME_BYTES: usize = 64;
/// Season-type suffixes observed in the audit's parameter documentation
/// ("Examples: 2018, 2018PRE, 2018POST, 2018STAR, 2019"). Nothing else is legal.
const SEASON_SUFFIXES: [&str; 3] = ["PRE", "POST", "STAR"];

/// Fail-closed bound on the adapter's per-entry change-detection map.
const MAX_TRACKED_ENTRIES: usize = 1_024;

/// Bound on one recorded response-header value.
const MAX_HEADER_VALUE_BYTES: usize = 256;

/// The closed SportsDataIO NBA operation allowlist: the SDIO-2 minimum matrix and
/// nothing else. A control file selects from these names; it can never supply a
/// URL, a host, a path or a query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Operation {
    /// Unscrambled control: active teams.
    Teams,
    /// Unscrambled control: all teams, including historical.
    AllTeams,
    /// Unscrambled control: active players, basic profile.
    PlayersActiveBasic,
    /// Unscrambled control: the season schedule.
    Games,
    /// Game state: completed games for one date.
    GamesByDateFinal,
    /// Game state / statistics: one final box score.
    BoxScoreFinal,
    /// Game state / statistics: all final box scores for one date.
    BoxScoresFinal,
    /// Delta: box scores changed in the last N minutes for one date.
    BoxScoresDelta,
    /// Pregame market: game odds snapshot for one date.
    GameOddsByDate,
    /// Pregame market: line movement for one game.
    GameOddsLineMovement,
    /// Projection: player game projections for one date.
    PlayerGameProjectionStatsByDate,
    /// Availability: league injured players.
    InjuredPlayers,
    /// Availability: starting lineups for one date.
    StartingLineupsByDate,
}

/// Every operation in control-file order. Adding a variant forces this list.
pub const OPERATIONS: [Operation; 13] = [
    Operation::Teams,
    Operation::AllTeams,
    Operation::PlayersActiveBasic,
    Operation::Games,
    Operation::GamesByDateFinal,
    Operation::BoxScoreFinal,
    Operation::BoxScoresFinal,
    Operation::BoxScoresDelta,
    Operation::GameOddsByDate,
    Operation::GameOddsLineMovement,
    Operation::PlayerGameProjectionStatsByDate,
    Operation::InjuredPlayers,
    Operation::StartingLineupsByDate,
];

/// Which parameter axis an operation expands over in the request matrix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParameterKind {
    /// No path parameter: exactly one request.
    None,
    /// One request for the control file's `season`.
    Season,
    /// One request per control-file `dates` entry.
    Date,
    /// One request per control-file `game_ids` entry.
    GameId,
    /// One request per (`dates`, `delta_minutes`) pair.
    DateMinutes,
}

impl Operation {
    /// The control-file name and the series/source-id suffix. One authority.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Teams => "teams",
            Self::AllTeams => "all_teams",
            Self::PlayersActiveBasic => "players_active_basic",
            Self::Games => "games",
            Self::GamesByDateFinal => "games_by_date_final",
            Self::BoxScoreFinal => "box_score_final",
            Self::BoxScoresFinal => "box_scores_final",
            Self::BoxScoresDelta => "box_scores_delta",
            Self::GameOddsByDate => "game_odds_by_date",
            Self::GameOddsLineMovement => "game_odds_line_movement",
            Self::PlayerGameProjectionStatsByDate => "player_game_projection_stats_by_date",
            Self::InjuredPlayers => "injured_players",
            Self::StartingLineupsByDate => "starting_lineups_by_date",
        }
    }

    /// The path template BELOW `API_ROOT`, exactly as the Aug. 11 audit recorded
    /// it in `ENDPOINTS.tsv` column 7. Placeholders are substituted only with
    /// values that passed control-file validation.
    pub const fn path_template(self) -> &'static str {
        match self {
            Self::Teams => "scores/json/teams",
            Self::AllTeams => "scores/json/AllTeams",
            Self::PlayersActiveBasic => "scores/json/PlayersActiveBasic",
            Self::Games => "scores/json/Games/{season}",
            Self::GamesByDateFinal => "scores/json/GamesByDateFinal/{date}",
            Self::BoxScoreFinal => "stats/json/BoxScoreFinal/{gameid}",
            Self::BoxScoresFinal => "stats/json/BoxScoresFinal/{date}",
            Self::BoxScoresDelta => "stats/json/BoxScoresDelta/{date}/{minutes}",
            Self::GameOddsByDate => "odds/json/GameOddsByDate/{date}",
            Self::GameOddsLineMovement => "odds/json/GameOddsLineMovement/{gameid}",
            Self::PlayerGameProjectionStatsByDate => {
                "projections/json/PlayerGameProjectionStatsByDate/{date}"
            }
            Self::InjuredPlayers => "projections/json/InjuredPlayers",
            Self::StartingLineupsByDate => "projections/json/StartingLineupsByDate/{date}",
        }
    }

    /// Which parameter axis this operation expands over when the matrix is
    /// compiled, and therefore which control-file list supplies its values.
    pub const fn parameter_kind(self) -> ParameterKind {
        match self {
            Self::Teams | Self::AllTeams | Self::PlayersActiveBasic | Self::InjuredPlayers => {
                ParameterKind::None
            }
            Self::Games => ParameterKind::Season,
            Self::GamesByDateFinal
            | Self::BoxScoresFinal
            | Self::GameOddsByDate
            | Self::PlayerGameProjectionStatsByDate
            | Self::StartingLineupsByDate => ParameterKind::Date,
            Self::BoxScoreFinal | Self::GameOddsLineMovement => ParameterKind::GameId,
            Self::BoxScoresDelta => ParameterKind::DateMinutes,
        }
    }

    /// The provider's own documented call interval for this operation, copied
    /// verbatim from `ENDPOINTS.tsv` column 8. Recorded as EVIDENCE of the
    /// provider's published cadence; this capture-only slice never polls at it.
    pub const fn documented_interval(self) -> &'static str {
        match self {
            Self::Teams | Self::AllTeams => "4 Hours",
            Self::PlayersActiveBasic | Self::GameOddsLineMovement => "15 Minutes",
            Self::Games | Self::PlayerGameProjectionStatsByDate => "5 Minutes",
            Self::GamesByDateFinal => "5 Seconds",
            Self::BoxScoreFinal | Self::BoxScoresFinal | Self::InjuredPlayers => "1 Minute",
            Self::BoxScoresDelta => "3 Seconds",
            Self::GameOddsByDate => "30 Seconds",
            Self::StartingLineupsByDate => "3 Minutes",
        }
    }

    /// Resolve a control-file operation name. Unknown names are a REFUSAL.
    pub fn from_name(name: &str) -> Option<Self> {
        OPERATIONS
            .into_iter()
            .find(|operation| operation.as_str() == name)
    }
}

/// The validated parameters one matrix entry substitutes into its template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestParameters {
    /// No substitution: the template has no placeholder.
    None,
    /// Substitutes `{season}` with a validated control-file season string.
    Season(String),
    /// Substitutes `{date}` with one validated control-file date.
    Date(String),
    /// Substitutes `{gameid}` with one validated control-file game id.
    GameId(u64),
    /// Substitutes `{date}` and `{minutes}` for a delta operation.
    DateMinutes {
        /// The validated control-file date.
        date: String,
        /// The delta lookback in minutes, from the control file.
        minutes: u32,
    },
}

/// One request the matrix will issue, once per cycle, in matrix order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatrixEntry {
    /// Which allowlisted operation this entry requests.
    pub operation: Operation,
    /// The validated values substituted into that operation's path template.
    pub parameters: RequestParameters,
}

/// The ordered request matrix compiled from one control file, plus the request
/// budget the run may spend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestMatrix {
    entries: Vec<MatrixEntry>,
    max_requests: u32,
}

impl RequestMatrix {
    /// The compiled entries in control-file order, which is also the order one
    /// cycle issues them in.
    pub fn entries(&self) -> &[MatrixEntry] {
        &self.entries
    }

    /// The run's request budget: the maximum number of requests the control
    /// file permits across all cycles, in requests.
    pub const fn max_requests(&self) -> u32 {
        self.max_requests
    }

    /// Number of entries, which is the number of requests one full cycle
    /// issues.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when the matrix compiled to no entries; a validated control file
    /// never produces one.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Parse and validate one control file's JSON text.
    ///
    /// The control file is the ONLY operator input on this path, so validation is
    /// strict in both directions: an operation whose parameter list is missing is
    /// a refusal, and a parameter list no selected operation consumes is ALSO a
    /// refusal (a silently ignored date is how a fidelity run ends up sampling a
    /// day nobody meant to sample).
    pub fn from_control_json(text: &str) -> Result<Self, SportsDataIoNbaError> {
        let control: ControlFile = serde_json::from_str(text)
            .map_err(|error| SportsDataIoNbaError::ControlFileJson(error.to_string()))?;

        if control.operations.is_empty() || control.operations.len() > MAX_OPERATIONS {
            return Err(SportsDataIoNbaError::InvalidControl(format!(
                "operations must name 1 to {MAX_OPERATIONS} operations"
            )));
        }
        let mut operations = Vec::with_capacity(control.operations.len());
        let mut seen = BTreeSet::new();
        for name in &control.operations {
            let operation = Operation::from_name(name)
                .ok_or_else(|| SportsDataIoNbaError::UnknownOperation(bounded_name(name)))?;
            if !seen.insert(operation) {
                return Err(SportsDataIoNbaError::InvalidControl(format!(
                    "operation {} is listed more than once",
                    operation.as_str()
                )));
            }
            operations.push(operation);
        }

        let dates = validated_dates(&control.dates)?;
        let game_ids = validated_game_ids(&control.game_ids)?;
        let delta_minutes = validated_delta_minutes(&control.delta_minutes)?;
        let season = match control.season.as_deref() {
            Some(season) => Some(validated_season(season)?),
            None => None,
        };

        let needs = |kind: ParameterKind| {
            operations
                .iter()
                .any(|operation| operation.parameter_kind() == kind)
        };
        let needs_date = needs(ParameterKind::Date) || needs(ParameterKind::DateMinutes);
        let needs_minutes = needs(ParameterKind::DateMinutes);
        let needs_game_id = needs(ParameterKind::GameId);
        let needs_season = needs(ParameterKind::Season);

        require(needs_season, season.is_some(), "season")?;
        require(needs_date, !dates.is_empty(), "dates")?;
        require(needs_game_id, !game_ids.is_empty(), "game_ids")?;
        require(needs_minutes, !delta_minutes.is_empty(), "delta_minutes")?;

        let mut entries = Vec::new();
        for operation in operations {
            match operation.parameter_kind() {
                ParameterKind::None => entries.push(MatrixEntry {
                    operation,
                    parameters: RequestParameters::None,
                }),
                ParameterKind::Season => entries.push(MatrixEntry {
                    operation,
                    parameters: RequestParameters::Season(
                        season.clone().expect("season presence was required above"),
                    ),
                }),
                ParameterKind::Date => entries.extend(dates.iter().map(|date| MatrixEntry {
                    operation,
                    parameters: RequestParameters::Date(date.clone()),
                })),
                ParameterKind::GameId => {
                    entries.extend(game_ids.iter().map(|game_id| MatrixEntry {
                        operation,
                        parameters: RequestParameters::GameId(*game_id),
                    }));
                }
                ParameterKind::DateMinutes => {
                    for date in &dates {
                        for minutes in &delta_minutes {
                            entries.push(MatrixEntry {
                                operation,
                                parameters: RequestParameters::DateMinutes {
                                    date: date.clone(),
                                    minutes: *minutes,
                                },
                            });
                        }
                    }
                }
            }
        }

        let max_requests = control.max_requests.unwrap_or(DEFAULT_MAX_REQUESTS);
        if max_requests == 0 || max_requests > MAXIMUM_MAX_REQUESTS {
            return Err(SportsDataIoNbaError::InvalidControl(format!(
                "max_requests must be between 1 and {MAXIMUM_MAX_REQUESTS}"
            )));
        }
        let requested = u32::try_from(entries.len()).map_err(|_| {
            SportsDataIoNbaError::InvalidControl("request matrix is too large".to_owned())
        })?;
        if requested > max_requests {
            return Err(SportsDataIoNbaError::QuotaExceeded {
                requested,
                maximum: max_requests,
            });
        }
        // Every entry must produce a legal allowlisted URL before the matrix is
        // accepted: a refusal belongs at load time, never mid-capture.
        for entry in &entries {
            request_url(entry.operation, &entry.parameters)?;
        }
        Ok(Self {
            entries,
            max_requests,
        })
    }
}

/// Read and validate one control file, refusing anything larger than
/// `MAX_CONTROL_FILE_BYTES`. The file never contains a credential.
pub fn read_control_file(path: &Path) -> Result<RequestMatrix, SportsDataIoNbaError> {
    let metadata =
        std::fs::metadata(path).map_err(|_| SportsDataIoNbaError::ControlFileUnreadable)?;
    if metadata.len() > MAX_CONTROL_FILE_BYTES {
        return Err(SportsDataIoNbaError::ControlFileTooLarge {
            maximum: MAX_CONTROL_FILE_BYTES,
        });
    }
    let text =
        std::fs::read_to_string(path).map_err(|_| SportsDataIoNbaError::ControlFileUnreadable)?;
    RequestMatrix::from_control_json(&text)
}

/// The control file's exact accepted shape. `deny_unknown_fields` so a typo is a
/// refusal rather than a silently dropped instruction.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ControlFile {
    #[serde(default)]
    season: Option<String>,
    #[serde(default)]
    dates: Vec<String>,
    #[serde(default)]
    game_ids: Vec<u64>,
    #[serde(default)]
    delta_minutes: Vec<u32>,
    operations: Vec<String>,
    #[serde(default)]
    max_requests: Option<u32>,
}

fn require(needed: bool, present: bool, field: &str) -> Result<(), SportsDataIoNbaError> {
    match (needed, present) {
        (true, false) => Err(SportsDataIoNbaError::InvalidControl(format!(
            "{field} is required by a selected operation but is missing or empty"
        ))),
        (false, true) => Err(SportsDataIoNbaError::InvalidControl(format!(
            "{field} is supplied but no selected operation consumes it"
        ))),
        _ => Ok(()),
    }
}

/// One strict `YYYY-MM-DD` calendar date. `2026-1-5` and `2026-02-30` are
/// refusals; the round trip through `NaiveDate` rejects both padding and calendar
/// errors. This is the single authority for "is a legal date parameter", used by
/// the control-file loader and again by `request_url`.
fn validated_date(date: &str) -> Result<String, SportsDataIoNbaError> {
    let parsed = NaiveDate::parse_from_str(date, "%Y-%m-%d").map_err(|_| {
        SportsDataIoNbaError::InvalidControl(format!(
            "date {} is not a YYYY-MM-DD calendar date",
            bounded_name(date)
        ))
    })?;
    if parsed.format("%Y-%m-%d").to_string() != *date {
        return Err(SportsDataIoNbaError::InvalidControl(format!(
            "date {} is not zero-padded YYYY-MM-DD",
            bounded_name(date)
        )));
    }
    Ok(date.to_owned())
}

fn validated_dates(dates: &[String]) -> Result<Vec<String>, SportsDataIoNbaError> {
    if dates.len() > MAX_DATES {
        return Err(SportsDataIoNbaError::InvalidControl(format!(
            "dates must name at most {MAX_DATES} days"
        )));
    }
    let mut seen = BTreeSet::new();
    let mut validated = Vec::with_capacity(dates.len());
    for date in dates {
        let date = validated_date(date)?;
        if !seen.insert(date.clone()) {
            return Err(SportsDataIoNbaError::InvalidControl(format!(
                "date {} is listed more than once",
                bounded_name(&date)
            )));
        }
        validated.push(date);
    }
    Ok(validated)
}

/// One game id inside the observed identifier band. The single authority, used by
/// the control-file loader and again by `request_url`.
fn validated_game_id(game_id: u64) -> Result<u64, SportsDataIoNbaError> {
    if game_id == 0 || game_id > MAX_GAME_ID {
        return Err(SportsDataIoNbaError::InvalidControl(format!(
            "game id must be between 1 and {MAX_GAME_ID}"
        )));
    }
    Ok(game_id)
}

fn validated_game_ids(game_ids: &[u64]) -> Result<Vec<u64>, SportsDataIoNbaError> {
    if game_ids.len() > MAX_GAME_IDS {
        return Err(SportsDataIoNbaError::InvalidControl(format!(
            "game_ids must name at most {MAX_GAME_IDS} games"
        )));
    }
    let mut seen = BTreeSet::new();
    for game_id in game_ids {
        validated_game_id(*game_id)?;
        if !seen.insert(*game_id) {
            return Err(SportsDataIoNbaError::InvalidControl(
                "a game id is listed more than once".to_owned(),
            ));
        }
    }
    Ok(game_ids.to_vec())
}

/// One delta window, in minutes. The single authority, used by the control-file
/// loader and again by `request_url`.
fn validated_minutes(window: u32) -> Result<u32, SportsDataIoNbaError> {
    if window == 0 || window > MAX_DELTA_WINDOW_MINUTES {
        return Err(SportsDataIoNbaError::InvalidControl(format!(
            "delta_minutes entries must be between 1 and {MAX_DELTA_WINDOW_MINUTES}"
        )));
    }
    Ok(window)
}

fn validated_delta_minutes(minutes: &[u32]) -> Result<Vec<u32>, SportsDataIoNbaError> {
    if minutes.len() > MAX_DELTA_MINUTES {
        return Err(SportsDataIoNbaError::InvalidControl(format!(
            "delta_minutes must name at most {MAX_DELTA_MINUTES} windows"
        )));
    }
    let mut seen = BTreeSet::new();
    for window in minutes {
        validated_minutes(*window)?;
        if !seen.insert(*window) {
            return Err(SportsDataIoNbaError::InvalidControl(
                "a delta_minutes window is listed more than once".to_owned(),
            ));
        }
    }
    Ok(minutes.to_vec())
}

/// Season strings in the forms the audit documented and nothing else: four
/// digits, optionally followed by `PRE`, `POST` or `STAR`.
fn validated_season(season: &str) -> Result<String, SportsDataIoNbaError> {
    let invalid = || {
        SportsDataIoNbaError::InvalidControl(format!(
            "season {} is not one of the observed forms YYYY, YYYYPRE, YYYYPOST, YYYYSTAR",
            bounded_name(season)
        ))
    };
    // Byte length is not character length: `202é` is five bytes with no character
    // boundary at four, and `split_at(4)` would PANIC on it. `get` refuses instead,
    // so an operator typo is the documented refusal rather than a crash.
    let (Some(year), Some(suffix)) = (season.get(..4), season.get(4..)) else {
        return Err(invalid());
    };
    if !year.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid());
    }
    let year: u32 = year.parse().map_err(|_| invalid())?;
    if !(MIN_SEASON_YEAR..=MAX_SEASON_YEAR).contains(&year) {
        return Err(invalid());
    }
    if !suffix.is_empty() && !SEASON_SUFFIXES.contains(&suffix) {
        return Err(invalid());
    }
    Ok(season.to_owned())
}

/// Render an operator-supplied name for one refusal message: VERBATIM, or not at
/// all. Operator input is not secret, but an unbounded echo is still a hazard, and
/// a pasted credential is exactly the value an operator should never see echoed.
///
/// Verbatim-or-nothing is what makes the capture path's exact-match credential
/// redactor complete over this echo: a truncated or character-filtered rendering
/// could print a PREFIX of a pasted key, which the redactor would then fail to
/// recognize. Anything longer than the bound, or not printable ASCII, is therefore
/// reported by byte length only.
fn bounded_name(name: &str) -> String {
    if name.len() <= MAX_ECHOED_NAME_BYTES && name.bytes().all(|byte| byte.is_ascii_graphic()) {
        name.to_owned()
    } else {
        format!("<{} bytes, not echoed>", name.len())
    }
}

/// Build the one legal URL for a matrix entry.
///
/// Every URL is `API_ROOT` plus a compiled template with validated substitutions;
/// the result is then re-checked STRUCTURALLY: it must start with `API_ROOT`, be
/// entirely ASCII-graphic, carry no `?` or `#` (so no query string and therefore
/// no `?key=` can ever exist on this path), leave no placeholder unsubstituted,
/// and contain no empty path segment.
///
/// Defence in depth means VALUE checks as well as structural ones, and both run
/// here rather than only at control-file load. `validated_segment` alone accepts
/// any short alphanumeric-or-dash run -- which a 32-character hexadecimal
/// subscription key satisfies -- so a future caller that built a matrix entry
/// without going through `RequestMatrix::from_control_json` could put a value in
/// a path that the load-time validators would have refused. Re-running
/// `validated_date` and `validated_season` on the same values makes this layer
/// independently sufficient: the only accepted date is a real calendar day and
/// the only accepted season is one of the four observed forms.
pub fn request_url(
    operation: Operation,
    parameters: &RequestParameters,
) -> Result<String, SportsDataIoNbaError> {
    let template = operation.path_template();
    let path = match (operation.parameter_kind(), parameters) {
        (ParameterKind::None, RequestParameters::None) => template.to_owned(),
        (ParameterKind::Season, RequestParameters::Season(season)) => {
            // Structural check first, so a smuggled separator stays the valueless
            // `UrlNotAllowlisted` refusal; then the value check.
            let segment = validated_segment(season)?;
            validated_season(segment)?;
            template.replace("{season}", segment)
        }
        (ParameterKind::Date, RequestParameters::Date(date)) => {
            let segment = validated_segment(date)?;
            validated_date(segment)?;
            template.replace("{date}", segment)
        }
        (ParameterKind::GameId, RequestParameters::GameId(game_id)) => {
            validated_game_id(*game_id)?;
            template.replace("{gameid}", &game_id.to_string())
        }
        (ParameterKind::DateMinutes, RequestParameters::DateMinutes { date, minutes }) => {
            let segment = validated_segment(date)?;
            validated_date(segment)?;
            validated_minutes(*minutes)?;
            template
                .replace("{date}", segment)
                .replace("{minutes}", &minutes.to_string())
        }
        _ => return Err(SportsDataIoNbaError::ParameterMismatch(operation.as_str())),
    };
    let url = format!("{API_ROOT}{path}");
    if !url.starts_with(API_ROOT)
        || !url.bytes().all(|byte| byte.is_ascii_graphic())
        || url.contains('?')
        || url.contains('#')
        || url.contains('{')
        || url.contains('}')
        || url[API_ROOT.len()..].split('/').any(str::is_empty)
    {
        return Err(SportsDataIoNbaError::UrlNotAllowlisted);
    }
    Ok(url)
}

/// One path-segment substitution: ASCII alphanumerics and `-` only, so no
/// separator, escape or query character can be smuggled into a path.
fn validated_segment(value: &str) -> Result<&str, SportsDataIoNbaError> {
    if value.is_empty()
        || value.len() > 32
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(SportsDataIoNbaError::UrlNotAllowlisted);
    }
    Ok(value)
}

/// Build the exact `OutboundText` descriptor recorded for one matrix request.
/// These are GETs with no body, so the descriptor IS the outbound record. The key
/// order is fixed at `{"request","operation","cycle","index"}` and the `request`
/// URL is sanitized by construction -- it can never carry a credential, because
/// `request_url` refuses a query string.
pub fn outbound_descriptor(
    request: &str,
    operation: Operation,
    cycle: u64,
    index: usize,
) -> String {
    #[derive(Serialize)]
    struct Descriptor<'a> {
        request: &'a str,
        operation: &'a str,
        cycle: u64,
        index: usize,
    }
    serde_json::to_string(&Descriptor {
        request,
        operation: operation.as_str(),
        cycle,
        index,
    })
    .expect("the compiled outbound descriptor always serializes")
}

/// How one recorded response resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseOutcome {
    /// The provider answered; `status` is its status, 2xx or not.
    Response,
    /// The provider answered with a redirect. The redirect is NOT followed and
    /// the `Location` value is never recorded -- an off-host redirect target is
    /// exactly the string a credential-bearing client must not chase or log.
    RedirectRefused,
}

impl ResponseOutcome {
    /// The stable `outcome` string written into the envelope: `response` or
    /// `redirect_refused`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Response => "response",
            Self::RedirectRefused => "redirect_refused",
        }
    }
}

/// How the provider body is carried inside the envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyEncoding {
    /// The body was valid UTF-8 and is carried verbatim.
    Utf8,
    /// The body was not UTF-8 and is carried base64-encoded; `body_sha256` is
    /// over the RAW bytes either way, so verbatim-ness stays provable.
    Base64,
}

impl BodyEncoding {
    /// The stable `body_encoding` string written into the envelope: `utf8` or
    /// `base64`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Utf8 => "utf8",
            Self::Base64 => "base64",
        }
    }
}

/// The `InboundText` payload recorded for one response.
///
/// The provider payload stays RAW in `body`; every other field is journal
/// metadata the gate requires (receipt time, status, an allowlisted header
/// subset, exact body byte count and SHA-256, endpoint identity). The
/// authentication header is not among the recorded fields and never can be:
/// only RESPONSE headers are inspected, and only through `allowlisted_headers`.
///
/// The retained header VALUES are redacted too: an allowlisted name is no promise
/// about what stands beside it, and `Retry-After` and the `x-ratelimit-*` family
/// are provider-controlled strings an echoing gateway can fill with the request it
/// received. See `allowlisted_headers`.
///
/// "Raw" has exactly one exception, and `body_redacted` names it: a provider or
/// upstream error page may echo the request headers it received, so the recorded
/// bytes pass through `SportsDataIoCredential::redact_body` first. When that flag
/// is true the recorded bytes are NOT the wire bytes, and `body_sha256` is the
/// digest of what is recorded rather than of what arrived -- which is the only
/// honest digest to publish, since the wire bytes were deliberately not kept.
///
/// The monotonic elapsed time the gate requires is carried by the `RawRecord`
/// that wraps this payload (`capture_elapsed_ns`), not by the envelope itself.
///
/// Field order is the serialized key order and is pinned by test.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponseEnvelope {
    /// The allowlisted operation's control-file name, e.g. `box_score_final`.
    pub operation: String,
    /// Zero-based cycle number: which pass over the matrix issued this request.
    pub cycle: u64,
    /// Zero-based position of the entry within the matrix for that cycle.
    pub index: usize,
    /// Wall-clock receipt time as an RFC 3339 timestamp. Monotonic elapsed time
    /// lives on the wrapping `RawRecord`, not here.
    pub received_at: String,
    /// The provider's HTTP status code, 2xx or not.
    pub status: u16,
    /// How the response resolved: `response` or `redirect_refused`.
    pub outcome: String,
    /// The allowlisted, lowercased, redacted subset of response headers. Never
    /// carries `Location`.
    pub headers: BTreeMap<String, String>,
    /// Byte length of the RECORDED body, after truncation and redaction; not
    /// necessarily the wire length.
    pub body_bytes: u64,
    /// Lowercase hex SHA-256 over those same recorded bytes, so the digest always
    /// describes this envelope's own `body`.
    pub body_sha256: String,
    /// How `body` is carried: `utf8` verbatim, or `base64` for non-UTF-8 bytes.
    pub body_encoding: String,
    /// True when the recording cap cut the body short.
    pub body_truncated: bool,
    /// True when the credential redactor rewrote the bytes because the response
    /// echoed request headers; the recorded bytes are then not the wire bytes.
    pub body_redacted: bool,
    /// The recorded provider payload, raw except for truncation and redaction.
    pub body: String,
}

/// Response headers this capture retains. Deliberately tiny, and deliberately
/// WITHOUT `Location`: a refused redirect records only that it was refused, never
/// where it pointed. Rate-limit headers are kept because gate SDIO-0 must learn
/// the real quota, and `Retry-After` because the loop honours it.
const HEADER_ALLOWLIST: [&str; 4] = ["content-type", "content-length", "retry-after", "date"];
const HEADER_ALLOWLIST_PREFIX: &str = "x-ratelimit-";

/// Filter one response's headers down to the allowlist, lowercased and bounded.
/// A value that is not short printable ASCII is dropped rather than recorded.
///
/// A retained VALUE is not trusted either. An allowlisted name is not a promise
/// about what the provider or an upstream proxy put beside it: `Retry-After` and
/// the open-ended `x-ratelimit-*` family are provider-controlled strings that an
/// echoing gateway can fill with the request it received. So every value that
/// survives the allowlist passes through the SAME redactor as the recorded body
/// before it can reach an envelope. The bound and character check run on the WIRE
/// value first, so redaction can never pull an over-cap value back under the cap.
pub fn allowlisted_headers(
    headers: &HeaderMap,
    credential: &SportsDataIoCredential,
) -> BTreeMap<String, String> {
    let mut retained = BTreeMap::new();
    for (name, value) in headers {
        let name = name.as_str().to_ascii_lowercase();
        if !HEADER_ALLOWLIST.contains(&name.as_str()) && !name.starts_with(HEADER_ALLOWLIST_PREFIX)
        {
            continue;
        }
        let Ok(value) = value.to_str() else {
            continue;
        };
        if value.len() > MAX_HEADER_VALUE_BYTES
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_graphic() || byte == b' ')
        {
            continue;
        }
        retained.insert(name, redact_secret(value, credential));
    }
    retained
}

/// Assemble the recorded envelope for one response.
///
/// `body` is exactly the bytes that will be RECORDED: the caller has already
/// applied the recording cap (`truncated`) and the credential redactor
/// (`redacted`). The digest is taken over those same bytes, so the envelope's
/// `body_sha256` always describes its own `body` and never a value the journal
/// does not carry.
#[allow(clippy::too_many_arguments)]
pub fn response_envelope(
    operation: Operation,
    cycle: u64,
    index: usize,
    received_at: &str,
    status: u16,
    outcome: ResponseOutcome,
    headers: BTreeMap<String, String>,
    body: &[u8],
    truncated: bool,
    redacted: bool,
) -> String {
    let (encoding, rendered) = match std::str::from_utf8(body) {
        Ok(text) => (BodyEncoding::Utf8, text.to_owned()),
        Err(_) => (BodyEncoding::Base64, BASE64.encode(body)),
    };
    let envelope = ResponseEnvelope {
        operation: operation.as_str().to_owned(),
        cycle,
        index,
        received_at: received_at.to_owned(),
        status,
        outcome: outcome.as_str().to_owned(),
        headers,
        body_bytes: body.len() as u64,
        body_sha256: crate::secure_http::sha256_hex(body),
        body_encoding: encoding.as_str().to_owned(),
        body_truncated: truncated,
        body_redacted: redacted,
        body: rendered,
    };
    serde_json::to_string(&envelope).expect("the compiled response envelope always serializes")
}

/// Permissive normalization series classifier. Never fails: an envelope that does
/// not parse, or names an operation outside the allowlist, groups as
/// `sportsdataio_nba:unknown` rather than aborting a normalization run.
pub(crate) fn classify_series(payload: &str) -> String {
    match serde_json::from_str::<ResponseEnvelope>(payload) {
        Ok(envelope) => match Operation::from_name(&envelope.operation) {
            Some(operation) => format!("{SERIES_PREFIX}:{}", operation.as_str()),
            None => UNKNOWN_SERIES.to_owned(),
        },
        Err(_) => UNKNOWN_SERIES.to_owned(),
    }
}

/// Explicit request budget for one capture run. A trial credential has a finite
/// quota, so the run refuses to exceed its own budget rather than discovering the
/// provider's limit as a wall of 429s. Exhaustion ends the capture CLEANLY.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestQuota {
    maximum: u32,
    used: u32,
}

impl RequestQuota {
    /// A budget of `maximum` requests with nothing charged against it.
    pub const fn new(maximum: u32) -> Self {
        Self { maximum, used: 0 }
    }

    /// Charge one request, or refuse. The charge happens BEFORE the request is
    /// sent, so a crash can never under-count what the credential was billed.
    pub fn try_consume(&mut self) -> Result<u32, SportsDataIoNbaError> {
        if self.used >= self.maximum {
            return Err(SportsDataIoNbaError::QuotaExhausted {
                maximum: self.maximum,
            });
        }
        self.used = self.used.saturating_add(1);
        Ok(self.used)
    }

    /// Requests charged against the credential so far, in requests.
    pub const fn used(self) -> u32 {
        self.used
    }

    /// Requests still available before `try_consume` refuses, in requests.
    pub const fn remaining(self) -> u32 {
        self.maximum.saturating_sub(self.used)
    }
}

/// Aggregate response-byte budget for one run, on top of the per-response cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResponseBudget {
    maximum: u64,
    used: u64,
}

impl ResponseBudget {
    /// A budget of `maximum` response bytes with nothing charged against it.
    pub const fn new(maximum: u64) -> Self {
        Self { maximum, used: 0 }
    }

    /// Charge `bytes` against the run budget, or refuse without charging.
    pub fn try_reserve(&mut self, bytes: u64) -> Result<u64, SportsDataIoNbaError> {
        let next =
            self.used
                .checked_add(bytes)
                .ok_or(SportsDataIoNbaError::ResponseBudgetExhausted {
                    maximum: self.maximum,
                })?;
        if next > self.maximum {
            return Err(SportsDataIoNbaError::ResponseBudgetExhausted {
                maximum: self.maximum,
            });
        }
        self.used = next;
        Ok(self.used)
    }

    /// Response bytes charged against the run so far.
    pub const fn used(self) -> u64 {
        self.used
    }
}

/// Deterministic SportsDataIO NBA replay adapter.
///
/// This slice is CAPTURE-ONLY, so the adapter deliberately extracts NO provider
/// field: it turns each recorded response into one per-operation EVIDENCE event
/// (operation identity, endpoint template, status, outcome, exact body size and
/// digest, payload shape) and leaves the raw body untouched in the journal for
/// gate SDIO-2 to compare against an independent authoritative record.
///
/// Odd input is never session-fatal (the Chainlink/ESPN precedent): an envelope
/// that does not parse, or names an operation outside the allowlist, produces a
/// notice and no event, and replay of the same journal completes.
pub struct SportsDataIoNbaAdapter {
    publisher_epoch: String,
    guard: RecordGuard,
    next_sequence: u64,
    /// Last body digest per matrix entry, per connection epoch. The matrix is
    /// fixed and ordered for a whole run, so `index` identifies the entry's
    /// parameters and `(operation, index)` is a stable per-entry key.
    last_digest: BTreeMap<String, String>,
}

impl SportsDataIoNbaAdapter {
    /// Build a replay adapter labelled with one publisher epoch. The epoch must
    /// be 1 to 128 bytes and not blank; anything else is `InvalidRecord`.
    pub fn new(publisher_epoch: impl Into<String>) -> Result<Self, AdapterError> {
        let publisher_epoch = publisher_epoch.into();
        if publisher_epoch.trim().is_empty() || publisher_epoch.len() > 128 {
            return Err(AdapterError::InvalidRecord(
                "publisher_epoch must contain 1 to 128 bytes".to_owned(),
            ));
        }
        Ok(Self {
            publisher_epoch,
            guard: RecordGuard::new(crate::Provider::SportsDataIo),
            next_sequence: 0,
            last_digest: BTreeMap::new(),
        })
    }

    fn ingest_payload(
        &mut self,
        record: &RawRecord,
        payload: &str,
    ) -> Result<IngestOutcome, AdapterError> {
        let envelope: ResponseEnvelope = match serde_json::from_str(payload) {
            Ok(envelope) => envelope,
            Err(error) => {
                return Ok(IngestOutcome::notice(FeedNotice::info(
                    FeedNoticeCode::UnsupportedMessage,
                    bounded_detail(
                        "SportsDataIO response envelope is not parseable",
                        &error.to_string(),
                    ),
                )));
            }
        };
        let Some(operation) = Operation::from_name(&envelope.operation) else {
            return Ok(IngestOutcome::notice(FeedNotice::info(
                FeedNoticeCode::UnsupportedMessage,
                bounded_detail(
                    "SportsDataIO envelope names an operation outside the allowlist",
                    &bounded_name(&envelope.operation),
                ),
            )));
        };

        let mut outcome = IngestOutcome::default();
        // The envelope's own receipt time is the gate's required receipt basis; a
        // malformed one falls back to the record's receipt time with a notice
        // rather than dropping the evidence row.
        let event_time = match chrono::DateTime::parse_from_rfc3339(&envelope.received_at) {
            Ok(_) => envelope.received_at.clone(),
            Err(_) => {
                outcome.notices.push(FeedNotice::info(
                    FeedNoticeCode::StateInconsistent,
                    "SportsDataIO envelope receipt time is not RFC3339; using the record receipt"
                        .to_owned(),
                ));
                record.received_at.clone()
            }
        };
        let successful = (200..300).contains(&envelope.status);
        if !successful {
            outcome.notices.push(FeedNotice::info(
                FeedNoticeCode::VenueStatus,
                format!(
                    "SportsDataIO {} returned status {} ({})",
                    operation.as_str(),
                    envelope.status,
                    envelope.outcome
                ),
            ));
        }

        let (payload_shape, payload_items) = if successful && envelope.body_encoding == "utf8" {
            match serde_json::from_str::<Value>(&envelope.body) {
                Ok(Value::Array(items)) => ("array", Some(items.len() as u64)),
                Ok(Value::Object(_)) => ("object", None),
                Ok(_) => ("scalar", None),
                Err(_) => ("malformed", None),
            }
        } else if successful {
            ("binary", None)
        } else {
            ("status_evidence", None)
        };
        if payload_shape == "malformed" {
            outcome.notices.push(FeedNotice::info(
                FeedNoticeCode::UnsupportedMessage,
                format!(
                    "SportsDataIO {} returned a 2xx body that is not JSON",
                    operation.as_str()
                ),
            ));
        }

        let entry_key = format!("{}#{}", operation.as_str(), envelope.index);
        let body_changed = self
            .last_digest
            .get(&entry_key)
            .is_none_or(|previous| previous != &envelope.body_sha256);
        if !self.last_digest.contains_key(&entry_key)
            && self.last_digest.len() >= MAX_TRACKED_ENTRIES
        {
            return Err(AdapterError::InvalidPayload(format!(
                "SportsDataIO tracked-entry count exceeded {MAX_TRACKED_ENTRIES}"
            )));
        }

        let mut values = BTreeMap::from([
            ("operation".to_owned(), json!(operation.as_str())),
            ("endpoint".to_owned(), json!(operation.path_template())),
            (
                "documented_interval".to_owned(),
                json!(operation.documented_interval()),
            ),
            ("cycle".to_owned(), json!(envelope.cycle)),
            ("matrix_index".to_owned(), json!(envelope.index)),
            ("status".to_owned(), json!(envelope.status)),
            ("outcome".to_owned(), json!(envelope.outcome)),
            ("body_bytes".to_owned(), json!(envelope.body_bytes)),
            ("body_sha256".to_owned(), json!(envelope.body_sha256)),
            ("body_encoding".to_owned(), json!(envelope.body_encoding)),
            ("body_truncated".to_owned(), json!(envelope.body_truncated)),
            ("body_redacted".to_owned(), json!(envelope.body_redacted)),
            ("body_changed".to_owned(), json!(body_changed)),
            ("payload_shape".to_owned(), json!(payload_shape)),
            ("event_time_basis".to_owned(), json!("receipt")),
            ("event_time".to_owned(), json!(event_time)),
        ]);
        if let Some(items) = payload_items {
            values.insert("payload_items".to_owned(), json!(items));
        }
        for (name, value) in &envelope.headers {
            values.insert(format!("header_{}", name.replace('-', "_")), json!(value));
        }

        let source_id = format!("{SOURCE_ID_PREFIX}{}", operation.as_str());
        let event = self.source_event(record, &source_id, event_time, values)?;
        self.last_digest.insert(entry_key, envelope.body_sha256);
        outcome.events.push(event);
        Ok(outcome)
    }

    /// Assemble one `SourceEvent`, advancing the shared monotonic sequence only
    /// AFTER the fallible construction succeeds.
    fn source_event(
        &mut self,
        record: &RawRecord,
        source_id: &str,
        event_time: String,
        values: BTreeMap<String, Value>,
    ) -> Result<SourceEvent, AdapterError> {
        let sequence = self.next_sequence;
        self.next_sequence = self
            .next_sequence
            .checked_add(1)
            .ok_or(AdapterError::SequenceExhausted)?;
        Ok(SourceEvent {
            schema_version: 1,
            source_id: source_id.to_owned(),
            source_version: SOURCE_VERSION.to_owned(),
            publisher_epoch: self.publisher_epoch.clone(),
            sequence,
            event_time,
            received_at: record.received_at.clone(),
            values,
        })
    }
}

impl FeedAdapter for SportsDataIoNbaAdapter {
    fn ingest(&mut self, record: &RawRecord) -> Result<IngestOutcome, AdapterError> {
        match self.guard.observe(record)? {
            GuardAction::Configuration(_) => Err(AdapterError::InvalidRecord(
                "SportsDataIO captures do not accept configuration records".to_owned(),
            )),
            GuardAction::Connected => {
                self.last_digest.clear();
                Ok(IngestOutcome::notice(FeedNotice::info(
                    FeedNoticeCode::Connected,
                    "SportsDataIO NBA matrix session connected; awaiting first cycle",
                )))
            }
            GuardAction::Disconnected => {
                self.last_digest.clear();
                Ok(IngestOutcome::notice(FeedNotice::info(
                    FeedNoticeCode::Disconnected,
                    "SportsDataIO NBA matrix session ended",
                )))
            }
            // The outbound descriptor names a GET with no market data; it produces
            // nothing on replay (mirrors the ESPN poll descriptor).
            GuardAction::Outbound => Ok(IngestOutcome::default()),
            GuardAction::Failed(_) => Err(AdapterError::InvalidRecord(
                "SportsDataIO captures do not accept failure records".to_owned(),
            )),
            GuardAction::Inbound(payload) => self.ingest_payload(record, payload),
        }
    }
}

fn bounded_detail(prefix: &str, detail: &str) -> String {
    let bounded: String = detail.chars().take(200).collect();
    format!("{prefix}: {bounded}")
}

/// Why a control file, a matrix entry, or a capture run was refused. Every
/// variant is a refusal before or instead of a request; none is recoverable in
/// place, and none carries a credential (echoed operator values pass through
/// `bounded_name` first).
#[derive(Debug, Error)]
pub enum SportsDataIoNbaError {
    /// The control-file path could not be stat'd or read as UTF-8 text. The
    /// underlying I/O error is deliberately not carried, so a path or filesystem
    /// detail cannot leak into a refusal message.
    #[error("the SportsDataIO control file could not be read")]
    ControlFileUnreadable,
    /// The control file on disk is larger than `MAX_CONTROL_FILE_BYTES`; it is
    /// refused on its metadata size, before any of it is read.
    #[error("the SportsDataIO control file is larger than {maximum} bytes")]
    ControlFileTooLarge {
        /// The size ceiling that was exceeded, in bytes.
        maximum: u64,
    },
    /// The control-file text is not valid JSON, or carries a field the schema
    /// does not declare; carries the serde_json message.
    #[error("the SportsDataIO control file is not valid JSON: {0}")]
    ControlFileJson(String),
    /// The control file parsed but is not usable: an empty or over-long
    /// operation list, a duplicate operation, date, game id or delta window, an
    /// out-of-band season, date, game id or window, a parameter list a selected
    /// operation needs and does not have, or a parameter list nothing consumes.
    #[error("the SportsDataIO control file is invalid: {0}")]
    InvalidControl(String),
    /// The control file names an operation that is not in the compiled
    /// `OPERATIONS` allowlist. Carries the bounded echo of the name.
    #[error("the SportsDataIO control file names an operation outside the allowlist: {0}")]
    UnknownOperation(String),
    /// `request_url` was given parameters that do not match the operation's
    /// `ParameterKind`. Carries the operation name; only reachable when a matrix
    /// entry was built without going through the control-file loader.
    #[error("SportsDataIO operation {0} received parameters of the wrong kind")]
    ParameterMismatch(&'static str),
    /// The compiled matrix would need more requests for one cycle than the
    /// control file's budget allows. Refused at load time, before any request.
    #[error("the request matrix of {requested} requests exceeds its {maximum} request budget")]
    QuotaExceeded {
        /// Requests one full cycle of the compiled matrix would issue, as a count.
        requested: u32,
        /// The budget that was exceeded, in requests.
        maximum: u32,
    },
    /// The run's request budget is fully charged; returned by
    /// `RequestQuota::try_consume` to end a capture cleanly.
    #[error("the SportsDataIO request budget of {maximum} requests is spent")]
    QuotaExhausted {
        /// The budget that is spent, in requests.
        maximum: u32,
    },
    /// The run's aggregate response-byte budget cannot absorb the next body;
    /// returned by `ResponseBudget::try_reserve` without charging it.
    #[error("the SportsDataIO run response budget of {maximum} bytes is spent")]
    ResponseBudgetExhausted {
        /// The budget that is spent, in response bytes.
        maximum: u64,
    },
    /// The built URL failed the structural allowlist check, or a path
    /// substitution was not ASCII alphanumerics-and-dash within 32 bytes. Carries
    /// no value, so a smuggled credential can never be echoed by this refusal.
    #[error("the request URL is outside the SportsDataIO NBA allowlist")]
    UrlNotAllowlisted,
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use reqwest::header::{HeaderMap, HeaderValue};

    use super::{
        API_ROOT, DEFAULT_MAX_REQUESTS, MAX_ECHOED_NAME_BYTES, MAX_RUN_RESPONSE_BYTES, OPERATIONS,
        Operation, ParameterKind, RequestMatrix, RequestParameters, RequestQuota, ResponseBudget,
        ResponseEnvelope, ResponseOutcome, SportsDataIoNbaAdapter, SportsDataIoNbaError,
        allowlisted_headers, classify_series, outbound_descriptor, request_url, response_envelope,
    };
    use crate::{CaptureStream, FeedAdapter, FeedNoticeCode, RawRecord, RecordKind};

    #[test]
    fn every_operation_url_is_inside_the_allowlisted_prefix_and_carries_no_query() {
        for operation in OPERATIONS {
            let parameters = match operation.parameter_kind() {
                ParameterKind::None => RequestParameters::None,
                ParameterKind::Season => RequestParameters::Season("2025".to_owned()),
                ParameterKind::Date => RequestParameters::Date("2026-01-15".to_owned()),
                ParameterKind::GameId => RequestParameters::GameId(12_345),
                ParameterKind::DateMinutes => RequestParameters::DateMinutes {
                    date: "2026-01-15".to_owned(),
                    minutes: 5,
                },
            };
            let url = request_url(operation, &parameters).expect("compiled template is legal");
            assert!(url.starts_with(API_ROOT), "{url}");
            assert!(!url.contains('?'), "{url}");
            assert!(!url.contains('#'), "{url}");
            assert!(!url.contains("key="), "{url}");
            assert!(!url.contains('{'), "{url}");
        }
        assert_eq!(
            request_url(Operation::Teams, &RequestParameters::None).expect("legal"),
            "https://api.sportsdata.io/v3/nba/scores/json/teams"
        );
        assert_eq!(
            request_url(
                Operation::BoxScoresDelta,
                &RequestParameters::DateMinutes {
                    date: "2026-01-15".to_owned(),
                    minutes: 5,
                }
            )
            .expect("legal"),
            "https://api.sportsdata.io/v3/nba/stats/json/BoxScoresDelta/2026-01-15/5"
        );
    }

    #[test]
    fn url_builder_refuses_smuggled_segments_and_mismatched_parameters() {
        for smuggled in [
            "2026-01-15?key=abc",
            "../../v3/nfl/scores/json/teams",
            "2026 01 15",
            "",
            "2026-01-15/extra",
            "2026-01-15#fragment",
            "%2e%2e",
        ] {
            let refusal = request_url(
                Operation::GamesByDateFinal,
                &RequestParameters::Date(smuggled.to_owned()),
            );
            assert!(
                matches!(refusal, Err(SportsDataIoNbaError::UrlNotAllowlisted)),
                "segment {smuggled:?} must be refused"
            );
        }
        assert!(matches!(
            request_url(Operation::Teams, &RequestParameters::GameId(1)),
            Err(SportsDataIoNbaError::ParameterMismatch("teams"))
        ));
    }

    #[test]
    fn control_file_compiles_an_ordered_matrix() {
        let matrix = RequestMatrix::from_control_json(
            r#"{"season":"2025","dates":["2026-01-15","2026-01-16"],"game_ids":[21500,21501],
                "delta_minutes":[5],
                "operations":["teams","games","games_by_date_final","box_score_final",
                              "box_scores_delta"]}"#,
        )
        .expect("valid control file");
        assert_eq!(matrix.max_requests(), DEFAULT_MAX_REQUESTS);
        // 1 teams + 1 games + 2 dates + 2 game ids + (2 dates x 1 window).
        assert_eq!(matrix.len(), 8);
        let names: Vec<&str> = matrix
            .entries()
            .iter()
            .map(|entry| entry.operation.as_str())
            .collect();
        assert_eq!(
            names,
            vec![
                "teams",
                "games",
                "games_by_date_final",
                "games_by_date_final",
                "box_score_final",
                "box_score_final",
                "box_scores_delta",
                "box_scores_delta",
            ]
        );
        assert_eq!(
            matrix.entries()[2].parameters,
            RequestParameters::Date("2026-01-15".to_owned())
        );
    }

    #[test]
    fn control_file_refuses_every_invalid_shape() {
        for (label, json) in [
            (
                "unknown operation",
                r#"{"operations":["teams","nfl_teams"]}"#,
            ),
            ("duplicate operation", r#"{"operations":["teams","teams"]}"#),
            ("no operations", r#"{"operations":[]}"#),
            ("unknown field", r#"{"operations":["teams"],"api_key":"x"}"#),
            (
                "unpadded date",
                r#"{"dates":["2026-1-5"],"operations":["games_by_date_final"]}"#,
            ),
            (
                "impossible date",
                r#"{"dates":["2026-02-30"],"operations":["games_by_date_final"]}"#,
            ),
            (
                "duplicate date",
                r#"{"dates":["2026-01-15","2026-01-15"],"operations":["games_by_date_final"]}"#,
            ),
            (
                "missing date",
                r#"{"dates":[],"operations":["games_by_date_final"]}"#,
            ),
            (
                "unconsumed date",
                r#"{"dates":["2026-01-15"],"operations":["teams"]}"#,
            ),
            ("missing season", r#"{"operations":["games"]}"#),
            (
                "bad season",
                r#"{"season":"2025REG","operations":["games"]}"#,
            ),
            ("short season", r#"{"season":"25","operations":["games"]}"#),
            // Five bytes, no character boundary at four: this must REFUSE. A
            // byte-length check followed by `split_at(4)` panicked here.
            (
                "non-ascii season",
                r#"{"season":"202é","operations":["games"]}"#,
            ),
            (
                "zero game id",
                r#"{"game_ids":[0],"operations":["box_score_final"]}"#,
            ),
            (
                "huge game id",
                r#"{"game_ids":[100000000],"operations":["box_score_final"]}"#,
            ),
            (
                "zero delta window",
                r#"{"dates":["2026-01-15"],"delta_minutes":[0],"operations":["box_scores_delta"]}"#,
            ),
            (
                "over-long delta window",
                r#"{"dates":["2026-01-15"],"delta_minutes":[1441],"operations":["box_scores_delta"]}"#,
            ),
            (
                "zero budget",
                r#"{"operations":["teams"],"max_requests":0}"#,
            ),
            (
                "budget over ceiling",
                r#"{"operations":["teams"],"max_requests":5001}"#,
            ),
            (
                "matrix over budget",
                r#"{"dates":["2026-01-15","2026-01-16"],"operations":["games_by_date_final"],"max_requests":1}"#,
            ),
            ("not json", "{"),
        ] {
            assert!(
                RequestMatrix::from_control_json(json).is_err(),
                "control file {label} must be refused"
            );
        }
        // Every documented season form is accepted.
        for season in ["2025", "2025PRE", "2025POST", "2025STAR"] {
            let json = format!(r#"{{"season":"{season}","operations":["games"]}}"#);
            assert!(
                RequestMatrix::from_control_json(&json).is_ok(),
                "season {season} must be accepted"
            );
        }
    }

    #[test]
    fn refusals_echo_an_operator_value_verbatim_or_not_at_all() {
        // Short printable ASCII is echoed VERBATIM. That is what lets the capture
        // path's exact-match credential redactor recognize a pasted key.
        let short = RequestMatrix::from_control_json(r#"{"operations":["nfl_teams"]}"#)
            .expect_err("an unknown operation is refused")
            .to_string();
        assert!(short.contains("nfl_teams"), "{short}");

        // Past the bound, the value is reported by length only: a truncated echo
        // would print a prefix the redactor cannot match.
        let long = "A".repeat(MAX_ECHOED_NAME_BYTES + 1);
        let message = RequestMatrix::from_control_json(&format!(r#"{{"operations":["{long}"]}}"#))
            .expect_err("an unknown operation is refused")
            .to_string();
        assert!(!message.contains(&long), "{message}");
        assert!(
            !message.contains(&long[..MAX_ECHOED_NAME_BYTES]),
            "{message}"
        );
        assert!(
            message.contains(&format!("<{} bytes, not echoed>", long.len())),
            "{message}"
        );

        // A non-ASCII value is not rendered either, and does not panic.
        let season =
            RequestMatrix::from_control_json(r#"{"season":"202é","operations":["games"]}"#)
                .expect_err("a non-ASCII season is refused")
                .to_string();
        assert!(!season.contains('é'), "{season}");
        assert!(season.contains("not one of the observed forms"), "{season}");
    }

    #[test]
    fn outbound_descriptor_key_order_is_fixed() {
        let descriptor = outbound_descriptor(
            "https://api.sportsdata.io/v3/nba/scores/json/teams",
            Operation::Teams,
            7,
            2,
        );
        assert_eq!(
            descriptor,
            r#"{"request":"https://api.sportsdata.io/v3/nba/scores/json/teams","operation":"teams","cycle":7,"index":2}"#
        );
    }

    #[test]
    fn response_envelope_key_order_is_fixed_and_keeps_the_body_raw() {
        let mut headers = BTreeMap::new();
        headers.insert("content-type".to_owned(), "application/json".to_owned());
        let envelope = response_envelope(
            Operation::Teams,
            1,
            0,
            "2026-09-01T00:00:00.000Z",
            200,
            ResponseOutcome::Response,
            headers,
            br#"[{"TeamID":1}]"#,
            false,
            false,
        );
        assert!(envelope.starts_with(
            r#"{"operation":"teams","cycle":1,"index":0,"received_at":"2026-09-01T00:00:00.000Z","status":200,"outcome":"response","headers":{"content-type":"application/json"},"body_bytes":14,"body_sha256":"#
        ));
        let parsed: ResponseEnvelope = serde_json::from_str(&envelope).expect("round trip");
        assert_eq!(parsed.body, r#"[{"TeamID":1}]"#);
        assert_eq!(parsed.body_encoding, "utf8");
        assert!(!parsed.body_truncated);
        assert_eq!(classify_series(&envelope), "sportsdataio_nba:teams");
        assert_eq!(classify_series("{"), "sportsdataio_nba:unknown");
        assert_eq!(
            classify_series(&envelope.replace(r#""operation":"teams""#, r#""operation":"nfl""#)),
            "sportsdataio_nba:unknown"
        );

        // A non-UTF-8 body is carried base64, with the digest still over raw bytes.
        let binary = response_envelope(
            Operation::Teams,
            1,
            0,
            "2026-09-01T00:00:00.000Z",
            200,
            ResponseOutcome::Response,
            BTreeMap::new(),
            &[0xff, 0xfe],
            true,
            false,
        );
        let parsed: ResponseEnvelope = serde_json::from_str(&binary).expect("round trip");
        assert_eq!(parsed.body_encoding, "base64");
        assert_eq!(parsed.body_bytes, 2);
        assert!(parsed.body_truncated);
        assert!(!parsed.body_redacted);
    }

    /// The journal is the sink the repository secret scanner cannot cover: its
    /// high-confidence marker needs the header NAME beside the value, and a
    /// journal line never carries the header name. So the body a provider echoes
    /// back must already be clean when the envelope is built. This pins the
    /// composition the capture path uses -- redact, then envelope -- including the
    /// property that the recorded digest describes the RECORDED bytes.
    #[test]
    fn an_echoed_credential_is_redacted_out_of_the_recorded_body() {
        use crate::SportsDataIoCredential;

        // Deliberately not key-shaped, so this file cannot become a scanner marker.
        const SENTINEL: &str = "SENTINEL-KEY-DO-NOT-LEAK-7f3a";
        let credential = SportsDataIoCredential::from_value(SENTINEL.to_owned())
            .expect("the sentinel is a well-formed credential value");

        // A 401 whose body echoes the request headers it received -- the exact
        // shape that would otherwise write the live key into a mode-0600 journal.
        let wire = format!(
            "{{\"Message\":\"Authentication failed\",\"Request\":\
             {{\"Ocp-Apim-Subscription-Key\":\"{SENTINEL}\"}}}}"
        );
        let (recorded, redacted) = credential.redact_body(wire.as_bytes(), false);
        assert!(redacted);
        let envelope = response_envelope(
            Operation::Teams,
            1,
            0,
            "2026-09-01T00:00:05.000Z",
            401,
            ResponseOutcome::Response,
            BTreeMap::new(),
            &recorded,
            false,
            redacted,
        );
        assert!(!envelope.contains(SENTINEL));
        let parsed: ResponseEnvelope = serde_json::from_str(&envelope).expect("round trip");
        assert!(parsed.body_redacted);
        assert!(!parsed.body.contains(SENTINEL));
        assert!(parsed.body.contains("<redacted>"));
        assert!(
            parsed
                .body
                .starts_with(r#"{"Message":"Authentication failed""#)
        );
        // The digest is over the RECORDED bytes, never over the wire bytes the
        // journal deliberately does not keep.
        assert_eq!(parsed.body_bytes, recorded.len() as u64);
        assert_eq!(
            parsed.body_sha256,
            crate::secure_http::sha256_hex(&recorded)
        );
        assert_ne!(
            parsed.body_sha256,
            crate::secure_http::sha256_hex(wire.as_bytes())
        );

        // The same envelope carries the evidence flag through replay.
        let mut adapter = SportsDataIoNbaAdapter::new("test").expect("valid epoch");
        adapter.ingest(&connected(1)).expect("connect");
        let outcome = adapter.ingest(&inbound(2, envelope)).expect("evidence");
        assert_eq!(
            outcome.events[0].values["body_redacted"],
            serde_json::json!(true)
        );
        assert!(!format!("{:?}", outcome.events[0].values).contains(SENTINEL));
    }

    /// Finding: `validated_segment` accepts any short alphanumeric-or-dash run, so
    /// a 32-character hexadecimal key satisfies it. The URL builder must therefore
    /// re-run the VALUE validators, not only the structural ones, for callers that
    /// did not come through `RequestMatrix::from_control_json`.
    #[test]
    fn request_url_revalidates_parameter_values_not_only_their_shape() {
        for rejected in [
            RequestParameters::Date("SENTINEL-AUD-9c2e".to_owned()),
            RequestParameters::Date("0f1e2d3c4b5a69788796a5b4c3d2e1f0".to_owned()),
            RequestParameters::Date("2026-1-5".to_owned()),
            RequestParameters::Date("2026-02-30".to_owned()),
        ] {
            let error = request_url(Operation::GamesByDateFinal, &rejected)
                .expect_err("an unvalidated date value is refused by the URL builder");
            assert!(!error.to_string().contains(API_ROOT));
        }
        assert!(
            request_url(
                Operation::Games,
                &RequestParameters::Season("20255".to_owned())
            )
            .is_err()
        );
        assert!(
            request_url(
                Operation::Games,
                &RequestParameters::Season("2025SUM".to_owned())
            )
            .is_err()
        );
        assert!(
            request_url(
                Operation::BoxScoreFinal,
                &RequestParameters::GameId(100_000_000)
            )
            .is_err()
        );
        assert!(
            request_url(
                Operation::BoxScoresDelta,
                &RequestParameters::DateMinutes {
                    date: "2026-01-15".to_owned(),
                    minutes: 1_441,
                }
            )
            .is_err()
        );

        // The legal forms still build.
        request_url(
            Operation::GamesByDateFinal,
            &RequestParameters::Date("2026-01-15".to_owned()),
        )
        .expect("a real calendar date is accepted");
        request_url(
            Operation::Games,
            &RequestParameters::Season("2025POST".to_owned()),
        )
        .expect("an observed season form is accepted");
    }

    #[test]
    fn header_allowlist_keeps_only_the_named_headers_and_never_location() {
        let mut headers = HeaderMap::new();
        headers.insert("content-type", HeaderValue::from_static("application/json"));
        headers.insert("content-length", HeaderValue::from_static("42"));
        headers.insert("retry-after", HeaderValue::from_static("30"));
        headers.insert(
            "date",
            HeaderValue::from_static("Tue, 01 Sep 2026 00:00:00 GMT"),
        );
        headers.insert("x-ratelimit-remaining", HeaderValue::from_static("199"));
        headers.insert(
            "location",
            HeaderValue::from_static("https://evil.example/redirect"),
        );
        headers.insert("set-cookie", HeaderValue::from_static("session=secret"));
        headers.insert(
            "ocp-apim-subscription-key",
            HeaderValue::from_static("sentinel-sdio-key-0123456789abcdef"),
        );

        let retained = allowlisted_headers(&headers, &header_sentinel_credential());
        let names: Vec<&str> = retained.keys().map(String::as_str).collect();
        assert_eq!(
            names,
            vec![
                "content-length",
                "content-type",
                "date",
                "retry-after",
                "x-ratelimit-remaining",
            ]
        );
        let rendered = format!("{retained:?}");
        assert!(!rendered.contains("evil.example"));
        assert!(!rendered.contains("sentinel-sdio-key"));
        assert!(!rendered.contains("session=secret"));
    }

    /// Deliberately not key-shaped, so this file cannot become a scanner marker.
    const HEADER_SENTINEL: &str = "SENTINEL-KEY-DO-NOT-LEAK-7f3a";

    fn header_sentinel_credential() -> crate::SportsDataIoCredential {
        crate::SportsDataIoCredential::from_value(HEADER_SENTINEL.to_owned())
            .expect("the sentinel is a well-formed credential value")
    }

    /// An allowlisted header NAME says nothing about what stands beside it.
    /// `Retry-After` and the open-ended `x-ratelimit-*` family are
    /// provider-controlled strings, and an echoing gateway is entitled to fill one
    /// with the request headers it received -- which is the same leak the body
    /// redactor exists to stop, on a field that was reaching the envelope raw.
    /// This pins the value side of the allowlist end to end: envelope, and the
    /// event a replay of that envelope produces.
    #[test]
    fn an_echoed_credential_is_redacted_out_of_a_retained_header_value() {
        let credential = header_sentinel_credential();
        let mut headers = HeaderMap::new();
        headers.insert("content-type", HeaderValue::from_static("application/json"));
        headers.insert(
            "retry-after",
            HeaderValue::from_bytes(HEADER_SENTINEL.as_bytes()).expect("ascii header value"),
        );
        headers.insert(
            "x-ratelimit-note",
            HeaderValue::from_bytes(format!("quota for {HEADER_SENTINEL} exhausted").as_bytes())
                .expect("ascii header value"),
        );

        let retained = allowlisted_headers(&headers, &credential);
        assert_eq!(retained["retry-after"], "<redacted>");
        assert_eq!(
            retained["x-ratelimit-note"],
            "quota for <redacted> exhausted"
        );
        // An unrelated value is carried through byte for byte.
        assert_eq!(retained["content-type"], "application/json");

        let envelope = response_envelope(
            Operation::Teams,
            1,
            0,
            "2026-09-01T00:00:06.000Z",
            429,
            ResponseOutcome::Response,
            retained,
            b"rate limited",
            false,
            false,
        );
        assert!(!envelope.contains(HEADER_SENTINEL));
        let parsed: ResponseEnvelope = serde_json::from_str(&envelope).expect("round trip");
        assert_eq!(parsed.headers["retry-after"], "<redacted>");
        assert!(parsed.headers["x-ratelimit-note"].contains("<redacted>"));

        // The replayed event carries the redacted values, never the sentinel.
        let mut adapter = SportsDataIoNbaAdapter::new("test").expect("valid epoch");
        adapter.ingest(&connected(1)).expect("connect");
        let outcome = adapter
            .ingest(&inbound(2, envelope))
            .expect("status evidence");
        let event = &outcome.events[0];
        assert_eq!(
            event.values["header_retry_after"],
            serde_json::json!("<redacted>")
        );
        assert_eq!(
            event.values["header_x_ratelimit_note"],
            serde_json::json!("quota for <redacted> exhausted")
        );
        assert!(!format!("{:?}", event.values).contains(HEADER_SENTINEL));
    }

    fn connected(ordinal: u64) -> RawRecord {
        RawRecord::connected_at(
            CaptureStream::SportsDataIoNba,
            1,
            ordinal,
            "2026-09-01T00:00:00.000Z".to_owned(),
            0,
            0,
        )
    }

    fn inbound(ordinal: u64, payload: String) -> RawRecord {
        RawRecord::text_at(
            CaptureStream::SportsDataIoNba,
            1,
            ordinal,
            RecordKind::InboundText,
            payload,
            "2026-09-01T00:00:01.000Z".to_owned(),
            1_000,
        )
    }

    #[test]
    fn adapter_emits_one_evidence_event_per_response_and_never_dies_on_an_odd_body() {
        let mut adapter = SportsDataIoNbaAdapter::new("test").expect("valid epoch");
        let connect = adapter.ingest(&connected(1)).expect("connect");
        assert!(connect.events.is_empty());
        assert_eq!(connect.notices[0].code, FeedNoticeCode::Connected);

        let envelope = response_envelope(
            Operation::GamesByDateFinal,
            1,
            0,
            "2026-09-01T00:00:01.000Z",
            200,
            ResponseOutcome::Response,
            BTreeMap::from([("content-type".to_owned(), "application/json".to_owned())]),
            br#"[{"GameID":21500},{"GameID":21501}]"#,
            false,
            false,
        );
        let outcome = adapter
            .ingest(&inbound(2, envelope.clone()))
            .expect("evidence event");
        assert_eq!(outcome.events.len(), 1);
        let event = &outcome.events[0];
        assert_eq!(event.source_id, "nba.sportsdataio.games_by_date_final");
        assert_eq!(event.event_time, "2026-09-01T00:00:01.000Z");
        assert_eq!(event.values["status"], serde_json::json!(200));
        assert_eq!(event.values["payload_shape"], serde_json::json!("array"));
        assert_eq!(event.values["payload_items"], serde_json::json!(2));
        assert_eq!(event.values["body_changed"], serde_json::json!(true));
        assert_eq!(
            event.values["endpoint"],
            serde_json::json!("scores/json/GamesByDateFinal/{date}")
        );
        assert_eq!(
            event.values["header_content_type"],
            serde_json::json!("application/json")
        );
        // The evidence row carries the digest and size, never the provider body.
        assert!(!format!("{:?}", event.values).contains("GameID"));

        // The identical body on the next cycle is still one row, marked unchanged.
        let repeat = envelope.replace(r#""cycle":1"#, r#""cycle":2"#);
        let outcome = adapter.ingest(&inbound(3, repeat)).expect("second cycle");
        assert_eq!(
            outcome.events[0].values["body_changed"],
            serde_json::json!(false)
        );

        // A recorded non-2xx is status evidence plus a venue notice, not a failure.
        let refusal = response_envelope(
            Operation::Teams,
            2,
            1,
            "2026-09-01T00:00:02.000Z",
            401,
            ResponseOutcome::Response,
            BTreeMap::new(),
            b"unauthorized",
            false,
            false,
        );
        let outcome = adapter
            .ingest(&inbound(4, refusal))
            .expect("status evidence");
        assert_eq!(outcome.events.len(), 1);
        assert_eq!(
            outcome.events[0].values["payload_shape"],
            serde_json::json!("status_evidence")
        );
        assert_eq!(outcome.notices[0].code, FeedNoticeCode::VenueStatus);

        // A refused redirect records the refusal and never a Location.
        let redirect = response_envelope(
            Operation::Teams,
            2,
            1,
            "2026-09-01T00:00:03.000Z",
            302,
            ResponseOutcome::RedirectRefused,
            BTreeMap::new(),
            b"",
            true,
            false,
        );
        let outcome = adapter
            .ingest(&inbound(5, redirect))
            .expect("redirect evidence");
        assert_eq!(
            outcome.events[0].values["outcome"],
            serde_json::json!("redirect_refused")
        );

        // Odd bodies notice and skip; they never abort the session.
        for (offset, odd) in [
            "{",
            r#"{"operation":"nfl_teams","cycle":1,"index":0,"received_at":"2026-09-01T00:00:04.000Z","status":200,"outcome":"response","headers":{},"body_bytes":0,"body_sha256":"x","body_encoding":"utf8","body_truncated":false,"body_redacted":false,"body":""}"#,
        ]
        .into_iter()
        .enumerate()
        {
            let outcome = adapter
                .ingest(&inbound(6 + offset as u64, odd.to_owned()))
                .expect("odd bodies are never fatal");
            assert!(outcome.events.is_empty());
            assert_eq!(outcome.notices[0].code, FeedNoticeCode::UnsupportedMessage);
        }
    }

    #[test]
    fn quota_and_response_budget_refuse_instead_of_overrunning() {
        let mut quota = RequestQuota::new(2);
        assert_eq!(quota.try_consume().expect("first"), 1);
        assert_eq!(quota.try_consume().expect("second"), 2);
        assert!(matches!(
            quota.try_consume(),
            Err(SportsDataIoNbaError::QuotaExhausted { maximum: 2 })
        ));
        assert_eq!(quota.used(), 2);
        assert_eq!(quota.remaining(), 0);

        let mut budget = ResponseBudget::new(100);
        assert_eq!(budget.try_reserve(60).expect("fits"), 60);
        assert!(matches!(
            budget.try_reserve(41),
            Err(SportsDataIoNbaError::ResponseBudgetExhausted { maximum: 100 })
        ));
        // A refused reservation charges nothing.
        assert_eq!(budget.used(), 60);
        let mut overflow = ResponseBudget::new(MAX_RUN_RESPONSE_BYTES);
        assert!(overflow.try_reserve(u64::MAX).is_err());
    }
}
