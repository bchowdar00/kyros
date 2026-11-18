//! Binance and Bybit tier-1 spot siblings (B1) as standalone [`LaneProcessor`]s
//! on the A6 venue-lane shell.
//!
//! `btc.binance.*` and `btc.bybit.*` reuse the spot engine verbatim: one
//! [`VenueLane`] per venue drives the same [`BookState`], [`FlowSeries`],
//! [`OfiSeries`] and microstructure the USD spot lane uses, so the 13 families
//! are byte-for-byte the Coinbase formulas with the venue segment swapped and a
//! constant `quote_currency` (R6/E42). `SpotSignalProcessor` is untouched: these
//! run in their own process with their own publisher epoch (spec §2, R5).
//!
//! The only per-venue divergences (spec §6) are structural, not arithmetic: the
//! displayed best quote is adapted to one internal [`QuoteObservation`] shape
//! from Binance `book-ticker` (a scalar row) or Bybit `bbo-levels` (a one-level
//! book), and Binance's depth20 image can truncate the 5 bps band.
//!
//! Clock note (ADR 0027 / E40): the value-emitting `LaneClock { now_ns,
//! produced_at }` contract is being ported onto this base in parallel and is the
//! contract at merge. Until then the shell passes only `now_ns`, so the wall
//! instant is derived in exactly one place — [`VenueSpotProcessor::produced_at`]
//! — and every handler body is free of clock arithmetic, so the switch to the
//! shell-supplied `produced_at` is a one-line change to that helper.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde_json::Value;
use signals_core::{
    ProducerStatus, SignalUpdate, SourceDefinition, SourceEvent, Validate, parse_decimal_text,
};

use crate::feed_health::{
    FeedAvailability, FeedCadence, FeedContract, FeedHealthBoard, FeedHealthSnapshot,
};
use crate::lane::{LaneClock, LaneDefinitionDocument, LaneProcessor, LaneProcessorError};
use crate::market_state::{
    AdmittedTrade, Bbo, BookIngestOutcome, BookState, DEPTH_BAND_BPS, FlowSeries, ONE_SECOND_NS,
    OfiSeries, SIXTY_SECONDS_NS, STALENESS_BOUND_NS, TEN_SECONDS_NS, checked_mean,
    is_replayed_trade_snapshot, parse_admitted_trade,
};
use crate::spot_reference::{Venue, VenueQuoteSnapshot};
use crate::spot_signals::{
    FlowMetrics, SpotSignalError, SpotSignalFeatureCheckpoint, VenueDecision, VenueFeatureKind,
    VenueLane, logical_timestamp_text,
};
use crate::stream_accounting::ProducerHealth;
use crate::usdt_reference::{KRAKEN_USDT_TICKER_SOURCE_ID, UsdtReference};
use crate::venue_derivatives_signals::VenueDerivativesEngine;

/// The per-venue spot identities that reach Ready directly from the current book
/// or quote (and the checkpoint-carried USDT reference), with no warm-up ring or
/// windowed history: `spot_quote`, `spot_microprice` and
/// `actionable_depth_imbalance`. After a C5 rotation resync rebuilds the books,
/// these reach Ready in a cold replay at the same instant the warm live run did,
/// with byte-equal values — the replay lower bound (C5 finding 4) checks exactly
/// this set. Every other venue family carries a flow / OFI / volatility / TA /
/// microstructure-horizon ring or window whose re-warm may lag on a cold replay
/// (decision 22:36Z) and is therefore excluded. Derived as the full catalogue
/// (`VenueFeatureKind::ALL`) minus the ring kinds, so a new snapshot-direct family
/// is covered by being absent from `venue_feature_requires_ring`, never by
/// hand-listing an identity here.
#[must_use]
pub fn non_ring_venue_spot_signal_ids(venue: Venue) -> Vec<&'static str> {
    VenueFeatureKind::ALL
        .iter()
        .filter(|kind| !venue_feature_requires_ring(**kind))
        .map(|kind| kind.id(venue))
        .collect()
}

/// Whether a venue spot feature reads a warm-up ring or windowed history — the
/// flow / OFI series, realized & horizon volatility, momentum, the microstructure
/// slots computed over a horizon (effective spread, Kyle's lambda, sweep
/// detection, book resilience), the TA bars and tick velocity — whose state the
/// sealed checkpoint deliberately does not carry (it re-warms from live input).
/// The snapshot-direct families — quote, microprice, actionable depth imbalance —
/// read only the current book/quote and so carry no ring.
fn venue_feature_requires_ring(kind: VenueFeatureKind) -> bool {
    !matches!(
        kind,
        VenueFeatureKind::SpotQuote
            | VenueFeatureKind::SpotMicroprice
            | VenueFeatureKind::ActionableDepthImbalance
    )
}

/// The shared signal-set version for every B1 identity (spec §1).
const VENUE_SPOT_SIGNAL_VERSION: &str = "1.0.0";

/// The Kraken USDT/USD reference source definition (E47(e)), embedded so the
/// venue lane can validate the reference frames it receives on the dedicated
/// reference socket and surface the feed in `feed_health`. It is a reference
/// input, not a market-data source, so it does not gate venue readiness.
const KRAKEN_USDT_REFERENCE_DEFINITION_JSON: &str =
    include_str!("../../../definitions/btc/sources/kraken.spot.usdt-usd.ticker@1.0.0.json");

/// The feed-health venue label for the USDT/USD reference feed (its own venue,
/// distinct from the lane's Binance/Bybit venue), so an operator sees it as the
/// Kraken reference rather than a venue market-data feed.
const KRAKEN_USDT_REFERENCE_STREAM_VENUE: &str = "kraken";

/// How a venue supplies its displayed best quote (spec §3/§6 row 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QuoteKind {
    /// Binance `book-ticker`: a scalar row carrying the whole BBO.
    BookTicker,
    /// Bybit `bbo-levels` (`orderbook.1`): a one-level book, retained bid row
    /// plus ask row; not ready until BOTH sides are seeded.
    OneLevelBook,
}

/// The static description of one B1 venue lane's spot sources.
struct VenueSpec {
    venue: Venue,
    /// The lane's signal-set id, stamped into the journal session/checkpoint.
    signal_set_id: &'static str,
    /// The processor identity stamped into the journal session and checkpoint by
    /// the lane runtime (F3). It names the running processor — the full venue
    /// processor, not the feed-only interim — so a feed-only-era checkpoint
    /// (`btc.<venue>.feed_only`) no longer verifies against it.
    processor_id: &'static str,
    /// The feed-health venue label.
    venue_token: &'static str,
    /// The book source id (`depth20` / `orderbook.200`).
    book_source: &'static str,
    /// The trade source id (`agg-trades` / `publicTrade`).
    trade_source: &'static str,
    /// The quote source id (`book-ticker` / `orderbook.1`).
    quote_source: &'static str,
    quote_kind: QuoteKind,
    /// Whether this venue's displayed depth arrives as periodic full images
    /// (`true` for Binance `depth20@100ms`) rather than incremental snapshot/delta
    /// books (`false` for Bybit `orderbook.200`). It selects the OFI continuity
    /// path: image-diff (§6 row 4) versus the shared delta accumulator.
    full_image_depth: bool,
    /// The three embedded source definition documents `(id, json)`.
    source_docs: [(&'static str, &'static str); 3],
}

/// How a full-image venue tags one depth image relative to the previous one
/// (spec §4.1). `Reseed` is a baseline after a connect or ordering reset;
/// `FullReplace` continues from the previously applied image, so a pair of them
/// contributes order flow (§6 row 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImageKind {
    FullReplace,
    Reseed,
}

fn image_kind_of(event: &SourceEvent) -> Result<ImageKind, SpotSignalError> {
    match event.values.get("image_kind").and_then(Value::as_str) {
        Some("full_replace") => Ok(ImageKind::FullReplace),
        Some("reseed") => Ok(ImageKind::Reseed),
        other => Err(SpotSignalError::MarketValue(format!(
            "full-image book row carries an invalid image_kind: {other:?}"
        ))),
    }
}

const BINANCE_SPEC: VenueSpec = VenueSpec {
    venue: Venue::Binance,
    signal_set_id: "btc.binance",
    processor_id: "btc.binance.venue",
    venue_token: "binance",
    book_source: "binance.spot.btcusdt.book-levels",
    trade_source: "binance.spot.btcusdt.agg-trades",
    quote_source: "binance.spot.btcusdt.book-ticker",
    quote_kind: QuoteKind::BookTicker,
    full_image_depth: true,
    source_docs: [
        (
            "binance.spot.btcusdt.book-levels",
            include_str!(
                "../../../definitions/btc/sources/binance.spot.btcusdt.book-levels@1.0.0.json"
            ),
        ),
        (
            "binance.spot.btcusdt.agg-trades",
            include_str!(
                "../../../definitions/btc/sources/binance.spot.btcusdt.agg-trades@1.0.0.json"
            ),
        ),
        (
            "binance.spot.btcusdt.book-ticker",
            include_str!(
                "../../../definitions/btc/sources/binance.spot.btcusdt.book-ticker@1.0.0.json"
            ),
        ),
    ],
};

const BYBIT_SPEC: VenueSpec = VenueSpec {
    venue: Venue::Bybit,
    signal_set_id: "btc.bybit",
    processor_id: "btc.bybit.venue",
    venue_token: "bybit",
    book_source: "bybit.spot.btcusdt.book-levels",
    trade_source: "bybit.spot.btcusdt.trades",
    quote_source: "bybit.spot.btcusdt.bbo-levels",
    quote_kind: QuoteKind::OneLevelBook,
    full_image_depth: false,
    source_docs: [
        (
            "bybit.spot.btcusdt.book-levels",
            include_str!(
                "../../../definitions/btc/sources/bybit.spot.btcusdt.book-levels@1.0.0.json"
            ),
        ),
        (
            "bybit.spot.btcusdt.trades",
            include_str!("../../../definitions/btc/sources/bybit.spot.btcusdt.trades@1.0.0.json"),
        ),
        (
            "bybit.spot.btcusdt.bbo-levels",
            include_str!(
                "../../../definitions/btc/sources/bybit.spot.btcusdt.bbo-levels@1.0.0.json"
            ),
        ),
    ],
};

/// One venue's displayed best quote, normalised across the scalar Binance
/// `book-ticker` row and the two-row Bybit `orderbook.1` book (spec §3, §4).
#[derive(Debug, Clone)]
struct QuoteObservation {
    bid: Decimal,
    ask: Decimal,
    event_time: String,
    /// The admission logical time of the observation, for the staleness gate.
    logical_receipt_ns: u64,
}

/// The embedded signal contract documents `(id, json)` for both B1 venues, for
/// the definition-coverage guard. Reuses the slice-1 `include_str!` embedding in
/// [`VenueFeatureKind::raw_definition`], so a single source of truth backs both
/// the processor's `signal_definitions()` and this list.
#[must_use]
pub fn venue_spot_signal_contract_documents() -> Vec<(&'static str, &'static str)> {
    let mut documents = Vec::with_capacity(2 * VenueFeatureKind::ALL.len());
    for venue in [Venue::Binance, Venue::Bybit] {
        for kind in VenueFeatureKind::ALL {
            documents.push((kind.id(venue), kind.raw_definition(venue)));
        }
    }
    documents
}

/// The shared engine behind both B1 venue processors. One instance drives a
/// single venue; [`BinanceProcessor`] and [`BybitProcessor`] are the named
/// [`LaneProcessor`] implementations that wrap it (`build_processor` routes to
/// them).
pub struct VenueSpotProcessor {
    spec: &'static VenueSpec,
    publisher_epoch: String,
    wall_start: DateTime<Utc>,
    /// The reconstructed order book (`depth20` / `orderbook.200`).
    book: BookState,
    /// The one-level Bybit quote book; unused for Binance (`book-ticker` is a
    /// scalar row parsed straight into a [`QuoteObservation`]).
    bbo_book: BookState,
    flow: FlowSeries,
    ofi: OfiSeries,
    lane: VenueLane,
    /// The latest displayed best quote, or `None` before one is seeded.
    quote: Option<QuoteObservation>,
    /// The best quote of the last applied depth image, for the full-image OFI
    /// continuity path (§6 row 4). `None` until a baseline image lands and after
    /// any book invalidation; unused by delta-book venues (Bybit).
    last_image_bbo: Option<Bbo>,
    /// Per-source ordering cursor `(publisher_epoch, sequence)` for the trade
    /// source, mirroring the spot processor's admission gate.
    trade_cursor: Option<(String, u64)>,
    /// The most recent admitted-input logical time, for the quiet-window
    /// aliveness rule.
    last_input_ns: Option<u64>,
    last_now_ns: Option<u64>,
    source_definitions: BTreeMap<String, SourceDefinition>,
    board: FeedHealthBoard,
    contracts: Vec<FeedContract>,
    live_streams: BTreeSet<String>,
    ever_ready: bool,
    /// The derivatives half of this venue lane (B2): the five tier-1 derivatives
    /// families published on the same publisher epoch as the 13 spot siblings
    /// (spec §2, R5 — one publisher per venue).
    derivatives: VenueDerivativesEngine,
    /// The USDT/USD reference (E47): fed by the `kraken.spot.usdt-usd.ticker`
    /// frames arriving over the dedicated `kraken_usdt_reference` socket, read at
    /// each decision to convert every USD-quoting identity. It is a reference
    /// input, never a market-data source: it never enters producer readiness
    /// (see `health`), only the decision-time `usdt_reference_stale` verdict.
    usdt_reference: UsdtReference,
    /// The observed-not-consumed source set (F1): sources a declared stream of
    /// this lane produces that no identity reads. The lane runtime computes it at
    /// start-up and hands it over with [`set_observed_not_consumed`]. An event
    /// from one of these is counted (`board.note_ignored`) and dropped before the
    /// book/flow/quote/derivatives dispatch, so it never aborts the lane and never
    /// marks the venue alive.
    observed_not_consumed: BTreeSet<String>,
    /// Publication coalescing memory and each identity's declared interval
    /// (see [`crate::spot_signals::coalesce_publications`]).
    published: BTreeMap<String, crate::spot_signals::PublishedMemory>,
    intervals_ns: BTreeMap<String, u64>,
}

impl VenueSpotProcessor {
    fn new(
        spec: &'static VenueSpec,
        publisher_epoch: impl Into<String>,
        wall_start: DateTime<Utc>,
    ) -> Result<Self, SpotSignalError> {
        let publisher_epoch = publisher_epoch.into();
        let mut source_definitions = BTreeMap::new();
        let mut contracts = Vec::with_capacity(spec.source_docs.len());
        for (id, raw) in spec.source_docs {
            let definition: SourceDefinition = serde_json::from_str(raw).map_err(|error| {
                SpotSignalError::Definition(format!("invalid {id} source JSON: {error}"))
            })?;
            definition.validate().map_err(|error| {
                SpotSignalError::Definition(format!("invalid {id} source: {error}"))
            })?;
            if definition.source_id != id {
                return Err(SpotSignalError::Definition(format!(
                    "embedded source id {} does not match {id}",
                    definition.source_id
                )));
            }
            contracts.push(FeedContract {
                source_id: id.to_owned(),
                venue: spec.venue_token.to_owned(),
                expected_heartbeat_ms: definition.expected_heartbeat_ms,
                cadence: FeedCadence::EventDriven,
                liveness_bound_override_ms: None,
            });
            source_definitions.insert(id.to_owned(), definition);
        }
        let derivatives =
            VenueDerivativesEngine::new(spec.venue, publisher_epoch.clone(), wall_start)?;
        // The derivatives sources join the feed-health board so a `feed_health`
        // frame is emitted per new source (spec §5); producer readiness stays
        // gated on the spot source set (see `health`).
        contracts.extend(derivatives.feed_contracts());
        // E47(e): the USDT/USD reference feed is validated and shown in
        // `feed_health` (operators must see the reference socket), but it is NOT
        // a readiness-gating source — `health` keys readiness on `spec.source_docs`
        // only, so the reference never appears there.
        let reference_definition: SourceDefinition =
            serde_json::from_str(KRAKEN_USDT_REFERENCE_DEFINITION_JSON).map_err(|error| {
                SpotSignalError::Definition(format!("invalid Kraken USDT reference JSON: {error}"))
            })?;
        reference_definition.validate().map_err(|error| {
            SpotSignalError::Definition(format!("invalid Kraken USDT reference: {error}"))
        })?;
        contracts.push(FeedContract {
            source_id: reference_definition.source_id.clone(),
            venue: KRAKEN_USDT_REFERENCE_STREAM_VENUE.to_owned(),
            expected_heartbeat_ms: reference_definition.expected_heartbeat_ms,
            cadence: FeedCadence::EventDriven,
            liveness_bound_override_ms: None,
        });
        source_definitions.insert(reference_definition.source_id.clone(), reference_definition);
        let board = FeedHealthBoard::new(contracts.clone(), 0);
        let lane = VenueLane::new(spec.venue, wall_start)?;
        let intervals_ns = lane
            .definitions()
            .map(|definition| {
                (
                    definition.signal_id.clone(),
                    definition.approximate_interval_ms.saturating_mul(1_000_000),
                )
            })
            .collect();
        Ok(Self {
            spec,
            lane,
            publisher_epoch,
            wall_start,
            book: BookState::default(),
            bbo_book: BookState::default(),
            flow: FlowSeries::default(),
            ofi: OfiSeries::default(),
            quote: None,
            last_image_bbo: None,
            trade_cursor: None,
            last_input_ns: None,
            last_now_ns: None,
            source_definitions,
            board,
            contracts,
            live_streams: BTreeSet::new(),
            ever_ready: false,
            derivatives,
            usdt_reference: UsdtReference::new_kraken(),
            observed_not_consumed: BTreeSet::new(),
            published: BTreeMap::new(),
            intervals_ns,
        })
    }

    /// Record the observed-not-consumed source set the runtime computed (F1).
    fn set_observed_not_consumed(&mut self, sources: BTreeSet<String>) {
        self.observed_not_consumed = sources;
    }

    /// Sole authority for the decision wall instant (see the clock note). At the
    /// LaneClock merge this becomes `clock.produced_at.clone()` — one line. Every
    /// emit path (interval and invalid-book) reads its `produced_at` from here, so
    /// no handler body performs clock arithmetic.
    fn produced_at(&self, now_ns: u64) -> Result<String, SpotSignalError> {
        logical_timestamp_text(self.wall_start, now_ns)
    }

    fn observe_time(&mut self, now_ns: u64) -> Result<(), LaneProcessorError> {
        if self.last_now_ns.is_some_and(|previous| now_ns < previous) {
            return Err(LaneProcessorError::Computation(
                "lane logical time regressed".to_owned(),
            ));
        }
        self.last_now_ns = Some(now_ns);
        Ok(())
    }

    fn validate_event(&self, event: &SourceEvent) -> Result<(), SpotSignalError> {
        let definition = self
            .source_definitions
            .get(&event.source_id)
            .ok_or_else(|| SpotSignalError::UnsupportedSource(event.source_id.clone()))?;
        event.validate_against(definition)?;
        Ok(())
    }

    /// Route one admitted event to the book, trade or quote path by its source
    /// id. Only an invalid-book transition emits immediately; every other input
    /// mutates state and the 13 families publish on the interval tick.
    fn ingest_event(
        &mut self,
        event: &SourceEvent,
        now_ns: u64,
    ) -> Result<Vec<SignalUpdate>, LaneProcessorError> {
        // F1: a source a declared stream produces that no identity consumes is
        // counted and routed past the processor's signal computation — never an
        // abort, and never a liveness signal for the venue. Checked before any
        // other handling so an unowned frame cannot mark the venue alive.
        if self.observed_not_consumed.contains(&event.source_id) {
            self.board.note_ignored(&event.source_id, now_ns);
            return Ok(Vec::new());
        }
        self.board.observe_event(&event.source_id, now_ns);
        // E47: a reference frame updates the USDT/USD rate and the feed-health
        // board only. It is NOT venue market data, so it does not mark the venue
        // alive (`last_input_ns`), does not enter the venue's readiness set, and
        // is not dispatched to the book/flow/quote paths.
        if self.usdt_reference.owns_source(&event.source_id) {
            self.validate_event(event).map_err(computation)?;
            // G1: a crossed / non-positive / otherwise-unusable reference quote is
            // feed-health telemetry, NOT a lane error — count it and keep the prior
            // rate (a stale prior is caught at decision time as `usdt_reference_stale`).
            // Only the schema violation above (validate_event) is fatal, matching
            // every other stream. Propagating the rejection here would take all 18
            // identities down for one routine locked-book tick, since the reference
            // socket shares the lane's `try_join_all` fan-out.
            if self.usdt_reference.observe(event, now_ns).is_err() {
                self.board.note_reference_rejected(now_ns);
            }
            return Ok(Vec::new());
        }
        self.live_streams.insert(event.source_id.clone());
        // Every admitted event proves the venue is alive at this logical time.
        let entry = self.last_input_ns.get_or_insert(0);
        *entry = (*entry).max(now_ns);
        self.dispatch(event, now_ns).map_err(computation)
    }

    fn dispatch(
        &mut self,
        event: &SourceEvent,
        now_ns: u64,
    ) -> Result<Vec<SignalUpdate>, SpotSignalError> {
        let source = event.source_id.as_str();
        if source == self.spec.book_source {
            self.validate_event(event)?;
            // The image_kind discriminator is a per-image field on every row of a
            // full-image venue's group; read it from the row that completes the
            // group. Delta-book venues carry no such field and stay on `ingest`.
            let image_kind = if self.spec.full_image_depth {
                Some(image_kind_of(event)?)
            } else {
                None
            };
            let outcome = self.book.ingest(event, now_ns)?;
            self.accept_book_outcome(outcome, image_kind, now_ns)
        } else if source == self.spec.trade_source {
            self.validate_event(event)?;
            if self.accept_trade_ordering(event)? {
                self.flow.ingest(self.spec.venue, event, now_ns)?;
                self.observe_lane_trade(event, now_ns)?;
            }
            Ok(Vec::new())
        } else if source == self.spec.quote_source {
            self.validate_event(event)?;
            self.update_quote(event, now_ns)?;
            Ok(Vec::new())
        } else if self.derivatives.owns_source(source) {
            // B2: the five derivatives families fold their own bounded histories;
            // they publish on the interval tick, so no immediate emission here.
            self.derivatives.ingest(event, now_ns)?;
            Ok(Vec::new())
        } else {
            Err(SpotSignalError::UnsupportedSource(source.to_owned()))
        }
    }

    fn accept_book_outcome(
        &mut self,
        outcome: BookIngestOutcome,
        image_kind: Option<ImageKind>,
        now_ns: u64,
    ) -> Result<Vec<SignalUpdate>, SpotSignalError> {
        match outcome {
            BookIngestOutcome::Pending => Ok(Vec::new()),
            BookIngestOutcome::Complete(completion) => {
                match image_kind {
                    // Full-image depth (Binance): a reseed is a baseline; a
                    // full_replace contributes OFI between its BBO and the prior
                    // image's (§6 row 4). The USD-lane delta arithmetic is reused
                    // verbatim via `ingest_image`.
                    Some(kind) => {
                        let previous = match kind {
                            ImageKind::Reseed => None,
                            ImageKind::FullReplace => self.last_image_bbo,
                        };
                        self.ofi.ingest_image(
                            self.spec.venue,
                            previous,
                            completion.current_bbo,
                            completion.logical_receipt_ns,
                        )?;
                        self.last_image_bbo = Some(completion.current_bbo);
                    }
                    // Delta-book depth (Bybit orderbook.200): the shared
                    // snapshot/delta accumulator, unchanged.
                    None => self.ofi.ingest(self.spec.venue, &completion)?,
                }
                self.lane.observe_book(&completion)?;
                Ok(Vec::new())
            }
            BookIngestOutcome::Invalid { transitioned } => {
                if !transitioned {
                    return Ok(Vec::new());
                }
                self.ofi.clear_venue(self.spec.venue);
                self.last_image_bbo = None;
                let produced_at = self.produced_at(now_ns)?;
                let publisher_epoch = self.publisher_epoch.clone();
                self.lane
                    .emit_book_failure(&publisher_epoch, &produced_at, false)
            }
        }
    }

    /// Admit a trade only if its per-source cursor advances by exactly one, and
    /// refuse a publisher-epoch change mid-process — the spot processor's rule.
    fn accept_trade_ordering(&mut self, event: &SourceEvent) -> Result<bool, SpotSignalError> {
        match &mut self.trade_cursor {
            None => {
                self.trade_cursor = Some((event.publisher_epoch.clone(), event.sequence));
                Ok(true)
            }
            Some((epoch, _)) if epoch != &event.publisher_epoch => Err(SpotSignalError::Ordering(
                "trade source publisher epoch changed inside one signal process".to_owned(),
            )),
            Some((_, sequence)) if event.sequence <= *sequence => Ok(false),
            Some((_, sequence)) => {
                let expected = sequence.checked_add(1).ok_or(SpotSignalError::Counter)?;
                if event.sequence != expected {
                    return Err(SpotSignalError::Ordering(format!(
                        "trade source sequence jumped from {sequence} to {}",
                        event.sequence
                    )));
                }
                *sequence = event.sequence;
                Ok(true)
            }
        }
    }

    fn observe_lane_trade(
        &mut self,
        event: &SourceEvent,
        logical_receipt_ns: u64,
    ) -> Result<(), SpotSignalError> {
        if is_replayed_trade_snapshot(self.spec.venue, event)? {
            return Ok(());
        }
        let trade: AdmittedTrade = parse_admitted_trade(event)?;
        self.lane
            .observe_trade(logical_receipt_ns, &event.event_time, trade)
    }

    /// Fold one quote input into the retained displayed best quote. Binance
    /// `book-ticker` is a scalar row; Bybit `orderbook.1` is a one-level book
    /// whose retained BBO is read once both sides are seeded (spec §6 row 1).
    fn update_quote(&mut self, event: &SourceEvent, now_ns: u64) -> Result<(), SpotSignalError> {
        match self.spec.quote_kind {
            QuoteKind::BookTicker => {
                let bid = decimal_field(event, "best_bid")?;
                let ask = decimal_field(event, "best_ask")?;
                self.quote = Some(QuoteObservation {
                    bid,
                    ask,
                    event_time: event.event_time.clone(),
                    logical_receipt_ns: now_ns,
                });
            }
            QuoteKind::OneLevelBook => {
                // The one-level book applies through the shared BookState so its
                // snapshot/delta and both-sides-seeded semantics are the engine's,
                // not a second implementation.
                let _ = self.bbo_book.ingest(event, now_ns)?;
                if let Some((Bbo { bid, ask, .. }, event_time, receipt_ns)) = self.bbo_book.quote()
                {
                    self.quote = Some(QuoteObservation {
                        bid,
                        ask,
                        event_time,
                        logical_receipt_ns: receipt_ns,
                    });
                }
            }
        }
        // C3 row 8: one admitted quote update for `tick_velocity`, counted once
        // the retained quote is usable — every `book-ticker` row qualifies, while
        // a one-level book counts only once both sides are seeded. The midpoint
        // is the venue's own (pre-conversion) midpoint: the counter compares an
        // arrival process with itself, so no USD conversion enters it.
        if let Some(quote) = self.quote.as_ref()
            && quote.bid > Decimal::ZERO
            && quote.ask > quote.bid
            && let Some(midpoint) = checked_mean(quote.bid, quote.ask)
        {
            self.lane.observe_quote(now_ns, midpoint);
        }
        Ok(())
    }

    /// The displayed best quote as a decision snapshot when it is positive,
    /// uncrossed and within the staleness bound (spec §6 row 1); `None`
    /// otherwise, which the family emitters render as their not-ready reason.
    fn venue_quote(&self, now_ns: u64) -> Result<Option<VenueQuoteSnapshot>, SpotSignalError> {
        let Some(quote) = self.quote.as_ref() else {
            return Ok(None);
        };
        if quote.bid <= Decimal::ZERO || quote.ask <= Decimal::ZERO || quote.ask <= quote.bid {
            return Ok(None);
        }
        let Some(age_ns) = now_ns.checked_sub(quote.logical_receipt_ns) else {
            return Ok(None);
        };
        if age_ns > STALENESS_BOUND_NS {
            return Ok(None);
        }
        let midpoint = checked_mean(quote.bid, quote.ask).ok_or_else(|| {
            SpotSignalError::Arithmetic("quote midpoint is not representable".to_owned())
        })?;
        Ok(Some(VenueQuoteSnapshot {
            bid: quote.bid,
            ask: quote.ask,
            midpoint,
            event_time: quote.event_time.clone(),
            age_ms: age_ns / 1_000_000,
        }))
    }

    fn venue_alive(&self, now_ns: u64) -> bool {
        self.last_input_ns.is_some_and(|receipt| {
            now_ns
                .checked_sub(receipt)
                .is_some_and(|age| age <= STALENESS_BOUND_NS)
        })
    }

    /// Assemble everything this venue's 13 identities read on one decision. Only
    /// Binance's depth20 image can truncate the 5 bps band (§6 rows 3, 9); Bybit
    /// (200 levels) and the full-book USD venues never do.
    fn venue_decision(&self, now_ns: u64) -> Result<VenueDecision, SpotSignalError> {
        let band_truncated = self.spec.quote_kind == QuoteKind::BookTicker
            && self.book.band_within_captured(DEPTH_BAND_BPS)? == Some(false);
        Ok(VenueDecision {
            now_ns,
            quote: self.venue_quote(now_ns)?,
            book: self.book.metrics(now_ns, DEPTH_BAND_BPS)?,
            book_invalid: self.book.invalid_reason().is_some(),
            alive: self.venue_alive(now_ns),
            flow: FlowMetrics {
                one_second: self
                    .flow
                    .venue_window(self.spec.venue, now_ns, ONE_SECOND_NS)?,
                ten_seconds: self
                    .flow
                    .venue_window(self.spec.venue, now_ns, TEN_SECONDS_NS)?,
                sixty_seconds: self
                    .flow
                    .venue_window(self.spec.venue, now_ns, SIXTY_SECONDS_NS)?,
            },
            flow_warmed: self.flow.venue_warmed(self.spec.venue, now_ns),
            flow_fresh: self.flow.venue_fresh(self.spec.venue, now_ns),
            ofi_one_second: self
                .ofi
                .venue_window(self.spec.venue, now_ns, ONE_SECOND_NS)?,
            ofi_ten_seconds: self
                .ofi
                .venue_window(self.spec.venue, now_ns, TEN_SECONDS_NS)?,
            ofi_sixty_seconds: self
                .ofi
                .venue_window(self.spec.venue, now_ns, SIXTY_SECONDS_NS)?,
            ofi_warmed: self.ofi.venue_warmed(self.spec.venue, now_ns),
            venue_unavailable: false,
            band_truncated,
        })
    }

    /// Republish this venue's spot identities against the current book and
    /// quote after an admitted input batch, so a changed reading publishes
    /// now rather than at the next interval tick. The rings keep their
    /// once-per-interval sampling, the open trade batch stays open, and the
    /// derivatives half keeps its interval cadence; the coalescing rule drops
    /// unchanged restatements.
    fn emit_after_events(&mut self, now_ns: u64) -> Result<Vec<SignalUpdate>, LaneProcessorError> {
        if !self.ever_ready {
            return Ok(Vec::new());
        }
        let decision = self.venue_decision(now_ns).map_err(computation)?;
        let produced_at = self.produced_at(now_ns).map_err(computation)?;
        let publisher_epoch = self.publisher_epoch.clone();
        let reference = self.usdt_reference.snapshot(now_ns);
        let updates = self
            .lane
            .emit_outputs(&publisher_epoch, &produced_at, &decision, Some(reference))
            .map_err(computation)?;
        Ok(self.coalesce(now_ns, updates))
    }

    fn coalesce(&mut self, now_ns: u64, updates: Vec<SignalUpdate>) -> Vec<SignalUpdate> {
        crate::spot_signals::coalesce_publications(
            &mut self.published,
            &self.intervals_ns,
            now_ns,
            updates,
        )
    }

    fn emit_interval(&mut self, now_ns: u64) -> Result<Vec<SignalUpdate>, LaneProcessorError> {
        self.board.observe_tick(now_ns);
        let decision = self.venue_decision(now_ns).map_err(computation)?;
        let produced_at = self.produced_at(now_ns).map_err(computation)?;
        let publisher_epoch = self.publisher_epoch.clone();
        // E47: the USDT/USD reference as seen from THIS decision, handed to both
        // the spot and derivatives emitters so every converting identity uses the
        // same rate and staleness verdict at one logical instant.
        let reference = self.usdt_reference.snapshot(now_ns);
        let mut updates = self
            .lane
            .emit(&publisher_epoch, &produced_at, &decision, Some(reference))
            .map_err(computation)?;
        if !updates.is_empty() {
            self.ever_ready = true;
        }
        // The derivatives half publishes on the same interval tick and the same
        // publisher epoch (spec §2); it derives its own decision timestamp from
        // the shared wall-start baseline, so both halves stamp the same instant.
        updates.extend(
            self.derivatives
                .emit(now_ns, reference)
                .map_err(computation)?,
        );
        Ok(self.coalesce(now_ns, updates))
    }

    fn source_documents(&self) -> Vec<LaneDefinitionDocument> {
        let mut documents: Vec<LaneDefinitionDocument> = self
            .spec
            .source_docs
            .iter()
            .map(|(id, raw)| ((*id).to_owned(), (*raw).to_owned()))
            .collect();
        documents.extend(self.derivatives.source_documents());
        // E47(e): the USDT/USD reference is a source this lane now consumes, so it
        // joins the lane's declared source set and its bytes enter the contract
        // fingerprint. It is still not a readiness-gating feed (see `health`).
        documents.push((
            KRAKEN_USDT_TICKER_SOURCE_ID.to_owned(),
            KRAKEN_USDT_REFERENCE_DEFINITION_JSON.to_owned(),
        ));
        documents
    }

    fn signal_documents(&self) -> Vec<LaneDefinitionDocument> {
        let mut documents: Vec<LaneDefinitionDocument> = VenueFeatureKind::ALL
            .iter()
            .map(|kind| {
                (
                    kind.id(self.spec.venue).to_owned(),
                    kind.raw_definition(self.spec.venue).to_owned(),
                )
            })
            .collect();
        documents.extend(self.derivatives.signal_documents());
        documents
    }

    fn signal_count(&self) -> usize {
        VenueFeatureKind::ALL.len() + self.derivatives.signal_count()
    }

    /// Every source this processor routes to a handler without error: the three
    /// spot sources, every derivatives source (including the socket `mark-price`,
    /// so the socket path still works on a host that receives it), and the Kraken
    /// USDT/USD reference. The lane runtime subtracts this from its declared
    /// streams' inventories to find the observed-not-consumed sources (F1).
    fn consumed_sources(&self) -> BTreeSet<String> {
        let mut sources = BTreeSet::new();
        sources.insert(self.spec.book_source.to_owned());
        sources.insert(self.spec.trade_source.to_owned());
        sources.insert(self.spec.quote_source.to_owned());
        for source in self.derivatives.consumed_sources() {
            sources.insert(source.to_owned());
        }
        sources.insert(KRAKEN_USDT_TICKER_SOURCE_ID.to_owned());
        sources
    }

    /// The sources a declared stream MUST produce for a non-unavailable identity
    /// to ever reach ready (F1 misconfiguration guard). The three spot sources
    /// (the 13 spot families) plus the derivatives-required set; the Kraken
    /// reference is deliberately excluded — it is feed-health only and never gates
    /// producer readiness (E47(e)).
    fn required_sources(&self) -> BTreeSet<String> {
        let mut sources = BTreeSet::new();
        sources.insert(self.spec.book_source.to_owned());
        sources.insert(self.spec.trade_source.to_owned());
        sources.insert(self.spec.quote_source.to_owned());
        for source in self.derivatives.required_sources() {
            sources.insert(source.to_owned());
        }
        sources
    }

    /// The C5 rotation resync inputs: one complete book snapshot per
    /// delta-reconstructed book, rebuilt from the warm in-memory book so a cold
    /// replay of a rotation-opened generation rebuilds the book the sealed checkpoint
    /// does not carry (decision 23:30Z, amendment 02:39Z). Only the delta-book venue
    /// needs them: Binance's `depth20` is a periodic full image (its next image
    /// reseeds on replay), and its scalar `book-ticker` quote is no book — both yield
    /// nothing here. Bybit re-snapshots its `orderbook.200` `book` and its one-level
    /// `orderbook.1` `bbo_book`. The rows carry each Bybit book source's full declared
    /// field set (the admit path requires it): `resync_rows` fills the core
    /// group/side/price/size/event_time fields, and the venue-static fields
    /// (`instrument`, the message-level `update_id`/`cross_sequence` ordering tokens
    /// the book path never reads, and `event_time_basis`) are supplied here as a
    /// synthesized resync group — honest because the record is tagged `resync`, not a
    /// received venue frame. An unready book yields an empty batch, dropped.
    fn book_resync_inputs(&self) -> Vec<Vec<SourceEvent>> {
        // Both branches are Bybit-only today (Binance is full-image + book-ticker), so
        // the Bybit declared field set applies to every batch produced here.
        let bybit_book_fields: [(&'static str, Value); 4] = [
            ("instrument", Value::from("BTCUSDT")),
            ("update_id", Value::from(0_u64)),
            ("cross_sequence", Value::from(0_u64)),
            ("event_time_basis", Value::from("source")),
        ];
        let mut batches = Vec::new();
        if !self.spec.full_image_depth {
            batches.push(self.book.resync_rows(
                self.spec.book_source,
                &self.source_version(self.spec.book_source),
                &bybit_book_fields,
            ));
        }
        if self.spec.quote_kind == QuoteKind::OneLevelBook {
            batches.push(self.bbo_book.resync_rows(
                self.spec.quote_source,
                &self.source_version(self.spec.quote_source),
                &bybit_book_fields,
            ));
        }
        batches
            .into_iter()
            .filter(|rows| !rows.is_empty())
            .collect()
    }

    /// The declared version of a source this processor knows, for stamping resync
    /// rows. Falls back to `1.0.0` (the only version the embedded definitions carry)
    /// if the id is absent, which cannot happen for the lane's own book sources.
    fn source_version(&self, source_id: &str) -> String {
        self.source_definitions
            .get(source_id)
            .map(|definition| definition.version.clone())
            .unwrap_or_else(|| "1.0.0".to_owned())
    }

    /// Apply a C5 rotation resync batch: route each row to its book ONLY (the deep
    /// `book` or the one-level `bbo_book`), touching nothing else. Deliberately does
    /// NOT call [`Self::ingest_event`]/[`Self::dispatch`], so a resync never runs the
    /// feed-health board observation, never inserts into `live_streams`, never bumps
    /// `last_input_ns` (the venue aliveness clock), and never reaches the USDT
    /// reference / provider-clock path: a rebuilt snapshot is not proof the feed is
    /// live, so a feed silent before the rotation still ages to `stale`/`degraded` at
    /// exactly the instant an unrotated run would. The book outcome is discarded (no
    /// invalid-book emission, no OFI observation — the OFI/flow rings re-warm honestly,
    /// like every ring), so the rows change no output on the warm live process
    /// (ordering-skipped) and only rebuild the book on a cold replay. `now_ns` still
    /// advances the monotonic logical clock guard.
    fn ingest_resync(
        &mut self,
        events: &[SourceEvent],
        now_ns: u64,
    ) -> Result<(), LaneProcessorError> {
        self.observe_time(now_ns)?;
        for event in events {
            self.ingest_resync_row(event, now_ns).map_err(computation)?;
        }
        Ok(())
    }

    fn ingest_resync_row(
        &mut self,
        event: &SourceEvent,
        now_ns: u64,
    ) -> Result<(), SpotSignalError> {
        self.validate_event(event)?;
        let source = event.source_id.as_str();
        if source == self.spec.book_source {
            let _ = self.book.ingest(event, now_ns)?;
        } else if source == self.spec.quote_source
            && self.spec.quote_kind == QuoteKind::OneLevelBook
        {
            let _ = self.bbo_book.ingest(event, now_ns)?;
        } else {
            return Err(SpotSignalError::UnsupportedSource(format!(
                "{source} is not a resync-eligible book source"
            )));
        }
        Ok(())
    }
}

fn computation(error: SpotSignalError) -> LaneProcessorError {
    LaneProcessorError::Computation(error.to_string())
}

fn decimal_field(event: &SourceEvent, name: &'static str) -> Result<Decimal, SpotSignalError> {
    let text = event
        .values
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| SpotSignalError::MissingField(name.to_owned()))?;
    Ok(parse_decimal_text(text)?)
}

/// The bounded resume state. The rolling book/flow/OFI/volatility histories are
/// deliberately not persisted: a resumed lane re-warms them from live input, so
/// only the emitters' output lineage and the readiness memory are carried, as
/// the spot processor's checkpoint does.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct VenueSpotCheckpointState {
    features: BTreeMap<String, SpotSignalFeatureCheckpoint>,
    last_now_ns: Option<u64>,
    #[serde(default)]
    ever_ready: bool,
    /// The USDT/USD reference state (E47 item 5), so a resumed lane converts with
    /// the last observed rate rather than going `usdt_reference_stale` until the
    /// reference socket re-seeds. Absent in pre-E47 checkpoints (deserializes to
    /// `None`), leaving the reference empty on resume — the safe default.
    #[serde(default)]
    usdt_reference: Option<crate::usdt_reference::UsdtReferenceCheckpoint>,
}

impl VenueSpotProcessor {
    /// Restart every published identity's output sequence at zero for a new file
    /// generation (C5 Design B): the 13 spot siblings and the derivatives
    /// families this lane publishes on one publisher epoch (spec §2, R5). The
    /// shell calls this on the warm processor at an in-place rotation boundary so
    /// the successor's fresh output epoch numbers each identity's first ready
    /// decision from zero, matching the sequence an independent replay of that
    /// generation's journal (whose restore starts the emitters at zero) produces.
    /// The warm rings, books and lifetime state cross the boundary untouched.
    fn reset_output_sequences(&mut self) {
        self.lane.reset_output_sequences();
        self.derivatives.reset_output_sequences();
        // Publication memory is not checkpointed: a rotation starts cold in
        // the live process exactly as its replay does.
        self.published.clear();
    }

    fn checkpoint_value(&self) -> serde_json::Value {
        let mut features = self.lane.checkpoint_features();
        features.extend(self.derivatives.checkpoint_features());
        serde_json::to_value(VenueSpotCheckpointState {
            features,
            last_now_ns: self.last_now_ns,
            ever_ready: self.ever_ready,
            usdt_reference: Some(self.usdt_reference.checkpoint()),
        })
        .expect("venue spot checkpoint state serializes")
    }

    fn restore_value(&mut self, state: serde_json::Value) -> Result<(), LaneProcessorError> {
        let state: VenueSpotCheckpointState = serde_json::from_value(state)
            .map_err(|error| LaneProcessorError::InvalidState(error.to_string()))?;
        self.lane
            .restore_features(&state.features)
            .map_err(|error| LaneProcessorError::InvalidState(error.to_string()))?;
        self.derivatives
            .restore_features(&state.features)
            .map_err(|error| LaneProcessorError::InvalidState(error.to_string()))?;
        self.last_now_ns = state.last_now_ns;
        self.ever_ready = state.ever_ready;
        // E47: restore the last observed USDT/USD reference so conversion resumes
        // immediately; it re-ages from the checkpointed observation time and goes
        // `usdt_reference_stale` on its own if the socket does not re-seed in time.
        self.usdt_reference = UsdtReference::new_kraken();
        if let Some(reference) = &state.usdt_reference {
            self.usdt_reference.restore(reference);
        }
        // A resumed lane re-warms its rolling state from live input and must not
        // claim a feed live until its socket reconnects (M6).
        self.book = BookState::default();
        self.bbo_book = BookState::default();
        self.flow = FlowSeries::default();
        self.ofi = OfiSeries::default();
        self.quote = None;
        self.last_image_bbo = None;
        self.trade_cursor = None;
        self.last_input_ns = None;
        self.live_streams.clear();
        Ok(())
    }

    fn health(&self) -> ProducerHealth {
        let required: BTreeSet<&str> = self.spec.source_docs.iter().map(|(id, _)| *id).collect();
        let live: BTreeSet<&str> = self.live_streams.iter().map(String::as_str).collect();
        if required.is_subset(&live) {
            return ProducerHealth {
                status: ProducerStatus::Ready,
                reasons: Vec::new(),
            };
        }
        let mut reasons: Vec<String> = required
            .difference(&live)
            .map(|id| format!("{id}_unavailable"))
            .collect();
        if reasons.is_empty() {
            reasons.push("feeds_incomplete".to_owned());
        }
        let status = if self.ever_ready {
            ProducerStatus::Degraded
        } else {
            ProducerStatus::Starting
        };
        ProducerHealth { status, reasons }
    }

    fn connectivity(
        &mut self,
        stream: &str,
        connected: bool,
        now_ns: u64,
    ) -> Result<Vec<SignalUpdate>, LaneProcessorError> {
        // E47(e): the reference socket's connectivity is reflected in feed_health
        // only. It is not a venue market-data stream, so it never enters the
        // venue readiness set and its loss does not reset the venue histories —
        // the reference simply ages out into `usdt_reference_stale` at decision
        // time.
        let is_reference = stream == crate::lane::KRAKEN_USDT_REFERENCE_STREAM.label;
        if connected {
            self.board.observe_connected(stream, now_ns);
            if !is_reference {
                self.live_streams.insert(stream.to_owned());
            }
        } else {
            self.board
                .observe_availability(stream, FeedAvailability::Down, "disconnected", now_ns);
            if !is_reference {
                self.live_streams.remove(stream);
                // A closed socket is a causal break for the derivatives histories
                // too: they re-enter warm-up (`<venue>_warming_up`) on reconnect
                // (spec §3).
                self.derivatives.reset();
            }
        }
        Ok(Vec::new())
    }
}

/// One [`LaneProcessor`] method surface, delegated from the two named venue
/// processors to the shared [`VenueSpotProcessor`]. Keeps the two processors
/// distinct types (as `build_processor` and the specs demand) with no duplicated
/// logic.
macro_rules! venue_lane_processor {
    ($name:ident, $spec:expr) => {
        /// A B1 venue lane processor. See the module docs.
        pub struct $name(VenueSpotProcessor);

        impl $name {
            /// Build the processor for a publisher epoch and the session wall
            /// clock (the produced-at baseline until the LaneClock merge).
            pub fn new(
                publisher_epoch: impl Into<String>,
                wall_start: DateTime<Utc>,
            ) -> Result<Self, SpotSignalError> {
                Ok(Self(VenueSpotProcessor::new(
                    $spec,
                    publisher_epoch,
                    wall_start,
                )?))
            }
        }

        impl LaneProcessor for $name {
            fn signal_set_id(&self) -> &str {
                self.0.spec.signal_set_id
            }

            fn signal_set_version(&self) -> &str {
                VENUE_SPOT_SIGNAL_VERSION
            }

            fn processor_id(&self) -> &str {
                self.0.spec.processor_id
            }

            fn processor_version(&self) -> &str {
                VENUE_SPOT_SIGNAL_VERSION
            }

            fn consumed_sources(&self) -> Option<std::collections::BTreeSet<String>> {
                Some(self.0.consumed_sources())
            }

            fn required_sources(&self) -> std::collections::BTreeSet<String> {
                self.0.required_sources()
            }

            fn set_observed_not_consumed(&mut self, sources: std::collections::BTreeSet<String>) {
                self.0.set_observed_not_consumed(sources);
            }

            fn signal_count(&self) -> usize {
                self.0.signal_count()
            }

            fn venues(&self) -> &'static [&'static str] {
                std::slice::from_ref(&self.0.spec.venue_token)
            }

            fn source_definitions(&self) -> Vec<LaneDefinitionDocument> {
                self.0.source_documents()
            }

            fn signal_definitions(&self) -> Vec<LaneDefinitionDocument> {
                self.0.signal_documents()
            }

            fn feed_contracts(&self) -> Vec<FeedContract> {
                self.0.contracts.clone()
            }

            fn on_events(
                &mut self,
                _stream: &str,
                events: &[SourceEvent],
                clock: &LaneClock,
            ) -> Result<Vec<SignalUpdate>, LaneProcessorError> {
                let now_ns = clock.now_ns;
                self.0.observe_time(now_ns)?;
                let mut updates = Vec::new();
                for event in events {
                    updates.extend(self.0.ingest_event(event, now_ns)?);
                }
                updates.extend(self.0.emit_after_events(now_ns)?);
                Ok(updates)
            }

            fn on_connectivity(
                &mut self,
                stream: &str,
                connected: bool,
                clock: &LaneClock,
            ) -> Result<Vec<SignalUpdate>, LaneProcessorError> {
                let now_ns = clock.now_ns;
                self.0.observe_time(now_ns)?;
                self.0.connectivity(stream, connected, now_ns)
            }

            fn on_interval(
                &mut self,
                clock: &LaneClock,
            ) -> Result<Vec<SignalUpdate>, LaneProcessorError> {
                let now_ns = clock.now_ns;
                self.0.observe_time(now_ns)?;
                self.0.emit_interval(now_ns)
            }

            fn producer_health(&self) -> ProducerHealth {
                self.0.health()
            }

            fn feed_health_snapshot(&self) -> FeedHealthSnapshot {
                self.0.board.snapshot()
            }

            fn feed_health_emission_revision(&self) -> u64 {
                self.0.board.emission_revision()
            }

            fn checkpoint_state(&self) -> serde_json::Value {
                self.0.checkpoint_value()
            }

            fn restore_state(
                &mut self,
                state: serde_json::Value,
            ) -> Result<(), LaneProcessorError> {
                self.0.restore_value(state)
            }

            fn reset_output_sequences(&mut self) {
                self.0.reset_output_sequences();
            }

            fn resync_inputs(&self) -> Vec<Vec<SourceEvent>> {
                self.0.book_resync_inputs()
            }

            fn on_resync_events(
                &mut self,
                events: &[SourceEvent],
                clock: &LaneClock,
            ) -> Result<Vec<SignalUpdate>, LaneProcessorError> {
                self.0.ingest_resync(events, clock.now_ns)?;
                Ok(Vec::new())
            }
        }
    };
}

venue_lane_processor!(BinanceProcessor, &BINANCE_SPEC);
venue_lane_processor!(BybitProcessor, &BYBIT_SPEC);

#[cfg(test)]
mod resync_tests {
    use super::*;
    use chrono::TimeZone;

    /// A valid Bybit `book-levels` snapshot group over `levels` (`(side, price,
    /// size)`), carrying the full declared field set the admit path requires,
    /// numbered contiguously from `base_seq`.
    fn bybit_snapshot(
        base_seq: u64,
        event_time: &str,
        levels: &[(&str, &str, &str)],
    ) -> Vec<SourceEvent> {
        let count = u32::try_from(levels.len()).expect("level count fits u32");
        let group = format!("snap-{base_seq}");
        levels
            .iter()
            .enumerate()
            .map(|(index, (side, price, size))| {
                let mut values: BTreeMap<String, Value> = BTreeMap::new();
                values.insert("instrument".to_owned(), Value::from("BTCUSDT"));
                values.insert("update_group".to_owned(), Value::from(group.clone()));
                values.insert("update_index".to_owned(), Value::from(index as u32));
                values.insert("update_count".to_owned(), Value::from(count));
                values.insert("update_kind".to_owned(), Value::from("snapshot"));
                values.insert("side".to_owned(), Value::from(*side));
                values.insert("price".to_owned(), Value::from(*price));
                values.insert("size".to_owned(), Value::from(*size));
                values.insert("update_id".to_owned(), Value::from(base_seq));
                values.insert("cross_sequence".to_owned(), Value::from(base_seq));
                values.insert("event_time_basis".to_owned(), Value::from("source"));
                values.insert("event_time".to_owned(), Value::from(event_time));
                SourceEvent {
                    schema_version: 1,
                    source_id: BYBIT_SPEC.book_source.to_owned(),
                    source_version: "1.0.0".to_owned(),
                    publisher_epoch: "epoch".to_owned(),
                    sequence: base_seq + index as u64,
                    event_time: event_time.to_owned(),
                    received_at: event_time.to_owned(),
                    values,
                }
            })
            .collect()
    }

    fn warm_bybit() -> VenueSpotProcessor {
        let wall = Utc.with_ymd_and_hms(2026, 9, 5, 0, 0, 0).unwrap();
        let mut processor =
            VenueSpotProcessor::new(&BYBIT_SPEC, "epoch", wall).expect("bybit processor builds");
        let snapshot = bybit_snapshot(
            10,
            "2026-09-05T00:00:00Z",
            &[
                ("bid", "100000.0", "2"),
                ("bid", "99998.0", "1"),
                ("ask", "100001.0", "3"),
                ("ask", "100002.0", "1"),
            ],
        );
        for event in &snapshot {
            processor
                .ingest_event(event, ONE_SECOND_NS)
                .expect("warm book snapshot ingests");
        }
        processor
    }

    /// Finding (1): a rotation resync rebuilds the book but NEVER revives the venue's
    /// aliveness clock, feed-health board, or stream-live set. A feed silent since T
    /// (last input at T) with a rotation-resync at T+9 s must still have its aliveness
    /// clock at T — so it fails closed at T+10 s exactly as an unrotated run would,
    /// not at T+19 s. The rebuilt book, by contrast, IS seeded.
    #[test]
    fn resync_rebuilds_the_book_without_reviving_venue_aliveness() {
        let warm = warm_bybit();
        assert_eq!(
            warm.last_input_ns,
            Some(ONE_SECOND_NS),
            "the live snapshot set the venue aliveness clock",
        );
        let warm_metrics = warm
            .book
            .metrics(ONE_SECOND_NS, DEPTH_BAND_BPS)
            .expect("warm book metrics compute")
            .expect("the warm book is ready");
        let book_batch: Vec<SourceEvent> = warm
            .book_resync_inputs()
            .into_iter()
            .find(|batch| {
                batch
                    .first()
                    .is_some_and(|row| row.source_id == BYBIT_SPEC.book_source)
            })
            .expect("the warm bybit book yields a resync batch");

        // A fresh, cold processor receives the resync at T+9 s (the feed has been
        // silent since T). ingest_resync must touch the book and nothing else.
        let mut cold = VenueSpotProcessor::new(
            &BYBIT_SPEC,
            "epoch",
            Utc.with_ymd_and_hms(2026, 9, 5, 0, 0, 0).unwrap(),
        )
        .expect("cold bybit processor builds");
        let board_revision_before = cold.board.emission_revision();
        let resync_ns = ONE_SECOND_NS + 9 * ONE_SECOND_NS;
        cold.ingest_resync(&book_batch, resync_ns)
            .expect("the resync rebuilds the book");

        // Aliveness NOT revived: a rebuilt snapshot is not proof the feed is live.
        assert_eq!(
            cold.last_input_ns, None,
            "resync must not set the venue aliveness clock (would delay fail-closed)",
        );
        // Feed-health board and stream-live set untouched.
        assert_eq!(
            cold.board.emission_revision(),
            board_revision_before,
            "resync must not observe on the feed-health board",
        );
        assert!(
            cold.live_streams.is_empty(),
            "resync must not mark the stream live",
        );
        // The book WAS rebuilt, byte-for-byte with the warm book's depth/imbalance.
        let cold_metrics = cold
            .book
            .metrics(resync_ns, DEPTH_BAND_BPS)
            .expect("rebuilt book metrics compute")
            .expect("the rebuilt book is ready");
        assert_eq!(cold_metrics.bid_depth, warm_metrics.bid_depth);
        assert_eq!(cold_metrics.ask_depth, warm_metrics.ask_depth);
        assert_eq!(cold_metrics.imbalance, warm_metrics.imbalance);
    }

    /// Finding (4) mutation: a resync that rebuilds a WRONG book (here, bids only —
    /// the ask side dropped) must produce a book-direct signal that diverges from the
    /// warm book's, so the per-identity replay verification catches it rather than
    /// waving it through as a re-warm.
    #[test]
    fn a_bids_only_resync_rebuilds_a_divergent_book() {
        let warm = warm_bybit();
        let warm_metrics = warm
            .book
            .metrics(ONE_SECOND_NS, DEPTH_BAND_BPS)
            .expect("warm book metrics compute")
            .expect("the warm book is ready");
        let book_batch: Vec<SourceEvent> = warm
            .book_resync_inputs()
            .into_iter()
            .find(|batch| {
                batch
                    .first()
                    .is_some_and(|row| row.source_id == BYBIT_SPEC.book_source)
            })
            .expect("the warm bybit book yields a resync batch");

        // Drop the ask rows and renumber the bid rows into a consistent group, so the
        // book ingests cleanly but rebuilds bids only.
        let bids: Vec<&SourceEvent> = book_batch
            .iter()
            .filter(|row| row.values.get("side").and_then(Value::as_str) == Some("bid"))
            .collect();
        let count = u32::try_from(bids.len()).unwrap();
        let base = bids[0].sequence;
        let mutated: Vec<SourceEvent> = bids
            .iter()
            .enumerate()
            .map(|(index, row)| {
                let mut event = (*row).clone();
                event.sequence = base + index as u64;
                event
                    .values
                    .insert("update_group".to_owned(), Value::from("resync-bids-only"));
                event
                    .values
                    .insert("update_index".to_owned(), Value::from(index as u32));
                event
                    .values
                    .insert("update_count".to_owned(), Value::from(count));
                event
            })
            .collect();

        let mut cold = VenueSpotProcessor::new(
            &BYBIT_SPEC,
            "epoch",
            Utc.with_ymd_and_hms(2026, 9, 5, 0, 0, 0).unwrap(),
        )
        .expect("cold bybit processor builds");
        cold.ingest_resync(&mutated, ONE_SECOND_NS)
            .expect("the bids-only resync ingests");
        let diverged = match cold.book.metrics(ONE_SECOND_NS, DEPTH_BAND_BPS) {
            Err(_) => true,
            Ok(None) => true,
            Ok(Some(metrics)) => {
                metrics.imbalance != warm_metrics.imbalance
                    || metrics.ask_depth != warm_metrics.ask_depth
            }
        };
        assert!(
            diverged,
            "a bids-only resync must rebuild a book whose depth/imbalance differs from \
             the warm book, so the per-identity replay check fails it",
        );
    }

    /// C5 Design B: at an in-place rotation boundary the shell keeps the venue
    /// processor warm (its rings, books and feature memory cross the boundary) and
    /// calls `reset_output_sequences` (the `LanePipeline::begin_generation` step),
    /// so the successor's fresh output epoch numbers every spot-sibling and
    /// derivatives identity's first ready decision from zero. Before the fix the
    /// warm emitters carried their departed sequence across the boundary, breaking
    /// the "first ready decision in an epoch is sequence zero" contract. A resume
    /// rebuilds the emitters from the checkpoint at sequence zero, so this reset
    /// keeps a live rotated generation numbering identically to that replay.
    #[test]
    fn rotation_restarts_every_venue_identity_sequence_at_zero() {
        use signals_core::QualityState;

        fn bybit_book_at(
            base_seq: u64,
            event_time: &str,
            bid: &str,
            ask: &str,
        ) -> Vec<SourceEvent> {
            bybit_snapshot(
                base_seq,
                event_time,
                &[
                    ("bid", bid, "2"),
                    ("bid", "99998.0", "1"),
                    ("ask", ask, "3"),
                    ("ask", "100050.0", "1"),
                ],
            )
        }

        fn ready_sequences(updates: &[SignalUpdate]) -> BTreeMap<String, u64> {
            updates
                .iter()
                .filter(|update| update.quality.state == QualityState::Ready)
                .map(|update| (update.signal_id.clone(), update.sequence))
                .collect()
        }

        // Warm the book (snapshot at 1 s), then publish twice over changing books so
        // at least one identity's per-identity sequence advances beyond zero.
        let mut processor = warm_bybit();
        processor
            .observe_time(2 * ONE_SECOND_NS)
            .expect("observe the first decision time");
        let first = processor
            .emit_interval(2 * ONE_SECOND_NS)
            .expect("first venue decision");
        for event in &bybit_book_at(14, "2026-09-05T00:00:03Z", "100010.0", "100011.0") {
            processor
                .ingest_event(event, 3 * ONE_SECOND_NS)
                .expect("second book snapshot ingests");
        }
        processor
            .observe_time(3 * ONE_SECOND_NS)
            .expect("observe the second decision time");
        let second = processor
            .emit_interval(3 * ONE_SECOND_NS)
            .expect("second venue decision");

        let max_before = first
            .iter()
            .chain(&second)
            .filter(|update| update.quality.state == QualityState::Ready)
            .map(|update| update.sequence)
            .max()
            .unwrap_or(0);
        assert!(
            max_before >= 1,
            "the warm venue lane advanced an identity's sequence beyond zero (max {max_before})"
        );

        // ROTATE: restart every per-identity sequence, exactly as the shell does at
        // `begin_generation`, while the warm book/rings cross the boundary.
        processor.reset_output_sequences();

        // A changed book publishes each identity again — now at sequence zero.
        for event in &bybit_book_at(18, "2026-09-05T00:00:04Z", "100020.0", "100021.0") {
            processor
                .ingest_event(event, 4 * ONE_SECOND_NS)
                .expect("post-rotation book snapshot ingests");
        }
        processor
            .observe_time(4 * ONE_SECOND_NS)
            .expect("observe the post-rotation decision time");
        let after = processor
            .emit_interval(4 * ONE_SECOND_NS)
            .expect("post-rotation venue decision");

        let ready_after = ready_sequences(&after);
        assert!(
            !ready_after.is_empty(),
            "the rotated generation published at least one identity to check"
        );
        for (signal_id, sequence) in &ready_after {
            assert_eq!(
                *sequence, 0,
                "{signal_id}'s first ready decision after the rotation carries sequence zero",
            );
        }
    }
}
