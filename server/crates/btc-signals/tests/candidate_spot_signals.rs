//! Candidate spot-microstructure computations.
//!
//! Every ready assertion carries its arithmetic in a comment so the exact
//! expected value is auditable. Divisors are kept to terminating decimals
//! (factors of 2 and 5 only) so `rust_decimal` results are exact.

use std::collections::BTreeSet;
use std::path::Path;

use btc_signals::SPOT_FEATURE_VERSION;
use btc_signals::candidate_spot_signals::{
    AggressorSide, BookResilienceCandidate, BookSide, BookTop, DEFAULT_DEPLETE_FRACTION,
    DEFAULT_KYLE_WINDOW_TRADES, DEFAULT_REALIZED_HORIZON_MS, DEFAULT_RECOVER_FRACTION,
    DEFAULT_SWEEP_MIN_LEVELS, DepletionSide, EffectiveSpreadCandidate, EffectiveSpreadOutcome,
    KyleLambdaDriftCandidate, KyleOutcome, RealizedPart, RealizedState, ResilienceState, Sweep,
    SweepBatchTrade, SweepDetectionCandidate, SweepDetectionState, SweepDirection, SweepReading,
};
use rust_decimal::Decimal;

/// The adopted spot venues that publish each microstructure measurement. The
/// four measurements are same-venue formulas, so each is published once per
/// venue and never as a blended `btc.external.*` identity (ADR 0024).
const ADOPTED_VENUES: [&str; 2] = ["coinbase", "kraken"];

/// The adopted V1 definition for one measurement under one venue namespace.
fn adopted_definition(venue: &str, measurement: &str) -> serde_json::Value {
    let file = format!("btc.{venue}.{measurement}@1.0.0.json");
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../definitions/btc/signals")
        .join(&file);
    let bytes = std::fs::read(&path).unwrap_or_else(|error| panic!("read {file}: {error}"));
    serde_json::from_slice(&bytes).unwrap_or_else(|error| panic!("parse {file}: {error}"))
}

fn ns(seconds: u64) -> u64 {
    seconds * 1_000_000_000
}

fn dec(mantissa: i64, scale: u32) -> Decimal {
    Decimal::new(mantissa, scale)
}

// --- 1. effective_spread / realized_spread ---------------------------------

#[test]
fn effective_and_realized_spread_match_hand_computed_values() {
    let mut candidate = EffectiveSpreadCandidate::with_default_horizon(); // 5000ms

    // A midpoint at/before the trade must exist for a ready effective spread.
    assert!(
        candidate
            .observe_book(ns(1), Decimal::from(100))
            .expect("book admits")
            .is_empty()
    );

    // effective = 2 * (+1) * (100.05 - 100) / 100 * 1e4 bps
    //           = 2 * (0.05/100) * 1e4 = 2 * 5 = 10 bps.
    let reading = candidate
        .observe_trade(ns(1), dec(10005, 2), AggressorSide::Buy)
        .expect("trade admits");
    assert_eq!(
        reading.effective,
        EffectiveSpreadOutcome::Ready {
            effective_spread_bps: Decimal::from(10),
            midpoint: Decimal::from(100),
            trade_price: dec(10005, 2),
            aggressor: AggressorSide::Buy,
            realized: RealizedPart::awaiting(),
        }
    );
    assert!(reading.resolved.is_empty());
    assert_eq!(candidate.pending_len(), 1);

    // A midpoint inside the 5s horizon (event 2s <= horizon end 6s) becomes the
    // prevailing-at-horizon midpoint; a later out-of-horizon midpoint does not.
    assert!(
        candidate
            .observe_book(ns(2), Decimal::from(125))
            .expect("in-horizon book admits")
            .is_empty()
    );

    // First event strictly past the horizon (8s > 6s) resolves the trade using
    // the prevailing mid at the horizon (125).
    // realized = 2 * (+1) * (100.05 - 125) / 125 * 1e4
    //          = 2 * (-24.95/125) * 1e4 = 2 * (-0.1996 * 1e4) = 2 * -1996 = -3992 bps.
    let resolved = candidate
        .observe_book(ns(8), Decimal::from(130))
        .expect("post-horizon book admits");
    assert_eq!(resolved.len(), 1);
    let realized = &resolved[0];
    assert!(realized.realized.realized_present);
    assert_eq!(realized.realized.realized_state, RealizedState::Ready);
    assert_eq!(realized.trade_time_ns, ns(1));
    assert_eq!(realized.aggressor, AggressorSide::Buy);
    assert_eq!(realized.realized.midpoint_at_horizon, Decimal::from(125));
    assert_eq!(realized.realized.realized_spread_bps, Decimal::from(-3992));
    assert_eq!(candidate.pending_len(), 0);
}

#[test]
fn realized_spread_uses_the_pre_trade_mid_when_no_midpoint_arrives_within_the_horizon() {
    let mut candidate = EffectiveSpreadCandidate::new(5000);
    candidate.observe_book(ns(1), Decimal::from(100)).unwrap();
    candidate
        .observe_trade(ns(1), dec(10005, 2), AggressorSide::Buy)
        .unwrap();

    // No midpoint arrives in (trade, horizon]; the freshest at-or-before-horizon
    // midpoint is the pre-trade mid (100) — correct TAQ, mid unchanged through
    // the horizon. realized therefore equals the effective spread (10 bps).
    let resolved = candidate
        .observe_book(ns(8), Decimal::from(130))
        .expect("post-horizon book admits");
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].realized.realized_state, RealizedState::Ready);
    assert_eq!(resolved[0].realized.midpoint_at_horizon, Decimal::from(100));
    assert_eq!(resolved[0].realized.realized_spread_bps, Decimal::from(10));
}

#[test]
fn a_later_midpoint_does_not_retroactively_change_a_resolved_realized_spread() {
    let mut candidate = EffectiveSpreadCandidate::new(5000);
    candidate.observe_book(ns(1), Decimal::from(100)).unwrap();
    candidate
        .observe_trade(ns(1), dec(10005, 2), AggressorSide::Buy)
        .unwrap();
    candidate.observe_book(ns(3), Decimal::from(120)).unwrap(); // in-horizon
    candidate.observe_book(ns(4), Decimal::from(125)).unwrap(); // in-horizon, freshest <= 6s

    // Trigger past the horizon: resolves using the freshest at-or-before-horizon
    // mid (125), NOT the 999 that triggered it. realized = -3992 bps (as above).
    let resolved = candidate
        .observe_book(ns(8), Decimal::from(999))
        .expect("post-horizon book admits");
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].realized.midpoint_at_horizon, Decimal::from(125));
    assert_eq!(
        resolved[0].realized.realized_spread_bps,
        Decimal::from(-3992)
    );

    // A midpoint after resolution emits nothing and cannot change the value.
    assert!(
        candidate
            .observe_book(ns(9), Decimal::from(111))
            .expect("later book admits")
            .is_empty()
    );
}

#[test]
fn effective_spread_is_not_ready_before_any_book_and_enqueues_no_realized() {
    let mut candidate = EffectiveSpreadCandidate::new(5000);

    // Trade before any book midpoint: a defined not-ready effective spread, and
    // no realized measurement is enqueued (spread is undefined without a mid).
    let reading = candidate
        .observe_trade(ns(1), Decimal::from(100), AggressorSide::Sell)
        .expect("trade admits");
    assert_eq!(
        reading.effective,
        EffectiveSpreadOutcome::NotReady {
            reason: "no_prior_midpoint",
        }
    );
    assert!(reading.resolved.is_empty());
    assert_eq!(candidate.pending_len(), 0);

    // A later midpoint (past the horizon) resolves nothing.
    assert!(
        candidate
            .observe_book(ns(8), Decimal::from(100))
            .expect("book admits")
            .is_empty()
    );
}

#[test]
fn seller_aggressor_effective_spread_is_positive_below_the_mid() {
    let mut candidate = EffectiveSpreadCandidate::new(5000);
    candidate.observe_book(ns(1), Decimal::from(100)).unwrap();

    // A seller-aggressor lifting below the mid pays a positive effective spread:
    // effective = 2 * (-1) * (99.95 - 100) / 100 * 1e4
    //           = 2 * (-1) * (-0.05/100) * 1e4 = 2 * (-1) * (-5) = +10 bps.
    let reading = candidate
        .observe_trade(ns(1), dec(9995, 2), AggressorSide::Sell)
        .expect("trade admits");
    assert_eq!(
        reading.effective,
        EffectiveSpreadOutcome::Ready {
            effective_spread_bps: Decimal::from(10),
            midpoint: Decimal::from(100),
            trade_price: dec(9995, 2),
            aggressor: AggressorSide::Sell,
            realized: RealizedPart::awaiting(),
        }
    );
}

#[test]
fn a_trade_observation_past_the_horizon_resolves_a_pending_measurement() {
    let mut candidate = EffectiveSpreadCandidate::new(5000);
    candidate.observe_book(ns(1), Decimal::from(100)).unwrap();
    let first = candidate
        .observe_trade(ns(1), dec(10005, 2), AggressorSide::Buy)
        .expect("trade admits");
    assert!(matches!(
        first.effective,
        EffectiveSpreadOutcome::Ready { .. }
    ));
    assert!(first.resolved.is_empty());

    // A later trade (8s > horizon end 6s) — not a book — resolves the pending
    // measurement, using the frozen pre-trade mid (100). realized (buy, price
    // 100.05, mid 100) = 10 bps.
    let second = candidate
        .observe_trade(ns(8), Decimal::from(100), AggressorSide::Buy)
        .expect("trade admits");
    assert_eq!(second.resolved.len(), 1);
    assert_eq!(second.resolved[0].trade_time_ns, ns(1));
    assert_eq!(
        second.resolved[0].realized.midpoint_at_horizon,
        Decimal::from(100)
    );
    assert_eq!(
        second.resolved[0].realized.realized_spread_bps,
        Decimal::from(10)
    );
}

// --- 2. kyle_lambda_drift --------------------------------------------------

// Impact FOLLOWS each trade: every buy (sv = +1) is followed by a midpoint rise,
// so forward pairing must yield a positive lambda. Pairs (mid_change, sv):
// [(1,1),(1,1),(2,1),(2,1)] for N = 2.
fn run_kyle_forward_fixture() -> KyleOutcome {
    let mut candidate = KyleLambdaDriftCandidate::new(2);
    candidate.observe_book(ns(1), Decimal::from(100)).unwrap(); // baseline prevailing mid
    assert_eq!(
        candidate.observe_trade(ns(2), Decimal::from(1)).unwrap(),
        KyleOutcome::NotReady {
            reason: "awaiting_forward_midpoint",
        }
    );
    // mid rises 100 -> 101 AFTER the buy: forward change = +1, pair (1, +1).
    assert_eq!(
        candidate.observe_book(ns(3), Decimal::from(101)).unwrap(),
        KyleOutcome::NotReady {
            reason: "kyle_window_warming_up",
        }
    );
    candidate.observe_trade(ns(4), Decimal::from(1)).unwrap();
    candidate.observe_book(ns(5), Decimal::from(102)).unwrap(); // pair (1, +1)
    candidate.observe_trade(ns(6), Decimal::from(1)).unwrap();
    candidate.observe_book(ns(7), Decimal::from(104)).unwrap(); // pair (2, +1)
    candidate.observe_trade(ns(8), Decimal::from(1)).unwrap();
    candidate.observe_book(ns(9), Decimal::from(106)).unwrap() // pair (2, +1) -> ready
}

#[test]
fn kyle_lambda_drift_measures_forward_price_impact() {
    // current window (last 2) = [(2,1),(2,1)]: num = 2+2 = 4, den = 1+1 = 2 -> lambda = 2.
    // previous window (first 2) = [(1,1),(1,1)]: num = 1+1 = 2, den = 2 -> lambda = 1.
    // drift = 2 - 1 = 1. All positive: buys followed by rising mids -> positive impact.
    match run_kyle_forward_fixture() {
        KyleOutcome::Ready(ready) => {
            assert_eq!(ready.kyle_lambda, Decimal::from(2));
            assert_eq!(ready.kyle_lambda_previous, Decimal::from(1));
            assert_eq!(ready.lambda_drift, Decimal::from(1));
            assert_eq!(ready.pair_count, 4);
        }
        other => panic!("expected ready kyle lambda, got {other:?}"),
    }
}

#[test]
fn kyle_trades_between_two_midpoint_updates_share_the_same_forward_change() {
    let mut candidate = KyleLambdaDriftCandidate::new(1); // N = 1 -> 2N = 2 pairs
    candidate.observe_book(ns(1), Decimal::from(100)).unwrap(); // baseline
    // Two buys before any midpoint update: both prevail at 100.
    candidate.observe_trade(ns(2), Decimal::from(1)).unwrap();
    candidate.observe_trade(ns(3), Decimal::from(2)).unwrap();
    // One update to 102: both pending trades pair with the SAME forward change 2.
    // pairs = [(2,1),(2,2)]. previous (first 1) = [(2,1)]: num = 2, den = 1 -> lambda = 2.
    // current (last 1) = [(2,2)]: num = 4, den = 4 -> lambda = 1. drift = 1 - 2 = -1.
    match candidate.observe_book(ns(4), Decimal::from(102)).unwrap() {
        KyleOutcome::Ready(ready) => {
            assert_eq!(ready.kyle_lambda, Decimal::from(1));
            assert_eq!(ready.kyle_lambda_previous, Decimal::from(2));
            assert_eq!(ready.lambda_drift, Decimal::from(-1));
            assert_eq!(ready.pair_count, 2);
        }
        other => panic!("expected ready kyle lambda, got {other:?}"),
    }
}

#[test]
fn kyle_lambda_reports_not_ready_on_zero_signed_volume_without_dividing() {
    let mut candidate = KyleLambdaDriftCandidate::new(1); // N = 1 -> 2N = 2 pairs
    candidate.observe_book(ns(1), Decimal::from(100)).unwrap();
    candidate.observe_trade(ns(2), Decimal::from(1)).unwrap();
    // pair 0: dm = 1, sv = +1 (warming; only one pair, 2N = 2 required).
    assert_eq!(
        candidate.observe_book(ns(3), Decimal::from(101)).unwrap(),
        KyleOutcome::NotReady {
            reason: "kyle_window_warming_up",
        }
    );
    candidate.observe_trade(ns(4), Decimal::ZERO).unwrap();
    // pair 1: dm = 1, sv = 0 -> current window sum(sv^2) = 0 -> not ready, never a divide-by-zero.
    assert_eq!(
        candidate.observe_book(ns(5), Decimal::from(102)).unwrap(),
        KyleOutcome::NotReady {
            reason: "degenerate_signed_volume",
        }
    );
}

#[test]
fn kyle_lambda_is_not_ready_before_a_book_baseline() {
    let mut candidate = KyleLambdaDriftCandidate::new(2);
    assert_eq!(
        candidate.observe_trade(ns(1), Decimal::from(1)).unwrap(),
        KyleOutcome::NotReady {
            reason: "book_baseline_warming_up",
        }
    );
}

#[test]
fn kyle_default_window_is_far_from_ready_after_one_pair() {
    let mut candidate = KyleLambdaDriftCandidate::with_default_window(); // N = 100
    candidate.observe_book(ns(1), Decimal::from(100)).unwrap();
    candidate.observe_trade(ns(2), Decimal::from(1)).unwrap();
    assert_eq!(
        candidate.observe_book(ns(3), Decimal::from(101)).unwrap(),
        KyleOutcome::NotReady {
            reason: "kyle_window_warming_up",
        }
    );
}

// --- 3. sweep_detection ----------------------------------------------------

#[test]
fn sweep_fires_at_k_distinct_levels_and_sums_all_consumed_size() {
    let mut candidate = SweepDetectionCandidate::with_default_min_levels(); // K = 3
    // Buy side spans 3 distinct prices; the duplicate at 100 still adds to size.
    let batch = [
        SweepBatchTrade {
            price: Decimal::from(100),
            size: Decimal::from(1),
            aggressor: AggressorSide::Buy,
        },
        SweepBatchTrade {
            price: Decimal::from(100),
            size: Decimal::from(2),
            aggressor: AggressorSide::Buy,
        },
        SweepBatchTrade {
            price: dec(10001, 2),
            size: Decimal::from(1),
            aggressor: AggressorSide::Buy,
        },
        SweepBatchTrade {
            price: dec(10002, 2),
            size: Decimal::from(1),
            aggressor: AggressorSide::Buy,
        },
    ];
    // distinct prices = {100, 100.01, 100.02} = 3 >= K; size = 1+2+1+1 = 5.
    let outcome = candidate
        .observe_batch(ns(1), &batch)
        .expect("batch admits");
    let expected = Sweep {
        direction: AggressorSide::Buy,
        swept_levels: 3,
        swept_size: Decimal::from(5),
    };
    assert_eq!(
        outcome,
        SweepReading {
            detection_state: SweepDetectionState::Sweep,
            direction: SweepDirection::Buy,
            swept_levels: 3,
            swept_size: Decimal::from(5),
        }
    );
    assert_eq!(candidate.latest(), Some(&expected));
}

#[test]
fn sweep_does_not_fire_at_exactly_k_minus_one_levels() {
    let mut candidate = SweepDetectionCandidate::with_default_min_levels(); // K = 3
    // Exactly K-1 = 2 distinct buy prices: no sweep, latest stays empty.
    let batch = [
        SweepBatchTrade {
            price: Decimal::from(100),
            size: Decimal::from(1),
            aggressor: AggressorSide::Buy,
        },
        SweepBatchTrade {
            price: dec(10001, 2),
            size: Decimal::from(1),
            aggressor: AggressorSide::Buy,
        },
    ];
    // Nothing has swept yet, so every state-dependent field is the explicit
    // sentinel rather than an absent value.
    assert_eq!(
        candidate
            .observe_batch(ns(1), &batch)
            .expect("batch admits"),
        SweepReading {
            detection_state: SweepDetectionState::Quiet,
            direction: SweepDirection::None,
            swept_levels: 0,
            swept_size: Decimal::ZERO,
        }
    );
    assert_eq!(candidate.latest(), None);
}

#[test]
fn sweep_both_sides_qualifying_picks_the_deeper_side_deterministically() {
    let mut candidate = SweepDetectionCandidate::with_default_min_levels();
    let batch = [
        // Buy: 3 distinct levels.
        SweepBatchTrade {
            price: Decimal::from(100),
            size: Decimal::from(1),
            aggressor: AggressorSide::Buy,
        },
        SweepBatchTrade {
            price: dec(10001, 2),
            size: Decimal::from(1),
            aggressor: AggressorSide::Buy,
        },
        SweepBatchTrade {
            price: dec(10002, 2),
            size: Decimal::from(1),
            aggressor: AggressorSide::Buy,
        },
        // Sell: 4 distinct levels -> deeper, so it wins.
        SweepBatchTrade {
            price: Decimal::from(100),
            size: Decimal::from(1),
            aggressor: AggressorSide::Sell,
        },
        SweepBatchTrade {
            price: dec(9999, 2),
            size: Decimal::from(1),
            aggressor: AggressorSide::Sell,
        },
        SweepBatchTrade {
            price: dec(9998, 2),
            size: Decimal::from(1),
            aggressor: AggressorSide::Sell,
        },
        SweepBatchTrade {
            price: dec(9997, 2),
            size: Decimal::from(1),
            aggressor: AggressorSide::Sell,
        },
    ];
    assert_eq!(
        candidate
            .observe_batch(ns(1), &batch)
            .expect("batch admits"),
        SweepReading {
            detection_state: SweepDetectionState::Sweep,
            direction: SweepDirection::Sell,
            swept_levels: 4,
            swept_size: Decimal::from(4),
        }
    );
}

#[test]
fn a_quiet_batch_republishes_the_retained_sweep_rather_than_dropping_the_fields() {
    let mut candidate = SweepDetectionCandidate::with_default_min_levels(); // K = 3
    let sweeping = [
        SweepBatchTrade {
            price: Decimal::from(100),
            size: Decimal::from(1),
            aggressor: AggressorSide::Buy,
        },
        SweepBatchTrade {
            price: dec(10001, 2),
            size: Decimal::from(1),
            aggressor: AggressorSide::Buy,
        },
        SweepBatchTrade {
            price: dec(10002, 2),
            size: Decimal::from(1),
            aggressor: AggressorSide::Buy,
        },
    ];
    candidate
        .observe_batch(ns(1), &sweeping)
        .expect("batch admits");

    // A later non-sweeping batch reports quiet but keeps every declared field
    // populated from the retained latest sweep.
    let quiet = candidate
        .observe_batch(ns(2), &sweeping[..1])
        .expect("batch admits");
    assert_eq!(
        quiet,
        SweepReading {
            detection_state: SweepDetectionState::Quiet,
            direction: SweepDirection::Buy,
            swept_levels: 3,
            swept_size: Decimal::from(3),
        }
    );
}

// --- 4. book_resilience ----------------------------------------------------

fn top(bid: i64, ask: i64) -> BookTop {
    BookTop::new(Decimal::from(bid), Decimal::from(ask))
}

#[test]
fn book_resilience_measures_recovery_after_a_depletion() {
    let mut candidate = BookResilienceCandidate::with_default_thresholds(); // 0.5 / 0.75
    // First update: no prior, quiet.
    assert_eq!(
        candidate.observe_book(ns(1), top(100, 100)).unwrap().state,
        ResilienceState::Quiet
    );
    // Bid drops 100 -> 40 (<= 50% of 100 = 50): depletion, pre = 100 at 2s.
    let depleted = candidate.observe_book(ns(2), top(40, 100)).unwrap();
    assert_eq!(depleted.state, ResilienceState::Recovering);
    assert_eq!(depleted.depletion_side, DepletionSide::Bid);
    assert_eq!(depleted.pre_depletion_size, Decimal::from(100));
    // No recovery has completed, so the flag is false and the value is the
    // explicit 0 sentinel rather than an absent field.
    assert!(!depleted.recovery_ms_present);
    assert_eq!(depleted.recovery_ms, 0);
    // Bid returns to 80 (>= 75% of 100 = 75): recovered at 3s.
    // recovery_ms = (3s - 2s) / 1e6 ns = 1000 ms.
    let recovered = candidate.observe_book(ns(3), top(80, 100)).unwrap();
    assert_eq!(recovered.state, ResilienceState::Recovered);
    assert!(recovered.recovery_ms_present);
    assert_eq!(recovered.recovery_ms, 1000);
    assert_eq!(recovered.depletion_side, DepletionSide::None);
    assert_eq!(recovered.pre_depletion_size, Decimal::ZERO);
}

#[test]
fn book_resilience_stays_recovering_when_liquidity_never_returns() {
    let mut candidate = BookResilienceCandidate::with_default_thresholds();
    candidate.observe_book(ns(1), top(100, 100)).unwrap();
    candidate.observe_book(ns(2), top(40, 100)).unwrap(); // deplete bid pre=100
    candidate.observe_book(ns(3), top(45, 100)).unwrap(); // 45 < 75: still recovering
    let unresolved = candidate.observe_book(ns(4), top(50, 100)).unwrap(); // 50 < 75
    assert_eq!(unresolved.state, ResilienceState::Recovering);
    assert!(!unresolved.recovery_ms_present);
    assert_eq!(unresolved.recovery_ms, 0);
    assert_eq!(unresolved.depletion_side, DepletionSide::Bid);
    assert_eq!(unresolved.pre_depletion_size, Decimal::from(100));
}

#[test]
fn book_resilience_abandons_the_first_measurement_on_a_second_depletion() {
    let mut candidate = BookResilienceCandidate::with_default_thresholds();
    candidate.observe_book(ns(1), top(100, 100)).unwrap();
    candidate.observe_book(ns(2), top(40, 100)).unwrap(); // deplete #1: pre=100 at 2s
    candidate.observe_book(ns(3), top(45, 100)).unwrap(); // recovering, no recovery
    // Second depletion during recovery: 45 -> 18 (<= 50% of 45 = 22.5). The rule
    // abandons the first, unresolved measurement (pre=100 at 2s) and tracks the
    // new one (pre=45 at 4s), so recovery is measured against the freshest
    // withdrawal.
    let retracked = candidate.observe_book(ns(4), top(18, 100)).unwrap();
    assert_eq!(retracked.state, ResilienceState::Recovering);
    assert_eq!(retracked.pre_depletion_size, Decimal::from(45));
    // Bid returns to 40 (>= 75% of 45 = 33.75): recovered at 5s.
    // recovery_ms = (5s - 4s)/1e6 = 1000 ms; measured from the second depletion,
    // NOT the abandoned first one (which would give 3000 ms).
    let recovered = candidate.observe_book(ns(5), top(40, 100)).unwrap();
    assert_eq!(recovered.state, ResilienceState::Recovered);
    assert_eq!(recovered.recovery_ms, 1000);
}

#[test]
fn book_resilience_treats_a_zero_current_size_as_maximal_depletion() {
    let mut candidate = BookResilienceCandidate::with_default_thresholds();
    candidate.observe_book(ns(1), top(100, 100)).unwrap();
    // Bid displayed size goes to zero against a positive baseline: maximal
    // depletion — a valid depletion event, not an error.
    let depleted = candidate.observe_book(ns(2), top(0, 100)).unwrap();
    assert_eq!(depleted.state, ResilienceState::Recovering);
    assert_eq!(depleted.depletion_side, DepletionSide::Bid);
    assert_eq!(depleted.pre_depletion_size, Decimal::from(100));
}

#[test]
fn book_resilience_recovers_after_a_to_zero_depletion() {
    let mut candidate = BookResilienceCandidate::with_default_thresholds();
    candidate.observe_book(ns(1), top(100, 100)).unwrap();
    // Bid drops to zero: maximal depletion, tracked pre=100 at 2s.
    let depleted = candidate.observe_book(ns(2), top(0, 100)).unwrap();
    assert_eq!(depleted.state, ResilienceState::Recovering);
    assert_eq!(depleted.pre_depletion_size, Decimal::from(100));
    // Bid returns to 90 one second later (>= 75% of 100 = 75): recovery is
    // measured against the fixed pre-depletion size, not the zero baseline.
    // recovery_ms = (3s - 2s)/1e6 = 1000 ms.
    let recovered = candidate.observe_book(ns(3), top(90, 100)).unwrap();
    assert_eq!(recovered.state, ResilienceState::Recovered);
    assert_eq!(recovered.recovery_ms, 1000);
    assert_eq!(recovered.depletion_side, DepletionSide::None);
}

#[test]
fn book_resilience_recovers_against_the_baseline_after_a_zero_observation() {
    let mut candidate = BookResilienceCandidate::with_default_thresholds();
    candidate.observe_book(ns(1), top(100, 100)).unwrap();
    candidate.observe_book(ns(2), top(40, 100)).unwrap(); // deplete bid pre=100 at 2s
    candidate.observe_book(ns(3), top(0, 100)).unwrap(); // deeper (maximal) depletion, pre=40 at 3s
    // The intervening bid baseline is zero, but recovery is measured against the
    // fixed pre-depletion size (40), not the baseline: 50 >= 75% of 40 = 30, so
    // this is a legitimate recovery, recovery_ms = (4s - 3s)/1e6 = 1000 ms.
    let recovered = candidate.observe_book(ns(4), top(50, 100)).unwrap();
    assert_eq!(recovered.state, ResilienceState::Recovered);
    assert_eq!(recovered.recovery_ms, 1000);
    assert_eq!(recovered.depletion_side, DepletionSide::None);
}

#[test]
fn book_resilience_rejects_degenerate_fractions() {
    assert!(BookResilienceCandidate::new(Decimal::ZERO, dec(75, 2)).is_err());
    assert!(BookResilienceCandidate::new(dec(5, 1), Decimal::ONE).is_err());
    assert!(BookResilienceCandidate::new(dec(-1, 1), dec(75, 2)).is_err());
    assert!(BookResilienceCandidate::new(dec(5, 1), dec(75, 2)).is_ok());
}

// --- drift guard: consts match the adopted definitions (S3) ----------------

#[test]
fn candidate_ids_version_and_parameter_defaults_match_the_adopted_definitions() {
    // Each code DEFAULT_* const is bound to its definition's stated default so
    // the echo can never rot: the const value is formatted into the exact
    // phrase the description must contain.
    let cases = [
        (
            "effective_spread",
            vec![format!(
                "realized_horizon_ms defaults to {DEFAULT_REALIZED_HORIZON_MS}"
            )],
        ),
        (
            "kyle_lambda_drift",
            vec![format!(
                "window_trades defaults to {DEFAULT_KYLE_WINDOW_TRADES}"
            )],
        ),
        (
            "sweep_detection",
            vec![format!("min_levels defaults to {DEFAULT_SWEEP_MIN_LEVELS}")],
        ),
        (
            "book_resilience",
            vec![
                format!("deplete_fraction defaults to {DEFAULT_DEPLETE_FRACTION}"),
                format!("recover_fraction defaults to {DEFAULT_RECOVER_FRACTION}"),
            ],
        ),
    ];
    for (measurement, expected_defaults) in cases {
        for venue in ADOPTED_VENUES {
            let value = adopted_definition(venue, measurement);
            let label = format!("btc.{venue}.{measurement}");
            assert_eq!(
                value["signal_id"],
                serde_json::json!(label),
                "{label} signal_id"
            );
            assert_eq!(
                value["version"],
                serde_json::json!(SPOT_FEATURE_VERSION),
                "{label} version"
            );
            let description = value["description"]
                .as_str()
                .unwrap_or_else(|| panic!("{label} description is a string"));
            for expected in &expected_defaults {
                assert!(
                    description.contains(expected.as_str()),
                    "{label} description must state the default: `{expected}`"
                );
            }
        }
    }
}

// --- determinism: same inputs in the same order -> identical outputs --------

#[test]
fn effective_spread_replays_identically() {
    let run = || {
        let mut candidate = EffectiveSpreadCandidate::new(5000);
        let mut out = Vec::new();
        out.push(format!(
            "{:?}",
            candidate.observe_book(ns(1), Decimal::from(100)).unwrap()
        ));
        out.push(format!(
            "{:?}",
            candidate
                .observe_trade(ns(1), dec(10005, 2), AggressorSide::Buy)
                .unwrap()
        ));
        out.push(format!(
            "{:?}",
            candidate.observe_book(ns(2), Decimal::from(125)).unwrap()
        ));
        out.push(format!(
            "{:?}",
            candidate.observe_book(ns(8), Decimal::from(130)).unwrap()
        ));
        out
    };
    assert_eq!(run(), run());
}

#[test]
fn kyle_lambda_replays_identically() {
    assert_eq!(run_kyle_forward_fixture(), run_kyle_forward_fixture());
}

#[test]
fn sweep_detection_replays_identically() {
    let run = || {
        let mut candidate = SweepDetectionCandidate::new(3);
        let batch = [
            SweepBatchTrade {
                price: Decimal::from(100),
                size: Decimal::from(1),
                aggressor: AggressorSide::Buy,
            },
            SweepBatchTrade {
                price: dec(10001, 2),
                size: Decimal::from(1),
                aggressor: AggressorSide::Buy,
            },
            SweepBatchTrade {
                price: dec(10002, 2),
                size: Decimal::from(1),
                aggressor: AggressorSide::Buy,
            },
        ];
        candidate.observe_batch(ns(1), &batch).unwrap()
    };
    assert_eq!(run(), run());
}

#[test]
fn book_resilience_replays_identically() {
    let run = || {
        let mut candidate = BookResilienceCandidate::with_default_thresholds();
        let mut out = Vec::new();
        for (second, bid) in [(1u64, 100i64), (2, 40), (3, 80)] {
            out.push(candidate.observe_book(ns(second), top(bid, 100)).unwrap());
        }
        out
    };
    assert_eq!(run(), run());
}

// --- variant-set guard: enum as_str mappings match the definitions -----------

/// The set of enum variant strings one measurement declares for a field.
///
/// Both venue definitions must declare the identical variant set: they are one
/// measurement published twice, and a divergence would mean the computation
/// means something different on one venue.
fn adopted_field_variants(measurement: &str, field_name: &str) -> BTreeSet<String> {
    let mut shared: Option<BTreeSet<String>> = None;
    for venue in ADOPTED_VENUES {
        let variants = declared_variants(&adopted_definition(venue, measurement), field_name);
        match &shared {
            None => shared = Some(variants),
            Some(previous) => assert_eq!(
                *previous, variants,
                "btc.*.{measurement} {field_name} variants differ across venues"
            ),
        }
    }
    shared.expect("at least one adopted venue")
}

fn declared_variants(value: &serde_json::Value, field_name: &str) -> BTreeSet<String> {
    value["fields"]
        .as_array()
        .unwrap_or_else(|| panic!("fields is an array"))
        .iter()
        .find(|field| field["name"].as_str() == Some(field_name))
        .unwrap_or_else(|| panic!("no field {field_name}"))["variants"]
        .as_array()
        .unwrap_or_else(|| panic!("field {field_name} has no variants"))
        .iter()
        .map(|variant| {
            variant
                .as_str()
                .unwrap_or_else(|| panic!("{field_name} variant is a string"))
                .to_owned()
        })
        .collect()
}

#[test]
fn candidate_spot_enum_variant_strings_match_the_adopted_definitions() {
    // AggressorSide covers the effective-spread aggressor.
    let aggressors: BTreeSet<String> = [AggressorSide::Buy, AggressorSide::Sell]
        .into_iter()
        .map(|value| value.as_str().to_owned())
        .collect();
    assert_eq!(
        adopted_field_variants("effective_spread", "aggressor_side"),
        aggressors,
        "effective_spread aggressor_side variants"
    );

    // The sweep direction is the aggressor side PLUS the `none` sentinel a
    // ready update carries before any sweep has fired.
    let directions: BTreeSet<String> = [
        SweepDirection::Buy,
        SweepDirection::Sell,
        SweepDirection::None,
    ]
    .into_iter()
    .map(|value| value.as_str().to_owned())
    .collect();
    assert_eq!(
        adopted_field_variants("sweep_detection", "direction"),
        directions,
        "sweep_detection direction variants"
    );
    assert_eq!(
        adopted_field_variants("sweep_detection", "detection_state"),
        [SweepDetectionState::Sweep, SweepDetectionState::Quiet]
            .into_iter()
            .map(|value| value.as_str().to_owned())
            .collect::<BTreeSet<String>>(),
        "sweep_detection detection_state variants"
    );

    // BookSide is the tracked side; DepletionSide adds the `none` sentinel the
    // draft declares for "no depletion tracked".
    let sides: BTreeSet<String> = [BookSide::Bid, BookSide::Ask]
        .into_iter()
        .map(|value| value.as_str().to_owned())
        .collect();
    let depletion_sides: BTreeSet<String> =
        [DepletionSide::Bid, DepletionSide::Ask, DepletionSide::None]
            .into_iter()
            .map(|value| value.as_str().to_owned())
            .collect();
    assert!(
        sides.is_subset(&depletion_sides),
        "every book side must remain a declarable depletion side"
    );
    assert_eq!(
        adopted_field_variants("book_resilience", "depletion_side"),
        depletion_sides,
        "book_resilience depletion_side variants"
    );
    assert_eq!(
        adopted_field_variants("book_resilience", "state"),
        [
            ResilienceState::Recovering,
            ResilienceState::Recovered,
            ResilienceState::Quiet,
        ]
        .into_iter()
        .map(|value| value.as_str().to_owned())
        .collect::<BTreeSet<String>>(),
        "book_resilience state variants"
    );
    assert_eq!(
        adopted_field_variants("effective_spread", "realized_state"),
        [RealizedState::Ready, RealizedState::AwaitingHorizon]
            .into_iter()
            .map(|value| value.as_str().to_owned())
            .collect::<BTreeSet<String>>(),
        "effective_spread realized_state variants"
    );
}

/// The presence flags and sentinels the definitions declare must exist as `bool`
/// fields, so a Ready update can carry every declared field (signals-core
/// requires all of them when the state is ready).
#[test]
fn candidate_spot_presence_flags_are_declared_by_the_adopted_definitions() {
    for (measurement, field) in [
        ("book_resilience", "recovery_ms_present"),
        ("effective_spread", "realized_present"),
    ] {
        for venue in ADOPTED_VENUES {
            let label = format!("btc.{venue}.{measurement}");
            let value = adopted_definition(venue, measurement);
            let declared = value["fields"]
                .as_array()
                .unwrap_or_else(|| panic!("{label} fields is an array"))
                .iter()
                .find(|candidate| candidate["name"].as_str() == Some(field))
                .unwrap_or_else(|| panic!("{label} must declare {field}"));
            assert_eq!(
                declared["type"],
                serde_json::json!("bool"),
                "{label} {field}"
            );
        }
    }

    // The sentinels the computations emit when the flag is false.
    let awaiting = RealizedPart::awaiting();
    assert!(!awaiting.realized_present);
    assert_eq!(awaiting.realized_spread_bps, Decimal::ZERO);
    assert_eq!(awaiting.midpoint_at_horizon, Decimal::ZERO);
}
