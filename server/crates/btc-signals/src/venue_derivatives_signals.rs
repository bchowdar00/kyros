//! Binance USD-M and Bybit linear tier-1 derivatives families (B2), consumed by
//! the two venue [`LaneProcessor`](crate::lane::LaneProcessor)s that B1 defines
//! (`BinanceProcessor` / `BybitProcessor`) on the A6 shell. One publisher per
//! venue runs the spot siblings and these five derivatives families together
//! (spec §2, R5): [`VenueDerivativesEngine`] is the derivatives half, owned by
//! the venue processor beside the spot [`VenueLane`](crate::spot_signals).
//!
//! The B2 families are `mark_index_basis_state`, `funding_state`,
//! `open_interest_delta`, `liquidation_pressure` and `perp_cvd`. The
//! [`DerivFamily`] enum is a shared fourteen (the five B2, the seven C3a `perp_*`
//! and the two C2 `perp_*`), but the families a venue actually PRODUCES are
//! per-venue: `DerivSpec::families`. Bybit produces all fourteen; Binance
//! produces twelve, because ruling E62 deleted the Binance USD-M `market`-socket
//! sources (`agg-trades`, `mark-price`, `liquidations`) that no host we own can
//! dial, taking `btc.binance.liquidation_pressure` and `btc.binance.perp_cvd`
//! with them (Bybit's twins are untouched). The two venues otherwise run
//! identical field lists and identical code paths, differing only by the
//! constants in the per-venue [`DerivSpec`] table (taxonomy rule 2, R11): which
//! source feeds each family, the liquidation notional pair and side field, and
//! the `feed_completeness` word. Binance's mark/index/funding state is fed from
//! the REST `premium-index` poll alone (F2/E50); the socket `mark-price` input is
//! gone (E62), so `mark_source`/`liquidation_source`/`trade_source` are `None`
//! for Binance.
//!
//! Arithmetic is `rust_decimal` only, through the checked helpers re-exported
//! from `spot_signals`; decimal outputs are rounded to eight places
//! round-half-to-even by [`decimal_value`]. State advances only on journaled
//! event timestamps and the journaled decision clock — no wall-clock read inside
//! a computation — so replay is byte-identical (spec §4).

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use signals_core::{SignalUpdate, SourceDefinition, SourceEvent, Validate, parse_decimal_text};

use crate::feed_health::{FeedCadence, FeedContract};
use crate::realized_volatility::{classify_volatility_ratio, root_mean_square};
use crate::spot_reference::Venue;
use crate::spot_signals::{
    FeatureEmitter, MISSED_OBSERVATION_NS, SpotSignalError, SpotSignalFeatureCheckpoint,
    apply_usdt_conversion, checked_add, checked_div, checked_mul, checked_sub, decimal_value,
    direction_word, logical_timestamp_text, return_bps,
};
use crate::ta::{FLAT_WINDOW_REASON, TaEngine, TaKind, TaOutcome};
use crate::usdt_reference::UsdtReferenceSnapshot;

/// The shared signal-set version for every B2 identity (spec §1).
const DERIV_SIGNAL_VERSION: &str = "1.0.0";
/// The quote currency both BTCUSDT perpetual instruments publish. E47: their
/// price, notional and quote-cost fields are converted USDT→USD through the
/// decision-time reference, so the declared quote currency is `usd`.
const QUOTE_CURRENCY: &str = "usd";

/// The quote-denominated output fields of one derivatives family that E47
/// converts USDT→USD (empty means the family has no quote-denominated field and
/// does not convert).
fn deriv_quote_fields(family: DerivFamily) -> &'static [&'static str] {
    match family {
        DerivFamily::MarkIndexBasisState => &["mark_price", "index_price"],
        DerivFamily::OpenInterestDelta => &["open_interest_notional_usd", "mark_price"],
        DerivFamily::PerpCvd => &[
            "signed_notional_usd_1s",
            "signed_notional_usd_10s",
            "signed_notional_usd_60s",
            "gross_notional_usd_60s",
        ],
        DerivFamily::LiquidationPressure => &[
            "notional_usd_10s",
            "notional_usd_60s",
            "notional_usd_300s",
            "long_notional_usd_60s",
            "short_notional_usd_60s",
            "max_single_notional_usd_300s",
        ],
        // funding_rate / *_bps / interval hours are rates and durations — neutral.
        DerivFamily::FundingState => &[],
        // C3a: the average true range and the bar close it is measured against
        // are price levels, as are the two exponential moving averages; the
        // remaining TA outputs are basis-point ratios, indices and counts.
        DerivFamily::PerpAtr => &["atr", "close"],
        DerivFamily::PerpEmaCross => &["ema_9", "ema_21"],
        DerivFamily::PerpRsi
        | DerivFamily::PerpRoc
        | DerivFamily::PerpMacd
        | DerivFamily::PerpBollinger
        | DerivFamily::PerpZScore => &[],
        // C2 (E49): perp momentum/volatility are reference-free — nothing in the
        // payload is a price level, only scale-free basis-point returns and their
        // RMS, so no field converts USDT→USD and no usdt_usd_rate is published.
        DerivFamily::PerpMomentum | DerivFamily::PerpVolatility => &[],
    }
}

const ONE_SECOND_NS: u64 = 1_000_000_000;
const TEN_SECONDS_NS: u64 = 10 * ONE_SECOND_NS;
const SIXTY_SECONDS_NS: u64 = 60 * ONE_SECOND_NS;
const FIVE_MINUTES_NS: u64 = 300 * ONE_SECOND_NS;
const FIFTEEN_MINUTES_NS: u64 = 900 * ONE_SECOND_NS;

/// Warm-up: 60 s uninterrupted observation for basis and funding (spec §4.1/4.2).
const WARMUP_60S_NS: u64 = SIXTY_SECONDS_NS;
/// Warm-up: 300 s for `liquidation_pressure` (§4.4).
const WARMUP_300S_NS: u64 = FIVE_MINUTES_NS;
/// Warm-up: 900 s for `open_interest_delta` (§4.3).
const WARMUP_900S_NS: u64 = FIFTEEN_MINUTES_NS;

/// History bound for basis readings: 60 s, at most 6000 entries per venue (§4.1).
const BASIS_HISTORY_MAX: usize = 6_000;
/// History bound for open-interest readings: 900 s, at most 90000 entries (§4.3).
const OI_HISTORY_MAX: usize = 90_000;
/// History bound for liquidation events: 300 s, at most 200000 entries (§4.4).
const LIQUIDATION_HISTORY_MAX: usize = 200_000;
/// Safety cap on the perp trade tape (60 s window); a fuzz guard against an
/// unbounded tape, never reached at real trade rates.
const TRADE_HISTORY_MAX: usize = 200_000;

/// A funding-info reading older than two poll intervals (7 200 000 ms) no longer
/// supplies `venue_declared` (spec §4.2).
const FUNDING_INFO_MAX_AGE_NS: u64 = 7_200 * ONE_SECOND_NS;

/// C2 (E49) perp momentum/volatility mark ring. The longest horizon in one-second
/// samples (15 minutes) plus one, so the 15 m return has a `now − 900 s`
/// reference. Warm-up is 901 contiguous one-second samples (spec §3).
const MARK_RING_MAX_SAMPLES: usize = 901;
/// Ring horizon offsets, in one-second samples: 10 s / 1 m / 5 m / 15 m (spec §1
/// Perps row). Four horizons, so the spot three-horizon `HorizonRing` is not
/// reused (builder brief).
const MARK_OFFSET_10S: usize = 10;
const MARK_OFFSET_1M: usize = 60;
const MARK_OFFSET_5M: usize = 300;
const MARK_OFFSET_15M: usize = 900;
/// E59: the one-second perp mark sampler holds the latest mark row for up to 3 s.
/// The live premium-index poll lands about every 1.27 s (median) and Bybit
/// `markPrice` carries multi-second gaps (a 37 s longest run in the observed
/// hour), so requiring a strictly new mark row in each of 901 consecutive seconds
/// could never warm the perp identities. Instead a decision second takes the
/// latest mark row when that row is younger than this bound; a mark row 3 s or
/// more old — a stalled poll or an absent mark — is a missing sample that breaks
/// continuity and restarts the 901-sample warm-up. Fabricated repeats of a held
/// mark are therefore bounded to two seconds, and a poll stall still invalidates
/// the bar. One rule, one constant for both the C2 mark ring and the C3a TA bars
/// (research default).
const PERP_MARK_HOLD_NS: u64 = 3 * ONE_SECOND_NS;

/// One family of the seven. The array order is the publication order. The two
/// C2 perp families (E49) are appended so the five B2 families keep their order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum DerivFamily {
    MarkIndexBasisState,
    FundingState,
    OpenInterestDelta,
    LiquidationPressure,
    PerpCvd,
    // C3a (E51): the technical-analysis pack on this venue's perpetual mark
    // series. `perp_` prefixed because the venue segment is shared with the
    // spot lane's `btc.<venue>.*` identities (the same convention `perp_cvd`
    // already uses).
    PerpAtr,
    PerpRsi,
    PerpRoc,
    PerpEmaCross,
    PerpMacd,
    PerpBollinger,
    PerpZScore,
    // C2 (E49): the reference-free perp momentum/volatility pack, appended after
    // the C3a families so the five B2 families keep their positions.
    PerpMomentum,
    PerpVolatility,
}

impl DerivFamily {
    pub(crate) const ALL: [Self; 14] = [
        Self::MarkIndexBasisState,
        Self::FundingState,
        Self::OpenInterestDelta,
        Self::LiquidationPressure,
        Self::PerpCvd,
        Self::PerpAtr,
        Self::PerpRsi,
        Self::PerpRoc,
        Self::PerpEmaCross,
        Self::PerpMacd,
        Self::PerpBollinger,
        Self::PerpZScore,
        Self::PerpMomentum,
        Self::PerpVolatility,
    ];

    const fn slug(self) -> &'static str {
        match self {
            Self::MarkIndexBasisState => "mark_index_basis_state",
            Self::FundingState => "funding_state",
            Self::OpenInterestDelta => "open_interest_delta",
            Self::LiquidationPressure => "liquidation_pressure",
            Self::PerpCvd => "perp_cvd",
            Self::PerpAtr => "perp_atr",
            Self::PerpRsi => "perp_rsi",
            Self::PerpRoc => "perp_roc",
            Self::PerpEmaCross => "perp_ema_cross",
            Self::PerpMacd => "perp_macd",
            Self::PerpBollinger => "perp_bollinger",
            Self::PerpZScore => "perp_zscore",
            Self::PerpMomentum => "perp_momentum",
            Self::PerpVolatility => "perp_volatility",
        }
    }

    /// The shared TA kind this family publishes, or `None` for the five B2
    /// derivatives families and the two C2 perp momentum/volatility families
    /// (which compute outside the shared-TA path, via `compute_perp_*`).
    const fn ta_kind(self) -> Option<TaKind> {
        match self {
            Self::PerpAtr => Some(TaKind::Atr),
            Self::PerpRsi => Some(TaKind::Rsi),
            Self::PerpRoc => Some(TaKind::Roc),
            Self::PerpEmaCross => Some(TaKind::EmaCross),
            Self::PerpMacd => Some(TaKind::Macd),
            Self::PerpBollinger => Some(TaKind::Bollinger),
            Self::PerpZScore => Some(TaKind::ZScore),
            // Enumerated for the same reason as `event_time_field`: a new
            // family must be classified here explicitly, not defaulted to "not
            // a technical-analysis family".
            Self::MarkIndexBasisState
            | Self::FundingState
            | Self::OpenInterestDelta
            | Self::LiquidationPressure
            | Self::PerpCvd
            | Self::PerpMomentum
            | Self::PerpVolatility => None,
        }
    }
}

/// Which venue-supplied pair prices one liquidation event and which price basis
/// it declares (spec §4.4).
#[derive(Debug, Clone, Copy)]
struct LiquidationShape {
    /// The base-quantity field on the source event.
    size_field: &'static str,
    /// The price field on the source event.
    price_field: &'static str,
    /// The `price_basis` enum the identity publishes.
    price_basis: &'static str,
    /// The venue side field and how it maps to a liquidated position side.
    side: LiquidationSideField,
}

/// How a venue's liquidation side field names the liquidated position side
/// (spec §4.4, R8).
#[derive(Debug, Clone, Copy)]
enum LiquidationSideField {
    /// Binance `liquidated_position_side` is already normalized (`long`/`short`).
    Normalized,
    /// Bybit `side` (`buy` = a long was liquidated, unconfirmed until the R1
    /// capture); the member is non-ready until [`DerivSpec::liq_side_confirmed`].
    BybitSide,
}

/// The static per-venue contract table: which source feeds each family, the
/// liquidation shape, the `feed_completeness` word, the perp trade size field,
/// and which families are unavailable from our hosts (E34).
struct DerivSpec {
    venue: Venue,
    token: &'static str,
    /// The families this venue PRODUCES, in publication order. Bybit produces the
    /// full [`DerivFamily::ALL`] fourteen; Binance produces twelve — E62 deleted
    /// its `liquidation_pressure` and `perp_cvd` with the `market`-socket sources
    /// they alone read. Every per-venue iteration and count is driven off this,
    /// so the shared enum stays fourteen while the venues differ.
    families: &'static [DerivFamily],
    /// The socket source that carries mark/index/funding (and, on Bybit, OI too):
    /// `Some(ticker)` for Bybit, `None` for Binance since E62 removed the socket
    /// `mark-price` — Binance's mark state comes from `premium_index_source` alone.
    mark_source: Option<&'static str>,
    /// An alternate source that feeds the SAME mark state, `Some` for Binance
    /// (the REST `premium-index` poll, F2/E50) and `None` for Bybit. Its rows are
    /// routed to `ingest_mark` exactly like a socket `mark-price` event.
    premium_index_source: Option<&'static str>,
    /// The funding-interval source, `Some` for Binance (`funding-info`) only.
    funding_info_source: Option<&'static str>,
    /// The open-interest source (`open-interest` on Binance, the ticker on Bybit).
    oi_source: &'static str,
    /// The liquidation source: `Some` for Bybit; `None` for Binance, which no
    /// longer produces `liquidation_pressure` (E62).
    liquidation_source: Option<&'static str>,
    /// The perp-trade source: `Some` for Bybit; `None` for Binance, which no
    /// longer produces `perp_cvd` (E62).
    trade_source: Option<&'static str>,
    liquidation: LiquidationShape,
    /// Whether the venue's liquidation side convention is confirmed (R8): `false`
    /// on Bybit until the observed-contract capture, so its member stays
    /// non-ready with `liquidation_side_unconfirmed`.
    liq_side_confirmed: bool,
    feed_completeness: &'static str,
    /// The perp trade base-size field (`quantity` on Binance, `size` on Bybit).
    trade_size_field: &'static str,
    /// Families this venue produces but that publish `unavailable` with
    /// `<venue>_recovery_required` (E34). Empty for both venues today: Bybit is
    /// fully observable, and Binance no longer produces the two socket families at
    /// all (E62) rather than producing them as unavailable.
    unavailable: &'static [DerivFamily],
    /// The derivatives sources a declared stream MUST produce for this venue's
    /// non-unavailable families to ever reach ready (F1 misconfiguration guard).
    /// It excludes the inputs read only by an `unavailable` family and — for
    /// Binance — the socket `mark-price`, which the Tokyo box never receives
    /// (E30): while it is unproduced the mark-fed families publish an honest
    /// non-ready rather than gating start-up (F2 supersedes the socket mark with
    /// the REST `premium-index`, which is then the required mark input).
    required_source_ids: &'static [&'static str],
    source_docs: &'static [(&'static str, &'static str)],
}

/// F2/E50: the REST `premiumIndex` poll carries the same mark/index/funding state
/// as the retired socket `mark-price` stream, over a plane the Tokyo box can
/// reach (the socket `market` connection could not — E30/E62). Since E62 deleted
/// the socket source, the premium-index poll is the sole Binance mark input.
const BINANCE_PREMIUM_INDEX_SOURCE: &str = "binance.usdm.btcusdt.premium-index";
const BINANCE_FUNDING_INFO_SOURCE: &str = "binance.usdm.btcusdt.funding-info";
const BINANCE_OI_SOURCE: &str = "binance.usdm.btcusdt.open-interest";

/// The twelve families Binance produces: the shared fourteen minus
/// `LiquidationPressure` and `PerpCvd`, whose only inputs were the `market`-socket
/// `liquidations` and `agg-trades` sources deleted by E62. Order matches
/// [`DerivFamily::ALL`] with the two removed.
const BINANCE_FAMILIES: &[DerivFamily] = &[
    DerivFamily::MarkIndexBasisState,
    DerivFamily::FundingState,
    DerivFamily::OpenInterestDelta,
    DerivFamily::PerpAtr,
    DerivFamily::PerpRsi,
    DerivFamily::PerpRoc,
    DerivFamily::PerpEmaCross,
    DerivFamily::PerpMacd,
    DerivFamily::PerpBollinger,
    DerivFamily::PerpZScore,
    DerivFamily::PerpMomentum,
    DerivFamily::PerpVolatility,
];

const BYBIT_TICKER_SOURCE: &str = "bybit.linear.btcusdt.tickers";
const BYBIT_LIQUIDATION_SOURCE: &str = "bybit.linear.btcusdt.liquidations";
const BYBIT_TRADE_SOURCE: &str = "bybit.linear.btcusdt.trades";

const BINANCE_SPEC: DerivSpec = DerivSpec {
    venue: Venue::Binance,
    token: "binance",
    families: BINANCE_FAMILIES,
    // E62: the socket `mark-price` source is gone; Binance's mark/index/funding
    // state is fed by the REST `premium-index` poll alone (`premium_index_source`).
    mark_source: None,
    premium_index_source: Some(BINANCE_PREMIUM_INDEX_SOURCE),
    funding_info_source: Some(BINANCE_FUNDING_INFO_SOURCE),
    oi_source: BINANCE_OI_SOURCE,
    // E62: Binance no longer produces `liquidation_pressure`/`perp_cvd`, so it has
    // no liquidation or trade source.
    liquidation_source: None,
    trade_source: None,
    // Vestigial for Binance (it produces no liquidation family); retained because
    // the struct field is shared with Bybit. The Normalized side and
    // average-fill basis describe the deleted `forceOrder` shape.
    liquidation: LiquidationShape {
        size_field: "filled_accumulated_quantity",
        price_field: "average_price",
        price_basis: "average_fill_price",
        side: LiquidationSideField::Normalized,
    },
    liq_side_confirmed: false,
    feed_completeness: "throttled",
    trade_size_field: "quantity",
    // E62: nothing is unavailable — the two socket families are not produced at
    // all rather than produced as unavailable, and the REST premium-index poll
    // makes mark/index/funding reachable from Tokyo (F2/E50).
    unavailable: &[],
    // The Binance families' required REST inputs: open-interest, funding-info, and
    // the premium-index poll — the sole mark input since E62 removed the socket
    // `mark-price`.
    required_source_ids: &[
        BINANCE_OI_SOURCE,
        BINANCE_FUNDING_INFO_SOURCE,
        BINANCE_PREMIUM_INDEX_SOURCE,
    ],
    source_docs: &[
        (
            BINANCE_PREMIUM_INDEX_SOURCE,
            include_str!(
                "../../../definitions/btc/sources/binance.usdm.btcusdt.premium-index@1.0.0.json"
            ),
        ),
        (
            BINANCE_FUNDING_INFO_SOURCE,
            include_str!(
                "../../../definitions/btc/sources/binance.usdm.btcusdt.funding-info@1.0.0.json"
            ),
        ),
        (
            BINANCE_OI_SOURCE,
            include_str!(
                "../../../definitions/btc/sources/binance.usdm.btcusdt.open-interest@1.0.0.json"
            ),
        ),
    ],
};

const BYBIT_SPEC: DerivSpec = DerivSpec {
    venue: Venue::Bybit,
    token: "bybit",
    families: &DerivFamily::ALL,
    mark_source: Some(BYBIT_TICKER_SOURCE),
    premium_index_source: None,
    funding_info_source: None,
    oi_source: BYBIT_TICKER_SOURCE,
    liquidation_source: Some(BYBIT_LIQUIDATION_SOURCE),
    trade_source: Some(BYBIT_TRADE_SOURCE),
    liquidation: LiquidationShape {
        size_field: "size",
        price_field: "bankruptcy_price",
        price_basis: "bankruptcy_price",
        side: LiquidationSideField::BybitSide,
    },
    // R8 (R1 step 2, 2026-09-04): Bybit v5 `allLiquidation` `S` is the POSITION
    // side — "When you receive a Buy update, this means that a long position has
    // been liquidated" — confirmed against the live capture (7 events, all Buy).
    // `liquidated_side` already maps buy -> long liquidated / sell -> short
    // liquidated (the position-side reading), so no inversion is needed; the side
    // gate is lifted and the family is ready with confirmed long/short skew.
    liq_side_confirmed: true,
    feed_completeness: "complete",
    trade_size_field: "size",
    unavailable: &[],
    // Bybit is fully observable: every family is available, so mark/OI (tickers),
    // liquidations and trades are all required.
    required_source_ids: &[
        BYBIT_TICKER_SOURCE,
        BYBIT_LIQUIDATION_SOURCE,
        BYBIT_TRADE_SOURCE,
    ],
    source_docs: &[
        (
            BYBIT_TICKER_SOURCE,
            include_str!(
                "../../../definitions/btc/sources/bybit.linear.btcusdt.tickers@1.0.0.json"
            ),
        ),
        (
            BYBIT_LIQUIDATION_SOURCE,
            include_str!(
                "../../../definitions/btc/sources/bybit.linear.btcusdt.liquidations@1.0.0.json"
            ),
        ),
        (
            BYBIT_TRADE_SOURCE,
            include_str!("../../../definitions/btc/sources/bybit.linear.btcusdt.trades@1.0.0.json"),
        ),
    ],
};

fn spec_for(venue: Venue) -> &'static DerivSpec {
    match venue {
        Venue::Binance => &BINANCE_SPEC,
        Venue::Bybit => &BYBIT_SPEC,
        other => unreachable!("no derivatives spec for {other:?}"),
    }
}

/// The signal id `btc.<venue>.<family>` for the contract embed and emitters.
fn signal_id(token: &str, family: DerivFamily) -> String {
    format!("btc.{token}.{}", family.slug())
}

/// The embedded signal contract documents `(id, json)` for every produced
/// identity — twelve on Binance, fourteen on Bybit (E62) — for the
/// definition-coverage guard (mirrors the spot siblings' equivalent).
#[must_use]
pub fn venue_derivatives_signal_contract_documents() -> Vec<(&'static str, &'static str)> {
    let mut documents = Vec::new();
    for venue in [Venue::Binance, Venue::Bybit] {
        for &family in spec_for(venue).families {
            documents.push((
                signal_id_static(venue, family),
                signal_definition_raw(venue, family),
            ));
        }
    }
    documents
}

/// The embedded signal definition json for `btc.<venue>.<family>`.
fn signal_definition_raw(venue: Venue, family: DerivFamily) -> &'static str {
    macro_rules! embed {
        ($token:literal, $slug:literal) => {
            include_str!(concat!(
                "../../../definitions/btc/signals/btc.",
                $token,
                ".",
                $slug,
                "@1.0.0.json"
            ))
        };
    }
    match (venue, family) {
        (Venue::Binance, DerivFamily::MarkIndexBasisState) => {
            embed!("binance", "mark_index_basis_state")
        }
        (Venue::Binance, DerivFamily::FundingState) => embed!("binance", "funding_state"),
        (Venue::Binance, DerivFamily::OpenInterestDelta) => {
            embed!("binance", "open_interest_delta")
        }
        // E62: Binance produces neither family (removed from `BINANCE_FAMILIES`),
        // so this is never reached and the definitions no longer exist.
        (Venue::Binance, DerivFamily::LiquidationPressure)
        | (Venue::Binance, DerivFamily::PerpCvd) => {
            unreachable!("Binance does not produce {family:?} (E62)")
        }
        (Venue::Bybit, DerivFamily::MarkIndexBasisState) => {
            embed!("bybit", "mark_index_basis_state")
        }
        (Venue::Bybit, DerivFamily::FundingState) => embed!("bybit", "funding_state"),
        (Venue::Bybit, DerivFamily::OpenInterestDelta) => embed!("bybit", "open_interest_delta"),
        (Venue::Bybit, DerivFamily::LiquidationPressure) => embed!("bybit", "liquidation_pressure"),
        (Venue::Bybit, DerivFamily::PerpCvd) => embed!("bybit", "perp_cvd"),
        (Venue::Binance, DerivFamily::PerpAtr) => embed!("binance", "perp_atr"),
        (Venue::Binance, DerivFamily::PerpRsi) => embed!("binance", "perp_rsi"),
        (Venue::Binance, DerivFamily::PerpRoc) => embed!("binance", "perp_roc"),
        (Venue::Binance, DerivFamily::PerpEmaCross) => embed!("binance", "perp_ema_cross"),
        (Venue::Binance, DerivFamily::PerpMacd) => embed!("binance", "perp_macd"),
        (Venue::Binance, DerivFamily::PerpBollinger) => embed!("binance", "perp_bollinger"),
        (Venue::Binance, DerivFamily::PerpZScore) => embed!("binance", "perp_zscore"),
        (Venue::Bybit, DerivFamily::PerpAtr) => embed!("bybit", "perp_atr"),
        (Venue::Bybit, DerivFamily::PerpRsi) => embed!("bybit", "perp_rsi"),
        (Venue::Bybit, DerivFamily::PerpRoc) => embed!("bybit", "perp_roc"),
        (Venue::Bybit, DerivFamily::PerpEmaCross) => embed!("bybit", "perp_ema_cross"),
        (Venue::Bybit, DerivFamily::PerpMacd) => embed!("bybit", "perp_macd"),
        (Venue::Bybit, DerivFamily::PerpBollinger) => embed!("bybit", "perp_bollinger"),
        (Venue::Bybit, DerivFamily::PerpZScore) => embed!("bybit", "perp_zscore"),
        (Venue::Binance, DerivFamily::PerpMomentum) => embed!("binance", "perp_momentum"),
        (Venue::Binance, DerivFamily::PerpVolatility) => embed!("binance", "perp_volatility"),
        (Venue::Bybit, DerivFamily::PerpMomentum) => embed!("bybit", "perp_momentum"),
        (Venue::Bybit, DerivFamily::PerpVolatility) => embed!("bybit", "perp_volatility"),
        (other, _) => unreachable!("no derivatives definitions for {other:?}"),
    }
}

// --- rolling state ---------------------------------------------------------

/// A per-input warm-up and liveness clock: `observed_since` is the logical time
/// of the first admitted input since the last reset, and `last_input_ns` the most
/// recent. A socket close / ordering reset clears both, re-entering warm-up.
#[derive(Debug, Clone, Copy, Default)]
struct Clock {
    observed_since: Option<u64>,
    last_input_ns: Option<u64>,
}

impl Clock {
    fn observe(&mut self, now_ns: u64) {
        self.observed_since.get_or_insert(now_ns);
        self.last_input_ns = Some(self.last_input_ns.map_or(now_ns, |prev| prev.max(now_ns)));
    }

    fn reset(&mut self) {
        *self = Self::default();
    }

    fn warmed(&self, now_ns: u64, warmup_ns: u64) -> bool {
        self.observed_since
            .is_some_and(|since| now_ns.saturating_sub(since) >= warmup_ns)
    }

    fn fresh(&self, now_ns: u64, bound_ns: u64) -> bool {
        self.last_input_ns
            .is_some_and(|input| now_ns.saturating_sub(input) <= bound_ns)
    }

    /// Whether the clock has never observed an input. F2/E50: a venue whose mark
    /// arrives over the REST premium-index fallback publishes an honest
    /// Unavailable until the first mark row, rather than a `warming_up`/`stale`
    /// that would never resolve if the plane is unreachable.
    fn never_observed(&self) -> bool {
        self.last_input_ns.is_none()
    }
}

/// The latest admitted mark/index/funding values, folded across the mark source.
#[derive(Debug, Clone, Default)]
struct MarkLatest {
    mark_price: Option<Decimal>,
    index_price: Option<Decimal>,
    funding_rate: Option<Decimal>,
    next_funding_time: Option<String>,
}

/// One liquidated position side after normalization (spec §4.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LiquidatedSide {
    Long,
    Short,
}

/// One retained liquidation event: its admission logical time, notional and the
/// liquidated position side.
#[derive(Debug, Clone, Copy)]
struct LiquidationEvent {
    receipt_ns: u64,
    notional: Decimal,
    side: LiquidatedSide,
}

/// One retained perp trade: its admission logical time, signed notional and
/// signed base size (buyer-aggressed positive, seller-aggressed negative).
#[derive(Debug, Clone, Copy)]
struct TradeEvent {
    receipt_ns: u64,
    signed_notional: Decimal,
    signed_base: Decimal,
    gross_notional: Decimal,
}

/// The bybit ticker ordering cursor: `(publisher_epoch, cross_sequence)`, seeded
/// by a snapshot. A delta before any snapshot, or one whose cross_sequence is not
/// strictly higher, is dropped (spec §3).
#[derive(Debug, Clone, Default)]
struct TickerCursor {
    seeded: Option<(String, u64)>,
}

// --- C2 perp momentum/volatility mark ring ---------------------------------

/// One one-second mark sample: its decision-clock logical time and the mark price.
#[derive(Debug, Clone, Copy)]
struct MarkSample {
    logical_time_ns: u64,
    price: Decimal,
}

/// The 10 s / 1 m / 5 m / 15 m causal simple basis-point returns of the mark.
#[derive(Debug, Clone, Copy)]
struct PerpMomentumMetrics {
    return_10s: Decimal,
    return_1m: Decimal,
    return_5m: Decimal,
    return_15m: Decimal,
}

/// The 10 s / 1 m / 5 m / 15 m realized-volatility metrics of the mark returns.
#[derive(Debug, Clone, Copy)]
struct PerpVolatilityMetrics {
    volatility_10s: Decimal,
    volatility_1m: Decimal,
    volatility_5m: Decimal,
    volatility_15m: Decimal,
    short_long_ratio: Decimal,
    regime: &'static str,
}

/// Lane-local one-second mark ring, cap [`MARK_RING_MAX_SAMPLES`], feeding the
/// perp momentum/volatility identities (spec §2/§3). Same continuity rule as
/// [`crate::realized_volatility::RealizedVolatility`]: a `None` observation, a
/// non-monotonic timestamp, or a gap of at least [`MISSED_OBSERVATION_NS`] clears
/// the ring and restarts the 901-sample warm-up (no gap bridging). Sampled once
/// per second off the decision clock from the merged mark state; not reused from
/// the spot [`crate::horizon::HorizonRing`], which carries only three horizons.
#[derive(Debug, Clone, Default)]
struct MarkRing {
    samples: VecDeque<MarkSample>,
}

impl MarkRing {
    fn new() -> Self {
        Self {
            samples: VecDeque::with_capacity(MARK_RING_MAX_SAMPLES),
        }
    }

    fn clear(&mut self) {
        self.samples.clear();
    }

    fn price_from_back(&self, offset: usize) -> Option<Decimal> {
        let index = self.samples.len().checked_sub(1 + offset)?;
        self.samples.get(index).map(|sample| sample.price)
    }

    /// Advance the ring from a one-second mark observation. `None` is a missing
    /// observation that breaks continuity and clears the ring.
    fn update_price(&mut self, price: Option<Decimal>, now_ns: u64) {
        let Some(price) = price else {
            self.samples.clear();
            return;
        };
        if self.samples.back().is_some_and(|sample| {
            now_ns <= sample.logical_time_ns
                || now_ns.saturating_sub(sample.logical_time_ns) >= MISSED_OBSERVATION_NS
        }) {
            self.samples.clear();
        }
        if self.samples.len() == MARK_RING_MAX_SAMPLES {
            self.samples.pop_front();
        }
        self.samples.push_back(MarkSample {
            logical_time_ns: now_ns,
            price,
        });
    }

    /// The 10 s / 1 m / 5 m / 15 m momentum, or `None` while the ring is still
    /// warming up to the longest horizon (fewer than [`MARK_RING_MAX_SAMPLES`]).
    fn momentum(&self) -> Result<Option<PerpMomentumMetrics>, SpotSignalError> {
        if self.samples.len() < MARK_RING_MAX_SAMPLES {
            return Ok(None);
        }
        let current = self
            .price_from_back(0)
            .ok_or_else(|| SpotSignalError::State("warm mark ring has no newest".to_owned()))?;
        let ten_second = self
            .price_from_back(MARK_OFFSET_10S)
            .ok_or_else(mark_missing)?;
        let one_minute = self
            .price_from_back(MARK_OFFSET_1M)
            .ok_or_else(mark_missing)?;
        let five_minute = self
            .price_from_back(MARK_OFFSET_5M)
            .ok_or_else(mark_missing)?;
        let fifteen_minute = self
            .price_from_back(MARK_OFFSET_15M)
            .ok_or_else(mark_missing)?;
        Ok(Some(PerpMomentumMetrics {
            return_10s: return_bps(current, ten_second)?,
            return_1m: return_bps(current, one_minute)?,
            return_5m: return_bps(current, five_minute)?,
            return_15m: return_bps(current, fifteen_minute)?,
        }))
    }

    /// The 10 s / 1 m / 5 m / 15 m realized volatility, or `None` during warm-up.
    fn volatility(&self) -> Result<Option<PerpVolatilityMetrics>, SpotSignalError> {
        if self.samples.len() < MARK_RING_MAX_SAMPLES {
            return Ok(None);
        }
        let mut returns = Vec::with_capacity(self.samples.len() - 1);
        for index in 1..self.samples.len() {
            returns.push(return_bps(
                self.samples[index].price,
                self.samples[index - 1].price,
            )?);
        }
        // The one-second returns inside (now − h, now]: the last h returns.
        let volatility_15m = root_mean_square(&returns[returns.len() - MARK_OFFSET_15M..])?;
        let volatility_5m = root_mean_square(&returns[returns.len() - MARK_OFFSET_5M..])?;
        let volatility_1m = root_mean_square(&returns[returns.len() - MARK_OFFSET_1M..])?;
        let volatility_10s = root_mean_square(&returns[returns.len() - MARK_OFFSET_10S..])?;
        // short / long = shortest (10 s) / longest (15 m) with the micro
        // convention: both zero → 1, regime stable (the 15 m window contains the
        // 10 s returns, so a zero 15 m RMS forces a zero 10 s RMS).
        let short_long_ratio = if volatility_15m.is_zero() {
            Decimal::ONE
        } else {
            checked_div(volatility_10s, volatility_15m)?
        };
        let regime = classify_volatility_ratio(short_long_ratio);
        Ok(Some(PerpVolatilityMetrics {
            volatility_10s,
            volatility_1m,
            volatility_5m,
            volatility_15m,
            short_long_ratio,
            regime,
        }))
    }
}

fn mark_missing() -> SpotSignalError {
    SpotSignalError::State("warm mark ring is missing a horizon sample".to_owned())
}

/// Projects the perp momentum into its `btc.<venue>.perp_momentum@1.0.0` value
/// map. `window_close` (event_time) and `quote_currency` are injected by `finish`.
fn perp_momentum_values(metrics: PerpMomentumMetrics) -> BTreeMap<String, Value> {
    BTreeMap::from([
        (
            "return_bps_10s".to_owned(),
            decimal_value(metrics.return_10s),
        ),
        ("return_bps_1m".to_owned(), decimal_value(metrics.return_1m)),
        ("return_bps_5m".to_owned(), decimal_value(metrics.return_5m)),
        (
            "return_bps_15m".to_owned(),
            decimal_value(metrics.return_15m),
        ),
        (
            "direction_5m".to_owned(),
            json!(direction_word(metrics.return_5m)),
        ),
    ])
}

/// Projects the perp volatility into its `btc.<venue>.perp_volatility@1.0.0` value
/// map. `window_close` (event_time) and `quote_currency` are injected by `finish`.
fn perp_volatility_values(metrics: PerpVolatilityMetrics) -> BTreeMap<String, Value> {
    BTreeMap::from([
        (
            "volatility_bps_10s".to_owned(),
            decimal_value(metrics.volatility_10s),
        ),
        (
            "volatility_bps_1m".to_owned(),
            decimal_value(metrics.volatility_1m),
        ),
        (
            "volatility_bps_5m".to_owned(),
            decimal_value(metrics.volatility_5m),
        ),
        (
            "volatility_bps_15m".to_owned(),
            decimal_value(metrics.volatility_15m),
        ),
        (
            "short_long_ratio".to_owned(),
            decimal_value(metrics.short_long_ratio),
        ),
        ("regime".to_owned(), json!(metrics.regime)),
    ])
}

// --- the engine ------------------------------------------------------------

/// The derivatives half of one venue lane. Owned by the venue processor beside
/// the spot [`VenueLane`](crate::spot_signals::VenueLane); drives the five
/// families' bounded histories, computations, emitters and readiness clocks.
pub struct VenueDerivativesEngine {
    spec: &'static DerivSpec,
    wall_start: DateTime<Utc>,
    publisher_epoch: String,
    source_definitions: BTreeMap<String, SourceDefinition>,
    emitters: BTreeMap<DerivFamily, FeatureEmitter>,

    // Rolling state.
    mark: MarkLatest,
    open_interest_base: Option<Decimal>,
    basis_history: VecDeque<(u64, Decimal)>,
    mark_history: VecDeque<(u64, Decimal)>,
    oi_history: VecDeque<(u64, Decimal)>,
    liquidations: VecDeque<LiquidationEvent>,
    trades: VecDeque<TradeEvent>,

    // Funding interval state.
    funding_info: Option<(u64, Decimal)>,
    funding_intervals_seen: BTreeSet<String>,
    observed_interval_hours: Option<Decimal>,

    // Ticker ordering (bybit).
    ticker_cursor: TickerCursor,

    // C2 (E49): the lane-local one-second mark ring feeding perp_momentum and
    // perp_volatility. One ring per venue engine, sampled off the decision clock.
    mark_ring: MarkRing,

    // Cross-source mark event-time ordering (F2/E50). The socket mark-price and
    // the REST premium-index feed the same mark state; on a host where both
    // deliver, a premium-index row whose venue event_time precedes the last
    // accepted mark must not overwrite fresher mark/funding fields. One cursor
    // spans both sources; a non-advancing row is dropped and counted, not an
    // error. `None` until the first mark row (or after a reset).
    last_mark_time_ns: Option<i64>,
    mark_out_of_order_drops: u64,

    /// The C3a technical-analysis unit over this venue's perpetual mark series
    /// (E51), sampled once per decision from the held mark price.
    ta: TaEngine,

    // C2 (E49) / E59: the logical decision-clock receipt time of the most recently
    // ingested mark row (both the socket mark-price and the REST premium-index set
    // it). The one-second perp sampler (mark ring and TA bars alike) takes the
    // latest mark while this receipt is younger than `PERP_MARK_HOLD_NS` (3 s), so
    // a sub-3 s poll cadence warms the identities and a longer stall or absent mark
    // is a missing sample that restarts the warm-up. `None` until the first mark
    // row (or after a reset).
    mark_row_ns: Option<u64>,

    // Readiness clocks.
    mark_clock: Clock,
    oi_clock: Clock,
    liquidation_clock: Clock,
    trade_clock: Clock,
    last_now_ns: Option<u64>,
}

/// The outcome of one family computation before the readiness gates apply.
enum Computed {
    Ready(BTreeMap<String, Value>),
    NotReady(&'static str),
}

impl VenueDerivativesEngine {
    /// Build the derivatives engine for a venue, publisher epoch and wall-start
    /// baseline. Errors if an embedded source definition is invalid.
    pub fn new(
        venue: Venue,
        publisher_epoch: impl Into<String>,
        wall_start: DateTime<Utc>,
    ) -> Result<Self, SpotSignalError> {
        let spec = spec_for(venue);
        let mut source_definitions = BTreeMap::new();
        for (id, raw) in spec.source_docs {
            let definition: SourceDefinition = serde_json::from_str(raw).map_err(|error| {
                SpotSignalError::Definition(format!("invalid {id} source JSON: {error}"))
            })?;
            definition.validate().map_err(|error| {
                SpotSignalError::Definition(format!("invalid {id} source: {error}"))
            })?;
            if definition.source_id != *id {
                return Err(SpotSignalError::Definition(format!(
                    "embedded source id {} does not match {id}",
                    definition.source_id
                )));
            }
            source_definitions.insert((*id).to_owned(), definition);
        }
        let mut emitters = BTreeMap::new();
        for &family in spec.families {
            let raw = signal_definition_raw(venue, family);
            let id = signal_id_static(venue, family);
            let emitter = FeatureEmitter::from_contract(raw, id, DERIV_SIGNAL_VERSION)?;
            emitters.insert(family, emitter);
        }
        Ok(Self {
            spec,
            wall_start,
            publisher_epoch: publisher_epoch.into(),
            source_definitions,
            emitters,
            mark: MarkLatest::default(),
            open_interest_base: None,
            basis_history: VecDeque::new(),
            mark_history: VecDeque::new(),
            oi_history: VecDeque::new(),
            liquidations: VecDeque::new(),
            trades: VecDeque::new(),
            funding_info: None,
            funding_intervals_seen: BTreeSet::new(),
            observed_interval_hours: None,
            ticker_cursor: TickerCursor::default(),
            mark_ring: MarkRing::new(),
            last_mark_time_ns: None,
            mark_out_of_order_drops: 0,
            ta: TaEngine::new()?,
            mark_row_ns: None,
            mark_clock: Clock::default(),
            oi_clock: Clock::default(),
            liquidation_clock: Clock::default(),
            trade_clock: Clock::default(),
            last_now_ns: None,
        })
    }

    /// The derivatives source definition documents `(id, json)`.
    pub fn source_documents(&self) -> Vec<(String, String)> {
        self.spec
            .source_docs
            .iter()
            .map(|(id, raw)| ((*id).to_owned(), (*raw).to_owned()))
            .collect()
    }

    /// This venue's produced derivatives signal definition documents `(id, json)`
    /// — twelve on Binance, fourteen on Bybit (E62).
    pub fn signal_documents(&self) -> Vec<(String, String)> {
        self.spec
            .families
            .iter()
            .map(|family| {
                (
                    signal_id(self.spec.token, *family),
                    signal_definition_raw(self.spec.venue, *family).to_owned(),
                )
            })
            .collect()
    }

    /// The feed contracts backing the feed-health board for the derivatives
    /// sources (spec §5: a `feed_health` frame per new source).
    pub fn feed_contracts(&self) -> Vec<FeedContract> {
        self.spec
            .source_docs
            .iter()
            .map(|(id, _)| {
                let definition = self
                    .source_definitions
                    .get(*id)
                    .expect("source definition is embedded");
                FeedContract {
                    source_id: (*id).to_owned(),
                    venue: self.spec.token.to_owned(),
                    expected_heartbeat_ms: definition.expected_heartbeat_ms,
                    cadence: FeedCadence::EventDriven,
                    liveness_bound_override_ms: None,
                }
            })
            .collect()
    }

    /// Whether `source_id` is one of this venue's derivatives sources, so the
    /// venue processor's dispatch routes it here.
    pub fn owns_source(&self, source_id: &str) -> bool {
        self.source_definitions.contains_key(source_id)
    }

    /// Every derivatives source this engine consumes (all embedded source docs),
    /// for the lane's `consumed_sources` set.
    pub fn consumed_sources(&self) -> impl Iterator<Item = &str> {
        self.source_definitions.keys().map(String::as_str)
    }

    /// The derivatives sources a declared stream must produce for this venue's
    /// non-unavailable families to reach ready (F1 misconfiguration guard).
    pub fn required_sources(&self) -> impl Iterator<Item = &'static str> {
        self.spec.required_source_ids.iter().copied()
    }

    /// How many signals this engine publishes: this venue's produced family count
    /// — twelve on Binance, fourteen on Bybit (E62).
    #[must_use]
    pub fn signal_count(&self) -> usize {
        self.spec.families.len()
    }

    /// Mark rows dropped for arriving out of venue event-time order across the
    /// socket mark-price and REST premium-index sources (F2/E50). A counted
    /// notice, never an error: a stale premium-index row is discarded, not
    /// applied over fresher mark/funding state.
    #[must_use]
    pub fn mark_out_of_order_drops(&self) -> u64 {
        self.mark_out_of_order_drops
    }

    fn validate_event(&self, event: &SourceEvent) -> Result<(), SpotSignalError> {
        let definition = self
            .source_definitions
            .get(&event.source_id)
            .ok_or_else(|| SpotSignalError::UnsupportedSource(event.source_id.clone()))?;
        event.validate_against(definition)?;
        Ok(())
    }

    /// Route one admitted derivatives event to its state, folding it into the
    /// bounded histories. A source the engine does not own is a hard error (the
    /// venue processor only routes owned sources here).
    pub fn ingest(&mut self, event: &SourceEvent, now_ns: u64) -> Result<(), SpotSignalError> {
        self.validate_event(event)?;
        let source = event.source_id.as_str();
        if Some(source) == self.spec.mark_source || Some(source) == self.spec.premium_index_source {
            // F2/E50: the REST premium-index row feeds the same mark state as a
            // socket mark-price event (Binance has only the former since E62).
            self.ingest_mark(event, now_ns)?;
        } else if source == self.spec.oi_source {
            // Bybit shares the ticker source between mark and OI; a ticker event
            // reaches `ingest_mark` first, which folds OI too. Only a distinct
            // Binance `open-interest` poll reaches here.
            if Some(source) != self.spec.mark_source {
                self.ingest_open_interest(event, now_ns)?;
            }
        } else if Some(source) == self.spec.funding_info_source {
            self.ingest_funding_info(event, now_ns)?;
        } else if Some(source) == self.spec.liquidation_source {
            self.ingest_liquidation(event, now_ns)?;
        } else if Some(source) == self.spec.trade_source {
            self.ingest_trade(event, now_ns)?;
        } else {
            return Err(SpotSignalError::UnsupportedSource(source.to_owned()));
        }
        Ok(())
    }

    fn ingest_mark(&mut self, event: &SourceEvent, now_ns: u64) -> Result<(), SpotSignalError> {
        // Bybit tickers carry a snapshot/delta cursor; a delta before a snapshot,
        // or a non-advancing cross_sequence, is dropped (spec §3).
        if self.spec.venue == Venue::Bybit && !self.accept_ticker(event)? {
            return Ok(());
        }
        // F2/E50: the socket mark-price and the REST premium-index feed one mark
        // state. Where both deliver (Binance), order them by venue event_time so a
        // premium-index row that arrives after but is timestamped before the last
        // accepted mark cannot overwrite fresher fields. Bybit has a single ticker
        // source ordered by cross_sequence above, so this gate is scoped to venues
        // with a premium-index sibling. Equal timestamps carry the same instant's
        // mark data, so `<=` drops the duplicate too.
        let event_time_ns = event_time_ns(event)?;
        if self.spec.premium_index_source.is_some() {
            if self
                .last_mark_time_ns
                .is_some_and(|last| event_time_ns <= last)
            {
                self.mark_out_of_order_drops = self.mark_out_of_order_drops.saturating_add(1);
                return Ok(());
            }
            self.last_mark_time_ns = Some(event_time_ns);
        }
        self.mark_clock.observe(now_ns);
        // A liquidation feed's liveness comes from the always-on mark/ticker
        // stream (E6), so a mark input proves the venue live for liquidations too.
        self.liquidation_clock.observe(now_ns);
        if let Some(mark) = optional_decimal(event, "mark_price")? {
            self.mark.mark_price = Some(mark);
            // E59: record this mark row's decision-clock receipt so the one-second
            // sampler can hold it for up to `PERP_MARK_HOLD_NS` and treat an older
            // row as a missing sample.
            self.mark_row_ns = Some(now_ns);
            push_bounded(&mut self.mark_history, now_ns, mark, OI_HISTORY_MAX)?;
        }
        if let Some(index) = optional_decimal(event, "index_price")? {
            self.mark.index_price = Some(index);
        }
        // The socket mark-price carries `funding_rate`; the REST premium-index
        // row carries the same value as `last_funding_rate` (F2/E50). Either feeds
        // the mark state's funding rate.
        if let Some(rate) = optional_decimal(event, "funding_rate")?
            .or(optional_decimal(event, "last_funding_rate")?)
        {
            self.mark.funding_rate = Some(rate);
        }
        if let Some(next) = optional_str(event, "next_funding_time") {
            if self.funding_intervals_seen.insert(next.clone())
                && self.funding_intervals_seen.len() >= 2
            {
                self.recompute_observed_interval();
            }
            self.mark.next_funding_time = Some(next);
        }
        // Basis history advances whenever both prices are present and positive.
        if let (Some(mark), Some(index)) = (self.mark.mark_price, self.mark.index_price)
            && index > Decimal::ZERO
        {
            let basis = basis_bps(mark, index)?;
            push_bounded(&mut self.basis_history, now_ns, basis, BASIS_HISTORY_MAX)?;
        }
        // Bybit folds open interest from the same ticker event.
        if Some(self.spec.oi_source) == self.spec.mark_source
            && let Some(oi) = optional_decimal(event, "open_interest_base")?
        {
            self.oi_clock.observe(now_ns);
            self.open_interest_base = Some(oi);
            push_bounded(&mut self.oi_history, now_ns, oi, OI_HISTORY_MAX)?;
        }
        self.prune(now_ns);
        Ok(())
    }

    fn ingest_open_interest(
        &mut self,
        event: &SourceEvent,
        now_ns: u64,
    ) -> Result<(), SpotSignalError> {
        self.oi_clock.observe(now_ns);
        let oi = decimal_field(event, "open_interest_base")?;
        self.open_interest_base = Some(oi);
        push_bounded(&mut self.oi_history, now_ns, oi, OI_HISTORY_MAX)?;
        self.prune(now_ns);
        Ok(())
    }

    fn ingest_funding_info(
        &mut self,
        event: &SourceEvent,
        now_ns: u64,
    ) -> Result<(), SpotSignalError> {
        let hours = u32_field(event, "funding_interval_hours")?;
        self.funding_info = Some((now_ns, Decimal::from(hours)));
        Ok(())
    }

    fn ingest_liquidation(
        &mut self,
        event: &SourceEvent,
        now_ns: u64,
    ) -> Result<(), SpotSignalError> {
        self.liquidation_clock.observe(now_ns);
        let size = decimal_field(event, self.spec.liquidation.size_field)?;
        let price = decimal_field(event, self.spec.liquidation.price_field)?;
        let notional = checked_mul(size, price)?;
        let side = self.liquidated_side(event)?;
        self.liquidations.push_back(LiquidationEvent {
            receipt_ns: now_ns,
            notional,
            side,
        });
        self.prune(now_ns);
        if self.liquidations.len() > LIQUIDATION_HISTORY_MAX {
            return Err(SpotSignalError::ResourceLimit(format!(
                "{} liquidation history exceeded {LIQUIDATION_HISTORY_MAX} entries",
                self.spec.token
            )));
        }
        Ok(())
    }

    fn ingest_trade(&mut self, event: &SourceEvent, now_ns: u64) -> Result<(), SpotSignalError> {
        self.trade_clock.observe(now_ns);
        let price = decimal_field(event, "price")?;
        let size = decimal_field(event, self.spec.trade_size_field)?;
        let aggressor = str_field(event, "aggressor_side")?;
        let notional = checked_mul(price, size)?;
        let (signed_notional, signed_base) = match aggressor {
            "buy" => (notional, size),
            "sell" => (
                checked_sub(Decimal::ZERO, notional)?,
                checked_sub(Decimal::ZERO, size)?,
            ),
            other => {
                return Err(SpotSignalError::MarketValue(format!(
                    "unknown aggressor_side {other}"
                )));
            }
        };
        self.trades.push_back(TradeEvent {
            receipt_ns: now_ns,
            signed_notional,
            signed_base,
            gross_notional: notional,
        });
        self.prune(now_ns);
        if self.trades.len() > TRADE_HISTORY_MAX {
            return Err(SpotSignalError::ResourceLimit(format!(
                "{} trade history exceeded {TRADE_HISTORY_MAX} entries",
                self.spec.token
            )));
        }
        Ok(())
    }

    /// Normalize a liquidation event's side onto the shared liquidated-position
    /// concept (spec §4.4). Never keyed on before the venue's convention is
    /// confirmed — that gate lives in [`Self::emit_liquidation`].
    fn liquidated_side(&self, event: &SourceEvent) -> Result<LiquidatedSide, SpotSignalError> {
        match self.spec.liquidation.side {
            LiquidationSideField::Normalized => match str_field(event, "liquidated_position_side")?
            {
                "long" => Ok(LiquidatedSide::Long),
                "short" => Ok(LiquidatedSide::Short),
                other => Err(SpotSignalError::MarketValue(format!(
                    "unknown liquidated_position_side {other}"
                ))),
            },
            // Bybit: `buy` = a long was liquidated (documented, unconfirmed R8).
            LiquidationSideField::BybitSide => match str_field(event, "side")? {
                "buy" => Ok(LiquidatedSide::Long),
                "sell" => Ok(LiquidatedSide::Short),
                other => Err(SpotSignalError::MarketValue(format!(
                    "unknown liquidation side {other}"
                ))),
            },
        }
    }

    /// Apply the bybit ticker ordering cursor. Returns whether the event is
    /// applied.
    fn accept_ticker(&mut self, event: &SourceEvent) -> Result<bool, SpotSignalError> {
        let cross = u64_field(event, "cross_sequence")?;
        let kind = str_field(event, "update_kind")?;
        match kind {
            "snapshot" => {
                self.ticker_cursor.seeded = Some((event.publisher_epoch.clone(), cross));
                Ok(true)
            }
            "delta" => match &self.ticker_cursor.seeded {
                // A delta before any snapshot is not applied (AwaitingSnapshot).
                None => Ok(false),
                Some((epoch, _)) if epoch != &event.publisher_epoch => {
                    Err(SpotSignalError::Ordering(
                        "ticker publisher epoch changed inside one signal process".to_owned(),
                    ))
                }
                // cross_sequence is not gapless: strictly higher is applied,
                // equal is a duplicate, lower is out of order (both dropped).
                Some((_, sequence)) if cross > *sequence => {
                    self.ticker_cursor.seeded = Some((event.publisher_epoch.clone(), cross));
                    Ok(true)
                }
                Some(_) => Ok(false),
            },
            other => Err(SpotSignalError::MarketValue(format!(
                "unknown ticker update_kind {other}"
            ))),
        }
    }

    fn recompute_observed_interval(&mut self) {
        // The two smallest distinct next_funding_time values give the interval.
        let mut times: Vec<i64> = self
            .funding_intervals_seen
            .iter()
            .filter_map(|text| {
                DateTime::parse_from_rfc3339(text)
                    .ok()
                    .map(|dt| dt.timestamp_millis())
            })
            .collect();
        times.sort_unstable();
        if let (Some(first), Some(second)) = (times.first(), times.get(1)) {
            let diff_ms = second.saturating_sub(*first);
            if diff_ms > 0 {
                // hours = diff_ms / 3_600_000, at 8 dp.
                if let Some(hours) = Decimal::from(diff_ms).checked_div(Decimal::from(3_600_000)) {
                    self.observed_interval_hours = Some(hours);
                }
            }
        }
    }

    fn prune(&mut self, now_ns: u64) {
        prune_before(
            &mut self.basis_history,
            now_ns.saturating_sub(SIXTY_SECONDS_NS + ONE_SECOND_NS),
        );
        prune_before(
            &mut self.mark_history,
            now_ns.saturating_sub(FIFTEEN_MINUTES_NS + ONE_SECOND_NS),
        );
        prune_before(
            &mut self.oi_history,
            now_ns.saturating_sub(FIFTEEN_MINUTES_NS + ONE_SECOND_NS),
        );
        let liq_cutoff = now_ns.saturating_sub(FIVE_MINUTES_NS + ONE_SECOND_NS);
        while self
            .liquidations
            .front()
            .is_some_and(|e| e.receipt_ns < liq_cutoff)
        {
            self.liquidations.pop_front();
        }
        let trade_cutoff = now_ns.saturating_sub(SIXTY_SECONDS_NS + ONE_SECOND_NS);
        while self
            .trades
            .front()
            .is_some_and(|e| e.receipt_ns < trade_cutoff)
        {
            self.trades.pop_front();
        }
    }

    /// Reset the rolling state and clocks on a socket close / ordering reset, so
    /// the families re-enter warm-up (`<venue>_warming_up`), matching the spot
    /// lane's causal-break handling.
    pub fn reset(&mut self) {
        self.mark = MarkLatest::default();
        self.open_interest_base = None;
        self.basis_history.clear();
        self.mark_history.clear();
        self.oi_history.clear();
        self.liquidations.clear();
        self.trades.clear();
        self.funding_info = None;
        self.funding_intervals_seen.clear();
        self.observed_interval_hours = None;
        self.ticker_cursor = TickerCursor::default();
        self.ta.clear();
        self.mark_ring.clear();
        self.last_mark_time_ns = None;
        self.mark_out_of_order_drops = 0;
        self.mark_row_ns = None;
        self.mark_clock.reset();
        self.oi_clock.reset();
        self.liquidation_clock.reset();
        self.trade_clock.reset();
    }

    /// E59: the one-second perp sample for this decision. It is the venue's latest
    /// mark price when its row was received within `PERP_MARK_HOLD_NS` (3 s hold
    /// bound); a mark row 3 s or more old — a stalled poll or an absent mark — is a
    /// missing sample (`None`) that breaks the mark ring and the TA bars alike.
    /// One rule, one constant for both C2 and C3a.
    fn held_mark_sample(&self, now_ns: u64) -> Option<Decimal> {
        let price = self.mark.mark_price?;
        let row_ns = self.mark_row_ns?;
        (now_ns.saturating_sub(row_ns) < PERP_MARK_HOLD_NS).then_some(price)
    }

    /// Emit one decision's worth of derivatives frames — one per family, in
    /// publication order. `produced_at` is the decision timestamp text; it is the
    /// `observation_time` / `window_close` of every frame.
    pub fn emit(
        &mut self,
        now_ns: u64,
        reference: UsdtReferenceSnapshot,
    ) -> Result<Vec<SignalUpdate>, SpotSignalError> {
        if self.last_now_ns.is_some_and(|prev| now_ns < prev) {
            return Err(SpotSignalError::TimeRegression);
        }
        self.last_now_ns = Some(now_ns);
        // C2 (E49) / E59: sample the merged mark once per second off the decision
        // clock into the four-horizon ring feeding perp_momentum/perp_volatility.
        // A decision second takes the latest mark row when that row is younger than
        // `PERP_MARK_HOLD_NS` (3 s hold bound); an older row — an absent mark, or a
        // poll stall past the hold — is a MISSING observation (feeds None), which
        // breaks continuity and restarts the 901-sample warm-up (spec §3), exactly
        // like the spot rings. The hold admits the sub-1 Hz premium-index cadence
        // (~1.27 s median) and the gappy Bybit mark; a stall longer than the hold
        // still re-warms, so a held mark is repeated for at most two seconds and
        // never deflates the realized volatility across a real gap.
        let mark_sample = self.held_mark_sample(now_ns);
        self.mark_ring.update_price(mark_sample, now_ns);
        let produced_at = logical_timestamp_text(self.wall_start, now_ns)?;
        // C3a / E59: the TA unit's one-second sample follows the same one rule —
        // the latest mark held for up to 3 s, else a missing sample. Passing `None`
        // breaks TA continuity exactly as a missing midpoint does on the spot lane,
        // so a bar spanning a stall is invalid and the `perp_*` identities re-warm.
        let mark_sample = self.held_mark_sample(now_ns);
        self.ta.update(mark_sample, now_ns)?;
        let mut updates = Vec::with_capacity(self.signal_count());
        for &family in self.spec.families {
            let computed = self.compute(family, now_ns, &produced_at)?;
            updates.push(self.finish(family, &produced_at, computed, reference)?);
        }
        Ok(updates)
    }

    /// Apply the readiness gates and the E34 unavailability to one computed
    /// family, then emit through its [`FeatureEmitter`].
    fn finish(
        &mut self,
        family: DerivFamily,
        produced_at: &str,
        computed: Computed,
        reference: UsdtReferenceSnapshot,
    ) -> Result<SignalUpdate, SpotSignalError> {
        let (mut ready_values, mut reasons) = if self.spec.unavailable.contains(&family) {
            // E34: built to spec, published unavailable until the market family
            // is observable from a serving region.
            (None, vec![recovery_reason(self.spec.venue).to_owned()])
        } else {
            match computed {
                Computed::Ready(mut values) => {
                    values.insert("quote_currency".to_owned(), json!(QUOTE_CURRENCY));
                    values.insert(event_time_field(family).to_owned(), json!(produced_at));
                    (Some(values), Vec::new())
                }
                Computed::NotReady(reason) => (None, vec![reason.to_owned()]),
            }
        };
        // E47: convert this family's quote-denominated fields USDT→USD, or force
        // `usdt_reference_stale` when the reference is absent/stale.
        apply_usdt_conversion(
            Some(reference),
            deriv_quote_fields(family),
            &mut ready_values,
            &mut reasons,
        )?;
        let emitter = self
            .emitters
            .get_mut(&family)
            .ok_or_else(|| SpotSignalError::State(format!("missing emitter {family:?}")))?;
        emitter.emit(
            &self.publisher_epoch,
            produced_at,
            produced_at.to_owned(),
            ready_values,
            reasons,
        )
    }

    fn compute(
        &self,
        family: DerivFamily,
        now_ns: u64,
        produced_at: &str,
    ) -> Result<Computed, SpotSignalError> {
        if let Some(ta_kind) = family.ta_kind() {
            return self.compute_ta(ta_kind, now_ns, produced_at);
        }
        match family {
            DerivFamily::MarkIndexBasisState => self.compute_basis(now_ns),
            DerivFamily::FundingState => self.compute_funding(now_ns),
            DerivFamily::OpenInterestDelta => self.compute_open_interest(now_ns),
            DerivFamily::LiquidationPressure => self.compute_liquidation(now_ns),
            DerivFamily::PerpCvd => self.compute_perp_cvd(now_ns),
            // Every TA family took the `ta_kind()` branch above; they are
            // enumerated rather than left to a wildcard so a family added to
            // `DerivFamily` without a compute path fails the build here instead
            // of silently publishing a permanent `<venue>_warming_up`.
            DerivFamily::PerpAtr
            | DerivFamily::PerpRsi
            | DerivFamily::PerpRoc
            | DerivFamily::PerpEmaCross
            | DerivFamily::PerpMacd
            | DerivFamily::PerpBollinger
            | DerivFamily::PerpZScore => Ok(Computed::NotReady(warming_reason(self.spec.venue))),
            DerivFamily::PerpMomentum => self.compute_perp_momentum(now_ns),
            DerivFamily::PerpVolatility => self.compute_perp_volatility(now_ns),
        }
    }

    // --- C2 perp_momentum / perp_volatility (E49) -------------------------

    /// The mark-missing readiness gate the perp momentum/volatility identities
    /// share, mirroring the mark-fed families (`compute_basis`): a REST-mark-fed
    /// venue (Binance premium-index) that has seen no mark yet publishes the E34
    /// recovery reason; a stale merged mark publishes the venue stale reason. No
    /// new reason is introduced (builder brief; E34/E50); the ring itself reports
    /// warm-up. `usdt_reference_stale` never applies — these two are reference-free.
    fn perp_mark_not_ready(&self, now_ns: u64) -> Option<&'static str> {
        if self.spec.premium_index_source.is_some() && self.mark_clock.never_observed() {
            return Some(recovery_reason(self.spec.venue));
        }
        if !self
            .mark_clock
            .fresh(now_ns, staleness_ns(DerivFamily::PerpMomentum))
        {
            return Some(stale_reason(self.spec.venue));
        }
        None
    }

    fn compute_perp_momentum(&self, now_ns: u64) -> Result<Computed, SpotSignalError> {
        if let Some(reason) = self.perp_mark_not_ready(now_ns) {
            return Ok(Computed::NotReady(reason));
        }
        match self.mark_ring.momentum()? {
            Some(metrics) => Ok(Computed::Ready(perp_momentum_values(metrics))),
            None => Ok(Computed::NotReady(warming_reason(self.spec.venue))),
        }
    }

    fn compute_perp_volatility(&self, now_ns: u64) -> Result<Computed, SpotSignalError> {
        if let Some(reason) = self.perp_mark_not_ready(now_ns) {
            return Ok(Computed::NotReady(reason));
        }
        match self.mark_ring.volatility()? {
            Some(metrics) => Ok(Computed::Ready(perp_volatility_values(metrics))),
            None => Ok(Computed::NotReady(warming_reason(self.spec.venue))),
        }
    }

    // --- C3a technical-analysis pack (E51) ---------------------------------

    /// One TA family's outcome on this decision.
    ///
    /// The readiness gates are the mark series' own: the same
    /// `<venue>_recovery_required` a REST-mark-fed venue publishes before its
    /// first mark row, then `<venue>_stale` outside the mark staleness bound,
    /// then the unit's own warm-up (which the stale gate has already restarted,
    /// so the two never disagree).
    fn compute_ta(
        &self,
        ta_kind: TaKind,
        now_ns: u64,
        produced_at: &str,
    ) -> Result<Computed, SpotSignalError> {
        if self.spec.premium_index_source.is_some() && self.mark_clock.never_observed() {
            return Ok(Computed::NotReady(recovery_reason(self.spec.venue)));
        }
        if !self
            .mark_clock
            .fresh(now_ns, staleness_ns(DerivFamily::MarkIndexBasisState))
        {
            return Ok(Computed::NotReady(stale_reason(self.spec.venue)));
        }
        Ok(
            match self.ta.outcome(ta_kind, produced_at, self.wall_start)? {
                TaOutcome::Ready(values) => Computed::Ready(values),
                TaOutcome::Warming => Computed::NotReady(warming_reason(self.spec.venue)),
                TaOutcome::FlatWindow => Computed::NotReady(FLAT_WINDOW_REASON),
            },
        )
    }

    // --- §4.1 mark_index_basis_state --------------------------------------

    fn compute_basis(&self, now_ns: u64) -> Result<Computed, SpotSignalError> {
        // F2/E50: for a REST-mark-fed venue (Binance premium-index), publish an
        // honest Unavailable (`<venue>_recovery_required`) until the first mark
        // row arrives, not a misleading warming_up/stale that never resolves if
        // the mark plane is unreachable (E34). A live-mark venue (Bybit tickers)
        // warms up normally.
        if self.spec.premium_index_source.is_some() && self.mark_clock.never_observed() {
            return Ok(Computed::NotReady(recovery_reason(self.spec.venue)));
        }
        if !self
            .mark_clock
            .fresh(now_ns, staleness_ns(DerivFamily::MarkIndexBasisState))
        {
            return Ok(Computed::NotReady(stale_reason(self.spec.venue)));
        }
        if !self.mark_clock.warmed(now_ns, WARMUP_60S_NS) {
            return Ok(Computed::NotReady(warming_reason(self.spec.venue)));
        }
        let (Some(mark), Some(index)) = (self.mark.mark_price, self.mark.index_price) else {
            return Ok(Computed::NotReady(warming_reason(self.spec.venue)));
        };
        if index <= Decimal::ZERO {
            return Ok(Computed::NotReady(warming_reason(self.spec.venue)));
        }
        let basis = basis_bps(mark, index)?;
        let (Some(basis_10s), Some(basis_60s)) = (
            reading_at_or_before(&self.basis_history, now_ns.saturating_sub(TEN_SECONDS_NS)),
            reading_at_or_before(&self.basis_history, now_ns.saturating_sub(SIXTY_SECONDS_NS)),
        ) else {
            return Ok(Computed::NotReady(warming_reason(self.spec.venue)));
        };
        let change_10s = checked_sub(basis, basis_10s)?;
        let change_60s = checked_sub(basis, basis_60s)?;
        let neutral_band = Decimal::from(2);
        let stress_change = Decimal::from(20);
        let regime = if basis > neutral_band {
            "premium"
        } else if basis < checked_sub(Decimal::ZERO, neutral_band)? {
            "discount"
        } else {
            "neutral"
        };
        let basis_stress = change_60s.abs() >= stress_change;
        let values = BTreeMap::from([
            ("mark_price".to_owned(), decimal_value(mark)),
            ("index_price".to_owned(), decimal_value(index)),
            ("basis_bps".to_owned(), decimal_value(basis)),
            ("basis_bps_change_10s".to_owned(), decimal_value(change_10s)),
            ("basis_bps_change_60s".to_owned(), decimal_value(change_60s)),
            ("regime".to_owned(), json!(regime)),
            ("neutral_band_bps".to_owned(), decimal_value(neutral_band)),
            ("stress_change_bps".to_owned(), decimal_value(stress_change)),
            ("basis_stress".to_owned(), json!(basis_stress)),
        ]);
        Ok(Computed::Ready(values))
    }

    // --- §4.2 funding_state -----------------------------------------------

    fn compute_funding(&self, now_ns: u64) -> Result<Computed, SpotSignalError> {
        // F2/E50: for a REST-mark-fed venue (Binance premium-index), publish an
        // honest Unavailable (`<venue>_recovery_required`) until the first mark
        // row arrives, not a misleading warming_up/stale that never resolves if
        // the mark plane is unreachable (E34). A live-mark venue (Bybit tickers)
        // warms up normally.
        if self.spec.premium_index_source.is_some() && self.mark_clock.never_observed() {
            return Ok(Computed::NotReady(recovery_reason(self.spec.venue)));
        }
        if !self
            .mark_clock
            .fresh(now_ns, staleness_ns(DerivFamily::FundingState))
        {
            return Ok(Computed::NotReady(stale_reason(self.spec.venue)));
        }
        if !self.mark_clock.warmed(now_ns, WARMUP_60S_NS) {
            return Ok(Computed::NotReady(warming_reason(self.spec.venue)));
        }
        let (Some(rate), Some(next_text)) =
            (self.mark.funding_rate, self.mark.next_funding_time.clone())
        else {
            return Ok(Computed::NotReady(warming_reason(self.spec.venue)));
        };
        let configured_hours = Decimal::from(8);
        // Interval basis: Binance venue_declared from a fresh funding-info
        // reading; Bybit observed from two distinct next_funding_time values.
        let (interval_hours, interval_basis) = match self.spec.venue {
            Venue::Binance => match self.funding_info {
                Some((seen_ns, hours))
                    if now_ns.saturating_sub(seen_ns) <= FUNDING_INFO_MAX_AGE_NS =>
                {
                    (hours, "venue_declared")
                }
                // Before or past a fresh reading: configured, degrade as
                // funding_info_missing (spec §4.2).
                _ => return Ok(Computed::NotReady("funding_info_missing")),
            },
            Venue::Bybit => match self.observed_interval_hours {
                Some(hours) => (hours, "observed"),
                None => (configured_hours, "configured"),
            },
            other => unreachable!("no funding for {other:?}"),
        };
        // A configured 8 h contradicted by an observed/declared interval by more
        // than 60 s degrades rather than re-scaling the annualisation (§4.2).
        if interval_basis != "configured" {
            let delta_hours = checked_sub(interval_hours, configured_hours)?.abs();
            let tolerance_hours = checked_div(Decimal::from(60), Decimal::from(3_600))?;
            if delta_hours > tolerance_hours {
                return Ok(Computed::NotReady("funding_interval_mismatch"));
            }
        }
        if interval_hours <= Decimal::ZERO {
            return Ok(Computed::NotReady("funding_interval_mismatch"));
        }
        let next = DateTime::parse_from_rfc3339(&next_text).map_err(|error| {
            SpotSignalError::MarketValue(format!("invalid next_funding_time: {error}"))
        })?;
        let observation = logical_timestamp(self.wall_start, now_ns)?;
        let to_funding_ms = next.timestamp_millis() - observation.timestamp_millis();
        // next_funding_time more than one interval in the past is a stalled clock.
        let interval_ms = checked_mul(interval_hours, Decimal::from(3_600_000))?;
        if Decimal::from(-to_funding_ms) > interval_ms {
            return Ok(Computed::NotReady(stale_reason(self.spec.venue)));
        }
        let time_to_funding_ms = u64::try_from(to_funding_ms.max(0)).unwrap_or(0);
        let rate_bps = checked_mul(rate, Decimal::from(10_000))?;
        let annualised = checked_mul(rate_bps, checked_div(Decimal::from(8_760), interval_hours)?)?;
        let flat_band = Decimal::new(1, 1); // 0.1
        let sign_regime = if rate_bps > flat_band {
            "longs_pay"
        } else if rate_bps < checked_sub(Decimal::ZERO, flat_band)? {
            "shorts_pay"
        } else {
            "flat"
        };
        let values = BTreeMap::from([
            ("funding_rate".to_owned(), decimal_value(rate)),
            ("funding_rate_bps".to_owned(), decimal_value(rate_bps)),
            (
                "funding_interval_hours".to_owned(),
                decimal_value(interval_hours),
            ),
            ("interval_basis".to_owned(), json!(interval_basis)),
            ("annualised_rate_bps".to_owned(), decimal_value(annualised)),
            ("next_funding_time".to_owned(), json!(next_text)),
            ("time_to_funding_ms".to_owned(), json!(time_to_funding_ms)),
            ("sign_regime".to_owned(), json!(sign_regime)),
            ("flat_band_bps".to_owned(), decimal_value(flat_band)),
        ]);
        Ok(Computed::Ready(values))
    }

    // --- §4.3 open_interest_delta -----------------------------------------

    fn compute_open_interest(&self, now_ns: u64) -> Result<Computed, SpotSignalError> {
        // F2/E50: for a REST-mark-fed venue (Binance premium-index), publish an
        // honest Unavailable (`<venue>_recovery_required`) until the first mark
        // row arrives, not a misleading warming_up/stale that never resolves if
        // the mark plane is unreachable (E34). A live-mark venue (Bybit tickers)
        // warms up normally.
        if self.spec.premium_index_source.is_some() && self.mark_clock.never_observed() {
            return Ok(Computed::NotReady(recovery_reason(self.spec.venue)));
        }
        if !self
            .oi_clock
            .fresh(now_ns, staleness_ns(DerivFamily::OpenInterestDelta))
        {
            return Ok(Computed::NotReady(stale_reason(self.spec.venue)));
        }
        let (Some(oi), Some(mark)) = (self.open_interest_base, self.mark.mark_price) else {
            return Ok(Computed::NotReady("open_interest_history_warming_up"));
        };
        if !self.oi_clock.warmed(now_ns, WARMUP_900S_NS) {
            return Ok(Computed::NotReady("open_interest_history_warming_up"));
        }
        // delta_base_15m requires a window start (a reading at or before now-900s).
        let (Some(oi_1m), Some(oi_5m), Some(oi_15m)) = (
            reading_at_or_before(&self.oi_history, now_ns.saturating_sub(SIXTY_SECONDS_NS)),
            reading_at_or_before(&self.oi_history, now_ns.saturating_sub(FIVE_MINUTES_NS)),
            reading_at_or_before(&self.oi_history, now_ns.saturating_sub(FIFTEEN_MINUTES_NS)),
        ) else {
            return Ok(Computed::NotReady("open_interest_history_warming_up"));
        };
        let Some(mark_5m) =
            reading_at_or_before(&self.mark_history, now_ns.saturating_sub(FIVE_MINUTES_NS))
        else {
            return Ok(Computed::NotReady("open_interest_history_warming_up"));
        };
        let notional = checked_mul(oi, mark)?;
        let delta_1m = checked_sub(oi, oi_1m)?;
        let delta_5m = checked_sub(oi, oi_5m)?;
        let delta_15m = checked_sub(oi, oi_15m)?;
        let pct_1m = delta_pct(delta_1m, oi_1m)?;
        let pct_5m = delta_pct(delta_5m, oi_5m)?;
        let pct_15m = delta_pct(delta_15m, oi_15m)?;
        let price_return_5m = if mark_5m > Decimal::ZERO {
            checked_mul(
                checked_sub(checked_div(mark, mark_5m)?, Decimal::ONE)?,
                Decimal::from(10_000),
            )?
        } else {
            Decimal::ZERO
        };
        let oi_band = Decimal::new(1, 1); // 0.1
        let price_band = Decimal::from(5);
        let quadrant = if pct_5m.abs() < oi_band || price_return_5m.abs() < price_band {
            "flat"
        } else {
            match (pct_5m > Decimal::ZERO, price_return_5m > Decimal::ZERO) {
                (true, true) => "oi_up_price_up",
                (true, false) => "oi_up_price_down",
                (false, true) => "oi_down_price_up",
                (false, false) => "oi_down_price_down",
            }
        };
        let divergence = quadrant == "oi_up_price_down" || quadrant == "oi_down_price_up";
        let values = BTreeMap::from([
            ("open_interest_base".to_owned(), decimal_value(oi)),
            (
                "open_interest_notional_usd".to_owned(),
                decimal_value(notional),
            ),
            ("mark_price".to_owned(), decimal_value(mark)),
            ("delta_base_1m".to_owned(), decimal_value(delta_1m)),
            ("delta_base_5m".to_owned(), decimal_value(delta_5m)),
            ("delta_base_15m".to_owned(), decimal_value(delta_15m)),
            ("delta_pct_1m".to_owned(), decimal_value(pct_1m)),
            ("delta_pct_5m".to_owned(), decimal_value(pct_5m)),
            ("delta_pct_15m".to_owned(), decimal_value(pct_15m)),
            (
                "price_return_bps_5m".to_owned(),
                decimal_value(price_return_5m),
            ),
            ("quadrant".to_owned(), json!(quadrant)),
            ("divergence".to_owned(), json!(divergence)),
            ("oi_band_pct".to_owned(), decimal_value(oi_band)),
            ("price_band_bps".to_owned(), decimal_value(price_band)),
        ]);
        Ok(Computed::Ready(values))
    }

    // --- §4.4 liquidation_pressure ----------------------------------------

    fn compute_liquidation(&self, now_ns: u64) -> Result<Computed, SpotSignalError> {
        // A venue whose liquidated-position side convention is still unconfirmed
        // (R8) is non-ready. Bybit is confirmed (R1 step 2, 2026-09-04); Binance's
        // forceOrder liquidation is `unavailable` from Tokyo, so it never reaches
        // this gate regardless of its flag.
        if !self.spec.liq_side_confirmed {
            return Ok(Computed::NotReady("liquidation_side_unconfirmed"));
        }
        // Liveness comes from the connection, never from events (E35): a live
        // venue with zero liquidations is ready with truthful zeros.
        if !self
            .liquidation_clock
            .fresh(now_ns, staleness_ns(DerivFamily::LiquidationPressure))
        {
            return Ok(Computed::NotReady(stale_reason(self.spec.venue)));
        }
        if !self.liquidation_clock.warmed(now_ns, WARMUP_300S_NS) {
            return Ok(Computed::NotReady(warming_reason(self.spec.venue)));
        }
        let values = liquidation_values(&self.liquidations, now_ns, self.spec)?;
        Ok(Computed::Ready(values))
    }

    // --- §4.5 perp_cvd -----------------------------------------------------

    fn compute_perp_cvd(&self, now_ns: u64) -> Result<Computed, SpotSignalError> {
        // A trade-quiet but live connection contributes truthful zeros; nothing
        // admitted within the bound degrades (spec §4.5).
        if !self
            .trade_clock
            .fresh(now_ns, staleness_ns(DerivFamily::PerpCvd))
        {
            return Ok(Computed::NotReady("trade_venue_stale_or_missing"));
        }
        if !self.trade_clock.warmed(now_ns, WARMUP_60S_NS) {
            return Ok(Computed::NotReady("trade_history_warming_up"));
        }
        let values = perp_cvd_values(&self.trades, now_ns)?;
        Ok(Computed::Ready(values))
    }

    // --- checkpoint --------------------------------------------------------

    /// The per-feature output lineage, keyed by signal id, for the venue lane's
    /// checkpoint (mirrors `VenueLane::checkpoint_features`).
    pub fn checkpoint_features(&self) -> BTreeMap<String, SpotSignalFeatureCheckpoint> {
        self.emitters
            .iter()
            .map(|(family, emitter)| (signal_id(self.spec.token, *family), emitter.checkpoint()))
            .collect()
    }

    /// Restart every derivatives family's per-generation output sequence at zero
    /// for a new file generation (C5), keeping the warm rings and lifetime state.
    /// Mirrors [`VenueLane::reset_output_sequences`]; called at a rotation boundary
    /// so the successor's fresh output epoch numbers each family's first ready
    /// decision from zero, as a resume's fresh restore would.
    pub fn reset_output_sequences(&mut self) {
        for emitter in self.emitters.values_mut() {
            emitter.reset_output_sequence();
        }
    }

    /// Restore the seven emitters from a checkpoint; a missing feature is a foreign
    /// checkpoint. The rolling histories are not persisted — a resumed lane
    /// re-warms them from live input (spec §5, matching the spot lane).
    pub fn restore_features(
        &mut self,
        features: &BTreeMap<String, SpotSignalFeatureCheckpoint>,
    ) -> Result<(), SpotSignalError> {
        for (family, emitter) in &mut self.emitters {
            let id = signal_id(self.spec.token, *family);
            let checkpoint = features.get(&id).ok_or_else(|| {
                SpotSignalError::Checkpoint(format!("checkpoint is missing feature {id}"))
            })?;
            emitter.restore(checkpoint);
        }
        self.reset();
        self.last_now_ns = None;
        Ok(())
    }
}

// --- pure helpers (unit-tested directly, §5) -------------------------------

/// The event's venue event-time as nanoseconds since the Unix epoch, for the
/// cross-source mark ordering gate (F2/E50). `event_time` is already validated
/// as an RFC 3339 timestamp by `validate_event`, so a parse failure here is a
/// malformed admitted event, not an ordering outcome.
fn event_time_ns(event: &SourceEvent) -> Result<i64, SpotSignalError> {
    DateTime::parse_from_rfc3339(&event.event_time)
        .map_err(|error| SpotSignalError::MarketValue(format!("invalid event_time: {error}")))?
        .timestamp_nanos_opt()
        .ok_or_else(|| {
            SpotSignalError::MarketValue("event_time is outside the representable range".to_owned())
        })
}

/// `basis_bps = (mark - index) / index * 10000`.
fn basis_bps(mark: Decimal, index: Decimal) -> Result<Decimal, SpotSignalError> {
    checked_mul(
        checked_div(checked_sub(mark, index)?, index)?,
        Decimal::from(10_000),
    )
}

/// `delta_pct = delta / level * 100`, exactly 0 when the level is 0 (spec §4.3).
fn delta_pct(delta: Decimal, level: Decimal) -> Result<Decimal, SpotSignalError> {
    if level.is_zero() {
        return Ok(Decimal::ZERO);
    }
    checked_mul(checked_div(delta, level)?, Decimal::from(100))
}

/// The `liquidation_pressure` value map over the 300 s liquidation window
/// (spec §4.4). Pure over the event history so the formula is unit-tested
/// independently of the venue readiness and side-confirmation gates.
fn liquidation_values(
    events: &VecDeque<LiquidationEvent>,
    now_ns: u64,
    spec: &DerivSpec,
) -> Result<BTreeMap<String, Value>, SpotSignalError> {
    let in_window = |window_ns: u64, event: &LiquidationEvent| {
        let start = now_ns.saturating_sub(window_ns);
        event.receipt_ns > start && event.receipt_ns <= now_ns
    };
    let mut count_10s = 0u32;
    let mut count_60s = 0u32;
    let mut count_300s = 0u32;
    let mut notional_10s = Decimal::ZERO;
    let mut notional_60s = Decimal::ZERO;
    let mut notional_300s = Decimal::ZERO;
    let mut long_60s = Decimal::ZERO;
    let mut short_60s = Decimal::ZERO;
    let mut max_300s = Decimal::ZERO;
    for event in events {
        if in_window(FIVE_MINUTES_NS, event) {
            count_300s += 1;
            notional_300s = checked_add(notional_300s, event.notional)?;
            if event.notional > max_300s {
                max_300s = event.notional;
            }
        }
        if in_window(SIXTY_SECONDS_NS, event) {
            count_60s += 1;
            notional_60s = checked_add(notional_60s, event.notional)?;
            match event.side {
                LiquidatedSide::Long => long_60s = checked_add(long_60s, event.notional)?,
                LiquidatedSide::Short => short_60s = checked_add(short_60s, event.notional)?,
            }
        }
        if in_window(TEN_SECONDS_NS, event) {
            count_10s += 1;
            notional_10s = checked_add(notional_10s, event.notional)?;
        }
    }
    let side_total = checked_add(long_60s, short_60s)?;
    let side_skew = if side_total.is_zero() {
        Decimal::ZERO
    } else {
        checked_div(checked_sub(long_60s, short_60s)?, side_total)?
    };
    // burst_ratio = notional_10s / (notional_300s / 30); 0 when the denom is 0.
    let denom = checked_div(notional_300s, Decimal::from(30))?;
    let burst_ratio = if denom.is_zero() {
        Decimal::ZERO
    } else {
        checked_div(notional_10s, denom)?
    };
    let cascade_ratio = Decimal::from(5);
    let elevated_ratio = Decimal::from(2);
    let cascade_min_count = 3u32;
    let clustering_state = if count_10s == 0 {
        "quiet"
    } else if burst_ratio >= cascade_ratio && count_10s >= cascade_min_count {
        "cascade"
    } else if burst_ratio >= elevated_ratio {
        "elevated"
    } else {
        "quiet"
    };
    Ok(BTreeMap::from([
        ("count_10s".to_owned(), json!(count_10s)),
        ("count_60s".to_owned(), json!(count_60s)),
        ("count_300s".to_owned(), json!(count_300s)),
        ("notional_usd_10s".to_owned(), decimal_value(notional_10s)),
        ("notional_usd_60s".to_owned(), decimal_value(notional_60s)),
        ("notional_usd_300s".to_owned(), decimal_value(notional_300s)),
        ("long_notional_usd_60s".to_owned(), decimal_value(long_60s)),
        (
            "short_notional_usd_60s".to_owned(),
            decimal_value(short_60s),
        ),
        ("side_skew_60s".to_owned(), decimal_value(side_skew)),
        (
            "max_single_notional_usd_300s".to_owned(),
            decimal_value(max_300s),
        ),
        ("burst_ratio".to_owned(), decimal_value(burst_ratio)),
        ("clustering_state".to_owned(), json!(clustering_state)),
        ("cascade_ratio".to_owned(), decimal_value(cascade_ratio)),
        ("elevated_ratio".to_owned(), decimal_value(elevated_ratio)),
        ("cascade_min_count".to_owned(), json!(cascade_min_count)),
        (
            "price_basis".to_owned(),
            json!(spec.liquidation.price_basis),
        ),
        (
            "feed_completeness".to_owned(),
            json!(spec.feed_completeness),
        ),
    ]))
}

/// The `perp_cvd` value map over the 1/10/60 s trade windows — the adopted
/// `btc.coinbase.aggressor_flow` formula applied to the perp tape (spec §4.5).
fn perp_cvd_values(
    trades: &VecDeque<TradeEvent>,
    now_ns: u64,
) -> Result<BTreeMap<String, Value>, SpotSignalError> {
    let in_window = |window_ns: u64, event: &TradeEvent| {
        let start = now_ns.saturating_sub(window_ns);
        event.receipt_ns > start && event.receipt_ns <= now_ns
    };
    let mut signed_1s = Decimal::ZERO;
    let mut signed_10s = Decimal::ZERO;
    let mut signed_60s = Decimal::ZERO;
    let mut cvd_base_10s = Decimal::ZERO;
    let mut cvd_base_60s = Decimal::ZERO;
    let mut gross_60s = Decimal::ZERO;
    let mut count_60s = 0u32;
    for event in trades {
        if in_window(SIXTY_SECONDS_NS, event) {
            signed_60s = checked_add(signed_60s, event.signed_notional)?;
            cvd_base_60s = checked_add(cvd_base_60s, event.signed_base)?;
            gross_60s = checked_add(gross_60s, event.gross_notional)?;
            count_60s += 1;
        }
        if in_window(TEN_SECONDS_NS, event) {
            signed_10s = checked_add(signed_10s, event.signed_notional)?;
            cvd_base_10s = checked_add(cvd_base_10s, event.signed_base)?;
        }
        if in_window(ONE_SECOND_NS, event) {
            signed_1s = checked_add(signed_1s, event.signed_notional)?;
        }
    }
    let flow_direction_10s = match signed_10s.cmp(&Decimal::ZERO) {
        std::cmp::Ordering::Greater => "buy",
        std::cmp::Ordering::Less => "sell",
        std::cmp::Ordering::Equal => "flat",
    };
    Ok(BTreeMap::from([
        (
            "signed_notional_usd_1s".to_owned(),
            decimal_value(signed_1s),
        ),
        (
            "signed_notional_usd_10s".to_owned(),
            decimal_value(signed_10s),
        ),
        (
            "signed_notional_usd_60s".to_owned(),
            decimal_value(signed_60s),
        ),
        (
            "cumulative_volume_delta_base_10s".to_owned(),
            decimal_value(cvd_base_10s),
        ),
        (
            "cumulative_volume_delta_base_60s".to_owned(),
            decimal_value(cvd_base_60s),
        ),
        (
            "gross_notional_usd_60s".to_owned(),
            decimal_value(gross_60s),
        ),
        ("trade_count_60s".to_owned(), json!(count_60s)),
        ("flow_direction_10s".to_owned(), json!(flow_direction_10s)),
    ]))
}

/// The latest reading with logical time at or before `target_ns` (spec §4).
fn reading_at_or_before(history: &VecDeque<(u64, Decimal)>, target_ns: u64) -> Option<Decimal> {
    history
        .iter()
        .rev()
        .find(|(ns, _)| *ns <= target_ns)
        .map(|(_, value)| *value)
}

/// Push a reading, then refuse if the bounded history exceeds its cap (§4).
fn push_bounded(
    history: &mut VecDeque<(u64, Decimal)>,
    now_ns: u64,
    value: Decimal,
    max_entries: usize,
) -> Result<(), SpotSignalError> {
    history.push_back((now_ns, value));
    if history.len() > max_entries {
        return Err(SpotSignalError::ResourceLimit(format!(
            "reading history exceeded {max_entries} entries"
        )));
    }
    Ok(())
}

fn prune_before(history: &mut VecDeque<(u64, Decimal)>, cutoff_ns: u64) {
    // Retain the newest reading at or before `cutoff_ns`: pop the front only
    // while the *second*-oldest entry is itself still at or before the cutoff.
    // A plain `front < cutoff` test drops the last reading inside the lookback
    // window whenever the poll cadence is coarser than the prune margin (e.g.
    // Binance's 5 s open-interest poll), which leaves a lookback of exactly the
    // window unsatisfiable and the family flapping ready <-> warming_up between
    // polls. Keeping the newest at-or-before-cutoff reading makes a lookback of
    // exactly the window satisfiable at any poll cadence (B2 §11). `get(1)`
    // yields `None` once one entry remains, so at least one reading is always
    // retained.
    while history.get(1).is_some_and(|(ns, _)| *ns <= cutoff_ns) {
        history.pop_front();
    }
}

fn logical_timestamp(
    wall_start: DateTime<Utc>,
    now_ns: u64,
) -> Result<DateTime<Utc>, SpotSignalError> {
    let delta = chrono::Duration::nanoseconds(
        i64::try_from(now_ns).map_err(|_| SpotSignalError::TimestampRange)?,
    );
    wall_start
        .checked_add_signed(delta)
        .ok_or(SpotSignalError::TimestampRange)
}

const fn event_time_field(family: DerivFamily) -> &'static str {
    // Enumerated, never a wildcard: a family added to `DerivFamily` without a
    // decision here must fail the build rather than silently inherit
    // `window_close`.
    match family {
        DerivFamily::MarkIndexBasisState
        | DerivFamily::FundingState
        | DerivFamily::OpenInterestDelta => "observation_time",
        DerivFamily::LiquidationPressure
        | DerivFamily::PerpCvd
        | DerivFamily::PerpAtr
        | DerivFamily::PerpRsi
        | DerivFamily::PerpRoc
        | DerivFamily::PerpEmaCross
        | DerivFamily::PerpMacd
        | DerivFamily::PerpBollinger
        | DerivFamily::PerpZScore
        | DerivFamily::PerpMomentum
        | DerivFamily::PerpVolatility => "window_close",
    }
}

const fn staleness_ns(family: DerivFamily) -> u64 {
    match family {
        // 30 000 ms for open_interest_delta, 10 000 ms for the rest (spec §1).
        DerivFamily::OpenInterestDelta => 30 * ONE_SECOND_NS,
        _ => 10 * ONE_SECOND_NS,
    }
}

const fn warming_reason(venue: Venue) -> &'static str {
    match venue {
        Venue::Binance => "binance_warming_up",
        Venue::Bybit => "bybit_warming_up",
        _ => "binance_warming_up",
    }
}

const fn stale_reason(venue: Venue) -> &'static str {
    match venue {
        Venue::Binance => "binance_stale",
        Venue::Bybit => "bybit_stale",
        _ => "binance_stale",
    }
}

const fn recovery_reason(venue: Venue) -> &'static str {
    match venue {
        Venue::Binance => "binance_recovery_required",
        Venue::Bybit => "bybit_recovery_required",
        _ => "binance_recovery_required",
    }
}

fn decimal_field(event: &SourceEvent, name: &str) -> Result<Decimal, SpotSignalError> {
    let text = event
        .values
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| SpotSignalError::MissingField(name.to_owned()))?;
    Ok(parse_decimal_text(text)?)
}

fn optional_decimal(event: &SourceEvent, name: &str) -> Result<Option<Decimal>, SpotSignalError> {
    match event.values.get(name) {
        None => Ok(None),
        Some(value) => {
            let text = value
                .as_str()
                .ok_or_else(|| SpotSignalError::MissingField(name.to_owned()))?;
            Ok(Some(parse_decimal_text(text)?))
        }
    }
}

fn str_field<'a>(event: &'a SourceEvent, name: &str) -> Result<&'a str, SpotSignalError> {
    event
        .values
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| SpotSignalError::MissingField(name.to_owned()))
}

fn optional_str(event: &SourceEvent, name: &str) -> Option<String> {
    event
        .values
        .get(name)
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn u32_field(event: &SourceEvent, name: &str) -> Result<u32, SpotSignalError> {
    event
        .values
        .get(name)
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| SpotSignalError::MissingField(name.to_owned()))
}

fn u64_field(event: &SourceEvent, name: &str) -> Result<u64, SpotSignalError> {
    event
        .values
        .get(name)
        .and_then(Value::as_u64)
        .ok_or_else(|| SpotSignalError::MissingField(name.to_owned()))
}

/// The `'static` signal id for a `(venue, family)`, matching [`signal_id`], so a
/// [`FeatureEmitter`]'s contract identity (a `&'static str`) needs no allocation.
const fn signal_id_static(venue: Venue, family: DerivFamily) -> &'static str {
    match (venue, family) {
        (Venue::Binance, DerivFamily::MarkIndexBasisState) => "btc.binance.mark_index_basis_state",
        (Venue::Binance, DerivFamily::FundingState) => "btc.binance.funding_state",
        (Venue::Binance, DerivFamily::OpenInterestDelta) => "btc.binance.open_interest_delta",
        // E62: Binance produces neither family, so this arm is never reached.
        (Venue::Binance, DerivFamily::LiquidationPressure)
        | (Venue::Binance, DerivFamily::PerpCvd) => {
            panic!("Binance does not produce liquidation_pressure/perp_cvd (E62)")
        }
        (Venue::Bybit, DerivFamily::MarkIndexBasisState) => "btc.bybit.mark_index_basis_state",
        (Venue::Bybit, DerivFamily::FundingState) => "btc.bybit.funding_state",
        (Venue::Bybit, DerivFamily::OpenInterestDelta) => "btc.bybit.open_interest_delta",
        (Venue::Bybit, DerivFamily::LiquidationPressure) => "btc.bybit.liquidation_pressure",
        (Venue::Bybit, DerivFamily::PerpCvd) => "btc.bybit.perp_cvd",
        (Venue::Binance, DerivFamily::PerpAtr) => "btc.binance.perp_atr",
        (Venue::Binance, DerivFamily::PerpRsi) => "btc.binance.perp_rsi",
        (Venue::Binance, DerivFamily::PerpRoc) => "btc.binance.perp_roc",
        (Venue::Binance, DerivFamily::PerpEmaCross) => "btc.binance.perp_ema_cross",
        (Venue::Binance, DerivFamily::PerpMacd) => "btc.binance.perp_macd",
        (Venue::Binance, DerivFamily::PerpBollinger) => "btc.binance.perp_bollinger",
        (Venue::Binance, DerivFamily::PerpZScore) => "btc.binance.perp_zscore",
        (Venue::Bybit, DerivFamily::PerpAtr) => "btc.bybit.perp_atr",
        (Venue::Bybit, DerivFamily::PerpRsi) => "btc.bybit.perp_rsi",
        (Venue::Bybit, DerivFamily::PerpRoc) => "btc.bybit.perp_roc",
        (Venue::Bybit, DerivFamily::PerpEmaCross) => "btc.bybit.perp_ema_cross",
        (Venue::Bybit, DerivFamily::PerpMacd) => "btc.bybit.perp_macd",
        (Venue::Bybit, DerivFamily::PerpBollinger) => "btc.bybit.perp_bollinger",
        (Venue::Bybit, DerivFamily::PerpZScore) => "btc.bybit.perp_zscore",
        (Venue::Binance, DerivFamily::PerpMomentum) => "btc.binance.perp_momentum",
        (Venue::Binance, DerivFamily::PerpVolatility) => "btc.binance.perp_volatility",
        (Venue::Bybit, DerivFamily::PerpMomentum) => "btc.bybit.perp_momentum",
        (Venue::Bybit, DerivFamily::PerpVolatility) => "btc.bybit.perp_volatility",
        _ => "btc.binance.mark_index_basis_state",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dec(text: &str) -> Decimal {
        parse_decimal_text(text).expect("decimal parses")
    }

    /// E62: the shared `DerivFamily` enum stays fourteen, but Binance produces
    /// twelve — never `liquidation_pressure`/`perp_cvd` — while Bybit produces all
    /// fourteen; and no Binance contract document names the two deleted identities.
    #[test]
    fn binance_produces_twelve_families_bybit_fourteen_and_never_the_socket_twins() {
        assert_eq!(DerivFamily::ALL.len(), 14, "the shared enum stays fourteen");
        assert_eq!(BINANCE_SPEC.families.len(), 12, "Binance produces twelve");
        assert_eq!(BYBIT_SPEC.families.len(), 14, "Bybit produces fourteen");
        for excluded in [DerivFamily::LiquidationPressure, DerivFamily::PerpCvd] {
            assert!(
                !BINANCE_SPEC.families.contains(&excluded),
                "{excluded:?} must not be a produced Binance family (E62)"
            );
            assert!(
                BYBIT_SPEC.families.contains(&excluded),
                "{excluded:?} must stay a Bybit family (twin untouched)"
            );
        }
        // The definition-coverage documents carry exactly the produced identities:
        // no `btc.binance.perp_cvd` / `btc.binance.liquidation_pressure`.
        let docs = venue_derivatives_signal_contract_documents();
        let binance: Vec<&str> = docs
            .iter()
            .map(|(id, _)| *id)
            .filter(|id| id.starts_with("btc.binance."))
            .collect();
        let bybit = docs
            .iter()
            .filter(|(id, _)| id.starts_with("btc.bybit."))
            .count();
        assert_eq!(binance.len(), 12);
        assert_eq!(bybit, 14);
        assert!(
            !binance.iter().any(|id| {
                *id == "btc.binance.perp_cvd" || *id == "btc.binance.liquidation_pressure"
            }),
            "no Binance contract document may name the deleted twins: {binance:?}"
        );
    }

    fn liq(receipt_ns: u64, notional: &str, side: LiquidatedSide) -> LiquidationEvent {
        LiquidationEvent {
            receipt_ns,
            notional: dec(notional),
            side,
        }
    }

    /// Every `DerivFamily` variant is classified explicitly by
    /// `event_time_field` and `ta_kind` — the two matches that C3a had left on
    /// wildcards. The expected table is written per variant, so a family added
    /// to the enum has to be added here as well as to the two matches (which
    /// the compiler now enforces) and to `ALL`.
    #[test]
    fn every_deriv_family_is_classified_explicitly() {
        const EXPECTED: [(DerivFamily, &str, bool); 14] = [
            (DerivFamily::MarkIndexBasisState, "observation_time", false),
            (DerivFamily::FundingState, "observation_time", false),
            (DerivFamily::OpenInterestDelta, "observation_time", false),
            (DerivFamily::LiquidationPressure, "window_close", false),
            (DerivFamily::PerpCvd, "window_close", false),
            (DerivFamily::PerpAtr, "window_close", true),
            (DerivFamily::PerpRsi, "window_close", true),
            (DerivFamily::PerpRoc, "window_close", true),
            (DerivFamily::PerpEmaCross, "window_close", true),
            (DerivFamily::PerpMacd, "window_close", true),
            (DerivFamily::PerpBollinger, "window_close", true),
            (DerivFamily::PerpZScore, "window_close", true),
            // C2 (E49): reference-free perp momentum/volatility — window_close
            // event field, and non-TA (they compute outside the shared-TA path).
            (DerivFamily::PerpMomentum, "window_close", false),
            (DerivFamily::PerpVolatility, "window_close", false),
        ];
        assert_eq!(
            DerivFamily::ALL.len(),
            EXPECTED.len(),
            "every published family is classified here"
        );
        for (family, event_field, is_ta) in EXPECTED {
            assert!(
                DerivFamily::ALL.contains(&family),
                "{family:?} is published"
            );
            assert_eq!(
                event_time_field(family),
                event_field,
                "{family:?}: event_time_field"
            );
            assert_eq!(
                family.ta_kind().is_some(),
                is_ta,
                "{family:?}: technical-analysis classification"
            );
        }
    }

    #[test]
    fn basis_bps_is_exact() {
        // (100.05 - 100) / 100 * 10000 = 5.
        assert_eq!(basis_bps(dec("100.05"), dec("100")).unwrap(), dec("5"));
        // A negative basis: (99.98 - 100)/100*10000 = -2.
        assert_eq!(basis_bps(dec("99.98"), dec("100")).unwrap(), dec("-2"));
    }

    #[test]
    fn delta_pct_is_zero_at_a_zero_level() {
        assert_eq!(delta_pct(dec("100"), Decimal::ZERO).unwrap(), Decimal::ZERO);
        // 50 / 1000 * 100 = 5.
        assert_eq!(delta_pct(dec("50"), dec("1000")).unwrap(), dec("5"));
    }

    #[test]
    fn side_skew_and_burst_ratio_are_zero_at_a_zero_denominator() {
        // Empty window: every count and sum is zero, side_skew and burst_ratio 0.
        let values = liquidation_values(&VecDeque::new(), TEN_SECONDS_NS, &BYBIT_SPEC).unwrap();
        assert_eq!(values["side_skew_60s"], json!("0"));
        assert_eq!(values["burst_ratio"], json!("0"));
        assert_eq!(values["clustering_state"], json!("quiet"));
        assert_eq!(values["count_10s"], json!(0u32));
        assert_eq!(values["max_single_notional_usd_300s"], json!("0"));
    }

    #[test]
    fn side_skew_splits_by_liquidated_side() {
        let now = SIXTY_SECONDS_NS;
        let events = VecDeque::from(vec![
            liq(now, "300", LiquidatedSide::Long),
            liq(now, "100", LiquidatedSide::Short),
        ]);
        let values = liquidation_values(&events, now, &BYBIT_SPEC).unwrap();
        assert_eq!(values["long_notional_usd_60s"], json!("300"));
        assert_eq!(values["short_notional_usd_60s"], json!("100"));
        // (300 - 100) / (300 + 100) = 0.5.
        assert_eq!(values["side_skew_60s"], json!("0.5"));
    }

    #[test]
    fn clustering_state_walks_the_bands() {
        let now = TEN_SECONDS_NS;
        // Three 100-notional events all inside the 10 s window; nothing else, so
        // notional_300s == notional_10s and burst_ratio = 30 >= cascade_ratio 5.
        let cascade = VecDeque::from(vec![
            liq(now, "100", LiquidatedSide::Long),
            liq(now, "100", LiquidatedSide::Long),
            liq(now, "100", LiquidatedSide::Long),
        ]);
        let values = liquidation_values(&cascade, now, &BYBIT_SPEC).unwrap();
        assert_eq!(values["burst_ratio"], json!("30"));
        assert_eq!(values["clustering_state"], json!("cascade"));

        // Same 300 in the 10 s window but 3000 across the 300 s window: burst
        // ratio = 300 / (3000/30) = 3, in [elevated_ratio 2, cascade_ratio 5).
        // `now` at 100 s so the 2700 event is outside 10 s but inside 300 s.
        let now = 100 * ONE_SECOND_NS;
        let mut elevated = VecDeque::from(vec![liq(
            now - 20 * ONE_SECOND_NS,
            "2700",
            LiquidatedSide::Long,
        )]);
        for _ in 0..3 {
            elevated.push_back(liq(now, "100", LiquidatedSide::Long));
        }
        let values = liquidation_values(&elevated, now, &BYBIT_SPEC).unwrap();
        assert_eq!(values["notional_usd_10s"], json!("300"));
        assert_eq!(values["notional_usd_300s"], json!("3000"));
        assert_eq!(values["burst_ratio"], json!("3"));
        assert_eq!(values["clustering_state"], json!("elevated"));
        assert_eq!(values["max_single_notional_usd_300s"], json!("2700"));
    }

    #[test]
    fn feed_completeness_and_price_basis_differ_by_venue() {
        let binance = liquidation_values(&VecDeque::new(), TEN_SECONDS_NS, &BINANCE_SPEC).unwrap();
        assert_eq!(binance["feed_completeness"], json!("throttled"));
        assert_eq!(binance["price_basis"], json!("average_fill_price"));
        let bybit = liquidation_values(&VecDeque::new(), TEN_SECONDS_NS, &BYBIT_SPEC).unwrap();
        assert_eq!(bybit["feed_completeness"], json!("complete"));
        assert_eq!(bybit["price_basis"], json!("bankruptcy_price"));
    }

    #[test]
    fn flow_direction_is_flat_at_exactly_zero() {
        let now = TEN_SECONDS_NS;
        let trades = VecDeque::from(vec![
            TradeEvent {
                receipt_ns: now,
                signed_notional: dec("100"),
                signed_base: dec("1"),
                gross_notional: dec("100"),
            },
            TradeEvent {
                receipt_ns: now,
                signed_notional: dec("-100"),
                signed_base: dec("-1"),
                gross_notional: dec("100"),
            },
        ]);
        let values = perp_cvd_values(&trades, now).unwrap();
        assert_eq!(values["signed_notional_usd_10s"], json!("0"));
        assert_eq!(values["flow_direction_10s"], json!("flat"));
        // gross sums absolute notional: 100 + 100 = 200; two admitted trades.
        assert_eq!(values["gross_notional_usd_60s"], json!("200"));
        assert_eq!(values["trade_count_60s"], json!(2u32));
    }

    #[test]
    fn reading_at_or_before_finds_the_latest_within_the_cutoff() {
        let history = VecDeque::from(vec![(10u64, dec("1")), (20, dec("2")), (30, dec("3"))]);
        assert_eq!(reading_at_or_before(&history, 25), Some(dec("2")));
        assert_eq!(reading_at_or_before(&history, 30), Some(dec("3")));
        assert_eq!(reading_at_or_before(&history, 5), None);
    }

    #[test]
    fn a_bounded_history_refuses_past_its_cap() {
        let mut history = VecDeque::new();
        for index in 0..BASIS_HISTORY_MAX {
            push_bounded(&mut history, index as u64, dec("1"), BASIS_HISTORY_MAX).unwrap();
        }
        let overflow = push_bounded(
            &mut history,
            BASIS_HISTORY_MAX as u64,
            dec("1"),
            BASIS_HISTORY_MAX,
        );
        assert!(matches!(overflow, Err(SpotSignalError::ResourceLimit(_))));
    }

    #[test]
    fn liquidated_side_maps_both_venue_conventions_to_one_concept() {
        let wall = DateTime::parse_from_rfc3339("2026-09-03T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let binance = VenueDerivativesEngine::new(Venue::Binance, "t", wall).unwrap();
        let bybit = VenueDerivativesEngine::new(Venue::Bybit, "t", wall).unwrap();
        // A Binance sell forced order liquidates a long (`liquidated_position_side`
        // is already normalized to `long`); Bybit `side = buy` liquidates a long
        // too. Both map to the one shared LiquidatedSide::Long.
        let binance_long = binance
            .liquidated_side(&liquidation_event(
                "binance.usdm.btcusdt.liquidations",
                &[("liquidated_position_side", json!("long"))],
            ))
            .unwrap();
        let bybit_long = bybit
            .liquidated_side(&liquidation_event(
                "bybit.linear.btcusdt.liquidations",
                &[("side", json!("buy"))],
            ))
            .unwrap();
        assert_eq!(binance_long, LiquidatedSide::Long);
        assert_eq!(bybit_long, LiquidatedSide::Long);
        let bybit_short = bybit
            .liquidated_side(&liquidation_event(
                "bybit.linear.btcusdt.liquidations",
                &[("side", json!("sell"))],
            ))
            .unwrap();
        assert_eq!(bybit_short, LiquidatedSide::Short);
    }

    fn liquidation_event(source_id: &str, extra: &[(&str, Value)]) -> SourceEvent {
        let mut values = BTreeMap::new();
        for (key, value) in extra {
            values.insert((*key).to_owned(), value.clone());
        }
        SourceEvent {
            schema_version: 1,
            source_id: source_id.to_owned(),
            source_version: "1.0.0".to_owned(),
            publisher_epoch: "t".to_owned(),
            sequence: 0,
            event_time: "2026-09-03T00:00:00Z".to_owned(),
            received_at: "2026-09-03T00:00:00Z".to_owned(),
            values,
        }
    }
}
