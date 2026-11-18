//! Registry series ids ↔ engine [`SeriesId`]s.
//!
//! The compiler's registry names a series by an opaque string
//! (`btc_usd.updown_2m`) and an instrument as `<series_id>.current`; the
//! engine names it by its `SERIES` table row. This is the one table that
//! joins them. The venue adapter resolves every order's instrument through
//! it, and (plan §3.0) the signal publishers derive their per-series signal
//! ids from it.
//!
//! The map is a plain value so tests can pin any registry id to any
//! series; production builds it from the series table with the boot-time
//! check against the registry (plan §8.3).

use std::collections::BTreeMap;

use crate::types::SeriesId;

/// The suffix that turns a registry series id into its live instrument.
pub const CURRENT_SUFFIX: &str = ".current";

#[derive(Debug, Clone, Default)]
pub struct SeriesMap {
    by_registry: BTreeMap<String, SeriesId>,
    by_series: BTreeMap<SeriesId, String>,
}

impl SeriesMap {
    /// Build from `(registry series id, engine series)` pairs.
    pub fn new(pairs: impl IntoIterator<Item = (impl Into<String>, SeriesId)>) -> SeriesMap {
        let mut map = SeriesMap::default();
        for (id, series) in pairs {
            let id = id.into();
            map.by_series.insert(series, id.clone());
            map.by_registry.insert(id, series);
        }
        map
    }

    /// The engine series behind a registry series id.
    pub fn series(&self, registry_id: &str) -> Option<SeriesId> {
        self.by_registry.get(registry_id).copied()
    }

    /// The registry series id of an engine series.
    pub fn registry_id(&self, series: SeriesId) -> Option<&str> {
        self.by_series.get(&series).map(String::as_str)
    }

    /// The engine series an instrument (`<series_id>.current`) trades in.
    pub fn instrument_series(&self, instrument_id: &str) -> Option<SeriesId> {
        let registry_id = instrument_id.strip_suffix(CURRENT_SUFFIX)?;
        self.series(registry_id)
    }

    /// The `<series_id>.current` instrument of an engine series.
    pub fn instrument_id(&self, series: SeriesId) -> Option<String> {
        self.registry_id(series).map(|id| format!("{id}{CURRENT_SUFFIX}"))
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, SeriesId)> {
        self.by_registry.iter().map(|(id, s)| (id.as_str(), *s))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instruments_resolve_through_the_map() {
        let m = SeriesMap::new([("btc_usd.updown_2m", SeriesId(0)), ("btc_usd.updown_15m", SeriesId(1))]);
        assert_eq!(m.instrument_series("btc_usd.updown_15m.current"), Some(SeriesId(1)));
        assert_eq!(m.instrument_series("btc_usd.updown_2m"), None, "not an instrument");
        assert_eq!(m.instrument_series("eth.current"), None);
        assert_eq!(m.instrument_id(SeriesId(0)).as_deref(), Some("btc_usd.updown_2m.current"));
        assert_eq!(m.registry_id(SeriesId(1)), Some("btc_usd.updown_15m"));
        assert_eq!(m.iter().count(), 2);
    }
}
