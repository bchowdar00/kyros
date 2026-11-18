//! Polymarket opening-strike fallback (FUTURE-BUILDS E11 / ruling E65, as amended).
//!
//! Production Polymarket went dark because the venue's RTDS Chainlink TWAP-60s
//! stream skipped the exact top-of-window tick for six consecutive 5-minute
//! markets: the opening strike was defined as "the TWAP frame whose event_time
//! equals window_open exactly", so a market whose boundary frame never arrived
//! never received a strike, stayed `opening_strike_unavailable` for its whole
//! life, and produced zero ready batches (see the finding scratchpad
//! `smoke/pushA/polymarket-4194221.md`, market 4194221 @ 2026-09-04T18:55:00Z,
//! frames 18:54:59 -> 18:55:01 with 18:55:00 absent, and the four-market repeat
//! in `smoke/prod-poly-rotation.md`).
//!
//! The fallback keeps the exact frame as the primary source; if no exact frame
//! has arrived by `window_open + 2 s` it takes the last frame with
//! `event_time <= window_open` within 5 s before the open, else the first frame
//! with `event_time > window_open` within 5 s after it. The market publishes
//! **Ready** either way; the additive `opening_strike_source` field
//! (`exact` | `prior_frame` | `post_frame`) rides the wire on
//! `btc.polymarket.market_contract` and on every strike-derived Polymarket
//! identity so a consumer can filter an approximate strike itself. A strike is
//! fixed once per market: a later exact frame never revises a strike already
//! taken.
//!
//! ## Fixture provenance (§20)
//!
//! No local capture contains the authoritative prior+post slice (market 4194221
//! @ 18:55:00Z lives only on the box, `/home/admin/smoke/evidence-keep/
//! pushA-20260904T182953Z/polymarket/gen/journal.jsonl`). The `twap_price`
//! values below are transcribed verbatim from the one local capture that carries
//! real RTDS TWAP-60s frames — `scratchpad/smoke/wave2/polymarket/gen/journal.jsonl`
//! (session `smoke-polymarket-1788500400`, 2026-09-04, frames 05:40:01Z..05:54:58Z).
//! That capture genuinely lacks its 05:40:00Z boundary tick (the recording began
//! at 05:40:01Z), which is the real slice driving the `post_frame` case. The
//! `prior_frame` case replays the real contiguous run of frames the capture holds
//! in the five seconds before its 05:45:00Z boundary and lets the 2 s grace
//! elapse before the exact frame is delivered — reproducing the venue's skipped
//! boundary observed in production, never by deleting a record. No frame is
//! edited: each `twap_price` is a verbatim capture value; only the relative
//! second at which it is replayed is chosen so window_open falls where the tick
//! is absent.

use std::collections::BTreeMap;

use btc_signals::{
    COINBASE_TICKER_SOURCE_ID, COINBASE_TICKER_SOURCE_VERSION, KRAKEN_TICKER_SOURCE_ID,
    KRAKEN_TICKER_SOURCE_VERSION, POLYMARKET_MARKET_CONTRACT_ID, PredictionBatch,
    PredictionSignalProcessor, prediction_signal_count,
};
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use serde_json::{Value, json};
use signals_core::{PredictionMarketVenue, QualityState, SignalUpdate, SourceEvent};

const ONE_SECOND_NS: u64 = 1_000_000_000;

// Verbatim `twap_price` values from the wave2 capture (see provenance above).
/// 05:45:00.000Z — the exact-boundary frame.
const EXACT_PRICE: &str = "80959.539726581218934784";
/// Rounded to eight places, round-half-to-even, as `decimal_value` publishes it.
const EXACT_STRIKE: &str = "80959.53972658";
/// 05:44:57 / :58 / :59 — the real run before the 05:45:00Z boundary.
const PRIOR_PRICES: [&str; 3] = [
    "80958.730460862720507904",
    "80958.997549671134527488",
    "80959.266281405286449152",
];
/// The last of `PRIOR_PRICES` (05:44:59), rounded to eight places.
const PRIOR_STRIKE: &str = "80959.26628141";
/// 05:40:01.000Z — the first frame after the capture's genuinely-absent
/// 05:40:00Z boundary tick.
const POST_PRICE: &str = "80938.3726205074669568";
/// `POST_PRICE` rounded to eight places.
const POST_STRIKE: &str = "80938.37262051";

/// A market opening well after the process origin, so the five seconds before
/// the open are still non-negative logical time.
const OPEN: u64 = 1_000;

// A market that opened 90 s BEFORE the process origin (`wall_start` is
// 2026-08-15T12:00:00Z) and is still open — it closes 300 s after its own open,
// at 12:03:30Z. A process starting now lands 90 s into it, so `window_open`
// precedes the wall origin (the push-B mid-market-start regression).
const MID_OPEN: &str = "2026-08-15T11:58:30.000Z";
const MID_CLOSE: &str = "2026-08-15T12:03:30.000Z";
/// A real post-open TWAP frame one second after the pre-origin open.
const MID_POST_FRAME: &str = "2026-08-15T11:58:31.000Z";

#[test]
fn exact_boundary_frame_is_the_primary_source() {
    let mut processor = polymarket_processor("poly-e11-exact");
    processor
        .ingest(&twap(0, OPEN, EXACT_PRICE), ns(OPEN), ns(OPEN))
        .expect("exact-boundary TWAP is admitted");
    processor
        .bind_market(1, &contract(0, 1, OPEN), ns(OPEN), ns(OPEN))
        .expect("Polymarket market binds");
    let batch = processor
        .advance_time(ns(OPEN + 1))
        .expect("ready batch emits");

    assert_ready_market_with_source(&batch, &processor, "exact", EXACT_STRIKE);
}

#[test]
fn missing_exact_frame_falls_back_to_the_prior_frame_and_stays_ready() {
    let mut processor = polymarket_processor("poly-e11-prior");
    // The real run of frames in the five seconds before the open arrives; the
    // exact-boundary frame never does.
    for (offset, price) in PRIOR_PRICES.iter().enumerate() {
        let second = OPEN - 3 + offset as u64; // 997, 998, 999
        processor
            .ingest(&twap(offset as u64, second, price), ns(second), ns(second))
            .expect("pre-open TWAP is admitted");
    }
    processor
        .bind_market(1, &contract(0, 1, OPEN), ns(OPEN), ns(OPEN))
        .expect("Polymarket market binds");
    // Past the two-second grace with no exact frame: the strike falls back.
    let batch = processor
        .advance_time(ns(OPEN + 2))
        .expect("fallback batch emits");

    assert_ready_market_with_source(&batch, &processor, "prior_frame", PRIOR_STRIKE);
}

#[test]
fn missing_exact_and_prior_frames_fall_back_to_the_post_frame() {
    let mut processor = polymarket_processor("poly-e11-post");
    // The capture's real 05:40:00Z boundary: no frame at or before the open,
    // the first frame lands one second after it.
    processor
        .bind_market(1, &contract(0, 1, OPEN), ns(OPEN), ns(OPEN))
        .expect("Polymarket market binds");
    processor
        .ingest(&twap(0, OPEN + 1, POST_PRICE), ns(OPEN + 1), ns(OPEN + 1))
        .expect("post-open TWAP is admitted");
    let batch = processor
        .advance_time(ns(OPEN + 2))
        .expect("fallback batch emits");

    assert_ready_market_with_source(&batch, &processor, "post_frame", POST_STRIKE);
}

#[test]
fn the_two_second_grace_holds_the_strike_before_it_elapses() {
    let mut processor = polymarket_processor("poly-e11-grace");
    for (offset, price) in PRIOR_PRICES.iter().enumerate() {
        let second = OPEN - 3 + offset as u64;
        processor
            .ingest(&twap(offset as u64, second, price), ns(second), ns(second))
            .expect("pre-open TWAP is admitted");
    }
    processor
        .bind_market(1, &contract(0, 1, OPEN), ns(OPEN), ns(OPEN))
        .expect("Polymarket market binds");

    // One second after the open the grace has not elapsed: no strike yet, the
    // market is warming on `opening_strike_unavailable`, and no identity carries
    // the field (the pre-strike phase is byte-identical to today).
    let warming = processor
        .advance_time(ns(OPEN + 1))
        .expect("warming batch emits");
    let warming_contract = market_contract(&warming);
    assert_eq!(warming_contract.quality.state, QualityState::WarmingUp);
    assert!(
        warming_contract
            .quality
            .reasons
            .iter()
            .any(|reason| reason == "opening_strike_unavailable")
    );
    assert!(
        warming_contract.values.is_empty(),
        "no strike before the grace"
    );
    assert!(
        warming
            .iter()
            .all(|update| !update.values.contains_key("opening_strike_source"))
    );

    // At exactly `window_open + 2 s` the grace elapses and the prior-frame
    // fallback fires — Ready, flagged.
    let resolved = processor
        .advance_time(ns(OPEN + 2))
        .expect("fallback batch emits");
    assert_ready_market_with_source(&resolved, &processor, "prior_frame", PRIOR_STRIKE);
}

#[test]
fn a_strike_is_fixed_once_and_a_late_exact_frame_never_revises_it() {
    let mut processor = polymarket_processor("poly-e11-once");
    for (offset, price) in PRIOR_PRICES.iter().enumerate() {
        let second = OPEN - 3 + offset as u64;
        processor
            .ingest(&twap(offset as u64, second, price), ns(second), ns(second))
            .expect("pre-open TWAP is admitted");
    }
    processor
        .bind_market(1, &contract(0, 1, OPEN), ns(OPEN), ns(OPEN))
        .expect("Polymarket market binds");
    let first = processor
        .advance_time(ns(OPEN + 2))
        .expect("fallback batch emits");
    assert_eq!(
        market_contract(&first).values["opening_strike_source"],
        json!("prior_frame")
    );

    // The exact-boundary frame arrives late (event_time == window_open, admitted
    // on a later clock). It must not switch the strike mid-market.
    processor
        .ingest(&twap(9, OPEN, EXACT_PRICE), ns(OPEN + 3), ns(OPEN + 3))
        .expect("late exact TWAP is admitted");
    let after = processor
        .advance_time(ns(OPEN + 4))
        .expect("post-late-frame batch emits");
    assert_ready_market_with_source(&after, &processor, "prior_frame", PRIOR_STRIKE);
}

#[test]
fn an_approximate_strike_behaves_like_an_exact_one_apart_from_the_field() {
    // The whole point of the amendment: a fallback strike degrades nothing. The
    // approximate batch has the same per-identity (state, reasons) as the exact
    // batch, differing only in the market_contract's strike value and the
    // opening_strike_source the strike-derived identities carry.
    let exact = {
        let mut p = polymarket_processor("poly-e11-parity-exact");
        p.ingest(&twap(0, OPEN, EXACT_PRICE), ns(OPEN), ns(OPEN))
            .expect("exact TWAP admitted");
        p.bind_market(1, &contract(0, 1, OPEN), ns(OPEN), ns(OPEN))
            .expect("binds");
        p.advance_time(ns(OPEN + 2)).expect("batch")
    };
    let prior = {
        let mut p = polymarket_processor("poly-e11-parity-prior");
        for (offset, price) in PRIOR_PRICES.iter().enumerate() {
            let second = OPEN - 3 + offset as u64;
            p.ingest(&twap(offset as u64, second, price), ns(second), ns(second))
                .expect("pre-open TWAP admitted");
        }
        p.bind_market(1, &contract(0, 1, OPEN), ns(OPEN), ns(OPEN))
            .expect("binds");
        p.advance_time(ns(OPEN + 2)).expect("batch")
    };

    assert_eq!(exact.len(), prior.len());
    for (e, a) in exact.iter().zip(prior.iter()) {
        assert_eq!(e.signal_id, a.signal_id);
        assert_eq!(
            e.quality.state, a.quality.state,
            "{} state differs between exact and approximate",
            e.signal_id
        );
        assert_eq!(
            e.quality.reasons, a.quality.reasons,
            "{} reasons differ between exact and approximate",
            e.signal_id
        );
        // Wherever the exact run stamps `opening_strike_source=exact`, the
        // approximate run stamps `prior_frame` on the same identity, and neither
        // stamps it anywhere the other does not.
        match (
            e.values.get("opening_strike_source"),
            a.values.get("opening_strike_source"),
        ) {
            (Some(ev), Some(av)) => {
                assert_eq!(ev, &json!("exact"), "{}", e.signal_id);
                assert_eq!(av, &json!("prior_frame"), "{}", a.signal_id);
            }
            (None, None) => {}
            _ => panic!(
                "{} disagrees on carrying opening_strike_source",
                e.signal_id
            ),
        }
    }
    // At least the market_contract carries it in this market-only fixture.
    assert_eq!(
        market_contract(&exact).values["opening_strike_source"],
        json!("exact")
    );
    assert_eq!(
        market_contract(&prior).values["opening_strike_source"],
        json!("prior_frame")
    );
}

#[test]
fn a_market_that_opened_before_the_process_origin_resolves_without_error() {
    // push-B regression: a start that lands mid-market has window_open < wall_start.
    // The fallback's grace arithmetic must not convert that through logical_ns
    // (which rejects a pre-origin timestamp); advance_time must return Ok.
    let mut processor = polymarket_processor("poly-e11-midstart-post");
    processor
        .bind_market(1, &contract_at(0, 1, MID_OPEN, MID_CLOSE), 0, 0)
        .expect("binds a market that opened before the process origin");
    // Exact frame absent; a post-open frame within 5 s resolves the fallback.
    processor
        .ingest(&twap_at(0, MID_POST_FRAME, POST_PRICE), 0, 0)
        .expect("post-open TWAP admitted");
    let batch = processor
        .advance_time(ns(1))
        .expect("advance_time must not reject a pre-origin window_open");
    assert_ready_market_with_source(&batch, &processor, "post_frame", POST_STRIKE);
}

#[test]
fn a_pre_origin_market_with_the_exact_frame_is_unchanged() {
    // Exact frame present (event_time == window_open, before wall_start): the
    // primary path is a BTreeMap lookup that never touched logical_ns, so it is
    // byte-identical to pre-E65 — Ready, opening_strike_source=exact.
    let mut processor = polymarket_processor("poly-e11-midstart-exact");
    processor
        .bind_market(1, &contract_at(0, 1, MID_OPEN, MID_CLOSE), 0, 0)
        .expect("binds");
    processor
        .ingest(&twap_at(0, MID_OPEN, EXACT_PRICE), 0, 0)
        .expect("exact TWAP admitted");
    let batch = processor.advance_time(ns(1)).expect("emits");
    assert_ready_market_with_source(&batch, &processor, "exact", EXACT_STRIKE);
}

// --- helpers -------------------------------------------------------------

/// The market_contract instance in a pass.
fn market_contract(batch: &PredictionBatch) -> &SignalUpdate {
    batch
        .iter()
        .find(|update| update.signal_id == POLYMARKET_MARKET_CONTRACT_ID)
        .expect("the market_contract identity is always evaluated")
}

/// Assert the market published Ready with the expected strike and source, that
/// the fallback degraded nothing (no `opening_strike_unavailable` anywhere), and
/// that every Ready strike-derived identity carries the same source value.
fn assert_ready_market_with_source(
    batch: &PredictionBatch,
    processor: &PredictionSignalProcessor,
    source: &str,
    strike: &str,
) {
    assert_eq!(batch.len(), prediction_signal_count(processor.venue()));
    let contract = market_contract(batch);
    assert_eq!(
        contract.quality.state,
        QualityState::Ready,
        "an approximate strike still publishes Ready"
    );
    assert!(
        contract.quality.reasons.is_empty(),
        "a Ready market has no reasons"
    );
    assert_eq!(contract.values["strike"], json!(strike));
    assert_eq!(contract.values["opening_strike_source"], json!(source));
    for update in batch.iter() {
        assert!(
            !update
                .quality
                .reasons
                .iter()
                .any(|reason| reason == "opening_strike_unavailable"),
            "{} must not be strike-unavailable once a strike is fixed",
            update.signal_id
        );
        // Every identity that DOES carry the field carries this market's source.
        if let Some(value) = update.values.get("opening_strike_source") {
            assert_eq!(value, &json!(source), "{}", update.signal_id);
        }
    }
}

fn polymarket_processor(epoch: &str) -> PredictionSignalProcessor {
    PredictionSignalProcessor::new(PredictionMarketVenue::Polymarket, epoch, wall_start())
        .expect("Polymarket processor constructs")
}

fn wall_start() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-08-15T12:00:00Z")
        .expect("wall start parses")
        .with_timezone(&Utc)
}

fn timestamp(second: u64) -> String {
    wall_start()
        .checked_add_signed(Duration::seconds(
            i64::try_from(second).expect("test second fits i64"),
        ))
        .expect("timestamp is representable")
        .to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn ns(second: u64) -> u64 {
    second * ONE_SECOND_NS
}

fn source_event(
    source_id: &str,
    publisher_epoch: &str,
    sequence: u64,
    second: u64,
    mut values: BTreeMap<String, Value>,
) -> SourceEvent {
    let event_time = timestamp(second);
    values.insert("event_time".to_owned(), json!(event_time));
    SourceEvent {
        schema_version: 1,
        source_id: source_id.to_owned(),
        source_version: "1.0.0".to_owned(),
        publisher_epoch: publisher_epoch.to_owned(),
        sequence,
        event_time: event_time.clone(),
        received_at: event_time,
        values,
    }
}

fn twap(sequence: u64, second: u64, price: &str) -> SourceEvent {
    source_event(
        "polymarket.rtds.btc-usd.twap-60s",
        "poly-twap",
        sequence,
        second,
        BTreeMap::from([
            ("symbol".to_owned(), json!("btc/usd")),
            ("window_seconds".to_owned(), json!(60)),
            ("twap_price".to_owned(), json!(price)),
            ("published_at".to_owned(), json!(timestamp(second))),
        ]),
    )
}

/// A source event whose `event_time` is an explicit RFC3339 string (which may
/// precede `wall_start`), rather than an offset from the process origin.
fn source_event_at(
    source_id: &str,
    publisher_epoch: &str,
    sequence: u64,
    event_time: &str,
    mut values: BTreeMap<String, Value>,
) -> SourceEvent {
    values.insert("event_time".to_owned(), json!(event_time));
    SourceEvent {
        schema_version: 1,
        source_id: source_id.to_owned(),
        source_version: "1.0.0".to_owned(),
        publisher_epoch: publisher_epoch.to_owned(),
        sequence,
        event_time: event_time.to_owned(),
        received_at: event_time.to_owned(),
        values,
    }
}

fn twap_at(sequence: u64, event_time: &str, price: &str) -> SourceEvent {
    source_event_at(
        "polymarket.rtds.btc-usd.twap-60s",
        "poly-twap",
        sequence,
        event_time,
        BTreeMap::from([
            ("symbol".to_owned(), json!("btc/usd")),
            ("window_seconds".to_owned(), json!(60)),
            ("twap_price".to_owned(), json!(price)),
            ("published_at".to_owned(), json!(event_time)),
        ]),
    )
}

/// A Polymarket contract with an explicit `window_open`/`window_close` (so the
/// open can precede `wall_start`). The contract event's own `event_time` is the
/// open, matching `contract`.
fn contract_at(sequence: u64, market: u64, window_open: &str, window_close: &str) -> SourceEvent {
    source_event_at(
        "polymarket.btc.market-contracts",
        "poly-contracts",
        sequence,
        window_open,
        BTreeMap::from([
            ("market_id".to_owned(), json!(format!("pm-{market}"))),
            ("condition_id".to_owned(), json!(format!("cond-{market}"))),
            (
                "question_id".to_owned(),
                json!(format!("question-{market}")),
            ),
            ("slug".to_owned(), json!(format!("btc-updown-{market}"))),
            (
                "question".to_owned(),
                json!("Will BTC be at or above the opening value?"),
            ),
            ("up_token_id".to_owned(), json!(format!("up-{market}"))),
            ("down_token_id".to_owned(), json!(format!("down-{market}"))),
            ("window_open".to_owned(), json!(window_open)),
            ("window_close".to_owned(), json!(window_close)),
            (
                "resolution_source".to_owned(),
                json!("https://data.chain.link/streams/btc-usd-twap-60s-streams"),
            ),
            ("comparator".to_owned(), json!("greater_than_or_equal")),
            ("tie_outcome".to_owned(), json!("up")),
            ("minimum_tick_size".to_owned(), json!("0.01")),
            ("minimum_order_size".to_owned(), json!("5")),
            ("order_book_enabled".to_owned(), json!(true)),
            ("active".to_owned(), json!(true)),
            ("closed".to_owned(), json!(false)),
            ("rules_sha256".to_owned(), json!("a".repeat(64))),
        ]),
    )
}

fn contract(sequence: u64, market: u64, open_second: u64) -> SourceEvent {
    source_event(
        "polymarket.btc.market-contracts",
        "poly-contracts",
        sequence,
        open_second,
        BTreeMap::from([
            ("market_id".to_owned(), json!(format!("pm-{market}"))),
            ("condition_id".to_owned(), json!(format!("cond-{market}"))),
            (
                "question_id".to_owned(),
                json!(format!("question-{market}")),
            ),
            ("slug".to_owned(), json!(format!("btc-updown-{market}"))),
            (
                "question".to_owned(),
                json!("Will BTC be at or above the opening value?"),
            ),
            ("up_token_id".to_owned(), json!(format!("up-{market}"))),
            ("down_token_id".to_owned(), json!(format!("down-{market}"))),
            ("window_open".to_owned(), json!(timestamp(open_second))),
            (
                "window_close".to_owned(),
                json!(timestamp(open_second + 300)),
            ),
            (
                "resolution_source".to_owned(),
                json!("https://data.chain.link/streams/btc-usd-twap-60s-streams"),
            ),
            ("comparator".to_owned(), json!("greater_than_or_equal")),
            ("tie_outcome".to_owned(), json!("up")),
            ("minimum_tick_size".to_owned(), json!("0.01")),
            ("minimum_order_size".to_owned(), json!("5")),
            ("order_book_enabled".to_owned(), json!(true)),
            ("active".to_owned(), json!(true)),
            ("closed".to_owned(), json!(false)),
            ("rules_sha256".to_owned(), json!("a".repeat(64))),
        ]),
    )
}

// --- coverage: every declaring identity stamps the source ----------------

/// The set of Polymarket signal identities whose V1 definition declares the
/// additive `opening_strike_source` field, derived from the definitions on disk
/// so a newly-declaring identity is picked up automatically. Returns wire signal
/// ids (`btc.polymarket.<name>`, version stripped).
fn polymarket_identities_declaring_the_source() -> Vec<String> {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../definitions/btc/signals");
    let mut ids = Vec::new();
    for entry in std::fs::read_dir(dir).expect("signal definitions directory is readable") {
        let path = entry.expect("dir entry").path();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_owned();
        if !name.starts_with("btc.polymarket.") || !name.ends_with(".json") {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("definition file is readable");
        let value: Value = serde_json::from_str(&text).expect("definition is valid JSON");
        let declares = value
            .get("fields")
            .and_then(Value::as_array)
            .map(|fields| {
                fields
                    .iter()
                    .any(|f| f.get("name").and_then(Value::as_str) == Some("opening_strike_source"))
            })
            .unwrap_or(false);
        if declares {
            // Wire id is the file stem up to the `@version` marker.
            let id = name.split('@').next().unwrap_or(&name).to_owned();
            ids.push(id);
        }
    }
    ids.sort();
    ids
}

/// A fully warmed, fully bound Polymarket market on an EXACT opening strike: the
/// boundary TWAP arrives at `window_open`, the hosted spot reference is warmed
/// over 75 one-second ticks, and native quotes/book/trade land at second 60 so
/// the spot-embedding composites are Ready. Returns the batch after the strike is
/// fixed, in which every strike-derived identity must carry the source.
fn warmed_ready_batch(epoch: &str) -> (PredictionSignalProcessor, PredictionBatch) {
    let mut processor = polymarket_processor(epoch);
    // Exact boundary strike (event_time == window_open == second 0).
    processor
        .ingest(&twap(0, 0, "60000"), 0, 0)
        .expect("boundary TWAP is admitted");
    processor
        .bind_market(1, &contract(0, 1, 0), 0, 0)
        .expect("Polymarket market binds");
    for second in 1..=75u64 {
        // Hold the reference at 60000 and jump once on the last tick, so the
        // realized-volatility ring is non-zero (a flat series refuses the
        // spot-embedding composites with `volatility_zero`), exactly as the
        // prediction-signals reference fixtures warm their spot state.
        let price = if second == 75 { "60120" } else { "60000" };
        processor
            .ingest(
                &coinbase_ticker(second, price, second),
                ns(second),
                ns(second),
            )
            .expect("coinbase ticker admits");
        processor
            .ingest(
                &kraken_ticker(second, price, second),
                ns(second),
                ns(second),
            )
            .expect("kraken ticker admits");
        // Feed the native quote/book/trade on the LAST warm tick, so the
        // executable quote is still fresh at the second-76 decision (a quote fed
        // at second 60 would be stale and hold `executable_edge` in warm-up).
        if second == 75 {
            for event in [
                poly_quote(0, 1, "up", "0.45", "0.47", 75),
                poly_quote(1, 1, "down", "0.53", "0.55", 75),
            ] {
                processor
                    .ingest(&event, ns(75), ns(75))
                    .expect("native quote is admitted");
            }
            for event in [
                poly_book(0, "up-snapshot", 0, 4, "up", "bid", "0.45", "12", 75),
                poly_book(1, "up-snapshot", 1, 4, "up", "bid", "0.44", "8", 75),
                poly_book(2, "up-snapshot", 2, 4, "up", "ask", "0.47", "6", 75),
                poly_book(3, "up-snapshot", 3, 4, "up", "ask", "0.48", "10", 75),
                poly_book(4, "down-snapshot", 0, 4, "down", "bid", "0.53", "12", 75),
                poly_book(5, "down-snapshot", 1, 4, "down", "bid", "0.52", "8", 75),
                poly_book(6, "down-snapshot", 2, 4, "down", "ask", "0.55", "12", 75),
                poly_book(7, "down-snapshot", 3, 4, "down", "ask", "0.56", "8", 75),
            ] {
                processor
                    .ingest(&event, ns(75), ns(75))
                    .expect("native book row is admitted");
            }
            processor
                .ingest(&poly_trade(0, "up", "buy", "0.46", "2", 75), ns(75), ns(75))
                .expect("Polymarket trade is admitted");
        }
        processor
            .health_tick(ns(second))
            .expect("health tick emits a batch");
    }
    let batch = processor
        .advance_time(ns(76))
        .expect("warmed ready batch emits");
    (processor, batch)
}

/// Every Polymarket identity whose definition declares `opening_strike_source`
/// must actually emit it on the wire once a strike is fixed and the identity is
/// Ready. Deriving the set from the definitions means a dropped emission site
/// (an identity that declares the field but stops stamping it) fails here.
#[test]
fn every_declaring_polymarket_identity_stamps_the_source_on_the_wire() {
    let declaring = polymarket_identities_declaring_the_source();
    assert_eq!(
        declaring.len(),
        8,
        "expected exactly the 8 declaring Polymarket identities, got {declaring:?}"
    );

    let (_processor, batch) = warmed_ready_batch("poly-e11-coverage");

    let carrying: std::collections::BTreeSet<&str> = batch
        .iter()
        .filter(|u| u.values.contains_key("opening_strike_source"))
        .map(|u| u.signal_id.as_str())
        .collect();

    for id in &declaring {
        let update = batch
            .iter()
            .find(|u| &u.signal_id == id)
            .unwrap_or_else(|| panic!("{id} is not in the emitted batch"));
        assert_eq!(
            update.quality.state,
            QualityState::Ready,
            "{id} must be Ready in the warmed fixture so its source is on the wire"
        );
        assert_eq!(
            update.values.get("opening_strike_source"),
            Some(&json!("exact")),
            "{id} declares opening_strike_source but does not carry `exact` on the wire (dropped emission site?)"
        );
    }

    // No identity OUTSIDE the declaring set may leak the field.
    let declaring_set: std::collections::BTreeSet<&str> =
        declaring.iter().map(String::as_str).collect();
    assert_eq!(
        carrying, declaring_set,
        "exactly the declaring identities carry opening_strike_source on the wire"
    );
}

// --- helpers replicated for the warmed fixture ---------------------------

fn coinbase_ticker(sequence: u64, price: &str, second: u64) -> SourceEvent {
    SourceEvent {
        schema_version: 1,
        source_id: COINBASE_TICKER_SOURCE_ID.to_owned(),
        source_version: COINBASE_TICKER_SOURCE_VERSION.to_owned(),
        publisher_epoch: "coinbase-feed".to_owned(),
        sequence,
        event_time: timestamp(second),
        received_at: timestamp(second),
        values: BTreeMap::from([
            ("instrument".to_owned(), json!("BTC-USD")),
            ("trade_id".to_owned(), json!(100 + sequence)),
            ("price".to_owned(), json!(price)),
            ("last_size".to_owned(), json!("0.01")),
            ("best_bid".to_owned(), json!(price)),
            ("best_bid_size".to_owned(), json!("1.25")),
            ("best_ask".to_owned(), json!(price)),
            ("best_ask_size".to_owned(), json!("2.5")),
            ("open_24h".to_owned(), json!("59800")),
            ("volume_24h".to_owned(), json!("1250.5")),
            ("low_24h".to_owned(), json!("59000")),
            ("high_24h".to_owned(), json!("61000")),
            ("volume_30d".to_owned(), json!("30000")),
            ("event_time".to_owned(), json!(timestamp(second))),
            ("source_sequence".to_owned(), json!(900 + sequence)),
        ]),
    }
}

fn kraken_ticker(sequence: u64, price: &str, second: u64) -> SourceEvent {
    SourceEvent {
        schema_version: 1,
        source_id: KRAKEN_TICKER_SOURCE_ID.to_owned(),
        source_version: KRAKEN_TICKER_SOURCE_VERSION.to_owned(),
        publisher_epoch: "kraken-feed".to_owned(),
        sequence,
        event_time: timestamp(second),
        received_at: timestamp(second),
        values: BTreeMap::from([
            ("instrument".to_owned(), json!("BTC/USD")),
            ("best_bid".to_owned(), json!(price)),
            ("best_bid_size".to_owned(), json!("0.02")),
            ("best_ask".to_owned(), json!(price)),
            ("best_ask_size".to_owned(), json!("0.17")),
            ("last_price".to_owned(), json!(price)),
            ("volume_24h".to_owned(), json!("973.7")),
            ("vwap_24h".to_owned(), json!("60010")),
            ("low_24h".to_owned(), json!("59000")),
            ("high_24h".to_owned(), json!("61000")),
            ("change_24h".to_owned(), json!("-72.9")),
            ("change_percent_24h".to_owned(), json!("-0.11")),
            ("trade_count_24h".to_owned(), json!(46574)),
            ("update_kind".to_owned(), json!("update")),
            ("event_time".to_owned(), json!(timestamp(second))),
        ]),
    }
}

fn poly_quote(
    sequence: u64,
    market: u64,
    outcome: &str,
    bid: &str,
    ask: &str,
    second: u64,
) -> SourceEvent {
    let bid_decimal = bid.parse::<rust_decimal::Decimal>().expect("bid parses");
    let ask_decimal = ask.parse::<rust_decimal::Decimal>().expect("ask parses");
    source_event(
        "polymarket.btc.quotes",
        "poly-quotes",
        sequence,
        second,
        BTreeMap::from([
            ("condition_id".to_owned(), json!(format!("cond-{market}"))),
            ("asset_id".to_owned(), json!(format!("{outcome}-{market}"))),
            ("outcome".to_owned(), json!(outcome)),
            ("best_bid".to_owned(), json!(bid)),
            ("best_ask".to_owned(), json!(ask)),
            (
                "spread".to_owned(),
                json!((ask_decimal - bid_decimal).to_string()),
            ),
        ]),
    )
}

#[allow(clippy::too_many_arguments)]
fn poly_book(
    sequence: u64,
    group: &str,
    index: u32,
    count: u32,
    outcome: &str,
    side: &str,
    price: &str,
    size: &str,
    second: u64,
) -> SourceEvent {
    source_event(
        "polymarket.btc.book-levels",
        "poly-books",
        sequence,
        second,
        BTreeMap::from([
            ("condition_id".to_owned(), json!("cond-1")),
            ("asset_id".to_owned(), json!(format!("{outcome}-1"))),
            ("outcome".to_owned(), json!(outcome)),
            ("update_group".to_owned(), json!(group)),
            ("update_index".to_owned(), json!(index)),
            ("update_count".to_owned(), json!(count)),
            ("update_kind".to_owned(), json!("snapshot")),
            ("side".to_owned(), json!(side)),
            ("price".to_owned(), json!(price)),
            ("size".to_owned(), json!(size)),
        ]),
    )
}

fn poly_trade(
    sequence: u64,
    outcome: &str,
    side: &str,
    price: &str,
    size: &str,
    second: u64,
) -> SourceEvent {
    source_event(
        "polymarket.btc.trades",
        "poly-trades",
        sequence,
        second,
        BTreeMap::from([
            ("condition_id".to_owned(), json!("cond-1")),
            ("asset_id".to_owned(), json!(format!("{outcome}-1"))),
            ("outcome".to_owned(), json!(outcome)),
            ("price".to_owned(), json!(price)),
            ("size".to_owned(), json!(size)),
            ("fee_rate_bps".to_owned(), json!("0")),
            ("reported_side".to_owned(), json!(side)),
        ]),
    )
}
