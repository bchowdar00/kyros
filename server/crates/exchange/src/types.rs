//! Money, quantity, and identifier newtypes.
//!
//! All monetary values are integers in **tenths of a cent** (`Tenths`). The
//! sub-cent unit is required because the fine-tail price ticks reach 0.1¢
//! granularity, and a single contract at e.g. 0.1¢ must still be an integer
//! amount of money. One dollar = 1,000 `Tenths`; one cent = 10 `Tenths`.
//! Quantities are share counts (`Qty`). Positions are signed share counts:
//! `+n` = long n YES, `-n` = long n NO (i.e. short n YES). There is no
//! floating point anywhere in the money path.

use std::fmt;
use std::ops::{Add, AddAssign, Neg, Sub, SubAssign};
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A signed amount of money in tenths of a cent. $1 = `Tenths(1000)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Tenths(pub i64);

impl Tenths {
    pub const ZERO: Tenths = Tenths(0);
    pub const DOLLAR: Tenths = Tenths(1000);
}

impl Add for Tenths {
    type Output = Tenths;
    fn add(self, rhs: Tenths) -> Tenths { Tenths(self.0 + rhs.0) }
}
impl Sub for Tenths {
    type Output = Tenths;
    fn sub(self, rhs: Tenths) -> Tenths { Tenths(self.0 - rhs.0) }
}
impl AddAssign for Tenths {
    fn add_assign(&mut self, rhs: Tenths) { self.0 += rhs.0; }
}
impl SubAssign for Tenths {
    fn sub_assign(&mut self, rhs: Tenths) { self.0 -= rhs.0; }
}
impl Neg for Tenths {
    type Output = Tenths;
    fn neg(self) -> Tenths { Tenths(-self.0) }
}

/// An unsigned share count.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Qty(pub u32);

impl Qty {
    pub const ZERO: Qty = Qty(0);
    pub fn is_zero(self) -> bool { self.0 == 0 }
}

impl Add for Qty {
    type Output = Qty;
    fn add(self, rhs: Qty) -> Qty { Qty(self.0 + rhs.0) }
}
impl Sub for Qty {
    type Output = Qty;
    fn sub(self, rhs: Qty) -> Qty { Qty(self.0 - rhs.0) }
}
impl AddAssign for Qty {
    fn add_assign(&mut self, rhs: Qty) { self.0 += rhs.0; }
}
impl SubAssign for Qty {
    fn sub_assign(&mut self, rhs: Qty) { self.0 -= rhs.0; }
}

/// A signed position: `+` long YES, `-` long NO (short YES), `0` flat.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Position(pub i64);

impl Position {
    pub const FLAT: Position = Position(0);
}

impl Add for Position {
    type Output = Position;
    fn add(self, rhs: Position) -> Position { Position(self.0 + rhs.0) }
}
impl Sub for Position {
    type Output = Position;
    fn sub(self, rhs: Position) -> Position { Position(self.0 - rhs.0) }
}
impl AddAssign for Position {
    fn add_assign(&mut self, rhs: Position) { self.0 += rhs.0; }
}
impl SubAssign for Position {
    fn sub_assign(&mut self, rhs: Position) { self.0 -= rhs.0; }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OrderId(pub u64);

/// A 20-byte Ethereum address. Identifies a user account.
///
/// Parses hex with or without `0x` prefix, case-insensitive. Displays as
/// lowercase hex with the `0x` prefix. EIP-55 checksums are *not* verified —
/// this is intentional for the demo.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Address(pub [u8; 20]);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddressParseError {
    BadLength,
    NonHex,
}

impl fmt::Display for AddressParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AddressParseError::BadLength => f.write_str("address must be 20 bytes (40 hex chars)"),
            AddressParseError::NonHex => f.write_str("address contains non-hex characters"),
        }
    }
}

impl std::error::Error for AddressParseError {}

impl Address {
    pub const ZERO: Address = Address([0u8; 20]);
}

impl FromStr for Address {
    type Err = AddressParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).unwrap_or(s);
        if s.len() != 40 {
            return Err(AddressParseError::BadLength);
        }
        let mut bytes = [0u8; 20];
        for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
            let hi = hex_nibble(chunk[0])?;
            let lo = hex_nibble(chunk[1])?;
            bytes[i] = (hi << 4) | lo;
        }
        Ok(Address(bytes))
    }
}

fn hex_nibble(c: u8) -> Result<u8, AddressParseError> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(AddressParseError::NonHex),
    }
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("0x")?;
        for b in self.0.iter() {
            write!(f, "{:02x}", b)?;
        }
        Ok(())
    }
}

impl Serialize for Address {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Address {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "0x1234567890abcdef1234567890abcdef12345678";

    #[test]
    fn parse_with_0x_prefix() {
        let a: Address = SAMPLE.parse().unwrap();
        assert_eq!(a.0[0], 0x12);
        assert_eq!(a.0[19], 0x78);
    }

    #[test]
    fn parse_without_prefix() {
        let bare = &SAMPLE[2..];
        let a: Address = bare.parse().unwrap();
        assert_eq!(a.to_string(), SAMPLE);
    }

    #[test]
    fn parse_is_case_insensitive() {
        let upper = SAMPLE.to_uppercase();
        let mixed = "0x1234567890AbCdEf1234567890aBcDeF12345678";
        let a: Address = upper.parse().unwrap();
        let b: Address = mixed.parse().unwrap();
        assert_eq!(a, b);
        assert_eq!(a.to_string(), SAMPLE); // display always lowercase
    }

    #[test]
    fn rejects_bad_lengths() {
        assert_eq!("".parse::<Address>(), Err(AddressParseError::BadLength));
        assert_eq!("0x".parse::<Address>(), Err(AddressParseError::BadLength));
        assert_eq!("0xdeadbeef".parse::<Address>(), Err(AddressParseError::BadLength));
        let too_long = format!("{}00", SAMPLE);
        assert_eq!(too_long.parse::<Address>(), Err(AddressParseError::BadLength));
    }

    #[test]
    fn rejects_non_hex() {
        let bad = "0x1234567890abcdef1234567890abcdef1234567g";
        assert_eq!(bad.parse::<Address>(), Err(AddressParseError::NonHex));
    }

    #[test]
    fn display_roundtrip() {
        let a: Address = SAMPLE.parse().unwrap();
        let s = a.to_string();
        let b: Address = s.parse().unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn serde_json_roundtrip() {
        let a: Address = SAMPLE.parse().unwrap();
        let json = serde_json::to_string(&a).unwrap();
        assert_eq!(json, format!("\"{}\"", SAMPLE));
        let b: Address = serde_json::from_str(&json).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn serde_rejects_invalid_string() {
        let bad = "\"0xnothex\"";
        assert!(serde_json::from_str::<Address>(bad).is_err());
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MarketId(pub u32);

/// Side of the (canonical YES) order book. A `Bid` is a willingness to buy
/// YES at or below the quoted price; an `Ask` is a willingness to sell YES at
/// or above the quoted price. "Buy NO" is encoded as a YES `Ask` at the
/// complement price.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Bid,
    Ask,
}

impl Side {
    pub fn opposite(self) -> Side {
        match self {
            Side::Bid => Side::Ask,
            Side::Ask => Side::Bid,
        }
    }
}

/// Identifies one market *series* — a family of back-to-back rounds sharing
/// a duration (e.g. the 2-minute BTC up/down series). Small integer index
/// into [`crate::series::SERIES`]; serializes as the series slug so the
/// wire never sees the integer. See `series.rs` for the table and the
/// serde/`FromStr` implementations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct SeriesId(pub u8);

/// The series every unparameterised request defaults to.
pub const DEFAULT_SERIES: SeriesId = SeriesId(0);
