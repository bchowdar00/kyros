//! The capture record: the crate's input vocabulary, its invariants, the
//! append-only recorder that writes it and the bounded reader that reads it
//! back.

use std::io::BufRead;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use crate::{MAX_MESSAGE_BYTES, MAX_REPLAY_LINE_BYTES};

/// Schema version stamped on every record this build writes.
///
/// Bumped 3 → 4 on 2026-09-03, when [`RecordKind::Failed`] entered the
/// vocabulary. The kind is the reason: `RecordKind` has no catch-all variant,
/// so a reader that predates it fails deserializing `"kind":"failed"` with
/// `unknown variant` — a JSON error from the middle of a replay rather than the
/// version triage this module exists to give. Stamping 4 is what lets such a
/// reader say "unsupported schema_version" instead.
///
/// Readers accept 1 through 4. The write version moves; the read range only
/// grows, so every retained capture stays replayable — narrowing it would
/// strand evidence, which is the opposite of why captures are kept.
pub const RAW_RECORD_SCHEMA_VERSION: u16 = 4;
/// Oldest version this build reads.
const MIN_RAW_RECORD_SCHEMA_VERSION: u16 = 1;
const LEGACY_RAW_RECORD_SCHEMA_VERSION: u16 = 1;
const MONOTONIC_RAW_RECORD_SCHEMA_VERSION: u16 = 2;
/// First version carrying `Configuration` records, and the oldest a
/// `Configuration` record may declare.
const CONFIGURATION_RAW_RECORD_SCHEMA_VERSION: u16 = 3;
/// First version carrying [`RecordKind::Failed`]. A `Failed` record older than
/// this is a forgery or a corruption: the kind did not exist.
const FAILED_KIND_RAW_RECORD_SCHEMA_VERSION: u16 = 4;
const CAPTURE_BUFFER_BYTES: usize = 1024 * 1024;
const CAPTURE_SYNC_BYTES: u64 = 16 * 1024 * 1024;
pub(crate) const CAPTURE_SYNC_INTERVAL: Duration = Duration::from_secs(5);

/// Provider identity fixed by the compiled capture binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    /// Binance spot exchange.
    BinanceSpot,
    /// Binance USD-M (USDT-margined) futures exchange.
    BinanceUsdm,
    /// Bybit spot exchange.
    BybitSpot,
    /// Bybit USDT-perpetual (linear) exchange.
    BybitLinear,
    /// Chainlink on-chain price aggregator.
    ChainlinkAggregator,
    /// Coinbase Exchange.
    CoinbaseExchange,
    /// Kalshi prediction exchange.
    Kalshi,
    /// Kraken spot exchange.
    KrakenSpot,
    /// ESPN NBA daily reference crawl.
    NbaEspnDaily,
    /// ESPN NBA game-day crawl.
    NbaEspnGames,
    /// ESPN NBA status poll.
    NbaEspnStatus,
    /// Polymarket. Shared by the CLOB market and RTDS streams.
    Polymarket,
    /// Pyth Network Hermes. Shared by the SSE price stream and the REST anchor.
    PythHermes,
    /// SportsDataIO NBA.
    SportsDataIo,
}

impl Provider {
    /// Stable lower-snake-case token for this provider, as written to captures.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BinanceSpot => "binance_spot",
            Self::BinanceUsdm => "binance_usdm",
            Self::BybitSpot => "bybit_spot",
            Self::BybitLinear => "bybit_linear",
            Self::ChainlinkAggregator => "chainlink_aggregator",
            Self::CoinbaseExchange => "coinbase_exchange",
            Self::Kalshi => "kalshi",
            Self::KrakenSpot => "kraken_spot",
            Self::NbaEspnDaily => "nba_espn_daily",
            Self::NbaEspnGames => "nba_espn_games",
            Self::NbaEspnStatus => "nba_espn_status",
            Self::Polymarket => "polymarket",
            Self::PythHermes => "pyth_hermes",
            Self::SportsDataIo => "sportsdataio",
        }
    }
}

/// Non-overlapping channel set that preserves message provenance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureStream {
    /// Binance spot BTCUSDT combined stream: partial depth, agg-trades and the
    /// displayed best quote.
    BinanceSpot,
    /// Binance USD-M BTCUSDT perpetual, high-frequency families: partial-depth
    /// book snapshots on `fstream.binance.com/public`.
    BinanceUsdmPublic,
    /// Binance USD-M bounded REST polls: open interest, its five-minute history
    /// and the venue-declared funding interval. An HTTP poll session, not a
    /// socket, recorded as request-descriptor and response pairs.
    BinanceUsdmRest,
    /// Bybit spot BTCUSDT: trades, level-two book, BBO.
    BybitSpot,
    /// Bybit USDT-perpetual BTCUSDT: all-liquidations.
    BybitLinear,
    /// Bybit USDT-perpetual BTCUSDT REST polls: open interest and funding
    /// history, over the `http_poll` transport. Shares the `bybit_linear`
    /// provider identity with the linear WebSocket stream.
    BybitLinearRest,
    /// Chainlink BTC/USD aggregator poll responses.
    ChainlinkAggregator,
    /// Coinbase BTC-USD trades, ticker and level-two book.
    CoinbaseCore,
    /// Coinbase BTC-USD full-channel (order-by-order) stream.
    CoinbaseLevel3,
    /// Coinbase market-mechanism channels (status, auction).
    CoinbaseMechanisms,
    /// Kraken spot aggregated book plus ticker, trade, OHLC, instrument and
    /// status, over BTC/USD and USDT/USD.
    KrakenCore,
    /// Kraken spot BTC/USD level-3 order-by-order book.
    KrakenLevel3,
    /// Kraken USDT/USD reference socket (E47(e)): a dedicated Kraken connection
    /// that subscribes ONLY the `USDT/USD` `ticker` channel, so the socket
    /// carries nothing but the reference frames the venue lanes' `UsdtReference`
    /// consumes. The fat `KrakenCore` socket (BTC/USD plus USDT/USD across book,
    /// ticker, trade, ohlc, instrument and status) is untouched; this narrow
    /// socket exists so a Binance/Bybit venue lane can drive one reference feed
    /// without pulling the whole Kraken core book. It routes solely to
    /// `kraken.spot.usdt-usd.ticker`.
    KrakenUsdtReference,
    /// Kalshi BTC 15-minute market: book, ticker and trade channels.
    KalshiMarket,
    /// Kalshi authenticated index socket: the CF Benchmarks BRTI 1 Hz and 5 Hz
    /// value channels and the Pyth BTC/USD relay. A second, dedicated
    /// connection: it carries no market binding, so it survives the market
    /// socket's 15-minute rollovers untouched.
    KalshiIndex,
    /// ESPN NBA daily reference crawl responses.
    NbaEspnDaily,
    /// ESPN NBA game-day crawl responses.
    NbaEspnGames,
    /// ESPN NBA scoreboard and injury poll responses.
    NbaEspnStatus,
    /// Polymarket CLOB market socket for one BTC Up/Down market.
    PolymarketMarket,
    /// Polymarket RTDS Chainlink BTC/USD 60-second TWAP topic.
    PolymarketChainlinkTwap,
    /// Pyth Hermes BTC/USD Server-Sent-Events price stream
    /// (`/v2/updates/price/stream`). A keyed HTTPS streaming GET, recorded as a
    /// Connected epoch, one outbound request descriptor, and the SSE event blocks
    /// that follow.
    PythHermesStream,
    /// Pyth Hermes BTC/USD REST latest-anchor polls
    /// (`/v2/updates/price/latest`). A bounded start-up and post-reconnect probe
    /// whose rows carry `update_kind: "latest"`. Shares the `pyth_hermes`
    /// provider identity with the SSE stream.
    PythHermesRest,
    /// SportsDataIO NBA request-matrix responses.
    SportsDataIoNba,
}

impl CaptureStream {
    /// The provider this stream belongs to. Several streams may share one
    /// provider (the three Coinbase streams, the two Kraken streams, the two
    /// Polymarket streams).
    pub const fn provider(self) -> Provider {
        match self {
            Self::BinanceSpot => Provider::BinanceSpot,
            Self::BinanceUsdmPublic | Self::BinanceUsdmRest => Provider::BinanceUsdm,
            Self::BybitSpot => Provider::BybitSpot,
            Self::BybitLinear => Provider::BybitLinear,
            Self::BybitLinearRest => Provider::BybitLinear,
            Self::ChainlinkAggregator => Provider::ChainlinkAggregator,
            Self::CoinbaseCore | Self::CoinbaseLevel3 | Self::CoinbaseMechanisms => {
                Provider::CoinbaseExchange
            }
            Self::KrakenCore | Self::KrakenLevel3 | Self::KrakenUsdtReference => {
                Provider::KrakenSpot
            }
            Self::KalshiMarket | Self::KalshiIndex => Provider::Kalshi,
            Self::NbaEspnDaily => Provider::NbaEspnDaily,
            Self::NbaEspnGames => Provider::NbaEspnGames,
            Self::NbaEspnStatus => Provider::NbaEspnStatus,
            Self::PolymarketMarket => Provider::Polymarket,
            Self::PolymarketChainlinkTwap => Provider::Polymarket,
            Self::PythHermesStream | Self::PythHermesRest => Provider::PythHermes,
            Self::SportsDataIoNba => Provider::SportsDataIo,
        }
    }

    /// Stable lower-snake-case token for this stream, as written to captures
    /// and accepted by the CLI (with hyphens).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BinanceSpot => "binance_spot",
            Self::BinanceUsdmPublic => "binance_usdm_public",
            Self::BinanceUsdmRest => "binance_usdm_rest",
            Self::BybitSpot => "bybit_spot",
            Self::BybitLinear => "bybit_linear",
            Self::BybitLinearRest => "bybit_linear_rest",
            Self::ChainlinkAggregator => "chainlink_aggregator",
            Self::CoinbaseCore => "coinbase_core",
            Self::CoinbaseLevel3 => "coinbase_level3",
            Self::CoinbaseMechanisms => "coinbase_mechanisms",
            Self::KrakenCore => "kraken_core",
            Self::KrakenLevel3 => "kraken_level3",
            Self::KrakenUsdtReference => "kraken_usdt_reference",
            Self::KalshiMarket => "kalshi_market",
            Self::KalshiIndex => "kalshi_index",
            Self::NbaEspnDaily => "nba_espn_daily",
            Self::NbaEspnGames => "nba_espn_games",
            Self::NbaEspnStatus => "nba_espn_status",
            Self::PolymarketMarket => "polymarket_market",
            Self::PolymarketChainlinkTwap => "polymarket_chainlink_twap",
            Self::PythHermesStream => "pyth_hermes_stream",
            Self::PythHermesRest => "pyth_hermes_rest",
            Self::SportsDataIoNba => "sportsdataio_nba",
        }
    }

    /// The transport this stream is carried over. A `Websocket` stream is dialled
    /// and read frame by frame; an `HttpPoll` stream has no socket — it is a
    /// bounded request/response poll session whose one logical connection spans
    /// the run. The live runner dispatches on this: a socket dial for the former,
    /// a poll loop for the latter, so an `http_poll` stream is never dialled as a
    /// websocket (ruling E41).
    pub const fn transport(self) -> StreamTransport {
        match self {
            // Both Pyth Hermes streams are keyed HTTPS GETs — the held-open SSE
            // price stream and the bounded REST latest-anchor — never a WebSocket
            // dial, so they take the non-websocket transport and are never dialled
            // as a socket (ruling E41). The live runner refuses both outright
            // (they are driven by the oracle lane's poll/SSE seam), so this
            // classification never reaches `run_live_http_poll`.
            Self::BinanceUsdmRest
            | Self::BybitLinearRest
            | Self::ChainlinkAggregator
            | Self::NbaEspnDaily
            | Self::NbaEspnGames
            | Self::NbaEspnStatus
            | Self::PythHermesStream
            | Self::PythHermesRest
            | Self::SportsDataIoNba => StreamTransport::HttpPoll,
            Self::BinanceSpot
            | Self::BinanceUsdmPublic
            | Self::BybitSpot
            | Self::BybitLinear
            | Self::CoinbaseCore
            | Self::CoinbaseLevel3
            | Self::CoinbaseMechanisms
            | Self::KrakenCore
            | Self::KrakenLevel3
            | Self::KrakenUsdtReference
            | Self::KalshiMarket
            | Self::KalshiIndex
            | Self::PolymarketMarket
            | Self::PolymarketChainlinkTwap => StreamTransport::Websocket,
        }
    }

    /// Every `source_id` an adapter for this stream can stamp on the events it
    /// emits, as a static inventory. The lane runtime subtracts the processor's
    /// consumed sources from the union of its declared streams' inventories to
    /// find the sources it observes but does not consume (routed past the
    /// processor, counted as `ignored_events` rather than aborting the lane), and
    /// to refuse at start-up a processor that requires a source no declared stream
    /// produces. The match is exhaustive with no wildcard so a new stream cannot
    /// silently ship an empty inventory; a lane-driven stream's inventory is
    /// pinned to its adapter's constant set by a unit test, and every observed
    /// capture is replayed to prove each emitted `source_id` appears here.
    pub const fn produced_source_ids(self) -> &'static [&'static str] {
        match self {
            Self::BinanceSpot => &[
                "binance.spot.btcusdt.book-levels",
                "binance.spot.btcusdt.agg-trades",
                "binance.spot.btcusdt.book-ticker",
            ],
            Self::BinanceUsdmPublic => &["binance.usdm.btcusdt.book-levels"],
            Self::BinanceUsdmRest => &[
                "binance.usdm.btcusdt.open-interest",
                "binance.usdm.btcusdt.open-interest-hist",
                "binance.usdm.btcusdt.funding-info",
                "binance.usdm.btcusdt.premium-index",
            ],
            Self::BybitSpot => &[
                "bybit.spot.btcusdt.trades",
                "bybit.spot.btcusdt.book-levels",
                "bybit.spot.btcusdt.bbo-levels",
            ],
            Self::BybitLinear => &[
                "bybit.linear.btcusdt.liquidations",
                "bybit.linear.btcusdt.trades",
                "bybit.linear.btcusdt.book-levels",
                "bybit.linear.btcusdt.tickers",
            ],
            Self::BybitLinearRest => &[
                "bybit.linear.btcusdt.open-interest",
                "bybit.linear.btcusdt.funding-history",
            ],
            Self::ChainlinkAggregator => &["chainlink.ethereum.btc-usd.rounds"],
            Self::CoinbaseCore => &[
                "coinbase.exchange.btc-usd.trades",
                "coinbase.exchange.btc-usd.ticker",
                "coinbase.exchange.usdt-usd.ticker",
                "coinbase.exchange.btc-usd.book-levels",
                "coinbase.exchange.btc-usd.product",
            ],
            // The shared Coinbase adapter normalizes only `status` -> `product`
            // for the level-3 and mechanisms streams; their full-channel and
            // auction messages are retained-not-normalized (coinbase.rs dispatch
            // gates trade/ticker/book to `CoinbaseCore`).
            Self::CoinbaseLevel3 | Self::CoinbaseMechanisms => {
                &["coinbase.exchange.btc-usd.product"]
            }
            Self::KrakenCore => &[
                "kraken.spot.btc-usd.trades",
                "kraken.spot.usdt-usd.trades",
                "kraken.spot.btc-usd.book-levels",
                "kraken.spot.usdt-usd.book-levels",
                "kraken.spot.btc-usd.instrument",
                "kraken.spot.usdt-usd.instrument",
                "kraken.spot.btc-usd.ticker",
                "kraken.spot.usdt-usd.ticker",
                "kraken.spot.btc-usd.ohlc-1m",
                "kraken.spot.usdt-usd.ohlc-1m",
                "kraken.spot.status",
            ],
            Self::KrakenLevel3 => &["kraken.spot.btc-usd.book-orders"],
            // E47(e): the dedicated reference socket subscribes ticker-only on
            // USDT/USD, but the shared Kraken adapter also emits the venue
            // `status` frame Kraken pushes on connect (the ticker-only
            // subscription still receives it; heartbeats are notices, not source
            // events). Both belong in the inventory so F1 routing counts the
            // status frame as observed-not-consumed instead of refusing it.
            Self::KrakenUsdtReference => &["kraken.spot.usdt-usd.ticker", "kraken.spot.status"],
            // Both Pyth Hermes streams (SSE price stream and REST latest-anchor)
            // emit the single shared price source; the live lane runner refuses
            // them (they are driven by the oracle poll/SSE seam), so this inventory
            // is proved only by replaying the observed capture.
            Self::PythHermesStream | Self::PythHermesRest => &["pyth.hermes.btc-usd.price"],
            Self::KalshiMarket => &[
                "kalshi.btc.market-contracts",
                "kalshi.btc.book-levels",
                "kalshi.btc.quotes",
                "kalshi.btc.trades",
            ],
            Self::KalshiIndex => &[
                "kalshi.cfbenchmarks.brti.value",
                "kalshi.cfbenchmarks.brti.value-5hz",
                "kalshi.cfbenchmarks.brti.settlement-window",
                "kalshi.cfbenchmarks.brti.history",
                "kalshi.pyth.btc-usd.value",
            ],
            Self::NbaEspnDaily => &[
                "nba.espn.daily.standings",
                "nba.espn.daily.roster",
                "nba.espn.daily.team-statistics",
                "nba.espn.daily.team-record",
                "nba.espn.daily.athlete-statistics",
                "nba.espn.daily.athlete-gamelog",
                "nba.espn.daily.athlete-splits",
            ],
            Self::NbaEspnGames => &[
                "nba.espn.games.odds",
                "nba.espn.games.predictor",
                "nba.espn.games.summary-context",
            ],
            Self::NbaEspnStatus => &["nba.espn.status.scoreboard", "nba.espn.status.injuries"],
            Self::PolymarketMarket => &[
                "polymarket.btc.market-contracts",
                "polymarket.btc.book-levels",
                "polymarket.btc.quotes",
                "polymarket.btc.trades",
            ],
            Self::PolymarketChainlinkTwap => &["polymarket.rtds.btc-usd.twap-60s"],
            // Built as `nba.sportsdataio.<operation>` over the closed `Operation`
            // enum (sportsdataio_nba.rs `Operation::as_str`); enumerated here.
            Self::SportsDataIoNba => &[
                "nba.sportsdataio.teams",
                "nba.sportsdataio.all_teams",
                "nba.sportsdataio.players_active_basic",
                "nba.sportsdataio.games",
                "nba.sportsdataio.games_by_date_final",
                "nba.sportsdataio.box_score_final",
                "nba.sportsdataio.box_scores_final",
                "nba.sportsdataio.box_scores_delta",
                "nba.sportsdataio.game_odds_by_date",
                "nba.sportsdataio.game_odds_line_movement",
                "nba.sportsdataio.player_game_projection_stats_by_date",
                "nba.sportsdataio.injured_players",
                "nba.sportsdataio.starting_lineups_by_date",
            ],
        }
    }
}

/// The wire transport a [`CaptureStream`] is carried over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamTransport {
    /// A dialled WebSocket read frame by frame.
    Websocket,
    /// A bounded HTTP request/response poll session, no socket.
    HttpPoll,
}

/// Direction or connection boundary represented by a raw record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordKind {
    /// Out-of-band binding established before the socket opened, carrying a
    /// discovery journal. First record of the stream; prediction markets only.
    Configuration,
    /// A connection was established, opening a new connection epoch.
    Connected,
    /// One message received from the venue, verbatim.
    InboundText,
    /// One message sent to the venue, verbatim (subscriptions, keepalives).
    OutboundText,
    /// A request the venue refused with a non-success status, carrying our own
    /// bounded description of the refusal rather than a body.
    ///
    /// The one record kind whose payload we author. It exists because an HTTP
    /// poll feed's refusals are otherwise invisible: a 451 or a 403 returns no
    /// body worth keeping, and writing nothing leaves a capture that cannot be
    /// told apart from a venue with nothing to say. A response body is never
    /// fabricated here — the payload states the status and nothing else.
    Failed,
    /// The active connection ended.
    Disconnected,
}

/// Why a poll produced no usable data, stamped into a [`RecordKind::Failed`]
/// record's `failure` discriminant so a consumer can tell the shapes apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// The request never reached a status: a refused connection, a DNS
    /// blackhole, a timeout, or a body that would not decode.
    Transport,
    /// The venue answered a non-success HTTP status (a 451, 403, 5xx).
    HttpStatus(u16),
    /// The venue answered HTTP 2xx but the body was a served error — a non-zero
    /// application code, an empty result, or a body that would not parse. The
    /// request was served, so it is neither a transport fault nor an HTTP status.
    Served,
}

/// One append-only capture record. `payload` preserves the exact WebSocket text.
///
/// This is the crate's whole input vocabulary: an adapter sees nothing but a
/// stream of these. Prefer the four constructors over building one by hand, and
/// see [`RawRecord::validate`] for the complete rule set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawRecord {
    /// Record format version. Writers stamp [`RAW_RECORD_SCHEMA_VERSION`];
    /// readers also accept 1 and 2.
    pub schema_version: u16,
    /// Provider identity. Must equal `stream.provider()`.
    pub provider: Provider,
    /// Which capture stream produced the record. Every record of one capture
    /// carries the same value.
    pub stream: CaptureStream,
    /// Connection generation, starting at 1 and increasing by one per
    /// reconnect. Must be ≥ 1.
    pub connection_epoch: u64,
    /// Position in the stream, starting at 1. Must be ≥ 1 and strictly
    /// increasing across the whole capture, including across epochs.
    pub ordinal: u64,
    /// Local wall-clock receipt time as an RFC 3339 instant. Copied into every
    /// event this record produces.
    pub received_at: String,
    /// Monotonic nanoseconds since the capture process started — a
    /// process-local origin, not a wall clock, and comparable only within one
    /// capture. Required on schema 2 and 3.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capture_elapsed_ns: Option<u64>,
    /// Monotonic nanoseconds, same origin as `capture_elapsed_ns`, at which the
    /// connection attempt began. Required on, and permitted only on,
    /// [`RecordKind::Connected`], and never greater than `capture_elapsed_ns`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connect_started_elapsed_ns: Option<u64>,
    /// What this record is.
    pub kind: RecordKind,
    /// The provider text, byte for byte. Required on `Configuration`,
    /// `InboundText` and `OutboundText`; forbidden on `Connected` and
    /// `Disconnected`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload: Option<String>,
}

impl RawRecord {
    /// Build the Configuration record that binds a prediction-market adapter to
    /// one market.
    ///
    /// Must be the very first record of the stream: `connection_epoch` must be
    /// 1, `ordinal` must be 1, and it must precede any `Connected` record. Only
    /// Kalshi and Polymarket accept one; every other adapter refuses it.
    ///
    /// `payload` is the venue's discovery journal — build it with
    /// [`KalshiAdapter::configuration_payload`](crate::kalshi::KalshiAdapter::configuration_payload)
    /// or
    /// [`PolymarketAdapter::configuration_payload`](crate::polymarket::PolymarketAdapter::configuration_payload),
    /// never by hand. `received_at` is an RFC 3339 instant and
    /// `capture_elapsed_ns` is monotonic nanoseconds since the capture process
    /// started (pass `0` if you are not measuring).
    pub fn configuration_at(
        stream: CaptureStream,
        connection_epoch: u64,
        ordinal: u64,
        payload: String,
        received_at: String,
        capture_elapsed_ns: u64,
    ) -> Self {
        Self {
            schema_version: RAW_RECORD_SCHEMA_VERSION,
            provider: stream.provider(),
            stream,
            connection_epoch,
            ordinal,
            received_at,
            capture_elapsed_ns: Some(capture_elapsed_ns),
            connect_started_elapsed_ns: None,
            kind: RecordKind::Configuration,
            payload: Some(payload),
        }
    }

    /// Build the record that opens a connection epoch.
    ///
    /// `connection_epoch` must be ≥ 1 and strictly greater than the previous
    /// epoch; `ordinal` must be strictly greater than every earlier ordinal in
    /// the stream. `connect_started_elapsed_ns` is the monotonic reading, on the
    /// same process-local origin as `capture_elapsed_ns`, at which the attempt
    /// began, and must not exceed it; both are nanoseconds since the capture
    /// process started. It is required here and rejected on every other kind.
    ///
    /// Adapters treat this as a hard reset of connection-local state: books are
    /// discarded and await a fresh venue snapshot.
    pub fn connected_at(
        stream: CaptureStream,
        connection_epoch: u64,
        ordinal: u64,
        received_at: String,
        capture_elapsed_ns: u64,
        connect_started_elapsed_ns: u64,
    ) -> Self {
        Self {
            schema_version: RAW_RECORD_SCHEMA_VERSION,
            provider: stream.provider(),
            stream,
            connection_epoch,
            ordinal,
            received_at,
            capture_elapsed_ns: Some(capture_elapsed_ns),
            connect_started_elapsed_ns: Some(connect_started_elapsed_ns),
            kind: RecordKind::Connected,
            payload: None,
        }
    }

    /// Build the record that closes the active connection epoch.
    ///
    /// `connection_epoch` must equal the epoch opened by the matching
    /// [`RawRecord::connected_at`], and `ordinal` must be strictly greater than
    /// every earlier ordinal. Adapters emit a `Disconnected` notice with a
    /// `Reconnect` recovery action.
    pub fn disconnected_at(
        stream: CaptureStream,
        connection_epoch: u64,
        ordinal: u64,
        received_at: String,
        capture_elapsed_ns: u64,
    ) -> Self {
        Self {
            schema_version: RAW_RECORD_SCHEMA_VERSION,
            provider: stream.provider(),
            stream,
            connection_epoch,
            ordinal,
            received_at,
            capture_elapsed_ns: Some(capture_elapsed_ns),
            connect_started_elapsed_ns: None,
            kind: RecordKind::Disconnected,
            payload: None,
        }
    }

    /// Build one message record, in either direction.
    ///
    /// `kind` must be [`RecordKind::InboundText`] (a message received from the
    /// venue) or [`RecordKind::OutboundText`] (one sent to it — subscriptions,
    /// keepalives). `payload` is that message's text byte for byte: never
    /// re-serialize it, since several adapters checksum or hash the exact
    /// bytes.
    ///
    /// `connection_epoch` must equal the active epoch and `ordinal` must be
    /// strictly greater than every earlier ordinal. Note that `ordinal` and
    /// `capture_elapsed_ns` are both `u64` and adjacent in call order: the guard
    /// catches a transposition as "record ordinal must increase strictly", but
    /// it catches it at ingest, not here.
    pub fn text_at(
        stream: CaptureStream,
        connection_epoch: u64,
        ordinal: u64,
        kind: RecordKind,
        payload: String,
        received_at: String,
        capture_elapsed_ns: u64,
    ) -> Self {
        debug_assert!(matches!(
            kind,
            RecordKind::InboundText | RecordKind::OutboundText
        ));
        Self {
            schema_version: RAW_RECORD_SCHEMA_VERSION,
            provider: stream.provider(),
            stream,
            connection_epoch,
            ordinal,
            received_at,
            capture_elapsed_ns: Some(capture_elapsed_ns),
            connect_started_elapsed_ns: None,
            kind,
            payload: Some(payload),
        }
    }

    /// A request that produced no usable response, recorded with what we know
    /// about why.
    ///
    /// `detail` is a short compiled description of what was being fetched (it may
    /// carry a bounded slice of the venue's own error bytes). The payload is our
    /// own JSON with a `failure` discriminant, so nothing here can be mistaken
    /// for a body the venue sent.
    pub fn failed_at(
        stream: CaptureStream,
        connection_epoch: u64,
        ordinal: u64,
        kind: FailureKind,
        detail: &str,
        received_at: String,
        capture_elapsed_ns: u64,
    ) -> Self {
        Self {
            schema_version: RAW_RECORD_SCHEMA_VERSION,
            provider: stream.provider(),
            stream,
            connection_epoch,
            ordinal,
            received_at,
            capture_elapsed_ns: Some(capture_elapsed_ns),
            connect_started_elapsed_ns: None,
            kind: RecordKind::Failed,
            payload: Some(
                match kind {
                    FailureKind::HttpStatus(status) => serde_json::json!({
                        "failure": "http_status",
                        "status": status,
                        "detail": detail
                    }),
                    FailureKind::Transport => {
                        serde_json::json!({"failure": "transport", "detail": detail})
                    }
                    FailureKind::Served => {
                        serde_json::json!({"failure": "served", "detail": detail})
                    }
                }
                .to_string(),
            ),
        }
    }

    /// Check every structural invariant of this record on its own.
    ///
    /// This is what an adapter runs before looking at any payload, and what
    /// [`read_record`] runs on every line it reads. It checks: a supported
    /// `schema_version`; the timing fields permitted by that version, with
    /// `connect_started_elapsed_ns` required on and only on `Connected` and
    /// never exceeding `capture_elapsed_ns`; `connection_epoch` and `ordinal`
    /// both ≥ 1; `provider == stream.provider()`; `received_at` parseable as
    /// RFC 3339; `Configuration` records at schema 3 or later (the kind's
    /// floor; the write version is [`RAW_RECORD_SCHEMA_VERSION`]); and a
    /// bounded payload present on exactly
    /// the text and configuration kinds.
    ///
    /// It cannot check the cross-record rules — strictly increasing ordinals,
    /// epoch ordering, connection lifecycle — which the adapter enforces as
    /// records arrive.
    pub fn validate(&self) -> Result<(), RawRecordError> {
        if !(MIN_RAW_RECORD_SCHEMA_VERSION..=RAW_RECORD_SCHEMA_VERSION)
            .contains(&self.schema_version)
        {
            return Err(unsupported_schema_version(self.schema_version));
        }
        match (
            self.schema_version,
            self.capture_elapsed_ns,
            self.connect_started_elapsed_ns,
            self.kind,
        ) {
            (LEGACY_RAW_RECORD_SCHEMA_VERSION, None, None, _) => {}
            (
                MONOTONIC_RAW_RECORD_SCHEMA_VERSION
                | CONFIGURATION_RAW_RECORD_SCHEMA_VERSION
                | RAW_RECORD_SCHEMA_VERSION,
                Some(elapsed),
                Some(started),
                RecordKind::Connected,
            ) if started <= elapsed => {}
            (
                MONOTONIC_RAW_RECORD_SCHEMA_VERSION
                | CONFIGURATION_RAW_RECORD_SCHEMA_VERSION
                | RAW_RECORD_SCHEMA_VERSION,
                Some(_),
                None,
                kind,
            ) if kind != RecordKind::Connected => {}
            (LEGACY_RAW_RECORD_SCHEMA_VERSION, Some(_), _, _)
            | (LEGACY_RAW_RECORD_SCHEMA_VERSION, _, Some(_), _) => {
                return Err(RawRecordError::Invalid(format!(
                    "schema_version {LEGACY_RAW_RECORD_SCHEMA_VERSION} forbids monotonic timing fields"
                )));
            }
            (
                MONOTONIC_RAW_RECORD_SCHEMA_VERSION
                | CONFIGURATION_RAW_RECORD_SCHEMA_VERSION
                | RAW_RECORD_SCHEMA_VERSION,
                None,
                _,
                _,
            ) => {
                return Err(RawRecordError::Invalid(format!(
                    "schema_version {MONOTONIC_RAW_RECORD_SCHEMA_VERSION} through {RAW_RECORD_SCHEMA_VERSION} require capture_elapsed_ns"
                )));
            }
            (
                MONOTONIC_RAW_RECORD_SCHEMA_VERSION
                | CONFIGURATION_RAW_RECORD_SCHEMA_VERSION
                | RAW_RECORD_SCHEMA_VERSION,
                Some(_),
                None,
                RecordKind::Connected,
            ) => {
                return Err(RawRecordError::Invalid(format!(
                    "schema_version {MONOTONIC_RAW_RECORD_SCHEMA_VERSION} through {RAW_RECORD_SCHEMA_VERSION} connected records require connect_started_elapsed_ns"
                )));
            }
            (
                MONOTONIC_RAW_RECORD_SCHEMA_VERSION
                | CONFIGURATION_RAW_RECORD_SCHEMA_VERSION
                | RAW_RECORD_SCHEMA_VERSION,
                Some(_),
                Some(_),
                kind,
            ) if kind != RecordKind::Connected => {
                return Err(RawRecordError::Invalid(
                    "connect_started_elapsed_ns is only valid on connected records".to_owned(),
                ));
            }
            (
                MONOTONIC_RAW_RECORD_SCHEMA_VERSION
                | CONFIGURATION_RAW_RECORD_SCHEMA_VERSION
                | RAW_RECORD_SCHEMA_VERSION,
                Some(elapsed),
                Some(started),
                RecordKind::Connected,
            ) => {
                return Err(RawRecordError::Invalid(format!(
                    "connect_started_elapsed_ns {started} exceeds capture_elapsed_ns {elapsed}"
                )));
            }
            // Every (version, timing, kind) combination is covered by the arms
            // above, because the version was already narrowed to 1, 2 or 3 at
            // the top of this function. The arm exists only to keep the match
            // total; it names the real residual condition rather than repeating
            // the version check that already passed.
            (schema_version, capture_elapsed_ns, connect_started_elapsed_ns, kind) => {
                return Err(RawRecordError::Invalid(format!(
                    "schema_version {schema_version} does not permit capture_elapsed_ns \
                     {capture_elapsed_ns:?} with connect_started_elapsed_ns \
                     {connect_started_elapsed_ns:?} on a {kind:?} record"
                )));
            }
        }
        if self.connection_epoch == 0 || self.ordinal == 0 {
            return Err(RawRecordError::Invalid(
                "connection_epoch and ordinal must be greater than zero".to_owned(),
            ));
        }
        if self.provider != self.stream.provider() {
            return Err(RawRecordError::Invalid(
                "provider does not match capture stream".to_owned(),
            ));
        }
        DateTime::parse_from_rfc3339(&self.received_at).map_err(|error| {
            RawRecordError::Invalid(format!("received_at must be RFC 3339: {error}"))
        })?;
        if self.kind == RecordKind::Configuration
            && self.schema_version < CONFIGURATION_RAW_RECORD_SCHEMA_VERSION
        {
            return Err(RawRecordError::Invalid(format!(
                "configuration records require schema_version {CONFIGURATION_RAW_RECORD_SCHEMA_VERSION} or newer"
            )));
        }
        // The `Failed` kind entered the vocabulary at version 4; a record
        // claiming it under an older version is describing a shape that did not
        // exist when that version was written.
        if self.kind == RecordKind::Failed
            && self.schema_version < FAILED_KIND_RAW_RECORD_SCHEMA_VERSION
        {
            return Err(RawRecordError::Invalid(format!(
                "failure records require schema_version {FAILED_KIND_RAW_RECORD_SCHEMA_VERSION} or newer"
            )));
        }
        match (self.kind, &self.payload) {
            (
                RecordKind::Configuration | RecordKind::InboundText | RecordKind::OutboundText,
                Some(payload),
            ) => {
                if payload.len() > MAX_MESSAGE_BYTES {
                    return Err(RawRecordError::MessageTooLarge {
                        observed_at_least: payload.len(),
                        maximum: MAX_MESSAGE_BYTES,
                    });
                }
            }
            (RecordKind::Failed, Some(payload)) => {
                if payload.len() > MAX_MESSAGE_BYTES {
                    return Err(RawRecordError::MessageTooLarge {
                        observed_at_least: payload.len(),
                        maximum: MAX_MESSAGE_BYTES,
                    });
                }
            }
            (RecordKind::Connected | RecordKind::Disconnected, None) => {}
            _ => {
                return Err(RawRecordError::Invalid(
                    "text records require payload and connection records forbid it".to_owned(),
                ));
            }
        }
        Ok(())
    }
}

/// Append-only writer that refuses to overwrite an existing capture.
///
/// This is the capture side of the crate, not the parsing side: a library
/// consumer normalizing its own live feed does not need it.
pub struct RawRecorder {
    inner: Arc<Mutex<RecorderInner>>,
    /// The background flusher task. `None` after `finish`, which stops it before
    /// the terminal flush. `Drop` aborts it if `finish` was never called.
    flusher: Option<JoinHandle<()>>,
}

/// The recorder's mutable state, shared between the caller's `append`/`finish`
/// and the background flusher behind one lock so a timer sync can never race a
/// hot-path write.
struct RecorderInner {
    file: BufWriter<File>,
    bytes_written: u64,
    bytes_since_sync: u64,
    maximum_bytes: u64,
}

impl RawRecorder {
    /// Create a new owner-only capture file at `path`, bounded to
    /// `maximum_bytes`. Fails if the path already exists: a capture is never
    /// overwritten. The recorder owns a background flusher that syncs unsynced
    /// bytes every [`CAPTURE_SYNC_INTERVAL`], so a low-volume or long-cadence
    /// capture (a quiet WebSocket, a 300 s REST poll) reaches disk on the
    /// interval without any capture loop knowing about the sync.
    pub async fn create(path: &Path, maximum_bytes: u64) -> Result<Self, RawRecordError> {
        Self::create_with_flush_interval(path, maximum_bytes, CAPTURE_SYNC_INTERVAL).await
    }

    async fn create_with_flush_interval(
        path: &Path,
        maximum_bytes: u64,
        flush_interval: Duration,
    ) -> Result<Self, RawRecordError> {
        if maximum_bytes == 0 {
            return Err(RawRecordError::Invalid(
                "capture maximum_bytes must be greater than zero".to_owned(),
            ));
        }
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            options.mode(0o600);
        }
        let file = options.open(path).await.map_err(RawRecordError::Io)?;
        let inner = Arc::new(Mutex::new(RecorderInner {
            file: BufWriter::with_capacity(CAPTURE_BUFFER_BYTES, file),
            bytes_written: 0,
            bytes_since_sync: 0,
            maximum_bytes,
        }));
        let flusher = spawn_flusher(Arc::clone(&inner), flush_interval);
        Ok(Self {
            inner,
            flusher: Some(flusher),
        })
    }

    /// Validate and append one record. On reaching the byte limit the complete
    /// prefix already accepted is flushed and synced before the error returns.
    pub async fn append(&mut self, record: &RawRecord) -> Result<(), RawRecordError> {
        self.inner.lock().await.append(record).await
    }

    /// Stop the background flusher and flush the terminal prefix, returning the
    /// total bytes written.
    pub async fn finish(mut self) -> Result<u64, RawRecordError> {
        if let Some(flusher) = self.flusher.take() {
            flusher.abort();
        }
        let mut inner = self.inner.lock().await;
        inner.flush_and_sync().await?;
        Ok(inner.bytes_written)
    }
}

impl Drop for RawRecorder {
    fn drop(&mut self) {
        // Stop the flusher on drop even when `finish` was never called (an error
        // path). Buffered-but-unsynced bytes are lost without `finish`, exactly
        // as before this change: nothing flushes in `Drop`.
        if let Some(flusher) = self.flusher.take() {
            flusher.abort();
        }
    }
}

/// Spawn the background flusher: every `interval`, sync any unsynced bytes under
/// the recorder's lock. A background sync error is not fatal and not lost — the
/// buffered bytes stay pending and the next `append` or `finish` retries and
/// surfaces it.
fn spawn_flusher(inner: Arc<Mutex<RecorderInner>>, interval: Duration) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        // Consume the immediate first tick so the first real sync is one
        // interval in, not at t=0 when there is nothing to sync.
        tick.tick().await;
        loop {
            tick.tick().await;
            let mut guard = inner.lock().await;
            if guard.bytes_since_sync > 0 {
                let _ = guard.flush_and_sync().await;
            }
        }
    })
}

impl RecorderInner {
    async fn append(&mut self, record: &RawRecord) -> Result<(), RawRecordError> {
        record.validate()?;
        let mut bytes = serde_json::to_vec(record).map_err(RawRecordError::Json)?;
        let encoded_line_bytes = bytes
            .len()
            .checked_add(1)
            .ok_or(RawRecordError::ByteCountOverflow)?;
        if encoded_line_bytes > MAX_REPLAY_LINE_BYTES {
            return Err(RawRecordError::LineTooLarge {
                maximum: MAX_REPLAY_LINE_BYTES,
            });
        }
        bytes.push(b'\n');
        let line_bytes =
            u64::try_from(bytes.len()).map_err(|_| RawRecordError::ByteCountOverflow)?;
        let next_total = self
            .bytes_written
            .checked_add(line_bytes)
            .ok_or(RawRecordError::ByteCountOverflow)?;
        if next_total > self.maximum_bytes {
            // Preserve the complete prefix already accepted by the recorder. This
            // runs only on the terminal limit path, never on the capture hot path.
            self.flush_and_sync().await?;
            return Err(RawRecordError::CaptureSizeLimit {
                attempted: next_total,
                maximum: self.maximum_bytes,
            });
        }
        self.file
            .write_all(&bytes)
            .await
            .map_err(RawRecordError::Io)?;
        self.bytes_written = next_total;
        self.bytes_since_sync = self
            .bytes_since_sync
            .checked_add(line_bytes)
            .ok_or(RawRecordError::ByteCountOverflow)?;
        // The byte-volume sync stays on the hot path; the time-based sync is the
        // background flusher's job now.
        if self.bytes_since_sync >= CAPTURE_SYNC_BYTES {
            self.flush_and_sync().await?;
        }
        Ok(())
    }

    async fn flush_and_sync(&mut self) -> Result<(), RawRecordError> {
        self.file.flush().await.map_err(RawRecordError::Io)?;
        self.file
            .get_ref()
            .sync_data()
            .await
            .map_err(RawRecordError::Io)?;
        self.bytes_since_sync = 0;
        Ok(())
    }
}

/// Read one bounded JSONL record without allowing an unbounded `read_line` allocation.
pub fn read_record<R: BufRead>(reader: &mut R) -> Result<Option<RawRecord>, RawRecordError> {
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf().map_err(RawRecordError::Io)?;
        if available.is_empty() {
            if line.is_empty() {
                return Ok(None);
            }
            break;
        }

        let newline = available.iter().position(|byte| *byte == b'\n');
        let take = newline.map_or(available.len(), |index| index + 1);
        let next_line_length = line
            .len()
            .checked_add(take)
            .ok_or(RawRecordError::ByteCountOverflow)?;
        if next_line_length > MAX_REPLAY_LINE_BYTES {
            return Err(RawRecordError::LineTooLarge {
                maximum: MAX_REPLAY_LINE_BYTES,
            });
        }
        line.extend_from_slice(&available[..take]);
        reader.consume(take);
        if newline.is_some() {
            break;
        }
    }

    if line.last() == Some(&b'\n') {
        line.pop();
    }
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    if line.is_empty() {
        return Err(RawRecordError::Invalid(
            "empty JSONL records are not allowed".to_owned(),
        ));
    }

    // The schema version is read BEFORE the record. A newer capture may carry a
    // record kind or a field this build has no variant for, and serde would
    // report that as `unknown variant ...` from the middle of a replay — a JSON
    // error where the operator needs a version answer. Probing first means an
    // out-of-range capture always gets the version triage, whatever new shape it
    // happens to contain.
    #[derive(Deserialize)]
    struct SchemaVersionProbe {
        schema_version: u16,
    }
    let probe: SchemaVersionProbe = serde_json::from_slice(&line).map_err(RawRecordError::Json)?;
    if !(MIN_RAW_RECORD_SCHEMA_VERSION..=RAW_RECORD_SCHEMA_VERSION).contains(&probe.schema_version)
    {
        return Err(unsupported_schema_version(probe.schema_version));
    }
    let record: RawRecord = serde_json::from_slice(&line).map_err(RawRecordError::Json)?;
    record.validate()?;
    Ok(Some(record))
}

/// The one wording for a capture this build cannot read.
fn unsupported_schema_version(observed: u16) -> RawRecordError {
    RawRecordError::Invalid(format!(
        "unsupported schema_version {observed}; expected {MIN_RAW_RECORD_SCHEMA_VERSION} through {RAW_RECORD_SCHEMA_VERSION}"
    ))
}

/// Current wall-clock time as an RFC 3339 instant with microsecond precision,
/// in the form `received_at` requires.
pub fn now_timestamp() -> String {
    DateTime::<Utc>::from(SystemTime::now()).to_rfc3339_opts(SecondsFormat::Micros, true)
}

/// Why a capture record could not be written, read, or validated.
#[derive(Debug, Error)]
pub enum RawRecordError {
    /// Filesystem I/O failed.
    #[error("capture I/O failed: {0}")]
    Io(std::io::Error),
    /// A record could not be serialized or deserialized.
    #[error("capture JSON failed: {0}")]
    Json(serde_json::Error),
    /// The record broke one of [`RawRecord::validate`]'s rules.
    #[error("invalid capture record: {0}")]
    Invalid(String),
    /// The payload exceeds [`MAX_MESSAGE_BYTES`](crate::MAX_MESSAGE_BYTES).
    /// The reported size is a LOWER BOUND, not always the payload's length:
    /// the streaming reader in [`crate::replay`] refuses as soon as the bound
    /// is crossed and never reads the rest of the line, so all it can honestly
    /// report is how far it had got. `RawRecord::validate` does know the exact
    /// length, and an exact length is also a lower bound on itself.
    #[error(
        "message exceeds the {maximum} byte maximum; at least {observed_at_least} bytes observed"
    )]
    MessageTooLarge {
        /// Bytes of payload seen before the refusal — at least this many exist.
        observed_at_least: usize,
        /// Permitted maximum.
        maximum: usize,
    },
    /// The encoded JSONL line exceeds
    /// [`MAX_REPLAY_LINE_BYTES`](crate::MAX_REPLAY_LINE_BYTES). Raised before
    /// any unbounded allocation.
    #[error("JSONL record exceeds {maximum} bytes")]
    LineTooLarge {
        /// Permitted maximum.
        maximum: usize,
    },
    /// Appending would take the capture past its configured size limit. The
    /// complete prefix is preserved.
    #[error("capture would reach {attempted} bytes; maximum is {maximum}")]
    CaptureSizeLimit {
        /// Total the capture would have reached.
        attempted: u64,
        /// Configured maximum.
        maximum: u64,
    },
    /// A byte counter overflowed. The recorder refuses to wrap.
    #[error("capture byte count overflowed")]
    ByteCountOverflow,
}

#[cfg(test)]
mod tests {
    use super::{FailureKind, MIN_RAW_RECORD_SCHEMA_VERSION, RAW_RECORD_SCHEMA_VERSION};

    /// The `Failed` kind moved the write version 3 → 4, and the read range only
    /// grew. Both halves matter: a retained v1/v2/v3 capture must still replay,
    /// and a capture this build cannot read must say so as a VERSION problem.
    #[test]
    fn the_failed_kind_moved_the_write_version_and_widened_nothing_else() {
        use std::io::Cursor;

        assert_eq!(RAW_RECORD_SCHEMA_VERSION, 4);
        assert_eq!(MIN_RAW_RECORD_SCHEMA_VERSION, 1);

        // A v3 record still reads, byte for byte, exactly as it did before.
        let v3 = r#"{"schema_version":3,"provider":"coinbase_exchange","stream":"coinbase_core","connection_epoch":1,"ordinal":1,"received_at":"2026-09-03T04:00:01Z","capture_elapsed_ns":2000,"kind":"inbound_text","payload":"{}"}"#;
        let record = read_record(&mut Cursor::new(format!("{v3}\n")))
            .expect("a v3 record still reads")
            .expect("one record");
        assert_eq!(record.schema_version, 3);
        assert_eq!(record.kind, RecordKind::InboundText);
        // And re-serializes to the same bytes: reading an old capture must not
        // silently rewrite its version.
        assert_eq!(
            serde_json::to_string(&record).expect("re-serializes"),
            v3,
            "a v3 record round-trips unchanged"
        );

        // Every version in the range validates; nothing outside it does.
        for version in [1_u16, 2, 3, 4] {
            let mut record = RawRecord::text_at(
                CaptureStream::CoinbaseCore,
                1,
                1,
                RecordKind::InboundText,
                "{}".to_owned(),
                "2026-09-03T04:00:01Z".to_owned(),
                2_000,
            );
            record.schema_version = version;
            if version == 1 {
                record.capture_elapsed_ns = None;
            }
            record
                .validate()
                .unwrap_or_else(|error| panic!("schema_version {version} must validate: {error}"));
        }

        // A capture from a NEWER build is a version answer, not a JSON error —
        // even when the thing this build cannot parse is the record kind
        // itself. This is the whole reason the version moved: before the probe,
        // an unknown kind surfaced as `unknown variant` from the middle of a
        // replay.
        let future = r#"{"schema_version":5,"provider":"binance_usdm","stream":"binance_usdm_rest","connection_epoch":1,"ordinal":1,"received_at":"2026-09-03T04:00:01Z","capture_elapsed_ns":2000,"connect_started_elapsed_ns":null,"kind":"some_future_kind","payload":"{}"}"#;
        let error = read_record(&mut Cursor::new(format!("{future}\n")))
            .expect_err("a newer capture is refused");
        assert!(
            matches!(error, RawRecordError::Invalid(ref message) if message.contains("unsupported schema_version 5")),
            "expected the version triage, got {error:?}"
        );
        assert!(
            !matches!(error, RawRecordError::Json(_)),
            "a version problem must never surface as a JSON parse error"
        );
        // Version 0 is refused by the same triage.
        let ancient = future.replace("\"schema_version\":5", "\"schema_version\":0");
        assert!(matches!(
            read_record(&mut Cursor::new(format!("{ancient}\n"))),
            Err(RawRecordError::Invalid(_))
        ));
    }

    /// A `Failed` record is only meaningful from version 4 on: before it, the
    /// kind did not exist.
    #[test]
    fn a_failure_record_requires_the_version_that_introduced_it() {
        let mut record = RawRecord::failed_at(
            CaptureStream::BinanceUsdmRest,
            1,
            1,
            FailureKind::HttpStatus(451),
            "open_interest",
            "2026-09-03T04:00:01Z".to_owned(),
            2_000,
        );
        assert_eq!(record.schema_version, RAW_RECORD_SCHEMA_VERSION);
        record.validate().expect("a v4 failure record is valid");
        // Versions 2 and 3 carry the same timing fields, so the version check
        // is the one that fires. Version 1 is refused earlier and for a
        // different reason — it forbids those fields outright — so it is
        // asserted as a refusal rather than for this message.
        for version in [2_u16, 3] {
            record.schema_version = version;
            let error = record
                .validate()
                .expect_err("the kind did not exist at this version");
            assert!(
                error.to_string().contains("failure records require"),
                "schema_version {version}: {error}"
            );
        }
        record.schema_version = 1;
        assert!(record.validate().is_err(), "version 1 refuses it too");
    }
    use std::io::Cursor;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use super::{
        CaptureStream, Provider, RawRecord, RawRecordError, RawRecorder, RecordKind, read_record,
    };
    use crate::MAX_REPLAY_LINE_BYTES;

    #[test]
    fn round_trip_preserves_exact_payload_text() {
        let record = RawRecord {
            schema_version: 1,
            provider: Provider::KrakenSpot,
            stream: CaptureStream::KrakenCore,
            connection_epoch: 1,
            ordinal: 2,
            received_at: "2026-08-12T12:00:00.123456Z".to_owned(),
            capture_elapsed_ns: None,
            connect_started_elapsed_ns: None,
            kind: RecordKind::InboundText,
            payload: Some(r#"{"price":1.2300}"#.to_owned()),
        };
        let mut bytes = serde_json::to_vec(&record).expect("record serializes");
        bytes.push(b'\n');
        let parsed = read_record(&mut Cursor::new(bytes))
            .expect("record parses")
            .expect("record exists");
        assert_eq!(parsed, record);
    }

    #[test]
    fn oversized_line_is_rejected_before_json_parsing() {
        let bytes = vec![b'x'; MAX_REPLAY_LINE_BYTES + 1];
        let error = read_record(&mut Cursor::new(bytes)).expect_err("line must be bounded");
        assert!(matches!(error, RawRecordError::LineTooLarge { .. }));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn recorder_preserves_complete_prefix_at_byte_limit() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is after Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "btc-feeds-raw-recorder-{}-{unique}.jsonl",
            std::process::id()
        ));
        let first = RawRecord::connected_at(
            CaptureStream::CoinbaseCore,
            1,
            1,
            "2026-08-12T12:00:00Z".to_owned(),
            2,
            1,
        );
        let mut expected = serde_json::to_vec(&first).expect("record serializes");
        expected.push(b'\n');
        let maximum_bytes = u64::try_from(expected.len()).expect("test record length fits u64");
        let mut recorder = RawRecorder::create(&path, maximum_bytes)
            .await
            .expect("new capture file is created");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mode = std::fs::metadata(&path)
                .expect("capture metadata is readable")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
        recorder
            .append(&first)
            .await
            .expect("first record fits exactly");
        let second = RawRecord::disconnected_at(
            CaptureStream::CoinbaseCore,
            1,
            2,
            "2026-08-12T12:00:01Z".to_owned(),
            3,
        );

        let error = recorder
            .append(&second)
            .await
            .expect_err("second record must exceed the exact capture limit");
        assert!(matches!(error, RawRecordError::CaptureSizeLimit { .. }));
        drop(recorder);
        assert_eq!(
            std::fs::read(&path).expect("flushed capture prefix is readable"),
            expected
        );
        std::fs::remove_file(path).expect("test capture is removed");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn recorder_background_flusher_persists_between_records_without_finish() {
        // One record then idle: the recorder's own background flusher syncs it to
        // disk on the interval, with no following append and no `finish`. This is
        // the low-volume/long-cadence case (a quiet socket, a 300 s REST poll),
        // and it holds regardless of which capture loop owns the recorder because
        // no loop touches the flush. A short interval keeps the test fast.
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is after Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "btc-feeds-raw-recorder-bg-flush-{}-{unique}.jsonl",
            std::process::id()
        ));
        let record = RawRecord::connected_at(
            CaptureStream::CoinbaseCore,
            1,
            1,
            "2026-08-13T12:00:00Z".to_owned(),
            2,
            1,
        );
        let mut expected = serde_json::to_vec(&record).expect("record serializes");
        expected.push(b'\n');
        let mut recorder =
            RawRecorder::create_with_flush_interval(&path, 1024 * 1024, Duration::from_millis(20))
                .await
                .expect("new capture file is created");
        recorder.append(&record).await.expect("record is buffered");
        // Wait past several flush intervals; the background task syncs the record
        // to disk without any further append or `finish`.
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(
            std::fs::read(&path).expect("flushed record is readable"),
            expected,
            "the background flusher persists an idle record on its interval"
        );
        // `finish` still writes the terminal record exactly once and stops the
        // flusher.
        let bytes = recorder.finish().await.expect("recorder finalizes");
        assert_eq!(
            bytes,
            u64::try_from(expected.len()).expect("length fits u64")
        );
        assert_eq!(
            std::fs::read(&path).expect("flushed record is readable"),
            expected
        );
        std::fs::remove_file(path).expect("test capture is removed");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn recorder_finish_persists_a_sub_buffer_record() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is after Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "btc-feeds-raw-recorder-finish-{}-{unique}.jsonl",
            std::process::id()
        ));
        let record = RawRecord::connected_at(
            CaptureStream::CoinbaseCore,
            1,
            1,
            "2026-08-13T12:00:00Z".to_owned(),
            2,
            1,
        );
        let mut expected = serde_json::to_vec(&record).expect("record serializes");
        expected.push(b'\n');
        let mut recorder = RawRecorder::create(&path, 1024 * 1024)
            .await
            .expect("new capture file is created");
        recorder.append(&record).await.expect("record is buffered");
        let bytes = recorder.finish().await.expect("recorder finalizes");
        assert_eq!(
            bytes,
            u64::try_from(expected.len()).expect("length fits u64")
        );
        assert_eq!(std::fs::read(&path).expect("capture is readable"), expected);
        std::fs::remove_file(path).expect("test capture is removed");
    }
}
