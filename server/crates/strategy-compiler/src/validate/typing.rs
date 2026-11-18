//! Family G: typing-lite — a four-point lattice and DEFINITE-ONLY judgments.
//!
//! This is deliberately not a type system (full inference, numeric promotion, and
//! totality live in a future AST spec). `Unknown` propagates silently; a diagnostic is
//! emitted only when two sides are both known and definitely incompatible. All four
//! registry numeric types collapse into one `Numeric` class — promotion is not this
//! module's question. Strings (`string` fields, params and slots) are a class of their
//! own: equality only, no arithmetic, no ordering.

use std::collections::BTreeSet;

use crate::ir::{FieldClass, Registry, StateSlot, Strategy, TypeKind, TypeSpec};

use crate::parse::ast::expr::{CmpOp, Expr, Literal, Namespace, UnaryOp};
use crate::parse::builtins::Builtin;
use crate::parse::span::Spanned;
use crate::parse::walk::{ParsedString, StringRole, WalkedString};
use crate::validate::diag::{DiagCode, DiagPath, Diagnostic};
use crate::validate::tables::Tables;

use super::context::{PinStatus, Pins};

/// The lattice. `Enum` carries its variant set so membership is checkable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValueType {
    /// Any of the numeric classes (decimal, u8, u32, duration).
    Numeric,
    /// A boolean.
    Bool,
    /// A closed variant set.
    Enum(BTreeSet<String>),
    /// A string: equality-comparable only.
    Str,
    /// Not determinable here; propagates silently.
    Unknown,
}

/// Everything `type_of` reads. One per walked string.
pub struct TypeCtx<'a> {
    /// Venue truth for field types.
    pub registry: &'a Registry,
    /// Resolution tables (signal entries, state indices).
    pub tables: &'a Tables,
    /// The document, for state slot type specs.
    pub strategy: &'a Strategy,
    /// The enclosing block's pin, when the string is block-scoped (None for named
    /// expressions — `event.*` is banned there anyway).
    pub pin: Option<&'a PinStatus>,
    /// The string under judgment, for diagnostic anchoring.
    pub walked: &'a WalkedString,
}

/// Run the typing pass: every string's internal judgments, plus the guard-root rule.
pub fn check(
    strategy: &Strategy,
    registry: &Registry,
    tables: &Tables,
    pins: &Pins,
    diagnostics: &mut Vec<Diagnostic>,
) {
    // Block-scoped strings carry their block's pin; document-level strings none.
    for walked in &tables.asts {
        let pin = pin_for_path(&walked.path, strategy, pins);
        let ctx = TypeCtx {
            registry,
            tables,
            strategy,
            pin,
            walked,
        };
        let root_type = match &walked.parsed {
            ParsedString::Expr(expr) => type_of(expr, &ctx, &mut |d| diagnostics.push(d)),
            ParsedString::Stmt(_) => continue, // Statement typing is stmt.rs's job.
        };
        if walked.role == StringRole::TriggerExpr
            && matches!(root_type, ValueType::Bool | ValueType::Enum(_) | ValueType::Str)
        {
            diagnostics.push(
                Diagnostic::rule(
                    DiagCode::LeafFieldNotNumeric,
                    DiagPath::from_walker(&walked.path),
                    "a trigger expression is arithmetic — this expression is not numeric",
                )
                .with_source(walked.source.clone(), root_span(walked))
                .with_help("compare enum, string and boolean fields with a field leaf"),
            );
        }
        if walked.role == StringRole::Guard
            && matches!(
                root_type,
                ValueType::Numeric | ValueType::Enum(_) | ValueType::Str
            )
        {
            diagnostics.push(
                Diagnostic::rule(
                    DiagCode::GuardNotBoolean,
                    DiagPath::from_walker(&walked.path),
                    "a guard is a boolean condition — this expression is a value",
                )
                .with_source(walked.source.clone(), root_span(walked))
                .with_help("compare the value against a threshold, or drop the guard"),
            );
        }
    }
}

fn root_span(walked: &WalkedString) -> crate::parse::span::Span {
    match &walked.parsed {
        ParsedString::Expr(expr) => expr.span,
        ParsedString::Stmt(stmt) => stmt.span,
    }
}

/// The pin of the block owning a walker path, if the path is block-scoped
/// (`legs[l].body[b].…`).
fn pin_for_path<'a>(path: &str, strategy: &Strategy, pins: &'a Pins) -> Option<&'a PinStatus> {
    let rest = path.strip_prefix("legs[")?;
    let (l, rest) = rest.split_once(']')?;
    let rest = rest.strip_prefix(".body[")?;
    let (b, _) = rest.split_once(']')?;
    let (l, b) = (l.parse::<usize>().ok()?, b.parse::<usize>().ok()?);
    // Guard against paths this document does not have (cannot happen for walker
    // paths, but the parse is total either way).
    strategy.legs.get(l)?.body.get(b)?;
    Some(pins.at(l, b))
}

/// Compute an expression's lattice type, emitting definite-mismatch diagnostics
/// through `emit` along the way. Pass a no-op sink for a quiet query (stmt.rs does,
/// to avoid double-emitting on subtrees this pass already judged).
pub fn type_of(
    expr: &Spanned<Expr>,
    ctx: &TypeCtx<'_>,
    emit: &mut dyn FnMut(Diagnostic),
) -> ValueType {
    match &expr.node {
        Expr::Literal(literal) => match literal {
            Literal::Decimal(_) | Literal::DurationMs(_) => ValueType::Numeric,
            Literal::Bool(_) => ValueType::Bool,
            // A bare variant's enum is determined by context; comparison and
            // assignment sites handle membership.
            Literal::Variant(_) => ValueType::Unknown,
        },
        Expr::Ref(r) => ref_type(r, ctx),
        Expr::Unary { op, operand } => {
            let inner = type_of(operand, ctx, emit);
            match op {
                UnaryOp::Neg => {
                    if matches!(inner, ValueType::Bool | ValueType::Enum(_) | ValueType::Str) {
                        emit(mismatch(ctx, operand.span, "negation needs a number"));
                    }
                    ValueType::Numeric
                }
                UnaryOp::Not => {
                    if matches!(inner, ValueType::Numeric | ValueType::Enum(_) | ValueType::Str) {
                        emit(mismatch(ctx, operand.span, "`not` needs a boolean"));
                    }
                    ValueType::Bool
                }
            }
        }
        Expr::Arith { lhs, rhs, .. } => {
            for side in [lhs, rhs] {
                let t = type_of(side, ctx, emit);
                if matches!(t, ValueType::Bool | ValueType::Enum(_) | ValueType::Str)
                    || matches!(side.node, Expr::Literal(Literal::Variant(_)))
                {
                    emit(mismatch(ctx, side.span, "arithmetic needs numbers"));
                }
            }
            ValueType::Numeric
        }
        Expr::Cmp { op, lhs, rhs } => {
            comparison(*op, lhs, rhs, ctx, emit);
            ValueType::Bool
        }
        Expr::Logic { lhs, rhs, .. } => {
            for side in [lhs, rhs] {
                let t = type_of(side, ctx, emit);
                if matches!(t, ValueType::Numeric | ValueType::Enum(_) | ValueType::Str) {
                    emit(mismatch(ctx, side.span, "`and`/`or` connect booleans"));
                }
            }
            ValueType::Bool
        }
        Expr::If { arms, else_value } => {
            let mut result: Option<ValueType> = None;
            for arm in arms {
                let cond = type_of(&arm.condition, ctx, emit);
                if matches!(cond, ValueType::Numeric | ValueType::Enum(_) | ValueType::Str) {
                    emit(mismatch(
                        ctx,
                        arm.condition.span,
                        "an `if` condition is a boolean",
                    ));
                }
                join_branch(
                    &mut result,
                    type_of(&arm.value, ctx, emit),
                    arm.value.span,
                    ctx,
                    emit,
                );
            }
            let else_type = type_of(else_value, ctx, emit);
            join_branch(&mut result, else_type, else_value.span, ctx, emit);
            result.unwrap_or(ValueType::Unknown)
        }
        Expr::Call { builtin, args, .. } => {
            let is_reducer = super::context::is_reducer(*builtin);
            for arg in args {
                // `lookup`'s first argument is a table reference and a reducer's is a
                // window reference: neither is a value.
                if (*builtin == Builtin::Lookup || is_reducer) && std::ptr::eq(arg, &args[0]) {
                    continue;
                }
                let t = type_of(arg, ctx, emit);
                if *builtin == Builtin::CountWhere {
                    if matches!(t, ValueType::Numeric | ValueType::Enum(_) | ValueType::Str) {
                        emit(mismatch(ctx, arg.span, "a count_where predicate is a boolean"));
                    }
                } else if matches!(t, ValueType::Bool | ValueType::Enum(_) | ValueType::Str) {
                    emit(mismatch(ctx, arg.span, "builtin arguments are numeric"));
                }
            }
            // Reducers are numeric too: windows hold numbers, counts are integers.
            ValueType::Numeric
        }
        Expr::Triggered { .. } => ValueType::Bool,
    }
}

/// Comparison compatibility, including variant-literal membership.
fn comparison(
    op: CmpOp,
    lhs: &Spanned<Expr>,
    rhs: &Spanned<Expr>,
    ctx: &TypeCtx<'_>,
    emit: &mut dyn FnMut(Diagnostic),
) {
    let lt = type_of(lhs, ctx, emit);
    let rt = type_of(rhs, ctx, emit);
    let ordering = matches!(op, CmpOp::Lt | CmpOp::Le | CmpOp::Gt | CmpOp::Ge);

    // Variant-literal membership against a known enum side.
    for (literal_side, other_type) in [(lhs, &rt), (rhs, &lt)] {
        if let Expr::Literal(Literal::Variant(variant)) = &literal_side.node {
            if ordering {
                emit(mismatch(
                    ctx,
                    literal_side.span,
                    "enums compare with == or != only",
                ));
            }
            match other_type {
                ValueType::Enum(variants) => {
                    if !variants.contains(variant.as_ref()) {
                        let known: Vec<&str> = variants.iter().map(String::as_str).collect();
                        emit(Diagnostic::rule(
                            DiagCode::UnknownVariant,
                            DiagPath::from_walker(&ctx.walked.path),
                            format!(
                                "'{variant}' is not a variant of the compared enum (variants: {})",
                                known.join(", ")
                            ),
                        )
                        .with_source(ctx.walked.source.clone(), literal_side.span));
                    }
                }
                ValueType::Numeric | ValueType::Bool | ValueType::Str => {
                    emit(mismatch(
                        ctx,
                        literal_side.span,
                        "a variant compared against a non-enum value",
                    ));
                }
                ValueType::Unknown => {}
            }
        }
    }

    match (&lt, &rt) {
        (ValueType::Numeric, ValueType::Enum(_))
        | (ValueType::Enum(_), ValueType::Numeric)
        | (ValueType::Bool, ValueType::Numeric)
        | (ValueType::Numeric, ValueType::Bool)
        | (ValueType::Bool, ValueType::Enum(_))
        | (ValueType::Enum(_), ValueType::Bool)
        | (ValueType::Str, ValueType::Numeric | ValueType::Bool | ValueType::Enum(_))
        | (ValueType::Numeric | ValueType::Bool | ValueType::Enum(_), ValueType::Str) => {
            emit(mismatch(
                ctx,
                lhs.span.join(rhs.span),
                "the two sides of this comparison are different kinds of value",
            ));
        }
        (ValueType::Str, ValueType::Str) => {
            if ordering {
                emit(mismatch(
                    ctx,
                    lhs.span.join(rhs.span),
                    "strings compare with == or != only",
                ));
            }
        }
        (ValueType::Enum(a), ValueType::Enum(b)) => {
            if ordering {
                emit(mismatch(
                    ctx,
                    lhs.span.join(rhs.span),
                    "enums compare with == or != only",
                ));
            } else if a.is_disjoint(b) {
                emit(mismatch(
                    ctx,
                    lhs.span.join(rhs.span),
                    "these enums share no variants — the comparison is constant",
                ));
            }
        }
        (ValueType::Bool, ValueType::Bool) => {
            if ordering {
                emit(mismatch(
                    ctx,
                    lhs.span.join(rhs.span),
                    "booleans compare with == or != only",
                ));
            }
        }
        (ValueType::Numeric, ValueType::Numeric)
        | (ValueType::Unknown, _)
        | (_, ValueType::Unknown) => {}
    }
}

/// Merge a branch type into the running if-expression result; definite class
/// disagreements are diagnostics, anything touching Unknown degrades quietly.
fn join_branch(
    result: &mut Option<ValueType>,
    branch: ValueType,
    span: crate::parse::span::Span,
    ctx: &TypeCtx<'_>,
    emit: &mut dyn FnMut(Diagnostic),
) {
    match result {
        None => *result = Some(branch),
        Some(current) => {
            let compatible = matches!(
                (&*current, &branch),
                (ValueType::Unknown, _)
                    | (_, ValueType::Unknown)
                    | (ValueType::Numeric, ValueType::Numeric)
                    | (ValueType::Bool, ValueType::Bool)
                    | (ValueType::Str, ValueType::Str)
                    | (ValueType::Enum(_), ValueType::Enum(_))
            );
            if !compatible {
                emit(mismatch(
                    ctx,
                    span,
                    "the branches of this conditional are different kinds of value",
                ));
            }
            if *current == ValueType::Unknown {
                *current = branch;
            } else if branch != *current {
                *current = ValueType::Unknown;
            }
        }
    }
}

/// A reference's lattice type.
fn ref_type(r: &crate::parse::ast::expr::Ref, ctx: &TypeCtx<'_>) -> ValueType {
    match r.namespace {
        Namespace::Signal => match r.path.as_slice() {
            [nick, field] => field_type_of(nick.node.as_ref(), field.node.as_ref(), ctx),
            _ => ValueType::Unknown,
        },
        Namespace::Event => match (ctx.pin, r.path.as_slice()) {
            (Some(PinStatus::Pinned { signal }), [field]) => {
                field_type_of(signal, field.node.as_ref(), ctx)
            }
            _ => ValueType::Unknown,
        },
        Namespace::State => {
            let Some(head) = r.path.first() else {
                return ValueType::Unknown;
            };
            let Some(index) = ctx.tables.state.get(head.node.as_ref()) else {
                return ValueType::Unknown;
            };
            match &ctx.strategy.state[*index] {
                StateSlot::Scalar { value_type, .. } | StateSlot::Map { value_type, .. } => {
                    spec_type(value_type)
                }
                // A window is not a value (refs.rs reports its misuse); stay quiet.
                StateSlot::Window { .. } => ValueType::Unknown,
            }
        }
        // The element under a count_where predicate: windows are numeric.
        Namespace::Entry => ValueType::Numeric,
        Namespace::Param => {
            let Some(head) = r.path.first() else {
                return ValueType::Unknown;
            };
            let Some(index) = ctx.tables.params.get(head.node.as_ref()) else {
                return ValueType::Unknown;
            };
            spec_type(&ctx.strategy.params[*index].type_spec)
        }
        // Closed numeric surfaces (the legality of the path is namespaces.rs's job).
        Namespace::Platform | Namespace::Envelope | Namespace::Legs => ValueType::Numeric,
        // Named expressions would need substitution to type; stay quiet. Table
        // references are not values (refs.rs owns their placement).
        Namespace::Expr | Namespace::Table => ValueType::Unknown,
    }
}

fn field_type_of(nick: &str, field: &str, ctx: &TypeCtx<'_>) -> ValueType {
    let Some(fields) = crate::validate::registry::signal_fields(nick, ctx.registry, ctx.tables)
    else {
        return ValueType::Unknown;
    };
    let Some(def) = fields.iter().find(|f| f.name.as_str() == field) else {
        return ValueType::Unknown;
    };
    match def.field_type.class() {
        FieldClass::Numeric => ValueType::Numeric,
        FieldClass::Enum(variants) => ValueType::Enum(variants.into_iter().collect()),
        FieldClass::Str => ValueType::Str,
    }
}

/// A declared `TypeSpec`'s lattice type (state slot values and map keys).
pub fn spec_type(spec: &TypeSpec) -> ValueType {
    match spec.kind() {
        TypeKind::U8 | TypeKind::Decimal => ValueType::Numeric,
        TypeKind::Bool => ValueType::Bool,
        TypeKind::Str => ValueType::Str,
        TypeKind::Enum(variants) => {
            ValueType::Enum(variants.iter().map(|v| v.as_str().to_string()).collect())
        }
    }
}

fn mismatch(ctx: &TypeCtx<'_>, span: crate::parse::span::Span, message: &str) -> Diagnostic {
    Diagnostic::rule(
        DiagCode::TypeMismatch,
        DiagPath::from_walker(&ctx.walked.path),
        message,
    )
    .with_source(ctx.walked.source.clone(), span)
}
