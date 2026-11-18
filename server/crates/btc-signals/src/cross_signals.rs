//! The cross lane's tier-3 `btc.cross.*` processor (C1, ruling E52).
//!
//! `venue-signals --lane cross` runs this processor on the A6 shell. There is no
//! inter-lane bus (E52 clause 1): the cross lane re-embeds the upstream
//! computations and subscribes their raw sockets itself, exactly as the
//! prediction lane embeds [`SpotReferenceProcessor`]. It embeds three engines and
//! harvests their per-interval value maps:
//!
//! * [`SpotReferenceProcessor`] over Coinbase + Kraken BTC/USD tickers — the USD
//!   composite spot reference (`btc.external.spot_reference`), the spot leg of the
//!   basis and the anchor of the liquidation response;
//! * the Bybit linear [`VenueDerivativesEngine`] over the Bybit tickers /
//!   liquidations / trades sockets — the Bybit `mark_index_basis_state`,
//!   `funding_state`, `open_interest_delta` and `liquidation_pressure` legs, all
//!   USD-converted upstream (E47);
//! * the [`UsdtReference`] over the Kraken USDT/USD reference socket — the rate the
//!   Bybit derivatives engine converts with.
//!
//! It PUBLISHES only `btc.cross.perp_spot_basis`,
//! `btc.cross.liquidation_vs_spot_response` and `btc.cross.derivatives_regime`;
//! the embedded engines' own `btc.external.*` / `btc.bybit.*` frames are harvested
//! and dropped, never republished. The cross lane is Bybit-only (ruling E66 under
//! E62): the `{binance, bybit}` venue set of the original spec §6.1 predates E62
//! and, from the Tokyo box, would make `perp_spot_basis` and `derivatives_regime`
//! structurally never ready (no Binance streams, E30/E34), which is not a signal.
//! So the three identities read the single Bybit derivatives leg against the
//! composite spot reference: `perp_spot_basis` and `derivatives_regime` reach ready
//! from the Bybit inputs alone, `liquidation_vs_spot_response` is triggered by a
//! Bybit cascade, and NO `binance_*` reason can appear on this lane.
//!
//! Every output decimal is rounded to eight places round-half-to-even
//! ([`decimal_value`]); every guard is evaluated at full internal precision first.
//! State advances only on journaled inputs and the journaled decision clock, so a
//! replay reproduces every frame byte-for-byte.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use signals_core::{
    ProducerStatus, QualityState, SignalUpdate, SourceDefinition, SourceEvent, Validate,
    parse_decimal_text,
};

use crate::feed_health::{
    FeedAvailability, FeedCadence, FeedContract, FeedHealthBoard, FeedHealthSnapshot,
};
use crate::lane::{LaneClock, LaneDefinitionDocument, LaneProcessor, LaneProcessorError};
use crate::spot_reference::{
    SpotReferenceCheckpointState, SpotReferenceProcessor, UnavailableReason, Venue,
};
use crate::spot_signals::{
    FeatureEmitter, SpotSignalError, SpotSignalFeatureCheckpoint, checked_div, checked_mul,
    checked_sub, decimal_value, logical_timestamp_text,
};
use crate::stream_accounting::ProducerHealth;
use crate::usdt_reference::{KRAKEN_USDT_TICKER_SOURCE_ID, UsdtReference, UsdtReferenceCheckpoint};
use crate::venue_derivatives_signals::VenueDerivativesEngine;

/// The cross lane's signal-set id and version (the checkpoint processor id, F3).
pub const CROSS_SIGNAL_SET_ID: &str = "btc.cross";
pub const CROSS_SIGNAL_SET_VERSION: &str = "1.0.0";
const CROSS_SIGNAL_VERSION: &str = "1.0.0";

/// The lane's venue tokens (the feeds it embeds); metadata for the journal
/// session and checkpoint identity.
const CROSS_VENUES: &[&str] = &["bybit", "coinbase", "kraken"];

// Stream labels (the LaneSpec's `LaneStream::label`s), for readiness tracking.
const COINBASE_CORE_LABEL: &str = "coinbase_core";
const KRAKEN_CORE_LABEL: &str = "kraken_core";
const BYBIT_LINEAR_LABEL: &str = "bybit_linear";
const BYBIT_LINEAR_REST_LABEL: &str = "bybit_linear_rest";
/// The Kraken USDT/USD reference socket label (E47(e)); a reference feed, never
/// readiness-gating.
pub const KRAKEN_USDT_REFERENCE_LABEL: &str = "kraken_usdt_reference";

/// The driver streams whose liveness gates producer readiness (the reference
/// stream is deliberately excluded).
const REQUIRED_STREAM_LABELS: [&str; 4] = [
    COINBASE_CORE_LABEL,
    KRAKEN_CORE_LABEL,
    BYBIT_LINEAR_LABEL,
    BYBIT_LINEAR_REST_LABEL,
];

const PERP_SPOT_BASIS_ID: &str = "btc.cross.perp_spot_basis";
const LIQUIDATION_RESPONSE_ID: &str = "btc.cross.liquidation_vs_spot_response";
const DERIVATIVES_REGIME_ID: &str = "btc.cross.derivatives_regime";

const PERP_SPOT_BASIS_DOCUMENT: &str =
    include_str!("../../../definitions/btc/signals/btc.cross.perp_spot_basis@1.0.0.json");
const LIQUIDATION_RESPONSE_DOCUMENT: &str = include_str!(
    "../../../definitions/btc/signals/btc.cross.liquidation_vs_spot_response@1.0.0.json"
);
const DERIVATIVES_REGIME_DOCUMENT: &str =
    include_str!("../../../definitions/btc/signals/btc.cross.derivatives_regime@1.0.0.json");

const KRAKEN_USDT_REFERENCE_DEFINITION_JSON: &str =
    include_str!("../../../definitions/btc/sources/kraken.spot.usdt-usd.ticker@1.0.0.json");

const ONE_SECOND_NS: u64 = 1_000_000_000;
/// The basis-change lookback / warm-up window (spec §6.2): 60 s of uninterrupted
/// observation from the first ready reading.
const BASIS_WINDOW_NS: u64 = 60 * ONE_SECOND_NS;
/// The two liquidation-response horizons (spec §6.3): 5 s and 30 s.
const HORIZON_5S_NS: u64 = 5 * ONE_SECOND_NS;
const HORIZON_30S_NS: u64 = 30 * ONE_SECOND_NS;
/// The horizon reading must be within 10 s of the target instant (spec §6.3).
const HORIZON_TOLERANCE_NS: u64 = 10 * ONE_SECOND_NS;
/// Spot-ring retention: the longest horizon plus its tolerance, a little slack.
const SPOT_RING_RETENTION_NS: u64 = HORIZON_30S_NS + HORIZON_TOLERANCE_NS + ONE_SECOND_NS;
/// Basis-ring retention: the 60 s change window plus a little slack.
const BASIS_RING_RETENTION_NS: u64 = BASIS_WINDOW_NS + ONE_SECOND_NS;

// Thresholds (research defaults, spec §6).
fn flat_threshold_bps() -> Decimal {
    Decimal::from(2)
}
fn response_threshold_bps() -> Decimal {
    Decimal::from(5)
}
fn oi_threshold_pct() -> Decimal {
    // 0.05
    Decimal::new(5, 2)
}
const STRESS_WINDOW_MS: u64 = 300_000;

/// The embedded cross signal contract documents `(id, json)`, for the
/// definition-coverage guard (mirrors the venue processors' equivalents).
#[must_use]
pub fn cross_signal_contract_documents() -> Vec<(&'static str, &'static str)> {
    vec![
        (PERP_SPOT_BASIS_ID, PERP_SPOT_BASIS_DOCUMENT),
        (LIQUIDATION_RESPONSE_ID, LIQUIDATION_RESPONSE_DOCUMENT),
        (DERIVATIVES_REGIME_ID, DERIVATIVES_REGIME_DOCUMENT),
    ]
}

/// The cross lane's single derivatives venue (ruling E66): Bybit. The published
/// per-venue field prefix and the token stamped on the tracked cascade.
const VENUE_TOKEN: &str = "bybit";

/// The non-ready reason each Bybit leg carries when its upstream identity is not
/// yet ready. No `binance_*` reason can appear on this lane (E66).
const BASIS_MISSING_REASON: &str = "derivatives_constituents_unavailable";
const FUNDING_MISSING_REASON: &str = "funding_unavailable";
const OI_MISSING_REASON: &str = "open_interest_unavailable";

/// The Bybit derivatives leg's harvested value maps for this decision. A `None`
/// field means that upstream identity is not ready. Public within the crate so the
/// pure computations are unit-testable with a synthetic leg.
#[derive(Debug, Clone, Default)]
struct VenueLegs {
    basis: Option<BTreeMap<String, Value>>,
    funding: Option<BTreeMap<String, Value>>,
    open_interest: Option<BTreeMap<String, Value>>,
    liquidation: Option<BTreeMap<String, Value>>,
}

/// The spot reference as seen from one decision.
#[derive(Debug, Clone)]
struct SpotSnapshot {
    ready: bool,
    reference_price: Option<Decimal>,
    reference_price_raw: Option<Value>,
    max_input_age_ms: Option<u64>,
}

// --- pure field helpers ----------------------------------------------------

fn dec_field(values: &BTreeMap<String, Value>, key: &str) -> Result<Decimal, SpotSignalError> {
    let text = values
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| SpotSignalError::MissingField(key.to_owned()))?;
    Ok(parse_decimal_text(text)?)
}

fn str_field<'a>(
    values: &'a BTreeMap<String, Value>,
    key: &str,
) -> Result<&'a str, SpotSignalError> {
    values
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| SpotSignalError::MissingField(key.to_owned()))
}

fn carried(values: &BTreeMap<String, Value>, key: &str) -> Result<Value, SpotSignalError> {
    values
        .get(key)
        .cloned()
        .ok_or_else(|| SpotSignalError::MissingField(key.to_owned()))
}

/// One venue's basis pieces against the composite spot: absolute, bps, index gap.
fn mark_vs_spot(
    mark: Decimal,
    index: Decimal,
    reference: Decimal,
) -> Result<(Decimal, Decimal, Decimal), SpotSignalError> {
    let absolute = checked_sub(mark, reference)?;
    let bps = checked_mul(checked_div(absolute, reference)?, Decimal::from(10_000))?;
    let index_gap = checked_mul(
        checked_div(checked_sub(index, reference)?, reference)?,
        Decimal::from(10_000),
    )?;
    Ok((absolute, bps, index_gap))
}

// --- perp_spot_basis -------------------------------------------------------

/// The `contango` / `backwardation` / `flat` band (spec §6.2), at full precision.
fn basis_state(basis_bps: Decimal) -> Result<&'static str, SpotSignalError> {
    let threshold = flat_threshold_bps();
    Ok(if basis_bps > threshold {
        "contango"
    } else if basis_bps < checked_sub(Decimal::ZERO, threshold)? {
        "backwardation"
    } else {
        "flat"
    })
}

/// Build the ready `perp_spot_basis` values for the single Bybit venue (E66), and
/// the full-precision `basis_bps` the ring records. Callers only reach here once
/// the spot reference and Bybit's `mark_index_basis_state` are ready and the 60 s
/// warm-up has elapsed. `basis_change` is `basis_bps` now minus the earliest sample
/// in the causal 60 s window.
fn perp_spot_basis_values(
    spot: &SpotSnapshot,
    bybit_basis: &BTreeMap<String, Value>,
    basis_change: Decimal,
    produced_at: &str,
) -> Result<(BTreeMap<String, Value>, Decimal), SpotSignalError> {
    let reference_price = spot
        .reference_price
        .ok_or_else(|| SpotSignalError::State("spot reference price missing".to_owned()))?;
    let reference_price_raw = spot
        .reference_price_raw
        .clone()
        .ok_or_else(|| SpotSignalError::State("spot reference price value missing".to_owned()))?;

    let mut values = BTreeMap::new();
    values.insert("reference_price".to_owned(), reference_price_raw);

    let token = VENUE_TOKEN;
    let mark = dec_field(bybit_basis, "mark_price")?;
    let index = dec_field(bybit_basis, "index_price")?;
    let (absolute, basis_bps, index_gap) = mark_vs_spot(mark, index, reference_price)?;
    values.insert(
        format!("{token}_mark_price"),
        carried(bybit_basis, "mark_price")?,
    );
    values.insert(
        format!("{token}_index_price"),
        carried(bybit_basis, "index_price")?,
    );
    values.insert(
        format!("{token}_mark_vs_spot_absolute"),
        decimal_value(absolute),
    );
    values.insert(
        format!("{token}_mark_vs_spot_bps"),
        decimal_value(basis_bps),
    );
    values.insert(format!("{token}_index_gap_bps"), decimal_value(index_gap));
    // Carried verbatim: the venue's own mark-vs-index basis (factor carrying,
    // SIG-045). `mark_index_basis_state` publishes it as `basis_bps`.
    values.insert(
        format!("{token}_mark_index_bps"),
        carried(bybit_basis, "basis_bps")?,
    );

    let state = basis_state(basis_bps)?;
    values.insert("basis_bps".to_owned(), decimal_value(basis_bps));
    values.insert(
        "basis_change_bps_60s".to_owned(),
        decimal_value(basis_change),
    );
    values.insert("state".to_owned(), json!(state));
    values.insert(
        "flat_threshold_bps".to_owned(),
        decimal_value(flat_threshold_bps()),
    );
    values.insert("venue_count".to_owned(), json!(1_u8));
    values.insert(
        "max_input_age_ms".to_owned(),
        json!(spot.max_input_age_ms.unwrap_or(0)),
    );
    values.insert("window_close".to_owned(), json!(produced_at));
    Ok((values, basis_bps))
}

/// The outcome of one `perp_spot_basis` evaluation.
enum BasisEvaluation {
    Ready {
        values: BTreeMap<String, Value>,
        state: String,
    },
    NotReady(Vec<String>),
}

// --- liquidation_vs_spot_response ------------------------------------------

/// The bounded, tracked cascade (spec §6.3): exactly one at a time; a new cascade
/// replaces it and the replaced cascade's unresolved horizons are never revised.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TrackedCascade {
    burst_sequence: u64,
    burst_now_ns: u64,
    burst_time: String,
    burst_venue: String,
    burst_side: String,
    /// The summed `side_skew_60s` (as canonical text), published verbatim.
    burst_side_skew: String,
    burst_notional_usd: String,
    burst_venue_count: u8,
    feed_completeness: Value,
    reference_price_at_burst: Value,
    reference_price_at_burst_dec: String,
    expected_direction: String,
    response_bps_5s: Option<String>,
    response_bps_30s: Option<String>,
}

/// `longs_liquidated` / `shorts_liquidated` / `balanced` from the summed upstream
/// side skew. `btc.<v>.liquidation_pressure` publishes `side_skew_60s` as the
/// NORMALIZED `(long_notional − short_notional) / total` over the 60 s window,
/// where a buy-side liquidation is a LONG position being liquidated (Bybit
/// `allLiquidation` S=Buy, ruling E55). So a positive skew means longs dominated
/// the liquidations (`longs_liquidated`), a negative skew means shorts did
/// (`shorts_liquidated`), and exactly zero is `balanced`. The summed value stays
/// in [-1, 1] because every constituent skew does.
fn burst_side(side_skew: Decimal) -> &'static str {
    if side_skew > Decimal::ZERO {
        "longs_liquidated"
    } else if side_skew < Decimal::ZERO {
        "shorts_liquidated"
    } else {
        "balanced"
    }
}

fn expected_direction(side: &str) -> &'static str {
    match side {
        "longs_liquidated" => "down",
        "shorts_liquidated" => "up",
        _ => "none",
    }
}

/// Resolve `follow_through` / `fade` / `muted` / `unresolved` from the 30 s
/// response and the expected direction (spec §6.3).
fn follow_through(
    horizon_state: &str,
    expected: &str,
    response_30s: Option<Decimal>,
) -> Result<&'static str, SpotSignalError> {
    if horizon_state != "both_resolved" {
        return Ok("unresolved");
    }
    let response = response_30s.unwrap_or(Decimal::ZERO);
    let threshold = response_threshold_bps();
    let magnitude = response.abs();
    if magnitude < threshold {
        return Ok("muted");
    }
    let sign_matches = match expected {
        "up" => response > Decimal::ZERO,
        "down" => response < Decimal::ZERO,
        _ => false,
    };
    Ok(if sign_matches {
        "follow_through"
    } else {
        "fade"
    })
}

// --- derivatives_regime ----------------------------------------------------

/// The closed classification table (spec §6.4), evaluated in order, with the
/// liquidation stress override winning first.
fn classify_regime(
    stress_active: bool,
    basis_state: &str,
    funding_regime: &str,
    open_interest_regime: &str,
) -> &'static str {
    if stress_active {
        return "stress";
    }
    match (basis_state, funding_regime, open_interest_regime) {
        ("contango", "longs_pay", "rising") => "long_leverage_building",
        ("contango", "longs_pay", "falling") => "long_leverage_unwinding",
        ("backwardation", "shorts_pay", "rising") => "short_leverage_building",
        ("backwardation", "shorts_pay", "falling") => "short_leverage_unwinding",
        _ => "mixed",
    }
}

/// The open-interest sub-regime from the mean 1-minute OI delta percent.
fn open_interest_regime(oi_delta_pct: Decimal) -> Result<&'static str, SpotSignalError> {
    let threshold = oi_threshold_pct();
    Ok(if oi_delta_pct > threshold {
        "rising"
    } else if oi_delta_pct < checked_sub(Decimal::ZERO, threshold)? {
        "falling"
    } else {
        "flat"
    })
}

// --- checkpoint ------------------------------------------------------------

/// The cross lane's bounded resume state (spec §9 restart equivalence). The
/// rolling rings (basis history, spot history), the basis warm-up clock and the
/// tracked cascade are checkpointed (Audit M), so a resume restates the tracked
/// cascade and reproduces the horizon lookups rather than silently re-warming;
/// the embedded engines' durable state and the emitters' output lineage round-trip
/// too. Only the embedded engines' own rolling histories re-warm — they never feed
/// a restated cascade.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CrossCheckpointState {
    spot_reference: SpotReferenceCheckpointState,
    usdt_reference: UsdtReferenceCheckpoint,
    bybit_features: BTreeMap<String, SpotSignalFeatureCheckpoint>,
    emitters: BTreeMap<String, SpotSignalFeatureCheckpoint>,
    last_now_ns: Option<u64>,
    ever_ready: bool,
    // Audit M: the cross rolling state is durable so a resume reproduces identical
    // cross values (it restates the tracked cascade and reproduces the horizon
    // lookups) rather than silently re-warming. Decimals are canonical text, the
    // tree checkpoint convention. The embedded engines' own rolling histories are
    // still not persisted — they re-warm — but they never feed a restated cascade.
    basis_ring: Vec<(u64, String)>,
    spot_ring: Vec<(u64, String)>,
    basis_warm_since: Option<u64>,
    cascade: Option<TrackedCascade>,
    burst_sequence: u64,
    last_clustering_bybit: Option<String>,
}

fn ring_to_text(ring: &VecDeque<(u64, Decimal)>) -> Vec<(u64, String)> {
    ring.iter()
        .map(|(ns, d)| (*ns, canonical_decimal(*d)))
        .collect()
}

fn ring_from_text(text: &[(u64, String)]) -> Result<VecDeque<(u64, Decimal)>, LaneProcessorError> {
    text.iter()
        .map(|(ns, d)| {
            parse_decimal_text(d)
                .map(|dec| (*ns, dec))
                .map_err(|error| LaneProcessorError::InvalidState(error.to_string()))
        })
        .collect()
}

// --- the processor ---------------------------------------------------------

/// The cross lane processor. Embeds the spot reference, the Bybit derivatives
/// engine and the USDT reference over the raw sockets (no bus, E52), harvests
/// their value maps each interval and publishes the three `btc.cross.*` identities.
pub struct CrossProcessor {
    publisher_epoch: String,
    wall_start: DateTime<Utc>,
    spot_reference: SpotReferenceProcessor,
    bybit: VenueDerivativesEngine,
    usdt_reference: UsdtReference,
    basis_emitter: FeatureEmitter,
    liquidation_emitter: FeatureEmitter,
    regime_emitter: FeatureEmitter,

    // Rolling state (checkpointed and restored, Audit M — see CrossCheckpointState;
    // only the embedded engines' own histories re-warm on resume).
    /// Full-precision `basis_bps` samples, for `basis_change_bps_60s`.
    basis_ring: VecDeque<(u64, Decimal)>,
    /// Ready spot `reference_price` samples, for the liquidation horizons.
    spot_ring: VecDeque<(u64, Decimal)>,
    /// The instant the current uninterrupted ready-inputs run began, or `None` when
    /// the basis inputs are not all ready (spec §6.2 warm-up).
    basis_warm_since: Option<u64>,
    cascade: Option<TrackedCascade>,
    burst_sequence: u64,
    last_clustering_bybit: Option<String>,

    // Feed health / readiness.
    board: FeedHealthBoard,
    contracts: Vec<FeedContract>,
    live_streams: BTreeSet<String>,
    ever_ready: bool,
    observed_not_consumed: BTreeSet<String>,
    last_now_ns: Option<u64>,
}

impl CrossProcessor {
    /// Build the cross processor for a publisher epoch and the session wall clock.
    pub fn new(
        publisher_epoch: impl Into<String>,
        wall_start: DateTime<Utc>,
    ) -> Result<Self, SpotSignalError> {
        let publisher_epoch = publisher_epoch.into();
        let spot_reference = SpotReferenceProcessor::new(&publisher_epoch, wall_start)
            .map_err(|error| SpotSignalError::Definition(error.to_string()))?;
        let bybit = VenueDerivativesEngine::new(Venue::Bybit, publisher_epoch.clone(), wall_start)?;

        let basis_emitter = FeatureEmitter::from_contract(
            PERP_SPOT_BASIS_DOCUMENT,
            PERP_SPOT_BASIS_ID,
            CROSS_SIGNAL_VERSION,
        )?;
        let liquidation_emitter = FeatureEmitter::from_contract(
            LIQUIDATION_RESPONSE_DOCUMENT,
            LIQUIDATION_RESPONSE_ID,
            CROSS_SIGNAL_VERSION,
        )?;
        let regime_emitter = FeatureEmitter::from_contract(
            DERIVATIVES_REGIME_DOCUMENT,
            DERIVATIVES_REGIME_ID,
            CROSS_SIGNAL_VERSION,
        )?;

        // Feed-health contracts: the consumed BTC/USD spot tickers, the Bybit
        // derivatives sources, and the Kraken USDT/USD reference (shown as a
        // reference feed, never readiness-gating). The source definitions are parsed
        // and validated here (a build-time invariant) but not retained — the embedded
        // engines own event validation against their own definitions.
        let mut source_definitions = BTreeMap::new();
        let mut contracts = Vec::new();
        for (id, raw) in spot_source_documents() {
            insert_source(
                &mut source_definitions,
                &mut contracts,
                id,
                raw,
                spot_venue_of(id),
            )?;
        }
        for (id, raw) in bybit.source_documents() {
            insert_source(&mut source_definitions, &mut contracts, &id, &raw, "bybit")?;
        }
        insert_source(
            &mut source_definitions,
            &mut contracts,
            KRAKEN_USDT_TICKER_SOURCE_ID,
            KRAKEN_USDT_REFERENCE_DEFINITION_JSON,
            "kraken",
        )?;
        let board = FeedHealthBoard::new(contracts.clone(), 0);

        Ok(Self {
            publisher_epoch,
            wall_start,
            spot_reference,
            bybit,
            usdt_reference: UsdtReference::new_kraken(),
            basis_emitter,
            liquidation_emitter,
            regime_emitter,
            basis_ring: VecDeque::new(),
            spot_ring: VecDeque::new(),
            basis_warm_since: None,
            cascade: None,
            burst_sequence: 0,
            last_clustering_bybit: None,
            board,
            contracts,
            live_streams: BTreeSet::new(),
            ever_ready: false,
            observed_not_consumed: BTreeSet::new(),
            last_now_ns: None,
        })
    }

    /// The lane's source definition documents `(id, json)` — the raw sources it
    /// consumes; their bytes enter the contract fingerprint.
    fn source_documents(&self) -> Vec<LaneDefinitionDocument> {
        let mut documents: Vec<LaneDefinitionDocument> = spot_source_documents()
            .into_iter()
            .map(|(id, raw)| (id.to_owned(), raw.to_owned()))
            .collect();
        documents.extend(self.bybit.source_documents());
        documents.push((
            KRAKEN_USDT_TICKER_SOURCE_ID.to_owned(),
            KRAKEN_USDT_REFERENCE_DEFINITION_JSON.to_owned(),
        ));
        documents
    }

    fn signal_documents(&self) -> Vec<LaneDefinitionDocument> {
        cross_signal_contract_documents()
            .into_iter()
            .map(|(id, raw)| (id.to_owned(), raw.to_owned()))
            .collect()
    }

    /// Every source this processor routes to a handler: the two spot BTC/USD
    /// tickers, every Bybit derivatives source, and the Kraken USDT/USD reference.
    fn consumed(&self) -> BTreeSet<String> {
        let mut sources = BTreeSet::new();
        sources.insert(crate::spot_reference::COINBASE_TICKER_SOURCE_ID.to_owned());
        sources.insert(crate::spot_reference::KRAKEN_TICKER_SOURCE_ID.to_owned());
        for source in self.bybit.consumed_sources() {
            sources.insert(source.to_owned());
        }
        sources.insert(KRAKEN_USDT_TICKER_SOURCE_ID.to_owned());
        sources
    }

    /// The sources a declared stream must produce for a non-unavailable identity to
    /// reach ready (F1 misconfiguration guard): the two spot tickers plus the Bybit
    /// required set. The Kraken USDT/USD reference is deliberately excluded — it is
    /// feed-health only and never gates readiness (E47(e)). Bybit is the only
    /// derivatives venue on this lane (E66).
    fn required(&self) -> BTreeSet<String> {
        let mut sources = BTreeSet::new();
        sources.insert(crate::spot_reference::COINBASE_TICKER_SOURCE_ID.to_owned());
        sources.insert(crate::spot_reference::KRAKEN_TICKER_SOURCE_ID.to_owned());
        for source in self.bybit.required_sources() {
            sources.insert(source.to_owned());
        }
        sources
    }

    fn observe_time(&mut self, now_ns: u64) -> Result<(), LaneProcessorError> {
        if self.last_now_ns.is_some_and(|previous| now_ns < previous) {
            return Err(LaneProcessorError::Computation(
                "cross lane logical time regressed".to_owned(),
            ));
        }
        self.last_now_ns = Some(now_ns);
        Ok(())
    }

    /// Route one admitted event to the spot reference, the Bybit derivatives engine
    /// or the USDT reference by its source id. A source neither owned nor declared
    /// observed-not-consumed is a genuine misconfiguration and aborts the lane (F1).
    fn ingest_event(&mut self, event: &SourceEvent, now_ns: u64) -> Result<(), SpotSignalError> {
        let source = event.source_id.as_str();
        if self.observed_not_consumed.contains(source) {
            self.board.note_ignored(source, now_ns);
            return Ok(());
        }
        self.board.observe_event(source, now_ns);
        if self.usdt_reference.owns_source(source) {
            // G1: a crossed / non-positive reference quote is feed-health telemetry,
            // never a lane error — keep the prior rate (a stale prior is caught at
            // decision time as `usdt_reference_stale`). It is not market data, so it
            // does not mark a driver stream live.
            if self.usdt_reference.observe(event, now_ns).is_err() {
                self.board.note_reference_rejected(now_ns);
            }
            return Ok(());
        }
        if SpotReferenceProcessor::accepts(source) {
            // Harvested on the interval via `advance_time`; the per-event update is
            // dropped (the cross lane publishes only `btc.cross.*`).
            let _ = self
                .spot_reference
                .ingest(event, now_ns, now_ns)
                .map_err(|error| SpotSignalError::State(error.to_string()))?;
            return Ok(());
        }
        if self.bybit.owns_source(source) {
            self.bybit.ingest(event, now_ns)?;
            return Ok(());
        }
        Err(SpotSignalError::UnsupportedSource(source.to_owned()))
    }

    /// Harvest the spot reference and the Bybit derivatives legs, then evaluate and
    /// emit the three cross identities. Called on every interval tick.
    fn emit_interval(&mut self, now_ns: u64) -> Result<Vec<SignalUpdate>, SpotSignalError> {
        self.board.observe_tick(now_ns);
        let produced_at = logical_timestamp_text(self.wall_start, now_ns)?;

        // Spot reference at this decision (always emits under advance_time).
        let spot_update = self
            .spot_reference
            .advance_time(now_ns)
            .map_err(|error| SpotSignalError::State(error.to_string()))?
            .ok_or_else(|| {
                SpotSignalError::State("spot reference produced no decision".to_owned())
            })?;
        let spot_ready = spot_update.quality.state == QualityState::Ready;
        let reference_price_raw = spot_update.values.get("reference_price").cloned();
        let reference_price = reference_price_raw
            .as_ref()
            .and_then(Value::as_str)
            .map(parse_decimal_text)
            .transpose()?;
        let max_input_age_ms = spot_update
            .values
            .get("max_input_age_ms")
            .and_then(Value::as_u64);
        let spot = SpotSnapshot {
            ready: spot_ready,
            reference_price,
            reference_price_raw,
            max_input_age_ms,
        };
        if spot.ready
            && let Some(price) = spot.reference_price
        {
            push_ring(&mut self.spot_ring, now_ns, price, SPOT_RING_RETENTION_NS);
        }

        // Bybit derivatives legs (harvested, not published).
        let reference = self.usdt_reference.snapshot(now_ns);
        let bybit_updates = self.bybit.emit(now_ns, reference)?;
        let bybit_legs = harvest_bybit_legs(&bybit_updates);

        let mut updates = Vec::with_capacity(3);

        // 1. perp_spot_basis. A computation error degrades THIS identity with an
        // honest reason instead of aborting the lane — the other two still publish.
        let basis = self
            .evaluate_basis(&spot, &bybit_legs, now_ns, &produced_at)
            .unwrap_or_else(|_| {
                BasisEvaluation::NotReady(vec!["derivatives_constituents_unavailable".to_owned()])
            });
        let (basis_values_opt, basis_reasons, basis_ready_state) = match &basis {
            BasisEvaluation::Ready { values, state, .. } => {
                (Some(values.clone()), Vec::new(), Some(state.clone()))
            }
            BasisEvaluation::NotReady(reasons) => (None, reasons.clone(), None),
        };
        updates.push(self.basis_emitter.emit(
            &self.publisher_epoch,
            &produced_at,
            produced_at.clone(),
            basis_values_opt,
            basis_reasons,
        )?);

        // 2. liquidation_vs_spot_response.
        let liquidation = self
            .evaluate_liquidation(&spot, &bybit_legs, now_ns, &produced_at)
            .unwrap_or_else(|_| {
                LiquidationEvaluation::NotReady(vec![
                    "derivatives_constituents_unavailable".to_owned(),
                ])
            });
        let (liq_values_opt, liq_reasons, liq_follow_through, liq_time_since) = match &liquidation {
            LiquidationEvaluation::Ready {
                values,
                follow_through,
                time_since_burst_ms,
            } => (
                Some(values.clone()),
                Vec::new(),
                Some(follow_through.clone()),
                Some(*time_since_burst_ms),
            ),
            LiquidationEvaluation::NotReady(reasons) => (None, reasons.clone(), None, None),
        };
        updates.push(self.liquidation_emitter.emit(
            &self.publisher_epoch,
            &produced_at,
            produced_at.clone(),
            liq_values_opt,
            liq_reasons,
        )?);

        // 3. derivatives_regime — reads the two above computed THIS interval.
        let regime = self
            .evaluate_regime(
                &spot,
                &bybit_legs,
                basis_ready_state.as_deref(),
                &basis,
                liq_follow_through.as_deref(),
                liq_time_since,
                &produced_at,
            )
            .unwrap_or_else(|_| {
                RegimeEvaluation::NotReady(vec!["derivatives_constituents_unavailable".to_owned()])
            });
        let (regime_values_opt, regime_reasons) = match regime {
            RegimeEvaluation::Ready(values) => (Some(values), Vec::new()),
            RegimeEvaluation::NotReady(reasons) => (None, reasons),
        };
        updates.push(self.regime_emitter.emit(
            &self.publisher_epoch,
            &produced_at,
            produced_at.clone(),
            regime_values_opt,
            regime_reasons,
        )?);

        if updates
            .iter()
            .any(|u| u.quality.state == QualityState::Ready)
        {
            self.ever_ready = true;
        }
        Ok(updates)
    }

    fn evaluate_basis(
        &mut self,
        spot: &SpotSnapshot,
        bybit: &VenueLegs,
        now_ns: u64,
        produced_at: &str,
    ) -> Result<BasisEvaluation, SpotSignalError> {
        let mut reasons = Vec::new();
        if !spot.ready {
            reasons.push("reference_unavailable".to_owned());
        }
        if bybit.basis.is_none() {
            reasons.push(BASIS_MISSING_REASON.to_owned());
        }
        if !reasons.is_empty() {
            // Inputs not all ready: the warm-up run is interrupted.
            self.basis_warm_since = None;
            return Ok(BasisEvaluation::NotReady(reasons));
        }

        // Inputs are all ready: manage the 60 s warm-up clock (spec §6.2). We can
        // compute the Bybit basis now (leg present), which the ring records.
        let bybit_basis = bybit.basis.as_ref().expect("bybit basis present");
        let reference_price = spot
            .reference_price
            .ok_or_else(|| SpotSignalError::State("ready spot without price".to_owned()))?;
        let basis_bps = mark_vs_spot(
            dec_field(bybit_basis, "mark_price")?,
            dec_field(bybit_basis, "index_price")?,
            reference_price,
        )?
        .1;

        let warm_since = *self.basis_warm_since.get_or_insert(now_ns);
        let warmed = now_ns.saturating_sub(warm_since) >= BASIS_WINDOW_NS;
        // The ring is fed on every ready-inputs interval (whether or not warmed), so
        // the change window is populated by the time the warm-up completes.
        push_ring(
            &mut self.basis_ring,
            now_ns,
            basis_bps,
            BASIS_RING_RETENTION_NS,
        );
        if !warmed {
            return Ok(BasisEvaluation::NotReady(vec![
                "basis_history_warming_up".to_owned(),
            ]));
        }

        // basis_change_bps_60s: basis now minus the earliest sample in the causal
        // (window_close − 60 s, window_close] receipt-time window.
        let cutoff = now_ns.saturating_sub(BASIS_WINDOW_NS);
        let earliest = self
            .basis_ring
            .iter()
            .find(|(sample_ns, _)| *sample_ns > cutoff)
            .map(|(_, value)| *value)
            .unwrap_or(basis_bps);
        let basis_change = checked_sub(basis_bps, earliest)?;

        let (values, basis_bps) =
            perp_spot_basis_values(spot, bybit_basis, basis_change, produced_at)?;
        let state = basis_state(basis_bps)?.to_owned();
        Ok(BasisEvaluation::Ready { values, state })
    }

    fn evaluate_liquidation(
        &mut self,
        spot: &SpotSnapshot,
        bybit: &VenueLegs,
        now_ns: u64,
        produced_at: &str,
    ) -> Result<LiquidationEvaluation, SpotSignalError> {
        // Detect a transition INTO cascade on the Bybit venue (the only derivatives
        // venue on this lane, E66); its cascade is what can trigger.
        let bybit_clustering = bybit
            .liquidation
            .as_ref()
            .map(|values| str_field(values, "clustering_state").map(str::to_owned))
            .transpose()?;
        let entered_cascade = matches!(bybit_clustering.as_deref(), Some("cascade"))
            && self.last_clustering_bybit.as_deref() != Some("cascade");
        self.last_clustering_bybit = bybit_clustering;

        if entered_cascade
            && spot.ready
            && let (Some(price), Some(price_raw)) =
                (spot.reference_price, spot.reference_price_raw.clone())
        {
            let liquidation = bybit
                .liquidation
                .as_ref()
                .expect("bybit cascade has values");
            let side_skew = dec_field(liquidation, "side_skew_60s")?;
            let notional = dec_field(liquidation, "notional_usd_60s")?;
            let side = burst_side(side_skew);
            self.burst_sequence = self.burst_sequence.saturating_add(1);
            self.cascade = Some(TrackedCascade {
                burst_sequence: self.burst_sequence,
                burst_now_ns: now_ns,
                burst_time: produced_at.to_owned(),
                burst_venue: "bybit".to_owned(),
                burst_side: side.to_owned(),
                burst_side_skew: canonical_decimal(side_skew),
                burst_notional_usd: canonical_decimal(notional),
                burst_venue_count: 1,
                feed_completeness: carried(liquidation, "feed_completeness")?,
                reference_price_at_burst: price_raw,
                reference_price_at_burst_dec: canonical_decimal(price),
                expected_direction: expected_direction(side).to_owned(),
                response_bps_5s: None,
                response_bps_30s: None,
            });
        }

        let Some(mut cascade) = self.cascade.clone() else {
            return Ok(LiquidationEvaluation::NotReady(vec![
                "liquidation_burst_not_observed".to_owned(),
            ]));
        };

        // Resolve horizons whose target instant has passed, once each.
        let burst_price = parse_decimal_text(&cascade.reference_price_at_burst_dec)?;
        if cascade.response_bps_5s.is_none()
            && now_ns >= cascade.burst_now_ns.saturating_add(HORIZON_5S_NS)
        {
            // A None sample keeps the horizon UNRESOLVED (it retries next interval),
            // rather than fabricating a resolved zero.
            cascade.response_bps_5s =
                self.resolve_horizon(cascade.burst_now_ns, HORIZON_5S_NS, burst_price)?;
        }
        if cascade.response_bps_30s.is_none()
            && now_ns >= cascade.burst_now_ns.saturating_add(HORIZON_30S_NS)
        {
            cascade.response_bps_30s =
                self.resolve_horizon(cascade.burst_now_ns, HORIZON_30S_NS, burst_price)?;
        }
        self.cascade = Some(cascade.clone());

        let values = liquidation_values(&cascade, spot, now_ns, produced_at)?;
        let time_since_burst_ms = now_ns.saturating_sub(cascade.burst_now_ns) / 1_000_000;
        let follow_through = values
            .get("follow_through")
            .and_then(Value::as_str)
            .unwrap_or("unresolved")
            .to_owned();
        Ok(LiquidationEvaluation::Ready {
            values,
            follow_through,
            time_since_burst_ms,
        })
    }

    /// The spot response at a horizon: the ready reference decision with the
    /// greatest logical time at or before `burst + horizon` and within 10 s of it,
    /// as bps against the burst price. An unresolved horizon (no qualifying sample)
    /// publishes zero — but this is only called once the target instant has passed,
    /// and the spot ring holds the recent ready readings on the lane's decision clock
    /// (the axis `burst_time` and `reference_time` share here).
    fn resolve_horizon(
        &self,
        burst_now_ns: u64,
        horizon_ns: u64,
        burst_price: Decimal,
    ) -> Result<Option<String>, SpotSignalError> {
        let target = burst_now_ns.saturating_add(horizon_ns);
        let lower = target.saturating_sub(HORIZON_TOLERANCE_NS);
        let sample = self
            .spot_ring
            .iter()
            .filter(|(sample_ns, _)| *sample_ns <= target && *sample_ns >= lower)
            .max_by_key(|(sample_ns, _)| *sample_ns)
            .map(|(_, price)| *price);
        // No qualifying reading, or a non-positive burst price: the horizon stays
        // UNRESOLVED (horizon_state is the authoritative field), and the published
        // response field carries zero only because the horizon is unresolved, not
        // because a real zero move was measured.
        let (Some(price), true) = (sample, burst_price > Decimal::ZERO) else {
            return Ok(None);
        };
        let response = checked_mul(
            checked_div(checked_sub(price, burst_price)?, burst_price)?,
            Decimal::from(10_000),
        )?;
        Ok(Some(canonical_decimal(response)))
    }

    #[allow(clippy::too_many_arguments)]
    fn evaluate_regime(
        &self,
        spot: &SpotSnapshot,
        bybit: &VenueLegs,
        basis_ready_state: Option<&str>,
        basis: &BasisEvaluation,
        liquidation_follow_through: Option<&str>,
        liquidation_time_since_ms: Option<u64>,
        produced_at: &str,
    ) -> Result<RegimeEvaluation, SpotSignalError> {
        let mut reasons = Vec::new();
        if basis_ready_state.is_none() {
            reasons.push("derivatives_constituents_unavailable".to_owned());
        }
        if bybit.funding.is_none() {
            reasons.push(FUNDING_MISSING_REASON.to_owned());
        }
        if bybit.open_interest.is_none() {
            reasons.push(OI_MISSING_REASON.to_owned());
        }
        if !reasons.is_empty() {
            return Ok(RegimeEvaluation::NotReady(reasons));
        }

        let BasisEvaluation::Ready {
            values: basis_values,
            ..
        } = basis
        else {
            return Ok(RegimeEvaluation::NotReady(vec![
                "derivatives_constituents_unavailable".to_owned(),
            ]));
        };
        let basis_state = basis_ready_state
            .expect("basis ready state present")
            .to_owned();
        let basis_bps = carried(basis_values, "basis_bps")?;

        let bybit_funding = bybit.funding.as_ref().expect("bybit funding present");
        let bybit_oi = bybit.open_interest.as_ref().expect("bybit oi present");

        let funding_rate = dec_field(bybit_funding, "funding_rate")?;
        let funding_annualised = dec_field(bybit_funding, "annualised_rate_bps")?;
        // Single venue (E66): the funding regime is the Bybit sign_regime verbatim;
        // there is no cross-venue disagreement, so `mixed` can never arise here.
        let funding_regime = str_field(bybit_funding, "sign_regime")?.to_owned();
        let oi_delta = dec_field(bybit_oi, "delta_pct_1m")?;
        let oi_regime = open_interest_regime(oi_delta)?;

        let stress_active = matches!(liquidation_follow_through, Some("follow_through"))
            && liquidation_time_since_ms.is_some_and(|ms| ms <= STRESS_WINDOW_MS);
        let regime = classify_regime(stress_active, &basis_state, &funding_regime, oi_regime);

        let mut values = BTreeMap::new();
        values.insert("regime".to_owned(), json!(regime));
        values.insert("basis_state".to_owned(), json!(basis_state));
        values.insert("funding_regime".to_owned(), json!(funding_regime));
        values.insert("open_interest_regime".to_owned(), json!(oi_regime));
        values.insert("basis_bps".to_owned(), basis_bps);
        values.insert("funding_rate".to_owned(), decimal_value(funding_rate));
        values.insert(
            "funding_annualised_bps".to_owned(),
            decimal_value(funding_annualised),
        );
        values.insert("oi_delta_pct_1m".to_owned(), decimal_value(oi_delta));
        values.insert("stress_active".to_owned(), json!(stress_active));
        values.insert("stress_window_ms".to_owned(), json!(STRESS_WINDOW_MS));
        values.insert(
            "oi_threshold_pct".to_owned(),
            decimal_value(oi_threshold_pct()),
        );
        values.insert("venue_count".to_owned(), json!(1_u8));
        values.insert(
            "max_input_age_ms".to_owned(),
            json!(spot.max_input_age_ms.unwrap_or(0)),
        );
        values.insert("observation_time".to_owned(), json!(produced_at));
        Ok(RegimeEvaluation::Ready(values))
    }

    fn health(&self) -> ProducerHealth {
        let required: BTreeSet<&str> = REQUIRED_STREAM_LABELS.into_iter().collect();
        let live: BTreeSet<&str> = self.live_streams.iter().map(String::as_str).collect();
        if required.is_subset(&live) {
            return ProducerHealth {
                status: ProducerStatus::Ready,
                reasons: Vec::new(),
            };
        }
        let mut reasons: Vec<String> = required
            .difference(&live)
            .map(|label| format!("{label}_unavailable"))
            .collect();
        reasons.sort();
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
    ) -> Result<(), SpotSignalError> {
        let is_reference = stream == KRAKEN_USDT_REFERENCE_LABEL;
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
            }
            // A closed market socket is a causal break for the embedded engines.
            match stream {
                COINBASE_CORE_LABEL => {
                    self.spot_reference
                        .mark_unavailable(Venue::Coinbase, UnavailableReason::Disconnected, now_ns)
                        .map_err(|error| SpotSignalError::State(error.to_string()))?;
                }
                KRAKEN_CORE_LABEL => {
                    self.spot_reference
                        .mark_unavailable(Venue::Kraken, UnavailableReason::Disconnected, now_ns)
                        .map_err(|error| SpotSignalError::State(error.to_string()))?;
                }
                BYBIT_LINEAR_LABEL | BYBIT_LINEAR_REST_LABEL => self.bybit.reset(),
                _ => {}
            }
        }
        Ok(())
    }

    fn checkpoint_value(&self) -> serde_json::Value {
        let mut emitters = BTreeMap::new();
        emitters.insert(
            PERP_SPOT_BASIS_ID.to_owned(),
            self.basis_emitter.checkpoint(),
        );
        emitters.insert(
            LIQUIDATION_RESPONSE_ID.to_owned(),
            self.liquidation_emitter.checkpoint(),
        );
        emitters.insert(
            DERIVATIVES_REGIME_ID.to_owned(),
            self.regime_emitter.checkpoint(),
        );
        serde_json::to_value(CrossCheckpointState {
            spot_reference: self.spot_reference.checkpoint_state(),
            usdt_reference: self.usdt_reference.checkpoint(),
            bybit_features: self.bybit.checkpoint_features(),
            emitters,
            last_now_ns: self.last_now_ns,
            ever_ready: self.ever_ready,
            basis_ring: ring_to_text(&self.basis_ring),
            spot_ring: ring_to_text(&self.spot_ring),
            basis_warm_since: self.basis_warm_since,
            cascade: self.cascade.clone(),
            burst_sequence: self.burst_sequence,
            last_clustering_bybit: self.last_clustering_bybit.clone(),
        })
        .expect("cross checkpoint state serializes")
    }

    fn restore_value(&mut self, state: serde_json::Value) -> Result<(), LaneProcessorError> {
        let state: CrossCheckpointState = serde_json::from_value(state)
            .map_err(|error| LaneProcessorError::InvalidState(error.to_string()))?;
        // The embedded engines' durable state round-trips: the spot reference under
        // this run's (new) publisher epoch, the USDT reference, and the Bybit and
        // cross emitters' output lineage. The cross rolling rings, the basis warm-up
        // clock and the tracked cascade round-trip too (restored below, Audit M), so
        // the resume restates the tracked cascade; only the embedded engines' own
        // histories re-warm under the new epoch.
        self.spot_reference = SpotReferenceProcessor::restore(
            self.publisher_epoch.clone(),
            self.wall_start,
            state.spot_reference,
        )
        .map_err(|error| LaneProcessorError::InvalidState(error.to_string()))?;
        self.usdt_reference = UsdtReference::new_kraken();
        self.usdt_reference.restore(&state.usdt_reference);
        self.bybit
            .restore_features(&state.bybit_features)
            .map_err(|error| LaneProcessorError::InvalidState(error.to_string()))?;
        restore_emitter(&mut self.basis_emitter, &state.emitters, PERP_SPOT_BASIS_ID)?;
        restore_emitter(
            &mut self.liquidation_emitter,
            &state.emitters,
            LIQUIDATION_RESPONSE_ID,
        )?;
        restore_emitter(
            &mut self.regime_emitter,
            &state.emitters,
            DERIVATIVES_REGIME_ID,
        )?;
        self.last_now_ns = state.last_now_ns;
        self.ever_ready = state.ever_ready;
        // Audit M: the cross rolling state is durable, so a resume restates the
        // tracked cascade and reproduces the horizon lookups — restored cross
        // values equal the pre-checkpoint ones. The embedded engines' own rolling
        // histories still re-warm (they never feed a restated cascade).
        self.basis_ring = ring_from_text(&state.basis_ring)?;
        self.spot_ring = ring_from_text(&state.spot_ring)?;
        self.basis_warm_since = state.basis_warm_since;
        self.cascade = state.cascade;
        self.burst_sequence = state.burst_sequence;
        self.last_clustering_bybit = state.last_clustering_bybit;
        self.live_streams.clear();
        Ok(())
    }
}

fn restore_emitter(
    emitter: &mut FeatureEmitter,
    emitters: &BTreeMap<String, SpotSignalFeatureCheckpoint>,
    id: &str,
) -> Result<(), LaneProcessorError> {
    let checkpoint = emitters.get(id).ok_or_else(|| {
        LaneProcessorError::InvalidState(format!("checkpoint missing emitter {id}"))
    })?;
    emitter.restore(checkpoint);
    Ok(())
}

enum LiquidationEvaluation {
    Ready {
        values: BTreeMap<String, Value>,
        follow_through: String,
        time_since_burst_ms: u64,
    },
    NotReady(Vec<String>),
}

enum RegimeEvaluation {
    Ready(BTreeMap<String, Value>),
    NotReady(Vec<String>),
}

/// Build the ready `liquidation_vs_spot_response` values for the tracked cascade.
fn liquidation_values(
    cascade: &TrackedCascade,
    spot: &SpotSnapshot,
    now_ns: u64,
    produced_at: &str,
) -> Result<BTreeMap<String, Value>, SpotSignalError> {
    let five_resolved = cascade.response_bps_5s.is_some();
    let thirty_resolved = cascade.response_bps_30s.is_some();
    let horizon_state = match (five_resolved, thirty_resolved) {
        (true, true) => "both_resolved",
        (true, false) => "five_resolved",
        _ => "none_resolved",
    };
    let response_5s = cascade
        .response_bps_5s
        .as_deref()
        .map(parse_decimal_text)
        .transpose()?;
    let response_30s = cascade
        .response_bps_30s
        .as_deref()
        .map(parse_decimal_text)
        .transpose()?;
    let follow = follow_through(horizon_state, &cascade.expected_direction, response_30s)?;
    let time_since_burst_ms = now_ns.saturating_sub(cascade.burst_now_ns) / 1_000_000;

    let mut values = BTreeMap::new();
    values.insert("burst_sequence".to_owned(), json!(cascade.burst_sequence));
    values.insert("burst_time".to_owned(), json!(cascade.burst_time));
    values.insert("burst_venue".to_owned(), json!(cascade.burst_venue));
    values.insert("burst_side".to_owned(), json!(cascade.burst_side));
    values.insert(
        "burst_notional_usd".to_owned(),
        decimal_value(parse_decimal_text(&cascade.burst_notional_usd)?),
    );
    values.insert(
        "burst_side_skew_60s".to_owned(),
        decimal_value(parse_decimal_text(&cascade.burst_side_skew)?),
    );
    values.insert(
        "burst_venue_count".to_owned(),
        json!(cascade.burst_venue_count),
    );
    values.insert(
        "feed_completeness".to_owned(),
        cascade.feed_completeness.clone(),
    );
    values.insert(
        "reference_price_at_burst".to_owned(),
        cascade.reference_price_at_burst.clone(),
    );
    values.insert(
        "response_bps_5s".to_owned(),
        decimal_value(response_5s.unwrap_or(Decimal::ZERO)),
    );
    values.insert(
        "response_bps_30s".to_owned(),
        decimal_value(response_30s.unwrap_or(Decimal::ZERO)),
    );
    values.insert("horizon_state".to_owned(), json!(horizon_state));
    values.insert(
        "expected_direction".to_owned(),
        json!(cascade.expected_direction),
    );
    values.insert("follow_through".to_owned(), json!(follow));
    values.insert(
        "response_threshold_bps".to_owned(),
        decimal_value(response_threshold_bps()),
    );
    values.insert("time_since_burst_ms".to_owned(), json!(time_since_burst_ms));
    values.insert(
        "max_input_age_ms".to_owned(),
        json!(spot.max_input_age_ms.unwrap_or(0)),
    );
    values.insert("observation_time".to_owned(), json!(produced_at));
    Ok(values)
}

/// Harvest the ready value maps from the Bybit derivatives engine's dropped
/// updates into the leg struct the cross computations read.
fn harvest_bybit_legs(updates: &[SignalUpdate]) -> VenueLegs {
    let mut legs = VenueLegs::default();
    for update in updates {
        if update.quality.state != QualityState::Ready {
            continue;
        }
        let values = update.values.clone();
        match update.signal_id.as_str() {
            "btc.bybit.mark_index_basis_state" => legs.basis = Some(values),
            "btc.bybit.funding_state" => legs.funding = Some(values),
            "btc.bybit.open_interest_delta" => legs.open_interest = Some(values),
            "btc.bybit.liquidation_pressure" => legs.liquidation = Some(values),
            _ => {}
        }
    }
    legs
}

fn push_ring(ring: &mut VecDeque<(u64, Decimal)>, now_ns: u64, value: Decimal, retention_ns: u64) {
    ring.push_back((now_ns, value));
    let cutoff = now_ns.saturating_sub(retention_ns);
    while ring
        .front()
        .is_some_and(|(sample_ns, _)| *sample_ns < cutoff)
    {
        ring.pop_front();
    }
}

/// A decimal as canonical (normalized) text, for internal round-tripping.
fn canonical_decimal(value: Decimal) -> String {
    value.normalize().to_string()
}

/// The two BTC/USD spot ticker source documents the spot reference consumes.
fn spot_source_documents() -> [(&'static str, &'static str); 2] {
    let docs = crate::spot_reference::contract_documents();
    // contract_documents() = [(spot_reference signal, ..), (coinbase source, ..),
    // (kraken source, ..)]; we take the two source docs.
    [docs[1], docs[2]]
}

fn spot_venue_of(source_id: &str) -> &'static str {
    if source_id.starts_with("coinbase") {
        "coinbase"
    } else {
        "kraken"
    }
}

fn insert_source(
    definitions: &mut BTreeMap<String, SourceDefinition>,
    contracts: &mut Vec<FeedContract>,
    id: &str,
    raw: &str,
    venue: &str,
) -> Result<(), SpotSignalError> {
    let definition: SourceDefinition = serde_json::from_str(raw).map_err(|error| {
        SpotSignalError::Definition(format!("invalid {id} source JSON: {error}"))
    })?;
    definition
        .validate()
        .map_err(|error| SpotSignalError::Definition(format!("invalid {id} source: {error}")))?;
    contracts.push(FeedContract {
        source_id: id.to_owned(),
        venue: venue.to_owned(),
        expected_heartbeat_ms: definition.expected_heartbeat_ms,
        cadence: FeedCadence::EventDriven,
        liveness_bound_override_ms: None,
    });
    definitions.insert(id.to_owned(), definition);
    Ok(())
}

fn computation(error: SpotSignalError) -> LaneProcessorError {
    LaneProcessorError::Computation(error.to_string())
}

impl LaneProcessor for CrossProcessor {
    fn signal_set_id(&self) -> &str {
        CROSS_SIGNAL_SET_ID
    }

    fn signal_set_version(&self) -> &str {
        CROSS_SIGNAL_SET_VERSION
    }

    fn signal_count(&self) -> usize {
        3
    }

    fn venues(&self) -> &'static [&'static str] {
        CROSS_VENUES
    }

    fn source_definitions(&self) -> Vec<LaneDefinitionDocument> {
        self.source_documents()
    }

    fn signal_definitions(&self) -> Vec<LaneDefinitionDocument> {
        self.signal_documents()
    }

    fn consumed_sources(&self) -> Option<BTreeSet<String>> {
        Some(self.consumed())
    }

    fn required_sources(&self) -> BTreeSet<String> {
        self.required()
    }

    fn set_observed_not_consumed(&mut self, sources: BTreeSet<String>) {
        self.observed_not_consumed = sources;
    }

    fn feed_contracts(&self) -> Vec<FeedContract> {
        self.contracts.clone()
    }

    fn on_events(
        &mut self,
        _stream: &str,
        events: &[SourceEvent],
        clock: &LaneClock,
    ) -> Result<Vec<SignalUpdate>, LaneProcessorError> {
        let now_ns = clock.now_ns;
        self.observe_time(now_ns)?;
        for event in events {
            // A market-data event proves its driver stream live for readiness (the
            // reference frames route through `ingest_event` without marking a
            // stream live — they update the USDT rate and feed health only).
            if !self.usdt_reference.owns_source(&event.source_id)
                && !self.observed_not_consumed.contains(&event.source_id)
            {
                self.live_streams
                    .insert(stream_label_for(&event.source_id).to_owned());
            }
            self.ingest_event(event, now_ns).map_err(computation)?;
        }
        Ok(Vec::new())
    }

    fn on_connectivity(
        &mut self,
        stream: &str,
        connected: bool,
        clock: &LaneClock,
    ) -> Result<Vec<SignalUpdate>, LaneProcessorError> {
        let now_ns = clock.now_ns;
        self.observe_time(now_ns)?;
        self.connectivity(stream, connected, now_ns)
            .map_err(computation)?;
        Ok(Vec::new())
    }

    fn on_interval(&mut self, clock: &LaneClock) -> Result<Vec<SignalUpdate>, LaneProcessorError> {
        let now_ns = clock.now_ns;
        self.observe_time(now_ns)?;
        self.emit_interval(now_ns).map_err(computation)
    }

    fn reset_output_sequences(&mut self) {
        // C5 Design B: this lane publishes exactly the three `btc.cross.*` identities
        // (E52), one per emitter, so a rotation restarts each emitter's per-identity
        // decision sequence at zero for the fresh output epoch. The embedded spot /
        // Bybit-derivatives engines and the USDT reference are harvested for values
        // here, never published on this lane, so they own no decision lineage to
        // reset; the warm rings, engine state and references carry across untouched.
        self.basis_emitter.reset_output_sequence();
        self.liquidation_emitter.reset_output_sequence();
        self.regime_emitter.reset_output_sequence();
    }

    fn producer_health(&self) -> ProducerHealth {
        self.health()
    }

    fn feed_health_snapshot(&self) -> FeedHealthSnapshot {
        self.board.snapshot()
    }

    fn feed_health_emission_revision(&self) -> u64 {
        self.board.emission_revision()
    }

    fn checkpoint_state(&self) -> serde_json::Value {
        self.checkpoint_value()
    }

    fn restore_state(&mut self, state: serde_json::Value) -> Result<(), LaneProcessorError> {
        self.restore_value(state)
    }
}

/// Map a consumed source id onto the stream label that carries it, for readiness
/// (a stream producing a consumed event is live). Bybit derivatives sources ride
/// the linear socket; the two spot tickers ride their core sockets.
fn stream_label_for(source_id: &str) -> &'static str {
    if source_id.starts_with("coinbase") {
        COINBASE_CORE_LABEL
    } else if source_id.starts_with("bybit") {
        BYBIT_LINEAR_LABEL
    } else {
        // kraken.spot.btc-usd.ticker rides the kraken core socket.
        KRAKEN_CORE_LABEL
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dec(text: &str) -> Decimal {
        parse_decimal_text(text).expect("decimal")
    }

    fn basis_map(mark: &str, index: &str, basis_bps: &str) -> BTreeMap<String, Value> {
        BTreeMap::from([
            ("mark_price".to_owned(), json!(mark)),
            ("index_price".to_owned(), json!(index)),
            ("basis_bps".to_owned(), json!(basis_bps)),
        ])
    }

    fn spot_snapshot(reference: &str) -> SpotSnapshot {
        SpotSnapshot {
            ready: true,
            reference_price: Some(dec(reference)),
            reference_price_raw: Some(json!(reference)),
            max_input_age_ms: Some(120),
        }
    }

    #[test]
    fn perp_spot_basis_single_venue_hand_computed() {
        // reference = 65000. bybit mark 65065 -> +65 abs, 10.0 bps; index 65032.5 ->
        // 5.0 gap; own mark-vs-index basis carried verbatim as 3.0. Single venue
        // (E66): basis_bps IS the Bybit mark-vs-spot bps = 10 -> contango (> 2).
        let spot = spot_snapshot("65000");
        let bybit = basis_map("65065", "65032.5", "3.0");
        let (values, basis) =
            perp_spot_basis_values(&spot, &bybit, dec("0"), "2026-09-04T00:00:00Z")
                .expect("basis values");
        // decimal_value rounds to 8 dp half-even then normalizes (tree convention:
        // no trailing-zero padding), so exact integers render without a fraction.
        assert_eq!(values["reference_price"], json!("65000"));
        assert_eq!(values["bybit_mark_vs_spot_absolute"], json!("65"));
        assert_eq!(values["bybit_mark_vs_spot_bps"], json!("10"));
        assert_eq!(values["bybit_index_gap_bps"], json!("5"));
        // bybit_mark_index_bps carried verbatim.
        assert_eq!(values["bybit_mark_index_bps"], json!("3.0"));
        assert_eq!(values["basis_bps"], json!("10"));
        assert_eq!(values["state"], json!("contango"));
        assert_eq!(values["venue_count"], json!(1u8));
        // No binance_* field survives on the single-venue contract.
        assert!(!values.keys().any(|key| key.starts_with("binance_")));
        assert_eq!(basis, dec("10"));
    }

    #[test]
    fn basis_state_boundaries_are_strict() {
        // flat_threshold = 2. Exactly +2 is flat; just above is contango; -2 is
        // flat; just below is backwardation.
        assert_eq!(basis_state(dec("2")).unwrap(), "flat");
        assert_eq!(basis_state(dec("2.00000001")).unwrap(), "contango");
        assert_eq!(basis_state(dec("-2")).unwrap(), "flat");
        assert_eq!(basis_state(dec("-2.00000001")).unwrap(), "backwardation");
        assert_eq!(basis_state(dec("0")).unwrap(), "flat");
    }

    #[test]
    fn basis_bybit_reaches_ready_with_values_once_warmed() {
        // E66: with the spot reference and the Bybit basis leg ready and the 60 s
        // warm-up elapsed, perp_spot_basis reaches READY with values from the Bybit
        // inputs alone — no binance_* reason, no binance_* field.
        let mut processor =
            CrossProcessor::new("cross-test", DateTime::UNIX_EPOCH).expect("processor builds");
        // Pre-seed the warm-up clock at t=0 so the first ready-inputs interval at
        // t=60 s is already warmed (the ring is fed on this same interval).
        processor.basis_warm_since = Some(0);
        let spot = spot_snapshot("65000");
        let bybit = VenueLegs {
            basis: Some(basis_map("65065", "65032.5", "3.0")),
            ..Default::default()
        };
        let evaluation = processor
            .evaluate_basis(&spot, &bybit, BASIS_WINDOW_NS, "2026-09-04T00:01:00Z")
            .expect("evaluation");
        let BasisEvaluation::Ready { values, state } = evaluation else {
            panic!("a warmed Bybit basis leg must be ready");
        };
        assert_eq!(state, "contango");
        assert_eq!(values["basis_bps"], json!("10"));
        assert_eq!(values["bybit_mark_vs_spot_bps"], json!("10"));
        assert_eq!(values["venue_count"], json!(1u8));
        assert!(!values.keys().any(|key| key.starts_with("binance_")));
    }

    #[test]
    fn basis_missing_leg_reports_bybit_reason_never_binance() {
        // With the Bybit basis leg absent the identity degrades with the Bybit
        // constituents reason; no binance_* reason can appear (E66).
        let mut processor =
            CrossProcessor::new("cross-test", DateTime::UNIX_EPOCH).expect("processor builds");
        let spot = spot_snapshot("65000");
        let evaluation = processor
            .evaluate_basis(
                &spot,
                &VenueLegs::default(),
                ONE_SECOND_NS,
                "2026-09-04T00:00:00Z",
            )
            .expect("evaluation");
        let BasisEvaluation::NotReady(reasons) = evaluation else {
            panic!("a missing Bybit basis leg must be non-ready");
        };
        assert_eq!(
            reasons,
            vec!["derivatives_constituents_unavailable".to_owned()]
        );
        assert!(!reasons.iter().any(|reason| reason.starts_with("binance_")));
    }

    #[test]
    fn basis_nothing_ready_reports_reference_and_bybit_reasons() {
        let mut processor =
            CrossProcessor::new("cross-test", DateTime::UNIX_EPOCH).expect("processor builds");
        let spot = SpotSnapshot {
            ready: false,
            reference_price: None,
            reference_price_raw: None,
            max_input_age_ms: None,
        };
        let evaluation = processor
            .evaluate_basis(
                &spot,
                &VenueLegs::default(),
                ONE_SECOND_NS,
                "2026-09-04T00:00:00Z",
            )
            .expect("evaluation");
        let BasisEvaluation::NotReady(reasons) = evaluation else {
            panic!("nothing-ready basis must be non-ready");
        };
        assert!(reasons.contains(&"reference_unavailable".to_owned()));
        assert!(reasons.contains(&"derivatives_constituents_unavailable".to_owned()));
        assert!(!reasons.iter().any(|reason| reason.starts_with("binance_")));
    }

    #[test]
    fn burst_side_and_expected_direction_follow_the_upstream_convention() {
        // side_skew_60s = (long - short)/total in [-1, 1]; buy = long liquidated
        // (E55), so a POSITIVE skew is longs_liquidated (expected down).
        assert_eq!(burst_side(dec("0.6")), "longs_liquidated");
        assert_eq!(burst_side(dec("-0.6")), "shorts_liquidated");
        assert_eq!(burst_side(dec("1")), "longs_liquidated");
        assert_eq!(burst_side(dec("-1")), "shorts_liquidated");
        assert_eq!(burst_side(dec("0")), "balanced");
        assert_eq!(expected_direction("longs_liquidated"), "down");
        assert_eq!(expected_direction("shorts_liquidated"), "up");
        assert_eq!(expected_direction("balanced"), "none");
    }

    #[test]
    fn follow_through_classification_matches_the_spec() {
        // response_threshold = 5 bps. shorts_liquidated -> expected up.
        assert_eq!(
            follow_through("both_resolved", "up", Some(dec("6"))).unwrap(),
            "follow_through"
        );
        assert_eq!(
            follow_through("both_resolved", "up", Some(dec("-6"))).unwrap(),
            "fade"
        );
        assert_eq!(
            follow_through("both_resolved", "up", Some(dec("4"))).unwrap(),
            "muted"
        );
        assert_eq!(
            follow_through("both_resolved", "up", Some(dec("-4"))).unwrap(),
            "muted"
        );
        assert_eq!(
            follow_through("five_resolved", "up", Some(dec("6"))).unwrap(),
            "unresolved"
        );
        // exactly at threshold with matching sign is follow_through.
        assert_eq!(
            follow_through("both_resolved", "down", Some(dec("-5"))).unwrap(),
            "follow_through"
        );
    }

    #[test]
    fn liquidation_bybit_only_cascade_and_horizons() {
        // A synthetic bybit-only LONG cascade: side_skew_60s +0.6 (in [-1,1])
        // means longs dominated the liquidations -> longs_liquidated -> expected
        // DOWN (E55). Spot then falls to give a -10 bps 30 s response, whose sign
        // matches the expected direction -> follow_through.
        let mut processor =
            CrossProcessor::new("cross-test", DateTime::UNIX_EPOCH).expect("processor builds");
        // Seed the spot ring with the burst price and horizon prices on the lane
        // decision clock.
        let burst_ns = 100 * ONE_SECOND_NS;
        push_ring(
            &mut processor.spot_ring,
            burst_ns,
            dec("65000"),
            SPOT_RING_RETENTION_NS,
        );

        let spot = spot_snapshot("65000");
        let mut bybit = VenueLegs {
            liquidation: Some(BTreeMap::from([
                ("clustering_state".to_owned(), json!("cascade")),
                ("notional_usd_60s".to_owned(), json!("1250000")),
                ("side_skew_60s".to_owned(), json!("0.6")),
                ("feed_completeness".to_owned(), json!("complete")),
            ])),
            ..Default::default()
        };
        let first = processor
            .evaluate_liquidation(&spot, &bybit, burst_ns, "2026-09-04T00:01:40Z")
            .expect("cascade evaluation");
        let LiquidationEvaluation::Ready { values, .. } = first else {
            panic!("a cascade makes the identity ready");
        };
        assert_eq!(values["burst_venue"], json!("bybit"));
        assert_eq!(values["burst_side"], json!("longs_liquidated"));
        assert_eq!(values["expected_direction"], json!("down"));
        assert_eq!(values["burst_sequence"], json!(1u64));
        assert_eq!(values["horizon_state"], json!("none_resolved"));
        assert_eq!(values["feed_completeness"], json!("complete"));
        assert_eq!(values["burst_notional_usd"], json!("1250000"));

        // Provide a +5 s spot at 64967.5 (-5 bps) and a +30 s spot at 64935 (-10 bps).
        push_ring(
            &mut processor.spot_ring,
            burst_ns + HORIZON_5S_NS,
            dec("64967.5"),
            SPOT_RING_RETENTION_NS,
        );
        push_ring(
            &mut processor.spot_ring,
            burst_ns + HORIZON_30S_NS,
            dec("64935"),
            SPOT_RING_RETENTION_NS,
        );
        // Clustering falls back so no new cascade fires on the resolving tick.
        bybit.liquidation = Some(BTreeMap::from([
            ("clustering_state".to_owned(), json!("elevated")),
            ("notional_usd_60s".to_owned(), json!("1250000")),
            ("side_skew_60s".to_owned(), json!("0.6")),
            ("feed_completeness".to_owned(), json!("complete")),
        ]));
        let resolved = processor
            .evaluate_liquidation(
                &spot,
                &bybit,
                burst_ns + HORIZON_30S_NS,
                "2026-09-04T00:02:10Z",
            )
            .expect("resolving evaluation");
        let LiquidationEvaluation::Ready {
            values,
            follow_through,
            ..
        } = resolved
        else {
            panic!("still ready while restating the cascade");
        };
        assert_eq!(values["horizon_state"], json!("both_resolved"));
        assert_eq!(values["response_bps_5s"], json!("-5"));
        assert_eq!(values["response_bps_30s"], json!("-10"));
        assert_eq!(follow_through, "follow_through");
        assert_eq!(
            values["burst_sequence"],
            json!(1u64),
            "no new cascade fired"
        );
    }

    #[test]
    fn liquidation_warms_up_before_the_first_cascade() {
        let mut processor =
            CrossProcessor::new("cross-test", DateTime::UNIX_EPOCH).expect("processor builds");
        let spot = spot_snapshot("65000");
        let evaluation = processor
            .evaluate_liquidation(
                &spot,
                &VenueLegs::default(),
                ONE_SECOND_NS,
                "2026-09-04T00:00:00Z",
            )
            .expect("evaluation");
        let LiquidationEvaluation::NotReady(reasons) = evaluation else {
            panic!("no cascade means warming");
        };
        assert_eq!(reasons, vec!["liquidation_burst_not_observed".to_owned()]);
    }

    fn funding_map(rate: &str, annualised: &str, sign: &str) -> BTreeMap<String, Value> {
        BTreeMap::from([
            ("funding_rate".to_owned(), json!(rate)),
            ("annualised_rate_bps".to_owned(), json!(annualised)),
            ("sign_regime".to_owned(), json!(sign)),
        ])
    }

    fn oi_map(delta_pct_1m: &str) -> BTreeMap<String, Value> {
        BTreeMap::from([("delta_pct_1m".to_owned(), json!(delta_pct_1m))])
    }

    fn regime_legs(sign: &str, oi: &str) -> VenueLegs {
        VenueLegs {
            funding: Some(funding_map("0.0001", "10", sign)),
            open_interest: Some(oi_map(oi)),
            ..Default::default()
        }
    }

    fn ready_basis(state: &str) -> BasisEvaluation {
        let mut values = BTreeMap::new();
        values.insert("basis_bps".to_owned(), json!("6.00000000"));
        BasisEvaluation::Ready {
            values,
            state: state.to_owned(),
        }
    }

    #[test]
    fn derivatives_regime_classification_and_stress_override() {
        let processor =
            CrossProcessor::new("cross-test", DateTime::UNIX_EPOCH).expect("processor builds");
        let spot = spot_snapshot("65000");

        // (contango, longs_pay, rising) -> long_leverage_building. Single venue
        // (E66): the Bybit funding/OI leg reaches ready with single-venue values.
        let bybit = regime_legs("longs_pay", "0.1");
        let regime = processor
            .evaluate_regime(
                &spot,
                &bybit,
                Some("contango"),
                &ready_basis("contango"),
                None,
                None,
                "2026-09-04T00:00:00Z",
            )
            .expect("regime");
        let RegimeEvaluation::Ready(values) = regime else {
            panic!("the Bybit leg present -> ready");
        };
        assert_eq!(values["regime"], json!("long_leverage_building"));
        assert_eq!(values["funding_regime"], json!("longs_pay"));
        assert_eq!(values["open_interest_regime"], json!("rising"));
        assert_eq!(values["basis_bps"], json!("6.00000000"));
        assert_eq!(values["funding_rate"], json!("0.0001"));
        assert_eq!(values["funding_annualised_bps"], json!("10"));
        assert_eq!(values["oi_delta_pct_1m"], json!("0.1"));
        assert_eq!(values["venue_count"], json!(1u8));
        assert_eq!(values["stress_active"], json!(false));
        // No mean_* field and no binance_* reason on the single-venue contract.
        assert!(!values.keys().any(|key| key.starts_with("mean_")));

        // A shorts_pay funding sign in a flat basis is an uncatalogued combination
        // -> mixed (the closed table's default), not a stress reading.
        let shorts = regime_legs("shorts_pay", "0.1");
        let regime = processor
            .evaluate_regime(
                &spot,
                &shorts,
                Some("flat"),
                &ready_basis("flat"),
                None,
                None,
                "2026-09-04T00:00:00Z",
            )
            .expect("regime");
        let RegimeEvaluation::Ready(values) = regime else {
            panic!("ready");
        };
        assert_eq!(values["funding_regime"], json!("shorts_pay"));
        assert_eq!(values["regime"], json!("mixed"));

        // Stress override wins: liquidation follow_through within the window.
        let regime = processor
            .evaluate_regime(
                &spot,
                &bybit,
                Some("contango"),
                &ready_basis("contango"),
                Some("follow_through"),
                Some(STRESS_WINDOW_MS),
                "2026-09-04T00:00:00Z",
            )
            .expect("regime");
        let RegimeEvaluation::Ready(values) = regime else {
            panic!("ready");
        };
        assert_eq!(values["regime"], json!("stress"));
        assert_eq!(values["stress_active"], json!(true));

        // Just past the stress window: no override.
        let regime = processor
            .evaluate_regime(
                &spot,
                &bybit,
                Some("contango"),
                &ready_basis("contango"),
                Some("follow_through"),
                Some(STRESS_WINDOW_MS + 1),
                "2026-09-04T00:00:00Z",
            )
            .expect("regime");
        let RegimeEvaluation::Ready(values) = regime else {
            panic!("ready");
        };
        assert_eq!(values["regime"], json!("long_leverage_building"));
        assert_eq!(values["stress_active"], json!(false));
    }

    #[test]
    fn derivatives_regime_missing_leg_reports_bybit_reason_never_binance() {
        // A missing Bybit funding/OI leg degrades with the Bybit reasons; no
        // binance_* reason can appear (E66).
        let processor =
            CrossProcessor::new("cross-test", DateTime::UNIX_EPOCH).expect("processor builds");
        let spot = spot_snapshot("65000");
        let regime = processor
            .evaluate_regime(
                &spot,
                &VenueLegs::default(),
                Some("contango"),
                &ready_basis("contango"),
                None,
                None,
                "2026-09-04T00:00:00Z",
            )
            .expect("regime");
        let RegimeEvaluation::NotReady(reasons) = regime else {
            panic!("a missing Bybit leg must be non-ready");
        };
        assert!(reasons.contains(&"funding_unavailable".to_owned()));
        assert!(reasons.contains(&"open_interest_unavailable".to_owned()));
        assert!(!reasons.iter().any(|reason| reason.starts_with("binance_")));
    }

    /// Audit M: reach a READY `liquidation_vs_spot_response` frame at the processor
    /// seam (the embedded Bybit engine's 300 s cascade warmup is unreachable in a
    /// bounded real-time lane run; the deterministic fold is exactly what replay
    /// reproduces). Prove (a) the fold is deterministic over a ready frame — two
    /// fresh processors given the same inputs emit byte-identical ready frames —
    /// and (b) a checkpoint round-trip restores IDENTICAL values (a resume restates
    /// the tracked cascade and reproduces the horizon lookups).
    fn seeded_long_cascade() -> CrossProcessor {
        let mut p =
            CrossProcessor::new("cross-m-1", DateTime::UNIX_EPOCH).expect("processor builds");
        let burst_ns = 100 * ONE_SECOND_NS;
        p.burst_sequence = 1;
        p.cascade = Some(TrackedCascade {
            burst_sequence: 1,
            burst_now_ns: burst_ns,
            burst_time: "2026-09-04T00:01:40.000000000Z".to_owned(),
            burst_venue: "bybit".to_owned(),
            burst_side: "longs_liquidated".to_owned(),
            burst_side_skew: "0.6".to_owned(),
            burst_notional_usd: "1250000".to_owned(),
            burst_venue_count: 1,
            feed_completeness: json!("complete"),
            reference_price_at_burst: json!("65000"),
            reference_price_at_burst_dec: "65000".to_owned(),
            expected_direction: "down".to_owned(),
            // Unresolved: on_interval resolves them from the spot ring.
            response_bps_5s: None,
            response_bps_30s: None,
        });
        p.last_clustering_bybit = Some("cascade".to_owned());
        // Burst at 65000; -5 bps at +5 s, -10 bps at +30 s (a long cascade's
        // expected DOWN move, so follow_through resolves).
        push_ring(
            &mut p.spot_ring,
            burst_ns,
            dec("65000"),
            SPOT_RING_RETENTION_NS,
        );
        push_ring(
            &mut p.spot_ring,
            burst_ns + HORIZON_5S_NS,
            dec("64967.5"),
            SPOT_RING_RETENTION_NS,
        );
        push_ring(
            &mut p.spot_ring,
            burst_ns + HORIZON_30S_NS,
            dec("64935"),
            SPOT_RING_RETENTION_NS,
        );
        p
    }

    fn clk(now_ns: u64) -> LaneClock {
        LaneClock {
            now_ns,
            produced_at: "2026-09-04T00:02:10.000000000Z".to_owned(),
        }
    }

    fn liquidation_update(updates: &[SignalUpdate]) -> SignalUpdate {
        updates
            .iter()
            .find(|u| u.signal_id == LIQUIDATION_RESPONSE_ID)
            .cloned()
            .expect("a liquidation_vs_spot_response frame is emitted")
    }

    #[test]
    fn ready_liquidation_frame_is_deterministic_and_checkpoint_value_preserving() {
        let t = 130 * ONE_SECOND_NS;

        // (a) The ready frame is what we expect, and the fold is deterministic: a
        // second fresh processor emits a byte-identical ready liquidation frame.
        let mut a = seeded_long_cascade();
        let frame_a = liquidation_update(&a.on_interval(&clk(t)).expect("interval admits"));
        assert_eq!(frame_a.quality.state, QualityState::Ready);
        assert_eq!(frame_a.values["burst_side"], json!("longs_liquidated"));
        assert_eq!(frame_a.values["expected_direction"], json!("down"));
        assert_eq!(frame_a.values["horizon_state"], json!("both_resolved"));
        assert_eq!(frame_a.values["response_bps_5s"], json!("-5"));
        assert_eq!(frame_a.values["response_bps_30s"], json!("-10"));
        assert_eq!(frame_a.values["follow_through"], json!("follow_through"));

        let mut b = seeded_long_cascade();
        let frame_b = liquidation_update(&b.on_interval(&clk(t)).expect("interval admits"));
        assert_eq!(
            serde_json::to_string(&frame_a).unwrap(),
            serde_json::to_string(&frame_b).unwrap(),
            "the ready liquidation fold is deterministic (replay-equivalent)"
        );

        // (b) A checkpoint round-trip restores IDENTICAL values under a new epoch.
        let checkpoint = a.checkpoint_state();
        let mut restored =
            CrossProcessor::new("cross-m-2", DateTime::UNIX_EPOCH).expect("processor builds");
        restored.restore_state(checkpoint).expect("state restores");
        let frame_r = liquidation_update(&restored.on_interval(&clk(t)).expect("interval admits"));
        assert_eq!(frame_r.quality.state, QualityState::Ready);
        assert_eq!(
            frame_r.values, frame_a.values,
            "restored cross values equal the pre-checkpoint ones"
        );
    }

    #[test]
    fn open_interest_regime_boundaries() {
        assert_eq!(open_interest_regime(dec("0.05")).unwrap(), "flat");
        assert_eq!(open_interest_regime(dec("0.06")).unwrap(), "rising");
        assert_eq!(open_interest_regime(dec("-0.05")).unwrap(), "flat");
        assert_eq!(open_interest_regime(dec("-0.06")).unwrap(), "falling");
    }

    #[test]
    fn reset_output_sequences_restarts_every_published_identitys_decision_sequence() {
        // C5 Design B: at a rotation boundary the shell calls `reset_output_sequences`
        // on the warm CrossProcessor (`LanePipeline::begin_generation`). The cross lane
        // publishes exactly three identities, one per emitter (E66/E52), so the override
        // MUST restart all three decision counters at zero for the fresh output epoch;
        // the trait default is a no-op, which would carry the counters across the
        // boundary and diverge from a replay. This advances each of the three real
        // emitters through several Ready decisions with evaluator-produced values, then
        // asserts the reset restarts every one at zero. The per-identity DECISION
        // sequence (not the reset STREAM frame the shell zeroes unconditionally) is what
        // the override governs. The shell-invocation path itself is proven end to end in
        // the venue-lane suite; the cross lane cannot reach Ready in a bounded shell run
        // (its 60 s basis warm-up), so its reset is exercised here on the real emitters.
        let mut processor =
            CrossProcessor::new("cross-reset-test", DateTime::UNIX_EPOCH).expect("builds");
        let epoch = processor.publisher_epoch.clone();
        let spot = spot_snapshot("65000");

        // perp_spot_basis: Ready once warmed (pre-seed the warm-up clock at t=0 so the
        // t=60 s interval is already warmed, as the warm-up unit test does).
        processor.basis_warm_since = Some(0);
        let bybit_basis = VenueLegs {
            basis: Some(basis_map("65065", "65032.5", "3.0")),
            ..Default::default()
        };
        let basis_values = match processor
            .evaluate_basis(&spot, &bybit_basis, BASIS_WINDOW_NS, "2026-09-04T00:01:00Z")
            .expect("basis evaluation")
        {
            BasisEvaluation::Ready { values, .. } => values,
            BasisEvaluation::NotReady(reasons) => panic!("warmed basis must be ready: {reasons:?}"),
        };

        // liquidation_vs_spot_response: Ready on a Bybit cascade (spot ring seeded).
        let burst_ns = 100 * ONE_SECOND_NS;
        push_ring(
            &mut processor.spot_ring,
            burst_ns,
            dec("65000"),
            SPOT_RING_RETENTION_NS,
        );
        let bybit_liq = VenueLegs {
            liquidation: Some(BTreeMap::from([
                ("clustering_state".to_owned(), json!("cascade")),
                ("notional_usd_60s".to_owned(), json!("1250000")),
                ("side_skew_60s".to_owned(), json!("0.6")),
                ("feed_completeness".to_owned(), json!("complete")),
            ])),
            ..Default::default()
        };
        let liquidation_values = match processor
            .evaluate_liquidation(&spot, &bybit_liq, burst_ns, "2026-09-04T00:01:40Z")
            .expect("liquidation evaluation")
        {
            LiquidationEvaluation::Ready { values, .. } => values,
            LiquidationEvaluation::NotReady(reasons) => {
                panic!("a cascade must be ready: {reasons:?}")
            }
        };

        // derivatives_regime: Ready with the single Bybit funding/OI leg.
        let regime_values = match processor
            .evaluate_regime(
                &spot,
                &regime_legs("longs_pay", "0.1"),
                Some("contango"),
                &ready_basis("contango"),
                None,
                None,
                "2026-09-04T00:00:00Z",
            )
            .expect("regime evaluation")
        {
            RegimeEvaluation::Ready(values) => values,
            RegimeEvaluation::NotReady(reasons) => panic!("regime must be ready: {reasons:?}"),
        };

        // Advance every emitter through several Ready decisions (sequence 0, 1, 2 …):
        // the decision sequence advances only on a Ready emit (the spot pipeline's
        // rule). Each identity re-emits under the SAME instant that produced its values
        // (its embedded `window_close` must equal the envelope event_time).
        let emit_all = |processor: &mut CrossProcessor| {
            (
                processor
                    .basis_emitter
                    .emit(
                        &epoch,
                        "2026-09-04T00:01:00Z",
                        "2026-09-04T00:01:00Z".to_owned(),
                        Some(basis_values.clone()),
                        Vec::new(),
                    )
                    .expect("basis emit")
                    .sequence,
                processor
                    .liquidation_emitter
                    .emit(
                        &epoch,
                        "2026-09-04T00:01:40Z",
                        "2026-09-04T00:01:40Z".to_owned(),
                        Some(liquidation_values.clone()),
                        Vec::new(),
                    )
                    .expect("liquidation emit")
                    .sequence,
                processor
                    .regime_emitter
                    .emit(
                        &epoch,
                        "2026-09-04T00:00:00Z",
                        "2026-09-04T00:00:00Z".to_owned(),
                        Some(regime_values.clone()),
                        Vec::new(),
                    )
                    .expect("regime emit")
                    .sequence,
            )
        };
        for expected in 0..3_u64 {
            assert_eq!(
                emit_all(&mut processor),
                (expected, expected, expected),
                "each identity's decision sequence advances only on Ready emits"
            );
        }

        // The rotation boundary: the reset restarts EVERY published identity's counter
        // at zero. Without the override (the trait default no-op) these would be 3.
        processor.reset_output_sequences();
        assert_eq!(
            emit_all(&mut processor),
            (0, 0, 0),
            "reset_output_sequences restarts all three btc.cross.* decision sequences at zero"
        );
    }
}
