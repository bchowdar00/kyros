//! The paced season world.
//!
//! [`NbaWorld`] is the hidden truth: a seeded matchup schedule (two
//! distinct teams per game, never the pair that just played) and, per
//! game, a [`GameEngine`] seeded from the game's sequence number, so the
//! same seed always yields the same season. It knows nothing about
//! markets; it exposes exactly two things:
//!
//! * a broadcast of [`GameEvent`]s, every play as it happens, and
//! * a polled [`MatchupState`]: the current matchup, its game number and
//!   the world's own win probability for the home side (the model in
//!   [`super::winprob`] over the true expected margin and the live score).
//!
//! The moment a final is emitted the polled state flips to the *next*
//! matchup, so the sharp line quotes the coming game during the settle
//! and pregame of the window that will trade it. That choreography is
//! what lets settlement and fair value learn the matchup before tipoff.
//!
//! Pacing is on the wall clock against the round the series' driver
//! hands in through [`WorldHandle::play_window`]: sleep through the
//! pregame, tip off, then pace the regulation possessions with anchored
//! sleeps (target time = tip + span · k / 200, never sleeping if behind)
//! so the last one lands `lead` seconds before the close. Overtime is
//! paced fast so it fits in that tail; if a second overtime overruns the
//! close, the driver simply waits for the final and the round runs long.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use rand::Rng;
use rand::SeedableRng;
use rand::rngs::StdRng;
use tokio::sync::{broadcast, mpsc};

use super::engine::{Action, GameEngine, GameEvent, REGULATION_POSSESSIONS};
use super::league::{HOME_COURT, TEAMS, Team, TeamCode, team};
use super::winprob::{REGULATION_SECONDS, expected_margin, game_seconds_remaining, win_probability};
use crate::pricing::feed::unix_now_ms;

/// Capacity of the event broadcast. A game is ~600 events over minutes;
/// a subscriber that lags this far has bigger problems.
pub const EVENT_BROADCAST_CAP: usize = 2048;
/// Trading time between a round's open and tipoff on a normal round.
pub const PREGAME_SECS: u64 = 5;

/// How a game is fitted into a round.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pacing {
    /// Trading time after the round opens before tipoff.
    pub pregame_secs: u64,
    /// The regulation final lands this many seconds before the close.
    pub lead_secs: u64,
    /// Wall seconds per overtime possession.
    pub overtime_possession_secs: f64,
}

impl Pacing {
    /// The pacing for a round of `duration_secs`: five seconds of pregame
    /// (a sixtieth of the round on a very long one, so the tip is never
    /// on the open), and a tail of one tenth of the round (at least three
    /// seconds) for the lead. The next matchup is quoted during the
    /// previous round's tail, so a short pregame costs nothing.
    pub fn for_duration(duration_secs: u64) -> Pacing {
        let pregame_secs = (duration_secs / 60).clamp(PREGAME_SECS, 60);
        let lead_secs = (duration_secs / 10).clamp(3, 30);
        Pacing { pregame_secs, lead_secs, overtime_possession_secs: 0.4 }
    }
}

/// The world's polled surface.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MatchupState {
    pub home: TeamCode,
    pub away: TeamCode,
    /// Counts games from 1; the reference on a round carries it.
    pub game_seq: u64,
    /// The world's own P(home) right now: the pregame truth before tipoff,
    /// the live model on the true drift and score during the game.
    pub true_home_prob: f64,
}

/// A round the world should play a game inside.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowSpec {
    pub opens_at_ms: u64,
    pub closes_at_ms: u64,
}

/// The hidden truth. Synchronous; the task in [`spawn_world`] drives it.
#[derive(Debug)]
pub struct NbaWorld {
    rng: StdRng,
    seed: u64,
    game_seq: u64,
    current: (&'static Team, &'static Team),
    last_pair: Option<(TeamCode, TeamCode)>,
}

impl NbaWorld {
    /// A world on `seed` with its first matchup drawn.
    pub fn new(seed: u64) -> Self {
        let mut w = Self {
            rng: StdRng::seed_from_u64(seed),
            seed,
            game_seq: 0,
            current: (&TEAMS[0], &TEAMS[1]),
            last_pair: None,
        };
        w.advance();
        w
    }

    /// The matchup being played (or about to be).
    pub fn matchup(&self) -> (&'static Team, &'static Team) {
        self.current
    }

    pub fn game_seq(&self) -> u64 {
        self.game_seq
    }

    /// The true pregame P(home) for the current matchup.
    pub fn pregame_home_prob(&self) -> f64 {
        let (h, a) = self.current;
        win_probability(0.0, REGULATION_SECONDS, expected_margin(h.net_rating, a.net_rating, HOME_COURT))
    }

    /// The engine for the current game, seeded from the season seed and
    /// the game number.
    pub fn engine(&self) -> GameEngine {
        let (h, a) = self.current;
        GameEngine::new(h, a, self.seed ^ self.game_seq.wrapping_mul(0x9E37_79B9_7F4A_7C15))
    }

    /// Draw the next matchup: two distinct teams, home first, never the
    /// same unordered pair as the game that just finished.
    pub fn advance(&mut self) -> (&'static Team, &'static Team) {
        loop {
            let i = self.rng.random_range(0..TEAMS.len());
            let mut j = self.rng.random_range(0..TEAMS.len() - 1);
            if j >= i {
                j += 1;
            }
            let (h, a) = (&TEAMS[i], &TEAMS[j]);
            let same_pair = self
                .last_pair
                .is_some_and(|(ph, pa)| (ph == h.code && pa == a.code) || (ph == a.code && pa == h.code));
            if same_pair {
                continue;
            }
            self.last_pair = Some((h.code, a.code));
            self.current = (h, a);
            self.game_seq += 1;
            return self.current;
        }
    }

    /// The polled state for the current matchup before tipoff.
    pub fn pregame_state(&self) -> MatchupState {
        let (h, a) = self.current;
        MatchupState { home: h.code, away: a.code, game_seq: self.game_seq, true_home_prob: self.pregame_home_prob() }
    }
}

/// Shared handle to the running world.
#[derive(Clone)]
pub struct WorldHandle {
    events: broadcast::Sender<GameEvent>,
    state: Arc<RwLock<MatchupState>>,
    windows: mpsc::UnboundedSender<WindowSpec>,
}

impl WorldHandle {
    pub fn subscribe(&self) -> broadcast::Receiver<GameEvent> {
        self.events.subscribe()
    }

    pub fn matchup(&self) -> MatchupState {
        *self.state.read().expect("world state poisoned")
    }

    /// Play the current matchup's game inside this round. Called by the
    /// series' driver once per round after it opens.
    pub fn play_window(&self, window: WindowSpec) {
        let _ = self.windows.send(window);
    }
}

/// Spawn the world task. Runs for the life of the process.
pub fn spawn_world(seed: u64, pacing: Pacing) -> WorldHandle {
    let world = NbaWorld::new(seed);
    let (events, _) = broadcast::channel(EVENT_BROADCAST_CAP);
    let state = Arc::new(RwLock::new(world.pregame_state()));
    let (windows_tx, windows_rx) = mpsc::unbounded_channel();
    let handle = WorldHandle { events: events.clone(), state: state.clone(), windows: windows_tx };
    tokio::spawn(run_world(world, pacing, events, state, windows_rx));
    handle
}

async fn run_world(
    mut world: NbaWorld,
    pacing: Pacing,
    events: broadcast::Sender<GameEvent>,
    state: Arc<RwLock<MatchupState>>,
    mut windows: mpsc::UnboundedReceiver<WindowSpec>,
) {
    while let Some(window) = windows.recv().await {
        play_game(&mut world, pacing, window, &events, &state).await;
    }
}

/// Play one game for the current matchup inside `window`, then flip the
/// world to the next matchup.
async fn play_game(
    world: &mut NbaWorld,
    pacing: Pacing,
    window: WindowSpec,
    events: &broadcast::Sender<GameEvent>,
    state: &Arc<RwLock<MatchupState>>,
) {
    let (h, a) = world.matchup();
    let drift = expected_margin(h.net_rating, a.net_rating, HOME_COURT);
    let game_seq = world.game_seq();
    let tip_ms = window.opens_at_ms + pacing.pregame_secs * 1000;
    let end_ms = window.closes_at_ms.saturating_sub(pacing.lead_secs * 1000);
    let span_ms = end_ms.saturating_sub(tip_ms).max(1_000);

    sleep_until_ms(tip_ms).await;
    let mut engine = world.engine();
    let publish = |e: GameEvent| {
        let _ = events.send(e);
    };
    publish(engine.tipoff());
    tracing::info!(home = h.code, away = a.code, game_seq, "nba world: tipoff");

    loop {
        // Anchor the next possession to its share of the span; overtime
        // possessions run at a fixed fast cadence past the boundary.
        let k = engine.possession();
        let target_ms = if k < REGULATION_POSSESSIONS {
            tip_ms + span_ms * u64::from(k + 1) / u64::from(REGULATION_POSSESSIONS)
        } else {
            unix_now_ms() + (pacing.overtime_possession_secs * 1000.0) as u64
        };
        sleep_until_ms(target_ms).await;
        let Some(plays) = engine.play_possession() else { break };
        let mut is_final = false;
        for e in plays {
            is_final |= e.action == Action::Final;
            let margin = e.home_score as f64 - e.away_score as f64;
            let secs = game_seconds_remaining(e.quarter, e.clock_remaining);
            let live = win_probability(margin, secs, drift);
            {
                let mut s = state.write().expect("world state poisoned");
                s.true_home_prob = live;
            }
            publish(e);
        }
        if is_final {
            break;
        }
    }
    let (hs, as_) = engine.score();
    tracing::info!(home = h.code, away = a.code, home_score = hs, away_score = as_, game_seq, "nba world: final");
    // Flip the polled surface to the next game at once.
    world.advance();
    *state.write().expect("world state poisoned") = world.pregame_state();
}

async fn sleep_until_ms(at_ms: u64) {
    let now = unix_now_ms();
    if at_ms > now {
        tokio::time::sleep(Duration::from_millis(at_ms - now)).await;
    }
}

/// The team behind a code carried on a signal.
pub fn team_by_code(code: &str) -> &'static Team {
    team(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matchups_are_distinct_teams_and_never_repeat_the_last_pair() {
        let mut w = NbaWorld::new(42);
        let mut prev = w.matchup();
        assert_ne!(prev.0.code, prev.1.code);
        for _ in 0..200 {
            let next = w.advance();
            assert_ne!(next.0.code, next.1.code);
            let same = (next.0.code == prev.0.code && next.1.code == prev.1.code)
                || (next.0.code == prev.1.code && next.1.code == prev.0.code);
            assert!(!same, "{:?} repeated", (next.0.code, next.1.code));
            prev = next;
        }
        assert_eq!(w.game_seq(), 201);
    }

    #[test]
    fn schedule_and_games_are_seeded() {
        let mut a = NbaWorld::new(7);
        let mut b = NbaWorld::new(7);
        for _ in 0..5 {
            assert_eq!(a.matchup().0.code, b.matchup().0.code);
            assert_eq!(a.matchup().1.code, b.matchup().1.code);
            let ea = GameEngine::play_full(a.matchup().0, a.matchup().1, 0);
            let mut ga = a.engine();
            let mut gb = b.engine();
            let mut va = vec![ga.tipoff()];
            let mut vb = vec![gb.tipoff()];
            while let Some(p) = ga.play_possession() {
                va.extend(p);
            }
            while let Some(p) = gb.play_possession() {
                vb.extend(p);
            }
            assert_eq!(va, vb);
            assert!(!ea.is_empty());
            a.advance();
            b.advance();
        }
        let mut c = NbaWorld::new(8);
        let mut differs = false;
        for _ in 0..10 {
            if c.matchup().0.code != a.matchup().0.code {
                differs = true;
            }
            c.advance();
            a.advance();
        }
        assert!(differs, "different seeds give different schedules");
    }

    #[test]
    fn pregame_prob_favours_the_better_team_at_home() {
        let mut w = NbaWorld::new(1);
        loop {
            let (h, a) = w.matchup();
            if h.code == "BOS" && a.code == "TOR" {
                let p = w.pregame_home_prob();
                assert!(p > 0.53 && p < 0.56, "the widest line in a tight league: {p}");
                break;
            }
            w.advance();
        }
    }

    #[test]
    fn pacing_scales_with_the_round() {
        let p = Pacing::for_duration(300);
        assert_eq!(p.pregame_secs, PREGAME_SECS);
        assert_eq!(p.lead_secs, 30);
        let short = Pacing::for_duration(20);
        assert_eq!(short.pregame_secs, PREGAME_SECS);
        assert_eq!(short.lead_secs, 3);
        assert_eq!(Pacing::for_duration(7_200).pregame_secs, 60);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn world_plays_a_game_inside_a_window_and_flips_the_matchup() {
        let pacing = Pacing { pregame_secs: 0, lead_secs: 0, overtime_possession_secs: 0.01 };
        let handle = spawn_world(3, pacing);
        let before = handle.matchup();
        let mut rx = handle.subscribe();
        let now = unix_now_ms();
        handle.play_window(WindowSpec { opens_at_ms: now, closes_at_ms: now + 1_500 });
        let mut saw_tip = false;
        let last = loop {
            match tokio::time::timeout(Duration::from_secs(10), rx.recv()).await {
                Ok(Ok(e)) => {
                    saw_tip |= e.action == Action::Tipoff;
                    assert_eq!(e.home_team, before.home);
                    if e.action == Action::Final {
                        break e;
                    }
                }
                other => panic!("event stream ended: {other:?}"),
            }
        };
        assert!(saw_tip);
        assert_ne!(last.home_score, last.away_score);
        tokio::time::sleep(Duration::from_millis(50)).await;
        let after = handle.matchup();
        assert_eq!(after.game_seq, before.game_seq + 1);
        assert!(after.home != before.home || after.away != before.away);
        assert!(after.true_home_prob > 0.0 && after.true_home_prob < 1.0);
    }
}
