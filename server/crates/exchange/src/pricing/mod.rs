//! BTC pricing layer: live price feed + digital-call fair-value math.
//!
//! Both the market-maker bot and the trader-bot population read from
//! [`PriceFeed`] and price quotes against [`compute_fair_value_tenths`],
//! so they share one source of truth for "what's BTC doing" and "what's
//! that worth as a YES probability."

pub mod fair_value;
pub mod feed;
pub mod mark;
pub mod pyth;
