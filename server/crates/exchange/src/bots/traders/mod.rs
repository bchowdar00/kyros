//! Heterogeneous trader-bot population.
//!
//! Hundreds of bot accounts share the same engine as human users and the
//! market-maker. Each bot reads the live BTC feed, forms a directional
//! view from how BTC has moved vs. the strike, and either rests limit
//! orders scattered across the book or sends marketable orders that move
//! price.
//!
//! Personalities are split across six archetypes (passive near / mid /
//! deep, momentum taker, contrarian, noise). The mix is tilted toward
//! crossing flow so the visible price actually moves on a BTC drift
//! instead of just FV-drifting behind a thick passive wall.
//!
//! The module is structured into four pieces:
//!
//! * [`archetypes`] — the bot data model: enum, per-bot parameters,
//!   per-task state.
//! * [`identity`] — name/colour/avatar/bankroll/address generation.
//! * [`population`] — the orchestration that builds the population.
//! * [`decision`] — the synchronous per-tick logic (`decide_and_act`
//!   and the per-archetype handlers), kept testable without tokio.
//! * [`spawn`] — the async glue that drives one tokio task per bot.

pub mod archetypes;
pub mod decision;
pub mod identity;
pub mod population;
pub mod spawn;

pub use archetypes::{Archetype, BotParams, BotState};
pub use identity::{load_names, parse_names, seeded_rng, shuffle_names};
pub use population::build_population;
pub use spawn::spawn_bot;
