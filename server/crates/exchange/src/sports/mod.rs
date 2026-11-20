//! Sports market models that run on real game data.
//!
//! Holds the shared win-probability model, ready for a real signal
//! producer to feed it live score, clock and opening line.

pub mod winprob;
pub mod espn;
pub mod nba_live;
pub mod polymarket;
pub mod kalshi;
