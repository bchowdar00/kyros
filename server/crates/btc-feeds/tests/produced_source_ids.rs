//! F1 completeness guard for [`CaptureStream::produced_source_ids`].
//!
//! Two checks keep the static inventory honest:
//! 1. every lane-driven stream's inventory equals its adapter's constant set
//!    (a regression to a partial inventory fails here), and
//! 2. replaying a bounded excerpt of each observed capture proves every
//!    `source_id` the adapter actually emits is listed in the inventory — so the
//!    lane runtime's observed-not-consumed routing cannot silently mis-route a
//!    real source.

use std::collections::BTreeSet;

use btc_feeds::replay::ReplaySession;
use btc_feeds::{CaptureStream, RawRecord};

/// The lane-driven streams and their exact expected inventories, pinned to the
/// adapters' `SOURCE_ID` constants. A change to either side must be reconciled
/// here, so a partial `produced_source_ids` arm fails rather than mis-routing.
fn lane_driven_expected() -> Vec<(CaptureStream, Vec<&'static str>)> {
    vec![
        (
            CaptureStream::BinanceSpot,
            vec![
                "binance.spot.btcusdt.book-levels",
                "binance.spot.btcusdt.agg-trades",
                "binance.spot.btcusdt.book-ticker",
            ],
        ),
        (
            CaptureStream::BinanceUsdmPublic,
            vec!["binance.usdm.btcusdt.book-levels"],
        ),
        (
            CaptureStream::BinanceUsdmRest,
            vec![
                "binance.usdm.btcusdt.open-interest",
                "binance.usdm.btcusdt.open-interest-hist",
                "binance.usdm.btcusdt.funding-info",
                "binance.usdm.btcusdt.premium-index",
            ],
        ),
        (
            CaptureStream::BybitSpot,
            vec![
                "bybit.spot.btcusdt.trades",
                "bybit.spot.btcusdt.book-levels",
                "bybit.spot.btcusdt.bbo-levels",
            ],
        ),
        (
            CaptureStream::BybitLinear,
            vec![
                "bybit.linear.btcusdt.liquidations",
                "bybit.linear.btcusdt.trades",
                "bybit.linear.btcusdt.book-levels",
                "bybit.linear.btcusdt.tickers",
            ],
        ),
        (
            CaptureStream::BybitLinearRest,
            vec![
                "bybit.linear.btcusdt.open-interest",
                "bybit.linear.btcusdt.funding-history",
            ],
        ),
        (
            CaptureStream::KrakenUsdtReference,
            vec!["kraken.spot.usdt-usd.ticker", "kraken.spot.status"],
        ),
    ]
}

#[test]
fn lane_driven_inventories_match_the_adapter_constants() {
    for (stream, expected) in lane_driven_expected() {
        let inventory: BTreeSet<&str> = stream.produced_source_ids().iter().copied().collect();
        let expected_set: BTreeSet<&str> = expected.iter().copied().collect();
        assert_eq!(
            inventory,
            expected_set,
            "{} inventory disagrees with its adapter constants",
            stream.as_str()
        );
    }
}

/// Replay a bounded excerpt of each observed capture and return the set of
/// `source_id`s the adapter emitted.
fn emitted_source_ids(stream: CaptureStream, capture: &str) -> BTreeSet<String> {
    let mut session = ReplaySession::new(stream, "fixture-epoch", None).expect("session builds");
    let mut emitted = BTreeSet::new();
    for (index, line) in capture.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let record: RawRecord = serde_json::from_str(line)
            .unwrap_or_else(|error| panic!("{} record {index} parses: {error}", stream.as_str()));
        let outcome = session
            .ingest(&record)
            .unwrap_or_else(|error| panic!("{} record {index} replays: {error}", stream.as_str()));
        for event in outcome.events {
            emitted.insert(event.source_id);
        }
    }
    emitted
}

#[test]
fn every_emitted_source_id_is_in_the_inventory() {
    // (stream, fixture bytes, whether the excerpt must emit at least one event).
    let cases: Vec<(CaptureStream, &str, bool)> = vec![
        (
            CaptureStream::BinanceSpot,
            include_str!("fixtures/observed/binance_spot.jsonl"),
            true,
        ),
        (
            CaptureStream::BinanceUsdmPublic,
            include_str!("fixtures/observed/binance_usdm_public.jsonl"),
            true,
        ),
        (
            CaptureStream::BinanceUsdmRest,
            include_str!("fixtures/observed/binance_usdm_rest.jsonl"),
            true,
        ),
        (
            CaptureStream::BybitSpot,
            include_str!("fixtures/observed/bybit_spot.jsonl"),
            true,
        ),
        (
            CaptureStream::BybitLinear,
            include_str!("fixtures/observed/bybit_linear.jsonl"),
            true,
        ),
        (
            CaptureStream::BybitLinearRest,
            include_str!("fixtures/observed/bybit_linear_rest.jsonl"),
            true,
        ),
        (
            CaptureStream::KrakenCore,
            include_str!("fixtures/observed/kraken_core.jsonl"),
            false,
        ),
        // Head-200 excerpt of coinbase-core-r2-20260904T010142Z.jsonl (lines
        // 1-200 of 107,244). Exercises the USDT/USD ticker source the inventory
        // was missing (raw.rs `CoinbaseCore`).
        (
            CaptureStream::CoinbaseCore,
            include_str!("fixtures/observed/coinbase_core.jsonl"),
            true,
        ),
        // Head-200 excerpt of kalshi-index-r4-20260903T100124Z.jsonl (lines
        // 1-200 of 23,403).
        (
            CaptureStream::KalshiIndex,
            include_str!("fixtures/observed/kalshi_index.jsonl"),
            true,
        ),
        // Full observed hour chainlink-aggregator-r1-20260903T064148Z.jsonl
        // (lines 1-130, the whole capture).
        (
            CaptureStream::ChainlinkAggregator,
            include_str!("fixtures/observed/chainlink_aggregator.jsonl"),
            true,
        ),
        // Head-200 excerpt of pyth-hermes-stream-r1-20260904T012850Z.jsonl
        // (lines 1-200 of 3,662). The oracle SSE seam refuses this stream in the
        // live lane, so the inventory is proved only by this replay.
        (
            CaptureStream::PythHermesStream,
            include_str!("fixtures/observed/pyth_hermes_stream.jsonl"),
            true,
        ),
        // Head-200 excerpt of pyth-hermes-rest-r1-20260904T012850Z.jsonl (lines
        // 1-200 of 781). Same oracle-poll refusal as the SSE stream.
        (
            CaptureStream::PythHermesRest,
            include_str!("fixtures/observed/pyth_hermes_rest.jsonl"),
            true,
        ),
        // E47(e): the reference socket must prove it emits BOTH the USDT ticker
        // and the `kraken.spot.status` frame Kraken pushes on connect — the
        // sidecar source the F1 routing was refusing. The fixture is a bounded
        // excerpt of a real Kraken WebSocket hour (status + USDT/USD ticker
        // frames, re-tagged to the reference socket) and must emit at least one
        // event.
        // provenance: see FUTURE-BUILDS — the source capture for this re-tagged
        // fixture is not present under scratchpad/observed/, so its exact capture
        // file and line range could not be recorded here.
        (
            CaptureStream::KrakenUsdtReference,
            include_str!("fixtures/observed/kraken_usdt_reference.jsonl"),
            true,
        ),
    ];
    for (stream, capture, must_emit) in cases {
        let inventory: BTreeSet<&str> = stream.produced_source_ids().iter().copied().collect();
        let emitted = emitted_source_ids(stream, capture);
        if must_emit {
            assert!(
                !emitted.is_empty(),
                "{} excerpt emitted no events — fixture is not exercising the adapter",
                stream.as_str()
            );
        }
        if stream == CaptureStream::KrakenUsdtReference {
            // The reference socket must prove BOTH sidecar sources survive the
            // F1 routing — the USDT/USD ticker AND the `kraken.spot.status`
            // frame — not merely that it emits something, so the sidecar proof
            // cannot vanish silently if one arm regresses.
            for required in ["kraken.spot.status", "kraken.spot.usdt-usd.ticker"] {
                assert!(
                    emitted.contains(required),
                    "kraken_usdt_reference must emit {required}; got {emitted:?}"
                );
            }
        }
        for source_id in &emitted {
            assert!(
                inventory.contains(source_id.as_str()),
                "{} emitted {source_id}, which is absent from produced_source_ids()",
                stream.as_str()
            );
        }
    }
}

/// F4: replaying a bounded excerpt of the observed Bybit spot hour raises ZERO
/// forward-`u`-gap notices — the gap guard added to the orderbook path does not
/// false-trigger on real contiguous frames. (The full 36,952-frame check was run
/// once out of band and is recorded in the fix report.)
#[test]
fn bybit_spot_orderbook_has_no_false_u_gap_triggers() {
    use btc_feeds::FeedNoticeCode;
    let capture = include_str!("fixtures/observed/bybit_spot.jsonl");
    let mut session =
        ReplaySession::new(CaptureStream::BybitSpot, "fixture-epoch", None).expect("session");
    let mut gaps = 0_u64;
    let mut orderbook_frames = 0_u64;
    for line in capture.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let record: RawRecord = serde_json::from_str(line).expect("record parses");
        let outcome = session.ingest(&record).expect("record replays");
        for event in &outcome.events {
            if event.source_id == "bybit.spot.btcusdt.book-levels" {
                orderbook_frames += 1;
            }
        }
        gaps += outcome
            .notices
            .iter()
            .filter(|notice| notice.code == FeedNoticeCode::Gap)
            .count() as u64;
    }
    assert!(
        orderbook_frames > 0,
        "the excerpt must exercise the orderbook path"
    );
    assert_eq!(
        gaps, 0,
        "no forward-u-gap false triggers over contiguous frames"
    );
}
