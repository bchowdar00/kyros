//! Publishes each series' distance from its strike as a signal.
//!
//! The registry declares `<slug>.current.delta.v1` for every series
//! (`crate::registry::series_signals`): the live BTC price against the
//! current round's strike in signed basis points, with the spot and strike
//! it was computed from. A strategy that prices the round (a fair-value
//! model) or asks "which side is spot on" reads this one frame instead of
//! joining the spot feed to the strike signal itself.
//!
//! One task per series subscribes to the feed's tick broadcast (already
//! rate-gated to one tick per `TICK_BROADCAST_INTERVAL_MS`), reads the
//! series' current round under the engine lock and publishes a frame per
//! tick while a round is open. A round change is published at once from
//! the feed's latest price, so a new round never reads a delta against the
//! old strike. Nothing publishes while no round is open.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use rust_decimal::Decimal;
use rust_decimal::prelude::FromPrimitive;
use serde_json::json;
use tokio::sync::broadcast::error::RecvError;

use crate::automation::signal_bus::{Quality, SignalBus, SignalFrame};
use crate::engine::{Exchange, MarketStatus};
use crate::pricing::feed::{FeedMessage, PriceFeed};
use crate::types::SeriesId;

/// Basis points per unit: `(spot - strike) / strike * BPS`.
const BPS: i64 = 10_000;

pub fn delta_signal_id(slug: &str) -> String {
    format!("{slug}.current.delta.v1")
}

/// Signed basis points of `spot` from `strike`, to a hundredth of a point.
/// `None` when the strike is not a positive finite number.
pub fn delta_bps(spot: f64, strike: f64) -> Option<Decimal> {
    if !(strike.is_finite() && strike > 0.0 && spot.is_finite()) {
        return None;
    }
    let spot = Decimal::from_f64(spot)?;
    let strike = Decimal::from_f64(strike)?;
    Some(((spot - strike) / strike * Decimal::from(BPS)).round_dp(2).normalize())
}

/// The frame for one tick against one strike.
pub fn delta_frame(slug: &str, ts_ms: u64, spot: f64, strike: f64) -> Option<SignalFrame> {
    let bps = delta_bps(spot, strike)?;
    let mut values = BTreeMap::new();
    values.insert("delta_bps".to_string(), json!(bps.to_string()));
    values.insert("spot".to_string(), json!(format!("{spot:.2}")));
    values.insert("strike".to_string(), json!(format!("{strike:.2}")));
    Some(SignalFrame { signal_id: delta_signal_id(slug), ts_ms, quality: Quality::ready(), values })
}

/// The strike of the series' open round, if any.
fn open_strike(exchange: &Exchange, series: SeriesId) -> Option<f64> {
    exchange.engine(series).current_market().filter(|m| m.status == MarketStatus::Open).and_then(|m| m.strike_btc_usd())
}

/// Spawn the publisher for `series`. Runs for the life of the process.
pub fn spawn_delta_publisher(
    exchange: Arc<Mutex<Exchange>>,
    feed: Arc<PriceFeed>,
    series: SeriesId,
    bus: Arc<SignalBus>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let slug = series.slug();
        let mut rx = feed.subscribe();
        let mut announced = false;
        loop {
            let (ts_ms, price) = match rx.recv().await {
                Ok(FeedMessage::Tick { ts_ms, price }) => (ts_ms, price),
                Ok(FeedMessage::MarketReset { series: reset, .. }) if reset == series => {
                    let Some(price) = feed.latest() else { continue };
                    (crate::pricing::feed::unix_now_ms(), price)
                }
                Ok(FeedMessage::MarketReset { .. }) => continue,
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => break,
            };
            let strike = {
                let x = exchange.lock().expect("exchange mutex poisoned");
                open_strike(&x, series)
            };
            let Some(strike) = strike else { continue };
            if let Some(frame) = delta_frame(slug, ts_ms, price, strike) {
                bus.publish(frame);
                if !announced {
                    announced = true;
                    tracing::info!(series = slug, "delta signal: first tick published");
                }
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
    use crate::registry;
    use crate::series::SERIES;
    use crate::types::Tenths;
    use std::str::FromStr;

    #[test]
    fn delta_is_signed_basis_points_to_a_hundredth() {
        assert_eq!(delta_bps(80_040.0, 80_000.0), Some(Decimal::from(5)));
        assert_eq!(delta_bps(79_960.0, 80_000.0), Some(Decimal::from(-5)));
        assert_eq!(delta_bps(80_000.0, 80_000.0), Some(Decimal::ZERO));
        assert_eq!(delta_bps(80_002.0, 80_000.0), Some(Decimal::from_str("0.25").unwrap()));
        assert_eq!(delta_bps(1.0, 0.0), None);
        assert_eq!(delta_bps(f64::NAN, 1.0), None);
    }

    #[test]
    fn frames_ingest_against_the_generated_registry_including_negative_deltas() {
        let reg = registry::for_tests();
        let mut store = SignalStore::from_registry(&reg);
        for spec in SERIES.iter().filter(|s| s.id.is_btc()) {
            let frame = delta_frame(spec.slug, 7, 79_920.0, 80_000.0).unwrap();
            assert!(store.enable(&frame.signal_id), "{} in registry", frame.signal_id);
            let def = reg.signals.iter().find(|s| s.signal_id == frame.signal_id).unwrap();
            let event = to_signal_event(&frame, &field_types(def)).expect("frame types against the registry");
            let typed = store.ingest(&event).unwrap_or_else(|e| panic!("{}: {e}", frame.signal_id));
            assert_eq!(typed.len(), def.fields.len());
            assert_eq!(typed["delta_bps"], Value::Decimal(Decimal::from(-10)));
            assert_eq!(typed["spot"], Value::Decimal(Decimal::from_str("79920.00").unwrap()));
            assert_eq!(typed["strike"], Value::Decimal(Decimal::from(80_000)));
        }
    }

    #[tokio::test]
    async fn publishes_only_while_a_round_is_open_and_follows_the_strike() {
        let series = SERIES[0].id;
        let exchange = Arc::new(Mutex::new(Exchange::new(Tenths(1_000_000), SERIES)));
        let bus = SignalBus::new(registry::for_tests(), None);
        let feed = PriceFeed::new();
        let task = spawn_delta_publisher(exchange.clone(), feed.clone(), series, bus.clone());
        tokio::task::yield_now().await;
        let id = delta_signal_id(series.slug());

        // No round yet: a tick publishes nothing.
        feed.record(50_000.0);
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(bus.latest(&id).is_none(), "no delta before the first round");

        // Open a round at 50,000; the next tick is +10 bps.
        exchange.lock().unwrap().open_first_market(series, 50_000.0, 0, 120);
        feed.record(50_050.0);
        for _ in 0..100 {
            if bus.latest(&id).is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let latest = bus.latest(&id).expect("delta published once a round is open");
        assert_eq!(latest.values["delta_bps"], "10");
        assert_eq!(latest.values["strike"], "50000.00");
        assert_eq!(latest.values["spot"], "50050.00");
        task.abort();
    }
}
