//! The named-expression graph (P2): cycle detection, poison propagation, and the
//! memoized substitution environment the clamp family consumes.
//!
//! Order of operations is fixed (design §1.6): edges from parsed bodies; Tarjan for
//! cycles (ONE `expr-cycle` diagnostic per cycle, members poisoned); transitive poison
//! propagation (parse-failed bodies, cycle members, and everything referencing them —
//! dependents get NO diagnostic, their root cause is already reported); then a
//! memoized inline of every clean name in dependency order. Memoization is what keeps
//! a DAG with shared subtrees from exploding exponentially on substitution.

use std::collections::{BTreeMap, BTreeSet};

use crate::parse::ast::expr::{Expr, IfArm, Namespace, Ref};
use crate::parse::span::Spanned;
use crate::parse::walk::ParsedString;
use crate::validate::diag::{DiagCode, DiagPath, Diagnostic};
use crate::validate::poison::Poisons;
use crate::validate::tables::Tables;
use crate::validate::visit::walk_expr;

/// The substitution environment: fully-inlined ASTs for every clean named expression.
#[derive(Debug, Clone, PartialEq)]
pub struct Subst {
    memo: BTreeMap<String, Spanned<Expr>>,
}

impl Subst {
    /// Build the environment. Emits `expr-cycle` diagnostics and extends
    /// `poisons.poisoned_exprs` with every unusable name.
    pub fn build(
        tables: &Tables,
        poisons: &mut Poisons,
        diagnostics: &mut Vec<Diagnostic>,
    ) -> Subst {
        // Bodies and edges of every parsed named expression. A body that failed to
        // parse is pre-poisoned (its path is in failed_strings).
        let mut bodies: BTreeMap<&str, &Spanned<Expr>> = BTreeMap::new();
        let mut edges: BTreeMap<&str, Vec<String>> = BTreeMap::new();
        for name in &tables.named_exprs {
            let path = format!("expressions.named.{name}");
            if poisons.failed_strings.contains(&path) {
                poisons.poisoned_exprs.insert(name.clone());
                continue;
            }
            let Some(walked) = tables.ast_at(&path) else {
                continue;
            };
            let ParsedString::Expr(body) = &walked.parsed else {
                continue;
            };
            let mut refs = Vec::new();
            walk_expr(body, &mut |node| {
                if let Expr::Ref(r) = &node.node
                    && r.namespace == Namespace::Expr
                    && let Some(head) = r.path.first()
                {
                    refs.push(head.node.to_string());
                }
            });
            bodies.insert(name.as_str(), body);
            edges.insert(name.as_str(), refs);
        }

        // Tarjan over the declared names. Non-trivial SCCs (and self-loops) are
        // cycles: one diagnostic each, anchored at the first member in map order.
        let cycles = tarjan_cycles(&edges);
        for cycle in &cycles {
            let anchor = &cycle[0];
            let mut rendered: Vec<&str> = cycle.iter().map(String::as_str).collect();
            rendered.push(anchor); // close the loop for the message
            diagnostics.push(
                Diagnostic::rule(
                    DiagCode::ExprCycle,
                    DiagPath::root()
                        .key("expressions")
                        .key("named")
                        .name(anchor.clone()),
                    format!(
                        "named expressions reference each other in a cycle: {}",
                        rendered.join(" -> ")
                    ),
                )
                .with_help("named expressions are pure substitution; break the loop"),
            );
            for member in cycle {
                poisons.poisoned_exprs.insert(member.clone());
            }
        }

        // Transitive closure: anything referencing a poisoned or undeclared name is
        // itself unusable for substitution (no diagnostic — root cause reported).
        loop {
            let mut grew = false;
            for (name, refs) in &edges {
                if poisons.poisoned_exprs.contains(*name) {
                    continue;
                }
                let tainted = refs
                    .iter()
                    .any(|r| poisons.poisoned_exprs.contains(r) || !edges.contains_key(r.as_str()));
                if tainted {
                    poisons.poisoned_exprs.insert((*name).to_string());
                    grew = true;
                }
            }
            if !grew {
                break;
            }
        }

        // Inline every clean name, dependencies first. Iterating until settled is
        // O(n^2) worst case over a handful of names — simpler than materializing the
        // topological order, and the memo guarantees each body inlines once.
        let mut memo: BTreeMap<String, Spanned<Expr>> = BTreeMap::new();
        loop {
            let mut grew = false;
            for (name, body) in &bodies {
                if memo.contains_key(*name) || poisons.poisoned_exprs.contains(*name) {
                    continue;
                }
                let ready = edges[name].iter().all(|r| memo.contains_key(r));
                if ready && let Some(inlined) = inline(body, &memo) {
                    memo.insert((*name).to_string(), inlined);
                    grew = true;
                }
            }
            if !grew {
                break;
            }
        }

        Subst { memo }
    }

    /// Fully inline an expression: every `expr.NAME` replaced by its substituted
    /// body. `None` when the expression references a poisoned or undeclared name —
    /// callers suppress their checks (the root cause is already reported).
    pub fn substituted(&self, expr: &Spanned<Expr>) -> Option<Spanned<Expr>> {
        inline(expr, &self.memo)
    }
}

/// Clone-transform: replace `expr.` references from the memo, rebuild everything
/// else. `None` bubbles up from any reference outside the memo.
fn inline(expr: &Spanned<Expr>, memo: &BTreeMap<String, Spanned<Expr>>) -> Option<Spanned<Expr>> {
    let node = match &expr.node {
        Expr::Ref(r) if r.namespace == Namespace::Expr => {
            let head = r.path.first()?;
            // The substituted body keeps its own spans; diagnostics on substituted
            // ASTs anchor at the consuming string's path, not a span.
            return memo.get(head.node.as_ref()).cloned();
        }
        Expr::Literal(_) | Expr::Triggered { .. } => expr.node.clone(),
        Expr::Ref(r) => {
            let index = match &r.index {
                None => None,
                Some(keys) => Some(
                    keys.iter()
                        .map(|k| inline(k, memo))
                        .collect::<Option<Vec<_>>>()?,
                ),
            };
            Expr::Ref(Ref {
                namespace: r.namespace,
                path: r.path.clone(),
                index,
            })
        }
        Expr::Unary { op, operand } => Expr::Unary {
            op: *op,
            operand: Box::new(inline(operand, memo)?),
        },
        Expr::Arith { op, lhs, rhs } => Expr::Arith {
            op: *op,
            lhs: Box::new(inline(lhs, memo)?),
            rhs: Box::new(inline(rhs, memo)?),
        },
        Expr::Cmp { op, lhs, rhs } => Expr::Cmp {
            op: *op,
            lhs: Box::new(inline(lhs, memo)?),
            rhs: Box::new(inline(rhs, memo)?),
        },
        Expr::Logic { op, lhs, rhs } => Expr::Logic {
            op: *op,
            lhs: Box::new(inline(lhs, memo)?),
            rhs: Box::new(inline(rhs, memo)?),
        },
        Expr::If { arms, else_value } => Expr::If {
            arms: arms
                .iter()
                .map(|arm| {
                    Some(IfArm {
                        condition: inline(&arm.condition, memo)?,
                        value: inline(&arm.value, memo)?,
                    })
                })
                .collect::<Option<Vec<_>>>()?,
            else_value: Box::new(inline(else_value, memo)?),
        },
        Expr::Call {
            builtin,
            name_span,
            args,
        } => Expr::Call {
            builtin: *builtin,
            name_span: *name_span,
            args: args
                .iter()
                .map(|a| inline(a, memo))
                .collect::<Option<Vec<_>>>()?,
        },
    };
    Some(Spanned::new(node, expr.span))
}

/// Tarjan's strongly-connected components, recursive (the graph is a handful of
/// named expressions; depth is bounded by their count). Returns only the CYCLES:
/// non-trivial SCCs plus trivial ones with a self-edge, members in map order.
fn tarjan_cycles(edges: &BTreeMap<&str, Vec<String>>) -> Vec<Vec<String>> {
    struct State<'a> {
        edges: &'a BTreeMap<&'a str, Vec<String>>,
        index: BTreeMap<&'a str, usize>,
        low: BTreeMap<&'a str, usize>,
        on_stack: BTreeSet<&'a str>,
        stack: Vec<&'a str>,
        next: usize,
        cycles: Vec<Vec<String>>,
    }

    fn visit<'a>(v: &'a str, st: &mut State<'a>) {
        st.index.insert(v, st.next);
        st.low.insert(v, st.next);
        st.next += 1;
        st.stack.push(v);
        st.on_stack.insert(v);

        for w in &st.edges[v] {
            let w = w.as_str();
            if !st.edges.contains_key(w) {
                continue; // undeclared or poisoned target: not part of this graph
            }
            if !st.index.contains_key(w) {
                // Rust note: the recursive call re-borrows `st` mutably; the borrow
                // checker allows it because the previous borrows ended.
                let w_key = st
                    .edges
                    .keys()
                    .find(|k| **k == w)
                    .copied()
                    .expect("checked contains_key above");
                visit(w_key, st);
                let low_w = st.low[w];
                let low_v = st.low[v];
                st.low.insert(v, low_v.min(low_w));
            } else if st.on_stack.contains(w) {
                let idx_w = st.index[w];
                let low_v = st.low[v];
                st.low.insert(v, low_v.min(idx_w));
            }
        }

        if st.low[v] == st.index[v] {
            let mut component = Vec::new();
            while let Some(w) = st.stack.pop() {
                st.on_stack.remove(w);
                component.push(w.to_string());
                if w == v {
                    break;
                }
            }
            let is_cycle = component.len() > 1 || st.edges[v].iter().any(|w| w == v);
            if is_cycle {
                component.sort();
                st.cycles.push(component);
            }
        }
    }

    let mut st = State {
        edges,
        index: BTreeMap::new(),
        low: BTreeMap::new(),
        on_stack: BTreeSet::new(),
        stack: Vec::new(),
        next: 0,
        cycles: Vec::new(),
    };
    for v in edges.keys() {
        if !st.index.contains_key(v) {
            visit(v, &mut st);
        }
    }
    st.cycles.sort();
    st.cycles
}
