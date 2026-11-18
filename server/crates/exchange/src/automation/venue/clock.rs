//! The venue clock over the exchange's live markets (plan §3.3 "Timers").
//!
//! A `window_clock` timer and `platform.window.remaining_ms` both read the
//! remaining time of the instrument's *own* series' current window, so a
//! 15-minute strategy's `remaining: 10s` never measures the 2-minute round.
//! Markets close on whole seconds (`Market::closes_at`), so the answer is
//! second-granular.

use std::sync::{Arc, Mutex};

use crate::automation::sme::VenueClock;
use crate::engine::{Exchange, MarketStatus};
use crate::pricing::feed::unix_now_ms;

use super::SeriesMap;

pub struct EngineClock {
    exchange: Arc<Mutex<Exchange>>,
    map: SeriesMap,
}

impl EngineClock {
    pub fn new(exchange: Arc<Mutex<Exchange>>, map: SeriesMap) -> Arc<EngineClock> {
        Arc::new(EngineClock { exchange, map })
    }
}

impl VenueClock for EngineClock {
    fn window_remaining_ms(&self, instrument_id: &str) -> Option<u64> {
        let series = self.map.instrument_series(instrument_id)?;
        let x = self.exchange.lock().expect("exchange mutex poisoned");
        let market = x.engine(series).current_market()?;
        if market.status != MarketStatus::Open {
            return None;
        }
        Some((market.closes_at * 1000).saturating_sub(unix_now_ms()))
    }

    fn now_ms(&self) -> u64 {
        unix_now_ms()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::series::SERIES;
    use crate::types::{SeriesId, Tenths};

    #[test]
    fn remaining_reads_the_instruments_own_series() {
        let exchange = Arc::new(Mutex::new(Exchange::new(Tenths(1_000_000), SERIES)));
        let map = SeriesMap::new([("a", SeriesId(0)), ("b", SeriesId(1))]);
        let clock = EngineClock::new(exchange.clone(), map);
        assert_eq!(clock.window_remaining_ms("a.current"), None, "no window yet");
        let now_s = unix_now_ms() / 1000;
        exchange.lock().unwrap().open_first_market(SeriesId(0), 1.0, now_s, 120);
        exchange.lock().unwrap().open_first_market(SeriesId(1), 1.0, now_s, 900);
        let a = clock.window_remaining_ms("a.current").unwrap();
        let b = clock.window_remaining_ms("b.current").unwrap();
        assert!(a <= 120_000 && a > 110_000, "{a}");
        assert!(b <= 900_000 && b > 890_000, "{b}");
        assert_eq!(clock.window_remaining_ms("zzz.current"), None);
    }
}
