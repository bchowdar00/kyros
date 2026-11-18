use std::collections::BTreeMap;
use std::io::Cursor;
use std::path::PathBuf;

use btc_feeds::replay::{KrakenBookDepths, NormalizeOptions, ReplayReport, normalize, replay};
use btc_feeds::{CaptureStream, RawRecord, RecordKind};
use sha2::{Digest, Sha256};
use signals_core::{SourceDefinition, SourceEvent};

#[test]
fn coinbase_replay_is_deterministic_and_contract_valid() {
    let records = vec![
        connection(CaptureStream::CoinbaseCore, 1),
        text(
            CaptureStream::CoinbaseCore,
            2,
            RecordKind::OutboundText,
            r#"{"type":"subscribe","channels":["level2_batch","matches","ticker"]}"#,
        ),
        text(
            CaptureStream::CoinbaseCore,
            3,
            RecordKind::InboundText,
            r#"{"type":"subscriptions","channels":[]}"#,
        ),
        text(
            CaptureStream::CoinbaseCore,
            4,
            RecordKind::InboundText,
            r#"{"type":"snapshot","product_id":"BTC-USD","bids":[["60000.10","1.2500"]],"asks":[["60001.20","2.5000"]]}"#,
        ),
        text(
            CaptureStream::CoinbaseCore,
            5,
            RecordKind::InboundText,
            r#"{"type":"match","trade_id":100,"sequence":900,"product_id":"BTC-USD","price":"60000.50","size":"0.0100","side":"sell","time":"2026-08-12T12:00:01Z"}"#,
        ),
        text(
            CaptureStream::CoinbaseCore,
            6,
            RecordKind::InboundText,
            r#"{"type":"ticker","trade_id":100,"sequence":901,"product_id":"BTC-USD","price":"60000.50","last_size":"0.0100","best_bid":"60000.10","best_bid_size":"1.2500","best_ask":"60001.20","best_ask_size":"2.5000","open_24h":"59800.00","volume_24h":"1250.50","low_24h":"59000.00","high_24h":"61000.00","volume_30d":"30000.00","time":"2026-08-12T12:00:01Z"}"#,
        ),
        text(
            CaptureStream::CoinbaseCore,
            7,
            RecordKind::InboundText,
            r#"{"type":"l2update","product_id":"BTC-USD","time":"2026-08-12T12:00:02Z","changes":[["buy","60000.10","1.0000"]]}"#,
        ),
        text(
            CaptureStream::CoinbaseCore,
            8,
            RecordKind::InboundText,
            r#"{"type":"status","products":[{"id":"BTC-USD","base_currency":"BTC","quote_currency":"USD","base_increment":"0.00000001","quote_increment":"0.01","status":"online","min_market_funds":"1.00","post_only":false,"limit_only":false,"cancel_only":false,"auction_mode":false}]}"#,
        ),
    ];

    let first = replay_records(&records, None);
    let second = replay_records(&records, None);
    assert_eq!(first, second);
    assert_eq!(first.0.events, 6);
    assert_eq!(first.0.recovery_notices, 0);

    let definitions = BTreeMap::from([
        definition("coinbase.exchange.btc-usd.book-levels", "1.0.0"),
        definition("coinbase.exchange.btc-usd.product", "1.0.0"),
        definition("coinbase.exchange.btc-usd.ticker", "1.0.0"),
        definition("coinbase.exchange.btc-usd.trades", "1.0.0"),
    ]);
    validate_events(&first.1, &definitions);
}

#[test]
fn binance_spot_replay_is_deterministic_over_partial_depth_and_agg_trades() {
    let records = vec![
        connection(CaptureStream::BinanceSpot, 1),
        text(
            CaptureStream::BinanceSpot,
            2,
            RecordKind::OutboundText,
            r#"{"method":"SUBSCRIBE","params":["btcusdt@depth20@100ms","btcusdt@aggTrade","btcusdt@bookTicker"],"id":1}"#,
        ),
        text(
            CaptureStream::BinanceSpot,
            3,
            RecordKind::InboundText,
            r#"{"result":null,"id":1}"#,
        ),
        text(
            CaptureStream::BinanceSpot,
            4,
            RecordKind::InboundText,
            r#"{"stream":"btcusdt@depth20@100ms","data":{"lastUpdateId":100,"bids":[["60000.10","1.2500"]],"asks":[["60001.20","2.5000"]]}}"#,
        ),
        text(
            CaptureStream::BinanceSpot,
            5,
            RecordKind::InboundText,
            r#"{"stream":"btcusdt@aggTrade","data":{"e":"aggTrade","E":1787918400123,"s":"BTCUSDT","a":500,"p":"60000.50","q":"0.0100","f":1,"l":2,"T":1787918400100,"m":true,"M":true}}"#,
        ),
        text(
            CaptureStream::BinanceSpot,
            6,
            RecordKind::InboundText,
            r#"{"stream":"btcusdt@depth20@100ms","data":{"lastUpdateId":101,"bids":[["60000.20","1.0000"]],"asks":[["60001.30","2.0000"]]}}"#,
        ),
        text(
            CaptureStream::BinanceSpot,
            7,
            RecordKind::InboundText,
            r#"{"stream":"btcusdt@bookTicker","data":{"u":900101,"s":"BTCUSDT","b":"60000.20","B":"1.0000","a":"60001.30","A":"2.0000"}}"#,
        ),
    ];

    let first = replay_records(&records, None);
    let second = replay_records(&records, None);
    assert_eq!(first, second);
    // Two depth snapshots (2 levels each), one aggregate trade, one best quote.
    assert_eq!(first.0.events, 6);
    assert_eq!(first.0.recovery_notices, 0);
    // The three spot promotions (group A3) published the field contracts these
    // events must conform to, so the adapter and the definitions are now checked
    // against each other on every run rather than by reading.
    let definitions = BTreeMap::from([
        definition("binance.spot.btcusdt.agg-trades", "1.0.0"),
        definition("binance.spot.btcusdt.book-levels", "1.0.0"),
        definition("binance.spot.btcusdt.book-ticker", "1.0.0"),
    ]);
    validate_events(&first.1, &definitions);
}

#[test]
fn binance_usdm_replay_is_byte_identical() {
    // DOCUMENTED, UNOBSERVED (ruling R1). Every payload below is synthetic: no
    // Binance USD-M frame or REST body has been captured, so this test pins the
    // contract the adapter was built to and the parity between that contract and
    // the USD-M source definitions. It is re-derived from the wave-A Tokyo
    // capture before the sources are adopted.
    let public = vec![
        connection(CaptureStream::BinanceUsdmPublic, 1),
        text(
            CaptureStream::BinanceUsdmPublic,
            2,
            RecordKind::OutboundText,
            r#"{"method":"SUBSCRIBE","params":["btcusdt@depth20@100ms"],"id":1}"#,
        ),
        text(
            CaptureStream::BinanceUsdmPublic,
            3,
            RecordKind::InboundText,
            r#"{"stream":"btcusdt@depth20@100ms","data":{"e":"depthUpdate","E":1788440000500,"T":1788440000480,"s":"BTCUSDT","ps":"BTCUSDT","U":880096,"u":880100,"pu":0,"st":1,"b":[["66120.10","1.50000000"]],"a":[["66121.30","0.80000000"]]}}"#,
        ),
        // A broken `pu` chain is informational on a full-replace stream, never a
        // gap: the image in hand is complete.
        text(
            CaptureStream::BinanceUsdmPublic,
            4,
            RecordKind::InboundText,
            r#"{"stream":"btcusdt@depth20@100ms","data":{"e":"depthUpdate","E":1788440000600,"T":1788440000580,"s":"BTCUSDT","ps":"BTCUSDT","U":880136,"u":880140,"pu":880120,"st":1,"b":[["66120.20","1.10000000"]],"a":[["66121.40","0.90000000"]]}}"#,
        ),
        // A duplicate update id is dropped without state change.
        text(
            CaptureStream::BinanceUsdmPublic,
            5,
            RecordKind::InboundText,
            r#"{"stream":"btcusdt@depth20@100ms","data":{"e":"depthUpdate","E":1788440000700,"T":1788440000680,"s":"BTCUSDT","ps":"BTCUSDT","U":880136,"u":880140,"pu":880136,"st":1,"b":[["66120.20","1.10000000"]],"a":[["66121.40","0.90000000"]]}}"#,
        ),
    ];
    let first_public = replay_records(&public, None);
    assert_eq!(first_public, replay_records(&public, None));
    assert_eq!(first_public.0.events, 4);
    assert_eq!(first_public.0.recovery_notices, 0);

    let rest = vec![
        connection(CaptureStream::BinanceUsdmRest, 1),
        text(
            CaptureStream::BinanceUsdmRest,
            2,
            RecordKind::OutboundText,
            r#"{"method":"GET","poll":1,"request":"open_interest","url":"https://fapi.binance.com/fapi/v1/openInterest?symbol=BTCUSDT"}"#,
        ),
        text(
            CaptureStream::BinanceUsdmRest,
            3,
            RecordKind::InboundText,
            r#"{"openInterest":"10659.50900000","symbol":"BTCUSDT","time":1788440000000}"#,
        ),
        text(
            CaptureStream::BinanceUsdmRest,
            4,
            RecordKind::OutboundText,
            r#"{"method":"GET","poll":1,"request":"open_interest_hist","url":"https://fapi.binance.com/futures/data/openInterestHist?symbol=BTCUSDT&period=5m&limit=30"}"#,
        ),
        text(
            CaptureStream::BinanceUsdmRest,
            5,
            RecordKind::InboundText,
            r#"[{"symbol":"BTCUSDT","sumOpenInterest":"10650.000","sumOpenInterestValue":"704000000.00","CMCCirculatingSupply":"19800000","timestamp":1788440000000}]"#,
        ),
        text(
            CaptureStream::BinanceUsdmRest,
            6,
            RecordKind::OutboundText,
            r#"{"method":"GET","poll":1,"request":"funding_info","url":"https://fapi.binance.com/fapi/v1/fundingInfo"}"#,
        ),
        text(
            CaptureStream::BinanceUsdmRest,
            7,
            RecordKind::InboundText,
            r#"[{"symbol":"BTCUSDT","adjustedFundingRateCap":"0.02000000","adjustedFundingRateFloor":"-0.02000000","fundingIntervalHours":4}]"#,
        ),
    ];
    let first_rest = replay_records(&rest, None);
    assert_eq!(first_rest, replay_records(&rest, None));
    assert_eq!(first_rest.0.events, 3);
    assert_eq!(first_rest.0.recovery_notices, 0);

    // Every USD-M event conforms to its published field contract, so an
    // approved definition and the adapter cannot disagree silently.
    let definitions = BTreeMap::from([
        definition("binance.usdm.btcusdt.book-levels", "1.0.0"),
        definition("binance.usdm.btcusdt.open-interest", "1.0.0"),
        definition("binance.usdm.btcusdt.open-interest-hist", "1.0.0"),
        definition("binance.usdm.btcusdt.funding-info", "1.0.0"),
    ]);
    validate_events(&first_public.1, &definitions);
    validate_events(&first_rest.1, &definitions);
}

#[test]
fn bybit_spot_and_linear_replay_are_deterministic_over_both_stream_shapes() {
    // Spot session: op ack, a deep-book snapshot, a trade, a bbo snapshot, and a
    // deep-book delta (with a level deletion).
    let spot = vec![
        connection(CaptureStream::BybitSpot, 1),
        text(
            CaptureStream::BybitSpot,
            2,
            RecordKind::OutboundText,
            r#"{"req_id":"1","op":"subscribe","args":["publicTrade.BTCUSDT","orderbook.200.BTCUSDT","orderbook.1.BTCUSDT"]}"#,
        ),
        text(
            CaptureStream::BybitSpot,
            3,
            RecordKind::InboundText,
            r#"{"success":true,"ret_msg":"subscribe","conn_id":"c1","req_id":"1","op":"subscribe"}"#,
        ),
        text(
            CaptureStream::BybitSpot,
            4,
            RecordKind::InboundText,
            r#"{"topic":"orderbook.200.BTCUSDT","type":"snapshot","ts":1787918400123,"data":{"s":"BTCUSDT","b":[["60000.10","1.2500"]],"a":[["60001.20","2.5000"]],"u":100,"seq":7961638724},"cts":1787918400120}"#,
        ),
        text(
            CaptureStream::BybitSpot,
            5,
            RecordKind::InboundText,
            r#"{"topic":"publicTrade.BTCUSDT","type":"snapshot","ts":1787918400123,"data":[{"T":1787918400100,"s":"BTCUSDT","S":"Buy","v":"0.0100","p":"60000.50","i":"20f43950-d8dd-5b31-9112-a178eb6023af","BT":false,"RPI":false,"seq":1783284617}]}"#,
        ),
        text(
            CaptureStream::BybitSpot,
            6,
            RecordKind::InboundText,
            r#"{"topic":"orderbook.1.BTCUSDT","type":"snapshot","ts":1787918400223,"data":{"s":"BTCUSDT","b":[["60000.10","1.0000"]],"a":[["60001.20","2.0000"]],"u":40,"seq":7961638730},"cts":1787918400220}"#,
        ),
        text(
            CaptureStream::BybitSpot,
            7,
            RecordKind::InboundText,
            r#"{"topic":"orderbook.200.BTCUSDT","type":"delta","ts":1787918400323,"data":{"s":"BTCUSDT","b":[["60000.10","0"]],"a":[],"u":101,"seq":7961638735},"cts":1787918400320}"#,
        ),
        // The real capture records the client keepalive ping and the venue pong;
        // both are operational, never events.
        text(
            CaptureStream::BybitSpot,
            8,
            RecordKind::OutboundText,
            r#"{"op":"ping"}"#,
        ),
        text(
            CaptureStream::BybitSpot,
            9,
            RecordKind::InboundText,
            r#"{"success":true,"ret_msg":"pong","conn_id":"c1","req_id":"","op":"ping"}"#,
        ),
    ];
    let spot_first = replay_records(&spot, None);
    let spot_second = replay_records(&spot, None);
    assert_eq!(spot_first, spot_second);
    // deep snapshot (2) + trade (1) + bbo snapshot (2) + deep delta (1) = 6; the
    // ping/pong keepalive frames produce no events.
    assert_eq!(spot_first.0.events, 6);
    assert_eq!(spot_first.0.recovery_notices, 0);

    // Linear session over all four subscribed topics: liquidations, the folded
    // ticker (snapshot then delta), the `orderbook.50` book (snapshot then a
    // level deletion) and the perp trade tape (which carries the futures-only L
    // tick direction).
    let linear = vec![
        connection(CaptureStream::BybitLinear, 1),
        text(
            CaptureStream::BybitLinear,
            2,
            RecordKind::OutboundText,
            r#"{"req_id":"1","op":"subscribe","args":["allLiquidation.BTCUSDT","tickers.BTCUSDT","orderbook.50.BTCUSDT","publicTrade.BTCUSDT"]}"#,
        ),
        text(
            CaptureStream::BybitLinear,
            3,
            RecordKind::InboundText,
            r#"{"success":true,"ret_msg":"subscribe","conn_id":"c1","req_id":"1","op":"subscribe"}"#,
        ),
        text(
            CaptureStream::BybitLinear,
            4,
            RecordKind::InboundText,
            r#"{"topic":"allLiquidation.BTCUSDT","type":"snapshot","ts":1739502303204,"data":[{"T":1739502302929,"s":"BTCUSDT","S":"Sell","v":"20000","p":"60000.00"}]}"#,
        ),
        text(
            CaptureStream::BybitLinear,
            5,
            RecordKind::InboundText,
            r#"{"topic":"allLiquidation.BTCUSDT","type":"snapshot","ts":1739502303704,"data":[{"T":1739502303500,"s":"BTCUSDT","S":"Buy","v":"15000","p":"60100.00"},{"T":1739502303600,"s":"BTCUSDT","S":"Sell","v":"5000","p":"59900.00"}]}"#,
        ),
        text(
            CaptureStream::BybitLinear,
            6,
            RecordKind::InboundText,
            r#"{"topic":"tickers.BTCUSDT","type":"snapshot","ts":1739502303804,"cs":100,"data":{"symbol":"BTCUSDT","tickDirection":"ZeroPlusTick","price24hPcnt":"0.005","lastPrice":"60000.00","prevPrice24h":"59600.00","highPrice24h":"60500.00","lowPrice24h":"59000.00","prevPrice1h":"60100.00","markPrice":"60000.50","indexPrice":"60000.20","openInterest":"12345.6700","openInterestValue":"740740200.00","turnover24h":"1234567.0","volume24h":"20.5","fundingIntervalHour":"8","fundingCap":"0.02","fundingRate":"-0.00012","nextFundingTime":"1739505600000","bid1Price":"59999.90","bid1Size":"1.5000","ask1Price":"60000.10","ask1Size":"2.0000","curPreListingPhase":"Finished","preOpenPrice":"0","preQty":"0","singleOpenInterest":"6172.83","singleOpenInterestValue":"370370100.00"}}"#,
        ),
        text(
            CaptureStream::BybitLinear,
            7,
            RecordKind::InboundText,
            r#"{"topic":"tickers.BTCUSDT","type":"delta","ts":1739502303904,"cs":101,"data":{"symbol":"BTCUSDT","markPrice":"60001.00"}}"#,
        ),
        text(
            CaptureStream::BybitLinear,
            8,
            RecordKind::InboundText,
            r#"{"topic":"orderbook.50.BTCUSDT","type":"snapshot","ts":1739502304004,"data":{"s":"BTCUSDT","b":[["59999.90","1.0000"]],"a":[["60000.10","2.0000"]],"u":200,"seq":7961700000},"cts":1739502304000}"#,
        ),
        text(
            CaptureStream::BybitLinear,
            9,
            RecordKind::InboundText,
            r#"{"topic":"orderbook.50.BTCUSDT","type":"delta","ts":1739502304104,"data":{"s":"BTCUSDT","b":[["59999.80","0"]],"a":[],"u":201,"seq":7961700005},"cts":1739502304100}"#,
        ),
        text(
            CaptureStream::BybitLinear,
            10,
            RecordKind::InboundText,
            r#"{"topic":"publicTrade.BTCUSDT","type":"snapshot","ts":1739502304204,"data":[{"T":1739502304200,"s":"BTCUSDT","S":"Buy","v":"0.5000","p":"60000.00","i":"2290000001203031348","BT":false,"RPI":false,"L":"PlusTick","seq":1783300000}]}"#,
        ),
    ];
    let linear_first = replay_records(&linear, None);
    let linear_second = replay_records(&linear, None);
    assert_eq!(linear_first, linear_second);
    // liquidations (1 + 2) + ticker (snapshot + delta) + orderbook.50 (2 + 1) +
    // one perp trade = 9.
    assert_eq!(linear_first.0.events, 9);
    assert_eq!(linear_first.0.recovery_notices, 0);

    // The `bybit.*` source definitions now exist, so both streams' emitted
    // events are validated field-for-field against them (was deferred to the
    // adoption slice, group A4).
    let definitions = BTreeMap::from([
        definition("bybit.spot.btcusdt.trades", "1.0.0"),
        definition("bybit.spot.btcusdt.book-levels", "1.0.0"),
        definition("bybit.spot.btcusdt.bbo-levels", "1.0.0"),
        definition("bybit.linear.btcusdt.liquidations", "1.0.0"),
        definition("bybit.linear.btcusdt.tickers", "1.0.0"),
        definition("bybit.linear.btcusdt.book-levels", "1.0.0"),
        definition("bybit.linear.btcusdt.trades", "1.0.0"),
    ]);
    validate_events(&spot_first.1, &definitions);
    validate_events(&linear_first.1, &definitions);
}

#[test]
fn bybit_rest_replay_is_deterministic_across_two_runs() {
    // The two linear REST identities ride one capture stream and are classified
    // by response shape: an open-interest body carries `openInterest` rows, a
    // funding body carries `fundingRate` rows. Each response yields one event.
    let records = vec![
        connection(CaptureStream::BybitLinearRest, 1),
        text(
            CaptureStream::BybitLinearRest,
            2,
            RecordKind::OutboundText,
            "https://api.bybit.com/v5/market/open-interest?category=linear&symbol=BTCUSDT&intervalTime=5min&limit=1",
        ),
        text(
            CaptureStream::BybitLinearRest,
            3,
            RecordKind::InboundText,
            r#"{"retCode":0,"retMsg":"OK","result":{"symbol":"BTCUSDT","category":"linear","list":[{"openInterest":"12345.6700","singleOpenInterest":"6172.83","timestamp":"1739502300000"}],"nextPageCursor":""},"retExtInfo":{},"time":1739502303204}"#,
        ),
        text(
            CaptureStream::BybitLinearRest,
            4,
            RecordKind::OutboundText,
            "https://api.bybit.com/v5/market/funding/history?category=linear&symbol=BTCUSDT&limit=1",
        ),
        text(
            CaptureStream::BybitLinearRest,
            5,
            RecordKind::InboundText,
            r#"{"retCode":0,"retMsg":"OK","result":{"category":"linear","list":[{"symbol":"BTCUSDT","fundingRate":"-0.00012","fundingRateTimestamp":"1739502000000"}],"nextPageCursor":""},"retExtInfo":{},"time":1739502303204}"#,
        ),
    ];
    let first = replay_records(&records, None);
    let second = replay_records(&records, None);
    assert_eq!(first, second);
    // One open-interest row and one funding row = 2.
    assert_eq!(first.0.events, 2);
    assert_eq!(first.0.recovery_notices, 0);

    let definitions = BTreeMap::from([
        definition("bybit.linear.btcusdt.open-interest", "1.0.0"),
        definition("bybit.linear.btcusdt.funding-history", "1.0.0"),
    ]);
    validate_events(&first.1, &definitions);
}

#[test]
fn chainlink_poll_replay_is_deterministic_over_connected_and_poll_pairs() {
    // One ABI word: a u128 value zero-extended to a 32-byte (64 hex char) word.
    fn word(value: u128) -> String {
        format!("{value:064x}")
    }
    fn result(
        round_id: u128,
        answer: u128,
        started: u128,
        updated: u128,
        answered: u128,
    ) -> String {
        format!(
            "0x{}{}{}{}{}",
            word(round_id),
            word(answer),
            word(started),
            word(updated),
            word(answered),
        )
    }
    fn response(result: &str) -> String {
        format!(r#"{{"jsonrpc":"2.0","id":1,"result":"{result}"}}"#)
    }
    // The exact eth_call poll request recorded as OutboundText, built from the one
    // authority so this fixture cannot drift from the wire shape; replay ignores it.
    let request = btc_feeds::chainlink::request_body(1);

    // Round id 129127208515966885128 (phase 7, agg round 23816) and answer
    // 77667.797 (raw 7766779700000) are the live-probed 2026-08-29 values; the
    // timestamps use a synthetic 2025-08-28 reference (unix seconds 1756400000),
    // matching the latency test rather than the live 2026 timestamp.
    let round_a = (7u128 << 64) | 23_816;
    let round_b = round_a + 1;
    let response_a = response(&result(
        round_a,
        7_766_779_700_000,
        1_756_400_000,
        1_756_400_000,
        round_a,
    ));
    let response_b = response(&result(
        round_b,
        7_766_800_000_000,
        1_756_400_060,
        1_756_400_060,
        round_b,
    ));

    let records = vec![
        connection(CaptureStream::ChainlinkAggregator, 1),
        text(
            CaptureStream::ChainlinkAggregator,
            2,
            RecordKind::OutboundText,
            &request,
        ),
        // First poll: round A -> one event.
        text(
            CaptureStream::ChainlinkAggregator,
            3,
            RecordKind::InboundText,
            &response_a,
        ),
        text(
            CaptureStream::ChainlinkAggregator,
            4,
            RecordKind::OutboundText,
            &request,
        ),
        // Second poll: the feed has not updated, same round A -> no event, no notice.
        text(
            CaptureStream::ChainlinkAggregator,
            5,
            RecordKind::InboundText,
            &response_a,
        ),
        text(
            CaptureStream::ChainlinkAggregator,
            6,
            RecordKind::OutboundText,
            &request,
        ),
        // Third poll: round advanced to B -> one event.
        text(
            CaptureStream::ChainlinkAggregator,
            7,
            RecordKind::InboundText,
            &response_b,
        ),
    ];

    let first = replay_records(&records, None);
    let second = replay_records(&records, None);
    assert_eq!(first, second);
    // Round A plus round B; the duplicate-round poll and the outbound requests
    // produce no events.
    assert_eq!(first.0.events, 2);
    assert_eq!(first.0.recovery_notices, 0);

    // T-REPLAY-1: the adoption slice publishes `chainlink.ethereum.btc-usd.rounds`,
    // so every replayed event is now validated against the source definition on
    // disk, in addition to the replay session mechanics and cross-run determinism.
    let definitions = BTreeMap::from([definition("chainlink.ethereum.btc-usd.rounds", "1.0.0")]);
    validate_events(&first.1, &definitions);
}

#[test]
fn chainlink_normalization_analysis_survives_a_garbage_poll_body() {
    // A non-JSON 2xx body from the RPC gateway is a per-poll anomaly: replay
    // skips it with a notice, and the normalization analyzer must complete the
    // same journal rather than abort on the shared envelope parse it never
    // needed for this provider's constant series.
    let request = btc_feeds::chainlink::request_body(1);
    let round = (7u128 << 64) | 23_816;
    let valid_response = format!(
        r#"{{"jsonrpc":"2.0","id":1,"result":"0x{round:064x}{:064x}{:064x}{:064x}{round:064x}"}}"#,
        7_766_779_700_000u128, 1_756_400_000u128, 1_756_400_000u128,
    );
    let records = vec![
        connection(CaptureStream::ChainlinkAggregator, 1),
        text(
            CaptureStream::ChainlinkAggregator,
            2,
            RecordKind::OutboundText,
            &request,
        ),
        text(
            CaptureStream::ChainlinkAggregator,
            3,
            RecordKind::InboundText,
            "<html>502 Bad Gateway</html>",
        ),
        text(
            CaptureStream::ChainlinkAggregator,
            4,
            RecordKind::OutboundText,
            &request,
        ),
        text(
            CaptureStream::ChainlinkAggregator,
            5,
            RecordKind::InboundText,
            &valid_response,
        ),
    ];
    let mut bytes = Vec::new();
    for record in &records {
        serde_json::to_writer(&mut bytes, record).expect("record serializes");
        bytes.push(b'\n');
    }
    let report = btc_feeds::normalization::analyze(&mut std::io::Cursor::new(bytes), "1", None)
        .expect("analysis completes despite the garbage body")
        .expect("journal is non-empty");
    assert_eq!(report.records, 5);
}

#[test]
fn nba_espn_status_replay_is_deterministic_over_a_two_cycle_capture() {
    // Minimal synthetic scoreboard/injuries bodies trimmed to the validated core;
    // shapes and field paths mirror the real 2026-08-30 ESPN fetches. No giant
    // blobs live in the repo.
    fn scoreboard(state: &str, home: u64, away: u64) -> String {
        format!(
            r#"{{"day":{{"date":"2026-10-03"}},"events":[{{"id":"401902644","status":{{"displayClock":"0.0","period":0,"type":{{"name":"STATUS_SCHEDULED","state":"{state}"}}}},"competitions":[{{"competitors":[{{"homeAway":"home","score":"{home}","team":{{"id":"28","abbreviation":"TOR"}}}},{{"homeAway":"away","score":"{away}","team":{{"id":"14","abbreviation":"MIA"}}}}]}}]}}]}}"#
        )
    }
    fn injuries(status: &str) -> String {
        format!(
            r#"{{"timestamp":"2026-08-30T23:58:12Z","status":"success","injuries":[{{"id":"1","displayName":"Atlanta Hawks","injuries":[{{"status":"{status}","date":"2026-07-19T00:14Z","athlete":{{"displayName":"Mouhamed Gueye","links":[{{"href":"https://www.espn.com/nba/player/_/id/4712863/mouhamed-gueye"}}]}},"details":{{"type":"Foot","detail":"Fracture","side":"Left","location":"Leg"}}}}]}}]}}"#
        )
    }
    let sb_url = "https://site.api.espn.com/apis/site/v2/sports/basketball/nba/scoreboard";
    let inj_url = "https://site.api.espn.com/apis/site/v2/sports/basketball/nba/injuries";

    let records = vec![
        connection(CaptureStream::NbaEspnStatus, 1),
        // Cycle 1: scoreboard (one game) then injuries (one entry).
        text(
            CaptureStream::NbaEspnStatus,
            2,
            RecordKind::OutboundText,
            &btc_feeds::nba_espn::outbound_descriptor(sb_url, 1, 0),
        ),
        text(
            CaptureStream::NbaEspnStatus,
            3,
            RecordKind::InboundText,
            &scoreboard("pre", 0, 0),
        ),
        text(
            CaptureStream::NbaEspnStatus,
            4,
            RecordKind::OutboundText,
            &btc_feeds::nba_espn::outbound_descriptor(inj_url, 1, 1),
        ),
        text(
            CaptureStream::NbaEspnStatus,
            5,
            RecordKind::InboundText,
            &injuries("Out"),
        ),
        // Cycle 2: scoreboard changed (emits), injuries unchanged (silent).
        text(
            CaptureStream::NbaEspnStatus,
            6,
            RecordKind::OutboundText,
            &btc_feeds::nba_espn::outbound_descriptor(sb_url, 2, 0),
        ),
        text(
            CaptureStream::NbaEspnStatus,
            7,
            RecordKind::InboundText,
            &scoreboard("in", 12, 10),
        ),
        text(
            CaptureStream::NbaEspnStatus,
            8,
            RecordKind::OutboundText,
            &btc_feeds::nba_espn::outbound_descriptor(inj_url, 2, 1),
        ),
        text(
            CaptureStream::NbaEspnStatus,
            9,
            RecordKind::InboundText,
            &injuries("Out"),
        ),
    ];

    let first = replay_records(&records, None);
    let second = replay_records(&records, None);
    assert_eq!(first, second);
    // Cycle 1: 1 scoreboard + 1 injury = 2. Cycle 2: scoreboard changed (1),
    // injury unchanged (silent). Total 3 events; outbound descriptors produce none.
    assert_eq!(first.0.events, 3);
    assert_eq!(first.0.recovery_notices, 0);

    // SourceDefinition contract validation is intentionally omitted: the `nba.*`
    // source definitions are published by a later slice (definitions/nba/ does not
    // exist yet), and only then can `validate_events` be applied. Until then this
    // pins replay session mechanics and cross-run determinism for the poll feed.
}

#[test]
fn nba_espn_daily_replay_is_deterministic_and_keys_bodyless_shapes_from_descriptors() {
    // Minimal synthetic bodies trimmed to the validated core; shapes and field
    // paths mirror the real 2026-08-31 ESPN fetches. No giant blobs in the repo.
    //
    // This pins the slice's load-bearing rule end to end through the real replay
    // session: the athlete gamelog and splits payloads carry NO occurrence of the
    // athlete id, so their instance identity comes from the paired OutboundText
    // request descriptor, exactly as replay already recovers Kraken book depth
    // from a recorded subscription message.
    const TEAMS_LIST: &str =
        r#"{"sports":[{"id":"40","leagues":[{"teams":[{"team":{"id":"2"}}]}]}]}"#;
    const STANDINGS: &str = r#"{"season":{"year":2026},"children":[{"id":"5","name":"Eastern Conference","standings":{"entries":[
        {"team":{"id":"8","abbreviation":"DET"},"stats":[
          {"name":"wins","type":"wins","value":60.0},
          {"name":"streak","type":"streak","value":3.0,"displayValue":"W3"},
          {"name":"overall","type":"total","summary":"60-22"},
          {"name":"Home","type":"home","summary":"31-9"},
          {"name":"Road","type":"road","summary":"28-13"},
          {"name":"vs. Div.","type":"vsdiv","summary":"12-4"},
          {"name":"vs. Conf.","type":"vsconf","summary":"39-13"},
          {"name":"Last Ten Games","type":"lasttengames","summary":"8-2"}]}]}}]}"#;
    const GAMELOG: &str = r#"{"filters":[],"labels":["MIN","FG","PTS"],
        "names":["minutes","fieldGoalsMade-fieldGoalsAttempted","points"],"displayNames":[],
        "events":{"401869384":{"id":"401869384","atVs":"@","gameDate":"2026-05-03T23:30:00.000+00:00",
          "gameResult":"L","opponent":{"id":"5","abbreviation":"CLE"}}},
        "seasonTypes":[{"displayName":"2025-26 Postseason","categories":[
          {"displayName":"CQF","type":"event","splitType":"conference_quarterfinals",
           "events":[{"eventId":"401869384","stats":["37","8-14","24"]}]}]}],"glossary":[]}"#;
    const SPLITS: &str = r#"{"filters":[],"displayName":"2025-26 Splits","labels":["GP","PTS"],
        "names":["gamesPlayed","avgPoints"],"displayNames":[],"descriptions":[],
        "splitCategories":[{"name":"split","splits":[
          {"displayName":"All Splits","abbreviation":"Total","stats":["80","18.1"]}]}]}"#;

    fn descriptor(family: &str, entity_id: &str, index: u64) -> String {
        btc_feeds::nba_espn_daily::outbound_descriptor(
            "https://site.web.api.espn.com/apis/common/v3/sports/basketball/nba/x",
            "b",
            family,
            entity_id,
            index,
        )
    }
    fn out(ordinal: u64, family: &str, entity_id: &str) -> RawRecord {
        text(
            CaptureStream::NbaEspnDaily,
            ordinal,
            RecordKind::OutboundText,
            &descriptor(family, entity_id, ordinal),
        )
    }
    fn inb(ordinal: u64, payload: &str) -> RawRecord {
        text(
            CaptureStream::NbaEspnDaily,
            ordinal,
            RecordKind::InboundText,
            payload,
        )
    }

    let records = vec![
        connection(CaptureStream::NbaEspnDaily, 1),
        // Phase A. The teams list is a recognized NON-EVENT shape: it classifies
        // cleanly, emits nothing, and raises no notice.
        out(2, "teams-list", ""),
        inb(3, TEAMS_LIST),
        out(4, "standings", ""),
        inb(5, STANDINGS),
        // Phase B. Two DIFFERENT athletes are served byte-identical splits bodies
        // -- a real possibility for two low-minute players. Only the descriptor
        // separates them, and this stream never dedups, so both must emit.
        out(6, "athlete-gamelog", "4433134"),
        inb(7, GAMELOG),
        out(8, "athlete-splits", "3945274"),
        inb(9, SPLITS),
        out(10, "athlete-splits", "4066648"),
        inb(11, SPLITS),
        disconnection(CaptureStream::NbaEspnDaily, 12),
    ];

    let (first_report, first_events) = replay_records(&records, None);
    let (second_report, second_events) = replay_records(&records, None);
    assert_eq!(first_report, second_report);
    assert_eq!(first_events, second_events);
    // Standings 1 + gamelog 1 + splits 1 + splits 1 = 4. Teams list emits none,
    // and outbound descriptors emit none.
    assert_eq!(first_report.events, 4);
    assert_eq!(first_report.recovery_notices, 0);

    let events: Vec<serde_json::Value> = String::from_utf8(first_events)
        .expect("normalized output is UTF-8")
        .lines()
        .map(|line| serde_json::from_str(line).expect("event line is JSON"))
        .collect();
    assert_eq!(events.len(), 4);
    assert_eq!(events[0]["source_id"], "nba.espn.daily.standings");
    // The seven catalog 5.7 display fields survive the whole replay path.
    for (field, expected) in [
        ("streak_display_season", "W3"),
        ("record_summary_overall_display", "60-22"),
        ("record_summary_home_display", "31-9"),
        ("record_summary_road_display", "28-13"),
        ("record_summary_vs_division_display", "12-4"),
        ("record_summary_vs_conference_display", "39-13"),
        ("record_summary_last10_display", "8-2"),
    ] {
        assert_eq!(events[0]["values"][field], expected, "{field}");
    }
    // Identity recovered from the descriptor, and labeled as such.
    assert_eq!(events[1]["source_id"], "nba.espn.daily.athlete-gamelog");
    assert_eq!(events[1]["values"]["athlete_id"], "4433134");
    assert_eq!(
        events[1]["values"]["athlete_id_basis"],
        "request_descriptor"
    );
    assert_eq!(events[1]["values"]["field_goals_made"], 8.0);
    assert_eq!(events[1]["values"]["field_goals_attempted"], 14.0);
    // Identical bodies, different descriptors, two distinct athletes.
    assert_eq!(events[2]["source_id"], "nba.espn.daily.athlete-splits");
    assert_eq!(events[2]["values"]["athlete_id"], "3945274");
    assert_eq!(events[3]["values"]["athlete_id"], "4066648");

    // SourceDefinition contract validation is intentionally omitted for the same
    // reason as the slice-1 status stream: the `nba.*` source definitions are
    // published by a later slice (definitions/nba/ does not exist yet), and only
    // then can `validate_events` be applied.
}

#[test]
fn nba_espn_daily_bodyless_shapes_fail_closed_without_a_usable_descriptor() {
    // The three fail-closed conditions of the descriptor-pairing rule, driven
    // through the real replay session: no descriptor at all, a descriptor for a
    // different request family, and a descriptor whose entity id is not an id.
    // Every one skips the body with a notice; none emits, and none is fatal.
    const GAMELOG: &str = r#"{"filters":[],"labels":["PTS"],"names":["points"],"displayNames":[],
        "events":{"1":{"id":"1"}},
        "seasonTypes":[{"displayName":"RS","categories":[
          {"displayName":"m","type":"event","events":[{"eventId":"1","stats":["24"]}]}]}],
        "glossary":[]}"#;

    for descriptor in [
        None,
        Some(("roster", "2")),
        Some(("athlete-gamelog", "not-an-id")),
    ] {
        let mut records = vec![connection(CaptureStream::NbaEspnDaily, 1)];
        let mut ordinal = 2;
        if let Some((family, entity_id)) = descriptor {
            records.push(text(
                CaptureStream::NbaEspnDaily,
                ordinal,
                RecordKind::OutboundText,
                &btc_feeds::nba_espn_daily::outbound_descriptor(
                    "https://site.web.api.espn.com/x",
                    "b",
                    family,
                    entity_id,
                    1,
                ),
            ));
            ordinal += 1;
        }
        records.push(text(
            CaptureStream::NbaEspnDaily,
            ordinal,
            RecordKind::InboundText,
            GAMELOG,
        ));

        let (report, output) = replay_records(&records, None);
        assert_eq!(report.events, 0, "descriptor {descriptor:?} must not emit");
        assert!(output.is_empty(), "descriptor {descriptor:?}");
        assert!(
            report.notices >= 1,
            "descriptor {descriptor:?} must be noticed"
        );
        assert_eq!(report.recovery_notices, 0, "descriptor {descriptor:?}");
    }
}

#[test]
fn nba_espn_games_replay_is_deterministic_over_a_two_cycle_game_day_capture() {
    // Minimal synthetic bodies trimmed to the validated core; shapes, field paths
    // and values mirror the real 2026-08-30 fetches of event 401811026 (DET @
    // CHA). No giant blobs live in the repo.
    //
    // This pins the slice's two load-bearing rules end to end through the real
    // replay session:
    //
    //  1. NO DEDUP. ESPN's own odds-movement endpoint answers `{count:0}`, so our
    //     poll sequence IS the movement series. Cycle 1 repeats cycle 0's odds
    //     body byte for byte and MUST emit a second, separately sequenced event.
    //  2. The summary is fetched only on cycles where `cycle % 6 == 0`, so the
    //     0-based cycle 0 carries it and cycle 1 does not.
    const ODDS: &str = r#"{"count":1,"pageIndex":1,"pageSize":100,"pageCount":1,"items":[
        {"$ref":"http://sports.core.api.espn.com/v2/sports/basketball/leagues/nba/events/401811026/competitions/401811026/odds/100?lang=en",
         "provider":{"id":"100","name":"Draft Kings","priority":1},
         "details":"CHA -6.5","overUnder":225.5,"spread":-6.5,"overOdds":-112.0,"underOdds":-108.0,
         "open":{"over":{"value":1.909,"american":"-110"},"under":{"value":1.909,"american":"-110"},
                 "total":{"value":222.5,"american":"222.5"}},
         "current":{"over":{"value":1.893,"american":"-112"},"under":{"value":1.926,"american":"-108"},
                    "total":{"value":225.5,"american":"225.5"}},
         "awayTeamOdds":{"favorite":false,"moneyLine":180,
           "team":{"$ref":"http://sports.core.api.espn.com/v2/sports/basketball/leagues/nba/seasons/2026/teams/8"},
           "open":{"pointSpread":{"american":"+2.5"},"moneyLine":{"value":2.14,"american":"+114"}},
           "current":{"pointSpread":{"american":"+6.5"},"moneyLine":{"value":2.8,"american":"+180"}}},
         "homeTeamOdds":{"favorite":true,"moneyLine":-218,
           "team":{"$ref":"http://sports.core.api.espn.com/v2/sports/basketball/leagues/nba/seasons/2026/teams/30"},
           "open":{"pointSpread":{"american":"-2.5"},"moneyLine":{"value":1.741,"american":"-135"}},
           "current":{"pointSpread":{"american":"-6.5"},"moneyLine":{"value":1.459,"american":"-218"}}}}]}"#;
    const PREDICTOR: &str = r#"{"$ref":"http://sports.core.api.espn.com/v2/sports/basketball/leagues/nba/events/401811026/competitions/401811026/predictor?lang=en",
        "lastModified":"2026-04-10T22:50Z",
        "homeTeam":{"statistics":[]},
        "awayTeam":{"statistics":[
          {"name":"gameProjection","value":46.722},
          {"name":"matchupQuality","value":99.0},
          {"name":"teamChanceLoss","value":53.278},
          {"name":"teamPredPtDiff","value":-1.028}]}}"#;
    const SUMMARY: &str = r#"{"boxscore":{"teams":[]},
        "header":{"id":"401811026","season":{"year":2026,"type":2},
          "competitions":[{"id":"401811026","boxscoreAvailable":true}]},
        "seasonseries":[{"type":"season","title":"Regular Season Series","summary":"DET wins series 3-0",
          "completed":true,"totalCompetitions":3,"seriesScore":"3-0",
          "events":[{"id":"401811000","date":"2026-01-05T00:00Z",
            "competitors":[{"homeAway":"home","winner":false,"score":"101","team":{"id":"30"}},
                           {"homeAway":"away","winner":true,"score":"113","team":{"id":"8"}}]}]}],
        "injuries":[{"team":{"id":"30"},"injuries":[{"status":"Out"}]}],
        "gameInfo":{"venue":{"id":"1893","fullName":"Spectrum Center","address":{"city":"Charlotte","state":"NC"}}}}"#;
    // The scoreboard is this stream's discovery input and a recognized NON-EVENT
    // shape: slice 1 already owns scoreboard emission, so here the identical body
    // must produce nothing at all.
    const SCOREBOARD: &str = r#"{"leagues":[{"calendar":["2026-04-10"]}],
        "events":[{"id":"401811026","date":"2026-04-10T23:00Z",
          "status":{"type":{"id":"1","name":"STATUS_SCHEDULED","state":"pre"}},
          "competitions":[{"id":"401811026","competitors":[
            {"homeAway":"home","score":"0","team":{"id":"30","abbreviation":"CHA"}},
            {"homeAway":"away","score":"0","team":{"id":"8","abbreviation":"DET"}}]}]}]}"#;

    fn out(ordinal: u64, url: &str, cycle: u64, index: u64, entity_id: &str) -> RawRecord {
        text(
            CaptureStream::NbaEspnGames,
            ordinal,
            RecordKind::OutboundText,
            &btc_feeds::nba_espn_games::outbound_descriptor(url, cycle, index, entity_id),
        )
    }
    fn inb(ordinal: u64, payload: &str) -> RawRecord {
        text(
            CaptureStream::NbaEspnGames,
            ordinal,
            RecordKind::InboundText,
            payload,
        )
    }
    let scoreboard_url =
        "https://site.api.espn.com/apis/site/v2/sports/basketball/nba/scoreboard?dates=20260410";
    let odds_url = "https://sports.core.api.espn.com/v2/sports/basketball/leagues/nba/events/401811026/competitions/401811026/odds?limit=100";
    let predictor_url = "https://sports.core.api.espn.com/v2/sports/basketball/leagues/nba/events/401811026/competitions/401811026/predictor";
    let summary_url =
        "https://site.api.espn.com/apis/site/v2/sports/basketball/nba/summary?event=401811026";

    let records = vec![
        connection(CaptureStream::NbaEspnGames, 1),
        // Cycle 0: discovery, then odds + predictor + summary for the one event.
        out(2, scoreboard_url, 0, 0, ""),
        inb(3, SCOREBOARD),
        out(4, odds_url, 0, 1, "401811026"),
        inb(5, ODDS),
        out(6, predictor_url, 0, 2, "401811026"),
        inb(7, PREDICTOR),
        out(8, summary_url, 0, 3, "401811026"),
        inb(9, SUMMARY),
        // Cycle 1: discovery, then odds + predictor only (1 % 6 != 0). The odds
        // body is byte-identical to cycle 0's.
        out(10, scoreboard_url, 1, 0, ""),
        inb(11, SCOREBOARD),
        out(12, odds_url, 1, 1, "401811026"),
        inb(13, ODDS),
        out(14, predictor_url, 1, 2, "401811026"),
        inb(15, PREDICTOR),
    ];

    let (first_report, first_events) = replay_records(&records, None);
    let (second_report, second_events) = replay_records(&records, None);
    assert_eq!(first_report, second_report);
    assert_eq!(first_events, second_events);
    // Cycle 0: odds 1 + predictor 1 + summary 1 = 3. Cycle 1: odds 1 + predictor
    // 1 = 2. The two scoreboards and every outbound descriptor emit nothing.
    assert_eq!(first_report.events, 5);
    assert_eq!(first_report.recovery_notices, 0);

    let events: Vec<serde_json::Value> = String::from_utf8(first_events)
        .expect("normalized output is UTF-8")
        .lines()
        .map(|line| serde_json::from_str(line).expect("event line is JSON"))
        .collect();
    assert_eq!(events.len(), 5);
    assert_eq!(
        events
            .iter()
            .map(|event| event["source_id"].as_str().expect("source id"))
            .collect::<Vec<_>>(),
        vec![
            "nba.espn.games.odds",
            "nba.espn.games.predictor",
            "nba.espn.games.summary-context",
            "nba.espn.games.odds",
            "nba.espn.games.predictor",
        ]
    );

    // Odds: the open -> current movement survives the whole replay path as exact
    // decimal TEXT, never as a binary float.
    for (field, expected) in [
        ("espn_event_id", "401811026"),
        ("event_id_basis", "response_body"),
        ("odds_provider_name", "Draft Kings"),
        ("odds_total_line_open", "222.5"),
        ("odds_total_line_current", "225.5"),
        ("odds_spread_line_home_open", "-2.5"),
        ("odds_spread_line_home_current", "-6.5"),
        ("odds_moneyline_away_american_open", "114"),
        ("odds_moneyline_away_american_current", "180"),
        ("odds_moneyline_home_american_current", "-218"),
        ("odds_team_id_home", "30"),
    ] {
        assert_eq!(events[0]["values"][field], expected, "{field}");
    }
    // Predictor: published on 0-100, plus the catalog's 0-1 form, on the model's
    // own `lastModified` as the source event time.
    assert_eq!(events[1]["values"]["away_bpi_game_projection"], "46.722");
    assert_eq!(events[1]["values"]["away_bpi_win_prob"], "0.46722");
    assert_eq!(events[1]["values"]["bpi_matchup_quality"], "99.0");
    assert_eq!(
        events[1]["values"]["bpi_projection_source_mode"],
        "predictor_one_team"
    );
    assert_eq!(events[1]["event_time"], "2026-04-10T22:50:00.000Z");
    assert_eq!(events[1]["values"]["event_time_basis"], "source");
    // Summary: the working head-to-head source, fetched on cycle 0 only.
    assert_eq!(
        events[2]["values"]["h2h_series_summary_text"],
        "DET wins series 3-0"
    );
    assert_eq!(events[2]["values"]["h2h_prior_meeting_count"], 1);
    assert_eq!(
        events[2]["values"]["h2h_prior_meetings"][0]["winner_team_id"],
        "8"
    );
    assert_eq!(events[2]["values"]["game_venue_city"], "Charlotte");
    // NO DEDUP: cycle 1's identical odds body emits a second event carrying the
    // same values under a distinct, monotonic sequence.
    assert_eq!(events[3]["values"], events[0]["values"]);
    assert_eq!(events[0]["sequence"], 0);
    assert_eq!(events[3]["sequence"], 3);

    // SourceDefinition contract validation is intentionally omitted for the same
    // reason as the slice-1 and slice-2 NBA streams: the `nba.*` source
    // definitions are published by a later slice (definitions/nba/ does not exist
    // yet), and only then can `validate_events` be applied. Until then this pins
    // replay session mechanics, extraction and cross-run determinism.
}

#[test]
fn kraken_reference_families_are_contract_valid() {
    let records = vec![
        connection(CaptureStream::KrakenCore, 1),
        text(
            CaptureStream::KrakenCore,
            2,
            RecordKind::OutboundText,
            &kraken_book_subscription("BTC/USD", 100),
        ),
        text(
            CaptureStream::KrakenCore,
            3,
            RecordKind::OutboundText,
            &kraken_book_subscription("USDT/USD", 10),
        ),
        text(
            CaptureStream::KrakenCore,
            4,
            RecordKind::InboundText,
            r#"{"channel":"status","type":"update","data":[{"version":"2.0.10","system":"online","api_version":"v2","connection_id":7274766005081286947}]}"#,
        ),
        text(
            CaptureStream::KrakenCore,
            5,
            RecordKind::InboundText,
            r#"{"channel":"instrument","type":"snapshot","data":{"pairs":[{"symbol":"BTC/USD","base":"BTC","quote":"USD","status":"online","qty_increment":1E-8,"price_increment":0.1,"cost_min":0.50,"qty_min":0.00005000},{"symbol":"USDT/USD","base":"USDT","quote":"USD","status":"online","qty_increment":1E-8,"price_increment":0.00001,"cost_min":0.50,"qty_min":5.00000000}]}}"#,
        ),
        text(
            CaptureStream::KrakenCore,
            6,
            RecordKind::InboundText,
            r#"{"channel":"ticker","type":"snapshot","data":[{"symbol":"BTC/USD","bid":63547.5,"bid_qty":0.02,"ask":63547.6,"ask_qty":0.17,"last":63547.6,"volume":973.7,"vwap":63672.6,"low":63247.4,"high":64409.2,"change":-72.9,"change_pct":-0.11,"trades":46574,"timestamp":"2026-08-12T12:00:01Z"},{"symbol":"USDT/USD","bid":0.99892,"bid_qty":263826.27,"ask":0.99893,"ask_qty":480014.16,"last":0.99892,"volume":149118950.67,"vwap":0.99897,"low":0.99871,"high":0.99910,"change":-0.00016,"change_pct":-0.02,"trades":21617,"timestamp":"2026-08-12T12:00:01Z"}]}"#,
        ),
        text(
            CaptureStream::KrakenCore,
            7,
            RecordKind::InboundText,
            r#"{"channel":"ohlc","type":"update","timestamp":"2026-08-12T12:00:02Z","data":[{"symbol":"BTC/USD","open":63542.6,"high":63628.0,"low":63542.6,"close":63628.0,"trades":87,"volume":13.92440212,"vwap":63596.7,"interval_begin":"2026-08-12T11:59:00Z","interval":1,"timestamp":"2026-08-12T12:00:00Z"},{"symbol":"USDT/USD","open":0.99892,"high":0.99893,"low":0.99891,"close":0.99892,"trades":12,"volume":14399.97089998,"vwap":0.99892,"interval_begin":"2026-08-12T11:59:00Z","interval":1,"timestamp":"2026-08-12T12:00:00Z"}]}"#,
        ),
    ];

    let (report, output) = replay_records(&records, None);
    assert_eq!(report.configuration.kraken_btc_book_depth, Some(100));
    assert_eq!(report.configuration.kraken_usdt_book_depth, Some(10));
    assert_eq!(report.events, 7);
    assert_eq!(report.recovery_notices, 0);
    let definitions = BTreeMap::from([
        definition("kraken.spot.status", "1.0.0"),
        definition("kraken.spot.btc-usd.instrument", "1.0.0"),
        definition("kraken.spot.usdt-usd.instrument", "1.0.0"),
        definition("kraken.spot.btc-usd.ticker", "1.0.0"),
        definition("kraken.spot.usdt-usd.ticker", "1.0.0"),
        definition("kraken.spot.btc-usd.ohlc-1m", "1.0.0"),
        definition("kraken.spot.usdt-usd.ohlc-1m", "1.0.0"),
    ]);
    validate_events(&output, &definitions);
}

#[test]
fn kraken_usdt_reference_stream_emits_only_the_usdt_ticker() {
    // E47(e): the dedicated USDT/USD reference socket subscribes ONLY the
    // `ticker` channel on `USDT/USD`, so its capture carries nothing but the
    // USDT ticker frames and the shared Kraken adapter emits exactly
    // `kraken.spot.usdt-usd.ticker`. The USDT ticker row is the same wire shape
    // as the core socket's, so a `kraken-core` capture's USDT frames replay
    // through this adapter unchanged (used for the observed sanity check once a
    // Kraken USDT/USD hour is captured).
    let records = vec![
        connection(CaptureStream::KrakenUsdtReference, 1),
        text(
            CaptureStream::KrakenUsdtReference,
            2,
            RecordKind::OutboundText,
            r#"{"method":"subscribe","params":{"channel":"ticker","symbol":["USDT/USD"],"event_trigger":"bbo","snapshot":true},"req_id":1}"#,
        ),
        text(
            CaptureStream::KrakenUsdtReference,
            3,
            RecordKind::InboundText,
            r#"{"channel":"ticker","type":"snapshot","data":[{"symbol":"USDT/USD","bid":0.99892,"bid_qty":263826.27,"ask":0.99893,"ask_qty":480014.16,"last":0.99892,"volume":149118950.67,"vwap":0.99897,"low":0.99871,"high":0.99910,"change":-0.00016,"change_pct":-0.02,"trades":21617,"timestamp":"2026-08-12T12:00:01Z"}]}"#,
        ),
        text(
            CaptureStream::KrakenUsdtReference,
            4,
            RecordKind::InboundText,
            r#"{"channel":"ticker","type":"update","data":[{"symbol":"USDT/USD","bid":1.00028,"bid_qty":263826.27,"ask":1.00032,"ask_qty":480014.16,"last":1.00030,"volume":149118950.67,"vwap":0.99897,"low":0.99871,"high":1.00050,"change":0.00016,"change_pct":0.02,"trades":21618,"timestamp":"2026-08-12T12:00:02Z"}]}"#,
        ),
    ];

    let (report, output) = replay_records(&records, None);
    assert_eq!(report.events, 2);
    assert_eq!(report.recovery_notices, 0);
    let definitions = BTreeMap::from([definition("kraken.spot.usdt-usd.ticker", "1.0.0")]);
    validate_events(&output, &definitions);
}

#[test]
fn kraken_status_without_connection_id_is_accepted_without_an_event() {
    // Observed fleet-wide during Kraken's 2026-08-20 v2.0.10 deploy window: the
    // status row briefly omitted `connection_id`. The adapter accepts the
    // message with a notice and emits no status event (the contract declares
    // the field), instead of failing the run.
    let records = vec![
        connection(CaptureStream::KrakenCore, 1),
        text(
            CaptureStream::KrakenCore,
            2,
            RecordKind::OutboundText,
            &kraken_book_subscription("BTC/USD", 100),
        ),
        text(
            CaptureStream::KrakenCore,
            3,
            RecordKind::OutboundText,
            &kraken_book_subscription("USDT/USD", 10),
        ),
        text(
            CaptureStream::KrakenCore,
            4,
            RecordKind::InboundText,
            r#"{"channel":"status","type":"update","data":[{"version":"2.0.10","system":"online","api_version":"v2"}]}"#,
        ),
        text(
            CaptureStream::KrakenCore,
            5,
            RecordKind::InboundText,
            r#"{"channel":"status","type":"update","data":[{"version":"2.0.10","system":"online","api_version":"v2","connection_id":7274766005081286947}]}"#,
        ),
    ];

    let (report, output) = replay_records(&records, None);
    // The fieldless status yields no event; the complete one yields exactly one.
    assert_eq!(report.events, 1);
    assert_eq!(report.recovery_notices, 0);
    let definitions = BTreeMap::from([definition("kraken.spot.status", "1.0.0")]);
    validate_events(&output, &definitions);
}

#[test]
fn kraken_level_three_snapshot_is_checksum_and_contract_valid() {
    let records = vec![
        connection(CaptureStream::KrakenLevel3, 1),
        text(
            CaptureStream::KrakenLevel3,
            2,
            RecordKind::OutboundText,
            &kraken_level_three_subscription(10),
        ),
        text(
            CaptureStream::KrakenLevel3,
            3,
            RecordKind::InboundText,
            r#"{"channel":"level3","type":"snapshot","data":[{"symbol":"BTC/USD","checksum":2981471764,"timestamp":"2026-08-12T12:00:01Z","bids":[{"order_id":"BID-1","limit_price":"60000.0","order_qty":"1.0","timestamp":"2026-08-12T12:00:00Z"}],"asks":[{"order_id":"ASK-1","limit_price":"60001.0","order_qty":"2.0","timestamp":"2026-08-12T12:00:00Z"}]}]}"#,
        ),
    ];
    let (report, output) = replay_records(&records, None);
    assert_eq!(report.configuration.kraken_btc_book_depth, Some(10));
    assert_eq!(report.events, 2);
    assert_eq!(report.recovery_notices, 0);
    let definitions = BTreeMap::from([definition("kraken.spot.btc-usd.book-orders", "1.0.0")]);
    validate_events(&output, &definitions);
}

#[test]
fn kraken_checksum_gates_contract_valid_output() {
    let records = vec![
        connection(CaptureStream::KrakenCore, 1),
        text(
            CaptureStream::KrakenCore,
            2,
            RecordKind::OutboundText,
            &kraken_book_subscription("BTC/USD", 10),
        ),
        text(
            CaptureStream::KrakenCore,
            3,
            RecordKind::OutboundText,
            &kraken_book_subscription("USDT/USD", 10),
        ),
        text(
            CaptureStream::KrakenCore,
            4,
            RecordKind::InboundText,
            KRAKEN_CHECKSUM_SNAPSHOT,
        ),
        text(
            CaptureStream::KrakenCore,
            5,
            RecordKind::InboundText,
            r#"{"channel":"trade","type":"update","data":[{"symbol":"BTC/USD","side":"buy","price":45284.0,"qty":0.01000000,"ord_type":"limit","trade_id":1000,"timestamp":"2026-08-12T12:00:01Z"}]}"#,
        ),
        text(
            CaptureStream::KrakenCore,
            6,
            RecordKind::InboundText,
            r#"{"channel":"book","type":"update","data":[{"symbol":"BTC/USD","bids":[{"price":45283.5,"qty":0.20000000}],"asks":[],"checksum":0,"timestamp":"2026-08-12T12:00:02Z"}]}"#,
        ),
    ];

    let (report, output) = replay_records(&records, None);
    assert_eq!(report.events, 21);
    assert_eq!(report.recovery_notices, 1);

    let definitions = BTreeMap::from([
        definition("kraken.spot.btc-usd.book-levels", "1.0.0"),
        definition("kraken.spot.btc-usd.trades", "1.0.0"),
    ]);
    validate_events(&output, &definitions);
}

#[test]
fn replay_rejects_operator_depth_that_disagrees_with_capture() {
    let records = vec![
        connection(CaptureStream::KrakenLevel3, 1),
        text(
            CaptureStream::KrakenLevel3,
            2,
            RecordKind::OutboundText,
            &kraken_level_three_subscription(100),
        ),
    ];
    let mut input = Vec::new();
    for record in records {
        serde_json::to_writer(&mut input, &record).expect("test record serializes");
        input.push(b'\n');
    }
    let error = replay(
        &mut Cursor::new(input),
        &mut Vec::new(),
        "integration-test",
        Some(10),
    )
    .expect_err("depth mismatch must be explicit");
    assert!(error.to_string().contains("capture recorded 100"));
}

#[test]
fn polymarket_gamma_and_market_channel_are_deterministic_and_contract_valid() {
    const SLUG: &str = "btc-updown-5m-1786726500";
    const CONDITION_ID: &str = "0x0f85721d0123456789abcdef0123456789abcdef0123456789abcdef01234567";
    const QUESTION_ID: &str = "0x123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0";
    const UP_TOKEN: &str = "11111111111111111111111111111111111111111111111111111111111111111";
    const DOWN_TOKEN: &str = "22222222222222222222222222222222222222222222222222222222222222222";

    let gamma_body = serde_json::json!([{
        "id": "3566011",
        "question": "Bitcoin Up or Down - August 14, 12:55PM-1:00PM ET",
        "conditionId": CONDITION_ID,
        "slug": SLUG,
        "resolutionSource": "https://data.chain.link/streams/btc-usd-twap-60s-streams",
        "endDate": "2026-08-14T17:00:00Z",
        "description": "This market will resolve to \"Up\" if the time-weighted average price (TWAP) is greater than or equal to the opening value. Otherwise, it will resolve to \"Down\".",
        "outcomes": "[\"Up\", \"Down\"]",
        "active": true,
        "closed": false,
        "questionID": QUESTION_ID,
        "enableOrderBook": true,
        "orderPriceMinTickSize": 0.01,
        "orderMinSize": 5,
        "clobTokenIds": format!("[\"{UP_TOKEN}\", \"{DOWN_TOKEN}\"]"),
        "updatedAt": "2026-08-14T16:55:01Z",
        "events": [{
            "ticker": SLUG,
            "slug": SLUG,
            "startTime": "2026-08-14T16:55:00Z"
        }]
    }])
    .to_string();
    let gamma_hash = format!("{:x}", Sha256::digest(gamma_body.as_bytes()));
    let configuration_payload = serde_json::json!({
        "schema_version": 1,
        "kind": "polymarket_gamma_market_v1",
        "request_path": format!("/markets?slug={SLUG}&closed=false"),
        "retrieved_at": "2026-08-14T16:55:02Z",
        "gamma_body_sha256": gamma_hash,
        "gamma_body": gamma_body
    })
    .to_string();

    let records = vec![
        configuration(CaptureStream::PolymarketMarket, 1, &configuration_payload),
        connection(CaptureStream::PolymarketMarket, 2),
        text(
            CaptureStream::PolymarketMarket,
            3,
            RecordKind::OutboundText,
            &serde_json::json!({
                "assets_ids": [UP_TOKEN, DOWN_TOKEN],
                "type": "market",
                "custom_feature_enabled": true
            })
            .to_string(),
        ),
        text(
            CaptureStream::PolymarketMarket,
            4,
            RecordKind::OutboundText,
            "PING",
        ),
        text(
            CaptureStream::PolymarketMarket,
            5,
            RecordKind::InboundText,
            "PONG",
        ),
        text(
            CaptureStream::PolymarketMarket,
            6,
            RecordKind::InboundText,
            &serde_json::json!([{
                "event_type": "book",
                "asset_id": UP_TOKEN,
                "market": CONDITION_ID,
                "bids": [{"price": ".48", "size": "30"}],
                "asks": [{"price": ".52", "size": "25"}],
                "timestamp": 1786726500000_u64,
                "hash": "0xabc"
            }])
            .to_string(),
        ),
        text(
            CaptureStream::PolymarketMarket,
            7,
            RecordKind::InboundText,
            &serde_json::json!({
                "event_type": "price_change",
                "market": CONDITION_ID,
                "price_changes": [{
                    "asset_id": UP_TOKEN,
                    "price": ".49",
                    "size": "10",
                    "side": "BUY",
                    "best_bid": ".49",
                    "best_ask": ".52"
                }],
                "timestamp": "1786726501000"
            })
            .to_string(),
        ),
        text(
            CaptureStream::PolymarketMarket,
            8,
            RecordKind::InboundText,
            &serde_json::json!({
                "event_type": "last_trade_price",
                "asset_id": UP_TOKEN,
                "market": CONDITION_ID,
                "transaction_hash": format!("0x{}", "a".repeat(64)),
                "price": ".51",
                "size": "2",
                "side": "BUY",
                "fee_rate_bps": "0",
                "timestamp": "1786726502000"
            })
            .to_string(),
        ),
        text(
            CaptureStream::PolymarketMarket,
            9,
            RecordKind::InboundText,
            &serde_json::json!({
                "event_type": "last_trade_price",
                "asset_id": UP_TOKEN,
                "market": CONDITION_ID,
                "transaction_hash": format!("0x{}", "b".repeat(64)),
                "price": ".51",
                "size": "2",
                "side": "BUY",
                "fee_rate_bps": "0",
                "timestamp": "1786726502000"
            })
            .to_string(),
        ),
        text(
            CaptureStream::PolymarketMarket,
            10,
            RecordKind::InboundText,
            &serde_json::json!({
                "event_type": "last_trade_price",
                "asset_id": UP_TOKEN,
                "market": CONDITION_ID,
                "transaction_hash": format!("0x{}", "a".repeat(64)),
                "price": ".51",
                "size": "2",
                "side": "BUY",
                "fee_rate_bps": "0",
                "timestamp": "1786726502000"
            })
            .to_string(),
        ),
        text(
            CaptureStream::PolymarketMarket,
            11,
            RecordKind::InboundText,
            &serde_json::json!({
                "event_type": "best_bid_ask",
                "asset_id": DOWN_TOKEN,
                "market": CONDITION_ID,
                "best_bid": ".47",
                "best_ask": ".50",
                "timestamp": "1786726503000"
            })
            .to_string(),
        ),
    ];

    let first = replay_records(&records, None);
    let second = replay_records(&records, None);
    assert_eq!(first, second);
    assert_eq!(first.0.events, 9);
    assert_eq!(first.0.recovery_notices, 0);

    let definitions = BTreeMap::from([
        definition("polymarket.btc.market-contracts", "1.0.0"),
        definition("polymarket.btc.book-levels", "1.0.0"),
        definition("polymarket.btc.quotes", "1.0.0"),
        definition("polymarket.btc.trades", "1.0.0"),
    ]);
    validate_events(&first.1, &definitions);
}

#[test]
fn polymarket_chainlink_twap_is_deterministic_and_contract_valid() {
    let records = vec![
        connection(CaptureStream::PolymarketChainlinkTwap, 1),
        text(
            CaptureStream::PolymarketChainlinkTwap,
            2,
            RecordKind::OutboundText,
            r#"{"action":"subscribe","subscriptions":[{"filters":"{\"symbol\":\"btc/usd\"}","topic":"crypto_prices_twap_sixty","type":"update"}]}"#,
        ),
        // The live endpoint acknowledges the exact subscription with an empty text frame.
        text(
            CaptureStream::PolymarketChainlinkTwap,
            3,
            RecordKind::InboundText,
            "",
        ),
        text(
            CaptureStream::PolymarketChainlinkTwap,
            4,
            RecordKind::InboundText,
            r#"{"connection_id":"retained-but-not-normalized","payload":{"full_accuracy_value":"63018956219347172851712","symbol":"btc/usd","timestamp":1786810469000,"value":1.0,"window_s":60},"timestamp":1786810470045,"topic":"crypto_prices_twap_sixty","type":"update"}"#,
        ),
        disconnection(CaptureStream::PolymarketChainlinkTwap, 5),
    ];

    let first = replay_records(&records, None);
    let second = replay_records(&records, None);
    assert_eq!(first, second);
    assert_eq!(first.0.events, 1);
    assert_eq!(first.0.notices, 3);
    assert_eq!(first.0.recovery_notices, 0);

    let definitions = BTreeMap::from([definition("polymarket.rtds.btc-usd.twap-60s", "1.0.0")]);
    validate_events(&first.1, &definitions);
    let event: SourceEvent =
        serde_json::from_slice(first.1.split(|byte| *byte == b'\n').next().unwrap())
            .expect("TWAP event deserializes");
    assert_eq!(
        event.values["twap_price"],
        serde_json::json!("63018.956219347172851712")
    );
}

#[test]
fn polymarket_chainlink_twap_retains_the_backfill_frame_without_normalizing_it() {
    // Synthetic reconstruction of the 2026-09-01 RTDS change: after the empty
    // subscription acknowledgement the relay sends one `type":"subscribe"`
    // backfill frame whose payload pins the same btc/usd 60-second window and
    // carries a `data` array of historical points, before the unchanged
    // per-second `update` frames. The keys are the observed shape; the values
    // here are invented, not captured.
    let backfill = serde_json::json!({
        "payload": {
            "data": [
                {
                    "full_accuracy_value": "65000500000000000000000",
                    "timestamp": 1786726748000_i64,
                    "value": 65000.5
                },
                {
                    "full_accuracy_value": "65001500000000000000000",
                    "timestamp": 1786726749000_i64,
                    "value": 65001.5
                }
            ],
            "symbol": "btc/usd",
            "window_s": 60
        },
        "timestamp": 1786726749675_i64,
        "topic": "crypto_prices_twap_sixty",
        "type": "subscribe"
    })
    .to_string();
    let records = vec![
        connection(CaptureStream::PolymarketChainlinkTwap, 1),
        text(
            CaptureStream::PolymarketChainlinkTwap,
            2,
            RecordKind::OutboundText,
            r#"{"action":"subscribe","subscriptions":[{"filters":"{\"symbol\":\"btc/usd\"}","topic":"crypto_prices_twap_sixty","type":"update"}]}"#,
        ),
        text(
            CaptureStream::PolymarketChainlinkTwap,
            3,
            RecordKind::InboundText,
            "",
        ),
        text(
            CaptureStream::PolymarketChainlinkTwap,
            4,
            RecordKind::InboundText,
            &backfill,
        ),
        text(
            CaptureStream::PolymarketChainlinkTwap,
            5,
            RecordKind::InboundText,
            r#"{"connection_id":"retained-but-not-normalized","payload":{"full_accuracy_value":"63018956219347172851712","symbol":"btc/usd","timestamp":1786726810000,"value":1.0,"window_s":60},"timestamp":1786726810045,"topic":"crypto_prices_twap_sixty","type":"update"}"#,
        ),
        disconnection(CaptureStream::PolymarketChainlinkTwap, 6),
    ];

    let first = replay_records(&records, None);
    let second = replay_records(&records, None);
    assert_eq!(first, second);
    // The backfill frame adds exactly one notice and no event: connected,
    // subscription accepted, backfill retained, disconnected.
    assert_eq!(first.0.events, 1);
    assert_eq!(first.0.notices, 4);
    assert_eq!(first.0.recovery_notices, 0);

    let definitions = BTreeMap::from([definition("polymarket.rtds.btc-usd.twap-60s", "1.0.0")]);
    validate_events(&first.1, &definitions);
    let events: Vec<SourceEvent> = first
        .1
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).expect("TWAP event deserializes"))
        .collect();
    // The single event is the live update, never an opening strike synthesized
    // from the backfill points.
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sequence, 0);
    assert_eq!(
        events[0].values["twap_price"],
        serde_json::json!("63018.956219347172851712")
    );
    assert_eq!(
        events[0].values["event_time"],
        serde_json::json!("2026-08-14T17:00:10.000Z")
    );
}

#[test]
fn kalshi_rest_and_authenticated_market_channel_are_deterministic_and_contract_valid() {
    const TICKER: &str = "KXBTC15M-26AUG141315-15";
    let rest_body = serde_json::json!({
        "markets": [{
            "ticker": TICKER,
            "event_ticker": "KXBTC15M-26AUG141315",
            "market_type": "binary",
            "title": "Bitcoin price at 1:15 PM EDT?",
            "status": "active",
            "open_time": "2026-08-14T17:00:00Z",
            "close_time": "2026-08-14T17:15:00Z",
            "expected_expiration_time": "2026-08-14T17:20:00Z",
            "expiration_time": "2026-08-21T17:15:00Z",
            "updated_time": "2026-08-14T17:00:01Z",
            "rules_primary": "CF Benchmarks BRTI simple average over sixty seconds; YES if the value is at least the strike.",
            "rules_secondary": "The exchange uses 60 RTI prices.",
            "strike_type": "greater_or_equal",
            "floor_strike": 63075.56
        }],
        "cursor": ""
    })
    .to_string();
    let configuration_payload = serde_json::json!({
        "schema_version": 1,
        "kind": "kalshi_btc_market_v1",
        "request_path": format!("/trade-api/v2/markets?tickers={TICKER}"),
        "retrieved_at": "2026-08-14T17:00:02Z",
        "response_body_sha256": format!("{:x}", Sha256::digest(rest_body.as_bytes())),
        "response_body": rest_body
    })
    .to_string();

    let records = vec![
        configuration(CaptureStream::KalshiMarket, 1, &configuration_payload),
        connection(CaptureStream::KalshiMarket, 2),
        text(
            CaptureStream::KalshiMarket,
            3,
            RecordKind::OutboundText,
            &serde_json::json!({
                "id": 1,
                "cmd": "subscribe",
                "params": {
                    "channels": ["orderbook_delta"],
                    "market_ticker": TICKER,
                    "use_yes_price": true
                }
            })
            .to_string(),
        ),
        text(
            CaptureStream::KalshiMarket,
            4,
            RecordKind::OutboundText,
            &serde_json::json!({
                "id": 2,
                "cmd": "subscribe",
                "params": {"channels": ["ticker"], "market_ticker": TICKER}
            })
            .to_string(),
        ),
        text(
            CaptureStream::KalshiMarket,
            5,
            RecordKind::OutboundText,
            &serde_json::json!({
                "id": 3,
                "cmd": "subscribe",
                "params": {"channels": ["trade"], "market_ticker": TICKER}
            })
            .to_string(),
        ),
        text(
            CaptureStream::KalshiMarket,
            6,
            RecordKind::InboundText,
            &serde_json::json!({
                "type": "orderbook_snapshot",
                "sid": 11,
                "seq": 2,
                "msg": {
                    "market_ticker": TICKER,
                    "yes_dollars_fp": [["0.4500", "10.00"]],
                    "no_dollars_fp": [["0.5500", "12.00"]]
                }
            })
            .to_string(),
        ),
        text(
            CaptureStream::KalshiMarket,
            7,
            RecordKind::InboundText,
            &serde_json::json!({
                "type": "orderbook_delta",
                "sid": 11,
                "seq": 3,
                "msg": {
                    "market_ticker": TICKER,
                    "price_dollars": "0.4500",
                    "delta_fp": "-2.00",
                    "side": "yes",
                    "ts": "2026-08-14T17:00:03Z",
                    "ts_ms": 1786726803000_u64
                }
            })
            .to_string(),
        ),
        text(
            CaptureStream::KalshiMarket,
            8,
            RecordKind::InboundText,
            &serde_json::json!({
                "type": "ticker",
                "sid": 12,
                "msg": {
                    "market_ticker": TICKER,
                    "price_dollars": "0.5000",
                    "yes_bid_dollars": "0.4500",
                    "yes_ask_dollars": "0.5500",
                    "volume_fp": "100.00",
                    "open_interest_fp": "50.00",
                    "yes_bid_size_fp": "8.00",
                    "yes_ask_size_fp": "12.00",
                    "ts_ms": 1786726803000_u64,
                    "time": "2026-08-14T17:00:03Z"
                }
            })
            .to_string(),
        ),
        text(
            CaptureStream::KalshiMarket,
            9,
            RecordKind::InboundText,
            &serde_json::json!({
                "type": "ticker",
                "sid": 12,
                "msg": {
                    "market_ticker": TICKER,
                    "price_dollars": "0.5100",
                    "yes_bid_dollars": "0.4600",
                    "yes_ask_dollars": "0.5600",
                    "volume_fp": "101.00",
                    "open_interest_fp": "51.00",
                    "yes_bid_size_fp": "9.00",
                    "yes_ask_size_fp": "11.00",
                    "ts_ms": 1786726803000_u64,
                    "time": "2026-08-14T17:00:03Z"
                }
            })
            .to_string(),
        ),
        text(
            CaptureStream::KalshiMarket,
            10,
            RecordKind::InboundText,
            &serde_json::json!({
                "type": "ticker",
                "sid": 12,
                "msg": {
                    "market_ticker": TICKER,
                    "price_dollars": "0.5000",
                    "yes_bid_dollars": "0.4500",
                    "yes_ask_dollars": "0.5500",
                    "volume_fp": "100.00",
                    "open_interest_fp": "50.00",
                    "yes_bid_size_fp": "8.00",
                    "yes_ask_size_fp": "12.00",
                    "ts_ms": 1786726803000_u64,
                    "time": "2026-08-14T17:00:03Z"
                }
            })
            .to_string(),
        ),
        text(
            CaptureStream::KalshiMarket,
            11,
            RecordKind::InboundText,
            &serde_json::json!({
                "type": "trade",
                "sid": 13,
                "msg": {
                    "trade_id": "d91bc706-ee49-470d-82d8-11418bda6fed",
                    "market_ticker": TICKER,
                    "yes_price_dollars": "0.5000",
                    "no_price_dollars": "0.5000",
                    "count_fp": "2.00",
                    "taker_outcome_side": "yes",
                    "taker_book_side": "bid",
                    "is_block_trade": false,
                    "ts_ms": 1786726804000_u64
                }
            })
            .to_string(),
        ),
    ];

    let first = replay_records(&records, None);
    let second = replay_records(&records, None);
    assert_eq!(first, second);
    assert_eq!(first.0.events, 7);
    assert_eq!(first.0.recovery_notices, 0);
    let quotes = first
        .1
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice::<SourceEvent>(line).expect("event decodes"))
        .filter(|event| event.source_id == "kalshi.btc.quotes")
        .collect::<Vec<_>>();
    assert_eq!(quotes.len(), 2, "A/B/A at one millisecond emits A then B");
    assert_eq!(quotes[1].values["last_price"], "0.51");

    let definitions = BTreeMap::from([
        definition("kalshi.btc.market-contracts", "1.0.0"),
        definition("kalshi.btc.book-levels", "1.0.0"),
        definition("kalshi.btc.quotes", "1.0.0"),
        definition("kalshi.btc.trades", "1.0.0"),
    ]);
    validate_events(&first.1, &definitions);
}

// Kalshi omits the empty-side key from a one-sided `orderbook_snapshot` rather
// than sending `[]` (soak-caught 2026-08-17, scratchpad/kalshi-diag.jsonl). The
// absent array parses as an empty side via `#[serde(default)]` and feeds the
// established one-sided book handling: `total` is the present side's row count,
// `is_uncrossed` falls through to `true` with one map empty, and only the
// present side emits book-level events.
fn kalshi_one_sided_snapshot_records(snapshot_msg: serde_json::Value) -> Vec<RawRecord> {
    const TICKER: &str = "KXBTC15M-26AUG141315-15";
    let rest_body = serde_json::json!({
        "markets": [{
            "ticker": TICKER,
            "event_ticker": "KXBTC15M-26AUG141315",
            "market_type": "binary",
            "title": "Bitcoin price at 1:15 PM EDT?",
            "status": "active",
            "open_time": "2026-08-14T17:00:00Z",
            "close_time": "2026-08-14T17:15:00Z",
            "expected_expiration_time": "2026-08-14T17:20:00Z",
            "expiration_time": "2026-08-21T17:15:00Z",
            "updated_time": "2026-08-14T17:00:01Z",
            "rules_primary": "CF Benchmarks BRTI simple average over sixty seconds; YES if the value is at least the strike.",
            "rules_secondary": "The exchange uses 60 RTI prices.",
            "strike_type": "greater_or_equal",
            "floor_strike": 63075.56
        }],
        "cursor": ""
    })
    .to_string();
    let configuration_payload = serde_json::json!({
        "schema_version": 1,
        "kind": "kalshi_btc_market_v1",
        "request_path": format!("/trade-api/v2/markets?tickers={TICKER}"),
        "retrieved_at": "2026-08-14T17:00:02Z",
        "response_body_sha256": format!("{:x}", Sha256::digest(rest_body.as_bytes())),
        "response_body": rest_body
    })
    .to_string();

    vec![
        configuration(CaptureStream::KalshiMarket, 1, &configuration_payload),
        connection(CaptureStream::KalshiMarket, 2),
        text(
            CaptureStream::KalshiMarket,
            3,
            RecordKind::OutboundText,
            &serde_json::json!({
                "id": 1,
                "cmd": "subscribe",
                "params": {
                    "channels": ["orderbook_delta"],
                    "market_ticker": TICKER,
                    "use_yes_price": true
                }
            })
            .to_string(),
        ),
        text(
            CaptureStream::KalshiMarket,
            4,
            RecordKind::OutboundText,
            &serde_json::json!({
                "id": 2,
                "cmd": "subscribe",
                "params": {"channels": ["ticker"], "market_ticker": TICKER}
            })
            .to_string(),
        ),
        text(
            CaptureStream::KalshiMarket,
            5,
            RecordKind::OutboundText,
            &serde_json::json!({
                "id": 3,
                "cmd": "subscribe",
                "params": {"channels": ["trade"], "market_ticker": TICKER}
            })
            .to_string(),
        ),
        text(
            CaptureStream::KalshiMarket,
            6,
            RecordKind::InboundText,
            &serde_json::json!({
                "type": "orderbook_snapshot",
                "sid": 11,
                "seq": 2,
                "msg": snapshot_msg
            })
            .to_string(),
        ),
    ]
}

#[test]
fn kalshi_one_sided_snapshot_no_absent_normalizes_to_bid_only_book() {
    const TICKER: &str = "KXBTC15M-26AUG141315-15";
    // NO side omitted entirely: only the YES (bid) side is present on the wire.
    let records = kalshi_one_sided_snapshot_records(serde_json::json!({
        "market_ticker": TICKER,
        "yes_dollars_fp": [["0.4500", "10.00"]]
    }));

    let first = replay_records(&records, None);
    let second = replay_records(&records, None);
    assert_eq!(first, second);
    // One REST market-contract event plus one bid-side book level.
    assert_eq!(first.0.events, 2);
    assert_eq!(first.0.recovery_notices, 0);

    let definitions = BTreeMap::from([
        definition("kalshi.btc.market-contracts", "1.0.0"),
        definition("kalshi.btc.book-levels", "1.0.0"),
    ]);
    validate_events(&first.1, &definitions);
}

#[test]
fn kalshi_one_sided_snapshot_yes_absent_normalizes_to_ask_only_book() {
    const TICKER: &str = "KXBTC15M-26AUG141315-15";
    // YES side omitted entirely: only the NO (ask) side is present on the wire.
    let records = kalshi_one_sided_snapshot_records(serde_json::json!({
        "market_ticker": TICKER,
        "no_dollars_fp": [["0.5500", "12.00"]]
    }));

    let first = replay_records(&records, None);
    let second = replay_records(&records, None);
    assert_eq!(first, second);
    // One REST market-contract event plus one ask-side book level.
    assert_eq!(first.0.events, 2);
    assert_eq!(first.0.recovery_notices, 0);

    let definitions = BTreeMap::from([
        definition("kalshi.btc.market-contracts", "1.0.0"),
        definition("kalshi.btc.book-levels", "1.0.0"),
    ]);
    validate_events(&first.1, &definitions);
}

#[test]
fn coinbase_heartbeat_detector_is_dormant_without_heartbeat_records() {
    // Synthetic heartbeat-free Coinbase capture (item 1, SIG-041). The
    // completeness cross-check only fires from heartbeat records; a capture
    // that carries none gives it nothing to observe, so retained heartbeat-free
    // replays stay dormant and produce zero recovery notices. The two matches
    // are contiguous, so the per-message gap detector also stays silent.
    let records = vec![
        connection(CaptureStream::CoinbaseCore, 1),
        text(
            CaptureStream::CoinbaseCore,
            2,
            RecordKind::InboundText,
            r#"{"type":"match","trade_id":100,"sequence":900,"product_id":"BTC-USD","price":"60000.50","size":"0.0100","side":"sell","time":"2026-08-12T12:00:01Z"}"#,
        ),
        text(
            CaptureStream::CoinbaseCore,
            3,
            RecordKind::InboundText,
            r#"{"type":"match","trade_id":101,"sequence":901,"product_id":"BTC-USD","price":"60000.60","size":"0.0100","side":"buy","time":"2026-08-12T12:00:02Z"}"#,
        ),
    ];

    let first = replay_records(&records, None);
    let second = replay_records(&records, None);
    assert_eq!(first, second);
    assert_eq!(first.0.events, 2);
    assert_eq!(first.0.recovery_notices, 0);

    let definitions = BTreeMap::from([definition("coinbase.exchange.btc-usd.trades", "1.0.0")]);
    validate_events(&first.1, &definitions);
}

#[test]
fn kraken_rest_reconciliation_never_enters_replay_state() {
    // The Kraken public-trade reconciliation (item 2, ADR 0013) is a
    // live-runner-only, observe-only maintenance job whose state never enters
    // admitted records. The replay path never constructs it, so a KrakenCore
    // capture carrying BTC trades replays byte-identically and its normalized
    // output is exactly the twenty book events plus two trades, with no extra
    // events, notices, or recovery attributable to reconciliation.
    let records = vec![
        connection(CaptureStream::KrakenCore, 1),
        text(
            CaptureStream::KrakenCore,
            2,
            RecordKind::OutboundText,
            &kraken_book_subscription("BTC/USD", 10),
        ),
        text(
            CaptureStream::KrakenCore,
            3,
            RecordKind::OutboundText,
            &kraken_book_subscription("USDT/USD", 10),
        ),
        text(
            CaptureStream::KrakenCore,
            4,
            RecordKind::InboundText,
            KRAKEN_CHECKSUM_SNAPSHOT,
        ),
        text(
            CaptureStream::KrakenCore,
            5,
            RecordKind::InboundText,
            r#"{"channel":"trade","type":"update","data":[{"symbol":"BTC/USD","side":"buy","price":45284.0,"qty":0.01000000,"ord_type":"limit","trade_id":1000,"timestamp":"2026-08-12T12:00:01Z"}]}"#,
        ),
        text(
            CaptureStream::KrakenCore,
            6,
            RecordKind::InboundText,
            r#"{"channel":"trade","type":"update","data":[{"symbol":"BTC/USD","side":"sell","price":45285.0,"qty":0.02000000,"ord_type":"market","trade_id":1001,"timestamp":"2026-08-12T12:00:02Z"}]}"#,
        ),
    ];

    let first = replay_records(&records, None);
    let second = replay_records(&records, None);
    assert_eq!(first, second);
    assert_eq!(first.0.events, 22);
    assert_eq!(first.0.recovery_notices, 0);

    let definitions = BTreeMap::from([
        definition("kraken.spot.btc-usd.book-levels", "1.0.0"),
        definition("kraken.spot.btc-usd.trades", "1.0.0"),
    ]);
    validate_events(&first.1, &definitions);
}

// --- SportsDataIO NBA capture-only slice (gate SDIO-1) contract tests.
//
// The real credential never enters this file, this test process, or any fixture.
// Every test below injects a SENTINEL that is deliberately NOT key-shaped -- it
// is not 32 hexadecimal characters and cannot be confused for one by the secret
// scanner -- and no test here reads `SPORTSDATAIO_NBA_API_KEY` out of the ambient
// environment or issues a request to `api.sportsdata.io`.
const SPORTSDATAIO_SENTINEL: &str = "SENTINEL-KEY-DO-NOT-LEAK-7f3a";

#[test]
fn sportsdataio_nba_replay_is_deterministic_over_a_two_cycle_matrix() {
    use btc_feeds::sportsdataio_nba::{
        Operation, RequestParameters, ResponseOutcome, outbound_descriptor, request_url,
        response_envelope,
    };

    // Minimal synthetic SDIO-shaped bodies. The FIELD NAMES mirror the provider's
    // documented NBA JSON; the VALUES are invented, because this slice is
    // capture-only: gate SDIO-2, not this test, is where recorded values are
    // compared against an independent authoritative record. No blobs in the repo.
    const TEAMS: &str = r#"[{"TeamID":1,"Key":"DET","City":"Detroit","Name":"Pistons"},{"TeamID":2,"Key":"CHA","City":"Charlotte","Name":"Hornets"}]"#;
    const ODDS_OBJECT: &str = r#"{"GameID":21500,"PregameOdds":[]}"#;
    // A 2xx body that is not JSON at all -- an upstream HTML error page served
    // with a 200. It is preserved as evidence, noticed, and never parsed as data.
    const MALFORMED: &str = "<html><body>upstream gateway error</body></html>";
    // A 429 body. NOTHING in it may reach the evidence row.
    const RATE_LIMITED: &str = r#"{"Message":"Rate limit is exceeded. Try again in 1 seconds."}"#;

    fn games_by_date(status: &str, home: u32, away: u32) -> String {
        format!(
            r#"[{{"GameID":21500,"Status":"{status}","HomeTeamID":1,"AwayTeamID":2,"HomeTeamScore":{home},"AwayTeamScore":{away}}}]"#
        )
    }

    // Four matrix entries, held fixed and ordered for the whole run, so `index`
    // identifies an entry's parameters and `(operation, index)` is its stable key.
    let date = || RequestParameters::Date("2026-01-15".to_owned());
    let matrix = [
        (Operation::Teams, RequestParameters::None),
        (Operation::GamesByDateFinal, date()),
        (Operation::BoxScoresFinal, date()),
        (Operation::GameOddsByDate, date()),
    ];
    // One response per (cycle, matrix index).
    //  * teams repeats byte for byte  -> the second cycle is marked unchanged;
    //  * games_by_date_final changes  -> the second cycle is marked changed;
    //  * box_scores_final is rate limited, then answers an odd but legal `[]`;
    //  * game_odds_by_date answers a non-JSON 2xx, then a JSON object.
    let responses: [[(u16, ResponseOutcome, String); 4]; 2] = [
        [
            (200, ResponseOutcome::Response, TEAMS.to_owned()),
            (
                200,
                ResponseOutcome::Response,
                games_by_date("Scheduled", 0, 0),
            ),
            (429, ResponseOutcome::Response, RATE_LIMITED.to_owned()),
            (200, ResponseOutcome::Response, MALFORMED.to_owned()),
        ],
        [
            (200, ResponseOutcome::Response, TEAMS.to_owned()),
            (
                200,
                ResponseOutcome::Response,
                games_by_date("Final", 112, 108),
            ),
            (200, ResponseOutcome::Response, "[]".to_owned()),
            (200, ResponseOutcome::Response, ODDS_OBJECT.to_owned()),
        ],
    ];

    let mut records = vec![connection(CaptureStream::SportsDataIoNba, 1)];
    let mut ordinal = 2;
    for (offset, cycle_responses) in responses.iter().enumerate() {
        let cycle = offset as u64 + 1;
        for (index, ((operation, parameters), (status, outcome, body))) in
            matrix.iter().zip(cycle_responses.iter()).enumerate()
        {
            // The descriptor is recorded BEFORE the GET, and is built from the
            // same allowlisted URL the capture would have requested.
            let url = request_url(*operation, parameters).expect("compiled template is legal");
            records.push(text(
                CaptureStream::SportsDataIoNba,
                ordinal,
                RecordKind::OutboundText,
                &outbound_descriptor(&url, *operation, cycle, index),
            ));
            ordinal += 1;
            let mut headers =
                BTreeMap::from([("content-type".to_owned(), "application/json".to_owned())]);
            if *status == 429 {
                headers.insert("retry-after".to_owned(), "1".to_owned());
            }
            records.push(text(
                CaptureStream::SportsDataIoNba,
                ordinal,
                RecordKind::InboundText,
                &response_envelope(
                    *operation,
                    cycle,
                    index,
                    &format!("2026-09-01T0{cycle}:0{index}:00.000Z"),
                    *status,
                    *outcome,
                    headers,
                    body.as_bytes(),
                    false,
                    false,
                ),
            ));
            ordinal += 1;
        }
    }

    let (first_report, first_events) = replay_records(&records, None);
    let (second_report, second_events) = replay_records(&records, None);
    assert_eq!(first_report, second_report);
    assert_eq!(first_events, second_events);
    assert_eq!(first_report.records, 17);
    // One evidence row per recorded response; outbound descriptors emit none.
    assert_eq!(first_report.events, 8);
    // Connected, one 429 venue status, one non-JSON 2xx body.
    assert_eq!(first_report.notices, 3);
    assert_eq!(first_report.recovery_notices, 0);

    let events: Vec<serde_json::Value> = String::from_utf8(first_events)
        .expect("normalized output is UTF-8")
        .lines()
        .map(|line| serde_json::from_str(line).expect("event line is JSON"))
        .collect();
    assert_eq!(events.len(), 8);

    // One evidence series per operation, in matrix order, twice.
    let source_ids: Vec<&str> = events
        .iter()
        .map(|event| event["source_id"].as_str().expect("source_id is a string"))
        .collect();
    assert_eq!(
        source_ids,
        vec![
            "nba.sportsdataio.teams",
            "nba.sportsdataio.games_by_date_final",
            "nba.sportsdataio.box_scores_final",
            "nba.sportsdataio.game_odds_by_date",
            "nba.sportsdataio.teams",
            "nba.sportsdataio.games_by_date_final",
            "nba.sportsdataio.box_scores_final",
            "nba.sportsdataio.game_odds_by_date",
        ]
    );
    for (position, event) in events.iter().enumerate() {
        assert_eq!(event["sequence"], position as u64);
        assert_eq!(event["values"]["matrix_index"], (position % 4) as u64);
        assert_eq!(event["values"]["cycle"], (position / 4) as u64 + 1);
    }

    // A byte-identical repeat is still one row, marked unchanged; a changed body
    // is marked changed. Change detection is per matrix ENTRY, not per operation.
    assert_eq!(events[0]["values"]["body_changed"], true);
    assert_eq!(events[4]["values"]["body_changed"], false);
    assert_eq!(events[1]["values"]["body_changed"], true);
    assert_eq!(events[5]["values"]["body_changed"], true);

    // The 429 is recorded as STATUS EVIDENCE only: nothing in the provider's
    // error body is turned into a value, and no item count is invented for it.
    let rate_limited = &events[2];
    assert_eq!(rate_limited["values"]["status"], 429);
    assert_eq!(rate_limited["values"]["payload_shape"], "status_evidence");
    assert!(rate_limited["values"].get("payload_items").is_none());
    assert_eq!(rate_limited["values"]["header_retry_after"], "1");
    assert!(!rate_limited.to_string().contains("Rate limit is exceeded"));

    // The non-JSON 2xx is preserved as evidence, classified as malformed, and its
    // bytes are likewise never lifted into the row.
    let malformed = &events[3];
    assert_eq!(malformed["values"]["status"], 200);
    assert_eq!(malformed["values"]["payload_shape"], "malformed");
    assert!(malformed["values"].get("payload_items").is_none());
    assert!(!malformed.to_string().contains("upstream gateway error"));

    // An odd but legal empty array counts zero items rather than being refused.
    assert_eq!(events[6]["values"]["payload_shape"], "array");
    assert_eq!(events[6]["values"]["payload_items"], 0);
    assert_eq!(events[7]["values"]["payload_shape"], "object");
    // Sizes and digests travel; provider payloads do not.
    assert_eq!(events[0]["values"]["body_bytes"], TEAMS.len() as u64);
    assert_eq!(
        events[0]["values"]["body_sha256"],
        format!("{:x}", Sha256::digest(TEAMS.as_bytes()))
    );
    assert!(!events[0].to_string().contains("Pistons"));

    // The normalization analyzer groups the same journal one series per
    // operation, under the stream's `sportsdataio_nba:` prefix.
    let mut journal = Vec::new();
    for record in &records {
        serde_json::to_writer(&mut journal, record).expect("test record serializes");
        journal.push(b'\n');
    }
    let analysis = btc_feeds::normalization::analyze(&mut Cursor::new(journal), "1", None)
        .expect("analysis completes")
        .expect("journal is non-empty");
    let mut series: Vec<&str> = analysis
        .series
        .iter()
        .map(|entry| entry.series.as_str())
        .collect();
    series.sort_unstable();
    assert_eq!(
        series,
        vec![
            "sportsdataio_nba:box_scores_final",
            "sportsdataio_nba:game_odds_by_date",
            "sportsdataio_nba:games_by_date_final",
            "sportsdataio_nba:teams",
        ]
    );

    // SourceDefinition contract validation is intentionally omitted, as for the
    // ESPN slices: the `nba.*` source definitions are published by a later slice
    // (definitions/nba/ does not exist), and this slice is candidate/capture-only
    // besides. This pins replay mechanics, evidence shape and determinism.
}

#[test]
fn sportsdataio_secret_never_leaks() {
    use btc_feeds::sportsdataio_auth::{ENVIRONMENT_VARIABLE, SUBSCRIPTION_KEY_HEADER};
    use btc_feeds::sportsdataio_nba::{
        API_ROOT, OPERATIONS, Operation, ParameterKind, RequestMatrix, RequestParameters,
        ResponseOutcome, outbound_descriptor, request_url, response_envelope,
    };
    use btc_feeds::{SportsDataIoAuthError, SportsDataIoCredential, redact_secret};

    // The sentinel is injected the way the module documents for tests. NOTE:
    // `std::env::set_var` is unusable in this crate -- it is an `unsafe fn` under
    // edition 2024 and the workspace sets `unsafe_code = "forbid"`, which an
    // `#[allow]` cannot lift and `Cargo.toml` may not be changed to relax. The
    // real `from_env` path is therefore exercised at the end of this test in a
    // CHILD PROCESS whose environment carries the sentinel, which also proves the
    // gate's actual exit criterion for stdout and stderr.
    let credential = SportsDataIoCredential::from_value(SPORTSDATAIO_SENTINEL.to_owned())
        .expect("the sentinel is a well-formed credential value");

    // 1. Debug renders the placeholder and nothing else. There is deliberately no
    //    `Display`, so `format!("{credential}")` does not compile at all.
    assert_eq!(format!("{credential:?}"), "<redacted>");

    // 2. An error string that embeds the key twice comes back with neither.
    let leaked = format!(
        "GET {API_ROOT}scores/json/teams failed presenting {SPORTSDATAIO_SENTINEL} \
         (retry presented {SPORTSDATAIO_SENTINEL})"
    );
    let redacted = redact_secret(&leaked, &credential);
    assert!(!redacted.contains(SPORTSDATAIO_SENTINEL));
    assert_eq!(redacted.matches("<redacted>").count(), 2);

    // 3. Every allowlisted URL: inside the root, no query string, no `key=`, no
    //    credential. A URL is where a `?key=` would hide, so this covers all 13.
    for operation in OPERATIONS {
        let parameters = match operation.parameter_kind() {
            ParameterKind::None => RequestParameters::None,
            ParameterKind::Season => RequestParameters::Season("2025".to_owned()),
            ParameterKind::Date => RequestParameters::Date("2026-01-15".to_owned()),
            ParameterKind::GameId => RequestParameters::GameId(21_500),
            ParameterKind::DateMinutes => RequestParameters::DateMinutes {
                date: "2026-01-15".to_owned(),
                minutes: 5,
            },
        };
        let url = request_url(operation, &parameters).expect("compiled template is legal");
        assert!(url.starts_with(API_ROOT), "{url}");
        assert!(!url.contains('?') && !url.contains('#'), "{url}");
        assert!(!url.contains("key="), "{url}");
        assert!(!url.contains(SPORTSDATAIO_SENTINEL), "{url}");
    }

    // 4. The credential rides exactly one header, that header is the documented
    //    non-secret literal, and nothing else about the request renders the value.
    assert_eq!(SUBSCRIPTION_KEY_HEADER, "Ocp-Apim-Subscription-Key");
    let client = reqwest::Client::builder()
        .build()
        .expect("client construction");
    let request = credential
        .authorize(client.get(format!("{API_ROOT}scores/json/teams")))
        .expect("the sentinel is a legal header value")
        .build()
        .expect("request builds");
    assert_eq!(request.url().query(), None);
    assert!(!request.url().as_str().contains("key="));
    let carrier = request
        .headers()
        .get(SUBSCRIPTION_KEY_HEADER)
        .expect("subscription header is attached");
    assert!(carrier.is_sensitive());
    for (name, value) in request.headers() {
        if value
            .to_str()
            .is_ok_and(|value| value == SPORTSDATAIO_SENTINEL)
        {
            assert_eq!(name.as_str(), "ocp-apim-subscription-key");
        }
    }
    assert!(!format!("{request:?}").contains(SPORTSDATAIO_SENTINEL));

    // 5. A synthetic journal produced through the adapter: the raw records, the
    //    replayed events and the replay report all carry no credential.
    let url = request_url(Operation::Teams, &RequestParameters::None).expect("legal");
    let records = vec![
        connection(CaptureStream::SportsDataIoNba, 1),
        text(
            CaptureStream::SportsDataIoNba,
            2,
            RecordKind::OutboundText,
            &outbound_descriptor(&url, Operation::Teams, 1, 0),
        ),
        text(
            CaptureStream::SportsDataIoNba,
            3,
            RecordKind::InboundText,
            &response_envelope(
                Operation::Teams,
                1,
                0,
                "2026-09-01T01:00:00.000Z",
                200,
                ResponseOutcome::Response,
                BTreeMap::from([("content-type".to_owned(), "application/json".to_owned())]),
                br#"[{"TeamID":1,"Key":"DET"}]"#,
                false,
                false,
            ),
        ),
    ];
    let mut journal = Vec::new();
    for record in &records {
        serde_json::to_writer(&mut journal, record).expect("test record serializes");
        journal.push(b'\n');
    }
    assert!(!String::from_utf8_lossy(&journal).contains(SPORTSDATAIO_SENTINEL));
    assert!(!String::from_utf8_lossy(&journal).contains("key="));
    let (report, output) = replay_records(&records, None);
    assert_eq!(report.events, 1);
    assert!(!String::from_utf8_lossy(&output).contains(SPORTSDATAIO_SENTINEL));
    assert!(!format!("{report:?}").contains(SPORTSDATAIO_SENTINEL));
    assert!(
        !serde_json::to_string(&report)
            .expect("report serializes")
            .contains(SPORTSDATAIO_SENTINEL)
    );

    // 6. Refusal strings on this path never echo a credential.
    let refusal = RequestMatrix::from_control_json(r#"{"operations":["nfl_teams"]}"#)
        .expect_err("an unknown operation is refused")
        .to_string();
    assert!(!refusal.contains(SPORTSDATAIO_SENTINEL));
    let missing = SportsDataIoAuthError::MissingEnvironment(ENVIRONMENT_VARIABLE).to_string();
    assert_eq!(ENVIRONMENT_VARIABLE, "SPORTSDATAIO_NBA_API_KEY");
    assert!(missing.contains("SPORTSDATAIO_NBA_API_KEY"));
    assert!(!missing.contains(SPORTSDATAIO_SENTINEL));

    // 7. The real `from_env` path, end to end, in a child process carrying the
    //    sentinel. Both invocations refuse BEFORE any socket is opened -- `run`
    //    at the live-runner gate, `capture` at the unreadable control file, which
    //    `run_capture` reads before it creates the output journal -- so no request
    //    to the provider is ever issued. The gate's exit criterion is that the
    //    injected sentinel appears in neither stdout, stderr nor an output file.
    let unique = format!("{}-{}", std::process::id(), line!());
    let output_path = std::env::temp_dir().join(format!("sportsdataio-sentinel-{unique}.jsonl"));
    let absent_control = std::env::temp_dir().join(format!("sportsdataio-absent-{unique}.json"));
    assert!(!output_path.exists());
    assert!(!absent_control.exists());

    for (label, arguments, expected) in [
        (
            "live runner refusal",
            vec![
                "run".to_owned(),
                "--stream".to_owned(),
                "sportsdataio-nba".to_owned(),
                "--output".to_owned(),
                output_path.display().to_string(),
                "--publisher-epoch".to_owned(),
                "sentinel-probe".to_owned(),
                "--duration-seconds".to_owned(),
                "1".to_owned(),
            ],
            "has no live-runner path yet",
        ),
        (
            "unreadable control file",
            vec![
                "capture".to_owned(),
                "--stream".to_owned(),
                "sportsdataio-nba".to_owned(),
                "--output".to_owned(),
                output_path.display().to_string(),
                "--control-file".to_owned(),
                absent_control.display().to_string(),
                "--duration-seconds".to_owned(),
                "1".to_owned(),
                "--max-records".to_owned(),
                "1".to_owned(),
                "--max-bytes".to_owned(),
                "4096".to_owned(),
            ],
            "control file could not be read",
        ),
    ] {
        let finished = std::process::Command::new(env!("CARGO_BIN_EXE_btc-feeds"))
            .args(&arguments)
            .env(ENVIRONMENT_VARIABLE, SPORTSDATAIO_SENTINEL)
            .output()
            .expect("the capture binary runs");
        let stdout = String::from_utf8_lossy(&finished.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&finished.stderr).into_owned();
        assert!(!finished.status.success(), "{label}: {stderr}");
        assert!(stderr.contains(expected), "{label}: {stderr}");
        assert!(!stdout.contains(SPORTSDATAIO_SENTINEL), "{label}");
        assert!(!stderr.contains(SPORTSDATAIO_SENTINEL), "{label}");
        assert!(
            !stdout.contains("key=") && !stderr.contains("key="),
            "{label}"
        );
        // A refusal leaves no journal behind for a scanner to have to cover.
        assert!(!output_path.exists(), "{label}");
    }

    // 8. THE PROVIDER'S OWN BODY is the remaining way the key could reach the
    //    journal: an authentication failure or an upstream gateway page is
    //    entitled to echo the request headers it received, and the capture
    //    journals bodies verbatim. The repository secret scanner cannot cover
    //    this after the fact -- its high-confidence marker needs the header NAME
    //    beside the value, and a journal line never carries the header name -- so
    //    the recorded bytes are redacted BEFORE the envelope is built, exactly as
    //    `sportsdataio_nba_request` composes it.
    let echoed = format!(
        "{{\"Message\":\"Authentication failed\",\"Request\":\
         {{\"Ocp-Apim-Subscription-Key\":\"{SPORTSDATAIO_SENTINEL}\",\
         \"Retry\":\"{SPORTSDATAIO_SENTINEL}\"}}}}"
    );
    let (recorded, was_redacted) = credential.redact_body(echoed.as_bytes(), false);
    assert!(was_redacted, "the echoed credential must be recognized");
    let leaky_records = vec![
        connection(CaptureStream::SportsDataIoNba, 1),
        text(
            CaptureStream::SportsDataIoNba,
            2,
            RecordKind::OutboundText,
            &outbound_descriptor(&url, Operation::Teams, 1, 0),
        ),
        text(
            CaptureStream::SportsDataIoNba,
            3,
            RecordKind::InboundText,
            &response_envelope(
                Operation::Teams,
                1,
                0,
                "2026-09-01T01:00:01.000Z",
                401,
                ResponseOutcome::Response,
                BTreeMap::new(),
                &recorded,
                false,
                was_redacted,
            ),
        ),
    ];
    let mut leaky_journal = Vec::new();
    for record in &leaky_records {
        serde_json::to_writer(&mut leaky_journal, record).expect("test record serializes");
        leaky_journal.push(b'\n');
    }
    // The gate's exit criterion, over journal BYTES.
    let journal_text = String::from_utf8_lossy(&leaky_journal).into_owned();
    assert!(!journal_text.contains(SPORTSDATAIO_SENTINEL));
    assert!(journal_text.contains("<redacted>"));
    // The provider's message survives; only the credential does not.
    assert!(journal_text.contains("Authentication failed"));
    let (leaky_report, leaky_output) = replay_records(&leaky_records, None);
    assert_eq!(leaky_report.events, 1);
    assert!(!String::from_utf8_lossy(&leaky_output).contains(SPORTSDATAIO_SENTINEL));
    assert!(!format!("{leaky_report:?}").contains(SPORTSDATAIO_SENTINEL));
    let evidence: serde_json::Value = serde_json::from_slice(
        leaky_output
            .split(|byte| *byte == b'\n')
            .find(|line| !line.is_empty())
            .expect("one evidence row"),
    )
    .expect("event line is JSON");
    assert_eq!(evidence["values"]["body_redacted"], true);
    assert_eq!(evidence["values"]["status"], 401);
    // The digest describes the RECORDED bytes, not the wire bytes the journal
    // deliberately does not keep.
    assert_eq!(
        evidence["values"]["body_sha256"],
        format!("{:x}", Sha256::digest(&recorded))
    );
    assert_ne!(
        evidence["values"]["body_sha256"],
        format!("{:x}", Sha256::digest(echoed.as_bytes()))
    );
}

/// The control file is operator input, and the gate's named threat is an operator
/// PASTING the credential into it. Every refusal the control-file load can raise --
/// ours and serde's -- must therefore pass through the same redactor the capture
/// loop uses, and no refusal may echo a value the redactor could not recognize.
///
/// Each case runs the real binary with the sentinel in the environment, so this
/// exercises `from_env`, the load path and stderr exactly as an operator would hit
/// them. Every case refuses before any socket is opened.
#[test]
fn sportsdataio_control_file_never_echoes_an_injected_credential() {
    use btc_feeds::sportsdataio_auth::ENVIRONMENT_VARIABLE;

    let unique = format!("{}-{}", std::process::id(), line!());
    let directory = std::env::temp_dir();
    // A pasted value longer than the refusal echo bound. A TRUNCATED echo would
    // print a prefix of the credential that the exact-match redactor cannot see,
    // so an over-long value must be reported by length instead of rendered.
    let padded = format!("2025{}{SPORTSDATAIO_SENTINEL}", "PAD".repeat(16));
    assert!(padded.len() > 64, "the padded season must exceed the bound");

    for (index, (label, control, marker)) in [
        (
            "operation names the key",
            format!(r#"{{"operations":["{SPORTSDATAIO_SENTINEL}"]}}"#),
            "<redacted>",
        ),
        (
            "date field holds the key",
            format!(
                r#"{{"operations":["games_by_date_final"],"dates":["{SPORTSDATAIO_SENTINEL}"]}}"#
            ),
            "<redacted>",
        ),
        (
            "season field holds the key",
            format!(r#"{{"operations":["games"],"season":"{SPORTSDATAIO_SENTINEL}"}}"#),
            "<redacted>",
        ),
        (
            "serde type mismatch echoes the key",
            format!(
                r#"{{"operations":["box_score_final"],"game_ids":["{SPORTSDATAIO_SENTINEL}"]}}"#
            ),
            "<redacted>",
        ),
        (
            "serde unknown field echoes the key",
            format!(r#"{{"operations":["teams"],"{SPORTSDATAIO_SENTINEL}":1}}"#),
            "<redacted>",
        ),
        (
            "value past the echo bound is not rendered at all",
            format!(r#"{{"operations":["games"],"season":"{padded}"}}"#),
            "bytes, not echoed",
        ),
        (
            // A multibyte character with no boundary at byte four: this refuses,
            // it does not panic (a panic exits 101 and bypasses redaction).
            "non-ascii season refuses instead of panicking",
            r#"{"operations":["games"],"season":"202é"}"#.to_owned(),
            "is not one of the observed forms",
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let control_path = directory.join(format!("sportsdataio-control-{unique}-{index}.json"));
        let output_path = directory.join(format!("sportsdataio-control-{unique}-{index}.jsonl"));
        assert!(!output_path.exists(), "{label}");
        std::fs::write(&control_path, control.as_bytes()).expect("the control file is written");

        let finished = std::process::Command::new(env!("CARGO_BIN_EXE_btc-feeds"))
            .args([
                "capture",
                "--stream",
                "sportsdataio-nba",
                "--output",
                &output_path.display().to_string(),
                "--control-file",
                &control_path.display().to_string(),
                "--duration-seconds",
                "1",
                "--max-records",
                "1",
                "--max-bytes",
                "4096",
            ])
            .env(ENVIRONMENT_VARIABLE, SPORTSDATAIO_SENTINEL)
            .output()
            .expect("the capture binary runs");
        let stdout = String::from_utf8_lossy(&finished.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&finished.stderr).into_owned();
        std::fs::remove_file(&control_path).expect("the control file is removed");

        // Exit 1 is the refusal; exit 101 would be a panic, which redacts nothing.
        assert_eq!(finished.status.code(), Some(1), "{label}: {stderr}");
        assert!(stderr.contains(marker), "{label}: {stderr}");
        assert!(!stdout.contains(SPORTSDATAIO_SENTINEL), "{label}");
        assert!(!stderr.contains(SPORTSDATAIO_SENTINEL), "{label}");
        // Not even a prefix of the pasted value survives the over-bound case.
        assert!(!stderr.contains(&padded[..64]), "{label}");
        assert!(
            !stdout.contains("key=") && !stderr.contains("key="),
            "{label}"
        );
        assert!(!output_path.exists(), "{label}");
    }
}

/// The secret scanner is only a guarantee if its markers actually fire. This runs
/// the real script over probe files that never enter the repository: an uppercase
/// key-shaped value beside the header name is the shape a scanner that anchored
/// case-sensitively would have missed, and so is an upper-case `?Key=` on the
/// provider host. The environment-variable probes cover the other way the key
/// reaches a file -- a pasted `SPORTSDATAIO_NBA_API_KEY=` line or its exported
/// form -- while the bare variable name, which the auth module and ADR 0021 both
/// carry legitimately, must stay unflagged. The 32-hex literals are assembled
/// from fragments so this source file cannot itself become a marker, and every
/// probe is written outside the repository.
#[test]
fn secret_scanner_markers_fire_on_key_shaped_probes() {
    use btc_feeds::sportsdataio_auth::{
        ENVIRONMENT_VARIABLE as SPORTSDATAIO_KEY_ENVIRONMENT, SUBSCRIPTION_KEY_HEADER,
    };

    let script = repository_root().join("scripts/check-secrets.sh");
    let unique = format!("{}-{}", std::process::id(), line!());
    let directory = std::env::temp_dir();
    let lower = concat!("0f1e2d3c", "4b5a6978", "8796a5b4", "c3d2e1f0");
    let upper = concat!("0F1E2D3C", "4B5A6978", "8796A5B4", "C3D2E1F0");

    for (index, (label, contents, expected)) in [
        (
            "lowercase key beside the header name",
            format!("{SUBSCRIPTION_KEY_HEADER}: {lower}\n"),
            1,
        ),
        (
            "uppercase key beside the header name",
            format!("{SUBSCRIPTION_KEY_HEADER}: {upper}\n"),
            1,
        ),
        (
            // Assembled from fragments: written literally, the probe URL would
            // make THIS source file a marker for the scanner it is testing.
            "query-parameter authentication on the provider host",
            format!(
                "https://api.sportsdata.io/v3/nba/scores/json/Teams?{}=x\n",
                "key"
            ),
            1,
        ),
        (
            // The provider's query mode is not case-sensitive, so neither is the
            // marker; a scanner anchored to lowercase `key=` would miss this.
            "upper-case query-parameter authentication on the provider host",
            format!(
                "https://api.sportsdata.io/v3/nba/scores/json/Teams?{}=x\n",
                "Key"
            ),
            1,
        ),
        (
            "a key-shaped value assigned to the environment variable",
            format!("{SPORTSDATAIO_KEY_ENVIRONMENT}={lower}\n"),
            1,
        ),
        (
            "a key-shaped value exported into the environment variable",
            format!("export {SPORTSDATAIO_KEY_ENVIRONMENT}=\"{upper}\"\n"),
            1,
        ),
        (
            "the header name alone is not a marker",
            format!("{SUBSCRIPTION_KEY_HEADER}\n"),
            0,
        ),
        (
            "the environment-variable name alone is not a marker",
            format!("{SPORTSDATAIO_KEY_ENVIRONMENT} is loaded before the run\n"),
            0,
        ),
        (
            "a non-key-shaped sentinel in the environment variable is not a marker",
            format!("{SPORTSDATAIO_KEY_ENVIRONMENT}={SPORTSDATAIO_SENTINEL}\n"),
            0,
        ),
        (
            "a non-key-shaped sentinel is not a marker",
            format!("{SUBSCRIPTION_KEY_HEADER}: {SPORTSDATAIO_SENTINEL}\n"),
            0,
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let probe = directory.join(format!("sportsdataio-scan-{unique}-{index}.txt"));
        std::fs::write(&probe, contents.as_bytes()).expect("the probe file is written");
        let finished = std::process::Command::new("sh")
            .arg(&script)
            .arg(&probe)
            .output()
            .expect("the secret scanner runs");
        std::fs::remove_file(&probe).expect("the probe file is removed");
        assert_eq!(finished.status.code(), Some(expected), "{label}");
    }
}

#[test]
fn sportsdataio_control_file_and_allowlist_refusals() {
    use btc_feeds::sportsdataio_nba::{
        API_ROOT, Operation, RequestMatrix, RequestParameters, RequestQuota, ResponseBudget,
        request_url,
    };

    let mut refusals: Vec<(&str, String)> = Vec::new();

    // The control file is the only operator input on this path. It names
    // operations and parameters; it can never name a host, a path or a query.
    for (label, control) in [
        (
            "unpadded date",
            r#"{"dates":["2026-1-5"],"operations":["games_by_date_final"]}"#,
        ),
        (
            "impossible date",
            r#"{"dates":["2026-02-30"],"operations":["games_by_date_final"]}"#,
        ),
        (
            "non-date text",
            r#"{"dates":["yesterday"],"operations":["games_by_date_final"]}"#,
        ),
        ("unknown operation", r#"{"operations":["nfl_teams"]}"#),
        (
            "host substitution",
            r#"{"dates":["https://evil.example/v3/nba"],"operations":["games_by_date_final"]}"#,
        ),
        (
            "query-string injection",
            r#"{"dates":["2026-01-15?key=deadbeef"],"operations":["games_by_date_final"]}"#,
        ),
        (
            "smuggled control field",
            r#"{"operations":["teams"],"api_key":"deadbeef"}"#,
        ),
        (
            "zero request budget",
            r#"{"operations":["teams"],"max_requests":0}"#,
        ),
    ] {
        let error = RequestMatrix::from_control_json(control)
            .err()
            .unwrap_or_else(|| panic!("control file {label} must be refused"));
        refusals.push((label, error.to_string()));
    }

    // The URL builder refuses the same smuggling structurally, one layer down.
    for (label, parameters) in [
        (
            "bare host segment",
            RequestParameters::Date("evil.example".to_owned()),
        ),
        (
            "absolute url segment",
            RequestParameters::Date("https://evil.example/v3".to_owned()),
        ),
        (
            "query-string segment",
            RequestParameters::Date("2026-01-15?key=deadbeef".to_owned()),
        ),
        (
            "path traversal segment",
            RequestParameters::Date("../../nfl".to_owned()),
        ),
    ] {
        let error = request_url(Operation::GamesByDateFinal, &parameters)
            .err()
            .unwrap_or_else(|| panic!("segment {label} must be refused"));
        refusals.push((label, error.to_string()));
    }
    refusals.push((
        "parameter kind mismatch",
        request_url(Operation::Teams, &RequestParameters::GameId(21_500))
            .expect_err("a parameter of the wrong kind is refused")
            .to_string(),
    ));

    // Budgets refuse rather than overrun. The per-response byte cap itself
    // (`MAX_RESPONSE_BYTES`) is `pub(crate)` and enforced inside the capture
    // loop, so the publicly reachable contract here is the run budget the
    // same reservation charges against; an over-cap reservation charges nothing.
    let mut quota = RequestQuota::new(1);
    quota
        .try_consume()
        .expect("the first request is affordable");
    refusals.push((
        "spent request budget",
        quota
            .try_consume()
            .expect_err("the budget is spent")
            .to_string(),
    ));
    let mut budget = ResponseBudget::new(1_024);
    refusals.push((
        "response over the run byte budget",
        budget
            .try_reserve(1_025)
            .expect_err("an over-budget response is refused")
            .to_string(),
    ));
    assert_eq!(budget.used(), 0);

    // No refusal message may carry a request URL, and none may carry a
    // credential: an error string is exactly where a `?key=` URL would surface.
    assert_eq!(refusals.len(), 15);
    for (label, message) in &refusals {
        assert!(!message.contains(API_ROOT), "{label}: {message}");
        assert!(!message.contains("api.sportsdata.io"), "{label}: {message}");
        assert!(
            !message.contains(SPORTSDATAIO_SENTINEL),
            "{label}: {message}"
        );
        assert!(!message.is_empty(), "{label}");
    }
}

fn replay_records(
    records: &[RawRecord],
    kraken_depth_override: Option<u16>,
) -> (ReplayReport, Vec<u8>) {
    let mut input = Vec::new();
    for record in records {
        serde_json::to_writer(&mut input, record).expect("test record serializes");
        input.push(b'\n');
    }
    let mut output = Vec::new();
    let report = replay(
        &mut Cursor::new(input),
        &mut output,
        "integration-test",
        kraken_depth_override,
    )
    .expect("replay succeeds")
    .expect("capture is nonempty");
    (report, output)
}

#[tokio::test]
async fn chainlink_capture_with_a_fault_replays_byte_for_byte() {
    // m5: a real capture -> replay of a run containing a failover fault, written
    // with the recorder and byte-compared. Round A; a Disconnected/Connected
    // failover (epoch 2); the same round A again (silent duplicate because the
    // round cursor is retained across the reconnect, B4); a lagging round A-1
    // (OutOfOrder); then a newer round B (event).
    let a: u128 = (7u128 << 64) | 1000;
    let secs: u128 = 1_756_400_000;
    let round = |rid: u128, s: u128| {
        format!(
            r#"{{"jsonrpc":"2.0","id":1,"result":"0x{rid:064x}{:064x}{s:064x}{s:064x}{rid:064x}"}}"#,
            7_766_779_700_000u128
        )
    };
    let epoch2 = |record: RawRecord| RawRecord {
        connection_epoch: 2,
        ..record
    };
    let req = btc_feeds::chainlink::request_body;
    let records = vec![
        connection(CaptureStream::ChainlinkAggregator, 1),
        text(
            CaptureStream::ChainlinkAggregator,
            2,
            RecordKind::OutboundText,
            &req(1),
        ),
        text(
            CaptureStream::ChainlinkAggregator,
            3,
            RecordKind::InboundText,
            &round(a, secs),
        ),
        disconnection(CaptureStream::ChainlinkAggregator, 4),
        epoch2(connection(CaptureStream::ChainlinkAggregator, 5)),
        epoch2(text(
            CaptureStream::ChainlinkAggregator,
            6,
            RecordKind::OutboundText,
            &req(2),
        )),
        epoch2(text(
            CaptureStream::ChainlinkAggregator,
            7,
            RecordKind::InboundText,
            &round(a, secs),
        )),
        epoch2(text(
            CaptureStream::ChainlinkAggregator,
            8,
            RecordKind::OutboundText,
            &req(3),
        )),
        epoch2(text(
            CaptureStream::ChainlinkAggregator,
            9,
            RecordKind::InboundText,
            &round(a - 1, secs - 60),
        )),
        epoch2(text(
            CaptureStream::ChainlinkAggregator,
            10,
            RecordKind::OutboundText,
            &req(4),
        )),
        epoch2(text(
            CaptureStream::ChainlinkAggregator,
            11,
            RecordKind::InboundText,
            &round(a + 1, secs + 60),
        )),
    ];

    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let path = std::env::temp_dir().join(format!("chainlink-fault-{unique}.jsonl"));
    let mut recorder = btc_feeds::raw::RawRecorder::create(&path, 1 << 20)
        .await
        .expect("capture file created");
    for record in &records {
        recorder.append(record).await.expect("record appended");
    }
    recorder.finish().await.expect("capture flushed");
    let captured = std::fs::read(&path).expect("capture readable");

    let replay_once = || {
        let mut output = Vec::new();
        let report = replay(
            &mut Cursor::new(captured.clone()),
            &mut output,
            "m5-fault",
            None,
        )
        .expect("replay succeeds")
        .expect("capture is nonempty");
        (report, output)
    };
    let (report, first) = replay_once();
    let (_second_report, second) = replay_once();
    std::fs::remove_file(&path).ok();

    assert_eq!(first, second, "the fault capture replays byte-for-byte");
    // Round A and round B only: the post-failover duplicate and the lagging
    // round emit nothing, so the retained cursor prevents a backwards event.
    assert_eq!(report.events, 2);
    let definitions = BTreeMap::from([definition("chainlink.ethereum.btc-usd.rounds", "1.0.0")]);
    validate_events(&first, &definitions);
}

fn kraken_book_subscription(symbol: &str, depth: u16) -> String {
    serde_json::json!({
        "method": "subscribe",
        "params": {
            "channel": "book",
            "symbol": [symbol],
            "depth": depth,
            "snapshot": true
        },
        "req_id": 1
    })
    .to_string()
}

fn kraken_level_three_subscription(depth: u16) -> String {
    serde_json::json!({
        "method": "subscribe",
        "params": {
            "channel": "level3",
            "symbol": ["BTC/USD"],
            "depth": depth,
            "snapshot": true,
            "token": "[REDACTED]"
        },
        "req_id": 1
    })
    .to_string()
}

fn validate_events(output: &[u8], definitions: &BTreeMap<(String, String), SourceDefinition>) {
    for line in output
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let event: SourceEvent = serde_json::from_slice(line).expect("event deserializes");
        let definition = definitions
            .get(&(event.source_id.clone(), event.source_version.clone()))
            .expect("event has a selected source definition");
        event
            .validate_against(definition)
            .expect("event conforms to its source definition");
    }
}

fn definition(source_id: &str, version: &str) -> ((String, String), SourceDefinition) {
    let path = repository_root()
        .join("definitions/btc/sources")
        .join(format!("{source_id}@{version}.json"));
    let bytes = std::fs::read(&path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
    let definition = serde_json::from_slice(&bytes)
        .unwrap_or_else(|error| panic!("failed to parse {}: {error}", path.display()));
    ((source_id.to_owned(), version.to_owned()), definition)
}

fn repository_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn connection(stream: CaptureStream, ordinal: u64) -> RawRecord {
    RawRecord {
        schema_version: 1,
        provider: stream.provider(),
        stream,
        connection_epoch: 1,
        ordinal,
        received_at: "2026-08-12T12:00:00Z".to_owned(),
        capture_elapsed_ns: None,
        connect_started_elapsed_ns: None,
        kind: RecordKind::Connected,
        payload: None,
    }
}

fn disconnection(stream: CaptureStream, ordinal: u64) -> RawRecord {
    RawRecord {
        schema_version: 1,
        provider: stream.provider(),
        stream,
        connection_epoch: 1,
        ordinal,
        received_at: "2026-08-12T12:00:04Z".to_owned(),
        capture_elapsed_ns: None,
        connect_started_elapsed_ns: None,
        kind: RecordKind::Disconnected,
        payload: None,
    }
}

fn configuration(stream: CaptureStream, ordinal: u64, payload: &str) -> RawRecord {
    RawRecord {
        schema_version: 3,
        provider: stream.provider(),
        stream,
        connection_epoch: 1,
        ordinal,
        received_at: "2026-08-14T16:55:02Z".to_owned(),
        capture_elapsed_ns: Some(0),
        connect_started_elapsed_ns: None,
        kind: RecordKind::Configuration,
        payload: Some(payload.to_owned()),
    }
}

fn text(stream: CaptureStream, ordinal: u64, kind: RecordKind, payload: &str) -> RawRecord {
    RawRecord {
        schema_version: 1,
        provider: stream.provider(),
        stream,
        connection_epoch: 1,
        ordinal,
        received_at: "2026-08-12T12:00:03Z".to_owned(),
        capture_elapsed_ns: None,
        connect_started_elapsed_ns: None,
        kind,
        payload: Some(payload.to_owned()),
    }
}

const KRAKEN_CHECKSUM_SNAPSHOT: &str = r#"{
  "channel":"book",
  "type":"snapshot",
  "data":[{
    "symbol":"BTC/USD",
    "bids":[
      {"price":45283.5,"qty":0.10000000},
      {"price":45283.4,"qty":1.54582015},
      {"price":45282.1,"qty":0.10000000},
      {"price":45281.0,"qty":0.10000000},
      {"price":45280.3,"qty":1.54592586},
      {"price":45279.0,"qty":0.07990000},
      {"price":45277.6,"qty":0.03310103},
      {"price":45277.5,"qty":0.30000000},
      {"price":45277.3,"qty":1.54602737},
      {"price":45276.6,"qty":0.15445238}
    ],
    "asks":[
      {"price":45285.2,"qty":0.00100000},
      {"price":45286.4,"qty":1.54571953},
      {"price":45286.6,"qty":1.54571109},
      {"price":45289.6,"qty":1.54560911},
      {"price":45290.2,"qty":0.15890660},
      {"price":45291.8,"qty":1.54553491},
      {"price":45294.7,"qty":0.04454749},
      {"price":45296.1,"qty":0.35380000},
      {"price":45297.5,"qty":0.09945542},
      {"price":45299.5,"qty":0.18772827}
    ],
    "checksum":3310070434,
    "timestamp":"2026-08-12T12:00:00Z"
  }]
}"#;

// --- Kalshi index socket (group A1) contract tests.
//
// SYNTHETIC FIXTURES REPRODUCING OBSERVED SHAPES (ruling E23). The envelope,
// the nested `subscribed`/`error` bodies, the string-encoded `data` and the
// `avg_60s_data` window opening at size 0 are what the R1 probe recorded on
// 2026-09-03; every price and timestamp below is invented. `cfbenchmarks_value`
// is observed; the `pyth_value` body and the conditional quarter object are
// still documented-only, and the Pyth relay publishes nothing until a frame is
// captured.

const KALSHI_INDEX_VALUE_ACK: &str =
    r#"{"type":"subscribed","id":101,"msg":{"channel":"cfbenchmarks_value","sid":1}}"#;
const KALSHI_INDEX_PYTH_ACK: &str =
    r#"{"type":"subscribed","id":102,"msg":{"channel":"pyth_value","sid":3}}"#;
const KALSHI_INDEX_5HZ_ACK: &str =
    r#"{"type":"subscribed","id":103,"msg":{"channel":"cfbenchmarks_value_5hz","sid":2}}"#;
// The observed 5 Hz shape (ruling E25): no `avg_60s_data`, an 8-scale
// `value_usd`, a free-scale value inside the embedded `data` string.
const KALSHI_INDEX_5HZ_TICK: &str = r#"{"type":"cfbenchmarks_value_5hz","sid":2,"seq":21,"msg":{"index_id":"BRTI","value_usd":"64250.55000000","source_ts_ms":1788000300200,"received_at":1788000300260,"data":"{\"type\":\"value\",\"time\":1788000300200,\"id\":\"BRTI\",\"value\":\"64250.55\"}"}}"#;
const KALSHI_INDEX_VALUE_TICK: &str = r#"{"type":"cfbenchmarks_value","sid":1,"seq":11,"msg":{"index_id":"BRTI","received_at":1788000300123,"data":"{\"type\":\"value\",\"time\":1788000300000,\"id\":\"BRTI\",\"value\":\"64250.11\"}","avg_60s_data":{"value":"64249.10000000","window_size":60,"window_start_ts_ms":1788000240000,"window_end_ts_exclusive":1788000300000},"last_60s_windowed_average_15min":{"value":"64248.50000000","window_size":60,"window_start_ts_ms":1788000240000,"window_end_ts_exclusive":1788000300000}}}"#;
const KALSHI_INDEX_PYTH_TICK: &str = r#"{"type":"pyth_value","sid":3,"seq":31,"msg":{"underlying_ticker":"Crypto.BTC/USD","value_usd":"64251.44000000","source_ts_ms":1788000300050,"received_at":1788000300090}}"#;

/// A 1 Hz tick at `index_time` with the given `seq`, and no quarter object.
/// `window_size` 0 is the fresh-subscription case the probe observed.
fn kalshi_index_value_tick(seq: u64, index_time: i64, window_size: u32) -> String {
    format!(
        r#"{{"type":"cfbenchmarks_value","sid":1,"seq":{seq},"msg":{{"index_id":"BRTI","received_at":{},"data":"{{\"type\":\"value\",\"time\":{index_time},\"id\":\"BRTI\",\"value\":\"64250.11\"}}","avg_60s_data":{{"value":"64249.10000000","window_size":{window_size},"window_start_ts_ms":{},"window_end_ts_exclusive":{index_time}}}}}}}"#,
        index_time + 123,
        index_time - 60_000,
    )
}

fn kalshi_index_records() -> Vec<RawRecord> {
    vec![
        connection(CaptureStream::KalshiIndex, 1),
        text(
            CaptureStream::KalshiIndex,
            2,
            RecordKind::OutboundText,
            r#"{"id":101,"cmd":"subscribe","params":{"channels":["cfbenchmarks_value"],"index_ids":["BRTI"]}}"#,
        ),
        text(
            CaptureStream::KalshiIndex,
            3,
            RecordKind::OutboundText,
            r#"{"id":102,"cmd":"subscribe","params":{"channels":["pyth_value"],"underlying_tickers":["Crypto.BTC/USD"]}}"#,
        ),
        text(
            CaptureStream::KalshiIndex,
            4,
            RecordKind::OutboundText,
            r#"{"id":103,"cmd":"subscribe","params":{"channels":["cfbenchmarks_value_5hz"],"index_ids":["BRTI"]}}"#,
        ),
        text(
            CaptureStream::KalshiIndex,
            5,
            RecordKind::InboundText,
            KALSHI_INDEX_VALUE_ACK,
        ),
        text(
            CaptureStream::KalshiIndex,
            6,
            RecordKind::InboundText,
            KALSHI_INDEX_5HZ_ACK,
        ),
        text(
            CaptureStream::KalshiIndex,
            7,
            RecordKind::InboundText,
            KALSHI_INDEX_PYTH_ACK,
        ),
        text(
            CaptureStream::KalshiIndex,
            8,
            RecordKind::InboundText,
            KALSHI_INDEX_VALUE_TICK,
        ),
        text(
            CaptureStream::KalshiIndex,
            9,
            RecordKind::InboundText,
            KALSHI_INDEX_5HZ_TICK,
        ),
        text(
            CaptureStream::KalshiIndex,
            10,
            RecordKind::InboundText,
            KALSHI_INDEX_PYTH_TICK,
        ),
        // Retained verbatim, normalized into nothing: no `*_list` command is
        // sent in wave A, so no list reply is modelled.
        text(
            CaptureStream::KalshiIndex,
            11,
            RecordKind::InboundText,
            r#"{"type":"cfbenchmarks_value_indexlist","sid":1,"msg":{"index_ids":["BRTI"]}}"#,
        ),
        disconnection(CaptureStream::KalshiIndex, 12),
    ]
}

/// A capture carrying the two ordering faults a value surface must survive: a
/// `seq` skip (a gap) and a repeated `seq` (a duplicate). Audit A1/F5 — the
/// happy-path fixture above never exercised either through `ReplayAdapter`.
fn kalshi_index_gap_and_duplicate_records() -> Vec<RawRecord> {
    let base_ms: i64 = 1_788_000_300_000;
    vec![
        connection(CaptureStream::KalshiIndex, 1),
        text(
            CaptureStream::KalshiIndex,
            2,
            RecordKind::InboundText,
            KALSHI_INDEX_VALUE_ACK,
        ),
        // The first frame of a fresh subscription: `seq` 1, window size 0.
        text(
            CaptureStream::KalshiIndex,
            3,
            RecordKind::InboundText,
            &kalshi_index_value_tick(1, base_ms, 0),
        ),
        text(
            CaptureStream::KalshiIndex,
            4,
            RecordKind::InboundText,
            &kalshi_index_value_tick(2, base_ms + 1_000, 1),
        ),
        // Gap: `seq` 5 skips 3 and 4. The surface reports and continues.
        text(
            CaptureStream::KalshiIndex,
            5,
            RecordKind::InboundText,
            &kalshi_index_value_tick(5, base_ms + 4_000, 4),
        ),
        // Duplicate: `seq` 5 again. Dropped, no event.
        text(
            CaptureStream::KalshiIndex,
            6,
            RecordKind::InboundText,
            &kalshi_index_value_tick(5, base_ms + 5_000, 5),
        ),
        text(
            CaptureStream::KalshiIndex,
            7,
            RecordKind::InboundText,
            &kalshi_index_value_tick(6, base_ms + 6_000, 6),
        ),
        disconnection(CaptureStream::KalshiIndex, 8),
    ]
}

/// F5: a gap-and-duplicate capture replays byte-identically through
/// `ReplayAdapter`, not only through the adapter's own unit tests.
#[test]
fn kalshi_index_replay_with_a_gap_and_a_duplicate_is_byte_identical() {
    let records = kalshi_index_gap_and_duplicate_records();
    let first = replay_records(&records, None);
    let second = replay_records(&records, None);
    assert_eq!(first, second, "the fault path replays byte for byte");
    // Four ticks admitted (seq 1, 2, 5, 6); the duplicate publishes nothing.
    assert_eq!(first.0.events, 4);
    let events: Vec<SourceEvent> = first
        .1
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).expect("index event deserializes"))
        .collect();
    assert_eq!(events.len(), 4);
    assert!(
        events
            .iter()
            .all(|event| event.source_id == "kalshi.cfbenchmarks.brti.value")
    );
    assert_eq!(
        events
            .iter()
            .map(|event| event.values["upstream_sequence"].as_u64())
            .collect::<Vec<_>>(),
        vec![Some(1), Some(2), Some(5), Some(6)],
        "the duplicate is dropped and the gap does not stop publication"
    );
    // A fresh subscription's window size 0 survives the whole replay path.
    assert_eq!(
        events[0].values["avg_60s_window_size"],
        serde_json::json!(0)
    );
    let definitions = BTreeMap::from([definition("kalshi.cfbenchmarks.brti.value", "1.0.0")]);
    validate_events(&first.1, &definitions);
    // The gap raises a repair-bearing recovery notice; the duplicate does not.
    assert!(
        first.0.recovery_notices >= 2,
        "the gap and the closing disconnect both ask for recovery"
    );
}

#[test]
fn kalshi_index_replay_is_byte_identical() {
    let records = kalshi_index_records();
    let first = replay_records(&records, None);
    let second = replay_records(&records, None);
    assert_eq!(first, second);
    // One 1 Hz tick carrying a quarter object emits two events and the 5 Hz tick
    // one. The Pyth relay is refuse-closed while its contract is unobserved, so
    // it emits none.
    assert_eq!(first.0.events, 3);
    assert_eq!(first.0.recovery_notices, 1, "only the closing disconnect");

    let definitions = BTreeMap::from([
        definition("kalshi.cfbenchmarks.brti.value", "1.0.0"),
        definition("kalshi.cfbenchmarks.brti.settlement-window", "1.0.0"),
        definition("kalshi.cfbenchmarks.brti.value-5hz", "1.0.0"),
    ]);
    validate_events(&first.1, &definitions);

    let events: Vec<SourceEvent> = first
        .1
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).expect("index event deserializes"))
        .collect();
    assert_eq!(events.len(), 3);
    assert_eq!(events[0].source_id, "kalshi.cfbenchmarks.brti.value");
    assert_eq!(
        events[1].source_id,
        "kalshi.cfbenchmarks.brti.settlement-window"
    );
    assert_eq!(events[2].source_id, "kalshi.cfbenchmarks.brti.value-5hz");
    // Decimal text survives the whole replay path byte for byte.
    assert_eq!(
        events[0].values["index_value"],
        serde_json::json!("64250.11")
    );
    assert_eq!(
        events[1].values["window_average"],
        serde_json::json!("64248.50000000")
    );
    // `window_end_inclusive` and `quarter_close_time` are one instant, DERIVED
    // from the window start (ruling E28); the wire's own end rides separately as
    // the running `window_as_of`. This fixture is a closing tick, so all three
    // coincide and the window is complete.
    assert_eq!(
        events[1].values["window_end_inclusive"],
        events[1].values["quarter_close_time"]
    );
    assert_eq!(
        events[1].values["window_as_of"], events[1].values["quarter_close_time"],
        "a closing tick completes the window"
    );
}

#[test]
fn kalshi_index_normalize_stdout_stable() {
    let records = kalshi_index_records();
    let input: Vec<u8> = records
        .iter()
        .filter(|record| record.kind == RecordKind::InboundText)
        .flat_map(|record| {
            let mut line = record.payload.clone().expect("text record has a payload");
            line.push('\n');
            line.into_bytes()
        })
        .collect();

    let mut runs = Vec::new();
    for _ in 0..2 {
        let mut events = Vec::new();
        let mut notices = Vec::new();
        let report = normalize(
            &mut Cursor::new(input.clone()),
            &mut events,
            &mut notices,
            &NormalizeOptions {
                stream: CaptureStream::KalshiIndex,
                publisher_epoch: "integration-test".to_owned(),
                kraken_depths: KrakenBookDepths::default(),
                configuration: None,
                subscriptions: Vec::new(),
            },
        )
        .expect("normalization succeeds");
        // `normalize` reads raw provider text, so it stamps `received_at` from
        // the wall clock — the one field that is deliberately not reproducible
        // between two runs of the same input. Everything else must be.
        let events: Vec<SourceEvent> = events
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| {
                let mut event: SourceEvent =
                    serde_json::from_slice(line).expect("index event deserializes");
                event.received_at = "<receipt>".to_owned();
                event
            })
            .collect();
        runs.push((report.messages, report.events, events, notices));
    }
    assert_eq!(runs[0], runs[1], "`btc-feeds normalize` output is stable");
    assert_eq!(runs[0].1, 3);

    let events = &runs[0].2;
    assert_eq!(events.len(), 3);
    assert_eq!(events[0].source_id, "kalshi.cfbenchmarks.brti.value");
    assert_eq!(
        events[1].source_id,
        "kalshi.cfbenchmarks.brti.settlement-window"
    );
}
