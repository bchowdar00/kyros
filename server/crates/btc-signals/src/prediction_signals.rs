use std::collections::{BTreeMap, BTreeSet, VecDeque};

use chrono::{DateTime, Duration as ChronoDuration, SecondsFormat, Utc};
use rust_decimal::{Decimal, RoundingStrategy};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use signals_core::{
    FieldType, PredictionMarketVenue, Quality, QualityState, SIGNAL_UPDATE_SCHEMA_VERSION,
    SignalDefinition, SignalScope, SignalUpdate, SourceDefinition, SourceEvent, Validate,
    parse_decimal_text,
};
use thiserror::Error;

use crate::candidate_prediction_signals::{
    KalshiPairCostOutcome, KalshiPairCostStateCandidate, PairCostOutcome, PairCostStateCandidate,
    TieOutcome, TieRuleContextCandidate, TieRuleOutcome, VenueRepricingLagCandidate,
};
use crate::candidate_settlement_signals::{
    AnchorOutcome, Comparator, DivergenceOutcome, OracleDivergenceCandidate, SettledSide,
    SettlementAnchorStateCandidate,
};
use crate::decimal_math::{ln_fixed, phi_cdf, sqrt_fixed};
use crate::lane::LaneClock;
use crate::oracle_signals::{OracleReferenceEmitter, oracle_reference_values};
use crate::realized_volatility::{
    RealizedVolatility, VolatilityMetrics, classify_volatility_ratio, root_mean_square,
};
use crate::spot_reference::{
    COINBASE_TICKER_SOURCE_ID, KRAKEN_TICKER_SOURCE_ID, SpotReferenceCheckpointState,
    SpotReferenceError, SpotReferenceProcessor,
};
use crate::spot_signals::{MISSED_OBSERVATION_NS, SpotSignalError, direction_word};
use crate::ta_bars::{
    BarSeries, RSI_PERIOD, RSI_WARMUP_BARS, RsiRegime, SampleRing, TaError, ZScoreOutcome,
    roc_percent, wilder_rsi, zscore,
};
use crate::tick_velocity::TickVelocity;

// One pre-production V1 contract is shared by every prediction signal identity.
pub const PREDICTION_SIGNAL_VERSION: &str = "1.0.0";

/// Every distinct prediction signal kind across both venues. The per-venue
/// expected sets are subsets of this one table.
pub const PREDICTION_SIGNAL_KIND_COUNT: usize = 21;

/// Polymarket's expected set is the whole table; Kalshi's omits the two
/// settlement-anchor identities, which resolve on the rights-gated CF
/// Benchmarks BRTI and are therefore deferred on that venue (ADR 0023
/// amendment, 2026-09-02). Both counts are derived from `PredictionSignalKind`
/// rather than written down, so a venue-asymmetric adoption cannot leave a
/// stale literal in a runtime guard, a checkpoint assert, or a test.
pub const POLYMARKET_PREDICTION_SIGNAL_COUNT: usize =
    venue_prediction_signal_count(PredictionMarketVenue::Polymarket);
pub const KALSHI_PREDICTION_SIGNAL_COUNT: usize =
    venue_prediction_signal_count(PredictionMarketVenue::Kalshi);

/// The size of one venue's expected prediction-signal set: the number of
/// identities that venue's processor publishes on every decision.
#[must_use]
pub const fn prediction_signal_count(venue: PredictionMarketVenue) -> usize {
    match venue {
        PredictionMarketVenue::Polymarket => POLYMARKET_PREDICTION_SIGNAL_COUNT,
        PredictionMarketVenue::Kalshi => KALSHI_PREDICTION_SIGNAL_COUNT,
    }
}

/// One venue's expected prediction-signal identity set, in table order. This is
/// the single authority for "which identities does this venue publish"; the
/// runtime batch guard, the checkpoint feature-set check and the emitter table
/// all read it rather than restating a list.
#[must_use]
pub fn prediction_signal_ids(venue: PredictionMarketVenue) -> Vec<&'static str> {
    PredictionSignalKind::for_venue(venue)
        .filter_map(|kind| kind.id(venue))
        .collect()
}

const fn venue_prediction_signal_count(venue: PredictionMarketVenue) -> usize {
    let mut count = 0;
    let mut index = 0;
    while index < PREDICTION_SIGNAL_KIND_COUNT {
        if PredictionSignalKind::ALL[index].base_applies_to(venue) {
            count += 1;
        }
        index += 1;
    }
    count
}

/// The size of one venue's expected prediction-signal set for the given
/// index-socket arming. With the socket disarmed this equals
/// [`prediction_signal_count`]; armed, it adds the venue's index-gated
/// identities (the two Kalshi settlement twins, E43). The deployed four-venue
/// configuration runs disarmed, so its batch size is byte-unchanged.
#[must_use]
pub fn prediction_signal_count_for(venue: PredictionMarketVenue, index_armed: bool) -> usize {
    PredictionSignalKind::expected_kinds(venue, index_armed).count()
}

/// One venue's expected prediction-signal identity set for the given arming, in
/// table order. The arming-aware form of [`prediction_signal_ids`].
#[must_use]
pub fn prediction_signal_ids_for(
    venue: PredictionMarketVenue,
    index_armed: bool,
) -> Vec<&'static str> {
    PredictionSignalKind::expected_kinds(venue, index_armed)
        .filter_map(|kind| kind.id(venue))
        .collect()
}
pub const PREDICTION_QUOTE_STALENESS_NS: u64 = 10_000_000_000;
/// The C2 warm-up not-ready reason for the probability momentum/volatility ring.
/// The instance-status registry has no prediction-specific ring-warming reason
/// (`prediction_window_rolled` named in the C2 spec does not exist and the
/// registry is append-only and centrally governed), so this reuses the existing
/// registered `volatility_warming_up` reason the prediction lane already
/// publishes while its realized-volatility ring fills — mirroring the spot C2
/// lane, which likewise publishes one existing ring-warming reason for both its
/// horizon identities regardless of a roll.
const PROBABILITY_HORIZON_WARMING_REASON: &str = "volatility_warming_up";
pub const PREDICTION_FLOW_WINDOW_NS: u64 = 60_000_000_000;
pub const PREDICTION_LIQUIDITY_PROBE_UNITS: u32 = 10;

pub const POLYMARKET_MARKET_CONTRACT_ID: &str = "btc.polymarket.market_contract";
pub const POLYMARKET_PREDICTION_QUOTE_ID: &str = "btc.polymarket.prediction_quote";
pub const POLYMARKET_PREDICTION_LIQUIDITY_ID: &str = "btc.polymarket.prediction_liquidity";
pub const POLYMARKET_PREDICTION_TRADE_FLOW_ID: &str = "btc.polymarket.prediction_trade_flow";
pub const KALSHI_MARKET_CONTRACT_ID: &str = "btc.kalshi.market_contract";
pub const KALSHI_PREDICTION_QUOTE_ID: &str = "btc.kalshi.prediction_quote";
pub const KALSHI_PREDICTION_LIQUIDITY_ID: &str = "btc.kalshi.prediction_liquidity";
pub const KALSHI_PREDICTION_TRADE_FLOW_ID: &str = "btc.kalshi.prediction_trade_flow";

// Composite prediction signals (ADR 0016). Each is @1.0.0 and is derived from the
// venue rules card plus the internally hosted spot reference and realized
// volatility, so the identical published spot code path computes S and sigma.
pub const POLYMARKET_WINDOW_DELTA_ID: &str = "btc.polymarket.window_delta";
pub const POLYMARKET_STANDARDIZED_MONEYNESS_ID: &str = "btc.polymarket.standardized_moneyness";
pub const POLYMARKET_FAIR_UP_PROBABILITY_ID: &str = "btc.polymarket.fair_up_probability";
pub const POLYMARKET_EXECUTABLE_EDGE_ID: &str = "btc.polymarket.executable_edge";
pub const KALSHI_WINDOW_DELTA_ID: &str = "btc.kalshi.window_delta";
pub const KALSHI_STANDARDIZED_MONEYNESS_ID: &str = "btc.kalshi.standardized_moneyness";
pub const KALSHI_FAIR_UP_PROBABILITY_ID: &str = "btc.kalshi.fair_up_probability";
pub const KALSHI_EXECUTABLE_EDGE_ID: &str = "btc.kalshi.executable_edge";

// Probability momentum and volatility (C2, ruling E49). Each venue publishes the
// ten-second / one-minute / five-minute momentum and realized volatility of its
// own prediction_quote affirmative_midpoint on a one-second per-generation ring.
pub const POLYMARKET_PROBABILITY_MOMENTUM_ID: &str = "btc.polymarket.probability_momentum";
pub const POLYMARKET_PROBABILITY_VOLATILITY_ID: &str = "btc.polymarket.probability_volatility";
pub const KALSHI_PROBABILITY_MOMENTUM_ID: &str = "btc.kalshi.probability_momentum";
pub const KALSHI_PROBABILITY_VOLATILITY_ID: &str = "btc.kalshi.probability_volatility";

// The C3 TA and timing pack on the prediction venues (ruling E51, slice C3b).
// Five identities per venue over the affirmative midpoint and the admitted event
// stream, plus the Kalshi-only settled-window streak.
pub const POLYMARKET_RSI_ID: &str = "btc.polymarket.rsi";
pub const POLYMARKET_ROC_ID: &str = "btc.polymarket.roc";
pub const POLYMARKET_ZSCORE_ID: &str = "btc.polymarket.zscore";
pub const POLYMARKET_TICK_VELOCITY_ID: &str = "btc.polymarket.tick_velocity";
pub const POLYMARKET_EXPECTED_VALUE_ID: &str = "btc.polymarket.expected_value";
pub const KALSHI_RSI_ID: &str = "btc.kalshi.rsi";
pub const KALSHI_ROC_ID: &str = "btc.kalshi.roc";
pub const KALSHI_ZSCORE_ID: &str = "btc.kalshi.zscore";
pub const KALSHI_TICK_VELOCITY_ID: &str = "btc.kalshi.tick_velocity";
pub const KALSHI_EXPECTED_VALUE_ID: &str = "btc.kalshi.expected_value";
/// The settled-window streak. Kalshi only: the settled decisions it folds are
/// the settlement twins' final decisions (E37b/E37c), and Polymarket resolves on
/// the RTDS TWAP with no settlement-window completion, so this lane holds no
/// settled outcome for that venue.
pub const KALSHI_WINDOW_STREAK_ID: &str = "btc.kalshi.window_streak";

/// Research-default RSI bands, echoed by every ready `rsi` update.
const RSI_OVERSOLD_THRESHOLD: Decimal = Decimal::from_parts(30, 0, 0, false, 0);
const RSI_OVERBOUGHT_THRESHOLD: Decimal = Decimal::from_parts(70, 0, 0, false, 0);
/// One-second closes retained for the z-score windows: the longest is 15 m.
const PROBABILITY_RING_SECONDS: u64 = 900;
/// Prediction-venue bar length in seconds (ruling E58). Ten seconds, not the
/// spot/perp minute: a Wilder RSI(14) needs fifteen closed bars, and at ten
/// seconds those close in 150 s — inside a 900 s Kalshi generation and the second
/// half of a 300 s Polymarket window — where sixty-second bars never would.
const PROBABILITY_BAR_SECONDS: u64 = 10;
/// Closed ten-second bars retained for the RSI warm-up, with headroom (the
/// warm-up needs fifteen; a 900 s generation closes at most ninety).
const PROBABILITY_BAR_CAPACITY: usize = 40;
/// ROC horizons in seconds: ten seconds, one minute, five minutes.
const ROC_HORIZONS_SECONDS: [u64; 3] = [10, 60, 300];
/// Z-score windows in seconds: one, five and fifteen minutes.
const ZSCORE_WINDOWS_SECONDS: [u64; 3] = [60, 300, 900];
/// How many settled outcomes the streak publishes as `last_n_outcomes`.
const STREAK_RECENT_OUTCOMES: usize = 8;
/// Settled windows needed before the streak publishes.
const STREAK_READY_WINDOWS: u32 = 2;
/// The instance-status reason a prediction identity carries while its own
/// rolling history is still warming inside the bound market's generation.
const PREDICTION_WARMING_REASON: &str = "market_contract_warming_up";
/// The E51 append: a complete window with zero dispersion has no z-score.
const FLAT_WINDOW_REASON: &str = "flat_window";

// Adopted 2026-09-01 from the candidate set: fee-adjusted pair cost, venue
// repricing lag, and tie-rule context, symmetric across both venues. The
// computations live in `candidate_prediction_signals`; the identities are
// re-exported here so this module owns one identity vocabulary.
pub use crate::candidate_prediction_signals::{
    KALSHI_PAIR_COST_STATE_ID, KALSHI_TIE_RULE_CONTEXT_ID, KALSHI_VENUE_REPRICING_LAG_ID,
    POLYMARKET_PAIR_COST_STATE_ID, POLYMARKET_TIE_RULE_CONTEXT_ID,
    POLYMARKET_VENUE_REPRICING_LAG_ID,
};

// Adopted 2026-09-02 from the candidate set (ADR 0023 amendment), Polymarket
// only: the settlement-anchor state and the oracle divergence re-homed from
// `btc.external.oracle_divergence`. The Kalshi variants of both were deferred
// on rights-gated CF Benchmarks BRTI access; they now publish, but only while
// the Kalshi lane's index socket is armed (ruling E43), so the two venues
// publish the same number of identities only when Kalshi's CF socket is on.
pub use crate::candidate_settlement_signals::{
    KALSHI_ORACLE_DIVERGENCE_ID, KALSHI_SETTLEMENT_ANCHOR_STATE_ID,
    POLYMARKET_ORACLE_DIVERGENCE_ID, POLYMARKET_SETTLEMENT_ANCHOR_STATE_ID,
};

const POLYMARKET_CONTRACT_SOURCE_ID: &str = "polymarket.btc.market-contracts";
const POLYMARKET_QUOTE_SOURCE_ID: &str = "polymarket.btc.quotes";
const POLYMARKET_BOOK_SOURCE_ID: &str = "polymarket.btc.book-levels";
const POLYMARKET_TRADE_SOURCE_ID: &str = "polymarket.btc.trades";
const POLYMARKET_TWAP_SOURCE_ID: &str = "polymarket.rtds.btc-usd.twap-60s";
const KALSHI_CONTRACT_SOURCE_ID: &str = "kalshi.btc.market-contracts";
const KALSHI_QUOTE_SOURCE_ID: &str = "kalshi.btc.quotes";
const KALSHI_BOOK_SOURCE_ID: &str = "kalshi.btc.book-levels";
const KALSHI_TRADE_SOURCE_ID: &str = "kalshi.btc.trades";
/// The authenticated CF Benchmarks BRTI value channel, relayed on the Kalshi
/// market socket. Only in the Kalshi source set while the index socket is armed
/// (E43); its bytes are the adapter's own (`btc_feeds::kalshi_index`).
const KALSHI_BRTI_VALUE_SOURCE_ID: &str = btc_feeds::kalshi_index::VALUE_SOURCE_ID;
/// The CF Benchmarks BRTI settlement-window channel: the final-minute average
/// the KXBTC15M contract settles on, one running quarter at a time (E28/E37).
/// The settlement twins read its completeness (`window_as_of == quarter_close`);
/// the value channel above supplies the anchor price. Armed with the value
/// source.
const KALSHI_BRTI_SETTLEMENT_SOURCE_ID: &str = btc_feeds::kalshi_index::SETTLEMENT_WINDOW_SOURCE_ID;
/// The global-scope CF Benchmarks BRTI oracle-reference identity (E43/E36).
pub const CFBRTI_ORACLE_REFERENCE_ID: &str = "btc.cfbrti.oracle_reference";
/// The declared full BRTI trailing-average window: sixty once-per-second prints.
/// A shorter `avg_60s_window_size` is a not-yet-full settlement anchor (§5.2).
const CFBRTI_FULL_WINDOW_SIZE: u32 = 60;
/// How long, in lane-time nanoseconds, a departed Kalshi market's pending final
/// settlement decision is retained after a rollover whose quarter had not yet
/// completed (ruling E37c). The completing settlement-window frame for a
/// quarter can land after the scheduled rollover; within this window the twins
/// still publish their one final ready decision scoped to the departed market.
/// Past it the slot expires unpublished and is counted in feed health.
const PENDING_FINAL_DECISION_RETENTION_NS: u64 = 5_000_000_000;
/// The CF Benchmarks BRTI oracle-reference staleness threshold, in milliseconds
/// (§5.1 row): a reading older than this publishes `freshness: stale`.
const CFBRTI_STALENESS_THRESHOLD_MS: u64 = 5000;
/// The CF Benchmarks BRTI staleness bound (`staleness_bound_ms` in the
/// definition): an anchor whose newest print is older than this is not a
/// settlement anchor, so the settlement twins degrade `cfbrti_stale` (§6). The
/// oracle reference itself keeps publishing `freshness: stale` — staleness is a
/// value there, not a status — so the bound gates only the twins.
const CFBRTI_STALENESS_BOUND_MS: i64 = 10_000;
/// The 15-minute grid the KXBTC15M settlement window closes on: a derived
/// `quarter_close_time` that is not a multiple of this is not a quarter close
/// (E28/E37), so the settlement-window observation is refused.
const QUARTER_HOUR_MILLIS: i64 = 900_000;
const SOURCE_VERSION: &str = "1.0.0";

const OUTPUT_DECIMAL_PLACES: u32 = 8;
const MAX_BOOK_LEVELS_PER_SIDE: usize = 100_000;
const MAX_PENDING_BOOK_ROWS: usize = 100_000;
const MAX_FLOW_EVENTS: usize = 1_000_000;
const MAX_RETAINED_TWAP_OBSERVATIONS: usize = 1_800;

const POLYMARKET_MARKET_CONTRACT_DEFINITION: &str =
    include_str!("../../../definitions/btc/signals/btc.polymarket.market_contract@1.0.0.json");
const POLYMARKET_PREDICTION_QUOTE_DEFINITION: &str =
    include_str!("../../../definitions/btc/signals/btc.polymarket.prediction_quote@1.0.0.json");
const POLYMARKET_PREDICTION_LIQUIDITY_DEFINITION: &str =
    include_str!("../../../definitions/btc/signals/btc.polymarket.prediction_liquidity@1.0.0.json");
const POLYMARKET_PREDICTION_TRADE_FLOW_DEFINITION: &str = include_str!(
    "../../../definitions/btc/signals/btc.polymarket.prediction_trade_flow@1.0.0.json"
);
const KALSHI_MARKET_CONTRACT_DEFINITION: &str =
    include_str!("../../../definitions/btc/signals/btc.kalshi.market_contract@1.0.0.json");
const KALSHI_PREDICTION_QUOTE_DEFINITION: &str =
    include_str!("../../../definitions/btc/signals/btc.kalshi.prediction_quote@1.0.0.json");
const KALSHI_PREDICTION_LIQUIDITY_DEFINITION: &str =
    include_str!("../../../definitions/btc/signals/btc.kalshi.prediction_liquidity@1.0.0.json");
const KALSHI_PREDICTION_TRADE_FLOW_DEFINITION: &str =
    include_str!("../../../definitions/btc/signals/btc.kalshi.prediction_trade_flow@1.0.0.json");

const POLYMARKET_WINDOW_DELTA_DEFINITION: &str =
    include_str!("../../../definitions/btc/signals/btc.polymarket.window_delta@1.0.0.json");
const POLYMARKET_STANDARDIZED_MONEYNESS_DEFINITION: &str = include_str!(
    "../../../definitions/btc/signals/btc.polymarket.standardized_moneyness@1.0.0.json"
);
const POLYMARKET_FAIR_UP_PROBABILITY_DEFINITION: &str =
    include_str!("../../../definitions/btc/signals/btc.polymarket.fair_up_probability@1.0.0.json");
const POLYMARKET_EXECUTABLE_EDGE_DEFINITION: &str =
    include_str!("../../../definitions/btc/signals/btc.polymarket.executable_edge@1.0.0.json");
const KALSHI_WINDOW_DELTA_DEFINITION: &str =
    include_str!("../../../definitions/btc/signals/btc.kalshi.window_delta@1.0.0.json");
const KALSHI_STANDARDIZED_MONEYNESS_DEFINITION: &str =
    include_str!("../../../definitions/btc/signals/btc.kalshi.standardized_moneyness@1.0.0.json");
const KALSHI_FAIR_UP_PROBABILITY_DEFINITION: &str =
    include_str!("../../../definitions/btc/signals/btc.kalshi.fair_up_probability@1.0.0.json");
const POLYMARKET_RSI_DEFINITION: &str =
    include_str!("../../../definitions/btc/signals/btc.polymarket.rsi@1.0.0.json");
const POLYMARKET_ROC_DEFINITION: &str =
    include_str!("../../../definitions/btc/signals/btc.polymarket.roc@1.0.0.json");
const POLYMARKET_ZSCORE_DEFINITION: &str =
    include_str!("../../../definitions/btc/signals/btc.polymarket.zscore@1.0.0.json");
const POLYMARKET_TICK_VELOCITY_DEFINITION: &str =
    include_str!("../../../definitions/btc/signals/btc.polymarket.tick_velocity@1.0.0.json");
const POLYMARKET_EXPECTED_VALUE_DEFINITION: &str =
    include_str!("../../../definitions/btc/signals/btc.polymarket.expected_value@1.0.0.json");
const KALSHI_RSI_DEFINITION: &str =
    include_str!("../../../definitions/btc/signals/btc.kalshi.rsi@1.0.0.json");
const KALSHI_ROC_DEFINITION: &str =
    include_str!("../../../definitions/btc/signals/btc.kalshi.roc@1.0.0.json");
const KALSHI_ZSCORE_DEFINITION: &str =
    include_str!("../../../definitions/btc/signals/btc.kalshi.zscore@1.0.0.json");
const KALSHI_TICK_VELOCITY_DEFINITION: &str =
    include_str!("../../../definitions/btc/signals/btc.kalshi.tick_velocity@1.0.0.json");
const KALSHI_EXPECTED_VALUE_DEFINITION: &str =
    include_str!("../../../definitions/btc/signals/btc.kalshi.expected_value@1.0.0.json");
const KALSHI_WINDOW_STREAK_DEFINITION: &str =
    include_str!("../../../definitions/btc/signals/btc.kalshi.window_streak@1.0.0.json");
const KALSHI_EXECUTABLE_EDGE_DEFINITION: &str =
    include_str!("../../../definitions/btc/signals/btc.kalshi.executable_edge@1.0.0.json");
const POLYMARKET_PROBABILITY_MOMENTUM_DEFINITION: &str =
    include_str!("../../../definitions/btc/signals/btc.polymarket.probability_momentum@1.0.0.json");
const POLYMARKET_PROBABILITY_VOLATILITY_DEFINITION: &str = include_str!(
    "../../../definitions/btc/signals/btc.polymarket.probability_volatility@1.0.0.json"
);
const KALSHI_PROBABILITY_MOMENTUM_DEFINITION: &str =
    include_str!("../../../definitions/btc/signals/btc.kalshi.probability_momentum@1.0.0.json");
const KALSHI_PROBABILITY_VOLATILITY_DEFINITION: &str =
    include_str!("../../../definitions/btc/signals/btc.kalshi.probability_volatility@1.0.0.json");

const POLYMARKET_PAIR_COST_STATE_DEFINITION: &str =
    include_str!("../../../definitions/btc/signals/btc.polymarket.pair_cost_state@1.0.0.json");
const POLYMARKET_VENUE_REPRICING_LAG_DEFINITION: &str =
    include_str!("../../../definitions/btc/signals/btc.polymarket.venue_repricing_lag@1.0.0.json");
const POLYMARKET_TIE_RULE_CONTEXT_DEFINITION: &str =
    include_str!("../../../definitions/btc/signals/btc.polymarket.tie_rule_context@1.0.0.json");
const KALSHI_PAIR_COST_STATE_DEFINITION: &str =
    include_str!("../../../definitions/btc/signals/btc.kalshi.pair_cost_state@1.0.0.json");
const KALSHI_VENUE_REPRICING_LAG_DEFINITION: &str =
    include_str!("../../../definitions/btc/signals/btc.kalshi.venue_repricing_lag@1.0.0.json");
const KALSHI_TIE_RULE_CONTEXT_DEFINITION: &str =
    include_str!("../../../definitions/btc/signals/btc.kalshi.tie_rule_context@1.0.0.json");

const POLYMARKET_SETTLEMENT_ANCHOR_STATE_DEFINITION: &str = include_str!(
    "../../../definitions/btc/signals/btc.polymarket.settlement_anchor_state@1.0.0.json"
);
const POLYMARKET_ORACLE_DIVERGENCE_DEFINITION: &str =
    include_str!("../../../definitions/btc/signals/btc.polymarket.oracle_divergence@1.0.0.json");
const KALSHI_SETTLEMENT_ANCHOR_STATE_DEFINITION: &str =
    include_str!("../../../definitions/btc/signals/btc.kalshi.settlement_anchor_state@1.0.0.json");
const KALSHI_ORACLE_DIVERGENCE_DEFINITION: &str =
    include_str!("../../../definitions/btc/signals/btc.kalshi.oracle_divergence@1.0.0.json");

// The global-scope CF Benchmarks BRTI oracle reference and the value source it
// reads. Both ride the Kalshi lane only while its index socket is armed (E43).
const CFBRTI_ORACLE_REFERENCE_DEFINITION: &str =
    include_str!("../../../definitions/btc/signals/btc.cfbrti.oracle_reference@1.0.0.json");
const KALSHI_BRTI_VALUE_SOURCE_DEFINITION: &str =
    include_str!("../../../definitions/btc/sources/kalshi.cfbenchmarks.brti.value@1.0.0.json");
const KALSHI_BRTI_SETTLEMENT_SOURCE_DEFINITION: &str = include_str!(
    "../../../definitions/btc/sources/kalshi.cfbenchmarks.brti.settlement-window@1.0.0.json"
);

// The two ticker source contracts consumed by the embedded spot reference. They
// are pinned at their own published versions (Coinbase @1.0.0, Kraken @1.0.0) and
// enter the composite contract fingerprint without joining `source_documents`
// (which requires the uniform 1.0.0 venue-source version).
const COINBASE_TICKER_SOURCE_DEFINITION: &str =
    include_str!("../../../definitions/btc/sources/coinbase.exchange.btc-usd.ticker@1.0.0.json");
const KRAKEN_TICKER_SOURCE_DEFINITION: &str =
    include_str!("../../../definitions/btc/sources/kraken.spot.btc-usd.ticker@1.0.0.json");

const POLYMARKET_CONTRACT_SOURCE_DEFINITION: &str =
    include_str!("../../../definitions/btc/sources/polymarket.btc.market-contracts@1.0.0.json");
const POLYMARKET_QUOTE_SOURCE_DEFINITION: &str =
    include_str!("../../../definitions/btc/sources/polymarket.btc.quotes@1.0.0.json");
const POLYMARKET_BOOK_SOURCE_DEFINITION: &str =
    include_str!("../../../definitions/btc/sources/polymarket.btc.book-levels@1.0.0.json");
const POLYMARKET_TRADE_SOURCE_DEFINITION: &str =
    include_str!("../../../definitions/btc/sources/polymarket.btc.trades@1.0.0.json");
const POLYMARKET_TWAP_SOURCE_DEFINITION: &str =
    include_str!("../../../definitions/btc/sources/polymarket.rtds.btc-usd.twap-60s@1.0.0.json");
const KALSHI_CONTRACT_SOURCE_DEFINITION: &str =
    include_str!("../../../definitions/btc/sources/kalshi.btc.market-contracts@1.0.0.json");
const KALSHI_QUOTE_SOURCE_DEFINITION: &str =
    include_str!("../../../definitions/btc/sources/kalshi.btc.quotes@1.0.0.json");
const KALSHI_BOOK_SOURCE_DEFINITION: &str =
    include_str!("../../../definitions/btc/sources/kalshi.btc.book-levels@1.0.0.json");
const KALSHI_TRADE_SOURCE_DEFINITION: &str =
    include_str!("../../../definitions/btc/sources/kalshi.btc.trades@1.0.0.json");

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum PredictionSignalKind {
    MarketContract,
    PredictionQuote,
    PredictionLiquidity,
    PredictionTradeFlow,
    WindowDelta,
    StandardizedMoneyness,
    FairUpProbability,
    ExecutableEdge,
    PairCostState,
    VenueRepricingLag,
    TieRuleContext,
    // C3b (ruling E51). Publication order matches the table below and the batch
    // assembled by `emit_all_with_base_reason`.
    Rsi,
    Roc,
    ZScore,
    TickVelocity,
    ExpectedValue,
    SettlementAnchorState,
    OracleDivergence,
    /// Kalshi only, and index-gated with the settlement twins it folds.
    WindowStreak,
    ProbabilityMomentum,
    ProbabilityVolatility,
}

impl PredictionSignalKind {
    /// Every kind, in publication order. A venue's expected set is this table
    /// filtered by [`Self::applies_to`].
    const ALL: [Self; PREDICTION_SIGNAL_KIND_COUNT] = [
        Self::MarketContract,
        Self::PredictionQuote,
        Self::PredictionLiquidity,
        Self::PredictionTradeFlow,
        Self::WindowDelta,
        Self::StandardizedMoneyness,
        Self::FairUpProbability,
        Self::ExecutableEdge,
        Self::PairCostState,
        Self::VenueRepricingLag,
        Self::TieRuleContext,
        Self::Rsi,
        Self::Roc,
        Self::ZScore,
        Self::TickVelocity,
        Self::ExpectedValue,
        Self::SettlementAnchorState,
        Self::OracleDivergence,
        Self::WindowStreak,
        Self::ProbabilityMomentum,
        Self::ProbabilityVolatility,
    ];

    /// This venue's expected set, in table order, with the index socket
    /// disarmed — the always-on set. `expected_kinds` is the arming-aware form.
    fn for_venue(venue: PredictionMarketVenue) -> impl Iterator<Item = Self> {
        Self::expected_kinds(venue, false)
    }

    /// This venue's expected set for the given index-socket arming, in table
    /// order. The index-gated kinds (the Kalshi settlement twins) join the set
    /// only when the socket is armed (E43); every other kind is always present
    /// exactly as `base_applies_to` decides.
    fn expected_kinds(
        venue: PredictionMarketVenue,
        index_armed: bool,
    ) -> impl Iterator<Item = Self> {
        Self::ALL.into_iter().filter(move |kind| {
            kind.base_applies_to(venue) || (index_armed && kind.index_gated(venue))
        })
    }

    /// Whether this kind is in `venue`'s always-on set: the identity table,
    /// minus the index-gated kinds, which are decided by `index_gated`. Derived
    /// so there is exactly one place a venue's base set is decided.
    const fn base_applies_to(self, venue: PredictionMarketVenue) -> bool {
        !self.index_gated(venue) && self.id(venue).is_some()
    }

    /// Whether this kind publishes on `venue` only while the index socket is
    /// armed. Exactly the two Kalshi settlement twins (E43); every other kind,
    /// and both twins on Polymarket, are never index-gated.
    const fn index_gated(self, venue: PredictionMarketVenue) -> bool {
        matches!(
            (venue, self),
            (
                PredictionMarketVenue::Kalshi,
                // The streak folds exactly the final decisions the two twins
                // publish, so it is armed with them: a disarmed lane has no
                // settled history to read and would publish a never-ready
                // identity.
                Self::SettlementAnchorState | Self::OracleDivergence | Self::WindowStreak,
            )
        )
    }

    /// The published identity for this kind on `venue`, or `None` when the kind
    /// is outside that venue's expected set.
    const fn id(self, venue: PredictionMarketVenue) -> Option<&'static str> {
        match (venue, self) {
            (PredictionMarketVenue::Polymarket, Self::MarketContract) => {
                Some(POLYMARKET_MARKET_CONTRACT_ID)
            }
            (PredictionMarketVenue::Polymarket, Self::PredictionQuote) => {
                Some(POLYMARKET_PREDICTION_QUOTE_ID)
            }
            (PredictionMarketVenue::Polymarket, Self::PredictionLiquidity) => {
                Some(POLYMARKET_PREDICTION_LIQUIDITY_ID)
            }
            (PredictionMarketVenue::Polymarket, Self::PredictionTradeFlow) => {
                Some(POLYMARKET_PREDICTION_TRADE_FLOW_ID)
            }
            (PredictionMarketVenue::Polymarket, Self::WindowDelta) => {
                Some(POLYMARKET_WINDOW_DELTA_ID)
            }
            (PredictionMarketVenue::Polymarket, Self::StandardizedMoneyness) => {
                Some(POLYMARKET_STANDARDIZED_MONEYNESS_ID)
            }
            (PredictionMarketVenue::Polymarket, Self::FairUpProbability) => {
                Some(POLYMARKET_FAIR_UP_PROBABILITY_ID)
            }
            (PredictionMarketVenue::Polymarket, Self::ExecutableEdge) => {
                Some(POLYMARKET_EXECUTABLE_EDGE_ID)
            }
            (PredictionMarketVenue::Kalshi, Self::MarketContract) => {
                Some(KALSHI_MARKET_CONTRACT_ID)
            }
            (PredictionMarketVenue::Kalshi, Self::PredictionQuote) => {
                Some(KALSHI_PREDICTION_QUOTE_ID)
            }
            (PredictionMarketVenue::Kalshi, Self::PredictionLiquidity) => {
                Some(KALSHI_PREDICTION_LIQUIDITY_ID)
            }
            (PredictionMarketVenue::Kalshi, Self::PredictionTradeFlow) => {
                Some(KALSHI_PREDICTION_TRADE_FLOW_ID)
            }
            (PredictionMarketVenue::Kalshi, Self::WindowDelta) => Some(KALSHI_WINDOW_DELTA_ID),
            (PredictionMarketVenue::Kalshi, Self::StandardizedMoneyness) => {
                Some(KALSHI_STANDARDIZED_MONEYNESS_ID)
            }
            (PredictionMarketVenue::Kalshi, Self::FairUpProbability) => {
                Some(KALSHI_FAIR_UP_PROBABILITY_ID)
            }
            (PredictionMarketVenue::Kalshi, Self::ExecutableEdge) => {
                Some(KALSHI_EXECUTABLE_EDGE_ID)
            }
            (PredictionMarketVenue::Polymarket, Self::PairCostState) => {
                Some(POLYMARKET_PAIR_COST_STATE_ID)
            }
            (PredictionMarketVenue::Polymarket, Self::VenueRepricingLag) => {
                Some(POLYMARKET_VENUE_REPRICING_LAG_ID)
            }
            (PredictionMarketVenue::Polymarket, Self::TieRuleContext) => {
                Some(POLYMARKET_TIE_RULE_CONTEXT_ID)
            }
            (PredictionMarketVenue::Kalshi, Self::PairCostState) => Some(KALSHI_PAIR_COST_STATE_ID),
            (PredictionMarketVenue::Kalshi, Self::VenueRepricingLag) => {
                Some(KALSHI_VENUE_REPRICING_LAG_ID)
            }
            (PredictionMarketVenue::Kalshi, Self::TieRuleContext) => {
                Some(KALSHI_TIE_RULE_CONTEXT_ID)
            }
            (PredictionMarketVenue::Polymarket, Self::SettlementAnchorState) => {
                Some(POLYMARKET_SETTLEMENT_ANCHOR_STATE_ID)
            }
            (PredictionMarketVenue::Polymarket, Self::OracleDivergence) => {
                Some(POLYMARKET_ORACLE_DIVERGENCE_ID)
            }
            // The Kalshi settlement twins resolve on the CF Benchmarks BRTI
            // anchor. They own an identity, but publish only while the index
            // socket is armed (E43); membership in the armed set is decided by
            // `index_gated`, not by this table.
            (PredictionMarketVenue::Kalshi, Self::SettlementAnchorState) => {
                Some(KALSHI_SETTLEMENT_ANCHOR_STATE_ID)
            }
            (PredictionMarketVenue::Kalshi, Self::OracleDivergence) => {
                Some(KALSHI_ORACLE_DIVERGENCE_ID)
            }
            (PredictionMarketVenue::Polymarket, Self::ProbabilityMomentum) => {
                Some(POLYMARKET_PROBABILITY_MOMENTUM_ID)
            }
            (PredictionMarketVenue::Polymarket, Self::ProbabilityVolatility) => {
                Some(POLYMARKET_PROBABILITY_VOLATILITY_ID)
            }
            (PredictionMarketVenue::Kalshi, Self::ProbabilityMomentum) => {
                Some(KALSHI_PROBABILITY_MOMENTUM_ID)
            }
            (PredictionMarketVenue::Kalshi, Self::ProbabilityVolatility) => {
                Some(KALSHI_PROBABILITY_VOLATILITY_ID)
            }
            (PredictionMarketVenue::Polymarket, Self::Rsi) => Some(POLYMARKET_RSI_ID),
            (PredictionMarketVenue::Polymarket, Self::Roc) => Some(POLYMARKET_ROC_ID),
            (PredictionMarketVenue::Polymarket, Self::ZScore) => Some(POLYMARKET_ZSCORE_ID),
            (PredictionMarketVenue::Polymarket, Self::TickVelocity) => {
                Some(POLYMARKET_TICK_VELOCITY_ID)
            }
            (PredictionMarketVenue::Polymarket, Self::ExpectedValue) => {
                Some(POLYMARKET_EXPECTED_VALUE_ID)
            }
            (PredictionMarketVenue::Kalshi, Self::Rsi) => Some(KALSHI_RSI_ID),
            (PredictionMarketVenue::Kalshi, Self::Roc) => Some(KALSHI_ROC_ID),
            (PredictionMarketVenue::Kalshi, Self::ZScore) => Some(KALSHI_ZSCORE_ID),
            (PredictionMarketVenue::Kalshi, Self::TickVelocity) => Some(KALSHI_TICK_VELOCITY_ID),
            (PredictionMarketVenue::Kalshi, Self::ExpectedValue) => Some(KALSHI_EXPECTED_VALUE_ID),
            (PredictionMarketVenue::Kalshi, Self::WindowStreak) => Some(KALSHI_WINDOW_STREAK_ID),
            // No Polymarket sibling: the settled outcomes the streak folds are
            // the Kalshi settlement twins' final decisions (E37b), and
            // Polymarket resolves on the RTDS TWAP with no settlement-window
            // completion, so this lane holds no settled outcome for it.
            (PredictionMarketVenue::Polymarket, Self::WindowStreak) => None,
        }
    }

    const fn version(self) -> &'static str {
        PREDICTION_SIGNAL_VERSION
    }

    /// The embedded definition document for this kind on `venue`, or `None`
    /// when the kind is outside that venue's expected set.
    const fn raw_definition(self, venue: PredictionMarketVenue) -> Option<&'static str> {
        match (venue, self) {
            (PredictionMarketVenue::Polymarket, Self::MarketContract) => {
                Some(POLYMARKET_MARKET_CONTRACT_DEFINITION)
            }
            (PredictionMarketVenue::Polymarket, Self::PredictionQuote) => {
                Some(POLYMARKET_PREDICTION_QUOTE_DEFINITION)
            }
            (PredictionMarketVenue::Polymarket, Self::PredictionLiquidity) => {
                Some(POLYMARKET_PREDICTION_LIQUIDITY_DEFINITION)
            }
            (PredictionMarketVenue::Polymarket, Self::PredictionTradeFlow) => {
                Some(POLYMARKET_PREDICTION_TRADE_FLOW_DEFINITION)
            }
            (PredictionMarketVenue::Polymarket, Self::WindowDelta) => {
                Some(POLYMARKET_WINDOW_DELTA_DEFINITION)
            }
            (PredictionMarketVenue::Polymarket, Self::StandardizedMoneyness) => {
                Some(POLYMARKET_STANDARDIZED_MONEYNESS_DEFINITION)
            }
            (PredictionMarketVenue::Polymarket, Self::FairUpProbability) => {
                Some(POLYMARKET_FAIR_UP_PROBABILITY_DEFINITION)
            }
            (PredictionMarketVenue::Polymarket, Self::ExecutableEdge) => {
                Some(POLYMARKET_EXECUTABLE_EDGE_DEFINITION)
            }
            (PredictionMarketVenue::Kalshi, Self::MarketContract) => {
                Some(KALSHI_MARKET_CONTRACT_DEFINITION)
            }
            (PredictionMarketVenue::Kalshi, Self::PredictionQuote) => {
                Some(KALSHI_PREDICTION_QUOTE_DEFINITION)
            }
            (PredictionMarketVenue::Kalshi, Self::PredictionLiquidity) => {
                Some(KALSHI_PREDICTION_LIQUIDITY_DEFINITION)
            }
            (PredictionMarketVenue::Kalshi, Self::PredictionTradeFlow) => {
                Some(KALSHI_PREDICTION_TRADE_FLOW_DEFINITION)
            }
            (PredictionMarketVenue::Kalshi, Self::WindowDelta) => {
                Some(KALSHI_WINDOW_DELTA_DEFINITION)
            }
            (PredictionMarketVenue::Kalshi, Self::StandardizedMoneyness) => {
                Some(KALSHI_STANDARDIZED_MONEYNESS_DEFINITION)
            }
            (PredictionMarketVenue::Kalshi, Self::FairUpProbability) => {
                Some(KALSHI_FAIR_UP_PROBABILITY_DEFINITION)
            }
            (PredictionMarketVenue::Kalshi, Self::ExecutableEdge) => {
                Some(KALSHI_EXECUTABLE_EDGE_DEFINITION)
            }
            (PredictionMarketVenue::Polymarket, Self::PairCostState) => {
                Some(POLYMARKET_PAIR_COST_STATE_DEFINITION)
            }
            (PredictionMarketVenue::Polymarket, Self::VenueRepricingLag) => {
                Some(POLYMARKET_VENUE_REPRICING_LAG_DEFINITION)
            }
            (PredictionMarketVenue::Polymarket, Self::TieRuleContext) => {
                Some(POLYMARKET_TIE_RULE_CONTEXT_DEFINITION)
            }
            (PredictionMarketVenue::Kalshi, Self::PairCostState) => {
                Some(KALSHI_PAIR_COST_STATE_DEFINITION)
            }
            (PredictionMarketVenue::Kalshi, Self::VenueRepricingLag) => {
                Some(KALSHI_VENUE_REPRICING_LAG_DEFINITION)
            }
            (PredictionMarketVenue::Kalshi, Self::TieRuleContext) => {
                Some(KALSHI_TIE_RULE_CONTEXT_DEFINITION)
            }
            (PredictionMarketVenue::Polymarket, Self::SettlementAnchorState) => {
                Some(POLYMARKET_SETTLEMENT_ANCHOR_STATE_DEFINITION)
            }
            (PredictionMarketVenue::Polymarket, Self::OracleDivergence) => {
                Some(POLYMARKET_ORACLE_DIVERGENCE_DEFINITION)
            }
            (PredictionMarketVenue::Kalshi, Self::SettlementAnchorState) => {
                Some(KALSHI_SETTLEMENT_ANCHOR_STATE_DEFINITION)
            }
            (PredictionMarketVenue::Kalshi, Self::OracleDivergence) => {
                Some(KALSHI_ORACLE_DIVERGENCE_DEFINITION)
            }
            (PredictionMarketVenue::Polymarket, Self::ProbabilityMomentum) => {
                Some(POLYMARKET_PROBABILITY_MOMENTUM_DEFINITION)
            }
            (PredictionMarketVenue::Polymarket, Self::ProbabilityVolatility) => {
                Some(POLYMARKET_PROBABILITY_VOLATILITY_DEFINITION)
            }
            (PredictionMarketVenue::Kalshi, Self::ProbabilityMomentum) => {
                Some(KALSHI_PROBABILITY_MOMENTUM_DEFINITION)
            }
            (PredictionMarketVenue::Kalshi, Self::ProbabilityVolatility) => {
                Some(KALSHI_PROBABILITY_VOLATILITY_DEFINITION)
            }
            (PredictionMarketVenue::Polymarket, Self::Rsi) => Some(POLYMARKET_RSI_DEFINITION),
            (PredictionMarketVenue::Polymarket, Self::Roc) => Some(POLYMARKET_ROC_DEFINITION),
            (PredictionMarketVenue::Polymarket, Self::ZScore) => Some(POLYMARKET_ZSCORE_DEFINITION),
            (PredictionMarketVenue::Polymarket, Self::TickVelocity) => {
                Some(POLYMARKET_TICK_VELOCITY_DEFINITION)
            }
            (PredictionMarketVenue::Polymarket, Self::ExpectedValue) => {
                Some(POLYMARKET_EXPECTED_VALUE_DEFINITION)
            }
            (PredictionMarketVenue::Kalshi, Self::Rsi) => Some(KALSHI_RSI_DEFINITION),
            (PredictionMarketVenue::Kalshi, Self::Roc) => Some(KALSHI_ROC_DEFINITION),
            (PredictionMarketVenue::Kalshi, Self::ZScore) => Some(KALSHI_ZSCORE_DEFINITION),
            (PredictionMarketVenue::Kalshi, Self::TickVelocity) => {
                Some(KALSHI_TICK_VELOCITY_DEFINITION)
            }
            (PredictionMarketVenue::Kalshi, Self::ExpectedValue) => {
                Some(KALSHI_EXPECTED_VALUE_DEFINITION)
            }
            (PredictionMarketVenue::Kalshi, Self::WindowStreak) => {
                Some(KALSHI_WINDOW_STREAK_DEFINITION)
            }
            (PredictionMarketVenue::Polymarket, Self::WindowStreak) => None,
        }
    }
}

/// The last ready decision published for one instance of a signal identity: its
/// change key and the decision clock the publication happened on. An evaluation
/// that reproduces the change key is suppressed until the identity's declared
/// interval falls due, so an unchanged instance is refreshed rather than
/// restated on every admitted market event.
#[derive(Debug, Clone)]
struct PublishedDecision {
    change_key: BTreeMap<String, Value>,
    published_ns: u64,
}

/// One evaluated instance and whether the coalescing rule publishes it. A
/// suppressed evaluation is still returned so the publication boundary can
/// verify per-venue completeness, but it consumes no decision sequence and is
/// never written.
#[derive(Debug, Clone)]
pub struct PredictionEmission {
    update: SignalUpdate,
    published: bool,
}

/// One evaluation pass over a venue's expected identity set: every instance the
/// processor evaluated, in table order, paired with the publication decision.
///
/// The type derefs to the full evaluation slice because the evaluation set is
/// what callers inspect; `into_published` is the only way to reach the frames
/// that are actually written.
#[derive(Debug, Clone, Default)]
pub struct PredictionBatch {
    evaluations: Vec<SignalUpdate>,
    published: Vec<bool>,
}

impl PredictionBatch {
    fn from_emissions(emissions: Vec<PredictionEmission>) -> Self {
        let mut evaluations = Vec::with_capacity(emissions.len());
        let mut published = Vec::with_capacity(emissions.len());
        for emission in emissions {
            evaluations.push(emission.update);
            published.push(emission.published);
        }
        Self {
            evaluations,
            published,
        }
    }

    /// Every instance evaluated on this pass, suppressed ones included.
    #[must_use]
    pub fn evaluations(&self) -> &[SignalUpdate] {
        &self.evaluations
    }

    /// The number of instances this pass publishes.
    #[must_use]
    pub fn published_count(&self) -> usize {
        self.published
            .iter()
            .filter(|published| **published)
            .count()
    }

    /// Reshape a pass to an arbitrary evaluated size so the publication
    /// boundary's per-venue completeness guard can be exercised against a batch
    /// no processor would ever produce.
    #[cfg(test)]
    pub(crate) fn resized_for_completeness_test(mut self, len: usize) -> Self {
        let seed = self
            .evaluations
            .first()
            .cloned()
            .expect("a resized batch starts from a non-empty pass");
        self.evaluations.resize(len, seed);
        self.published.resize(len, true);
        self
    }

    /// The instances this pass publishes, in evaluation order. An all-suppressed
    /// pass yields nothing, and no batch is written for it.
    #[must_use]
    pub fn into_published(self) -> Vec<SignalUpdate> {
        self.evaluations
            .into_iter()
            .zip(self.published)
            .filter_map(|(update, published)| published.then_some(update))
            .collect()
    }
}

impl std::ops::Deref for PredictionBatch {
    type Target = [SignalUpdate];

    fn deref(&self) -> &Self::Target {
        &self.evaluations
    }
}

#[derive(Debug, Clone)]
struct PredictionEmitter {
    definition: SignalDefinition,
    next_sequence: u64,
    lifetime_output_count: u64,
    ever_ready: bool,
    last_quality: Option<Quality>,
    quality_reason_entries: BTreeMap<String, u64>,
    /// The declared output fields whose value carries a measurement rather than
    /// a clock reading — every field whose declared type is neither `timestamp`
    /// nor `duration_ms`. Derived once from the definition at construction, so
    /// the rule is a property of the contract and not of a field-name list that
    /// a new field could silently escape.
    change_key_fields: BTreeSet<String>,
    last_published: BTreeMap<SignalScope, PublishedDecision>,
}

impl PredictionEmitter {
    fn new(
        venue: PredictionMarketVenue,
        kind: PredictionSignalKind,
    ) -> Result<Self, PredictionSignalError> {
        // An emitter is only ever built for a kind in the venue's expected set,
        // so a missing identity or document here is a broken table, not a
        // reachable state.
        let (id, raw) = kind
            .id(venue)
            .zip(kind.raw_definition(venue))
            .ok_or_else(|| {
                PredictionSignalError::Definition(format!(
                    "{venue:?} has no embedded contract for {kind:?}"
                ))
            })?;
        let definition = parse_document::<SignalDefinition>(raw, id)?;
        if definition.signal_id != id || definition.version != kind.version() {
            return Err(PredictionSignalError::Definition(format!(
                "embedded definition identity {}@{} does not match {}@{}",
                definition.signal_id,
                definition.version,
                id,
                kind.version()
            )));
        }
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
            .collect::<BTreeSet<_>>();
        Ok(Self {
            definition,
            next_sequence: 0,
            lifetime_output_count: 0,
            ever_ready: false,
            last_quality: None,
            quality_reason_entries: BTreeMap::new(),
            change_key_fields,
            last_published: BTreeMap::new(),
        })
    }

    /// A restore always opens a new publisher epoch, so the decision sequence
    /// restarts at zero and the coalescing memory starts empty: the first
    /// evaluation after a resume republishes every instance, which is exactly
    /// what a consumer joining a new epoch has to be told.
    fn restore_history(&mut self, state: &PredictionFeatureCheckpoint) {
        self.next_sequence = 0;
        self.lifetime_output_count = state.lifetime_output_count;
        self.ever_ready = state.ever_ready;
        self.last_quality = state.last_quality.clone();
        self.quality_reason_entries = state.quality_reason_entries.clone();
        self.last_published.clear();
    }

    /// Forget the departed generation's published values. The instances are
    /// invalidated at the publication boundary and can never be published
    /// again, so retaining them would only grow unbounded across rollovers.
    fn forget_published(&mut self) {
        self.last_published.clear();
    }

    /// Restart the per-generation output sequence at zero for a new file
    /// generation (C5), keeping the warm feature state. Mirrors the sequence
    /// reset [`Self::restore_history`] performs on a resume, so a rotation-opened
    /// generation's first ready decision is sequence zero exactly as an
    /// independent replay of that generation reproduces it.
    fn reset_output_sequence(&mut self) {
        self.next_sequence = 0;
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "the emitter validates each independent wire-contract component at one boundary"
    )]
    fn emit(
        &mut self,
        publisher_epoch: &str,
        produced_at: &str,
        now_ns: u64,
        event_time: String,
        ready_values: Option<BTreeMap<String, Value>>,
        mut reasons: Vec<String>,
        force_degraded: bool,
        scope: SignalScope,
    ) -> Result<PredictionEmission, PredictionSignalError> {
        reasons.sort();
        reasons.dedup();
        let ready = ready_values.is_some();
        // A non-ready evaluation is never coalesced: instance_status keeps its
        // own revision-and-periodic logic at the publication boundary.
        let change_key = ready_values.as_ref().map(|values| self.change_key(values));
        let published = match &change_key {
            Some(change_key) => self.publishes(&scope, change_key, now_ns)?,
            None => true,
        };
        let quality = if ready {
            Quality {
                state: QualityState::Ready,
                reasons: Vec::new(),
            }
        } else {
            if reasons.is_empty() {
                return Err(PredictionSignalError::State(
                    "non-ready prediction output requires a reason".to_owned(),
                ));
            }
            Quality {
                state: if force_degraded || self.ever_ready {
                    QualityState::Degraded
                } else {
                    QualityState::WarmingUp
                },
                reasons,
            }
        };
        let (valid_from, valid_until) =
            SignalUpdate::validity_window(produced_at, self.definition.staleness_bound_ms)?;
        let update = SignalUpdate {
            schema_version: SIGNAL_UPDATE_SCHEMA_VERSION,
            signal_id: self.definition.signal_id.clone(),
            signal_version: self.definition.version.clone(),
            publisher_epoch: publisher_epoch.to_owned(),
            sequence: self.next_sequence,
            event_time,
            produced_at: produced_at.to_owned(),
            valid_from,
            valid_until,
            quality: quality.clone(),
            values: ready_values.unwrap_or_default(),
            scope,
        };
        update.validate_against(&self.definition)?;

        let prior: BTreeSet<&str> = self
            .last_quality
            .as_ref()
            .into_iter()
            .flat_map(|value| value.reasons.iter().map(String::as_str))
            .collect();
        for reason in &quality.reasons {
            if !prior.contains(reason.as_str()) {
                let count = self
                    .quality_reason_entries
                    .entry(reason.clone())
                    .or_default();
                *count = count.checked_add(1).ok_or(PredictionSignalError::Counter)?;
            }
        }
        self.ever_ready |= ready;
        self.last_quality = Some(quality);
        // The decision sequence counts published decisions only, so a suppressed
        // evaluation leaves it where it is and the next published decision is
        // still contiguous.
        if let Some(change_key) = change_key.filter(|_| published) {
            self.last_published.insert(
                update.scope.clone(),
                PublishedDecision {
                    change_key,
                    published_ns: now_ns,
                },
            );
            self.next_sequence = self
                .next_sequence
                .checked_add(1)
                .ok_or(PredictionSignalError::Counter)?;
        } else if !ready {
            // A non-ready evaluation ends the published decision's standing: the
            // consumer now holds a status beside it. The recovery decision is
            // therefore new information even when it restates the same change
            // key, so the memory is dropped rather than allowed to suppress a
            // sub-interval degraded -> ready recovery.
            self.last_published.remove(&update.scope);
        }
        self.lifetime_output_count = self
            .lifetime_output_count
            .checked_add(1)
            .ok_or(PredictionSignalError::Counter)?;
        Ok(PredictionEmission { update, published })
    }

    /// This evaluation's change key: the `values` map restricted to the declared
    /// non-clock fields. Every declared field still rides on the published
    /// update with its own fresh value — the restriction decides *when* to
    /// publish, never *what* to publish.
    fn change_key(&self, values: &BTreeMap<String, Value>) -> BTreeMap<String, Value> {
        values
            .iter()
            .filter(|(name, _)| self.change_key_fields.contains(name.as_str()))
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect()
    }

    /// The coalescing rule for one ready instance: publish it when the instance
    /// has not been published in this epoch, when its change key differs from
    /// the last published one, or when its declared interval has fallen due
    /// since that publication. Every input is journaled state or the journaled
    /// decision clock, so a replay reproduces the decision exactly.
    ///
    /// Change keys are compared as decoded JSON objects. Decimals cross this
    /// boundary as strings, so object equality is exactly text equality of the
    /// serialized object. Clock-derived fields — declared `timestamp` or
    /// `duration_ms` — are outside the key: an age or a lag that only advances
    /// because the decision clock advanced is not a new measurement, and the
    /// interval refresh bounds how stale such a reading can get to one interval.
    fn publishes(
        &self,
        scope: &SignalScope,
        change_key: &BTreeMap<String, Value>,
        now_ns: u64,
    ) -> Result<bool, PredictionSignalError> {
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
            .ok_or(PredictionSignalError::Counter)?;
        Ok(now_ns.saturating_sub(last.published_ns) >= interval_ns)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PredictionFeatureCheckpoint {
    pub lifetime_output_count: u64,
    pub ever_ready: bool,
    pub last_quality: Option<Quality>,
    pub quality_reason_entries: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PredictionSignalCheckpointState {
    pub venue: PredictionMarketVenue,
    /// The withheld Kalshi settlement window, if one is outstanding.
    ///
    /// A settlement-window pause is recovery state that outlives the process
    /// (audit A1/F4): a restart that rebuilt an empty processor would admit the
    /// next quarter print and publish an average over a window with lost prints
    /// — the exact "never a partial average" property the design exists to
    /// guarantee. Checkpointing it is what makes a restart mid-recovery resume
    /// paused.
    ///
    /// Omitted from the serialized form when absent, so a state with no pause is
    /// byte-identical to the pre-v6 shape and a v5 checkpoint still matches its
    /// recorded digest (ruling N2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settlement_window_pause: Option<SettlementWindowPause>,
    /// The derived `quarter_close_time` (Unix ms) of the last settlement window
    /// whose one final ready decision was published (ruling E37b), so a resume
    /// after the close does not re-publish it. Added in checkpoint schema v7.
    ///
    /// Omitted from the serialized form when absent, so a state that has not yet
    /// reached a close is byte-identical to the pre-v7 shape and a v5/v6
    /// checkpoint still matches its recorded digest (ruling N2). A restored
    /// `None` — from an older checkpoint or before the first close — chooses not
    /// to re-publish: older checkpoints predate the feature and recorded no final
    /// decision, so there is nothing to replay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settlement_final_close_ms: Option<i64>,
    /// A departed Kalshi market's pending final settlement decision (ruling
    /// E37c): retained across a rollover whose quarter had not yet completed, so
    /// a restart in the gap neither re-publishes a released decision nor loses an
    /// unreleased one. Cleared the instant the decision publishes (the
    /// high-water `settlement_final_close_ms` then guards against re-publish) or
    /// the retention deadline lapses. Added in checkpoint schema v8.
    ///
    /// Omitted from the serialized form when absent, so a state with no pending
    /// decision is byte-identical to the pre-v8 shape and a v5/v6/v7 checkpoint
    /// still matches its recorded digest (ruling N2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_final_decision: Option<PendingFinalDecisionState>,
    /// The settled-window streak (C3 row 10, ruling E51), so a restart resumes
    /// the run instead of re-warming. Additive and optional: the schema version
    /// is unchanged, a lane that has seen no settled window serializes exactly
    /// as before, and a checkpoint written before this identity existed restores
    /// as an empty history (ruling N2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_streak: Option<WindowStreakCheckpointState>,
    pub last_market_generation: u64,
    pub active_market_id: Option<String>,
    pub features: BTreeMap<String, PredictionFeatureCheckpoint>,
    pub last_now_ns: Option<u64>,
    /// Embedded state of the internally hosted spot reference (ADR 0016). The
    /// realized-volatility sample ring is deliberately not checkpointed: it is
    /// rolling market-relative state that re-warms after a restore.
    pub spot_reference: SpotReferenceCheckpointState,
    /// The active market binding, retained so a rotated file generation's journal
    /// (whose session carries an `admission_epoch`) can rebuild `active` on
    /// replay without a fresh Bind (C5 in-process rotation). Additive and
    /// optional: the schema version is unchanged, an unbound state serializes
    /// exactly as before, and a checkpoint written before this field existed —
    /// or read on the live `--resume-from-checkpoint` crash-recovery path —
    /// restores as `None`, keeping today's re-discover-and-rebind behaviour.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_binding: Option<ActiveBindingCheckpoint>,
}

/// The active market binding carried across an in-process file-generation
/// rotation (C5): the raw contract event and its generation, from which
/// [`PredictionSignalProcessor`] re-parses the same `ActiveContract` on a rotated
/// generation's replay. Restored only on that gap-free path; the live crash
/// resume ignores it and re-discovers, since the binding may be stale.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActiveBindingCheckpoint {
    /// The bound market generation (`ActiveContract::generation`).
    pub generation: u64,
    /// The contract source event this binding was parsed from.
    pub contract_event: SourceEvent,
}

/// A departed Kalshi market's frozen final settlement decision (ruling E37c).
///
/// Filled at the rollover — before `clear_market_state` wipes the departing
/// market's inputs — from the state as of the close: the settled anchor price,
/// the rules-card threshold/comparator/tie, and the composite spot-reference mid
/// the divergence twin needs. When the completing settlement-window frame for
/// this quarter arrives (up to the retention deadline), both twins publish one
/// final ready decision scoped to THIS departed market with
/// `settlement_window_complete: true`; otherwise the slot expires unpublished.
///
/// Decimals are stored as full-precision text so a checkpoint round-trip is
/// exact and the published field applies the same output rounding a live E37b
/// decision does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingFinalDecisionState {
    /// The departed market the final decision is scoped to.
    pub market_id: String,
    /// The departed market's discovery generation.
    pub market_generation: u64,
    /// The derived quarter close verbatim, republished as `quarter_close_time`
    /// and `window_as_of` on the final decision.
    pub quarter_close_time: String,
    /// The derived quarter close in Unix milliseconds, matched against an
    /// arriving completing frame and folded into the high-water mark on release.
    pub quarter_close_ms: i64,
    /// Lane-time nanosecond deadline past which the slot expires unpublished.
    pub deadline_ns: u64,
    /// The settled anchor price as of the close (decimal text).
    pub anchor_price: String,
    /// The rules-card threshold (decimal text).
    pub threshold: String,
    /// The composite spot-reference mid frozen at the close (decimal text).
    pub spot_reference_mid: String,
    /// The rules-card comparator, canonical wire string form.
    pub comparator: String,
    /// The rules-card tie outcome in the venue's own vocabulary, decoded on
    /// release through the same `settled_tie_side` helper a live decision uses.
    pub tie_outcome: String,
}

/// A withheld Kalshi settlement window and the repair that would release it.
///
/// Mirrors `btc_feeds::kalshi_index::PendingSettlementBackfill`: the adapter
/// owns the live pause, this is its durable representation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettlementWindowPause {
    /// The index the pause is scoped to.
    pub index_id: String,
    /// The quarter whose settlement window is withheld, in Unix milliseconds.
    pub quarter_close_time: i64,
    /// Exclusive lower bound of the interval a repair must cover.
    pub after_index_time: i64,
    /// Inclusive upper bound of that interval.
    pub through_index_time: i64,
}

impl SettlementWindowPause {
    /// Largest accepted `index_id`, matching the adapter's own bound.
    const MAX_INDEX_ID_BYTES: usize = 32;

    pub fn validate(&self) -> Result<(), PredictionSignalError> {
        if self.index_id.is_empty() || self.index_id.len() > Self::MAX_INDEX_ID_BYTES {
            return Err(PredictionSignalError::Checkpoint(
                "settlement-window pause index_id is empty or too long".to_owned(),
            ));
        }
        if self.after_index_time > self.through_index_time {
            return Err(PredictionSignalError::Checkpoint(
                "settlement-window pause interval ends before it starts".to_owned(),
            ));
        }
        if self.quarter_close_time < self.through_index_time {
            return Err(PredictionSignalError::Checkpoint(
                "settlement-window pause quarter closes before the interval it withholds"
                    .to_owned(),
            ));
        }
        Ok(())
    }

    /// True when `quarter_close_time` names a quarter this pause withholds: the
    /// paused quarter itself, and every earlier one still unrepaired.
    #[must_use]
    pub fn withholds(&self, quarter_close_time: i64) -> bool {
        quarter_close_time <= self.quarter_close_time
    }

    /// Widen this pause to also cover `other`, keeping the later quarter close
    /// (so the `<=` withholding rule subsumes both gaps) and unioning the repair
    /// interval a resume must cover. The `index_id` follows the later quarter,
    /// which for the single configured index (BRTI) is always the same string.
    #[must_use]
    pub fn widen(self, other: SettlementWindowPause) -> SettlementWindowPause {
        let (index_id, quarter_close_time) = if other.quarter_close_time >= self.quarter_close_time
        {
            (other.index_id, other.quarter_close_time)
        } else {
            (self.index_id, self.quarter_close_time)
        };
        SettlementWindowPause {
            index_id,
            quarter_close_time,
            after_index_time: self.after_index_time.min(other.after_index_time),
            through_index_time: self.through_index_time.max(other.through_index_time),
        }
    }
}

impl PredictionSignalCheckpointState {
    pub fn validate(&self, index_armed: bool) -> Result<(), PredictionSignalError> {
        if let Some(pause) = &self.settlement_window_pause {
            pause.validate()?;
        }
        self.spot_reference
            .validate()
            .map_err(|error| PredictionSignalError::Checkpoint(error.to_string()))?;
        // The venue's expected set for the restoring arming, not a shared count:
        // Polymarket and Kalshi publish different identity sets, and an armed
        // Kalshi lane adds its two settlement twins, so a checkpoint written by
        // one venue — or under a different arming — can never restore here. The
        // global oracle reference re-warms and is not a checkpointed feature.
        let expected: BTreeSet<String> = prediction_signal_ids_for(self.venue, index_armed)
            .into_iter()
            .map(ToOwned::to_owned)
            .collect();
        if self.features.keys().cloned().collect::<BTreeSet<_>>() != expected {
            return Err(PredictionSignalError::Checkpoint(
                "prediction checkpoint feature identities do not match the venue signal set"
                    .to_owned(),
            ));
        }
        if self.last_market_generation == 0 && self.active_market_id.is_some() {
            return Err(PredictionSignalError::Checkpoint(
                "generation zero cannot carry an active market identity".to_owned(),
            ));
        }
        if self
            .active_market_id
            .as_ref()
            .is_some_and(|value| value.is_empty() || value.len() > 256)
        {
            return Err(PredictionSignalError::Checkpoint(
                "active market identity is empty or too long".to_owned(),
            ));
        }
        for (id, feature) in &self.features {
            let has_lifetime_output = feature.lifetime_output_count > 0;
            if has_lifetime_output != feature.last_quality.is_some()
                || (!has_lifetime_output
                    && (feature.ever_ready || !feature.quality_reason_entries.is_empty()))
            {
                return Err(PredictionSignalError::Checkpoint(format!(
                    "feature {id} has inconsistent prior output state"
                )));
            }
            if feature
                .quality_reason_entries
                .iter()
                .any(|(reason, count)| {
                    reason.is_empty() || *count == 0 || *count > feature.lifetime_output_count
                })
            {
                return Err(PredictionSignalError::Checkpoint(format!(
                    "feature {id} has an invalid quality transition counter"
                )));
            }
            if let Some(quality) = &feature.last_quality {
                let reasons_valid = match quality.state {
                    QualityState::Ready => quality.reasons.is_empty(),
                    QualityState::WarmingUp | QualityState::Degraded => {
                        !quality.reasons.is_empty()
                            && quality.reasons.iter().all(|reason| !reason.is_empty())
                    }
                };
                if !reasons_valid
                    || quality.reasons.windows(2).any(|pair| pair[0] >= pair[1])
                    || (quality.state == QualityState::Ready && !feature.ever_ready)
                {
                    return Err(PredictionSignalError::Checkpoint(format!(
                        "feature {id} has invalid persisted quality state"
                    )));
                }
            }
        }
        Ok(())
    }
}

/// The affirmative midpoint sampled once per second, plus the ten-second bars
/// folded from it (C3 rows 2, 3, 7; ruling E58).
///
/// Prediction-specific by construction: the sampled quantity is a probability in
/// points, the series is per market generation, and it is cleared at the roll so
/// a reading never spans two contracts (spec C3 §2). The generic bar and ring
/// mechanics live in [`crate::ta`].
#[derive(Debug, Clone)]
struct ProbabilitySeries {
    ring: SampleRing,
    bars: BarSeries,
}

impl ProbabilitySeries {
    fn new() -> Self {
        Self {
            ring: SampleRing::new(PROBABILITY_RING_SECONDS),
            bars: BarSeries::new(PROBABILITY_BAR_CAPACITY, PROBABILITY_BAR_SECONDS),
        }
    }

    /// Fold one decision's midpoint into the one-second ring, closing the
    /// previous second into the bar series first.
    ///
    /// The bar series is fed from the CLOSED second — the second whose last
    /// value is now known — so a bar is never built from a second still being
    /// written, and a skipped second reaches the series as the gap it is.
    fn observe(&mut self, now_ns: u64, points: Decimal) {
        let second = now_ns / crate::ta_bars::SECOND_NS;
        if let Some((last_second, last_value)) = self.ring.latest()
            && second > last_second
        {
            self.bars.observe(last_second, last_value);
        }
        self.ring.observe(now_ns, points);
    }

    fn clear(&mut self) {
        self.ring.clear();
        self.bars.clear();
    }
}

/// The run of settled outcomes over the bound market family's prior windows
/// (C3 row 10).
///
/// Fed by exactly one event: a settlement twin publishing its one final ready
/// decision for a completing window (E37b, including an E37c departed release).
/// Each window is counted once — a quarter close at or before the last counted
/// one is ignored — so a replay, a re-fed completing frame or a restart cannot
/// double-count a window.
#[derive(Debug, Clone, Default)]
struct WindowStreak {
    recent: VecDeque<bool>,
    streak_length: u32,
    streak_up: Option<bool>,
    settled_window_count: u32,
    last_close_ms: Option<i64>,
}

impl WindowStreak {
    fn from_checkpoint(state: &WindowStreakCheckpointState) -> Self {
        let recent: VecDeque<bool> = state
            .recent_outcomes
            .iter()
            .rev()
            .take(STREAK_RECENT_OUTCOMES)
            .rev()
            .copied()
            .collect();
        Self {
            recent,
            streak_length: state.streak_length,
            streak_up: state.streak_up,
            settled_window_count: state.settled_window_count,
            last_close_ms: state.last_close_ms,
        }
    }

    fn checkpoint(&self) -> Option<WindowStreakCheckpointState> {
        if self.settled_window_count == 0 {
            return None;
        }
        Some(WindowStreakCheckpointState {
            recent_outcomes: self.recent.iter().copied().collect(),
            streak_length: self.streak_length,
            streak_up: self.streak_up,
            settled_window_count: self.settled_window_count,
            last_close_ms: self.last_close_ms,
        })
    }

    fn record(&mut self, quarter_close_ms: i64, up: bool) {
        if self
            .last_close_ms
            .is_some_and(|last| quarter_close_ms <= last)
        {
            return;
        }
        self.last_close_ms = Some(quarter_close_ms);
        self.settled_window_count = self.settled_window_count.saturating_add(1);
        if self.streak_up == Some(up) {
            self.streak_length = self.streak_length.saturating_add(1);
        } else {
            self.streak_up = Some(up);
            self.streak_length = 1;
        }
        self.recent.push_back(up);
        while self.recent.len() > STREAK_RECENT_OUTCOMES {
            self.recent.pop_front();
        }
    }

    fn ready(&self) -> bool {
        self.settled_window_count >= STREAK_READY_WINDOWS
    }

    /// The last outcomes as `u`/`d`, oldest first.
    fn recent_text(&self) -> String {
        self.recent
            .iter()
            .map(|up| if *up { 'u' } else { 'd' })
            .collect()
    }

    fn up_share(&self) -> Result<Decimal, PredictionSignalError> {
        let total = u32::try_from(self.recent.len()).map_err(|_| PredictionSignalError::Counter)?;
        if total == 0 {
            return Ok(Decimal::ZERO);
        }
        let ups = u32::try_from(self.recent.iter().filter(|up| **up).count())
            .map_err(|_| PredictionSignalError::Counter)?;
        checked_div(Decimal::from(ups), Decimal::from(total))
    }
}

/// The durable form of [`WindowStreak`], carried in the prediction checkpoint as
/// an additive optional field (the checkpoint schema is unchanged at 8: a state
/// with no settled window serializes byte-identically to the pre-C3b shape, so
/// every existing checkpoint still matches its recorded digest, ruling N2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowStreakCheckpointState {
    /// The retained outcomes, oldest first, at most eight.
    pub recent_outcomes: Vec<bool>,
    /// Length of the current run, which may exceed the retained outcomes.
    pub streak_length: u32,
    /// The side of the current run, or `None` before the first settled window.
    pub streak_up: Option<bool>,
    /// Settled windows observed since this processor first started.
    pub settled_window_count: u32,
    /// The derived quarter close of the newest counted window (Unix ms), the
    /// exactly-once guard on a replay or a resume.
    pub last_close_ms: Option<i64>,
}

/// Which Polymarket RTDS TWAP frame the opening strike was taken from (E11).
/// The primary source is the frame whose `event_time` equals `window_open`
/// exactly; when that frame never arrives the fallback selects an adjacent
/// frame. Either way the market publishes Ready — this value rides the wire on
/// `market_contract` and every strike-derived identity so a consumer can filter
/// an approximate strike itself. A strike is fixed once per market, so this is
/// set once and never revised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpeningStrikeSource {
    /// The TWAP frame with `event_time == window_open`.
    Exact,
    /// The last frame with `event_time <= window_open`, within 5 s before the open.
    PriorFrame,
    /// The first frame with `event_time > window_open`, within 5 s after the open.
    PostFrame,
}

impl OpeningStrikeSource {
    /// The wire enum value for the additive `opening_strike_source` field.
    fn as_wire(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::PriorFrame => "prior_frame",
            Self::PostFrame => "post_frame",
        }
    }
}

#[derive(Debug, Clone)]
struct ActiveContract {
    market_id: String,
    /// The raw contract source event this binding was parsed from, retained so a
    /// rotated generation's checkpoint can rebuild `active` on replay without a
    /// fresh Bind (C5). Deterministic: re-parsing it yields this same contract.
    contract_event: SourceEvent,
    routing_id: String,
    generation: u64,
    window_open: String,
    window_close: String,
    parsed_open: DateTime<Utc>,
    parsed_close: DateTime<Utc>,
    strike: Option<Decimal>,
    /// The provenance of `strike`, set once when the strike is fixed (E11). `None`
    /// until a strike is attached; on Kalshi the strike is bound directly and this
    /// stays `None` (its rules card carries no opening-TWAP boundary observation).
    opening_strike_source: Option<OpeningStrikeSource>,
    settlement_benchmark: &'static str,
    averaging_window_seconds: u32,
    comparator: &'static str,
    tie_outcome: &'static str,
    affirmative_outcome: &'static str,
    negative_outcome: &'static str,
    minimum_tick_size: Option<Decimal>,
    minimum_order_size: Option<Decimal>,
    rules_sha256: String,
    contract_time: String,
}

#[derive(Debug, Clone)]
struct QuoteObservation {
    bid: Decimal,
    ask: Decimal,
    logical_receipt_ns: u64,
    event_time: String,
}

#[derive(Debug, Clone)]
struct TradeObservation {
    outcome: String,
    price: Decimal,
    size: Decimal,
    logical_receipt_ns: u64,
    event_time: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum OutcomeAxis {
    Affirmative,
    Negative,
}

#[derive(Debug, Clone, Default)]
struct PriceBook {
    bids: BTreeMap<Decimal, Decimal>,
    asks: BTreeMap<Decimal, Decimal>,
    snapshot_seen: bool,
    ready: bool,
    invalid_reason: Option<String>,
    logical_receipt_ns: Option<u64>,
    event_time: Option<String>,
}

impl PriceBook {
    fn clear(&mut self) {
        self.bids.clear();
        self.asks.clear();
        self.snapshot_seen = false;
        self.ready = false;
        self.invalid_reason = None;
        self.logical_receipt_ns = None;
        self.event_time = None;
    }

    fn refresh_readiness(&mut self) -> Result<(), PredictionSignalError> {
        if self.bids.len() > MAX_BOOK_LEVELS_PER_SIDE || self.asks.len() > MAX_BOOK_LEVELS_PER_SIDE
        {
            return Err(PredictionSignalError::ResourceLimit(
                "prediction book exceeded 100000 active levels on one side".to_owned(),
            ));
        }
        self.ready = false;
        self.invalid_reason = if !self.snapshot_seen {
            Some("snapshot_missing".to_owned())
        } else if self.bids.is_empty() || self.asks.is_empty() {
            Some("one_sided".to_owned())
        } else if self.validate().is_err() {
            Some("crossed_or_invalid".to_owned())
        } else {
            self.ready = true;
            None
        };
        Ok(())
    }

    fn unavailable_reason(&self, prefix: &str) -> String {
        if self.ready {
            format!("{prefix}_stale")
        } else {
            format!(
                "{prefix}_{}",
                self.invalid_reason.as_deref().unwrap_or("snapshot_missing")
            )
        }
    }

    fn validate(&self) -> Result<(), PredictionSignalError> {
        if self.bids.len() > MAX_BOOK_LEVELS_PER_SIDE || self.asks.len() > MAX_BOOK_LEVELS_PER_SIDE
        {
            return Err(PredictionSignalError::ResourceLimit(
                "prediction book exceeded 100000 active levels on one side".to_owned(),
            ));
        }
        let Some((&bid, &bid_size)) = self.bids.iter().next_back() else {
            return Err(PredictionSignalError::Book(
                "prediction book has no bid".to_owned(),
            ));
        };
        let Some((&ask, &ask_size)) = self.asks.iter().next() else {
            return Err(PredictionSignalError::Book(
                "prediction book has no ask".to_owned(),
            ));
        };
        if bid <= Decimal::ZERO
            || ask > Decimal::ONE
            || bid > ask
            || bid_size <= Decimal::ZERO
            || ask_size <= Decimal::ZERO
        {
            return Err(PredictionSignalError::Book(
                "prediction book best quote is invalid or crossed".to_owned(),
            ));
        }
        Ok(())
    }

    fn metrics(
        &self,
        now_ns: u64,
        venue_alive: bool,
    ) -> Result<Option<BookMetrics>, PredictionSignalError> {
        if !self.ready {
            return Ok(None);
        }
        let receipt_ns = self.logical_receipt_ns.ok_or_else(|| {
            PredictionSignalError::State("ready prediction book is missing receipt time".to_owned())
        })?;
        let age_ns = now_ns
            .checked_sub(receipt_ns)
            .ok_or(PredictionSignalError::ReceiptAfterAdmission { receipt_ns, now_ns })?;
        if !venue_alive && age_ns > PREDICTION_QUOTE_STALENESS_NS {
            return Ok(None);
        }
        let (&best_bid, &best_bid_size) = self.bids.iter().next_back().ok_or_else(|| {
            PredictionSignalError::State("ready prediction book lost its bid".to_owned())
        })?;
        let (&best_ask, &best_ask_size) = self.asks.iter().next().ok_or_else(|| {
            PredictionSignalError::State("ready prediction book lost its ask".to_owned())
        })?;
        let total_bid_depth = checked_sum(self.bids.values().copied())?;
        let total_ask_depth = checked_sum(self.asks.values().copied())?;
        let probe = Decimal::from(PREDICTION_LIQUIDITY_PROBE_UNITS);
        let buy_cost = walk_asks(&self.asks, probe)?;
        let sell_proceeds = walk_bids(&self.bids, probe)?;
        Ok(Some(BookMetrics {
            best_bid,
            best_ask,
            best_bid_size,
            best_ask_size,
            total_bid_depth,
            total_ask_depth,
            buy_cost,
            sell_proceeds,
            age_ms: age_ns / 1_000_000,
            event_time: self.event_time.clone().ok_or_else(|| {
                PredictionSignalError::State(
                    "ready prediction book is missing event time".to_owned(),
                )
            })?,
        }))
    }

    fn kalshi_complement_metrics(
        &self,
        now_ns: u64,
        venue_alive: bool,
    ) -> Result<(Option<BookMetrics>, Option<&'static str>), PredictionSignalError> {
        if !self.ready {
            return Ok((None, None));
        }
        let receipt_ns = self.logical_receipt_ns.ok_or_else(|| {
            PredictionSignalError::State("ready prediction book is missing receipt time".to_owned())
        })?;
        let age_ns = now_ns
            .checked_sub(receipt_ns)
            .ok_or(PredictionSignalError::ReceiptAfterAdmission { receipt_ns, now_ns })?;
        if !venue_alive && age_ns > PREDICTION_QUOTE_STALENESS_NS {
            return Ok((None, None));
        }
        let (&yes_bid, &yes_bid_size) = self.bids.iter().next_back().ok_or_else(|| {
            PredictionSignalError::State("ready prediction book lost its bid".to_owned())
        })?;
        let (&yes_ask, &yes_ask_size) = self.asks.iter().next().ok_or_else(|| {
            PredictionSignalError::State("ready prediction book lost its ask".to_owned())
        })?;
        let no_bid = checked_sub(Decimal::ONE, yes_ask)?;
        let no_ask = checked_sub(Decimal::ONE, yes_bid)?;
        // A YES ask at exactly 1.00 is legal at the venue, but its synthetic
        // NO bid is zero and is outside this signal contract's positive-price
        // book domain. Contain the representational edge as venue degradation
        // instead of rejecting the valid provider book or failing the run.
        if no_bid <= Decimal::ZERO || no_ask > Decimal::ONE || no_bid > no_ask {
            return Ok((None, Some("kalshi_complement_invalid")));
        }
        let probe = Decimal::from(PREDICTION_LIQUIDITY_PROBE_UNITS);
        let buy_cost = walk_complement_levels(self.bids.iter().rev(), probe)?;
        let sell_proceeds = walk_complement_levels(self.asks.iter(), probe)?;
        Ok((
            Some(BookMetrics {
                best_bid: no_bid,
                best_ask: no_ask,
                best_bid_size: yes_ask_size,
                best_ask_size: yes_bid_size,
                total_bid_depth: checked_sum(self.asks.values().copied())?,
                total_ask_depth: checked_sum(self.bids.values().copied())?,
                buy_cost,
                sell_proceeds,
                age_ms: age_ns / 1_000_000,
                event_time: self.event_time.clone().ok_or_else(|| {
                    PredictionSignalError::State(
                        "ready prediction book is missing event time".to_owned(),
                    )
                })?,
            }),
            None,
        ))
    }
}

#[derive(Debug, Clone)]
struct BookRow {
    axis: OutcomeAxis,
    side: String,
    price: Decimal,
    size: Decimal,
}

#[derive(Debug, Clone)]
struct PendingBookGroup {
    id: String,
    kind: String,
    count: u32,
    next_index: u32,
    logical_receipt_ns: u64,
    event_time: String,
    rows: Vec<BookRow>,
}

#[derive(Debug, Clone, Default)]
struct PredictionBooks {
    affirmative: PriceBook,
    negative: PriceBook,
    pending: Option<PendingBookGroup>,
}

impl PredictionBooks {
    fn clear(&mut self) {
        self.affirmative.clear();
        self.negative.clear();
        self.pending = None;
    }

    fn ingest(
        &mut self,
        venue: PredictionMarketVenue,
        event: &SourceEvent,
        logical_receipt_ns: u64,
    ) -> Result<(), PredictionSignalError> {
        let group = required_text(&event.values, "update_group")?;
        let index = required_u32(&event.values, "update_index")?;
        let count = required_u32(&event.values, "update_count")?;
        let kind = required_text(&event.values, "update_kind")?;
        if count == 0
            || index >= count
            || usize::try_from(count).unwrap_or(usize::MAX) > MAX_PENDING_BOOK_ROWS
        {
            return Err(PredictionSignalError::Book(
                "prediction book group count or index is invalid".to_owned(),
            ));
        }
        if !matches!(kind, "snapshot" | "delta") {
            return Err(PredictionSignalError::Book(format!(
                "unsupported prediction book update kind {kind}"
            )));
        }
        if index == 0 {
            if self.pending.is_some() {
                return Err(PredictionSignalError::Book(
                    "new prediction book group arrived before prior completion".to_owned(),
                ));
            }
            self.pending = Some(PendingBookGroup {
                id: group.to_owned(),
                kind: kind.to_owned(),
                count,
                next_index: 0,
                logical_receipt_ns,
                event_time: event.event_time.clone(),
                rows: Vec::with_capacity(usize::try_from(count).unwrap_or(0)),
            });
        }
        let pending = self.pending.as_mut().ok_or_else(|| {
            PredictionSignalError::Book(
                "prediction book group did not begin at index zero".to_owned(),
            )
        })?;
        if pending.id != group
            || pending.kind != kind
            || pending.count != count
            || pending.next_index != index
            || pending.logical_receipt_ns != logical_receipt_ns
            || pending.event_time != event.event_time
        {
            return Err(PredictionSignalError::Book(
                "prediction book group metadata or row order changed before completion".to_owned(),
            ));
        }
        let axis = if venue == PredictionMarketVenue::Kalshi {
            OutcomeAxis::Affirmative
        } else {
            match required_text(&event.values, "outcome")? {
                "up" => OutcomeAxis::Affirmative,
                "down" => OutcomeAxis::Negative,
                other => {
                    return Err(PredictionSignalError::MarketValue(format!(
                        "unsupported Polymarket outcome {other}"
                    )));
                }
            }
        };
        let side = required_text(&event.values, "side")?;
        if !matches!(side, "bid" | "ask") {
            return Err(PredictionSignalError::MarketValue(format!(
                "unsupported prediction book side {side}"
            )));
        }
        let price = required_decimal(&event.values, "price")?;
        let size = required_decimal(&event.values, "size")?;
        if price <= Decimal::ZERO || price > Decimal::ONE || size < Decimal::ZERO {
            return Err(PredictionSignalError::MarketValue(
                "prediction book price must be in (0,1] and size non-negative".to_owned(),
            ));
        }
        pending.rows.push(BookRow {
            axis,
            side: side.to_owned(),
            price,
            size,
        });
        pending.next_index = pending
            .next_index
            .checked_add(1)
            .ok_or(PredictionSignalError::Counter)?;
        if pending.next_index != pending.count {
            return Ok(());
        }

        let pending = self.pending.take().ok_or_else(|| {
            PredictionSignalError::State("completed prediction book group disappeared".to_owned())
        })?;
        let mut affirmative = self.affirmative.clone();
        let mut negative = self.negative.clone();
        let axes = pending
            .rows
            .iter()
            .map(|row| row.axis)
            .collect::<BTreeSet<_>>();
        if pending.kind == "snapshot" {
            if venue == PredictionMarketVenue::Polymarket && axes.len() != 1 {
                return Err(PredictionSignalError::Book(
                    "Polymarket snapshot group must contain one outcome token".to_owned(),
                ));
            }
            for axis in &axes {
                match axis {
                    OutcomeAxis::Affirmative => {
                        affirmative.clear();
                        affirmative.snapshot_seen = true;
                    }
                    OutcomeAxis::Negative => {
                        negative.clear();
                        negative.snapshot_seen = true;
                    }
                }
            }
        }
        for row in pending.rows {
            let book = match row.axis {
                OutcomeAxis::Affirmative => &mut affirmative,
                OutcomeAxis::Negative => &mut negative,
            };
            let levels = if row.side == "bid" {
                &mut book.bids
            } else {
                &mut book.asks
            };
            if row.size.is_zero() {
                levels.remove(&row.price);
            } else {
                levels.insert(row.price, row.size);
            }
            book.logical_receipt_ns = Some(pending.logical_receipt_ns);
            book.event_time = Some(pending.event_time.clone());
        }
        for axis in axes {
            match axis {
                OutcomeAxis::Affirmative => affirmative.refresh_readiness()?,
                OutcomeAxis::Negative => negative.refresh_readiness()?,
            }
        }
        self.affirmative = affirmative;
        self.negative = negative;
        Ok(())
    }
}

/// The standardized-moneyness computation shared across the composite emitters.
#[derive(Debug, Clone)]
struct Moneyness {
    d: Decimal,
    reference_price: Decimal,
    strike: Decimal,
    sigma_per_second: Decimal,
    tau_seconds: Decimal,
    observation_time: String,
}

#[derive(Debug, Clone)]
struct BookMetrics {
    best_bid: Decimal,
    best_ask: Decimal,
    best_bid_size: Decimal,
    best_ask_size: Decimal,
    total_bid_depth: Decimal,
    total_ask_depth: Decimal,
    buy_cost: Option<Decimal>,
    sell_proceeds: Option<Decimal>,
    age_ms: u64,
    event_time: String,
}

#[derive(Debug, Clone, Copy, Default)]
struct FlowTotals {
    signed_notional: Decimal,
    signed_units: Decimal,
    gross_notional: Decimal,
    count: u64,
}

#[derive(Debug, Clone, Copy)]
struct FlowEntry {
    logical_receipt_ns: u64,
    signed_notional: Decimal,
    signed_units: Decimal,
    gross_notional: Decimal,
}

#[derive(Debug, Clone, Default)]
struct PredictionFlow {
    entries: VecDeque<FlowEntry>,
    observed_since_ns: Option<u64>,
    last_receipt_ns: Option<u64>,
}

impl PredictionFlow {
    fn reset(&mut self, observed_since_ns: u64) {
        self.entries.clear();
        self.observed_since_ns = Some(observed_since_ns);
        self.last_receipt_ns = None;
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.observed_since_ns = None;
        self.last_receipt_ns = None;
    }

    fn ingest(
        &mut self,
        logical_receipt_ns: u64,
        signed_notional: Decimal,
        signed_units: Decimal,
        gross_notional: Decimal,
    ) -> Result<(), PredictionSignalError> {
        if self
            .last_receipt_ns
            .is_some_and(|previous| logical_receipt_ns < previous)
        {
            return Err(PredictionSignalError::ReceiptRegression);
        }
        if self.entries.len() >= MAX_FLOW_EVENTS {
            return Err(PredictionSignalError::ResourceLimit(
                "prediction flow exceeded 1000000 retained trades".to_owned(),
            ));
        }
        self.entries.push_back(FlowEntry {
            logical_receipt_ns,
            signed_notional,
            signed_units,
            gross_notional,
        });
        self.last_receipt_ns = Some(logical_receipt_ns);
        Ok(())
    }

    fn prune(&mut self, now_ns: u64) {
        let cutoff = now_ns.saturating_sub(PREDICTION_FLOW_WINDOW_NS + 1_000_000_000);
        while self
            .entries
            .front()
            .is_some_and(|entry| entry.logical_receipt_ns <= cutoff)
        {
            self.entries.pop_front();
        }
    }

    fn window(&self, now_ns: u64, duration_ns: u64) -> Result<FlowTotals, PredictionSignalError> {
        let cutoff = now_ns.saturating_sub(duration_ns);
        let mut totals = FlowTotals::default();
        for entry in self
            .entries
            .iter()
            .filter(|entry| entry.logical_receipt_ns > cutoff && entry.logical_receipt_ns <= now_ns)
        {
            totals.signed_notional = checked_add(totals.signed_notional, entry.signed_notional)?;
            totals.signed_units = checked_add(totals.signed_units, entry.signed_units)?;
            totals.gross_notional = checked_add(totals.gross_notional, entry.gross_notional)?;
            totals.count = totals
                .count
                .checked_add(1)
                .ok_or(PredictionSignalError::Counter)?;
        }
        Ok(totals)
    }

    fn warmed(&self, now_ns: u64) -> bool {
        self.observed_since_ns
            .is_some_and(|start| now_ns.saturating_sub(start) >= PREDICTION_FLOW_WINDOW_NS)
    }
}

/// Deterministic first-stage prediction-market signal set for one venue.
///
/// A caller owns discovery and emits the corresponding `market_transition`
/// frame. It must preflight `validate_market_binding` before writing a durable
/// bound frame, then call `bind_market`; `mark_unavailable` follows the matching
/// unavailable frame. Both operations reset all market-relative state; shared
/// spot state lives outside this processor.
pub struct PredictionSignalProcessor {
    venue: PredictionMarketVenue,
    publisher_epoch: String,
    wall_start: DateTime<Utc>,
    emitters: BTreeMap<PredictionSignalKind, PredictionEmitter>,
    source_definitions: BTreeMap<String, SourceDefinition>,
    // Internally hosted spot pipeline (ADR 0016): the identical published
    // spot_reference code path over the Coinbase/Kraken ticker streams, plus the
    // shared realized-volatility unit. Not market-relative: survives rollover.
    reference: SpotReferenceProcessor,
    volatility: RealizedVolatility,
    // Latest reference update (S and its readiness) and latest volatility metrics
    // (sigma), refreshed on the health tick; read by the composite emitters.
    latest_reference: Option<SignalUpdate>,
    latest_volatility: Option<VolatilityMetrics>,
    // C2 (E49): the venue's own probability momentum/volatility ring. It samples
    // this market generation's prediction_quote affirmative_midpoint once per
    // second (health-tick cadence). Market-relative rolling state: reset on every
    // rollover by `clear_market_state`, re-warms after a restore, never
    // checkpointed — exactly like the realized-volatility ring above.
    prob_ring: ProbabilityHorizonRing,
    active: Option<ActiveContract>,
    last_market_generation: u64,
    affirmative_quote: Option<QuoteObservation>,
    negative_quote: Option<QuoteObservation>,
    affirmative_quote_fault: Option<String>,
    negative_quote_fault: Option<String>,
    last_trade: Option<TradeObservation>,
    books: PredictionBooks,
    flow: PredictionFlow,
    twap_by_time: BTreeMap<DateTime<Utc>, Decimal>,
    // Adopted candidate computations. `pair_cost` and the tie-rule band are
    // parameter holders evaluated from same-decision inputs, so they carry no
    // cross-decision state; `repricing_lag` is genuinely stateful (pending move,
    // previous spot and venue midpoints) and is market-relative, so it is reset
    // on every rollover and re-warms after a restore exactly as the flow and
    // book state do.
    pair_cost: PairCostCandidate,
    repricing_lag: VenueRepricingLagCandidate,
    tie_rule: TieRuleContextCandidate,
    // Polymarket-only (ADR 0023 amendment, 2026-09-02). Both are parameter
    // holders: every input they read — the latest admitted resolving TWAP, the
    // bound rules card, the hosted spot reference — is re-fed from processor
    // state on each decision, so neither carries market-relative state to reset
    // on a rollover. The anchor state's only cross-decision state is its
    // monotonic observation clock, which is the processor's own decision clock.
    // Both are constructed on either venue so construction stays venue-uniform;
    // the Kalshi processor never feeds or reads them.
    settlement_anchor: SettlementAnchorStateCandidate,
    oracle_divergence: OracleDivergenceCandidate,
    source_ordering: BTreeMap<String, (String, u64)>,
    last_now_ns: Option<u64>,
    // Latest admitted venue-input receipt (book, quote, trade, or contract).
    // Market-relative, so it does not survive recovery and is not checkpointed.
    last_input_receipt_ns: Option<u64>,
    /// The withheld Kalshi settlement window, if one is outstanding (A1/F4).
    settlement_window_pause: Option<SettlementWindowPause>,
    /// The derived `quarter_close_time` (Unix ms) of the most recent settlement
    /// window whose one final ready decision has already been published (ruling
    /// E37b), or `None` before any close. A high-water mark: the completing tick
    /// publishes the settled anchor exactly once, and every later decision for
    /// that window — a re-fed completing frame, a health tick, a replay, or a
    /// resume from the checkpoint that recorded it — is `market_closed` because
    /// its `quarter_close_time` is no longer strictly greater. Unix milliseconds,
    /// not the wall-relative logical clock, so it stays comparable across a
    /// restart whose wall origin differs. Checkpointed (v7).
    settlement_final_close_ms: Option<i64>,
    /// Whether the Kalshi lane's CF (`cfbrti`) index socket is armed. Off in the
    /// deployed four-venue configuration, so the Kalshi batch and emitter set are
    /// byte-unchanged by construction (E43). Always false on Polymarket.
    index_armed: bool,
    /// The global-scope `btc.cfbrti.oracle_reference` emitter, present only while
    /// the index socket is armed. It is a coalescing emitter reused verbatim from
    /// the oracle lane (`OracleReferenceEmitter`, E36: 1 Hz channel only).
    cfbrti_emitter: Option<OracleReferenceEmitter>,
    /// The latest accepted CF BRTI value observation: the anchor the Kalshi
    /// settlement twins resolve on and the reading the oracle reference publishes.
    /// Re-warms from the live socket after a restart; never checkpointed (§5.4).
    cf_anchor: Option<CfAnchorObservation>,
    /// The prior accepted BRTI publish time, for the oracle reference's
    /// `update_interval_ms`. Cleared with the anchor; not checkpointed.
    cf_previous_publish_time: Option<String>,
    /// A standing CF value fault (a non-positive or unrepresentable index value):
    /// the index-dependent instances publish it as `cfbrti_value_invalid` instead
    /// of a zero, and it clears on the next valid frame. Not checkpointed.
    cf_fault: Option<&'static str>,
    /// Global-scope updates the current decision produced (the oracle reference),
    /// drained by the runtime outside the market completeness guard.
    pending_global_updates: Vec<SignalUpdate>,
    /// The latest accepted settlement-window observation (F3): the derived
    /// `quarter_close_time` on the 15-minute grid and the running `window_as_of`.
    /// Completeness is `window_as_of == quarter_close_time`, true four times an
    /// hour, and it — not the always-full trailing average of the value frame —
    /// is what makes the settlement anchor a settlement state. The anchor PRICE
    /// still comes from the value channel's `avg_60s_value`. Not checkpointed;
    /// re-warms from the live socket after a restart.
    cf_settlement_window: Option<CfSettlementWindow>,
    /// Whether an accepted CF observation has ever made the index warm. Before
    /// it, a dropped or refused socket is still `cfbrti_warming_up`; after it, a
    /// drop degrades to `cfbrti_disconnected` (§6, "after a ready decision").
    /// Not checkpointed.
    cf_ever_warm: bool,
    /// Whether the CF index socket is currently down after a ready decision
    /// (F1). Set by the runtime from a `kalshi_index` connectivity transition;
    /// clears on reconnect. Not checkpointed.
    cf_disconnected: bool,
    /// The standing subscription/entitlement refusal on the armed 1 Hz value
    /// channel, if any (F2): the runtime mirrors
    /// `index_channel_unavailable_reason` for the value source here, so the
    /// index-dependent instances publish it (`cfbrti_subscription_rejected` /
    /// `cfbrti_entitlement_denied`) and clear it on acceptance. Not checkpointed.
    cf_subscription_reason: Option<&'static str>,
    /// A departed Kalshi market's pending final settlement decision (ruling
    /// E37c), frozen at the rollover and released when the completing
    /// settlement-window frame for its quarter arrives. Checkpointed (v8) so a
    /// restart in the gap neither re-publishes nor loses it.
    pending_final_decision: Option<PendingFinalDecisionState>,
    /// The departed pair a release produced this decision: written by the runtime
    /// outside the per-market completeness guard, exactly like the global oracle
    /// reference, since the pair is scoped to the departed market, not the bound
    /// one. Never checkpointed — a restart re-derives it from the pending slot.
    pending_departed_updates: Vec<SignalUpdate>,
    /// Count of pending final decisions that expired unpublished since the last
    /// drain (ruling E37c), reported to feed health by the runtime. Never
    /// checkpointed.
    expired_final_decisions: u32,
    /// The affirmative-midpoint one-second ring and its ten-second bars, behind
    /// `rsi`, `roc` and `zscore` (C3b). Per generation: cleared at the market
    /// roll, and rolling state that re-warms after a restart rather than being
    /// checkpointed.
    probability: ProbabilitySeries,
    /// Admitted quote/trade/midpoint-change counters behind `tick_velocity`.
    /// Per generation and not checkpointed, like the probability series.
    tick_velocity: TickVelocity,
    /// The settled-window streak behind `window_streak` (Kalshi, armed only).
    /// The one identity that reads across windows, so it survives the market
    /// roll and is checkpointed.
    window_streak: WindowStreak,
}

/// One oracle-reference evaluation: the ready `values` (or `None`), and the
/// non-ready reasons. The tuple the coalescing emitter consumes.
type CfbrtiReading = (Option<BTreeMap<String, Value>>, Vec<String>);

/// The latest accepted CF Benchmarks BRTI value-channel observation. The oracle
/// reference reads `index_value`; the settlement twins read `avg_60s_value` as
/// the settlement anchor PRICE (§5.2). The value frame's trailing average is
/// always a full 60-second span, so its bounds say nothing about settlement
/// completeness — that comes from the settlement-window source (E37,
/// `CfSettlementWindow`), never from here.
#[derive(Debug, Clone)]
struct CfAnchorObservation {
    index_value: Decimal,
    avg_60s_value: Decimal,
    avg_60s_window_size: u32,
    index_id: String,
    publish_time: String,
    /// Wall-clock ms of the newest accepted print, for the staleness bound (F1).
    publish_time_ms: i64,
}

/// The latest accepted settlement-window observation (E28/E37, F3). The wire's
/// `window_start_ts_ms` is fixed for a quarter and its `window_end_ts_exclusive`
/// advances, so the close is DERIVED (`start + 60_000`) and validated on the
/// 15-minute grid; `window_as_of` is how far the final-minute average has
/// actually reached. The settlement anchor is complete exactly on the tick whose
/// as-of reaches the close — four times an hour — never on the always-full
/// trailing average of the 1 Hz value frame.
#[derive(Debug, Clone)]
struct CfSettlementWindow {
    /// The derived quarter close verbatim from the wire, republished as the
    /// `quarter_close_time` field of the settlement twins (F1).
    quarter_close_time: String,
    quarter_close_ms: i64,
    /// How far the final-minute average has reached, verbatim from the wire,
    /// republished as the `window_as_of` field of the settlement twins (F1).
    window_as_of: String,
    window_as_of_ms: i64,
    window_size: u32,
}

impl CfSettlementWindow {
    /// Complete when the running final-minute average has reached the derived
    /// close (`window_as_of == quarter_close`) with the full declared window of
    /// sixty prints. The 59 partials before the closing tick are incomplete.
    fn is_complete(&self) -> bool {
        self.window_as_of_ms == self.quarter_close_ms && self.window_size >= CFBRTI_FULL_WINDOW_SIZE
    }
}

/// Insert the three settlement-window fields the Kalshi settlement twins publish
/// (F1/E37) into a ready values map: the verbatim derived close, the running
/// as-of and the completeness flag. A no-op when `window` is `None` — the
/// Polymarket twins declare none of these fields and never pass one.
fn insert_settlement_window_fields(
    values: &mut BTreeMap<String, Value>,
    window: Option<&CfSettlementWindow>,
) {
    let Some(window) = window else {
        return;
    };
    values.insert(
        "quarter_close_time".to_owned(),
        json!(window.quarter_close_time),
    );
    values.insert("window_as_of".to_owned(), json!(window.window_as_of));
    values.insert(
        "settlement_window_complete".to_owned(),
        json!(window.is_complete()),
    );
}

impl PredictionSignalProcessor {
    pub fn new(
        venue: PredictionMarketVenue,
        publisher_epoch: impl Into<String>,
        wall_start: DateTime<Utc>,
    ) -> Result<Self, PredictionSignalError> {
        Self::new_indexed(venue, publisher_epoch, wall_start, false)
    }

    pub fn restore(
        publisher_epoch: impl Into<String>,
        wall_start: DateTime<Utc>,
        state: PredictionSignalCheckpointState,
    ) -> Result<Self, PredictionSignalError> {
        Self::restore_indexed(publisher_epoch, wall_start, state, false)
    }

    /// Build a processor with the Kalshi index socket armed or disarmed (E43).
    /// `new` is `new_indexed(.., false)`, the deployed four-venue configuration;
    /// only the demo Kalshi lane passes `true`. Arming is meaningful on Kalshi
    /// alone — Polymarket has no index socket and ignores the flag.
    pub fn new_indexed(
        venue: PredictionMarketVenue,
        publisher_epoch: impl Into<String>,
        wall_start: DateTime<Utc>,
        index_armed: bool,
    ) -> Result<Self, PredictionSignalError> {
        Self::from_history(
            venue,
            publisher_epoch.into(),
            wall_start,
            None,
            index_armed,
            false,
        )
    }

    /// Restore a processor with the given index-socket arming. The checkpoint's
    /// feature set is validated against the armed expected set, so a checkpoint
    /// written under one arming cannot restore under the other.
    ///
    /// The active market binding is NOT rebuilt: `active` stays `None` and the
    /// caller re-discovers and re-binds, exactly as the live
    /// `--resume-from-checkpoint` crash-recovery path does today (the binding may
    /// be stale after a crash).
    pub fn restore_indexed(
        publisher_epoch: impl Into<String>,
        wall_start: DateTime<Utc>,
        state: PredictionSignalCheckpointState,
        index_armed: bool,
    ) -> Result<Self, PredictionSignalError> {
        state.validate(index_armed)?;
        Self::from_history(
            state.venue,
            publisher_epoch.into(),
            wall_start,
            Some(state),
            index_armed,
            false,
        )
    }

    /// Restore a processor AND rebuild the active market binding from the
    /// checkpoint's `active_binding` (C5). Used only on the gap-free path — the
    /// replay of a rotated file generation, whose journal session carries an
    /// `admission_epoch` — where the binding provably continues from the sealed
    /// generation and there was no fresh Bind to re-establish it. A checkpoint
    /// without `active_binding` (unbound, or pre-C5) restores as `active = None`,
    /// identically to [`Self::restore_indexed`].
    pub fn restore_rotated_indexed(
        publisher_epoch: impl Into<String>,
        wall_start: DateTime<Utc>,
        state: PredictionSignalCheckpointState,
        index_armed: bool,
    ) -> Result<Self, PredictionSignalError> {
        state.validate(index_armed)?;
        Self::from_history(
            state.venue,
            publisher_epoch.into(),
            wall_start,
            Some(state),
            index_armed,
            true,
        )
    }

    fn from_history(
        venue: PredictionMarketVenue,
        publisher_epoch: String,
        wall_start: DateTime<Utc>,
        state: Option<PredictionSignalCheckpointState>,
        index_armed: bool,
        rebuild_active: bool,
    ) -> Result<Self, PredictionSignalError> {
        if publisher_epoch.trim().is_empty() || publisher_epoch.len() > 128 {
            return Err(PredictionSignalError::State(
                "publisher_epoch must contain 1 to 128 bytes".to_owned(),
            ));
        }
        let mut emitters = BTreeMap::new();
        for kind in PredictionSignalKind::expected_kinds(venue, index_armed) {
            let mut emitter = PredictionEmitter::new(venue, kind)?;
            if let Some(history) = state
                .as_ref()
                .and_then(|state| state.features.get(emitter.definition.signal_id.as_str()))
            {
                emitter.restore_history(history);
            }
            emitters.insert(kind, emitter);
        }
        // The global oracle reference rides the Kalshi lane only while its index
        // socket is armed; it re-warms from the live socket after a restart, so no
        // emitter memory is restored from the checkpoint (§5.4).
        let cfbrti_emitter = if index_armed && venue == PredictionMarketVenue::Kalshi {
            Some(
                OracleReferenceEmitter::from_document(CFBRTI_ORACLE_REFERENCE_DEFINITION).map_err(
                    |error| {
                        PredictionSignalError::Definition(format!(
                            "cfbrti oracle reference definition is invalid: {error}"
                        ))
                    },
                )?,
            )
        } else {
            None
        };
        let source_definitions = source_documents(venue, index_armed)
            .into_iter()
            .map(|(id, raw)| {
                let definition = parse_document::<SourceDefinition>(raw, id)?;
                if definition.source_id != id || definition.version != SOURCE_VERSION {
                    return Err(PredictionSignalError::Definition(format!(
                        "embedded source identity {}@{} does not match {id}@{SOURCE_VERSION}",
                        definition.source_id, definition.version
                    )));
                }
                Ok((id.to_owned(), definition))
            })
            .collect::<Result<BTreeMap<_, _>, _>>()?;
        let last_market_generation = state
            .as_ref()
            .map_or(0, |state| state.last_market_generation);
        let last_now_ns = state.as_ref().and_then(|state| state.last_now_ns);
        let settlement_window_pause = state
            .as_ref()
            .and_then(|state| state.settlement_window_pause.clone());
        // Restore the E37b high-water mark. A v7 checkpoint carries the last
        // published close, so a resume after the close does not re-publish the
        // final decision. Absent (a v5/v6 checkpoint, ruling N2), it defaults to
        // `None` — safe, because those predate this feature and so recorded no
        // final decision to replay.
        let settlement_final_close_ms = state
            .as_ref()
            .and_then(|state| state.settlement_final_close_ms);
        // Restore the E37c pending final decision. A v8 checkpoint written in the
        // gap carries the departed market's frozen decision, so a restart releases
        // it exactly once when the completing frame arrives — the high-water mark
        // above guards a restart that happens after the release. Absent (a
        // pre-v8 checkpoint, ruling N2), it defaults to `None`.
        let pending_final_decision = state
            .as_ref()
            .and_then(|state| state.pending_final_decision.clone());
        // The streak is the one identity that reads across windows, so it is
        // restored rather than re-warmed. A checkpoint without the field (any
        // state written before C3b) restores as an empty history.
        let window_streak = state
            .as_ref()
            .and_then(|state| state.window_streak.as_ref())
            .map_or_else(WindowStreak::default, WindowStreak::from_checkpoint);
        let reference = match &state {
            Some(state) => SpotReferenceProcessor::restore(
                publisher_epoch.clone(),
                wall_start,
                state.spot_reference.clone(),
            )?,
            None => SpotReferenceProcessor::new(publisher_epoch.clone(), wall_start)?,
        };
        let mut processor = Self {
            venue,
            publisher_epoch,
            wall_start,
            emitters,
            source_definitions,
            reference,
            volatility: RealizedVolatility::new(),
            latest_reference: None,
            latest_volatility: None,
            prob_ring: ProbabilityHorizonRing::new(probability_horizon_spec(venue)),
            active: None,
            last_market_generation,
            affirmative_quote: None,
            negative_quote: None,
            affirmative_quote_fault: None,
            negative_quote_fault: None,
            last_trade: None,
            books: PredictionBooks::default(),
            flow: PredictionFlow::default(),
            twap_by_time: BTreeMap::new(),
            pair_cost: PairCostCandidate::with_defaults(venue),
            repricing_lag: VenueRepricingLagCandidate::with_default_threshold(),
            tie_rule: TieRuleContextCandidate::with_default_threshold(),
            settlement_anchor: SettlementAnchorStateCandidate::with_defaults(),
            oracle_divergence: OracleDivergenceCandidate::with_default_threshold(),
            source_ordering: BTreeMap::new(),
            last_now_ns,
            last_input_receipt_ns: None,
            settlement_window_pause,
            settlement_final_close_ms,
            index_armed,
            cfbrti_emitter,
            cf_anchor: None,
            cf_previous_publish_time: None,
            cf_fault: None,
            pending_global_updates: Vec::new(),
            cf_settlement_window: None,
            cf_ever_warm: false,
            cf_disconnected: false,
            cf_subscription_reason: None,
            pending_final_decision,
            pending_departed_updates: Vec::new(),
            expired_final_decisions: 0,
            probability: ProbabilitySeries::new(),
            tick_velocity: TickVelocity::new(),
            window_streak,
        };
        // C5: on the gap-free rotated-replay path, rebuild the active binding from
        // the checkpoint by re-parsing the retained contract event — the same
        // parse `bind_market` runs — so market-relative events after the boundary
        // are accepted without a fresh Bind. Deterministic, so the reconstructed
        // contract is byte-identical to the live one. The live crash-resume path
        // (rebuild_active = false) leaves `active = None` and re-discovers.
        if rebuild_active
            && let Some(binding) = state
                .as_ref()
                .and_then(|state| state.active_binding.clone())
        {
            let active = match processor.venue {
                PredictionMarketVenue::Polymarket => {
                    processor.polymarket_contract(binding.generation, &binding.contract_event)?
                }
                PredictionMarketVenue::Kalshi => {
                    processor.kalshi_contract(binding.generation, &binding.contract_event)?
                }
            };
            processor.active = Some(active);
        }
        Ok(processor)
    }

    pub fn venue(&self) -> PredictionMarketVenue {
        self.venue
    }

    /// Whether the Kalshi index socket is armed for this processor (E43). The
    /// runtime reads it to size the completeness guard and to decide whether the
    /// global oracle reference is published.
    pub fn index_armed(&self) -> bool {
        self.index_armed
    }

    pub fn publisher_epoch(&self) -> &str {
        &self.publisher_epoch
    }

    /// Restart every published identity's output sequence at zero for a new file
    /// generation (C5 Design B), keeping the warm feature/ring state intact. Each
    /// file generation is a fresh output epoch, and the contract requires the
    /// first ready decision in an epoch to be sequence zero, so the in-process
    /// rotation paths (the pipeline's `rotate` and its replay twin
    /// `rotate_replay`) call this on the warm processor they carry across the
    /// boundary. It resets exactly the counters a resume's fresh rebuild starts at
    /// zero for every identity this lane writes — every market emitter and the
    /// global oracle-reference emitter — so a live rotated generation and an
    /// independent replay of its journal number their decisions identically. The
    /// embedded spot reference is a decision-time input this lane never publishes,
    /// so its sequence is not part of the output and is left warm. The warm
    /// feature memory, lifetime counts and coalescing state are untouched.
    pub fn reset_output_sequences(&mut self) {
        for emitter in self.emitters.values_mut() {
            emitter.reset_output_sequence();
        }
        if let Some(emitter) = self.cfbrti_emitter.as_mut() {
            emitter.reset_output_sequence();
        }
    }

    pub fn definitions(&self) -> impl Iterator<Item = &SignalDefinition> {
        // The global oracle reference is a separate emitter, not a market-scoped
        // one, but its declared interval must reach the runtime's interval table
        // like every other identity, so it is chained here when armed.
        self.emitters
            .values()
            .map(|emitter| &emitter.definition)
            .chain(
                self.cfbrti_emitter
                    .as_ref()
                    .map(OracleReferenceEmitter::definition),
            )
    }

    /// Every consumed source definition for this venue, each carrying its declared
    /// liveness contract. The feed-health board derives per-feed liveness bounds
    /// from these without duplicating any `expected_heartbeat_ms` value.
    pub fn source_definitions(&self) -> impl Iterator<Item = &SourceDefinition> {
        // Chain the two ticker contracts hosted by the embedded spot reference at
        // their own published versions so feed-health and the contract fingerprint
        // cover them (ADR 0016); they never join `source_documents`.
        self.source_definitions
            .values()
            .chain(self.reference.source_definitions())
    }

    pub fn active_market_id(&self) -> Option<&str> {
        self.active.as_ref().map(|active| active.market_id.as_str())
    }

    pub fn active_market_generation(&self) -> Option<u64> {
        self.active.as_ref().map(|active| active.generation)
    }

    pub fn last_market_generation(&self) -> u64 {
        self.last_market_generation
    }

    /// Whether a pending E37c final decision is currently frozen for the given
    /// departed market generation — i.e. this roll's `mark_unavailable` just
    /// filled the slot for it (a direct bind-over-bind is refused by
    /// `validate_market_binding`, so it never fills one). The roll uses this to
    /// DEFER the two settlement identities' invalidation until that final decision
    /// releases or its slot expires (ruling E37c/M1); a generation that froze no
    /// slot invalidates them at the roll as before.
    pub fn pending_final_decision_for_generation(&self, market_generation: u64) -> bool {
        self.pending_final_decision
            .as_ref()
            .is_some_and(|slot| slot.market_generation == market_generation)
    }

    /// The departed market scope (`market_id`, `market_generation`) of a
    /// currently frozen E37c pending final-decision slot, if one is present.
    /// After a restart in the gap the runtime reconstructs its deferred
    /// settlement-twin invalidation from this — the slot is checkpointed but the
    /// runtime's `pending_departed_invalidation` is not — so the departed final
    /// ready decision that releases post-restart is still followed by the twin
    /// invalidation, exactly as in-process (ruling M1).
    #[must_use]
    pub fn pending_final_decision_scope(&self) -> Option<(&str, u64)> {
        self.pending_final_decision
            .as_ref()
            .map(|slot| (slot.market_id.as_str(), slot.market_generation))
    }

    pub fn reserve_next_generation(&self) -> Result<u64, PredictionSignalError> {
        self.last_market_generation
            .checked_add(1)
            .ok_or(PredictionSignalError::Counter)
    }

    pub fn accepts(&self, source_id: &str) -> bool {
        self.source_definitions.contains_key(source_id)
    }

    /// The market-scoped and index source ids this processor consumes (the keys
    /// of its own source-definition set — the reference tickers the internally
    /// hosted spot reference admits are deliberately excluded, matching
    /// [`Self::accepts`]). The prediction runtime uses this to compute
    /// observed-not-consumed routing against its declared streams (F1): a source
    /// a declared stream produces that is not consumed is routed past the
    /// processor and counted, never a fatal `UnsupportedSource` (E53 follow-up).
    #[must_use]
    pub fn consumed_source_ids(&self) -> std::collections::BTreeSet<String> {
        self.source_definitions.keys().cloned().collect()
    }

    pub fn bind_market(
        &mut self,
        generation: u64,
        event: &SourceEvent,
        logical_receipt_ns: u64,
        now_ns: u64,
    ) -> Result<(), PredictionSignalError> {
        self.validate_market_binding(generation, event, logical_receipt_ns, now_ns)?;
        self.observe_time(now_ns)?;
        let active = match self.venue {
            PredictionMarketVenue::Polymarket => self.polymarket_contract(generation, event)?,
            PredictionMarketVenue::Kalshi => self.kalshi_contract(generation, event)?,
        };
        // Each discovery generation creates a new market adapter and therefore
        // a new source-sequence scope. Generation validation is the contract
        // admission guard; retaining the contract event's sequence here would
        // be dead state because the market-relative scope is reset immediately.
        // Defensive: freeze any pending E37c final decision for a departing
        // market before its inputs are cleared. In practice this is a no-op —
        // `validate_market_binding` refuses a bind while a market is active, so
        // a direct bind-over-bind never reaches here and the scheduled rollover
        // always snapshots in `mark_unavailable` first (no departing active).
        self.snapshot_pending_final_decision(now_ns)?;
        self.clear_market_state();
        self.flow.reset(now_ns);
        self.last_market_generation = generation;
        self.active = Some(active);
        self.note_input(logical_receipt_ns);
        self.attach_opening_twap()?;
        Ok(())
    }

    /// Validate every fallible contract property before a caller persists the
    /// corresponding bound transition.
    pub fn validate_market_binding(
        &self,
        generation: u64,
        event: &SourceEvent,
        logical_receipt_ns: u64,
        now_ns: u64,
    ) -> Result<(), PredictionSignalError> {
        if self.active.is_some() {
            return Err(PredictionSignalError::State(
                "a market bind is valid only while the venue is unbound".to_owned(),
            ));
        }
        if self.last_now_ns.is_some_and(|previous| now_ns < previous) {
            return Err(PredictionSignalError::ClockRegression);
        }
        self.validate_receipt(logical_receipt_ns, now_ns)?;
        let expected_generation = self.reserve_next_generation()?;
        if generation != expected_generation {
            return Err(PredictionSignalError::Generation {
                expected: expected_generation,
                actual: generation,
            });
        }
        let expected_source = match self.venue {
            PredictionMarketVenue::Polymarket => POLYMARKET_CONTRACT_SOURCE_ID,
            PredictionMarketVenue::Kalshi => KALSHI_CONTRACT_SOURCE_ID,
        };
        if event.source_id != expected_source {
            return Err(PredictionSignalError::UnsupportedSource(
                event.source_id.clone(),
            ));
        }
        self.validate_source_event(event)?;
        match self.venue {
            PredictionMarketVenue::Polymarket => self.polymarket_contract(generation, event)?,
            PredictionMarketVenue::Kalshi => self.kalshi_contract(generation, event)?,
        };
        Ok(())
    }

    pub fn ingest(
        &mut self,
        event: &SourceEvent,
        logical_receipt_ns: u64,
        now_ns: u64,
    ) -> Result<(), PredictionSignalError> {
        self.observe_time(now_ns)?;
        self.validate_receipt(logical_receipt_ns, now_ns)?;
        // Ticker admissions feed the internally hosted spot reference (ADR 0016).
        // They are not market-scoped and bypass the generation/identity guards; the
        // reference validates their contract, version, and ordering itself.
        if SpotReferenceProcessor::accepts(&event.source_id) {
            self.reference.ingest(event, logical_receipt_ns, now_ns)?;
            return Ok(());
        }
        if !self.accepts(&event.source_id) {
            return Err(PredictionSignalError::UnsupportedSource(
                event.source_id.clone(),
            ));
        }
        self.validate_source_event(event)?;
        self.validate_source_order(event)?;
        if event.source_id == POLYMARKET_TWAP_SOURCE_ID {
            return self.ingest_twap(event);
        }
        // The CF BRTI value channel is a global index, not a market-scoped venue
        // input: like the opening TWAP it is admitted before the market-identity
        // and venue-liveness checks and it survives a market handoff.
        if event.source_id == KALSHI_BRTI_VALUE_SOURCE_ID {
            return self.ingest_cf_value(event);
        }
        // The settlement-window channel carries the completeness the settlement
        // twins gate on (E37). Global-scoped like the value channel: admitted
        // before the market-identity and liveness checks and it survives a
        // market handoff.
        if event.source_id == KALSHI_BRTI_SETTLEMENT_SOURCE_ID {
            return self.ingest_settlement_window(event);
        }
        if matches!(
            event.source_id.as_str(),
            POLYMARKET_CONTRACT_SOURCE_ID | KALSHI_CONTRACT_SOURCE_ID
        ) {
            return Err(PredictionSignalError::State(
                "market-contract source events must enter through bind_market".to_owned(),
            ));
        }
        self.check_market_identity(event)?;
        // Book, quote, and trade admissions are venue-liveness evidence; the TWAP
        // benchmark relay above is not, so it is excluded from aliveness.
        self.note_input(logical_receipt_ns);
        match event.source_id.as_str() {
            POLYMARKET_QUOTE_SOURCE_ID | KALSHI_QUOTE_SOURCE_ID => {
                self.ingest_quote(event, logical_receipt_ns)
            }
            POLYMARKET_BOOK_SOURCE_ID | KALSHI_BOOK_SOURCE_ID => {
                self.books.ingest(self.venue, event, logical_receipt_ns)
            }
            POLYMARKET_TRADE_SOURCE_ID | KALSHI_TRADE_SOURCE_ID => {
                self.ingest_trade(event, logical_receipt_ns)
            }
            other => Err(PredictionSignalError::UnsupportedSource(other.to_owned())),
        }
    }

    pub fn mark_unavailable(
        &mut self,
        now_ns: u64,
        reason: impl Into<String>,
    ) -> Result<Vec<SignalUpdate>, PredictionSignalError> {
        self.observe_time(now_ns)?;
        let reason = reason.into();
        if reason.is_empty() || reason.len() > 128 {
            return Err(PredictionSignalError::State(
                "unavailable reason must contain 1 to 128 bytes".to_owned(),
            ));
        }
        // Freeze any pending E37c final decision for the departing market before
        // `active` and its inputs are cleared by the rollover.
        self.snapshot_pending_final_decision(now_ns)?;
        self.active = None;
        self.clear_market_state();
        self.flow.clear();
        // A venue known unavailable must degrade immediately: the aliveness
        // timestamp is cleared so the quiet-window rule cannot ride a residual
        // pre-disconnect admission for up to the bound (SIG-046).
        self.last_input_receipt_ns = None;
        // Unbound venue state belongs to market_transition in stream v3: this
        // produces no evaluation. The departing generation's bound instances are
        // invalidated by the publication ledger separately — at the roll, or, when
        // this call has just frozen an E37c final decision, with the two settlement
        // identities deferred to the departed path (ruling E37c/M1) — never as an
        // unscoped signal here.
        Ok(Vec::new())
    }

    /// Re-emit every signal at an admitted market-input or bind time. The embedded
    /// spot reference is re-evaluated for staleness (refreshing S), but the
    /// realized-volatility ring is NOT sampled here: sampling is health-tick-only,
    /// matching the published spot pipeline, so sigma is a one-second cadence.
    pub fn advance_time(&mut self, now_ns: u64) -> Result<PredictionBatch, PredictionSignalError> {
        self.observe_time(now_ns)?;
        self.flow.prune(now_ns);
        if let Some(update) = self.reference.advance_time(now_ns)? {
            self.latest_reference = Some(update);
        }
        // The global oracle reference is not market-scoped: drive it on every
        // decision before the unbound early return, so an unbound Kalshi lane
        // still publishes the CF BRTI reading (§5.1).
        self.drive_cfbrti(now_ns)?;
        // Release or expire a pending E37c final decision before the unbound
        // early return, so a completing frame that lands while the lane is
        // momentarily unbound between the rollover and the successor bind is not
        // missed.
        self.drive_pending_final_decision(now_ns)?;
        if self.active.is_none() {
            return Ok(PredictionBatch::default());
        }
        // E11: fix the opening strike from a fallback frame once the exact-boundary
        // TWAP frame has missed its `window_open + 2 s` grace. The market still
        // publishes Ready; `opening_strike_source` on every strike-derived identity
        // tells consumers which frame the strike came from.
        self.resolve_opening_strike(now_ns)?;
        self.emit_all_with_base_reason(now_ns, None)
    }

    /// Drive the embedded spot pipeline on the one-second health tick: re-evaluate
    /// the reference, feed its output to the realized-volatility ring (one sample
    /// per tick, exactly as the published spot pipeline does), refresh sigma, and
    /// re-emit every signal.
    pub fn health_tick(&mut self, now_ns: u64) -> Result<PredictionBatch, PredictionSignalError> {
        self.observe_time(now_ns)?;
        self.flow.prune(now_ns);
        if let Some(update) = self.reference.advance_time(now_ns)? {
            self.volatility.update(&update, now_ns)?;
            self.latest_reference = Some(update);
        }
        self.latest_volatility = self.volatility.metrics()?;
        // Global-scope: driven before the unbound early return, like advance_time.
        self.drive_cfbrti(now_ns)?;
        // E37c release/expiry, also before the unbound early return.
        self.drive_pending_final_decision(now_ns)?;
        if self.active.is_none() {
            return Ok(PredictionBatch::default());
        }
        // C2: sample this generation's own prediction_quote affirmative_midpoint
        // into the probability ring at the one-second health cadence (the same
        // cadence the realized-volatility ring uses). A non-ready quote is a
        // missing observation that breaks continuity and restarts the warm-up.
        let probability_sample = self.sample_affirmative_midpoint(now_ns)?;
        self.prob_ring.update_price(probability_sample, now_ns);
        // E11: see `advance_time` — fix a fallback strike after the grace; the
        // market stays Ready with `opening_strike_source` marking the provenance.
        self.resolve_opening_strike(now_ns)?;
        self.emit_all_with_base_reason(now_ns, None)
    }

    /// Drive the global `btc.cfbrti.oracle_reference` emitter one decision. A
    /// no-op unless the Kalshi index socket is armed. The reading is the raw
    /// index value (not the settlement average), staleness publishes as a
    /// `freshness` value, and the coalescing emitter suppresses an unchanged
    /// restatement until its 1 Hz interval falls due (§5.1). Any produced update
    /// is stashed for the runtime to write outside the market completeness guard.
    fn drive_cfbrti(&mut self, now_ns: u64) -> Result<(), PredictionSignalError> {
        if self.cfbrti_emitter.is_none() {
            return Ok(());
        }
        let produced_at = self.logical_timestamp(now_ns)?;
        let (ready_values, reasons) = self.cfbrti_reading(&produced_at)?;
        let clock = LaneClock {
            now_ns,
            produced_at,
        };
        let publisher_epoch = self.publisher_epoch.clone();
        let emitter = self
            .cfbrti_emitter
            .as_mut()
            .expect("cfbrti emitter present after the guard above");
        let update = emitter
            .emit(&publisher_epoch, &clock, ready_values, reasons)
            .map_err(|error| {
                PredictionSignalError::State(format!(
                    "cfbrti oracle reference emit failed: {error}"
                ))
            })?;
        // Append rather than overwrite (F4): a decision that produced a global
        // update must never clobber one an earlier drive in the same admission
        // left undrained. The runtime drains after each admission, so the vector
        // is normally short-lived; the invariant is that nothing is silently lost
        // if two drives land before a drain.
        self.pending_global_updates.extend(update);
        Ok(())
    }

    /// The oracle-reference reading for this decision: `None` values with a
    /// reason before an accepted frame, under a value fault, or while the index
    /// channel is unavailable (subscription refused, socket dropped after
    /// warm-up, or an unrepaired gap — F1/F2); otherwise the venue-parameterised
    /// oracle-reference values built from the latest accepted index observation.
    /// Neither the thin-window guard nor the staleness bound is applied here — a
    /// below-full trailing window is a published field and staleness is a
    /// published `freshness` value, not a non-ready state (§6, the oracle-vs-
    /// settlement asymmetry); those degrade only the settlement family.
    fn cfbrti_reading(&self, produced_at: &str) -> Result<CfbrtiReading, PredictionSignalError> {
        if let Some(reason) = self.cf_index_unavailable_reason() {
            return Ok((None, vec![reason.to_owned()]));
        }
        if let Some(fault) = self.cf_fault {
            return Ok((None, vec![fault.to_owned()]));
        }
        let Some(anchor) = self.cf_anchor.as_ref() else {
            return Ok((None, vec!["cfbrti_warming_up".to_owned()]));
        };
        let extra = BTreeMap::from([
            (
                "avg_60s_value".to_owned(),
                decimal_value(anchor.avg_60s_value),
            ),
            (
                "avg_60s_window_size".to_owned(),
                json!(anchor.avg_60s_window_size),
            ),
            ("index_id".to_owned(), json!(anchor.index_id)),
        ]);
        let values = oracle_reference_values(
            anchor.index_value,
            &anchor.publish_time,
            produced_at,
            self.cf_previous_publish_time.as_deref(),
            CFBRTI_STALENESS_THRESHOLD_MS,
            extra,
        )
        .map_err(|error| {
            PredictionSignalError::State(format!("cfbrti oracle reference values failed: {error}"))
        })?;
        Ok((Some(values), Vec::new()))
    }

    /// Drain the global-scope updates produced by the most recent decision (the
    /// oracle reference). The runtime writes these outside the per-market
    /// completeness guard, since a global identity is not part of a venue's
    /// market-scoped batch.
    pub fn take_global_updates(&mut self) -> Vec<SignalUpdate> {
        std::mem::take(&mut self.pending_global_updates)
    }

    /// Drain the departed-market final decision produced by the most recent
    /// release (ruling E37c). Written by the runtime outside the per-market
    /// completeness guard, exactly like the global oracle reference, since the
    /// pair is scoped to the departed market rather than the bound one.
    pub fn take_departed_updates(&mut self) -> Vec<SignalUpdate> {
        std::mem::take(&mut self.pending_departed_updates)
    }

    /// Drain the count of pending final decisions that expired unpublished since
    /// the last drain (ruling E37c), for the runtime to record in feed health.
    pub fn take_expired_final_decisions(&mut self) -> u32 {
        std::mem::take(&mut self.expired_final_decisions)
    }

    /// Freeze a departed Kalshi market's final settlement decision before the
    /// rollover clears its inputs (ruling E37c). A slot is filled only when the
    /// market was one tick from a real final ready decision: an armed Kalshi
    /// lane, a ready contract with a strike, a readable settled anchor and spot
    /// reference, and a quarter whose final decision has NOT already published.
    /// The completing settlement-window frame for the quarter may land after the
    /// scheduled rollover; [`Self::drive_pending_final_decision`] releases the
    /// decision when it does, or lets the slot expire.
    ///
    /// Called with the departing market still `active`: from `mark_unavailable`
    /// before `active` is cleared, and from `bind_market` before
    /// `clear_market_state`. Idempotent across the pair — the second call sees no
    /// departing active (an intervening `mark_unavailable` already cleared it) or
    /// an already-filled slot and does nothing.
    fn snapshot_pending_final_decision(
        &mut self,
        now_ns: u64,
    ) -> Result<(), PredictionSignalError> {
        if self.pending_final_decision.is_some() {
            return Ok(());
        }
        if self.venue != PredictionMarketVenue::Kalshi || !self.index_armed {
            return Ok(());
        }
        let Some(active) = self.active.as_ref() else {
            return Ok(());
        };
        let Some(threshold) = active.strike else {
            return Ok(());
        };
        let quarter_close_ms = active.parsed_close.timestamp_millis();
        // The final decision for this quarter already published before the roll
        // (E37b fired on a completing tick that beat the rollover): nothing pends.
        if self
            .settlement_final_close_ms
            .is_some_and(|published| quarter_close_ms <= published)
        {
            return Ok(());
        }
        // A slot only matters if a ready settlement decision was pending: the
        // settled anchor and the composite spot reference must both be readable
        // at the roll, else no final ready decision would ever have published.
        let produced_at = self.logical_timestamp(now_ns)?;
        let mut anchor_reasons = Vec::new();
        let Some(anchor_price) =
            self.settlement_anchor_reading(&produced_at, &mut anchor_reasons)?
        else {
            return Ok(());
        };
        let mut reference_reasons = Vec::new();
        let Some((spot_reference_mid, _)) = self.reference_state(&mut reference_reasons)? else {
            return Ok(());
        };
        // Validate the rules-card comparator/tie now so a slot never carries an
        // unsettleable contract; store the comparator in its canonical wire form
        // and the tie in the venue's own vocabulary, both re-decoded on release
        // through the same helpers a live decision uses.
        let comparator = settlement_comparator(active.comparator)?;
        let _ = settled_tie_side(self.venue, active.tie_outcome)?;
        self.pending_final_decision = Some(PendingFinalDecisionState {
            market_id: active.market_id.clone(),
            market_generation: active.generation,
            quarter_close_time: active.window_close.clone(),
            quarter_close_ms,
            deadline_ns: now_ns.saturating_add(PENDING_FINAL_DECISION_RETENTION_NS),
            anchor_price: anchor_price.to_string(),
            threshold: threshold.to_string(),
            spot_reference_mid: spot_reference_mid.to_string(),
            comparator: comparator.as_str().to_owned(),
            tie_outcome: active.tie_outcome.to_owned(),
        });
        Ok(())
    }

    /// Release or expire a pending E37c final decision. Driven on every decision
    /// before the unbound early return — like [`Self::drive_cfbrti`] — so a
    /// completing frame that arrives while the lane is momentarily unbound
    /// between the rollover and the successor bind still releases the decision.
    ///
    /// When the completing settlement-window frame for the pending quarter has
    /// arrived (`window_as_of == quarter_close`, full window, matching close),
    /// both twins publish one final ready decision scoped to the DEPARTED market
    /// with `settlement_window_complete: true`; the high-water mark advances so a
    /// later tick or a restart never re-publishes, and the slot clears. Past the
    /// retention deadline the slot expires unpublished and is counted for feed
    /// health.
    fn drive_pending_final_decision(&mut self, now_ns: u64) -> Result<(), PredictionSignalError> {
        let Some(slot) = self.pending_final_decision.as_ref() else {
            return Ok(());
        };
        // Expiry is tested BEFORE completeness: once the retention deadline has
        // passed, a completing frame that only now arrives is too late — the slot
        // expires (counted) rather than releasing an arbitrarily late "final"
        // decision. Checking completeness first would let a frame at, say, +5.2 s
        // publish a decision the deadline had already retired.
        if now_ns >= slot.deadline_ns {
            self.pending_final_decision = None;
            self.expired_final_decisions = self.expired_final_decisions.saturating_add(1);
            return Ok(());
        }
        let complete_for_quarter = self.cf_settlement_window.as_ref().is_some_and(|window| {
            window.is_complete() && window.quarter_close_ms == slot.quarter_close_ms
        });
        if !complete_for_quarter {
            return Ok(());
        }
        let slot = self
            .pending_final_decision
            .take()
            .expect("pending final decision present under the guard above");
        let produced_at = self.logical_timestamp(now_ns)?;
        let scope = SignalScope::Market {
            market_id: slot.market_id.clone(),
            market_generation: slot.market_generation,
        };
        let anchor_price = parse_decimal_text(&slot.anchor_price)?;
        let threshold = parse_decimal_text(&slot.threshold)?;
        let spot_reference_mid = parse_decimal_text(&slot.spot_reference_mid)?;
        let comparator = settlement_comparator(&slot.comparator)?;
        let tie_outcome = settled_tie_side(self.venue, &slot.tie_outcome)?;
        // The completing window, reconstructed from the frozen quarter close so
        // the published `window_as_of`/`quarter_close_time`/completeness fields
        // describe the state AT the close (ruling E37c), independent of the frame
        // whose arrival triggered the release.
        let window = CfSettlementWindow {
            quarter_close_time: slot.quarter_close_time.clone(),
            quarter_close_ms: slot.quarter_close_ms,
            window_as_of: slot.quarter_close_time.clone(),
            window_as_of_ms: slot.quarter_close_ms,
            window_size: CFBRTI_FULL_WINDOW_SIZE,
        };

        let settled = self.settlement_anchor.settled_outcome(
            anchor_price,
            threshold,
            comparator,
            tie_outcome,
        )?;
        // C3b: the departed quarter's settled side joins the streak on release
        // (E37c), under the same exactly-once quarter-close guard.
        self.window_streak.record(
            slot.quarter_close_ms,
            settled.side_if_settled_now == SettledSide::Up,
        );
        let mut anchor_values = BTreeMap::from([
            (
                "anchor_price".to_owned(),
                decimal_value(settled.anchor_price),
            ),
            ("threshold".to_owned(), decimal_value(settled.threshold)),
            (
                "distance_bps".to_owned(),
                decimal_value(settled.distance_bps),
            ),
            (
                "time_to_close_ms".to_owned(),
                json!(settled.time_to_close_ms),
            ),
            (
                "comparator".to_owned(),
                json!(Comparator::GreaterThanOrEqual.as_str()),
            ),
            (
                "side_if_settled_now".to_owned(),
                json!(settled.side_if_settled_now.as_str()),
            ),
            ("state".to_owned(), json!(settled.state.as_str())),
            (
                "pin_threshold_bps".to_owned(),
                decimal_value(self.settlement_anchor.pin_threshold_bps()),
            ),
            (
                "pin_window_ms".to_owned(),
                json!(self.settlement_anchor.pin_window_ms()),
            ),
            ("observation_time".to_owned(), json!(produced_at)),
        ]);
        insert_settlement_window_fields(&mut anchor_values, Some(&window));

        // The divergence twin recomputes against the anchor and the FROZEN spot
        // mid on a throwaway candidate, so the live candidate the successor market
        // uses is never perturbed by the departed decision.
        let mut divergence_candidate =
            OracleDivergenceCandidate::new(self.oracle_divergence.diverged_threshold_bps());
        divergence_candidate.observe_anchor(anchor_price);
        let divergence_outcome = divergence_candidate.observe_spot_reference(spot_reference_mid);
        let DivergenceOutcome::Ready(divergence) = divergence_outcome else {
            return Err(PredictionSignalError::State(
                "departed oracle divergence is not ready from frozen inputs".to_owned(),
            ));
        };
        let mut divergence_values = BTreeMap::from([
            (
                "divergence_bps".to_owned(),
                decimal_value(divergence.divergence_bps),
            ),
            (
                "anchor_price".to_owned(),
                decimal_value(divergence.anchor_price),
            ),
            (
                "spot_reference_mid".to_owned(),
                decimal_value(divergence.spot_reference_mid),
            ),
            ("state".to_owned(), json!(divergence.state.as_str())),
            (
                "diverged_threshold_bps".to_owned(),
                decimal_value(self.oracle_divergence.diverged_threshold_bps()),
            ),
            ("observation_time".to_owned(), json!(produced_at)),
        ]);
        insert_settlement_window_fields(&mut divergence_values, Some(&window));

        let anchor_emission = self.emit_scoped(
            PredictionSignalKind::SettlementAnchorState,
            &produced_at,
            now_ns,
            produced_at.clone(),
            Some(anchor_values),
            Vec::new(),
            false,
            scope.clone(),
        )?;
        let divergence_emission = self.emit_scoped(
            PredictionSignalKind::OracleDivergence,
            &produced_at,
            now_ns,
            produced_at.clone(),
            Some(divergence_values),
            Vec::new(),
            false,
            scope,
        )?;
        let mut released = Vec::new();
        for emission in [anchor_emission, divergence_emission] {
            released.extend(PredictionBatch::from_emissions(vec![emission]).into_published());
        }
        self.pending_departed_updates.extend(released);
        // Advance the high-water mark so a later tick or a restart never
        // re-publishes this quarter's final decision.
        self.settlement_final_close_ms = Some(slot.quarter_close_ms);
        Ok(())
    }

    pub fn checkpoint_state(&self) -> PredictionSignalCheckpointState {
        PredictionSignalCheckpointState {
            venue: self.venue,
            last_market_generation: self.last_market_generation,
            active_market_id: self.active.as_ref().map(|active| active.market_id.clone()),
            features: self
                .emitters
                .values()
                .map(|emitter| {
                    (
                        emitter.definition.signal_id.clone(),
                        PredictionFeatureCheckpoint {
                            lifetime_output_count: emitter.lifetime_output_count,
                            ever_ready: emitter.ever_ready,
                            last_quality: emitter.last_quality.clone(),
                            quality_reason_entries: emitter.quality_reason_entries.clone(),
                        },
                    )
                })
                .collect(),
            last_now_ns: self.last_now_ns,
            spot_reference: self.reference.checkpoint_state(),
            settlement_window_pause: self.settlement_window_pause.clone(),
            settlement_final_close_ms: self.settlement_final_close_ms,
            pending_final_decision: self.pending_final_decision.clone(),
            window_streak: self.window_streak.checkpoint(),
            active_binding: self.active.as_ref().map(|active| ActiveBindingCheckpoint {
                generation: active.generation,
                contract_event: active.contract_event.clone(),
            }),
        }
    }

    /// The withheld Kalshi settlement window, if one is outstanding.
    pub fn settlement_window_pause(&self) -> Option<&SettlementWindowPause> {
        self.settlement_window_pause.as_ref()
    }

    /// Withhold the settlement window for a quarter until a repair covers the
    /// named interval. Re-pausing widens the outstanding pause to cover every
    /// unrepaired quarter: it keeps the LATEST quarter close — which, under the
    /// `<=` withholding rule, subsumes every earlier gap — and unions the repair
    /// interval, so a second, later gap can never escape the withhold while an
    /// earlier one is still outstanding.
    pub fn pause_settlement_window(
        &mut self,
        pause: SettlementWindowPause,
    ) -> Result<(), PredictionSignalError> {
        pause.validate()?;
        let widened = match self.settlement_window_pause.take() {
            Some(existing) => existing.widen(pause),
            None => pause,
        };
        self.settlement_window_pause = Some(widened);
        Ok(())
    }

    /// Release the pause once a repair has closed it.
    pub fn resume_settlement_window(&mut self) {
        self.settlement_window_pause = None;
    }

    /// Whether a settlement-window event for `quarter_close_time` must be
    /// withheld rather than admitted.
    #[must_use]
    pub fn withholds_settlement_window(&self, quarter_close_time: i64) -> bool {
        self.settlement_window_pause
            .as_ref()
            .is_some_and(|pause| pause.withholds(quarter_close_time))
    }

    pub fn quality_reason_entries(&self) -> BTreeMap<String, u64> {
        let mut entries = BTreeMap::new();
        for emitter in self.emitters.values() {
            for (reason, count) in &emitter.quality_reason_entries {
                entries.insert(format!("{}:{reason}", emitter.definition.signal_id), *count);
            }
        }
        entries
    }

    fn polymarket_contract(
        &self,
        generation: u64,
        event: &SourceEvent,
    ) -> Result<ActiveContract, PredictionSignalError> {
        if required_text(&event.values, "resolution_source")?
            != "https://data.chain.link/streams/btc-usd-twap-60s-streams"
            || required_text(&event.values, "comparator")? != "greater_than_or_equal"
            || required_text(&event.values, "tie_outcome")? != "up"
        {
            return Err(PredictionSignalError::MarketValue(
                "Polymarket rules card does not match the pinned Chainlink >= Up contract"
                    .to_owned(),
            ));
        }
        let window_open = required_text(&event.values, "window_open")?.to_owned();
        let window_close = required_text(&event.values, "window_close")?.to_owned();
        let parsed_open = parse_utc(&window_open, "window_open")?;
        let parsed_close = parse_utc(&window_close, "window_close")?;
        let duration = parsed_close
            .signed_duration_since(parsed_open)
            .num_seconds();
        if !matches!(duration, 300 | 900) {
            return Err(PredictionSignalError::MarketValue(
                "Polymarket contract window must be five or fifteen minutes".to_owned(),
            ));
        }
        Ok(ActiveContract {
            market_id: required_text(&event.values, "market_id")?.to_owned(),
            routing_id: required_text(&event.values, "condition_id")?.to_owned(),
            generation,
            window_open,
            window_close,
            parsed_open,
            parsed_close,
            strike: None,
            opening_strike_source: None,
            settlement_benchmark: "chainlink_btc_usd_twap_60s",
            averaging_window_seconds: 60,
            comparator: "greater_than_or_equal",
            tie_outcome: "up",
            affirmative_outcome: "up",
            negative_outcome: "down",
            minimum_tick_size: Some(required_decimal(&event.values, "minimum_tick_size")?),
            minimum_order_size: Some(required_decimal(&event.values, "minimum_order_size")?),
            rules_sha256: required_text(&event.values, "rules_sha256")?.to_owned(),
            contract_time: event.event_time.clone(),
            contract_event: event.clone(),
        })
    }

    fn kalshi_contract(
        &self,
        generation: u64,
        event: &SourceEvent,
    ) -> Result<ActiveContract, PredictionSignalError> {
        if required_text(&event.values, "benchmark")? != "cf_benchmarks_brti"
            || required_u32(&event.values, "averaging_window_seconds")? != 60
            || required_text(&event.values, "comparator")? != "greater_than_or_equal"
            || required_text(&event.values, "tie_outcome")? != "yes"
        {
            return Err(PredictionSignalError::MarketValue(
                "Kalshi rules card does not match the pinned BRTI sixty-print >= YES contract"
                    .to_owned(),
            ));
        }
        let window_open = required_text(&event.values, "open_time")?.to_owned();
        let window_close = required_text(&event.values, "close_time")?.to_owned();
        let parsed_open = parse_utc(&window_open, "open_time")?;
        let parsed_close = parse_utc(&window_close, "close_time")?;
        if parsed_close
            .signed_duration_since(parsed_open)
            .num_seconds()
            != 900
        {
            return Err(PredictionSignalError::MarketValue(
                "Kalshi contract window must be fifteen minutes".to_owned(),
            ));
        }
        Ok(ActiveContract {
            market_id: required_text(&event.values, "market_ticker")?.to_owned(),
            routing_id: required_text(&event.values, "market_ticker")?.to_owned(),
            generation,
            window_open,
            window_close,
            parsed_open,
            parsed_close,
            strike: Some(required_decimal(&event.values, "reference_value")?),
            // Kalshi binds its reference value directly; there is no opening-TWAP
            // boundary observation, so the E11 provenance stays unset and the
            // Polymarket-only `opening_strike_source` field is never emitted here.
            opening_strike_source: None,
            settlement_benchmark: "cf_benchmarks_brti_sixty_print",
            averaging_window_seconds: 60,
            comparator: "greater_than_or_equal",
            tie_outcome: "yes",
            affirmative_outcome: "yes",
            negative_outcome: "no",
            minimum_tick_size: None,
            minimum_order_size: None,
            rules_sha256: required_text(&event.values, "rules_sha256")?.to_owned(),
            contract_time: event.event_time.clone(),
            contract_event: event.clone(),
        })
    }

    fn ingest_twap(&mut self, event: &SourceEvent) -> Result<(), PredictionSignalError> {
        if self.venue != PredictionMarketVenue::Polymarket {
            return Err(PredictionSignalError::UnsupportedSource(
                event.source_id.clone(),
            ));
        }
        if required_text(&event.values, "symbol")? != "btc/usd"
            || required_u32(&event.values, "window_seconds")? != 60
        {
            return Err(PredictionSignalError::MarketValue(
                "Polymarket TWAP identity or window is invalid".to_owned(),
            ));
        }
        let time = parse_utc(&event.event_time, "event_time")?;
        let price = required_decimal(&event.values, "twap_price")?;
        if price <= Decimal::ZERO {
            return Err(PredictionSignalError::MarketValue(
                "Polymarket opening TWAP must be positive".to_owned(),
            ));
        }
        if let Some(existing) = self.twap_by_time.get(&time) {
            if *existing == price {
                return self.attach_opening_twap();
            }
            return Err(PredictionSignalError::State(
                "conflicting Polymarket TWAP values share one observation timestamp".to_owned(),
            ));
        }
        self.twap_by_time.insert(time, price);
        while self.twap_by_time.len() > MAX_RETAINED_TWAP_OBSERVATIONS {
            let first = self.twap_by_time.keys().next().copied().ok_or_else(|| {
                PredictionSignalError::State("TWAP retention map became empty".to_owned())
            })?;
            self.twap_by_time.remove(&first);
        }
        self.attach_opening_twap()
    }

    fn attach_opening_twap(&mut self) -> Result<(), PredictionSignalError> {
        let Some(active) = self.active.as_mut() else {
            return Ok(());
        };
        if self.venue == PredictionMarketVenue::Polymarket
            && active.strike.is_none()
            && let Some(price) = self.twap_by_time.get(&active.parsed_open).copied()
        {
            active.strike = Some(price);
            active.opening_strike_source = Some(OpeningStrikeSource::Exact);
            active.contract_time = active.window_open.clone();
        }
        Ok(())
    }

    /// Resolve a still-unset Polymarket opening strike on the decision clock (E11).
    ///
    /// The primary source is always the exact-boundary TWAP frame; `attach_opening_twap`
    /// takes it the moment it is ingested, and this method takes it too if it is
    /// already retained. When that frame has not arrived by `window_open + 2 s`
    /// (judged on the decision clock, so the fallback still fires if the stream
    /// falls silent after the open), the strike falls back to the last frame with
    /// `event_time <= window_open` within the 5 s before the open, else the first
    /// frame with `event_time > window_open` within the 5 s after it. A fallback
    /// strike is an approximation of the venue's exact-boundary Chainlink TWAP; the
    /// market still publishes Ready and `opening_strike_source` records which frame
    /// it came from (`prior_frame`/`post_frame` vs `exact`) so consumers can
    /// filter. A strike is fixed exactly once per market: the `strike.is_none()`
    /// guard means a late-arriving exact frame never revises a strike already taken.
    fn resolve_opening_strike(&mut self, now_ns: u64) -> Result<(), PredictionSignalError> {
        let (parsed_open, window_open) = match self.active.as_ref() {
            Some(active)
                if self.venue == PredictionMarketVenue::Polymarket && active.strike.is_none() =>
            {
                (active.parsed_open, active.window_open.clone())
            }
            _ => return Ok(()),
        };
        // The grace is judged on the wall clock, not the logical-ns clock: a
        // market that opened before this process's wall origin (a mid-market
        // start — push-B) has `parsed_open < wall_start`, which `logical_ns`
        // rejects with "market timestamp precedes the process wall origin".
        // Converting `now_ns` UP to wall time is always representable, so the
        // comparison never errors on a legitimately earlier timestamp; when the
        // open already precedes now, the grace has by definition elapsed and the
        // prior/post frames are selected by their own event times below (the
        // BTreeMap ranges never convert through `logical_ns`).
        let grace_elapsed = match (
            self.wall_start
                .checked_add_signed(ChronoDuration::nanoseconds(
                    i64::try_from(now_ns).unwrap_or(i64::MAX),
                )),
            parsed_open.checked_add_signed(ChronoDuration::seconds(2)),
        ) {
            (Some(now_wall), Some(grace_deadline)) => now_wall >= grace_deadline,
            _ => true,
        };
        let resolved = if let Some(price) = self.twap_by_time.get(&parsed_open).copied() {
            Some((price, OpeningStrikeSource::Exact))
        } else if !grace_elapsed {
            None
        } else if let Some((_, &price)) = self
            .twap_by_time
            .range((parsed_open - ChronoDuration::seconds(5))..parsed_open)
            .next_back()
        {
            Some((price, OpeningStrikeSource::PriorFrame))
        } else if let Some((_, &price)) = self
            .twap_by_time
            .range(parsed_open..=(parsed_open + ChronoDuration::seconds(5)))
            .next()
        {
            // The exact-boundary key is absent on this branch, so the range's
            // first entry is the first frame strictly after the open.
            Some((price, OpeningStrikeSource::PostFrame))
        } else {
            None
        };
        if let Some((price, source)) = resolved
            && let Some(active) = self.active.as_mut()
        {
            active.strike = Some(price);
            active.opening_strike_source = Some(source);
            active.contract_time = window_open;
        }
        Ok(())
    }

    /// The wire value of the additive `opening_strike_source` field for the
    /// strike-derived Polymarket identities (E11). `Some` once a Polymarket strike
    /// is fixed (`exact` | `prior_frame` | `post_frame`); `None` on Kalshi (whose
    /// definitions do not declare the field) and before a strike is fixed.
    fn opening_strike_source_wire(&self) -> Option<&'static str> {
        if self.venue != PredictionMarketVenue::Polymarket {
            return None;
        }
        self.active
            .as_ref()
            .and_then(|active| active.opening_strike_source)
            .map(OpeningStrikeSource::as_wire)
    }

    fn ingest_quote(
        &mut self,
        event: &SourceEvent,
        logical_receipt_ns: u64,
    ) -> Result<(), PredictionSignalError> {
        let polymarket_axis = if self.venue == PredictionMarketVenue::Polymarket {
            Some(match required_text(&event.values, "outcome")? {
                "up" => OutcomeAxis::Affirmative,
                "down" => OutcomeAxis::Negative,
                other => {
                    return Err(PredictionSignalError::MarketValue(format!(
                        "unsupported Polymarket quote outcome {other}"
                    )));
                }
            })
        } else {
            None
        };
        let bid = required_decimal(&event.values, "best_bid")?;
        let ask = required_decimal(&event.values, "best_ask")?;
        if bid <= Decimal::ZERO
            || bid > Decimal::ONE
            || ask <= Decimal::ZERO
            || ask > Decimal::ONE
            || bid > ask
        {
            match polymarket_axis {
                Some(OutcomeAxis::Affirmative) => {
                    self.affirmative_quote = None;
                    self.affirmative_quote_fault = Some("quote_crossed_or_invalid".to_owned());
                }
                Some(OutcomeAxis::Negative) => {
                    self.negative_quote = None;
                    self.negative_quote_fault = Some("quote_crossed_or_invalid".to_owned());
                }
                None => {
                    self.affirmative_quote = None;
                    self.negative_quote = None;
                    self.affirmative_quote_fault = Some("quote_crossed_or_invalid".to_owned());
                    self.negative_quote_fault = Some("quote_crossed_or_invalid".to_owned());
                }
            }
            return Ok(());
        }
        let observation = QuoteObservation {
            bid,
            ask,
            logical_receipt_ns,
            event_time: event.event_time.clone(),
        };
        match self.venue {
            PredictionMarketVenue::Polymarket => match polymarket_axis {
                Some(OutcomeAxis::Affirmative) => {
                    self.affirmative_quote = Some(observation);
                    self.affirmative_quote_fault = None;
                }
                Some(OutcomeAxis::Negative) => {
                    self.negative_quote = Some(observation);
                    self.negative_quote_fault = None;
                }
                None => unreachable!("Polymarket quote axis was parsed above"),
            },
            PredictionMarketVenue::Kalshi => {
                let negative_bid = Decimal::ONE.checked_sub(ask).ok_or_else(|| {
                    PredictionSignalError::Arithmetic("NO bid overflowed".to_owned())
                })?;
                let negative_ask = Decimal::ONE.checked_sub(bid).ok_or_else(|| {
                    PredictionSignalError::Arithmetic("NO ask overflowed".to_owned())
                })?;
                self.affirmative_quote = Some(observation.clone());
                self.negative_quote = Some(QuoteObservation {
                    bid: negative_bid,
                    ask: negative_ask,
                    ..observation
                });
                self.affirmative_quote_fault = None;
                self.negative_quote_fault = None;
            }
        }
        // C3b: one admitted quote update for `tick_velocity`, counted once an
        // affirmative midpoint exists to compare against. A quote on either
        // outcome is an update; only a changed affirmative midpoint is a
        // midpoint change.
        if let Some(quote) = self.affirmative_quote.as_ref() {
            let midpoint = quote_midpoint(quote)?;
            self.tick_velocity
                .observe_quote(logical_receipt_ns, midpoint);
        }
        Ok(())
    }

    fn ingest_trade(
        &mut self,
        event: &SourceEvent,
        logical_receipt_ns: u64,
    ) -> Result<(), PredictionSignalError> {
        let (outcome, price, size, positive) = match self.venue {
            PredictionMarketVenue::Polymarket => {
                let outcome = required_text(&event.values, "outcome")?;
                let side = required_text(&event.values, "reported_side")?;
                let positive = matches!((outcome, side), ("up", "buy") | ("down", "sell"));
                if !matches!(outcome, "up" | "down") || !matches!(side, "buy" | "sell") {
                    return Err(PredictionSignalError::MarketValue(
                        "Polymarket trade outcome or reported side is invalid".to_owned(),
                    ));
                }
                (
                    outcome.to_owned(),
                    required_decimal(&event.values, "price")?,
                    required_decimal(&event.values, "size")?,
                    positive,
                )
            }
            PredictionMarketVenue::Kalshi => {
                let outcome = required_text(&event.values, "taker_outcome_side")?;
                let (price, positive) = match outcome {
                    "yes" => (required_decimal(&event.values, "yes_price")?, true),
                    "no" => (required_decimal(&event.values, "no_price")?, false),
                    _ => {
                        return Err(PredictionSignalError::MarketValue(
                            "Kalshi taker outcome side is invalid".to_owned(),
                        ));
                    }
                };
                (
                    outcome.to_owned(),
                    price,
                    required_decimal(&event.values, "count")?,
                    positive,
                )
            }
        };
        if price <= Decimal::ZERO || price > Decimal::ONE || size <= Decimal::ZERO {
            return Err(PredictionSignalError::MarketValue(
                "prediction trade price or size is invalid".to_owned(),
            ));
        }
        let gross = price.checked_mul(size).ok_or_else(|| {
            PredictionSignalError::Arithmetic("trade notional overflowed".to_owned())
        })?;
        let (signed_notional, signed_units) = if positive {
            (gross, size)
        } else {
            (
                Decimal::ZERO.checked_sub(gross).ok_or_else(|| {
                    PredictionSignalError::Arithmetic("signed notional overflowed".to_owned())
                })?,
                Decimal::ZERO.checked_sub(size).ok_or_else(|| {
                    PredictionSignalError::Arithmetic("signed units overflowed".to_owned())
                })?,
            )
        };
        self.flow
            .ingest(logical_receipt_ns, signed_notional, signed_units, gross)?;
        // C3b: one admitted trade for `tick_velocity`.
        self.tick_velocity.observe_trade(logical_receipt_ns);
        self.last_trade = Some(TradeObservation {
            outcome,
            price,
            size,
            logical_receipt_ns,
            event_time: event.event_time.clone(),
        });
        Ok(())
    }

    fn emit_all_with_base_reason(
        &mut self,
        now_ns: u64,
        base_reason: Option<String>,
    ) -> Result<PredictionBatch, PredictionSignalError> {
        let produced_at = self.logical_timestamp(now_ns)?;
        let contract = self.contract_output(now_ns, &produced_at, base_reason.clone())?;
        let quote = self.quote_output(now_ns, &produced_at, base_reason.clone())?;
        let liquidity = self.liquidity_output(now_ns, &produced_at, base_reason.clone())?;
        let flow = self.flow_output(now_ns, &produced_at, base_reason.clone())?;
        let window_delta = self.window_delta_output(now_ns, &produced_at, base_reason.clone())?;
        let moneyness = self.moneyness_output(now_ns, &produced_at, base_reason.clone())?;
        let fair_up = self.fair_up_probability_output(now_ns, &produced_at, base_reason.clone())?;
        let edge = self.executable_edge_output(now_ns, &produced_at, base_reason.clone())?;
        // The repricing-lag state machine is driven once per decision, before
        // its own output is read, so every emitted reading is a fold over the
        // same ordered observations a replay reproduces.
        self.observe_repricing(now_ns)?;
        let pair_cost = self.pair_cost_output(now_ns, &produced_at, base_reason.clone())?;
        let repricing_lag = self.repricing_lag_output(now_ns, &produced_at, base_reason.clone())?;
        let tie_rule = self.tie_rule_output(now_ns, &produced_at, base_reason.clone())?;
        // C3b (ruling E51): the TA series is folded once per decision, before any
        // of the identities that read it, so every published reading is a fold over
        // the same ordered samples a replay reproduces.
        self.observe_probability(now_ns)?;
        let rsi = self.rsi_output(now_ns, &produced_at, base_reason.clone())?;
        let roc = self.roc_output(now_ns, &produced_at, base_reason.clone())?;
        let zscore = self.zscore_output(now_ns, &produced_at, base_reason.clone())?;
        let tick_velocity = self.tick_velocity_output(now_ns, &produced_at, base_reason.clone())?;
        let expected_value =
            self.expected_value_output(now_ns, &produced_at, base_reason.clone())?;
        // C2 (E49): the venue's own probability momentum and volatility, read from
        // the per-generation ring sampled on the one-second health tick. Computed
        // before the settlement block consumes `base_reason` and appended last so
        // the batch order matches the `PredictionSignalKind::ALL` table order.
        let probability_momentum =
            self.probability_momentum_output(now_ns, &produced_at, base_reason.clone())?;
        let probability_volatility =
            self.probability_volatility_output(now_ns, &produced_at, base_reason.clone())?;
        let mut updates = vec![
            contract,
            quote,
            liquidity,
            flow,
            window_delta,
            moneyness,
            fair_up,
            edge,
            pair_cost,
            repricing_lag,
            tie_rule,
            rsi,
            roc,
            zscore,
            tick_velocity,
            expected_value,
        ];
        let base_reason_for_streak = base_reason.clone();
        // The two settlement-anchor identities are Polymarket's alone, so this
        // batch is one identity set per venue rather than one shared count.
        // The two settlement-anchor identities publish on Polymarket always, and
        // on Kalshi only while its CF index socket is armed (E43); the guard is
        // the arming, not the venue, and the anchor binding differs per venue
        // (§5.4). A disarmed Kalshi lane emits neither, so its batch is unchanged.
        if self.emits_settlement_identities() {
            // Ruling E37b: on the one tick a settlement window completes, both
            // twins publish a final ready decision (settled anchor,
            // `settlement_window_complete` true, `window_as_of == quarter_close`)
            // before going `market_closed`. A forced-degraded decision never does.
            // The high-water mark is advanced once, after both twins have taken
            // that decision, so exactly one final update publishes per window on
            // either twin and neither re-publishes on a later tick or replay.
            let final_close = if base_reason.is_none() {
                self.settlement_final_close(now_ns)
            } else {
                None
            };
            updates.push(self.settlement_anchor_output(
                now_ns,
                &produced_at,
                base_reason.clone(),
                final_close,
            )?);
            updates.push(self.oracle_divergence_output(
                now_ns,
                &produced_at,
                base_reason,
                final_close,
            )?);
            if let Some(quarter_close_ms) = final_close {
                self.settlement_final_close_ms = Some(quarter_close_ms);
            }
        }
        // The streak is published after the twins on the same decision, so the
        // window that just settled is already folded in (WindowStreak precedes the
        // C2 probability identities in `PredictionSignalKind::ALL`).
        if self.emits_window_streak() {
            updates.push(self.window_streak_output(
                now_ns,
                &produced_at,
                base_reason_for_streak,
            )?);
        }
        // Appended after the (optional) settlement twins so the evaluation order
        // is the `PredictionSignalKind::ALL` order for either venue and arming.
        updates.push(probability_momentum);
        updates.push(probability_volatility);
        debug_assert_eq!(
            updates.len(),
            prediction_signal_count_for(self.venue, self.index_armed)
        );
        Ok(PredictionBatch::from_emissions(updates))
    }

    /// Whether this processor publishes the settled-window streak: Kalshi with
    /// its index socket armed, and nothing else. The streak folds the settlement
    /// twins' final decisions, which exist only there (E37b/E43).
    fn emits_window_streak(&self) -> bool {
        self.venue == PredictionMarketVenue::Kalshi && self.index_armed
    }

    /// Whether this processor publishes the two settlement-anchor identities:
    /// Polymarket always, Kalshi only with its index socket armed (E43).
    fn emits_settlement_identities(&self) -> bool {
        self.venue == PredictionMarketVenue::Polymarket
            || (self.venue == PredictionMarketVenue::Kalshi && self.index_armed)
    }

    /// The latest admitted resolving-TWAP observation: the settlement anchor
    /// Polymarket resolves on. It is deliberately not market-relative — the same
    /// oracle path carries across a market handoff exactly as the opening-TWAP
    /// retention does — so it survives `clear_market_state`.
    fn latest_anchor_price(&self) -> Option<Decimal> {
        match self.venue {
            PredictionMarketVenue::Polymarket => self.twap_by_time.values().next_back().copied(),
            // Kalshi resolves on the CF Benchmarks BRTI trailing-sixty-second
            // average, the same quantity `KXBTC15M` settles on when sampled at
            // window close — never the raw index value (§5.2).
            PredictionMarketVenue::Kalshi => {
                self.cf_anchor.as_ref().map(|anchor| anchor.avg_60s_value)
            }
        }
    }

    /// The settlement anchor for this decision, or `None` with the venue's
    /// anchor-availability reason pushed. Polymarket returns the latest resolving
    /// TWAP and leaves the "no anchor" reason to the caller's existing fallback,
    /// exactly as before. Kalshi applies the CF guards (§5.2/§6) in precedence:
    /// a standing 1 Hz subscription refusal (`cfbrti_subscription_rejected` /
    /// `cfbrti_entitlement_denied`, F2); a socket drop after warm-up
    /// (`cfbrti_disconnected`, F1); an unrepaired gap (`cfbrti_anchor_missing`);
    /// a non-positive value (`cfbrti_value_invalid`); no admitted frame
    /// (`cfbrti_warming_up`); an anchor older than the staleness bound
    /// (`cfbrti_stale`, F1); a settlement window that has not reached its derived
    /// close (`cfbrti_anchor_window_thin`, from the settlement-window source, not
    /// the always-full trailing average — E37/F3); otherwise the anchor price.
    fn settlement_anchor_reading(
        &self,
        produced_at: &str,
        reasons: &mut Vec<String>,
    ) -> Result<Option<Decimal>, PredictionSignalError> {
        match self.venue {
            PredictionMarketVenue::Polymarket => Ok(self.latest_anchor_price()),
            PredictionMarketVenue::Kalshi => {
                if let Some(reason) = self.cf_index_unavailable_reason() {
                    reasons.push(reason.to_owned());
                    return Ok(None);
                }
                if let Some(fault) = self.cf_fault {
                    reasons.push(fault.to_owned());
                    return Ok(None);
                }
                let Some(anchor) = self.cf_anchor.as_ref() else {
                    reasons.push("cfbrti_warming_up".to_owned());
                    return Ok(None);
                };
                if self.cf_anchor_is_stale(anchor, produced_at)? {
                    reasons.push("cfbrti_stale".to_owned());
                    return Ok(None);
                }
                // A below-full trailing average is not a settlement anchor
                // (§5.2): the channel counts prior ticks only, so a 1-sample
                // average is reachable after a gap and must never be published.
                // This value-channel sample count is the safe guard the audit
                // confirmed "does the work"; the settlement-window source (E37)
                // supplies the real, grid-validated quarter close, not the
                // tautological window the value frame used to derive.
                if anchor.avg_60s_window_size < CFBRTI_FULL_WINDOW_SIZE {
                    reasons.push("cfbrti_anchor_window_thin".to_owned());
                    return Ok(None);
                }
                Ok(Some(anchor.avg_60s_value))
            }
        }
    }

    /// The index-wide unavailable reason for this decision, shared by the
    /// settlement twins and the oracle reference: a 1 Hz subscription refusal
    /// (F2), a post-warm-up socket drop (F1), or an unrepaired settlement-window
    /// gap. `None` when the index channel is available. Precedence is fixed so
    /// the three identities always agree on one reason.
    fn cf_index_unavailable_reason(&self) -> Option<&'static str> {
        if let Some(reason) = self.cf_subscription_reason {
            return Some(reason);
        }
        if self.cf_disconnected && self.cf_ever_warm {
            return Some("cfbrti_disconnected");
        }
        if self.settlement_window_pause.is_some() {
            return Some("cfbrti_anchor_missing");
        }
        None
    }

    /// Whether the anchor's newest print is older than the CF staleness bound
    /// (F1): on a socket drop or a silence past the bound the settlement twins
    /// degrade `cfbrti_stale`. `produced_at` is the decision's wall instant.
    fn cf_anchor_is_stale(
        &self,
        anchor: &CfAnchorObservation,
        produced_at: &str,
    ) -> Result<bool, PredictionSignalError> {
        let observation_ms = parse_utc(produced_at, "observation_time")?.timestamp_millis();
        Ok(observation_ms.saturating_sub(anchor.publish_time_ms) > CFBRTI_STALENESS_BOUND_MS)
    }

    /// Whether the latest settlement-window observation has reached its derived
    /// close with the full declared window (E37/F3): `window_as_of ==
    /// quarter_close_time` and a sixty-print window. `None` before any
    /// settlement-window frame. Between closes it is `Some(false)` and it is
    /// `Some(true)` exactly on the four closing ticks an hour — the completeness
    /// the KXBTC15M settlement locks on, computed from the settlement-window
    /// source rather than the always-full trailing average of the value frame.
    #[must_use]
    pub fn settlement_window_complete(&self) -> Option<bool> {
        self.cf_settlement_window
            .as_ref()
            .map(CfSettlementWindow::is_complete)
    }

    /// The settlement-window observation the Kalshi settlement twins publish as
    /// declared fields (F1/E37): its verbatim derived close, running as-of and
    /// completeness. The settlement-window source is a declared input, so its
    /// frame gates readiness — `None` with `cfbrti_warming_up` pushed before the
    /// first admitted frame. Frame presence gates; the completeness *value*
    /// never does (E37a), so the anchor is ready before, during and after each
    /// close and the field simply flips true on the four closing ticks an hour.
    /// Kalshi only; Polymarket resolves on the RTDS TWAP and carries no such
    /// fields.
    fn settlement_window_reading(&self, reasons: &mut Vec<String>) -> Option<CfSettlementWindow> {
        match self.cf_settlement_window.clone() {
            Some(window) => Some(window),
            None => {
                reasons.push("cfbrti_warming_up".to_owned());
                None
            }
        }
    }

    /// Admit one CF Benchmarks BRTI value-channel frame (E23/E36, 1 Hz). It sets
    /// the anchor the Kalshi settlement twins resolve on and the reading the
    /// global oracle reference publishes. A non-positive index or trailing
    /// average is a standing `cfbrti_value_invalid` fault, never a published zero;
    /// the frame is not market-scoped, so it never advances venue liveness.
    fn ingest_cf_value(&mut self, event: &SourceEvent) -> Result<(), PredictionSignalError> {
        if self.venue != PredictionMarketVenue::Kalshi {
            return Err(PredictionSignalError::UnsupportedSource(
                event.source_id.clone(),
            ));
        }
        let index_id = required_text(&event.values, "index_id")?.to_owned();
        let index_value = required_decimal(&event.values, "index_value")?;
        let avg_60s_value = required_decimal(&event.values, "avg_60s_value")?;
        let avg_60s_window_size = required_u32(&event.values, "avg_60s_window_size")?;
        if index_value <= Decimal::ZERO || avg_60s_value <= Decimal::ZERO {
            // Keep the previous anchor's publish cadence intact but stop
            // publishing a value: the instances degrade to the invalid fault.
            self.cf_anchor = None;
            self.cf_fault = Some("cfbrti_value_invalid");
            return Ok(());
        }
        let publish_time_ms = parse_utc(&event.event_time, "event_time")?.timestamp_millis();
        self.cf_previous_publish_time = self
            .cf_anchor
            .as_ref()
            .map(|anchor| anchor.publish_time.clone());
        self.cf_anchor = Some(CfAnchorObservation {
            index_value,
            avg_60s_value,
            avg_60s_window_size,
            index_id,
            publish_time: event.event_time.clone(),
            publish_time_ms,
        });
        self.cf_fault = None;
        // A first accepted observation completes warm-up (§5.1), so a later
        // socket drop degrades rather than re-warms (§6).
        self.cf_ever_warm = true;
        Ok(())
    }

    /// Admit one CF Benchmarks BRTI settlement-window frame (E28/E37, F3). It
    /// carries the derived `quarter_close_time` on the 15-minute grid and the
    /// running `window_as_of`; the settlement twins gate their completeness on
    /// it (`window_as_of == quarter_close`), four completions an hour. The value
    /// channel supplies the anchor price, so this frame never touches
    /// `cf_anchor`. Not market-scoped, so it never advances venue liveness.
    fn ingest_settlement_window(
        &mut self,
        event: &SourceEvent,
    ) -> Result<(), PredictionSignalError> {
        if self.venue != PredictionMarketVenue::Kalshi {
            return Err(PredictionSignalError::UnsupportedSource(
                event.source_id.clone(),
            ));
        }
        // The wire strings are republished verbatim as the settlement twins'
        // `quarter_close_time` / `window_as_of` fields (F1), so keep them beside
        // the parsed millis the completeness comparison uses.
        let quarter_close_time = required_text(&event.values, "quarter_close_time")?.to_owned();
        let quarter_close_ms =
            parse_utc(&quarter_close_time, "quarter_close_time")?.timestamp_millis();
        // Validate the derived close on the 15-minute grid where it is consumed
        // (E28/E37): a close off the `:00/:15/:30/:45` boundary is not a quarter
        // window at all, so the observation is refused rather than gating on it.
        if quarter_close_ms % QUARTER_HOUR_MILLIS != 0 {
            return Err(PredictionSignalError::MarketValue(format!(
                "BRTI settlement quarter close {quarter_close_ms} is not on the 15-minute grid"
            )));
        }
        let window_as_of = required_text(&event.values, "window_as_of")?.to_owned();
        let window_as_of_ms = parse_utc(&window_as_of, "window_as_of")?.timestamp_millis();
        let window_size = required_u32(&event.values, "window_size")?;
        self.cf_settlement_window = Some(CfSettlementWindow {
            quarter_close_time,
            quarter_close_ms,
            window_as_of,
            window_as_of_ms,
            window_size,
        });
        Ok(())
    }

    /// Mirror the runtime's index-channel connectivity state onto the processor
    /// (F1). A `kalshi_index` socket drop after the index has warmed makes the
    /// three index-dependent instances publish `cfbrti_disconnected`; a
    /// reconnect clears it and the next accepted frame returns them to ready.
    pub fn set_index_connected(&mut self, connected: bool) {
        self.cf_disconnected = !connected;
    }

    /// Mirror a standing subscription/entitlement refusal on the armed 1 Hz
    /// value channel onto the processor (F2). `None` clears it. The reason must
    /// be an `Unavailable`-class index reason; the runtime, which owns the
    /// registry check, is the only caller.
    pub fn set_index_channel_reason(&mut self, reason: Option<&'static str>) {
        self.cf_subscription_reason = reason;
    }

    /// Polymarket settlement-anchor state. Both inputs are re-read from
    /// processor state on this decision — the bound market's rules card and the
    /// latest admitted anchor — so a reading can never pair one market's
    /// threshold with a reading taken against another's. An unbound, still
    /// warming or closed market fails it closed exactly as it does the
    /// composites.
    fn settlement_anchor_output(
        &mut self,
        now_ns: u64,
        produced_at: &str,
        base_reason: Option<String>,
        final_close: Option<i64>,
    ) -> Result<PredictionEmission, PredictionSignalError> {
        let force_degraded = base_reason.is_some();
        // On the one final completing tick (E37b) the closed market is admitted so
        // this decision reports the settled anchor with `settlement_window_complete`
        // true before the next decision goes `market_closed`.
        let admit_closed = final_close.is_some();
        let mut reasons = base_reason.into_iter().collect::<Vec<_>>();
        let contract = match self.ready_active_for_settlement(now_ns, admit_closed, &mut reasons) {
            Some(active) => {
                let threshold = active.strike.ok_or_else(|| {
                    PredictionSignalError::State("ready contract lost its strike".to_owned())
                })?;
                let comparator = settlement_comparator(active.comparator)?;
                let tie_outcome = settled_tie_side(self.venue, active.tie_outcome)?;
                let close_ns = self.logical_ns(active.parsed_close)?;
                Some((threshold, comparator, tie_outcome, close_ns))
            }
            None => None,
        };
        // Kalshi additionally publishes the settlement-window completeness the
        // twin locks on (F1/E37); the frame gates readiness, so a ready Kalshi
        // reading always has it. Polymarket carries no such fields.
        let window = if self.venue == PredictionMarketVenue::Kalshi {
            self.settlement_window_reading(&mut reasons)
        } else {
            None
        };
        let anchor = self.settlement_anchor_reading(produced_at, &mut reasons)?;
        let outcome = match (contract, anchor) {
            (Some((threshold, comparator, tie_outcome, _close_ns)), Some(anchor))
                if reasons.is_empty() && admit_closed =>
            {
                // The final settled decision (E37b): time-to-close is zero and the
                // anchor is reported as it settles, rather than refused
                // `window_closed`. The candidate is read, not advanced, so a
                // replay reproduces it byte-for-byte.
                let settled = self.settlement_anchor.settled_outcome(
                    anchor,
                    threshold,
                    comparator,
                    tie_outcome,
                )?;
                // C3b: this window's settled side joins the streak exactly once,
                // guarded by the quarter close so a replay cannot double-count.
                if let Some(quarter_close_ms) = final_close {
                    self.window_streak.record(
                        quarter_close_ms,
                        settled.side_if_settled_now == SettledSide::Up,
                    );
                }
                Some(AnchorOutcome::Ready(settled))
            }
            (Some((threshold, comparator, tie_outcome, close_ns)), Some(anchor))
                if reasons.is_empty() =>
            {
                self.settlement_anchor.observe_contract(
                    now_ns,
                    threshold,
                    comparator,
                    tie_outcome,
                    close_ns,
                )?;
                Some(self.settlement_anchor.observe_anchor(now_ns, anchor)?)
            }
            _ => None,
        };
        let values = match outcome {
            Some(AnchorOutcome::Ready(state)) => {
                let mut values = BTreeMap::from([
                    ("anchor_price".to_owned(), decimal_value(state.anchor_price)),
                    ("threshold".to_owned(), decimal_value(state.threshold)),
                    ("distance_bps".to_owned(), decimal_value(state.distance_bps)),
                    ("time_to_close_ms".to_owned(), json!(state.time_to_close_ms)),
                    (
                        "comparator".to_owned(),
                        json!(Comparator::GreaterThanOrEqual.as_str()),
                    ),
                    (
                        "side_if_settled_now".to_owned(),
                        json!(state.side_if_settled_now.as_str()),
                    ),
                    ("state".to_owned(), json!(state.state.as_str())),
                    (
                        "pin_threshold_bps".to_owned(),
                        decimal_value(self.settlement_anchor.pin_threshold_bps()),
                    ),
                    (
                        "pin_window_ms".to_owned(),
                        json!(self.settlement_anchor.pin_window_ms()),
                    ),
                    ("observation_time".to_owned(), json!(produced_at)),
                ]);
                insert_settlement_window_fields(&mut values, window.as_ref());
                // E11: the threshold/side/distance are computed against the opening
                // strike, so this identity carries the strike's provenance (Polymarket).
                if let Some(source) = self.opening_strike_source_wire() {
                    values.insert("opening_strike_source".to_owned(), json!(source));
                }
                Some(values)
            }
            _ => {
                if reasons.is_empty() {
                    reasons.push("upstream_measurement_unavailable".to_owned());
                }
                None
            }
        };
        self.emit(
            PredictionSignalKind::SettlementAnchorState,
            produced_at,
            now_ns,
            produced_at.to_owned(),
            values,
            reasons,
            force_degraded,
        )
    }

    /// Polymarket oracle divergence: the bound market's settlement anchor
    /// against the internally hosted composite spot reference, both read on this
    /// decision. Market-scoped like every other prediction identity, so it fails
    /// closed while the venue is unbound rather than reporting a divergence no
    /// contract is exposed to.
    fn oracle_divergence_output(
        &mut self,
        now_ns: u64,
        produced_at: &str,
        base_reason: Option<String>,
        final_close: Option<i64>,
    ) -> Result<PredictionEmission, PredictionSignalError> {
        let force_degraded = base_reason.is_some();
        // The final completing tick (E37b) admits the closed market so the twin
        // publishes its one final ready decision with completeness true. The
        // divergence formula carries no close guard of its own, so admitting the
        // contract is all this twin needs.
        let admit_closed = final_close.is_some();
        let mut reasons = base_reason.into_iter().collect::<Vec<_>>();
        let bound = self
            .ready_active_for_settlement(now_ns, admit_closed, &mut reasons)
            .is_some();
        let reference = self.reference_state(&mut reasons)?;
        // Kalshi additionally publishes the settlement-window completeness (F1/
        // E37); its frame gates readiness the same as on the anchor twin.
        let window = if self.venue == PredictionMarketVenue::Kalshi {
            self.settlement_window_reading(&mut reasons)
        } else {
            None
        };
        let anchor = self.settlement_anchor_reading(produced_at, &mut reasons)?;
        let outcome = match (bound, anchor, reference) {
            (true, Some(anchor), Some((reference_mid, _))) if reasons.is_empty() => {
                self.oracle_divergence.observe_anchor(anchor);
                Some(self.oracle_divergence.observe_spot_reference(reference_mid))
            }
            _ => None,
        };
        let values = match outcome {
            Some(DivergenceOutcome::Ready(context)) => {
                let mut values = BTreeMap::from([
                    (
                        "divergence_bps".to_owned(),
                        decimal_value(context.divergence_bps),
                    ),
                    (
                        "anchor_price".to_owned(),
                        decimal_value(context.anchor_price),
                    ),
                    (
                        "spot_reference_mid".to_owned(),
                        decimal_value(context.spot_reference_mid),
                    ),
                    ("state".to_owned(), json!(context.state.as_str())),
                    (
                        "diverged_threshold_bps".to_owned(),
                        decimal_value(self.oracle_divergence.diverged_threshold_bps()),
                    ),
                    ("observation_time".to_owned(), json!(produced_at)),
                ]);
                insert_settlement_window_fields(&mut values, window.as_ref());
                Some(values)
            }
            _ => {
                if reasons.is_empty() {
                    reasons.push("upstream_measurement_unavailable".to_owned());
                }
                None
            }
        };
        self.emit(
            PredictionSignalKind::OracleDivergence,
            produced_at,
            now_ns,
            produced_at.to_owned(),
            values,
            reasons,
            force_degraded,
        )
    }

    /// Feed the repricing-lag fold with this decision's spot reference and venue
    /// midpoints. Both observations use the processor's decision clock `now_ns`,
    /// which `observe_time` already proves monotonic, so the two input streams
    /// share one clock and can never regress against each other. The cost is
    /// that a completed lag is quantized to the decision cadence (every admitted
    /// market input plus each one-second health tick), which is the resolution
    /// this signal publishes.
    fn observe_repricing(&mut self, now_ns: u64) -> Result<(), PredictionSignalError> {
        let mut discard = Vec::new();
        if let Some((reference_price, _)) = self.reference_state(&mut discard)?
            && reference_price > Decimal::ZERO
        {
            self.repricing_lag.observe_spot(now_ns, reference_price)?;
        }
        let alive = self.venue_alive(now_ns);
        let mut quote_reasons = Vec::new();
        let affirmative = fresh_quote(
            self.affirmative_quote.as_ref(),
            self.affirmative_quote_fault.as_deref(),
            now_ns,
            &mut quote_reasons,
            "affirmative",
            alive,
        )?
        .cloned();
        let negative = fresh_quote(
            self.negative_quote.as_ref(),
            self.negative_quote_fault.as_deref(),
            now_ns,
            &mut quote_reasons,
            "negative",
            alive,
        )?
        .cloned();
        if let (Some(affirmative), Some(negative)) = (affirmative, negative) {
            let affirmative_mid = quote_midpoint(&affirmative)?;
            let negative_mid = quote_midpoint(&negative)?;
            if affirmative_mid > Decimal::ZERO && negative_mid > Decimal::ZERO {
                self.repricing_lag
                    .observe_venue_quote(now_ns, affirmative_mid, negative_mid)?;
            }
        }
        Ok(())
    }

    fn pair_cost_output(
        &mut self,
        now_ns: u64,
        produced_at: &str,
        base_reason: Option<String>,
    ) -> Result<PredictionEmission, PredictionSignalError> {
        let force_degraded = base_reason.is_some();
        let mut reasons = base_reason.into_iter().collect::<Vec<_>>();
        // Pair cost is a reading about the bound contract, so a closed, unbound
        // or still-warming market fails it closed exactly as it does the
        // composites — fresh quotes alone must never keep it publishing.
        let bound = self.ready_active(now_ns, &mut reasons).is_some();
        let alive = self.venue_alive(now_ns);
        let affirmative = fresh_quote(
            self.affirmative_quote.as_ref(),
            self.affirmative_quote_fault.as_deref(),
            now_ns,
            &mut reasons,
            "affirmative",
            alive,
        )?
        .cloned();
        let negative = fresh_quote(
            self.negative_quote.as_ref(),
            self.negative_quote_fault.as_deref(),
            now_ns,
            &mut reasons,
            "negative",
            alive,
        )?
        .cloned();
        let mut event_time = produced_at.to_owned();
        let values = match (affirmative, negative) {
            (Some(affirmative), Some(negative)) if bound && reasons.is_empty() => {
                // The reading is as of the freshest of the two admitted quotes;
                // ties resolve to the affirmative side so the choice is total.
                event_time = if negative.logical_receipt_ns > affirmative.logical_receipt_ns {
                    negative.event_time.clone()
                } else {
                    affirmative.event_time.clone()
                };
                self.pair_cost
                    .values(affirmative.ask, negative.ask, &event_time, &mut reasons)?
            }
            _ => None,
        };
        self.emit(
            PredictionSignalKind::PairCostState,
            produced_at,
            now_ns,
            event_time,
            values,
            reasons,
            force_degraded,
        )
    }

    fn repricing_lag_output(
        &mut self,
        now_ns: u64,
        produced_at: &str,
        base_reason: Option<String>,
    ) -> Result<PredictionEmission, PredictionSignalError> {
        let force_degraded = base_reason.is_some();
        let mut reasons = base_reason.into_iter().collect::<Vec<_>>();
        // The lag is measured against the bound market's quote stream, so an
        // unbound or closed market fails it closed rather than restating the
        // last completed lag.
        let bound = self.ready_active(now_ns, &mut reasons).is_some();
        let reading = self.repricing_lag.reading_at(now_ns);
        let move_threshold_bps = self.repricing_lag.move_threshold_bps();
        let values = match reading.repricing_lag_ms {
            // `repricing_lag_ms` is a required published field, so the instance
            // stays not-ready until one qualifying move has actually been
            // answered; publishing a placeholder lag would be a fabricated value.
            Some(lag_ms) if bound && reasons.is_empty() => Some(BTreeMap::from([
                ("state".to_owned(), json!(reading.state.as_str())),
                ("repricing_lag_ms".to_owned(), json!(lag_ms)),
                ("staleness_ms".to_owned(), json!(reading.staleness_ms)),
                (
                    "move_threshold_bps".to_owned(),
                    decimal_value(move_threshold_bps),
                ),
                ("observation_time".to_owned(), json!(produced_at)),
            ])),
            _ => {
                if reasons.is_empty() {
                    // E53: distinguish a genuinely missing upstream from a present
                    // upstream that has simply not produced a qualifying move yet.
                    // `no_qualifying_move` is the self-healing not-yet state once
                    // both the spot reference and a venue quote have been seen;
                    // `upstream_measurement_unavailable` is reserved for before
                    // that, when an input is genuinely missing.
                    if self.repricing_lag.has_measurement_inputs() {
                        reasons.push("no_qualifying_move".to_owned());
                    } else {
                        reasons.push("upstream_measurement_unavailable".to_owned());
                    }
                }
                None
            }
        };
        self.emit(
            PredictionSignalKind::VenueRepricingLag,
            produced_at,
            now_ns,
            produced_at.to_owned(),
            values,
            reasons,
            force_degraded,
        )
    }

    fn tie_rule_output(
        &mut self,
        now_ns: u64,
        produced_at: &str,
        base_reason: Option<String>,
    ) -> Result<PredictionEmission, PredictionSignalError> {
        let force_degraded = base_reason.is_some();
        let mut reasons = base_reason.into_iter().collect::<Vec<_>>();
        let moneyness = self.compute_moneyness(now_ns, &mut reasons)?;
        let tie_outcome = self
            .active
            .as_ref()
            .map(|active| normalized_tie_outcome(self.venue, active.tie_outcome))
            .transpose()?;
        let near_tie_threshold = self.tie_rule.near_tie_threshold();
        let mut event_time = produced_at.to_owned();
        let values = match (moneyness, tie_outcome) {
            (Some(moneyness), Some(tie_outcome)) if reasons.is_empty() => {
                event_time = moneyness.observation_time.clone();
                // Evaluated from this decision's inputs only: the rules-card tie
                // outcome of the bound market and the moneyness just computed,
                // so no reading can pair a tie rule with another market's
                // moneyness.
                self.tie_rule.observe_tie_outcome(tie_outcome);
                match self.tie_rule.observe_moneyness(moneyness.d) {
                    TieRuleOutcome::Ready(context) => {
                        let mut values = BTreeMap::from([
                            (
                                "tie_outcome".to_owned(),
                                json!(context.tie_outcome.as_str()),
                            ),
                            ("near_tie".to_owned(), json!(context.near_tie)),
                            (
                                "tie_advantage_side".to_owned(),
                                json!(
                                    context
                                        .tie_advantage_side
                                        .map_or("none", TieOutcome::as_str)
                                ),
                            ),
                            (
                                "near_tie_threshold".to_owned(),
                                decimal_value(near_tie_threshold),
                            ),
                            ("observation_time".to_owned(), json!(event_time)),
                        ]);
                        // E11: near_tie is the moneyness (distance from the opening
                        // strike) inside the tie band, so carry the strike's source.
                        if let Some(source) = self.opening_strike_source_wire() {
                            values.insert("opening_strike_source".to_owned(), json!(source));
                        }
                        Some(values)
                    }
                    TieRuleOutcome::NotReady { reason: _ } => {
                        reasons.push("upstream_measurement_unavailable".to_owned());
                        None
                    }
                }
            }
            _ => {
                if reasons.is_empty() {
                    reasons.push("market_contract_warming_up".to_owned());
                }
                None
            }
        };
        self.emit(
            PredictionSignalKind::TieRuleContext,
            produced_at,
            now_ns,
            event_time,
            values,
            reasons,
            force_degraded,
        )
    }

    fn contract_output(
        &mut self,
        now_ns: u64,
        produced_at: &str,
        base_reason: Option<String>,
    ) -> Result<PredictionEmission, PredictionSignalError> {
        let force_degraded = base_reason.is_some();
        let mut reasons = base_reason.into_iter().collect::<Vec<_>>();
        let values = if let Some(active) = &self.active {
            if now_ns >= self.logical_ns(active.parsed_close)? {
                reasons.push("market_closed".to_owned());
                None
            } else if let Some(strike) = active.strike {
                let mut values = BTreeMap::from([
                    ("market_id".to_owned(), json!(active.market_id)),
                    ("market_generation".to_owned(), json!(active.generation)),
                    ("window_open".to_owned(), json!(active.window_open)),
                    ("window_close".to_owned(), json!(active.window_close)),
                    ("strike".to_owned(), decimal_value(strike)),
                    (
                        "settlement_benchmark".to_owned(),
                        json!(active.settlement_benchmark),
                    ),
                    (
                        "averaging_window_seconds".to_owned(),
                        json!(active.averaging_window_seconds),
                    ),
                    ("comparator".to_owned(), json!(active.comparator)),
                    ("tie_outcome".to_owned(), json!(active.tie_outcome)),
                    (
                        "affirmative_outcome".to_owned(),
                        json!(active.affirmative_outcome),
                    ),
                    (
                        "negative_outcome".to_owned(),
                        json!(active.negative_outcome),
                    ),
                    ("rules_sha256".to_owned(), json!(active.rules_sha256)),
                    ("contract_time".to_owned(), json!(active.contract_time)),
                ]);
                if self.venue == PredictionMarketVenue::Polymarket {
                    values.insert(
                        "minimum_tick_size".to_owned(),
                        decimal_value(active.minimum_tick_size.ok_or_else(|| {
                            PredictionSignalError::State(
                                "Polymarket contract lost minimum tick size".to_owned(),
                            )
                        })?),
                    );
                    values.insert(
                        "minimum_order_size".to_owned(),
                        decimal_value(active.minimum_order_size.ok_or_else(|| {
                            PredictionSignalError::State(
                                "Polymarket contract lost minimum order size".to_owned(),
                            )
                        })?),
                    );
                    // E11: the opening-strike provenance rides on every published
                    // Polymarket market (and on every strike-derived identity). A
                    // fixed strike always has a source; `prior_frame`/`post_frame`
                    // let a consumer filter an approximate strike while the market
                    // still publishes Ready.
                    values.insert(
                        "opening_strike_source".to_owned(),
                        json!(
                            active
                                .opening_strike_source
                                .ok_or_else(|| {
                                    PredictionSignalError::State(
                                "Polymarket contract has a strike without an opening-strike source"
                                    .to_owned(),
                            )
                                })?
                                .as_wire()
                        ),
                    );
                }
                Some(values)
            } else {
                reasons.push("opening_strike_unavailable".to_owned());
                None
            }
        } else {
            reasons.push("market_unavailable".to_owned());
            None
        };
        self.emit(
            PredictionSignalKind::MarketContract,
            produced_at,
            now_ns,
            self.active.as_ref().map_or_else(
                || produced_at.to_owned(),
                |active| active.contract_time.clone(),
            ),
            values,
            reasons,
            force_degraded,
        )
    }

    fn quote_output(
        &mut self,
        now_ns: u64,
        produced_at: &str,
        base_reason: Option<String>,
    ) -> Result<PredictionEmission, PredictionSignalError> {
        let force_degraded = base_reason.is_some();
        let mut reasons = base_reason.into_iter().collect::<Vec<_>>();
        let active = self.ready_active(now_ns, &mut reasons);
        // While the venue is alive an unchanged resting book republishes its held
        // quote values; missing books and integrity faults still degrade.
        let alive = self.venue_alive(now_ns);
        let affirmative = fresh_quote(
            self.affirmative_quote.as_ref(),
            self.affirmative_quote_fault.as_deref(),
            now_ns,
            &mut reasons,
            "affirmative",
            alive,
        )?;
        let negative = fresh_quote(
            self.negative_quote.as_ref(),
            self.negative_quote_fault.as_deref(),
            now_ns,
            &mut reasons,
            "negative",
            alive,
        )?;
        let trade = fresh_trade(self.last_trade.as_ref(), now_ns, &mut reasons, alive)?;
        let values = match (active, affirmative, negative, trade) {
            (Some(active), Some(affirmative), Some(negative), Some(trade))
                if reasons.is_empty() =>
            {
                let max_age = [
                    now_ns - affirmative.logical_receipt_ns,
                    now_ns - negative.logical_receipt_ns,
                    now_ns - trade.logical_receipt_ns,
                ]
                .into_iter()
                .max()
                .unwrap_or(0)
                    / 1_000_000;
                Some(BTreeMap::from([
                    ("market_id".to_owned(), json!(active.market_id)),
                    ("market_generation".to_owned(), json!(active.generation)),
                    ("affirmative_bid".to_owned(), decimal_value(affirmative.bid)),
                    ("affirmative_ask".to_owned(), decimal_value(affirmative.ask)),
                    (
                        "affirmative_midpoint".to_owned(),
                        decimal_value(checked_mean(affirmative.bid, affirmative.ask)?),
                    ),
                    ("negative_bid".to_owned(), decimal_value(negative.bid)),
                    ("negative_ask".to_owned(), decimal_value(negative.ask)),
                    (
                        "negative_midpoint".to_owned(),
                        decimal_value(checked_mean(negative.bid, negative.ask)?),
                    ),
                    ("last_trade_outcome".to_owned(), json!(trade.outcome)),
                    ("last_trade_price".to_owned(), decimal_value(trade.price)),
                    ("last_trade_size".to_owned(), decimal_value(trade.size)),
                    ("max_quote_age_ms".to_owned(), json!(max_age)),
                    (
                        "quote_time".to_owned(),
                        json!(latest_timestamp([
                            affirmative.event_time.as_str(),
                            negative.event_time.as_str(),
                            trade.event_time.as_str(),
                        ])?),
                    ),
                ]))
            }
            _ => None,
        };
        let event_time = values
            .as_ref()
            .and_then(|values| values.get("quote_time"))
            .and_then(Value::as_str)
            .unwrap_or(produced_at)
            .to_owned();
        self.emit(
            PredictionSignalKind::PredictionQuote,
            produced_at,
            now_ns,
            event_time,
            values,
            reasons,
            force_degraded,
        )
    }

    /// The affirmative_midpoint the `prediction_quote` identity would publish this
    /// tick, or `None` when that identity is not ready — the exact readiness gate
    /// [`Self::quote_output`] applies (bound ready market, two-sided fresh quotes,
    /// a fresh trade, no fault). `None` is a missing observation for the
    /// probability ring, so the C2 continuity rule restarts the warm-up (spec §3).
    fn sample_affirmative_midpoint(
        &self,
        now_ns: u64,
    ) -> Result<Option<Decimal>, PredictionSignalError> {
        let mut reasons = Vec::new();
        let active = self.ready_active(now_ns, &mut reasons);
        let alive = self.venue_alive(now_ns);
        let affirmative = fresh_quote(
            self.affirmative_quote.as_ref(),
            self.affirmative_quote_fault.as_deref(),
            now_ns,
            &mut reasons,
            "affirmative",
            alive,
        )?;
        let negative = fresh_quote(
            self.negative_quote.as_ref(),
            self.negative_quote_fault.as_deref(),
            now_ns,
            &mut reasons,
            "negative",
            alive,
        )?;
        let trade = fresh_trade(self.last_trade.as_ref(), now_ns, &mut reasons, alive)?;
        match (active, affirmative, negative, trade) {
            (Some(_), Some(affirmative), Some(_), Some(_)) if reasons.is_empty() => {
                Ok(Some(checked_mean(affirmative.bid, affirmative.ask)?))
            }
            _ => Ok(None),
        }
    }

    /// The venue's own probability momentum (C2, E49): the venue's short / mid /
    /// long probability-point returns of the per-generation `affirmative_midpoint`
    /// ring (Kalshi 10 s / 1 m / 5 m, Polymarket 10 s / 30 s / 2 m). Ready only
    /// once the ring holds its full warm-up of contiguous one-second samples
    /// (`spec.max_samples`); a forced-degraded decision or a warming ring omits
    /// values.
    fn probability_momentum_output(
        &mut self,
        now_ns: u64,
        produced_at: &str,
        base_reason: Option<String>,
    ) -> Result<PredictionEmission, PredictionSignalError> {
        let force_degraded = base_reason.is_some();
        let mut reasons = base_reason.into_iter().collect::<Vec<_>>();
        // Fail closed with the market's own reason when it is not a bound, open
        // market, exactly as the quote identity does: momentum over a closed or
        // unbound market is not a live series.
        let active = self.ready_active(now_ns, &mut reasons);
        let metrics = self.prob_ring.momentum()?;
        let values =
            match (active, metrics) {
                (Some(_), Some(metrics)) if reasons.is_empty() => Some(
                    probability_momentum_values(self.prob_ring.spec, metrics, produced_at),
                ),
                _ => None,
            };
        // A ready market whose ring has not yet warmed to its full contiguous span.
        if values.is_none() && reasons.is_empty() {
            reasons.push(PROBABILITY_HORIZON_WARMING_REASON.to_owned());
        }
        self.emit(
            PredictionSignalKind::ProbabilityMomentum,
            produced_at,
            now_ns,
            produced_at.to_owned(),
            values,
            reasons,
            force_degraded,
        )
    }

    /// The venue's own probability volatility (C2, E49): the RMS of the one-second
    /// probability-point returns over the venue's short / mid / long windows on the
    /// per-generation ring (Kalshi 10 s / 1 m / 5 m, Polymarket 10 s / 30 s / 2 m),
    /// with the shortest/longest ratio and regime. Ready only once the ring holds
    /// its full warm-up of contiguous samples (`spec.max_samples`).
    fn probability_volatility_output(
        &mut self,
        now_ns: u64,
        produced_at: &str,
        base_reason: Option<String>,
    ) -> Result<PredictionEmission, PredictionSignalError> {
        let force_degraded = base_reason.is_some();
        let mut reasons = base_reason.into_iter().collect::<Vec<_>>();
        // Fail closed with the market's own reason when it is not a bound, open
        // market, exactly as the quote identity does.
        let active = self.ready_active(now_ns, &mut reasons);
        let metrics = self.prob_ring.volatility()?;
        let values =
            match (active, metrics) {
                (Some(_), Some(metrics)) if reasons.is_empty() => Some(
                    probability_volatility_values(self.prob_ring.spec, metrics, produced_at),
                ),
                _ => None,
            };
        // A ready market whose ring has not yet warmed to its full contiguous span.
        if values.is_none() && reasons.is_empty() {
            reasons.push(PROBABILITY_HORIZON_WARMING_REASON.to_owned());
        }
        self.emit(
            PredictionSignalKind::ProbabilityVolatility,
            produced_at,
            now_ns,
            produced_at.to_owned(),
            values,
            reasons,
            force_degraded,
        )
    }

    fn liquidity_output(
        &mut self,
        now_ns: u64,
        produced_at: &str,
        base_reason: Option<String>,
    ) -> Result<PredictionEmission, PredictionSignalError> {
        let mut force_degraded = base_reason.is_some();
        let mut reasons = base_reason.into_iter().collect::<Vec<_>>();
        let active = self.ready_active(now_ns, &mut reasons);
        // A resting book republishes its band-depth values while the venue is
        // alive; snapshot/one-sided/crossed integrity faults still degrade.
        let alive = self.venue_alive(now_ns);
        let affirmative = self.books.affirmative.metrics(now_ns, alive)?;
        if affirmative.is_none() {
            reasons.push(self.books.affirmative.unavailable_reason("book"));
        }
        let (negative, complement_reason) = match self.venue {
            PredictionMarketVenue::Polymarket => {
                (self.books.negative.metrics(now_ns, alive)?, None)
            }
            PredictionMarketVenue::Kalshi => self
                .books
                .affirmative
                .kalshi_complement_metrics(now_ns, alive)?,
        };
        force_degraded |= complement_reason.is_some();
        reasons.extend(complement_reason.map(str::to_owned));
        if negative.is_none() && complement_reason.is_none() {
            reasons.push(match self.venue {
                PredictionMarketVenue::Polymarket => {
                    self.books.negative.unavailable_reason("negative_book")
                }
                PredictionMarketVenue::Kalshi => self.books.affirmative.unavailable_reason("book"),
            });
        }
        if affirmative
            .as_ref()
            .is_some_and(|metrics| metrics.buy_cost.is_none() || metrics.sell_proceeds.is_none())
            || negative.as_ref().is_some_and(|metrics| {
                metrics.buy_cost.is_none() || metrics.sell_proceeds.is_none()
            })
        {
            reasons.push("probe_depth_insufficient".to_owned());
        }
        let values = match (active, affirmative, negative) {
            (Some(active), Some(affirmative), Some(negative)) if reasons.is_empty() => {
                let affirmative_buy_cost = affirmative.buy_cost.ok_or_else(|| {
                    PredictionSignalError::State("ready affirmative probe lost buy cost".to_owned())
                })?;
                let affirmative_sell_proceeds = affirmative.sell_proceeds.ok_or_else(|| {
                    PredictionSignalError::State(
                        "ready affirmative probe lost sell proceeds".to_owned(),
                    )
                })?;
                let negative_buy_cost = negative.buy_cost.ok_or_else(|| {
                    PredictionSignalError::State("ready negative probe lost buy cost".to_owned())
                })?;
                let negative_sell_proceeds = negative.sell_proceeds.ok_or_else(|| {
                    PredictionSignalError::State(
                        "ready negative probe lost sell proceeds".to_owned(),
                    )
                })?;
                let values = BTreeMap::from([
                    ("market_id".to_owned(), json!(active.market_id)),
                    ("market_generation".to_owned(), json!(active.generation)),
                    (
                        "probe_units".to_owned(),
                        decimal_value(Decimal::from(PREDICTION_LIQUIDITY_PROBE_UNITS)),
                    ),
                    (
                        "affirmative_spread".to_owned(),
                        decimal_value(checked_sub(affirmative.best_ask, affirmative.best_bid)?),
                    ),
                    (
                        "negative_spread".to_owned(),
                        decimal_value(checked_sub(negative.best_ask, negative.best_bid)?),
                    ),
                    (
                        "affirmative_bid_size".to_owned(),
                        decimal_value(affirmative.best_bid_size),
                    ),
                    (
                        "affirmative_ask_size".to_owned(),
                        decimal_value(affirmative.best_ask_size),
                    ),
                    (
                        "negative_bid_size".to_owned(),
                        decimal_value(negative.best_bid_size),
                    ),
                    (
                        "negative_ask_size".to_owned(),
                        decimal_value(negative.best_ask_size),
                    ),
                    (
                        "affirmative_total_bid_depth".to_owned(),
                        decimal_value(affirmative.total_bid_depth),
                    ),
                    (
                        "affirmative_total_ask_depth".to_owned(),
                        decimal_value(affirmative.total_ask_depth),
                    ),
                    (
                        "negative_total_bid_depth".to_owned(),
                        decimal_value(negative.total_bid_depth),
                    ),
                    (
                        "negative_total_ask_depth".to_owned(),
                        decimal_value(negative.total_ask_depth),
                    ),
                    (
                        "affirmative_buy_cost".to_owned(),
                        decimal_value(affirmative_buy_cost),
                    ),
                    (
                        "affirmative_sell_proceeds".to_owned(),
                        decimal_value(affirmative_sell_proceeds),
                    ),
                    (
                        "negative_buy_cost".to_owned(),
                        decimal_value(negative_buy_cost),
                    ),
                    (
                        "negative_sell_proceeds".to_owned(),
                        decimal_value(negative_sell_proceeds),
                    ),
                    (
                        "max_book_age_ms".to_owned(),
                        json!(affirmative.age_ms.max(negative.age_ms)),
                    ),
                    (
                        "book_time".to_owned(),
                        json!(latest_timestamp([
                            affirmative.event_time.as_str(),
                            negative.event_time.as_str(),
                        ])?),
                    ),
                ]);
                Some(values)
            }
            _ => None,
        };
        let event_time = values
            .as_ref()
            .and_then(|values| values.get("book_time"))
            .and_then(Value::as_str)
            .unwrap_or(produced_at)
            .to_owned();
        self.emit(
            PredictionSignalKind::PredictionLiquidity,
            produced_at,
            now_ns,
            event_time,
            values,
            reasons,
            force_degraded,
        )
    }

    fn flow_output(
        &mut self,
        now_ns: u64,
        produced_at: &str,
        base_reason: Option<String>,
    ) -> Result<PredictionEmission, PredictionSignalError> {
        let force_degraded = base_reason.is_some();
        let mut reasons = base_reason.into_iter().collect::<Vec<_>>();
        let active = self.ready_active(now_ns, &mut reasons);
        if !self.flow.warmed(now_ns) {
            reasons.push("trade_history_warming_up".to_owned());
        }
        // A trade-quiet window publishes truthful zeros while the venue is alive;
        // only a genuinely silent venue degrades trade_stale_or_missing.
        if !self.venue_alive(now_ns) {
            reasons.push("trade_stale_or_missing".to_owned());
        }
        let one = self.flow.window(now_ns, 1_000_000_000)?;
        let ten = self.flow.window(now_ns, 10_000_000_000)?;
        let sixty = self.flow.window(now_ns, PREDICTION_FLOW_WINDOW_NS)?;
        let values = active.filter(|_| reasons.is_empty()).map(|active| {
            BTreeMap::from([
                ("market_id".to_owned(), json!(active.market_id)),
                ("market_generation".to_owned(), json!(active.generation)),
                (
                    "flow_semantics".to_owned(),
                    json!(match self.venue {
                        PredictionMarketVenue::Polymarket => "provider_reported_side",
                        PredictionMarketVenue::Kalshi => "verified_taker_outcome",
                    }),
                ),
                (
                    "signed_notional_1s".to_owned(),
                    decimal_value(one.signed_notional),
                ),
                (
                    "signed_notional_10s".to_owned(),
                    decimal_value(ten.signed_notional),
                ),
                (
                    "signed_notional_60s".to_owned(),
                    decimal_value(sixty.signed_notional),
                ),
                (
                    "gross_notional_60s".to_owned(),
                    decimal_value(sixty.gross_notional),
                ),
                (
                    "signed_units_10s".to_owned(),
                    decimal_value(ten.signed_units),
                ),
                (
                    "signed_units_60s".to_owned(),
                    decimal_value(sixty.signed_units),
                ),
                (
                    "trade_count_60s".to_owned(),
                    json!(u32::try_from(sixty.count).unwrap_or(u32::MAX)),
                ),
                (
                    "flow_direction_10s".to_owned(),
                    json!(if ten.signed_notional.is_zero() {
                        "flat"
                    } else if ten.signed_notional.is_sign_positive() {
                        "positive"
                    } else {
                        "negative"
                    }),
                ),
                ("window_close".to_owned(), json!(produced_at)),
            ])
        });
        self.emit(
            PredictionSignalKind::PredictionTradeFlow,
            produced_at,
            now_ns,
            produced_at.to_owned(),
            values,
            reasons,
            force_degraded,
        )
    }

    fn window_delta_output(
        &mut self,
        now_ns: u64,
        produced_at: &str,
        base_reason: Option<String>,
    ) -> Result<PredictionEmission, PredictionSignalError> {
        let force_degraded = base_reason.is_some();
        let mut reasons = base_reason.into_iter().collect::<Vec<_>>();
        let active = self.ready_active(now_ns, &mut reasons);
        let reference = self.reference_state(&mut reasons)?;
        let values = match (active, reference) {
            (Some(active), Some((reference_price, observation_time))) if reasons.is_empty() => {
                let strike = active.strike.ok_or_else(|| {
                    PredictionSignalError::State("ready contract lost its strike".to_owned())
                })?;
                let delta_absolute = checked_sub(reference_price, strike)?;
                let delta_bps =
                    checked_mul(checked_div(delta_absolute, strike)?, Decimal::from(10_000))?;
                let mut values = BTreeMap::from([
                    ("reference_price".to_owned(), decimal_value(reference_price)),
                    ("strike".to_owned(), decimal_value(strike)),
                    ("delta_absolute".to_owned(), decimal_value(delta_absolute)),
                    ("delta_bps".to_owned(), decimal_value(delta_bps)),
                    ("observation_time".to_owned(), json!(observation_time)),
                ]);
                // E11: delta is measured from the opening strike, so carry its source.
                if let Some(source) = self.opening_strike_source_wire() {
                    values.insert("opening_strike_source".to_owned(), json!(source));
                }
                Some(values)
            }
            _ => None,
        };
        let event_time = values
            .as_ref()
            .and_then(|values| values.get("observation_time"))
            .and_then(Value::as_str)
            .unwrap_or(produced_at)
            .to_owned();
        self.emit(
            PredictionSignalKind::WindowDelta,
            produced_at,
            now_ns,
            event_time,
            values,
            reasons,
            force_degraded,
        )
    }

    fn moneyness_output(
        &mut self,
        now_ns: u64,
        produced_at: &str,
        base_reason: Option<String>,
    ) -> Result<PredictionEmission, PredictionSignalError> {
        let force_degraded = base_reason.is_some();
        let mut reasons = base_reason.into_iter().collect::<Vec<_>>();
        let moneyness = self.compute_moneyness(now_ns, &mut reasons)?;
        let values = match moneyness {
            Some(moneyness) if reasons.is_empty() => {
                let mut values = BTreeMap::from([
                    ("moneyness".to_owned(), decimal_value(moneyness.d)),
                    (
                        "sigma_per_second".to_owned(),
                        decimal_value(moneyness.sigma_per_second),
                    ),
                    (
                        "tau_seconds".to_owned(),
                        decimal_value(moneyness.tau_seconds),
                    ),
                    (
                        "observation_time".to_owned(),
                        json!(moneyness.observation_time),
                    ),
                ]);
                // E11: moneyness is the distance from the opening strike.
                if let Some(source) = self.opening_strike_source_wire() {
                    values.insert("opening_strike_source".to_owned(), json!(source));
                }
                Some(values)
            }
            _ => None,
        };
        let event_time = values
            .as_ref()
            .and_then(|values| values.get("observation_time"))
            .and_then(Value::as_str)
            .unwrap_or(produced_at)
            .to_owned();
        self.emit(
            PredictionSignalKind::StandardizedMoneyness,
            produced_at,
            now_ns,
            event_time,
            values,
            reasons,
            force_degraded,
        )
    }

    fn fair_up_probability_output(
        &mut self,
        now_ns: u64,
        produced_at: &str,
        base_reason: Option<String>,
    ) -> Result<PredictionEmission, PredictionSignalError> {
        let force_degraded = base_reason.is_some();
        let mut reasons = base_reason.into_iter().collect::<Vec<_>>();
        let moneyness = self.compute_moneyness(now_ns, &mut reasons)?;
        let values = match moneyness {
            Some(moneyness) if reasons.is_empty() => {
                let probability = self.affirmative_probability(moneyness.d)?;
                let mut values = BTreeMap::from([
                    ("probability".to_owned(), decimal_value(probability)),
                    ("moneyness".to_owned(), decimal_value(moneyness.d)),
                    (
                        "reference_price".to_owned(),
                        decimal_value(moneyness.reference_price),
                    ),
                    ("strike".to_owned(), decimal_value(moneyness.strike)),
                    (
                        "sigma_per_second".to_owned(),
                        decimal_value(moneyness.sigma_per_second),
                    ),
                    (
                        "tau_seconds".to_owned(),
                        decimal_value(moneyness.tau_seconds),
                    ),
                    (
                        "observation_time".to_owned(),
                        json!(moneyness.observation_time),
                    ),
                ]);
                // E11: the fair probability is a function of the opening strike.
                if let Some(source) = self.opening_strike_source_wire() {
                    values.insert("opening_strike_source".to_owned(), json!(source));
                }
                Some(values)
            }
            _ => None,
        };
        let event_time = values
            .as_ref()
            .and_then(|values| values.get("observation_time"))
            .and_then(Value::as_str)
            .unwrap_or(produced_at)
            .to_owned();
        self.emit(
            PredictionSignalKind::FairUpProbability,
            produced_at,
            now_ns,
            event_time,
            values,
            reasons,
            force_degraded,
        )
    }

    fn executable_edge_output(
        &mut self,
        now_ns: u64,
        produced_at: &str,
        base_reason: Option<String>,
    ) -> Result<PredictionEmission, PredictionSignalError> {
        let force_degraded = base_reason.is_some();
        let mut reasons = base_reason.into_iter().collect::<Vec<_>>();
        let moneyness = self.compute_moneyness(now_ns, &mut reasons)?;
        let alive = self.venue_alive(now_ns);
        let affirmative = fresh_quote(
            self.affirmative_quote.as_ref(),
            self.affirmative_quote_fault.as_deref(),
            now_ns,
            &mut reasons,
            "affirmative",
            alive,
        )?;
        let negative = fresh_quote(
            self.negative_quote.as_ref(),
            self.negative_quote_fault.as_deref(),
            now_ns,
            &mut reasons,
            "negative",
            alive,
        )?;
        let values = match (moneyness, affirmative, negative) {
            (Some(moneyness), Some(affirmative), Some(negative)) if reasons.is_empty() => {
                let probability = self.affirmative_probability(moneyness.d)?;
                let executable_up_price = affirmative.ask;
                let executable_down_price = negative.ask;
                let edge_up = checked_sub(probability, executable_up_price)?;
                let edge_down = checked_sub(
                    checked_sub(Decimal::ONE, probability)?,
                    executable_down_price,
                )?;
                let mut values = BTreeMap::from([
                    ("edge_up".to_owned(), decimal_value(edge_up)),
                    ("edge_down".to_owned(), decimal_value(edge_down)),
                    ("probability".to_owned(), decimal_value(probability)),
                    (
                        "executable_up_price".to_owned(),
                        decimal_value(executable_up_price),
                    ),
                    (
                        "executable_down_price".to_owned(),
                        decimal_value(executable_down_price),
                    ),
                    ("cost_model".to_owned(), json!("none")),
                    (
                        "observation_time".to_owned(),
                        json!(moneyness.observation_time),
                    ),
                ]);
                // E11: edge is derived from the strike-based fair probability.
                if let Some(source) = self.opening_strike_source_wire() {
                    values.insert("opening_strike_source".to_owned(), json!(source));
                }
                Some(values)
            }
            _ => None,
        };
        let event_time = values
            .as_ref()
            .and_then(|values| values.get("observation_time"))
            .and_then(Value::as_str)
            .unwrap_or(produced_at)
            .to_owned();
        self.emit(
            PredictionSignalKind::ExecutableEdge,
            produced_at,
            now_ns,
            event_time,
            values,
            reasons,
            force_degraded,
        )
    }

    /// Fold this decision's affirmative midpoint into the one-second ring.
    ///
    /// Sampled only from a bound, ready market with a fresh, uncrossed
    /// affirmative quote: a sample is the contract's probability, so a decision
    /// that has no such quote contributes no sample and leaves the window
    /// incomplete rather than carrying the last one forward.
    fn observe_probability(&mut self, now_ns: u64) -> Result<(), PredictionSignalError> {
        let mut reasons = Vec::new();
        let bound = self.ready_active(now_ns, &mut reasons).is_some();
        let alive = self.venue_alive(now_ns);
        let quote = fresh_quote(
            self.affirmative_quote.as_ref(),
            self.affirmative_quote_fault.as_deref(),
            now_ns,
            &mut reasons,
            "affirmative",
            alive,
        )?
        .cloned();
        let (Some(quote), true) = (quote, bound && reasons.is_empty()) else {
            return Ok(());
        };
        let points = checked_mul(quote_midpoint(&quote)?, Decimal::from(100))?;
        self.probability.observe(now_ns, points);
        Ok(())
    }

    /// Wilder RSI(14) over the affirmative midpoint's ten-second closes (ruling
    /// E58): fifteen closed bars warm the indicator in 150 s, inside a market
    /// generation, where sixty-second bars never would.
    fn rsi_output(
        &mut self,
        now_ns: u64,
        produced_at: &str,
        base_reason: Option<String>,
    ) -> Result<PredictionEmission, PredictionSignalError> {
        let force_degraded = base_reason.is_some();
        let mut reasons = base_reason.into_iter().collect::<Vec<_>>();
        let bound = self.ready_active(now_ns, &mut reasons).is_some();
        let closes = self.probability.bars.closes();
        let bars_observed = u16::try_from(closes.len()).unwrap_or(u16::MAX);
        let rsi = if bound && reasons.is_empty() && closes.len() >= RSI_WARMUP_BARS {
            wilder_rsi(&closes, RSI_PERIOD)?
        } else {
            None
        };
        let values = match rsi {
            Some(rsi) => Some(BTreeMap::from([
                ("rsi_14".to_owned(), decimal_value(rsi)),
                (
                    "regime".to_owned(),
                    json!(
                        RsiRegime::classify(rsi, RSI_OVERSOLD_THRESHOLD, RSI_OVERBOUGHT_THRESHOLD)
                            .as_str()
                    ),
                ),
                ("bars_observed".to_owned(), json!(bars_observed)),
                (
                    "oversold_threshold".to_owned(),
                    decimal_value(RSI_OVERSOLD_THRESHOLD),
                ),
                (
                    "overbought_threshold".to_owned(),
                    decimal_value(RSI_OVERBOUGHT_THRESHOLD),
                ),
                ("observation_time".to_owned(), json!(produced_at)),
            ])),
            None => {
                if reasons.is_empty() {
                    reasons.push(PREDICTION_WARMING_REASON.to_owned());
                }
                None
            }
        };
        self.emit(
            PredictionSignalKind::Rsi,
            produced_at,
            now_ns,
            produced_at.to_owned(),
            values,
            reasons,
            force_degraded,
        )
    }

    /// Rate of change of the affirmative midpoint over 10 s / 1 m / 5 m.
    fn roc_output(
        &mut self,
        now_ns: u64,
        produced_at: &str,
        base_reason: Option<String>,
    ) -> Result<PredictionEmission, PredictionSignalError> {
        let force_degraded = base_reason.is_some();
        let mut reasons = base_reason.into_iter().collect::<Vec<_>>();
        let bound = self.ready_active(now_ns, &mut reasons).is_some();
        let latest = self.probability.ring.latest();
        let mut values = None;
        if let (true, Some((_, points))) = (bound && reasons.is_empty(), latest) {
            let mut horizons = Vec::with_capacity(ROC_HORIZONS_SECONDS.len());
            for horizon in ROC_HORIZONS_SECONDS {
                let prior = self.probability.ring.at_offset(horizon);
                let measured = match prior {
                    Some(prior) => {
                        match (roc_percent(points, prior)?, checked_sub(points, prior).ok()) {
                            (Some(percent), Some(delta)) => Some((delta, percent)),
                            // A zero prior probability has no percent change.
                            _ => None,
                        }
                    }
                    None => None,
                };
                horizons.push(measured);
            }
            // Readiness is the fastest horizon: the identity publishes early in a
            // window and the longer horizons join as they fill.
            if let Some(Some(_)) = horizons.first() {
                let mut map = BTreeMap::from([
                    ("probability_points".to_owned(), decimal_value(points)),
                    ("observation_time".to_owned(), json!(produced_at)),
                ]);
                for (index, label) in ["10s", "1m", "5m"].iter().enumerate() {
                    let measured = horizons[index];
                    map.insert(format!("roc_{label}_present"), json!(measured.is_some()));
                    let (delta, percent) = measured.unwrap_or((Decimal::ZERO, Decimal::ZERO));
                    map.insert(format!("roc_points_{label}"), decimal_value(delta));
                    map.insert(format!("roc_percent_{label}"), decimal_value(percent));
                }
                values = Some(map);
            }
        }
        if values.is_none() && reasons.is_empty() {
            reasons.push(PREDICTION_WARMING_REASON.to_owned());
        }
        self.emit(
            PredictionSignalKind::Roc,
            produced_at,
            now_ns,
            produced_at.to_owned(),
            values,
            reasons,
            force_degraded,
        )
    }

    /// Z-score of the affirmative midpoint over 1 m / 5 m / 15 m.
    fn zscore_output(
        &mut self,
        now_ns: u64,
        produced_at: &str,
        base_reason: Option<String>,
    ) -> Result<PredictionEmission, PredictionSignalError> {
        let force_degraded = base_reason.is_some();
        let mut reasons = base_reason.into_iter().collect::<Vec<_>>();
        let bound = self.ready_active(now_ns, &mut reasons).is_some();
        let latest = self.probability.ring.latest();
        let mut values = None;
        let mut flat = false;
        if let (true, Some((_, points))) = (bound && reasons.is_empty(), latest) {
            let mut outcomes = Vec::with_capacity(ZSCORE_WINDOWS_SECONDS.len());
            for window in ZSCORE_WINDOWS_SECONDS {
                let outcome = match self.probability.ring.window(window) {
                    Some(samples) => zscore(&samples, points)?,
                    None => ZScoreOutcome::Insufficient,
                };
                outcomes.push(outcome);
            }
            match &outcomes[0] {
                ZScoreOutcome::Ready { z, mean, sigma } => {
                    let mut map = BTreeMap::from([
                        ("probability_points".to_owned(), decimal_value(points)),
                        ("z_1m".to_owned(), decimal_value(*z)),
                        ("mean_1m".to_owned(), decimal_value(*mean)),
                        ("sigma_1m".to_owned(), decimal_value(*sigma)),
                        ("observation_time".to_owned(), json!(produced_at)),
                    ]);
                    for (index, label) in ["5m", "15m"].iter().enumerate() {
                        let value = match &outcomes[index + 1] {
                            ZScoreOutcome::Ready { z, .. } => Some(*z),
                            // A flat or short longer window has no z-score; the
                            // presence flag says so and the value stays zero.
                            ZScoreOutcome::FlatWindow | ZScoreOutcome::Insufficient => None,
                        };
                        map.insert(format!("z_{label}_present"), json!(value.is_some()));
                        map.insert(
                            format!("z_{label}"),
                            decimal_value(value.unwrap_or(Decimal::ZERO)),
                        );
                    }
                    values = Some(map);
                }
                ZScoreOutcome::FlatWindow => flat = true,
                ZScoreOutcome::Insufficient => {}
            }
        }
        if values.is_none() && reasons.is_empty() {
            reasons.push(if flat {
                FLAT_WINDOW_REASON.to_owned()
            } else {
                PREDICTION_WARMING_REASON.to_owned()
            });
        }
        self.emit(
            PredictionSignalKind::ZScore,
            produced_at,
            now_ns,
            produced_at.to_owned(),
            values,
            reasons,
            force_degraded,
        )
    }

    /// The bound market's update rate (C3 row 8).
    fn tick_velocity_output(
        &mut self,
        now_ns: u64,
        produced_at: &str,
        base_reason: Option<String>,
    ) -> Result<PredictionEmission, PredictionSignalError> {
        let force_degraded = base_reason.is_some();
        let mut reasons = base_reason.into_iter().collect::<Vec<_>>();
        let bound = self.ready_active(now_ns, &mut reasons).is_some();
        let warmed = self.tick_velocity.warmed(now_ns);
        let reading = self.tick_velocity.reading(now_ns);
        let values = if bound && reasons.is_empty() && warmed {
            Some(crate::spot_signals::tick_velocity_values(
                &reading,
                produced_at,
            ))
        } else {
            if reasons.is_empty() {
                reasons.push(PREDICTION_WARMING_REASON.to_owned());
            }
            None
        };
        self.emit(
            PredictionSignalKind::TickVelocity,
            produced_at,
            now_ns,
            produced_at.to_owned(),
            values,
            reasons,
            force_degraded,
        )
    }

    /// Expected value at the ask on both sides, net of the venue's taker fee.
    ///
    /// Ready only when both inputs are: the fair probability (the same
    /// standardized-moneyness chain `fair_up_probability` publishes) and both
    /// executable asks. Each gate pushes its own reason, so a degraded reading
    /// never carries a substituted price or probability.
    fn expected_value_output(
        &mut self,
        now_ns: u64,
        produced_at: &str,
        base_reason: Option<String>,
    ) -> Result<PredictionEmission, PredictionSignalError> {
        let force_degraded = base_reason.is_some();
        let mut reasons = base_reason.into_iter().collect::<Vec<_>>();
        let moneyness = self.compute_moneyness(now_ns, &mut reasons)?;
        let alive = self.venue_alive(now_ns);
        let affirmative = fresh_quote(
            self.affirmative_quote.as_ref(),
            self.affirmative_quote_fault.as_deref(),
            now_ns,
            &mut reasons,
            "affirmative",
            alive,
        )?
        .cloned();
        let negative = fresh_quote(
            self.negative_quote.as_ref(),
            self.negative_quote_fault.as_deref(),
            now_ns,
            &mut reasons,
            "negative",
            alive,
        )?
        .cloned();
        let values = match (moneyness, affirmative, negative) {
            (Some(moneyness), Some(affirmative), Some(negative)) if reasons.is_empty() => {
                let probability = self.affirmative_probability(moneyness.d)?;
                let yes_ask = affirmative.ask;
                let no_ask = negative.ask;
                let yes_fee = self.pair_cost.leg_fee(yes_ask)?;
                let no_fee = self.pair_cost.leg_fee(no_ask)?;
                let payout = Decimal::ONE;
                let ev_yes = checked_sub(
                    checked_sub(checked_mul(probability, payout)?, yes_ask)?,
                    yes_fee,
                )?;
                let ev_no = checked_sub(
                    checked_sub(
                        checked_mul(checked_sub(Decimal::ONE, probability)?, payout)?,
                        no_ask,
                    )?,
                    no_fee,
                )?;
                // An exact tie resolves to the affirmative side so the choice is
                // total and reproducible.
                let best_yes = ev_yes >= ev_no;
                let (best_ev, best_stake) = if best_yes {
                    (ev_yes, checked_add(yes_ask, yes_fee)?)
                } else {
                    (ev_no, checked_add(no_ask, no_fee)?)
                };
                // A zero stake risks no capital, so it has no return on stake.
                let ev_bps_of_stake = if best_stake.is_zero() {
                    Decimal::ZERO
                } else {
                    checked_mul(checked_div(best_ev, best_stake)?, Decimal::from(10_000))?
                };
                let mut values = BTreeMap::from([
                    ("fair_probability".to_owned(), decimal_value(probability)),
                    ("payout".to_owned(), decimal_value(payout)),
                    ("yes_ask".to_owned(), decimal_value(yes_ask)),
                    ("no_ask".to_owned(), decimal_value(no_ask)),
                    ("yes_fee".to_owned(), decimal_value(yes_fee)),
                    ("no_fee".to_owned(), decimal_value(no_fee)),
                    ("ev_buy_yes_at_ask".to_owned(), decimal_value(ev_yes)),
                    ("ev_buy_no_at_ask".to_owned(), decimal_value(ev_no)),
                    (
                        "best_side".to_owned(),
                        json!(if best_yes { "yes" } else { "no" }),
                    ),
                    ("ev_bps_of_stake".to_owned(), decimal_value(ev_bps_of_stake)),
                    (
                        "observation_time".to_owned(),
                        json!(moneyness.observation_time),
                    ),
                ]);
                // E11: expected value is derived from the strike-based fair probability.
                if let Some(source) = self.opening_strike_source_wire() {
                    values.insert("opening_strike_source".to_owned(), json!(source));
                }
                Some(values)
            }
            _ => None,
        };
        let event_time = values
            .as_ref()
            .and_then(|values| values.get("observation_time"))
            .and_then(Value::as_str)
            .unwrap_or(produced_at)
            .to_owned();
        self.emit(
            PredictionSignalKind::ExpectedValue,
            produced_at,
            now_ns,
            event_time,
            values,
            reasons,
            force_degraded,
        )
    }

    /// The settled-window streak (C3 row 10). Reads across windows, so it is
    /// gated on the bound market only for its scope, not for its history.
    fn window_streak_output(
        &mut self,
        now_ns: u64,
        produced_at: &str,
        base_reason: Option<String>,
    ) -> Result<PredictionEmission, PredictionSignalError> {
        let force_degraded = base_reason.is_some();
        let mut reasons = base_reason.into_iter().collect::<Vec<_>>();
        let bound = self.ready_active(now_ns, &mut reasons).is_some();
        let values = if bound && reasons.is_empty() && self.window_streak.ready() {
            let streak = &self.window_streak;
            let up = streak.streak_up.unwrap_or(false);
            let last_close = streak
                .last_close_ms
                .and_then(DateTime::from_timestamp_millis)
                .ok_or_else(|| {
                    PredictionSignalError::State(
                        "a settled window streak lost its last quarter close".to_owned(),
                    )
                })?
                .to_rfc3339_opts(SecondsFormat::Millis, true);
            let up_share = streak.up_share()?;
            Some(BTreeMap::from([
                (
                    "streak_direction".to_owned(),
                    json!(if up { "up" } else { "down" }),
                ),
                ("streak_length".to_owned(), json!(streak.streak_length)),
                ("last_n_outcomes".to_owned(), json!(streak.recent_text())),
                ("up_share_8".to_owned(), decimal_value(up_share)),
                (
                    "settled_window_count".to_owned(),
                    json!(streak.settled_window_count),
                ),
                ("last_settled_close".to_owned(), json!(last_close)),
                ("observation_time".to_owned(), json!(produced_at)),
            ]))
        } else {
            if reasons.is_empty() {
                reasons.push(PREDICTION_WARMING_REASON.to_owned());
            }
            None
        };
        self.emit(
            PredictionSignalKind::WindowStreak,
            produced_at,
            now_ns,
            produced_at.to_owned(),
            values,
            reasons,
            force_degraded,
        )
    }

    /// Walk the composite chain to the standardized moneyness statistic d, pushing
    /// a distinct fail-closed reason at every gate (market, reference, volatility,
    /// tau, model domain) so downstream degradation never carries stale values.
    fn compute_moneyness(
        &self,
        now_ns: u64,
        reasons: &mut Vec<String>,
    ) -> Result<Option<Moneyness>, PredictionSignalError> {
        let active = self.ready_active(now_ns, reasons);
        let reference = self.reference_state(reasons)?;
        let sigma = self.sigma_per_second(reasons);
        let (Some(active), Some((reference_price, observation_time)), Some(sigma_per_second)) =
            (active, reference, sigma)
        else {
            return Ok(None);
        };
        let strike = active.strike.ok_or_else(|| {
            PredictionSignalError::State("ready contract lost its strike".to_owned())
        })?;
        let tau_seconds = self.tau_seconds(active, now_ns)?;
        let ratio = checked_div(reference_price, strike)?;
        let lower = Decimal::from_i128_with_scale(25, 2); // 0.25
        let upper = Decimal::from(4);
        if ratio < lower || ratio > upper {
            reasons.push("model_domain_exceeded".to_owned());
            return Ok(None);
        }
        let ln_ratio = ln_fixed(ratio)?;
        let sqrt_tau = sqrt_fixed(tau_seconds)?;
        let denominator = checked_mul(sigma_per_second, sqrt_tau)?;
        if denominator.is_zero() {
            reasons.push("model_denominator_zero".to_owned());
            return Ok(None);
        }
        let d = checked_div(ln_ratio, denominator)?;
        Ok(Some(Moneyness {
            d,
            reference_price,
            strike,
            sigma_per_second,
            tau_seconds,
            observation_time,
        }))
    }

    /// Fair probability of the affirmative outcome from the rules card comparator:
    /// a greater-than(-or-equal) card pays the affirmative on the up move, so the
    /// probability is phi_cdf(d); an affirmative-down card publishes 1 - phi_cdf(d).
    /// Never clamped; exact ties carry zero mass in the continuous model.
    fn affirmative_probability(&self, d: Decimal) -> Result<Decimal, PredictionSignalError> {
        let active = self.active.as_ref().ok_or_else(|| {
            PredictionSignalError::State("fair probability requires an active contract".to_owned())
        })?;
        let affirmative_is_up =
            matches!(active.comparator, "greater_than_or_equal" | "greater_than");
        let phi = phi_cdf(d)?;
        if affirmative_is_up {
            Ok(phi)
        } else {
            checked_sub(Decimal::ONE, phi)
        }
    }

    /// The ready spot reference price and its observation time, or a fail-closed
    /// reason. The reference is the internally hosted spot pipeline's latest update.
    fn reference_state(
        &self,
        reasons: &mut Vec<String>,
    ) -> Result<Option<(Decimal, String)>, PredictionSignalError> {
        match &self.latest_reference {
            Some(update) if update.quality.state == QualityState::Ready => {
                match update.values.get("reference_price").and_then(Value::as_str) {
                    Some(text) => Ok(Some((parse_decimal_text(text)?, update.event_time.clone()))),
                    None => {
                        reasons.push("reference_unavailable".to_owned());
                        Ok(None)
                    }
                }
            }
            _ => {
                reasons.push("reference_unavailable".to_owned());
                Ok(None)
            }
        }
    }

    /// Sigma per second = volatility_bps_15s / 10^4, or a fail-closed reason when
    /// the volatility ring is warming or measures exactly zero.
    fn sigma_per_second(&self, reasons: &mut Vec<String>) -> Option<Decimal> {
        match &self.latest_volatility {
            None => {
                reasons.push("volatility_warming_up".to_owned());
                None
            }
            Some(metrics) => {
                let sigma = checked_div(metrics.short_bps, Decimal::from(10_000)).ok()?;
                if sigma.is_zero() {
                    reasons.push("volatility_zero".to_owned());
                    None
                } else {
                    Some(sigma)
                }
            }
        }
    }

    /// Remaining time to the rules-card close in seconds. `ready_active` guarantees
    /// the close is still ahead, so tau is strictly positive.
    fn tau_seconds(
        &self,
        active: &ActiveContract,
        now_ns: u64,
    ) -> Result<Decimal, PredictionSignalError> {
        let close_ns = self.logical_ns(active.parsed_close)?;
        let tau_ns = close_ns
            .checked_sub(now_ns)
            .ok_or_else(|| PredictionSignalError::State("tau computed after close".to_owned()))?;
        checked_div(Decimal::from(tau_ns), Decimal::from(1_000_000_000_u64))
    }

    fn ready_active<'a>(
        &'a self,
        now_ns: u64,
        reasons: &mut Vec<String>,
    ) -> Option<&'a ActiveContract> {
        self.ready_active_for_settlement(now_ns, false, reasons)
    }

    /// [`Self::ready_active`], but for the settlement twins it additionally admits
    /// a closed market on the single final completing tick (ruling E37b). When
    /// `admit_closed` is set the caller has already established, via
    /// [`Self::settlement_final_close`], that this decision is the one final
    /// settlement completion, so the closed contract is admitted once (carrying a
    /// zero time-to-close) rather than refused `market_closed`; every other
    /// decision refuses a closed market exactly as before.
    fn ready_active_for_settlement<'a>(
        &'a self,
        now_ns: u64,
        admit_closed: bool,
        reasons: &mut Vec<String>,
    ) -> Option<&'a ActiveContract> {
        let Some(active) = &self.active else {
            reasons.push("market_unavailable".to_owned());
            return None;
        };
        if active.strike.is_none() {
            reasons.push("market_contract_warming_up".to_owned());
            return None;
        }
        match self.logical_ns(active.parsed_close) {
            Ok(close_ns) if now_ns < close_ns => Some(active),
            Ok(_) if admit_closed => Some(active),
            Ok(_) => {
                reasons.push("market_closed".to_owned());
                None
            }
            Err(_) => {
                reasons.push("market_clock_unrepresentable".to_owned());
                None
            }
        }
    }

    /// The derived `quarter_close_time` (Unix ms) if THIS decision is the one
    /// final settlement completion the Kalshi settlement twins publish before the
    /// market goes `market_closed` (ruling E37b), else `None`. It is `Some` only
    /// when: the lane is an armed Kalshi lane that emits the settlement twins; a
    /// contract with a strike is bound and its window has closed
    /// (`now_ns >= close_ns`); the latest settlement-window frame is complete
    /// (`window_as_of == quarter_close_time` with the full sixty-print window) and
    /// its derived close is exactly this market's close; and that close has not
    /// already been published (`quarter_close_ms` strictly above the high-water
    /// mark). The high-water mark is advanced once, after both twins have taken
    /// their one final decision, in [`Self::emit_all_with_base_reason`].
    fn settlement_final_close(&self, now_ns: u64) -> Option<i64> {
        if !self.emits_settlement_identities() || self.venue != PredictionMarketVenue::Kalshi {
            return None;
        }
        let active = self.active.as_ref()?;
        active.strike?;
        let close_ns = self.logical_ns(active.parsed_close).ok()?;
        if now_ns < close_ns {
            return None;
        }
        let window = self.cf_settlement_window.as_ref()?;
        if !window.is_complete() {
            return None;
        }
        // Only the completion of THIS market's own quarter earns its final
        // decision; a stale window from a prior quarter never does.
        if window.quarter_close_ms != active.parsed_close.timestamp_millis() {
            return None;
        }
        if self
            .settlement_final_close_ms
            .is_some_and(|published| window.quarter_close_ms <= published)
        {
            return None;
        }
        Some(window.quarter_close_ms)
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "one evaluation carries its identity, both clocks, and its full quality outcome"
    )]
    fn emit(
        &mut self,
        kind: PredictionSignalKind,
        produced_at: &str,
        now_ns: u64,
        event_time: String,
        values: Option<BTreeMap<String, Value>>,
        reasons: Vec<String>,
        force_degraded: bool,
    ) -> Result<PredictionEmission, PredictionSignalError> {
        let active = self.active.as_ref().ok_or_else(|| {
            PredictionSignalError::State(
                "a prediction instance cannot emit while its venue is unbound".to_owned(),
            )
        })?;
        let scope = SignalScope::Market {
            market_id: active.market_id.clone(),
            market_generation: active.generation,
        };
        self.emit_scoped(
            kind,
            produced_at,
            now_ns,
            event_time,
            values,
            reasons,
            force_degraded,
            scope,
        )
    }

    /// Emit one instance under an explicit scope. The bound-market path uses
    /// [`Self::emit`], which derives the scope from the active market; the E37c
    /// release uses this directly to publish a departed market's final decision
    /// under the DEPARTED market's scope while a successor is already bound. The
    /// coalescing memory is keyed per scope, so a departed-scope emission neither
    /// consults nor perturbs the bound market's coalescing.
    #[allow(
        clippy::too_many_arguments,
        reason = "mirrors the emitter boundary, one component per wire field"
    )]
    fn emit_scoped(
        &mut self,
        kind: PredictionSignalKind,
        produced_at: &str,
        now_ns: u64,
        event_time: String,
        values: Option<BTreeMap<String, Value>>,
        reasons: Vec<String>,
        force_degraded: bool,
        scope: SignalScope,
    ) -> Result<PredictionEmission, PredictionSignalError> {
        self.emitters
            .get_mut(&kind)
            .ok_or_else(|| {
                PredictionSignalError::State("prediction emitter is missing".to_owned())
            })?
            .emit(
                &self.publisher_epoch,
                produced_at,
                now_ns,
                event_time,
                values,
                reasons,
                force_degraded,
                scope,
            )
    }

    fn clear_market_state(&mut self) {
        // The departing generation's instances are invalidated at the
        // publication boundary, so their coalescing memory goes with them: a
        // value published under the old generation can never suppress one under
        // the new.
        for emitter in self.emitters.values_mut() {
            emitter.forget_published();
        }
        self.affirmative_quote = None;
        self.negative_quote = None;
        self.affirmative_quote_fault = None;
        self.negative_quote_fault = None;
        self.last_trade = None;
        self.books.clear();
        self.flow.clear();
        // C3b: the TA series and the update-rate counters describe ONE
        // contract's probability path and arrival process, so they go with the
        // departing generation. The settled-window streak deliberately does not:
        // it is the one identity that reads across windows.
        self.probability.clear();
        self.tick_velocity.clear();
        // The repricing-lag measurement is bound to one market's quote stream:
        // a lag carried across a rollover would time a move against a different
        // contract's book. Its monotonic observation clock is the processor's
        // own decision clock, which never regresses across the reset.
        self.repricing_lag = VenueRepricingLagCandidate::with_default_threshold();
        // C2: the probability momentum/volatility ring is scoped to one market
        // generation — a series carried across a rollover would compute a return
        // between two different contracts' probabilities — so the rollover resets
        // it and the successor generation re-warms from zero (spec §3).
        self.prob_ring.clear();
        self.clear_market_source_ordering();
    }

    fn clear_market_source_ordering(&mut self) {
        self.source_ordering
            .retain(|source, _| source == POLYMARKET_TWAP_SOURCE_ID);
    }

    fn check_market_identity(&self, event: &SourceEvent) -> Result<(), PredictionSignalError> {
        let active = self.active.as_ref().ok_or_else(|| {
            PredictionSignalError::State(
                "prediction source event arrived without an active market".to_owned(),
            )
        })?;
        let observed = match self.venue {
            PredictionMarketVenue::Polymarket => required_text(&event.values, "condition_id")?,
            PredictionMarketVenue::Kalshi => required_text(&event.values, "market_ticker")?,
        };
        if observed != active.routing_id {
            return Err(PredictionSignalError::MarketIdentity {
                expected: active.routing_id.clone(),
                actual: observed.to_owned(),
            });
        }
        Ok(())
    }

    fn validate_source_event(&self, event: &SourceEvent) -> Result<(), PredictionSignalError> {
        let definition = self
            .source_definitions
            .get(&event.source_id)
            .ok_or_else(|| PredictionSignalError::UnsupportedSource(event.source_id.clone()))?;
        event.validate_against(definition)?;
        Ok(())
    }

    fn validate_source_order(&mut self, event: &SourceEvent) -> Result<(), PredictionSignalError> {
        if let Some((epoch, sequence)) = self.source_ordering.get(&event.source_id)
            && epoch == &event.publisher_epoch
            && event.sequence <= *sequence
        {
            return Err(PredictionSignalError::SourceOrdering {
                source_id: event.source_id.clone(),
                previous: *sequence,
                actual: event.sequence,
            });
        }
        self.source_ordering.insert(
            event.source_id.clone(),
            (event.publisher_epoch.clone(), event.sequence),
        );
        Ok(())
    }

    fn validate_receipt(&self, receipt_ns: u64, now_ns: u64) -> Result<(), PredictionSignalError> {
        if receipt_ns > now_ns {
            return Err(PredictionSignalError::ReceiptAfterAdmission { receipt_ns, now_ns });
        }
        Ok(())
    }

    fn note_input(&mut self, logical_receipt_ns: u64) {
        self.last_input_receipt_ns = Some(
            self.last_input_receipt_ns
                .map_or(logical_receipt_ns, |previous| {
                    previous.max(logical_receipt_ns)
                }),
        );
    }

    /// A quiet activity window is a truthful measurement only while the venue is
    /// demonstrably alive: some admitted venue input within the declared
    /// staleness bound of the decision clock. Derived solely from journaled
    /// admitted receipts so replay reproduces every decision.
    fn venue_alive(&self, now_ns: u64) -> bool {
        self.last_input_receipt_ns
            .is_some_and(|last| now_ns >= last && now_ns - last <= PREDICTION_QUOTE_STALENESS_NS)
    }

    fn observe_time(&mut self, now_ns: u64) -> Result<(), PredictionSignalError> {
        if self.last_now_ns.is_some_and(|previous| now_ns < previous) {
            return Err(PredictionSignalError::ClockRegression);
        }
        self.last_now_ns = Some(now_ns);
        Ok(())
    }

    fn logical_timestamp(&self, now_ns: u64) -> Result<String, PredictionSignalError> {
        let nanos = i64::try_from(now_ns).map_err(|_| PredictionSignalError::Counter)?;
        self.wall_start
            .checked_add_signed(ChronoDuration::nanoseconds(nanos))
            .map(|value| value.to_rfc3339_opts(SecondsFormat::Nanos, true))
            .ok_or(PredictionSignalError::Counter)
    }

    fn logical_ns(&self, timestamp: DateTime<Utc>) -> Result<u64, PredictionSignalError> {
        let nanos = timestamp
            .signed_duration_since(self.wall_start)
            .num_nanoseconds()
            .ok_or(PredictionSignalError::Counter)?;
        u64::try_from(nanos).map_err(|_| {
            PredictionSignalError::MarketValue(
                "market timestamp precedes the process wall origin".to_owned(),
            )
        })
    }
}

pub fn prediction_signal_contract_documents() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            POLYMARKET_MARKET_CONTRACT_ID,
            POLYMARKET_MARKET_CONTRACT_DEFINITION,
        ),
        (
            POLYMARKET_PREDICTION_QUOTE_ID,
            POLYMARKET_PREDICTION_QUOTE_DEFINITION,
        ),
        (
            POLYMARKET_PREDICTION_LIQUIDITY_ID,
            POLYMARKET_PREDICTION_LIQUIDITY_DEFINITION,
        ),
        (
            POLYMARKET_PREDICTION_TRADE_FLOW_ID,
            POLYMARKET_PREDICTION_TRADE_FLOW_DEFINITION,
        ),
        (KALSHI_MARKET_CONTRACT_ID, KALSHI_MARKET_CONTRACT_DEFINITION),
        (
            KALSHI_PREDICTION_QUOTE_ID,
            KALSHI_PREDICTION_QUOTE_DEFINITION,
        ),
        (
            KALSHI_PREDICTION_LIQUIDITY_ID,
            KALSHI_PREDICTION_LIQUIDITY_DEFINITION,
        ),
        (
            KALSHI_PREDICTION_TRADE_FLOW_ID,
            KALSHI_PREDICTION_TRADE_FLOW_DEFINITION,
        ),
        (
            POLYMARKET_WINDOW_DELTA_ID,
            POLYMARKET_WINDOW_DELTA_DEFINITION,
        ),
        (
            POLYMARKET_STANDARDIZED_MONEYNESS_ID,
            POLYMARKET_STANDARDIZED_MONEYNESS_DEFINITION,
        ),
        (
            POLYMARKET_FAIR_UP_PROBABILITY_ID,
            POLYMARKET_FAIR_UP_PROBABILITY_DEFINITION,
        ),
        (
            POLYMARKET_EXECUTABLE_EDGE_ID,
            POLYMARKET_EXECUTABLE_EDGE_DEFINITION,
        ),
        (KALSHI_WINDOW_DELTA_ID, KALSHI_WINDOW_DELTA_DEFINITION),
        (
            KALSHI_STANDARDIZED_MONEYNESS_ID,
            KALSHI_STANDARDIZED_MONEYNESS_DEFINITION,
        ),
        (
            KALSHI_FAIR_UP_PROBABILITY_ID,
            KALSHI_FAIR_UP_PROBABILITY_DEFINITION,
        ),
        (KALSHI_EXECUTABLE_EDGE_ID, KALSHI_EXECUTABLE_EDGE_DEFINITION),
        (
            POLYMARKET_PAIR_COST_STATE_ID,
            POLYMARKET_PAIR_COST_STATE_DEFINITION,
        ),
        (
            POLYMARKET_VENUE_REPRICING_LAG_ID,
            POLYMARKET_VENUE_REPRICING_LAG_DEFINITION,
        ),
        (
            POLYMARKET_TIE_RULE_CONTEXT_ID,
            POLYMARKET_TIE_RULE_CONTEXT_DEFINITION,
        ),
        (KALSHI_PAIR_COST_STATE_ID, KALSHI_PAIR_COST_STATE_DEFINITION),
        (
            KALSHI_VENUE_REPRICING_LAG_ID,
            KALSHI_VENUE_REPRICING_LAG_DEFINITION,
        ),
        (
            KALSHI_TIE_RULE_CONTEXT_ID,
            KALSHI_TIE_RULE_CONTEXT_DEFINITION,
        ),
        (
            POLYMARKET_SETTLEMENT_ANCHOR_STATE_ID,
            POLYMARKET_SETTLEMENT_ANCHOR_STATE_DEFINITION,
        ),
        (
            POLYMARKET_ORACLE_DIVERGENCE_ID,
            POLYMARKET_ORACLE_DIVERGENCE_DEFINITION,
        ),
        (
            POLYMARKET_PROBABILITY_MOMENTUM_ID,
            POLYMARKET_PROBABILITY_MOMENTUM_DEFINITION,
        ),
        (
            POLYMARKET_PROBABILITY_VOLATILITY_ID,
            POLYMARKET_PROBABILITY_VOLATILITY_DEFINITION,
        ),
        (
            KALSHI_PROBABILITY_MOMENTUM_ID,
            KALSHI_PROBABILITY_MOMENTUM_DEFINITION,
        ),
        (
            KALSHI_PROBABILITY_VOLATILITY_ID,
            KALSHI_PROBABILITY_VOLATILITY_DEFINITION,
        ),
        (POLYMARKET_RSI_ID, POLYMARKET_RSI_DEFINITION),
        (POLYMARKET_ROC_ID, POLYMARKET_ROC_DEFINITION),
        (POLYMARKET_ZSCORE_ID, POLYMARKET_ZSCORE_DEFINITION),
        (
            POLYMARKET_TICK_VELOCITY_ID,
            POLYMARKET_TICK_VELOCITY_DEFINITION,
        ),
        (
            POLYMARKET_EXPECTED_VALUE_ID,
            POLYMARKET_EXPECTED_VALUE_DEFINITION,
        ),
        (KALSHI_RSI_ID, KALSHI_RSI_DEFINITION),
        (KALSHI_ROC_ID, KALSHI_ROC_DEFINITION),
        (KALSHI_ZSCORE_ID, KALSHI_ZSCORE_DEFINITION),
        (KALSHI_TICK_VELOCITY_ID, KALSHI_TICK_VELOCITY_DEFINITION),
        (KALSHI_EXPECTED_VALUE_ID, KALSHI_EXPECTED_VALUE_DEFINITION),
    ]
}

/// The two ticker source contracts hosted by the embedded spot reference. They
/// enter the composite contract fingerprint (both venues host spot) at their own
/// published versions, but never join `source_documents`.
fn ticker_source_documents() -> [(&'static str, &'static str); 2] {
    [
        (COINBASE_TICKER_SOURCE_ID, COINBASE_TICKER_SOURCE_DEFINITION),
        (KRAKEN_TICKER_SOURCE_ID, KRAKEN_TICKER_SOURCE_DEFINITION),
    ]
}

/// Complete signal and source contract set for one venue-specific processor,
/// with the Kalshi index socket disarmed (the deployed configuration).
pub fn prediction_contract_documents(
    venue: PredictionMarketVenue,
) -> Vec<(&'static str, &'static str)> {
    prediction_contract_documents_for(venue, false)
}

/// Complete signal and source contract set for one venue-specific processor at
/// the given index-socket arming. When Kalshi's socket is armed this adds the
/// global `btc.cfbrti.oracle_reference` signal, the three index-gated signals
/// (the two settlement twins plus the window streak) and the CF value source;
/// disarmed, it is byte-identical to the four-venue set.
pub fn prediction_contract_documents_for(
    venue: PredictionMarketVenue,
    index_armed: bool,
) -> Vec<(&'static str, &'static str)> {
    let venue_prefix = match venue {
        PredictionMarketVenue::Polymarket => "btc.polymarket.",
        PredictionMarketVenue::Kalshi => "btc.kalshi.",
    };
    prediction_signal_contract_documents()
        .into_iter()
        .filter(|(id, _)| id.starts_with(venue_prefix))
        .chain(index_signal_contract_documents(venue, index_armed))
        .chain(source_documents(venue, index_armed))
        .chain(ticker_source_documents())
        .collect()
}

/// The index-gated signal contracts the Kalshi lane publishes while its index
/// socket is armed: the global `btc.cfbrti.oracle_reference` and the three
/// Kalshi index-gated signals (the two settlement twins plus the window streak).
/// None is in the disarmed baseline
/// (`prediction_signal_contract_documents`), so the disarmed fingerprint is
/// unchanged; armed, they join the venue's contract set here.
fn index_signal_contract_documents(
    venue: PredictionMarketVenue,
    index_armed: bool,
) -> Vec<(&'static str, &'static str)> {
    if index_armed && venue == PredictionMarketVenue::Kalshi {
        vec![
            (
                CFBRTI_ORACLE_REFERENCE_ID,
                CFBRTI_ORACLE_REFERENCE_DEFINITION,
            ),
            (
                KALSHI_SETTLEMENT_ANCHOR_STATE_ID,
                KALSHI_SETTLEMENT_ANCHOR_STATE_DEFINITION,
            ),
            (
                KALSHI_ORACLE_DIVERGENCE_ID,
                KALSHI_ORACLE_DIVERGENCE_DEFINITION,
            ),
            (KALSHI_WINDOW_STREAK_ID, KALSHI_WINDOW_STREAK_DEFINITION),
        ]
    } else {
        Vec::new()
    }
}

fn source_documents(
    venue: PredictionMarketVenue,
    index_armed: bool,
) -> Vec<(&'static str, &'static str)> {
    let mut documents = source_documents_base(venue);
    if index_armed && venue == PredictionMarketVenue::Kalshi {
        documents.push((
            KALSHI_BRTI_VALUE_SOURCE_ID,
            KALSHI_BRTI_VALUE_SOURCE_DEFINITION,
        ));
        // The settlement twins read this channel's completeness, not the value
        // channel's always-full trailing average (E37, F3).
        documents.push((
            KALSHI_BRTI_SETTLEMENT_SOURCE_ID,
            KALSHI_BRTI_SETTLEMENT_SOURCE_DEFINITION,
        ));
    }
    documents
}

fn source_documents_base(venue: PredictionMarketVenue) -> Vec<(&'static str, &'static str)> {
    match venue {
        PredictionMarketVenue::Polymarket => vec![
            (
                POLYMARKET_CONTRACT_SOURCE_ID,
                POLYMARKET_CONTRACT_SOURCE_DEFINITION,
            ),
            (
                POLYMARKET_QUOTE_SOURCE_ID,
                POLYMARKET_QUOTE_SOURCE_DEFINITION,
            ),
            (POLYMARKET_BOOK_SOURCE_ID, POLYMARKET_BOOK_SOURCE_DEFINITION),
            (
                POLYMARKET_TRADE_SOURCE_ID,
                POLYMARKET_TRADE_SOURCE_DEFINITION,
            ),
            (POLYMARKET_TWAP_SOURCE_ID, POLYMARKET_TWAP_SOURCE_DEFINITION),
        ],
        PredictionMarketVenue::Kalshi => vec![
            (KALSHI_CONTRACT_SOURCE_ID, KALSHI_CONTRACT_SOURCE_DEFINITION),
            (KALSHI_QUOTE_SOURCE_ID, KALSHI_QUOTE_SOURCE_DEFINITION),
            (KALSHI_BOOK_SOURCE_ID, KALSHI_BOOK_SOURCE_DEFINITION),
            (KALSHI_TRADE_SOURCE_ID, KALSHI_TRADE_SOURCE_DEFINITION),
        ],
    }
}

/// The two venue pair-cost computations behind one venue-dispatched holder.
///
/// Each variant owns its own verified fee model — Polymarket rounds each leg to
/// a chargeable amount, Kalshi rounds each leg up onto a member-tier increment —
/// so the venue is chosen once at construction and never re-decided per
/// decision.
#[derive(Debug, Clone)]
enum PairCostCandidate {
    Polymarket(PairCostStateCandidate),
    Kalshi(KalshiPairCostStateCandidate),
}

impl PairCostCandidate {
    fn with_defaults(venue: PredictionMarketVenue) -> Self {
        match venue {
            PredictionMarketVenue::Polymarket => {
                Self::Polymarket(PairCostStateCandidate::with_defaults())
            }
            PredictionMarketVenue::Kalshi => {
                Self::Kalshi(KalshiPairCostStateCandidate::with_defaults())
            }
        }
    }

    /// The per-contract taker fee this venue's model charges at `price`.
    ///
    /// `expected_value` subtracts exactly the fee `pair_cost_state` adds, so the
    /// two identities cannot charge different fees for the same trade.
    fn leg_fee(&self, price: Decimal) -> Result<Decimal, PredictionSignalError> {
        Ok(match self {
            Self::Polymarket(candidate) => candidate.contract_fee(price)?,
            Self::Kalshi(candidate) => candidate.contract_fee(price)?,
        })
    }

    /// Published field map for one decision, or `None` with a pushed reason when
    /// the computation declares itself not ready. The echoed parameter fields are
    /// read back from the computation so the published values and the charged
    /// values have one authority (STANDING-PRACTICES rule 6).
    fn values(
        &self,
        affirmative_ask: Decimal,
        negative_ask: Decimal,
        event_time: &str,
        reasons: &mut Vec<String>,
    ) -> Result<Option<BTreeMap<String, Value>>, PredictionSignalError> {
        match self {
            Self::Polymarket(candidate) => {
                match candidate.evaluate(Some(affirmative_ask), Some(negative_ask))? {
                    PairCostOutcome::Ready(reading) => Ok(Some(BTreeMap::from([
                        ("pair_cost".to_owned(), decimal_value(reading.pair_cost)),
                        (
                            "fee_adjusted_pair_cost".to_owned(),
                            decimal_value(reading.fee_adjusted_pair_cost),
                        ),
                        ("up_leg_fee".to_owned(), decimal_value(reading.up_leg_fee)),
                        (
                            "down_leg_fee".to_owned(),
                            decimal_value(reading.down_leg_fee),
                        ),
                        ("fee_rate".to_owned(), decimal_value(candidate.fee_rate())),
                        (
                            "fee_rounding".to_owned(),
                            serde_json::to_value(candidate.fee_rounding())
                                .map_err(|error| PredictionSignalError::State(error.to_string()))?,
                        ),
                        (
                            "premium_threshold".to_owned(),
                            decimal_value(candidate.premium_threshold()),
                        ),
                        ("state".to_owned(), json!(reading.state.as_str())),
                        ("observation_time".to_owned(), json!(event_time)),
                    ]))),
                    PairCostOutcome::NotReady { reason } => {
                        push_pair_cost_reason(reason, reasons);
                        Ok(None)
                    }
                }
            }
            Self::Kalshi(candidate) => {
                match candidate.evaluate(Some(affirmative_ask), Some(negative_ask))? {
                    KalshiPairCostOutcome::Ready(reading) => Ok(Some(BTreeMap::from([
                        ("pair_cost".to_owned(), decimal_value(reading.pair_cost)),
                        (
                            "fee_adjusted_pair_cost".to_owned(),
                            decimal_value(reading.fee_adjusted_pair_cost),
                        ),
                        ("yes_leg_fee".to_owned(), decimal_value(reading.yes_leg_fee)),
                        ("no_leg_fee".to_owned(), decimal_value(reading.no_leg_fee)),
                        ("fee_rate".to_owned(), decimal_value(candidate.fee_rate())),
                        (
                            "fee_alignment".to_owned(),
                            decimal_value(candidate.fee_alignment()),
                        ),
                        (
                            "premium_threshold".to_owned(),
                            decimal_value(candidate.premium_threshold()),
                        ),
                        ("state".to_owned(), json!(reading.state.as_str())),
                        ("observation_time".to_owned(), json!(event_time)),
                    ]))),
                    KalshiPairCostOutcome::NotReady { reason } => {
                        push_pair_cost_reason(reason, reasons);
                        Ok(None)
                    }
                }
            }
        }
    }
}

/// Map a pair-cost not-ready disposition onto the frozen stream-v3 vocabulary.
/// The caller has already gated on both quote sides, so `missing_outcome_ask` is
/// defence in depth rather than the expected path.
fn push_pair_cost_reason(reason: &str, reasons: &mut Vec<String>) {
    debug_assert_eq!(reason, "missing_outcome_ask");
    reasons.push("upstream_measurement_unavailable".to_owned());
}

/// Exact midpoint of an admitted quote. Both sides are already bounded to
/// `(0, 1]` with `bid <= ask` by ingest, so the midpoint is positive and the
/// halving is exact in decimal.
fn quote_midpoint(quote: &QuoteObservation) -> Result<Decimal, PredictionSignalError> {
    checked_div(checked_add(quote.bid, quote.ask)?, Decimal::from(2))
}

/// Normalize a rules-card tie outcome to the canonical up/down/void vocabulary.
/// Kalshi's affirmative side is `yes` and Polymarket's is `up`; both map to
/// `up`. An unmapped vocabulary is a hard error rather than a guess, so a new
/// venue word can never be silently read as an existing one.
/// The settlement comparator declared by a bound rules card. Binding already
/// refuses any other comparator, so this is a total mapping over what can be
/// active rather than a second validation.
fn settlement_comparator(comparator: &str) -> Result<Comparator, PredictionSignalError> {
    match comparator {
        "greater_than_or_equal" => Ok(Comparator::GreaterThanOrEqual),
        other => Err(PredictionSignalError::MarketValue(format!(
            "unsupported settlement comparator {other}"
        ))),
    }
}

/// The settled side a tie resolves to, routed through `normalized_tie_outcome`
/// so the venue tie vocabulary is decoded in exactly one place.
fn settled_tie_side(
    venue: PredictionMarketVenue,
    tie_outcome: &str,
) -> Result<SettledSide, PredictionSignalError> {
    Ok(match normalized_tie_outcome(venue, tie_outcome)? {
        TieOutcome::Up => SettledSide::Up,
        TieOutcome::Down => SettledSide::Down,
        TieOutcome::Void => SettledSide::Void,
    })
}

fn normalized_tie_outcome(
    venue: PredictionMarketVenue,
    tie_outcome: &str,
) -> Result<TieOutcome, PredictionSignalError> {
    match (venue, tie_outcome) {
        (PredictionMarketVenue::Polymarket, "up") | (PredictionMarketVenue::Kalshi, "yes") => {
            Ok(TieOutcome::Up)
        }
        (PredictionMarketVenue::Polymarket, "down") | (PredictionMarketVenue::Kalshi, "no") => {
            Ok(TieOutcome::Down)
        }
        (_, "void") => Ok(TieOutcome::Void),
        (_, other) => Err(PredictionSignalError::MarketValue(format!(
            "unsupported {venue:?} tie outcome {other}"
        ))),
    }
}

fn fresh_quote<'a>(
    value: Option<&'a QuoteObservation>,
    fault: Option<&str>,
    now_ns: u64,
    reasons: &mut Vec<String>,
    label: &str,
    venue_alive: bool,
) -> Result<Option<&'a QuoteObservation>, PredictionSignalError> {
    let Some(value) = value else {
        reasons.push(format!("{label}_{}", fault.unwrap_or("quote_missing")));
        return Ok(None);
    };
    let age = now_ns.checked_sub(value.logical_receipt_ns).ok_or(
        PredictionSignalError::ReceiptAfterAdmission {
            receipt_ns: value.logical_receipt_ns,
            now_ns,
        },
    )?;
    // A resting quote republishes while the venue is alive; staleness only
    // degrades a silent venue.
    if !venue_alive && age > PREDICTION_QUOTE_STALENESS_NS {
        reasons.push(format!("{label}_quote_stale"));
        Ok(None)
    } else {
        Ok(Some(value))
    }
}

fn fresh_trade<'a>(
    value: Option<&'a TradeObservation>,
    now_ns: u64,
    reasons: &mut Vec<String>,
    venue_alive: bool,
) -> Result<Option<&'a TradeObservation>, PredictionSignalError> {
    let Some(value) = value else {
        reasons.push("last_trade_missing".to_owned());
        return Ok(None);
    };
    let age = now_ns.checked_sub(value.logical_receipt_ns).ok_or(
        PredictionSignalError::ReceiptAfterAdmission {
            receipt_ns: value.logical_receipt_ns,
            now_ns,
        },
    )?;
    if !venue_alive && age > PREDICTION_QUOTE_STALENESS_NS {
        reasons.push("last_trade_stale".to_owned());
        Ok(None)
    } else {
        Ok(Some(value))
    }
}

fn walk_asks(
    levels: &BTreeMap<Decimal, Decimal>,
    requested: Decimal,
) -> Result<Option<Decimal>, PredictionSignalError> {
    walk_levels(levels.iter(), requested)
}

fn walk_bids(
    levels: &BTreeMap<Decimal, Decimal>,
    requested: Decimal,
) -> Result<Option<Decimal>, PredictionSignalError> {
    walk_levels(levels.iter().rev(), requested)
}

fn walk_complement_levels<'a>(
    levels: impl Iterator<Item = (&'a Decimal, &'a Decimal)>,
    requested: Decimal,
) -> Result<Option<Decimal>, PredictionSignalError> {
    let mut remaining = requested;
    let mut total = Decimal::ZERO;
    for (yes_price, available) in levels {
        if remaining.is_zero() {
            break;
        }
        let no_price = checked_sub(Decimal::ONE, *yes_price)?;
        let quantity = remaining.min(*available);
        let contribution = no_price.checked_mul(quantity).ok_or_else(|| {
            PredictionSignalError::Arithmetic("book execution cost overflowed".to_owned())
        })?;
        total = checked_add(total, contribution)?;
        remaining = checked_sub(remaining, quantity)?;
    }
    Ok(remaining.is_zero().then_some(total))
}

fn walk_levels<'a>(
    levels: impl Iterator<Item = (&'a Decimal, &'a Decimal)>,
    requested: Decimal,
) -> Result<Option<Decimal>, PredictionSignalError> {
    let mut remaining = requested;
    let mut total = Decimal::ZERO;
    for (price, available) in levels {
        if remaining.is_zero() {
            break;
        }
        let quantity = remaining.min(*available);
        let contribution = price.checked_mul(quantity).ok_or_else(|| {
            PredictionSignalError::Arithmetic("book execution cost overflowed".to_owned())
        })?;
        total = checked_add(total, contribution)?;
        remaining = checked_sub(remaining, quantity)?;
    }
    Ok(remaining.is_zero().then_some(total))
}

fn checked_sum(
    mut values: impl Iterator<Item = Decimal>,
) -> Result<Decimal, PredictionSignalError> {
    values.try_fold(Decimal::ZERO, checked_add)
}

/// Per-venue probability horizon parameters (C2, ruling E49).
///
/// Kalshi's 900 s generation holds the 10 s / 1 m / 5 m horizons, so the pair is
/// ready once the ring holds 301 contiguous one-second samples (the 5 m longest
/// horizon plus one reference sample). Polymarket's markets roll every 300 s, so
/// a 5 m ring is structurally never ready there; its pair uses the shorter
/// 10 s / 30 s / 2 m horizons and is ready once 121 contiguous samples are held.
#[derive(Debug, Clone, Copy)]
struct ProbabilityHorizonSpec {
    /// Ring depth: the longest horizon offset plus one reference sample.
    max_samples: usize,
    offset_short: usize,
    offset_mid: usize,
    offset_long: usize,
    /// The horizon suffixes the published field names carry, e.g. `10s`.
    label_short: &'static str,
    label_mid: &'static str,
    label_long: &'static str,
}

/// Kalshi: 10 s / 1 m / 5 m, warm at 301 contiguous samples.
const KALSHI_PROBABILITY_SPEC: ProbabilityHorizonSpec = ProbabilityHorizonSpec {
    max_samples: 301,
    offset_short: 10,
    offset_mid: 60,
    offset_long: 300,
    label_short: "10s",
    label_mid: "1m",
    label_long: "5m",
};

/// Polymarket: 10 s / 30 s / 2 m, warm at 121 contiguous samples — inside the
/// 300 s market window a 5 m ring could never fill (a window roll resets it).
const POLYMARKET_PROBABILITY_SPEC: ProbabilityHorizonSpec = ProbabilityHorizonSpec {
    max_samples: 121,
    offset_short: 10,
    offset_mid: 30,
    offset_long: 120,
    label_short: "10s",
    label_mid: "30s",
    label_long: "2m",
};

/// The probability horizon parameters for a venue's momentum/volatility pair.
const fn probability_horizon_spec(venue: PredictionMarketVenue) -> ProbabilityHorizonSpec {
    match venue {
        PredictionMarketVenue::Kalshi => KALSHI_PROBABILITY_SPEC,
        PredictionMarketVenue::Polymarket => POLYMARKET_PROBABILITY_SPEC,
    }
}

#[derive(Debug, Clone, Copy)]
struct ProbabilitySample {
    logical_time_ns: u64,
    midpoint: Decimal,
}

/// The short / mid / long causal probability-point returns (the horizons are
/// per venue, [`ProbabilityHorizonSpec`]).
#[derive(Debug, Clone, Copy)]
struct ProbabilityMomentumMetrics {
    return_short: Decimal,
    return_mid: Decimal,
    return_long: Decimal,
}

/// The short / mid / long probability-point realized-volatility metrics (the
/// horizons are per venue, [`ProbabilityHorizonSpec`]).
#[derive(Debug, Clone, Copy)]
struct ProbabilityVolatilityMetrics {
    volatility_short: Decimal,
    volatility_mid: Decimal,
    volatility_long: Decimal,
    short_long_ratio: Decimal,
    regime: &'static str,
}

/// The lane-local per-`market_generation` probability ring: up to
/// `spec.max_samples` one-second `affirmative_midpoint` samples with the same
/// continuity rule as [`crate::realized_volatility::RealizedVolatility`] —
/// `None`, a non-monotonic timestamp, or a gap of at least
/// [`MISSED_OBSERVATION_NS`] clears the ring and restarts the warm-up. It is a
/// separate ring from the spot `HorizonRing` (different horizons; probability
/// points, not basis points), and its horizons are per venue
/// ([`ProbabilityHorizonSpec`]).
#[derive(Debug, Clone)]
struct ProbabilityHorizonRing {
    spec: ProbabilityHorizonSpec,
    samples: VecDeque<ProbabilitySample>,
}

impl ProbabilityHorizonRing {
    fn new(spec: ProbabilityHorizonSpec) -> Self {
        Self {
            spec,
            samples: VecDeque::with_capacity(spec.max_samples),
        }
    }

    fn clear(&mut self) {
        self.samples.clear();
    }

    fn midpoint_from_back(&self, offset: usize) -> Option<Decimal> {
        let index = self.samples.len().checked_sub(1 + offset)?;
        self.samples.get(index).map(|sample| sample.midpoint)
    }

    /// Advance the ring from a one-second `affirmative_midpoint` observation.
    /// `None` is an unusable observation and clears the ring (continuity break),
    /// exactly as the realized-volatility ring treats a non-ready reference.
    fn update_price(&mut self, midpoint: Option<Decimal>, now_ns: u64) {
        let Some(midpoint) = midpoint else {
            self.samples.clear();
            return;
        };
        if self.samples.back().is_some_and(|sample| {
            now_ns <= sample.logical_time_ns
                || now_ns.saturating_sub(sample.logical_time_ns) >= MISSED_OBSERVATION_NS
        }) {
            self.samples.clear();
        }
        if self.samples.len() == self.spec.max_samples {
            self.samples.pop_front();
        }
        self.samples.push_back(ProbabilitySample {
            logical_time_ns: now_ns,
            midpoint,
        });
    }

    /// The short / mid / long momentum, or `None` while the ring is still warming
    /// up to the longest horizon (fewer than `spec.max_samples`).
    fn momentum(&self) -> Result<Option<ProbabilityMomentumMetrics>, PredictionSignalError> {
        if self.samples.len() < self.spec.max_samples {
            return Ok(None);
        }
        let current = self.midpoint_from_back(0).ok_or_else(|| {
            PredictionSignalError::State("warm probability ring has no newest".to_owned())
        })?;
        let short = self
            .midpoint_from_back(self.spec.offset_short)
            .ok_or_else(probability_horizon_missing)?;
        let mid = self
            .midpoint_from_back(self.spec.offset_mid)
            .ok_or_else(probability_horizon_missing)?;
        let long = self
            .midpoint_from_back(self.spec.offset_long)
            .ok_or_else(probability_horizon_missing)?;
        Ok(Some(ProbabilityMomentumMetrics {
            return_short: probability_return_pp(current, short)?,
            return_mid: probability_return_pp(current, mid)?,
            return_long: probability_return_pp(current, long)?,
        }))
    }

    /// The short / mid / long realized volatility, or `None` during warm-up.
    fn volatility(&self) -> Result<Option<ProbabilityVolatilityMetrics>, PredictionSignalError> {
        if self.samples.len() < self.spec.max_samples {
            return Ok(None);
        }
        let mut returns = Vec::with_capacity(self.samples.len() - 1);
        for index in 1..self.samples.len() {
            returns.push(probability_return_pp(
                self.samples[index].midpoint,
                self.samples[index - 1].midpoint,
            )?);
        }
        // The one-second returns inside (now − h, now]: the last h returns.
        let volatility_long = root_mean_square(&returns[returns.len() - self.spec.offset_long..])?;
        let volatility_mid = root_mean_square(&returns[returns.len() - self.spec.offset_mid..])?;
        let volatility_short =
            root_mean_square(&returns[returns.len() - self.spec.offset_short..])?;
        // shortest / longest with the micro convention: both zero → 1, stable.
        let short_long_ratio = if volatility_long.is_zero() {
            Decimal::ONE
        } else {
            checked_div(volatility_short, volatility_long)?
        };
        let regime = classify_volatility_ratio(short_long_ratio);
        Ok(Some(ProbabilityVolatilityMetrics {
            volatility_short,
            volatility_mid,
            volatility_long,
            short_long_ratio,
            regime,
        }))
    }
}

fn probability_horizon_missing() -> PredictionSignalError {
    PredictionSignalError::State("warm probability ring is missing a horizon sample".to_owned())
}

/// The probability-point return between two probabilities in [0, 1]:
/// `100 × (current − prior)`, full precision (8 dp round-half-even on output).
fn probability_return_pp(
    current: Decimal,
    prior: Decimal,
) -> Result<Decimal, PredictionSignalError> {
    checked_mul(checked_sub(current, prior)?, Decimal::from(100))
}

/// Projects the probability momentum into the published
/// `btc.<venue>.probability_momentum@1.0.0` value map.
fn probability_momentum_values(
    spec: ProbabilityHorizonSpec,
    metrics: ProbabilityMomentumMetrics,
    produced_at: &str,
) -> BTreeMap<String, Value> {
    BTreeMap::from([
        (
            format!("return_pp_{}", spec.label_short),
            decimal_value(metrics.return_short),
        ),
        (
            format!("return_pp_{}", spec.label_mid),
            decimal_value(metrics.return_mid),
        ),
        (
            format!("return_pp_{}", spec.label_long),
            decimal_value(metrics.return_long),
        ),
        (
            format!("direction_{}", spec.label_mid),
            json!(direction_word(metrics.return_mid)),
        ),
        ("window_close".to_owned(), json!(produced_at)),
    ])
}

/// Projects the probability volatility into the published
/// `btc.<venue>.probability_volatility@1.0.0` value map.
fn probability_volatility_values(
    spec: ProbabilityHorizonSpec,
    metrics: ProbabilityVolatilityMetrics,
    produced_at: &str,
) -> BTreeMap<String, Value> {
    BTreeMap::from([
        (
            format!("volatility_pp_{}", spec.label_short),
            decimal_value(metrics.volatility_short),
        ),
        (
            format!("volatility_pp_{}", spec.label_mid),
            decimal_value(metrics.volatility_mid),
        ),
        (
            format!("volatility_pp_{}", spec.label_long),
            decimal_value(metrics.volatility_long),
        ),
        (
            "short_long_ratio".to_owned(),
            decimal_value(metrics.short_long_ratio),
        ),
        ("regime".to_owned(), json!(metrics.regime)),
        ("window_close".to_owned(), json!(produced_at)),
    ])
}

fn checked_add(left: Decimal, right: Decimal) -> Result<Decimal, PredictionSignalError> {
    left.checked_add(right)
        .ok_or_else(|| PredictionSignalError::Arithmetic("decimal addition overflowed".to_owned()))
}

fn checked_mul(left: Decimal, right: Decimal) -> Result<Decimal, PredictionSignalError> {
    left.checked_mul(right).ok_or_else(|| {
        PredictionSignalError::Arithmetic("decimal multiplication overflowed".to_owned())
    })
}

fn checked_div(left: Decimal, right: Decimal) -> Result<Decimal, PredictionSignalError> {
    left.checked_div(right)
        .ok_or_else(|| PredictionSignalError::Arithmetic("decimal division failed".to_owned()))
}

fn checked_sub(left: Decimal, right: Decimal) -> Result<Decimal, PredictionSignalError> {
    left.checked_sub(right).ok_or_else(|| {
        PredictionSignalError::Arithmetic("decimal subtraction overflowed".to_owned())
    })
}

fn checked_mean(left: Decimal, right: Decimal) -> Result<Decimal, PredictionSignalError> {
    left.checked_add(right)
        .and_then(|value| value.checked_div(Decimal::TWO))
        .ok_or_else(|| PredictionSignalError::Arithmetic("decimal mean overflowed".to_owned()))
}

fn decimal_value(value: Decimal) -> Value {
    json!(
        value
            .round_dp_with_strategy(OUTPUT_DECIMAL_PLACES, RoundingStrategy::MidpointNearestEven)
            .to_string()
    )
}

fn required_text<'a>(
    values: &'a BTreeMap<String, Value>,
    field: &'static str,
) -> Result<&'a str, PredictionSignalError> {
    values
        .get(field)
        .and_then(Value::as_str)
        .ok_or(PredictionSignalError::MissingField(field))
}

fn required_decimal(
    values: &BTreeMap<String, Value>,
    field: &'static str,
) -> Result<Decimal, PredictionSignalError> {
    parse_decimal_text(required_text(values, field)?).map_err(PredictionSignalError::Decimal)
}

fn required_u32(
    values: &BTreeMap<String, Value>,
    field: &'static str,
) -> Result<u32, PredictionSignalError> {
    values
        .get(field)
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .ok_or(PredictionSignalError::MissingField(field))
}

fn parse_utc(value: &str, field: &'static str) -> Result<DateTime<Utc>, PredictionSignalError> {
    DateTime::parse_from_rfc3339(value)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|error| PredictionSignalError::Timestamp {
            field,
            detail: error.to_string(),
        })
}

fn latest_timestamp<const N: usize>(values: [&str; N]) -> Result<String, PredictionSignalError> {
    values
        .into_iter()
        .map(|value| Ok((parse_utc(value, "event_time")?, value)))
        .collect::<Result<Vec<_>, PredictionSignalError>>()?
        .into_iter()
        .max_by_key(|(parsed, _)| *parsed)
        .map(|(_, value)| value.to_owned())
        .ok_or_else(|| PredictionSignalError::State("event-time set is empty".to_owned()))
}

fn parse_document<T>(raw: &str, id: &str) -> Result<T, PredictionSignalError>
where
    T: serde::de::DeserializeOwned + Validate,
{
    let value = serde_json::from_str::<T>(raw)
        .map_err(|error| PredictionSignalError::Definition(format!("{id}: {error}")))?;
    value
        .validate()
        .map_err(|error| PredictionSignalError::Definition(format!("{id}: {error}")))?;
    Ok(value)
}

#[derive(Debug, Error)]
pub enum PredictionSignalError {
    #[error("prediction definition is invalid: {0}")]
    Definition(String),
    #[error("unsupported prediction source {0}")]
    UnsupportedSource(String),
    #[error("missing or invalid field {0}")]
    MissingField(&'static str),
    #[error("decimal field is invalid: {0}")]
    Decimal(#[from] signals_core::DecimalTextError),
    #[error("contract validation failed: {0}")]
    Validation(#[from] signals_core::ValidationError),
    #[error("invalid timestamp field {field}: {detail}")]
    Timestamp { field: &'static str, detail: String },
    #[error("prediction market value is invalid: {0}")]
    MarketValue(String),
    #[error("prediction book state is invalid: {0}")]
    Book(String),
    #[error("prediction state is invalid: {0}")]
    State(String),
    #[error("prediction checkpoint is invalid: {0}")]
    Checkpoint(String),
    #[error("prediction arithmetic failed: {0}")]
    Arithmetic(String),
    #[error("prediction resource limit reached: {0}")]
    ResourceLimit(String),
    #[error("prediction market generation mismatch: expected {expected}, received {actual}")]
    Generation { expected: u64, actual: u64 },
    #[error("prediction event market identity mismatch: expected {expected}, received {actual}")]
    MarketIdentity { expected: String, actual: String },
    #[error(
        "prediction source {source_id} sequence did not advance: previous {previous}, received {actual}"
    )]
    SourceOrdering {
        source_id: String,
        previous: u64,
        actual: u64,
    },
    #[error("prediction logical receipt {receipt_ns} exceeds admission time {now_ns}")]
    ReceiptAfterAdmission { receipt_ns: u64, now_ns: u64 },
    #[error("prediction logical receipt time regressed")]
    ReceiptRegression,
    #[error("prediction monotonic clock regressed")]
    ClockRegression,
    #[error("prediction counter exhausted")]
    Counter,
    #[error("embedded spot reference failed: {0}")]
    SpotReference(#[from] SpotReferenceError),
    #[error("embedded realized volatility failed: {0}")]
    SpotSignal(#[from] SpotSignalError),
    #[error("prediction model kernel domain error: {0}")]
    Kernel(String),
}

impl From<TaError> for PredictionSignalError {
    fn from(error: TaError) -> Self {
        PredictionSignalError::Arithmetic(error.to_string())
    }
}

impl From<crate::decimal_math::DecimalMathError> for PredictionSignalError {
    fn from(error: crate::decimal_math::DecimalMathError) -> Self {
        // The composite emitters pre-guard the kernel domains (ratio in [0.25, 4],
        // tau > 0), so a kernel domain error is a defensive last resort rather than
        // an expected fail-closed path.
        PredictionSignalError::Kernel(error.to_string())
    }
}

#[cfg(test)]
mod stream_v3_sequence_tests {
    use std::collections::{BTreeMap, BTreeSet};

    use serde_json::{Value, json};
    use signals_core::{PredictionMarketVenue, QualityState, SignalScope};

    use super::{PredictionEmitter, PredictionSignalKind};

    const TEST_TIME: &str = "2026-08-25T00:00:00Z";

    fn scope(generation: u64) -> SignalScope {
        SignalScope::Market {
            market_id: format!("KXBTC15M-{generation}"),
            market_generation: generation,
        }
    }

    fn contract_values(generation: u64) -> BTreeMap<String, Value> {
        BTreeMap::from([
            (
                "market_id".to_owned(),
                json!(format!("KXBTC15M-{generation}")),
            ),
            ("market_generation".to_owned(), json!(generation)),
            ("window_open".to_owned(), json!(TEST_TIME)),
            ("window_close".to_owned(), json!("2026-08-25T00:15:00Z")),
            ("strike".to_owned(), json!("60000")),
            (
                "settlement_benchmark".to_owned(),
                json!("cf_benchmarks_brti_sixty_print"),
            ),
            ("averaging_window_seconds".to_owned(), json!(60)),
            ("comparator".to_owned(), json!("greater_than_or_equal")),
            ("tie_outcome".to_owned(), json!("yes")),
            ("affirmative_outcome".to_owned(), json!("yes")),
            ("negative_outcome".to_owned(), json!("no")),
            ("rules_sha256".to_owned(), json!("sequence-test-rules")),
            ("contract_time".to_owned(), json!(TEST_TIME)),
        ])
    }

    const SECOND_NS: u64 = 1_000_000_000;

    fn emitter() -> PredictionEmitter {
        PredictionEmitter::new(
            PredictionMarketVenue::Kalshi,
            PredictionSignalKind::MarketContract,
        )
        .expect("prediction emitter constructs")
    }

    fn ready(
        emitter: &mut PredictionEmitter,
        now_ns: u64,
        generation: u64,
        values: BTreeMap<String, Value>,
    ) -> super::PredictionEmission {
        emitter
            .emit(
                "prediction-sequence-test",
                TEST_TIME,
                now_ns,
                TEST_TIME.to_owned(),
                Some(values),
                Vec::new(),
                false,
                scope(generation),
            )
            .expect("ready evaluation emits")
    }

    #[test]
    fn decisions_ignore_non_ready_evaluations_and_continue_across_generations() {
        let mut emitter = emitter();
        for _ in 0..2 {
            let warming = emitter
                .emit(
                    "prediction-sequence-test",
                    TEST_TIME,
                    0,
                    TEST_TIME.to_owned(),
                    None,
                    vec!["market_contract_warming_up".to_owned()],
                    false,
                    scope(1),
                )
                .expect("warming evaluation emits");
            assert_eq!(warming.update.sequence, 0);
            assert_ne!(warming.update.quality.state, QualityState::Ready);
            assert!(
                warming.published,
                "a non-ready evaluation is never coalesced"
            );
        }
        let first = ready(&mut emitter, 0, 1, contract_values(1));
        assert!(first.published);
        assert_eq!(first.update.sequence, 0);

        let gap = emitter
            .emit(
                "prediction-sequence-test",
                TEST_TIME,
                0,
                TEST_TIME.to_owned(),
                None,
                vec!["market_closed".to_owned()],
                true,
                scope(1),
            )
            .expect("non-ready boundary emits");
        assert_eq!(gap.update.sequence, 1);

        // A new generation is a new instance key, so its first decision is
        // published even though the identity has published before.
        let rebound = ready(&mut emitter, 0, 2, contract_values(2));
        assert!(rebound.published);
        assert_eq!(rebound.update.sequence, 1);

        // The original two-decision loop, restored under the coalescing rule: a
        // second consecutive ready decision on the same instance still advances
        // the sequence, provided it carries a changed measurement.
        let mut moved = contract_values(2);
        moved.insert("strike".to_owned(), json!("60500"));
        let second = ready(&mut emitter, 0, 2, moved);
        assert!(second.published);
        assert_eq!(second.update.sequence, 2);
    }

    /// A non-ready evaluation ends the published decision's standing, so the
    /// recovery decision publishes at once even inside the refresh interval and
    /// even when it restates exactly the same change key.
    #[test]
    fn a_sub_interval_recovery_publishes_instead_of_being_suppressed() {
        let mut emitter = emitter();
        assert!(ready(&mut emitter, 0, 1, contract_values(1)).published);
        assert!(
            !ready(&mut emitter, SECOND_NS / 10, 1, contract_values(1)).published,
            "the unchanged value is suppressed while the instance stays ready"
        );

        let degraded = emitter
            .emit(
                "prediction-sequence-test",
                TEST_TIME,
                SECOND_NS / 5,
                TEST_TIME.to_owned(),
                None,
                vec!["market_closed".to_owned()],
                true,
                scope(1),
            )
            .expect("degraded evaluation emits");
        assert_ne!(degraded.update.quality.state, QualityState::Ready);

        let recovered = ready(&mut emitter, SECOND_NS / 2, 1, contract_values(1));
        assert!(
            recovered.published,
            "a degraded -> ready recovery is new information, not a restatement"
        );
        assert_eq!(recovered.update.sequence, 1);
    }

    #[test]
    fn an_unchanged_instance_refreshes_exactly_on_its_declared_interval() {
        let interval_ns = emitter().definition.approximate_interval_ms * 1_000_000;
        assert_eq!(interval_ns, SECOND_NS);
        let mut emitter = emitter();

        let first = ready(&mut emitter, 0, 1, contract_values(1));
        assert!(first.published);
        assert_eq!(first.update.sequence, 0);

        let early = ready(&mut emitter, interval_ns - 1, 1, contract_values(1));
        assert!(
            !early.published,
            "an unchanged value is not re-sent before its interval falls due"
        );

        let refresh = ready(&mut emitter, interval_ns, 1, contract_values(1));
        assert!(
            refresh.published,
            "an unchanged value refreshes at the interval boundary"
        );
        assert_eq!(
            refresh.update.sequence, 1,
            "a suppressed evaluation consumes no decision sequence"
        );

        // The refresh restarts the interval: the next boundary is measured from
        // the publication, not from the first one.
        let after_refresh = ready(
            &mut emitter,
            interval_ns + interval_ns - 1,
            1,
            contract_values(1),
        );
        assert!(!after_refresh.published);
        let second_refresh = ready(&mut emitter, 2 * interval_ns, 1, contract_values(1));
        assert!(second_refresh.published);
        assert_eq!(second_refresh.update.sequence, 2);
    }

    #[test]
    fn a_changed_value_publishes_immediately_and_sequences_contiguously() {
        let mut emitter = emitter();
        assert_eq!(
            ready(&mut emitter, 0, 1, contract_values(1))
                .update
                .sequence,
            0
        );
        assert!(!ready(&mut emitter, 1, 1, contract_values(1)).published);

        let mut changed = contract_values(1);
        changed.insert("strike".to_owned(), json!("60500"));
        let published = ready(&mut emitter, 2, 1, changed.clone());
        assert!(published.published, "a changed value publishes immediately");
        assert_eq!(
            published.update.sequence, 1,
            "the decision after a suppression is still contiguous"
        );

        assert!(!ready(&mut emitter, 3, 1, changed).published);
        let reverted = ready(&mut emitter, 4, 1, contract_values(1));
        assert!(
            reverted.published,
            "reverting to an earlier value is still a change against the last published one"
        );
        assert_eq!(reverted.update.sequence, 2);
    }

    /// The change key is derived by declared type, so a value that only moved
    /// because the decision clock moved does not count as a change. Every clock
    /// field still rides on the published update with its own fresh value.
    #[test]
    fn a_clock_only_movement_is_suppressed_until_the_interval() {
        let mut emitter = emitter();
        assert!(
            emitter.change_key_fields.is_disjoint(&BTreeSet::from([
                "window_open".to_owned(),
                "window_close".to_owned(),
                "contract_time".to_owned(),
            ])),
            "declared timestamp fields are outside the change key"
        );
        assert!(emitter.change_key_fields.contains("strike"));

        assert!(ready(&mut emitter, 0, 1, contract_values(1)).published);

        let mut later = contract_values(1);
        later.insert("window_close".to_owned(), json!("2026-08-25T00:16:00Z"));
        let clock_only = ready(&mut emitter, SECOND_NS / 2, 1, later.clone());
        assert!(
            !clock_only.published,
            "a clock-only movement is not a new measurement"
        );

        let refresh = ready(&mut emitter, SECOND_NS, 1, later.clone());
        assert!(refresh.published, "the interval still refreshes it");
        assert_eq!(
            refresh.update.values["window_close"],
            json!("2026-08-25T00:16:00Z"),
            "the refresh carries the fresh clock value, not the last published one"
        );
        assert_eq!(refresh.update.sequence, 1);
    }

    /// A measurement field of any non-clock declared type publishes at once.
    #[test]
    fn a_measurement_field_of_any_non_clock_type_publishes_immediately() {
        for (field, value) in [
            // decimal
            ("strike", json!("60500")),
            // string
            ("rules_sha256", json!("changed-rules")),
            // u64
            ("market_generation", json!(2)),
            // u32
            ("averaging_window_seconds", json!(30)),
        ] {
            let mut emitter = emitter();
            assert!(ready(&mut emitter, 0, 1, contract_values(1)).published);
            assert!(!ready(&mut emitter, 1, 1, contract_values(1)).published);

            let mut changed = contract_values(1);
            changed.insert(field.to_owned(), value);
            let published = ready(&mut emitter, 2, 1, changed);
            assert!(published.published, "{field} is a measurement change");
            assert_eq!(published.update.sequence, 1);
        }
    }

    /// A definition whose every declared field is clock-derived would have an
    /// empty change key and could only ever refresh on its interval. No current
    /// prediction identity is in that state, and this pins it: the least
    /// measurement-bearing identity, `venue_repricing_lag`, still declares two
    /// non-clock fields beside its three clock ones.
    #[test]
    fn every_prediction_identity_has_at_least_one_measurement_field() {
        for venue in [
            PredictionMarketVenue::Polymarket,
            PredictionMarketVenue::Kalshi,
        ] {
            for kind in PredictionSignalKind::for_venue(venue) {
                let emitter =
                    PredictionEmitter::new(venue, kind).expect("prediction emitter constructs");
                assert!(
                    !emitter.change_key_fields.is_empty(),
                    "{} declares only clock fields and could never publish on change",
                    emitter.definition.signal_id
                );
            }
        }
    }

    #[test]
    fn a_departed_generation_is_forgotten_and_republishes() {
        let mut emitter = emitter();
        assert!(ready(&mut emitter, 0, 1, contract_values(1)).published);
        assert!(!ready(&mut emitter, 1, 1, contract_values(1)).published);

        emitter.forget_published();

        let rebound = ready(&mut emitter, 2, 1, contract_values(1));
        assert!(
            rebound.published,
            "a rebind cannot be suppressed by the departed generation's values"
        );
        assert_eq!(rebound.update.sequence, 1);
    }
}

#[cfg(test)]
mod affirmative_mapping_tests {
    use std::collections::BTreeMap;
    use std::str::FromStr;

    use chrono::{DateTime, Duration, SecondsFormat, Utc};
    use rust_decimal::Decimal;
    use serde_json::json;
    use signals_core::{PredictionMarketVenue, SourceEvent};

    use super::PredictionSignalProcessor;

    fn wall_start() -> DateTime<Utc> {
        "2026-08-15T12:00:00Z".parse().expect("wall start parses")
    }

    fn timestamp(second: i64) -> String {
        wall_start()
            .checked_add_signed(Duration::seconds(second))
            .expect("timestamp is representable")
            .to_rfc3339_opts(SecondsFormat::Millis, true)
    }

    fn kalshi_contract() -> SourceEvent {
        let values = BTreeMap::from([
            ("market_ticker".to_owned(), json!("KXBTC15M-1")),
            ("event_ticker".to_owned(), json!("KXBTC15M-E1")),
            ("title".to_owned(), json!("BTC price in fifteen minutes")),
            ("status".to_owned(), json!("active")),
            ("open_time".to_owned(), json!(timestamp(0))),
            ("close_time".to_owned(), json!(timestamp(900))),
            ("expected_expiration_time".to_owned(), json!(timestamp(900))),
            ("legal_expiration_time".to_owned(), json!(timestamp(1_800))),
            ("benchmark".to_owned(), json!("cf_benchmarks_brti")),
            ("averaging_window_seconds".to_owned(), json!(60)),
            ("comparator".to_owned(), json!("greater_than_or_equal")),
            ("tie_outcome".to_owned(), json!("yes")),
            ("reference_value".to_owned(), json!("60100")),
            ("rules_sha256".to_owned(), json!("b".repeat(64))),
            ("event_time".to_owned(), json!(timestamp(0))),
        ]);
        SourceEvent {
            schema_version: 1,
            source_id: "kalshi.btc.market-contracts".to_owned(),
            source_version: "1.0.0".to_owned(),
            publisher_epoch: "kalshi-mapping".to_owned(),
            sequence: 0,
            event_time: timestamp(0),
            received_at: timestamp(0),
            values,
        }
    }

    // ADR 0016 / SIG-038: the affirmative outcome mapping is derived from the venue
    // rules card's comparator, never hardcoded to "up". Both pinned v1.0.0 cards are
    // greater-than-or-equal (affirmative up), so the affirmative-down branch
    // (1 - phi) is not reachable through `bind_market`; this exercises the mapping at
    // the function that derives it, with the bound card's comparator swapped to a
    // less-than (affirmative-down) card. Exact ties carry zero mass in the continuous
    // model, so the up and down mappings are exact complements.
    #[test]
    fn affirmative_probability_follows_the_rules_card_comparator() {
        let mut processor = PredictionSignalProcessor::new(
            PredictionMarketVenue::Kalshi,
            "kalshi-mapping",
            wall_start(),
        )
        .expect("processor constructs");
        processor
            .bind_market(1, &kalshi_contract(), 0, 0)
            .expect("market binds");

        let d = Decimal::from_str("0.3").expect("decimal parses");
        // A greater-than(-or-equal) card pays the affirmative on the up move: phi(d).
        let up = processor.affirmative_probability(d).expect("up mapping");
        assert!(
            up > Decimal::from_str("0.5").expect("half parses"),
            "phi(0.3) exceeds one half"
        );

        // An affirmative-down card (less-than comparator) publishes 1 - phi(d).
        processor
            .active
            .as_mut()
            .expect("bound contract")
            .comparator = "less_than";
        let down = processor.affirmative_probability(d).expect("down mapping");
        assert_eq!(down, Decimal::ONE - up, "down card publishes 1 - phi(d)");
        assert_eq!(
            up + down,
            Decimal::ONE,
            "the mappings are exact complements"
        );

        // greater_than is likewise an affirmative-up comparator.
        processor
            .active
            .as_mut()
            .expect("bound contract")
            .comparator = "greater_than";
        assert_eq!(
            processor.affirmative_probability(d).expect("gt mapping"),
            up,
            "a strict greater-than card is also affirmative up"
        );
    }
}

#[cfg(test)]
mod twap_replay_tests {
    use std::collections::BTreeMap;

    use chrono::{DateTime, Utc};
    use serde_json::json;
    use signals_core::{PredictionMarketVenue, SourceEvent};

    use super::PredictionSignalProcessor;

    fn twap(price: &str) -> SourceEvent {
        SourceEvent {
            schema_version: 1,
            source_id: "polymarket.rtds.btc-usd.twap-60s".into(),
            source_version: "1.0.0".into(),
            publisher_epoch: "twap-test".into(),
            sequence: 0,
            event_time: "2026-08-26T00:00:00Z".into(),
            received_at: "2026-08-26T00:00:01Z".into(),
            values: BTreeMap::from([
                ("symbol".into(), json!("btc/usd")),
                ("window_seconds".into(), json!(60)),
                ("twap_price".into(), json!(price)),
            ]),
        }
    }

    fn processor() -> PredictionSignalProcessor {
        PredictionSignalProcessor::new(
            PredictionMarketVenue::Polymarket,
            "twap-test",
            "2026-08-26T00:00:00Z"
                .parse::<DateTime<Utc>>()
                .expect("time parses"),
        )
        .expect("processor constructs")
    }

    #[test]
    fn identical_twap_redelivery_is_idempotent() {
        let mut processor = processor();
        processor.ingest_twap(&twap("60000")).expect("first TWAP");
        processor
            .ingest_twap(&twap("60000"))
            .expect("identical TWAP redelivery");
        assert_eq!(processor.twap_by_time.len(), 1);
    }

    #[test]
    fn conflicting_twap_redelivery_fails_without_mutation() {
        let mut processor = processor();
        processor.ingest_twap(&twap("60000")).expect("first TWAP");
        assert!(processor.ingest_twap(&twap("60001")).is_err());
        assert_eq!(
            processor
                .twap_by_time
                .values()
                .next()
                .map(ToString::to_string),
            Some("60000".into())
        );
    }
}

#[cfg(test)]
mod settlement_pause_tests {
    use chrono::{DateTime, Utc};
    use signals_core::PredictionMarketVenue;

    use super::{PredictionSignalProcessor, SettlementWindowPause};

    fn processor() -> PredictionSignalProcessor {
        PredictionSignalProcessor::new(
            PredictionMarketVenue::Kalshi,
            "settlement-pause-test",
            "2026-08-29T10:00:00Z"
                .parse::<DateTime<Utc>>()
                .expect("time parses"),
        )
        .expect("processor constructs")
    }

    fn pause(quarter_close_time: i64, after: i64, through: i64) -> SettlementWindowPause {
        SettlementWindowPause {
            index_id: "BRTI".to_owned(),
            quarter_close_time,
            after_index_time: after,
            through_index_time: through,
        }
    }

    /// A second gap in a LATER quarter, raised before the first repair lands,
    /// must not escape the withhold. Re-pausing widens the outstanding pause to
    /// the later quarter (the `<=` rule then covers both) instead of keeping the
    /// earliest and dropping coverage of the later gap.
    #[test]
    fn a_second_later_gap_is_still_withheld() {
        const Q11_00: i64 = 1_788_001_200_000;
        const Q11_15: i64 = 1_788_002_100_000;
        let mut processor = processor();

        processor
            .pause_settlement_window(pause(Q11_00, Q11_00 - 900_000, Q11_00 - 5_000))
            .expect("first pause accepted");
        // The later gap arrives before the first repair lands.
        processor
            .pause_settlement_window(pause(Q11_15, Q11_15 - 900_000, Q11_15 - 5_000))
            .expect("second pause accepted");

        // Both quarters — the earlier and the later — are withheld.
        assert!(
            processor.withholds_settlement_window(Q11_00),
            "the earlier gap stays withheld"
        );
        assert!(
            processor.withholds_settlement_window(Q11_15),
            "the later gap is now covered too"
        );
        // The union interval spans both gaps.
        let outstanding = processor
            .settlement_window_pause()
            .expect("a pause is outstanding");
        assert_eq!(outstanding.quarter_close_time, Q11_15);
        assert_eq!(outstanding.after_index_time, Q11_00 - 900_000);
        assert_eq!(outstanding.through_index_time, Q11_15 - 5_000);
        outstanding.validate().expect("the widened pause is valid");
    }

    /// Re-pausing on an EARLIER quarter after a later one is already outstanding
    /// keeps the later coverage; the earlier gap is subsumed by the `<=` rule.
    #[test]
    fn an_earlier_second_gap_does_not_narrow_coverage() {
        const Q11_00: i64 = 1_788_001_200_000;
        const Q11_15: i64 = 1_788_002_100_000;
        let mut processor = processor();

        processor
            .pause_settlement_window(pause(Q11_15, Q11_15 - 900_000, Q11_15 - 5_000))
            .expect("later pause accepted");
        processor
            .pause_settlement_window(pause(Q11_00, Q11_00 - 900_000, Q11_00 - 5_000))
            .expect("earlier pause accepted");

        assert!(processor.withholds_settlement_window(Q11_00));
        assert!(processor.withholds_settlement_window(Q11_15));
        assert_eq!(
            processor
                .settlement_window_pause()
                .expect("a pause is outstanding")
                .quarter_close_time,
            Q11_15,
            "coverage stays at the later quarter"
        );
    }
}

#[cfg(test)]
mod cf_index_tests {
    //! B3-B: the Kalshi lane's CF-index ingestion, the global
    //! `btc.cfbrti.oracle_reference` emission and the two settlement identities,
    //! all gated on the armed index socket (E43). Fixtures are synthetic and
    //! shape-exact; no captured BRTI payload enters the tree (§8).
    use std::collections::BTreeMap;

    use chrono::{DateTime, Duration, SecondsFormat, Utc};
    use serde_json::json;
    use signals_core::{
        PredictionMarketVenue, QualityState, SignalScope, SignalUpdate, SourceEvent,
    };

    use super::{
        CFBRTI_ORACLE_REFERENCE_ID, KALSHI_ORACLE_DIVERGENCE_ID, KALSHI_SETTLEMENT_ANCHOR_STATE_ID,
        KALSHI_WINDOW_STREAK_ID, PredictionBatch, PredictionSignalProcessor,
    };

    const EPOCH: &str = "cf-index-epoch";
    const SECOND_NS: u64 = 1_000_000_000;

    fn wall_start() -> DateTime<Utc> {
        "2026-09-03T10:00:00Z".parse().expect("wall start parses")
    }

    fn timestamp(second: i64) -> String {
        wall_start()
            .checked_add_signed(Duration::seconds(second))
            .expect("timestamp is representable")
            .to_rfc3339_opts(SecondsFormat::Millis, true)
    }

    fn kalshi_contract() -> SourceEvent {
        let values = BTreeMap::from([
            ("market_ticker".to_owned(), json!("KXBTC15M-1")),
            ("event_ticker".to_owned(), json!("KXBTC15M-E1")),
            ("title".to_owned(), json!("BTC price in fifteen minutes")),
            ("status".to_owned(), json!("active")),
            ("open_time".to_owned(), json!(timestamp(0))),
            ("close_time".to_owned(), json!(timestamp(900))),
            ("expected_expiration_time".to_owned(), json!(timestamp(900))),
            ("legal_expiration_time".to_owned(), json!(timestamp(1_800))),
            ("benchmark".to_owned(), json!("cf_benchmarks_brti")),
            ("averaging_window_seconds".to_owned(), json!(60)),
            ("comparator".to_owned(), json!("greater_than_or_equal")),
            ("tie_outcome".to_owned(), json!("yes")),
            ("reference_value".to_owned(), json!("60100")),
            ("rules_sha256".to_owned(), json!("b".repeat(64))),
            ("event_time".to_owned(), json!(timestamp(0))),
        ]);
        SourceEvent {
            schema_version: 1,
            source_id: "kalshi.btc.market-contracts".to_owned(),
            source_version: "1.0.0".to_owned(),
            publisher_epoch: EPOCH.to_owned(),
            sequence: 0,
            event_time: timestamp(0),
            received_at: timestamp(0),
            values,
        }
    }

    /// A CF BRTI value frame at `second`, with the given raw index value, trailing
    /// average and window size. The window spans exactly sixty seconds, so a
    /// full-size window is settlement-complete (window_as_of == quarter_close).
    fn cf_value(
        second: i64,
        sequence: u64,
        index_value: &str,
        avg: &str,
        window: u32,
    ) -> SourceEvent {
        let values = BTreeMap::from([
            ("index_id".to_owned(), json!("BRTI")),
            ("index_value".to_owned(), json!(index_value)),
            ("event_time".to_owned(), json!(timestamp(second))),
            ("kalshi_received_at".to_owned(), json!(timestamp(second))),
            ("upstream_sequence".to_owned(), json!(sequence)),
            ("avg_60s_value".to_owned(), json!(avg)),
            ("avg_60s_window_size".to_owned(), json!(window)),
            (
                "avg_60s_window_start".to_owned(),
                json!(timestamp(second - 60)),
            ),
            (
                "avg_60s_window_end_exclusive".to_owned(),
                json!(timestamp(second)),
            ),
        ]);
        SourceEvent {
            schema_version: 1,
            source_id: "kalshi.cfbenchmarks.brti.value".to_owned(),
            source_version: "1.0.0".to_owned(),
            publisher_epoch: EPOCH.to_owned(),
            sequence,
            event_time: timestamp(second),
            received_at: timestamp(second),
            values,
        }
    }

    /// A settlement-window frame (E28/E37, F3): the derived quarter close on the
    /// 15-minute grid, the running `window_as_of`, and the accumulated
    /// `window_size`. Completeness is `window_as_of == quarter_close` with a
    /// full sixty-print window — the KXBTC15M settlement lock, computed here from
    /// the settlement-window SOURCE, not the always-full trailing average.
    fn cf_settlement(
        second: i64,
        sequence: u64,
        close_second: i64,
        as_of_second: i64,
        window: u32,
    ) -> SourceEvent {
        let values = BTreeMap::from([
            ("index_id".to_owned(), json!("BRTI")),
            ("event_time".to_owned(), json!(timestamp(second))),
            ("kalshi_received_at".to_owned(), json!(timestamp(second))),
            ("upstream_sequence".to_owned(), json!(sequence)),
            ("index_value".to_owned(), json!("60150.00000000")),
            ("window_average".to_owned(), json!("60150.00000000")),
            ("window_size".to_owned(), json!(window)),
            (
                "window_start_exclusive".to_owned(),
                json!(timestamp(close_second - 60)),
            ),
            (
                "window_end_inclusive".to_owned(),
                json!(timestamp(close_second)),
            ),
            ("window_as_of".to_owned(), json!(timestamp(as_of_second))),
            (
                "quarter_close_time".to_owned(),
                json!(timestamp(close_second)),
            ),
        ]);
        SourceEvent {
            schema_version: 1,
            source_id: "kalshi.cfbenchmarks.brti.settlement-window".to_owned(),
            source_version: "1.0.0".to_owned(),
            publisher_epoch: EPOCH.to_owned(),
            sequence,
            event_time: timestamp(second),
            received_at: timestamp(second),
            values,
        }
    }

    fn armed() -> PredictionSignalProcessor {
        PredictionSignalProcessor::new_indexed(
            PredictionMarketVenue::Kalshi,
            EPOCH,
            wall_start(),
            true,
        )
        .expect("armed processor builds")
    }

    fn find<'a>(updates: &'a [SignalUpdate], id: &str) -> &'a SignalUpdate {
        updates
            .iter()
            .find(|update| update.signal_id == id)
            .unwrap_or_else(|| panic!("{id} is present in the batch"))
    }

    #[test]
    fn armed_kalshi_publishes_the_settlement_twins_and_the_global_oracle_reference() {
        let mut processor = armed();
        processor
            .bind_market(1, &kalshi_contract(), 0, 0)
            .expect("market binds");
        let now = 120 * SECOND_NS;
        processor
            .ingest(&cf_value(120, 1, "60200.5", "60150.25000000", 60), 0, now)
            .expect("cf value admitted");
        // The settlement-window source is a declared input of the two twins
        // (F1), so a frame gates their readiness: an incomplete quarter (as-of
        // 120 s short of the 900 s grid close) readies them with
        // settlement_window_complete false.
        processor
            .ingest(&cf_settlement(120, 2, 900, 120, 60), 0, now)
            .expect("settlement window admitted");
        let batch = processor.advance_time(now).expect("advance emits");
        // Armed Kalshi publishes the two settlement identities in its market batch.
        let anchor = find(batch.evaluations(), KALSHI_SETTLEMENT_ANCHOR_STATE_ID);
        assert_eq!(anchor.quality.state, QualityState::Ready);
        assert!(matches!(anchor.scope, SignalScope::Market { .. }));
        assert_eq!(
            anchor.values.get("anchor_price").and_then(|v| v.as_str()),
            Some("60150.25000000"),
            "the anchor is the sixty-second average, not the raw index value"
        );
        // The three settlement-window fields (F1) are published verbatim from
        // the settlement-window source, complete false between closes.
        assert_eq!(
            anchor
                .values
                .get("quarter_close_time")
                .and_then(|v| v.as_str()),
            Some(timestamp(900).as_str()),
            "quarter_close_time is the derived close on the grid"
        );
        assert_eq!(
            anchor.values.get("window_as_of").and_then(|v| v.as_str()),
            Some(timestamp(120).as_str())
        );
        assert_eq!(
            anchor.values.get("settlement_window_complete"),
            Some(&json!(false)),
            "an incomplete quarter publishes settlement_window_complete false"
        );
        // The divergence twin is present in the batch; its own readiness needs
        // the hosted spot reference, exercised in its dedicated test.
        let _divergence = find(batch.evaluations(), KALSHI_ORACLE_DIVERGENCE_ID);
        // The global oracle reference is emitted separately, outside the batch.
        let globals = processor.take_global_updates();
        let reference = find(&globals, CFBRTI_ORACLE_REFERENCE_ID);
        assert_eq!(reference.quality.state, QualityState::Ready);
        assert!(matches!(reference.scope, SignalScope::Global));
        assert_eq!(
            reference.values.get("value").and_then(|v| v.as_str()),
            Some("60200.5"),
            "the oracle reference value is the raw index observation (free-scale within eight)"
        );
        assert_eq!(
            reference
                .values
                .get("avg_60s_value")
                .and_then(|v| v.as_str()),
            Some("60150.25000000")
        );
    }

    #[test]
    fn disarmed_kalshi_omits_the_index_identities_and_emits_no_global() {
        let mut processor =
            PredictionSignalProcessor::new(PredictionMarketVenue::Kalshi, EPOCH, wall_start())
                .expect("disarmed processor builds");
        processor
            .bind_market(1, &kalshi_contract(), 0, 0)
            .expect("market binds");
        // A disarmed lane does not accept the CF value source at all.
        assert!(!processor.accepts("kalshi.cfbenchmarks.brti.value"));
        let batch = processor.advance_time(120 * SECOND_NS).expect("advance");
        assert!(
            !batch.evaluations().iter().any(|update| update.signal_id
                == KALSHI_SETTLEMENT_ANCHOR_STATE_ID
                || update.signal_id == KALSHI_ORACLE_DIVERGENCE_ID),
            "disarmed Kalshi publishes neither settlement twin"
        );
        assert!(processor.take_global_updates().is_empty());
    }

    #[test]
    fn avg_60s_window_size_below_full_window_degrades_anchor_not_publishes() {
        let mut processor = armed();
        processor
            .bind_market(1, &kalshi_contract(), 0, 0)
            .expect("market binds");
        let now = 120 * SECOND_NS;
        // A fifty-nine-print window is not a full settlement average.
        processor
            .ingest(&cf_value(120, 1, "60200", "60150.00000000", 59), 0, now)
            .expect("cf value admitted");
        // A settlement-window frame is present (F1), so the only anchor reason is
        // the thin value-channel window, not warm-up.
        processor
            .ingest(&cf_settlement(120, 2, 900, 120, 60), 0, now)
            .expect("settlement window admitted");
        let batch = processor.advance_time(now).expect("advance");
        let anchor = find(batch.evaluations(), KALSHI_SETTLEMENT_ANCHOR_STATE_ID);
        assert_ne!(anchor.quality.state, QualityState::Ready);
        assert!(
            anchor
                .quality
                .reasons
                .iter()
                .any(|reason| reason == "cfbrti_anchor_window_thin"),
            "a thin window degrades the settlement anchor"
        );
        // The oracle reference publishes as-is: the window size is a declared
        // field on the reference, never a non-ready state (§6).
        let globals = processor.take_global_updates();
        let reference = find(&globals, CFBRTI_ORACLE_REFERENCE_ID);
        assert_eq!(reference.quality.state, QualityState::Ready);
        assert_eq!(
            reference
                .values
                .get("avg_60s_window_size")
                .and_then(|v| v.as_u64()),
            Some(59)
        );
    }

    #[test]
    fn oracle_reference_warms_then_readies_and_staleness_is_a_published_value() {
        let mut processor = armed();
        processor
            .bind_market(1, &kalshi_contract(), 0, 0)
            .expect("market binds");
        // Before any frame the reference is warming, not unavailable.
        let batch = processor.advance_time(SECOND_NS).expect("advance");
        let _ = batch;
        let warming = processor.take_global_updates();
        let reference = find(&warming, CFBRTI_ORACLE_REFERENCE_ID);
        assert_eq!(reference.quality.state, QualityState::WarmingUp);
        assert!(
            reference
                .quality
                .reasons
                .iter()
                .any(|reason| reason == "cfbrti_warming_up")
        );
        // A frame ten seconds stale still publishes: freshness is a value.
        let now = 130 * SECOND_NS;
        processor
            .ingest(&cf_value(120, 1, "60200", "60150.00000000", 60), 0, now)
            .expect("cf value admitted");
        processor.advance_time(now).expect("advance");
        let ready = processor.take_global_updates();
        let reference = find(&ready, CFBRTI_ORACLE_REFERENCE_ID);
        assert_eq!(reference.quality.state, QualityState::Ready);
        assert_eq!(
            reference.values.get("freshness").and_then(|v| v.as_str()),
            Some("stale"),
            "a ten-second-old index publishes freshness stale, not a status"
        );
        assert_eq!(
            reference
                .values
                .get("staleness_ms")
                .and_then(|v| v.as_u64()),
            Some(10_000)
        );
    }

    #[test]
    fn non_positive_index_value_is_cfbrti_value_invalid_never_a_zero() {
        let mut processor = armed();
        processor
            .bind_market(1, &kalshi_contract(), 0, 0)
            .expect("market binds");
        let now = 120 * SECOND_NS;
        processor
            .ingest(&cf_value(120, 1, "0", "0", 60), 0, now)
            .expect("cf value admitted");
        processor.advance_time(now).expect("advance");
        let globals = processor.take_global_updates();
        let reference = find(&globals, CFBRTI_ORACLE_REFERENCE_ID);
        assert_ne!(reference.quality.state, QualityState::Ready);
        assert!(
            reference
                .quality
                .reasons
                .iter()
                .any(|reason| reason == "cfbrti_value_invalid")
        );
        assert!(
            !reference.values.contains_key("value"),
            "never a published zero"
        );
    }

    #[test]
    fn restart_with_index_armed_rewarms_the_cf_anchor_and_holds_no_checkpoint_cache() {
        let mut processor = armed();
        processor
            .bind_market(1, &kalshi_contract(), 0, 0)
            .expect("market binds");
        let now = 120 * SECOND_NS;
        processor
            .ingest(&cf_value(120, 1, "60200", "60150.00000000", 60), 0, now)
            .expect("cf value admitted");
        processor.advance_time(now).expect("advance");
        let state = processor.checkpoint_state();
        // The checkpoint carries no CF anchor cache: no feature keyed on the
        // global oracle reference, and the settlement twins re-warm their anchor.
        assert!(!state.features.contains_key(CFBRTI_ORACLE_REFERENCE_ID));
        assert!(
            state
                .features
                .contains_key(KALSHI_SETTLEMENT_ANCHOR_STATE_ID)
        );
        // Serialize/deserialize proves no CF field rides the schema.
        let bytes = serde_json::to_string(&state).expect("state serializes");
        assert!(!bytes.contains("60150"), "no anchor value is persisted");
        assert!(
            !bytes.contains("avg_60s"),
            "no CF window state is persisted"
        );
        // A restore always opens a NEW publisher epoch (the spot reference forbids
        // reusing one), which is exactly why the coalescing memory restarts.
        let restored = PredictionSignalProcessor::restore_indexed(
            "cf-index-epoch-resume",
            wall_start(),
            state,
            true,
        )
        .expect("armed restore");
        // After a restore the anchor is empty: the settlement twins are warming
        // again until the socket re-delivers a frame.
        let mut restored = restored;
        restored
            .bind_market(2, &kalshi_contract(), now, now)
            .expect("rebind after restore");
        let batch = restored.advance_time(now).expect("advance after restore");
        let anchor = find(batch.evaluations(), KALSHI_SETTLEMENT_ANCHOR_STATE_ID);
        assert_ne!(
            anchor.quality.state,
            QualityState::Ready,
            "the CF anchor re-warms from the live socket, not the checkpoint"
        );
    }

    /// F1: a CF socket drop after the index has warmed withholds the three
    /// index-dependent instances under `cfbrti_disconnected`; a reconnect and a
    /// fresh frame return them to ready.
    #[test]
    fn cf_socket_drop_after_ready_disconnects_the_index_identities_then_reconnect_readies() {
        let mut processor = armed();
        processor
            .bind_market(1, &kalshi_contract(), 0, 0)
            .expect("market binds");
        let now = 120 * SECOND_NS;
        processor
            .ingest(&cf_value(120, 1, "60200.5", "60150.25000000", 60), 0, now)
            .expect("cf value admitted");
        // The settlement-window frame is the twins' third declared input (F1).
        processor
            .ingest(&cf_settlement(120, 2, 900, 120, 60), 0, now)
            .expect("settlement window admitted");
        let ready = processor.advance_time(now).expect("advance");
        assert_eq!(
            find(ready.evaluations(), KALSHI_SETTLEMENT_ANCHOR_STATE_ID)
                .quality
                .state,
            QualityState::Ready
        );
        // Drain the ready global update, exactly as the runtime does after every
        // admission, so the next drain carries only the disconnect decision.
        let _ = processor.take_global_updates();

        // The index socket drops after the ready decision.
        processor.set_index_connected(false);
        let now = 121 * SECOND_NS;
        let dropped = processor.advance_time(now).expect("advance after drop");
        for id in [
            KALSHI_SETTLEMENT_ANCHOR_STATE_ID,
            KALSHI_ORACLE_DIVERGENCE_ID,
        ] {
            let update = find(dropped.evaluations(), id);
            assert_ne!(update.quality.state, QualityState::Ready, "{id} withheld");
            assert!(
                update
                    .quality
                    .reasons
                    .iter()
                    .any(|reason| reason == "cfbrti_disconnected"),
                "{id} publishes cfbrti_disconnected on a socket drop"
            );
        }
        let globals = processor.take_global_updates();
        let reference = find(&globals, CFBRTI_ORACLE_REFERENCE_ID);
        assert!(
            reference
                .quality
                .reasons
                .iter()
                .any(|reason| reason == "cfbrti_disconnected"),
            "the oracle reference is disconnected too"
        );

        // The reconnect and a fresh frame return every identity to ready.
        processor.set_index_connected(true);
        let now = 122 * SECOND_NS;
        processor
            .ingest(&cf_value(122, 2, "60201.0", "60150.30000000", 60), 0, now)
            .expect("post-reconnect frame admitted");
        let readied = processor
            .advance_time(now)
            .expect("advance after reconnect");
        assert_eq!(
            find(readied.evaluations(), KALSHI_SETTLEMENT_ANCHOR_STATE_ID)
                .quality
                .state,
            QualityState::Ready,
            "the anchor readies after reconnect"
        );
    }

    /// F1: an anchor whose newest print is older than the staleness bound
    /// (10 s) degrades the settlement twins `cfbrti_stale`, while the oracle
    /// reference keeps publishing — staleness is a value there, not a status.
    #[test]
    fn cf_anchor_silence_past_the_staleness_bound_degrades_the_twins() {
        let mut processor = armed();
        processor
            .bind_market(1, &kalshi_contract(), 0, 0)
            .expect("market binds");
        processor
            .ingest(
                &cf_value(120, 1, "60200.5", "60150.25000000", 60),
                0,
                120 * SECOND_NS,
            )
            .expect("cf value admitted");
        // A settlement-window frame is present (F1), so the anchor's only reason
        // is the stale value-channel anchor, not warm-up.
        processor
            .ingest(&cf_settlement(120, 2, 900, 120, 60), 0, 120 * SECOND_NS)
            .expect("settlement window admitted");
        // Eleven seconds of silence: the anchor's print is now past the bound.
        let now = 131 * SECOND_NS;
        let batch = processor.advance_time(now).expect("advance");
        let anchor = find(batch.evaluations(), KALSHI_SETTLEMENT_ANCHOR_STATE_ID);
        assert_ne!(anchor.quality.state, QualityState::Ready);
        assert!(
            anchor
                .quality
                .reasons
                .iter()
                .any(|reason| reason == "cfbrti_stale"),
            "a silence past the bound degrades the anchor cfbrti_stale"
        );
        // The oracle reference is still ready, publishing freshness stale.
        let globals = processor.take_global_updates();
        let reference = find(&globals, CFBRTI_ORACLE_REFERENCE_ID);
        assert_eq!(reference.quality.state, QualityState::Ready);
        assert_eq!(
            reference.values.get("freshness").and_then(|v| v.as_str()),
            Some("stale"),
            "the reference publishes staleness as a value, never cfbrti_stale"
        );
        assert!(
            !reference
                .quality
                .reasons
                .iter()
                .any(|reason| reason == "cfbrti_stale")
        );
    }

    /// F2: a standing subscription refusal on the armed 1 Hz value channel marks
    /// the three index identities unavailable with `cfbrti_subscription_rejected`
    /// and clears on acceptance, even after they were ready.
    #[test]
    fn armed_1hz_subscription_refusal_marks_the_index_identities_unavailable_and_clears() {
        let mut processor = armed();
        processor
            .bind_market(1, &kalshi_contract(), 0, 0)
            .expect("market binds");
        let now = 120 * SECOND_NS;
        processor
            .ingest(&cf_value(120, 1, "60200.5", "60150.25000000", 60), 0, now)
            .expect("cf value admitted");
        // The settlement-window frame is the twins' third declared input (F1).
        processor
            .ingest(&cf_settlement(120, 2, 900, 120, 60), 0, now)
            .expect("settlement window admitted");
        assert_eq!(
            find(
                processor.advance_time(now).expect("advance").evaluations(),
                KALSHI_SETTLEMENT_ANCHOR_STATE_ID
            )
            .quality
            .state,
            QualityState::Ready
        );
        let _ = processor.take_global_updates();

        processor.set_index_channel_reason(Some("cfbrti_subscription_rejected"));
        let now = 121 * SECOND_NS;
        let refused = processor.advance_time(now).expect("advance under refusal");
        for id in [
            KALSHI_SETTLEMENT_ANCHOR_STATE_ID,
            KALSHI_ORACLE_DIVERGENCE_ID,
        ] {
            let update = find(refused.evaluations(), id);
            assert_ne!(update.quality.state, QualityState::Ready);
            assert!(
                update
                    .quality
                    .reasons
                    .iter()
                    .any(|reason| reason == "cfbrti_subscription_rejected"),
                "{id} publishes the refusal reason"
            );
        }
        assert!(
            find(&processor.take_global_updates(), CFBRTI_ORACLE_REFERENCE_ID)
                .quality
                .reasons
                .iter()
                .any(|reason| reason == "cfbrti_subscription_rejected")
        );

        // Acceptance clears it; the standing anchor readies again.
        processor.set_index_channel_reason(None);
        let now = 122 * SECOND_NS;
        assert_eq!(
            find(
                processor
                    .advance_time(now)
                    .expect("advance after accept")
                    .evaluations(),
                KALSHI_SETTLEMENT_ANCHOR_STATE_ID
            )
            .quality
            .state,
            QualityState::Ready,
            "the identities clear the refusal on acceptance"
        );
    }

    /// F3: the settlement-window SOURCE completes exactly four times an hour, and
    /// completeness is false between closes — the r4-shaped hour, computed from
    /// the settlement-window source, never the always-full value-frame average.
    #[test]
    fn settlement_window_source_completes_four_times_an_hour_false_between_closes() {
        let mut processor = armed();
        let mut sequence = 0_u64;
        let mut completions = 0_u32;
        let mut ever_false = false;
        // Four quarters, each a final minute of sixty accumulating prints closing
        // on the 15-minute grid (900, 1800, 2700, 3600 seconds past the hour).
        for quarter in 1..=4_i64 {
            let close_second = quarter * 900;
            for size in 1..=60_u32 {
                let as_of_second = close_second - 60 + i64::from(size);
                sequence += 1;
                let event = cf_settlement(as_of_second, sequence, close_second, as_of_second, size);
                let now = (as_of_second as u64) * SECOND_NS;
                processor
                    .ingest(&event, now, now)
                    .expect("settlement admitted");
                match processor.settlement_window_complete() {
                    Some(true) => completions += 1,
                    Some(false) => ever_false = true,
                    None => panic!("a settlement observation exists after ingest"),
                }
            }
        }
        assert_eq!(completions, 4, "exactly four complete windows in the hour");
        assert!(ever_false, "completeness is false between closes");
    }

    /// F3: a derived quarter close off the 15-minute grid is refused where it is
    /// consumed (E28), never gated on.
    #[test]
    fn settlement_window_off_the_15_minute_grid_is_refused() {
        let mut processor = armed();
        // Close at second 950 = 10:15:50Z, not a :00/:15/:30/:45 boundary.
        let event = cf_settlement(950, 1, 950, 950, 60);
        let now = 950 * SECOND_NS;
        assert!(
            processor.ingest(&event, now, now).is_err(),
            "an off-grid derived close is refused"
        );
    }

    /// A KXBTC15M contract for the quarter opening at `open_second` (its close is
    /// exactly fifteen minutes later, the only window Kalshi declares).
    fn kalshi_contract_opening_at(open_second: i64) -> SourceEvent {
        let mut event = kalshi_contract();
        for (field, second) in [
            ("open_time", open_second),
            ("close_time", open_second + 900),
            ("expected_expiration_time", open_second + 900),
            ("legal_expiration_time", open_second + 1_800),
        ] {
            event
                .values
                .insert(field.to_owned(), json!(timestamp(second)));
        }
        event
    }

    /// F1: the r4-shaped armed hour, read from the PUBLISHED settlement anchor
    /// across four rolling KXBTC15M markets. A ready pre-close emission each
    /// quarter carries `settlement_window_complete` false, the derived
    /// `quarter_close_time` on the 15-minute grid, and exactly the definition's
    /// declared field set (field-set equality). At each grid close the anchor
    /// publishes exactly ONE final ready decision (ruling E37b): the settled
    /// anchor with `settlement_window_complete` true, `window_as_of ==
    /// quarter_close_time` and a zero time-to-close — four an hour — and the very
    /// next decision for the window goes `market_closed`.
    #[test]
    fn armed_r4_replay_publishes_four_final_ready_settlement_completions() {
        let mut processor = armed();

        // The declared field set of the updated Kalshi settlement anchor, keyed
        // exactly as a ready emission must carry it (F1).
        let definition: serde_json::Value =
            serde_json::from_str(super::KALSHI_SETTLEMENT_ANCHOR_STATE_DEFINITION)
                .expect("the settlement-anchor definition parses");
        let declared: std::collections::BTreeSet<String> = definition["fields"]
            .as_array()
            .expect("fields is an array")
            .iter()
            .map(|field| field["name"].as_str().expect("field name").to_owned())
            .collect();

        let mut completions = 0_u32;
        let mut ready_false_emissions = 0_u32;
        let mut final_ready_completions = 0_u32;
        let mut checked_field_set = false;
        for quarter in 1..=4_i64 {
            let close_second = quarter * 900;
            processor
                .bind_market(
                    quarter as u64,
                    &kalshi_contract_opening_at(close_second - 900),
                    ((close_second - 900) as u64) * SECOND_NS,
                    ((close_second - 900) as u64) * SECOND_NS,
                )
                .expect("the quarter's market binds");

            // A ready pre-close decision thirty seconds before the grid close.
            let as_of = close_second - 30;
            let now = (as_of as u64) * SECOND_NS;
            processor
                .ingest(
                    &cf_value(as_of, as_of as u64, "60200.5", "60150.25000000", 60),
                    0,
                    now,
                )
                .expect("cf value admitted");
            processor
                .ingest(
                    &cf_settlement(as_of, as_of as u64, close_second, as_of, 60),
                    0,
                    now,
                )
                .expect("incomplete settlement window admitted");
            let batch = processor.advance_time(now).expect("advance emits");
            let anchor = find(batch.evaluations(), KALSHI_SETTLEMENT_ANCHOR_STATE_ID);
            assert_eq!(
                anchor.quality.state,
                QualityState::Ready,
                "the anchor is ready on the sixty-second average before the close"
            );
            if !checked_field_set {
                let published: std::collections::BTreeSet<String> =
                    anchor.values.keys().cloned().collect();
                assert_eq!(
                    published, declared,
                    "a ready settlement anchor carries exactly the definition's field set"
                );
                checked_field_set = true;
            }
            assert_eq!(
                anchor
                    .values
                    .get("quarter_close_time")
                    .and_then(|value| value.as_str()),
                Some(timestamp(close_second).as_str()),
                "quarter_close_time is the derived close on the 15-minute grid"
            );
            assert_eq!(
                anchor.values.get("settlement_window_complete"),
                Some(&json!(false)),
                "the published field is false before the close"
            );
            ready_false_emissions += 1;

            // The completing frame arrives at the grid close; the producer's
            // completeness flips true (transition false -> true, once per
            // quarter), and the completing decision publishes one final ready
            // settlement decision before going market_closed (E37b).
            assert_eq!(processor.settlement_window_complete(), Some(false));
            let close_now = (close_second as u64) * SECOND_NS;
            processor
                .ingest(
                    &cf_value(
                        close_second,
                        close_second as u64,
                        "60200.5",
                        "60150.25000000",
                        60,
                    ),
                    0,
                    close_now,
                )
                .expect("closing cf value admitted");
            processor
                .ingest(
                    &cf_settlement(
                        close_second,
                        close_second as u64,
                        close_second,
                        close_second,
                        60,
                    ),
                    0,
                    close_now,
                )
                .expect("complete settlement window admitted");
            assert_eq!(
                processor.settlement_window_complete(),
                Some(true),
                "completeness flips true at the grid close"
            );
            completions += 1;
            let closed = processor.advance_time(close_now).expect("advance at close");
            let anchor = find(closed.evaluations(), KALSHI_SETTLEMENT_ANCHOR_STATE_ID);
            // E37b: the completing instant publishes one final ready decision
            // carrying the settled anchor with completeness true.
            assert_eq!(
                anchor.quality.state,
                QualityState::Ready,
                "the anchor publishes one final ready decision at the completing instant (E37b)"
            );
            assert_eq!(
                anchor.values.get("settlement_window_complete"),
                Some(&json!(true)),
                "the final ready decision carries settlement_window_complete true"
            );
            assert_eq!(
                anchor
                    .values
                    .get("window_as_of")
                    .and_then(|value| value.as_str()),
                Some(timestamp(close_second).as_str()),
                "window_as_of equals the quarter close on the final decision"
            );
            assert_eq!(
                anchor
                    .values
                    .get("quarter_close_time")
                    .and_then(|value| value.as_str()),
                Some(timestamp(close_second).as_str())
            );
            assert_eq!(
                anchor.values.get("time_to_close_ms"),
                Some(&json!(0)),
                "the settled anchor reports a zero time to close"
            );
            final_ready_completions += 1;
            // That exactly one final ready decision publishes per window — the
            // next decision goes market_closed — is proved without the back-to-back
            // rollover in `settlement_completion_publishes_exactly_one_final_ready_decision`.

            // Roll the closed market so the next quarter's KXBTC15M can bind.
            processor
                .mark_unavailable(close_now, "scheduled_rollover")
                .expect("the closed market rolls off");
        }
        assert_eq!(
            completions, 4,
            "settlement_window_complete flips false->true exactly four times an hour"
        );
        assert_eq!(
            ready_false_emissions, 4,
            "each quarter publishes a ready anchor carrying settlement_window_complete false"
        );
        assert_eq!(
            final_ready_completions, 4,
            "each grid close publishes exactly one final ready decision with completeness true"
        );
    }

    /// C3b (ruling E51): the settled-window streak folds exactly the final ready
    /// decisions the settlement twins publish, is ready after two settled
    /// windows, and survives a restart through the prediction checkpoint.
    #[test]
    fn the_settled_window_streak_folds_final_decisions_and_survives_a_restart() {
        let mut processor = armed();

        /// Bind, ready and settle one quarter; returns the pre-close batch, which
        /// is the last decision on a bound, open market.
        fn settle_quarter(
            processor: &mut PredictionSignalProcessor,
            quarter: i64,
        ) -> PredictionBatch {
            let close_second = quarter * 900;
            processor
                .bind_market(
                    quarter as u64,
                    &kalshi_contract_opening_at(close_second - 900),
                    ((close_second - 900) as u64) * SECOND_NS,
                    ((close_second - 900) as u64) * SECOND_NS,
                )
                .expect("the quarter's market binds");
            let as_of = close_second - 30;
            let now = (as_of as u64) * SECOND_NS;
            processor
                .ingest(
                    &cf_value(as_of, as_of as u64, "60200.5", "60150.25000000", 60),
                    0,
                    now,
                )
                .expect("cf value admitted");
            processor
                .ingest(
                    &cf_settlement(as_of, as_of as u64, close_second, as_of, 60),
                    0,
                    now,
                )
                .expect("incomplete settlement window admitted");
            let pre_close = processor.advance_time(now).expect("advance emits");

            let close_now = (close_second as u64) * SECOND_NS;
            processor
                .ingest(
                    &cf_value(
                        close_second,
                        close_second as u64,
                        "60200.5",
                        "60150.25000000",
                        60,
                    ),
                    0,
                    close_now,
                )
                .expect("closing cf value admitted");
            processor
                .ingest(
                    &cf_settlement(
                        close_second,
                        close_second as u64,
                        close_second,
                        close_second,
                        60,
                    ),
                    0,
                    close_now,
                )
                .expect("complete settlement window admitted");
            processor
                .advance_time(close_now)
                .expect("the completing decision emits");
            processor
                .mark_unavailable(close_now, "scheduled_rollover")
                .expect("the closed market rolls off");
            pre_close
        }

        // Quarter 1: nothing has settled yet, so the streak is still warming.
        let first = settle_quarter(&mut processor, 1);
        let streak = find(first.evaluations(), KALSHI_WINDOW_STREAK_ID);
        assert_eq!(streak.quality.state, QualityState::WarmingUp);
        assert!(streak.values.is_empty());

        // Quarter 2: one settled window is not yet a streak.
        let second = settle_quarter(&mut processor, 2);
        let streak = find(second.evaluations(), KALSHI_WINDOW_STREAK_ID);
        assert_eq!(streak.quality.state, QualityState::WarmingUp);
        assert_eq!(
            streak.quality.reasons,
            vec!["market_contract_warming_up".to_owned()]
        );

        // Quarter 3: two settled windows, both up (the 60150.25 anchor sits above
        // the 60100 strike), so the streak is ready and two long.
        let third = settle_quarter(&mut processor, 3);
        let streak = find(third.evaluations(), KALSHI_WINDOW_STREAK_ID);
        assert_eq!(streak.quality.state, QualityState::Ready);
        assert_eq!(streak.values["streak_direction"], json!("up"));
        assert_eq!(streak.values["streak_length"], json!(2));
        assert_eq!(streak.values["last_n_outcomes"], json!("uu"));
        assert_eq!(streak.values["up_share_8"], json!("1"));
        assert_eq!(streak.values["settled_window_count"], json!(2));
        assert_eq!(streak.values["last_settled_close"], json!(timestamp(1_800)));

        // Quarter 4: the third settled window extends the run.
        let fourth = settle_quarter(&mut processor, 4);
        let streak = find(fourth.evaluations(), KALSHI_WINDOW_STREAK_ID);
        assert_eq!(streak.values["streak_length"], json!(3));
        assert_eq!(streak.values["last_n_outcomes"], json!("uuu"));

        // The checkpoint carries the run, and a restore reproduces it exactly.
        let state = processor.checkpoint_state();
        let carried = state
            .window_streak
            .clone()
            .expect("a settled window is checkpointed");
        assert_eq!(carried.settled_window_count, 4);
        assert_eq!(carried.streak_length, 4);
        assert_eq!(carried.streak_up, Some(true));
        assert_eq!(carried.recent_outcomes, vec![true, true, true, true]);
        let restored =
            PredictionSignalProcessor::restore_indexed("streak-resume", wall_start(), state, true)
                .expect("the armed checkpoint restores");
        assert_eq!(
            restored
                .checkpoint_state()
                .window_streak
                .expect("the restored processor carries the streak"),
            carried
        );
    }

    /// A processor that has settled no window serializes exactly as it did before
    /// C3b: the streak field is absent, so every pre-C3b checkpoint digest still
    /// matches (ruling N2) and the schema version stays 8.
    #[test]
    fn a_lane_with_no_settled_window_omits_the_streak_from_its_checkpoint() {
        let processor = armed();
        let state = processor.checkpoint_state();
        assert!(state.window_streak.is_none());
        let encoded = serde_json::to_value(&state).expect("checkpoint state serializes");
        assert!(
            encoded.get("window_streak").is_none(),
            "an empty streak adds no field to the checkpoint"
        );
    }

    /// E37b: a settlement window publishes exactly ONE final ready decision. The
    /// completing tick emits the settled anchor with completeness true; re-feeding
    /// the completing frame and advancing again never emits a second — the very
    /// next decision is market_closed.
    #[test]
    fn settlement_completion_publishes_exactly_one_final_ready_decision() {
        let mut processor = armed();
        let close_second = 900_i64;
        processor
            .bind_market(1, &kalshi_contract_opening_at(0), 0, 0)
            .expect("the quarter's market binds");

        // Warm the anchor and window before the close.
        let pre = (close_second - 5) as u64 * SECOND_NS;
        processor
            .ingest(
                &cf_value(
                    close_second - 5,
                    (close_second - 5) as u64,
                    "60200.5",
                    "60150.25000000",
                    60,
                ),
                0,
                pre,
            )
            .expect("cf value admitted");
        processor
            .ingest(
                &cf_settlement(
                    close_second - 5,
                    (close_second - 5) as u64,
                    close_second,
                    close_second - 5,
                    60,
                ),
                0,
                pre,
            )
            .expect("incomplete settlement window admitted");

        // The completing frame at the grid close.
        let close_now = close_second as u64 * SECOND_NS;
        processor
            .ingest(
                &cf_value(
                    close_second,
                    close_second as u64,
                    "60200.5",
                    "60150.25000000",
                    60,
                ),
                0,
                close_now,
            )
            .expect("closing cf value admitted");
        processor
            .ingest(
                &cf_settlement(
                    close_second,
                    close_second as u64,
                    close_second,
                    close_second,
                    60,
                ),
                0,
                close_now,
            )
            .expect("complete settlement window admitted");

        let mut final_ready = 0_u32;
        let batch = processor.advance_time(close_now).expect("advance at close");
        let anchor = find(batch.evaluations(), KALSHI_SETTLEMENT_ANCHOR_STATE_ID);
        if anchor.quality.state == QualityState::Ready
            && anchor.values.get("settlement_window_complete") == Some(&json!(true))
        {
            final_ready += 1;
        }

        // A redelivered completing frame plus another decision must not publish a
        // second final ready update; the window's close is already spent.
        processor
            .ingest(
                &cf_settlement(
                    close_second,
                    (close_second + 1) as u64,
                    close_second,
                    close_second,
                    60,
                ),
                0,
                close_now + SECOND_NS,
            )
            .expect("redelivered settlement window admitted");
        let again = processor
            .advance_time(close_now + SECOND_NS)
            .expect("advance after the close");
        let anchor_again = find(again.evaluations(), KALSHI_SETTLEMENT_ANCHOR_STATE_ID);
        if anchor_again.quality.state == QualityState::Ready
            && anchor_again.values.get("settlement_window_complete") == Some(&json!(true))
        {
            final_ready += 1;
        }
        assert!(
            anchor_again
                .quality
                .reasons
                .iter()
                .any(|reason| reason == "market_closed"),
            "the decision after the final one is market_closed"
        );
        assert_eq!(
            final_ready, 1,
            "exactly one final ready decision publishes per settlement window"
        );
    }

    /// E37b restart safety: the checkpoint records the close whose final decision
    /// was published, so a resume does not replay it. Restoring and re-driving the
    /// same completed window yields market_closed, never a second final ready
    /// decision.
    #[test]
    fn a_restart_after_the_close_does_not_re_publish_the_final_decision() {
        let mut processor = armed();
        let close_second = 900_i64;
        processor
            .bind_market(1, &kalshi_contract_opening_at(0), 0, 0)
            .expect("the quarter's market binds");
        let close_now = close_second as u64 * SECOND_NS;
        processor
            .ingest(
                &cf_value(
                    close_second,
                    close_second as u64,
                    "60200.5",
                    "60150.25000000",
                    60,
                ),
                0,
                close_now,
            )
            .expect("closing cf value admitted");
        processor
            .ingest(
                &cf_settlement(
                    close_second,
                    close_second as u64,
                    close_second,
                    close_second,
                    60,
                ),
                0,
                close_now,
            )
            .expect("complete settlement window admitted");
        let batch = processor.advance_time(close_now).expect("advance at close");
        let anchor = find(batch.evaluations(), KALSHI_SETTLEMENT_ANCHOR_STATE_ID);
        assert_eq!(
            anchor.quality.state,
            QualityState::Ready,
            "the final ready decision publishes before the restart"
        );
        assert_eq!(
            anchor.values.get("settlement_window_complete"),
            Some(&json!(true))
        );

        // The completion is recorded in the checkpoint and round-trips.
        let qc_ms = wall_start()
            .checked_add_signed(Duration::seconds(close_second))
            .expect("the close is representable")
            .timestamp_millis();
        let state = processor.checkpoint_state();
        assert_eq!(
            state.settlement_final_close_ms,
            Some(qc_ms),
            "the checkpoint records the published close"
        );

        // Resume from it and re-drive the same completed window: no second final.
        let mut resumed =
            PredictionSignalProcessor::restore_indexed("epoch-b", wall_start(), state, true)
                .expect("the processor resumes from the checkpoint");
        resumed
            .bind_market(2, &kalshi_contract_opening_at(0), close_now, close_now)
            .expect("the same market re-binds after the resume");
        let after = (close_second as u64 + 1) * SECOND_NS;
        resumed
            .ingest(
                &cf_value(
                    close_second,
                    close_second as u64,
                    "60200.5",
                    "60150.25000000",
                    60,
                ),
                0,
                after,
            )
            .expect("cf value re-warms after resume");
        resumed
            .ingest(
                &cf_settlement(
                    close_second,
                    close_second as u64,
                    close_second,
                    close_second,
                    60,
                ),
                0,
                after,
            )
            .expect("settlement window re-warms after resume");
        let replayed = resumed
            .advance_time(after)
            .expect("advance after the resume");
        let anchor_after = find(replayed.evaluations(), KALSHI_SETTLEMENT_ANCHOR_STATE_ID);
        assert_ne!(
            anchor_after.quality.state,
            QualityState::Ready,
            "the resumed processor does not re-publish the final decision"
        );
        assert!(
            anchor_after
                .quality
                .reasons
                .iter()
                .any(|reason| reason == "market_closed"),
            "the resumed decision for the completed window is market_closed"
        );
    }

    /// F4: two global updates produced before a drain both reach the writer —
    /// appended, never overwritten.
    #[test]
    fn two_global_updates_before_a_drain_both_reach_the_writer() {
        let mut processor = armed();
        processor
            .ingest(
                &cf_value(120, 1, "60200.5", "60150.25000000", 60),
                0,
                120 * SECOND_NS,
            )
            .expect("first cf value");
        processor
            .advance_time(120 * SECOND_NS)
            .expect("first drive");
        // A second, changed frame drives again WITHOUT a drain in between.
        processor
            .ingest(
                &cf_value(121, 2, "60300.5", "60250.25000000", 60),
                0,
                121 * SECOND_NS,
            )
            .expect("second cf value");
        processor
            .advance_time(121 * SECOND_NS)
            .expect("second drive");
        let globals = processor.take_global_updates();
        assert_eq!(
            globals.len(),
            2,
            "both global updates are retained; the second never clobbers the first"
        );
        assert!(
            globals
                .iter()
                .all(|update| update.signal_id == CFBRTI_ORACLE_REFERENCE_ID)
        );
    }

    #[test]
    fn a_disarmed_checkpoint_cannot_restore_into_an_armed_lane() {
        let mut disarmed =
            PredictionSignalProcessor::new(PredictionMarketVenue::Kalshi, EPOCH, wall_start())
                .expect("disarmed builds");
        disarmed
            .bind_market(1, &kalshi_contract(), 0, 0)
            .expect("market binds");
        disarmed.advance_time(SECOND_NS).expect("advance");
        let state = disarmed.checkpoint_state();
        assert!(
            PredictionSignalProcessor::restore_indexed(EPOCH, wall_start(), state, true).is_err(),
            "a thirteen-feature disarmed checkpoint is refused by the armed lane"
        );
    }
}
