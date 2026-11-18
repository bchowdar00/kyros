//! The USDT/USD reference (E47 / E47(e)).
//!
//! Binance and Bybit trade BTC against USDT, so every USD-quoting `btc.binance.*`
//! / `btc.bybit.*` identity converts its price, notional and quote-denominated
//! cost fields through a USDT/USD reference rate at decision time. This component
//! holds the latest reference mid rate and the logical instant it was observed,
//! and answers two questions for a decision at logical time `now_ns`:
//!
//! * the rate to convert with (the mid, `(best_bid + best_ask) / 2`), and
//! * whether that rate is fresh enough — non-stale means observed within
//!   [`STALENESS_BOUND_MS`] of the decision (E47(b)/(c)).
//!
//! The reference is fed by the adopted `kraken.spot.usdt-usd.ticker@1.0.0`,
//! which arrives on the venue lanes over the dedicated `KrakenUsdtReference`
//! socket (E47(e)). A second reference —
//! [`coinbase.exchange.usdt-usd.ticker`](COINBASE_USDT_TICKER_SOURCE_ID) — is
//! being built in another worktree; its constructor slot is
//! [`UsdtReference::new_coinbase`], left documented-but-unused here so the seam
//! exists before that source lands. Pyth `Crypto.USDT/USD` joins the same way
//! once keyed.
//!
//! The component performs no rounding of the internal mid; the *published*
//! `usdt_usd_rate` field and every converted value are rounded to
//! [`RATE_DECIMAL_PLACES`] with round-half-to-even, matching the tier-1 output
//! rounding rule, so a consumer can reproduce a converted value exactly as
//! `round8(usdt_value * usdt_usd_rate)`.

use rust_decimal::{Decimal, RoundingStrategy};
use serde::{Deserialize, Serialize};
use signals_core::{SourceEvent, parse_decimal_text};
use thiserror::Error;

/// The Kraken USDT/USD reference source (E47(e)): the adopted, on-wire reference.
pub const KRAKEN_USDT_TICKER_SOURCE_ID: &str = "kraken.spot.usdt-usd.ticker";
/// The Kraken USDT/USD reference source version.
pub const KRAKEN_USDT_TICKER_SOURCE_VERSION: &str = "1.0.0";

/// The second reference source (E47(a)/(e)), built in another worktree. Declared
/// here so [`UsdtReference::new_coinbase`]'s seam is explicit; no definition for
/// it is embedded in this worktree and no `CoinbaseUsdtReference` stream is
/// wired until that source lands.
pub const COINBASE_USDT_TICKER_SOURCE_ID: &str = "coinbase.exchange.usdt-usd.ticker";

/// The reference is stale when it was last observed strictly more than this many
/// milliseconds before the decision (E47(b)/(c)). A rate observed exactly this
/// far back is still fresh — the boundary at exactly 60 000 ms is the last
/// non-stale value.
pub const STALENESS_BOUND_MS: u64 = 60_000;

/// Decimal places for the published `usdt_usd_rate` and every converted field,
/// round-half-to-even (the tier-1 output rounding rule).
pub const RATE_DECIMAL_PLACES: u32 = 8;

/// Which venue supplies the reference. The rate itself is venue-neutral; this is
/// the provenance stamped into the checkpoint so a resumed lane restores the
/// reference it was actually fed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReferenceVenue {
    /// Fed by `kraken.spot.usdt-usd.ticker`.
    Kraken,
    /// Fed by `coinbase.exchange.usdt-usd.ticker` (E47(e) seam; not yet wired).
    Coinbase,
}

#[derive(Debug, Error)]
pub enum UsdtReferenceError {
    #[error("USDT reference event is missing the {0} field")]
    MissingField(&'static str),
    #[error("USDT reference event field {field} is not a decimal: {source}")]
    Decimal {
        field: &'static str,
        #[source]
        source: signals_core::DecimalTextError,
    },
    #[error("USDT reference quote is not positive and uncrossed (bid {bid}, ask {ask})")]
    Crossed { bid: Decimal, ask: Decimal },
    #[error("USDT reference logical time regressed (observed {observed_ns}, now {now_ns})")]
    TimeRegressed { observed_ns: u64, now_ns: u64 },
}

/// One observed reference mid and the logical instant it was observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Observation {
    /// The mid rate, `(best_bid + best_ask) / 2`, at full internal precision.
    mid: Decimal,
    /// The logical time (lane `now_ns`) the reference frame was admitted.
    observed_ns: u64,
}

/// The latest USDT/USD reference state.
#[derive(Debug, Clone)]
pub struct UsdtReference {
    venue: ReferenceVenue,
    source_id: &'static str,
    latest: Option<Observation>,
}

impl UsdtReference {
    /// The primary reference, fed by `kraken.spot.usdt-usd.ticker` (E47(e)).
    pub fn new_kraken() -> Self {
        Self {
            venue: ReferenceVenue::Kraken,
            source_id: KRAKEN_USDT_TICKER_SOURCE_ID,
            latest: None,
        }
    }

    /// The second reference, fed by `coinbase.exchange.usdt-usd.ticker`
    /// (E47(a)/(e)). The constructor slot exists so the seam is in place before
    /// the Coinbase source and its `CoinbaseUsdtReference` stream land in another
    /// worktree; nothing constructs this yet. The two-reference selection policy
    /// (prefer, or blend, Kraken and Coinbase) is decided when the second source
    /// ships — this component deliberately holds exactly one feed today.
    pub fn new_coinbase() -> Self {
        Self {
            venue: ReferenceVenue::Coinbase,
            source_id: COINBASE_USDT_TICKER_SOURCE_ID,
            latest: None,
        }
    }

    /// The source id this reference consumes.
    pub fn source_id(&self) -> &'static str {
        self.source_id
    }

    /// Whether `source_id` is the reference feed (used to route it out of the
    /// venue's own market-data dispatch).
    pub fn owns_source(&self, source_id: &str) -> bool {
        source_id == self.source_id
    }

    /// Admit one reference ticker frame observed at lane logical time `now_ns`.
    /// The mid is `(best_bid + best_ask) / 2`; a non-positive or crossed quote is
    /// rejected and leaves the prior rate in place (a stale prior is caught at
    /// decision time by [`UsdtReferenceSnapshot::is_stale`]). Logical time must
    /// not regress relative to the last observation.
    pub fn observe(&mut self, event: &SourceEvent, now_ns: u64) -> Result<(), UsdtReferenceError> {
        let bid = decimal_field(event, "best_bid")?;
        let ask = decimal_field(event, "best_ask")?;
        if bid <= Decimal::ZERO || ask <= Decimal::ZERO || ask < bid {
            return Err(UsdtReferenceError::Crossed { bid, ask });
        }
        if let Some(previous) = self.latest
            && now_ns < previous.observed_ns
        {
            return Err(UsdtReferenceError::TimeRegressed {
                observed_ns: previous.observed_ns,
                now_ns,
            });
        }
        let mid = (bid + ask) / Decimal::from(2);
        self.latest = Some(Observation {
            mid,
            observed_ns: now_ns,
        });
        Ok(())
    }

    /// The reference state as seen from a decision at logical time `now_ns`.
    /// `age_ms` is `(now_ns - observed_ns) / 1_000_000`, saturating at zero if a
    /// checkpoint restores an observation stamped after `now_ns` (defensive; the
    /// lane clock is monotonic in normal operation).
    pub fn snapshot(&self, now_ns: u64) -> UsdtReferenceSnapshot {
        match self.latest {
            Some(observation) => {
                let age_ms = now_ns.saturating_sub(observation.observed_ns) / 1_000_000;
                UsdtReferenceSnapshot {
                    rate: Some(observation.mid),
                    age_ms: Some(age_ms),
                }
            }
            None => UsdtReferenceSnapshot {
                rate: None,
                age_ms: None,
            },
        }
    }

    /// The bounded checkpoint state (E47 item 5: the reference survives restart).
    pub fn checkpoint(&self) -> UsdtReferenceCheckpoint {
        UsdtReferenceCheckpoint {
            venue: self.venue,
            latest: self.latest,
        }
    }

    /// Restore the reference from a checkpoint. The provenance must match the
    /// reference this lane is configured with, so a checkpoint taken under one
    /// feed is never silently reinterpreted under another.
    pub fn restore(&mut self, checkpoint: &UsdtReferenceCheckpoint) {
        if checkpoint.venue == self.venue {
            self.latest = checkpoint.latest;
        }
    }
}

/// The reference as seen from one decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsdtReferenceSnapshot {
    rate: Option<Decimal>,
    age_ms: Option<u64>,
}

impl UsdtReferenceSnapshot {
    /// A fresh snapshot at `rate`, age zero. The primary use is tests and
    /// callers that want an explicit rate; the production path builds snapshots
    /// through [`UsdtReference::snapshot`].
    pub fn fresh(rate: Decimal) -> Self {
        Self {
            rate: Some(rate),
            age_ms: Some(0),
        }
    }

    /// An absent snapshot — no rate observed. Always [`Self::is_stale`].
    pub fn absent() -> Self {
        Self {
            rate: None,
            age_ms: None,
        }
    }

    /// The reference is stale when there is no rate, or the rate is older than
    /// [`STALENESS_BOUND_MS`] (E47(c)). Exactly `STALENESS_BOUND_MS` is fresh.
    pub fn is_stale(&self) -> bool {
        match (self.rate, self.age_ms) {
            (Some(_), Some(age_ms)) => age_ms > STALENESS_BOUND_MS,
            _ => true,
        }
    }

    /// The raw mid, before rounding. `None` when no rate has been observed.
    pub fn rate(&self) -> Option<Decimal> {
        self.rate
    }

    /// The age of the rate in milliseconds, or `None` when absent.
    pub fn age_ms(&self) -> Option<u64> {
        self.age_ms
    }

    /// The published `usdt_usd_rate`: the mid rounded to [`RATE_DECIMAL_PLACES`]
    /// half-even. `None` when absent.
    pub fn published_rate(&self) -> Option<Decimal> {
        self.rate.map(round_rate)
    }

    /// Convert one quote-denominated (USDT) value to USD with the published
    /// rate, rounded to [`RATE_DECIMAL_PLACES`] half-even, so
    /// `converted == round8(value * usdt_usd_rate)` exactly. `None` when the
    /// reference is absent.
    pub fn convert(&self, value: Decimal) -> Option<Decimal> {
        self.published_rate().map(|rate| round_rate(value * rate))
    }
}

/// The reference's bounded, serialized state for the lane checkpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsdtReferenceCheckpoint {
    venue: ReferenceVenue,
    latest: Option<Observation>,
}

fn round_rate(value: Decimal) -> Decimal {
    value.round_dp_with_strategy(RATE_DECIMAL_PLACES, RoundingStrategy::MidpointNearestEven)
}

fn decimal_field(event: &SourceEvent, name: &'static str) -> Result<Decimal, UsdtReferenceError> {
    let text = event
        .values
        .get(name)
        .and_then(serde_json::Value::as_str)
        .ok_or(UsdtReferenceError::MissingField(name))?;
    parse_decimal_text(text).map_err(|source| UsdtReferenceError::Decimal {
        field: name,
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    const NS_PER_MS: u64 = 1_000_000;

    fn ticker(bid: &str, ask: &str) -> SourceEvent {
        SourceEvent {
            schema_version: 1,
            source_id: KRAKEN_USDT_TICKER_SOURCE_ID.to_owned(),
            source_version: KRAKEN_USDT_TICKER_SOURCE_VERSION.to_owned(),
            publisher_epoch: "epoch".to_owned(),
            sequence: 0,
            event_time: "2026-08-12T12:00:01Z".to_owned(),
            received_at: "2026-08-12T12:00:01Z".to_owned(),
            values: BTreeMap::from([
                ("best_bid".to_owned(), serde_json::json!(bid)),
                ("best_ask".to_owned(), serde_json::json!(ask)),
            ]),
        }
    }

    fn dec(text: &str) -> Decimal {
        parse_decimal_text(text).expect("decimal")
    }

    #[test]
    fn absent_reference_is_stale_and_yields_no_conversion() {
        let reference = UsdtReference::new_kraken();
        let snapshot = reference.snapshot(0);
        assert!(snapshot.is_stale());
        assert_eq!(snapshot.rate(), None);
        assert_eq!(snapshot.age_ms(), None);
        assert_eq!(snapshot.published_rate(), None);
        assert_eq!(snapshot.convert(dec("100")), None);
    }

    #[test]
    fn mid_is_the_average_of_bid_and_ask() {
        let mut reference = UsdtReference::new_kraken();
        reference.observe(&ticker("0.99990", "1.00010"), 0).unwrap();
        let snapshot = reference.snapshot(0);
        // (0.99990 + 1.00010) / 2 = 1.00000000
        assert_eq!(snapshot.published_rate(), Some(dec("1.00000000")));
        assert!(!snapshot.is_stale());
        assert_eq!(snapshot.age_ms(), Some(0));
    }

    #[test]
    fn conversion_is_hand_computable_and_reproducible() {
        let mut reference = UsdtReference::new_kraken();
        // bid/ask average to a mid of 1.00030000 exactly.
        reference.observe(&ticker("1.00020", "1.00040"), 0).unwrap();
        let snapshot = reference.snapshot(0);
        assert_eq!(snapshot.published_rate(), Some(dec("1.00030000")));
        // 65000.00000000 USDT * 1.00030000 = 65019.50000000 USD (exact).
        assert_eq!(snapshot.convert(dec("65000")), Some(dec("65019.50000000")));
        // A product with a digit beyond the 8th place rounds half-even at 8dp:
        // 1.25 * 1.00030000 = 1.250375 -> 1.25037500.
        assert_eq!(snapshot.convert(dec("1.25")), Some(dec("1.25037500")));
        // The consumer reproduction identity holds: converted == round8(v*rate).
        let rate = snapshot.published_rate().unwrap();
        assert_eq!(
            snapshot.convert(dec("42123.45")),
            Some(round_rate(dec("42123.45") * rate))
        );
    }

    #[test]
    fn staleness_boundary_is_at_exactly_60000_ms() {
        let mut reference = UsdtReference::new_kraken();
        reference.observe(&ticker("0.99990", "1.00010"), 0).unwrap();
        // Exactly 60_000 ms old -> still fresh.
        let at_bound = reference.snapshot(STALENESS_BOUND_MS * NS_PER_MS);
        assert_eq!(at_bound.age_ms(), Some(STALENESS_BOUND_MS));
        assert!(!at_bound.is_stale());
        // One millisecond older -> stale.
        let past_bound = reference.snapshot((STALENESS_BOUND_MS + 1) * NS_PER_MS);
        assert_eq!(past_bound.age_ms(), Some(STALENESS_BOUND_MS + 1));
        assert!(past_bound.is_stale());
        // A newer observation makes the reference fresh again at the next decision.
        reference
            .observe(
                &ticker("0.99990", "1.00010"),
                (STALENESS_BOUND_MS + 1) * NS_PER_MS,
            )
            .unwrap();
        let refreshed = reference.snapshot((STALENESS_BOUND_MS + 1) * NS_PER_MS);
        assert!(!refreshed.is_stale());
        assert_eq!(refreshed.age_ms(), Some(0));
    }

    #[test]
    fn crossed_or_nonpositive_quote_is_rejected_and_keeps_the_prior_rate() {
        let mut reference = UsdtReference::new_kraken();
        reference.observe(&ticker("0.99990", "1.00010"), 0).unwrap();
        assert!(
            reference
                .observe(&ticker("1.00010", "0.99990"), 10)
                .is_err()
        );
        assert!(reference.observe(&ticker("0", "1.0"), 10).is_err());
        // The prior rate is intact.
        assert_eq!(
            reference.snapshot(0).published_rate(),
            Some(dec("1.00000000"))
        );
    }

    #[test]
    fn logical_time_must_not_regress() {
        let mut reference = UsdtReference::new_kraken();
        reference
            .observe(&ticker("0.99990", "1.00010"), 100)
            .unwrap();
        assert!(
            reference
                .observe(&ticker("0.99990", "1.00010"), 99)
                .is_err()
        );
    }

    #[test]
    fn checkpoint_round_trips_the_reference_state() {
        let mut reference = UsdtReference::new_kraken();
        reference
            .observe(&ticker("1.00020", "1.00040"), 5_000_000)
            .unwrap();
        let checkpoint = reference.checkpoint();
        let mut restored = UsdtReference::new_kraken();
        restored.restore(&checkpoint);
        assert_eq!(
            restored.snapshot(5_000_000).published_rate(),
            Some(dec("1.00030000"))
        );
        // A checkpoint from a different reference feed is not adopted.
        let mut other = UsdtReference::new_coinbase();
        other.restore(&checkpoint);
        assert_eq!(other.snapshot(5_000_000).rate(), None);
    }
}
