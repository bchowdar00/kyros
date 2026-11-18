//! Settlement-anchor computations from the candidate set.
//!
//! Two families, both adopted on 2026-09-02 (ADR 0023 amendment) and both
//! Polymarket-only: the settlement-anchor state (the resolving Chainlink
//! 60-second TWAP path versus the market's reference level) and the
//! oracle-versus-spot divergence, re-homed from `btc.external.oracle_divergence`
//! to `btc.polymarket.oracle_divergence` because it is computed per bound
//! Polymarket market from that market's settlement anchor. The Polymarket
//! prediction processor owns both wirings; the Kalshi variants stay deferred on
//! rights-gated CF Benchmarks BRTI access.
//!
//! The computations kept the idioms they were reviewed under: const signal IDs,
//! deterministic state driven only by event timestamps (no wall-clock reads),
//! and exact `rust_decimal` arithmetic through the shared checked helpers in
//! [`crate::spot_signals`]. No binary floating point enters any path. Each
//! computation carries an explicit readiness/not-ready disposition rather than
//! fabricating a value. Both signals contextualize an observed anchor path; they
//! do not predict.

use std::cmp::Ordering;

use rust_decimal::Decimal;

use crate::spot_signals::{SpotSignalError, return_bps};

/// Shared version for every identity here.
pub const CANDIDATE_SETTLEMENT_SIGNAL_VERSION: &str = "1.0.0";

pub const POLYMARKET_SETTLEMENT_ANCHOR_STATE_ID: &str = "btc.polymarket.settlement_anchor_state";
pub const POLYMARKET_ORACLE_DIVERGENCE_ID: &str = "btc.polymarket.oracle_divergence";
// The Kalshi settlement twins are the same two computations bound to the CF
// Benchmarks BRTI anchor the venue settles on; they publish only while the
// Kalshi lane's index socket is armed (ruling E43).
pub const KALSHI_SETTLEMENT_ANCHOR_STATE_ID: &str = "btc.kalshi.settlement_anchor_state";
pub const KALSHI_ORACLE_DIVERGENCE_ID: &str = "btc.kalshi.oracle_divergence";

/// Default pin-sensitivity distance band in basis points; echoed by the draft's
/// `pin_threshold_bps` field.
pub const DEFAULT_PIN_THRESHOLD_BPS: Decimal = Decimal::from_parts(10, 0, 0, false, 0);
/// Default pin-sensitivity time window in milliseconds; echoed by the draft's
/// `pin_window_ms` field.
pub const DEFAULT_PIN_WINDOW_MS: u64 = 60000;
/// Default divergence threshold in basis points; echoed by the draft's
/// `diverged_threshold_bps` field.
pub const DEFAULT_DIVERGED_THRESHOLD_BPS: Decimal = Decimal::from_parts(10, 0, 0, false, 0);

const NANOS_PER_MILLI: u64 = 1_000_000;

fn observe_monotonic(last_event_ns: &mut Option<u64>, now_ns: u64) -> Result<(), SpotSignalError> {
    if last_event_ns.is_some_and(|previous| now_ns < previous) {
        return Err(SpotSignalError::TimeRegression);
    }
    *last_event_ns = Some(now_ns);
    Ok(())
}

/// The settled side a contract would resolve to, normalized to the canonical
/// up/down/void vocabulary (Polymarket maps up->up; a future venue vocabulary
/// must extend the mapping).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettledSide {
    Up,
    Down,
    Void,
}

impl SettledSide {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Up => "up",
            Self::Down => "down",
            Self::Void => "void",
        }
    }
}

/// The contract's settlement comparator. Only greater-than-or-equal is declared
/// by the current market_contract surface; the enum is retained so a future
/// comparator vocabulary extends here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Comparator {
    GreaterThanOrEqual,
}

impl Comparator {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::GreaterThanOrEqual => "greater_than_or_equal",
        }
    }

    /// The settled side of `anchor` against `threshold`. A strict comparison
    /// decides up or down; an exact tie defers to the contract's tie outcome.
    fn settled_side(
        self,
        anchor: Decimal,
        threshold: Decimal,
        tie_outcome: SettledSide,
    ) -> SettledSide {
        match self {
            Self::GreaterThanOrEqual => match anchor.cmp(&threshold) {
                Ordering::Greater => SettledSide::Up,
                Ordering::Less => SettledSide::Down,
                Ordering::Equal => tie_outcome,
            },
        }
    }
}

// ---------------------------------------------------------------------------
// 1. settlement_anchor_state (Polymarket)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnchorSettlementState {
    Pinned,
    Tracking,
}

impl AnchorSettlementState {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pinned => "pinned",
            Self::Tracking => "tracking",
        }
    }
}

/// Ready settlement-anchor reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnchorState {
    pub anchor_price: Decimal,
    pub threshold: Decimal,
    pub distance_bps: Decimal,
    pub time_to_close_ms: u64,
    pub side_if_settled_now: SettledSide,
    pub state: AnchorSettlementState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnchorOutcome {
    Ready(AnchorState),
    NotReady { reason: &'static str },
}

#[derive(Debug, Clone, Copy)]
struct SettlementContract {
    threshold: Decimal,
    comparator: Comparator,
    tie_outcome: SettledSide,
    window_close_ns: u64,
}

/// Settlement-anchor state (`btc.polymarket.settlement_anchor_state`).
///
/// Consumes the resolving Chainlink 60-second TWAP (the exact data Polymarket
/// resolves on) and the market's rules card. `distance_bps` is the signed
/// basis-point distance of the anchor from the contract threshold;
/// `time_to_close_ms` is the time remaining to the rules-card close;
/// `side_if_settled_now` applies the contract comparator (ties per the contract's
/// tie outcome). State is `pinned` when the absolute distance is within
/// `pin_threshold_bps` AND the time to close is within `pin_window_ms` — the
/// resolution-sensitivity context — and `tracking` otherwise. Not ready until
/// both inputs are seen, and not ready once the window is closed. Polymarket
/// only. This contextualizes the anchor path; it does no probability or
/// volatility modeling and predicts nothing.
#[derive(Debug, Clone)]
pub struct SettlementAnchorStateCandidate {
    pin_threshold_bps: Decimal,
    pin_window_ms: u64,
    anchor_price: Option<Decimal>,
    contract: Option<SettlementContract>,
    last_event_ns: Option<u64>,
}

impl SettlementAnchorStateCandidate {
    #[must_use]
    pub fn new(pin_threshold_bps: Decimal, pin_window_ms: u64) -> Self {
        Self {
            pin_threshold_bps,
            pin_window_ms,
            anchor_price: None,
            contract: None,
            last_event_ns: None,
        }
    }

    /// Constructs with the documented default pin distance band and time window.
    #[must_use]
    pub fn with_defaults() -> Self {
        Self::new(DEFAULT_PIN_THRESHOLD_BPS, DEFAULT_PIN_WINDOW_MS)
    }

    /// The configured pin distance band, echoed by the published
    /// `pin_threshold_bps` field.
    #[must_use]
    pub fn pin_threshold_bps(&self) -> Decimal {
        self.pin_threshold_bps
    }

    /// The configured pin time window, echoed by the published `pin_window_ms`
    /// field.
    #[must_use]
    pub fn pin_window_ms(&self) -> u64 {
        self.pin_window_ms
    }

    /// Observe the latest resolving TWAP anchor price.
    pub fn observe_anchor(
        &mut self,
        event_time_ns: u64,
        anchor_price: Decimal,
    ) -> Result<AnchorOutcome, SpotSignalError> {
        observe_monotonic(&mut self.last_event_ns, event_time_ns)?;
        if anchor_price <= Decimal::ZERO {
            return Err(SpotSignalError::MarketValue(
                "anchor price must be positive".to_owned(),
            ));
        }
        self.anchor_price = Some(anchor_price);
        self.evaluate(event_time_ns)
    }

    /// Observe the latest market rules card: threshold, comparator, tie outcome,
    /// and window close (as a logical event timestamp).
    pub fn observe_contract(
        &mut self,
        event_time_ns: u64,
        threshold: Decimal,
        comparator: Comparator,
        tie_outcome: SettledSide,
        window_close_ns: u64,
    ) -> Result<AnchorOutcome, SpotSignalError> {
        observe_monotonic(&mut self.last_event_ns, event_time_ns)?;
        if threshold <= Decimal::ZERO {
            return Err(SpotSignalError::MarketValue(
                "contract threshold must be positive".to_owned(),
            ));
        }
        self.contract = Some(SettlementContract {
            threshold,
            comparator,
            tie_outcome,
            window_close_ns,
        });
        self.evaluate(event_time_ns)
    }

    fn evaluate(&self, event_time_ns: u64) -> Result<AnchorOutcome, SpotSignalError> {
        let (Some(anchor_price), Some(contract)) = (self.anchor_price, self.contract) else {
            return Ok(AnchorOutcome::NotReady {
                reason: "inputs_warming_up",
            });
        };
        // A closed window (close at or before now) has no resolution-sensitivity
        // context left to report.
        let Some(time_to_close_ns) = contract.window_close_ns.checked_sub(event_time_ns) else {
            return Ok(AnchorOutcome::NotReady {
                reason: "window_closed",
            });
        };
        if time_to_close_ns == 0 {
            return Ok(AnchorOutcome::NotReady {
                reason: "window_closed",
            });
        }
        let time_to_close_ms = time_to_close_ns / NANOS_PER_MILLI;
        let distance_bps = return_bps(anchor_price, contract.threshold)?;
        let side_if_settled_now = contract.comparator.settled_side(
            anchor_price,
            contract.threshold,
            contract.tie_outcome,
        );
        // Both conditions inclusive: the pin context is the sensitive band.
        let state = if distance_bps.abs() <= self.pin_threshold_bps
            && time_to_close_ms <= self.pin_window_ms
        {
            AnchorSettlementState::Pinned
        } else {
            AnchorSettlementState::Tracking
        };
        Ok(AnchorOutcome::Ready(AnchorState {
            anchor_price,
            threshold: contract.threshold,
            distance_bps,
            time_to_close_ms,
            side_if_settled_now,
            state,
        }))
    }

    /// The SETTLED anchor reading published at the grid close (ruling E37b): the
    /// one final ready decision the Kalshi settlement anchor emits at the instant
    /// `window_as_of == quarter_close_time`, carrying the settled anchor with
    /// `time_to_close_ms == 0`, immediately before the market goes
    /// `market_closed`. Unlike [`Self::evaluate`], a zero time-to-close is the
    /// settled state here, not `window_closed`: this is the one decision that
    /// reports the anchor exactly as it settles. It reads no candidate state and
    /// mutates none — the rules-card fields are supplied by the caller — so a
    /// replay reproduces it byte-for-byte. `state` is `pinned` when the absolute
    /// distance is within `pin_threshold_bps` (the time-to-close arm of the pin
    /// test is zero and so always satisfied at settlement) and `tracking`
    /// otherwise.
    pub fn settled_outcome(
        &self,
        anchor_price: Decimal,
        threshold: Decimal,
        comparator: Comparator,
        tie_outcome: SettledSide,
    ) -> Result<AnchorState, SpotSignalError> {
        if anchor_price <= Decimal::ZERO {
            return Err(SpotSignalError::MarketValue(
                "anchor price must be positive".to_owned(),
            ));
        }
        if threshold <= Decimal::ZERO {
            return Err(SpotSignalError::MarketValue(
                "contract threshold must be positive".to_owned(),
            ));
        }
        let distance_bps = return_bps(anchor_price, threshold)?;
        let side_if_settled_now = comparator.settled_side(anchor_price, threshold, tie_outcome);
        let state = if distance_bps.abs() <= self.pin_threshold_bps {
            AnchorSettlementState::Pinned
        } else {
            AnchorSettlementState::Tracking
        };
        Ok(AnchorState {
            anchor_price,
            threshold,
            distance_bps,
            time_to_close_ms: 0,
            side_if_settled_now,
            state,
        })
    }
}

// ---------------------------------------------------------------------------
// 2. oracle_divergence (Polymarket)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DivergenceState {
    Aligned,
    Diverged,
}

impl DivergenceState {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Aligned => "aligned",
            Self::Diverged => "diverged",
        }
    }
}

/// Ready oracle-divergence reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DivergenceContext {
    pub divergence_bps: Decimal,
    pub anchor_price: Decimal,
    pub spot_reference_mid: Decimal,
    pub state: DivergenceState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DivergenceOutcome {
    Ready(DivergenceContext),
    NotReady { reason: &'static str },
}

/// Oracle divergence (`btc.polymarket.oracle_divergence`).
///
/// `divergence_bps = (anchor - spot_reference_mid) / spot_reference_mid` in
/// signed basis points, comparing the bound Polymarket market's resolving TWAP
/// settlement anchor to the composite spot reference. State is `aligned` when the absolute divergence is strictly
/// below `diverged_threshold_bps` and `diverged` otherwise. Mechanical caveat:
/// the anchor is a 60-second average, so nonzero divergence during a spot move
/// is expected averaging lag, not an anomaly — this contextualizes
/// oracle-versus-spot positioning and leaves the judgment to consumers. A zero
/// or invalid spot mid is a defined not-ready state; not ready until both inputs
/// have emitted.
#[derive(Debug, Clone)]
pub struct OracleDivergenceCandidate {
    diverged_threshold_bps: Decimal,
    anchor_price: Option<Decimal>,
    spot_reference_mid: Option<Decimal>,
}

impl OracleDivergenceCandidate {
    #[must_use]
    pub fn new(diverged_threshold_bps: Decimal) -> Self {
        Self {
            diverged_threshold_bps,
            anchor_price: None,
            spot_reference_mid: None,
        }
    }

    /// Constructs with the documented default divergence threshold.
    #[must_use]
    pub fn with_default_threshold() -> Self {
        Self::new(DEFAULT_DIVERGED_THRESHOLD_BPS)
    }

    /// The configured divergence threshold, echoed by the published
    /// `diverged_threshold_bps` field.
    #[must_use]
    pub fn diverged_threshold_bps(&self) -> Decimal {
        self.diverged_threshold_bps
    }

    /// Observe the latest resolving TWAP anchor price.
    pub fn observe_anchor(&mut self, anchor_price: Decimal) -> DivergenceOutcome {
        self.anchor_price = Some(anchor_price);
        self.evaluate()
    }

    /// Observe the latest composite spot-reference midpoint.
    pub fn observe_spot_reference(&mut self, spot_reference_mid: Decimal) -> DivergenceOutcome {
        self.spot_reference_mid = Some(spot_reference_mid);
        self.evaluate()
    }

    fn evaluate(&self) -> DivergenceOutcome {
        let (Some(anchor_price), Some(spot_reference_mid)) =
            (self.anchor_price, self.spot_reference_mid)
        else {
            return DivergenceOutcome::NotReady {
                reason: "inputs_warming_up",
            };
        };
        // This family treats an invalid observation on either input as
        // not-ready rather than an error: both inputs are symmetric latest
        // values, and a transient bad print must not wedge the pairing.
        if anchor_price <= Decimal::ZERO {
            return DivergenceOutcome::NotReady {
                reason: "invalid_anchor_price",
            };
        }
        if spot_reference_mid <= Decimal::ZERO {
            return DivergenceOutcome::NotReady {
                reason: "invalid_spot_mid",
            };
        }
        // spot_reference_mid > 0 is guaranteed above, so return_bps cannot divide
        // by zero here.
        let divergence_bps = match return_bps(anchor_price, spot_reference_mid) {
            Ok(value) => value,
            Err(_) => {
                return DivergenceOutcome::NotReady {
                    reason: "invalid_spot_mid",
                };
            }
        };
        let state = if divergence_bps.abs() < self.diverged_threshold_bps {
            DivergenceState::Aligned
        } else {
            DivergenceState::Diverged
        };
        DivergenceOutcome::Ready(DivergenceContext {
            divergence_bps,
            anchor_price,
            spot_reference_mid,
            state,
        })
    }
}
