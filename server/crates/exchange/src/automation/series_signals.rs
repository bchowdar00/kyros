//! Publishes each series' round internals as signals.
//!
//! The registry declares five exchange-published signals per series
//! (`crate::registry::series_signals`). [`crate::automation::stats_signal`]
//! covers `stats`; this task covers the other four, all from the engine's
//! current round and book:
//!
//! | signal                    | published when                                       |
//! |---------------------------|------------------------------------------------------|
//! | `<slug>.current.strike.v1`| once per round, when the round is first seen (BTC)   |
//! | `<slug>.current.matchup.v1`| once per round, when the round is first seen (NBA)  |
//! | `<slug>.current.window.v1`| `settled` for the old round then `open` for the new   |
//! |                           | one at rollover, and `open` once a second while open |
//! | `<slug>.current.price.v1` | whenever top of book, mark or last trade changes, and |
//! |                           | once a second regardless while a book has a quote     |
//! | `<slug>.current.trades.v1`| once per fill, in fill order                         |
//!
//! One task per series polls the engine on a short interval, the same way
//! the stats publisher and the trades batcher do; a lock hold is a few
//! reads. Settlement and the next open happen in one engine call, so the
//! `settled` phase is never observable on the engine: the publisher
//! synthesises it for the round that just closed when it sees the round
//! id change, so a strategy still gets the transition.
//!
//! Prices are up-side probabilities as decimal strings (`0.520`). When one
//! side of the book is empty its quote is the implied extreme (`0.000` for
//! the bid, `1.000` for the ask), but the up-side mark is not: a one-sided
//! book keeps the mid it last had, and an even `0.500` on a round whose
//! book has never been two-sided, so a chart of the mark starts level
//! instead of at the boundary the engine's mark pins to. `last` is `0.000`
//! until the round's first fill. The price signal is not published until
//! the book has a quote; once it has one it is republished at least every
//! [`PRICE_HEARTBEAT`], so a trace of it keeps advancing through a quiet
//! book (a decided game late in the fourth quarter, say) instead of
//! ending on the last change.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value as Json, json};

use crate::automation::signal_bus::{Quality, SignalBus, SignalFrame};
use crate::automation::stats_signal::dollars;
use crate::engine::model::Fill;
use crate::engine::{Exchange, MarketEngine, MarketStatus, RECENT_FILLS_CAP, RoundReference};
use crate::pricing::feed::unix_now_ms;
use crate::types::{SeriesId, Side};

/// How often the engine is sampled.
const POLL_INTERVAL: Duration = Duration::from_millis(100);
/// How often the `open` window frame is republished while nothing changes.
const WINDOW_TICK: Duration = Duration::from_secs(1);
/// The up-side mark of a round whose book has never been two-sided.
const EVEN_MARK_TENTHS: u16 = 500;
/// The longest a quoted book goes without its price frame being republished.
pub const PRICE_HEARTBEAT: Duration = Duration::from_secs(1);

/// The four signal ids for `slug`.
fn signal_id(slug: &str, kind: &str) -> String {
    format!("{slug}.current.{kind}.v1")
}

/// The round as the publisher tracks it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Round {
    id: u32,
    closes_at_ms: u64,
    open: bool,
}

/// Top of book, mark and last trade, in tenths of a cent on the up side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PriceSnapshot {
    up: u16,
    bid: Option<u16>,
    ask: Option<u16>,
    last: Option<u16>,
}

/// What one poll reads from the engine.
#[derive(Debug, Clone)]
struct Sample {
    /// The round and what it opened on.
    round: Option<(Round, RoundReference)>,
    price: Option<PriceSnapshot>,
    /// Fills newer than `after`, oldest first.
    new_fills: Vec<Fill>,
}

fn sample(engine: &MarketEngine, after_fill_seq: u64) -> Sample {
    let round = engine.current_market().map(|m| {
        (
            Round { id: m.id.0, closes_at_ms: m.closes_at * 1000, open: m.status == MarketStatus::Open },
            m.reference,
        )
    });
    let book = engine.book();
    let last = engine.recent_trades(1).next().map(|f| f.price.tenths());
    // The raw implied mid, not `mark_price()`: that one snaps to the tick
    // grid and is `None` whenever the mid lands between ticks or the book
    // is one-sided, which is most of the time.
    let price = book.implied_mark_yes_tenths().map(|up| PriceSnapshot {
        up,
        bid: book.best_bid().map(|p| p.tenths()),
        ask: book.best_ask().map(|p| p.tenths()),
        last,
    });
    let mut new_fills: Vec<Fill> =
        engine.recent_trades(RECENT_FILLS_CAP).filter(|f| f.ts > after_fill_seq).copied().collect();
    new_fills.reverse();
    Sample { round, price, new_fills }
}

/// A probability in tenths of a cent as a canonical decimal string.
fn prob(tenths: u16) -> String {
    dollars(i64::from(tenths))
}

/// A BTC/USD spot as a decimal string (cents are plenty for a strike).
fn spot(price: f64) -> String {
    format!("{price:.2}")
}

fn frame(slug: &str, kind: &str, ts_ms: u64, values: Vec<(&str, Json)>) -> SignalFrame {
    let values: BTreeMap<String, Json> = values.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
    SignalFrame { signal_id: signal_id(slug, kind), ts_ms, quality: Quality::ready(), values }
}

pub fn strike_frame(slug: &str, ts_ms: u64, strike_btc_usd: f64) -> SignalFrame {
    frame(slug, "strike", ts_ms, vec![("price", json!(spot(strike_btc_usd)))])
}

/// The round's reference as its opening frame: the strike of a BTC
/// round, the matchup of an NBA round.
pub fn reference_frame(slug: &str, ts_ms: u64, reference: RoundReference) -> SignalFrame {
    match reference {
        RoundReference::Btc { strike_btc_usd } => strike_frame(slug, ts_ms, strike_btc_usd),
        RoundReference::Nba { home_team, away_team, pregame_home_prob, game_seq } => frame(
            slug,
            "matchup",
            ts_ms,
            vec![
                ("home_team", json!(home_team)),
                ("away_team", json!(away_team)),
                ("pregame_home_prob", json!(format!("{pregame_home_prob:.6}"))),
                ("game_seq", json!(game_seq)),
            ],
        ),
    }
}

pub fn window_frame(slug: &str, ts_ms: u64, phase: &str, seq: u32, remaining_ms: u64) -> SignalFrame {
    frame(
        slug,
        "window",
        ts_ms,
        vec![("phase", json!(phase)), ("seq", json!(seq)), ("remaining", json!(remaining_ms))],
    )
}

fn price_frame(slug: &str, outcomes: [&str; 2], ts_ms: u64, p: &PriceSnapshot) -> SignalFrame {
    let down = 1000u16.saturating_sub(p.up);
    frame(
        slug,
        "price",
        ts_ms,
        vec![
            (outcomes[0], json!(prob(p.up))),
            (outcomes[1], json!(prob(down))),
            ("bid", json!(prob(p.bid.unwrap_or(0)))),
            ("ask", json!(prob(p.ask.unwrap_or(1000)))),
            ("last", json!(prob(p.last.unwrap_or(0)))),
        ],
    )
}

pub fn trade_frame(slug: &str, ts_ms: u64, fill: &Fill) -> SignalFrame {
    let side = match fill.taker_side {
        Side::Bid => "buy",
        Side::Ask => "sell",
    };
    frame(
        slug,
        "trades",
        ts_ms,
        vec![
            ("taker", json!(fill.taker.to_string())),
            ("side", json!(side)),
            ("price", json!(prob(fill.price.tenths()))),
            ("size", json!(fill.qty.0.to_string())),
        ],
    )
}

/// Per-series publisher state between polls.
#[derive(Debug, Default)]
struct Tracker {
    round: Option<Round>,
    price: Option<PriceSnapshot>,
    last_fill_seq: u64,
    last_window_ms: u64,
    /// When the price frame last went out, for the heartbeat.
    last_price_ms: u64,
}

impl Tracker {
    /// Fold one sample into the frames to publish, in order.
    fn step(&mut self, slug: &str, outcomes: [&str; 2], now: u64, s: Sample) -> Vec<SignalFrame> {
        let mut out = Vec::new();
        if let Some((round, reference)) = s.round {
            let changed = self.round.map(|r| r.id) != Some(round.id);
            if changed {
                if let Some(prev) = self.round {
                    out.push(window_frame(slug, now, "settled", prev.id, 0));
                }
                out.push(reference_frame(slug, now, reference));
                // A new round starts with a fresh book. The fill sequence is
                // process-wide and the tape rolls across rounds, so the
                // high-water mark carries over.
                self.price = None;
            }
            let remaining = round.closes_at_ms.saturating_sub(now);
            let tick_due = now.saturating_sub(self.last_window_ms) >= WINDOW_TICK.as_millis() as u64;
            if round.open && (changed || tick_due) {
                out.push(window_frame(slug, now, "open", round.id, remaining));
                self.last_window_ms = now;
            }
            self.round = Some(round);
        }
        if let Some(mut p) = s.price {
            // The engine pins a one-sided book's mark to the boundary;
            // the signal keeps the last two-sided mid instead.
            if p.bid.is_none() || p.ask.is_none() {
                p.up = self.price.map_or(EVEN_MARK_TENTHS, |prev| prev.up);
            }
            let heartbeat_due = now.saturating_sub(self.last_price_ms) >= PRICE_HEARTBEAT.as_millis() as u64;
            if self.price != Some(p) || heartbeat_due {
                out.push(price_frame(slug, outcomes, now, &p));
                self.price = Some(p);
                self.last_price_ms = now;
            }
        }
        for fill in &s.new_fills {
            out.push(trade_frame(slug, now, fill));
            self.last_fill_seq = self.last_fill_seq.max(fill.ts);
        }
        out
    }
}

/// Spawn the publisher for `series`. Runs for the life of the process.
pub fn spawn_series_publisher(
    exchange: Arc<Mutex<Exchange>>,
    series: SeriesId,
    bus: Arc<SignalBus>,
) -> tokio::task::JoinHandle<()> {
    use tokio::time::{MissedTickBehavior, interval};

    tokio::spawn(async move {
        let slug = series.slug();
        let outcomes = series.outcomes();
        let mut ticker = interval(POLL_INTERVAL);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut tracker = Tracker::default();
        let mut announced = false;
        loop {
            ticker.tick().await;
            let s = {
                let x = exchange.lock().expect("exchange mutex poisoned");
                sample(x.engine(series), tracker.last_fill_seq)
            };
            let frames = tracker.step(slug, outcomes, unix_now_ms(), s);
            if !frames.is_empty() && !announced {
                announced = true;
                tracing::info!(series = slug, "series signals: first round seen");
            }
            for f in frames {
                bus.publish(f);
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::automation::signal_bus::{field_types, to_signal_event};
    use crate::automation::signals::SignalStore;
    use crate::automation::value::Value;
    use crate::engine::Engine;
    use crate::registry;
    use crate::series::{BTC_OUTCOMES, SERIES};
    use crate::engine::TradeSource;
    use crate::ticks::PriceTick;
    use crate::types::{Address, Qty, Tenths};
    use rust_decimal::Decimal;
    use std::str::FromStr;

    fn addr(n: u8) -> Address {
        Address([n; 20])
    }

    /// Every frame this publisher emits must type against the registry
    /// entry for its signal, or the platform drops it.
    #[test]
    fn frames_ingest_against_the_generated_registry() {
        let reg = registry::for_tests();
        let mut store = SignalStore::from_registry(&reg);
        let fill = Fill {
            taker: addr(1),
            maker: addr(2),
            taker_side: Side::Ask,
            price: PriceTick::from_tenths(520).unwrap(),
            qty: Qty(7),
            ts: 3,
            kind: crate::engine::model::FillKind::Open,
        };
        let p = PriceSnapshot { up: 520, bid: Some(510), ask: None, last: Some(520) };
        for spec in SERIES {
            let reference = if spec.id.is_btc() {
                crate::engine::RoundReference::Btc { strike_btc_usd: 65432.109 }
            } else {
                crate::engine::RoundReference::Nba { home_team: "BOS", away_team: "SAS", pregame_home_prob: 0.6, game_seq: 1 }
            };
            let frames = [
                reference_frame(spec.slug, 1, reference),
                window_frame(spec.slug, 1, "open", 9, 1500),
                price_frame(spec.slug, spec.outcomes, 1, &p),
                trade_frame(spec.slug, 1, &fill),
            ];
            for f in frames {
                assert!(store.enable(&f.signal_id), "{} in registry", f.signal_id);
                let def = reg.signals.iter().find(|s| s.signal_id == f.signal_id).unwrap();
                let event = to_signal_event(&f, &field_types(def)).expect("frame types against the registry");
                let typed = store.ingest(&event).unwrap_or_else(|e| panic!("{}: {e}", f.signal_id));
                assert_eq!(typed.len(), def.fields.len());
            }
        }
        let price_id = signal_id(SERIES[0].slug, "price");
        assert_eq!(store.field(&price_id, "down"), Some(&Value::Decimal(Decimal::from_str("0.480").unwrap())));
        assert_eq!(store.field(&price_id, "ask"), Some(&Value::Decimal(Decimal::from_str("1.000").unwrap())));
        let trades_id = signal_id(SERIES[0].slug, "trades");
        assert_eq!(store.field(&trades_id, "side"), Some(&Value::Text("sell".into())));
        assert_eq!(store.field(&trades_id, "size"), Some(&Value::Decimal(Decimal::from(7))));
        assert_eq!(store.field(&trades_id, "taker"), Some(&Value::Text(addr(1).to_string())));
    }

    #[test]
    fn nba_round_opens_with_a_matchup_frame_keyed_home_away() {
        use crate::series::{NBA_OUTCOMES, SeriesSpec};
        let spec = SeriesSpec { id: crate::types::SeriesId(0), slug: "nba-h2h", duration_secs: 300, underlying: "nba", outcomes: NBA_OUTCOMES };
        let mut x = crate::engine::Exchange::new(Tenths(1_000_000), std::slice::from_ref(&spec));
        x.open_first_market_with(
            spec.id,
            crate::engine::RoundReference::Nba { home_team: "BOS", away_team: "SAS", pregame_home_prob: 0.6142, game_seq: 3 },
            100,
            300,
        );
        let mut t = Tracker::default();
        let frames = t.step(spec.slug, spec.outcomes, 100_000, sample(x.engine(spec.id), 0));
        let kinds: Vec<_> = frames.iter().map(|f| f.signal_id.as_str()).collect();
        assert_eq!(kinds, ["nba-h2h.current.matchup.v1", "nba-h2h.current.window.v1"]);
        assert_eq!(frames[0].values["home_team"], "BOS");
        assert_eq!(frames[0].values["pregame_home_prob"], "0.614200");
        assert_eq!(frames[0].values["game_seq"], 3);
        let p = PriceSnapshot { up: 620, bid: Some(610), ask: Some(630), last: None };
        let f = price_frame(spec.slug, spec.outcomes, 1, &p);
        assert_eq!(f.values["home"], "0.620");
        assert_eq!(f.values["away"], "0.380");
        assert!(!f.values.contains_key("up"));
        // Every frame types against the NBA internals the registry would generate.
        let reg = crate::registry::for_tests();
        let internals: Vec<strategy_compiler::ir::SignalDef> = crate::registry::nba_series_internals_for_tests(&spec);
        for fr in frames.iter().chain(std::iter::once(&f)) {
            let def = internals.iter().find(|d| d.signal_id == fr.signal_id).unwrap_or_else(|| panic!("{}", fr.signal_id));
            to_signal_event(fr, &field_types(def)).unwrap_or_else(|e| panic!("{}: {e}", fr.signal_id));
        }
        let _ = reg;
    }

    #[test]
    fn tracker_publishes_rollover_and_fills_in_order() {
        let mut e = Engine::new(Tenths(1_000_000));
        let slug = "s";
        let mut t = Tracker::default();
        // Nothing yet.
        assert!(t.step(slug, BTC_OUTCOMES, 1_000, sample(e.market(), 0)).is_empty());

        e.open_first_market(50_000.0, 100, 120);
        let frames = t.step(slug, BTC_OUTCOMES, 100_000, sample(e.market(), 0));
        let kinds: Vec<_> = frames.iter().map(|f| f.signal_id.as_str()).collect();
        assert_eq!(kinds, ["s.current.strike.v1", "s.current.window.v1"]);
        assert_eq!(frames[0].values["price"], "50000.00");
        assert_eq!(frames[1].values["phase"], "open");
        assert_eq!(frames[1].values["remaining"], 120_000);

        // Same round, 100 ms later: nothing changed, window tick not due.
        assert!(t.step(slug, BTC_OUTCOMES, 100_100, sample(e.market(), 0)).is_empty());
        // A second later the open frame repeats with less time left.
        let frames = t.step(slug, BTC_OUTCOMES, 101_000, sample(e.market(), 0));
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].values["remaining"], 119_000);

        // A bid alone: the engine's mark pins to 99.9¢, the signal says even.
        e.get_or_create_account(addr(1));
        e.get_or_create_account(addr(2));
        e.place_limit(addr(1), Side::Bid, PriceTick::from_tenths(500).unwrap(), Qty(10), TradeSource::Ui).unwrap();
        let frames = t.step(slug, BTC_OUTCOMES, 101_050, sample(e.market(), t.last_fill_seq));
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].values["up"], "0.500");
        assert_eq!(frames[0].values["ask"], "1.000");
        // Quotes on both sides, then a fill on each poll.
        e.place_limit(addr(2), Side::Ask, PriceTick::from_tenths(540).unwrap(), Qty(10), TradeSource::Ui).unwrap();
        let frames = t.step(slug, BTC_OUTCOMES, 101_100, sample(e.market(), t.last_fill_seq));
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].signal_id, "s.current.price.v1");
        assert_eq!(frames[0].values["up"], "0.520");
        assert_eq!(frames[0].values["bid"], "0.500");
        assert_eq!(frames[0].values["ask"], "0.540");
        assert_eq!(frames[0].values["last"], "0.000");

        e.place_market(addr(1), Side::Bid, Qty(3), TradeSource::Ui).unwrap();
        e.place_market(addr(2), Side::Ask, Qty(4), TradeSource::Ui).unwrap();
        let frames = t.step(slug, BTC_OUTCOMES, 101_200, sample(e.market(), t.last_fill_seq));
        let kinds: Vec<_> = frames.iter().map(|f| f.signal_id.as_str()).collect();
        assert_eq!(kinds, ["s.current.price.v1", "s.current.trades.v1", "s.current.trades.v1"]);
        assert_eq!(frames[1].values["side"], "buy");
        assert_eq!(frames[1].values["size"], "3");
        assert_eq!(frames[2].values["side"], "sell");
        assert_eq!(frames[2].values["size"], "4");
        // Already published: no repeats.
        assert!(t.step(slug, BTC_OUTCOMES, 101_300, sample(e.market(), t.last_fill_seq)).is_empty());
        // The ask side empties: the mark keeps its last mid rather than pinning.
        e.place_market(addr(1), Side::Bid, Qty(7), TradeSource::Ui).unwrap();
        let frames = t.step(slug, BTC_OUTCOMES, 101_400, sample(e.market(), t.last_fill_seq));
        let price = frames.iter().find(|f| f.signal_id == "s.current.price.v1").expect("price republished");
        assert_eq!(price.values["up"], "0.520");
        assert_eq!(price.values["ask"], "1.000");
        // A quiet book still heartbeats its price once a second, so a trace keeps advancing.
        assert!(t.step(slug, BTC_OUTCOMES, 101_900, sample(e.market(), t.last_fill_seq)).iter().all(|f| f.signal_id != "s.current.price.v1"));
        let frames = t.step(slug, BTC_OUTCOMES, 102_400, sample(e.market(), t.last_fill_seq));
        let beat = frames.iter().find(|f| f.signal_id == "s.current.price.v1").expect("heartbeat");
        assert_eq!(beat.values["up"], "0.520");
        assert_eq!(beat.ts_ms, 102_400);

        // Rollover: settled for the old round, then strike and open for the new.
        e.resolve_and_open_next(50_100.0, 50_100.0, 220, 120);
        let frames = t.step(slug, BTC_OUTCOMES, 220_000, sample(e.market(), t.last_fill_seq));
        let kinds: Vec<_> = frames.iter().map(|f| f.signal_id.as_str()).collect();
        assert_eq!(kinds, ["s.current.window.v1", "s.current.strike.v1", "s.current.window.v1"]);
        assert_eq!(frames[0].values["phase"], "settled");
        assert_eq!(frames[0].values["seq"], 1);
        assert_eq!(frames[2].values["phase"], "open");
        assert_eq!(frames[2].values["seq"], 2);
        assert!(t.last_fill_seq > 0, "fill high-water mark carries across rounds");
        assert!(t.step(slug, BTC_OUTCOMES, 220_100, sample(e.market(), t.last_fill_seq)).is_empty(), "old fills are not replayed");
    }
}
