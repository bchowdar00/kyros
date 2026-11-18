//! Condition rows: one shared, deduplicated predicate per distinct trigger
//! condition, with the dependency set that drives evaluate-on-publish.
//!
//! A `TriggerSource::All | Any` is canonicalized into a [`RowKey`] (shape plus
//! leaves sorted by their canonical JSON). Identical predicates from different
//! deployments share one row and are evaluated once per affecting event.
//!
//! Param-valued leaves and `ExprNode::Param` reads make a row depend on a
//! connection's params. Such rows cannot be shared across connections (the
//! param values differ), so their key carries the connection id.

use std::collections::{BTreeMap, BTreeSet};

use rust_decimal::Decimal;
use strategy_compiler::ir::Comparator;
use strategy_compiler::program::{ExprNode, Leaf, LeafValue, TriggerSource};

use crate::automation::eval::{EvalCtx, EvalError, eval};
use crate::automation::signals::SignalStore;
use crate::automation::value::{Value, compare};

use super::ConnId;

/// What a row depends on: a whole signal (`Leaf::Always`) or one field.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Dependency {
    pub signal_id: String,
    /// `None` for whole-signal (`Always`) dependencies.
    pub field: Option<String>,
}

/// The dedup key of a row.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RowKey {
    /// Canonical JSON of the sorted predicate.
    pub canonical: String,
    /// Present iff the predicate reads params; such rows are per connection.
    pub conn: Option<ConnId>,
}

/// The predicate shape.
#[derive(Debug, Clone, PartialEq)]
enum Shape {
    All,
    Any,
}

/// One shared predicate.
#[derive(Debug, Clone)]
pub struct ConditionRow {
    pub key: RowKey,
    shape: Shape,
    leaves: Vec<Leaf>,
    pub dependencies: BTreeSet<Dependency>,
    /// The param names the predicate reads (empty for shared rows).
    pub params: BTreeSet<String>,
    /// How many times the predicate has been evaluated on publish.
    pub evaluations: u64,
}

impl ConditionRow {
    /// Build a row from a condition source. `None` for timer sources.
    pub fn from_source(source: &TriggerSource, conn: ConnId) -> Option<ConditionRow> {
        let (shape, leaves) = match source {
            TriggerSource::All { leaves } => (Shape::All, leaves),
            TriggerSource::Any { leaves } => (Shape::Any, leaves),
            _ => return None,
        };
        let mut sorted: Vec<(String, Leaf)> = leaves
            .iter()
            .map(|l| (serde_json::to_string(l).unwrap_or_default(), l.clone()))
            .collect();
        sorted.sort_by(|a, b| a.0.cmp(&b.0));
        let shape_tag = match shape {
            Shape::All => "all",
            Shape::Any => "any",
        };
        let canonical = format!(
            "{{\"{shape_tag}\":[{}]}}",
            sorted.iter().map(|(json, _)| json.as_str()).collect::<Vec<_>>().join(",")
        );
        let leaves: Vec<Leaf> = sorted.into_iter().map(|(_, l)| l).collect();
        let mut dependencies = BTreeSet::new();
        let mut params = BTreeSet::new();
        for leaf in &leaves {
            match leaf {
                Leaf::Always { signal_id } => {
                    dependencies.insert(Dependency { signal_id: signal_id.clone(), field: None });
                }
                Leaf::Field { signal_id, field, value, .. } => {
                    dependencies.insert(Dependency { signal_id: signal_id.clone(), field: Some(field.clone()) });
                    if let LeafValue::Param { name } = value {
                        params.insert(name.clone());
                    }
                }
                Leaf::Expr { expr, value, .. } => {
                    collect_reads(expr, &mut dependencies, &mut params);
                    if let LeafValue::Param { name } = value {
                        params.insert(name.clone());
                    }
                }
            }
        }
        let key = RowKey { canonical, conn: if params.is_empty() { None } else { Some(conn) } };
        Some(ConditionRow { key, shape, leaves, dependencies, params, evaluations: 0 })
    }

    /// Evaluate the predicate against the latest snapshots.
    pub fn evaluate(&self, store: &SignalStore, params: &BTreeMap<String, Value>) -> bool {
        let ctx = RowCtx { store, params };
        match self.shape {
            Shape::All => self.leaves.iter().all(|l| leaf_holds(l, store, &ctx)),
            Shape::Any => self.leaves.iter().any(|l| leaf_holds(l, store, &ctx)),
        }
    }
}

fn collect_reads(node: &ExprNode, deps: &mut BTreeSet<Dependency>, params: &mut BTreeSet<String>) {
    match node {
        ExprNode::Signal { signal_id, field } => {
            deps.insert(Dependency { signal_id: signal_id.clone(), field: Some(field.clone()) });
        }
        ExprNode::Param { name } => {
            params.insert(name.clone());
        }
        ExprNode::Neg { arg } | ExprNode::Not { arg } => collect_reads(arg, deps, params),
        ExprNode::Add { lhs, rhs }
        | ExprNode::Sub { lhs, rhs }
        | ExprNode::Mul { lhs, rhs }
        | ExprNode::Div { lhs, rhs }
        | ExprNode::Lt { lhs, rhs }
        | ExprNode::Le { lhs, rhs }
        | ExprNode::Gt { lhs, rhs }
        | ExprNode::Ge { lhs, rhs }
        | ExprNode::Eq { lhs, rhs }
        | ExprNode::Ne { lhs, rhs }
        | ExprNode::And { lhs, rhs }
        | ExprNode::Or { lhs, rhs } => {
            collect_reads(lhs, deps, params);
            collect_reads(rhs, deps, params);
        }
        ExprNode::If { arms, else_value } => {
            for arm in arms {
                collect_reads(&arm.condition, deps, params);
                collect_reads(&arm.value, deps, params);
            }
            collect_reads(else_value, deps, params);
        }
        ExprNode::Call { args, .. } => args.iter().for_each(|a| collect_reads(a, deps, params)),
        ExprNode::Lookup { key, .. } => collect_reads(key, deps, params),
        ExprNode::StateMap { keys, .. } => keys.iter().for_each(|k| collect_reads(k, deps, params)),
        ExprNode::Reduce { predicate: Some(p), .. } => collect_reads(p, deps, params),
        // Leaves without reads, and reads the trigger grammar forbids.
        _ => {}
    }
}

/// Evaluation context for a trigger leaf: snapshots and params only; division
/// by zero makes the leaf false.
struct RowCtx<'a> {
    store: &'a SignalStore,
    params: &'a BTreeMap<String, Value>,
}

impl EvalCtx for RowCtx<'_> {
    fn signal(&self, signal_id: &str, field: &str) -> Result<Value, EvalError> {
        self.store.field(signal_id, field).cloned().ok_or_else(|| EvalError::MissingSignalField {
            signal_id: signal_id.into(),
            field: field.into(),
        })
    }

    fn param(&self, name: &str) -> Result<Value, EvalError> {
        self.params.get(name).cloned().ok_or_else(|| EvalError::UnknownParam(name.into()))
    }

    fn division_by_zero(&self) -> Option<Decimal> {
        None
    }
}

fn leaf_holds(leaf: &Leaf, store: &SignalStore, ctx: &RowCtx<'_>) -> bool {
    match leaf {
        Leaf::Always { signal_id } => store.has_published(signal_id),
        Leaf::Field { signal_id, field, comparator, value } => {
            let Some(actual) = store.field(signal_id, field) else { return false };
            let Some(expected) = resolve_leaf_value(value, actual, ctx) else { return false };
            compare(*comparator, actual, &expected).unwrap_or(false)
        }
        Leaf::Expr { expr, comparator, value } => {
            let Ok(actual) = eval(expr, ctx) else { return false };
            let Some(expected) = resolve_leaf_value(value, &actual, ctx) else { return false };
            compare(*comparator, &actual, &expected).unwrap_or(false)
        }
    }
}

/// The leaf's comparison value, parsed in the shape of `actual` (literals) or
/// read from the connection's params.
fn resolve_leaf_value(value: &LeafValue, actual: &Value, ctx: &RowCtx<'_>) -> Option<Value> {
    match value {
        LeafValue::Literal { value } => Value::parse_like(value, actual),
        LeafValue::Param { name } => ctx.params.get(name).cloned(),
    }
}

/// Convenience for tests and stats: a leaf's comparator as text.
pub fn comparator_symbol(c: Comparator) -> &'static str {
    match c {
        Comparator::Eq => "==",
        Comparator::Ne => "!=",
        Comparator::Lt => "<",
        Comparator::Le => "<=",
        Comparator::Gt => ">",
        Comparator::Ge => ">=",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::automation::signals::tests::ab_store;
    use crate::automation::signals::{SignalEvent, SignalValue};

    fn source(json: serde_json::Value) -> TriggerSource {
        serde_json::from_value(json).unwrap()
    }

    fn field_leaf(signal: &str, field: &str, cmp: &str, value: &str) -> serde_json::Value {
        serde_json::json!({"kind": "field", "signal_id": signal, "field": field, "comparator": cmp,
            "value": {"kind": "literal", "value": value}})
    }

    #[test]
    fn key_is_order_independent_and_shape_sensitive() {
        let a = field_leaf("a", "x", ">", "0.3");
        let b = field_leaf("b", "kind", "==", "yes");
        let r1 = ConditionRow::from_source(&source(serde_json::json!({"kind": "all", "leaves": [a.clone(), b.clone()]})), 1).unwrap();
        let r2 = ConditionRow::from_source(&source(serde_json::json!({"kind": "all", "leaves": [b.clone(), a.clone()]})), 2).unwrap();
        let r3 = ConditionRow::from_source(&source(serde_json::json!({"kind": "any", "leaves": [a, b]})), 1).unwrap();
        assert_eq!(r1.key, r2.key, "different connections share a param-free row");
        assert_ne!(r1.key, r3.key);
        assert_eq!(
            r1.dependencies,
            [
                Dependency { signal_id: "a".into(), field: Some("x".into()) },
                Dependency { signal_id: "b".into(), field: Some("kind".into()) }
            ]
            .into_iter()
            .collect()
        );
    }

    #[test]
    fn always_depends_on_whole_signal_and_needs_a_publish() {
        let mut store = ab_store();
        let row = ConditionRow::from_source(&source(serde_json::json!({"kind": "all", "leaves": [{"kind": "always", "signal_id": "a"}]})), 1).unwrap();
        assert_eq!(row.dependencies, [Dependency { signal_id: "a".into(), field: None }].into_iter().collect());
        let params = BTreeMap::new();
        assert!(!row.evaluate(&store, &params));
        store.ingest(&SignalEvent::new("a", 1, [("x", SignalValue::decimal("1")), ("y", SignalValue::decimal("2"))])).unwrap();
        assert!(row.evaluate(&store, &params));
    }

    #[test]
    fn expr_leaf_is_exact_and_missing_field_is_false() {
        let mut store = ab_store();
        let row = ConditionRow::from_source(
            &source(serde_json::json!({"kind": "all", "leaves": [
                {"kind": "expr", "expr": {"op": "+", "lhs": {"op": "signal", "signal_id": "a", "field": "x"}, "rhs": {"op": "signal", "signal_id": "a", "field": "y"}},
                 "comparator": "==", "value": {"kind": "literal", "value": "0.3"}},
                field_leaf("b", "kind", "==", "yes")
            ]})),
            1,
        )
        .unwrap();
        let params = BTreeMap::new();
        assert!(!row.evaluate(&store, &params));
        store.ingest(&SignalEvent::new("a", 1, [("x", SignalValue::decimal("0.1")), ("y", SignalValue::decimal("0.2"))])).unwrap();
        assert!(!row.evaluate(&store, &params), "b has not published");
        store.ingest(&SignalEvent::new("b", 2, [("kind", SignalValue::text("yes"))])).unwrap();
        assert!(row.evaluate(&store, &params));
    }

    #[test]
    fn division_by_zero_in_a_leaf_is_false() {
        let mut store = ab_store();
        store.ingest(&SignalEvent::new("a", 1, [("x", SignalValue::decimal("1")), ("y", SignalValue::decimal("0"))])).unwrap();
        let row = ConditionRow::from_source(
            &source(serde_json::json!({"kind": "all", "leaves": [
                {"kind": "expr", "expr": {"op": "/", "lhs": {"op": "signal", "signal_id": "a", "field": "x"}, "rhs": {"op": "signal", "signal_id": "a", "field": "y"}},
                 "comparator": ">=", "value": {"kind": "literal", "value": "0"}}]})),
            1,
        )
        .unwrap();
        assert!(!row.evaluate(&store, &BTreeMap::new()));
    }

    #[test]
    fn param_rows_are_per_connection() {
        let mut store = ab_store();
        store.ingest(&SignalEvent::new("b", 1, [("kind", SignalValue::text("yes"))])).unwrap();
        let src = source(serde_json::json!({"kind": "all", "leaves": [
            {"kind": "field", "signal_id": "b", "field": "kind", "comparator": "==", "value": {"kind": "param", "name": "target"}}]}));
        let r1 = ConditionRow::from_source(&src, 1).unwrap();
        let r2 = ConditionRow::from_source(&src, 2).unwrap();
        assert_ne!(r1.key, r2.key);
        assert_eq!(r1.params, ["target".to_string()].into_iter().collect());
        let mut params = BTreeMap::new();
        params.insert("target".to_string(), Value::Text("yes".into()));
        assert!(r1.evaluate(&store, &params));
        params.insert("target".to_string(), Value::Text("no".into()));
        assert!(!r1.evaluate(&store, &params));
        assert!(!r1.evaluate(&store, &BTreeMap::new()), "unknown param is false");
    }
}
