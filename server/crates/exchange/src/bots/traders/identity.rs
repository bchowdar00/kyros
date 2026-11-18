//! Visual / financial identity for trader bots: bankroll, address,
//! display name, colour, emoji. Pure helpers — no engine access — so the
//! population builder can compose these in whatever order it wants.

use std::path::Path;

use rand::Rng;
use rand::SeedableRng;
use rand::rngs::StdRng;
use rand::seq::SliceRandom;

use crate::types::{Address, SeriesId, Tenths};

// ────────────────────────── Bankroll & address gen ────────────────────────

const BANKROLL_MIN_TENTHS: i64 = 5_000_000; // $5,000
const BANKROLL_MAX_TENTHS: i64 = 100_000_000; // $100,000

/// Log-uniform bankroll between $5k and $100k. Log-uniform so $100k whales
/// are genuinely rare; arithmetic-uniform would give a fat upper tail.
pub fn random_bankroll<R: Rng + ?Sized>(rng: &mut R) -> Tenths {
    let lo = (BANKROLL_MIN_TENTHS as f64).ln();
    let hi = (BANKROLL_MAX_TENTHS as f64).ln();
    let v = (rng.random::<f64>() * (hi - lo) + lo).exp();
    Tenths(v.round().clamp(BANKROLL_MIN_TENTHS as f64, BANKROLL_MAX_TENTHS as f64) as i64)
}

/// Generate a fresh random 20-byte address. The high bytes are pinned to
/// `0xb0 0x07 <series>` so trader-bot addresses are visually identifiable
/// in logs, carry their cohort, and stay distinct from the market-maker's
/// 0xbb01 prefix.
pub fn random_bot_address<R: Rng + ?Sized>(rng: &mut R, series: SeriesId) -> Address {
    let mut bytes = [0u8; 20];
    rng.fill(&mut bytes[..]);
    bytes[0] = 0xb0;
    bytes[1] = 0x7;
    bytes[2] = series.0;
    Address(bytes)
}

// ──────────────────────────────── Names ───────────────────────────────────

/// Load usernames from a newline-separated file. Blank lines and lines
/// starting with `#` are ignored.
pub fn load_names(path: &Path) -> std::io::Result<Vec<String>> {
    let raw = std::fs::read_to_string(path)?;
    Ok(parse_names(&raw))
}

/// Parse a names list: one per line, blanks and `#` comments skipped.
pub fn parse_names(raw: &str) -> Vec<String> {
    raw.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_owned)
        .collect()
}

/// Shuffle a names vec in place using the given rng. Wrapping helper so
/// callers don't need a direct `rand::seq` dependency.
pub fn shuffle_names<R: Rng + ?Sized>(names: &mut [String], rng: &mut R) {
    names.shuffle(rng);
}

// ──────────────────────────────── Avatar ──────────────────────────────────

const BOT_COLORS: &[&str] = &[
    "#f97316", "#22c55e", "#3b82f6", "#a855f7", "#ec4899", "#14b8a6",
    "#eab308", "#ef4444", "#8b5cf6", "#06b6d4", "#84cc16", "#f43f5e",
];

const BOT_EMOJIS: &[&str] = &[
    "🦊", "🐺", "🦁", "🐯", "🐻", "🐼", "🐨", "🐸", "🐵", "🦄",
    "🐢", "🐙", "🦀", "🦋", "🐝", "🦔", "🦉", "🦇", "🐠", "🐬",
];

pub fn pick_color<R: Rng + ?Sized>(rng: &mut R) -> String {
    BOT_COLORS[rng.random_range(0..BOT_COLORS.len())].to_string()
}

pub fn pick_emoji<R: Rng + ?Sized>(rng: &mut R) -> String {
    BOT_EMOJIS[rng.random_range(0..BOT_EMOJIS.len())].to_string()
}

// ──────────────────────────────── RNG ─────────────────────────────────────

/// Seeded RNG for reproducible bot personality across server restarts.
/// The seed itself can come from env or be fixed for debugging.
pub fn seeded_rng(seed: u64) -> StdRng {
    StdRng::seed_from_u64(seed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;
    use rand::rngs::StdRng;

    #[test]
    fn random_bankroll_stays_in_range() {
        let mut rng = StdRng::seed_from_u64(42);
        for _ in 0..1_000 {
            let b = random_bankroll(&mut rng).0;
            assert!((BANKROLL_MIN_TENTHS..=BANKROLL_MAX_TENTHS).contains(&b), "got {}", b);
        }
    }

    #[test]
    fn random_bankroll_log_uniform_skews_toward_small() {
        // Sanity check: a true log-uniform on [$5k, $100k] should have
        // *more* mass below $25k than above. (Median ≈ $22k.)
        let mut rng = StdRng::seed_from_u64(7);
        let n = 5_000;
        let mut below_25k = 0;
        for _ in 0..n {
            if random_bankroll(&mut rng).0 < 25_000_000 {
                below_25k += 1;
            }
        }
        let frac = below_25k as f64 / n as f64;
        assert!(frac > 0.5, "log-uniform should put >50 % below $25k, got {}", frac);
    }

    #[test]
    fn random_bot_address_has_stable_prefix() {
        let mut rng = StdRng::seed_from_u64(1);
        for _ in 0..100 {
            let a = random_bot_address(&mut rng, SeriesId(0));
            assert_eq!(a.0[0], 0xb0);
            assert_eq!(a.0[1], 0x07);
            assert_eq!(a.0[2], 0);
            let b = random_bot_address(&mut rng, SeriesId(1));
            assert_eq!((b.0[0], b.0[1], b.0[2]), (0xb0, 0x07, 1));
        }
    }

    #[test]
    fn random_bot_addresses_are_unique() {
        let mut rng = StdRng::seed_from_u64(99);
        let mut seen = std::collections::HashSet::new();
        for _ in 0..1_000 {
            assert!(seen.insert(random_bot_address(&mut rng, SeriesId(0))));
        }
    }
}
