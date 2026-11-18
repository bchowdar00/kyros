//! Two series, one ledger: bots, a human main account and one strategy
//! sub-account per series churn through thousands of random steps while
//! every cross-series invariant is checked after each one. Then the
//! series settle one at a time and the books are reconciled.

use exchange::bots::traders::decision::decide_and_act;
use exchange::bots::traders::{BotState, build_population};
use exchange::engine::strategies::test_support::idle_record;
use exchange::engine::{Exchange, OrderError, PAYOUT_PER_CONTRACT, TradeSource};
use exchange::pricing::mark::SpotMark;
use exchange::series::SERIES;
use exchange::ticks::PriceTick;
use exchange::types::{Address, OrderId, Qty, SeriesId, Side, Tenths};
use rand::Rng;
use rand::SeedableRng;
use rand::rngs::StdRng;

const START: Tenths = Tenths(1_000_000);
const S0: SeriesId = SeriesId(0);
const S1: SeriesId = SeriesId(1);

/// Σ (balance + locked) over every account plus Σ open interest × $1.
fn total_value(x: &Exchange) -> i64 {
    let cash: i64 = x.accounts_iter().map(|a| a.usdc_balance.0 + a.locked.0).sum();
    let escrow: i64 = x
        .engines()
        .map(|e| e.open_interest() as i64 * PAYOUT_PER_CONTRACT)
        .sum();
    cash + escrow
}

fn family_cash(x: &Exchange, root: Address) -> i64 {
    std::iter::once(root)
        .chain(x.sub_accounts_of(&root).iter().copied())
        .filter_map(|a| x.account(&a))
        .map(|a| a.usdc_balance.0 + a.locked.0)
        .sum()
}

/// Realized-so-far plus the settlement leg every family member would
/// book if `series` resolved with `yes` right now.
fn family_round_pnl_if_settled(x: &Exchange, root: Address, series: SeriesId, yes: bool) -> i64 {
    let price = if yes { PAYOUT_PER_CONTRACT } else { 0 };
    std::iter::once(root)
        .chain(x.sub_accounts_of(&root).iter().copied())
        .map(|a| {
            let ps = x.position(series, &a);
            let p = ps.position.0;
            let leg = if p > 0 {
                price * p - ps.entry_cost_tenths.0
            } else if p < 0 {
                ps.entry_cost_tenths.0 - price * (-p)
            } else {
                0
            };
            ps.realized_pnl_this_market.0 + leg
        })
        .sum()
}

#[test]
fn two_series_conservation() {
    let mut rng = StdRng::seed_from_u64(0x5EED_2025);
    let mut x = Exchange::new(START, SERIES);

    // 40 bots: 25 on the 2m series, 15 on the 15m series.
    let names: Vec<String> = (0..40).map(|i| format!("bot{i:02}")).collect();
    let mut bots: Vec<BotState> = build_population(&mut x, &names[..25], 25, 11, S0);
    bots.extend(build_population(&mut x, &names[25..], 15, 11 ^ (1 << 48), S1));
    let bot_bankrolls: Vec<(Address, Tenths, SeriesId)> =
        bots.iter().map(|b| (b.address, b.bankroll, b.series)).collect();

    // One human with a strategy sub-account per series.
    let human = Address([0xA0; 20]);
    x.register_user(human);
    x.get_or_create_account(human);
    let child0 = x.create_sub_account_in(human, Tenths(100_000), S0).unwrap();
    let child1 = x.create_sub_account_in(human, Tenths(100_000), S1).unwrap();
    // The record marks each child as a live strategy so settlement does not
    // sweep it as an orphan; the runtime is not running in this test, so
    // the strategy orders below are placed by hand.
    x.deploy_strategy_in(child0, idle_record("buy yes", S0, 1)).unwrap();
    x.deploy_strategy_in(child1, idle_record("buy yes", S1, 2)).unwrap();

    // The families' cash is what every deposit adds up to.
    let deposits = total_value(&x);

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    x.open_first_market(S0, 100_000.0, now, 60);
    x.open_first_market(S1, 100_000.0, now, 900);
    x.debug_assert_invariants();

    // Warm both books so the strategies' market buys have something to hit.
    for series in [S0, S1] {
        for bot in bots.iter_mut().filter(|b| b.series == series) {
            decide_and_act(&mut x.ctx(series), bot, &SpotMark(100_000.0, 0), &mut rng);
        }
    }
    let _ = x.place_market(S0, child0, Side::Bid, Qty(3), TradeSource::Strategy);
    let _ = x.place_market(S1, child1, Side::Bid, Qty(3), TradeSource::Strategy);
    x.debug_assert_invariants();
    assert_eq!(total_value(&x), deposits, "conservation broken after warm-up");

    let mut human_orders: Vec<(SeriesId, OrderId)> = Vec::new();
    let mut btc = 100_000.0_f64;
    let mut over_budget_rejections = 0;
    let mut mismatch_rejections = 0;

    for step in 0..2000 {
        btc += rng.random_range(-40.0..40.0);
        let velocity = rng.random_range(-60..=60);

        // A random bot acts on its own series.
        let i = rng.random_range(0..bots.len());
        let series = bots[i].series;
        decide_and_act(&mut x.ctx(series), &mut bots[i], &SpotMark(btc, velocity), &mut rng);

        // The human trades a random series, sometimes far beyond budget.
        let series = if rng.random_bool(0.5) { S0 } else { S1 };
        let side = if rng.random_bool(0.5) { Side::Bid } else { Side::Ask };
        let qty = if rng.random_bool(0.1) {
            Qty(5_000_000) // deliberately unaffordable
        } else {
            Qty(rng.random_range(1..=20))
        };
        let res = if rng.random_bool(0.6) {
            let tick = PriceTick::from_tenths(rng.random_range(30..=97) * 10).unwrap();
            x.place_limit(series, human, side, tick, qty, TradeSource::Ui)
        } else {
            x.place_market(series, human, side, qty, TradeSource::Ui)
        };
        match res {
            Ok(out) if !out.resting_qty.is_zero() => human_orders.push((series, out.order_id)),
            Ok(_) => {}
            Err(OrderError::InsufficientBalance { .. }) => over_budget_rejections += 1,
            Err(OrderError::NoLiquidity) => {}
            Err(e) => panic!("unexpected order error {e:?}"),
        }

        // A bot must never be able to trade the other series.
        if step % 100 == 0 {
            let bot = &bots[i];
            let other = if bot.series == S0 { S1 } else { S0 };
            let tick = PriceTick::from_tenths(500).unwrap();
            match x.place_limit(other, bot.address, Side::Bid, tick, Qty(1), TradeSource::Bot) {
                Err(OrderError::SeriesMismatch { home, requested }) => {
                    assert_eq!((home, requested), (bot.series, other));
                    mismatch_rejections += 1;
                }
                other_result => panic!("cross-series bot order accepted: {other_result:?}"),
            }
        }

        if step % 50 == 0 && !human_orders.is_empty() {
            let (series, id) = human_orders.swap_remove(rng.random_range(0..human_orders.len()));
            // Probe from the wrong series first: cancel_any must still find it.
            let preferred = if series == S0 { S1 } else { S0 };
            match x.cancel_any(preferred, human, id) {
                Ok((found, _)) => assert_eq!(found, series),
                Err(exchange::engine::CancelError::UnknownOrder) => {} // already filled
                Err(e) => panic!("cancel failed: {e:?}"),
            }
        }

        x.debug_assert_invariants();
        assert_eq!(total_value(&x), deposits, "conservation broken at step {step}");
    }
    assert!(over_budget_rejections > 0, "over-budget orders never exercised");
    assert!(mismatch_rejections > 0);

    // ── Resolve the 2m series while the 15m one still holds exposure ──
    let s1_oi_before = x.engine(S1).open_interest();
    assert!(s1_oi_before > 0, "15m series should hold open interest");
    let human_s1_pos = x.position(S1, &human).position.0;
    let child1_pos = x.position(S1, &child1).position.0;
    let close = btc + 500.0;
    let yes = close > 100_000.0;
    let expected_round0 = family_round_pnl_if_settled(&x, human, S0, yes);
    let family_before = family_cash(&x, human);
    // Settlement pays $1 per winning contract; the losing side's stake
    // already left the balance at fill time.
    let expected_settlement_cash: i64 = std::iter::once(human)
        .chain([child0, child1])
        .map(|a| {
            let p = x.position(S0, &a).position.0;
            if yes { p.max(0) } else { (-p).max(0) }
        })
        .sum::<i64>()
        * PAYOUT_PER_CONTRACT;

    let report = x.resolve_and_open_next(S0, close, close, now + 60, 60);
    x.debug_assert_invariants();
    assert_eq!(x.engine(S0).open_interest(), 0);
    assert_eq!(x.engine(S1).open_interest(), s1_oi_before, "15m OI untouched");
    assert_eq!(x.position(S1, &human).position.0, human_s1_pos);
    assert_eq!(x.position(S1, &child1).position.0, child1_pos);
    let standing = x
        .leaderboard_standings()
        .find(|(a, _)| **a == human)
        .map(|(_, s)| *s)
        .expect("human participated");
    assert_eq!(standing.cumulative_pnl_tenths, expected_round0);
    assert_eq!(standing.last_series, S0);
    assert_eq!(standing.last_market_id, report.resolved_id.0);
    // Settlement moves the family's 2m escrow back into cash: exactly
    // its winning 2m contracts × $1.
    let family_after = family_cash(&x, human);
    assert_eq!(family_after - family_before, expected_settlement_cash);
    // 2m bots are back on their bankrolls; 15m bots are not.
    for (a, bankroll, s) in &bot_bankrolls {
        let acct = x.account(a).unwrap();
        if *s == S0 {
            assert_eq!(acct.usdc_balance, *bankroll, "2m bot {a} not reset");
            assert_eq!(acct.locked, Tenths::ZERO);
        }
    }
    assert!(
        bot_bankrolls
            .iter()
            .filter(|(_, _, s)| *s == S1)
            .any(|(a, bankroll, _)| x.account(a).unwrap().usdc_balance != *bankroll
                || !x.position(S1, a).is_flat()),
        "some 15m bot should still be mid-round"
    );

    // ── Resolve the 15m series ──
    let expected_round1 = family_round_pnl_if_settled(&x, human, S1, yes);
    x.resolve_and_open_next(S1, close, close, now + 900, 900);
    x.debug_assert_invariants();
    for (a, bankroll, _) in &bot_bankrolls {
        let acct = x.account(a).unwrap();
        assert_eq!(acct.usdc_balance, *bankroll, "bot {a} not on bankroll");
        assert_eq!(acct.locked, Tenths::ZERO);
    }
    let standing = x
        .leaderboard_standings()
        .find(|(a, _)| **a == human)
        .map(|(_, s)| *s)
        .unwrap();
    assert_eq!(standing.cumulative_pnl_tenths, expected_round0 + expected_round1);
    assert_eq!(standing.last_series, S1);
    assert_eq!(standing.markets_played, 2);
    // Every user family's cash equals its deposit plus its settled PnL.
    assert_eq!(family_cash(&x, human), START.0 + standing.cumulative_pnl_tenths);
    assert!(x.position(S0, &human).is_flat() && x.position(S1, &human).is_flat());
}
