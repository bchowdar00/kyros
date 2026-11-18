//! The coverage lint, DRIFT-ONLY scope (ruled 2026-07-20, Q3 = option B): an intent
//! block's guard threshold warns only when a guaranteed same-field trigger leaf
//! EXISTS and fails to imply it — the residual failure mode of writing one threshold
//! in two places and letting them drift. A guard comparison on a field no relevant
//! trigger constrains is silent in v1.
//!
//! Mechanics (design §2.6): conjuncts are the guard flattened over top-level `and`
//! only (an `or`-rooted guard contributes nothing — recorded limitation); a conjunct
//! participates iff it compares `signal.<nick>.<field>` against a literal; relevant
//! triggers are the guard's pinned set, else the leg's armed condition triggers;
//! guaranteed leaves are field leaves of a lone-leaf condition or an `all` group
//! (`any` guarantees nothing at fire time); implication is exact decimal comparison,
//! never floats.

use std::cmp::Ordering;

use crate::ir::{Comparator, Condition, ConditionLeaf, FieldLeaf, LeafValue, Strategy, TriggerSource};

use crate::parse::ast::expr::{CmpOp, Expr, Literal, Namespace, UnaryOp};
use crate::parse::span::Spanned;
use crate::parse::walk::ParsedString;
use crate::validate::decimal::cmp_decimal;
use crate::validate::diag::{DiagCode, DiagPath, Diagnostic};
use crate::validate::tables::{Tables, TriggerKind};

use super::BlockInfo;

/// One classified guard comparison: `signal.<nick>.<field> <op> <literal>`.
struct GuardCmp {
    nick: String,
    field: String,
    op: CmpOp,
    value: Lit,
}

/// A comparison literal: a (possibly signed) decimal, or a symbolic value (enum
/// variant / boolean — only equality logic applies).
enum Lit {
    Num(String),
    Sym(String),
}

/// Run the coverage lint.
pub(super) fn check(
    strategy: &Strategy,
    tables: &Tables,
    blocks: &[BlockInfo],
    diagnostics: &mut Vec<Diagnostic>,
) {
    for info in blocks {
        if !info.has_intent || info.guard_failed {
            continue;
        }
        let path = format!("legs[{}].body[{}].if", info.leg, info.block);
        let Some(walked) = tables.ast_at(&path) else {
            continue;
        };
        let ParsedString::Expr(guard) = &walked.parsed else {
            continue;
        };

        let mut comparisons = Vec::new();
        for conjunct in conjuncts(guard) {
            if let Some(cmp) = classify(conjunct) {
                comparisons.push(cmp);
            }
        }
        if comparisons.is_empty() {
            continue;
        }

        // Relevant triggers: pinned set when the guard names any, else the leg's
        // armed condition triggers.
        let relevant: Vec<&str> = if info.triggered.is_empty() {
            strategy.legs[info.leg]
                .arm
                .iter()
                .map(|t| t.as_str())
                .filter(|t| {
                    tables
                        .triggers
                        .get(*t)
                        .is_some_and(|e| e.kind == TriggerKind::Condition)
                })
                .collect()
        } else {
            info.triggered.iter().map(String::as_str).collect()
        };

        for cmp in &comparisons {
            // Every guaranteed same-field leaf across the relevant triggers.
            let mut same_field: Vec<(&str, &FieldLeaf)> = Vec::new();
            for trigger_name in &relevant {
                let Some(entry) = tables.triggers.get(*trigger_name) else {
                    continue;
                };
                let TriggerSource::Condition(condition) = &strategy.triggers[entry.index].source
                else {
                    continue;
                };
                for leaf in guaranteed_leaves(condition) {
                    if leaf.signal.as_str() == cmp.nick && leaf.field.as_str() == cmp.field {
                        same_field.push((trigger_name, leaf));
                    }
                }
            }
            // Drift-only: no same-field leaf means no warning (Q3 = B).
            if same_field.is_empty() {
                continue;
            }
            if same_field.iter().any(|(_, leaf)| implies(leaf, cmp)) {
                continue;
            }
            let (trigger_name, nearest) = same_field[0];
            diagnostics.push(
                Diagnostic::rule(
                    DiagCode::LintCoverage,
                    DiagPath::from_walker(&path),
                    format!(
                        "guard requires signal.{}.{} {} {} but armed trigger `{}` fires at \
                         {} {} — the block runs on a subset of wakes; one threshold is stale",
                        cmp.nick,
                        cmp.field,
                        op_str(cmp.op),
                        lit_str(&cmp.value),
                        trigger_name,
                        comparator_str(nearest.comparator),
                        leaf_value_str(&nearest.value),
                    ),
                )
                .with_source(walked.source.clone(), guard.span)
                .with_help(
                    "the trigger is the threshold's home — make the guard's number match \
                     or drop the guard comparison",
                ),
            );
        }
    }
}

/// Flatten over top-level `and` only.
fn conjuncts(expr: &Spanned<Expr>) -> Vec<&Spanned<Expr>> {
    match &expr.node {
        Expr::Logic {
            op: crate::parse::ast::expr::LogicOp::And,
            lhs,
            rhs,
        } => {
            let mut out = conjuncts(lhs);
            out.extend(conjuncts(rhs));
            out
        }
        Expr::Logic { .. }
        | Expr::Literal(_)
        | Expr::Ref(_)
        | Expr::Unary { .. }
        | Expr::Arith { .. }
        | Expr::Cmp { .. }
        | Expr::If { .. }
        | Expr::Call { .. }
        | Expr::Triggered { .. }
        => vec![expr],
    }
}

/// Classify a conjunct as a signal-field-vs-literal comparison, ref normalized to
/// the left (comparator flipped when the source wrote it reversed).
fn classify(expr: &Spanned<Expr>) -> Option<GuardCmp> {
    let Expr::Cmp { op, lhs, rhs } = &expr.node else {
        return None;
    };
    if let (Some((nick, field)), Some(value)) = (signal_ref(lhs), literal(rhs)) {
        return Some(GuardCmp {
            nick,
            field,
            op: *op,
            value,
        });
    }
    if let (Some(value), Some((nick, field))) = (literal(lhs), signal_ref(rhs)) {
        return Some(GuardCmp {
            nick,
            field,
            op: flip(*op),
            value,
        });
    }
    None
}

fn signal_ref(expr: &Spanned<Expr>) -> Option<(String, String)> {
    let Expr::Ref(r) = &expr.node else {
        return None;
    };
    if r.namespace != Namespace::Signal || r.index.is_some() {
        return None;
    }
    let [nick, field] = r.path.as_slice() else {
        return None;
    };
    Some((nick.node.to_string(), field.node.to_string()))
}

fn literal(expr: &Spanned<Expr>) -> Option<Lit> {
    match &expr.node {
        Expr::Literal(Literal::Decimal(text)) => Some(Lit::Num(text.to_string())),
        Expr::Literal(Literal::Variant(v)) => Some(Lit::Sym(v.to_string())),
        Expr::Literal(Literal::Bool(b)) => Some(Lit::Sym(b.to_string())),
        Expr::Unary {
            op: UnaryOp::Neg,
            operand,
        } => match &operand.node {
            Expr::Literal(Literal::Decimal(text)) => Some(Lit::Num(format!("-{text}"))),
            Expr::Literal(Literal::Bool(_) | Literal::DurationMs(_) | Literal::Variant(_))
            | Expr::Ref(_)
            | Expr::Unary { .. }
            | Expr::Arith { .. }
            | Expr::Cmp { .. }
            | Expr::Logic { .. }
            | Expr::If { .. }
            | Expr::Call { .. }
            | Expr::Triggered { .. }
            => None,
        },
        Expr::Literal(Literal::DurationMs(_))
        | Expr::Ref(_)
        | Expr::Unary { .. }
        | Expr::Arith { .. }
        | Expr::Cmp { .. }
        | Expr::Logic { .. }
        | Expr::If { .. }
        | Expr::Call { .. }
        | Expr::Triggered { .. }
        => None,
    }
}

fn flip(op: CmpOp) -> CmpOp {
    match op {
        CmpOp::Lt => CmpOp::Gt,
        CmpOp::Le => CmpOp::Ge,
        CmpOp::Gt => CmpOp::Lt,
        CmpOp::Ge => CmpOp::Le,
        CmpOp::Eq => CmpOp::Eq,
        CmpOp::Ne => CmpOp::Ne,
    }
}

/// Field leaves guaranteed true at fire time: a lone leaf, or `all`-group members.
fn guaranteed_leaves(condition: &Condition) -> Vec<&FieldLeaf> {
    let leaves: &[ConditionLeaf] = match condition {
        Condition::Leaf(leaf) => std::slice::from_ref(leaf),
        Condition::All(group) => &group.all,
        Condition::Any(_) => return Vec::new(),
    };
    leaves
        .iter()
        .filter_map(|leaf| match leaf {
            ConditionLeaf::Field(field) => Some(field),
            ConditionLeaf::Bare(_) | ConditionLeaf::Expr(_) => None,
        })
        .collect()
}

/// Does `leaf` (true at fire time) imply the guard comparison?
fn implies(leaf: &FieldLeaf, cmp: &GuardCmp) -> bool {
    match (&leaf.value, &cmp.value) {
        (LeafValue::Decimal(a), Lit::Num(b)) => {
            numeric_implies(leaf.comparator, a.as_str(), cmp.op, b)
        }
        (LeafValue::Variant(a), Lit::Sym(b)) => {
            let equal = a.as_str() == b;
            match (leaf.comparator, cmp.op) {
                (Comparator::Eq, CmpOp::Eq) => equal,
                (Comparator::Eq, CmpOp::Ne) => !equal,
                (Comparator::Ne, CmpOp::Ne) => equal,
                // Ordering on symbols never implies; Ne implies Eq never.
                _ => false,
            }
        }
        // Mixed numeric/symbolic never implies (and is a type error elsewhere); a
        // string or param-valued leaf pins nothing the guard's literal can be
        // compared against at compile time.
        (LeafValue::Decimal(_), Lit::Sym(_))
        | (LeafValue::Variant(_), Lit::Num(_))
        | (LeafValue::Str(_) | LeafValue::Param(_), _) => false,
    }
}

/// The numeric implication table over exact decimal comparison.
/// `f <op_l> a` (known) implies `f <op_g> b`?
fn numeric_implies(op_l: Comparator, a: &str, op_g: CmpOp, b: &str) -> bool {
    let ab = cmp_decimal(a, b);
    match op_g {
        CmpOp::Lt => match op_l {
            Comparator::Lt => ab != Ordering::Greater, // a <= b
            Comparator::Le | Comparator::Eq => ab == Ordering::Less, // a < b
            Comparator::Gt | Comparator::Ge | Comparator::Ne => false,
        },
        CmpOp::Le => match op_l {
            Comparator::Lt | Comparator::Le | Comparator::Eq => ab != Ordering::Greater,
            Comparator::Gt | Comparator::Ge | Comparator::Ne => false,
        },
        CmpOp::Gt => match op_l {
            Comparator::Gt => ab != Ordering::Less, // a >= b
            Comparator::Ge | Comparator::Eq => ab == Ordering::Greater, // a > b
            Comparator::Lt | Comparator::Le | Comparator::Ne => false,
        },
        CmpOp::Ge => match op_l {
            Comparator::Gt | Comparator::Ge | Comparator::Eq => ab != Ordering::Less,
            Comparator::Lt | Comparator::Le | Comparator::Ne => false,
        },
        CmpOp::Eq => matches!(op_l, Comparator::Eq) && ab == Ordering::Equal,
        CmpOp::Ne => match op_l {
            Comparator::Ne => ab == Ordering::Equal,
            Comparator::Eq => ab != Ordering::Equal,
            Comparator::Lt => ab != Ordering::Greater, // f < a <= b => f != b
            Comparator::Le => ab == Ordering::Less,    // f <= a < b => f != b
            Comparator::Gt => ab != Ordering::Less,    // f > a >= b => f != b
            Comparator::Ge => ab == Ordering::Greater, // f >= a > b => f != b
        },
    }
}

fn op_str(op: CmpOp) -> &'static str {
    match op {
        CmpOp::Lt => "<",
        CmpOp::Le => "<=",
        CmpOp::Gt => ">",
        CmpOp::Ge => ">=",
        CmpOp::Eq => "==",
        CmpOp::Ne => "!=",
    }
}

fn comparator_str(comparator: Comparator) -> &'static str {
    match comparator {
        Comparator::Lt => "<",
        Comparator::Le => "<=",
        Comparator::Gt => ">",
        Comparator::Ge => ">=",
        Comparator::Eq => "==",
        Comparator::Ne => "!=",
    }
}

fn lit_str(lit: &Lit) -> String {
    match lit {
        Lit::Num(n) => n.clone(),
        Lit::Sym(s) => format!("'{s}'"),
    }
}

fn leaf_value_str(value: &LeafValue) -> String {
    match value {
        LeafValue::Decimal(d) => d.as_str().to_string(),
        LeafValue::Variant(v) => v.as_str().to_string(),
        LeafValue::Str(s) => format!("{s:?}"),
        LeafValue::Param(p) => format!("param.{p}"),
    }
}
