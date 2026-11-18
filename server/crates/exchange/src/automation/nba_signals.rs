//! Publishes the NBA world as signals.
//!
//! Two feeds observe the world and two derived signals are computed from
//! *the frames those feeds published*, never from the world, so a strategy
//! trades on exactly what the model saw:
//!
//! | signal                  | pattern | published when                                   |
//! |-------------------------|---------|--------------------------------------------------|
//! | `nba.sim.pbp.v1`        | push    | every play, one atomic frame, `clock_remaining` last |
//! | `nba.sim.sharp_line.v1` | poll    | every 500 ms: the world's true home probability plus slow AR(1) noise, printed on a move of at least 0.005 or a matchup change |
//! | `nba.sim.margin.v1`     | derived | after every play, from the play's scores        |
//! | `nba.sim.winprob.v1`    | derived | after every play (the diffusion model on the frozen pregame prior) and on sharp prints while no game is in play (the prior itself) |
//!
//! The sharp line keeps moving in play because it tracks the game; that is
//! why the model freezes the prior at tipoff rather than reading the live
//! line, which would double count the margin. The sharp print on a matchup
//! change is how the next game is announced before its window opens.
//!
//! One task per process does all four, so a play's frames go out in order
//! under one timestamp: play, margin, win probability.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use rand::Rng;
use rand::SeedableRng;
use rand::rngs::StdRng;
use serde_json::{Value as Json, json};
use tokio::sync::broadcast::error::RecvError;

use crate::automation::signal_bus::{Quality, SignalBus, SignalFrame};
use crate::pricing::feed::unix_now_ms;
use crate::registry::{NBA_MARGIN_SIGNAL, NBA_PBP_SIGNAL, NBA_SHARP_LINE_SIGNAL, NBA_WINPROB_SIGNAL};
use crate::sim::nba::engine::{Action, GameEvent, Phase};
use crate::sim::nba::league::player_name;
use crate::sim::nba::winprob::{game_seconds_remaining, prior_drift, win_probability};
use crate::sim::nba::world::{MatchupState, WorldHandle};

/// How often the sharp line samples the world.
pub const SHARP_POLL: Duration = Duration::from_millis(500);
/// Smallest move the sharp line prints.
pub const SHARP_MIN_MOVE: f64 = 0.005;
const SHARP_PHI: f64 = 0.9;
const SHARP_SIGMA: f64 = 0.008;
const SHARP_MIN: f64 = 0.02;
const SHARP_MAX: f64 = 0.98;

fn frame(signal_id: &str, ts_ms: u64, values: Vec<(&str, Json)>) -> SignalFrame {
    let values: BTreeMap<String, Json> = values.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
    SignalFrame { signal_id: signal_id.to_string(), ts_ms, quality: Quality::ready(), values }
}

/// A probability as a six-place decimal string.
pub fn prob(p: f64) -> String {
    format!("{p:.6}")
}

/// One play as a frame. The registry lists `clock_remaining` last; the
/// values map is keyed, so order there is by name, but the platform
/// ingests the frame whole.
pub fn pbp_frame(ts_ms: u64, e: &GameEvent) -> SignalFrame {
    frame(
        NBA_PBP_SIGNAL,
        ts_ms,
        vec![
            ("seq", json!(e.seq)),
            ("action", json!(e.action.as_str())),
            ("result", json!(e.result.as_str())),
            ("team", json!(e.team)),
            ("player", json!(player_name(e.player))),
            ("phase", json!(e.phase.as_str())),
            ("home_team", json!(e.home_team)),
            ("away_team", json!(e.away_team)),
            ("quarter", json!(e.quarter)),
            ("home_score_before", json!(e.home_score_before)),
            ("away_score_before", json!(e.away_score_before)),
            ("home_score", json!(e.home_score)),
            ("away_score", json!(e.away_score)),
            ("clock_remaining", json!(format!("{:.1}", e.clock_remaining))),
        ],
    )
}

pub fn margin_frame(ts_ms: u64, margin: i64) -> SignalFrame {
    frame(NBA_MARGIN_SIGNAL, ts_ms, vec![("value", json!(margin.to_string()))])
}

pub fn winprob_frame(ts_ms: u64, home: f64) -> SignalFrame {
    frame(NBA_WINPROB_SIGNAL, ts_ms, vec![("home", json!(prob(home)))])
}

pub fn sharp_frame(ts_ms: u64, home_team: &str, away_team: &str, home_prob: f64) -> SignalFrame {
    frame(
        NBA_SHARP_LINE_SIGNAL,
        ts_ms,
        vec![
            ("home_team", json!(home_team)),
            ("away_team", json!(away_team)),
            ("home_prob", json!(prob(home_prob))),
            ("away_prob", json!(prob(1.0 - home_prob))),
        ],
    )
}

/// The model's P(home) for a play given the frozen pregame prior.
pub fn live_home_prob(home_score: u32, away_score: u32, quarter: u8, clock_remaining: f64, prior: f64) -> f64 {
    let margin = f64::from(home_score) - f64::from(away_score);
    win_probability(margin, game_seconds_remaining(quarter, clock_remaining), prior_drift(prior))
}

/// The derivation engine: folds published plays and sharp prints into
/// margin and win-probability frames.
#[derive(Debug, Default)]
pub struct Derivation {
    /// The latest sharp line: (home, away, home_prob).
    sharp: Option<(String, String, f64)>,
    /// The prior frozen at tipoff for the game in play.
    frozen_prior: Option<f64>,
    in_play: bool,
}

impl Derivation {
    /// A sharp print: republish it as the model's win probability while no
    /// game is in play.
    pub fn on_sharp(&mut self, ts_ms: u64, home_team: &str, away_team: &str, home_prob: f64) -> Vec<SignalFrame> {
        self.sharp = Some((home_team.to_string(), away_team.to_string(), home_prob));
        if self.in_play { Vec::new() } else { vec![winprob_frame(ts_ms, home_prob)] }
    }

    /// A play: the margin and the model's win probability. At tipoff the
    /// sharp prior for this matchup is frozen; a tipoff with no matching
    /// sharp print yet uses a level prior.
    pub fn on_play(&mut self, ts_ms: u64, e: &GameEvent) -> Vec<SignalFrame> {
        if e.action == Action::Tipoff {
            let prior = match &self.sharp {
                Some((h, a, p)) if h == e.home_team && a == e.away_team => *p,
                _ => 0.5,
            };
            self.frozen_prior = Some(prior);
            self.in_play = true;
        }
        let prior = self.frozen_prior.unwrap_or(0.5);
        let margin = i64::from(e.home_score) - i64::from(e.away_score);
        let p = if e.phase == Phase::Final {
            win_probability(margin as f64, 0.0, 0.0)
        } else {
            live_home_prob(e.home_score, e.away_score, e.quarter, e.clock_remaining, prior)
        };
        if e.phase == Phase::Final {
            self.in_play = false;
            self.frozen_prior = None;
        }
        vec![margin_frame(ts_ms, margin), winprob_frame(ts_ms, p)]
    }

    pub fn in_play(&self) -> bool {
        self.in_play
    }
}

/// The sharp line feed: the world's truth plus slow AR(1) noise, printed
/// on a move or a matchup change.
#[derive(Debug)]
pub struct SharpLine {
    rng: StdRng,
    noise: f64,
    last: Option<(&'static str, &'static str, f64)>,
}

impl SharpLine {
    pub fn new(seed: u64) -> Self {
        Self { rng: StdRng::seed_from_u64(seed), noise: 0.0, last: None }
    }

    /// One poll of the world. `Some` when a print is due.
    pub fn step(&mut self, ts_ms: u64, state: MatchupState) -> Option<SignalFrame> {
        self.noise = SHARP_PHI * self.noise + SHARP_SIGMA * gaussian(&mut self.rng);
        let value = (state.true_home_prob + self.noise).clamp(SHARP_MIN, SHARP_MAX);
        let changed = match self.last {
            None => true,
            Some((h, a, p)) => h != state.home || a != state.away || (value - p).abs() >= SHARP_MIN_MOVE,
        };
        if !changed {
            return None;
        }
        self.last = Some((state.home, state.away, value));
        Some(sharp_frame(ts_ms, state.home, state.away, value))
    }
}

/// A standard normal draw (Box–Muller).
fn gaussian<R: Rng + ?Sized>(rng: &mut R) -> f64 {
    let u1: f64 = rng.random::<f64>().max(f64::MIN_POSITIVE);
    let u2: f64 = rng.random();
    (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
}

/// Spawn the publisher over the running world. Runs for the life of the
/// process.
pub fn spawn_nba_publishers(world: WorldHandle, bus: Arc<SignalBus>, seed: u64) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut events = world.subscribe();
        let mut ticker = tokio::time::interval(SHARP_POLL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut derivation = Derivation::default();
        let mut sharp = SharpLine::new(seed);
        let mut announced = false;
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let now = unix_now_ms();
                    let state = world.matchup();
                    if let Some(f) = sharp.step(now, state) {
                        let (h, a, p) = (state.home, state.away, f.values["home_prob"].as_str().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.5));
                        bus.publish(f);
                        for d in derivation.on_sharp(now, h, a, p) {
                            bus.publish(d);
                        }
                    }
                }
                ev = events.recv() => match ev {
                    Ok(e) => {
                        let now = unix_now_ms();
                        if !announced {
                            announced = true;
                            tracing::info!("nba signals: first play published");
                        }
                        bus.publish(pbp_frame(now, &e));
                        for d in derivation.on_play(now, &e) {
                            bus.publish(d);
                        }
                    }
                    Err(RecvError::Lagged(n)) => tracing::warn!(dropped = n, "nba signals: lagged behind the world"),
                    Err(RecvError::Closed) => break,
                },
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
    use crate::sim::nba::engine::GameEngine;
    use crate::sim::nba::league::team;
    use crate::sim::nba::winprob::REGULATION_SECONDS;
    use rust_decimal::Decimal;
    use std::str::FromStr;

    #[test]
    fn every_frame_types_against_the_generated_registry() {
        let reg = registry::for_tests();
        let mut store = SignalStore::from_registry(&reg);
        let events = GameEngine::play_full(team("BOS"), team("SAS"), 1);
        let mut d = Derivation::default();
        let mut frames = vec![sharp_frame(1, "BOS", "SAS", 0.6142)];
        frames.extend(d.on_sharp(1, "BOS", "SAS", 0.6142));
        for (i, e) in events.iter().enumerate() {
            frames.push(pbp_frame(2 + i as u64, e));
            frames.extend(d.on_play(2 + i as u64, e));
        }
        for f in &frames {
            assert!(store.enable(&f.signal_id), "{} in registry", f.signal_id);
            let def = reg.signals.iter().find(|s| s.signal_id == f.signal_id).unwrap();
            let event = to_signal_event(f, &field_types(def)).unwrap_or_else(|e| panic!("{}: {e}", f.signal_id));
            let typed = store.ingest(&event).unwrap_or_else(|e| panic!("{}: {e}", f.signal_id));
            assert_eq!(typed.len(), def.fields.len(), "{}", f.signal_id);
        }
        let last = events.last().unwrap();
        assert_eq!(store.field(NBA_PBP_SIGNAL, "phase"), Some(&Value::Text("final".into())));
        assert_eq!(store.field(NBA_PBP_SIGNAL, "home_score"), Some(&Value::from_u64(u64::from(last.home_score))));
        let margin = i64::from(last.home_score) - i64::from(last.away_score);
        assert_eq!(store.field(NBA_MARGIN_SIGNAL, "value"), Some(&Value::Decimal(Decimal::from(margin))));
        assert_eq!(store.field(NBA_SHARP_LINE_SIGNAL, "away_prob"), Some(&Value::Decimal(Decimal::from_str("0.385800").unwrap())));
    }

    #[test]
    fn derivation_freezes_the_prior_at_tipoff_and_releases_it_at_the_final() {
        let mut d = Derivation::default();
        // Pregame: the sharp line is the model.
        let f = d.on_sharp(1, "LAL", "BOS", 0.62);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].values["home"], "0.620000");
        let mut engine = GameEngine::new(team("LAL"), team("BOS"), 4);
        let tip = engine.tipoff();
        let f = d.on_play(2, &tip);
        assert_eq!(f[0].values["value"], "0");
        assert_eq!(f[1].values["home"], prob(win_probability(0.0, REGULATION_SECONDS, prior_drift(0.62))));
        assert!(d.in_play());
        // In play the sharp line moves but the model ignores it.
        assert!(d.on_sharp(3, "LAL", "BOS", 0.80).is_empty());
        let mut last_home = 0.0;
        let mut final_event = None;
        while let Some(plays) = engine.play_possession() {
            for e in plays {
                let f = d.on_play(4, &e);
                let margin: i64 = f[0].values["value"].as_str().unwrap().parse().unwrap();
                assert_eq!(margin, i64::from(e.home_score) - i64::from(e.away_score));
                last_home = f[1].values["home"].as_str().unwrap().parse().unwrap();
                if e.phase == Phase::Final {
                    final_event = Some(e);
                }
            }
        }
        let fin = final_event.unwrap();
        assert_eq!(last_home, if fin.home_score > fin.away_score { 0.99 } else { 0.01 });
        assert!(!d.in_play());
        // The next matchup's line becomes the model again.
        let f = d.on_sharp(5, "SAS", "TOR", 0.55);
        assert_eq!(f[0].values["home"], "0.550000");
        // A tipoff whose matchup the sharp line never quoted gets a level prior.
        let mut other = GameEngine::new(team("NYK"), team("PHI"), 9);
        let f = d.on_play(6, &other.tipoff());
        assert_eq!(f[1].values["home"], "0.500000");
    }

    #[test]
    fn sharp_line_prints_on_matchup_change_and_on_moves_only() {
        let mut s = SharpLine::new(1);
        let state = MatchupState { home: "BOS", away: "LAL", game_seq: 1, true_home_prob: 0.6 };
        let first = s.step(1, state).expect("first poll prints");
        assert_eq!(first.values["home_team"], "BOS");
        let p: f64 = first.values["home_prob"].as_str().unwrap().parse().unwrap();
        assert!((p - 0.6).abs() < 0.05, "{p}");
        let mut prints = 0;
        for i in 0..200u64 {
            if s.step(2 + i, state).is_some() {
                prints += 1;
            }
        }
        assert!(prints < 200, "small noise is not printed every poll: {prints}");
        assert!(prints > 0, "noise wanders enough to print sometimes");
        let next = MatchupState { home: "SAS", away: "TOR", game_seq: 2, true_home_prob: 0.5 };
        let f = s.step(500, next).expect("matchup change prints");
        assert_eq!(f.values["home_team"], "SAS");
        assert_eq!(f.values["away_team"], "TOR");
        let p: f64 = f.values["home_prob"].as_str().unwrap().parse().unwrap();
        assert!((SHARP_MIN..=SHARP_MAX).contains(&p));
    }
}
