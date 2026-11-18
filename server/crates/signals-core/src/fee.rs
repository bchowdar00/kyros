//! The repository's single venue taker-fee authority.
//!
//! Two places charge venue taker fees: the benchmark's label economics
//! (`btc-benchmark`) and the prediction-venue pair-cost computations
//! (`btc-signals`). They previously carried byte-for-byte mirrored copies of the
//! rounding modes and the charging formulas, because `btc-benchmark` depends on
//! `btc-signals` and so `btc-signals` could not depend back on it. `signals-core`
//! sits under both, so the rule lives here exactly once (STANDING-PRACTICES rule
//! 6, one authority per fact: two implementations of the same fact will diverge).
//!
//! Every computation is exact decimal within `rust_decimal`'s range. The
//! charging helpers are checked and return `None` on arithmetic overflow so no
//! caller can silently publish a wrapped fee; each caller maps `None` onto its
//! own error type and keeps its own message. Exactness has one documented limit
//! that `None` does not signal: `rust_decimal` rounds a product once it exceeds
//! 28 significant digits (see
//! [`checked_rate_times_probability_product_fee`]). Rounding to a chargeable
//! amount is applied by [`round_fee`] as a separate step, because the venues
//! round at different places in the calculation.

use rust_decimal::{Decimal, RoundingStrategy};
use serde::{Deserialize, Serialize};

/// How a venue rounds a raw taker fee to a chargeable amount.
///
/// `NearestFiveDecimalsWithMinimum` also encodes a minimum charge: anything
/// below one unit in the fifth decimal is not charged at all rather than being
/// rounded up to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeeRounding {
    None,
    CeilCent,
    NearestCent,
    FloorCent,
    NearestFiveDecimalsWithMinimum,
}

/// Rounds a raw fee to its chargeable amount under `rounding`.
#[must_use]
pub fn round_fee(value: Decimal, rounding: FeeRounding) -> Decimal {
    match rounding {
        FeeRounding::None => value,
        FeeRounding::CeilCent => {
            value.round_dp_with_strategy(2, RoundingStrategy::ToPositiveInfinity)
        }
        FeeRounding::NearestCent => {
            value.round_dp_with_strategy(2, RoundingStrategy::MidpointAwayFromZero)
        }
        FeeRounding::FloorCent => {
            value.round_dp_with_strategy(2, RoundingStrategy::ToNegativeInfinity)
        }
        FeeRounding::NearestFiveDecimalsWithMinimum => {
            let minimum = Decimal::new(1, 5);
            if value < minimum {
                Decimal::ZERO
            } else {
                value.round_dp_with_strategy(5, RoundingStrategy::MidpointAwayFromZero)
            }
        }
    }
}

/// Raw fee for a flat per-contract schedule: `contracts * fee_per_contract`.
#[must_use]
pub fn checked_flat_per_contract_fee(
    contracts: Decimal,
    fee_per_contract: Decimal,
) -> Option<Decimal> {
    contracts.checked_mul(fee_per_contract)
}

/// Raw fee for the share-count probability-product schedule used by the
/// prediction venues: `contracts * rate * price * (1 - price)`.
///
/// This is the share-count rule, not a notional rule: the charge scales with the
/// number of contracts (shares) and peaks at a price of `0.5`. Pass
/// `Decimal::ONE` for `contracts` to charge one contract's leg.
///
/// The multiplication order is fixed and load-bearing. `rust_decimal` is exact
/// only until a product exceeds 28 significant digits, where it rounds, so
/// re-associating these factors can move the last digit of an extreme input.
#[must_use]
pub fn checked_rate_times_probability_product_fee(
    contracts: Decimal,
    rate: Decimal,
    price: Decimal,
) -> Option<Decimal> {
    contracts
        .checked_mul(rate)?
        .checked_mul(price)?
        .checked_mul(Decimal::ONE.checked_sub(price)?)
}

/// Raw fee for a percentage-of-entry-notional schedule: `entry_notional * rate`.
#[must_use]
pub fn checked_percentage_of_entry_notional_fee(
    entry_notional: Decimal,
    rate: Decimal,
) -> Option<Decimal> {
    entry_notional.checked_mul(rate)
}

/// Rounds `value` up to the nearest multiple of `alignment`:
/// `ceil(value / alignment) * alignment`.
///
/// This is the Kalshi fee-alignment rule, which rounds a fee up onto a member
/// tier's charging increment ($0.01 for non-direct members, $0.0001 for direct
/// members) rather than onto a fixed number of decimal places. Returns `None`
/// when `alignment` is not strictly positive (it is a divisor) or when the
/// arithmetic overflows.
#[must_use]
pub fn checked_align_fee_up(value: Decimal, alignment: Decimal) -> Option<Decimal> {
    if alignment <= Decimal::ZERO {
        return None;
    }
    value
        .checked_div(alignment)?
        .round_dp_with_strategy(0, RoundingStrategy::ToPositiveInfinity)
        .checked_mul(alignment)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probability_product_is_share_count_scaled_and_peaks_at_the_midpoint() {
        let rate = Decimal::new(7, 2);
        let one =
            checked_rate_times_probability_product_fee(Decimal::ONE, rate, Decimal::new(5, 1))
                .expect("one contract");
        let ten =
            checked_rate_times_probability_product_fee(Decimal::from(10), rate, Decimal::new(5, 1))
                .expect("ten contracts");
        assert_eq!(one, Decimal::new(175, 4));
        assert_eq!(ten, one * Decimal::from(10));
        let off_midpoint =
            checked_rate_times_probability_product_fee(Decimal::ONE, rate, Decimal::new(5, 2))
                .expect("off midpoint");
        assert!(off_midpoint < one);
    }

    #[test]
    fn charging_one_contract_equals_the_bare_product() {
        // `btc-signals` charges a single leg by passing `Decimal::ONE`; that must
        // be the identity so its per-leg fees keep their recorded values.
        let rate = Decimal::new(7, 2);
        let price = Decimal::new(55, 2);
        assert_eq!(
            checked_rate_times_probability_product_fee(Decimal::ONE, rate, price),
            Some(rate * price * (Decimal::ONE - price))
        );
    }

    #[test]
    fn five_decimal_rounding_enforces_the_minimum_charge() {
        assert_eq!(
            round_fee(
                Decimal::new(9, 6),
                FeeRounding::NearestFiveDecimalsWithMinimum
            ),
            Decimal::ZERO
        );
        assert_eq!(
            round_fee(
                Decimal::new(1, 5),
                FeeRounding::NearestFiveDecimalsWithMinimum
            ),
            Decimal::new(1, 5)
        );
        assert_eq!(
            round_fee(
                Decimal::new(15, 6),
                FeeRounding::NearestFiveDecimalsWithMinimum
            ),
            Decimal::new(2, 5)
        );
    }

    #[test]
    fn cent_rounding_modes_move_in_their_declared_directions() {
        let raw = Decimal::new(17325, 6);
        assert_eq!(round_fee(raw, FeeRounding::None), raw);
        assert_eq!(round_fee(raw, FeeRounding::CeilCent), Decimal::new(2, 2));
        assert_eq!(round_fee(raw, FeeRounding::FloorCent), Decimal::new(1, 2));
        assert_eq!(round_fee(raw, FeeRounding::NearestCent), Decimal::new(2, 2));
    }

    #[test]
    fn alignment_rounds_up_onto_the_member_tier_increment() {
        let raw = Decimal::new(3325, 6);
        assert_eq!(
            checked_align_fee_up(raw, Decimal::new(1, 2)),
            Some(Decimal::new(1, 2))
        );
        assert_eq!(
            checked_align_fee_up(raw, Decimal::new(1, 4)),
            Some(Decimal::new(34, 4))
        );
    }

    #[test]
    fn alignment_rejects_a_non_positive_increment() {
        for alignment in [Decimal::ZERO, Decimal::new(-1, 2)] {
            assert_eq!(checked_align_fee_up(Decimal::new(1, 2), alignment), None);
        }
    }

    #[test]
    fn charging_helpers_report_overflow_instead_of_wrapping() {
        assert_eq!(
            checked_flat_per_contract_fee(Decimal::MAX, Decimal::MAX),
            None
        );
        assert_eq!(
            checked_percentage_of_entry_notional_fee(Decimal::MAX, Decimal::MAX),
            None
        );
        assert_eq!(
            checked_rate_times_probability_product_fee(Decimal::MAX, Decimal::MAX, Decimal::ONE),
            None
        );
    }
}
