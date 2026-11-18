//! Bot data model: archetype enum, per-bot parameter band, and the
//! per-task mutable state.
//!
//! These types are the schema both the population builder and the
//! decision loop read against. Each archetype carries an opinion about
//! "where on the book should this bot live" (passive vs. taker) and a
//! parameter band that the population builder samples from to produce a
//! heterogeneous cohort.

use rand::Rng;

use crate::types::{Address, OrderId, Tenths, SeriesId};

// ─────────────────────────────── Archetypes ───────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Archetype {
    /// Rests both sides 1–3¢ off fair value, refreshes frequently.
    PassiveNear,
    /// Rests both sides 5–15¢ off fair value, slower cadence.
    PassiveMid,
    /// Parks size at the extremes (≤15¢ / ≥85¢) as standing interest.
    PassiveDeep,
    /// Crosses the spread on strong directional moves.
    MomentumTaker,
    /// Fades strong moves with smaller size.
    Contrarian,
    /// Tiny random orders, random timing — keeps the tape from idling.
    Noise,
}

impl Archetype {
    /// Population weights — sum to 100 for readability. Tilted toward
    /// crossing flow (37 % takers + contrarians + noise) so the visible
    /// price actually moves on a BTC drift instead of just FV-drifting
    /// behind a thick passive wall. The passive layer is still the
    /// majority (63 %) to keep both sides quoted.
    pub const ALL_WITH_WEIGHTS: &'static [(Archetype, u32)] = &[
        (Archetype::PassiveNear, 30),
        (Archetype::PassiveMid, 18),
        (Archetype::PassiveDeep, 15),
        (Archetype::MomentumTaker, 20),
        (Archetype::Contrarian, 10),
        (Archetype::Noise, 7),
    ];
}

/// Weighted pick from [`Archetype::ALL_WITH_WEIGHTS`].
pub fn pick_archetype<R: Rng + ?Sized>(rng: &mut R) -> Archetype {
    let total: u32 = Archetype::ALL_WITH_WEIGHTS
        .iter()
        .map(|(_, w)| *w)
        .sum();
    let mut pick = rng.random_range(0..total);
    for (a, w) in Archetype::ALL_WITH_WEIGHTS {
        if pick < *w {
            return *a;
        }
        pick -= *w;
    }
    unreachable!("weights sum to total, pick always lands");
}

// ─────────────────────────────── Parameters ───────────────────────────────

/// Per-bot behavioral knobs. Drawn once at population build from the
/// archetype's parameter band so every bot in a cohort acts a bit
/// differently — preventing herding without making behavior fully random.
#[derive(Debug, Clone, Copy)]
pub struct BotParams {
    /// Distance from fair value at which passive bots anchor their resting
    /// orders, in tenths (1 tenth = 0.1¢). Unused by takers/noise.
    pub anchor_tenths: u16,
    /// Typical order size (contracts). Scaled by bankroll at action time.
    pub base_qty: u32,
    /// Maximum order size for this bot — clamped at action time so even a
    /// random-size jitter cannot exceed it.
    pub max_qty: u32,
    /// How much fair value must drift (in tenths) before a passive bot
    /// cancels and reprices. Larger ⇒ stickier book.
    pub reaction_threshold_tenths: u16,
    /// Minimum wait between actions, in milliseconds.
    pub reprice_interval_min_ms: u64,
    /// Maximum wait between actions, in milliseconds.
    pub reprice_interval_max_ms: u64,
    /// For takers/contrarians: BTC must move at least this many basis
    /// points relative to strike before a cross fires.
    pub momentum_threshold_bps: i32,
    /// Per-bot noise on the perceived fair value (in tenths). Drawn fresh
    /// on each decision step so the bot disagrees with consensus a bit.
    pub perception_noise_tenths: i16,
}

impl BotParams {
    /// Adapt a parameter draw (banded for a 60 s round) to a round of
    /// `duration_secs`. Fair value already scales with time remaining,
    /// but taker thresholds are absolute basis points vs. the strike: BTC
    /// drifts further over a longer round, so an unscaled 2–10 bps trigger
    /// would fire every few hundred ms for fifteen minutes. Thresholds
    /// grow with √duration (diffusion); cadences and sizes are unchanged.
    pub fn scaled_for(self, duration_secs: u64) -> BotParams {
        let factor = (duration_secs as f64 / 60.0).sqrt();
        if self.momentum_threshold_bps == 0 || factor <= 1.0 {
            return self;
        }
        BotParams {
            momentum_threshold_bps: ((self.momentum_threshold_bps as f64) * factor).round() as i32,
            ..self
        }
    }
}

/// Parameter band for a given archetype. Draws are independent so two
/// PassiveNear bots will still differ on anchor / cadence / size.
pub fn params_for<R: Rng + ?Sized>(archetype: Archetype, rng: &mut R) -> BotParams {
    match archetype {
        // Tight passive quotes — anchored 0.3–1.2¢ off FV and re-priced
        // sub-second. Pulls the visible best-bid/ask very close to FV so
        // the displayed price tracks BTC drift in near-real-time.
        Archetype::PassiveNear => BotParams {
            anchor_tenths: rng.random_range(3..=12),
            base_qty: rng.random_range(25..=75),
            max_qty: rng.random_range(100..=200),
            reaction_threshold_tenths: rng.random_range(1..=4),
            reprice_interval_min_ms: 400,
            reprice_interval_max_ms: 1_500,
            momentum_threshold_bps: 0,
            perception_noise_tenths: rng.random_range(-3..=3),
        },
        Archetype::PassiveMid => BotParams {
            anchor_tenths: rng.random_range(20..=80),
            base_qty: rng.random_range(25..=100),
            max_qty: rng.random_range(125..=300),
            reaction_threshold_tenths: rng.random_range(5..=15),
            reprice_interval_min_ms: 2_000,
            reprice_interval_max_ms: 7_000,
            momentum_threshold_bps: 0,
            perception_noise_tenths: rng.random_range(-10..=10),
        },
        Archetype::PassiveDeep => BotParams {
            // 30–45¢ off fair value: orders end up near the 0–15 / 85–100
            // tails when FV is mid-book.
            anchor_tenths: rng.random_range(300..=450),
            base_qty: rng.random_range(50..=200),
            max_qty: rng.random_range(250..=750),
            reaction_threshold_tenths: rng.random_range(80..=200),
            reprice_interval_min_ms: 30_000,
            reprice_interval_max_ms: 90_000,
            momentum_threshold_bps: 0,
            perception_noise_tenths: rng.random_range(-30..=30),
        },
        // Hair-trigger takers — 2–10 bps from strike fires a cross, and
        // each cross is 2–4× the previous size band. Combined with the
        // velocity surge in decision, this is what flips the book on a
        // sudden BTC move.
        Archetype::MomentumTaker => BotParams {
            anchor_tenths: 0,
            base_qty: rng.random_range(50..=200),
            max_qty: rng.random_range(300..=1000),
            reaction_threshold_tenths: 0,
            reprice_interval_min_ms: 300,
            reprice_interval_max_ms: 1_500,
            momentum_threshold_bps: rng.random_range(2..=10),
            perception_noise_tenths: 0,
        },
        Archetype::Contrarian => BotParams {
            anchor_tenths: 0,
            base_qty: rng.random_range(25..=100),
            max_qty: rng.random_range(125..=400),
            reaction_threshold_tenths: 0,
            reprice_interval_min_ms: 1_000,
            reprice_interval_max_ms: 4_000,
            momentum_threshold_bps: rng.random_range(8..=25),
            perception_noise_tenths: 0,
        },
        Archetype::Noise => BotParams {
            anchor_tenths: rng.random_range(20..=400),
            base_qty: rng.random_range(5..=20),
            max_qty: rng.random_range(10..=40),
            reaction_threshold_tenths: 0,
            reprice_interval_min_ms: 4_000,
            reprice_interval_max_ms: 25_000,
            momentum_threshold_bps: 0,
            perception_noise_tenths: rng.random_range(-50..=50),
        },
    }
}

// ─────────────────────────────── Bot state ───────────────────────────────

/// Long-lived per-bot state held by the bot's tokio task. Tracks resting
/// order ids so they can be cancelled before re-pricing.
#[derive(Debug)]
pub struct BotState {
    pub address: Address,
    /// The one series this bot trades (its account's home series).
    pub series: SeriesId,
    pub archetype: Archetype,
    pub params: BotParams,
    pub bankroll: Tenths,
    /// Order id of the bot's resting bid, if any.
    pub resting_bid: Option<OrderId>,
    /// Order id of the bot's resting ask, if any.
    pub resting_ask: Option<OrderId>,
    /// Last fair value the bot observed and acted on (tenths). Used by
    /// passive bots to decide whether the book has drifted enough to merit
    /// a reprice.
    pub last_fv_seen: Option<u16>,
    /// Market id the bot's tracked orders belong to. A change here means
    /// the engine already cancelled them at resolution.
    pub last_market_id: Option<u32>,
}

impl BotState {
    pub fn new(
        address: Address,
        archetype: Archetype,
        params: BotParams,
        bankroll: Tenths,
        series: SeriesId,
    ) -> Self {
        Self {
            address,
            series,
            archetype,
            params,
            bankroll,
            resting_bid: None,
            resting_ask: None,
            last_fv_seen: None,
            last_market_id: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;
    use rand::rngs::StdRng;

    #[test]
    fn archetype_weights_sum_to_one_hundred() {
        let total: u32 = Archetype::ALL_WITH_WEIGHTS.iter().map(|(_, w)| *w).sum();
        assert_eq!(total, 100);
    }

    #[test]
    fn pick_archetype_distribution_within_tolerance() {
        let mut rng = StdRng::seed_from_u64(0xC0FFEE);
        let n = 10_000;
        let mut counts = std::collections::HashMap::new();
        for _ in 0..n {
            *counts.entry(pick_archetype(&mut rng)).or_insert(0_u32) += 1;
        }
        for (arch, weight) in Archetype::ALL_WITH_WEIGHTS {
            let observed = *counts.get(arch).unwrap_or(&0) as f64 / n as f64;
            let expected = *weight as f64 / 100.0;
            let drift = (observed - expected).abs();
            assert!(
                drift < 0.02,
                "{:?}: observed {:.3}, expected {:.3}",
                arch,
                observed,
                expected,
            );
        }
    }

    #[test]
    fn params_for_passive_near_lives_inside_band() {
        let mut rng = StdRng::seed_from_u64(11);
        for _ in 0..200 {
            let p = params_for(Archetype::PassiveNear, &mut rng);
            assert!(p.anchor_tenths >= 3 && p.anchor_tenths <= 12);
            assert!(p.base_qty >= 25 && p.base_qty <= 75);
            assert!(p.reprice_interval_min_ms <= p.reprice_interval_max_ms);
        }
    }

    #[test]
    fn params_for_taker_has_momentum_threshold() {
        let mut rng = StdRng::seed_from_u64(12);
        for _ in 0..100 {
            let p = params_for(Archetype::MomentumTaker, &mut rng);
            assert!(p.momentum_threshold_bps >= 2 && p.momentum_threshold_bps <= 10);
        }
    }

    #[test]
    fn scaled_for_grows_taker_threshold_with_sqrt_duration() {
        let mut rng = StdRng::seed_from_u64(14);
        let p = params_for(Archetype::MomentumTaker, &mut rng);
        let s = p.scaled_for(900);
        // √15 ≈ 3.87
        let expected = ((p.momentum_threshold_bps as f64) * 3.872_983).round() as i32;
        assert_eq!(s.momentum_threshold_bps, expected);
        assert_eq!(s.base_qty, p.base_qty);
        assert_eq!(s.reprice_interval_min_ms, p.reprice_interval_min_ms);
        // Passive bots have no threshold and are untouched; 60 s is identity.
        let q = params_for(Archetype::PassiveNear, &mut rng);
        assert_eq!(q.scaled_for(900).momentum_threshold_bps, 0);
        assert_eq!(p.scaled_for(60).momentum_threshold_bps, p.momentum_threshold_bps);
    }

    #[test]
    fn params_for_passive_deep_anchors_far_from_fv() {
        let mut rng = StdRng::seed_from_u64(13);
        for _ in 0..50 {
            let p = params_for(Archetype::PassiveDeep, &mut rng);
            assert!(p.anchor_tenths >= 300);
        }
    }
}
