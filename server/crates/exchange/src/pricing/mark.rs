//! Where the bots get their view of a round: the mark source.
//!
//! The market maker and the trader cohort never look at a price feed or
//! a signal directly. Per series they hold one [`MarkSource`] that answers
//! three questions about the current round: what is it worth (the mark,
//! in tenths on the positive side), how far has the underlying drifted
//! from where the round opened (basis points), and how fast is it moving
//! right now (basis points over a short lookback). The decision logic is
//! written once against those three numbers.
//!
//! [`BtcMark`] is the BTC up/down source: the digital-call model over the
//! live spot against the strike, drift as spot versus strike, velocity
//! from the feed's rolling buffer. The NBA source (`sim::nba::mark`) reads
//! only the published play-by-play and sharp frames.
//!
//! `bps_unit` scales an underlying's basis points onto the taker
//! thresholds the archetype bands are tuned in: one for BTC spot, where a
//! 5 bps move is news; larger for a win probability, where a single
//! basket moves hundreds of basis points.

use std::sync::Arc;

use crate::engine::Market;
use crate::pricing::fair_value::compute_fair_value_tenths;
use crate::pricing::feed::PriceFeed;

/// Lookback window (seconds) used for the velocity reading. Short enough
/// that a flash move is fresh, long enough to ignore single-tick jitter.
pub const VELOCITY_LOOKBACK_SECS: f64 = 2.0;

/// What one bot decision reads about the round.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarkInputs {
    /// Fair value of the positive outcome in tenths of a cent, 1–999.
    pub fv_tenths: u16,
    /// Underlying drift since the round opened, in the source's basis
    /// points, positive toward the positive outcome.
    pub drift_bps: i32,
    /// Underlying move over the last [`VELOCITY_LOOKBACK_SECS`], in the
    /// source's basis points.
    pub velocity_bps: i32,
}

pub trait MarkSource: Send + Sync + 'static {
    /// Everything a bot needs for `market` with `secs_remaining` left, or
    /// `None` when there is nothing to price on yet.
    fn inputs(&self, market: &Market, secs_remaining: f64) -> Option<MarkInputs>;

    /// How many of this source's basis points count as one threshold
    /// unit in the archetype bands (`BotParams::momentum_threshold_bps`).
    fn bps_unit(&self) -> i32 {
        1
    }

    /// The range the mark is confined to with `secs_remaining` left, in
    /// tenths: `(floor, cap)`. The bots clamp their perceived fair value
    /// to it, so a source that caps its mark (the NBA one, until the last
    /// seconds) keeps the whole cohort quoting inside the cap. The full
    /// grid by default.
    fn fv_bounds(&self, _secs_remaining: f64) -> (u16, u16) {
        (1, 999)
    }
}

/// Shared handle to a series' mark source.
pub type SharedMark = Arc<dyn MarkSource>;

/// The BTC up/down mark: digital call on spot versus strike.
pub struct BtcMark {
    feed: Arc<PriceFeed>,
}

impl BtcMark {
    pub fn new(feed: Arc<PriceFeed>) -> Arc<Self> {
        Arc::new(Self { feed })
    }
}

/// Spot versus strike in basis points, positive when spot is above.
pub fn momentum_bps(spot: f64, strike: f64) -> i32 {
    if strike <= 0.0 || !spot.is_finite() {
        return 0;
    }
    (((spot - strike) / strike) * 10_000.0).round() as i32
}

impl MarkSource for BtcMark {
    fn inputs(&self, market: &Market, secs_remaining: f64) -> Option<MarkInputs> {
        let strike = market.strike_btc_usd()?;
        let spot = self.feed.latest()?;
        let fv_tenths = compute_fair_value_tenths(spot, strike, secs_remaining)?;
        Some(MarkInputs {
            fv_tenths,
            drift_bps: momentum_bps(spot, strike),
            velocity_bps: self.feed.recent_velocity_bps(VELOCITY_LOOKBACK_SECS).unwrap_or(0),
        })
    }
}

/// A fixed mark for tests: whatever inputs it is told to report.
#[derive(Debug, Clone, Copy)]
pub struct FixedMark(pub Option<MarkInputs>);

impl MarkSource for FixedMark {
    fn inputs(&self, _market: &Market, _secs_remaining: f64) -> Option<MarkInputs> {
        self.0
    }
}

/// A BTC mark from an explicit spot and velocity, for tests that drive
/// bots against a hand-picked price: `SpotMark(spot, velocity_bps)`.
#[derive(Debug, Clone, Copy)]
pub struct SpotMark(pub f64, pub i32);

impl MarkSource for SpotMark {
    fn inputs(&self, market: &Market, secs_remaining: f64) -> Option<MarkInputs> {
        let strike = market.strike_btc_usd()?;
        Some(MarkInputs {
            fv_tenths: compute_fair_value_tenths(self.0, strike, secs_remaining)?,
            drift_bps: momentum_bps(self.0, strike),
            velocity_bps: self.1,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{MarketStatus, RoundReference};
    use crate::types::{MarketId, SeriesId};

    fn market(strike: f64) -> Market {
        Market {
            id: MarketId(1),
            series: SeriesId(0),
            reference: RoundReference::Btc { strike_btc_usd: strike },
            opened_at: 0,
            closes_at: 60,
            duration_secs: 60,
            status: MarketStatus::Open,
        }
    }

    #[test]
    fn momentum_bps_sign_and_magnitude() {
        assert_eq!(momentum_bps(100_100.0, 100_000.0), 10);
        assert_eq!(momentum_bps(99_900.0, 100_000.0), -10);
        assert_eq!(momentum_bps(100_000.0, 100_000.0), 0);
    }

    #[test]
    fn btc_mark_reads_the_feed_against_the_strike() {
        let feed = PriceFeed::new();
        let mark = BtcMark::new(feed.clone());
        assert!(mark.inputs(&market(100_000.0), 60.0).is_none(), "no price yet");
        feed.record(100_100.0);
        let i = mark.inputs(&market(100_000.0), 60.0).unwrap();
        assert!(i.fv_tenths > 800);
        assert_eq!(i.drift_bps, 10);
        assert_eq!(mark.bps_unit(), 1);
        let nba = Market { reference: RoundReference::Nba { home_team: "BOS", away_team: "LAL", pregame_home_prob: 0.6, game_seq: 1 }, ..market(0.0) };
        assert!(mark.inputs(&nba, 60.0).is_none(), "a BTC mark cannot price an NBA round");
    }
}
