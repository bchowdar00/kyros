//! The words a person reads about a signal: one JSON file per indicator
//! under `copy/signals/`, keyed by the third segment of the signal id
//! (`btc.coinbase.rsi.v1` → `rsi.json`, `btc-5m.current.stats.v1` →
//! `stats.json`, `pyth.btc_usd.spot.v1` → `spot.json`), so one file covers
//! every venue that publishes the same indicator.
//!
//! A file carries the signal's title and a two-sentence description, and a
//! label and description for each field worth reading. Listing a field is
//! what exposes it: the registry marks listed fields `headline` and the
//! catalogue shows only those by default. Adding a signal is adding a file.
//!
//! ```json
//! {
//!   "title": "Liquidity shock",
//!   "description": "Depth and spread right now are compared with … .",
//!   "fields": {
//!     "state": { "label": "Liquidity state", "description": "… ." }
//!   }
//! }
//! ```
//!
//! A series internal (`<slug>.current.<indicator>.v1`) may also carry
//! copy specific to its series' underlying in `<underlying>.<indicator>.json`
//! (`nba.price.json`); that file wins over the shared one for those series.
//!
//! The directory is `SIGNALS_COPY_DIR`, else `copy/signals` under the
//! working directory, else the checkout's.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const COPY_DIR_ENV: &str = "SIGNALS_COPY_DIR";
const COPY_RELATIVE: &str = "copy/signals";
const COPY_IN_CHECKOUT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../copy/signals");
const INDICATOR_SEGMENT: usize = 2;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FieldCopy {
    pub label: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignalCopy {
    pub title: String,
    pub description: String,
    /// Field name → its copy. Fields not listed are plumbing.
    #[serde(default)]
    pub fields: BTreeMap<String, FieldCopy>,
}

/// Every copy file, by indicator.
#[derive(Debug, Default, Clone)]
pub struct CopyBook {
    by_indicator: BTreeMap<String, SignalCopy>,
}

/// The indicator a signal id is filed under.
pub fn indicator_of(signal_id: &str) -> &str {
    signal_id.split('.').nth(INDICATOR_SEGMENT).unwrap_or("")
}

/// A copy file's key split into its optional underlying prefix and the
/// indicator: `nba.price` → (`Some("nba")`, `price`), `price` → (`None`, `price`).
pub fn split_key(key: &str) -> (Option<&str>, &str) {
    match key.split_once('.') {
        Some((underlying, indicator)) => (Some(underlying), indicator),
        None => (None, key),
    }
}

/// Resolve the copy directory: an override, the environment, the working
/// directory, then the checkout.
pub fn copy_dir(override_dir: Option<&Path>) -> Option<PathBuf> {
    let candidates: Vec<PathBuf> = override_dir
        .map(Path::to_path_buf)
        .into_iter()
        .chain(std::env::var(COPY_DIR_ENV).ok().map(PathBuf::from))
        .chain([PathBuf::from(COPY_RELATIVE), PathBuf::from(COPY_IN_CHECKOUT)])
        .collect();
    candidates.into_iter().find(|p| p.is_dir())
}

impl CopyBook {
    /// Read every `<dir>/*.json`. A malformed file is an error, not a skip,
    /// so a typo cannot silently hide a signal.
    pub fn load(dir: &Path) -> Result<Self> {
        let mut by_indicator = BTreeMap::new();
        let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
            .with_context(|| format!("read {}", dir.display()))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .collect();
        paths.sort();
        for path in &paths {
            let indicator = path
                .file_stem()
                .and_then(|s| s.to_str())
                .with_context(|| format!("non-UTF-8 copy file {}", path.display()))?
                .to_string();
            let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
            let copy: SignalCopy =
                serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
            by_indicator.insert(indicator, copy);
        }
        Ok(Self { by_indicator })
    }

    pub fn get(&self, indicator: &str) -> Option<&SignalCopy> {
        self.by_indicator.get(indicator)
    }

    pub fn for_signal(&self, signal_id: &str) -> Option<&SignalCopy> {
        self.get(indicator_of(signal_id))
    }

    /// The copy for a signal published on a series of `underlying`: a
    /// file named `<underlying>.<indicator>.json` wins over the shared
    /// `<indicator>.json`, so the price and clock of an NBA series can
    /// speak of home and away and a final score while the BTC series
    /// keep talking about Up and the strike.
    pub fn for_signal_in(&self, signal_id: &str, underlying: Option<&str>) -> Option<&SignalCopy> {
        let indicator = indicator_of(signal_id);
        underlying
            .and_then(|u| self.get(&format!("{u}.{indicator}")))
            .or_else(|| self.get(indicator))
    }

    pub fn indicators(&self) -> impl Iterator<Item = &str> {
        self.by_indicator.keys().map(String::as_str)
    }

    pub fn len(&self) -> usize {
        self.by_indicator.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_indicator.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_one_file_per_indicator_and_rejects_bad_ones() {
        let dir = std::env::temp_dir().join(format!("tiny-copy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("rsi.json"),
            r#"{"title":"RSI","description":"Gains against losses. Zero to one hundred.","fields":{"rsi_14":{"label":"RSI","description":"The reading. Above seventy is stretched."}}}"#,
        )
        .unwrap();
        std::fs::write(dir.join("notes.txt"), "ignored").unwrap();

        let book = CopyBook::load(&dir).unwrap();
        assert_eq!(book.len(), 1);
        assert_eq!(book.for_signal("btc.coinbase.rsi.v1").unwrap().title, "RSI");
        assert_eq!(book.for_signal("btc.kraken.rsi.v1").unwrap().fields["rsi_14"].label, "RSI");
        assert!(book.for_signal("btc.coinbase.macd.v1").is_none());
        assert_eq!(indicator_of("btc-5m.current.stats.v1"), "stats");
        assert_eq!(indicator_of("pyth.btc_usd.spot.v1"), "spot");

        std::fs::write(dir.join("bad.json"), r#"{"title":"x","description":"y","fields":{"a":{"label":"A"}}}"#).unwrap();
        assert!(CopyBook::load(&dir).is_err(), "a field without a description is a mistake");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
