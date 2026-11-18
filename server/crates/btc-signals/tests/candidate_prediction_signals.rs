//! Prediction-venue computations from the candidate module, six of which the
//! prediction processors publish and one of which the spot processor publishes.
//!
//! Ready assertions carry their arithmetic in comments so the exact expected
//! value is auditable, and the fee cases are hand-computed against the shared
//! `signals-core` rounding rule. Divisors are kept to terminating decimals so
//! `rust_decimal` results are exact.

use std::collections::BTreeSet;
use std::path::Path;

use btc_signals::candidate_prediction_signals::{
    AbsorptionState, CANDIDATE_PREDICTION_SIGNAL_VERSION, DEFAULT_BIG_MOVE_BPS,
    DEFAULT_FEE_ALIGNMENT, DEFAULT_KALSHI_FEE_RATE, DEFAULT_MOVE_THRESHOLD_BPS,
    DEFAULT_NEAR_TIE_THRESHOLD, DEFAULT_POLYMARKET_FEE_RATE, DEFAULT_PREMIUM_THRESHOLD,
    EARLY_MOVE_FADE_CONTEXT_ID, EarlyMoveFadeContextCandidate, FadeOutcome, FadeState, FeeRounding,
    KALSHI_PAIR_COST_STATE_ID, KALSHI_TIE_RULE_CONTEXT_ID, KALSHI_VENUE_REPRICING_LAG_ID,
    KalshiPairCostOutcome, KalshiPairCostStateCandidate, MomentumDirection,
    POLYMARKET_PAIR_COST_STATE_ID, POLYMARKET_TIE_RULE_CONTEXT_ID,
    POLYMARKET_VENUE_REPRICING_LAG_ID, PairCostOutcome, PairCostState, PairCostStateCandidate,
    RepricingState, TieOutcome, TieRuleContextCandidate, TieRuleOutcome,
    VenueRepricingLagCandidate,
};
use rust_decimal::Decimal;

fn ns(seconds: u64) -> u64 {
    seconds * 1_000_000_000
}

fn dec(mantissa: i64, scale: u32) -> Decimal {
    Decimal::new(mantissa, scale)
}

// --- 1. pair_cost_state ----------------------------------------------------

#[test]
fn pair_cost_premium_matches_hand_computed_fees() {
    let candidate = PairCostStateCandidate::with_defaults(); // rate 0.07, ceil-cent, premium 0.01
    // pair_cost = 0.55 + 0.47 - 1 = 0.02.
    // up leg raw fee = 0.07 * 0.55 * 0.45 = 0.017325 -> ceil-cent = 0.02.
    // down leg raw fee = 0.07 * 0.47 * 0.53 = 0.0174370 -> ceil-cent = 0.02.
    // fee_adjusted = 0.02 + 0.02 + 0.02 = 0.06 > premium_threshold (0.01) -> premium.
    match candidate
        .evaluate(Some(dec(55, 2)), Some(dec(47, 2)))
        .expect("evaluates")
    {
        PairCostOutcome::Ready(pair) => {
            assert_eq!(pair.pair_cost, dec(2, 2));
            assert_eq!(pair.up_leg_fee, dec(2, 2));
            assert_eq!(pair.down_leg_fee, dec(2, 2));
            assert_eq!(pair.fee_adjusted_pair_cost, dec(6, 2));
            assert_eq!(pair.state, PairCostState::Premium);
        }
        other => panic!("expected ready pair cost, got {other:?}"),
    }
}

#[test]
fn pair_cost_locked_profit_when_fee_adjusted_is_negative() {
    let candidate = PairCostStateCandidate::with_defaults();
    // pair_cost = 0.40 + 0.45 - 1 = -0.15.
    // up raw = 0.07 * 0.40 * 0.60 = 0.0168 -> 0.02; down raw = 0.07 * 0.45 * 0.55 = 0.017325 -> 0.02.
    // fee_adjusted = -0.15 + 0.02 + 0.02 = -0.11 < 0 -> locked_profit.
    match candidate
        .evaluate(Some(dec(40, 2)), Some(dec(45, 2)))
        .expect("evaluates")
    {
        PairCostOutcome::Ready(pair) => {
            assert_eq!(pair.pair_cost, dec(-15, 2));
            assert_eq!(pair.fee_adjusted_pair_cost, dec(-11, 2));
            assert_eq!(pair.state, PairCostState::LockedProfit);
        }
        other => panic!("expected ready pair cost, got {other:?}"),
    }
}

#[test]
fn pair_cost_is_fair_exactly_at_the_premium_threshold() {
    let candidate = PairCostStateCandidate::with_defaults();
    // pair_cost = 0.48 + 0.49 - 1 = -0.03.
    // up raw = 0.07 * 0.48 * 0.52 = 0.0174720 -> 0.02; down raw = 0.07 * 0.49 * 0.51 = 0.0174930 -> 0.02.
    // fee_adjusted = -0.03 + 0.04 = 0.01 == premium_threshold -> fair (premium is strictly greater).
    match candidate
        .evaluate(Some(dec(48, 2)), Some(dec(49, 2)))
        .expect("evaluates")
    {
        PairCostOutcome::Ready(pair) => {
            assert_eq!(pair.fee_adjusted_pair_cost, dec(1, 2));
            assert_eq!(pair.state, PairCostState::Fair);
        }
        other => panic!("expected ready pair cost, got {other:?}"),
    }
}

#[test]
fn pair_cost_is_not_ready_with_a_one_sided_book() {
    let candidate = PairCostStateCandidate::with_defaults();
    assert_eq!(
        candidate
            .evaluate(Some(dec(55, 2)), None)
            .expect("evaluates"),
        PairCostOutcome::NotReady {
            reason: "missing_outcome_ask",
        }
    );
    assert_eq!(
        candidate
            .evaluate(None, Some(dec(47, 2)))
            .expect("evaluates"),
        PairCostOutcome::NotReady {
            reason: "missing_outcome_ask",
        }
    );
}

#[test]
fn pair_cost_fee_rounding_mirrors_the_authority_across_modes() {
    // Same raw up-leg fee 0.017325 under different rounding modes.
    let floor = PairCostStateCandidate::new(
        DEFAULT_POLYMARKET_FEE_RATE,
        FeeRounding::FloorCent,
        DEFAULT_PREMIUM_THRESHOLD,
    );
    match floor
        .evaluate(Some(dec(55, 2)), Some(dec(47, 2)))
        .expect("evaluates")
    {
        // floor-cent(0.017325) = 0.01.
        PairCostOutcome::Ready(pair) => assert_eq!(pair.up_leg_fee, dec(1, 2)),
        other => panic!("expected ready pair cost, got {other:?}"),
    }

    let nearest = PairCostStateCandidate::new(
        DEFAULT_POLYMARKET_FEE_RATE,
        FeeRounding::NearestCent,
        DEFAULT_PREMIUM_THRESHOLD,
    );
    match nearest
        .evaluate(Some(dec(55, 2)), Some(dec(47, 2)))
        .expect("evaluates")
    {
        // nearest-cent(0.017325) = 0.02.
        PairCostOutcome::Ready(pair) => assert_eq!(pair.up_leg_fee, dec(2, 2)),
        other => panic!("expected ready pair cost, got {other:?}"),
    }
}

// --- 1b. pair_cost_state (Kalshi) ------------------------------------------

#[test]
fn kalshi_fee_reproduces_the_official_schedule_table_values() {
    let candidate = KalshiPairCostStateCandidate::with_defaults(); // rate 0.07, align 0.01
    // Per-contract fee = ceil_to_0.01(0.07 * P * (1 - P)):
    //   P=0.05 -> 0.07*0.05*0.95 = 0.003325 -> $0.01
    //   P=0.50 -> 0.07*0.50*0.50 = 0.017500 -> $0.02
    //   P=0.85 -> 0.07*0.85*0.15 = 0.008925 -> $0.01
    // (100-contract sanity per the PDF, aggregate-rounded, not what this
    //  per-contract candidate computes: 0.35*100 -> 1.5925 -> $1.60.)
    match candidate
        .evaluate(Some(dec(5, 2)), Some(dec(50, 2)))
        .unwrap()
    {
        KalshiPairCostOutcome::Ready(pair) => {
            assert_eq!(pair.yes_leg_fee, dec(1, 2)); // fee(0.05)
            assert_eq!(pair.no_leg_fee, dec(2, 2)); // fee(0.50)
        }
        other => panic!("expected ready pair cost, got {other:?}"),
    }
    match candidate
        .evaluate(Some(dec(85, 2)), Some(dec(5, 2)))
        .unwrap()
    {
        KalshiPairCostOutcome::Ready(pair) => {
            assert_eq!(pair.yes_leg_fee, dec(1, 2)); // fee(0.85)
            assert_eq!(pair.no_leg_fee, dec(1, 2)); // fee(0.05)
        }
        other => panic!("expected ready pair cost, got {other:?}"),
    }
}

#[test]
fn kalshi_direct_member_alignment_rounds_to_a_tenth_of_a_cent() {
    // Direct-member tier aligns to $0.0001: fee(0.05) = ceil(0.003325/0.0001)*0.0001
    //   = ceil(33.25) * 0.0001 = 34 * 0.0001 = 0.0034.
    let candidate = KalshiPairCostStateCandidate::new(
        DEFAULT_KALSHI_FEE_RATE,
        dec(1, 4),
        DEFAULT_PREMIUM_THRESHOLD,
    )
    .expect("valid alignment");
    match candidate
        .evaluate(Some(dec(5, 2)), Some(dec(50, 2)))
        .unwrap()
    {
        KalshiPairCostOutcome::Ready(pair) => assert_eq!(pair.yes_leg_fee, dec(34, 4)),
        other => panic!("expected ready pair cost, got {other:?}"),
    }
    // A non-positive alignment is rejected (it is a rounding divisor).
    assert!(
        KalshiPairCostStateCandidate::new(
            DEFAULT_KALSHI_FEE_RATE,
            Decimal::ZERO,
            DEFAULT_PREMIUM_THRESHOLD
        )
        .is_err()
    );
}

#[test]
fn kalshi_pair_cost_equals_the_yes_spread() {
    let candidate = KalshiPairCostStateCandidate::with_defaults();
    // yes_bid = 0.48, yes_ask = 0.52, so no_ask = 1 - yes_bid = 0.52.
    // pair_cost = yes_ask + no_ask - 1 = 0.52 + 0.52 - 1 = 0.04, which equals the
    // YES spread yes_ask - yes_bid = 0.52 - 0.48 = 0.04.
    match candidate
        .evaluate(Some(dec(52, 2)), Some(dec(52, 2)))
        .unwrap()
    {
        KalshiPairCostOutcome::Ready(pair) => assert_eq!(pair.pair_cost, dec(4, 2)),
        other => panic!("expected ready pair cost, got {other:?}"),
    }
}

#[test]
fn kalshi_pair_cost_states_mirror_the_polymarket_bands() {
    let candidate = KalshiPairCostStateCandidate::with_defaults();
    // premium: pair_cost 0.02 + fees 0.02 + 0.02 = 0.06 > 0.01.
    match candidate
        .evaluate(Some(dec(55, 2)), Some(dec(47, 2)))
        .unwrap()
    {
        KalshiPairCostOutcome::Ready(pair) => {
            assert_eq!(pair.fee_adjusted_pair_cost, dec(6, 2));
            assert_eq!(pair.state, PairCostState::Premium);
        }
        other => panic!("got {other:?}"),
    }
    // locked_profit: pair_cost -0.15 + 0.02 + 0.02 = -0.11 < 0.
    match candidate
        .evaluate(Some(dec(40, 2)), Some(dec(45, 2)))
        .unwrap()
    {
        KalshiPairCostOutcome::Ready(pair) => {
            assert_eq!(pair.fee_adjusted_pair_cost, dec(-11, 2));
            assert_eq!(pair.state, PairCostState::LockedProfit);
        }
        other => panic!("got {other:?}"),
    }
    // fair exactly at the premium threshold: pair_cost -0.03 + 0.04 = 0.01.
    match candidate
        .evaluate(Some(dec(48, 2)), Some(dec(49, 2)))
        .unwrap()
    {
        KalshiPairCostOutcome::Ready(pair) => {
            assert_eq!(pair.fee_adjusted_pair_cost, dec(1, 2));
            assert_eq!(pair.state, PairCostState::Fair);
        }
        other => panic!("got {other:?}"),
    }
}

#[test]
fn kalshi_pair_cost_is_not_ready_with_a_one_sided_book() {
    let candidate = KalshiPairCostStateCandidate::with_defaults();
    assert_eq!(
        candidate.evaluate(Some(dec(52, 2)), None).unwrap(),
        KalshiPairCostOutcome::NotReady {
            reason: "missing_outcome_ask",
        }
    );
    assert_eq!(
        candidate.evaluate(None, Some(dec(52, 2))).unwrap(),
        KalshiPairCostOutcome::NotReady {
            reason: "missing_outcome_ask",
        }
    );
}

#[test]
fn kalshi_pair_cost_replays_identically() {
    let candidate = KalshiPairCostStateCandidate::with_defaults();
    let left = candidate
        .evaluate(Some(dec(55, 2)), Some(dec(47, 2)))
        .unwrap();
    let right = candidate
        .evaluate(Some(dec(55, 2)), Some(dec(47, 2)))
        .unwrap();
    assert_eq!(left, right);
}

// --- 2. venue_repricing_lag ------------------------------------------------

#[test]
fn repricing_lag_measures_the_first_answering_change() {
    let mut candidate = VenueRepricingLagCandidate::with_default_threshold(); // 10 bps
    candidate.observe_spot(ns(1), Decimal::from(100)).unwrap();
    candidate
        .observe_venue_quote(ns(1), dec(5, 1), dec(5, 1))
        .unwrap(); // 0.50 / 0.50 baseline
    // Spot 100 -> 100.2 is a 20 bps move (>= 10): unanswered from 2s.
    candidate.observe_spot(ns(2), dec(1002, 1)).unwrap();
    // First venue change after the move (4s, affirmative 0.50 -> 0.51) answers it.
    let answered = candidate
        .observe_venue_quote(ns(4), dec(51, 2), dec(5, 1))
        .unwrap();
    assert_eq!(answered.state, RepricingState::Current);
    assert_eq!(answered.repricing_lag_ms, Some(2000)); // 4s - 2s
    assert_eq!(answered.staleness_ms, 0);
}

#[test]
fn repricing_staleness_is_the_unanswered_move_waiting_time() {
    let mut candidate = VenueRepricingLagCandidate::with_default_threshold();
    candidate.observe_spot(ns(1), Decimal::from(100)).unwrap();
    candidate
        .observe_venue_quote(ns(1), dec(5, 1), dec(5, 1))
        .unwrap();
    // A venue change BEFORE any move (no move pending) must not inflate staleness.
    candidate
        .observe_venue_quote(ns(2), dec(51, 2), dec(49, 2))
        .unwrap();
    // 100 -> 100.2 over the last two seconds (within the 5 s lookback) is a 20 bps
    // move, armed at 3s. staleness is measured from the move, so at the move it is 0.
    let moved = candidate.observe_spot(ns(3), dec(1002, 1)).unwrap();
    assert_eq!(moved.state, RepricingState::Lagging);
    assert_eq!(moved.staleness_ms, 0);
    // One second later with no new answering quote: staleness = 4s - 3s = 1000 ms.
    // The still-elevated window does not re-arm while the move is unanswered.
    let waited = candidate.observe_spot(ns(4), dec(1002, 1)).unwrap();
    assert_eq!(waited.state, RepricingState::Lagging);
    assert_eq!(waited.staleness_ms, 1000);
}

#[test]
fn repricing_does_not_re_arm_while_a_move_is_unanswered() {
    let mut candidate = VenueRepricingLagCandidate::with_default_threshold();
    candidate.observe_spot(ns(1), Decimal::from(100)).unwrap();
    candidate
        .observe_venue_quote(ns(1), dec(5, 1), dec(5, 1))
        .unwrap();
    candidate.observe_spot(ns(2), dec(1002, 1)).unwrap(); // move armed at 2s (20 bps over 1s)
    // A further qualifying return at 3s (still 100 in the 5 s window -> 100.4 ~40 bps)
    // does NOT re-arm; the first unanswered move stands.
    candidate.observe_spot(ns(3), dec(1004, 1)).unwrap();
    // Venue answers at 5s: lag is measured from the first unanswered move (2s).
    let answered = candidate
        .observe_venue_quote(ns(5), dec(51, 2), dec(5, 1))
        .unwrap();
    assert_eq!(answered.repricing_lag_ms, Some(3000)); // 5s - 2s
}

#[test]
fn repricing_an_unchanged_two_sided_quote_does_not_answer_a_move() {
    let mut candidate = VenueRepricingLagCandidate::with_default_threshold();
    candidate.observe_spot(ns(1), Decimal::from(100)).unwrap();
    candidate
        .observe_venue_quote(ns(1), dec(5, 1), dec(5, 1))
        .unwrap();
    candidate.observe_spot(ns(2), dec(1002, 1)).unwrap(); // move pending at 2s

    // Both mids unchanged -> not a reprice, does not answer the move.
    let unchanged = candidate
        .observe_venue_quote(ns(3), dec(5, 1), dec(5, 1))
        .unwrap();
    assert_eq!(unchanged.state, RepricingState::Lagging);
    assert_eq!(unchanged.repricing_lag_ms, None);

    // The next genuine change answers it: lag = 4s - 2s = 2000 ms.
    let answered = candidate
        .observe_venue_quote(ns(4), dec(51, 2), dec(5, 1))
        .unwrap();
    assert_eq!(answered.repricing_lag_ms, Some(2000));
}

#[test]
fn repricing_a_negative_mid_only_change_answers_a_move() {
    let mut candidate = VenueRepricingLagCandidate::with_default_threshold();
    candidate.observe_spot(ns(1), Decimal::from(100)).unwrap();
    candidate
        .observe_venue_quote(ns(1), dec(5, 1), dec(5, 1))
        .unwrap();
    candidate.observe_spot(ns(2), dec(1002, 1)).unwrap(); // move pending at 2s

    // Affirmative mid unchanged (0.50), negative mid changes 0.50 -> 0.49: a change
    // in EITHER outcome midpoint answers, so a one-sided reprice is not blind.
    let answered = candidate
        .observe_venue_quote(ns(4), dec(5, 1), dec(49, 2))
        .unwrap();
    assert_eq!(answered.state, RepricingState::Current);
    assert_eq!(answered.repricing_lag_ms, Some(2000)); // 4s - 2s
}

// E53 rolling-lookback move detection: hand-computed cases.

#[test]
fn repricing_qualifies_a_move_spread_across_the_lookback() {
    // 12 bps accumulated from ns(1)=100 to ns(4)=100.12 (3 s, inside the 5 s
    // lookback) in ~4 bps consecutive steps. No consecutive step reaches 10 bps,
    // so the old sample-to-sample detector would miss it; the rolling window
    // (current vs oldest-in-window) sees the full 12 bps and arms.
    let mut candidate = VenueRepricingLagCandidate::with_default_threshold(); // 10 bps
    assert_eq!(
        candidate
            .observe_spot(ns(1), Decimal::from(100))
            .unwrap()
            .state,
        RepricingState::Current
    );
    assert_eq!(
        candidate.observe_spot(ns(2), dec(10004, 2)).unwrap().state, // +4 bps window
        RepricingState::Current
    );
    assert_eq!(
        candidate.observe_spot(ns(3), dec(10008, 2)).unwrap().state, // +8 bps window
        RepricingState::Current
    );
    assert_eq!(
        candidate.observe_spot(ns(4), dec(10012, 2)).unwrap().state, // +12 bps window
        RepricingState::Lagging
    );
}

#[test]
fn repricing_qualifies_a_ten_bps_move_at_exactly_five_seconds() {
    // 10 bps from ns(1)=100 to ns(6)=100.10 over exactly 5 s. The ns(1) sample is
    // at the boundary: eviction is strict (`event_time - oldest > lookback`), so it
    // is retained and the window return is the full 10 bps -> qualifies. Flipping
    // the eviction to `>=` would drop the boundary sample, leaving a zero-return
    // single-sample window (Current), so this pins the `>` boundary.
    let mut candidate = VenueRepricingLagCandidate::with_default_threshold();
    candidate.observe_spot(ns(1), Decimal::from(100)).unwrap();
    candidate
        .observe_venue_quote(ns(1), dec(5, 1), dec(5, 1))
        .unwrap();
    let armed = candidate.observe_spot(ns(6), dec(10010, 2)).unwrap();
    assert_eq!(armed.state, RepricingState::Lagging);
    // A venue reprice answers it: lag = 7s - 6s = 1000 ms, confirming the move
    // really armed at the boundary sample.
    let answered = candidate
        .observe_venue_quote(ns(7), dec(51, 2), dec(5, 1))
        .unwrap();
    assert_eq!(answered.state, RepricingState::Current);
    assert_eq!(answered.repricing_lag_ms, Some(1000));
}

#[test]
fn repricing_qualifies_at_exactly_ten_bps_but_not_at_9_99() {
    // Exactly 10 bps qualifies (>= threshold).
    let mut ten = VenueRepricingLagCandidate::with_default_threshold();
    ten.observe_spot(ns(1), Decimal::from(100)).unwrap();
    assert_eq!(
        ten.observe_spot(ns(2), dec(10010, 2)).unwrap().state, // 100 -> 100.10 = 10 bps
        RepricingState::Lagging
    );
    // 9.99 bps does not: 100 -> 100.0999 = 0.0999/100 * 1e4 = 9.99 bps < 10.
    let mut just_under = VenueRepricingLagCandidate::with_default_threshold();
    just_under.observe_spot(ns(1), Decimal::from(100)).unwrap();
    let reading = just_under.observe_spot(ns(2), dec(1000999, 4)).unwrap();
    assert_eq!(reading.state, RepricingState::Current);
    assert_eq!(reading.repricing_lag_ms, None);
}

#[test]
fn repricing_does_not_re_arm_a_stale_window_after_an_answer() {
    // Audit scenario: 100 @1s, 100.2 @2s arms, answered @4s, an unchanged 100.2 @5s
    // must NOT re-arm even though the pre-move 100 sample is still in the 5 s window.
    let mut candidate = VenueRepricingLagCandidate::with_default_threshold();
    candidate.observe_spot(ns(1), Decimal::from(100)).unwrap();
    candidate
        .observe_venue_quote(ns(1), dec(5, 1), dec(5, 1))
        .unwrap();
    candidate.observe_spot(ns(2), dec(1002, 1)).unwrap(); // 20 bps -> armed at 2s
    let answered = candidate
        .observe_venue_quote(ns(4), dec(51, 2), dec(5, 1))
        .unwrap();
    assert_eq!(answered.repricing_lag_ms, Some(2000)); // 4s - 2s
    // Unchanged spot at 5s: oldest retained is still 100 @1s (5-1=4 <= 5), so the
    // window return is still 20 bps, but the qualifying flag never fell -> no re-arm.
    let quiet = candidate.observe_spot(ns(5), dec(1002, 1)).unwrap();
    assert_eq!(quiet.state, RepricingState::Current);
    // A subsequent venue reprice must not publish a fabricated lag from the quiet
    // sample: pending is empty, so the recorded lag stays the genuine 2000 ms.
    let after = candidate
        .observe_venue_quote(ns(6), dec(52, 2), dec(5, 1))
        .unwrap();
    assert_eq!(after.state, RepricingState::Current);
    assert_eq!(after.repricing_lag_ms, Some(2000));
}

#[test]
fn repricing_re_arms_a_genuine_second_move_after_the_window_clears() {
    let mut candidate = VenueRepricingLagCandidate::with_default_threshold();
    candidate.observe_spot(ns(1), Decimal::from(100)).unwrap();
    candidate
        .observe_venue_quote(ns(1), dec(5, 1), dec(5, 1))
        .unwrap();
    candidate.observe_spot(ns(2), dec(1002, 1)).unwrap(); // armed at 2s
    candidate
        .observe_venue_quote(ns(4), dec(51, 2), dec(5, 1))
        .unwrap(); // answered, lag 2000 ms
    // Advance past the lookback so both 100 @1s and 100.2 @2s are evicted; the lone
    // 100.2 @8s sample has a zero-return window -> qualifying flag falls.
    let cleared = candidate.observe_spot(ns(8), dec(1002, 1)).unwrap();
    assert_eq!(cleared.state, RepricingState::Current);
    // A genuine new move now re-qualifies (100.2 -> 100.4 = ~19.96 bps over 1s) and
    // arms a fresh measurement.
    let rearmed = candidate.observe_spot(ns(9), dec(1004, 1)).unwrap();
    assert_eq!(rearmed.state, RepricingState::Lagging);
    let answered = candidate
        .observe_venue_quote(ns(10), dec(52, 2), dec(5, 1))
        .unwrap();
    assert_eq!(answered.repricing_lag_ms, Some(1000)); // 10s - 9s
}

#[test]
fn repricing_counts_a_move_that_reverses_within_the_window_once() {
    let mut candidate = VenueRepricingLagCandidate::with_default_threshold();
    candidate.observe_spot(ns(1), Decimal::from(100)).unwrap();
    candidate
        .observe_venue_quote(ns(1), dec(5, 1), dec(5, 1))
        .unwrap();
    candidate.observe_spot(ns(2), dec(1002, 1)).unwrap(); // 20 bps -> armed at 2s
    // Price reverses back to 100 within the window: the qualifying flag falls, but
    // the pending move is only cleared by a venue answer, so it still stands.
    let reversed = candidate.observe_spot(ns(3), Decimal::from(100)).unwrap();
    assert_eq!(reversed.state, RepricingState::Lagging);
    // A re-qualifying move while the first is still unanswered does NOT re-arm.
    candidate.observe_spot(ns(4), dec(1002, 1)).unwrap();
    // The single answer measures from the original arm at 2s: counted once.
    let answered = candidate
        .observe_venue_quote(ns(5), dec(51, 2), dec(5, 1))
        .unwrap();
    assert_eq!(answered.repricing_lag_ms, Some(3000)); // 5s - 2s
}

#[test]
fn repricing_does_not_qualify_a_sustained_twelve_bps_drift_over_six_seconds() {
    // A slow drift of 12 bps from ns(1)=100 to ns(7)=100.12 sampled every 2 s. No
    // 5 s window ever accumulates 10 bps (the widest is 8 bps), so the flag never
    // rises and nothing arms across the whole drift.
    let mut candidate = VenueRepricingLagCandidate::with_default_threshold();
    candidate
        .observe_venue_quote(ns(1), dec(5, 1), dec(5, 1))
        .unwrap();
    for (t, price) in [
        (1_u64, Decimal::from(100)),
        (3, dec(10004, 2)),
        (5, dec(10008, 2)),
        (7, dec(10012, 2)),
    ] {
        let reading = candidate.observe_spot(ns(t), price).unwrap();
        assert_eq!(reading.state, RepricingState::Current, "armed at t={t}s");
        assert_eq!(reading.repricing_lag_ms, None);
    }
}

#[test]
fn repricing_does_not_qualify_a_twelve_bps_move_over_six_seconds() {
    // 12 bps from ns(1)=100 to ns(7)=100.12 over 6 s: the ns(1) sample is older
    // than the 5 s lookback and evicted, leaving only the current sample, so the
    // window return is 0 -> a move spread beyond the lookback is invisible.
    let mut candidate = VenueRepricingLagCandidate::with_default_threshold();
    candidate.observe_spot(ns(1), Decimal::from(100)).unwrap();
    let reading = candidate.observe_spot(ns(7), dec(10012, 2)).unwrap();
    assert_eq!(reading.state, RepricingState::Current);
    assert_eq!(reading.repricing_lag_ms, None);
}

#[test]
fn repricing_reason_sequence_no_qualifying_move_then_ready() {
    // Mirrors the emitter mapping in prediction_signals::repricing_lag_output:
    // no inputs -> upstream_measurement_unavailable; both inputs present but no
    // answered move -> no_qualifying_move (repricing_lag_ms is None, not ready);
    // first answered qualifying move -> ready (repricing_lag_ms is Some).
    let mut candidate = VenueRepricingLagCandidate::with_default_threshold();
    assert!(
        !candidate.has_measurement_inputs(),
        "no inputs yet -> upstream_measurement_unavailable"
    );
    candidate.observe_spot(ns(1), Decimal::from(100)).unwrap();
    candidate
        .observe_venue_quote(ns(1), dec(5, 1), dec(5, 1))
        .unwrap();
    // Both upstreams seen, but no qualifying move yet -> no_qualifying_move.
    let warming = candidate.observe_spot(ns(2), Decimal::from(100)).unwrap();
    assert!(candidate.has_measurement_inputs());
    assert_eq!(warming.repricing_lag_ms, None);
    // A qualifying move (20 bps over 1 s) then a venue reprice completes the first
    // measurement -> ready.
    candidate.observe_spot(ns(3), dec(1002, 1)).unwrap();
    let answered = candidate
        .observe_venue_quote(ns(4), dec(51, 2), dec(5, 1))
        .unwrap();
    assert_eq!(answered.repricing_lag_ms, Some(1000)); // 4s - 3s
}

// --- 3. tie_rule_context ---------------------------------------------------

#[test]
fn tie_rule_gives_an_advantage_near_a_non_void_tie() {
    let mut candidate = TieRuleContextCandidate::with_default_threshold(); // 0.1
    // Only one input seen -> not ready.
    assert_eq!(
        candidate.observe_tie_outcome(TieOutcome::Up),
        TieRuleOutcome::NotReady {
            reason: "inputs_warming_up",
        }
    );
    // |0.05| <= 0.1 -> near tie; tie is up -> advantage up.
    match candidate.observe_moneyness(dec(5, 2)) {
        TieRuleOutcome::Ready(context) => {
            assert_eq!(context.tie_outcome, TieOutcome::Up);
            assert!(context.near_tie);
            assert_eq!(context.tie_advantage_side, Some(TieOutcome::Up));
        }
        other => panic!("expected ready tie context, got {other:?}"),
    }
}

#[test]
fn tie_rule_void_near_a_tie_has_no_advantage_side() {
    let mut candidate = TieRuleContextCandidate::with_default_threshold();
    candidate.observe_tie_outcome(TieOutcome::Void);
    match candidate.observe_moneyness(Decimal::ZERO) {
        TieRuleOutcome::Ready(context) => {
            assert!(context.near_tie);
            assert_eq!(context.tie_advantage_side, None);
        }
        other => panic!("expected ready tie context, got {other:?}"),
    }
}

#[test]
fn tie_rule_threshold_is_inclusive() {
    let mut candidate = TieRuleContextCandidate::with_default_threshold(); // 0.1
    candidate.observe_tie_outcome(TieOutcome::Down);
    // Exactly at the threshold is near.
    match candidate.observe_moneyness(dec(1, 1)) {
        TieRuleOutcome::Ready(context) => {
            assert!(context.near_tie);
            assert_eq!(context.tie_advantage_side, Some(TieOutcome::Down));
        }
        other => panic!("expected ready tie context, got {other:?}"),
    }
    // Just beyond the threshold is not near.
    match candidate.observe_moneyness(dec(11, 2)) {
        TieRuleOutcome::Ready(context) => {
            assert!(!context.near_tie);
            assert_eq!(context.tie_advantage_side, None);
        }
        other => panic!("expected ready tie context, got {other:?}"),
    }
}

// --- 4. early_move_fade_context --------------------------------------------

#[test]
fn fade_is_quiet_below_the_big_move_threshold() {
    let mut candidate = EarlyMoveFadeContextCandidate::with_default_threshold(); // 20 bps
    // One input only -> not ready.
    assert_eq!(
        candidate.observe_momentum(Decimal::from(10), Decimal::from(10), MomentumDirection::Up),
        FadeOutcome::NotReady {
            reason: "inputs_warming_up",
        }
    );
    // |10| < 20 -> quiet regardless of absorption.
    match candidate.observe_absorption(AbsorptionState::Absorbing) {
        FadeOutcome::Ready(context) => assert_eq!(context.state, FadeState::Quiet),
        other => panic!("expected ready fade context, got {other:?}"),
    }
}

#[test]
fn fade_classifies_each_outsized_absorption_state() {
    // Outsized + absorbing -> the fade-candidate context.
    let mut absorbing = EarlyMoveFadeContextCandidate::with_default_threshold();
    absorbing.observe_momentum(Decimal::from(25), Decimal::from(25), MomentumDirection::Up);
    match absorbing.observe_absorption(AbsorptionState::Absorbing) {
        FadeOutcome::Ready(context) => assert_eq!(context.state, FadeState::OutsizedAbsorbing),
        other => panic!("got {other:?}"),
    }

    // Outsized (negative) + price-responsive.
    let mut responsive = EarlyMoveFadeContextCandidate::with_default_threshold();
    responsive.observe_momentum(
        Decimal::from(-25),
        Decimal::from(-25),
        MomentumDirection::Down,
    );
    match responsive.observe_absorption(AbsorptionState::PriceResponsive) {
        FadeOutcome::Ready(context) => assert_eq!(context.state, FadeState::OutsizedResponsive),
        other => panic!("got {other:?}"),
    }

    // Exactly at the threshold (|20| >= 20) is outsized; mixed -> unclear.
    let mut unclear = EarlyMoveFadeContextCandidate::with_default_threshold();
    unclear.observe_momentum(
        Decimal::from(20),
        Decimal::from(20),
        MomentumDirection::Flat,
    );
    match unclear.observe_absorption(AbsorptionState::Mixed) {
        FadeOutcome::Ready(context) => assert_eq!(context.state, FadeState::OutsizedUnclear),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn fade_outsized_classification_uses_the_thirty_second_return() {
    // E53: 25 bps over 20 s (captured by the 30 s return) is outsized even though
    // the 1 s return is only 1 bps; the 1 s return remains published as context.
    let mut outsized = EarlyMoveFadeContextCandidate::with_default_threshold(); // 20 bps
    outsized.observe_momentum(Decimal::from(1), Decimal::from(25), MomentumDirection::Up);
    match outsized.observe_absorption(AbsorptionState::Absorbing) {
        FadeOutcome::Ready(context) => {
            assert_eq!(context.state, FadeState::OutsizedAbsorbing);
            assert_eq!(context.return_bps_1s, Decimal::from(1));
        }
        other => panic!("expected ready fade context, got {other:?}"),
    }
    // 5 bps over 30 s stays quiet even though the 1 s return (20 bps) would have
    // been outsized under the retired sample-to-sample rule.
    let mut quiet = EarlyMoveFadeContextCandidate::with_default_threshold();
    quiet.observe_momentum(Decimal::from(20), Decimal::from(5), MomentumDirection::Up);
    match quiet.observe_absorption(AbsorptionState::Absorbing) {
        FadeOutcome::Ready(context) => assert_eq!(context.state, FadeState::Quiet),
        other => panic!("expected ready fade context, got {other:?}"),
    }
}

// --- drift guard: consts and defaults match the definitions ----------------

#[test]
fn candidate_prediction_ids_version_and_defaults_match_the_definitions() {
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../definitions/btc/signals");
    let cases = [
        (
            POLYMARKET_PAIR_COST_STATE_ID,
            "btc.polymarket.pair_cost_state@1.0.0.json",
            vec![
                format!("fee_rate defaults to {DEFAULT_POLYMARKET_FEE_RATE}"),
                "fee_rounding defaults to ceil_cent".to_owned(),
                format!("premium_threshold defaults to {DEFAULT_PREMIUM_THRESHOLD}"),
            ],
        ),
        (
            KALSHI_PAIR_COST_STATE_ID,
            "btc.kalshi.pair_cost_state@1.0.0.json",
            vec![
                format!("fee_rate defaults to {DEFAULT_KALSHI_FEE_RATE}"),
                format!("fee_alignment defaults to {DEFAULT_FEE_ALIGNMENT}"),
                format!("premium_threshold defaults to {DEFAULT_PREMIUM_THRESHOLD}"),
            ],
        ),
        (
            POLYMARKET_VENUE_REPRICING_LAG_ID,
            "btc.polymarket.venue_repricing_lag@1.0.0.json",
            vec![format!(
                "move_threshold_bps defaults to {DEFAULT_MOVE_THRESHOLD_BPS}"
            )],
        ),
        (
            KALSHI_VENUE_REPRICING_LAG_ID,
            "btc.kalshi.venue_repricing_lag@1.0.0.json",
            vec![format!(
                "move_threshold_bps defaults to {DEFAULT_MOVE_THRESHOLD_BPS}"
            )],
        ),
        (
            POLYMARKET_TIE_RULE_CONTEXT_ID,
            "btc.polymarket.tie_rule_context@1.0.0.json",
            vec![
                format!("near_tie_threshold defaults to {DEFAULT_NEAR_TIE_THRESHOLD}"),
                // The definition must name the normalization mapping (B1).
                "Polymarket up maps to up".to_owned(),
            ],
        ),
        (
            KALSHI_TIE_RULE_CONTEXT_ID,
            "btc.kalshi.tie_rule_context@1.0.0.json",
            vec![
                format!("near_tie_threshold defaults to {DEFAULT_NEAR_TIE_THRESHOLD}"),
                "Kalshi yes maps to up".to_owned(),
            ],
        ),
        (
            EARLY_MOVE_FADE_CONTEXT_ID,
            "btc.external.early_move_fade_context@1.0.0.json",
            vec![format!("big_move_bps defaults to {DEFAULT_BIG_MOVE_BPS}")],
        ),
    ];
    for (id, file, expected_defaults) in cases {
        let bytes =
            std::fs::read(base.join(file)).unwrap_or_else(|error| panic!("read {file}: {error}"));
        let value: serde_json::Value =
            serde_json::from_slice(&bytes).unwrap_or_else(|error| panic!("parse {file}: {error}"));
        assert_eq!(
            value["signal_id"],
            serde_json::json!(id),
            "{file} signal_id"
        );
        assert_eq!(
            value["version"],
            serde_json::json!(CANDIDATE_PREDICTION_SIGNAL_VERSION),
            "{file} version"
        );
        let description = value["description"]
            .as_str()
            .unwrap_or_else(|| panic!("{file} description is a string"));
        for expected in &expected_defaults {
            assert!(
                description.contains(expected.as_str()),
                "{file} description must state the default: `{expected}`"
            );
        }
    }
}

/// The set of enum variant strings a definition field declares.
fn definition_field_variants(base: &Path, file: &str, field_name: &str) -> BTreeSet<String> {
    let bytes =
        std::fs::read(base.join(file)).unwrap_or_else(|error| panic!("read {file}: {error}"));
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).unwrap_or_else(|error| panic!("parse {file}: {error}"));
    value["fields"]
        .as_array()
        .unwrap_or_else(|| panic!("{file} fields is an array"))
        .iter()
        .find(|field| field["name"].as_str() == Some(field_name))
        .unwrap_or_else(|| panic!("{file} has no field {field_name}"))["variants"]
        .as_array()
        .unwrap_or_else(|| panic!("{file} field {field_name} has no variants"))
        .iter()
        .map(|variant| {
            variant
                .as_str()
                .unwrap_or_else(|| panic!("{file} {field_name} variant is a string"))
                .to_owned()
        })
        .collect()
}

#[test]
fn candidate_prediction_enum_variant_strings_match_the_definitions() {
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../definitions/btc/signals");

    // Every `as_str` mapping must exactly cover its definition's declared
    // variants.
    let tie_outcomes: BTreeSet<String> = [TieOutcome::Up, TieOutcome::Down, TieOutcome::Void]
        .into_iter()
        .map(|value| value.as_str().to_owned())
        .collect();
    for file in [
        "btc.polymarket.tie_rule_context@1.0.0.json",
        "btc.kalshi.tie_rule_context@1.0.0.json",
    ] {
        assert_eq!(
            definition_field_variants(&base, file, "tie_outcome"),
            tie_outcomes,
            "{file} tie_outcome variants"
        );
    }

    let directions: BTreeSet<String> = [
        MomentumDirection::Up,
        MomentumDirection::Down,
        MomentumDirection::Flat,
    ]
    .into_iter()
    .map(|value| value.as_str().to_owned())
    .collect();
    assert_eq!(
        definition_field_variants(
            &base,
            "btc.external.early_move_fade_context@1.0.0.json",
            "direction_10s"
        ),
        directions,
        "fade direction_10s variants"
    );

    let absorptions: BTreeSet<String> = [
        AbsorptionState::InsufficientFlow,
        AbsorptionState::Absorbing,
        AbsorptionState::PriceResponsive,
        AbsorptionState::Mixed,
    ]
    .into_iter()
    .map(|value| value.as_str().to_owned())
    .collect();
    assert_eq!(
        definition_field_variants(
            &base,
            "btc.external.early_move_fade_context@1.0.0.json",
            "absorption_state"
        ),
        absorptions,
        "fade absorption_state variants"
    );
}

// --- determinism per family ------------------------------------------------

#[test]
fn pair_cost_replays_identically() {
    let candidate = PairCostStateCandidate::with_defaults();
    let left = candidate
        .evaluate(Some(dec(55, 2)), Some(dec(47, 2)))
        .unwrap();
    let right = candidate
        .evaluate(Some(dec(55, 2)), Some(dec(47, 2)))
        .unwrap();
    assert_eq!(left, right);
}

#[test]
fn repricing_lag_replays_identically() {
    let run = || {
        let mut candidate = VenueRepricingLagCandidate::with_default_threshold();
        // vec! evaluates elements left to right, preserving observation order.
        vec![
            candidate.observe_spot(ns(1), Decimal::from(100)).unwrap(),
            candidate
                .observe_venue_quote(ns(1), dec(5, 1), dec(5, 1))
                .unwrap(),
            candidate.observe_spot(ns(2), dec(1002, 1)).unwrap(),
            candidate
                .observe_venue_quote(ns(4), dec(51, 2), dec(5, 1))
                .unwrap(),
        ]
    };
    assert_eq!(run(), run());
}

#[test]
fn tie_rule_replays_identically() {
    let run = || {
        let mut candidate = TieRuleContextCandidate::with_default_threshold();
        candidate.observe_tie_outcome(TieOutcome::Up);
        candidate.observe_moneyness(dec(5, 2))
    };
    assert_eq!(run(), run());
}

#[test]
fn fade_replays_identically() {
    let run = || {
        let mut candidate = EarlyMoveFadeContextCandidate::with_default_threshold();
        candidate.observe_momentum(Decimal::from(25), Decimal::from(25), MomentumDirection::Up);
        candidate.observe_absorption(AbsorptionState::Absorbing)
    };
    assert_eq!(run(), run());
}
