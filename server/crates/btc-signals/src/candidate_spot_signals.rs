//! Spot-microstructure computations, adopted per venue on 2026-09-02.
//!
//! Four signal computations (effective/realized spread, Kyle-lambda drift,
//! multi-level sweep detection, and book resilience). Each is a strictly
//! same-venue formula, so each is published once per adopted spot venue as
//! `btc.coinbase.*`/`btc.kraken.*` and has NO blended `btc.external.*` variant
//! — that variant is deferred pending evidence, not omitted (ADR 0024's
//! 2026-09-02 amendment). The module keeps its `candidate_` name because it
//! records where the computations were specified, not their adoption state, as
//! `candidate_prediction_signals` and `candidate_settlement_signals` already do.
//!
//! Identity, version and contract document belong to the per-venue contract
//! table in [`crate::spot_signals`]; this module owns only the computations.
//!
//! The computations mirror the adopted spot idioms deliberately:
//! deterministic state driven only by event timestamps (no wall-clock
//! reads), and exact `rust_decimal` arithmetic through the shared checked
//! helpers in [`crate::spot_signals`]. No binary floating point enters any
//! path; every value is a multiply/divide/add/subtract expressible in the
//! existing numeric vocabulary. Each computation carries an explicit
//! readiness/not-ready disposition rather than fabricating a value, exactly as
//! the adopted signals do with their quality reasons.
//!
//! Inputs are the adopted spot feeds only (Coinbase/Kraken `book-levels` and
//! `trades`); the paired candidate definition drafts declare those source ids.
//! Each computation is written for a single logical view of already-derived
//! primitives (a midpoint series, a signed-volume series, an aggressed-trade
//! batch, a top-of-book size series) so the least-squares / threshold math that
//! is the signal content stays exact and hand-verifiable. The code that joins
//! raw `book-levels`/`trades` events into those primitives is
//! [`crate::spot_event_join`]; [`CandidateMicrostructureSet`] at the end of
//! this module is the per-venue driver that runs all four computations over
//! that one join. Each `VenueLane` in [`crate::spot_signals`] owns one driver
//! and drives only its own venue, which is what makes "gated by its own venue
//! only" structural rather than conventional.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use rust_decimal::Decimal;

use crate::Venue;
use crate::spot_event_join::{JoinedObservation, PrevailingBook, SpotEventJoin, VenueJoinSnapshot};
use crate::spot_signals::{SpotSignalError, checked_add, checked_div, checked_mul, return_bps};

// The four measurements are published per venue since the 2026-09-02 adoption
// (ADR 0024), so their identities and versions live with the per-venue contract
// table in `crate::spot_signals` rather than here: one authority for what each
// identity IS, and no `btc.external.*` name left behind to imply a blended
// variant that is deliberately deferred.

/// Default realized-spread horizon (milliseconds); echoed by the definition
/// draft's `realized_horizon_ms` parameter field.
pub const DEFAULT_REALIZED_HORIZON_MS: u64 = 5000;
/// Default rolling window length in trades for the Kyle-lambda estimator (N);
/// echoed by the definition draft's `window_trades` field.
pub const DEFAULT_KYLE_WINDOW_TRADES: usize = 100;
/// Default minimum distinct price levels for a sweep (K); echoed by the
/// definition draft's `min_levels` field.
pub const DEFAULT_SWEEP_MIN_LEVELS: usize = 3;
/// Default depletion fraction (0.50); echoed by the definition draft's
/// `deplete_fraction` field.
pub const DEFAULT_DEPLETE_FRACTION: Decimal = Decimal::from_parts(5, 0, 0, false, 1);
/// Default recovery fraction (0.75); echoed by the definition draft's
/// `recover_fraction` field.
pub const DEFAULT_RECOVER_FRACTION: Decimal = Decimal::from_parts(75, 0, 0, false, 2);

const NANOS_PER_MILLI: u64 = 1_000_000;

// Bounds on unresolved state, mirroring the adopted rolling-history cap. Public
// so a test can drive a computation to its bound by name rather than by a
// copied magic number.

/// Most realized-spread measurements one venue may hold awaiting their horizon.
/// Exceeding it is a [`SpotSignalError::ResourceLimit`] refusal.
pub const MAX_PENDING_TRADES: usize = 1_000_000;
/// Most trades one venue's Kyle-lambda estimator may hold awaiting the forward
/// midpoint that completes their pair.
pub const MAX_KYLE_PAIRS: usize = 4_000_000;

/// Local exact subtraction. The shared `checked_sub` in [`crate::spot_signals`]
/// is module-private; this mirrors it so every candidate stays inside the same
/// checked-decimal vocabulary rather than reaching for a raw operator.
fn checked_sub(left: Decimal, right: Decimal) -> Result<Decimal, SpotSignalError> {
    left.checked_sub(right)
        .ok_or_else(|| SpotSignalError::Arithmetic("decimal subtraction overflowed".to_owned()))
}

fn observe_monotonic(last_event_ns: &mut Option<u64>, now_ns: u64) -> Result<(), SpotSignalError> {
    if last_event_ns.is_some_and(|previous| now_ns < previous) {
        return Err(SpotSignalError::TimeRegression);
    }
    *last_event_ns = Some(now_ns);
    Ok(())
}

/// The aggressing side of a trade (taker direction). Defined once in
/// [`crate::spot_event_join`] — the join and every computation here share it —
/// and re-exported so the candidate module keeps one import surface.
pub use crate::spot_event_join::AggressorSide;

impl AggressorSide {
    /// `+2` for a buyer-aggressor, `-2` for a seller-aggressor: the `2 * side_sign`
    /// factor of the effective/realized spread formula, kept exact. It lives with
    /// the spread computation rather than with the join, which carries only the
    /// plain `sign()`.
    fn double_sign(self) -> Decimal {
        match self {
            Self::Buy => Decimal::from(2),
            Self::Sell => Decimal::from(-2),
        }
    }
}

/// One side of the resting book (top-of-book displayed liquidity).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BookSide {
    Bid,
    Ask,
}

impl BookSide {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bid => "bid",
            Self::Ask => "ask",
        }
    }
}

// ---------------------------------------------------------------------------
// 1. effective_spread / realized_spread
// ---------------------------------------------------------------------------

/// Immediate effective-spread reading produced at a trade.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EffectiveSpreadOutcome {
    /// A same-venue midpoint at or before the trade existed. `realized` is
    /// always [`RealizedPart::awaiting`]: the paired realized-spread
    /// measurement has been enqueued and resolves only once the horizon elapses.
    Ready {
        effective_spread_bps: Decimal,
        midpoint: Decimal,
        /// The trade this reading describes, carried so the reading is a
        /// complete measurement on its own — the same shape a later
        /// [`RealizedSpread`] resolution carries.
        trade_price: Decimal,
        aggressor: AggressorSide,
        realized: RealizedPart,
    },
    /// No midpoint observed yet — a defined not-ready state, never a fabricated
    /// value. `reason` mirrors the adopted quality-reason vocabulary. No
    /// realized measurement is enqueued for a trade with no prevailing midpoint.
    NotReady { reason: &'static str },
}

/// Resolution state of a realized-spread measurement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RealizedState {
    /// The horizon has not yet elapsed; the measurement is pending.
    AwaitingHorizon,
    /// The horizon has elapsed and the prevailing-at-horizon midpoint is known.
    Ready,
}

impl RealizedState {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AwaitingHorizon => "awaiting_horizon",
            Self::Ready => "ready",
        }
    }
}

/// The realized-spread half of the published shape, carried by both the
/// immediate reading at a trade and the later resolution.
///
/// The draft declares `realized_spread_bps` and `midpoint_at_horizon`
/// unconditionally, and a Ready update must carry every declared field, so
/// neither is ever omitted: `realized_present` says whether the two carry a
/// measured value or the explicit `0` sentinel. `realized_state` is the same
/// fact in the draft's enum vocabulary; the flag exists so a consumer never has
/// to parse an enum string to know whether a number means anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RealizedPart {
    pub realized_present: bool,
    pub realized_state: RealizedState,
    pub realized_spread_bps: Decimal,
    pub midpoint_at_horizon: Decimal,
}

impl RealizedPart {
    /// The sentinel shape: the horizon has not elapsed, so both numbers are the
    /// explicit zero placeholder and carry no measurement.
    #[must_use]
    pub fn awaiting() -> Self {
        Self {
            realized_present: false,
            realized_state: RealizedState::AwaitingHorizon,
            realized_spread_bps: Decimal::ZERO,
            midpoint_at_horizon: Decimal::ZERO,
        }
    }

    /// A resolved measurement.
    #[must_use]
    pub fn resolved(realized_spread_bps: Decimal, midpoint_at_horizon: Decimal) -> Self {
        Self {
            realized_present: true,
            realized_state: RealizedState::Ready,
            realized_spread_bps,
            midpoint_at_horizon,
        }
    }
}

/// A resolved realized-spread measurement, emitted once the trade's horizon has
/// strictly elapsed in event time. Resolution TRIGGERS on the first observation —
/// book OR trade — with event time strictly past the horizon end; the VALUE uses
/// the prevailing midpoint at the horizon — the freshest at-or-before-horizon-end
/// midpoint (the midpoint is a step function), i.e. the textbook TAQ mid at
/// t+horizon. A later midpoint never retroactively changes a resolved value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RealizedSpread {
    pub trade_time_ns: u64,
    pub aggressor: AggressorSide,
    pub trade_price: Decimal,
    /// The trade's own effective spread and the midpoint it was priced
    /// against, carried forward from the immediate reading. A resolution is
    /// therefore the COMPLETE measurement for that trade rather than only its
    /// realized half, so a publisher can emit it without having to re-pair it
    /// with an earlier reading it no longer holds.
    pub effective_spread_bps: Decimal,
    pub midpoint: Decimal,
    /// Always [`RealizedPart::resolved`] here: a measurement reaches this type
    /// only once its horizon has elapsed.
    pub realized: RealizedPart,
}

/// The result of observing a trade: its immediate effective-spread reading plus
/// any realized-spread measurements this observation's event time resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TradeObservation {
    pub effective: EffectiveSpreadOutcome,
    pub resolved: Vec<RealizedSpread>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingTrade {
    trade_time_ns: u64,
    horizon_end_ns: u64,
    aggressor: AggressorSide,
    price: Decimal,
    // The immediate reading this trade already published, retained so the
    // later resolution carries the whole measurement (see [`RealizedSpread`]).
    effective_spread_bps: Decimal,
    midpoint: Decimal,
    // Freshest midpoint seen with event time <= horizon_end_ns; frozen once the
    // horizon elapses. Seeded with the prevailing midpoint at trade time (which
    // is itself <= horizon_end_ns), then advanced by later in-horizon midpoints.
    // Always populated: a trade is enqueued only when a prevailing midpoint
    // exists, so the prevailing-at-horizon midpoint is always defined.
    midpoint_at_horizon: Decimal,
}

/// Effective/realized spread candidate (`btc.external.effective_spread`).
///
/// At each trade, `effective_spread_bps = 2 * side_sign * (price - mid) / mid`
/// in basis points, where `mid` is the freshest same-venue top-of-book midpoint
/// at or before the trade. The paired `realized_spread_bps` uses the prevailing
/// midpoint at `trade_time + horizon` (the freshest at-or-before-horizon
/// midpoint); it resolves only once an event with time strictly past that
/// horizon is observed, so an unelapsed horizon never fabricates a value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveSpreadCandidate {
    horizon_ns: u64,
    last_midpoint: Option<Decimal>,
    pending: VecDeque<PendingTrade>,
    last_event_ns: Option<u64>,
}

impl EffectiveSpreadCandidate {
    #[must_use]
    pub fn new(realized_horizon_ms: u64) -> Self {
        Self {
            horizon_ns: realized_horizon_ms.saturating_mul(NANOS_PER_MILLI),
            last_midpoint: None,
            pending: VecDeque::new(),
            last_event_ns: None,
        }
    }

    /// Constructs with the documented default realized-spread horizon.
    #[must_use]
    pub fn with_default_horizon() -> Self {
        Self::new(DEFAULT_REALIZED_HORIZON_MS)
    }

    /// Number of trades still awaiting their realized-spread horizon.
    #[must_use]
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Observe a fresh same-venue top-of-book midpoint. Advances the
    /// prevailing-at-horizon midpoint of every unresolved trade whose horizon
    /// has not yet passed, then resolves (in trade order) every trade whose
    /// horizon has strictly elapsed by this observation's event time.
    pub fn observe_book(
        &mut self,
        event_time_ns: u64,
        midpoint: Decimal,
    ) -> Result<Vec<RealizedSpread>, SpotSignalError> {
        observe_monotonic(&mut self.last_event_ns, event_time_ns)?;
        if midpoint <= Decimal::ZERO {
            return Err(SpotSignalError::MarketValue(
                "book midpoint must be positive".to_owned(),
            ));
        }
        self.last_midpoint = Some(midpoint);
        for trade in &mut self.pending {
            if event_time_ns <= trade.horizon_end_ns {
                trade.midpoint_at_horizon = midpoint;
            }
        }
        self.resolve_elapsed(event_time_ns)
    }

    /// Observe a trade. First resolves any pending measurement whose horizon has
    /// strictly elapsed by this trade's event time — resolution triggers on the
    /// first observation past the horizon, book OR trade, and the resolved value
    /// still uses the frozen at-or-before-horizon midpoint (a trade carries no
    /// midpoint, so it only advances the trigger clock). Then returns the trade's
    /// immediate effective-spread reading, enqueuing a realized measurement only
    /// when a prevailing midpoint exists.
    pub fn observe_trade(
        &mut self,
        event_time_ns: u64,
        price: Decimal,
        aggressor: AggressorSide,
    ) -> Result<TradeObservation, SpotSignalError> {
        observe_monotonic(&mut self.last_event_ns, event_time_ns)?;
        if price <= Decimal::ZERO {
            return Err(SpotSignalError::MarketValue(
                "trade price must be positive".to_owned(),
            ));
        }
        let resolved = self.resolve_elapsed(event_time_ns)?;
        let effective = match self.last_midpoint {
            None => EffectiveSpreadOutcome::NotReady {
                reason: "no_prior_midpoint",
            },
            Some(midpoint) => {
                let horizon_end_ns = event_time_ns
                    .checked_add(self.horizon_ns)
                    .ok_or(SpotSignalError::Counter)?;
                if self.pending.len() >= MAX_PENDING_TRADES {
                    return Err(SpotSignalError::ResourceLimit(
                        "pending realized-spread measurements exceeded the bound".to_owned(),
                    ));
                }
                let effective_spread_bps =
                    checked_mul(return_bps(price, midpoint)?, aggressor.double_sign())?;
                // The prevailing midpoint has event time <= trade time <= horizon
                // end, so it is a valid initial prevailing-at-horizon midpoint.
                self.pending.push_back(PendingTrade {
                    trade_time_ns: event_time_ns,
                    horizon_end_ns,
                    aggressor,
                    price,
                    effective_spread_bps,
                    midpoint,
                    midpoint_at_horizon: midpoint,
                });
                EffectiveSpreadOutcome::Ready {
                    effective_spread_bps,
                    midpoint,
                    trade_price: price,
                    aggressor,
                    realized: RealizedPart::awaiting(),
                }
            }
        };
        Ok(TradeObservation {
            effective,
            resolved,
        })
    }

    /// Resolves, in trade order, every pending measurement whose horizon has
    /// strictly elapsed by `now_ns`, using each trade's frozen
    /// prevailing-at-horizon midpoint.
    fn resolve_elapsed(&mut self, now_ns: u64) -> Result<Vec<RealizedSpread>, SpotSignalError> {
        let mut resolved = Vec::new();
        while self
            .pending
            .front()
            .is_some_and(|front| now_ns > front.horizon_end_ns)
        {
            let trade = self
                .pending
                .pop_front()
                .expect("front presence just checked");
            resolved.push(RealizedSpread {
                trade_time_ns: trade.trade_time_ns,
                aggressor: trade.aggressor,
                trade_price: trade.price,
                effective_spread_bps: trade.effective_spread_bps,
                midpoint: trade.midpoint,
                realized: RealizedPart::resolved(
                    checked_mul(
                        return_bps(trade.price, trade.midpoint_at_horizon)?,
                        trade.aggressor.double_sign(),
                    )?,
                    trade.midpoint_at_horizon,
                ),
            });
        }
        Ok(resolved)
    }
}

// ---------------------------------------------------------------------------
// 2. kyle_lambda_drift
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct KylePair {
    mid_change: Decimal,
    signed_volume: Decimal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PendingKyleTrade {
    signed_volume: Decimal,
    prevailing_mid: Decimal,
}

/// Ready Kyle-lambda drift reading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KyleLambda {
    pub kyle_lambda: Decimal,
    pub kyle_lambda_previous: Decimal,
    pub lambda_drift: Decimal,
    pub pair_count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KyleOutcome {
    Ready(KyleLambda),
    NotReady { reason: &'static str },
}

/// Kyle-lambda drift candidate (`btc.external.kyle_lambda_drift`).
///
/// Over a rolling window of the last `N` per-trade observations,
/// `lambda = sum(mid_change_i * signed_volume_i) / sum(signed_volume_i^2)` — the
/// least-squares slope through the origin of per-trade midpoint change on signed
/// volume, chosen for the exact-arithmetic idiom because it needs only
/// multiply/divide. Each pair aligns a trade's `signed_volume` (size times
/// aggressor sign) with the midpoint change that FOLLOWS it: from the midpoint
/// prevailing at the trade to the next observed midpoint update. Trades between
/// two midpoint updates each pair their own signed volume with that same forward
/// change, so the estimator measures price impact, not anticipation. A trade
/// with no subsequent midpoint update yet has no completed pair.
///
/// `lambda_drift` is the current window's lambda minus the previous disjoint
/// window's. A pair completes only when a midpoint update follows the trade, so
/// Ready requires `2N` completed pairs — at least `2N` trades each followed by a
/// later midpoint update, after the seeding baseline midpoint. Fewer than `2N`
/// pairs, or a window whose sum of squared signed volume is zero, is a defined
/// not-ready state and never a division by zero.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KyleLambdaDriftCandidate {
    window: usize,
    current_mid: Option<Decimal>,
    pending: Vec<PendingKyleTrade>,
    pairs: VecDeque<KylePair>,
    last_event_ns: Option<u64>,
}

impl KyleLambdaDriftCandidate {
    #[must_use]
    pub fn new(window_trades: usize) -> Self {
        Self {
            window: window_trades,
            current_mid: None,
            pending: Vec::new(),
            pairs: VecDeque::new(),
            last_event_ns: None,
        }
    }

    /// Constructs with the documented default rolling window length.
    #[must_use]
    pub fn with_default_window() -> Self {
        Self::new(DEFAULT_KYLE_WINDOW_TRADES)
    }

    /// Number of trades still awaiting the forward midpoint that completes
    /// their pair. Bounded by [`MAX_KYLE_PAIRS`].
    #[must_use]
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Discard every trade still awaiting a forward midpoint and forget the
    /// prevailing midpoint.
    ///
    /// Called when the venue's book has gone stale: the forward midpoint those
    /// trades are waiting for can no longer arrive within the staleness bound,
    /// so pairing them would measure impact across a gap. Dropping them is also
    /// what keeps a stalled book with live trades from growing `pending` toward
    /// [`MAX_KYLE_PAIRS`] — completed pairs are untouched, because they were
    /// measured while the book was fresh.
    pub fn drain_pending(&mut self) {
        self.pending.clear();
        self.current_mid = None;
    }

    /// Observe a fresh same-venue midpoint. Closes every trade pending since the
    /// last midpoint update into a completed pair, using the forward change from
    /// each trade's prevailing midpoint to this new midpoint, then re-evaluates.
    pub fn observe_book(
        &mut self,
        event_time_ns: u64,
        midpoint: Decimal,
    ) -> Result<KyleOutcome, SpotSignalError> {
        observe_monotonic(&mut self.last_event_ns, event_time_ns)?;
        if midpoint <= Decimal::ZERO {
            return Err(SpotSignalError::MarketValue(
                "book midpoint must be positive".to_owned(),
            ));
        }
        for trade in self.pending.drain(..) {
            let mid_change = checked_sub(midpoint, trade.prevailing_mid)?;
            self.pairs.push_back(KylePair {
                mid_change,
                signed_volume: trade.signed_volume,
            });
            while self.pairs.len() > self.window.saturating_mul(2) {
                self.pairs.pop_front();
            }
        }
        self.current_mid = Some(midpoint);
        self.evaluate()
    }

    /// Observe a trade carrying its `signed_volume` (size times aggressor sign).
    /// The trade becomes pending against the prevailing midpoint; its pair
    /// completes only when the next midpoint update is observed.
    pub fn observe_trade(
        &mut self,
        event_time_ns: u64,
        signed_volume: Decimal,
    ) -> Result<KyleOutcome, SpotSignalError> {
        observe_monotonic(&mut self.last_event_ns, event_time_ns)?;
        let Some(prevailing_mid) = self.current_mid else {
            return Ok(KyleOutcome::NotReady {
                reason: "book_baseline_warming_up",
            });
        };
        if self.pending.len() >= MAX_KYLE_PAIRS {
            return Err(SpotSignalError::ResourceLimit(
                "pending Kyle-lambda trades exceeded the bound".to_owned(),
            ));
        }
        self.pending.push(PendingKyleTrade {
            signed_volume,
            prevailing_mid,
        });
        Ok(KyleOutcome::NotReady {
            reason: "awaiting_forward_midpoint",
        })
    }

    fn evaluate(&self) -> Result<KyleOutcome, SpotSignalError> {
        if self.pairs.len() < self.window.saturating_mul(2) {
            return Ok(KyleOutcome::NotReady {
                reason: "kyle_window_warming_up",
            });
        }
        let split = self.pairs.len() - self.window;
        let Some(previous_lambda) = self.window_lambda(0, split)? else {
            return Ok(KyleOutcome::NotReady {
                reason: "degenerate_signed_volume",
            });
        };
        let Some(current_lambda) = self.window_lambda(split, self.pairs.len())? else {
            return Ok(KyleOutcome::NotReady {
                reason: "degenerate_signed_volume",
            });
        };
        Ok(KyleOutcome::Ready(KyleLambda {
            kyle_lambda: current_lambda,
            kyle_lambda_previous: previous_lambda,
            lambda_drift: checked_sub(current_lambda, previous_lambda)?,
            pair_count: u32::try_from(self.pairs.len()).map_err(|_| SpotSignalError::Counter)?,
        }))
    }

    /// Least-squares slope through the origin over pairs `[start, end)`; `None`
    /// when the denominator is zero (guards the divide-by-zero edge).
    fn window_lambda(&self, start: usize, end: usize) -> Result<Option<Decimal>, SpotSignalError> {
        let mut numerator = Decimal::ZERO;
        let mut denominator = Decimal::ZERO;
        for index in start..end {
            let pair = self.pairs[index];
            numerator = checked_add(numerator, checked_mul(pair.mid_change, pair.signed_volume)?)?;
            denominator = checked_add(
                denominator,
                checked_mul(pair.signed_volume, pair.signed_volume)?,
            )?;
        }
        if denominator.is_zero() {
            return Ok(None);
        }
        Ok(Some(checked_div(numerator, denominator)?))
    }
}

// ---------------------------------------------------------------------------
// 3. sweep_detection
// ---------------------------------------------------------------------------

/// One aggressed trade inside an atomic same-venue, same-timestamp batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SweepBatchTrade {
    pub price: Decimal,
    pub size: Decimal,
    pub aggressor: AggressorSide,
}

/// A detected multi-level sweep.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sweep {
    pub direction: AggressorSide,
    pub swept_levels: u32,
    pub swept_size: Decimal,
}

/// Whether the batch just observed was itself a sweep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SweepDetectionState {
    Sweep,
    Quiet,
}

impl SweepDetectionState {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sweep => "sweep",
            Self::Quiet => "quiet",
        }
    }
}

/// The direction of the sweep the payload describes, with an explicit `None`
/// for "no sweep has been detected yet".
///
/// The draft declares `direction` unconditionally, so it is never omitted: the
/// `none` variant is what a Ready update carries before the first sweep, paired
/// with the `swept_levels = 0` / `swept_size = 0` sentinels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SweepDirection {
    Buy,
    Sell,
    None,
}

impl SweepDirection {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Buy => "buy",
            Self::Sell => "sell",
            Self::None => "none",
        }
    }
}

impl From<AggressorSide> for SweepDirection {
    fn from(side: AggressorSide) -> Self {
        match side {
            AggressorSide::Buy => Self::Buy,
            AggressorSide::Sell => Self::Sell,
        }
    }
}

/// The full sweep payload published at every batch: the batch's own verdict
/// plus the retained latest-sweep description, which is the sentinel shape
/// until the first sweep fires.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SweepReading {
    pub detection_state: SweepDetectionState,
    pub direction: SweepDirection,
    pub swept_levels: u32,
    pub swept_size: Decimal,
}

impl SweepReading {
    /// The payload for a state with no retained sweep: every state-dependent
    /// field present, as an explicit sentinel.
    fn sentinel(detection_state: SweepDetectionState) -> Self {
        Self {
            detection_state,
            direction: SweepDirection::None,
            swept_levels: 0,
            swept_size: Decimal::ZERO,
        }
    }

    fn describing(detection_state: SweepDetectionState, sweep: &Sweep) -> Self {
        Self {
            detection_state,
            direction: sweep.direction.into(),
            swept_levels: sweep.swept_levels,
            swept_size: sweep.swept_size,
        }
    }
}

/// Multi-level sweep-detection candidate (`btc.external.sweep_detection`).
///
/// Within one ingested batch (same venue, same event timestamp), aggressive
/// flow on one side that consumes `>= K` distinct prices is a sweep. This is a
/// STATE-SHAPED candidate with a latest-sweep payload and a quiet default: the
/// adopted spot set has no emit-only-on-detection signal to mirror, and every
/// adopted signal publishes a value each evaluation, so a batch that does not
/// sweep reports `detection_state = quiet` and republishes the retained
/// `latest` sweep untouched. Before any sweep has fired there is nothing to
/// retain, so the payload is the explicit sentinel
/// (`direction = none`, `swept_levels = 0`, `swept_size = 0`) rather than a
/// missing field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SweepDetectionCandidate {
    min_levels: usize,
    latest: Option<Sweep>,
    last_event_ns: Option<u64>,
}

impl SweepDetectionCandidate {
    #[must_use]
    pub fn new(min_levels: usize) -> Self {
        Self {
            min_levels,
            latest: None,
            last_event_ns: None,
        }
    }

    /// Constructs with the documented default minimum sweep levels.
    #[must_use]
    pub fn with_default_min_levels() -> Self {
        Self::new(DEFAULT_SWEEP_MIN_LEVELS)
    }

    /// The most recent detected sweep, if any.
    #[must_use]
    pub fn latest(&self) -> Option<&Sweep> {
        self.latest.as_ref()
    }

    pub fn observe_batch(
        &mut self,
        event_time_ns: u64,
        trades: &[SweepBatchTrade],
    ) -> Result<SweepReading, SpotSignalError> {
        observe_monotonic(&mut self.last_event_ns, event_time_ns)?;
        let buy = self.side_totals(trades, AggressorSide::Buy)?;
        let sell = self.side_totals(trades, AggressorSide::Sell)?;

        let buy_qualifies = buy.0 >= self.min_levels;
        let sell_qualifies = sell.0 >= self.min_levels;
        // A genuine sweep is one-sided; when both sides somehow qualify in one
        // batch, prefer the deeper level count, then the larger swept size, then
        // buy — a total order so the choice is deterministic.
        let chosen = match (buy_qualifies, sell_qualifies) {
            (true, false) => Some((AggressorSide::Buy, buy)),
            (false, true) => Some((AggressorSide::Sell, sell)),
            (true, true) => {
                if sell.0 > buy.0 || (sell.0 == buy.0 && sell.1 > buy.1) {
                    Some((AggressorSide::Sell, sell))
                } else {
                    Some((AggressorSide::Buy, buy))
                }
            }
            (false, false) => None,
        };
        match chosen {
            Some((direction, (levels, size))) => {
                let sweep = Sweep {
                    direction,
                    swept_levels: u32::try_from(levels).map_err(|_| SpotSignalError::Counter)?,
                    swept_size: size,
                };
                let reading = SweepReading::describing(SweepDetectionState::Sweep, &sweep);
                self.latest = Some(sweep);
                Ok(reading)
            }
            // A quiet batch republishes the retained latest sweep, or the
            // sentinel when none has fired yet — never an absent field.
            None => Ok(match &self.latest {
                Some(sweep) => SweepReading::describing(SweepDetectionState::Quiet, sweep),
                None => SweepReading::sentinel(SweepDetectionState::Quiet),
            }),
        }
    }

    /// `(distinct price levels, total consumed size)` for one aggressor side.
    fn side_totals(
        &self,
        trades: &[SweepBatchTrade],
        side: AggressorSide,
    ) -> Result<(usize, Decimal), SpotSignalError> {
        let mut prices = BTreeSet::new();
        let mut size = Decimal::ZERO;
        for trade in trades.iter().filter(|trade| trade.aggressor == side) {
            if trade.price <= Decimal::ZERO || trade.size <= Decimal::ZERO {
                return Err(SpotSignalError::MarketValue(
                    "sweep trade price and size must be positive".to_owned(),
                ));
            }
            prices.insert(trade.price);
            size = checked_add(size, trade.size)?;
        }
        Ok((prices.len(), size))
    }
}

// ---------------------------------------------------------------------------
// 4. book_resilience
// ---------------------------------------------------------------------------

/// Top-of-book displayed sizes on both sides at one book update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BookTop {
    pub bid_size: Decimal,
    pub ask_size: Decimal,
}

impl BookTop {
    #[must_use]
    pub fn new(bid_size: Decimal, ask_size: Decimal) -> Self {
        Self { bid_size, ask_size }
    }

    fn size(self, side: BookSide) -> Decimal {
        match side {
            BookSide::Bid => self.bid_size,
            BookSide::Ask => self.ask_size,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResilienceState {
    Recovering,
    Recovered,
    Quiet,
}

impl ResilienceState {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Recovering => "recovering",
            Self::Recovered => "recovered",
            Self::Quiet => "quiet",
        }
    }
}

/// The side a depletion is being tracked on, with an explicit `None` for "no
/// depletion is currently tracked".
///
/// The draft declares `depletion_side` unconditionally, so it is never omitted:
/// the `none` variant is what a Ready update carries while nothing is tracked,
/// paired with the `pre_depletion_size = 0` sentinel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DepletionSide {
    Bid,
    Ask,
    None,
}

impl DepletionSide {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bid => "bid",
            Self::Ask => "ask",
            Self::None => "none",
        }
    }
}

impl From<BookSide> for DepletionSide {
    fn from(side: BookSide) -> Self {
        match side {
            BookSide::Bid => Self::Bid,
            BookSide::Ask => Self::Ask,
        }
    }
}

/// Book-resilience reading at one book update.
///
/// Every state-dependent field is always present. `recovery_ms_present` is the
/// flag for `recovery_ms`, which cannot use a value sentinel of its own — a
/// completed recovery may legitimately measure `0` ms — and `depletion_side =
/// none` is the flag for `pre_depletion_size`, whose `0` sentinel is
/// unambiguous because a tracked pre-depletion size is always strictly
/// positive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResilienceReading {
    pub state: ResilienceState,
    /// Whether `recovery_ms` carries a completed measurement.
    pub recovery_ms_present: bool,
    /// Most recent completed recovery time; `0` while `recovery_ms_present` is
    /// false.
    pub recovery_ms: u64,
    pub depletion_side: DepletionSide,
    /// The tracked depletion's pre-depletion size; `0` while `depletion_side`
    /// is `none`.
    pub pre_depletion_size: Decimal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Depletion {
    side: BookSide,
    pre_size: Decimal,
    depletion_ns: u64,
}

/// Book-resilience candidate (`btc.external.book_resilience`).
///
/// After a depletion event — a side's top-level displayed size dropping by
/// `>= deplete_fraction` between consecutive same-venue book updates —
/// `recovery_ms` is the time until that side's top-level size first returns to
/// `>= recover_fraction` of the pre-depletion size. Emits the most recent
/// completed `recovery_ms` and a `state` (recovering / recovered / quiet). An
/// unresolved recovery stays `recovering` — no fabricated recovery time — and a
/// second depletion during recovery abandons the first, unresolved measurement
/// and tracks the new one (recovery is always measured against the most recent
/// liquidity withdrawal).
///
/// Zero displayed sizes are wire-plausible and are not errors: a current top
/// size of zero against a positive baseline is maximal depletion (a valid
/// depletion event). A zero baseline size defines no fractional drop, so no new
/// depletion starts from it, but an in-progress recovery is unaffected — it is
/// measured against the fixed pre-depletion size, not the intervening baseline.
/// Only a negative size (not wire-plausible) is rejected. `deplete_fraction` and
/// `recover_fraction` must each lie strictly in (0, 1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BookResilienceCandidate {
    deplete_fraction: Decimal,
    recover_fraction: Decimal,
    previous: Option<BookTop>,
    tracked: Option<Depletion>,
    last_recovery_ms: Option<u64>,
    last_event_ns: Option<u64>,
}

impl BookResilienceCandidate {
    /// Rejects degenerate fractions: each must lie strictly in (0, 1).
    pub fn new(
        deplete_fraction: Decimal,
        recover_fraction: Decimal,
    ) -> Result<Self, SpotSignalError> {
        for (name, fraction) in [
            ("deplete_fraction", deplete_fraction),
            ("recover_fraction", recover_fraction),
        ] {
            if fraction <= Decimal::ZERO || fraction >= Decimal::ONE {
                return Err(SpotSignalError::MarketValue(format!(
                    "{name} must lie strictly in (0, 1)"
                )));
            }
        }
        Ok(Self {
            deplete_fraction,
            recover_fraction,
            previous: None,
            tracked: None,
            last_recovery_ms: None,
            last_event_ns: None,
        })
    }

    /// Constructs with the documented default depletion and recovery fractions.
    #[must_use]
    pub fn with_default_thresholds() -> Self {
        Self::new(DEFAULT_DEPLETE_FRACTION, DEFAULT_RECOVER_FRACTION)
            .expect("default resilience fractions lie in (0, 1)")
    }

    pub fn observe_book(
        &mut self,
        event_time_ns: u64,
        top: BookTop,
    ) -> Result<ResilienceReading, SpotSignalError> {
        observe_monotonic(&mut self.last_event_ns, event_time_ns)?;
        if top.bid_size < Decimal::ZERO || top.ask_size < Decimal::ZERO {
            return Err(SpotSignalError::MarketValue(
                "top-of-book displayed sizes cannot be negative".to_owned(),
            ));
        }

        let mut state = if self.tracked.is_some() {
            ResilienceState::Recovering
        } else {
            ResilienceState::Quiet
        };

        if let Some(previous) = self.previous {
            if let Some((side, pre_size)) = self.detect_depletion(previous, top)? {
                // Covers both the first depletion and a second depletion during
                // recovery: the freshest depletion becomes the tracked baseline;
                // any earlier unresolved measurement is abandoned here.
                self.tracked = Some(Depletion {
                    side,
                    pre_size,
                    depletion_ns: event_time_ns,
                });
                state = ResilienceState::Recovering;
            } else if let Some(depletion) = self.tracked {
                let threshold = checked_mul(depletion.pre_size, self.recover_fraction)?;
                if top.size(depletion.side) >= threshold {
                    let elapsed_ns = event_time_ns
                        .checked_sub(depletion.depletion_ns)
                        .ok_or(SpotSignalError::TimeRegression)?;
                    self.last_recovery_ms = Some(elapsed_ns / NANOS_PER_MILLI);
                    self.tracked = None;
                    state = ResilienceState::Recovered;
                }
            }
        }

        self.previous = Some(top);
        Ok(ResilienceReading {
            state,
            recovery_ms_present: self.last_recovery_ms.is_some(),
            recovery_ms: self.last_recovery_ms.unwrap_or(0),
            depletion_side: self
                .tracked
                .map_or(DepletionSide::None, |depletion| depletion.side.into()),
            pre_depletion_size: self
                .tracked
                .map_or(Decimal::ZERO, |depletion| depletion.pre_size),
        })
    }

    /// The newly depleted side and its pre-depletion size, if any. A zero
    /// baseline size defines no fractional drop, so that side is skipped; a
    /// current size of zero against a positive baseline is maximal depletion and
    /// qualifies. When both sides drop past the threshold in one update, the
    /// deeper relative drop wins (bid on an exact tie) so the choice is
    /// deterministic.
    fn detect_depletion(
        &self,
        previous: BookTop,
        current: BookTop,
    ) -> Result<Option<(BookSide, Decimal)>, SpotSignalError> {
        let retain = checked_sub(Decimal::ONE, self.deplete_fraction)?;
        let mut chosen: Option<(BookSide, Decimal)> = None;
        for side in [BookSide::Bid, BookSide::Ask] {
            let prev_size = previous.size(side);
            if prev_size <= Decimal::ZERO {
                continue;
            }
            let new_size = current.size(side);
            let threshold = checked_mul(prev_size, retain)?;
            if new_size <= threshold {
                let deeper = match chosen {
                    // Compare relative drops new/prev across sides without
                    // division: new_a/prev_a < new_b/prev_b  <=>
                    // new_a*prev_b < new_b*prev_a (all sizes positive).
                    Some((other_side, _)) => {
                        let other_prev = previous.size(other_side);
                        checked_mul(new_size, other_prev)?
                            < checked_mul(current.size(other_side), prev_size)?
                    }
                    None => true,
                };
                if deeper {
                    chosen = Some((side, prev_size));
                }
            }
        }
        Ok(chosen)
    }
}

// ---------------------------------------------------------------------------
// 5. per-venue driver over the shared raw-event join
// ---------------------------------------------------------------------------

/// The parameters the four computations take, kept together so a venue reset
/// rebuilds its state from exactly the configuration it started with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MicrostructureParams {
    pub realized_horizon_ms: u64,
    pub window_trades: usize,
    pub sweep_min_levels: usize,
    pub deplete_fraction: Decimal,
    pub recover_fraction: Decimal,
}

impl Default for MicrostructureParams {
    fn default() -> Self {
        Self {
            realized_horizon_ms: DEFAULT_REALIZED_HORIZON_MS,
            window_trades: DEFAULT_KYLE_WINDOW_TRADES,
            sweep_min_levels: DEFAULT_SWEEP_MIN_LEVELS,
            deplete_fraction: DEFAULT_DEPLETE_FRACTION,
            recover_fraction: DEFAULT_RECOVER_FRACTION,
        }
    }
}

/// What one joined observation produced for its venue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MicrostructureOutcome {
    /// A joined book observation: the realized-spread measurements its event
    /// time resolved, the re-evaluated Kyle-lambda drift, and the
    /// book-resilience reading.
    Book {
        venue: Venue,
        receipt_ns: u64,
        resolved: Vec<RealizedSpread>,
        kyle: KyleOutcome,
        resilience: ResilienceReading,
    },
    /// A joined trade batch: the sweep verdict for the batch as a whole and the
    /// per-trade effective-spread readings in admission order.
    Batch {
        venue: Venue,
        receipt_ns: u64,
        sweep: SweepReading,
        trades: Vec<TradeObservation>,
    },
    /// The observation was refused by the staleness gate and never reached the
    /// computations. `reason` is an existing instance-status reason from the
    /// frozen stream-v3 vocabulary, so a not-ready publication needs no new
    /// term.
    NotReady {
        venue: Venue,
        receipt_ns: u64,
        reason: &'static str,
        /// Whether the gate RESET this venue's four computations rather than
        /// merely withholding the observation from them.
        ///
        /// A stale prevailing book on a BOOK observation is a causal gap, so
        /// the venue's retained state is discarded (see [`Self::apply`]); a
        /// batch with no fresh book reaches no computation but disturbs none
        /// of their retained state either. A consumer that republishes the
        /// last reading of each computation must drop those readings exactly
        /// when this is true, and this flag is the one authority for that
        /// fact — nothing downstream re-derives it from the reason string.
        reset: bool,
    },
}

/// The instance-status reason a staleness-gated observation publishes. It is an
/// existing entry of `signals_core::validation::INSTANCE_STATUS_REASONS`, and
/// covers both halves of the gate: a retained book past the bound, and a venue
/// with no retained book at all.
pub const BOOK_STALE_REASON: &str = "book_snapshot_stale_or_missing";

impl MicrostructureOutcome {
    #[must_use]
    pub fn venue(&self) -> Venue {
        match self {
            Self::Book { venue, .. } | Self::Batch { venue, .. } | Self::NotReady { venue, .. } => {
                *venue
            }
        }
    }

    #[must_use]
    pub fn receipt_ns(&self) -> u64 {
        match self {
            Self::Book { receipt_ns, .. }
            | Self::Batch { receipt_ns, .. }
            | Self::NotReady { receipt_ns, .. } => *receipt_ns,
        }
    }
}

/// A restore point for ONE venue: that venue's slice of the shared join buffer
/// and its four computations. Taken before a step that can refuse part way
/// through, and written back on refusal.
///
/// It is per venue because every mutation the driver performs is per venue —
/// each computation is same-venue and the join's two venues are independent
/// (join invariant 2) — so a step that touches Coinbase never needs to copy
/// Kraken. That is what bounds the restore cost of a step to the state it can
/// actually disturb.
#[derive(Debug, Clone, PartialEq, Eq)]
struct VenueRestore {
    venue: Venue,
    join: VenueJoinSnapshot,
    candidates: VenueCandidates,
}

/// One venue's four computations.
#[derive(Debug, Clone, PartialEq, Eq)]
struct VenueCandidates {
    effective_spread: EffectiveSpreadCandidate,
    kyle: KyleLambdaDriftCandidate,
    sweep: SweepDetectionCandidate,
    resilience: BookResilienceCandidate,
}

impl VenueCandidates {
    fn new(params: MicrostructureParams) -> Result<Self, SpotSignalError> {
        Ok(Self {
            effective_spread: EffectiveSpreadCandidate::new(params.realized_horizon_ms),
            kyle: KyleLambdaDriftCandidate::new(params.window_trades),
            sweep: SweepDetectionCandidate::new(params.sweep_min_levels),
            resilience: BookResilienceCandidate::new(
                params.deplete_fraction,
                params.recover_fraction,
            )?,
        })
    }
}

/// The four spot-microstructure computations driven per venue over one shared
/// [`SpotEventJoin`].
///
/// Every draft formula is same-venue — a trade is priced against that venue's
/// prevailing midpoint, a sweep is one venue's batch, a depletion compares one
/// venue's consecutive top-of-book updates — so each venue owns an independent
/// instance of all four computations and the two venues never share state. The
/// join supplies the ordering, atomicity, bound and staleness rules (see
/// [`crate::spot_event_join`]); this type adds the per-venue fan-out, the fixed
/// within-observation evaluation order, and the ENFORCEMENT of the join's
/// staleness classification — no observation whose prevailing book has gone
/// stale reaches a computation (see [`Self::apply`]).
///
/// Each per-venue lane in [`crate::spot_signals`] owns one of these and drives
/// only its own venue; the peer partition is never fed and stays at its initial
/// state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateMicrostructureSet {
    params: MicrostructureParams,
    join: SpotEventJoin,
    coinbase: VenueCandidates,
    kraken: VenueCandidates,
    /// Per-venue computations for the rights-gated venues (B1). A single-venue
    /// lane owns its own set and drives exactly one entry; the spot lane never
    /// touches these, so its behaviour is unchanged.
    extra: BTreeMap<Venue, VenueCandidates>,
    /// A fresh, never-fed set returned by [`Self::venue`] for a rights-gated
    /// venue not yet observed, so the read accessor stays infallible; also the
    /// template cloned when a venue's `extra` entry is first created.
    empty: VenueCandidates,
}

impl CandidateMicrostructureSet {
    /// Constructs with explicit parameters. Rejects degenerate resilience
    /// fractions exactly as [`BookResilienceCandidate::new`] does.
    pub fn new(params: MicrostructureParams) -> Result<Self, SpotSignalError> {
        Ok(Self {
            params,
            join: SpotEventJoin::new(),
            coinbase: VenueCandidates::new(params)?,
            kraken: VenueCandidates::new(params)?,
            extra: BTreeMap::new(),
            empty: VenueCandidates::new(params)?,
        })
    }

    /// Constructs with every documented default parameter.
    #[must_use]
    pub fn with_defaults() -> Self {
        Self::new(MicrostructureParams::default())
            .expect("default microstructure parameters are valid")
    }

    /// The parameters this driver was constructed with. Echoed as output fields
    /// by every published reading, so the publisher reads them from here rather
    /// than keeping a second copy that could drift.
    #[must_use]
    pub fn params(&self) -> MicrostructureParams {
        self.params
    }

    /// Read-only access to the underlying join, for staleness and open-batch
    /// checks at an evaluation boundary.
    #[must_use]
    pub fn join(&self) -> &SpotEventJoin {
        &self.join
    }

    /// Realized-spread measurements one venue holds awaiting their horizon.
    /// Bounded by [`MAX_PENDING_TRADES`].
    #[must_use]
    pub fn pending_realized_len(&self, venue: Venue) -> usize {
        self.venue(venue).effective_spread.pending_len()
    }

    /// Trades one venue's Kyle estimator holds awaiting a forward midpoint.
    /// Bounded by [`MAX_KYLE_PAIRS`].
    #[must_use]
    pub fn pending_kyle_len(&self, venue: Venue) -> usize {
        self.venue(venue).kyle.pending_len()
    }

    fn venue(&self, venue: Venue) -> &VenueCandidates {
        match venue {
            Venue::Coinbase => &self.coinbase,
            Venue::Kraken => &self.kraken,
            other => self.extra.get(&other).unwrap_or(&self.empty),
        }
    }

    fn venue_mut(&mut self, venue: Venue) -> &mut VenueCandidates {
        match venue {
            Venue::Coinbase => &mut self.coinbase,
            Venue::Kraken => &mut self.kraken,
            other => {
                if !self.extra.contains_key(&other) {
                    let fresh = self.empty.clone();
                    self.extra.insert(other, fresh);
                }
                self.extra
                    .get_mut(&other)
                    .expect("the rights-gated venue entry was just inserted")
            }
        }
    }

    /// Join a completed same-venue book group and run the computations over
    /// every observation it produced (an open trade batch closes first).
    ///
    /// Whole or nothing: a book observation always reaches the computations, so
    /// the step runs under a restore point and a refusal leaves the driver
    /// exactly as it was.
    pub fn observe_book(
        &mut self,
        venue: Venue,
        receipt_ns: u64,
        bid: Decimal,
        bid_size: Decimal,
        ask: Decimal,
        ask_size: Decimal,
    ) -> Result<Vec<MicrostructureOutcome>, SpotSignalError> {
        self.transact_venue(venue, |driver| {
            let observations = driver
                .join
                .observe_book(venue, receipt_ns, bid, bid_size, ask, ask_size)?;
            driver.apply_all(observations)
        })
    }

    /// Join one admitted trade row and run the computations over any
    /// observation it closed. A trade that merely extends the open batch
    /// produces no outcome until that batch closes.
    ///
    /// Whole or nothing. A trade that closes the venue's open batch feeds that
    /// batch to the computations, so it runs under a restore point. A trade
    /// that opens or extends a batch runs no computation at all: the join's own
    /// refusals already leave the venue untouched (join invariants 3 and 5), so
    /// that path — the per-trade path — takes no restore point and stays
    /// allocation-free. [`SpotEventJoin::trade_closes_open_batch`] is the single
    /// authority that separates the two.
    pub fn observe_trade(
        &mut self,
        venue: Venue,
        receipt_ns: u64,
        event_time: &str,
        price: Decimal,
        size: Decimal,
        aggressor: AggressorSide,
    ) -> Result<Vec<MicrostructureOutcome>, SpotSignalError> {
        if !self
            .join
            .trade_closes_open_batch(venue, receipt_ns, event_time)
        {
            let observations = self
                .join
                .observe_trade(venue, receipt_ns, event_time, price, size, aggressor)?;
            debug_assert!(
                observations.is_empty(),
                "a trade that closes no batch emits no observation"
            );
            return self.apply_all(observations);
        }
        self.transact_venue(venue, |driver| {
            let observations = driver
                .join
                .observe_trade(venue, receipt_ns, event_time, price, size, aggressor)?;
            driver.apply_all(observations)
        })
    }

    /// Close every open batch in the join's fixed venue order and run the
    /// computations over them.
    ///
    /// Whole or nothing across BOTH venues: one venue's refusal restores the
    /// other venue's flush too, so a retry replays the same observations.
    pub fn flush(&mut self) -> Result<Vec<MicrostructureOutcome>, SpotSignalError> {
        self.transact_all(|driver| {
            let observations = driver.join.flush();
            driver.apply_all(observations)
        })
    }

    /// Return one venue to its initial state — both the join buffer and the
    /// four computations. Used when the processor marks a venue unavailable or
    /// a reconnect breaks causal continuity: state from before the gap must not
    /// be folded into observations after it.
    pub fn clear_venue(&mut self, venue: Venue) -> Result<(), SpotSignalError> {
        self.transact_venue(venue, |driver| {
            driver.join.clear_venue(venue);
            *driver.venue_mut(venue) = VenueCandidates::new(driver.params)?;
            Ok(())
        })
    }

    fn restore_point(&self, venue: Venue) -> VenueRestore {
        VenueRestore {
            venue,
            join: self.join.snapshot_venue(venue),
            candidates: self.venue(venue).clone(),
        }
    }

    fn roll_back(&mut self, restore: VenueRestore) {
        self.join.restore_venue(restore.join);
        *self.venue_mut(restore.venue) = restore.candidates;
    }

    /// Run one join-plus-computation step against ONE venue as a transaction.
    ///
    /// The driver's mutable state is the join buffer and the two venues'
    /// computations (`params` is `Copy` configuration and never changes). A
    /// step touches several of those in sequence and can refuse part way
    /// through — the join has already released a closed batch by the time the
    /// computations see it, `sweep` has already advanced when the per-trade
    /// loop starts, and the loop mutates `effective_spread` and `kyle` one
    /// trade at a time. Restoring that venue's whole state on refusal is what
    /// keeps "a joined trade batch is applied whole or not at all" true across
    /// all four computations, instead of leaving them mutually inconsistent
    /// with a batch that no retry can recover.
    ///
    /// A refusal restores the join too, so the batch that refused stays OPEN:
    /// closing it again refuses again, deterministically, rather than silently
    /// half-applying. The disposition for a batch that cannot be applied is
    /// [`Self::clear_venue`] — the same venue reset a reconnect or an
    /// unavailable venue takes — which is what keeps a refusal from being a
    /// wedge.
    ///
    /// The restore point is a clone of the affected venue's retained state, so
    /// a step that reaches the computations costs one copy of THAT venue —
    /// never the other one, which the step cannot have touched. Copying rather
    /// than deriving what a step can refuse is deliberate: a second,
    /// hand-written refusal predicate would be a second authority for a fact
    /// the computations already own (STANDING-PRACTICES rule 6). The copy is
    /// bounded by the same [`MAX_PENDING_TRADES`]/[`MAX_KYLE_PAIRS`] caps the
    /// computations are — and the staleness gate in [`Self::apply`] is what
    /// keeps a stalled venue from driving those caps upward — while the
    /// per-trade path in [`Self::observe_trade`] skips the copy entirely.
    fn transact_venue<T>(
        &mut self,
        venue: Venue,
        step: impl FnOnce(&mut Self) -> Result<T, SpotSignalError>,
    ) -> Result<T, SpotSignalError> {
        let restore = self.restore_point(venue);
        step(self).inspect_err(|_| self.roll_back(restore))
    }

    /// The same transaction across every venue, for a step that can touch both.
    /// Only [`Self::flush`] needs it, because only a flush spans venues.
    fn transact_all<T>(
        &mut self,
        step: impl FnOnce(&mut Self) -> Result<T, SpotSignalError>,
    ) -> Result<T, SpotSignalError> {
        let restores = [
            self.restore_point(Venue::Coinbase),
            self.restore_point(Venue::Kraken),
        ];
        step(self).inspect_err(move |_| {
            for restore in restores {
                self.roll_back(restore);
            }
        })
    }

    fn apply_all(
        &mut self,
        observations: Vec<JoinedObservation>,
    ) -> Result<Vec<MicrostructureOutcome>, SpotSignalError> {
        observations
            .into_iter()
            .map(|observation| self.apply(observation))
            .collect()
    }

    /// The fixed within-observation evaluation order. It is the same on every
    /// run, so a replayed observation stream produces an identical outcome
    /// stream.
    ///
    /// This is deliberately NOT self-atomic: the batch arm advances `sweep` for
    /// the whole batch, then `effective_spread` and `kyle` one trade at a time,
    /// and any of those can refuse. Whole-or-nothing is owned one level up by
    /// [`Self::transact_venue`], which restores that venue's entire retained
    /// state — including its slice of the join, which has already released the
    /// batch by the time this runs. Every caller of this function reaches it
    /// through [`Self::apply_all`] inside a transaction.
    ///
    /// # The staleness gate
    ///
    /// Both arms are gated on the [`PrevailingBook`] the join stamped on the
    /// observation (join invariant 6), which is the state of that venue's
    /// retained book at the observation's own receipt time — for a book
    /// observation, its predecessor, stamped before it was replaced. Neither
    /// arm re-derives the bound; [`SpotEventJoin::book_fresh`] remains the one
    /// authority.
    ///
    /// - A **batch** needs a prevailing midpoint to be priced against at all,
    ///   so anything but [`PrevailingBook::Fresh`] publishes
    ///   [`BOOK_STALE_REASON`] and reaches none of the four computations. The
    ///   sweep verdict is withheld with the rest: the draft declares the
    ///   book-levels sources among its inputs, so a venue whose book is stale
    ///   is degraded for the whole microstructure family, not selectively.
    ///   The gate also drains `kyle`'s unpaired trades, whose forward midpoint
    ///   can no longer arrive inside the bound — which is what stops a stalled
    ///   book with live trades from growing pending state toward
    ///   [`MAX_KYLE_PAIRS`].
    /// - A **book** needs a predecessor only to be comparable with, so
    ///   [`PrevailingBook::Missing`] is an ordinary cold start and is applied:
    ///   each computation already has its own warming-up disposition for a
    ///   first observation. [`PrevailingBook::Stale`] is a gap, and a gap is a
    ///   causal discontinuity — the same fact [`Self::clear_venue`] encodes —
    ///   so the venue's computations are reset and this book is withheld from
    ///   them. Resetting rather than merely skipping is what stops the NEXT
    ///   book from being compared against a pre-gap top-of-book, which is the
    ///   comparison the gate exists to prevent.
    fn apply(
        &mut self,
        observation: JoinedObservation,
    ) -> Result<MicrostructureOutcome, SpotSignalError> {
        match observation {
            JoinedObservation::Book(book) if book.prevailing == PrevailingBook::Stale => {
                *self.venue_mut(book.venue) = VenueCandidates::new(self.params)?;
                Ok(MicrostructureOutcome::NotReady {
                    venue: book.venue,
                    receipt_ns: book.receipt_ns,
                    reason: BOOK_STALE_REASON,
                    reset: true,
                })
            }
            JoinedObservation::Batch(batch) if batch.prevailing != PrevailingBook::Fresh => {
                self.venue_mut(batch.venue).kyle.drain_pending();
                Ok(MicrostructureOutcome::NotReady {
                    venue: batch.venue,
                    receipt_ns: batch.receipt_ns,
                    reason: BOOK_STALE_REASON,
                    reset: false,
                })
            }
            JoinedObservation::Book(book) => {
                let candidates = self.venue_mut(book.venue);
                let resolved = candidates
                    .effective_spread
                    .observe_book(book.receipt_ns, book.midpoint)?;
                let kyle = candidates
                    .kyle
                    .observe_book(book.receipt_ns, book.midpoint)?;
                let resilience = candidates
                    .resilience
                    .observe_book(book.receipt_ns, BookTop::new(book.bid_size, book.ask_size))?;
                Ok(MicrostructureOutcome::Book {
                    venue: book.venue,
                    receipt_ns: book.receipt_ns,
                    resolved,
                    kyle,
                    resilience,
                })
            }
            JoinedObservation::Batch(batch) => {
                let receipt_ns = batch.receipt_ns;
                let venue = batch.venue;
                let sweep_trades = batch
                    .trades
                    .iter()
                    .map(|trade| SweepBatchTrade {
                        price: trade.price,
                        size: trade.size,
                        aggressor: trade.aggressor,
                    })
                    .collect::<Vec<_>>();
                let candidates = self.venue_mut(venue);
                let sweep = candidates.sweep.observe_batch(receipt_ns, &sweep_trades)?;
                let mut trades = Vec::with_capacity(batch.trades.len());
                for trade in &batch.trades {
                    trades.push(candidates.effective_spread.observe_trade(
                        receipt_ns,
                        trade.price,
                        trade.aggressor,
                    )?);
                    // The Kyle pair only completes at the next midpoint update,
                    // so the per-trade outcome here is always the not-ready
                    // `awaiting_forward_midpoint`; the informative reading is
                    // the one the following book observation returns.
                    candidates
                        .kyle
                        .observe_trade(receipt_ns, trade.signed_volume()?)?;
                }
                Ok(MicrostructureOutcome::Batch {
                    venue,
                    receipt_ns,
                    sweep,
                    trades,
                })
            }
        }
    }
}
