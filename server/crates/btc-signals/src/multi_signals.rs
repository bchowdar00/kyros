//! `btc.multi.*` cross-venue prediction blends (C1, ruling E57).
//!
//! Two tier-2 identities that read the PUBLISHED fields of the Kalshi and
//! Polymarket single-venue prediction identities and combine them across venues
//! for the aligned market pair: [`PREDICTION_DISLOCATION_ID`] and
//! [`PAIR_COST_SPREAD_ID`]. They are `multi_market`-scoped (ADR 0026): the scope
//! carries both bound legs (kalshi then polymarket in V3 sort order) and the
//! shared window, and a change of any leg's id or generation is a different
//! instance key (V10).
//!
//! The processor never re-derives an upstream fee or model probability: it
//! consumes `model_adjustment` from the two `fair_up_probability` identities and
//! the per-leg fees from the two `pair_cost_state` identities verbatim (§2, §5.3,
//! §5.4). The alignment gate is close-only (W1): the two legs align exactly when
//! `window_close` is byte-identical after RFC 3339 normalization, both markets
//! are open, and the windows overlap; anything else is `window_alignment_mismatch`
//! and binds no instance (W2).
//!
//! The coalescing emitter [`MultiMarketEmitter`] mirrors the oracle lane's
//! [`crate::oracle_signals::OracleReferenceEmitter`] verbatim (do not invent),
//! differing only in that it stamps a caller-supplied `multi_market` scope and
//! keys its coalescing memory per scope so two distinct instances (a rollover on
//! either leg) never share a published decision.

use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;

use chrono::{DateTime, Utc};
use rust_decimal::{Decimal, RoundingStrategy};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use signals_core::{
    FieldType, MultiMarketLeg, MultiMarketWindow, Quality, QualityState,
    SIGNAL_UPDATE_SCHEMA_VERSION, SignalDefinition, SignalScope, SignalUpdate,
};

use crate::lane::LaneClock;

/// Eight-place round-half-to-even, the wave's output rounding (mirrors
/// `spot_signals::OUTPUT_DECIMAL_PLACES`).
const OUTPUT_DECIMAL_PLACES: u32 = 8;

/// `btc.multi.prediction_dislocation` identity.
pub const PREDICTION_DISLOCATION_ID: &str = "btc.multi.prediction_dislocation";
/// `btc.multi.pair_cost_spread` identity.
pub const PAIR_COST_SPREAD_ID: &str = "btc.multi.pair_cost_spread";
/// Shared version of the two multi identities.
pub const MULTI_SIGNAL_VERSION: &str = "1.0.0";
/// The publisher signal-set id and version (E24: checkpoint processor id = the
/// lane's signal-set id).
pub const MULTI_SIGNAL_SET_ID: &str = "btc.multi.venue";
/// The publisher signal-set version.
pub const MULTI_SIGNAL_SET_VERSION: &str = "1.0.0";
/// Number of identities this processor publishes.
pub const MULTI_SIGNAL_COUNT: usize = 2;

const PREDICTION_DISLOCATION_DOCUMENT: &str =
    include_str!("../../../definitions/btc/signals/btc.multi.prediction_dislocation@1.0.0.json");
const PAIR_COST_SPREAD_DOCUMENT: &str =
    include_str!("../../../definitions/btc/signals/btc.multi.pair_cost_spread@1.0.0.json");

/// The two embedded identity documents `(id, raw json)`, chained into the
/// `every_active_definition_has_an_embedded_processor_contract` guard exactly as
/// the oracle and venue processors chain theirs.
#[must_use]
pub fn multi_signal_contract_documents() -> Vec<(&'static str, &'static str)> {
    vec![
        (PREDICTION_DISLOCATION_ID, PREDICTION_DISLOCATION_DOCUMENT),
        (PAIR_COST_SPREAD_ID, PAIR_COST_SPREAD_DOCUMENT),
    ]
}

/// The two multi identity ids, in publication (table) order.
#[must_use]
pub fn multi_signal_ids() -> Vec<&'static str> {
    vec![PREDICTION_DISLOCATION_ID, PAIR_COST_SPREAD_ID]
}

// --- upstream input identity ids (the published fields C1 reads, §2) ---

const KALSHI_CONTRACT_ID: &str = "btc.kalshi.market_contract";
const POLYMARKET_CONTRACT_ID: &str = "btc.polymarket.market_contract";
const KALSHI_QUOTE_ID: &str = "btc.kalshi.prediction_quote";
const POLYMARKET_QUOTE_ID: &str = "btc.polymarket.prediction_quote";
const KALSHI_FAIR_ID: &str = "btc.kalshi.fair_up_probability";
const POLYMARKET_FAIR_ID: &str = "btc.polymarket.fair_up_probability";
const KALSHI_PAIR_COST_ID: &str = "btc.kalshi.pair_cost_state";
const POLYMARKET_PAIR_COST_ID: &str = "btc.polymarket.pair_cost_state";

/// The prediction venues a multi instance blends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MultiVenue {
    Kalshi,
    Polymarket,
}

impl MultiVenue {
    fn token(self) -> &'static str {
        match self {
            Self::Kalshi => "kalshi",
            Self::Polymarket => "polymarket",
        }
    }
}

/// Fatal, non-recoverable errors in the multi computation. A per-decision
/// non-ready READING is never an error — it is published as a non-ready
/// evaluation with a reason.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum MultiSignalError {
    #[error("embedded multi definition is invalid: {0}")]
    Definition(String),
    #[error("multi computation failed: {0}")]
    Computation(String),
    #[error("multi checkpoint state is invalid: {0}")]
    Checkpoint(String),
}

/// The last published decision for one instance scope: the change key and the
/// logical time it published at, for the coalescing rule (mirrors the oracle and
/// prediction emitters).
#[derive(Debug, Clone)]
struct PublishedDecision {
    change_key: BTreeMap<String, Value>,
    published_ns: u64,
}

/// A `multi_market`-scoped coalescing emitter. Mirrors
/// [`crate::oracle_signals::OracleReferenceEmitter`] verbatim; it differs only in
/// stamping a caller-supplied scope and keying coalescing memory per scope so a
/// rollover on either leg starts a fresh instance (V10).
struct MultiMarketEmitter {
    definition: SignalDefinition,
    next_sequence: u64,
    lifetime_output_count: u64,
    ever_ready: bool,
    change_key_fields: BTreeSet<String>,
    last_published: BTreeMap<SignalScope, PublishedDecision>,
}

impl MultiMarketEmitter {
    fn from_document(document: &str) -> Result<Self, MultiSignalError> {
        let definition: SignalDefinition = serde_json::from_str(document)
            .map_err(|error| MultiSignalError::Definition(error.to_string()))?;
        let change_key_fields = definition
            .fields
            .iter()
            .filter(|field| {
                !matches!(
                    field.field_type,
                    FieldType::Timestamp | FieldType::DurationMs
                )
            })
            .map(|field| field.name.clone())
            .collect();
        Ok(Self {
            definition,
            next_sequence: 0,
            lifetime_output_count: 0,
            ever_ready: false,
            change_key_fields,
            last_published: BTreeMap::new(),
        })
    }

    fn definition(&self) -> &SignalDefinition {
        &self.definition
    }

    /// Restart the per-generation output sequence at zero for a new file
    /// generation (C5), keeping the warm coalescing/lifetime state. A rotation
    /// opens a fresh output epoch whose first ready decision must be sequence
    /// zero, so the multi runtime resets both blend emitters at the boundary,
    /// matching the sequence a resume's fresh rebuild would start at.
    fn reset_output_sequence(&mut self) {
        self.next_sequence = 0;
    }

    fn change_key(&self, values: &BTreeMap<String, Value>) -> BTreeMap<String, Value> {
        values
            .iter()
            .filter(|(name, _)| self.change_key_fields.contains(name.as_str()))
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect()
    }

    fn publishes(
        &self,
        scope: &SignalScope,
        change_key: &BTreeMap<String, Value>,
        now_ns: u64,
    ) -> Result<bool, MultiSignalError> {
        let Some(last) = self.last_published.get(scope) else {
            return Ok(true);
        };
        if &last.change_key != change_key {
            return Ok(true);
        }
        let interval_ns = self
            .definition
            .approximate_interval_ms
            .checked_mul(1_000_000)
            .ok_or_else(|| MultiSignalError::Computation("interval overflow".to_owned()))?;
        Ok(now_ns.saturating_sub(last.published_ns) >= interval_ns)
    }

    /// Evaluate one decision for one instance scope. `ready_values` is `Some` for
    /// a ready reading and `None` for a non-ready one (which carries at least one
    /// reason). Returns the update to write, or `None` when the coalescing rule
    /// suppresses a ready restatement.
    fn emit(
        &mut self,
        publisher_epoch: &str,
        clock: &LaneClock,
        scope: SignalScope,
        ready_values: Option<BTreeMap<String, Value>>,
        mut reasons: Vec<String>,
    ) -> Result<Option<SignalUpdate>, MultiSignalError> {
        reasons.sort();
        reasons.dedup();
        let ready = ready_values.is_some();
        let change_key = ready_values.as_ref().map(|values| self.change_key(values));
        let published = match &change_key {
            Some(change_key) => self.publishes(&scope, change_key, clock.now_ns)?,
            None => true,
        };
        let quality = if ready {
            Quality {
                state: QualityState::Ready,
                reasons: Vec::new(),
            }
        } else {
            if reasons.is_empty() {
                return Err(MultiSignalError::Computation(
                    "a non-ready multi output requires a reason".to_owned(),
                ));
            }
            Quality {
                state: if self.ever_ready {
                    QualityState::Degraded
                } else {
                    QualityState::WarmingUp
                },
                reasons,
            }
        };
        let (valid_from, valid_until) =
            SignalUpdate::validity_window(&clock.produced_at, self.definition.staleness_bound_ms)
                .map_err(|error| MultiSignalError::Computation(error.to_string()))?;
        let update = SignalUpdate {
            schema_version: SIGNAL_UPDATE_SCHEMA_VERSION,
            signal_id: self.definition.signal_id.clone(),
            signal_version: self.definition.version.clone(),
            publisher_epoch: publisher_epoch.to_owned(),
            sequence: self.next_sequence,
            event_time: clock.produced_at.clone(),
            produced_at: clock.produced_at.clone(),
            valid_from,
            valid_until,
            quality,
            values: ready_values.clone().unwrap_or_default(),
            scope: scope.clone(),
        };
        update
            .validate_against(self.definition())
            .map_err(|error| MultiSignalError::Computation(error.to_string()))?;
        self.ever_ready |= ready;
        if let (Some(change_key), true) = (change_key, published) {
            self.last_published.insert(
                scope,
                PublishedDecision {
                    change_key,
                    published_ns: clock.now_ns,
                },
            );
            self.next_sequence = self
                .next_sequence
                .checked_add(1)
                .ok_or_else(|| MultiSignalError::Computation("sequence overflow".to_owned()))?;
        } else if !ready {
            self.last_published.remove(&scope);
        }
        if published {
            self.lifetime_output_count =
                self.lifetime_output_count.checked_add(1).ok_or_else(|| {
                    MultiSignalError::Computation("output counter overflow".to_owned())
                })?;
        }
        Ok(if published { Some(update) } else { None })
    }
}

/// The persisted emitter memory (ever_ready + lifetime output count), mirroring
/// the oracle lane. The decision sequence and coalescing memory restart each
/// epoch and are NOT persisted (the spot pipeline's rule).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EmitterCheckpoint {
    ever_ready: bool,
    lifetime_output_count: u64,
}

/// The bounded resume state of the multi processor: the two emitters' lifetime
/// memory only. The bound leg state re-warms from the live venue inputs after a
/// restart (market-relative), so it is not persisted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MultiCheckpointState {
    dislocation: EmitterCheckpoint,
    pair_cost: EmitterCheckpoint,
}

/// One bound contract leg's carried fields (the published `market_contract`).
#[derive(Debug, Clone)]
struct ContractLeg {
    market_id: String,
    market_generation: u64,
    window_open: String,
    window_close: String,
    strike: Decimal,
    settlement_benchmark: String,
    comparator: String,
    tie_outcome: String,
    open: bool,
}

/// The latest harvested reading of one input identity for one venue: the ready
/// value map (or the non-ready reasons) plus the decision instant it was emitted.
#[derive(Debug, Clone, Default)]
struct Harvest {
    ready: bool,
    values: BTreeMap<String, Value>,
    reasons: Vec<String>,
}

/// The latest state harvested for one venue leg.
#[derive(Debug, Clone, Default)]
struct LegState {
    /// Whether this venue has ever produced any evaluation (to separate a venue
    /// that is entirely absent — `counterparty_venue_unavailable` — from one that
    /// is present but unbound — `leg_market_unbound`).
    seen: bool,
    contract: Option<ContractLeg>,
    quote: Option<Harvest>,
    fair: Option<Harvest>,
    pair_cost: Option<Harvest>,
}

impl LegState {
    fn clear_market(&mut self) {
        self.contract = None;
        self.quote = None;
        self.fair = None;
        self.pair_cost = None;
    }
}

/// The `btc.multi.*` cross-venue processor. Harvests the published Kalshi and
/// Polymarket prediction identities and, on the aligned pair, publishes the two
/// blends with `multi_market` scope (E57).
pub struct MultiProcessor {
    publisher_epoch: String,
    dislocation: MultiMarketEmitter,
    pair_cost: MultiMarketEmitter,
    kalshi: LegState,
    polymarket: LegState,
}

impl MultiProcessor {
    /// Build the processor for a publisher epoch.
    pub fn new(publisher_epoch: impl Into<String>) -> Result<Self, MultiSignalError> {
        Ok(Self {
            publisher_epoch: publisher_epoch.into(),
            dislocation: MultiMarketEmitter::from_document(PREDICTION_DISLOCATION_DOCUMENT)?,
            pair_cost: MultiMarketEmitter::from_document(PAIR_COST_SPREAD_DOCUMENT)?,
            kalshi: LegState::default(),
            polymarket: LegState::default(),
        })
    }

    /// The two identity definitions this processor embeds.
    pub fn definitions(&self) -> Vec<SignalDefinition> {
        vec![
            self.dislocation.definition().clone(),
            self.pair_cost.definition().clone(),
        ]
    }

    fn leg_mut(&mut self, venue: MultiVenue) -> &mut LegState {
        match venue {
            MultiVenue::Kalshi => &mut self.kalshi,
            MultiVenue::Polymarket => &mut self.polymarket,
        }
    }

    /// Harvest one venue's published batch: store the latest reading of each
    /// consumed input identity, retaining a prior reading for an input the batch
    /// did not restate. A ready `market_contract` (re)binds the leg.
    pub fn observe(
        &mut self,
        venue: MultiVenue,
        updates: &[SignalUpdate],
    ) -> Result<(), MultiSignalError> {
        let leg = self.leg_mut(venue);
        leg.seen = true;
        for update in updates {
            let harvest = Harvest {
                ready: update.quality.state == QualityState::Ready,
                values: update.values.clone(),
                reasons: update.quality.reasons.clone(),
            };
            match (venue, update.signal_id.as_str()) {
                (MultiVenue::Kalshi, KALSHI_CONTRACT_ID)
                | (MultiVenue::Polymarket, POLYMARKET_CONTRACT_ID) => {
                    if harvest.ready {
                        leg.contract = Some(parse_contract(&update.values)?);
                    }
                }
                (MultiVenue::Kalshi, KALSHI_QUOTE_ID)
                | (MultiVenue::Polymarket, POLYMARKET_QUOTE_ID) => leg.quote = Some(harvest),
                (MultiVenue::Kalshi, KALSHI_FAIR_ID)
                | (MultiVenue::Polymarket, POLYMARKET_FAIR_ID) => leg.fair = Some(harvest),
                (MultiVenue::Kalshi, KALSHI_PAIR_COST_ID)
                | (MultiVenue::Polymarket, POLYMARKET_PAIR_COST_ID) => {
                    leg.pair_cost = Some(harvest)
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// A venue leg departed (a scheduled rollover's unavailable edge, or a
    /// discovery loss). Clears the bound market so the next evaluation reports the
    /// multi instance non-ready (L2: the runtime writes this invalidation before
    /// the leg's own `market_transition unavailable` frame).
    pub fn mark_leg_unavailable(&mut self, venue: MultiVenue) {
        self.leg_mut(venue).clear_market();
    }

    /// Evaluate both identities for one decision. Returns the updates to publish
    /// (coalesced ready restatements omitted; non-ready evaluations returned).
    pub fn evaluate(&mut self, clock: &LaneClock) -> Result<Vec<SignalUpdate>, MultiSignalError> {
        // A `multi_market` identity's scope shape is pinned by `scope_kind`
        // (E20): the verifier rejects a Global scope on these two identities. A
        // valid `multi_market` scope needs exactly two bound legs and a shared
        // window (V2/V8/W1), which exists only for an aligned pair. Before that
        // — start-up and the gap after every roll — there is NO bound instance
        // (W2 "bind no instance"), so the lane WITHHOLDS both blends entirely
        // (no signal_batch, no instance_status) rather than publish a
        // partial-leg or Global scope the validators would reject. A non-ready
        // reading is published only once a valid aligned instance exists (e.g. a
        // stale quote or a warming fair probability on the aligned pair), which
        // carries the proper `multi_market` scope. The old instance's terminal
        // `invalidated` on a leg departure is emitted by the runtime, not here.
        let Ok(pair) = self.align(&clock.produced_at)? else {
            return Ok(Vec::new());
        };
        let scope = Self::scope_for(&pair);
        let publisher_epoch = self.publisher_epoch.clone();
        let mut out = Vec::new();

        let dislocation = self.dislocation_reading(&pair, &clock.produced_at)?;
        if let Some(update) = self.dislocation.emit(
            &publisher_epoch,
            clock,
            scope.clone(),
            dislocation.values,
            dislocation.reasons,
        )? {
            out.push(update);
        }

        let pair_cost = self.pair_cost_reading(&pair, &clock.produced_at)?;
        if let Some(update) = self.pair_cost.emit(
            &publisher_epoch,
            clock,
            scope,
            pair_cost.values,
            pair_cost.reasons,
        )? {
            out.push(update);
        }
        Ok(out)
    }

    /// The current aligned instance's `multi_market` scope, if any — the runtime
    /// reads it to invalidate the old instance on a leg change (V10/L2).
    pub fn aligned_scope(
        &self,
        produced_at: &str,
    ) -> Result<Option<SignalScope>, MultiSignalError> {
        Ok(self
            .align(produced_at)?
            .ok()
            .map(|pair| Self::scope_for(&pair)))
    }

    /// The `multi_market` scope for this decision: the two legs in V3 order
    /// (kalshi before polymarket) plus the overlapping window `[max(open), close]`.
    /// When the legs are not aligned there is no bound instance, but the emitter
    /// still needs a scope to key the non-ready evaluation's `instance_status`;
    /// a global-shaped placeholder is used and never validated as an instance.
    /// The `multi_market` scope for an aligned pair: the two legs in V3 order
    /// (kalshi before polymarket) plus the overlapping window `[max(open), close]`.
    /// Never Global — the lane withholds when there is no aligned pair.
    fn scope_for(pair: &AlignedPair) -> SignalScope {
        SignalScope::MultiMarket {
            legs: vec![
                MultiMarketLeg {
                    venue: MultiVenue::Kalshi.token().to_owned(),
                    market_id: pair.kalshi.market_id.clone(),
                    market_generation: pair.kalshi.market_generation,
                },
                MultiMarketLeg {
                    venue: MultiVenue::Polymarket.token().to_owned(),
                    market_id: pair.polymarket.market_id.clone(),
                    market_generation: pair.polymarket.market_generation,
                },
            ],
            window: MultiMarketWindow {
                open: pair.window_open.clone(),
                close: pair.window_close.clone(),
            },
        }
    }

    /// Apply the W1 alignment gate. Returns the aligned pair or the ordered set of
    /// non-ready reasons.
    fn align(
        &self,
        produced_at: &str,
    ) -> Result<Result<AlignedPair, Vec<String>>, MultiSignalError> {
        let mut reasons: Vec<String> = Vec::new();
        let kalshi = self.kalshi.contract.clone();
        let polymarket = self.polymarket.contract.clone();
        // Binding: a leg with no ready contract is unbound. A venue that has
        // produced nothing at all is the absent counterparty.
        if kalshi.is_none() {
            reasons.push(
                if self.kalshi.seen {
                    "leg_market_unbound"
                } else {
                    "counterparty_venue_unavailable"
                }
                .to_owned(),
            );
        }
        if polymarket.is_none() {
            reasons.push(
                if self.polymarket.seen {
                    "leg_market_unbound"
                } else {
                    "counterparty_venue_unavailable"
                }
                .to_owned(),
            );
        }
        let (Some(kalshi), Some(polymarket)) = (kalshi, polymarket) else {
            return Ok(Err(reasons));
        };
        // Both bound: a closed market blocks the pair.
        if !kalshi.open {
            reasons.push("leg_market_closed".to_owned());
        }
        if !polymarket.open {
            reasons.push("leg_market_closed".to_owned());
        }
        // W1: close instants must be byte-identical after RFC 3339 normalization,
        // and the windows must overlap.
        let close_k = parse_time(&kalshi.window_close)?;
        let close_p = parse_time(&polymarket.window_close)?;
        let open_k = parse_time(&kalshi.window_open)?;
        let open_p = parse_time(&polymarket.window_open)?;
        let overlap_open = open_k.max(open_p);
        if close_k != close_p || overlap_open >= close_k {
            reasons.push("window_alignment_mismatch".to_owned());
        }
        if !reasons.is_empty() {
            reasons.sort();
            reasons.dedup();
            return Ok(Err(reasons));
        }
        Ok(Ok(AlignedPair {
            window_open: rfc3339(overlap_open),
            window_close: kalshi.window_close.clone(),
            close_ms: millis(close_k),
            produced_ms: millis(parse_time(produced_at)?),
            kalshi,
            polymarket,
        }))
    }

    /// The `btc.multi.prediction_dislocation` reading for an aligned pair.
    fn dislocation_reading(
        &self,
        pair: &AlignedPair,
        produced_at: &str,
    ) -> Result<Reading, MultiSignalError> {
        let mut reasons = Vec::new();
        let kalshi_quote = self.quote_reading(&self.kalshi, produced_at, &mut reasons);
        let polymarket_quote = self.quote_reading(&self.polymarket, produced_at, &mut reasons);
        let kalshi_fair = fair_probability(&self.kalshi, &mut reasons);
        let polymarket_fair = fair_probability(&self.polymarket, &mut reasons);
        let time_to_close_ms = pair.close_ms - pair.produced_ms;
        if time_to_close_ms <= 0 {
            reasons.push("leg_market_closed".to_owned());
        }
        let (Some(kq), Some(pq), Some(kf), Some(pf)) =
            (kalshi_quote, polymarket_quote, kalshi_fair, polymarket_fair)
        else {
            return Ok(Reading::non_ready(reasons));
        };
        if !reasons.is_empty() {
            return Ok(Reading::non_ready(reasons));
        }

        let dislocation = pq.midpoint - kq.midpoint;
        let strike_difference_absolute = pair.polymarket.strike - pair.kalshi.strike;
        let model_adjustment = pf - kf;
        let strike_adjusted = dislocation - model_adjustment;
        let dislocation_threshold = Decimal::new(2, 2); // 0.02

        let mut values = BTreeMap::new();
        values.insert("kalshi_market_id".to_owned(), json!(pair.kalshi.market_id));
        values.insert(
            "kalshi_market_generation".to_owned(),
            json!(pair.kalshi.market_generation),
        );
        values.insert(
            "polymarket_market_id".to_owned(),
            json!(pair.polymarket.market_id),
        );
        values.insert(
            "polymarket_market_generation".to_owned(),
            json!(pair.polymarket.market_generation),
        );
        values.insert("implied_up_kalshi".to_owned(), decimal_value(kq.midpoint));
        values.insert(
            "implied_up_polymarket".to_owned(),
            decimal_value(pq.midpoint),
        );
        values.insert("kalshi_up_bid".to_owned(), decimal_value(kq.bid));
        values.insert("kalshi_up_ask".to_owned(), decimal_value(kq.ask));
        values.insert("polymarket_up_bid".to_owned(), decimal_value(pq.bid));
        values.insert("polymarket_up_ask".to_owned(), decimal_value(pq.ask));
        values.insert("dislocation".to_owned(), decimal_value(dislocation));
        values.insert(
            "dislocation_bps".to_owned(),
            decimal_value(dislocation * Decimal::from(10_000)),
        );
        values.insert(
            "model_adjustment".to_owned(),
            decimal_value(model_adjustment),
        );
        values.insert(
            "strike_adjusted_dislocation".to_owned(),
            decimal_value(strike_adjusted),
        );
        values.insert(
            "kalshi_strike".to_owned(),
            decimal_value(pair.kalshi.strike),
        );
        values.insert(
            "polymarket_strike".to_owned(),
            decimal_value(pair.polymarket.strike),
        );
        values.insert(
            "strike_difference_absolute".to_owned(),
            decimal_value(strike_difference_absolute),
        );
        let strike_difference_bps =
            checked_div(strike_difference_absolute, pair.kalshi.strike)? * Decimal::from(10_000);
        values.insert(
            "strike_difference_bps".to_owned(),
            decimal_value(strike_difference_bps),
        );
        values.insert(
            "direction".to_owned(),
            json!(match strike_adjusted.cmp(&Decimal::ZERO) {
                std::cmp::Ordering::Greater => "polymarket_rich",
                std::cmp::Ordering::Less => "kalshi_rich",
                std::cmp::Ordering::Equal => "tied",
            }),
        );
        values.insert(
            "state".to_owned(),
            json!(if strike_adjusted.abs() > dislocation_threshold {
                "dislocated"
            } else {
                "aligned"
            }),
        );
        values.insert(
            "dislocation_threshold".to_owned(),
            decimal_value(dislocation_threshold),
        );
        values.insert("window_alignment".to_owned(), json!("exact_close"));
        values.insert(
            "underlying_mismatch".to_owned(),
            json!(pair.kalshi.settlement_benchmark != pair.polymarket.settlement_benchmark),
        );
        values.insert(
            "tie_rule_mismatch".to_owned(),
            json!(
                pair.kalshi.comparator != pair.polymarket.comparator
                    || pair.kalshi.tie_outcome != pair.polymarket.tie_outcome
            ),
        );
        values.insert(
            "time_to_close_ms".to_owned(),
            json!(time_to_close_ms.max(0)),
        );
        values.insert(
            "max_input_age_ms".to_owned(),
            json!(kq.age_ms.max(pq.age_ms).max(0)),
        );
        values.insert("observation_time".to_owned(), json!(produced_at));
        Ok(Reading::ready(values))
    }

    /// The `btc.multi.pair_cost_spread` reading for an aligned pair.
    fn pair_cost_reading(
        &self,
        pair: &AlignedPair,
        produced_at: &str,
    ) -> Result<Reading, MultiSignalError> {
        let mut reasons = Vec::new();
        let kalshi_quote = self.quote_reading(&self.kalshi, produced_at, &mut reasons);
        let polymarket_quote = self.quote_reading(&self.polymarket, produced_at, &mut reasons);
        let kalshi_pc = pair_cost_state(&self.kalshi, &mut reasons);
        let polymarket_pc = pair_cost_state(&self.polymarket, &mut reasons);
        let (Some(kq), Some(pq), Some(kpc), Some(ppc)) =
            (kalshi_quote, polymarket_quote, kalshi_pc, polymarket_pc)
        else {
            return Ok(Reading::non_ready(reasons));
        };
        if !reasons.is_empty() {
            return Ok(Reading::non_ready(reasons));
        }

        let pair_cost_spread = ppc.fee_adjusted_pair_cost - kpc.fee_adjusted_pair_cost;
        let cross_up_kalshi =
            (kq.ask + kpc.yes_leg_fee) + (pq.negative_ask + ppc.down_leg_fee) - Decimal::ONE;
        let cross_up_polymarket =
            (pq.ask + ppc.up_leg_fee) + (kq.negative_ask + kpc.no_leg_fee) - Decimal::ONE;
        let (best_cross, best_direction) = if cross_up_kalshi <= cross_up_polymarket {
            (cross_up_kalshi, "up_kalshi_down_polymarket")
        } else {
            (cross_up_polymarket, "up_polymarket_down_kalshi")
        };
        let premium_threshold = kpc.premium_threshold;
        let state = if best_cross < Decimal::ZERO {
            "locked_profit"
        } else if best_cross > premium_threshold {
            "premium"
        } else {
            "fair"
        };
        let cheaper_venue = match ppc.fee_adjusted_pair_cost.cmp(&kpc.fee_adjusted_pair_cost) {
            std::cmp::Ordering::Less => "polymarket",
            std::cmp::Ordering::Greater => "kalshi",
            std::cmp::Ordering::Equal => "tied",
        };
        let lock_is_conditional = pair.kalshi.settlement_benchmark
            != pair.polymarket.settlement_benchmark
            || pair.kalshi.comparator != pair.polymarket.comparator
            || pair.kalshi.tie_outcome != pair.polymarket.tie_outcome
            || pair.kalshi.strike != pair.polymarket.strike;
        let time_to_close_ms = pair.close_ms - pair.produced_ms;

        let mut values = BTreeMap::new();
        values.insert("kalshi_market_id".to_owned(), json!(pair.kalshi.market_id));
        values.insert(
            "kalshi_market_generation".to_owned(),
            json!(pair.kalshi.market_generation),
        );
        values.insert(
            "polymarket_market_id".to_owned(),
            json!(pair.polymarket.market_id),
        );
        values.insert(
            "polymarket_market_generation".to_owned(),
            json!(pair.polymarket.market_generation),
        );
        values.insert("kalshi_pair_cost".to_owned(), decimal_value(kpc.pair_cost));
        values.insert(
            "kalshi_fee_adjusted_pair_cost".to_owned(),
            decimal_value(kpc.fee_adjusted_pair_cost),
        );
        values.insert(
            "polymarket_pair_cost".to_owned(),
            decimal_value(ppc.pair_cost),
        );
        values.insert(
            "polymarket_fee_adjusted_pair_cost".to_owned(),
            decimal_value(ppc.fee_adjusted_pair_cost),
        );
        values.insert(
            "pair_cost_spread".to_owned(),
            decimal_value(pair_cost_spread),
        );
        values.insert("cheaper_venue".to_owned(), json!(cheaper_venue));
        values.insert(
            "cross_pair_cost_up_kalshi".to_owned(),
            decimal_value(cross_up_kalshi),
        );
        values.insert(
            "cross_pair_cost_up_polymarket".to_owned(),
            decimal_value(cross_up_polymarket),
        );
        values.insert("best_cross_pair_cost".to_owned(), decimal_value(best_cross));
        values.insert("best_cross_direction".to_owned(), json!(best_direction));
        values.insert("state".to_owned(), json!(state));
        values.insert(
            "premium_threshold".to_owned(),
            decimal_value(premium_threshold),
        );
        values.insert("lock_is_conditional".to_owned(), json!(lock_is_conditional));
        values.insert(
            "time_to_close_ms".to_owned(),
            json!(time_to_close_ms.max(0)),
        );
        values.insert(
            "max_input_age_ms".to_owned(),
            json!(kq.age_ms.max(pq.age_ms).max(0)),
        );
        values.insert("observation_time".to_owned(), json!(produced_at));
        Ok(Reading::ready(values))
    }

    /// Read one venue's quote leg, pushing a reason for a missing, non-ready,
    /// crossed, or stale quote.
    fn quote_reading(
        &self,
        leg: &LegState,
        produced_at: &str,
        reasons: &mut Vec<String>,
    ) -> Option<QuoteLeg> {
        let Some(harvest) = leg.quote.as_ref() else {
            reasons.push("affirmative_quote_missing".to_owned());
            return None;
        };
        if !harvest.ready {
            if harvest.reasons.is_empty() {
                reasons.push("affirmative_quote_missing".to_owned());
            } else {
                reasons.extend(harvest.reasons.iter().cloned());
            }
            return None;
        }
        let bid = required_decimal(&harvest.values, "affirmative_bid", reasons)?;
        let ask = required_decimal(&harvest.values, "affirmative_ask", reasons)?;
        let midpoint = required_decimal(&harvest.values, "affirmative_midpoint", reasons)?;
        let negative_ask = required_decimal(&harvest.values, "negative_ask", reasons)?;
        if bid > ask {
            reasons.push("affirmative_quote_crossed_or_invalid".to_owned());
            return None;
        }
        let quote_time = harvest.values.get("quote_time").and_then(Value::as_str);
        let age_ms = match quote_time.and_then(|text| age_ms(produced_at, text).ok()) {
            Some(age) => age,
            None => {
                reasons.push("affirmative_quote_missing".to_owned());
                return None;
            }
        };
        if age_ms > i64::from(u32::from(STALE_QUOTE_MS)) {
            reasons.push("affirmative_quote_stale".to_owned());
            return None;
        }
        Some(QuoteLeg {
            bid,
            ask,
            midpoint,
            negative_ask,
            age_ms,
        })
    }

    /// The bounded resume state (the two emitters' lifetime memory).
    pub fn checkpoint_state(&self) -> MultiCheckpointState {
        MultiCheckpointState {
            dislocation: EmitterCheckpoint {
                ever_ready: self.dislocation.ever_ready,
                lifetime_output_count: self.dislocation.lifetime_output_count,
            },
            pair_cost: EmitterCheckpoint {
                ever_ready: self.pair_cost.ever_ready,
                lifetime_output_count: self.pair_cost.lifetime_output_count,
            },
        }
    }

    /// Restore the bounded resume state written by [`Self::checkpoint_state`].
    pub fn restore_state(&mut self, state: MultiCheckpointState) {
        self.dislocation.ever_ready = state.dislocation.ever_ready;
        self.dislocation.lifetime_output_count = state.dislocation.lifetime_output_count;
        self.pair_cost.ever_ready = state.pair_cost.ever_ready;
        self.pair_cost.lifetime_output_count = state.pair_cost.lifetime_output_count;
    }

    /// Restart both blends' per-identity output sequences at zero for a new file
    /// generation (C5 Design B). The multi runtime keeps this warm `MultiProcessor`
    /// across an in-process rotation, so without this the blend emitters would
    /// carry their departed sequence into the successor's fresh output epoch,
    /// breaking the "first ready decision in an epoch is sequence zero" contract.
    /// The warm blend memory (`ever_ready`, lifetime counts, coalescing state) is
    /// left intact, exactly as the leg processors cross the boundary warm.
    pub fn reset_output_sequences(&mut self) {
        self.dislocation.reset_output_sequence();
        self.pair_cost.reset_output_sequence();
    }
}

/// Max quote age tolerated by both blends (§5.3/§5.4), in milliseconds.
const STALE_QUOTE_MS: u16 = 10_000;

/// The aligned two-leg pair for one decision.
#[derive(Debug, Clone)]
struct AlignedPair {
    kalshi: ContractLeg,
    polymarket: ContractLeg,
    window_open: String,
    window_close: String,
    close_ms: i64,
    produced_ms: i64,
}

/// One evaluated identity reading: the ready values (or the non-ready reasons).
struct Reading {
    values: Option<BTreeMap<String, Value>>,
    reasons: Vec<String>,
}

impl Reading {
    fn ready(values: BTreeMap<String, Value>) -> Self {
        Self {
            values: Some(values),
            reasons: Vec::new(),
        }
    }

    fn non_ready(reasons: Vec<String>) -> Self {
        Self {
            values: None,
            reasons,
        }
    }
}

/// A ready quote leg's carried and derived fields.
#[derive(Debug, Clone)]
struct QuoteLeg {
    bid: Decimal,
    ask: Decimal,
    midpoint: Decimal,
    negative_ask: Decimal,
    age_ms: i64,
}

/// A ready pair-cost-state leg's carried fields (venue-uniform: the Kalshi
/// `yes_leg_fee`/`no_leg_fee` and Polymarket `up_leg_fee`/`down_leg_fee` are read
/// into the same slots so the cross formulas stay one shape).
#[derive(Debug, Clone)]
struct PairCostLeg {
    pair_cost: Decimal,
    fee_adjusted_pair_cost: Decimal,
    /// The affirmative-leg fee (`yes_leg_fee` on Kalshi, `up_leg_fee` on Polymarket).
    yes_leg_fee: Decimal,
    /// The negative-leg fee (`no_leg_fee` on Kalshi, `down_leg_fee` on Polymarket).
    no_leg_fee: Decimal,
    /// Alias of `yes_leg_fee` for the Polymarket affirmative cross leg.
    up_leg_fee: Decimal,
    /// Alias of `no_leg_fee` for the Polymarket negative cross leg.
    down_leg_fee: Decimal,
    premium_threshold: Decimal,
}

fn fair_probability(leg: &LegState, reasons: &mut Vec<String>) -> Option<Decimal> {
    let Some(harvest) = leg.fair.as_ref() else {
        reasons.push("volatility_warming_up".to_owned());
        return None;
    };
    if !harvest.ready {
        if harvest.reasons.is_empty() {
            reasons.push("volatility_warming_up".to_owned());
        } else {
            reasons.extend(harvest.reasons.iter().cloned());
        }
        return None;
    }
    required_decimal(&harvest.values, "probability", reasons)
}

fn pair_cost_state(leg: &LegState, reasons: &mut Vec<String>) -> Option<PairCostLeg> {
    let Some(harvest) = leg.pair_cost.as_ref() else {
        reasons.push("affirmative_quote_missing".to_owned());
        return None;
    };
    if !harvest.ready {
        if harvest.reasons.is_empty() {
            reasons.push("affirmative_quote_missing".to_owned());
        } else {
            reasons.extend(harvest.reasons.iter().cloned());
        }
        return None;
    }
    let values = &harvest.values;
    let pair_cost = required_decimal(values, "pair_cost", reasons)?;
    let fee_adjusted_pair_cost = required_decimal(values, "fee_adjusted_pair_cost", reasons)?;
    let premium_threshold = required_decimal(values, "premium_threshold", reasons)?;
    // The affirmative/negative leg fees are named per venue; read whichever pair
    // the identity published into the venue-uniform slots.
    let affirmative_fee =
        optional_decimal(values, "yes_leg_fee").or_else(|| optional_decimal(values, "up_leg_fee"));
    let negative_fee =
        optional_decimal(values, "no_leg_fee").or_else(|| optional_decimal(values, "down_leg_fee"));
    let (Some(affirmative_fee), Some(negative_fee)) = (affirmative_fee, negative_fee) else {
        reasons.push("affirmative_quote_missing".to_owned());
        return None;
    };
    Some(PairCostLeg {
        pair_cost,
        fee_adjusted_pair_cost,
        yes_leg_fee: affirmative_fee,
        no_leg_fee: negative_fee,
        up_leg_fee: affirmative_fee,
        down_leg_fee: negative_fee,
        premium_threshold,
    })
}

fn parse_contract(values: &BTreeMap<String, Value>) -> Result<ContractLeg, MultiSignalError> {
    let text = |field: &str| -> Result<String, MultiSignalError> {
        values
            .get(field)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| {
                MultiSignalError::Computation(format!("market_contract missing {field}"))
            })
    };
    let market_generation = values
        .get("market_generation")
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            MultiSignalError::Computation("market_contract missing market_generation".to_owned())
        })?;
    let strike = decimal_field(values, "strike")?;
    Ok(ContractLeg {
        market_id: text("market_id")?,
        market_generation,
        window_open: text("window_open")?,
        window_close: text("window_close")?,
        strike,
        settlement_benchmark: text("settlement_benchmark")?,
        comparator: text("comparator")?,
        tie_outcome: text("tie_outcome")?,
        // A published `market_contract` is a bound, open market; a closed market
        // stops publishing the contract, so a harvested ready contract is open.
        open: true,
    })
}

fn decimal_field(
    values: &BTreeMap<String, Value>,
    field: &str,
) -> Result<Decimal, MultiSignalError> {
    optional_decimal(values, field)
        .ok_or_else(|| MultiSignalError::Computation(format!("undecodable decimal field {field}")))
}

fn optional_decimal(values: &BTreeMap<String, Value>, field: &str) -> Option<Decimal> {
    let raw = values.get(field)?;
    match raw {
        Value::String(text) => Decimal::from_str(text).ok(),
        Value::Number(number) => Decimal::from_str(&number.to_string()).ok(),
        _ => None,
    }
}

fn required_decimal(
    values: &BTreeMap<String, Value>,
    field: &'static str,
    reasons: &mut Vec<String>,
) -> Option<Decimal> {
    match optional_decimal(values, field) {
        Some(value) => Some(value),
        None => {
            reasons.push("affirmative_quote_crossed_or_invalid".to_owned());
            None
        }
    }
}

fn checked_div(numerator: Decimal, denominator: Decimal) -> Result<Decimal, MultiSignalError> {
    if denominator.is_zero() {
        return Err(MultiSignalError::Computation(
            "division by zero strike".to_owned(),
        ));
    }
    Ok(numerator / denominator)
}

fn decimal_value(value: Decimal) -> Value {
    json!(
        value
            .round_dp_with_strategy(OUTPUT_DECIMAL_PLACES, RoundingStrategy::MidpointNearestEven)
            .to_string()
    )
}

fn parse_time(text: &str) -> Result<DateTime<Utc>, MultiSignalError> {
    DateTime::parse_from_rfc3339(text)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|error| {
            MultiSignalError::Computation(format!("undecodable timestamp {text}: {error}"))
        })
}

fn rfc3339(time: DateTime<Utc>) -> String {
    time.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
}

fn millis(time: DateTime<Utc>) -> i64 {
    time.timestamp_millis()
}

fn age_ms(now: &str, then: &str) -> Result<i64, MultiSignalError> {
    Ok(millis(parse_time(now)?) - millis(parse_time(then)?))
}

#[cfg(test)]
mod tests;
