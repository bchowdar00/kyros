//! Server-side trading participants.
//!
//! Two families, both clients of the engine via the same code path as
//! human users:
//!
//! * [`market_maker`] — single bot that continuously quotes a 1-cent
//!   spread around the computed mark.
//! * [`traders`] — heterogeneous population of hundreds of bots split
//!   across passive/taker/contrarian/noise archetypes that supply
//!   directional and crossing flow.

pub mod market_maker;
pub mod traders;
