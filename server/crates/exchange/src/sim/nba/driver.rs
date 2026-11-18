//! The NBA round driver: what an `nba-h2h` round is about.
//!
//! A round opens on the world's current matchup with the sharp line's
//! pregame home probability captured as the reference, and the driver
//! tells the world to play that game inside the round. Finality and the
//! outcome are read from the *published* play-by-play, never the world:
//! the round is final once the stream shows phase `final` for this
//! round's matchup (the matchup guard is what rejects the previous game's
//! final during the pregame), and home wins iff its score is higher. A
//! missing score resolves away as a house rule.
//!
//! The driver never returns before the round's close; if overtime runs
//! past it the round simply runs long and the next one opens on the
//! wall clock when the final lands.
//!
//! Every open is also announced on the price feed as a `MarketReset`
//! with a zero strike: the browser learns round boundaries for every
//! series from `/ws/price`, and an NBA round has no strike to carry.

use std::sync::Arc;
use std::time::Duration;

use crate::automation::signal_bus::SignalBus;
use crate::engine::{Market, Outcome, RoundClose, RoundReference};
use crate::pricing::feed::PriceFeed;
use crate::registry::{NBA_PBP_SIGNAL, NBA_SHARP_LINE_SIGNAL};
use crate::scheduler::{RoundDriver, RoundPlan, RoundResolution, sleep_until_unix};

use super::world::{MatchupState, WindowSpec, WorldHandle};

const POLL: Duration = Duration::from_millis(200);

pub struct NbaDriver {
    world: WorldHandle,
    bus: Arc<SignalBus>,
    /// Announces round boundaries to `/ws/price` subscribers; `None` in
    /// tests that have no feed.
    feed: Option<Arc<PriceFeed>>,
    /// How long each round runs once it opens.
    duration_secs: u64,
}

impl NbaDriver {
    pub fn new(world: WorldHandle, bus: Arc<SignalBus>, duration_secs: u64) -> Self {
        Self { world, bus, feed: None, duration_secs }
    }

    /// Also announce every open on the price feed.
    pub fn with_feed(mut self, feed: Arc<PriceFeed>) -> Self {
        self.feed = Some(feed);
        self
    }

    /// The reference for the world's current matchup, once the sharp line
    /// has quoted it: the line is the public prior the round opens on.
    async fn reference_for_current(&self) -> RoundReference {
        loop {
            let state: MatchupState = self.world.matchup();
            if let Some(p) = self.sharp_prob_for(&state) {
                return RoundReference::Nba {
                    home_team: state.home,
                    away_team: state.away,
                    pregame_home_prob: p,
                    game_seq: state.game_seq,
                };
            }
            tokio::time::sleep(POLL).await;
        }
    }

    fn sharp_prob_for(&self, state: &MatchupState) -> Option<f64> {
        let f = self.bus.latest(NBA_SHARP_LINE_SIGNAL)?;
        let s = |k: &str| f.values.get(k).and_then(|v| v.as_str());
        if s("home_team") != Some(state.home) || s("away_team") != Some(state.away) {
            return None;
        }
        s("home_prob").and_then(|p| p.parse::<f64>().ok())
    }

    /// The published final for `current`'s matchup, if it has landed.
    fn published_final(&self, current: &Market) -> Option<(u32, u32)> {
        let RoundReference::Nba { home_team, away_team, .. } = current.reference else {
            return None;
        };
        let f = self.bus.latest(NBA_PBP_SIGNAL)?;
        let s = |k: &str| f.values.get(k).and_then(|v| v.as_str());
        let n = |k: &str| f.values.get(k).and_then(|v| v.as_u64()).map(|v| v as u32);
        if s("phase") != Some("final") || s("home_team") != Some(home_team) || s("away_team") != Some(away_team) {
            return None;
        }
        Some((n("home_score").unwrap_or(0), n("away_score").unwrap_or(0)))
    }
}

impl RoundDriver for NbaDriver {
    async fn first_plan(&mut self) -> RoundPlan {
        RoundPlan::from_now(self.reference_for_current().await, self.duration_secs)
    }

    async fn await_resolution(&mut self, current: &Market) -> RoundResolution {
        sleep_until_unix(current.closes_at).await;
        let (home_score, away_score) = loop {
            if let Some(score) = self.published_final(current) {
                break score;
            }
            tokio::time::sleep(POLL).await;
        };
        let outcome = if home_score > away_score { Outcome::Yes } else { Outcome::No };
        // The world flipped to the next matchup on its final; the sharp
        // line announces it within a poll.
        let next = RoundPlan::from_now(self.reference_for_current().await, self.duration_secs);
        RoundResolution { outcome, close: RoundClose::Nba { home_score, away_score }, next: Some(next) }
    }

    fn on_opened(&mut self, market: &Market) {
        let (opens_at_ms, closes_at_ms) = (market.opened_at * 1000, market.closes_at * 1000);
        self.world.play_window(WindowSpec { opens_at_ms, closes_at_ms });
        if let Some(feed) = &self.feed {
            feed.reset_for_new_market_in(market.series, market.id.0, 0.0, opens_at_ms, closes_at_ms);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::automation::nba_signals::spawn_nba_publishers;
    use crate::automation::runtime::runtime_channel;
    use crate::engine::{Exchange, MarketStatus};
    use crate::registry;
    use crate::scheduler::spawn_with_driver;
    use crate::series::{NBA_OUTCOMES, SeriesSpec};
    use crate::sim::nba::world::{Pacing, spawn_world};
    use crate::types::{SeriesId, Tenths};
    use std::sync::Mutex;

    /// A whole round on a real exchange: the world plays a game inside
    /// the window, the publishers put it on the bus, the driver settles
    /// the round from the published final and opens the next one on the
    /// announced matchup.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn scheduler_settles_an_nba_round_from_published_state() {
        let spec = SeriesSpec { id: SeriesId(0), slug: "nba-h2h", duration_secs: 3, underlying: "nba", outcomes: NBA_OUTCOMES };
        let exchange = Arc::new(Mutex::new(Exchange::new(Tenths(1_000_000), std::slice::from_ref(&spec))));
        let bus = SignalBus::new(registry::for_tests(), None);
        let pacing = Pacing { pregame_secs: 0, lead_secs: 1, overtime_possession_secs: 0.01 };
        let world = spawn_world(11, pacing);
        spawn_nba_publishers(world.clone(), bus.clone(), 11);
        let (runtime, _rx) = runtime_channel();
        let mut frames = bus.subscribe();
        let driver = NbaDriver::new(world.clone(), bus.clone(), 3);
        spawn_with_driver(exchange.clone(), SeriesId(0), driver, runtime, |_| {});

        // The first round opens on the sharp line's matchup and prior.
        let first = loop {
            if let Some(m) = exchange.lock().unwrap().engine(SeriesId(0)).current_market().copied() {
                break m;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        let RoundReference::Nba { home_team, away_team, pregame_home_prob, game_seq } = first.reference else {
            panic!("NBA reference");
        };
        assert_eq!(game_seq, 1);
        assert_ne!(home_team, away_team);
        assert!(pregame_home_prob > 0.0 && pregame_home_prob < 1.0);

        // It settles on the published final and the next round opens on
        // a different matchup.
        let resolved = loop {
            let last = exchange.lock().unwrap().engine(SeriesId(0)).resolved_markets(1).next().copied();
            if let Some(r) = last {
                break r;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        let MarketStatus::Resolved { outcome, close: RoundClose::Nba { home_score, away_score } } = resolved.status else {
            panic!("resolved with a final score: {:?}", resolved.status);
        };
        assert_ne!(home_score, away_score);
        assert_eq!(outcome, if home_score > away_score { Outcome::Yes } else { Outcome::No });
        // The score came from the published final of this matchup (the
        // next game may already be on the stream by now).
        let mut published_final = None;
        while let Ok(f) = frames.try_recv() {
            if f.signal_id == NBA_PBP_SIGNAL
                && f.values["phase"] == "final"
                && f.values["home_team"] == home_team
                && f.values["away_team"] == away_team
            {
                published_final = Some(f);
            }
        }
        let pbp = published_final.expect("final published for the round's matchup");
        assert_eq!(pbp.values["home_score"], home_score);
        assert_eq!(pbp.values["away_score"], away_score);
        let next = exchange.lock().unwrap().engine(SeriesId(0)).current_market().copied().unwrap();
        let RoundReference::Nba { game_seq: next_seq, home_team: h2, away_team: a2, .. } = next.reference else {
            panic!("NBA reference");
        };
        assert_eq!(next_seq, 2);
        assert!(h2 != home_team || a2 != away_team);
    }
}
