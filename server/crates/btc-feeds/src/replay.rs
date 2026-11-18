//! Stream dispatch and the two ways to drive it: [`replay`] over a recorded
//! capture, and [`normalize`] over live line-delimited provider messages. Both
//! run the same deterministic adapters.

use std::io::{BufRead, Write};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::binance::BinanceAdapter;
use crate::binance_usdm::{BinanceUsdmAdapter, BinanceUsdmConnection};
use crate::bybit::{BybitAdapter, BybitMarket, BybitRestAdapter};
use crate::chainlink::ChainlinkAdapter;
use crate::coinbase::CoinbaseAdapter;
use crate::kalshi::KalshiAdapter;
use crate::kalshi_index::KalshiIndexAdapter;
use crate::kraken::KrakenAdapter;
use crate::kraken_l3::KrakenLevel3Adapter;
use crate::nba_espn::NbaEspnAdapter;
use crate::nba_espn_daily::NbaEspnDailyAdapter;
use crate::nba_espn_games::NbaEspnGamesAdapter;
use crate::polymarket::PolymarketAdapter;
use crate::polymarket_twap::PolymarketTwapAdapter;
use crate::pyth_hermes::{PythHermesAdapter, PythHermesConnection};
use crate::raw::{CaptureStream, RawRecord, RawRecordError, RecordKind, read_record};
use crate::sportsdataio_nba::SportsDataIoNbaAdapter;
use crate::{AdapterError, FeedAdapter, FeedNoticeCode, IngestOutcome};

const BTC_SYMBOL: &str = "BTC/USD";
const USDT_SYMBOL: &str = "USDT/USD";
const MAX_CONFIGURATION_PREFIX_RECORDS: usize = 64;

/// Provider-sensitive configuration recovered from the capture itself.
///
/// Only Kraken carries any: its book depth is a subscription parameter that the
/// adapter must know before it can maintain a book, and it is read back out of
/// the capture's recorded outbound subscribe messages rather than supplied by
/// the operator. Every other stream leaves both fields `None`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct ReplayConfiguration {
    /// Depth recorded for the `BTC/USD` book subscription, if the capture
    /// contains one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kraken_btc_book_depth: Option<u16>,
    /// Depth recorded for the `USDT/USD` book subscription, if the capture
    /// contains one (`kraken-core` only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kraken_usdt_book_depth: Option<u16>,
}

/// Kraken aggregated-book subscription depths an adapter must be built with.
///
/// These are Kraken-only: every other stream passes
/// [`KrakenBookDepths::default()`], whose 10/10 is the venue's own default depth
/// and is never read for a non-Kraken stream. Kraken accepts only 10, 25, 100,
/// 500 or 1000; any other value fails at construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KrakenBookDepths {
    /// Depth of the `BTC/USD` book subscription. Also used for the level-3
    /// (order-by-order) book.
    pub btc: u16,
    /// Depth of the `USDT/USD` book subscription (`kraken-core` only).
    pub usdt: u16,
}

impl Default for KrakenBookDepths {
    fn default() -> Self {
        Self { btc: 10, usdt: 10 }
    }
}

/// Deterministic adapter selection for one capture stream.
///
/// This is the single type that dispatches every venue: construct it from a
/// [`CaptureStream`] and drive it with [`FeedAdapter::ingest`], and the right
/// adapter and state machine are selected for you.
pub enum ReplayAdapter {
    /// [`CaptureStream::BinanceSpot`].
    Binance(BinanceAdapter),
    /// [`CaptureStream::BinanceUsdmPublic`] or [`CaptureStream::BinanceUsdmRest`].
    /// The connection kind the adapter carries fixes which stream names and
    /// source ids it will normalize.
    BinanceUsdm(BinanceUsdmAdapter),
    /// [`CaptureStream::BybitSpot`] or [`CaptureStream::BybitLinear`].
    Bybit(BybitAdapter),
    /// [`CaptureStream::BybitLinearRest`]: the two linear REST poll identities.
    BybitRest(BybitRestAdapter),
    /// [`CaptureStream::ChainlinkAggregator`].
    Chainlink(ChainlinkAdapter),
    /// The three Coinbase streams.
    Coinbase(CoinbaseAdapter),
    /// [`CaptureStream::KrakenCore`].
    Kraken(KrakenAdapter),
    /// [`CaptureStream::KrakenLevel3`].
    KrakenLevel3(KrakenLevel3Adapter),
    /// [`CaptureStream::NbaEspnStatus`].
    NbaEspn(NbaEspnAdapter),
    /// [`CaptureStream::NbaEspnDaily`].
    NbaEspnDaily(NbaEspnDailyAdapter),
    /// [`CaptureStream::NbaEspnGames`].
    NbaEspnGames(NbaEspnGamesAdapter),
    /// [`CaptureStream::KalshiMarket`]. Boxed: its state is much larger than
    /// every other variant's.
    ///
    /// `index` is the index-channel adapter for a socket that also carries the
    /// CF Benchmarks and Pyth channels. The two families share one connection
    /// (observed, ruling E23), so one variant owns both adapters and routes each
    /// inbound frame to the one that models it. It is `None` for a capture or a
    /// lane that subscribed no index channel, which is what keeps an existing
    /// `kalshi_market` replay byte-identical.
    Kalshi {
        /// The market channels: `orderbook_delta`, `ticker`, `trade`.
        market: Box<KalshiAdapter>,
        /// The index channels riding the same socket, when armed.
        index: Option<Box<KalshiIndexAdapter>>,
    },
    /// [`CaptureStream::KalshiIndex`].
    KalshiIndex(KalshiIndexAdapter),
    /// [`CaptureStream::PolymarketMarket`]. Boxed for the same reason.
    Polymarket(Box<PolymarketAdapter>),
    /// [`CaptureStream::PolymarketChainlinkTwap`].
    PolymarketTwap(PolymarketTwapAdapter),
    /// [`CaptureStream::PythHermesStream`] or [`CaptureStream::PythHermesRest`].
    /// The connection kind the adapter carries fixes the `update_kind` it stamps.
    PythHermes(PythHermesAdapter),
    /// [`CaptureStream::SportsDataIoNba`].
    SportsDataIoNba(SportsDataIoNbaAdapter),
}

impl ReplayAdapter {
    /// Construct the adapter that owns `stream`.
    ///
    /// `publisher_epoch` is copied verbatim into every emitted event; it must be
    /// non-blank and at most 128 bytes. `kraken_depths` is read only for the
    /// `kraken-core` and `kraken-level3` streams — pass
    /// [`KrakenBookDepths::default()`] for everything else.
    ///
    /// Prefer [`ReplaySession`] for a Kraken capture: it recovers the recorded
    /// depths from the capture itself and constructs the adapter with them.
    pub fn new(
        stream: CaptureStream,
        publisher_epoch: impl Into<String>,
        kraken_depths: KrakenBookDepths,
    ) -> Result<Self, AdapterError> {
        let publisher_epoch = publisher_epoch.into();
        let KrakenBookDepths {
            btc: kraken_btc_book_depth,
            usdt: kraken_usdt_book_depth,
        } = kraken_depths;
        match stream {
            CaptureStream::BinanceSpot => Ok(Self::Binance(BinanceAdapter::new(publisher_epoch)?)),
            CaptureStream::BinanceUsdmPublic => Ok(Self::BinanceUsdm(BinanceUsdmAdapter::new(
                publisher_epoch,
                BinanceUsdmConnection::Public,
            )?)),
            CaptureStream::BinanceUsdmRest => Ok(Self::BinanceUsdm(BinanceUsdmAdapter::new(
                publisher_epoch,
                BinanceUsdmConnection::Rest,
            )?)),
            CaptureStream::BybitSpot => Ok(Self::Bybit(BybitAdapter::new(
                publisher_epoch,
                BybitMarket::Spot,
            )?)),
            CaptureStream::BybitLinear => Ok(Self::Bybit(BybitAdapter::new(
                publisher_epoch,
                BybitMarket::Linear,
            )?)),
            CaptureStream::BybitLinearRest => {
                Ok(Self::BybitRest(BybitRestAdapter::new(publisher_epoch)?))
            }
            CaptureStream::ChainlinkAggregator => {
                Ok(Self::Chainlink(ChainlinkAdapter::new(publisher_epoch)?))
            }
            CaptureStream::CoinbaseCore
            | CaptureStream::CoinbaseLevel3
            | CaptureStream::CoinbaseMechanisms => {
                Ok(Self::Coinbase(CoinbaseAdapter::new(publisher_epoch)?))
            }
            // E47(e): the dedicated USDT/USD reference socket is decoded by the
            // same Kraken adapter; its capture carries only `ticker` frames for
            // `USDT/USD`, so the adapter emits only `kraken.spot.usdt-usd.ticker`
            // and its book state (constructed but never fed) is inert.
            CaptureStream::KrakenCore | CaptureStream::KrakenUsdtReference => {
                Ok(Self::Kraken(KrakenAdapter::new(
                    publisher_epoch,
                    kraken_btc_book_depth,
                    kraken_usdt_book_depth,
                )?))
            }
            CaptureStream::KrakenLevel3 => Ok(Self::KrakenLevel3(KrakenLevel3Adapter::new(
                publisher_epoch,
                kraken_btc_book_depth,
            )?)),
            CaptureStream::NbaEspnStatus => {
                Ok(Self::NbaEspn(NbaEspnAdapter::new(publisher_epoch)?))
            }
            CaptureStream::NbaEspnDaily => Ok(Self::NbaEspnDaily(NbaEspnDailyAdapter::new(
                publisher_epoch,
            )?)),
            CaptureStream::NbaEspnGames => Ok(Self::NbaEspnGames(NbaEspnGamesAdapter::new(
                publisher_epoch,
            )?)),
            CaptureStream::KalshiMarket => Ok(Self::Kalshi {
                market: Box::new(KalshiAdapter::new(publisher_epoch)?),
                index: None,
            }),
            CaptureStream::KalshiIndex => Ok(Self::KalshiIndex(
                KalshiIndexAdapter::with_default_configuration(publisher_epoch)?,
            )),
            CaptureStream::PolymarketMarket => Ok(Self::Polymarket(Box::new(
                PolymarketAdapter::new(publisher_epoch)?,
            ))),
            CaptureStream::PolymarketChainlinkTwap => Ok(Self::PolymarketTwap(
                PolymarketTwapAdapter::new(publisher_epoch)?,
            )),
            CaptureStream::PythHermesStream => Ok(Self::PythHermes(PythHermesAdapter::new(
                publisher_epoch,
                PythHermesConnection::Stream,
            )?)),
            CaptureStream::PythHermesRest => Ok(Self::PythHermes(PythHermesAdapter::new(
                publisher_epoch,
                PythHermesConnection::Latest,
            )?)),
            CaptureStream::SportsDataIoNba => Ok(Self::SportsDataIoNba(
                SportsDataIoNbaAdapter::new(publisher_epoch)?,
            )),
        }
    }

    /// End-of-stream completeness check for the selected adapter.
    ///
    /// Kalshi, Polymarket and Polymarket-TWAP require evidence the capture was
    /// complete (a configuration record, the exact recorded subscriptions);
    /// every other stream has no such requirement and always returns `Ok`.
    /// Call it once, after the last record.
    pub fn finish(&self) -> Result<(), AdapterError> {
        match self {
            Self::Kalshi { market, .. } => market.finish(),
            Self::Polymarket(adapter) => adapter.finish(),
            Self::PolymarketTwap(adapter) => adapter.finish(),
            Self::Binance(_)
            | Self::BinanceUsdm(_)
            | Self::Bybit(_)
            | Self::BybitRest(_)
            | Self::Chainlink(_)
            | Self::Coinbase(_)
            | Self::Kraken(_)
            | Self::KrakenLevel3(_)
            | Self::NbaEspn(_)
            | Self::NbaEspnDaily(_)
            | Self::NbaEspnGames(_)
            | Self::KalshiIndex(_)
            | Self::SportsDataIoNba(_) => Ok(()),
            Self::PythHermes(adapter) => adapter.finish(),
        }
    }

    /// Arm the Kalshi index channels on a `kalshi_market` adapter.
    ///
    /// The index channels are subscribed on the market socket, so the lane that
    /// subscribes them normalizes them through the same record stream. Calling
    /// this on any other variant is a programming error and is refused.
    pub fn arm_kalshi_index_channels(
        &mut self,
        publisher_epoch: impl Into<String>,
    ) -> Result<(), AdapterError> {
        let Self::Kalshi { index, .. } = self else {
            return Err(AdapterError::InvalidRecord(
                "only the Kalshi market socket carries the index channels".to_owned(),
            ));
        };
        *index = Some(Box::new(KalshiIndexAdapter::with_default_configuration(
            publisher_epoch,
        )?));
        Ok(())
    }
}

/// Route one record of a Kalshi socket that carries both the market and the
/// index channels.
///
/// Control records reach both adapters — each keeps its own connection state —
/// while a text record goes to exactly the adapter that models it. A
/// configuration record describes the market binding only, so the index adapter,
/// which has none, never sees one.
fn ingest_kalshi_socket(
    market: &mut KalshiAdapter,
    index: Option<&mut KalshiIndexAdapter>,
    record: &RawRecord,
) -> Result<IngestOutcome, AdapterError> {
    let Some(index) = index else {
        return market.ingest(record);
    };
    match record.kind {
        RecordKind::InboundText
            if record
                .payload
                .as_deref()
                .is_some_and(crate::kalshi_index::routes_to_index_channel) =>
        {
            index.ingest(record)
        }
        // Index subscribes are OutboundText on the shared socket (ruling E23).
        // They belong to the index adapter, which ignores outbound records; the
        // market adapter would refuse them and stop the whole market lane, the
        // outcome ruling E2 forbids when `cfbrti`/`pyth` is armed.
        RecordKind::OutboundText
            if record
                .payload
                .as_deref()
                .is_some_and(crate::kalshi_index::outbound_routes_to_index_channel) =>
        {
            index.ingest(record)
        }
        RecordKind::Connected | RecordKind::Disconnected => {
            let mut outcome = market.ingest(record)?;
            let index_outcome = index.ingest(record)?;
            outcome.events.extend(index_outcome.events);
            outcome.notices.extend(index_outcome.notices);
            Ok(outcome)
        }
        _ => market.ingest(record),
    }
}

impl FeedAdapter for ReplayAdapter {
    fn ingest(&mut self, record: &RawRecord) -> Result<IngestOutcome, AdapterError> {
        match self {
            Self::Binance(adapter) => adapter.ingest(record),
            Self::BinanceUsdm(adapter) => adapter.ingest(record),
            Self::Bybit(adapter) => adapter.ingest(record),
            Self::BybitRest(adapter) => adapter.ingest(record),
            Self::Chainlink(adapter) => adapter.ingest(record),
            Self::Coinbase(adapter) => adapter.ingest(record),
            Self::Kraken(adapter) => adapter.ingest(record),
            Self::KrakenLevel3(adapter) => adapter.ingest(record),
            Self::NbaEspn(adapter) => adapter.ingest(record),
            Self::NbaEspnDaily(adapter) => adapter.ingest(record),
            Self::NbaEspnGames(adapter) => adapter.ingest(record),
            Self::Kalshi { market, index } => {
                ingest_kalshi_socket(market, index.as_deref_mut(), record)
            }
            Self::KalshiIndex(adapter) => adapter.ingest(record),
            Self::Polymarket(adapter) => adapter.ingest(record),
            Self::PolymarketTwap(adapter) => adapter.ingest(record),
            Self::SportsDataIoNba(adapter) => adapter.ingest(record),
            Self::PythHermes(adapter) => adapter.ingest(record),
        }
    }
}

/// Streaming replay state that derives configuration before constructing Kraken state.
///
/// Feed it every record of one capture, in order, then call
/// [`ReplaySession::finish`]. For a Kraken stream it buffers the prefix until
/// the recorded book subscriptions reveal the depths, then constructs the
/// adapter and replays the buffered prefix through it; for every other stream
/// the adapter exists from the start.
pub struct ReplaySession {
    stream: CaptureStream,
    publisher_epoch: String,
    kraken_btc_depth_override: Option<u16>,
    configuration: ReplayConfiguration,
    adapter: Option<ReplayAdapter>,
    pending: Vec<RawRecord>,
}

impl ReplaySession {
    /// Begin a replay of `stream`.
    ///
    /// `kraken_btc_depth_override` is an assertion, not a setting: replay always
    /// uses the depth recorded in the capture, and a supplied value that
    /// disagrees fails with [`ReplayError::ConfigurationMismatch`].
    pub fn new(
        stream: CaptureStream,
        publisher_epoch: impl Into<String>,
        kraken_btc_depth_override: Option<u16>,
    ) -> Result<Self, ReplayError> {
        let publisher_epoch = publisher_epoch.into();
        let adapter = match stream {
            CaptureStream::BinanceSpot
            | CaptureStream::BinanceUsdmPublic
            | CaptureStream::BinanceUsdmRest
            | CaptureStream::BybitSpot
            | CaptureStream::BybitLinear
            | CaptureStream::BybitLinearRest
            | CaptureStream::ChainlinkAggregator
            | CaptureStream::CoinbaseCore
            | CaptureStream::CoinbaseLevel3
            | CaptureStream::CoinbaseMechanisms
            | CaptureStream::KalshiMarket
            | CaptureStream::KalshiIndex
            | CaptureStream::NbaEspnDaily
            | CaptureStream::NbaEspnGames
            | CaptureStream::NbaEspnStatus
            | CaptureStream::PolymarketMarket
            | CaptureStream::PolymarketChainlinkTwap
            | CaptureStream::PythHermesStream
            | CaptureStream::PythHermesRest
            // E47(e): the USDT/USD reference socket carries no book channel, so
            // there is no capture-recorded depth to observe first; its adapter is
            // constructed eagerly like the other depthless streams.
            | CaptureStream::KrakenUsdtReference
            | CaptureStream::SportsDataIoNba => Some(ReplayAdapter::new(
                stream,
                publisher_epoch.clone(),
                KrakenBookDepths::default(),
            )?),
            CaptureStream::KrakenCore | CaptureStream::KrakenLevel3 => None,
        };
        Ok(Self {
            stream,
            publisher_epoch,
            kraken_btc_depth_override,
            configuration: ReplayConfiguration::default(),
            adapter,
            pending: Vec::new(),
        })
    }

    /// Configuration recovered from the capture so far.
    pub const fn configuration(&self) -> ReplayConfiguration {
        self.configuration
    }

    /// Admit one record of the capture, in capture order.
    ///
    /// Every record must belong to `stream`; a mixed capture is
    /// [`ReplayError::MixedStreams`]. While a Kraken capture's depths are still
    /// unknown the outcome is empty and the record is buffered, and the buffered
    /// results arrive with the record that completes the configuration.
    pub fn ingest(&mut self, record: &RawRecord) -> Result<IngestOutcome, ReplayError> {
        if record.stream != self.stream {
            return Err(ReplayError::MixedStreams {
                expected: self.stream,
                actual: record.stream,
            });
        }
        if self.adapter.is_none() {
            record.validate()?;
        }
        self.observe_configuration(record)?;

        if let Some(adapter) = &mut self.adapter {
            return adapter.ingest(record).map_err(ReplayError::Adapter);
        }
        if record.kind == RecordKind::InboundText || record.kind == RecordKind::Disconnected {
            return Err(ReplayError::MissingConfiguration {
                stream: self.stream,
            });
        }
        if self.pending.len() >= MAX_CONFIGURATION_PREFIX_RECORDS {
            return Err(ReplayError::ConfigurationPrefixLimit {
                maximum: MAX_CONFIGURATION_PREFIX_RECORDS,
            });
        }
        self.pending.push(record.clone());
        if !self.configuration_is_complete() {
            return Ok(IngestOutcome::default());
        }

        let btc_depth =
            self.configuration
                .kraken_btc_book_depth
                .ok_or(ReplayError::MissingConfiguration {
                    stream: self.stream,
                })?;
        let usdt_depth = self.configuration.kraken_usdt_book_depth.unwrap_or(10);
        let mut adapter = ReplayAdapter::new(
            self.stream,
            self.publisher_epoch.clone(),
            KrakenBookDepths {
                btc: btc_depth,
                usdt: usdt_depth,
            },
        )?;
        let mut combined = IngestOutcome::default();
        for pending in self.pending.drain(..) {
            let outcome = adapter.ingest(&pending)?;
            combined.events.extend(outcome.events);
            combined.notices.extend(outcome.notices);
        }
        self.adapter = Some(adapter);
        Ok(combined)
    }

    /// Assert the capture was complete. Call once, after the last record.
    pub fn finish(&self) -> Result<(), ReplayError> {
        self.adapter
            .as_ref()
            .ok_or(ReplayError::MissingConfiguration {
                stream: self.stream,
            })?
            .finish()
            .map_err(ReplayError::Adapter)
    }

    fn configuration_is_complete(&self) -> bool {
        match self.stream {
            CaptureStream::BinanceSpot
            | CaptureStream::BinanceUsdmPublic
            | CaptureStream::BinanceUsdmRest
            | CaptureStream::BybitSpot
            | CaptureStream::BybitLinear
            | CaptureStream::BybitLinearRest
            | CaptureStream::ChainlinkAggregator
            | CaptureStream::CoinbaseCore
            | CaptureStream::CoinbaseLevel3
            | CaptureStream::CoinbaseMechanisms
            | CaptureStream::KalshiMarket
            | CaptureStream::KalshiIndex
            | CaptureStream::NbaEspnDaily
            | CaptureStream::NbaEspnGames
            | CaptureStream::NbaEspnStatus
            | CaptureStream::PolymarketMarket
            | CaptureStream::PolymarketChainlinkTwap
            | CaptureStream::PythHermesStream
            | CaptureStream::PythHermesRest
            // E47(e): depthless reference socket, nothing to configure.
            | CaptureStream::KrakenUsdtReference
            | CaptureStream::SportsDataIoNba => true,
            CaptureStream::KrakenCore => {
                self.configuration.kraken_btc_book_depth.is_some()
                    && self.configuration.kraken_usdt_book_depth.is_some()
            }
            CaptureStream::KrakenLevel3 => self.configuration.kraken_btc_book_depth.is_some(),
        }
    }

    fn observe_configuration(&mut self, record: &RawRecord) -> Result<(), ReplayError> {
        if record.kind != RecordKind::OutboundText
            || !matches!(
                self.stream,
                CaptureStream::KrakenCore | CaptureStream::KrakenLevel3
            )
        {
            return Ok(());
        }
        let payload = record
            .payload
            .as_deref()
            .ok_or_else(|| ReplayError::InvalidConfiguration("missing payload".to_owned()))?;
        let subscription: Subscription = serde_json::from_str(payload).map_err(|error| {
            ReplayError::InvalidConfiguration(format!("invalid outbound JSON: {error}"))
        })?;
        if subscription.method.as_deref() != Some("subscribe") {
            return Ok(());
        }
        let Some(params) = subscription.params else {
            return Ok(());
        };
        let expected_channel = match self.stream {
            CaptureStream::KrakenCore => "book",
            CaptureStream::KrakenLevel3 => "level3",
            _ => return Ok(()),
        };
        if params.channel.as_deref() != Some(expected_channel) {
            return Ok(());
        }
        let depth = params.depth.ok_or_else(|| {
            ReplayError::InvalidConfiguration(format!(
                "Kraken {expected_channel} subscription is missing depth"
            ))
        })?;
        for symbol in params.symbol {
            match symbol.as_str() {
                BTC_SYMBOL => self.record_depth(BTC_SYMBOL, depth)?,
                USDT_SYMBOL if self.stream == CaptureStream::KrakenCore => {
                    self.record_depth(USDT_SYMBOL, depth)?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn record_depth(&mut self, symbol: &'static str, depth: u16) -> Result<(), ReplayError> {
        if symbol == BTC_SYMBOL
            && let Some(requested) = self.kraken_btc_depth_override
            && requested != depth
        {
            return Err(ReplayError::ConfigurationMismatch {
                symbol,
                recorded: depth,
                requested,
            });
        }
        let slot = if symbol == BTC_SYMBOL {
            &mut self.configuration.kraken_btc_book_depth
        } else {
            &mut self.configuration.kraken_usdt_book_depth
        };
        if let Some(recorded) = *slot {
            if recorded != depth {
                return Err(ReplayError::ConfigurationChanged {
                    symbol,
                    first: recorded,
                    later: depth,
                });
            }
        } else {
            *slot = Some(depth);
        }
        Ok(())
    }
}

#[derive(Deserialize)]
struct Subscription {
    method: Option<String>,
    params: Option<SubscriptionParams>,
}

#[derive(Deserialize)]
struct SubscriptionParams {
    channel: Option<String>,
    #[serde(default)]
    symbol: Vec<String>,
    depth: Option<u16>,
}

/// Totals for one completed [`replay`] run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReplayReport {
    /// Stream identity taken from the capture's first record.
    pub stream: CaptureStream,
    /// Configuration recovered from the capture (Kraken depths, or empty).
    pub configuration: ReplayConfiguration,
    /// Capture records admitted.
    pub records: u64,
    /// Normalized events written to the output.
    pub events: u64,
    /// Feed notices produced, of every code.
    pub notices: u64,
    /// Subset of `notices` that carried a recovery action, or whose code was
    /// `Gap` or `ChecksumMismatch`.
    pub recovery_notices: u64,
}

/// Replay one capture and write normalized `SourceEvent` JSONL.
///
/// Reads bounded JSONL [`RawRecord`]s from `reader`, drives the stream's
/// adapter, and writes one JSON [`SourceEvent`] per line to `writer`. Notices
/// are counted in the returned report but not written. Returns `Ok(None)` for
/// an empty capture.
pub fn replay<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    publisher_epoch: &str,
    kraken_btc_depth_override: Option<u16>,
) -> Result<Option<ReplayReport>, ReplayError> {
    let Some(first) = read_record(reader)? else {
        return Ok(None);
    };
    let stream = first.stream;
    let mut session = ReplaySession::new(stream, publisher_epoch, kraken_btc_depth_override)?;
    let mut report = ReplayReport {
        stream,
        configuration: ReplayConfiguration::default(),
        records: 0,
        events: 0,
        notices: 0,
        recovery_notices: 0,
    };

    ingest_and_write(&mut session, &first, writer, &mut report)?;
    while let Some(record) = read_record(reader)? {
        ingest_and_write(&mut session, &record, writer, &mut report)?;
    }
    session.finish()?;
    report.configuration = session.configuration();
    writer.flush().map_err(ReplayError::Io)?;
    Ok(Some(report))
}

/// How to drive [`normalize`].
#[derive(Debug, Clone)]
pub struct NormalizeOptions {
    /// Which stream the incoming messages belong to. Selects the adapter.
    pub stream: CaptureStream,
    /// Copied verbatim into every emitted event. Non-blank, at most 128 bytes.
    pub publisher_epoch: String,
    /// Kraken aggregated-book subscription depths. Read only for the
    /// `kraken-core` and `kraken-level3` streams; pass
    /// [`KrakenBookDepths::default()`] otherwise.
    pub kraken_depths: KrakenBookDepths,
    /// Discovery journal for a prediction-market stream, ingested as the
    /// Configuration record before anything else. Required for `kalshi-market`
    /// and `polymarket-market`; refused by every other stream.
    pub configuration: Option<String>,
    /// Messages the producer sent *to* the venue — subscriptions, keepalives —
    /// replayed as `OutboundText` records, in this order, after the `Connected`
    /// record and before the first inbound message.
    ///
    /// Kalshi and Polymarket refuse market data until they have seen the exact
    /// subscription that was sent, so those two streams need one entry per
    /// subscription. Every other stream can leave this empty.
    pub subscriptions: Vec<String>,
}

/// Totals for one completed [`normalize`] run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct NormalizeReport {
    /// Provider messages read from the input.
    pub messages: u64,
    /// Normalized events written.
    pub events: u64,
    /// Feed notices written.
    pub notices: u64,
}

/// Normalize a live line-delimited provider message stream.
///
/// Reads one raw provider message per line from `messages` — exactly the
/// WebSocket text as received, or one REST response body per line for a
/// REST-polled stream — synthesizes the [`RawRecord`] envelope around each
/// (Configuration first when `options.configuration` is set, then `Connected`,
/// then one `OutboundText` per entry of `options.subscriptions`, then one
/// `InboundText` per line, with ordinals increasing from 1 and `received_at`
/// taken from the wall clock at read time), and drives the same
/// [`ReplayAdapter`] replay uses.
///
/// Normalized events are written to `events` as JSONL and feed notices to
/// `notices`, one JSON object per line. Blank input lines are skipped. On
/// end of input the adapter's [`ReplayAdapter::finish`] runs, so an incomplete
/// prediction-market stream is reported rather than silently accepted.
///
/// The wall clock is the only nondeterminism: `received_at` differs between two
/// runs over the same input, and so therefore does every event's `received_at`.
/// Parsing itself is unchanged.
pub fn normalize<R: BufRead, W: Write, N: Write>(
    messages: &mut R,
    events: &mut W,
    notices: &mut N,
    options: &NormalizeOptions,
) -> Result<NormalizeReport, ReplayError> {
    let mut adapter = ReplayAdapter::new(
        options.stream,
        options.publisher_epoch.clone(),
        options.kraken_depths,
    )?;
    let mut report = NormalizeReport {
        messages: 0,
        events: 0,
        notices: 0,
    };
    let mut ordinal = 0_u64;
    let mut next_ordinal = || -> Result<u64, ReplayError> {
        ordinal = ordinal
            .checked_add(1)
            .ok_or(ReplayError::CounterExhausted)?;
        Ok(ordinal)
    };

    if let Some(configuration) = &options.configuration {
        let record = RawRecord::configuration_at(
            options.stream,
            1,
            next_ordinal()?,
            configuration.clone(),
            crate::raw::now_timestamp(),
            0,
        );
        write_outcome(adapter.ingest(&record)?, events, notices, &mut report)?;
    }
    let connected = RawRecord::connected_at(
        options.stream,
        1,
        next_ordinal()?,
        crate::raw::now_timestamp(),
        0,
        0,
    );
    write_outcome(adapter.ingest(&connected)?, events, notices, &mut report)?;

    for subscription in &options.subscriptions {
        let record = RawRecord::text_at(
            options.stream,
            1,
            next_ordinal()?,
            RecordKind::OutboundText,
            subscription.clone(),
            crate::raw::now_timestamp(),
            0,
        );
        write_outcome(adapter.ingest(&record)?, events, notices, &mut report)?;
    }

    let mut line = String::new();
    loop {
        line.clear();
        if read_bounded_line(messages, &mut line)? == 0 {
            break;
        }
        let payload = line.trim_end_matches(['\n', '\r']);
        if payload.is_empty() {
            continue;
        }
        let record = RawRecord::text_at(
            options.stream,
            1,
            next_ordinal()?,
            RecordKind::InboundText,
            payload.to_owned(),
            crate::raw::now_timestamp(),
            0,
        );
        report.messages = report
            .messages
            .checked_add(1)
            .ok_or(ReplayError::CounterExhausted)?;
        write_outcome(adapter.ingest(&record)?, events, notices, &mut report)?;
    }

    adapter.finish()?;
    events.flush().map_err(ReplayError::Io)?;
    notices.flush().map_err(ReplayError::Io)?;
    Ok(report)
}

/// Read one line into `line`, refusing anything over
/// [`MAX_MESSAGE_BYTES`](crate::MAX_MESSAGE_BYTES) so a hostile stdin cannot
/// force an unbounded allocation. The bound is on the line's payload; the
/// terminating newline is not counted. Returns the bytes read including any
/// newline, `0` at end of input.
///
/// Bytes are accumulated across `fill_buf` chunks and decoded once, because a
/// buffered reader may hand back a chunk that ends inside a multi-byte
/// character; decoding chunk by chunk would refuse valid input whenever such a
/// character straddles the buffer boundary.
fn read_bounded_line<R: BufRead>(reader: &mut R, line: &mut String) -> Result<usize, ReplayError> {
    let mut bytes = Vec::new();
    let mut total = 0_usize;
    let terminated = loop {
        let available = reader.fill_buf().map_err(ReplayError::Io)?;
        if available.is_empty() {
            break false;
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let payload = newline.unwrap_or(available.len());
        if bytes.len().saturating_add(payload) > crate::MAX_MESSAGE_BYTES {
            // Where the bound was crossed, which is all this reader knows: the
            // rest of the line is deliberately never read, so the true length
            // is this or more.
            return Err(ReplayError::Raw(RawRecordError::MessageTooLarge {
                observed_at_least: bytes.len().saturating_add(payload),
                maximum: crate::MAX_MESSAGE_BYTES,
            }));
        }
        bytes.extend_from_slice(&available[..payload]);
        let take = newline.map_or(payload, |index| index + 1);
        total = total.saturating_add(take);
        reader.consume(take);
        if newline.is_some() {
            break true;
        }
    };
    if total == 0 {
        return Ok(0);
    }
    let text = String::from_utf8(bytes).map_err(|error| {
        ReplayError::InvalidConfiguration(format!("input line is not UTF-8: {error}"))
    })?;
    line.push_str(&text);
    if terminated {
        line.push('\n');
    }
    Ok(total)
}

fn write_outcome<W: Write, N: Write>(
    outcome: IngestOutcome,
    events: &mut W,
    notices: &mut N,
    report: &mut NormalizeReport,
) -> Result<(), ReplayError> {
    for event in outcome.events {
        serde_json::to_writer(&mut *events, &event).map_err(ReplayError::Json)?;
        events.write_all(b"\n").map_err(ReplayError::Io)?;
        report.events = report
            .events
            .checked_add(1)
            .ok_or(ReplayError::CounterExhausted)?;
    }
    for notice in outcome.notices {
        serde_json::to_writer(&mut *notices, &notice).map_err(ReplayError::Json)?;
        notices.write_all(b"\n").map_err(ReplayError::Io)?;
        report.notices = report
            .notices
            .checked_add(1)
            .ok_or(ReplayError::CounterExhausted)?;
    }
    Ok(())
}

fn ingest_and_write<W: Write>(
    session: &mut ReplaySession,
    record: &RawRecord,
    writer: &mut W,
    report: &mut ReplayReport,
) -> Result<(), ReplayError> {
    let outcome = session.ingest(record)?;
    report.records = report
        .records
        .checked_add(1)
        .ok_or(ReplayError::CounterExhausted)?;
    report.events = report
        .events
        .checked_add(
            u64::try_from(outcome.events.len()).map_err(|_| ReplayError::CounterExhausted)?,
        )
        .ok_or(ReplayError::CounterExhausted)?;
    report.notices = report
        .notices
        .checked_add(
            u64::try_from(outcome.notices.len()).map_err(|_| ReplayError::CounterExhausted)?,
        )
        .ok_or(ReplayError::CounterExhausted)?;
    report.recovery_notices = report
        .recovery_notices
        .checked_add(
            u64::try_from(
                outcome
                    .notices
                    .iter()
                    .filter(|notice| {
                        notice.recovery.is_some()
                            || matches!(
                                notice.code,
                                FeedNoticeCode::Gap | FeedNoticeCode::ChecksumMismatch
                            )
                    })
                    .count(),
            )
            .map_err(|_| ReplayError::CounterExhausted)?,
        )
        .ok_or(ReplayError::CounterExhausted)?;

    for event in outcome.events {
        serde_json::to_writer(&mut *writer, &event).map_err(ReplayError::Json)?;
        writer.write_all(b"\n").map_err(ReplayError::Io)?;
    }
    Ok(())
}

/// Anything that stops a replay: a bad record, an adapter refusal, output I/O,
/// or a capture whose configuration cannot be established.
#[derive(Debug, Error)]
pub enum ReplayError {
    /// A capture record could not be read or failed validation.
    #[error(transparent)]
    Raw(#[from] RawRecordError),
    /// The adapter refused the record or the payload it carried.
    #[error(transparent)]
    Adapter(#[from] AdapterError),
    /// Writing normalized output failed.
    #[error("normalized output I/O failed: {0}")]
    Io(std::io::Error),
    /// Serializing a normalized event failed.
    #[error("normalized output JSON failed: {0}")]
    Json(serde_json::Error),
    /// The capture contains records from more than one stream.
    #[error("capture mixes streams: expected {expected:?}, received {actual:?}")]
    MixedStreams {
        /// Stream established by the capture's first record.
        expected: CaptureStream,
        /// Stream carried by the offending record.
        actual: CaptureStream,
    },
    /// A Kraken capture ended without ever revealing its book depths, or
    /// `finish` ran before the adapter existed.
    #[error("capture does not contain the required replay configuration for {stream:?}")]
    MissingConfiguration {
        /// The stream whose configuration is missing.
        stream: CaptureStream,
    },
    /// A recorded Kraken subscription message could not be parsed, or omitted
    /// its depth.
    #[error("invalid capture replay configuration: {0}")]
    InvalidConfiguration(String),
    /// The operator asserted a depth the capture disagrees with. Replay always
    /// uses the recorded depth; the assertion fails rather than being ignored.
    #[error(
        "Kraken {symbol} depth mismatch: capture recorded {recorded}, operator requested {requested}"
    )]
    ConfigurationMismatch {
        /// Kraken pair symbol.
        symbol: &'static str,
        /// Depth the capture recorded.
        recorded: u16,
        /// Depth the operator asserted.
        requested: u16,
    },
    /// One capture recorded two different depths for the same pair, so no
    /// single book state can reproduce it.
    #[error("Kraken {symbol} depth changes inside the capture from {first} to {later}")]
    ConfigurationChanged {
        /// Kraken pair symbol.
        symbol: &'static str,
        /// Depth recorded first.
        first: u16,
        /// Conflicting depth recorded later.
        later: u16,
    },
    /// The Kraken subscription messages did not appear within the bounded
    /// prefix, so the buffered prefix is refused rather than grown.
    #[error("replay configuration was not found within {maximum} prefix records")]
    ConfigurationPrefixLimit {
        /// Prefix-record budget that was exhausted.
        maximum: usize,
    },
    /// A run counter reached its maximum. Replay refuses to wrap.
    #[error("replay counters exhausted")]
    CounterExhausted,
}

#[cfg(test)]
mod bounded_line_tests {
    use std::io::{BufReader, Cursor};

    use super::read_bounded_line;

    fn read_all(capacity: usize, input: &str) -> Vec<String> {
        let mut reader = BufReader::with_capacity(capacity, Cursor::new(input.as_bytes()));
        let mut lines = Vec::new();
        loop {
            let mut line = String::new();
            let read = read_bounded_line(&mut reader, &mut line).expect("valid input reads");
            if read == 0 {
                break;
            }
            lines.push(line);
        }
        lines
    }

    #[test]
    fn a_multibyte_character_straddling_the_buffer_boundary_is_decoded_once() {
        // "é" is two bytes; with a 4-byte buffer the first fill ends inside it.
        let input = "abcé\n—x\n";
        for capacity in 1..=8 {
            assert_eq!(
                read_all(capacity, input),
                vec!["abcé\n".to_owned(), "—x\n".to_owned()],
                "capacity {capacity}"
            );
        }
    }

    #[test]
    fn an_unterminated_final_line_is_returned_without_a_newline() {
        assert_eq!(
            read_all(3, "one\ntwo"),
            vec!["one\n".to_owned(), "two".to_owned()]
        );
    }

    #[test]
    fn invalid_utf8_is_refused_with_the_line_intact_in_the_error() {
        let mut reader = BufReader::with_capacity(2, Cursor::new(&b"ab\xff\n"[..]));
        let mut line = String::new();
        let error = read_bounded_line(&mut reader, &mut line).expect_err("invalid UTF-8");
        assert!(error.to_string().contains("not UTF-8"), "{error}");
    }

    #[test]
    fn the_size_bound_counts_the_payload_and_not_the_newline() {
        let payload = "x".repeat(crate::MAX_MESSAGE_BYTES);
        let terminated = format!("{payload}\n");
        let mut reader = BufReader::new(Cursor::new(terminated.as_bytes()));
        let mut line = String::new();
        let read = read_bounded_line(&mut reader, &mut line).expect("exactly at the bound");
        assert_eq!(read, crate::MAX_MESSAGE_BYTES + 1);
        let over = "x".repeat(crate::MAX_MESSAGE_BYTES + 1);
        let mut reader = BufReader::new(Cursor::new(over.as_bytes()));
        let mut line = String::new();
        read_bounded_line(&mut reader, &mut line).expect_err("one byte over the bound");
    }
}
