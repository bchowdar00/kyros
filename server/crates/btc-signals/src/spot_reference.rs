use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Duration as ChronoDuration, FixedOffset, SecondsFormat, Utc};
use rust_decimal::{Decimal, RoundingStrategy};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use signals_core::{
    Quality, QualityState, SIGNAL_UPDATE_SCHEMA_VERSION, SignalDefinition, SignalScope,
    SignalUpdate, SourceDefinition, SourceEvent, Validate, parse_decimal_text,
};
use thiserror::Error;

pub const SPOT_REFERENCE_ID: &str = "btc.external.spot_reference";
pub const SPOT_REFERENCE_VERSION: &str = "1.0.0";
pub const COINBASE_TICKER_SOURCE_ID: &str = "coinbase.exchange.btc-usd.ticker";
pub const COINBASE_TICKER_SOURCE_VERSION: &str = "1.0.0";
pub const KRAKEN_TICKER_SOURCE_ID: &str = "kraken.spot.btc-usd.ticker";
pub const KRAKEN_TICKER_SOURCE_VERSION: &str = "1.0.0";

pub const HEALTH_INTERVAL_NS: u64 = 1_000_000_000;
pub const STALENESS_BOUND_NS: u64 = 10_000_000_000;
pub const OUTPUT_DECIMAL_PLACES: u32 = 8;
pub const MAX_DISPERSION_BPS: u32 = 10;

const COINBASE_INSTRUMENT: &str = "BTC-USD";
const KRAKEN_INSTRUMENT: &str = "BTC/USD";
const ROUNDING_RULE: &str = "midpoint_nearest_even";

const SIGNAL_DEFINITION_JSON: &str =
    include_str!("../../../definitions/btc/signals/btc.external.spot_reference@1.0.0.json");
const COINBASE_DEFINITION_JSON: &str =
    include_str!("../../../definitions/btc/sources/coinbase.exchange.btc-usd.ticker@1.0.0.json");
const KRAKEN_DEFINITION_JSON: &str =
    include_str!("../../../definitions/btc/sources/kraken.spot.btc-usd.ticker@1.0.0.json");

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Venue {
    Coinbase,
    Kraken,
    // The rights-gated per-venue spot venues (B1). They publish their own
    // `btc.binance.*` / `btc.bybit.*` tier-1 identities on the venue lanes but
    // are NEVER constituents of the `btc.external.*` blend (R6): the blend stays
    // pinned to the two USD venues, so no reduction that defines the blend reads
    // these variants. See `BLEND_VENUES`.
    Binance,
    Bybit,
}

/// The constituents of the blended `btc.external.*` reference layer. Pinned to
/// the two USD venues by R6: adding a `Venue` variant must never widen the
/// blend's every-venue AND-gate, so every blend reduction reads this rather than
/// the full `Venue` set. USDT venues join a blend only through a tier-3
/// `btc.cross.*` identity that declares the conversion.
pub const BLEND_VENUES: [Venue; 2] = [Venue::Coinbase, Venue::Kraken];

// R6, enforced at compile time: the blend is exactly the two USD venues, in this
// order. A future edit that adds a `Venue` variant to the blend (widening the
// `btc.external.*` AND-gate) or reorders it fails the build here rather than
// silently changing every blend fingerprint.
const _: () = {
    assert!(BLEND_VENUES.len() == 2);
    assert!(matches!(BLEND_VENUES[0], Venue::Coinbase));
    assert!(matches!(BLEND_VENUES[1], Venue::Kraken));
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnavailableReason {
    Disconnected,
    Recovery,
    SubscriptionRejected,
}

/// Semantic settings included in the journal compatibility fingerprint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpotReferenceRuntimeConfig {
    pub health_interval_ns: u64,
    pub staleness_bound_ns: u64,
    pub maximum_dispersion_bps: u32,
    pub output_decimal_places: u32,
    pub decimal_rounding: String,
    pub coinbase_stream: String,
    pub kraken_stream: String,
    pub kraken_btc_book_depth: u16,
    /// Whether this session emits observe-only `feed_health` stream frames. It is
    /// a semantic capability that enters the contract fingerprint: a session that
    /// declares it emits, while a replay of a session recorded before the
    /// capability existed (the field absent, deserialized as `false`) never emits
    /// and stays byte-identical. Absent-and-false serializes to nothing, so a
    /// pre-capability config re-serializes to identical fingerprint bytes.
    #[serde(default, skip_serializing_if = "is_false")]
    pub feed_health_stream: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl SpotReferenceRuntimeConfig {
    pub fn new(kraken_btc_book_depth: u16) -> Result<Self, SpotReferenceError> {
        if !matches!(kraken_btc_book_depth, 10 | 25 | 100 | 500 | 1000) {
            return Err(SpotReferenceError::RuntimeConfig(
                "Kraken core depth must be one of 10, 25, 100, 500, or 1000".to_owned(),
            ));
        }
        Ok(Self {
            health_interval_ns: HEALTH_INTERVAL_NS,
            staleness_bound_ns: STALENESS_BOUND_NS,
            maximum_dispersion_bps: MAX_DISPERSION_BPS,
            output_decimal_places: OUTPUT_DECIMAL_PLACES,
            decimal_rounding: ROUNDING_RULE.to_owned(),
            coinbase_stream: "coinbase_core".to_owned(),
            kraken_stream: "kraken_core".to_owned(),
            kraken_btc_book_depth,
            feed_health_stream: true,
        })
    }

    pub fn validate(&self) -> Result<(), SpotReferenceError> {
        // The capability flag is intentionally exempt from the exact-match check:
        // both a declaring (new) session and a pre-capability (replayed) session
        // are valid, and only the flag may differ from a fresh build.
        let mut expected = Self::new(self.kraken_btc_book_depth)?;
        expected.feed_health_stream = self.feed_health_stream;
        if self == &expected {
            Ok(())
        } else {
            Err(SpotReferenceError::RuntimeConfig(
                "runtime configuration does not match this processor build".to_owned(),
            ))
        }
    }

    /// Whether two configurations are equal apart from the `feed_health_stream`
    /// capability. Resuming across the capability boundary (a pre-capability
    /// checkpoint under a declaring binary) is a legitimate additive upgrade, so
    /// the flag alone may differ.
    pub fn matches_ignoring_feed_health(&self, other: &Self) -> bool {
        let mut normalized = self.clone();
        normalized.feed_health_stream = other.feed_health_stream;
        &normalized == other
    }
}

pub(crate) fn contract_documents() -> [(&'static str, &'static str); 3] {
    [
        (SPOT_REFERENCE_ID, SIGNAL_DEFINITION_JSON),
        (COINBASE_TICKER_SOURCE_ID, COINBASE_DEFINITION_JSON),
        (KRAKEN_TICKER_SOURCE_ID, KRAKEN_DEFINITION_JSON),
    ]
}

#[derive(Debug, Clone)]
struct OrderingState {
    publisher_epoch: String,
    sequence: u64,
}

#[derive(Debug, Clone)]
struct Quote {
    bid: Decimal,
    ask: Decimal,
    midpoint: Decimal,
    event_time_text: String,
    event_time: DateTime<FixedOffset>,
    logical_receipt_ns: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InvalidQuoteReason {
    PriceShape,
    MidpointUnrepresentable,
}

#[derive(Debug, Clone)]
enum VenueStatus {
    Awaiting,
    Valid(Quote),
    Invalid(InvalidQuoteReason),
    Unavailable(UnavailableReason),
}

#[derive(Debug, Clone)]
struct VenueState {
    status: VenueStatus,
    ordering: Option<OrderingState>,
}

impl Default for VenueState {
    fn default() -> Self {
        Self {
            status: VenueStatus::Awaiting,
            ordering: None,
        }
    }
}

/// One venue's fresh displayed best quote, projected out of the reference
/// processor for the per-venue spot identities (ADR 0024).
#[derive(Debug, Clone)]
pub(crate) struct VenueQuoteSnapshot {
    pub(crate) bid: Decimal,
    pub(crate) ask: Decimal,
    pub(crate) midpoint: Decimal,
    pub(crate) event_time: String,
    pub(crate) age_ms: u64,
}

#[derive(Debug, Clone)]
enum EvaluatedQuote<'a> {
    Fresh { quote: &'a Quote, age_ms: u64 },
    Stale,
    Awaiting,
    Invalid(InvalidQuoteReason),
    Unavailable(UnavailableReason),
}

impl EvaluatedQuote<'_> {
    fn fresh(&self) -> Option<(&Quote, u64)> {
        match self {
            Self::Fresh { quote, age_ms } => Some((quote, *age_ms)),
            Self::Stale | Self::Awaiting | Self::Invalid(_) | Self::Unavailable(_) => None,
        }
    }
}

enum ParsedQuote {
    Valid(Quote),
    Invalid(InvalidQuoteReason),
}

/// Version-independent processor state carried by a durable checkpoint.
///
/// Output identity is recorded for auditability but is never reused on
/// restore. A resumed process receives a new publisher epoch and starts its
/// output sequence at zero.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpotReferenceCheckpointState {
    pub publisher_epoch_lineage: Vec<String>,
    pub previous_next_sequence: u64,
    coinbase: CheckpointVenueState,
    kraken: CheckpointVenueState,
    last_observation_time: Option<String>,
    last_quality: Option<Quality>,
    quality_reason_entries: BTreeMap<String, u64>,
    last_now_ns: Option<u64>,
    ever_ready: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckpointVenueState {
    status: CheckpointVenueStatus,
    ordering: Option<CheckpointOrderingState>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckpointOrderingState {
    publisher_epoch: String,
    sequence: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
enum CheckpointVenueStatus {
    Awaiting,
    Valid {
        bid: String,
        ask: String,
        event_time: String,
        logical_receipt_ns: u64,
    },
    Invalid {
        reason: CheckpointInvalidQuoteReason,
    },
    Unavailable {
        reason: UnavailableReason,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CheckpointInvalidQuoteReason {
    PriceShape,
    MidpointUnrepresentable,
}

/// Two-venue, exact-decimal BTC/USD reference processor.
///
/// `now_ns` and `logical_receipt_ns` share one monotonic process origin.
/// Provider and local wall timestamps remain output/provenance metadata only.
pub struct SpotReferenceProcessor {
    publisher_epoch: String,
    publisher_epoch_lineage: Vec<String>,
    wall_start: DateTime<Utc>,
    next_sequence: u64,
    definition: SignalDefinition,
    coinbase_definition: SourceDefinition,
    kraken_definition: SourceDefinition,
    coinbase: VenueState,
    kraken: VenueState,
    last_observation_time: Option<(String, DateTime<FixedOffset>)>,
    last_quality: Option<Quality>,
    quality_reason_entries: BTreeMap<String, u64>,
    last_now_ns: Option<u64>,
    ever_ready: bool,
}

impl SpotReferenceProcessor {
    pub fn new(
        publisher_epoch: impl Into<String>,
        wall_start: DateTime<Utc>,
    ) -> Result<Self, SpotReferenceError> {
        let publisher_epoch = publisher_epoch.into();
        validate_epoch(&publisher_epoch)?;

        let definition = parse_definition::<SignalDefinition>(SIGNAL_DEFINITION_JSON, "signal")?;
        let coinbase_definition =
            parse_definition::<SourceDefinition>(COINBASE_DEFINITION_JSON, "Coinbase source")?;
        let kraken_definition =
            parse_definition::<SourceDefinition>(KRAKEN_DEFINITION_JSON, "Kraken source")?;

        require_definition_identity(
            &definition.signal_id,
            &definition.version,
            SPOT_REFERENCE_ID,
            SPOT_REFERENCE_VERSION,
            "signal",
        )?;
        require_definition_identity(
            &coinbase_definition.source_id,
            &coinbase_definition.version,
            COINBASE_TICKER_SOURCE_ID,
            COINBASE_TICKER_SOURCE_VERSION,
            "Coinbase source",
        )?;
        require_definition_identity(
            &kraken_definition.source_id,
            &kraken_definition.version,
            KRAKEN_TICKER_SOURCE_ID,
            KRAKEN_TICKER_SOURCE_VERSION,
            "Kraken source",
        )?;
        if definition.staleness_bound_ms
            != STALENESS_BOUND_NS
                .checked_div(1_000_000)
                .ok_or(SpotReferenceError::CounterExhausted)?
        {
            return Err(SpotReferenceError::Definition(
                "signal staleness bound does not match the processor".to_owned(),
            ));
        }

        Ok(Self {
            publisher_epoch_lineage: vec![publisher_epoch.clone()],
            publisher_epoch,
            wall_start,
            next_sequence: 0,
            definition,
            coinbase_definition,
            kraken_definition,
            coinbase: VenueState::default(),
            kraken: VenueState::default(),
            last_observation_time: None,
            last_quality: None,
            quality_reason_entries: BTreeMap::new(),
            last_now_ns: None,
            ever_ready: false,
        })
    }

    pub fn accepts(source_id: &str) -> bool {
        matches!(
            source_id,
            COINBASE_TICKER_SOURCE_ID | KRAKEN_TICKER_SOURCE_ID
        )
    }

    pub fn definition(&self) -> &SignalDefinition {
        &self.definition
    }

    /// The two consumed ticker source definitions, carrying their declared
    /// `expected_heartbeat_ms` liveness contracts.
    pub fn source_definitions(&self) -> [&SourceDefinition; 2] {
        [&self.coinbase_definition, &self.kraken_definition]
    }

    /// Stream-v3 spot-feature availability is driven by the same evaluated
    /// quote availability state as the reference itself, never by book state.
    pub fn venue_is_unavailable(&self, venue: Venue) -> bool {
        matches!(self.state(venue).status, VenueStatus::Unavailable(_))
    }

    /// This venue's displayed best quote when it is fresh at `now_ns`.
    ///
    /// Read-only projection of the same evaluated quote state the reference
    /// blend consumes, so a per-venue identity and the blended reference never
    /// disagree about whether a constituent is fresh (STANDING-PRACTICES rule 6,
    /// one authority per fact). Returns `None` for every non-fresh state —
    /// awaiting, stale, invalid, or unavailable.
    pub(crate) fn venue_quote(
        &self,
        venue: Venue,
        now_ns: u64,
    ) -> Result<Option<VenueQuoteSnapshot>, SpotReferenceError> {
        let evaluated = evaluate_quote(&self.state(venue).status, now_ns)?;
        Ok(evaluated.fresh().map(|(quote, age_ms)| VenueQuoteSnapshot {
            bid: quote.bid,
            ask: quote.ask,
            midpoint: quote.midpoint,
            event_time: quote.event_time_text.clone(),
            age_ms,
        }))
    }

    pub fn quality_reason_entries(&self) -> &BTreeMap<String, u64> {
        &self.quality_reason_entries
    }

    pub fn last_now_ns(&self) -> Option<u64> {
        self.last_now_ns
    }

    /// Capture the complete deterministic calculation state.
    pub fn checkpoint_state(&self) -> SpotReferenceCheckpointState {
        SpotReferenceCheckpointState {
            publisher_epoch_lineage: self.publisher_epoch_lineage.clone(),
            previous_next_sequence: self.next_sequence,
            coinbase: CheckpointVenueState::from_runtime(&self.coinbase),
            kraken: CheckpointVenueState::from_runtime(&self.kraken),
            last_observation_time: self
                .last_observation_time
                .as_ref()
                .map(|(text, _)| text.clone()),
            last_quality: self.last_quality.clone(),
            quality_reason_entries: self.quality_reason_entries.clone(),
            last_now_ns: self.last_now_ns,
            ever_ready: self.ever_ready,
        }
    }

    /// Restore calculation state under a new, non-colliding output identity.
    pub fn restore(
        publisher_epoch: impl Into<String>,
        wall_start: DateTime<Utc>,
        state: SpotReferenceCheckpointState,
    ) -> Result<Self, SpotReferenceError> {
        state.validate()?;
        let publisher_epoch = publisher_epoch.into();
        validate_epoch(&publisher_epoch)?;
        if state
            .publisher_epoch_lineage
            .iter()
            .any(|prior| prior == &publisher_epoch)
        {
            return Err(SpotReferenceError::ReusedPublisherEpoch);
        }
        let mut publisher_epoch_lineage = state.publisher_epoch_lineage;
        publisher_epoch_lineage.push(publisher_epoch.clone());
        let mut processor = Self::new(publisher_epoch, wall_start)?;
        processor.publisher_epoch_lineage = publisher_epoch_lineage;
        processor.coinbase = state.coinbase.into_runtime()?;
        processor.kraken = state.kraken.into_runtime()?;
        processor.last_observation_time = state
            .last_observation_time
            .map(|text| {
                parse_timestamp(&text, "last_observation_time").map(|parsed| (text, parsed))
            })
            .transpose()?;
        processor.last_quality = state.last_quality;
        processor.quality_reason_entries = state.quality_reason_entries;
        processor.last_now_ns = state.last_now_ns;
        processor.ever_ready = state.ever_ready;
        // A new publisher epoch makes sequence zero unique to this run. The
        // prior next sequence remains in the checkpoint for audit only.
        processor.next_sequence = 0;
        Ok(processor)
    }

    /// Rotate to a new output publisher epoch in place, keeping every warm
    /// calculation ring and window intact (C5 in-process file-generation
    /// rotation).
    ///
    /// Unlike [`SpotReferenceProcessor::restore`], which rebuilds from a
    /// checkpoint and re-warms the rings, this keeps the live processor's warm
    /// state and only re-seats the output identity exactly as a restart would:
    /// the output sequence restarts at zero under a fresh, non-colliding epoch
    /// and that epoch is appended to the lineage. It is the reference side of
    /// the runtime's per-generation reset; the rolling state stays warm so an
    /// identity that was Ready before the boundary is Ready in the first
    /// decision after it.
    pub fn rotate_output_epoch(
        &mut self,
        publisher_epoch: impl Into<String>,
    ) -> Result<(), SpotReferenceError> {
        let publisher_epoch = publisher_epoch.into();
        validate_epoch(&publisher_epoch)?;
        if self
            .publisher_epoch_lineage
            .iter()
            .any(|prior| prior == &publisher_epoch)
        {
            return Err(SpotReferenceError::ReusedPublisherEpoch);
        }
        self.publisher_epoch_lineage.push(publisher_epoch.clone());
        self.publisher_epoch = publisher_epoch;
        self.next_sequence = 0;
        Ok(())
    }

    /// Apply one normalized market event at its admitted logical time.
    pub fn ingest(
        &mut self,
        event: &SourceEvent,
        logical_receipt_ns: u64,
        now_ns: u64,
    ) -> Result<Option<SignalUpdate>, SpotReferenceError> {
        let venue = match event.source_id.as_str() {
            COINBASE_TICKER_SOURCE_ID => Venue::Coinbase,
            KRAKEN_TICKER_SOURCE_ID => Venue::Kraken,
            _ => return Ok(None),
        };
        self.admit_time(now_ns)?;
        if logical_receipt_ns > now_ns {
            return Err(SpotReferenceError::ReceiptAfterAdmission {
                receipt_ns: logical_receipt_ns,
                now_ns,
            });
        }

        let expected_version = match venue {
            Venue::Coinbase => COINBASE_TICKER_SOURCE_VERSION,
            Venue::Kraken => KRAKEN_TICKER_SOURCE_VERSION,
            Venue::Binance | Venue::Bybit => unreachable!(
                "the spot reference blend is pinned to the USD venues (BLEND_VENUES); \
                 a rights-gated venue never reaches this path"
            ),
        };
        if event.source_version != expected_version {
            return Err(SpotReferenceError::InputVersion {
                expected: expected_version,
            });
        }
        match venue {
            Venue::Coinbase => event.validate_against(&self.coinbase_definition)?,
            Venue::Kraken => event.validate_against(&self.kraken_definition)?,
            Venue::Binance | Venue::Bybit => unreachable!(
                "the spot reference blend is pinned to the USD venues (BLEND_VENUES); \
                 a rights-gated venue never reaches this path"
            ),
        }

        let event_time = parse_timestamp(&event.event_time, "event_time")?;
        let quote = quote_from_event(venue, event, logical_receipt_ns, event_time)?;
        if !self.accept_ordering(venue, event) {
            return Ok(None);
        }

        self.observe_time(event.event_time.clone(), event_time);
        let quote_changed = match quote {
            ParsedQuote::Valid(quote) => {
                let changed =
                    self.state(venue).status.as_valid().is_none_or(|previous| {
                        previous.bid != quote.bid || previous.ask != quote.ask
                    });
                self.state_mut(venue).status = VenueStatus::Valid(quote);
                changed
            }
            ParsedQuote::Invalid(reason) => {
                let changed = !matches!(
                    self.state(venue).status,
                    VenueStatus::Invalid(existing) if existing == reason
                );
                self.state_mut(venue).status = VenueStatus::Invalid(reason);
                changed
            }
        };

        self.evaluate(now_ns, quote_changed)
    }

    /// Re-evaluate staleness at an actually admitted health-tick time.
    pub fn advance_time(
        &mut self,
        now_ns: u64,
    ) -> Result<Option<SignalUpdate>, SpotReferenceError> {
        self.admit_time(now_ns)?;
        self.evaluate(now_ns, true)
    }

    /// Invalidate one venue after a connection or recovery failure.
    pub fn mark_unavailable(
        &mut self,
        venue: Venue,
        reason: UnavailableReason,
        now_ns: u64,
    ) -> Result<Option<SignalUpdate>, SpotReferenceError> {
        self.admit_time(now_ns)?;
        let changed = !matches!(
            self.state(venue).status,
            VenueStatus::Unavailable(existing) if existing == reason
        );
        self.state_mut(venue).status = VenueStatus::Unavailable(reason);
        self.evaluate(now_ns, changed)
    }

    fn admit_time(&mut self, now_ns: u64) -> Result<(), SpotReferenceError> {
        if let Some(previous) = self.last_now_ns
            && now_ns < previous
        {
            return Err(SpotReferenceError::ClockRegression {
                previous_ns: previous,
                now_ns,
            });
        }
        self.last_now_ns = Some(now_ns);
        Ok(())
    }

    fn accept_ordering(&mut self, venue: Venue, event: &SourceEvent) -> bool {
        let state = self.state_mut(venue);
        if let Some(ordering) = &state.ordering {
            if ordering.publisher_epoch == event.publisher_epoch {
                if event.sequence <= ordering.sequence {
                    return false;
                }
            } else {
                state.status = VenueStatus::Awaiting;
            }
        }
        state.ordering = Some(OrderingState {
            publisher_epoch: event.publisher_epoch.clone(),
            sequence: event.sequence,
        });
        true
    }

    fn evaluate(
        &mut self,
        now_ns: u64,
        emission_requested: bool,
    ) -> Result<Option<SignalUpdate>, SpotReferenceError> {
        let coinbase = evaluate_quote(&self.coinbase.status, now_ns)?;
        let kraken = evaluate_quote(&self.kraken.status, now_ns)?;
        let mut reasons = Vec::with_capacity(3);
        append_reason(&mut reasons, Venue::Coinbase, &coinbase);
        append_reason(&mut reasons, Venue::Kraken, &kraken);

        let mut values = BTreeMap::new();
        let mut healthy_venue_count = 0_u8;
        let mut maximum_age_ms: Option<u64> = None;
        let mut reference_time = None;

        for (name, evaluated) in [("coinbase_mid", &coinbase), ("kraken_mid", &kraken)] {
            if let Some((quote, age_ms)) = evaluated.fresh() {
                healthy_venue_count += 1;
                maximum_age_ms = Some(maximum_age_ms.map_or(age_ms, |age| age.max(age_ms)));
                if reference_time.as_ref().is_none_or(
                    |(_, current): &(String, DateTime<FixedOffset>)| quote.event_time > *current,
                ) {
                    reference_time = Some((quote.event_time_text.clone(), quote.event_time));
                }
                values.insert(
                    name.to_owned(),
                    json!(decimal_text(round_output(quote.midpoint))),
                );
            }
        }

        let mut reference_ready = false;
        if let (Some((coinbase_quote, _)), Some((kraken_quote, _))) =
            (coinbase.fresh(), kraken.fresh())
        {
            match reference_metrics(coinbase_quote.midpoint, kraken_quote.midpoint) {
                Some((reference, dispersion)) => {
                    values.insert(
                        "dispersion_bps".to_owned(),
                        json!(decimal_text(round_output(dispersion))),
                    );
                    if dispersion <= Decimal::from(MAX_DISPERSION_BPS) {
                        values.insert(
                            "reference_price".to_owned(),
                            json!(decimal_text(round_output(reference))),
                        );
                        reference_ready = true;
                    } else {
                        reasons.push("cross_venue_dispersion_exceeded".to_owned());
                    }
                }
                None => reasons.push("reference_arithmetic_unrepresentable".to_owned()),
            }
        }

        values.insert("healthy_venue_count".to_owned(), json!(healthy_venue_count));
        if let Some(age_ms) = maximum_age_ms {
            values.insert("max_input_age_ms".to_owned(), json!(age_ms));
        }
        if let Some((text, _)) = &reference_time {
            values.insert("reference_time".to_owned(), json!(text));
        }

        let produced_at = self.logical_datetime(now_ns)?;
        let produced_at_text = produced_at.to_rfc3339_opts(SecondsFormat::Nanos, true);
        let represented_time = reference_time
            .or_else(|| self.last_observation_time.clone())
            .unwrap_or_else(|| (produced_at_text.clone(), produced_at.fixed_offset()));

        let quality = if reference_ready && reasons.is_empty() {
            Quality {
                state: QualityState::Ready,
                reasons,
            }
        } else {
            let has_hard_failure = self.ever_ready
                || matches!(
                    coinbase,
                    EvaluatedQuote::Stale
                        | EvaluatedQuote::Invalid(_)
                        | EvaluatedQuote::Unavailable(_)
                )
                || matches!(
                    kraken,
                    EvaluatedQuote::Stale
                        | EvaluatedQuote::Invalid(_)
                        | EvaluatedQuote::Unavailable(_)
                )
                || reasons.iter().any(|reason| {
                    matches!(
                        reason.as_str(),
                        "cross_venue_dispersion_exceeded" | "reference_arithmetic_unrepresentable"
                    )
                });
            Quality {
                state: if has_hard_failure {
                    QualityState::Degraded
                } else {
                    QualityState::WarmingUp
                },
                reasons,
            }
        };

        let quality_changed = self.last_quality.as_ref() != Some(&quality);
        if !emission_requested && !quality_changed {
            return Ok(None);
        }

        let (valid_from, valid_until) =
            SignalUpdate::validity_window(&produced_at_text, self.definition.staleness_bound_ms)?;
        let update = SignalUpdate {
            schema_version: SIGNAL_UPDATE_SCHEMA_VERSION,
            signal_id: SPOT_REFERENCE_ID.to_owned(),
            signal_version: SPOT_REFERENCE_VERSION.to_owned(),
            publisher_epoch: self.publisher_epoch.clone(),
            sequence: self.next_sequence,
            event_time: represented_time.0,
            produced_at: produced_at_text,
            valid_from,
            valid_until,
            quality: quality.clone(),
            values,
            scope: SignalScope::Global,
        };
        update.validate_against(&self.definition)?;

        self.record_reason_entries(&quality)?;
        if quality.state == QualityState::Ready {
            self.ever_ready = true;
        }
        if quality.state == QualityState::Ready {
            self.next_sequence = self
                .next_sequence
                .checked_add(1)
                .ok_or(SpotReferenceError::SequenceExhausted)?;
        }
        self.last_quality = Some(quality);
        Ok(Some(update))
    }

    fn logical_datetime(&self, now_ns: u64) -> Result<DateTime<Utc>, SpotReferenceError> {
        let delta = ChronoDuration::nanoseconds(
            i64::try_from(now_ns).map_err(|_| SpotReferenceError::TimestampRange)?,
        );
        self.wall_start
            .checked_add_signed(delta)
            .ok_or(SpotReferenceError::TimestampRange)
    }

    fn record_reason_entries(&mut self, quality: &Quality) -> Result<(), SpotReferenceError> {
        let prior: BTreeSet<&str> = self
            .last_quality
            .as_ref()
            .into_iter()
            .flat_map(|quality| quality.reasons.iter().map(String::as_str))
            .collect();
        for reason in &quality.reasons {
            if !prior.contains(reason.as_str()) {
                let counter = self
                    .quality_reason_entries
                    .entry(reason.clone())
                    .or_default();
                *counter = counter
                    .checked_add(1)
                    .ok_or(SpotReferenceError::CounterExhausted)?;
            }
        }
        Ok(())
    }

    fn observe_time(&mut self, text: String, parsed: DateTime<FixedOffset>) {
        if self
            .last_observation_time
            .as_ref()
            .is_none_or(|(_, current)| parsed > *current)
        {
            self.last_observation_time = Some((text, parsed));
        }
    }

    fn state(&self, venue: Venue) -> &VenueState {
        match venue {
            Venue::Coinbase => &self.coinbase,
            Venue::Kraken => &self.kraken,
            Venue::Binance | Venue::Bybit => unreachable!(
                "the spot reference blend is pinned to the USD venues (BLEND_VENUES); \
                 a rights-gated venue never reaches this path"
            ),
        }
    }

    fn state_mut(&mut self, venue: Venue) -> &mut VenueState {
        match venue {
            Venue::Coinbase => &mut self.coinbase,
            Venue::Kraken => &mut self.kraken,
            Venue::Binance | Venue::Bybit => unreachable!(
                "the spot reference blend is pinned to the USD venues (BLEND_VENUES); \
                 a rights-gated venue never reaches this path"
            ),
        }
    }
}

impl SpotReferenceCheckpointState {
    pub fn last_now_ns(&self) -> Option<u64> {
        self.last_now_ns
    }

    pub fn validate(&self) -> Result<(), SpotReferenceError> {
        if self.publisher_epoch_lineage.is_empty() {
            return Err(SpotReferenceError::Checkpoint(
                "publisher epoch lineage must contain the current epoch".to_owned(),
            ));
        }
        let mut unique_epochs = BTreeSet::new();
        for epoch in &self.publisher_epoch_lineage {
            validate_epoch(epoch)?;
            if !unique_epochs.insert(epoch.as_str()) {
                return Err(SpotReferenceError::Checkpoint(
                    "publisher epoch lineage cannot contain duplicates".to_owned(),
                ));
            }
        }
        self.coinbase.validate(self.last_now_ns)?;
        self.kraken.validate(self.last_now_ns)?;
        if let Some(text) = &self.last_observation_time {
            parse_timestamp(text, "last_observation_time")?;
        }
        if let Some(quality) = &self.last_quality {
            if quality.reasons.iter().any(|reason| reason.is_empty()) {
                return Err(SpotReferenceError::Checkpoint(
                    "quality reasons cannot be empty strings".to_owned(),
                ));
            }
            let mut unique = BTreeSet::new();
            if quality
                .reasons
                .iter()
                .any(|reason| !unique.insert(reason.as_str()))
            {
                return Err(SpotReferenceError::Checkpoint(
                    "quality reasons must be unique".to_owned(),
                ));
            }
        }
        if self
            .quality_reason_entries
            .iter()
            .any(|(reason, count)| reason.is_empty() || *count == 0)
        {
            return Err(SpotReferenceError::Checkpoint(
                "quality transition counters require non-empty reasons and positive counts"
                    .to_owned(),
            ));
        }
        Ok(())
    }
}

impl CheckpointVenueState {
    fn from_runtime(state: &VenueState) -> Self {
        let status = match &state.status {
            VenueStatus::Awaiting => CheckpointVenueStatus::Awaiting,
            VenueStatus::Valid(quote) => CheckpointVenueStatus::Valid {
                bid: decimal_text(quote.bid),
                ask: decimal_text(quote.ask),
                event_time: quote.event_time_text.clone(),
                logical_receipt_ns: quote.logical_receipt_ns,
            },
            VenueStatus::Invalid(reason) => CheckpointVenueStatus::Invalid {
                reason: (*reason).into(),
            },
            VenueStatus::Unavailable(reason) => {
                CheckpointVenueStatus::Unavailable { reason: *reason }
            }
        };
        Self {
            status,
            ordering: state
                .ordering
                .as_ref()
                .map(|ordering| CheckpointOrderingState {
                    publisher_epoch: ordering.publisher_epoch.clone(),
                    sequence: ordering.sequence,
                }),
        }
    }

    fn validate(&self, last_now_ns: Option<u64>) -> Result<(), SpotReferenceError> {
        if let Some(ordering) = &self.ordering {
            validate_epoch(&ordering.publisher_epoch)?;
        }
        if !matches!(self.status, CheckpointVenueStatus::Awaiting) && last_now_ns.is_none() {
            return Err(SpotReferenceError::Checkpoint(
                "non-awaiting venue state requires an admitted logical time".to_owned(),
            ));
        }
        if let CheckpointVenueStatus::Valid {
            bid,
            ask,
            event_time,
            logical_receipt_ns,
        } = &self.status
        {
            let bid = parse_decimal_text(bid)?;
            let ask = parse_decimal_text(ask)?;
            if bid <= Decimal::ZERO || ask <= Decimal::ZERO || bid > ask {
                return Err(SpotReferenceError::Checkpoint(
                    "checkpoint quote must be positive and uncrossed".to_owned(),
                ));
            }
            exact_mean(bid, ask).ok_or_else(|| {
                SpotReferenceError::Checkpoint(
                    "checkpoint quote midpoint is not exactly representable".to_owned(),
                )
            })?;
            parse_timestamp(event_time, "checkpoint_event_time")?;
            if last_now_ns.is_none_or(|now_ns| *logical_receipt_ns > now_ns) {
                return Err(SpotReferenceError::Checkpoint(
                    "checkpoint quote receipt exceeds its last admitted logical time".to_owned(),
                ));
            }
        }
        Ok(())
    }

    fn into_runtime(self) -> Result<VenueState, SpotReferenceError> {
        let status = match self.status {
            CheckpointVenueStatus::Awaiting => VenueStatus::Awaiting,
            CheckpointVenueStatus::Valid {
                bid,
                ask,
                event_time,
                logical_receipt_ns,
            } => {
                let bid = parse_decimal_text(&bid)?;
                let ask = parse_decimal_text(&ask)?;
                let midpoint = exact_mean(bid, ask).ok_or_else(|| {
                    SpotReferenceError::Checkpoint(
                        "checkpoint quote midpoint is not exactly representable".to_owned(),
                    )
                })?;
                let parsed = parse_timestamp(&event_time, "checkpoint_event_time")?;
                VenueStatus::Valid(Quote {
                    bid,
                    ask,
                    midpoint,
                    event_time_text: event_time,
                    event_time: parsed,
                    logical_receipt_ns,
                })
            }
            CheckpointVenueStatus::Invalid { reason } => VenueStatus::Invalid(reason.into()),
            CheckpointVenueStatus::Unavailable { reason } => VenueStatus::Unavailable(reason),
        };
        Ok(VenueState {
            status,
            ordering: self.ordering.map(|ordering| OrderingState {
                publisher_epoch: ordering.publisher_epoch,
                sequence: ordering.sequence,
            }),
        })
    }
}

impl From<InvalidQuoteReason> for CheckpointInvalidQuoteReason {
    fn from(value: InvalidQuoteReason) -> Self {
        match value {
            InvalidQuoteReason::PriceShape => Self::PriceShape,
            InvalidQuoteReason::MidpointUnrepresentable => Self::MidpointUnrepresentable,
        }
    }
}

impl From<CheckpointInvalidQuoteReason> for InvalidQuoteReason {
    fn from(value: CheckpointInvalidQuoteReason) -> Self {
        match value {
            CheckpointInvalidQuoteReason::PriceShape => Self::PriceShape,
            CheckpointInvalidQuoteReason::MidpointUnrepresentable => Self::MidpointUnrepresentable,
        }
    }
}

impl VenueStatus {
    fn as_valid(&self) -> Option<&Quote> {
        match self {
            Self::Valid(quote) => Some(quote),
            Self::Awaiting | Self::Invalid(_) | Self::Unavailable(_) => None,
        }
    }
}

fn parse_definition<T>(raw: &str, name: &str) -> Result<T, SpotReferenceError>
where
    T: serde::de::DeserializeOwned + Validate,
{
    let definition: T = serde_json::from_str(raw)
        .map_err(|error| SpotReferenceError::Definition(format!("invalid {name} JSON: {error}")))?;
    definition
        .validate()
        .map_err(|error| SpotReferenceError::Definition(format!("invalid {name}: {error}")))?;
    Ok(definition)
}

fn validate_epoch(value: &str) -> Result<(), SpotReferenceError> {
    if value.trim().is_empty() || value.len() > 128 {
        Err(SpotReferenceError::InvalidPublisherEpoch)
    } else {
        Ok(())
    }
}

fn require_definition_identity(
    actual_id: &str,
    actual_version: &str,
    expected_id: &str,
    expected_version: &str,
    name: &str,
) -> Result<(), SpotReferenceError> {
    if actual_id == expected_id && actual_version == expected_version {
        Ok(())
    } else {
        Err(SpotReferenceError::Definition(format!(
            "{name} identity is {actual_id}@{actual_version}; expected {expected_id}@{expected_version}"
        )))
    }
}

fn quote_from_event(
    venue: Venue,
    event: &SourceEvent,
    logical_receipt_ns: u64,
    event_time: DateTime<FixedOffset>,
) -> Result<ParsedQuote, SpotReferenceError> {
    let expected_instrument = match venue {
        Venue::Coinbase => COINBASE_INSTRUMENT,
        Venue::Kraken => KRAKEN_INSTRUMENT,
        Venue::Binance | Venue::Bybit => unreachable!(
            "the spot reference blend is pinned to the USD venues (BLEND_VENUES); \
             a rights-gated venue never reaches this path"
        ),
    };
    let instrument = required_text(&event.values, "instrument")?;
    if instrument != expected_instrument {
        return Err(SpotReferenceError::Instrument {
            expected: expected_instrument,
        });
    }
    let bid = parse_decimal_text(required_text(&event.values, "best_bid")?)?;
    let ask = parse_decimal_text(required_text(&event.values, "best_ask")?)?;
    if bid <= Decimal::ZERO || ask <= Decimal::ZERO || bid > ask {
        return Ok(ParsedQuote::Invalid(InvalidQuoteReason::PriceShape));
    }
    let Some(midpoint) = exact_mean(bid, ask) else {
        return Ok(ParsedQuote::Invalid(
            InvalidQuoteReason::MidpointUnrepresentable,
        ));
    };
    Ok(ParsedQuote::Valid(Quote {
        bid,
        ask,
        midpoint,
        event_time_text: event.event_time.clone(),
        event_time,
        logical_receipt_ns,
    }))
}

fn required_text<'a>(
    values: &'a BTreeMap<String, Value>,
    name: &str,
) -> Result<&'a str, SpotReferenceError> {
    values
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| SpotReferenceError::MissingField(name.to_owned()))
}

fn exact_mean(left: Decimal, right: Decimal) -> Option<Decimal> {
    let (lower, upper) = if left <= right {
        (left, right)
    } else {
        (right, left)
    };
    let difference = upper.checked_sub(lower)?;
    let half_difference = difference.checked_div(Decimal::from(2_u8))?;
    if half_difference.checked_mul(Decimal::from(2_u8)) != Some(difference) {
        return None;
    }
    let mean = lower.checked_add(half_difference)?;
    (mean.checked_sub(lower) == Some(half_difference)).then_some(mean)
}

fn reference_metrics(left: Decimal, right: Decimal) -> Option<(Decimal, Decimal)> {
    let reference = exact_mean(left, right)?;
    let (lower, upper) = if left <= right {
        (left, right)
    } else {
        (right, left)
    };
    let difference = upper.checked_sub(lower)?;
    let dispersion = difference
        .checked_div(reference)?
        .checked_mul(Decimal::from(10_000_u32))?;
    Some((reference, dispersion))
}

fn round_output(value: Decimal) -> Decimal {
    value.round_dp_with_strategy(OUTPUT_DECIMAL_PLACES, RoundingStrategy::MidpointNearestEven)
}

fn decimal_text(value: Decimal) -> String {
    value.normalize().to_string()
}

fn parse_timestamp(
    value: &str,
    field: &'static str,
) -> Result<DateTime<FixedOffset>, SpotReferenceError> {
    DateTime::parse_from_rfc3339(value).map_err(|error| SpotReferenceError::Timestamp {
        field,
        detail: error.to_string(),
    })
}

fn evaluate_quote(
    status: &VenueStatus,
    now_ns: u64,
) -> Result<EvaluatedQuote<'_>, SpotReferenceError> {
    match status {
        VenueStatus::Awaiting => Ok(EvaluatedQuote::Awaiting),
        VenueStatus::Invalid(reason) => Ok(EvaluatedQuote::Invalid(*reason)),
        VenueStatus::Unavailable(reason) => Ok(EvaluatedQuote::Unavailable(*reason)),
        VenueStatus::Valid(quote) => {
            let age_ns = now_ns.checked_sub(quote.logical_receipt_ns).ok_or(
                SpotReferenceError::ReceiptAfterAdmission {
                    receipt_ns: quote.logical_receipt_ns,
                    now_ns,
                },
            )?;
            if age_ns <= STALENESS_BOUND_NS {
                Ok(EvaluatedQuote::Fresh {
                    quote,
                    age_ms: age_ns / 1_000_000,
                })
            } else {
                Ok(EvaluatedQuote::Stale)
            }
        }
    }
}

fn append_reason(reasons: &mut Vec<String>, venue: Venue, quote: &EvaluatedQuote<'_>) {
    let prefix = match venue {
        Venue::Coinbase => "coinbase",
        Venue::Kraken => "kraken",
        Venue::Binance | Venue::Bybit => unreachable!(
            "the spot reference blend is pinned to the USD venues (BLEND_VENUES); \
             a rights-gated venue never reaches this path"
        ),
    };
    let suffix = match quote {
        EvaluatedQuote::Fresh { .. } => return,
        EvaluatedQuote::Stale => "stale",
        EvaluatedQuote::Awaiting => "warming_up",
        EvaluatedQuote::Invalid(InvalidQuoteReason::PriceShape) => "invalid_quote",
        EvaluatedQuote::Invalid(InvalidQuoteReason::MidpointUnrepresentable) => {
            "midpoint_unrepresentable"
        }
        EvaluatedQuote::Unavailable(UnavailableReason::Disconnected) => "disconnected",
        EvaluatedQuote::Unavailable(UnavailableReason::Recovery) => "recovery_required",
        EvaluatedQuote::Unavailable(UnavailableReason::SubscriptionRejected) => {
            "subscription_rejected"
        }
    };
    reasons.push(format!("{prefix}_{suffix}"));
}

#[derive(Debug, Error)]
pub enum SpotReferenceError {
    #[error("publisher_epoch must contain 1 to 128 bytes")]
    InvalidPublisherEpoch,
    #[error("publisher_epoch already appears in the checkpoint lineage")]
    ReusedPublisherEpoch,
    #[error("invalid spot-reference checkpoint state: {0}")]
    Checkpoint(String),
    #[error("embedded definition error: {0}")]
    Definition(String),
    #[error("invalid runtime configuration: {0}")]
    RuntimeConfig(String),
    #[error(transparent)]
    Validation(#[from] signals_core::ValidationError),
    #[error(transparent)]
    Decimal(#[from] signals_core::DecimalTextError),
    #[error("relevant source event does not use required version {expected}")]
    InputVersion { expected: &'static str },
    #[error("relevant source event does not use required instrument {expected}")]
    Instrument { expected: &'static str },
    #[error("required field {0} is missing or not text")]
    MissingField(String),
    #[error("invalid {field} timestamp: {detail}")]
    Timestamp { field: &'static str, detail: String },
    #[error("logical time regressed from {previous_ns}ns to {now_ns}ns")]
    ClockRegression { previous_ns: u64, now_ns: u64 },
    #[error("logical receipt {receipt_ns}ns exceeds admission time {now_ns}ns")]
    ReceiptAfterAdmission { receipt_ns: u64, now_ns: u64 },
    #[error("logical time cannot be represented as a wall timestamp")]
    TimestampRange,
    #[error("signal output sequence exhausted")]
    SequenceExhausted,
    #[error("signal counter exhausted")]
    CounterExhausted,
}
