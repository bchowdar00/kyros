//! Deterministic BTC signal processors built from normalized source events.

mod atomic_file;
// Candidate-set computations, all now adopted; the module names record where
// each computation was specified rather than its adoption state.
// `candidate_prediction_signals` holds the seven identities adopted on
// 2026-09-01 (per-venue pair cost, venue repricing lag, tie-rule context, and
// the spot early-move fade context), `candidate_settlement_signals` the two
// adopted on 2026-09-02 (Polymarket settlement-anchor state and oracle
// divergence), and `candidate_spot_signals` the four spot-microstructure
// measurements adopted the same day as eight per-venue identities (ADR 0024's
// amendment). The spot and prediction processors wire all of them.
pub mod candidate_prediction_signals;
pub mod candidate_settlement_signals;
pub mod candidate_spot_signals;
pub mod checkpoint;
mod crc32c;
pub mod cross_signals;
mod decimal_math;
mod digest;
pub mod exit_code;
pub mod feed_health;
// C5 in-process generation rotation: the shared file-generation layout
// (naming, RUN-META schema, atomic CURRENT write, rotation deadlines) the
// three runtimes drive. See file_generation.rs.
pub mod file_generation;
mod horizon;
pub mod journal;
mod journal_core;
pub mod lane;
pub mod lane_checkpoint;
pub mod lane_gate;
pub mod lane_journal;
pub mod lane_runtime;
mod market_state;
pub mod multi_runtime;
pub mod multi_signals;
pub mod oracle_signals;
pub mod prediction_checkpoint;
pub mod prediction_journal;
pub mod prediction_runtime;
mod prediction_signals;
mod realized_volatility;
// Deterministic per-venue raw-event join for the spot microstructure family.
// Join-only infrastructure: it computes no signal value, and is consumed only
// by the microstructure driver each per-venue spot lane owns.
pub mod spot_event_join;
mod spot_reference;
mod spot_signals;
pub mod stream_accounting;
// The shared C3a technical-analysis unit: the one-second sample ring, the
// one-minute bar series and the seven indicators every price scope publishes.
mod ta;
// C3b (ruling E51): the update-rate counters behind `tick_velocity`,
// used by the spot venue lanes and the prediction lanes.
mod tick_velocity;
// C3b (ruling E51): the prediction-venue bar/indicator toolkit (10 s bars,
// Wilder RSI, ROC, population z-score). Kept separate from `ta` (C3a's 60 s
// TaEngine for the price scopes) because the two share no types; see the
// FUTURE-BUILDS note at the head of ta_bars.rs.
pub mod ta_bars;
pub mod usdt_reference;
pub mod venue_derivatives_signals;
pub mod venue_spot_signals;
// The spot and prediction lane runners, lifted out of their binaries so the
// exchange server can drive every lane in-process. The venue lanes were
// already library code (`lane_runtime::run_lane`).
pub mod prediction_run;
pub mod spot_run;

pub use prediction_signals::{
    KALSHI_EXECUTABLE_EDGE_ID, KALSHI_FAIR_UP_PROBABILITY_ID, KALSHI_MARKET_CONTRACT_ID,
    KALSHI_PAIR_COST_STATE_ID, KALSHI_PREDICTION_LIQUIDITY_ID, KALSHI_PREDICTION_QUOTE_ID,
    KALSHI_PREDICTION_TRADE_FLOW_ID, KALSHI_STANDARDIZED_MONEYNESS_ID, KALSHI_TIE_RULE_CONTEXT_ID,
    KALSHI_VENUE_REPRICING_LAG_ID, KALSHI_WINDOW_DELTA_ID, POLYMARKET_EXECUTABLE_EDGE_ID,
    POLYMARKET_FAIR_UP_PROBABILITY_ID, POLYMARKET_MARKET_CONTRACT_ID,
    POLYMARKET_PAIR_COST_STATE_ID, POLYMARKET_PREDICTION_LIQUIDITY_ID,
    POLYMARKET_PREDICTION_QUOTE_ID, POLYMARKET_PREDICTION_TRADE_FLOW_ID,
    POLYMARKET_STANDARDIZED_MONEYNESS_ID, POLYMARKET_TIE_RULE_CONTEXT_ID,
    POLYMARKET_VENUE_REPRICING_LAG_ID, POLYMARKET_WINDOW_DELTA_ID, PREDICTION_FLOW_WINDOW_NS,
    PREDICTION_LIQUIDITY_PROBE_UNITS, PREDICTION_QUOTE_STALENESS_NS, PREDICTION_SIGNAL_KIND_COUNT,
    PREDICTION_SIGNAL_VERSION, PredictionBatch, PredictionFeatureCheckpoint,
    PredictionSignalCheckpointState, PredictionSignalError, PredictionSignalProcessor,
    SettlementWindowPause, prediction_contract_documents, prediction_contract_documents_for,
    prediction_signal_contract_documents,
};

// The prediction surface is venue-asymmetric since the 2026-09-02 adoption, so
// the expected set is exported per venue rather than as one shared count.
// C3b (ruling E51): the prediction-venue TA and timing identities. The Kalshi
// window streak is exported alongside them even though it is index-gated, so a
// consumer names it from one place.
pub use prediction_signals::{
    KALSHI_EXPECTED_VALUE_ID, KALSHI_ROC_ID, KALSHI_RSI_ID, KALSHI_TICK_VELOCITY_ID,
    KALSHI_WINDOW_STREAK_ID, KALSHI_ZSCORE_ID, POLYMARKET_EXPECTED_VALUE_ID, POLYMARKET_ROC_ID,
    POLYMARKET_RSI_ID, POLYMARKET_TICK_VELOCITY_ID, POLYMARKET_ZSCORE_ID,
};

pub use prediction_signals::{
    CFBRTI_ORACLE_REFERENCE_ID, KALSHI_ORACLE_DIVERGENCE_ID, KALSHI_PREDICTION_SIGNAL_COUNT,
    KALSHI_SETTLEMENT_ANCHOR_STATE_ID, POLYMARKET_ORACLE_DIVERGENCE_ID,
    POLYMARKET_PREDICTION_SIGNAL_COUNT, POLYMARKET_SETTLEMENT_ANCHOR_STATE_ID,
    prediction_signal_count, prediction_signal_count_for, prediction_signal_ids,
    prediction_signal_ids_for,
};

pub use spot_reference::{
    COINBASE_TICKER_SOURCE_ID, COINBASE_TICKER_SOURCE_VERSION, HEALTH_INTERVAL_NS,
    KRAKEN_TICKER_SOURCE_ID, KRAKEN_TICKER_SOURCE_VERSION, MAX_DISPERSION_BPS,
    OUTPUT_DECIMAL_PLACES, SPOT_REFERENCE_ID, SPOT_REFERENCE_VERSION, STALENESS_BOUND_NS,
    SpotReferenceCheckpointState, SpotReferenceError, SpotReferenceProcessor,
    SpotReferenceRuntimeConfig, UnavailableReason, Venue,
};
pub use spot_signals::{
    ABSORPTION_STATE_ID, ACTIONABLE_DEPTH_IMBALANCE_ID, AGGRESSOR_FLOW_ID,
    CROSS_VENUE_DISPERSION_ID, EARLY_MOVE_FADE_CONTEXT_ID, LIQUIDITY_SHOCK_ID, MICRO_MOMENTUM_ID,
    ORDER_FLOW_IMBALANCE_ID, REALIZED_VOLATILITY_ID, SPOT_FEATURE_VERSION, SPOT_MICROPRICE_ID,
    SPOT_SIGNAL_KRAKEN_BOOK_DEPTH, SPOT_SIGNAL_SET_COUNT, SPOT_SIGNAL_SET_ID,
    SPOT_SIGNAL_SET_VERSION, SpotSignalCheckpointState, SpotSignalError,
    SpotSignalFeatureCheckpoint, SpotSignalProcessor, VENUE_SPOT_SIGNAL_COUNT, VENUE_SPOT_VENUES,
    VWAP_DEVIATION_ID, contract_documents as spot_signal_contract_documents, venue_spot_signal_ids,
};
