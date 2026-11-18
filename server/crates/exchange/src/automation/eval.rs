//! The [`ExprNode`] evaluator, shared by trigger leaves (SME) and block bodies
//! (interpreter). One evaluator, one dialect: the compiler lowers both through
//! the same grammar.
//!
//! Reads are supplied by an [`EvalCtx`]. The SME's context knows only signal
//! snapshots and params; the interpreter's knows everything. A read the
//! context cannot serve is an [`EvalError`], which the SME turns into `false`
//! and the interpreter treats as a deployment-fatal invariant failure (the
//! validator and the block's `reads` gate should have prevented it).
//!
//! # Pinned totality
//!
//! The IR spec leaves the analytic domain edges open (IR-CONFIRM-4). This
//! runtime pins them, and the pins are the contract for every strategy that
//! runs here:
//!
//! | Case | Result |
//! |---|---|
//! | `x / 0` | context decides: [`EvalCtx::division_by_zero`] returns a value (bodies: `0`) or `None` (triggers: the leaf is false) |
//! | `ln(x)`, `x <= 0` | `0` |
//! | `sqrt(x)`, `x < 0` | `0` |
//! | `pow(x, y)` outside the real domain or overflowing | `0` |
//! | `exp(x)` overflowing | `0` |
//! | `norm_cdf(x)` | `x` clamped to `[-8, 8]` first (the tails beyond are under 1e-15; the library overflows past them) |
//! | `round(x)` | half away from zero (`round(2.5) = 3`, `round(-2.5) = -3`) |
//! | reducer over an empty window | `0` |
//! | `lookup` outside the key range | clamps to the end values |

use rust_decimal::{Decimal, MathematicalOps, RoundingStrategy};

/// `norm_cdf` input clamp: the library's expansion overflows past here and
/// the tail is already under 1e-15.
const NORM_CDF_MIN: Decimal = Decimal::from_parts(8, 0, 0, true, 0);
const NORM_CDF_MAX: Decimal = Decimal::from_parts(8, 0, 0, false, 0);
use strategy_compiler::ir::Interpolation;
use strategy_compiler::program::{Builtin, ExprNode, LegFillField, PlatformRead, Reducer, TableDef};

use super::value::{Value, compare, parse_decimal};

/// Why an expression could not be evaluated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvalError {
    /// A signal field the context has no snapshot for.
    MissingSignalField { signal_id: String, field: String },
    /// An `event.<field>` read with no event or no such field.
    MissingEventField(String),
    /// A state slot or map key the context does not know.
    UnknownStateSlot(String),
    /// A param the context does not know.
    UnknownParam(String),
    /// A table the program does not carry.
    UnknownTable(String),
    /// Division by zero in a context that does not pin it.
    DivisionByZero,
    /// A value of the wrong shape reached an operator (should be unreachable
    /// after validation).
    TypeMismatch(&'static str),
    /// A literal in the program did not parse.
    BadLiteral(String),
    /// The context does not serve this kind of read (e.g. `state.*` in a
    /// trigger leaf).
    Unsupported(&'static str),
}

/// What an expression may read. Every method has a default of
/// [`EvalError::Unsupported`], so a context implements only what its position
/// in the pipeline allows.
pub trait EvalCtx {
    /// `signal.<nick>.<field>` from the latest snapshot.
    fn signal(&self, signal_id: &str, field: &str) -> Result<Value, EvalError>;

    /// `param.<name>`.
    fn param(&self, name: &str) -> Result<Value, EvalError>;

    /// The pinned value of `x / 0`, or `None` to make the expression fail.
    fn division_by_zero(&self) -> Option<Decimal>;

    /// `event.<field>` of the triggering event.
    fn event(&self, _field: &str) -> Result<Value, EvalError> {
        Err(EvalError::Unsupported("event"))
    }

    /// `state.<slot>`.
    fn state(&self, _slot: &str) -> Result<Value, EvalError> {
        Err(EvalError::Unsupported("state"))
    }

    /// `state.<slot>[keys]`.
    fn state_map(&self, _slot: &str, _keys: &[Value]) -> Result<Value, EvalError> {
        Err(EvalError::Unsupported("state_map"))
    }

    /// The elements of a window slot, oldest first.
    fn window(&self, _slot: &str) -> Result<Vec<Decimal>, EvalError> {
        Err(EvalError::Unsupported("window"))
    }

    /// `platform.*`.
    fn platform(&self, _read: PlatformRead) -> Result<Value, EvalError> {
        Err(EvalError::Unsupported("platform"))
    }

    /// `legs.<leg>[.tag.<tag>].<field>`.
    fn leg_fill(&self, _leg: &str, _tag: Option<&str>, _field: LegFillField) -> Result<Value, EvalError> {
        Err(EvalError::Unsupported("leg_fill"))
    }

    /// `triggered(NAME)`.
    fn triggered(&self, _name: &str) -> Result<bool, EvalError> {
        Err(EvalError::Unsupported("triggered"))
    }

    /// `lookup(table.NAME, key)`'s table.
    fn table(&self, _name: &str) -> Result<&TableDef, EvalError> {
        Err(EvalError::Unsupported("lookup"))
    }
}

/// Evaluate `node` in `ctx`.
pub fn eval(node: &ExprNode, ctx: &dyn EvalCtx) -> Result<Value, EvalError> {
    eval_inner(node, ctx, None)
}

/// `entry` is the element bound under a `count_where` predicate.
fn eval_inner(node: &ExprNode, ctx: &dyn EvalCtx, entry: Option<Decimal>) -> Result<Value, EvalError> {
    let ev = |n: &ExprNode| eval_inner(n, ctx, entry);
    let num = |n: &ExprNode| -> Result<Decimal, EvalError> {
        ev(n)?.as_decimal().ok_or(EvalError::TypeMismatch("expected a number"))
    };
    let boolean = |n: &ExprNode| -> Result<bool, EvalError> {
        ev(n)?.as_bool().ok_or(EvalError::TypeMismatch("expected a boolean"))
    };

    Ok(match node {
        ExprNode::Decimal { value } => Value::Decimal(literal(value)?),
        ExprNode::Bool { value } => Value::Bool(*value),
        ExprNode::DurationMs { value } => Value::from_u64(*value),
        ExprNode::Variant { value } | ExprNode::Str { value } => Value::Text(value.clone()),
        ExprNode::Param { name } => ctx.param(name)?,
        ExprNode::Signal { signal_id, field } => ctx.signal(signal_id, field)?,
        ExprNode::Event { field } => ctx.event(field)?,
        ExprNode::State { slot } => ctx.state(slot)?,
        ExprNode::StateMap { slot, keys } => {
            let keys = keys.iter().map(ev).collect::<Result<Vec<_>, _>>()?;
            ctx.state_map(slot, &keys)?
        }
        ExprNode::Envelope { value, .. } => Value::Decimal(literal(value)?),
        ExprNode::Platform { read } => ctx.platform(*read)?,
        ExprNode::LegFill { leg, tag, field } => ctx.leg_fill(leg, tag.as_deref(), *field)?,
        ExprNode::Neg { arg } => Value::Decimal(-num(arg)?),
        ExprNode::Not { arg } => Value::Bool(!boolean(arg)?),
        ExprNode::Add { lhs, rhs } => Value::Decimal(num(lhs)? + num(rhs)?),
        ExprNode::Sub { lhs, rhs } => Value::Decimal(num(lhs)? - num(rhs)?),
        ExprNode::Mul { lhs, rhs } => Value::Decimal(num(lhs)? * num(rhs)?),
        ExprNode::Div { lhs, rhs } => {
            let l = num(lhs)?;
            let r = num(rhs)?;
            if r.is_zero() {
                Value::Decimal(ctx.division_by_zero().ok_or(EvalError::DivisionByZero)?)
            } else {
                Value::Decimal(l / r)
            }
        }
        ExprNode::Lt { lhs, rhs } => cmp(strategy_compiler::ir::Comparator::Lt, &ev(lhs)?, &ev(rhs)?)?,
        ExprNode::Le { lhs, rhs } => cmp(strategy_compiler::ir::Comparator::Le, &ev(lhs)?, &ev(rhs)?)?,
        ExprNode::Gt { lhs, rhs } => cmp(strategy_compiler::ir::Comparator::Gt, &ev(lhs)?, &ev(rhs)?)?,
        ExprNode::Ge { lhs, rhs } => cmp(strategy_compiler::ir::Comparator::Ge, &ev(lhs)?, &ev(rhs)?)?,
        ExprNode::Eq { lhs, rhs } => cmp(strategy_compiler::ir::Comparator::Eq, &ev(lhs)?, &ev(rhs)?)?,
        ExprNode::Ne { lhs, rhs } => cmp(strategy_compiler::ir::Comparator::Ne, &ev(lhs)?, &ev(rhs)?)?,
        // Short-circuit like the source language.
        ExprNode::And { lhs, rhs } => Value::Bool(boolean(lhs)? && boolean(rhs)?),
        ExprNode::Or { lhs, rhs } => Value::Bool(boolean(lhs)? || boolean(rhs)?),
        ExprNode::If { arms, else_value } => {
            for arm in arms {
                if boolean(&arm.condition)? {
                    return ev(&arm.value);
                }
            }
            ev(else_value)?
        }
        ExprNode::Call { builtin, args } => {
            let args = args.iter().map(num).collect::<Result<Vec<_>, _>>()?;
            Value::Decimal(call(*builtin, &args)?)
        }
        ExprNode::Lookup { table, key } => {
            let key = num(key)?;
            let table = ctx.table(table)?;
            Value::Decimal(lookup(table, key)?)
        }
        ExprNode::Triggered { name } => Value::Bool(ctx.triggered(name)?),
        ExprNode::Reduce { reducer, slot, predicate } => {
            let window = ctx.window(slot)?;
            Value::Decimal(reduce(*reducer, &window, predicate.as_deref(), ctx)?)
        }
        ExprNode::Entry => Value::Decimal(entry.ok_or(EvalError::Unsupported("entry outside count_where"))?),
    })
}

fn literal(s: &str) -> Result<Decimal, EvalError> {
    parse_decimal(s).ok_or_else(|| EvalError::BadLiteral(s.to_string()))
}

fn cmp(c: strategy_compiler::ir::Comparator, l: &Value, r: &Value) -> Result<Value, EvalError> {
    compare(c, l, r)
        .map(Value::Bool)
        .ok_or(EvalError::TypeMismatch("comparison across value shapes"))
}

/// The math builtins over exact arguments. Arity was checked by the parser; a
/// wrong count here is a program-format violation and reads as a mismatch.
fn call(builtin: Builtin, args: &[Decimal]) -> Result<Decimal, EvalError> {
    let arg = |i: usize| args.get(i).copied().ok_or(EvalError::TypeMismatch("builtin arity"));
    Ok(match builtin {
        Builtin::Abs => arg(0)?.abs(),
        Builtin::Min => arg(0)?.min(arg(1)?),
        Builtin::Max => arg(0)?.max(arg(1)?),
        Builtin::Clamp => {
            let (x, lo, hi) = (arg(0)?, arg(1)?, arg(2)?);
            x.max(lo).min(hi)
        }
        Builtin::Floor => arg(0)?.floor(),
        Builtin::Ceil => arg(0)?.ceil(),
        Builtin::Round => arg(0)?.round_dp_with_strategy(0, RoundingStrategy::MidpointAwayFromZero),
        Builtin::Exp => arg(0)?.checked_exp().unwrap_or(Decimal::ZERO),
        Builtin::Ln => {
            let x = arg(0)?;
            if x <= Decimal::ZERO { Decimal::ZERO } else { x.checked_ln().unwrap_or(Decimal::ZERO) }
        }
        Builtin::Sqrt => {
            let x = arg(0)?;
            if x < Decimal::ZERO { Decimal::ZERO } else { x.sqrt().unwrap_or(Decimal::ZERO) }
        }
        Builtin::Pow => {
            let (x, y) = (arg(0)?, arg(1)?);
            if x.is_zero() && y <= Decimal::ZERO {
                // 0^0 and 0^negative: pinned to 0.
                Decimal::ZERO
            } else if x < Decimal::ZERO && y.fract() != Decimal::ZERO {
                // Negative base, fractional exponent: no real result.
                Decimal::ZERO
            } else {
                x.checked_powd(y).unwrap_or(Decimal::ZERO)
            }
        }
        Builtin::NormCdf => {
            // The library's series expansion overflows (and panics) for a
            // large |x|; beyond ±8 the tail is under 1e-15, so the input is
            // clamped there. A panic here would take the platform task,
            // and every deployment's connection, down with it.
            let x = arg(0)?.clamp(NORM_CDF_MIN, NORM_CDF_MAX);
            x.norm_cdf()
        }
    })
}

/// `lookup`: linear interpolation over an ascending key column, clamped to
/// the end values outside the range.
fn lookup(table: &TableDef, key: Decimal) -> Result<Decimal, EvalError> {
    let keys = table.keys.iter().map(|k| literal(k)).collect::<Result<Vec<_>, _>>()?;
    let values = table.values.iter().map(|v| literal(v)).collect::<Result<Vec<_>, _>>()?;
    if keys.is_empty() || keys.len() != values.len() {
        return Err(EvalError::BadLiteral("malformed table".into()));
    }
    if key <= keys[0] {
        return Ok(values[0]);
    }
    let last = keys.len() - 1;
    if key >= keys[last] {
        return Ok(values[last]);
    }
    // First index whose key is >= the lookup key; 1..=last by the checks above.
    let hi = keys.iter().position(|k| *k >= key).unwrap_or(last);
    if keys[hi] == key {
        return Ok(values[hi]);
    }
    let lo = hi - 1;
    Ok(match table.interpolation {
        Interpolation::Linear => {
            let span = keys[hi] - keys[lo];
            if span.is_zero() {
                values[lo]
            } else {
                values[lo] + (values[hi] - values[lo]) * (key - keys[lo]) / span
            }
        }
    })
}

/// Window reducers. Empty window: `0` for every reducer (pinned).
fn reduce(
    reducer: Reducer,
    window: &[Decimal],
    predicate: Option<&ExprNode>,
    ctx: &dyn EvalCtx,
) -> Result<Decimal, EvalError> {
    Ok(match reducer {
        Reducer::Count => Decimal::from(window.len()),
        Reducer::Sum => window.iter().copied().sum(),
        Reducer::MaxOf => window.iter().copied().max().unwrap_or(Decimal::ZERO),
        Reducer::MinOf => window.iter().copied().min().unwrap_or(Decimal::ZERO),
        Reducer::Oldest => window.first().copied().unwrap_or(Decimal::ZERO),
        Reducer::Newest => window.last().copied().unwrap_or(Decimal::ZERO),
        Reducer::CountWhere => {
            let predicate = predicate.ok_or(EvalError::TypeMismatch("count_where without predicate"))?;
            let mut n = 0u64;
            for &element in window {
                let holds = eval_inner(predicate, ctx, Some(element))?
                    .as_bool()
                    .ok_or(EvalError::TypeMismatch("count_where predicate must be boolean"))?;
                if holds {
                    n += 1;
                }
            }
            Decimal::from(n)
        }
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// A context over a flat map of signal fields and params, dividing by
    /// zero to zero. Reused by the interpreter tests.
    #[derive(Default)]
    pub(crate) struct MapCtx {
        pub signals: BTreeMap<(String, String), Value>,
        pub params: BTreeMap<String, Value>,
        pub tables: BTreeMap<String, TableDef>,
        pub windows: BTreeMap<String, Vec<Decimal>>,
        pub div_zero_fails: bool,
    }

    impl EvalCtx for MapCtx {
        fn signal(&self, signal_id: &str, field: &str) -> Result<Value, EvalError> {
            self.signals
                .get(&(signal_id.to_string(), field.to_string()))
                .cloned()
                .ok_or_else(|| EvalError::MissingSignalField {
                    signal_id: signal_id.into(),
                    field: field.into(),
                })
        }
        fn param(&self, name: &str) -> Result<Value, EvalError> {
            self.params.get(name).cloned().ok_or_else(|| EvalError::UnknownParam(name.into()))
        }
        fn division_by_zero(&self) -> Option<Decimal> {
            if self.div_zero_fails { None } else { Some(Decimal::ZERO) }
        }
        fn table(&self, name: &str) -> Result<&TableDef, EvalError> {
            self.tables.get(name).ok_or_else(|| EvalError::UnknownTable(name.into()))
        }
        fn window(&self, slot: &str) -> Result<Vec<Decimal>, EvalError> {
            self.windows.get(slot).cloned().ok_or_else(|| EvalError::UnknownStateSlot(slot.into()))
        }
    }

    pub(crate) fn node(json: serde_json::Value) -> ExprNode {
        serde_json::from_value(json).expect("expr node json")
    }

    fn d(s: &str) -> Decimal {
        parse_decimal(s).unwrap()
    }

    fn eval_num(json: serde_json::Value, ctx: &MapCtx) -> Decimal {
        eval(&node(json), ctx).unwrap().as_decimal().unwrap()
    }

    #[test]
    fn arithmetic_is_exact() {
        let ctx = MapCtx::default();
        let n = serde_json::json!({"op": "+", "lhs": {"op": "decimal", "value": "0.1"}, "rhs": {"op": "decimal", "value": "0.2"}});
        assert_eq!(eval_num(n, &ctx), d("0.3"));
    }

    #[test]
    fn division_by_zero_follows_context() {
        let n = node(serde_json::json!({"op": "/", "lhs": {"op": "decimal", "value": "1"}, "rhs": {"op": "decimal", "value": "0"}}));
        let body = MapCtx::default();
        assert_eq!(eval(&n, &body).unwrap(), Value::zero());
        let trigger = MapCtx { div_zero_fails: true, ..Default::default() };
        assert_eq!(eval(&n, &trigger), Err(EvalError::DivisionByZero));
    }

    #[test]
    fn builtins_pinned_totality() {
        let c = |b: &str, args: &[&str]| {
            let args: Vec<_> = args.iter().map(|a| serde_json::json!({"op": "decimal", "value": a})).collect();
            eval_num(serde_json::json!({"op": "call", "builtin": b, "args": args}), &MapCtx::default())
        };
        assert_eq!(c("clamp", &["1.5", "0", "1"]), d("1"));
        assert_eq!(c("clamp", &["-1", "0", "1"]), d("0"));
        assert_eq!(c("round", &["2.5"]), d("3"));
        assert_eq!(c("round", &["-2.5"]), d("-3"));
        assert_eq!(c("floor", &["-2.5"]), d("-3"));
        assert_eq!(c("ceil", &["2.1"]), d("3"));
        assert_eq!(c("abs", &["-4"]), d("4"));
        assert_eq!(c("min", &["1", "2"]), d("1"));
        assert_eq!(c("max", &["1", "2"]), d("2"));
        assert_eq!(c("ln", &["0"]), Decimal::ZERO);
        assert_eq!(c("ln", &["-3"]), Decimal::ZERO);
        assert_eq!(c("sqrt", &["-1"]), Decimal::ZERO);
        assert_eq!(c("sqrt", &["9"]), d("3"));
        assert_eq!(c("pow", &["2", "10"]), d("1024"));
        assert_eq!(c("pow", &["0", "0"]), Decimal::ZERO);
        assert_eq!(c("pow", &["-8", "0.5"]), Decimal::ZERO);
        assert_eq!(c("exp", &["0"]), d("1"));
        assert_eq!(c("exp", &["100000"]), Decimal::ZERO, "overflow pins to zero");
        let half = c("norm_cdf", &["0"]);
        assert!((half - d("0.5")).abs() < d("0.000001"));
        // Far tails: the library overflows (and would panic) past ±8; the
        // clamp keeps them total. A round's last second produces z-scores
        // in the hundreds.
        for x in ["8", "50", "703", "1000000"] {
            let hi = c("norm_cdf", &[x]);
            assert!(hi > d("0.999999") && hi <= Decimal::ONE, "norm_cdf({x}) = {hi}");
            let lo = c("norm_cdf", &[&format!("-{x}")]);
            assert!(lo >= Decimal::ZERO && lo < d("0.000001"), "norm_cdf(-{x}) = {lo}");
        }
        // ln(e) ≈ 1 within the maths feature's tolerance.
        let one = c("ln", &["2.718281828459045"]);
        assert!((one - Decimal::ONE).abs() < d("0.000001"));
    }

    #[test]
    fn lookup_interpolates_and_clamps() {
        let mut ctx = MapCtx::default();
        ctx.tables.insert(
            "curve".into(),
            TableDef {
                keys: vec!["-10".into(), "0".into(), "10".into()],
                values: vec!["0.3".into(), "0.5".into(), "0.7".into()],
                interpolation: Interpolation::Linear,
            },
        );
        let at = |k: &str| eval_num(serde_json::json!({"op": "lookup", "table": "curve", "key": {"op": "decimal", "value": k}}), &ctx);
        assert_eq!(at("-100"), d("0.3"));
        assert_eq!(at("100"), d("0.7"));
        assert_eq!(at("0"), d("0.5"));
        assert_eq!(at("5"), d("0.6"));
        assert_eq!(at("-5"), d("0.4"));
    }

    #[test]
    fn reducers_and_count_where() {
        let mut ctx = MapCtx::default();
        ctx.windows.insert("w".into(), vec![d("1"), d("5"), d("3")]);
        ctx.windows.insert("empty".into(), vec![]);
        let r = |reducer: &str, slot: &str| eval_num(serde_json::json!({"op": "reduce", "reducer": reducer, "slot": slot}), &ctx);
        assert_eq!(r("count", "w"), d("3"));
        assert_eq!(r("sum", "w"), d("9"));
        assert_eq!(r("max_of", "w"), d("5"));
        assert_eq!(r("min_of", "w"), d("1"));
        assert_eq!(r("oldest", "w"), d("1"));
        assert_eq!(r("newest", "w"), d("3"));
        for reducer in ["count", "sum", "max_of", "min_of", "oldest", "newest"] {
            assert_eq!(r(reducer, "empty"), Decimal::ZERO, "{reducer} over empty");
        }
        let cw = eval_num(
            serde_json::json!({"op": "reduce", "reducer": "count_where", "slot": "w",
                "predicate": {"op": ">", "lhs": {"op": "entry"}, "rhs": {"op": "decimal", "value": "2"}}}),
            &ctx,
        );
        assert_eq!(cw, d("2"));
    }

    #[test]
    fn if_and_boolean_ops() {
        let ctx = MapCtx::default();
        let v = eval(
            &node(serde_json::json!({"op": "if",
                "arms": [{"condition": {"op": "and", "lhs": {"op": "bool", "value": true}, "rhs": {"op": "not", "arg": {"op": "bool", "value": false}}}, "value": {"op": "decimal", "value": "7"}}],
                "else": {"op": "decimal", "value": "0"}})),
            &ctx,
        )
        .unwrap();
        assert_eq!(v, Value::Decimal(d("7")));
    }

    #[test]
    fn missing_signal_is_an_error() {
        let ctx = MapCtx::default();
        let err = eval(&node(serde_json::json!({"op": "signal", "signal_id": "s", "field": "f"})), &ctx).unwrap_err();
        assert!(matches!(err, EvalError::MissingSignalField { .. }));
    }
}
