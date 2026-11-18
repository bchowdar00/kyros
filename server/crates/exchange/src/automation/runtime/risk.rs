//! Envelope risk checks, in `Decimal`, before every strategy order reaches the
//! venue. A failed check is a synthesized `Reject { envelope_violation }` so
//! the intent's `on.reject` transition applies uniformly.
//!
//! The engine's own cash lock remains the final authority
//! (`insufficient_funds`); these checks enforce the strategy's declared
//! envelope, which is usually tighter.

use rust_decimal::Decimal;
use strategy_compiler::ir::{Envelope, Side};

use crate::automation::value::parse_decimal;
use crate::automation::venue::{AccountView, RejectReason, VenueOrder};

/// What the risk gate needs to know about the deployment's live orders.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OpenOrders {
    /// Count of live resting orders.
    pub resting: u64,
    /// Σ size × worst-case cost over live resting orders (quoted frame).
    pub resting_notional: Decimal,
    /// Σ signed size over live resting orders (+ buys the quoted outcome).
    pub resting_signed_size: Decimal,
    /// Σ opening cost over live resting orders: the part of each order that
    /// would extend the position rather than close it, at worst-case cost.
    pub resting_opening_notional: Decimal,
    /// The signed position left to close once every resting order that
    /// reduces it has been netted off, in the quoted frame.
    pub closable: Decimal,
}

impl OpenOrders {
    /// Summarize live resting orders against the account's signed position.
    /// Reducing orders consume the position in iteration order; what is left
    /// is `closable`, which the next order may still close for free.
    pub fn summarize<'a>(position: Decimal, resting: impl IntoIterator<Item = &'a VenueOrder>) -> OpenOrders {
        let mut open = OpenOrders { closable: position, ..OpenOrders::default() };
        for order in resting {
            open.resting += 1;
            open.resting_notional += worst_case_cost(order);
            open.resting_signed_size += signed_size(order);
            open.resting_opening_notional += opening_cost(order, &mut open.closable);
        }
        open
    }
}

fn dec(s: &str) -> Decimal {
    parse_decimal(s).unwrap_or(Decimal::ZERO)
}

/// Signed size in the quoted frame: buying the quoted outcome or selling
/// its complement is long.
pub fn signed_size(order: &VenueOrder) -> Decimal {
    let quoted = crate::series::is_quoted_outcome(order.outcome.as_deref());
    let long = match order.side {
        Side::Buy => quoted,
        Side::Sell => !quoted,
    };
    if long { order.size } else { -order.size }
}

/// Worst-case cash one contract of the order can consume on a binary: the
/// price for a buy, `1 - price` for a sell (the short's collateral); market
/// orders assume the worst price of 1.
pub fn unit_cost(order: &VenueOrder) -> Decimal {
    let Some(price) = order.price else {
        // Market order: assume the worst price of 1 either way.
        return Decimal::ONE;
    };
    let price = price.clamp(Decimal::ZERO, Decimal::ONE);
    match order.side {
        Side::Buy => price,
        Side::Sell => Decimal::ONE - price,
    }
}

/// Worst-case cash the whole order can consume: `size × unit_cost`.
pub fn worst_case_cost(order: &VenueOrder) -> Decimal {
    order.size * unit_cost(order)
}

/// The cash the order commits beyond what it closes. `closable` is the signed
/// position (quoted frame) the order may still reduce; the reducing portion is
/// netted off it and costs nothing, because closing releases collateral. The
/// rest is charged at worst case.
pub fn opening_cost(order: &VenueOrder, closable: &mut Decimal) -> Decimal {
    let signed = signed_size(order);
    let reduces = !closable.is_zero() && signed.is_sign_positive() != closable.is_sign_positive();
    let closes = if reduces { signed.abs().min(closable.abs()) } else { Decimal::ZERO };
    if reduces {
        // Move `closable` toward zero by the closed quantity.
        *closable += if signed.is_sign_positive() { closes } else { -closes };
    }
    (order.size - closes).max(Decimal::ZERO) * unit_cost(order)
}

/// Collateral held under the open position: the entry price per long
/// contract, its complement per short contract (`avg_cost` is quoted-frame).
pub fn open_cost(view: &AccountView) -> Decimal {
    let avg = view.position_avg_cost.clamp(Decimal::ZERO, Decimal::ONE);
    if view.position_size.is_sign_negative() {
        view.position_size.abs() * (Decimal::ONE - avg)
    } else {
        view.position_size * avg
    }
}

/// Run every envelope check. `Ok(())` lets the order through.
pub fn check(envelope: &Envelope, view: &AccountView, open: &OpenOrders, order: &VenueOrder) -> Result<(), (RejectReason, String)> {
    let violation = |detail: String| Err((RejectReason::EnvelopeViolation, detail));

    let max_order = dec(envelope.max_order_size.as_str());
    if order.size > max_order {
        return violation(format!("size {} exceeds max_order_size {max_order}", order.size));
    }

    if let Some(cap) = envelope.max_open_orders.as_ref().map(|d| dec(d.as_str()))
        && order.may_rest()
        && Decimal::from(open.resting) >= cap
    {
        return violation(format!("{} resting orders reach max_open_orders {cap}", open.resting));
    }

    if let Some(cap) = envelope.max_position_per_market.as_ref().map(|d| dec(d.as_str())) {
        let projected = view.position_size + open.resting_signed_size + signed_size(order);
        if projected.abs() > cap {
            return violation(format!("projected position {projected} exceeds max_position_per_market {cap}"));
        }
    }

    // The round budget: what the strategy believes it has for this window.
    // Reducing orders are free (they release collateral); the round's
    // realized loss, the open position, the resting opening notional and
    // this order's opening cost may not exceed it. Position, resting orders
    // and round PnL all reset at settlement, so the cap re-arms.
    let open_cost = open_cost(view);
    if let Some(round_budget) = envelope.round_budget.as_ref().map(|d| dec(d.as_str())) {
        let round_loss = (-view.realized_pnl).max(Decimal::ZERO);
        let mut closable = open.closable;
        let this_opening = opening_cost(order, &mut closable);
        let committed = round_loss + open_cost + open.resting_opening_notional + this_opening;
        // An order that only closes never adds commitment, so it passes even
        // when the position already fills the budget: a strategy can always
        // sell out.
        if !this_opening.is_zero() && committed > round_budget {
            return violation(format!("round commitment {committed} exceeds round_budget {round_budget}"));
        }
    }

    let loss_budget = dec(envelope.loss_budget.as_str());
    let realized_loss = (-view.realized_pnl_lifetime).max(Decimal::ZERO);
    // Closing lots are not new exposure: the lots that close the position
    // release collateral rather than commit it, so a sell-out or the closing
    // half of a flip is not charged; only the lots past flat are, and a
    // pure close passes even when the open position already fills the budget.
    let this_cost = {
        let mut closable = open.closable;
        opening_cost(order, &mut closable)
    };
    let worst = realized_loss + open_cost + open.resting_notional + this_cost;
    if !this_cost.is_zero() && worst > loss_budget {
        return violation(format!("worst-case exposure {worst} exceeds loss_budget {loss_budget}"));
    }

    if let Some(fraction) = envelope.max_exposure_fraction.as_ref().map(|d| dec(d.as_str())) {
        let remaining = (loss_budget - realized_loss).max(Decimal::ZERO);
        let in_flight = open.resting_notional + this_cost;
        if in_flight > remaining * fraction {
            return violation(format!("in-flight exposure {in_flight} exceeds {fraction} of remaining budget {remaining}"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use strategy_compiler::ir::{OrderType, Tif};

    fn envelope(json: &str) -> Envelope {
        serde_json::from_str(json).unwrap()
    }

    fn order(side: Side, outcome: &str, size: &str, price: Option<&str>) -> VenueOrder {
        VenueOrder {
            client_order_id: Arc::from("c"),
            instrument_id: "s.current".into(),
            outcome: Some(outcome.into()),
            side,
            order_type: if price.is_some() { OrderType::Limit } else { OrderType::Market },
            tif: if price.is_some() { Tif::Gtc } else { Tif::Ioc },
            size: size.parse().unwrap(),
            price: price.map(|p| p.parse().unwrap()),
            post_only: false,
            expire_after_ms: None,
            reduce_only: false,
        }
    }

    fn d(s: &str) -> Decimal {
        s.parse().unwrap()
    }

    #[test]
    fn signed_size_follows_outcome_frame() {
        assert_eq!(signed_size(&order(Side::Buy, "up", "10", None)), d("10"));
        assert_eq!(signed_size(&order(Side::Sell, "up", "10", None)), d("-10"));
        assert_eq!(signed_size(&order(Side::Buy, "down", "10", None)), d("-10"));
        assert_eq!(signed_size(&order(Side::Sell, "down", "10", None)), d("10"));
    }

    #[test]
    fn worst_case_cost_is_binary_collateral() {
        assert_eq!(worst_case_cost(&order(Side::Buy, "up", "10", Some("0.4"))), d("4"));
        assert_eq!(worst_case_cost(&order(Side::Sell, "up", "10", Some("0.4"))), d("6"));
        assert_eq!(worst_case_cost(&order(Side::Buy, "up", "10", None)), d("10"));
        assert_eq!(worst_case_cost(&order(Side::Sell, "up", "10", None)), d("10"));
    }

    #[test]
    fn opening_cost_nets_the_closed_portion_off_the_position() {
        // Flat: everything opens.
        let mut closable = Decimal::ZERO;
        assert_eq!(opening_cost(&order(Side::Buy, "up", "10", Some("0.4")), &mut closable), d("4"));
        assert_eq!(closable, Decimal::ZERO);
        // Long 10: a sell of 6 closes 6 for free, 4 are left to close.
        let mut closable = d("10");
        assert_eq!(opening_cost(&order(Side::Sell, "up", "6", Some("0.4")), &mut closable), Decimal::ZERO);
        assert_eq!(closable, d("4"));
        // Then a sell of 10 closes the last 4 and opens a 6 short at 0.6 each.
        assert_eq!(opening_cost(&order(Side::Sell, "up", "10", Some("0.4")), &mut closable), d("3.6"));
        assert_eq!(closable, Decimal::ZERO);
        // Buying `down` is a sell of `up`: it closes a long too.
        let mut closable = d("5");
        assert_eq!(opening_cost(&order(Side::Buy, "down", "5", None), &mut closable), Decimal::ZERO);
        assert_eq!(closable, Decimal::ZERO);
        // A market buy that extends a long pays the worst price of 1.
        let mut closable = d("5");
        assert_eq!(opening_cost(&order(Side::Buy, "up", "3", None), &mut closable), d("3"));
        assert_eq!(closable, d("5"));
        // Short 4: a buy of 10 closes 4 and opens 6 at 0.5.
        let mut closable = d("-4");
        assert_eq!(opening_cost(&order(Side::Buy, "up", "10", Some("0.5")), &mut closable), d("3"));
        assert_eq!(closable, Decimal::ZERO);
    }

    #[test]
    fn open_cost_is_entry_for_longs_and_complement_for_shorts() {
        assert_eq!(open_cost(&AccountView { position_size: d("10"), position_avg_cost: d("0.3"), ..Default::default() }), d("3"));
        assert_eq!(open_cost(&AccountView { position_size: d("-10"), position_avg_cost: d("0.3"), ..Default::default() }), d("7"));
        assert_eq!(open_cost(&AccountView::default()), Decimal::ZERO);
    }

    #[test]
    fn summarize_nets_resting_reducers_before_charging_openers() {
        let sell = order(Side::Sell, "up", "6", Some("0.7"));
        let buy = order(Side::Buy, "up", "5", Some("0.4"));
        let open = OpenOrders::summarize(d("10"), [&sell, &buy]);
        assert_eq!(open.resting, 2);
        assert_eq!(open.resting_signed_size, d("-1"));
        // Worst case still counts the sell's short collateral (6 × 0.3).
        assert_eq!(open.resting_notional, d("3.8"));
        // Opening notional: the sell closes for free, the buy opens 5 × 0.4.
        assert_eq!(open.resting_opening_notional, d("2"));
        assert_eq!(open.closable, d("4"));
    }

    #[test]
    fn round_budget_caps_this_windows_commitment_and_frees_closes() {
        // A $10 round budget under a generous allocation and lifetime budget.
        let env = envelope(r#"{"allocation": "1000", "loss_budget": "1000", "round_budget": "10", "max_order_size": "100", "max_position_per_market": "100"}"#);
        let none = OpenOrders::default();
        let flat = AccountView::default();
        assert!(check(&env, &flat, &none, &order(Side::Buy, "up", "20", Some("0.5"))).is_ok(), "exactly the budget fits");
        let over = check(&env, &flat, &none, &order(Side::Buy, "up", "21", Some("0.5"))).unwrap_err();
        assert!(over.1.contains("round_budget"), "{}", over.1);
        // Holding $10 of position: another buy is refused, a closing sell is free.
        let full = AccountView { position_size: d("20"), position_avg_cost: d("0.5"), ..Default::default() };
        let full_open = OpenOrders::summarize(full.position_size, []);
        assert!(check(&env, &full, &full_open, &order(Side::Buy, "up", "1", Some("0.5"))).unwrap_err().1.contains("round_budget"));
        assert!(check(&env, &full, &full_open, &order(Side::Sell, "up", "20", None)).is_ok(), "market flatten is free");
        assert!(check(&env, &full, &full_open, &order(Side::Sell, "up", "21", Some("0.5"))).unwrap_err().1.contains("round_budget"), "the extra contract opens a short");
        // This round's realized loss counts against the budget; a gain does not extend it.
        let bled = AccountView { realized_pnl: d("-6"), ..Default::default() };
        assert!(check(&env, &bled, &none, &order(Side::Buy, "up", "8", Some("0.5"))).is_ok());
        assert!(check(&env, &bled, &none, &order(Side::Buy, "up", "9", Some("0.5"))).unwrap_err().1.contains("round_budget"));
        let won = AccountView { realized_pnl: d("6"), ..Default::default() };
        assert!(check(&env, &won, &none, &order(Side::Buy, "up", "21", Some("0.5"))).unwrap_err().1.contains("round_budget"));
        // A resting bid's opening notional is committed too.
        let resting = OpenOrders::summarize(Decimal::ZERO, [&order(Side::Buy, "up", "10", Some("0.5"))]);
        assert!(check(&env, &flat, &resting, &order(Side::Buy, "up", "10", Some("0.5"))).is_ok());
        assert!(check(&env, &flat, &resting, &order(Side::Buy, "up", "11", Some("0.5"))).unwrap_err().1.contains("round_budget"));
    }

    #[test]
    fn checks_in_order() {
        let env = envelope(r#"{"allocation": "1000", "loss_budget": "500", "max_order_size": "100", "max_position_per_market": "200", "max_open_orders": "2", "max_exposure_fraction": "0.5"}"#);
        let view = AccountView::default();
        let none = OpenOrders::default();
        assert!(check(&env, &view, &none, &order(Side::Buy, "up", "100", Some("0.5"))).is_ok());
        assert!(check(&env, &view, &none, &order(Side::Buy, "up", "101", Some("0.5"))).unwrap_err().1.contains("max_order_size"));
        let full = OpenOrders { resting: 2, ..Default::default() };
        assert!(check(&env, &view, &full, &order(Side::Buy, "up", "10", Some("0.5"))).unwrap_err().1.contains("max_open_orders"));
        assert!(check(&env, &view, &full, &order(Side::Buy, "up", "10", None)).is_ok(), "market orders do not rest");
        let big = AccountView { position_size: d("150"), ..Default::default() };
        assert!(check(&env, &big, &none, &order(Side::Buy, "up", "60", None)).unwrap_err().1.contains("max_position_per_market"));
        assert!(check(&env, &big, &none, &order(Side::Sell, "up", "60", None)).is_ok(), "reducing is fine");
        let bleeding = AccountView { realized_pnl_lifetime: d("-450"), ..Default::default() };
        assert!(check(&env, &bleeding, &none, &order(Side::Buy, "up", "100", Some("0.6"))).unwrap_err().1.contains("loss_budget"));
        // 100 × 0.4 = 40 fits the budget (450 + 40 <= 500) but not half the remaining 50.
        assert!(check(&env, &bleeding, &none, &order(Side::Buy, "up", "100", Some("0.4"))).unwrap_err().1.contains("in-flight"));
        assert!(check(&env, &bleeding, &none, &order(Side::Buy, "up", "50", Some("0.4"))).is_ok());
    }

    /// A budget that the open position already fills must still let the
    /// strategy sell out, and a flip is charged only for the lots past flat.
    #[test]
    fn closing_lots_are_not_new_exposure() {
        let env = envelope(r#"{"allocation": "25", "loss_budget": "25", "max_order_size": "200"}"#);
        // Short 28 Down bought at 0.70 (position -28 in the quoted frame,
        // average cost 0.30 on the Up side): 19.6 of collateral held, plus
        // 16 lost, so the position alone already exceeds the budget.
        let view = AccountView { position_size: d("-28"), position_avg_cost: d("0.3"), realized_pnl_lifetime: d("-16"), ..Default::default() };
        // No resting orders: the whole position is still closable, which is
        // what the runtime hands the gate.
        let none = OpenOrders::summarize(view.position_size, std::iter::empty());
        // Selling out: buy 28 Up at 0.41. Before, 28 × 0.41 = 11.48 was
        // charged on top of 35.6 and refused.
        let exit = order(Side::Buy, "up", "28", Some("0.41"));
        assert_eq!(opening_cost(&exit, &mut view.position_size.clone()), Decimal::ZERO);
        assert!(check(&env, &view, &none, &exit).is_ok());
        // A flip of 28 + 2: only the two opening lots are charged, and they
        // do not fit a budget the position already fills.
        let flip = order(Side::Buy, "up", "30", Some("0.41"));
        assert_eq!(opening_cost(&flip, &mut view.position_size.clone()), d("0.82"));
        assert!(check(&env, &view, &none, &flip).unwrap_err().1.contains("loss_budget"));
        // Adding to the short is fully charged and refused.
        let add = order(Side::Buy, "down", "10", Some("0.7"));
        assert_eq!(opening_cost(&add, &mut view.position_size.clone()), d("7"));
        assert!(check(&env, &view, &none, &add).unwrap_err().1.contains("loss_budget"));
    }
}
