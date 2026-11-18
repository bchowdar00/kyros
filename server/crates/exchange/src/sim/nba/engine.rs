//! The pure, clock-free possession engine.
//!
//! A game is 200 regulation possessions (50 per quarter), each producing a
//! short list of [`GameEvent`]s, then 20-possession overtime periods until
//! the score is not tied. Every event carries the full game context —
//! action, result, team, player, quarter, clock, scores before and after,
//! matchup, phase — so *the events are the score*: a consumer replaying
//! them reconstructs exactly the final the round settles on, and a test
//! pins that.
//!
//! Per possession the finishing player is drawn by usage weight, then:
//!
//! | branch    | probability | result                                                    |
//! |-----------|-------------|-----------------------------------------------------------|
//! | turnover  | 0.13        | one `turnover`                                            |
//! | foul trip | 0.12        | a `foul` by a defender, then two `free_throw`s at 0.78   |
//! | shot      | remainder   | a two or three by `three_rate`; made twos draw an and-one |
//!
//! A shot's make rate is the base (0.36 for threes, 0.53 for twos) plus
//! the team shift plus the player's adjustment *relative to the roster*
//! (each roster's usage-weighted adjustment is normalised to zero, so
//! stars still shoot better than role players but no roster is better
//! than another: net rating is the only source of team strength),
//! clamped. The team shift is how net rating enters: `eps = net_advantage
//! / POINTS_PER_MAKE_RATE` per point, signed by which team has the ball,
//! with the constant measured so the simulated mean margin equals the
//! rating edge. Two realism
//! tweaks: in the final 24 possessions of a close game the rating edge is
//! dampened to a quarter (clutch tightening), and in the last 10
//! possessions with a small lead make rate transfers from the leader to
//! the trailer (the foul game). A `force_tie` knob rigs the last
//! regulation possession for deterministic overtime tests.

use rand::Rng;
use rand::SeedableRng;
use rand::rngs::StdRng;

use super::league::{HOME_COURT, Player, Team, players_of};
use super::winprob::{OVERTIME_SECONDS, PERIOD_SECONDS};

pub const REGULATION_POSSESSIONS: u32 = 200;
pub const POSSESSIONS_PER_QUARTER: u32 = 50;
pub const OVERTIME_POSSESSIONS: u32 = 20;

const P_TURNOVER: f64 = 0.13;
const P_FOUL_TRIP: f64 = 0.12;
const P_FREE_THROW: f64 = 0.78;
const P_AND_ONE: f64 = 0.06;
const BASE_THREE: f64 = 0.36;
const BASE_TWO: f64 = 0.53;
const MAKE_MIN: f64 = 0.05;
const MAKE_MAX: f64 = 0.95;
/// Points per 100 possessions moved by a uniform +1 make-rate shift.
/// Measured, not derived: the shift lands on twos and threes alike, the
/// and-one and the foul game ride on it, and the clutch dampening takes
/// some back, so the analytic 176 undershoots by about 2.4×. The
/// calibration test pins the simulated mean margin to the rating edge.
const POINTS_PER_MAKE_RATE: f64 = 420.0;
const CLUTCH_POSSESSIONS: u32 = 24;
const CLUTCH_MARGIN: i32 = 6;
const CLUTCH_DAMPING: f64 = 0.25;
const FOUL_GAME_POSSESSIONS: u32 = 10;
const FOUL_GAME_MAX_LEAD: i32 = 6;
const FOUL_GAME_TRANSFER: f64 = 0.04;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Tipoff,
    TwoPointer,
    ThreePointer,
    FreeThrow,
    Turnover,
    Rebound,
    Foul,
    Final,
}

impl Action {
    pub fn as_str(self) -> &'static str {
        match self {
            Action::Tipoff => "tipoff",
            Action::TwoPointer => "two_pointer",
            Action::ThreePointer => "three_pointer",
            Action::FreeThrow => "free_throw",
            Action::Turnover => "turnover",
            Action::Rebound => "rebound",
            Action::Foul => "foul",
            Action::Final => "final",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShotResult {
    Made,
    Missed,
    None,
}

impl ShotResult {
    pub fn as_str(self) -> &'static str {
        match self {
            ShotResult::Made => "made",
            ShotResult::Missed => "missed",
            ShotResult::None => "none",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    InPlay,
    Final,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::InPlay => "in_play",
            Phase::Final => "final",
        }
    }
}

/// One play. `team` is the offense for shots, turnovers and free throws
/// and the defense for fouls and rebounds; `player` is 0 on tipoff and
/// final.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GameEvent {
    /// Position in the game's event stream, from 0.
    pub seq: u32,
    pub action: Action,
    pub result: ShotResult,
    pub team: &'static str,
    pub player: u32,
    pub phase: Phase,
    pub home_team: &'static str,
    pub away_team: &'static str,
    /// 1–4 regulation, 5+ overtime.
    pub quarter: u8,
    /// Seconds left in the current period.
    pub clock_remaining: f64,
    pub home_score_before: u32,
    pub away_score_before: u32,
    pub home_score: u32,
    pub away_score: u32,
    /// Regulation possessions and overtime possessions played so far,
    /// including the one this event belongs to.
    pub possession: u32,
}

/// The engine for one game.
#[derive(Debug)]
pub struct GameEngine {
    rng: StdRng,
    home: &'static Team,
    away: &'static Team,
    home_players: Vec<&'static Player>,
    away_players: Vec<&'static Player>,
    /// Usage-weighted mean `make_adj` per roster, subtracted from every
    /// shooter's so the roster as a whole is average.
    home_adj_mean: f64,
    away_adj_mean: f64,
    home_score: u32,
    away_score: u32,
    /// Possessions completed.
    possession: u32,
    seq: u32,
    started: bool,
    finished: bool,
    force_tie: bool,
}

impl GameEngine {
    pub fn new(home: &'static Team, away: &'static Team, seed: u64) -> Self {
        assert_ne!(home.code, away.code, "a team cannot play itself");
        let home_players: Vec<&'static Player> = players_of(home.code).collect();
        let away_players: Vec<&'static Player> = players_of(away.code).collect();
        Self {
            rng: StdRng::seed_from_u64(seed),
            home,
            away,
            home_adj_mean: roster_adj_mean(&home_players),
            away_adj_mean: roster_adj_mean(&away_players),
            home_players,
            away_players,
            home_score: 0,
            away_score: 0,
            possession: 0,
            seq: 0,
            started: false,
            finished: false,
            force_tie: false,
        }
    }

    /// Rig the last regulation possession so the game goes to overtime.
    pub fn with_force_tie(mut self, force: bool) -> Self {
        self.force_tie = force;
        self
    }

    pub fn is_final(&self) -> bool {
        self.finished
    }

    pub fn score(&self) -> (u32, u32) {
        (self.home_score, self.away_score)
    }

    pub fn home(&self) -> &'static Team {
        self.home
    }

    pub fn away(&self) -> &'static Team {
        self.away
    }

    /// Possessions in the regulation game plus every overtime played or
    /// in progress.
    pub fn possession(&self) -> u32 {
        self.possession
    }

    /// Whether the game is past regulation.
    pub fn in_overtime(&self) -> bool {
        self.possession >= REGULATION_POSSESSIONS
    }

    /// The tipoff event; must be the first call.
    pub fn tipoff(&mut self) -> GameEvent {
        assert!(!self.started, "tipoff twice");
        self.started = true;
        self.event(Action::Tipoff, ShotResult::None, self.home.code, 0, 0, 0)
    }

    /// Play the whole game, tipoff to final.
    pub fn play_full(home: &'static Team, away: &'static Team, seed: u64) -> Vec<GameEvent> {
        let mut engine = GameEngine::new(home, away, seed);
        let mut out = vec![engine.tipoff()];
        while let Some(events) = engine.play_possession() {
            out.extend(events);
        }
        out
    }

    /// Play the next possession. Returns the final event alone once the
    /// game is decided, and `None` after that.
    pub fn play_possession(&mut self) -> Option<Vec<GameEvent>> {
        assert!(self.started, "play before tipoff");
        if self.finished {
            return None;
        }
        if self.at_period_boundary() && self.home_score != self.away_score {
            self.finished = true;
            return Some(vec![self.event(Action::Final, ShotResult::None, self.home.code, 0, 0, 0)]);
        }
        let home_offense = self.possession.is_multiple_of(2);
        let mut events = Vec::with_capacity(3);
        // Copies of the roster slices (five `&'static` pointers each) so
        // the draws below don't hold a borrow across `self.event`.
        let (offense_players, defense_players): (Vec<&'static Player>, Vec<&'static Player>) = if home_offense {
            (self.home_players.clone(), self.away_players.clone())
        } else {
            (self.away_players.clone(), self.home_players.clone())
        };
        let (offense_players, defense_players) = (&offense_players[..], &defense_players[..]);
        let shooter = draw_by_usage(&mut self.rng, offense_players);
        let shooter_adj = shooter.make_adj - if home_offense { self.home_adj_mean } else { self.away_adj_mean };
        let offense_code = if home_offense { self.home.code } else { self.away.code };
        let defense_code = if home_offense { self.away.code } else { self.home.code };
        let shift = self.make_rate_shift(home_offense);
        let roll: f64 = self.rng.random();

        let forced_tie = self.force_tie && self.possession == REGULATION_POSSESSIONS - 1;
        if forced_tie {
            // Test knob: the trailing side scores exactly its deficit in
            // one play so regulation ends level; a level score turns over.
            let (h, a) = (self.home_score, self.away_score);
            if h == a {
                events.push(self.event(Action::Turnover, ShotResult::None, offense_code, shooter.id, 0, 0));
            } else {
                let home_trails = h < a;
                let deficit = h.abs_diff(a);
                let (scorer_code, players) = if home_trails { (self.home.code, offense_players) } else { (self.away.code, defense_players) };
                let players = if home_trails == home_offense { offense_players } else { players };
                let scorer = draw_by_usage(&mut self.rng, players);
                let (hp, ap) = if home_trails { (deficit, 0) } else { (0, deficit) };
                events.push(self.event(Action::TwoPointer, ShotResult::Made, scorer_code, scorer.id, hp, ap));
            }
        } else if roll < P_TURNOVER {
            events.push(self.event(Action::Turnover, ShotResult::None, offense_code, shooter.id, 0, 0));
        } else if roll < P_TURNOVER + P_FOUL_TRIP {
            let fouler = draw_by_usage(&mut self.rng, defense_players);
            events.push(self.event(Action::Foul, ShotResult::None, defense_code, fouler.id, 0, 0));
            for _ in 0..2 {
                let made = self.rng.random::<f64>() < P_FREE_THROW;
                let (h, a) = points(home_offense, if made { 1 } else { 0 });
                let result = if made { ShotResult::Made } else { ShotResult::Missed };
                events.push(self.event(Action::FreeThrow, result, offense_code, shooter.id, h, a));
            }
        } else {
            let three = self.rng.random::<f64>() < shooter.three_rate;
            let base = if three { BASE_THREE } else { BASE_TWO };
            let make_rate = (base + shift + shooter_adj).clamp(MAKE_MIN, MAKE_MAX);
            let made = self.rng.random::<f64>() < make_rate;
            let action = if three { Action::ThreePointer } else { Action::TwoPointer };
            if made {
                let (h, a) = points(home_offense, if three { 3 } else { 2 });
                events.push(self.event(action, ShotResult::Made, offense_code, shooter.id, h, a));
                if !three && self.rng.random::<f64>() < P_AND_ONE {
                    let fouler = draw_by_usage(&mut self.rng, defense_players);
                    events.push(self.event(Action::Foul, ShotResult::None, defense_code, fouler.id, 0, 0));
                    let made_ft = self.rng.random::<f64>() < P_FREE_THROW;
                    let (h, a) = points(home_offense, if made_ft { 1 } else { 0 });
                    let result = if made_ft { ShotResult::Made } else { ShotResult::Missed };
                    events.push(self.event(Action::FreeThrow, result, offense_code, shooter.id, h, a));
                }
            } else {
                events.push(self.event(action, ShotResult::Missed, offense_code, shooter.id, 0, 0));
                let rebounder = draw_by_usage(&mut self.rng, defense_players);
                events.push(self.event(Action::Rebound, ShotResult::None, defense_code, rebounder.id, 0, 0));
            }
        }
        self.possession += 1;
        // Stamp the possession's closing clock on its events: the play's
        // last event carries the time the possession ended.
        let (quarter, clock) = self.clock_after(self.possession);
        for e in &mut events {
            e.quarter = quarter;
            e.clock_remaining = clock;
            e.possession = self.possession;
        }
        Some(events)
    }

    /// Whether the game is at the end of regulation or of an overtime
    /// period: the only moments it can end, and a tie there extends it
    /// by one more period.
    fn at_period_boundary(&self) -> bool {
        self.possession >= REGULATION_POSSESSIONS
            && (self.possession - REGULATION_POSSESSIONS).is_multiple_of(OVERTIME_POSSESSIONS)
    }

    /// The make-rate shift for the offense: the rating edge, dampened in
    /// the clutch, with the foul-game transfer late in a close game.
    fn make_rate_shift(&self, home_offense: bool) -> f64 {
        let net_advantage = self.home.net_rating + HOME_COURT - self.away.net_rating;
        let mut eps = net_advantage / POINTS_PER_MAKE_RATE;
        let margin = self.home_score as i32 - self.away_score as i32;
        let remaining = REGULATION_POSSESSIONS.saturating_sub(self.possession);
        let in_regulation = self.possession < REGULATION_POSSESSIONS;
        if in_regulation && remaining <= CLUTCH_POSSESSIONS && margin.abs() <= CLUTCH_MARGIN {
            eps *= CLUTCH_DAMPING;
        }
        let mut shift = if home_offense { eps } else { -eps };
        if in_regulation && remaining <= FOUL_GAME_POSSESSIONS && margin != 0 && margin.abs() <= FOUL_GAME_MAX_LEAD {
            let home_leads = margin > 0;
            shift += if home_offense == home_leads { -FOUL_GAME_TRANSFER } else { FOUL_GAME_TRANSFER };
        }
        shift
    }

    /// The (quarter, seconds left in it) after `completed` possessions.
    fn clock_after(&self, completed: u32) -> (u8, f64) {
        if completed <= REGULATION_POSSESSIONS {
            let idx = completed.saturating_sub(1);
            let quarter = (idx / POSSESSIONS_PER_QUARTER) as u8 + 1;
            let within = idx % POSSESSIONS_PER_QUARTER + 1;
            let per = PERIOD_SECONDS / POSSESSIONS_PER_QUARTER as f64;
            (quarter, round1(PERIOD_SECONDS - per * within as f64))
        } else {
            let past = completed - REGULATION_POSSESSIONS - 1;
            let period = 5 + (past / OVERTIME_POSSESSIONS) as u8;
            let within = past % OVERTIME_POSSESSIONS + 1;
            let per = OVERTIME_SECONDS / OVERTIME_POSSESSIONS as f64;
            (period, round1(OVERTIME_SECONDS - per * within as f64))
        }
    }

    fn event(&mut self, action: Action, result: ShotResult, team: &'static str, player: u32, home_pts: u32, away_pts: u32) -> GameEvent {
        let (hb, ab) = (self.home_score, self.away_score);
        self.home_score += home_pts;
        self.away_score += away_pts;
        let phase = if action == Action::Final { Phase::Final } else { Phase::InPlay };
        let (quarter, clock) = match action {
            Action::Tipoff => (1, PERIOD_SECONDS),
            Action::Final => self.clock_after(self.possession),
            _ => (0, 0.0), // stamped by play_possession
        };
        let e = GameEvent {
            seq: self.seq,
            action,
            result,
            team,
            player,
            phase,
            home_team: self.home.code,
            away_team: self.away.code,
            quarter,
            clock_remaining: clock,
            home_score_before: hb,
            away_score_before: ab,
            home_score: self.home_score,
            away_score: self.away_score,
            possession: self.possession,
        };
        self.seq += 1;
        e
    }
}

fn points(home_offense: bool, pts: u32) -> (u32, u32) {
    if home_offense { (pts, 0) } else { (0, pts) }
}

fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

/// The usage-weighted mean make-rate adjustment of a roster.
fn roster_adj_mean(players: &[&'static Player]) -> f64 {
    let usage: f64 = players.iter().map(|p| p.usage).sum();
    players.iter().map(|p| p.usage * p.make_adj).sum::<f64>() / usage.max(f64::MIN_POSITIVE)
}

fn draw_by_usage<R: Rng + ?Sized>(rng: &mut R, players: &[&'static Player]) -> &'static Player {
    let total: f64 = players.iter().map(|p| p.usage).sum();
    let mut pick = rng.random::<f64>() * total;
    for p in players {
        if pick < p.usage {
            return p;
        }
        pick -= p.usage;
    }
    players[players.len() - 1]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::nba::league::team;

    fn replay(events: &[GameEvent]) -> (u32, u32) {
        let mut h = 0;
        let mut a = 0;
        for e in events {
            assert_eq!((e.home_score_before, e.away_score_before), (h, a), "seq {}", e.seq);
            h = e.home_score;
            a = e.away_score;
        }
        (h, a)
    }

    #[test]
    fn events_are_the_score() {
        for seed in 0..20u64 {
            let events = GameEngine::play_full(team("BOS"), team("SAS"), seed);
            assert_eq!(events[0].action, Action::Tipoff);
            let last = events.last().unwrap();
            assert_eq!(last.action, Action::Final);
            assert_eq!(last.phase, Phase::Final);
            assert_ne!(last.home_score, last.away_score, "a final is never tied");
            assert_eq!(replay(&events), (last.home_score, last.away_score));
            assert!(events.windows(2).all(|w| w[0].seq + 1 == w[1].seq));
            assert!(events.iter().all(|e| e.home_team == "BOS" && e.away_team == "SAS"));
        }
    }

    #[test]
    fn deterministic_per_seed_and_realistic_totals() {
        let a = GameEngine::play_full(team("LAL"), team("NYK"), 7);
        let b = GameEngine::play_full(team("LAL"), team("NYK"), 7);
        assert_eq!(a, b);
        let c = GameEngine::play_full(team("LAL"), team("NYK"), 8);
        assert_ne!(a, c);
        let mut totals = Vec::new();
        for seed in 0..40u64 {
            let events = GameEngine::play_full(team("LAL"), team("NYK"), seed);
            let last = events.last().unwrap();
            totals.push(last.home_score + last.away_score);
        }
        let mean = totals.iter().sum::<u32>() as f64 / totals.len() as f64;
        assert!((180.0..260.0).contains(&mean), "mean total {mean}");
    }

    /// The engine is calibrated to the ratings and the model to the
    /// engine: over many games the mean margin is the rating edge, the
    /// spread is [`SIGMA_FINAL`], and the favourite wins as often as the
    /// model's pregame number says. Both directions of the widest matchup.
    #[test]
    fn engine_is_calibrated_to_the_ratings_and_the_model_to_the_engine() {
        use crate::sim::nba::league::HOME_COURT;
        use crate::sim::nba::winprob::{REGULATION_SECONDS, SIGMA_FINAL, expected_margin, win_probability};
        let n = 400u64;
        for (h, a) in [("BOS", "TOR"), ("TOR", "BOS")] {
            let mut margins = Vec::with_capacity(n as usize);
            for seed in 0..n {
                let last = *GameEngine::play_full(team(h), team(a), seed).last().unwrap();
                margins.push(f64::from(last.home_score) - f64::from(last.away_score));
            }
            let mean = margins.iter().sum::<f64>() / n as f64;
            let sd = (margins.iter().map(|m| (m - mean).powi(2)).sum::<f64>() / n as f64).sqrt();
            let wins = margins.iter().filter(|m| **m > 0.0).count() as f64 / n as f64;
            let edge = expected_margin(team(h).net_rating, team(a).net_rating, HOME_COURT);
            let model = win_probability(0.0, REGULATION_SECONDS, edge);
            assert!((mean - edge).abs() < 1.5, "{h} v {a}: mean margin {mean:.2} vs rating edge {edge:.2}");
            assert!((sd - SIGMA_FINAL).abs() < 1.5, "{h} v {a}: margin sd {sd:.1} vs model {SIGMA_FINAL}");
            assert!((wins - model).abs() < 0.06, "{h} v {a}: home wins {wins:.3} vs model {model:.3}");
        }
        // The best side at home over the worst is a favourite, not a lock.
        let bos: f64 = (0..n)
            .filter(|&seed| {
                let last = *GameEngine::play_full(team("BOS"), team("TOR"), seed).last().unwrap();
                last.home_score > last.away_score
            })
            .count() as f64
            / n as f64;
        assert!(bos > 0.53 && bos < 0.63, "{bos}");
    }

    #[test]
    fn rosters_are_average_as_a_whole() {
        for t in crate::sim::nba::league::TEAMS {
            let players: Vec<&'static Player> = players_of(t.code).collect();
            let mean = roster_adj_mean(&players);
            let centred: f64 = players.iter().map(|p| p.usage * (p.make_adj - mean)).sum();
            assert!(centred.abs() < 1e-12, "{}: {centred}", t.code);
        }
    }

    #[test]
    fn clock_runs_down_each_quarter_and_overtime_only_counts_its_own_period() {
        let events = GameEngine::play_full(team("PHI"), team("SAS"), 3);
        let first_q2 = events.iter().find(|e| e.quarter == 2).unwrap();
        assert!(first_q2.clock_remaining < PERIOD_SECONDS && first_q2.clock_remaining > 700.0);
        let last_q4 = events.iter().rfind(|e| e.quarter == 4 && e.action != Action::Final).unwrap();
        assert_eq!(last_q4.clock_remaining, 0.0);
        assert!(events.iter().all(|e| e.quarter >= 1));
    }

    #[test]
    fn force_tie_sends_the_game_to_overtime() {
        let mut engine = GameEngine::new(team("NYK"), team("TOR"), 11).with_force_tie(true);
        let mut events = vec![engine.tipoff()];
        while let Some(more) = engine.play_possession() {
            events.extend(more);
        }
        let end_of_regulation = events.iter().find(|e| e.possession == REGULATION_POSSESSIONS).unwrap();
        let reg_last = events.iter().rfind(|e| e.possession == REGULATION_POSSESSIONS).unwrap();
        assert_eq!(reg_last.home_score, reg_last.away_score, "tied after regulation");
        assert_eq!(end_of_regulation.quarter, 4);
        assert!(events.iter().any(|e| e.quarter == 5), "overtime was played");
        let last = events.last().unwrap();
        assert_eq!(last.action, Action::Final);
        assert_ne!(last.home_score, last.away_score);
        assert_eq!(replay(&events), (last.home_score, last.away_score));
        let ot_first = events.iter().find(|e| e.quarter == 5).unwrap();
        assert!(ot_first.clock_remaining < OVERTIME_SECONDS);
        // Overtime is a whole period: the game ends at a period boundary, never mid-period.
        let last_play = events.iter().rfind(|e| e.action != Action::Final).unwrap();
        assert!((last_play.possession - REGULATION_POSSESSIONS).is_multiple_of(OVERTIME_POSSESSIONS));
        assert_eq!(last_play.clock_remaining, 0.0);
    }

    #[test]
    fn possessions_alternate_and_players_belong_to_the_right_team() {
        let events = GameEngine::play_full(team("BOS"), team("LAL"), 5);
        for e in &events {
            if e.action == Action::Tipoff || e.action == Action::Final {
                assert_eq!(e.player, 0);
                continue;
            }
            let home_offense = (e.possession - 1).is_multiple_of(2);
            let offense = if home_offense { "BOS" } else { "LAL" };
            let defense = if home_offense { "LAL" } else { "BOS" };
            let expected = match e.action {
                Action::Foul | Action::Rebound => defense,
                _ => offense,
            };
            assert_eq!(e.team, expected, "seq {}", e.seq);
            assert!(players_of(e.team).any(|p| p.id == e.player), "seq {} player {}", e.seq, e.player);
        }
    }
}
