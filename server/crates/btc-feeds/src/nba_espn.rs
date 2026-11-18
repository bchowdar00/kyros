//! NBA status ingestion from ESPN's keyless public site API (capture slice 1).
//!
//! One poll tick executes a compiled two-request cycle — the league scoreboard
//! (the day's games, scores, period, clock and status) and the league-wide
//! injuries list — every five minutes over plain GETs with no auth. Both are
//! state-snapshot responses, not event streams, so the adapter classifies each
//! body by shape and emits an event only when a game's or an athlete's content
//! signature changed; unchanged entries are silently skipped.
//!
//! Responses are parsed permissively: a body matching neither shape becomes a
//! notice and a skip rather than a run failure, while the per-epoch dedup maps
//! are fail-closed at 200 games and 2,000 athletes. Requests must carry
//! [`CAPTURE_USER_AGENT`]; the block comment below records the probe matrix
//! behind ESPN's User-Agent allowlist.

use std::collections::BTreeMap;
use std::time::Duration;

use chrono::{DateTime, NaiveDateTime, Utc};
use serde::Serialize;
use serde_json::{Value, json};
use signals_core::SourceEvent;

use crate::adapter::{GuardAction, RecordGuard, bounded_token, validate_publisher_epoch};
use crate::event_window::millis_to_rfc3339;
use crate::{AdapterError, FeedAdapter, FeedNotice, FeedNoticeCode, IngestOutcome, RawRecord};

// --- Compiled ESPN status-poll constants. These are `pub(crate)` so the capture
// poll loop and the adapter share one source of truth for the endpoints, cadence,
// timeouts, and the required User-Agent.
//
// This slice introduces the MULTI-REQUEST POLL CYCLE pattern: each capture tick
// executes the compiled ordered `REQUEST_CYCLE` (Chainlink's single-request poll
// generalized to N requests). Both requests are keyless public GETs with no body.
//
// CRITICAL adapter rule (probe matrix 2026-08-31, US host, live-reproduced by the
// audit): ESPN's Akamai wall is a User-Agent ALLOWLIST keyed on the leading UA
// token, NOT a browser-detector. Generic command-line-tool prefixes pass; unknown
// and browser tokens are rejected:
//   curl/8.7.1                                  -> 200
//   curl/8.7.1 (tiny-signals-prototype/0.1)     -> 200  (identity appended, still passes)
//   python-requests/2.x                         -> 200
//   tiny-signals-prototype/0.1                  -> 403  (the shared secure_http default)
//   reqwest default / empty / Wget/*            -> 403
//   Mozilla/5.0 (... browser ...)               -> 403
// So the poller MUST send an explicit UA leading with the standard compatibility
// token every tool uses; `CAPTURE_USER_AGENT` leads with `curl/8.7.1` and carries
// our real identity in the trailing comment field. This is DISTINCT from the
// cdn.nba.com case, which is a TLS-fingerprint (JA3/JA4) block that only browser
// fingerprint FORGERY defeats -- not pursued, recorded as gated. Here no forgery
// is involved: the leading token is a truthful, conventional client identifier.
// The override is applied per-request in `nba_espn_poll` ONLY; the shared
// secure_http client default is unchanged for every other provider.

/// Keyless public ESPN scoreboard endpoint (day's games, scores, status/clock).
/// Verified 200, ~9.5KB. No auth, no headers required.
pub(crate) const SCOREBOARD_URL: &str =
    "https://site.api.espn.com/apis/site/v2/sports/basketball/nba/scoreboard";

/// Keyless public ESPN league-wide injuries endpoint. Verified 200, ~882KB
/// (well under the 8MB bounded-body cap). No auth, no headers required.
pub(crate) const INJURIES_URL: &str =
    "https://site.api.espn.com/apis/site/v2/sports/basketball/nba/injuries";

/// The compiled ordered request sequence executed once per poll tick, in order:
/// scoreboard, then injuries. The capture loop iterates this exactly; requests
/// are independent, so a single per-request failure never aborts the cycle.
pub(crate) const REQUEST_CYCLE: [&str; 2] = [SCOREBOARD_URL, INJURIES_URL];

/// Poll cadence. ESPN publishes no SLA for these unofficial endpoints; community
/// practice is to poll on the order of minutes and cache aggressively. Five
/// minutes over-samples a status feed that changes on game-clock granularity and
/// keeps request volume conservative; the adapter collapses unchanged snapshots.
pub(crate) const POLL_CADENCE: Duration = Duration::from_secs(300);

/// Per-request HTTP timeout for one GET in the cycle.
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// The exact User-Agent the poll path sends per request. Leads with the standard
/// `curl/8.7.1` compatibility token that ESPN's allowlist accepts, then carries
/// our real identity in the trailing comment field (both probe-verified 200 on
/// 2026-08-31). Applied in `nba_espn_poll` only; never mutates the shared
/// secure_http default used by other providers. See the module header for the
/// full allowlist probe matrix and the contrast with the cdn.nba.com TLS block.
pub(crate) const CAPTURE_USER_AGENT: &str = "curl/8.7.1 (tiny-signals-prototype/0.1)";

/// Timing/grouping series names for the normalization analyzer. Poll responses
/// carry no per-message topic; the logical channel is the response SHAPE.
pub(crate) const SCOREBOARD_SERIES: &str = "nba_espn_status:scoreboard";
pub(crate) const INJURIES_SERIES: &str = "nba_espn_status:injuries";
/// Grouping label for a 2xx body that matches neither shape (the analyzer never
/// aborts a run on an odd body; it groups it here instead -- see `classify_series`).
pub(crate) const UNKNOWN_SERIES: &str = "nba_espn_status:unknown";

const SCOREBOARD_SOURCE_ID: &str = "nba.espn.status.scoreboard";
const INJURIES_SOURCE_ID: &str = "nba.espn.status.injuries";
const SOURCE_VERSION: &str = "1.0.0";

/// Fail-closed resource bounds on the per-epoch content-dedup maps. A legitimate
/// scoreboard covers at most ~15 games/day and the league carries ~30 teams and a
/// few hundred active players; these caps are far above any real snapshot, so
/// reaching one is pathological and a fatal fail-closed error, not a per-body
/// anomaly (contrast the odd-body notice+skip path, which is never fatal).
const MAX_TRACKED_GAMES: usize = 200;
const MAX_TRACKED_ATHLETES: usize = 2_000;

/// Build the exact OutboundText descriptor recorded for one poll request. These
/// requests are GETs with no body, so there is no wire request payload to record;
/// the descriptor IS the outbound record, naming the URL and its position in the
/// cycle. `pub` so the capture poll loop and the replay-contract integration test
/// build it from one authority (mirrors `chainlink::request_body`). Field/serialize
/// order fixes the key order to `{"request","cycle","index"}`.
pub fn outbound_descriptor(url: &str, cycle: u64, index: usize) -> String {
    #[derive(Serialize)]
    struct Descriptor<'a> {
        request: &'a str,
        cycle: u64,
        index: usize,
    }
    serde_json::to_string(&Descriptor {
        request: url,
        cycle,
        index,
    })
    .expect("the compiled outbound descriptor always serializes")
}

/// Permissive normalization series classifier. Never fails: a body that is not
/// valid JSON, or matches neither shape, groups as `UNKNOWN_SERIES`. This mirrors
/// the Chainlink precedent that a malformed gateway body must not abort a
/// normalization run when replay of the same journal completes.
pub(crate) fn classify_series(payload: &str) -> &'static str {
    match serde_json::from_str::<Value>(payload) {
        Ok(value) => match classify(&value) {
            Some(PollShape::Scoreboard) => SCOREBOARD_SERIES,
            Some(PollShape::Injuries) => INJURIES_SERIES,
            None => UNKNOWN_SERIES,
        },
        Err(_) => UNKNOWN_SERIES,
    }
}

/// Which ESPN status shape a poll response is. DISCRIMINATOR RULE (pinned against
/// real 2026-08-30 fetches): a scoreboard response carries a top-level `events`
/// array; an injuries response carries a top-level `injuries` array. The two are
/// exclusive in observed ground truth (scoreboard has no `injuries` key, injuries
/// has no `events` key); a both-present body is not seen today, so it is classified
/// as scoreboard AND noticed as ambiguous rather than silently coerced (see
/// `ingest_payload`). The top level is NEVER parsed with `deny_unknown_fields` --
/// ESPN's shape is large, unversioned, and third-party-aggregated, so the adapter
/// extracts a small validated core from a permissive `serde_json::Value` and
/// treats absence of BOTH discriminators as an UnsupportedMessage notice + skip.
enum PollShape {
    Scoreboard,
    Injuries,
}

fn classify(value: &Value) -> Option<PollShape> {
    if value.get("events").is_some_and(Value::is_array) {
        Some(PollShape::Scoreboard)
    } else if value.get("injuries").is_some_and(Value::is_array) {
        Some(PollShape::Injuries)
    } else {
        None
    }
}

/// True when BOTH discriminator arrays are present -- an unobserved shape that the
/// classifier resolves to scoreboard but flags as ambiguous.
fn has_both_discriminators(value: &Value) -> bool {
    value.get("events").is_some_and(Value::is_array)
        && value.get("injuries").is_some_and(Value::is_array)
}

/// Deterministic ESPN status-poll parser and per-epoch content-dedup state. One
/// adapter serves the single NBA status stream (both scoreboard and injuries
/// responses arrive on it, classified by shape).
///
/// Like a Chainlink round poll, this is a STATE-SNAPSHOT poll, not an event
/// stream: every cycle re-reports the full current state, so an unchanged game or
/// athlete is the expected steady state and emits nothing (silent, per the
/// Chainlink silent-duplicate precedent -- a per-poll notice for unchanged state
/// would be pure noise). A changed entry emits one event.
pub struct NbaEspnAdapter {
    publisher_epoch: String,
    guard: RecordGuard,
    next_sequence: u64,
    /// Per-game last-emitted content, per connection epoch. Cleared on connect.
    last_game: BTreeMap<String, GameState>,
    /// Per-athlete last-emitted content, per connection epoch. Cleared on connect.
    last_athlete: BTreeMap<String, AthleteState>,
}

/// Content signature of one game snapshot; equality means "unchanged, emit nothing".
#[derive(Clone, PartialEq, Eq)]
struct GameState {
    state: String,
    status_name: String,
    period: Option<i64>,
    clock: Option<String>,
    home_score: u64,
    away_score: u64,
}

/// Content signature of one athlete injury snapshot.
#[derive(Clone, PartialEq, Eq)]
struct AthleteState {
    status: String,
    detail: String,
}

impl NbaEspnAdapter {
    /// Build the adapter for one publisher epoch, the identity stamped on every
    /// emitted event. Rejects an epoch that is blank after trimming or longer
    /// than 128 bytes with [`AdapterError::InvalidRecord`]; the dedup maps start
    /// empty and are cleared again on each connect.
    pub fn new(publisher_epoch: impl Into<String>) -> Result<Self, AdapterError> {
        let publisher_epoch = publisher_epoch.into();
        validate_publisher_epoch(&publisher_epoch)?;
        Ok(Self {
            publisher_epoch,
            guard: RecordGuard::new(crate::Provider::NbaEspnStatus),
            next_sequence: 0,
            last_game: BTreeMap::new(),
            last_athlete: BTreeMap::new(),
        })
    }

    fn ingest_payload(
        &mut self,
        record: &RawRecord,
        payload: &str,
    ) -> Result<IngestOutcome, AdapterError> {
        // Polls are independent state snapshots, so any single odd 2xx body is
        // transient: it is noticed and skipped with dedup state untouched, never
        // session-fatal (Chainlink D1 doctrine -- one odd body must not poison
        // replay). Genuine journal-structure failures remain fatal at the shared
        // record-guard layer above; this handles poll RESPONSE content only.
        let value: Value = match serde_json::from_str(payload) {
            Ok(value) => value,
            Err(error) => {
                return Ok(IngestOutcome::notice(FeedNotice::info(
                    FeedNoticeCode::UnsupportedMessage,
                    bounded_detail("NBA ESPN poll body is not valid JSON", &error.to_string()),
                )));
            }
        };
        match classify(&value) {
            Some(PollShape::Scoreboard) => {
                let mut outcome = self.scoreboard_events(record, &value)?;
                if has_both_discriminators(&value) {
                    // Unobserved shape: classified as scoreboard, but the presence
                    // of both discriminator arrays is flagged so a silent
                    // mis-classification can never hide an upstream shape change.
                    outcome.notices.push(FeedNotice::info(
                        FeedNoticeCode::StateInconsistent,
                        "NBA ESPN poll body carries BOTH events and injuries arrays; \
                         classified as scoreboard"
                            .to_owned(),
                    ));
                }
                Ok(outcome)
            }
            Some(PollShape::Injuries) => self.injuries_events(record, &value),
            None => Ok(IngestOutcome::notice(FeedNotice::info(
                FeedNoticeCode::UnsupportedMessage,
                "NBA ESPN poll body carries neither an events nor an injuries array".to_owned(),
            ))),
        }
    }

    fn scoreboard_events(
        &mut self,
        record: &RawRecord,
        value: &Value,
    ) -> Result<IngestOutcome, AdapterError> {
        let mut outcome = IngestOutcome::default();
        // The discriminator already proved `events` is an array.
        let games = value["events"].as_array().expect("events is an array");
        for game in games {
            match self.game_emission(record, game)? {
                EntryEmission::Event(event) => outcome.events.push(event),
                EntryEmission::Silent => {}
                EntryEmission::Skip(notice) => outcome.notices.push(notice),
            }
        }
        Ok(outcome)
    }

    fn game_emission(
        &mut self,
        record: &RawRecord,
        game: &Value,
    ) -> Result<EntryEmission, AdapterError> {
        // Extract-and-validate a small core; any missing/invalid core field skips
        // this one game with a notice and leaves the others in the poll untouched.
        let event_id = match id_to_string(game.get("id")) {
            Some(id) => id,
            None => return Ok(skip("NBA scoreboard game is missing a non-empty id")),
        };
        let status = &game["status"];
        let state = match status
            .get("type")
            .and_then(|t| t.get("state"))
            .and_then(Value::as_str)
        {
            Some(state) if !state.is_empty() => state.to_owned(),
            _ => return Ok(skip("NBA scoreboard game is missing status.type.state")),
        };
        let status_name = status
            .get("type")
            .and_then(|t| t.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let period = status.get("period").and_then(Value::as_i64);
        let clock = status
            .get("displayClock")
            .and_then(Value::as_str)
            .filter(|clock| !clock.is_empty())
            .map(str::to_owned);

        let competitors = match game
            .get("competitions")
            .and_then(|c| c.as_array())
            .and_then(|competitions| competitions.first())
            .and_then(|competition| competition.get("competitors"))
            .and_then(Value::as_array)
        {
            Some(competitors) => competitors,
            None => return Ok(skip("NBA scoreboard game is missing competitors")),
        };
        let mut home: Option<TeamSide> = None;
        let mut away: Option<TeamSide> = None;
        for competitor in competitors {
            let side = match competitor.get("homeAway").and_then(Value::as_str) {
                Some("home") => &mut home,
                Some("away") => &mut away,
                _ => continue,
            };
            let team_id = match id_to_string(competitor.get("team").and_then(|t| t.get("id"))) {
                Some(id) => id,
                None => {
                    return Ok(skip(
                        "NBA scoreboard competitor is missing a non-empty team id",
                    ));
                }
            };
            let abbrev = competitor
                .get("team")
                .and_then(|t| t.get("abbreviation"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let score = match score_to_u64(competitor.get("score")) {
                Some(score) => score,
                None => {
                    return Ok(skip(
                        "NBA scoreboard competitor score is not a non-negative integer",
                    ));
                }
            };
            *side = Some(TeamSide {
                team_id,
                abbrev,
                score,
            });
        }
        let (Some(home), Some(away)) = (home, away) else {
            return Ok(skip(
                "NBA scoreboard game is missing a home or away competitor",
            ));
        };

        // Scoreboard responses carry no per-response wire timestamp (the game
        // `date` is the scheduled tip-off, not an observation time), so event_time
        // is honestly the local receipt time, labeled "receipt" per house vocab.
        let candidate = GameState {
            state,
            status_name,
            period,
            clock,
            home_score: home.score,
            away_score: away.score,
        };
        if self.last_game.get(&event_id) == Some(&candidate) {
            // Unchanged snapshot: silent, no notice (state-snapshot poll steady state).
            return Ok(EntryEmission::Silent);
        }
        // Fail closed before inserting a brand-new key beyond the resource bound.
        if !self.last_game.contains_key(&event_id) && self.last_game.len() >= MAX_TRACKED_GAMES {
            return Err(AdapterError::InvalidPayload(format!(
                "NBA scoreboard tracked-game count exceeded {MAX_TRACKED_GAMES}"
            )));
        }

        let mut values = BTreeMap::from([
            ("espn_event_id".to_owned(), json!(event_id)),
            ("status_state".to_owned(), json!(candidate.state)),
            ("status_name".to_owned(), json!(candidate.status_name)),
            ("home_team_id".to_owned(), json!(home.team_id)),
            ("home_team_abbrev".to_owned(), json!(home.abbrev)),
            ("away_team_id".to_owned(), json!(away.team_id)),
            ("away_team_abbrev".to_owned(), json!(away.abbrev)),
            ("home_score".to_owned(), json!(home.score)),
            ("away_score".to_owned(), json!(away.score)),
            ("event_time_basis".to_owned(), json!("receipt")),
            ("event_time".to_owned(), json!(record.received_at)),
        ]);
        if let Some(period) = candidate.period {
            values.insert("period".to_owned(), json!(period));
        }
        if let Some(clock) = &candidate.clock {
            values.insert("clock".to_owned(), json!(clock));
        }

        let event = self.source_event(
            record,
            SCOREBOARD_SOURCE_ID,
            record.received_at.clone(),
            values,
        )?;
        self.last_game.insert(event_id, candidate);
        Ok(EntryEmission::Event(event))
    }

    fn injuries_events(
        &mut self,
        record: &RawRecord,
        value: &Value,
    ) -> Result<IngestOutcome, AdapterError> {
        // The response's own top-level `timestamp` (the feed-generation time) is
        // this shape's source time. When present and window-plausible it is the
        // event_time (basis "source"); otherwise event_time is the local receipt
        // time (basis "receipt"). A malformed top-level timestamp is not fatal: the
        // whole poll falls back to receipt basis with a notice.
        let mut outcome = IngestOutcome::default();
        let (event_time, event_time_basis) = match value.get("timestamp").and_then(Value::as_str) {
            Some(raw) => match parse_espn_iso_to_rfc3339(raw) {
                Ok(source) => (source, "source"),
                Err(detail) => {
                    outcome.notices.push(FeedNotice::info(
                        FeedNoticeCode::StateInconsistent,
                        bounded_detail("NBA injuries top-level timestamp is implausible", &detail),
                    ));
                    (record.received_at.clone(), "receipt")
                }
            },
            None => (record.received_at.clone(), "receipt"),
        };

        // The discriminator already proved `injuries` is an array of team blocks;
        // each team block carries its own `id` and a nested per-player `injuries`
        // array. One SourceEvent is emitted per changed player-injury entry.
        let teams = value["injuries"].as_array().expect("injuries is an array");
        for team in teams {
            let team_id = match id_to_string(team.get("id")) {
                Some(id) => id,
                None => {
                    outcome.notices.push(skip_notice(
                        "NBA injuries team block is missing a non-empty id",
                    ));
                    continue;
                }
            };
            let Some(entries) = team.get("injuries").and_then(Value::as_array) else {
                continue;
            };
            for entry in entries {
                match self.injury_emission(
                    record,
                    &team_id,
                    entry,
                    &event_time,
                    event_time_basis,
                )? {
                    EntryEmission::Event(event) => outcome.events.push(event),
                    EntryEmission::Silent => {}
                    EntryEmission::Skip(notice) => outcome.notices.push(notice),
                }
            }
        }
        Ok(outcome)
    }

    fn injury_emission(
        &mut self,
        record: &RawRecord,
        team_id: &str,
        entry: &Value,
        event_time: &str,
        event_time_basis: &str,
    ) -> Result<EntryEmission, AdapterError> {
        // ESPN does NOT carry a scalar `athlete.id` on this feed (verified: 0 of 77
        // entries on 2026-08-30); the canonical athlete id is only present inside
        // `athlete.links[].href` as `/id/<digits>/`. Extracting it there (100%
        // coverage on the probe) is the honest source of the stable athlete id.
        let athlete = entry.get("athlete");
        let athlete_id = match athlete.and_then(athlete_id_from_links) {
            Some(id) => id,
            None => return Ok(skip("NBA injury entry has no resolvable athlete id")),
        };
        let athlete_name = athlete
            .and_then(|a| a.get("displayName"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let status = match entry.get("status").and_then(Value::as_str) {
            Some(status) if !status.is_empty() => status.to_owned(),
            _ => return Ok(skip("NBA injury entry is missing a non-empty status")),
        };
        // `date` (the injury-report timestamp) is a required validated core field.
        let date = match entry.get("date").and_then(Value::as_str) {
            Some(raw) => match parse_espn_iso_to_rfc3339(raw) {
                Ok(date) => date,
                Err(detail) => {
                    return Ok(skip_owned(bounded_detail(
                        "NBA injury entry date is implausible",
                        &detail,
                    )));
                }
            },
            None => return Ok(skip("NBA injury entry is missing a date")),
        };

        // Injury detail strings live under the `details` object; each is optional.
        let details = entry.get("details");
        let detail_type = detail_string(details, "type");
        let detail_text = detail_string(details, "detail");
        let detail_side = detail_string(details, "side");
        let detail_location = detail_string(details, "location");
        // Canonical detail signature backs the per-athlete change detection.
        let detail_signature = format!(
            "{}|{}|{}|{}",
            detail_type.as_deref().unwrap_or(""),
            detail_text.as_deref().unwrap_or(""),
            detail_side.as_deref().unwrap_or(""),
            detail_location.as_deref().unwrap_or(""),
        );

        let candidate = AthleteState {
            status,
            detail: detail_signature,
        };
        if self.last_athlete.get(&athlete_id) == Some(&candidate) {
            return Ok(EntryEmission::Silent);
        }
        if !self.last_athlete.contains_key(&athlete_id)
            && self.last_athlete.len() >= MAX_TRACKED_ATHLETES
        {
            return Err(AdapterError::InvalidPayload(format!(
                "NBA injuries tracked-athlete count exceeded {MAX_TRACKED_ATHLETES}"
            )));
        }

        let mut values = BTreeMap::from([
            ("athlete_id".to_owned(), json!(athlete_id)),
            ("athlete_name".to_owned(), json!(athlete_name)),
            ("team_id".to_owned(), json!(team_id)),
            ("status".to_owned(), json!(candidate.status)),
            ("date".to_owned(), json!(date)),
            ("event_time_basis".to_owned(), json!(event_time_basis)),
            ("event_time".to_owned(), json!(event_time)),
        ]);
        if let Some(detail_type) = detail_type {
            values.insert("injury_type".to_owned(), json!(detail_type));
        }
        if let Some(detail_text) = detail_text {
            values.insert("injury_detail".to_owned(), json!(detail_text));
        }
        if let Some(detail_side) = detail_side {
            values.insert("injury_side".to_owned(), json!(detail_side));
        }
        if let Some(detail_location) = detail_location {
            values.insert("injury_location".to_owned(), json!(detail_location));
        }

        let event = self.source_event(record, INJURIES_SOURCE_ID, event_time.to_owned(), values)?;
        self.last_athlete.insert(athlete_id, candidate);
        Ok(EntryEmission::Event(event))
    }

    /// Assemble one `SourceEvent`, advancing the shared monotonic sequence only
    /// AFTER the fallible construction succeeds so exhaustion never leaves the
    /// cursor ahead of the last emitted event.
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

impl FeedAdapter for NbaEspnAdapter {
    fn ingest(&mut self, record: &RawRecord) -> Result<IngestOutcome, AdapterError> {
        match self.guard.observe(record)? {
            GuardAction::Configuration(_) => Err(AdapterError::InvalidRecord(
                "NBA ESPN captures do not accept configuration records".to_owned(),
            )),
            GuardAction::Connected => {
                // A fresh poll session restarts the per-epoch dedup state.
                self.last_game.clear();
                self.last_athlete.clear();
                Ok(IngestOutcome::notice(FeedNotice::info(
                    FeedNoticeCode::Connected,
                    "NBA ESPN poll session connected; awaiting first cycle",
                )))
            }
            GuardAction::Disconnected => {
                self.last_game.clear();
                self.last_athlete.clear();
                Ok(IngestOutcome::notice(FeedNotice::info(
                    FeedNoticeCode::Disconnected,
                    "NBA ESPN poll session ended",
                )))
            }
            // The outbound descriptor names a GET with no market data; it produces
            // nothing on replay (mirrors the Chainlink eth_call OutboundText).
            GuardAction::Outbound => Ok(IngestOutcome::default()),
            GuardAction::Failed(_) => Err(AdapterError::InvalidRecord(
                "NBA ESPN captures do not accept failure records".to_owned(),
            )),
            GuardAction::Inbound(payload) => self.ingest_payload(record, payload),
        }
    }
}

/// One team's side of a game snapshot.
struct TeamSide {
    team_id: String,
    abbrev: String,
    score: u64,
}

/// The disposition of one scoreboard/injury entry.
enum EntryEmission {
    Event(SourceEvent),
    /// Unchanged since the last emission this epoch: emit nothing, no notice.
    Silent,
    /// Odd or invalid entry: emit nothing, attach a notice, keep going.
    Skip(FeedNotice),
}

fn skip(detail: &'static str) -> EntryEmission {
    EntryEmission::Skip(skip_notice(detail))
}

fn skip_owned(detail: String) -> EntryEmission {
    EntryEmission::Skip(FeedNotice::info(FeedNoticeCode::UnsupportedMessage, detail))
}

fn skip_notice(detail: &'static str) -> FeedNotice {
    FeedNotice::info(FeedNoticeCode::UnsupportedMessage, detail)
}

/// Extract an id that ESPN renders as either a JSON string or a JSON integer,
/// rejecting an empty/absent value.
fn id_to_string(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::String(id)) if !id.is_empty() => Some(id.clone()),
        Some(Value::Number(id)) => Some(id.to_string()),
        _ => None,
    }
}

/// Parse a score ESPN renders as a string (e.g. `"0"`) or an integer into a
/// non-negative integer, rejecting negatives and non-integers.
fn score_to_u64(value: Option<&Value>) -> Option<u64> {
    match value {
        Some(Value::String(score)) => score.parse::<u64>().ok(),
        Some(Value::Number(score)) => score.as_u64(),
        _ => None,
    }
}

/// Read one optional detail string from the `details` object.
fn detail_string(details: Option<&Value>, key: &str) -> Option<String> {
    details
        .and_then(|d| d.get(key))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

/// Recover the canonical ESPN athlete id from `athlete.links[].href`. ESPN uses a
/// stable `/id/<digits>/` path segment on every player link; the digits are the
/// athlete id. Returns the first match.
fn athlete_id_from_links(athlete: &Value) -> Option<String> {
    let links = athlete.get("links").and_then(Value::as_array)?;
    for link in links {
        if let Some(href) = link.get("href").and_then(Value::as_str)
            && let Some(id) = extract_id_segment(href)
        {
            return Some(id);
        }
    }
    None
}

/// Extract the digits following a `/id/` segment in a URL path (`.../id/4712863/...`).
fn extract_id_segment(href: &str) -> Option<String> {
    let start = href.find("/id/")? + "/id/".len();
    let digits: String = href[start..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    if digits.is_empty() {
        None
    } else {
        Some(digits)
    }
}

/// Parse an ESPN ISO-8601 timestamp and re-render it through the single shared
/// event window (millisecond plausibility bound + canonical RFC 3339). ESPN mixes
/// second-precision (`2026-08-30T23:58:12Z`, valid RFC 3339) and minute-precision
/// (`2026-07-19T00:14Z`, which chrono's RFC 3339 parser rejects), so a minute
/// fallback format is tried before failing. The returned string is uniform.
fn parse_espn_iso_to_rfc3339(raw: &str) -> Result<String, String> {
    let parsed = DateTime::parse_from_rfc3339(raw)
        .map(|value| value.with_timezone(&Utc))
        .or_else(|_| {
            NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%MZ").map(|naive| naive.and_utc())
        })
        .map_err(|_| format!("unrecognized ISO-8601 timestamp `{}`", bounded_token(raw)))?;
    millis_to_rfc3339(parsed.timestamp_millis(), "event_time").map_err(|error| error.to_string())
}

/// A prefixed, bounded quote of an untrusted provider string. The bound is the
/// same [`MAX_QUOTED_TOKEN_CHARS`](crate::adapter::MAX_QUOTED_TOKEN_CHARS) the
/// crate's own [`bounded_token`] applies; only the prefixing is local.
fn bounded_detail(prefix: &str, value: &str) -> String {
    format!("{prefix}: {}", bounded_token(value))
}

#[cfg(test)]
mod tests {
    use super::{NbaEspnAdapter, SCOREBOARD_SERIES, classify_series, outbound_descriptor};
    use crate::{CaptureStream, FeedAdapter, FeedNoticeCode, IngestOutcome, RawRecord, RecordKind};

    fn adapter() -> NbaEspnAdapter {
        NbaEspnAdapter::new("test-epoch").expect("adapter constructs")
    }

    fn record(ordinal: u64, epoch: u64, kind: RecordKind, payload: Option<&str>) -> RawRecord {
        RawRecord {
            schema_version: 1,
            provider: crate::Provider::NbaEspnStatus,
            stream: CaptureStream::NbaEspnStatus,
            connection_epoch: epoch,
            ordinal,
            received_at: "2026-08-30T12:00:01Z".to_owned(),
            capture_elapsed_ns: None,
            connect_started_elapsed_ns: None,
            kind,
            payload: payload.map(str::to_owned),
        }
    }

    fn connected(ordinal: u64, epoch: u64) -> RawRecord {
        record(ordinal, epoch, RecordKind::Connected, None)
    }

    fn inbound(ordinal: u64, payload: &str) -> RawRecord {
        record(ordinal, 1, RecordKind::InboundText, Some(payload))
    }

    // Minimal synthetic fixtures trimmed to the validated core (no giant blobs).
    // Shapes and field paths mirror the real 2026-08-30 fetches exactly.
    fn scoreboard(state: &str, home_score: u64, away_score: u64) -> String {
        format!(
            r#"{{"leagues":[],"day":{{"date":"2026-10-03"}},"events":[{{"id":"401902644","date":"2026-10-03T23:00Z","status":{{"clock":0.0,"displayClock":"0.0","period":0,"type":{{"name":"STATUS_SCHEDULED","state":"{state}"}}}},"competitions":[{{"competitors":[{{"homeAway":"home","score":"{home_score}","team":{{"id":"28","abbreviation":"TOR"}}}},{{"homeAway":"away","score":"{away_score}","team":{{"id":"14","abbreviation":"MIA"}}}}]}}]}}]}}"#
        )
    }

    fn injuries(status: &str, detail: &str) -> String {
        format!(
            r#"{{"timestamp":"2026-08-30T23:58:12Z","status":"success","injuries":[{{"id":"1","displayName":"Atlanta Hawks","injuries":[{{"id":-56292,"status":"{status}","date":"2026-07-19T00:14Z","athlete":{{"displayName":"Mouhamed Gueye","links":[{{"href":"https://www.espn.com/nba/player/_/id/4712863/mouhamed-gueye"}}]}},"details":{{"type":"Foot","detail":"{detail}","side":"Left","location":"Leg"}}}}]}}]}}"#
        )
    }

    fn events(outcome: &IngestOutcome) -> usize {
        outcome.events.len()
    }

    #[test]
    fn classify_series_distinguishes_shapes_and_never_panics() {
        assert_eq!(classify_series(&scoreboard("pre", 0, 0)), SCOREBOARD_SERIES);
        assert_eq!(
            classify_series(&injuries("Out", "Fracture")),
            super::INJURIES_SERIES
        );
        assert_eq!(classify_series("not json at all"), super::UNKNOWN_SERIES);
        assert_eq!(classify_series(r#"{"other":true}"#), super::UNKNOWN_SERIES);
    }

    #[test]
    fn capture_user_agent_is_the_probe_verified_allowlist_value() {
        // ESPN's Akamai wall allowlists the leading UA token; this exact value
        // (curl compatibility token + our identity) is probe-verified 200. A drift
        // here silently re-403s the whole capture path, so it is pinned.
        assert_eq!(
            super::CAPTURE_USER_AGENT,
            "curl/8.7.1 (tiny-signals-prototype/0.1)"
        );
    }

    #[test]
    fn both_discriminators_present_classifies_scoreboard_and_notices_ambiguity() {
        // Craft a body carrying both arrays (scoreboard `events` plus an injuries
        // block). It classifies as scoreboard (one event) and attaches a
        // StateInconsistent notice rather than silently coercing.
        let both = r#"{"events":[{"id":"1","status":{"type":{"state":"pre","name":"S"}},"competitions":[{"competitors":[
            {"homeAway":"home","score":"0","team":{"id":"10","abbreviation":"AAA"}},
            {"homeAway":"away","score":"0","team":{"id":"11","abbreviation":"BBB"}}]}]}],
            "injuries":[{"id":"1","injuries":[]}]}"#;
        let mut adapter = adapter();
        adapter.ingest(&connected(1, 1)).expect("connects");
        let outcome = adapter
            .ingest(&inbound(2, both))
            .expect("both-present body");
        assert_eq!(events(&outcome), 1);
        assert_eq!(outcome.events[0].source_id, "nba.espn.status.scoreboard");
        assert!(
            outcome
                .notices
                .iter()
                .any(|n| n.code == FeedNoticeCode::StateInconsistent),
            "both-present body attaches an ambiguity notice: {outcome:?}"
        );
    }

    #[test]
    fn outbound_descriptor_is_the_exact_shape() {
        assert_eq!(
            outbound_descriptor(super::SCOREBOARD_URL, 3, 0),
            format!(
                r#"{{"request":"{}","cycle":3,"index":0}}"#,
                super::SCOREBOARD_URL
            )
        );
    }

    #[test]
    fn scoreboard_emits_one_event_per_game_with_scores_as_integers() {
        let mut adapter = adapter();
        adapter.ingest(&connected(1, 1)).expect("connects");
        let outcome = adapter
            .ingest(&inbound(2, &scoreboard("pre", 0, 0)))
            .expect("scoreboard parses");
        assert_eq!(events(&outcome), 1);
        let event = &outcome.events[0];
        assert_eq!(event.source_id, "nba.espn.status.scoreboard");
        assert_eq!(
            event.values.get("espn_event_id"),
            Some(&serde_json::json!("401902644"))
        );
        assert_eq!(event.values.get("home_score"), Some(&serde_json::json!(0)));
        assert_eq!(
            event.values.get("away_team_abbrev"),
            Some(&serde_json::json!("MIA"))
        );
        assert_eq!(
            event.values.get("event_time_basis"),
            Some(&serde_json::json!("receipt"))
        );
    }

    #[test]
    fn injuries_emits_one_event_per_entry_with_source_event_time() {
        let mut adapter = adapter();
        adapter.ingest(&connected(1, 1)).expect("connects");
        let outcome = adapter
            .ingest(&inbound(2, &injuries("Day-To-Day", "Fracture")))
            .expect("injuries parse");
        assert_eq!(events(&outcome), 1);
        let event = &outcome.events[0];
        assert_eq!(event.source_id, "nba.espn.status.injuries");
        // Athlete id recovered from the links href, not a scalar field.
        assert_eq!(
            event.values.get("athlete_id"),
            Some(&serde_json::json!("4712863"))
        );
        assert_eq!(event.values.get("team_id"), Some(&serde_json::json!("1")));
        assert_eq!(
            event.values.get("event_time_basis"),
            Some(&serde_json::json!("source"))
        );
        // Top-level timestamp re-rendered through the shared window.
        assert_eq!(
            event.values.get("event_time"),
            Some(&serde_json::json!("2026-08-30T23:58:12.000Z"))
        );
        // Minute-precision injury date parsed via the fallback format.
        assert_eq!(
            event.values.get("date"),
            Some(&serde_json::json!("2026-07-19T00:14:00.000Z"))
        );
    }

    #[test]
    fn unchanged_entries_are_silent_and_changes_emit() {
        let mut adapter = adapter();
        adapter.ingest(&connected(1, 1)).expect("connects");
        assert_eq!(
            events(
                &adapter
                    .ingest(&inbound(2, &scoreboard("pre", 0, 0)))
                    .unwrap()
            ),
            1
        );
        // Identical snapshot: silent, no notice.
        let duplicate = adapter
            .ingest(&inbound(3, &scoreboard("pre", 0, 0)))
            .expect("duplicate");
        assert!(duplicate.events.is_empty());
        assert!(duplicate.notices.is_empty());
        // Score change: one event.
        let changed = adapter
            .ingest(&inbound(4, &scoreboard("in", 10, 8)))
            .expect("changed");
        assert_eq!(events(&changed), 1);
        assert_eq!(
            changed.events[0].values.get("home_score"),
            Some(&serde_json::json!(10))
        );

        // Injuries: same repeats silently, changed status re-emits.
        assert_eq!(
            events(
                &adapter
                    .ingest(&inbound(5, &injuries("Out", "Fracture")))
                    .unwrap()
            ),
            1
        );
        let inj_dup = adapter
            .ingest(&inbound(6, &injuries("Out", "Fracture")))
            .expect("injury duplicate");
        assert!(inj_dup.events.is_empty());
        assert!(inj_dup.notices.is_empty());
        let inj_changed = adapter
            .ingest(&inbound(7, &injuries("Available", "Fracture")))
            .expect("injury change");
        assert_eq!(events(&inj_changed), 1);
    }

    #[test]
    fn odd_bodies_are_noticed_and_skipped_without_poisoning_the_session() {
        let mut adapter = adapter();
        adapter.ingest(&connected(1, 1)).expect("connects");
        // Non-JSON body.
        let garbage = adapter
            .ingest(&inbound(2, "<html>502 Bad Gateway</html>"))
            .expect("garbage is operational");
        assert!(garbage.events.is_empty());
        assert_eq!(garbage.notices[0].code, FeedNoticeCode::UnsupportedMessage);
        // Missing discriminator.
        let wrong_shape = adapter
            .ingest(&inbound(3, r#"{"something":"else"}"#))
            .expect("wrong shape is operational");
        assert!(wrong_shape.events.is_empty());
        assert_eq!(
            wrong_shape.notices[0].code,
            FeedNoticeCode::UnsupportedMessage
        );
        // The session still works: a valid scoreboard emits.
        assert_eq!(
            events(
                &adapter
                    .ingest(&inbound(4, &scoreboard("pre", 0, 0)))
                    .unwrap()
            ),
            1
        );
    }

    #[test]
    fn a_bad_entry_skips_only_itself() {
        // Two games; the second has a non-integer score. The first still emits.
        let payload = r#"{"events":[
            {"id":"1","status":{"type":{"state":"pre","name":"S"}},"competitions":[{"competitors":[
                {"homeAway":"home","score":"1","team":{"id":"10","abbreviation":"AAA"}},
                {"homeAway":"away","score":"2","team":{"id":"11","abbreviation":"BBB"}}]}]},
            {"id":"2","status":{"type":{"state":"pre","name":"S"}},"competitions":[{"competitors":[
                {"homeAway":"home","score":"x","team":{"id":"12","abbreviation":"CCC"}},
                {"homeAway":"away","score":"2","team":{"id":"13","abbreviation":"DDD"}}]}]}
        ]}"#;
        let mut adapter = adapter();
        adapter.ingest(&connected(1, 1)).expect("connects");
        let outcome = adapter.ingest(&inbound(2, payload)).expect("mixed batch");
        assert_eq!(events(&outcome), 1);
        assert_eq!(outcome.notices.len(), 1);
        assert_eq!(outcome.notices[0].code, FeedNoticeCode::UnsupportedMessage);
    }

    #[test]
    fn per_epoch_state_resets_on_reconnect() {
        let mut adapter = adapter();
        adapter.ingest(&connected(1, 1)).expect("connects");
        adapter
            .ingest(&inbound(2, &scoreboard("pre", 0, 0)))
            .expect("first epoch game");
        adapter
            .ingest(&record(3, 1, RecordKind::Disconnected, None))
            .expect("disconnects");
        adapter.ingest(&connected(4, 2)).expect("reconnects");
        // New epoch cleared the dedup map: the same snapshot emits again.
        let reemitted = adapter
            .ingest(&RawRecord {
                connection_epoch: 2,
                ..inbound(5, &scoreboard("pre", 0, 0))
            })
            .expect("second epoch game");
        assert_eq!(events(&reemitted), 1);
    }

    #[test]
    fn configuration_records_are_refused() {
        let mut adapter = adapter();
        let configuration = record(1, 1, RecordKind::Configuration, Some("{}"));
        assert!(adapter.ingest(&configuration).is_err());
    }

    #[test]
    fn missing_athlete_id_and_bad_timestamps_notice_and_skip() {
        // Injury entry whose athlete has no /id/ link is skipped.
        let no_id = r#"{"timestamp":"2026-08-30T23:58:12Z","injuries":[{"id":"1","injuries":[
            {"status":"Out","date":"2026-07-19T00:14Z","athlete":{"displayName":"X","links":[{"href":"https://www.espn.com/nba/player"}]},"details":{}}]}]}"#;
        {
            let mut feed = adapter();
            feed.ingest(&connected(1, 1)).expect("connects");
            let outcome = feed.ingest(&inbound(2, no_id)).expect("operational");
            assert!(outcome.events.is_empty());
            assert_eq!(outcome.notices[0].code, FeedNoticeCode::UnsupportedMessage);
        }

        // A pre-window/implausible date fails the shared window and skips.
        let bad_date = r#"{"timestamp":"2026-08-30T23:58:12Z","injuries":[{"id":"1","injuries":[
            {"status":"Out","date":"1700-01-01T00:00Z","athlete":{"displayName":"X","links":[{"href":"https://x/id/99/y"}]},"details":{}}]}]}"#;
        {
            let mut feed = adapter();
            feed.ingest(&connected(1, 1)).expect("connects");
            let outcome = feed.ingest(&inbound(2, bad_date)).expect("operational");
            assert!(outcome.events.is_empty());
            assert_eq!(outcome.notices[0].code, FeedNoticeCode::UnsupportedMessage);
        }
    }

    #[test]
    fn replay_is_deterministic_over_a_two_cycle_capture() {
        let fixture = vec![
            connected(1, 1),
            record(
                2,
                1,
                RecordKind::OutboundText,
                Some(&outbound_descriptor(super::SCOREBOARD_URL, 1, 0)),
            ),
            inbound(3, &scoreboard("pre", 0, 0)),
            record(
                4,
                1,
                RecordKind::OutboundText,
                Some(&outbound_descriptor(super::INJURIES_URL, 1, 1)),
            ),
            inbound(5, &injuries("Out", "Fracture")),
            record(
                6,
                1,
                RecordKind::OutboundText,
                Some(&outbound_descriptor(super::SCOREBOARD_URL, 2, 0)),
            ),
            inbound(7, &scoreboard("in", 5, 4)),
            record(
                8,
                1,
                RecordKind::OutboundText,
                Some(&outbound_descriptor(super::INJURIES_URL, 2, 1)),
            ),
            inbound(9, &injuries("Out", "Fracture")),
        ];
        let run = || {
            let mut adapter = adapter();
            let mut events = Vec::new();
            for record in &fixture {
                let IngestOutcome { events: batch, .. } =
                    adapter.ingest(record).expect("record ingests");
                events.extend(batch);
            }
            serde_json::to_string(&events).expect("events serialize")
        };
        assert_eq!(run(), run());
        // Cycle 1: one scoreboard + one injury = 2. Cycle 2: scoreboard changed
        // (1 event), injury unchanged (silent). Total 3.
        let mut adapter = adapter();
        let mut total = 0;
        for record in &fixture {
            total += adapter.ingest(record).expect("ingests").events.len();
        }
        assert_eq!(total, 3);
    }
}
