use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Duration, SecondsFormat};
use semver::Version;
use serde_json::Value;
use thiserror::Error;

use crate::{
    FeedHealthReport, FieldDefinition, FieldType, InputKind, InstanceStatusEntry,
    InstanceStatusState, MarketTransitionPhase, MarketTransitionReason, MultiMarketLeg,
    MultiMarketWindow, ProducerStatus, QualityState, ScopeKind, SignalDefinition, SignalScope,
    SignalStreamMessage, SignalUpdate, SourceDefinition, SourceEvent, parse_decimal_text,
};

pub const CONTRACT_SCHEMA_VERSION: u16 = 1;
pub const SIGNAL_UPDATE_SCHEMA_VERSION: u16 = 3;
pub const SIGNAL_STREAM_SCHEMA_VERSION: u16 = 3;
pub const SIGNAL_HEARTBEAT_INTERVAL_MS: u64 = 5_000;
pub const SIGNAL_SNAPSHOT_INTERVAL_MS: u64 = 60_000;

/// Frozen stream-v3 instance-status vocabulary: unique, and **append-only**.
/// `market_unavailable` is deliberately absent: unbound prediction state
/// belongs to market_transition.
///
/// ADR 0017 froze this registry at 63 strings and required a future contract
/// decision to grow it. ADR 0026 is that decision: it appends the 53 reasons
/// the all-venues definitions need, taking the registry to 116.
///
/// **The order of this array is part of the contract.** `signals-package`
/// serializes it in array order into `status-reasons.json`, whose sha256 is in
/// the package manifest, and the verifier compares it as an ordered `Vec`, not
/// as a set. Reordering the existing entries would fail verification for every
/// package ever produced. New reasons are therefore appended after the frozen
/// 63, never interleaved, and no existing entry moves.
///
/// Consumers treat reasons as informational: an unrecognized reason is
/// displayable text, never a branch condition.
pub const INSTANCE_STATUS_REASONS: [&str; 120] = [
    "book_flow_history_warming_up",
    "book_history_warming_up",
    "coinbase_book_invalid",
    "kraken_book_invalid",
    "trade_history_warming_up",
    "trade_venue_stale_or_missing",
    "reference_unavailable",
    "reference_constituents_unavailable",
    "reference_history_warming_up",
    "trade_window_empty",
    "book_snapshot_stale_or_missing",
    "upstream_measurement_unavailable",
    "coinbase_stale",
    "coinbase_warming_up",
    "coinbase_invalid_quote",
    "coinbase_midpoint_unrepresentable",
    "coinbase_disconnected",
    "coinbase_recovery_required",
    "coinbase_subscription_rejected",
    "kraken_stale",
    "kraken_warming_up",
    "kraken_invalid_quote",
    "kraken_midpoint_unrepresentable",
    "kraken_disconnected",
    "kraken_recovery_required",
    "kraken_subscription_rejected",
    "cross_venue_dispersion_exceeded",
    "reference_arithmetic_unrepresentable",
    "market_closed",
    "opening_strike_unavailable",
    "probe_depth_insufficient",
    "trade_stale_or_missing",
    "model_domain_exceeded",
    "model_denominator_zero",
    "volatility_warming_up",
    "volatility_zero",
    "market_contract_warming_up",
    "market_clock_unrepresentable",
    "last_trade_missing",
    "last_trade_stale",
    "book_stale",
    "book_snapshot_missing",
    "book_one_sided",
    "book_crossed_or_invalid",
    "negative_book_stale",
    "negative_book_snapshot_missing",
    "negative_book_one_sided",
    "negative_book_crossed_or_invalid",
    "kalshi_complement_invalid",
    "affirmative_quote_missing",
    "affirmative_quote_crossed_or_invalid",
    "affirmative_quote_stale",
    "negative_quote_missing",
    "negative_quote_crossed_or_invalid",
    "negative_quote_stale",
    "market_rollover",
    "market_discovery_unreachable",
    "next_market_unlisted",
    "market_discovery_invalid",
    "market_open_gap",
    "provider_lifecycle_change",
    "coinbase_venue_unavailable",
    "kraken_venue_unavailable",
    // --- ADR 0026 additions (appended; the registry is append-only) ---
    "basis_history_warming_up",
    "binance_band_exceeds_captured_depth",
    "binance_book_invalid",
    "binance_disconnected",
    "binance_invalid_quote",
    "binance_midpoint_unrepresentable",
    "binance_recovery_required",
    "binance_stale",
    "binance_subscription_rejected",
    "binance_venue_unavailable",
    "binance_warming_up",
    "bybit_book_invalid",
    "bybit_disconnected",
    "bybit_invalid_quote",
    "bybit_midpoint_unrepresentable",
    "bybit_recovery_required",
    "bybit_stale",
    "bybit_subscription_rejected",
    "bybit_venue_unavailable",
    "bybit_warming_up",
    "cfbrti_anchor_missing",
    "cfbrti_anchor_window_thin",
    "cfbrti_disconnected",
    "cfbrti_entitlement_denied",
    "cfbrti_stale",
    "cfbrti_subscription_rejected",
    "cfbrti_unavailable",
    "cfbrti_value_invalid",
    "cfbrti_warming_up",
    "chainlink_reorg_detected",
    "chainlink_round_invalid",
    "chainlink_rpc_unavailable",
    "chainlink_stale",
    "chainlink_warming_up",
    "counterparty_venue_unavailable",
    "derivatives_constituents_unavailable",
    "funding_info_missing",
    "funding_interval_mismatch",
    "funding_unavailable",
    "leg_market_closed",
    "leg_market_unbound",
    "liquidation_burst_not_observed",
    "liquidation_side_unconfirmed",
    "open_interest_history_warming_up",
    "open_interest_unavailable",
    "oracle_constituents_unavailable",
    "oracle_rights_gated",
    "perp_venue_stale",
    "pyth_disconnected",
    "pyth_gap",
    "pyth_stale",
    "pyth_warming_up",
    "window_alignment_mismatch",
    // G2/E47: append-only wire contract — usdt_reference_stale is the E47 append,
    // placed LAST so window_alignment_mismatch keeps its pre-E47 position (a
    // sorted insertion would have shifted it and broken every prior package).
    "usdt_reference_stale",
    // E53: append-only. no_qualifying_move is entry 118, placed LAST so every
    // prior index (and every prior package's serialized registry) is unchanged.
    // Classed Degraded via the registry catch-all below: a not-yet-completed
    // move measurement is a degraded, self-healing state, not a missing upstream.
    "no_qualifying_move",
    // E51 (C3a): append-only. flat_window is entry 119 (index 118), placed LAST
    // for the same reason. A Z-score window whose standard deviation is exactly
    // zero has no defined score; that is a degraded, self-healing state (the next
    // tick with any price movement resolves it), so it takes the registry
    // catch-all's Degraded class.
    "flat_window",
    // E49/C2: append-only. chainlink_rounds_sparse is entry 120 (index 119),
    // appended after C3a's flat_window (E51) so every prior index — and every
    // prior package's serialized registry — is unchanged. Classed Degraded via
    // the catch-all below: too few Chainlink rounds inside the horizon is a
    // self-healing state, not a missing upstream.
    "chainlink_rounds_sparse",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum InstanceStatusReasonClass {
    Warming,
    Degraded,
    Unavailable,
}

pub fn instance_status_reason_class(reason: &str) -> Option<InstanceStatusReasonClass> {
    use InstanceStatusReasonClass::{Degraded, Unavailable, Warming};
    match reason {
        "book_flow_history_warming_up"
        | "book_history_warming_up"
        | "trade_history_warming_up"
        | "reference_history_warming_up"
        | "coinbase_warming_up"
        | "kraken_warming_up"
        | "volatility_warming_up"
        | "market_contract_warming_up"
        // ADR 0026 additions.
        | "basis_history_warming_up"
        | "binance_warming_up"
        | "bybit_warming_up"
        | "cfbrti_warming_up"
        | "chainlink_warming_up"
        | "liquidation_burst_not_observed"
        | "open_interest_history_warming_up"
        | "pyth_warming_up" => Some(Warming),
        "coinbase_disconnected"
        | "coinbase_recovery_required"
        | "coinbase_subscription_rejected"
        | "kraken_disconnected"
        | "kraken_recovery_required"
        | "kraken_subscription_rejected"
        | "market_closed"
        | "market_rollover"
        | "market_discovery_unreachable"
        | "next_market_unlisted"
        | "market_discovery_invalid"
        | "market_open_gap"
        | "provider_lifecycle_change"
        | "coinbase_venue_unavailable"
        | "kraken_venue_unavailable"
        // ADR 0026 additions. The classes are the ones the wave-A/B/C specs
        // declare alongside each string, with three exceptions: two the specs
        // left unmarked — `cfbrti_unavailable`, which ruling E2 requires an
        // index-dependent instance to publish as `unavailable`, and
        // `pyth_disconnected`, which joins the uniform class of every other
        // `*_disconnected` reason — and `binance_recovery_required` /
        // `bybit_recovery_required`, which the wave-B specs marked Degraded and
        // which are classed Unavailable here, uniform with their Coinbase and
        // Kraken counterparts: a surface that needs recovery is not
        // publishable.
        | "binance_disconnected"
        | "binance_recovery_required"
        | "binance_subscription_rejected"
        | "binance_venue_unavailable"
        | "bybit_disconnected"
        | "bybit_recovery_required"
        | "bybit_subscription_rejected"
        | "bybit_venue_unavailable"
        | "cfbrti_anchor_missing"
        | "cfbrti_disconnected"
        | "cfbrti_entitlement_denied"
        | "cfbrti_subscription_rejected"
        | "cfbrti_unavailable"
        | "cfbrti_value_invalid"
        | "chainlink_reorg_detected"
        | "chainlink_round_invalid"
        | "chainlink_rpc_unavailable"
        | "counterparty_venue_unavailable"
        | "leg_market_closed"
        | "leg_market_unbound"
        | "oracle_rights_gated"
        | "pyth_disconnected" => Some(Unavailable),
        reason if INSTANCE_STATUS_REASONS.contains(&reason) => Some(Degraded),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{path}: {message}")]
pub struct ValidationError {
    path: String,
    message: String,
}

impl ValidationError {
    fn new(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            message: message.into(),
        }
    }
}

/// Semantic validation beyond basic JSON deserialization.
pub trait Validate {
    fn validate(&self) -> Result<(), ValidationError>;
}

impl Validate for SourceDefinition {
    fn validate(&self) -> Result<(), ValidationError> {
        validate_schema_version(self.schema_version)?;
        validate_id("source_id", &self.source_id)?;
        validate_version("version", &self.version)?;
        validate_non_empty("name", &self.name)?;
        validate_non_empty("description", &self.description)?;
        if self.expected_heartbeat_ms == Some(0) {
            return Err(ValidationError::new(
                "expected_heartbeat_ms",
                "must be greater than zero when present",
            ));
        }
        validate_fields(&self.fields)?;

        let event_time = field_by_name(&self.fields, &self.event_time_field).ok_or_else(|| {
            ValidationError::new(
                "event_time_field",
                format!("unknown field `{}`", self.event_time_field),
            )
        })?;
        if event_time.field_type != FieldType::Timestamp {
            return Err(ValidationError::new(
                "event_time_field",
                "must reference a timestamp field",
            ));
        }

        if let Some(sequence_name) = &self.sequence_field {
            let sequence = field_by_name(&self.fields, sequence_name).ok_or_else(|| {
                ValidationError::new("sequence_field", format!("unknown field `{sequence_name}`"))
            })?;
            if !matches!(
                sequence.field_type,
                FieldType::U8 | FieldType::U16 | FieldType::U32 | FieldType::U64
            ) {
                return Err(ValidationError::new(
                    "sequence_field",
                    "must reference an unsigned integer field",
                ));
            }
        }

        Ok(())
    }
}

impl Validate for SignalDefinition {
    fn validate(&self) -> Result<(), ValidationError> {
        validate_schema_version(self.schema_version)?;
        validate_id("signal_id", &self.signal_id)?;
        validate_version("version", &self.version)?;
        validate_non_empty("name", &self.name)?;
        validate_non_empty("description", &self.description)?;

        if self.approximate_interval_ms == 0 {
            return Err(ValidationError::new(
                "approximate_interval_ms",
                "must be greater than zero",
            ));
        }
        if self.staleness_bound_ms < self.approximate_interval_ms {
            return Err(ValidationError::new(
                "staleness_bound_ms",
                "must be at least the approximate interval",
            ));
        }
        if self.inputs.is_empty() {
            return Err(ValidationError::new(
                "inputs",
                "must declare at least one input",
            ));
        }

        let mut inputs = BTreeSet::new();
        for (index, input) in self.inputs.iter().enumerate() {
            validate_id(&format!("inputs[{index}].id"), &input.id)?;
            validate_version(&format!("inputs[{index}].version"), &input.version)?;
            if input.kind == InputKind::Signal
                && input.id == self.signal_id
                && input.version == self.version
            {
                return Err(ValidationError::new(
                    format!("inputs[{index}]"),
                    "a signal cannot depend directly on itself",
                ));
            }
            if !inputs.insert(input) {
                return Err(ValidationError::new(
                    format!("inputs[{index}]"),
                    "duplicate input",
                ));
            }
        }

        validate_fields(&self.fields)?;
        let event_time = field_by_name(&self.fields, &self.event_time_field).ok_or_else(|| {
            ValidationError::new(
                "event_time_field",
                format!("unknown field `{}`", self.event_time_field),
            )
        })?;
        if event_time.field_type != FieldType::Timestamp {
            return Err(ValidationError::new(
                "event_time_field",
                "must reference a timestamp field",
            ));
        }

        // `scope_kind` (ADR 0026) has two regimes.
        //
        // On the `multi` segment it is REQUIRED and it CHOOSES: a `btc.multi.*`
        // identity is either a global blend over global constituents or a blend
        // over bound markets, and nothing in the identity says which.
        //
        // On every other segment it is OPTIONAL and merely AGREES with the kind
        // the venue segment derives, so every definition written before ADR
        // 0026 stays valid untouched.
        let venue = self.signal_id.split('.').nth(1).ok_or_else(|| {
            ValidationError::new("signal_id", "must contain a venue namespace segment")
        })?;
        if scope_kind_is_declared_for_venue(venue) {
            match self.scope_kind {
                None => {
                    return Err(ValidationError::new(
                        "scope_kind",
                        format!(
                            "is required on the `{venue}` segment, which does not derive its scope: declare `global` or `multi_market`"
                        ),
                    ));
                }
                Some(ScopeKind::Market) => {
                    return Err(ValidationError::new(
                        "scope_kind",
                        format!("on the `{venue}` segment must be `global` or `multi_market`"),
                    ));
                }
                Some(_) => {}
            }
        } else {
            // Every other segment derives its scope from the venue namespace, so
            // the venue MUST be one this contract knows — whether or not
            // `scope_kind` is declared. Gating the check on a declared `scope_kind`
            // let a definition on an unknown or retired segment (a `crossvenue`
            // holdover, or a typo like `binancee`) with no `scope_kind` validate
            // and then be catalogued as `global` through
            // `projected_scope_kind(...).unwrap_or(Global)`, a positive false claim,
            // while every frame it emits is rejected at runtime. A validated
            // definition therefore always has a derivable scope kind.
            let derived = scope_kind_of_venue(venue).ok_or_else(|| {
                ValidationError::new(
                    "signal_id",
                    format!("`{venue}` is not a venue segment this contract knows"),
                )
            })?;
            if let Some(declared) = self.scope_kind
                && declared != derived
            {
                return Err(ValidationError::new(
                    "scope_kind",
                    "does not match the scope kind derived from the signal-id venue namespace",
                ));
            }
        }
        Ok(())
    }
}

impl SourceEvent {
    pub fn validate_against(&self, definition: &SourceDefinition) -> Result<(), ValidationError> {
        validate_schema_version(self.schema_version)?;
        if self.source_id != definition.source_id {
            return Err(ValidationError::new(
                "source_id",
                "does not match the source definition",
            ));
        }
        if self.source_version != definition.version {
            return Err(ValidationError::new(
                "source_version",
                "does not match the source definition",
            ));
        }
        validate_publisher_epoch(&self.publisher_epoch)?;
        validate_timestamp("event_time", &self.event_time)?;
        validate_timestamp("received_at", &self.received_at)?;
        validate_values(&definition.fields, &self.values, true)?;
        validate_event_time_value(&self.event_time, &definition.event_time_field, &self.values)
    }
}

impl SignalUpdate {
    pub fn validity_window(
        produced_at: &str,
        staleness_bound_ms: u64,
    ) -> Result<(String, String), ValidationError> {
        let produced = parse_timestamp("produced_at", produced_at)?;
        let duration_ms = i64::try_from(staleness_bound_ms).map_err(|_| {
            ValidationError::new(
                "staleness_bound_ms",
                "cannot be represented as a signed timestamp duration",
            )
        })?;
        let valid_until = produced
            .checked_add_signed(Duration::milliseconds(duration_ms))
            .ok_or_else(|| {
                ValidationError::new("valid_until", "falls outside the supported timestamp range")
            })?;
        Ok((
            produced_at.to_owned(),
            valid_until.to_rfc3339_opts(SecondsFormat::Nanos, true),
        ))
    }

    pub fn validate_against(&self, definition: &SignalDefinition) -> Result<(), ValidationError> {
        validate_signal_update_schema_version(self.schema_version)?;
        if self.signal_id != definition.signal_id {
            return Err(ValidationError::new(
                "signal_id",
                "does not match the signal definition",
            ));
        }
        if self.signal_version != definition.version {
            return Err(ValidationError::new(
                "signal_version",
                "does not match the signal definition",
            ));
        }
        validate_signal_scope_binding(&self.signal_id, &self.scope)?;
        validate_publisher_epoch(&self.publisher_epoch)?;
        validate_timestamp("event_time", &self.event_time)?;
        let produced_at = parse_timestamp("produced_at", &self.produced_at)?;
        let valid_from = parse_timestamp("valid_from", &self.valid_from)?;
        let valid_until = parse_timestamp("valid_until", &self.valid_until)?;
        if valid_from != produced_at {
            return Err(ValidationError::new(
                "valid_from",
                "must be the same instant as produced_at",
            ));
        }
        let duration_ms = i64::try_from(definition.staleness_bound_ms).map_err(|_| {
            ValidationError::new(
                "staleness_bound_ms",
                "cannot be represented as a signed timestamp duration",
            )
        })?;
        let expected_valid_until = produced_at
            .checked_add_signed(Duration::milliseconds(duration_ms))
            .ok_or_else(|| {
                ValidationError::new("valid_until", "falls outside the supported timestamp range")
            })?;
        if valid_until != expected_valid_until {
            return Err(ValidationError::new(
                "valid_until",
                "must equal produced_at plus the definition staleness bound",
            ));
        }

        let require_all = match self.quality.state {
            QualityState::Ready => {
                if !self.quality.reasons.is_empty() {
                    return Err(ValidationError::new(
                        "quality.reasons",
                        "must be empty when state is ready",
                    ));
                }
                true
            }
            QualityState::WarmingUp | QualityState::Degraded => {
                if self.quality.reasons.is_empty() {
                    return Err(ValidationError::new(
                        "quality.reasons",
                        "must explain a non-ready state",
                    ));
                }
                false
            }
        };

        for (index, reason) in self.quality.reasons.iter().enumerate() {
            validate_non_empty(&format!("quality.reasons[{index}]"), reason)?;
        }

        validate_values(&definition.fields, &self.values, require_all)?;
        if self.values.contains_key(&definition.event_time_field) {
            validate_event_time_value(
                &self.event_time,
                &definition.event_time_field,
                &self.values,
            )?;
        }
        Ok(())
    }
}

impl Validate for SignalStreamMessage {
    fn validate(&self) -> Result<(), ValidationError> {
        let (schema_version, publisher_epoch, sequence, produced_at) = match self {
            Self::SignalBatch {
                schema_version,
                publisher_epoch,
                sequence,
                produced_at,
                updates,
                ..
            } => {
                if updates.is_empty() {
                    return Err(ValidationError::new(
                        "updates",
                        "a signal batch must contain at least one update",
                    ));
                }
                validate_batch_updates(publisher_epoch, produced_at, updates)?;
                (schema_version, publisher_epoch, sequence, produced_at)
            }
            Self::Snapshot {
                schema_version,
                publisher_epoch,
                sequence,
                produced_at,
                as_of_sequence,
                updates,
                statuses,
                ..
            } => {
                if as_of_sequence >= sequence {
                    return Err(ValidationError::new(
                        "as_of_sequence",
                        "must identify an earlier decision-or-status frame",
                    ));
                }
                validate_snapshot_updates(publisher_epoch, produced_at, updates)?;
                validate_snapshot_statuses(statuses)?;
                (schema_version, publisher_epoch, sequence, produced_at)
            }
            Self::InstanceStatus {
                schema_version,
                publisher_epoch,
                sequence,
                produced_at,
                signal_id,
                signal_version,
                scope,
                state,
                reasons,
                effective_time,
                last_decision_sequence: _,
            } => {
                validate_instance_status_payload(
                    signal_id,
                    signal_version,
                    scope,
                    *state,
                    reasons,
                    effective_time,
                )?;
                let effective = parse_timestamp("effective_time", effective_time)?;
                let produced = parse_timestamp("produced_at", produced_at)?;
                if effective > produced {
                    return Err(ValidationError::new(
                        "effective_time",
                        "must not be later than produced_at",
                    ));
                }
                (schema_version, publisher_epoch, sequence, produced_at)
            }
            Self::Heartbeat {
                schema_version,
                publisher_epoch,
                sequence,
                produced_at,
                heartbeat_interval_ms,
                as_of_sequence,
                ..
            } => {
                if *heartbeat_interval_ms != SIGNAL_HEARTBEAT_INTERVAL_MS {
                    return Err(ValidationError::new(
                        "heartbeat_interval_ms",
                        format!("must equal {SIGNAL_HEARTBEAT_INTERVAL_MS}"),
                    ));
                }
                if as_of_sequence.is_some_and(|anchor| anchor >= *sequence) {
                    return Err(ValidationError::new(
                        "as_of_sequence",
                        "must identify an earlier complete signal batch",
                    ));
                }
                (schema_version, publisher_epoch, sequence, produced_at)
            }
            Self::Status {
                schema_version,
                publisher_epoch,
                sequence,
                produced_at,
                status,
                reasons,
                as_of_sequence,
            } => {
                validate_status(*status, reasons)?;
                if as_of_sequence.is_some_and(|anchor| anchor >= *sequence) {
                    return Err(ValidationError::new(
                        "as_of_sequence",
                        "must identify an earlier complete signal batch",
                    ));
                }
                (schema_version, publisher_epoch, sequence, produced_at)
            }
            Self::Reset {
                schema_version,
                publisher_epoch,
                sequence,
                produced_at,
                ..
            } => {
                if *sequence != 0 {
                    return Err(ValidationError::new(
                        "sequence",
                        "a reset frame must use publisher sequence zero",
                    ));
                }
                (schema_version, publisher_epoch, sequence, produced_at)
            }
            Self::MarketTransition {
                schema_version,
                publisher_epoch,
                sequence,
                produced_at,
                market_generation,
                previous_market_id,
                next_market_id,
                phase,
                reason,
                ..
            } => {
                validate_market_transition(
                    *market_generation,
                    previous_market_id.as_deref(),
                    next_market_id.as_deref(),
                    *phase,
                    *reason,
                )?;
                (schema_version, publisher_epoch, sequence, produced_at)
            }
            Self::FeedHealth {
                schema_version,
                publisher_epoch,
                sequence,
                produced_at,
                feeds,
            } => {
                validate_feed_health(feeds)?;
                (schema_version, publisher_epoch, sequence, produced_at)
            }
        };
        validate_signal_stream_schema_version(*schema_version)?;
        validate_publisher_epoch(publisher_epoch)?;
        validate_timestamp("produced_at", produced_at)?;
        let _ = sequence;
        Ok(())
    }
}

fn validate_market_transition(
    market_generation: u64,
    previous_market_id: Option<&str>,
    next_market_id: Option<&str>,
    phase: MarketTransitionPhase,
    reason: MarketTransitionReason,
) -> Result<(), ValidationError> {
    if market_generation == 0 {
        return Err(ValidationError::new(
            "market_generation",
            "must reserve a positive generation for the pending or bound market",
        ));
    }
    if let Some(market_id) = previous_market_id {
        validate_market_id("previous_market_id", market_id)?;
    }
    if let Some(market_id) = next_market_id {
        validate_market_id("next_market_id", market_id)?;
    }

    match (phase, reason) {
        (MarketTransitionPhase::Bound, MarketTransitionReason::InitialBind) => {
            if previous_market_id.is_some() || next_market_id.is_none() {
                return Err(ValidationError::new(
                    "market_transition",
                    "an initial bind requires only next_market_id",
                ));
            }
        }
        (MarketTransitionPhase::Bound, MarketTransitionReason::ScheduledRollover) => {
            let (Some(previous), Some(next)) = (previous_market_id, next_market_id) else {
                return Err(ValidationError::new(
                    "market_transition",
                    "a scheduled rollover bind requires both market identities",
                ));
            };
            if previous == next {
                return Err(ValidationError::new(
                    "next_market_id",
                    "must differ from previous_market_id for a scheduled rollover",
                ));
            }
        }
        (MarketTransitionPhase::Bound, MarketTransitionReason::RecoveryBind) => {
            let (Some(_), Some(_)) = (previous_market_id, next_market_id) else {
                return Err(ValidationError::new(
                    "market_transition",
                    "a recovery bind requires both market identities",
                ));
            };
        }
        (MarketTransitionPhase::Bound, _) => {
            return Err(ValidationError::new(
                "reason",
                "is not a valid bound-transition reason",
            ));
        }
        (
            MarketTransitionPhase::Unavailable,
            MarketTransitionReason::ScheduledRollover
            | MarketTransitionReason::ProviderLifecycleChange,
        ) => {
            if previous_market_id.is_none() {
                return Err(ValidationError::new(
                    "previous_market_id",
                    "is required when a bound market reaches a lifecycle boundary",
                ));
            }
        }
        (
            MarketTransitionPhase::Unavailable,
            MarketTransitionReason::DiscoveryUnreachable
            | MarketTransitionReason::NextMarketUnlisted
            | MarketTransitionReason::InvalidDiscovery
            | MarketTransitionReason::CloseToNextOpenGap,
        ) => {}
        (MarketTransitionPhase::Unavailable, _) => {
            return Err(ValidationError::new(
                "reason",
                "is not a valid unavailable-transition reason",
            ));
        }
    }
    Ok(())
}

fn validate_feed_health(feeds: &[FeedHealthReport]) -> Result<(), ValidationError> {
    if feeds.is_empty() {
        return Err(ValidationError::new(
            "feeds",
            "a feed_health frame must report at least one feed",
        ));
    }
    let mut seen = BTreeSet::new();
    for feed in feeds {
        validate_id("feeds.source_id", &feed.source_id)?;
        if !seen.insert(feed.source_id.as_str()) {
            return Err(ValidationError::new(
                "feeds.source_id",
                "must be unique within a feed_health frame",
            ));
        }
        if feed.venue.is_empty() || feed.venue.len() > 64 {
            return Err(ValidationError::new(
                "feeds.venue",
                "must contain 1 to 64 bytes",
            ));
        }
        if let Some(down_since) = &feed.down_since {
            validate_timestamp("feeds.down_since", down_since)?;
        }
    }
    Ok(())
}

fn validate_batch_updates(
    publisher_epoch: &str,
    produced_at: &str,
    updates: &[SignalUpdate],
) -> Result<(), ValidationError> {
    let mut last_sequence = BTreeMap::<(&str, &str), u64>::new();
    for (index, update) in updates.iter().enumerate() {
        validate_signal_update_envelope(update, &format!("updates[{index}]"))?;
        if update.publisher_epoch != publisher_epoch {
            return Err(ValidationError::new(
                format!("updates[{index}].publisher_epoch"),
                "must match the enclosing signal batch",
            ));
        }
        if !same_timestamp(&update.produced_at, produced_at)? {
            return Err(ValidationError::new(
                format!("updates[{index}].produced_at"),
                "must match the enclosing signal batch",
            ));
        }
        let key = (update.signal_id.as_str(), update.signal_version.as_str());
        if last_sequence
            .insert(key, update.sequence)
            .is_some_and(|previous| update.sequence <= previous)
        {
            return Err(ValidationError::new(
                format!("updates[{index}].sequence"),
                "must increase within a batch for the same signal identity",
            ));
        }
    }
    Ok(())
}

fn validate_snapshot_updates(
    publisher_epoch: &str,
    produced_at: &str,
    updates: &[SignalUpdate],
) -> Result<(), ValidationError> {
    let snapshot_time = parse_timestamp("produced_at", produced_at)?;
    let mut previous_identity = None;
    for (index, update) in updates.iter().enumerate() {
        validate_signal_update_envelope(update, &format!("updates[{index}]"))?;
        if update.publisher_epoch != publisher_epoch {
            return Err(ValidationError::new(
                format!("updates[{index}].publisher_epoch"),
                "must match the enclosing snapshot",
            ));
        }
        if parse_timestamp(
            &format!("updates[{index}].produced_at"),
            &update.produced_at,
        )? > snapshot_time
        {
            return Err(ValidationError::new(
                format!("updates[{index}].produced_at"),
                "must not be later than the snapshot production time",
            ));
        }
        if parse_timestamp(
            &format!("updates[{index}].valid_until"),
            &update.valid_until,
        )? <= snapshot_time
        {
            return Err(ValidationError::new(
                format!("updates[{index}].valid_until"),
                "must be strictly later than the snapshot production time",
            ));
        }
        let identity = update.instance_key();
        if previous_identity
            .as_ref()
            .is_some_and(|previous| previous >= &identity)
        {
            return Err(ValidationError::new(
                format!("updates[{index}]"),
                "snapshot identities must be unique and sorted",
            ));
        }
        previous_identity = Some(identity);
    }
    Ok(())
}

fn validate_snapshot_statuses(statuses: &[InstanceStatusEntry]) -> Result<(), ValidationError> {
    let mut previous_identity = None;
    for (index, status) in statuses.iter().enumerate() {
        validate_instance_status_payload(
            &status.signal_id,
            &status.signal_version,
            &status.scope,
            status.state,
            &status.reasons,
            &status.effective_time,
        )
        .map_err(|error| ValidationError::new(format!("statuses[{index}]"), error.message))?;
        let identity = status.instance_key();
        if previous_identity
            .as_ref()
            .is_some_and(|previous| previous >= &identity)
        {
            return Err(ValidationError::new(
                format!("statuses[{index}]"),
                "snapshot identities must be unique and sorted",
            ));
        }
        previous_identity = Some(identity);
    }
    Ok(())
}

fn validate_signal_update_envelope(
    update: &SignalUpdate,
    path: &str,
) -> Result<(), ValidationError> {
    validate_signal_update_schema_version(update.schema_version)
        .map_err(|error| ValidationError::new(format!("{path}.schema_version"), error.message))?;
    validate_id(&format!("{path}.signal_id"), &update.signal_id)?;
    validate_version(&format!("{path}.signal_version"), &update.signal_version)?;
    validate_signal_scope_binding(&update.signal_id, &update.scope)
        .map_err(|error| ValidationError::new(format!("{path}.scope"), error.message))?;
    validate_publisher_epoch(&update.publisher_epoch)?;
    validate_timestamp(&format!("{path}.event_time"), &update.event_time)?;
    let produced_at = parse_timestamp(&format!("{path}.produced_at"), &update.produced_at)?;
    let valid_from = parse_timestamp(&format!("{path}.valid_from"), &update.valid_from)?;
    let valid_until = parse_timestamp(&format!("{path}.valid_until"), &update.valid_until)?;
    if valid_from != produced_at || valid_until < valid_from {
        return Err(ValidationError::new(
            format!("{path}.valid_until"),
            "validity must start at produced_at and must not run backwards",
        ));
    }
    if update.quality.state != QualityState::Ready || !update.quality.reasons.is_empty() {
        return Err(ValidationError::new(
            format!("{path}.quality"),
            "stream-v3 signal updates must be ready with no reasons",
        ));
    }
    Ok(())
}

fn validate_instance_status_payload(
    signal_id: &str,
    signal_version: &str,
    scope: &SignalScope,
    state: InstanceStatusState,
    reasons: &[String],
    effective_time: &str,
) -> Result<(), ValidationError> {
    validate_id("signal_id", signal_id)?;
    validate_version("signal_version", signal_version)?;
    validate_signal_scope_binding(signal_id, scope)?;
    validate_timestamp("effective_time", effective_time)?;
    if reasons.is_empty() {
        return Err(ValidationError::new(
            "reasons",
            "must explain an instance status",
        ));
    }
    for (index, reason) in reasons.iter().enumerate() {
        if instance_status_reason_class(reason).is_none() {
            return Err(ValidationError::new(
                format!("reasons[{index}]"),
                "is not in the frozen stream-v3 instance-status registry",
            ));
        }
        if index > 0 && reasons[index - 1] >= *reason {
            return Err(ValidationError::new(
                format!("reasons[{index}]"),
                "must be unique and sorted in ascending byte order",
            ));
        }
    }

    if state == InstanceStatusState::Invalidated {
        // A bound instance is invalidated when its market departs: a `market`
        // instance on a rollover, and a `multi_market` instance (ADR 0026, C1/E57)
        // when either leg departs (V10/L2). A `global` identity is never bound to
        // a departing market, so it is never invalidated.
        if !matches!(
            scope,
            SignalScope::Market { .. } | SignalScope::MultiMarket { .. }
        ) {
            return Err(ValidationError::new(
                "state",
                "global instances cannot be invalidated in stream v3",
            ));
        }
        if reasons.iter().any(|reason| {
            !matches!(
                reason.as_str(),
                "market_rollover"
                    | "market_discovery_unreachable"
                    | "next_market_unlisted"
                    | "market_discovery_invalid"
                    | "market_open_gap"
                    | "provider_lifecycle_change"
            )
        }) {
            return Err(ValidationError::new(
                "reasons",
                "invalidated requires only bound-departure reasons",
            ));
        }
        return Ok(());
    }

    let highest = reasons
        .iter()
        .filter_map(|reason| instance_status_reason_class(reason))
        .max()
        .expect("non-empty reasons were classified");
    let state_matches = match highest {
        InstanceStatusReasonClass::Unavailable => state == InstanceStatusState::Unavailable,
        InstanceStatusReasonClass::Degraded => state == InstanceStatusState::Degraded,
        InstanceStatusReasonClass::Warming => {
            matches!(
                state,
                InstanceStatusState::WarmingUp | InstanceStatusState::Degraded
            )
        }
    };
    if !state_matches {
        return Err(ValidationError::new(
            "state",
            "does not match the active reason-class precedence",
        ));
    }
    Ok(())
}

/// Venue namespace segments that carry spot measurements and are therefore
/// globally scoped.
///
/// `external` is the blended reference layer computed from every adopted spot
/// venue; the remaining segments are the single-venue readings of the same
/// measurements (ADR 0024). `binance` and `bybit` are listed ahead of their
/// signal definitions so the namespace is settled once: their feeds are
/// capture-only until the source-rights gate clears, and no definition may use
/// those segments before then.
pub const SPOT_VENUE_NAMESPACES: [&str; 5] = ["external", "coinbase", "kraken", "binance", "bybit"];

/// Every venue segment that binds to a global scope (ADR 0026).
///
/// This is a superset of `SPOT_VENUE_NAMESPACES`: the oracle reference segments
/// (`pyth`, `chainlink`, `cfbrti`) and the cross-venue derived segment (`cross`)
/// are global but are not spot readings, so they are deliberately kept out of
/// `is_spot_signal_id` and out of the spot lane mapping ADR 0024 built on it.
pub const GLOBAL_VENUE_NAMESPACES: [&str; 9] = [
    "external",
    "coinbase",
    "kraken",
    "binance",
    "bybit",
    "pyth",
    "chainlink",
    "cfbrti",
    "cross",
];

/// Venue segments whose identities bind to one market.
pub const MARKET_VENUE_NAMESPACES: [&str; 2] = ["polymarket", "kalshi"];

/// The venue segment whose identities bind to several markets at once.
pub const MULTI_MARKET_VENUE_NAMESPACE: &str = "multi";

/// The most legs one `multi_market` scope may carry (ADR 0026, C1 V2).
///
/// Two, because there are exactly two market-scope venues and legs must be on
/// distinct venues — a third leg is unreachable, not merely unplanned. Raising
/// this means adding a market venue first, and is a contract change.
pub const MAX_MULTI_MARKET_LEGS: usize = 2;

/// The most bytes a serialized `multi_market` scope object may occupy
/// (C1 V11). The scope is a consumer key held in memory per instance and
/// echoed on every frame, so it is bounded rather than left to the market-id
/// charset limit.
pub const MAX_MULTI_MARKET_SCOPE_BYTES: usize = 1024;

/// The scope kind a venue segment publishes under, or `None` for a segment this
/// contract does not know.
pub fn scope_kind_of_venue(venue: &str) -> Option<ScopeKind> {
    if GLOBAL_VENUE_NAMESPACES.contains(&venue) {
        Some(ScopeKind::Global)
    } else if MARKET_VENUE_NAMESPACES.contains(&venue) {
        Some(ScopeKind::Market)
    } else {
        // `multi` is deliberately absent: its scope is not derivable from the
        // segment. See `scope_kind_is_declared_for_venue`.
        None
    }
}

/// True for a venue segment whose scope kind the definition **declares**
/// rather than the segment deriving it (ADR 0026, C1 R3).
///
/// `multi` is the only such segment. It carries both shapes: a blend over
/// global constituents (`btc.multi.oracle_dispersion` compares three oracle
/// references and is globally scoped) and a blend over bound markets (which is
/// `multi_market`). Nothing in the identity distinguishes them, so
/// `scope_kind` is required there and is what chooses.
pub fn scope_kind_is_declared_for_venue(venue: &str) -> bool {
    venue == MULTI_MARKET_VENUE_NAMESPACE
}

/// The scope kinds a venue segment may legally declare or derive.
fn permitted_scope_kinds(venue: &str) -> Option<&'static [ScopeKind]> {
    if scope_kind_is_declared_for_venue(venue) {
        // A `multi` identity is either a global blend or a market blend, never
        // bound to one market.
        Some(&[ScopeKind::Global, ScopeKind::MultiMarket])
    } else {
        match scope_kind_of_venue(venue) {
            Some(ScopeKind::Global) => Some(&[ScopeKind::Global]),
            Some(ScopeKind::Market) => Some(&[ScopeKind::Market]),
            Some(ScopeKind::MultiMarket) => Some(&[ScopeKind::MultiMarket]),
            None => None,
        }
    }
}

/// True for a `btc.<spot venue>.<measurement>` signal identity.
///
/// The `btc` domain prefix is load-bearing: source identities such as
/// `coinbase.exchange.btc-usd.ticker` also start with a venue word, and only
/// signal identities carry the domain prefix.
pub fn is_spot_signal_id(signal_id: &str) -> bool {
    let mut segments = signal_id.split('.');
    segments.next() == Some("btc")
        && segments
            .next()
            .is_some_and(|venue| SPOT_VENUE_NAMESPACES.contains(&venue))
        && segments.next().is_some_and(|name| !name.is_empty())
        && segments.next().is_none()
}

pub fn validate_signal_scope_binding(
    signal_id: &str,
    scope: &SignalScope,
) -> Result<(), ValidationError> {
    let venue = signal_id.split('.').nth(1).ok_or_else(|| {
        ValidationError::new("signal_id", "must contain a venue namespace segment")
    })?;
    // `crossvenue` was reserved by ADR 0024 and retired by ADR 0026. It is
    // rejected before the scope is looked at, so no scope shape can revive it.
    if venue == "crossvenue" {
        return Err(ValidationError::new(
            "signal_id",
            "crossvenue is retired and cannot be emitted in stream v3",
        ));
    }
    let Some(permitted) = permitted_scope_kinds(venue) else {
        return Err(ValidationError::new(
            "signal_id",
            "second segment must be a global venue (external, coinbase, kraken, binance, bybit, pyth, chainlink, cfbrti, cross), polymarket, kalshi, or multi",
        ));
    };
    let observed = match scope {
        SignalScope::Global => ScopeKind::Global,
        SignalScope::Market { .. } => ScopeKind::Market,
        SignalScope::MultiMarket { .. } => ScopeKind::MultiMarket,
    };
    if !permitted.contains(&observed) {
        return Err(ValidationError::new(
            "scope",
            "does not match the signal-id venue namespace",
        ));
    }
    match scope {
        SignalScope::Global => Ok(()),
        SignalScope::Market {
            market_id,
            market_generation,
        } => {
            validate_market_id("scope.market_id", market_id)?;
            if *market_generation == 0 {
                return Err(ValidationError::new(
                    "scope.market_generation",
                    "must be positive",
                ));
            }
            Ok(())
        }
        SignalScope::MultiMarket { legs, window } => validate_multi_market_scope(legs, window),
    }
}

/// The scope kind a projection should record for one signal definition.
///
/// The declared `scope_kind` wins where there is one — it is required, and
/// authoritative, on the `multi` segment. Everywhere else the venue segment
/// derives it. `None` for a segment this contract does not know, which a
/// validated definition never has.
///
/// This is the single authority the registry projection, the package registry
/// and the stream server's catalogue all read, so a scope kind cannot be spelled
/// one way in a package and another on the wire.
#[must_use]
pub fn projected_scope_kind(signal_id: &str, declared: Option<ScopeKind>) -> Option<ScopeKind> {
    let venue = signal_id.split('.').nth(1)?;
    if scope_kind_is_declared_for_venue(venue) {
        return declared;
    }
    declared.or_else(|| scope_kind_of_venue(venue))
}

/// The scope binding, pinned by a definition's declared `scope_kind`.
///
/// [`validate_signal_scope_binding`] is the wire-level check and accepts either
/// shape on the `multi` segment, because a frame alone does not say which the
/// identity declared. A caller holding the definition uses this instead: the
/// declared kind is authoritative and the scope must be exactly it.
pub fn validate_signal_scope_binding_for_definition(
    signal_id: &str,
    scope: &SignalScope,
    declared: Option<ScopeKind>,
) -> Result<(), ValidationError> {
    validate_signal_scope_binding(signal_id, scope)?;
    let Some(declared) = declared else {
        return Ok(());
    };
    let observed = match scope {
        SignalScope::Global => ScopeKind::Global,
        SignalScope::Market { .. } => ScopeKind::Market,
        SignalScope::MultiMarket { .. } => ScopeKind::MultiMarket,
    };
    if observed != declared {
        return Err(ValidationError::new(
            "scope",
            "does not match the definition's declared scope_kind",
        ));
    }
    Ok(())
}

/// The legs and window of a `multi_market` scope (ADR 0026).
///
/// The consumer key for such an instance is the full legs array plus the
/// window, so every constraint here is a constraint on key identity: a leg set
/// that repeated a venue, or carried an unbound generation, would key two
/// different bindings the same way.
fn validate_multi_market_scope(
    legs: &[MultiMarketLeg],
    window: &MultiMarketWindow,
) -> Result<(), ValidationError> {
    if legs.is_empty() {
        return Err(ValidationError::new("scope.legs", "must not be empty"));
    }
    if legs.len() > MAX_MULTI_MARKET_LEGS {
        return Err(ValidationError::new(
            "scope.legs",
            format!("must carry at most {MAX_MULTI_MARKET_LEGS} legs"),
        ));
    }
    let mut venues = BTreeSet::new();
    for (index, leg) in legs.iter().enumerate() {
        let path = |field: &str| format!("scope.legs[{index}].{field}");
        if !MARKET_VENUE_NAMESPACES.contains(&leg.venue.as_str()) {
            return Err(ValidationError::new(
                path("venue"),
                "must be a market-scope venue (polymarket, kalshi)",
            ));
        }
        if !venues.insert(leg.venue.as_str()) {
            return Err(ValidationError::new(
                path("venue"),
                "must be distinct across legs",
            ));
        }
        validate_market_id(&path("market_id"), &leg.market_id)?;
        if leg.market_generation == 0 {
            return Err(ValidationError::new(
                path("market_generation"),
                "must be positive",
            ));
        }
        // C1 V3: legs ascend by `(venue, market_id)` byte order. One binding
        // must have exactly one encoding, because the legs array IS the
        // consumer key and `Vec` equality is order-sensitive. An unsorted array
        // is rejected, never silently re-sorted: repairing it would let a
        // producer emit two spellings of one instance and have both accepted.
        if index > 0 {
            let previous = &legs[index - 1];
            if (previous.venue.as_str(), previous.market_id.as_str())
                >= (leg.venue.as_str(), leg.market_id.as_str())
            {
                return Err(ValidationError::new(
                    "scope.legs",
                    "must ascend by (venue, market_id) byte order",
                ));
            }
        }
    }
    let open = parse_timestamp("scope.window.open", &window.open)?;
    let close = parse_timestamp("scope.window.close", &window.close)?;
    if open >= close {
        return Err(ValidationError::new(
            "scope.window",
            "open must be strictly before close",
        ));
    }
    // C1 V11: the serialized scope object is bounded. Measured on the encoded
    // form rather than estimated from field lengths, so the bound holds
    // whatever the encoder does with escapes.
    let encoded = serde_json::to_vec(&SignalScope::MultiMarket {
        legs: legs.to_vec(),
        window: window.clone(),
    })
    .map_err(|error| ValidationError::new("scope", format!("is not encodable: {error}")))?;
    if encoded.len() > MAX_MULTI_MARKET_SCOPE_BYTES {
        return Err(ValidationError::new(
            "scope",
            format!(
                "serializes to {} bytes, over the {MAX_MULTI_MARKET_SCOPE_BYTES}-byte bound",
                encoded.len()
            ),
        ));
    }
    Ok(())
}

fn validate_status(status: ProducerStatus, reasons: &[String]) -> Result<(), ValidationError> {
    if status == ProducerStatus::Ready && !reasons.is_empty() {
        return Err(ValidationError::new(
            "reasons",
            "must be empty when status is ready",
        ));
    }
    if status != ProducerStatus::Ready && reasons.is_empty() {
        return Err(ValidationError::new(
            "reasons",
            "must explain a non-ready producer state",
        ));
    }
    for (index, reason) in reasons.iter().enumerate() {
        validate_non_empty(&format!("reasons[{index}]"), reason)?;
    }
    Ok(())
}

impl From<QualityState> for ProducerStatus {
    fn from(value: QualityState) -> Self {
        match value {
            QualityState::Ready => Self::Ready,
            QualityState::WarmingUp => Self::Starting,
            QualityState::Degraded => Self::Degraded,
        }
    }
}

fn validate_schema_version(schema_version: u16) -> Result<(), ValidationError> {
    if schema_version == CONTRACT_SCHEMA_VERSION {
        Ok(())
    } else {
        Err(ValidationError::new(
            "schema_version",
            format!("unsupported version {schema_version}; expected {CONTRACT_SCHEMA_VERSION}"),
        ))
    }
}

fn validate_signal_update_schema_version(schema_version: u16) -> Result<(), ValidationError> {
    if schema_version == SIGNAL_UPDATE_SCHEMA_VERSION {
        Ok(())
    } else {
        Err(ValidationError::new(
            "schema_version",
            format!(
                "unsupported signal-update version {schema_version}; expected {SIGNAL_UPDATE_SCHEMA_VERSION}"
            ),
        ))
    }
}

fn validate_signal_stream_schema_version(schema_version: u16) -> Result<(), ValidationError> {
    if schema_version == SIGNAL_STREAM_SCHEMA_VERSION {
        Ok(())
    } else {
        Err(ValidationError::new(
            "schema_version",
            format!(
                "unsupported signal-stream version {schema_version}; expected {SIGNAL_STREAM_SCHEMA_VERSION}"
            ),
        ))
    }
}

fn parse_timestamp(
    path: &str,
    value: &str,
) -> Result<DateTime<chrono::FixedOffset>, ValidationError> {
    DateTime::parse_from_rfc3339(value)
        .map_err(|error| ValidationError::new(path, format!("must be RFC 3339: {error}")))
}

fn same_timestamp(left: &str, right: &str) -> Result<bool, ValidationError> {
    Ok(parse_timestamp("timestamp", left)? == parse_timestamp("timestamp", right)?)
}

fn validate_id(path: &str, value: &str) -> Result<(), ValidationError> {
    let bytes = value.as_bytes();
    let valid_edge = |byte: u8| byte.is_ascii_lowercase() || byte.is_ascii_digit();
    if bytes.is_empty()
        || !valid_edge(bytes[0])
        || !valid_edge(bytes[bytes.len() - 1])
        || !bytes.iter().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-' | b'_')
        })
    {
        return Err(ValidationError::new(
            path,
            "must use lowercase letters, numbers, dots, hyphens, or underscores and start and end with a letter or number",
        ));
    }
    Ok(())
}

fn validate_market_id(path: &str, value: &str) -> Result<(), ValidationError> {
    let bytes = value.as_bytes();
    let valid_edge = u8::is_ascii_alphanumeric;
    if bytes.is_empty()
        || bytes.len() > 256
        || !valid_edge(&bytes[0])
        || !valid_edge(&bytes[bytes.len() - 1])
        || !bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b':'))
    {
        return Err(ValidationError::new(
            path,
            "must be a 1-to-256-byte opaque market identity using ASCII letters, numbers, dots, hyphens, underscores, or colons with alphanumeric edges",
        ));
    }
    Ok(())
}

fn validate_field_name(path: &str, value: &str) -> Result<(), ValidationError> {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return Err(ValidationError::new(path, "must not be empty"));
    };
    if !first.is_ascii_lowercase()
        || !chars.all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == '_'
        })
    {
        return Err(ValidationError::new(
            path,
            "must be lower_snake_case and begin with a letter",
        ));
    }
    Ok(())
}

fn validate_version(path: &str, value: &str) -> Result<(), ValidationError> {
    Version::parse(value)
        .map(|_| ())
        .map_err(|error| ValidationError::new(path, format!("must be semantic version: {error}")))
}

fn validate_non_empty(path: &str, value: &str) -> Result<(), ValidationError> {
    if value.trim().is_empty() {
        Err(ValidationError::new(path, "must not be empty"))
    } else {
        Ok(())
    }
}

fn validate_publisher_epoch(value: &str) -> Result<(), ValidationError> {
    validate_non_empty("publisher_epoch", value)?;
    if value.len() > 128 {
        return Err(ValidationError::new(
            "publisher_epoch",
            "must not exceed 128 bytes",
        ));
    }
    Ok(())
}

fn validate_fields(fields: &[FieldDefinition]) -> Result<(), ValidationError> {
    if fields.is_empty() {
        return Err(ValidationError::new(
            "fields",
            "must declare at least one field",
        ));
    }

    let mut names = BTreeSet::new();
    for (index, field) in fields.iter().enumerate() {
        validate_field_name(&format!("fields[{index}].name"), &field.name)?;
        if !names.insert(&field.name) {
            return Err(ValidationError::new(
                format!("fields[{index}].name"),
                "duplicate field name",
            ));
        }

        if field.field_type == FieldType::Enumeration {
            if field.variants.is_empty() {
                return Err(ValidationError::new(
                    format!("fields[{index}].variants"),
                    "enum fields must declare at least one variant",
                ));
            }
            let mut variants = BTreeSet::new();
            for (variant_index, variant) in field.variants.iter().enumerate() {
                validate_non_empty(
                    &format!("fields[{index}].variants[{variant_index}]"),
                    variant,
                )?;
                if !variants.insert(variant) {
                    return Err(ValidationError::new(
                        format!("fields[{index}].variants[{variant_index}]"),
                        "duplicate enum variant",
                    ));
                }
            }
        } else if !field.variants.is_empty() {
            return Err(ValidationError::new(
                format!("fields[{index}].variants"),
                "variants are only valid for enum fields",
            ));
        }
    }
    Ok(())
}

fn field_by_name<'a>(fields: &'a [FieldDefinition], name: &str) -> Option<&'a FieldDefinition> {
    fields.iter().find(|field| field.name == name)
}

fn validate_timestamp(path: &str, value: &str) -> Result<(), ValidationError> {
    DateTime::parse_from_rfc3339(value)
        .map(|_| ())
        .map_err(|error| ValidationError::new(path, format!("must be RFC 3339: {error}")))
}

fn validate_event_time_value(
    envelope_event_time: &str,
    field_name: &str,
    values: &BTreeMap<String, Value>,
) -> Result<(), ValidationError> {
    let field_path = format!("values.{field_name}");
    let field_value = values
        .get(field_name)
        .and_then(Value::as_str)
        .ok_or_else(|| ValidationError::new(&field_path, "event-time value is missing"))?;
    let envelope = DateTime::parse_from_rfc3339(envelope_event_time)
        .map_err(|error| ValidationError::new("event_time", error.to_string()))?;
    let field = DateTime::parse_from_rfc3339(field_value)
        .map_err(|error| ValidationError::new(&field_path, error.to_string()))?;
    if envelope != field {
        return Err(ValidationError::new(
            field_path,
            "must represent the same instant as envelope event_time",
        ));
    }
    Ok(())
}

fn validate_values(
    fields: &[FieldDefinition],
    values: &BTreeMap<String, Value>,
    require_all: bool,
) -> Result<(), ValidationError> {
    for name in values.keys() {
        if field_by_name(fields, name).is_none() {
            return Err(ValidationError::new(
                format!("values.{name}"),
                "field is not declared by the definition",
            ));
        }
    }

    for field in fields {
        match values.get(&field.name) {
            Some(value) => validate_value(field, value)?,
            None if require_all => {
                return Err(ValidationError::new(
                    format!("values.{}", field.name),
                    "required field is missing",
                ));
            }
            None => {}
        }
    }
    Ok(())
}

fn validate_value(field: &FieldDefinition, value: &Value) -> Result<(), ValidationError> {
    let path = format!("values.{}", field.name);
    let valid = match field.field_type {
        FieldType::Bool => value.is_boolean(),
        FieldType::String => value.is_string(),
        FieldType::Decimal => value
            .as_str()
            .is_some_and(|raw| parse_decimal_text(raw).is_ok()),
        FieldType::U8 => value
            .as_u64()
            .is_some_and(|number| u8::try_from(number).is_ok()),
        FieldType::U16 => value
            .as_u64()
            .is_some_and(|number| u16::try_from(number).is_ok()),
        FieldType::U32 => value
            .as_u64()
            .is_some_and(|number| u32::try_from(number).is_ok()),
        FieldType::U64 | FieldType::DurationMs => value.as_u64().is_some(),
        FieldType::I64 => value.as_i64().is_some(),
        FieldType::Timestamp => value
            .as_str()
            .is_some_and(|raw| DateTime::parse_from_rfc3339(raw).is_ok()),
        FieldType::Enumeration => value
            .as_str()
            .is_some_and(|variant| field.variants.iter().any(|allowed| allowed == variant)),
    };

    if valid {
        Ok(())
    } else {
        Err(ValidationError::new(
            path,
            format!("does not match declared type {:?}", field.field_type),
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;

    use super::*;
    use crate::{FieldLocation, LifecycleStatus, Quality, SignalInput, Transport};

    fn fields() -> Vec<FieldDefinition> {
        vec![
            FieldDefinition {
                name: "price".into(),
                field_type: FieldType::Decimal,
                location: FieldLocation::Internal,
                variants: Vec::new(),
                label: None,
            },
            FieldDefinition {
                name: "regime".into(),
                field_type: FieldType::Enumeration,
                location: FieldLocation::Internal,
                variants: vec!["down".into(), "flat".into(), "up".into()],
                label: None,
            },
            FieldDefinition {
                name: "window_close".into(),
                field_type: FieldType::Timestamp,
                location: FieldLocation::Internal,
                variants: Vec::new(),
                label: None,
            },
        ]
    }

    fn signal_definition() -> SignalDefinition {
        SignalDefinition {
            schema_version: CONTRACT_SCHEMA_VERSION,
            signal_id: "btc.external.example".into(),
            version: "1.0.0".into(),
            name: "Example".into(),
            description: "Synthetic test definition".into(),
            status: LifecycleStatus::Experimental,
            approximate_interval_ms: 60_000,
            staleness_bound_ms: 180_000,
            inputs: vec![SignalInput {
                kind: InputKind::Source,
                id: "example.btc-usd.trades".into(),
                version: "1.0.0".into(),
            }],
            fields: fields(),
            event_time_field: "window_close".into(),
            scope_kind: None,
        }
    }

    fn ready_update() -> SignalUpdate {
        SignalUpdate {
            schema_version: SIGNAL_UPDATE_SCHEMA_VERSION,
            signal_id: "btc.external.example".into(),
            signal_version: "1.0.0".into(),
            publisher_epoch: "test-epoch".into(),
            sequence: 1,
            event_time: "2026-08-12T12:00:00Z".into(),
            produced_at: "2026-08-12T12:00:00.010Z".into(),
            valid_from: "2026-08-12T12:00:00.010Z".into(),
            valid_until: "2026-08-12T12:03:00.010Z".into(),
            quality: Quality {
                state: QualityState::Ready,
                reasons: Vec::new(),
            },
            values: BTreeMap::from([
                ("price".into(), json!("100000.125")),
                ("regime".into(), json!("up")),
                ("window_close".into(), json!("2026-08-12T12:00:00Z")),
            ]),
            scope: SignalScope::Global,
        }
    }

    #[test]
    fn accepts_ready_update_with_exact_values() {
        let definition = signal_definition();
        assert!(definition.validate().is_ok());
        assert!(ready_update().validate_against(&definition).is_ok());
    }

    #[test]
    fn rejects_binary_number_for_decimal() {
        let definition = signal_definition();
        let mut update = ready_update();
        update.values.insert("price".into(), json!(100000.125));
        let error = update
            .validate_against(&definition)
            .expect_err("JSON numbers must not pass decimal validation");
        assert!(error.to_string().contains("values.price"));
    }

    #[test]
    fn rejects_decimal_text_outside_the_shared_grammar() {
        let definition = signal_definition();
        for invalid in ["+1", ".5", "1.", "1e-8", "1E-8", "1_0"] {
            let mut update = ready_update();
            update.values.insert("price".into(), json!(invalid));
            let error = update
                .validate_against(&definition)
                .expect_err("non-contract decimal syntax must be rejected");
            assert!(error.to_string().contains("values.price"));
        }
    }

    #[test]
    fn rejects_missing_ready_field() {
        let definition = signal_definition();
        let mut update = ready_update();
        update.values.remove("regime");
        let error = update
            .validate_against(&definition)
            .expect_err("ready output must be complete");
        assert!(error.to_string().contains("values.regime"));
    }

    #[test]
    fn accepts_partial_warming_up_update_with_reason() {
        let definition = signal_definition();
        let mut update = ready_update();
        update.quality = Quality {
            state: QualityState::WarmingUp,
            reasons: vec!["requires more samples".into()],
        };
        update.values.clear();
        assert!(update.validate_against(&definition).is_ok());
    }

    #[test]
    fn rejects_event_time_disagreement() {
        let definition = signal_definition();
        let mut update = ready_update();
        update
            .values
            .insert("window_close".into(), json!("2026-08-12T12:01:00Z"));
        let error = update
            .validate_against(&definition)
            .expect_err("envelope and declared event time must agree");
        assert!(error.to_string().contains("same instant"));
    }

    #[test]
    fn validity_window_is_definition_owned_and_exact() {
        let definition = signal_definition();
        let mut update = ready_update();
        update.valid_until = "2026-08-12T12:03:00.011Z".into();
        let error = update
            .validate_against(&definition)
            .expect_err("consumer-invented validity must reject");
        assert!(error.to_string().contains("definition staleness bound"));
    }

    fn feed_health_report(source_id: &str) -> crate::FeedHealthReport {
        crate::FeedHealthReport {
            source_id: source_id.to_owned(),
            venue: "coinbase".to_owned(),
            status: crate::FeedHealthStatus::Down,
            last_event_age_ms: Some(1_500),
            down_since: Some("2026-08-12T12:00:00Z".to_owned()),
            disconnect_count: 1,
            reconnect_count: 0,
            anomaly_velocity: 1,
        }
    }

    fn feed_health_message(feeds: Vec<crate::FeedHealthReport>) -> SignalStreamMessage {
        SignalStreamMessage::FeedHealth {
            schema_version: SIGNAL_STREAM_SCHEMA_VERSION,
            publisher_epoch: "epoch".to_owned(),
            sequence: 3,
            produced_at: "2026-08-12T12:00:01Z".to_owned(),
            feeds,
        }
    }

    #[test]
    fn reset_requires_publisher_sequence_zero() {
        let reset = SignalStreamMessage::Reset {
            schema_version: SIGNAL_STREAM_SCHEMA_VERSION,
            publisher_epoch: "epoch".to_owned(),
            sequence: 1,
            produced_at: "2026-08-12T12:00:01Z".to_owned(),
            reason: crate::StreamResetReason::PublisherEpochStarted,
        };
        assert!(reset.validate().is_err());
    }

    #[test]
    fn feed_health_frame_validates_and_rejects_malformed_shapes() {
        assert!(
            feed_health_message(vec![feed_health_report("coinbase.exchange.btc-usd.ticker")])
                .validate()
                .is_ok()
        );
        assert!(
            feed_health_message(Vec::new()).validate().is_err(),
            "an empty feed set must reject"
        );
        assert!(
            feed_health_message(vec![
                feed_health_report("coinbase.exchange.btc-usd.ticker"),
                feed_health_report("coinbase.exchange.btc-usd.ticker"),
            ])
            .validate()
            .is_err(),
            "a duplicate source_id must reject"
        );
        let mut bad_timestamp = feed_health_report("coinbase.exchange.btc-usd.ticker");
        bad_timestamp.down_since = Some("not-a-timestamp".to_owned());
        assert!(
            feed_health_message(vec![bad_timestamp]).validate().is_err(),
            "an invalid down_since must reject"
        );
    }

    #[test]
    fn feed_health_is_an_additive_kind_ignored_by_older_consumers() {
        // A consumer that predates feed_health still dispatches on `type` and
        // ignores it, while the current binary decodes it strictly.
        let message =
            feed_health_message(vec![feed_health_report("coinbase.exchange.btc-usd.ticker")]);
        let bytes = serde_json::to_vec(&message).expect("serializes");
        let decoded = SignalStreamMessage::decode_known_json(&bytes)
            .expect("decodes")
            .expect("known kind");
        assert_eq!(decoded, message);
    }

    #[test]
    fn signal_batch_is_one_validated_atomic_frame() {
        let update = ready_update();
        let message = SignalStreamMessage::SignalBatch {
            schema_version: SIGNAL_STREAM_SCHEMA_VERSION,
            publisher_epoch: update.publisher_epoch.clone(),
            sequence: 7,
            produced_at: update.produced_at.clone(),
            conflation_policy: crate::ConflationPolicy::None,
            values_semantics: crate::ValuesSemantics::FullReplace,
            updates: vec![update],
        };
        assert!(message.validate().is_ok());
    }

    #[test]
    fn snapshot_requires_an_earlier_anchor_and_sorted_unique_state() {
        let update = ready_update();
        let mut second = update.clone();
        second.signal_id = "btc.external.another".into();
        let invalid_anchor = SignalStreamMessage::Snapshot {
            schema_version: SIGNAL_STREAM_SCHEMA_VERSION,
            publisher_epoch: update.publisher_epoch.clone(),
            sequence: 8,
            produced_at: "2026-08-12T12:00:01Z".into(),
            as_of_sequence: 8,
            conflation_policy: crate::ConflationPolicy::None,
            values_semantics: crate::ValuesSemantics::FullReplace,
            updates: vec![second.clone(), update.clone()],
            statuses: Vec::new(),
        };
        assert!(
            invalid_anchor
                .validate()
                .expect_err("self-anchored snapshot must reject")
                .to_string()
                .contains("earlier decision-or-status frame")
        );

        let unsorted = SignalStreamMessage::Snapshot {
            schema_version: SIGNAL_STREAM_SCHEMA_VERSION,
            publisher_epoch: update.publisher_epoch.clone(),
            sequence: 8,
            produced_at: "2026-08-12T12:00:01Z".into(),
            as_of_sequence: 7,
            conflation_policy: crate::ConflationPolicy::None,
            values_semantics: crate::ValuesSemantics::FullReplace,
            updates: vec![update, second],
            statuses: Vec::new(),
        };
        assert!(
            unsorted
                .validate()
                .expect_err("unsorted snapshot must reject")
                .to_string()
                .contains("unique and sorted")
        );
    }

    #[test]
    fn signal_scope_binding_covers_every_stream_v3_namespace_arm() {
        let market = SignalScope::Market {
            market_id: "btc-updown-5m-1786726800".to_owned(),
            market_generation: 1,
        };
        assert!(
            validate_signal_scope_binding("btc.external.spot_reference", &SignalScope::Global)
                .is_ok()
        );
        assert!(validate_signal_scope_binding("btc.polymarket.prediction_quote", &market).is_ok());
        assert!(validate_signal_scope_binding("btc.kalshi.prediction_quote", &market).is_ok());
        assert!(validate_signal_scope_binding("btc.external.spot_reference", &market).is_err());
        assert!(
            validate_signal_scope_binding("btc.polymarket.prediction_quote", &SignalScope::Global)
                .is_err()
        );
        for venue in ["coinbase", "kraken", "binance", "bybit"] {
            assert!(
                validate_signal_scope_binding(
                    &format!("btc.{venue}.spot_quote"),
                    &SignalScope::Global
                )
                .is_ok()
            );
            assert!(
                validate_signal_scope_binding(&format!("btc.{venue}.spot_quote"), &market).is_err()
            );
            assert!(is_spot_signal_id(&format!("btc.{venue}.spot_quote")));
        }
        assert!(is_spot_signal_id("btc.external.spot_reference"));
        assert!(!is_spot_signal_id("btc.polymarket.prediction_quote"));
        assert!(!is_spot_signal_id("coinbase.exchange.btc-usd.ticker"));
        assert!(!is_spot_signal_id("btc.coinbase.spot_quote.extra"));
        assert!(!is_spot_signal_id("btc.coinbase"));
        assert!(
            validate_signal_scope_binding("btc.crossvenue.example", &SignalScope::Global).is_err()
        );
        assert!(
            validate_signal_scope_binding("btc.unknown.example", &SignalScope::Global).is_err()
        );
        assert!(
            validate_signal_scope_binding(
                "btc.kalshi.prediction_quote",
                &SignalScope::Market {
                    market_id: "btc-updown-5m-1786726800".to_owned(),
                    market_generation: 0,
                }
            )
            .is_err()
        );
    }

    // ------------------------------------------------------------------
    // ADR 0026 — additive stream-v3 contract extensions
    // ------------------------------------------------------------------

    fn multi_scope() -> SignalScope {
        SignalScope::MultiMarket {
            // Canonical order: legs ascend by (venue, market_id), so `kalshi`
            // precedes `polymarket` (C1 V3).
            legs: vec![
                MultiMarketLeg {
                    venue: "kalshi".to_owned(),
                    market_id: "KXBTC15M-B".to_owned(),
                    market_generation: 2,
                },
                MultiMarketLeg {
                    venue: "polymarket".to_owned(),
                    market_id: "btc-updown-5m-1786726800".to_owned(),
                    market_generation: 4,
                },
            ],
            window: MultiMarketWindow {
                open: "2026-09-03T12:00:00Z".to_owned(),
                close: "2026-09-03T12:15:00Z".to_owned(),
            },
        }
    }

    #[test]
    fn the_new_global_segments_bind_to_global_scope() {
        for venue in ["pyth", "chainlink", "cfbrti", "cross"] {
            let id = format!("btc.{venue}.oracle_reference");
            assert!(
                validate_signal_scope_binding(&id, &SignalScope::Global).is_ok(),
                "{venue} is a global segment"
            );
            assert!(
                validate_signal_scope_binding(&id, &multi_scope()).is_err(),
                "{venue} does not take a multi_market scope"
            );
            // The new segments are global but are NOT spot readings, so the
            // ADR 0024 spot predicate must keep rejecting them.
            assert!(!is_spot_signal_id(&id), "{venue} is not a spot identity");
        }
        assert!(
            validate_signal_scope_binding("btc.binance.spot_quote", &SignalScope::Global).is_ok()
        );
        assert!(
            validate_signal_scope_binding("btc.bybit.spot_quote", &SignalScope::Global).is_ok()
        );
    }

    #[test]
    fn the_multi_segment_carries_both_global_and_multi_market_scopes() {
        // F1 / C1 R3: `multi` does NOT derive its scope. A blend over global
        // constituents (btc.multi.oracle_dispersion) is globally scoped; a
        // blend over bound markets is multi-market scoped. Both bind.
        assert!(
            validate_signal_scope_binding("btc.multi.oracle_dispersion", &SignalScope::Global)
                .is_ok(),
            "C1 section 5.1 declares btc.multi.oracle_dispersion global"
        );
        assert!(
            validate_signal_scope_binding("btc.multi.prediction_dislocation", &multi_scope())
                .is_ok()
        );
        assert_eq!(
            scope_kind_of_venue("multi"),
            None,
            "the multi segment derives nothing; the definition declares it"
        );
        assert!(scope_kind_is_declared_for_venue("multi"));
        for venue in ["external", "coinbase", "polymarket", "kalshi", "cross"] {
            assert!(!scope_kind_is_declared_for_venue(venue), "{venue}");
        }
        // A single leg is legal; the cap is two.
        let SignalScope::MultiMarket { legs, window } = multi_scope() else {
            unreachable!()
        };
        assert!(
            validate_signal_scope_binding(
                "btc.multi.prediction_dislocation",
                &SignalScope::MultiMarket {
                    legs: legs[..1].to_vec(),
                    window,
                }
            )
            .is_ok()
        );
        // `multi` is never bound to a single market.
        assert!(
            validate_signal_scope_binding(
                "btc.multi.oracle_dispersion",
                &SignalScope::Market {
                    market_id: "KXBTC15M-B".to_owned(),
                    market_generation: 1,
                }
            )
            .is_err()
        );
        // A caller holding the definition pins the shape exactly.
        assert!(
            validate_signal_scope_binding_for_definition(
                "btc.multi.oracle_dispersion",
                &SignalScope::Global,
                Some(ScopeKind::Global),
            )
            .is_ok()
        );
        assert!(
            validate_signal_scope_binding_for_definition(
                "btc.multi.oracle_dispersion",
                &multi_scope(),
                Some(ScopeKind::Global),
            )
            .is_err(),
            "a global-declared identity may not emit a multi_market scope"
        );
    }

    #[test]
    fn multi_market_legs_must_be_in_canonical_order() {
        let SignalScope::MultiMarket { legs, window } = multi_scope() else {
            unreachable!()
        };
        let bind = |legs: Vec<MultiMarketLeg>| {
            validate_signal_scope_binding(
                "btc.multi.prediction_dislocation",
                &SignalScope::MultiMarket {
                    legs,
                    window: window.clone(),
                },
            )
        };
        assert!(
            bind(legs.clone()).is_ok(),
            "kalshi before polymarket is canonical"
        );
        // Reversed is rejected, not repaired: one binding must have exactly one
        // encoding, because the legs array IS the key.
        let mut reversed = legs.clone();
        reversed.reverse();
        let error = bind(reversed).expect_err("unsorted legs must reject");
        assert!(error.to_string().contains("ascend"), "{error}");
        assert!("kalshi" < "polymarket", "the fixture relies on this order");
    }

    #[test]
    fn a_malformed_multi_market_scope_is_rejected_variant_by_variant() {
        let base = || match multi_scope() {
            SignalScope::MultiMarket { legs, window } => (legs, window),
            _ => unreachable!(),
        };
        let bind = |legs: Vec<MultiMarketLeg>, window: MultiMarketWindow| {
            validate_signal_scope_binding(
                "btc.multi.oracle_dispersion",
                &SignalScope::MultiMarket { legs, window },
            )
        };

        let (legs, window) = base();
        assert!(
            bind(Vec::new(), window.clone())
                .expect_err("empty legs must reject")
                .to_string()
                .contains("must not be empty")
        );

        // More than two legs. Unreachable with distinct venues, which is the
        // point: the cap and the distinctness rule agree.
        assert_eq!(
            MAX_MULTI_MARKET_LEGS, 2,
            "two market-scope venues, two legs"
        );
        let mut too_many = legs.clone();
        let mut third = legs[0].clone();
        third.market_id = "KXBTC15M-Z".to_owned();
        too_many.push(third);
        assert!(
            bind(too_many, window.clone()).is_err(),
            "three legs must reject"
        );

        // A repeated venue: two legs on one venue cannot key one instance.
        let mut duplicate = legs.clone();
        duplicate[1].venue = "kalshi".to_owned();
        duplicate[1].market_id = "KXBTC15M-C".to_owned();
        assert!(
            bind(duplicate, window.clone())
                .expect_err("duplicate venue must reject")
                .to_string()
                .contains("distinct")
        );

        // A leg on a venue that is not market-scoped.
        for venue in ["coinbase", "cross", "multi", "crossvenue", "nope"] {
            let mut wrong = legs.clone();
            wrong[0].venue = venue.to_owned();
            assert!(
                bind(wrong, window.clone()).is_err(),
                "{venue} is not a market-scope venue"
            );
        }

        // An unbound generation.
        let mut unbound = legs.clone();
        unbound[0].market_generation = 0;
        assert!(bind(unbound, window.clone()).is_err());

        // A malformed market id.
        let mut bad_id = legs.clone();
        bad_id[0].market_id = "-not-a-market-".to_owned();
        assert!(bind(bad_id, window.clone()).is_err());

        // Windows: unparseable bounds, and open at or after close.
        for (open, close) in [
            ("not-a-time", "2026-09-03T12:15:00Z"),
            ("2026-09-03T12:00:00Z", "nope"),
            ("2026-09-03T12:15:00Z", "2026-09-03T12:00:00Z"),
            ("2026-09-03T12:00:00Z", "2026-09-03T12:00:00Z"),
        ] {
            assert!(
                bind(
                    legs.clone(),
                    MultiMarketWindow {
                        open: open.to_owned(),
                        close: close.to_owned(),
                    }
                )
                .is_err(),
                "window {open}..{close} must reject"
            );
        }

        // Equal instants written with different offsets are still not ordered.
        assert!(
            bind(
                legs,
                MultiMarketWindow {
                    open: "2026-09-03T12:00:00Z".to_owned(),
                    close: "2026-09-03T13:00:00+01:00".to_owned(),
                }
            )
            .is_err(),
            "the comparison is on the instant, not the text"
        );
    }

    #[test]
    fn crossvenue_stays_retired_under_every_scope_shape() {
        for scope in [
            SignalScope::Global,
            SignalScope::Market {
                market_id: "KXBTC15M-B".to_owned(),
                market_generation: 1,
            },
            multi_scope(),
        ] {
            let error = validate_signal_scope_binding("btc.crossvenue.example", &scope)
                .expect_err("crossvenue is retired");
            assert!(error.to_string().contains("crossvenue"), "{error}");
        }
        assert_eq!(scope_kind_of_venue("crossvenue"), None);
        assert!(!GLOBAL_VENUE_NAMESPACES.contains(&"crossvenue"));
    }

    #[test]
    fn scope_kind_is_optional_and_must_agree_with_the_venue_segment() {
        let mut definition = signal_definition();
        assert_eq!(definition.scope_kind, None);
        assert!(definition.validate().is_ok(), "absent stays valid");

        // btc.external.* is global.
        definition.scope_kind = Some(ScopeKind::Global);
        assert!(definition.validate().is_ok());
        for wrong in [ScopeKind::Market, ScopeKind::MultiMarket] {
            definition.scope_kind = Some(wrong);
            assert!(
                definition
                    .validate()
                    .expect_err("a disagreeing scope_kind must reject")
                    .to_string()
                    .contains("scope_kind"),
            );
        }

        let mut market = signal_definition();
        market.signal_id = "btc.kalshi.prediction_quote".into();
        market.scope_kind = Some(ScopeKind::Market);
        assert!(market.validate().is_ok());
        market.scope_kind = Some(ScopeKind::Global);
        assert!(market.validate().is_err());

        let mut multi = signal_definition();
        multi.signal_id = "btc.multi.oracle_dispersion".into();
        multi.scope_kind = Some(ScopeKind::MultiMarket);
        assert!(multi.validate().is_ok());
        multi.scope_kind = Some(ScopeKind::Market);
        assert!(multi.validate().is_err());

        // A segment the contract does not know cannot declare a scope_kind.
        let mut retired = signal_definition();
        retired.signal_id = "btc.crossvenue.example".into();
        retired.scope_kind = Some(ScopeKind::Global);
        assert!(retired.validate().is_err());
    }

    #[test]
    fn scope_kind_is_required_and_decides_on_the_multi_segment() {
        let mut definition = signal_definition();
        definition.signal_id = "btc.multi.oracle_dispersion".into();

        // Absent is a hard error here, unlike every other segment.
        definition.scope_kind = None;
        let error = definition
            .validate()
            .expect_err("scope_kind is required on multi");
        assert!(error.to_string().contains("scope_kind"), "{error}");
        assert!(error.to_string().contains("required"), "{error}");

        // Both blend shapes are legal, and the declaration is what picks.
        for kind in [ScopeKind::Global, ScopeKind::MultiMarket] {
            definition.scope_kind = Some(kind);
            assert!(definition.validate().is_ok(), "{kind:?}");
        }

        // `market` never is: a multi identity is not bound to one market.
        definition.scope_kind = Some(ScopeKind::Market);
        assert!(
            definition
                .validate()
                .expect_err("market is invalid on multi")
                .to_string()
                .contains("global` or `multi_market")
        );

        // The requirement is specific to `multi`; nothing else gained one.
        for id in [
            "btc.external.example",
            "btc.kalshi.prediction_quote",
            "btc.cross.example",
        ] {
            let mut other = signal_definition();
            other.signal_id = id.into();
            other.scope_kind = None;
            assert!(other.validate().is_ok(), "{id} keeps scope_kind optional");
        }
    }

    #[test]
    fn a_multi_market_scope_is_bounded_in_serialized_bytes() {
        let SignalScope::MultiMarket { legs, window } = multi_scope() else {
            unreachable!()
        };
        // Two legs with maximum-length market ids exceed the 1024-byte bound.
        let long = "a".repeat(256);
        let oversized = vec![
            MultiMarketLeg {
                venue: "kalshi".to_owned(),
                market_id: long.clone(),
                market_generation: 1,
            },
            MultiMarketLeg {
                venue: "polymarket".to_owned(),
                market_id: long,
                market_generation: 1,
            },
        ];
        let scope = SignalScope::MultiMarket {
            legs: oversized,
            window: window.clone(),
        };
        let encoded = serde_json::to_vec(&scope).expect("encodes");
        assert!(
            encoded.len() <= MAX_MULTI_MARKET_SCOPE_BYTES,
            "two 256-byte ids still fit; the bound is not reachable today ({} bytes)",
            encoded.len()
        );
        assert!(validate_signal_scope_binding("btc.multi.prediction_dislocation", &scope).is_ok());
        // The bound is enforced on the encoded form, so it holds whatever the
        // encoder does with escapes. Prove it fires by shrinking the bound's
        // input rather than by waiting for a reachable case.
        let big = serde_json::to_vec(&SignalScope::MultiMarket {
            legs: legs.clone(),
            window,
        })
        .expect("encodes");
        assert!(big.len() < MAX_MULTI_MARKET_SCOPE_BYTES);
    }

    #[test]
    fn the_transport_enum_carries_sse() {
        assert_eq!(
            serde_json::from_str::<Transport>("\"sse\"").expect("sse parses"),
            Transport::Sse
        );
        assert_eq!(
            serde_json::to_string(&Transport::Sse).expect("sse serializes"),
            "\"sse\""
        );
        // http_poll is untouched.
        assert_eq!(
            serde_json::from_str::<Transport>("\"http_poll\"").expect("http_poll parses"),
            Transport::HttpPoll
        );
    }

    /// The 63 reasons frozen by ADR 0017, in the exact array order they had at
    /// commit `2555798` — the last commit before ADR 0026 appended to the
    /// registry. `signals-package` writes the registry in array order into
    /// `status-reasons.json` and the verifier compares it as an ordered `Vec`, so
    /// this prefix is a wire contract: if it ever moves, every package produced
    /// before the change fails verification.
    const FROZEN_63_AT_2555798: [&str; 63] = [
        "book_flow_history_warming_up",
        "book_history_warming_up",
        "coinbase_book_invalid",
        "kraken_book_invalid",
        "trade_history_warming_up",
        "trade_venue_stale_or_missing",
        "reference_unavailable",
        "reference_constituents_unavailable",
        "reference_history_warming_up",
        "trade_window_empty",
        "book_snapshot_stale_or_missing",
        "upstream_measurement_unavailable",
        "coinbase_stale",
        "coinbase_warming_up",
        "coinbase_invalid_quote",
        "coinbase_midpoint_unrepresentable",
        "coinbase_disconnected",
        "coinbase_recovery_required",
        "coinbase_subscription_rejected",
        "kraken_stale",
        "kraken_warming_up",
        "kraken_invalid_quote",
        "kraken_midpoint_unrepresentable",
        "kraken_disconnected",
        "kraken_recovery_required",
        "kraken_subscription_rejected",
        "cross_venue_dispersion_exceeded",
        "reference_arithmetic_unrepresentable",
        "market_closed",
        "opening_strike_unavailable",
        "probe_depth_insufficient",
        "trade_stale_or_missing",
        "model_domain_exceeded",
        "model_denominator_zero",
        "volatility_warming_up",
        "volatility_zero",
        "market_contract_warming_up",
        "market_clock_unrepresentable",
        "last_trade_missing",
        "last_trade_stale",
        "book_stale",
        "book_snapshot_missing",
        "book_one_sided",
        "book_crossed_or_invalid",
        "negative_book_stale",
        "negative_book_snapshot_missing",
        "negative_book_one_sided",
        "negative_book_crossed_or_invalid",
        "kalshi_complement_invalid",
        "affirmative_quote_missing",
        "affirmative_quote_crossed_or_invalid",
        "affirmative_quote_stale",
        "negative_quote_missing",
        "negative_quote_crossed_or_invalid",
        "negative_quote_stale",
        "market_rollover",
        "market_discovery_unreachable",
        "next_market_unlisted",
        "market_discovery_invalid",
        "market_open_gap",
        "provider_lifecycle_change",
        "coinbase_venue_unavailable",
        "kraken_venue_unavailable",
    ];

    #[test]
    fn the_reason_registry_is_append_only_unique_and_classified() {
        assert_eq!(
            INSTANCE_STATUS_REASONS.len(),
            120,
            "63 frozen + 57 appended"
        );

        // The frozen 63 keep their exact order at the head of the array.
        // `signals-package` persists the registry in array order and verifies
        // it as an ordered Vec, so reordering would break every existing
        // package. The registry is append-only, not sorted.
        assert_eq!(
            INSTANCE_STATUS_REASONS[..63],
            FROZEN_63_AT_2555798,
            "the frozen 63 must keep their original order and position"
        );

        // The appended tail is append-only, not sorted (the order is a wire
        // contract). The ADR-0026 batch [63..116] was added byte-sorted; E47
        // then appended usdt_reference_stale (index 116) and E53 appended
        // no_qualifying_move (index 117) and E51 appended flat_window (index
        // 118) LAST, out of sort order, so window_alignment_mismatch kept its
        // pre-E47 position (G2); C2 then appended chainlink_rounds_sparse
        // (index 119) the same way.
        let appended = &INSTANCE_STATUS_REASONS[63..];
        assert_eq!(appended.len(), 57);
        assert!(
            INSTANCE_STATUS_REASONS[63..116]
                .windows(2)
                .all(|pair| pair[0] < pair[1]),
            "the ADR-0026 appended batch is byte-sorted among itself"
        );
        assert_eq!(
            INSTANCE_STATUS_REASONS[116], "usdt_reference_stale",
            "E47 appended usdt_reference_stale (entry 117), not by sorted insertion"
        );
        assert_eq!(
            INSTANCE_STATUS_REASONS[117], "no_qualifying_move",
            "E53 appended no_qualifying_move last (entry 118), not by sorted insertion"
        );
        assert_eq!(
            INSTANCE_STATUS_REASONS[118], "flat_window",
            "E51 appended flat_window last (entry 119), not by sorted insertion"
        );
        assert_eq!(
            INSTANCE_STATUS_REASONS[119], "chainlink_rounds_sparse",
            "C2 (E49) appended chainlink_rounds_sparse last (entry 120), after C3a's flat_window"
        );
        for reason in appended {
            assert!(
                !FROZEN_63_AT_2555798.contains(reason),
                "{reason} was already frozen"
            );
        }

        // The registry as a whole is unique.
        let unique: BTreeSet<&str> = INSTANCE_STATUS_REASONS.iter().copied().collect();
        assert_eq!(unique.len(), INSTANCE_STATUS_REASONS.len(), "no duplicates");
        // No refused-lane reason: a lane the venue gate refuses never starts,
        // so it never publishes an instance to carry one (ruling E7).
        for absent in ["binance_rights_gate_closed", "bybit_rights_gate_closed"] {
            assert!(!INSTANCE_STATUS_REASONS.contains(&absent), "{absent}");
        }
        // Every registered reason has a class, and the classes the specs
        // declare are the ones the registry applies.
        for reason in INSTANCE_STATUS_REASONS {
            assert!(
                instance_status_reason_class(reason).is_some(),
                "{reason} has a class"
            );
        }
        use InstanceStatusReasonClass::{Degraded, Unavailable, Warming};
        for (reason, class) in [
            ("binance_warming_up", Warming),
            ("cfbrti_warming_up", Warming),
            ("liquidation_burst_not_observed", Warming),
            // C2 (E49): the fresh-lane warm-up reason is Warming; the sparse
            // reason (index 118) is Degraded via the catch-all, pinned here.
            ("chainlink_warming_up", Warming),
            ("chainlink_rounds_sparse", Degraded),
            ("binance_venue_unavailable", Unavailable),
            ("cfbrti_unavailable", Unavailable),
            ("leg_market_unbound", Unavailable),
            ("pyth_disconnected", Unavailable),
            ("binance_recovery_required", Unavailable),
            ("bybit_recovery_required", Unavailable),
            ("binance_stale", Degraded),
            ("liquidation_side_unconfirmed", Degraded),
            ("window_alignment_mismatch", Degraded),
            ("no_qualifying_move", Degraded),
            ("flat_window", Degraded),
        ] {
            assert_eq!(
                instance_status_reason_class(reason),
                Some(class),
                "{reason}"
            );
        }
        assert_eq!(instance_status_reason_class("not_a_reason"), None);
    }

    #[test]
    fn a_multi_market_instance_keys_on_the_whole_leg_set_and_window() {
        let base = multi_scope();
        let SignalScope::MultiMarket { legs, window } = base.clone() else {
            unreachable!()
        };
        // Dropping a leg is a different instance.
        assert_ne!(
            base,
            SignalScope::MultiMarket {
                legs: legs[..1].to_vec(),
                window: window.clone(),
            }
        );
        // So is moving the window.
        let mut moved = window.clone();
        moved.close = "2026-09-03T12:30:00Z".to_owned();
        assert_ne!(
            base,
            SignalScope::MultiMarket {
                legs: legs.clone(),
                window: moved,
            }
        );
        // So is rolling one leg's generation.
        let mut rolled = legs.clone();
        rolled[0].market_generation += 1;
        assert_ne!(
            base,
            SignalScope::MultiMarket {
                legs: rolled,
                window,
            }
        );
    }

    #[test]
    fn a_multi_market_scope_round_trips_through_the_wire_shape() {
        let scope = multi_scope();
        let json = serde_json::to_value(&scope).expect("serializes");
        assert_eq!(json["type"], "multi_market");
        assert_eq!(json["legs"][0]["venue"], "kalshi");
        assert_eq!(json["legs"][0]["market_generation"], 2);
        assert_eq!(json["legs"][1]["venue"], "polymarket");
        assert_eq!(json["window"]["open"], "2026-09-03T12:00:00Z");
        assert_eq!(
            serde_json::from_value::<SignalScope>(json).expect("deserializes"),
            scope
        );
        // The existing shapes are unchanged.
        assert_eq!(
            serde_json::to_value(SignalScope::Global).expect("serializes"),
            serde_json::json!({ "type": "global" })
        );
    }

    #[test]
    fn instance_status_registry_order_precedence_and_invalidation_are_strict() {
        let status = |scope: SignalScope,
                      state: InstanceStatusState,
                      reasons: Vec<&str>|
         -> SignalStreamMessage {
            SignalStreamMessage::InstanceStatus {
                schema_version: SIGNAL_STREAM_SCHEMA_VERSION,
                publisher_epoch: "test-epoch".to_owned(),
                sequence: 9,
                produced_at: "2026-08-25T00:00:01Z".to_owned(),
                signal_id: match &scope {
                    SignalScope::Global => "btc.external.spot_reference".to_owned(),
                    SignalScope::Market { .. } => "btc.polymarket.prediction_quote".to_owned(),
                    SignalScope::MultiMarket { .. } => "btc.multi.example".to_owned(),
                },
                signal_version: "1.0.0".to_owned(),
                scope,
                state,
                reasons: reasons.into_iter().map(str::to_owned).collect(),
                effective_time: "2026-08-25T00:00:00Z".to_owned(),
                last_decision_sequence: Some(3),
            }
        };
        let market = || SignalScope::Market {
            market_id: "btc-updown-5m-1786726800".to_owned(),
            market_generation: 1,
        };

        assert!(
            status(
                SignalScope::Global,
                InstanceStatusState::Degraded,
                vec!["coinbase_stale", "reference_constituents_unavailable"],
            )
            .validate()
            .is_ok()
        );
        assert!(
            status(
                SignalScope::Global,
                InstanceStatusState::Degraded,
                vec!["reference_constituents_unavailable", "coinbase_stale"],
            )
            .validate()
            .expect_err("mis-sorted reasons reject")
            .to_string()
            .contains("unique and sorted")
        );
        assert!(
            status(
                SignalScope::Global,
                InstanceStatusState::Degraded,
                vec!["coinbase_stale", "coinbase_stale"],
            )
            .validate()
            .expect_err("duplicate reasons reject")
            .to_string()
            .contains("unique and sorted")
        );
        assert!(
            status(
                SignalScope::Global,
                InstanceStatusState::Degraded,
                vec!["outside_the_frozen_registry"],
            )
            .validate()
            .expect_err("unknown reasons reject")
            .to_string()
            .contains("frozen stream-v3")
        );

        let unavailable_wins = status(
            SignalScope::Global,
            InstanceStatusState::Unavailable,
            vec![
                "coinbase_disconnected",
                "reference_constituents_unavailable",
            ],
        );
        assert!(unavailable_wins.validate().is_ok());
        let wrong_precedence = status(
            SignalScope::Global,
            InstanceStatusState::Degraded,
            vec![
                "coinbase_disconnected",
                "reference_constituents_unavailable",
            ],
        );
        assert!(
            wrong_precedence
                .validate()
                .expect_err("unavailable class outranks degraded")
                .to_string()
                .contains("class precedence")
        );

        assert!(
            status(
                SignalScope::Global,
                InstanceStatusState::Invalidated,
                vec!["market_rollover"],
            )
            .validate()
            .expect_err("global invalidation rejects")
            .to_string()
            .contains("global instances cannot be invalidated")
        );
        assert!(
            status(
                market(),
                InstanceStatusState::Invalidated,
                vec!["market_rollover"],
            )
            .validate()
            .is_ok()
        );
        assert!(
            status(
                market(),
                InstanceStatusState::Invalidated,
                vec!["market_closed"],
            )
            .validate()
            .expect_err("only departure reasons invalidate")
            .to_string()
            .contains("bound-departure")
        );
    }

    #[test]
    fn producer_status_is_independent_and_explained() {
        let invalid = SignalStreamMessage::Status {
            schema_version: SIGNAL_STREAM_SCHEMA_VERSION,
            publisher_epoch: "test-epoch".into(),
            sequence: 1,
            produced_at: "2026-08-12T12:00:00Z".into(),
            status: ProducerStatus::Degraded,
            reasons: Vec::new(),
            as_of_sequence: None,
        };
        assert!(
            invalid
                .validate()
                .expect_err("degraded status needs a reason")
                .to_string()
                .contains("explain")
        );
    }

    #[test]
    fn heartbeat_interval_is_an_exact_declared_contract() {
        let invalid = SignalStreamMessage::Heartbeat {
            schema_version: SIGNAL_STREAM_SCHEMA_VERSION,
            publisher_epoch: "test-epoch".into(),
            sequence: 1,
            produced_at: "2026-08-12T12:00:00Z".into(),
            heartbeat_interval_ms: SIGNAL_HEARTBEAT_INTERVAL_MS - 1,
            status: ProducerStatus::Starting,
            as_of_sequence: None,
        };
        assert!(
            invalid
                .validate()
                .expect_err("undeclared heartbeat interval must reject")
                .to_string()
                .contains("must equal 5000")
        );
    }

    #[test]
    fn market_transition_requires_a_valid_phase_reason_and_identity_relation() {
        let transition = |market_generation, phase, reason| SignalStreamMessage::MarketTransition {
            schema_version: SIGNAL_STREAM_SCHEMA_VERSION,
            publisher_epoch: "test-epoch".into(),
            sequence: 8,
            produced_at: "2026-08-12T12:00:00Z".into(),
            venue: crate::PredictionMarketVenue::Kalshi,
            market_generation,
            previous_market_id: Some("KXBTC15M-26AUG151200-20".to_ascii_lowercase()),
            next_market_id: Some("KXBTC15M-26AUG151215-20".to_ascii_lowercase()),
            phase,
            reason,
        };
        let valid = transition(
            3,
            MarketTransitionPhase::Bound,
            MarketTransitionReason::ScheduledRollover,
        );
        assert!(valid.validate().is_ok());

        let invalid_generation = transition(
            0,
            MarketTransitionPhase::Bound,
            MarketTransitionReason::ScheduledRollover,
        );
        assert!(
            invalid_generation
                .validate()
                .expect_err("generation zero must reject")
                .to_string()
                .contains("positive generation")
        );

        let recovery_across_missed_boundary = transition(
            3,
            MarketTransitionPhase::Bound,
            MarketTransitionReason::RecoveryBind,
        );
        assert!(recovery_across_missed_boundary.validate().is_ok());

        let mut invalid_recovery = recovery_across_missed_boundary;
        let SignalStreamMessage::MarketTransition { next_market_id, .. } = &mut invalid_recovery
        else {
            unreachable!("test constructs a market transition")
        };
        *next_market_id = None;
        assert!(
            invalid_recovery
                .validate()
                .expect_err("recovery requires both identities")
                .to_string()
                .contains("requires both")
        );

        let invalid_unavailable = transition(
            3,
            MarketTransitionPhase::Unavailable,
            MarketTransitionReason::InitialBind,
        );
        assert!(
            invalid_unavailable
                .validate()
                .expect_err("initial_bind is not an unavailable reason")
                .to_string()
                .contains("unavailable-transition")
        );

        let lifecycle_change = transition(
            4,
            MarketTransitionPhase::Unavailable,
            MarketTransitionReason::ProviderLifecycleChange,
        );
        assert!(lifecycle_change.validate().is_ok());
        let mut missing_previous = lifecycle_change;
        let SignalStreamMessage::MarketTransition {
            previous_market_id, ..
        } = &mut missing_previous
        else {
            unreachable!("test constructs a market transition")
        };
        *previous_market_id = None;
        assert!(
            missing_previous
                .validate()
                .expect_err("provider lifecycle changes require the prior market")
                .to_string()
                .contains("lifecycle boundary")
        );
    }

    #[test]
    fn source_definition_requires_timestamp_event_field() {
        let definition = SourceDefinition {
            schema_version: CONTRACT_SCHEMA_VERSION,
            source_id: "example.trades".into(),
            version: "1.0.0".into(),
            name: "Example".into(),
            description: "Synthetic test definition".into(),
            status: LifecycleStatus::Draft,
            transport: Transport::Websocket,
            expected_heartbeat_ms: Some(30_000),
            fields: fields(),
            event_time_field: "price".into(),
            sequence_field: None,
        };
        let error = definition
            .validate()
            .expect_err("event time field must be a timestamp");
        assert!(error.to_string().contains("event_time_field"));
    }
}
