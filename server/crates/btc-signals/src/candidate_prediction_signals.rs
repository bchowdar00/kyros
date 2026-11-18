//! Prediction-venue computations that entered the surface from the candidate
//! set, plus the early-move fade context.
//!
//! Four families: fee-adjusted pair-cost (per venue, each with its own verified
//! fee model), venue repricing lag versus the spot reference (per venue),
//! tie-rule context (per venue), and an early-move fade context derived from the
//! spot momentum/absorption pair.
//!
//! All seven identities are ADOPTED (2026-09-01): their definitions live in
//! `definitions/btc/signals/`, `signals validate` counts them, and the
//! processors wire them — the six venue identities through
//! [`crate::prediction_signals`] and the fade context through
//! [`crate::spot_signals`]. The computations stay in this module; adoption added
//! the definition binding, the emitter wiring, and the published-field
//! accessors, not new arithmetic. Candidates that remain unadopted are listed in
//! `definitions/btc/candidates/README.md`.
//!
//! The computations follow the adopted idioms: const signal IDs, deterministic
//! state driven only by event timestamps (no wall-clock reads), and exact
//! `rust_decimal` arithmetic through the shared checked helpers in
//! [`crate::spot_signals`] and the single fee authority in `signals-core`. No
//! binary floating point enters any path. Each computation carries an explicit
//! readiness/not-ready disposition rather than fabricating a value. Inputs are
//! already-derived primitives from the declared upstream signals; the processors
//! join the upstream state into those primitives.

use std::collections::VecDeque;

use rust_decimal::Decimal;
use signals_core::{checked_align_fee_up, checked_rate_times_probability_product_fee, round_fee};

use crate::spot_signals::{SpotSignalError, checked_add, return_bps};

/// Taker-fee rounding modes. Re-exported from the shared fee authority in
/// `signals-core`, which `btc-benchmark`'s label economics also calls, so the
/// two charging paths cannot diverge (STANDING-PRACTICES rule 6).
pub use signals_core::FeeRounding;

/// Shared version for every identity computed here.
pub const CANDIDATE_PREDICTION_SIGNAL_VERSION: &str = "1.0.0";

pub const POLYMARKET_PAIR_COST_STATE_ID: &str = "btc.polymarket.pair_cost_state";
pub const KALSHI_PAIR_COST_STATE_ID: &str = "btc.kalshi.pair_cost_state";
pub const POLYMARKET_VENUE_REPRICING_LAG_ID: &str = "btc.polymarket.venue_repricing_lag";
pub const KALSHI_VENUE_REPRICING_LAG_ID: &str = "btc.kalshi.venue_repricing_lag";
pub const POLYMARKET_TIE_RULE_CONTEXT_ID: &str = "btc.polymarket.tie_rule_context";
pub const KALSHI_TIE_RULE_CONTEXT_ID: &str = "btc.kalshi.tie_rule_context";
pub const EARLY_MOVE_FADE_CONTEXT_ID: &str = "btc.external.early_move_fade_context";

/// Default per-share taker-fee rate for the pair-cost fee model (mirrors the
/// authority's documented example); echoed by the published `fee_rate` field.
pub const DEFAULT_POLYMARKET_FEE_RATE: Decimal = Decimal::from_parts(7, 0, 0, false, 2);
/// Default premium threshold (locked-cost band above breakeven); echoed by the
/// published `premium_threshold` field.
pub const DEFAULT_PREMIUM_THRESHOLD: Decimal = Decimal::from_parts(1, 0, 0, false, 2);
/// Default qualifying spot-move magnitude in basis points; echoed by the published
/// `move_threshold_bps` field. A research default: the qualifying move is the
/// simple return measured over [`REPRICING_MOVE_LOOKBACK_NS`] (below), not
/// between consecutive samples, so a real move spread across the many sub-second
/// decision-clock samples is captured rather than lost.
pub const DEFAULT_MOVE_THRESHOLD_BPS: Decimal = Decimal::from_parts(10, 0, 0, false, 0);

/// Rolling lookback over which `venue_repricing_lag` measures the qualifying spot
/// move (5 seconds). A research default. The move is the simple return between
/// the current spot reference and the oldest sample still within this window;
/// samples older than the window are evicted. Measuring over a fixed time window
/// rather than between consecutive decision-clock samples is the E53 fix: the
/// consecutive-sample return maxed at ~1 bps on real data, so a 10 bps threshold
/// could never qualify.
pub const REPRICING_MOVE_LOOKBACK_NS: u64 = 5 * 1_000_000_000;
/// Default near-tie moneyness band; echoed by the published `near_tie_threshold`
/// field.
pub const DEFAULT_NEAR_TIE_THRESHOLD: Decimal = Decimal::from_parts(1, 0, 0, false, 1);
/// Default outsized-move magnitude in basis points; echoed by the published
/// `big_move_bps` field. A research default, applied by `early_move_fade_context`
/// to the thirty-second return (the 30 s lookback is itself a research default).
pub const DEFAULT_BIG_MOVE_BPS: Decimal = Decimal::from_parts(20, 0, 0, false, 0);

const NANOS_PER_MILLI: u64 = 1_000_000;

/// Local exact subtraction, mirroring the module-private helper in
/// [`crate::spot_signals`] so this module stays inside the same checked-decimal
/// vocabulary rather than reaching for a raw operator.
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

// ---------------------------------------------------------------------------
// 1. pair_cost_state (Polymarket)
// ---------------------------------------------------------------------------

/// One leg's taker fee for a single contract under the shared authority's
/// share-count probability-product schedule: `rate * price * (1 - price)`,
/// rounded.
fn leg_fee(rate: Decimal, ask: Decimal, rounding: FeeRounding) -> Result<Decimal, SpotSignalError> {
    let raw = checked_rate_times_probability_product_fee(Decimal::ONE, rate, ask)
        .ok_or_else(|| SpotSignalError::Arithmetic("leg fee overflowed".to_owned()))?;
    Ok(round_fee(raw, rounding))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairCostState {
    LockedProfit,
    Fair,
    Premium,
}

impl PairCostState {
    /// The published `state` enum variant for this band.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LockedProfit => "locked_profit",
            Self::Fair => "fair",
            Self::Premium => "premium",
        }
    }
}

/// Ready pair-cost reading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairCost {
    pub pair_cost: Decimal,
    pub fee_adjusted_pair_cost: Decimal,
    pub up_leg_fee: Decimal,
    pub down_leg_fee: Decimal,
    pub state: PairCostState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairCostOutcome {
    Ready(PairCost),
    NotReady { reason: &'static str },
}

/// Two-sided pair-cost computation (`btc.polymarket.pair_cost_state`).
///
/// From the market's two native outcome asks, `pair_cost = up_ask + down_ask - 1`
/// is the cost of locking a $1 payout (negative is gross locked profit).
/// `fee_adjusted_pair_cost` adds the per-share taker fee on both legs. State is
/// `locked_profit` when the fee-adjusted cost is negative, `premium` when it
/// exceeds `premium_threshold`, and `fair` otherwise. A missing outcome ask is a
/// defined not-ready state. The Kalshi variant with its own verified fee
/// model is [`KalshiPairCostStateCandidate`].
#[derive(Debug, Clone)]
pub struct PairCostStateCandidate {
    fee_rate: Decimal,
    fee_rounding: FeeRounding,
    premium_threshold: Decimal,
}

impl PairCostStateCandidate {
    #[must_use]
    pub fn new(fee_rate: Decimal, fee_rounding: FeeRounding, premium_threshold: Decimal) -> Self {
        Self {
            fee_rate,
            fee_rounding,
            premium_threshold,
        }
    }

    /// Constructs with the documented default fee rate, ceil-cent rounding, and
    /// premium threshold.
    #[must_use]
    pub fn with_defaults() -> Self {
        Self::new(
            DEFAULT_POLYMARKET_FEE_RATE,
            FeeRounding::CeilCent,
            DEFAULT_PREMIUM_THRESHOLD,
        )
    }

    /// The configured per-share taker-fee rate, echoed by the published
    /// `fee_rate` field. Reading the charging parameter back from the compute
    /// keeps one authority for it (STANDING-PRACTICES rule 6).
    #[must_use]
    pub fn fee_rate(&self) -> Decimal {
        self.fee_rate
    }

    /// The configured rounding mode, echoed by the published `fee_rounding`
    /// field.
    #[must_use]
    pub fn fee_rounding(&self) -> FeeRounding {
        self.fee_rounding
    }

    /// The configured premium band, echoed by the published `premium_threshold`
    /// field.
    #[must_use]
    pub fn premium_threshold(&self) -> Decimal {
        self.premium_threshold
    }

    /// The per-contract taker fee this model charges at `price`.
    ///
    /// The same `leg_fee` the pair cost adds to each leg, exposed so
    /// `expected_value` charges the identical fee rather than restating the
    /// schedule (STANDING-PRACTICES rule 6).
    pub fn contract_fee(&self, price: Decimal) -> Result<Decimal, SpotSignalError> {
        if price < Decimal::ZERO || price > Decimal::ONE {
            return Err(SpotSignalError::MarketValue(
                "outcome price must be a probability in [0, 1]".to_owned(),
            ));
        }
        leg_fee(self.fee_rate, price, self.fee_rounding)
    }

    pub fn evaluate(
        &self,
        up_ask: Option<Decimal>,
        down_ask: Option<Decimal>,
    ) -> Result<PairCostOutcome, SpotSignalError> {
        let (Some(up_ask), Some(down_ask)) = (up_ask, down_ask) else {
            return Ok(PairCostOutcome::NotReady {
                reason: "missing_outcome_ask",
            });
        };
        for ask in [up_ask, down_ask] {
            if ask < Decimal::ZERO || ask > Decimal::ONE {
                return Err(SpotSignalError::MarketValue(
                    "outcome ask must be a probability in [0, 1]".to_owned(),
                ));
            }
        }
        let pair_cost = checked_sub(checked_add(up_ask, down_ask)?, Decimal::ONE)?;
        let up_leg_fee = leg_fee(self.fee_rate, up_ask, self.fee_rounding)?;
        let down_leg_fee = leg_fee(self.fee_rate, down_ask, self.fee_rounding)?;
        let fee_adjusted_pair_cost =
            checked_add(checked_add(pair_cost, up_leg_fee)?, down_leg_fee)?;
        let state = if fee_adjusted_pair_cost < Decimal::ZERO {
            PairCostState::LockedProfit
        } else if fee_adjusted_pair_cost > self.premium_threshold {
            PairCostState::Premium
        } else {
            PairCostState::Fair
        };
        Ok(PairCostOutcome::Ready(PairCost {
            pair_cost,
            fee_adjusted_pair_cost,
            up_leg_fee,
            down_leg_fee,
            state,
        }))
    }
}

// ---------------------------------------------------------------------------
// 1b. pair_cost_state (Kalshi)
// ---------------------------------------------------------------------------

/// Default Kalshi per-share taker-fee rate; echoed by the published `fee_rate`
/// field. From Kalshi's official fee schedule (effective 2026-07-07).
pub const DEFAULT_KALSHI_FEE_RATE: Decimal = Decimal::from_parts(7, 0, 0, false, 2);
/// Default fee alignment increment (non-direct/retail member, $0.01); echoed by
/// the published `fee_alignment` field. The direct-member tier is $0.0001.
pub const DEFAULT_FEE_ALIGNMENT: Decimal = Decimal::from_parts(1, 0, 0, false, 2);

/// Kalshi taker fee for a single contract at price `p`, rounded up to the fee
/// alignment increment. Mirrors the official schedule's
/// `round_up(0.07 * P * (1 - P))` where the round-up lands the fee on the
/// alignment precision ($0.01 for non-direct members via Kalshi's rounding-fee
/// ledger, $0.0001 for direct members).
fn kalshi_leg_fee(
    rate: Decimal,
    price: Decimal,
    alignment: Decimal,
) -> Result<Decimal, SpotSignalError> {
    let raw = checked_rate_times_probability_product_fee(Decimal::ONE, rate, price)
        .ok_or_else(|| SpotSignalError::Arithmetic("leg fee overflowed".to_owned()))?;
    checked_align_fee_up(raw, alignment)
        .ok_or_else(|| SpotSignalError::Arithmetic("fee alignment overflowed".to_owned()))
}

/// Kalshi two-sided pair-cost reading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KalshiPairCost {
    pub pair_cost: Decimal,
    pub fee_adjusted_pair_cost: Decimal,
    pub yes_leg_fee: Decimal,
    pub no_leg_fee: Decimal,
    pub state: PairCostState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KalshiPairCostOutcome {
    Ready(KalshiPairCost),
    NotReady { reason: &'static str },
}

/// Kalshi two-sided pair-cost computation (`btc.kalshi.pair_cost_state`).
///
/// Kalshi quotes a single binary YES book. Locking a $1 payout means buying YES
/// at `yes_ask` and buying NO at `no_ask`, where `no_ask = 1 - yes_bid` crosses
/// the other side of the same book. So `pair_cost = yes_ask + no_ask - 1 =
/// yes_ask - yes_bid` — the YES spread; pair-locking on one binary book is spread
/// capture. `fee_adjusted_pair_cost` adds the taker fee on both legs. The fee is
/// the conservative taker-taker case: resting one side pays the maker rate, which
/// defaults to zero for the BTC series under the current schedule, so both legs
/// crossing is the worst case. There is no settlement fee, so holding both sides
/// to expiry costs exactly the two entry fees. State is `locked_profit` when the
/// fee-adjusted cost is negative, `premium` when it exceeds `premium_threshold`,
/// and `fair` otherwise — the same bands as the Polymarket variant. A missing
/// side of the book is a defined not-ready state.
#[derive(Debug, Clone)]
pub struct KalshiPairCostStateCandidate {
    fee_rate: Decimal,
    fee_alignment: Decimal,
    premium_threshold: Decimal,
}

impl KalshiPairCostStateCandidate {
    /// Rejects a non-positive fee alignment (it is a rounding divisor).
    pub fn new(
        fee_rate: Decimal,
        fee_alignment: Decimal,
        premium_threshold: Decimal,
    ) -> Result<Self, SpotSignalError> {
        if fee_alignment <= Decimal::ZERO {
            return Err(SpotSignalError::MarketValue(
                "fee alignment must be positive".to_owned(),
            ));
        }
        Ok(Self {
            fee_rate,
            fee_alignment,
            premium_threshold,
        })
    }

    /// Constructs with the documented default fee rate, retail alignment, and
    /// premium threshold (shared with the Polymarket variant).
    #[must_use]
    pub fn with_defaults() -> Self {
        Self::new(
            DEFAULT_KALSHI_FEE_RATE,
            DEFAULT_FEE_ALIGNMENT,
            DEFAULT_PREMIUM_THRESHOLD,
        )
        .expect("default fee alignment is positive")
    }

    /// The configured per-contract taker-fee rate, echoed by the published
    /// `fee_rate` field.
    #[must_use]
    pub fn fee_rate(&self) -> Decimal {
        self.fee_rate
    }

    /// The configured member-tier charging increment, echoed by the published
    /// `fee_alignment` field.
    #[must_use]
    pub fn fee_alignment(&self) -> Decimal {
        self.fee_alignment
    }

    /// The configured premium band, echoed by the published `premium_threshold`
    /// field.
    #[must_use]
    pub fn premium_threshold(&self) -> Decimal {
        self.premium_threshold
    }

    /// The per-contract taker fee this model charges at `price`.
    ///
    /// The same `kalshi_leg_fee` the pair cost adds to each leg, exposed so
    /// `expected_value` charges the identical fee rather than restating the
    /// official schedule (STANDING-PRACTICES rule 6).
    pub fn contract_fee(&self, price: Decimal) -> Result<Decimal, SpotSignalError> {
        if price < Decimal::ZERO || price > Decimal::ONE {
            return Err(SpotSignalError::MarketValue(
                "outcome price must be a probability in [0, 1]".to_owned(),
            ));
        }
        kalshi_leg_fee(self.fee_rate, price, self.fee_alignment)
    }

    pub fn evaluate(
        &self,
        yes_ask: Option<Decimal>,
        no_ask: Option<Decimal>,
    ) -> Result<KalshiPairCostOutcome, SpotSignalError> {
        let (Some(yes_ask), Some(no_ask)) = (yes_ask, no_ask) else {
            return Ok(KalshiPairCostOutcome::NotReady {
                reason: "missing_outcome_ask",
            });
        };
        for ask in [yes_ask, no_ask] {
            if ask < Decimal::ZERO || ask > Decimal::ONE {
                return Err(SpotSignalError::MarketValue(
                    "outcome ask must be a probability in [0, 1]".to_owned(),
                ));
            }
        }
        let pair_cost = checked_sub(checked_add(yes_ask, no_ask)?, Decimal::ONE)?;
        let yes_leg_fee = kalshi_leg_fee(self.fee_rate, yes_ask, self.fee_alignment)?;
        let no_leg_fee = kalshi_leg_fee(self.fee_rate, no_ask, self.fee_alignment)?;
        let fee_adjusted_pair_cost = checked_add(checked_add(pair_cost, yes_leg_fee)?, no_leg_fee)?;
        let state = if fee_adjusted_pair_cost < Decimal::ZERO {
            PairCostState::LockedProfit
        } else if fee_adjusted_pair_cost > self.premium_threshold {
            PairCostState::Premium
        } else {
            PairCostState::Fair
        };
        Ok(KalshiPairCostOutcome::Ready(KalshiPairCost {
            pair_cost,
            fee_adjusted_pair_cost,
            yes_leg_fee,
            no_leg_fee,
            state,
        }))
    }
}

// ---------------------------------------------------------------------------
// 2. venue_repricing_lag (Polymarket / Kalshi)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepricingState {
    Current,
    Lagging,
}

impl RepricingState {
    /// The published `state` enum variant for this reading.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Lagging => "lagging",
        }
    }
}

/// Venue repricing-lag reading at one observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RepricingReading {
    pub state: RepricingState,
    /// Most recent completed lag from a qualifying move to the first venue quote
    /// change answering it; `None` until one completes.
    pub repricing_lag_ms: Option<u64>,
    /// Time the currently unanswered qualifying move has waited (now minus the
    /// move's event time); `0` when no move is pending.
    pub staleness_ms: u64,
}

/// Venue repricing-lag computation (`btc.{venue}.venue_repricing_lag`).
///
/// A qualifying spot move is a spot-reference midpoint simple return of at least
/// `move_threshold_bps` measured over a rolling [`REPRICING_MOVE_LOOKBACK_NS`]
/// lookback — the return between the current price and the oldest spot sample
/// still within the window (both research defaults). `repricing_lag_ms` is the
/// time from that move to the first venue quote change — a nonzero change in
/// either outcome midpoint — after it; `staleness_ms` is the time the currently
/// unanswered move has waited (measured from the move itself). State is `lagging`
/// while a move is unanswered and `current` otherwise. A qualifying move is armed
/// on entry into the qualifying state; while a move is unanswered a further
/// qualifying return does NOT re-arm, so the lag measures the venue's response to
/// the first unanswered move (the earlier E53 fix replaced consecutive-sample
/// detection, under which a real move was split into sub-basis-point increments
/// and never qualified). This deliberately does not model the implied magnitude
/// of the venue's response; that is the fair-up-probability lane.
#[derive(Debug, Clone)]
pub struct VenueRepricingLagCandidate {
    move_threshold_bps: Decimal,
    lookback_ns: u64,
    // Spot samples (event_time_ns, price) within the rolling lookback, oldest at
    // the front. The qualifying move is the return of the newest against the
    // oldest retained sample.
    spot_window: VecDeque<(u64, Decimal)>,
    // Whether the window return currently satisfies the qualifying threshold.
    // Arming is edge-triggered on the false -> true transition of this flag, so a
    // window that stays qualifying (e.g. the pre-move sample lingering after an
    // answer) does not re-arm; a re-arm needs the window to clear (flag falls) and
    // re-qualify.
    qualifying: bool,
    // Both outcome midpoints (affirmative, negative): an answer is a change in
    // EITHER, so a one-sided (negative-book-only) reprice is not blind.
    previous_venue_mids: Option<(Decimal, Decimal)>,
    pending_move_ns: Option<u64>,
    last_repricing_lag_ms: Option<u64>,
    last_event_ns: Option<u64>,
}

impl VenueRepricingLagCandidate {
    #[must_use]
    pub fn new(move_threshold_bps: Decimal) -> Self {
        Self {
            move_threshold_bps,
            lookback_ns: REPRICING_MOVE_LOOKBACK_NS,
            spot_window: VecDeque::new(),
            qualifying: false,
            previous_venue_mids: None,
            pending_move_ns: None,
            last_repricing_lag_ms: None,
            last_event_ns: None,
        }
    }

    /// Constructs with the documented default qualifying-move threshold.
    #[must_use]
    pub fn with_default_threshold() -> Self {
        Self::new(DEFAULT_MOVE_THRESHOLD_BPS)
    }

    /// The configured qualifying-move magnitude, echoed by the published
    /// `move_threshold_bps` field.
    #[must_use]
    pub fn move_threshold_bps(&self) -> Decimal {
        self.move_threshold_bps
    }

    /// Whether the fold has received the inputs a measurement needs: at least one
    /// spot sample AND one venue quote. Before both upstreams have produced, a
    /// missing measurement is `upstream_measurement_unavailable`; once both are
    /// present it is `no_qualifying_move` until a move is answered (E53).
    #[must_use]
    pub fn has_measurement_inputs(&self) -> bool {
        !self.spot_window.is_empty() && self.previous_venue_mids.is_some()
    }

    /// Observe a spot-reference midpoint. The qualifying move is the return over
    /// the rolling lookback (current price versus the oldest retained sample); it
    /// arms a pending measurement on entry into the qualifying state (the
    /// false -> true edge of the qualifying flag) and does not re-arm while the
    /// window stays qualifying, so a re-arm needs the window to clear and
    /// re-qualify.
    pub fn observe_spot(
        &mut self,
        event_time_ns: u64,
        reference_price: Decimal,
    ) -> Result<RepricingReading, SpotSignalError> {
        observe_monotonic(&mut self.last_event_ns, event_time_ns)?;
        if reference_price <= Decimal::ZERO {
            return Err(SpotSignalError::MarketValue(
                "spot reference price must be positive".to_owned(),
            ));
        }
        self.spot_window.push_back((event_time_ns, reference_price));
        // Evict samples strictly older than the lookback, so the oldest retained
        // sample sits at or after `event_time_ns - lookback_ns`. A sample exactly
        // at the boundary is kept, so a move spread over exactly the lookback
        // still measures the full window.
        while let Some(&(oldest_ns, _)) = self.spot_window.front() {
            if event_time_ns.saturating_sub(oldest_ns) > self.lookback_ns {
                self.spot_window.pop_front();
            } else {
                break;
            }
        }
        // Arm only on entry into the qualifying state: the false -> true edge of
        // `qualifying`. A window that is still qualifying does not re-arm, so after
        // an answered move the pre-move sample lingering in the window cannot
        // fabricate a fresh move on the next unchanged sample; and a further
        // qualifying return while a move is unanswered keeps the earlier move so
        // the lag measures the venue's response to the first unanswered move. A
        // re-arm therefore requires the window to clear (the flag falls) and
        // re-qualify.
        let qualifying_now = match self.spot_window.front() {
            Some(&(_, oldest_price)) if oldest_price != reference_price => {
                return_bps(reference_price, oldest_price)?.abs() >= self.move_threshold_bps
            }
            _ => false,
        };
        if qualifying_now && !self.qualifying && self.pending_move_ns.is_none() {
            self.pending_move_ns = Some(event_time_ns);
        }
        self.qualifying = qualifying_now;
        Ok(self.reading(event_time_ns))
    }

    /// Observe both outcome midpoints of a venue quote. A change in EITHER
    /// midpoint is a genuine reprice that answers a pending move; an unchanged
    /// two-sided quote does not.
    pub fn observe_venue_quote(
        &mut self,
        event_time_ns: u64,
        affirmative_mid: Decimal,
        negative_mid: Decimal,
    ) -> Result<RepricingReading, SpotSignalError> {
        observe_monotonic(&mut self.last_event_ns, event_time_ns)?;
        if affirmative_mid <= Decimal::ZERO || negative_mid <= Decimal::ZERO {
            return Err(SpotSignalError::MarketValue(
                "venue quote midpoints must be positive".to_owned(),
            ));
        }
        if let Some((previous_affirmative, previous_negative)) = self.previous_venue_mids
            && (affirmative_mid != previous_affirmative || negative_mid != previous_negative)
            && let Some(move_ns) = self.pending_move_ns.take()
        {
            let lag_ns = event_time_ns
                .checked_sub(move_ns)
                .ok_or(SpotSignalError::TimeRegression)?;
            self.last_repricing_lag_ms = Some(lag_ns / NANOS_PER_MILLI);
        }
        self.previous_venue_mids = Some((affirmative_mid, negative_mid));
        Ok(self.reading(event_time_ns))
    }

    /// The current reading at decision time `now_ns`, without folding in a new
    /// observation. `staleness_ms` is measured from the pending move to
    /// `now_ns`, so a caller that emits between observations still publishes a
    /// truthful waiting time.
    #[must_use]
    pub fn reading_at(&self, now_ns: u64) -> RepricingReading {
        self.reading(now_ns)
    }

    fn reading(&self, now_ns: u64) -> RepricingReading {
        let (state, staleness_ms) = match self.pending_move_ns {
            // Staleness is the unanswered move's own waiting time: every venue
            // change would have answered it, so measuring from the move (not the
            // last venue change, which necessarily predates the move) is the
            // promised semantics and never folds in the pre-move idle gap.
            Some(move_ns) => (
                RepricingState::Lagging,
                now_ns.saturating_sub(move_ns) / NANOS_PER_MILLI,
            ),
            None => (RepricingState::Current, 0),
        };
        RepricingReading {
            state,
            repricing_lag_ms: self.last_repricing_lag_ms,
            staleness_ms,
        }
    }
}

// ---------------------------------------------------------------------------
// 3. tie_rule_context (Polymarket / Kalshi)
// ---------------------------------------------------------------------------

/// A market's tie outcome, normalized across venues.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TieOutcome {
    Up,
    Down,
    Void,
}

impl TieOutcome {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Up => "up",
            Self::Down => "down",
            Self::Void => "void",
        }
    }
}

/// Ready tie-rule reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TieRuleContext {
    pub tie_outcome: TieOutcome,
    pub near_tie: bool,
    pub tie_advantage_side: Option<TieOutcome>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TieRuleOutcome {
    Ready(TieRuleContext),
    NotReady { reason: &'static str },
}

/// Tie-rule context computation (`btc.{venue}.tie_rule_context`).
///
/// Consumes the market's tie outcome and standardized moneyness. The tie outcome
/// is normalized to the canonical up/down/void vocabulary before it reaches this
/// compute (venue mapping: Kalshi `yes` -> up, Polymarket `up` -> up; a future
/// venue vocabulary must extend the mapping). `near_tie` is true when the
/// absolute moneyness is at or within `near_tie_threshold`; `tie_advantage_side`
/// is the tie outcome when near a tie and the tie is not a void, otherwise none.
/// Not ready until both inputs have been seen.
#[derive(Debug, Clone)]
pub struct TieRuleContextCandidate {
    near_tie_threshold: Decimal,
    tie_outcome: Option<TieOutcome>,
    moneyness: Option<Decimal>,
}

impl TieRuleContextCandidate {
    #[must_use]
    pub fn new(near_tie_threshold: Decimal) -> Self {
        Self {
            near_tie_threshold,
            tie_outcome: None,
            moneyness: None,
        }
    }

    /// Constructs with the documented default near-tie threshold.
    #[must_use]
    pub fn with_default_threshold() -> Self {
        Self::new(DEFAULT_NEAR_TIE_THRESHOLD)
    }

    /// The configured near-tie band, echoed by the published
    /// `near_tie_threshold` field.
    #[must_use]
    pub fn near_tie_threshold(&self) -> Decimal {
        self.near_tie_threshold
    }

    /// Observe the market rules card's tie outcome.
    pub fn observe_tie_outcome(&mut self, tie_outcome: TieOutcome) -> TieRuleOutcome {
        self.tie_outcome = Some(tie_outcome);
        self.evaluate()
    }

    /// Observe the standardized-moneyness statistic.
    pub fn observe_moneyness(&mut self, moneyness: Decimal) -> TieRuleOutcome {
        self.moneyness = Some(moneyness);
        self.evaluate()
    }

    fn evaluate(&self) -> TieRuleOutcome {
        let (Some(tie_outcome), Some(moneyness)) = (self.tie_outcome, self.moneyness) else {
            return TieRuleOutcome::NotReady {
                reason: "inputs_warming_up",
            };
        };
        let near_tie = moneyness.abs() <= self.near_tie_threshold;
        // The void carve-out is retained for market_contract variants that declare
        // a void tie; it is unreachable from the currently declared upstream
        // variants (Kalshi tie_outcome = [yes], Polymarket = [up]), which normalize
        // only to up.
        let tie_advantage_side = if near_tie && tie_outcome != TieOutcome::Void {
            Some(tie_outcome)
        } else {
            None
        };
        TieRuleOutcome::Ready(TieRuleContext {
            tie_outcome,
            near_tie,
            tie_advantage_side,
        })
    }
}

// ---------------------------------------------------------------------------
// 4. early_move_fade_context (external)
// ---------------------------------------------------------------------------

/// The ten-second momentum direction, mirroring `micro_momentum`'s
/// `direction_10s`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MomentumDirection {
    Up,
    Down,
    Flat,
}

impl MomentumDirection {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Up => "up",
            Self::Down => "down",
            Self::Flat => "flat",
        }
    }
}

/// The absorption state, mirroring `absorption_state`'s `state`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AbsorptionState {
    InsufficientFlow,
    Absorbing,
    PriceResponsive,
    Mixed,
}

impl AbsorptionState {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InsufficientFlow => "insufficient_flow",
            Self::Absorbing => "absorbing",
            Self::PriceResponsive => "price_responsive",
            Self::Mixed => "mixed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FadeState {
    Quiet,
    OutsizedAbsorbing,
    OutsizedResponsive,
    OutsizedUnclear,
}

impl FadeState {
    /// The published `state` enum variant for this reading.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Quiet => "quiet",
            Self::OutsizedAbsorbing => "outsized_absorbing",
            Self::OutsizedResponsive => "outsized_responsive",
            Self::OutsizedUnclear => "outsized_unclear",
        }
    }
}

/// Ready early-move fade reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FadeContext {
    pub state: FadeState,
    pub return_bps_1s: Decimal,
    pub direction_10s: MomentumDirection,
    pub absorption_state: AbsorptionState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FadeOutcome {
    Ready(FadeContext),
    NotReady { reason: &'static str },
}

/// Early-move fade context computation (`btc.external.early_move_fade_context`).
///
/// Pairs the latest momentum (a one-second return kept as published context, a
/// thirty-second return used for the outsized classification, and the
/// ten-second direction) from `micro_momentum` with the latest
/// `absorption_state`. State is `quiet` when the absolute thirty-second return is
/// below `big_move_bps`; on an outsized move it is `outsized_absorbing` when
/// absorption is absorbing (the fade-candidate context), `outsized_responsive`
/// when price-responsive, and `outsized_unclear` for the remaining absorption
/// states. Not ready until both inputs have emitted. E53: the outsized test moved
/// off the one-second return (which maxed at ~5 bps on real data, so the 20 bps
/// `outsized_*` classes were unreachable) to the thirty-second return (both the
/// 30 s lookback and the 20 bps `big_move_bps` are research defaults); readiness
/// is unchanged — the identity is ready whenever both inputs are present.
#[derive(Debug, Clone)]
pub struct EarlyMoveFadeContextCandidate {
    big_move_bps: Decimal,
    // (return_bps_1s published as context, return_bps_30s driving the
    // classification, ten-second direction).
    momentum: Option<(Decimal, Decimal, MomentumDirection)>,
    absorption: Option<AbsorptionState>,
}

impl EarlyMoveFadeContextCandidate {
    #[must_use]
    pub fn new(big_move_bps: Decimal) -> Self {
        Self {
            big_move_bps,
            momentum: None,
            absorption: None,
        }
    }

    /// Constructs with the documented default outsized-move threshold.
    #[must_use]
    pub fn with_default_threshold() -> Self {
        Self::new(DEFAULT_BIG_MOVE_BPS)
    }

    /// The configured outsized-move magnitude, echoed by the published
    /// `big_move_bps` field. A research default, applied to the thirty-second
    /// return.
    #[must_use]
    pub fn big_move_bps(&self) -> Decimal {
        self.big_move_bps
    }

    /// Observe the latest momentum: the one-second return (published as context),
    /// the thirty-second return (drives the outsized classification), and the
    /// ten-second direction.
    pub fn observe_momentum(
        &mut self,
        return_bps_1s: Decimal,
        return_bps_30s: Decimal,
        direction_10s: MomentumDirection,
    ) -> FadeOutcome {
        self.momentum = Some((return_bps_1s, return_bps_30s, direction_10s));
        self.evaluate()
    }

    /// Observe the latest absorption state.
    pub fn observe_absorption(&mut self, absorption_state: AbsorptionState) -> FadeOutcome {
        self.absorption = Some(absorption_state);
        self.evaluate()
    }

    fn evaluate(&self) -> FadeOutcome {
        let (Some((return_bps_1s, return_bps_30s, direction_10s)), Some(absorption_state)) =
            (self.momentum, self.absorption)
        else {
            return FadeOutcome::NotReady {
                reason: "inputs_warming_up",
            };
        };
        let state = if return_bps_30s.abs() < self.big_move_bps {
            FadeState::Quiet
        } else {
            match absorption_state {
                AbsorptionState::Absorbing => FadeState::OutsizedAbsorbing,
                AbsorptionState::PriceResponsive => FadeState::OutsizedResponsive,
                AbsorptionState::InsufficientFlow | AbsorptionState::Mixed => {
                    FadeState::OutsizedUnclear
                }
            }
        };
        FadeOutcome::Ready(FadeContext {
            state,
            return_bps_1s,
            direction_10s,
            absorption_state,
        })
    }
}
