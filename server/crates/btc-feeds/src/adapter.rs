use chrono::{DateTime, Utc};
use serde::Serialize;
use signals_core::SourceEvent;
use thiserror::Error;

use crate::raw::{Provider, RawRecord, RecordKind};

/// Longest accepted `publisher_epoch`, in bytes.
///
/// The publisher epoch is copied into every emitted [`SourceEvent`], so it is
/// bounded once here rather than per adapter.
pub(crate) const MAX_PUBLISHER_EPOCH_BYTES: usize = 128;

/// Longest accepted provider timestamp token, in bytes.
///
/// RFC 3339 instants are far shorter than this; the cap exists so a hostile
/// payload cannot make an error message carry an unbounded provider string.
pub(crate) const MAX_TIMESTAMP_BYTES: usize = 64;

/// Longest provider token quoted back inside an error or notice detail, in
/// characters (not bytes: truncation is always on a character boundary).
///
/// Sized to the longest identifier any adapter quotes rather than to the
/// shortest: a Kalshi market ticker and an ESPN entity key both have to survive
/// intact, because a truncated identifier in a refusal is an identifier an
/// operator cannot search for.
pub(crate) const MAX_QUOTED_TOKEN_CHARS: usize = 128;

/// A non-market observation emitted by an adapter.
///
/// Notices are how the adapter reports operational state as data instead of as
/// logging: a consumer routes them to its own alerting and, when `recovery` is
/// present, performs the named work before the affected state is trustworthy
/// again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FeedNotice {
    /// Stable machine-readable category to branch on.
    pub code: FeedNoticeCode,
    /// Human-readable explanation. Provider tokens inside it are truncated to
    /// [`MAX_QUOTED_TOKEN_CHARS`]; the text is not part of the contract.
    pub detail: String,
    /// Work the consumer must perform before the affected state can be trusted
    /// again. `None` means the notice is informational only.
    pub recovery: Option<RecoveryAction>,
    /// Which source a subscription result concerns, when this notice is one.
    ///
    /// A consumer that must publish per-source availability needs the identity
    /// as **data**: `detail` is explicitly not part of the contract, so routing
    /// on its wording would couple a lane to prose. Present only on
    /// [`FeedNoticeCode::SubscriptionAccepted`] / [`FeedNoticeCode::SubscriptionRejected`]
    /// notices whose adapter can name the affected source.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subscription: Option<SubscriptionOutcome>,
}

/// The outcome of one subscription request, as data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct SubscriptionOutcome {
    /// The source identity the subscribed channel publishes under. When a
    /// channel feeds more than one identity this names its primary one; the
    /// others are derived downstream.
    pub source_id: &'static str,
    /// True when the venue acknowledged the subscription, false when it
    /// refused it.
    pub accepted: bool,
    /// The venue's refusal code, when it sent one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<i64>,
}

impl FeedNotice {
    pub(crate) fn info(code: FeedNoticeCode, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
            recovery: None,
            subscription: None,
        }
    }

    pub(crate) fn recovery(
        code: FeedNoticeCode,
        detail: impl Into<String>,
        recovery: RecoveryAction,
    ) -> Self {
        Self {
            code,
            detail: detail.into(),
            recovery: Some(recovery),
            subscription: None,
        }
    }

    /// A subscription result that names the source it concerns.
    pub(crate) fn subscription(
        code: FeedNoticeCode,
        detail: impl Into<String>,
        subscription: SubscriptionOutcome,
    ) -> Self {
        Self {
            code,
            detail: detail.into(),
            recovery: None,
            subscription: Some(subscription),
        }
    }
}

/// Stable machine-readable categories for operational feed observations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedNoticeCode {
    /// A new connection epoch opened; connection-local state (books, snapshot
    /// flags) was reset and awaits a fresh venue snapshot.
    Connected,
    /// The connection ended. Everything derived from it is stale until a new
    /// epoch opens.
    Disconnected,
    /// The venue acknowledged a subscription request.
    SubscriptionAccepted,
    /// The venue refused a subscription request; the detail carries the venue's
    /// bounded reason.
    SubscriptionRejected,
    /// A venue keepalive was observed. Some venues (Coinbase) also carry
    /// completeness evidence on this channel, so a heartbeat can be the record
    /// that raises [`FeedNoticeCode::Gap`].
    Heartbeat,
    /// The venue reported its own operational status (trading state, system
    /// status). Informational; it never mutates market state.
    VenueStatus,
    /// A message repeating an already-admitted identity was ignored. No event
    /// is emitted and no state changes.
    Duplicate,
    /// A message older than the admitted frontier was ignored. No event is
    /// emitted and no state changes.
    OutOfOrder,
    /// Messages are provably missing. The adapter fails closed: affected
    /// publication pauses until the accompanying [`RecoveryAction`] completes.
    Gap,
    /// A venue-supplied book checksum disagreed with the locally maintained
    /// book. The book is discarded rather than repaired.
    ChecksumMismatch,
    /// Local state cannot be reconciled with what the venue just asserted, and
    /// the affected state was cleared instead of guessed at.
    StateInconsistent,
    /// An incremental update arrived before the snapshot it applies to. The
    /// update is not applied and no event is emitted.
    AwaitingSnapshot,
    /// A well-formed message of a type this adapter deliberately does not
    /// normalize. It is retained in the capture, but produces no event.
    UnsupportedMessage,
}

pub(crate) enum GuardAction<'a> {
    Configuration(&'a str),
    Connected,
    Disconnected,
    Outbound,
    Inbound(&'a str),
    /// A request that produced no usable response, carrying our own bounded
    /// description of why. Only a poll-based capture stream can produce one: a
    /// WebSocket capture has no request to fail, so every socket adapter — the
    /// USD-M ones included, on their WebSocket connections — refuses it as a
    /// malformed record.
    Failed(&'a str),
}

pub(crate) struct RecordGuard {
    provider: Provider,
    connected: bool,
    connection_epoch: u64,
    last_ordinal: u64,
}

impl RecordGuard {
    pub(crate) const fn new(provider: Provider) -> Self {
        Self {
            provider,
            connected: false,
            connection_epoch: 0,
            last_ordinal: 0,
        }
    }

    pub(crate) fn observe<'a>(
        &mut self,
        record: &'a RawRecord,
    ) -> Result<GuardAction<'a>, AdapterError> {
        record
            .validate()
            .map_err(|error| AdapterError::InvalidRecord(error.to_string()))?;
        if record.provider != self.provider {
            return Err(AdapterError::InvalidRecord(format!(
                "expected provider {}, received {}",
                self.provider.as_str(),
                record.provider.as_str()
            )));
        }
        if record.ordinal <= self.last_ordinal {
            return Err(AdapterError::InvalidRecord(
                "record ordinal must increase strictly".to_owned(),
            ));
        }
        self.last_ordinal = record.ordinal;

        match record.kind {
            RecordKind::Configuration => {
                if self.connected || self.connection_epoch != 0 || record.connection_epoch != 1 {
                    return Err(AdapterError::InvalidRecord(
                        "configuration must be the first connection epoch before any connection"
                            .to_owned(),
                    ));
                }
                let payload = record.payload.as_deref().ok_or_else(|| {
                    AdapterError::InvalidRecord(
                        "configuration record is missing payload".to_owned(),
                    )
                })?;
                Ok(GuardAction::Configuration(payload))
            }
            RecordKind::Connected => {
                if self.connected || record.connection_epoch <= self.connection_epoch {
                    return Err(AdapterError::InvalidRecord(
                        "connected record must begin a new connection epoch".to_owned(),
                    ));
                }
                self.connected = true;
                self.connection_epoch = record.connection_epoch;
                Ok(GuardAction::Connected)
            }
            RecordKind::Failed => {
                if !self.connected || record.connection_epoch != self.connection_epoch {
                    return Err(AdapterError::InvalidRecord(
                        "failure record is outside the active connection epoch".to_owned(),
                    ));
                }
                let payload = record.payload.as_deref().ok_or_else(|| {
                    AdapterError::InvalidRecord("failure record is missing payload".to_owned())
                })?;
                Ok(GuardAction::Failed(payload))
            }
            RecordKind::InboundText | RecordKind::OutboundText => {
                if !self.connected || record.connection_epoch != self.connection_epoch {
                    return Err(AdapterError::InvalidRecord(
                        "text record is outside the active connection epoch".to_owned(),
                    ));
                }
                let payload = record.payload.as_deref().ok_or_else(|| {
                    AdapterError::InvalidRecord("text record is missing payload".to_owned())
                })?;
                if record.kind == RecordKind::InboundText {
                    Ok(GuardAction::Inbound(payload))
                } else {
                    Ok(GuardAction::Outbound)
                }
            }
            RecordKind::Disconnected => {
                if !self.connected || record.connection_epoch != self.connection_epoch {
                    return Err(AdapterError::InvalidRecord(
                        "disconnected record does not match an active connection".to_owned(),
                    ));
                }
                self.connected = false;
                Ok(GuardAction::Disconnected)
            }
        }
    }
}

/// Explicit work required before affected state can be trusted again.
///
/// Every variant is a fail-closed instruction: the adapter has already stopped
/// publishing the affected surface, and will not resume until the named work is
/// done. Ignoring an action does not degrade output quality, it stops that
/// surface's output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryAction {
    /// Open a new connection. Book state is connection-local and only a fresh
    /// venue snapshot restores it.
    Reconnect,
    /// Re-run prediction-market discovery and feed a new Configuration record
    /// to a freshly constructed adapter: the market binding this adapter holds
    /// no longer describes the stream.
    RediscoverPredictionMarket,
    /// Fetch Coinbase trades in `(after_trade_id, through_trade_id]` from REST
    /// and hand them back with
    /// [`CoinbaseAdapter::apply_trade_backfill`](crate::coinbase::CoinbaseAdapter::apply_trade_backfill).
    /// Trade publication stays paused until that call succeeds.
    BackfillCoinbaseTrades {
        /// Venue product the range belongs to (always `BTC-USD` today).
        product_id: String,
        /// Exclusive lower bound: the last trade id the adapter admitted.
        after_trade_id: u64,
        /// Inclusive upper bound: the first trade id proven to exist beyond the
        /// gap.
        through_trade_id: u64,
    },
    /// Resubscribe the Kraken aggregated book for `symbol` at `depth`; the
    /// locally maintained book was discarded and only a fresh snapshot rebuilds
    /// it.
    ResubscribeKrakenBook {
        /// Kraken pair symbol, e.g. `BTC/USD`.
        symbol: String,
        /// Subscription depth the replacement must request, so the replayed and
        /// live books stay comparable.
        depth: u16,
    },
    /// Resubscribe the Kraken level-3 (order-by-order) book for `symbol` at
    /// `depth`, for the same reason as [`RecoveryAction::ResubscribeKrakenBook`].
    ResubscribeKrakenLevel3 {
        /// Kraken pair symbol, e.g. `BTC/USD`.
        symbol: String,
        /// Subscription depth the replacement must request.
        depth: u16,
    },
    /// Fetch CF Benchmarks BRTI prints in `(after_index_time, through_index_time]`
    /// from Kalshi's authenticated REST passthrough and hand them back with
    /// [`KalshiIndexAdapter::apply_history_backfill`](crate::kalshi_index::KalshiIndexAdapter::apply_history_backfill).
    /// The quarter-hour settlement window stays paused until a response covers
    /// the whole interval: every print inside a quarter is load-bearing, so a
    /// lost print may not be silently averaged over.
    BackfillKalshiBrti {
        /// CF Benchmarks index the range belongs to (always `BRTI` today).
        index_id: String,
        /// Exclusive lower bound: the last index time the adapter admitted, in
        /// Unix milliseconds.
        after_index_time: i64,
        /// Inclusive upper bound: the first index time proven to exist beyond
        /// the gap, in Unix milliseconds.
        through_index_time: i64,
    },
    /// Resubscribe the Kalshi book channel for `market_ticker`; the local book
    /// was invalidated.
    ResubscribeKalshiBook {
        /// Exact Kalshi market ticker whose book must be re-snapshotted.
        market_ticker: String,
    },
}

/// Everything produced from one captured record.
///
/// Both fields are frequently empty: most records produce neither an event nor
/// a notice. An empty outcome is normal and is not an error.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct IngestOutcome {
    /// Normalized events, in emission order. Per source id, `sequence` starts
    /// at 0 and increases by exactly one per event for the life of the adapter.
    pub events: Vec<SourceEvent>,
    /// Operational observations produced by the same record, in order.
    pub notices: Vec<FeedNotice>,
}

impl IngestOutcome {
    pub(crate) fn notice(notice: FeedNotice) -> Self {
        Self {
            events: Vec::new(),
            notices: vec![notice],
        }
    }
}

/// One deterministic parser/state machine used for both live input and replay.
pub trait FeedAdapter {
    /// Admit one capture record and return everything it produced.
    ///
    /// Records must arrive in capture order with strictly increasing
    /// `ordinal`s, starting from the stream's first record. The same record
    /// sequence always produces the same events and notices: there is no clock,
    /// no randomness and no network access on this path.
    ///
    /// Returns `Err` only when the record itself, or the provider payload it
    /// carries, is unusable — never for an expected venue condition, which is
    /// reported as a [`FeedNotice`] instead.
    fn ingest(&mut self, record: &RawRecord) -> Result<IngestOutcome, AdapterError>;
}

/// Input or state failure that prevents normal event emission.
#[derive(Debug, Error)]
pub enum AdapterError {
    /// The capture record is malformed or arrived out of the connection
    /// lifecycle the adapter requires (bad epoch/ordinal, wrong provider,
    /// missing payload, unusable constructor argument).
    #[error("invalid capture record: {0}")]
    InvalidRecord(String),
    /// The record was well-formed but the provider payload inside it was not
    /// parseable, violated a venue invariant, or carried a value the adapter
    /// refuses to round or reinterpret.
    #[error("invalid provider payload: {0}")]
    InvalidPayload(String),
    /// A monotonic counter (per-source `sequence`, trade id arithmetic) reached
    /// its maximum. The adapter refuses to wrap.
    #[error("numeric sequence exhausted")]
    SequenceExhausted,
}

/// One rule for the `publisher_epoch` every BTC adapter constructor accepts.
///
/// The epoch is opaque to the crate: it is copied verbatim into every emitted
/// [`SourceEvent`] so a consumer can tell one publisher run from another. It
/// must be non-blank and at most [`MAX_PUBLISHER_EPOCH_BYTES`] bytes.
pub(crate) fn validate_publisher_epoch(publisher_epoch: &str) -> Result<(), AdapterError> {
    if publisher_epoch.trim().is_empty() || publisher_epoch.len() > MAX_PUBLISHER_EPOCH_BYTES {
        return Err(AdapterError::InvalidRecord(
            "publisher_epoch must contain 1 to 128 bytes".to_owned(),
        ));
    }
    Ok(())
}

/// Parse one bounded RFC 3339 provider timestamp into UTC.
///
/// `field` names the provider field for the error message. This is the single
/// timestamp gate for every BTC adapter, so a hostile or malformed instant
/// fails the same way regardless of venue.
pub(crate) fn parse_timestamp(value: &str, field: &str) -> Result<DateTime<Utc>, AdapterError> {
    if value.len() > MAX_TIMESTAMP_BYTES {
        return Err(AdapterError::InvalidPayload(format!(
            "{field} timestamp exceeds {MAX_TIMESTAMP_BYTES} bytes"
        )));
    }
    DateTime::parse_from_rfc3339(value)
        .map(|timestamp| timestamp.with_timezone(&Utc))
        .map_err(|error| AdapterError::InvalidPayload(format!("invalid {field}: {error}")))
}

/// Check that `value` is a bounded RFC 3339 instant, discarding the parse.
pub(crate) fn validate_timestamp(value: &str, field: &str) -> Result<(), AdapterError> {
    parse_timestamp(value, field).map(|_| ())
}

/// Truncate an untrusted provider token to [`MAX_QUOTED_TOKEN_CHARS`] before it
/// is quoted back inside an error or notice detail.
///
/// Truncation is by character, so the result is always valid UTF-8 and a
/// multi-byte token is never cut mid-character.
pub(crate) fn bounded_token(value: &str) -> String {
    value.chars().take(MAX_QUOTED_TOKEN_CHARS).collect()
}
