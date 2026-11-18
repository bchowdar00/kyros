//! In-process market-maker bot.
//!
//! Continuously quotes a multi-level ladder on both sides of the canonical
//! YES book so users and other bots always have deep liquidity to trade
//! against. Runs as a single tokio task that holds the engine mutex
//! briefly each iteration; it never goes through the HTTP layer.
//!
//! Strategy, each iteration (every [`LOOP_INTERVAL`]):
//!   1. Compute fair value (mark) from the series' mark source (for BTC
//!      the digital-call model on spot vs strike), falling back to last
//!      trade → mid → 50¢.
//!   2. Skew the mark against inventory: long YES shifts quotes down (to
//!      invite buyers and discourage more sellers), short YES shifts them
//!      up. Skew is linear in net position, capped at [`MAX_SKEW_TENTHS`].
//!   3. Bid at the largest valid tick ≤ skewed mark, ask at the next tick
//!      above — the tightest spread the grid allows (1¢ in the coarse
//!      middle, 0.1¢ in the fine tails). Deeper levels step one tick
//!      further out per level with sizes from [`LADDER_QTY`].
//!   4. For every level: if there's no resting order, it sits at the wrong
//!      price, or it has been eaten below [`REFRESH_BELOW_PCT`] of target,
//!      cancel and replace. Small partial fills are left alone so the bot
//!      keeps queue priority instead of churning.
//!   5. Broadcast a fresh book snapshot if anything changed.
//!
//! One maker runs per market series, each on its own account (address
//! `0xbb01<series>…`, username `sorcery` / `sorcery-<suffix>`). The
//! account is funded with [`DEFAULT_BANKROLL_USD`] (override with
//! `MM_BANKROLL_USD`, or per series with `MM_BANKROLL_USD_<SLUG>` where
//! the slug is upper-cased with `-` → `_`) and, like every bot, reset to
//! that bankroll after each of its series' rounds settles.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::engine::{Exchange, MarketEngine, SeriesCtx, TradeSource};
use crate::pricing::mark::{MarkSource, SharedMark};
use crate::ticks::{NUM_TICKS, PriceTick, TICKS};
use crate::types::{Address, DEFAULT_SERIES, OrderId, Qty, SeriesId, Side, Tenths};

/// Fixed per-series bot address — recognisable when it shows up in
/// trade feeds: `0xbb 0x01 <series byte> 0…`.
pub const fn mm_address(series: SeriesId) -> Address {
    Address([
        0xbb, 0x01, series.0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    ])
}

/// `sorcery` for the first series, `sorcery-<slug suffix>` for the rest
/// (e.g. `sorcery-15m`).
pub fn mm_username(series: SeriesId) -> String {
    if series == DEFAULT_SERIES {
        return BOT_USERNAME.to_string();
    }
    let suffix = series.slug().rsplit('-').next().unwrap_or(series.slug());
    format!("{BOT_USERNAME}-{suffix}")
}

const BOT_USERNAME: &str = "sorcery";
const BOT_COLOR: &str = "#22b8cf";
const BOT_EMOJI: &str = "🤖";

/// Contracts quoted per level, touch first. 2,000 at the touch is ≈ $1,000
/// of notional at mid-book, so a single user fill can't walk the price.
const LADDER_QTY: [u32; 5] = [2_000, 3_000, 4_000, 6_000, 8_000];
const LADDER_LEVELS: usize = LADDER_QTY.len();
/// Replace a level once fills have taken it below this share of target.
const REFRESH_BELOW_PCT: u32 = 50;
/// Max quote shift from inventory, in tenths of a cent (3¢).
const MAX_SKEW_TENTHS: i64 = 30;
/// Bankroll the bot is seeded with and reset to every market. The full
/// ladder locks at most Σ LADDER_QTY × $1 per side, so this is ample.
const DEFAULT_BANKROLL_USD: i64 = 5_000_000;
const LOOP_INTERVAL: Duration = Duration::from_millis(100);

fn bankroll(series: SeriesId) -> Tenths {
    let per_series = format!(
        "MM_BANKROLL_USD_{}",
        series.slug().to_ascii_uppercase().replace('-', "_")
    );
    let usd = std::env::var(per_series)
        .ok()
        .or_else(|| std::env::var("MM_BANKROLL_USD").ok())
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_BANKROLL_USD);
    Tenths(usd * 1_000)
}

#[derive(Debug)]
struct BotState {
    address: Address,
    bids: [Option<OrderId>; LADDER_LEVELS],
    asks: [Option<OrderId>; LADDER_LEVELS],
}

impl BotState {
    fn new(address: Address) -> Self {
        Self {
            address,
            bids: [None; LADDER_LEVELS],
            asks: [None; LADDER_LEVELS],
        }
    }
}

/// Total contracts across one side of the ladder — the inventory at which
/// skew saturates.
const fn ladder_total() -> i64 {
    let mut i = 0;
    let mut sum = 0i64;
    while i < LADDER_LEVELS {
        sum += LADDER_QTY[i] as i64;
        i += 1;
    }
    sum
}

/// Inventory skew in tenths: long YES → negative (quote lower), short YES
/// → positive. Linear in position, saturating at ±MAX_SKEW_TENTHS.
fn inventory_skew_tenths(position: i64) -> i64 {
    let max_inv = ladder_total();
    (-position * MAX_SKEW_TENTHS / max_inv).clamp(-MAX_SKEW_TENTHS, MAX_SKEW_TENTHS)
}

/// Index of the touch bid/ask pair for a mark (tenths of a cent). The bid
/// is the largest valid tick ≤ mark and the ask is the very next valid
/// tick — so the spread is whatever the local tick spacing is:
///   * 1¢ in the coarse middle of the grid (4¢–96¢),
///   * 0.1¢ in the fine tails (≤3¢ and ≥97¢),
///   * 1¢ at the boundaries between fine and coarse.
///
/// Returns `None` only at the very top of the grid (mark = 999) where no
/// ask tick exists above the bid.
fn touch_indices(mark_tenths: u16) -> Option<(usize, usize)> {
    let bid_idx = match TICKS.binary_search(&mark_tenths) {
        Ok(i) => i,
        Err(0) => 0,
        Err(i) => i - 1,
    };
    let ask_idx = bid_idx + 1;
    if ask_idx >= NUM_TICKS {
        return None;
    }
    Some((bid_idx, ask_idx))
}

/// Touch bid/ask pair for a mark. See [`touch_indices`].
#[cfg(test)]
fn quote_pair(mark_tenths: u16) -> Option<(PriceTick, PriceTick)> {
    let (b, a) = touch_indices(mark_tenths)?;
    Some((PriceTick::from_index(b)?, PriceTick::from_index(a)?))
}

/// Fair-value estimate (tenths of a cent). Primary signal: the series'
/// mark source over the current market with the remaining time (for BTC
/// the digital call on spot versus strike). Falls through to the
/// latest-trade → mid → 50¢ chain when there's no open market or the
/// source has nothing to price on (no tick yet, zero time left).
fn compute_mark(engine: &MarketEngine, mark: &dyn MarkSource) -> u16 {
    if let Some(market) = engine.current_market() {
        let now = unix_now_secs();
        let secs_remaining = (market.closes_at as i64 - now as i64).max(0) as f64;
        if let Some(inputs) = mark.inputs(market, secs_remaining) {
            return inputs.fv_tenths;
        }
    }

    if let Some(fill) = engine.recent_trades(1).next() {
        return fill.price.tenths();
    }
    let book = engine.book();
    match (book.best_bid(), book.best_ask()) {
        (Some(bid), Some(ask)) => (bid.tenths() + ask.tenths()) / 2,
        _ => 500,
    }
}

fn unix_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Make the bot's resting order on `side` match `price` at `qty`.
/// Returns `true` iff a placement or cancellation happened.
fn ensure_quote(
    engine: &mut SeriesCtx<'_>,
    bot_addr: Address,
    order_id_slot: &mut Option<OrderId>,
    side: Side,
    price: PriceTick,
    qty: u32,
) -> bool {
    if let Some(id) = *order_id_slot {
        match engine.book().get_order(id) {
            Some(o) if o.price == price && o.qty_open.0 * 100 >= qty * REFRESH_BELOW_PCT => {
                // Right price, still mostly intact — keep queue priority.
                return false;
            }
            Some(_) => {
                // Wrong price or mostly eaten — replace.
                let _ = engine.cancel(bot_addr, id);
            }
            None => {
                // Order is gone (fully filled, or cancelled by market reset).
            }
        }
    }

    match engine.place_limit(bot_addr, side, price, Qty(qty), TradeSource::Bot) {
        Ok(outcome) => {
            // Only track ids that actually rested. A placement that fully
            // crosses returns an order id with nothing on the book — there's
            // no point chasing it next iteration.
            *order_id_slot = if engine.book().get_order(outcome.order_id).is_some() {
                Some(outcome.order_id)
            } else {
                None
            };
            true
        }
        Err(_) => {
            *order_id_slot = None;
            false
        }
    }
}

/// Cancel a level we can no longer quote (ran off the grid).
fn clear_quote(
    engine: &mut SeriesCtx<'_>,
    bot_addr: Address,
    order_id_slot: &mut Option<OrderId>,
) -> bool {
    if let Some(id) = order_id_slot.take() {
        return engine.cancel(bot_addr, id).is_ok();
    }
    false
}

fn requote(
    engine: &mut SeriesCtx<'_>,
    mark_source: &dyn MarkSource,
    state: &mut BotState,
    on_change: &impl Fn(&MarketEngine),
) {
    let mark = compute_mark(engine.engine(), mark_source) as i64;
    let bot_addr = state.address;
    let position = engine.position(&bot_addr).position.0;
    // Cap at 998 so there is always an ask tick above the bid; at 999 the
    // bot would otherwise stop requoting and leave stale levels behind.
    let skewed = (mark + inventory_skew_tenths(position)).clamp(0, 998) as u16;
    let Some((bid_idx, ask_idx)) = touch_indices(skewed) else {
        return;
    };

    let mut changed = false;
    for (level, &qty) in LADDER_QTY.iter().enumerate() {
        changed |= match bid_idx
            .checked_sub(level)
            .and_then(PriceTick::from_index)
        {
            Some(tick) => ensure_quote(engine, bot_addr, &mut state.bids[level], Side::Bid, tick, qty),
            None => clear_quote(engine, bot_addr, &mut state.bids[level]),
        };
        changed |= match PriceTick::from_index(ask_idx + level) {
            Some(tick) => ensure_quote(engine, bot_addr, &mut state.asks[level], Side::Ask, tick, qty),
            None => clear_quote(engine, bot_addr, &mut state.asks[level]),
        };
    }

    if changed {
        on_change(engine.engine());
    }
}

pub fn spawn<F>(
    exchange: Arc<Mutex<Exchange>>,
    series: SeriesId,
    mark: SharedMark,
    on_book_change: F,
) -> tokio::task::JoinHandle<()>
where
    F: Fn(&MarketEngine) + Send + 'static,
{
    tokio::spawn(async move {
        // Provision the bot account with its bankroll, username and a
        // distinctive avatar so it's easy to pick out in any trade-history
        // UI. `seed_bot_account_in` also records the bankroll as the
        // balance the engine resets it to after every one of its rounds.
        let address = mm_address(series);
        let bankroll = bankroll(series);
        {
            let mut x = exchange.lock().expect("exchange mutex poisoned");
            x.seed_bot_account_in(
                address,
                bankroll,
                mm_username(series),
                Some(BOT_COLOR.to_string()),
                Some(BOT_EMOJI.to_string()),
                series,
            );
        }
        tracing::info!(
            %series,
            address = %address,
            bankroll_usd = bankroll.0 / 1_000,
            ladder = ?LADDER_QTY,
            max_skew_tenths = MAX_SKEW_TENTHS,
            interval_ms = LOOP_INTERVAL.as_millis() as u64,
            "market-maker bot started",
        );

        let mut state = BotState::new(address);
        let mut ticker = tokio::time::interval(LOOP_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            ticker.tick().await;
            let mut x = exchange.lock().expect("exchange mutex poisoned");
            let mut ctx = x.ctx(series);
            requote(&mut ctx, mark.as_ref(), &mut state, &on_book_change);
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mm_addresses_differ_per_series() {
        let a0 = mm_address(SeriesId(0));
        let a1 = mm_address(SeriesId(1));
        assert_ne!(a0, a1);
        assert_eq!((a0.0[0], a0.0[1], a0.0[2]), (0xbb, 0x01, 0));
        assert_eq!((a1.0[0], a1.0[1], a1.0[2]), (0xbb, 0x01, 1));
        assert_eq!(mm_username(SeriesId(0)), "sorcery");
        assert_eq!(mm_username(SeriesId(1)), "sorcery-15m");
    }

    fn pair(mark: u16) -> (u16, u16) {
        let (b, a) = quote_pair(mark).expect("expected a quote pair");
        (b.tenths(), a.tenths())
    }

    #[test]
    fn coarse_middle_exact_tick() {
        assert_eq!(pair(500), (500, 510));
    }

    #[test]
    fn coarse_middle_between_ticks() {
        // 50.4¢ is not on the grid; bid snaps down to 50¢, ask up to 51¢.
        assert_eq!(pair(504), (500, 510));
    }

    #[test]
    fn fine_bottom_exact_tick() {
        // 0.5¢ → bid 0.5¢, ask 0.6¢, spread 0.1¢.
        assert_eq!(pair(5), (5, 6));
    }

    #[test]
    fn fine_top_exact_tick() {
        assert_eq!(pair(980), (980, 981));
    }

    #[test]
    fn boundary_fine_bottom_to_coarse() {
        // 3.0¢ is the top of the fine bottom; next valid tick is 4.0¢.
        assert_eq!(pair(30), (30, 40));
    }

    #[test]
    fn boundary_coarse_to_fine_top() {
        // 96¢ is the top of the coarse band; next valid tick is 97.0¢.
        assert_eq!(pair(960), (960, 970));
    }

    #[test]
    fn below_lowest_tick_clamps_to_lowest() {
        // mark = 0 is below the lowest valid tick (1). Bid sits at 1, ask 2.
        assert_eq!(pair(0), (1, 2));
    }

    #[test]
    fn at_top_tick_returns_none() {
        assert!(quote_pair(999).is_none());
    }

    #[test]
    fn gap_above_fine_bottom() {
        // 3.5¢ sits in the gap (3.0¢ → 4.0¢ in the grid).
        assert_eq!(pair(35), (30, 40));
    }

    #[test]
    fn gap_below_fine_top() {
        // 96.5¢ sits in the gap (96¢ → 97.0¢ in the grid).
        assert_eq!(pair(965), (960, 970));
    }

    #[test]
    fn spread_in_coarse_middle_is_always_one_cent() {
        for mark in (40..=960).step_by(7) {
            let (b, a) = pair(mark);
            // Difference between adjacent coarse ticks is 10 tenths = 1¢
            // unless we straddle a boundary, in which case it's still ≤ 10.
            assert!(a - b <= 10, "spread > 1¢ at mark {}: {}/{}", mark, b, a);
        }
    }

    #[test]
    fn skew_is_zero_when_flat_and_saturates_at_full_ladder() {
        assert_eq!(inventory_skew_tenths(0), 0);
        assert!(inventory_skew_tenths(ladder_total() / 2) < 0, "long YES quotes lower");
        assert!(inventory_skew_tenths(-ladder_total() / 2) > 0, "short YES quotes higher");
        assert_eq!(inventory_skew_tenths(ladder_total() * 10), -MAX_SKEW_TENTHS);
        assert_eq!(inventory_skew_tenths(-ladder_total() * 10), MAX_SKEW_TENTHS);
    }

    #[test]
    fn spread_in_fine_tails_is_at_most_one_cent() {
        for mark in [1, 5, 15, 29, 970, 980, 990, 998] {
            let (b, a) = pair(mark);
            assert!(a - b <= 10, "spread > 1¢ at mark {}: {}/{}", mark, b, a);
        }
    }

}
