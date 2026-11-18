//! Pyth Hermes BTC/USD aggregate-price adapter: bounded SSE and REST responses
//! normalized into single-value price updates. Adopted as the DRAFT source
//! `pyth.hermes.btc-usd.price@1.0.0`, the second, independent source for the
//! `btc.pyth.oracle_reference` identity (the Kalshi `pyth_value` relay is the
//! first). This source is built to the full standard and **refused at runtime**
//! until a paid `PYTH_API_KEY` exists (ruling R1/R7): the capture path exits 78
//! without the key. An authenticated hour was captured on the Tokyo box on
//! 2026-09-04 (`pyth-hermes-stream-r1-20260904T012850Z.jsonl`, 3660 stream
//! frames; `pyth-hermes-rest-r1-20260904T012850Z.jsonl`, 390 REST anchors) and
//! replays cleanly through this adapter with no unknown fields and no shape or
//! value rejection, so the field set (§3.2 of `build/A2-pyth-hermes-feed.md`)
//! and gap detection via `prev_publish_time` (§3.4) are confirmed against the
//! capture; `deny_unknown_fields` at every envelope level would have failed a
//! wrong shape loudly. The synthetic fixtures below are joined by the
//! `observed_capture_frames_*` tests, which drive verbatim captured frames.
//!
//! The adapter mirrors [`crate::chainlink`] (single-value oracle, compiled
//! constants, transient-vs-fatal classification) and
//! [`crate::polymarket_twap`] (subscription pinning, [`finish`] completeness
//! assertion). It never reads a clock, never allocates beyond the compiled
//! bounds, and never touches the credential: the key is a request header the
//! capture layer attaches, never a value this module sees.
//!
//! [`finish`]: PythHermesAdapter::finish

use std::collections::BTreeMap;
use std::time::Duration;

use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::json;
use signals_core::SourceEvent;

use crate::adapter::{GuardAction, RecordGuard, validate_publisher_epoch};
use crate::event_window::millis_to_rfc3339;
use crate::{AdapterError, FeedAdapter, FeedNotice, FeedNoticeCode, IngestOutcome, RawRecord};

// --- Compiled constants (§3.1). The feed id is a compiled constant, never read
// from configuration, argv or environment, and every emitted event re-checks
// `parsed[].id` against it (V2). Verified against the keyless `/v2/price_feeds`
// catalogue on 2026-09-03: `Crypto.BTC/USD`, `min_channel real_time`, 24/7.

/// Adopted source identity.
pub const SOURCE_ID: &str = "pyth.hermes.btc-usd.price";
/// Adopted source version.
pub const SOURCE_VERSION: &str = "1.0.0";
/// The one BTC/USD feed id this adapter admits. A row with any other id is
/// refused, not ignored (V2, test T-N14).
pub const FEED_ID: &str = "e62df6c8b4a85fe1a67db44dc12de5db330f7ac66b72dc658afedf0f4a415b43";
const INSTRUMENT: &str = "BTC-USD";
const SYMBOL: &str = "Crypto.BTC/USD";

/// Hermes host (TLS 1.3, shared `secure_http::client()`).
pub const HERMES_HOST: &str = "https://hermes.pyth.network";
/// SSE stream path.
pub const STREAM_PATH: &str = "/v2/updates/price/stream";
/// REST latest-anchor path.
pub const LATEST_PATH: &str = "/v2/updates/price/latest";

/// Timing/grouping series name for this feed (cf. `chainlink::ROUNDS_SERIES`);
/// there is no per-message topic, so the feed has a single logical channel.
pub const STREAM_SERIES: &str = "pyth_hermes:price-stream";

/// Per-request HTTP timeout for the SSE connect and each REST anchor probe.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Compiled minimum interval between REST `latest` probes (documented budget:
/// REST ≤ 1 req / 10 s).
pub const MIN_REST_INTERVAL: Duration = Duration::from_secs(10);

/// Upper bound on one buffered SSE event; overflow ends the connection and is
/// refused without growing a buffer (test T-N15).
pub const MAX_SSE_EVENT_BYTES: usize = 65_536;
/// Upper bound on one SSE line; overflow ends the connection (test T-N15).
pub const MAX_SSE_LINE_BYTES: usize = 65_536;

/// At most four `parsed` rows, and at most four `binary.data` elements, each at
/// most [`MAX_BINARY_CHARS`] hex characters.
const MAX_PARSED_ROWS: usize = 4;
/// Upper bound on one `binary.data` element, in hex characters.
const MAX_BINARY_CHARS: usize = 65_536;

/// Admissible exponent band. Pyth publishes a negative exponent for a crypto
/// price (`-8` for BTC/USD); the band admits `[-18, 0]` and refuses anything
/// else (V5). An exponent outside the band is a value-class refusal.
const EXPO_MIN: i64 = -18;
const EXPO_MAX: i64 = 0;

/// A publish time this far ahead of local receipt is refused as a clock fault
/// (V9).
const MAX_PUBLISH_SKEW_MS: i64 = 60_000;

/// Published source contract, bound in-crate so the emitted `values` map and the
/// definition `fields` array have one authority (the field-set equality test
/// asserts they agree, both directions).
pub const SOURCE_DEFINITION: &str =
    include_str!("../../../definitions/btc/sources/pyth.hermes.btc-usd.price@1.0.0.json");

/// Which Hermes surface a record came from, fixing the `update_kind` the adapter
/// stamps so a rolling window can exclude the REST anchor rows (§3.6). The two
/// surfaces are separate capture streams with separate provenance, exactly as
/// the Binance USD-M REST poll is separate from its WebSocket streams.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PythHermesConnection {
    /// The SSE `/v2/updates/price/stream` connection. Rows carry
    /// `update_kind: "stream"` and this connection asserts its subscription pin
    /// in [`PythHermesAdapter::finish`].
    Stream,
    /// The REST `/v2/updates/price/latest` anchor probe. Rows carry
    /// `update_kind: "latest"` so rolling windows exclude them; it never
    /// substitutes for the stream.
    Latest,
}

impl PythHermesConnection {
    const fn update_kind(self) -> &'static str {
        match self {
            Self::Stream => "stream",
            Self::Latest => "latest",
        }
    }
}

/// Build the exact SSE subscription URL, pinning `ids[]=FEED_ID`, `parsed=true`
/// and `encoding=hex`. Pure so the request shape is unit-tested without a socket
/// (test T-U1, `chainlink::request_body` pattern). The query string carries no
/// secret; the credential is a request header the capture layer attaches.
#[must_use]
pub fn stream_url() -> String {
    price_update_url(STREAM_PATH)
}

/// Build the exact REST latest-anchor URL, with the same pinned query.
#[must_use]
pub fn latest_url() -> String {
    price_update_url(LATEST_PATH)
}

fn price_update_url(path: &str) -> String {
    format!("{HERMES_HOST}{path}?ids[]={FEED_ID}&parsed=true&encoding=hex")
}

/// The recorded outbound request descriptor for one Hermes GET, authored by the
/// capture layer and read back on replay. The credential never appears here: it
/// is a header, and this descriptor is the URL and method only.
#[must_use]
pub fn request_descriptor(url: &str) -> String {
    json!({ "method": "GET", "url": url }).to_string()
}

/// True when an outbound descriptor's URL pins the compiled feed id, `parsed`
/// and `encoding` — i.e. the subscription this adapter requires
/// (`polymarket_twap::finish` completeness assertion). Used by [`finish`].
///
/// [`finish`]: PythHermesAdapter::finish
fn descriptor_pins_subscription(payload: &str) -> bool {
    #[derive(Deserialize)]
    struct Descriptor {
        #[serde(default)]
        url: Option<String>,
    }
    let Ok(descriptor) = serde_json::from_str::<Descriptor>(payload) else {
        return false;
    };
    let Some(url) = descriptor.url else {
        return false;
    };
    url_pins_subscription(&url)
}

fn url_pins_subscription(url: &str) -> bool {
    url.contains(&format!("ids[]={FEED_ID}"))
        && url.contains("parsed=true")
        && url.contains("encoding=hex")
}

/// Deterministic Pyth Hermes price parser and per-epoch ordering state. One
/// adapter serves one connection (SSE stream, or the REST anchor).
///
/// Hermes carries no per-message sequence, so the cursor is `(last_slot,
/// last_publish_time_ms)`, both reset on `Connected`/`Disconnected` (§3.4,
/// `bybit.rs` pattern). The crate's own `next_sequence` advances only after the
/// fallible `SourceEvent` construction succeeds, so `SequenceExhausted` can never
/// leave the cursor ahead of the last emitted update (§3.5).
pub struct PythHermesAdapter {
    connection: PythHermesConnection,
    publisher_epoch: String,
    guard: RecordGuard,
    next_sequence: u64,
    last_slot: Option<u64>,
    last_publish_time_ms: Option<i64>,
    /// Scaled outputs of the last admitted update, so a same-slot redelivery can
    /// be told from a same-slot value change (§3.4). Cleared with the cursor.
    last_values: Option<ScaledValues>,
    saw_subscription: bool,
}

impl PythHermesAdapter {
    /// Construct a Pyth Hermes adapter for one connection.
    ///
    /// `publisher_epoch` is copied verbatim into every emitted event; it must be
    /// non-blank and at most 128 bytes, or the call returns
    /// [`AdapterError::InvalidRecord`].
    pub fn new(
        publisher_epoch: impl Into<String>,
        connection: PythHermesConnection,
    ) -> Result<Self, AdapterError> {
        let publisher_epoch = publisher_epoch.into();
        validate_publisher_epoch(&publisher_epoch)?;
        Ok(Self {
            connection,
            publisher_epoch,
            guard: RecordGuard::new(crate::Provider::PythHermes),
            next_sequence: 0,
            last_slot: None,
            last_publish_time_ms: None,
            last_values: None,
            saw_subscription: false,
        })
    }

    /// Force the next per-source sequence, so the overflow guard (§3.5) is
    /// testable without emitting `u64::MAX` events.
    #[cfg(test)]
    fn force_next_sequence(&mut self, next: u64) {
        self.next_sequence = next;
    }

    /// End-of-stream completeness check. The SSE stream must have recorded
    /// exactly one outbound request pinning `ids[]=FEED_ID`, `parsed=true` and
    /// `encoding=hex`; a REST anchor connection has no subscription contract and
    /// always returns `Ok` (test T-N16).
    pub fn finish(&self) -> Result<(), AdapterError> {
        if self.connection == PythHermesConnection::Stream && !self.saw_subscription {
            return Err(AdapterError::InvalidRecord(
                "Pyth Hermes stream capture is missing its pinned ids[]=FEED_ID subscription"
                    .to_owned(),
            ));
        }
        Ok(())
    }

    fn reset_cursor(&mut self) {
        self.last_slot = None;
        self.last_publish_time_ms = None;
        self.last_values = None;
    }

    /// Ingest one price-carrying payload: an SSE event block on the stream
    /// connection, or a REST `latest` body on the anchor connection.
    fn ingest_payload(
        &mut self,
        record: &RawRecord,
        payload: &str,
    ) -> Result<IngestOutcome, AdapterError> {
        let json = match self.connection {
            PythHermesConnection::Stream => match parse_stream_event(payload) {
                Ok(StreamFrame::Data(json)) => json,
                Ok(StreamFrame::Heartbeat) => {
                    return Ok(IngestOutcome::notice(FeedNotice::info(
                        FeedNoticeCode::Heartbeat,
                        "Pyth Hermes SSE keepalive",
                    )));
                }
                // `event:`/`id:`/`retry:` only: recognized, no event, no notice.
                Ok(StreamFrame::Meta) => return Ok(IngestOutcome::default()),
                Err(reject) => {
                    return Ok(IngestOutcome::notice(FeedNotice::info(
                        reject.code,
                        reject.detail,
                    )));
                }
            },
            PythHermesConnection::Latest => payload.to_owned(),
        };
        // Validation runs BEFORE any ordering or cursor logic
        // (`bybit.rs::trade_validates_before_the_ordering_check`).
        match decode_price_update(&json, &record.received_at) {
            Ok(update) => self.emit(record, update),
            Err(reject) => Ok(IngestOutcome::notice(FeedNotice::info(
                reject.code,
                reject.detail,
            ))),
        }
    }

    /// Apply the ordering matrix (§3.4) to a validated update and emit it, or
    /// classify it as a duplicate / out-of-order / gap.
    fn emit(
        &mut self,
        record: &RawRecord,
        update: DecodedUpdate,
    ) -> Result<IngestOutcome, AdapterError> {
        match (self.last_slot, self.last_publish_time_ms) {
            (Some(last_slot), Some(last_publish)) => {
                if update.slot < last_slot || update.publish_time_ms < last_publish {
                    return Ok(IngestOutcome::notice(FeedNotice::info(
                        FeedNoticeCode::OutOfOrder,
                        format!(
                            "Pyth Hermes slot {} / publish {} precedes the admitted {last_slot} / {last_publish}; skipped",
                            update.slot, update.publish_time_ms
                        ),
                    )));
                }
                if update.slot == last_slot {
                    if update.publish_time_ms == last_publish
                        && update.scaled_values_equal(&self.last_values)
                    {
                        // Same slot, all scaled values equal: a redelivery.
                        return Ok(IngestOutcome::notice(FeedNotice::info(
                            FeedNoticeCode::Duplicate,
                            "Pyth Hermes repeated an admitted slot; skipped",
                        )));
                    }
                    // Same slot, a value differs: fail closed on the state.
                    return Ok(IngestOutcome::notice(FeedNotice::info(
                        FeedNoticeCode::StateInconsistent,
                        "Pyth Hermes reused an admitted slot with a different value; skipped",
                    )));
                }
                // slot advances. A re-aggregation at the same publish time is
                // legal: emit and advance `last_slot` only, with an informational
                // Duplicate notice.
                let mut notices = Vec::new();
                if update.publish_time_ms == last_publish {
                    notices.push(FeedNotice::info(
                        FeedNoticeCode::Duplicate,
                        "Pyth Hermes re-aggregated at the same publish time; slot advanced",
                    ));
                }
                // Gap: the update's `prev_publish_time` does not chain to the last
                // admitted publish time. Emit the update (valid and current); the
                // processor discards its state and re-warms. No adapter-side
                // recovery action (§3.4) — the notice carries `recovery: None`.
                if update.prev_publish_time_ms != last_publish {
                    notices.push(FeedNotice::info(
                        FeedNoticeCode::Gap,
                        format!(
                            "Pyth Hermes prev_publish_time {} does not chain to the last admitted {last_publish}; state re-warms",
                            update.prev_publish_time_ms
                        ),
                    ));
                }
                self.emit_event(record, update, notices)
            }
            _ => {
                // First update of the epoch.
                self.emit_event(record, update, Vec::new())
            }
        }
    }

    fn emit_event(
        &mut self,
        record: &RawRecord,
        update: DecodedUpdate,
        notices: Vec<FeedNotice>,
    ) -> Result<IngestOutcome, AdapterError> {
        let event = self.source_event(record, &update)?;
        // Advance the cursor only AFTER the fallible event construction succeeds
        // (§3.5), so a `SequenceExhausted` never leaves it ahead of the last
        // emitted update.
        self.last_slot = Some(update.slot);
        self.last_publish_time_ms = Some(update.publish_time_ms);
        self.last_values = Some(update.scaled.clone());
        Ok(IngestOutcome {
            events: vec![event],
            notices,
        })
    }

    fn source_event(
        &mut self,
        record: &RawRecord,
        update: &DecodedUpdate,
    ) -> Result<SourceEvent, AdapterError> {
        let values = BTreeMap::from([
            ("instrument".to_owned(), json!(INSTRUMENT)),
            ("feed_id".to_owned(), json!(FEED_ID)),
            ("symbol".to_owned(), json!(SYMBOL)),
            (
                "update_kind".to_owned(),
                json!(self.connection.update_kind()),
            ),
            ("price".to_owned(), json!(update.scaled.price)),
            ("conf".to_owned(), json!(update.scaled.conf)),
            ("price_raw".to_owned(), json!(update.price_raw)),
            ("conf_raw".to_owned(), json!(update.conf_raw)),
            ("expo".to_owned(), json!(update.expo)),
            ("ema_price".to_owned(), json!(update.scaled.ema_price)),
            ("ema_conf".to_owned(), json!(update.scaled.ema_conf)),
            ("ema_price_raw".to_owned(), json!(update.ema_price_raw)),
            ("ema_conf_raw".to_owned(), json!(update.ema_conf_raw)),
            ("slot".to_owned(), json!(update.slot)),
            (
                "prev_publish_time".to_owned(),
                json!(update.prev_publish_time_rfc3339),
            ),
            (
                "proof_available_time".to_owned(),
                json!(update.proof_available_time_rfc3339),
            ),
            ("event_time".to_owned(), json!(update.event_time_rfc3339)),
        ]);
        let sequence = self.next_sequence;
        self.next_sequence = self
            .next_sequence
            .checked_add(1)
            .ok_or(AdapterError::SequenceExhausted)?;
        Ok(SourceEvent {
            schema_version: 1,
            source_id: SOURCE_ID.to_owned(),
            source_version: SOURCE_VERSION.to_owned(),
            publisher_epoch: self.publisher_epoch.clone(),
            sequence,
            event_time: update.event_time_rfc3339.clone(),
            received_at: record.received_at.clone(),
            values,
        })
    }
}

impl FeedAdapter for PythHermesAdapter {
    fn ingest(&mut self, record: &RawRecord) -> Result<IngestOutcome, AdapterError> {
        match self.guard.observe(record)? {
            GuardAction::Configuration(_) => Err(AdapterError::InvalidRecord(
                "Pyth Hermes captures do not accept configuration records".to_owned(),
            )),
            GuardAction::Connected => {
                self.reset_cursor();
                Ok(IngestOutcome::notice(FeedNotice::info(
                    FeedNoticeCode::Connected,
                    "Pyth Hermes connected; per-epoch cursor reset",
                )))
            }
            GuardAction::Disconnected => {
                self.reset_cursor();
                Ok(IngestOutcome::notice(FeedNotice::info(
                    FeedNoticeCode::Disconnected,
                    "Pyth Hermes connection ended",
                )))
            }
            // The outbound record is the recorded GET descriptor. On the stream
            // connection it pins the subscription (its URL carries
            // `ids[]=FEED_ID`); it is journal evidence, not an event.
            GuardAction::Outbound => {
                if self.connection == PythHermesConnection::Stream {
                    let payload = record.payload.as_deref().unwrap_or_default();
                    if descriptor_pins_subscription(payload) {
                        self.saw_subscription = true;
                    }
                }
                Ok(IngestOutcome::default())
            }
            // A refused HTTP request, recorded by the capture layer because an SSE
            // GET or a REST anchor probe that never returns a body would otherwise
            // be invisible. Classified by the recorded status (§3.2 M6/M7/M8).
            GuardAction::Failed(detail) => self.ingest_failure(detail),
            GuardAction::Inbound(payload) => self.ingest_payload(record, payload),
        }
    }
}

impl PythHermesAdapter {
    fn ingest_failure(&mut self, detail: &str) -> Result<IngestOutcome, AdapterError> {
        match classify_failure(detail) {
            FailureClass::Terminal(reason) => Err(AdapterError::InvalidRecord(reason)),
            FailureClass::RateLimited(reason) => Ok(IngestOutcome::notice(FeedNotice::info(
                FeedNoticeCode::VenueStatus,
                reason,
            ))),
            FailureClass::Transient(reason) => Ok(IngestOutcome::notice(FeedNotice::info(
                FeedNoticeCode::StateInconsistent,
                reason,
            ))),
        }
    }
}

/// Classification of a recorded HTTP failure (§3.2 M6/M7/M8).
enum FailureClass {
    /// 401/403 (unauthorized) or 404 (bad id/path): terminate this feed only.
    Terminal(String),
    /// 429: honour the backoff, keep the feed.
    RateLimited(String),
    /// 5xx or a transport fault: transient, reconnect with backoff.
    Transient(String),
}

fn classify_failure(detail: &str) -> FailureClass {
    #[derive(Deserialize)]
    struct Failure {
        #[serde(default)]
        failure: Option<String>,
        #[serde(default)]
        status: Option<u16>,
    }
    let bounded = bounded_detail("Pyth Hermes request failed", detail);
    let Ok(failure) = serde_json::from_str::<Failure>(detail) else {
        return FailureClass::Transient(bounded);
    };
    match (failure.failure.as_deref(), failure.status) {
        (Some("http_status"), Some(401 | 403)) => FailureClass::Terminal(format!(
            "Pyth Hermes returned unauthorized; the feed is terminal until re-keyed ({bounded})"
        )),
        (Some("http_status"), Some(404)) => FailureClass::Terminal(format!(
            "Pyth Hermes returned not-found on the id or path; misconfigured ({bounded})"
        )),
        (Some("http_status"), Some(429)) => FailureClass::RateLimited(bounded),
        _ => FailureClass::Transient(bounded),
    }
}

// --- SSE framing (§3.2 M1/M2/M3). One recorded InboundText is one SSE event
// block: its lines up to the blank-line boundary. The capture layer bounds the
// block and each line; the adapter re-checks both so replay refuses an oversize
// block by the same rule.

/// The classification of one recorded SSE event block.
enum StreamFrame {
    /// A price update: the concatenated `data` field, non-empty and to be decoded.
    Data(String),
    /// A comment/keepalive (`:`-prefixed line, or an empty `data:`): liveness only.
    Heartbeat,
    /// `event:`/`id:`/`retry:` only, no data: recognized, no event, no notice.
    Meta,
}

/// Parse one recorded SSE event block into its class. `deny_unknown_fields` is
/// applied later at JSON decode; here only the line framing and bounds are
/// enforced, and an oversize block or line is refused (shape-class, test T-N15).
fn parse_stream_event(block: &str) -> Result<StreamFrame, ResponseReject> {
    if block.len() > MAX_SSE_EVENT_BYTES {
        return Err(shape_reject(format!(
            "Pyth Hermes SSE event of {} bytes exceeds MAX_SSE_EVENT_BYTES ({MAX_SSE_EVENT_BYTES})",
            block.len()
        )));
    }
    let mut data = String::new();
    let mut saw_data = false;
    let mut saw_comment = false;
    let mut saw_meta = false;
    for raw_line in block.split('\n') {
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        if line.len() > MAX_SSE_LINE_BYTES {
            return Err(shape_reject(format!(
                "Pyth Hermes SSE line of {} bytes exceeds MAX_SSE_LINE_BYTES ({MAX_SSE_LINE_BYTES})",
                line.len()
            )));
        }
        if line.is_empty() {
            continue;
        }
        if let Some(comment) = line.strip_prefix(':') {
            let _ = comment;
            saw_comment = true;
            continue;
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };
        match field {
            "data" => {
                if value.is_empty() {
                    // An empty `data:` line is a keepalive (§3.2 M2).
                    saw_comment = true;
                } else {
                    if saw_data {
                        data.push('\n');
                    }
                    data.push_str(value);
                    saw_data = true;
                }
            }
            "event" | "id" | "retry" => saw_meta = true,
            // An unknown SSE field is ignored by the SSE spec; treat the block as
            // meta so it produces no event and no notice.
            _ => saw_meta = true,
        }
    }
    if saw_data && !data.is_empty() {
        Ok(StreamFrame::Data(data))
    } else if saw_comment {
        Ok(StreamFrame::Heartbeat)
    } else if saw_meta {
        Ok(StreamFrame::Meta)
    } else {
        // A wholly empty block is a keepalive.
        Ok(StreamFrame::Heartbeat)
    }
}

// --- Envelope decode and validation (§3.3 V1..V13).

/// A rejected update. Shape-class (V1..V4) carries [`FeedNoticeCode::UnsupportedMessage`];
/// value-class (V5..V13, and the empty-`parsed` M5) carries
/// [`FeedNoticeCode::StateInconsistent`]. Both are notice + skip, cursor
/// untouched, never session-fatal.
#[derive(Debug)]
struct ResponseReject {
    code: FeedNoticeCode,
    detail: String,
}

fn shape_reject(detail: impl Into<String>) -> ResponseReject {
    ResponseReject {
        code: FeedNoticeCode::UnsupportedMessage,
        detail: detail.into(),
    }
}

fn value_reject(detail: impl Into<String>) -> ResponseReject {
    ResponseReject {
        code: FeedNoticeCode::StateInconsistent,
        detail: detail.into(),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HermesEnvelope {
    binary: BinaryUpdate,
    parsed: Vec<ParsedRow>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BinaryUpdate {
    encoding: String,
    data: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ParsedRow {
    id: String,
    price: PriceFeed,
    ema_price: PriceFeed,
    metadata: Metadata,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PriceFeed {
    price: String,
    conf: String,
    expo: i64,
    publish_time: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Metadata {
    slot: u64,
    proof_available_time: i64,
    prev_publish_time: i64,
}

/// The scaled decimal outputs, kept so a same-slot redelivery can be told from a
/// same-slot value change (§3.4).
#[derive(Debug, Clone, PartialEq, Eq)]
struct ScaledValues {
    price: String,
    conf: String,
    ema_price: String,
    ema_conf: String,
}

/// One decoded, fully validated price update.
struct DecodedUpdate {
    price_raw: i64,
    conf_raw: u64,
    ema_price_raw: i64,
    ema_conf_raw: u64,
    expo: i64,
    slot: u64,
    publish_time_ms: i64,
    prev_publish_time_ms: i64,
    scaled: ScaledValues,
    event_time_rfc3339: String,
    prev_publish_time_rfc3339: String,
    proof_available_time_rfc3339: String,
}

impl DecodedUpdate {
    fn scaled_values_equal(&self, previous: &Option<ScaledValues>) -> bool {
        previous
            .as_ref()
            .is_some_and(|values| *values == self.scaled)
    }
}

/// Parse and fully validate one Hermes price envelope. Exposed for fuzzing
/// (test T-F2): arbitrary bytes must yield only a classified reject, never a
/// panic and never an allocation beyond the compiled bounds.
///
/// `received_at` is the envelope's local receipt instant, used for the skew
/// check (V9).
#[doc(hidden)]
pub fn decode_price_update_public(json: &str, received_at: &str) -> Result<(), String> {
    decode_price_update(json, received_at)
        .map(|_| ())
        .map_err(|reject| reject.detail)
}

fn decode_price_update(json: &str, received_at: &str) -> Result<DecodedUpdate, ResponseReject> {
    // V1: envelope parses with deny_unknown_fields at every level.
    let envelope: HermesEnvelope =
        serde_json::from_str(json).map_err(|error| shape_reject(error.to_string()))?;
    // M5: parsed absent or empty is a value-class StateInconsistent.
    if envelope.parsed.is_empty() {
        return Err(value_reject(
            "Pyth Hermes envelope carries an empty parsed set",
        ));
    }
    // V2: length in 1..=MAX_PARSED_ROWS; exactly one row is FEED_ID; a row with
    // any other id is refused, not ignored.
    if envelope.parsed.len() > MAX_PARSED_ROWS {
        return Err(shape_reject(format!(
            "Pyth Hermes parsed set has {} rows; at most {MAX_PARSED_ROWS} are admitted",
            envelope.parsed.len()
        )));
    }
    if envelope.parsed.iter().any(|row| row.id != FEED_ID) {
        return Err(shape_reject(
            "Pyth Hermes parsed set carries a non-BTC/USD feed id; refused",
        ));
    }
    if envelope.parsed.len() != 1 {
        return Err(shape_reject(
            "Pyth Hermes parsed set must carry exactly one BTC/USD row",
        ));
    }
    // V3: binary encoding hex; each element lowercase hex, even length, bounded.
    validate_binary(&envelope.binary)?;
    let row = &envelope.parsed[0];
    // V4: numeric text parses to the declared widths.
    let price_raw = parse_i64(&row.price.price, "price.price")?;
    let conf_raw = parse_u64(&row.price.conf, "price.conf")?;
    let ema_price_raw = parse_i64(&row.ema_price.price, "ema_price.price")?;
    let ema_conf_raw = parse_u64(&row.ema_price.conf, "ema_price.conf")?;
    let expo = row.price.expo;
    // V13: the EMA sub-object's exponent and publish time equal the aggregate's.
    if row.ema_price.expo != expo {
        return Err(value_reject(
            "Pyth Hermes ema_price.expo differs from price.expo; refused",
        ));
    }
    if row.ema_price.publish_time != row.price.publish_time {
        return Err(value_reject(
            "Pyth Hermes ema_price.publish_time differs from price.publish_time; refused",
        ));
    }
    // V5: exponent band.
    if !(EXPO_MIN..=EXPO_MAX).contains(&expo) {
        return Err(value_reject(format!(
            "Pyth Hermes expo {expo} is outside [{EXPO_MIN}, {EXPO_MAX}]"
        )));
    }
    // V11: exact rust_decimal scaling by 10^expo, never float.
    let price = scale(i128::from(price_raw), expo)?;
    let conf = scale(i128::from(conf_raw), expo)?;
    let ema_price = scale(i128::from(ema_price_raw), expo)?;
    let ema_conf = scale(i128::from(ema_conf_raw), expo)?;
    // V6: scaled price strictly positive.
    if price <= Decimal::ZERO {
        return Err(value_reject(
            "Pyth Hermes price is not strictly positive; refused",
        ));
    }
    // V7: scaled conf >= 0 and conf < price.
    if conf < Decimal::ZERO || conf >= price {
        return Err(value_reject(
            "Pyth Hermes conf is negative or not below price; refused",
        ));
    }
    // V8: unix-seconds timestamps through the shared event window (a value
    // already in ms or us lands outside and fails closed).
    let publish_time_ms = seconds_to_millis(row.price.publish_time, "publish_time")?;
    let prev_publish_time_ms =
        seconds_to_millis(row.metadata.prev_publish_time, "prev_publish_time")?;
    let event_time_rfc3339 = millis_to_rfc3339(publish_time_ms, "publish_time")
        .map_err(|error| value_reject(error.to_string()))?;
    let prev_publish_time_rfc3339 = millis_to_rfc3339(prev_publish_time_ms, "prev_publish_time")
        .map_err(|error| value_reject(error.to_string()))?;
    let proof_available_time_ms =
        seconds_to_millis(row.metadata.proof_available_time, "proof_available_time")?;
    let proof_available_time_rfc3339 =
        millis_to_rfc3339(proof_available_time_ms, "proof_available_time")
            .map_err(|error| value_reject(error.to_string()))?;
    // V9: publish time not implausibly ahead of local receipt.
    let received_at_ms = received_at_millis(received_at)?;
    if publish_time_ms > received_at_ms.saturating_add(MAX_PUBLISH_SKEW_MS) {
        return Err(value_reject(format!(
            "Pyth Hermes publish time {publish_time_ms} is more than {MAX_PUBLISH_SKEW_MS} ms ahead of receipt {received_at_ms}"
        )));
    }
    // V10: prev_publish_time <= publish_time.
    if row.metadata.prev_publish_time > row.price.publish_time {
        return Err(value_reject(
            "Pyth Hermes prev_publish_time is after publish_time; refused",
        ));
    }
    // V12: slot > 0.
    if row.metadata.slot == 0 {
        return Err(value_reject("Pyth Hermes slot is zero; refused"));
    }
    Ok(DecodedUpdate {
        price_raw,
        conf_raw,
        ema_price_raw,
        ema_conf_raw,
        expo,
        slot: row.metadata.slot,
        publish_time_ms,
        prev_publish_time_ms,
        scaled: ScaledValues {
            price: price.to_string(),
            conf: conf.to_string(),
            ema_price: ema_price.to_string(),
            ema_conf: ema_conf.to_string(),
        },
        event_time_rfc3339,
        prev_publish_time_rfc3339,
        proof_available_time_rfc3339,
    })
}

fn validate_binary(binary: &BinaryUpdate) -> Result<(), ResponseReject> {
    if binary.encoding != "hex" {
        return Err(shape_reject(format!(
            "Pyth Hermes binary.encoding is {:?}; expected \"hex\"",
            bounded(&binary.encoding)
        )));
    }
    if binary.data.len() > MAX_PARSED_ROWS {
        return Err(shape_reject(format!(
            "Pyth Hermes binary.data has {} elements; at most {MAX_PARSED_ROWS} are admitted",
            binary.data.len()
        )));
    }
    for element in &binary.data {
        if element.len() > MAX_BINARY_CHARS {
            return Err(shape_reject(format!(
                "Pyth Hermes binary.data element of {} chars exceeds MAX_BINARY_CHARS ({MAX_BINARY_CHARS})",
                element.len()
            )));
        }
        if !element.len().is_multiple_of(2) {
            return Err(shape_reject(
                "Pyth Hermes binary.data element has an odd hex length",
            ));
        }
        if !element
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(shape_reject(
                "Pyth Hermes binary.data element is not lowercase hex",
            ));
        }
    }
    Ok(())
}

/// Scale a raw integer by `10^expo` with exact decimal arithmetic. `expo` is in
/// `[EXPO_MIN, EXPO_MAX]` (V5), i.e. non-positive, so the scale is `-expo` and no
/// multiplication is needed; scale <= 18 <= 28 and the coefficient fits 96 bits
/// (V11).
fn scale(raw: i128, expo: i64) -> Result<Decimal, ResponseReject> {
    let scale = u32::try_from(-expo)
        .map_err(|_| value_reject("Pyth Hermes exponent is positive; unsupported"))?;
    Decimal::try_from_i128_with_scale(raw, scale)
        .map_err(|error| value_reject(format!("Pyth Hermes decimal scaling failed: {error}")))
}

fn parse_i64(text: &str, field: &str) -> Result<i64, ResponseReject> {
    text.parse::<i64>()
        .map_err(|_| shape_reject(format!("Pyth Hermes {field} is not base-10 i64 text")))
}

fn parse_u64(text: &str, field: &str) -> Result<u64, ResponseReject> {
    text.parse::<u64>()
        .map_err(|_| shape_reject(format!("Pyth Hermes {field} is not base-10 u64 text")))
}

/// Convert unix seconds to milliseconds, refusing an overflow (shape-class: a
/// word wide enough to overflow is malformed).
fn seconds_to_millis(seconds: i64, field: &str) -> Result<i64, ResponseReject> {
    seconds
        .checked_mul(1_000)
        .ok_or_else(|| shape_reject(format!("Pyth Hermes {field} seconds-to-millis overflowed")))
}

fn received_at_millis(received_at: &str) -> Result<i64, ResponseReject> {
    chrono::DateTime::parse_from_rfc3339(received_at)
        .map(|value| value.timestamp_millis())
        .map_err(|error| shape_reject(format!("Pyth Hermes received_at is not RFC 3339: {error}")))
}

fn bounded(value: &str) -> String {
    value.chars().take(32).collect()
}

fn bounded_detail(prefix: &str, value: &str) -> String {
    format!("{prefix}: {}", value.chars().take(80).collect::<String>())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use serde_json::json;

    use super::{
        FEED_ID, PythHermesAdapter, PythHermesConnection, decode_price_update, latest_url,
        stream_url,
    };
    use crate::{
        AdapterError, CaptureStream, FeedAdapter, FeedNoticeCode, IngestOutcome, RawRecord,
        RecordKind,
    };

    // --- Synthetic fixtures: their SHAPE matches the authenticated capture of
    // 2026-09-04 (confirmed by the observed_capture_frames_* tests below); their
    // VALUES are invented so each validation rule is exercised in isolation.
    const RECEIVED_AT: &str = "2025-08-28T16:53:25.000Z";
    const PUBLISH_SECONDS: i64 = 1_756_400_000; // 2025-08-28T16:53:20Z, inside the window.

    #[allow(clippy::too_many_arguments)]
    fn envelope(
        id: &str,
        price_raw: &str,
        conf_raw: &str,
        expo: i64,
        publish_time: i64,
        prev_publish_time: i64,
        slot: u64,
        ema_expo: i64,
        ema_publish_time: i64,
    ) -> String {
        json!({
            "binary": { "encoding": "hex", "data": ["deadbeef"] },
            "parsed": [{
                "id": id,
                "price": {
                    "price": price_raw, "conf": conf_raw,
                    "expo": expo, "publish_time": publish_time
                },
                "ema_price": {
                    "price": "7766000000000", "conf": "4000000000",
                    "expo": ema_expo, "publish_time": ema_publish_time
                },
                "metadata": {
                    "slot": slot,
                    "proof_available_time": publish_time + 1,
                    "prev_publish_time": prev_publish_time
                }
            }]
        })
        .to_string()
    }

    /// A canonical valid envelope: one BTC/USD row, exponent -8, slot `slot`,
    /// chaining `prev` to the previous publish time.
    fn valid(slot: u64, publish_time: i64, prev_publish_time: i64) -> String {
        envelope(
            FEED_ID,
            "7766779700000",
            "5000000000",
            -8,
            publish_time,
            prev_publish_time,
            slot,
            -8,
            publish_time,
        )
    }

    fn sse_block(json: &str) -> String {
        format!("data: {json}")
    }

    fn new_adapter(connection: PythHermesConnection) -> PythHermesAdapter {
        PythHermesAdapter::new("test-epoch", connection).expect("adapter constructs")
    }

    fn record(ordinal: u64, epoch: u64, kind: RecordKind, payload: Option<&str>) -> RawRecord {
        RawRecord {
            schema_version: 1,
            provider: crate::Provider::PythHermes,
            stream: CaptureStream::PythHermesStream,
            connection_epoch: epoch,
            ordinal,
            received_at: RECEIVED_AT.to_owned(),
            capture_elapsed_ns: None,
            connect_started_elapsed_ns: None,
            kind,
            payload: payload.map(str::to_owned),
        }
    }

    fn connected(ordinal: u64, epoch: u64) -> RawRecord {
        record(ordinal, epoch, RecordKind::Connected, None)
    }

    fn inbound_stream(ordinal: u64, json: &str) -> RawRecord {
        record(ordinal, 1, RecordKind::InboundText, Some(&sse_block(json)))
    }

    fn connected_adapter(connection: PythHermesConnection) -> PythHermesAdapter {
        let mut adapter = new_adapter(connection);
        adapter.ingest(&connected(1, 1)).expect("connects");
        adapter
    }

    // --- T-U1: the request-URL builders are pure and pin id, parsed, encoding.
    #[test]
    fn request_urls_pin_the_feed_id_parsed_and_encoding() {
        for url in [stream_url(), latest_url()] {
            assert!(url.contains(&format!("ids[]={FEED_ID}")), "{url}");
            assert!(url.contains("parsed=true"), "{url}");
            assert!(url.contains("encoding=hex"), "{url}");
            // The key is a header, never the URL.
            assert!(!url.contains("token="), "{url}");
        }
        assert!(stream_url().contains("/v2/updates/price/stream"));
        assert!(latest_url().contains("/v2/updates/price/latest"));
    }

    // --- T-U2: exact scaling by 10^expo for -8, 0, -18 with no float. Every
    // price_raw here fits i64 (V4).
    #[test]
    fn scaling_is_exact_across_exponents() {
        for (expo, price_raw, expected) in [
            (-8_i64, "7766779700000", "77667.79700000"),
            (0, "77668", "77668"),
            (-18, "77668000000000", "0.000077668000000000"),
        ] {
            let json = envelope(
                FEED_ID,
                price_raw,
                "1",
                expo,
                PUBLISH_SECONDS,
                PUBLISH_SECONDS - 1,
                100,
                expo,
                PUBLISH_SECONDS,
            );
            let mut adapter = connected_adapter(PythHermesConnection::Stream);
            let outcome = adapter
                .ingest(&inbound_stream(2, &json))
                .expect("valid update");
            assert_eq!(
                outcome.events[0].values["price"].as_str().unwrap(),
                expected,
                "expo {expo}"
            );
        }
    }

    // --- T-U3: seconds -> RFC 3339, and rejection of a value already in ms/us.
    #[test]
    fn seconds_convert_and_millisecond_or_microsecond_values_fail_closed() {
        let mut adapter = connected_adapter(PythHermesConnection::Stream);
        let outcome = adapter
            .ingest(&inbound_stream(
                2,
                &valid(100, PUBLISH_SECONDS, PUBLISH_SECONDS - 1),
            ))
            .expect("valid update");
        assert_eq!(
            outcome.events[0].values["event_time"].as_str().unwrap(),
            "2025-08-28T16:53:20.000Z"
        );
        // publish_time already in milliseconds is ~1000x too large after
        // seconds->millis and lands far outside the plausibility window.
        for scaled in [PUBLISH_SECONDS * 1_000, PUBLISH_SECONDS * 1_000_000] {
            let mut adapter = connected_adapter(PythHermesConnection::Stream);
            let outcome = adapter
                .ingest(&inbound_stream(2, &valid(100, scaled, scaled - 1)))
                .expect("operational");
            assert!(outcome.events.is_empty());
            assert_eq!(outcome.notices[0].code, FeedNoticeCode::StateInconsistent);
        }
    }

    // --- T-U4: the §3.4 ordering matrix, one case per row.
    #[test]
    fn ordering_matrix_one_case_per_row() {
        let mut adapter = connected_adapter(PythHermesConnection::Stream);
        // First update of an epoch: emit, set cursor.
        let first = adapter
            .ingest(&inbound_stream(
                2,
                &valid(100, PUBLISH_SECONDS, PUBLISH_SECONDS),
            ))
            .expect("first");
        assert_eq!(first.events.len(), 1);
        assert!(first.notices.is_empty());
        // slot > and publish_time >: emit, advance.
        let advance = adapter
            .ingest(&inbound_stream(
                3,
                &valid(101, PUBLISH_SECONDS + 1, PUBLISH_SECONDS),
            ))
            .expect("advance");
        assert_eq!(advance.events.len(), 1);
        assert!(advance.notices.is_empty());
        // slot ==, all values equal: drop, Duplicate.
        let dup = adapter
            .ingest(&inbound_stream(
                4,
                &valid(101, PUBLISH_SECONDS + 1, PUBLISH_SECONDS),
            ))
            .expect("dup");
        assert!(dup.events.is_empty());
        assert_eq!(dup.notices[0].code, FeedNoticeCode::Duplicate);
        // slot ==, a value differs: drop, StateInconsistent.
        let changed = envelope(
            FEED_ID,
            "7766779800000",
            "5000000000",
            -8,
            PUBLISH_SECONDS + 1,
            PUBLISH_SECONDS,
            101,
            -8,
            PUBLISH_SECONDS + 1,
        );
        let inconsistent = adapter
            .ingest(&inbound_stream(5, &changed))
            .expect("state change");
        assert!(inconsistent.events.is_empty());
        assert_eq!(
            inconsistent.notices[0].code,
            FeedNoticeCode::StateInconsistent
        );
        // slot <: drop, OutOfOrder.
        let older = adapter
            .ingest(&inbound_stream(
                6,
                &valid(50, PUBLISH_SECONDS, PUBLISH_SECONDS),
            ))
            .expect("older");
        assert!(older.events.is_empty());
        assert_eq!(older.notices[0].code, FeedNoticeCode::OutOfOrder);
        // slot > but publish_time ==: emit, informational Duplicate.
        let reaggregate = adapter
            .ingest(&inbound_stream(
                7,
                &valid(102, PUBLISH_SECONDS + 1, PUBLISH_SECONDS + 1),
            ))
            .expect("re-aggregate");
        assert_eq!(reaggregate.events.len(), 1);
        assert!(
            reaggregate
                .notices
                .iter()
                .any(|notice| notice.code == FeedNoticeCode::Duplicate)
        );
    }

    // --- T-U5: a prev_publish_time chain break raises exactly one Gap, with
    // recovery None, and still emits.
    #[test]
    fn a_chain_break_raises_one_gap_and_still_emits() {
        let mut adapter = connected_adapter(PythHermesConnection::Stream);
        adapter
            .ingest(&inbound_stream(
                2,
                &valid(100, PUBLISH_SECONDS, PUBLISH_SECONDS),
            ))
            .expect("first");
        // prev_publish_time does not chain to the last admitted publish time.
        let gapped = adapter
            .ingest(&inbound_stream(
                3,
                &valid(101, PUBLISH_SECONDS + 2, PUBLISH_SECONDS + 1),
            ))
            .expect("gapped");
        assert_eq!(gapped.events.len(), 1, "the current update still emits");
        let gaps: Vec<_> = gapped
            .notices
            .iter()
            .filter(|notice| notice.code == FeedNoticeCode::Gap)
            .collect();
        assert_eq!(gaps.len(), 1);
        assert!(
            gaps[0].recovery.is_none(),
            "no adapter-side recovery action"
        );
    }

    // --- T-U6: Connected/Disconnected reset the cursor per epoch.
    #[test]
    fn connect_and_disconnect_reset_the_cursor() {
        let mut adapter = connected_adapter(PythHermesConnection::Stream);
        adapter
            .ingest(&inbound_stream(
                2,
                &valid(100, PUBLISH_SECONDS, PUBLISH_SECONDS),
            ))
            .expect("first");
        adapter
            .ingest(&record(3, 1, RecordKind::Disconnected, None))
            .expect("disconnects");
        adapter.ingest(&connected(4, 2)).expect("reconnects");
        // After a reset, the SAME slot is the first of a new epoch and emits.
        let again = adapter
            .ingest(&RawRecord {
                connection_epoch: 2,
                ..inbound_stream(5, &valid(100, PUBLISH_SECONDS, PUBLISH_SECONDS))
            })
            .expect("first of new epoch");
        assert_eq!(again.events.len(), 1);
        assert!(again.notices.is_empty());
    }

    // --- T-U7: sequence overflow returns SequenceExhausted and leaves the cursor
    // unadvanced.
    #[test]
    fn sequence_overflow_leaves_the_cursor_unadvanced() {
        let mut adapter = connected_adapter(PythHermesConnection::Stream);
        adapter.force_next_sequence(u64::MAX);
        let error = adapter
            .ingest(&inbound_stream(
                2,
                &valid(100, PUBLISH_SECONDS, PUBLISH_SECONDS),
            ))
            .expect_err("overflow");
        assert!(matches!(error, AdapterError::SequenceExhausted));
        // The cursor never advanced: the same update is now the epoch's first and
        // would emit if the sequence were not exhausted.
        adapter.force_next_sequence(0);
        let outcome = adapter
            .ingest(&inbound_stream(
                3,
                &valid(100, PUBLISH_SECONDS, PUBLISH_SECONDS),
            ))
            .expect("emits after the sequence is reset");
        assert_eq!(outcome.events.len(), 1);
        assert_eq!(outcome.events[0].sequence, 0);
    }

    // --- T-U8: V13, an EMA sub-object exponent or publish time that differs from
    // the aggregate's refuses the update.
    #[test]
    fn v13_ema_expo_or_publish_time_mismatch_refuses() {
        let mut adapter = connected_adapter(PythHermesConnection::Stream);
        let mismatched_expo = envelope(
            FEED_ID,
            "7766779700000",
            "5000000000",
            -8,
            PUBLISH_SECONDS,
            PUBLISH_SECONDS,
            100,
            -7,
            PUBLISH_SECONDS,
        );
        let outcome = adapter
            .ingest(&inbound_stream(2, &mismatched_expo))
            .expect("operational");
        assert!(outcome.events.is_empty());
        assert_eq!(outcome.notices[0].code, FeedNoticeCode::StateInconsistent);

        let mut adapter = connected_adapter(PythHermesConnection::Stream);
        let mismatched_time = envelope(
            FEED_ID,
            "7766779700000",
            "5000000000",
            -8,
            PUBLISH_SECONDS,
            PUBLISH_SECONDS,
            100,
            -8,
            PUBLISH_SECONDS + 5,
        );
        let outcome = adapter
            .ingest(&inbound_stream(2, &mismatched_time))
            .expect("operational");
        assert!(outcome.events.is_empty());
        assert_eq!(outcome.notices[0].code, FeedNoticeCode::StateInconsistent);
    }

    // --- T-N: message types and validation failures.
    #[test]
    fn m2_comment_and_empty_data_are_heartbeats_m3_meta_is_silent() {
        let mut adapter = connected_adapter(PythHermesConnection::Stream);
        for (ordinal, block) in [(2_u64, ": keepalive"), (3, "data: ")] {
            let outcome = adapter
                .ingest(&record(ordinal, 1, RecordKind::InboundText, Some(block)))
                .expect("operational");
            assert!(outcome.events.is_empty());
            assert_eq!(outcome.notices[0].code, FeedNoticeCode::Heartbeat);
        }
        // event:/id:/retry: only: no event, no notice.
        let meta = adapter
            .ingest(&record(
                9,
                1,
                RecordKind::InboundText,
                Some("event: message\nid: 42\nretry: 3000"),
            ))
            .expect("operational");
        assert!(meta.events.is_empty());
        assert!(meta.notices.is_empty());
    }

    #[test]
    fn m5_empty_parsed_is_state_inconsistent() {
        let mut adapter = connected_adapter(PythHermesConnection::Stream);
        let empty =
            json!({ "binary": { "encoding": "hex", "data": [] }, "parsed": [] }).to_string();
        let outcome = adapter
            .ingest(&inbound_stream(2, &empty))
            .expect("operational");
        assert!(outcome.events.is_empty());
        assert_eq!(outcome.notices[0].code, FeedNoticeCode::StateInconsistent);
    }

    #[test]
    fn m6_unauthorized_failure_is_terminal() {
        let mut adapter = connected_adapter(PythHermesConnection::Stream);
        for status in [401_u16, 403, 404] {
            let mut adapter = connected_adapter(PythHermesConnection::Stream);
            let failed = crate::RawRecord::failed_at(
                CaptureStream::PythHermesStream,
                1,
                2,
                crate::raw::FailureKind::HttpStatus(status),
                "refused",
                RECEIVED_AT.to_owned(),
                0,
            );
            let error = adapter.ingest(&failed).expect_err("terminal");
            assert!(
                matches!(error, AdapterError::InvalidRecord(_)),
                "status {status}"
            );
        }
        // A 429 is a VenueStatus notice, not terminal.
        let throttled = crate::RawRecord::failed_at(
            CaptureStream::PythHermesStream,
            1,
            2,
            crate::raw::FailureKind::HttpStatus(429),
            "rate limited",
            RECEIVED_AT.to_owned(),
            0,
        );
        let outcome = adapter.ingest(&throttled).expect("operational");
        assert_eq!(outcome.notices[0].code, FeedNoticeCode::VenueStatus);
        assert!(outcome.events.is_empty());
    }

    #[test]
    fn m8_server_and_transport_failures_are_transient() {
        let mut adapter = connected_adapter(PythHermesConnection::Stream);
        for failed in [
            crate::RawRecord::failed_at(
                CaptureStream::PythHermesStream,
                1,
                2,
                crate::raw::FailureKind::HttpStatus(503),
                "server error",
                RECEIVED_AT.to_owned(),
                0,
            ),
            crate::RawRecord::failed_at(
                CaptureStream::PythHermesStream,
                1,
                3,
                crate::raw::FailureKind::Transport,
                "reset",
                RECEIVED_AT.to_owned(),
                0,
            ),
        ] {
            let outcome = adapter.ingest(&failed).expect("operational");
            assert!(outcome.events.is_empty());
            assert_eq!(outcome.notices[0].code, FeedNoticeCode::StateInconsistent);
        }
    }

    #[test]
    fn m9_unknown_json_and_m10_outbound_and_m13_configuration() {
        // M9: well-formed JSON that is not the envelope fails deny_unknown_fields.
        let mut adapter = connected_adapter(PythHermesConnection::Stream);
        let unknown = adapter
            .ingest(&inbound_stream(2, r#"{"hello":"world"}"#))
            .expect("operational");
        assert!(unknown.events.is_empty());
        assert_eq!(unknown.notices[0].code, FeedNoticeCode::UnsupportedMessage);
        // M10: an outbound descriptor is silent (and pins the subscription).
        let outbound = adapter
            .ingest(&record(
                3,
                1,
                RecordKind::OutboundText,
                Some(&super::request_descriptor(&stream_url())),
            ))
            .expect("operational");
        assert!(outbound.events.is_empty());
        assert!(outbound.notices.is_empty());
        // M13: a configuration record is refused.
        let mut fresh = new_adapter(PythHermesConnection::Stream);
        let configuration = RawRecord {
            schema_version: 3,
            provider: crate::Provider::PythHermes,
            stream: CaptureStream::PythHermesStream,
            connection_epoch: 1,
            ordinal: 1,
            received_at: RECEIVED_AT.to_owned(),
            capture_elapsed_ns: None,
            connect_started_elapsed_ns: None,
            kind: RecordKind::Configuration,
            payload: Some("{}".to_owned()),
        };
        assert!(matches!(
            fresh.ingest(&configuration),
            Err(AdapterError::InvalidRecord(_))
        ));
    }

    // --- T-N14: a parsed row carrying another BTC-variant id is refused.
    #[test]
    fn a_non_btc_usd_feed_id_is_refused_not_ignored() {
        let mut adapter = connected_adapter(PythHermesConnection::Stream);
        // WBTC id (from the catalogue) — not a substitute.
        let wbtc = "c9d8b075a5c69303365ae23633d4e085199bf5c520a3b90fed1322a0342ffc33";
        let json = envelope(
            wbtc,
            "7766779700000",
            "5000000000",
            -8,
            PUBLISH_SECONDS,
            PUBLISH_SECONDS,
            100,
            -8,
            PUBLISH_SECONDS,
        );
        let outcome = adapter
            .ingest(&inbound_stream(2, &json))
            .expect("operational");
        assert!(outcome.events.is_empty());
        assert_eq!(outcome.notices[0].code, FeedNoticeCode::UnsupportedMessage);
    }

    // --- T-N15: an oversize SSE event or line is bounded and refused.
    #[test]
    fn an_oversize_sse_event_is_bounded_and_refused() {
        let mut adapter = connected_adapter(PythHermesConnection::Stream);
        let huge = format!("data: {}", "x".repeat(super::MAX_SSE_EVENT_BYTES + 10));
        let outcome = adapter
            .ingest(&record(2, 1, RecordKind::InboundText, Some(&huge)))
            .expect("operational");
        assert!(outcome.events.is_empty());
        assert_eq!(outcome.notices[0].code, FeedNoticeCode::UnsupportedMessage);
    }

    // --- T-N16: finish() fails when the capture has no pinned subscription.
    #[test]
    fn finish_requires_the_pinned_subscription_on_the_stream() {
        let mut adapter = connected_adapter(PythHermesConnection::Stream);
        assert!(adapter.finish().is_err(), "no outbound seen yet");
        adapter
            .ingest(&record(
                2,
                1,
                RecordKind::OutboundText,
                Some(&super::request_descriptor(&stream_url())),
            ))
            .expect("outbound");
        assert!(adapter.finish().is_ok(), "the pinned subscription was seen");
        // The REST anchor has no subscription contract.
        let rest = new_adapter(PythHermesConnection::Latest);
        assert!(rest.finish().is_ok());
        // A descriptor that does not pin the feed id does not satisfy finish().
        let mut adapter = connected_adapter(PythHermesConnection::Stream);
        adapter
            .ingest(&record(
                2, 1, RecordKind::OutboundText,
                Some(r#"{"method":"GET","url":"https://hermes.pyth.network/v2/updates/price/stream?ids[]=deadbeef&parsed=true&encoding=hex"}"#),
            ))
            .expect("outbound");
        assert!(adapter.finish().is_err(), "a wrong id does not pin");
    }

    // --- T-N18: ChecksumMismatch and AwaitingSnapshot are never emitted.
    #[test]
    fn checksum_and_snapshot_notices_are_never_emitted() {
        let mut adapter = connected_adapter(PythHermesConnection::Stream);
        let mut notices = Vec::new();
        for (ordinal, block) in [
            (2, sse_block(&valid(100, PUBLISH_SECONDS, PUBLISH_SECONDS))),
            (3, ": keepalive".to_owned()),
            (4, r#"{"hello":"world"}"#.to_owned()),
            (
                5,
                sse_block(&valid(101, PUBLISH_SECONDS + 1, PUBLISH_SECONDS + 5)),
            ),
        ] {
            let outcome = adapter
                .ingest(&record(ordinal, 1, RecordKind::InboundText, Some(&block)))
                .expect("operational");
            notices.extend(outcome.notices);
        }
        assert!(
            notices
                .iter()
                .all(|notice| notice.code != FeedNoticeCode::ChecksumMismatch)
        );
        assert!(
            notices
                .iter()
                .all(|notice| notice.code != FeedNoticeCode::AwaitingSnapshot)
        );
    }

    // --- The REST anchor stamps update_kind: "latest".
    #[test]
    fn the_rest_anchor_stamps_update_kind_latest() {
        let mut adapter = new_adapter(PythHermesConnection::Latest);
        adapter.ingest(&connected(1, 1)).expect("connects");
        // The REST body is the bare envelope, not SSE-framed.
        let outcome = adapter
            .ingest(&record(
                2,
                1,
                RecordKind::InboundText,
                Some(&valid(100, PUBLISH_SECONDS, PUBLISH_SECONDS)),
            ))
            .expect("valid");
        assert_eq!(outcome.events[0].values["update_kind"], json!("latest"));
    }

    // --- The emitted value set equals the definition's field set, both ways.
    #[test]
    fn emitted_values_match_the_source_definition_fields() {
        #[derive(serde::Deserialize)]
        struct Definition {
            fields: Vec<Field>,
            event_time_field: String,
        }
        #[derive(serde::Deserialize)]
        struct Field {
            name: String,
        }
        let definition: Definition =
            serde_json::from_str(super::SOURCE_DEFINITION).expect("definition parses");
        let declared: BTreeSet<String> = definition
            .fields
            .iter()
            .map(|field| field.name.clone())
            .collect();
        assert!(
            declared.contains(&definition.event_time_field),
            "event_time_field names a declared field"
        );

        let mut adapter = connected_adapter(PythHermesConnection::Stream);
        let outcome = adapter
            .ingest(&inbound_stream(
                2,
                &valid(100, PUBLISH_SECONDS, PUBLISH_SECONDS),
            ))
            .expect("valid");
        let emitted: BTreeSet<String> = outcome.events[0].values.keys().cloned().collect();
        assert_eq!(emitted, declared, "emitted keys must equal declared fields");
        // The stream row stamps update_kind: "stream".
        assert_eq!(outcome.events[0].values["update_kind"], json!("stream"));
        assert_eq!(outcome.events[0].source_id, super::SOURCE_ID);
    }

    // --- Determinism (adapter-level T-R1): the same records produce byte-identical
    // events twice. The checkpoint-restart variants (T-R2/T-R3) are lane-level and
    // land with the deferred oracle-lane wiring.
    #[test]
    fn replay_is_byte_identical_across_two_runs() {
        let records = [
            connected(1, 1),
            inbound_stream(2, &valid(100, PUBLISH_SECONDS, PUBLISH_SECONDS)),
            inbound_stream(3, &valid(101, PUBLISH_SECONDS + 1, PUBLISH_SECONDS)),
            record(4, 1, RecordKind::Disconnected, None),
        ];
        let render = || {
            let mut adapter = new_adapter(PythHermesConnection::Stream);
            let mut lines = Vec::new();
            for record in &records {
                let outcome: IngestOutcome = adapter.ingest(record).expect("operational");
                for event in outcome.events {
                    lines.push(serde_json::to_string(&event).expect("serializes"));
                }
            }
            lines.join("\n")
        };
        assert_eq!(render(), render());
    }

    // --- Direct decode fuzz-adjacent sanity: a hostile envelope is a classified
    // reject, never a panic.
    #[test]
    fn decode_rejects_hostile_envelopes_without_panicking() {
        for hostile in [
            "",
            "{",
            "not json",
            r#"{"parsed":[]}"#,
            r#"{"binary":{}}"#,
            r#"{"binary":{"encoding":"base64","data":[]},"parsed":[]}"#,
        ] {
            let _ = decode_price_update(hostile, RECEIVED_AT);
        }
    }

    // --- Observed-capture proof (ruling R1). These payloads are verbatim
    // `data:` lines from the authenticated Tokyo capture
    // pyth-hermes-stream-r1-20260904T012850Z.jsonl (ordinals 3, 12 and 13),
    // driven through the same adapter the live runner uses. They close the
    // "documented, not observed" status of the Hermes contract: a real frame
    // parses under `deny_unknown_fields` to a ready-quality event, and a real
    // `prev_publish_time` chain break raises exactly one `Gap` and still emits.
    const OBSERVED_FRAME_FIRST: &str = r#"data: {"binary":{"data":["504e415501000000012401000000000301cf6082d7c563b53a62cbb55eaff3cdac78ee774d09d4b92d713f158e1a27e061411502ae36d28d1dcfbc8dd292852dec9a4d225b4304d8f1d32485eae97b9cb80002a642b0aef0342355d9b929a99a6636557d0033d836f0998699734c2d84c8807c3b22f84c5b8de65f77aea8e6291f733aade0774012798e6c49cd791d517499ec010428592f52c36ac670917be260c9e3c121addde651f2ff8367ed148e93c36aea8645a69ea46d1c9d186a2d9ef2e185ab8f6d43cb45789a09b6d0b73fb683fadc35016a9a1ed300000000001a507974686e6574507974686e6574507974686e6574507974686e65745079746800000008540a687c00415557560000000008540a687c0000000040e3eced6700225e027bbb256b7b6a57ac34a91a01005500e62df6c8b4a85fe1a67db44dc12de5db330f7ac66b72dc658afedf0f4a415b430000075b3dc159dd0000000024319fcafffffff8000000006a9a1ed3000000006a9a1ed2000007606c5f26b0000000003084aef80c3f2370d0f03513bc1dd7424a6814a382c270278734e42bbd00ece42bd4f41aaf9f396ee602b5591be6138761b44be1bc4faf1dfd8328e1bbcecbb942b1882911cbb3d55830402d44b4b173d64b2eee501d511e2a41f837ed363dbb7db6d02f22e147536d48df0ce421a50f2296e1c1d1e50a96ab68a41b3e1c7484e37195a625ff8eb6674e7c7d36f8a4e7e9935c8ce9309f20bac794b2eb63166230691b971d0224786ace52b86631d49be3a3126fad861f02c4c3c25f4f1c034f0ce6b5842e3c9078913f007877faaf20e54ee7e949c24271dc6ab02bea7a1938dcbb98ec85aef73d1b94efdfe659b4f9b430c425e1"],"encoding":"hex"},"parsed":[{"ema_price":{"conf":"814001912","expo":-8,"price":"8110716430000","publish_time":1788485331},"id":"e62df6c8b4a85fe1a67db44dc12de5db330f7ac66b72dc658afedf0f4a415b43","metadata":{"prev_publish_time":1788485330,"proof_available_time":1788485331,"slot":35769706620},"price":{"conf":"607231946","expo":-8,"price":"8088459499997","publish_time":1788485331}}]}"#;
    const OBSERVED_FRAME_GAP_A: &str = r#"data: {"binary":{"data":["504e415501000000012401000000000301ff1aba4aded3d99fe0c4b9d53d9c8794743b429aed80827066e56d7b78dde5d4081af1b1aa4ced4f52109ff2bd1c2a0ebab2b74040dc6ad2166a113f644cd891010269e1bffa0630a150c72bad047de58ef448618799cdd058253c211df3fff871a43315d46b95aa73a8d4fbc21a65463a47fc4e5e82e71cdd44e7253471c81b453e010393a19b4e1e3302f58096214d2fb64f8d6578deba266fbc392c59ae19b9dcb0d131dba6c08938993a49f495681c0fca78a4afc3c6155fd75f5c52527db088549a006a9a1edc00000000001a507974686e6574507974686e6574507974686e6574507974686e65745079746800000008540a693000415557560000000008540a693000000000c362a5112e0c630c1d2ccc1f2677285d464febae01005500e62df6c8b4a85fe1a67db44dc12de5db330f7ac66b72dc658afedf0f4a415b430000075b3d83a5140000000023f3eb01fffffff8000000006a9a1edc000000006a9a1edb0000076067d09de000000000307aafc20c82deabd823cda5e8654a0e097d6e40a0f788e6d6a2960540d28f31b1977433292b0cb1aca13e39930a45771fb6a7d6789be8396064ef93aa21ac2fe2371b705461d44f7c0620ef164b954f0618713b29e08ed26569b96b29c66fba14ef22b3fc6a2f10d43ecdad2abc9e31db4fb2a1534d01413f116fbda84280bed46cea7bdcf6997a76c9ed4ba91078e911c175152337e5582e7407b11b1cf319dea6a9726127553d18a6af21bdd9d000a05063875c07ce7264bee70eb564170b0855b93e57346a2975de8fa43aa21e48c56d362d23487c9ba259afbe836f1c250a78da57b4d3478290ce2d7273999479bf6e7fafbe"],"encoding":"hex"},"parsed":[{"ema_price":{"conf":"813346754","expo":-8,"price":"8110639980000","publish_time":1788485340},"id":"e62df6c8b4a85fe1a67db44dc12de5db330f7ac66b72dc658afedf0f4a415b43","metadata":{"prev_publish_time":1788485339,"proof_available_time":1788485340,"slot":35769706800},"price":{"conf":"603187969","expo":-8,"price":"8088455456020","publish_time":1788485340}}]}"#;
    const OBSERVED_FRAME_GAP_B: &str = r#"data: {"binary":{"data":["504e415501000000012401000000000301081a120eb28f6c6ada0482f80daca6209ebcc212bbe16f73974d380d27d3db591d8282d4a10eadd7d685e471e35db42a41cb59f5e62beffdd5f21a6e53f762f50102797ddf8a261da34c77494d20eac6620e90f5ebebe769688f20b6c46b5db8ba073feb375c93f891bb10a430d1b8682f32a578c84282203f8b4dfb3e58a1447d8d0104538bbc748159ed9af25b878b1a6f9ff20a306939d65343e0729e58c6553e1d0e55789e0bc077e834f0e7363a65b0a07e6292e704190519e907cfd639102a8bc1016a9a1ede00000000001a507974686e6574507974686e6574507974686e6574507974686e65745079746800000008540a695800415557560000000008540a6958000000008b62b9f9021004d5e88494aa7ccec311152c186b01005500e62df6c8b4a85fe1a67db44dc12de5db330f7ac66b72dc658afedf0f4a415b430000075b39abfe69000000002067cf82fffffff8000000006a9a1ede000000006a9a1edd0000076066c3258000000000307813fa0cfbbe61c84a66ca562f13366885857bbe1a1a39fb26aebdd25965c00cdc4d1cbf65910b3f924e65b25f5bd94a0ef98044fb35fb8af6acbdddd7af26d3ea7c83cbbe6aa2dd93ea236ed0de3a3fca61d29267a3853fd2942a7d79c7fc087765bb07cbaf01c44f832b3b4e1353bef7760b94fcf5f565467b539ba173787c96d22c65c8f59acd321d2bd550dcefaec5fe89b53308860c53cc413ca078ba446624aec3608d6114f709cd5e067d8548ad305ffe9c2a4db46a391fc6c0e8a7f92f1b0ddc8e60de4756967327eb2498bbcbeabc846d65f50dac60a80425226530b99a455d17b2d20782e2958a7375cb802e454904"],"encoding":"hex"},"parsed":[{"ema_price":{"conf":"813175802","expo":-8,"price":"8110622320000","publish_time":1788485342},"id":"e62df6c8b4a85fe1a67db44dc12de5db330f7ac66b72dc658afedf0f4a415b43","metadata":{"prev_publish_time":1788485341,"proof_available_time":1788485342,"slot":35769706840},"price":{"conf":"543674242","expo":-8,"price":"8088390991465","publish_time":1788485342}}]}"#;

    fn inbound_observed(ordinal: u64, received_at: &str, payload: &str) -> RawRecord {
        RawRecord {
            schema_version: 1,
            provider: crate::Provider::PythHermes,
            stream: CaptureStream::PythHermesStream,
            connection_epoch: 1,
            ordinal,
            received_at: received_at.to_owned(),
            capture_elapsed_ns: None,
            connect_started_elapsed_ns: None,
            kind: RecordKind::InboundText,
            payload: Some(payload.to_owned()),
        }
    }

    #[test]
    fn observed_capture_frames_emit_ready_values() {
        let mut adapter = connected_adapter(PythHermesConnection::Stream);
        let outcome = adapter
            .ingest(&inbound_observed(
                3,
                "2026-09-04T01:28:51.065642Z",
                OBSERVED_FRAME_FIRST,
            ))
            .expect("observed frame ingests");
        assert_eq!(outcome.events.len(), 1, "one event per observed frame");
        let values = &outcome.events[0].values;
        // Exact scaled values from the real frame (expo -8), confidence and EMA
        // present, slot and event time carried through -> ready-quality (V6).
        assert_eq!(values["price"].as_str().unwrap(), "80884.59499997");
        assert_eq!(values["conf"].as_str().unwrap(), "6.07231946");
        assert_eq!(values["ema_price"].as_str().unwrap(), "81107.16430000");
        assert_eq!(values["ema_conf"].as_str().unwrap(), "8.14001912");
        assert_eq!(values["expo"].as_i64().unwrap(), -8);
        assert_eq!(values["slot"].as_u64().unwrap(), 35_769_706_620);
        assert_eq!(values["update_kind"].as_str().unwrap(), "stream");
        assert_eq!(values["feed_id"].as_str().unwrap(), FEED_ID);
        assert_eq!(
            values["event_time"].as_str().unwrap(),
            "2026-09-04T01:28:51.000Z"
        );
        // A real frame is neither a shape refusal (V1-V4) nor a value refusal
        // (V5-V13): no such notice is raised.
        assert!(
            outcome.notices.iter().all(|notice| {
                notice.code != FeedNoticeCode::UnsupportedMessage
                    && notice.code != FeedNoticeCode::StateInconsistent
            }),
            "a real observed frame is not refused"
        );
    }

    #[test]
    fn observed_capture_chain_break_raises_one_gap_and_still_emits() {
        let mut adapter = connected_adapter(PythHermesConnection::Stream);
        let earlier = adapter
            .ingest(&inbound_observed(
                12,
                "2026-09-04T01:29:00.064993Z",
                OBSERVED_FRAME_GAP_A,
            ))
            .expect("frame A ingests");
        assert_eq!(earlier.events.len(), 1, "frame A emits");
        assert!(
            !earlier
                .notices
                .iter()
                .any(|n| n.code == FeedNoticeCode::Gap),
            "frame A is the first of the epoch, no gap"
        );
        // Frame B advances slot and publish time but its prev_publish_time does
        // not chain to A's publish time (a real 1 s aggregate was skipped in the
        // 1 Hz stream), so it emits and raises exactly one informational Gap.
        let gap = adapter
            .ingest(&inbound_observed(
                13,
                "2026-09-04T01:29:02.058170Z",
                OBSERVED_FRAME_GAP_B,
            ))
            .expect("frame B ingests");
        assert_eq!(gap.events.len(), 1, "the gap frame still emits");
        assert_eq!(
            gap.notices
                .iter()
                .filter(|n| n.code == FeedNoticeCode::Gap)
                .count(),
            1,
            "a real chain break raises exactly one Gap"
        );
    }
}
