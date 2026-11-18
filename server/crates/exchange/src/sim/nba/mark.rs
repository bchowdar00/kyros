//! The bots' fair value for an NBA round, from published signals only.
//!
//! [`NbaMark`] never touches the world. It reads the bus's latest
//! play-by-play and sharp-line frames and prices the round's matchup with
//! the shared model:
//!
//! * while the play-by-play shows *this* round's game, the diffusion model
//!   on the score, the game clock and the prior captured on the round's
//!   reference (frozen at open, so the live sharp line is never double
//!   counted against the margin); a final is the step function;
//! * otherwise (pregame: the stream still shows the previous game's
//!   final, or nothing yet) the sharp line if it quotes this matchup,
//!   else the reference's pregame prior.
//!
//! The underlying the takers read drift and velocity from is the win
//! probability itself, sampled on every call into a short history:
//! drift is the move since the round's first sample, velocity the move
//! over the last two seconds. One basket moves a win probability by
//! hundreds of basis points, so `bps_unit` maps those onto the taker
//! thresholds the archetype bands are tuned in.
//!
//! The mark the bots quote against is confined to 5–95¢ until the final
//! seconds of the round, then released to the model's full 1–99¢ across
//! the bots' end-of-market window, so a decided game trades at 95 with a
//! two-sided book and only pins to the boundary as the round closes. The
//! model itself (`home_prob`, the published win probability) is never
//! capped: the cap is the crowd's, not the model's.
//!
//! Between plays nothing moves the model, so the mark also carries a
//! small seeded mean-reverting wobble, stepped on the wall clock in
//! quarter-second buckets (every bot reading the mark in the same instant
//! sees the same value). It is what keeps the market maker requoting and
//! the takers seeing velocity while the game stands still; it is applied
//! before the cap, so the cap still holds.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use rand::Rng;
use rand::SeedableRng;
use rand::rngs::StdRng;

use crate::automation::nba_signals::live_home_prob;
use crate::automation::signal_bus::{SignalBus, SignalFrame};
use crate::bots::traders::decision::time_pressure;
use crate::engine::{Market, RoundReference};
use crate::pricing::feed::unix_now_ms;
use crate::pricing::mark::{MarkInputs, MarkSource, VELOCITY_LOOKBACK_SECS};
use crate::registry::{NBA_PBP_SIGNAL, NBA_SHARP_LINE_SIGNAL};
use crate::sim::nba::winprob::win_probability;

/// Win-probability basis points per taker threshold unit.
pub const NBA_BPS_UNIT: i32 = 30;
/// How far back the sample history reaches.
const HISTORY_MS: u64 = 30_000;
/// The mark's cap and floor, in tenths, while the round is not in its
/// final window.
pub const MARK_CAP_TENTHS: u16 = 950;
pub const MARK_FLOOR_TENTHS: u16 = 1000 - MARK_CAP_TENTHS;
/// Where the cap and floor end up once the round closes.
const MARK_MAX_TENTHS: u16 = 999;
const MARK_MIN_TENTHS: u16 = 1;
/// The wobble: one step per bucket, `noise = PHI * noise + SIGMA * z`,
/// so it wanders about ±1¢ and decays back to the model within seconds.
pub const DRIFT_BUCKET_MS: u64 = 250;
const DRIFT_PHI: f64 = 0.9;
const DRIFT_SIGMA: f64 = 0.004;
/// The wobble never strays further than this from the model.
const DRIFT_MAX: f64 = 0.02;
/// The seed the mark drifts on when nobody hands it one.
pub const DEFAULT_DRIFT_SEED: u64 = 0x4E42_4144_5249_4654;

/// The bounds of the mark with `secs_remaining` left: the cap until the
/// final window, then a linear release to the full grid as the bots'
/// time pressure climbs to one.
pub fn mark_bounds(secs_remaining: f64) -> (u16, u16) {
    let pressure = time_pressure(secs_remaining);
    let cap = f64::from(MARK_CAP_TENTHS) + f64::from(MARK_MAX_TENTHS - MARK_CAP_TENTHS) * pressure;
    let floor = f64::from(MARK_FLOOR_TENTHS) - f64::from(MARK_FLOOR_TENTHS - MARK_MIN_TENTHS) * pressure;
    (floor.round() as u16, cap.round() as u16)
}

#[derive(Debug, Default)]
struct Samples {
    /// (ts_ms, P(home)) newest last, bounded by [`HISTORY_MS`].
    history: VecDeque<(u64, f64)>,
    /// The round the history belongs to and its first sample.
    round: Option<(u32, f64)>,
}

/// The seeded wobble, advanced one AR(1) step per elapsed bucket.
#[derive(Debug)]
struct Drift {
    rng: StdRng,
    value: f64,
    bucket: u64,
}

impl Drift {
    fn new(seed: u64) -> Self {
        Self { rng: StdRng::seed_from_u64(seed), value: 0.0, bucket: 0 }
    }

    /// The wobble for the bucket `now_ms` falls in. Buckets skipped while
    /// nobody asked are stepped through (bounded) so the process keeps
    /// its statistics whatever the polling cadence.
    fn at(&mut self, now_ms: u64) -> f64 {
        let bucket = now_ms / DRIFT_BUCKET_MS;
        let steps = bucket.saturating_sub(self.bucket).min(64);
        for _ in 0..steps {
            self.value = (DRIFT_PHI * self.value + DRIFT_SIGMA * gaussian(&mut self.rng)).clamp(-DRIFT_MAX, DRIFT_MAX);
        }
        self.bucket = bucket;
        self.value
    }
}

/// A standard normal draw (Box–Muller).
fn gaussian<R: Rng + ?Sized>(rng: &mut R) -> f64 {
    let u1: f64 = rng.random::<f64>().max(f64::MIN_POSITIVE);
    let u2: f64 = rng.random();
    (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
}

pub struct NbaMark {
    bus: Arc<SignalBus>,
    samples: Mutex<Samples>,
    /// `None` when the mark is quiet (tests that pin exact values).
    drift: Mutex<Option<Drift>>,
}

impl NbaMark {
    /// A mark drifting on [`DEFAULT_DRIFT_SEED`].
    pub fn new(bus: Arc<SignalBus>) -> Arc<Self> {
        Self::seeded(bus, DEFAULT_DRIFT_SEED)
    }

    /// A mark whose wobble is drawn from `seed`.
    pub fn seeded(bus: Arc<SignalBus>, seed: u64) -> Arc<Self> {
        Arc::new(Self { bus, samples: Mutex::new(Samples::default()), drift: Mutex::new(Some(Drift::new(seed))) })
    }

    /// A mark with no wobble: exactly the model, capped.
    pub fn quiet(bus: Arc<SignalBus>) -> Arc<Self> {
        Arc::new(Self { bus, samples: Mutex::new(Samples::default()), drift: Mutex::new(None) })
    }

    /// The wobble right now; zero for a quiet mark.
    fn drift_now(&self, now_ms: u64) -> f64 {
        self.drift.lock().expect("nba mark drift poisoned").as_mut().map_or(0.0, |d| d.at(now_ms))
    }

    /// The model's P(home) for `market` from the published frames.
    pub fn home_prob(&self, market: &Market) -> Option<f64> {
        let RoundReference::Nba { home_team, away_team, pregame_home_prob, .. } = market.reference else {
            return None;
        };
        let pbp = self.bus.latest(NBA_PBP_SIGNAL);
        if let Some(pbp) = pbp.as_deref()
            && text(pbp, "home_team") == Some(home_team)
            && text(pbp, "away_team") == Some(away_team)
        {
            let home = uint(pbp, "home_score")?;
            let away = uint(pbp, "away_score")?;
            if text(pbp, "phase") == Some("final") {
                return Some(win_probability(home as f64 - away as f64, 0.0, 0.0));
            }
            let quarter = uint(pbp, "quarter")? as u8;
            let clock = decimal(pbp, "clock_remaining")?;
            return Some(live_home_prob(home as u32, away as u32, quarter, clock, pregame_home_prob));
        }
        // Pregame: the sharp line for this matchup, else the captured prior.
        let sharp = self.bus.latest(NBA_SHARP_LINE_SIGNAL);
        if let Some(sharp) = sharp.as_deref()
            && text(sharp, "home_team") == Some(home_team)
            && text(sharp, "away_team") == Some(away_team)
            && let Some(p) = decimal(sharp, "home_prob")
        {
            return Some(p);
        }
        Some(pregame_home_prob)
    }

    fn sample(&self, market_id: u32, p: f64) -> (i32, i32) {
        let now = unix_now_ms();
        let mut s = self.samples.lock().expect("nba mark samples poisoned");
        if s.round.is_none_or(|(id, _)| id != market_id) {
            s.round = Some((market_id, p));
            s.history.clear();
        }
        let cutoff = now.saturating_sub(HISTORY_MS);
        while s.history.front().is_some_and(|(t, _)| *t < cutoff) {
            s.history.pop_front();
        }
        s.history.push_back((now, p));
        let open = s.round.map(|(_, p0)| p0).unwrap_or(p);
        let lookback = now.saturating_sub((VELOCITY_LOOKBACK_SECS * 1000.0) as u64);
        let anchor = s
            .history
            .iter()
            .find(|(t, _)| *t >= lookback)
            .map(|(_, v)| *v)
            .unwrap_or(p);
        (bps(p - open), bps(p - anchor))
    }
}

fn bps(delta: f64) -> i32 {
    (delta * 10_000.0).round() as i32
}

fn text<'a>(frame: &'a SignalFrame, field: &str) -> Option<&'a str> {
    frame.values.get(field).and_then(|v| v.as_str())
}

fn uint(frame: &SignalFrame, field: &str) -> Option<u64> {
    frame.values.get(field).and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
}

fn decimal(frame: &SignalFrame, field: &str) -> Option<f64> {
    frame.values.get(field).and_then(|v| v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
}

impl MarkSource for NbaMark {
    fn inputs(&self, market: &Market, secs_remaining: f64) -> Option<MarkInputs> {
        let p = self.home_prob(market)? + self.drift_now(unix_now_ms());
        let (floor, cap) = mark_bounds(secs_remaining);
        let capped = p.clamp(f64::from(floor) / 1000.0, f64::from(cap) / 1000.0);
        let (drift_bps, velocity_bps) = self.sample(market.id.0, capped);
        let fv_tenths = ((capped * 1000.0).round() as i64).clamp(i64::from(floor), i64::from(cap)) as u16;
        Some(MarkInputs { fv_tenths, drift_bps, velocity_bps })
    }

    fn bps_unit(&self) -> i32 {
        NBA_BPS_UNIT
    }

    fn fv_bounds(&self, secs_remaining: f64) -> (u16, u16) {
        mark_bounds(secs_remaining)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::automation::nba_signals::{pbp_frame, sharp_frame};
    use crate::engine::MarketStatus;
    use crate::registry;
    use crate::sim::nba::engine::GameEngine;
    use crate::sim::nba::league::team;
    use crate::sim::nba::winprob::{REGULATION_SECONDS, prior_drift};
    use crate::types::{MarketId, SeriesId};

    fn round(id: u32, home: &'static str, away: &'static str, prior: f64) -> Market {
        Market {
            id: MarketId(id),
            series: SeriesId(0),
            reference: RoundReference::Nba { home_team: home, away_team: away, pregame_home_prob: prior, game_seq: u64::from(id) },
            opened_at: 0,
            closes_at: 300,
            duration_secs: 300,
            status: MarketStatus::Open,
        }
    }

    #[test]
    fn prices_pregame_from_the_line_and_in_play_from_the_frozen_prior() {
        let bus = SignalBus::new(registry::for_tests(), None);
        let mark = NbaMark::quiet(bus.clone());
        let m = round(7, "BOS", "SAS", 0.62);
        // Nothing published: the captured prior.
        assert_eq!(mark.home_prob(&m), Some(0.62));
        // The previous game's final is still on the stream: still pregame.
        let prev = GameEngine::play_full(team("LAL"), team("NYK"), 1);
        bus.publish(pbp_frame(1, prev.last().unwrap()));
        assert_eq!(mark.home_prob(&m), Some(0.62));
        // The sharp line for this matchup wins over the captured prior.
        bus.publish(sharp_frame(2, "BOS", "SAS", 0.65));
        assert_eq!(mark.home_prob(&m), Some(0.65));
        let i = mark.inputs(&m, 200.0).unwrap();
        assert_eq!(i.fv_tenths, 650);
        assert_eq!(i.drift_bps, 0);
        assert_eq!(mark.bps_unit(), NBA_BPS_UNIT);
        // Tipoff: the model on the round's frozen prior, whatever the line does now.
        let mut engine = GameEngine::new(team("BOS"), team("SAS"), 3);
        bus.publish(pbp_frame(3, &engine.tipoff()));
        bus.publish(sharp_frame(4, "BOS", "SAS", 0.90));
        let p = mark.home_prob(&m).unwrap();
        assert!((p - win_probability(0.0, REGULATION_SECONDS, prior_drift(0.62))).abs() < 1e-6);
        // A play moves it; drift since the round's first sample follows.
        let mut last = None;
        while let Some(plays) = engine.play_possession() {
            for e in plays {
                bus.publish(pbp_frame(5, &e));
                last = Some(e);
            }
            if engine.possession() > 60 {
                break;
            }
        }
        let e = last.unwrap();
        let expected = live_home_prob(e.home_score, e.away_score, e.quarter, e.clock_remaining, 0.62);
        assert!((mark.home_prob(&m).unwrap() - expected).abs() < 1e-9);
        let i = mark.inputs(&m, 100.0).unwrap();
        assert_eq!(i.drift_bps, bps(expected - 0.65));
        // A new round resets the drift anchor.
        let m2 = round(8, "BOS", "SAS", 0.62);
        assert_eq!(mark.inputs(&m2, 100.0).unwrap().drift_bps, 0);
        // A final is the step function.
        while let Some(plays) = engine.play_possession() {
            for e in plays {
                bus.publish(pbp_frame(6, &e));
                last = Some(e);
            }
        }
        let fin = last.unwrap();
        let p = mark.home_prob(&m).unwrap();
        assert_eq!(p, if fin.home_score > fin.away_score { 0.99 } else { 0.01 });
        // A BTC round is not ours.
        let btc = Market { reference: RoundReference::Btc { strike_btc_usd: 1.0 }, ..m };
        assert!(mark.inputs(&btc, 1.0).is_none());
    }

    #[test]
    fn the_crowd_mark_is_capped_until_the_final_window() {
        use crate::bots::traders::decision::FINAL_WINDOW_SECS;
        assert_eq!(mark_bounds(60.0), (MARK_FLOOR_TENTHS, MARK_CAP_TENTHS));
        assert_eq!(mark_bounds(FINAL_WINDOW_SECS), (MARK_FLOOR_TENTHS, MARK_CAP_TENTHS));
        assert_eq!(mark_bounds(0.0), (1, 999));
        let (floor, cap) = mark_bounds(FINAL_WINDOW_SECS / 2.0);
        assert!(floor > 1 && floor < MARK_FLOOR_TENTHS, "{floor}");
        assert!(cap > MARK_CAP_TENTHS && cap < 999, "{cap}");

        let bus = SignalBus::new(registry::for_tests(), None);
        let mark = NbaMark::quiet(bus);
        // A decided game: the model says 99, the crowd quotes 95 until the close.
        let sure = round(9, "BOS", "SAS", 0.99);
        assert_eq!(mark.home_prob(&sure), Some(0.99), "the model is never capped");
        assert_eq!(mark.inputs(&sure, 60.0).unwrap().fv_tenths, MARK_CAP_TENTHS);
        assert_eq!(mark.fv_bounds(60.0), (MARK_FLOOR_TENTHS, MARK_CAP_TENTHS));
        assert_eq!(mark.inputs(&sure, 0.0).unwrap().fv_tenths, 990);
        let lost = round(10, "BOS", "SAS", 0.01);
        assert_eq!(mark.inputs(&lost, 60.0).unwrap().fv_tenths, MARK_FLOOR_TENTHS);
        assert_eq!(mark.inputs(&lost, 0.0).unwrap().fv_tenths, 10);
        // Inside the band nothing changes.
        let even = round(11, "BOS", "SAS", 0.62);
        assert_eq!(mark.inputs(&even, 60.0).unwrap().fv_tenths, 620);
    }

    #[test]
    fn the_mark_wobbles_within_bounds_and_reproducibly() {
        let mut a = Drift::new(7);
        let mut b = Drift::new(7);
        let mut c = Drift::new(8);
        let mut values = Vec::new();
        let mut differs = false;
        for bucket in 1..=400u64 {
            let t = bucket * DRIFT_BUCKET_MS;
            let v = a.at(t);
            assert_eq!(v, b.at(t), "same seed, same wobble");
            assert!(v.abs() <= DRIFT_MAX, "{v}");
            differs |= v != c.at(t);
            values.push(v);
        }
        assert!(differs, "different seeds differ");
        // The same bucket asked twice is the same value; it moves between buckets.
        assert_eq!(a.at(400 * DRIFT_BUCKET_MS + 10), *values.last().unwrap());
        let distinct: std::collections::BTreeSet<u64> = values.iter().map(|v| v.to_bits()).collect();
        assert!(distinct.len() > 100, "the wobble moves: {} distinct values", distinct.len());
        let sd = (values.iter().map(|v| v * v).sum::<f64>() / values.len() as f64).sqrt();
        assert!(sd > 0.002 && sd < 0.015, "about a cent of wobble: sd {sd}");

        // On the mark: the wobble rides on the model and the cap still holds.
        let bus = SignalBus::new(registry::for_tests(), None);
        let mark = NbaMark::seeded(bus, 3);
        let m = round(12, "BOS", "SAS", 0.62);
        let fv = mark.inputs(&m, 60.0).unwrap().fv_tenths;
        assert!((600..=640).contains(&fv), "{fv}");
        let sure = round(13, "BOS", "SAS", 0.99);
        assert_eq!(mark.inputs(&sure, 60.0).unwrap().fv_tenths, MARK_CAP_TENTHS);
    }
}
