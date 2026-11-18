//! Consumer-style tests: everything here goes through the crate's public API
//! only, exactly as an outside ingestion service would use it.

use std::io::Cursor;

use btc_feeds::replay::{NormalizeOptions, normalize};
use btc_feeds::{
    CaptureStream, CoinbaseAdapter, CoinbaseRestTrade, FeedAdapter, FeedNoticeCode, KalshiAdapter,
    KrakenBookDepths, PolymarketAdapter, RawRecord, RecordKind, RecoveryAction, ReplayAdapter,
};

const KALSHI_TICKER: &str = "KXBTC15M-26AUG141315-15";
const KALSHI_REQUEST_PATH: &str = "/trade-api/v2/markets?tickers=KXBTC15M-26AUG141315-15";
const POLYMARKET_SLUG: &str = "btc-updown-5m-1786726500";
const POLYMARKET_REQUEST_PATH: &str = "/markets?slug=btc-updown-5m-1786726500&closed=false";
const POLYMARKET_CONDITION_ID: &str =
    "0x0f85721d0123456789abcdef0123456789abcdef0123456789abcdef01234567";
const POLYMARKET_UP_TOKEN: &str =
    "11111111111111111111111111111111111111111111111111111111111111111";
const POLYMARKET_DOWN_TOKEN: &str =
    "22222222222222222222222222222222222222222222222222222222222222222";

fn kalshi_rest_body() -> String {
    serde_json::json!({
        "markets": [{
            "ticker": KALSHI_TICKER,
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
    .to_string()
}

fn polymarket_gamma_body() -> String {
    serde_json::json!([{
        "id": "3566011",
        "question": "Bitcoin Up or Down - August 14, 12:55PM-1:00PM ET",
        "conditionId": POLYMARKET_CONDITION_ID,
        "slug": POLYMARKET_SLUG,
        "resolutionSource": "https://data.chain.link/streams/btc-usd-twap-60s-streams",
        "endDate": "2026-08-14T17:00:00Z",
        "description": "This market will resolve to \"Up\" if the time-weighted average price (TWAP) is greater than or equal to the opening value. Otherwise, it will resolve to \"Down\".",
        "outcomes": "[\"Up\", \"Down\"]",
        "active": true,
        "closed": false,
        "questionID": "0x123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0",
        "enableOrderBook": true,
        "orderPriceMinTickSize": 0.01,
        "orderMinSize": 5,
        "clobTokenIds": format!("[\"{POLYMARKET_UP_TOKEN}\", \"{POLYMARKET_DOWN_TOKEN}\"]"),
        "updatedAt": "2026-08-14T16:55:01Z",
        "events": [{
            "ticker": POLYMARKET_SLUG,
            "slug": POLYMARKET_SLUG,
            "startTime": "2026-08-14T16:55:00Z"
        }]
    }])
    .to_string()
}

fn kalshi_subscriptions() -> Vec<String> {
    [(1, "orderbook_delta"), (2, "ticker"), (3, "trade")]
        .into_iter()
        .map(|(id, channel)| {
            serde_json::json!({
                "id": id,
                "cmd": "subscribe",
                "params": {
                    "channels": [channel],
                    "market_ticker": KALSHI_TICKER,
                    "use_yes_price": true
                }
            })
            .to_string()
        })
        .collect()
}

fn kalshi_book_snapshot() -> String {
    serde_json::json!({
        "type": "orderbook_snapshot",
        "sid": 11,
        "seq": 2,
        "msg": {
            "market_ticker": KALSHI_TICKER,
            "yes_dollars_fp": [["0.4500", "10.00"]],
            "no_dollars_fp": [["0.5500", "12.00"]]
        }
    })
    .to_string()
}

fn connected(stream: CaptureStream, ordinal: u64) -> RawRecord {
    RawRecord::connected_at(stream, 1, ordinal, "2026-08-14T17:00:03Z".to_owned(), 10, 5)
}

fn inbound(stream: CaptureStream, ordinal: u64, payload: &str) -> RawRecord {
    RawRecord::text_at(
        stream,
        1,
        ordinal,
        RecordKind::InboundText,
        payload.to_owned(),
        "2026-08-14T17:00:04Z".to_owned(),
        20,
    )
}

fn outbound(stream: CaptureStream, ordinal: u64, payload: &str) -> RawRecord {
    RawRecord::text_at(
        stream,
        1,
        ordinal,
        RecordKind::OutboundText,
        payload.to_owned(),
        "2026-08-14T17:00:04Z".to_owned(),
        20,
    )
}

/// A consumer holding only a Kalshi REST body can build the Configuration
/// record, bind an adapter to the market and normalize book messages, without
/// reaching any crate-private item.
#[test]
fn kalshi_configuration_payload_binds_an_adapter_that_then_normalizes_a_book_message() {
    let payload = KalshiAdapter::configuration_payload(
        &kalshi_rest_body(),
        KALSHI_REQUEST_PATH,
        "2026-08-14T17:00:02Z",
    )
    .expect("a valid REST body produces a configuration payload");

    let mut adapter = KalshiAdapter::new("consumer-epoch").expect("adapter constructs");
    let configured = adapter
        .ingest(&RawRecord::configuration_at(
            CaptureStream::KalshiMarket,
            1,
            1,
            payload,
            "2026-08-14T17:00:02Z".to_owned(),
            0,
        ))
        .expect("the built configuration record is accepted");
    assert_eq!(configured.events.len(), 1);
    assert_eq!(
        configured.events[0].source_id,
        "kalshi.btc.market-contracts"
    );

    adapter
        .ingest(&connected(CaptureStream::KalshiMarket, 2))
        .expect("connection opens");
    for (ordinal, channel) in [(3, "orderbook_delta"), (4, "ticker"), (5, "trade")] {
        adapter
            .ingest(&outbound(
                CaptureStream::KalshiMarket,
                ordinal,
                &serde_json::json!({
                    "id": ordinal,
                    "cmd": "subscribe",
                    "params": {
                        "channels": [channel],
                        "market_ticker": KALSHI_TICKER,
                        "use_yes_price": true
                    }
                })
                .to_string(),
            ))
            .expect("subscription is accepted");
    }

    let outcome = adapter
        .ingest(&inbound(
            CaptureStream::KalshiMarket,
            6,
            &serde_json::json!({
                "type": "orderbook_snapshot",
                "sid": 11,
                "seq": 2,
                "msg": {
                    "market_ticker": KALSHI_TICKER,
                    "yes_dollars_fp": [["0.4500", "10.00"]],
                    "no_dollars_fp": [["0.5500", "12.00"]]
                }
            })
            .to_string(),
        ))
        .expect("book snapshot normalizes");
    assert!(
        outcome
            .events
            .iter()
            .any(|event| event.source_id == "kalshi.btc.book-levels"),
        "a book snapshot must produce book-levels events"
    );
    adapter.finish().expect("the session is complete");
}

/// The same, for Polymarket's Gamma journal.
#[test]
fn polymarket_configuration_payload_binds_an_adapter_that_then_normalizes_a_book_message() {
    let payload = PolymarketAdapter::configuration_payload(
        &polymarket_gamma_body(),
        POLYMARKET_REQUEST_PATH,
        "2026-08-14T16:55:02Z",
    )
    .expect("a valid Gamma body produces a configuration payload");

    let mut adapter = PolymarketAdapter::new("consumer-epoch").expect("adapter constructs");
    let configured = adapter
        .ingest(&RawRecord::configuration_at(
            CaptureStream::PolymarketMarket,
            1,
            1,
            payload,
            "2026-08-14T16:55:02Z".to_owned(),
            0,
        ))
        .expect("the built configuration record is accepted");
    assert_eq!(
        configured.events[0].source_id,
        "polymarket.btc.market-contracts"
    );

    adapter
        .ingest(&connected(CaptureStream::PolymarketMarket, 2))
        .expect("connection opens");
    adapter
        .ingest(&outbound(
            CaptureStream::PolymarketMarket,
            3,
            &serde_json::json!({
                "assets_ids": [POLYMARKET_UP_TOKEN, POLYMARKET_DOWN_TOKEN],
                "type": "market",
                "custom_feature_enabled": true
            })
            .to_string(),
        ))
        .expect("subscription is accepted");

    let outcome = adapter
        .ingest(&inbound(
            CaptureStream::PolymarketMarket,
            4,
            &serde_json::json!([{
                "event_type": "book",
                "asset_id": POLYMARKET_UP_TOKEN,
                "market": POLYMARKET_CONDITION_ID,
                "bids": [{"price": ".48", "size": "30"}],
                "asks": [{"price": ".52", "size": "25"}],
                "timestamp": 1_786_726_500_000_u64,
                "hash": "0xabc"
            }])
            .to_string(),
        ))
        .expect("book message normalizes");
    assert!(
        outcome
            .events
            .iter()
            .any(|event| event.source_id == "polymarket.btc.book-levels"),
        "a book message must produce book-levels events"
    );
    adapter.finish().expect("the session is complete");
}

/// The documented Coinbase recovery loop closes from outside the crate: gap
/// notice names a range, the consumer fetches it, hands it back through the
/// public `apply_trade_backfill`, and live trades resume.
#[test]
fn coinbase_trade_gap_recovers_through_the_public_backfill_entry_point() {
    fn trade(ordinal: u64, trade_id: u64, sequence: u64) -> RawRecord {
        inbound(
            CaptureStream::CoinbaseCore,
            ordinal,
            &serde_json::json!({
                "type": "match",
                "trade_id": trade_id,
                "sequence": sequence,
                "product_id": "BTC-USD",
                "price": "60000.50",
                "size": "0.0100",
                "side": "sell",
                "time": "2026-08-14T17:00:05Z"
            })
            .to_string(),
        )
    }

    let mut adapter = CoinbaseAdapter::new("consumer-epoch").expect("adapter constructs");
    adapter
        .ingest(&connected(CaptureStream::CoinbaseCore, 1))
        .expect("connection opens");
    let first = adapter.ingest(&trade(2, 100, 900)).expect("first trade");
    assert_eq!(first.events.len(), 1);

    // A jump to 103 leaves 101 and 102 unproven.
    let gap = adapter
        .ingest(&trade(3, 103, 901))
        .expect("gap is a notice");
    assert!(gap.events.is_empty(), "publication pauses on a gap");
    let notice = gap
        .notices
        .iter()
        .find(|notice| notice.code == FeedNoticeCode::Gap)
        .expect("the gap is reported");
    let RecoveryAction::BackfillCoinbaseTrades {
        product_id,
        after_trade_id,
        through_trade_id,
    } = notice
        .recovery
        .clone()
        .expect("a gap carries a recovery action")
    else {
        panic!("a Coinbase trade gap must ask for a Coinbase trade backfill");
    };
    assert_eq!(product_id, "BTC-USD");
    assert_eq!((after_trade_id, through_trade_id), (100, 102));

    // A wrong range is refused without disturbing the pending state.
    assert!(
        adapter
            .apply_trade_backfill(100, 101, "2026-08-14T17:00:06Z", Vec::new())
            .is_err(),
        "a range that is not the pending one is refused"
    );

    let recovered = adapter
        .apply_trade_backfill(
            after_trade_id,
            through_trade_id,
            "2026-08-14T17:00:06Z",
            vec![
                CoinbaseRestTrade {
                    trade_id: 101,
                    price: "60001.00".to_owned(),
                    size: "0.0200".to_owned(),
                    side: "buy".to_owned(),
                    time: "2026-08-14T17:00:05.100Z".to_owned(),
                },
                CoinbaseRestTrade {
                    trade_id: 102,
                    price: "60002.00".to_owned(),
                    size: "0.0300".to_owned(),
                    side: "sell".to_owned(),
                    time: "2026-08-14T17:00:05.200Z".to_owned(),
                },
            ],
        )
        .expect("the exact pending range is accepted");
    // 101 and 102 from REST, then the held 103 that arrived during the pause.
    let recovered_ids: Vec<_> = recovered
        .events
        .iter()
        .map(|event| event.values["trade_id"].clone())
        .collect();
    assert_eq!(
        recovered_ids,
        vec![
            serde_json::json!(101),
            serde_json::json!(102),
            serde_json::json!(103)
        ]
    );

    // Live trades resume.
    let resumed = adapter
        .ingest(&trade(4, 104, 902))
        .expect("live trades resume after recovery");
    assert_eq!(resumed.events.len(), 1);
    assert_eq!(resumed.events[0].values["trade_id"], serde_json::json!(104));
}

/// `ReplayAdapter` satisfies `FeedAdapter`, so a consumer can write one generic
/// driver and pass the type that dispatches every venue.
#[test]
fn replay_adapter_is_usable_through_the_feed_adapter_trait() {
    fn drive<A: FeedAdapter>(adapter: &mut A, records: &[RawRecord]) -> usize {
        records
            .iter()
            .map(|record| {
                adapter
                    .ingest(record)
                    .expect("record is accepted")
                    .events
                    .len()
            })
            .sum()
    }

    let mut adapter = ReplayAdapter::new(
        CaptureStream::CoinbaseCore,
        "consumer-epoch",
        KrakenBookDepths::default(),
    )
    .expect("adapter constructs");
    let records = vec![
        connected(CaptureStream::CoinbaseCore, 1),
        inbound(
            CaptureStream::CoinbaseCore,
            2,
            r#"{"type":"snapshot","product_id":"BTC-USD","bids":[["60000.10","1.2500"]],"asks":[["60001.20","2.5000"]]}"#,
        ),
    ];
    assert_eq!(drive(&mut adapter, &records), 2);
    adapter.finish().expect("Coinbase has no completeness rule");
}

/// The language-neutral entry point: raw provider text in, `SourceEvent` JSONL
/// out, feed notices as JSON on their own stream.
#[test]
fn normalize_turns_two_coinbase_messages_into_source_event_jsonl() {
    let input = concat!(
        r#"{"type":"snapshot","product_id":"BTC-USD","bids":[["60000.10","1.2500"]],"asks":[["60001.20","2.5000"]]}"#,
        "\n",
        r#"{"type":"match","trade_id":100,"sequence":900,"product_id":"BTC-USD","price":"60000.50","size":"0.0100","side":"sell","time":"2026-08-14T17:00:05Z"}"#,
        "\n",
    );
    let mut events = Vec::new();
    let mut notices = Vec::new();
    let report = normalize(
        &mut Cursor::new(input),
        &mut events,
        &mut notices,
        &NormalizeOptions {
            stream: CaptureStream::CoinbaseCore,
            publisher_epoch: "cli-epoch".to_owned(),
            kraken_depths: KrakenBookDepths::default(),
            configuration: None,
            subscriptions: Vec::new(),
        },
    )
    .expect("normalization succeeds");

    assert_eq!(report.messages, 2);
    assert_eq!(report.events, 3);
    let events: Vec<serde_json::Value> = String::from_utf8(events)
        .expect("output is UTF-8")
        .lines()
        .map(|line| serde_json::from_str(line).expect("each line is one JSON object"))
        .collect();
    assert_eq!(events.len(), 3);
    assert_eq!(
        events[0]["source_id"],
        "coinbase.exchange.btc-usd.book-levels"
    );
    assert_eq!(events[0]["publisher_epoch"], "cli-epoch");
    assert_eq!(events[0]["sequence"], 0);
    assert_eq!(events[1]["sequence"], 1);
    assert_eq!(events[2]["source_id"], "coinbase.exchange.btc-usd.trades");
    assert_eq!(events[2]["values"]["trade_id"], 100);

    // The synthesized Connected record produces exactly one notice, serialized
    // in the documented shape.
    let notices: Vec<serde_json::Value> = String::from_utf8(notices)
        .expect("notices are UTF-8")
        .lines()
        .map(|line| serde_json::from_str(line).expect("each line is one JSON object"))
        .collect();
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0]["code"], "connected");
    assert!(notices[0]["detail"].is_string());
    assert!(notices[0]["recovery"].is_null());
}

/// A configured Kalshi session drives the same entry point end to end: the
/// discovery journal as the Configuration record, the recorded subscriptions as
/// outbound records, then the venue text on stdin.
#[test]
fn normalize_drives_a_configured_kalshi_session() {
    let configuration = KalshiAdapter::configuration_payload(
        &kalshi_rest_body(),
        KALSHI_REQUEST_PATH,
        "2026-08-14T17:00:02Z",
    )
    .expect("configuration payload builds");

    let input = format!("{}\n", kalshi_book_snapshot());
    let mut events = Vec::new();
    let mut notices = Vec::new();
    let report = normalize(
        &mut Cursor::new(input.as_bytes()),
        &mut events,
        &mut notices,
        &NormalizeOptions {
            stream: CaptureStream::KalshiMarket,
            publisher_epoch: "cli-epoch".to_owned(),
            kraken_depths: KrakenBookDepths::default(),
            configuration: Some(configuration),
            subscriptions: kalshi_subscriptions(),
        },
    )
    .expect("a fully described session normalizes");

    assert_eq!(report.messages, 1);
    let events: Vec<serde_json::Value> = String::from_utf8(events)
        .expect("output is UTF-8")
        .lines()
        .map(|line| serde_json::from_str(line).expect("each line is one JSON object"))
        .collect();
    assert_eq!(events[0]["source_id"], "kalshi.btc.market-contracts");
    assert_eq!(events[0]["publisher_epoch"], "cli-epoch");
    assert_eq!(events[0]["sequence"], 0);
    assert!(
        events
            .iter()
            .any(|event| event["source_id"] == "kalshi.btc.book-levels"),
        "the book snapshot must now normalize: {events:?}"
    );
}

/// Without the recorded subscriptions the same input still fails closed: the
/// contract event is emitted and the book is refused, because nothing proves the
/// channel was subscribed.
#[test]
fn normalize_without_subscriptions_emits_the_kalshi_contract_then_fails_closed() {
    let configuration = KalshiAdapter::configuration_payload(
        &kalshi_rest_body(),
        KALSHI_REQUEST_PATH,
        "2026-08-14T17:00:02Z",
    )
    .expect("configuration payload builds");

    let input = format!("{}\n", kalshi_book_snapshot());
    let mut events = Vec::new();
    let mut notices = Vec::new();
    let error = normalize(
        &mut Cursor::new(input.as_bytes()),
        &mut events,
        &mut notices,
        &NormalizeOptions {
            stream: CaptureStream::KalshiMarket,
            publisher_epoch: "cli-epoch".to_owned(),
            kraken_depths: KrakenBookDepths::default(),
            configuration: Some(configuration),
            subscriptions: Vec::new(),
        },
    )
    .expect_err("an unproven subscription must not publish book state");
    assert!(
        error
            .to_string()
            .contains("book snapshot arrived before recorded subscription"),
        "unexpected error: {error}"
    );

    let events: Vec<serde_json::Value> = String::from_utf8(events)
        .expect("output is UTF-8")
        .lines()
        .map(|line| serde_json::from_str(line).expect("each line is one JSON object"))
        .collect();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["source_id"], "kalshi.btc.market-contracts");
}

/// The second recovery action with an in-crate completion path. Its shape is
/// part of the crate's public surface: a consumer branching on `RecoveryAction`
/// must be able to name the index, the exclusive lower bound and the inclusive
/// upper bound of the interval a repair has to cover.
#[test]
fn the_kalshi_brti_backfill_action_is_public_and_names_its_interval() {
    let action = RecoveryAction::BackfillKalshiBrti {
        index_id: "BRTI".to_owned(),
        after_index_time: 1_788_000_300_000,
        through_index_time: 1_788_001_195_000,
    };
    let RecoveryAction::BackfillKalshiBrti {
        index_id,
        after_index_time,
        through_index_time,
    } = action.clone()
    else {
        panic!("the variant destructures by name");
    };
    assert_eq!(index_id, "BRTI");
    assert!(after_index_time < through_index_time);

    // It serializes under the enum's snake_case contract, alongside the other
    // actions, so a consumer reading a notice can branch on the tag.
    let encoded = serde_json::to_value(&action).expect("the action serializes");
    assert_eq!(
        encoded,
        serde_json::json!({
            "backfill_kalshi_brti": {
                "index_id": "BRTI",
                "after_index_time": 1_788_000_300_000_i64,
                "through_index_time": 1_788_001_195_000_i64
            }
        })
    );
    assert_eq!(
        serde_json::to_value(RecoveryAction::Reconnect).expect("serializes"),
        serde_json::json!("reconnect")
    );
}

/// The index socket's four adopted identities and its repair-only fifth are
/// public constants, so a consumer never spells a source id by hand.
/// `kalshi.cfbenchmarks.brti.value-5hz` is back (ruling E25): the channel is
/// intermittently available, not absent.
#[test]
fn the_kalshi_index_source_identities_are_public() {
    use btc_feeds::kalshi_index;

    assert_eq!(
        kalshi_index::VALUE_SOURCE_ID,
        "kalshi.cfbenchmarks.brti.value"
    );
    assert_eq!(
        kalshi_index::SETTLEMENT_WINDOW_SOURCE_ID,
        "kalshi.cfbenchmarks.brti.settlement-window"
    );
    assert_eq!(
        kalshi_index::VALUE_5HZ_SOURCE_ID,
        "kalshi.cfbenchmarks.brti.value-5hz"
    );
    assert_eq!(
        kalshi_index::PYTH_VALUE_SOURCE_ID,
        "kalshi.pyth.btc-usd.value"
    );
    // Repair-only: registered for its field contract, never emitted.
    assert_eq!(
        kalshi_index::HISTORY_SOURCE_ID,
        "kalshi.cfbenchmarks.brti.history"
    );
    assert_eq!(CaptureStream::KalshiIndex.as_str(), "kalshi_index");
    // The one supported endpoint, and the reserved subscribe-command band that
    // lets an `error` reply be attributed to its channel on the shared socket.
    assert_eq!(kalshi_index::INDEX_WEBSOCKET_PATH, "/trade-api/ws/v2");
    for id in [
        kalshi_index::VALUE_COMMAND_ID,
        kalshi_index::PYTH_COMMAND_ID,
    ] {
        assert!(id >= kalshi_index::INDEX_COMMAND_ID_BASE, "{id}");
    }
    assert_ne!(
        kalshi_index::VALUE_COMMAND_ID,
        kalshi_index::PYTH_COMMAND_ID
    );
}

/// The Binance USD-M surface, driven exactly as an outside ingestion service
/// would drive it: one `CaptureStream`, one `ReplayAdapter`, and nothing named
/// from inside the crate.
///
/// DOCUMENTED, UNOBSERVED (ruling R1): these payloads are synthetic. What the
/// test freezes is the public API's shape — the USD-M `public` depth stream and
/// the REST poll stream, the source ids they emit, and the per-connection
/// refusal — not a wire contract anybody has seen.
#[test]
fn binance_usdm_public_api_surface_frozen() {
    let mut adapter = ReplayAdapter::new(
        CaptureStream::BinanceUsdmPublic,
        "public-api-test",
        KrakenBookDepths::default(),
    )
    .expect("the USD-M public stream selects an adapter");
    adapter
        .ingest(&RawRecord::connected_at(
            CaptureStream::BinanceUsdmPublic,
            1,
            1,
            "2026-09-03T04:00:00Z".to_owned(),
            1_000,
            0,
        ))
        .expect("the socket opens");
    let outcome = adapter
        .ingest(&RawRecord::text_at(
            CaptureStream::BinanceUsdmPublic,
            1,
            2,
            RecordKind::InboundText,
            r#"{"stream":"btcusdt@depth20@100ms","data":{"e":"depthUpdate","E":1788440000500,"T":1788440000480,"s":"BTCUSDT","ps":"BTCUSDT","U":880096,"u":880100,"pu":0,"st":1,"b":[["66120.10","1.5"]],"a":[["66121.30","0.8"]]}}"#
                .to_owned(),
            "2026-09-03T04:00:01.500Z".to_owned(),
            2_000,
        ))
        .expect("the depth snapshot normalizes");
    let event = &outcome.events[0];
    assert_eq!(event.source_id, "binance.usdm.btcusdt.book-levels");
    assert_eq!(event.source_version, "1.0.0");
    assert_eq!(event.publisher_epoch, "public-api-test");
    assert_eq!(event.sequence, 0);
    // Decimal text survives the whole path unparsed.
    assert_eq!(event.values["price"], serde_json::json!("66120.10"));
    assert_eq!(event.values["side"], serde_json::json!("bid"));

    // Mark price belonged to the removed `market` connection (ruling E62).
    // Handed to the `public` adapter it is retained and reported, never
    // normalized under a source id this connection does not carry.
    let foreign = adapter
        .ingest(&RawRecord::text_at(
            CaptureStream::BinanceUsdmPublic,
            1,
            3,
            RecordKind::InboundText,
            r#"{"stream":"btcusdt@markPrice@1s","data":{"e":"markPriceUpdate","E":1788440000600,"s":"BTCUSDT","p":"66150.20","i":"66148.90","P":"0","r":"-0.00012500","T":1788480000000}}"#
                .to_owned(),
            "2026-09-03T04:00:01.600Z".to_owned(),
            3_000,
        ))
        .expect("a foreign stream is operational state, not an error");
    assert!(foreign.events.is_empty());
    assert_eq!(foreign.notices[0].code, FeedNoticeCode::UnsupportedMessage);

    // The REST connection is descriptor-paired: a body is only interpretable
    // against the request that preceded it.
    let mut rest = ReplayAdapter::new(
        CaptureStream::BinanceUsdmRest,
        "public-api-test",
        KrakenBookDepths::default(),
    )
    .expect("the USD-M REST stream selects an adapter");
    rest.ingest(&RawRecord::connected_at(
        CaptureStream::BinanceUsdmRest,
        1,
        1,
        "2026-09-03T04:00:00Z".to_owned(),
        1_000,
        0,
    ))
    .expect("the poll session opens");
    rest.ingest(&RawRecord::text_at(
        CaptureStream::BinanceUsdmRest,
        1,
        2,
        RecordKind::OutboundText,
        r#"{"method":"GET","poll":1,"request":"open_interest","url":"https://fapi.binance.com/fapi/v1/openInterest?symbol=BTCUSDT"}"#.to_owned(),
        "2026-09-03T04:00:02.000Z".to_owned(),
        2_000,
    ))
    .expect("the descriptor is recorded");
    let polled = rest
        .ingest(&RawRecord::text_at(
            CaptureStream::BinanceUsdmRest,
            1,
            3,
            RecordKind::InboundText,
            r#"{"openInterest":"10659.50900000","symbol":"BTCUSDT","time":1788440000000}"#
                .to_owned(),
            "2026-09-03T04:00:02.250Z".to_owned(),
            3_000,
        ))
        .expect("the response normalizes");
    assert_eq!(
        polled.events[0].source_id,
        "binance.usdm.btcusdt.open-interest"
    );
    assert_eq!(
        polled.events[0].values["poll_sequence"],
        serde_json::json!(0_u64)
    );
}
