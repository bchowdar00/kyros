//! Kalshi authenticated index channels: the CF Benchmarks BRTI value feeds and
//! the Pyth BTC/USD relay.
//!
//! One adapter serves the two index channels — `cfbenchmarks_value` (1 Hz raw
//! tick plus its trailing 60-second average and, in the final minute of each
//! quarter hour, the settlement window) and `pyth_value`. They are full-state
//! value streams, not snapshot-plus-delta books, so a sequence gap on a value
//! surface is an observability notice and publication continues from the newest
//! valid image; the settlement window instead fails closed, because every print
//! inside a quarter is load-bearing and a lost print may not be silently
//! averaged over.
//!
//! **The contracts below are observed on in-tree evidence.** The 65-minute
//! `kalshi-index` capture of 2026-09-03 10:01–11:06Z replays through
//! `btc-feeds replay` unmodified — 23,403 records → 23,636 events, zero adapter
//! errors — and covers the whole surface in one hour: 19,497 `…value-5hz`,
//! 3,899 `…value`, 240 `…settlement-window` over four derived closes with four
//! complete windows (rulings E23, E25, E28). The index
//! channels do not live on their own per-channel paths: they are subscribed on
//! the **market socket** [`INDEX_WEBSOCKET_PATH`] with the market command
//! envelope, and the documented per-channel `channel-type` envelope does not
//! connect. `cfbenchmarks_value_5hz`'s availability **varies per connection**
//! (ruling E25): with the same credentials it was answered `error` code 8,
//! "Unknown channel name", on three of four connections of one hour and
//! `subscribed` on the fourth, then accepted on the first subscribe of the next
//! hour and held for its full 65 minutes — refused 3 of 4, then accepted 1 of 1.
//! A refusal says nothing about entitlement. Its source is
//! a reinstated `draft` and its subscription is tolerant — the subscribe goes
//! out on every connection, a refusal is one notice plus an unavailable surface
//! for that connection, and the next `Connected` re-attempts. The cross-channel
//! equality invariant stays withdrawn pending a ruling.
//!
//! `pyth_value` subscribed and acknowledged but has delivered **no frame** in
//! any capture — a little over three hours of acknowledged silence across four
//! of them, including two full in-tree hours — so its cadence and body shape stay
//! UNOBSERVED: the model below is the
//! documented one and the surface is refuse-closed — parsed, ordered and
//! reported, never published — until a real frame is captured. The quarter-hour
//! `last_60s_windowed_average_15min` object was likewise absent from every
//! observed frame (all were mid-quarter), so its conditional presence also stays
//! documented-only; absence is the normal case.
//!
//! The CF Benchmarks REST passthrough is modelled here only as a repair path:
//! [`KalshiIndexAdapter::apply_history_backfill`] closes a settlement-window
//! pause. It emits no [`SourceEvent`], is not a stream-visible source, and is in
//! no package closure (ruling R2).

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;
use serde_json::{Value, json};
use signals_core::SourceEvent;

use reqwest::header::{ACCEPT, CACHE_CONTROL, HeaderName, HeaderValue};
use thiserror::Error;

use crate::adapter::{GuardAction, RecordGuard, bounded_token, validate_publisher_epoch};
use crate::decimal::validate_decimal;
use crate::event_window::millis_to_rfc3339;
use crate::kalshi_auth::{KalshiAuthClient, KalshiAuthError};
use crate::secure_http::{self, SecureHttpError};
use crate::{
    AdapterError, FeedAdapter, FeedNotice, FeedNoticeCode, IngestOutcome, Provider, RawRecord,
    RecoveryAction, SubscriptionOutcome,
};

/// The 1 Hz CF Benchmarks BRTI index value, with its trailing 60-second average.
pub const VALUE_SOURCE_ID: &str = "kalshi.cfbenchmarks.brti.value";
/// The quarter-hour final-minute accumulating average — the settlement quantity.
pub const SETTLEMENT_WINDOW_SOURCE_ID: &str = "kalshi.cfbenchmarks.brti.settlement-window";
/// The 5 Hz CF Benchmarks BRTI index value.
///
/// **Intermittently available** (ruling E25): three of the four connections in
/// the 65-minute R1 hour were answered `error` code 8 on this channel and the
/// fourth was accepted, delivering 2,202 frames at exactly 5 Hz. The
/// subscription is therefore tolerant, not fatal, and is re-attempted on every
/// reconnect.
pub const VALUE_5HZ_SOURCE_ID: &str = "kalshi.cfbenchmarks.brti.value-5hz";
/// The Kalshi relay of the Pyth BTC/USD price.
pub const PYTH_VALUE_SOURCE_ID: &str = "kalshi.pyth.btc-usd.value";
/// The CF Benchmarks REST passthrough repair-row contract. **Repair-only**: no
/// event is ever emitted under this identity.
pub const HISTORY_SOURCE_ID: &str = "kalshi.cfbenchmarks.brti.history";

const SOURCE_VERSION: &str = "1.0.0";

/// Kalshi channel name for the 1 Hz CF Benchmarks value feed.
pub(crate) const VALUE_CHANNEL: &str = "cfbenchmarks_value";
/// Kalshi channel name for the 5 Hz CF Benchmarks value feed.
pub(crate) const VALUE_5HZ_CHANNEL: &str = "cfbenchmarks_value_5hz";
/// Kalshi channel name for the Pyth relay.
pub(crate) const PYTH_CHANNEL: &str = "pyth_value";

/// The one path the index channels are subscribed on: Kalshi's market socket
/// (`wss://external-api-ws.kalshi.com/trade-api/ws/v2`). Observed, ruling E23 —
/// the documented per-channel paths do not connect, and there is therefore one
/// supported endpoint rather than an operator-named one.
pub const INDEX_WEBSOCKET_PATH: &str = "/trade-api/ws/v2";

/// Command `id` of the `cfbenchmarks_value` subscribe.
///
/// The index channels share the market socket, so their command ids are drawn
/// from a reserved band above the market commands'. That band is what lets an
/// `error` reply — which names only the `id` it answers — be attributed to the
/// channel it rejected.
pub const VALUE_COMMAND_ID: u64 = 101;
/// Command `id` of the `pyth_value` subscribe. See [`VALUE_COMMAND_ID`].
pub const PYTH_COMMAND_ID: u64 = 102;
/// Command `id` of the `cfbenchmarks_value_5hz` subscribe. See
/// [`VALUE_COMMAND_ID`]; this one is the reason the band matters, because its
/// reply is routinely an `error` that names only the id.
pub const VALUE_5HZ_COMMAND_ID: u64 = 103;
/// Lowest command id reserved for index-channel subscribes. A `subscribed` or
/// `error` reply below it answers a market command and is not this adapter's.
pub const INDEX_COMMAND_ID_BASE: u64 = 100;

/// The one index id this program configures (ruling R2: BRTI only).
pub const DEFAULT_INDEX_ID: &str = "BRTI";
/// The presumed Pyth underlying-ticker spelling. It is a configured string, not
/// a compiled assumption: the adapter accepts exactly what it was configured
/// with and refuses any other spelling on the wire. The R1 capture confirms it.
pub const DEFAULT_UNDERLYING_TICKER: &str = "Crypto.BTC/USD";

/// Largest accepted index message. The documented tick is ~500 B; this is far
/// under the crate-wide [`crate::MAX_MESSAGE_BYTES`].
pub const MAX_INDEX_MESSAGE_BYTES: usize = 64 * 1024;
/// Largest accepted embedded raw CF Benchmarks frame (the `msg.data` string).
pub const MAX_RAW_FRAME_BYTES: usize = 8 * 1024;
/// Largest configured index-id set.
pub const MAX_CONFIGURED_INDEX_IDS: usize = 8;
/// Largest configured underlying-ticker set.
pub const MAX_CONFIGURED_UNDERLYINGS: usize = 4;
/// Largest accepted REST passthrough body, matching `kalshi::MAX_DISCOVERY_RESPONSE_BYTES`.
pub const MAX_PASSTHROUGH_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
/// Largest accepted repair-row count, the observed `timespan=HOUR` payload shape.
pub const MAX_HISTORY_ROWS: usize = 3_600;
/// Smallest interval between two passthrough requests, in milliseconds. Each
/// request costs 50 tokens from the Read bucket, so 0.2 req/s leaves the bucket
/// for market discovery.
pub const PASSTHROUGH_MIN_INTERVAL_MS: u64 = 5_000;
/// Largest number of passthrough attempts for one gap, bounding a pathological
/// repair loop.
pub const PASSTHROUGH_MAX_PER_GAP: u32 = 12;
/// Largest number of passthrough attempts in one hour. Exceeding it is a
/// terminal misconfiguration, not a slower loop.
pub const PASSTHROUGH_MAX_PER_HOUR: u32 = 60;

/// Milliseconds in a quarter hour, the settlement-window close cadence.
const QUARTER_HOUR_MILLIS: i64 = 15 * 60 * 1_000;
/// The accumulation span of both the trailing and the quarter-hour average.
const AVERAGE_WINDOW_MILLIS: i64 = 60_000;
/// Largest accepted trailing-average window size, in ticks. Observed: the
/// window opens at **0** on a fresh subscription and grows to 60.
const MAX_TRAILING_WINDOW_SIZE: u32 = 600;
/// Largest accepted quarter-hour window size, in ticks (`:01`…`:59` plus close).
const MAX_QUARTER_WINDOW_SIZE: u32 = 60;
/// Exact fractional-digit count Kalshi documents for the averaged and relayed
/// decimal fields. The raw tick value is free-form within [`MAX_VALUE_SCALE`].
const EXACT_VALUE_SCALE: usize = 8;
/// Largest accepted fractional-digit count on the raw tick value.
const MAX_VALUE_SCALE: usize = 8;
/// Largest accepted decimal token, in bytes.
const MAX_VALUE_BYTES: usize = 40;
/// Largest accepted index-id token, in bytes.
const MAX_INDEX_ID_BYTES: usize = 32;
/// Largest accepted underlying-ticker token, in bytes.
const MAX_UNDERLYING_TICKER_BYTES: usize = 64;

/// Which of the two subscribed channels a message belongs to.
///
/// Each carries its own `sid` binding, its own `seq` cursor and its own
/// frontier, so the dispatch is a closed enum rather than a match over strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum IndexChannel {
    /// `cfbenchmarks_value` — emits `…brti.value` and, in the final minute of a
    /// quarter, `…brti.settlement-window`.
    Value,
    /// `cfbenchmarks_value_5hz` — emits `…brti.value-5hz`. Lean raw ticks with
    /// no trailing average and no quarter object.
    Value5Hz,
    /// `pyth_value` — models `kalshi.pyth.btc-usd.value`, which publishes
    /// nothing while its contract is unobserved.
    Pyth,
}

impl IndexChannel {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Value => VALUE_CHANNEL,
            Self::Value5Hz => VALUE_5HZ_CHANNEL,
            Self::Pyth => PYTH_CHANNEL,
        }
    }

    fn from_channel_name(name: &str) -> Option<Self> {
        match name {
            VALUE_CHANNEL => Some(Self::Value),
            VALUE_5HZ_CHANNEL => Some(Self::Value5Hz),
            PYTH_CHANNEL => Some(Self::Pyth),
            _ => None,
        }
    }

    /// The channel a reserved subscribe-command id addresses. `None` for a
    /// market command, whose replies belong to the market adapter.
    const fn from_command_id(id: u64) -> Option<Self> {
        match id {
            VALUE_COMMAND_ID => Some(Self::Value),
            VALUE_5HZ_COMMAND_ID => Some(Self::Value5Hz),
            PYTH_COMMAND_ID => Some(Self::Pyth),
            _ => None,
        }
    }

    /// The source identity this channel publishes under. `Value` also drives
    /// `…settlement-window`, which is derived from the same frames downstream.
    const fn source_id(self) -> &'static str {
        match self {
            Self::Value => VALUE_SOURCE_ID,
            Self::Value5Hz => VALUE_5HZ_SOURCE_ID,
            Self::Pyth => PYTH_VALUE_SOURCE_ID,
        }
    }

    /// The subscribe-command id this channel is requested under.
    pub(crate) const fn command_id(self) -> u64 {
        match self {
            Self::Value => VALUE_COMMAND_ID,
            Self::Value5Hz => VALUE_5HZ_COMMAND_ID,
            Self::Pyth => PYTH_COMMAND_ID,
        }
    }
}

/// Connection-local subscription state for one channel.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct ChannelCursor {
    /// The `sid` this channel was acknowledged on. `None` until an ack arrives.
    sid: Option<u64>,
    /// The next `seq` expected on that `sid`. `None` until the first message.
    expected_seq: Option<u64>,
}

/// A settlement-window interval the venue proved exists but the socket never
/// delivered. While one is pending the surface publishes nothing, and only a
/// passthrough repair covering the exact interval resumes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingSettlementBackfill {
    /// The index the pause is scoped to.
    pub index_id: String,
    /// The quarter whose settlement window is withheld, named by its **derived
    /// close** (ruling E28): the first `:00/:15/:30/:45` boundary at or after
    /// [`Self::through_index_time`], which is the same instant the quarter
    /// object's `window_start_ts_ms + 60_000` derives. A consumer checkpoints
    /// this, so a restart mid-recovery withholds the same quarter the adapter
    /// was withholding.
    pub quarter_close_time: i64,
    /// Exclusive lower bound: the last index time admitted on the value channel.
    pub after_index_time: i64,
    /// Inclusive upper bound: the first index time proven to exist beyond the gap.
    pub through_index_time: i64,
}

impl PendingSettlementBackfill {
    /// True when no repair can close this pause because the gap opened before
    /// any value-channel frontier existed, leaving an empty `(t, t]` interval.
    /// Only a reconnect — which re-seeds the frontier and clears the pause —
    /// resumes the surface (audit A1/F2).
    #[must_use]
    pub const fn is_unrepairable(&self) -> bool {
        self.after_index_time >= self.through_index_time
    }
}

/// The first quarter close at or after `index_time`.
///
/// This is the same derived close the quarter object carries — the grid
/// boundary one minute after a window's fixed start (ruling E28) — so a pause
/// opened by a gap and a window published by a tick name the quarter
/// identically.
const fn quarter_close_at_or_after(index_time: i64) -> i64 {
    let remainder = index_time.rem_euclid(QUARTER_HOUR_MILLIS);
    if remainder == 0 {
        index_time
    } else {
        index_time + (QUARTER_HOUR_MILLIS - remainder)
    }
}

/// One row of a CF Benchmarks REST passthrough history response.
///
/// This is the repair-row contract of `kalshi.cfbenchmarks.brti.history`. Rows
/// are never published: they close a pause, they do not become events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KalshiBrtiHistoryRow {
    /// CF Benchmarks index time, in Unix milliseconds.
    pub time: i64,
    /// Exact decimal index value text. Preserved verbatim; never rounded.
    pub value: String,
}

/// Passthrough request budget.
///
/// The adapter reads no clock, so the hourly window is advanced by the caller —
/// which owns the clock — through [`PassthroughBudget::note_hour_rollover`].
/// The per-gap counter resets when a pause is opened or closed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct PassthroughBudget {
    in_gap: u32,
    in_hour: u32,
}

/// Deterministic parser and ordering state for the Kalshi index socket.
///
/// One adapter serves one socket carrying all three index channels. Bounded,
/// fail-closed, notices rather than logs: no clock is read, no network call is
/// made, and the same records always produce the same events.
pub struct KalshiIndexAdapter {
    publisher_epoch: String,
    guard: RecordGuard,
    /// Configured index ids, byte-sorted and unique. A wire `index_id` outside
    /// this set is refused rather than normalized under a foreign identity.
    index_ids: Vec<String>,
    /// Configured Pyth underlying tickers, byte-sorted and unique.
    underlying_tickers: Vec<String>,
    /// Connection-local per-channel `sid`/`seq` state.
    cursors: BTreeMap<IndexChannel, ChannelCursor>,
    /// `(channel, index_id | underlying_ticker)` → last accepted index time and
    /// its verbatim value text. Survives reconnects within a publisher epoch.
    frontiers: BTreeMap<(IndexChannel, String), (i64, String)>,
    /// The last accepted quarter-hour window size per `quarter_close_time`.
    quarter_window_sizes: BTreeMap<i64, u32>,
    /// The pending settlement-window repair, if any.
    settlement_backfill: Option<PendingSettlementBackfill>,
    passthrough: PassthroughBudget,
    /// Channels the venue refused on this connection. Connection-local: every
    /// reconnect re-sends the whole subscribe set, so a channel refused once is
    /// re-attempted (ruling E25).
    rejected: BTreeSet<IndexChannel>,
    next_value_sequence: u64,
    next_settlement_sequence: u64,
    next_value_5hz_sequence: u64,
}

impl KalshiIndexAdapter {
    /// Construct an adapter for the configured index ids and Pyth underlying
    /// tickers.
    ///
    /// `publisher_epoch` is copied verbatim into every emitted event; it must be
    /// non-blank and at most 128 bytes. The two configured sets are the closed
    /// vocabularies the wire is checked against: they must be non-empty, within
    /// [`MAX_CONFIGURED_INDEX_IDS`] / [`MAX_CONFIGURED_UNDERLYINGS`], and carry
    /// no blank or over-long token. There is no default — the caller states what
    /// it subscribed, so a message for anything else is refused rather than
    /// silently accepted under a name nobody asked for.
    pub fn new(
        publisher_epoch: impl Into<String>,
        index_ids: &[&str],
        underlying_tickers: &[&str],
    ) -> Result<Self, AdapterError> {
        let publisher_epoch = publisher_epoch.into();
        validate_publisher_epoch(&publisher_epoch)?;
        let index_ids = configured_set(
            index_ids,
            MAX_CONFIGURED_INDEX_IDS,
            MAX_INDEX_ID_BYTES,
            "index_ids",
            is_index_id_token,
        )?;
        let underlying_tickers = configured_set(
            underlying_tickers,
            MAX_CONFIGURED_UNDERLYINGS,
            MAX_UNDERLYING_TICKER_BYTES,
            "underlying_tickers",
            is_underlying_ticker_token,
        )?;
        Ok(Self {
            publisher_epoch,
            guard: RecordGuard::new(Provider::Kalshi),
            index_ids,
            underlying_tickers,
            cursors: BTreeMap::new(),
            frontiers: BTreeMap::new(),
            quarter_window_sizes: BTreeMap::new(),
            settlement_backfill: None,
            passthrough: PassthroughBudget::default(),
            rejected: BTreeSet::new(),
            next_value_sequence: 0,
            next_settlement_sequence: 0,
            next_value_5hz_sequence: 0,
        })
    }

    /// The default wave-A configuration: BRTI only, one Pyth underlying.
    pub fn with_default_configuration(
        publisher_epoch: impl Into<String>,
    ) -> Result<Self, AdapterError> {
        Self::new(
            publisher_epoch,
            &[DEFAULT_INDEX_ID],
            &[DEFAULT_UNDERLYING_TICKER],
        )
    }

    /// True while the settlement-window surface is paused awaiting a repair.
    pub fn settlement_window_paused(&self) -> bool {
        self.settlement_backfill.is_some()
    }

    /// The pending settlement-window repair, if any. Its `quarter_close_time`
    /// and interval are what a restarting consumer checkpoints so it resumes
    /// paused rather than publishing a partial window (audit A1/F4).
    pub fn settlement_window_pause(&self) -> Option<&PendingSettlementBackfill> {
        self.settlement_backfill.as_ref()
    }

    /// True while the venue has refused `cfbenchmarks_value_5hz` on this
    /// connection, so `kalshi.cfbenchmarks.brti.value-5hz` is unavailable rather
    /// than merely idle.
    ///
    /// The channel's availability varies by connection (ruling E25: refused on
    /// three of four connections in the R1 hour, accepted on the fourth), so a
    /// consumer reads this to publish `unavailable` on the 5 Hz-dependent
    /// instances instead of waiting out a liveness bound. The refusal never
    /// touches the market channels or the 1 Hz channel, and never the lane.
    pub fn value_5hz_unavailable(&self) -> bool {
        self.rejected.contains(&IndexChannel::Value5Hz)
    }

    /// Advance the hourly passthrough budget window. The adapter reads no clock,
    /// so the caller — which does — calls this once per elapsed hour.
    pub fn note_passthrough_hour_rollover(&mut self) {
        self.passthrough.in_hour = 0;
    }

    /// Charge one passthrough attempt against the per-gap and hourly budgets.
    ///
    /// Returns [`AdapterError::InvalidRecord`] when either budget is exhausted,
    /// **before** any network call is made — the budgets are a refusal, not a
    /// slower loop.
    pub fn charge_passthrough_attempt(&mut self) -> Result<(), AdapterError> {
        if self.settlement_backfill.is_none() {
            return Err(AdapterError::InvalidRecord(
                "no Kalshi BRTI settlement-window repair is pending".to_owned(),
            ));
        }
        if self.passthrough.in_gap >= PASSTHROUGH_MAX_PER_GAP {
            return Err(AdapterError::InvalidRecord(format!(
                "Kalshi CF Benchmarks passthrough exhausted its {PASSTHROUGH_MAX_PER_GAP}-request budget for one gap"
            )));
        }
        if self.passthrough.in_hour >= PASSTHROUGH_MAX_PER_HOUR {
            return Err(AdapterError::InvalidRecord(format!(
                "Kalshi CF Benchmarks passthrough exhausted its {PASSTHROUGH_MAX_PER_HOUR}-request hourly budget"
            )));
        }
        self.passthrough.in_gap = self.passthrough.in_gap.saturating_add(1);
        self.passthrough.in_hour = self.passthrough.in_hour.saturating_add(1);
        Ok(())
    }

    /// Close a pending settlement-window pause with repair rows fetched from the
    /// CF Benchmarks REST passthrough.
    ///
    /// `rows` must be in strictly increasing `time` order and must cover the
    /// whole pending `(after_index_time, through_index_time]` interval: the
    /// first row strictly after the lower bound, the last row exactly on the
    /// upper bound. A response that does not is a repair **failure** and the
    /// surface stays paused — never a partial apply. A row whose value conflicts
    /// with an already accepted live value is a state inconsistency, never an
    /// overwrite.
    ///
    /// Repair rows are evidence, not publication: this call emits no
    /// [`SourceEvent`] (ruling R2, OD-7).
    pub fn apply_history_backfill(
        &mut self,
        rows: &[KalshiBrtiHistoryRow],
    ) -> Result<IngestOutcome, AdapterError> {
        let Some(pending) = self.settlement_backfill.clone() else {
            return Err(AdapterError::InvalidRecord(
                "no Kalshi BRTI settlement-window repair is pending".to_owned(),
            ));
        };
        if pending.is_unrepairable() {
            return Err(AdapterError::InvalidRecord(format!(
                "the Kalshi BRTI settlement-window pause for quarter close {} has no repairable interval; only a reconnect resumes it",
                pending.quarter_close_time
            )));
        }
        if rows.len() > MAX_HISTORY_ROWS {
            return Err(AdapterError::InvalidPayload(format!(
                "Kalshi CF Benchmarks history carries {} rows, over the {MAX_HISTORY_ROWS}-row bound",
                rows.len()
            )));
        }
        let covered = self.history_rows_cover(&pending, rows)?;
        if !covered {
            return Ok(IngestOutcome::notice(FeedNotice::recovery(
                FeedNoticeCode::Gap,
                format!(
                    "Kalshi CF Benchmarks history did not cover ({}, {}] for index {}; the settlement window stays paused",
                    pending.after_index_time,
                    pending.through_index_time,
                    bounded_token(&pending.index_id)
                ),
                RecoveryAction::BackfillKalshiBrti {
                    index_id: pending.index_id.clone(),
                    after_index_time: pending.after_index_time,
                    through_index_time: pending.through_index_time,
                },
            )));
        }
        self.settlement_backfill = None;
        self.passthrough.in_gap = 0;
        Ok(IngestOutcome::notice(FeedNotice::info(
            FeedNoticeCode::VenueStatus,
            format!(
                "Kalshi CF Benchmarks history repaired ({}, {}] for index {}; the settlement window resumes",
                pending.after_index_time,
                pending.through_index_time,
                bounded_token(&pending.index_id)
            ),
        )))
    }

    /// Whether `rows` cover the pending interval exactly. Ordering, bounds and
    /// value conflicts are hard failures; a short response is a repair failure,
    /// reported as `Ok(false)` so the caller can retry within its budget.
    fn history_rows_cover(
        &self,
        pending: &PendingSettlementBackfill,
        rows: &[KalshiBrtiHistoryRow],
    ) -> Result<bool, AdapterError> {
        let mut previous: Option<i64> = None;
        for row in rows {
            millis_to_rfc3339(row.time, "kalshi.cfbenchmarks history time")?;
            let value = validate_index_value(&row.value, "kalshi.cfbenchmarks history value")?;
            if let Some(previous) = previous
                && row.time <= previous
            {
                return Err(AdapterError::InvalidPayload(
                    "Kalshi CF Benchmarks history rows must increase strictly in time".to_owned(),
                ));
            }
            if row.time <= pending.after_index_time || row.time > pending.through_index_time {
                return Err(AdapterError::InvalidPayload(format!(
                    "Kalshi CF Benchmarks history row at {} falls outside the requested ({}, {}] interval",
                    row.time, pending.after_index_time, pending.through_index_time
                )));
            }
            if let Some((admitted_time, admitted_value)) = self
                .frontiers
                .get(&(IndexChannel::Value, pending.index_id.clone()))
                && *admitted_time == row.time
                && !decimals_equal(admitted_value, &value)?
            {
                return Err(AdapterError::InvalidPayload(format!(
                    "Kalshi CF Benchmarks history row at {} disagrees with the accepted live value",
                    row.time
                )));
            }
            previous = Some(row.time);
        }
        Ok(previous == Some(pending.through_index_time))
    }

    fn ingest_payload(
        &mut self,
        record: &RawRecord,
        payload: &str,
    ) -> Result<IngestOutcome, AdapterError> {
        if payload.len() > MAX_INDEX_MESSAGE_BYTES {
            return Err(AdapterError::InvalidPayload(format!(
                "Kalshi index message of {} bytes exceeds the {MAX_INDEX_MESSAGE_BYTES}-byte bound",
                payload.len()
            )));
        }
        let dispatch: TypeOnly = serde_json::from_str(payload).map_err(|error| {
            AdapterError::InvalidPayload(format!("Kalshi index message is not usable: {error}"))
        })?;
        match dispatch.message_type.as_str() {
            VALUE_CHANNEL => self.ingest_value(record, payload),
            VALUE_5HZ_CHANNEL => self.ingest_value_5hz(record, payload),
            PYTH_CHANNEL => self.ingest_pyth(record, payload),
            "subscribed" => self.ingest_ack(payload),
            "error" => ingest_error(&mut self.rejected, payload),
            // Every other well-formed `type` — the `*_indexlist` and
            // `*_underlying_list` replies included (§3: no list command is sent
            // in wave A, so none is modelled) — is retained in the capture and
            // produces no event.
            other => Ok(IngestOutcome::notice(FeedNotice::info(
                FeedNoticeCode::UnsupportedMessage,
                format!(
                    "Kalshi index message type {} is not normalized",
                    bounded_token(other)
                ),
            ))),
        }
    }

    /// Ingest a `subscribed` reply.
    ///
    /// Observed shape (E23): `{"type":"subscribed","id":N,"msg":{"channel":…,
    /// "sid":N}}` — the channel and sid are nested under `msg`, and `id` names
    /// the subscribe command the reply answers.
    fn ingest_ack(&mut self, payload: &str) -> Result<IngestOutcome, AdapterError> {
        let ack: Ack = serde_json::from_str(payload).map_err(|error| {
            AdapterError::InvalidPayload(format!(
                "Kalshi index subscription acknowledgement is not usable: {error}"
            ))
        })?;
        let Some(channel) = IndexChannel::from_channel_name(&ack.msg.channel) else {
            return Ok(IngestOutcome::notice(FeedNotice::info(
                FeedNoticeCode::UnsupportedMessage,
                format!(
                    "Kalshi acknowledged unsubscribed channel {}",
                    bounded_token(&ack.msg.channel)
                ),
            )));
        };
        if ack.id != channel.command_id() {
            return Ok(IngestOutcome::notice(FeedNotice::info(
                FeedNoticeCode::StateInconsistent,
                format!(
                    "Kalshi acknowledged channel {} under command id {} rather than {}",
                    channel.as_str(),
                    ack.id,
                    channel.command_id()
                ),
            )));
        }
        let cursor = self.cursors.entry(channel).or_default();
        let rebound_from = match cursor.sid {
            Some(bound) if bound != ack.msg.sid => Some(bound),
            _ => None,
        };
        // A re-acknowledgement on a new sid REBINDS the channel rather than
        // being refused: a stale sid would drop every following frame on the new
        // sid for the rest of the connection (silent data loss). The new sid is
        // a fresh subscription, so the sequence cursor resets with it; the
        // rebind is still surfaced as one StateInconsistent notice.
        cursor.sid = Some(ack.msg.sid);
        if rebound_from.is_some() {
            cursor.expected_seq = None;
        }
        // The acceptance clears any standing refusal for this channel, so a
        // reconnect that reacquires it says so in the same notice a consumer
        // routes on.
        self.rejected.remove(&channel);
        if let Some(bound) = rebound_from {
            return Ok(IngestOutcome::notice(FeedNotice::subscription(
                FeedNoticeCode::StateInconsistent,
                format!(
                    "Kalshi channel {} was bound to sid {bound} and is now re-acknowledged on sid {}; rebinding to the new sid",
                    channel.as_str(),
                    ack.msg.sid
                ),
                SubscriptionOutcome {
                    source_id: channel.source_id(),
                    accepted: true,
                    code: None,
                },
            )));
        }
        Ok(IngestOutcome::notice(FeedNotice::subscription(
            FeedNoticeCode::SubscriptionAccepted,
            format!(
                "Kalshi acknowledged channel {} on sid {}",
                channel.as_str(),
                ack.msg.sid
            ),
            SubscriptionOutcome {
                source_id: channel.source_id(),
                accepted: true,
                code: None,
            },
        )))
    }

    fn ingest_value(
        &mut self,
        record: &RawRecord,
        payload: &str,
    ) -> Result<IngestOutcome, AdapterError> {
        let message: ValueMessage = serde_json::from_str(payload).map_err(|error| {
            AdapterError::InvalidPayload(format!(
                "Kalshi {VALUE_CHANNEL} message is not usable: {error}"
            ))
        })?;
        let index_id = self.checked_index_id(&message.msg.index_id)?;
        let frame = parse_raw_frame(&message.msg.data)?;
        if frame.id != index_id {
            return Ok(IngestOutcome::notice(FeedNotice::info(
                FeedNoticeCode::StateInconsistent,
                format!(
                    "Kalshi {VALUE_CHANNEL} embedded frame names index {} inside a message for {}",
                    bounded_token(&frame.id),
                    bounded_token(&index_id)
                ),
            )));
        }
        let index_time = frame.time;
        let event_time = millis_to_rfc3339(index_time, "kalshi.cfbenchmarks data.time")?;
        let kalshi_received_at =
            millis_to_rfc3339(message.msg.received_at, "kalshi.cfbenchmarks received_at")?;

        let trailing = &message.msg.avg_60s_data;
        let trailing_value =
            validate_exact_scale_value(&trailing.value, "kalshi.cfbenchmarks avg_60s_data.value")?;
        validate_window_size(
            trailing.window_size,
            MAX_TRAILING_WINDOW_SIZE,
            "kalshi.cfbenchmarks avg_60s_data.window_size",
        )?;
        let trailing_start = millis_to_rfc3339(
            trailing.window_start_ts_ms,
            "kalshi.cfbenchmarks avg_60s_data.window_start_ts_ms",
        )?;
        let trailing_end = millis_to_rfc3339(
            trailing.window_end_ts_exclusive,
            "kalshi.cfbenchmarks avg_60s_data.window_end_ts_exclusive",
        )?;
        if trailing.window_end_ts_exclusive <= trailing.window_start_ts_ms {
            return Err(AdapterError::InvalidPayload(
                "kalshi.cfbenchmarks avg_60s_data window must end after it starts".to_owned(),
            ));
        }
        if trailing.window_end_ts_exclusive != index_time {
            return Err(AdapterError::InvalidPayload(format!(
                "kalshi.cfbenchmarks avg_60s_data.window_end_ts_exclusive {} must equal data.time {index_time}",
                trailing.window_end_ts_exclusive
            )));
        }

        let mut notices = Vec::new();
        match self.step_sequence(IndexChannel::Value, message.sid, message.seq, &mut notices)? {
            SequenceStep::Accept => {}
            SequenceStep::Drop => {
                return Ok(IngestOutcome {
                    events: Vec::new(),
                    notices,
                });
            }
            SequenceStep::Gap { missing } => {
                notices.push(FeedNotice::recovery(
                    FeedNoticeCode::Gap,
                    format!(
                        "Kalshi {VALUE_CHANNEL} skipped {missing} messages; publication continues from the newest valid image"
                    ),
                    RecoveryAction::Reconnect,
                ));
                notices.push(self.open_settlement_backfill(&index_id, index_time));
            }
        }

        match self.step_frontier(IndexChannel::Value, &index_id, index_time, &frame.value)? {
            FrontierStep::Accept => {}
            FrontierStep::Drop(notice) => {
                notices.push(notice);
                return Ok(IngestOutcome {
                    events: Vec::new(),
                    notices,
                });
            }
        }
        let mut events = Vec::new();
        let mut values = BTreeMap::new();
        values.insert("index_id".to_owned(), json!(index_id));
        values.insert("index_value".to_owned(), json!(frame.value));
        values.insert("event_time".to_owned(), json!(event_time));
        values.insert("kalshi_received_at".to_owned(), json!(kalshi_received_at));
        values.insert("upstream_sequence".to_owned(), json!(message.seq));
        values.insert("avg_60s_value".to_owned(), json!(trailing_value));
        values.insert(
            "avg_60s_window_size".to_owned(),
            json!(trailing.window_size),
        );
        values.insert("avg_60s_window_start".to_owned(), json!(trailing_start));
        values.insert(
            "avg_60s_window_end_exclusive".to_owned(),
            json!(trailing_end),
        );
        events.push(self.source_event(
            VALUE_SOURCE_ID,
            event_time.clone(),
            record.received_at.clone(),
            values,
        )?);

        // UNOBSERVED (E23): every frame in the R1 probe was mid-quarter, so the
        // conditional quarter-hour object was never present. Absence is the
        // normal case and is handled here as one; presence follows the
        // documented spec below and is exercised only by fixtures.
        if let Some(quarter) = &message.msg.last_60s_windowed_average_15min {
            match self.settlement_window_event(
                record,
                &index_id,
                index_time,
                &event_time,
                &kalshi_received_at,
                message.seq,
                &frame.value,
                quarter,
            )? {
                Ok(event) => events.push(event),
                Err(notice) => notices.push(notice),
            }
        }

        Ok(IngestOutcome { events, notices })
    }

    #[allow(clippy::too_many_arguments)]
    fn settlement_window_event(
        &mut self,
        record: &RawRecord,
        index_id: &str,
        index_time: i64,
        event_time: &str,
        kalshi_received_at: &str,
        upstream_sequence: u64,
        index_value: &str,
        quarter: &AverageWindow,
    ) -> Result<Result<SourceEvent, FeedNotice>, AdapterError> {
        let window_average = validate_exact_scale_value(
            &quarter.value,
            "kalshi.cfbenchmarks last_60s_windowed_average_15min.value",
        )?;
        validate_window_size(
            quarter.window_size,
            MAX_QUARTER_WINDOW_SIZE,
            "kalshi.cfbenchmarks last_60s_windowed_average_15min.window_size",
        )?;
        // Ruling E28. The wire's `window_start_ts_ms` is FIXED for a quarter and
        // its `window_end_ts_exclusive` ADVANCES with every tick — the average is
        // a running one and the wire end is an as-of, not a close. So the start
        // is the anchor and dedup key, and the close is DERIVED from it. The
        // previous reading had it backwards and hard-refused 236 of the 240
        // quarter objects in the observed hour.
        let start_millis = quarter.window_start_ts_ms;
        let window_start = millis_to_rfc3339(
            start_millis,
            "kalshi.cfbenchmarks last_60s_windowed_average_15min.window_start_ts_ms",
        )?;
        let as_of_millis = quarter.window_end_ts_exclusive;
        let window_as_of = millis_to_rfc3339(
            as_of_millis,
            "kalshi.cfbenchmarks last_60s_windowed_average_15min.window_end_ts_exclusive",
        )?;
        let close_millis = start_millis
            .checked_add(AVERAGE_WINDOW_MILLIS)
            .ok_or_else(|| {
                AdapterError::InvalidPayload(
                    "kalshi.cfbenchmarks quarter window start overflows its derived close"
                        .to_owned(),
                )
            })?;
        // The derived close carries the whole identity claim, so it is the one
        // thing checked hard: a start that does not sit exactly one minute
        // before a `:00/:15/:30/:45` boundary is not a quarter window at all.
        if close_millis % QUARTER_HOUR_MILLIS != 0 {
            return Err(AdapterError::InvalidPayload(format!(
                "kalshi.cfbenchmarks quarter window start {start_millis} derives close {close_millis}, which is not on a :00/:15/:30/:45 boundary"
            )));
        }
        let quarter_close_time =
            millis_to_rfc3339(close_millis, "kalshi.cfbenchmarks derived quarter close")?;
        if as_of_millis <= start_millis || as_of_millis > close_millis {
            return Err(AdapterError::InvalidPayload(format!(
                "kalshi.cfbenchmarks quarter as-of {as_of_millis} falls outside ({start_millis}, {close_millis}]"
            )));
        }
        // The window is complete on the tick whose as-of reaches the close; the
        // 59 before it are running partials of the same quarter. Completeness is
        // published as the equality itself — `window_as_of == quarter_close_time`
        // — rather than as a redundant boolean.

        // Ruling E13: a non-increasing window size for one quarter close is a
        // state inconsistency with the frame dropped — never an adapter error
        // that stops the lane. The key is the DERIVED close (E28), which is what
        // makes it stable across the 60 ticks of one quarter.
        if let Some(previous) = self.quarter_window_sizes.get(&close_millis).copied()
            && quarter.window_size <= previous
        {
            return Ok(Err(FeedNotice::info(
                FeedNoticeCode::StateInconsistent,
                format!(
                    "Kalshi quarter close {close_millis} repeated window size {} after {previous}",
                    quarter.window_size
                ),
            )));
        }
        if self.settlement_backfill.is_some() {
            return Ok(Err(FeedNotice::info(
                FeedNoticeCode::Gap,
                format!(
                    "Kalshi settlement window for quarter close {close_millis} stays paused pending repair"
                ),
            )));
        }
        self.quarter_window_sizes
            .insert(close_millis, quarter.window_size);
        // One quarter close is live at a time; older entries are pruned so the
        // map cannot grow with the run.
        self.quarter_window_sizes
            .retain(|close, _| *close >= close_millis - QUARTER_HOUR_MILLIS);

        let mut values = BTreeMap::new();
        values.insert("index_id".to_owned(), json!(index_id));
        values.insert("event_time".to_owned(), json!(event_time));
        values.insert("kalshi_received_at".to_owned(), json!(kalshi_received_at));
        values.insert("upstream_sequence".to_owned(), json!(upstream_sequence));
        values.insert("index_value".to_owned(), json!(index_value));
        values.insert("window_average".to_owned(), json!(window_average));
        values.insert("window_size".to_owned(), json!(quarter.window_size));
        values.insert("window_start_exclusive".to_owned(), json!(window_start));
        // `window_end_inclusive` is the accumulation boundary of the whole
        // quarter — the derived close — and `window_as_of` is how far the
        // running average has actually reached on this tick. They coincide on
        // the closing tick and only there, which is how a consumer tells a
        // complete window from the 59 running partials before it.
        values.insert(
            "window_end_inclusive".to_owned(),
            json!(quarter_close_time.clone()),
        );
        values.insert("window_as_of".to_owned(), json!(window_as_of));
        values.insert("quarter_close_time".to_owned(), json!(quarter_close_time));
        let _ = index_time;
        Ok(Ok(self.source_event(
            SETTLEMENT_WINDOW_SOURCE_ID,
            event_time.to_owned(),
            record.received_at.clone(),
            values,
        )?))
    }

    /// Ingest a `cfbenchmarks_value_5hz` message.
    ///
    /// **OBSERVED** (ruling E25) against the 2,202 frames of the R1 hour's
    /// fourth connection: `msg` is exactly
    /// `{index_id, value_usd, source_ts_ms, received_at, data}` — no
    /// `avg_60s_data` and no quarter object, which is what makes this a separate
    /// identity rather than a faster spelling of the 1 Hz channel. `value_usd`
    /// carries exactly 8 fractional digits while the embedded `data.value` is
    /// free-scale, so the two are compared by decimal value and never as text.
    /// `source_ts_ms` sits on an exact 200 ms grid and `seq` is contiguous from
    /// 1 per `sid`.
    fn ingest_value_5hz(
        &mut self,
        record: &RawRecord,
        payload: &str,
    ) -> Result<IngestOutcome, AdapterError> {
        let message: Value5HzMessage = serde_json::from_str(payload).map_err(|error| {
            AdapterError::InvalidPayload(format!(
                "Kalshi {VALUE_5HZ_CHANNEL} message is not usable: {error}"
            ))
        })?;
        let index_id = self.checked_index_id(&message.msg.index_id)?;
        let index_value =
            validate_exact_scale_value(&message.msg.value_usd, "kalshi.cfbenchmarks value_usd")?;
        let index_time = message.msg.source_ts_ms;
        let event_time = millis_to_rfc3339(index_time, "kalshi.cfbenchmarks source_ts_ms")?;
        let kalshi_received_at =
            millis_to_rfc3339(message.msg.received_at, "kalshi.cfbenchmarks received_at")?;
        if message.msg.received_at < index_time {
            return Err(AdapterError::InvalidPayload(format!(
                "kalshi.cfbenchmarks received_at {} precedes source_ts_ms {index_time}",
                message.msg.received_at
            )));
        }
        let frame = parse_raw_frame(&message.msg.data)?;

        let mut notices = Vec::new();
        match self.step_sequence(IndexChannel::Value5Hz, message.sid, message.seq, &mut notices)? {
            SequenceStep::Accept => {}
            SequenceStep::Drop => {
                return Ok(IngestOutcome {
                    events: Vec::new(),
                    notices,
                });
            }
            SequenceStep::Gap { missing } => notices.push(FeedNotice::recovery(
                FeedNoticeCode::Gap,
                format!(
                    "Kalshi {VALUE_5HZ_CHANNEL} skipped {missing} messages; publication continues from the newest valid image"
                ),
                RecoveryAction::Reconnect,
            )),
        }

        // The embedded frame is a cross-check only: a disagreement drops the
        // message rather than choosing a side.
        if frame.id != index_id
            || frame.time != index_time
            || !decimals_equal(&frame.value, &index_value)?
        {
            notices.push(FeedNotice::info(
                FeedNoticeCode::StateInconsistent,
                format!(
                    "Kalshi {VALUE_5HZ_CHANNEL} embedded frame disagrees with its own message for index {}",
                    bounded_token(&index_id)
                ),
            ));
            return Ok(IngestOutcome {
                events: Vec::new(),
                notices,
            });
        }

        match self.step_frontier(IndexChannel::Value5Hz, &index_id, index_time, &index_value)? {
            FrontierStep::Accept => {}
            FrontierStep::Drop(notice) => {
                notices.push(notice);
                return Ok(IngestOutcome {
                    events: Vec::new(),
                    notices,
                });
            }
        }

        let mut values = BTreeMap::new();
        values.insert("index_id".to_owned(), json!(index_id));
        values.insert("index_value".to_owned(), json!(index_value));
        values.insert("event_time".to_owned(), json!(event_time));
        values.insert("kalshi_received_at".to_owned(), json!(kalshi_received_at));
        values.insert("upstream_sequence".to_owned(), json!(message.seq));
        let event = self.source_event(
            VALUE_5HZ_SOURCE_ID,
            event_time,
            record.received_at.clone(),
            values,
        )?;
        Ok(IngestOutcome {
            events: vec![event],
            notices,
        })
    }

    /// Ingest a `pyth_value` message.
    ///
    /// **Refuse-closed and UNOBSERVED.** The R1 probe subscribed the channel and
    /// was acknowledged on sid 2, but Kalshi delivered no `pyth_value` frame in
    /// 120 s, so neither its cadence nor its body shape is known (ruling E23).
    /// The model below is Kalshi's documented one: the message is parsed
    /// strictly, sequenced and ordered so a real frame would be reported the
    /// moment it arrives, but nothing is published under
    /// [`PYTH_VALUE_SOURCE_ID`] until a frame is captured. Publishing an
    /// identity whose wire shape is a guess is the failure this refusal exists
    /// to prevent; the 65-minute capture settles it.
    fn ingest_pyth(
        &mut self,
        record: &RawRecord,
        payload: &str,
    ) -> Result<IngestOutcome, AdapterError> {
        let _ = record;
        let message: PythMessage = serde_json::from_str(payload).map_err(|error| {
            AdapterError::InvalidPayload(format!(
                "Kalshi {PYTH_CHANNEL} message is not usable: {error}"
            ))
        })?;
        let ticker = message.msg.underlying_ticker;
        if !self.underlying_tickers.contains(&ticker) {
            return Err(AdapterError::InvalidPayload(format!(
                "Kalshi {PYTH_CHANNEL} carried unsubscribed underlying ticker {}",
                bounded_token(&ticker)
            )));
        }
        let price = validate_exact_scale_value(&message.msg.value_usd, "kalshi.pyth value_usd")?;
        let index_time = message.msg.source_ts_ms;
        let _event_time = millis_to_rfc3339(index_time, "kalshi.pyth source_ts_ms")?;
        let _kalshi_received_at =
            millis_to_rfc3339(message.msg.received_at, "kalshi.pyth received_at")?;
        if message.msg.received_at < index_time {
            return Err(AdapterError::InvalidPayload(format!(
                "kalshi.pyth received_at {} precedes source_ts_ms {index_time}",
                message.msg.received_at
            )));
        }

        let mut notices = Vec::new();
        match self.step_sequence(IndexChannel::Pyth, message.sid, message.seq, &mut notices)? {
            SequenceStep::Accept => {}
            SequenceStep::Drop => return Ok(IngestOutcome { events: Vec::new(), notices }),
            SequenceStep::Gap { missing } => notices.push(FeedNotice::recovery(
                FeedNoticeCode::Gap,
                format!(
                    "Kalshi {PYTH_CHANNEL} skipped {missing} messages; publication continues from the newest valid image"
                ),
                RecoveryAction::Reconnect,
            )),
        }
        match self.step_frontier(IndexChannel::Pyth, &ticker, index_time, &price)? {
            FrontierStep::Accept => {}
            FrontierStep::Drop(notice) => {
                notices.push(notice);
                return Ok(IngestOutcome {
                    events: Vec::new(),
                    notices,
                });
            }
        }

        notices.push(FeedNotice::info(
            FeedNoticeCode::UnsupportedMessage,
            format!(
                "{PYTH_VALUE_SOURCE_ID} is not ready: the {PYTH_CHANNEL} wire contract is documented but unobserved, so the relay publishes nothing for {}",
                bounded_token(&ticker)
            ),
        ));
        Ok(IngestOutcome {
            events: Vec::new(),
            notices,
        })
    }

    fn checked_index_id(&self, index_id: &str) -> Result<String, AdapterError> {
        if !is_index_id_token(index_id) || index_id.len() > MAX_INDEX_ID_BYTES {
            return Err(AdapterError::InvalidPayload(format!(
                "Kalshi index id {} is not an accepted token",
                bounded_token(index_id)
            )));
        }
        if !self.index_ids.iter().any(|value| value == index_id) {
            return Err(AdapterError::InvalidPayload(format!(
                "Kalshi index socket carried unsubscribed index id {}",
                bounded_token(index_id)
            )));
        }
        Ok(index_id.to_owned())
    }

    /// Apply the per-subscription `seq` rules of §6.1. Pushes the duplicate and
    /// out-of-order notices itself; a gap is returned so each surface can apply
    /// its own policy.
    fn step_sequence(
        &mut self,
        channel: IndexChannel,
        sid: u64,
        seq: u64,
        notices: &mut Vec<FeedNotice>,
    ) -> Result<SequenceStep, AdapterError> {
        let cursor = self.cursors.entry(channel).or_default();
        match cursor.sid {
            Some(bound) if bound == sid => {}
            Some(bound) => {
                notices.push(FeedNotice::info(
                    FeedNoticeCode::StateInconsistent,
                    format!(
                        "Kalshi channel {} is bound to sid {bound} but a message arrived on sid {sid}",
                        channel.as_str()
                    ),
                ));
                return Ok(SequenceStep::Drop);
            }
            None => {
                notices.push(FeedNotice::info(
                    FeedNoticeCode::StateInconsistent,
                    format!(
                        "Kalshi channel {} carried a message before its subscription was acknowledged",
                        channel.as_str()
                    ),
                ));
                return Ok(SequenceStep::Drop);
            }
        }
        let Some(expected) = cursor.expected_seq else {
            // First sample on this sid: record the cursor, claim no gap.
            cursor.expected_seq = Some(seq.checked_add(1).ok_or(AdapterError::SequenceExhausted)?);
            return Ok(SequenceStep::Accept);
        };
        if seq == expected {
            cursor.expected_seq = Some(seq.checked_add(1).ok_or(AdapterError::SequenceExhausted)?);
            return Ok(SequenceStep::Accept);
        }
        if seq < expected {
            let code = if seq + 1 == expected {
                FeedNoticeCode::Duplicate
            } else {
                FeedNoticeCode::OutOfOrder
            };
            notices.push(FeedNotice::info(
                code,
                format!(
                    "Kalshi channel {} received seq {seq} behind expected {expected}",
                    channel.as_str()
                ),
            ));
            return Ok(SequenceStep::Drop);
        }
        let missing = seq - expected;
        cursor.expected_seq = Some(seq.checked_add(1).ok_or(AdapterError::SequenceExhausted)?);
        Ok(SequenceStep::Gap { missing })
    }

    /// Apply the `(channel, key, event_time)` identity rules of §6.3.
    fn step_frontier(
        &mut self,
        channel: IndexChannel,
        key: &str,
        index_time: i64,
        value: &str,
    ) -> Result<FrontierStep, AdapterError> {
        let entry = self.frontiers.get(&(channel, key.to_owned())).cloned();
        match entry {
            Some((frontier, _)) if index_time > frontier => {}
            Some((frontier, admitted)) if index_time == frontier => {
                let code = if admitted == value {
                    FeedNoticeCode::Duplicate
                } else {
                    // Wave A's whole correction behaviour: report and drop.
                    // Revision and replay semantics are deferred (R2, OD-8).
                    //
                    // Audit A1/F2: a same-index-time/different-value tick is a
                    // correction, not a lost print — it opens no backfill. The
                    // previous code recorded an empty `(t, t]` interval no
                    // repair could ever cover, silencing the settlement window
                    // for the life of the publisher epoch.
                    FeedNoticeCode::StateInconsistent
                };
                return Ok(FrontierStep::Drop(FeedNotice::info(
                    code,
                    format!(
                        "Kalshi channel {} repeated index time {index_time} for {}",
                        channel.as_str(),
                        bounded_token(key)
                    ),
                )));
            }
            Some(_) => {
                return Ok(FrontierStep::Drop(FeedNotice::info(
                    FeedNoticeCode::OutOfOrder,
                    format!(
                        "Kalshi channel {} received index time {index_time} behind its frontier for {}",
                        channel.as_str(),
                        bounded_token(key)
                    ),
                )));
            }
            None => {}
        }
        let max_frontiers = self.max_tracked_frontiers();
        if self.frontiers.len() >= max_frontiers
            && !self.frontiers.contains_key(&(channel, key.to_owned()))
        {
            return Err(AdapterError::InvalidPayload(format!(
                "Kalshi index socket tracks more than {max_frontiers} frontiers"
            )));
        }
        self.frontiers
            .insert((channel, key.to_owned()), (index_time, value.to_owned()));
        Ok(FrontierStep::Accept)
    }

    /// The largest number of `(channel, key)` frontiers this configuration can
    /// legally produce: the two CF channels (`cfbenchmarks_value` and
    /// `cfbenchmarks_value_5hz`) per configured index id, plus one Pyth frontier
    /// per configured underlying ticker. Derived from the configured set sizes
    /// so an accepted configuration can never hard-fail mid-stream on the
    /// tracking bound — a fixed cap of 16 could, since a full configuration
    /// reaches 8 index ids × 2 + 4 underlyings = 20 distinct frontiers.
    fn max_tracked_frontiers(&self) -> usize {
        self.index_ids.len() * 2 + self.underlying_tickers.len()
    }

    /// Open a settlement-window pause and name the interval a repair must cover.
    ///
    /// When no value-channel frontier exists there is nothing to anchor the
    /// interval to, so the pause records an empty `(t, t]` interval that no
    /// repair can cover — [`PendingSettlementBackfill::is_unrepairable`]. The
    /// surface then stays paused until a reconnect, which now really does clear
    /// it (see [`Self::reset_connection_state`]). Fail closed, never a partial
    /// average.
    fn open_settlement_backfill(&mut self, index_id: &str, through_index_time: i64) -> FeedNotice {
        let after_index_time = self
            .frontiers
            .get(&(IndexChannel::Value, index_id.to_owned()))
            .map_or(through_index_time, |(frontier, _)| *frontier);
        let pending = PendingSettlementBackfill {
            index_id: index_id.to_owned(),
            quarter_close_time: quarter_close_at_or_after(through_index_time),
            after_index_time,
            through_index_time,
        };
        self.settlement_backfill = Some(pending.clone());
        self.passthrough.in_gap = 0;
        FeedNotice::recovery(
            FeedNoticeCode::Gap,
            format!(
                "Kalshi settlement window lost prints in ({after_index_time}, {through_index_time}] for {}; publication pauses until repair",
                bounded_token(index_id)
            ),
            RecoveryAction::BackfillKalshiBrti {
                index_id: pending.index_id,
                after_index_time: pending.after_index_time,
                through_index_time: pending.through_index_time,
            },
        )
    }

    /// Reset connection-local state.
    ///
    /// `sid`/`seq` bindings are connection-local and the `event_time` frontiers
    /// are not, so the frontiers survive a reconnect. The settlement-window
    /// pause is cleared: a reconnect re-seeds the value channel from a fresh
    /// subscription, so the interval a repair would have covered is no longer
    /// the one the surface is waiting on (audit A1/F2 — before this, the doc
    /// claimed a reconnect re-seeded a pause it never cleared).
    fn reset_connection_state(&mut self) {
        self.cursors.clear();
        // A refusal is connection-local: the dialer re-sends the whole subscribe
        // set on every reconnect, and the R1 hour shows the venue accepting a
        // channel it had refused on the three previous connections (E25).
        self.rejected.clear();
        self.settlement_backfill = None;
        self.passthrough.in_gap = 0;
    }

    fn source_event(
        &mut self,
        source_id: &'static str,
        event_time: String,
        received_at: String,
        values: BTreeMap<String, Value>,
    ) -> Result<SourceEvent, AdapterError> {
        let sequence = match source_id {
            VALUE_SOURCE_ID => &mut self.next_value_sequence,
            SETTLEMENT_WINDOW_SOURCE_ID => &mut self.next_settlement_sequence,
            VALUE_5HZ_SOURCE_ID => &mut self.next_value_5hz_sequence,
            // `kalshi.pyth.btc-usd.value` has no counter: the relay is
            // refuse-closed while its contract is unobserved, so nothing is
            // ever published under that identity.
            _ => {
                return Err(AdapterError::InvalidRecord(
                    "unknown Kalshi index source identity".to_owned(),
                ));
            }
        };
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
}

impl FeedAdapter for KalshiIndexAdapter {
    fn ingest(&mut self, record: &RawRecord) -> Result<IngestOutcome, AdapterError> {
        match self.guard.observe(record)? {
            // The index socket has no market binding, so a configuration record
            // does not describe anything it could bind to.
            GuardAction::Configuration(_) => Err(AdapterError::InvalidRecord(
                "the Kalshi index stream has no market binding and accepts no configuration record"
                    .to_owned(),
            )),
            GuardAction::Connected => {
                self.reset_connection_state();
                Ok(IngestOutcome::notice(FeedNotice::info(
                    FeedNoticeCode::Connected,
                    "Kalshi index socket connected; subscriptions await acknowledgement",
                )))
            }
            GuardAction::Disconnected => Ok(IngestOutcome::notice(FeedNotice::recovery(
                FeedNoticeCode::Disconnected,
                "Kalshi index socket disconnected",
                RecoveryAction::Reconnect,
            ))),
            GuardAction::Outbound => Ok(IngestOutcome::default()),
            GuardAction::Failed(_) => Err(AdapterError::InvalidRecord(
                "Kalshi index captures do not accept failure records".to_owned(),
            )),
            GuardAction::Inbound(payload) => self.ingest_payload(record, payload),
        }
    }
}

/// Outcome of the per-subscription `seq` check.
enum SequenceStep {
    Accept,
    Drop,
    Gap { missing: u64 },
}

/// Outcome of the `(channel, key, event_time)` identity check.
enum FrontierStep {
    Accept,
    Drop(FeedNotice),
}

/// Ingest an `error` reply.
///
/// Observed shape (E23): `{"type":"error","id":N,"msg":{"code":8,"msg":"Unknown
/// channel name"}}`. The reply names only the command id, so the channel is
/// recovered from the reserved id band. A rejected subscription is a
/// [`FeedNoticeCode::SubscriptionRejected`] notice naming that channel — never a
/// lane failure: code 8 on `cfbenchmarks_value_5hz` is routine — one hour saw
/// the venue accept on its fourth connection a channel it had refused on the
/// three before, and the next hour accepted it outright (ruling E25).
fn ingest_error(
    rejected: &mut BTreeSet<IndexChannel>,
    payload: &str,
) -> Result<IngestOutcome, AdapterError> {
    let message: ErrorMessage = serde_json::from_str(payload).map_err(|error| {
        AdapterError::InvalidPayload(format!("Kalshi index error message is not usable: {error}"))
    })?;
    let channel = message.id.and_then(IndexChannel::from_command_id);
    let named = channel.map_or_else(
        || match message.id {
            Some(id) => format!("command id {id}"),
            None => "an unidentified command".to_owned(),
        },
        |channel| format!("channel {}", channel.as_str()),
    );
    // Tolerant subscription (ruling E25): the refusal is recorded so the
    // channel's own surfaces can publish `unavailable`, and nothing else on the
    // socket is touched. It is cleared by the next `Connected`, which re-sends
    // the whole subscribe set.
    if let Some(channel) = channel {
        rejected.insert(channel);
    }
    let detail = format!(
        "Kalshi index subscription for {named} was rejected with code {}: {}; the other channels are unaffected and the subscribe is re-attempted on the next connection",
        message.msg.code,
        bounded_token(message.msg.msg.as_deref().unwrap_or("no reason provided"))
    );
    // Only a reply whose id resolves to a channel can name a source; one that
    // does not is still reported, but carries no routable outcome.
    let outcome = channel.map(|channel| SubscriptionOutcome {
        source_id: channel.source_id(),
        accepted: false,
        code: Some(i64::from(message.msg.code)),
    });
    Ok(IngestOutcome::notice(match outcome {
        Some(outcome) => {
            FeedNotice::subscription(FeedNoticeCode::SubscriptionRejected, detail, outcome)
        }
        None => FeedNotice::info(FeedNoticeCode::SubscriptionRejected, detail),
    }))
}

/// Whether an inbound frame on a shared Kalshi socket belongs to the index
/// channels rather than the market ones.
///
/// The index and market channels ride one connection (observed, ruling E23), so
/// something has to say which adapter models each frame. A data frame is named
/// by its `type`; a `subscribed` or `error` reply names only the command `id` it
/// answers, which is why the index subscribes use the reserved band at and above
/// [`INDEX_COMMAND_ID_BASE`]. Anything unparseable is left to the market
/// adapter, which reports a malformed payload as it always has.
pub(crate) fn routes_to_index_channel(payload: &str) -> bool {
    #[derive(Deserialize)]
    struct Route {
        #[serde(rename = "type")]
        message_type: String,
        #[serde(default)]
        id: Option<u64>,
    }
    let Ok(route) = serde_json::from_str::<Route>(payload) else {
        return false;
    };
    match route.message_type.as_str() {
        VALUE_CHANNEL | VALUE_5HZ_CHANNEL | PYTH_CHANNEL => true,
        "subscribed" => route.id.is_some_and(|id| id >= INDEX_COMMAND_ID_BASE),
        // An `error` reply that omits `id` is routed to the index adapter, not
        // the market one. The index channels are the ones the venue refuses
        // (ruling E25: the 5 Hz channel is refused on most connections), and
        // `ingest_error` downgrades an id-less error to a per-source notice,
        // whereas the market adapter turns any error into a hard AdapterError
        // that stops the whole lane — the outcome ruling E2 forbids. A genuine
        // market error still echoes its own command id (below the index band)
        // and stays with the market adapter.
        "error" => route.id.is_none_or(|id| id >= INDEX_COMMAND_ID_BASE),
        _ => false,
    }
}

/// Whether an OUTBOUND command recorded on the shared Kalshi socket addresses
/// the index channels rather than the market ones.
///
/// The index and market subscribes ride one connection (ruling E23), so an
/// index subscribe replayed as an `OutboundText` record must reach the index
/// adapter — which accepts and ignores outbound records — instead of the market
/// adapter, whose `outbound` handler refuses any command whose channels or
/// ticker it does not model and thereby stops the whole market lane. Arming
/// `cfbrti`/`pyth` must never do that (ruling E2).
///
/// An index subscribe is named two ways, and either is sufficient: its command
/// `id` sits in the reserved band at and above [`INDEX_COMMAND_ID_BASE`], and
/// its `params.channels` names one of the index channels. A market command
/// carries neither — its id is below the band and its channels are market
/// channels — so it stays with the market adapter.
pub(crate) fn outbound_routes_to_index_channel(payload: &str) -> bool {
    #[derive(Deserialize, Default)]
    struct CommandParams {
        #[serde(default)]
        channels: Vec<String>,
    }
    #[derive(Deserialize)]
    struct Command {
        #[serde(default)]
        id: Option<u64>,
        #[serde(default)]
        params: CommandParams,
    }
    let Ok(command) = serde_json::from_str::<Command>(payload) else {
        return false;
    };
    if command.id.is_some_and(|id| id >= INDEX_COMMAND_ID_BASE) {
        return true;
    }
    command
        .params
        .channels
        .iter()
        .any(|channel| IndexChannel::from_channel_name(channel).is_some())
}

/// Parse the embedded raw CF Benchmarks frame carried as a JSON string.
fn parse_raw_frame(data: &str) -> Result<RawFrame, AdapterError> {
    if data.len() > MAX_RAW_FRAME_BYTES {
        return Err(AdapterError::InvalidPayload(format!(
            "Kalshi embedded CF Benchmarks frame of {} bytes exceeds the {MAX_RAW_FRAME_BYTES}-byte bound",
            data.len()
        )));
    }
    let frame: RawFrameWire = serde_json::from_str(data).map_err(|error| {
        AdapterError::InvalidPayload(format!(
            "Kalshi embedded CF Benchmarks frame is not usable: {error}"
        ))
    })?;
    if frame.frame_type != "value" {
        return Err(AdapterError::InvalidPayload(format!(
            "Kalshi embedded CF Benchmarks frame type {} is not a value frame",
            bounded_token(&frame.frame_type)
        )));
    }
    let value = validate_index_value(&frame.value, "kalshi.cfbenchmarks data.value")?;
    Ok(RawFrame {
        id: frame.id,
        time: frame.time,
        value,
    })
}

/// Validate a raw index value: positive, canonical base-10, bounded length and
/// at most [`MAX_VALUE_SCALE`] fractional digits. Returned verbatim.
fn validate_index_value(value: &str, field: &str) -> Result<String, AdapterError> {
    if value.len() > MAX_VALUE_BYTES {
        return Err(AdapterError::InvalidPayload(format!(
            "{field} exceeds {MAX_VALUE_BYTES} bytes"
        )));
    }
    let parsed = validate_decimal(value, field)?;
    if parsed.is_zero() {
        return Err(AdapterError::InvalidPayload(format!(
            "{field} must be strictly positive"
        )));
    }
    if fractional_digits(value) > MAX_VALUE_SCALE {
        return Err(AdapterError::InvalidPayload(format!(
            "{field} carries more than {MAX_VALUE_SCALE} fractional digits"
        )));
    }
    Ok(value.to_owned())
}

/// Validate a value Kalshi documents at exactly [`EXACT_VALUE_SCALE`]
/// fractional digits — the averaged and relayed fields.
fn validate_exact_scale_value(value: &str, field: &str) -> Result<String, AdapterError> {
    let value = validate_index_value(value, field)?;
    if fractional_digits(&value) != EXACT_VALUE_SCALE {
        return Err(AdapterError::InvalidPayload(format!(
            "{field} must carry exactly {EXACT_VALUE_SCALE} fractional digits"
        )));
    }
    Ok(value)
}

/// Validate an average-window tick count.
///
/// Observed (E23): a fresh subscription's first `avg_60s_data` carries
/// `window_size` **0** while the window is still filling, so zero is a normal
/// value and not a payload error. The documented assumption was `>= 1`; a hard
/// refusal there would have stopped the lane on its own first frame.
fn validate_window_size(size: u32, maximum: u32, field: &str) -> Result<(), AdapterError> {
    if size > maximum {
        return Err(AdapterError::InvalidPayload(format!(
            "{field} {size} is outside 0..={maximum}"
        )));
    }
    Ok(())
}

fn fractional_digits(value: &str) -> usize {
    value.split_once('.').map_or(0, |(_, rest)| rest.len())
}

/// Compare two validated decimal tokens by value rather than by text, so
/// `64250.10` and `64250.1000` agree.
fn decimals_equal(left: &str, right: &str) -> Result<bool, AdapterError> {
    let left = validate_decimal(left, "kalshi.cfbenchmarks cross-check value")?;
    let right = validate_decimal(right, "kalshi.cfbenchmarks cross-check value")?;
    Ok(left == right)
}

fn is_index_id_token(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
}

fn is_underlying_ticker_token(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_graphic())
}

fn configured_set(
    values: &[&str],
    maximum: usize,
    maximum_bytes: usize,
    field: &str,
    accepted: fn(&str) -> bool,
) -> Result<Vec<String>, AdapterError> {
    if values.is_empty() || values.len() > maximum {
        return Err(AdapterError::InvalidRecord(format!(
            "{field} must name 1 to {maximum} entries"
        )));
    }
    let mut set: Vec<String> = Vec::with_capacity(values.len());
    for value in values {
        if value.len() > maximum_bytes || !accepted(value) {
            return Err(AdapterError::InvalidRecord(format!(
                "{field} entry {} is not an accepted token",
                bounded_token(value)
            )));
        }
        set.push((*value).to_owned());
    }
    set.sort_unstable();
    let unique = set.len();
    set.dedup();
    if set.len() != unique {
        return Err(AdapterError::InvalidRecord(format!(
            "{field} must not repeat an entry"
        )));
    }
    Ok(set)
}

#[derive(Deserialize)]
struct TypeOnly {
    #[serde(rename = "type")]
    message_type: String,
}

/// Observed subscription acknowledgement (E23).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Ack {
    #[serde(rename = "type")]
    _message_type: String,
    id: u64,
    msg: AckBody,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AckBody {
    sid: u64,
    channel: String,
}

/// Observed error reply (E23). `id` is the subscribe command it answers.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ErrorMessage {
    #[serde(rename = "type")]
    _message_type: String,
    #[serde(default)]
    id: Option<u64>,
    msg: ErrorBody,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ErrorBody {
    code: u32,
    #[serde(default)]
    msg: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ValueMessage {
    #[serde(rename = "type")]
    _message_type: String,
    sid: u64,
    seq: u64,
    msg: ValueBody,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ValueBody {
    index_id: String,
    received_at: i64,
    data: String,
    avg_60s_data: AverageWindow,
    #[serde(default)]
    last_60s_windowed_average_15min: Option<AverageWindow>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AverageWindow {
    value: String,
    window_size: u32,
    window_start_ts_ms: i64,
    window_end_ts_exclusive: i64,
}

/// Observed 5 Hz frame (ruling E25). Note the absence of `avg_60s_data`: this
/// channel carries the lean tick only.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Value5HzMessage {
    #[serde(rename = "type")]
    _message_type: String,
    sid: u64,
    seq: u64,
    msg: Value5HzBody,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Value5HzBody {
    index_id: String,
    value_usd: String,
    source_ts_ms: i64,
    received_at: i64,
    data: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PythMessage {
    #[serde(rename = "type")]
    _message_type: String,
    sid: u64,
    seq: u64,
    msg: PythBody,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PythBody {
    underlying_ticker: String,
    value_usd: String,
    source_ts_ms: i64,
    received_at: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFrameWire {
    #[serde(rename = "type")]
    frame_type: String,
    id: String,
    time: i64,
    value: String,
}

struct RawFrame {
    id: String,
    time: i64,
    value: String,
}

// --- CF Benchmarks REST passthrough (repair-only) ---------------------------

/// Base of Kalshi's authenticated REST API.
pub const PASSTHROUGH_BASE_URL: &str = "https://external-api.kalshi.com";
/// Path of the CF Benchmarks tick-level history passthrough. Signed WITHOUT its
/// query string, which is what Kalshi's signing rule requires.
pub const HISTORY_PATH: &str = "/trade-api/v2/cfbenchmarks/history/values";
/// Path of the CF Benchmarks current-value passthrough.
pub const VALUES_PATH: &str = "/trade-api/v2/cfbenchmarks/values";
/// The one history timespan this repair path requests.
pub const HISTORY_TIMESPAN: &str = "HOUR";

/// The signed CF Benchmarks REST passthrough client.
///
/// Credentials are read from the environment exactly as the market socket reads
/// them, are held in zeroizing memory, and never appear in argv, notices,
/// captures or logs. The client is repair-only: it fetches evidence that closes
/// a settlement-window pause and publishes nothing.
pub struct KalshiPassthroughClient {
    auth: KalshiAuthClient,
}

impl KalshiPassthroughClient {
    /// Build the client from `KALSHI_API_KEY_ID` and `KALSHI_PRIVATE_KEY_PATH`.
    pub fn from_env() -> Result<Self, KalshiPassthroughError> {
        Ok(Self {
            auth: KalshiAuthClient::from_env()?,
        })
    }

    /// Fetch one hour of CF Benchmarks tick history ending at `timestamp`.
    pub async fn fetch_brti_history(
        &self,
        index_id: &str,
        timestamp: i64,
    ) -> Result<Vec<KalshiBrtiHistoryRow>, KalshiPassthroughError> {
        fetch_brti_history(&self.auth, index_id, timestamp).await
    }
}

/// Fetch one hour of CF Benchmarks tick history through Kalshi's authenticated
/// passthrough, for a settlement-window repair.
///
/// This is the only signed REST call in the crate. The path is signed without
/// its query string; the three `kalshi-access-*` headers are marked sensitive so
/// they are redacted from request debug output and never enter the raw journal.
/// The response is read through a bounded reader and its rows are returned for
/// [`KalshiIndexAdapter::apply_history_backfill`] — nothing here publishes an
/// event.
///
/// **Documented, not observed** (ruling R1): the envelope and row shape below
/// are transcribed from Kalshi's passthrough page and are confirmed against a
/// live probe response before the repair path is enabled.
pub(crate) async fn fetch_brti_history(
    auth: &KalshiAuthClient,
    index_id: &str,
    timestamp: i64,
) -> Result<Vec<KalshiBrtiHistoryRow>, KalshiPassthroughError> {
    if !is_index_id_token(index_id) || index_id.len() > MAX_INDEX_ID_BYTES {
        return Err(KalshiPassthroughError::InvalidRequest(
            "index id is not an accepted token".to_owned(),
        ));
    }
    let signed = auth.sign_get(HISTORY_PATH)?;
    let mut request = secure_http::client()
        .map_err(passthrough_transport_error)?
        .get(format!("{PASSTHROUGH_BASE_URL}{HISTORY_PATH}"))
        .header(ACCEPT, "application/json")
        .header(CACHE_CONTROL, "no-cache")
        .query(&[
            ("id", index_id.to_owned()),
            ("timespan", HISTORY_TIMESPAN.to_owned()),
            ("timestamp", timestamp.to_string()),
        ]);
    for (name, value) in [
        ("kalshi-access-key", signed.key_id.as_str()),
        ("kalshi-access-signature", signed.signature.as_str()),
        ("kalshi-access-timestamp", &signed.timestamp.to_string()),
    ] {
        let name = HeaderName::from_static(name);
        let mut value = HeaderValue::from_str(value)
            .map_err(|_| KalshiPassthroughError::Auth(KalshiAuthError::InvalidCredentials))?;
        value.set_sensitive(true);
        request = request.header(name, value);
    }
    let response = request
        .send()
        .await
        .map_err(|_| KalshiPassthroughError::Http)?;
    let body = secure_http::bounded_body(response, MAX_PASSTHROUGH_RESPONSE_BYTES)
        .await
        .map_err(passthrough_transport_error)?;
    let body = String::from_utf8(body).map_err(|_| {
        KalshiPassthroughError::InvalidResponse("passthrough response is not UTF-8".to_owned())
    })?;
    parse_history_response(&body)
}

/// Parse the Kalshi passthrough envelope into repair rows.
///
/// Rows are returned verbatim: the decimal text is preserved and the caller's
/// [`KalshiIndexAdapter::apply_history_backfill`] applies the ordering, coverage
/// and conflict rules.
pub fn parse_history_response(
    body: &str,
) -> Result<Vec<KalshiBrtiHistoryRow>, KalshiPassthroughError> {
    let envelope: HistoryEnvelope = serde_json::from_str(body).map_err(|error| {
        KalshiPassthroughError::InvalidResponse(format!(
            "passthrough response is not usable: {error}"
        ))
    })?;
    if envelope.data.values.len() > MAX_HISTORY_ROWS {
        return Err(KalshiPassthroughError::InvalidResponse(format!(
            "passthrough response carries {} rows, over the {MAX_HISTORY_ROWS}-row bound",
            envelope.data.values.len()
        )));
    }
    Ok(envelope
        .data
        .values
        .into_iter()
        .map(|row| KalshiBrtiHistoryRow {
            time: row.time,
            value: row.value,
        })
        .collect())
}

fn passthrough_transport_error(error: SecureHttpError) -> KalshiPassthroughError {
    match error {
        SecureHttpError::HttpStatus(status) => KalshiPassthroughError::HttpStatus(status),
        other => KalshiPassthroughError::Transport(other.to_string()),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryEnvelope {
    data: HistoryData,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryData {
    values: Vec<HistoryRowWire>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryRowWire {
    time: i64,
    value: String,
}

/// Why a CF Benchmarks passthrough repair failed.
///
/// No variant carries key material, a signature, or the private-key path.
#[derive(Debug, Error)]
pub enum KalshiPassthroughError {
    /// The request arguments were outside the accepted vocabulary.
    #[error("invalid Kalshi passthrough request: {0}")]
    InvalidRequest(String),
    /// The request could not be signed.
    #[error(transparent)]
    Auth(#[from] KalshiAuthError),
    /// The request never produced a response.
    #[error("Kalshi passthrough request failed")]
    Http,
    /// The answer carried a non-2xx status. A 429 is honoured with the shared
    /// reconnect backoff and counts against the per-gap budget; a 400 or 404
    /// means the interval is unrepairable and the surface stays paused; a 503 is
    /// retried within the per-gap budget and then pauses.
    #[error("Kalshi passthrough request returned status {0}")]
    HttpStatus(u16),
    /// The answer was refused by the shared HTTP boundary for a reason other
    /// than its status: an over-sized body, or a body that could not be read.
    #[error("Kalshi passthrough response was refused: {0}")]
    Transport(String),
    /// The answer was 2xx but its envelope or rows were unusable.
    #[error("invalid Kalshi passthrough response: {0}")]
    InvalidResponse(String),
}

impl KalshiPassthroughError {
    /// Whether another attempt inside the per-gap budget can still repair the
    /// interval. A 400 or 404 says the interval is gone; everything else is
    /// worth one more attempt while the budget lasts.
    pub fn is_retryable(&self) -> bool {
        !matches!(self, Self::HttpStatus(400 | 404) | Self::InvalidRequest(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raw::{CaptureStream, RawRecord, RecordKind};

    const EPOCH: &str = "kalshi-index-test-epoch";
    const RECEIVED_AT: &str = "2026-09-03T12:00:00.310Z";

    /// Wire fixtures.
    ///
    /// **Synthetic, with altered values, reproducing the shapes the R1 probe
    /// observed** (ruling E23): the market-command `subscribed` reply with its
    /// nested `msg`, the `error` reply with its nested `code`/`msg`, the 1 Hz
    /// `cfbenchmarks_value` frame with its string-encoded `data` and its
    /// `avg_60s_data` window opening at size 0. No captured frame is copied
    /// here; every price, timestamp and index time is invented.
    ///
    /// Two shapes remain **documented-only** because the probe never delivered
    /// them: the `pyth_value` body, and the conditional
    /// `last_60s_windowed_average_15min` quarter object.
    mod fixtures {
        use super::{IndexChannel, PYTH_CHANNEL, VALUE_5HZ_CHANNEL, VALUE_CHANNEL};

        pub(super) const INDEX_TIME_MS: i64 = 1_788_000_300_000;
        pub(super) const QUARTER_CLOSE_MS: i64 = 1_788_000_300_000;
        /// The next quarter close, and a tick inside its final minute. Used by
        /// the settlement-window gap and repair tests, which need an index time
        /// beyond the frontier the first tick establishes.
        pub(super) const NEXT_QUARTER_CLOSE_MS: i64 = 1_788_001_200_000;
        pub(super) const NEXT_QUARTER_TICK_MS: i64 = 1_788_001_195_000;

        /// The observed `sid` each channel is acknowledged on: the socket
        /// numbers subscriptions in acceptance order from 1.
        pub(super) const fn sid(channel: &str) -> u64 {
            match channel.as_bytes() {
                b"cfbenchmarks_value" => 1,
                b"cfbenchmarks_value_5hz" => 2,
                _ => 3,
            }
        }

        pub(super) fn value_tick(seq: u64, with_quarter: bool) -> String {
            value_tick_window(seq, with_quarter, 60)
        }

        /// A 1 Hz tick whose trailing average reports `window_size` ticks.
        /// Observed: a fresh subscription's first frame reports **0**.
        pub(super) fn value_tick_window(seq: u64, with_quarter: bool, window_size: u32) -> String {
            let quarter = if with_quarter {
                r#","last_60s_windowed_average_15min":{"value":"64248.50000000","window_size":60,"window_start_ts_ms":1788000240000,"window_end_ts_exclusive":1788000300000}"#
            } else {
                ""
            };
            format!(
                r#"{{"type":"cfbenchmarks_value","sid":1,"seq":{seq},"msg":{{"index_id":"BRTI","received_at":1788000300123,"data":"{{\"type\":\"value\",\"time\":1788000300000,\"id\":\"BRTI\",\"value\":\"64250.11\"}}","avg_60s_data":{{"value":"64249.10000000","window_size":{window_size},"window_start_ts_ms":1788000240000,"window_end_ts_exclusive":1788000300000}}{quarter}}}}}"#
            )
        }

        /// A 5 Hz tick on the observed 200 ms grid. `seq` is contiguous from 1
        /// per `sid`, exactly as the R1 hour recorded it.
        ///
        /// Shape is the observed one (ruling E25): no `avg_60s_data`, an 8-scale
        /// `value_usd`, and a free-scale value inside the embedded `data`
        /// string. Values are invented.
        pub(super) fn five_hz_tick(seq: u64) -> String {
            five_hz_tick_at(seq, FIVE_HZ_FIRST_MS + (seq as i64 - 1) * 200, "64250.11")
        }

        pub(super) const FIVE_HZ_FIRST_MS: i64 = 1_788_000_300_200;

        pub(super) fn five_hz_tick_at(seq: u64, source_ts_ms: i64, value: &str) -> String {
            let padded = format!(
                "{value}{}",
                "0".repeat(8 - value.split_once('.').map_or(0, |(_, rest)| rest.len()))
            );
            format!(
                r#"{{"type":"cfbenchmarks_value_5hz","sid":2,"seq":{seq},"msg":{{"index_id":"BRTI","value_usd":"{padded}","source_ts_ms":{source_ts_ms},"received_at":{},"data":"{{\"type\":\"value\",\"time\":{source_ts_ms},\"id\":\"BRTI\",\"value\":\"{value}\"}}"}}}}"#,
                source_ts_ms + 26
            )
        }

        pub(super) fn pyth_tick(seq: u64) -> String {
            format!(
                r#"{{"type":"pyth_value","sid":3,"seq":{seq},"msg":{{"underlying_ticker":"Crypto.BTC/USD","value_usd":"64251.44000000","source_ts_ms":1788000300050,"received_at":1788000300090}}}}"#
            )
        }

        /// The observed `subscribed` reply: `id` at the top level, `channel` and
        /// `sid` nested under `msg`.
        pub(super) fn ack(channel: &str) -> String {
            let id = IndexChannel::from_channel_name(channel).map_or(1, IndexChannel::command_id);
            ack_parts(id, sid(channel), channel)
        }

        pub(super) fn ack_parts(id: u64, sid: u64, channel: &str) -> String {
            format!(
                r#"{{"type":"subscribed","id":{id},"msg":{{"channel":"{channel}","sid":{sid}}}}}"#
            )
        }

        /// The observed `error` reply. Code 8 / "Unknown channel name" is the
        /// exact answer `cfbenchmarks_value_5hz` returned.
        pub(super) fn error(id: u64, code: u32, message: &str) -> String {
            format!(r#"{{"type":"error","id":{id},"msg":{{"code":{code},"msg":"{message}"}}}}"#)
        }

        pub(super) const ALL_CHANNELS: [&str; 3] = [VALUE_CHANNEL, VALUE_5HZ_CHANNEL, PYTH_CHANNEL];
    }

    struct Stream {
        adapter: KalshiIndexAdapter,
        ordinal: u64,
        epoch: u64,
    }

    impl Stream {
        fn new() -> Self {
            Self {
                adapter: KalshiIndexAdapter::with_default_configuration(EPOCH)
                    .expect("the default configuration is accepted"),
                ordinal: 0,
                epoch: 0,
            }
        }

        fn connect(&mut self) -> IngestOutcome {
            self.epoch += 1;
            self.ordinal += 1;
            let record = RawRecord::connected_at(
                CaptureStream::KalshiIndex,
                self.epoch,
                self.ordinal,
                RECEIVED_AT.to_owned(),
                1_000,
                0,
            );
            self.adapter.ingest(&record).expect("connect is accepted")
        }

        /// A server-side drop and the immediate redial. The R1 hour saw three
        /// of these — 07:30:21, 07:32:33 and 07:35:23 — each answered by a
        /// reconnect inside a second (ruling E25 (d)); they are normal
        /// behaviour for this socket, not an incident.
        fn reconnect(&mut self) {
            self.ordinal += 1;
            let record = RawRecord::disconnected_at(
                CaptureStream::KalshiIndex,
                self.epoch,
                self.ordinal,
                RECEIVED_AT.to_owned(),
                3_000,
            );
            self.adapter
                .ingest(&record)
                .expect("disconnect is accepted");
            self.connect();
        }

        fn text(&mut self, payload: &str) -> Result<IngestOutcome, AdapterError> {
            self.ordinal += 1;
            let record = RawRecord::text_at(
                CaptureStream::KalshiIndex,
                self.epoch,
                self.ordinal,
                RecordKind::InboundText,
                payload.to_owned(),
                RECEIVED_AT.to_owned(),
                2_000,
            );
            self.adapter.ingest(&record)
        }

        fn ok(&mut self, payload: &str) -> IngestOutcome {
            self.text(payload).expect("payload is accepted")
        }

        /// Connect and acknowledge both subscribed channels.
        fn ready() -> Self {
            let mut stream = Self::new();
            stream.connect();
            for channel in fixtures::ALL_CHANNELS {
                stream.ok(&fixtures::ack(channel));
            }
            stream
        }
    }

    fn codes(outcome: &IngestOutcome) -> Vec<FeedNoticeCode> {
        outcome.notices.iter().map(|notice| notice.code).collect()
    }

    #[test]
    fn parses_documented_value_tick() {
        let mut stream = Stream::ready();
        let outcome = stream.ok(&fixtures::value_tick(10, false));
        assert_eq!(outcome.events.len(), 1);
        let event = &outcome.events[0];
        assert_eq!(event.source_id, VALUE_SOURCE_ID);
        assert_eq!(event.source_version, SOURCE_VERSION);
        assert_eq!(event.publisher_epoch, EPOCH);
        assert_eq!(event.sequence, 0);
        assert_eq!(event.event_time, "2026-08-29T10:45:00.000Z");
        assert_eq!(event.received_at, RECEIVED_AT);
        assert_eq!(event.values["index_id"], json!("BRTI"));
        // Decimal text is preserved byte for byte, never re-rendered.
        assert_eq!(event.values["index_value"], json!("64250.11"));
        assert_eq!(event.values["avg_60s_value"], json!("64249.10000000"));
        assert_eq!(event.values["avg_60s_window_size"], json!(60));
        assert_eq!(
            event.values["avg_60s_window_start"],
            json!("2026-08-29T10:44:00.000Z")
        );
        assert_eq!(
            event.values["avg_60s_window_end_exclusive"],
            json!("2026-08-29T10:45:00.000Z")
        );
        assert_eq!(
            event.values["kalshi_received_at"],
            json!("2026-08-29T10:45:00.123Z")
        );
        assert_eq!(event.values["upstream_sequence"], json!(10_u64));
        assert_eq!(event.values.len(), 9, "every declared field is emitted");
    }

    /// A re-acknowledgement on a new sid must REBIND the channel, not leave the
    /// stale sid bound. Before the fix, `ingest_ack` returned early on a sid
    /// change without updating the cursor, so every following frame on the new
    /// sid hit `step_sequence`'s "bound to sid X but a message arrived on sid Y"
    /// branch and was dropped for the rest of the connection (silent data loss).
    #[test]
    fn a_reacknowledgement_on_a_new_sid_rebinds_and_keeps_flowing() {
        let mut stream = Stream::ready();

        // The venue re-acknowledges cfbenchmarks_value on a NEW sid (an internal
        // resubscribe), so the channel that `Stream::ready` bound to sid 1 is now
        // acknowledged on sid 7.
        let reack = fixtures::ack_parts(VALUE_COMMAND_ID, 7, VALUE_CHANNEL);
        let outcome = stream.ok(&reack);
        assert!(
            outcome
                .notices
                .iter()
                .any(|notice| notice.code == FeedNoticeCode::StateInconsistent),
            "the rebind is surfaced as a StateInconsistent notice"
        );

        // A frame on the NEW sid now flows. Before the fix the stale sid-1
        // binding stood and this frame hit the sid-mismatch drop instead.
        let frame_on_new_sid = fixtures::value_tick(1, false).replace(r#""sid":1"#, r#""sid":7"#);
        let outcome = stream.ok(&frame_on_new_sid);
        assert_eq!(
            outcome.events.len(),
            1,
            "frames on the rebound sid are accepted, not dropped"
        );
        assert!(
            outcome.notices.is_empty(),
            "the rebound frame is clean: {:?}",
            codes(&outcome)
        );

        // A stale frame still on the old sid 1 is the one now dropped.
        let frame_on_old_sid = fixtures::value_tick(2, false);
        assert!(
            stream.ok(&frame_on_old_sid).events.is_empty(),
            "a frame on the superseded sid no longer publishes"
        );
    }

    /// The frontier-tracking bound is derived from the configured set sizes, so
    /// a fully configured socket (8 index ids × 2 CF channels + 4 Pyth
    /// underlyings = 20 frontiers) cannot exceed it and hard-fail mid-stream —
    /// the failure a fixed cap of 16 produced.
    #[test]
    fn the_frontier_bound_covers_a_full_configuration() {
        let default = KalshiIndexAdapter::with_default_configuration(EPOCH)
            .expect("default configuration is accepted");
        assert_eq!(default.max_tracked_frontiers(), 3);

        let index_ids = ["BRTI", "A", "B", "C", "D", "E", "F", "G"];
        let underlyings = [
            "Crypto.BTC/USD",
            "Crypto.ETH/USD",
            "Crypto.SOL/USD",
            "Crypto.XRP/USD",
        ];
        let full = KalshiIndexAdapter::new(EPOCH, &index_ids, &underlyings)
            .expect("a full configuration is accepted");
        assert_eq!(
            full.max_tracked_frontiers(),
            index_ids.len() * 2 + underlyings.len(),
            "the bound tracks the configured sets, not a fixed 16"
        );
        assert_eq!(full.max_tracked_frontiers(), 20);
    }

    /// An `error` reply that omits `id` routes to the index adapter and is
    /// downgraded to a per-source notice, rather than reaching the market
    /// adapter (which would hard-fail the whole lane). A market error still
    /// echoes its own command id below the index band and stays with the market
    /// adapter.
    #[test]
    fn an_id_less_error_routes_to_the_index_adapter_as_a_notice() {
        const ID_LESS_ERROR: &str =
            r#"{"type":"error","msg":{"code":8,"msg":"index not entitled"}}"#;
        assert!(
            routes_to_index_channel(ID_LESS_ERROR),
            "an id-less error is the index adapter's to downgrade, not the market adapter's to fatal"
        );
        // A market error keeps its own command id (below the index band) and is
        // not the index adapter's.
        let market_error = fixtures::error(7, 6, "bad ticker");
        assert!(!routes_to_index_channel(&market_error));

        // The index adapter turns the id-less error into a notice, never an error.
        let mut stream = Stream::ready();
        let outcome = stream.ok(ID_LESS_ERROR);
        assert!(
            outcome.events.is_empty() && !outcome.notices.is_empty(),
            "the id-less error is a notice, not a lane-stopping AdapterError"
        );
    }

    /// The live-lane subscription set (`cfbrti` + `pyth` armed) is the exact
    /// set of OutboundText commands `capture::kalshi_index_subscriptions`
    /// appends to the shared Kalshi market socket. Every one must route to the
    /// index adapter so the market adapter never sees it and never refuses the
    /// lane (ruling E2); a market subscribe must stay with the market adapter.
    #[test]
    fn the_live_lane_index_subscriptions_route_outbound_to_the_index_adapter() {
        let value = format!(
            r#"{{"id":{VALUE_COMMAND_ID},"cmd":"subscribe","params":{{"channels":["{VALUE_CHANNEL}"],"index_ids":["BRTI"]}}}}"#
        );
        let value_5hz = format!(
            r#"{{"id":{VALUE_5HZ_COMMAND_ID},"cmd":"subscribe","params":{{"channels":["{VALUE_5HZ_CHANNEL}"],"index_ids":["BRTI"]}}}}"#
        );
        let pyth = format!(
            r#"{{"id":{PYTH_COMMAND_ID},"cmd":"subscribe","params":{{"channels":["{PYTH_CHANNEL}"],"underlying_tickers":["Crypto.BTC/USD"]}}}}"#
        );
        for subscription in [&value, &value_5hz, &pyth] {
            assert!(
                outbound_routes_to_index_channel(subscription),
                "index subscribe must route to the index adapter: {subscription}"
            );
        }

        // A market subscribe (id below the band, market channels) stays with the
        // market adapter.
        let market = r#"{"id":10,"cmd":"subscribe","params":{"channels":["orderbook_delta"],"market_tickers":["KXBTC"]}}"#;
        assert!(!outbound_routes_to_index_channel(market));

        // An unparseable payload is left to the market adapter, unchanged.
        assert!(!outbound_routes_to_index_channel("not json"));
    }

    /// Ruling E25: the whole observed 5 Hz burst replays cleanly.
    ///
    /// The R1 hour's fourth connection delivered **2,202** frames — `seq`
    /// contiguous from 1 on one `sid`, `source_ts_ms` on an exact 200 ms grid
    /// over 440.2 s (5.002 frames/s), `value_usd` at exactly 8 fractional
    /// digits, `msg` keys exactly
    /// `{index_id, value_usd, source_ts_ms, received_at, data}` with no
    /// `avg_60s_data`. All 2,202 real frames were checked against these
    /// constraints out of tree with **0 violations**; the fixtures here
    /// reproduce that shape and length with invented prices, because captured
    /// venue data does not enter the repository.
    #[test]
    fn the_observed_five_hz_burst_replays_frame_for_frame() {
        const FRAMES: u64 = 2_202;
        let mut stream = Stream::ready();
        let mut events = Vec::new();
        for seq in 1..=FRAMES {
            let outcome = stream.ok(&fixtures::five_hz_tick(seq));
            assert!(
                outcome.notices.is_empty(),
                "frame {seq} produced {:?}",
                codes(&outcome)
            );
            assert_eq!(outcome.events.len(), 1, "frame {seq} publishes one event");
            events.extend(outcome.events);
        }
        assert_eq!(events.len(), usize::try_from(FRAMES).expect("fits"));
        assert!(
            events
                .iter()
                .all(|event| event.source_id == VALUE_5HZ_SOURCE_ID)
        );
        // The per-subscription `seq` is carried through verbatim and the
        // adapter's own sequence is contiguous from 0.
        assert_eq!(events[0].values["upstream_sequence"], json!(1_u64));
        assert_eq!(events[0].values["index_value"], json!("64250.11000000"));
        assert_eq!(
            events[usize::try_from(FRAMES).expect("fits") - 1].values["upstream_sequence"],
            json!(FRAMES)
        );
        assert_eq!(
            events
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            (0..FRAMES).collect::<Vec<_>>()
        );
        // The observed 200 ms grid survives into `event_time`.
        assert_eq!(events[0].event_time, "2026-08-29T10:45:00.200Z");
        assert_eq!(events[1].event_time, "2026-08-29T10:45:00.400Z");
        assert_eq!(events[4].event_time, "2026-08-29T10:45:01.000Z");
    }

    /// The 5 Hz channel carries no `avg_60s_data` and no quarter object: its
    /// `msg` key set is closed at the five observed keys, so a frame carrying
    /// the 1 Hz channel's fields is refused rather than half-read.
    #[test]
    fn the_five_hz_body_is_closed_at_the_observed_key_set() {
        let mut stream = Stream::ready();
        let with_average = fixtures::five_hz_tick(1).replace(
            r#","data":"#,
            r#","avg_60s_data":{"value":"64249.10000000","window_size":60,"window_start_ts_ms":1788000240000,"window_end_ts_exclusive":1788000300200},"data":"#,
        );
        assert!(matches!(
            stream.text(&with_average),
            Err(AdapterError::InvalidPayload(_))
        ));
        // The 8-scale rule on `value_usd` holds; the embedded value is free.
        assert!(matches!(
            stream.text(&fixtures::five_hz_tick(1).replace("64250.11000000", "64250.11")),
            Err(AdapterError::InvalidPayload(_))
        ));
        let free_scale = stream.ok(&fixtures::five_hz_tick_at(
            1,
            fixtures::FIVE_HZ_FIRST_MS,
            "64250.1",
        ));
        assert_eq!(free_scale.events.len(), 1, "a 1-scale tick value is fine");
    }

    /// Ruling E25 (b): a code-8 refusal on the 5 Hz subscribe is one notice, one
    /// unavailable surface, and nothing else. The market channels and the 1 Hz
    /// channel keep publishing, the lane never fails, and the next connection
    /// re-attempts the subscribe — which is exactly how the R1 hour's fourth
    /// connection got the channel after three refusals.
    #[test]
    fn a_refused_five_hz_subscribe_is_tolerated_and_retried_on_reconnect() {
        let mut stream = Stream::new();
        stream.connect();
        stream.ok(&fixtures::ack(VALUE_CHANNEL));
        stream.ok(&fixtures::ack(PYTH_CHANNEL));
        let refused = stream.ok(&fixtures::error(
            VALUE_5HZ_COMMAND_ID,
            8,
            "Unknown channel name",
        ));
        assert_eq!(codes(&refused), vec![FeedNoticeCode::SubscriptionRejected]);
        assert!(refused.notices[0].detail.contains(VALUE_5HZ_CHANNEL));
        assert!(
            stream.adapter.value_5hz_unavailable(),
            "the 5 Hz surface is unavailable, not merely idle"
        );

        // The 1 Hz channel is untouched.
        let tick = stream.ok(&fixtures::value_tick(1, false));
        assert_eq!(tick.events.len(), 1);
        assert_eq!(tick.events[0].source_id, VALUE_SOURCE_ID);

        // An acceptance on the same connection would clear it too; the next
        // connection re-attempts it in any case, and the venue accepts.
        stream.reconnect();
        assert!(
            !stream.adapter.value_5hz_unavailable(),
            "a refusal is connection-local"
        );
        for channel in fixtures::ALL_CHANNELS {
            stream.ok(&fixtures::ack(channel));
        }
        let accepted = stream.ok(&fixtures::five_hz_tick(1));
        assert_eq!(accepted.events.len(), 1);
        assert_eq!(accepted.events[0].source_id, VALUE_5HZ_SOURCE_ID);
        assert!(!stream.adapter.value_5hz_unavailable());
    }

    /// The refusal has to reach a consumer, and the only carrier is the notice.
    /// It travels as typed data — never as prose, because `detail` is
    /// explicitly not part of the contract (ruling E25).
    #[test]
    fn a_subscription_notice_names_the_source_it_makes_unavailable() {
        let mut stream = Stream::new();
        stream.connect();
        let refused = stream.ok(&fixtures::error(
            VALUE_5HZ_COMMAND_ID,
            8,
            "Unknown channel name",
        ));
        assert_eq!(
            refused.notices[0].subscription,
            Some(SubscriptionOutcome {
                source_id: VALUE_5HZ_SOURCE_ID,
                accepted: false,
                code: Some(8),
            })
        );
        let accepted = stream.ok(&fixtures::ack(VALUE_5HZ_CHANNEL));
        assert_eq!(
            accepted.notices[0].subscription,
            Some(SubscriptionOutcome {
                source_id: VALUE_5HZ_SOURCE_ID,
                accepted: true,
                code: None,
            })
        );
        // The 1 Hz channel and the relay name their own identities.
        for (channel, source_id) in [
            (VALUE_CHANNEL, VALUE_SOURCE_ID),
            (PYTH_CHANNEL, PYTH_VALUE_SOURCE_ID),
        ] {
            let outcome = stream.ok(&fixtures::ack(channel));
            assert_eq!(
                outcome.notices[0].subscription.map(|s| s.source_id),
                Some(source_id)
            );
        }
        // A reply whose id names no index channel carries no routable outcome.
        let unknown = stream.ok(&fixtures::error(3, 8, "Unknown channel name"));
        assert_eq!(codes(&unknown), vec![FeedNoticeCode::SubscriptionRejected]);
        assert_eq!(unknown.notices[0].subscription, None);
        // Neither does a data-path notice.
        let gap = stream.ok(&at(&fixtures::value_tick(9, false), 1_788_000_305_000));
        assert!(
            gap.notices
                .iter()
                .all(|notice| notice.subscription.is_none())
        );
    }

    /// A code-8 refusal on any index channel is a notice naming that channel,
    /// never a lane failure — the property that let the R1 hour keep running
    /// through three 5 Hz refusals and pick the channel up on the fourth
    /// connection (rulings E23, E25).
    #[test]
    fn a_rejected_subscription_names_its_channel_and_does_not_fail_the_lane() {
        let mut stream = Stream::ready();
        let outcome = stream.ok(&fixtures::error(
            VALUE_COMMAND_ID,
            8,
            "Unknown channel name",
        ));
        assert!(outcome.events.is_empty());
        assert_eq!(
            codes(&outcome),
            vec![FeedNoticeCode::SubscriptionRejected],
            "a rejected subscribe is reported, not raised"
        );
        let notice = &outcome.notices[0];
        assert!(notice.detail.contains(VALUE_CHANNEL), "{notice:?}");
        assert!(notice.detail.contains("code 8"), "{notice:?}");
        assert!(notice.detail.contains("Unknown channel name"), "{notice:?}");
        // An id outside the reserved band cannot be attributed to a channel, and
        // is still reported rather than raised.
        let unknown = stream.ok(&fixtures::error(3, 8, "Unknown channel name"));
        assert_eq!(codes(&unknown), vec![FeedNoticeCode::SubscriptionRejected]);
        assert!(unknown.notices[0].detail.contains("command id 3"));
        // The lane keeps publishing afterwards.
        assert_eq!(stream.ok(&fixtures::value_tick(10, false)).events.len(), 1);
    }

    /// Observed (E23): the trailing average opens at `window_size` 0 on a fresh
    /// subscription and grows. The documented assumption was `>= 1`, which would
    /// have refused the very first frame of every run.
    #[test]
    fn trailing_window_size_zero_is_accepted_and_grows() {
        let mut stream = Stream::ready();
        let first = stream.ok(&fixtures::value_tick_window(1, false, 0));
        assert_eq!(first.events.len(), 1, "a fresh window is not an error");
        assert_eq!(first.events[0].values["avg_60s_window_size"], json!(0_u32));
        let grown = stream.ok(&at(
            &fixtures::value_tick_window(2, false, 1),
            1_788_000_301_000,
        ));
        assert_eq!(grown.events[0].values["avg_60s_window_size"], json!(1_u32));
    }

    /// The Pyth relay is refuse-closed: the probe never delivered a
    /// `pyth_value` frame, so the body shape is documented-only and nothing is
    /// published under that identity until one is captured (ruling E23).
    #[test]
    fn the_pyth_relay_is_parsed_but_publishes_nothing_while_unobserved() {
        let mut stream = Stream::ready();
        let outcome = stream.ok(&fixtures::pyth_tick(30));
        assert!(
            outcome.events.is_empty(),
            "an unobserved contract publishes nothing"
        );
        assert_eq!(codes(&outcome), vec![FeedNoticeCode::UnsupportedMessage]);
        assert!(
            outcome.notices[0].detail.contains(PYTH_VALUE_SOURCE_ID),
            "the notice names the withheld identity"
        );
        // The shape is still enforced, so a real frame is reported the moment it
        // stops matching the model.
        let malformed = stream.text(&fixtures::pyth_tick(31).replace(
            "\"value_usd\":\"64251.44000000\"",
            "\"value_usd\":\"64251.44\"",
        ));
        assert!(matches!(malformed, Err(AdapterError::InvalidPayload(_))));
    }

    #[test]
    fn quarter_object_absent_emits_one_event() {
        let mut stream = Stream::ready();
        let outcome = stream.ok(&fixtures::value_tick(10, false));
        assert_eq!(outcome.events.len(), 1);
        assert_eq!(outcome.events[0].source_id, VALUE_SOURCE_ID);
    }

    #[test]
    fn quarter_object_present_emits_two_events() {
        let mut stream = Stream::ready();
        let outcome = stream.ok(&fixtures::value_tick(10, true));
        assert_eq!(outcome.events.len(), 2);
        assert_eq!(outcome.events[0].source_id, VALUE_SOURCE_ID);
        assert_eq!(outcome.events[1].source_id, SETTLEMENT_WINDOW_SOURCE_ID);
        // Both carry the same event_time: the tick's index time.
        assert_eq!(outcome.events[0].event_time, outcome.events[1].event_time);
    }

    /// Ruling E28: the wire start is fixed for a quarter and the wire end
    /// advances with every tick, so the quarter's identity is DERIVED from the
    /// start. This walks a whole quarter — the shape and length of each of the
    /// four quarters in the observed hour — and checks that all 60 ticks carry
    /// one stable `quarter_close_time` while `window_as_of` advances behind it.
    #[test]
    fn a_whole_quarter_accumulates_under_one_derived_close() {
        let mut stream = Stream::ready();
        let close = fixtures::QUARTER_CLOSE_MS;
        let start = close - 60_000;
        let expected_close = millis_to_rfc3339(close, "close").expect("in range");
        let mut complete = 0_u32;
        for size in 1..=60_u32 {
            let index_time = start + i64::from(size) * 1_000;
            let outcome = stream.ok(&quarter_message(
                11 + u64::from(size),
                index_time,
                close,
                size,
            ));
            let event = outcome
                .events
                .iter()
                .find(|event| event.source_id == SETTLEMENT_WINDOW_SOURCE_ID)
                .expect("a quarter object emits a settlement-window event");
            assert_eq!(event.values["window_size"], json!(size));
            // Fixed for the whole quarter.
            assert_eq!(
                event.values["window_start_exclusive"],
                json!(millis_to_rfc3339(start, "start").expect("in range"))
            );
            // Derived, and identical on all 60 ticks — this is the dedup key.
            assert_eq!(event.values["quarter_close_time"], json!(expected_close));
            assert_eq!(event.values["window_end_inclusive"], json!(expected_close));
            // Running, and behind the close until the closing tick.
            assert_eq!(
                event.values["window_as_of"],
                json!(millis_to_rfc3339(index_time, "as of").expect("in range"))
            );
            if event.values["window_as_of"] == event.values["quarter_close_time"] {
                complete += 1;
                assert_eq!(size, 60, "the window completes on the closing tick only");
            }
        }
        assert_eq!(complete, 1, "exactly one complete window per quarter");
    }

    /// The observed hour carried 240 quarter objects across four quarter closes.
    /// Every one is accepted, each quarter publishes one stable
    /// `quarter_close_time`, and exactly **4** of the 240 are complete windows.
    ///
    /// Values are synthetic; the shape, the four closes, the fixed starts, the
    /// advancing as-ofs and the 1…60 size vector are the observed ones.
    #[test]
    fn the_observed_hour_of_quarter_objects_is_accepted_end_to_end() {
        // The four quarter closes the hour crossed, an hour apart in the
        // fixture's own timeline so each is a fresh quarter.
        let closes: Vec<i64> = (0..4)
            .map(|q| fixtures::QUARTER_CLOSE_MS + q * QUARTER_HOUR_MILLIS)
            .collect();
        let mut stream = Stream::ready();
        let mut seq = 100_u64;
        let mut objects = 0_u32;
        let mut complete = 0_u32;
        let mut closes_seen = std::collections::BTreeSet::new();
        for close in &closes {
            assert_eq!(close % QUARTER_HOUR_MILLIS, 0, "fixture closes are on grid");
            for size in 1..=60_u32 {
                let index_time = close - 60_000 + i64::from(size) * 1_000;
                seq += 1;
                let outcome = stream.ok(&quarter_message(seq, index_time, *close, size));
                let event = outcome
                    .events
                    .iter()
                    .find(|event| event.source_id == SETTLEMENT_WINDOW_SOURCE_ID)
                    .expect("every observed quarter object is accepted");
                objects += 1;
                closes_seen.insert(
                    event.values["quarter_close_time"]
                        .as_str()
                        .expect("the derived close is a timestamp string")
                        .to_owned(),
                );
                if event.values["window_as_of"] == event.values["quarter_close_time"] {
                    complete += 1;
                }
            }
        }
        assert_eq!(objects, 240, "the observed hour's quarter-object count");
        assert_eq!(closes_seen.len(), 4, "four stable quarter keys, not 240");
        assert_eq!(complete, 4, "exactly one complete window per quarter close");
    }

    /// `window_end_inclusive` and `quarter_close_time` remain one instant under
    /// two semantic names — but under ruling E28 that instant is DERIVED from
    /// `window_start_ts_ms`, not read from the wire, and the wire's own end is
    /// published separately as the running `window_as_of`.
    #[test]
    fn window_end_inclusive_equals_the_derived_quarter_close_time() {
        let mut stream = Stream::ready();
        let outcome = stream.ok(&fixtures::value_tick(10, true));
        let event = &outcome.events[1];
        assert_eq!(
            event.values["window_end_inclusive"],
            event.values["quarter_close_time"]
        );
        assert_eq!(
            event.values["quarter_close_time"],
            json!("2026-08-29T10:45:00.000Z")
        );
        // This fixture is a closing tick, so the running as-of has reached the
        // close: the window is complete.
        assert_eq!(
            event.values["window_as_of"],
            event.values["quarter_close_time"]
        );
        assert_eq!(event.values.len(), 11, "every declared field is emitted");
    }

    /// Ruling E28's one hard refusal: a start that does not sit exactly one
    /// minute before a `:00/:15/:30/:45` boundary derives an off-grid close and
    /// is not a quarter window at all, so the frame is refused rather than
    /// published under a fabricated key.
    #[test]
    fn a_quarter_object_whose_derived_close_is_off_grid_is_refused() {
        let close = fixtures::QUARTER_CLOSE_MS;
        for offset in [1_000_i64, -1_000, 450_000] {
            let mut fresh = Stream::ready();
            // Shift the window start off the grid; the derived close moves with
            // it and stops landing on a quarter boundary.
            let payload = quarter_message(40, close - 1_000, close, 30).replace(
                &format!(r#""window_start_ts_ms":{}"#, close - 60_000),
                &format!(r#""window_start_ts_ms":{}"#, close - 60_000 + offset),
            );
            let refusal = fresh.text(&payload);
            assert!(
                matches!(&refusal, Err(AdapterError::InvalidPayload(message))
                    if message.contains(":00/:15/:30/:45")),
                "offset {offset} must be refused, got {refusal:?}"
            );
        }
        // On the grid, the same frame is accepted.
        let mut fresh = Stream::ready();
        let accepted = fresh.ok(&quarter_message(40, close - 1_000, close, 30));
        assert!(
            accepted
                .events
                .iter()
                .any(|event| event.source_id == SETTLEMENT_WINDOW_SOURCE_ID)
        );
    }

    #[test]
    fn avg_window_end_must_equal_data_time() {
        let mut stream = Stream::ready();
        let payload = fixtures::value_tick(10, false).replace(
            r#""window_end_ts_exclusive":1788000300000}"#,
            r#""window_end_ts_exclusive":1788000301000}"#,
        );
        assert!(matches!(
            stream.text(&payload),
            Err(AdapterError::InvalidPayload(_))
        ));
    }

    #[test]
    fn data_id_mismatch_is_state_inconsistent() {
        let mut stream = Stream::ready();
        let payload =
            fixtures::value_tick(10, false).replace(r#"\"id\":\"BRTI\""#, r#"\"id\":\"BRTIX\""#);
        let outcome = stream.ok(&payload);
        assert!(outcome.events.is_empty());
        assert_eq!(codes(&outcome), vec![FeedNoticeCode::StateInconsistent]);
    }

    /// F1/E23: the cross-channel equality invariant is **withdrawn with the
    /// 5 Hz channel**, not repaired. It compared one price seen on two channels
    /// and there is now only one channel, so §6.4 is not applicable until
    /// `cfbenchmarks_value_5hz` is entitled. The audit's blocker — the
    /// comparison was made on decimal *text*, so `64250.11` and
    /// `64250.11000000` disagreed — is gone with it, and `decimals_equal`
    /// remains the only way two prices are compared here.
    #[test]
    fn one_price_spelled_at_two_scales_compares_equal() {
        assert!(decimals_equal("64250.11", "64250.11000000").expect("both parse"));
        assert!(!decimals_equal("64250.11", "64260.11000000").expect("both parse"));
    }

    /// A repair row that restates an accepted live value at a different scale is
    /// the same price, not a conflict.
    #[test]
    fn a_repair_row_agrees_with_a_live_value_across_scales() {
        let mut stream = Stream::ready();
        stream.ok(&fixtures::value_tick(10, false));
        stream.ok(&gap_quarter(20));
        assert!(stream.adapter.settlement_window_paused());
        let rows: Vec<KalshiBrtiHistoryRow> = (1..=5)
            .map(|step| KalshiBrtiHistoryRow {
                time: fixtures::INDEX_TIME_MS + step * 1_000,
                value: "64250.11000000".to_owned(),
            })
            .chain(std::iter::once(KalshiBrtiHistoryRow {
                time: fixtures::NEXT_QUARTER_TICK_MS,
                value: "64250.11000000".to_owned(),
            }))
            .collect();
        // The live frontier holds "64250.11"; the row spells it at scale 8.
        let outcome = stream
            .adapter
            .apply_history_backfill(&rows)
            .expect("scale is not a conflict");
        assert_eq!(codes(&outcome), vec![FeedNoticeCode::VenueStatus]);
        assert!(!stream.adapter.settlement_window_paused());
    }

    #[test]
    fn seq_duplicate_out_of_order_and_gap() {
        let mut stream = Stream::ready();
        stream.ok(&fixtures::value_tick(10, false));
        // +1 is accepted.
        let accepted = stream.ok(&at(&fixtures::value_tick(11, false), 1_788_000_301_000));
        assert_eq!(accepted.events.len(), 1);
        // A repeat of the last seq is a duplicate.
        let duplicate = stream.ok(&at(&fixtures::value_tick(11, false), 1_788_000_302_000));
        assert_eq!(codes(&duplicate), vec![FeedNoticeCode::Duplicate]);
        assert!(duplicate.events.is_empty());
        // Further back is out of order.
        let stale = stream.ok(&at(&fixtures::value_tick(5, false), 1_788_000_303_000));
        assert_eq!(codes(&stale), vec![FeedNoticeCode::OutOfOrder]);
        // A skip is a gap.
        let gap = stream.ok(&at(&fixtures::value_tick(20, false), 1_788_000_304_000));
        assert!(codes(&gap).contains(&FeedNoticeCode::Gap));
    }

    /// A value surface loses the missed ticks rather than repairing them:
    /// nothing downstream integrates them, so publication continues from the
    /// newest valid image. The `Reconnect` action is an observability marker —
    /// `FeedNotice::recovery` requires one and the enum has no no-op variant.
    #[test]
    fn gap_on_value_surface_is_observability_only() {
        let mut stream = Stream::ready();
        stream.ok(&fixtures::value_tick(20, false));
        let gap = stream.ok(&at(&fixtures::value_tick(25, false), 1_788_000_301_000));
        let notice = gap
            .notices
            .iter()
            .find(|notice| notice.code == FeedNoticeCode::Gap)
            .expect("the gap is reported");
        assert_eq!(notice.recovery, Some(RecoveryAction::Reconnect));
        assert_eq!(
            gap.events.len(),
            1,
            "the value surface publishes from the newest valid image"
        );
        assert_eq!(gap.events[0].source_id, VALUE_SOURCE_ID);
    }

    /// Every print inside a quarter window is load-bearing, so this surface
    /// fails closed where the value surfaces do not.
    #[test]
    fn gap_on_settlement_window_pauses_with_backfill() {
        let mut stream = Stream::ready();
        stream.ok(&fixtures::value_tick(10, false));
        let gap = stream.ok(&gap_quarter(20));
        // Two notices: the value surface's observability-only gap, and the
        // settlement window's fail-closed repair request.
        let backfill = gap
            .notices
            .iter()
            .filter_map(|notice| notice.recovery.clone())
            .find(|action| matches!(action, RecoveryAction::BackfillKalshiBrti { .. }))
            .expect("a settlement gap asks for a repair");
        assert_eq!(
            backfill,
            RecoveryAction::BackfillKalshiBrti {
                index_id: "BRTI".to_owned(),
                after_index_time: fixtures::INDEX_TIME_MS,
                through_index_time: fixtures::NEXT_QUARTER_TICK_MS,
            }
        );
        assert!(stream.adapter.settlement_window_paused());
        assert!(
            gap.events
                .iter()
                .all(|event| event.source_id != SETTLEMENT_WINDOW_SOURCE_ID),
            "the paused surface publishes nothing"
        );
    }

    #[test]
    fn reconnect_resets_seq_keeps_frontier() {
        let mut stream = Stream::ready();
        stream.ok(&fixtures::value_tick(10, false));
        stream.ordinal += 1;
        let disconnect = RawRecord::disconnected_at(
            CaptureStream::KalshiIndex,
            stream.epoch,
            stream.ordinal,
            RECEIVED_AT.to_owned(),
            3_000,
        );
        stream
            .adapter
            .ingest(&disconnect)
            .expect("disconnect is accepted");
        stream.connect();
        // The sid binding is gone, so a message before a fresh ack is refused.
        let unbound = stream.ok(&fixtures::value_tick(1, false));
        assert_eq!(codes(&unbound), vec![FeedNoticeCode::StateInconsistent]);
        stream.ok(&fixtures::ack_parts(VALUE_COMMAND_ID, 7, VALUE_CHANNEL));
        // The `seq` cursor restarts from whatever the new subscription gives...
        let fresh = stream.ok(&at(
            &fixtures::value_tick(1, false).replace("\"sid\":1", "\"sid\":7"),
            1_788_000_309_000,
        ));
        assert_eq!(fresh.events.len(), 1);
        // ...but the event_time frontier survived, so a pre-frontier tick is
        // still out of order.
        let stale = stream.ok(&at(
            &fixtures::value_tick(2, false).replace("\"sid\":1", "\"sid\":7"),
            1_788_000_308_000,
        ));
        assert_eq!(codes(&stale), vec![FeedNoticeCode::OutOfOrder]);
    }

    #[test]
    fn same_event_time_different_value_is_state_inconsistent() {
        let mut stream = Stream::ready();
        stream.ok(&fixtures::value_tick(10, false));
        let changed = stream.ok(&fixtures::value_tick(11, false)
            .replace(r#"\"value\":\"64250.11\""#, r#"\"value\":\"64250.12\""#));
        assert!(changed.events.is_empty(), "never a silent overwrite");
        assert!(codes(&changed).contains(&FeedNoticeCode::StateInconsistent));
        // The byte-identical repeat is a plain duplicate, not an inconsistency.
        let repeat = stream.ok(&fixtures::value_tick(12, false));
        assert_eq!(codes(&repeat), vec![FeedNoticeCode::Duplicate]);
    }

    /// Round 2 ruling E13: a non-increasing window size inside one quarter is a
    /// state-inconsistency notice with the frame dropped, never an adapter error
    /// that stops the lane.
    #[test]
    fn quarter_window_size_must_increase_within_one_close() {
        let mut stream = Stream::ready();
        let close = fixtures::QUARTER_CLOSE_MS;
        stream.ok(&quarter_message(11, close - 30_000, close, 30));
        let repeated = stream.ok(&quarter_message(12, close - 29_000, close, 30));
        assert!(
            repeated
                .events
                .iter()
                .all(|event| event.source_id != SETTLEMENT_WINDOW_SOURCE_ID)
        );
        assert!(codes(&repeated).contains(&FeedNoticeCode::StateInconsistent));
        // The paired value event still publishes: the lane does not stop.
        assert_eq!(repeated.events.len(), 1);
    }

    #[test]
    fn configuration_record_refused() {
        let mut adapter =
            KalshiIndexAdapter::with_default_configuration(EPOCH).expect("constructed");
        let record = RawRecord::configuration_at(
            CaptureStream::KalshiIndex,
            1,
            1,
            "{}".to_owned(),
            RECEIVED_AT.to_owned(),
            0,
        );
        assert!(matches!(
            adapter.ingest(&record),
            Err(AdapterError::InvalidRecord(_))
        ));
    }

    /// One smallest representable unit either side of a boundary survives parse
    /// and emission with its text intact.
    #[test]
    fn decimal_boundary_survives_round_trip() {
        let mut stream = Stream::ready();
        for (offset, value) in ["0.00000001", "99999999.99999999"].into_iter().enumerate() {
            let step = i64::try_from(offset).expect("two values fit") + 1;
            let payload = fixtures::value_tick(10 + offset as u64, false).replace(
                r#"\"value\":\"64250.11\""#,
                &format!(r#"\"value\":\"{value}\""#),
            );
            let payload = at(&payload, fixtures::INDEX_TIME_MS + step * 1_000);
            let outcome = stream.ok(&payload);
            assert_eq!(outcome.events[0].values["index_value"], json!(value));
            let json = serde_json::to_string(&outcome.events[0]).expect("event serializes");
            assert!(json.contains(&format!("\"{value}\"")), "{json}");
        }
    }

    #[test]
    fn message_size_bound_enforced() {
        let mut stream = Stream::ready();
        // A message exactly at the bound is accepted; one byte more is refused.
        // Padding rides an unmodelled message type so the size gate — not the
        // field model — is what the test exercises.
        let prefix = r#"{"type":"cfbenchmarks_value_indexlist","pad":""#;
        let suffix = r#""}"#;
        let pad = MAX_INDEX_MESSAGE_BYTES - prefix.len() - suffix.len();
        let at_bound = format!("{prefix}{}{suffix}", "a".repeat(pad));
        assert_eq!(at_bound.len(), MAX_INDEX_MESSAGE_BYTES);
        assert_eq!(
            codes(&stream.ok(&at_bound)),
            vec![FeedNoticeCode::UnsupportedMessage]
        );
        let over = format!("{prefix}{}{suffix}", "a".repeat(pad + 1));
        assert!(matches!(
            stream.text(&over),
            Err(AdapterError::InvalidPayload(_))
        ));
    }

    #[test]
    fn history_row_bound_enforced() {
        let mut stream = Stream::ready();
        stream.ok(&fixtures::value_tick(10, false));
        stream.ok(&gap_quarter(20));
        let rows: Vec<KalshiBrtiHistoryRow> = (0..=MAX_HISTORY_ROWS)
            .map(|index| KalshiBrtiHistoryRow {
                time: fixtures::INDEX_TIME_MS + index as i64 + 1,
                value: "64250.11".to_owned(),
            })
            .collect();
        assert!(matches!(
            stream.adapter.apply_history_backfill(&rows),
            Err(AdapterError::InvalidPayload(_))
        ));
    }

    #[test]
    fn backfill_not_covering_interval_fails_repair() {
        let mut stream = Stream::ready();
        stream.ok(&fixtures::value_tick(10, false));
        stream.ok(&gap_quarter(20));
        assert!(stream.adapter.settlement_window_paused());
        // Short of the upper bound: a repair failure, never a partial apply.
        let short: Vec<KalshiBrtiHistoryRow> = (1..5)
            .map(|index| KalshiBrtiHistoryRow {
                time: fixtures::INDEX_TIME_MS + index * 1_000,
                value: "64250.11".to_owned(),
            })
            .collect();
        let outcome = stream
            .adapter
            .apply_history_backfill(&short)
            .expect("a short response is a repair failure, not an error");
        assert!(outcome.events.is_empty());
        assert!(stream.adapter.settlement_window_paused());
        // Covering the whole interval resumes the surface.
        let mut full: Vec<KalshiBrtiHistoryRow> = (1..5)
            .map(|index| KalshiBrtiHistoryRow {
                time: fixtures::INDEX_TIME_MS + index * 1_000,
                value: "64250.11".to_owned(),
            })
            .collect();
        full.push(KalshiBrtiHistoryRow {
            time: fixtures::NEXT_QUARTER_TICK_MS,
            value: "64250.11".to_owned(),
        });
        let outcome = stream
            .adapter
            .apply_history_backfill(&full)
            .expect("a covering response repairs");
        assert!(outcome.events.is_empty(), "repair rows are never published");
        assert!(!stream.adapter.settlement_window_paused());
    }

    /// A row that disagrees with an accepted live value is a state
    /// inconsistency, never an overwrite.
    #[test]
    fn backfill_row_conflicting_with_a_live_value_is_refused() {
        let mut stream = Stream::ready();
        stream.ok(&fixtures::value_tick(10, false));
        stream.ok(&gap_quarter(20));
        let rows = vec![KalshiBrtiHistoryRow {
            time: fixtures::NEXT_QUARTER_TICK_MS,
            value: "64250.12".to_owned(),
        }];
        assert!(matches!(
            stream.adapter.apply_history_backfill(&rows),
            Err(AdapterError::InvalidPayload(_))
        ));
    }

    #[test]
    fn passthrough_per_gap_and_per_hour_budgets() {
        let mut stream = Stream::ready();
        stream.ok(&fixtures::value_tick(10, false));
        stream.ok(&gap_quarter(20));
        for attempt in 1..=PASSTHROUGH_MAX_PER_GAP {
            stream
                .adapter
                .charge_passthrough_attempt()
                .unwrap_or_else(|_| panic!("attempt {attempt} is inside the per-gap budget"));
        }
        assert!(
            stream.adapter.charge_passthrough_attempt().is_err(),
            "the 13th attempt in one gap is refused without a network call"
        );
        // A fresh gap resets the per-gap budget but never the hourly one, so
        // the hourly ceiling still binds across gaps.
        let mut charged = PASSTHROUGH_MAX_PER_GAP;
        while charged < PASSTHROUGH_MAX_PER_HOUR {
            stream.adapter.passthrough.in_gap = 0;
            while charged < PASSTHROUGH_MAX_PER_HOUR
                && stream.adapter.passthrough.in_gap < PASSTHROUGH_MAX_PER_GAP
            {
                stream
                    .adapter
                    .charge_passthrough_attempt()
                    .expect("inside both budgets");
                charged += 1;
            }
        }
        assert_eq!(charged, PASSTHROUGH_MAX_PER_HOUR);
        stream.adapter.passthrough.in_gap = 0;
        assert!(
            stream.adapter.charge_passthrough_attempt().is_err(),
            "the 61st attempt in one hour is refused without a network call"
        );
        stream.adapter.note_passthrough_hour_rollover();
        assert!(stream.adapter.charge_passthrough_attempt().is_ok());
    }

    /// No `*_list` command is sent in wave A, so no list reply is modelled: it
    /// is retained in the capture and produces no event.
    #[test]
    fn indexlist_message_is_unsupported_not_modelled() {
        let mut stream = Stream::ready();
        for payload in [
            r#"{"type":"cfbenchmarks_value_indexlist","sid":1,"msg":{"index_ids":["BRTI"]}}"#,
            r#"{"type":"cfbenchmarks_value_5hz_indexlist","sid":2,"msg":{"index_ids":["BRTI"]}}"#,
            r#"{"type":"pyth_value_underlying_list","sid":3,"msg":{"underlying_tickers":["Crypto.BTC/USD"]}}"#,
        ] {
            let outcome = stream.ok(payload);
            assert!(outcome.events.is_empty());
            assert_eq!(codes(&outcome), vec![FeedNoticeCode::UnsupportedMessage]);
        }
    }

    #[test]
    fn rejects_malformed_and_out_of_contract_messages() {
        let mut stream = Stream::ready();
        // Structural refusals: not usable input at all.
        for payload in [
            "",
            "[]",
            "\"text\"",
            r#"{"sid":1,"seq":1}"#,
            r#"{"type":"cfbenchmarks_value","sid":1,"seq":1,"msg":{},"extra":1}"#,
        ] {
            assert!(
                matches!(stream.text(payload), Err(AdapterError::InvalidPayload(_))),
                "{payload} must be refused"
            );
        }

        // Field-level refusals, one per modelled surface.
        let base = fixtures::value_tick(10, false);
        for (name, payload) in [
            (
                "unknown field in msg",
                base.replace(r#""index_id":"BRTI""#, r#""index_id":"BRTI","extra":1"#),
            ),
            (
                "unknown field in msg.data",
                base.replace(r#"\"type\":\"value\""#, r#"\"type\":\"value\",\"extra\":1"#),
            ),
            (
                "non-decimal value",
                base.replace(r#"\"value\":\"64250.11\""#, r#"\"value\":\"64,250.11\""#),
            ),
            (
                "zero value",
                base.replace(r#"\"value\":\"64250.11\""#, r#"\"value\":\"0\""#),
            ),
            (
                "negative value",
                base.replace(r#"\"value\":\"64250.11\""#, r#"\"value\":\"-1.0\""#),
            ),
            (
                "nine fractional digits",
                base.replace(r#"\"value\":\"64250.11\""#, r#"\"value\":\"1.123456789\""#),
            ),
            (
                "trailing average not at eight digits",
                base.replace(r#""value":"64249.10000000""#, r#""value":"64249.1""#),
            ),
            (
                "timestamp below the plausibility window",
                base.replace(r#"\"time\":1788000300000"#, r#"\"time\":999999999999"#),
            ),
            (
                "timestamp above the plausibility window",
                base.replace("1788000300123", "4200000000000"),
            ),
            (
                "window size over the trailing bound",
                base.replace(r#""window_size":60"#, r#""window_size":601"#),
            ),
            (
                "unsubscribed index id",
                base.replace(r#""index_id":"BRTI""#, r#""index_id":"ETHUSD_RR""#),
            ),
        ] {
            assert!(
                matches!(stream.text(&payload), Err(AdapterError::InvalidPayload(_))),
                "{name} must be refused"
            );
        }

        // Quarter-object refusals. Each runs on its own stream: the frontier is
        // advanced before the quarter object is read, so a second message at the
        // same index time on one stream would be dropped as a repeat before its
        // quarter object was ever examined.
        let close = fixtures::QUARTER_CLOSE_MS;
        for (name, payload) in [
            (
                "quarter window size over sixty",
                quarter_message(40, close - 1_000, close, 61),
            ),
            (
                // The derived close is `start + 60s`; a start 30 s late derives
                // a close 30 s off the grid.
                "quarter window start off the grid, so the derived close is too",
                quarter_message(42, close - 1_000, close, 30).replace(
                    &format!(r#""window_start_ts_ms":{}"#, close - 60_000),
                    &format!(r#""window_start_ts_ms":{}"#, close - 30_000),
                ),
            ),
            (
                "quarter as-of beyond its derived close",
                quarter_message(43, close + 1_000, close, 30),
            ),
        ] {
            let mut fresh = Stream::ready();
            assert!(
                matches!(fresh.text(&payload), Err(AdapterError::InvalidPayload(_))),
                "{name} must be refused"
            );
        }

        // Pyth refusals: the shape is enforced even though nothing is published.
        assert!(matches!(
            stream.text(&fixtures::pyth_tick(30).replace("Crypto.BTC/USD", "Crypto.ETH/USD")),
            Err(AdapterError::InvalidPayload(_))
        ));
        assert!(matches!(
            stream.text(&fixtures::pyth_tick(31).replace(
                "\"received_at\":1788000300090",
                "\"received_at\":1788000300010"
            )),
            Err(AdapterError::InvalidPayload(_))
        ));
    }

    #[test]
    fn rejects_error_and_conflicting_acknowledgements() {
        let mut stream = Stream::ready();
        let rejected = stream.ok(&fixtures::error(PYTH_COMMAND_ID, 24, "index not entitled"));
        assert_eq!(codes(&rejected), vec![FeedNoticeCode::SubscriptionRejected]);
        assert!(rejected.notices[0].detail.contains(PYTH_CHANNEL));
        // A second ack for a bound channel on a different sid is an
        // inconsistency, surfaced as one StateInconsistent notice.
        let conflicting = stream.ok(&fixtures::ack_parts(VALUE_COMMAND_ID, 9, VALUE_CHANNEL));
        assert_eq!(codes(&conflicting), vec![FeedNoticeCode::StateInconsistent]);
        // The channel REBINDS to the new sid rather than the outage lasting the
        // whole connection: a frame on sid 9 now flows.
        let on_new_sid = fixtures::value_tick(10, false).replace(r#""sid":1"#, r#""sid":9"#);
        assert_eq!(stream.ok(&on_new_sid).events.len(), 1);
    }

    #[test]
    fn configured_sets_are_bounded_and_unique() {
        assert!(KalshiIndexAdapter::new(EPOCH, &[], &["Crypto.BTC/USD"]).is_err());
        assert!(KalshiIndexAdapter::new(EPOCH, &["BRTI", "BRTI"], &["Crypto.BTC/USD"]).is_err());
        assert!(KalshiIndexAdapter::new(EPOCH, &["brti"], &["Crypto.BTC/USD"]).is_err());
        let too_many: Vec<&str> = vec!["A", "B", "C", "D", "E", "F", "G", "H", "I"];
        assert!(KalshiIndexAdapter::new(EPOCH, &too_many, &["Crypto.BTC/USD"]).is_err());
        assert!(KalshiIndexAdapter::new(EPOCH, &["BRTI"], &[]).is_err());
    }

    #[test]
    fn passthrough_history_envelope_parses_and_is_bounded() {
        let rows = parse_history_response(
            r#"{"data":{"values":[{"time":1788000301000,"value":"64250.11"},{"time":1788000302000,"value":"64250.12"}]}}"#,
        )
        .expect("the documented envelope parses");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].value, "64250.11");
        assert!(parse_history_response(r#"{"data":{"values":[],"extra":1}}"#).is_err());
        assert!(parse_history_response("{}").is_err());
    }

    /// Two adapters driven by the same record prefix reach the same pause: same
    /// quarter close, same repair bounds, no partial-window settlement decision.
    ///
    /// Audit A1/F4: this is a **determinism** property, not a restart. The real
    /// restart — write a checkpoint, build a new pipeline from it, and check it
    /// resumes paused — is `settlement_window_pause_survives_a_real_restart` in
    /// `crates/btc-signals/tests/checkpoint.rs`, against the checkpoint
    /// representation this adapter now exposes through
    /// [`KalshiIndexAdapter::settlement_window_pause`].
    #[test]
    fn the_settlement_window_pause_is_deterministic_across_identical_replays() {
        fn drive() -> (Stream, Vec<SourceEvent>) {
            let mut stream = Stream::ready();
            let mut events = Vec::new();
            events.extend(stream.ok(&fixtures::value_tick(10, false)).events);
            events.extend(stream.ok(&gap_quarter(20)).events);
            // A further quarter print arrives while the repair is outstanding.
            events.extend(
                stream
                    .ok(&quarter_message(
                        21,
                        fixtures::NEXT_QUARTER_CLOSE_MS,
                        fixtures::NEXT_QUARTER_CLOSE_MS,
                        60,
                    ))
                    .events,
            );
            (stream, events)
        }

        let (before, before_events) = drive();
        assert!(before.adapter.settlement_window_paused());
        let (after, after_events) = drive();
        assert!(after.adapter.settlement_window_paused());
        let pause = after
            .adapter
            .settlement_window_pause()
            .expect("the pause is exposed for a consumer to checkpoint");
        assert_eq!(pause.index_id, "BRTI");
        assert_eq!(pause.quarter_close_time, fixtures::NEXT_QUARTER_CLOSE_MS);
        assert_eq!(pause.after_index_time, fixtures::INDEX_TIME_MS);
        assert_eq!(pause.through_index_time, fixtures::NEXT_QUARTER_TICK_MS);
        assert!(!pause.is_unrepairable());
        assert_eq!(
            before.adapter.settlement_backfill, after.adapter.settlement_backfill,
            "on the same quarter close, with the same repair bounds"
        );
        assert_eq!(before_events, after_events, "replay is deterministic");
        assert!(
            after_events
                .iter()
                .all(|event| event.source_id != SETTLEMENT_WINDOW_SOURCE_ID),
            "no partial-window settlement decision crosses the restart boundary"
        );
    }

    /// One `cfbenchmarks_value` message inside the NEXT quarter's final minute,
    /// beyond the frontier the documented tick establishes.
    fn gap_quarter(seq: u64) -> String {
        quarter_message(
            seq,
            fixtures::NEXT_QUARTER_TICK_MS,
            fixtures::NEXT_QUARTER_CLOSE_MS,
            55,
        )
    }

    /// Rewrite a value tick's index time (both the embedded frame and the
    /// trailing window's exclusive end, which must equal it).
    fn at(payload: &str, index_time: i64) -> String {
        payload
            .replace(
                r#"\"time\":1788000300000"#,
                &format!(r#"\"time\":{index_time}"#),
            )
            .replace(
                r#""window_end_ts_exclusive":1788000300000,"#,
                &format!(r#""window_end_ts_exclusive":{index_time},"#),
            )
            .replace(
                r#""window_start_ts_ms":1788000240000,"window_end_ts_exclusive":1788000300000}"#,
                &format!(
                    r#""window_start_ts_ms":{},"window_end_ts_exclusive":{index_time}}}"#,
                    index_time - 60_000
                ),
            )
    }

    /// One `cfbenchmarks_value` message carrying a quarter object with an
    /// explicit close and window size.
    fn quarter_message(seq: u64, index_time: i64, close: i64, window_size: u32) -> String {
        format!(
            r#"{{"type":"cfbenchmarks_value","sid":1,"seq":{seq},"msg":{{"index_id":"BRTI","received_at":1788000300123,"data":"{{\"type\":\"value\",\"id\":\"BRTI\",\"time\":{index_time},\"value\":\"64250.11\"}}","avg_60s_data":{{"value":"64249.10000000","window_size":60,"window_start_ts_ms":{},"window_end_ts_exclusive":{index_time}}},"last_60s_windowed_average_15min":{{"value":"64248.50000000","window_size":{window_size},"window_start_ts_ms":{},"window_end_ts_exclusive":{index_time}}}}}}}"#,
            index_time - 60_000,
            close - 60_000
        )
    }
}
