//! Usage meter for the frontend language-model "writer".
//!
//! The Next.js server calls this before every model call so a stranger
//! cannot drain the owner's API credits. Three daily caps guard strategy
//! **builds** (per guest, per account address, site-wide) and two guard
//! **chat** turns (per guest, site-wide). One build — repairs included — is
//! one charge; one chat turn is one charge.
//!
//! * `POST /api/writer/charge` — check-and-increment of every applicable
//!   scope, atomic under one lock. Order is guest, address, site; a refused
//!   charge increments nothing and answers `429` with the scope that
//!   refused and the next UTC-midnight reset.
//! * `POST /api/writer/usage` — records the tokens a completed call spent
//!   and logs one line (no secrets, no prompt text). Answers `204`.
//! * `GET /api/writer/meter` — today's charges and refusals by kind and
//!   scope, tokens by model, and the caps in force.
//!
//! The meter is closed by default. With no `WRITER_METER_TOKEN` set, all
//! three routes answer `503 {"error":"writer_meter_unconfigured"}` and move
//! no counter; the exchange is public with permissive CORS, so an unguarded
//! meter would let a stranger drain the owner's credits. With a token set,
//! every route (charge, usage and meter) requires the `x-writer-token`
//! header to match it (constant-time) and answers `401` otherwise. State is
//! in memory, keyed by UTC day; the clock is injectable so tests control
//! which day it is.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};

/// A day is a whole number of UTC days since the Unix epoch, so a reset
/// lands exactly on UTC midnight.
const MS_PER_DAY: i64 = 86_400_000;

/// What a charge is for. `build` is one strategy compile (repairs
/// included); `chat` is one Teeny turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    #[default]
    Build,
    Chat,
}

/// Which cap refused a charge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    Guest,
    Address,
    Site,
}

/// The daily ceilings, from the environment at boot. Build has three
/// scopes; chat has two (no per-address cap).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct WriterCaps {
    pub build_guest: u32,
    pub build_address: u32,
    pub build_site: u32,
    pub chat_guest: u32,
    pub chat_site: u32,
}

impl Default for WriterCaps {
    fn default() -> Self {
        WriterCaps {
            build_guest: 8,
            build_address: 20,
            build_site: 300,
            chat_guest: 40,
            chat_site: 2000,
        }
    }
}

impl WriterCaps {
    /// Parse the caps from a key→value lookup, defaulting a missing key and
    /// rejecting a present-but-unparsable one (a boot error).
    pub fn from_env(get: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let d = WriterCaps::default();
        Ok(WriterCaps {
            build_guest: parse_cap(&get, "WRITER_CAP_PER_GUEST_PER_DAY", d.build_guest)?,
            build_address: parse_cap(&get, "WRITER_CAP_PER_ADDRESS_PER_DAY", d.build_address)?,
            build_site: parse_cap(&get, "WRITER_CAP_SITE_PER_DAY", d.build_site)?,
            chat_guest: parse_cap(&get, "WRITER_CAP_CHAT_PER_GUEST_PER_DAY", d.chat_guest)?,
            chat_site: parse_cap(&get, "WRITER_CAP_CHAT_SITE_PER_DAY", d.chat_site)?,
        })
    }

    /// Read the caps from the process environment.
    pub fn from_process_env() -> Result<Self, String> {
        WriterCaps::from_env(|key| std::env::var(key).ok())
    }

    fn guest(self, kind: Kind) -> u32 {
        match kind {
            Kind::Build => self.build_guest,
            Kind::Chat => self.chat_guest,
        }
    }

    fn site(self, kind: Kind) -> u32 {
        match kind {
            Kind::Build => self.build_site,
            Kind::Chat => self.chat_site,
        }
    }

    /// The per-address cap applies to builds only; chat has none.
    fn address(self, kind: Kind) -> Option<u32> {
        match kind {
            Kind::Build => Some(self.build_address),
            Kind::Chat => None,
        }
    }
}

fn parse_cap(
    get: &impl Fn(&str) -> Option<String>,
    key: &str,
    default: u32,
) -> Result<u32, String> {
    match get(key) {
        Some(raw) => raw
            .trim()
            .parse::<u32>()
            .map_err(|_| format!("{key}: expected a non-negative integer, got `{raw}`")),
        None => Ok(default),
    }
}

/// A clock returning Unix time in milliseconds. Injectable so tests can
/// move the day.
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

fn system_clock() -> Clock {
    Arc::new(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_millis() as i64
    })
}

/// Input + output tokens a model has spent today.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct TokenTotals {
    pub input: u64,
    pub output: u64,
}

/// The models the meter will account for. The usage map is keyed by a
/// caller-supplied id, so it accepts only these three and refuses anything
/// else; otherwise a hostile caller could grow the map without bound.
const METERED_MODELS: [&str; 3] = ["claude-opus-5-5", "claude-sonnet-5-5", "claude-haiku-5-5"];

/// The largest input or output token count a single report may carry. A
/// report above this is almost certainly hostile or a bug; it is refused.
const MAX_TOKENS_PER_REPORT: u64 = 10_000_000;

/// Why a usage report was not recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordUsageError {
    /// The model id is not one of `METERED_MODELS`.
    UnknownModel,
}

/// Whether a request may proceed. The meter is closed until a token is
/// configured, and then every route requires a matching header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// No token is configured: every route answers `503`.
    Unconfigured,
    /// A token is configured but the request's header is missing or wrong.
    Unauthorized,
    /// The request carried the configured token.
    Granted,
}

/// One UTC day's counters. Reset wholesale when the day rolls over.
#[derive(Default)]
struct DayCounts {
    guest: HashMap<(String, Kind), u32>,
    address: HashMap<(String, Kind), u32>,
    site: HashMap<Kind, u32>,
    charges: HashMap<Kind, u64>,
    refusals: HashMap<(Kind, Scope), u64>,
    tokens: HashMap<String, TokenTotals>,
}

struct MeterInner {
    day: i64,
    counts: DayCounts,
}

struct Shared {
    inner: Mutex<MeterInner>,
    caps: WriterCaps,
    /// `None` (or empty) means the endpoints are closed: every request is
    /// answered 503 until a token is configured.
    token: Option<String>,
    clock: Clock,
}

/// The shared meter handed to the HTTP handlers. Cheap to clone.
#[derive(Clone)]
pub struct WriterMeter {
    shared: Arc<Shared>,
}

/// A granted charge: what the guest and the site have left today.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ChargeOk {
    pub remaining_guest: u32,
    pub remaining_site: u32,
}

/// A refused charge: which cap refused, and when it next resets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ChargeDenied {
    pub scope: Scope,
    pub resets_at_ms: i64,
}

impl WriterMeter {
    pub fn new(caps: WriterCaps, token: Option<String>, clock: Clock) -> Self {
        let day = (clock)() / MS_PER_DAY;
        WriterMeter {
            shared: Arc::new(Shared {
                inner: Mutex::new(MeterInner {
                    day,
                    counts: DayCounts::default(),
                }),
                caps,
                token,
                clock,
            }),
        }
    }

    /// Production constructor: the real wall clock.
    pub fn with_system_clock(caps: WriterCaps, token: Option<String>) -> Self {
        WriterMeter::new(caps, token, system_clock())
    }

    pub fn caps(&self) -> WriterCaps {
        self.shared.caps
    }

    fn today(&self) -> i64 {
        (self.shared.clock)() / MS_PER_DAY
    }

    /// Check every applicable cap and, only if all pass, increment every
    /// one. Atomic under one lock; a refusal increments no counter.
    pub fn charge(
        &self,
        guest: &str,
        address: Option<&str>,
        kind: Kind,
    ) -> Result<ChargeOk, ChargeDenied> {
        let caps = self.shared.caps;
        let today = self.today();
        let mut inner = self.shared.inner.lock().expect("writer meter poisoned");
        roll_day(&mut inner, today);
        let counts = &mut inner.counts;

        let guest_key = (guest.to_string(), kind);
        if *counts.guest.get(&guest_key).unwrap_or(&0) >= caps.guest(kind) {
            return Err(deny(counts, kind, Scope::Guest, today));
        }
        // The address cap guards builds only, and only when the caller is a
        // known account; a guest with no address is held back by the guest
        // and site caps alone.
        let address_key = match (caps.address(kind), address) {
            (Some(cap), Some(addr)) => {
                let key = (addr.to_string(), kind);
                if *counts.address.get(&key).unwrap_or(&0) >= cap {
                    return Err(deny(counts, kind, Scope::Address, today));
                }
                Some(key)
            }
            _ => None,
        };
        if *counts.site.get(&kind).unwrap_or(&0) >= caps.site(kind) {
            return Err(deny(counts, kind, Scope::Site, today));
        }

        let guest_now = bump(counts.guest.entry(guest_key).or_insert(0));
        if let Some(key) = address_key {
            bump(counts.address.entry(key).or_insert(0));
        }
        let site_now = bump(counts.site.entry(kind).or_insert(0));
        *counts.charges.entry(kind).or_insert(0) += 1;

        Ok(ChargeOk {
            remaining_guest: caps.guest(kind).saturating_sub(guest_now),
            remaining_site: caps.site(kind).saturating_sub(site_now),
        })
    }

    /// Record the tokens a completed call spent. Refuses an unmetered model
    /// id so the per-model map cannot grow without bound; accumulates with
    /// `saturating_add` so a hostile `u64::MAX` cannot overflow and poison
    /// the mutex.
    pub fn record_usage(
        &self,
        model: &str,
        input: u64,
        output: u64,
    ) -> Result<(), RecordUsageError> {
        if !METERED_MODELS.contains(&model) {
            return Err(RecordUsageError::UnknownModel);
        }
        let today = self.today();
        let mut inner = self.shared.inner.lock().expect("writer meter poisoned");
        roll_day(&mut inner, today);
        let totals = inner.counts.tokens.entry(model.to_string()).or_default();
        totals.input = totals.input.saturating_add(input);
        totals.output = totals.output.saturating_add(output);
        Ok(())
    }

    /// Whether a request may proceed. Closed (`Unconfigured`) until a token
    /// is set; then the request's `x-writer-token` must match (constant-time).
    pub fn access(&self, provided: Option<&str>) -> Access {
        match &self.shared.token {
            // An empty token is no token: it must never match an empty header.
            Some(expected) if !expected.is_empty() => {
                if provided.is_some_and(|p| constant_time_eq(p.as_bytes(), expected.as_bytes())) {
                    Access::Granted
                } else {
                    Access::Unauthorized
                }
            }
            _ => Access::Unconfigured,
        }
    }

    /// Today's counters, for `GET /api/writer/meter`.
    pub fn snapshot(&self) -> MeterSnapshot {
        let today = self.today();
        let mut inner = self.shared.inner.lock().expect("writer meter poisoned");
        roll_day(&mut inner, today);
        let counts = &inner.counts;
        let charges = KindCounts {
            build: *counts.charges.get(&Kind::Build).unwrap_or(&0),
            chat: *counts.charges.get(&Kind::Chat).unwrap_or(&0),
        };
        let refusal = |kind: Kind, scope: Scope| *counts.refusals.get(&(kind, scope)).unwrap_or(&0);
        let refusals = KindScopeCounts {
            build: ScopeCounts {
                guest: refusal(Kind::Build, Scope::Guest),
                address: refusal(Kind::Build, Scope::Address),
                site: refusal(Kind::Build, Scope::Site),
            },
            chat: ScopeCounts {
                guest: refusal(Kind::Chat, Scope::Guest),
                address: refusal(Kind::Chat, Scope::Address),
                site: refusal(Kind::Chat, Scope::Site),
            },
        };
        MeterSnapshot {
            caps: self.shared.caps,
            charges,
            refusals,
            tokens: counts.tokens.clone(),
            resets_at_ms: resets_at_ms(today),
        }
    }
}

/// Drop the day's counters when the UTC day advances.
fn roll_day(inner: &mut MeterInner, today: i64) {
    if inner.day != today {
        inner.day = today;
        inner.counts = DayCounts::default();
    }
}

fn resets_at_ms(today: i64) -> i64 {
    (today + 1) * MS_PER_DAY
}

fn deny(counts: &mut DayCounts, kind: Kind, scope: Scope, today: i64) -> ChargeDenied {
    *counts.refusals.entry((kind, scope)).or_insert(0) += 1;
    ChargeDenied {
        scope,
        resets_at_ms: resets_at_ms(today),
    }
}

fn bump(slot: &mut u32) -> u32 {
    *slot += 1;
    *slot
}

/// Length-checked constant-time byte comparison. The length is allowed to
/// leak; the token's contents are not.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

// ──────────────────────────── Wire shapes ──────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ChargeRequest {
    pub guest: String,
    #[serde(default)]
    pub address: Option<String>,
    #[serde(default)]
    pub kind: Kind,
}

#[derive(Debug, Deserialize)]
pub struct UsageRequest {
    pub guest: String,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct KindCounts {
    pub build: u64,
    pub chat: u64,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct ScopeCounts {
    pub guest: u64,
    pub address: u64,
    pub site: u64,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct KindScopeCounts {
    pub build: ScopeCounts,
    pub chat: ScopeCounts,
}

#[derive(Debug, Clone, Serialize)]
pub struct MeterSnapshot {
    pub caps: WriterCaps,
    pub charges: KindCounts,
    pub refusals: KindScopeCounts,
    pub tokens: HashMap<String, TokenTotals>,
    pub resets_at_ms: i64,
}

// ───────────────────────────── Handlers ────────────────────────────────

fn header_token(headers: &HeaderMap) -> Option<&str> {
    headers.get("x-writer-token").and_then(|v| v.to_str().ok())
}

fn unauthorized() -> Response {
    (StatusCode::UNAUTHORIZED, Json(serde_json::json!({ "error": "unauthorized" }))).into_response()
}

fn unconfigured() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({ "error": "writer_meter_unconfigured" })),
    )
        .into_response()
}

/// Returns a refusal response when the request may not proceed, or `None`
/// when access is granted.
fn gate(meter: &WriterMeter, headers: &HeaderMap) -> Option<Response> {
    match meter.access(header_token(headers)) {
        Access::Unconfigured => Some(unconfigured()),
        Access::Unauthorized => Some(unauthorized()),
        Access::Granted => None,
    }
}

async fn charge_handler(
    State(meter): State<WriterMeter>,
    headers: HeaderMap,
    Json(req): Json<ChargeRequest>,
) -> Response {
    if let Some(refused) = gate(&meter, &headers) {
        return refused;
    }
    let address = req.address.as_deref().filter(|a| !a.is_empty());
    match meter.charge(&req.guest, address, req.kind) {
        Ok(ok) => (StatusCode::OK, Json(ok)).into_response(),
        Err(denied) => (StatusCode::TOO_MANY_REQUESTS, Json(denied)).into_response(),
    }
}

async fn usage_handler(
    State(meter): State<WriterMeter>,
    headers: HeaderMap,
    Json(req): Json<UsageRequest>,
) -> Response {
    if let Some(refused) = gate(&meter, &headers) {
        return refused;
    }
    if req.input_tokens > MAX_TOKENS_PER_REPORT || req.output_tokens > MAX_TOKENS_PER_REPORT {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "usage_out_of_range" })),
        )
            .into_response();
    }
    match meter.record_usage(&req.model, req.input_tokens, req.output_tokens) {
        Ok(()) => {
            // One line, no secrets and no prompt text: the guest is a hashed id.
            tracing::info!(
                guest = %req.guest,
                model = %req.model,
                input_tokens = req.input_tokens,
                output_tokens = req.output_tokens,
                "writer usage",
            );
            StatusCode::NO_CONTENT.into_response()
        }
        Err(RecordUsageError::UnknownModel) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "unknown_model" })),
        )
            .into_response(),
    }
}

async fn meter_handler(State(meter): State<WriterMeter>, headers: HeaderMap) -> Response {
    if let Some(refused) = gate(&meter, &headers) {
        return refused;
    }
    (StatusCode::OK, Json(meter.snapshot())).into_response()
}

/// The writer-meter routes, with their own in-memory state. Reads the caps
/// and the optional token from the environment; an unparsable cap is a boot
/// failure (exit 78, like the registry and the templates).
pub fn routes() -> Router<super::AppState> {
    let caps = WriterCaps::from_process_env().unwrap_or_else(|e| {
        tracing::error!(error = %e, "writer meter: bad cap environment");
        std::process::exit(78);
    });
    let token = std::env::var("WRITER_METER_TOKEN").ok().filter(|t| !t.is_empty());
    let meter = WriterMeter::with_system_clock(caps, token);
    Router::new()
        .route("/api/writer/charge", post(charge_handler))
        .route("/api/writer/usage", post(usage_handler))
        .route("/api/writer/meter", get(meter_handler))
        .with_state(meter)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::thread;

    /// A clock parked at a fixed millisecond, movable between calls.
    #[derive(Clone)]
    struct FakeClock(Arc<Mutex<i64>>);
    impl FakeClock {
        fn at(ms: i64) -> Self {
            FakeClock(Arc::new(Mutex::new(ms)))
        }
        fn clock(&self) -> Clock {
            let slot = self.0.clone();
            Arc::new(move || *slot.lock().unwrap())
        }
        fn set(&self, ms: i64) {
            *self.0.lock().unwrap() = ms;
        }
    }

    fn meter(caps: WriterCaps, clock: Clock) -> WriterMeter {
        WriterMeter::new(caps, None, clock)
    }

    const MIDDAY: i64 = 100 * MS_PER_DAY + 12 * 3_600_000;

    #[test]
    fn fifty_concurrent_build_charges_admit_exactly_the_guest_cap() {
        let caps = WriterCaps::default(); // build_guest = 8
        let m = meter(caps, FakeClock::at(MIDDAY).clock());
        let granted = Arc::new(AtomicU32::new(0));
        let mut handles = Vec::new();
        for _ in 0..50 {
            let m = m.clone();
            let granted = granted.clone();
            handles.push(thread::spawn(move || {
                if m.charge("guest-1", None, Kind::Build).is_ok() {
                    granted.fetch_add(1, Ordering::SeqCst);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(granted.load(Ordering::SeqCst), caps.build_guest);
    }

    #[test]
    fn a_refused_charge_changes_no_counter() {
        let caps = WriterCaps {
            build_guest: 1,
            ..WriterCaps::default()
        };
        let m = meter(caps, FakeClock::at(MIDDAY).clock());
        assert!(m.charge("g", Some("addr"), Kind::Build).is_ok());
        let before = m.snapshot();
        // The guest cap is spent; this one is refused.
        assert!(m.charge("g", Some("addr"), Kind::Build).is_err());
        let after = m.snapshot();
        // Charges, token counts and the address counter are untouched; only
        // the refusal tally moved.
        assert_eq!(after.charges.build, before.charges.build);
        assert_eq!(after.charges.build, 1);
        assert_eq!(after.refusals.build.guest, 1);
        // The address scope never refused and its one grant still stands:
        // a second guest hitting the same address still charges.
        assert!(m.charge("g2", Some("addr"), Kind::Build).is_ok());
    }

    #[test]
    fn each_build_cap_refuses_with_its_own_scope() {
        // Guest scope.
        let m = meter(
            WriterCaps { build_guest: 1, ..WriterCaps::default() },
            FakeClock::at(MIDDAY).clock(),
        );
        assert!(m.charge("g", Some("a"), Kind::Build).is_ok());
        let denied = m.charge("g", Some("a"), Kind::Build).unwrap_err();
        assert_eq!(denied.scope, Scope::Guest);
        assert_eq!(denied.resets_at_ms, 101 * MS_PER_DAY);

        // Address scope: different guests, same address, address cap first.
        let m = meter(
            WriterCaps { build_guest: 9, build_address: 2, ..WriterCaps::default() },
            FakeClock::at(MIDDAY).clock(),
        );
        assert!(m.charge("g1", Some("shared"), Kind::Build).is_ok());
        assert!(m.charge("g2", Some("shared"), Kind::Build).is_ok());
        let denied = m.charge("g3", Some("shared"), Kind::Build).unwrap_err();
        assert_eq!(denied.scope, Scope::Address);

        // Site scope: distinct guests and addresses, small site cap.
        let m = meter(
            WriterCaps { build_guest: 9, build_address: 9, build_site: 2, ..WriterCaps::default() },
            FakeClock::at(MIDDAY).clock(),
        );
        assert!(m.charge("g1", Some("a1"), Kind::Build).is_ok());
        assert!(m.charge("g2", Some("a2"), Kind::Build).is_ok());
        let denied = m.charge("g3", Some("a3"), Kind::Build).unwrap_err();
        assert_eq!(denied.scope, Scope::Site);
    }

    #[test]
    fn each_chat_cap_refuses_with_its_own_scope() {
        // Chat has no address cap, so address is never the refusing scope.
        let m = meter(
            WriterCaps { chat_guest: 1, ..WriterCaps::default() },
            FakeClock::at(MIDDAY).clock(),
        );
        assert!(m.charge("g", Some("a"), Kind::Chat).is_ok());
        let denied = m.charge("g", Some("a"), Kind::Chat).unwrap_err();
        assert_eq!(denied.scope, Scope::Guest);

        let m = meter(
            WriterCaps { chat_guest: 9, chat_site: 2, ..WriterCaps::default() },
            FakeClock::at(MIDDAY).clock(),
        );
        assert!(m.charge("g1", None, Kind::Chat).is_ok());
        assert!(m.charge("g2", None, Kind::Chat).is_ok());
        let denied = m.charge("g3", None, Kind::Chat).unwrap_err();
        assert_eq!(denied.scope, Scope::Site);
    }

    #[test]
    fn chat_never_refuses_on_the_address_scope() {
        // Even a flood from one address on chat refuses on guest/site only.
        let m = meter(
            WriterCaps { chat_guest: 100, chat_site: 100, ..WriterCaps::default() },
            FakeClock::at(MIDDAY).clock(),
        );
        for _ in 0..50 {
            m.charge("g", Some("addr"), Kind::Chat).unwrap();
        }
        assert_eq!(m.snapshot().refusals.chat.address, 0);
    }

    #[test]
    fn counters_reset_at_utc_midnight() {
        let clock = FakeClock::at(MIDDAY);
        let caps = WriterCaps { build_guest: 1, ..WriterCaps::default() };
        let m = meter(caps, clock.clock());
        assert!(m.charge("g", None, Kind::Build).is_ok());
        assert!(m.charge("g", None, Kind::Build).is_err());
        // One millisecond before the next UTC midnight: still the same day.
        clock.set(101 * MS_PER_DAY - 1);
        assert!(m.charge("g", None, Kind::Build).is_err());
        // Cross UTC midnight: the day's counters are gone.
        clock.set(101 * MS_PER_DAY);
        assert!(m.charge("g", None, Kind::Build).is_ok());
        assert_eq!(m.snapshot().charges.build, 1);
    }

    #[test]
    fn a_bad_cap_env_value_is_an_error() {
        let bad = |key: &str| {
            if key == "WRITER_CAP_SITE_PER_DAY" {
                Some("three-hundred".to_string())
            } else {
                None
            }
        };
        assert!(WriterCaps::from_env(bad).is_err());
        // A clean environment (all unset) yields the defaults.
        let defaults = WriterCaps::from_env(|_| None).unwrap();
        assert_eq!(defaults, WriterCaps::default());
        // A present, parsable value overrides.
        let custom = WriterCaps::from_env(|key| {
            (key == "WRITER_CAP_PER_GUEST_PER_DAY").then(|| "3".to_string())
        })
        .unwrap();
        assert_eq!(custom.build_guest, 3);
    }

    #[test]
    fn access_is_closed_until_a_token_is_configured() {
        // Unset: closed. Every request is refused as unconfigured.
        let open = WriterMeter::new(WriterCaps::default(), None, FakeClock::at(MIDDAY).clock());
        assert_eq!(open.access(None), Access::Unconfigured);
        assert_eq!(open.access(Some("anything")), Access::Unconfigured);
        // Set: the header must match exactly.
        let guarded = WriterMeter::new(
            WriterCaps::default(),
            Some("s3cret".to_string()),
            FakeClock::at(MIDDAY).clock(),
        );
        assert_eq!(guarded.access(None), Access::Unauthorized);
        assert_eq!(guarded.access(Some("")), Access::Unauthorized);
        assert_eq!(guarded.access(Some("s3cre")), Access::Unauthorized);
        assert_eq!(guarded.access(Some("s3cret-extra")), Access::Unauthorized);
        assert_eq!(guarded.access(Some("wrong!")), Access::Unauthorized);
        assert_eq!(guarded.access(Some("s3cret")), Access::Granted);
    }

    #[test]
    fn an_empty_token_is_no_token() {
        // A token set to "" must not let an empty header through.
        let empty = WriterMeter::new(WriterCaps::default(), Some(String::new()), FakeClock::at(MIDDAY).clock());
        assert_eq!(empty.access(Some("")), Access::Unconfigured);
        assert_eq!(empty.access(None), Access::Unconfigured);
    }

    #[test]
    fn records_usage_tokens_by_model() {
        let m = meter(WriterCaps::default(), FakeClock::at(MIDDAY).clock());
        m.record_usage("claude-opus-5-5", 100, 20).unwrap();
        m.record_usage("claude-opus-5-5", 5, 3).unwrap();
        let snap = m.snapshot();
        let totals = snap.tokens.get("claude-opus-5-5").copied().unwrap_or_default();
        assert_eq!(totals, TokenTotals { input: 105, output: 23 });
    }

    // ── HTTP boundary: status codes and the 429 body ──

    async fn body_json(resp: Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        if bytes.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        }
    }

    #[tokio::test]
    async fn charge_handler_answers_200_then_429_with_scope() {
        // The meter is closed by default, so this boundary test configures a
        // token and sends it on each call.
        let m = WriterMeter::new(
            WriterCaps { build_guest: 1, ..WriterCaps::default() },
            Some("tok".to_string()),
            FakeClock::at(MIDDAY).clock(),
        );
        let ok = charge_handler(
            State(m.clone()),
            with_token("tok"),
            Json(ChargeRequest { guest: "g".into(), address: None, kind: Kind::Build }),
        )
        .await;
        assert_eq!(ok.status(), StatusCode::OK);
        assert_eq!(body_json(ok).await, serde_json::json!({ "remaining_guest": 0, "remaining_site": 299 }));

        let denied = charge_handler(
            State(m),
            with_token("tok"),
            Json(ChargeRequest { guest: "g".into(), address: None, kind: Kind::Build }),
        )
        .await;
        assert_eq!(denied.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            body_json(denied).await,
            serde_json::json!({ "scope": "guest", "resets_at_ms": 101 * MS_PER_DAY }),
        );
    }

    #[tokio::test]
    async fn charge_defaults_kind_to_build_when_absent() {
        let m = meter(WriterCaps::default(), FakeClock::at(MIDDAY).clock());
        let req: ChargeRequest = serde_json::from_value(serde_json::json!({ "guest": "g" })).unwrap();
        assert_eq!(req.kind, Kind::Build);
        assert!(m.charge(&req.guest, req.address.as_deref(), req.kind).is_ok());
        assert_eq!(m.snapshot().charges.build, 1);
    }

    #[tokio::test]
    async fn protected_endpoints_require_the_token() {
        let m = WriterMeter::new(
            WriterCaps::default(),
            Some("tok".to_string()),
            FakeClock::at(MIDDAY).clock(),
        );
        let missing = charge_handler(
            State(m.clone()),
            HeaderMap::new(),
            Json(ChargeRequest { guest: "g".into(), address: None, kind: Kind::Build }),
        )
        .await;
        assert_eq!(missing.status(), StatusCode::UNAUTHORIZED);
        // A refused (unauthorized) request charges nothing.
        assert_eq!(m.snapshot().charges.build, 0);

        let mut headers = HeaderMap::new();
        headers.insert("x-writer-token", "tok".parse().unwrap());
        let ok = charge_handler(
            State(m),
            headers,
            Json(ChargeRequest { guest: "g".into(), address: None, kind: Kind::Build }),
        )
        .await;
        assert_eq!(ok.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn usage_handler_answers_204_and_requires_token() {
        let m = WriterMeter::new(
            WriterCaps::default(),
            Some("tok".to_string()),
            FakeClock::at(MIDDAY).clock(),
        );
        let unauth = usage_handler(
            State(m.clone()),
            HeaderMap::new(),
            Json(UsageRequest {
                guest: "g".into(),
                model: "claude-opus-5-5".into(),
                input_tokens: 10,
                output_tokens: 2,
            }),
        )
        .await;
        assert_eq!(unauth.status(), StatusCode::UNAUTHORIZED);
        assert!(m.snapshot().tokens.is_empty());

        let mut headers = HeaderMap::new();
        headers.insert("x-writer-token", "tok".parse().unwrap());
        let ok = usage_handler(
            State(m.clone()),
            headers,
            Json(UsageRequest {
                guest: "g".into(),
                model: "claude-opus-5-5".into(),
                input_tokens: 10,
                output_tokens: 2,
            }),
        )
        .await;
        assert_eq!(ok.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            m.snapshot().tokens.get("claude-opus-5-5").copied().unwrap_or_default(),
            TokenTotals { input: 10, output: 2 },
        );
    }

    // ── W2: closed by default ──

    fn configured() -> WriterMeter {
        WriterMeter::new(
            WriterCaps::default(),
            Some("tok".to_string()),
            FakeClock::at(MIDDAY).clock(),
        )
    }

    fn with_token(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("x-writer-token", value.parse().unwrap());
        headers
    }

    fn a_charge() -> ChargeRequest {
        ChargeRequest { guest: "g".into(), address: None, kind: Kind::Build }
    }

    fn a_usage() -> UsageRequest {
        UsageRequest {
            guest: "g".into(),
            model: "claude-opus-5-5".into(),
            input_tokens: 10,
            output_tokens: 2,
        }
    }

    #[tokio::test]
    async fn an_unconfigured_meter_answers_503_on_every_route_and_moves_no_counter() {
        // No token configured: the meter is closed, not open.
        let m = meter(WriterCaps::default(), FakeClock::at(MIDDAY).clock());
        let unconfigured = serde_json::json!({ "error": "writer_meter_unconfigured" });

        let charge = charge_handler(State(m.clone()), HeaderMap::new(), Json(a_charge())).await;
        assert_eq!(charge.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body_json(charge).await, unconfigured);

        let usage = usage_handler(State(m.clone()), HeaderMap::new(), Json(a_usage())).await;
        assert_eq!(usage.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body_json(usage).await, unconfigured);

        let snapshot = meter_handler(State(m.clone()), HeaderMap::new()).await;
        assert_eq!(snapshot.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body_json(snapshot).await, unconfigured);

        // Nothing was charged or recorded.
        let snap = m.snapshot();
        assert_eq!(snap.charges.build, 0);
        assert!(snap.tokens.is_empty());
    }

    #[tokio::test]
    async fn a_configured_meter_rejects_a_missing_or_wrong_token_on_every_route_with_401() {
        for headers in [HeaderMap::new(), with_token(""), with_token("nope")] {
            let m = configured();
            let charge =
                charge_handler(State(m.clone()), headers.clone(), Json(a_charge())).await;
            assert_eq!(charge.status(), StatusCode::UNAUTHORIZED);
            let usage = usage_handler(State(m.clone()), headers.clone(), Json(a_usage())).await;
            assert_eq!(usage.status(), StatusCode::UNAUTHORIZED);
            let snapshot = meter_handler(State(m.clone()), headers.clone()).await;
            assert_eq!(snapshot.status(), StatusCode::UNAUTHORIZED);
            // A rejected request charges and records nothing.
            let snap = m.snapshot();
            assert_eq!(snap.charges.build, 0);
            assert!(snap.tokens.is_empty());
        }
    }

    #[tokio::test]
    async fn a_configured_meter_serves_every_route_with_the_right_token() {
        let m = configured();
        let charge =
            charge_handler(State(m.clone()), with_token("tok"), Json(a_charge())).await;
        assert_eq!(charge.status(), StatusCode::OK);
        let usage = usage_handler(State(m.clone()), with_token("tok"), Json(a_usage())).await;
        assert_eq!(usage.status(), StatusCode::NO_CONTENT);
        let snapshot = meter_handler(State(m.clone()), with_token("tok")).await;
        assert_eq!(snapshot.status(), StatusCode::OK);
    }

    // ── W3: record_usage guards ──

    #[test]
    fn two_u64_max_reports_saturate_without_panicking() {
        let m = meter(WriterCaps::default(), FakeClock::at(MIDDAY).clock());
        m.record_usage("claude-opus-5-5", u64::MAX, u64::MAX).unwrap();
        m.record_usage("claude-opus-5-5", u64::MAX, u64::MAX).unwrap();
        let totals = m.snapshot().tokens.get("claude-opus-5-5").copied().unwrap_or_default();
        assert_eq!(totals, TokenTotals { input: u64::MAX, output: u64::MAX });
    }

    #[test]
    fn record_usage_rejects_an_unknown_model_and_records_nothing() {
        let m = meter(WriterCaps::default(), FakeClock::at(MIDDAY).clock());
        assert_eq!(m.record_usage("gpt-5", 10, 2), Err(RecordUsageError::UnknownModel));
        assert!(m.snapshot().tokens.is_empty());
        // Each of the three known ids is accepted.
        for id in ["claude-opus-5-5", "claude-sonnet-5-5", "claude-haiku-5-5"] {
            assert!(m.record_usage(id, 1, 1).is_ok());
        }
    }

    #[tokio::test]
    async fn usage_handler_rejects_an_unknown_model_with_400_and_records_nothing() {
        let m = configured();
        let resp = usage_handler(
            State(m.clone()),
            with_token("tok"),
            Json(UsageRequest {
                guest: "g".into(),
                model: "gpt-5".into(),
                input_tokens: 1,
                output_tokens: 1,
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(body_json(resp).await, serde_json::json!({ "error": "unknown_model" }));
        assert!(m.snapshot().tokens.is_empty());
    }

    #[tokio::test]
    async fn usage_handler_rejects_out_of_range_tokens_with_400() {
        let m = configured();
        let over = usage_handler(
            State(m.clone()),
            with_token("tok"),
            Json(UsageRequest {
                guest: "g".into(),
                model: "claude-opus-5-5".into(),
                input_tokens: 10_000_001,
                output_tokens: 1,
            }),
        )
        .await;
        assert_eq!(over.status(), StatusCode::BAD_REQUEST);
        assert_eq!(body_json(over).await, serde_json::json!({ "error": "usage_out_of_range" }));
        assert!(m.snapshot().tokens.is_empty());

        // The ceiling itself is accepted.
        let edge = usage_handler(
            State(m.clone()),
            with_token("tok"),
            Json(UsageRequest {
                guest: "g".into(),
                model: "claude-opus-5-5".into(),
                input_tokens: 10_000_000,
                output_tokens: 10_000_000,
            }),
        )
        .await;
        assert_eq!(edge.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            m.snapshot().tokens.get("claude-opus-5-5").copied().unwrap_or_default(),
            TokenTotals { input: 10_000_000, output: 10_000_000 },
        );
    }
}
