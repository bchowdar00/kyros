//! NBA reference-data ingestion from ESPN's keyless public core and site APIs
//! (capture slice 2).
//!
//! Once per capture run this walks a discovered entity tree over eight endpoint
//! families: the teams list and league standings, then per team a roster, season
//! statistics and records, then per rostered athlete season statistics, a
//! gamelog and splits. Requests are keyless HTTPS GETs paced at 150 ms and
//! bounded at 40 teams and 800 athletes; the crawl binds one season, keyed by
//! its ending calendar year.
//!
//! Because these are GETs with no body, the capture loop journals an
//! `OutboundText` descriptor naming each URL and the adapter pairs it with the
//! following `InboundText` to know which family a response belongs to. A body
//! matching no known family groups as `UNKNOWN_SERIES` rather than failing the
//! run.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::Serialize;
use serde_json::{Value, json};
use signals_core::SourceEvent;

use crate::adapter::{GuardAction, RecordGuard, bounded_token, validate_publisher_epoch};
use crate::{AdapterError, FeedAdapter, FeedNotice, FeedNoticeCode, IngestOutcome, RawRecord};

// --- Compiled ESPN daily-crawl constants. `pub(crate)` so the capture crawl loop
// and the adapter share one source of truth for the endpoint TEMPLATES, the
// season binding, the crawl bounds, the pacing, and the timeout.
//
// This slice introduces the TWO-PHASE CRAWL pattern. Slice 1 (`nba_espn`) polls a
// fixed two-URL cycle on a cadence; this stream instead walks a DISCOVERED entity
// tree exactly once per capture run:
//
//   phase A (fixed + per-team, 2 + 3N_team requests)
//     1. the teams list          -> discovers the team ids (emits nothing)
//     2. the league standings    -> one event per team entry
//     3. per team, in id order:  roster, season statistics, records
//        (the rosters discover the athlete ids for phase B)
//   phase B (per-athlete, 3N_athlete requests)
//     4. per athlete, in id order: season statistics, gamelog, splits
//
// At 30 real teams that is 2 + 90 = 92 phase-A requests; at the ~480 rostered
// athletes seen in the offseason (624 in season) phase B is 1,440 (1,872)
// requests, so a whole crawl is ~1,532-1,964 requests, ~4-6 minutes at the
// compiled 150 ms pacing plus latency.
//
// The User-Agent story is IDENTICAL to slice 1 and is not restated here: ESPN's
// Akamai wall is a UA allowlist keyed on the leading token, so the crawl reuses
// `nba_espn::CAPTURE_USER_AGENT` (`curl/8.7.1 (tiny-signals-prototype/0.1)`),
// probe-verified 200 against every one of the eight endpoint families below.
// Every template is HTTPS: ESPN's core API advertises itself over `http://` in
// its own `$ref` links, but the `https://` origin answers 200 for all of them
// (probe-verified 2026-08-30), so the crawl never leaves TLS.

/// The season the crawl binds. ESPN's core API keys seasons by their ENDING
/// calendar year, so `2026` is the completed 2025-26 season -- the most recent
/// season carrying a full statistics, record, gamelog and splits surface. The
/// site API's roster endpoint independently reports the UPCOMING season
/// (2026-27 preseason) because it is not season-parameterized; that divergence
/// is real, is carried honestly in the roster event's own `season_year`, and is
/// never reconciled away here.
pub(crate) const SEASON: &str = "2026";

/// ESPN season-type code 2 = Regular Season (1 = Preseason, 3 = Postseason).
pub(crate) const SEASON_TYPE: &str = "2";

/// Teams list. Discovery input only: it classifies cleanly and emits NOTHING
/// (see `Shape::TeamsList`), the way an op-ack does on a WebSocket feed.
pub(crate) const TEAMS_URL: &str =
    "https://site.api.espn.com/apis/site/v2/sports/basketball/nba/teams";

/// League standings, both conferences in one response. Sole source of the seven
/// captured display fields in catalog section 5.7 (`streak_display_season` and the six
/// `record_summary_*_display` strings).
pub(crate) const STANDINGS_URL: &str =
    "https://site.api.espn.com/apis/v2/sports/basketball/nba/standings";

/// Per-team roster. `{team}` is the ESPN team id.
pub(crate) const ROSTER_TEMPLATE: &str =
    "https://site.api.espn.com/apis/site/v2/sports/basketball/nba/teams/{team}/roster";

/// Per-team season statistics inventory (109 stats across three categories).
pub(crate) const TEAM_STATISTICS_TEMPLATE: &str = "https://sports.core.api.espn.com/v2/sports/basketball/leagues/nba/seasons/{season}/types/{type}/teams/{team}/statistics";

/// Per-team record collection. `limit=100` is REQUIRED, not cosmetic: the default
/// page size is 25 and a real team carries 52 record rows over 3 pages, so the
/// bare URL would silently truncate the collection. With the limit the response
/// is one page (`pageCount` 1) and the crawl needs no pagination loop.
pub(crate) const TEAM_RECORD_TEMPLATE: &str = "https://sports.core.api.espn.com/v2/sports/basketball/leagues/nba/seasons/{season}/types/{type}/teams/{team}/records?limit=100";

/// Per-athlete season statistics inventory (108 stats across three categories).
pub(crate) const ATHLETE_STATISTICS_TEMPLATE: &str = "https://sports.core.api.espn.com/v2/sports/basketball/leagues/nba/seasons/{season}/types/{type}/athletes/{athlete}/statistics";

/// Per-athlete gamelog: one stat line per completed game, partitioned by season
/// type. Catalog source S2; the ONLY source of the `_last5`/`_last10` window bases.
pub(crate) const ATHLETE_GAMELOG_TEMPLATE: &str = "https://site.web.api.espn.com/apis/common/v3/sports/basketball/nba/athletes/{athlete}/gamelog?season={season}";

/// Per-athlete splits: box columns across six split dimensions. Catalog source S3;
/// the ONLY source of the `_home`/`_road`/`_rest3plus`/`_month_*`/`_dow_*` bases.
pub(crate) const ATHLETE_SPLITS_TEMPLATE: &str = "https://site.web.api.espn.com/apis/common/v3/sports/basketball/nba/athletes/{athlete}/splits?season={season}";

/// Delay between consecutive crawl requests. ESPN publishes no SLA for these
/// unofficial endpoints; ~6.7 requests/second is well inside ordinary browsing
/// behaviour and keeps a whole crawl to minutes rather than seconds of burst.
pub(crate) const REQUEST_PACING: Duration = Duration::from_millis(150);

/// Per-request HTTP timeout. Larger than slice 1's 10 s because a gamelog body is
/// ~880 KB (measured 639,082 / 797,577 / 882,547 B on three athletes) rather than
/// slice 1's ~9.5 KB scoreboard.
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// Fail-closed bound on discovered teams. The league has 30; 40 leaves headroom
/// for an expansion or an all-star entry without ever letting a malformed
/// discovery response drive an unbounded crawl.
pub(crate) const MAX_TEAMS: usize = 40;

/// Fail-closed bound on discovered athletes. Rosters carried ~480 athletes in the
/// 2026-08-31 offseason and ~624 in season; 800 leaves headroom while bounding
/// the crawl.
pub(crate) const MAX_ATHLETES: usize = 800;

/// Grouping label for a 2xx body that matches no known shape. The analyzer never
/// aborts a run on an odd body; it groups it here instead (see `classify_series`).
pub(crate) const UNKNOWN_SERIES: &str = "nba_espn_daily:unknown";

const SOURCE_VERSION: &str = "1.0.0";

/// The seven emitting source ids. Teams-list is deliberately absent: it is a
/// recognized NON-EVENT shape.
const STANDINGS_SOURCE_ID: &str = "nba.espn.daily.standings";
const ROSTER_SOURCE_ID: &str = "nba.espn.daily.roster";
const TEAM_STATISTICS_SOURCE_ID: &str = "nba.espn.daily.team-statistics";
const TEAM_RECORD_SOURCE_ID: &str = "nba.espn.daily.team-record";
const ATHLETE_STATISTICS_SOURCE_ID: &str = "nba.espn.daily.athlete-statistics";
const ATHLETE_GAMELOG_SOURCE_ID: &str = "nba.espn.daily.athlete-gamelog";
const ATHLETE_SPLITS_SOURCE_ID: &str = "nba.espn.daily.athlete-splits";

/// Fail-closed bounds on ONE response body's emission fan-out. These are far
/// above any real payload (a gamelog carried 94 events, a splits payload 54 rows,
/// a record collection 52 rows, a roster 16-18 athletes, standings 30 entries),
/// so reaching one means the body is pathological. Exceeding a bound is a fatal
/// fail-closed error, not a per-row anomaly.
const MAX_ROWS_PER_BODY: usize = 4_000;
/// Fail-closed bound on the number of `values` entries in one emitted event.
const MAX_VALUES_PER_EVENT: usize = 1_000;

/// The eight response shapes this stream recognizes. This is the CLOSED list.
///
/// DISCRIMINATOR RULES, each pinned against a real 2026-08-30 fetch of the
/// corresponding endpoint (the byte sizes in the module header name those
/// fetches). The top level is NEVER parsed with `deny_unknown_fields`: ESPN's
/// shapes are large, unversioned and third-party-aggregated, so the adapter
/// extracts a validated core from a permissive `serde_json::Value` and treats an
/// unrecognized body as an UnsupportedMessage notice + skip.
///
/// The rules are checked in the fixed order below, so each rule may assume the
/// earlier ones already failed:
///
/// 1. `TeamsList` -- top-level `sports` ARRAY.
/// 2. `AthleteSplits` -- top-level `splitCategories` ARRAY.
/// 3. `AthleteGamelog` -- top-level `events` OBJECT (a map keyed by event id) AND
///    a `seasonTypes` ARRAY. The JSON TYPE of `events` is the whole discriminator
///    against slice 1's scoreboard, whose `events` is an ARRAY; an events-ARRAY
///    body is deliberately NOT recognized here and falls through to the
///    unsupported path.
/// 4. `Roster` -- top-level `athletes` ARRAY and a `team` OBJECT.
/// 5. `Standings` -- top-level `children` ARRAY.
/// 6. `TeamRecord` -- top-level `items` ARRAY and a `pageCount` NUMBER (the ESPN
///    core-API paginated-collection envelope). Note `items[].type` is NOT usable
///    as the discriminator or as an instance key: a live 52-row collection
///    carries `type` "total" TWICE (ids 0 and 900). The row `id` is unique and is
///    the row key.
/// 7. `AthleteStatistics` -- `splits.categories` ARRAY and a top-level `athlete`
///    OBJECT.
/// 8. `TeamStatistics` -- `splits.categories` ARRAY and a top-level `team`
///    OBJECT.
///
/// Rules 7 and 8 share the `splits.categories` carrier and are separated only by
/// which entity object accompanies it, so they are checked last and together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Shape {
    TeamsList,
    Standings,
    Roster,
    TeamStatistics,
    TeamRecord,
    AthleteStatistics,
    AthleteGamelog,
    AthleteSplits,
}

impl Shape {
    /// The wire family token used in the outbound descriptor and in the
    /// normalization series. One authority for both directions.
    pub(crate) const fn family(self) -> &'static str {
        match self {
            Self::TeamsList => "teams-list",
            Self::Standings => "standings",
            Self::Roster => "roster",
            Self::TeamStatistics => "team-statistics",
            Self::TeamRecord => "team-record",
            Self::AthleteStatistics => "athlete-statistics",
            Self::AthleteGamelog => "athlete-gamelog",
            Self::AthleteSplits => "athlete-splits",
        }
    }

    /// Parse a family token back into a shape. Used to compare an inbound body's
    /// classified shape against the paired descriptor's declared family.
    fn from_family(token: &str) -> Option<Self> {
        Some(match token {
            "teams-list" => Self::TeamsList,
            "standings" => Self::Standings,
            "roster" => Self::Roster,
            "team-statistics" => Self::TeamStatistics,
            "team-record" => Self::TeamRecord,
            "athlete-statistics" => Self::AthleteStatistics,
            "athlete-gamelog" => Self::AthleteGamelog,
            "athlete-splits" => Self::AthleteSplits,
            _ => return None,
        })
    }

    /// True for the two families whose response body carries NO occurrence of the
    /// athlete id anywhere, so their instance identity must come from the paired
    /// outbound descriptor instead of from the body (ruling R1).
    const fn identity_comes_from_descriptor(self) -> bool {
        matches!(self, Self::AthleteGamelog | Self::AthleteSplits)
    }
}

/// Build the exact outbound descriptor recorded before one crawl request.
///
/// Crawl requests are GETs with no body, so the descriptor IS the outbound
/// record. It extends slice 1's `{"request","cycle","index"}` with the two fields
/// the daily stream needs: the request's `family` and its `entity_id`.
///
/// `entity_id` is load-bearing under ruling R1. The athlete gamelog and athlete
/// splits payloads contain ZERO occurrences of the athlete id -- verified against
/// three athletes (4433134 / 3945274 / 4066648), with and without an explicit
/// `season` parameter, 12 combinations, `grep -c` = 0 in every one; there is no
/// `athlete` key, no `athlete*` string and no `/id/<digits>/` link anywhere, and
/// `filters[]` carries only `league` and `season`. Two low-minute players can even
/// produce byte-identical splits bodies, so identity is not recoverable from
/// content either. The descriptor is therefore the only honest carrier, and the
/// adapter reads `entity_id` off it for exactly those two families.
///
/// This mirrors the established house precedent that replay may recover
/// configuration from an `OutboundText` record (`replay::ReplaySession::
/// observe_configuration` recovers Kraken book depth the same way), and the
/// adapter already sees these records via `GuardAction::Outbound`.
///
/// Field/serialize order fixes the key order to
/// `{"request","phase","family","entity_id","index"}`.
pub fn outbound_descriptor(
    url: &str,
    phase: &str,
    family: &str,
    entity_id: &str,
    index: u64,
) -> String {
    #[derive(Serialize)]
    struct Descriptor<'a> {
        request: &'a str,
        phase: &'a str,
        family: &'a str,
        entity_id: &'a str,
        index: u64,
    }
    serde_json::to_string(&Descriptor {
        request: url,
        phase,
        family,
        entity_id,
        index,
    })
    .expect("the compiled outbound descriptor always serializes")
}

/// Render one compiled template. Substitution is by literal placeholder, so a
/// template that forgets a placeholder is visible in the produced URL rather than
/// silently dropping the entity.
pub(crate) fn render(template: &str, team: &str, athlete: &str) -> String {
    template
        .replace("{season}", SEASON)
        .replace("{type}", SEASON_TYPE)
        .replace("{team}", team)
        .replace("{athlete}", athlete)
}

/// Permissive normalization series classifier. Never fails: a body that is not
/// valid JSON, or matches no shape, groups as `UNKNOWN_SERIES`. Mirrors the
/// Chainlink and slice-1 precedent that a malformed body must not abort a
/// normalization run when replay of the same journal completes.
pub(crate) fn classify_series(payload: &str) -> String {
    match serde_json::from_str::<Value>(payload) {
        Ok(value) => match classify(&value) {
            Some(shape) => format!("nba_espn_daily:{}", shape.family()),
            None => UNKNOWN_SERIES.to_owned(),
        },
        Err(_) => UNKNOWN_SERIES.to_owned(),
    }
}

/// Apply the discriminator rules in their documented fixed order.
pub(crate) fn classify(value: &Value) -> Option<Shape> {
    if is_array(value, "sports") {
        return Some(Shape::TeamsList);
    }
    if is_array(value, "splitCategories") {
        return Some(Shape::AthleteSplits);
    }
    // The JSON TYPE of `events` is the discriminator against slice 1's scoreboard.
    if value.get("events").is_some_and(Value::is_object) && is_array(value, "seasonTypes") {
        return Some(Shape::AthleteGamelog);
    }
    if is_array(value, "athletes") && value.get("team").is_some_and(Value::is_object) {
        return Some(Shape::Roster);
    }
    if is_array(value, "children") {
        return Some(Shape::Standings);
    }
    if is_array(value, "items") && value.get("pageCount").is_some_and(Value::is_number) {
        return Some(Shape::TeamRecord);
    }
    if value
        .get("splits")
        .is_some_and(|splits| is_array(splits, "categories"))
    {
        if value.get("athlete").is_some_and(Value::is_object) {
            return Some(Shape::AthleteStatistics);
        }
        if value.get("team").is_some_and(Value::is_object) {
            return Some(Shape::TeamStatistics);
        }
    }
    None
}

fn is_array(value: &Value, key: &str) -> bool {
    value.get(key).is_some_and(Value::is_array)
}

/// One outbound descriptor awaiting its response.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingDescriptor {
    family: String,
    entity_id: String,
}

/// Deterministic ESPN daily-crawl parser.
///
/// SNAPSHOT SEMANTICS, DELIBERATELY WITHOUT DEDUP. Slice 1 polls the same two
/// URLs every five minutes, so an unchanged entry is the steady state and is
/// collapsed. This stream walks the whole entity tree exactly ONCE per capture
/// run: every row it sees is the first and only observation of that entity this
/// run, so there is nothing to collapse. Cross-run change detection is the
/// consumer's job over the daily files, not the adapter's, and inventing an
/// in-adapter dedup map here would silently drop the second of two legitimately
/// identical rows (two low-minute players really can produce identical splits).
///
/// DESCRIPTOR PAIRING (ruling R1). The adapter keeps ONE pending descriptor:
/// every `OutboundText` sets it, every `InboundText` consumes it. A new
/// descriptor arriving while one is still pending REPLACES it and emits a
/// `StateInconsistent` notice for the orphan -- that is a legal, expected state,
/// because the capture loop writes the descriptor BEFORE the GET and fabricates
/// no inbound record when a request fails, so each per-request failure orphans
/// exactly one descriptor.
///
/// A descriptor still pending at the END of a journal is silently discarded, and
/// that disposition applies to EVERY trailing pending descriptor -- however the
/// journal ends: a failed final request, a capture deadline, an orderly
/// shutdown, or truncation by the record or byte cap. This stream never writes a
/// `Disconnected` record, so the journal simply stops on its trailing
/// `OutboundText` with no terminator and there is no end-of-journal hook to fire
/// the orphan notice. That is deliberate: no response arrived for that
/// descriptor, so the only thing lost is the notice, never an observation -- the
/// same notices-only disposition the Chainlink D1 doctrine carried through
/// slice 1. Making a truncated-but-valid journal fail replay would be worse than
/// the missing notice.
pub struct NbaEspnDailyAdapter {
    publisher_epoch: String,
    guard: RecordGuard,
    next_sequence: u64,
    pending: Option<PendingDescriptor>,
}

impl NbaEspnDailyAdapter {
    /// Build the adapter for one publisher epoch, the identity stamped on every
    /// emitted event. Rejects an epoch that is blank after trimming or longer
    /// than 128 bytes with [`AdapterError::InvalidRecord`]; no descriptor is
    /// pending at construction.
    pub fn new(publisher_epoch: impl Into<String>) -> Result<Self, AdapterError> {
        let publisher_epoch = publisher_epoch.into();
        validate_publisher_epoch(&publisher_epoch)?;
        Ok(Self {
            publisher_epoch,
            guard: RecordGuard::new(crate::Provider::NbaEspnDaily),
            next_sequence: 0,
            pending: None,
        })
    }

    /// Record one outbound descriptor, replacing (and noticing) an orphan.
    fn observe_descriptor(&mut self, payload: &str) -> IngestOutcome {
        let mut outcome = IngestOutcome::default();
        let parsed = serde_json::from_str::<Value>(payload)
            .ok()
            .and_then(|value| {
                let family = value.get("family").and_then(Value::as_str)?.to_owned();
                let entity_id = value
                    .get("entity_id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                Some(PendingDescriptor { family, entity_id })
            });
        if let Some(orphan) = self.pending.take() {
            outcome.notices.push(FeedNotice::info(
                FeedNoticeCode::StateInconsistent,
                format!(
                    "NBA ESPN daily descriptor for family `{}` entity `{}` was never paired with a \
                     response and is replaced by a new request",
                    bounded_token(&orphan.family),
                    bounded_token(&orphan.entity_id)
                ),
            ));
        }
        match parsed {
            Some(descriptor) => self.pending = Some(descriptor),
            None => {
                // An outbound record that is not a recognizable descriptor leaves
                // the pending slot EMPTY rather than holding a half-parsed one, so
                // the next gamelog/splits body fails closed on "no descriptor"
                // instead of being keyed from garbage.
                outcome.notices.push(FeedNotice::info(
                    FeedNoticeCode::UnsupportedMessage,
                    "NBA ESPN daily outbound record is not a recognizable crawl descriptor"
                        .to_owned(),
                ));
            }
        }
        outcome
    }

    fn ingest_payload(
        &mut self,
        record: &RawRecord,
        payload: &str,
    ) -> Result<IngestOutcome, AdapterError> {
        // Every crawl request is independent, so any single odd 2xx body is
        // transient: it is noticed and skipped, never session-fatal (Chainlink D1
        // doctrine, carried through slice 1). Genuine journal-structure failures
        // remain fatal at the shared record-guard layer above.
        let pending = self.pending.take();
        let value: Value = match serde_json::from_str(payload) {
            Ok(value) => value,
            Err(error) => {
                return Ok(IngestOutcome::notice(FeedNotice::info(
                    FeedNoticeCode::UnsupportedMessage,
                    bounded_detail("NBA ESPN daily body is not valid JSON", &error.to_string()),
                )));
            }
        };
        let Some(shape) = classify(&value) else {
            return Ok(IngestOutcome::notice(FeedNotice::info(
                FeedNoticeCode::UnsupportedMessage,
                "NBA ESPN daily body matches none of the eight recognized crawl shapes".to_owned(),
            )));
        };

        // Resolve the descriptor-derived identity (R1) before any extraction.
        let mut outcome = IngestOutcome::default();
        let declared = pending.as_ref().and_then(|p| Shape::from_family(&p.family));
        let family_matches = declared == Some(shape);
        if let Some(paired) = pending.as_ref()
            && !family_matches
        {
            // For the six body-keyed shapes this is a notice only: their identity
            // is body-derived and stays authoritative. For the two
            // descriptor-keyed shapes it is fatal to the body (handled below).
            outcome.notices.push(FeedNotice::info(
                FeedNoticeCode::StateInconsistent,
                format!(
                    "NBA ESPN daily descriptor declared family `{}` but the response classified as \
                     `{}`",
                    bounded_token(&paired.family),
                    shape.family()
                ),
            ));
        }
        let descriptor_entity = if shape.identity_comes_from_descriptor() {
            let Some(pending) = pending.as_ref() else {
                outcome.notices.push(FeedNotice::info(
                    FeedNoticeCode::StateInconsistent,
                    format!(
                        "NBA ESPN daily `{}` response has no paired request descriptor, so its \
                         athlete id is unrecoverable; skipped",
                        shape.family()
                    ),
                ));
                return Ok(outcome);
            };
            if !family_matches {
                outcome.notices.push(FeedNotice::info(
                    FeedNoticeCode::StateInconsistent,
                    format!(
                        "NBA ESPN daily `{}` response is paired with a descriptor for a different \
                         request family, so its athlete id is untrustworthy; skipped",
                        shape.family()
                    ),
                ));
                return Ok(outcome);
            }
            if !is_entity_id(&pending.entity_id) {
                outcome.notices.push(FeedNotice::info(
                    FeedNoticeCode::StateInconsistent,
                    format!(
                        "NBA ESPN daily `{}` descriptor carries a non-numeric entity id `{}`; \
                         skipped",
                        shape.family(),
                        bounded_token(&pending.entity_id)
                    ),
                ));
                return Ok(outcome);
            }
            pending.entity_id.clone()
        } else {
            String::new()
        };

        match shape {
            // Recognized NON-EVENT shape (ruling R2): the teams list is the crawl's
            // discovery input, the way an op-ack is on a WebSocket feed. It
            // classifies cleanly, emits no event, and raises NO notice.
            Shape::TeamsList => Ok(outcome),
            Shape::Standings => self.standings_events(record, &value, outcome),
            Shape::Roster => self.roster_events(record, &value, outcome),
            Shape::TeamStatistics => self.statistics_events(
                record,
                &value,
                outcome,
                TEAM_STATISTICS_SOURCE_ID,
                "team_id",
                "teams",
            ),
            Shape::TeamRecord => self.team_record_events(record, &value, outcome),
            Shape::AthleteStatistics => self.statistics_events(
                record,
                &value,
                outcome,
                ATHLETE_STATISTICS_SOURCE_ID,
                "athlete_id",
                "athletes",
            ),
            Shape::AthleteGamelog => {
                self.gamelog_events(record, &value, outcome, &descriptor_entity)
            }
            Shape::AthleteSplits => self.splits_events(record, &value, outcome, &descriptor_entity),
        }
    }

    /// Standings: one event per team entry, across every conference child.
    ///
    /// The seven catalog section 5.7 display fields are produced here and only here.
    /// `streak` contributes `streak_display_season` from its `displayValue`
    /// ("W3"); the six record-typed stats contribute
    /// `record_summary_{scope}_display` from their `summary` ("60-22"). Every
    /// other stat contributes its numeric `value` under its own snake-cased name.
    fn standings_events(
        &mut self,
        record: &RawRecord,
        value: &Value,
        mut outcome: IngestOutcome,
    ) -> Result<IngestOutcome, AdapterError> {
        let season_year = value
            .get("season")
            .and_then(|s| s.get("year"))
            .and_then(Value::as_i64);
        let children = value["children"].as_array().expect("children is an array");
        let mut rows = 0usize;
        for child in children {
            let group_id = id_to_string(child.get("id")).unwrap_or_default();
            let group_name = string_field(child, "name").unwrap_or_default();
            let Some(entries) = child
                .get("standings")
                .and_then(|s| s.get("entries"))
                .and_then(Value::as_array)
            else {
                continue;
            };
            for entry in entries {
                rows = bump(rows)?;
                let Some(team_id) = id_to_string(entry.get("team").and_then(|t| t.get("id")))
                else {
                    outcome.notices.push(skip_notice(
                        "NBA standings entry is missing a non-empty team id",
                    ));
                    continue;
                };
                let mut values = BTreeMap::from([
                    ("team_id".to_owned(), json!(team_id)),
                    ("group_id".to_owned(), json!(group_id)),
                    ("group_name".to_owned(), json!(group_name)),
                    ("event_time_basis".to_owned(), json!("receipt")),
                    ("event_time".to_owned(), json!(record.received_at)),
                ]);
                if let Some(season_year) = season_year {
                    values.insert("season_year".to_owned(), json!(season_year));
                }
                if let Some(abbreviation) =
                    string_field(entry.get("team").unwrap_or(&Value::Null), "abbreviation")
                {
                    values.insert("team_abbrev".to_owned(), json!(abbreviation));
                }
                if let Some(stats) = entry.get("stats").and_then(Value::as_array) {
                    for stat in stats {
                        insert_standings_stat(&mut values, stat);
                    }
                }
                let event = self.source_event(
                    record,
                    STANDINGS_SOURCE_ID,
                    record.received_at.clone(),
                    values,
                )?;
                outcome.events.push(event);
            }
        }
        Ok(outcome)
    }

    /// Roster: one event per rostered athlete. Also the crawl's discovery input
    /// for phase B, though that discovery happens in the capture loop, not here.
    fn roster_events(
        &mut self,
        record: &RawRecord,
        value: &Value,
        mut outcome: IngestOutcome,
    ) -> Result<IngestOutcome, AdapterError> {
        let team = value.get("team").unwrap_or(&Value::Null);
        let Some(team_id) = id_to_string(team.get("id")) else {
            return Ok(IngestOutcome::notice(skip_notice(
                "NBA roster response is missing a non-empty team id",
            )));
        };
        let team_abbrev = string_field(team, "abbreviation");
        // The roster endpoint is NOT season-parameterized and reports the season it
        // considers current, which in the offseason is the UPCOMING one while the
        // rest of the crawl is bound to `SEASON`. That divergence is carried, not
        // reconciled.
        let season = value.get("season");
        let season_year = season.and_then(|s| s.get("year")).and_then(Value::as_i64);
        let season_type = season.and_then(|s| s.get("type")).and_then(Value::as_i64);

        let athletes = value["athletes"].as_array().expect("athletes is an array");
        let mut rows = 0usize;
        for athlete in athletes {
            rows = bump(rows)?;
            let Some(athlete_id) = id_to_string(athlete.get("id")) else {
                outcome
                    .notices
                    .push(skip_notice("NBA roster athlete is missing a non-empty id"));
                continue;
            };
            let mut values = BTreeMap::from([
                ("athlete_id".to_owned(), json!(athlete_id)),
                ("team_id".to_owned(), json!(team_id)),
                ("event_time_basis".to_owned(), json!("receipt")),
                ("event_time".to_owned(), json!(record.received_at)),
            ]);
            if let Some(team_abbrev) = &team_abbrev {
                values.insert("team_abbrev".to_owned(), json!(team_abbrev));
            }
            if let Some(season_year) = season_year {
                values.insert("roster_season_year".to_owned(), json!(season_year));
            }
            if let Some(season_type) = season_type {
                values.insert("roster_season_type".to_owned(), json!(season_type));
            }
            for (field, key) in [
                ("display_name", "displayName"),
                ("first_name", "firstName"),
                ("last_name", "lastName"),
                ("short_name", "shortName"),
                ("jersey", "jersey"),
                ("date_of_birth", "dateOfBirth"),
                ("display_height", "displayHeight"),
                ("display_weight", "displayWeight"),
                ("slug", "slug"),
            ] {
                if let Some(text) = string_field(athlete, key) {
                    values.insert(field.to_owned(), json!(text));
                }
            }
            for (field, key) in [("height", "height"), ("weight", "weight"), ("age", "age")] {
                if let Some(number) = finite_number(athlete.get(key)) {
                    values.insert(field.to_owned(), json!(number));
                }
            }
            if let Some(years) = athlete
                .get("experience")
                .and_then(|e| e.get("years"))
                .and_then(Value::as_i64)
            {
                values.insert("experience_years".to_owned(), json!(years));
            }
            if let Some(position) = athlete.get("position") {
                if let Some(id) = id_to_string(position.get("id")) {
                    values.insert("position_id".to_owned(), json!(id));
                }
                for (field, key) in [
                    ("position_name", "name"),
                    ("position_abbrev", "abbreviation"),
                ] {
                    if let Some(text) = string_field(position, key) {
                        values.insert(field.to_owned(), json!(text));
                    }
                }
            }
            if let Some(status) = athlete.get("status") {
                for (field, key) in [("status_name", "name"), ("status_type", "type")] {
                    if let Some(text) = string_field(status, key) {
                        values.insert(field.to_owned(), json!(text));
                    }
                }
            }
            // The roster carries its own per-athlete injuries array; its length is
            // the honest "is this player carrying a listed injury" signal without
            // duplicating slice 1's injury stream.
            if let Some(injuries) = athlete.get("injuries").and_then(Value::as_array) {
                values.insert(
                    "listed_injury_count".to_owned(),
                    json!(u64::try_from(injuries.len()).unwrap_or(u64::MAX)),
                );
            }
            let event =
                self.source_event(record, ROSTER_SOURCE_ID, record.received_at.clone(), values)?;
            outcome.events.push(event);
        }
        Ok(outcome)
    }

    /// Team and athlete season statistics share one payload shape
    /// (`splits.categories[].stats[]`), so they share one extraction. The only
    /// differences are the source id, the entity value key, and the URL path
    /// segment the entity id is recovered from.
    ///
    /// Both bodies name their entity ONLY inside a `$ref` URL (the top-level
    /// `$ref`, `.../{segment}/{id}/statistics/0`), so the id is recovered from the
    /// path -- the same technique slice 1 uses to recover an athlete id from an
    /// injury entry's `links[].href`.
    ///
    /// One event per entity, carrying every stat as `{category}_{name}` plus the
    /// `_per_game` and `_rank` companions ESPN supplies. Category-prefixing is
    /// what keeps `defensive_blocks` and `general_blocks` distinct.
    fn statistics_events(
        &mut self,
        record: &RawRecord,
        value: &Value,
        mut outcome: IngestOutcome,
        source_id: &str,
        entity_key: &str,
        path_segment: &str,
    ) -> Result<IngestOutcome, AdapterError> {
        let Some(entity_id) = value
            .get("$ref")
            .and_then(Value::as_str)
            .and_then(|href| path_id_segment(href, path_segment))
        else {
            return Ok(IngestOutcome::notice(skip_notice(
                "NBA statistics response has no resolvable entity id in its $ref",
            )));
        };
        let mut values = BTreeMap::from([
            (entity_key.to_owned(), json!(entity_id)),
            ("season_year".to_owned(), json!(SEASON)),
            ("season_type".to_owned(), json!(SEASON_TYPE)),
            ("event_time_basis".to_owned(), json!("receipt")),
            ("event_time".to_owned(), json!(record.received_at)),
        ]);
        let splits = &value["splits"];
        if let Some(name) = string_field(splits, "name") {
            values.insert("split_name".to_owned(), json!(name));
        }
        let categories = splits["categories"]
            .as_array()
            .expect("splits.categories is an array");
        let mut rows = 0usize;
        for category in categories {
            let category_name = string_field(category, "name").unwrap_or_default();
            let Some(stats) = category.get("stats").and_then(Value::as_array) else {
                continue;
            };
            for stat in stats {
                rows = bump(rows)?;
                let Some(name) = string_field(stat, "name") else {
                    continue;
                };
                let stem = if category_name.is_empty() {
                    slug(&name)
                } else {
                    format!("{}_{}", slug(&category_name), slug(&name))
                };
                if let Some(number) = finite_number(stat.get("value")) {
                    values.insert(stem.clone(), json!(number));
                }
                if let Some(number) = finite_number(stat.get("perGameValue")) {
                    values.insert(format!("{stem}_per_game"), json!(number));
                }
                if let Some(rank) = stat.get("rank").and_then(Value::as_i64) {
                    values.insert(format!("{stem}_rank"), json!(rank));
                }
            }
        }
        guard_values(&values)?;
        let event = self.source_event(record, source_id, record.received_at.clone(), values)?;
        outcome.events.push(event);
        Ok(outcome)
    }

    /// Team records: ONE event per record row.
    ///
    /// The row key is the row `id`, NOT its `type`: a live 52-row collection
    /// carries `type` "total" twice (ids "0" and "900", summaries "56-26" and
    /// "0-1"), so `type` is not unique within a single response. `type` is carried
    /// as an ordinary value.
    fn team_record_events(
        &mut self,
        record: &RawRecord,
        value: &Value,
        mut outcome: IngestOutcome,
    ) -> Result<IngestOutcome, AdapterError> {
        let items = value["items"].as_array().expect("items is an array");
        // Pagination visibility: the crawl fetches page 1 only (see
        // `TEAM_RECORD_TEMPLATE`). When the body declares more than one page, or
        // more rows than page 1 carries, the extracted rows are a truncated view.
        // We do not follow the next page here -- the notice makes the truncation
        // visible and adoption decides whether pagination is worth building; the
        // page-1 rows are still extracted below exactly as before.
        let page_count = value.get("pageCount").and_then(Value::as_u64);
        let declared_count = value.get("count").and_then(Value::as_u64);
        if page_count.is_some_and(|pages| pages > 1)
            || declared_count.is_some_and(|count| count > items.len() as u64)
        {
            let page_count = page_count.map_or_else(|| "absent".to_owned(), |p| p.to_string());
            let declared_count =
                declared_count.map_or_else(|| "absent".to_owned(), |c| c.to_string());
            outcome.notices.push(FeedNotice::info(
                FeedNoticeCode::StateInconsistent,
                format!(
                    "NBA team record collection is paginated (pageCount {page_count}, count \
                     {declared_count}, page-1 rows {}); only page-1 rows are extracted",
                    items.len()
                ),
            ));
        }
        // The collection envelope names no team; each row's own `$ref` does.
        let mut rows = 0usize;
        for item in items {
            rows = bump(rows)?;
            let Some(team_id) = item
                .get("$ref")
                .and_then(Value::as_str)
                .and_then(|href| path_id_segment(href, "teams"))
            else {
                outcome.notices.push(skip_notice(
                    "NBA team record row has no resolvable team id in its $ref",
                ));
                continue;
            };
            let Some(record_id) = id_to_string(item.get("id")) else {
                outcome
                    .notices
                    .push(skip_notice("NBA team record row is missing a non-empty id"));
                continue;
            };
            let mut values = BTreeMap::from([
                ("team_id".to_owned(), json!(team_id)),
                ("record_id".to_owned(), json!(record_id)),
                ("season_year".to_owned(), json!(SEASON)),
                ("season_type".to_owned(), json!(SEASON_TYPE)),
                ("event_time_basis".to_owned(), json!("receipt")),
                ("event_time".to_owned(), json!(record.received_at)),
            ]);
            for (field, key) in [
                ("record_type", "type"),
                ("record_name", "name"),
                ("record_display_name", "displayName"),
                ("record_summary", "summary"),
            ] {
                if let Some(text) = string_field(item, key) {
                    values.insert(field.to_owned(), json!(text));
                }
            }
            if let Some(number) = finite_number(item.get("value")) {
                values.insert("record_value".to_owned(), json!(number));
            }
            if let Some(stats) = item.get("stats").and_then(Value::as_array) {
                for stat in stats {
                    let Some(name) = string_field(stat, "name") else {
                        continue;
                    };
                    if let Some(number) = finite_number(stat.get("value")) {
                        values.insert(slug(&name), json!(number));
                    }
                    // `streak` is the one record stat whose display form carries
                    // information the float does not (direction as a letter).
                    if name == "streak"
                        && let Some(display) = string_field(stat, "displayValue")
                    {
                        values.insert("streak_display".to_owned(), json!(display));
                    }
                    // `clincher` (catalog section 5.7 stem) is an enum letter
                    // (z/y/x) that lives only in `displayValue`; it has no numeric
                    // form. Offseason rows omit it entirely, and absent is fine.
                    if name == "clincher"
                        && let Some(display) = string_field(stat, "displayValue")
                    {
                        values.insert("clincher".to_owned(), json!(display));
                    }
                }
            }
            guard_values(&values)?;
            let event = self.source_event(
                record,
                TEAM_RECORD_SOURCE_ID,
                record.received_at.clone(),
                values,
            )?;
            outcome.events.push(event);
        }
        Ok(outcome)
    }

    /// Gamelog: ONE event per (athlete, game).
    ///
    /// The payload splits the data in two and the extraction joins them:
    /// `seasonTypes[].categories[].events[]` holds `{eventId, stats[]}` stat lines
    /// positionally aligned to the top-level `names[]` column list, while the
    /// top-level `events{}` MAP holds each game's metadata keyed by the same id.
    ///
    /// The athlete id comes from the paired descriptor (ruling R1); it appears
    /// nowhere in this body.
    fn gamelog_events(
        &mut self,
        record: &RawRecord,
        value: &Value,
        mut outcome: IngestOutcome,
        athlete_id: &str,
    ) -> Result<IngestOutcome, AdapterError> {
        let columns = column_ids(value);
        let metadata = value["events"].as_object().expect("events is an object");
        let season_types = value["seasonTypes"]
            .as_array()
            .expect("seasonTypes is an array");
        let mut rows = 0usize;
        for season_type in season_types {
            let season_type_label = string_field(season_type, "displayName").unwrap_or_default();
            let Some(categories) = season_type.get("categories").and_then(Value::as_array) else {
                continue;
            };
            for category in categories {
                let split_type = string_field(category, "splitType");
                let category_label = string_field(category, "displayName").unwrap_or_default();
                let Some(lines) = category.get("events").and_then(Value::as_array) else {
                    continue;
                };
                for line in lines {
                    rows = bump(rows)?;
                    let Some(event_id) = id_to_string(line.get("eventId")) else {
                        outcome.notices.push(skip_notice(
                            "NBA gamelog stat line is missing a non-empty eventId",
                        ));
                        continue;
                    };
                    let mut values = BTreeMap::from([
                        ("athlete_id".to_owned(), json!(athlete_id)),
                        ("athlete_id_basis".to_owned(), json!("request_descriptor")),
                        ("event_id".to_owned(), json!(event_id)),
                        ("season_year".to_owned(), json!(SEASON)),
                        ("season_type_label".to_owned(), json!(season_type_label)),
                        ("category_label".to_owned(), json!(category_label)),
                        ("event_time_basis".to_owned(), json!("receipt")),
                        ("event_time".to_owned(), json!(record.received_at)),
                    ]);
                    if let Some(split_type) = &split_type {
                        values.insert("split_type".to_owned(), json!(split_type));
                    }
                    if let Some(stats) = line.get("stats").and_then(Value::as_array) {
                        insert_columns(&mut values, &columns, stats);
                    }
                    if let Some(game) = metadata.get(&event_id) {
                        for (field, key) in [
                            ("game_date", "gameDate"),
                            ("at_vs", "atVs"),
                            ("game_result", "gameResult"),
                            ("score", "score"),
                            ("home_team_id", "homeTeamId"),
                            ("away_team_id", "awayTeamId"),
                            ("home_team_score", "homeTeamScore"),
                            ("away_team_score", "awayTeamScore"),
                        ] {
                            if let Some(text) = string_field(game, key) {
                                values.insert(field.to_owned(), json!(text));
                            }
                        }
                        if let Some(opponent) = id_to_string(
                            game.get("opponent").and_then(|opponent| opponent.get("id")),
                        ) {
                            values.insert("opponent_team_id".to_owned(), json!(opponent));
                        }
                        if let Some(abbrev) = string_field(
                            game.get("opponent").unwrap_or(&Value::Null),
                            "abbreviation",
                        ) {
                            values.insert("opponent_team_abbrev".to_owned(), json!(abbrev));
                        }
                    }
                    guard_values(&values)?;
                    let event = self.source_event(
                        record,
                        ATHLETE_GAMELOG_SOURCE_ID,
                        record.received_at.clone(),
                        values,
                    )?;
                    outcome.events.push(event);
                }
            }
        }
        Ok(outcome)
    }

    /// Splits: ONE event per (athlete, split-category, split-row).
    ///
    /// Rows are positionally aligned to the top-level `names[]` column list, the
    /// same convention as the gamelog. The athlete id comes from the paired
    /// descriptor (ruling R1); it appears nowhere in this body, and two low-minute
    /// players can produce byte-identical bodies.
    fn splits_events(
        &mut self,
        record: &RawRecord,
        value: &Value,
        mut outcome: IngestOutcome,
        athlete_id: &str,
    ) -> Result<IngestOutcome, AdapterError> {
        let columns = column_ids(value);
        let categories = value["splitCategories"]
            .as_array()
            .expect("splitCategories is an array");
        let mut rows = 0usize;
        for category in categories {
            let category_name = string_field(category, "name").unwrap_or_default();
            let Some(splits) = category.get("splits").and_then(Value::as_array) else {
                continue;
            };
            for split in splits {
                rows = bump(rows)?;
                let Some(display_name) = string_field(split, "displayName") else {
                    outcome.notices.push(skip_notice(
                        "NBA splits row is missing a non-empty displayName",
                    ));
                    continue;
                };
                let mut values = BTreeMap::from([
                    ("athlete_id".to_owned(), json!(athlete_id)),
                    ("athlete_id_basis".to_owned(), json!("request_descriptor")),
                    ("season_year".to_owned(), json!(SEASON)),
                    ("split_category".to_owned(), json!(category_name)),
                    ("split_display_name".to_owned(), json!(display_name)),
                    ("split_key".to_owned(), json!(slug(&display_name))),
                    ("event_time_basis".to_owned(), json!("receipt")),
                    ("event_time".to_owned(), json!(record.received_at)),
                ]);
                if let Some(abbreviation) = string_field(split, "abbreviation") {
                    values.insert("split_abbrev".to_owned(), json!(abbreviation));
                }
                if let Some(stats) = split.get("stats").and_then(Value::as_array) {
                    insert_columns(&mut values, &columns, stats);
                }
                guard_values(&values)?;
                let event = self.source_event(
                    record,
                    ATHLETE_SPLITS_SOURCE_ID,
                    record.received_at.clone(),
                    values,
                )?;
                outcome.events.push(event);
            }
        }
        Ok(outcome)
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

impl FeedAdapter for NbaEspnDailyAdapter {
    fn ingest(&mut self, record: &RawRecord) -> Result<IngestOutcome, AdapterError> {
        match self.guard.observe(record)? {
            GuardAction::Configuration(_) => Err(AdapterError::InvalidRecord(
                "NBA ESPN daily captures do not accept configuration records".to_owned(),
            )),
            GuardAction::Connected => {
                self.pending = None;
                Ok(IngestOutcome::notice(FeedNotice::info(
                    FeedNoticeCode::Connected,
                    "NBA ESPN daily crawl connected; awaiting phase A",
                )))
            }
            GuardAction::Disconnected => {
                let mut outcome = IngestOutcome::default();
                if let Some(orphan) = self.pending.take() {
                    outcome.notices.push(FeedNotice::info(
                        FeedNoticeCode::StateInconsistent,
                        format!(
                            "NBA ESPN daily crawl ended with an unpaired descriptor for family \
                             `{}` entity `{}`",
                            bounded_token(&orphan.family),
                            bounded_token(&orphan.entity_id)
                        ),
                    ));
                }
                outcome.notices.push(FeedNotice::info(
                    FeedNoticeCode::Disconnected,
                    "NBA ESPN daily crawl ended",
                ));
                Ok(outcome)
            }
            // Unlike slice 1, the outbound descriptor is NOT inert here: it carries
            // the instance identity for the two families whose bodies have none.
            GuardAction::Outbound => {
                let payload = record.payload.as_deref().unwrap_or_default();
                Ok(self.observe_descriptor(payload))
            }
            GuardAction::Failed(_) => Err(AdapterError::InvalidRecord(
                "NBA ESPN daily captures do not accept failure records".to_owned(),
            )),
            GuardAction::Inbound(payload) => self.ingest_payload(record, payload),
        }
    }
}

// --- Shared extraction helpers.

/// Fail closed once one response body produces an implausible number of rows.
fn bump(rows: usize) -> Result<usize, AdapterError> {
    let next = rows.saturating_add(1);
    if next > MAX_ROWS_PER_BODY {
        return Err(AdapterError::InvalidPayload(format!(
            "NBA ESPN daily response row count exceeded {MAX_ROWS_PER_BODY}"
        )));
    }
    Ok(next)
}

/// Fail closed once one event would carry an implausible number of values.
fn guard_values(values: &BTreeMap<String, Value>) -> Result<(), AdapterError> {
    if values.len() > MAX_VALUES_PER_EVENT {
        return Err(AdapterError::InvalidPayload(format!(
            "NBA ESPN daily event value count exceeded {MAX_VALUES_PER_EVENT}"
        )));
    }
    Ok(())
}

/// Column ids for the positional stat arrays the gamelog and splits payloads use.
///
/// PAIR RULE (binding, derived entirely from the payload's own `names[]`, never
/// from a hardcoded column list). ESPN ships several columns as hyphenated
/// made-attempted pairs in ONE cell (`"8-14"`), and the catalog requires each such
/// cell to become two numerics.
///
///  * If `names[i]` splits on `-` into exactly two parts and NEITHER part contains
///    whitespace, the column is a DECLARED pair and the two parts are the two
///    field ids (`fieldGoalsMade-fieldGoalsAttempted` -> `field_goals_made`,
///    `field_goals_attempted`).
///  * Otherwise the column is scalar with field id `slug(names[i])`. If its VALUE
///    then turns out to be a numeric pair, it is split into `{id}_made` and
///    `{id}_attempted` at extraction time.
///
/// The second branch exists because ESPN is not internally consistent: the splits
/// payload names one pair column `"Free Throws Made-Attempted Per Game"` -- a
/// display string, not two identifiers -- so splitting its NAME would produce the
/// nonsense id `attempted_per_game`. Deriving from the VALUE instead keeps that
/// column honest as `free_throws_made_attempted_per_game_made` / `_attempted`.
fn column_ids(value: &Value) -> Vec<ColumnId> {
    let Some(names) = value.get("names").and_then(Value::as_array) else {
        return Vec::new();
    };
    names
        .iter()
        .map(|name| {
            let name = name.as_str().unwrap_or("");
            let parts: Vec<&str> = name.split('-').collect();
            if parts.len() == 2
                && !parts[0].is_empty()
                && !parts[1].is_empty()
                && !parts.iter().any(|part| part.contains(char::is_whitespace))
            {
                ColumnId::DeclaredPair(slug(parts[0]), slug(parts[1]))
            } else {
                ColumnId::Scalar(slug(name))
            }
        })
        .collect()
}

/// A positional stat column's resolved field id(s).
enum ColumnId {
    Scalar(String),
    DeclaredPair(String, String),
}

/// Insert one positional stat row under its resolved column ids. A row longer or
/// shorter than the column list contributes only its overlapping prefix; the
/// surplus is dropped rather than guessed at.
fn insert_columns(values: &mut BTreeMap<String, Value>, columns: &[ColumnId], stats: &[Value]) {
    for (column, cell) in columns.iter().zip(stats) {
        let Some(text) = cell.as_str() else {
            continue;
        };
        match column {
            ColumnId::DeclaredPair(made, attempted) => {
                if let Some((left, right)) = numeric_pair(text) {
                    values.insert(made.clone(), json!(left));
                    values.insert(attempted.clone(), json!(right));
                }
            }
            ColumnId::Scalar(id) => {
                if id.is_empty() {
                    continue;
                }
                if let Some(number) = parse_finite(text) {
                    values.insert(id.clone(), json!(number));
                } else if let Some((left, right)) = numeric_pair(text) {
                    values.insert(format!("{id}_made"), json!(left));
                    values.insert(format!("{id}_attempted"), json!(right));
                }
            }
        }
    }
}

/// Split a `"made-attempted"` cell into two finite numbers. Requires exactly one
/// `-` with a finite number on each side, so a plain negative number or a date
/// never masquerades as a pair.
fn numeric_pair(text: &str) -> Option<(f64, f64)> {
    let (left, right) = text.split_once('-')?;
    Some((parse_finite(left)?, parse_finite(right)?))
}

/// Parse a decimal string into a finite `f64`. Non-finite values are rejected:
/// `serde_json` cannot serialize NaN or infinity, so admitting one would fail the
/// whole normalized-output write rather than one field.
fn parse_finite(text: &str) -> Option<f64> {
    let parsed: f64 = text.trim().parse().ok()?;
    parsed.is_finite().then_some(parsed)
}

/// Read a JSON number as a finite `f64`, rejecting non-finite values for the same
/// serialization reason.
fn finite_number(value: Option<&Value>) -> Option<f64> {
    let parsed = value?.as_f64()?;
    parsed.is_finite().then_some(parsed)
}

/// Insert one standings stat under the catalog's field ids.
///
/// The six record-typed stats and `streak` are the seven captured display fields
/// of catalog section 5.7 and are named exactly as the catalog names them. Every other
/// stat contributes its numeric value under its own snake-cased name.
fn insert_standings_stat(values: &mut BTreeMap<String, Value>, stat: &Value) {
    let Some(name) = string_field(stat, "name") else {
        return;
    };
    if name == "streak"
        && let Some(display) = string_field(stat, "displayValue")
    {
        values.insert("streak_display_season".to_owned(), json!(display));
    }
    // `clincher` (catalog section 5.7 stem) is an enum letter (z/y/x) carried
    // only in `displayValue`, with no meaningful numeric form. Offseason bodies
    // omit it and absent is fine -- not-emitted. The letter must win over any
    // numeric `value` the row also carries, so this branch RETURNS: letting the
    // numeric fallthrough run would overwrite the letter with a number under
    // the same `clincher` key.
    if name == "clincher" {
        if let Some(display) = string_field(stat, "displayValue") {
            values.insert("clincher".to_owned(), json!(display));
        }
        return;
    }
    if let Some(scope) = stat
        .get("type")
        .and_then(Value::as_str)
        .and_then(standings_record_scope)
        && let Some(summary) = string_field(stat, "summary")
    {
        values.insert(format!("record_summary_{scope}_display"), json!(summary));
        return;
    }
    if let Some(number) = finite_number(stat.get("value")) {
        values.insert(slug(&name), json!(number));
    }
}

/// Map ESPN's standings record `type` token onto the catalog's scope token. The
/// closed list is the catalog's own six scopes; any other record type in the
/// standings entry is not a catalog section 5.7 display field and falls through to the
/// numeric path.
fn standings_record_scope(record_type: &str) -> Option<&'static str> {
    Some(match record_type {
        "total" => "overall",
        "home" => "home",
        "road" => "road",
        "vsdiv" => "vs_division",
        "vsconf" => "vs_conference",
        "lasttengames" => "last10",
        _ => return None,
    })
}

fn skip_notice(detail: &'static str) -> FeedNotice {
    FeedNotice::info(FeedNoticeCode::UnsupportedMessage, detail)
}

/// Read one optional non-empty string field.
fn string_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
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

/// True when a descriptor `entity_id` is a usable ESPN entity id: a non-empty
/// bounded run of ASCII digits. This is the fail-closed check ruling R1 requires
/// before a descriptor may key a gamelog or splits body.
fn is_entity_id(candidate: &str) -> bool {
    !candidate.is_empty()
        && candidate.len() <= 32
        && candidate.bytes().all(|byte| byte.is_ascii_digit())
}

/// Extract the digits following a `/{segment}/` path element in a URL
/// (`.../teams/2/statistics/0` with segment `teams` yields `2`). Generalizes the
/// slice-1 `/id/<digits>/` recovery to a named segment.
fn path_id_segment(href: &str, segment: &str) -> Option<String> {
    let needle = format!("/{segment}/");
    let start = href.find(&needle)? + needle.len();
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

/// Deterministic field-id slug: ASCII-lowercase, every run of non-alphanumeric
/// characters collapsed to one `_`, no leading or trailing `_`. camelCase word
/// boundaries are also split, so `fieldGoalsMade` becomes `field_goals_made`.
fn slug(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 8);
    let mut previous_lower_or_digit = false;
    for character in name.chars() {
        if character.is_ascii_alphanumeric() {
            if character.is_ascii_uppercase() && previous_lower_or_digit {
                out.push('_');
            }
            out.push(character.to_ascii_lowercase());
            previous_lower_or_digit = !character.is_ascii_uppercase();
        } else {
            if !out.ends_with('_') && !out.is_empty() {
                out.push('_');
            }
            previous_lower_or_digit = false;
        }
    }
    while out.ends_with('_') {
        out.pop();
    }
    out
}

/// A prefixed, bounded quote of an untrusted provider string. The bound is the
/// same [`MAX_QUOTED_TOKEN_CHARS`](crate::adapter::MAX_QUOTED_TOKEN_CHARS) the
/// crate's own [`bounded_token`] applies; only the prefixing is local.
fn bounded_detail(prefix: &str, value: &str) -> String {
    format!("{prefix}: {}", bounded_token(value))
}

#[cfg(test)]
mod tests {
    use super::{
        ATHLETE_GAMELOG_TEMPLATE, ATHLETE_SPLITS_TEMPLATE, ATHLETE_STATISTICS_TEMPLATE,
        MAX_ATHLETES, MAX_TEAMS, NbaEspnDailyAdapter, REQUEST_PACING, ROSTER_TEMPLATE, SEASON,
        SEASON_TYPE, STANDINGS_URL, Shape, TEAM_RECORD_TEMPLATE, TEAM_STATISTICS_TEMPLATE,
        TEAMS_URL, classify, classify_series, outbound_descriptor, render, slug,
    };
    use crate::{CaptureStream, FeedAdapter, FeedNoticeCode, IngestOutcome, RawRecord, RecordKind};
    use serde_json::{Value, json};

    fn adapter() -> NbaEspnDailyAdapter {
        NbaEspnDailyAdapter::new("test-epoch").expect("adapter constructs")
    }

    fn record(ordinal: u64, epoch: u64, kind: RecordKind, payload: Option<&str>) -> RawRecord {
        RawRecord {
            schema_version: 1,
            provider: crate::Provider::NbaEspnDaily,
            stream: CaptureStream::NbaEspnDaily,
            connection_epoch: epoch,
            ordinal,
            received_at: "2026-08-30T12:00:01Z".to_owned(),
            capture_elapsed_ns: None,
            connect_started_elapsed_ns: None,
            kind,
            payload: payload.map(str::to_owned),
        }
    }

    fn connected(ordinal: u64) -> RawRecord {
        record(ordinal, 1, RecordKind::Connected, None)
    }

    fn inbound(ordinal: u64, payload: &str) -> RawRecord {
        record(ordinal, 1, RecordKind::InboundText, Some(payload))
    }

    fn descriptor(ordinal: u64, family: &str, entity_id: &str) -> RawRecord {
        record(
            ordinal,
            1,
            RecordKind::OutboundText,
            Some(&outbound_descriptor(
                "https://example.invalid/x",
                "b",
                family,
                entity_id,
                0,
            )),
        )
    }

    // --- Minimal synthetic fixtures. Shapes and field paths mirror the real
    // 2026-08-30 fetches exactly; the giant bodies themselves are not committed.

    const TEAMS_LIST: &str = r#"{"sports":[{"id":"40","leagues":[{"teams":[{"team":{"id":"1"}},{"team":{"id":"2"}}]}]}]}"#;

    const STANDINGS: &str = r#"{"id":"7","name":"National Basketball Association","season":{"year":2026},
        "children":[{"id":"5","name":"Eastern Conference","standings":{"entries":[
          {"team":{"id":"8","abbreviation":"DET"},"stats":[
            {"name":"wins","type":"wins","value":60.0,"displayValue":"60"},
            {"name":"streak","type":"streak","value":3.0,"displayValue":"W3"},
            {"name":"overall","type":"total","summary":"60-22","displayValue":"60-22"},
            {"name":"Home","type":"home","summary":"31-9"},
            {"name":"Road","type":"road","summary":"28-13"},
            {"name":"vs. Div.","type":"vsdiv","summary":"12-4"},
            {"name":"vs. Conf.","type":"vsconf","summary":"39-13"},
            {"name":"Last Ten Games","type":"lasttengames","summary":"8-2"}]}]}}]}"#;

    const ROSTER: &str = r#"{"timestamp":"2026-08-31T06:24:50Z","status":"success",
        "season":{"year":2027,"type":1},
        "athletes":[{"id":"5142621","displayName":"Chris Cenac Jr.","jersey":"12","height":83.0,
          "weight":240.0,"age":19,"position":{"id":"7","name":"Forward","abbreviation":"F"},
          "status":{"id":"1","name":"Active","type":"active"},"injuries":[],"experience":{"years":0}}],
        "team":{"id":"2","abbreviation":"BOS"}}"#;

    const TEAM_STATISTICS: &str = r#"{"$ref":"http://sports.core.api.espn.com/v2/sports/basketball/leagues/nba/seasons/2026/types/2/teams/2/statistics/0?lang=en",
        "team":{"$ref":"http://sports.core.api.espn.com/v2/sports/basketball/leagues/nba/seasons/2026/teams/2"},
        "splits":{"id":"0","name":"All Splits","categories":[
          {"name":"defensive","stats":[{"name":"blocks","value":410.0,"perGameValue":5.0,"rank":2}]},
          {"name":"general","stats":[{"name":"blocks","value":9.0}]}]}}"#;

    const ATHLETE_STATISTICS: &str = r#"{"$ref":"http://sports.core.api.espn.com/v2/sports/basketball/leagues/nba/seasons/2026/types/2/athletes/4433134/statistics/0?lang=en",
        "athlete":{"$ref":"http://sports.core.api.espn.com/v2/sports/basketball/leagues/nba/seasons/2026/athletes/4433134"},
        "splits":{"id":"0","name":"All Splits","categories":[
          {"name":"defensive","stats":[{"name":"blocks","value":116.0,"rank":6}]}]}}"#;

    const TEAM_RECORD: &str = r#"{"count":2,"pageIndex":1,"pageSize":100,"pageCount":1,"items":[
        {"$ref":"http://sports.core.api.espn.com/v2/sports/basketball/leagues/nba/seasons/2026/types/2/teams/2/records/0",
         "id":"0","name":"overall","type":"total","summary":"56-26","value":0.6829268292682927,
         "stats":[{"name":"wins","value":56.0},{"name":"streak","value":-1.0,"displayValue":"L1"}]},
        {"$ref":"http://sports.core.api.espn.com/v2/sports/basketball/leagues/nba/seasons/2026/types/2/teams/2/records/900",
         "id":"900","name":"overall","type":"total","summary":"0-1","value":0.0,
         "stats":[{"name":"wins","value":0.0}]}]}"#;

    // A page-1-shaped body from a multi-page collection: `pageCount` > 1 and
    // `count` far above the rows actually carried on this page.
    const TEAM_RECORD_PAGED: &str = r#"{"count":120,"pageIndex":1,"pageSize":100,"pageCount":2,"items":[
        {"$ref":"http://sports.core.api.espn.com/v2/sports/basketball/leagues/nba/seasons/2026/types/2/teams/2/records/0",
         "id":"0","name":"overall","type":"total","summary":"56-26","value":0.6829268292682927,
         "stats":[{"name":"wins","value":56.0}]}]}"#;

    // A team-record row carrying a clincher stat whose letter lives only in
    // `displayValue` ("z" = clinched best record), plus a standings entry with the
    // same clincher stat, to exercise both extraction paths.
    const TEAM_RECORD_CLINCHER: &str = r#"{"count":1,"pageIndex":1,"pageSize":100,"pageCount":1,"items":[
        {"$ref":"http://sports.core.api.espn.com/v2/sports/basketball/leagues/nba/seasons/2026/types/2/teams/2/records/0",
         "id":"0","name":"overall","type":"total","summary":"56-26","value":0.68,
         "stats":[{"name":"wins","value":56.0},{"name":"clincher","value":0.0,"displayValue":"z"}]}]}"#;

    const STANDINGS_CLINCHER: &str = r#"{"id":"7","name":"National Basketball Association","season":{"year":2026},
        "children":[{"id":"5","name":"Eastern Conference","standings":{"entries":[
          {"team":{"id":"8","abbreviation":"DET"},"stats":[
            {"name":"wins","type":"wins","value":60.0,"displayValue":"60"},
            {"name":"clincher","type":"clincher","value":0.0,"displayValue":"z"}]}]}}]}"#;

    const GAMELOG: &str = r#"{"filters":[{"name":"season","value":"2026"}],
        "labels":["MIN","FG","PTS"],
        "names":["minutes","fieldGoalsMade-fieldGoalsAttempted","points"],
        "displayNames":["Minutes","Field Goals Made-Attempted","Points"],
        "events":{"401869384":{"id":"401869384","atVs":"@","gameDate":"2026-05-03T23:30:00.000+00:00",
          "score":"114-102","homeTeamId":"5","awayTeamId":"28","homeTeamScore":"114",
          "awayTeamScore":"102","gameResult":"L","opponent":{"id":"5","abbreviation":"CLE"}}},
        "seasonTypes":[{"displayName":"2025-26 Postseason","categories":[
          {"displayName":"Conference Quarterfinals","type":"event","splitType":"conference_quarterfinals",
           "events":[{"eventId":"401869384","stats":["37","8-14","24"]}]}]}],
        "glossary":[]}"#;

    const SPLITS: &str = r#"{"filters":[{"name":"season","value":"2026"}],
        "displayName":"2025-26 Splits",
        "labels":["GP","FG","FT","PTS"],
        "names":["gamesPlayed","avgFieldGoalsMade-avgFieldGoalsAttempted",
                 "Free Throws Made-Attempted Per Game","avgPoints"],
        "displayNames":["Games Played","FG","FT","Points"],
        "descriptions":[],
        "splitCategories":[{"name":"split","displayName":"split","splits":[
          {"displayName":"All Splits","abbreviation":"Total","stats":["80","7.1-14.0","3.0-3.7","18.1"]},
          {"displayName":"Home","abbreviation":"Home","stats":["40","7.5-14.2","3.1-3.8","18.9"]}]}]}"#;

    fn ingest(adapter: &mut NbaEspnDailyAdapter, records: &[RawRecord]) -> IngestOutcome {
        let mut combined = IngestOutcome::default();
        for record in records {
            let outcome = adapter.ingest(record).expect("record ingests");
            combined.events.extend(outcome.events);
            combined.notices.extend(outcome.notices);
        }
        combined
    }

    fn value_of<'a>(outcome: &'a IngestOutcome, index: usize, key: &str) -> Option<&'a Value> {
        outcome.events[index].values.get(key)
    }

    // --- Compiled-constant pins.

    #[test]
    fn templates_render_the_probe_verified_urls() {
        // Every one of these exact URLs was probe-verified HTTP 200 over HTTPS on
        // 2026-08-30 with the allowlisted UA. A silent drift here re-404s or
        // re-403s the whole crawl, so they are pinned.
        assert_eq!(
            render(ROSTER_TEMPLATE, "2", ""),
            "https://site.api.espn.com/apis/site/v2/sports/basketball/nba/teams/2/roster"
        );
        assert_eq!(
            render(TEAM_STATISTICS_TEMPLATE, "2", ""),
            "https://sports.core.api.espn.com/v2/sports/basketball/leagues/nba/seasons/2026/types/2/teams/2/statistics"
        );
        assert_eq!(
            render(TEAM_RECORD_TEMPLATE, "2", ""),
            "https://sports.core.api.espn.com/v2/sports/basketball/leagues/nba/seasons/2026/types/2/teams/2/records?limit=100"
        );
        assert_eq!(
            render(ATHLETE_STATISTICS_TEMPLATE, "", "4433134"),
            "https://sports.core.api.espn.com/v2/sports/basketball/leagues/nba/seasons/2026/types/2/athletes/4433134/statistics"
        );
        assert_eq!(
            render(ATHLETE_GAMELOG_TEMPLATE, "", "4433134"),
            "https://site.web.api.espn.com/apis/common/v3/sports/basketball/nba/athletes/4433134/gamelog?season=2026"
        );
        assert_eq!(
            render(ATHLETE_SPLITS_TEMPLATE, "", "4433134"),
            "https://site.web.api.espn.com/apis/common/v3/sports/basketball/nba/athletes/4433134/splits?season=2026"
        );
        // Every compiled endpoint stays on TLS even though ESPN's own `$ref`
        // links advertise the core API over plain http.
        for url in [TEAMS_URL, STANDINGS_URL] {
            assert!(url.starts_with("https://"), "{url} must be https");
        }
        for template in [
            ROSTER_TEMPLATE,
            TEAM_STATISTICS_TEMPLATE,
            TEAM_RECORD_TEMPLATE,
            ATHLETE_STATISTICS_TEMPLATE,
            ATHLETE_GAMELOG_TEMPLATE,
            ATHLETE_SPLITS_TEMPLATE,
        ] {
            assert!(template.starts_with("https://"), "{template} must be https");
        }
        assert_eq!((SEASON, SEASON_TYPE), ("2026", "2"));
        assert_eq!((MAX_TEAMS, MAX_ATHLETES), (40, 800));
        assert_eq!(REQUEST_PACING.as_millis(), 150);
    }

    #[test]
    fn team_record_template_requests_one_page() {
        // Not cosmetic: the default page size is 25 and a real team carries 52
        // record rows, so dropping `limit=100` silently truncates the collection.
        assert!(TEAM_RECORD_TEMPLATE.ends_with("/records?limit=100"));
    }

    #[test]
    fn outbound_descriptor_is_the_exact_shape() {
        assert_eq!(
            outbound_descriptor(TEAMS_URL, "a", "teams-list", "", 0),
            format!(
                r#"{{"request":"{TEAMS_URL}","phase":"a","family":"teams-list","entity_id":"","index":0}}"#
            )
        );
    }

    // --- Classification: the closed eight-shape list.

    #[test]
    fn every_shape_classifies_and_the_family_tokens_round_trip() {
        for (payload, expected) in [
            (TEAMS_LIST, Shape::TeamsList),
            (STANDINGS, Shape::Standings),
            (ROSTER, Shape::Roster),
            (TEAM_STATISTICS, Shape::TeamStatistics),
            (TEAM_RECORD, Shape::TeamRecord),
            (ATHLETE_STATISTICS, Shape::AthleteStatistics),
            (GAMELOG, Shape::AthleteGamelog),
            (SPLITS, Shape::AthleteSplits),
        ] {
            let value: Value = serde_json::from_str(payload).expect("fixture parses");
            assert_eq!(classify(&value), Some(expected), "{}", expected.family());
            assert_eq!(
                classify_series(payload),
                format!("nba_espn_daily:{}", expected.family())
            );
            assert_eq!(Shape::from_family(expected.family()), Some(expected));
        }
    }

    #[test]
    fn events_array_and_events_object_are_separated_strictly_by_json_type() {
        // The daily gamelog carries `events` as an OBJECT keyed by event id; slice
        // 1's scoreboard carries `events` as an ARRAY. The discriminator is the
        // JSON TYPE alone, so a scoreboard body misrouted onto this stream is NOT
        // silently read as a gamelog.
        let gamelog: Value = serde_json::from_str(GAMELOG).expect("gamelog parses");
        assert!(gamelog["events"].is_object());
        assert_eq!(classify(&gamelog), Some(Shape::AthleteGamelog));

        let scoreboard = json!({
            "events": [{"id": "401902644"}],
            "seasonTypes": [],
            "glossary": []
        });
        assert!(scoreboard["events"].is_array());
        assert_eq!(classify(&scoreboard), None);
        assert_eq!(
            classify_series(&scoreboard.to_string()),
            super::UNKNOWN_SERIES
        );
    }

    #[test]
    fn classify_series_never_fails_on_odd_bodies() {
        assert_eq!(classify_series("not json at all"), super::UNKNOWN_SERIES);
        assert_eq!(classify_series(r#"{"other":true}"#), super::UNKNOWN_SERIES);
        assert_eq!(classify_series("[]"), super::UNKNOWN_SERIES);
    }

    // --- Ruling R2: teams-list is a silent, recognized non-event shape.

    #[test]
    fn teams_list_emits_nothing_and_raises_no_notice() {
        let mut feed = adapter();
        feed.ingest(&connected(1)).expect("connects");
        let outcome = ingest(
            &mut feed,
            &[descriptor(2, "teams-list", ""), inbound(3, TEAMS_LIST)],
        );
        assert!(outcome.events.is_empty(), "{outcome:?}");
        assert!(outcome.notices.is_empty(), "{outcome:?}");
    }

    // --- Per-shape extraction.

    #[test]
    fn standings_emits_the_seven_catalog_display_fields_per_team() {
        let mut feed = adapter();
        feed.ingest(&connected(1)).expect("connects");
        let outcome = ingest(
            &mut feed,
            &[descriptor(2, "standings", ""), inbound(3, STANDINGS)],
        );
        assert_eq!(outcome.events.len(), 1);
        assert_eq!(outcome.events[0].source_id, "nba.espn.daily.standings");
        assert_eq!(value_of(&outcome, 0, "team_id"), Some(&json!("8")));
        assert_eq!(
            value_of(&outcome, 0, "group_name"),
            Some(&json!("Eastern Conference"))
        );
        assert_eq!(value_of(&outcome, 0, "season_year"), Some(&json!(2026)));
        // The seven captured display fields of catalog section 5.7, by their catalog ids.
        assert_eq!(
            value_of(&outcome, 0, "streak_display_season"),
            Some(&json!("W3"))
        );
        for (field, expected) in [
            ("record_summary_overall_display", "60-22"),
            ("record_summary_home_display", "31-9"),
            ("record_summary_road_display", "28-13"),
            ("record_summary_vs_division_display", "12-4"),
            ("record_summary_vs_conference_display", "39-13"),
            ("record_summary_last10_display", "8-2"),
        ] {
            assert_eq!(
                value_of(&outcome, 0, field),
                Some(&json!(expected)),
                "{field}"
            );
        }
        // A plain numeric stat still lands as a number under its slug.
        assert_eq!(value_of(&outcome, 0, "wins"), Some(&json!(60.0)));
    }

    #[test]
    fn roster_emits_one_event_per_athlete_and_carries_its_own_season() {
        let mut feed = adapter();
        feed.ingest(&connected(1)).expect("connects");
        let outcome = ingest(
            &mut feed,
            &[descriptor(2, "roster", "2"), inbound(3, ROSTER)],
        );
        assert_eq!(outcome.events.len(), 1);
        assert_eq!(outcome.events[0].source_id, "nba.espn.daily.roster");
        assert_eq!(value_of(&outcome, 0, "athlete_id"), Some(&json!("5142621")));
        assert_eq!(value_of(&outcome, 0, "team_id"), Some(&json!("2")));
        assert_eq!(value_of(&outcome, 0, "position_abbrev"), Some(&json!("F")));
        assert_eq!(value_of(&outcome, 0, "status_type"), Some(&json!("active")));
        assert_eq!(value_of(&outcome, 0, "experience_years"), Some(&json!(0)));
        assert_eq!(
            value_of(&outcome, 0, "listed_injury_count"),
            Some(&json!(0))
        );
        // The roster endpoint is not season-parameterized: it reports the upcoming
        // season while the rest of the crawl is bound to SEASON. Carried, not
        // reconciled.
        assert_eq!(
            value_of(&outcome, 0, "roster_season_year"),
            Some(&json!(2027))
        );
    }

    #[test]
    fn statistics_recover_the_entity_id_from_the_ref_and_prefix_by_category() {
        let mut feed = adapter();
        feed.ingest(&connected(1)).expect("connects");
        let outcome = ingest(
            &mut feed,
            &[
                descriptor(2, "team-statistics", "2"),
                inbound(3, TEAM_STATISTICS),
                descriptor(4, "athlete-statistics", "4433134"),
                inbound(5, ATHLETE_STATISTICS),
            ],
        );
        assert_eq!(outcome.events.len(), 2);
        assert_eq!(
            outcome.events[0].source_id,
            "nba.espn.daily.team-statistics"
        );
        assert_eq!(value_of(&outcome, 0, "team_id"), Some(&json!("2")));
        // Category prefixing is what keeps the two `blocks` stats distinct.
        assert_eq!(
            value_of(&outcome, 0, "defensive_blocks"),
            Some(&json!(410.0))
        );
        assert_eq!(
            value_of(&outcome, 0, "defensive_blocks_per_game"),
            Some(&json!(5.0))
        );
        assert_eq!(
            value_of(&outcome, 0, "defensive_blocks_rank"),
            Some(&json!(2))
        );
        assert_eq!(value_of(&outcome, 0, "general_blocks"), Some(&json!(9.0)));

        assert_eq!(
            outcome.events[1].source_id,
            "nba.espn.daily.athlete-statistics"
        );
        assert_eq!(value_of(&outcome, 1, "athlete_id"), Some(&json!("4433134")));
        assert_eq!(
            value_of(&outcome, 1, "defensive_blocks"),
            Some(&json!(116.0))
        );
    }

    #[test]
    fn team_record_keys_rows_by_id_because_type_repeats() {
        // A live 52-row collection carries `type` "total" twice (ids 0 and 900).
        // Keying by `type` would collapse two genuinely different rows.
        let mut feed = adapter();
        feed.ingest(&connected(1)).expect("connects");
        let outcome = ingest(
            &mut feed,
            &[descriptor(2, "team-record", "2"), inbound(3, TEAM_RECORD)],
        );
        assert_eq!(outcome.events.len(), 2);
        assert_eq!(outcome.events[0].source_id, "nba.espn.daily.team-record");
        assert_eq!(value_of(&outcome, 0, "record_id"), Some(&json!("0")));
        assert_eq!(value_of(&outcome, 1, "record_id"), Some(&json!("900")));
        assert_eq!(value_of(&outcome, 0, "record_type"), Some(&json!("total")));
        assert_eq!(value_of(&outcome, 1, "record_type"), Some(&json!("total")));
        assert_eq!(
            value_of(&outcome, 0, "record_summary"),
            Some(&json!("56-26"))
        );
        assert_eq!(value_of(&outcome, 1, "record_summary"), Some(&json!("0-1")));
        assert_eq!(value_of(&outcome, 0, "team_id"), Some(&json!("2")));
        assert_eq!(value_of(&outcome, 0, "streak_display"), Some(&json!("L1")));
    }

    #[test]
    fn team_record_multi_page_body_raises_a_pagination_notice_and_still_extracts_page_1() {
        let mut feed = adapter();
        feed.ingest(&connected(1)).expect("connects");
        let outcome = ingest(
            &mut feed,
            &[
                descriptor(2, "team-record", "2"),
                inbound(3, TEAM_RECORD_PAGED),
            ],
        );
        // Page-1 rows are still extracted exactly as before.
        assert_eq!(outcome.events.len(), 1);
        assert_eq!(value_of(&outcome, 0, "record_id"), Some(&json!("0")));
        // The truncation is made visible by exactly one StateInconsistent notice
        // that names the declared counts and the page-1 row count.
        let notices: Vec<_> = outcome
            .notices
            .iter()
            .filter(|notice| notice.code == FeedNoticeCode::StateInconsistent)
            .collect();
        assert_eq!(notices.len(), 1, "{outcome:?}");
        let detail = &notices[0].detail;
        assert!(detail.contains("pageCount 2"), "{detail}");
        assert!(detail.contains("count 120"), "{detail}");
        assert!(detail.contains("page-1 rows 1"), "{detail}");
    }

    #[test]
    fn clincher_display_value_is_captured_from_both_extraction_paths() {
        // Team-record path.
        let mut feed = adapter();
        feed.ingest(&connected(1)).expect("connects");
        let outcome = ingest(
            &mut feed,
            &[
                descriptor(2, "team-record", "2"),
                inbound(3, TEAM_RECORD_CLINCHER),
            ],
        );
        assert_eq!(outcome.events.len(), 1);
        assert_eq!(value_of(&outcome, 0, "clincher"), Some(&json!("z")));

        // Standings path.
        let mut feed = adapter();
        feed.ingest(&connected(1)).expect("connects");
        let outcome = ingest(
            &mut feed,
            &[
                descriptor(2, "standings", ""),
                inbound(3, STANDINGS_CLINCHER),
            ],
        );
        assert_eq!(outcome.events.len(), 1);
        assert_eq!(value_of(&outcome, 0, "clincher"), Some(&json!("z")));
    }

    #[test]
    fn gamelog_joins_stat_lines_to_metadata_and_splits_declared_pairs() {
        let mut feed = adapter();
        feed.ingest(&connected(1)).expect("connects");
        let outcome = ingest(
            &mut feed,
            &[
                descriptor(2, "athlete-gamelog", "4433134"),
                inbound(3, GAMELOG),
            ],
        );
        assert_eq!(outcome.events.len(), 1);
        assert_eq!(
            outcome.events[0].source_id,
            "nba.espn.daily.athlete-gamelog"
        );
        // Identity from the descriptor (ruling R1), labeled as such.
        assert_eq!(value_of(&outcome, 0, "athlete_id"), Some(&json!("4433134")));
        assert_eq!(
            value_of(&outcome, 0, "athlete_id_basis"),
            Some(&json!("request_descriptor"))
        );
        assert_eq!(value_of(&outcome, 0, "event_id"), Some(&json!("401869384")));
        // Metadata joined from the top-level `events` map by event id.
        assert_eq!(value_of(&outcome, 0, "game_result"), Some(&json!("L")));
        assert_eq!(
            value_of(&outcome, 0, "opponent_team_abbrev"),
            Some(&json!("CLE"))
        );
        assert_eq!(
            value_of(&outcome, 0, "game_date"),
            Some(&json!("2026-05-03T23:30:00.000+00:00"))
        );
        // Positional columns: a scalar and a declared made-attempted pair.
        assert_eq!(value_of(&outcome, 0, "minutes"), Some(&json!(37.0)));
        assert_eq!(value_of(&outcome, 0, "field_goals_made"), Some(&json!(8.0)));
        assert_eq!(
            value_of(&outcome, 0, "field_goals_attempted"),
            Some(&json!(14.0))
        );
        assert_eq!(value_of(&outcome, 0, "points"), Some(&json!(24.0)));
        assert_eq!(
            value_of(&outcome, 0, "split_type"),
            Some(&json!("conference_quarterfinals"))
        );
    }

    #[test]
    fn splits_emit_one_event_per_row_and_handle_the_display_named_pair_column() {
        let mut feed = adapter();
        feed.ingest(&connected(1)).expect("connects");
        let outcome = ingest(
            &mut feed,
            &[
                descriptor(2, "athlete-splits", "4433134"),
                inbound(3, SPLITS),
            ],
        );
        assert_eq!(outcome.events.len(), 2);
        assert_eq!(outcome.events[0].source_id, "nba.espn.daily.athlete-splits");
        assert_eq!(value_of(&outcome, 0, "athlete_id"), Some(&json!("4433134")));
        assert_eq!(
            value_of(&outcome, 0, "split_category"),
            Some(&json!("split"))
        );
        assert_eq!(
            value_of(&outcome, 0, "split_display_name"),
            Some(&json!("All Splits"))
        );
        assert_eq!(
            value_of(&outcome, 0, "split_key"),
            Some(&json!("all_splits"))
        );
        assert_eq!(value_of(&outcome, 1, "split_key"), Some(&json!("home")));
        assert_eq!(value_of(&outcome, 0, "games_played"), Some(&json!(80.0)));
        // Declared pair: both names are identifiers, so both become field ids.
        assert_eq!(
            value_of(&outcome, 0, "avg_field_goals_made"),
            Some(&json!(7.1))
        );
        assert_eq!(
            value_of(&outcome, 0, "avg_field_goals_attempted"),
            Some(&json!(14.0))
        );
        // ESPN names this pair column with a DISPLAY string, so the pair rule
        // falls back to deriving the two ids from the value.
        assert_eq!(
            value_of(&outcome, 0, "free_throws_made_attempted_per_game_made"),
            Some(&json!(3.0))
        );
        assert_eq!(
            value_of(&outcome, 0, "free_throws_made_attempted_per_game_attempted"),
            Some(&json!(3.7))
        );
    }

    #[test]
    fn identical_split_rows_both_emit_because_this_stream_never_dedups() {
        // Two low-minute athletes can produce byte-identical splits bodies. A
        // dedup map would silently drop the second; the crawl is a one-pass
        // snapshot, so both are emitted.
        let mut feed = adapter();
        feed.ingest(&connected(1)).expect("connects");
        let outcome = ingest(
            &mut feed,
            &[
                descriptor(2, "athlete-splits", "111"),
                inbound(3, SPLITS),
                descriptor(4, "athlete-splits", "222"),
                inbound(5, SPLITS),
            ],
        );
        assert_eq!(outcome.events.len(), 4);
        assert_eq!(value_of(&outcome, 0, "athlete_id"), Some(&json!("111")));
        assert_eq!(value_of(&outcome, 2, "athlete_id"), Some(&json!("222")));
    }

    // --- Ruling R1: descriptor pairing.

    #[test]
    fn descriptor_pairing_missing_descriptor_skips_gamelog_and_splits() {
        for payload in [GAMELOG, SPLITS] {
            let mut feed = adapter();
            feed.ingest(&connected(1)).expect("connects");
            let outcome = feed.ingest(&inbound(2, payload)).expect("operational");
            assert!(outcome.events.is_empty(), "{outcome:?}");
            assert_eq!(outcome.notices.len(), 1);
            assert_eq!(outcome.notices[0].code, FeedNoticeCode::StateInconsistent);
        }
    }

    #[test]
    fn descriptor_pairing_mismatched_family_skips_gamelog_and_splits() {
        let mut feed = adapter();
        feed.ingest(&connected(1)).expect("connects");
        // A roster descriptor paired with a gamelog body: the athlete id on that
        // descriptor is a TEAM id, so keying from it would be silently wrong.
        let outcome = ingest(
            &mut feed,
            &[descriptor(2, "roster", "2"), inbound(3, GAMELOG)],
        );
        assert!(outcome.events.is_empty(), "{outcome:?}");
        assert!(
            outcome
                .notices
                .iter()
                .all(|notice| notice.code == FeedNoticeCode::StateInconsistent),
            "{outcome:?}"
        );
    }

    #[test]
    fn descriptor_pairing_non_digit_entity_id_skips_gamelog_and_splits() {
        let mut feed = adapter();
        feed.ingest(&connected(1)).expect("connects");
        let outcome = ingest(
            &mut feed,
            &[
                descriptor(2, "athlete-gamelog", "not-an-id"),
                inbound(3, GAMELOG),
            ],
        );
        assert!(outcome.events.is_empty(), "{outcome:?}");
        assert_eq!(outcome.notices.len(), 1);
        assert_eq!(outcome.notices[0].code, FeedNoticeCode::StateInconsistent);
    }

    #[test]
    fn descriptor_pairing_replaces_an_orphan_and_notices_it() {
        // The capture loop writes the descriptor BEFORE the GET and fabricates no
        // inbound record on failure, so a failed request legally orphans exactly
        // one descriptor. The next descriptor replaces it with one notice, and the
        // body that follows is keyed from the NEW descriptor.
        let mut feed = adapter();
        feed.ingest(&connected(1)).expect("connects");
        let outcome = ingest(
            &mut feed,
            &[
                descriptor(2, "athlete-gamelog", "111"),
                descriptor(3, "athlete-gamelog", "222"),
                inbound(4, GAMELOG),
            ],
        );
        assert_eq!(outcome.events.len(), 1);
        assert_eq!(value_of(&outcome, 0, "athlete_id"), Some(&json!("222")));
        assert_eq!(
            outcome
                .notices
                .iter()
                .filter(|notice| notice.code == FeedNoticeCode::StateInconsistent)
                .count(),
            1,
            "{outcome:?}"
        );
    }

    #[test]
    fn a_body_keyed_shape_keeps_body_identity_on_a_family_mismatch() {
        // The six body-keyed shapes self-identify, so a descriptor mismatch is
        // observable (a notice) but never silently changes their identity.
        let mut feed = adapter();
        feed.ingest(&connected(1)).expect("connects");
        let outcome = ingest(
            &mut feed,
            &[
                descriptor(2, "athlete-gamelog", "999"),
                inbound(3, TEAM_STATISTICS),
            ],
        );
        assert_eq!(outcome.events.len(), 1);
        assert_eq!(value_of(&outcome, 0, "team_id"), Some(&json!("2")));
        assert!(
            outcome
                .notices
                .iter()
                .any(|notice| notice.code == FeedNoticeCode::StateInconsistent),
            "{outcome:?}"
        );
    }

    #[test]
    fn an_unparseable_outbound_record_leaves_no_pending_descriptor() {
        let mut feed = adapter();
        feed.ingest(&connected(1)).expect("connects");
        let outcome = ingest(
            &mut feed,
            &[
                record(2, 1, RecordKind::OutboundText, Some("<not a descriptor>")),
                inbound(3, GAMELOG),
            ],
        );
        assert!(outcome.events.is_empty(), "{outcome:?}");
        assert!(
            outcome
                .notices
                .iter()
                .any(|notice| notice.code == FeedNoticeCode::UnsupportedMessage),
            "{outcome:?}"
        );
    }

    #[test]
    fn disconnect_reports_an_unpaired_trailing_descriptor() {
        let mut feed = adapter();
        feed.ingest(&connected(1)).expect("connects");
        feed.ingest(&descriptor(2, "athlete-splits", "42"))
            .expect("descriptor");
        let outcome = feed
            .ingest(&record(3, 1, RecordKind::Disconnected, None))
            .expect("disconnects");
        assert!(
            outcome
                .notices
                .iter()
                .any(|notice| notice.code == FeedNoticeCode::StateInconsistent),
            "{outcome:?}"
        );
        assert!(
            outcome
                .notices
                .iter()
                .any(|notice| notice.code == FeedNoticeCode::Disconnected),
            "{outcome:?}"
        );
    }

    // --- Permissive core.

    #[test]
    fn odd_bodies_are_noticed_and_skipped_without_poisoning_the_crawl() {
        let mut feed = adapter();
        feed.ingest(&connected(1)).expect("connects");
        let garbage = feed
            .ingest(&inbound(2, "<html>502 Bad Gateway</html>"))
            .expect("garbage is operational");
        assert!(garbage.events.is_empty());
        assert_eq!(garbage.notices[0].code, FeedNoticeCode::UnsupportedMessage);
        let wrong_shape = feed
            .ingest(&inbound(3, r#"{"something":"else"}"#))
            .expect("wrong shape is operational");
        assert!(wrong_shape.events.is_empty());
        assert_eq!(
            wrong_shape.notices[0].code,
            FeedNoticeCode::UnsupportedMessage
        );
        // The crawl still works.
        let outcome = ingest(
            &mut feed,
            &[descriptor(4, "standings", ""), inbound(5, STANDINGS)],
        );
        assert_eq!(outcome.events.len(), 1);
    }

    #[test]
    fn a_bad_row_skips_only_itself() {
        // Two standings entries; the second has no team id. The first still emits.
        let payload = r#"{"children":[{"id":"5","name":"East","standings":{"entries":[
            {"team":{"id":"8"},"stats":[{"name":"wins","type":"wins","value":60.0}]},
            {"team":{},"stats":[]}]}}]}"#;
        let mut feed = adapter();
        feed.ingest(&connected(1)).expect("connects");
        let outcome = feed.ingest(&inbound(2, payload)).expect("mixed batch");
        assert_eq!(outcome.events.len(), 1);
        assert_eq!(outcome.notices.len(), 1);
        assert_eq!(outcome.notices[0].code, FeedNoticeCode::UnsupportedMessage);
    }

    #[test]
    fn configuration_records_are_refused() {
        let mut feed = adapter();
        let configuration = record(1, 1, RecordKind::Configuration, Some("{}"));
        assert!(feed.ingest(&configuration).is_err());
    }

    #[test]
    fn non_finite_and_unparseable_cells_are_dropped_not_serialized() {
        // serde_json cannot serialize NaN or infinity, so a non-finite cell must
        // never reach an event's values; the field is simply absent.
        let payload = r#"{"filters":[],"labels":["A","B","C"],"names":["alpha","beta","gamma"],
            "displayNames":[],"descriptions":[],"splitCategories":[{"name":"split","splits":[
              {"displayName":"All Splits","stats":["NaN","inf","3.5"]}]}]}"#;
        let mut feed = adapter();
        feed.ingest(&connected(1)).expect("connects");
        let outcome = ingest(
            &mut feed,
            &[descriptor(2, "athlete-splits", "7"), inbound(3, payload)],
        );
        assert_eq!(outcome.events.len(), 1);
        assert_eq!(value_of(&outcome, 0, "alpha"), None);
        assert_eq!(value_of(&outcome, 0, "beta"), None);
        assert_eq!(value_of(&outcome, 0, "gamma"), Some(&json!(3.5)));
        // The whole event still serializes, which is the point of the guard.
        serde_json::to_string(&outcome.events[0]).expect("event serializes");
    }

    #[test]
    fn slug_is_deterministic_over_camel_case_and_punctuation() {
        assert_eq!(slug("fieldGoalsMade"), "field_goals_made");
        assert_eq!(slug("vs. Div."), "vs_div");
        assert_eq!(slug("Last Ten Games"), "last_ten_games");
        assert_eq!(slug("avgPointsAgainst"), "avg_points_against");
        assert_eq!(
            slug("Free Throws Made-Attempted Per Game"),
            "free_throws_made_attempted_per_game"
        );
        assert_eq!(slug("---"), "");
        // A digit-to-uppercase transition is a camelCase word boundary too, so
        // ESPN's `avg48Points` slugs to `avg48_points`. The same rule makes the
        // display LABEL "3PT%" slug to `3_pt`; labels are never used as field ids
        // (only `names[]` and stat `name` are), so that is inert, but it is pinned
        // here so the rule is not silently changed.
        assert_eq!(slug("avg48Points"), "avg48_points");
        assert_eq!(slug("3PT%"), "3_pt");
    }

    // --- Replay determinism.

    #[test]
    fn replay_is_deterministic_over_a_full_two_phase_crawl() {
        let fixture = vec![
            connected(1),
            descriptor(2, "teams-list", ""),
            inbound(3, TEAMS_LIST),
            descriptor(4, "standings", ""),
            inbound(5, STANDINGS),
            descriptor(6, "roster", "2"),
            inbound(7, ROSTER),
            descriptor(8, "team-statistics", "2"),
            inbound(9, TEAM_STATISTICS),
            descriptor(10, "team-record", "2"),
            inbound(11, TEAM_RECORD),
            descriptor(12, "athlete-statistics", "4433134"),
            inbound(13, ATHLETE_STATISTICS),
            descriptor(14, "athlete-gamelog", "4433134"),
            inbound(15, GAMELOG),
            descriptor(16, "athlete-splits", "4433134"),
            inbound(17, SPLITS),
            record(18, 1, RecordKind::Disconnected, None),
        ];
        let run = || {
            let mut feed = adapter();
            let mut events = Vec::new();
            for record in &fixture {
                let IngestOutcome { events: batch, .. } =
                    feed.ingest(record).expect("record ingests");
                events.extend(batch);
            }
            serde_json::to_string(&events).expect("events serialize")
        };
        assert_eq!(run(), run());

        // Teams list 0, standings 1, roster 1, team statistics 1, team record 2,
        // athlete statistics 1, gamelog 1, splits 2 = 9.
        let mut feed = adapter();
        let mut total = 0;
        for record in &fixture {
            total += feed.ingest(record).expect("ingests").events.len();
        }
        assert_eq!(total, 9);
    }
}
