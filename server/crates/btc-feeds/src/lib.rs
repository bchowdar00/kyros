//! Provider message parsing and normalization for BTC market data.
//!
//! `btc-feeds` turns raw provider messages into normalized
//! [`SourceEvent`](signals_core::SourceEvent)s. Eleven BTC adapters cover
//! Coinbase Exchange, Kraken spot (aggregated book and level-3), Kalshi (the
//! market socket and the authenticated index socket carrying CF Benchmarks
//! BRTI and the Pyth relay), Polymarket (CLOB market and the RTDS Chainlink
//! 60-second TWAP), a Chainlink
//! on-chain aggregator, Binance (spot, and the USD-M perpetual's two sockets
//! and REST pollers), Bybit (spot and USDT-perpetual) and a Pyth Network Hermes
//! BTC/USD price feed (SSE stream and REST anchor, refuse-closed behind its
//! key). Four
//! further capture-only adapters cover NBA data (three ESPN crawls and
//! SportsDataIO); they share the same machinery but feed no signal processor.
//!
//! The crate also contains a live runner and a capture recorder. Those are the
//! producer's own transport, not part of the parsing library — a consumer
//! bringing its own ingestion only needs what this page describes.
//!
//! # The pipeline
//!
//! ```text
//! raw provider message  ──►  RawRecord  ──►  FeedAdapter::ingest  ──►  IngestOutcome
//!  (WebSocket text, or                                                   ├─ events: Vec<SourceEvent>
//!   one REST body)                                                       └─ notices: Vec<FeedNotice>
//! ```
//!
//! You wrap each message as a [`RawRecord`] and feed the records, in order, to
//! an adapter. [`ReplayAdapter`] is the one type that dispatches every venue:
//! construct it from a [`CaptureStream`] and you never name a venue adapter.
//! Each call returns an [`IngestOutcome`] holding zero or more normalized
//! events plus zero or more [`FeedNotice`]s — operational state as data, not as
//! logging. Errors are reserved for input the adapter cannot use; an expected
//! venue condition (a gap, a duplicate, an unsupported message type) is a
//! notice.
//!
//! Everything on this path is deterministic: no clock is read, nothing is
//! random, and no network call is made. The same records always produce the same
//! events.
//!
//! # Worked example
//!
//! ```
//! use btc_feeds::{CaptureStream, FeedAdapter, RawRecord, RecordKind, ReplayAdapter};
//! use btc_feeds::replay::KrakenBookDepths;
//!
//! // One adapter for the stream; the publisher epoch is copied into every event.
//! let mut adapter = ReplayAdapter::new(
//!     CaptureStream::CoinbaseCore,
//!     "example-epoch",
//!     KrakenBookDepths::default(),
//! )?;
//!
//! // Every stream opens with a Connected record. Ordinals start at 1.
//! adapter.ingest(&RawRecord::connected_at(
//!     CaptureStream::CoinbaseCore,
//!     1,
//!     1,
//!     "2026-09-02T12:00:00Z".to_owned(),
//!     1_000,
//!     0,
//! ))?;
//!
//! // Then the venue text, byte for byte as it came off the socket.
//! let message = r#"{"type":"ticker","product_id":"BTC-USD","sequence":91234567,
//!     "price":"64250.11","open_24h":"63000.00","volume_24h":"1234.5678",
//!     "low_24h":"62500.00","high_24h":"64900.00","volume_30d":"98765.4321",
//!     "best_bid":"64250.10","best_bid_size":"0.51000000",
//!     "best_ask":"64250.12","best_ask_size":"0.32000000",
//!     "side":"buy","time":"2026-09-02T12:00:01.250000Z",
//!     "trade_id":55501,"last_size":"0.00100000"}"#;
//!
//! let outcome = adapter.ingest(&RawRecord::text_at(
//!     CaptureStream::CoinbaseCore,
//!     1,
//!     2,
//!     RecordKind::InboundText,
//!     message.to_owned(),
//!     "2026-09-02T12:00:01.310000Z".to_owned(),
//!     2_000,
//! ))?;
//!
//! let event = &outcome.events[0];
//! assert_eq!(event.source_id, "coinbase.exchange.btc-usd.ticker");
//! assert_eq!(event.publisher_epoch, "example-epoch");
//! assert_eq!(event.sequence, 0);
//! assert_eq!(event.event_time, "2026-09-02T12:00:01.250000Z");
//! assert_eq!(event.received_at, "2026-09-02T12:00:01.310000Z");
//! // Decimal text is preserved exactly, never parsed into a float.
//! assert_eq!(event.values["price"], serde_json::json!("64250.11"));
//! assert_eq!(event.values["source_sequence"], serde_json::json!(91_234_567_u64));
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! # Record invariants
//!
//! [`RawRecord::validate`] enforces these, and the adapter enforces the
//! cross-record ones as records arrive:
//!
//! * `connection_epoch` and `ordinal` are both ≥ 1.
//! * `ordinal` increases **strictly** across the whole stream, including across
//!   reconnects. It is a stream position, not a per-epoch counter.
//! * `connection_epoch` starts at 1 and increases by one per reconnect. Records
//!   must fall inside the epoch opened by a [`RecordKind::Connected`] record.
//! * `connect_started_elapsed_ns` is required on, and permitted only on,
//!   `Connected` records, and never exceeds `capture_elapsed_ns`.
//! * `capture_elapsed_ns` is **monotonic nanoseconds since the producing
//!   process started** — a process-local origin, not a wall clock, comparable
//!   only within one stream. `received_at` is the wall clock. Pass `0` for
//!   `capture_elapsed_ns` if you are not measuring.
//! * A `Configuration` record requires `schema_version` 3 or later (the kind
//!   arrived with schema 3; the current write version is
//!   [`RAW_RECORD_SCHEMA_VERSION`]), must be the stream's first record at
//!   epoch 1 and ordinal 1, and is accepted only by the Kalshi and Polymarket
//!   market adapters.
//! * `provider` must equal `stream.provider()`. Payloads are required on the
//!   text and configuration kinds and forbidden on the connection kinds.
//!
//! # Prediction markets need a Configuration record
//!
//! Kalshi and Polymarket bind to one exact market, discovered over REST before
//! the socket opens. Build that record's payload with
//! [`KalshiAdapter::configuration_payload`](kalshi::KalshiAdapter::configuration_payload)
//! or
//! [`PolymarketAdapter::configuration_payload`](polymarket::PolymarketAdapter::configuration_payload),
//! wrap it with [`RawRecord::configuration_at`], and ingest it first. Both
//! document the journal format, which embeds the REST body and a SHA-256 over
//! it so the binding is reproducible without the network.
//!
//! # Recovery
//!
//! A [`FeedNotice`] carrying a [`RecoveryAction`] means the adapter has stopped
//! publishing the affected surface and will not resume until the named work is
//! done. The one action with an in-crate completion path is
//! [`RecoveryAction::BackfillCoinbaseTrades`]: fetch the range from Coinbase
//! REST and hand it back to
//! [`CoinbaseAdapter::apply_trade_backfill`](coinbase::CoinbaseAdapter::apply_trade_backfill).
//! Ignoring it does not degrade Coinbase trade output, it stops it.
//! [`RecoveryAction::BackfillKalshiBrti`] is the second: fetch the CF
//! Benchmarks index range from Kalshi's authenticated REST passthrough and hand
//! it back to
//! [`KalshiIndexAdapter::apply_history_backfill`](kalshi_index::KalshiIndexAdapter::apply_history_backfill).
//! The quarter-hour settlement window stays paused until a response covers the
//! whole interval; a partial response is a repair failure, never a partial apply.
//!
//! Streams that can be incomplete expose a `finish()` to call after the last
//! record — [`ReplayAdapter::finish`] dispatches it.
//!
//! # Output vocabulary
//!
//! Event `values` are venue-shaped by design, so the same concept is spelled
//! differently across venues. The crate does not paper over this today; these
//! are the differences a consumer joining across venues has to know about.
//!
//! **Venue sequence numbers.** Each name is a different venue concept, not a
//! synonym:
//!
//! | Field | Emitted by | Venue meaning |
//! | --- | --- | --- |
//! | `source_sequence` | Coinbase ticker | Coinbase product-wide message sequence |
//! | `upstream_sequence` | Kalshi book/quotes/trades | Kalshi per-market message sequence |
//! | `upstream_sequence` | Kalshi index channels | Kalshi per-subscription `seq`, one cursor per `sid` |
//! | `cross_sequence` | Bybit trades, book-levels | Bybit cross-sequence, shared across topics |
//! | `update_id` | Bybit book-levels | Bybit per-topic book update id |
//! | `last_update_id` | Binance book-levels (spot and USD-M) | Binance depth-snapshot update id |
//! | `order_book_update_id` | Binance spot book-ticker | Binance order-book update id of the displayed best quote |
//! | `poll_sequence` | Binance USD-M open-interest | our own poll ordinal, not a venue concept (ruling E7) |
//!
//! Coinbase trades, Kraken and Binance agg-trades instead carry venue ids
//! (`trade_id`, `agg_trade_id`) or no venue sequence at all. Every event always
//! carries the crate's own per-source `sequence`, which starts at 0 and
//! increases by exactly one per event for the life of the adapter — that is the
//! ordering to rely on.
//!
//! **`event_time_basis`.** Where present it says whether `event_time` came from
//! the venue or from local receipt, but there are two vocabularies and four
//! adapters emit none:
//!
//! | Adapter | `event_time_basis` |
//! | --- | --- |
//! | Coinbase, Binance, Bybit, Chainlink | `"source"` / `"receipt"` |
//! | Kalshi | `"provider"` / `"local_receipt"` |
//! | Kraken, Kraken level-3, Polymarket, Polymarket TWAP | not emitted |
//!
//! A consumer branching on `"receipt"` alone silently treats every Kalshi book
//! event as venue-timed. Unifying the two vocabularies would change the frozen
//! source contracts and break replay of existing evidence, so it is an open
//! owner decision rather than a pending code change.
//!
//! Where the field is absent, the basis is still not uniform. Two Kraken
//! reference surfaces put **local receipt time** in `event_time` —
//! `kraken.spot.status` and both `*.instrument` sources — while
//! `kraken.spot.*.ticker`, `*.trades`, `*.book-levels`, `*.ohlc-1m` and
//! `kraken.spot.btc-usd.book-orders` carry venue time. Computing feed lag as
//! `now - event_time` over a Kraken stream therefore mixes a true lag with a
//! near-zero one, from one adapter, with nothing in the payload to separate
//! them. Use the source id.
//!
//! One more asymmetry: Binance agg-trades spell the size field `quantity` where
//! Binance book-levels and every other venue spell it `size`.
//!
//! # Which source ids have a field contract
//!
//! Fifty-two V1 source definitions live in `definitions/btc/sources/` and are
//! the field-level contract for what an event's `values` may contain. They
//! cover Coinbase (including the USDT-USD reference ticker of ruling E47),
//! Kraken (both streams), Kalshi (the market channels and the
//! four index channels), both Polymarket streams, the Chainlink on-chain
//! aggregator, Bybit (spot, linear WebSocket and linear REST), Binance spot and
//! USD-M, and — since 2026-09-03 — the Pyth Hermes BTC/USD price. Fifty-one of the
//! fifty-two are defined
//! stream sources; the nine Bybit definitions and the ten Binance definitions
//! (groups A4 and A3) are published but rights-gated behind
//! `SIGNALS_ENABLED_VENUES` (R7); `kalshi.cfbenchmarks.brti.history` is the
//! repair-row contract of
//! the CF Benchmarks REST passthrough, which emits no event and appears in no
//! package closure or delivery catalogue (ruling R2).
//!
//! Of the four Kalshi index sources, `kalshi.cfbenchmarks.brti.value` and
//! `…brti.value-5hz` carry **observed** wire contracts (rulings E23 and E25,
//! the 2026-09-03 Tokyo hour). The 5 Hz channel is intermittently available —
//! refused with `error` code 8 on three of that hour's four connections and
//! accepted on the fourth — so its subscription is tolerant and its surface
//! reports `unavailable` rather than failing anything.
//! `…brti.settlement-window` rides an observed frame, and
//! `kalshi.pyth.btc-usd.value` has never delivered a frame at all and is
//! refuse-closed until one is captured.
//!
//! Every NBA source id has **no** definition file. Those streams are
//! rights-gated
//! capture-only candidates: their events are well-formed and deterministic, but
//! nothing pins their field set, and no signal processor consumes them. Treat
//! the presence of a definition file as the boundary between a contract and a
//! candidate. [`chainlink`] and [`bybit`] crossed that boundary on 2026-09-03
//! (A5 and A4): the Chainlink on-chain round source and the nine Bybit
//! contracts (spot, linear WebSocket and linear REST) are now approved sources,
//! though no signal processor consumes them yet.
//!
//! [`binance`] and [`binance_usdm`] are the exception, and a published contract
//! there is not a published feed. Binance is still rights-gated: what holds it
//! off the wire is the `SIGNALS_ENABLED_VENUES` allow-list, not a missing file
//! (ruling R7), and the capture path refuses to dial unless `binance` is
//! listed. The three `binance.spot.*` contracts describe what [`binance`]
//! already emits and are validated against it on every replay run. The
//! `binance.usdm.*` contracts split on their evidence: `book-levels`,
//! `open-interest`, `open-interest-hist` and `funding-info` are **observed**
//! against captured hours (their tables carry the provenance; the status is
//! `approved` — built, tested and ready to run — like every source, ruling E45
//! superseding E10), while `mark-price`,
//! `agg-trades` and
//! `liquidations` ride the `market` connection, which no available host can
//! reach (ruling E30) — they stay documented-unobserved (stated in each
//! description), and the live runner refuses every USD-M stream outright.
//!
//! # Language-neutral entry point
//!
//! `btc-feeds normalize` does everything above from the command line: raw
//! provider messages on stdin, one per line; `SourceEvent` JSONL on stdout;
//! `FeedNotice` JSON on stderr. See `crates/btc-feeds/README.md`.

#![warn(missing_docs)]

pub mod binance;
pub mod binance_usdm;
pub mod binance_usdm_rest;
pub mod bybit;
pub mod capture;
pub mod chainlink;
pub mod coinbase;
pub mod coinbase_rest;
pub mod kalshi;
mod kalshi_auth;
pub mod kalshi_index;
pub mod kraken;
// `kraken_auth` is public and `kalshi_auth` is private because `btc-signals`
// maps `KrakenAuthError` in its process exit-code taxonomy; nothing outside the
// workspace needs either. Kalshi's equivalent error is re-exported below
// instead of publishing the module.
pub mod kraken_auth;
pub mod kraken_l3;
pub mod kraken_rest;
pub mod latency;
pub mod live;
pub mod nba_espn;
pub mod nba_espn_daily;
pub mod nba_espn_games;
pub mod normalization;
pub mod polymarket;
pub mod polymarket_twap;
pub mod prediction_live;
pub mod pyth_hermes;
pub mod pyth_hermes_auth;
pub mod raw;
pub mod replay;
pub mod sportsdataio_auth;
pub mod sportsdataio_nba;

mod adapter;
mod counters;
mod decimal;
mod event_window;
mod secure_http;
mod websocket_tls;

pub use adapter::{
    AdapterError, FeedAdapter, FeedNotice, FeedNoticeCode, IngestOutcome, RecoveryAction,
    SubscriptionOutcome,
};
pub use binance::BinanceAdapter;
pub use binance_usdm::{BinanceUsdmAdapter, BinanceUsdmConnection};
pub use binance_usdm_rest::{BinanceUsdmRestEndpoint, RateLimitBackoff, RestBudget};
pub use bybit::{BybitAdapter, BybitMarket};
pub use chainlink::ChainlinkAdapter;
pub use coinbase::{CoinbaseAdapter, CoinbaseRestTrade};
pub use kalshi::{KalshiAdapter, KalshiDiscovery, KalshiDiscoveryError};
pub use kalshi_auth::KalshiAuthError;
pub use kalshi_index::{
    KalshiBrtiHistoryRow, KalshiIndexAdapter, KalshiPassthroughClient, KalshiPassthroughError,
};
pub use kraken::KrakenAdapter;
pub use kraken_l3::KrakenLevel3Adapter;
pub use normalization::{NormalizationError, NormalizationReport};
pub use polymarket::{PolymarketAdapter, PolymarketDiscovery, PolymarketDiscoveryError};
pub use polymarket_twap::PolymarketTwapAdapter;
pub use pyth_hermes::{PythHermesAdapter, PythHermesConnection};
pub use pyth_hermes_auth::{PythHermesAuthError, PythHermesCredential};
pub use raw::{
    CaptureStream, FailureKind, Provider, RAW_RECORD_SCHEMA_VERSION, RawRecord, RawRecordError,
    RawRecorder, RecordKind, StreamTransport, read_record,
};
pub use replay::{
    KrakenBookDepths, ReplayAdapter, ReplayConfiguration, ReplayError, ReplayReport, ReplaySession,
};
pub use sportsdataio_auth::{SportsDataIoAuthError, SportsDataIoCredential, redact_secret};

/// Thin crate-internal parse entries exposed only for fuzzing.
#[cfg(feature = "fuzzing")]
#[doc(hidden)]
pub mod fuzz_api {
    use serde_json::value::RawValue;

    use crate::AdapterError;

    pub fn decimal_string(raw: &RawValue, field: &str) -> Result<String, AdapterError> {
        crate::decimal::decimal_string(raw, field)
    }

    pub fn signed_decimal_string(raw: &RawValue, field: &str) -> Result<String, AdapterError> {
        crate::decimal::signed_decimal_string(raw, field)
    }
}

/// Maximum accepted WebSocket text message size.
pub const MAX_MESSAGE_BYTES: usize = 8 * 1024 * 1024;

/// Maximum accepted JSONL record size during replay.
pub const MAX_REPLAY_LINE_BYTES: usize = 16 * 1024 * 1024;
