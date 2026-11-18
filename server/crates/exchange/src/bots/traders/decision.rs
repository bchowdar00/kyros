//! Synchronous per-tick decision logic.
//!
//! [`decide_and_act`] is the single entry point: given the current
//! engine state, the bot's mutable [`BotState`], the latest BTC price,
//! and a recent-velocity reading, it dispatches to one of three
//! archetype handlers ([`act_passive`], [`act_taker`], [`act_noise`])
//! and reports whether the book was mutated.
//!
//! Keeping this layer synchronous and engine-only means the entire
//! decision tree is exercised in unit tests without spinning up tokio
//! or a real price feed.

use rand::Rng;

use crate::engine::{TradeSource, SeriesCtx};
use crate::pricing::mark::MarkSource;
use crate::ticks::{NUM_TICKS, PriceTick, TICKS};
use crate::types::{Address, OrderId, Qty, Side};

use super::archetypes::{Archetype, BotParams, BotState};

// ──────────────────────────── Engine micro-helpers ────────────────────────

/// Snap a raw tenths value to the closest valid `PriceTick` on the ladder.
/// `tenths` outside [1, 999] is clamped first.
fn nearest_tick(tenths: i32) -> Option<PriceTick> {
    let clamped = tenths.clamp(1, 999) as u16;
    // The ladder is sorted; binary_search returns either the exact index or
    // the insertion point, so the nearest valid tick is whichever neighbor
    // is closer in absolute tenths.
    match TICKS.binary_search(&clamped) {
        Ok(i) => PriceTick::from_index(i),
        Err(0) => PriceTick::from_index(0),
        Err(i) if i >= NUM_TICKS => PriceTick::from_index(NUM_TICKS - 1),
        Err(i) => {
            let lo = TICKS[i - 1];
            let hi = TICKS[i];
            if (clamped - lo) <= (hi - clamped) {
                PriceTick::from_index(i - 1)
            } else {
                PriceTick::from_index(i)
            }
        }
    }
}

/// Cancel a tracked resting order if it still exists, regardless of fill
/// state. Always clears the slot, mirroring market_maker behavior.
fn cancel_tracked(engine: &mut SeriesCtx<'_>, bot_addr: Address, slot: &mut Option<OrderId>) {
    if let Some(id) = slot.take()
        && engine.book().get_order(id).is_some()
    {
        let _ = engine.cancel(bot_addr, id);
    }
}

/// Place a limit and, if it actually rested, stash the id in `slot`.
fn place_and_track(
    engine: &mut SeriesCtx<'_>,
    bot_addr: Address,
    side: Side,
    price: PriceTick,
    qty: Qty,
    slot: &mut Option<OrderId>,
) -> bool {
    match engine.place_limit(bot_addr, side, price, qty, TradeSource::Bot) {
        Ok(outcome) => {
            *slot = engine
                .book()
                .get_order(outcome.order_id)
                .map(|o| o.id);
            true
        }
        Err(_) => false,
    }
}

/// Random qty within the bot's size band. Picks an integer ≥ 1.
fn pick_qty<R: Rng + ?Sized>(params: &BotParams, rng: &mut R) -> u32 {
    let lo = params.base_qty.max(1);
    let hi = params.max_qty.max(lo);
    rng.random_range(lo..=hi)
}

// ──────────────────────── Conviction + time pressure ──────────────────────

/// At `time_pressure = 0` (early in the round), fair value within this
/// many tenths of the boundary forces passive bots to one-sided quoting.
/// As pressure rises toward 1 the band widens linearly so even moderate
/// FV drifts collapse to one-sided in the final seconds. See
/// [`conviction_band_tenths`] for the actual schedule.
const CONVICTION_TENTHS: i32 = 50;

/// Additional widening of the conviction band at `pressure = 1`. With
/// the base 50 + extra 300 = 350, anything outside [350, 650] tenths
/// (i.e. any FV that has *picked a side*) collapses passive bots to
/// the winning side in the final second.
const CONVICTION_MAX_EXTRA_TENTHS: i32 = 300;

/// Anchor cap applied to passive bots in the conviction zone, *before*
/// pressure scaling. At pressure = 0 the cap is 30 tenths (3¢) so
/// PassiveDeep doesn't stay parked at 50¢ when YES is already 99¢
/// certain — it climbs. At pressure = 1 the cap collapses to 0 and the
/// winning-side bot quotes at exactly FV — i.e. the boundary tick
/// (99.9¢ or 0.1¢) once FV has pinned there.
const CONVICTION_ANCHOR_CAP: i32 = 30;

/// Duration of the end-of-market amplification window. Within the last
/// `FINAL_WINDOW_SECS` seconds, `time_pressure` ramps from 0 to 1, and
/// taker thresholds collapse, passive anchors shrink to zero, the
/// conviction band widens to ~350 tenths, and cadence is overridden in
/// `spawn::spawn_bot` to a sub-second loop.
pub const FINAL_WINDOW_SECS: f64 = 10.0;

/// `velocity_bps` magnitudes at or below this are treated as noise. The
/// surge term in [`act_taker`] kicks in only when the recent 2-second
/// BTC move exceeds this floor.
const VELOCITY_NOISE_FLOOR_BPS: i32 = 5;

/// When a fresh BTC move (over 2 seconds) is at least this many bps and
/// opposite to the cumulative drift vs. strike, takers follow the
/// velocity instead of the cumulative bps. This is the path that
/// produces a "flip" — a sharp drop late in a round overrides the
/// earlier upward bias and floods the ask.
const VELOCITY_DOMINANCE_BPS: i32 = 12;

/// Cap on the taker order size after surge + pressure multipliers. Sized
/// to be visibly large compared with the typical passive resting quote
/// (25–200 contracts) and to bite into the market maker's 2,000-contract
/// touch level, so a surge taker can still sweep a level or two.
const TAKER_QTY_CEILING: u32 = 2_500;

/// Time-pressure ramp: 0 for the bulk of the round, climbing linearly
/// to 1 as `secs_remaining` falls to 0 inside the final
/// [`FINAL_WINDOW_SECS`] seconds. The bot-decision layers consume this
/// to widen the conviction band, shrink anchor caps, lower taker
/// thresholds, and inflate taker quantities.
pub fn time_pressure(secs_remaining: f64) -> f64 {
    if !secs_remaining.is_finite() || secs_remaining >= FINAL_WINDOW_SECS {
        0.0
    } else if secs_remaining <= 0.0 {
        1.0
    } else {
        1.0 - (secs_remaining / FINAL_WINDOW_SECS)
    }
}

/// Conviction-band width (in tenths) at the given pressure.
fn conviction_band_tenths(pressure: f64) -> i32 {
    let extra = (CONVICTION_MAX_EXTRA_TENTHS as f64 * pressure).round() as i32;
    CONVICTION_TENTHS + extra
}

/// Determine the side a passive bot should still be quoting on, given
/// a perceived fair value and the current time-pressure. `Some(Side::Bid)`
/// means YES is near-certain and only bids belong on the book;
/// `Some(Side::Ask)` means NO is near-certain and only asks belong;
/// `None` means quote both sides normally. The band widens with pressure.
fn conviction_side(perceived_fv: i32, pressure: f64) -> Option<Side> {
    let band = conviction_band_tenths(pressure);
    // Strictly inside the band: a mark sitting exactly on a source's cap
    // (the NBA mark at 95¢) keeps both sides on the book.
    if perceived_fv > 1000 - band {
        Some(Side::Bid)
    } else if perceived_fv < band {
        Some(Side::Ask)
    } else {
        None
    }
}

/// Anchor cap applied in the conviction zone, scaled down by pressure.
/// At pressure = 1 the cap is 0 and quotes land at exactly FV.
fn conviction_anchor_cap(pressure: f64) -> i32 {
    ((CONVICTION_ANCHOR_CAP as f64) * (1.0 - pressure)).round().max(0.0) as i32
}

// ────────────────────────────── Decision step ─────────────────────────────

/// One pass of the bot's logic: react to the current market through the
/// series' mark source. Returns `true` iff anything mutated the book
/// (placement or cancel that actually fired).
///
/// The mark source supplies fair value, the underlying's drift since the
/// round opened and its short-horizon velocity; both basis-point readings
/// are divided by the source's `bps_unit` so the archetype thresholds mean
/// the same thing on every underlying.
///
/// Synchronous on purpose — the tokio task wraps this and the unit tests
/// invoke it directly without spinning up a runtime.
pub fn decide_and_act<R: Rng + ?Sized>(
    engine: &mut SeriesCtx<'_>,
    bot: &mut BotState,
    mark: &dyn MarkSource,
    rng: &mut R,
) -> bool {
    let Some(market) = engine.current_market().copied() else {
        return false;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let secs_remaining = (market.closes_at as i64 - now as i64).max(0) as f64;
    let pressure = time_pressure(secs_remaining);
    let Some(inputs) = mark.inputs(&market, secs_remaining) else {
        return false;
    };
    let unit = mark.bps_unit().max(1);
    let bps = inputs.drift_bps / unit;
    let velocity_bps = inputs.velocity_bps / unit;

    // Market rollover: engine cancelled our resting orders at resolution.
    // Forget the stale ids before doing anything else.
    if bot.last_market_id != Some(market.id.0) {
        bot.resting_bid = None;
        bot.resting_ask = None;
        bot.last_fv_seen = None;
        bot.last_market_id = Some(market.id.0);
    }

    // Per-bot perception noise — they don't all agree on FV exactly — kept
    // inside the source's bounds so noise cannot carry a capped mark past
    // its cap and into the conviction zone.
    let (floor, cap) = mark.fv_bounds(secs_remaining);
    let perceived_fv = (inputs.fv_tenths as i32 + bot.params.perception_noise_tenths as i32)
        .clamp(i32::from(floor).max(1), i32::from(cap).min(999));

    match bot.archetype {
        Archetype::PassiveNear | Archetype::PassiveMid | Archetype::PassiveDeep => {
            act_passive(engine, bot, perceived_fv, bps, pressure, rng)
        }
        Archetype::MomentumTaker => act_taker(engine, bot, bps, velocity_bps, false, pressure, rng),
        Archetype::Contrarian => act_taker(engine, bot, bps, velocity_bps, true, pressure, rng),
        Archetype::Noise => act_noise(engine, bot, perceived_fv, rng),
    }
}

fn act_passive<R: Rng + ?Sized>(
    engine: &mut SeriesCtx<'_>,
    bot: &mut BotState,
    perceived_fv: i32,
    bps: i32,
    pressure: f64,
    rng: &mut R,
) -> bool {
    // In conviction zone the losing side empties out and the winning side
    // clusters tight near the boundary — the canonical look of a digital
    // market in its final seconds. The band widens with time-pressure so
    // even modest FV drifts collapse to one-sided near expiry.
    let conv = conviction_side(perceived_fv, pressure);
    let place_bid = conv != Some(Side::Ask);
    let place_ask = conv != Some(Side::Bid);

    let bid_alive = bot.resting_bid.is_some_and(|id| engine.book().get_order(id).is_some());
    let ask_alive = bot.resting_ask.is_some_and(|id| engine.book().get_order(id).is_some());

    // The "skip if nothing's changed" check has to track BOTH whether each
    // side we want is alive AND that no stale order sits on a side we
    // shouldn't be quoting. Otherwise a bot that just rolled into the
    // conviction zone would happily sit on its now-wrong ask.
    let bid_state_ok = place_bid == bid_alive;
    let ask_state_ok = place_ask == ask_alive;
    // In the final window we never short-circuit — even tiny FV drift
    // matters when there's 2s left, and slow archetypes that just woke
    // up should not skip their one chance to swing.
    if pressure <= 0.0
        && bid_state_ok
        && ask_state_ok
        && let Some(prev) = bot.last_fv_seen
    {
        let drift = (perceived_fv - prev as i32).unsigned_abs() as u16;
        if drift < bot.params.reaction_threshold_tenths {
            return false;
        }
    }

    // Anchor: tightened in conviction so even PassiveDeep clusters
    // near the boundary instead of leaving size at 50¢. As pressure
    // approaches 1 the cap collapses to 0 and the winning-side quote
    // lands at exactly FV — i.e. the boundary tick once FV pins to
    // 999 or 1.
    let base_anchor = bot.params.anchor_tenths as i32;
    let anchor = if conv.is_some() {
        base_anchor.min(conviction_anchor_cap(pressure))
    } else {
        base_anchor
    };

    // Directional skew: lean tighter on the side our view favors. Skipped
    // entirely in conviction zone since we're already one-sided.
    let (bid_anchor, ask_anchor) = if conv.is_some() {
        (anchor, anchor)
    } else if bps > 5 {
        ((anchor * 7 / 10).max(1), (anchor * 13 / 10).max(1))
    } else if bps < -5 {
        ((anchor * 13 / 10).max(1), (anchor * 7 / 10).max(1))
    } else {
        (anchor, anchor)
    };

    let bid_tick = nearest_tick(perceived_fv - bid_anchor);
    let ask_tick = nearest_tick(perceived_fv + ask_anchor);
    if place_bid && bid_tick.is_none() {
        return false;
    }
    if place_ask && ask_tick.is_none() {
        return false;
    }
    // Sanity: when both sides are active, bid must sit strictly below ask.
    if place_bid
        && place_ask
        && let (Some(b), Some(a)) = (bid_tick, ask_tick)
        && b.tenths() >= a.tenths()
    {
        return false;
    }

    // Wipe stale orders unconditionally — the engine refunds the lock on
    // cancel and this is the simplest way to also drop a now-suppressed
    // losing-side order.
    cancel_tracked(engine, bot.address, &mut bot.resting_bid);
    cancel_tracked(engine, bot.address, &mut bot.resting_ask);

    let qty = Qty(pick_qty(&bot.params, rng));
    let mut changed = false;
    if let (true, Some(tick)) = (place_bid, bid_tick) {
        changed |= place_and_track(engine, bot.address, Side::Bid, tick, qty, &mut bot.resting_bid);
    }
    if let (true, Some(tick)) = (place_ask, ask_tick) {
        changed |= place_and_track(engine, bot.address, Side::Ask, tick, qty, &mut bot.resting_ask);
    }

    bot.last_fv_seen = Some(perceived_fv as u16);
    // Even a cancel-only step changed the book, so report any state mismatch
    // as a change.
    changed || !bid_state_ok || !ask_state_ok
}

fn act_taker<R: Rng + ?Sized>(
    engine: &mut SeriesCtx<'_>,
    bot: &mut BotState,
    bps: i32,
    velocity_bps: i32,
    fade: bool,
    pressure: f64,
    rng: &mut R,
) -> bool {
    // Surge: how far the most recent BTC move exceeds the per-tick
    // noise floor. Drives both threshold reduction and size inflation.
    // Floor at 0 so a *calm* market doesn't subtract from the threshold.
    let surge = (velocity_bps.abs() - VELOCITY_NOISE_FLOOR_BPS).max(0);

    // Effective threshold collapses on two independent inputs: a fresh
    // velocity surge and end-of-market time pressure. Either alone can
    // pull it to zero.
    let base = bot.params.momentum_threshold_bps;
    let surge_reduction = surge / 2;
    let pressure_reduction = (base as f64 * pressure).round() as i32;
    let effective_threshold = (base - surge_reduction - pressure_reduction).max(0);

    // Direction: cumulative drift vs. strike normally drives the trade
    // (MomentumTaker rides it; Contrarian fades it). A sharp velocity
    // surge that points the *opposite* way overrides cumulative — that's
    // the "sudden drop flips the book" path the user wants.
    let signal =
        if velocity_bps.abs() >= VELOCITY_DOMINANCE_BPS && velocity_bps.signum() != bps.signum() {
            velocity_bps
        } else {
            bps
        };
    // Never fire on a zero signal even if the threshold collapsed to 0:
    // we have no direction to take.
    if signal.abs() < effective_threshold.max(1) {
        return false;
    }
    let mut side = if signal > 0 { Side::Bid } else { Side::Ask };
    if fade {
        side = side.opposite();
    }

    // Size: the larger of the surge or pressure multiplier. Surge can
    // 7× the order on a 30 bps/2s move; pressure can 5× it in the last
    // second. Clamped at TAKER_QTY_CEILING since this is hand-tuned
    // taker flow, not an arbitrage engine.
    let surge_mult = 1 + (surge / 4).min(6) as u32;
    let pressure_mult = 1 + (4.0 * pressure).round() as u32;
    let mult = surge_mult.max(pressure_mult);
    let base_qty = pick_qty(&bot.params, rng);
    let qty_raw = base_qty.saturating_mul(mult);
    let qty = qty_raw.clamp(1, TAKER_QTY_CEILING);

    engine
        .place_market(bot.address, side, Qty(qty), TradeSource::Bot)
        .is_ok()
}

fn act_noise<R: Rng + ?Sized>(
    engine: &mut SeriesCtx<'_>,
    bot: &mut BotState,
    perceived_fv: i32,
    rng: &mut R,
) -> bool {
    // Most ticks the noise bot does nothing — the random spacing matters
    // more than the noise itself.
    if !rng.random_bool(0.3) {
        return false;
    }
    let side = if rng.random_bool(0.5) { Side::Bid } else { Side::Ask };
    let anchor = bot.params.anchor_tenths as i32;
    let target_tenths = match side {
        Side::Bid => perceived_fv - anchor,
        Side::Ask => perceived_fv + anchor,
    };
    let Some(price) = nearest_tick(target_tenths) else {
        return false;
    };
    let qty = Qty(pick_qty(&bot.params, rng));
    let slot = match side {
        Side::Bid => &mut bot.resting_bid,
        Side::Ask => &mut bot.resting_ask,
    };
    cancel_tracked(engine, bot.address, slot);
    place_and_track(engine, bot.address, side, price, qty, slot)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Engine;
    use crate::pricing::mark::{MarkInputs, SpotMark};
    use crate::types::Tenths;
    use rand::SeedableRng;
    use rand::rngs::StdRng;

    /// Build an engine with an open market that closes 60s in the future
    /// and a counterparty account that can absorb taker market orders.
    fn engine_with_open_market() -> (Engine, Address) {
        let mut e = Engine::new(Tenths(1_000_000));
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        e.open_first_market(100_000.0, now, 60);

        // Counterparty: post liquidity only in the outer tails so takers
        // always find a fill but passive bot orders placed near the mid
        // (FV ≈ 500) don't immediately cross. Bids sit ≤ 40¢, asks ≥ 60¢.
        let counter = Address([0xCC; 20]);
        e.seed_bot_account(counter, Tenths(50_000_000), "counter".into(), None, None);
        for tenths in [100u16, 200, 300, 400] {
            let p = PriceTick::from_tenths(tenths).unwrap();
            e.place_limit(counter, Side::Bid, p, Qty(200), TradeSource::Bot)
                .unwrap();
        }
        for tenths in [600u16, 700, 800, 900] {
            let p = PriceTick::from_tenths(tenths).unwrap();
            e.place_limit(counter, Side::Ask, p, Qty(200), TradeSource::Bot)
                .unwrap();
        }
        (e, counter)
    }

    /// Open market with no counterparty liquidity — the bot's quotes
    /// won't cross anyone, so we can verify resting state directly.
    fn engine_with_open_market_clean() -> Engine {
        engine_with_open_market_clean_duration(60)
    }

    /// Open a market with a custom duration so we can hit the
    /// final-window time-pressure ramp without sleeping in tests.
    fn engine_with_open_market_clean_duration(duration_secs: u64) -> Engine {
        let mut e = Engine::new(Tenths(1_000_000));
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        e.open_first_market(100_000.0, now, duration_secs);
        e
    }

    /// Same idea but with counterparty liquidity that spans the whole
    /// book so a taker market order always finds a fill.
    fn engine_with_full_counter_liquidity(duration_secs: u64) -> (Engine, Address) {
        let mut e = engine_with_open_market_clean_duration(duration_secs);
        let counter = Address([0xCC; 20]);
        e.seed_bot_account(counter, Tenths(500_000_000), "counter".into(), None, None);
        for tenths in [100u16, 200, 300, 400, 460, 480] {
            let p = PriceTick::from_tenths(tenths).unwrap();
            e.place_limit(counter, Side::Bid, p, Qty(300), TradeSource::Bot)
                .unwrap();
        }
        for tenths in [520u16, 540, 600, 700, 800, 900] {
            let p = PriceTick::from_tenths(tenths).unwrap();
            e.place_limit(counter, Side::Ask, p, Qty(300), TradeSource::Bot)
                .unwrap();
        }
        (e, counter)
    }

    fn fresh_bot(arch: Archetype, anchor_tenths: u16) -> BotState {
        let params = BotParams {
            anchor_tenths,
            base_qty: 5,
            max_qty: 10,
            reaction_threshold_tenths: 5,
            reprice_interval_min_ms: 1_000,
            reprice_interval_max_ms: 2_000,
            momentum_threshold_bps: 10,
            perception_noise_tenths: 0,
        };
        let addr = Address([0xAB; 20]);
        BotState::new(addr, arch, params, Tenths(5_000_000), crate::types::DEFAULT_SERIES)
    }

    #[test]
    fn scaled_taker_threshold_stays_quiet_on_small_move_in_15m() {
        // 15 bps of drift fires a 10 bps taker in a 60 s round …
        let (mut e, _) = engine_with_full_counter_liquidity(900);
        let mut bot = fresh_bot(Archetype::MomentumTaker, 0);
        e.seed_bot_account(bot.address, bot.bankroll, "taker".into(), None, None);
        let mut rng = StdRng::seed_from_u64(303);
        assert!(decide_and_act(&mut e.ctx(), &mut bot, &SpotMark(100_150.0, 0), &mut rng));

        // … but the same draw scaled for a 900 s round needs ~39 bps.
        let (mut e, _) = engine_with_full_counter_liquidity(900);
        let mut bot = fresh_bot(Archetype::MomentumTaker, 0);
        bot.params = bot.params.scaled_for(900);
        assert_eq!(bot.params.momentum_threshold_bps, 39);
        e.seed_bot_account(bot.address, bot.bankroll, "taker".into(), None, None);
        let mut rng = StdRng::seed_from_u64(303);
        assert!(!decide_and_act(&mut e.ctx(), &mut bot, &SpotMark(100_150.0, 0), &mut rng));
        assert!(decide_and_act(&mut e.ctx(), &mut bot, &SpotMark(100_500.0, 0), &mut rng));
    }

    // ───────────────────────── micro-helper tests ─────────────────────────

    #[test]
    fn nearest_tick_snaps_to_grid_in_coarse_middle() {
        // 504 tenths is not on the grid (50¢=500, 51¢=510); snap to 500.
        assert_eq!(nearest_tick(504).unwrap().tenths(), 500);
        // 506 should snap up to 510.
        assert_eq!(nearest_tick(506).unwrap().tenths(), 510);
    }

    #[test]
    fn nearest_tick_clamps_out_of_range() {
        assert_eq!(nearest_tick(-5).unwrap().tenths(), 1);
        assert_eq!(nearest_tick(9_999).unwrap().tenths(), 999);
    }

    // ───────────────────────── passive behavior ─────────────────────────

    /// A source that caps its mark at 95¢ the way the NBA one does.
    struct CappedMark;

    impl MarkSource for CappedMark {
        fn inputs(&self, _market: &crate::engine::Market, _secs: f64) -> Option<MarkInputs> {
            Some(MarkInputs { fv_tenths: 950, drift_bps: 0, velocity_bps: 0 })
        }

        fn fv_bounds(&self, _secs: f64) -> (u16, u16) {
            (50, 950)
        }
    }

    #[test]
    fn perception_noise_cannot_carry_a_capped_mark_into_conviction() {
        let mut engine = engine_with_open_market_clean();
        let mut bot = fresh_bot(Archetype::PassiveNear, 20);
        bot.params.perception_noise_tenths = 10;
        engine.seed_bot_account(bot.address, bot.bankroll, "bot".into(), None, None);
        let mut rng = StdRng::seed_from_u64(1);
        assert!(decide_and_act(&mut engine.ctx(), &mut bot, &CappedMark, &mut rng));
        let bid = engine.book().get_order(bot.resting_bid.expect("bid rests")).unwrap();
        let ask = engine.book().get_order(bot.resting_ask.expect("ask rests at the cap")).unwrap();
        assert!(bid.price.tenths() < 950 && ask.price.tenths() > 950);
        assert_eq!(bot.last_fv_seen, Some(950), "perceived fair value is clamped to the cap");
    }

    #[test]
    fn passive_near_rests_both_sides_around_fv() {
        let (mut engine, _) = engine_with_open_market();
        let mut bot = fresh_bot(Archetype::PassiveNear, 20);
        engine.seed_bot_account(bot.address, bot.bankroll, "bot".into(), None, None);
        let mut rng = StdRng::seed_from_u64(1);

        // BTC == strike → FV ≈ 500.
        let changed = decide_and_act(&mut engine.ctx(), &mut bot, &SpotMark(100_000.0, 0), &mut rng);
        assert!(changed, "expected placements");
        let bid_id = bot.resting_bid.expect("bid was placed");
        let ask_id = bot.resting_ask.expect("ask was placed");
        let bid = engine.book().get_order(bid_id).unwrap();
        let ask = engine.book().get_order(ask_id).unwrap();
        assert!(bid.price.tenths() < 500, "bid below FV");
        assert!(ask.price.tenths() > 500, "ask above FV");
        // Anchor was 20 tenths; expect within ~10 tenths after snapping.
        assert!((500_i32 - bid.price.tenths() as i32).abs() <= 30);
        assert!((ask.price.tenths() as i32 - 500).abs() <= 30);
    }

    #[test]
    fn passive_skips_when_fv_drift_is_tiny() {
        let (mut engine, _) = engine_with_open_market();
        let mut bot = fresh_bot(Archetype::PassiveNear, 20);
        engine.seed_bot_account(bot.address, bot.bankroll, "bot".into(), None, None);
        let mut rng = StdRng::seed_from_u64(2);

        let first = decide_and_act(&mut engine.ctx(), &mut bot, &SpotMark(100_000.0, 0), &mut rng);
        assert!(first);
        // Second call with same FV: drift = 0 < threshold (5), so no action.
        let second = decide_and_act(&mut engine.ctx(), &mut bot, &SpotMark(100_000.0, 0), &mut rng);
        assert!(!second, "bot should sit still when FV hasn't moved");
    }

    #[test]
    fn passive_deep_anchors_far_from_fv() {
        let (mut engine, _) = engine_with_open_market();
        let mut bot = fresh_bot(Archetype::PassiveDeep, 400);
        engine.seed_bot_account(bot.address, bot.bankroll, "bot".into(), None, None);
        let mut rng = StdRng::seed_from_u64(3);

        decide_and_act(&mut engine.ctx(), &mut bot, &SpotMark(100_000.0, 0), &mut rng);
        let bid = engine.book().get_order(bot.resting_bid.unwrap()).unwrap();
        let ask = engine.book().get_order(bot.resting_ask.unwrap()).unwrap();
        // Anchored ~400 tenths off mid → bid in the low tail, ask in the high tail.
        assert!(bid.price.tenths() <= 150, "deep bid sits low, got {}", bid.price.tenths());
        assert!(ask.price.tenths() >= 850, "deep ask sits high, got {}", ask.price.tenths());
    }

    // ───────────────────────── taker behavior ─────────────────────────

    #[test]
    fn momentum_taker_fires_when_above_threshold() {
        let (mut engine, _) = engine_with_open_market();
        let mut bot = fresh_bot(Archetype::MomentumTaker, 0);
        engine.seed_bot_account(bot.address, bot.bankroll, "taker".into(), None, None);
        let mut rng = StdRng::seed_from_u64(4);

        // BTC 0.5 % above strike → ~50 bps, threshold is 10 bps. Should fire
        // a market order on the bid (buy YES) and leave a positive position.
        let before = engine.position(&bot.address).position;
        let changed = decide_and_act(&mut engine.ctx(), &mut bot, &SpotMark(100_500.0, 0), &mut rng);
        assert!(changed, "taker should have crossed");
        let after = engine.position(&bot.address).position;
        assert!(after.0 > before.0, "long position increased: {:?} → {:?}", before, after);
    }

    #[test]
    fn momentum_taker_skips_when_below_threshold() {
        let (mut engine, _) = engine_with_open_market();
        let mut bot = fresh_bot(Archetype::MomentumTaker, 0);
        engine.seed_bot_account(bot.address, bot.bankroll, "taker".into(), None, None);
        let mut rng = StdRng::seed_from_u64(5);

        // BTC 0.01 % above strike → ~1 bp, below threshold (10).
        let changed = decide_and_act(&mut engine.ctx(), &mut bot, &SpotMark(100_010.0, 0), &mut rng);
        assert!(!changed, "taker should not act on a sub-threshold move");
    }

    #[test]
    fn contrarian_fades_the_move() {
        let (mut engine, _) = engine_with_open_market();
        let mut bot = fresh_bot(Archetype::Contrarian, 0);
        engine.seed_bot_account(bot.address, bot.bankroll, "fader".into(), None, None);
        let mut rng = StdRng::seed_from_u64(6);

        // BTC above strike → momentum says buy YES; contrarian sells NO
        // (Side::Ask), so position should go negative (short).
        let changed = decide_and_act(&mut engine.ctx(), &mut bot, &SpotMark(100_500.0, 0), &mut rng);
        assert!(changed);
        let pos = engine.position(&bot.address).position;
        assert!(pos.0 < 0, "contrarian opens short, got {:?}", pos);
    }

    // ───────────────────────── lifecycle / sanity ─────────────────────────

    #[test]
    fn market_rollover_clears_tracked_order_ids() {
        let (mut engine, _) = engine_with_open_market();
        let mut bot = fresh_bot(Archetype::PassiveNear, 20);
        engine.seed_bot_account(bot.address, bot.bankroll, "bot".into(), None, None);
        let mut rng = StdRng::seed_from_u64(7);

        decide_and_act(&mut engine.ctx(), &mut bot, &SpotMark(100_000.0, 0), &mut rng);
        assert!(bot.resting_bid.is_some() && bot.resting_ask.is_some());

        // Simulate a market resolution + new market opening.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        engine.resolve_and_open_next(100_000.0, 100_000.0, now, 60);

        // Next decision should observe the new market id and drop the
        // stale ids before placing fresh ones.
        decide_and_act(&mut engine.ctx(), &mut bot, &SpotMark(100_000.0, 0), &mut rng);
        let new_market_id = engine.current_market().unwrap().id.0;
        assert_eq!(bot.last_market_id, Some(new_market_id));
    }

    #[test]
    fn decide_does_nothing_without_open_market() {
        let mut engine = Engine::new(Tenths(1_000_000));
        let mut bot = fresh_bot(Archetype::PassiveNear, 20);
        engine.seed_bot_account(bot.address, bot.bankroll, "bot".into(), None, None);
        let mut rng = StdRng::seed_from_u64(8);
        let changed = decide_and_act(&mut engine.ctx(), &mut bot, &SpotMark(100_000.0, 0), &mut rng);
        assert!(!changed);
        assert!(bot.resting_bid.is_none() && bot.resting_ask.is_none());
    }

    // ───────────────────────── conviction-zone behavior ─────────────────────────

    #[test]
    fn conviction_side_classifies_zones() {
        // pressure = 0 → band = 50 tenths
        assert_eq!(conviction_side(500, 0.0), None);
        assert_eq!(conviction_side(951, 0.0), Some(Side::Bid));
        assert_eq!(conviction_side(999, 0.0), Some(Side::Bid));
        assert_eq!(conviction_side(49, 0.0), Some(Side::Ask));
        assert_eq!(conviction_side(1, 0.0), Some(Side::Ask));
        assert_eq!(conviction_side(900, 0.0), None);
        assert_eq!(conviction_side(100, 0.0), None);
        // On the band's edge (the NBA mark's cap and floor) both sides stay quoted.
        assert_eq!(conviction_side(950, 0.0), None);
        assert_eq!(conviction_side(50, 0.0), None);
    }

    #[test]
    fn conviction_band_widens_with_pressure() {
        // pressure = 1 → band = 350 → anything above 650 collapses to Bid,
        // anything below 350 collapses to Ask; the edges stay two-sided.
        assert_eq!(conviction_side(651, 1.0), Some(Side::Bid));
        assert_eq!(conviction_side(800, 1.0), Some(Side::Bid));
        assert_eq!(conviction_side(349, 1.0), Some(Side::Ask));
        assert_eq!(conviction_side(200, 1.0), Some(Side::Ask));
        assert_eq!(conviction_side(650, 1.0), None);
        assert_eq!(conviction_side(350, 1.0), None);
        // Right in the middle: still two-sided even at pressure = 1.
        assert_eq!(conviction_side(500, 1.0), None);
    }

    #[test]
    fn time_pressure_ramps() {
        assert_eq!(time_pressure(60.0), 0.0);
        assert_eq!(time_pressure(FINAL_WINDOW_SECS), 0.0);
        assert!((time_pressure(5.0) - 0.5).abs() < 1e-9);
        assert!((time_pressure(2.0) - 0.8).abs() < 1e-9);
        assert_eq!(time_pressure(0.0), 1.0);
        assert_eq!(time_pressure(-3.0), 1.0);
        // Non-finite inputs default to "no pressure" — don't accidentally
        // amplify on bogus data.
        assert_eq!(time_pressure(f64::NAN), 0.0);
        assert_eq!(time_pressure(f64::INFINITY), 0.0);
    }

    #[test]
    fn conviction_anchor_cap_collapses_with_pressure() {
        assert_eq!(conviction_anchor_cap(0.0), 30);
        assert_eq!(conviction_anchor_cap(0.5), 15);
        assert_eq!(conviction_anchor_cap(1.0), 0);
    }

    #[test]
    fn passive_suppresses_ask_when_yes_is_near_certain() {
        let mut e = engine_with_open_market_clean();
        let mut bot = fresh_bot(Archetype::PassiveNear, 20);
        e.seed_bot_account(bot.address, bot.bankroll, "bot".into(), None, None);
        let mut rng = StdRng::seed_from_u64(101);

        // BTC 1 % above strike with 60 s left → FV pins at 999 (yes-certain).
        let changed = decide_and_act(&mut e.ctx(), &mut bot, &SpotMark(101_000.0, 0), &mut rng);
        assert!(changed, "expected at least a bid placement");
        assert!(bot.resting_bid.is_some(), "winning-side bid should be placed");
        assert!(bot.resting_ask.is_none(), "losing-side ask should be suppressed");
    }

    #[test]
    fn passive_suppresses_bid_when_no_is_near_certain() {
        let mut e = engine_with_open_market_clean();
        let mut bot = fresh_bot(Archetype::PassiveNear, 20);
        e.seed_bot_account(bot.address, bot.bankroll, "bot".into(), None, None);
        let mut rng = StdRng::seed_from_u64(102);

        let changed = decide_and_act(&mut e.ctx(), &mut bot, &SpotMark(99_000.0, 0), &mut rng);
        assert!(changed);
        assert!(bot.resting_ask.is_some(), "winning-side ask should be placed");
        assert!(bot.resting_bid.is_none(), "losing-side bid should be suppressed");
    }

    #[test]
    fn entering_conviction_cancels_stale_losing_side() {
        let mut e = engine_with_open_market_clean();
        let mut bot = fresh_bot(Archetype::PassiveNear, 20);
        e.seed_bot_account(bot.address, bot.bankroll, "bot".into(), None, None);
        let mut rng = StdRng::seed_from_u64(103);

        // First, mid-market: both sides placed.
        decide_and_act(&mut e.ctx(), &mut bot, &SpotMark(100_000.0, 0), &mut rng);
        assert!(bot.resting_bid.is_some() && bot.resting_ask.is_some());
        let stale_ask = bot.resting_ask.unwrap();

        // BTC rockets → FV pins at 999 → ask suppressed and existing
        // ask cancelled out of the book.
        decide_and_act(&mut e.ctx(), &mut bot, &SpotMark(101_000.0, 0), &mut rng);
        assert!(bot.resting_ask.is_none(), "losing-side ask was cancelled");
        assert!(e.book().get_order(stale_ask).is_none(), "stale ask removed from book");
    }

    #[test]
    fn passive_deep_clusters_near_boundary_in_conviction() {
        let mut e = engine_with_open_market_clean();
        // PassiveDeep would normally anchor 300+ tenths from FV. In
        // conviction the anchor is capped so its bid lands close to FV,
        // not at 50¢.
        let mut bot = fresh_bot(Archetype::PassiveDeep, 400);
        e.seed_bot_account(bot.address, bot.bankroll, "bot".into(), None, None);
        let mut rng = StdRng::seed_from_u64(104);

        decide_and_act(&mut e.ctx(), &mut bot, &SpotMark(101_000.0, 0), &mut rng);
        let bid = e.book().get_order(bot.resting_bid.unwrap()).unwrap();
        // FV is 999, anchor capped at 30 → bid lands in the 96–99¢ band.
        assert!(
            bid.price.tenths() >= 960,
            "deep passive should cluster near boundary in conviction, got {}",
            bid.price.tenths(),
        );
    }

    // ───────────────────────── velocity surge & end-of-market ─────────────────────────

    #[test]
    fn velocity_surge_lowers_taker_threshold() {
        // A taker with threshold 10 bps would normally skip a 3 bp drift.
        // With a 20 bps/2s velocity surge, the effective threshold collapses
        // and the taker fires.
        let (mut e, _) = engine_with_full_counter_liquidity(60);
        let mut bot = fresh_bot(Archetype::MomentumTaker, 0);
        e.seed_bot_account(bot.address, bot.bankroll, "taker".into(), None, None);
        let mut rng = StdRng::seed_from_u64(201);

        let before = e.position(&bot.address).position;
        // BTC 0.03 % above strike → +3 bps cumulative, well below the 10
        // bp threshold. But surge says BTC is ripping +20 bps in the
        // last 2s — fire.
        let changed = decide_and_act(&mut e.ctx(), &mut bot, &SpotMark(100_030.0, 20), &mut rng);
        assert!(changed, "taker should fire under velocity surge");
        let after = e.position(&bot.address).position;
        assert!(after.0 > before.0, "long position grew: {:?} → {:?}", before, after);
    }

    #[test]
    fn velocity_surge_does_not_fire_with_zero_signal() {
        // BTC exactly at strike and velocity within the noise floor —
        // even though the bot is otherwise hair-trigger, there's no
        // direction to take, so no order goes out.
        let (mut e, _) = engine_with_full_counter_liquidity(60);
        let mut bot = fresh_bot(Archetype::MomentumTaker, 0);
        e.seed_bot_account(bot.address, bot.bankroll, "taker".into(), None, None);
        let mut rng = StdRng::seed_from_u64(202);

        let changed = decide_and_act(&mut e.ctx(), &mut bot, &SpotMark(100_000.0, 3), &mut rng);
        assert!(!changed, "no direction → no fire");
    }

    #[test]
    fn velocity_dominance_flips_direction_against_cumulative() {
        // Cumulative drift says BTC is above strike (would buy YES), but
        // BTC is plummeting fast — velocity dominates and the bot sells
        // YES.
        let (mut e, _) = engine_with_full_counter_liquidity(60);
        let mut bot = fresh_bot(Archetype::MomentumTaker, 0);
        e.seed_bot_account(bot.address, bot.bankroll, "taker".into(), None, None);
        let mut rng = StdRng::seed_from_u64(203);

        // bps = +20 cumulative, velocity = -25 (dominant). Should open
        // short instead of long.
        let before = e.position(&bot.address).position;
        let changed = decide_and_act(&mut e.ctx(), &mut bot, &SpotMark(100_200.0, -25), &mut rng);
        assert!(changed);
        let after = e.position(&bot.address).position;
        assert!(
            after.0 < before.0,
            "velocity-dominated taker opens short: {:?} → {:?}",
            before,
            after,
        );
    }

    #[test]
    fn velocity_surge_inflates_taker_size() {
        // Same cumulative move with vs. without a velocity surge should
        // produce strictly more contracts on the surge side. We can
        // observe this through the resulting position magnitude.
        let mk = |velocity: i32| {
            let (mut e, _) = engine_with_full_counter_liquidity(60);
            let mut bot = fresh_bot(Archetype::MomentumTaker, 0);
            e.seed_bot_account(bot.address, bot.bankroll, "taker".into(), None, None);
            let mut rng = StdRng::seed_from_u64(204);
            decide_and_act(&mut e.ctx(), &mut bot, &SpotMark(100_500.0, velocity), &mut rng);
            e.position(&bot.address).position.0
        };
        let calm = mk(0);
        let surge = mk(30);
        assert!(calm > 0);
        assert!(surge > calm, "surge size > calm size: {} > {}", surge, calm);
    }

    #[test]
    fn end_of_market_pinning_lands_winning_side_at_boundary() {
        // 2 seconds left, FV pinned to 999. Pressure ≈ 0.8 collapses the
        // anchor cap to ~6, and FV → boundary tick puts the bot's bid in
        // the 994–999 fine-top band.
        let mut e = engine_with_open_market_clean_duration(2);
        let mut bot = fresh_bot(Archetype::PassiveNear, 20);
        e.seed_bot_account(bot.address, bot.bankroll, "bot".into(), None, None);
        let mut rng = StdRng::seed_from_u64(205);

        // BTC 1 % above strike with 2s left → FV pins to 999.
        let changed = decide_and_act(&mut e.ctx(), &mut bot, &SpotMark(101_000.0, 0), &mut rng);
        assert!(changed);
        let bid = e.book().get_order(bot.resting_bid.unwrap()).unwrap();
        assert!(
            bid.price.tenths() >= 993,
            "winning-side bid should pin near 99.9¢ in final window, got {}",
            bid.price.tenths(),
        );
        assert!(bot.resting_ask.is_none(), "losing-side suppressed");
    }

    #[test]
    fn end_of_market_widens_conviction_for_moderate_fv() {
        // Mid-round, FV around 700 → no conviction.
        // 1 second left, FV around 700 → conviction (band widens past 350).
        assert_eq!(conviction_side(700, 0.0), None);
        assert_eq!(conviction_side(700, 0.9), Some(Side::Bid));
    }

    #[test]
    fn end_of_market_amplifies_taker_size() {
        let mk = |duration: u64| {
            let (mut e, _) = engine_with_full_counter_liquidity(duration);
            let mut bot = fresh_bot(Archetype::MomentumTaker, 0);
            e.seed_bot_account(bot.address, bot.bankroll, "taker".into(), None, None);
            let mut rng = StdRng::seed_from_u64(206);
            decide_and_act(&mut e.ctx(), &mut bot, &SpotMark(100_500.0, 0), &mut rng);
            e.position(&bot.address).position.0
        };
        let mid = mk(60);
        let end = mk(2);
        assert!(mid > 0);
        assert!(end > mid, "end-of-market size > mid-round size: {} > {}", end, mid);
    }

    #[test]
    fn end_of_market_no_skip_on_tiny_drift() {
        // Same FV both calls — under pressure the bot still re-acts
        // (cancels + replaces) instead of short-circuiting on the drift
        // threshold. The point is that a slow archetype that just woke
        // up shouldn't waste its final-window turn.
        let mut e = engine_with_open_market_clean_duration(2);
        let mut bot = fresh_bot(Archetype::PassiveNear, 20);
        e.seed_bot_account(bot.address, bot.bankroll, "bot".into(), None, None);
        let mut rng = StdRng::seed_from_u64(207);

        let first = decide_and_act(&mut e.ctx(), &mut bot, &SpotMark(101_000.0, 0), &mut rng);
        assert!(first);
        let second = decide_and_act(&mut e.ctx(), &mut bot, &SpotMark(101_000.0, 0), &mut rng);
        assert!(second, "should keep refreshing in the final window");
    }
}
