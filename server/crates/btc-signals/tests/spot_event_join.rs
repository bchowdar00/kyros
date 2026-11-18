//! Unit tests for the deterministic per-venue raw-event join and the
//! microstructure driver built on it.
//!
//! Every numbered invariant of `btc_signals::spot_event_join` maps to at least
//! one named test here:
//!
//! 1. logical receipt clock — `every_observation_is_stamped_with_the_admitted_receipt_time`
//! 2. per-venue monotonicity — `receipt_time_regression_is_refused_for_that_venue`,
//!    `the_two_venues_do_not_constrain_or_close_each_other`
//! 3. atomic, identified batches — `an_open_batch_is_never_observed_before_it_closes`,
//!    `a_batch_closes_when_event_time_or_receipt_time_changes`,
//!    `trades_sharing_one_identity_stay_in_one_batch`
//! 4. causal emission order — `a_trade_batch_closes_before_the_book_observation_that_follows_it`
//! 5. explicit bounds — `an_over_full_batch_refuses_and_leaves_the_open_batch_intact`,
//!    `implausible_quotes_and_trades_are_refused_without_touching_state`
//! 6. staleness classified and enforced —
//!    `book_freshness_applies_the_shared_staleness_bound`,
//!    `the_prevailing_book_classification_splits_missing_from_stale`,
//!    `a_trade_batch_against_a_stale_book_is_not_ready_instead_of_computing`,
//!    `a_book_after_a_gap_resets_the_venue_instead_of_comparing_across_it`,
//!    `pending_state_stays_bounded_while_a_venue_book_is_stalled`
//! 7. restart determinism — `replaying_the_same_admitted_sequence_reproduces_the_observation_stream`,
//!    `clearing_a_venue_returns_it_to_its_initial_state`
//! 8. no binary floating point — `the_midpoint_is_the_exact_decimal_mean_not_a_binary_float`
//!
//! The caller precondition the module documents separately — that a book
//! observation is fed only from a completed book group — is a caller
//! obligation the join cannot check, so it has no test here.
//!
//! The remaining tests cover the driver: that the four computations really run
//! per venue over the one join, and that a joined batch is applied whole or not
//! at all.

use btc_signals::candidate_spot_signals::{
    BOOK_STALE_REASON, CandidateMicrostructureSet, DepletionSide, KyleOutcome, MAX_PENDING_TRADES,
    MicrostructureOutcome, MicrostructureParams, ResilienceState, SweepDetectionState,
    SweepDirection,
};
use btc_signals::spot_event_join::{
    AggressorSide, JoinedObservation, JoinedTrade, MAX_BATCH_TRADES, PrevailingBook, SpotEventJoin,
};
use btc_signals::{STALENESS_BOUND_NS, SpotSignalError, Venue};
use rust_decimal::Decimal;

const T0: &str = "2026-09-01T00:00:00.000000000Z";
const T1: &str = "2026-09-01T00:00:00.500000000Z";

fn decimal(text: &str) -> Decimal {
    text.parse().expect("decimal literal parses")
}

fn trade(price: &str, size: &str, aggressor: AggressorSide) -> JoinedTrade {
    JoinedTrade {
        price: decimal(price),
        size: decimal(size),
        aggressor,
    }
}

/// One admitted input, so a script can be replayed into a fresh join.
#[derive(Debug, Clone)]
enum Input {
    Book {
        venue: Venue,
        receipt_ns: u64,
        bid: &'static str,
        bid_size: &'static str,
        ask: &'static str,
        ask_size: &'static str,
    },
    Trade {
        venue: Venue,
        receipt_ns: u64,
        event_time: &'static str,
        price: &'static str,
        size: &'static str,
        aggressor: AggressorSide,
    },
    Flush,
}

fn run(script: &[Input]) -> Result<Vec<JoinedObservation>, SpotSignalError> {
    let mut join = SpotEventJoin::new();
    let mut observed = Vec::new();
    for input in script {
        match input {
            Input::Book {
                venue,
                receipt_ns,
                bid,
                bid_size,
                ask,
                ask_size,
            } => observed.extend(join.observe_book(
                *venue,
                *receipt_ns,
                decimal(bid),
                decimal(bid_size),
                decimal(ask),
                decimal(ask_size),
            )?),
            Input::Trade {
                venue,
                receipt_ns,
                event_time,
                price,
                size,
                aggressor,
            } => observed.extend(join.observe_trade(
                *venue,
                *receipt_ns,
                event_time,
                decimal(price),
                decimal(size),
                *aggressor,
            )?),
            Input::Flush => observed.extend(join.flush()),
        }
    }
    Ok(observed)
}

fn coinbase_trade(receipt_ns: u64, event_time: &'static str, price: &'static str) -> Input {
    Input::Trade {
        venue: Venue::Coinbase,
        receipt_ns,
        event_time,
        price,
        size: "1",
        aggressor: AggressorSide::Buy,
    }
}

fn coinbase_book(receipt_ns: u64) -> Input {
    Input::Book {
        venue: Venue::Coinbase,
        receipt_ns,
        bid: "100.01",
        bid_size: "5",
        ask: "100.03",
        ask_size: "7",
    }
}

// ---------------------------------------------------------------------------
// Invariant 1: the join clock is the admitted logical receipt time.
// ---------------------------------------------------------------------------

#[test]
fn every_observation_is_stamped_with_the_admitted_receipt_time() {
    // Two trades admitted at receipt 10, closed by a book admitted at 4_000;
    // then a Kraken book at 7. The join reads no clock of its own, so each
    // observation carries the receipt time its own events were admitted with,
    // never the closing event's and never a wall-clock reading.
    let observed = run(&[
        coinbase_trade(10, T0, "100.02"),
        coinbase_trade(10, T0, "100.04"),
        coinbase_book(4_000),
        Input::Book {
            venue: Venue::Kraken,
            receipt_ns: 7,
            bid: "100.01",
            bid_size: "5",
            ask: "100.03",
            ask_size: "7",
        },
    ])
    .expect("script joins");

    let stamps = observed
        .iter()
        .map(|observation| (observation.venue(), observation.receipt_ns()))
        .collect::<Vec<_>>();
    assert_eq!(
        stamps,
        vec![
            (Venue::Coinbase, 10),
            (Venue::Coinbase, 4_000),
            (Venue::Kraken, 7),
        ],
        "observations are stamped and ordered by admitted receipt time, not by \
         any clock the join reads"
    );
}

// ---------------------------------------------------------------------------
// Invariants 3 and 4: atomic batches, emitted before the book that closed them.
// ---------------------------------------------------------------------------

#[test]
fn a_trade_batch_closes_before_the_book_observation_that_follows_it() {
    let observed = run(&[
        coinbase_trade(10, T0, "100.02"),
        coinbase_trade(10, T0, "100.04"),
        coinbase_book(20),
    ])
    .expect("script joins");

    match observed.as_slice() {
        [
            JoinedObservation::Batch(batch),
            JoinedObservation::Book(book),
        ] => {
            assert_eq!(batch.venue, Venue::Coinbase);
            assert_eq!(batch.receipt_ns, 10);
            assert_eq!(
                batch.trades,
                vec![
                    trade("100.02", "1", AggressorSide::Buy),
                    trade("100.04", "1", AggressorSide::Buy),
                ],
                "batch trades keep admission order"
            );
            assert_eq!(book.receipt_ns, 20);
            // The exact shared mean of 100.01 and 100.03, no binary float.
            assert_eq!(book.midpoint, decimal("100.02"));
            assert_eq!(book.bid_size, decimal("5"));
            assert_eq!(book.ask_size, decimal("7"));
        }
        other => panic!("expected one batch then one book, got {other:?}"),
    }
}

#[test]
fn an_open_batch_is_never_observed_before_it_closes() {
    let mut join = SpotEventJoin::new();
    let produced = join
        .observe_trade(
            Venue::Coinbase,
            10,
            T0,
            decimal("100.02"),
            decimal("1"),
            AggressorSide::Buy,
        )
        .expect("trade joins");
    assert!(
        produced.is_empty(),
        "an opening trade produces no observation of its own"
    );
    assert!(join.has_open_batch());
    let flushed = join.flush();
    assert_eq!(flushed.len(), 1, "flush closes exactly the open batch");
    assert!(!join.has_open_batch());
    assert!(
        join.flush().is_empty(),
        "flushing twice produces nothing the second time"
    );
}

// ---------------------------------------------------------------------------
// Invariant 3: batch identity is (venue, event_time, receipt_ns).
// ---------------------------------------------------------------------------

#[test]
fn a_batch_closes_when_event_time_or_receipt_time_changes() {
    let observed = run(&[
        coinbase_trade(10, T0, "100.02"),
        // Same receipt, different provider event time -> a different batch.
        coinbase_trade(10, T1, "100.05"),
        // Same event time, later receipt -> a different batch again.
        coinbase_trade(11, T1, "100.06"),
        Input::Flush,
    ])
    .expect("script joins");

    let batches = observed
        .iter()
        .map(|observation| match observation {
            JoinedObservation::Batch(batch) => batch.trades.len(),
            JoinedObservation::Book(_) => panic!("no book in this script"),
        })
        .collect::<Vec<_>>();
    assert_eq!(batches, vec![1, 1, 1], "three separate one-trade batches");
}

#[test]
fn trades_sharing_one_identity_stay_in_one_batch() {
    let observed = run(&[
        coinbase_trade(10, T0, "100.02"),
        coinbase_trade(10, T0, "100.03"),
        coinbase_trade(10, T0, "100.04"),
        Input::Flush,
    ])
    .expect("script joins");
    match observed.as_slice() {
        [JoinedObservation::Batch(batch)] => assert_eq!(batch.trades.len(), 3),
        other => panic!("expected exactly one batch, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Invariant 2: per-venue monotone receipt time, venues independent.
// ---------------------------------------------------------------------------

#[test]
fn receipt_time_regression_is_refused_for_that_venue() {
    let mut join = SpotEventJoin::new();
    join.observe_book(
        Venue::Coinbase,
        100,
        decimal("100.01"),
        decimal("5"),
        decimal("100.03"),
        decimal("7"),
    )
    .expect("first book joins");
    let error = join
        .observe_trade(
            Venue::Coinbase,
            99,
            T0,
            decimal("100.02"),
            decimal("1"),
            AggressorSide::Buy,
        )
        .expect_err("an earlier receipt is refused");
    assert!(matches!(error, SpotSignalError::TimeRegression));
    assert!(!join.has_open_batch(), "a refused trade opens no batch");
    // Equal receipt times are admitted: they are the same admission instant.
    join.observe_trade(
        Venue::Coinbase,
        100,
        T0,
        decimal("100.02"),
        decimal("1"),
        AggressorSide::Buy,
    )
    .expect("an equal receipt is admitted");
}

#[test]
fn the_two_venues_do_not_constrain_or_close_each_other() {
    let mut join = SpotEventJoin::new();
    join.observe_trade(
        Venue::Kraken,
        500,
        T0,
        decimal("100.02"),
        decimal("1"),
        AggressorSide::Buy,
    )
    .expect("kraken trade joins");
    // A Coinbase book at an EARLIER receipt is fine — clocks are per venue —
    // and it must not close Kraken's open batch.
    let produced = join
        .observe_book(
            Venue::Coinbase,
            10,
            decimal("100.01"),
            decimal("5"),
            decimal("100.03"),
            decimal("7"),
        )
        .expect("coinbase book joins");
    assert_eq!(produced.len(), 1);
    assert!(matches!(produced[0], JoinedObservation::Book(_)));
    assert!(join.has_open_batch(), "kraken's batch is still open");

    let flushed = join.flush();
    assert_eq!(flushed.len(), 1);
    assert_eq!(flushed[0].venue(), Venue::Kraken);
}

#[test]
fn flush_closes_batches_in_a_fixed_venue_order() {
    let mut join = SpotEventJoin::new();
    // Open Kraken first, then Coinbase: the flush order must still be
    // Coinbase then Kraken, so the stream does not depend on arrival order.
    for venue in [Venue::Kraken, Venue::Coinbase] {
        join.observe_trade(
            venue,
            10,
            T0,
            decimal("100.02"),
            decimal("1"),
            AggressorSide::Buy,
        )
        .expect("trade joins");
    }
    let flushed = join.flush();
    assert_eq!(
        flushed
            .iter()
            .map(JoinedObservation::venue)
            .collect::<Vec<_>>(),
        vec![Venue::Coinbase, Venue::Kraken]
    );
}

// ---------------------------------------------------------------------------
// Invariant 5: explicit bounds, refusal instead of silent loss.
// ---------------------------------------------------------------------------

#[test]
fn an_over_full_batch_refuses_and_leaves_the_open_batch_intact() {
    let mut join = SpotEventJoin::new();
    for _ in 0..MAX_BATCH_TRADES {
        join.observe_trade(
            Venue::Coinbase,
            10,
            T0,
            decimal("100.02"),
            decimal("1"),
            AggressorSide::Buy,
        )
        .expect("trade inside the bound joins");
    }
    let error = join
        .observe_trade(
            Venue::Coinbase,
            10,
            T0,
            decimal("100.02"),
            decimal("1"),
            AggressorSide::Buy,
        )
        .expect_err("the bound refuses");
    assert!(matches!(error, SpotSignalError::ResourceLimit(_)));

    match join.flush().as_slice() {
        [JoinedObservation::Batch(batch)] => assert_eq!(
            batch.trades.len(),
            MAX_BATCH_TRADES,
            "the refusal dropped nothing and added nothing"
        ),
        other => panic!("expected the intact batch, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Invariant 6: staleness observed; and (invariant 5) a refusal changes no
// state.
// ---------------------------------------------------------------------------

#[test]
fn book_freshness_applies_the_shared_staleness_bound() {
    let mut join = SpotEventJoin::new();
    assert!(
        !join.book_fresh(Venue::Coinbase, 0),
        "a venue with no book is never fresh"
    );
    join.observe_book(
        Venue::Coinbase,
        1_000,
        decimal("100.01"),
        decimal("5"),
        decimal("100.03"),
        decimal("7"),
    )
    .expect("book joins");
    assert!(join.book_fresh(Venue::Coinbase, 1_000));
    assert!(join.book_fresh(Venue::Coinbase, 1_000 + STALENESS_BOUND_NS));
    assert!(!join.book_fresh(Venue::Coinbase, 1_001 + STALENESS_BOUND_NS));
    assert!(
        !join.book_fresh(Venue::Kraken, 1_000),
        "freshness is per venue"
    );
}

#[test]
fn the_prevailing_book_classification_splits_missing_from_stale() {
    // The driver needs the split: a cold start is not a gap, and only a gap is
    // a causal discontinuity. Freshness itself still comes from `book_fresh`.
    let mut join = SpotEventJoin::new();
    assert_eq!(
        join.prevailing_book(Venue::Coinbase, 0),
        PrevailingBook::Missing
    );
    join.observe_book(
        Venue::Coinbase,
        1_000,
        decimal("100.01"),
        decimal("5"),
        decimal("100.03"),
        decimal("7"),
    )
    .expect("book joins");
    assert_eq!(
        join.prevailing_book(Venue::Coinbase, 1_000 + STALENESS_BOUND_NS),
        PrevailingBook::Fresh
    );
    assert_eq!(
        join.prevailing_book(Venue::Coinbase, 1_001 + STALENESS_BOUND_NS),
        PrevailingBook::Stale
    );
    assert_eq!(
        join.prevailing_book(Venue::Kraken, 1_000),
        PrevailingBook::Missing,
        "the classification is per venue"
    );

    // A book observation is stamped with its PREDECESSOR, before it replaces it.
    let observed = join
        .observe_book(
            Venue::Coinbase,
            1_001 + STALENESS_BOUND_NS,
            decimal("100.01"),
            decimal("5"),
            decimal("100.03"),
            decimal("7"),
        )
        .expect("post-gap book joins");
    match observed.as_slice() {
        [JoinedObservation::Book(book)] => assert_eq!(book.prevailing, PrevailingBook::Stale),
        other => panic!("expected one book, got {other:?}"),
    }
}

#[test]
fn implausible_quotes_and_trades_are_refused_without_touching_state() {
    let mut join = SpotEventJoin::new();
    for (bid, bid_size, ask, ask_size) in [
        ("100.05", "5", "100.03", "7"),  // crossed
        ("0", "5", "100.03", "7"),       // non-positive bid
        ("100.01", "5", "0", "7"),       // non-positive ask
        ("100.01", "-1", "100.03", "7"), // negative size
    ] {
        let error = join
            .observe_book(
                Venue::Coinbase,
                10,
                decimal(bid),
                decimal(bid_size),
                decimal(ask),
                decimal(ask_size),
            )
            .expect_err("an implausible quote is refused");
        assert!(matches!(error, SpotSignalError::MarketValue(_)));
    }
    for (price, size) in [("0", "1"), ("100.02", "0"), ("-1", "1")] {
        let error = join
            .observe_trade(
                Venue::Coinbase,
                10,
                T0,
                decimal(price),
                decimal(size),
                AggressorSide::Buy,
            )
            .expect_err("an implausible trade is refused");
        assert!(matches!(error, SpotSignalError::MarketValue(_)));
    }
    assert!(join.last_book(Venue::Coinbase).is_none());
    assert!(!join.has_open_batch());
    // The refusals never advanced the venue clock, so an earlier receipt is
    // still admissible.
    join.observe_book(
        Venue::Coinbase,
        5,
        decimal("100.01"),
        decimal("5"),
        decimal("100.03"),
        decimal("7"),
    )
    .expect("an earlier receipt is still admissible after refusals");
}

#[test]
fn a_zero_top_size_is_wire_plausible_and_joins() {
    let mut join = SpotEventJoin::new();
    let produced = join
        .observe_book(
            Venue::Coinbase,
            10,
            decimal("100.01"),
            decimal("0"),
            decimal("100.03"),
            decimal("7"),
        )
        .expect("a zero displayed size is not an error");
    match produced.as_slice() {
        [JoinedObservation::Book(book)] => assert_eq!(book.bid_size, Decimal::ZERO),
        other => panic!("expected one book, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Invariant 8: no binary floating point.
// ---------------------------------------------------------------------------

#[test]
fn the_midpoint_is_the_exact_decimal_mean_not_a_binary_float() {
    // (0.1 + 0.2) / 2 is 0.15000000000000002 in binary floating point and
    // exactly 0.15 here; 100.01/100.02 needs a digit neither input carries.
    let mut join = SpotEventJoin::new();
    let observed = join
        .observe_book(
            Venue::Coinbase,
            1,
            decimal("0.1"),
            decimal("1"),
            decimal("0.2"),
            decimal("1"),
        )
        .expect("book joins");
    match observed.as_slice() {
        [JoinedObservation::Book(book)] => {
            assert_eq!(book.midpoint, decimal("0.15"));
            // Exact: in binary floating point 0.1 + 0.2 is not 0.3.
            assert_eq!(book.midpoint * Decimal::from(2), decimal("0.3"));
        }
        other => panic!("expected one book, got {other:?}"),
    }

    let observed = join
        .observe_book(
            Venue::Kraken,
            1,
            decimal("100.01"),
            decimal("1"),
            decimal("100.02"),
            decimal("1"),
        )
        .expect("book joins");
    match observed.as_slice() {
        [JoinedObservation::Book(book)] => {
            // A digit neither input carries, produced without rounding.
            assert_eq!(book.midpoint, decimal("100.015"));
            assert_eq!(book.midpoint * Decimal::from(2), decimal("200.03"));
        }
        other => panic!("expected one book, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Invariant 7: restart determinism.
// ---------------------------------------------------------------------------

fn determinism_script() -> Vec<Input> {
    vec![
        coinbase_book(10),
        coinbase_trade(20, T0, "100.02"),
        coinbase_trade(20, T0, "100.04"),
        Input::Trade {
            venue: Venue::Kraken,
            receipt_ns: 21,
            event_time: T0,
            price: "100.10",
            size: "2",
            aggressor: AggressorSide::Sell,
        },
        coinbase_book(30),
        Input::Book {
            venue: Venue::Kraken,
            receipt_ns: 31,
            bid: "100.09",
            bid_size: "3",
            ask: "100.11",
            ask_size: "4",
        },
        coinbase_trade(40, T1, "100.05"),
        Input::Flush,
    ]
}

#[test]
fn replaying_the_same_admitted_sequence_reproduces_the_observation_stream() {
    let script = determinism_script();
    let first = run(&script).expect("first run");
    let second = run(&script).expect("replay");
    assert_eq!(first, second);
    assert!(!first.is_empty());
}

#[test]
fn clearing_a_venue_returns_it_to_its_initial_state() {
    let mut join = SpotEventJoin::new();
    join.observe_book(
        Venue::Coinbase,
        1_000,
        decimal("100.01"),
        decimal("5"),
        decimal("100.03"),
        decimal("7"),
    )
    .expect("book joins");
    join.observe_trade(
        Venue::Coinbase,
        1_000,
        T0,
        decimal("100.02"),
        decimal("1"),
        AggressorSide::Buy,
    )
    .expect("trade joins");
    join.observe_trade(
        Venue::Kraken,
        1_000,
        T0,
        decimal("100.02"),
        decimal("1"),
        AggressorSide::Buy,
    )
    .expect("kraken trade joins");

    join.clear_venue(Venue::Coinbase);
    assert!(join.last_book(Venue::Coinbase).is_none());
    assert!(
        join.has_open_batch(),
        "clearing one venue leaves the other venue's batch open"
    );
    // The cleared venue accepts an earlier receipt again, and produces the same
    // stream a fresh join would.
    let after_gap = join
        .observe_book(
            Venue::Coinbase,
            1,
            decimal("100.01"),
            decimal("5"),
            decimal("100.03"),
            decimal("7"),
        )
        .expect("post-gap book joins");
    let fresh = SpotEventJoin::new()
        .observe_book(
            Venue::Coinbase,
            1,
            decimal("100.01"),
            decimal("5"),
            decimal("100.03"),
            decimal("7"),
        )
        .expect("fresh book joins");
    assert_eq!(after_gap, fresh);
}

// ---------------------------------------------------------------------------
// The driver: four computations, per venue, over the one join.
// ---------------------------------------------------------------------------

fn short_horizon_params() -> MicrostructureParams {
    MicrostructureParams {
        realized_horizon_ms: 1,
        window_trades: 1,
        sweep_min_levels: 3,
        ..MicrostructureParams::default()
    }
}

#[test]
fn each_venue_runs_its_own_book_resilience_state() {
    let mut set = CandidateMicrostructureSet::with_defaults();
    // Coinbase establishes a 10/10 top.
    set.observe_book(
        Venue::Coinbase,
        10,
        decimal("100.01"),
        decimal("10"),
        decimal("100.03"),
        decimal("10"),
    )
    .expect("coinbase book joins");
    // Kraken's much thinner top must not become Coinbase's baseline.
    set.observe_book(
        Venue::Kraken,
        11,
        decimal("100.01"),
        decimal("1"),
        decimal("100.03"),
        decimal("1"),
    )
    .expect("kraken book joins");
    // A 10% Coinbase drop is not a depletion; against Kraken's 1/1 it would be.
    let outcomes = set
        .observe_book(
            Venue::Coinbase,
            12,
            decimal("100.01"),
            decimal("9"),
            decimal("100.03"),
            decimal("9"),
        )
        .expect("coinbase book joins");
    match outcomes.as_slice() {
        [
            MicrostructureOutcome::Book {
                venue, resilience, ..
            },
        ] => {
            assert_eq!(*venue, Venue::Coinbase);
            assert_eq!(resilience.state, ResilienceState::Quiet);
            assert_eq!(resilience.depletion_side, DepletionSide::None);
        }
        other => panic!("expected one coinbase book outcome, got {other:?}"),
    }

    // The same venue really does detect its own depletion.
    let outcomes = set
        .observe_book(
            Venue::Coinbase,
            13,
            decimal("100.01"),
            decimal("4"),
            decimal("100.03"),
            decimal("9"),
        )
        .expect("coinbase book joins");
    match outcomes.as_slice() {
        [MicrostructureOutcome::Book { resilience, .. }] => {
            assert_eq!(resilience.state, ResilienceState::Recovering);
            assert_eq!(resilience.pre_depletion_size, decimal("9"));
        }
        other => panic!("expected one coinbase book outcome, got {other:?}"),
    }
}

#[test]
fn the_kyle_pair_uses_the_midpoint_change_that_follows_the_trade() {
    let mut set =
        CandidateMicrostructureSet::new(short_horizon_params()).expect("parameters are valid");
    // Baseline midpoint 100.
    set.observe_book(
        Venue::Coinbase,
        10,
        decimal("99"),
        decimal("5"),
        decimal("101"),
        decimal("5"),
    )
    .expect("book joins");
    // Trade of +1 base, then the midpoint moves to 101: pair (+1, +1).
    set.observe_trade(
        Venue::Coinbase,
        20,
        T0,
        decimal("100"),
        decimal("1"),
        AggressorSide::Buy,
    )
    .expect("trade joins");
    set.observe_book(
        Venue::Coinbase,
        30,
        decimal("100"),
        decimal("5"),
        decimal("102"),
        decimal("5"),
    )
    .expect("book joins");
    // Trade of +2 base, then the midpoint moves back to 100: pair (-1, +2).
    set.observe_trade(
        Venue::Coinbase,
        40,
        T1,
        decimal("101"),
        decimal("2"),
        AggressorSide::Buy,
    )
    .expect("trade joins");
    let outcomes = set
        .observe_book(
            Venue::Coinbase,
            50,
            decimal("99"),
            decimal("5"),
            decimal("101"),
            decimal("5"),
        )
        .expect("book joins");

    let kyle = outcomes
        .iter()
        .find_map(|outcome| match outcome {
            MicrostructureOutcome::Book { kyle, .. } => Some(kyle.clone()),
            MicrostructureOutcome::Batch { .. } | MicrostructureOutcome::NotReady { .. } => None,
        })
        .expect("a book outcome carries the kyle reading");
    match kyle {
        KyleOutcome::Ready(reading) => {
            // previous window = pair 1: (1 * 1) / (1 * 1) = 1
            assert_eq!(reading.kyle_lambda_previous, Decimal::ONE);
            // current window = pair 2: (-1 * 2) / (2 * 2) = -0.5
            assert_eq!(reading.kyle_lambda, decimal("-0.5"));
            assert_eq!(reading.lambda_drift, decimal("-1.5"));
            assert_eq!(reading.pair_count, 2);
        }
        KyleOutcome::NotReady { reason } => panic!("expected a ready reading, got {reason}"),
    }
}

#[test]
fn a_multi_level_batch_sweeps_and_resolves_its_realized_spreads() {
    let mut set =
        CandidateMicrostructureSet::new(short_horizon_params()).expect("parameters are valid");
    set.observe_book(
        Venue::Coinbase,
        10,
        decimal("99"),
        decimal("5"),
        decimal("101"),
        decimal("5"),
    )
    .expect("book joins");
    for price in ["100.10", "100.20", "100.30"] {
        set.observe_trade(
            Venue::Coinbase,
            20,
            T0,
            decimal(price),
            decimal("1"),
            AggressorSide::Buy,
        )
        .expect("trade joins");
    }
    // A book two milliseconds later closes the batch and passes the horizon.
    let outcomes = set
        .observe_book(
            Venue::Coinbase,
            20 + 2_000_000,
            decimal("100"),
            decimal("5"),
            decimal("102"),
            decimal("5"),
        )
        .expect("book joins");

    match outcomes.as_slice() {
        [
            MicrostructureOutcome::Batch { sweep, trades, .. },
            MicrostructureOutcome::Book { resolved, .. },
        ] => {
            assert_eq!(
                sweep.detection_state,
                SweepDetectionState::Sweep,
                "three distinct prices must sweep"
            );
            assert_eq!(sweep.direction, SweepDirection::Buy);
            assert_eq!(sweep.swept_levels, 3);
            assert_eq!(sweep.swept_size, decimal("3"));
            assert_eq!(trades.len(), 3, "one effective reading per trade");
            assert_eq!(
                resolved.len(),
                3,
                "the past-horizon book resolves all three realized measurements"
            );
            assert!(
                resolved
                    .iter()
                    .all(|measurement| measurement.realized.midpoint_at_horizon == decimal("100")),
                "the prevailing at-or-before-horizon midpoint is the pre-trade one"
            );
        }
        other => panic!("expected a batch then a book outcome, got {other:?}"),
    }
}

#[test]
fn clearing_a_venue_resets_its_computations_as_well_as_its_join() {
    let mut set = CandidateMicrostructureSet::with_defaults();
    set.observe_book(
        Venue::Coinbase,
        1_000,
        decimal("100.01"),
        decimal("10"),
        decimal("100.03"),
        decimal("10"),
    )
    .expect("book joins");
    set.clear_venue(Venue::Coinbase).expect("venue resets");
    assert!(set.join().last_book(Venue::Coinbase).is_none());

    // With the pre-gap top forgotten, the first post-gap book establishes a new
    // baseline and cannot itself be a depletion.
    let outcomes = set
        .observe_book(
            Venue::Coinbase,
            1,
            decimal("100.01"),
            decimal("1"),
            decimal("100.03"),
            decimal("1"),
        )
        .expect("post-gap book joins");
    match outcomes.as_slice() {
        [MicrostructureOutcome::Book { resilience, .. }] => {
            assert_eq!(resilience.state, ResilienceState::Quiet);
        }
        other => panic!("expected one book outcome, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// The driver: a joined trade batch is applied whole or not at all.
// ---------------------------------------------------------------------------

/// A driver with a single Coinbase midpoint of exactly 1, so a trade price is
/// also its return multiplier and an overflow is easy to place precisely.
fn driver_with_unit_midpoint() -> CandidateMicrostructureSet {
    let mut set = CandidateMicrostructureSet::with_defaults();
    set.observe_book(
        Venue::Coinbase,
        0,
        decimal("1"),
        decimal("1"),
        decimal("1"),
        decimal("1"),
    )
    .expect("baseline book joins");
    set
}

fn buy(set: &mut CandidateMicrostructureSet, event_time: &str, price: &str) {
    set.observe_trade(
        Venue::Coinbase,
        0,
        event_time,
        decimal(price),
        decimal("1"),
        AggressorSide::Buy,
    )
    .expect("trade joins");
}

#[test]
fn a_batch_refused_part_way_through_leaves_the_driver_byte_identical() {
    let mut set = driver_with_unit_midpoint();
    // One ordinary trade, then one whose effective spread overflows: against a
    // midpoint of 1, `2 * (price - 1) / 1 * 10_000` exceeds Decimal::MAX. The
    // refusal therefore lands on the SECOND trade of the batch, after the first
    // has already advanced effective_spread and kyle and after sweep has been
    // advanced for the batch as a whole.
    buy(&mut set, T1, "2");
    buy(&mut set, T1, "4000000000000000000000000");

    let before = set.clone();
    let error = set.flush().expect_err("the batch is refused");
    assert!(
        matches!(error, SpotSignalError::Arithmetic(_)),
        "expected an arithmetic refusal, got {error:?}"
    );
    assert_eq!(
        set, before,
        "a refused batch leaves the join and all four computations exactly as \
         they were"
    );

    // Whole-or-nothing keeps the offending batch open, so closing it again
    // refuses again — deterministically, not as a corrupted half-application.
    let error = set.flush().expect_err("the retained batch still refuses");
    assert!(matches!(error, SpotSignalError::Arithmetic(_)));
    assert_eq!(set, before, "the retry is equally whole-or-nothing");

    // The documented disposition is a venue reset, and it clears the batch.
    set.clear_venue(Venue::Coinbase)
        .expect("the venue resets after a refusal");
    assert!(!set.join().has_open_batch());
    set.observe_book(
        Venue::Coinbase,
        0,
        decimal("1"),
        decimal("1"),
        decimal("1"),
        decimal("1"),
    )
    .expect("the driver accepts work again after the reset");
}

#[test]
fn a_batch_refused_at_the_pending_bound_leaves_the_driver_byte_identical() {
    // The acceptance reproduction for the mid-batch atomicity finding: drive
    // one venue to MAX_PENDING_TRADES inside a batch. Receipt time never
    // advances, so no realized-spread horizon ever elapses and nothing drains.
    let mut set = driver_with_unit_midpoint();

    let full_batches = (MAX_PENDING_TRADES - 1) / MAX_BATCH_TRADES;
    let remainder = (MAX_PENDING_TRADES - 1) % MAX_BATCH_TRADES;
    for batch in 0..full_batches {
        let event_time = format!("ramp-{batch}");
        for _ in 0..MAX_BATCH_TRADES {
            buy(&mut set, &event_time, "2");
        }
        set.flush().expect("a ramp batch applies");
    }
    let event_time = format!("ramp-{full_batches}");
    for _ in 0..remainder {
        buy(&mut set, &event_time, "2");
    }
    set.flush().expect("the last ramp batch applies");
    // MAX_PENDING_TRADES - 1 measurements are now pending for Coinbase.

    // A two-trade batch: the first fills the bound, the second exceeds it.
    buy(&mut set, "bound", "2");
    buy(&mut set, "bound", "2");

    let before = set.clone();
    let error = set.flush().expect_err("the batch is refused at the bound");
    assert!(
        matches!(error, SpotSignalError::ResourceLimit(_)),
        "expected a resource-limit refusal, got {error:?}"
    );
    assert_eq!(
        set, before,
        "a batch refused mid-way leaves sweep, effective_spread, kyle and the \
         join byte-identical to their pre-call state"
    );
}

#[test]
fn a_trade_that_only_extends_the_open_batch_runs_no_computation() {
    // The per-trade path takes no restore point precisely because it can emit
    // nothing; this pins that property to the join predicate the driver asks.
    let mut set = driver_with_unit_midpoint();
    let outcomes = set
        .observe_trade(
            Venue::Coinbase,
            0,
            T1,
            decimal("2"),
            decimal("1"),
            AggressorSide::Buy,
        )
        .expect("the opening trade joins");
    assert!(outcomes.is_empty(), "an opening trade emits nothing");
    assert!(!set.join().trade_closes_open_batch(Venue::Coinbase, 0, T1));
    assert!(set.join().trade_closes_open_batch(Venue::Coinbase, 0, T0));

    let outcomes = set
        .observe_trade(
            Venue::Coinbase,
            0,
            T1,
            decimal("2"),
            decimal("1"),
            AggressorSide::Buy,
        )
        .expect("the extending trade joins");
    assert!(outcomes.is_empty(), "an extending trade emits nothing");
}

// ---------------------------------------------------------------------------
// The driver: the staleness gate (join invariant 6) is enforced, not advisory.
// ---------------------------------------------------------------------------

/// `T0 + 10 * STALENESS_BOUND_NS` receipt time, well past the bound.
const STALLED_NS: u64 = 1_000 + 10 * STALENESS_BOUND_NS;

fn set_with_book_at(receipt_ns: u64) -> CandidateMicrostructureSet {
    let mut set = CandidateMicrostructureSet::with_defaults();
    set.observe_book(
        Venue::Coinbase,
        receipt_ns,
        decimal("99"),
        decimal("10"),
        decimal("101"),
        decimal("10"),
    )
    .expect("baseline book joins");
    set
}

#[test]
fn a_trade_batch_against_a_stale_book_is_not_ready_instead_of_computing() {
    // The reproduction: a book at t0, a trade ten staleness bounds later. The
    // prevailing midpoint is long dead, so the batch must publish a not-ready
    // reason rather than price the trade against it.
    let mut set = set_with_book_at(1_000);
    set.observe_trade(
        Venue::Coinbase,
        STALLED_NS,
        T0,
        decimal("100"),
        decimal("1"),
        AggressorSide::Buy,
    )
    .expect("the trade joins");

    let outcomes = set.flush().expect("the stalled batch closes");
    match outcomes.as_slice() {
        [
            MicrostructureOutcome::NotReady {
                venue,
                receipt_ns,
                reason,
                reset,
            },
        ] => {
            assert_eq!(*venue, Venue::Coinbase);
            assert_eq!(*receipt_ns, STALLED_NS);
            assert_eq!(*reason, BOOK_STALE_REASON);
            // A withheld BATCH resets nothing: the four computations keep the
            // state they already held.
            assert!(!*reset);
        }
        other => panic!("expected one not-ready outcome, got {other:?}"),
    }

    // None of the four computations saw the batch: no realized measurement was
    // enqueued and no Kyle trade is waiting for a forward midpoint.
    assert_eq!(set.pending_realized_len(Venue::Coinbase), 0);
    assert_eq!(set.pending_kyle_len(Venue::Coinbase), 0);

    // A batch inside the bound against the same book still computes, so the
    // gate is the staleness bound and not a blanket refusal.
    let mut fresh = set_with_book_at(1_000);
    fresh
        .observe_trade(
            Venue::Coinbase,
            1_000 + STALENESS_BOUND_NS,
            T0,
            decimal("100"),
            decimal("1"),
            AggressorSide::Buy,
        )
        .expect("the trade joins");
    let outcomes = fresh.flush().expect("the in-bound batch closes");
    assert!(
        matches!(outcomes.as_slice(), [MicrostructureOutcome::Batch { .. }]),
        "a batch at exactly the bound is still fresh, got {outcomes:?}"
    );
    assert_eq!(fresh.pending_realized_len(Venue::Coinbase), 1);
}

#[test]
fn a_book_after_a_gap_resets_the_venue_instead_of_comparing_across_it() {
    let mut set = set_with_book_at(1_000);
    // A 10/10 top, then a 1/1 top ten bounds later. Compared naively that is a
    // 90% depletion; across a gap it is not a comparison at all.
    let outcomes = set
        .observe_book(
            Venue::Coinbase,
            STALLED_NS,
            decimal("99"),
            decimal("1"),
            decimal("101"),
            decimal("1"),
        )
        .expect("the post-gap book joins");
    match outcomes.as_slice() {
        [MicrostructureOutcome::NotReady { reason, .. }] => assert_eq!(*reason, BOOK_STALE_REASON),
        other => panic!("expected one not-ready outcome, got {other:?}"),
    }

    // The venue was reset, so the NEXT book cannot be compared against the
    // pre-gap top either: it establishes a new baseline and is quiet.
    let outcomes = set
        .observe_book(
            Venue::Coinbase,
            STALLED_NS + 1_000,
            decimal("99"),
            decimal("1"),
            decimal("101"),
            decimal("1"),
        )
        .expect("the first in-bound book after the gap joins");
    match outcomes.as_slice() {
        [MicrostructureOutcome::Book { resilience, .. }] => {
            assert_eq!(resilience.state, ResilienceState::Quiet);
            assert_eq!(resilience.depletion_side, DepletionSide::None);
        }
        other => panic!("expected one book outcome, got {other:?}"),
    }
}

#[test]
fn pending_state_stays_bounded_while_a_venue_book_is_stalled() {
    // Live trades against a stalled book must not accumulate: the gate refuses
    // the batch and drains what the Kyle estimator was still holding, so
    // neither pending series can grow toward its cap.
    let mut set = set_with_book_at(1_000);
    set.observe_trade(
        Venue::Coinbase,
        1_000,
        T0,
        decimal("100"),
        decimal("1"),
        AggressorSide::Buy,
    )
    .expect("the in-bound trade joins");
    set.flush().expect("the in-bound batch applies");
    assert_eq!(set.pending_realized_len(Venue::Coinbase), 1);
    assert_eq!(
        set.pending_kyle_len(Venue::Coinbase),
        1,
        "an in-bound trade waits for its forward midpoint"
    );

    // Now the book stalls and the trades keep coming, one batch each.
    for step in 0..64_u64 {
        let receipt_ns = STALLED_NS + step;
        set.observe_trade(
            Venue::Coinbase,
            receipt_ns,
            T1,
            decimal("100"),
            decimal("1"),
            AggressorSide::Buy,
        )
        .expect("the stalled trade joins");
        let outcomes = set.flush().expect("the stalled batch closes");
        assert!(
            matches!(
                outcomes.as_slice(),
                [MicrostructureOutcome::NotReady { .. }]
            ),
            "every stalled batch is not ready, got {outcomes:?}"
        );
        assert_eq!(
            set.pending_kyle_len(Venue::Coinbase),
            0,
            "the gate drained the Kyle pending trades and admitted no more"
        );
        assert_eq!(
            set.pending_realized_len(Venue::Coinbase),
            1,
            "no stalled batch enqueued a realized measurement"
        );
    }
}
