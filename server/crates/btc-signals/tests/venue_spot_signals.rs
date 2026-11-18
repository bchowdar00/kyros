//! B1 slice-3 tests: the Binance and Bybit tier-1 spot siblings driven as the
//! real [`LaneProcessor`] implementations (`BinanceProcessor` / `BybitProcessor`)
//! that `LaneSpec::build_processor` routes to. These exercise the processor seam
//! itself — the `quote_currency` (R6) contract, per-venue emission isolation,
//! staleness, the Binance depth20 band-truncation deviation (spec §6 row 3),
//! checkpoint round-trip, the USDT/USD reference socket's telemetry and the
//! technical-analysis ready path.
//!
//! The lanes publish after every admitted input batch and coalesce unchanged
//! readings (`spot_signals::coalesce_publications`), so a single tick's output
//! is not one update per family; tests assert on the identities they drive
//! rather than on a full per-tick roster.
//!
//! The generic StreamRunner e2e seam and the R7 rights gate are covered by
//! `venue_lane.rs`; these tests own the processor arithmetic and its contract.

use std::collections::BTreeMap;

use btc_signals::lane::{LaneClock, LaneProcessor};
use btc_signals::venue_spot_signals::{BinanceProcessor, BybitProcessor};
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use rust_decimal::{Decimal, RoundingStrategy};
use serde_json::{Value, json};
use signals_core::{ProducerStatus, QualityState, SignalDefinition, SignalUpdate, SourceEvent};

const ONE_SECOND_NS: u64 = 1_000_000_000;

const FAMILIES: [&str; 23] = [
    "spot_quote",
    "spot_microprice",
    "actionable_depth_imbalance",
    "order_flow_imbalance",
    "aggressor_flow",
    "vwap_deviation",
    "realized_volatility",
    "micro_momentum",
    "horizon_momentum",
    "horizon_volatility",
    "liquidity_shock",
    "effective_spread",
    "kyle_lambda_drift",
    "sweep_detection",
    "book_resilience",
    // C3a (E51): the technical-analysis pack on the venue's own midpoint ring.
    "atr",
    "rsi",
    "roc",
    "ema_cross",
    "macd",
    "bollinger",
    "zscore",
    // C3b (ruling E51): the venue's update rate.
    "tick_velocity",
];

/// E62: Binance produces twelve derivatives families — the shared fourteen minus
/// `liquidation_pressure` and `perp_cvd`, whose only inputs were the deleted
/// `market`-socket sources. Bybit still produces all fourteen.
const BINANCE_DERIVATIVE_FAMILIES: usize = 12;
fn wall_start() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-08-13T12:00:00Z")
        .expect("wall start parses")
        .with_timezone(&Utc)
}

fn ns(second: u64) -> u64 {
    second * ONE_SECOND_NS
}

/// Build a `LaneClock` for a test tick. Venue processors derive produced-at from
/// their own construction `wall_start` and the journaled `now_ns`, so the clock's
/// `produced_at` is unused here; a value-emitting processor would read it.
fn clock_at(now_ns: u64) -> LaneClock {
    LaneClock {
        now_ns,
        produced_at: String::new(),
    }
}

fn timestamp(second: u64) -> String {
    wall_start()
        .checked_add_signed(Duration::seconds(
            i64::try_from(second).expect("second fits i64"),
        ))
        .expect("timestamp is representable")
        .to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn source_event(
    source_id: &str,
    publisher_epoch: &str,
    sequence: u64,
    second: u64,
    values: BTreeMap<String, Value>,
) -> SourceEvent {
    let event_time = timestamp(second);
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

fn update<'a>(updates: &'a [SignalUpdate], signal_id: &str) -> &'a SignalUpdate {
    updates
        .iter()
        .find(|update| update.signal_id == signal_id)
        .unwrap_or_else(|| panic!("missing output for {signal_id}"))
}

// --- Binance fixtures -----------------------------------------------------

/// One depth20 book level row. `image_kind` is carried for the definition
/// contract (§6 row 4); the engine treats a `snapshot` group as a full replace.
#[allow(clippy::too_many_arguments)]
fn binance_book_row(
    sequence: u64,
    index: u32,
    count: u32,
    side: &str,
    price: &str,
    size: &str,
    second: u64,
) -> SourceEvent {
    source_event(
        "binance.spot.btcusdt.book-levels",
        "binance-book",
        sequence,
        second,
        BTreeMap::from([
            ("instrument".to_owned(), json!("BTCUSDT")),
            ("update_group".to_owned(), json!(format!("bnb-{second}"))),
            ("update_index".to_owned(), json!(index)),
            ("update_count".to_owned(), json!(count)),
            ("update_kind".to_owned(), json!("snapshot")),
            ("image_kind".to_owned(), json!("full_replace")),
            ("side".to_owned(), json!(side)),
            ("price".to_owned(), json!(price)),
            ("size".to_owned(), json!(size)),
            ("last_update_id".to_owned(), json!(1_000 + sequence)),
            ("event_time_basis".to_owned(), json!("receipt")),
            ("event_time".to_owned(), json!(timestamp(second))),
        ]),
    )
}

/// A full multi-level depth20 image whose 5 bps band is inside the captured
/// levels, so the band-truncation guard stays clear (spec §6 row 3).
fn binance_book_group(seq_start: u64, second: u64) -> Vec<SourceEvent> {
    vec![
        binance_book_row(seq_start, 0, 4, "bid", "100", "2", second),
        binance_book_row(seq_start + 1, 1, 4, "bid", "99.90", "3", second),
        binance_book_row(seq_start + 2, 2, 4, "ask", "100.02", "1", second),
        binance_book_row(seq_start + 3, 3, 4, "ask", "100.12", "2", second),
    ]
}

fn binance_book_ticker(second: u64) -> SourceEvent {
    source_event(
        "binance.spot.btcusdt.book-ticker",
        "binance-ticker",
        second,
        second,
        BTreeMap::from([
            ("instrument".to_owned(), json!("BTCUSDT")),
            ("order_book_update_id".to_owned(), json!(5_000 + second)),
            ("best_bid".to_owned(), json!("100")),
            ("best_bid_size".to_owned(), json!("2")),
            ("best_ask".to_owned(), json!("100.02")),
            ("best_ask_size".to_owned(), json!("1")),
            ("event_time_basis".to_owned(), json!("receipt")),
            ("event_time".to_owned(), json!(timestamp(second))),
        ]),
    )
}

fn binance_agg_trade(sequence: u64, aggressor: &str, size: &str, second: u64) -> SourceEvent {
    source_event(
        "binance.spot.btcusdt.agg-trades",
        "binance-trades",
        sequence,
        second,
        BTreeMap::from([
            ("instrument".to_owned(), json!("BTCUSDT")),
            ("agg_trade_id".to_owned(), json!(9_000 + sequence)),
            ("first_trade_id".to_owned(), json!(20_000 + sequence)),
            ("last_trade_id".to_owned(), json!(20_000 + sequence)),
            ("price".to_owned(), json!("100.01")),
            ("quantity".to_owned(), json!(size)),
            ("buyer_is_maker".to_owned(), json!(aggressor == "sell")),
            ("aggressor_side".to_owned(), json!(aggressor)),
            ("trade_time".to_owned(), json!(timestamp(second))),
            ("event_time".to_owned(), json!(timestamp(second))),
        ]),
    )
}

/// Drive one Binance second: depth image, book-ticker quote, one aggressive
/// trade, then a health tick returning the interval frames.
/// A full Kraken USDT/USD reference ticker at parity (bid 0.99995, ask 1.00005 →
/// mid 1.0), so E47 conversion leaves the venue's numeric values unchanged and
/// these B1 warm-up assertions read the same figures; each converting identity
/// additionally carries `usdt_usd_rate` "1" and `usdt_usd_rate_age_ms` 0.
fn kraken_usdt_reference_ticker(second: u64) -> SourceEvent {
    // Parity: bid 0.99995 / ask 1.00005 → mid 1.0.
    kraken_usdt_reference_ticker_at(second, "0.99995", "1.00005")
}

/// A Kraken USDT/USD reference ticker with an explicit bid/ask, for tests that
/// need a non-unit rate.
fn kraken_usdt_reference_ticker_at(second: u64, bid: &str, ask: &str) -> SourceEvent {
    let event_time = timestamp(second);
    SourceEvent {
        schema_version: 1,
        source_id: "kraken.spot.usdt-usd.ticker".to_owned(),
        source_version: "1.0.0".to_owned(),
        publisher_epoch: "epoch".to_owned(),
        sequence: second,
        event_time: event_time.clone(),
        received_at: event_time.clone(),
        values: BTreeMap::from([
            ("instrument".to_owned(), json!("USDT/USD")),
            ("best_bid".to_owned(), json!(bid)),
            ("best_bid_size".to_owned(), json!("263826.27")),
            ("best_ask".to_owned(), json!(ask)),
            ("best_ask_size".to_owned(), json!("480014.16")),
            ("last_price".to_owned(), json!("1.00000")),
            ("volume_24h".to_owned(), json!("149118950.67")),
            ("vwap_24h".to_owned(), json!("0.99997")),
            ("low_24h".to_owned(), json!("0.99871")),
            ("high_24h".to_owned(), json!("1.00050")),
            ("change_24h".to_owned(), json!("0.00016")),
            ("change_percent_24h".to_owned(), json!("0.02")),
            ("trade_count_24h".to_owned(), json!(21617u64)),
            ("update_kind".to_owned(), json!("update")),
            ("event_time".to_owned(), json!(event_time)),
        ]),
    }
}

fn binance_second(processor: &mut BinanceProcessor, second: u64) -> Vec<SignalUpdate> {
    let now = ns(second);
    processor
        .on_events(
            "kraken_usdt_reference",
            &[kraken_usdt_reference_ticker(second)],
            &clock_at(now),
        )
        .expect("kraken usdt reference ingests");
    processor
        .on_events(
            "binance.spot.btcusdt.book-levels",
            &binance_book_group(4 * (second - 1), second),
            &clock_at(now),
        )
        .expect("binance book group ingests");
    processor
        .on_events(
            "binance.spot.btcusdt.book-ticker",
            &[binance_book_ticker(second)],
            &clock_at(now),
        )
        .expect("binance book ticker ingests");
    processor
        .on_events(
            "binance.spot.btcusdt.agg-trades",
            &[binance_agg_trade(second, "buy", "1", second)],
            &clock_at(now),
        )
        .expect("binance agg trade ingests");
    processor
        .on_interval(&clock_at(now))
        .expect("binance health tick succeeds")
}

// --- Bybit fixtures -------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn bybit_book_row(
    source_id: &str,
    epoch: &str,
    group: &str,
    sequence: u64,
    index: u32,
    count: u32,
    side: &str,
    price: &str,
    size: &str,
    second: u64,
) -> SourceEvent {
    source_event(
        source_id,
        epoch,
        sequence,
        second,
        BTreeMap::from([
            ("instrument".to_owned(), json!("BTCUSDT")),
            ("update_group".to_owned(), json!(group)),
            ("update_index".to_owned(), json!(index)),
            ("update_count".to_owned(), json!(count)),
            ("update_kind".to_owned(), json!("snapshot")),
            ("side".to_owned(), json!(side)),
            ("price".to_owned(), json!(price)),
            ("size".to_owned(), json!(size)),
            ("update_id".to_owned(), json!(1_000 + sequence)),
            ("cross_sequence".to_owned(), json!(7_000 + sequence)),
            ("event_time_basis".to_owned(), json!("source")),
            ("event_time".to_owned(), json!(timestamp(second))),
        ]),
    )
}

fn bybit_book_group(seq_start: u64, second: u64) -> Vec<SourceEvent> {
    let group = format!("byb-{second}");
    vec![
        bybit_book_row(
            "bybit.spot.btcusdt.book-levels",
            "bybit-book",
            &group,
            seq_start,
            0,
            4,
            "bid",
            "100",
            "2",
            second,
        ),
        bybit_book_row(
            "bybit.spot.btcusdt.book-levels",
            "bybit-book",
            &group,
            seq_start + 1,
            1,
            4,
            "bid",
            "99.90",
            "3",
            second,
        ),
        bybit_book_row(
            "bybit.spot.btcusdt.book-levels",
            "bybit-book",
            &group,
            seq_start + 2,
            2,
            4,
            "ask",
            "100.02",
            "1",
            second,
        ),
        bybit_book_row(
            "bybit.spot.btcusdt.book-levels",
            "bybit-book",
            &group,
            seq_start + 3,
            3,
            4,
            "ask",
            "100.12",
            "2",
            second,
        ),
    ]
}

/// A Bybit `orderbook.1` group. When `both_sides` is false only the bid row is
/// sent, exercising the not-ready-until-both-sides-seeded deviation (§6 row 1).
fn bybit_bbo_group(seq_start: u64, second: u64, both_sides: bool) -> Vec<SourceEvent> {
    let group = format!("byb-bbo-{second}");
    let count = if both_sides { 2 } else { 1 };
    let mut rows = vec![bybit_book_row(
        "bybit.spot.btcusdt.bbo-levels",
        "bybit-bbo",
        &group,
        seq_start,
        0,
        count,
        "bid",
        "100",
        "2",
        second,
    )];
    if both_sides {
        rows.push(bybit_book_row(
            "bybit.spot.btcusdt.bbo-levels",
            "bybit-bbo",
            &group,
            seq_start + 1,
            1,
            count,
            "ask",
            "100.02",
            "1",
            second,
        ));
    }
    rows
}

fn bybit_trade(sequence: u64, aggressor: &str, size: &str, second: u64) -> SourceEvent {
    source_event(
        "bybit.spot.btcusdt.trades",
        "bybit-trades",
        sequence,
        second,
        BTreeMap::from([
            ("instrument".to_owned(), json!("BTCUSDT")),
            ("trade_id".to_owned(), json!(format!("t-{sequence}"))),
            ("price".to_owned(), json!("100.01")),
            ("size".to_owned(), json!(size)),
            ("aggressor_side".to_owned(), json!(aggressor)),
            ("block_trade".to_owned(), json!(false)),
            ("rpi".to_owned(), json!(false)),
            ("cross_sequence".to_owned(), json!(8_000 + sequence)),
            ("event_time_basis".to_owned(), json!("source")),
            ("event_time".to_owned(), json!(timestamp(second))),
        ]),
    )
}

fn bybit_second(processor: &mut BybitProcessor, second: u64) -> Vec<SignalUpdate> {
    let now = ns(second);
    processor
        .on_events(
            "kraken_usdt_reference",
            &[kraken_usdt_reference_ticker(second)],
            &clock_at(now),
        )
        .expect("kraken usdt reference ingests");
    processor
        .on_events(
            "bybit.spot.btcusdt.book-levels",
            &bybit_book_group(4 * (second - 1), second),
            &clock_at(now),
        )
        .expect("bybit book group ingests");
    processor
        .on_events(
            "bybit.spot.btcusdt.bbo-levels",
            &bybit_bbo_group(2 * (second - 1), second, true),
            &clock_at(now),
        )
        .expect("bybit bbo group ingests");
    processor
        .on_events(
            "bybit.spot.btcusdt.trades",
            &[bybit_trade(second, "buy", "1", second)],
            &clock_at(now),
        )
        .expect("bybit trade ingests");
    processor
        .on_interval(&clock_at(now))
        .expect("bybit health tick succeeds")
}

// --- shared assertions ----------------------------------------------------

fn embedded_definition(signal_id: &str) -> &'static str {
    let path = format!("../../definitions/btc/signals/{signal_id}@1.0.0.json");
    EMBEDDED
        .iter()
        .find(|(id, _)| *id == signal_id)
        .map(|(_, raw)| *raw)
        .unwrap_or_else(|| panic!("no embedded definition for {signal_id} ({path})"))
}

macro_rules! embed {
    ($venue:literal, $family:literal) => {
        (
            concat!("btc.", $venue, ".", $family),
            include_str!(concat!(
                "../../../definitions/btc/signals/btc.",
                $venue,
                ".",
                $family,
                "@1.0.0.json"
            )),
        )
    };
}

const EMBEDDED: &[(&str, &str)] = &[
    embed!("binance", "spot_quote"),
    embed!("binance", "spot_microprice"),
    embed!("binance", "actionable_depth_imbalance"),
    embed!("binance", "order_flow_imbalance"),
    embed!("binance", "aggressor_flow"),
    embed!("binance", "vwap_deviation"),
    embed!("binance", "realized_volatility"),
    embed!("binance", "micro_momentum"),
    embed!("binance", "horizon_momentum"),
    embed!("binance", "horizon_volatility"),
    embed!("binance", "liquidity_shock"),
    embed!("binance", "effective_spread"),
    embed!("binance", "kyle_lambda_drift"),
    embed!("binance", "sweep_detection"),
    embed!("binance", "book_resilience"),
    embed!("binance", "tick_velocity"),
    embed!("bybit", "spot_quote"),
    embed!("bybit", "spot_microprice"),
    embed!("bybit", "actionable_depth_imbalance"),
    embed!("bybit", "order_flow_imbalance"),
    embed!("bybit", "aggressor_flow"),
    embed!("bybit", "vwap_deviation"),
    embed!("bybit", "realized_volatility"),
    embed!("bybit", "micro_momentum"),
    embed!("bybit", "horizon_momentum"),
    embed!("bybit", "horizon_volatility"),
    embed!("bybit", "liquidity_shock"),
    embed!("bybit", "effective_spread"),
    embed!("bybit", "kyle_lambda_drift"),
    embed!("bybit", "sweep_detection"),
    embed!("bybit", "book_resilience"),
    embed!("bybit", "tick_velocity"),
    // The seven derivatives families per venue (five B2 plus the two C2 perp
    // identities), so the converting set (presence
    // of `usdt_usd_rate`) can be derived from the definition for every identity.
    embed!("binance", "mark_index_basis_state"),
    embed!("binance", "funding_state"),
    embed!("binance", "open_interest_delta"),
    embed!("binance", "perp_momentum"),
    embed!("binance", "perp_volatility"),
    embed!("bybit", "mark_index_basis_state"),
    embed!("bybit", "funding_state"),
    embed!("bybit", "open_interest_delta"),
    embed!("bybit", "liquidation_pressure"),
    embed!("bybit", "perp_cvd"),
    // C3a (E51): the seven TA identities on each venue's spot midpoint ring and
    // the seven on its perpetual mark series.
    embed!("binance", "atr"),
    embed!("binance", "rsi"),
    embed!("binance", "roc"),
    embed!("binance", "ema_cross"),
    embed!("binance", "macd"),
    embed!("binance", "bollinger"),
    embed!("binance", "zscore"),
    embed!("bybit", "atr"),
    embed!("bybit", "rsi"),
    embed!("bybit", "roc"),
    embed!("bybit", "ema_cross"),
    embed!("bybit", "macd"),
    embed!("bybit", "bollinger"),
    embed!("bybit", "zscore"),
    embed!("binance", "perp_atr"),
    embed!("binance", "perp_rsi"),
    embed!("binance", "perp_roc"),
    embed!("binance", "perp_ema_cross"),
    embed!("binance", "perp_macd"),
    embed!("binance", "perp_bollinger"),
    embed!("binance", "perp_zscore"),
    embed!("bybit", "perp_atr"),
    embed!("bybit", "perp_rsi"),
    embed!("bybit", "perp_roc"),
    embed!("bybit", "perp_ema_cross"),
    embed!("bybit", "perp_macd"),
    embed!("bybit", "perp_bollinger"),
    embed!("bybit", "perp_zscore"),
    embed!("bybit", "perp_momentum"),
    embed!("bybit", "perp_volatility"),
];

// --- tests ----------------------------------------------------------------

#[test]
fn each_processor_publishes_only_its_own_venue_segment() {
    let mut binance =
        BinanceProcessor::new("binance-scope", wall_start()).expect("binance processor constructs");
    let mut bybit =
        BybitProcessor::new("bybit-scope", wall_start()).expect("bybit processor constructs");
    let binance_tick = (1..=61).fold(Vec::new(), |_, s| binance_second(&mut binance, s));
    let bybit_tick = (1..=61).fold(Vec::new(), |_, s| bybit_second(&mut bybit, s));
    for update in &binance_tick {
        assert!(
            update.signal_id.starts_with("btc.binance."),
            "binance lane emitted {}",
            update.signal_id
        );
    }
    for update in &bybit_tick {
        assert!(
            update.signal_id.starts_with("btc.bybit."),
            "bybit lane emitted {}",
            update.signal_id
        );
    }
}

#[test]
fn quote_currency_is_usd_on_every_ready_decision() {
    // R6/§8 item 6: the declared quote currency is present and constant `usd` (E47)
    // on every ready decision of all 26 identities, across both lanes.
    let mut binance =
        BinanceProcessor::new("binance-qc", wall_start()).expect("binance processor constructs");
    let mut bybit =
        BybitProcessor::new("bybit-qc", wall_start()).expect("bybit processor constructs");
    let binance_tick = (1..=61).fold(Vec::new(), |_, s| binance_second(&mut binance, s));
    let bybit_tick = (1..=61).fold(Vec::new(), |_, s| bybit_second(&mut bybit, s));
    let mut ready_seen = 0;
    for update in binance_tick.iter().chain(bybit_tick.iter()) {
        if update.quality.state == QualityState::Ready {
            ready_seen += 1;
            assert_eq!(
                update.values.get("quote_currency"),
                Some(&json!("usd")),
                "{} ready without quote_currency usd",
                update.signal_id
            );
        }
    }
    assert!(ready_seen > 0, "at least one ready decision is observed");
}

#[test]
fn stale_inputs_degrade_out_of_ready_after_the_staleness_bound() {
    let mut processor =
        BinanceProcessor::new("binance-stale", wall_start()).expect("binance processor constructs");
    for second in 1..=61 {
        binance_second(&mut processor, second);
    }
    // A health tick twelve seconds after the last input, with no new events,
    // is past the 10 s staleness bound: the quote-derived family must leave
    // ready.
    let stale_tick = processor
        .on_interval(&clock_at(ns(73)))
        .expect("late health tick succeeds");
    assert_ne!(
        update(&stale_tick, "btc.binance.spot_quote").quality.state,
        QualityState::Ready,
        "a stale quote must not stay ready"
    );
}

#[test]
fn binance_band_truncation_leaves_depth_families_not_ready() {
    // Spec §6 row 3: when the 5 bps band reaches the outermost captured depth20
    // level, actionable_depth_imbalance is not fully observed and must not be
    // ready. A single level per side puts the band edge beyond the book.
    let mut processor = BinanceProcessor::new("binance-truncated", wall_start())
        .expect("binance processor constructs");
    let narrow = vec![
        binance_book_row(0, 0, 2, "bid", "100", "2", 1),
        binance_book_row(1, 1, 2, "ask", "100.02", "1", 1),
    ];
    processor
        .on_events(
            "binance.spot.btcusdt.book-levels",
            &narrow,
            &clock_at(ns(1)),
        )
        .expect("narrow binance book ingests");
    let tick = processor
        .on_interval(&clock_at(ns(1)))
        .expect("health tick succeeds");
    assert_ne!(
        update(&tick, "btc.binance.actionable_depth_imbalance")
            .quality
            .state,
        QualityState::Ready,
        "a band that exceeds captured depth must not be ready"
    );
}

#[test]
fn checkpoint_state_round_trips_through_a_restored_processor() {
    // A resumed lane carries each feature's output lineage (lifetime count and
    // last quality) and the readiness memory, and re-warms its rolling state
    // from live input, so the per-run sequence numbering restarts at zero — the
    // same round-trip the spot processor's checkpoint keeps.
    let mut processor =
        BinanceProcessor::new("binance-ckpt", wall_start()).expect("binance processor constructs");
    for second in 1..=61 {
        binance_second(&mut processor, second);
    }
    let state = processor.checkpoint_state();
    let mut restored = BinanceProcessor::new("binance-ckpt-resume", wall_start())
        .expect("binance processor constructs");
    restored
        .restore_state(state.clone())
        .expect("checkpoint state restores");
    let resumed = restored.checkpoint_state();

    assert_eq!(resumed["ever_ready"], state["ever_ready"]);
    assert_eq!(resumed["last_now_ns"], state["last_now_ns"]);
    let features = state["features"].as_object().expect("features object");
    let resumed_features = resumed["features"].as_object().expect("features object");
    assert_eq!(resumed_features.len(), features.len());
    assert_eq!(
        resumed_features.len(),
        FAMILIES.len() + BINANCE_DERIVATIVE_FAMILIES
    );
    for (id, feature) in features {
        let after = &resumed_features[id];
        assert_eq!(
            after["lifetime_output_count"], feature["lifetime_output_count"],
            "{id}: lifetime output count carries across the resume"
        );
        assert_eq!(
            after["last_quality"], feature["last_quality"],
            "{id}: last quality carries across the resume"
        );
        assert_eq!(
            after["quality_reason_entries"], feature["quality_reason_entries"],
            "{id}: quality reason entries carry across the resume"
        );
        assert_eq!(
            after["previous_next_sequence"],
            json!(0),
            "{id}: per-run sequence numbering restarts on resume"
        );
    }
}

// --- USDT/USD reference socket telemetry ----------------------------------

#[test]
fn a_crossed_reference_frame_is_telemetry_not_a_lane_error() {
    // G1: a crossed / non-positive USDT/USD reference tick is routine on the thin
    // USDT/USD book. It must be counted as feed-health telemetry and leave the
    // prior rate in place — never take the venue's 18 identities down.
    let mut processor =
        BinanceProcessor::new("binance-g1", wall_start()).expect("binance processor constructs");
    // A valid reference sets the rate.
    processor
        .on_events(
            "kraken_usdt_reference",
            &[kraken_usdt_reference_ticker(1)],
            &clock_at(ns(1)),
        )
        .expect("a valid reference ingests");
    assert_eq!(
        processor.feed_health_snapshot().reference_frames_rejected,
        0
    );
    // A crossed reference (best_ask < best_bid) is REJECTED but does not error.
    processor
        .on_events(
            "kraken_usdt_reference",
            &[kraken_usdt_reference_ticker_at(2, "1.00005", "0.99995")],
            &clock_at(ns(2)),
        )
        .expect("a crossed reference frame does not kill the lane");
    assert_eq!(
        processor.feed_health_snapshot().reference_frames_rejected,
        1,
        "the crossed frame is counted as a rejection"
    );
    // The lane is still alive and reaches ready over the following seconds, with
    // the valid references admitted (the counter does not climb past the one
    // crossed frame).
    let mut ever_ready = false;
    for second in 3..=40 {
        if !binance_second(&mut processor, second).is_empty() {
            ever_ready = true;
        }
    }
    assert!(
        ever_ready,
        "the lane still publishes after a rejected reference frame"
    );
    assert_eq!(
        processor.feed_health_snapshot().reference_frames_rejected,
        1,
        "only the crossed frame was rejected; the valid frames were admitted"
    );
}

#[test]
fn a_reference_disconnect_does_not_degrade_the_venue_lane() {
    // G1 / check 2c: the reference socket is feed-health only. A reference
    // disconnect must never enter the venue's readiness or outage decision — the
    // reference simply ages into `usdt_reference_stale` at decision time.
    let mut processor = BinanceProcessor::new("binance-refconn", wall_start())
        .expect("binance processor constructs");
    for second in 1..=5 {
        binance_second(&mut processor, second);
    }
    assert_eq!(
        processor.producer_health().status,
        ProducerStatus::Ready,
        "the venue lane is ready once its spot sources are live"
    );
    processor
        .on_connectivity("kraken_usdt_reference", false, &clock_at(ns(6)))
        .expect("a reference disconnect admits");
    assert_eq!(
        processor.producer_health().status,
        ProducerStatus::Ready,
        "a reference disconnect does not degrade the venue lane"
    );
}

// --- C3a technical analysis: the ready path of the seven TA identities ------

/// Long enough for every C3a technical-analysis window to close: MACD needs
/// 26 + 9 one-minute bars (2 100 s) and the Z-score needs its fifteen-minute
/// sample ring. `assert_warm_tick` only reaches 61 s, where no TA window has
/// closed, so the TA ready path is unexercised without a run this long.
const TA_WARM_SECONDS: u64 = 2_160;

/// Each C3a technical-analysis identity and the price-level fields E47 converts
/// USDT→USD on it (`venue_quote_fields`). An empty list means the identity
/// publishes no quote-denominated field, so its whole value map must be
/// byte-identical at any reference rate.
const TA_PRICE_FIELDS: [(&str, &[&str]); 7] = [
    ("atr", &["atr", "close"]),
    ("rsi", &[]),
    ("roc", &[]),
    ("ema_cross", &["ema_9", "ema_21"]),
    ("macd", &[]),
    ("bollinger", &[]),
    ("zscore", &[]),
];

/// A book-ticker quote whose midpoint moves with the second, so no ATR, RSI,
/// Bollinger or Z-score window is degenerate (a flat window publishes
/// `flat_window`, not a value, and would never reach the ready path).
fn ta_quote_prices(second: u64) -> (String, String) {
    let step = second % 97;
    (format!("100.{step:02}"), format!("100.{:02}", step + 2))
}

fn binance_ta_book_ticker(second: u64) -> SourceEvent {
    let (bid, ask) = ta_quote_prices(second);
    source_event(
        "binance.spot.btcusdt.book-ticker",
        "binance-ticker",
        second,
        second,
        BTreeMap::from([
            ("instrument".to_owned(), json!("BTCUSDT")),
            ("order_book_update_id".to_owned(), json!(5_000 + second)),
            ("best_bid".to_owned(), json!(bid)),
            ("best_bid_size".to_owned(), json!("2")),
            ("best_ask".to_owned(), json!(ask)),
            ("best_ask_size".to_owned(), json!("1")),
            ("event_time_basis".to_owned(), json!("receipt")),
            ("event_time".to_owned(), json!(timestamp(second))),
        ]),
    )
}

fn bybit_ta_bbo_group(seq_start: u64, second: u64) -> Vec<SourceEvent> {
    let group = format!("byb-bbo-{second}");
    let (bid, ask) = ta_quote_prices(second);
    vec![
        bybit_book_row(
            "bybit.spot.btcusdt.bbo-levels",
            "bybit-bbo",
            &group,
            seq_start,
            0,
            2,
            "bid",
            &bid,
            "2",
            second,
        ),
        bybit_book_row(
            "bybit.spot.btcusdt.bbo-levels",
            "bybit-bbo",
            &group,
            seq_start + 1,
            1,
            2,
            "ask",
            &ask,
            "1",
            second,
        ),
    ]
}

/// Drive the Binance lane through a full TA warm-up at an explicit USDT/USD
/// reference bid/ask, returning the final interval tick.
fn binance_ta_run(reference_bid: &str, reference_ask: &str) -> Vec<SignalUpdate> {
    let mut processor =
        BinanceProcessor::new("binance-ta", wall_start()).expect("binance processor constructs");
    let mut last = Vec::new();
    for second in 1..=TA_WARM_SECONDS {
        let now = ns(second);
        processor
            .on_events(
                "kraken_usdt_reference",
                &[kraken_usdt_reference_ticker_at(
                    second,
                    reference_bid,
                    reference_ask,
                )],
                &clock_at(now),
            )
            .expect("kraken usdt reference ingests");
        processor
            .on_events(
                "binance.spot.btcusdt.book-levels",
                &binance_book_group(4 * (second - 1), second),
                &clock_at(now),
            )
            .expect("binance book group ingests");
        processor
            .on_events(
                "binance.spot.btcusdt.book-ticker",
                &[binance_ta_book_ticker(second)],
                &clock_at(now),
            )
            .expect("binance book ticker ingests");
        last = processor
            .on_interval(&clock_at(now))
            .expect("binance health tick succeeds");
    }
    last
}

/// The Bybit equivalent of [`binance_ta_run`].
fn bybit_ta_run(reference_bid: &str, reference_ask: &str) -> Vec<SignalUpdate> {
    let mut processor =
        BybitProcessor::new("bybit-ta", wall_start()).expect("bybit processor constructs");
    let mut last = Vec::new();
    for second in 1..=TA_WARM_SECONDS {
        let now = ns(second);
        processor
            .on_events(
                "kraken_usdt_reference",
                &[kraken_usdt_reference_ticker_at(
                    second,
                    reference_bid,
                    reference_ask,
                )],
                &clock_at(now),
            )
            .expect("kraken usdt reference ingests");
        processor
            .on_events(
                "bybit.spot.btcusdt.book-levels",
                &bybit_book_group(4 * (second - 1), second),
                &clock_at(now),
            )
            .expect("bybit book group ingests");
        processor
            .on_events(
                "bybit.spot.btcusdt.bbo-levels",
                &bybit_ta_bbo_group(2 * (second - 1), second),
                &clock_at(now),
            )
            .expect("bybit bbo group ingests");
        last = processor
            .on_interval(&clock_at(now))
            .expect("bybit health tick succeeds");
    }
    last
}

/// Assert one identity's ready frame against the same identity from a
/// parity-rate replay of the identical input series: it is Ready in both runs,
/// every declared price level is exactly `rate` times its parity value (8 dp,
/// round half to even), and every other field is byte-identical. A declared
/// price field missing from the value map fails here, so a rename in the E47
/// conversion table cannot pass silently.
fn assert_rate_scaled(
    parity: &SignalUpdate,
    scaled: &SignalUpdate,
    price_fields: &[&str],
    rate: Decimal,
) {
    let id = &scaled.signal_id;
    assert_eq!(
        parity.quality.state,
        QualityState::Ready,
        "{id}: ready on the parity run, reasons {:?}",
        parity.quality.reasons
    );
    assert_eq!(
        scaled.quality.state,
        QualityState::Ready,
        "{id}: ready on the non-parity run, reasons {:?}",
        scaled.quality.reasons
    );
    for field in price_fields {
        let before = decimal_field(parity, field);
        let after = decimal_field(scaled, field);
        assert_ne!(
            before,
            Decimal::ZERO,
            "{id}.{field}: a zero level would make the conversion unobservable"
        );
        assert_eq!(
            after,
            (before * rate).round_dp_with_strategy(8, RoundingStrategy::MidpointNearestEven),
            "{id}.{field}: converts USDT→USD at the reference rate"
        );
    }
    for (key, before) in &parity.values {
        if price_fields.contains(&key.as_str())
            || key == "usdt_usd_rate"
            || key == "usdt_usd_rate_age_ms"
        {
            continue;
        }
        assert_eq!(
            scaled.values.get(key),
            Some(before),
            "{id}.{key}: scale-free, must not move with the reference rate"
        );
    }
    assert_eq!(
        parity.values.len(),
        scaled.values.len(),
        "{id}: both runs publish the same field set"
    );
    if price_fields.is_empty() {
        assert!(
            !scaled.values.contains_key("usdt_usd_rate"),
            "{id}: a non-converting identity publishes no rate"
        );
    } else {
        assert_eq!(
            scaled.values.get("usdt_usd_rate"),
            Some(&json!("1.05")),
            "{id}: a converting identity publishes the rate it used"
        );
    }
}

fn decimal_field(update: &SignalUpdate, field: &str) -> Decimal {
    update
        .values
        .get(field)
        .unwrap_or_else(|| {
            panic!(
                "{}: declared price field {field} is published",
                update.signal_id
            )
        })
        .as_str()
        .unwrap_or_else(|| panic!("{}.{field} is a decimal string", update.signal_id))
        .parse()
        .unwrap_or_else(|_| panic!("{}.{field} parses as a decimal", update.signal_id))
}

/// C3a + E47 on the Binance spot lane: every technical-analysis identity reaches
/// Ready after a full warm-up, validates against its embedded definition, and
/// converts exactly the price levels the E47 table declares — at a non-parity
/// USDT/USD rate of 1.05.
#[test]
fn binance_spot_ta_reaches_ready_and_converts_price_levels() {
    let parity = binance_ta_run("0.99995", "1.00005");
    let scaled = binance_ta_run("1.04", "1.06");
    for (family, price_fields) in TA_PRICE_FIELDS {
        let signal_id = format!("btc.binance.{family}");
        let after = update(&scaled, &signal_id);
        let definition: SignalDefinition = serde_json::from_str(embedded_definition(&signal_id))
            .expect("embedded definition parses");
        after
            .validate_against(&definition)
            .unwrap_or_else(|error| panic!("{signal_id} validates: {error}"));
        assert_rate_scaled(
            update(&parity, &signal_id),
            after,
            price_fields,
            Decimal::new(105, 2),
        );
    }
}

/// The Bybit spot lane equivalent of
/// [`binance_spot_ta_reaches_ready_and_converts_price_levels`].
#[test]
fn bybit_spot_ta_reaches_ready_and_converts_price_levels() {
    let parity = bybit_ta_run("0.99995", "1.00005");
    let scaled = bybit_ta_run("1.04", "1.06");
    for (family, price_fields) in TA_PRICE_FIELDS {
        let signal_id = format!("btc.bybit.{family}");
        let after = update(&scaled, &signal_id);
        let definition: SignalDefinition = serde_json::from_str(embedded_definition(&signal_id))
            .expect("embedded definition parses");
        after
            .validate_against(&definition)
            .unwrap_or_else(|error| panic!("{signal_id} validates: {error}"));
        assert_rate_scaled(
            update(&parity, &signal_id),
            after,
            price_fields,
            Decimal::new(105, 2),
        );
    }
}
