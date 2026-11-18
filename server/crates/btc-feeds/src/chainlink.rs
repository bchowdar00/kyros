//! Chainlink on-chain BTC/USD aggregator adapter: bounded JSON-RPC poll
//! responses normalized into round updates. Adopted as the approved source
//! `chainlink.ethereum.btc-usd.rounds@1.0.0` (A5 phase 1); it feeds the shared
//! `oracle` lane (built as the generic venue lane in A6). The source definition
//! is bound in-crate as [`SOURCE_DEFINITION`] so the emitted field set and the
//! published contract have one authority (see the field-set equality test).

use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::json;
use serde_json::value::RawValue;
use signals_core::SourceEvent;

use crate::adapter::{GuardAction, RecordGuard, validate_publisher_epoch};
use crate::event_window::millis_to_rfc3339;
use crate::{AdapterError, FeedAdapter, FeedNotice, FeedNoticeCode, IngestOutcome, RawRecord};

// --- Compiled on-chain call constants (verified on Ethereum mainnet 2026-08-29:
// description "BTC / USD", decimals 8, live answer 77667.797, phaseId 7). These
// are consumed by both the capture poll loop and the live poll driver so they
// share one source of truth for the endpoint list, target, and cadence. Public
// reads are keyless; the endpoint list fails over on any fault (ruling E29).

/// BTC/USD `EACAggregatorProxy` address on Ethereum mainnet. Always read the
/// PROXY, never a hardcoded aggregator: the underlying aggregator implementation
/// self-destructed in 2021 (phaseId is now at 7) and is swapped on upgrades,
/// while the proxy `roundId` stays monotonic across those swaps.
pub(crate) const PROXY_ADDRESS: &str = "0xF4030086522a5bEEa4988F8cA5B36dbC97BeE88c";

/// 4-byte selector of `latestRoundData()` (keccak256("latestRoundData()")[..4]).
pub(crate) const LATEST_ROUND_DATA_SELECTOR: &str = "0xfeaf968c";

/// Poll cadence. The feed's heartbeat is a *documented* one hour and its
/// deviation threshold a *documented* 0.5% (Chainlink's published feed
/// parameters, not independently measured on chain; the 2026-08-29 probe read
/// description/decimals/answer/phaseId only). The ~1 h R1 sample corroborates
/// the heartbeat (one round transition, `updated_at` +3624 s, answer +0.39% —
/// below the deviation threshold) but a single interval cannot confirm it. One
/// poll a minute over-samples the feed on purpose; the adapter collapses the
/// resulting duplicate rounds.
pub(crate) const POLL_CADENCE: Duration = Duration::from_secs(60);

/// Per-request HTTP timeout for one `eth_call` poll.
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Fixed-point scale of the BTC/USD feed's `answer` (its on-chain `decimals()`).
/// A raw answer of 7766779700000 renders as "77667.79700000".
const ANSWER_DECIMALS: usize = 8;

const INSTRUMENT: &str = "BTC-USD";
const SOURCE_ID: &str = "chainlink.ethereum.btc-usd.rounds";
const SOURCE_VERSION: &str = "1.0.0";

/// Timing/grouping series name for this feed, shared by the normalization and
/// latency analyzers (there is no per-message topic to group by; the feed has a
/// single logical channel).
pub(crate) const ROUNDS_SERIES: &str = "chainlink_aggregator:latest-round-data";

/// One static ABI word.
const WORD_BYTES: usize = 32;
/// `latestRoundData()` returns five static words (no dynamic types, so the
/// encoding is a flat 5x32-byte concatenation with no offset/length headers).
const RESULT_WORDS: usize = 5;
const RESULT_HEX_CHARS: usize = RESULT_WORDS * WORD_BYTES * 2;

/// Published source contract for this adapter, bound in-crate so the emitted
/// `values` map and the definition `fields` array have one authority (the
/// field-set equality test asserts they agree, both directions).
pub const SOURCE_DEFINITION: &str =
    include_str!("../../../definitions/btc/sources/chainlink.ethereum.btc-usd.rounds@1.0.0.json");

/// Upper bound on one poll response body, checked before parsing so the live
/// poll driver and replay reject an oversize body by the same rule. Sized
/// against the R1 capture: a valid reply is ~360 bytes, so 64 KiB is >100x
/// headroom while still refusing a runaway or wrong-endpoint body.
pub(crate) const MAX_RESPONSE_BYTES: usize = 65_536;

/// Environment variable naming the ordered, comma-separated RPC endpoint list
/// used by the live poll driver (R9: public multi-endpoint failover).
pub const RPC_ENDPOINTS_ENV: &str = "CHAINLINK_RPC_ENDPOINTS";

/// Default ordered endpoint list when `CHAINLINK_RPC_ENDPOINTS` is unset, in
/// failover order (ruling E29, probed from the Tokyo box 2026-09-03): all three
/// are keyless public HTTPS JSON-RPC endpoints that answered a real
/// `latestRoundData` call. `cloudflare-eth.com` was removed (it returns HTTP 200
/// with a JSON-RPC "Internal error").
pub const DEFAULT_RPC_ENDPOINTS: &[&str] = &[
    "https://ethereum-rpc.publicnode.com",
    "https://eth.drpc.org",
    "https://eth.merkle.io",
];

/// The primary (first) default endpoint, for the defensive stream-endpoint
/// lookup only; the poll path always uses the full failover list.
pub(crate) const fn primary_rpc_endpoint() -> &'static str {
    DEFAULT_RPC_ENDPOINTS[0]
}

/// 4-byte selector of `decimals()` (keccak256("decimals()")[..4]); its reply
/// must decode to 8 or the driver refuses to start (§4.5 fail-closed scale check).
pub(crate) const DECIMALS_SELECTOR: &str = "0x313ce567";

/// 4-byte selector of `description()` (keccak256("description()")[..4]); its
/// reply must decode to "BTC / USD" or the driver refuses to start (identity check).
pub(crate) const DESCRIPTION_SELECTOR: &str = "0x7284e416";

/// Expected `decimals()` value; a change would silently misplace the decimal
/// point in every `answer`, so a mismatch is fail-closed at start-up.
pub(crate) const EXPECTED_DECIMALS: u8 = 8;

/// Expected `description()` value; a mismatch means a wrong proxy address.
pub(crate) const EXPECTED_DESCRIPTION: &str = "BTC / USD";

/// Parse the ordered RPC endpoint list from the raw `CHAINLINK_RPC_ENDPOINTS`
/// value. Enforces R9's policy: at least two entries, every entry `https://`.
/// Returns the ordered list, or a one-line reason the caller reports as a
/// configuration-class refusal.
pub fn parse_rpc_endpoints(raw: &str) -> Result<Vec<String>, String> {
    let endpoints: Vec<String> = raw
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(str::to_owned)
        .collect();
    validate_endpoint_list(&endpoints).map_err(|reason| format!("{RPC_ENDPOINTS_ENV} {reason}"))?;
    Ok(endpoints)
}

/// The one rule set for an endpoint list, whichever way it arrived (the
/// environment variable, or `ChainlinkPollOptions` built in code): at least two
/// entries, each `https://` with a host, and pairwise distinct after
/// normalisation — so `https://a.example` and `https://a.example/` count as the
/// same endpoint and do not satisfy R9's two-endpoint failover.
pub(crate) fn validate_endpoint_list(endpoints: &[String]) -> Result<(), String> {
    if endpoints.len() < 2 {
        return Err(format!(
            "must list at least two endpoints (R9 multi-endpoint failover); got {}",
            endpoints.len()
        ));
    }
    let mut distinct = std::collections::BTreeSet::new();
    for entry in endpoints {
        let Some(rest) = entry.strip_prefix("https://") else {
            return Err(format!(
                "entries must be https:// with a host; got {}",
                redact_endpoint(entry)
            ));
        };
        let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
        let host = authority.rsplit('@').next().unwrap_or(authority);
        if host.is_empty() {
            return Err(format!(
                "entries must be https:// with a host; got {}",
                redact_endpoint(entry)
            ));
        }
        // Normalised identity: lower-cased host, path without its trailing
        // slash, query and fragment kept as written.
        let path = &rest[authority.len()..];
        let key = format!(
            "{}{}",
            host.to_ascii_lowercase(),
            path.strip_suffix('/').unwrap_or(path)
        );
        if !distinct.insert(key) {
            return Err(format!(
                "lists {} more than once; endpoints must be distinct",
                redact_endpoint(entry)
            ));
        }
    }
    Ok(())
}

/// Redact an endpoint URL to `scheme://host` for notices and error strings
/// (§4.4): drop any path, query, fragment, or `userinfo@` credentials so a
/// full URL never lands in a message or a log line.
pub(crate) fn redact_endpoint(endpoint: &str) -> String {
    match endpoint.split_once("://") {
        Some((scheme, rest)) => {
            let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
            let host = authority.rsplit('@').next().unwrap_or(authority);
            format!("{scheme}://{host}")
        }
        None => {
            let authority = endpoint.split(['/', '?', '#']).next().unwrap_or(endpoint);
            authority.rsplit('@').next().unwrap_or(authority).to_owned()
        }
    }
}

/// The configured endpoint list, taking `CHAINLINK_RPC_ENDPOINTS` when set and
/// non-empty, otherwise the compiled [`DEFAULT_RPC_ENDPOINTS`] pair.
pub fn configured_rpc_endpoints() -> Result<Vec<String>, String> {
    match std::env::var(RPC_ENDPOINTS_ENV) {
        Ok(raw) if !raw.trim().is_empty() => parse_rpc_endpoints(&raw),
        _ => Ok(DEFAULT_RPC_ENDPOINTS
            .iter()
            .map(|e| (*e).to_owned())
            .collect()),
    }
}

/// Outcome of a start-up target check (§4.5), classified so the driver can tell
/// a **transient** fault (rotate to the next endpoint and retry — never fatal)
/// from a decoded **mismatch** (a wrong proxy or a changed contract — fatal,
/// exit-78 class). Anything that does not decode to a concrete value — a
/// transport/HTTP failure (the reply never arrives), a JSON-RPC error object, a
/// wrong-length or non-hex result, a malformed ABI encoding — is transient.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetCheck {
    /// Decoded and equals the expected value.
    Verified,
    /// Could not be decoded to a value; rotate and retry.
    Transient(String),
    /// Decoded to a concrete value that differs from the expected one; fatal.
    Mismatch(String),
}

/// Decode a `decimals()` reply (one uint8 word) and classify it against the
/// compiled [`EXPECTED_DECIMALS`].
pub fn verify_decimals_reply(payload: &str) -> TargetCheck {
    let value = match single_uint_result(payload) {
        Ok(value) => value,
        Err(reason) => return TargetCheck::Transient(format!("decimals() undecodable: {reason}")),
    };
    match u8::try_from(value) {
        Ok(decimals) if decimals == EXPECTED_DECIMALS => TargetCheck::Verified,
        Ok(decimals) => TargetCheck::Mismatch(format!(
            "decimals() returned {decimals}, expected {EXPECTED_DECIMALS}"
        )),
        Err(_) => TargetCheck::Mismatch(format!(
            "decimals() returned {value}, expected {EXPECTED_DECIMALS}"
        )),
    }
}

/// Decode a `description()` reply (an ABI-encoded dynamic string) and classify
/// it against the compiled [`EXPECTED_DESCRIPTION`].
pub fn verify_description_reply(payload: &str) -> TargetCheck {
    match abi_string_result(payload) {
        Ok(text) if text == EXPECTED_DESCRIPTION => TargetCheck::Verified,
        Ok(text) => TargetCheck::Mismatch(format!(
            "description() returned {text:?}, expected {EXPECTED_DESCRIPTION:?}"
        )),
        Err(reason) => TargetCheck::Transient(format!("description() undecodable: {reason}")),
    }
}

/// Extract the `result` hex of a JSON-RPC reply and decode a single 32-byte
/// word as an unsigned integer (used for `decimals()`).
fn single_uint_result(payload: &str) -> Result<u128, String> {
    let hex = result_hex(payload)?;
    if hex.len() != WORD_BYTES * 2 {
        return Err(format!(
            "expected one 32-byte word, got {} hex chars",
            hex.len()
        ));
    }
    let bytes = hex_to_bytes(&hex)?;
    let mut word = [0u8; WORD_BYTES];
    word.copy_from_slice(&bytes);
    word_to_u128(&word, WORD_BYTES, "value").map_err(|reject| reject.detail)
}

/// Decode an ABI-encoded dynamic `string` return (offset, length, data words).
fn abi_string_result(payload: &str) -> Result<String, String> {
    let hex = result_hex(payload)?;
    let bytes = hex_to_bytes(&hex)?;
    // [0..32] offset (expected 0x20), [offset..offset+32] length, then the
    // UTF-8 bytes. Every offset/length add is checked so a hostile word near
    // usize::MAX (e.g. u64::MAX-16) fails closed instead of wrapping.
    if bytes.len() < 2 * WORD_BYTES {
        return Err(format!("string return too short ({} bytes)", bytes.len()));
    }
    let offset = be_word_to_usize(&bytes[0..WORD_BYTES])?;
    let length_end = offset
        .checked_add(WORD_BYTES)
        .ok_or_else(|| "string offset overflows usize".to_owned())?;
    if length_end > bytes.len() {
        return Err("string offset points past the payload".to_owned());
    }
    let length = be_word_to_usize(&bytes[offset..length_end])?;
    let start = length_end;
    let end = start
        .checked_add(length)
        .ok_or_else(|| "string length overflows usize".to_owned())?;
    if end > bytes.len() {
        return Err("string length runs past the payload".to_owned());
    }
    String::from_utf8(bytes[start..end].to_vec()).map_err(|_| "string is not UTF-8".to_owned())
}

fn be_word_to_usize(word: &[u8]) -> Result<usize, String> {
    // High bytes beyond usize must be zero; the ABI words we read here are small.
    let split = word.len().saturating_sub(8);
    if word[..split].iter().any(|byte| *byte != 0) {
        return Err("ABI word exceeds usize width".to_owned());
    }
    let mut value: usize = 0;
    for &byte in &word[split..] {
        value = (value << 8) | usize::from(byte);
    }
    Ok(value)
}

fn result_hex(payload: &str) -> Result<String, String> {
    let response: JsonRpcResponse =
        serde_json::from_str(payload).map_err(|error| error.to_string())?;
    if response.error.is_some() {
        return Err("JSON-RPC error reply".to_owned());
    }
    let result = response
        .result
        .ok_or_else(|| "reply carries neither result nor error".to_owned())?;
    let hex = result
        .strip_prefix("0x")
        .or_else(|| result.strip_prefix("0X"))
        .unwrap_or(&result);
    Ok(hex.to_owned())
}

fn hex_to_bytes(hex: &str) -> Result<Vec<u8>, String> {
    if !hex.len().is_multiple_of(2) {
        return Err("odd hex length".to_owned());
    }
    let bytes = hex.as_bytes();
    let mut out = Vec::with_capacity(hex.len() / 2);
    for pair in 0..(hex.len() / 2) {
        let high = hex_nibble(bytes[pair * 2]).ok_or_else(|| "non-hex digit".to_owned())?;
        let low = hex_nibble(bytes[pair * 2 + 1]).ok_or_else(|| "non-hex digit".to_owned())?;
        out.push((high << 4) | low);
    }
    Ok(out)
}

/// Build the exact JSON-RPC `eth_call` request body for one poll. Pure so the
/// request shape is unit-tested without a live socket. The struct field and
/// tuple order fix the serialized key order to
/// `{"jsonrpc","method","params":[{"to","data"},"latest"],"id"}`.
///
/// `pub` so the request wire shape has one authority: the capture poll loop and
/// the replay-contract integration test both build it here rather than repeating
/// a hand-written literal.
pub fn request_body(id: u64) -> String {
    eth_call_body(LATEST_ROUND_DATA_SELECTOR, id)
}

/// Build the exact JSON-RPC `eth_call` body for an arbitrary 4-byte selector
/// against the proxy at the latest block. Shared by the round poll and the
/// start-up `decimals()` / `description()` verification calls so every request
/// has one wire authority.
pub fn eth_call_body(data: &str, id: u64) -> String {
    #[derive(Serialize)]
    struct EthCall<'a> {
        to: &'a str,
        data: &'a str,
    }
    #[derive(Serialize)]
    struct Request<'a> {
        jsonrpc: &'a str,
        method: &'a str,
        params: (EthCall<'a>, &'a str),
        id: u64,
    }
    let request = Request {
        jsonrpc: "2.0",
        method: "eth_call",
        params: (
            EthCall {
                to: PROXY_ADDRESS,
                data,
            },
            "latest",
        ),
        id,
    };
    serde_json::to_string(&request).expect("the compiled request struct always serializes")
}

/// True when an `OutboundText` `eth_call` body targets `decimals()` or
/// `description()` rather than `latestRoundData()` — i.e. a start-up target
/// check whose recorded reply must be skipped, not decoded as a round (§4.5).
fn outbound_is_target_call(payload: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(payload) else {
        return false;
    };
    let data = value
        .get("params")
        .and_then(|params| params.get(0))
        .and_then(|call| call.get("data"))
        .and_then(|data| data.as_str());
    matches!(data, Some(DECIMALS_SELECTOR) | Some(DESCRIPTION_SELECTOR))
}

/// Deterministic Chainlink `AggregatorV3Interface` poll-response parser and
/// round-ordering state. One adapter serves the single BTC/USD proxy stream.
///
/// Unlike a push stream, this rides a low-frequency HTTP poll: the same
/// `roundId` recurs on every poll until the on-chain feed next updates, so an
/// unchanged round is the expected steady state (see `round_event`).
///
/// The proxy `roundId` is global chain state and stays monotonic across a poll
/// reconnect (the "session" is logical; the chain does not restart), so the
/// round cursor **persists across `Connected`/`Disconnected` for the life of one
/// adapter** — i.e. one run. It resets only when a fresh adapter is constructed
/// (a new run), or when A6 restores it from a checkpoint. This is what lets the
/// failover reconnect the live driver performs still catch a lagging node that
/// replays an older round after a rotation.
pub struct ChainlinkAdapter {
    publisher_epoch: String,
    guard: RecordGuard,
    next_round_sequence: u64,
    /// Last emitted proxy `roundId` for the life of this adapter (one run).
    /// `None` before the first round.
    last_round_id: Option<u128>,
    /// `updated_at` of the last emitted round, so `event_time` never moves
    /// backwards even if a lagging node serves a higher `roundId` with an older
    /// timestamp. Cleared only with `last_round_id`.
    last_updated_at: Option<String>,
    /// Set while the previous `OutboundText` was a start-up target call
    /// (`decimals()`/`description()`), so its recorded reply is skipped instead
    /// of decoded as a round (§4.5): the call is journal evidence, not an event.
    expect_target_reply: bool,
}

impl ChainlinkAdapter {
    /// Construct a Chainlink aggregator adapter.
    ///
    /// `publisher_epoch` is copied verbatim into every emitted event; it must be
    /// non-blank and at most 128 bytes, or the call returns
    /// [`AdapterError::InvalidRecord`].
    pub fn new(publisher_epoch: impl Into<String>) -> Result<Self, AdapterError> {
        let publisher_epoch = publisher_epoch.into();
        validate_publisher_epoch(&publisher_epoch)?;
        Ok(Self {
            publisher_epoch,
            guard: RecordGuard::new(crate::Provider::ChainlinkAggregator),
            next_round_sequence: 0,
            last_round_id: None,
            last_updated_at: None,
            expect_target_reply: false,
        })
    }

    fn ingest_payload(
        &mut self,
        record: &RawRecord,
        payload: &str,
    ) -> Result<IngestOutcome, AdapterError> {
        // Polls are independent and the public endpoint load-balances over
        // heterogeneous backends, so any failure of ONE poll response is
        // transient, never session-fatal: it is retained as an error-class notice
        // and skipped with the cursor untouched, instead of aborting replay of the
        // whole session. Response-content failures split into an envelope-shape
        // class and a value-validation class, each carrying its own notice code.
        // (Genuine journal-structure failures -- missing payload, bad ordinal --
        // remain fatal at the shared record-guard layer above; this handles poll
        // RESPONSE content only.) Validation still runs BEFORE any cursor logic.
        match parse_round_response(payload) {
            Ok(RoundResponse::ProviderError(detail)) => {
                // A JSON-RPC error (node error, rate limit, transient node state)
                // is transient: note it and emit nothing.
                Ok(IngestOutcome::notice(FeedNotice::info(
                    FeedNoticeCode::StateInconsistent,
                    detail,
                )))
            }
            Ok(RoundResponse::Round(round)) => self.round_event(record, round),
            Err(reject) => Ok(IngestOutcome::notice(FeedNotice::info(
                reject.code,
                reject.detail,
            ))),
        }
    }

    fn round_event(
        &mut self,
        record: &RawRecord,
        round: DecodedRound,
    ) -> Result<IngestOutcome, AdapterError> {
        // A higher `roundId` whose `updated_at` moves backwards is a lagging-node
        // anomaly: skip it as out-of-order so `event_time` never regresses.
        if let (Some(previous_round), Some(previous_time)) =
            (self.last_round_id, self.last_updated_at.as_deref())
            && round.round_id > previous_round
            && round.updated_at.as_str() < previous_time
        {
            return Ok(IngestOutcome::notice(FeedNotice::info(
                FeedNoticeCode::OutOfOrder,
                format!(
                    "Chainlink round {} has updated_at {} before the last accepted {previous_time}; skipped",
                    round.round_id, round.updated_at
                ),
            )));
        }
        match self.last_round_id {
            Some(previous) if round.round_id == previous => {
                // Polling faster than the feed updates makes an unchanged
                // `roundId` the expected steady state, so a per-poll notice would
                // be pure noise: emit nothing, no notice. This deliberately
                // differs from the push-stream `Duplicate` vocabulary, where a
                // repeated id is a redelivery worth flagging; here it is the norm.
                Ok(IngestOutcome::default())
            }
            Some(previous) if round.round_id < previous => {
                // The proxy `roundId` is documented monotonic across aggregator
                // upgrades, so a decrease is a provider anomaly. Note it, emit
                // nothing, and leave the cursor unchanged.
                Ok(IngestOutcome::notice(FeedNotice::info(
                    FeedNoticeCode::OutOfOrder,
                    format!(
                        "out-of-order Chainlink round {} after {previous}; skipped",
                        round.round_id
                    ),
                )))
            }
            previous => {
                // First round of the epoch, or an advance. Emit one round event
                // and advance the cursor.
                let mut notices = Vec::new();
                if let Some(previous) = previous {
                    // The proxy `roundId` stays monotonic even when the aggregator
                    // implementation is upgraded (phaseId increments). Naming the
                    // upgrade helps the adopting consumer correlate the phase break.
                    let previous_phase = previous >> 64;
                    if u128::from(round.phase_id) > previous_phase {
                        notices.push(FeedNotice::info(
                            FeedNoticeCode::VenueStatus,
                            format!(
                                "Chainlink aggregator upgraded: phase {previous_phase} -> {}",
                                round.phase_id
                            ),
                        ));
                    }
                }
                // Advance the cursor only AFTER the fallible event construction
                // succeeds, so a sequence-exhaustion error never leaves the cursor
                // ahead of the last emitted round.
                let event = self.round_source_event(record, &round)?;
                self.last_round_id = Some(round.round_id);
                self.last_updated_at = Some(round.updated_at.clone());
                Ok(IngestOutcome {
                    events: vec![event],
                    notices,
                })
            }
        }
    }

    fn round_source_event(
        &mut self,
        record: &RawRecord,
        round: &DecodedRound,
    ) -> Result<SourceEvent, AdapterError> {
        let values = BTreeMap::from([
            ("instrument".to_owned(), json!(INSTRUMENT)),
            // `round_id` is a u80 that exceeds u64, so it is carried as a string.
            ("round_id".to_owned(), json!(round.round_id.to_string())),
            ("phase_id".to_owned(), json!(round.phase_id)),
            (
                "aggregator_round_id".to_owned(),
                json!(round.aggregator_round_id),
            ),
            ("answer".to_owned(), json!(round.answer)),
            ("started_at".to_owned(), json!(round.started_at)),
            ("updated_at".to_owned(), json!(round.updated_at)),
            // `answered_in_round` is a deprecated u80 (previously used when an
            // answer could take multiple rounds to compute). It is captured as a
            // string; no staleness logic is applied to it. The adopting consumer
            // owns any `updatedAt`-age staleness policy.
            (
                "answered_in_round".to_owned(),
                json!(round.answered_in_round.to_string()),
            ),
            ("event_time_basis".to_owned(), json!("source")),
            ("event_time".to_owned(), json!(round.updated_at)),
        ]);
        let sequence = self.next_round_sequence;
        self.next_round_sequence = self
            .next_round_sequence
            .checked_add(1)
            .ok_or(AdapterError::SequenceExhausted)?;
        Ok(SourceEvent {
            schema_version: 1,
            source_id: SOURCE_ID.to_owned(),
            source_version: SOURCE_VERSION.to_owned(),
            publisher_epoch: self.publisher_epoch.clone(),
            sequence,
            event_time: round.updated_at.clone(),
            received_at: record.received_at.clone(),
            values,
        })
    }
}

impl FeedAdapter for ChainlinkAdapter {
    fn ingest(&mut self, record: &RawRecord) -> Result<IngestOutcome, AdapterError> {
        match self.guard.observe(record)? {
            GuardAction::Configuration(_) => Err(AdapterError::InvalidRecord(
                "Chainlink captures do not accept configuration records".to_owned(),
            )),
            GuardAction::Connected => {
                // The round cursor is global chain state, so a reconnect (failover
                // or a genuine session restart within one run) does NOT reset it;
                // only a fresh adapter does. Clear any pending target-reply flag.
                self.expect_target_reply = false;
                Ok(IngestOutcome::notice(FeedNotice::info(
                    FeedNoticeCode::Connected,
                    "Chainlink poll session connected; round cursor retained",
                )))
            }
            GuardAction::Disconnected => {
                self.expect_target_reply = false;
                Ok(IngestOutcome::notice(FeedNotice::info(
                    FeedNoticeCode::Disconnected,
                    "Chainlink poll session ended",
                )))
            }
            // The poll request (`eth_call`) is recorded as OutboundText. A start-up
            // `decimals()`/`description()` request marks the next reply to be
            // recorded but skipped (§4.5); a `latestRoundData()` request does not.
            GuardAction::Outbound => {
                self.expect_target_reply = record
                    .payload
                    .as_deref()
                    .is_some_and(outbound_is_target_call);
                Ok(IngestOutcome::default())
            }
            GuardAction::Failed(_) => Err(AdapterError::InvalidRecord(
                "Chainlink captures do not accept failure records".to_owned(),
            )),
            GuardAction::Inbound(payload) => {
                if self.expect_target_reply {
                    // A recorded start-up reply: journal evidence, not a round.
                    self.expect_target_reply = false;
                    return Ok(IngestOutcome::default());
                }
                self.ingest_payload(record, payload)
            }
        }
    }
}

/// One decoded `latestRoundData()` response. Every field is fully validated at
/// decode time (single authority shared by the adapter and the latency analyzer).
pub(crate) struct DecodedRound {
    pub(crate) round_id: u128,
    /// `roundId >> 64`; increments on each aggregator upgrade. Fits a small
    /// integer (u80 minus the 64-bit low half).
    pub(crate) phase_id: u64,
    /// `roundId & 0xFFFF_FFFF_FFFF_FFFF`; the aggregator's own round counter.
    pub(crate) aggregator_round_id: u64,
    /// `answer` rendered as a fixed 8-decimal string.
    pub(crate) answer: String,
    /// `startedAt`, converted from unix seconds to an RFC 3339 timestamp.
    pub(crate) started_at: String,
    /// `updatedAt`, converted from unix seconds to an RFC 3339 timestamp.
    pub(crate) updated_at: String,
    /// Deprecated `answeredInRound` (u80).
    pub(crate) answered_in_round: u128,
}

/// A parsed poll response: either a decoded round or a transient provider error.
pub(crate) enum RoundResponse {
    Round(DecodedRound),
    ProviderError(String),
}

/// A rejected poll response. Both classes are transient (notice + skip, cursor
/// untouched), never session-fatal; the `code` distinguishes the class.
#[derive(Debug)]
pub(crate) struct ResponseReject {
    pub(crate) code: FeedNoticeCode,
    pub(crate) detail: String,
}

/// Envelope-shape rejection: the response is not a well-formed
/// `latestRoundData()` reply (bad JSON, unknown field, non-hex or wrong-length
/// result, or a word wider than its declared ABI width).
fn shape_reject(detail: impl Into<String>) -> ResponseReject {
    ResponseReject {
        code: FeedNoticeCode::UnsupportedMessage,
        detail: detail.into(),
    }
}

/// Value-validation rejection: the response decodes structurally but carries an
/// out-of-contract value (non-positive answer, or a timestamp outside the shared
/// plausibility window).
fn value_reject(detail: impl Into<String>) -> ResponseReject {
    ResponseReject {
        code: FeedNoticeCode::StateInconsistent,
        detail: detail.into(),
    }
}

/// Parse and fully validate one JSON-RPC `eth_call` response. `deny_unknown_fields`
/// keeps the envelope tight; a response carrying `error` instead of `result`
/// yields a transient `ProviderError`, and any content failure yields a
/// classified `ResponseReject` rather than a fatal error.
pub(crate) fn parse_round_response(payload: &str) -> Result<RoundResponse, ResponseReject> {
    // Oversize precondition, checked BEFORE parsing so the live poll driver and
    // replay reject a runaway body by the same rule (a valid reply is ~360 bytes).
    if payload.len() > MAX_RESPONSE_BYTES {
        return Err(shape_reject(format!(
            "Chainlink poll response of {} bytes exceeds MAX_RESPONSE_BYTES ({MAX_RESPONSE_BYTES})",
            payload.len()
        )));
    }
    let response: JsonRpcResponse =
        serde_json::from_str(payload).map_err(|error| shape_reject(error.to_string()))?;
    if let Some(error) = response.error {
        let error: RpcErrorBody =
            serde_json::from_str(error.get()).map_err(|error| shape_reject(error.to_string()))?;
        return Ok(RoundResponse::ProviderError(bounded_detail(
            "Chainlink node returned a JSON-RPC error",
            error.message.as_deref().unwrap_or("no message provided"),
        )));
    }
    let result = response.result.ok_or_else(|| {
        shape_reject("Chainlink JSON-RPC response carries neither result nor error")
    })?;
    Ok(RoundResponse::Round(decode_round(&result)?))
}

fn decode_round(result: &str) -> Result<DecodedRound, ResponseReject> {
    let words = decode_result_words(result)?;
    // Word 0: roundId (u80). Split the proxy id into its phase and aggregator
    // round per the documented encoding `phaseId << 64 | aggregatorRoundId`.
    let round_id = word_to_u128(&words[0], 10, "roundId")?;
    let phase_id =
        u64::try_from(round_id >> 64).map_err(|_| shape_reject("Chainlink phaseId exceeds u64"))?;
    let aggregator_round_id = (round_id & u128::from(u64::MAX)) as u64;
    // Word 1: answer (int256). A USD price feed must report a strictly positive
    // answer; a zero or negative answer is invalid.
    let answer = decode_answer(&words[1])?;
    // Words 2 and 3: startedAt / updatedAt are unix SECONDS. Convert seconds to
    // milliseconds (this feed's unit-mismatch analogue of the microsecond trap)
    // and validate the result through the shared event window: a value already in
    // milliseconds or microseconds lands far above the window and fails closed.
    let started_at = word_to_seconds_rfc3339(&words[2], "startedAt")?;
    let updated_at = word_to_seconds_rfc3339(&words[3], "updatedAt")?;
    // Word 4: answeredInRound (u80, deprecated).
    let answered_in_round = word_to_u128(&words[4], 10, "answeredInRound")?;
    Ok(DecodedRound {
        round_id,
        phase_id,
        aggregator_round_id,
        answer,
        started_at,
        updated_at,
        answered_in_round,
    })
}

/// Decode the `0x`-prefixed result into its five 32-byte ABI words.
fn decode_result_words(result: &str) -> Result<[[u8; WORD_BYTES]; RESULT_WORDS], ResponseReject> {
    let hex = result
        .strip_prefix("0x")
        .or_else(|| result.strip_prefix("0X"))
        .unwrap_or(result);
    if hex.len() != RESULT_HEX_CHARS {
        return Err(shape_reject(format!(
            "Chainlink latestRoundData result must be {RESULT_WORDS}x{WORD_BYTES} bytes \
             ({RESULT_HEX_CHARS} hex chars); received {} hex chars",
            hex.len()
        )));
    }
    let bytes = hex.as_bytes();
    let mut words = [[0u8; WORD_BYTES]; RESULT_WORDS];
    for index in 0..(RESULT_WORDS * WORD_BYTES) {
        let high = hex_nibble(bytes[index * 2]).ok_or_else(nonhex)?;
        let low = hex_nibble(bytes[index * 2 + 1]).ok_or_else(nonhex)?;
        words[index / WORD_BYTES][index % WORD_BYTES] = (high << 4) | low;
    }
    Ok(words)
}

fn nonhex() -> ResponseReject {
    shape_reject("Chainlink result contains a non-hexadecimal digit")
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Decode a big-endian word as an unsigned integer at most `width` bytes wide. A
/// value zero-extended to 32 bytes must have zero high bytes beyond `width`;
/// anything else is a malformed word.
fn word_to_u128(
    word: &[u8; WORD_BYTES],
    width: usize,
    field: &str,
) -> Result<u128, ResponseReject> {
    let high = WORD_BYTES - width;
    if word[..high].iter().any(|byte| *byte != 0) {
        return Err(shape_reject(format!(
            "Chainlink {field} exceeds its {width}-byte width"
        )));
    }
    let mut value: u128 = 0;
    for &byte in &word[high..] {
        value = (value << 8) | u128::from(byte);
    }
    Ok(value)
}

/// Decode a unix-seconds word and convert it, seconds -> milliseconds, into an
/// RFC 3339 timestamp validated through the shared event window.
fn word_to_seconds_rfc3339(word: &[u8; WORD_BYTES], field: &str) -> Result<String, ResponseReject> {
    // A word wider than 8 bytes is structurally malformed (shape); a value that
    // fits but lands outside the plausibility window is a value-validation
    // failure (the seconds-to-millis conversion is this feed's microsecond-trap
    // analogue: a value already in milliseconds is ~1000x too large).
    let seconds = word_to_u128(word, 8, field)?;
    let seconds = i64::try_from(seconds)
        .map_err(|_| shape_reject(format!("Chainlink {field} exceeds i64")))?;
    let millis = seconds
        .checked_mul(1_000)
        .ok_or_else(|| shape_reject(format!("Chainlink {field} seconds-to-millis overflowed")))?;
    millis_to_rfc3339(millis, field).map_err(|error| value_reject(error.to_string()))
}

/// Decode the int256 `answer` word to a fixed 8-decimal string, rejecting any
/// non-positive value.
fn decode_answer(word: &[u8; WORD_BYTES]) -> Result<String, ResponseReject> {
    // Two's-complement sign bit: a set high bit is a negative price, which a USD
    // feed must never report.
    if word[0] & 0x80 != 0 {
        return Err(value_reject(
            "Chainlink answer is negative; a strictly positive price is required",
        ));
    }
    if word.iter().all(|byte| *byte == 0) {
        return Err(value_reject(
            "Chainlink answer is zero; a strictly positive price is required",
        ));
    }
    // Non-negative: the big-endian bytes are the magnitude.
    Ok(scale_digits(
        &be_bytes_to_decimal_digits(word),
        ANSWER_DECIMALS,
    ))
}

/// Convert a big-endian unsigned integer (arbitrary width) to its base-10 digit
/// string via schoolbook division by ten (no floating point, exact).
fn be_bytes_to_decimal_digits(bytes: &[u8]) -> String {
    let mut value = bytes.to_vec();
    let mut digits = Vec::new();
    while value.iter().any(|byte| *byte != 0) {
        let mut remainder: u16 = 0;
        for byte in &mut value {
            let accumulator = (remainder << 8) | u16::from(*byte);
            *byte = (accumulator / 10) as u8;
            remainder = accumulator % 10;
        }
        digits.push(b'0' + remainder as u8);
    }
    if digits.is_empty() {
        digits.push(b'0');
    }
    digits.reverse();
    String::from_utf8(digits).expect("decimal digits are ASCII")
}

/// Place a decimal point `scale` digits from the right of a base-10 digit string.
fn scale_digits(digits: &str, scale: usize) -> String {
    if digits.len() <= scale {
        format!("0.{}{digits}", "0".repeat(scale - digits.len()))
    } else {
        let point = digits.len() - scale;
        format!("{}.{}", &digits[..point], &digits[point..])
    }
}

fn bounded_detail(prefix: &str, value: &str) -> String {
    format!("{prefix}: {}", value.chars().take(80).collect::<String>())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JsonRpcResponse<'a> {
    #[serde(default, borrow)]
    #[allow(dead_code)]
    jsonrpc: Option<&'a str>,
    // The echoed request id may be a number or a string; captured but unused.
    #[serde(default, borrow)]
    #[allow(dead_code)]
    id: Option<&'a RawValue>,
    #[serde(default)]
    result: Option<String>,
    #[serde(default, borrow)]
    error: Option<&'a RawValue>,
}

#[derive(Deserialize)]
struct RpcErrorBody {
    #[serde(default)]
    #[allow(dead_code)]
    code: Option<i64>,
    #[serde(default)]
    message: Option<String>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{PROXY_ADDRESS, ROUNDS_SERIES, RoundResponse, parse_round_response, request_body};
    use crate::chainlink::ChainlinkAdapter;
    use crate::{CaptureStream, FeedAdapter, FeedNoticeCode, IngestOutcome, RawRecord, RecordKind};

    // Live-probed reference round (2026-08-29): roundId 129127208515966885128
    // (phase 7, aggregator round 23816), answer 77667.797 (raw 7766779700000).
    const REFERENCE_ROUND_ID: u128 = 129_127_208_515_966_885_128;
    const REFERENCE_ANSWER_RAW: u128 = 7_766_779_700_000;
    // A plausible seconds timestamp inside the shared event window (2025-08-28).
    const REFERENCE_SECONDS: u128 = 1_756_400_000;

    fn word(value: u128) -> String {
        // A u128 is at most 16 bytes; a 32-byte ABI word is 64 hex chars, so a
        // zero-extended value pads on the left.
        format!("{value:064x}")
    }

    fn result_hex(
        round_id: u128,
        answer: u128,
        started: u128,
        updated: u128,
        answered: u128,
    ) -> String {
        format!(
            "0x{}{}{}{}{}",
            word(round_id),
            word(answer),
            word(started),
            word(updated),
            word(answered),
        )
    }

    fn reference_hex() -> String {
        result_hex(
            REFERENCE_ROUND_ID,
            REFERENCE_ANSWER_RAW,
            REFERENCE_SECONDS,
            REFERENCE_SECONDS,
            REFERENCE_ROUND_ID,
        )
    }

    fn response(result: &str) -> String {
        format!(r#"{{"jsonrpc":"2.0","id":1,"result":"{result}"}}"#)
    }

    fn adapter() -> ChainlinkAdapter {
        ChainlinkAdapter::new("test-epoch").expect("adapter constructs")
    }

    fn record(ordinal: u64, epoch: u64, kind: RecordKind, payload: Option<&str>) -> RawRecord {
        RawRecord {
            schema_version: 1,
            provider: crate::Provider::ChainlinkAggregator,
            stream: CaptureStream::ChainlinkAggregator,
            connection_epoch: epoch,
            ordinal,
            received_at: "2026-08-28T12:00:01Z".to_owned(),
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

    #[test]
    fn request_body_is_the_exact_eth_call_shape() {
        assert_eq!(
            request_body(7),
            format!(
                r#"{{"jsonrpc":"2.0","method":"eth_call","params":[{{"to":"{PROXY_ADDRESS}","data":"0xfeaf968c"}},"latest"],"id":7}}"#
            )
        );
    }

    #[test]
    fn decodes_the_live_probed_round_and_answer() {
        let RoundResponse::Round(round) =
            parse_round_response(&response(&reference_hex())).expect("valid round")
        else {
            panic!("expected a decoded round");
        };
        assert_eq!(round.round_id, REFERENCE_ROUND_ID);
        assert_eq!(round.phase_id, 7);
        assert_eq!(round.aggregator_round_id, 23_816);
        assert_eq!(round.answer, "77667.79700000");
        assert_eq!(round.answered_in_round, REFERENCE_ROUND_ID);
        // startedAt / updatedAt round-trip through the seconds->millis window.
        assert_eq!(round.updated_at, "2025-08-28T16:53:20.000Z");
        assert_eq!(round.started_at, round.updated_at);
    }

    #[test]
    fn small_answer_renders_with_leading_zero_and_eight_decimals() {
        let hex = result_hex(
            REFERENCE_ROUND_ID,
            797,
            REFERENCE_SECONDS,
            REFERENCE_SECONDS,
            1,
        );
        let RoundResponse::Round(round) =
            parse_round_response(&response(&hex)).expect("valid round")
        else {
            panic!("expected a decoded round");
        };
        assert_eq!(round.answer, "0.00000797");
    }

    /// Drive a valid round (to seed the cursor), then a bad response, and assert
    /// the bad response is a notice + skip -- no events, the expected class code,
    /// and the cursor untouched (proved by the seeded round still replaying as a
    /// silent duplicate afterwards, and the session continuing).
    fn assert_notice_and_skip(bad_response: &str, expected: FeedNoticeCode) {
        let mut adapter = adapter();
        adapter.ingest(&connected(1, 1)).expect("connects");
        let first = adapter
            .ingest(&inbound(2, &response(&reference_hex())))
            .expect("first round");
        assert_eq!(first.events.len(), 1);
        let skipped = adapter
            .ingest(&inbound(3, bad_response))
            .expect("a bad poll response is operational, never session-fatal");
        assert!(skipped.events.is_empty(), "bad response emits no event");
        assert_eq!(skipped.notices.len(), 1);
        assert_eq!(skipped.notices[0].code, expected);
        assert!(skipped.notices[0].recovery.is_none());
        // Cursor untouched: the seeded round is still a silent duplicate.
        let duplicate = adapter
            .ingest(&inbound(4, &response(&reference_hex())))
            .expect("duplicate round");
        assert!(duplicate.events.is_empty());
        assert!(duplicate.notices.is_empty());
    }

    #[test]
    fn value_validation_failures_notice_and_skip() {
        // Zero answer.
        assert_notice_and_skip(
            &response(&result_hex(
                REFERENCE_ROUND_ID,
                0,
                REFERENCE_SECONDS,
                REFERENCE_SECONDS,
                1,
            )),
            FeedNoticeCode::StateInconsistent,
        );
        // Negative answer: word 1 with the sign bit set (two's complement -1).
        let negative = format!(
            "0x{}{}{}{}{}",
            word(REFERENCE_ROUND_ID),
            "f".repeat(64),
            word(REFERENCE_SECONDS),
            word(REFERENCE_SECONDS),
            word(1),
        );
        assert_notice_and_skip(&response(&negative), FeedNoticeCode::StateInconsistent);
        // updatedAt in MILLISECONDS is ~1000x too large after seconds->millis and
        // lands outside the shared plausibility window.
        assert_notice_and_skip(
            &response(&result_hex(
                REFERENCE_ROUND_ID,
                REFERENCE_ANSWER_RAW,
                REFERENCE_SECONDS,
                REFERENCE_SECONDS * 1_000,
                REFERENCE_ROUND_ID,
            )),
            FeedNoticeCode::StateInconsistent,
        );
    }

    #[test]
    fn envelope_shape_failures_notice_and_skip() {
        // Short result.
        assert_notice_and_skip(&response("0x1234"), FeedNoticeCode::UnsupportedMessage);
        // Non-hex result of the right length.
        assert_notice_and_skip(
            &response(&format!("0x{}", "z".repeat(super::RESULT_HEX_CHARS))),
            FeedNoticeCode::UnsupportedMessage,
        );
        // roundId (word 0) with a byte set above its 10-byte width (bit 88).
        let round_overflow = word(1u128 << 88);
        let overflow = format!(
            "0x{}{}{}{}{}",
            round_overflow,
            word(REFERENCE_ANSWER_RAW),
            word(REFERENCE_SECONDS),
            word(REFERENCE_SECONDS),
            word(REFERENCE_ROUND_ID),
        );
        assert_notice_and_skip(&response(&overflow), FeedNoticeCode::UnsupportedMessage);
        // An unknown top-level field fails deny_unknown_fields (envelope shape).
        let extra = format!(
            r#"{{"jsonrpc":"2.0","id":1,"result":"{}","extra":true}}"#,
            reference_hex()
        );
        assert_notice_and_skip(&extra, FeedNoticeCode::UnsupportedMessage);
    }

    #[test]
    fn a_bad_response_does_not_poison_the_rest_of_the_session() {
        // One odd 2xx body (here an extra top-level field) is noticed and
        // skipped, and the session replays to completion -- a later advanced
        // round still emits.
        let mut adapter = adapter();
        adapter.ingest(&connected(1, 1)).expect("connects");
        adapter
            .ingest(&inbound(2, &response(&reference_hex())))
            .expect("first round");
        let bad = format!(
            r#"{{"jsonrpc":"2.0","id":1,"result":"{}","extra":true}}"#,
            reference_hex()
        );
        let skipped = adapter
            .ingest(&inbound(3, &bad))
            .expect("bad body is operational");
        assert!(skipped.events.is_empty());
        assert_eq!(skipped.notices[0].code, FeedNoticeCode::UnsupportedMessage);
        // The session continues: an advanced round emits normally.
        let advanced = result_hex(
            REFERENCE_ROUND_ID + 1,
            REFERENCE_ANSWER_RAW,
            REFERENCE_SECONDS + 60,
            REFERENCE_SECONDS + 60,
            REFERENCE_ROUND_ID + 1,
        );
        let advance = adapter
            .ingest(&inbound(4, &response(&advanced)))
            .expect("advance round");
        assert_eq!(advance.events.len(), 1);
    }

    #[test]
    fn error_response_is_a_transient_notice_without_events() {
        let mut adapter = adapter();
        adapter.ingest(&connected(1, 1)).expect("connects");
        let outcome = adapter
            .ingest(&inbound(
                2,
                r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"rate limited"}}"#,
            ))
            .expect("error response is operational");
        assert!(outcome.events.is_empty());
        assert_eq!(outcome.notices[0].code, FeedNoticeCode::StateInconsistent);
        assert!(outcome.notices[0].recovery.is_none());
    }

    #[test]
    fn duplicate_round_is_silent_and_an_advance_emits() {
        let mut adapter = adapter();
        adapter.ingest(&connected(1, 1)).expect("connects");
        let first = adapter
            .ingest(&inbound(2, &response(&reference_hex())))
            .expect("first round");
        assert_eq!(first.events.len(), 1);
        assert_eq!(
            first.events[0].source_id,
            "chainlink.ethereum.btc-usd.rounds"
        );
        assert_eq!(
            first.events[0].values.get("instrument"),
            Some(&json!("BTC-USD"))
        );
        assert_eq!(
            first.events[0].values.get("round_id"),
            Some(&json!(REFERENCE_ROUND_ID.to_string()))
        );
        assert_eq!(first.events[0].values.get("phase_id"), Some(&json!(7)));
        assert!(first.notices.is_empty());
        // Same roundId: no event, no notice.
        let duplicate = adapter
            .ingest(&inbound(3, &response(&reference_hex())))
            .expect("duplicate round");
        assert!(duplicate.events.is_empty());
        assert!(duplicate.notices.is_empty());
        // Advanced roundId within the same phase: one event, no phase notice.
        let advanced = result_hex(
            REFERENCE_ROUND_ID + 1,
            REFERENCE_ANSWER_RAW,
            REFERENCE_SECONDS + 60,
            REFERENCE_SECONDS + 60,
            REFERENCE_ROUND_ID + 1,
        );
        let advance = adapter
            .ingest(&inbound(4, &response(&advanced)))
            .expect("advance round");
        assert_eq!(advance.events.len(), 1);
        assert!(advance.notices.is_empty());
    }

    #[test]
    fn phase_increase_attaches_an_upgrade_notice() {
        let mut adapter = adapter();
        adapter.ingest(&connected(1, 1)).expect("connects");
        adapter
            .ingest(&inbound(2, &response(&reference_hex())))
            .expect("phase-7 round");
        // Phase 8, aggregator round 1: roundId = (8 << 64) | 1.
        let phase_8 = (8u128 << 64) | 1;
        let upgraded = result_hex(
            phase_8,
            REFERENCE_ANSWER_RAW,
            REFERENCE_SECONDS + 3_600,
            REFERENCE_SECONDS + 3_600,
            phase_8,
        );
        let outcome = adapter
            .ingest(&inbound(3, &response(&upgraded)))
            .expect("phase-8 round");
        assert_eq!(outcome.events.len(), 1);
        assert_eq!(outcome.events[0].values.get("phase_id"), Some(&json!(8)));
        assert_eq!(outcome.notices.len(), 1);
        assert_eq!(outcome.notices[0].code, FeedNoticeCode::VenueStatus);
    }

    #[test]
    fn round_regression_is_out_of_order_without_events() {
        let mut adapter = adapter();
        adapter.ingest(&connected(1, 1)).expect("connects");
        adapter
            .ingest(&inbound(2, &response(&reference_hex())))
            .expect("first round");
        let regressed = result_hex(
            REFERENCE_ROUND_ID - 1,
            REFERENCE_ANSWER_RAW,
            REFERENCE_SECONDS,
            REFERENCE_SECONDS,
            REFERENCE_ROUND_ID - 1,
        );
        let outcome = adapter
            .ingest(&inbound(3, &response(&regressed)))
            .expect("regressed round");
        assert!(outcome.events.is_empty());
        assert_eq!(outcome.notices[0].code, FeedNoticeCode::OutOfOrder);
    }

    #[test]
    fn cursor_survives_reconnect_within_a_run() {
        // B4: the round cursor is global chain state, so a Disconnected/Connected
        // reconnect (a failover) does NOT reset it. After the reconnect the same
        // roundId is a silent duplicate, and a LOWER roundId from a lagging node
        // is skipped OutOfOrder — not republished with a backwards event_time.
        let mut adapter = adapter();
        adapter.ingest(&connected(1, 1)).expect("connects");
        adapter
            .ingest(&inbound(2, &response(&reference_hex())))
            .expect("first round in epoch 1");
        adapter
            .ingest(&record(3, 1, RecordKind::Disconnected, None))
            .expect("disconnects");
        adapter.ingest(&connected(4, 2)).expect("reconnects");
        // Same round after failover: silent duplicate, no event.
        let duplicate = adapter
            .ingest(&RawRecord {
                connection_epoch: 2,
                ..inbound(5, &response(&reference_hex()))
            })
            .expect("same round in epoch 2");
        assert!(
            duplicate.events.is_empty(),
            "cursor retained across reconnect"
        );
        assert!(duplicate.notices.is_empty());
        // A lagging node serves an OLDER round after failover: OutOfOrder, no event.
        let lagging = result_hex(
            REFERENCE_ROUND_ID - 1,
            REFERENCE_ANSWER_RAW,
            REFERENCE_SECONDS - 60,
            REFERENCE_SECONDS - 60,
            REFERENCE_ROUND_ID - 1,
        );
        let regressed = adapter
            .ingest(&RawRecord {
                connection_epoch: 2,
                ..inbound(6, &response(&lagging))
            })
            .expect("older round in epoch 2");
        assert!(regressed.events.is_empty());
        assert_eq!(regressed.notices[0].code, FeedNoticeCode::OutOfOrder);
        // A newer round after the gap is accepted.
        let newer = result_hex(
            REFERENCE_ROUND_ID + 1,
            REFERENCE_ANSWER_RAW,
            REFERENCE_SECONDS + 60,
            REFERENCE_SECONDS + 60,
            REFERENCE_ROUND_ID + 1,
        );
        let advance = adapter
            .ingest(&RawRecord {
                connection_epoch: 2,
                ..inbound(7, &response(&newer))
            })
            .expect("newer round in epoch 2");
        assert_eq!(advance.events.len(), 1);
    }

    #[test]
    fn configuration_records_are_refused() {
        let mut adapter = adapter();
        let configuration = record(1, 1, RecordKind::Configuration, Some("{}"));
        assert!(adapter.ingest(&configuration).is_err());
    }

    #[test]
    fn replay_is_deterministic_over_connected_and_poll_pairs() {
        let advanced = result_hex(
            REFERENCE_ROUND_ID + 1,
            REFERENCE_ANSWER_RAW,
            REFERENCE_SECONDS + 60,
            REFERENCE_SECONDS + 60,
            REFERENCE_ROUND_ID + 1,
        );
        let fixture = vec![
            connected(1, 1),
            record(2, 1, RecordKind::OutboundText, Some(&request_body(1))),
            inbound(3, &response(&reference_hex())),
            record(4, 1, RecordKind::OutboundText, Some(&request_body(2))),
            inbound(5, &response(&reference_hex())),
            record(6, 1, RecordKind::OutboundText, Some(&request_body(3))),
            inbound(7, &response(&advanced)),
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
    }

    #[test]
    fn rounds_series_matches_the_shared_constant() {
        assert_eq!(ROUNDS_SERIES, "chainlink_aggregator:latest-round-data");
    }

    // --- Adoption guards (A5 phase 1) ---

    fn emitted_field_names() -> std::collections::BTreeSet<String> {
        let mut adapter = adapter();
        adapter.ingest(&connected(1, 1)).expect("connects");
        let first = adapter
            .ingest(&inbound(2, &response(&reference_hex())))
            .expect("first round");
        first.events[0].values.keys().cloned().collect()
    }

    /// T-DEF-1: the definition `fields` array and the emitted `values` map agree
    /// exactly, both directions.
    #[test]
    fn definition_field_set_matches_round_source_event() {
        let definition: signals_core::SourceDefinition =
            serde_json::from_str(super::SOURCE_DEFINITION).expect("definition parses");
        let declared: std::collections::BTreeSet<String> =
            definition.fields.iter().map(|f| f.name.clone()).collect();
        assert_eq!(declared, emitted_field_names());
    }

    /// T-DEF-2: identity and lifecycle facts of the bound definition.
    #[test]
    fn source_id_and_version_match_the_definition() {
        let definition: signals_core::SourceDefinition =
            serde_json::from_str(super::SOURCE_DEFINITION).expect("definition parses");
        assert_eq!(definition.source_id, super::SOURCE_ID);
        assert_eq!(definition.version, super::SOURCE_VERSION);
        assert_eq!(definition.status, signals_core::LifecycleStatus::Approved);
        assert_eq!(definition.transport, signals_core::Transport::HttpPoll);
        assert_eq!(definition.event_time_field, "event_time");
        assert!(definition.sequence_field.is_none());
    }

    /// The three call selectors are the documented keccak256(signature)[..4]
    /// values. (Computed offline and pinned; no keccak crate is vendored.)
    #[test]
    fn call_selectors_are_pinned() {
        assert_eq!(super::LATEST_ROUND_DATA_SELECTOR, "0xfeaf968c");
        assert_eq!(super::DECIMALS_SELECTOR, "0x313ce567");
        assert_eq!(super::DESCRIPTION_SELECTOR, "0x7284e416");
    }

    #[test]
    fn oversize_response_is_unsupported_and_skips() {
        let big = format!("0x{}", "0".repeat(super::MAX_RESPONSE_BYTES + 1));
        assert_notice_and_skip(&response(&big), FeedNoticeCode::UnsupportedMessage);
    }

    #[test]
    fn off_by_one_hex_length_is_a_shape_reject() {
        // 319 and 321 hex chars bracket the 320-char (160-byte) five-word reply.
        assert_notice_and_skip(
            &response(&format!("0x{}", "0".repeat(319))),
            FeedNoticeCode::UnsupportedMessage,
        );
        assert_notice_and_skip(
            &response(&format!("0x{}", "0".repeat(321))),
            FeedNoticeCode::UnsupportedMessage,
        );
    }

    #[test]
    fn endpoint_list_requires_two_https_entries() {
        // T-CFG-1: fewer than two → refuse; a non-https entry → refuse; a valid
        // two-entry list yields the order as written.
        assert!(super::parse_rpc_endpoints("https://only-one.example").is_err());
        assert!(
            super::parse_rpc_endpoints("https://a.example,http://b.example").is_err(),
            "a non-https entry refuses"
        );
        let ok =
            super::parse_rpc_endpoints("https://a.example, https://b.example, https://c.example")
                .expect("three https entries parse");
        assert_eq!(
            ok,
            vec![
                "https://a.example".to_owned(),
                "https://b.example".to_owned(),
                "https://c.example".to_owned(),
            ]
        );
        assert!(
            super::DEFAULT_RPC_ENDPOINTS.len() >= 2
                && super::DEFAULT_RPC_ENDPOINTS
                    .iter()
                    .all(|e| e.starts_with("https://")),
            "the compiled default satisfies R9"
        );
    }

    fn decimals_reply(value: u128) -> String {
        response(&format!("0x{value:064x}"))
    }

    fn description_reply(text: &str) -> String {
        // ABI dynamic string: offset(0x20), length, then right-padded UTF-8 data.
        let data = text.as_bytes();
        let mut data_hex = String::new();
        for byte in data {
            data_hex.push_str(&format!("{byte:02x}"));
        }
        while !data_hex.len().is_multiple_of(64) {
            data_hex.push('0');
        }
        response(&format!(
            "0x{:064x}{:064x}{data_hex}",
            32u128,
            data.len() as u128
        ))
    }

    /// T-DEC-2: a correct scale/identity is Verified; a decoded wrong value is a
    /// fatal Mismatch; an undecodable reply (JSON-RPC error, junk) is Transient
    /// (rotate, not fatal) — this is the B1 classification.
    #[test]
    fn target_checks_classify_verified_mismatch_and_transient() {
        use super::TargetCheck;
        assert_eq!(
            super::verify_decimals_reply(&decimals_reply(8)),
            TargetCheck::Verified
        );
        assert!(matches!(
            super::verify_decimals_reply(&decimals_reply(6)),
            TargetCheck::Mismatch(_)
        ));
        assert!(matches!(
            super::verify_decimals_reply(&decimals_reply(18)),
            TargetCheck::Mismatch(_)
        ));
        assert_eq!(
            super::verify_description_reply(&description_reply("BTC / USD")),
            TargetCheck::Verified
        );
        assert!(matches!(
            super::verify_description_reply(&description_reply("ETH / USD")),
            TargetCheck::Mismatch(_)
        ));
        assert!(matches!(
            super::verify_decimals_reply(
                r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32603,"message":"internal"}}"#
            ),
            TargetCheck::Transient(_)
        ));
        assert!(matches!(
            super::verify_description_reply("<html>502</html>"),
            TargetCheck::Transient(_)
        ));
    }

    /// B2 regression: a `description()` reply whose ABI offset is `u64::MAX - 16`
    /// must fail closed (Transient), never panic or wrap on the offset add.
    #[test]
    fn description_offset_near_usize_max_fails_closed() {
        let payload = format!(
            r#"{{"jsonrpc":"2.0","id":1,"result":"0x{:064x}{:064x}"}}"#,
            u128::from(u64::MAX - 16),
            9u128
        );
        assert!(matches!(
            super::verify_description_reply(&payload),
            super::TargetCheck::Transient(_)
        ));
    }

    #[test]
    fn endpoint_lists_are_distinct_after_normalisation() {
        // Same host with and without a trailing slash, or differing only in
        // case, is one endpoint and voids R9.
        assert!(super::parse_rpc_endpoints("https://a.example,https://a.example/").is_err());
        assert!(super::parse_rpc_endpoints("https://A.example,https://a.example").is_err());
        assert!(super::parse_rpc_endpoints("https://,https://b.example").is_err());
        assert!(super::parse_rpc_endpoints("https://user:pw@a.example,https://b.example").is_ok());
        assert!(
            super::validate_endpoint_list(&["https://".to_owned(), "https://".to_owned()])
                .unwrap_err()
                .contains("with a host")
        );
    }

    #[test]
    fn redact_endpoint_strips_path_query_and_credentials() {
        assert_eq!(
            super::redact_endpoint("https://user:pass@eth.example.com/v1/key?token=abc"),
            "https://eth.example.com"
        );
        assert_eq!(
            super::redact_endpoint("https://eth.merkle.io"),
            "https://eth.merkle.io"
        );
    }

    /// M3: the driver represents an outage/recovery as a `Disconnected` then a
    /// `Connected` record, so the operational notices are adapter-derived and
    /// replay reproduces them byte-for-byte and deterministically.
    #[test]
    fn reconnect_record_grammar_is_deterministic_in_the_adapter() {
        let round = reference_hex();
        let sequence = || {
            vec![
                connected(1, 1),
                record(2, 1, RecordKind::OutboundText, Some(&request_body(1))),
                record(3, 1, RecordKind::Disconnected, None),
                record(4, 2, RecordKind::Connected, None),
                record(5, 2, RecordKind::OutboundText, Some(&request_body(2))),
                record(6, 2, RecordKind::InboundText, Some(&response(&round))),
            ]
        };
        let run = || {
            let mut adapter = adapter();
            let mut codes = Vec::new();
            let mut events = Vec::new();
            for r in sequence() {
                let out = adapter.ingest(&r).expect("record ingests");
                for notice in &out.notices {
                    codes.push(notice.code);
                }
                events.extend(out.events);
            }
            (
                codes,
                serde_json::to_string(&events).expect("events serialize"),
            )
        };
        let (codes_a, events_a) = run();
        let (codes_b, events_b) = run();
        assert_eq!(codes_a, codes_b, "notice codes are deterministic");
        assert_eq!(events_a, events_b, "events replay byte-for-byte");
        assert_eq!(
            codes_a,
            vec![
                FeedNoticeCode::Connected,
                FeedNoticeCode::Disconnected,
                FeedNoticeCode::Connected,
            ],
            "outage is a Disconnected, recovery is a Connected"
        );
    }
}
