//! Orchestrator that builds a deterministic, heterogeneous bot
//! population and seeds their accounts into the engine.

use crate::engine::Exchange;
use crate::types::SeriesId;

use super::archetypes::{BotParams, BotState, params_for, pick_archetype};
use super::identity::{
    pick_color, pick_emoji, random_bankroll, random_bot_address, seeded_rng, shuffle_names,
};

/// Build one series' bot cohort, seeding each bot's account into `engine`
/// (home = `series`) with a random log-uniform bankroll, a name drawn
/// from `names`, and an archetype drawn from the weighted distribution
/// with its parameters scaled to the series' round length. `count` is
/// capped at `names.len()` so every bot gets a unique handle; callers
/// hand each series a disjoint slice of names. Returns the constructed
/// [`BotState`]s in the order they were generated.
pub fn build_population(
    engine: &mut Exchange,
    names: &[String],
    count: usize,
    rng_seed: u64,
    series: SeriesId,
) -> Vec<BotState> {
    let mut rng = seeded_rng(rng_seed);
    let actual = count.min(names.len());

    let mut shuffled: Vec<String> = names.to_vec();
    shuffle_names(&mut shuffled, &mut rng);

    let mut population = Vec::with_capacity(actual);
    for name in shuffled.into_iter().take(actual) {
        let archetype = pick_archetype(&mut rng);
        let params: BotParams =
            params_for(archetype, &mut rng).scaled_for(series.duration_secs());
        let bankroll = random_bankroll(&mut rng);
        let address = random_bot_address(&mut rng, series);
        let color = Some(pick_color(&mut rng));
        let emoji = Some(pick_emoji(&mut rng));
        engine.seed_bot_account_in(address, bankroll, name, color, emoji, series);
        population.push(BotState::new(address, archetype, params, bankroll, series));
    }
    population
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Engine;
    use crate::types::Tenths;

    #[test]
    fn two_cohorts_have_disjoint_names_and_addresses() {
        use crate::series::SERIES;
        let mut x = Exchange::new(Tenths(1_000_000), SERIES);
        let names: Vec<String> = (0..40).map(|i| format!("bot_{i:03}")).collect();
        let (first, second) = names.split_at(25);
        let a = build_population(&mut x, first, 25, 7, SeriesId(0));
        let b = build_population(&mut x, second, 15, 7 ^ (1u64 << 48), SeriesId(1));
        assert_eq!(a.len(), 25);
        assert_eq!(b.len(), 15);
        let mut addrs = std::collections::HashSet::new();
        let mut handles = std::collections::HashSet::new();
        for bot in a.iter().chain(b.iter()) {
            assert!(addrs.insert(bot.address), "duplicate address across cohorts");
            let acct = x.account(&bot.address).unwrap();
            assert!(handles.insert(acct.username.clone().unwrap()), "duplicate name");
            assert_eq!(acct.home_series, Some(bot.series));
            assert_eq!(bot.address.0[2], bot.series.0);
        }
        // 15m takers carry the scaled (larger) threshold.
        let taker_15m = b
            .iter()
            .find(|bot| bot.params.momentum_threshold_bps > 0)
            .expect("some taker in 15 bots");
        assert!(taker_15m.params.momentum_threshold_bps >= 8);
    }

    /// Drive one bot per series through a real decision against a
    /// counterparty in each series, then settle the series one at a time:
    /// only the settled series' bot goes back to its bankroll.
    #[test]
    fn bot_bankroll_resets_only_when_home_series_settles() {
        use crate::engine::TradeSource;
        use crate::series::SERIES;
        use crate::ticks::PriceTick;
        use crate::types::{Address, Qty, Side};
        use rand::SeedableRng;
        use rand::rngs::StdRng;

        let mut x = Exchange::new(Tenths(1_000_000), SERIES);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        x.open_first_market(SeriesId(0), 100_000.0, now, 60);
        x.open_first_market(SeriesId(1), 100_000.0, now, 900);
        // A counterparty per series quoting both sides so takers can hit.
        for s in [SeriesId(0), SeriesId(1)] {
            let counter = Address([0xC0 + s.0; 20]);
            x.seed_bot_account_in(counter, Tenths(500_000_000), format!("c{}", s.0), None, None, s);
            for t in [400u16, 450] {
                x.place_limit(s, counter, Side::Bid, PriceTick::from_tenths(t).unwrap(), Qty(500), TradeSource::Bot).unwrap();
            }
            for t in [550u16, 600] {
                x.place_limit(s, counter, Side::Ask, PriceTick::from_tenths(t).unwrap(), Qty(500), TradeSource::Bot).unwrap();
            }
        }
        let names: Vec<String> = (0..2).map(|i| format!("t{i}")).collect();
        // Force a momentum taker in each cohort by scanning seeds.
        let mut bots = Vec::new();
        for s in [SeriesId(0), SeriesId(1)] {
            let mut seed = 1u64;
            let bot = loop {
                let mut cohort = build_population(&mut x, &names[s.0 as usize..s.0 as usize + 1], 1, seed, s);
                let bot = cohort.remove(0);
                if bot.params.momentum_threshold_bps > 0 {
                    break bot;
                }
                // Undo the seeded account so the next draw gets a clean address.
                seed += 1;
            };
            bots.push(bot);
        }
        let mut rng = StdRng::seed_from_u64(9);
        for bot in bots.iter_mut() {
            // 5 % up: far beyond any threshold; velocity huge to force a cross.
            let changed = crate::bots::traders::decision::decide_and_act(
                &mut x.ctx(bot.series), bot, &crate::pricing::mark::SpotMark(105_000.0, 500), &mut rng,
            );
            assert!(changed, "taker in {} should have crossed", bot.series);
            assert_ne!(x.account(&bot.address).unwrap().usdc_balance, bot.bankroll);
        }
        x.debug_assert_invariants();

        x.resolve_and_open_next(SeriesId(0), 105_000.0, 105_000.0, now + 60, 60);
        assert_eq!(x.account(&bots[0].address).unwrap().usdc_balance, bots[0].bankroll);
        assert_ne!(x.account(&bots[1].address).unwrap().usdc_balance, bots[1].bankroll);
        assert!(!x.position(SeriesId(1), &bots[1].address).is_flat());

        x.resolve_and_open_next(SeriesId(1), 105_000.0, 105_000.0, now + 900, 900);
        assert_eq!(x.account(&bots[1].address).unwrap().usdc_balance, bots[1].bankroll);
        x.debug_assert_invariants();
    }

    #[test]
    fn build_population_seeds_accounts_in_engine() {
        let mut engine = Engine::new(Tenths(1_000_000));
        let names: Vec<String> = (0..100).map(|i| format!("bot_{i:03}")).collect();
        let pop = build_population(engine.exchange_mut(), &names, 50, 0xDEAD_BEEF, SeriesId(0));

        assert_eq!(pop.len(), 50);

        // Every bot's address must point to an account in the engine with
        // the bankroll and username that came back in the BotState.
        let mut seen_addrs = std::collections::HashSet::new();
        let mut seen_names = std::collections::HashSet::new();
        for bot in &pop {
            assert!(seen_addrs.insert(bot.address), "duplicate address");
            let acct = engine.account(&bot.address).expect("account seeded");
            assert_eq!(acct.usdc_balance, bot.bankroll);
            let name = acct.username.as_deref().expect("username set");
            assert!(seen_names.insert(name.to_string()), "duplicate name");
            assert!(acct.color.is_some());
            assert!(acct.emoji.is_some());
        }
    }

    #[test]
    fn build_population_caps_at_names_length() {
        let mut engine = Engine::new(Tenths(1_000_000));
        let names: Vec<String> = (0..10).map(|i| format!("n{i}")).collect();
        let pop = build_population(engine.exchange_mut(), &names, 25, 1, SeriesId(0));
        // Asked for 25, only 10 names available.
        assert_eq!(pop.len(), 10);
    }

    #[test]
    fn build_population_is_deterministic_for_same_seed() {
        let names: Vec<String> = (0..30).map(|i| format!("n{i}")).collect();
        let mut e1 = Engine::new(Tenths(1_000_000));
        let mut e2 = Engine::new(Tenths(1_000_000));
        let p1 = build_population(e1.exchange_mut(), &names, 20, 12345, SeriesId(0));
        let p2 = build_population(e2.exchange_mut(), &names, 20, 12345, SeriesId(0));
        let a1: Vec<_> = p1.iter().map(|b| b.address).collect();
        let a2: Vec<_> = p2.iter().map(|b| b.address).collect();
        assert_eq!(a1, a2);
    }
}
