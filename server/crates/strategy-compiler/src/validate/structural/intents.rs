//! Order-shape rules: price/band presence by order type, cancel-replace order type,
//! band orientation, and the sparse-in on-table warnings.

use crate::ir::{DoItem, OnDest, OnTable, OrderType, PriceBand, Strategy, Tif};

use crate::validate::decimal::cmp_decimal;
use crate::validate::diag::{DiagCode, DiagPath, Diagnostic};

/// Run the order-shape rules over every intent.
pub fn check(strategy: &Strategy, diagnostics: &mut Vec<Diagnostic>) {
    for (l, leg) in strategy.legs.iter().enumerate() {
        for (b, block) in leg.body.iter().enumerate() {
            for (d, item) in block.do_items.iter().enumerate() {
                let base = DiagPath::root()
                    .key("legs")
                    .index(l)
                    .key("body")
                    .index(b)
                    .key("do")
                    .index(d);
                match item {
                    DoItem::Place(place_item) => {
                        let place = &place_item.place;
                        let path = base.key("place");
                        place_shape(place, path.clone(), diagnostics);
                        flag_shape(
                            place.order_type,
                            place.tif,
                            place.post_only,
                            place.expire_after.is_some(),
                            path.clone(),
                            diagnostics,
                        );
                        if let Some(band) = &place.price_band {
                            band_orientation(band, path.clone().key("price_band"), diagnostics);
                        }
                        if let Some(on) = &place.on {
                            on_table_style(on, path.key("on"), diagnostics);
                        }
                    }
                    DoItem::CancelReplace(replace_item) => {
                        let replace = &replace_item.cancel_replace;
                        let path = base.key("cancel_replace");
                        // The type requires price and band; only the order type and
                        // the band's orientation can go wrong.
                        if replace.order_type != OrderType::Limit {
                            diagnostics.push(
                                Diagnostic::rule(
                                    DiagCode::CancelReplaceNotLimit,
                                    path.clone().key("order_type"),
                                    "a cancel-replace places a limit order — market replaces \
                                     are not supported",
                                )
                                .with_help("set order_type to `limit`"),
                            );
                        }
                        flag_shape(
                            replace.order_type,
                            replace.tif,
                            replace.post_only,
                            replace.expire_after.is_some(),
                            path.clone(),
                            diagnostics,
                        );
                        band_orientation(&replace.price_band, path.key("price_band"), diagnostics);
                    }
                    DoItem::TargetPosition(target_item) => {
                        if let Some(on) = &target_item.target_position.on {
                            on_table_style(on, base.key("target_position").key("on"), diagnostics);
                        }
                    }
                    DoItem::Statement(_) | DoItem::Cancel(_) => {}
                }
            }
        }
    }
}

fn place_shape(place: &crate::ir::Place, path: DiagPath, diagnostics: &mut Vec<Diagnostic>) {
    match place.order_type {
        OrderType::Limit => {
            if place.price.is_none() || place.price_band.is_none() {
                let missing = match (&place.price, &place.price_band) {
                    (None, None) => "`price` and `price_band`",
                    (None, Some(_)) => "`price`",
                    (Some(_), None) => "`price_band`",
                    (Some(_), Some(_)) => return,
                };
                diagnostics.push(
                    Diagnostic::rule(
                        DiagCode::LimitRequiresPrice,
                        path,
                        format!("a limit order requires {missing}"),
                    )
                    .with_help(
                        "limit orders carry a clamp-wrapped price expression and a \
                         [lo, hi] price band",
                    ),
                );
            }
        }
        OrderType::Market => {
            if place.price.is_some() || place.price_band.is_some() {
                let present = match (&place.price, &place.price_band) {
                    (Some(_), Some(_)) => "`price` and `price_band`",
                    (Some(_), None) => "`price`",
                    (None, Some(_)) => "`price_band`",
                    (None, None) => return,
                };
                diagnostics.push(
                    Diagnostic::rule(
                        DiagCode::MarketForbidsPrice,
                        path,
                        format!("a market order carries no price — remove {present}"),
                    )
                    .with_help(
                        "a slippage-capped take is the limit-ify pattern: a limit order \
                         at a clamped price with tif: ioc",
                    ),
                );
            }
        }
    }
}

/// `post_only` only means something for a limit order; `expire_after` only for an
/// order that rests.
fn flag_shape(
    order_type: OrderType,
    tif: Tif,
    post_only: Option<bool>,
    expires: bool,
    path: DiagPath,
    diagnostics: &mut Vec<Diagnostic>,
) {
    if post_only == Some(true) && order_type == OrderType::Market {
        diagnostics.push(
            Diagnostic::rule(
                DiagCode::OrderFlagMisuse,
                path.clone().key("post_only"),
                "post_only applies to limit orders — a market order always takes",
            )
            .with_help("drop post_only, or make the order a limit"),
        );
    }
    if expires && tif == Tif::Ioc {
        diagnostics.push(
            Diagnostic::rule(
                DiagCode::OrderFlagMisuse,
                path.key("expire_after"),
                "expire_after applies to resting orders — an IOC order never rests",
            )
            .with_help("drop expire_after, or use tif: gtc"),
        );
    }
}

fn band_orientation(band: &PriceBand, path: DiagPath, diagnostics: &mut Vec<Diagnostic>) {
    if cmp_decimal(band.lo().as_str(), band.hi().as_str()) == std::cmp::Ordering::Greater {
        diagnostics.push(
            Diagnostic::rule(
                DiagCode::BandInverted,
                path,
                format!(
                    "price band [{}, {}] is inverted — the low bound exceeds the high",
                    band.lo().as_str(),
                    band.hi().as_str()
                ),
            )
            .with_help("write the band as [lo, hi]"),
        );
    }
}

/// Sparse-in warnings: an empty on table, and explicit `stay` destinations.
fn on_table_style(on: &OnTable, path: DiagPath, diagnostics: &mut Vec<Diagnostic>) {
    let keys: [(&'static str, &Option<OnDest>); 5] = [
        ("fill", &on.fill),
        ("partial", &on.partial),
        ("reject", &on.reject),
        ("cancel_ack", &on.cancel_ack),
        ("expiry", &on.expiry),
    ];
    if keys.iter().all(|(_, dest)| dest.is_none()) {
        diagnostics.push(
            Diagnostic::rule(
                DiagCode::EmptyOnTable,
                path,
                "this on table has no keys — omit it (sparse-in: never write a default)",
            )
            .with_help("absent keys already default to stay"),
        );
        return;
    }
    for (key, dest) in keys {
        if matches!(dest, Some(OnDest::Stay)) {
            diagnostics.push(
                Diagnostic::rule(
                    DiagCode::ExplicitStay,
                    path.clone().key(key),
                    format!("`{key}: stay` is the default — omit it (sparse-in)"),
                )
                .with_help("write only the keys that transition"),
            );
        }
    }
}
