//! Intent lowering: order intents with resolved references, plus the intent index —
//! `intent_id` -> totalized transition table and resolved bounds.
//!
//! `intent_id` is `"<leg>.<block>.<do-index>"`, the exact path vocabulary of validator
//! diagnostics. It is an opaque key into the index plus provenance for logs; the
//! runner never parses it for semantics. Cancel-by-tag intents carry NO id and no
//! index entry (python-target-spec §5.2): they have no transition table and no bounds.

use std::collections::BTreeMap;

use crate::ir::{OnDest, OnTable, OrderType, Place, Side, TargetPosition, Tif};

use crate::parse::ast::expr::{Expr, Literal, Namespace, Ref};
use crate::parse::span::Spanned;
use crate::validate::ValidStrategy;
use crate::validate::context::EnvelopeField;

/// A lowered place/cancel-replace order body. Price and size are SUBSTITUTED ASTs
/// (`expr.NAME` references inlined), ready for the program emitter.
#[derive(Debug, Clone, PartialEq)]
pub struct OrderLower {
    /// The resolved venue instrument id.
    pub instrument_id: String,
    /// The outcome label, present iff the market is outcome-structured.
    pub outcome: Option<String>,
    /// Buy or sell.
    pub side: Side,
    /// Market or limit.
    pub order_type: OrderType,
    /// Time in force.
    pub tif: Tif,
    /// The logical order tag.
    pub tag: String,
    /// The size expression, raw AST.
    pub size: Spanned<Expr>,
    /// The price expression, raw AST; present iff `order_type` is limit.
    pub price: Option<Spanned<Expr>>,
    /// Rest or reject, never take.
    pub post_only: bool,
    /// Good-till-time, milliseconds from placement.
    pub expire_after_ms: Option<u64>,
    /// May only shrink the position.
    pub reduce_only: bool,
}

/// One lowered intent, in do-list position.
#[derive(Debug, Clone, PartialEq)]
pub enum IntentLower {
    /// A place order, with its index key.
    Place {
        /// `"<leg>.<block>.<do-index>"`.
        intent_id: String,
        /// The order body.
        order: OrderLower,
    },
    /// Cancel all live orders carrying the tag. No id, no index entry.
    Cancel {
        /// The targeted tag.
        tag: String,
    },
    /// Cancel the tag's live orders and place the replacement.
    CancelReplace {
        /// `"<leg>.<block>.<do-index>"`.
        intent_id: String,
        /// The replacement order body (carries the same tag).
        order: OrderLower,
    },
    /// Reach a target position with one market IOC delta order (or nothing). The
    /// order's `size` is the TARGET signed position; `side` is `buy` as a placeholder —
    /// the runtime picks the side from the delta's sign.
    TargetPosition {
        /// `"<leg>.<block>.<do-index>"`.
        intent_id: String,
        /// The target and the delta order's shape.
        order: OrderLower,
    },
}

/// The five-key totalized transition table. Every key present, absent authored keys
/// filled with `stay` — sparse-in, total-out; the decompilation and the runner both
/// see the whole alphabet.
#[derive(Debug, Clone, PartialEq)]
pub struct OnTotal {
    /// Destination on full fill.
    pub fill: String,
    /// Destination on partial fill.
    pub partial: String,
    /// Destination on venue rejection.
    pub reject: String,
    /// Destination on cancel acknowledgement.
    pub cancel_ack: String,
    /// Destination on expiry.
    pub expiry: String,
}

/// One intent index entry: what the runner snapshots onto the order at placement.
///
/// `max_size` is POST-RESOLUTION: the canonical decimal value, not the authored
/// expression — the authored expression lives only in the source document. The
/// magnitude rule guarantees it is envelope-derived, so resolution is total.
#[derive(Debug, Clone, PartialEq)]
pub struct IntentEntry {
    /// The totalized transition table.
    pub on: OnTotal,
    /// The resolved per-intent size bound, canonical decimal.
    pub max_size: String,
}

/// Lower one place intent.
pub fn lower_place(
    valid: &ValidStrategy,
    intent_id: String,
    place: &Place,
    ast_base: &str,
) -> (IntentLower, IntentEntry) {
    let order = OrderLower {
        instrument_id: instrument_id(valid, place.instrument.as_str()),
        outcome: place.outcome.as_ref().map(|o| o.as_str().to_string()),
        side: place.side,
        order_type: place.order_type,
        tif: place.tif,
        tag: place.tag.as_str().to_string(),
        size: expr_ast(valid, &format!("{ast_base}.size")),
        price: place
            .price
            .as_ref()
            .map(|_| expr_ast(valid, &format!("{ast_base}.price"))),
        post_only: place.post_only == Some(true),
        expire_after_ms: place.expire_after.as_ref().map(|d| d.as_millis()),
        reduce_only: place.reduce_only == Some(true),
    };
    let entry = IntentEntry {
        on: totalize(place.on.as_ref()),
        max_size: resolve_max_size(valid, &format!("{ast_base}.max_size")),
    };
    (
        IntentLower::Place {
            intent_id: intent_id.clone(),
            order,
        },
        entry,
    )
}

/// Lower one cancel-replace intent. The IR type carries no on-table, so its index
/// entry totalizes to all-stay.
pub fn lower_cancel_replace(
    valid: &ValidStrategy,
    intent_id: String,
    replace: &crate::ir::CancelReplace,
    ast_base: &str,
) -> (IntentLower, IntentEntry) {
    let order = OrderLower {
        instrument_id: instrument_id(valid, replace.instrument.as_str()),
        outcome: Some(replace.outcome.as_str().to_string()),
        side: replace.side,
        order_type: replace.order_type,
        tif: replace.tif,
        tag: replace.tag.as_str().to_string(),
        size: expr_ast(valid, &format!("{ast_base}.size")),
        price: Some(expr_ast(valid, &format!("{ast_base}.price"))),
        post_only: replace.post_only == Some(true),
        expire_after_ms: replace.expire_after.as_ref().map(|d| d.as_millis()),
        reduce_only: replace.reduce_only == Some(true),
    };
    let entry = IntentEntry {
        on: totalize(None),
        max_size: resolve_max_size(valid, &format!("{ast_base}.max_size")),
    };
    (
        IntentLower::CancelReplace {
            intent_id: intent_id.clone(),
            order,
        },
        entry,
    )
}

/// Lower one target-position intent: a market IOC delta order whose `size` is the
/// target.
pub fn lower_target_position(
    valid: &ValidStrategy,
    intent_id: String,
    target: &TargetPosition,
    ast_base: &str,
) -> (IntentLower, IntentEntry) {
    let order = OrderLower {
        instrument_id: instrument_id(valid, target.instrument.as_str()),
        outcome: target.outcome.as_ref().map(|o| o.as_str().to_string()),
        side: Side::Buy,
        order_type: OrderType::Market,
        tif: Tif::Ioc,
        tag: target.tag.as_str().to_string(),
        size: expr_ast(valid, &format!("{ast_base}.size")),
        price: None,
        post_only: false,
        expire_after_ms: None,
        reduce_only: target.reduce_only == Some(true),
    };
    let entry = IntentEntry {
        on: totalize(target.on.as_ref()),
        max_size: resolve_max_size(valid, &format!("{ast_base}.max_size")),
    };
    (
        IntentLower::TargetPosition {
            intent_id: intent_id.clone(),
            order,
        },
        entry,
    )
}

/// The intent-index map type: `intent_id` -> entry, lexicographic (BTreeMap — the
/// order reaches the manifest).
pub type IntentIndex = BTreeMap<String, IntentEntry>;

/// Totalize an authored on-table over the full five-key alphabet, absent -> `stay`.
fn totalize(on: Option<&OnTable>) -> OnTotal {
    let dest = |d: &Option<OnDest>| match d {
        None | Some(OnDest::Stay) => "stay".to_string(),
        Some(OnDest::Leg(leg)) => leg.as_str().to_string(),
    };
    match on {
        Some(table) => OnTotal {
            fill: dest(&table.fill),
            partial: dest(&table.partial),
            reject: dest(&table.reject),
            cancel_ack: dest(&table.cancel_ack),
            expiry: dest(&table.expiry),
        },
        None => OnTotal {
            fill: "stay".to_string(),
            partial: "stay".to_string(),
            reject: "stay".to_string(),
            cancel_ack: "stay".to_string(),
            expiry: "stay".to_string(),
        },
    }
}

/// The SUBSTITUTED AST at a walker path. Total on validated documents.
fn expr_ast(valid: &ValidStrategy, path: &str) -> Spanned<Expr> {
    let walked = valid
        .tables()
        .ast_at(path)
        .expect("validated documents have every order expression parsed");
    match &walked.parsed {
        crate::parse::walk::ParsedString::Expr(ast) => super::blocks::substituted(valid, ast),
        crate::parse::walk::ParsedString::Stmt(_) => {
            unreachable!("an order-expression path never parses as a statement")
        }
    }
}

/// Resolve a `max_size` expression to its canonical decimal value, through the
/// substitution environment.
///
/// The shapes the magnitude rule plus the corpus ratify: a (possibly
/// named-expression-wrapped) bare `envelope.*` read, or a bare decimal literal.
/// Arithmetic over envelope reads is FIXTURE-STARVED — exact decimal arithmetic joins
/// this function with its first fixture; until then an arithmetic `max_size` is a
/// compiler invariant failure, not a silent mislowering.
fn resolve_max_size(valid: &ValidStrategy, path: &str) -> String {
    let substituted = expr_ast(valid, path);
    match &substituted.node {
        Expr::Ref(Ref {
            namespace: Namespace::Envelope,
            path: fields,
            ..
        }) => {
            let field = fields
                .first()
                .and_then(|f| EnvelopeField::from_name(&f.node))
                .expect("the validator accepts only readable envelope fields");
            envelope_field_value(valid, field)
        }
        Expr::Literal(Literal::Decimal(d)) => d.to_string(),
        _ => unreachable!(
            "max_size resolution beyond an envelope read or decimal literal is fixture-starved"
        ),
    }
}

/// The decimal value of a readable envelope field, from the submission envelope.
fn envelope_field_value(valid: &ValidStrategy, field: EnvelopeField) -> String {
    let envelope = valid.envelope();
    let value = match field {
        EnvelopeField::Allocation => Some(&envelope.allocation),
        EnvelopeField::LossBudget => Some(&envelope.loss_budget),
        EnvelopeField::RoundBudget => envelope.round_budget.as_ref(),
        EnvelopeField::MaxOrderSize => Some(&envelope.max_order_size),
        EnvelopeField::MaxPositionPerMarket => envelope.max_position_per_market.as_ref(),
        EnvelopeField::MaxOpenOrders => envelope.max_open_orders.as_ref(),
    };
    value
        .expect("validator guarantees read envelope fields are present")
        .as_str()
        .to_string()
}

/// Resolve a declared instrument nickname to its venue id. Total on validated
/// documents.
fn instrument_id(valid: &ValidStrategy, nick: &str) -> String {
    valid
        .strategy()
        .instruments
        .iter()
        .find(|(name, _)| name.as_str() == nick)
        .map(|(_, id)| id.clone())
        .expect("the validator resolves every intent instrument nickname")
}
