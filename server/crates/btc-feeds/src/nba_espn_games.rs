//! NBA game-day ingestion from ESPN's keyless public core and site APIs
//! (capture slice 3).
//!
//! Every ten minutes the day's scoreboard is fetched for discovery — it emits no
//! event itself — and the cycle then walks each discovered event id in ascending
//! order: the competition odds list and the predictor on every cycle, and the
//! much larger game summary only on every sixth cycle, so the summary lands
//! roughly hourly. Requests are keyless HTTPS GETs paced at 150 ms and bounded
//! at 30 events per cycle.
//!
//! Because these are GETs with no body, the capture loop journals an
//! `OutboundText` descriptor naming each URL and the adapter pairs it with the
//! following `InboundText` to know which family a response belongs to. A body
//! matching no known family groups as `UNKNOWN_SERIES` rather than failing the
//! run.

use std::collections::BTreeMap;
use std::time::Duration;

use chrono::{DateTime, NaiveDateTime, Utc};
use serde::Serialize;
use serde_json::value::RawValue;
use serde_json::{Value, json};
use signals_core::SourceEvent;

use crate::adapter::{GuardAction, RecordGuard, bounded_token, validate_publisher_epoch};
use crate::decimal::signed_decimal_string;
use crate::event_window::millis_to_rfc3339;
use crate::{AdapterError, FeedAdapter, FeedNotice, FeedNoticeCode, IngestOutcome, RawRecord};

// --- Compiled ESPN game-day constants. `pub(crate)` so the capture poll loop and
// the adapter share one source of truth for the endpoint TEMPLATES, the cadence,
// the summary sub-cadence, the crawl bounds, the pacing, and the timeout.
//
// This is slice 3, the GAME-DAY CRAWLER. It combines the two established shapes:
//
//   * like slice 1 (`nba_espn`) it is a REPEATING poll cycle -- it re-executes on
//     a cadence for the whole capture, because the product IS the time series;
//   * like slice 2 (`nba_espn_daily`) the per-cycle request list is DISCOVERED
//     rather than compiled: today's scoreboard names the day's event ids and the
//     cycle then walks per-event endpoints.
//
// One cycle, in order:
//
//   1. scoreboard?dates=YYYYMMDD          (discovery; recognized NON-EVENT shape)
//   2. per discovered event id, ascending:
//        events/{id}/competitions/{id}/odds?limit=100      every cycle
//        events/{id}/competitions/{id}/predictor           every cycle
//        summary?event={id}                 only when cycle_index % 6 == 0
//
// VOLUME RATIONALE for the summary sub-cadence. A completed-game summary measured
// 399,957 B (pregame is smaller but still the largest body in the slice), against
// ~6.7 KB for an odds list and ~1.7 KB for a predictor. At the 600 s cycle a
// 15-game slate would pull ~6 MB/hour of summary alone if it were fetched every
// cycle; every sixth cycle makes it hourly, which is the ruled cadence and is
// what the surface research recommends (T-6h / T-30m / ~tip). The odds and
// predictor bodies are cheap and are fetched EVERY cycle, because those are the
// two surfaces that actually move.
//
// The User-Agent story is IDENTICAL to slices 1 and 2 and is not restated here:
// ESPN's Akamai wall is a UA allowlist keyed on the leading token, so this stream
// reuses `nba_espn::CAPTURE_USER_AGENT` -- ONE authority for that fact, never a
// second copy. The odds family additionally carries the sent UA as a captured
// provenance field, because the catalog makes it load-bearing: an odds poll that
// 403s on a UA drift is a permanently missing point in a series nobody can
// reconstruct.
//
// TLS ROUTING. Two hosts are involved and they are NOT interchangeable:
//   site.api.espn.com        scoreboard, summary   -> negotiates TLS 1.3
//   sports.core.api.espn.com odds, predictor       -> TLS 1.2 only
// Slice 2 established the second fact by measurement (145 of 177 requests failed
// under the shared 1.3-only client, every one of them on a core/web host). The
// capture loop therefore holds BOTH clients and routes per host with
// `is_core_host`; no shared default is widened for any other provider.

/// Today's scoreboard. `{date}` is `YYYYMMDD` in UTC, computed by the CAPTURE
/// side (a capture-side wall clock is legal -- it already stamps `received_at`)
/// and recorded verbatim inside the outbound descriptor's `request` URL, so
/// replay never needs a clock to know which slate a cycle asked for.
pub(crate) const SCOREBOARD_TEMPLATE: &str =
    "https://site.api.espn.com/apis/site/v2/sports/basketball/nba/scoreboard?dates={date}";

/// Per-event sportsbook odds ref-list. Items are INLINED, so no `$ref` follow up
/// is needed. The event id appears TWICE (event and competition); on NBA the
/// competition id equals the event id.
///
/// HONEST PROVENANCE OF `limit=100`. The bare odds URL is what the surface
/// research fetched and verified 200; the `limit=100` parameter is carried over
/// from slice 2's record collection, where it WAS verified 200 and where the
/// default page size of 25 demonstrably truncated a 52-row collection. Here it is
/// a guard, not a fix for an observed truncation: today's collection holds a
/// single provider item, so the parameter changes nothing until a second book
/// re-appears in season. It is an unverified parameter on a verified path and is
/// named as such -- the first in-season fetch must re-confirm it, and the
/// adapter's paginated-envelope handling does not depend on it either way.
pub(crate) const ODDS_TEMPLATE: &str = "https://sports.core.api.espn.com/v2/sports/basketball/leagues/nba/events/{event}/competitions/{event}/odds?limit=100";

/// Per-event BPI predictor: win projection, matchup quality, predicted point
/// differential and expected points, plus the model's own `lastModified`.
pub(crate) const PREDICTOR_TEMPLATE: &str = "https://sports.core.api.espn.com/v2/sports/basketball/leagues/nba/events/{event}/competitions/{event}/predictor";

/// Per-event composed summary: season series (the working head-to-head source),
/// live injury report, venue/officials/attendance context, standings snapshot.
pub(crate) const SUMMARY_TEMPLATE: &str =
    "https://site.api.espn.com/apis/site/v2/sports/basketball/nba/summary?event={event}";

/// Host prefix of the TLS-1.2-only normalized entity API. The capture loop routes
/// a request to the TLS-1.2-permitted client when and only when its URL starts
/// with this prefix; every other request rides the shared 1.3-only client.
pub(crate) const CORE_HOST_PREFIX: &str = "https://sports.core.api.espn.com/";

/// Cycle cadence. The catalog's odds cadence is "every 5-15 minutes from first
/// posting to tip"; 600 s sits inside that band and bounds a 15-game slate to
/// ~31 requests/cycle (1 scoreboard + 2 per event), or ~46 on a summary cycle.
pub(crate) const POLL_CADENCE: Duration = Duration::from_secs(600);

/// Summary sub-cadence: fetched only on cycles whose 0-based index is divisible
/// by this, i.e. hourly at the 600 s cadence. See the volume rationale above.
pub(crate) const SUMMARY_CYCLE_MODULUS: u64 = 6;

/// Delay between consecutive requests inside one cycle. Same value as slice 2:
/// ~6.7 requests/second is ordinary browsing behaviour, and a full 15-game
/// summary cycle (46 requests) still completes in ~7 s plus latency.
pub(crate) const REQUEST_PACING: Duration = Duration::from_millis(150);

/// Per-request HTTP timeout. Sized for the summary body (~400 KB), matching
/// slice 2 rather than slice 1's 10 s scoreboard-only bound.
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// Fail-closed bound on the event ids one cycle may crawl. The densest observed
/// NBA slate is 15 games; 30 leaves headroom for a doubleheader calendar day
/// without ever letting a malformed scoreboard drive an unbounded cycle.
pub(crate) const MAX_EVENTS_PER_CYCLE: usize = 30;

/// Grouping label for a 2xx body that matches no known shape. The analyzer never
/// aborts a run on an odd body; it groups it here instead (see `classify_series`).
pub(crate) const UNKNOWN_SERIES: &str = "nba_espn_games:unknown";

const SOURCE_VERSION: &str = "1.0.0";

/// The three emitting source ids. The scoreboard is deliberately absent: it is a
/// recognized NON-EVENT shape in this stream (see `Shape::Scoreboard`).
const ODDS_SOURCE_ID: &str = "nba.espn.games.odds";
const PREDICTOR_SOURCE_ID: &str = "nba.espn.games.predictor";
const SUMMARY_SOURCE_ID: &str = "nba.espn.games.summary-context";

/// Fail-closed bounds on ONE response body. These sit far above any real payload
/// (an odds collection carried exactly one provider item, a season series 3-4
/// prior meetings, a summary 3 officials and 2 broadcast entries), so reaching
/// one means the body is pathological: exceeding a bound is a fatal fail-closed
/// error, not a per-row anomaly.
const MAX_ODDS_ITEMS: usize = 32;
const MAX_H2H_MEETINGS: usize = 32;
const MAX_LIST_ENTRIES: usize = 32;
const MAX_VALUES_PER_EVENT: usize = 512;

/// The market `rel` tokens ESPN tags each DK bet-slip link with (catalog 3.5a).
/// A `links[]` entry whose `rel` array carries one of these is a deep bet-slip
/// link for that market; entries without a market rel are not bet-slip links.
const BET_SLIP_RELS: [&str; 7] = [
    "home",
    "away",
    "homeSpread",
    "awaySpread",
    "over",
    "under",
    "game",
];

/// Depth and node bounds on the `$ref` scan that recovers an event id from a
/// core-API body. A body deep or wide enough to exceed these is pathological and
/// simply yields no id, which falls through to the descriptor path.
const MAX_REF_SCAN_NODES: usize = 4_096;

// --- NUMERIC POLICY (deliberate, and a deliberate divergence from slice 2).
//
// This stream carries PRICED MARKET STATE (spread lines, totals, American and
// decimal prices) and the model projections consumers price against. The
// repository invariant is that market values use decimal strings at JSON
// boundaries and never route through binary floating point, so every non-integer
// value this adapter emits is canonical decimal TEXT produced by the shared
// `decimal::signed_decimal_string` converter over the payload's own raw token --
// the same one authority the BTC adapters use, which rejects precision loss
// rather than accepting a rounded value.
//
// That is why the odds and predictor extractions navigate a `serde_json::RawValue`
// tree instead of the `serde_json::Value` used for classification: `Value` (built
// without `arbitrary_precision`) materializes every JSON number as an `f64`, and
// a market price must never pass through one. Classification still uses `Value`,
// which is safe because it only inspects the presence and JSON TYPE of keys.
//
// Slice 2 emits sports counting statistics as JSON numbers; that stays correct
// there (rebounds are not a price) and is not what this stream does.

/// Build the exact outbound descriptor recorded before one game-day request.
///
/// Requests are GETs with no body, so the descriptor IS the outbound record. It
/// extends slice 1's `{"request","cycle","index"}` with the `entity_id` field
/// slice 2 introduced: the event id for a per-event request, empty for the
/// cycle's scoreboard discovery request.
///
/// `entity_id` is the FALLBACK identity carrier, not the primary one (contrast
/// slice 2, where two families have no body-borne id at all). The odds and
/// predictor bodies name their event inside core-API `$ref` URLs and the summary
/// names it at `header.id`, so identity is normally body-derived and the emitted
/// `event_id_basis` field says which path produced it. The descriptor exists so a
/// body that genuinely carries no id is still keyable -- the same mechanism, and
/// the same fail-closed digit validation, as the slice-2 ruling R1 precedent
/// (`nba_espn_daily::outbound_descriptor`), implemented locally rather than
/// shared because the two descriptors carry different field sets.
///
/// The scoreboard URL carries the capture-side `?dates=YYYYMMDD` parameter, so
/// the slate a cycle asked for is recorded in the journal and replay stays clock
/// free.
///
/// Field/serialize order fixes the key order to
/// `{"request","cycle","index","entity_id"}`.
pub fn outbound_descriptor(url: &str, cycle: u64, index: u64, entity_id: &str) -> String {
    #[derive(Serialize)]
    struct Descriptor<'a> {
        request: &'a str,
        cycle: u64,
        index: u64,
        entity_id: &'a str,
    }
    serde_json::to_string(&Descriptor {
        request: url,
        cycle,
        index,
        entity_id,
    })
    .expect("the compiled outbound descriptor always serializes")
}

/// Render one compiled template. Substitution is by literal placeholder, so a
/// template that forgets a placeholder is visible in the produced URL rather than
/// silently dropping the entity (slice-2 precedent).
pub(crate) fn render(template: &str, date: &str, event: &str) -> String {
    template.replace("{date}", date).replace("{event}", event)
}

/// True when a URL addresses the TLS-1.2-only normalized entity API and must ride
/// the TLS-1.2-permitted client. ONE authority for the routing fact, used by the
/// capture loop and pinned by test.
pub(crate) fn is_core_host(url: &str) -> bool {
    url.starts_with(CORE_HOST_PREFIX)
}

/// The four response shapes this stream recognizes. This is the CLOSED list.
///
/// DISCRIMINATOR RULES, each pinned against the real 2026-08-30 fetches recorded
/// in the game-surface research. The top level is NEVER parsed with
/// `deny_unknown_fields`: ESPN's shapes are large, unversioned and
/// third-party-aggregated, so the adapter extracts a validated core from a
/// permissive `serde_json::Value` and treats an unrecognized body as an
/// UnsupportedMessage notice + skip.
///
/// The rules are checked in the fixed order below, so each rule may assume the
/// earlier ones already failed:
///
/// 1. `Scoreboard` -- top-level `events` ARRAY. Recognized and SILENT: see the
///    variant comment.
/// 2. `Odds` -- the ESPN core paginated-collection envelope (`items` ARRAY plus a
///    `pageCount` NUMBER) whose first item carries a `provider` OBJECT. An EMPTY
///    `items` array is also accepted as odds: a game with no posted line answers
///    `{count:0, items:[]}`, and that is a recognized steady state, not an odd
///    body. Slice 2's team-record shape rides the same envelope, but the two
///    never meet: they are separate streams with separate adapters, and this
///    stream never requests a record collection.
/// 3. `Summary` -- top-level `boxscore` OBJECT accompanied by at least one of the
///    composed-view blocks (`header` OBJECT, `seasonseries` ARRAY, `injuries`
///    ARRAY).
/// 4. `Predictor` -- top-level `homeTeam` OBJECT **or** `awayTeam` OBJECT. EITHER
///    is enough on purpose: on the verified reference event ESPN populated only
///    one side, so a rule demanding both would refuse a real predictor body the
///    moment ESPN also omits the empty side's key. The summary rule is checked
///    FIRST so the two can never contend for a composed body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Shape {
    /// The cycle's discovery response. It classifies cleanly, emits NOTHING, and
    /// raises NO notice -- the recognized non-event shape of this stream, exactly
    /// as slice 2 treats its teams list.
    ///
    /// The silence is deliberate and load-bearing. Slice 1 (`nba.espn.status.*`)
    /// already owns scoreboard emission: it polls the same surface for game state
    /// and emits one event per changed game. Emitting the same games again here
    /// under a second source id would publish one observation twice, and the two
    /// copies would drift the moment either stream's cadence changed. In THIS
    /// stream the scoreboard is a request-list generator and nothing else.
    Scoreboard,
    Odds,
    Predictor,
    Summary,
}

impl Shape {
    /// The wire family token used in the normalization series. One authority.
    pub(crate) const fn family(self) -> &'static str {
        match self {
            Self::Scoreboard => "scoreboard",
            Self::Odds => "odds",
            Self::Predictor => "predictor",
            Self::Summary => "summary",
        }
    }
}

/// Permissive normalization series classifier. Never fails: a body that is not
/// valid JSON, or matches no shape, groups as `UNKNOWN_SERIES`. Mirrors the
/// Chainlink and slice-1/2 precedent that a malformed body must not abort a
/// normalization run when replay of the same journal completes.
pub(crate) fn classify_series(payload: &str) -> String {
    match serde_json::from_str::<Value>(payload) {
        Ok(value) => match classify(&value) {
            Some(shape) => format!("nba_espn_games:{}", shape.family()),
            None => UNKNOWN_SERIES.to_owned(),
        },
        Err(_) => UNKNOWN_SERIES.to_owned(),
    }
}

/// Apply the discriminator rules in their documented fixed order.
pub(crate) fn classify(value: &Value) -> Option<Shape> {
    if is_array(value, "events") {
        return Some(Shape::Scoreboard);
    }
    if is_array(value, "items") && value.get("pageCount").is_some_and(Value::is_number) {
        let items = value["items"].as_array().expect("items is an array");
        match items.first() {
            // A game with no posted line: `{count:0, items:[]}`.
            None => return Some(Shape::Odds),
            Some(first) if first.get("provider").is_some_and(Value::is_object) => {
                return Some(Shape::Odds);
            }
            // A paginated collection that is not an odds list is NOT recognized
            // here; it falls through to the unsupported path rather than being
            // coerced into the odds extraction.
            Some(_) => {}
        }
    }
    if value.get("boxscore").is_some_and(Value::is_object)
        && (value.get("header").is_some_and(Value::is_object)
            || is_array(value, "seasonseries")
            || is_array(value, "injuries"))
    {
        return Some(Shape::Summary);
    }
    if value.get("homeTeam").is_some_and(Value::is_object)
        || value.get("awayTeam").is_some_and(Value::is_object)
    {
        return Some(Shape::Predictor);
    }
    None
}

fn is_array(value: &Value, key: &str) -> bool {
    value.get(key).is_some_and(Value::is_array)
}

/// One outbound descriptor awaiting its response. Local copy of the slice-2
/// pending-descriptor mechanism (ruling R1); this stream's descriptor carries no
/// `family` field, so the struct is a strict subset and sharing the sibling's
/// type would import a field this stream cannot populate.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingDescriptor {
    entity_id: String,
}

/// Deterministic ESPN game-day parser.
///
/// NO DEDUP, ANYWHERE, DELIBERATELY. Slice 1 polls a state snapshot and collapses
/// an unchanged game, because there the repeat carries no information. Here the
/// repeat IS the information: ESPN's own odds-movement endpoint answers
/// `{count:0, items:[]}` on completed games, so the line history is not durably
/// public and OUR POLL SEQUENCE IS THE MOVEMENT SERIES. Every cycle emits one
/// event per provider item per event id, with its own `received_at`, whether or
/// not the numbers changed; a consumer reconstructs the series by ordering those
/// emissions, and "unchanged for 40 minutes" is a fact the series must be able to
/// state. Collapsing duplicates here would silently delete the product.
///
/// The same reasoning covers the predictor (a BPI re-run that reproduces the
/// previous numbers is a real observation of model stability) and the summary
/// (fetched hourly at most, so there is nothing to collapse).
///
/// The adapter is therefore STATELESS across bodies apart from the sequence
/// counter and the single pending descriptor: no per-entity maps, and so no
/// per-entity resource bounds to reason about. Bounds are per body.
///
/// DESCRIPTOR PAIRING. The adapter keeps ONE pending descriptor: every
/// `OutboundText` sets it, every `InboundText` consumes it. A new descriptor
/// arriving while one is still pending REPLACES it and emits a `StateInconsistent`
/// notice for the orphan -- a legal, expected state, because the capture loop
/// writes the descriptor BEFORE the GET and fabricates no inbound record when a
/// request fails, so each per-request failure orphans exactly one descriptor. A
/// descriptor still pending when the journal ends is silently discarded: no
/// response arrived for it, so the only thing lost is a notice, never an
/// observation (slice-2 disposition, carried unchanged).
pub struct NbaEspnGamesAdapter {
    publisher_epoch: String,
    guard: RecordGuard,
    next_sequence: u64,
    pending: Option<PendingDescriptor>,
}

impl NbaEspnGamesAdapter {
    /// Build the adapter for one publisher epoch, the identity stamped on every
    /// emitted event. Rejects an epoch that is blank after trimming or longer
    /// than 128 bytes with [`AdapterError::InvalidRecord`]; no descriptor is
    /// pending at construction.
    pub fn new(publisher_epoch: impl Into<String>) -> Result<Self, AdapterError> {
        let publisher_epoch = publisher_epoch.into();
        validate_publisher_epoch(&publisher_epoch)?;
        Ok(Self {
            publisher_epoch,
            guard: RecordGuard::new(crate::Provider::NbaEspnGames),
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
                // `request` is the field every descriptor carries; its presence is
                // what makes an outbound record recognizable as one of ours.
                value.get("request").and_then(Value::as_str)?;
                Some(PendingDescriptor {
                    entity_id: value
                        .get("entity_id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned(),
                })
            });
        if let Some(orphan) = self.pending.take() {
            outcome.notices.push(FeedNotice::info(
                FeedNoticeCode::StateInconsistent,
                format!(
                    "NBA ESPN games descriptor for entity `{}` was never paired with a response \
                     and is replaced by a new request",
                    bounded_token(&orphan.entity_id)
                ),
            ));
        }
        match parsed {
            Some(descriptor) => self.pending = Some(descriptor),
            None => {
                // An outbound record that is not a recognizable descriptor leaves
                // the pending slot EMPTY rather than holding a half-parsed one, so
                // an id-less body fails closed on "no descriptor" instead of being
                // keyed from garbage.
                outcome.notices.push(FeedNotice::info(
                    FeedNoticeCode::UnsupportedMessage,
                    "NBA ESPN games outbound record is not a recognizable request descriptor"
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
        // Every request is independent, so any single odd 2xx body is transient:
        // it is noticed and skipped, never session-fatal (Chainlink D1 doctrine,
        // carried through slices 1 and 2). Genuine journal-structure failures
        // remain fatal at the shared record-guard layer above.
        let pending = self.pending.take();
        let value: Value = match serde_json::from_str(payload) {
            Ok(value) => value,
            Err(error) => {
                return Ok(IngestOutcome::notice(FeedNotice::info(
                    FeedNoticeCode::UnsupportedMessage,
                    bounded_detail("NBA ESPN games body is not valid JSON", &error.to_string()),
                )));
            }
        };
        let Some(shape) = classify(&value) else {
            return Ok(IngestOutcome::notice(FeedNotice::info(
                FeedNoticeCode::UnsupportedMessage,
                "NBA ESPN games body matches none of the four recognized game-day shapes"
                    .to_owned(),
            )));
        };
        if shape == Shape::Scoreboard {
            // Recognized NON-EVENT shape: discovery input only, no event, no
            // notice. See `Shape::Scoreboard`.
            return Ok(IngestOutcome::default());
        }

        // Identity, body first and descriptor second.
        let mut outcome = IngestOutcome::default();
        let body_event_id = match shape {
            Shape::Odds | Shape::Predictor => event_id_from_refs(&value),
            Shape::Summary => value
                .get("header")
                .and_then(|header| id_to_string(header.get("id"))),
            Shape::Scoreboard => None,
        }
        .filter(|id| is_entity_id(id));
        let (event_id, event_id_basis) = match body_event_id {
            Some(id) => {
                // Body-derived identity is authoritative, but when a paired
                // descriptor names a DIFFERENT entity the mismatch earns a notice:
                // the request/response pairing drifted, exactly the cross-check
                // slice 2 raises when a descriptor's declared family disagrees with
                // the body. The body id still wins.
                if let Some(paired) = pending
                    .as_ref()
                    .filter(|p| is_entity_id(&p.entity_id) && p.entity_id != id)
                {
                    outcome.notices.push(FeedNotice::info(
                        FeedNoticeCode::StateInconsistent,
                        format!(
                            "NBA ESPN games {} response body names event `{}` but its paired \
                             request descriptor names `{}`; the body id is authoritative",
                            shape.family(),
                            bounded_token(&id),
                            bounded_token(&paired.entity_id)
                        ),
                    ));
                }
                (id, "response_body")
            }
            None => {
                let Some(pending) = pending.as_ref().filter(|p| is_entity_id(&p.entity_id)) else {
                    outcome.notices.push(FeedNotice::info(
                        FeedNoticeCode::StateInconsistent,
                        format!(
                            "NBA ESPN games `{}` response carries no event id and has no usable \
                             paired request descriptor; skipped",
                            shape.family()
                        ),
                    ));
                    return Ok(outcome);
                };
                (pending.entity_id.clone(), "request_descriptor")
            }
        };

        match shape {
            Shape::Scoreboard => unreachable!("the scoreboard returned above"),
            Shape::Odds => self.odds_events(record, payload, &event_id, event_id_basis, outcome),
            Shape::Predictor => {
                self.predictor_events(record, payload, &event_id, event_id_basis, outcome)
            }
            Shape::Summary => {
                self.summary_events(record, &value, &event_id, event_id_basis, outcome)
            }
        }
    }

    /// Odds: ONE event per provider item.
    ///
    /// The extraction navigates the raw JSON tokens (see the NUMERIC POLICY note
    /// above), so every line and price is emitted as canonical decimal TEXT taken
    /// from the payload's own token, never through an `f64`.
    ///
    /// Snapshot layout, pinned against the real fetch: the item carries `open`,
    /// `close` and `current` blocks for the TOTAL (`{over, under, total}`, where
    /// `total` is the line and `over`/`under` are the prices), and each of
    /// `homeTeamOdds` / `awayTeamOdds` carries its own `open`/`close`/`current`
    /// blocks holding `pointSpread` (the LINE), `spread` (the PRICE at that line)
    /// and `moneyLine`. The current line and current prices are ADDITIONALLY
    /// exposed as native numbers at item and side top level, and the catalog names
    /// those numeric paths the preferred source, so they are written LAST and win
    /// over the block-derived current values.
    fn odds_events(
        &mut self,
        record: &RawRecord,
        payload: &str,
        event_id: &str,
        event_id_basis: &str,
        mut outcome: IngestOutcome,
    ) -> Result<IngestOutcome, AdapterError> {
        let Some(root) = raw_object_from_str(payload) else {
            return Ok(IngestOutcome::notice(skip_notice(
                "NBA games odds body is not a JSON object",
            )));
        };
        let Some(items) = root.get("items").copied().and_then(raw_array) else {
            return Ok(IngestOutcome::notice(skip_notice(
                "NBA games odds body carries no items array",
            )));
        };
        if items.len() > MAX_ODDS_ITEMS {
            return Err(AdapterError::InvalidPayload(format!(
                "NBA games odds item count exceeded {MAX_ODDS_ITEMS}"
            )));
        }
        // An empty collection is the recognized "no line posted for this game"
        // steady state: no event, and no notice either (a per-poll notice for
        // every lineless game on the slate would be pure noise).
        if items.is_empty() {
            return Ok(outcome);
        }
        // Captured provenance for the movement record: the exact body these
        // numbers were read from, so a consumer can tell an unchanged line from a
        // re-fetch without diffing every field.
        let payload_sha256 = crate::secure_http::sha256_hex(payload.as_bytes());

        for (index, item) in items.into_iter().enumerate() {
            let Some(item) = raw_object(item) else {
                outcome
                    .notices
                    .push(skip_notice("NBA games odds item is not a JSON object"));
                continue;
            };
            let mut values = BTreeMap::from([
                ("espn_event_id".to_owned(), json!(event_id)),
                ("event_id_basis".to_owned(), json!(event_id_basis)),
                // Constant, so a future basis change is detectable by a consumer.
                ("odds_snapshot_basis".to_owned(), json!("current")),
                (
                    "odds_capture_user_agent".to_owned(),
                    json!(crate::nba_espn::CAPTURE_USER_AGENT),
                ),
                ("odds_payload_sha256".to_owned(), json!(payload_sha256)),
                ("event_time_basis".to_owned(), json!("receipt")),
                ("event_time".to_owned(), json!(record.received_at)),
            ]);

            // Every emitted odds event must name its book: without a provider there
            // is no `odds_provider_id`/`odds_provider_name`, and the event would
            // carry a nameless line. The classify rule only guarantees the FIRST
            // item has a provider (that is what makes the body odds), so a later
            // provider-less item is skipped with a notice rather than emitted
            // book-less.
            let Some(provider) = item.get("provider").copied().and_then(raw_object) else {
                outcome.notices.push(FeedNotice::info(
                    FeedNoticeCode::StateInconsistent,
                    format!(
                        "NBA games odds item {index} carries no provider object and would emit a \
                         book-less line; skipped"
                    ),
                ));
                continue;
            };
            put(
                &mut values,
                "odds_provider_id",
                provider
                    .get("id")
                    .copied()
                    .and_then(raw_id)
                    .map(Value::from),
            );
            put(
                &mut values,
                "odds_provider_name",
                provider
                    .get("name")
                    .copied()
                    .and_then(raw_text)
                    .map(Value::from),
            );
            put(
                &mut values,
                "odds_provider_priority",
                provider
                    .get("priority")
                    .copied()
                    .and_then(raw_integer)
                    .map(Value::from),
            );
            put(
                &mut values,
                "odds_item_ref",
                item.get("$ref")
                    .copied()
                    .and_then(raw_text)
                    .map(Value::from),
            );
            put(
                &mut values,
                "odds_details_current",
                item.get("details")
                    .copied()
                    .and_then(raw_text)
                    .map(Value::from),
            );
            // Bet-slip links (catalog 3.5a): ESPN carries a `links[]` array of DK
            // deep links, one per market, each tagged with a market `rel` (see
            // `BET_SLIP_RELS`). The bet-slip hrefs are emitted as a string array,
            // and the book's own event id -- embedded in every such href and useful
            // as a cross-book join key -- is read off the first one. Absent links
            // are fine and simply not emitted.
            if let Some(links) = item.get("links").copied().and_then(raw_array) {
                if links.len() > MAX_LIST_ENTRIES {
                    return Err(AdapterError::InvalidPayload(format!(
                        "NBA games odds link count exceeded {MAX_LIST_ENTRIES}"
                    )));
                }
                let mut bet_slip_hrefs = Vec::new();
                for link in links {
                    let Some(link) = raw_object(link) else {
                        continue;
                    };
                    let is_bet_slip =
                        link.get("rel")
                            .copied()
                            .and_then(raw_array)
                            .is_some_and(|rels| {
                                rels.into_iter()
                                    .filter_map(raw_text)
                                    .any(|rel| BET_SLIP_RELS.contains(&rel.as_str()))
                            });
                    if !is_bet_slip {
                        continue;
                    }
                    if let Some(href) = link.get("href").copied().and_then(raw_text) {
                        bet_slip_hrefs.push(href);
                    }
                }
                if let Some(first) = bet_slip_hrefs.first() {
                    // Real bet-slip hrefs are book GATEWAY urls carrying the
                    // event path percent-encoded inside a query parameter
                    // (`...&preurl=https%3A%2F%2F...%2Fevent%2F33954994%3F...`),
                    // so the encoded form is matched first; the literal
                    // `/event/<digits>` form is kept as a fallback in case a
                    // book ever links directly.
                    let book_event_id =
                        encoded_event_id_segment(first).or_else(|| path_id_segment(first, "event"));
                    put(
                        &mut values,
                        "odds_book_event_id",
                        book_event_id.map(Value::from),
                    );
                }
                if !bet_slip_hrefs.is_empty() {
                    values.insert(
                        "odds_bet_slip_links".to_owned(),
                        Value::from(bet_slip_hrefs),
                    );
                }
            }
            for (field, key) in [
                ("odds_moneyline_winner", "moneylineWinner"),
                ("odds_spread_winner", "spreadWinner"),
            ] {
                put(
                    &mut values,
                    field,
                    item.get(key).copied().and_then(raw_bool).map(Value::from),
                );
            }
            // Presence flag only. The research observed that a LIVE game's item
            // additionally carries `pointSpread`/`moneyline` sub-blocks at item
            // level, but their internal shape was never captured, so the catalog
            // defines a presence flag and no fields from them.
            values.insert(
                "odds_live_block_present".to_owned(),
                json!(item.contains_key("pointSpread") || item.contains_key("moneyline")),
            );

            // Totals: the three snapshot blocks.
            for phase in SNAPSHOT_PHASES {
                let Some(block) = item.get(phase).copied().and_then(raw_object) else {
                    continue;
                };
                if let Some(over) = block.get("over").copied() {
                    put_price(&mut values, "odds_total_over_price", phase, over);
                }
                if let Some(under) = block.get("under").copied() {
                    put_price(&mut values, "odds_total_under_price", phase, under);
                }
                if let Some(total) = block.get("total").copied() {
                    put_line(&mut values, &format!("odds_total_line_{phase}"), total);
                }
            }

            // Spread and moneyline: per side, the three snapshot blocks plus the
            // side-level flat conveniences.
            for (side, key) in [("home", "homeTeamOdds"), ("away", "awayTeamOdds")] {
                let Some(block) = item.get(key).copied().and_then(raw_object) else {
                    continue;
                };
                for phase in SNAPSHOT_PHASES {
                    let Some(snapshot) = block.get(phase).copied().and_then(raw_object) else {
                        continue;
                    };
                    if let Some(line) = snapshot.get("pointSpread").copied() {
                        put_line(
                            &mut values,
                            &format!("odds_spread_line_{side}_{phase}"),
                            line,
                        );
                    }
                    if let Some(price) = snapshot.get("spread").copied() {
                        put_price(
                            &mut values,
                            &format!("odds_spread_price_{side}"),
                            phase,
                            price,
                        );
                    }
                    if let Some(price) = snapshot.get("moneyLine").copied() {
                        put_price(&mut values, &format!("odds_moneyline_{side}"), phase, price);
                    }
                }
                put(
                    &mut values,
                    &format!("odds_spread_favorite_{side}_current"),
                    block
                        .get("favorite")
                        .copied()
                        .and_then(raw_bool)
                        .map(Value::from),
                );
                put(
                    &mut values,
                    &format!("odds_team_id_{side}"),
                    block
                        .get("team")
                        .copied()
                        .and_then(raw_object)
                        .and_then(|team| team.get("$ref").copied().and_then(raw_text))
                        .and_then(|href| path_id_segment(&href, "teams"))
                        .map(Value::from),
                );
                // Side-level flat numerics: the CURRENT moneyline and the CURRENT
                // spread price, preferred over the block-derived pair.
                put(
                    &mut values,
                    &format!("odds_moneyline_{side}_american_current"),
                    block
                        .get("moneyLine")
                        .copied()
                        .and_then(|raw| market_decimal(raw, "moneyLine"))
                        .map(Value::from),
                );
                put(
                    &mut values,
                    &format!("odds_spread_price_{side}_american_current"),
                    block
                        .get("spreadOdds")
                        .copied()
                        .and_then(|raw| market_decimal(raw, "spreadOdds"))
                        .map(Value::from),
                );
            }

            // Item-level flat numerics, written last so the preferred numeric
            // source wins for the current snapshot. `spread` is HOME-relative.
            for (field, key) in [
                ("odds_spread_line_home_current", "spread"),
                ("odds_total_line_current", "overUnder"),
                ("odds_total_over_price_american_current", "overOdds"),
                ("odds_total_under_price_american_current", "underOdds"),
            ] {
                put(
                    &mut values,
                    field,
                    item.get(key)
                        .copied()
                        .and_then(|raw| market_decimal(raw, key))
                        .map(Value::from),
                );
            }

            guard_values(&values)?;
            let event =
                self.source_event(record, ODDS_SOURCE_ID, record.received_at.clone(), values)?;
            outcome.events.push(event);
        }
        Ok(outcome)
    }

    /// Predictor: ONE event per body.
    ///
    /// `lastModified` is the model's own publication time for this state, which is
    /// exactly what an envelope `event_time` on basis `source` means; when it is
    /// absent or implausible the emission falls back to receipt basis with a
    /// notice rather than being dropped.
    ///
    /// On the verified reference event only ONE side carried statistics and the
    /// other was ESPN's arithmetic complement. This adapter never computes the
    /// complement -- publishing an inference as a reading is exactly what the
    /// catalog's provenance note forbids. The `bpi_projection_source_mode` field
    /// is emitted with one of `powerindex_per_team | predictor_both_teams |
    /// predictor_one_team`: `powerindex_per_team` is the per-team powerindex
    /// fetch's mode, and on a predictor body this adapter reports
    /// `predictor_both_teams` when both sides carry statistics and
    /// `predictor_one_team` when only one does. The catalog's former complement
    /// value (`predictor_one_team_complement`) is never computed by this adapter;
    /// the catalog is being amended to match this emitted vocabulary.
    fn predictor_events(
        &mut self,
        record: &RawRecord,
        payload: &str,
        event_id: &str,
        event_id_basis: &str,
        mut outcome: IngestOutcome,
    ) -> Result<IngestOutcome, AdapterError> {
        let Some(root) = raw_object_from_str(payload) else {
            return Ok(IngestOutcome::notice(skip_notice(
                "NBA games predictor body is not a JSON object",
            )));
        };
        let mut values = BTreeMap::from([
            ("espn_event_id".to_owned(), json!(event_id)),
            ("event_id_basis".to_owned(), json!(event_id_basis)),
        ]);

        let last_modified = root.get("lastModified").copied().and_then(raw_text);
        let (event_time, event_time_basis) = match last_modified.as_deref() {
            Some(raw) => match parse_espn_iso_to_rfc3339(raw) {
                Ok(source) => {
                    values.insert(
                        "bpi_predictor_last_modified".to_owned(),
                        json!(source.clone()),
                    );
                    (source, "source")
                }
                Err(detail) => {
                    outcome.notices.push(FeedNotice::info(
                        FeedNoticeCode::StateInconsistent,
                        bounded_detail("NBA games predictor lastModified is implausible", &detail),
                    ));
                    (record.received_at.clone(), "receipt")
                }
            },
            None => (record.received_at.clone(), "receipt"),
        };

        let mut populated_sides = 0u8;
        for (side, key) in [("home", "homeTeam"), ("away", "awayTeam")] {
            let Some(block) = root.get(key).copied().and_then(raw_object) else {
                continue;
            };
            let Some(statistics) = block.get("statistics").copied().and_then(raw_array) else {
                continue;
            };
            if statistics.is_empty() {
                continue;
            }
            if statistics.len() > MAX_LIST_ENTRIES {
                return Err(AdapterError::InvalidPayload(format!(
                    "NBA games predictor statistic count exceeded {MAX_LIST_ENTRIES}"
                )));
            }
            populated_sides = populated_sides.saturating_add(1);
            for statistic in statistics {
                let Some(statistic) = raw_object(statistic) else {
                    continue;
                };
                let Some(name) = statistic.get("name").copied().and_then(raw_text) else {
                    continue;
                };
                let Some(text) = statistic
                    .get("value")
                    .copied()
                    .and_then(|raw| market_decimal(raw, "value"))
                else {
                    continue;
                };
                match name.as_str() {
                    // ESPN publishes both projections on 0-100. The verbatim
                    // published value is retained AND the catalog's 0-1 `_prob`
                    // form is emitted beside it; the division is an exact decimal
                    // point shift, verified to round-trip before emission.
                    "gameProjection" => {
                        values.insert(format!("{side}_bpi_game_projection"), json!(text.clone()));
                        put(
                            &mut values,
                            &format!("{side}_bpi_win_prob"),
                            scaled_probability(&text).map(Value::from),
                        );
                    }
                    "teamChanceLoss" => {
                        values.insert(format!("{side}_bpi_team_chance_loss"), json!(text.clone()));
                        put(
                            &mut values,
                            &format!("{side}_bpi_loss_prob"),
                            scaled_probability(&text).map(Value::from),
                        );
                    }
                    "teamPredPtDiff" => {
                        values.insert(format!("{side}_bpi_pred_pt_diff"), json!(text));
                    }
                    "teamExpectedPts" => {
                        values.insert(format!("{side}_bpi_team_expected_pts"), json!(text));
                    }
                    "oppExpectedPts" => {
                        values.insert(format!("{side}_bpi_opp_expected_pts"), json!(text));
                    }
                    // Game-level, not per side: ESPN repeats the same value on
                    // whichever sides are populated, so the first one wins and a
                    // second copy is never written under a side-prefixed id.
                    "matchupQuality" => {
                        values
                            .entry("bpi_matchup_quality".to_owned())
                            .or_insert_with(|| json!(text));
                    }
                    _ => {}
                }
            }
        }

        if populated_sides == 0 {
            // A pregame predictor with no statistics yet is the recognized "model
            // has not published a projection for this game" steady state, exactly
            // like the empty-odds collection above: no event, and no notice either,
            // because a per-poll notice for every not-yet-projected game on the
            // slate would be pure noise.
            return Ok(outcome);
        }
        values.insert(
            "bpi_projection_source_mode".to_owned(),
            json!(if populated_sides >= 2 {
                "predictor_both_teams"
            } else {
                "predictor_one_team"
            }),
        );
        values.insert("event_time_basis".to_owned(), json!(event_time_basis));
        values.insert("event_time".to_owned(), json!(event_time.clone()));
        guard_values(&values)?;
        let event = self.source_event(record, PREDICTOR_SOURCE_ID, event_time, values)?;
        outcome.events.push(event);
        Ok(outcome)
    }

    /// Summary context: ONE event per body.
    ///
    /// Carries the catalog's head-to-head family (the summary `seasonseries[]` is
    /// the working h2h source -- the core head-to-heads endpoint answers
    /// `count:0` and no `/versus` endpoint exists) plus the summary-only game
    /// context rows of the identity/schedule family.
    ///
    /// The summary body is the only shape in this stream with no numeric market
    /// values, so it is extracted from the already-parsed permissive `Value` --
    /// every field here is a string, an integer, a boolean, or the fixed-schema
    /// prior-meetings row array the catalog specifies.
    ///
    /// The live injury report is deliberately reduced to a COUNT. Slice 1 owns
    /// per-athlete injury emission league-wide; re-emitting the same entries per
    /// game would duplicate that stream's product. The count is the honest
    /// per-game volume signal, the same shape as slice 2's `listed_injury_count`.
    fn summary_events(
        &mut self,
        record: &RawRecord,
        value: &Value,
        event_id: &str,
        event_id_basis: &str,
        mut outcome: IngestOutcome,
    ) -> Result<IngestOutcome, AdapterError> {
        let mut values = BTreeMap::from([
            ("espn_event_id".to_owned(), json!(event_id)),
            ("event_id_basis".to_owned(), json!(event_id_basis)),
            ("event_time_basis".to_owned(), json!("receipt")),
            ("event_time".to_owned(), json!(record.received_at)),
        ]);

        let header = value.get("header");
        if let Some(season) = header.and_then(|header| header.get("season")) {
            put(
                &mut values,
                "season_year",
                season.get("year").and_then(Value::as_i64).map(Value::from),
            );
            put(
                &mut values,
                "season_type",
                season.get("type").and_then(Value::as_i64).map(Value::from),
            );
        }
        put(
            &mut values,
            "game_boxscore_available_flag",
            header
                .and_then(|header| header.get("competitions"))
                .and_then(Value::as_array)
                .and_then(|competitions| competitions.first())
                .and_then(|competition| competition.get("boxscoreAvailable"))
                .and_then(Value::as_bool)
                .map(Value::from),
        );

        // Head to head: the FIRST season-series block. Playoff games can carry a
        // second block; the first is the one the catalog's family describes, and
        // the count is carried so a consumer can see when there were more.
        if let Some(series) = value
            .get("seasonseries")
            .and_then(Value::as_array)
            .and_then(|series| {
                values.insert(
                    "h2h_series_block_count".to_owned(),
                    json!(u64::try_from(series.len()).unwrap_or(u64::MAX)),
                );
                series.first()
            })
        {
            for (field, key) in [
                ("h2h_series_type", "type"),
                ("h2h_series_title", "title"),
                ("h2h_series_summary_text", "summary"),
                ("h2h_series_score_display", "seriesScore"),
            ] {
                put(
                    &mut values,
                    field,
                    string_field(series, key).map(Value::from),
                );
            }
            put(
                &mut values,
                "h2h_series_completed_flag",
                series
                    .get("completed")
                    .and_then(Value::as_bool)
                    .map(Value::from),
            );
            put(
                &mut values,
                "h2h_meetings_scheduled_total",
                series
                    .get("totalCompetitions")
                    .and_then(Value::as_i64)
                    .map(Value::from),
            );
            if let Some(meetings) = series.get("events").and_then(Value::as_array) {
                if meetings.len() > MAX_H2H_MEETINGS {
                    return Err(AdapterError::InvalidPayload(format!(
                        "NBA games summary prior-meeting count exceeded {MAX_H2H_MEETINGS}"
                    )));
                }
                let rows: Vec<Value> = meetings.iter().map(prior_meeting_row).collect();
                values.insert(
                    "h2h_prior_meeting_count".to_owned(),
                    json!(u64::try_from(rows.len()).unwrap_or(u64::MAX)),
                );
                values.insert("h2h_prior_meetings".to_owned(), json!(rows));
            }
        }

        // Venue, attendance, officials.
        if let Some(game_info) = value.get("gameInfo") {
            if let Some(venue) = game_info.get("venue") {
                put(
                    &mut values,
                    "game_venue_id",
                    id_to_string(venue.get("id")).map(Value::from),
                );
                put(
                    &mut values,
                    "game_venue_name",
                    string_field(venue, "fullName").map(Value::from),
                );
                put(
                    &mut values,
                    "game_venue_capacity",
                    venue
                        .get("capacity")
                        .and_then(Value::as_i64)
                        .map(Value::from),
                );
                put(
                    &mut values,
                    "game_venue_indoor_flag",
                    venue
                        .get("indoor")
                        .and_then(Value::as_bool)
                        .map(Value::from),
                );
                if let Some(address) = venue.get("address") {
                    put(
                        &mut values,
                        "game_venue_city",
                        string_field(address, "city").map(Value::from),
                    );
                    put(
                        &mut values,
                        "game_venue_state",
                        string_field(address, "state").map(Value::from),
                    );
                }
            }
            put(
                &mut values,
                "game_attendance",
                game_info
                    .get("attendance")
                    .and_then(Value::as_i64)
                    .map(Value::from),
            );
            // Officials appear around tip-off rather than early pregame, so an
            // absent block is the expected pregame state, not an anomaly.
            if let Some(officials) = game_info.get("officials").and_then(Value::as_array) {
                values.insert(
                    "game_official_count".to_owned(),
                    json!(u64::try_from(officials.len()).unwrap_or(u64::MAX)),
                );
            }
        }

        // National broadcast list.
        if let Some(broadcasts) = value.get("broadcasts").and_then(Value::as_array) {
            let names: Vec<Value> = broadcasts
                .iter()
                .take(MAX_LIST_ENTRIES)
                .filter_map(broadcast_name)
                .map(Value::from)
                .collect();
            if !names.is_empty() {
                values.insert("game_broadcast_national_list".to_owned(), json!(names));
            }
        }

        // Live injury report volume (see the doc comment for why this is a count).
        if let Some(teams) = value.get("injuries").and_then(Value::as_array) {
            let entries: usize = teams
                .iter()
                .filter_map(|team| team.get("injuries").and_then(Value::as_array))
                .map(Vec::len)
                .sum();
            values.insert(
                "summary_injury_report_entry_count".to_owned(),
                json!(u64::try_from(entries).unwrap_or(u64::MAX)),
            );
        }

        guard_values(&values)?;
        let event = self.source_event(
            record,
            SUMMARY_SOURCE_ID,
            record.received_at.clone(),
            values,
        )?;
        outcome.events.push(event);
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

impl FeedAdapter for NbaEspnGamesAdapter {
    fn ingest(&mut self, record: &RawRecord) -> Result<IngestOutcome, AdapterError> {
        match self.guard.observe(record)? {
            GuardAction::Configuration(_) => Err(AdapterError::InvalidRecord(
                "NBA ESPN games captures do not accept configuration records".to_owned(),
            )),
            GuardAction::Connected => {
                self.pending = None;
                Ok(IngestOutcome::notice(FeedNotice::info(
                    FeedNoticeCode::Connected,
                    "NBA ESPN games poll session connected; awaiting first cycle",
                )))
            }
            GuardAction::Disconnected => {
                let mut outcome = IngestOutcome::default();
                if let Some(orphan) = self.pending.take() {
                    outcome.notices.push(FeedNotice::info(
                        FeedNoticeCode::StateInconsistent,
                        format!(
                            "NBA ESPN games poll session ended with an unpaired descriptor for \
                             entity `{}`",
                            bounded_token(&orphan.entity_id)
                        ),
                    ));
                }
                outcome.notices.push(FeedNotice::info(
                    FeedNoticeCode::Disconnected,
                    "NBA ESPN games poll session ended",
                ));
                Ok(outcome)
            }
            // Like slice 2 and unlike slice 1, the outbound descriptor is not
            // inert: it is the fallback identity carrier for a body with no id.
            GuardAction::Outbound => {
                let payload = record.payload.as_deref().unwrap_or_default();
                Ok(self.observe_descriptor(payload))
            }
            GuardAction::Failed(_) => Err(AdapterError::InvalidRecord(
                "NBA ESPN games captures do not accept failure records".to_owned(),
            )),
            GuardAction::Inbound(payload) => self.ingest_payload(record, payload),
        }
    }
}

/// The three odds snapshot blocks, in the order the catalog names them.
const SNAPSHOT_PHASES: [&str; 3] = ["open", "close", "current"];

// --- Extraction helpers.

/// Fail closed once one event would carry an implausible number of values.
fn guard_values(values: &BTreeMap<String, Value>) -> Result<(), AdapterError> {
    if values.len() > MAX_VALUES_PER_EVENT {
        return Err(AdapterError::InvalidPayload(format!(
            "NBA ESPN games event value count exceeded {MAX_VALUES_PER_EVENT}"
        )));
    }
    Ok(())
}

/// Insert one optional value, leaving the field absent when the source has none.
fn put(values: &mut BTreeMap<String, Value>, key: &str, value: Option<Value>) {
    if let Some(value) = value {
        values.insert(key.to_owned(), value);
    }
}

/// Insert the American and decimal prices of one ESPN price object. Price objects
/// carry the full odds shape (`value`, `displayValue`, `alternateDisplayValue`,
/// `decimal`, `fraction`, `american`); the catalog captures the American and
/// decimal forms, both as canonical decimal text.
fn put_price(values: &mut BTreeMap<String, Value>, stem: &str, phase: &str, price: &RawValue) {
    let Some(price) = raw_object(price) else {
        return;
    };
    put(
        values,
        &format!("{stem}_american_{phase}"),
        price
            .get("american")
            .copied()
            .and_then(|raw| market_decimal(raw, "american"))
            .map(Value::from),
    );
    // `decimal` and `value` carry the same decimal price on every observed price
    // object (`{value:1.9, ..., decimal:1.9, ...}`), so `value` is the fallback
    // when a body ships one and not the other. `value` is NEVER used this way for
    // a line object -- there it is the line, and `put_line` reads it.
    put(
        values,
        &format!("{stem}_decimal_{phase}"),
        price
            .get("decimal")
            .copied()
            .and_then(|raw| market_decimal(raw, "decimal"))
            .or_else(|| {
                price
                    .get("value")
                    .copied()
                    .and_then(|raw| market_decimal(raw, "value"))
            })
            .map(Value::from),
    );
}

/// Insert the numeric LINE carried by an ESPN line object (a `pointSpread`, or
/// the totals `total` block). Line objects carry NO numeric field in the open and
/// close snapshots -- only the signed display strings -- so the line is read from
/// `value` when a numeric one exists and parsed out of `american` otherwise.
fn put_line(values: &mut BTreeMap<String, Value>, key: &str, line: &RawValue) {
    let Some(line) = raw_object(line) else {
        return;
    };
    let text = line
        .get("value")
        .copied()
        .and_then(|raw| market_decimal(raw, "value"))
        .or_else(|| {
            line.get("american")
                .copied()
                .and_then(|raw| market_decimal(raw, "american"))
        })
        .or_else(|| {
            line.get("alternateDisplayValue")
                .copied()
                .and_then(|raw| market_decimal(raw, "alternateDisplayValue"))
        });
    put(values, key, text.map(Value::from));
}

/// Reshape one prior meeting into the catalog's fixed row schema.
fn prior_meeting_row(meeting: &Value) -> Value {
    let mut row = serde_json::Map::new();
    if let Some(id) = id_to_string(meeting.get("id")) {
        row.insert("event_id".to_owned(), json!(id));
    }
    if let Some(date) = string_field(meeting, "date") {
        row.insert("date".to_owned(), json!(date));
    }
    if let Some(status) = meeting
        .get("statusType")
        .and_then(|status| string_field(status, "name"))
    {
        row.insert("status_type".to_owned(), json!(status));
    }
    if let Some(competitors) = meeting.get("competitors").and_then(Value::as_array) {
        for competitor in competitors {
            let side = match competitor.get("homeAway").and_then(Value::as_str) {
                Some("home") => "home",
                Some("away") => "away",
                _ => continue,
            };
            if let Some(team_id) = id_to_string(competitor.get("team").and_then(|t| t.get("id"))) {
                row.insert(format!("{side}_team_id"), json!(team_id.clone()));
                if competitor.get("winner").and_then(Value::as_bool) == Some(true) {
                    row.insert("winner_team_id".to_owned(), json!(team_id));
                }
            }
            if let Some(score) = id_to_string(competitor.get("score")) {
                row.insert(format!("{side}_score"), json!(score));
            }
        }
    }
    Value::Object(row)
}

/// Read one broadcast entry's display name, tolerating both the nested `media`
/// shape and a flat name.
fn broadcast_name(broadcast: &Value) -> Option<String> {
    broadcast
        .get("media")
        .and_then(|media| string_field(media, "shortName").or_else(|| string_field(media, "name")))
        .or_else(|| string_field(broadcast, "shortName"))
        .or_else(|| string_field(broadcast, "name"))
}

/// Recover the event id from any core-API `$ref` in the body.
///
/// Both core shapes name their event only inside `$ref` URLs
/// (`.../events/401811026/competitions/401811026/...`): the odds collection names
/// it on each item, the predictor on its own top-level ref. A bounded walk over
/// the parsed body returns the first `/events/<digits>/` segment it finds, which
/// generalizes slice 1's `/id/<digits>/` athlete-href recovery and slice 2's
/// named-segment recovery to "any ref in this body".
fn event_id_from_refs(value: &Value) -> Option<String> {
    let mut stack = vec![value];
    let mut visited = 0usize;
    while let Some(node) = stack.pop() {
        visited = visited.saturating_add(1);
        if visited > MAX_REF_SCAN_NODES {
            return None;
        }
        match node {
            Value::Object(map) => {
                if let Some(href) = map.get("$ref").and_then(Value::as_str)
                    && let Some(id) = path_id_segment(href, "events")
                {
                    return Some(id);
                }
                stack.extend(map.values());
            }
            Value::Array(entries) => stack.extend(entries.iter()),
            _ => {}
        }
    }
    None
}

/// Extract the digits following a `/{segment}/` path element in a URL
/// (`.../events/401811026/competitions/...` with segment `events` yields
/// `401811026`). Local copy of the slice-2 helper of the same name; the sibling's
/// is private to its module and this stream must not reach into it.
/// Extract the digits following a percent-encoded `/event/` path segment
/// (`%2Fevent%2F<digits>`), the form real book gateway urls carry inside their
/// deep-link query parameters. Case-insensitive on the percent-encoding hex.
fn encoded_event_id_segment(href: &str) -> Option<String> {
    let lower = href.to_ascii_lowercase();
    let needle = "%2fevent%2f";
    let start = lower.find(needle)? + needle.len();
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

/// True when an id is a usable ESPN entity id: a non-empty bounded run of ASCII
/// digits. This is the fail-closed check a descriptor must pass before it may key
/// a body, and the same check a body-derived id must pass (slice-2 precedent).
fn is_entity_id(candidate: &str) -> bool {
    !candidate.is_empty()
        && candidate.len() <= 32
        && candidate.bytes().all(|byte| byte.is_ascii_digit())
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

// --- Raw-token navigation. See the NUMERIC POLICY note above for why the market
// shapes are read from raw JSON tokens rather than from `serde_json::Value`.

/// Parse a whole payload as a JSON object of raw tokens.
fn raw_object_from_str(payload: &str) -> Option<BTreeMap<String, &RawValue>> {
    serde_json::from_str(payload).ok()
}

/// Parse one raw token as a JSON object of raw tokens.
fn raw_object(raw: &RawValue) -> Option<BTreeMap<String, &RawValue>> {
    serde_json::from_str(raw.get()).ok()
}

/// Parse one raw token as a JSON array of raw tokens.
fn raw_array(raw: &RawValue) -> Option<Vec<&RawValue>> {
    serde_json::from_str(raw.get()).ok()
}

/// Parse one raw token as a non-empty JSON string.
fn raw_text(raw: &RawValue) -> Option<String> {
    serde_json::from_str::<String>(raw.get())
        .ok()
        .filter(|text| !text.is_empty())
}

/// Parse one raw token as a JSON boolean.
fn raw_bool(raw: &RawValue) -> Option<bool> {
    serde_json::from_str::<bool>(raw.get()).ok()
}

/// Parse one raw token as a JSON integer. Used only for non-market counts
/// (provider priority), never for a price.
fn raw_integer(raw: &RawValue) -> Option<i64> {
    serde_json::from_str::<i64>(raw.get()).ok()
}

/// Read an id that ESPN renders as either a JSON string or a JSON integer.
fn raw_id(raw: &RawValue) -> Option<String> {
    if let Some(text) = raw_text(raw) {
        return Some(text);
    }
    let token = raw.get().trim();
    (!token.is_empty() && token.bytes().all(|byte| byte.is_ascii_digit())).then(|| token.to_owned())
}

/// Convert one ESPN market token into canonical decimal TEXT.
///
/// The token is handed to the shared `decimal::signed_decimal_string` converter,
/// which is the single authority for decimal conversion in this crate and rejects
/// any token it cannot represent exactly. Two ESPN-specific normalizations happen
/// first:
///
///  * American prices and point spreads are published as SIGNED display strings
///    (`"+180"`, `"-6.5"`); the shared decimal grammar has no leading `+`, so an
///    explicit plus is removed before conversion.
///  * A token that is not a number at all (`"EVEN"`, `null`, `""`) yields `None`,
///    and the field is simply absent from the emission rather than guessed at.
fn market_decimal(raw: &RawValue, field: &str) -> Option<String> {
    let token = raw.get().trim();
    if let Some(rest) = token.strip_prefix("\"+") {
        let normalized = RawValue::from_string(format!("\"{rest}")).ok()?;
        return signed_decimal_string(&normalized, field).ok();
    }
    signed_decimal_string(raw, field).ok()
}

/// Convert an ESPN 0-100 model projection into the catalog's 0-1 probability.
///
/// The division is an exact decimal point shift and is verified to round-trip
/// before the value is emitted, so a token that cannot be scaled exactly yields
/// no field rather than a rounded one.
fn scaled_probability(published: &str) -> Option<String> {
    let published = signals_core::parse_decimal_text(published).ok()?;
    let hundred = rust_decimal::Decimal::from(100);
    let scaled = published.checked_div(hundred)?;
    if scaled.checked_mul(hundred)? != published {
        return None;
    }
    Some(scaled.normalize().to_string())
}

/// Parse an ESPN ISO-8601 timestamp and re-render it through the single shared
/// event window (millisecond plausibility bound + canonical RFC 3339). ESPN mixes
/// second-precision (`2026-08-30T23:58:12Z`) and minute-precision
/// (`2026-04-10T22:50Z`, which chrono's RFC 3339 parser rejects), so a minute
/// fallback format is tried before failing. Local copy of the slice-1 helper of
/// the same name: the sibling's is private to its module and slice 1 is not
/// modified by this slice.
fn parse_espn_iso_to_rfc3339(raw: &str) -> Result<String, String> {
    let parsed = DateTime::parse_from_rfc3339(raw)
        .map(|value| value.with_timezone(&Utc))
        .or_else(|_| {
            NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%MZ").map(|naive| naive.and_utc())
        })
        .map_err(|_| format!("unrecognized ISO-8601 timestamp `{}`", bounded_token(raw)))?;
    millis_to_rfc3339(parsed.timestamp_millis(), "event_time").map_err(|error| error.to_string())
}

fn skip_notice(detail: &'static str) -> FeedNotice {
    FeedNotice::info(FeedNoticeCode::UnsupportedMessage, detail)
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
        MAX_EVENTS_PER_CYCLE, NbaEspnGamesAdapter, ODDS_TEMPLATE, POLL_CADENCE, PREDICTOR_TEMPLATE,
        SCOREBOARD_TEMPLATE, SUMMARY_CYCLE_MODULUS, SUMMARY_TEMPLATE, Shape, classify,
        classify_series, is_core_host, outbound_descriptor, render,
    };
    use crate::{CaptureStream, FeedAdapter, FeedNoticeCode, IngestOutcome, RawRecord, RecordKind};
    use serde_json::{Value, json};

    fn adapter() -> NbaEspnGamesAdapter {
        NbaEspnGamesAdapter::new("test-epoch").expect("adapter constructs")
    }

    fn record(ordinal: u64, epoch: u64, kind: RecordKind, payload: Option<&str>) -> RawRecord {
        RawRecord {
            schema_version: 1,
            provider: crate::Provider::NbaEspnGames,
            stream: CaptureStream::NbaEspnGames,
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

    fn descriptor(ordinal: u64, entity_id: &str) -> RawRecord {
        record(
            ordinal,
            1,
            RecordKind::OutboundText,
            Some(&outbound_descriptor(
                "https://example.invalid/x",
                0,
                1,
                entity_id,
            )),
        )
    }

    // --- Minimal synthetic fixtures. Shapes, field paths and VALUES mirror the
    // real 2026-08-30 fetches of event 401811026 (DET @ CHA) exactly; the giant
    // bodies themselves are not committed.

    const SCOREBOARD: &str = r#"{"leagues":[{"calendar":["2026-04-10"]}],
        "events":[{"id":"401811026","uid":"s:40~l:46~e:401811026","name":"Detroit Pistons at Charlotte Hornets",
          "shortName":"DET @ CHA","date":"2026-04-10T23:00Z","season":{"year":2026,"type":2},
          "status":{"type":{"id":"3","name":"STATUS_FINAL","state":"post","completed":true}},
          "competitions":[{"id":"401811026","competitors":[
            {"homeAway":"home","score":"120","team":{"id":"30","abbreviation":"CHA"}},
            {"homeAway":"away","score":"110","team":{"id":"8","abbreviation":"DET"}}]}]}]}"#;

    /// The real odds item shape: the open/close/current triplets at both levels,
    /// with the DET @ CHA numbers (total 222.5 -> 225.5, CHA -2.5 -> -6.5,
    /// away moneyline +114 -> +180, home -135 -> -218).
    const ODDS: &str = r#"{"count":1,"pageIndex":1,"pageSize":100,"pageCount":1,"items":[
        {"$ref":"http://sports.core.api.espn.com/v2/sports/basketball/leagues/nba/events/401811026/competitions/401811026/odds/100?lang=en",
         "provider":{"id":"100","name":"Draft Kings","priority":1},
         "details":"CHA -6.5","overUnder":225.5,"spread":-6.5,"overOdds":-112.0,"underOdds":-108.0,
         "moneylineWinner":false,"spreadWinner":false,
         "open":{"over":{"value":1.909,"american":"-110"},"under":{"value":1.909,"american":"-110"},
                 "total":{"value":222.5,"american":"222.5"}},
         "close":{"over":{"value":1.893,"decimal":1.893,"american":"-112"},
                  "under":{"value":1.926,"decimal":1.926,"american":"-108"},
                  "total":{"value":225.5,"american":"225.5"}},
         "current":{"over":{"value":1.893,"american":"-112"},"under":{"value":1.926,"american":"-108"},
                    "total":{"value":225.5,"american":"225.5"}},
         "awayTeamOdds":{"favorite":false,"underdog":true,"moneyLine":180,"spreadOdds":-115.0,
           "team":{"$ref":"http://sports.core.api.espn.com/v2/sports/basketball/leagues/nba/seasons/2026/teams/8?lang=en"},
           "open":{"pointSpread":{"american":"+2.5"},"spread":{"value":1.909,"american":"-110"},
                   "moneyLine":{"value":2.14,"american":"+114"}},
           "close":{"pointSpread":{"american":"+6.5"},"spread":{"value":1.869,"american":"-115"},
                    "moneyLine":{"value":2.8,"american":"+180"}},
           "current":{"pointSpread":{"american":"+6.5"},"spread":{"value":1.869,"american":"-115"},
                      "moneyLine":{"value":2.8,"american":"+180"}}},
         "homeTeamOdds":{"favorite":true,"underdog":false,"moneyLine":-218,"spreadOdds":-105.0,
           "team":{"$ref":"http://sports.core.api.espn.com/v2/sports/basketball/leagues/nba/seasons/2026/teams/30?lang=en"},
           "open":{"pointSpread":{"american":"-2.5"},"spread":{"value":1.909,"american":"-110"},
                   "moneyLine":{"value":1.741,"american":"-135"}},
           "close":{"pointSpread":{"american":"-6.5"},"spread":{"value":1.952,"american":"-105"},
                    "moneyLine":{"value":1.459,"american":"-218"}},
           "current":{"pointSpread":{"american":"-6.5"},"spread":{"value":1.952,"american":"-105"},
                      "moneyLine":{"value":1.459,"american":"-218"}}}}]}"#;

    const ODDS_EMPTY: &str = r#"{"count":0,"pageIndex":1,"pageSize":100,"pageCount":0,"items":[]}"#;

    /// Two items: the first carries a provider (which is what classifies the body
    /// as odds), the second does not. The second must be skipped with a notice
    /// rather than emitted with no book identity.
    const ODDS_SECOND_ITEM_NO_PROVIDER: &str = r#"{"count":2,"pageIndex":1,"pageSize":100,"pageCount":1,"items":[
        {"$ref":"http://sports.core.api.espn.com/v2/sports/basketball/leagues/nba/events/401811026/competitions/401811026/odds/100?lang=en",
         "provider":{"id":"100","name":"Draft Kings","priority":1},
         "details":"CHA -6.5","spread":-6.5},
        {"$ref":"http://sports.core.api.espn.com/v2/sports/basketball/leagues/nba/events/401811026/competitions/401811026/odds/200?lang=en",
         "details":"CHA -7.0","spread":-7.0}]}"#;

    /// One item carrying a two-entry `links[]` array of DK bet-slip deep links,
    /// each tagged with a market rel and embedding DK's own event id 33954994.
    const ODDS_WITH_LINKS: &str = r#"{"count":1,"pageIndex":1,"pageSize":100,"pageCount":1,"items":[
        {"$ref":"http://sports.core.api.espn.com/v2/sports/basketball/leagues/nba/events/401811026/competitions/401811026/odds/100?lang=en",
         "provider":{"id":"100","name":"Draft Kings","priority":1},
         "details":"CHA -6.5","spread":-6.5,
         "links":[
           {"language":"en-US","rel":["home","bet-slip"],"href":"https://dksb.sng.link/As9kz/c37t?_dl=x&pcid=100&preurl=https%3A%2F%2Fsportsbook.draftkings.com%2Fevent%2F33954994%3Foutcomes%3Dhome"},
           {"language":"en-US","rel":["over","bet-slip"],"href":"https://dksb.sng.link/As9kz/c37t?_dl=x&pcid=100&preurl=https%3A%2F%2Fsportsbook.draftkings.com%2Fevent%2F33954994%3Foutcomes%3Dover"}]}]}"#;

    /// The real predictor shape: only the away side is populated on the reference
    /// event, so this fixture pins the one-sided case.
    const PREDICTOR: &str = r#"{"$ref":"http://sports.core.api.espn.com/v2/sports/basketball/leagues/nba/events/401811026/competitions/401811026/predictor?lang=en",
        "lastModified":"2026-04-10T22:50Z",
        "homeTeam":{"team":{"$ref":"http://sports.core.api.espn.com/v2/sports/basketball/leagues/nba/seasons/2026/teams/30"},"statistics":[]},
        "awayTeam":{"team":{"$ref":"http://sports.core.api.espn.com/v2/sports/basketball/leagues/nba/seasons/2026/teams/8"},
          "statistics":[
            {"name":"gameProjection","value":46.722},
            {"name":"matchupQuality","value":99.0},
            {"name":"teamChanceLoss","value":53.278},
            {"name":"teamPredPtDiff","value":-1.028},
            {"name":"teamExpectedPts","value":112.525},
            {"name":"oppExpectedPts","value":113.553}]}}"#;

    const SUMMARY: &str = r#"{"boxscore":{"teams":[]},
        "header":{"id":"401811026","season":{"year":2026,"type":2},
          "competitions":[{"id":"401811026","boxscoreAvailable":true}]},
        "seasonseries":[{"type":"season","title":"Regular Season Series","summary":"DET wins series 3-0",
          "completed":true,"totalCompetitions":3,"seriesScore":"3-0",
          "events":[{"id":"401811000","date":"2026-01-05T00:00Z","statusType":{"name":"STATUS_FINAL"},
            "competitors":[{"homeAway":"home","winner":false,"score":"101","team":{"id":"30"}},
                           {"homeAway":"away","winner":true,"score":"113","team":{"id":"8"}}]}]}],
        "injuries":[{"team":{"id":"30"},"injuries":[{"status":"Day-To-Day"},{"status":"Out"}]},
                    {"team":{"id":"8"},"injuries":[{"status":"Out"}]}],
        "gameInfo":{"venue":{"id":"1893","fullName":"Spectrum Center","capacity":19077,"indoor":true,
            "address":{"city":"Charlotte","state":"NC"}},
          "attendance":19623,
          "officials":[{"fullName":"Courtney Kirkland"},{"fullName":"A B"},{"fullName":"C D"}]},
        "broadcasts":[{"media":{"shortName":"FanDuel SN SE"}},{"media":{"shortName":"ABC"}}]}"#;

    fn ingest(adapter: &mut NbaEspnGamesAdapter, records: &[RawRecord]) -> IngestOutcome {
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
    fn templates_render_the_researched_urls_and_route_by_host() {
        // Each base path is one the surface research fetched live on 2026-08-30;
        // the odds path's `limit=100` is the one unverified addition and says so
        // on the constant. A silent drift in any of them re-404s the cycle, so
        // they are pinned.
        assert_eq!(
            render(SCOREBOARD_TEMPLATE, "20260410", ""),
            "https://site.api.espn.com/apis/site/v2/sports/basketball/nba/scoreboard?dates=20260410"
        );
        assert_eq!(
            render(ODDS_TEMPLATE, "", "401811026"),
            "https://sports.core.api.espn.com/v2/sports/basketball/leagues/nba/events/401811026/competitions/401811026/odds?limit=100"
        );
        assert_eq!(
            render(PREDICTOR_TEMPLATE, "", "401811026"),
            "https://sports.core.api.espn.com/v2/sports/basketball/leagues/nba/events/401811026/competitions/401811026/predictor"
        );
        assert_eq!(
            render(SUMMARY_TEMPLATE, "", "401811026"),
            "https://site.api.espn.com/apis/site/v2/sports/basketball/nba/summary?event=401811026"
        );
        // TLS routing: only the core host takes the TLS-1.2-permitted client.
        assert!(is_core_host(&render(ODDS_TEMPLATE, "", "1")));
        assert!(is_core_host(&render(PREDICTOR_TEMPLATE, "", "1")));
        assert!(!is_core_host(&render(SCOREBOARD_TEMPLATE, "20260410", "")));
        assert!(!is_core_host(&render(SUMMARY_TEMPLATE, "", "1")));
        // Every compiled endpoint stays on TLS.
        for template in [
            SCOREBOARD_TEMPLATE,
            ODDS_TEMPLATE,
            PREDICTOR_TEMPLATE,
            SUMMARY_TEMPLATE,
        ] {
            assert!(template.starts_with("https://"), "{template} must be https");
        }
    }

    #[test]
    fn cadence_and_bounds_are_the_ruled_values() {
        assert_eq!(POLL_CADENCE, std::time::Duration::from_secs(600));
        // Hourly summaries at the 600 s cycle cadence.
        assert_eq!(SUMMARY_CYCLE_MODULUS, 6);
        assert_eq!(MAX_EVENTS_PER_CYCLE, 30);
    }

    #[test]
    fn outbound_descriptor_is_the_exact_shape() {
        assert_eq!(
            outbound_descriptor("https://x/y", 3, 2, "401811026"),
            r#"{"request":"https://x/y","cycle":3,"index":2,"entity_id":"401811026"}"#
        );
    }

    // --- Discriminators.

    #[test]
    fn discriminators_separate_the_four_shapes_and_never_panic() {
        assert_eq!(classify_series(SCOREBOARD), "nba_espn_games:scoreboard");
        assert_eq!(classify_series(ODDS), "nba_espn_games:odds");
        assert_eq!(classify_series(ODDS_EMPTY), "nba_espn_games:odds");
        assert_eq!(classify_series(PREDICTOR), "nba_espn_games:predictor");
        assert_eq!(classify_series(SUMMARY), "nba_espn_games:summary");
        assert_eq!(classify_series("not json at all"), super::UNKNOWN_SERIES);
        assert_eq!(classify_series(r#"{"other":true}"#), super::UNKNOWN_SERIES);
        // A paginated collection that is NOT an odds list is not coerced.
        assert_eq!(
            classify_series(r#"{"pageCount":1,"items":[{"id":"0","type":"total"}]}"#),
            super::UNKNOWN_SERIES
        );
        // A predictor that ships only the populated side still classifies: on the
        // verified reference event ESPN populated one side only, so demanding
        // both team keys would refuse a real body.
        assert_eq!(
            classify_series(r#"{"lastModified":"2026-04-10T22:50Z","awayTeam":{"statistics":[]}}"#),
            "nba_espn_games:predictor"
        );
        // ...and the summary rule is checked first, so a composed body carrying
        // both a boxscore and team blocks is never taken for a predictor.
        assert_eq!(
            classify(
                &serde_json::from_str::<Value>(
                    r#"{"boxscore":{},"header":{"id":"1"},"homeTeam":{}}"#
                )
                .expect("fixture parses")
            ),
            Some(Shape::Summary)
        );
    }

    #[test]
    fn a_scoreboard_events_array_is_recognized_and_silent_in_this_stream() {
        // Slice 1 emits one event per game from the very same body. In THIS stream
        // the scoreboard is the cycle's request-list generator, so it classifies
        // cleanly and emits nothing at all -- no event AND no notice. The two
        // streams never collide (separate CaptureStreams, separate adapters), but
        // the silence is what keeps one observation from being published twice.
        let value: Value = serde_json::from_str(SCOREBOARD).expect("fixture parses");
        assert_eq!(classify(&value), Some(Shape::Scoreboard));

        let mut adapter = adapter();
        let outcome = ingest(&mut adapter, &[connected(1), inbound(2, SCOREBOARD)]);
        // Only the Connected notice; the scoreboard body adds nothing.
        assert!(outcome.events.is_empty());
        assert_eq!(outcome.notices.len(), 1);
        assert_eq!(outcome.notices[0].code, FeedNoticeCode::Connected);
    }

    // --- Odds.

    #[test]
    fn odds_extracts_the_open_close_current_triplets_as_decimal_text() {
        let mut adapter = adapter();
        let outcome = ingest(&mut adapter, &[connected(1), inbound(2, ODDS)]);
        assert_eq!(outcome.events.len(), 1);
        assert_eq!(outcome.events[0].source_id, "nba.espn.games.odds");
        // Identity came from the item's own core `$ref`, not from a descriptor.
        assert_eq!(
            value_of(&outcome, 0, "espn_event_id"),
            Some(&json!("401811026"))
        );
        assert_eq!(
            value_of(&outcome, 0, "event_id_basis"),
            Some(&json!("response_body"))
        );
        // Provider identity.
        assert_eq!(
            value_of(&outcome, 0, "odds_provider_id"),
            Some(&json!("100"))
        );
        assert_eq!(
            value_of(&outcome, 0, "odds_provider_name"),
            Some(&json!("Draft Kings"))
        );
        assert_eq!(
            value_of(&outcome, 0, "odds_provider_priority"),
            Some(&json!(1))
        );
        // The total moved 222.5 -> 225.5; every value is decimal TEXT.
        assert_eq!(
            value_of(&outcome, 0, "odds_total_line_open"),
            Some(&json!("222.5"))
        );
        assert_eq!(
            value_of(&outcome, 0, "odds_total_line_close"),
            Some(&json!("225.5"))
        );
        assert_eq!(
            value_of(&outcome, 0, "odds_total_line_current"),
            Some(&json!("225.5"))
        );
        assert_eq!(
            value_of(&outcome, 0, "odds_total_over_price_american_open"),
            Some(&json!("-110"))
        );
        // The CURRENT prices come from the item-level native numbers, which ESPN
        // publishes with a trailing `.0`. The converter preserves the provider's
        // exact token rather than restyling it -- the same verbatim-text rule the
        // BTC adapters follow -- so the open string form (`"-110"`) and the
        // current numeric form (`-112.0`) keep their own scales. Both are exact
        // decimal text; a consumer compares them numerically, never as strings.
        assert_eq!(
            value_of(&outcome, 0, "odds_total_over_price_american_current"),
            Some(&json!("-112.0"))
        );
        // `decimal` when the body ships it, `value` when it does not (the two are
        // the same price on every observed price object).
        assert_eq!(
            value_of(&outcome, 0, "odds_total_under_price_decimal_close"),
            Some(&json!("1.926"))
        );
        assert_eq!(
            value_of(&outcome, 0, "odds_total_over_price_decimal_open"),
            Some(&json!("1.909"))
        );
        // The spread moved CHA -2.5 -> -6.5, and the away side is read from its
        // OWN published line rather than by negating the home line. The leading
        // `+` of the away line is normalized away by the decimal converter.
        assert_eq!(
            value_of(&outcome, 0, "odds_spread_line_home_open"),
            Some(&json!("-2.5"))
        );
        assert_eq!(
            value_of(&outcome, 0, "odds_spread_line_home_close"),
            Some(&json!("-6.5"))
        );
        assert_eq!(
            value_of(&outcome, 0, "odds_spread_line_away_open"),
            Some(&json!("2.5"))
        );
        assert_eq!(
            value_of(&outcome, 0, "odds_spread_line_away_current"),
            Some(&json!("6.5"))
        );
        // Item-level `spread` is home-relative and is the preferred source for
        // the current home line.
        assert_eq!(
            value_of(&outcome, 0, "odds_spread_line_home_current"),
            Some(&json!("-6.5"))
        );
        // Moneylines: away +114 -> +180, home -135 -> -218.
        assert_eq!(
            value_of(&outcome, 0, "odds_moneyline_away_american_open"),
            Some(&json!("114"))
        );
        assert_eq!(
            value_of(&outcome, 0, "odds_moneyline_away_american_current"),
            Some(&json!("180"))
        );
        assert_eq!(
            value_of(&outcome, 0, "odds_moneyline_home_american_current"),
            Some(&json!("-218"))
        );
        assert_eq!(
            value_of(&outcome, 0, "odds_moneyline_home_price_decimal_open"),
            None,
            "prices are keyed by the documented stems only"
        );
        assert_eq!(
            value_of(&outcome, 0, "odds_moneyline_home_decimal_open"),
            Some(&json!("1.741"))
        );
        // Team ids off the side `team.$ref`, and the settlement/state flags.
        assert_eq!(
            value_of(&outcome, 0, "odds_team_id_home"),
            Some(&json!("30"))
        );
        assert_eq!(
            value_of(&outcome, 0, "odds_team_id_away"),
            Some(&json!("8"))
        );
        assert_eq!(
            value_of(&outcome, 0, "odds_spread_favorite_home_current"),
            Some(&json!(true))
        );
        assert_eq!(
            value_of(&outcome, 0, "odds_live_block_present"),
            Some(&json!(false))
        );
        assert_eq!(
            value_of(&outcome, 0, "odds_snapshot_basis"),
            Some(&json!("current"))
        );
        // Load-bearing capture provenance: the allowlisted UA, one authority.
        assert_eq!(
            value_of(&outcome, 0, "odds_capture_user_agent"),
            Some(&json!(crate::nba_espn::CAPTURE_USER_AGENT))
        );
        assert!(
            value_of(&outcome, 0, "odds_payload_sha256")
                .and_then(Value::as_str)
                .is_some_and(|hash| hash.len() == 64)
        );
        // No source timestamp exists on this shape (the movement history ESPN
        // does not keep is exactly why we poll), so the basis is honest receipt.
        assert_eq!(
            value_of(&outcome, 0, "event_time_basis"),
            Some(&json!("receipt"))
        );
    }

    #[test]
    fn identical_odds_on_two_cycles_both_emit_because_the_series_is_the_product() {
        // THE no-dedup semantics test. Slice 1 collapses an unchanged snapshot;
        // this stream must not, because ESPN's own movement endpoint returns
        // `{count:0}` and our poll sequence IS the movement series. Two cycles of
        // a byte-identical odds body are two observations, and both must survive.
        let mut adapter = adapter();
        let outcome = ingest(
            &mut adapter,
            &[connected(1), inbound(2, ODDS), inbound(3, ODDS)],
        );
        assert_eq!(outcome.events.len(), 2);
        assert_eq!(outcome.events[0].values, outcome.events[1].values);
        // Distinct, monotonic sequence numbers keep the two orderable.
        assert_eq!(outcome.events[0].sequence, 0);
        assert_eq!(outcome.events[1].sequence, 1);
        // And no notice is raised for the repeat: it is the expected steady state.
        assert!(
            outcome
                .notices
                .iter()
                .all(|notice| notice.code == FeedNoticeCode::Connected)
        );
    }

    #[test]
    fn an_empty_odds_collection_is_silent_and_not_an_odd_body() {
        // A game with no posted line answers `{count:0, items:[]}` every poll. It
        // is recognized, emits nothing, and raises no notice -- a per-poll notice
        // per lineless game would be pure noise.
        let mut adapter = adapter();
        let outcome = ingest(
            &mut adapter,
            &[
                connected(1),
                descriptor(2, "401811026"),
                inbound(3, ODDS_EMPTY),
            ],
        );
        assert!(outcome.events.is_empty());
        assert_eq!(outcome.notices.len(), 1);
        assert_eq!(outcome.notices[0].code, FeedNoticeCode::Connected);
    }

    #[test]
    fn a_provider_less_odds_item_is_skipped_with_a_notice() {
        // The first item has a provider (that is what makes the body odds); the
        // second does not, so it would emit a nameless line. It must be skipped
        // with a StateInconsistent notice naming its index, and exactly one event
        // (the first) must survive.
        let mut adapter = adapter();
        let outcome = ingest(
            &mut adapter,
            &[connected(1), inbound(2, ODDS_SECOND_ITEM_NO_PROVIDER)],
        );
        assert_eq!(outcome.events.len(), 1);
        assert_eq!(
            value_of(&outcome, 0, "odds_provider_id"),
            Some(&json!("100"))
        );
        let skips: Vec<_> = outcome
            .notices
            .iter()
            .filter(|notice| notice.code == FeedNoticeCode::StateInconsistent)
            .collect();
        assert_eq!(skips.len(), 1);
        assert!(
            skips[0].detail.contains("item 1"),
            "the skip notice names the offending item index: {}",
            skips[0].detail
        );
    }

    #[test]
    fn odds_extracts_the_bet_slip_links_and_book_event_id() {
        let mut adapter = adapter();
        let outcome = ingest(&mut adapter, &[connected(1), inbound(2, ODDS_WITH_LINKS)]);
        assert_eq!(outcome.events.len(), 1);
        // Both market-rel hrefs, in body order, as a string array.
        assert_eq!(
            value_of(&outcome, 0, "odds_bet_slip_links"),
            Some(&json!([
                "https://dksb.sng.link/As9kz/c37t?_dl=x&pcid=100&preurl=https%3A%2F%2Fsportsbook.draftkings.com%2Fevent%2F33954994%3Foutcomes%3Dhome",
                "https://dksb.sng.link/As9kz/c37t?_dl=x&pcid=100&preurl=https%3A%2F%2Fsportsbook.draftkings.com%2Fevent%2F33954994%3Foutcomes%3Dover"
            ]))
        );
        // The book's own event id, read off the first bet-slip href.
        assert_eq!(
            value_of(&outcome, 0, "odds_book_event_id"),
            Some(&json!("33954994"))
        );
    }

    // --- Predictor.

    #[test]
    fn predictor_extracts_model_fields_on_the_source_timestamp() {
        let mut adapter = adapter();
        let outcome = ingest(&mut adapter, &[connected(1), inbound(2, PREDICTOR)]);
        assert_eq!(outcome.events.len(), 1);
        assert_eq!(outcome.events[0].source_id, "nba.espn.games.predictor");
        assert_eq!(
            value_of(&outcome, 0, "espn_event_id"),
            Some(&json!("401811026"))
        );
        // Published on 0-100, retained verbatim, plus the catalog's 0-1 form.
        assert_eq!(
            value_of(&outcome, 0, "away_bpi_game_projection"),
            Some(&json!("46.722"))
        );
        assert_eq!(
            value_of(&outcome, 0, "away_bpi_win_prob"),
            Some(&json!("0.46722"))
        );
        assert_eq!(
            value_of(&outcome, 0, "away_bpi_loss_prob"),
            Some(&json!("0.53278"))
        );
        assert_eq!(
            value_of(&outcome, 0, "away_bpi_pred_pt_diff"),
            Some(&json!("-1.028"))
        );
        assert_eq!(
            value_of(&outcome, 0, "away_bpi_team_expected_pts"),
            Some(&json!("112.525"))
        );
        assert_eq!(
            value_of(&outcome, 0, "away_bpi_opp_expected_pts"),
            Some(&json!("113.553"))
        );
        // Game-level, never side-prefixed.
        // Verbatim provider token, trailing zero and all.
        assert_eq!(
            value_of(&outcome, 0, "bpi_matchup_quality"),
            Some(&json!("99.0"))
        );
        // Only the away side carried statistics on the reference event, and no
        // complement is ever computed for the home side.
        assert_eq!(
            value_of(&outcome, 0, "bpi_projection_source_mode"),
            Some(&json!("predictor_one_team"))
        );
        assert_eq!(value_of(&outcome, 0, "home_bpi_win_prob"), None);
        // `lastModified` is minute-precision and is the envelope's source time.
        assert_eq!(
            value_of(&outcome, 0, "event_time_basis"),
            Some(&json!("source"))
        );
        assert_eq!(
            value_of(&outcome, 0, "event_time"),
            Some(&json!("2026-04-10T22:50:00.000Z"))
        );
        assert_eq!(
            value_of(&outcome, 0, "bpi_predictor_last_modified"),
            Some(&json!("2026-04-10T22:50:00.000Z"))
        );
        assert_eq!(outcome.events[0].event_time, "2026-04-10T22:50:00.000Z");
    }

    #[test]
    fn a_predictor_with_two_populated_sides_reports_both_teams_mode() {
        let both = r#"{"$ref":"http://x/events/401811026/competitions/401811026/predictor",
            "lastModified":"2026-04-10T22:50Z",
            "homeTeam":{"statistics":[{"name":"gameProjection","value":53.278}]},
            "awayTeam":{"statistics":[{"name":"gameProjection","value":46.722}]}}"#;
        let mut adapter = adapter();
        let outcome = ingest(&mut adapter, &[connected(1), inbound(2, both)]);
        assert_eq!(outcome.events.len(), 1);
        assert_eq!(
            value_of(&outcome, 0, "bpi_projection_source_mode"),
            Some(&json!("predictor_both_teams"))
        );
        assert_eq!(
            value_of(&outcome, 0, "home_bpi_win_prob"),
            Some(&json!("0.53278"))
        );
    }

    #[test]
    fn a_predictor_with_no_populated_statistics_is_silent() {
        // Pregame, before the model publishes, both sides carry empty (or absent)
        // `statistics`. That is the recognized steady state -- like the empty-odds
        // collection -- so it emits nothing and raises no notice.
        let empty = r#"{"$ref":"http://x/events/401811026/competitions/401811026/predictor",
            "lastModified":"2026-04-10T22:50Z",
            "homeTeam":{"statistics":[]},"awayTeam":{"statistics":[]}}"#;
        let mut adapter = adapter();
        let outcome = ingest(&mut adapter, &[connected(1), inbound(2, empty)]);
        assert!(outcome.events.is_empty());
        assert!(
            outcome
                .notices
                .iter()
                .all(|notice| notice.code == FeedNoticeCode::Connected),
            "an empty predictor raises no notice of its own"
        );
    }

    #[test]
    fn a_descriptor_agreeing_with_the_body_id_is_silent() {
        // Body id and paired descriptor name the same event: the cross-check is
        // satisfied and raises no notice. The body id keys the event.
        let mut adapter = adapter();
        let outcome = ingest(
            &mut adapter,
            &[
                connected(1),
                descriptor(2, "401811026"),
                inbound(3, PREDICTOR),
            ],
        );
        assert_eq!(outcome.events.len(), 1);
        assert_eq!(
            value_of(&outcome, 0, "espn_event_id"),
            Some(&json!("401811026"))
        );
        assert!(
            outcome
                .notices
                .iter()
                .all(|notice| notice.code == FeedNoticeCode::Connected),
            "an agreeing descriptor raises no cross-check notice"
        );
    }

    #[test]
    fn a_descriptor_disagreeing_with_the_body_id_notices_and_keeps_the_body_id() {
        // Body id 401811026 disagrees with the paired descriptor's 999999: the
        // mismatch earns one StateInconsistent notice naming both, and the
        // body-derived id stays authoritative.
        let mut adapter = adapter();
        let outcome = ingest(
            &mut adapter,
            &[connected(1), descriptor(2, "999999"), inbound(3, PREDICTOR)],
        );
        assert_eq!(outcome.events.len(), 1);
        assert_eq!(
            value_of(&outcome, 0, "espn_event_id"),
            Some(&json!("401811026"))
        );
        assert_eq!(
            value_of(&outcome, 0, "event_id_basis"),
            Some(&json!("response_body"))
        );
        let mismatches: Vec<_> = outcome
            .notices
            .iter()
            .filter(|notice| notice.code == FeedNoticeCode::StateInconsistent)
            .collect();
        assert_eq!(mismatches.len(), 1);
        assert!(
            mismatches[0].detail.contains("401811026") && mismatches[0].detail.contains("999999"),
            "the notice names both ids: {}",
            mismatches[0].detail
        );
    }

    // --- Summary.

    #[test]
    fn summary_extracts_the_season_series_and_game_context() {
        let mut adapter = adapter();
        let outcome = ingest(&mut adapter, &[connected(1), inbound(2, SUMMARY)]);
        assert_eq!(outcome.events.len(), 1);
        assert_eq!(
            outcome.events[0].source_id,
            "nba.espn.games.summary-context"
        );
        // Identity from `header.id`.
        assert_eq!(
            value_of(&outcome, 0, "espn_event_id"),
            Some(&json!("401811026"))
        );
        assert_eq!(
            value_of(&outcome, 0, "event_id_basis"),
            Some(&json!("response_body"))
        );
        assert_eq!(value_of(&outcome, 0, "season_year"), Some(&json!(2026)));
        // Head to head, the working source.
        assert_eq!(
            value_of(&outcome, 0, "h2h_series_summary_text"),
            Some(&json!("DET wins series 3-0"))
        );
        assert_eq!(
            value_of(&outcome, 0, "h2h_series_score_display"),
            Some(&json!("3-0"))
        );
        assert_eq!(
            value_of(&outcome, 0, "h2h_meetings_scheduled_total"),
            Some(&json!(3))
        );
        assert_eq!(
            value_of(&outcome, 0, "h2h_prior_meeting_count"),
            Some(&json!(1))
        );
        let meetings = value_of(&outcome, 0, "h2h_prior_meetings")
            .and_then(Value::as_array)
            .expect("prior meetings are an array");
        assert_eq!(meetings.len(), 1);
        assert_eq!(meetings[0]["event_id"], json!("401811000"));
        assert_eq!(meetings[0]["home_team_id"], json!("30"));
        assert_eq!(meetings[0]["away_score"], json!("113"));
        assert_eq!(meetings[0]["winner_team_id"], json!("8"));
        // Game context.
        assert_eq!(value_of(&outcome, 0, "game_venue_id"), Some(&json!("1893")));
        assert_eq!(
            value_of(&outcome, 0, "game_venue_city"),
            Some(&json!("Charlotte"))
        );
        assert_eq!(
            value_of(&outcome, 0, "game_venue_capacity"),
            Some(&json!(19077))
        );
        assert_eq!(
            value_of(&outcome, 0, "game_attendance"),
            Some(&json!(19623))
        );
        assert_eq!(
            value_of(&outcome, 0, "game_official_count"),
            Some(&json!(3))
        );
        assert_eq!(
            value_of(&outcome, 0, "game_boxscore_available_flag"),
            Some(&json!(true))
        );
        assert_eq!(
            value_of(&outcome, 0, "game_broadcast_national_list"),
            Some(&json!(["FanDuel SN SE", "ABC"]))
        );
        // The live injury report is carried as a COUNT: slice 1 owns per-athlete
        // injury emission and this stream never duplicates it.
        assert_eq!(
            value_of(&outcome, 0, "summary_injury_report_entry_count"),
            Some(&json!(3))
        );
        assert_eq!(
            value_of(&outcome, 0, "event_time_basis"),
            Some(&json!("receipt"))
        );
    }

    // --- Identity fallback and fail-closed paths.

    #[test]
    fn an_id_less_body_is_keyed_from_the_descriptor_and_labeled_as_such() {
        // An odds item with no core `$ref` anywhere: the paired descriptor is the
        // only carrier of the event id, exactly as in the slice-2 R1 precedent.
        const NO_REF: &str = r#"{"count":1,"pageIndex":1,"pageSize":100,"pageCount":1,"items":[
            {"provider":{"id":"100","name":"Draft Kings","priority":1},"spread":-6.5}]}"#;
        let mut adapter = adapter();
        let outcome = ingest(
            &mut adapter,
            &[connected(1), descriptor(2, "401811026"), inbound(3, NO_REF)],
        );
        assert_eq!(outcome.events.len(), 1);
        assert_eq!(
            value_of(&outcome, 0, "espn_event_id"),
            Some(&json!("401811026"))
        );
        assert_eq!(
            value_of(&outcome, 0, "event_id_basis"),
            Some(&json!("request_descriptor"))
        );
    }

    #[test]
    fn non_digit_event_ids_are_refused_from_both_the_ref_and_the_descriptor() {
        // A `$ref` whose events segment is not digits yields no id, and a
        // descriptor whose entity id is not digits cannot key the body either:
        // the body is skipped with a notice and nothing is emitted.
        const BAD_REF: &str = r#"{"count":1,"pageIndex":1,"pageSize":100,"pageCount":1,"items":[
            {"$ref":"http://sports.core.api.espn.com/v2/sports/basketball/leagues/nba/events/not-an-id/competitions/x/odds/100",
             "provider":{"id":"100","name":"Draft Kings","priority":1},"spread":-6.5}]}"#;
        for entity in ["", "not-an-id"] {
            let mut adapter = adapter();
            let outcome = ingest(
                &mut adapter,
                &[connected(1), descriptor(2, entity), inbound(3, BAD_REF)],
            );
            assert!(outcome.events.is_empty(), "entity `{entity}` must not emit");
            assert!(
                outcome
                    .notices
                    .iter()
                    .any(|notice| notice.code == FeedNoticeCode::StateInconsistent),
                "entity `{entity}` must be noticed"
            );
        }
        // With NO descriptor at all the disposition is the same.
        let mut adapter = adapter();
        let outcome = ingest(&mut adapter, &[connected(1), inbound(2, BAD_REF)]);
        assert!(outcome.events.is_empty());
        assert!(
            outcome
                .notices
                .iter()
                .any(|notice| notice.code == FeedNoticeCode::StateInconsistent)
        );
    }

    #[test]
    fn an_orphaned_descriptor_is_replaced_with_a_notice_and_never_keys_a_later_body() {
        // The capture loop writes the descriptor BEFORE the GET and fabricates no
        // inbound record on failure, so a failed request orphans exactly one
        // descriptor. The replacement is noticed, and the surviving descriptor is
        // the one that keys the next body.
        const NO_REF: &str = r#"{"count":1,"pageIndex":1,"pageSize":100,"pageCount":1,"items":[
            {"provider":{"id":"100","name":"Draft Kings","priority":1}}]}"#;
        let mut adapter = adapter();
        let outcome = ingest(
            &mut adapter,
            &[
                connected(1),
                descriptor(2, "401811026"),
                descriptor(3, "401811027"),
                inbound(4, NO_REF),
            ],
        );
        assert_eq!(outcome.events.len(), 1);
        assert_eq!(
            value_of(&outcome, 0, "espn_event_id"),
            Some(&json!("401811027"))
        );
        assert!(
            outcome
                .notices
                .iter()
                .any(|notice| notice.code == FeedNoticeCode::StateInconsistent)
        );
    }

    #[test]
    fn odd_bodies_are_noticed_and_skipped_without_poisoning_the_session() {
        let mut adapter = adapter();
        let garbage = ingest(
            &mut adapter,
            &[connected(1), inbound(2, "<html>502 Bad Gateway</html>")],
        );
        assert!(garbage.events.is_empty());
        assert!(
            garbage
                .notices
                .iter()
                .any(|notice| notice.code == FeedNoticeCode::UnsupportedMessage)
        );
        let wrong_shape = ingest(&mut adapter, &[inbound(3, r#"{"something":"else"}"#)]);
        assert!(wrong_shape.events.is_empty());
        assert_eq!(
            wrong_shape.notices[0].code,
            FeedNoticeCode::UnsupportedMessage
        );
        // The session still works.
        let good = ingest(&mut adapter, &[inbound(4, ODDS)]);
        assert_eq!(good.events.len(), 1);
    }

    #[test]
    fn a_pathological_body_fails_closed_on_the_item_bound() {
        let item = r#"{"provider":{"id":"100"},"spread":-6.5}"#;
        let items = std::iter::repeat_n(item, super::MAX_ODDS_ITEMS + 1)
            .collect::<Vec<_>>()
            .join(",");
        let payload = format!(r#"{{"pageCount":1,"items":[{items}]}}"#);
        let mut adapter = adapter();
        adapter.ingest(&connected(1)).expect("connects");
        adapter
            .ingest(&descriptor(2, "401811026"))
            .expect("descriptor");
        let error = adapter
            .ingest(&inbound(3, &payload))
            .expect_err("an implausible item count fails closed");
        assert!(
            error.to_string().contains("item count exceeded"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn configuration_records_are_refused() {
        let mut adapter = adapter();
        let configuration = record(1, 1, RecordKind::Configuration, Some("{}"));
        assert!(adapter.ingest(&configuration).is_err());
    }

    #[test]
    fn replay_is_deterministic_over_a_two_cycle_capture() {
        // Cycle 0 carries odds + predictor + summary (0 % 6 == 0); cycle 1 carries
        // odds + predictor only. Nothing is deduplicated.
        let fixture = vec![
            connected(1),
            descriptor(2, "401811026"),
            inbound(3, ODDS),
            descriptor(4, "401811026"),
            inbound(5, PREDICTOR),
            descriptor(6, "401811026"),
            inbound(7, SUMMARY),
            descriptor(8, "401811026"),
            inbound(9, ODDS),
            descriptor(10, "401811026"),
            inbound(11, PREDICTOR),
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
        let mut adapter = adapter();
        let total: usize = fixture
            .iter()
            .map(|record| adapter.ingest(record).expect("ingests").events.len())
            .sum();
        assert_eq!(total, 5);
    }
}
