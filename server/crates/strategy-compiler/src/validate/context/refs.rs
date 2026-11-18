//! Position rules for `table.*`, window slots, the reducers, `push`, and `entry.*`.
//!
//! A table is not a value: it appears only as `lookup`'s first argument. A window is
//! not a value either: it appears only as a reducer's first argument or a `push`
//! target, and a reducer takes nothing else there. `entry.value` is the element under
//! `count_where`'s predicate and means nothing outside one; the predicate itself is a
//! pure function of the element — no signal, state, event, platform or fill reads,
//! no nested reducers, no `triggered()`.

use crate::ir::{StateSlot, Strategy};

use crate::parse::ast::expr::{Expr, Namespace};
use crate::parse::ast::stmt::Stmt;
use crate::parse::builtins::Builtin;
use crate::parse::span::{Span, Spanned};
use crate::parse::walk::{ParsedString, WalkedString};
use crate::validate::diag::{DiagCode, DiagPath, Diagnostic};
use crate::validate::tables::Tables;
use crate::validate::visit::{walk_expr, walk_stmt};

/// Run the position rules over every parsed string.
pub fn check(strategy: &Strategy, tables: &Tables, diagnostics: &mut Vec<Diagnostic>) {
    for walked in &tables.asts {
        // Legal positions, collected first so the flagging pass can test membership
        // and containment by span (spans are unique per node within one string).
        let mut legal_table_spans: Vec<Span> = Vec::new();
        let mut window_arg_spans: Vec<Span> = Vec::new();
        let mut predicate_spans: Vec<Span> = Vec::new();
        let mut collect = |node: &Spanned<Expr>| {
            if let Expr::Call { builtin, args, .. } = &node.node {
                if *builtin == Builtin::Lookup
                    && let Some(first) = args.first()
                    && matches!(&first.node, Expr::Ref(r) if r.namespace == Namespace::Table)
                {
                    legal_table_spans.push(first.span);
                }
                if is_reducer(*builtin)
                    && let Some(first) = args.first()
                {
                    window_arg_spans.push(first.span);
                }
                if *builtin == Builtin::CountWhere
                    && let Some(predicate) = args.get(1)
                {
                    predicate_spans.push(predicate.span);
                }
            }
        };
        match &walked.parsed {
            ParsedString::Expr(expr) => walk_expr(expr, &mut collect),
            ParsedString::Stmt(stmt) => walk_stmt(stmt, &mut collect),
        }

        let path = DiagPath::from_walker(&walked.path);
        let is_window = |slot: &str| {
            tables
                .state
                .get(slot)
                .is_some_and(|&i| matches!(strategy.state[i], StateSlot::Window { .. }))
        };
        let inside = |spans: &[Span], span: Span| {
            spans
                .iter()
                .any(|s| s.start <= span.start && span.end <= s.end)
        };

        // `push` targets a window; `set` never does (checked before the closure below
        // takes its mutable borrow of the diagnostic list).
        if let ParsedString::Stmt(stmt) = &walked.parsed {
            match &stmt.node {
                Stmt::Push { slot, .. }
                    if tables.state.contains_key(slot.node.as_ref())
                        && !is_window(&slot.node) =>
                {
                    diagnostics.push(
                        misuse(&path, walked, slot.span, format!(
                            "`push` appends to a window — state slot `{}` is not one",
                            slot.node
                        ))
                        .with_help("declare the slot with kind: window, or use `set`"),
                    );
                }
                Stmt::Set { slot, .. } if is_window(&slot.node) => {
                    diagnostics.push(
                        misuse(&path, walked, slot.span, format!(
                            "state slot `{}` is a window — it is appended with `push`, never set",
                            slot.node
                        ))
                        .with_help("write `push state.<window> = <value>`"),
                    );
                }
                Stmt::Push { .. } | Stmt::Set { .. } => {}
            }
        }

        let mut flag = |node: &Spanned<Expr>| {
            let in_predicate = inside(&predicate_spans, node.span);
            match &node.node {
                Expr::Ref(r) if r.namespace == Namespace::Table => {
                    if !legal_table_spans.contains(&node.span) {
                        diagnostics.push(
                            Diagnostic::rule(
                                DiagCode::TableOutsideLookup,
                                path.clone(),
                                "a table is not a value — tables are consumed only via \
                                 lookup(table.NAME, key)",
                            )
                            .with_source(walked.source.clone(), node.span)
                            .with_help("wrap the reference: lookup(table.NAME, <key expression>)"),
                        );
                    }
                }
                Expr::Ref(r) if r.namespace == Namespace::Entry => {
                    if !in_predicate {
                        diagnostics.push(
                            Diagnostic::rule(
                                DiagCode::EntryOutsideReducer,
                                path.clone(),
                                "entry.value is the element under a count_where predicate and \
                                 means nothing outside one",
                            )
                            .with_source(walked.source.clone(), node.span),
                        );
                    } else if !matches!(r.path.as_slice(), [only] if only.node.as_ref() == "value")
                        || r.index.is_some()
                    {
                        diagnostics.push(
                            Diagnostic::rule(
                                DiagCode::EntryOutsideReducer,
                                path.clone(),
                                "the element under a reducer has one field: entry.value",
                            )
                            .with_source(walked.source.clone(), node.span),
                        );
                    }
                }
                Expr::Ref(r) if r.namespace == Namespace::State => {
                    let Some(head) = r.path.first() else { return };
                    if is_window(&head.node) && !window_arg_spans.contains(&node.span) {
                        diagnostics.push(
                            misuse(&path, walked, node.span, format!(
                                "state slot `{}` is a window, not a value",
                                head.node
                            ))
                            .with_help(
                                "reduce it: count(state.w), sum(state.w), max_of, min_of, \
                                 oldest, newest, count_where(state.w, entry.value ...)",
                            ),
                        );
                    } else if in_predicate {
                        diagnostics.push(predicate_impure(&path, walked, node.span, "state"));
                    }
                }
                Expr::Ref(r) if in_predicate => {
                    match r.namespace {
                        Namespace::Param | Namespace::Envelope => {}
                        Namespace::Signal
                        | Namespace::Event
                        | Namespace::Platform
                        | Namespace::Legs
                        | Namespace::Expr => diagnostics.push(predicate_impure(
                            &path,
                            walked,
                            node.span,
                            r.namespace.as_str(),
                        )),
                        // Table and entry handled above; state handled above.
                        Namespace::Table | Namespace::Entry | Namespace::State => {}
                    }
                }
                Expr::Call { builtin, args, name_span } if is_reducer(*builtin) => {
                    if in_predicate {
                        diagnostics.push(predicate_impure(&path, walked, *name_span, "a reducer"));
                    }
                    let window_ok = args.first().is_some_and(|first| {
                        matches!(&first.node, Expr::Ref(r)
                            if r.namespace == Namespace::State
                                && r.index.is_none()
                                && r.path.len() == 1
                                && (is_window(&r.path[0].node)
                                    || !tables.state.contains_key(r.path[0].node.as_ref())))
                    });
                    if !window_ok {
                        let span = args.first().map_or(*name_span, |a| a.span);
                        diagnostics.push(
                            misuse(&path, walked, span, "a reducer's first argument is a window slot".to_string())
                                .with_help("write count(state.<window>), sum(state.<window>), ..."),
                        );
                    }
                }
                Expr::Triggered { .. } if in_predicate => {
                    diagnostics.push(predicate_impure(&path, walked, node.span, "triggered()"));
                }
                Expr::Ref(_)
                | Expr::Literal(_)
                | Expr::Unary { .. }
                | Expr::Arith { .. }
                | Expr::Cmp { .. }
                | Expr::Logic { .. }
                | Expr::If { .. }
                | Expr::Call { .. }
                | Expr::Triggered { .. } => {}
            }
        };
        match &walked.parsed {
            ParsedString::Expr(expr) => walk_expr(expr, &mut flag),
            ParsedString::Stmt(stmt) => walk_stmt(stmt, &mut flag),
        }
    }
}

fn misuse(path: &DiagPath, walked: &WalkedString, span: Span, message: String) -> Diagnostic {
    Diagnostic::rule(DiagCode::WindowMisuse, path.clone(), message)
        .with_source(walked.source.clone(), span)
}

fn predicate_impure(path: &DiagPath, walked: &WalkedString, span: Span, what: &str) -> Diagnostic {
    misuse(
        path,
        walked,
        span,
        format!("{what} is not allowed inside a count_where predicate"),
    )
    .with_help(
        "the predicate is a pure function of entry.value (plus params, envelope constants \
         and literals); compute anything else outside the reducer",
    )
}

/// The reducer family of builtins.
pub(crate) fn is_reducer(builtin: Builtin) -> bool {
    matches!(
        builtin,
        Builtin::Count
            | Builtin::Sum
            | Builtin::MaxOf
            | Builtin::MinOf
            | Builtin::Oldest
            | Builtin::Newest
            | Builtin::CountWhere
    )
}
