//! A seeded NBA season: six real teams, five named players each, one
//! simulated game per window of the `nba-h2h` series.
//!
//! * [`league`] — the teams, rosters, net ratings and home court.
//! * [`winprob`] — the one win-probability model shared by the world's own
//!   truth, the derived signal and the bots' fair value.
//! * [`engine`] — the pure, clock-free possession engine: a game is a fold
//!   of events, each carrying the score before and after, so replaying the
//!   events reconstructs exactly the final the round settles on.
//! * [`world`] — the paced world task: draws matchups, plays games against
//!   the series cadence on the wall clock, broadcasts events and exposes
//!   the current matchup and true home win probability.

//! * [`mark`] — the bots' fair value from the published frames only.
//! * [`driver`] — the series' round driver: opens rounds on the announced
//!   matchup and settles them from the published final.

pub mod driver;
pub mod engine;
pub mod league;
pub mod mark;
pub mod winprob;
pub mod world;

pub use driver::NbaDriver;
pub use engine::{Action, GameEngine, GameEvent, Phase, ShotResult};
pub use league::{HOME_COURT, Player, ROSTERS, TEAMS, Team, TeamCode, players_of, team};
pub use mark::NbaMark;
pub use world::{MatchupState, NbaWorld, Pacing, WindowSpec, WorldHandle, spawn_world};
