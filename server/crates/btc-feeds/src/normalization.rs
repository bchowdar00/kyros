//! Release-build timing harness for the parse-and-normalize path, measured by
//! replaying a capture through the same deterministic adapters. Capture I/O and
//! JSON serialization are outside the timer.

use std::collections::BTreeMap;
use std::hint::black_box;
use std::io::BufRead;
use std::time::Instant;

use chrono::DateTime;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::counters::checked_add;
use crate::raw::{Provider, RawRecordError, RecordKind, read_record};
use crate::replay::{ReplayConfiguration, ReplayError, ReplaySession};

const MAX_SAMPLES: usize = 5_000_000;

/// Release-build timing for adapter parsing and normalization before JSON serialization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NormalizationReport {
    /// Prose statement of exactly what the timer covered.
    pub measurement: &'static str,
    /// Configuration recovered from the capture (Kraken depths, or empty).
    pub configuration: ReplayConfiguration,
    /// Capture records admitted.
    pub records: u64,
    /// Inbound provider messages among those records.
    pub inbound_messages: u64,
    /// Records that produced at least one normalized event.
    pub messages_with_events: u64,
    /// Normalized events produced in total.
    pub normalized_events: u64,
    /// Notices that carried a recovery action.
    pub recovery_notices: u64,
    /// Per-message-family timings, ordered by series name.
    pub series: Vec<NormalizationSeries>,
    /// Prose statements of what these numbers do not prove.
    pub limitations: [&'static str; 4],
}

/// Timings for one message family (venue plus channel or topic).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NormalizationSeries {
    /// Series name, e.g. `coinbase_exchange:ticker`.
    pub series: String,
    /// Messages classified into this series.
    pub messages: u64,
    /// Normalized events those messages produced.
    pub normalized_events: u64,
    /// Time from payload available to normalized Rust values, per message.
    pub processing_time: NanosecondDistribution,
    /// Captured receipt lag plus measured processing, present only where the
    /// series produced events carrying a venue `event_time`. Requires
    /// synchronized clocks to be meaningful.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_to_normalized_ready: Option<MicrosecondDistribution>,
}

/// Summary of a nanosecond sample set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NanosecondDistribution {
    /// Number of samples summarized.
    pub samples: u64,
    /// Smallest sample.
    pub minimum_ns: u64,
    /// Median.
    pub p50_ns: u64,
    /// 95th percentile.
    pub p95_ns: u64,
    /// 99th percentile.
    pub p99_ns: u64,
    /// Largest sample.
    pub maximum_ns: u64,
}

/// Summary of a microsecond sample set that may contain negative values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MicrosecondDistribution {
    /// Number of samples summarized.
    pub samples: u64,
    /// Smallest sample.
    pub minimum_us: i64,
    /// Median.
    pub p50_us: i64,
    /// 95th percentile.
    pub p95_us: i64,
    /// 99th percentile.
    pub p99_us: i64,
    /// Largest sample.
    pub maximum_us: i64,
    /// Samples below zero, i.e. a venue timestamp ahead of local receipt. A
    /// nonzero count means the two clocks were not synchronized.
    pub negative_samples: u64,
}

#[derive(Default)]
struct Samples {
    messages: u64,
    events: u64,
    processing_ns: Vec<u64>,
    source_to_ready_us: Vec<i64>,
}

/// Time the same deterministic adapter used by replay, excluding capture I/O and JSON output.
///
/// Returns `Ok(None)` for an empty capture. `kraken_btc_depth_override` is an
/// assertion that must match the depth recorded in the capture.
pub fn analyze<R: BufRead>(
    reader: &mut R,
    publisher_epoch: &str,
    kraken_btc_depth_override: Option<u16>,
) -> Result<Option<NormalizationReport>, NormalizationError> {
    let Some(first) = read_record(reader)? else {
        return Ok(None);
    };
    let stream = first.stream;
    let mut session = ReplaySession::new(stream, publisher_epoch, kraken_btc_depth_override)?;
    let mut records = 0_u64;
    let mut inbound_messages = 0_u64;
    let mut messages_with_events = 0_u64;
    let mut normalized_events = 0_u64;
    let mut recovery_notices = 0_u64;
    let mut total_samples = 0_usize;
    let mut by_series = BTreeMap::<String, Samples>::new();

    let mut next = Some(first);
    while let Some(record) = next {
        if record.stream != stream {
            return Err(NormalizationError::MixedStreams);
        }
        records = checked_add(records, 1, || NormalizationError::CounterExhausted)?;
        let is_inbound = record.kind == RecordKind::InboundText;

        let started = Instant::now();
        let outcome = session.ingest(&record)?;
        // The measured work is the parse and normalization, whose only product
        // is `outcome`. Without this the optimizer is free to delete it as dead
        // code between the two `Instant` reads and report a timing of ~0.
        black_box(&outcome);
        let processing_ns = u64::try_from(started.elapsed().as_nanos())
            .map_err(|_| NormalizationError::CounterExhausted)?;
        let series = if is_inbound {
            Some(message_series(&record)?)
        } else {
            None
        };

        let event_count = u64::try_from(outcome.events.len())
            .map_err(|_| NormalizationError::CounterExhausted)?;
        normalized_events = checked_add(normalized_events, event_count, || {
            NormalizationError::CounterExhausted
        })?;
        if event_count > 0 {
            messages_with_events = checked_add(messages_with_events, 1, || {
                NormalizationError::CounterExhausted
            })?;
        }
        let recovery_count = u64::try_from(
            outcome
                .notices
                .iter()
                .filter(|notice| notice.recovery.is_some())
                .count(),
        )
        .map_err(|_| NormalizationError::CounterExhausted)?;
        recovery_notices = checked_add(recovery_notices, recovery_count, || {
            NormalizationError::CounterExhausted
        })?;

        if let Some(series) = series {
            inbound_messages =
                checked_add(inbound_messages, 1, || NormalizationError::CounterExhausted)?;
            total_samples = total_samples
                .checked_add(1)
                .ok_or(NormalizationError::CounterExhausted)?;
            if total_samples > MAX_SAMPLES {
                return Err(NormalizationError::SampleLimit {
                    maximum: MAX_SAMPLES,
                });
            }
            let source_to_ready =
                source_to_ready_us(&record.received_at, &outcome.events, processing_ns)?;
            let samples = by_series.entry(series).or_default();
            samples.messages =
                checked_add(samples.messages, 1, || NormalizationError::CounterExhausted)?;
            samples.events = checked_add(samples.events, event_count, || {
                NormalizationError::CounterExhausted
            })?;
            samples.processing_ns.push(processing_ns);
            if let Some(sample) = source_to_ready {
                samples.source_to_ready_us.push(sample);
            }
        }
        next = read_record(reader)?;
    }
    session.finish()?;

    let series = by_series
        .into_iter()
        .map(|(series, mut samples)| {
            let processing_time = nanosecond_distribution(&mut samples.processing_ns)?;
            let source_to_normalized_ready = if samples.source_to_ready_us.is_empty() {
                None
            } else {
                Some(microsecond_distribution(&mut samples.source_to_ready_us)?)
            };
            Ok(NormalizationSeries {
                series,
                messages: samples.messages,
                normalized_events: samples.events,
                processing_time,
                source_to_normalized_ready,
            })
        })
        .collect::<Result<Vec<_>, NormalizationError>>()?;

    Ok(Some(NormalizationReport {
        measurement: "captured WebSocket text payload available to the adapter through normalized Rust SourceEvent values; JSON serialization excluded",
        configuration: session.configuration(),
        records,
        inbound_messages,
        messages_with_events,
        normalized_events,
        recovery_notices,
        series,
        limitations: [
            "measured by deterministic replay in an optimized release build, not inline on the live socket",
            "capture-file decoding and normalized JSON serialization are outside the timer",
            "source-to-ready combines captured wall-clock receipt lag with measured local processing and requires synchronized clocks",
            "results are host-, payload-, depth-, load-, and build-specific and require longer regional benchmarks",
        ],
    }))
}

fn message_series(record: &crate::RawRecord) -> Result<String, NormalizationError> {
    let payload = record.payload.as_deref().ok_or_else(|| {
        NormalizationError::InvalidPayload("inbound record is missing payload".to_owned())
    })?;
    if record.stream == crate::CaptureStream::PolymarketChainlinkTwap {
        if payload == "PONG" {
            return Ok("polymarket_rtds:heartbeat".to_owned());
        }
        let value: serde_json::Value = serde_json::from_str(payload)
            .map_err(|error| NormalizationError::InvalidPayload(error.to_string()))?;
        let topic = value
            .get("topic")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        let event_type = value
            .get("type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        return Ok(format!("polymarket_rtds:{topic}:{event_type}"));
    }
    if record.provider == Provider::Polymarket {
        if payload == "PONG" {
            return Ok("polymarket:heartbeat".to_owned());
        }
        let value: serde_json::Value = serde_json::from_str(payload)
            .map_err(|error| NormalizationError::InvalidPayload(error.to_string()))?;
        let first = value
            .as_array()
            .and_then(|values| values.first())
            .unwrap_or(&value);
        let event_type = first
            .get("event_type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        return Ok(format!("polymarket:{event_type}"));
    }
    if record.provider == Provider::Kalshi {
        let envelope: MessageEnvelope<'_> = serde_json::from_str(payload)
            .map_err(|error| NormalizationError::InvalidPayload(error.to_string()))?;
        return Ok(format!(
            "kalshi:{}",
            envelope.message_type.unwrap_or("unknown")
        ));
    }
    if record.provider == Provider::ChainlinkAggregator {
        // Chainlink poll responses carry no topic/stream/channel and the
        // adapter tolerates arbitrary response content with a notice, so the
        // series is constant and the payload is deliberately NOT parsed here:
        // a malformed gateway body must not abort a normalization run when
        // replay of the same journal completes.
        return Ok(crate::chainlink::ROUNDS_SERIES.to_owned());
    }
    if record.provider == Provider::NbaEspnStatus {
        // NBA ESPN poll responses group by response SHAPE (scoreboard vs
        // injuries). The classifier is permissive and NEVER fails: an odd 2xx
        // body groups as `nba_espn_status:unknown` rather than aborting the run,
        // mirroring the Chainlink precedent (a malformed poll body must not abort
        // normalization when replay of the same journal completes).
        return Ok(crate::nba_espn::classify_series(payload).to_owned());
    }
    if record.provider == Provider::NbaEspnDaily {
        // The daily crawl groups by response SHAPE too, over the eight-shape
        // closed list. Same permissive contract as slice 1: an odd 2xx body
        // groups as `nba_espn_daily:unknown` rather than aborting the run.
        return Ok(crate::nba_espn_daily::classify_series(payload));
    }
    if record.provider == Provider::NbaEspnGames {
        // The game-day crawl groups by response SHAPE too, over the four-shape
        // closed list (scoreboard, odds, predictor, summary). Same permissive
        // contract as slices 1 and 2: an odd 2xx body groups as
        // `nba_espn_games:unknown` rather than aborting the run.
        return Ok(crate::nba_espn_games::classify_series(payload));
    }
    if record.provider == Provider::SportsDataIo {
        // SportsDataIO responses group by the OPERATION named in their recorded
        // response envelope, one series per allowlisted operation. Same permissive
        // contract as the ESPN slices: an envelope that does not parse, or names an
        // operation outside the allowlist, groups as `sportsdataio_nba:unknown`
        // rather than aborting a run that replay of the same journal completes.
        return Ok(crate::sportsdataio_nba::classify_series(payload));
    }
    if record.provider == Provider::PythHermes {
        // Pyth Hermes SSE event blocks are not the shared JSON envelope and the
        // feed has one logical channel, so the series is constant and the payload
        // is deliberately NOT parsed here (the Chainlink precedent).
        return Ok(crate::pyth_hermes::STREAM_SERIES.to_owned());
    }
    let envelope: MessageEnvelope<'_> = serde_json::from_str(payload)
        .map_err(|error| NormalizationError::InvalidPayload(error.to_string()))?;
    let series = match record.provider {
        Provider::CoinbaseExchange => format!(
            "coinbase_exchange:{}",
            envelope.message_type.unwrap_or("unknown")
        ),
        Provider::KrakenSpot => match (envelope.channel, envelope.message_type, envelope.method) {
            (Some(channel), Some(message_type), _) => {
                format!("kraken_spot:{channel}:{message_type}")
            }
            (_, _, Some(method)) => format!("kraken_spot:method:{method}"),
            _ => "kraken_spot:unknown".to_owned(),
        },
        // Binance combined-stream messages are grouped by their wrapping stream
        // name (`btcusdt@depth20@100ms` / `btcusdt@aggTrade`).
        Provider::BinanceSpot => {
            format!("binance_spot:{}", envelope.stream.unwrap_or("unknown"))
        }
        // USD-M frames carry the same combined-stream wrapper when the combined
        // URL form is in use; a bare per-stream frame has no wrapper and groups
        // by its own event type instead.
        Provider::BinanceUsdm => format!(
            "binance_usdm:{}",
            envelope
                .stream
                .or(envelope.message_type)
                .unwrap_or("unknown")
        ),
        // Bybit messages are grouped by their `topic` (op-response acks carry
        // none and group as `unknown`).
        Provider::BybitSpot => format!("bybit_spot:{}", envelope.topic.unwrap_or("unknown")),
        Provider::BybitLinear => format!("bybit_linear:{}", envelope.topic.unwrap_or("unknown")),
        // These seven providers are classified by the early returns above and
        // never reach the shared envelope. `RawRecord` is deserialized from an
        // untrusted file, so this is an error rather than a panic: deleting one
        // of those returns must not turn hostile input into a process abort.
        provider @ (Provider::ChainlinkAggregator
        | Provider::NbaEspnStatus
        | Provider::NbaEspnDaily
        | Provider::NbaEspnGames
        | Provider::SportsDataIo
        | Provider::PythHermes
        | Provider::Polymarket
        | Provider::Kalshi) => {
            return Err(NormalizationError::UnclassifiedProvider {
                provider: provider.as_str(),
            });
        }
    };
    Ok(series)
}

#[derive(Deserialize)]
struct MessageEnvelope<'a> {
    #[serde(rename = "type", borrow)]
    message_type: Option<&'a str>,
    #[serde(borrow)]
    channel: Option<&'a str>,
    #[serde(borrow)]
    method: Option<&'a str>,
    #[serde(borrow)]
    stream: Option<&'a str>,
    #[serde(borrow)]
    topic: Option<&'a str>,
}

fn source_to_ready_us(
    received_at: &str,
    events: &[signals_core::SourceEvent],
    processing_ns: u64,
) -> Result<Option<i64>, NormalizationError> {
    let Some(latest_event_us) = events
        .iter()
        .map(|event| timestamp_us(&event.event_time, "event_time"))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .max()
    else {
        return Ok(None);
    };
    let receipt_us = timestamp_us(received_at, "received_at")?;
    let processing_us =
        i64::try_from(processing_ns / 1_000).map_err(|_| NormalizationError::TimestampOverflow)?;
    receipt_us
        .checked_sub(latest_event_us)
        .and_then(|delay| delay.checked_add(processing_us))
        .map(Some)
        .ok_or(NormalizationError::TimestampOverflow)
}

fn timestamp_us(value: &str, field: &str) -> Result<i64, NormalizationError> {
    if value.len() > 64 {
        return Err(NormalizationError::InvalidPayload(format!(
            "{field} exceeds 64 bytes"
        )));
    }
    DateTime::parse_from_rfc3339(value)
        .map(|timestamp| timestamp.timestamp_micros())
        .map_err(|error| NormalizationError::InvalidPayload(format!("invalid {field}: {error}")))
}

fn nanosecond_distribution(
    samples: &mut [u64],
) -> Result<NanosecondDistribution, NormalizationError> {
    if samples.is_empty() {
        return Err(NormalizationError::InvalidPayload(
            "cannot summarize empty timing samples".to_owned(),
        ));
    }
    samples.sort_unstable();
    Ok(NanosecondDistribution {
        samples: u64::try_from(samples.len()).map_err(|_| NormalizationError::CounterExhausted)?,
        minimum_ns: samples[0],
        p50_ns: percentile(samples, 50),
        p95_ns: percentile(samples, 95),
        p99_ns: percentile(samples, 99),
        maximum_ns: samples[samples.len() - 1],
    })
}

fn microsecond_distribution(
    samples: &mut [i64],
) -> Result<MicrosecondDistribution, NormalizationError> {
    if samples.is_empty() {
        return Err(NormalizationError::InvalidPayload(
            "cannot summarize empty timing samples".to_owned(),
        ));
    }
    samples.sort_unstable();
    Ok(MicrosecondDistribution {
        samples: u64::try_from(samples.len()).map_err(|_| NormalizationError::CounterExhausted)?,
        minimum_us: samples[0],
        p50_us: percentile(samples, 50),
        p95_us: percentile(samples, 95),
        p99_us: percentile(samples, 99),
        maximum_us: samples[samples.len() - 1],
        negative_samples: u64::try_from(samples.partition_point(|sample| *sample < 0))
            .map_err(|_| NormalizationError::CounterExhausted)?,
    })
}

fn percentile<T: Copy>(sorted: &[T], percentile: usize) -> T {
    let rank = sorted.len().saturating_mul(percentile).saturating_add(99) / 100;
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

/// Anything that stops a normalization timing run.
#[derive(Debug, Error)]
pub enum NormalizationError {
    /// A capture record could not be read or failed validation.
    #[error(transparent)]
    Raw(#[from] RawRecordError),
    /// The underlying replay refused the record.
    #[error(transparent)]
    Replay(#[from] ReplayError),
    /// The capture contains records from more than one stream.
    #[error("capture mixes stream identities")]
    MixedStreams,
    /// A payload could not be classified into a timing series, or a timestamp
    /// needed for the source-to-ready measurement was unusable.
    #[error("invalid normalization timing payload: {0}")]
    InvalidPayload(String),
    /// The run exceeded the bounded sample budget.
    #[error("normalization timing sample count exceeds {maximum}")]
    SampleLimit {
        /// The bounded sample budget that was exceeded.
        maximum: usize,
    },
    /// Timestamp subtraction or addition overflowed `i64` microseconds.
    #[error("timestamp arithmetic overflowed")]
    TimestampOverflow,
    /// A run counter reached its maximum. The analyzer refuses to wrap.
    #[error("normalization timing counter exhausted")]
    CounterExhausted,
    /// A provider reached the shared envelope classifier that should have been
    /// classified by its own dedicated branch. Structurally unreachable today;
    /// it exists so untrusted input can never abort the process.
    #[error("provider {provider} has no timing-series classifier")]
    UnclassifiedProvider {
        /// The provider identity, as it appears in the record.
        provider: &'static str,
    },
}
