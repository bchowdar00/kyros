//! Publishes the live BTC price feed as the registry's external signal.
//!
//! The registry declares one external signal for the price source the
//! exchange is running on (`crate::registry::price_feed_signal`):
//! `pyth.btc_usd.spot.v1` with `last` and `conf`, or
//! `coinbase.btc_usd.spot.v1` with `last`. This task subscribes to the
//! feed's tick broadcast, which is already rate-gated to one tick per
//! [`TICK_BROADCAST_INTERVAL_MS`], and republishes each tick as a frame.
//! Pyth's confidence interval rides along from the feed's latest value.
//! A lagging subscriber skips ahead; only the newest price matters.

use std::sync::Arc;

use serde_json::json;
use tokio::sync::broadcast::error::RecvError;

use crate::automation::signal_bus::{Quality, SignalBus, SignalFrame};
use crate::pricing::feed::{FeedMessage, PriceFeed, TICK_BROADCAST_INTERVAL_MS};
use crate::registry::{PriceSource, price_feed_signal_id};

/// Dollars as a decimal string, to the cent.
fn usd(price: f64) -> String {
    format!("{price:.2}")
}

/// The frame for one tick. `conf` is included only for a source that
/// declares it; the registry rejects a field set that differs from the
/// declaration, so a Pyth tick without a confidence yet is skipped.
pub fn spot_frame(source: PriceSource, ts_ms: u64, price: f64, conf: Option<f64>) -> Option<SignalFrame> {
    let mut values = std::collections::BTreeMap::new();
    values.insert("last".to_string(), json!(usd(price)));
    if source == PriceSource::PythPro {
        values.insert("conf".to_string(), json!(usd(conf?)));
    }
    Some(SignalFrame { signal_id: price_feed_signal_id(source), ts_ms, quality: Quality::ready(), values })
}

/// Spawn the publisher. Runs for the life of the process.
pub fn spawn_spot_publisher(
    feed: Arc<PriceFeed>,
    source: PriceSource,
    bus: Arc<SignalBus>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut rx = feed.subscribe();
        let mut published: u64 = 0;
        loop {
            match rx.recv().await {
                Ok(FeedMessage::Tick { ts_ms, price }) => {
                    if let Some(frame) = spot_frame(source, ts_ms, price, feed.latest_confidence()) {
                        bus.publish(frame);
                        published += 1;
                        if published == 1 {
                            tracing::info!(
                                signal = price_feed_signal_id(source),
                                interval_ms = TICK_BROADCAST_INTERVAL_MS,
                                "spot signal: first tick published"
                            );
                        }
                    }
                }
                Ok(FeedMessage::MarketReset { .. }) => {}
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => break,
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::automation::signal_bus::{field_types, to_signal_event};
    use crate::automation::signals::SignalStore;
    use crate::automation::value::Value;
    use crate::registry::{self, Inputs};
    use rust_decimal::Decimal;
    use std::str::FromStr;

    fn store_for(source: PriceSource) -> (std::sync::Arc<strategy_compiler::Registry>, SignalStore) {
        let empty = std::env::temp_dir().join("tiny-spot-signal-no-defs");
        let _ = std::fs::create_dir_all(&empty);
        let reg = registry::build(&Inputs { price_source: source, live_lanes: &[], definitions_dir: Some(empty), copy_dir: None }).unwrap();
        let store = SignalStore::from_registry(&reg);
        (std::sync::Arc::new(reg), store)
    }

    #[test]
    fn frames_ingest_against_both_price_sources() {
        for (source, conf) in [(PriceSource::Coinbase, None), (PriceSource::PythPro, Some(12.5))] {
            let (reg, mut store) = store_for(source);
            let frame = spot_frame(source, 7, 79_845.735, conf).unwrap();
            assert!(store.enable(&frame.signal_id));
            let def = reg.signals.iter().find(|s| s.signal_id == frame.signal_id).unwrap();
            let event = to_signal_event(&frame, &field_types(def)).expect("frame types against the registry");
            let typed = store.ingest(&event).expect("field set matches");
            assert_eq!(typed["last"], Value::Decimal(Decimal::from_str("79845.74").unwrap()));
            assert_eq!(typed.len(), def.fields.len());
        }
    }

    #[test]
    fn pyth_without_a_confidence_yet_is_skipped() {
        assert!(spot_frame(PriceSource::PythPro, 1, 1.0, None).is_none());
        assert!(spot_frame(PriceSource::Coinbase, 1, 1.0, None).is_some());
    }

    #[tokio::test]
    async fn ticks_reach_the_bus() {
        let bus = SignalBus::new(registry::for_tests(), None);
        let feed = PriceFeed::new();
        let task = spawn_spot_publisher(feed.clone(), PriceSource::Coinbase, bus.clone());
        tokio::task::yield_now().await;
        feed.record(50_000.0);
        let id = price_feed_signal_id(PriceSource::Coinbase);
        for _ in 0..50 {
            if bus.latest(&id).is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let latest = bus.latest(&id).expect("tick published");
        assert_eq!(latest.values["last"], "50000.00");
        task.abort();
    }
}
