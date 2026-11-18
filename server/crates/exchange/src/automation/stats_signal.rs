//! Publishes each series' round statistics as a signal.
//!
//! The six numbers on the stats strip (round volume, open interest,
//! collateral locked, trades, traders, largest trade) are counters on the
//! series' engine, reset at settlement. This task turns them into the
//! `<slug>.current.stats.v1` signal the generated registry declares
//! ([`crate::registry::STATS_FIELDS`]), so strategies can trigger on them.
//!
//! One task per series polls the engine on a short interval and publishes a
//! snapshot whenever any value changes, when a
//! new round opens (the zeroed snapshot, so a strategy sees the reset
//! rather than stale end-of-round numbers), and on a slow heartbeat so a
//! quiet round still carries a fresh timestamp. Levels only: rates are
//! for strategies to derive with a window.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use std::collections::BTreeMap;

use serde_json::{Value as Json, json};

use crate::automation::signal_bus::{Quality, SignalBus, SignalFrame};
use crate::engine::{Exchange, MarketEngine, PAYOUT_PER_CONTRACT};
use crate::pricing::feed::unix_now_ms;
use crate::registry::stats_signal_id;
use crate::types::SeriesId;

/// How often the engine is sampled for a change.
const POLL_INTERVAL: Duration = Duration::from_millis(250);
/// Republish the unchanged snapshot at least this often.
const HEARTBEAT: Duration = Duration::from_secs(5);

/// The published values, as read from the engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatsSnapshot {
    pub market_id: Option<u32>,
    pub volume_tenths: i64,
    pub open_interest: u64,
    pub collateral_tenths: i64,
    pub trades: u64,
    pub traders: u64,
    pub largest_trade: u64,
}

impl StatsSnapshot {
    pub fn read(engine: &MarketEngine) -> Self {
        let oi = engine.open_interest();
        Self {
            market_id: engine.current_market().map(|m| m.id.0),
            volume_tenths: engine.market_volume_tenths(),
            open_interest: oi,
            collateral_tenths: oi as i64 * PAYOUT_PER_CONTRACT,
            trades: engine.market_trade_count(),
            traders: engine.market_unique_traders(),
            largest_trade: engine.market_largest_trade().map_or(0, |f| u64::from(f.qty.0)),
        }
    }

    /// The bus frame for `slug`, field set exactly as the registry
    /// declares it (decimals as strings, counts as numbers).
    pub fn frame(&self, slug: &str, ts_ms: u64) -> SignalFrame {
        let values: BTreeMap<String, Json> = [
            ("volume", json!(dollars(self.volume_tenths))),
            ("open_interest", json!(self.open_interest)),
            ("collateral", json!(dollars(self.collateral_tenths))),
            ("trades", json!(self.trades)),
            ("traders", json!(self.traders)),
            ("largest_trade", json!(self.largest_trade)),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        SignalFrame { signal_id: stats_signal_id(slug), ts_ms, quality: Quality::ready(), values }
    }
}

/// Tenths of a cent to a canonical dollar decimal string: `2_365_000` → `2365.000`.
pub fn dollars(tenths: i64) -> String {
    let sign = if tenths < 0 { "-" } else { "" };
    let t = tenths.unsigned_abs();
    format!("{sign}{}.{:03}", t / 1000, t % 1000)
}

/// Spawn the publisher for `series`. Runs for the life of the process.
pub fn spawn_stats_publisher(
    exchange: Arc<Mutex<Exchange>>,
    series: SeriesId,
    bus: Arc<SignalBus>,
) -> tokio::task::JoinHandle<()> {
    use tokio::time::{MissedTickBehavior, interval};

    tokio::spawn(async move {
        let slug = series.slug();
        let mut ticker = interval(POLL_INTERVAL);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut last: Option<StatsSnapshot> = None;
        let mut last_sent_ms: u64 = 0;
        let mut published: u64 = 0;
        loop {
            ticker.tick().await;
            let snap = {
                let x = exchange.lock().expect("exchange mutex poisoned");
                StatsSnapshot::read(x.engine(series))
            };
            let now = unix_now_ms();
            let changed = last.as_ref() != Some(&snap);
            let heartbeat_due = now.saturating_sub(last_sent_ms) >= HEARTBEAT.as_millis() as u64;
            if !(changed || heartbeat_due) {
                continue;
            }
            bus.publish(snap.frame(slug, now));
            published += 1;
            if published == 1 {
                tracing::info!(series = slug, "stats signal: first snapshot published");
            }
            tracing::debug!(series = slug, ?snap, changed, "stats signal: published");
            last = Some(snap);
            last_sent_ms = now;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::automation::signal_bus::{field_types, to_signal_event};
    use crate::automation::signals::SignalStore;
    use crate::automation::value::Value;
    use crate::registry::{self, Inputs, PriceSource};
    use crate::series::SERIES;
    use rust_decimal::Decimal;
    use std::str::FromStr;

    #[test]
    fn dollars_formats_tenths() {
        assert_eq!(dollars(0), "0.000");
        assert_eq!(dollars(2_365_000), "2365.000");
        assert_eq!(dollars(1_512_500), "1512.500");
        assert_eq!(dollars(-7), "-0.007");
    }

    /// The publisher's field set and types must match the registry entry
    /// byte for byte, or the platform rejects every event.
    #[test]
    fn snapshot_ingests_against_the_generated_registry() {
        let empty = std::env::temp_dir().join("tiny-stats-signal-no-defs");
        let _ = std::fs::create_dir_all(&empty);
        let reg = registry::build(&Inputs { price_source: PriceSource::GeminiIndex, live_lanes: &[], definitions_dir: Some(empty), copy_dir: None }).unwrap();
        let mut store = SignalStore::from_registry(&reg);
        for spec in SERIES {
            let id = stats_signal_id(spec.slug);
            assert!(store.enable(&id), "{id} in registry");
            let snap = StatsSnapshot {
                market_id: Some(7),
                volume_tenths: 2_365_000,
                open_interest: 1512,
                collateral_tenths: 1_512_000,
                trades: 29,
                traders: 28,
                largest_trade: 190,
            };
            let def = reg.signals.iter().find(|s| s.signal_id == id).unwrap();
            let event = to_signal_event(&snap.frame(spec.slug, 1_000), &field_types(def)).expect("frame types against the registry");
            let typed = store.ingest(&event).expect("field set and types match the registry");
            assert_eq!(typed["volume"], Value::Decimal(Decimal::from_str("2365.000").unwrap()));
            assert_eq!(typed["open_interest"], Value::from_u64(1512));
            assert_eq!(typed["largest_trade"], Value::from_u64(190));
            assert_eq!(typed.len(), registry::STATS_FIELDS.len());
        }
    }

    #[test]
    fn fresh_engine_reads_zeros() {
        use crate::engine::Engine;
        use crate::types::Tenths;
        let e = Engine::new(Tenths(1_000_000));
        let s = StatsSnapshot::read(e.market());
        assert_eq!(s, StatsSnapshot { market_id: None, volume_tenths: 0, open_interest: 0, collateral_tenths: 0, trades: 0, traders: 0, largest_trade: 0 });
    }
}
