use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

use chrono::{DateTime, Duration as ChronoDuration, SecondsFormat, Utc};
use rust_decimal::{Decimal, RoundingStrategy};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use signals_core::{
    Quality, QualityState, SIGNAL_UPDATE_SCHEMA_VERSION, SignalDefinition, SignalScope,
    SignalUpdate, SourceDefinition, SourceEvent, Validate, parse_decimal_text,
};
use thiserror::Error;

use crate::candidate_prediction_signals::{
    AbsorptionState, EarlyMoveFadeContextCandidate, FadeOutcome, MomentumDirection,
};
use crate::candidate_spot_signals::{
    BOOK_STALE_REASON, CandidateMicrostructureSet, EffectiveSpreadOutcome, KyleLambda, KyleOutcome,
    MicrostructureOutcome, MicrostructureParams, RealizedPart, RealizedSpread, ResilienceReading,
    SweepReading, TradeObservation,
};
use crate::horizon::{HorizonRing, horizon_momentum_values, horizon_volatility_values};
use crate::market_state::{
    AdmittedTrade, BookCompletion, BookIngestOutcome, BookMetrics, BookState,
    COINBASE_BOOK_SOURCE_ID, COINBASE_BOOK_SOURCE_VERSION, COINBASE_TRADE_SOURCE_ID,
    COINBASE_TRADE_SOURCE_VERSION, DEPTH_BAND_BPS, FlowSeries, FlowTotals, KRAKEN_BOOK_SOURCE_ID,
    KRAKEN_BOOK_SOURCE_VERSION, KRAKEN_TRADE_SOURCE_ID, KRAKEN_TRADE_SOURCE_VERSION, ONE_SECOND_NS,
    OfiSeries, OfiTotals, SIXTY_SECONDS_NS, STALENESS_BOUND_NS, TEN_SECONDS_NS, checked_mean,
    is_replayed_trade_snapshot, parse_admitted_trade,
};
#[cfg(test)]
use crate::realized_volatility::classify_volatility_ratio;
use crate::realized_volatility::{RealizedVolatility, realized_volatility_values};
use crate::spot_event_join::AggressorSide;
use crate::spot_reference::VenueQuoteSnapshot;
use crate::ta::{FLAT_WINDOW_REASON, TaEngine, TaKind, TaOutcome};
use crate::tick_velocity::{TickVelocity, TickVelocityReading};
use crate::{
    COINBASE_TICKER_SOURCE_ID, KRAKEN_TICKER_SOURCE_ID, MAX_DISPERSION_BPS,
    SpotReferenceCheckpointState, SpotReferenceError, SpotReferenceProcessor, UnavailableReason,
    Venue,
};

pub const REALIZED_VOLATILITY_ID: &str = "btc.external.realized_volatility";
pub const CROSS_VENUE_DISPERSION_ID: &str = "btc.external.cross_venue_dispersion";
pub const MICRO_MOMENTUM_ID: &str = "btc.external.micro_momentum";
pub const HORIZON_MOMENTUM_ID: &str = "btc.external.horizon_momentum";
pub const HORIZON_VOLATILITY_ID: &str = "btc.external.horizon_volatility";
pub const VWAP_DEVIATION_ID: &str = "btc.external.vwap_deviation";
pub const SPOT_MICROPRICE_ID: &str = "btc.external.spot_microprice";
pub const ACTIONABLE_DEPTH_IMBALANCE_ID: &str = "btc.external.actionable_depth_imbalance";
pub const ORDER_FLOW_IMBALANCE_ID: &str = "btc.external.order_flow_imbalance";
pub const AGGRESSOR_FLOW_ID: &str = "btc.external.aggressor_flow";
pub const ABSORPTION_STATE_ID: &str = "btc.external.absorption_state";
pub const LIQUIDITY_SHOCK_ID: &str = "btc.external.liquidity_shock";
// C3a (E51): the seven technical-analysis identities of the blended scope.
pub const ATR_ID: &str = "btc.external.atr";
pub const RSI_ID: &str = "btc.external.rsi";
pub const ROC_ID: &str = "btc.external.roc";
pub const EMA_CROSS_ID: &str = "btc.external.ema_cross";
pub const MACD_ID: &str = "btc.external.macd";
pub const BOLLINGER_ID: &str = "btc.external.bollinger";
pub const ZSCORE_ID: &str = "btc.external.zscore";
// Adopted 2026-09-01 from the candidate set. The computation stays in
// `candidate_prediction_signals` next to the venue readings it was specified
// with; only its identity and wiring live here.
pub use crate::candidate_prediction_signals::EARLY_MOVE_FADE_CONTEXT_ID;
/// Every identity the spot processor publishes on one decision: the hosted spot
/// reference plus each derived feature. Tests and runtime batch guards read this
/// rather than a literal so adopting a feature cannot leave a stale count behind.
pub const SPOT_SIGNAL_SET_COUNT: usize =
    FeatureKind::ALL.len() + 1 + VENUE_SPOT_VENUES.len() * VenueFeatureKind::ALL.len();
/// The adopted spot venues that publish their own single-venue identity set
/// alongside the blended `btc.external.*` layer (ADR 0024), inside the blended
/// spot processor. Binance and Bybit are not here: they run as their own venue
/// lanes on the A6 shell (`venue_spot_signals` for the 13 spot siblings, B1;
/// `venue_derivatives_signals` for the 5 derivatives families, B2), refusal-gated
/// at runtime by `SIGNALS_ENABLED_VENUES` (R7) rather than publishing nothing.
pub const VENUE_SPOT_VENUES: [Venue; 2] = [Venue::Coinbase, Venue::Kraken];
/// Per-venue spot identities published for each venue in [`VENUE_SPOT_VENUES`].
pub const VENUE_SPOT_SIGNAL_COUNT: usize = VenueFeatureKind::ALL.len();
pub const SPOT_FEATURE_VERSION: &str = "1.0.0";
pub const SPOT_SIGNAL_SET_ID: &str = "btc.spot.signal_set";
pub const SPOT_SIGNAL_SET_VERSION: &str = "1.0.0";
pub const SPOT_SIGNAL_KRAKEN_BOOK_DEPTH: u16 = 100;

const OUTPUT_DECIMAL_PLACES: u32 = 8;
const LIQUIDITY_BASELINE_SAMPLES: usize = 60;
const MAX_LIQUIDITY_SAMPLES: usize = 60;
// Shared with the extracted realized-volatility sample ring (ADR 0016).
pub(crate) const MISSED_OBSERVATION_NS: u64 = 2 * ONE_SECOND_NS;
const ABSORPTION_MIN_DEPTH_TURNS_NUMERATOR: u32 = 25;
const ABSORPTION_MIN_DEPTH_TURNS_DENOMINATOR: u32 = 100;
const ABSORPTION_MAX_RESPONSE_BPS: u32 = 1;
const RESPONSIVE_MIN_RESPONSE_BPS: u32 = 3;

const REALIZED_VOLATILITY_DEFINITION: &str =
    include_str!("../../../definitions/btc/signals/btc.external.realized_volatility@1.0.0.json");
const CROSS_VENUE_DISPERSION_DEFINITION: &str =
    include_str!("../../../definitions/btc/signals/btc.external.cross_venue_dispersion@1.0.0.json");
const MICRO_MOMENTUM_DEFINITION: &str =
    include_str!("../../../definitions/btc/signals/btc.external.micro_momentum@1.0.0.json");
const HORIZON_MOMENTUM_DEFINITION: &str =
    include_str!("../../../definitions/btc/signals/btc.external.horizon_momentum@1.0.0.json");
const HORIZON_VOLATILITY_DEFINITION: &str =
    include_str!("../../../definitions/btc/signals/btc.external.horizon_volatility@1.0.0.json");
const VWAP_DEVIATION_DEFINITION: &str =
    include_str!("../../../definitions/btc/signals/btc.external.vwap_deviation@1.0.0.json");
const SPOT_MICROPRICE_DEFINITION: &str =
    include_str!("../../../definitions/btc/signals/btc.external.spot_microprice@1.0.0.json");
const ACTIONABLE_DEPTH_IMBALANCE_DEFINITION: &str = include_str!(
    "../../../definitions/btc/signals/btc.external.actionable_depth_imbalance@1.0.0.json"
);
const ORDER_FLOW_IMBALANCE_DEFINITION: &str =
    include_str!("../../../definitions/btc/signals/btc.external.order_flow_imbalance@1.0.0.json");
const AGGRESSOR_FLOW_DEFINITION: &str =
    include_str!("../../../definitions/btc/signals/btc.external.aggressor_flow@1.0.0.json");
const ABSORPTION_STATE_DEFINITION: &str =
    include_str!("../../../definitions/btc/signals/btc.external.absorption_state@1.0.0.json");
const LIQUIDITY_SHOCK_DEFINITION: &str =
    include_str!("../../../definitions/btc/signals/btc.external.liquidity_shock@1.0.0.json");
const EARLY_MOVE_FADE_CONTEXT_DEFINITION: &str = include_str!(
    "../../../definitions/btc/signals/btc.external.early_move_fade_context@1.0.0.json"
);
const ATR_DEFINITION: &str =
    include_str!("../../../definitions/btc/signals/btc.external.atr@1.0.0.json");
const RSI_DEFINITION: &str =
    include_str!("../../../definitions/btc/signals/btc.external.rsi@1.0.0.json");
const ROC_DEFINITION: &str =
    include_str!("../../../definitions/btc/signals/btc.external.roc@1.0.0.json");
const EMA_CROSS_DEFINITION: &str =
    include_str!("../../../definitions/btc/signals/btc.external.ema_cross@1.0.0.json");
const MACD_DEFINITION: &str =
    include_str!("../../../definitions/btc/signals/btc.external.macd@1.0.0.json");
const BOLLINGER_DEFINITION: &str =
    include_str!("../../../definitions/btc/signals/btc.external.bollinger@1.0.0.json");
const ZSCORE_DEFINITION: &str =
    include_str!("../../../definitions/btc/signals/btc.external.zscore@1.0.0.json");

const COINBASE_BOOK_DEFINITION: &str = include_str!(
    "../../../definitions/btc/sources/coinbase.exchange.btc-usd.book-levels@1.0.0.json"
);
const KRAKEN_BOOK_DEFINITION: &str =
    include_str!("../../../definitions/btc/sources/kraken.spot.btc-usd.book-levels@1.0.0.json");
const COINBASE_TRADE_DEFINITION: &str =
    include_str!("../../../definitions/btc/sources/coinbase.exchange.btc-usd.trades@1.0.0.json");
const KRAKEN_TRADE_DEFINITION: &str =
    include_str!("../../../definitions/btc/sources/kraken.spot.btc-usd.trades@1.0.0.json");

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum FeatureKind {
    RealizedVolatility,
    CrossVenueDispersion,
    MicroMomentum,
    HorizonMomentum,
    HorizonVolatility,
    VwapDeviation,
    SpotMicroprice,
    ActionableDepthImbalance,
    OrderFlowImbalance,
    AggressorFlow,
    AbsorptionState,
    LiquidityShock,
    EarlyMoveFadeContext,
    // C3a (E51): the technical-analysis pack, published on the same one-second
    // reference ring the volatility and momentum readings already drive.
    Atr,
    Rsi,
    Roc,
    EmaCross,
    Macd,
    Bollinger,
    ZScore,
}

impl FeatureKind {
    const ALL: [Self; 20] = [
        Self::RealizedVolatility,
        Self::CrossVenueDispersion,
        Self::MicroMomentum,
        Self::HorizonMomentum,
        Self::HorizonVolatility,
        Self::VwapDeviation,
        Self::SpotMicroprice,
        Self::ActionableDepthImbalance,
        Self::OrderFlowImbalance,
        Self::AggressorFlow,
        Self::AbsorptionState,
        Self::LiquidityShock,
        Self::EarlyMoveFadeContext,
        Self::Atr,
        Self::Rsi,
        Self::Roc,
        Self::EmaCross,
        Self::Macd,
        Self::Bollinger,
        Self::ZScore,
    ];

    const fn id(self) -> &'static str {
        match self {
            Self::RealizedVolatility => REALIZED_VOLATILITY_ID,
            Self::CrossVenueDispersion => CROSS_VENUE_DISPERSION_ID,
            Self::MicroMomentum => MICRO_MOMENTUM_ID,
            Self::HorizonMomentum => HORIZON_MOMENTUM_ID,
            Self::HorizonVolatility => HORIZON_VOLATILITY_ID,
            Self::VwapDeviation => VWAP_DEVIATION_ID,
            Self::SpotMicroprice => SPOT_MICROPRICE_ID,
            Self::ActionableDepthImbalance => ACTIONABLE_DEPTH_IMBALANCE_ID,
            Self::OrderFlowImbalance => ORDER_FLOW_IMBALANCE_ID,
            Self::AggressorFlow => AGGRESSOR_FLOW_ID,
            Self::AbsorptionState => ABSORPTION_STATE_ID,
            Self::LiquidityShock => LIQUIDITY_SHOCK_ID,
            Self::EarlyMoveFadeContext => EARLY_MOVE_FADE_CONTEXT_ID,
            Self::Atr => ATR_ID,
            Self::Rsi => RSI_ID,
            Self::Roc => ROC_ID,
            Self::EmaCross => EMA_CROSS_ID,
            Self::Macd => MACD_ID,
            Self::Bollinger => BOLLINGER_ID,
            Self::ZScore => ZSCORE_ID,
        }
    }

    const fn expected_version(self) -> &'static str {
        SPOT_FEATURE_VERSION
    }

    const fn raw_definition(self) -> &'static str {
        match self {
            Self::RealizedVolatility => REALIZED_VOLATILITY_DEFINITION,
            Self::CrossVenueDispersion => CROSS_VENUE_DISPERSION_DEFINITION,
            Self::MicroMomentum => MICRO_MOMENTUM_DEFINITION,
            Self::HorizonMomentum => HORIZON_MOMENTUM_DEFINITION,
            Self::HorizonVolatility => HORIZON_VOLATILITY_DEFINITION,
            Self::VwapDeviation => VWAP_DEVIATION_DEFINITION,
            Self::SpotMicroprice => SPOT_MICROPRICE_DEFINITION,
            Self::ActionableDepthImbalance => ACTIONABLE_DEPTH_IMBALANCE_DEFINITION,
            Self::OrderFlowImbalance => ORDER_FLOW_IMBALANCE_DEFINITION,
            Self::AggressorFlow => AGGRESSOR_FLOW_DEFINITION,
            Self::AbsorptionState => ABSORPTION_STATE_DEFINITION,
            Self::LiquidityShock => LIQUIDITY_SHOCK_DEFINITION,
            Self::EarlyMoveFadeContext => EARLY_MOVE_FADE_CONTEXT_DEFINITION,
            Self::Atr => ATR_DEFINITION,
            Self::Rsi => RSI_DEFINITION,
            Self::Roc => ROC_DEFINITION,
            Self::EmaCross => EMA_CROSS_DEFINITION,
            Self::Macd => MACD_DEFINITION,
            Self::Bollinger => BOLLINGER_DEFINITION,
            Self::ZScore => ZSCORE_DEFINITION,
        }
    }
}

/// Whether a processor build publishes the per-venue identity lanes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VenueIdentities {
    Published,
    Suppressed,
}

/// One single-venue spot measurement identity (ADR 0024).
///
/// Each variant is published once per venue in [`VENUE_SPOT_VENUES`] as
/// `btc.<venue>.<measurement>`. A per-venue reading depends only on that
/// venue's inputs; the blended `btc.external.*` reading of the same measurement
/// keeps its every-venue AND-gate and stays the reference layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum VenueFeatureKind {
    SpotQuote,
    SpotMicroprice,
    ActionableDepthImbalance,
    LiquidityShock,
    OrderFlowImbalance,
    AggressorFlow,
    VwapDeviation,
    RealizedVolatility,
    MicroMomentum,
    // The two longer-horizon measurements adopted on 2026-09-04 (C2, E49): 1 m /
    // 5 m / 15 m simple-return momentum and realized volatility on the same
    // one-second midpoint ring. Same-venue like the nine above; the blended
    // `btc.external.*` reading is published by the blended processor.
    HorizonMomentum,
    HorizonVolatility,
    // The four microstructure measurements adopted on 2026-09-02. Each is a
    // strictly same-venue formula, so unlike the nine above it has NO blended
    // `btc.external.*` counterpart: that variant is deferred, not omitted (ADR
    // 0023 amendment, ADR 0024).
    EffectiveSpread,
    KyleLambdaDrift,
    SweepDetection,
    BookResilience,
    // C3a (E51): the technical-analysis pack. Each reads only this venue's own
    // one-second midpoint ring, so like the nine above it is single-venue by
    // construction; the blended `btc.external.*` sibling is the reference layer.
    Atr,
    Rsi,
    Roc,
    EmaCross,
    Macd,
    Bollinger,
    ZScore,
    // C3 row 8 (ruling E51): the venue's update rate. It counts admitted
    // events rather than reading the book or the flow series, so like the four
    // microstructure measurements above it has no blended `btc.external.*`
    // counterpart.
    TickVelocity,
}

/// One published venue's identity string and embedded contract document for a
/// given [`VenueFeatureKind`]. A row per venue (rather than a fixed column pair)
/// so a new venue is a table entry, not a struct field (B1: Binance, Bybit).
struct VenueIdentity {
    venue: Venue,
    id: &'static str,
    definition: &'static str,
}

/// The embedded contract documents for one per-venue measurement, one identity
/// per adopted venue. Held as a table rather than per-venue constants so the
/// identity, its definition, and its position in [`VenueFeatureKind::ALL`]
/// cannot drift apart.
struct VenueFeatureContract {
    kind: VenueFeatureKind,
    identities: &'static [VenueIdentity],
}

const VENUE_FEATURE_CONTRACTS: [VenueFeatureContract; 23] = [
    VenueFeatureContract {
        kind: VenueFeatureKind::SpotQuote,
        identities: &[
            VenueIdentity {
                venue: Venue::Coinbase,
                id: "btc.coinbase.spot_quote",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.coinbase.spot_quote@1.0.0.json"
                ),
            },
            VenueIdentity {
                venue: Venue::Kraken,
                id: "btc.kraken.spot_quote",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.kraken.spot_quote@1.0.0.json"
                ),
            },
            VenueIdentity {
                venue: Venue::Binance,
                id: "btc.binance.spot_quote",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.binance.spot_quote@1.0.0.json"
                ),
            },
            VenueIdentity {
                venue: Venue::Bybit,
                id: "btc.bybit.spot_quote",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.bybit.spot_quote@1.0.0.json"
                ),
            },
        ],
    },
    VenueFeatureContract {
        kind: VenueFeatureKind::SpotMicroprice,
        identities: &[
            VenueIdentity {
                venue: Venue::Coinbase,
                id: "btc.coinbase.spot_microprice",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.coinbase.spot_microprice@1.0.0.json"
                ),
            },
            VenueIdentity {
                venue: Venue::Kraken,
                id: "btc.kraken.spot_microprice",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.kraken.spot_microprice@1.0.0.json"
                ),
            },
            VenueIdentity {
                venue: Venue::Binance,
                id: "btc.binance.spot_microprice",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.binance.spot_microprice@1.0.0.json"
                ),
            },
            VenueIdentity {
                venue: Venue::Bybit,
                id: "btc.bybit.spot_microprice",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.bybit.spot_microprice@1.0.0.json"
                ),
            },
        ],
    },
    VenueFeatureContract {
        kind: VenueFeatureKind::ActionableDepthImbalance,
        identities: &[
            VenueIdentity {
                venue: Venue::Coinbase,
                id: "btc.coinbase.actionable_depth_imbalance",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.coinbase.actionable_depth_imbalance@1.0.0.json"
                ),
            },
            VenueIdentity {
                venue: Venue::Kraken,
                id: "btc.kraken.actionable_depth_imbalance",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.kraken.actionable_depth_imbalance@1.0.0.json"
                ),
            },
            VenueIdentity {
                venue: Venue::Binance,
                id: "btc.binance.actionable_depth_imbalance",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.binance.actionable_depth_imbalance@1.0.0.json"
                ),
            },
            VenueIdentity {
                venue: Venue::Bybit,
                id: "btc.bybit.actionable_depth_imbalance",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.bybit.actionable_depth_imbalance@1.0.0.json"
                ),
            },
        ],
    },
    VenueFeatureContract {
        kind: VenueFeatureKind::LiquidityShock,
        identities: &[
            VenueIdentity {
                venue: Venue::Coinbase,
                id: "btc.coinbase.liquidity_shock",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.coinbase.liquidity_shock@1.0.0.json"
                ),
            },
            VenueIdentity {
                venue: Venue::Kraken,
                id: "btc.kraken.liquidity_shock",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.kraken.liquidity_shock@1.0.0.json"
                ),
            },
            VenueIdentity {
                venue: Venue::Binance,
                id: "btc.binance.liquidity_shock",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.binance.liquidity_shock@1.0.0.json"
                ),
            },
            VenueIdentity {
                venue: Venue::Bybit,
                id: "btc.bybit.liquidity_shock",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.bybit.liquidity_shock@1.0.0.json"
                ),
            },
        ],
    },
    VenueFeatureContract {
        kind: VenueFeatureKind::OrderFlowImbalance,
        identities: &[
            VenueIdentity {
                venue: Venue::Coinbase,
                id: "btc.coinbase.order_flow_imbalance",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.coinbase.order_flow_imbalance@1.0.0.json"
                ),
            },
            VenueIdentity {
                venue: Venue::Kraken,
                id: "btc.kraken.order_flow_imbalance",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.kraken.order_flow_imbalance@1.0.0.json"
                ),
            },
            VenueIdentity {
                venue: Venue::Binance,
                id: "btc.binance.order_flow_imbalance",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.binance.order_flow_imbalance@1.0.0.json"
                ),
            },
            VenueIdentity {
                venue: Venue::Bybit,
                id: "btc.bybit.order_flow_imbalance",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.bybit.order_flow_imbalance@1.0.0.json"
                ),
            },
        ],
    },
    VenueFeatureContract {
        kind: VenueFeatureKind::AggressorFlow,
        identities: &[
            VenueIdentity {
                venue: Venue::Coinbase,
                id: "btc.coinbase.aggressor_flow",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.coinbase.aggressor_flow@1.0.0.json"
                ),
            },
            VenueIdentity {
                venue: Venue::Kraken,
                id: "btc.kraken.aggressor_flow",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.kraken.aggressor_flow@1.0.0.json"
                ),
            },
            VenueIdentity {
                venue: Venue::Binance,
                id: "btc.binance.aggressor_flow",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.binance.aggressor_flow@1.0.0.json"
                ),
            },
            VenueIdentity {
                venue: Venue::Bybit,
                id: "btc.bybit.aggressor_flow",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.bybit.aggressor_flow@1.0.0.json"
                ),
            },
        ],
    },
    VenueFeatureContract {
        kind: VenueFeatureKind::VwapDeviation,
        identities: &[
            VenueIdentity {
                venue: Venue::Coinbase,
                id: "btc.coinbase.vwap_deviation",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.coinbase.vwap_deviation@1.0.0.json"
                ),
            },
            VenueIdentity {
                venue: Venue::Kraken,
                id: "btc.kraken.vwap_deviation",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.kraken.vwap_deviation@1.0.0.json"
                ),
            },
            VenueIdentity {
                venue: Venue::Binance,
                id: "btc.binance.vwap_deviation",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.binance.vwap_deviation@1.0.0.json"
                ),
            },
            VenueIdentity {
                venue: Venue::Bybit,
                id: "btc.bybit.vwap_deviation",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.bybit.vwap_deviation@1.0.0.json"
                ),
            },
        ],
    },
    VenueFeatureContract {
        kind: VenueFeatureKind::RealizedVolatility,
        identities: &[
            VenueIdentity {
                venue: Venue::Coinbase,
                id: "btc.coinbase.realized_volatility",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.coinbase.realized_volatility@1.0.0.json"
                ),
            },
            VenueIdentity {
                venue: Venue::Kraken,
                id: "btc.kraken.realized_volatility",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.kraken.realized_volatility@1.0.0.json"
                ),
            },
            VenueIdentity {
                venue: Venue::Binance,
                id: "btc.binance.realized_volatility",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.binance.realized_volatility@1.0.0.json"
                ),
            },
            VenueIdentity {
                venue: Venue::Bybit,
                id: "btc.bybit.realized_volatility",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.bybit.realized_volatility@1.0.0.json"
                ),
            },
        ],
    },
    VenueFeatureContract {
        kind: VenueFeatureKind::MicroMomentum,
        identities: &[
            VenueIdentity {
                venue: Venue::Coinbase,
                id: "btc.coinbase.micro_momentum",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.coinbase.micro_momentum@1.0.0.json"
                ),
            },
            VenueIdentity {
                venue: Venue::Kraken,
                id: "btc.kraken.micro_momentum",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.kraken.micro_momentum@1.0.0.json"
                ),
            },
            VenueIdentity {
                venue: Venue::Binance,
                id: "btc.binance.micro_momentum",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.binance.micro_momentum@1.0.0.json"
                ),
            },
            VenueIdentity {
                venue: Venue::Bybit,
                id: "btc.bybit.micro_momentum",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.bybit.micro_momentum@1.0.0.json"
                ),
            },
        ],
    },
    VenueFeatureContract {
        kind: VenueFeatureKind::HorizonMomentum,
        identities: &[
            VenueIdentity {
                venue: Venue::Coinbase,
                id: "btc.coinbase.horizon_momentum",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.coinbase.horizon_momentum@1.0.0.json"
                ),
            },
            VenueIdentity {
                venue: Venue::Kraken,
                id: "btc.kraken.horizon_momentum",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.kraken.horizon_momentum@1.0.0.json"
                ),
            },
            VenueIdentity {
                venue: Venue::Binance,
                id: "btc.binance.horizon_momentum",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.binance.horizon_momentum@1.0.0.json"
                ),
            },
            VenueIdentity {
                venue: Venue::Bybit,
                id: "btc.bybit.horizon_momentum",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.bybit.horizon_momentum@1.0.0.json"
                ),
            },
        ],
    },
    VenueFeatureContract {
        kind: VenueFeatureKind::HorizonVolatility,
        identities: &[
            VenueIdentity {
                venue: Venue::Coinbase,
                id: "btc.coinbase.horizon_volatility",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.coinbase.horizon_volatility@1.0.0.json"
                ),
            },
            VenueIdentity {
                venue: Venue::Kraken,
                id: "btc.kraken.horizon_volatility",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.kraken.horizon_volatility@1.0.0.json"
                ),
            },
            VenueIdentity {
                venue: Venue::Binance,
                id: "btc.binance.horizon_volatility",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.binance.horizon_volatility@1.0.0.json"
                ),
            },
            VenueIdentity {
                venue: Venue::Bybit,
                id: "btc.bybit.horizon_volatility",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.bybit.horizon_volatility@1.0.0.json"
                ),
            },
        ],
    },
    VenueFeatureContract {
        kind: VenueFeatureKind::EffectiveSpread,
        identities: &[
            VenueIdentity {
                venue: Venue::Coinbase,
                id: "btc.coinbase.effective_spread",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.coinbase.effective_spread@1.0.0.json"
                ),
            },
            VenueIdentity {
                venue: Venue::Kraken,
                id: "btc.kraken.effective_spread",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.kraken.effective_spread@1.0.0.json"
                ),
            },
            VenueIdentity {
                venue: Venue::Binance,
                id: "btc.binance.effective_spread",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.binance.effective_spread@1.0.0.json"
                ),
            },
            VenueIdentity {
                venue: Venue::Bybit,
                id: "btc.bybit.effective_spread",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.bybit.effective_spread@1.0.0.json"
                ),
            },
        ],
    },
    VenueFeatureContract {
        kind: VenueFeatureKind::KyleLambdaDrift,
        identities: &[
            VenueIdentity {
                venue: Venue::Coinbase,
                id: "btc.coinbase.kyle_lambda_drift",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.coinbase.kyle_lambda_drift@1.0.0.json"
                ),
            },
            VenueIdentity {
                venue: Venue::Kraken,
                id: "btc.kraken.kyle_lambda_drift",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.kraken.kyle_lambda_drift@1.0.0.json"
                ),
            },
            VenueIdentity {
                venue: Venue::Binance,
                id: "btc.binance.kyle_lambda_drift",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.binance.kyle_lambda_drift@1.0.0.json"
                ),
            },
            VenueIdentity {
                venue: Venue::Bybit,
                id: "btc.bybit.kyle_lambda_drift",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.bybit.kyle_lambda_drift@1.0.0.json"
                ),
            },
        ],
    },
    VenueFeatureContract {
        kind: VenueFeatureKind::SweepDetection,
        identities: &[
            VenueIdentity {
                venue: Venue::Coinbase,
                id: "btc.coinbase.sweep_detection",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.coinbase.sweep_detection@1.0.0.json"
                ),
            },
            VenueIdentity {
                venue: Venue::Kraken,
                id: "btc.kraken.sweep_detection",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.kraken.sweep_detection@1.0.0.json"
                ),
            },
            VenueIdentity {
                venue: Venue::Binance,
                id: "btc.binance.sweep_detection",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.binance.sweep_detection@1.0.0.json"
                ),
            },
            VenueIdentity {
                venue: Venue::Bybit,
                id: "btc.bybit.sweep_detection",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.bybit.sweep_detection@1.0.0.json"
                ),
            },
        ],
    },
    VenueFeatureContract {
        kind: VenueFeatureKind::BookResilience,
        identities: &[
            VenueIdentity {
                venue: Venue::Coinbase,
                id: "btc.coinbase.book_resilience",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.coinbase.book_resilience@1.0.0.json"
                ),
            },
            VenueIdentity {
                venue: Venue::Kraken,
                id: "btc.kraken.book_resilience",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.kraken.book_resilience@1.0.0.json"
                ),
            },
            VenueIdentity {
                venue: Venue::Binance,
                id: "btc.binance.book_resilience",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.binance.book_resilience@1.0.0.json"
                ),
            },
            VenueIdentity {
                venue: Venue::Bybit,
                id: "btc.bybit.book_resilience",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.bybit.book_resilience@1.0.0.json"
                ),
            },
        ],
    },
    VenueFeatureContract {
        kind: VenueFeatureKind::Atr,
        identities: &[
            VenueIdentity {
                venue: Venue::Coinbase,
                id: "btc.coinbase.atr",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.coinbase.atr@1.0.0.json"
                ),
            },
            VenueIdentity {
                venue: Venue::Kraken,
                id: "btc.kraken.atr",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.kraken.atr@1.0.0.json"
                ),
            },
            VenueIdentity {
                venue: Venue::Binance,
                id: "btc.binance.atr",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.binance.atr@1.0.0.json"
                ),
            },
            VenueIdentity {
                venue: Venue::Bybit,
                id: "btc.bybit.atr",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.bybit.atr@1.0.0.json"
                ),
            },
        ],
    },
    VenueFeatureContract {
        kind: VenueFeatureKind::Rsi,
        identities: &[
            VenueIdentity {
                venue: Venue::Coinbase,
                id: "btc.coinbase.rsi",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.coinbase.rsi@1.0.0.json"
                ),
            },
            VenueIdentity {
                venue: Venue::Kraken,
                id: "btc.kraken.rsi",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.kraken.rsi@1.0.0.json"
                ),
            },
            VenueIdentity {
                venue: Venue::Binance,
                id: "btc.binance.rsi",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.binance.rsi@1.0.0.json"
                ),
            },
            VenueIdentity {
                venue: Venue::Bybit,
                id: "btc.bybit.rsi",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.bybit.rsi@1.0.0.json"
                ),
            },
        ],
    },
    VenueFeatureContract {
        kind: VenueFeatureKind::Roc,
        identities: &[
            VenueIdentity {
                venue: Venue::Coinbase,
                id: "btc.coinbase.roc",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.coinbase.roc@1.0.0.json"
                ),
            },
            VenueIdentity {
                venue: Venue::Kraken,
                id: "btc.kraken.roc",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.kraken.roc@1.0.0.json"
                ),
            },
            VenueIdentity {
                venue: Venue::Binance,
                id: "btc.binance.roc",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.binance.roc@1.0.0.json"
                ),
            },
            VenueIdentity {
                venue: Venue::Bybit,
                id: "btc.bybit.roc",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.bybit.roc@1.0.0.json"
                ),
            },
        ],
    },
    VenueFeatureContract {
        kind: VenueFeatureKind::EmaCross,
        identities: &[
            VenueIdentity {
                venue: Venue::Coinbase,
                id: "btc.coinbase.ema_cross",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.coinbase.ema_cross@1.0.0.json"
                ),
            },
            VenueIdentity {
                venue: Venue::Kraken,
                id: "btc.kraken.ema_cross",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.kraken.ema_cross@1.0.0.json"
                ),
            },
            VenueIdentity {
                venue: Venue::Binance,
                id: "btc.binance.ema_cross",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.binance.ema_cross@1.0.0.json"
                ),
            },
            VenueIdentity {
                venue: Venue::Bybit,
                id: "btc.bybit.ema_cross",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.bybit.ema_cross@1.0.0.json"
                ),
            },
        ],
    },
    VenueFeatureContract {
        kind: VenueFeatureKind::Macd,
        identities: &[
            VenueIdentity {
                venue: Venue::Coinbase,
                id: "btc.coinbase.macd",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.coinbase.macd@1.0.0.json"
                ),
            },
            VenueIdentity {
                venue: Venue::Kraken,
                id: "btc.kraken.macd",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.kraken.macd@1.0.0.json"
                ),
            },
            VenueIdentity {
                venue: Venue::Binance,
                id: "btc.binance.macd",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.binance.macd@1.0.0.json"
                ),
            },
            VenueIdentity {
                venue: Venue::Bybit,
                id: "btc.bybit.macd",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.bybit.macd@1.0.0.json"
                ),
            },
        ],
    },
    VenueFeatureContract {
        kind: VenueFeatureKind::Bollinger,
        identities: &[
            VenueIdentity {
                venue: Venue::Coinbase,
                id: "btc.coinbase.bollinger",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.coinbase.bollinger@1.0.0.json"
                ),
            },
            VenueIdentity {
                venue: Venue::Kraken,
                id: "btc.kraken.bollinger",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.kraken.bollinger@1.0.0.json"
                ),
            },
            VenueIdentity {
                venue: Venue::Binance,
                id: "btc.binance.bollinger",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.binance.bollinger@1.0.0.json"
                ),
            },
            VenueIdentity {
                venue: Venue::Bybit,
                id: "btc.bybit.bollinger",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.bybit.bollinger@1.0.0.json"
                ),
            },
        ],
    },
    VenueFeatureContract {
        kind: VenueFeatureKind::ZScore,
        identities: &[
            VenueIdentity {
                venue: Venue::Coinbase,
                id: "btc.coinbase.zscore",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.coinbase.zscore@1.0.0.json"
                ),
            },
            VenueIdentity {
                venue: Venue::Kraken,
                id: "btc.kraken.zscore",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.kraken.zscore@1.0.0.json"
                ),
            },
            VenueIdentity {
                venue: Venue::Binance,
                id: "btc.binance.zscore",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.binance.zscore@1.0.0.json"
                ),
            },
            VenueIdentity {
                venue: Venue::Bybit,
                id: "btc.bybit.zscore",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.bybit.zscore@1.0.0.json"
                ),
            },
        ],
    },
    VenueFeatureContract {
        kind: VenueFeatureKind::TickVelocity,
        identities: &[
            VenueIdentity {
                venue: Venue::Coinbase,
                id: "btc.coinbase.tick_velocity",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.coinbase.tick_velocity@1.0.0.json"
                ),
            },
            VenueIdentity {
                venue: Venue::Kraken,
                id: "btc.kraken.tick_velocity",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.kraken.tick_velocity@1.0.0.json"
                ),
            },
            VenueIdentity {
                venue: Venue::Binance,
                id: "btc.binance.tick_velocity",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.binance.tick_velocity@1.0.0.json"
                ),
            },
            VenueIdentity {
                venue: Venue::Bybit,
                id: "btc.bybit.tick_velocity",
                definition: include_str!(
                    "../../../definitions/btc/signals/btc.bybit.tick_velocity@1.0.0.json"
                ),
            },
        ],
    },
];

impl VenueFeatureKind {
    pub(crate) const ALL: [Self; 23] = [
        Self::SpotQuote,
        Self::SpotMicroprice,
        Self::ActionableDepthImbalance,
        Self::LiquidityShock,
        Self::OrderFlowImbalance,
        Self::AggressorFlow,
        Self::VwapDeviation,
        Self::RealizedVolatility,
        Self::MicroMomentum,
        Self::HorizonMomentum,
        Self::HorizonVolatility,
        Self::EffectiveSpread,
        Self::KyleLambdaDrift,
        Self::SweepDetection,
        Self::BookResilience,
        Self::Atr,
        Self::Rsi,
        Self::Roc,
        Self::EmaCross,
        Self::Macd,
        Self::Bollinger,
        Self::ZScore,
        Self::TickVelocity,
    ];

    /// The table row for this kind, found by identity rather than by position,
    /// so reordering the table cannot silently re-point a kind at another
    /// measurement's contract.
    fn contract(self) -> &'static VenueFeatureContract {
        VENUE_FEATURE_CONTRACTS
            .iter()
            .find(|contract| contract.kind == self)
            .expect("every per-venue feature kind has a contract row")
    }

    /// The identity row for this measurement on `venue`, found by venue so a new
    /// venue is a table entry. Panics only if asked for a venue the table does
    /// not publish this kind for — never on the published set.
    fn identity(self, venue: Venue) -> &'static VenueIdentity {
        self.contract()
            .identities
            .iter()
            .find(|identity| identity.venue == venue)
            .expect("every published venue has an identity row for this kind")
    }

    pub(crate) fn id(self, venue: Venue) -> &'static str {
        self.identity(venue).id
    }

    pub(crate) fn raw_definition(self, venue: Venue) -> &'static str {
        self.identity(venue).definition
    }

    const fn expected_version(self) -> &'static str {
        SPOT_FEATURE_VERSION
    }
}

/// The per-venue feature kind one shared TA identity publishes under.
const fn venue_ta_kind(kind: TaKind) -> VenueFeatureKind {
    match kind {
        TaKind::Atr => VenueFeatureKind::Atr,
        TaKind::Rsi => VenueFeatureKind::Rsi,
        TaKind::Roc => VenueFeatureKind::Roc,
        TaKind::EmaCross => VenueFeatureKind::EmaCross,
        TaKind::Macd => VenueFeatureKind::Macd,
        TaKind::Bollinger => VenueFeatureKind::Bollinger,
        TaKind::ZScore => VenueFeatureKind::ZScore,
    }
}

/// The blended feature kind one shared TA identity publishes under.
const fn blended_ta_kind(kind: TaKind) -> FeatureKind {
    match kind {
        TaKind::Atr => FeatureKind::Atr,
        TaKind::Rsi => FeatureKind::Rsi,
        TaKind::Roc => FeatureKind::Roc,
        TaKind::EmaCross => FeatureKind::EmaCross,
        TaKind::Macd => FeatureKind::Macd,
        TaKind::Bollinger => FeatureKind::Bollinger,
        TaKind::ZScore => FeatureKind::ZScore,
    }
}

/// Every per-venue spot identity published for `venue`, in publication order.
pub fn venue_spot_signal_ids(venue: Venue) -> Vec<&'static str> {
    VenueFeatureKind::ALL
        .iter()
        .map(|kind| kind.id(venue))
        .collect()
}

/// The fixed quality reason a spot feature carries while its venue is marked
/// unavailable. Sole authority for the two reason strings, shared by the
/// blended and per-venue emitters.
const fn venue_unavailable_reason(venue: Venue) -> &'static str {
    match venue {
        Venue::Coinbase => "coinbase_venue_unavailable",
        Venue::Kraken => "kraken_venue_unavailable",
        Venue::Binance => "binance_venue_unavailable",
        Venue::Bybit => "bybit_venue_unavailable",
    }
}

/// The fixed quality reason a venue-scoped identity carries while it is still
/// warming on that venue's own admitted stream.
const fn venue_warming_reason(venue: Venue) -> &'static str {
    match venue {
        Venue::Coinbase => "coinbase_warming_up",
        Venue::Kraken => "kraken_warming_up",
        Venue::Binance => "binance_warming_up",
        Venue::Bybit => "bybit_warming_up",
    }
}

/// The fixed quality reason a venue-scoped identity carries while that venue's
/// stream has been silent past the shared staleness bound.
const fn venue_stale_reason(venue: Venue) -> &'static str {
    match venue {
        Venue::Coinbase => "coinbase_stale",
        Venue::Kraken => "kraken_stale",
        Venue::Binance => "binance_stale",
        Venue::Bybit => "bybit_stale",
    }
}

/// The published field map of one `tick_velocity` reading. Shared by every
/// scope that publishes the identity so the field set cannot drift per venue.
pub(crate) fn tick_velocity_values(
    reading: &TickVelocityReading,
    observation_time: &str,
) -> BTreeMap<String, Value> {
    BTreeMap::from([
        (
            "quote_updates_1s".to_owned(),
            json!(reading.quote_updates_1s),
        ),
        (
            "quote_updates_10s".to_owned(),
            json!(reading.quote_updates_10s),
        ),
        ("trades_1s".to_owned(), json!(reading.trades_1s)),
        ("trades_10s".to_owned(), json!(reading.trades_10s)),
        (
            "midpoint_changes_10s".to_owned(),
            json!(reading.midpoint_changes_10s),
        ),
        (
            "velocity_ratio_present".to_owned(),
            json!(reading.velocity_ratio_present),
        ),
        (
            "velocity_ratio".to_owned(),
            decimal_value(reading.velocity_ratio),
        ),
        (
            "baseline_quote_updates".to_owned(),
            json!(reading.baseline_quote_updates),
        ),
        (
            "baseline_window_ms".to_owned(),
            json!(reading.baseline_window_ms),
        ),
        ("observation_time".to_owned(), json!(observation_time)),
    ])
}

/// The fixed quality reason naming an invalid reconstructed book for `venue`.
const fn venue_book_invalid_reason(venue: Venue) -> &'static str {
    match venue {
        Venue::Coinbase => "coinbase_book_invalid",
        Venue::Kraken => "kraken_book_invalid",
        Venue::Binance => "binance_book_invalid",
        Venue::Bybit => "bybit_book_invalid",
    }
}

/// The quote currency a per-venue tier-1 identity declares (R6/E42), or `None`
/// for the USD venues whose adopted definitions carry no `quote_currency` field.
/// Injected into every ready value map by [`VenueLane::emit_feature`] so the
/// emitted output matches the definition; `None` leaves the USD venues'
/// byte-identical output untouched.
const fn venue_quote_currency(venue: Venue) -> Option<&'static str> {
    match venue {
        Venue::Coinbase | Venue::Kraken => None,
        // E47: every `btc.binance.*` / `btc.bybit.*` identity now quotes in USD.
        // The venue trades against USDT, but its price, notional and quote-cost
        // fields are converted through the USDT/USD reference at decision time,
        // so the declared quote currency is `usd` (the lowercase `usdt` value of
        // E8/E42 is retired — nothing shipped).
        Venue::Binance | Venue::Bybit => Some("usd"),
    }
}

/// The readiness reason a converting identity publishes when the USDT/USD
/// reference is absent or older than the staleness bound (E47(c)/E19).
pub(crate) const USDT_REFERENCE_STALE_REASON: &str = "usdt_reference_stale";

/// Apply the E47 USDT→USD conversion to one converting identity's decision.
///
/// `quote_fields` is the identity's quote-denominated field set; empty means the
/// identity does not convert and this is a no-op. When the reference is fresh,
/// every present quote field is multiplied by the rate (`decimal_value`
/// formatting, 8dp half-even), and `usdt_usd_rate` / `usdt_usd_rate_age_ms` are
/// inserted so a ready update carries them (they are declared required). When
/// the reference is absent or stale the update is forced non-ready with
/// [`USDT_REFERENCE_STALE_REASON`]; an already-non-ready update is left as-is
/// (its missing market data is the reason it is not ready). Used by both the
/// spot and derivatives venue emitters.
pub(crate) fn apply_usdt_conversion(
    reference: Option<crate::usdt_reference::UsdtReferenceSnapshot>,
    quote_fields: &[&str],
    ready_values: &mut Option<BTreeMap<String, Value>>,
    reasons: &mut Vec<String>,
) -> Result<(), SpotSignalError> {
    if quote_fields.is_empty() {
        return Ok(());
    }
    let Some(values) = ready_values.as_mut() else {
        // Already non-ready for another reason; the rate is moot.
        return Ok(());
    };
    let fresh = reference.is_some_and(|snapshot| !snapshot.is_stale());
    if !fresh {
        *ready_values = None;
        reasons.push(USDT_REFERENCE_STALE_REASON.to_owned());
        return Ok(());
    }
    let snapshot = reference.expect("fresh reference is present");
    let rate = snapshot.rate().expect("fresh reference has a rate");
    let age_ms = snapshot.age_ms().expect("fresh reference has an age");
    for field in quote_fields {
        if let Some(existing) = values.get(*field) {
            let text = existing.as_str().ok_or_else(|| {
                SpotSignalError::State(format!("{field} is not a decimal string"))
            })?;
            let parsed = parse_decimal_text(text)?;
            let converted = snapshot.convert(parsed).expect("fresh reference converts");
            values.insert((*field).to_owned(), decimal_value(converted));
        }
    }
    values.insert("usdt_usd_rate".to_owned(), decimal_value(rate));
    values.insert("usdt_usd_rate_age_ms".to_owned(), json!(age_ms));
    Ok(())
}

/// The quote-denominated output fields of one venue spot identity that E47
/// converts USDT→USD. Empty means the identity has no quote-denominated field
/// (its outputs are base sizes, counts, bps ratios or durations) and so does not
/// convert, publishes no `usdt_usd_rate`, and never goes `usdt_reference_stale`.
/// Only the USDT venues convert; the USD venues (Coinbase/Kraken) return an
/// empty slice here and are never handed a reference, so their bytes are
/// untouched.
fn venue_quote_fields(venue: Venue, kind: VenueFeatureKind) -> &'static [&'static str] {
    if !matches!(venue, Venue::Binance | Venue::Bybit) {
        return &[];
    }
    match kind {
        VenueFeatureKind::SpotQuote => &["bid", "ask", "midpoint"],
        VenueFeatureKind::SpotMicroprice => &["microprice", "book_midpoint"],
        VenueFeatureKind::AggressorFlow => &[
            "signed_notional_usd_1s",
            "signed_notional_usd_10s",
            "signed_notional_usd_60s",
            "gross_notional_usd_60s",
        ],
        VenueFeatureKind::VwapDeviation => &["vwap_10s", "vwap_60s", "midpoint"],
        VenueFeatureKind::EffectiveSpread => &["midpoint", "midpoint_at_horizon", "trade_price"],
        // Kyle's lambda is a price-impact coefficient in quote per base unit
        // (E47(e) ratified: it converts). `lambda_drift` is a difference of two
        // such coefficients and converts identically.
        VenueFeatureKind::KyleLambdaDrift => {
            &["kyle_lambda", "kyle_lambda_previous", "lambda_drift"]
        }
        // C3a: the average true range and the bar close it is measured against
        // are price levels, so they convert (E47 / C3 §2); `atr_bps` is
        // scale-free and is not listed.
        VenueFeatureKind::Atr => &["atr", "close"],
        // The two exponential moving averages are price levels; `spread_bps` is
        // their scale-free ratio and is not listed.
        VenueFeatureKind::EmaCross => &["ema_9", "ema_21"],
        // Base sizes, counts, bps ratios and durations only — no conversion.
        VenueFeatureKind::ActionableDepthImbalance
        | VenueFeatureKind::LiquidityShock
        | VenueFeatureKind::OrderFlowImbalance
        | VenueFeatureKind::RealizedVolatility
        | VenueFeatureKind::MicroMomentum
        // C2: horizon momentum/volatility are simple returns and their RMS —
        // scale-free, so no field is quote-denominated and nothing converts.
        | VenueFeatureKind::HorizonMomentum
        | VenueFeatureKind::HorizonVolatility
        | VenueFeatureKind::SweepDetection
        | VenueFeatureKind::BookResilience
        | VenueFeatureKind::Rsi
        | VenueFeatureKind::Roc
        | VenueFeatureKind::Macd
        | VenueFeatureKind::Bollinger
        | VenueFeatureKind::ZScore
        // Counts, a dimensionless ratio and a duration: nothing to convert.
        | VenueFeatureKind::TickVelocity => &[],
    }
}

/// The instance-state reason a venue publishes when its 5 bps depth band reaches
/// the outermost captured book level (§6). Only Binance's depth20 image can
/// truncate the band; the full-book USD venues and Bybit (200 levels) never do,
/// and the registry carries no twin for them (§12).
const fn venue_band_exceeds_reason(venue: Venue) -> Option<&'static str> {
    match venue {
        Venue::Binance => Some("binance_band_exceeds_captured_depth"),
        Venue::Coinbase | Venue::Kraken | Venue::Bybit => None,
    }
}

#[derive(Debug, Clone)]
pub(crate) struct FeatureEmitter {
    definition: SignalDefinition,
    next_sequence: u64,
    lifetime_output_count: u64,
    ever_ready: bool,
    last_quality: Option<Quality>,
    reason_entries: BTreeMap<String, u64>,
}

impl FeatureEmitter {
    fn new(kind: FeatureKind) -> Result<Self, SpotSignalError> {
        Self::from_contract(kind.raw_definition(), kind.id(), kind.expected_version())
    }

    fn venue(kind: VenueFeatureKind, venue: Venue) -> Result<Self, SpotSignalError> {
        Self::from_contract(
            kind.raw_definition(venue),
            kind.id(venue),
            kind.expected_version(),
        )
    }

    pub(crate) fn from_contract(
        raw: &'static str,
        id: &'static str,
        version: &'static str,
    ) -> Result<Self, SpotSignalError> {
        let definition = parse_definition::<SignalDefinition>(raw, id)?;
        if definition.signal_id != id || definition.version != version {
            return Err(SpotSignalError::Definition(format!(
                "embedded definition identity {}@{} does not match {id}@{version}",
                definition.signal_id, definition.version,
            )));
        }
        Ok(Self {
            definition,
            next_sequence: 0,
            lifetime_output_count: 0,
            ever_ready: false,
            last_quality: None,
            reason_entries: BTreeMap::new(),
        })
    }

    pub(crate) fn emit(
        &mut self,
        publisher_epoch: &str,
        produced_at: &str,
        event_time: String,
        ready_values: Option<BTreeMap<String, Value>>,
        mut reasons: Vec<String>,
    ) -> Result<SignalUpdate, SpotSignalError> {
        let lifetime_output_count = self
            .lifetime_output_count
            .checked_add(1)
            .ok_or(SpotSignalError::Counter)?;
        reasons.sort();
        reasons.dedup();
        let ready = ready_values.is_some();
        let quality = if ready {
            Quality {
                state: QualityState::Ready,
                reasons: Vec::new(),
            }
        } else {
            if reasons.is_empty() {
                return Err(SpotSignalError::State(
                    "non-ready feature output requires a reason".to_owned(),
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
            scope: SignalScope::Global,
        };
        update.validate_against(&self.definition)?;
        self.record_reasons(&quality)?;
        self.ever_ready |= ready;
        self.last_quality = Some(quality);
        if ready {
            self.next_sequence = self
                .next_sequence
                .checked_add(1)
                .ok_or(SpotSignalError::SequenceExhausted)?;
        }
        self.lifetime_output_count = lifetime_output_count;
        Ok(update)
    }

    pub(crate) fn checkpoint(&self) -> SpotSignalFeatureCheckpoint {
        SpotSignalFeatureCheckpoint {
            previous_next_sequence: self.next_sequence,
            lifetime_output_count: self.lifetime_output_count,
            ever_ready: self.ever_ready,
            last_quality: self.last_quality.clone(),
            quality_reason_entries: self.reason_entries.clone(),
        }
    }

    /// Re-seat prior output state from a durable checkpoint. Output identity is
    /// never reused: a resumed process starts its sequence at zero under a new
    /// publisher epoch.
    pub(crate) fn restore(&mut self, checkpoint: &SpotSignalFeatureCheckpoint) {
        self.ever_ready = checkpoint.ever_ready;
        self.last_quality = checkpoint.last_quality.clone();
        self.reason_entries = checkpoint.quality_reason_entries.clone();
        self.lifetime_output_count = checkpoint.lifetime_output_count;
        self.next_sequence = 0;
    }

    /// Restart the per-generation output sequence at zero for a new file
    /// generation without disturbing the warm feature state (C5 rotation).
    /// Mirrors the sequence reset [`FeatureEmitter::restore`] performs, but
    /// keeps `ever_ready`, the last quality and the reason tallies so a rotated
    /// generation carries the warm identity straight across the boundary.
    pub(crate) fn reset_output_sequence(&mut self) {
        self.next_sequence = 0;
    }

    fn record_reasons(&mut self, quality: &Quality) -> Result<(), SpotSignalError> {
        let prior: BTreeSet<&str> = self
            .last_quality
            .as_ref()
            .into_iter()
            .flat_map(|value| value.reasons.iter().map(String::as_str))
            .collect();
        for reason in &quality.reasons {
            if !prior.contains(reason.as_str()) {
                let value = self.reason_entries.entry(reason.clone()).or_default();
                *value = value.checked_add(1).ok_or(SpotSignalError::Counter)?;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
struct LiquiditySample {
    logical_time_ns: u64,
    depth: Decimal,
    spread_bps: Decimal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum LiquidityState {
    Normal,
    Thinning,
    Shock,
    Recovering,
}

#[derive(Debug, Clone)]
struct BookPairMetrics {
    coinbase: BookMetrics,
    kraken: BookMetrics,
}

#[derive(Debug, Clone, Copy)]
struct MomentumMetrics {
    return_1s: Decimal,
    return_10s: Decimal,
    return_30s: Decimal,
}

#[derive(Debug, Clone, Copy)]
struct DepthMetrics {
    coinbase_imbalance: Decimal,
    kraken_imbalance: Decimal,
    combined_imbalance: Decimal,
    bid_depth: Decimal,
    ask_depth: Decimal,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct FlowMetrics {
    pub(crate) one_second: FlowTotals,
    pub(crate) ten_seconds: FlowTotals,
    pub(crate) sixty_seconds: FlowTotals,
}

/// Everything one venue's identities read on a single decision.
///
/// Assembled by the processor from state it already owns, then handed to the
/// lane, so the lane never reaches back into shared state and a per-venue
/// reading cannot depend on the other venue.
#[derive(Debug, Clone)]
pub(crate) struct VenueDecision {
    pub(crate) now_ns: u64,
    pub(crate) quote: Option<VenueQuoteSnapshot>,
    pub(crate) book: Option<BookMetrics>,
    pub(crate) book_invalid: bool,
    pub(crate) alive: bool,
    pub(crate) flow: FlowMetrics,
    pub(crate) flow_warmed: bool,
    pub(crate) flow_fresh: bool,
    pub(crate) ofi_one_second: OfiTotals,
    pub(crate) ofi_ten_seconds: OfiTotals,
    pub(crate) ofi_sixty_seconds: OfiTotals,
    pub(crate) ofi_warmed: bool,
    pub(crate) venue_unavailable: bool,
    /// Whether the venue's 5 bps depth band reached the outermost captured book
    /// level on either side (Binance depth20, §6). `false` for the full-book USD
    /// venues and Bybit (200 levels), so their decisions are unchanged.
    pub(crate) band_truncated: bool,
}

/// The instance-status reason a microstructure identity publishes while it has
/// no reading yet, for the two shapes of warm-up.
const TRADE_WARMING_REASON: &str = "trade_history_warming_up";
const BOOK_WARMING_REASON: &str = "book_history_warming_up";

/// Map a microstructure computation's own not-ready term onto the frozen
/// stream-v3 instance-status vocabulary.
///
/// Sole authority for the mapping. The four computations name their states in
/// their own vocabulary, while every published quality reason must already be
/// an entry of `signals_core::INSTANCE_STATUS_REASONS`; adopting them
/// deliberately adds no vocabulary term, exactly as the staleness gate reuses
/// [`BOOK_STALE_REASON`].
fn microstructure_reason(reason: &str) -> &'static str {
    match reason {
        // No midpoint has been observed for this venue yet, so no book-derived
        // baseline exists to price a trade against.
        "no_prior_midpoint" | "book_baseline_warming_up" => BOOK_WARMING_REASON,
        // The regression window is degenerate — its sum of squared signed
        // volume is zero — so the slope is undefined rather than merely
        // unwarmed.
        "degenerate_signed_volume" => "model_denominator_zero",
        // "awaiting_forward_midpoint", "kyle_window_warming_up", and any term
        // added later: not enough completed per-trade observations yet.
        _ => TRADE_WARMING_REASON,
    }
}

/// One microstructure payload retained for publication: the values a ready
/// update carries and the event time it is stamped with.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MicrostructureReading {
    event_time: String,
    values: BTreeMap<String, Value>,
}

/// What one microstructure identity publishes at the next decision: either a
/// retained reading, or the frozen-vocabulary reason it has none.
///
/// The four measurements are event-driven — they produce a reading when a book
/// group completes or a trade batch closes — while every spot identity
/// publishes on the one-second decision clock. The slot is what bridges the
/// two, and holding a fully built payload rather than the computation's own
/// types keeps the decision path free of arithmetic that could fail there.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MicrostructureSlot {
    reading: Option<MicrostructureReading>,
    reason: &'static str,
}

impl MicrostructureSlot {
    const fn empty(reason: &'static str) -> Self {
        Self {
            reading: None,
            reason,
        }
    }

    fn publish(&mut self, reading: MicrostructureReading) {
        self.reading = Some(reading);
    }

    /// Drop the retained reading: the state it was derived from is gone.
    fn withdraw(&mut self, reason: &'static str) {
        self.reading = None;
        self.reason = reason;
    }

    /// Record a not-ready reason without discarding a retained reading.
    ///
    /// Used only where one not-ready observation does not invalidate what the
    /// computation still holds — a trade with no prevailing midpoint, which can
    /// only occur before the first midpoint of this venue and therefore only
    /// while the slot is already empty.
    fn note_reason_if_empty(&mut self, reason: &'static str) {
        if self.reading.is_none() {
            self.reason = reason;
        }
    }
}

/// The four microstructure slots of one venue lane.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MicrostructureReadings {
    effective_spread: MicrostructureSlot,
    kyle: MicrostructureSlot,
    sweep: MicrostructureSlot,
    resilience: MicrostructureSlot,
    /// Whether any realized-spread measurement of this venue has resolved since
    /// the last reset. The definition ranks a resolved measurement above a
    /// still-awaiting trade, so once this is set the awaiting reading of a new
    /// trade no longer supersedes the retained resolved one.
    effective_spread_resolved: bool,
}

impl MicrostructureReadings {
    const fn new() -> Self {
        Self {
            effective_spread: MicrostructureSlot::empty(TRADE_WARMING_REASON),
            kyle: MicrostructureSlot::empty(TRADE_WARMING_REASON),
            sweep: MicrostructureSlot::empty(TRADE_WARMING_REASON),
            resilience: MicrostructureSlot::empty(BOOK_WARMING_REASON),
            effective_spread_resolved: false,
        }
    }

    /// Drop every retained reading. Called exactly when the venue's four
    /// computations are reset, so nothing survives a causal gap.
    fn withdraw_all(&mut self, reason: &'static str) {
        self.effective_spread.withdraw(reason);
        self.kyle.withdraw(reason);
        self.sweep.withdraw(reason);
        self.resilience.withdraw(reason);
        self.effective_spread_resolved = false;
    }
}

fn effective_spread_reading(
    effective_spread_bps: Decimal,
    midpoint: Decimal,
    trade_price: Decimal,
    aggressor: AggressorSide,
    realized: RealizedPart,
    realized_horizon_ms: u64,
    trade_time: String,
) -> MicrostructureReading {
    let values = BTreeMap::from([
        (
            "effective_spread_bps".to_owned(),
            decimal_value(effective_spread_bps),
        ),
        (
            "realized_present".to_owned(),
            json!(realized.realized_present),
        ),
        (
            "realized_spread_bps".to_owned(),
            decimal_value(realized.realized_spread_bps),
        ),
        (
            "realized_state".to_owned(),
            json!(realized.realized_state.as_str()),
        ),
        ("midpoint".to_owned(), decimal_value(midpoint)),
        (
            "midpoint_at_horizon".to_owned(),
            decimal_value(realized.midpoint_at_horizon),
        ),
        ("trade_price".to_owned(), decimal_value(trade_price)),
        ("aggressor_side".to_owned(), json!(aggressor.as_str())),
        ("realized_horizon_ms".to_owned(), json!(realized_horizon_ms)),
        ("trade_time".to_owned(), json!(trade_time.as_str())),
    ]);
    MicrostructureReading {
        event_time: trade_time,
        values,
    }
}

fn kyle_reading(
    lambda: &KyleLambda,
    window_trades: usize,
    window_close: String,
) -> Result<MicrostructureReading, SpotSignalError> {
    let values = BTreeMap::from([
        ("kyle_lambda".to_owned(), decimal_value(lambda.kyle_lambda)),
        (
            "kyle_lambda_previous".to_owned(),
            decimal_value(lambda.kyle_lambda_previous),
        ),
        (
            "lambda_drift".to_owned(),
            decimal_value(lambda.lambda_drift),
        ),
        (
            "window_trades".to_owned(),
            json!(u32::try_from(window_trades).map_err(|_| SpotSignalError::Counter)?),
        ),
        ("pair_count".to_owned(), json!(lambda.pair_count)),
        ("window_close".to_owned(), json!(window_close.as_str())),
    ]);
    Ok(MicrostructureReading {
        event_time: window_close,
        values,
    })
}

fn sweep_reading(
    sweep: &SweepReading,
    min_levels: usize,
    observation_time: String,
) -> Result<MicrostructureReading, SpotSignalError> {
    let values = BTreeMap::from([
        (
            "detection_state".to_owned(),
            json!(sweep.detection_state.as_str()),
        ),
        ("direction".to_owned(), json!(sweep.direction.as_str())),
        ("swept_levels".to_owned(), json!(sweep.swept_levels)),
        ("swept_size".to_owned(), decimal_value(sweep.swept_size)),
        (
            "min_levels".to_owned(),
            json!(u32::try_from(min_levels).map_err(|_| SpotSignalError::Counter)?),
        ),
        (
            "observation_time".to_owned(),
            json!(observation_time.as_str()),
        ),
    ]);
    Ok(MicrostructureReading {
        event_time: observation_time,
        values,
    })
}

fn resilience_reading(
    resilience: &ResilienceReading,
    params: MicrostructureParams,
    observation_time: String,
) -> MicrostructureReading {
    let values = BTreeMap::from([
        ("state".to_owned(), json!(resilience.state.as_str())),
        (
            "recovery_ms_present".to_owned(),
            json!(resilience.recovery_ms_present),
        ),
        ("recovery_ms".to_owned(), json!(resilience.recovery_ms)),
        (
            "depletion_side".to_owned(),
            json!(resilience.depletion_side.as_str()),
        ),
        (
            "pre_depletion_size".to_owned(),
            decimal_value(resilience.pre_depletion_size),
        ),
        (
            "deplete_fraction".to_owned(),
            decimal_value(params.deplete_fraction),
        ),
        (
            "recover_fraction".to_owned(),
            decimal_value(params.recover_fraction),
        ),
        (
            "observation_time".to_owned(),
            json!(observation_time.as_str()),
        ),
    ]);
    MicrostructureReading {
        event_time: observation_time,
        values,
    }
}

/// One venue's per-venue identity lane: its emitters plus the rolling state
/// those identities derive from.
///
/// The lane's rolling state is deliberately its own rather than a view over the
/// blended pipeline's. `mark_unavailable` clears the blended flow, OFI,
/// volatility and liquidity history for every venue whenever one venue drops,
/// because the blended readings are an every-venue AND; a per-venue lane must
/// re-warm only for the venue that actually dropped. Sharing one history could
/// not express both rules, and the blended frames must stay byte-identical
/// (ADR 0024).
#[derive(Debug, Clone)]
pub(crate) struct VenueLane {
    venue: Venue,
    emitters: BTreeMap<VenueFeatureKind, FeatureEmitter>,
    volatility: RealizedVolatility,
    /// The C2 longer-horizon (1 m / 5 m / 15 m) ring, fed the same one-second
    /// midpoint samples as `volatility` above but held independently so the
    /// micro identities keep their byte-for-byte contract.
    horizon: HorizonRing,
    liquidity_samples: VecDeque<LiquiditySample>,
    previous_liquidity_state: Option<LiquidityState>,
    /// This process's logical-clock baseline, so a microstructure observation
    /// can be stamped with the time it was ADMITTED rather than the decision
    /// that publishes it. Rendered through the one shared
    /// [`logical_timestamp_text`], never a second offset calculation.
    wall_start: DateTime<Utc>,
    /// This venue's update-rate counters (C3 row 8). Rolling market-relative
    /// state like the volatility ring: cleared with the rest of the lane's
    /// history when the venue drops, and deliberately not checkpointed.
    tick_velocity: TickVelocity,
    /// The lane's own microstructure driver over its own raw-event join.
    ///
    /// The driver is venue-partitioned internally, and this lane drives only
    /// [`Self::venue`]: the peer partition is never fed, never read, and stays
    /// at its initial state for the life of the lane. Owning one driver per
    /// lane rather than sharing one is what makes "gated by its own venue
    /// only" structural instead of conventional — the lane physically cannot
    /// reach the other venue's observations.
    microstructure: CandidateMicrostructureSet,
    readings: MicrostructureReadings,
    /// The USDT/USD reference snapshot for the decision currently being emitted
    /// (E47). Set at the top of [`Self::emit`] and read by [`Self::emit_feature`]
    /// so the per-feature emit bodies need no new parameter; `None` for the USD
    /// venues (Coinbase/Kraken), whose output is byte-identical and never
    /// converts. It is transient decision state, not lane state, and is cleared
    /// after each decision.
    current_reference: Option<crate::usdt_reference::UsdtReferenceSnapshot>,
    /// The C3a technical-analysis unit over this venue's own one-second
    /// midpoint ring (E51). It is driven from the same decision-time midpoint
    /// the volatility ring reads, so the two share one continuity rule.
    ta: TaEngine,
}

impl VenueLane {
    pub(crate) fn new(venue: Venue, wall_start: DateTime<Utc>) -> Result<Self, SpotSignalError> {
        let mut emitters = BTreeMap::new();
        for kind in VenueFeatureKind::ALL {
            emitters.insert(kind, FeatureEmitter::venue(kind, venue)?);
        }
        Ok(Self {
            venue,
            emitters,
            volatility: RealizedVolatility::new(),
            horizon: HorizonRing::new(),
            liquidity_samples: VecDeque::with_capacity(MAX_LIQUIDITY_SAMPLES),
            previous_liquidity_state: None,
            wall_start,
            tick_velocity: TickVelocity::new(),
            microstructure: CandidateMicrostructureSet::with_defaults(),
            readings: MicrostructureReadings::new(),
            current_reference: None,
            ta: TaEngine::new()?,
        })
    }

    /// Restart every feature emitter's per-generation output sequence at zero,
    /// keeping the lane's warm rings and windows intact (C5 rotation).
    pub(crate) fn reset_output_sequences(&mut self) {
        for emitter in self.emitters.values_mut() {
            emitter.reset_output_sequence();
        }
    }

    /// Join one completed book group for this lane's venue and absorb whatever
    /// the four computations produced.
    pub(crate) fn observe_book(
        &mut self,
        completion: &BookCompletion,
    ) -> Result<(), SpotSignalError> {
        let bbo = completion.current_bbo;
        let outcomes = self.microstructure.observe_book(
            self.venue,
            completion.logical_receipt_ns,
            bbo.bid,
            bbo.bid_size,
            bbo.ask,
            bbo.ask_size,
        )?;
        self.absorb(outcomes)
    }

    /// Count one admitted best-quote update for this lane's venue (C3 row 8).
    ///
    /// Called once per admitted quote event, before any decision reads the
    /// counters, so the published rate is a fold over exactly the admitted
    /// quote stream. A midpoint equal to the previously admitted one is a quote
    /// update without a midpoint change.
    pub(crate) fn observe_quote(&mut self, logical_receipt_ns: u64, midpoint: Decimal) {
        self.tick_velocity
            .observe_quote(logical_receipt_ns, midpoint);
    }

    /// Join one admitted trade row for this lane's venue and absorb whatever
    /// batch it closed.
    pub(crate) fn observe_trade(
        &mut self,
        logical_receipt_ns: u64,
        event_time: &str,
        trade: AdmittedTrade,
    ) -> Result<(), SpotSignalError> {
        self.tick_velocity.observe_trade(logical_receipt_ns);
        let outcomes = self.microstructure.observe_trade(
            self.venue,
            logical_receipt_ns,
            event_time,
            trade.price,
            trade.size,
            trade.aggressor,
        )?;
        self.absorb(outcomes)
    }

    /// Fold the driver's outcome stream into the four publication slots.
    ///
    /// The slots are last-writer-wins in admission order, which is what makes
    /// the published reading "the most recent one" deterministically: the
    /// driver emits observations in admission order and this walks them in that
    /// order. Three rules are not last-writer-wins, and each is stated at its
    /// site: an effective-spread not-ready never discards a retained reading,
    /// an awaiting effective-spread reading never supersedes a resolved
    /// measurement, and a reset withdraws every reading at once.
    fn absorb(&mut self, outcomes: Vec<MicrostructureOutcome>) -> Result<(), SpotSignalError> {
        let params = self.microstructure.params();
        for outcome in outcomes {
            if outcome.venue() != self.venue {
                return Err(SpotSignalError::State(format!(
                    "{:?} lane received a {:?} microstructure outcome",
                    self.venue,
                    outcome.venue()
                )));
            }
            match outcome {
                MicrostructureOutcome::Book {
                    receipt_ns,
                    resolved,
                    kyle,
                    resilience,
                    ..
                } => {
                    let stamp = logical_timestamp_text(self.wall_start, receipt_ns)?;
                    self.readings.resilience.publish(resilience_reading(
                        &resilience,
                        params,
                        stamp.clone(),
                    ));
                    match kyle {
                        // The estimator recomputes its whole window on every
                        // midpoint update, so a not-ready verdict supersedes an
                        // earlier lambda rather than leaving it standing.
                        KyleOutcome::Ready(lambda) => self.readings.kyle.publish(kyle_reading(
                            &lambda,
                            params.window_trades,
                            stamp,
                        )?),
                        KyleOutcome::NotReady { reason } => {
                            self.readings.kyle.withdraw(microstructure_reason(reason));
                        }
                    }
                    self.absorb_resolved(&resolved, params.realized_horizon_ms)?;
                }
                MicrostructureOutcome::Batch {
                    receipt_ns,
                    sweep,
                    trades,
                    ..
                } => {
                    let stamp = logical_timestamp_text(self.wall_start, receipt_ns)?;
                    self.readings.sweep.publish(sweep_reading(
                        &sweep,
                        params.sweep_min_levels,
                        stamp.clone(),
                    )?);
                    for observation in &trades {
                        self.absorb_trade(observation, params.realized_horizon_ms, &stamp)?;
                    }
                }
                // A gap reset this venue's four computations, so every reading
                // derived from the pre-gap state must go with them; a batch
                // merely withheld from the computations disturbs nothing they
                // retain, and the decision-time freshness gate already covers
                // it.
                MicrostructureOutcome::NotReady { reason, reset, .. } => {
                    if reset {
                        self.readings.withdraw_all(reason);
                    }
                }
            }
        }
        Ok(())
    }

    /// Absorb one trade's observation: first the measurements its event time
    /// resolved (older trades), then — only while nothing has resolved yet —
    /// the trade's own still-awaiting reading.
    ///
    /// The definition ranks the two: "the published reading is the most recent
    /// resolved measurement when one exists, otherwise the most recent priced
    /// trade still awaiting its horizon". Under continuous flow every trade
    /// closes a batch, so publishing the awaiting reading last would hold the
    /// identity at `awaiting_horizon` forever and never surface a realized
    /// spread. The awaiting reading is therefore only the warm-up shape.
    fn absorb_trade(
        &mut self,
        observation: &TradeObservation,
        realized_horizon_ms: u64,
        trade_time: &str,
    ) -> Result<(), SpotSignalError> {
        self.absorb_resolved(&observation.resolved, realized_horizon_ms)?;
        match &observation.effective {
            EffectiveSpreadOutcome::Ready {
                effective_spread_bps,
                midpoint,
                trade_price,
                aggressor,
                realized,
            } => {
                let reading = effective_spread_reading(
                    *effective_spread_bps,
                    *midpoint,
                    *trade_price,
                    *aggressor,
                    *realized,
                    realized_horizon_ms,
                    trade_time.to_owned(),
                );
                if !self.readings.effective_spread_resolved {
                    self.readings.effective_spread.publish(reading);
                }
            }
            // A trade with no prevailing midpoint is only possible before this
            // venue's first midpoint, when nothing is retained; it never
            // withdraws a reading.
            EffectiveSpreadOutcome::NotReady { reason } => self
                .readings
                .effective_spread
                .note_reason_if_empty(microstructure_reason(reason)),
        }
        Ok(())
    }

    /// Publish the newest resolved measurement, if any. Resolutions arrive in
    /// trade order, so the last of them is the most recent. A resolution always
    /// supersedes whatever the slot held, and from the first one onward the
    /// identity publishes resolved measurements only.
    fn absorb_resolved(
        &mut self,
        resolved: &[RealizedSpread],
        realized_horizon_ms: u64,
    ) -> Result<(), SpotSignalError> {
        let Some(measurement) = resolved.last() else {
            return Ok(());
        };
        let trade_time = logical_timestamp_text(self.wall_start, measurement.trade_time_ns)?;
        self.readings.effective_spread_resolved = true;
        self.readings
            .effective_spread
            .publish(effective_spread_reading(
                measurement.effective_spread_bps,
                measurement.midpoint,
                measurement.trade_price,
                measurement.aggressor,
                measurement.realized,
                realized_horizon_ms,
                trade_time,
            ));
        Ok(())
    }

    pub(crate) fn definitions(&self) -> impl Iterator<Item = &SignalDefinition> {
        self.emitters.values().map(|emitter| &emitter.definition)
    }

    /// Drop every rolling measurement this venue derives. Called only for the
    /// venue that dropped, never for its peer.
    pub(crate) fn clear_history(&mut self) -> Result<(), SpotSignalError> {
        self.volatility.clear();
        self.ta.clear();
        self.horizon.clear();
        self.liquidity_samples.clear();
        self.previous_liquidity_state = None;
        self.tick_velocity.clear();
        self.clear_microstructure()
    }

    /// This venue's per-feature checkpoint slots, keyed by published signal id.
    /// Used by the standalone venue lane (B1) to persist its 13 emitters exactly
    /// as [`SpotSignalProcessor::checkpoint_state`] persists the spot lane's.
    pub(crate) fn checkpoint_features(&self) -> BTreeMap<String, SpotSignalFeatureCheckpoint> {
        self.emitters
            .iter()
            .map(|(kind, emitter)| (kind.id(self.venue).to_owned(), emitter.checkpoint()))
            .collect()
    }

    /// Restore the 13 emitters from a checkpoint written by
    /// [`Self::checkpoint_features`]; a missing feature is a foreign checkpoint.
    pub(crate) fn restore_features(
        &mut self,
        features: &BTreeMap<String, SpotSignalFeatureCheckpoint>,
    ) -> Result<(), SpotSignalError> {
        for (kind, emitter) in &mut self.emitters {
            let id = kind.id(self.venue);
            let checkpoint = features.get(id).ok_or_else(|| {
                SpotSignalError::Checkpoint(format!("checkpoint is missing feature {id}"))
            })?;
            emitter.restore(checkpoint);
        }
        Ok(())
    }

    /// Return this venue's microstructure state — its join buffer, its four
    /// computations and their retained readings — to the initial state. A gap
    /// makes the state before it non-continuous with the state after, so it is
    /// discarded rather than folded forward.
    fn clear_microstructure(&mut self) -> Result<(), SpotSignalError> {
        self.microstructure.clear_venue(self.venue)?;
        self.readings.withdraw_all(BOOK_STALE_REASON);
        Ok(())
    }

    pub(crate) fn emit(
        &mut self,
        publisher_epoch: &str,
        produced_at: &str,
        decision: &VenueDecision,
        reference: Option<crate::usdt_reference::UsdtReferenceSnapshot>,
    ) -> Result<Vec<SignalUpdate>, SpotSignalError> {
        // E47: the reference snapshot for this decision, read by `emit_feature`
        // for the converting venues. Cleared before returning so it never leaks
        // into a later decision.
        self.current_reference = reference;
        self.volatility.update_price(
            decision.quote.as_ref().map(|quote| quote.midpoint),
            decision.now_ns,
        );
        self.ta.update(
            decision.quote.as_ref().map(|quote| quote.midpoint),
            decision.now_ns,
        )?;
        self.horizon.update_price(
            decision.quote.as_ref().map(|quote| quote.midpoint),
            decision.now_ns,
        );
        // A trade batch stays open until something closes it, so the join is
        // flushed before any derived value is read: a decision never publishes
        // a half-observed batch (join caller precondition).
        let flushed = self.microstructure.flush()?;
        self.absorb(flushed)?;
        self.emit_outputs(publisher_epoch, produced_at, decision, reference)
    }

    /// Publish every identity of this lane from its current rings without
    /// taking a new sample or closing the open trade batch: the event-driven
    /// republish between health ticks.
    pub(crate) fn emit_outputs(
        &mut self,
        publisher_epoch: &str,
        produced_at: &str,
        decision: &VenueDecision,
        reference: Option<crate::usdt_reference::UsdtReferenceSnapshot>,
    ) -> Result<Vec<SignalUpdate>, SpotSignalError> {
        self.current_reference = reference;
        let momentum = self.momentum_metrics();
        let updates = self.emit_all(publisher_epoch, produced_at, decision, momentum);
        // Transient decision state never survives the decision (see the field).
        self.current_reference = None;
        updates
    }

    fn emit_all(
        &mut self,
        publisher_epoch: &str,
        produced_at: &str,
        decision: &VenueDecision,
        momentum: Option<MomentumMetrics>,
    ) -> Result<Vec<SignalUpdate>, SpotSignalError> {
        Ok(vec![
            self.emit_spot_quote(publisher_epoch, produced_at, decision)?,
            self.emit_spot_microprice(publisher_epoch, produced_at, decision)?,
            self.emit_depth_imbalance(publisher_epoch, produced_at, decision)?,
            self.emit_liquidity_shock(publisher_epoch, produced_at, decision)?,
            self.emit_ofi(publisher_epoch, produced_at, decision)?,
            self.emit_aggressor_flow(publisher_epoch, produced_at, decision)?,
            self.emit_vwap_deviation(publisher_epoch, produced_at, decision)?,
            self.emit_realized_volatility(publisher_epoch, produced_at, decision)?,
            self.emit_micro_momentum(publisher_epoch, produced_at, decision, momentum)?,
            self.emit_horizon_momentum(publisher_epoch, produced_at, decision)?,
            self.emit_horizon_volatility(publisher_epoch, produced_at, decision)?,
            self.emit_microstructure(
                VenueFeatureKind::EffectiveSpread,
                publisher_epoch,
                produced_at,
                decision,
            )?,
            self.emit_microstructure(
                VenueFeatureKind::KyleLambdaDrift,
                publisher_epoch,
                produced_at,
                decision,
            )?,
            self.emit_microstructure(
                VenueFeatureKind::SweepDetection,
                publisher_epoch,
                produced_at,
                decision,
            )?,
            self.emit_microstructure(
                VenueFeatureKind::BookResilience,
                publisher_epoch,
                produced_at,
                decision,
            )?,
            self.emit_ta(TaKind::Atr, publisher_epoch, produced_at, decision)?,
            self.emit_ta(TaKind::Rsi, publisher_epoch, produced_at, decision)?,
            self.emit_ta(TaKind::Roc, publisher_epoch, produced_at, decision)?,
            self.emit_ta(TaKind::EmaCross, publisher_epoch, produced_at, decision)?,
            self.emit_ta(TaKind::Macd, publisher_epoch, produced_at, decision)?,
            self.emit_ta(TaKind::Bollinger, publisher_epoch, produced_at, decision)?,
            self.emit_ta(TaKind::ZScore, publisher_epoch, produced_at, decision)?,
            self.emit_tick_velocity(publisher_epoch, produced_at, decision)?,
        ])
    }

    /// Publish this venue's update rate (C3 row 8).
    ///
    /// Readiness has one condition: ten seconds of lane time observed since the
    /// venue's first admitted event, so the ten-second counts are complete. The
    /// counters need neither a book nor a quote snapshot — they describe the
    /// arrival process itself — so a stale book does not withdraw them; a venue
    /// marked unavailable does, through the shared `emit_feature` gate.
    fn emit_tick_velocity(
        &mut self,
        publisher_epoch: &str,
        produced_at: &str,
        decision: &VenueDecision,
    ) -> Result<SignalUpdate, SpotSignalError> {
        let warmed = self.tick_velocity.warmed(decision.now_ns);
        let reading = self.tick_velocity.reading(decision.now_ns);
        // A venue silent past the shared staleness bound is stale, not slow: the
        // arrival process is no longer being observed, so the rate is withdrawn
        // rather than published as a confident zero. Uniform with every other
        // identity on this lane.
        let (values, reason) = if !decision.alive {
            (None, venue_stale_reason(self.venue))
        } else if warmed {
            (
                Some(tick_velocity_values(&reading, produced_at)),
                venue_warming_reason(self.venue),
            )
        } else {
            (None, venue_warming_reason(self.venue))
        };
        self.emit_feature(
            VenueFeatureKind::TickVelocity,
            publisher_epoch,
            produced_at,
            produced_at.to_owned(),
            values,
            vec![reason.to_owned()],
            decision.venue_unavailable,
        )
    }

    /// Publish one C3a technical-analysis identity from this venue's TA unit.
    ///
    /// The unit decides only between a reading, a warm-up and the degenerate
    /// flat Z-score window; the scope-specific warm-up word is this lane's, and
    /// it is the same `reference_history_warming_up` the volatility and momentum
    /// identities publish off the same ring.
    fn emit_ta(
        &mut self,
        ta_kind: TaKind,
        publisher_epoch: &str,
        produced_at: &str,
        decision: &VenueDecision,
    ) -> Result<SignalUpdate, SpotSignalError> {
        let (values, reasons) = match self.ta.outcome(ta_kind, produced_at, self.wall_start)? {
            TaOutcome::Ready(values) => (Some(values), Vec::new()),
            TaOutcome::Warming => (None, vec!["reference_history_warming_up".to_owned()]),
            TaOutcome::FlatWindow => (None, vec![FLAT_WINDOW_REASON.to_owned()]),
        };
        self.emit_feature(
            venue_ta_kind(ta_kind),
            publisher_epoch,
            produced_at,
            produced_at.to_owned(),
            values,
            reasons,
            decision.venue_unavailable,
        )
    }

    /// Publish one microstructure identity from its retained slot.
    ///
    /// Readiness has exactly two conditions and no third: this venue's book
    /// must be inside the shared staleness bound at the decision time, and the
    /// slot must hold a reading. The freshness half is asked of
    /// [`crate::spot_event_join::SpotEventJoin::book_fresh`] — the one
    /// authority for that fact — and it covers both halves of the gate, a
    /// retained book past the bound and a venue with no retained book at all.
    fn emit_microstructure(
        &mut self,
        kind: VenueFeatureKind,
        publisher_epoch: &str,
        produced_at: &str,
        decision: &VenueDecision,
    ) -> Result<SignalUpdate, SpotSignalError> {
        let fresh = self
            .microstructure
            .join()
            .book_fresh(self.venue, decision.now_ns);
        let slot = self.slot(kind)?;
        let (event_time, values, reason) = match (fresh, &slot.reading) {
            (true, Some(reading)) => (
                reading.event_time.clone(),
                Some(reading.values.clone()),
                None,
            ),
            (true, None) => (produced_at.to_owned(), None, Some(slot.reason)),
            (false, _) => (produced_at.to_owned(), None, Some(BOOK_STALE_REASON)),
        };
        self.emit_feature(
            kind,
            publisher_epoch,
            produced_at,
            event_time,
            values,
            reason.map(str::to_owned).into_iter().collect(),
            decision.venue_unavailable,
        )
    }

    fn slot(&self, kind: VenueFeatureKind) -> Result<&MicrostructureSlot, SpotSignalError> {
        match kind {
            VenueFeatureKind::EffectiveSpread => Ok(&self.readings.effective_spread),
            VenueFeatureKind::KyleLambdaDrift => Ok(&self.readings.kyle),
            VenueFeatureKind::SweepDetection => Ok(&self.readings.sweep),
            VenueFeatureKind::BookResilience => Ok(&self.readings.resilience),
            other => Err(SpotSignalError::State(format!(
                "{} is not a microstructure identity",
                other.id(self.venue)
            ))),
        }
    }

    /// The immediate transition emission for this venue's book-derived
    /// identities, mirroring the blended pipeline's transition set restricted to
    /// the identities this venue actually publishes.
    pub(crate) fn emit_book_failure(
        &mut self,
        publisher_epoch: &str,
        produced_at: &str,
        venue_unavailable: bool,
    ) -> Result<Vec<SignalUpdate>, SpotSignalError> {
        self.liquidity_samples.clear();
        self.previous_liquidity_state = None;
        // An invalid book is a causal break for the microstructure family too:
        // its join buffer, computations and retained readings all describe a
        // book that no longer exists.
        self.clear_microstructure()?;
        let book_reason = venue_book_invalid_reason(self.venue).to_owned();
        let mut updates = vec![
            self.emit_feature(
                VenueFeatureKind::SpotMicroprice,
                publisher_epoch,
                produced_at,
                produced_at.to_owned(),
                None,
                vec![book_reason.clone()],
                venue_unavailable,
            )?,
            self.emit_feature(
                VenueFeatureKind::ActionableDepthImbalance,
                publisher_epoch,
                produced_at,
                produced_at.to_owned(),
                None,
                vec![book_reason.clone()],
                venue_unavailable,
            )?,
            self.emit_feature(
                VenueFeatureKind::OrderFlowImbalance,
                publisher_epoch,
                produced_at,
                produced_at.to_owned(),
                None,
                vec![
                    book_reason.clone(),
                    "book_flow_history_warming_up".to_owned(),
                ],
                venue_unavailable,
            )?,
            self.emit_feature(
                VenueFeatureKind::LiquidityShock,
                publisher_epoch,
                produced_at,
                produced_at.to_owned(),
                None,
                vec![book_reason.clone(), "book_history_warming_up".to_owned()],
                venue_unavailable,
            )?,
        ];
        // The four microstructure identities read this venue's book directly,
        // so they fail closed with the rest of the book-derived set.
        for kind in [
            VenueFeatureKind::EffectiveSpread,
            VenueFeatureKind::KyleLambdaDrift,
            VenueFeatureKind::SweepDetection,
            VenueFeatureKind::BookResilience,
        ] {
            updates.push(self.emit_feature(
                kind,
                publisher_epoch,
                produced_at,
                produced_at.to_owned(),
                None,
                vec![book_reason.clone(), BOOK_STALE_REASON.to_owned()],
                venue_unavailable,
            )?);
        }
        Ok(updates)
    }

    fn emit_spot_quote(
        &mut self,
        publisher_epoch: &str,
        produced_at: &str,
        decision: &VenueDecision,
    ) -> Result<SignalUpdate, SpotSignalError> {
        let values = decision
            .quote
            .as_ref()
            .map(
                |quote| -> Result<BTreeMap<String, Value>, SpotSignalError> {
                    let spread_bps = checked_mul(
                        checked_div(checked_sub(quote.ask, quote.bid)?, quote.midpoint)?,
                        Decimal::from(10_000_u32),
                    )?;
                    Ok(BTreeMap::from([
                        ("bid".to_owned(), decimal_value(quote.bid)),
                        ("ask".to_owned(), decimal_value(quote.ask)),
                        ("midpoint".to_owned(), decimal_value(quote.midpoint)),
                        ("spread_bps".to_owned(), decimal_value(spread_bps)),
                        ("input_age_ms".to_owned(), json!(quote.age_ms)),
                        ("observation_time".to_owned(), json!(quote.event_time)),
                    ]))
                },
            )
            .transpose()?;
        let event_time = decision
            .quote
            .as_ref()
            .map_or_else(|| produced_at.to_owned(), |quote| quote.event_time.clone());
        self.emit_feature(
            VenueFeatureKind::SpotQuote,
            publisher_epoch,
            produced_at,
            event_time,
            values,
            vec!["reference_constituents_unavailable".to_owned()],
            decision.venue_unavailable,
        )
    }

    fn emit_spot_microprice(
        &mut self,
        publisher_epoch: &str,
        produced_at: &str,
        decision: &VenueDecision,
    ) -> Result<SignalUpdate, SpotSignalError> {
        let values = decision
            .book
            .as_ref()
            .map(|book| -> Result<BTreeMap<String, Value>, SpotSignalError> {
                Ok(BTreeMap::from([
                    ("microprice".to_owned(), decimal_value(book.microprice)),
                    ("book_midpoint".to_owned(), decimal_value(book.midpoint)),
                    (
                        "microprice_deviation_bps".to_owned(),
                        decimal_value(return_bps(book.microprice, book.midpoint)?),
                    ),
                    (
                        "book_age_ms".to_owned(),
                        json!(book_age_ms(decision.now_ns, book.logical_receipt_ns)?),
                    ),
                    ("observation_time".to_owned(), json!(book.event_time)),
                ]))
            })
            .transpose()?;
        let event_time = decision
            .book
            .as_ref()
            .map_or_else(|| produced_at.to_owned(), |book| book.event_time.clone());
        self.emit_feature(
            VenueFeatureKind::SpotMicroprice,
            publisher_epoch,
            produced_at,
            event_time,
            values,
            self.book_reasons(decision),
            decision.venue_unavailable,
        )
    }

    fn emit_depth_imbalance(
        &mut self,
        publisher_epoch: &str,
        produced_at: &str,
        decision: &VenueDecision,
    ) -> Result<SignalUpdate, SpotSignalError> {
        if decision.band_truncated {
            // The 5 bps band reached the outermost captured level: not fully
            // observed, so publish the reason rather than a lower-bound depth (§6).
            let reason = venue_band_exceeds_reason(self.venue)
                .expect("band truncation is only ever set on a banded venue");
            return self.emit_feature(
                VenueFeatureKind::ActionableDepthImbalance,
                publisher_epoch,
                produced_at,
                produced_at.to_owned(),
                None,
                vec![reason.to_owned()],
                decision.venue_unavailable,
            );
        }
        let values = decision
            .book
            .as_ref()
            .map(|book| -> Result<BTreeMap<String, Value>, SpotSignalError> {
                Ok(BTreeMap::from([
                    ("band_bps".to_owned(), json!(DEPTH_BAND_BPS)),
                    ("imbalance".to_owned(), decimal_value(book.imbalance)),
                    ("bid_depth_base".to_owned(), decimal_value(book.bid_depth)),
                    ("ask_depth_base".to_owned(), decimal_value(book.ask_depth)),
                    (
                        "book_age_ms".to_owned(),
                        json!(book_age_ms(decision.now_ns, book.logical_receipt_ns)?),
                    ),
                    ("observation_time".to_owned(), json!(book.event_time)),
                ]))
            })
            .transpose()?;
        let event_time = decision
            .book
            .as_ref()
            .map_or_else(|| produced_at.to_owned(), |book| book.event_time.clone());
        self.emit_feature(
            VenueFeatureKind::ActionableDepthImbalance,
            publisher_epoch,
            produced_at,
            event_time,
            values,
            self.book_reasons(decision),
            decision.venue_unavailable,
        )
    }

    fn emit_liquidity_shock(
        &mut self,
        publisher_epoch: &str,
        produced_at: &str,
        decision: &VenueDecision,
    ) -> Result<SignalUpdate, SpotSignalError> {
        if decision.band_truncated {
            // Same band-truncation guard as depth imbalance (§6): the current
            // depth reading is a lower bound, so the history restarts and the
            // identity is not ready.
            self.liquidity_samples.clear();
            self.previous_liquidity_state = None;
            let reason = venue_band_exceeds_reason(self.venue)
                .expect("band truncation is only ever set on a banded venue");
            return self.emit_feature(
                VenueFeatureKind::LiquidityShock,
                publisher_epoch,
                produced_at,
                produced_at.to_owned(),
                None,
                vec![reason.to_owned(), "book_history_warming_up".to_owned()],
                decision.venue_unavailable,
            );
        }
        let Some(book) = decision.book.as_ref() else {
            self.liquidity_samples.clear();
            self.previous_liquidity_state = None;
            let mut reasons = self.book_reasons(decision);
            reasons.push("book_history_warming_up".to_owned());
            return self.emit_feature(
                VenueFeatureKind::LiquidityShock,
                publisher_epoch,
                produced_at,
                produced_at.to_owned(),
                None,
                reasons,
                decision.venue_unavailable,
            );
        };
        let now_ns = decision.now_ns;
        if self.liquidity_samples.back().is_some_and(|sample| {
            now_ns <= sample.logical_time_ns
                || now_ns.saturating_sub(sample.logical_time_ns) >= MISSED_OBSERVATION_NS
        }) {
            self.liquidity_samples.clear();
            self.previous_liquidity_state = None;
        }
        let current = LiquiditySample {
            logical_time_ns: now_ns,
            depth: checked_add(book.bid_depth, book.ask_depth)?,
            spread_bps: book.spread_bps,
        };
        let metrics = if self.liquidity_samples.len() == LIQUIDITY_BASELINE_SAMPLES {
            let baseline = mean_liquidity(&self.liquidity_samples)?;
            if baseline.depth <= Decimal::ZERO || baseline.spread_bps <= Decimal::ZERO {
                None
            } else {
                let depth_ratio = checked_div(current.depth, baseline.depth)?;
                let spread_ratio = checked_div(current.spread_bps, baseline.spread_bps)?;
                let state =
                    classify_liquidity(depth_ratio, spread_ratio, self.previous_liquidity_state);
                Some((baseline, depth_ratio, spread_ratio, state))
            }
        } else {
            None
        };
        if self.liquidity_samples.len() == MAX_LIQUIDITY_SAMPLES {
            self.liquidity_samples.pop_front();
        }
        self.liquidity_samples.push_back(current);
        let values = metrics.map(|(baseline, depth_ratio, spread_ratio, state)| {
            self.previous_liquidity_state = Some(state);
            liquidity_shock_values(
                baseline,
                current,
                depth_ratio,
                spread_ratio,
                state,
                produced_at,
            )
        });
        self.emit_feature(
            VenueFeatureKind::LiquidityShock,
            publisher_epoch,
            produced_at,
            produced_at.to_owned(),
            values,
            vec!["book_history_warming_up".to_owned()],
            decision.venue_unavailable,
        )
    }

    fn emit_ofi(
        &mut self,
        publisher_epoch: &str,
        produced_at: &str,
        decision: &VenueDecision,
    ) -> Result<SignalUpdate, SpotSignalError> {
        let mut reasons = Vec::new();
        if decision.book_invalid {
            reasons.push(venue_book_invalid_reason(self.venue).to_owned());
        }
        if !decision.ofi_warmed {
            reasons.push("book_flow_history_warming_up".to_owned());
        }
        // Same SIG-046 rule the blended reading applies, narrowed to this venue:
        // an eventless window on a live connection is a truthful zero, and only
        // silence with no admitted input in the bound degrades.
        if reasons.is_empty() && !decision.alive {
            reasons.push("book_snapshot_stale_or_missing".to_owned());
        }
        let values = if reasons.is_empty() {
            Some(ofi_values(
                decision.ofi_one_second,
                decision.ofi_ten_seconds,
                decision.ofi_sixty_seconds,
                produced_at,
            )?)
        } else {
            None
        };
        self.emit_feature(
            VenueFeatureKind::OrderFlowImbalance,
            publisher_epoch,
            produced_at,
            produced_at.to_owned(),
            values,
            reasons,
            decision.venue_unavailable,
        )
    }

    fn emit_aggressor_flow(
        &mut self,
        publisher_epoch: &str,
        produced_at: &str,
        decision: &VenueDecision,
    ) -> Result<SignalUpdate, SpotSignalError> {
        let mut reasons = Vec::new();
        if !decision.flow_warmed {
            reasons.push("trade_history_warming_up".to_owned());
        }
        if !decision.alive {
            reasons.push("trade_venue_stale_or_missing".to_owned());
        }
        let values = if reasons.is_empty() {
            Some(aggressor_flow_values(&decision.flow, produced_at)?)
        } else {
            None
        };
        self.emit_feature(
            VenueFeatureKind::AggressorFlow,
            publisher_epoch,
            produced_at,
            produced_at.to_owned(),
            values,
            reasons,
            decision.venue_unavailable,
        )
    }

    fn emit_vwap_deviation(
        &mut self,
        publisher_epoch: &str,
        produced_at: &str,
        decision: &VenueDecision,
    ) -> Result<SignalUpdate, SpotSignalError> {
        let mut reasons = Vec::new();
        if !decision.flow_warmed {
            reasons.push("trade_history_warming_up".to_owned());
        }
        if !decision.flow_fresh {
            reasons.push("trade_venue_stale_or_missing".to_owned());
        }
        let midpoint = decision.quote.as_ref().map(|quote| quote.midpoint);
        if midpoint.is_none() {
            reasons.push("reference_unavailable".to_owned());
        }
        if decision.flow.ten_seconds.base_volume <= Decimal::ZERO
            || decision.flow.sixty_seconds.base_volume <= Decimal::ZERO
        {
            reasons.push("trade_window_empty".to_owned());
        }
        let values = if reasons.is_empty() {
            let midpoint = midpoint.ok_or_else(|| {
                SpotSignalError::State("ready venue VWAP lost its quote midpoint".to_owned())
            })?;
            let vwap_10s = checked_div(
                decision.flow.ten_seconds.gross_notional,
                decision.flow.ten_seconds.base_volume,
            )?;
            let vwap_60s = checked_div(
                decision.flow.sixty_seconds.gross_notional,
                decision.flow.sixty_seconds.base_volume,
            )?;
            Some(BTreeMap::from([
                ("vwap_10s".to_owned(), decimal_value(vwap_10s)),
                ("vwap_60s".to_owned(), decimal_value(vwap_60s)),
                ("midpoint".to_owned(), decimal_value(midpoint)),
                (
                    "deviation_bps_10s".to_owned(),
                    decimal_value(return_bps(midpoint, vwap_10s)?),
                ),
                (
                    "deviation_bps_60s".to_owned(),
                    decimal_value(return_bps(midpoint, vwap_60s)?),
                ),
                (
                    "base_volume_60s".to_owned(),
                    decimal_value(decision.flow.sixty_seconds.base_volume),
                ),
                (
                    "trade_count_60s".to_owned(),
                    json!(
                        u32::try_from(decision.flow.sixty_seconds.count)
                            .map_err(|_| SpotSignalError::Counter)?
                    ),
                ),
                ("window_close".to_owned(), json!(produced_at)),
            ]))
        } else {
            None
        };
        self.emit_feature(
            VenueFeatureKind::VwapDeviation,
            publisher_epoch,
            produced_at,
            produced_at.to_owned(),
            values,
            reasons,
            decision.venue_unavailable,
        )
    }

    fn emit_realized_volatility(
        &mut self,
        publisher_epoch: &str,
        produced_at: &str,
        decision: &VenueDecision,
    ) -> Result<SignalUpdate, SpotSignalError> {
        let metrics = self.volatility.metrics()?;
        let values = metrics.map(|metrics| realized_volatility_values(&metrics, produced_at));
        self.emit_feature(
            VenueFeatureKind::RealizedVolatility,
            publisher_epoch,
            produced_at,
            produced_at.to_owned(),
            values,
            vec!["reference_history_warming_up".to_owned()],
            decision.venue_unavailable,
        )
    }

    fn emit_micro_momentum(
        &mut self,
        publisher_epoch: &str,
        produced_at: &str,
        decision: &VenueDecision,
        momentum: Option<MomentumMetrics>,
    ) -> Result<SignalUpdate, SpotSignalError> {
        let sample_count = u16::try_from(self.volatility.len()).unwrap_or(u16::MAX);
        let values =
            momentum.map(|metrics| micro_momentum_values(metrics, sample_count, produced_at));
        self.emit_feature(
            VenueFeatureKind::MicroMomentum,
            publisher_epoch,
            produced_at,
            produced_at.to_owned(),
            values,
            vec!["reference_history_warming_up".to_owned()],
            decision.venue_unavailable,
        )
    }

    fn emit_horizon_momentum(
        &mut self,
        publisher_epoch: &str,
        produced_at: &str,
        decision: &VenueDecision,
    ) -> Result<SignalUpdate, SpotSignalError> {
        let values = self
            .horizon
            .momentum()?
            .map(|metrics| horizon_momentum_values(metrics, produced_at));
        self.emit_feature(
            VenueFeatureKind::HorizonMomentum,
            publisher_epoch,
            produced_at,
            produced_at.to_owned(),
            values,
            vec!["reference_history_warming_up".to_owned()],
            decision.venue_unavailable,
        )
    }

    fn emit_horizon_volatility(
        &mut self,
        publisher_epoch: &str,
        produced_at: &str,
        decision: &VenueDecision,
    ) -> Result<SignalUpdate, SpotSignalError> {
        let values = self
            .horizon
            .volatility()?
            .map(|metrics| horizon_volatility_values(metrics, produced_at));
        self.emit_feature(
            VenueFeatureKind::HorizonVolatility,
            publisher_epoch,
            produced_at,
            produced_at.to_owned(),
            values,
            vec!["reference_history_warming_up".to_owned()],
            decision.venue_unavailable,
        )
    }

    fn book_reasons(&self, decision: &VenueDecision) -> Vec<String> {
        vec![if decision.book_invalid {
            venue_book_invalid_reason(self.venue).to_owned()
        } else {
            "book_snapshot_stale_or_missing".to_owned()
        }]
    }

    fn momentum_metrics(&self) -> Option<MomentumMetrics> {
        momentum_from_ring(&self.volatility)
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_feature(
        &mut self,
        kind: VenueFeatureKind,
        publisher_epoch: &str,
        produced_at: &str,
        event_time: String,
        mut ready_values: Option<BTreeMap<String, Value>>,
        mut reasons: Vec<String>,
        venue_unavailable: bool,
    ) -> Result<SignalUpdate, SpotSignalError> {
        // A per-venue identity is gated by its own venue only: the blended
        // emitter's both-venue gate would defeat the purpose of the split.
        if venue_unavailable {
            ready_values = None;
            reasons.push(venue_unavailable_reason(self.venue).to_owned());
        }
        // R6/E42: a rights-gated venue's identities declare `quote_currency`, so
        // every ready value map carries it (constant `usdt` for these venues). The
        // USD venues return `None` here and are untouched (byte-identical).
        if let (Some(currency), Some(values)) =
            (venue_quote_currency(self.venue), ready_values.as_mut())
        {
            values.insert("quote_currency".to_owned(), json!(currency));
        }
        // E47: convert this identity's quote-denominated fields USDT→USD with the
        // decision-time reference, or force `usdt_reference_stale` when the rate
        // is absent/stale. A no-op for non-converting identities and USD venues.
        apply_usdt_conversion(
            self.current_reference,
            venue_quote_fields(self.venue, kind),
            &mut ready_values,
            &mut reasons,
        )?;
        let venue = self.venue;
        self.emitters
            .get_mut(&kind)
            .ok_or_else(|| SpotSignalError::State(format!("missing emitter {}", kind.id(venue))))?
            .emit(
                publisher_epoch,
                produced_at,
                event_time,
                ready_values,
                reasons,
            )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpotSignalFeatureCheckpoint {
    pub previous_next_sequence: u64,
    pub lifetime_output_count: u64,
    pub ever_ready: bool,
    pub last_quality: Option<Quality>,
    pub quality_reason_entries: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpotSignalCheckpointState {
    pub reference: SpotReferenceCheckpointState,
    pub features: BTreeMap<String, SpotSignalFeatureCheckpoint>,
    pub last_now_ns: Option<u64>,
}

/// One deterministic processor for the complete currently buildable BTC spot feature set.
///
/// Market events mutate bounded state. Normal derived measurements publish on
/// the one-second health clock, so event rate cannot multiply JSON output
/// volume. A newly invalid completed book emits one immediate, transition-
/// counted quality update for each affected feature before recovery begins.
/// What one identity last published, for the publication coalescing rule.
#[derive(Debug, Clone)]
pub(crate) struct PublishedMemory {
    quality: Quality,
    values: BTreeMap<String, Value>,
    emitted_ns: u64,
}

/// Whether a value field is derived from the clock rather than from the
/// inputs: an age, a staleness, a time-to or time-since. These tick on every
/// evaluation and would make every restatement look like a change, so the
/// coalescing rule compares values without them; the declared-interval
/// restatement keeps them fresh.
pub(crate) fn is_clock_derived_field(name: &str) -> bool {
    name.ends_with("_age_ms")
        || name.ends_with("staleness_ms")
        || name.starts_with("time_to_")
        || name.starts_with("time_since_")
}

fn values_changed(previous: &BTreeMap<String, Value>, next: &BTreeMap<String, Value>) -> bool {
    let compared = |values: &BTreeMap<String, Value>| {
        values
            .iter()
            .filter(|(name, _)| !is_clock_derived_field(name))
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect::<BTreeMap<_, _>>()
    };
    compared(previous) != compared(next)
}

/// The publication coalescing rule shared by the event-driven lanes.
///
/// Evaluation is cheap and happens on every admitted input batch, so an
/// identity is evaluated far more often than its reading changes. An update
/// that restates the previous quality and values (clock-derived fields
/// aside, see [`is_clock_derived_field`]) is dropped until the identity's
/// declared `approximate_interval_ms` has elapsed since the last
/// publication, which keeps the declared interval as the restatement
/// heartbeat while a changed reading publishes the moment it is computed.
/// Identities without a known interval pass through untouched.
pub(crate) fn coalesce_publications(
    memory: &mut BTreeMap<String, PublishedMemory>,
    intervals_ns: &BTreeMap<String, u64>,
    now_ns: u64,
    updates: Vec<SignalUpdate>,
) -> Vec<SignalUpdate> {
    let mut out = Vec::with_capacity(updates.len());
    for update in updates {
        let Some(&interval_ns) = intervals_ns.get(&update.signal_id) else {
            out.push(update);
            continue;
        };
        let unchanged = memory.get(&update.signal_id).is_some_and(|previous| {
            previous.quality == update.quality
                && !values_changed(&previous.values, &update.values)
                && now_ns < previous.emitted_ns.saturating_add(interval_ns)
        });
        if unchanged {
            continue;
        }
        memory.insert(
            update.signal_id.clone(),
            PublishedMemory {
                quality: update.quality.clone(),
                values: update.values.clone(),
                emitted_ns: now_ns,
            },
        );
        out.push(update);
    }
    out
}

pub struct SpotSignalProcessor {
    publisher_epoch: String,
    wall_start: DateTime<Utc>,
    reference: SpotReferenceProcessor,
    emitters: BTreeMap<FeatureKind, FeatureEmitter>,
    source_definitions: BTreeMap<String, SourceDefinition>,
    coinbase_book: BookState,
    kraken_book: BookState,
    flow: FlowSeries,
    ofi: OfiSeries,
    // Per-venue identity lanes (ADR 0024) and their own rolling trade and
    // book-flow history. The blended `flow`/`ofi` above keep their every-venue
    // clear semantics; these are cleared for one venue at a time, so a venue
    // that never dropped keeps publishing while its peer re-warms.
    venue_lanes: BTreeMap<Venue, VenueLane>,
    venue_flow: FlowSeries,
    venue_ofi: OfiSeries,
    trade_ordering: BTreeMap<String, (String, u64)>,
    volatility: RealizedVolatility,
    /// The C3a technical-analysis unit over the blended reference ring (E51),
    /// driven from the same one-second reference observation the volatility ring
    /// reads so the two share one continuity rule.
    ta: TaEngine,
    /// The blended C2 longer-horizon ring, fed the same ready spot-reference
    /// price as `volatility` above (see `emit_features`).
    horizon: HorizonRing,
    liquidity_samples: VecDeque<LiquiditySample>,
    previous_liquidity_state: Option<LiquidityState>,
    pending_book_recoveries: BTreeSet<Venue>,
    last_now_ns: Option<u64>,
    // Quiet-window aliveness (SIG-046): the most recent admitted-input logical
    // receipt per venue across every stream this processor consumes. Written
    // only on the journaled admission path from logical_receipt_ns, never a
    // wall clock, so replay reconstructs it byte-identically; deliberately not
    // checkpointed, since a restore clears the rolling flow/OFI state and the
    // signals re-warm before they could publish a quiet-window zero.
    venue_last_input_ns: BTreeMap<Venue, u64>,
    /// The most recent spot-reference update, so an admitted book or trade
    /// event can republish every identity against the current reference
    /// without waiting for the next health tick.
    last_reference: Option<SignalUpdate>,
    /// Publication coalescing memory and each identity's declared interval
    /// (see [`coalesce_publications`]).
    published: BTreeMap<String, PublishedMemory>,
    intervals_ns: BTreeMap<String, u64>,
}

impl SpotSignalProcessor {
    pub fn new(
        publisher_epoch: impl Into<String>,
        wall_start: DateTime<Utc>,
    ) -> Result<Self, SpotSignalError> {
        Self::build(publisher_epoch, wall_start, VenueIdentities::Published)
    }

    /// Determinism-harness constructor that suppresses the per-venue identity
    /// lanes.
    ///
    /// This is not a publishing mode: the runner always uses
    /// [`SpotSignalProcessor::new`], which publishes the complete surface. It
    /// exists so one fixture can be replayed with and without the per-venue
    /// lanes and the `btc.external.*` frames compared byte for byte, proving
    /// the lanes do not perturb the blended pipeline (ADR 0024).
    #[doc(hidden)]
    pub fn new_with_venue_identities_suppressed(
        publisher_epoch: impl Into<String>,
        wall_start: DateTime<Utc>,
    ) -> Result<Self, SpotSignalError> {
        Self::build(publisher_epoch, wall_start, VenueIdentities::Suppressed)
    }

    fn build(
        publisher_epoch: impl Into<String>,
        wall_start: DateTime<Utc>,
        venue_identities: VenueIdentities,
    ) -> Result<Self, SpotSignalError> {
        let publisher_epoch = publisher_epoch.into();
        let reference = SpotReferenceProcessor::new(&publisher_epoch, wall_start)?;
        Self::from_reference(publisher_epoch, wall_start, reference, venue_identities)
    }

    pub fn restore(
        publisher_epoch: impl Into<String>,
        wall_start: DateTime<Utc>,
        state: SpotSignalCheckpointState,
    ) -> Result<Self, SpotSignalError> {
        state.validate()?;
        let publisher_epoch = publisher_epoch.into();
        let reference =
            SpotReferenceProcessor::restore(&publisher_epoch, wall_start, state.reference)?;
        let mut processor = Self::from_reference(
            publisher_epoch,
            wall_start,
            reference,
            VenueIdentities::Published,
        )?;
        for (kind, emitter) in &mut processor.emitters {
            let checkpoint = state.features.get(kind.id()).ok_or_else(|| {
                SpotSignalError::Checkpoint(format!("checkpoint is missing feature {}", kind.id()))
            })?;
            emitter.restore(checkpoint);
        }
        for (venue, lane) in &mut processor.venue_lanes {
            for (kind, emitter) in &mut lane.emitters {
                let id = kind.id(*venue);
                let checkpoint = state.features.get(id).ok_or_else(|| {
                    SpotSignalError::Checkpoint(format!("checkpoint is missing feature {id}"))
                })?;
                emitter.restore(checkpoint);
            }
        }
        processor.last_now_ns = state.last_now_ns;
        let reset_ns = state.last_now_ns.unwrap_or(0);
        processor.flow.clear(reset_ns);
        processor.ofi.clear(reset_ns);
        processor.venue_flow.clear(reset_ns);
        processor.venue_ofi.clear(reset_ns);
        Ok(processor)
    }

    fn from_reference(
        publisher_epoch: String,
        wall_start: DateTime<Utc>,
        reference: SpotReferenceProcessor,
        venue_identities: VenueIdentities,
    ) -> Result<Self, SpotSignalError> {
        let mut emitters = BTreeMap::new();
        for kind in FeatureKind::ALL {
            emitters.insert(kind, FeatureEmitter::new(kind)?);
        }
        let mut venue_lanes = BTreeMap::new();
        if venue_identities == VenueIdentities::Published {
            for venue in VENUE_SPOT_VENUES {
                venue_lanes.insert(venue, VenueLane::new(venue, wall_start)?);
            }
        }
        let definitions = [
            (
                COINBASE_BOOK_SOURCE_ID,
                COINBASE_BOOK_SOURCE_VERSION,
                COINBASE_BOOK_DEFINITION,
            ),
            (
                KRAKEN_BOOK_SOURCE_ID,
                KRAKEN_BOOK_SOURCE_VERSION,
                KRAKEN_BOOK_DEFINITION,
            ),
            (
                COINBASE_TRADE_SOURCE_ID,
                COINBASE_TRADE_SOURCE_VERSION,
                COINBASE_TRADE_DEFINITION,
            ),
            (
                KRAKEN_TRADE_SOURCE_ID,
                KRAKEN_TRADE_SOURCE_VERSION,
                KRAKEN_TRADE_DEFINITION,
            ),
        ];
        let mut source_definitions = BTreeMap::new();
        for (id, version, raw) in definitions {
            let definition = parse_definition::<SourceDefinition>(raw, id)?;
            if definition.source_id != id || definition.version != version {
                return Err(SpotSignalError::Definition(format!(
                    "embedded source identity {}@{} does not match {id}@{version}",
                    definition.source_id, definition.version
                )));
            }
            source_definitions.insert(id.to_owned(), definition);
        }
        let mut intervals_ns = BTreeMap::new();
        for definition in emitters
            .values()
            .map(|emitter| &emitter.definition)
            .chain(venue_lanes.values().flat_map(VenueLane::definitions))
        {
            intervals_ns.insert(
                definition.signal_id.clone(),
                definition.approximate_interval_ms.saturating_mul(1_000_000),
            );
        }
        Ok(Self {
            publisher_epoch,
            wall_start,
            reference,
            emitters,
            source_definitions,
            coinbase_book: BookState::default(),
            kraken_book: BookState::default(),
            flow: FlowSeries::default(),
            ofi: OfiSeries::default(),
            trade_ordering: BTreeMap::new(),
            volatility: RealizedVolatility::new(),
            ta: TaEngine::new()?,
            horizon: HorizonRing::new(),
            liquidity_samples: VecDeque::with_capacity(MAX_LIQUIDITY_SAMPLES),
            previous_liquidity_state: None,
            pending_book_recoveries: BTreeSet::new(),
            last_now_ns: None,
            venue_last_input_ns: BTreeMap::new(),
            last_reference: None,
            published: BTreeMap::new(),
            intervals_ns,
            venue_lanes,
            venue_flow: FlowSeries::default(),
            venue_ofi: OfiSeries::default(),
        })
    }

    pub fn accepts(source_id: &str) -> bool {
        matches!(
            source_id,
            COINBASE_TICKER_SOURCE_ID
                | KRAKEN_TICKER_SOURCE_ID
                | COINBASE_BOOK_SOURCE_ID
                | KRAKEN_BOOK_SOURCE_ID
                | COINBASE_TRADE_SOURCE_ID
                | KRAKEN_TRADE_SOURCE_ID
        )
    }

    pub fn definitions(&self) -> impl Iterator<Item = &SignalDefinition> {
        std::iter::once(self.reference.definition())
            .chain(self.emitters.values().map(|emitter| &emitter.definition))
            .chain(self.venue_lanes.values().flat_map(VenueLane::definitions))
    }

    /// Every consumed source definition — the two reference tickers plus the
    /// book and trade feeds — each carrying its declared liveness contract. The
    /// feed-health board derives per-feed liveness bounds from these without
    /// duplicating any `expected_heartbeat_ms` value.
    pub fn source_definitions(&self) -> impl Iterator<Item = &SourceDefinition> {
        self.reference
            .source_definitions()
            .into_iter()
            .chain(self.source_definitions.values())
    }

    pub fn publisher_epoch(&self) -> &str {
        &self.publisher_epoch
    }

    /// Rotate to a new output publisher epoch in place for a new file
    /// generation, keeping every warm ring and window intact (C5 in-process
    /// rotation).
    ///
    /// This is the processor side of the runtime's per-generation reset. It
    /// re-seats the output identity exactly as a restart would — a fresh,
    /// non-colliding epoch, every output sequence restarted at zero, the epoch
    /// recorded in the reference lineage — while the rolling rings (TA,
    /// horizon, realized volatility, blended and per-venue flow/OFI, tick
    /// velocity, books) stay warm, so the first decision after the boundary
    /// carries every identity that was Ready before it. Non-ring feature state
    /// is checkpointed and therefore reproduced byte-for-byte by an independent
    /// replay of the new generation's journal; the warm rings are not
    /// checkpointed and re-warm honestly on that replay.
    pub fn rotate_output_epoch(
        &mut self,
        publisher_epoch: impl Into<String>,
    ) -> Result<(), SpotSignalError> {
        let publisher_epoch = publisher_epoch.into();
        self.reference
            .rotate_output_epoch(publisher_epoch.as_str())?;
        for emitter in self.emitters.values_mut() {
            emitter.reset_output_sequence();
        }
        for lane in self.venue_lanes.values_mut() {
            lane.reset_output_sequences();
        }
        // Publication memory is not checkpointed, so a rotation-opened
        // generation starts cold in the warm live process exactly as its
        // standalone replay does: the first evaluation of the new epoch
        // publishes every identity, and event-driven republishing waits for
        // the epoch's first reference decision.
        self.published.clear();
        self.last_reference = None;
        self.publisher_epoch = publisher_epoch;
        Ok(())
    }

    /// Book resync inputs for a C5 rotation, one single-venue batch per book
    /// source (Coinbase then Kraken). A rotation-opened generation admits these
    /// as its first records so a cold replay rebuilds the books the checkpoint
    /// does not carry; on the warm live process they are ordering-skipped and
    /// change nothing. Empty batches (an unready book) are omitted.
    pub fn book_resync_inputs(&self) -> Vec<Vec<SourceEvent>> {
        let coinbase = self.coinbase_book.resync_rows(
            COINBASE_BOOK_SOURCE_ID,
            COINBASE_BOOK_SOURCE_VERSION,
            &[
                ("instrument", serde_json::Value::from("BTC-USD")),
                ("event_time_basis", serde_json::Value::from("receipt")),
            ],
        );
        let kraken = self.kraken_book.resync_rows(
            KRAKEN_BOOK_SOURCE_ID,
            KRAKEN_BOOK_SOURCE_VERSION,
            &[
                ("instrument", serde_json::Value::from("BTC/USD")),
                (
                    "configured_depth",
                    serde_json::Value::from(SPOT_SIGNAL_KRAKEN_BOOK_DEPTH),
                ),
                ("change_origin", serde_json::Value::from("provider")),
                ("book_checksum", serde_json::Value::from(0_u32)),
            ],
        );
        [coinbase, kraken]
            .into_iter()
            .filter(|rows| !rows.is_empty())
            .collect()
    }

    pub fn checkpoint_state(&self) -> SpotSignalCheckpointState {
        let mut features: BTreeMap<String, SpotSignalFeatureCheckpoint> = self
            .emitters
            .iter()
            .map(|(kind, emitter)| (kind.id().to_owned(), emitter.checkpoint()))
            .collect();
        for (venue, lane) in &self.venue_lanes {
            for (kind, emitter) in &lane.emitters {
                features.insert(kind.id(*venue).to_owned(), emitter.checkpoint());
            }
        }
        SpotSignalCheckpointState {
            reference: self.reference.checkpoint_state(),
            features,
            last_now_ns: self.last_now_ns,
        }
    }

    pub fn ingest(
        &mut self,
        event: &SourceEvent,
        logical_receipt_ns: u64,
        now_ns: u64,
    ) -> Result<Vec<SignalUpdate>, SpotSignalError> {
        self.observe_time(now_ns)?;
        if logical_receipt_ns > now_ns {
            return Err(SpotSignalError::ReceiptAfterAdmission {
                receipt_ns: logical_receipt_ns,
                now_ns,
            });
        }
        // Quiet-window aliveness: every admitted event from a venue — ticker,
        // book, or trade — proves that venue's connection is alive, monotone in
        // logical receipt time.
        if let Some(venue) = venue_of_source(event.source_id.as_str()) {
            let entry = self.venue_last_input_ns.entry(venue).or_insert(0);
            *entry = (*entry).max(logical_receipt_ns);
        }
        match event.source_id.as_str() {
            COINBASE_TICKER_SOURCE_ID | KRAKEN_TICKER_SOURCE_ID => {
                let updates = self.reference.ingest(event, logical_receipt_ns, now_ns)?;
                if let Some(reference) = &updates {
                    self.last_reference = Some(reference.clone());
                }
                // C3 row 8: one admitted quote update for this venue's
                // `tick_velocity`, read back from the reference so the counted
                // midpoint is the same one every other identity sees.
                if let Some(venue) = venue_of_source(event.source_id.as_str()) {
                    self.observe_lane_quote(venue, logical_receipt_ns, now_ns)?;
                }
                Ok(updates.into_iter().collect())
            }
            COINBASE_BOOK_SOURCE_ID => {
                self.validate_source_event(event)?;
                let outcome = self.coinbase_book.ingest(event, logical_receipt_ns)?;
                self.accept_book_outcome(Venue::Coinbase, outcome, now_ns)
            }
            KRAKEN_BOOK_SOURCE_ID => {
                self.validate_source_event(event)?;
                if event.values.get("configured_depth").and_then(Value::as_u64)
                    != Some(u64::from(SPOT_SIGNAL_KRAKEN_BOOK_DEPTH))
                {
                    return Err(SpotSignalError::MarketValue(format!(
                        "Kraken book depth must be {} for this signal contract",
                        SPOT_SIGNAL_KRAKEN_BOOK_DEPTH
                    )));
                }
                let outcome = self.kraken_book.ingest(event, logical_receipt_ns)?;
                self.accept_book_outcome(Venue::Kraken, outcome, now_ns)
            }
            COINBASE_TRADE_SOURCE_ID => {
                self.validate_source_event(event)?;
                if self.accept_trade_ordering(event)? {
                    self.flow
                        .ingest(Venue::Coinbase, event, logical_receipt_ns)?;
                    self.venue_flow
                        .ingest(Venue::Coinbase, event, logical_receipt_ns)?;
                    self.observe_lane_trade(Venue::Coinbase, event, logical_receipt_ns)?;
                }
                Ok(Vec::new())
            }
            KRAKEN_TRADE_SOURCE_ID => {
                self.validate_source_event(event)?;
                if self.accept_trade_ordering(event)? {
                    self.flow.ingest(Venue::Kraken, event, logical_receipt_ns)?;
                    self.venue_flow
                        .ingest(Venue::Kraken, event, logical_receipt_ns)?;
                    self.observe_lane_trade(Venue::Kraken, event, logical_receipt_ns)?;
                }
                Ok(Vec::new())
            }
            other => Err(SpotSignalError::UnsupportedSource(other.to_owned())),
        }
    }

    /// Admit a C5 rotation book-resync row. Unlike [`SpotSignalProcessor::ingest`]
    /// this applies to the [`BookState`] ONLY: it rebuilds the book (and the
    /// book-derived features that fold over it) so a cold replay of a
    /// rotation-opened generation can reconstruct the books the checkpoint does
    /// not carry. It deliberately does NOT prove venue liveness — it never
    /// touches `venue_last_input_ns`/`venue_alive` — because a re-snapshot is
    /// synthesized from the warm in-memory book, not received from the venue.
    /// A silent feed must therefore still fail closed across a rotation. The
    /// caller (`admit_book_resync`) likewise skips feed-health and provider-clock
    /// observation for these rows; only genuine venue frames feed those.
    pub fn ingest_resync(
        &mut self,
        event: &SourceEvent,
        logical_receipt_ns: u64,
        now_ns: u64,
    ) -> Result<Vec<SignalUpdate>, SpotSignalError> {
        self.observe_time(now_ns)?;
        if logical_receipt_ns > now_ns {
            return Err(SpotSignalError::ReceiptAfterAdmission {
                receipt_ns: logical_receipt_ns,
                now_ns,
            });
        }
        match event.source_id.as_str() {
            COINBASE_BOOK_SOURCE_ID => {
                self.validate_source_event(event)?;
                let outcome = self.coinbase_book.ingest(event, logical_receipt_ns)?;
                self.accept_book_outcome(Venue::Coinbase, outcome, now_ns)
            }
            KRAKEN_BOOK_SOURCE_ID => {
                self.validate_source_event(event)?;
                if event.values.get("configured_depth").and_then(Value::as_u64)
                    != Some(u64::from(SPOT_SIGNAL_KRAKEN_BOOK_DEPTH))
                {
                    return Err(SpotSignalError::MarketValue(format!(
                        "Kraken book depth must be {} for this signal contract",
                        SPOT_SIGNAL_KRAKEN_BOOK_DEPTH
                    )));
                }
                let outcome = self.kraken_book.ingest(event, logical_receipt_ns)?;
                self.accept_book_outcome(Venue::Kraken, outcome, now_ns)
            }
            // A resync batch is only ever the two book sources; any other source
            // is a synthesis fault, not a venue frame.
            other => Err(SpotSignalError::UnsupportedSource(other.to_owned())),
        }
    }

    pub fn advance_time(&mut self, now_ns: u64) -> Result<Vec<SignalUpdate>, SpotSignalError> {
        self.observe_time(now_ns)?;
        let Some(reference) = self.reference.advance_time(now_ns)? else {
            return Ok(Vec::new());
        };
        self.last_reference = Some(reference.clone());
        let mut updates = vec![reference.clone()];
        updates.extend(self.emit_features(now_ns, &reference)?);
        Ok(self.coalesce(now_ns, updates))
    }

    /// Republish every identity against the current reference after an
    /// admitted input batch (a quote, book or trade event), so a reading that
    /// changed publishes now rather than at the next health tick. The windowed
    /// rings keep their once-per-tick sampling; only publication is
    /// event-driven, and the coalescing rule drops unchanged restatements.
    pub fn emit_after_events(
        &mut self,
        now_ns: u64,
    ) -> Result<Vec<SignalUpdate>, SpotSignalError> {
        self.observe_time(now_ns)?;
        let Some(reference) = self.last_reference.clone() else {
            return Ok(Vec::new());
        };
        let updates = self.emit_outputs(now_ns, &reference, false)?;
        Ok(self.coalesce(now_ns, updates))
    }

    fn coalesce(&mut self, now_ns: u64, updates: Vec<SignalUpdate>) -> Vec<SignalUpdate> {
        coalesce_publications(&mut self.published, &self.intervals_ns, now_ns, updates)
    }

    pub fn mark_unavailable(
        &mut self,
        venue: Venue,
        reason: UnavailableReason,
        now_ns: u64,
    ) -> Result<Vec<SignalUpdate>, SpotSignalError> {
        self.observe_time(now_ns)?;
        match venue {
            Venue::Coinbase => self.coinbase_book.clear(),
            Venue::Kraken => self.kraken_book.clear(),
            Venue::Binance | Venue::Bybit => unreachable!(
                "the spot processor publishes only the USD venues; the rights-gated \
                 venues run on their own venue lane (B1)"
            ),
        }
        // The blended readings are an every-venue AND, so one venue dropping
        // invalidates the whole blended history and it re-warms from scratch.
        self.flow.clear(now_ns);
        self.ofi.clear(now_ns);
        self.volatility.clear();
        self.horizon.clear();
        self.liquidity_samples.clear();
        self.previous_liquidity_state = None;
        // The per-venue lanes carry no cross-venue dependency, so only the
        // venue that dropped loses its history; its peer keeps publishing.
        self.venue_flow.clear_venue(venue);
        self.venue_ofi.clear_venue(venue);
        if let Some(lane) = self.venue_lanes.get_mut(&venue) {
            lane.clear_history()?;
        }
        let mut updates = Vec::new();
        if let Some(reference) = self.reference.mark_unavailable(venue, reason, now_ns)? {
            self.last_reference = Some(reference.clone());
            updates.push(reference.clone());
            updates.extend(self.emit_features(now_ns, &reference)?);
        }
        Ok(self.coalesce(now_ns, updates))
    }

    pub fn quality_reason_entries(&self) -> BTreeMap<String, u64> {
        let mut entries = BTreeMap::new();
        for (reason, count) in self.reference.quality_reason_entries() {
            entries.insert(format!("{}:{reason}", crate::SPOT_REFERENCE_ID), *count);
        }
        for (kind, emitter) in &self.emitters {
            for (reason, count) in &emitter.reason_entries {
                entries.insert(format!("{}:{reason}", kind.id()), *count);
            }
        }
        for (venue, lane) in &self.venue_lanes {
            for (kind, emitter) in &lane.emitters {
                for (reason, count) in &emitter.reason_entries {
                    entries.insert(format!("{}:{reason}", kind.id(*venue)), *count);
                }
            }
        }
        entries
    }

    pub fn take_book_recovery_requests(&mut self) -> BTreeSet<Venue> {
        std::mem::take(&mut self.pending_book_recoveries)
    }

    pub fn last_now_ns(&self) -> Option<u64> {
        self.last_now_ns
    }

    fn accept_book_outcome(
        &mut self,
        venue: Venue,
        outcome: BookIngestOutcome,
        now_ns: u64,
    ) -> Result<Vec<SignalUpdate>, SpotSignalError> {
        match outcome {
            BookIngestOutcome::Pending => Ok(Vec::new()),
            BookIngestOutcome::Complete(completion) => {
                self.ofi.ingest(venue, &completion)?;
                self.venue_ofi.ingest(venue, &completion)?;
                // The join takes only completed groups, which is exactly what
                // this arm proves (join caller precondition).
                if let Some(lane) = self.venue_lanes.get_mut(&venue) {
                    lane.observe_book(&completion)?;
                }
                Ok(Vec::new())
            }
            BookIngestOutcome::Invalid { transitioned } => {
                self.pending_book_recoveries.insert(venue);
                if !transitioned {
                    return Ok(Vec::new());
                }
                self.ofi.clear_venue(venue);
                self.venue_ofi.clear_venue(venue);
                self.liquidity_samples.clear();
                self.previous_liquidity_state = None;
                self.emit_book_failure_transition(venue, now_ns)
            }
        }
    }

    fn emit_book_failure_transition(
        &mut self,
        venue: Venue,
        now_ns: u64,
    ) -> Result<Vec<SignalUpdate>, SpotSignalError> {
        let produced_at = self
            .logical_timestamp(now_ns)?
            .to_rfc3339_opts(SecondsFormat::Nanos, true);
        let book_reasons = self.book_failure_reasons();
        let mut ofi_reasons = book_reasons.clone();
        ofi_reasons.push("book_flow_history_warming_up".to_owned());
        let mut liquidity_reasons = book_reasons.clone();
        liquidity_reasons.push("book_history_warming_up".to_owned());
        let mut fade_reasons = book_reasons.clone();
        fade_reasons.push("upstream_measurement_unavailable".to_owned());
        let mut updates = vec![
            self.emit_feature(
                FeatureKind::SpotMicroprice,
                &produced_at,
                produced_at.clone(),
                None,
                book_reasons.clone(),
            )?,
            self.emit_feature(
                FeatureKind::ActionableDepthImbalance,
                &produced_at,
                produced_at.clone(),
                None,
                book_reasons.clone(),
            )?,
            self.emit_feature(
                FeatureKind::OrderFlowImbalance,
                &produced_at,
                produced_at.clone(),
                None,
                ofi_reasons,
            )?,
            self.emit_feature(
                FeatureKind::AbsorptionState,
                &produced_at,
                produced_at.clone(),
                None,
                book_reasons,
            )?,
            self.emit_feature(
                FeatureKind::LiquidityShock,
                &produced_at,
                produced_at.clone(),
                None,
                liquidity_reasons,
            )?,
            // The fade context is a fold over absorption, which just failed
            // closed, so it must not keep publishing while its input is gone.
            self.emit_feature(
                FeatureKind::EarlyMoveFadeContext,
                &produced_at,
                produced_at.clone(),
                None,
                fade_reasons,
            )?,
        ];
        // Only the venue whose book just failed emits an immediate per-venue
        // transition; its peer's identities are untouched by this fault.
        let venue_unavailable = self.reference.venue_is_unavailable(venue);
        let publisher_epoch = self.publisher_epoch.clone();
        if let Some(lane) = self.venue_lanes.get_mut(&venue) {
            updates.extend(lane.emit_book_failure(
                &publisher_epoch,
                &produced_at,
                venue_unavailable,
            )?);
        }
        Ok(updates)
    }

    // A venue is alive when any stream this processor consumes for it admitted
    // an event within the staleness bound. An empty activity window is a
    // truthful zero only against a live venue; a venue with no admitted input
    // in the bound is indistinguishable from blindness and stays degraded.
    fn venues_alive(&self, now_ns: u64) -> bool {
        VENUE_SPOT_VENUES
            .into_iter()
            .all(|venue| self.venue_alive(venue, now_ns))
    }

    /// The per-venue half of the aliveness rule: sole authority for whether one
    /// venue has admitted any input inside the staleness bound.
    fn venue_alive(&self, venue: Venue, now_ns: u64) -> bool {
        self.venue_last_input_ns.get(&venue).is_some_and(|receipt| {
            now_ns
                .checked_sub(*receipt)
                .is_some_and(|age| age <= STALENESS_BOUND_NS)
        })
    }

    fn book_failure_reasons(&self) -> Vec<String> {
        let mut reasons = Vec::with_capacity(2);
        if self.coinbase_book.invalid_reason().is_some() {
            reasons.push("coinbase_book_invalid".to_owned());
        }
        if self.kraken_book.invalid_reason().is_some() {
            reasons.push("kraken_book_invalid".to_owned());
        }
        reasons
    }

    /// The health-tick decision: absorb the reference observation into the
    /// windowed rings, then publish every identity.
    fn emit_features(
        &mut self,
        now_ns: u64,
        reference: &SignalUpdate,
    ) -> Result<Vec<SignalUpdate>, SpotSignalError> {
        self.volatility.update(reference, now_ns)?;
        // The TA unit reads the same reference observation, through the same
        // readiness gate: a non-ready reference is an unusable sample.
        self.ta.update(
            if reference.quality.state == QualityState::Ready {
                optional_decimal(&reference.values, "reference_price")?
            } else {
                None
            },
            now_ns,
        )?;
        self.horizon.update(reference, now_ns)?;
        self.emit_outputs(now_ns, reference, true)
    }

    /// Publish every identity from the current state. `absorb` is whether the
    /// per-venue lanes also take this decision's midpoint sample into their
    /// rings (the health tick) or only read them (an event-driven republish).
    fn emit_outputs(
        &mut self,
        now_ns: u64,
        reference: &SignalUpdate,
        absorb: bool,
    ) -> Result<Vec<SignalUpdate>, SpotSignalError> {
        let produced_at = self.logical_timestamp(now_ns)?;
        let produced_at_text = produced_at.to_rfc3339_opts(SecondsFormat::Nanos, true);
        let book_pair = self.book_pair(now_ns)?;
        let momentum = self.momentum_metrics();
        let flow = self.flow_metrics(now_ns)?;
        let depth = book_pair.as_ref().map(depth_metrics).transpose()?;

        let mut updates = Vec::with_capacity(FeatureKind::ALL.len());
        updates.push(self.emit_cross_venue_dispersion(reference, &produced_at_text)?);
        updates.push(self.emit_micro_momentum(momentum, &produced_at_text)?);
        updates.push(self.emit_realized_volatility(&produced_at_text)?);
        updates.push(self.emit_horizon_momentum(&produced_at_text)?);
        updates.push(self.emit_horizon_volatility(&produced_at_text)?);
        updates.push(self.emit_vwap_deviation(now_ns, reference, flow, &produced_at_text)?);
        updates.push(self.emit_spot_microprice(now_ns, book_pair.as_ref(), &produced_at_text)?);
        updates.push(self.emit_depth_imbalance(
            now_ns,
            book_pair.as_ref(),
            depth,
            &produced_at_text,
        )?);
        updates.push(self.emit_ofi(now_ns, &produced_at_text)?);
        updates.push(self.emit_aggressor_flow(now_ns, flow, &produced_at_text)?);
        let (absorption, absorption_state) =
            self.emit_absorption(momentum, depth, flow, &produced_at_text)?;
        updates.push(absorption);
        updates.push(self.emit_liquidity_shock(now_ns, book_pair.as_ref(), &produced_at_text)?);
        updates.push(self.emit_early_move_fade(momentum, absorption_state, &produced_at_text)?);
        for kind in TaKind::ALL {
            updates.push(self.emit_ta(kind, &produced_at_text)?);
        }
        updates.extend(self.emit_venue_features(now_ns, &produced_at_text, absorb)?);
        Ok(updates)
    }

    /// Publish one C3a technical-analysis identity of the blended scope.
    fn emit_ta(
        &mut self,
        ta_kind: TaKind,
        produced_at: &str,
    ) -> Result<SignalUpdate, SpotSignalError> {
        let (values, reasons) = match self.ta.outcome(ta_kind, produced_at, self.wall_start)? {
            TaOutcome::Ready(values) => (Some(values), Vec::new()),
            TaOutcome::Warming => (None, vec!["reference_history_warming_up".to_owned()]),
            TaOutcome::FlatWindow => (None, vec![FLAT_WINDOW_REASON.to_owned()]),
        };
        self.emit_feature(
            blended_ta_kind(ta_kind),
            produced_at,
            produced_at.to_owned(),
            values,
            reasons,
        )
    }

    /// The per-venue identity lanes for this decision, appended after the
    /// blended set. Each lane reads only its own venue's assembled decision, so
    /// nothing here can change a `btc.external.*` frame (ADR 0024).
    fn emit_venue_features(
        &mut self,
        now_ns: u64,
        produced_at: &str,
        absorb: bool,
    ) -> Result<Vec<SignalUpdate>, SpotSignalError> {
        let mut updates = Vec::with_capacity(self.venue_lanes.len() * VenueFeatureKind::ALL.len());
        for venue in VENUE_SPOT_VENUES {
            if !self.venue_lanes.contains_key(&venue) {
                continue;
            }
            let decision = self.venue_decision(venue, now_ns)?;
            let publisher_epoch = self.publisher_epoch.clone();
            let lane = self
                .venue_lanes
                .get_mut(&venue)
                .ok_or_else(|| SpotSignalError::State(format!("missing venue lane {venue:?}")))?;
            updates.extend(if absorb {
                lane.emit(&publisher_epoch, produced_at, &decision, None)?
            } else {
                lane.emit_outputs(&publisher_epoch, produced_at, &decision, None)?
            });
        }
        Ok(updates)
    }

    /// Count one admitted best-quote update on its venue lane (C3 row 8).
    ///
    /// The midpoint is read back from the spot reference rather than re-parsed,
    /// so the counted quote is exactly the one the venue's other identities see;
    /// a ticker row that leaves the venue without a fresh usable quote counts no
    /// update.
    fn observe_lane_quote(
        &mut self,
        venue: Venue,
        logical_receipt_ns: u64,
        now_ns: u64,
    ) -> Result<(), SpotSignalError> {
        let Some(quote) = self.reference.venue_quote(venue, now_ns)? else {
            return Ok(());
        };
        if let Some(lane) = self.venue_lanes.get_mut(&venue) {
            lane.observe_quote(logical_receipt_ns, quote.midpoint);
        }
        Ok(())
    }

    /// Feed one admitted trade row to its venue lane's microstructure join.
    ///
    /// A Kraken subscription snapshot is a replay of trade history rather than
    /// live flow, so it is excluded here exactly as the rolling flow series
    /// excludes it — both ask the one classifier rather than re-deriving it.
    fn observe_lane_trade(
        &mut self,
        venue: Venue,
        event: &SourceEvent,
        logical_receipt_ns: u64,
    ) -> Result<(), SpotSignalError> {
        if is_replayed_trade_snapshot(venue, event)? {
            return Ok(());
        }
        let trade = parse_admitted_trade(event)?;
        let Some(lane) = self.venue_lanes.get_mut(&venue) else {
            return Ok(());
        };
        lane.observe_trade(logical_receipt_ns, &event.event_time, trade)
    }

    /// Assemble everything one venue's identities read on this decision.
    fn venue_decision(&self, venue: Venue, now_ns: u64) -> Result<VenueDecision, SpotSignalError> {
        let book_state = match venue {
            Venue::Coinbase => &self.coinbase_book,
            Venue::Kraken => &self.kraken_book,
            Venue::Binance | Venue::Bybit => unreachable!(
                "the spot processor publishes only the USD venues; the rights-gated \
                 venues run on their own venue lane (B1)"
            ),
        };
        Ok(VenueDecision {
            now_ns,
            quote: self.reference.venue_quote(venue, now_ns)?,
            book: book_state.metrics(now_ns, DEPTH_BAND_BPS)?,
            book_invalid: book_state.invalid_reason().is_some(),
            alive: self.venue_alive(venue, now_ns),
            flow: FlowMetrics {
                one_second: self.venue_flow.venue_window(venue, now_ns, ONE_SECOND_NS)?,
                ten_seconds: self
                    .venue_flow
                    .venue_window(venue, now_ns, TEN_SECONDS_NS)?,
                sixty_seconds: self
                    .venue_flow
                    .venue_window(venue, now_ns, SIXTY_SECONDS_NS)?,
            },
            flow_warmed: self.venue_flow.venue_warmed(venue, now_ns),
            flow_fresh: self.venue_flow.venue_fresh(venue, now_ns),
            ofi_one_second: self.venue_ofi.venue_window(venue, now_ns, ONE_SECOND_NS)?,
            ofi_ten_seconds: self.venue_ofi.venue_window(venue, now_ns, TEN_SECONDS_NS)?,
            ofi_sixty_seconds: self
                .venue_ofi
                .venue_window(venue, now_ns, SIXTY_SECONDS_NS)?,
            ofi_warmed: self.venue_ofi.venue_warmed(venue, now_ns),
            venue_unavailable: self.reference.venue_is_unavailable(venue),
            // The USD spot venues carry the full displayed book, so their 5 bps
            // band is never truncated.
            band_truncated: false,
        })
    }

    /// Pair this decision's momentum with this decision's absorption state.
    ///
    /// Binding both inputs to the same decision is deliberate: every spot
    /// feature is recomputed on each cycle, so "the latest" reading of each
    /// input is this cycle's, and a reading is never assembled from an
    /// absorption state that a later cycle has already superseded.
    fn emit_early_move_fade(
        &mut self,
        momentum: Option<MomentumMetrics>,
        absorption_state: Option<&'static str>,
        produced_at: &str,
    ) -> Result<SignalUpdate, SpotSignalError> {
        let mut candidate = EarlyMoveFadeContextCandidate::with_default_threshold();
        let big_move_bps = candidate.big_move_bps();
        let values = match (momentum, absorption_state) {
            (Some(momentum), Some(absorption_state)) => {
                candidate.observe_momentum(
                    momentum.return_1s,
                    momentum.return_30s,
                    momentum_direction(momentum.return_10s),
                );
                match candidate.observe_absorption(absorption_from_str(absorption_state)?) {
                    FadeOutcome::Ready(context) => Some(BTreeMap::from([
                        ("state".to_owned(), json!(context.state.as_str())),
                        (
                            "return_bps_1s".to_owned(),
                            decimal_value(context.return_bps_1s),
                        ),
                        (
                            "direction_10s".to_owned(),
                            json!(context.direction_10s.as_str()),
                        ),
                        (
                            "absorption_state".to_owned(),
                            json!(context.absorption_state.as_str()),
                        ),
                        ("big_move_bps".to_owned(), decimal_value(big_move_bps)),
                        ("observation_time".to_owned(), json!(produced_at)),
                    ])),
                    FadeOutcome::NotReady { reason: _ } => None,
                }
            }
            _ => None,
        };
        let mut reasons = self.book_failure_reasons();
        if values.is_none() && reasons.is_empty() {
            reasons.push("upstream_measurement_unavailable".to_owned());
        }
        self.emit_feature(
            FeatureKind::EarlyMoveFadeContext,
            produced_at,
            produced_at.to_owned(),
            values,
            reasons,
        )
    }

    fn emit_cross_venue_dispersion(
        &mut self,
        reference: &SignalUpdate,
        produced_at: &str,
    ) -> Result<SignalUpdate, SpotSignalError> {
        let values = (|| -> Result<Option<BTreeMap<String, Value>>, SpotSignalError> {
            let Some(coinbase) = optional_decimal(&reference.values, "coinbase_mid")? else {
                return Ok(None);
            };
            let Some(kraken) = optional_decimal(&reference.values, "kraken_mid")? else {
                return Ok(None);
            };
            let Some(dispersion) = optional_decimal(&reference.values, "dispersion_bps")? else {
                return Ok(None);
            };
            let Some(max_age) = reference
                .values
                .get("max_input_age_ms")
                .and_then(Value::as_u64)
            else {
                return Ok(None);
            };
            let Some(observation_time) = reference
                .values
                .get("reference_time")
                .and_then(Value::as_str)
            else {
                return Ok(None);
            };
            let absolute = checked_sub(coinbase.max(kraken), coinbase.min(kraken))?;
            let leading = match coinbase.cmp(&kraken) {
                Ordering::Greater => "coinbase",
                Ordering::Less => "kraken",
                Ordering::Equal => "tied",
            };
            Ok(Some(BTreeMap::from([
                ("coinbase_mid".to_owned(), decimal_value(coinbase)),
                ("kraken_mid".to_owned(), decimal_value(kraken)),
                ("absolute_difference".to_owned(), decimal_value(absolute)),
                ("dispersion_bps".to_owned(), decimal_value(dispersion)),
                ("leading_venue".to_owned(), json!(leading)),
                (
                    "reference_guard_state".to_owned(),
                    json!(if dispersion <= Decimal::from(MAX_DISPERSION_BPS) {
                        "within_guard"
                    } else {
                        "outside_guard"
                    }),
                ),
                ("max_input_age_ms".to_owned(), json!(max_age)),
                ("observation_time".to_owned(), json!(observation_time)),
            ])))
        })()?;
        let event_time = values
            .as_ref()
            .and_then(|values| values.get("observation_time"))
            .and_then(Value::as_str)
            .unwrap_or(produced_at)
            .to_owned();
        self.emit_feature(
            FeatureKind::CrossVenueDispersion,
            produced_at,
            event_time,
            values,
            vec!["reference_constituents_unavailable".to_owned()],
        )
    }

    fn emit_micro_momentum(
        &mut self,
        metrics: Option<MomentumMetrics>,
        produced_at: &str,
    ) -> Result<SignalUpdate, SpotSignalError> {
        let sample_count = u16::try_from(self.volatility.len()).unwrap_or(u16::MAX);
        let values =
            metrics.map(|metrics| micro_momentum_values(metrics, sample_count, produced_at));
        self.emit_feature(
            FeatureKind::MicroMomentum,
            produced_at,
            produced_at.to_owned(),
            values,
            vec!["reference_history_warming_up".to_owned()],
        )
    }

    fn emit_realized_volatility(
        &mut self,
        produced_at: &str,
    ) -> Result<SignalUpdate, SpotSignalError> {
        let metrics = self.volatility.metrics()?;
        let values = metrics.map(|metrics| realized_volatility_values(&metrics, produced_at));
        self.emit_feature(
            FeatureKind::RealizedVolatility,
            produced_at,
            produced_at.to_owned(),
            values,
            vec!["reference_history_warming_up".to_owned()],
        )
    }

    fn emit_horizon_momentum(
        &mut self,
        produced_at: &str,
    ) -> Result<SignalUpdate, SpotSignalError> {
        let values = self
            .horizon
            .momentum()?
            .map(|metrics| horizon_momentum_values(metrics, produced_at));
        self.emit_feature(
            FeatureKind::HorizonMomentum,
            produced_at,
            produced_at.to_owned(),
            values,
            vec!["reference_history_warming_up".to_owned()],
        )
    }

    fn emit_horizon_volatility(
        &mut self,
        produced_at: &str,
    ) -> Result<SignalUpdate, SpotSignalError> {
        let values = self
            .horizon
            .volatility()?
            .map(|metrics| horizon_volatility_values(metrics, produced_at));
        self.emit_feature(
            FeatureKind::HorizonVolatility,
            produced_at,
            produced_at.to_owned(),
            values,
            vec!["reference_history_warming_up".to_owned()],
        )
    }

    fn emit_vwap_deviation(
        &mut self,
        now_ns: u64,
        reference: &SignalUpdate,
        flow: FlowMetrics,
        produced_at: &str,
    ) -> Result<SignalUpdate, SpotSignalError> {
        let mut reasons = Vec::new();
        if !self.flow.warmed(now_ns) {
            reasons.push("trade_history_warming_up".to_owned());
        }
        if !self.flow.venues_fresh(now_ns) {
            reasons.push("trade_venue_stale_or_missing".to_owned());
        }
        let reference_price = optional_decimal(&reference.values, "reference_price")?;
        if reference_price.is_none() {
            reasons.push("reference_unavailable".to_owned());
        }
        if flow.ten_seconds.base_volume <= Decimal::ZERO
            || flow.sixty_seconds.base_volume <= Decimal::ZERO
        {
            reasons.push("trade_window_empty".to_owned());
        }
        let values = if reasons.is_empty() {
            let reference_price = reference_price.ok_or_else(|| {
                SpotSignalError::State("ready VWAP lost reference price".to_owned())
            })?;
            let vwap_10s = checked_div(
                flow.ten_seconds.gross_notional,
                flow.ten_seconds.base_volume,
            )?;
            let vwap_60s = checked_div(
                flow.sixty_seconds.gross_notional,
                flow.sixty_seconds.base_volume,
            )?;
            Some(BTreeMap::from([
                ("vwap_10s".to_owned(), decimal_value(vwap_10s)),
                ("vwap_60s".to_owned(), decimal_value(vwap_60s)),
                ("reference_price".to_owned(), decimal_value(reference_price)),
                (
                    "deviation_bps_10s".to_owned(),
                    decimal_value(return_bps(reference_price, vwap_10s)?),
                ),
                (
                    "deviation_bps_60s".to_owned(),
                    decimal_value(return_bps(reference_price, vwap_60s)?),
                ),
                (
                    "base_volume_60s".to_owned(),
                    decimal_value(flow.sixty_seconds.base_volume),
                ),
                (
                    "trade_count_60s".to_owned(),
                    json!(
                        u32::try_from(flow.sixty_seconds.count)
                            .map_err(|_| SpotSignalError::Counter)?
                    ),
                ),
                ("window_close".to_owned(), json!(produced_at)),
            ]))
        } else {
            None
        };
        self.emit_feature(
            FeatureKind::VwapDeviation,
            produced_at,
            produced_at.to_owned(),
            values,
            reasons,
        )
    }

    fn emit_spot_microprice(
        &mut self,
        now_ns: u64,
        pair: Option<&BookPairMetrics>,
        produced_at: &str,
    ) -> Result<SignalUpdate, SpotSignalError> {
        let values = pair
            .map(|pair| -> Result<BTreeMap<String, Value>, SpotSignalError> {
                let microprice = checked_mean(pair.coinbase.microprice, pair.kraken.microprice)
                    .ok_or_else(|| {
                        SpotSignalError::Arithmetic(
                            "combined microprice is not representable".to_owned(),
                        )
                    })?;
                let midpoint = checked_mean(pair.coinbase.midpoint, pair.kraken.midpoint)
                    .ok_or_else(|| {
                        SpotSignalError::Arithmetic(
                            "combined book midpoint is not representable".to_owned(),
                        )
                    })?;
                let max_age = max_book_age_ms(now_ns, pair)?;
                let observation_time = latest_book_time(pair);
                Ok(BTreeMap::from([
                    ("microprice".to_owned(), decimal_value(microprice)),
                    (
                        "coinbase_microprice".to_owned(),
                        decimal_value(pair.coinbase.microprice),
                    ),
                    (
                        "kraken_microprice".to_owned(),
                        decimal_value(pair.kraken.microprice),
                    ),
                    ("book_midpoint".to_owned(), decimal_value(midpoint)),
                    (
                        "microprice_deviation_bps".to_owned(),
                        decimal_value(return_bps(microprice, midpoint)?),
                    ),
                    ("max_book_age_ms".to_owned(), json!(max_age)),
                    ("observation_time".to_owned(), json!(observation_time)),
                ]))
            })
            .transpose()?;
        let event_time = values
            .as_ref()
            .and_then(|values| values.get("observation_time"))
            .and_then(Value::as_str)
            .unwrap_or(produced_at)
            .to_owned();
        let mut reasons = self.book_failure_reasons();
        if reasons.is_empty() {
            reasons.push("book_snapshot_stale_or_missing".to_owned());
        }
        self.emit_feature(
            FeatureKind::SpotMicroprice,
            produced_at,
            event_time,
            values,
            reasons,
        )
    }

    fn emit_depth_imbalance(
        &mut self,
        now_ns: u64,
        pair: Option<&BookPairMetrics>,
        metrics: Option<DepthMetrics>,
        produced_at: &str,
    ) -> Result<SignalUpdate, SpotSignalError> {
        let values = match (pair, metrics) {
            (Some(pair), Some(metrics)) => Some(BTreeMap::from([
                ("band_bps".to_owned(), json!(DEPTH_BAND_BPS)),
                (
                    "coinbase_imbalance".to_owned(),
                    decimal_value(metrics.coinbase_imbalance),
                ),
                (
                    "kraken_imbalance".to_owned(),
                    decimal_value(metrics.kraken_imbalance),
                ),
                (
                    "combined_imbalance".to_owned(),
                    decimal_value(metrics.combined_imbalance),
                ),
                (
                    "bid_depth_base".to_owned(),
                    decimal_value(metrics.bid_depth),
                ),
                (
                    "ask_depth_base".to_owned(),
                    decimal_value(metrics.ask_depth),
                ),
                (
                    "max_book_age_ms".to_owned(),
                    json!(max_book_age_ms(now_ns, pair)?),
                ),
                ("observation_time".to_owned(), json!(latest_book_time(pair))),
            ])),
            _ => None,
        };
        let event_time = values
            .as_ref()
            .and_then(|values| values.get("observation_time"))
            .and_then(Value::as_str)
            .unwrap_or(produced_at)
            .to_owned();
        let mut reasons = self.book_failure_reasons();
        if reasons.is_empty() {
            reasons.push("book_snapshot_stale_or_missing".to_owned());
        }
        self.emit_feature(
            FeatureKind::ActionableDepthImbalance,
            produced_at,
            event_time,
            values,
            reasons,
        )
    }

    fn emit_ofi(
        &mut self,
        now_ns: u64,
        produced_at: &str,
    ) -> Result<SignalUpdate, SpotSignalError> {
        // Integrity and warm-up degradations stand; only the book-quiet path
        // moves to the aliveness rule (SIG-046). OFI reads the delta series, not
        // the book snapshot, so an eventless window on an alive venue is a
        // truthful zero; a crossed/invalid book (book_failure_reasons) and an
        // unwarmed or unsnapshotted history (ofi not warmed) still degrade.
        let mut reasons = self.book_failure_reasons();
        if !self.ofi.warmed(now_ns) {
            reasons.push("book_flow_history_warming_up".to_owned());
        }
        // Silence is reported only when no integrity fault already explains the
        // withheld window, preserving the pre-SIG-046 reason set in the overlap.
        if reasons.is_empty() && !self.venues_alive(now_ns) {
            reasons.push("book_snapshot_stale_or_missing".to_owned());
        }
        let one = self.ofi.window(now_ns, ONE_SECOND_NS)?;
        let ten = self.ofi.window(now_ns, TEN_SECONDS_NS)?;
        let sixty = self.ofi.window(now_ns, SIXTY_SECONDS_NS)?;
        let values = if reasons.is_empty() {
            Some(ofi_values(one, ten, sixty, produced_at)?)
        } else {
            None
        };
        self.emit_feature(
            FeatureKind::OrderFlowImbalance,
            produced_at,
            produced_at.to_owned(),
            values,
            reasons,
        )
    }

    fn emit_aggressor_flow(
        &mut self,
        now_ns: u64,
        metrics: FlowMetrics,
        produced_at: &str,
    ) -> Result<SignalUpdate, SpotSignalError> {
        let mut reasons = Vec::new();
        if !self.flow.warmed(now_ns) {
            reasons.push("trade_history_warming_up".to_owned());
        }
        // Quiet-window (SIG-046): a trade-quiet but alive venue yields truthful
        // zeros; only a venue with no admitted input in the bound degrades.
        if !self.venues_alive(now_ns) {
            reasons.push("trade_venue_stale_or_missing".to_owned());
        }
        let values = if reasons.is_empty() {
            Some(aggressor_flow_values(&metrics, produced_at)?)
        } else {
            None
        };
        self.emit_feature(
            FeatureKind::AggressorFlow,
            produced_at,
            produced_at.to_owned(),
            values,
            reasons,
        )
    }

    /// Emits the absorption reading and returns its classified state alongside
    /// it, so the early-move fade context folds over the same classification
    /// this decision published instead of recomputing it (STANDING-PRACTICES
    /// rule 6, one authority per fact).
    fn emit_absorption(
        &mut self,
        momentum: Option<MomentumMetrics>,
        depth: Option<DepthMetrics>,
        flow: FlowMetrics,
        produced_at: &str,
    ) -> Result<(SignalUpdate, Option<&'static str>), SpotSignalError> {
        let flow_ready = self
            .emitters
            .get(&FeatureKind::AggressorFlow)
            .is_some_and(|emitter| {
                emitter
                    .last_quality
                    .as_ref()
                    .is_some_and(|quality| quality.state == QualityState::Ready)
            });
        let values = match (momentum, depth, flow_ready) {
            (Some(momentum), Some(depth), true) => {
                let total_depth = checked_add(depth.bid_depth, depth.ask_depth)?;
                let signed_base = flow.ten_seconds.signed_base;
                let flow_to_depth = checked_div(signed_base.abs(), total_depth)?;
                let response = momentum.return_10s.abs();
                let response_per_turn = if flow_to_depth.is_zero() {
                    Decimal::ZERO
                } else {
                    checked_div(response, flow_to_depth)?
                };
                let state = classify_absorption(flow_to_depth, response_per_turn)?;
                Some(BTreeMap::from([
                    (
                        "signed_flow_notional_usd_10s".to_owned(),
                        decimal_value(flow.ten_seconds.signed_notional),
                    ),
                    (
                        "price_response_bps_10s".to_owned(),
                        decimal_value(momentum.return_10s),
                    ),
                    (
                        "flow_to_depth_ratio".to_owned(),
                        decimal_value(flow_to_depth),
                    ),
                    (
                        "response_bps_per_depth_turn".to_owned(),
                        decimal_value(response_per_turn),
                    ),
                    (
                        "flow_direction".to_owned(),
                        json!(flow_direction(flow.ten_seconds.signed_notional)),
                    ),
                    ("state".to_owned(), json!(state)),
                    ("window_close".to_owned(), json!(produced_at)),
                ]))
            }
            _ => None,
        };
        let mut reasons = self.book_failure_reasons();
        if values.is_none() && reasons.is_empty() {
            reasons.push("upstream_measurement_unavailable".to_owned());
        }
        let state = values
            .as_ref()
            .and_then(|values| values.get("state"))
            .and_then(Value::as_str)
            .map(absorption_state_word)
            .transpose()?;
        let update = self.emit_feature(
            FeatureKind::AbsorptionState,
            produced_at,
            produced_at.to_owned(),
            values,
            reasons,
        )?;
        Ok((update, state))
    }

    fn emit_liquidity_shock(
        &mut self,
        now_ns: u64,
        pair: Option<&BookPairMetrics>,
        produced_at: &str,
    ) -> Result<SignalUpdate, SpotSignalError> {
        let Some(pair) = pair else {
            self.liquidity_samples.clear();
            self.previous_liquidity_state = None;
            let mut reasons = self.book_failure_reasons();
            reasons.push("book_history_warming_up".to_owned());
            return self.emit_feature(
                FeatureKind::LiquidityShock,
                produced_at,
                produced_at.to_owned(),
                None,
                reasons,
            );
        };
        if self.liquidity_samples.back().is_some_and(|sample| {
            now_ns <= sample.logical_time_ns
                || now_ns.saturating_sub(sample.logical_time_ns) >= MISSED_OBSERVATION_NS
        }) {
            self.liquidity_samples.clear();
            self.previous_liquidity_state = None;
        }
        let current = liquidity_sample(pair, now_ns)?;
        let metrics = if self.liquidity_samples.len() == LIQUIDITY_BASELINE_SAMPLES {
            let baseline = mean_liquidity(&self.liquidity_samples)?;
            if baseline.depth <= Decimal::ZERO || baseline.spread_bps <= Decimal::ZERO {
                None
            } else {
                let depth_ratio = checked_div(current.depth, baseline.depth)?;
                let spread_ratio = checked_div(current.spread_bps, baseline.spread_bps)?;
                let state =
                    classify_liquidity(depth_ratio, spread_ratio, self.previous_liquidity_state);
                Some((baseline, depth_ratio, spread_ratio, state))
            }
        } else {
            None
        };
        if self.liquidity_samples.len() == MAX_LIQUIDITY_SAMPLES {
            self.liquidity_samples.pop_front();
        }
        self.liquidity_samples.push_back(current);
        let values = metrics.map(|(baseline, depth_ratio, spread_ratio, state)| {
            self.previous_liquidity_state = Some(state);
            liquidity_shock_values(
                baseline,
                current,
                depth_ratio,
                spread_ratio,
                state,
                produced_at,
            )
        });
        self.emit_feature(
            FeatureKind::LiquidityShock,
            produced_at,
            produced_at.to_owned(),
            values,
            vec!["book_history_warming_up".to_owned()],
        )
    }

    fn momentum_metrics(&self) -> Option<MomentumMetrics> {
        momentum_from_ring(&self.volatility)
    }

    fn book_pair(&self, now_ns: u64) -> Result<Option<BookPairMetrics>, SpotSignalError> {
        let Some(coinbase) = self.coinbase_book.metrics(now_ns, DEPTH_BAND_BPS)? else {
            return Ok(None);
        };
        let Some(kraken) = self.kraken_book.metrics(now_ns, DEPTH_BAND_BPS)? else {
            return Ok(None);
        };
        Ok(Some(BookPairMetrics { coinbase, kraken }))
    }

    fn flow_metrics(&self, now_ns: u64) -> Result<FlowMetrics, SpotSignalError> {
        Ok(FlowMetrics {
            one_second: self.flow.window(now_ns, ONE_SECOND_NS)?,
            ten_seconds: self.flow.window(now_ns, TEN_SECONDS_NS)?,
            sixty_seconds: self.flow.window(now_ns, SIXTY_SECONDS_NS)?,
        })
    }

    fn validate_source_event(&self, event: &SourceEvent) -> Result<(), SpotSignalError> {
        let definition = self
            .source_definitions
            .get(&event.source_id)
            .ok_or_else(|| SpotSignalError::UnsupportedSource(event.source_id.clone()))?;
        event.validate_against(definition)?;
        Ok(())
    }

    fn accept_trade_ordering(&mut self, event: &SourceEvent) -> Result<bool, SpotSignalError> {
        match self.trade_ordering.get_mut(&event.source_id) {
            None => {
                self.trade_ordering.insert(
                    event.source_id.clone(),
                    (event.publisher_epoch.clone(), event.sequence),
                );
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

    fn observe_time(&mut self, now_ns: u64) -> Result<(), SpotSignalError> {
        if self.last_now_ns.is_some_and(|previous| now_ns < previous) {
            return Err(SpotSignalError::TimeRegression);
        }
        self.last_now_ns = Some(now_ns);
        Ok(())
    }

    fn logical_timestamp(&self, now_ns: u64) -> Result<DateTime<Utc>, SpotSignalError> {
        logical_timestamp(self.wall_start, now_ns)
    }

    fn emitter_mut(&mut self, kind: FeatureKind) -> Result<&mut FeatureEmitter, SpotSignalError> {
        self.emitters
            .get_mut(&kind)
            .ok_or_else(|| SpotSignalError::State(format!("missing emitter {}", kind.id())))
    }

    fn emit_feature(
        &mut self,
        kind: FeatureKind,
        produced_at: &str,
        event_time: String,
        mut ready_values: Option<BTreeMap<String, Value>>,
        mut reasons: Vec<String>,
    ) -> Result<SignalUpdate, SpotSignalError> {
        // The blended readings stay an every-venue AND: either venue going
        // unavailable withholds every blended feature.
        for venue in VENUE_SPOT_VENUES {
            if self.reference.venue_is_unavailable(venue) {
                ready_values = None;
                reasons.push(venue_unavailable_reason(venue).to_owned());
            }
        }
        let publisher_epoch = self.publisher_epoch.clone();
        self.emitter_mut(kind)?.emit(
            &publisher_epoch,
            produced_at,
            event_time,
            ready_values,
            reasons,
        )
    }
}

impl SpotSignalCheckpointState {
    pub fn validate(&self) -> Result<(), SpotSignalError> {
        self.reference.validate()?;
        let required: BTreeSet<&str> = FeatureKind::ALL.iter().map(|kind| kind.id()).collect();
        let optional: BTreeSet<&str> = VENUE_SPOT_VENUES
            .iter()
            .flat_map(|venue| {
                VenueFeatureKind::ALL
                    .iter()
                    .map(move |kind| kind.id(*venue))
            })
            .collect();
        let actual: BTreeSet<&str> = self.features.keys().map(String::as_str).collect();
        // The blended features are always present; the per-venue features are
        // present for every published build and absent only for the suppressed
        // determinism harness. Any other identity is a foreign checkpoint.
        if !required.is_subset(&actual)
            || actual
                .iter()
                .any(|id| !required.contains(id) && !optional.contains(id))
        {
            return Err(SpotSignalError::Checkpoint(
                "checkpoint feature set does not match this processor build".to_owned(),
            ));
        }
        for (id, feature) in &self.features {
            let has_lifetime_output = feature.lifetime_output_count > 0;
            if feature.previous_next_sequence > feature.lifetime_output_count
                || has_lifetime_output != feature.last_quality.is_some()
                || (!has_lifetime_output
                    && (feature.ever_ready || !feature.quality_reason_entries.is_empty()))
            {
                return Err(SpotSignalError::Checkpoint(format!(
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
                return Err(SpotSignalError::Checkpoint(format!(
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
                    return Err(SpotSignalError::Checkpoint(format!(
                        "feature {id} has invalid persisted quality state"
                    )));
                }
            }
        }
        match (self.reference.last_now_ns(), self.last_now_ns) {
            (Some(reference), Some(processor)) if reference > processor => {
                return Err(SpotSignalError::Checkpoint(
                    "reference logical time exceeds the processor admission time".to_owned(),
                ));
            }
            (Some(_), None) => {
                return Err(SpotSignalError::Checkpoint(
                    "reference logical time exists without processor admission time".to_owned(),
                ));
            }
            _ => {}
        }
        Ok(())
    }
}

pub fn contract_documents() -> Vec<(&'static str, &'static str)> {
    let mut documents = crate::spot_reference::contract_documents().to_vec();
    for kind in FeatureKind::ALL {
        documents.push((kind.id(), kind.raw_definition()));
    }
    for venue in VENUE_SPOT_VENUES {
        for kind in VenueFeatureKind::ALL {
            documents.push((kind.id(venue), kind.raw_definition(venue)));
        }
    }
    documents.extend([
        (COINBASE_BOOK_SOURCE_ID, COINBASE_BOOK_DEFINITION),
        (KRAKEN_BOOK_SOURCE_ID, KRAKEN_BOOK_DEFINITION),
        (COINBASE_TRADE_SOURCE_ID, COINBASE_TRADE_DEFINITION),
        (KRAKEN_TRADE_SOURCE_ID, KRAKEN_TRADE_DEFINITION),
    ]);
    documents.sort_by_key(|(name, _)| *name);
    documents
}

fn parse_definition<T>(raw: &str, name: &str) -> Result<T, SpotSignalError>
where
    T: serde::de::DeserializeOwned + Validate,
{
    let definition: T = serde_json::from_str(raw)
        .map_err(|error| SpotSignalError::Definition(format!("invalid {name} JSON: {error}")))?;
    definition
        .validate()
        .map_err(|error| SpotSignalError::Definition(format!("invalid {name}: {error}")))?;
    Ok(definition)
}

pub(crate) fn optional_decimal(
    values: &BTreeMap<String, Value>,
    name: &str,
) -> Result<Option<Decimal>, SpotSignalError> {
    values
        .get(name)
        .map(|value| {
            value
                .as_str()
                .ok_or_else(|| SpotSignalError::MissingField(name.to_owned()))
                .and_then(|text| parse_decimal_text(text).map_err(SpotSignalError::from))
        })
        .transpose()
}

// Maps every consumed spot stream to its venue for aliveness tracking. The set
// is closed to the sources SpotSignalProcessor::accepts admits.
/// A logical nanosecond stamp resolved against this process's wall-start
/// baseline. Sole authority for that conversion: the processor and every
/// per-venue lane call it rather than each deriving the offset.
fn logical_timestamp(
    wall_start: DateTime<Utc>,
    now_ns: u64,
) -> Result<DateTime<Utc>, SpotSignalError> {
    let delta = ChronoDuration::nanoseconds(
        i64::try_from(now_ns).map_err(|_| SpotSignalError::TimestampRange)?,
    );
    wall_start
        .checked_add_signed(delta)
        .ok_or(SpotSignalError::TimestampRange)
}

pub(crate) fn logical_timestamp_text(
    wall_start: DateTime<Utc>,
    now_ns: u64,
) -> Result<String, SpotSignalError> {
    Ok(logical_timestamp(wall_start, now_ns)?.to_rfc3339_opts(SecondsFormat::Nanos, true))
}

fn venue_of_source(source_id: &str) -> Option<Venue> {
    match source_id {
        COINBASE_TICKER_SOURCE_ID | COINBASE_BOOK_SOURCE_ID | COINBASE_TRADE_SOURCE_ID => {
            Some(Venue::Coinbase)
        }
        KRAKEN_TICKER_SOURCE_ID | KRAKEN_BOOK_SOURCE_ID | KRAKEN_TRADE_SOURCE_ID => {
            Some(Venue::Kraken)
        }
        _ => None,
    }
}

fn depth_metrics(pair: &BookPairMetrics) -> Result<DepthMetrics, SpotSignalError> {
    let bid_depth = checked_add(pair.coinbase.bid_depth, pair.kraken.bid_depth)?;
    let ask_depth = checked_add(pair.coinbase.ask_depth, pair.kraken.ask_depth)?;
    let total = checked_add(bid_depth, ask_depth)?;
    Ok(DepthMetrics {
        coinbase_imbalance: pair.coinbase.imbalance,
        kraken_imbalance: pair.kraken.imbalance,
        combined_imbalance: checked_div(checked_sub(bid_depth, ask_depth)?, total)?,
        bid_depth,
        ask_depth,
    })
}

fn liquidity_sample(
    pair: &BookPairMetrics,
    logical_time_ns: u64,
) -> Result<LiquiditySample, SpotSignalError> {
    let depth = checked_add(
        checked_add(pair.coinbase.bid_depth, pair.coinbase.ask_depth)?,
        checked_add(pair.kraken.bid_depth, pair.kraken.ask_depth)?,
    )?;
    let spread_bps =
        checked_mean(pair.coinbase.spread_bps, pair.kraken.spread_bps).ok_or_else(|| {
            SpotSignalError::Arithmetic("mean spread is not representable".to_owned())
        })?;
    Ok(LiquiditySample {
        logical_time_ns,
        depth,
        spread_bps,
    })
}

fn mean_liquidity(samples: &VecDeque<LiquiditySample>) -> Result<LiquiditySample, SpotSignalError> {
    let mut depth = Decimal::ZERO;
    let mut spread = Decimal::ZERO;
    for sample in samples {
        depth = checked_add(depth, sample.depth)?;
        spread = checked_add(spread, sample.spread_bps)?;
    }
    let count = Decimal::from(u32::try_from(samples.len()).map_err(|_| SpotSignalError::Counter)?);
    Ok(LiquiditySample {
        logical_time_ns: samples.back().map_or(0, |sample| sample.logical_time_ns),
        depth: checked_div(depth, count)?,
        spread_bps: checked_div(spread, count)?,
    })
}

fn classify_liquidity(
    depth_ratio: Decimal,
    spread_ratio: Decimal,
    previous: Option<LiquidityState>,
) -> LiquidityState {
    let shock = depth_ratio <= Decimal::new(50, 2) || spread_ratio >= Decimal::new(20, 1);
    if shock {
        return LiquidityState::Shock;
    }
    let thinning = depth_ratio < Decimal::new(75, 2) || spread_ratio > Decimal::new(15, 1);
    if thinning && previous == Some(LiquidityState::Shock) {
        LiquidityState::Recovering
    } else if thinning {
        LiquidityState::Thinning
    } else {
        LiquidityState::Normal
    }
}

fn classify_absorption(
    flow_to_depth: Decimal,
    response_per_turn: Decimal,
) -> Result<&'static str, SpotSignalError> {
    let minimum_turns = checked_div(
        Decimal::from(ABSORPTION_MIN_DEPTH_TURNS_NUMERATOR),
        Decimal::from(ABSORPTION_MIN_DEPTH_TURNS_DENOMINATOR),
    )?;
    Ok(if flow_to_depth < minimum_turns {
        "insufficient_flow"
    } else if response_per_turn <= Decimal::from(ABSORPTION_MAX_RESPONSE_BPS) {
        "absorbing"
    } else if response_per_turn >= Decimal::from(RESPONSIVE_MIN_RESPONSE_BPS) {
        "price_responsive"
    } else {
        "mixed"
    })
}

/// The one-, ten-, and thirty-second momentum of a one-second price ring.
///
/// Sole authority for the momentum computation, shared by the blended reading
/// over the reference price and each per-venue reading over that venue's own
/// displayed midpoint (ADR 0024).
fn momentum_from_ring(ring: &RealizedVolatility) -> Option<MomentumMetrics> {
    if ring.len() < 31 {
        return None;
    }
    let current = ring.price_from_back(0)?;
    let one = ring.price_from_back(1)?;
    let ten = ring.price_from_back(10)?;
    let thirty = ring.price_from_back(30)?;
    Some(MomentumMetrics {
        return_1s: return_bps(current, one).ok()?,
        return_10s: return_bps(current, ten).ok()?,
        return_30s: return_bps(current, thirty).ok()?,
    })
}

fn micro_momentum_values(
    metrics: MomentumMetrics,
    sample_count: u16,
    produced_at: &str,
) -> BTreeMap<String, Value> {
    BTreeMap::from([
        ("return_bps_1s".to_owned(), decimal_value(metrics.return_1s)),
        (
            "return_bps_10s".to_owned(),
            decimal_value(metrics.return_10s),
        ),
        (
            "return_bps_30s".to_owned(),
            decimal_value(metrics.return_30s),
        ),
        (
            "direction_10s".to_owned(),
            json!(direction_word(metrics.return_10s)),
        ),
        ("sample_count".to_owned(), json!(sample_count)),
        ("window_close".to_owned(), json!(produced_at)),
    ])
}

fn aggressor_flow_values(
    metrics: &FlowMetrics,
    produced_at: &str,
) -> Result<BTreeMap<String, Value>, SpotSignalError> {
    Ok(BTreeMap::from([
        (
            "signed_notional_usd_1s".to_owned(),
            decimal_value(metrics.one_second.signed_notional),
        ),
        (
            "signed_notional_usd_10s".to_owned(),
            decimal_value(metrics.ten_seconds.signed_notional),
        ),
        (
            "signed_notional_usd_60s".to_owned(),
            decimal_value(metrics.sixty_seconds.signed_notional),
        ),
        (
            "cumulative_volume_delta_base_10s".to_owned(),
            decimal_value(metrics.ten_seconds.signed_base),
        ),
        (
            "cumulative_volume_delta_base_60s".to_owned(),
            decimal_value(metrics.sixty_seconds.signed_base),
        ),
        (
            "gross_notional_usd_60s".to_owned(),
            decimal_value(metrics.sixty_seconds.gross_notional),
        ),
        (
            "trade_count_60s".to_owned(),
            json!(
                u32::try_from(metrics.sixty_seconds.count).map_err(|_| SpotSignalError::Counter)?
            ),
        ),
        (
            "flow_direction_10s".to_owned(),
            json!(flow_direction(metrics.ten_seconds.signed_notional)),
        ),
        ("window_close".to_owned(), json!(produced_at)),
    ]))
}

fn liquidity_shock_values(
    baseline: LiquiditySample,
    current: LiquiditySample,
    depth_ratio: Decimal,
    spread_ratio: Decimal,
    state: LiquidityState,
    produced_at: &str,
) -> BTreeMap<String, Value> {
    BTreeMap::from([
        ("depth_ratio_60s".to_owned(), decimal_value(depth_ratio)),
        ("spread_ratio_60s".to_owned(), decimal_value(spread_ratio)),
        (
            "current_depth_base".to_owned(),
            decimal_value(current.depth),
        ),
        (
            "baseline_depth_base".to_owned(),
            decimal_value(baseline.depth),
        ),
        (
            "current_spread_bps".to_owned(),
            decimal_value(current.spread_bps),
        ),
        (
            "baseline_spread_bps".to_owned(),
            decimal_value(baseline.spread_bps),
        ),
        (
            "state".to_owned(),
            json!(match state {
                LiquidityState::Normal => "normal",
                LiquidityState::Thinning => "thinning",
                LiquidityState::Shock => "shock",
                LiquidityState::Recovering => "recovering",
            }),
        ),
        (
            "sample_count".to_owned(),
            json!(u16::try_from(LIQUIDITY_BASELINE_SAMPLES).unwrap_or(u16::MAX)),
        ),
        ("window_close".to_owned(), json!(produced_at)),
    ])
}

fn ofi_values(
    one: OfiTotals,
    ten: OfiTotals,
    sixty: OfiTotals,
    produced_at: &str,
) -> Result<BTreeMap<String, Value>, SpotSignalError> {
    Ok(BTreeMap::from([
        ("ofi_base_1s".to_owned(), decimal_value(one.value)),
        ("ofi_base_10s".to_owned(), decimal_value(ten.value)),
        ("ofi_base_60s".to_owned(), decimal_value(sixty.value)),
        (
            "event_count_60s".to_owned(),
            json!(u32::try_from(sixty.count).map_err(|_| SpotSignalError::Counter)?),
        ),
        ("window_close".to_owned(), json!(produced_at)),
    ]))
}

fn latest_book_time(pair: &BookPairMetrics) -> &str {
    if pair.coinbase.parsed_event_time >= pair.kraken.parsed_event_time {
        &pair.coinbase.event_time
    } else {
        &pair.kraken.event_time
    }
}

fn max_book_age_ms(now_ns: u64, pair: &BookPairMetrics) -> Result<u64, SpotSignalError> {
    let oldest = pair
        .coinbase
        .logical_receipt_ns
        .min(pair.kraken.logical_receipt_ns);
    book_age_ms(now_ns, oldest)
}

/// Age in milliseconds of one book observation at admission time.
fn book_age_ms(now_ns: u64, logical_receipt_ns: u64) -> Result<u64, SpotSignalError> {
    now_ns
        .checked_sub(logical_receipt_ns)
        .map(|age| age / 1_000_000)
        .ok_or(SpotSignalError::ReceiptAfterAdmission {
            receipt_ns: logical_receipt_ns,
            now_ns,
        })
}

pub(crate) fn return_bps(current: Decimal, prior: Decimal) -> Result<Decimal, SpotSignalError> {
    if prior <= Decimal::ZERO {
        return Err(SpotSignalError::MarketValue(
            "return denominator must be positive".to_owned(),
        ));
    }
    checked_mul(
        checked_div(checked_sub(current, prior)?, prior)?,
        Decimal::from(10_000_u32),
    )
}

pub(crate) fn direction_word(value: Decimal) -> &'static str {
    match value.cmp(&Decimal::ZERO) {
        Ordering::Greater => "up",
        Ordering::Less => "down",
        Ordering::Equal => "flat",
    }
}

/// The ten-second momentum direction as the fade computation's enum. Shares the
/// sign rule with [`direction`], which publishes the same classification as the
/// `micro_momentum` `direction_10s` word.
fn momentum_direction(value: Decimal) -> MomentumDirection {
    match value.cmp(&Decimal::ZERO) {
        Ordering::Greater => MomentumDirection::Up,
        Ordering::Less => MomentumDirection::Down,
        Ordering::Equal => MomentumDirection::Flat,
    }
}

/// Round-trips the published absorption word through the classifier's own
/// vocabulary, so a word this processor never publishes cannot reach the fade
/// computation.
fn absorption_state_word(state: &str) -> Result<&'static str, SpotSignalError> {
    absorption_from_str(state).map(AbsorptionState::as_str)
}

/// The published absorption word as the fade computation's enum. An unknown word
/// is a hard error rather than a defaulted state.
fn absorption_from_str(state: &str) -> Result<AbsorptionState, SpotSignalError> {
    match state {
        "insufficient_flow" => Ok(AbsorptionState::InsufficientFlow),
        "absorbing" => Ok(AbsorptionState::Absorbing),
        "price_responsive" => Ok(AbsorptionState::PriceResponsive),
        "mixed" => Ok(AbsorptionState::Mixed),
        other => Err(SpotSignalError::State(format!(
            "unsupported absorption state {other}"
        ))),
    }
}

fn flow_direction(value: Decimal) -> &'static str {
    match value.cmp(&Decimal::ZERO) {
        Ordering::Greater => "buy",
        Ordering::Less => "sell",
        Ordering::Equal => "flat",
    }
}

pub(crate) fn decimal_value(value: Decimal) -> Value {
    json!(
        value
            .round_dp_with_strategy(OUTPUT_DECIMAL_PLACES, RoundingStrategy::MidpointNearestEven)
            .normalize()
            .to_string()
    )
}

pub(crate) fn checked_add(left: Decimal, right: Decimal) -> Result<Decimal, SpotSignalError> {
    left.checked_add(right)
        .ok_or_else(|| SpotSignalError::Arithmetic("decimal addition overflowed".to_owned()))
}

pub(crate) fn checked_sub(left: Decimal, right: Decimal) -> Result<Decimal, SpotSignalError> {
    left.checked_sub(right)
        .ok_or_else(|| SpotSignalError::Arithmetic("decimal subtraction overflowed".to_owned()))
}

pub(crate) fn checked_mul(left: Decimal, right: Decimal) -> Result<Decimal, SpotSignalError> {
    left.checked_mul(right)
        .ok_or_else(|| SpotSignalError::Arithmetic("decimal multiplication overflowed".to_owned()))
}

pub(crate) fn checked_div(left: Decimal, right: Decimal) -> Result<Decimal, SpotSignalError> {
    if right.is_zero() {
        return Err(SpotSignalError::Arithmetic(
            "decimal division by zero".to_owned(),
        ));
    }
    left.checked_div(right)
        .ok_or_else(|| SpotSignalError::Arithmetic("decimal division overflowed".to_owned()))
}

#[derive(Debug, Error)]
pub enum SpotSignalError {
    #[error(transparent)]
    Reference(#[from] SpotReferenceError),
    #[error(transparent)]
    Validation(#[from] signals_core::ValidationError),
    #[error(transparent)]
    Decimal(#[from] signals_core::DecimalTextError),
    #[error("embedded definition error: {0}")]
    Definition(String),
    #[error("invalid spot-signal checkpoint: {0}")]
    Checkpoint(String),
    #[error("unsupported source event {0}")]
    UnsupportedSource(String),
    #[error("required field {0} is missing or has the wrong type")]
    MissingField(String),
    #[error("invalid {field} timestamp: {detail}")]
    Timestamp { field: &'static str, detail: String },
    #[error("book group error: {0}")]
    BookGroup(String),
    #[error("source ordering error: {0}")]
    Ordering(String),
    #[error("invalid market value: {0}")]
    MarketValue(String),
    #[error("arithmetic failed: {0}")]
    Arithmetic(String),
    #[error("processor state error: {0}")]
    State(String),
    #[error("bounded resource limit reached: {0}")]
    ResourceLimit(String),
    #[error("logical time regressed")]
    TimeRegression,
    #[error("logical receipt {receipt_ns}ns exceeds admission time {now_ns}ns")]
    ReceiptAfterAdmission { receipt_ns: u64, now_ns: u64 },
    #[error("logical time cannot be represented as a wall timestamp")]
    TimestampRange,
    #[error("signal output sequence exhausted")]
    SequenceExhausted,
    #[error("signal counter exhausted")]
    Counter,
}

#[cfg(test)]
mod tests {
    #[test]
    fn coalescing_drops_unchanged_restatements_until_the_interval() {
        use super::{PublishedMemory, coalesce_publications};
        use signals_core::{Quality, QualityState, SignalScope, SignalUpdate};
        use std::collections::BTreeMap;
        let make = |values: &[(&str, serde_json::Value)], ready: bool| SignalUpdate {
            schema_version: 3,
            signal_id: "x".into(),
            signal_version: "1.0.0".into(),
            publisher_epoch: "e".into(),
            sequence: 0,
            event_time: "2026-01-01T00:00:00Z".into(),
            produced_at: "2026-01-01T00:00:00Z".into(),
            valid_from: "2026-01-01T00:00:00Z".into(),
            valid_until: "2026-01-01T00:00:10Z".into(),
            quality: Quality {
                state: if ready { QualityState::Ready } else { QualityState::WarmingUp },
                reasons: if ready { Vec::new() } else { vec!["warming".into()] },
            },
            values: values.iter().map(|(k, v)| ((*k).to_owned(), v.clone())).collect(),
            scope: SignalScope::Global,
        };
        let mut memory: BTreeMap<String, PublishedMemory> = BTreeMap::new();
        let intervals: BTreeMap<String, u64> = [("x".to_owned(), 1_000_000_000u64)].into();
        let ns = 1_000_000u64;
        let a = make(&[("mid", "1".into()), ("input_age_ms", 5.into())], true);
        assert_eq!(coalesce_publications(&mut memory, &intervals, 0, vec![a.clone()]).len(), 1);
        // Same reading, only the age moved: dropped.
        let b = make(&[("mid", "1".into()), ("input_age_ms", 90.into())], true);
        assert!(coalesce_publications(&mut memory, &intervals, 100 * ns, vec![b]).is_empty());
        // The reading changed: published at once.
        let c = make(&[("mid", "2".into()), ("input_age_ms", 91.into())], true);
        assert_eq!(coalesce_publications(&mut memory, &intervals, 101 * ns, vec![c.clone()]).len(), 1);
        // Unchanged, but the declared interval elapsed: restated.
        let d = make(&[("mid", "2".into()), ("input_age_ms", 1_100.into())], true);
        assert_eq!(coalesce_publications(&mut memory, &intervals, 1_101 * ns, vec![d]).len(), 1);
        // A quality change always publishes.
        let e = make(&[], false);
        assert_eq!(coalesce_publications(&mut memory, &intervals, 1_102 * ns, vec![e.clone()]).len(), 1);
        assert!(coalesce_publications(&mut memory, &intervals, 1_103 * ns, vec![e]).is_empty());
        // Unknown identities pass through.
        let mut other = make(&[], true);
        other.signal_id = "unknown".into();
        assert_eq!(coalesce_publications(&mut memory, &intervals, 1_104 * ns, vec![other.clone(), other]).len(), 2);
    }

    #[test]
    fn clock_derived_fields() {
        use super::is_clock_derived_field;
        for name in ["input_age_ms", "book_age_ms", "max_input_age_ms", "pyth_staleness_ms", "time_to_close_ms", "time_since_burst_ms"] {
            assert!(is_clock_derived_field(name), "{name}");
        }
        for name in ["midpoint", "observation_time", "baseline_window_ms", "realized_horizon_ms", "spread_bps"] {
            assert!(!is_clock_derived_field(name), "{name}");
        }
    }

    use super::*;

    const TEST_TIME: &str = "2026-08-25T00:00:00Z";

    fn dispersion_values() -> BTreeMap<String, Value> {
        BTreeMap::from([
            ("coinbase_mid".to_owned(), json!("60000")),
            ("kraken_mid".to_owned(), json!("60001")),
            ("absolute_difference".to_owned(), json!("1")),
            ("dispersion_bps".to_owned(), json!("0.16666667")),
            ("leading_venue".to_owned(), json!("kraken")),
            ("reference_guard_state".to_owned(), json!("within_guard")),
            ("max_input_age_ms".to_owned(), json!(0)),
            ("observation_time".to_owned(), json!(TEST_TIME)),
        ])
    }

    #[test]
    fn spot_decision_sequence_advances_only_for_ready_updates() {
        let mut emitter = FeatureEmitter::new(FeatureKind::CrossVenueDispersion)
            .expect("feature emitter constructs");
        for reason in [
            "reference_history_warming_up",
            "reference_constituents_unavailable",
        ] {
            let update = emitter
                .emit(
                    "spot-sequence-test",
                    TEST_TIME,
                    TEST_TIME.to_owned(),
                    None,
                    vec![reason.to_owned()],
                )
                .expect("non-ready evaluation emits");
            assert_eq!(update.sequence, 0);
            assert_ne!(update.quality.state, QualityState::Ready);
        }
        for expected_sequence in 0..3 {
            let ready = emitter
                .emit(
                    "spot-sequence-test",
                    TEST_TIME,
                    TEST_TIME.to_owned(),
                    Some(dispersion_values()),
                    Vec::new(),
                )
                .expect("ready decision emits");
            assert_eq!(ready.sequence, expected_sequence);
            assert_eq!(ready.quality.state, QualityState::Ready);
            if expected_sequence == 0 {
                let non_ready = emitter
                    .emit(
                        "spot-sequence-test",
                        TEST_TIME,
                        TEST_TIME.to_owned(),
                        None,
                        vec!["reference_constituents_unavailable".to_owned()],
                    )
                    .expect("intervening non-ready evaluation emits");
                assert_eq!(non_ready.sequence, 1);
            }
        }
    }

    #[test]
    fn evaluated_spot_venue_unavailability_wires_both_feature_reason_strings() {
        for (venue, unavailable_reason, expected_reason) in [
            (
                Venue::Coinbase,
                UnavailableReason::Disconnected,
                "coinbase_venue_unavailable",
            ),
            (
                Venue::Kraken,
                UnavailableReason::Recovery,
                "kraken_venue_unavailable",
            ),
        ] {
            let mut processor = SpotSignalProcessor::new(
                format!("{expected_reason}-test"),
                TEST_TIME.parse().expect("wall start"),
            )
            .expect("spot processor constructs");
            processor
                .reference
                .mark_unavailable(venue, unavailable_reason, 0)
                .expect("venue availability transition admits");
            let evaluations = processor.advance_time(0).expect("boundary evaluates");
            let carries_reason = |update: &&SignalUpdate| {
                update
                    .quality
                    .reasons
                    .iter()
                    .any(|reason| reason == expected_reason)
            };

            // Every blended feature is withheld: they are an every-venue AND.
            let blended = evaluations
                .iter()
                .filter(|update| {
                    update.signal_id.starts_with("btc.external.")
                        && update.signal_id != crate::SPOT_REFERENCE_ID
                })
                .collect::<Vec<_>>();
            assert_eq!(blended.len(), FeatureKind::ALL.len());
            assert!(blended.iter().all(carries_reason));

            // The dropped venue's own identities carry the same reason; the
            // peer venue's identities never see it (ADR 0024).
            for lane_venue in VENUE_SPOT_VENUES {
                let lane = evaluations
                    .iter()
                    .filter(|update| {
                        venue_spot_signal_ids(lane_venue).contains(&update.signal_id.as_str())
                    })
                    .collect::<Vec<_>>();
                assert_eq!(lane.len(), VenueFeatureKind::ALL.len());
                if lane_venue == venue {
                    assert!(lane.iter().all(carries_reason));
                } else {
                    assert!(!lane.iter().any(carries_reason));
                }
            }
        }
    }

    #[test]
    fn venue_feature_contract_table_covers_every_kind_exactly_once() {
        assert_eq!(VENUE_FEATURE_CONTRACTS.len(), VenueFeatureKind::ALL.len());
        for kind in VenueFeatureKind::ALL {
            assert_eq!(
                VENUE_FEATURE_CONTRACTS
                    .iter()
                    .filter(|contract| contract.kind == kind)
                    .count(),
                1
            );
            for venue in VENUE_SPOT_VENUES {
                let prefix = match venue {
                    Venue::Coinbase => "btc.coinbase.",
                    Venue::Kraken => "btc.kraken.",
                    Venue::Binance | Venue::Bybit => {
                        unreachable!("VENUE_SPOT_VENUES is the two USD venues")
                    }
                };
                assert!(kind.id(venue).starts_with(prefix), "{}", kind.id(venue));
                let definition: SignalDefinition =
                    serde_json::from_str(kind.raw_definition(venue)).expect("definition parses");
                assert_eq!(definition.signal_id, kind.id(venue));
                assert_eq!(definition.version, kind.expected_version());
            }
        }
        assert_eq!(VENUE_SPOT_SIGNAL_COUNT, VenueFeatureKind::ALL.len());
        assert_eq!(
            SPOT_SIGNAL_SET_COUNT,
            FeatureKind::ALL.len() + 1 + VENUE_SPOT_VENUES.len() * VENUE_SPOT_SIGNAL_COUNT
        );
    }

    #[test]
    fn absorption_classifier_covers_every_boundary_and_state() {
        assert_eq!(
            classify_absorption(Decimal::new(249_999, 6), Decimal::ZERO).expect("classifies"),
            "insufficient_flow"
        );
        assert_eq!(
            classify_absorption(Decimal::new(25, 2), Decimal::ONE).expect("classifies"),
            "absorbing"
        );
        assert_eq!(
            classify_absorption(Decimal::new(25, 2), Decimal::new(100_001, 5)).expect("classifies"),
            "mixed"
        );
        assert_eq!(
            classify_absorption(Decimal::new(25, 2), Decimal::new(299_999, 5)).expect("classifies"),
            "mixed"
        );
        assert_eq!(
            classify_absorption(Decimal::new(25, 2), Decimal::from(3_u32)).expect("classifies"),
            "price_responsive"
        );
    }

    #[test]
    fn liquidity_classifier_covers_boundaries_and_recovery_transition() {
        assert_eq!(
            classify_liquidity(Decimal::new(50, 2), Decimal::ONE, None),
            LiquidityState::Shock
        );
        assert_eq!(
            classify_liquidity(Decimal::ONE, Decimal::new(20, 1), None),
            LiquidityState::Shock
        );
        assert_eq!(
            classify_liquidity(Decimal::new(75, 2), Decimal::new(15, 1), None),
            LiquidityState::Normal
        );
        assert_eq!(
            classify_liquidity(Decimal::new(749_999, 6), Decimal::ONE, None),
            LiquidityState::Thinning
        );
        assert_eq!(
            classify_liquidity(Decimal::ONE, Decimal::new(150_001, 5), None),
            LiquidityState::Thinning
        );
        assert_eq!(
            classify_liquidity(
                Decimal::new(60, 2),
                Decimal::ONE,
                Some(LiquidityState::Shock),
            ),
            LiquidityState::Recovering
        );
        assert_eq!(
            classify_liquidity(Decimal::ONE, Decimal::ONE, Some(LiquidityState::Recovering),),
            LiquidityState::Normal
        );
    }

    #[test]
    fn volatility_regime_boundaries_are_inclusive() {
        assert_eq!(classify_volatility_ratio(Decimal::new(8, 1)), "contracting");
        assert_eq!(
            classify_volatility_ratio(Decimal::new(800_001, 6)),
            "stable"
        );
        assert_eq!(
            classify_volatility_ratio(Decimal::new(1_199_999, 6)),
            "stable"
        );
        assert_eq!(classify_volatility_ratio(Decimal::new(12, 1)), "expanding");
    }
}
