//! Deterministic per-venue raw-event join for the spot microstructure family.
//!
//! The four spot-microstructure candidates (`btc.external.effective_spread`,
//! `btc.external.kyle_lambda_drift`, `btc.external.sweep_detection`,
//! `btc.external.book_resilience`) all need the same thing the adopted spot
//! signals never needed: the raw `book-levels` and `trades` streams of ONE
//! venue interleaved into a single ordered observation series. Every one of
//! their formulas is same-venue (a trade is priced against *that* venue's
//! prevailing midpoint, a sweep is one venue's batch, a depletion compares one
//! venue's consecutive top-of-book updates), so the join is defined once here
//! rather than four times inside the computations.
//!
//! This module is the "derived-primitive interface" residual recorded for rows
//! 10–13 of `signals/btc/adoption/ADOPTION-candidates.md`. It is join-only: it
//! computes no signal value and embeds no definition. Its only consumer is the
//! driver in [`crate::candidate_spot_signals`], which each per-venue lane of
//! [`crate::spot_signals::SpotSignalProcessor`] owns since the 2026-09-02
//! per-venue adoption of the four measurements.
//!
//! # Invariants
//!
//! Each is enforced by this module and named by one test in
//! `tests/spot_event_join.rs`.
//!
//! 1. **The join clock is logical receipt time.** Every observation is ordered
//!    and stamped by the `logical_receipt_ns` the processor already admits
//!    events with ([`crate::spot_signals::SpotSignalProcessor::ingest`]). No
//!    wall clock is read anywhere in this module, so a replay of the same
//!    admitted sequence reproduces the same observation stream.
//! 2. **Per-venue monotonicity.** Receipt time is monotone non-decreasing per
//!    venue; a regression is [`SpotSignalError::TimeRegression`], never a
//!    silent reorder. The two venues are independent: Kraken's clock never
//!    constrains Coinbase's, mirroring the per-venue cursors in
//!    `market_state::VenueFlowSeries`.
//! 3. **Trade batches are atomic, and identified by
//!    `(venue, event_time, receipt_ns)`.** A trade extends the open batch only
//!    when all three match; any change closes the open batch first, and a batch
//!    leaves this module whole or not at all — never as a partial group. This
//!    is the "one ingested batch, same venue, one event timestamp" the sweep
//!    draft specifies. [`SpotEventJoin::trade_closes_open_batch`] is the single
//!    authority for whether a given trade closes the open batch.
//! 4. **Causal emission order.** Within one call the pending trade batch is
//!    emitted BEFORE the book observation that closed it. Trades admitted
//!    before a book update are therefore joined before it, so a forward
//!    midpoint change is never attributed to a trade that followed it — the
//!    alignment error round 16 of the dossier caught by hand-recomputation.
//! 5. **Explicit bounds.** State is at most one open batch and one retained
//!    book per venue; a batch holds at most [`MAX_BATCH_TRADES`] trades.
//!    Exceeding that bound is a [`SpotSignalError::ResourceLimit`] refusal that
//!    leaves the open batch untouched — never a silent drop, and never
//!    unbounded growth.
//! 6. **Staleness is classified on every observation, never substituted.**
//!    [`SpotEventJoin::book_fresh`] applies the shared [`STALENESS_BOUND_NS`]
//!    and is the one authority for that fact;
//!    [`SpotEventJoin::prevailing_book`] classifies with it, and every
//!    observation carries the resulting [`PrevailingBook`] as of the moment it
//!    was constructed — for a book observation, that is the state of the
//!    PREVIOUS retained book, stamped before this one replaces it. The
//!    classification is enforced, not merely offered: the driver in
//!    [`crate::candidate_spot_signals`] gates both arms of
//!    `CandidateMicrostructureSet::apply` on it and publishes
//!    `book_snapshot_stale_or_missing` instead of computing on a stale book.
//! 7. **Restart determinism.** The join is bounded derived state, is not
//!    checkpointed, and starts empty — exactly as `flow` and `ofi` are cleared
//!    by `SpotSignalProcessor::restore`. Replaying an admitted sequence into a
//!    fresh join yields a byte-identical observation stream, and a
//!    [`SpotEventJoin::clear_venue`] on reconnect returns that venue to its
//!    initial state.
//! 8. **No binary floating point.** The midpoint is the same exact
//!    `checked_mean` the adopted book metrics use, so the join introduces no
//!    second authority for that fact.
//!
//! # Caller preconditions
//!
//! These are obligations of the caller, not properties this module can check.
//! They are listed separately because a numbered invariant reads as enforced.
//!
//! - **Completed book groups only.** [`SpotEventJoin::observe_book`] takes four
//!   loose decimals and cannot observe a provider group's ingest outcome, so it
//!   cannot tell a completed group from a partially applied one. The caller
//!   must call it only for a group that ingested as `BookIngestOutcome::Complete`
//!   — the same rule `BookState::metrics` enforces on the adopted path by
//!   refusing to expose metrics while a group is still pending. The join
//!   validates only that the quote it is handed is positive and uncrossed.
//! - **Admitted events only, in admission order.** The caller passes the
//!   `logical_receipt_ns` the processor admitted the event with, in the order
//!   it admitted them; the join orders by that stamp and does not sort.
//! - **Flush before reading a derived value.** A batch stays open until
//!   something closes it, so a caller that evaluates at an arbitrary boundary
//!   calls [`SpotEventJoin::flush`] first (see [`SpotEventJoin::flush_venue`]).

use std::collections::BTreeMap;

use rust_decimal::Decimal;

use crate::Venue;
use crate::market_state::{STALENESS_BOUND_NS, checked_mean};
use crate::spot_signals::SpotSignalError;

/// Upper bound on the trades held in one open batch. A single provider batch
/// sharing one event timestamp is orders of magnitude smaller; the bound exists
/// so a malformed or hostile stream refuses instead of growing without limit.
pub const MAX_BATCH_TRADES: usize = 4_096;

/// The aggressing side of a trade (taker direction).
///
/// Defined here because both the join and every microstructure computation
/// need it; the spread-specific `2 * side_sign` factor stays with the
/// computation in [`crate::candidate_spot_signals`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggressorSide {
    Buy,
    Sell,
}

impl AggressorSide {
    /// `+1` for a buyer-aggressor, `-1` for a seller-aggressor.
    #[must_use]
    pub fn sign(self) -> Decimal {
        match self {
            Self::Buy => Decimal::ONE,
            Self::Sell => Decimal::NEGATIVE_ONE,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Buy => "buy",
            Self::Sell => "sell",
        }
    }
}

/// One aggressed trade inside a joined batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JoinedTrade {
    pub price: Decimal,
    pub size: Decimal,
    pub aggressor: AggressorSide,
}

impl JoinedTrade {
    /// Trade size times aggressor sign — the signed volume the Kyle-lambda
    /// estimator regresses on.
    pub fn signed_volume(self) -> Result<Decimal, SpotSignalError> {
        self.size
            .checked_mul(self.aggressor.sign())
            .ok_or_else(|| SpotSignalError::Arithmetic("signed trade size overflowed".to_owned()))
    }
}

/// The state of a venue's retained book at the moment an observation was
/// constructed, as judged by the one authority for that fact,
/// [`SpotEventJoin::book_fresh`].
///
/// It is stamped on the observation rather than recomputed by the consumer:
/// a book observation replaces the retained book as it is emitted, so by the
/// time a consumer sees the stream the prevailing book for an earlier
/// observation is already gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrevailingBook {
    /// A retained book inside [`STALENESS_BOUND_NS`] of this observation.
    Fresh,
    /// A retained book older than the bound: a gap separates this observation
    /// from it, so the two are not causally continuous.
    Stale,
    /// No retained book for this venue yet — a cold start, or the state after
    /// [`SpotEventJoin::clear_venue`].
    Missing,
}

/// A completed same-venue top-of-book observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JoinedBook {
    pub venue: Venue,
    pub receipt_ns: u64,
    /// The venue's PREVIOUS retained book as of this observation, stamped
    /// before this observation replaced it.
    pub prevailing: PrevailingBook,
    pub midpoint: Decimal,
    pub bid_size: Decimal,
    pub ask_size: Decimal,
}

/// A closed same-venue trade batch: every trade admitted under one event
/// timestamp and one receipt time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinedBatch {
    pub venue: Venue,
    pub receipt_ns: u64,
    /// The venue's retained book as of the batch's receipt time — the midpoint
    /// the batch's trades would be priced against.
    pub prevailing: PrevailingBook,
    pub trades: Vec<JoinedTrade>,
}

/// One joined observation in admission order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JoinedObservation {
    Batch(JoinedBatch),
    Book(JoinedBook),
}

impl JoinedObservation {
    #[must_use]
    pub fn venue(&self) -> Venue {
        match self {
            Self::Batch(batch) => batch.venue,
            Self::Book(book) => book.venue,
        }
    }

    #[must_use]
    pub fn receipt_ns(&self) -> u64 {
        match self {
            Self::Batch(batch) => batch.receipt_ns,
            Self::Book(book) => book.receipt_ns,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OpenBatch {
    event_time: String,
    receipt_ns: u64,
    trades: Vec<JoinedTrade>,
}

/// One venue's join state, captured by [`SpotEventJoin::snapshot_venue`] and
/// put back by [`SpotEventJoin::restore_venue`]. Its contents are the join's
/// own bounded state (invariant 5) and are deliberately not readable from
/// outside this module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VenueJoinSnapshot {
    venue: Venue,
    state: VenueJoin,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct VenueJoin {
    last_receipt_ns: Option<u64>,
    open_batch: Option<OpenBatch>,
    last_book: Option<JoinedBook>,
}

impl VenueJoin {
    fn observe_receipt(&mut self, receipt_ns: u64) -> Result<(), SpotSignalError> {
        if self
            .last_receipt_ns
            .is_some_and(|previous| receipt_ns < previous)
        {
            return Err(SpotSignalError::TimeRegression);
        }
        self.last_receipt_ns = Some(receipt_ns);
        Ok(())
    }
}

/// The per-venue raw-event join buffer.
///
/// Feed it completed book groups and admitted trade rows in admission order;
/// it returns the joined observation stream the microstructure computations
/// consume. See the module documentation for the invariants it holds.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SpotEventJoin {
    coinbase: VenueJoin,
    kraken: VenueJoin,
    /// Per-venue join buffers for the rights-gated venues (B1). A single-venue
    /// lane owns its own join and drives exactly one entry; the spot lane's join
    /// never touches these, so its behaviour is unchanged.
    extra: BTreeMap<Venue, VenueJoin>,
    /// A stable empty buffer returned by [`Self::venue`] for a rights-gated venue
    /// not yet observed, so the read accessor can stay infallible. Always
    /// default, so it never affects equality.
    empty: VenueJoin,
}

impl SpotEventJoin {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn venue_mut(&mut self, venue: Venue) -> &mut VenueJoin {
        match venue {
            Venue::Coinbase => &mut self.coinbase,
            Venue::Kraken => &mut self.kraken,
            other => self.extra.entry(other).or_default(),
        }
    }

    fn venue(&self, venue: Venue) -> &VenueJoin {
        match venue {
            Venue::Coinbase => &self.coinbase,
            Venue::Kraken => &self.kraken,
            other => self.extra.get(&other).unwrap_or(&self.empty),
        }
    }

    /// Whether joining a trade with this identity would CLOSE (and therefore
    /// emit) the venue's open batch: true only when that venue holds an open
    /// batch whose `(event_time, receipt_ns)` differs (invariant 3). A venue
    /// with no open batch, or a trade that extends the open one, closes
    /// nothing and so emits nothing.
    ///
    /// This is the single authority for that fact.
    /// [`Self::observe_trade`] decides with it, and a caller that must take a
    /// restore point before a step that can run downstream computations — the
    /// candidate driver in [`crate::candidate_spot_signals`] does — asks with
    /// it instead of re-deriving the comparison.
    #[must_use]
    pub fn trade_closes_open_batch(&self, venue: Venue, receipt_ns: u64, event_time: &str) -> bool {
        self.venue(venue)
            .open_batch
            .as_ref()
            .is_some_and(|batch| batch.event_time != event_time || batch.receipt_ns != receipt_ns)
    }

    /// Close the venue's open batch, if any, stamping it with the venue's
    /// retained book AS OF the batch's own receipt time (invariant 6). Every
    /// batch leaves the join through here, so the stamp is taken exactly once,
    /// before any book observation in the same call replaces the retained book.
    fn close_batch(&mut self, venue: Venue) -> Option<JoinedObservation> {
        let receipt_ns = self.venue(venue).open_batch.as_ref()?.receipt_ns;
        let prevailing = self.prevailing_book(venue, receipt_ns);
        let batch = self.venue_mut(venue).open_batch.take()?;
        Some(JoinedObservation::Batch(JoinedBatch {
            venue,
            receipt_ns: batch.receipt_ns,
            prevailing,
            trades: batch.trades,
        }))
    }

    /// Join a completed same-venue book group. Any open trade batch for that
    /// venue closes first (invariant 4), then the book observation follows.
    ///
    /// # Precondition
    ///
    /// The quote must come from a group that ingested as
    /// `BookIngestOutcome::Complete`. This function receives four loose
    /// decimals and cannot observe that outcome, so completeness is the
    /// caller's obligation (see the module's caller preconditions), not
    /// something checked here. What *is* checked is that the quote handed over
    /// is positive and uncrossed: such a quote is rejected rather than joined,
    /// because the adopted book state treats it as an invalid book rather than
    /// a midpoint.
    pub fn observe_book(
        &mut self,
        venue: Venue,
        receipt_ns: u64,
        bid: Decimal,
        bid_size: Decimal,
        ask: Decimal,
        ask_size: Decimal,
    ) -> Result<Vec<JoinedObservation>, SpotSignalError> {
        if bid <= Decimal::ZERO || ask <= Decimal::ZERO || bid > ask {
            return Err(SpotSignalError::MarketValue(
                "joined book quote must be positive and uncrossed".to_owned(),
            ));
        }
        if bid_size < Decimal::ZERO || ask_size < Decimal::ZERO {
            return Err(SpotSignalError::MarketValue(
                "joined book sizes cannot be negative".to_owned(),
            ));
        }
        let midpoint = checked_mean(bid, ask).ok_or_else(|| {
            SpotSignalError::Arithmetic("joined book midpoint is not representable".to_owned())
        })?;

        // Stamped BEFORE this book replaces the retained one, so it describes
        // the predecessor this observation would be compared against.
        let prevailing = self.prevailing_book(venue, receipt_ns);
        self.venue_mut(venue).observe_receipt(receipt_ns)?;
        let mut observations = Vec::with_capacity(2);
        if let Some(batch) = self.close_batch(venue) {
            observations.push(batch);
        }
        let book = JoinedBook {
            venue,
            receipt_ns,
            prevailing,
            midpoint,
            bid_size,
            ask_size,
        };
        self.venue_mut(venue).last_book = Some(book);
        observations.push(JoinedObservation::Book(book));
        Ok(observations)
    }

    /// Join one admitted trade row. A trade extends the open batch only when
    /// venue, event time and receipt time all match (invariant 3); otherwise
    /// the open batch closes and is returned before the new one opens.
    pub fn observe_trade(
        &mut self,
        venue: Venue,
        receipt_ns: u64,
        event_time: &str,
        price: Decimal,
        size: Decimal,
        aggressor: AggressorSide,
    ) -> Result<Vec<JoinedObservation>, SpotSignalError> {
        if price <= Decimal::ZERO || size <= Decimal::ZERO {
            return Err(SpotSignalError::MarketValue(
                "joined trade price and size must be positive".to_owned(),
            ));
        }
        let closes = self.trade_closes_open_batch(venue, receipt_ns, event_time);
        self.venue_mut(venue).observe_receipt(receipt_ns)?;

        let mut observations = Vec::with_capacity(1);
        if closes && let Some(batch) = self.close_batch(venue) {
            observations.push(batch);
        }
        let entry = self.venue_mut(venue);
        let trade = JoinedTrade {
            price,
            size,
            aggressor,
        };
        match &mut entry.open_batch {
            Some(batch) => {
                if batch.trades.len() >= MAX_BATCH_TRADES {
                    return Err(SpotSignalError::ResourceLimit(
                        "joined trade batch exceeded the bound".to_owned(),
                    ));
                }
                batch.trades.push(trade);
            }
            None => {
                entry.open_batch = Some(OpenBatch {
                    event_time: event_time.to_owned(),
                    receipt_ns,
                    trades: vec![trade],
                });
            }
        }
        Ok(observations)
    }

    /// Close the open batch of one venue, if any. Callers flush before reading
    /// a derived value so a batch is never half-observed at an evaluation
    /// boundary.
    pub fn flush_venue(&mut self, venue: Venue) -> Option<JoinedObservation> {
        self.close_batch(venue)
    }

    /// Close every open batch in a fixed venue order, so the flushed stream is
    /// identical on every run.
    pub fn flush(&mut self) -> Vec<JoinedObservation> {
        [Venue::Coinbase, Venue::Kraken]
            .into_iter()
            .filter_map(|venue| self.flush_venue(venue))
            .collect()
    }

    /// The most recent joined book observation for a venue.
    #[must_use]
    pub fn last_book(&self, venue: Venue) -> Option<JoinedBook> {
        self.venue(venue).last_book
    }

    /// Whether that venue's retained book is inside the shared staleness bound
    /// at `now_ns`. A venue that has never produced a book, or whose book is
    /// older than the bound, is not fresh.
    #[must_use]
    pub fn book_fresh(&self, venue: Venue, now_ns: u64) -> bool {
        self.venue(venue).last_book.is_some_and(|book| {
            now_ns
                .checked_sub(book.receipt_ns)
                .is_some_and(|age| age <= STALENESS_BOUND_NS)
        })
    }

    /// Classify the venue's retained book at `now_ns`. Freshness itself comes
    /// from [`Self::book_fresh`] — this adds only the missing/stale split that
    /// a consumer needs to tell a cold start from a gap, and never re-derives
    /// the bound.
    #[must_use]
    pub fn prevailing_book(&self, venue: Venue, now_ns: u64) -> PrevailingBook {
        if self.venue(venue).last_book.is_none() {
            PrevailingBook::Missing
        } else if self.book_fresh(venue, now_ns) {
            PrevailingBook::Fresh
        } else {
            PrevailingBook::Stale
        }
    }

    /// Whether either venue currently holds an unclosed batch.
    #[must_use]
    pub fn has_open_batch(&self) -> bool {
        self.coinbase.open_batch.is_some() || self.kraken.open_batch.is_some()
    }

    /// An opaque restore point for ONE venue's join state, so a caller that
    /// must undo a partly applied step pays for the venue it touched instead of
    /// the whole join. The two venues are independent (invariant 2), so
    /// restoring one never disturbs the other.
    #[must_use]
    pub fn snapshot_venue(&self, venue: Venue) -> VenueJoinSnapshot {
        VenueJoinSnapshot {
            venue,
            state: self.venue(venue).clone(),
        }
    }

    /// Put one venue back to the state its snapshot recorded.
    pub fn restore_venue(&mut self, snapshot: VenueJoinSnapshot) {
        *self.venue_mut(snapshot.venue) = snapshot.state;
    }

    /// Return one venue to its initial state. Used on reconnect or when the
    /// processor marks a venue unavailable: a stale open batch and a stale book
    /// must not survive a gap, because the events after the gap are not
    /// causally continuous with the events before it.
    pub fn clear_venue(&mut self, venue: Venue) {
        *self.venue_mut(venue) = VenueJoin::default();
    }

    /// Return both venues to their initial state.
    pub fn clear(&mut self) {
        self.coinbase = VenueJoin::default();
        self.kraken = VenueJoin::default();
    }
}
