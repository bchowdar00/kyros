//! Trades batcher: polls the engine's recent-fills buffer every
//! [`TRADES_BATCH_INTERVAL_MS`], aggregates new fills into per-taker-order
//! rows, caps at [`TRADES_PER_BATCH`], and broadcasts on the trades
//! channel. Uses the underlying fill `ts` as a high-water mark so we
//! never re-emit a row.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use rand::seq::SliceRandom;
use tokio::sync::broadcast;

use crate::engine::model::Fill;
use crate::engine::{Exchange, Ledger, MarketEngine, RECENT_FILLS_CAP};
use crate::types::{Address, SeriesId, Side};

use super::{TradeIdentity, TradeRow, TradeSide, TradesMessage};

/// How often the batcher polls the engine for new taker trades.
pub const TRADES_BATCH_INTERVAL_MS: u64 = 100;

/// Hard cap on rows pushed in a single broadcast batch. If more taker
/// orders happened in the interval we keep only the newest 20 — the
/// activity feed is a "feels populated" tape, not an audit log.
pub const TRADES_PER_BATCH: usize = 20;

/// Number of taker trades sent in the initial snapshot when a client
/// connects. A bit more than the per-batch cap so the feed lands looking
/// already populated.
pub const TRADES_SNAPSHOT_LIMIT: usize = 30;

/// In-progress aggregation of one taker order's fills. Lives only inside
/// the batcher — not on the wire.
struct AggregatedTakerTrade {
    first_ts: u64,
    last_ts: u64,
    taker: Address,
    maker: Address,
    taker_side: Side,
    qty_total: u32,
    /// Sum of `qty × price` (both in their native integer units), used
    /// to compute the VWAP at the end. `u64` so we can't overflow even
    /// in the worst case (1M contracts × 999 tenths fits easily).
    notional_tenths: u64,
}

/// Identity for the activity feed. Name/avatar come from the *root*
/// account so strategy sub-accounts display as their owner.
fn identity_of(ledger: &Ledger, address: Address) -> TradeIdentity {
    let root_address = ledger.root_of(&address);
    let acct = ledger.account(&root_address);
    TradeIdentity {
        address,
        root_address,
        username: acct.and_then(|a| a.username.clone()),
        color: acct.and_then(|a| a.color.clone()),
        emoji: acct.and_then(|a| a.emoji.clone()),
    }
}

/// Collapse a chronological run of fills into per-taker-order
/// aggregates split by maker. Fills from one taker order arrive
/// contiguously in the engine's sequence (cross() holds the engine lock
/// and runs to completion), but one taker can sweep multiple makers. We
/// keep maker identity in the grouping so maker-side user notifications
/// can report the correct filled quantity.
fn aggregate_taker_trades(fills: &[Fill]) -> Vec<AggregatedTakerTrade> {
    let mut out: Vec<AggregatedTakerTrade> = Vec::with_capacity(fills.len());
    for f in fills {
        if let Some(last) = out.last_mut()
            && last.taker == f.taker
            && last.maker == f.maker
            && last.taker_side == f.taker_side
            && f.ts == last.last_ts + 1
        {
            last.qty_total += f.qty.0;
            last.notional_tenths += (f.qty.0 as u64) * (f.price.tenths() as u64);
            last.last_ts = f.ts;
            continue;
        }
        out.push(AggregatedTakerTrade {
            first_ts: f.ts,
            last_ts: f.ts,
            taker: f.taker,
            maker: f.maker,
            taker_side: f.taker_side,
            qty_total: f.qty.0,
            notional_tenths: (f.qty.0 as u64) * (f.price.tenths() as u64),
        });
    }
    out
}

/// Collapse to at most one row per taker *identity*, keeping that
/// taker's most recent aggregate. Keyed by root address so a user's
/// many strategy sub-accounts show up as one trader, not fifty. This
/// prevents the feed from showing the same trader multiple times in a
/// single batch or snapshot.
fn dedupe_by_taker(
    ledger: &Ledger,
    rows: Vec<AggregatedTakerTrade>,
) -> Vec<AggregatedTakerTrade> {
    let mut best: HashMap<Address, AggregatedTakerTrade> = HashMap::with_capacity(rows.len());
    for row in rows {
        let key = ledger.root_of(&row.taker);
        let replace = match best.get(&key) {
            None => true,
            Some(existing) => row.last_ts > existing.last_ts,
        };
        if replace {
            best.insert(key, row);
        }
    }
    best.into_values().collect()
}

fn enrich_taker_trade(ledger: &Ledger, series: SeriesId, t: &AggregatedTakerTrade) -> TradeRow {
    let vwap_tenths = if t.qty_total > 0 {
        (t.notional_tenths / t.qty_total as u64) as u16
    } else {
        0
    };
    TradeRow {
        series,
        price: vwap_tenths,
        qty: t.qty_total,
        ts: t.first_ts,
        side: match t.taker_side {
            Side::Bid => TradeSide::Yes,
            Side::Ask => TradeSide::No,
        },
        taker: identity_of(ledger, t.taker),
        maker: identity_of(ledger, t.maker),
    }
}

pub fn build_trades_snapshot(exchange: &Exchange, series: SeriesId, limit: usize) -> TradesMessage {
    build_trades_snapshot_for(exchange.engine(series), exchange.ledger(), limit)
}

/// One row per fill, as it happened. A mirrored series' tape is the
/// venue's own prints: every one carries the mirror's address (the
/// venue's traders are unknown), so collapsing rows per trader would
/// leave a single row, and a tape is read in order.
fn print_row(ledger: &Ledger, series: SeriesId, f: &Fill) -> TradeRow {
    TradeRow {
        series,
        price: f.price.tenths(),
        qty: f.qty.0,
        ts: f.ts,
        side: match f.taker_side {
            Side::Bid => TradeSide::Yes,
            Side::Ask => TradeSide::No,
        },
        taker: identity_of(ledger, f.taker),
        maker: identity_of(ledger, f.maker),
    }
}

fn build_trades_snapshot_for(engine: &MarketEngine, ledger: &Ledger, limit: usize) -> TradesMessage {
    if engine.mirror().is_some() {
        let series = engine.series();
        let trades = engine.recent_trades(limit).map(|f| print_row(ledger, series, f)).collect();
        return TradesMessage { kind: "snapshot", trades };
    }
    let series = engine.series();
    // Walk the entire recent-fills buffer chronologically, aggregate
    // into taker trades, collapse to one row per taker (newest), then
    // keep the newest `limit`. Sending the newest is what makes the
    // feed land already populated, and the per-taker dedup ensures the
    // visible roster shows distinct traders.
    let mut fills: Vec<Fill> = engine.recent_trades(RECENT_FILLS_CAP).copied().collect();
    fills.reverse();
    let aggregated = aggregate_taker_trades(&fills);
    let mut deduped = dedupe_by_taker(ledger, aggregated);
    // Newest taker activity first, then truncate to the wire limit.
    deduped.sort_by(|a, b| b.last_ts.cmp(&a.last_ts));
    deduped.truncate(limit);
    let mut trades: Vec<TradeRow> = deduped
        .iter()
        .map(|t| enrich_taker_trade(ledger, series, t))
        .collect();
    // Shuffle so the snapshot doesn't read as a strict recency ordering.
    trades.shuffle(&mut rand::rng());
    TradesMessage {
        kind: "snapshot",
        trades,
    }
}

/// The rows for the fills newer than `last_seen_ts`, which it advances;
/// `None` when there are none.
fn next_batch(engine: &MarketEngine, ledger: &Ledger, last_seen_ts: &mut u64) -> Option<TradesMessage> {
    let series = engine.series();
    let mut new_fills: Vec<Fill> =
        engine.recent_trades(RECENT_FILLS_CAP).filter(|f| f.ts > *last_seen_ts).copied().collect();
    if new_fills.is_empty() {
        return None;
    }
    if engine.mirror().is_some() {
        // Newest first, like the snapshot.
        *last_seen_ts = new_fills.first().expect("non-empty by check").ts;
        let trades = new_fills.iter().map(|f| print_row(ledger, series, f)).collect();
        return Some(TradesMessage { kind: "batch", trades });
    }
    new_fills.reverse(); // chronological
    *last_seen_ts = new_fills.last().expect("non-empty by check").ts;
    let aggregated = aggregate_taker_trades(&new_fills);
    // Collapse to one row per taker (most recent activity),
    // then keep the newest TRADES_PER_BATCH unique-taker
    // rows under heavy churn.
    let mut deduped = dedupe_by_taker(ledger, aggregated);
    deduped.sort_by(|a, b| b.last_ts.cmp(&a.last_ts));
    deduped.truncate(TRADES_PER_BATCH);
    let mut trades: Vec<TradeRow> = deduped.iter().map(|t| enrich_taker_trade(ledger, series, t)).collect();
    // Shuffle so the batch doesn't land as a strict
    // recency-ordered run.
    trades.shuffle(&mut rand::rng());
    Some(TradesMessage { kind: "batch", trades })
}

/// Background task that polls the engine's recent-fills buffer and
/// broadcasts per-taker-order rows.
pub fn spawn_trades_batcher(
    exchange: Arc<Mutex<Exchange>>,
    series: SeriesId,
    tx: broadcast::Sender<TradesMessage>,
) -> tokio::task::JoinHandle<()> {
    use tokio::time::{Duration, MissedTickBehavior, interval};

    tokio::spawn(async move {
        let mut last_seen_ts: u64 = 0;
        let mut ticker = interval(Duration::from_millis(TRADES_BATCH_INTERVAL_MS));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            ticker.tick().await;
            let msg = {
                let exchange = exchange.lock().expect("exchange mutex poisoned");
                next_batch(exchange.engine(series), exchange.ledger(), &mut last_seen_ts)
            };
            if let Some(msg) = msg {
                let _ = tx.send(msg);
            }
        }
    })
}

#[cfg(test)]
mod mirrored_series_tests {
    use super::*;
    use crate::engine::Engine;
    use crate::ticks::PriceTick;
    use crate::types::{DEFAULT_SERIES, Qty, Tenths};

    const MIRROR: Address = Address([0x6e; 20]);

    fn tick(t: u16) -> PriceTick {
        PriceTick::from_tenths(t).unwrap()
    }

    /// A mirrored series with three of the venue's prints on its tape.
    fn rig() -> Engine {
        let mut e = Engine::new(Tenths(1_000_000));
        e.seed_bot_account_in(MIRROR, Tenths(1_000_000_000), "gemini-book".into(), None, None, DEFAULT_SERIES);
        e.open_first_market(100_000.0, 1_900_000_000, 300);
        let m = e.exchange_mut().engine_mut(DEFAULT_SERIES);
        m.set_mirror(MIRROR);
        m.record_print(Side::Bid, tick(560), Qty(2));
        m.record_print(Side::Ask, tick(540), Qty(9));
        m.record_print(Side::Bid, tick(550), Qty(4));
        e
    }

    fn shape(rows: &[TradeRow]) -> Vec<(u16, u32, bool)> {
        rows.iter().map(|r| (r.price, r.qty, matches!(r.side, TradeSide::Yes))).collect()
    }

    #[test]
    fn every_print_is_its_own_row_newest_first() {
        let e = rig();
        let snap = build_trades_snapshot(e.exchange(), DEFAULT_SERIES, 30);
        // Not collapsed to one row per trader, and not shuffled: the
        // venue's traders are unknown, and this is a tape.
        assert_eq!(shape(&snap.trades), vec![(550, 4, true), (540, 9, false), (560, 2, true)]);
        assert!(snap.trades.iter().all(|r| r.taker.username.as_deref() == Some("gemini-book")));
        assert!(snap.trades[0].ts > snap.trades[1].ts);
        let two = build_trades_snapshot(e.exchange(), DEFAULT_SERIES, 2);
        assert_eq!(shape(&two.trades), vec![(550, 4, true), (540, 9, false)]);
    }

    #[test]
    fn a_batch_carries_only_the_prints_since_the_last_one() {
        let mut e = rig();
        let mut seen = 0;
        let first = next_batch(e.exchange().engine(DEFAULT_SERIES), e.exchange().ledger(), &mut seen).expect("three prints");
        assert_eq!(first.kind, "batch");
        assert_eq!(shape(&first.trades), vec![(550, 4, true), (540, 9, false), (560, 2, true)]);
        assert!(next_batch(e.exchange().engine(DEFAULT_SERIES), e.exchange().ledger(), &mut seen).is_none());
        e.exchange_mut().engine_mut(DEFAULT_SERIES).record_print(Side::Ask, tick(530), Qty(1));
        let second = next_batch(e.exchange().engine(DEFAULT_SERIES), e.exchange().ledger(), &mut seen).expect("one more");
        assert_eq!(shape(&second.trades), vec![(530, 1, false)]);
    }
}

#[cfg(test)]
mod sub_account_tests {
    use super::*;
    use crate::engine::{Engine, TradeSource};
    use crate::ticks::PriceTick;
    use crate::types::{Qty, Tenths};

    fn addr(byte: u8) -> Address {
        Address([byte; 20])
    }

    #[test]
    fn feed_rows_show_parent_identity_and_collapse_children() {
        let mut e = Engine::new(Tenths(1_000_000));
        let (parent, bot) = (addr(1), addr(2));
        e.register_user(parent);
        {
            let p = e.get_or_create_account(parent);
            p.username = Some("jet".into());
            p.emoji = Some("🦊".into());
        }
        e.seed_bot_account(bot, Tenths(50_000_000), "bot".into(), None, None);
        let c1 = e.create_sub_account(parent, Tenths(100_000)).unwrap();
        let c2 = e.create_sub_account(parent, Tenths(100_000)).unwrap();

        let t = PriceTick::from_tenths(500).unwrap();
        e.place_limit(bot, Side::Ask, t, Qty(10), TradeSource::Bot).unwrap();
        e.place_market(c1, Side::Bid, Qty(2), TradeSource::Strategy).unwrap();
        e.place_market(c2, Side::Bid, Qty(3), TradeSource::Strategy).unwrap();

        let snap = build_trades_snapshot(e.exchange(), crate::types::DEFAULT_SERIES, 30);
        assert_eq!(snap.trades.len(), 1, "two children, one identity → one row");
        let row = &snap.trades[0];
        assert_eq!(row.taker.address, c2, "the newest child's trade wins");
        assert_eq!(row.taker.root_address, parent);
        assert_eq!(row.taker.username.as_deref(), Some("jet"));
        assert_eq!(row.taker.emoji.as_deref(), Some("🦊"));
        assert_eq!(row.qty, 3);
        // Bots are their own root.
        assert_eq!(row.maker.root_address, bot);
        assert_eq!(row.maker.username.as_deref(), Some("bot"));

        // Unique traders counts the family once: {parent, bot}.
        assert_eq!(e.market_unique_traders(), 2);
    }
}
