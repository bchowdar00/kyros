//! Trigger condition leaves against the registry: field existence, comparator
//! legality, and value typing.

use crate::ir::{
    Comparator, Condition, ConditionLeaf, ExprLeaf, FieldClass, FieldDef, FieldLeaf, LeafValue,
    Registry, Strategy, TriggerSource, TypeKind,
};

use crate::validate::diag::{DiagCode, DiagPath, Diagnostic};
use crate::validate::poison::Poisons;
use crate::validate::tables::Tables;

use super::{is_numeric, param_spec, signal_fields};

/// Check every condition leaf of every trigger.
pub fn check(
    strategy: &Strategy,
    registry: &Registry,
    tables: &Tables,
    poisons: &Poisons,
    diagnostics: &mut Vec<Diagnostic>,
) {
    for (t, trigger) in strategy.triggers.iter().enumerate() {
        let TriggerSource::Condition(condition) = &trigger.source else {
            continue;
        };
        let base = DiagPath::root().key("triggers").index(t).key("condition");
        let base_str = format!("triggers[{t}].condition");
        match condition {
            Condition::Leaf(leaf) => {
                check_leaf(
                    leaf,
                    base,
                    &base_str,
                    strategy,
                    registry,
                    tables,
                    poisons,
                    diagnostics,
                );
            }
            Condition::All(group) => {
                for (i, leaf) in group.all.iter().enumerate() {
                    check_leaf(
                        leaf,
                        base.clone().key("all").index(i),
                        &format!("{base_str}.all[{i}]"),
                        strategy,
                        registry,
                        tables,
                        poisons,
                        diagnostics,
                    );
                }
            }
            Condition::Any(group) => {
                for (i, leaf) in group.any.iter().enumerate() {
                    check_leaf(
                        leaf,
                        base.clone().key("any").index(i),
                        &format!("{base_str}.any[{i}]"),
                        strategy,
                        registry,
                        tables,
                        poisons,
                        diagnostics,
                    );
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn check_leaf(
    leaf: &ConditionLeaf,
    path: DiagPath,
    path_str: &str,
    strategy: &Strategy,
    registry: &Registry,
    tables: &Tables,
    poisons: &Poisons,
    diagnostics: &mut Vec<Diagnostic>,
) {
    match leaf {
        // A bare leaf states only identity; family A resolved it.
        ConditionLeaf::Bare(_) => {}
        ConditionLeaf::Field(field_leaf) => {
            field(field_leaf, path, strategy, registry, tables, diagnostics);
        }
        ConditionLeaf::Expr(expr_leaf) => {
            expr(
                expr_leaf,
                path,
                path_str,
                strategy,
                registry,
                tables,
                poisons,
                diagnostics,
            );
        }
    }
}

fn field(
    leaf: &FieldLeaf,
    path: DiagPath,
    strategy: &Strategy,
    registry: &Registry,
    tables: &Tables,
    diagnostics: &mut Vec<Diagnostic>,
) {
    // Undeclared or unresolved nickname: the root cause is already reported.
    let Some(fields) = signal_fields(leaf.signal.as_str(), registry, tables) else {
        return;
    };
    let Some(def) = fields.iter().find(|f| f.name == leaf.field) else {
        diagnostics.push(unknown_field(
            leaf.signal.as_str(),
            leaf.field.as_str(),
            fields,
            path.key("field"),
        ));
        return;
    };

    let ordering = matches!(
        leaf.comparator,
        Comparator::Lt | Comparator::Le | Comparator::Gt | Comparator::Ge
    );
    let class = def.field_type.class();
    if ordering && !is_numeric(&def.field_type) {
        let (what, help) = match &class {
            FieldClass::Str => ("a string", "use == or != against a {\"str\": ...} value"),
            FieldClass::Enum(_) | FieldClass::Numeric => {
                ("an enum", "use == or != against a variant")
            }
        };
        diagnostics.push(
            Diagnostic::rule(
                DiagCode::ComparatorTypeMismatch,
                path.clone().key("comparator"),
                format!(
                    "field `{}` is {what}; ordering comparators require a numeric field",
                    leaf.field
                ),
            )
            .with_help(help),
        );
    }

    // Numeric fields (whether a decimal fits the integer width is a lowering
    // concern, not a leaf-typing one), enum fields, string fields — against a
    // literal of each form or a param of a declared type.
    let mismatch = |what: String, help: &str| {
        Diagnostic::rule(
            DiagCode::ValueTypeMismatch,
            path.clone().key("value"),
            what,
        )
        .with_help(help)
    };
    let field_name = leaf.field.as_str();
    match (&class, &leaf.value) {
        (FieldClass::Numeric, LeafValue::Decimal(_)) => {}
        (FieldClass::Numeric, LeafValue::Variant(variant)) => diagnostics.push(mismatch(
            format!("`{variant}` is not a number — field `{field_name}` is numeric"),
            "compare a numeric field against a canonical decimal",
        )),
        (FieldClass::Numeric, LeafValue::Str(text)) => diagnostics.push(mismatch(
            format!("{text:?} is text — field `{field_name}` is numeric"),
            "compare a numeric field against a canonical decimal",
        )),
        (FieldClass::Enum(variants), LeafValue::Variant(variant)) => {
            if !variants.iter().any(|v| v == variant.as_str()) {
                diagnostics.push(mismatch(
                    format!(
                        "`{variant}` is not a variant of field `{field_name}` (expected one of: {})",
                        variants.join(", ")
                    ),
                    "leaf values write enum variants plain, without quotes",
                ));
            }
        }
        (FieldClass::Enum(variants), LeafValue::Decimal(decimal)) => {
            diagnostics.push(mismatch(
                format!(
                    "`{}` is a number — field `{field_name}` is an enum (variants: {})",
                    decimal.as_str(),
                    variants.join(", ")
                ),
                "compare an enum field against one of its variants",
            ));
        }
        (FieldClass::Enum(variants), LeafValue::Str(text)) => diagnostics.push(mismatch(
            format!(
                "{text:?} is text — field `{field_name}` is an enum (variants: {})",
                variants.join(", ")
            ),
            "compare an enum field against one of its variants, written plain",
        )),
        (FieldClass::Str, LeafValue::Str(_)) => {}
        (FieldClass::Str, LeafValue::Variant(word)) => diagnostics.push(mismatch(
            format!("`{word}` is a bare word — field `{field_name}` is a string"),
            "write string values as {\"str\": \"...\"}",
        )),
        (FieldClass::Str, LeafValue::Decimal(decimal)) => diagnostics.push(mismatch(
            format!(
                "`{}` is a number — field `{field_name}` is a string",
                decimal.as_str()
            ),
            "write string values as {\"str\": \"...\"}",
        )),
        (class, LeafValue::Param(param)) => {
            // Undeclared: family A reported it.
            let Some(spec) = param_spec(param.as_str(), strategy, tables) else {
                return;
            };
            let fits = match (class, spec.kind()) {
                (FieldClass::Numeric, TypeKind::Decimal | TypeKind::U8) => true,
                (FieldClass::Enum(variants), TypeKind::Enum(declared)) => {
                    declared.iter().all(|v| variants.iter().any(|f| f == v.as_str()))
                }
                // A bool param fits a bool field (class: the `true`/`false` enum).
                (FieldClass::Enum(variants), TypeKind::Bool) => {
                    variants.iter().any(|v| v == "true") && variants.iter().any(|v| v == "false")
                }
                (FieldClass::Str, TypeKind::Str) => true,
                (
                    FieldClass::Numeric | FieldClass::Enum(_) | FieldClass::Str,
                    TypeKind::Decimal | TypeKind::U8 | TypeKind::Bool | TypeKind::Str | TypeKind::Enum(_),
                ) => false,
            };
            if !fits {
                diagnostics.push(mismatch(
                    format!(
                        "param `{param}` is not the type of field `{field_name}` (a param compared \
                         against a field must be declared in the field's type; enum params \
                         must use only the field's variants)"
                    ),
                    "declare the param in the field's type, or compare a literal",
                ));
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn expr(
    leaf: &ExprLeaf,
    path: DiagPath,
    path_str: &str,
    strategy: &Strategy,
    registry: &Registry,
    tables: &Tables,
    poisons: &Poisons,
    diagnostics: &mut Vec<Diagnostic>,
) {
    // Expr leaves are arithmetic over numeric fields, so the compared value must be
    // numeric: a decimal, or a decimal/u8 param. This holds even when the expression
    // string itself failed to parse.
    let non_numeric: Option<String> = match &leaf.value {
        LeafValue::Decimal(_) => None,
        LeafValue::Variant(variant) => Some(format!("`{variant}` is not a number")),
        LeafValue::Str(text) => Some(format!("{text:?} is text, not a number")),
        LeafValue::Param(param) => match param_spec(param.as_str(), strategy, tables) {
            // Undeclared: family A reported it.
            None => None,
            Some(spec) => match spec.kind() {
                TypeKind::Decimal | TypeKind::U8 => None,
                TypeKind::Bool | TypeKind::Str | TypeKind::Enum(_) => {
                    Some(format!("param `{param}` is not numeric"))
                }
            },
        },
    };
    if let Some(what) = non_numeric {
        diagnostics.push(
            Diagnostic::rule(
                DiagCode::ValueTypeMismatch,
                path.key("value"),
                format!("{what} — an expr leaf compares arithmetic against a decimal"),
            )
            .with_help("compare the expression against a canonical decimal or a decimal param"),
        );
    }
    // The expression's signal reads resolve and type-check with every other parsed
    // string (families A, B and G); its purity is `context::trigger_expr`'s rule.
    let _ = (path_str, registry, poisons);
}

fn unknown_field(signal: &str, field: &str, fields: &[FieldDef], path: DiagPath) -> Diagnostic {
    let declared: Vec<&str> = fields.iter().map(|f| f.name.as_str()).collect();
    Diagnostic::rule(
        DiagCode::UnknownField,
        path,
        format!("signal `{signal}` has no field `{field}`"),
    )
    .with_help(format!("declared fields: {}", declared.join(", ")))
}
