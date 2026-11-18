//! `triggered()` placement and the `event.*` pin rule.
//!
//! `triggered()` is legal only in guard position (ir-spec §11.2); `event.*` is
//! well-typed only in a block whose guard pins exactly one single-signal condition
//! trigger, and its fields are that signal's (§11.3). Named expressions are
//! document-level formulas, so `event.*` is forbidden there outright (ruled
//! 2026-07-20, Q6): substitution would make their validity depend on the use site.

use crate::ir::{DoItem, Registry, Strategy};

use crate::parse::ast::expr::{Expr, Namespace};
use crate::parse::span::Spanned;
use crate::parse::walk::{StringRole, WalkedString};
use crate::validate::diag::{DiagCode, DiagPath, Diagnostic};
use crate::validate::tables::Tables;
use crate::validate::visit::walk_string;

use super::pin::{PinStatus, Pins, UnpinReason};

/// Run the placement rules.
pub fn check(
    strategy: &Strategy,
    registry: &Registry,
    tables: &Tables,
    pins: &Pins,
    diagnostics: &mut Vec<Diagnostic>,
) {
    triggered_placement(tables, diagnostics);
    event_in_named_exprs(tables, diagnostics);
    event_pins(strategy, registry, tables, pins, diagnostics);
}

/// `triggered()` anywhere outside a guard string.
fn triggered_placement(tables: &Tables, diagnostics: &mut Vec<Diagnostic>) {
    for walked in &tables.asts {
        if walked.role == StringRole::Guard {
            continue;
        }
        walk_string(walked, &mut |node| {
            if let Expr::Triggered { name } = &node.node {
                diagnostics.push(
                    Diagnostic::rule(
                        DiagCode::TriggeredOutsideGuard,
                        DiagPath::from_walker(&walked.path),
                        "triggered() is legal only in a block's `if` guard",
                    )
                    .with_source(walked.source.clone(), node.span)
                    .with_help(format!(
                        "move the `triggered({})` test into the block's `if`",
                        name.node
                    )),
                );
            }
        });
    }
}

/// `event.*` inside a named expression body.
fn event_in_named_exprs(tables: &Tables, diagnostics: &mut Vec<Diagnostic>) {
    for walked in &tables.asts {
        if walked.role != StringRole::NamedExpr {
            continue;
        }
        walk_string(walked, &mut |node| {
            if let Expr::Ref(r) = &node.node
                && r.namespace == Namespace::Event
            {
                diagnostics.push(
                    Diagnostic::rule(
                        DiagCode::EventInNamedExpr,
                        DiagPath::from_walker(&walked.path),
                        "named expressions cannot read event.* — they substitute into \
                         arbitrary blocks, and the triggering event differs per block",
                    )
                    .with_source(walked.source.clone(), node.span)
                    .with_help("read the event field directly in the block that pins the trigger"),
                );
            }
        });
    }
}

/// The pin rule: every `event.*` read in every block-scoped string.
fn event_pins(
    strategy: &Strategy,
    registry: &Registry,
    tables: &Tables,
    pins: &Pins,
    diagnostics: &mut Vec<Diagnostic>,
) {
    for (l, leg) in strategy.legs.iter().enumerate() {
        for (b, block) in leg.body.iter().enumerate() {
            let pin = pins.at(l, b);
            // A failed guard means the pin is unknowable: stay quiet (the parse error
            // is already reported) rather than calling every event.* read unpinned.
            if *pin == PinStatus::GuardFailed {
                continue;
            }
            for path in block_string_paths(l, b, block) {
                let Some(walked) = tables.ast_at(&path) else {
                    continue;
                };
                walk_string(walked, &mut |node| {
                    let Expr::Ref(r) = &node.node else { return };
                    if r.namespace != Namespace::Event {
                        return;
                    }
                    check_event_read(node, r, walked, pin, registry, tables, diagnostics);
                });
            }
        }
    }
}

fn check_event_read(
    node: &Spanned<Expr>,
    r: &crate::parse::ast::expr::Ref,
    walked: &WalkedString,
    pin: &PinStatus,
    registry: &Registry,
    tables: &Tables,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let signal = match pin {
        PinStatus::Pinned { signal } => signal,
        PinStatus::GuardFailed => return,
        PinStatus::Unpinned(reason) => {
            let why = match reason {
                // Family A already reported the unknown trigger; the pin is unknowable.
                UnpinReason::UnknownTrigger => return,
                UnpinReason::NoTriggered => {
                    "the block's guard pins no trigger — add exactly one triggered(NAME)"
                }
                UnpinReason::MultipleTriggered => {
                    "the block's guard pins more than one trigger, so the firing event is ambiguous"
                }
                UnpinReason::Timer => {
                    "the pinned trigger is a timer — timers carry no event payload"
                }
                UnpinReason::MultiSignal => {
                    "the pinned trigger spans several signals, so the firing event is ambiguous"
                }
            };
            diagnostics.push(
                Diagnostic::rule(
                    DiagCode::EventUnpinned,
                    DiagPath::from_walker(&walked.path),
                    format!("event.* is not available here: {why}"),
                )
                .with_source(walked.source.clone(), node.span)
                .with_help("read signal.* snapshots instead, or restructure the guard"),
            );
            return;
        }
    };

    // Pinned: the field must exist on the pinned signal, one segment deep.
    let Some(fields) = crate::validate::registry::signal_fields(signal, registry, tables) else {
        return; // Unresolved signal id — root cause already reported.
    };
    match r.path.as_slice() {
        [field] => {
            if !fields
                .iter()
                .any(|f| f.name.as_str() == field.node.as_ref())
            {
                let declared: Vec<&str> = fields.iter().map(|f| f.name.as_str()).collect();
                diagnostics.push(
                    Diagnostic::rule(
                        DiagCode::UnknownField,
                        DiagPath::from_walker(&walked.path),
                        format!(
                            "signal `{signal}` (the pinned trigger's signal) has no field `{}`",
                            field.node
                        ),
                    )
                    .with_source(walked.source.clone(), field.span)
                    .with_help(format!("declared fields: {}", declared.join(", "))),
                );
            }
        }
        [_, extra, ..] => {
            diagnostics.push(
                Diagnostic::rule(
                    DiagCode::UnknownField,
                    DiagPath::from_walker(&walked.path),
                    format!(
                        "event fields are flat — `{}` reads into nothing",
                        extra.node
                    ),
                )
                .with_source(walked.source.clone(), extra.span),
            );
        }
        [] => {}
    }
}

/// The walker paths of every expression string a block owns.
pub(super) fn block_string_paths(l: usize, b: usize, block: &crate::ir::Block) -> Vec<String> {
    let base = format!("legs[{l}].body[{b}]");
    let mut paths = Vec::with_capacity(1 + block.do_items.len() * 3);
    if block.guard.is_some() {
        paths.push(format!("{base}.if"));
    }
    for (d, item) in block.do_items.iter().enumerate() {
        match item {
            DoItem::Statement(_) => paths.push(format!("{base}.do[{d}]")),
            DoItem::Place(_) => {
                for field in ["price", "size", "max_size"] {
                    paths.push(format!("{base}.do[{d}].place.{field}"));
                }
            }
            DoItem::CancelReplace(_) => {
                for field in ["price", "size", "max_size"] {
                    paths.push(format!("{base}.do[{d}].cancel_replace.{field}"));
                }
            }
            DoItem::TargetPosition(_) => {
                for field in ["size", "max_size"] {
                    paths.push(format!("{base}.do[{d}].target_position.{field}"));
                }
            }
            DoItem::Cancel(_) => {}
        }
    }
    paths
}
