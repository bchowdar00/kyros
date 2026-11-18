//! `Program` order vocabulary → engine vocabulary (plan §3.5, "Translation").
//!
//! The book is a single canonical YES (`up`) book. A `down` order is the
//! complementary YES order: buying `down` at `p` is selling `up` at `1 - p`.
//!
//! | Program | Engine |
//! |---|---|
//! | `up`, buy | `Side::Bid` at `p` |
//! | `up`, sell | `Side::Ask` at `p` |
//! | `down`, buy | `Side::Ask` at `1 - p` |
//! | `down`, sell | `Side::Bid` at `1 - p` |
//!
//! Prices are exact decimals snapped to the nearest tick of the 153-tick
//! grid (the grid is symmetric, so the complement of a snapped `down`
//! price is itself a tick). Sizes are floored to whole contracts; zero
//! after flooring is a local reject. Engine errors map onto the closed
//! [`RejectReason`] set.

use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use strategy_compiler::ir::{OrderType, Side as IrSide, Tif};

use super::{RejectReason, VenueOrder};
use crate::series::is_quoted_outcome;
use crate::engine::{ExecReject, OrderError, PlaceOpts};
use crate::ticks::{PriceTick, TICKS};
use crate::types::{Qty, Side};

/// An order in engine terms, ready for `place_*_with`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineOrder {
    pub side: Side,
    /// `None` for market orders.
    pub price: Option<PriceTick>,
    pub qty: Qty,
    pub opts: PlaceOpts,
}

/// Whether the order is on its series' quoted outcome (`up`, `home`).
/// `None` outcome (a non-outcome instrument) is treated as quoted.
pub fn is_quoted(order: &VenueOrder) -> bool {
    is_quoted_outcome(order.outcome.as_deref())
}

/// Engine side of the order on the YES book.
pub fn side(order: &VenueOrder) -> Side {
    match (order.side, is_quoted(order)) {
        (IrSide::Buy, true) | (IrSide::Sell, false) => Side::Bid,
        (IrSide::Sell, true) | (IrSide::Buy, false) => Side::Ask,
    }
}

/// The price in the quoted frame: complemented for the other outcome.
pub fn quoted_price(order: &VenueOrder) -> Option<Decimal> {
    let p = order.price?;
    Some(if is_quoted(order) { p } else { Decimal::ONE - p })
}

/// Snap a probability to the nearest tick. `None` outside `(0, 1)`. Ties
/// go to the lower tick.
pub fn snap_price(p: Decimal) -> Option<PriceTick> {
    if p <= Decimal::ZERO || p >= Decimal::ONE {
        return None;
    }
    let tenths = (p * Decimal::from(1000)).to_f64()?;
    let mut best: Option<(f64, u16)> = None;
    for &t in TICKS.iter() {
        let d = (t as f64 - tenths).abs();
        match best {
            Some((bd, _)) if d >= bd => {}
            _ => best = Some((d, t)),
        }
    }
    PriceTick::from_tenths(best?.1)
}

/// Floor a decimal size to whole contracts.
pub fn floor_size(size: Decimal) -> Qty {
    if size <= Decimal::ZERO {
        return Qty::ZERO;
    }
    Qty(size.floor().to_u32().unwrap_or(u32::MAX))
}

/// Translate one runtime order; the outcome label decides its frame
/// (`series::is_quoted_outcome`). `Err` is a local reject that never
/// reaches the engine.
pub fn to_engine(order: &VenueOrder) -> Result<EngineOrder, RejectReason> {
    let qty = floor_size(order.size);
    if qty.is_zero() {
        return Err(RejectReason::InvalidOrder);
    }
    let price = match order.order_type {
        OrderType::Market => None,
        OrderType::Limit => {
            let p = quoted_price(order).ok_or(RejectReason::InvalidOrder)?;
            Some(snap_price(p).ok_or(RejectReason::PriceBand)?)
        }
    };
    let opts = PlaceOpts {
        client_order_id: Some(order.client_order_id.clone()),
        ioc: order.order_type == OrderType::Limit && order.tif == Tif::Ioc,
        post_only: order.post_only,
        reduce_only: order.reduce_only,
    };
    Ok(EngineOrder { side: side(order), price, qty, opts })
}

/// The closed reject vocabulary for engine refusals.
pub fn reject_reason(e: OrderError) -> RejectReason {
    reject_reason_of(e.into())
}

pub fn reject_reason_of(e: ExecReject) -> RejectReason {
    match e {
        ExecReject::InsufficientBalance => RejectReason::InsufficientFunds,
        ExecReject::NoLiquidity => RejectReason::NoLiquidity,
        ExecReject::ZeroQuantity
        | ExecReject::SeriesMismatch
        | ExecReject::PostOnlyWouldCross
        | ExecReject::ReduceOnlyNoPosition => RejectReason::InvalidOrder,
        ExecReject::UnknownAccount => RejectReason::InternalError,
        ExecReject::MarketClosed => RejectReason::MarketClosed,
    }
}

/// A tick as an exact probability.
pub fn tick_to_decimal(t: PriceTick) -> Decimal {
    Decimal::new(t.tenths() as i64, 3)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn order(outcome: &str, side: IrSide, order_type: OrderType, tif: Tif, size: &str, price: Option<&str>) -> VenueOrder {
        VenueOrder {
            client_order_id: Arc::from("c"),
            instrument_id: "s.current".into(),
            outcome: Some(outcome.into()),
            side,
            order_type,
            tif,
            size: size.parse().unwrap(),
            price: price.map(|p| p.parse().unwrap()),
            post_only: false,
            expire_after_ms: None,
            reduce_only: false,
        }
    }

    fn tick(t: u16) -> PriceTick {
        PriceTick::from_tenths(t).unwrap()
    }

    #[test]
    fn outcome_and_side_map_onto_the_yes_book() {
        let lim = |o, s| order(o, s, OrderType::Limit, Tif::Gtc, "10", Some("0.3"));
        let up_buy = to_engine(&lim("up", IrSide::Buy)).unwrap();
        assert_eq!((up_buy.side, up_buy.price), (Side::Bid, Some(tick(300))));
        let up_sell = to_engine(&lim("up", IrSide::Sell)).unwrap();
        assert_eq!((up_sell.side, up_sell.price), (Side::Ask, Some(tick(300))));
        let down_buy = to_engine(&lim("down", IrSide::Buy)).unwrap();
        assert_eq!((down_buy.side, down_buy.price), (Side::Ask, Some(tick(700))));
        let down_sell = to_engine(&lim("down", IrSide::Sell)).unwrap();
        assert_eq!((down_sell.side, down_sell.price), (Side::Bid, Some(tick(700))));
        assert_eq!(up_buy.opts.client_order_id.as_deref(), Some("c"));
    }

    #[test]
    fn prices_snap_to_the_nearest_tick() {
        assert_eq!(snap_price("0.5".parse().unwrap()), Some(tick(500)));
        assert_eq!(snap_price("0.504".parse().unwrap()), Some(tick(500)), "1c grid in the middle");
        assert_eq!(snap_price("0.506".parse().unwrap()), Some(tick(510)));
        assert_eq!(snap_price("0.505".parse().unwrap()), Some(tick(500)), "ties go down");
        assert_eq!(snap_price("0.0123".parse().unwrap()), Some(tick(12)), "0.1c grid at the bottom");
        assert_eq!(snap_price("0.9876".parse().unwrap()), Some(tick(988)));
        assert_eq!(snap_price("0.035".parse().unwrap()), Some(tick(30)), "gap between 3c and 4c");
        assert_eq!(snap_price("0.0004".parse().unwrap()), Some(tick(1)));
        assert_eq!(snap_price("0".parse().unwrap()), None);
        assert_eq!(snap_price("1".parse().unwrap()), None);
        // Down complement of a snapped price is a tick too.
        let d = order("down", IrSide::Buy, OrderType::Limit, Tif::Gtc, "1", Some("0.0123"));
        assert_eq!(to_engine(&d).unwrap().price, Some(tick(988)));
    }

    #[test]
    fn sizes_floor_and_zero_is_a_local_reject() {
        assert_eq!(floor_size("10.9".parse().unwrap()), Qty(10));
        assert_eq!(floor_size("0.9".parse().unwrap()), Qty::ZERO);
        let tiny = order("up", IrSide::Buy, OrderType::Market, Tif::Ioc, "0.5", None);
        assert_eq!(to_engine(&tiny), Err(RejectReason::InvalidOrder));
        let out = order("up", IrSide::Buy, OrderType::Limit, Tif::Gtc, "1", Some("1.2"));
        assert_eq!(to_engine(&out), Err(RejectReason::PriceBand));
    }

    #[test]
    fn tif_and_flags_reach_the_opts() {
        let mut o = order("up", IrSide::Buy, OrderType::Limit, Tif::Ioc, "1", Some("0.5"));
        o.post_only = true;
        o.reduce_only = true;
        let e = to_engine(&o).unwrap();
        assert!(e.opts.ioc && e.opts.post_only && e.opts.reduce_only);
        let m = to_engine(&order("up", IrSide::Buy, OrderType::Market, Tif::Ioc, "1", None)).unwrap();
        assert!(!m.opts.ioc, "market orders are IOC by nature; the flag is for limits");
        assert_eq!(m.price, None);
    }

    #[test]
    fn engine_errors_map_onto_the_closed_set() {
        assert_eq!(reject_reason(OrderError::NoLiquidity), RejectReason::NoLiquidity);
        assert_eq!(reject_reason(OrderError::MarketClosed), RejectReason::MarketClosed);
        assert_eq!(
            reject_reason(OrderError::InsufficientBalance { required: crate::types::Tenths(1), available: crate::types::Tenths(0) }),
            RejectReason::InsufficientFunds
        );
        assert_eq!(reject_reason(OrderError::PostOnlyWouldCross), RejectReason::InvalidOrder);
        assert_eq!(reject_reason(OrderError::UnknownAccount), RejectReason::InternalError);
        assert_eq!(tick_to_decimal(tick(650)), "0.65".parse().unwrap());
    }
}
