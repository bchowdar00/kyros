//! The real venue: [`Venue`] over `Arc<Mutex<Exchange>>` (plan §8.2).
//!
//! Transport is the mutex shim from §3.5: every call takes the exchange
//! lock for a few microseconds. When the two-lane ingress queue (§3.7)
//! lands, only this file changes.
//!
//! Order fates come back through the engine's [`ExecutionSink`]: the
//! callback installed by [`EngineVenue::install`] converts each engine
//! event to the runtime's decimal, quoted-frame [`ExecutionEvent`] and
//! pushes it into the runtime manager's inbox *synchronously, under the
//! engine lock*. The scheduler sends `window_settled` after it releases
//! the lock, so the lock-phase cancel acks always precede the settlement
//! in the manager's channel (the ordering the session reset relies on).
//!
//! Good-till-time (`expire_after_ms`) is adapter-owned: a Tokio sleep per
//! resting GTD order that calls `Exchange::expire`, which reports `Expiry`
//! if the order is still on the book and is a no-op otherwise.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use rust_decimal::Decimal;

use crate::engine::{self, ExecKind, Exchange, MarketStatus, OrderStatus, ReplaceError, TradeSource};
use crate::types::{Address, OrderId, SeriesId, Tenths};

use super::translate::{self, tick_to_decimal};
use super::{Accepted, AccountView, ExecutionEvent, ExecutionKind, RejectReason, SeriesMap, Venue, VenueOrder};
use crate::automation::runtime::RuntimeHandle;
use crate::engine::ExecutionSink;

pub struct EngineVenue {
    exchange: Arc<Mutex<Exchange>>,
    map: SeriesMap,
}

impl EngineVenue {
    pub fn new(exchange: Arc<Mutex<Exchange>>, map: SeriesMap) -> Arc<EngineVenue> {
        Arc::new(EngineVenue { exchange, map })
    }

    /// Install the execution-event sink on the exchange, converting into
    /// `runtime`'s inbox. Call once at boot before any strategy order.
    pub fn install(exchange: &Arc<Mutex<Exchange>>, runtime: RuntimeHandle) {
        let sink = ExecutionSink::new(move |ev| runtime.execution(convert(ev)));
        exchange.lock().expect("exchange mutex poisoned").set_execution_sink(sink);
    }

    pub fn map(&self) -> &SeriesMap {
        &self.map
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Exchange> {
        self.exchange.lock().expect("exchange mutex poisoned")
    }

    /// The open market of the series the instrument trades in, checked
    /// against the deployment's home series.
    fn open_market(&self, x: &Exchange, series: SeriesId, instrument_id: &str) -> Result<u32, RejectReason> {
        let target = self.map.instrument_series(instrument_id).ok_or(RejectReason::InvalidOrder)?;
        if target != series {
            return Err(RejectReason::InvalidOrder);
        }
        match x.engine(series).current_market() {
            Some(m) if m.status == MarketStatus::Open => Ok(m.id.0),
            _ => Err(RejectReason::MarketClosed),
        }
    }

    /// Arm the good-till-time sweep for an order that rests.
    fn arm_expiry(&self, series: SeriesId, account: Address, order_id: OrderId, after_ms: u64) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            tracing::warn!(%series, order = order_id.0, "expire_after outside a Tokio runtime: not armed");
            return;
        };
        let exchange = self.exchange.clone();
        handle.spawn(async move {
            tokio::time::sleep(Duration::from_millis(after_ms)).await;
            let mut x = exchange.lock().expect("exchange mutex poisoned");
            // Already filled, cancelled or settled: nothing to do.
            let _ = x.expire(series, account, order_id);
        });
    }

    fn accepted(&self, series: SeriesId, account: Address, order: &VenueOrder, out: engine::PlaceOutcome, market_id: u32) -> Accepted {
        let rests = matches!(out.status, OrderStatus::Rested | OrderStatus::PartiallyFilled);
        if let (true, Some(ms)) = (rests, order.expire_after_ms) {
            self.arm_expiry(series, account, out.order_id, ms);
        }
        Accepted { venue_order_id: out.order_id.0, market_id: Some(market_id) }
    }
}

impl Venue for EngineVenue {
    fn submit(&self, series: SeriesId, account: Address, order: &VenueOrder) -> Result<Accepted, RejectReason> {
        let eo = translate::to_engine(order)?;
        let mut x = self.lock();
        let market_id = self.open_market(&x, series, &order.instrument_id)?;
        let result = match eo.price {
            Some(price) => x.place_limit_with(series, account, eo.side, price, eo.qty, TradeSource::Strategy, eo.opts),
            None => x.place_market_with(series, account, eo.side, eo.qty, TradeSource::Strategy, eo.opts),
        };
        drop(x);
        match result {
            Ok(out) => Ok(self.accepted(series, account, order, out, market_id)),
            Err(e) => Err(translate::reject_reason(e)),
        }
    }

    fn cancel(&self, series: SeriesId, account: Address, venue_order_id: u64) -> Result<(), String> {
        self.lock()
            .cancel(series, account, OrderId(venue_order_id))
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    fn cancel_replace(&self, series: SeriesId, account: Address, old: u64, order: &VenueOrder) -> Result<Accepted, RejectReason> {
        let eo = translate::to_engine(order)?;
        let Some(price) = eo.price else {
            return Err(RejectReason::InvalidOrder);
        };
        let mut x = self.lock();
        let market_id = self.open_market(&x, series, &order.instrument_id)?;
        let result = x.cancel_replace(series, account, OrderId(old), eo.side, price, eo.qty, TradeSource::Strategy, eo.opts);
        drop(x);
        match result {
            Ok(out) => Ok(self.accepted(series, account, order, out, market_id)),
            Err(ReplaceError::Cancel(_)) => Err(RejectReason::InvalidOrder),
            Err(ReplaceError::Place(e)) => Err(translate::reject_reason(e)),
        }
    }

    fn account_view(&self, series: SeriesId, account: Address) -> AccountView {
        account_view(&self.lock(), series, account)
    }
}

/// The sub-account as any venue over this ledger sees it.
pub fn account_view(exchange: &Exchange, series: SeriesId, account: Address) -> AccountView {
    let Some(acct) = exchange.account(&account) else {
        return AccountView::default();
    };
    let engine = exchange.engine(series);
    let ps = engine.position(&account);
    let pos = ps.position.0;
    let avg_tenths = ps.avg_entry().map(i64::from).unwrap_or(0);
    let unrealized_tenths = match engine.mark_price() {
        Some(mark) if pos != 0 => pos * (i64::from(mark.tenths()) - avg_tenths),
        _ => 0,
    };
    AccountView {
        cash: tenths(acct.usdc_balance),
        position_size: Decimal::from(pos),
        position_avg_cost: Decimal::new(avg_tenths, 3),
        realized_pnl: tenths(ps.realized_pnl_this_market),
        realized_pnl_lifetime: tenths(Tenths(acct.lifetime_realized_pnl_tenths + ps.realized_pnl_this_market.0)),
        unrealized_pnl: tenths(Tenths(unrealized_tenths)),
    }
}

/// `Tenths` (1/10 cent) as dollars.
fn tenths(t: Tenths) -> Decimal {
    Decimal::new(t.0, 3)
}

/// Engine event → runtime event. Prices stay in the quoted (`up`) frame;
/// the runtime complements `down` orders itself.
pub fn convert(ev: engine::ExecutionEvent) -> ExecutionEvent {
    let kind = match ev.kind {
        ExecKind::Fill => ExecutionKind::Fill,
        ExecKind::Partial => ExecutionKind::Partial,
        ExecKind::Reject(r) => ExecutionKind::Reject { reason: translate::reject_reason_of(r), detail: format!("{r:?}") },
        ExecKind::CancelAck => ExecutionKind::CancelAck,
        ExecKind::Expiry => ExecutionKind::Expiry,
        ExecKind::Replaced => ExecutionKind::Replaced,
    };
    ExecutionEvent {
        series: ev.series,
        account: ev.account,
        client_order_id: ev.client_order_id,
        venue_order_id: ev.venue_order_id.map(|id| id.0),
        kind,
        filled_qty: Decimal::from(ev.filled_qty.0),
        remaining_qty: Decimal::from(ev.remaining_qty.0),
        price: ev.price.map(tick_to_decimal),
        market_id: ev.market.map(|m| m.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::automation::runtime::{RuntimeCommand, runtime_channel};
    use crate::series::SERIES;
    use crate::ticks::PriceTick;
    use crate::types::{Qty, Side};
    use strategy_compiler::ir::{OrderType, Side as IrSide, Tif};

    const BOT: Address = Address([0xb0; 20]);
    const STRAT: Address = Address([0x51; 20]);

    fn order(outcome: &str, side: IrSide, order_type: OrderType, size: &str, price: Option<&str>) -> VenueOrder {
        VenueOrder {
            client_order_id: Arc::from("c1"),
            instrument_id: "a.current".into(),
            outcome: Some(outcome.into()),
            side,
            order_type,
            tif: if order_type == OrderType::Limit { Tif::Gtc } else { Tif::Ioc },
            size: size.parse().unwrap(),
            price: price.map(|p| p.parse().unwrap()),
            post_only: false,
            expire_after_ms: None,
            reduce_only: false,
        }
    }

    fn rig() -> (Arc<Mutex<Exchange>>, Arc<EngineVenue>, tokio::sync::mpsc::UnboundedReceiver<RuntimeCommand>) {
        let exchange = Arc::new(Mutex::new(Exchange::new(Tenths(1_000_000), SERIES)));
        {
            let mut x = exchange.lock().unwrap();
            x.get_or_create_account(BOT);
            x.get_or_create_account(STRAT);
            x.open_first_market(SeriesId(0), 1.0, 0, 120);
        }
        let (runtime, rx) = runtime_channel();
        EngineVenue::install(&exchange, runtime);
        let venue = EngineVenue::new(exchange.clone(), SeriesMap::new([("a", SeriesId(0)), ("b", SeriesId(1))]));
        (exchange, venue, rx)
    }

    fn events(rx: &mut tokio::sync::mpsc::UnboundedReceiver<RuntimeCommand>) -> Vec<ExecutionEvent> {
        let mut out = Vec::new();
        while let Ok(cmd) = rx.try_recv() {
            if let RuntimeCommand::Execution(ev) = cmd {
                out.push(ev);
            }
        }
        out
    }

    #[test]
    fn submit_translates_and_reports_in_the_quoted_frame() {
        let (exchange, venue, mut rx) = rig();
        // A bot bid at 40c on the YES book; the strategy sells `down` at 0.6 = buys YES at 0.4.
        exchange.lock().unwrap().place_limit(SeriesId(0), BOT, Side::Ask, PriceTick::from_tenths(400).unwrap(), Qty(3), TradeSource::Bot).unwrap();
        let acc = venue.submit(SeriesId(0), STRAT, &order("down", IrSide::Sell, OrderType::Limit, "5", Some("0.6"))).unwrap();
        assert_eq!(acc.market_id, Some(1));
        let evs = events(&mut rx);
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].kind, ExecutionKind::Partial);
        assert_eq!(evs[0].filled_qty, Decimal::from(3));
        assert_eq!(evs[0].remaining_qty, Decimal::from(2));
        assert_eq!(evs[0].price, Some("0.4".parse().unwrap()), "quoted frame; the runtime complements");
        assert_eq!(evs[0].venue_order_id, Some(acc.venue_order_id));
        let view = venue.account_view(SeriesId(0), STRAT);
        assert_eq!(view.position_size, Decimal::from(3));
        assert_eq!(view.position_avg_cost, "0.4".parse().unwrap());
        assert_eq!(view.cash, Decimal::from(1000) - "1.2".parse::<Decimal>().unwrap() - "0.8".parse::<Decimal>().unwrap(), "3 filled at 0.4 plus 2 locked at 0.4");
        venue.cancel(SeriesId(0), STRAT, acc.venue_order_id).unwrap();
        assert_eq!(events(&mut rx)[0].kind, ExecutionKind::CancelAck);
        assert_eq!(venue.account_view(SeriesId(0), STRAT).cash, Decimal::from(1000) - "1.2".parse::<Decimal>().unwrap());
    }

    #[test]
    fn closed_or_foreign_markets_reject_locally() {
        let (_exchange, venue, mut rx) = rig();
        let mut o = order("up", IrSide::Buy, OrderType::Limit, "1", Some("0.5"));
        o.instrument_id = "b.current".into();
        assert_eq!(venue.submit(SeriesId(1), STRAT, &o), Err(RejectReason::MarketClosed), "series 1 has no window");
        assert_eq!(venue.submit(SeriesId(0), STRAT, &o), Err(RejectReason::InvalidOrder), "instrument outside the home series");
        assert!(events(&mut rx).is_empty(), "local rejects never reach the engine");
    }

    #[test]
    fn engine_rejects_are_reported_and_returned() {
        let (_exchange, venue, mut rx) = rig();
        let err = venue.submit(SeriesId(0), STRAT, &order("up", IrSide::Buy, OrderType::Market, "1", None)).unwrap_err();
        assert_eq!(err, RejectReason::NoLiquidity);
        let evs = events(&mut rx);
        assert!(matches!(evs[0].kind, ExecutionKind::Reject { reason: RejectReason::NoLiquidity, .. }));
    }

    #[test]
    fn cancel_replace_moves_the_order() {
        let (_exchange, venue, mut rx) = rig();
        let a = venue.submit(SeriesId(0), STRAT, &order("up", IrSide::Buy, OrderType::Limit, "2", Some("0.5"))).unwrap();
        let mut o2 = order("up", IrSide::Buy, OrderType::Limit, "2", Some("0.52"));
        o2.client_order_id = Arc::from("c2");
        let b = venue.cancel_replace(SeriesId(0), STRAT, a.venue_order_id, &o2).unwrap();
        assert_ne!(a.venue_order_id, b.venue_order_id);
        let evs = events(&mut rx);
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].kind, ExecutionKind::Replaced);
        assert_eq!(evs[0].client_order_id.as_ref(), "c1");
        assert_eq!(venue.cancel_replace(SeriesId(0), STRAT, a.venue_order_id, &o2), Err(RejectReason::InvalidOrder), "old id is gone");
    }

    #[test]
    fn unrealized_marks_the_position_at_mid() {
        let (exchange, venue, _rx) = rig();
        {
            let mut x = exchange.lock().unwrap();
            x.place_limit(SeriesId(0), BOT, Side::Ask, PriceTick::from_tenths(400).unwrap(), Qty(2), TradeSource::Bot).unwrap();
        }
        venue.submit(SeriesId(0), STRAT, &order("up", IrSide::Buy, OrderType::Market, "2", None)).unwrap();
        {
            let mut x = exchange.lock().unwrap();
            x.place_limit(SeriesId(0), BOT, Side::Bid, PriceTick::from_tenths(500).unwrap(), Qty(1), TradeSource::Bot).unwrap();
            x.place_limit(SeriesId(0), BOT, Side::Ask, PriceTick::from_tenths(600).unwrap(), Qty(1), TradeSource::Bot).unwrap();
        }
        let view = venue.account_view(SeriesId(0), STRAT);
        assert_eq!(view.position_size, Decimal::from(2));
        // Long 2 at 0.40, mid 0.55: +0.30.
        assert_eq!(view.unrealized_pnl, "0.3".parse().unwrap());
        assert_eq!(view.realized_pnl, Decimal::ZERO);
    }
}
