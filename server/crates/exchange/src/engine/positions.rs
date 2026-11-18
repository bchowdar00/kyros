//! Per-series position state.
//!
//! Balances live once per account on the ledger; exposure lives once per
//! (account, series) inside that series' engine. Everything a round
//! resets — the signed position, its YES-side cost basis, the realized
//! PnL accumulated so far this round, and the frozen result of the last
//! round — is bundled here so the engine can keep one map of them.

use serde::{Deserialize, Serialize};

use super::PAYOUT_PER_CONTRACT;
use super::model::MarketResult;
use crate::types::{Position, Tenths};

/// One account's exposure in one series.
///
/// `position` is signed: `+n` long n YES, `-n` long n NO.
///
/// `entry_cost_tenths` is the running Σ(price_yes × qty) over the current
/// directional run — i.e. the YES-side cost basis for the open position.
/// Always in YES terms regardless of position sign; avg entry =
/// entry_cost_tenths / |position|. Zeroed at flat and at market resolve.
///
/// `realized_pnl_this_market` accumulates closed-position PnL within the
/// active round and resets at resolve.
///
/// `last_market` is the result of the most recently resolved round this
/// account held a position or placed an order in. `None` until the first
/// resolution. Bots never get one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PositionState {
    pub position: Position,
    pub entry_cost_tenths: Tenths,
    pub realized_pnl_this_market: Tenths,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_market: Option<MarketResult>,
}

impl PositionState {
    pub fn is_flat(&self) -> bool {
        self.position.0 == 0
    }

    /// Average YES entry price in tenths, or `None` when flat.
    pub fn avg_entry(&self) -> Option<u16> {
        let signed = self.position.0;
        if signed == 0 {
            return None;
        }
        Some((self.entry_cost_tenths.0 / signed.abs()) as u16)
    }

    /// Side-aware cost basis: what the holder paid for the open exposure.
    /// YES longs paid the YES price; NO longs paid the complement.
    pub fn cost_basis(&self) -> i64 {
        let signed = self.position.0;
        if signed == 0 {
            0
        } else if signed > 0 {
            self.entry_cost_tenths.0
        } else {
            PAYOUT_PER_CONTRACT * signed.abs() - self.entry_cost_tenths.0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_flat_with_no_history() {
        let p = PositionState::default();
        assert!(p.is_flat());
        assert_eq!(p.avg_entry(), None);
        assert_eq!(p.cost_basis(), 0);
        assert!(p.last_market.is_none());
    }

    #[test]
    fn avg_entry_and_cost_basis_are_side_aware() {
        let long = PositionState {
            position: Position(10),
            entry_cost_tenths: Tenths(4000),
            ..Default::default()
        };
        assert_eq!(long.avg_entry(), Some(400));
        assert_eq!(long.cost_basis(), 4000);

        let short = PositionState {
            position: Position(-10),
            entry_cost_tenths: Tenths(4000),
            ..Default::default()
        };
        assert_eq!(short.avg_entry(), Some(400));
        // Paid (1000 - 400) per contract of NO.
        assert_eq!(short.cost_basis(), 6000);
    }
}
