//! Simulated worlds: hidden, seeded truths that feeds observe.
//!
//! A world knows nothing about markets or signals. It emits events and
//! exposes a small polled surface; publishers in `automation` turn those
//! into registry signals, and a `scheduler::RoundDriver` turns its finals
//! into round outcomes. Fair value and settlement read only what was
//! published, never the world, so a strategy could in principle predict
//! settlement from public information alone.

pub mod nba;
