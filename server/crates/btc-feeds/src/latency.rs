//! Private-capture timing analysis over a raw capture journal.
//!
//! One read-only pass over a `raw` record stream produces two independent
//! measurements. The first is per-series source-to-receipt delay: the provider's
//! own event timestamp subtracted from the capture host's `received_at` wall
//! clock for the complete frame. Both ends are wall clocks on different
//! machines, so that figure contains provider publication, Internet transit,
//! TLS/WebSocket framing, host scheduling and clock offset -- it is a capture
//! timing summary, not a one-way network latency claim. The second is
//! per-connection startup: connect call to WebSocket ready, subscribe call to
//! ack, and subscribe call to first matching data frame, all differences on the
//! capture's monotonic elapsed clock.
//!
//! Every duration reported here is microseconds, truncated from nanoseconds
//! where the source is the monotonic clock. Message interarrival prefers the
//! monotonic clock and falls back to `received_at` when a record carries no
//! elapsed reading; a negative gap is discarded and counted as a local clock
//! regression rather than entering a distribution.
//!
//! The pass is bounded and fails closed: total samples, rows per message,
//! retained connection epochs, and subscriptions per epoch each abort with a
//! `LatencyError` at a fixed maximum, and any unusable timestamp aborts rather
//! than being silently dropped. Streams with no trustworthy provider send time
//! (SportsDataIO, NBA ESPN, the Binance partial-depth stream) are reported as
//! untimed messages instead of being timed against a substitute field.

use std::collections::{BTreeMap, BTreeSet};
use std::io::BufRead;

use chrono::{DateTime, SecondsFormat, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use thiserror::Error;

use crate::raw::{Provider, RawRecord, RawRecordError, RecordKind, read_record};

const MAX_SAMPLES: usize = 5_000_000;
const MAX_ROWS_PER_MESSAGE: usize = 100_000;
const MAX_STARTUP_CONNECTIONS: usize = 10_000;
const MAX_SUBSCRIPTIONS_PER_CONNECTION: usize = 64;

/// Private-capture timing summary. This is not a claim of one-way network latency.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LatencyReport {
    /// Fixed prose naming the two clocks that were differenced, so a reader of
    /// the serialized report cannot mistake it for one-way network latency.
    pub measurement: &'static str,
    /// Per-connection-epoch startup milestones on the monotonic capture clock.
    pub startup: StartupReport,
    /// Capture records read in the pass, every kind included, as a count.
    pub records: u64,
    /// Source-to-receipt samples contributed to the series distributions, as a
    /// count. One message can contribute several (one per row or asset id).
    pub timed_events: u64,
    /// Inbound text messages that yielded no sample, as a count: no source
    /// timestamp, an unhandled channel, or a deliberately untimed stream.
    pub untimed_messages: u64,
    /// Interarrival gaps that came out negative and were discarded rather than
    /// recorded, as a count. Nonzero means the receipt clock moved backwards.
    pub local_clock_regressions: u64,
    /// One entry per timing series, ordered by the series key.
    pub series: Vec<SeriesLatency>,
    /// Fixed caveats that must travel with the numbers: what the delay includes,
    /// the clock-synchronization precondition, and what it does not cover.
    pub limitations: [&'static str; 3],
}

/// Connection and subscription startup timing, measured entirely on the
/// capture's monotonic elapsed clock rather than against provider timestamps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StartupReport {
    /// Fixed prose naming the monotonic start point and the observed milestone.
    pub measurement: &'static str,
    /// One entry per connection epoch that carried both a connect-start and a
    /// ready reading, ordered by epoch. Bounded at `MAX_STARTUP_CONNECTIONS`.
    pub connections: Vec<ConnectionStartup>,
    /// Fixed caveat that first-data time includes provider publication and can
    /// absorb market inactivity when a channel has no immediate snapshot.
    pub limitation: &'static str,
}

/// Startup milestones for one connection attempt, keyed by its capture epoch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConnectionStartup {
    /// Capture-assigned epoch identifying this connection attempt; a reconnect
    /// gets a new epoch, so the same channel can appear more than once.
    pub connection_epoch: u64,
    /// Venue the connection was made to.
    pub provider: Provider,
    /// Capture stream the connection belongs to.
    pub stream: crate::raw::CaptureStream,
    /// Monotonic microseconds from the connect call to the socket being ready
    /// for use, covering DNS, TCP, TLS and the WebSocket upgrade.
    pub connect_call_to_websocket_ready_us: u64,
    /// One entry per timed subscribe request sent on this epoch, in send order.
    /// Bounded at `MAX_SUBSCRIPTIONS_PER_CONNECTION`.
    pub subscriptions: Vec<SubscriptionStartup>,
}

/// Startup milestones for one subscribe request on a connection epoch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SubscriptionStartup {
    /// Channel the request subscribed to, as the provider names it (Coinbase
    /// has no per-channel request, so it records the capture stream name).
    pub subscription: String,
    /// Provider request correlation id where the protocol has one (Kraken
    /// `req_id`, Kalshi `id`); absent for Coinbase and Polymarket.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<u64>,
    /// Monotonic microseconds from the subscribe call to its acknowledgement.
    /// `None` when no ack was observed before the capture ended.
    pub call_to_ack_us: Option<u64>,
    /// Whether the acknowledgement accepted the subscription; `false` for an
    /// error ack. `None` while unacknowledged.
    pub accepted: Option<bool>,
    /// Monotonic microseconds from the subscribe call to the first data frame
    /// matching this channel and symbol set. `None` when none arrived.
    pub call_to_first_data_us: Option<u64>,
}

/// Delay and cadence summaries for one timing series.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SeriesLatency {
    /// Series key, `provider:channel[:symbol]` as built by the per-venue sample
    /// extractors, e.g. `coinbase_exchange:match:BTC-USD`.
    pub series: String,
    /// Distribution of provider source timestamp to local complete-frame
    /// receipt, in wall-clock microseconds. Sensitive to host/provider skew.
    pub source_to_local_receipt: Distribution,
    /// Distribution of gaps between consecutive local receipts for this series,
    /// in monotonic microseconds. `None` until a series has a second message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_message_interarrival: Option<Distribution>,
}

/// Order statistics over one set of microsecond samples. Percentiles are
/// nearest-rank over the sorted samples, so each is an observed value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Distribution {
    /// Samples summarized, as a count. Never zero; an empty set is an error.
    pub samples: u64,
    /// Smallest sample, in microseconds. Negative when the source timestamp is
    /// ahead of local receipt.
    pub minimum_us: i64,
    /// Median sample, in microseconds.
    pub p50_us: i64,
    /// 95th-percentile sample, in microseconds.
    pub p95_us: i64,
    /// 99th-percentile sample, in microseconds.
    pub p99_us: i64,
    /// Largest sample, in microseconds.
    pub maximum_us: i64,
    /// Samples strictly below zero, as a count. On a source-to-receipt series
    /// this is the clock-skew tell; interarrival series never record one.
    pub negative_samples: u64,
}

#[derive(Default)]
struct SeriesSamples {
    source_to_receipt_us: Vec<i64>,
    interarrival_us: Vec<i64>,
    last_receipt_us: Option<i64>,
}

#[derive(Default)]
struct StartupAnalyzer {
    connections: BTreeMap<u64, ConnectionStartupState>,
}

struct ConnectionStartupState {
    provider: Provider,
    stream: crate::raw::CaptureStream,
    connect_call_to_websocket_ready_us: u64,
    subscriptions: Vec<SubscriptionStartupState>,
}

struct SubscriptionStartupState {
    subscription: String,
    request_id: Option<u64>,
    symbols: BTreeSet<String>,
    sent_elapsed_ns: u64,
    call_to_ack_us: Option<u64>,
    accepted: Option<bool>,
    call_to_first_data_us: Option<u64>,
}

/// Summarize source timestamps against local frame receipt timestamps.
pub fn analyze<R: BufRead>(reader: &mut R) -> Result<LatencyReport, LatencyError> {
    let mut records = 0_u64;
    let mut timed_events = 0_u64;
    let mut untimed_messages = 0_u64;
    let mut local_clock_regressions = 0_u64;
    let mut total_samples = 0_usize;
    let mut by_series = BTreeMap::<String, SeriesSamples>::new();
    let mut startup = StartupAnalyzer::default();
    // Chainlink poll responses repeat the same round until the on-chain feed
    // updates; this cursor times only new-round responses so duplicate polls do
    // not inflate the source-to-receipt series (see `chainlink_samples`).
    let mut chainlink_cursor: Option<u128> = None;

    while let Some(record) = read_record(reader)? {
        records = records
            .checked_add(1)
            .ok_or(LatencyError::CounterExhausted)?;
        startup.observe(&record)?;
        // A fresh connection epoch restarts the round cursor, so a round re-seen
        // after a reconnect is timed again -- matching the adapter, which clears
        // its cursor on Connected and re-emits the same round in a new epoch.
        if record.kind == RecordKind::Connected {
            chainlink_cursor = None;
        }
        if record.kind != RecordKind::InboundText {
            continue;
        }
        let payload = record.payload.as_deref().ok_or_else(|| {
            LatencyError::InvalidPayload("inbound record is missing payload".to_owned())
        })?;
        let receipt_us = timestamp_us(&record.received_at, "received_at")?;
        let monotonic_receipt_us = record
            .capture_elapsed_ns
            .map(|elapsed_ns| i64::try_from(elapsed_ns / 1_000))
            .transpose()
            .map_err(|_| LatencyError::TimestampOverflow)?
            .unwrap_or(receipt_us);
        let samples = if record.provider == Provider::ChainlinkAggregator {
            chainlink_samples(&mut chainlink_cursor, payload)
        } else {
            extract_samples(&record, payload)?
        };
        if samples.is_empty() {
            untimed_messages = untimed_messages
                .checked_add(1)
                .ok_or(LatencyError::CounterExhausted)?;
            continue;
        }

        let mut message_series = BTreeSet::new();
        for sample in samples {
            total_samples = total_samples
                .checked_add(1)
                .ok_or(LatencyError::CounterExhausted)?;
            if total_samples > MAX_SAMPLES {
                return Err(LatencyError::SampleLimit {
                    maximum: MAX_SAMPLES,
                });
            }
            let source_us = timestamp_us(&sample.source_time, "source timestamp")?;
            let delay_us = receipt_us
                .checked_sub(source_us)
                .ok_or(LatencyError::TimestampOverflow)?;
            by_series
                .entry(sample.series.clone())
                .or_default()
                .source_to_receipt_us
                .push(delay_us);
            message_series.insert(sample.series);
            timed_events = timed_events
                .checked_add(1)
                .ok_or(LatencyError::CounterExhausted)?;
        }

        for series in message_series {
            let state = by_series
                .get_mut(&series)
                .ok_or_else(|| LatencyError::InvalidPayload("missing timing series".to_owned()))?;
            if let Some(previous) = state.last_receipt_us {
                let interarrival = monotonic_receipt_us
                    .checked_sub(previous)
                    .ok_or(LatencyError::TimestampOverflow)?;
                if interarrival < 0 {
                    local_clock_regressions = local_clock_regressions
                        .checked_add(1)
                        .ok_or(LatencyError::CounterExhausted)?;
                } else {
                    state.interarrival_us.push(interarrival);
                }
            }
            state.last_receipt_us = Some(monotonic_receipt_us);
        }
    }

    let series = by_series
        .into_iter()
        .map(|(series, mut samples)| {
            let source_to_local_receipt = distribution(&mut samples.source_to_receipt_us)?;
            let local_message_interarrival = if samples.interarrival_us.is_empty() {
                None
            } else {
                Some(distribution(&mut samples.interarrival_us)?)
            };
            Ok(SeriesLatency {
                series,
                source_to_local_receipt,
                local_message_interarrival,
            })
        })
        .collect::<Result<Vec<_>, LatencyError>>()?;

    Ok(LatencyReport {
        measurement: "source timestamp to local complete-frame receipt",
        startup: startup.finish(),
        records,
        timed_events,
        untimed_messages,
        local_clock_regressions,
        series,
        limitations: [
            "includes provider publication, Internet transit, TLS/WebSocket framing, scheduling, and clock offset",
            "requires synchronized host and provider clocks before it can approximate one-way latency",
            "does not measure downstream normalization, signal calculation, or publication latency",
        ],
    })
}

impl StartupAnalyzer {
    fn observe(&mut self, record: &RawRecord) -> Result<(), LatencyError> {
        match record.kind {
            RecordKind::Configuration => Ok(()),
            RecordKind::Connected => self.connected(record),
            RecordKind::OutboundText => self.outbound(record),
            RecordKind::InboundText => self.inbound(record),
            RecordKind::Disconnected => Ok(()),
            // A refused request has no venue message to time.
            RecordKind::Failed => Ok(()),
        }
    }

    fn connected(&mut self, record: &RawRecord) -> Result<(), LatencyError> {
        let (Some(ready_ns), Some(started_ns)) =
            (record.capture_elapsed_ns, record.connect_started_elapsed_ns)
        else {
            return Ok(());
        };
        if self.connections.len() >= MAX_STARTUP_CONNECTIONS {
            return Err(LatencyError::StartupConnectionLimit {
                maximum: MAX_STARTUP_CONNECTIONS,
            });
        }
        let setup_us = elapsed_us(started_ns, ready_ns)?;
        if self
            .connections
            .insert(
                record.connection_epoch,
                ConnectionStartupState {
                    provider: record.provider,
                    stream: record.stream,
                    connect_call_to_websocket_ready_us: setup_us,
                    subscriptions: Vec::new(),
                },
            )
            .is_some()
        {
            return Err(LatencyError::InvalidPayload(
                "duplicate timed connection epoch".to_owned(),
            ));
        }
        Ok(())
    }

    fn outbound(&mut self, record: &RawRecord) -> Result<(), LatencyError> {
        let Some(sent_elapsed_ns) = record.capture_elapsed_ns else {
            return Ok(());
        };
        let payload = record.payload.as_deref().ok_or_else(|| {
            LatencyError::InvalidPayload("outbound record is missing payload".to_owned())
        })?;
        let Some(subscription) = parse_subscription(record, payload, sent_elapsed_ns)? else {
            return Ok(());
        };
        let connection = self
            .connections
            .get_mut(&record.connection_epoch)
            .ok_or_else(|| {
                LatencyError::InvalidPayload(
                    "timed subscription precedes its connected record".to_owned(),
                )
            })?;
        if connection.subscriptions.len() >= MAX_SUBSCRIPTIONS_PER_CONNECTION {
            return Err(LatencyError::SubscriptionLimit {
                maximum: MAX_SUBSCRIPTIONS_PER_CONNECTION,
            });
        }
        connection.subscriptions.push(subscription);
        Ok(())
    }

    fn inbound(&mut self, record: &RawRecord) -> Result<(), LatencyError> {
        let Some(received_elapsed_ns) = record.capture_elapsed_ns else {
            return Ok(());
        };
        let payload = record.payload.as_deref().ok_or_else(|| {
            LatencyError::InvalidPayload("inbound record is missing payload".to_owned())
        })?;
        let Some(connection) = self.connections.get_mut(&record.connection_epoch) else {
            return Ok(());
        };
        match record.provider {
            Provider::CoinbaseExchange => {
                let envelope: Envelope<'_> = serde_json::from_str(payload)
                    .map_err(|error| LatencyError::Json(error.to_string()))?;
                observe_coinbase_startup(connection, envelope, received_elapsed_ns)
            }
            Provider::KrakenSpot => {
                let envelope: Envelope<'_> = serde_json::from_str(payload)
                    .map_err(|error| LatencyError::Json(error.to_string()))?;
                observe_kraken_startup(connection, envelope, received_elapsed_ns)
            }
            Provider::Polymarket => {
                observe_polymarket_startup(connection, payload, received_elapsed_ns)
            }
            Provider::Kalshi => observe_kalshi_startup(connection, payload, received_elapsed_ns),
            // Binance's combined subscription is acknowledged with a plain
            // `{"result":null,"id":1}` frame that carries no channel or symbol,
            // so there is no per-subscription startup timing to observe.
            // Binance USD-M acknowledges each connection's SUBSCRIBE the same
            // way, and its REST pollers have no handshake at all.
            Provider::BinanceSpot | Provider::BinanceUsdm => Ok(()),
            // Bybit acknowledges one combined subscription request with an
            // `op`-response frame that carries no channel or symbol, so there is
            // no per-subscription startup timing to observe.
            Provider::BybitSpot | Provider::BybitLinear => Ok(()),
            // Chainlink is a poll feed with no subscription handshake, so there
            // is no per-subscription startup timing to observe.
            Provider::ChainlinkAggregator => Ok(()),
            // NBA ESPN is a multi-request poll feed (slice 1), a one-pass entity
            // crawl (slice 2), or a repeating discovered game-day cycle (slice
            // 3), none with a subscription handshake, so there is no
            // per-subscription startup timing.
            Provider::NbaEspnStatus | Provider::NbaEspnDaily | Provider::NbaEspnGames => Ok(()),
            // SportsDataIO is an authenticated REST matrix, not a WebSocket: there
            // is no connection handshake and no subscription to time.
            Provider::SportsDataIo => Ok(()),
            // Pyth Hermes SSE carries no subscription handshake (the query string
            // is the subscription) and the REST anchor has none either.
            Provider::PythHermes => Ok(()),
        }
    }

    fn finish(self) -> StartupReport {
        let connections = self
            .connections
            .into_iter()
            .map(|(connection_epoch, state)| ConnectionStartup {
                connection_epoch,
                provider: state.provider,
                stream: state.stream,
                connect_call_to_websocket_ready_us: state.connect_call_to_websocket_ready_us,
                subscriptions: state
                    .subscriptions
                    .into_iter()
                    .map(|subscription| SubscriptionStartup {
                        subscription: subscription.subscription,
                        request_id: subscription.request_id,
                        call_to_ack_us: subscription.call_to_ack_us,
                        accepted: subscription.accepted,
                        call_to_first_data_us: subscription.call_to_first_data_us,
                    })
                    .collect(),
            })
            .collect();
        StartupReport {
            measurement: "local monotonic call start to client-observed milestone",
            connections,
            limitation: "first-data time includes provider response/publication and can include market inactivity when a channel has no immediate snapshot",
        }
    }
}

fn elapsed_us(started_ns: u64, completed_ns: u64) -> Result<u64, LatencyError> {
    completed_ns
        .checked_sub(started_ns)
        .map(|elapsed| elapsed / 1_000)
        .ok_or(LatencyError::TimestampOverflow)
}

struct TimingSample {
    series: String,
    source_time: String,
}

#[derive(Deserialize)]
struct Envelope<'a> {
    #[serde(rename = "type", borrow)]
    message_type: Option<&'a str>,
    #[serde(borrow)]
    method: Option<&'a str>,
    req_id: Option<u64>,
    success: Option<bool>,
    #[serde(borrow)]
    channel: Option<&'a str>,
    #[serde(borrow)]
    product_id: Option<&'a str>,
    #[serde(borrow)]
    time: Option<&'a str>,
    #[serde(borrow)]
    timestamp: Option<&'a str>,
    #[serde(borrow)]
    data: Option<&'a RawValue>,
}

#[derive(Deserialize)]
struct KrakenTimingRow<'a> {
    #[serde(borrow)]
    symbol: Option<&'a str>,
    #[serde(borrow)]
    timestamp: Option<&'a str>,
}

#[derive(Deserialize)]
struct OutboundEnvelope<'a> {
    #[serde(rename = "type", borrow)]
    message_type: Option<&'a str>,
    #[serde(borrow)]
    method: Option<&'a str>,
    req_id: Option<u64>,
    #[serde(borrow)]
    params: Option<OutboundParams<'a>>,
}

#[derive(Deserialize)]
struct OutboundParams<'a> {
    #[serde(borrow)]
    channel: &'a str,
    #[serde(default, borrow)]
    symbol: Vec<&'a str>,
}

fn parse_subscription(
    record: &RawRecord,
    payload: &str,
    sent_elapsed_ns: u64,
) -> Result<Option<SubscriptionStartupState>, LatencyError> {
    if record.provider == Provider::Polymarket {
        if payload == "PING" {
            return Ok(None);
        }
        let outbound: PolymarketSubscription =
            serde_json::from_str(payload).map_err(|error| LatencyError::Json(error.to_string()))?;
        if outbound.message_type != "market" {
            return Ok(None);
        }
        if outbound.asset_ids.is_empty()
            || outbound.asset_ids.len() > 32
            || outbound
                .asset_ids
                .iter()
                .any(|asset_id| asset_id.is_empty() || asset_id.len() > 128)
        {
            return Err(LatencyError::InvalidPayload(
                "Polymarket subscription asset ids exceed timing-analysis bounds".to_owned(),
            ));
        }
        return Ok(Some(SubscriptionStartupState {
            subscription: "market".to_owned(),
            request_id: None,
            symbols: outbound.asset_ids.into_iter().collect(),
            sent_elapsed_ns,
            call_to_ack_us: None,
            accepted: None,
            call_to_first_data_us: None,
        }));
    }
    if record.provider == Provider::ChainlinkAggregator {
        // A poll request (`eth_call`) is not a channel subscription, and its
        // `params` is a JSON array rather than the subscription-params object the
        // shared outbound envelope expects; there is nothing to time.
        return Ok(None);
    }
    if record.provider == Provider::SportsDataIo {
        // The outbound record is a sanitized request DESCRIPTOR
        // ({"request","operation","cycle","index"}), not a channel subscription;
        // there is nothing to time.
        return Ok(None);
    }
    if matches!(
        record.provider,
        Provider::NbaEspnStatus | Provider::NbaEspnDaily | Provider::NbaEspnGames
    ) {
        // The outbound record is a request DESCRIPTOR (slice 1
        // {"request","cycle","index"}, slice 2 additionally {"phase","family",
        // "entity_id"}, slice 3 {"request","cycle","index","entity_id"}), not a
        // channel subscription; there is nothing to time.
        return Ok(None);
    }
    if record.provider == Provider::Kalshi {
        let outbound: KalshiSubscription<'_> =
            serde_json::from_str(payload).map_err(|error| LatencyError::Json(error.to_string()))?;
        if outbound.cmd != "subscribe" || outbound.id == 0 {
            return Ok(None);
        }
        if outbound.params.channels.len() != 1 {
            return Err(LatencyError::InvalidPayload(
                "Kalshi timing requires one channel per bounded subscription".to_owned(),
            ));
        }
        let channel = outbound.params.channels[0];
        if !matches!(channel, "orderbook_delta" | "ticker" | "trade") {
            return Err(LatencyError::InvalidPayload(
                "Kalshi timing received an unsupported subscription channel".to_owned(),
            ));
        }
        let symbols = outbound.params.market_tickers();
        if symbols.len() != 1 || symbols[0].is_empty() || symbols[0].len() > 128 {
            return Err(LatencyError::InvalidPayload(
                "Kalshi subscription ticker exceeds timing-analysis bounds".to_owned(),
            ));
        }
        return Ok(Some(SubscriptionStartupState {
            subscription: channel.to_owned(),
            request_id: Some(outbound.id),
            symbols: symbols.into_iter().map(str::to_owned).collect(),
            sent_elapsed_ns,
            call_to_ack_us: None,
            accepted: None,
            call_to_first_data_us: None,
        }));
    }
    let outbound: OutboundEnvelope<'_> =
        serde_json::from_str(payload).map_err(|error| LatencyError::Json(error.to_string()))?;
    match record.provider {
        Provider::CoinbaseExchange if outbound.message_type == Some("subscribe") => {
            Ok(Some(SubscriptionStartupState {
                subscription: record.stream.as_str().to_owned(),
                request_id: None,
                symbols: BTreeSet::new(),
                sent_elapsed_ns,
                call_to_ack_us: None,
                accepted: None,
                call_to_first_data_us: None,
            }))
        }
        Provider::KrakenSpot if outbound.method == Some("subscribe") => {
            let params = outbound.params.ok_or_else(|| {
                LatencyError::InvalidPayload(
                    "Kraken subscribe request is missing params".to_owned(),
                )
            })?;
            if params.channel.is_empty() || params.channel.len() > 64 {
                return Err(LatencyError::InvalidPayload(
                    "Kraken subscription channel must contain 1 to 64 bytes".to_owned(),
                ));
            }
            if params.symbol.len() > 32
                || params
                    .symbol
                    .iter()
                    .any(|symbol| symbol.is_empty() || symbol.len() > 32)
            {
                return Err(LatencyError::InvalidPayload(
                    "Kraken subscription symbols exceed timing-analysis bounds".to_owned(),
                ));
            }
            Ok(Some(SubscriptionStartupState {
                subscription: params.channel.to_owned(),
                request_id: outbound.req_id,
                symbols: params.symbol.into_iter().map(str::to_owned).collect(),
                sent_elapsed_ns,
                call_to_ack_us: None,
                accepted: None,
                call_to_first_data_us: None,
            }))
        }
        _ => Ok(None),
    }
}

fn observe_kalshi_startup(
    connection: &mut ConnectionStartupState,
    payload: &str,
    received_elapsed_ns: u64,
) -> Result<(), LatencyError> {
    let envelope: KalshiEnvelope<'_> =
        serde_json::from_str(payload).map_err(|error| LatencyError::Json(error.to_string()))?;
    if matches!(envelope.message_type, "subscribed" | "ok" | "error") {
        if let Some(request_id) = envelope.id
            && let Some(subscription) = connection
                .subscriptions
                .iter_mut()
                .find(|subscription| subscription.request_id == Some(request_id))
            && subscription.call_to_ack_us.is_none()
        {
            subscription.call_to_ack_us = Some(elapsed_us(
                subscription.sent_elapsed_ns,
                received_elapsed_ns,
            )?);
            subscription.accepted = Some(envelope.message_type != "error");
        }
        return Ok(());
    }
    let channel = match envelope.message_type {
        "orderbook_snapshot" | "orderbook_delta" => "orderbook_delta",
        "ticker" => "ticker",
        "trade" => "trade",
        _ => return Ok(()),
    };
    let message: serde_json::Value = serde_json::from_str(envelope.msg.get())
        .map_err(|error| LatencyError::Json(error.to_string()))?;
    let ticker = message
        .get("market_ticker")
        .and_then(serde_json::Value::as_str);
    for subscription in &mut connection.subscriptions {
        if subscription.subscription == channel
            && subscription.call_to_first_data_us.is_none()
            && ticker.is_some_and(|ticker| subscription.symbols.contains(ticker))
        {
            subscription.call_to_first_data_us = Some(elapsed_us(
                subscription.sent_elapsed_ns,
                received_elapsed_ns,
            )?);
        }
    }
    Ok(())
}

fn observe_polymarket_startup(
    connection: &mut ConnectionStartupState,
    payload: &str,
    received_elapsed_ns: u64,
) -> Result<(), LatencyError> {
    if payload == "PONG" {
        return Ok(());
    }
    let rows = polymarket_rows(payload)?;
    let assets = rows
        .iter()
        .flat_map(PolymarketTimingRow::asset_ids)
        .collect::<BTreeSet<_>>();
    for subscription in &mut connection.subscriptions {
        if subscription.subscription == "market"
            && subscription.call_to_first_data_us.is_none()
            && subscription
                .symbols
                .iter()
                .any(|asset_id| assets.contains(asset_id))
        {
            subscription.call_to_first_data_us = Some(elapsed_us(
                subscription.sent_elapsed_ns,
                received_elapsed_ns,
            )?);
        }
    }
    Ok(())
}

fn observe_coinbase_startup(
    connection: &mut ConnectionStartupState,
    envelope: Envelope<'_>,
    received_elapsed_ns: u64,
) -> Result<(), LatencyError> {
    let Some(message_type) = envelope.message_type else {
        return Ok(());
    };
    if matches!(message_type, "subscriptions" | "error") {
        for subscription in &mut connection.subscriptions {
            if subscription.call_to_ack_us.is_none() {
                subscription.call_to_ack_us = Some(elapsed_us(
                    subscription.sent_elapsed_ns,
                    received_elapsed_ns,
                )?);
                subscription.accepted = Some(message_type == "subscriptions");
            }
        }
        return Ok(());
    }
    if matches!(message_type, "heartbeat" | "status") {
        return Ok(());
    }
    for subscription in &mut connection.subscriptions {
        if subscription.call_to_first_data_us.is_none() {
            subscription.call_to_first_data_us = Some(elapsed_us(
                subscription.sent_elapsed_ns,
                received_elapsed_ns,
            )?);
        }
    }
    Ok(())
}

fn observe_kraken_startup(
    connection: &mut ConnectionStartupState,
    envelope: Envelope<'_>,
    received_elapsed_ns: u64,
) -> Result<(), LatencyError> {
    if envelope.method == Some("subscribe") {
        if let Some(request_id) = envelope.req_id
            && let Some(subscription) = connection
                .subscriptions
                .iter_mut()
                .find(|subscription| subscription.request_id == Some(request_id))
            && subscription.call_to_ack_us.is_none()
        {
            subscription.call_to_ack_us = Some(elapsed_us(
                subscription.sent_elapsed_ns,
                received_elapsed_ns,
            )?);
            subscription.accepted = envelope.success;
        }
        return Ok(());
    }

    let Some(channel) = envelope.channel else {
        return Ok(());
    };
    if !connection.subscriptions.iter().any(|subscription| {
        subscription.subscription == channel && subscription.call_to_first_data_us.is_none()
    }) {
        return Ok(());
    }

    let needs_symbols = connection.subscriptions.iter().any(|subscription| {
        subscription.subscription == channel
            && subscription.call_to_first_data_us.is_none()
            && !subscription.symbols.is_empty()
    });
    let message_symbols = if needs_symbols {
        let Some(data) = envelope.data else {
            return Ok(());
        };
        let rows: Vec<KrakenTimingRow<'_>> = serde_json::from_str(data.get())
            .map_err(|error| LatencyError::Json(error.to_string()))?;
        if rows.len() > MAX_ROWS_PER_MESSAGE {
            return Err(LatencyError::RowLimit {
                actual: rows.len(),
                maximum: MAX_ROWS_PER_MESSAGE,
            });
        }
        rows.into_iter()
            .filter_map(|row| row.symbol.map(str::to_owned))
            .collect::<BTreeSet<_>>()
    } else {
        BTreeSet::new()
    };

    for subscription in &mut connection.subscriptions {
        if subscription.subscription == channel
            && subscription.call_to_first_data_us.is_none()
            && (subscription.symbols.is_empty()
                || subscription
                    .symbols
                    .iter()
                    .any(|symbol| message_symbols.contains(symbol)))
        {
            subscription.call_to_first_data_us = Some(elapsed_us(
                subscription.sent_elapsed_ns,
                received_elapsed_ns,
            )?);
        }
    }
    Ok(())
}

fn extract_samples(record: &RawRecord, payload: &str) -> Result<Vec<TimingSample>, LatencyError> {
    if record.provider == Provider::Polymarket {
        return polymarket_samples(payload);
    }
    if record.provider == Provider::Kalshi {
        return kalshi_samples(payload);
    }
    if record.provider == Provider::BinanceSpot {
        return binance_samples(payload);
    }
    if record.provider == Provider::BinanceUsdm {
        return binance_usdm_samples(payload);
    }
    if matches!(record.provider, Provider::BybitSpot | Provider::BybitLinear) {
        return bybit_samples(record.provider, payload);
    }
    if record.provider == Provider::SportsDataIo {
        // This capture-only slice records a RESPONSE ENVELOPE and keeps the
        // provider payload raw: it extracts no provider field, so there is no
        // provider event/update time to convert into a transport-latency sample.
        // SportsDataIO's per-record `Updated`/`LastUpdated` fields are the natural
        // timestamp source, but reading them means parsing the payload, which is
        // gate SDIO-2 work and deliberately not in this slice. Honestly untimed.
        return Ok(Vec::new());
    }
    if matches!(
        record.provider,
        Provider::NbaEspnStatus | Provider::NbaEspnDaily | Provider::NbaEspnGames
    ) {
        // ESPN aggregator payloads carry no reliable per-message wire-send time
        // (the injuries top-level `timestamp` is a whole-payload feed-generation
        // time, identical across a poll; the scoreboard has none), so this stream
        // is honestly untimed, like the Binance partial-depth stream.
        //
        // The slice-3 game-day shapes are untimed for the same reason, checked
        // shape by shape against the surface research: the odds item publishes NO
        // per-snapshot timestamp at all (which is precisely why our poll sequence
        // has to BE the movement series), and the summary carries none either.
        // The predictor's `lastModified` is the BPI model's publication time for
        // the state, not a wire-send time for this message, so the adapter uses it
        // as the event_time on basis `source` and it is deliberately NOT converted
        // into a source-to-receipt transport latency sample here: the gap between
        // a model run and our poll is cadence, not transport delay, and reporting
        // it as latency would misdescribe it.
        return Ok(Vec::new());
    }
    if record.provider == Provider::PythHermes {
        // The SSE payload is an SSE event block, not the shared JSON envelope,
        // and its publish_time is a source aggregate time, not a wire-send time;
        // this stream is honestly untimed here, like the ESPN aggregator streams.
        return Ok(Vec::new());
    }
    let envelope: Envelope<'_> =
        serde_json::from_str(payload).map_err(|error| LatencyError::Json(error.to_string()))?;
    match record.provider {
        Provider::CoinbaseExchange => coinbase_samples(envelope),
        Provider::KrakenSpot => kraken_samples(envelope),
        Provider::BinanceSpot | Provider::BinanceUsdm => {
            unreachable!("Binance and Binance USD-M return before shared envelope parsing")
        }
        Provider::BybitSpot | Provider::BybitLinear => {
            unreachable!("Bybit returned before shared envelope parsing")
        }
        Provider::ChainlinkAggregator => {
            unreachable!(
                "Chainlink is timed with round-cursor state before shared envelope parsing"
            )
        }
        Provider::NbaEspnStatus | Provider::NbaEspnDaily | Provider::NbaEspnGames => {
            unreachable!("NBA ESPN is untimed and returned before shared envelope parsing")
        }
        Provider::SportsDataIo => {
            unreachable!("SportsDataIO is untimed and returned before shared envelope parsing")
        }
        Provider::Polymarket => unreachable!("Polymarket returned before envelope parsing"),
        Provider::Kalshi => unreachable!("Kalshi returned before shared envelope parsing"),
        Provider::PythHermes => {
            unreachable!("Pyth Hermes is untimed and returned before shared envelope parsing")
        }
    }
}

/// Binance combined-stream timing: the aggregate-trade stream carries an event
/// time (`E`, Unix milliseconds), so it yields a source-to-receipt sample. The
/// partial-depth stream has no source timestamp and is left untimed.
fn binance_samples(payload: &str) -> Result<Vec<TimingSample>, LatencyError> {
    #[derive(serde::Deserialize)]
    struct Wrapper<'a> {
        #[serde(borrow)]
        stream: Option<&'a str>,
        #[serde(borrow)]
        data: Option<&'a RawValue>,
    }
    #[derive(serde::Deserialize)]
    struct AggEventTime {
        #[serde(rename = "E")]
        event_time: i64,
    }

    let wrapper: Wrapper<'_> =
        serde_json::from_str(payload).map_err(|error| LatencyError::Json(error.to_string()))?;
    let (Some(stream), Some(data)) = (wrapper.stream, wrapper.data) else {
        return Ok(Vec::new());
    };
    if stream != crate::binance::AGG_TRADE_STREAM {
        return Ok(Vec::new());
    }
    let agg: AggEventTime =
        serde_json::from_str(data.get()).map_err(|error| LatencyError::Json(error.to_string()))?;
    // Same plausibility window the adapter enforces: a zero, negative, or
    // microsecond-scale E would otherwise time-travel the latency series
    // while replay of the same record fails closed.
    if !(crate::event_window::MIN_EVENT_MILLIS..=crate::event_window::MAX_EVENT_MILLIS)
        .contains(&agg.event_time)
    {
        return Err(LatencyError::InvalidPayload(format!(
            "Binance aggregate-trade E {} is outside the plausible millisecond window {}..={}",
            agg.event_time,
            crate::event_window::MIN_EVENT_MILLIS,
            crate::event_window::MAX_EVENT_MILLIS
        )));
    }
    let source_time = Utc
        .timestamp_millis_opt(agg.event_time)
        .single()
        .ok_or_else(|| {
            LatencyError::InvalidPayload(
                "Binance aggregate-trade E is outside UTC range".to_owned(),
            )
        })?
        .to_rfc3339_opts(SecondsFormat::Millis, true);
    Ok(vec![TimingSample {
        series: format!("binance_spot:{stream}"),
        source_time,
    }])
}

/// Binance USD-M source-to-receipt timing. After ruling E62 removed the
/// `market`-connection socket (mark price, aggregate trades, forced orders), no
/// remaining USD-M frame carries a per-message wire-send time to trust:
/// `depth20@100ms` on the `public` socket is a partial-book snapshot, and the
/// bounded REST bodies (open-interest, funding-info, premium-index) are response
/// objects or JSON arrays. Every USD-M frame is therefore honestly untimed
/// rather than surfaced as an error, so no sample is ever produced.
fn binance_usdm_samples(_payload: &str) -> Result<Vec<TimingSample>, LatencyError> {
    Ok(Vec::new())
}

/// Bybit source-to-receipt timing. `publicTrade` and `allLiquidation` entries
/// each carry an event time `T`; orderbook pushes carry a matching-engine cross
/// timestamp `cts` (used when present). Operation-response acks carry no source
/// time and are left untimed.
fn bybit_samples(provider: Provider, payload: &str) -> Result<Vec<TimingSample>, LatencyError> {
    #[derive(Deserialize)]
    struct Wrapper<'a> {
        #[serde(default, borrow)]
        topic: Option<&'a str>,
        #[serde(default, borrow)]
        op: Option<&'a str>,
        #[serde(default)]
        cts: Option<i64>,
        #[serde(default, borrow)]
        data: Option<&'a RawValue>,
    }
    #[derive(Deserialize)]
    struct EntryTime {
        #[serde(rename = "T")]
        event_time: i64,
    }

    let wrapper: Wrapper<'_> =
        serde_json::from_str(payload).map_err(|error| LatencyError::Json(error.to_string()))?;
    if wrapper.op.is_some() {
        return Ok(Vec::new());
    }
    let (Some(topic), Some(data)) = (wrapper.topic, wrapper.data) else {
        return Ok(Vec::new());
    };
    let prefix = provider.as_str();
    match topic {
        crate::bybit::PUBLIC_TRADE_TOPIC | crate::bybit::ALL_LIQUIDATION_TOPIC => {
            let entries: Vec<EntryTime> = serde_json::from_str(data.get())
                .map_err(|error| LatencyError::Json(error.to_string()))?;
            if entries.len() > MAX_ROWS_PER_MESSAGE {
                return Err(LatencyError::RowLimit {
                    actual: entries.len(),
                    maximum: MAX_ROWS_PER_MESSAGE,
                });
            }
            entries
                .into_iter()
                .map(|entry| {
                    Ok(TimingSample {
                        series: format!("{prefix}:{topic}"),
                        source_time: bybit_source_time(entry.event_time, "T")?,
                    })
                })
                .collect()
        }
        crate::bybit::ORDERBOOK_200_TOPIC | crate::bybit::ORDERBOOK_1_TOPIC => {
            // The book push is timed by its cross timestamp when present; the
            // envelope `ts` fallback is left untimed here.
            let Some(cts) = wrapper.cts else {
                return Ok(Vec::new());
            };
            Ok(vec![TimingSample {
                series: format!("{prefix}:{topic}"),
                source_time: bybit_source_time(cts, "cts")?,
            }])
        }
        _ => Ok(Vec::new()),
    }
}

fn bybit_source_time(millis: i64, field: &str) -> Result<String, LatencyError> {
    // Same plausibility window the adapter enforces, so a microsecond-scale
    // value that replay fails closed on never time-travels the latency series.
    if !(crate::event_window::MIN_EVENT_MILLIS..=crate::event_window::MAX_EVENT_MILLIS)
        .contains(&millis)
    {
        return Err(LatencyError::InvalidPayload(format!(
            "Bybit {field} {millis} is outside the plausible millisecond window {}..={}",
            crate::event_window::MIN_EVENT_MILLIS,
            crate::event_window::MAX_EVENT_MILLIS
        )));
    }
    Utc.timestamp_millis_opt(millis)
        .single()
        .ok_or_else(|| LatencyError::InvalidPayload(format!("Bybit {field} is outside UTC range")))
        .map(|value| value.to_rfc3339_opts(SecondsFormat::Millis, true))
}

/// Chainlink source-to-receipt timing. A round event is timed by its `updatedAt`
/// (decoded and window-validated through the shared adapter authority). Only a
/// NEW round is a fresh on-chain observation: a duplicate or regressed `roundId`
/// (the steady state when polling faster than the feed updates) is an untimed
/// message, and a JSON-RPC error response carries no source time. The `cursor`
/// tracks the last timed round across the pass.
fn chainlink_samples(cursor: &mut Option<u128>, payload: &str) -> Vec<TimingSample> {
    // A poll response that fails to parse/validate, or a JSON-RPC error, carries
    // no source time: it is an untimed message, mirroring how the adapter skips
    // it (transient, never fatal). Only a NEW round is a fresh on-chain
    // observation; a duplicate or regressed roundId is untimed.
    match crate::chainlink::parse_round_response(payload) {
        Ok(crate::chainlink::RoundResponse::Round(round)) => {
            let advance = match *cursor {
                Some(previous) => round.round_id > previous,
                None => true,
            };
            if !advance {
                return Vec::new();
            }
            *cursor = Some(round.round_id);
            vec![TimingSample {
                series: crate::chainlink::ROUNDS_SERIES.to_owned(),
                source_time: round.updated_at,
            }]
        }
        Ok(crate::chainlink::RoundResponse::ProviderError(_)) | Err(_) => Vec::new(),
    }
}

#[derive(Deserialize)]
struct KalshiSubscription<'a> {
    id: u64,
    #[serde(borrow)]
    cmd: &'a str,
    #[serde(borrow)]
    params: KalshiSubscriptionParams<'a>,
}

#[derive(Deserialize)]
struct KalshiSubscriptionParams<'a> {
    #[serde(borrow)]
    channels: Vec<&'a str>,
    #[serde(default, borrow)]
    market_ticker: Option<&'a str>,
    #[serde(default, borrow)]
    market_tickers: Vec<&'a str>,
}

impl<'a> KalshiSubscriptionParams<'a> {
    fn market_tickers(&self) -> Vec<&'a str> {
        self.market_ticker
            .iter()
            .copied()
            .chain(self.market_tickers.iter().copied())
            .collect()
    }
}

#[derive(Deserialize)]
struct KalshiEnvelope<'a> {
    #[serde(rename = "type", borrow)]
    message_type: &'a str,
    id: Option<u64>,
    #[serde(borrow)]
    msg: &'a RawValue,
}

fn kalshi_samples(payload: &str) -> Result<Vec<TimingSample>, LatencyError> {
    let envelope: KalshiEnvelope<'_> =
        serde_json::from_str(payload).map_err(|error| LatencyError::Json(error.to_string()))?;
    let channel = match envelope.message_type {
        "orderbook_delta" | "ticker" | "trade" => envelope.message_type,
        _ => return Ok(Vec::new()),
    };
    let message: serde_json::Value = serde_json::from_str(envelope.msg.get())
        .map_err(|error| LatencyError::Json(error.to_string()))?;
    let Some(ticker) = message
        .get("market_ticker")
        .and_then(serde_json::Value::as_str)
    else {
        return Ok(Vec::new());
    };
    if ticker.is_empty() || ticker.len() > 128 {
        return Err(LatencyError::InvalidPayload(
            "Kalshi timing market ticker exceeds bounds".to_owned(),
        ));
    }
    let Some(timestamp) = message.get("ts_ms") else {
        return Ok(Vec::new());
    };
    Ok(vec![TimingSample {
        series: format!("kalshi:{channel}:{ticker}"),
        source_time: kalshi_timestamp(timestamp)?,
    }])
}

fn kalshi_timestamp(timestamp: &serde_json::Value) -> Result<String, LatencyError> {
    let text = match timestamp {
        serde_json::Value::String(value) => value.clone(),
        serde_json::Value::Number(value) => value.to_string(),
        _ => {
            return Err(LatencyError::InvalidPayload(
                "Kalshi ts_ms must be Unix milliseconds".to_owned(),
            ));
        }
    };
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(LatencyError::InvalidPayload(
            "Kalshi ts_ms must be Unix milliseconds".to_owned(),
        ));
    }
    let millis = text
        .parse::<i64>()
        .map_err(|_| LatencyError::InvalidPayload("Kalshi ts_ms exceeds i64".to_owned()))?;
    Utc.timestamp_millis_opt(millis)
        .single()
        .ok_or_else(|| LatencyError::InvalidPayload("Kalshi ts_ms is outside UTC range".to_owned()))
        .map(|value| value.to_rfc3339_opts(SecondsFormat::Millis, true))
}

fn coinbase_samples(envelope: Envelope<'_>) -> Result<Vec<TimingSample>, LatencyError> {
    let Some(message_type) = envelope.message_type else {
        return Ok(Vec::new());
    };
    let channel = match message_type {
        "last_match" => "match_snapshot",
        "match" | "ticker" | "heartbeat" | "l2update" => message_type,
        _ => return Ok(Vec::new()),
    };
    let Some(source_time) = envelope.time else {
        return Ok(Vec::new());
    };
    let product = envelope.product_id.unwrap_or("unknown");
    Ok(vec![TimingSample {
        series: format!("coinbase_exchange:{channel}:{product}"),
        source_time: source_time.to_owned(),
    }])
}

fn kraken_samples(envelope: Envelope<'_>) -> Result<Vec<TimingSample>, LatencyError> {
    let Some(channel @ ("trade" | "book" | "ticker" | "ohlc")) = envelope.channel else {
        return Ok(Vec::new());
    };
    let Some(data) = envelope.data else {
        return Ok(Vec::new());
    };
    let message_type = match envelope.message_type {
        Some(value @ ("snapshot" | "update")) => value,
        _ => return Ok(Vec::new()),
    };
    let rows: Vec<KrakenTimingRow<'_>> =
        serde_json::from_str(data.get()).map_err(|error| LatencyError::Json(error.to_string()))?;
    if rows.len() > MAX_ROWS_PER_MESSAGE {
        return Err(LatencyError::RowLimit {
            actual: rows.len(),
            maximum: MAX_ROWS_PER_MESSAGE,
        });
    }
    if channel == "ohlc" {
        let Some(source_time) = envelope.timestamp else {
            return Ok(Vec::new());
        };
        let symbols = rows
            .into_iter()
            .filter_map(|row| row.symbol)
            .collect::<BTreeSet<_>>();
        return Ok(symbols
            .into_iter()
            .map(|symbol| TimingSample {
                series: format!("kraken_spot:ohlc_message:{message_type}:{symbol}"),
                source_time: source_time.to_owned(),
            })
            .collect());
    }
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            Some(TimingSample {
                series: format!("kraken_spot:{channel}:{message_type}:{}", row.symbol?),
                source_time: row.timestamp?.to_owned(),
            })
        })
        .collect())
}

#[derive(Deserialize)]
struct PolymarketSubscription {
    #[serde(rename = "type")]
    message_type: String,
    #[serde(rename = "assets_ids")]
    asset_ids: Vec<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum PolymarketPayload {
    Rows(Vec<PolymarketTimingRow>),
    Row(PolymarketTimingRow),
}

#[derive(Deserialize)]
struct PolymarketTimingRow {
    event_type: String,
    timestamp: PolymarketTimestamp,
    #[serde(default)]
    asset_id: Option<String>,
    #[serde(default)]
    price_changes: Vec<PolymarketPriceChange>,
}

impl PolymarketTimingRow {
    fn asset_ids(&self) -> impl Iterator<Item = &String> {
        self.asset_id
            .iter()
            .chain(self.price_changes.iter().map(|change| &change.asset_id))
    }
}

#[derive(Deserialize)]
struct PolymarketPriceChange {
    asset_id: String,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum PolymarketTimestamp {
    String(String),
    Number(serde_json::Number),
}

impl PolymarketTimestamp {
    fn text(&self) -> String {
        match self {
            Self::String(value) => value.clone(),
            Self::Number(value) => value.to_string(),
        }
    }
}

fn polymarket_rows(payload: &str) -> Result<Vec<PolymarketTimingRow>, LatencyError> {
    let rows = match serde_json::from_str(payload)
        .map_err(|error| LatencyError::Json(error.to_string()))?
    {
        PolymarketPayload::Rows(rows) => rows,
        PolymarketPayload::Row(row) => vec![row],
    };
    if rows.len() > MAX_ROWS_PER_MESSAGE
        || rows
            .iter()
            .any(|row| row.price_changes.len() > MAX_ROWS_PER_MESSAGE)
    {
        return Err(LatencyError::RowLimit {
            actual: rows.iter().map(|row| row.price_changes.len().max(1)).sum(),
            maximum: MAX_ROWS_PER_MESSAGE,
        });
    }
    Ok(rows)
}

fn polymarket_samples(payload: &str) -> Result<Vec<TimingSample>, LatencyError> {
    if payload == "PONG" {
        return Ok(Vec::new());
    }
    let rows = polymarket_rows(payload)?;
    let mut samples = Vec::new();
    for row in rows {
        let source_time = polymarket_timestamp(&row.timestamp)?;
        let asset_ids = row.asset_ids().cloned().collect::<Vec<_>>();
        let event_type = row.event_type;
        for asset_id in asset_ids {
            samples.push(TimingSample {
                series: format!("polymarket:{event_type}:{asset_id}"),
                source_time: source_time.clone(),
            });
        }
    }
    Ok(samples)
}

fn polymarket_timestamp(timestamp: &PolymarketTimestamp) -> Result<String, LatencyError> {
    let text = timestamp.text();
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(LatencyError::InvalidPayload(
            "Polymarket timestamp must be Unix milliseconds".to_owned(),
        ));
    }
    let millis = text
        .parse::<i64>()
        .map_err(|_| LatencyError::InvalidPayload("Polymarket timestamp exceeds i64".to_owned()))?;
    Utc.timestamp_millis_opt(millis)
        .single()
        .ok_or_else(|| {
            LatencyError::InvalidPayload("Polymarket timestamp is outside UTC range".to_owned())
        })
        .map(|value| value.to_rfc3339_opts(SecondsFormat::Millis, true))
}

fn timestamp_us(value: &str, field: &str) -> Result<i64, LatencyError> {
    if value.len() > 64 {
        return Err(LatencyError::InvalidPayload(format!(
            "{field} exceeds 64 bytes"
        )));
    }
    DateTime::parse_from_rfc3339(value)
        .map(|timestamp| timestamp.timestamp_micros())
        .map_err(|error| LatencyError::InvalidPayload(format!("invalid {field}: {error}")))
}

fn distribution(samples: &mut [i64]) -> Result<Distribution, LatencyError> {
    if samples.is_empty() {
        return Err(LatencyError::InvalidPayload(
            "cannot summarize an empty sample set".to_owned(),
        ));
    }
    samples.sort_unstable();
    let sample_count = u64::try_from(samples.len()).map_err(|_| LatencyError::CounterExhausted)?;
    let negative_samples = u64::try_from(samples.partition_point(|sample| *sample < 0))
        .map_err(|_| LatencyError::CounterExhausted)?;
    Ok(Distribution {
        samples: sample_count,
        minimum_us: samples[0],
        p50_us: percentile(samples, 50),
        p95_us: percentile(samples, 95),
        p99_us: percentile(samples, 99),
        maximum_us: samples[samples.len() - 1],
        negative_samples,
    })
}

fn percentile(sorted: &[i64], percentile: usize) -> i64 {
    let rank = sorted.len().saturating_mul(percentile).saturating_add(99) / 100;
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

/// Why a timing pass refused to produce a report. Every variant aborts the pass;
/// the analyzer never degrades to a partial summary.
#[derive(Debug, Error)]
pub enum LatencyError {
    /// The capture journal itself could not be read or decoded into a record.
    #[error(transparent)]
    Raw(#[from] RawRecordError),
    /// A record was structurally readable but not timeable: a missing payload, a
    /// timestamp that is not RFC 3339 or not Unix milliseconds, a value outside
    /// the shared plausibility window, a subscription whose channel/symbol
    /// exceeds the analysis bounds, or a series with no samples to summarize.
    #[error("invalid timing payload: {0}")]
    InvalidPayload(String),
    /// A payload failed JSON deserialization; carries the serde_json message.
    #[error("timing JSON failed: {0}")]
    Json(String),
    /// Total timed samples across all series would exceed `MAX_SAMPLES`.
    #[error("timing sample count exceeds {maximum}")]
    SampleLimit {
        /// The ceiling that was exceeded, in samples.
        maximum: usize,
    },
    /// One message carried more data rows than `MAX_ROWS_PER_MESSAGE`.
    #[error("timing message has {actual} rows; maximum is {maximum}")]
    RowLimit {
        /// Rows the offending message actually carried, as a count.
        actual: usize,
        /// The ceiling that was exceeded, in rows per message.
        maximum: usize,
    },
    /// The capture recorded more distinct connection epochs than
    /// `MAX_STARTUP_CONNECTIONS`, so startup timing was abandoned.
    #[error("startup timing exceeds {maximum} connection epochs")]
    StartupConnectionLimit {
        /// The ceiling that was exceeded, in retained connection epochs.
        maximum: usize,
    },
    /// One connection epoch sent more subscribe requests than
    /// `MAX_SUBSCRIPTIONS_PER_CONNECTION`.
    #[error("startup timing exceeds {maximum} subscriptions in one connection epoch")]
    SubscriptionLimit {
        /// The ceiling that was exceeded, in subscriptions per connection epoch.
        maximum: usize,
    },
    /// A timestamp subtraction or unit conversion went out of range: a source
    /// time after receipt beyond `i64`, a monotonic milestone before its start,
    /// or nanoseconds that do not fit a signed microsecond count.
    #[error("timestamp arithmetic overflowed")]
    TimestampOverflow,
    /// A record, sample, or regression counter would overflow `u64`, or a sample
    /// count does not fit `u64`.
    #[error("timing counter exhausted")]
    CounterExhausted,
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::analyze;
    use crate::{CaptureStream, Provider, RawRecord, RecordKind};

    #[test]
    fn reports_exact_percentiles_and_interarrival() {
        let records = [
            RawRecord {
                schema_version: 1,
                provider: Provider::CoinbaseExchange,
                stream: CaptureStream::CoinbaseCore,
                connection_epoch: 1,
                ordinal: 1,
                received_at: "2026-08-12T12:00:00Z".to_owned(),
                capture_elapsed_ns: None,
                connect_started_elapsed_ns: None,
                kind: RecordKind::Connected,
                payload: None,
            },
            RawRecord {
                schema_version: 1,
                provider: Provider::CoinbaseExchange,
                stream: CaptureStream::CoinbaseCore,
                connection_epoch: 1,
                ordinal: 2,
                received_at: "2026-08-12T12:00:00.001000Z".to_owned(),
                capture_elapsed_ns: None,
                connect_started_elapsed_ns: None,
                kind: RecordKind::InboundText,
                payload: Some(
                    r#"{"type":"ticker","product_id":"BTC-USD","time":"2026-08-12T12:00:00Z"}"#
                        .to_owned(),
                ),
            },
            RawRecord {
                schema_version: 1,
                provider: Provider::CoinbaseExchange,
                stream: CaptureStream::CoinbaseCore,
                connection_epoch: 1,
                ordinal: 3,
                received_at: "2026-08-12T12:00:00.004000Z".to_owned(),
                capture_elapsed_ns: None,
                connect_started_elapsed_ns: None,
                kind: RecordKind::InboundText,
                payload: Some(
                    r#"{"type":"ticker","product_id":"BTC-USD","time":"2026-08-12T12:00:00.002000Z"}"#
                        .to_owned(),
                ),
            },
        ];
        let mut bytes = Vec::new();
        for record in records {
            serde_json::to_writer(&mut bytes, &record).expect("record serializes");
            bytes.push(b'\n');
        }

        let report = analyze(&mut Cursor::new(bytes)).expect("analysis succeeds");
        assert_eq!(report.timed_events, 2);
        let series = &report.series[0];
        assert_eq!(series.source_to_local_receipt.minimum_us, 1_000);
        assert_eq!(series.source_to_local_receipt.p50_us, 1_000);
        assert_eq!(series.source_to_local_receipt.p99_us, 2_000);
        assert_eq!(
            series
                .local_message_interarrival
                .as_ref()
                .expect("interarrival exists")
                .p99_us,
            3_000
        );
    }

    #[test]
    fn times_coinbase_call_to_ack_and_first_market_data_monotonically() {
        let records = [
            RawRecord::connected_at(
                CaptureStream::CoinbaseCore,
                1,
                1,
                "2026-08-12T12:00:00Z".to_owned(),
                50_000_000,
                10_000_000,
            ),
            RawRecord::text_at(
                CaptureStream::CoinbaseCore,
                1,
                2,
                RecordKind::OutboundText,
                r#"{"type":"subscribe","channels":["level2_batch"]}"#.to_owned(),
                "2026-08-12T12:00:00.010000Z".to_owned(),
                60_000_000,
            ),
            RawRecord::text_at(
                CaptureStream::CoinbaseCore,
                1,
                3,
                RecordKind::InboundText,
                r#"{"type":"subscriptions","channels":[]}"#.to_owned(),
                "2026-08-12T12:00:00.015000Z".to_owned(),
                65_000_000,
            ),
            RawRecord::text_at(
                CaptureStream::CoinbaseCore,
                1,
                4,
                RecordKind::InboundText,
                r#"{"type":"snapshot","product_id":"BTC-USD","bids":[],"asks":[]}"#.to_owned(),
                "2026-08-12T12:00:00.018000Z".to_owned(),
                68_000_000,
            ),
        ];
        let mut bytes = Vec::new();
        for record in records {
            serde_json::to_writer(&mut bytes, &record).expect("record serializes");
            bytes.push(b'\n');
        }

        let report = analyze(&mut Cursor::new(bytes)).expect("analysis succeeds");
        let connection = &report.startup.connections[0];
        assert_eq!(connection.connect_call_to_websocket_ready_us, 40_000);
        let subscription = &connection.subscriptions[0];
        assert_eq!(subscription.call_to_ack_us, Some(5_000));
        assert_eq!(subscription.accepted, Some(true));
        assert_eq!(subscription.call_to_first_data_us, Some(8_000));
    }

    #[test]
    fn times_kraken_subscription_by_request_and_channel() {
        let records = [
            RawRecord::connected_at(
                CaptureStream::KrakenCore,
                1,
                1,
                "2026-08-12T12:00:00Z".to_owned(),
                50_000_000,
                10_000_000,
            ),
            RawRecord::text_at(
                CaptureStream::KrakenCore,
                1,
                2,
                RecordKind::OutboundText,
                r#"{"method":"subscribe","params":{"channel":"trade","symbol":["BTC/USD"]},"req_id":2}"#.to_owned(),
                "2026-08-12T12:00:00.010000Z".to_owned(),
                60_000_000,
            ),
            RawRecord::text_at(
                CaptureStream::KrakenCore,
                1,
                3,
                RecordKind::InboundText,
                r#"{"method":"subscribe","req_id":2,"success":true}"#.to_owned(),
                "2026-08-12T12:00:00.017000Z".to_owned(),
                67_000_000,
            ),
            RawRecord::text_at(
                CaptureStream::KrakenCore,
                1,
                4,
                RecordKind::InboundText,
                r#"{"channel":"trade","type":"snapshot","data":[{"symbol":"BTC/USD","timestamp":"2026-08-12T12:00:00.020000Z"}]}"#.to_owned(),
                "2026-08-12T12:00:00.022000Z".to_owned(),
                72_000_000,
            ),
        ];
        let mut bytes = Vec::new();
        for record in records {
            serde_json::to_writer(&mut bytes, &record).expect("record serializes");
            bytes.push(b'\n');
        }

        let report = analyze(&mut Cursor::new(bytes)).expect("analysis succeeds");
        let connection = &report.startup.connections[0];
        assert_eq!(connection.connect_call_to_websocket_ready_us, 40_000);
        let subscription = &connection.subscriptions[0];
        assert_eq!(subscription.subscription, "trade");
        assert_eq!(subscription.request_id, Some(2));
        assert_eq!(subscription.call_to_ack_us, Some(7_000));
        assert_eq!(subscription.accepted, Some(true));
        assert_eq!(subscription.call_to_first_data_us, Some(12_000));
    }

    #[test]
    fn times_binance_aggregate_trade_source_to_receipt() {
        let records = [
            RawRecord {
                schema_version: 1,
                provider: Provider::BinanceSpot,
                stream: CaptureStream::BinanceSpot,
                connection_epoch: 1,
                ordinal: 1,
                received_at: "2026-08-28T12:00:00Z".to_owned(),
                capture_elapsed_ns: None,
                connect_started_elapsed_ns: None,
                kind: RecordKind::Connected,
                payload: None,
            },
            RawRecord {
                schema_version: 1,
                provider: Provider::BinanceSpot,
                stream: CaptureStream::BinanceSpot,
                connection_epoch: 1,
                ordinal: 2,
                // 100 ms after the aggTrade event time E below.
                received_at: "2026-08-28T12:00:00.223000Z".to_owned(),
                capture_elapsed_ns: None,
                connect_started_elapsed_ns: None,
                kind: RecordKind::InboundText,
                payload: Some(
                    r#"{"stream":"btcusdt@aggTrade","data":{"e":"aggTrade","E":1787918400123,"s":"BTCUSDT","a":10,"p":"60000.00","q":"0.5","f":1,"l":2,"T":1787918400100,"m":true,"M":true}}"#
                        .to_owned(),
                ),
            },
        ];
        let mut bytes = Vec::new();
        for record in records {
            serde_json::to_writer(&mut bytes, &record).expect("record serializes");
            bytes.push(b'\n');
        }

        let report = analyze(&mut Cursor::new(bytes)).expect("analysis succeeds");
        assert_eq!(report.timed_events, 1);
        assert_eq!(report.series[0].series, "binance_spot:btcusdt@aggTrade");
        assert_eq!(report.series[0].source_to_local_receipt.minimum_us, 100_000);
    }

    #[test]
    fn usdm_leaves_every_remaining_frame_untimed() {
        // After ruling E62 removed the `market`-connection socket, no USD-M frame
        // carries a per-message wire-send time: the `public` partial-book depth
        // snapshot and the bounded REST bodies (object and top-level array) are
        // honestly untimed rather than panicking in the shared envelope parser
        // (the pre-fix behaviour).
        fn inbound(
            stream: CaptureStream,
            ordinal: u64,
            received_at: &str,
            payload: &str,
        ) -> RawRecord {
            RawRecord {
                schema_version: 1,
                provider: stream.provider(),
                stream,
                connection_epoch: 1,
                ordinal,
                received_at: received_at.to_owned(),
                capture_elapsed_ns: None,
                connect_started_elapsed_ns: None,
                kind: RecordKind::InboundText,
                payload: Some(payload.to_owned()),
            }
        }
        let records = [
            RawRecord {
                schema_version: 1,
                provider: Provider::BinanceUsdm,
                stream: CaptureStream::BinanceUsdmPublic,
                connection_epoch: 1,
                ordinal: 1,
                received_at: "2026-08-28T12:00:00Z".to_owned(),
                capture_elapsed_ns: None,
                connect_started_elapsed_ns: None,
                kind: RecordKind::Connected,
                payload: None,
            },
            // Partial-book depth snapshot: untimed.
            inbound(
                CaptureStream::BinanceUsdmPublic,
                2,
                "2026-08-28T12:00:00.500000Z",
                r#"{"stream":"btcusdt@depth20@100ms","data":{"e":"depthUpdate","E":1787918400400,"T":1787918400380,"s":"BTCUSDT","U":880200,"u":880100,"pu":880096,"b":[["60000.10","1.0"]],"a":[]}}"#,
            ),
            // REST response object body (open interest): untimed, not a JSON error.
            inbound(
                CaptureStream::BinanceUsdmRest,
                3,
                "2026-08-28T12:00:01.000000Z",
                r#"{"symbol":"BTCUSDT","openInterest":"12345.678","time":1787918400000}"#,
            ),
            // REST response array body (open-interest history / funding info): untimed.
            inbound(
                CaptureStream::BinanceUsdmRest,
                4,
                "2026-08-28T12:00:02.000000Z",
                r#"[{"symbol":"BTCUSDT","sumOpenInterest":"12345.678","timestamp":1787918400000}]"#,
            ),
        ];
        let mut bytes = Vec::new();
        for record in records {
            serde_json::to_writer(&mut bytes, &record).expect("record serializes");
            bytes.push(b'\n');
        }

        let report = analyze(&mut Cursor::new(bytes)).expect("analysis succeeds");
        assert_eq!(report.timed_events, 0);
        assert!(report.series.is_empty());
    }

    #[test]
    fn times_bybit_trade_orderbook_and_liquidation_source_shapes() {
        fn inbound(
            stream: CaptureStream,
            ordinal: u64,
            received_at: &str,
            payload: &str,
        ) -> RawRecord {
            RawRecord {
                schema_version: 1,
                provider: stream.provider(),
                stream,
                connection_epoch: 1,
                ordinal,
                received_at: received_at.to_owned(),
                capture_elapsed_ns: None,
                connect_started_elapsed_ns: None,
                kind: RecordKind::InboundText,
                payload: Some(payload.to_owned()),
            }
        }

        fn analyze_series(records: &[RawRecord]) -> super::LatencyReport {
            let mut bytes = Vec::new();
            for record in records {
                serde_json::to_writer(&mut bytes, record).expect("record serializes");
                bytes.push(b'\n');
            }
            analyze(&mut Cursor::new(bytes)).expect("analysis succeeds")
        }

        // Spot capture: publicTrade is timed by `T`, the book push by `cts`.
        let spot = analyze_series(&[
            inbound(
                CaptureStream::BybitSpot,
                1,
                "2026-08-28T12:00:00.223000Z",
                r#"{"topic":"publicTrade.BTCUSDT","type":"snapshot","ts":1787918400123,"data":[{"T":1787918400100,"s":"BTCUSDT","S":"Buy","v":"0.001","p":"60000.50","i":"uuid","BT":false,"seq":1}]}"#,
            ),
            inbound(
                CaptureStream::BybitSpot,
                2,
                "2026-08-28T12:00:00.220000Z",
                r#"{"topic":"orderbook.200.BTCUSDT","type":"snapshot","ts":1787918400123,"data":{"s":"BTCUSDT","b":[["60000.10","1.0"]],"a":[],"u":100,"seq":1},"cts":1787918400120}"#,
            ),
        ]);
        assert_eq!(spot.timed_events, 2);
        let spot_series: Vec<&str> = spot.series.iter().map(|s| s.series.as_str()).collect();
        assert!(spot_series.contains(&"bybit_spot:publicTrade.BTCUSDT"));
        assert!(spot_series.contains(&"bybit_spot:orderbook.200.BTCUSDT"));

        // Linear capture: allLiquidation is timed by `T`.
        let linear = analyze_series(&[inbound(
            CaptureStream::BybitLinear,
            1,
            "2026-08-28T12:00:00.223000Z",
            r#"{"topic":"allLiquidation.BTCUSDT","type":"snapshot","ts":1787918400123,"data":[{"T":1787918400100,"s":"BTCUSDT","S":"Sell","v":"20000","p":"0.04499"}]}"#,
        )]);
        assert_eq!(linear.timed_events, 1);
        assert_eq!(
            linear.series[0].series,
            "bybit_linear:allLiquidation.BTCUSDT"
        );

        // A book push with no cts is untimed: it yields no timing sample.
        let untimed = analyze_series(&[inbound(
            CaptureStream::BybitSpot,
            1,
            "2026-08-28T12:00:00.220000Z",
            r#"{"topic":"orderbook.200.BTCUSDT","type":"snapshot","ts":1787918400123,"data":{"s":"BTCUSDT","b":[["60000.10","1.0"]],"a":[],"u":100,"seq":1}}"#,
        )]);
        assert_eq!(untimed.timed_events, 0);
        assert!(untimed.series.is_empty());
    }

    #[test]
    fn times_chainlink_new_rounds_only_and_skips_duplicate_rounds() {
        fn word(value: u128) -> String {
            format!("{value:064x}")
        }
        fn response(round_id: u128, answer: u128, updated: u128) -> String {
            let result = format!(
                "0x{}{}{}{}{}",
                word(round_id),
                word(answer),
                word(updated),
                word(updated),
                word(round_id),
            );
            format!(r#"{{"jsonrpc":"2.0","id":1,"result":"{result}"}}"#)
        }
        fn inbound(ordinal: u64, received_at: &str, payload: &str) -> RawRecord {
            RawRecord {
                schema_version: 1,
                provider: Provider::ChainlinkAggregator,
                stream: CaptureStream::ChainlinkAggregator,
                connection_epoch: 1,
                ordinal,
                received_at: received_at.to_owned(),
                capture_elapsed_ns: None,
                connect_started_elapsed_ns: None,
                kind: RecordKind::InboundText,
                payload: Some(payload.to_owned()),
            }
        }

        let round_id = 129_127_208_515_966_885_128u128;
        let answer = 7_766_779_700_000u128;
        // updatedAt 1756400000 = 2025-08-28T16:53:20Z; receipts land 100ms after.
        let records = [
            inbound(
                1,
                "2025-08-28T16:53:20.100Z",
                &response(round_id, answer, 1_756_400_000),
            ),
            // Same round: untimed (a poll that saw no on-chain update).
            inbound(
                2,
                "2025-08-28T16:53:50.100Z",
                &response(round_id, answer, 1_756_400_000),
            ),
            // Advanced round: timed by its own updatedAt.
            inbound(
                3,
                "2025-08-28T16:54:20.100Z",
                &response(round_id + 1, answer, 1_756_400_060),
            ),
        ];
        let mut bytes = Vec::new();
        for record in records {
            serde_json::to_writer(&mut bytes, &record).expect("record serializes");
            bytes.push(b'\n');
        }

        let report = analyze(&mut Cursor::new(bytes)).expect("analysis succeeds");
        // Two new rounds are timed; the duplicate-round poll is an untimed message.
        assert_eq!(report.timed_events, 2);
        assert_eq!(report.untimed_messages, 1);
        assert_eq!(report.series.len(), 1);
        assert_eq!(
            report.series[0].series,
            "chainlink_aggregator:latest-round-data"
        );
        assert_eq!(report.series[0].source_to_local_receipt.minimum_us, 100_000);
    }

    #[test]
    fn chainlink_round_cursor_resets_on_connected_across_epochs() {
        fn word(value: u128) -> String {
            format!("{value:064x}")
        }
        fn response(round_id: u128, updated: u128) -> String {
            let result = format!(
                "0x{}{}{}{}{}",
                word(round_id),
                word(7_766_779_700_000),
                word(updated),
                word(updated),
                word(round_id),
            );
            format!(r#"{{"jsonrpc":"2.0","id":1,"result":"{result}"}}"#)
        }
        fn base(ordinal: u64, epoch: u64, kind: RecordKind, payload: Option<&str>) -> RawRecord {
            RawRecord {
                schema_version: 1,
                provider: Provider::ChainlinkAggregator,
                stream: CaptureStream::ChainlinkAggregator,
                connection_epoch: epoch,
                ordinal,
                received_at: "2025-08-28T16:53:20.100Z".to_owned(),
                capture_elapsed_ns: None,
                connect_started_elapsed_ns: None,
                kind,
                payload: payload.map(str::to_owned),
            }
        }

        let round = 129_127_208_515_966_885_128u128;
        // Round R is timed, then re-seen after a reconnect and timed AGAIN because
        // the new epoch clears the cursor (mirrors the adapter re-emitting it).
        let records = [
            base(1, 1, RecordKind::Connected, None),
            base(
                2,
                1,
                RecordKind::InboundText,
                Some(&response(round, 1_756_400_000)),
            ),
            base(3, 1, RecordKind::Disconnected, None),
            base(4, 2, RecordKind::Connected, None),
            base(
                5,
                2,
                RecordKind::InboundText,
                Some(&response(round, 1_756_400_000)),
            ),
        ];
        let mut bytes = Vec::new();
        for record in records {
            serde_json::to_writer(&mut bytes, &record).expect("record serializes");
            bytes.push(b'\n');
        }

        let report = analyze(&mut Cursor::new(bytes)).expect("analysis succeeds");
        assert_eq!(report.timed_events, 2);
        assert_eq!(report.untimed_messages, 0);
    }

    #[test]
    fn rejects_binance_aggregate_trade_event_time_outside_plausible_window() {
        // A microsecond-scale E would otherwise produce a time-traveled latency
        // sample; the analyzer enforces the same window as the adapter.
        let record = RawRecord {
            schema_version: 1,
            provider: Provider::BinanceSpot,
            stream: CaptureStream::BinanceSpot,
            connection_epoch: 1,
            ordinal: 1,
            received_at: "2026-08-28T12:00:00Z".to_owned(),
            capture_elapsed_ns: None,
            connect_started_elapsed_ns: None,
            kind: RecordKind::InboundText,
            payload: Some(
                r#"{"stream":"btcusdt@aggTrade","data":{"e":"aggTrade","E":1787918400123000,"s":"BTCUSDT","a":10,"p":"60000.00","q":"0.5","f":1,"l":2,"T":1787918400100,"m":true,"M":true}}"#
                    .to_owned(),
            ),
        };
        let mut bytes = Vec::new();
        serde_json::to_writer(&mut bytes, &record).expect("record serializes");
        bytes.push(b'\n');

        let error = analyze(&mut Cursor::new(bytes)).expect_err("microsecond E is rejected");
        assert!(
            error
                .to_string()
                .contains("outside the plausible millisecond window"),
            "unexpected error: {error}"
        );
    }
}
