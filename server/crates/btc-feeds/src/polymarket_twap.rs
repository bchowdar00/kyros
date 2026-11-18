//! Polymarket RTDS adapter for the Chainlink BTC/USD 60-second TWAP topic.

use std::collections::BTreeMap;

use chrono::{SecondsFormat, TimeZone, Utc};
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::json;
use signals_core::SourceEvent;

use crate::adapter::{GuardAction, RecordGuard, bounded_token, validate_publisher_epoch};
use crate::{
    AdapterError, FeedAdapter, FeedNotice, FeedNoticeCode, IngestOutcome, Provider, RawRecord,
};

/// The one source identity this adapter emits.
pub const SOURCE_ID: &str = "polymarket.rtds.btc-usd.twap-60s";
/// Version of the source definition [`SOURCE_ID`] resolves to.
pub const SOURCE_VERSION: &str = "1.0.0";
pub(crate) const TOPIC: &str = "crypto_prices_twap_sixty";
pub(crate) const SYMBOL_FILTER: &str = r#"{"symbol":"btc/usd"}"#;
const E18_SCALE: u32 = 18;
const MAX_E18_BYTES: usize = 96;

/// Deterministic adapter for Polymarket's keyless Chainlink 60-second TWAP relay.
pub struct PolymarketTwapAdapter {
    publisher_epoch: String,
    guard: RecordGuard,
    saw_subscription: bool,
    currently_subscribed: bool,
    subscription_acknowledged: bool,
    backfill_retained: bool,
    next_sequence: u64,
}

impl PolymarketTwapAdapter {
    /// Construct a Polymarket RTDS Chainlink-TWAP adapter.
    ///
    /// `publisher_epoch` is copied verbatim into every emitted event; it must be
    /// non-blank and at most 128 bytes, or the call returns
    /// [`AdapterError::InvalidRecord`].
    pub fn new(publisher_epoch: impl Into<String>) -> Result<Self, AdapterError> {
        let publisher_epoch = publisher_epoch.into();
        validate_publisher_epoch(&publisher_epoch)?;
        Ok(Self {
            publisher_epoch,
            guard: RecordGuard::new(Provider::Polymarket),
            saw_subscription: false,
            currently_subscribed: false,
            subscription_acknowledged: false,
            backfill_retained: false,
            next_sequence: 0,
        })
    }

    /// Assert the capture was complete. Call once, after the last record.
    ///
    /// Returns [`AdapterError::InvalidRecord`] if the exact RTDS subscription
    /// was never recorded, without which the capture is not evidence that the
    /// TWAP topic was subscribed at all.
    pub fn finish(&self) -> Result<(), AdapterError> {
        if !self.saw_subscription {
            return Err(AdapterError::InvalidRecord(
                "Polymarket TWAP capture is missing its exact subscription".to_owned(),
            ));
        }
        Ok(())
    }

    fn outbound(&mut self, payload: &str) -> Result<IngestOutcome, AdapterError> {
        if payload == "PING" {
            return Ok(IngestOutcome::default());
        }
        let subscription: Subscription = serde_json::from_str(payload).map_err(payload_error)?;
        let [entry] = subscription.subscriptions.as_slice() else {
            return Err(AdapterError::InvalidPayload(
                "Polymarket TWAP subscription must contain exactly one entry".to_owned(),
            ));
        };
        if subscription.action != "subscribe"
            || entry.topic != TOPIC
            || entry.message_type != "update"
            || entry.filters != SYMBOL_FILTER
        {
            return Err(AdapterError::InvalidPayload(
                "Polymarket TWAP subscription does not pin the 60-second btc/usd stream".to_owned(),
            ));
        }
        if self.currently_subscribed {
            return Err(AdapterError::InvalidRecord(
                "Polymarket TWAP connection contains multiple subscriptions".to_owned(),
            ));
        }
        self.saw_subscription = true;
        self.currently_subscribed = true;
        Ok(IngestOutcome::default())
    }

    fn inbound(
        &mut self,
        record: &RawRecord,
        payload: &str,
    ) -> Result<IngestOutcome, AdapterError> {
        if payload == "PONG" {
            return Ok(IngestOutcome::notice(FeedNotice::info(
                FeedNoticeCode::Heartbeat,
                "Polymarket RTDS literal-text heartbeat acknowledged",
            )));
        }
        if !self.currently_subscribed {
            return Err(AdapterError::InvalidRecord(
                "Polymarket TWAP update precedes subscription".to_owned(),
            ));
        }
        // The live RTDS endpoint acknowledges a successful subscription with one empty text frame.
        // It is retained verbatim in capture and interpreted here instead of discarded in transport.
        if payload.is_empty() {
            if self.subscription_acknowledged {
                return Err(AdapterError::InvalidRecord(
                    "Polymarket RTDS repeated its empty subscription acknowledgement".to_owned(),
                ));
            }
            self.subscription_acknowledged = true;
            return Ok(IngestOutcome::notice(FeedNotice::info(
                FeedNoticeCode::SubscriptionAccepted,
                "Polymarket RTDS acknowledged the exact TWAP subscription",
            )));
        }
        // Observed 2026-09-01: the relay now opens a subscription with one
        // backfill frame carrying ~60 historical TWAP points before the
        // unchanged per-second update frames. The frame is retained verbatim in
        // capture and refused normalization here — its points are history the
        // relay replayed, not observations this capture made, so an opening
        // strike must never be synthesized from them. The first normalized TWAP
        // still comes from the first live `update` frame.
        // Exactly one backfill frame per connection is the observed shape; a
        // second one is an unmodelled relay change, refused rather than retained
        // a second time, exactly as a repeated subscription acknowledgement is.
        if let Some(points) = backfill_points(payload) {
            if self.backfill_retained {
                return Err(AdapterError::InvalidRecord(
                    "Polymarket TWAP connection repeated its backfill frame".to_owned(),
                ));
            }
            self.backfill_retained = true;
            return Ok(IngestOutcome::notice(FeedNotice::info(
                FeedNoticeCode::UnsupportedMessage,
                format!(
                    "retained Polymarket RTDS backfill frame of {points} points without normalized output"
                ),
            )));
        }
        // Ruling E61: classify the frame's shape before the strict update parse.
        // The one modelled message on this stream is the per-second TWAP `update`.
        // A frame that carries no `topic`, or whose top-level `type` is not
        // `update`, is an unsupported RTDS shape — a connection or status
        // acknowledgement, or another auxiliary frame the adapter does not model —
        // and is retained as a counted notice so the stream continues instead of
        // failing closed (the 2026-09-04 production incident: a topic-less frame
        // reached the strict parse below and surfaced as `missing field topic`,
        // darkening the unit). A frame that IS a pinned `update` falls through to
        // the strict parse, where a genuinely invalid modelled value still refuses.
        let shape: FrameShape = serde_json::from_str(payload).map_err(payload_error)?;
        if shape.topic.is_none() || shape.message_type.as_deref() != Some("update") {
            return Ok(IngestOutcome::notice(FeedNotice::info(
                FeedNoticeCode::UnsupportedMessage,
                format!(
                    "retained unsupported Polymarket RTDS message shape (topic `{}`, type `{}`) without normalized output",
                    bounded_token(shape.topic.as_deref().unwrap_or("absent")),
                    bounded_token(shape.message_type.as_deref().unwrap_or("absent")),
                ),
            )));
        }
        let message: TwapMessage = serde_json::from_str(payload).map_err(payload_error)?;
        if message.topic != TOPIC
            || message.message_type != "update"
            || message.payload.symbol != "btc/usd"
            || message.payload.window_s != 60
        {
            return Err(AdapterError::InvalidPayload(
                "Polymarket RTDS message is not the pinned BTC/USD 60-second TWAP".to_owned(),
            ));
        }
        let event_time = timestamp(message.payload.timestamp, "payload.timestamp")?;
        let published_at = timestamp(message.timestamp, "timestamp")?;
        if message.timestamp < message.payload.timestamp {
            return Err(AdapterError::InvalidPayload(
                "Polymarket RTDS publication timestamp precedes its observation".to_owned(),
            ));
        }
        let twap_price = e18_decimal(&message.payload.full_accuracy_value)?;
        if twap_price <= Decimal::ZERO {
            return Err(AdapterError::InvalidPayload(
                "Polymarket Chainlink TWAP must be positive".to_owned(),
            ));
        }
        let sequence = self.next_sequence;
        self.next_sequence = self
            .next_sequence
            .checked_add(1)
            .ok_or(AdapterError::SequenceExhausted)?;
        Ok(IngestOutcome {
            events: vec![SourceEvent {
                schema_version: 1,
                source_id: SOURCE_ID.to_owned(),
                source_version: SOURCE_VERSION.to_owned(),
                publisher_epoch: self.publisher_epoch.clone(),
                sequence,
                event_time: event_time.clone(),
                received_at: record.received_at.clone(),
                values: BTreeMap::from([
                    ("symbol".to_owned(), json!("btc/usd")),
                    ("window_seconds".to_owned(), json!(60)),
                    ("twap_price".to_owned(), json!(twap_price.to_string())),
                    ("event_time".to_owned(), json!(event_time)),
                    ("published_at".to_owned(), json!(published_at)),
                ]),
            }],
            notices: Vec::new(),
        })
    }
}

impl FeedAdapter for PolymarketTwapAdapter {
    fn ingest(&mut self, record: &RawRecord) -> Result<IngestOutcome, AdapterError> {
        match self.guard.observe(record)? {
            GuardAction::Configuration(_) => Err(AdapterError::InvalidRecord(
                "Polymarket TWAP capture does not use a configuration record".to_owned(),
            )),
            GuardAction::Connected => {
                self.currently_subscribed = false;
                self.subscription_acknowledged = false;
                self.backfill_retained = false;
                Ok(IngestOutcome::notice(FeedNotice::info(
                    FeedNoticeCode::Connected,
                    "Polymarket RTDS connected; TWAP awaits an exact subscription",
                )))
            }
            GuardAction::Disconnected => {
                self.currently_subscribed = false;
                self.subscription_acknowledged = false;
                self.backfill_retained = false;
                Ok(IngestOutcome::notice(FeedNotice::info(
                    FeedNoticeCode::Disconnected,
                    "Polymarket RTDS disconnected; no snapshot or history is available",
                )))
            }
            GuardAction::Outbound => self.outbound(record.payload.as_deref().ok_or_else(|| {
                AdapterError::InvalidRecord("missing outbound payload".to_owned())
            })?),
            GuardAction::Failed(_) => Err(AdapterError::InvalidRecord(
                "Polymarket TWAP captures do not accept failure records".to_owned(),
            )),
            GuardAction::Inbound(payload) => self.inbound(record, payload),
        }
    }
}

fn e18_decimal(value: &str) -> Result<Decimal, AdapterError> {
    if value.is_empty()
        || value.len() > MAX_E18_BYTES
        || !value.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(AdapterError::InvalidPayload(
            "Polymarket full_accuracy_value must be a bounded positive E18 integer".to_owned(),
        ));
    }
    let integer = value.parse::<i128>().map_err(|_| {
        AdapterError::InvalidPayload(
            "Polymarket full_accuracy_value exceeds exact decimal range".to_owned(),
        )
    })?;
    Decimal::try_from_i128_with_scale(integer, E18_SCALE)
        .map(|value| value.normalize())
        .map_err(|_| {
            AdapterError::InvalidPayload(
                "Polymarket full_accuracy_value exceeds exact decimal range".to_owned(),
            )
        })
}

/// Recognizes the RTDS backfill frame by its exact observed shape: top-level
/// `topic` pinned to the subscribed topic, top-level `type` of `subscribe`, and
/// a payload that pins the same `btc/usd` 60-second window and carries a `data`
/// array. The array elements are deliberately not validated — the frame is
/// refused normalization, so only its shape and point count are read. Every
/// other frame — including a malformed one — returns `None` and falls through to
/// the strict update parse unchanged, so update-frame strictness is untouched.
fn backfill_points(payload: &str) -> Option<usize> {
    let frame: BackfillFrame = serde_json::from_str(payload).ok()?;
    if frame.topic.as_deref() != Some(TOPIC) || frame.message_type.as_deref() != Some("subscribe") {
        return None;
    }
    let payload = frame.payload?;
    if payload.symbol.as_deref() != Some("btc/usd") || payload.window_s != Some(60) {
        return None;
    }
    payload.data.map(|data| data.len())
}

fn timestamp(millis: i64, field: &str) -> Result<String, AdapterError> {
    Utc.timestamp_millis_opt(millis)
        .single()
        .map(|value| value.to_rfc3339_opts(SecondsFormat::Millis, true))
        .ok_or_else(|| {
            AdapterError::InvalidPayload(format!(
                "Polymarket RTDS {field} is outside the Unix-millisecond range"
            ))
        })
}

fn payload_error(error: impl std::fmt::Display) -> AdapterError {
    AdapterError::InvalidPayload(error.to_string())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Subscription {
    action: String,
    subscriptions: Vec<SubscriptionEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SubscriptionEntry {
    topic: String,
    #[serde(rename = "type")]
    message_type: String,
    filters: String,
}

/// The minimal discriminator peek used to classify a frame's shape before the
/// strict [`TwapMessage`] parse. Both fields are optional so a frame missing the
/// `topic` discriminator, or carrying an unmodelled `type`, is recognized as an
/// unsupported shape rather than surfacing as a strict-deserialisation refusal.
#[derive(Deserialize)]
struct FrameShape {
    topic: Option<String>,
    #[serde(rename = "type")]
    message_type: Option<String>,
}

#[derive(Deserialize)]
struct BackfillFrame {
    topic: Option<String>,
    #[serde(rename = "type")]
    message_type: Option<String>,
    payload: Option<BackfillPayload>,
}

#[derive(Deserialize)]
struct BackfillPayload {
    symbol: Option<String>,
    window_s: Option<u32>,
    data: Option<Vec<serde::de::IgnoredAny>>,
}

#[derive(Deserialize)]
struct TwapMessage {
    topic: String,
    #[serde(rename = "type")]
    message_type: String,
    timestamp: i64,
    payload: TwapPayload,
}

#[derive(Deserialize)]
struct TwapPayload {
    symbol: String,
    full_accuracy_value: String,
    timestamp: i64,
    window_s: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::{CaptureStream, RecordKind};

    const BACKFILL: &str = r#"{"payload":{"data":[{"full_accuracy_value":"65000500000000000000000","timestamp":1786726748000,"value":65000.5}],"symbol":"btc/usd","window_s":60},"timestamp":1786726748675,"topic":"crypto_prices_twap_sixty","type":"subscribe"}"#;
    const SUBSCRIPTION: &str = r#"{"action":"subscribe","subscriptions":[{"filters":"{\"symbol\":\"btc/usd\"}","topic":"crypto_prices_twap_sixty","type":"update"}]}"#;

    fn record(
        connection_epoch: u64,
        ordinal: u64,
        kind: RecordKind,
        payload: Option<&str>,
    ) -> RawRecord {
        RawRecord {
            schema_version: 1,
            provider: Provider::Polymarket,
            stream: CaptureStream::PolymarketChainlinkTwap,
            connection_epoch,
            ordinal,
            received_at: "2026-09-01T00:00:00Z".to_owned(),
            capture_elapsed_ns: None,
            connect_started_elapsed_ns: None,
            kind,
            payload: payload.map(str::to_owned),
        }
    }

    fn subscribed_adapter(ordinals: &mut u64) -> PolymarketTwapAdapter {
        let mut adapter = PolymarketTwapAdapter::new("epoch").expect("adapter");
        for (kind, payload) in [
            (RecordKind::Connected, None),
            (RecordKind::OutboundText, Some(SUBSCRIPTION)),
            (RecordKind::InboundText, Some("")),
        ] {
            *ordinals += 1;
            adapter
                .ingest(&record(1, *ordinals, kind, payload))
                .expect("subscription handshake");
        }
        adapter
    }

    #[test]
    fn a_repeated_backfill_frame_in_one_connection_is_refused() {
        let mut ordinals = 0;
        let mut adapter = subscribed_adapter(&mut ordinals);
        ordinals += 1;
        let first = adapter
            .ingest(&record(
                1,
                ordinals,
                RecordKind::InboundText,
                Some(BACKFILL),
            ))
            .expect("the first backfill frame is retained");
        assert!(first.events.is_empty());
        assert_eq!(first.notices.len(), 1);
        assert_eq!(first.notices[0].code, FeedNoticeCode::UnsupportedMessage);

        ordinals += 1;
        let repeated = adapter.ingest(&record(
            1,
            ordinals,
            RecordKind::InboundText,
            Some(BACKFILL),
        ));
        assert!(matches!(
            repeated,
            Err(AdapterError::InvalidRecord(ref detail))
                if detail == "Polymarket TWAP connection repeated its backfill frame"
        ));

        // The refusal is per connection: a reconnection reopens with its own
        // backfill frame exactly as the empty acknowledgement does.
        for (epoch, kind, payload) in [
            (1, RecordKind::Disconnected, None),
            (2, RecordKind::Connected, None),
            (2, RecordKind::OutboundText, Some(SUBSCRIPTION)),
            (2, RecordKind::InboundText, Some("")),
        ] {
            ordinals += 1;
            adapter
                .ingest(&record(epoch, ordinals, kind, payload))
                .expect("reconnection handshake");
        }
        ordinals += 1;
        assert!(
            adapter
                .ingest(&record(
                    2,
                    ordinals,
                    RecordKind::InboundText,
                    Some(BACKFILL),
                ))
                .is_ok()
        );
    }

    const UPDATE: &str = r#"{"topic":"crypto_prices_twap_sixty","type":"update","timestamp":1786726810045,"payload":{"symbol":"btc/usd","full_accuracy_value":"65000500000000000000000","timestamp":1786726810000,"window_s":60}}"#;

    #[test]
    fn a_topicless_frame_is_an_unsupported_shape_not_a_refusal() {
        // The 2026-09-04 production incident: an auxiliary RTDS frame carrying no
        // `topic` reached the strict update parse and failed with `missing field
        // topic`, exiting the unit. Ruling E61 makes it a counted notice.
        let mut ordinals = 0;
        let mut adapter = subscribed_adapter(&mut ordinals);
        ordinals += 1;
        let outcome = adapter
            .ingest(&record(
                1,
                ordinals,
                RecordKind::InboundText,
                Some(
                    r#"{"type":"connected","connection_id":"c-8f31ab","payload":{"status":"ok"},"timestamp":1786726810045}"#,
                ),
            ))
            .expect("a topic-less frame is an unsupported shape, not an AdapterError");
        assert!(outcome.events.is_empty());
        assert_eq!(outcome.notices.len(), 1);
        assert_eq!(outcome.notices[0].code, FeedNoticeCode::UnsupportedMessage);
    }

    #[test]
    fn an_unsupported_shape_is_a_notice_but_an_invalid_update_refuses() {
        // Ruling E61: an unsupported shape never returns an AdapterError, while a
        // genuinely invalid value in a modelled `update` still fails closed.
        let mut ordinals = 0;
        let mut adapter = subscribed_adapter(&mut ordinals);

        // A pinned `update` with a valid value normalizes as before.
        ordinals += 1;
        let admitted = adapter
            .ingest(&record(1, ordinals, RecordKind::InboundText, Some(UPDATE)))
            .expect("a valid pinned update normalizes");
        assert_eq!(admitted.events.len(), 1);

        // An unmodelled `type` (topic present) is a notice, not a refusal.
        ordinals += 1;
        let notice = adapter
            .ingest(&record(
                1,
                ordinals,
                RecordKind::InboundText,
                Some(
                    r#"{"topic":"crypto_prices_twap_sixty","type":"status","timestamp":1786726810050}"#,
                ),
            ))
            .expect("an unmodelled type is retained without normalized output");
        assert!(notice.events.is_empty());
        assert_eq!(notice.notices[0].code, FeedNoticeCode::UnsupportedMessage);

        // A modelled `update` whose TWAP value is non-positive stays fail-closed.
        ordinals += 1;
        let error = adapter.ingest(&record(
            1,
            ordinals,
            RecordKind::InboundText,
            Some(
                r#"{"topic":"crypto_prices_twap_sixty","type":"update","timestamp":1786726810055,"payload":{"symbol":"btc/usd","full_accuracy_value":"0","timestamp":1786726810000,"window_s":60}}"#,
            ),
        ));
        assert!(matches!(error, Err(AdapterError::InvalidPayload(_))));
    }

    #[test]
    fn exact_e18_conversion_never_uses_the_display_number() {
        assert_eq!(
            e18_decimal("65000500000000000000000")
                .expect("valid E18")
                .to_string(),
            "65000.5"
        );
        assert!(e18_decimal("65000.5").is_err());
        assert!(e18_decimal("-1").is_err());
        assert!(e18_decimal(&i128::MAX.to_string()).is_err());
    }

    #[test]
    fn only_the_pinned_subscribe_data_frame_is_recognized_as_backfill() {
        // The observed backfill key shape, with synthetic values: top-level
        // `topic`/`type`, and a payload pinning btc/usd 60s with a `data` array.
        assert_eq!(
            backfill_points(
                r#"{"payload":{"data":[{"full_accuracy_value":"65000500000000000000000","timestamp":1786726748000,"value":65000.5}],"symbol":"btc/usd","window_s":60},"timestamp":1786726748675,"topic":"crypto_prices_twap_sixty","type":"subscribe"}"#
            ),
            Some(1)
        );
        assert_eq!(
            backfill_points(
                r#"{"payload":{"data":[],"symbol":"btc/usd","window_s":60},"timestamp":1786726748675,"topic":"crypto_prices_twap_sixty","type":"subscribe"}"#
            ),
            Some(0)
        );
        // An update frame, a subscribe frame without a `data` array, a
        // subscribe frame whose payload is not the pinned btc/usd 60s stream, a
        // wrong topic, a non-array `data`, a topicless frame, and a malformed
        // frame all fall through to the strict update parse.
        assert_eq!(
            backfill_points(
                r#"{"connection_id":"c","payload":{"full_accuracy_value":"65000500000000000000000","symbol":"btc/usd","timestamp":1786726810000,"value":65000.5,"window_s":60},"timestamp":1786726810045,"topic":"crypto_prices_twap_sixty","type":"update"}"#
            ),
            None
        );
        assert_eq!(
            backfill_points(
                r#"{"payload":{"symbol":"btc/usd","window_s":60},"timestamp":1786726748675,"topic":"crypto_prices_twap_sixty","type":"subscribe"}"#
            ),
            None
        );
        assert_eq!(
            backfill_points(
                r#"{"payload":{"data":[],"symbol":"eth/usd","window_s":60},"timestamp":1786726748675,"topic":"crypto_prices_twap_sixty","type":"subscribe"}"#
            ),
            None
        );
        assert_eq!(
            backfill_points(
                r#"{"payload":{"data":[],"symbol":"btc/usd","window_s":300},"timestamp":1786726748675,"topic":"crypto_prices_twap_sixty","type":"subscribe"}"#
            ),
            None
        );
        assert_eq!(
            backfill_points(
                r#"{"payload":{"data":[],"symbol":"btc/usd","window_s":60},"timestamp":1786726748675,"topic":"crypto_prices_twap_three_hundred","type":"subscribe"}"#
            ),
            None
        );
        assert_eq!(
            backfill_points(
                r#"{"payload":{"data":{},"symbol":"btc/usd","window_s":60},"timestamp":1786726748675,"topic":"crypto_prices_twap_sixty","type":"subscribe"}"#
            ),
            None
        );
        assert_eq!(
            backfill_points(r#"{"payload":{"data":[],"symbol":"btc/usd","window_s":60}}"#),
            None
        );
        assert_eq!(backfill_points("{"), None);
    }
}
