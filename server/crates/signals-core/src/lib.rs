//! Contracts shared by source adapters, signal processors, and repository tooling.

mod decimal;
mod definition;
mod fee;
mod runtime;
mod validation;
mod venue_gate;

pub use decimal::{DecimalTextError, MAX_DECIMAL_TEXT_BYTES, parse_decimal_text};
pub use definition::{
    FieldDefinition, FieldLocation, FieldType, InputKind, LifecycleStatus, ScopeKind,
    SignalDefinition, SignalInput, SourceDefinition, Transport,
};
pub use fee::{
    FeeRounding, checked_align_fee_up, checked_flat_per_contract_fee,
    checked_percentage_of_entry_notional_fee, checked_rate_times_probability_product_fee,
    round_fee,
};
pub use runtime::{
    ConflationPolicy, FeedHealthReport, FeedHealthStatus, InstanceStatusEntry, InstanceStatusState,
    MarketTransitionPhase, MarketTransitionReason, MultiMarketLeg, MultiMarketWindow,
    PredictionMarketVenue, ProducerStatus, Quality, QualityState, SignalInstanceKey, SignalScope,
    SignalStreamMessage, SignalUpdate, SourceEvent, StreamResetReason, ValuesSemantics,
};
pub use validation::{
    CONTRACT_SCHEMA_VERSION, INSTANCE_STATUS_REASONS, InstanceStatusReasonClass,
    SIGNAL_HEARTBEAT_INTERVAL_MS, SIGNAL_SNAPSHOT_INTERVAL_MS, SIGNAL_STREAM_SCHEMA_VERSION,
    SIGNAL_UPDATE_SCHEMA_VERSION, SPOT_VENUE_NAMESPACES, Validate, ValidationError,
    instance_status_reason_class, is_spot_signal_id, projected_scope_kind,
    scope_kind_is_declared_for_venue, scope_kind_of_venue, validate_signal_scope_binding,
    validate_signal_scope_binding_for_definition,
};
pub use validation::{
    GLOBAL_VENUE_NAMESPACES, MARKET_VENUE_NAMESPACES, MAX_MULTI_MARKET_LEGS,
    MAX_MULTI_MARKET_SCOPE_BYTES, MULTI_MARKET_VENUE_NAMESPACE,
};
pub use venue_gate::{
    ArmedIndexSockets, BINANCE_LANE, BYBIT_LANE, CROSS_LANE, DEFAULT_ENABLED_VENUES, EnabledVenues,
    INDEX_SOCKET_TOKENS, KALSHI_LANE, KNOWN_VENUE_TOKENS, LaneRequirement, LaneStart, ORACLE_LANE,
    POLYMARKET_LANE, SPOT_LANE, VENUE_GATE_ENV, VenueGateError, lane_start,
};
