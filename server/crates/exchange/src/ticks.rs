//! Fixed price-tick universe for the canonical YES book.
//!
//! Prices are represented internally as `u16` tenths-of-a-cent in the open
//! interval (0, 100): values 1..=999. The set of *valid* prices is fixed and
//! non-uniform, with finer granularity near the tails where short-duration
//! prediction markets need more resolution:
//!
//! * Fine bottom : 0.1¢ .. 3.0¢   in 0.1¢ steps  (30 ticks: tenths 1..=30)
//! * Coarse mid  : 4¢   .. 96¢    in 1¢   steps  (93 ticks: tenths 40, 50, ..., 960)
//! * Fine top    : 97.0¢ .. 99.9¢ in 0.1¢ steps  (30 ticks: tenths 970..=999)
//!
//! Total: 153 ticks. The table is symmetric about 50¢, so the complement of
//! every valid tick (used to display the NO price) is itself valid.
//!
//! Both directions of the price ↔ index map are O(1): `TICKS[idx]` returns the
//! tenths-of-cent value at a given index, and `INDEX_OF[tenths]` returns the
//! index (or `INVALID_INDEX` if the price is not a valid tick).

use std::fmt;

use serde::{Deserialize, Serialize};

pub const NUM_TICKS: usize = 30 + 93 + 30;

const INVALID_INDEX: u16 = u16::MAX;

const TABLES: ([u16; NUM_TICKS], [u16; 1000]) = {
    let mut ticks = [0u16; NUM_TICKS];
    let mut index_of = [INVALID_INDEX; 1000];
    let mut i = 0usize;

    let mut v = 1u16;
    while v <= 30 {
        ticks[i] = v;
        index_of[v as usize] = i as u16;
        i += 1;
        v += 1;
    }

    let mut v = 40u16;
    while v <= 960 {
        ticks[i] = v;
        index_of[v as usize] = i as u16;
        i += 1;
        v += 10;
    }

    let mut v = 970u16;
    while v <= 999 {
        ticks[i] = v;
        index_of[v as usize] = i as u16;
        i += 1;
        v += 1;
    }

    (ticks, index_of)
};

pub static TICKS: [u16; NUM_TICKS] = TABLES.0;
pub static INDEX_OF: [u16; 1000] = TABLES.1;

/// A valid price on the canonical YES book.
///
/// Held as tenths-of-a-cent. Construct via [`PriceTick::from_tenths`] or
/// [`PriceTick::from_index`]; both reject prices outside the tick universe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PriceTick(u16);

impl PriceTick {
    /// Construct from tenths-of-a-cent (e.g. 650 = 65¢). Returns `None` if
    /// the value is not on the fixed tick grid.
    pub fn from_tenths(tenths: u16) -> Option<PriceTick> {
        if (tenths as usize) < INDEX_OF.len() && INDEX_OF[tenths as usize] != INVALID_INDEX {
            Some(PriceTick(tenths))
        } else {
            None
        }
    }

    /// Construct from a dense tick index in `0..NUM_TICKS`.
    pub fn from_index(idx: usize) -> Option<PriceTick> {
        if idx < NUM_TICKS {
            Some(PriceTick(TICKS[idx]))
        } else {
            None
        }
    }

    /// Tenths-of-a-cent representation.
    pub fn tenths(self) -> u16 {
        self.0
    }

    /// Dense index into `TICKS`. The book uses this to address its level array.
    pub fn to_index(self) -> usize {
        INDEX_OF[self.0 as usize] as usize
    }

    /// The complement price used to display the NO side: NO = 1 − YES.
    /// Always returns a valid `PriceTick` by construction of the tick table.
    pub fn complement(self) -> PriceTick {
        let c = 1000 - self.0;
        debug_assert!(
            INDEX_OF[c as usize] != INVALID_INDEX,
            "complement of valid tick must be valid: {} -> {}",
            self.0,
            c,
        );
        PriceTick(c)
    }
}

/// Formats as a dollar value with three decimal places, e.g. `$0.650`.
impl fmt::Display for PriceTick {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let cents = self.0 / 10;
        let tenth = self.0 % 10;
        write!(f, "$0.{:02}{}", cents, tenth)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_has_expected_size() {
        assert_eq!(NUM_TICKS, 153);
        assert_eq!(TICKS.len(), NUM_TICKS);
    }

    #[test]
    fn ticks_are_strictly_increasing() {
        for w in TICKS.windows(2) {
            assert!(w[0] < w[1], "ticks not increasing at {:?}", w);
        }
    }

    #[test]
    fn fine_bottom_then_coarse_then_fine_top() {
        // First 30 entries: 1..=30.
        for (i, &t) in TICKS.iter().take(30).enumerate() {
            assert_eq!(t, (i + 1) as u16);
        }
        // Next 93 entries: 40, 50, ..., 960.
        for (k, &t) in TICKS[30..123].iter().enumerate() {
            assert_eq!(t, 40 + 10 * k as u16);
        }
        // Last 30 entries: 970..=999.
        for (k, &t) in TICKS[123..].iter().enumerate() {
            assert_eq!(t, 970 + k as u16);
        }
    }

    #[test]
    fn from_tenths_accepts_only_valid_prices() {
        for &t in TICKS.iter() {
            let pt = PriceTick::from_tenths(t).expect("valid tick");
            assert_eq!(pt.tenths(), t);
        }
        // Invalid: between fine-bottom and coarse (31..=39), non-multiples
        // of 10 inside the coarse band, and between coarse and fine-top
        // (961..=969).
        for invalid in [31u16, 35, 39, 41, 105, 333, 555, 707, 869, 961, 965, 969] {
            assert!(PriceTick::from_tenths(invalid).is_none(), "{} should be invalid", invalid);
        }
        // Out-of-range.
        assert!(PriceTick::from_tenths(0).is_none());
        assert!(PriceTick::from_tenths(1000).is_none());
    }

    #[test]
    fn index_roundtrip() {
        for idx in 0..NUM_TICKS {
            let pt = PriceTick::from_index(idx).unwrap();
            assert_eq!(pt.to_index(), idx);
        }
        assert!(PriceTick::from_index(NUM_TICKS).is_none());
    }

    #[test]
    fn complement_of_every_tick_is_also_a_tick() {
        for &t in TICKS.iter() {
            let pt = PriceTick::from_tenths(t).unwrap();
            let c = pt.complement();
            assert_eq!(c.tenths(), 1000 - t);
            // Roundtrip: complement of complement is self.
            assert_eq!(c.complement(), pt);
        }
    }

    #[test]
    fn display_formats_to_three_decimals() {
        assert_eq!(PriceTick::from_tenths(1).unwrap().to_string(), "$0.001");
        assert_eq!(PriceTick::from_tenths(30).unwrap().to_string(), "$0.030");
        assert_eq!(PriceTick::from_tenths(500).unwrap().to_string(), "$0.500");
        assert_eq!(PriceTick::from_tenths(650).unwrap().to_string(), "$0.650");
        assert_eq!(PriceTick::from_tenths(999).unwrap().to_string(), "$0.999");
    }
}
