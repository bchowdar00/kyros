//! The fixed table of market series.
//!
//! A *series* is a family of back-to-back rounds with one duration. Every
//! series runs its own scheduler, order book, market maker and trader
//! cohort; all of them share one ledger of account balances. The table is
//! compile-time: adding a series is one line here, and every consumer
//! (`SeriesId::all`, the price feed's per-series windows, the bot cohorts)
//! sizes itself from it.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub use crate::types::{DEFAULT_SERIES, SeriesId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeriesSpec {
    pub id: SeriesId,
    /// Wire identifier (`"btc-5m"`). Stable: clients key on it.
    pub slug: &'static str,
    /// Round length in seconds.
    pub duration_secs: u64,
    /// What the series' rounds are about: the asset or event whose feeds
    /// apply to it (`btc`, later `eth`, `crude`, an NBA game, …). Signal
    /// definitions live under `definitions/<underlying>/signals/` and the
    /// registry scopes them to every series with that underlying.
    pub underlying: &'static str,
    /// The two outcome labels, positive side first: the book quotes
    /// `outcomes[0]` and the engine's `Outcome::Yes` means it won.
    pub outcomes: [&'static str; 2],
}

/// The outcome labels of a BTC up/down series.
pub const BTC_OUTCOMES: [&str; 2] = ["up", "down"];
/// The outcome labels of an NBA head-to-head series.
pub const NBA_OUTCOMES: [&str; 2] = ["home", "away"];

/// Every series the server runs, indexed by `SeriesId.0`.
///
/// The frontend derives every chart window from the server's snapshot, so
/// round lengths here are the only place they are defined.
pub const SERIES: &[SeriesSpec] = &[
    SeriesSpec { id: SeriesId(0), slug: "btc-5m", duration_secs: 300, underlying: "btc", outcomes: BTC_OUTCOMES },
    SeriesSpec { id: SeriesId(1), slug: "btc-15m", duration_secs: 900, underlying: "btc", outcomes: BTC_OUTCOMES },
    SeriesSpec { id: SeriesId(2), slug: "nba-h2h", duration_secs: 300, underlying: "nba", outcomes: NBA_OUTCOMES },
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownSeries(pub String);

impl fmt::Display for UnknownSeries {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown series `{}`", self.0)
    }
}

impl std::error::Error for UnknownSeries {}

impl SeriesId {
    pub fn spec(self) -> &'static SeriesSpec {
        &SERIES[self.0 as usize]
    }

    pub fn slug(self) -> &'static str {
        self.spec().slug
    }

    pub fn duration_secs(self) -> u64 {
        self.spec().duration_secs
    }

    pub fn underlying(self) -> &'static str {
        self.spec().underlying
    }

    pub fn outcomes(self) -> [&'static str; 2] {
        self.spec().outcomes
    }

    /// Whether this series' rounds are BTC up/down.
    pub fn is_btc(self) -> bool {
        self.underlying() == "btc"
    }

    /// Every series in table order.
    pub fn all() -> impl Iterator<Item = SeriesId> {
        SERIES.iter().map(|s| s.id)
    }

    /// The outcome this series' book quotes (`up`, `home`); the other
    /// outcome's prices are the complement.
    pub fn quoted_outcome(self) -> &'static str {
        self.spec().outcomes[0]
    }
}

/// Whether an outcome label is the quoted side of the series that owns
/// it. Labels are disjoint across underlyings (`up`/`down`, `home`/`away`),
/// so the label alone decides the frame; a test pins that. `None` (a
/// non-outcome instrument) is quoted.
pub fn is_quoted_outcome(label: Option<&str>) -> bool {
    match label {
        None => true,
        Some(label) => SERIES.iter().any(|s| s.outcomes[0] == label),
    }
}

impl SeriesId {
    pub fn from_slug(slug: &str) -> Result<SeriesId, UnknownSeries> {
        SERIES
            .iter()
            .find(|s| s.slug == slug)
            .map(|s| s.id)
            .ok_or_else(|| UnknownSeries(slug.to_string()))
    }
}

impl fmt::Display for SeriesId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.slug())
    }
}

impl FromStr for SeriesId {
    type Err = UnknownSeries;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        SeriesId::from_slug(s)
    }
}

impl Serialize for SeriesId {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.slug())
    }
}

impl<'de> Deserialize<'de> for SeriesId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    /// The runtime decides an order's frame from its outcome label alone,
    /// so no complement label may double as another series' quoted one.
    #[test]
    fn outcome_labels_are_disjoint() {
        for s in SERIES {
            assert!(is_quoted_outcome(Some(s.outcomes[0])), "{}", s.slug);
            assert!(!is_quoted_outcome(Some(s.outcomes[1])), "{}: {} is the complement", s.slug, s.outcomes[1]);
            assert_eq!(s.id.quoted_outcome(), s.outcomes[0]);
        }
        assert!(is_quoted_outcome(None));
        assert!(!is_quoted_outcome(Some("nope")));
    }

    use super::*;

    #[test]
    fn table_ids_match_positions() {
        for (i, spec) in SERIES.iter().enumerate() {
            assert_eq!(spec.id.0 as usize, i);
        }
        assert_eq!(DEFAULT_SERIES, SERIES[0].id);
    }

    #[test]
    fn slug_round_trip() {
        for s in SeriesId::all() {
            assert_eq!(s.slug().parse::<SeriesId>().unwrap(), s);
            assert_eq!(s.to_string(), s.slug());
        }
    }

    #[test]
    fn unknown_slug_errors() {
        let err = "eth-5m".parse::<SeriesId>().unwrap_err();
        assert_eq!(err, UnknownSeries("eth-5m".to_string()));
    }

    #[test]
    fn serde_as_slug_string() {
        let json = serde_json::to_string(&SeriesId(1)).unwrap();
        assert_eq!(json, "\"btc-15m\"");
        let back: SeriesId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, SeriesId(1));
        assert!(serde_json::from_str::<SeriesId>("\"nope\"").is_err());
        assert!(serde_json::from_str::<SeriesId>("1").is_err());
    }

    #[test]
    fn durations() {
        assert_eq!(DEFAULT_SERIES.duration_secs(), 300);
        assert_eq!(SeriesId(1).duration_secs(), 900);
        assert_eq!(SeriesId(2).duration_secs(), 300);
    }

    #[test]
    fn nba_series_is_home_away_and_not_btc() {
        let nba = SeriesId::from_slug("nba-h2h").unwrap();
        assert_eq!(nba.underlying(), "nba");
        assert_eq!(nba.outcomes(), NBA_OUTCOMES);
        assert!(!nba.is_btc());
        assert!(DEFAULT_SERIES.is_btc());
    }
}
