//! Closed-set namespaces: `envelope.*`, `platform.*`, and `legs.*` reads.
//!
//! The platform path set is [`PlatformRead::from_path`] (the runtime's closed list);
//! the `legs.*` shape is `legs.<leg>.<field>` or `legs.<leg>.tag.<tag>.<field>` with
//! `<field>` in [`LegFillField`], `<leg>` a declared leg (family A resolves it) and
//! `<tag>` a tag some order placed from that leg carries.

use crate::parse::ast::expr::{Expr, Namespace, Ref};
use crate::parse::span::Spanned;
use crate::parse::walk::WalkedString;
use crate::program::{LegFillField, PlatformRead};
use crate::validate::diag::{DiagCode, DiagPath, Diagnostic};
use crate::validate::tables::Tables;

use crate::validate::visit::walk_string;

/// The six readable envelope fields, as an owned closed set — recorded per program
/// into the manifest's envelope-read set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum EnvelopeField {
    /// `envelope.allocation`
    Allocation,
    /// `envelope.loss_budget`
    LossBudget,
    /// `envelope.round_budget`
    RoundBudget,
    /// `envelope.max_order_size`
    MaxOrderSize,
    /// `envelope.max_position_per_market`
    MaxPositionPerMarket,
    /// `envelope.max_open_orders`
    MaxOpenOrders,
}

impl EnvelopeField {
    /// Resolve a readable field from its spelling.
    pub fn from_name(name: &str) -> Option<EnvelopeField> {
        match name {
            "allocation" => Some(EnvelopeField::Allocation),
            "loss_budget" => Some(EnvelopeField::LossBudget),
            "round_budget" => Some(EnvelopeField::RoundBudget),
            "max_order_size" => Some(EnvelopeField::MaxOrderSize),
            "max_position_per_market" => Some(EnvelopeField::MaxPositionPerMarket),
            "max_open_orders" => Some(EnvelopeField::MaxOpenOrders),
            _ => None,
        }
    }

    /// The source spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            EnvelopeField::Allocation => "allocation",
            EnvelopeField::LossBudget => "loss_budget",
            EnvelopeField::RoundBudget => "round_budget",
            EnvelopeField::MaxOrderSize => "max_order_size",
            EnvelopeField::MaxPositionPerMarket => "max_position_per_market",
            EnvelopeField::MaxOpenOrders => "max_open_orders",
        }
    }
}

/// The gate parameters: present on the envelope, never readable — branching on your
/// own circuit breaker is a validation error.
const GATE_PARAMETERS: [&str; 2] = ["max_exposure_fraction", "pause_at_consumed"];

/// Run the closed-set namespace rules over every parsed string.
pub fn check(tables: &Tables, diagnostics: &mut Vec<Diagnostic>) {
    for walked in &tables.asts {
        walk_string(walked, &mut |node| {
            let Expr::Ref(r) = &node.node else { return };
            match r.namespace {
                Namespace::Envelope => envelope(node, r, walked, diagnostics),
                Namespace::Platform => platform(node, r, walked, diagnostics),
                Namespace::Legs => legs_read(node, r, walked, tables, diagnostics),
                Namespace::Event
                | Namespace::Signal
                | Namespace::State
                | Namespace::Expr
                | Namespace::Table
                | Namespace::Entry
                | Namespace::Param => {}
            }
        });
    }
}

fn envelope(
    node: &Spanned<Expr>,
    r: &Ref,
    walked: &WalkedString,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let path = DiagPath::from_walker(&walked.path);
    let [field] = r.path.as_slice() else {
        diagnostics.push(
            Diagnostic::rule(
                DiagCode::UnknownEnvelopeField,
                path,
                "an envelope read is `envelope.<field>`, one field deep",
            )
            .with_source(walked.source.clone(), node.span),
        );
        return;
    };
    if EnvelopeField::from_name(&field.node).is_some() {
        return;
    }
    if GATE_PARAMETERS.contains(&field.node.as_ref()) {
        diagnostics.push(
            Diagnostic::rule(
                DiagCode::EnvelopeUnreadable,
                path,
                format!(
                    "`{}` is a gate parameter — branching on your own circuit breaker \
                     is not allowed",
                    field.node
                ),
            )
            .with_source(walked.source.clone(), field.span),
        );
    } else {
        diagnostics.push(
            Diagnostic::rule(
                DiagCode::UnknownEnvelopeField,
                path,
                format!("the envelope has no readable field `{}`", field.node),
            )
            .with_source(walked.source.clone(), field.span)
            .with_help(
                "readable fields: allocation, loss_budget, round_budget, max_order_size, \
                 max_position_per_market, max_open_orders",
            ),
        );
    }
}

fn platform(
    node: &Spanned<Expr>,
    r: &Ref,
    walked: &WalkedString,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let spelled: Vec<&str> = r.path.iter().map(|s| s.node.as_ref()).collect();
    if PlatformRead::from_path(&spelled).is_none() {
        diagnostics.push(
            Diagnostic::rule(
                DiagCode::UnknownPlatformPath,
                DiagPath::from_walker(&walked.path),
                format!("`platform.{}` is not a platform read", spelled.join(".")),
            )
            .with_source(walked.source.clone(), node.span)
            .with_help(format!("supported reads: {}", PlatformRead::PATHS.join(", "))),
        );
    }
}

fn legs_read(
    node: &Spanned<Expr>,
    r: &Ref,
    walked: &WalkedString,
    tables: &Tables,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let path = DiagPath::from_walker(&walked.path);
    let shape_help = format!(
        "a leg read is `legs.<leg>.<field>` or `legs.<leg>.tag.<tag>.<field>`; fields: {}",
        LegFillField::NAMES.join(", ")
    );
    // The leg name itself resolved (or errored) in family A; unknown legs skip the
    // tag check rather than cascading.
    let (leg, tag, field) = match r.path.as_slice() {
        [leg, field] => (leg, None, field),
        [leg, kw, tag, field] if kw.node.as_ref() == "tag" => (leg, Some(tag), field),
        _ => {
            diagnostics.push(
                Diagnostic::rule(DiagCode::LegsReadInvalid, path, "malformed leg read")
                    .with_source(walked.source.clone(), node.span)
                    .with_help(shape_help),
            );
            return;
        }
    };
    if LegFillField::from_name(&field.node).is_none() {
        diagnostics.push(
            Diagnostic::rule(
                DiagCode::LegsReadInvalid,
                path.clone(),
                format!("`{}` is not a leg fill field", field.node),
            )
            .with_source(walked.source.clone(), field.span)
            .with_help(shape_help),
        );
    }
    if let Some(tag) = tag
        && let Some(entry) = tables.legs.get(leg.node.as_ref())
        && !entry.tags.contains(tag.node.as_ref())
    {
        let declared: Vec<&str> = entry.tags.iter().map(String::as_str).collect();
        diagnostics.push(
            Diagnostic::rule(
                DiagCode::LegsUnknownTag,
                path,
                format!("leg `{}` places no order tagged `{}`", leg.node, tag.node),
            )
            .with_source(walked.source.clone(), tag.span)
            .with_help(if declared.is_empty() {
                format!("leg `{}` places no orders", leg.node)
            } else {
                format!("tags placed from `{}`: {}", leg.node, declared.join(", "))
            }),
        );
    }
}
