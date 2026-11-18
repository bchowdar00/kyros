//! Async glue: one tokio task per bot, calling into [`decide_and_act`].
//!
//! Holds the engine mutex only briefly each iteration. Implements the
//! end-of-market cadence override so even slow archetypes get multiple
//! turns in the closing window.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use rand::Rng;
use rand::SeedableRng;
use rand::rngs::StdRng;

use crate::engine::{Exchange, MarketEngine};
use crate::pricing::mark::SharedMark;
use crate::types::SeriesId;

use super::archetypes::BotState;
use super::decision::decide_and_act;

/// In the last `FINAL_CADENCE_OVERRIDE_SECS` seconds of a market, all
/// bots — even slow archetypes like PassiveDeep — are clamped to a
/// sub-second sleep so they get multiple turns in the closing stretch.
/// Without this, a PassiveDeep bot whose normal cadence is 30–90s would
/// literally not act in the final push.
const FINAL_CADENCE_OVERRIDE_SECS: f64 = 5.0;
const FINAL_CADENCE_MIN_MS: u64 = 200;
const FINAL_CADENCE_MAX_MS: u64 = 600;

/// Peek the engine for `secs_remaining` on the current market. Used to
/// shorten the sleep cadence in the closing window without holding the
/// engine mutex through `decide_and_act`.
fn current_secs_remaining(exchange: &Mutex<Exchange>, series: SeriesId) -> Option<f64> {
    let guard = exchange.lock().expect("exchange mutex poisoned");
    let market = guard.engine(series).current_market()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Some((market.closes_at as i64 - now as i64).max(0) as f64)
}

/// Spawn one tokio task per bot. Each task sleeps a jittered interval
/// drawn from the bot's archetype band, briefly grabs the engine mutex,
/// invokes [`decide_and_act`] against the series' mark source, and calls
/// `on_book_change` if the book moved.
///
/// `task_seed` seeds the bot's persistent RNG so behavior is reproducible
/// for a given (population seed, bot index) pair.
pub fn spawn_bot<F>(
    exchange: Arc<Mutex<Exchange>>,
    mark: SharedMark,
    mut bot: BotState,
    task_seed: u64,
    on_book_change: F,
) -> tokio::task::JoinHandle<()>
where
    F: Fn(&MarketEngine) + Send + 'static,
{
    let series = bot.series;
    tokio::spawn(async move {
        let mut rng = StdRng::seed_from_u64(task_seed);

        // Stagger the first action so the population doesn't all swing at
        // once — bots trickle into the market over ~5s after the task spawns.
        let initial_delay_ms = rng.random_range(0..5_000);
        tokio::time::sleep(Duration::from_millis(initial_delay_ms)).await;

        loop {
            // Cadence override: when we're inside the closing window,
            // ignore archetype reprice bands and tick on a sub-second
            // loop so every bot gets one last swing.
            let wait = match current_secs_remaining(&exchange, series) {
                Some(secs) if secs > 0.0 && secs <= FINAL_CADENCE_OVERRIDE_SECS => {
                    rng.random_range(FINAL_CADENCE_MIN_MS..=FINAL_CADENCE_MAX_MS)
                }
                _ => rng.random_range(
                    bot.params.reprice_interval_min_ms..=bot.params.reprice_interval_max_ms,
                ),
            };
            tokio::time::sleep(Duration::from_millis(wait)).await;

            let changed = {
                let mut x = exchange.lock().expect("exchange mutex poisoned");
                let mut ctx = x.ctx(series);
                decide_and_act(&mut ctx, &mut bot, mark.as_ref(), &mut rng)
            };
            if changed {
                let x = exchange.lock().expect("exchange mutex poisoned");
                on_book_change(x.engine(series));
            }
        }
    })
}
