//! Wire types for Gemini's public prediction-market data, and the pure
//! functions that turn its messages into [`WsEvent`]s and its event list
//! into [`UpWindow`]s. Nothing here touches the network.
//!
//! Three things about the wire that the docs do not make obvious, each
//! pinned by a fixture under `tests/fixtures/gemini/`:
//!
//! * Symbols arrive lowercase on the socket and uppercase over REST.
//!   Everything leaving this module is uppercase.
//! * The event time `E` is nanoseconds on depth and trade frames,
//!   milliseconds on `contractStatus`, and seconds on index prices.
//!   Everything leaving this module is milliseconds.
//! * A delta's first update id `U` equals the previous frame's last id
//!   `u`; it does not follow it by one. The frame straight after a
//!   snapshot can be older than the snapshot and must be dropped.

use std::collections::{BTreeMap, HashMap};
use std::str::FromStr;

use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::Value;

/// One rolling "Up" contract: a window with a start, an expiry and a
/// strike that Gemini publishes a few seconds after the window starts.
#[derive(Debug, Clone, PartialEq)]
pub struct UpWindow {
    /// Gemini's event ticker, e.g. `BTC05M2611181720`.
    pub event_ticker: String,
    /// The Up contract's instrument symbol, e.g. `GEMI-BTC05M2611181720-UP`.
    pub symbol: String,
    pub starts_at_ms: u64,
    pub expires_at_ms: u64,
    /// `None` until Gemini has set it.
    pub strike: Option<Decimal>,
    /// Gemini's own word: `open`, `post_only`, …
    pub market_state: String,
}

/// What the public socket hands to the rest of the server.
#[derive(Debug, Clone, PartialEq)]
pub enum WsEvent {
    /// Book levels as `(price, quantity)`. With `snapshot` set the lists
    /// are the whole book; otherwise they are changes, and a quantity of
    /// zero removes the level.
    Depth {
        symbol: String,
        bids: Vec<(Decimal, Decimal)>,
        asks: Vec<(Decimal, Decimal)>,
        snapshot: bool,
        ts_ms: u64,
    },
    Trade { symbol: String, price: Decimal, qty: Decimal, ts_ms: u64 },
    /// A reference price. `index` is Gemini's index id, e.g. `LRRR_XBT`.
    Index { index: String, price: f64, ts_ms: u64 },
    ContractStatus {
        symbol: String,
        event_ticker: String,
        strike: Option<Decimal>,
        new_status: String,
        ts_ms: u64,
    },
    Connected,
    Disconnected,
}

/// A depth frame before anyone has decided whether it is a snapshot.
#[derive(Debug, Clone, PartialEq)]
pub struct DepthFrame {
    pub symbol: String,
    pub first_id: u64,
    pub last_id: u64,
    pub bids: Vec<(Decimal, Decimal)>,
    pub asks: Vec<(Decimal, Decimal)>,
    pub ts_ms: u64,
}

/// One parsed socket message.
#[derive(Debug, Clone, PartialEq)]
pub enum Parsed {
    /// Complete as it stands.
    Event(WsEvent),
    /// Needs the per-symbol sequence check in [`DepthTracker`].
    Depth(DepthFrame),
}

/// Gemini's event time, whatever unit it came in, as milliseconds.
pub fn event_time_ms(e: u64) -> u64 {
    // Seconds are ten digits, milliseconds thirteen, nanoseconds nineteen
    // for any date this century; the gaps between them are wide.
    if e >= 100_000_000_000_000_000 {
        e / 1_000_000
    } else if e >= 100_000_000_000_000 {
        e / 1_000
    } else if e >= 100_000_000_000 {
        e
    } else {
        e.saturating_mul(1_000)
    }
}

fn decimal(v: &Value) -> Option<Decimal> {
    Decimal::from_str(v.as_str()?).ok()
}

fn levels(v: &Value) -> Option<Vec<(Decimal, Decimal)>> {
    v.as_array()?
        .iter()
        .map(|level| {
            let pair = level.as_array()?;
            Some((decimal(pair.first()?)?, decimal(pair.get(1)?)?))
        })
        .collect()
}

fn upper(v: &Value) -> Option<String> {
    Some(v.as_str()?.to_ascii_uppercase())
}

/// Parse one text frame. `None` for anything this server does not use
/// (acks, best-quote tickers, kinds added later) and for anything
/// malformed: a bad frame is skipped, never an error.
pub fn parse_ws_message(text: &str) -> Option<Parsed> {
    let v: Value = serde_json::from_str(text).ok()?;
    let ts_ms = event_time_ms(v.get("E")?.as_u64()?);
    match v.get("e").and_then(Value::as_str) {
        Some("depthUpdate") => Some(Parsed::Depth(DepthFrame {
            symbol: upper(v.get("s")?)?,
            first_id: v.get("U")?.as_u64()?,
            last_id: v.get("u")?.as_u64()?,
            bids: levels(v.get("b")?)?,
            asks: levels(v.get("a")?)?,
            ts_ms,
        })),
        Some("thirdPartyPrice") => Some(Parsed::Event(WsEvent::Index {
            index: v.get("I")?.as_str()?.to_owned(),
            price: v.get("p")?.as_str()?.parse::<f64>().ok().filter(|p| p.is_finite() && *p > 0.0)?,
            ts_ms,
        })),
        Some("contractStatus") => Some(Parsed::Event(WsEvent::ContractStatus {
            symbol: upper(v.get("s")?)?,
            event_ticker: upper(v.get("k")?)?,
            strike: v.get("p").and_then(decimal),
            new_status: v.get("n")?.as_str()?.to_owned(),
            ts_ms,
        })),
        Some(_) => None,
        // Trades carry no kind: a trade id, a price and a quantity.
        None if v.get("t").is_some() && v.get("q").is_some() => {
            Some(Parsed::Event(WsEvent::Trade {
                symbol: upper(v.get("s")?)?,
                price: decimal(v.get("p")?)?,
                qty: decimal(v.get("q")?)?,
                ts_ms,
            }))
        }
        None => None,
    }
}

/// What [`DepthTracker::accept`] decided about a frame.
#[derive(Debug, Clone, PartialEq)]
pub enum DepthStep {
    /// Apply this.
    Event(WsEvent),
    /// Already covered by what was applied; drop it.
    Stale,
    /// Updates were missed. The book for this symbol can no longer be
    /// trusted and the connection must be reopened for a new snapshot.
    Gap,
}

/// Per-symbol sequence state for the differential depth stream.
#[derive(Debug, Default)]
pub struct DepthTracker {
    last_id: HashMap<String, u64>,
}

impl DepthTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// The first frame seen for a symbol is its snapshot (the socket is
    /// opened with `snapshot=-1`).
    pub fn accept(&mut self, frame: DepthFrame) -> DepthStep {
        let snapshot = match self.last_id.get(&frame.symbol) {
            None => true,
            Some(&last) if frame.last_id <= last => return DepthStep::Stale,
            Some(&last) if frame.first_id > last => return DepthStep::Gap,
            Some(_) => false,
        };
        self.last_id.insert(frame.symbol.clone(), frame.last_id);
        DepthStep::Event(WsEvent::Depth {
            symbol: frame.symbol,
            bids: frame.bids,
            asks: frame.asks,
            snapshot,
            ts_ms: frame.ts_ms,
        })
    }

    /// Forget one symbol, so its next frame is taken as a snapshot.
    pub fn forget(&mut self, symbol: &str) {
        self.last_id.remove(symbol);
    }

    /// Forget everything: the connection was reopened.
    pub fn clear(&mut self) {
        self.last_id.clear();
    }
}

/// A book kept current from [`WsEvent::Depth`] events.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct LocalBook {
    bids: BTreeMap<Decimal, Decimal>,
    asks: BTreeMap<Decimal, Decimal>,
}

impl LocalBook {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn apply(
        &mut self,
        bids: &[(Decimal, Decimal)],
        asks: &[(Decimal, Decimal)],
        snapshot: bool,
    ) {
        if snapshot {
            self.clear();
        }
        for (side, changes) in [(&mut self.bids, bids), (&mut self.asks, asks)] {
            for &(price, qty) in changes {
                if qty.is_zero() {
                    side.remove(&price);
                } else {
                    side.insert(price, qty);
                }
            }
        }
    }

    pub fn clear(&mut self) {
        self.bids.clear();
        self.asks.clear();
    }

    /// Best bid first.
    pub fn bids(&self) -> Vec<(Decimal, Decimal)> {
        self.bids.iter().rev().map(|(p, q)| (*p, *q)).collect()
    }

    /// Best ask first.
    pub fn asks(&self) -> Vec<(Decimal, Decimal)> {
        self.asks.iter().map(|(p, q)| (*p, *q)).collect()
    }
}

/// The Up windows of one series (`"BTC05M"`, `"BTC15M"`) out of a page
/// of `GET /v1/prediction-markets/events`, earliest expiry first.
pub fn up_windows_from_events(body: &str, series: &str) -> anyhow::Result<Vec<UpWindow>> {
    let page: EventsPage = serde_json::from_str(body)
        .map_err(|e| anyhow::anyhow!("gemini event list is not the expected JSON: {e}"))?;
    let mut windows: Vec<UpWindow> = page
        .data
        .into_iter()
        .filter(|event| event.series.as_deref() == Some(series))
        .filter_map(|event| {
            // A malformed event is skipped, not fatal: the next poll will
            // see it again.
            let contract = event.contracts.into_iter().find(|c| c.ticker.as_deref() == Some("UP"))?;
            Some(UpWindow {
                event_ticker: event.ticker?,
                symbol: contract.instrument_symbol?,
                starts_at_ms: rfc3339_ms(event.start_time.as_deref()?)?,
                expires_at_ms: rfc3339_ms(event.expiry_date.as_deref()?)?,
                strike: contract
                    .strike
                    .and_then(|s| s.value)
                    .and_then(|v| Decimal::from_str(&v).ok()),
                market_state: contract.market_state.unwrap_or_default(),
            })
        })
        .collect();
    windows.sort_by_key(|w| w.expires_at_ms);
    Ok(windows)
}

fn rfc3339_ms(s: &str) -> Option<u64> {
    let at = chrono::DateTime::parse_from_rfc3339(s).ok()?;
    u64::try_from(at.timestamp_millis()).ok()
}

#[derive(Deserialize)]
struct EventsPage {
    #[serde(default)]
    data: Vec<EventRow>,
    pagination: Option<PageInfo>,
}

#[derive(Deserialize)]
struct PageInfo {
    limit: u64,
    offset: u64,
    total: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EventRow {
    ticker: Option<String>,
    series: Option<String>,
    start_time: Option<String>,
    expiry_date: Option<String>,
    #[serde(default)]
    contracts: Vec<ContractRow>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ContractRow {
    ticker: Option<String>,
    instrument_symbol: Option<String>,
    market_state: Option<String>,
    strike: Option<StrikeRow>,
}

#[derive(Deserialize)]
struct StrikeRow {
    value: Option<String>,
}

/// `(offset, limit, total)` from a page of the event list, if it says.
pub fn events_pagination(body: &str) -> Option<(u64, u64, u64)> {
    let page: EventsPage = serde_json::from_str(body).ok()?;
    page.pagination.map(|p| (p.offset, p.limit, p.total))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        let path = format!("{}/tests/fixtures/gemini/{name}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
    }

    fn lines(name: &str) -> Vec<String> {
        fixture(name).lines().map(str::to_owned).collect()
    }

    fn d(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }

    #[test]
    fn event_time_is_milliseconds_whatever_unit_it_came_in() {
        assert_eq!(event_time_ms(1_795_021_354), 1_795_021_354_000); // seconds
        assert_eq!(event_time_ms(1_795_021_505_600), 1_795_021_505_600); // ms
        assert_eq!(event_time_ms(1_795_022_187_405_302_187), 1_795_022_187_405); // ns
    }

    #[test]
    fn a_trade_line_parses_with_an_uppercase_symbol_and_ms_time() {
        let got = parse_ws_message(&lines("trade.jsonl")[0]);
        assert_eq!(
            got,
            Some(Parsed::Event(WsEvent::Trade {
                symbol: "GEMI-BTC05M2611181705-UP".into(),
                price: d("0.99"),
                qty: d("4"),
                ts_ms: 1_795_021_382_566,
            }))
        );
    }

    #[test]
    fn an_index_line_parses_with_seconds_turned_into_ms() {
        let got = parse_ws_message(&lines("index.jsonl")[0]);
        let Some(Parsed::Event(WsEvent::Index { index, price, ts_ms })) = got else {
            panic!("not an index event: {got:?}");
        };
        assert_eq!(index, "LRRR_XBT");
        assert!((price - 62_766.298_166_666_67).abs() < 1e-9);
        assert_eq!(ts_ms, 1_795_021_354_000);
    }

    #[test]
    fn a_contract_status_without_a_strike_parses_with_none() {
        let got = parse_ws_message(&lines("contract_status.jsonl")[0]);
        assert_eq!(
            got,
            Some(Parsed::Event(WsEvent::ContractStatus {
                symbol: "GEMI-NCAAM-2611182330-CHAT-WIN-S-WIN20".into(),
                event_ticker: "NCAAM-2611182330-CHAT-WIN-S".into(),
                strike: None,
                new_status: "Approved".into(),
                ts_ms: 1_795_021_383_721,
            }))
        );
    }

    #[test]
    fn a_contract_status_with_a_strike_carries_it() {
        let got = parse_ws_message(&lines("contract_status.jsonl")[2]);
        assert_eq!(
            got,
            Some(Parsed::Event(WsEvent::ContractStatus {
                symbol: "GEMI-BTC05M2611181710-UP".into(),
                event_ticker: "BTC05M2611181710".into(),
                strike: Some(d("62801.74")),
                new_status: "Active".into(),
                ts_ms: 1_795_021_505_600,
            }))
        );
    }

    #[test]
    fn a_depth_line_parses_to_a_frame_with_both_ids() {
        let got = parse_ws_message(&lines("depth.jsonl")[0]);
        assert_eq!(
            got,
            Some(Parsed::Depth(DepthFrame {
                symbol: "GEMI-BTC05M2611181720-UP".into(),
                first_id: 1_794_981_548_004_796,
                last_id: 1_794_981_548_004_796,
                bids: vec![(d("0.60"), d("15")), (d("0.59"), d("254")), (d("0.01"), d("35"))],
                asks: vec![(d("0.77"), d("652")), (d("0.99"), d("35"))],
                ts_ms: 1_795_022_187_405,
            }))
        );
    }

    #[test]
    fn acks_tickers_unknown_kinds_and_rubbish_are_skipped_not_errors() {
        assert_eq!(parse_ws_message(&lines("ack.jsonl")[0]), None);
        assert_eq!(parse_ws_message(&lines("book_ticker.jsonl")[0]), None);
        assert_eq!(parse_ws_message(r#"{"e":"somethingNew","E":1,"s":"x"}"#), None);
        assert_eq!(parse_ws_message("not json"), None);
        assert_eq!(parse_ws_message(r#"{"e":"depthUpdate","s":"x"}"#), None);
        assert_eq!(parse_ws_message("[1,2,3]"), None);
    }

    fn frame(first_id: u64, last_id: u64) -> DepthFrame {
        DepthFrame {
            symbol: "S".into(),
            first_id,
            last_id,
            bids: vec![(d("0.5"), d("1"))],
            asks: vec![],
            ts_ms: 7,
        }
    }

    #[test]
    fn the_first_depth_frame_for_a_symbol_is_its_snapshot() {
        let mut t = DepthTracker::new();
        let DepthStep::Event(WsEvent::Depth { snapshot, symbol, .. }) = t.accept(frame(10, 10))
        else {
            panic!("first frame not accepted");
        };
        assert!(snapshot);
        assert_eq!(symbol, "S");
    }

    #[test]
    fn a_frame_older_than_what_was_applied_is_stale() {
        let mut t = DepthTracker::new();
        t.accept(frame(10, 10));
        assert_eq!(t.accept(frame(4, 10)), DepthStep::Stale);
        assert_eq!(t.accept(frame(2, 9)), DepthStep::Stale);
    }

    #[test]
    fn a_frame_that_starts_at_the_last_applied_id_is_a_delta() {
        let mut t = DepthTracker::new();
        t.accept(frame(10, 10));
        let DepthStep::Event(WsEvent::Depth { snapshot, .. }) = t.accept(frame(10, 14)) else {
            panic!("delta not accepted");
        };
        assert!(!snapshot);
        // Overlapping the applied range is still fine.
        assert!(matches!(t.accept(frame(12, 20)), DepthStep::Event(_)));
    }

    #[test]
    fn a_frame_that_skips_ahead_is_a_gap() {
        let mut t = DepthTracker::new();
        t.accept(frame(10, 10));
        assert_eq!(t.accept(frame(12, 15)), DepthStep::Gap);
    }

    #[test]
    fn forgetting_a_symbol_makes_its_next_frame_a_snapshot() {
        let mut t = DepthTracker::new();
        t.accept(frame(10, 10));
        t.forget("S");
        let DepthStep::Event(WsEvent::Depth { snapshot, .. }) = t.accept(frame(50, 50)) else {
            panic!("frame after forget not accepted");
        };
        assert!(snapshot);
    }

    #[test]
    fn a_snapshot_replaces_the_book_and_a_zero_quantity_removes_a_level() {
        let mut b = LocalBook::new();
        b.apply(&[(d("0.40"), d("5")), (d("0.41"), d("2"))], &[(d("0.50"), d("9"))], true);
        assert_eq!(b.bids(), vec![(d("0.41"), d("2")), (d("0.40"), d("5"))]);
        b.apply(&[(d("0.41"), d("0.00"))], &[(d("0.49"), d("3"))], false);
        assert_eq!(b.bids(), vec![(d("0.40"), d("5"))]);
        assert_eq!(b.asks(), vec![(d("0.49"), d("3")), (d("0.50"), d("9"))]);
        b.apply(&[(d("0.10"), d("1"))], &[], true);
        assert_eq!(b.bids(), vec![(d("0.10"), d("1"))]);
        assert!(b.asks().is_empty());
    }

    /// The recorded run: a snapshot and 335 further frames, then a REST
    /// read of the same book. Applying the frames must give that book.
    #[test]
    fn the_recorded_snapshot_and_deltas_rebuild_the_book_rest_showed_next() {
        let mut tracker = DepthTracker::new();
        let mut book = LocalBook::new();
        let (mut snapshots, mut deltas, mut stale) = (0, 0, 0);
        for line in lines("depth.jsonl") {
            let Some(Parsed::Depth(frame)) = parse_ws_message(&line) else {
                panic!("not a depth frame: {line}");
            };
            match tracker.accept(frame) {
                DepthStep::Event(WsEvent::Depth { bids, asks, snapshot, .. }) => {
                    if snapshot {
                        snapshots += 1;
                    } else {
                        deltas += 1;
                    }
                    book.apply(&bids, &asks, snapshot);
                }
                DepthStep::Event(other) => panic!("unexpected {other:?}"),
                DepthStep::Stale => stale += 1,
                DepthStep::Gap => panic!("gap in a continuous recording"),
            }
        }
        assert_eq!((snapshots, deltas, stale), (1, 334, 1));

        let rest: Value = serde_json::from_str(&fixture("book_after_depth.json")).unwrap();
        let side = |name: &str| -> Vec<(Decimal, Decimal)> {
            rest[name]
                .as_array()
                .unwrap()
                .iter()
                .map(|l| (d(l["price"].as_str().unwrap()), d(l["amount"].as_str().unwrap())))
                .collect()
        };
        assert_eq!(book.bids(), side("bids"));
        assert_eq!(book.asks(), side("asks"));
        assert_eq!(book.bids().len(), 6);
    }

    #[test]
    fn the_event_list_gives_one_series_up_windows_in_expiry_order() {
        let body = fixture("events_btc.json");
        let five = up_windows_from_events(&body, "BTC05M").unwrap();
        assert_eq!(five.len(), 14);
        assert_eq!(
            five[0],
            UpWindow {
                event_ticker: "BTC05M2611181720".into(),
                symbol: "GEMI-BTC05M2611181720-UP".into(),
                starts_at_ms: 1_795_022_100_000, // 2026-11-18T17:15:00Z
                expires_at_ms: 1_795_022_400_000, // 2026-11-18T17:20:00Z
                strike: Some(d("62597.88")),
                market_state: "open".into(),
            }
        );
        assert_eq!(five[1].event_ticker, "BTC05M2611181725");
        assert_eq!(five[1].strike, None);
        assert_eq!(five[1].market_state, "post_only");
        assert!(five.windows(2).all(|w| w[0].expires_at_ms < w[1].expires_at_ms));
        assert!(five.iter().all(|w| w.expires_at_ms - w.starts_at_ms == 300_000));

        let fifteen = up_windows_from_events(&body, "BTC15M").unwrap();
        assert_eq!(fifteen.len(), 5);
        assert!(fifteen.iter().all(|w| w.expires_at_ms - w.starts_at_ms == 900_000));
        assert!(up_windows_from_events(&body, "DOGE05M").unwrap().is_empty());
    }

    #[test]
    fn an_event_list_that_is_not_json_is_an_error_and_a_bad_event_is_skipped() {
        assert!(up_windows_from_events("<html>", "BTC05M").is_err());
        let body = r#"{"data":[
            {"ticker":"BTC05M1","series":"BTC05M","startTime":"nonsense","expiryDate":"2026-11-18T17:20:00.000Z",
             "contracts":[{"ticker":"UP","instrumentSymbol":"GEMI-BTC05M1-UP","marketState":"open"}]},
            {"ticker":"BTC05M2","series":"BTC05M","startTime":"2026-11-18T17:15:00.000Z","expiryDate":"2026-11-18T17:20:00.000Z",
             "contracts":[]},
            {"ticker":"BTC05M3","series":"BTC05M","startTime":"2026-11-18T17:15:00.000Z","expiryDate":"2026-11-18T17:20:00.000Z",
             "contracts":[{"ticker":"UP","instrumentSymbol":"GEMI-BTC05M3-UP","marketState":"open","strike":{"type":"reference","value":"abc"}}]}
        ]}"#;
        let got = up_windows_from_events(body, "BTC05M").unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].event_ticker, "BTC05M3");
        assert_eq!(got[0].strike, None);
    }

    #[test]
    fn pagination_is_read_when_present() {
        assert_eq!(events_pagination(&fixture("events_btc.json")), Some((0, 100, 96)));
        assert_eq!(events_pagination(r#"{"data":[]}"#), None);
    }
}
