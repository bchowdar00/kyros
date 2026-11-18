//! Binance USD-M BTCUSDT perpetual adapter: partial-depth book snapshots on the
//! `public` WebSocket connection, plus the bounded REST polls.
//!
//! # Documented, not observed (ruling R1)
//!
//! Every wire fact in this module comes from `A3-binance-usdm-feeds.md`, which
//! built it from `DATA-AVAILABILITY.md`'s Binance USD-M rows. **No USD-M frame
//! has been captured yet.** The module is written so that the wave-A capture on
//! the Tokyo host can correct it: each message type's assumptions are stated at
//! its parser, the live runner refuses these streams outright
//! (`ensure_live_stream_supported`), and the `binance` lane is refused by the
//! venue gate until the rights row clears. Capture with `btc-feeds capture`,
//! normalize with `btc-feeds replay`, then finalize this module against the
//! frames and correct the source definitions to what was observed.
//!
//! # One socket connection
//!
//! USD-M market data is carried here on the `public` WebSocket connection, which
//! delivers the high-frequency depth family. The low-frequency families that
//! once shared a second `market` connection (mark price, aggregate trades,
//! forced orders) were removed with the `market`-connection sources that no host
//! we own can dial (ruling E62); the mark/index/funding state they used to feed
//! now comes from the REST premium-index poll alone. The bounded REST polls
//! (open interest, its history, the venue-declared funding interval and the
//! premium index) travel on the `rest` connection. The connection an adapter was
//! constructed for fixes the stream names it will normalize, so a payload on a
//! connection whose compiled set does not name its stream is an unsupported
//! message rather than a book event normalized under a source id its own
//! connection never carried. This is the binding `bybit.rs` uses between
//! `BybitMarket` and its topic set.
//!
//! The `public` connection dials the venue's **combined-stream** URL,
//! `wss://fstream.binance.com/stream?streams=<a>`, naming exactly that
//! connection's compiled stream set. An earlier reading of this spec had the
//! depth family on a `/public` path; Binance publishes no such path, so the
//! upgrade simply failed, and because nothing logged the failure a 65-minute
//! capture recorded zero bytes in silence (ruling E27). The connection name
//! survives that correction because it names our binding; only the path was
//! never real. Subscribing through the URL is also why the connection sends no
//! `SUBSCRIBE` frame — the stream is live the moment the socket upgrades — and
//! why every frame arrives in the combined `{"stream":…,"data":{…}}` wrapper,
//! though a bare payload is still accepted.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::value::RawValue;
use serde_json::{Value, json};
use signals_core::SourceEvent;

use crate::adapter::{GuardAction, RecordGuard, bounded_token, validate_publisher_epoch};
use crate::decimal::validate_decimal;
use crate::event_window::millis_to_rfc3339;
use crate::{
    AdapterError, FeedAdapter, FeedNotice, FeedNoticeCode, IngestOutcome, Provider, RawRecord,
    RecoveryAction,
};

const INSTRUMENT: &str = "BTCUSDT";

const DEPTH_SOURCE_ID: &str = "binance.usdm.btcusdt.book-levels";
const SOURCE_VERSION: &str = "1.0.0";

/// Stream name carried by the `public` connection.
///
/// Compiled here and shared verbatim with `capture.rs::subscriptions`, exactly
/// as `binance.rs::DEPTH_STREAM` is today: the subscription and the dispatch
/// must never be able to drift apart.
pub(crate) const DEPTH_STREAM: &str = "btcusdt@depth20@100ms";

/// The `public` connection's compiled stream set.
pub(crate) const PUBLIC_STREAMS: [&str; 1] = [DEPTH_STREAM];

/// Maximum accepted bid+ask rows in one partial-depth message.
///
/// `depth20` carries at most 40; the bound is the same generous ceiling
/// `binance.rs` uses so a hostile payload cannot force an unbounded allocation.
pub(crate) const MAX_DEPTH_ROWS: usize = 4_096;

/// Which USD-M connection an adapter instance normalizes.
///
/// The variant fixes both the accepted stream names and the emittable source
/// ids. It is not a hint: a message belonging to another connection is refused
/// with an `UnsupportedMessage` notice and never normalized.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinanceUsdmConnection {
    /// `wss://fstream.binance.com/public`: partial-depth book snapshots.
    Public,
    /// The bounded REST pollers. Their request and response records are parsed
    /// by [`crate::binance_usdm_rest`]; this adapter delegates to it so one
    /// type covers every USD-M capture stream.
    Rest,
}

impl BinanceUsdmConnection {
    /// The stream names this connection subscribes and normalizes.
    #[must_use]
    pub fn streams(self) -> &'static [&'static str] {
        match self {
            Self::Public => &PUBLIC_STREAMS,
            Self::Rest => &[],
        }
    }
}

/// Deterministic Binance USD-M parser and ordering state for one connection.
pub struct BinanceUsdmAdapter {
    publisher_epoch: String,
    guard: RecordGuard,
    connection: BinanceUsdmConnection,
    next_depth_sequence: u64,
    /// `u` of the last admitted depth message. Connection-local.
    last_depth_update_id: Option<u64>,
    rest: crate::binance_usdm_rest::BinanceUsdmRestState,
}

impl BinanceUsdmAdapter {
    /// Construct a USD-M adapter bound to one connection.
    ///
    /// `publisher_epoch` is copied verbatim into every emitted event; it must be
    /// non-blank and at most 128 bytes, or the call returns
    /// [`AdapterError::InvalidRecord`].
    pub fn new(
        publisher_epoch: impl Into<String>,
        connection: BinanceUsdmConnection,
    ) -> Result<Self, AdapterError> {
        let publisher_epoch = publisher_epoch.into();
        validate_publisher_epoch(&publisher_epoch)?;
        Ok(Self {
            publisher_epoch,
            guard: RecordGuard::new(Provider::BinanceUsdm),
            connection,
            next_depth_sequence: 0,
            last_depth_update_id: None,
            rest: crate::binance_usdm_rest::BinanceUsdmRestState::new(),
        })
    }

    /// Which connection this adapter normalizes.
    #[must_use]
    pub fn connection(&self) -> BinanceUsdmConnection {
        self.connection
    }

    /// Reset every connection-local cursor.
    ///
    fn reset_connection_state(&mut self) {
        self.last_depth_update_id = None;
        self.rest.reset();
    }

    fn ingest_payload(
        &mut self,
        record: &RawRecord,
        payload: &str,
    ) -> Result<IngestOutcome, AdapterError> {
        if self.connection == BinanceUsdmConnection::Rest {
            return crate::binance_usdm_rest::ingest_response(self, record, payload);
        }
        // Probe P1 has not settled whether the combined URL form is accepted on
        // the `public` connection. Its fail-closed default is one connection per
        // stream name, whose frames carry NO `{"stream":…,"data":{…}}` wrapper,
        // so both shapes are accepted here and the probe's answer changes no
        // wiring. The wrapper is tried first because it is the stricter parse;
        // anything it refuses is dispatched as a bare payload on its own `e`.
        let envelope: Option<BinanceUsdmEnvelope> = serde_json::from_str(payload).ok();
        if let Some(error) = envelope.as_ref().and_then(|value| value.error.as_deref()) {
            return Ok(IngestOutcome::notice(FeedNotice::info(
                FeedNoticeCode::SubscriptionRejected,
                bounded_detail("Binance USD-M subscription rejected", error.get()),
            )));
        }
        // The combined-stream wrapper is `{"stream":<name>,"data":{..}}`. Probe
        // P1 has not yet settled whether the combined URL form is accepted on
        // the `public` connection, so a BARE payload — one connection per stream
        // name, the fail-closed default — is dispatched on its own `e` field.
        // Both shapes are accepted here so the capture's result changes no
        // wiring, only which shape the frames happen to have.
        let wrapped = envelope.as_ref().and_then(|value| {
            match (value.stream.as_deref(), value.data.as_deref()) {
                (Some(stream), Some(data)) => Some((stream, data)),
                _ => None,
            }
        });
        if wrapped.is_none()
            && envelope
                .as_ref()
                .is_some_and(|value| value.id.is_some() && value.stream.is_none())
        {
            return Ok(IngestOutcome::notice(FeedNotice::info(
                FeedNoticeCode::SubscriptionAccepted,
                "Binance USD-M subscription acknowledged",
            )));
        }
        let bare;
        let (stream, data) = match wrapped {
            Some((stream, data)) => (Some(stream), data),
            None => {
                bare = serde_json::from_str::<&RawValue>(payload).map_err(payload_error)?;
                (None, bare)
            }
        };
        if let Some(stream) = stream
            && !self.connection.streams().contains(&stream)
        {
            return Ok(IngestOutcome::notice(FeedNotice::info(
                FeedNoticeCode::UnsupportedMessage,
                format!(
                    "retained Binance USD-M stream `{}` outside this connection's compiled set without normalized output",
                    bounded_token(stream)
                ),
            )));
        }
        let event_type: BinanceUsdmEventType =
            serde_json::from_str(data.get()).map_err(payload_error)?;
        let dispatch = match (event_type.event_type.as_deref(), self.connection) {
            (Some("depthUpdate"), BinanceUsdmConnection::Public) => Some(self.depth(record, data)),
            _ => None,
        };
        dispatch.unwrap_or_else(|| {
            Ok(IngestOutcome::notice(FeedNotice::info(
                FeedNoticeCode::UnsupportedMessage,
                format!(
                    "retained unsupported Binance USD-M event type `{}` on the {} connection without normalized output",
                    bounded_token(event_type.event_type.as_deref().unwrap_or("<absent>")),
                    self.connection.token()
                ),
            )))
        })
    }

    /// `depthUpdate` on `btcusdt@depth20@100ms`.
    ///
    /// Partial depth is a COMPLETE top-20 replacement per message, not a diff,
    /// so there is no snapshot seeding and no diff-continuity machinery. Unlike
    /// the spot stream it carries `E`/`T` and `U`/`u`/`pu`, so the event time is
    /// source-based and the ordering cursor is the venue's own final update id.
    ///
    /// DOCUMENTED, UNOBSERVED: that the USD-M partial-depth message carries
    /// those five fields at all. If the capture shows otherwise, this parser and
    /// `binance.usdm.btcusdt.book-levels` are corrected together.
    ///
    /// The partial-depth stream does not display Retail Price Improvement
    /// orders, which the source definition states so no consumer reads it as the
    /// whole book.
    fn depth(
        &mut self,
        record: &RawRecord,
        data: &RawValue,
    ) -> Result<IngestOutcome, AdapterError> {
        let depth: BinanceUsdmDepth = serde_json::from_str(data.get()).map_err(payload_error)?;
        require_symbol(&depth.symbol)?;
        let row_count = depth
            .bids
            .len()
            .checked_add(depth.asks.len())
            .ok_or_else(|| AdapterError::InvalidPayload("depth row count overflowed".to_owned()))?;
        if row_count > MAX_DEPTH_ROWS {
            return Err(AdapterError::InvalidPayload(format!(
                "Binance USD-M depth message has {row_count} levels; maximum is {MAX_DEPTH_ROWS}"
            )));
        }
        if depth.first_update_id > depth.last_update_id {
            return Err(AdapterError::InvalidPayload(format!(
                "Binance USD-M depth first update id {} exceeds last update id {}",
                depth.first_update_id, depth.last_update_id
            )));
        }
        let event_time = millis_to_rfc3339(depth.event_time, "event_time")?;
        let transaction_time = millis_to_rfc3339(depth.transaction_time, "transaction_time")?;
        let mut rows = Vec::with_capacity(row_count);
        for level in &depth.bids {
            validate_decimal(&level[0], "bids.price")?;
            validate_decimal(&level[1], "bids.size")?;
            rows.push(("bid", level[0].clone(), level[1].clone()));
        }
        for level in &depth.asks {
            validate_decimal(&level[0], "asks.price")?;
            validate_decimal(&level[1], "asks.size")?;
            rows.push(("ask", level[0].clone(), level[1].clone()));
        }

        let mut notices = Vec::new();
        if let Some(previous) = self.last_depth_update_id {
            if depth.last_update_id == previous {
                return Ok(IngestOutcome::notice(FeedNotice::info(
                    FeedNoticeCode::Duplicate,
                    format!("duplicate Binance USD-M depth update id {previous} ignored"),
                )));
            }
            if depth.last_update_id < previous {
                return Ok(IngestOutcome::notice(FeedNotice::info(
                    FeedNoticeCode::OutOfOrder,
                    format!(
                        "out-of-order Binance USD-M depth update id {} after {previous} ignored",
                        depth.last_update_id
                    ),
                )));
            }
            // `pu` is the previous message's final update id. A break in that
            // chain loses no state on a FULL-REPLACE stream — the message in
            // hand is a complete image — so it is informational, never a gap.
            //
            // A NEGATIVE `pu` is the venue's resync marker. The observed hour
            // carried exactly one, at frame 17,033, immediately after its
            // 5,299 ms stall, with 25,030 update ids between the previous `u`
            // and this frame's `U` never delivered. It is not a connection
            // opener (frame 1 chained normally), so it is reported like any
            // other break, with the number of ids the venue skipped.
            if depth.previous_final_update_id < 0 {
                let skipped = depth
                    .first_update_id
                    .saturating_sub(previous)
                    .saturating_sub(1);
                notices.push(FeedNotice::info(
                    FeedNoticeCode::StateInconsistent,
                    format!(
                        "Binance USD-M depth resync marker (pu {}) after {previous}: {skipped} update ids not delivered; the full-replace image still applies",
                        depth.previous_final_update_id
                    ),
                ));
            } else if u64::try_from(depth.previous_final_update_id).is_ok_and(|pu| pu != previous) {
                notices.push(FeedNotice::info(
                    FeedNoticeCode::StateInconsistent,
                    format!(
                        "Binance USD-M depth previous final update id {} does not chain to {previous}; the full-replace image still applies",
                        depth.previous_final_update_id
                    ),
                ));
            }
        }
        self.last_depth_update_id = Some(depth.last_update_id);

        if rows.is_empty() {
            // An empty top-20 book is a degenerate venue state, not a parse
            // failure: note it and skip normalized output so replay does not
            // abort. Unlike the previous early return, the cursor HAS advanced
            // above and the ordering/chain checks have run, so the next frame's
            // `pu` chains against this frame's id — not a superseded one — and a
            // redelivered empty frame is recognised as a duplicate. Any chain
            // notice this frame itself raised rides alongside the no-levels one.
            notices.push(FeedNotice::info(
                FeedNoticeCode::StateInconsistent,
                "Binance USD-M depth message carried no levels; skipped without normalized output",
            ));
            return Ok(IngestOutcome {
                events: Vec::new(),
                notices,
            });
        }

        let update_count = u32::try_from(rows.len()).map_err(|_| {
            AdapterError::InvalidPayload("depth update_count exceeds u32".to_owned())
        })?;
        // One message's levels are bound together by `update_group`, and
        // `update_count` states the complete set: a level absent from this
        // message is gone from the book (top-20 truncation is authoritative).
        let update_group = format!(
            "{}:{}:{}",
            record.stream.as_str(),
            record.connection_epoch,
            record.ordinal
        );
        let mut events = Vec::with_capacity(rows.len());
        for (index, (side, price, size)) in rows.into_iter().enumerate() {
            let update_index = u32::try_from(index).map_err(|_| {
                AdapterError::InvalidPayload("depth update_index exceeds u32".to_owned())
            })?;
            let values = BTreeMap::from([
                ("instrument".to_owned(), json!(INSTRUMENT)),
                ("update_group".to_owned(), json!(update_group)),
                ("update_index".to_owned(), json!(update_index)),
                ("update_count".to_owned(), json!(update_count)),
                ("update_kind".to_owned(), json!("snapshot")),
                ("side".to_owned(), json!(side)),
                ("price".to_owned(), json!(price)),
                ("size".to_owned(), json!(size)),
                ("first_update_id".to_owned(), json!(depth.first_update_id)),
                ("last_update_id".to_owned(), json!(depth.last_update_id)),
                (
                    "previous_final_update_id".to_owned(),
                    json!(depth.previous_final_update_id),
                ),
                ("event_time_basis".to_owned(), json!("source")),
                ("transaction_time".to_owned(), json!(transaction_time)),
                ("event_time".to_owned(), json!(event_time)),
            ]);
            events.push(self.source_event(
                DEPTH_SOURCE_ID,
                event_time.clone(),
                record.received_at.clone(),
                values,
            )?);
        }
        Ok(IngestOutcome { events, notices })
    }

    /// The per-source `sequence` counter for `source_id`, or `None` when this
    /// adapter does not own that identity. The REST identities live on the REST
    /// state so one counter set covers every USD-M source.
    fn sequence_slot(&mut self, source_id: &str) -> Option<&mut u64> {
        match source_id {
            DEPTH_SOURCE_ID => Some(&mut self.next_depth_sequence),
            other => self.rest.sequence_slot(other),
        }
    }

    pub(crate) fn source_event(
        &mut self,
        source_id: &'static str,
        event_time: String,
        received_at: String,
        values: BTreeMap<String, Value>,
    ) -> Result<SourceEvent, AdapterError> {
        let sequence = self.sequence_slot(source_id).ok_or_else(|| {
            AdapterError::InvalidRecord("unknown Binance USD-M source identity".to_owned())
        })?;
        let current = *sequence;
        *sequence = sequence
            .checked_add(1)
            .ok_or(AdapterError::SequenceExhausted)?;
        Ok(SourceEvent {
            schema_version: 1,
            source_id: source_id.to_owned(),
            source_version: SOURCE_VERSION.to_owned(),
            publisher_epoch: self.publisher_epoch.clone(),
            sequence: current,
            event_time,
            received_at,
            values,
        })
    }

    pub(crate) fn rest_state(&mut self) -> &mut crate::binance_usdm_rest::BinanceUsdmRestState {
        &mut self.rest
    }
}

impl BinanceUsdmConnection {
    /// Stable token naming this connection in notices and errors.
    pub(crate) const fn token(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Rest => "rest",
        }
    }
}

impl FeedAdapter for BinanceUsdmAdapter {
    fn ingest(&mut self, record: &RawRecord) -> Result<IngestOutcome, AdapterError> {
        match self.guard.observe(record)? {
            GuardAction::Configuration(_) => Err(AdapterError::InvalidRecord(
                "Binance USD-M captures do not accept configuration records".to_owned(),
            )),
            GuardAction::Connected => {
                self.reset_connection_state();
                Ok(IngestOutcome::notice(FeedNotice::info(
                    FeedNoticeCode::Connected,
                    "Binance USD-M WebSocket connected; connection-local cursors reset",
                )))
            }
            GuardAction::Disconnected => {
                self.reset_connection_state();
                Ok(IngestOutcome::notice(FeedNotice::recovery(
                    FeedNoticeCode::Disconnected,
                    "Binance USD-M WebSocket disconnected",
                    RecoveryAction::Reconnect,
                )))
            }
            // On a socket the outbound record is our own SUBSCRIBE frame: it
            // carries no market data and produces nothing on replay. On the REST
            // connection it is the request descriptor, which IS load-bearing —
            // it is what tells the replay which endpoint parser owns the
            // response that follows.
            GuardAction::Outbound => {
                if self.connection == BinanceUsdmConnection::Rest {
                    let payload = record.payload.as_deref().unwrap_or_default();
                    crate::binance_usdm_rest::observe_request(self, payload)
                } else {
                    Ok(IngestOutcome::default())
                }
            }
            // A poller's failed request, recorded by the capture because a 451
            // or a blackholed host produces no body worth keeping and writing
            // nothing would leave a file indistinguishable from a quiet venue.
            // It is operational state, not market data: no event, and a recovery
            // notice carrying what the capture wrote down.
            //
            // Only the REST connection can produce one. A WebSocket capture has
            // no request to fail — its failures are `Disconnected` — so a
            // failure record on either socket is a malformed capture and is
            // refused exactly as every other WebSocket adapter refuses it.
            GuardAction::Failed(detail) => {
                if self.connection == BinanceUsdmConnection::Rest {
                    Ok(IngestOutcome::notice(FeedNotice::recovery(
                        FeedNoticeCode::StateInconsistent,
                        bounded_detail("Binance USD-M request failed", detail),
                        RecoveryAction::Reconnect,
                    )))
                } else {
                    Err(AdapterError::InvalidRecord(
                        "Binance USD-M WebSocket captures do not accept failure records".to_owned(),
                    ))
                }
            }
            GuardAction::Inbound(payload) => self.ingest_payload(record, payload),
        }
    }
}

/// The venue's own ordering key for one USD-M frame, as `(stream, sequence)`.
///
/// Only `depthUpdate` carries one, in its final update id `u`; every other
/// USD-M socket frame yields `None`.
///
/// This is the one place the capture layer looks inside a payload, and it looks
/// at exactly one field. It exists for connection rotation: when a replacement
/// socket is already carrying the stream, a frame drained from the retiring
/// socket at or past the replacement's first sequence is one the replacement
/// has already delivered, and recording it would duplicate an event. A payload
/// that does not parse yields `None` and is kept — dropping data on a parse
/// failure would be the worse error, and the adapter validates it properly on
/// replay.
#[must_use]
pub(crate) fn upstream_sequence(payload: &str) -> Option<(String, u64)> {
    #[derive(Deserialize)]
    struct Envelope {
        #[serde(default)]
        stream: Option<String>,
        #[serde(default)]
        data: Option<Box<RawValue>>,
    }
    #[derive(Deserialize)]
    struct DepthKey {
        #[serde(rename = "u")]
        last_update_id: u64,
    }
    let envelope: Envelope = serde_json::from_str(payload).ok()?;
    let body = envelope.data.as_deref().map_or(payload, RawValue::get);
    // The event type is read FIRST and the key struct chosen from it, so a
    // non-depth frame is never misread through the depth key.
    let event_type = serde_json::from_str::<BinanceUsdmEventType>(body)
        .ok()?
        .event_type?;
    let sequence = match event_type.as_str() {
        "depthUpdate" => serde_json::from_str::<DepthKey>(body).ok()?.last_update_id,
        _ => return None,
    };
    Some((envelope.stream.unwrap_or(event_type), sequence))
}

/// Whether a frame drained from a retiring socket is one the replacement has
/// already delivered.
///
/// True only when both frames belong to the same stream AND that stream carries
/// a venue sequence AND the drained frame is at or past the replacement's first
/// one. Everything else is kept: a frame we cannot prove is a duplicate is
/// data, and the adapter's own cursors are the second line of defence.
#[must_use]
pub(crate) fn frame_is_superseded(
    payload: &str,
    replacement_first: Option<&(String, u64)>,
) -> bool {
    let Some((first_stream, first_sequence)) = replacement_first else {
        return false;
    };
    upstream_sequence(payload)
        .is_some_and(|(stream, sequence)| stream == *first_stream && sequence >= *first_sequence)
}

pub(crate) fn require_symbol(symbol: &str) -> Result<(), AdapterError> {
    if symbol == INSTRUMENT {
        Ok(())
    } else {
        Err(AdapterError::InvalidPayload(format!(
            "unexpected Binance USD-M symbol `{}`",
            bounded_token(symbol)
        )))
    }
}

pub(crate) fn payload_error(error: serde_json::Error) -> AdapterError {
    AdapterError::InvalidPayload(error.to_string())
}

fn bounded_detail(prefix: &str, value: &str) -> String {
    format!("{prefix}: {}", bounded_token(value))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BinanceUsdmEnvelope {
    #[serde(default)]
    stream: Option<String>,
    #[serde(default)]
    data: Option<Box<RawValue>>,
    #[serde(default)]
    id: Option<u64>,
    #[serde(default)]
    #[allow(dead_code)]
    result: Option<Box<RawValue>>,
    #[serde(default)]
    error: Option<Box<RawValue>>,
}

/// The discriminator every USD-M data payload carries. Read first so a payload
/// can be dispatched before it is parsed into its own shape.
#[derive(Deserialize)]
struct BinanceUsdmEventType {
    #[serde(rename = "e", default)]
    event_type: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BinanceUsdmDepth {
    #[serde(rename = "e")]
    #[allow(dead_code)]
    event_type: String,
    #[serde(rename = "E")]
    event_time: i64,
    #[serde(rename = "T")]
    transaction_time: i64,
    #[serde(rename = "s")]
    symbol: String,
    // OBSERVED 2026-09-03, absent from the documented table: the contract PAIR
    // symbol, "BTCUSDT" on all 36,921 frames of the observed hour. On a
    // perpetual it equals `s`; on a dated future it would not, which is why the
    // venue carries both. Accepted and not carried — a field that is a copy of
    // one we already publish is not a second fact.
    #[serde(rename = "ps")]
    #[allow(dead_code)]
    pair_symbol: String,
    // OBSERVED 2026-09-03, absent from the documented table: integer 1 on every
    // frame of the observed hour. With one observed value its meaning cannot be
    // read off the tape, so it is accepted and not carried rather than published
    // under a guessed name.
    #[serde(rename = "st")]
    #[allow(dead_code)]
    stream_type: i64,
    #[serde(rename = "U")]
    first_update_id: u64,
    #[serde(rename = "u")]
    last_update_id: u64,
    // SIGNED, on observation: the first frame of a connection carries `-1`,
    // the venue's "no predecessor" sentinel. Typed `u64` this failed to
    // deserialize the opening frame of every capture.
    #[serde(rename = "pu")]
    previous_final_update_id: i64,
    #[serde(rename = "b")]
    bids: Vec<[String; 2]>,
    #[serde(rename = "a")]
    asks: Vec<[String; 2]>,
}

#[cfg(test)]
mod tests {
    use crate::{CaptureStream, FeedAdapter, FeedNoticeCode, Provider, RawRecord, RecordKind};

    use super::{BinanceUsdmAdapter, BinanceUsdmConnection};

    // Every fixture below is SYNTHETIC. No Binance USD-M frame has been observed
    // (ruling R1), so these payloads assert the contract this adapter was built
    // to from `A3-binance-usdm-feeds.md` §4 — not a shape anybody has seen. When
    // the Tokyo capture lands, these fixtures are replaced by frames derived from
    // it and any disagreement is resolved in the venue's favour.

    fn record(
        stream: CaptureStream,
        ordinal: u64,
        kind: RecordKind,
        payload: Option<&str>,
    ) -> RawRecord {
        RawRecord {
            schema_version: 1,
            provider: Provider::BinanceUsdm,
            stream,
            connection_epoch: 1,
            ordinal,
            received_at: "2026-09-03T04:00:01.500Z".to_owned(),
            capture_elapsed_ns: None,
            connect_started_elapsed_ns: None,
            kind,
            payload: payload.map(str::to_owned),
        }
    }

    fn connected(stream: CaptureStream, ordinal: u64, epoch: u64) -> RawRecord {
        RawRecord {
            schema_version: 1,
            provider: Provider::BinanceUsdm,
            stream,
            connection_epoch: epoch,
            ordinal,
            received_at: "2026-09-03T04:00:00Z".to_owned(),
            capture_elapsed_ns: None,
            connect_started_elapsed_ns: None,
            kind: RecordKind::Connected,
            payload: None,
        }
    }

    fn public(ordinal: u64, payload: &str) -> RawRecord {
        record(
            CaptureStream::BinanceUsdmPublic,
            ordinal,
            RecordKind::InboundText,
            Some(payload),
        )
    }

    fn public_adapter() -> BinanceUsdmAdapter {
        let mut adapter = BinanceUsdmAdapter::new("test-epoch", BinanceUsdmConnection::Public)
            .expect("adapter constructs");
        adapter
            .ingest(&connected(CaptureStream::BinanceUsdmPublic, 1, 1))
            .expect("connects");
        adapter
    }

    fn depth(last_update_id: u64, previous_final: u64, bids: &str, asks: &str) -> String {
        format!(
            r#"{{"stream":"btcusdt@depth20@100ms","data":{{"e":"depthUpdate","E":1788440000500,"T":1788440000480,"s":"BTCUSDT","ps":"BTCUSDT","U":{},"u":{last_update_id},"pu":{previous_final},"st":1,"b":{bids},"a":{asks}}}}}"#,
            last_update_id.saturating_sub(4)
        )
    }

    #[test]
    fn depth20_is_full_replace_snapshot_rows() {
        let mut adapter = public_adapter();
        let outcome = adapter
            .ingest(&public(
                2,
                &depth(
                    880_100,
                    880_096,
                    r#"[["66120.10","1.50000000"],["66120.00","0.25000000"]]"#,
                    r#"[["66121.30","0.80000000"]]"#,
                ),
            ))
            .expect("depth parses");
        assert_eq!(outcome.events.len(), 3);
        assert!(outcome.notices.is_empty());
        let group = outcome.events[0].values["update_group"].clone();
        for (index, event) in outcome.events.iter().enumerate() {
            assert_eq!(event.source_id, "binance.usdm.btcusdt.book-levels");
            assert_eq!(event.values["update_group"], group);
            assert_eq!(event.values["update_count"], serde_json::json!(3_u32));
            assert_eq!(
                event.values["update_index"],
                serde_json::json!(u32::try_from(index).expect("index fits"))
            );
            // Every message is a complete image, so no row is ever a delta.
            assert_eq!(event.values["update_kind"], serde_json::json!("snapshot"));
            // Unlike the spot stream, USD-M depth carries a venue clock.
            assert_eq!(
                event.values["event_time_basis"],
                serde_json::json!("source")
            );
            assert_eq!(event.event_time, "2026-09-03T12:53:20.500Z");
            assert_eq!(
                event.values["transaction_time"],
                serde_json::json!("2026-09-03T12:53:20.480Z")
            );
            assert_eq!(
                event.values["last_update_id"],
                serde_json::json!(880_100_u64)
            );
            assert_eq!(
                event.values["previous_final_update_id"],
                serde_json::json!(880_096_u64)
            );
        }
        // Bids come first, then asks, in the venue's own order.
        assert_eq!(outcome.events[0].values["side"], serde_json::json!("bid"));
        assert_eq!(outcome.events[1].values["side"], serde_json::json!("bid"));
        assert_eq!(outcome.events[2].values["side"], serde_json::json!("ask"));
        assert_eq!(
            outcome.events[0].values["price"],
            serde_json::json!("66120.10")
        );
        assert_eq!(
            outcome.events[2].values["size"],
            serde_json::json!("0.80000000")
        );
    }

    /// The shape of the observed hour (2026-09-03 08:08Z–09:13Z, US host,
    /// 36,921 frames), with synthetic values.
    ///
    /// Three facts here were NOT in the documented table this adapter was first
    /// built to, and each one broke it: the pair symbol `ps`, the constant `st`,
    /// and a NEGATIVE `pu` — the venue's resync marker, seen once in the hour,
    /// right after its longest stall, with 25,030 update ids skipped. The
    /// first two were unknown fields on a `deny_unknown_fields` struct; the
    /// third did not fit `u64`.
    #[test]
    fn the_observed_depth_frame_shape_parses_including_its_undocumented_fields() {
        let mut adapter = public_adapter();
        // A normally chained frame first, so the resync marker below has a
        // predecessor to be measured against (the audit of the first version
        // of this test found it fed the marker first, which never exercised the
        // chain check at all).
        let prior = r#"{"stream":"btcusdt@depth20@100ms","data":{"e":"depthUpdate","E":1788422920000,"T":1788422919999,"s":"BTCUSDT","ps":"BTCUSDT","U":11462436080000,"u":11462436085455,"pu":11462436079999,"st":1,"b":[["77940.70","1.000"]],"a":[["77940.90","0.100"]]}}"#;
        let opening_chain = adapter
            .ingest(&public(2, prior))
            .expect("a chained frame parses");
        assert!(opening_chain.notices.is_empty());
        // `pu: -1` is the venue's resync marker; here 12 ids (…85456 … …85467)
        // were never delivered, and the frame says so.
        let opening = r#"{"stream":"btcusdt@depth20@100ms","data":{"e":"depthUpdate","E":1788422925587,"T":1788422925586,"s":"BTCUSDT","ps":"BTCUSDT","U":11462436085468,"u":11462436096974,"pu":-1,"st":1,"b":[["77940.80","6.650"]],"a":[["77941.00","0.211"]]}}"#;
        let first = adapter
            .ingest(&public(3, opening))
            .expect("the resync frame parses");
        assert_eq!(
            first.events.len(),
            2,
            "a full-replace image is still published"
        );
        assert_eq!(first.notices.len(), 1, "{:?}", first.notices);
        assert_eq!(first.notices[0].code, FeedNoticeCode::StateInconsistent);
        assert!(
            first.notices[0]
                .detail
                .contains("12 update ids not delivered"),
            "{}",
            first.notices[0].detail
        );
        // The update ids observed are ~1.15e13 — comfortably inside u64 and far
        // outside u32, which is why the cursor is not narrowed.
        assert_eq!(
            first.events[0].values["last_update_id"],
            serde_json::json!(11_462_436_096_974_u64)
        );
        assert_eq!(
            first.events[0].values["previous_final_update_id"],
            serde_json::json!(-1_i64)
        );
        // `ps` duplicates `s` on a perpetual and `st` was constant 1 for the
        // whole hour: both are accepted and neither is published.
        assert!(!first.events[0].values.contains_key("pair_symbol"));
        assert!(!first.events[0].values.contains_key("stream_type"));

        // The next frame chains exactly, as all 36,920 others did.
        let second = r#"{"stream":"btcusdt@depth20@100ms","data":{"e":"depthUpdate","E":1788422925689,"T":1788422925688,"s":"BTCUSDT","ps":"BTCUSDT","U":11462436096975,"u":11462436101003,"pu":11462436096974,"st":1,"b":[["77940.90","1.200"]],"a":[["77941.10","0.500"]]}}"#;
        let chained = adapter
            .ingest(&public(4, second))
            .expect("the next frame parses");
        assert_eq!(chained.events.len(), 2);
        assert!(chained.notices.is_empty(), "a chained frame is silent");
        // Observed scales: prices carry two fractional digits, sizes three, and
        // the text is preserved rather than reformatted.
        assert_eq!(
            chained.events[0].values["price"],
            serde_json::json!("77940.90")
        );
        assert_eq!(chained.events[0].values["size"], serde_json::json!("1.200"));
    }

    #[test]
    fn depth_pu_discontinuity_is_informational_not_gap() {
        let mut adapter = public_adapter();
        adapter
            .ingest(&public(
                2,
                &depth(
                    880_100,
                    0,
                    r#"[["66120.10","1.0"]]"#,
                    r#"[["66121.30","1.0"]]"#,
                ),
            ))
            .expect("first admits");
        let broken = adapter
            .ingest(&public(
                3,
                &depth(
                    880_140,
                    880_120,
                    r#"[["66120.20","1.0"]]"#,
                    r#"[["66121.40","1.0"]]"#,
                ),
            ))
            .expect("a broken chain still applies");
        // A full-replace image loses no state, so a broken `pu` chain is
        // informational — never a Gap, which would pause publication.
        assert_eq!(broken.events.len(), 2);
        assert_eq!(broken.notices.len(), 1);
        assert_eq!(broken.notices[0].code, FeedNoticeCode::StateInconsistent);
        assert!(broken.notices[0].recovery.is_none());
    }

    #[test]
    fn depth_duplicate_and_out_of_order_update_ids_are_dropped() {
        let mut adapter = public_adapter();
        adapter
            .ingest(&public(
                2,
                &depth(
                    880_100,
                    0,
                    r#"[["66120.10","1.0"]]"#,
                    r#"[["66121.30","1.0"]]"#,
                ),
            ))
            .expect("first admits");
        let duplicate = adapter
            .ingest(&public(
                3,
                &depth(
                    880_100,
                    880_100,
                    r#"[["66120.10","1.0"]]"#,
                    r#"[["66121.30","1.0"]]"#,
                ),
            ))
            .expect("duplicate is a notice");
        assert!(duplicate.events.is_empty());
        assert_eq!(duplicate.notices[0].code, FeedNoticeCode::Duplicate);
        let stale = adapter
            .ingest(&public(
                4,
                &depth(
                    880_099,
                    880_095,
                    r#"[["66120.10","1.0"]]"#,
                    r#"[["66121.30","1.0"]]"#,
                ),
            ))
            .expect("out-of-order is a notice");
        assert!(stale.events.is_empty());
        assert_eq!(stale.notices[0].code, FeedNoticeCode::OutOfOrder);
    }

    #[test]
    fn depth_zero_rows_skips_but_advances_the_cursor() {
        let mut adapter = public_adapter();
        // Establish a cursor at 900.
        adapter
            .ingest(&public(
                2,
                &depth(
                    900,
                    896,
                    r#"[["66120.10","1.0"]]"#,
                    r#"[["66121.30","1.0"]]"#,
                ),
            ))
            .expect("first populated image applies");
        // An empty book skips normalized output but still advances the cursor to
        // 1000 and chains against the previous id (pu 900 == cursor 900), so its
        // only notice is the no-levels one.
        let empty = adapter
            .ingest(&public(3, &depth(1000, 900, "[]", "[]")))
            .expect("an empty book is a degenerate state, not a parse failure");
        assert!(empty.events.is_empty());
        assert_eq!(empty.notices.len(), 1);
        assert_eq!(empty.notices[0].code, FeedNoticeCode::StateInconsistent);
        // The next populated frame chains against the EMPTY frame's id (pu 1000
        // == advanced cursor 1000), so it applies with no spurious "does not
        // chain" notice — the bug the untouched cursor caused.
        let chained = adapter
            .ingest(&public(
                4,
                &depth(
                    1010,
                    1000,
                    r#"[["66120.20","1.0"]]"#,
                    r#"[["66121.40","1.0"]]"#,
                ),
            ))
            .expect("a chained populated image applies");
        assert_eq!(chained.events.len(), 2);
        assert!(
            chained.notices.is_empty(),
            "a frame chaining to the empty frame's id must be silent"
        );
    }

    #[test]
    fn depth_over_4096_rows_is_invalid_payload() {
        let mut adapter = public_adapter();
        let level = r#"["66120.10","1.0"]"#;
        let rows = vec![level; 4_097].join(",");
        assert!(
            adapter
                .ingest(&public(2, &depth(880_100, 0, &format!("[{rows}]"), "[]")))
                .is_err()
        );
    }

    #[test]
    fn depth_rejects_first_update_id_above_last() {
        let mut adapter = public_adapter();
        let payload = r#"{"stream":"btcusdt@depth20@100ms","data":{"e":"depthUpdate","E":1788440000500,"T":1788440000480,"s":"BTCUSDT","ps":"BTCUSDT","U":880200,"u":880100,"pu":880096,"st":1,"b":[["66120.10","1.0"]],"a":[]}}"#;
        assert!(adapter.ingest(&public(2, payload)).is_err());
    }

    #[test]
    fn unknown_stream_on_connection_is_unsupported_not_normalized() {
        // Depth is the only stream the `public` connection compiles. A frame
        // naming any other stream is retained and reported, NEVER normalized
        // under `…book-levels`: the connection kind fixes the emittable source
        // ids.
        let mut public_only = public_adapter();
        let foreign = public_only
            .ingest(&public(
                2,
                r#"{"stream":"btcusdt@markPrice@1s","data":{"e":"markPriceUpdate","E":1788440000000,"s":"BTCUSDT","p":"66150.20","i":"66148.90","P":"0","r":"-0.00012500","T":1788480000000}}"#,
            ))
            .expect("a foreign stream is a notice, not an error");
        assert!(foreign.events.is_empty());
        assert_eq!(foreign.notices[0].code, FeedNoticeCode::UnsupportedMessage);

        // A stream name the connection does not compile is reported too.
        let unknown = public_only
            .ingest(&public(
                3,
                r#"{"stream":"btcusdt@kline_1m","data":{"e":"kline"}}"#,
            ))
            .expect("a notice, not an error");
        assert!(unknown.events.is_empty());
        assert_eq!(unknown.notices[0].code, FeedNoticeCode::UnsupportedMessage);
    }

    #[test]
    fn wrapper_edges_are_classified_without_events() {
        let mut adapter = public_adapter();
        let ack = adapter
            .ingest(&public(2, r#"{"result":null,"id":1}"#))
            .expect("ack is operational state");
        assert!(ack.events.is_empty());
        assert_eq!(ack.notices[0].code, FeedNoticeCode::SubscriptionAccepted);

        let rejected = adapter
            .ingest(&public(
                3,
                r#"{"error":{"code":2,"msg":"Invalid request"},"id":2}"#,
            ))
            .expect("a refusal is operational state");
        assert!(rejected.events.is_empty());
        assert_eq!(
            rejected.notices[0].code,
            FeedNoticeCode::SubscriptionRejected
        );

        assert!(
            adapter.ingest(&public(4, "{not json")).is_err(),
            "malformed JSON fails closed"
        );
    }

    #[test]
    fn a_bare_payload_without_the_combined_wrapper_still_dispatches() {
        // Probe P1's fail-closed default is one connection per stream name,
        // whose frames carry NO combined wrapper. Both shapes are accepted so
        // the probe's answer changes no wiring.
        let mut adapter = public_adapter();
        let bare = r#"{"e":"depthUpdate","E":1788440000500,"T":1788440000480,"s":"BTCUSDT","ps":"BTCUSDT","U":880096,"u":880100,"pu":880096,"st":1,"b":[["66120.10","1.50000000"]],"a":[["66121.30","0.80000000"]]}"#;
        let outcome = adapter.ingest(&public(2, bare)).expect("bare frame parses");
        assert_eq!(outcome.events.len(), 2);
        assert_eq!(
            outcome.events[0].source_id,
            "binance.usdm.btcusdt.book-levels"
        );
    }

    #[test]
    fn an_unknown_field_fails_closed() {
        // Every wire struct denies unknown fields: a venue-side addition must be
        // modelled deliberately, never absorbed silently.
        let mut adapter = public_adapter();
        let extra = r#"{"stream":"btcusdt@depth20@100ms","data":{"e":"depthUpdate","E":1788440000500,"T":1788440000480,"s":"BTCUSDT","ps":"BTCUSDT","U":880096,"u":880100,"pu":880096,"st":1,"b":[["66120.10","1.0"]],"a":[["66121.30","1.0"]],"x":"new"}}"#;
        assert!(adapter.ingest(&extra_record(extra)).is_err());
        // A MISSING required field fails the same way.
        let missing = r#"{"stream":"btcusdt@depth20@100ms","data":{"e":"depthUpdate","E":1788440000500,"T":1788440000480,"s":"BTCUSDT","ps":"BTCUSDT","U":880096,"u":880100,"pu":880096,"st":1,"b":[["66120.10","1.0"]]}}"#;
        let mut second = public_adapter();
        assert!(second.ingest(&extra_record(missing)).is_err());
    }

    fn extra_record(payload: &str) -> RawRecord {
        public(2, payload)
    }

    #[test]
    fn configuration_records_are_refused() {
        let mut adapter = BinanceUsdmAdapter::new("test-epoch", BinanceUsdmConnection::Public)
            .expect("adapter constructs");
        let mut configuration = record(
            CaptureStream::BinanceUsdmPublic,
            1,
            RecordKind::Configuration,
            Some("{}"),
        );
        configuration.schema_version = 3;
        configuration.capture_elapsed_ns = Some(0);
        assert!(adapter.ingest(&configuration).is_err());
    }

    /// The rotation drain's dedup key, and the honest limits of it.
    #[test]
    fn upstream_sequence_reads_only_the_streams_that_have_one() {
        use super::{frame_is_superseded, upstream_sequence};

        let depth = depth(880_100, 880_096, r#"[["66120.10","1.0"]]"#, "[]");
        assert_eq!(
            upstream_sequence(&depth),
            Some(("btcusdt@depth20@100ms".to_owned(), 880_100))
        );
        // Depth is the only USD-M socket stream that carries a venue sequence;
        // anything else — a mark-price-shaped frame, or an unparseable one —
        // yields None.
        let mark = r#"{"stream":"btcusdt@markPrice@1s","data":{"e":"markPriceUpdate","E":1788440000000,"s":"BTCUSDT","p":"66150.20","i":"66148.90","P":"0","r":"-0.00012500","T":1788480000000}}"#;
        assert_eq!(upstream_sequence(mark), None);
        assert_eq!(upstream_sequence("{not json"), None);

        // A drained frame the replacement has already delivered is dropped.
        let first = Some(("btcusdt@depth20@100ms".to_owned(), 880_100));
        assert!(frame_is_superseded(&depth, first.as_ref()));
        assert!(frame_is_superseded(
            &super::tests::depth(880_140, 880_100, r#"[["66120.10","1.0"]]"#, "[]"),
            first.as_ref()
        ));
        // One the replacement does NOT have is kept: that is the gap the drain
        // exists to close.
        assert!(!frame_is_superseded(
            &super::tests::depth(880_099, 880_095, r#"[["66120.10","1.0"]]"#, "[]"),
            first.as_ref()
        ));
        // A sequence-less frame is always kept — the honest limit of the rule.
        assert!(!frame_is_superseded(mark, first.as_ref()));
        // With no first frame to compare against, nothing is dropped.
        assert!(!frame_is_superseded(&depth, None));
    }

    #[test]
    fn replay_is_deterministic_over_a_mixed_fixture_sequence() {
        let fixture = [
            depth(880_100, 880_096, r#"[["66120.10","1.50000000"]]"#, "[]"),
            depth(880_104, 880_100, r#"[["66120.00","0.25000000"]]"#, "[]"),
            depth(880_108, 880_104, "[]", r#"[["66121.40","0.10000000"]]"#),
        ];
        let run = || {
            let mut adapter = public_adapter();
            let mut events = Vec::new();
            for (index, payload) in fixture.iter().enumerate() {
                let ordinal = u64::try_from(index).expect("index fits") + 2;
                events.extend(
                    adapter
                        .ingest(&public(ordinal, payload))
                        .expect("fixture parses")
                        .events,
                );
            }
            events
        };
        assert_eq!(run(), run());
        assert_eq!(run().len(), 3);
    }
}
