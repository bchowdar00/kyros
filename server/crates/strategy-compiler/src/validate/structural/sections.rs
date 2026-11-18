//! Section shape rules: group sizes, constant-table shape, inline enum shape, map
//! bounds, and the sparse-in emptiness doctrine.

use crate::ir::{Condition, Strategy, TriggerSource, TypeKind, TypeSpec, TypeSpecObject};

use crate::validate::decimal::cmp_decimal;
use crate::validate::diag::{DiagCode, DiagPath, Diagnostic};

/// Run the section shape rules.
pub fn check(strategy: &Strategy, diagnostics: &mut Vec<Diagnostic>) {
    empty_sections(strategy, diagnostics);
    condition_groups(strategy, diagnostics);
    constant_tables(strategy, diagnostics);
    state_slots(strategy, diagnostics);
}

/// Required sections must not be empty; optional sections must not be PRESENT and
/// empty (sparse-in: omit, never write `{}` / `[]`).
fn empty_sections(strategy: &Strategy, diagnostics: &mut Vec<Diagnostic>) {
    let mut empty = |section: &'static str| {
        diagnostics.push(
            Diagnostic::rule(
                DiagCode::EmptySection,
                DiagPath::root().key(section),
                format!("`{section}` is empty"),
            )
            .with_help("a strategy declares at least one entry here"),
        );
    };
    if strategy.instruments.is_empty() {
        empty("instruments");
    }
    if strategy.signals.is_empty() {
        empty("signals");
    }
    if strategy.triggers.is_empty() {
        empty("triggers");
    }
    if strategy.legs.is_empty() {
        empty("legs");
    }
    if let Some(expressions) = &strategy.expressions {
        if expressions.named.as_ref().is_some_and(|n| n.is_empty()) {
            diagnostics.push(
                Diagnostic::rule(
                    DiagCode::EmptySection,
                    DiagPath::root().key("expressions").key("named"),
                    "`expressions.named` is present but empty",
                )
                .with_help("omit an empty section entirely (sparse-in)"),
            );
        }
        if expressions.tables.as_ref().is_some_and(|t| t.is_empty()) {
            diagnostics.push(
                Diagnostic::rule(
                    DiagCode::EmptySection,
                    DiagPath::root().key("expressions").key("tables"),
                    "`expressions.tables` is present but empty",
                )
                .with_help("omit an empty section entirely (sparse-in)"),
            );
        }
    }
}

/// `all`/`any` groups need at least two leaves; one leaf needs no group.
fn condition_groups(strategy: &Strategy, diagnostics: &mut Vec<Diagnostic>) {
    for (t, trigger) in strategy.triggers.iter().enumerate() {
        let TriggerSource::Condition(condition) = &trigger.source else {
            continue;
        };
        let (kind, len) = match condition {
            Condition::Leaf(_) => continue,
            Condition::All(group) => ("all", group.all.len()),
            Condition::Any(group) => ("any", group.any.len()),
        };
        if len < 2 {
            diagnostics.push(
                Diagnostic::rule(
                    DiagCode::GroupTooSmall,
                    DiagPath::root()
                        .key("triggers")
                        .index(t)
                        .key("condition")
                        .key(if kind == "all" { "all" } else { "any" }),
                    format!("an `{kind}` group needs at least 2 leaves"),
                )
                .with_help("a single leaf needs no group — write the leaf directly"),
            );
        }
    }
}

/// Constant tables: equal column lengths, at least two entries, keys strictly
/// increasing (exact decimal comparison, no floats).
fn constant_tables(strategy: &Strategy, diagnostics: &mut Vec<Diagnostic>) {
    let Some(tables) = strategy
        .expressions
        .as_ref()
        .and_then(|e| e.tables.as_ref())
    else {
        return;
    };
    for (name, table) in tables.iter() {
        let path = DiagPath::root()
            .key("expressions")
            .key("tables")
            .name(name.as_str());
        if table.keys.len() != table.values.len() {
            diagnostics.push(
                Diagnostic::rule(
                    DiagCode::TableLengthMismatch,
                    path.clone(),
                    format!(
                        "table `{name}` has {} keys but {} values",
                        table.keys.len(),
                        table.values.len()
                    ),
                )
                .with_help("every key needs exactly one value"),
            );
        }
        if table.keys.len() < 2 {
            diagnostics.push(
                Diagnostic::rule(
                    DiagCode::TableTooSmall,
                    path.clone(),
                    format!("table `{name}` has fewer than 2 entries"),
                )
                .with_help("a curve needs at least two points; a constant needs no table"),
            );
        }
        for pair in table.keys.windows(2) {
            if cmp_decimal(pair[0].as_str(), pair[1].as_str()) != std::cmp::Ordering::Less {
                diagnostics.push(
                    Diagnostic::rule(
                        DiagCode::TableKeysNotIncreasing,
                        path.clone().key("keys"),
                        format!(
                            "table `{name}` keys are not strictly increasing at `{}` -> `{}`",
                            pair[0].as_str(),
                            pair[1].as_str()
                        ),
                    )
                    .with_help("sort the keys ascending and remove duplicates"),
                );
            }
        }
    }
}

/// The largest window a program may declare (the runtime's ring buffer is allocated
/// at deploy).
pub const WINDOW_CAPACITY_MAX: u32 = 4096;

/// Inline enum shape (>= 2 unique variants), map bounds (`max_keys` >= 1) and window
/// shape (numeric, capacity in range), everywhere a `TypeSpec` appears.
fn state_slots(strategy: &Strategy, diagnostics: &mut Vec<Diagnostic>) {
    for (i, slot) in strategy.state.iter().enumerate() {
        let base = DiagPath::root().key("state").index(i);
        match slot {
            crate::ir::StateSlot::Scalar { value_type, .. } => {
                enum_shape(value_type, base.key("value_type"), diagnostics);
            }
            crate::ir::StateSlot::Window {
                value_type,
                capacity,
                ..
            } => {
                if !matches!(value_type.kind(), TypeKind::U8 | TypeKind::Decimal) {
                    diagnostics.push(
                        Diagnostic::rule(
                            DiagCode::WindowShape,
                            base.clone().key("value_type"),
                            "a window holds numbers — its value_type is u8 or decimal",
                        )
                        .with_help("keep symbolic values in a scalar or map slot"),
                    );
                }
                if !(1..=WINDOW_CAPACITY_MAX).contains(capacity) {
                    diagnostics.push(
                        Diagnostic::rule(
                            DiagCode::WindowShape,
                            base.key("capacity"),
                            format!(
                                "a window's capacity is 1 to {WINDOW_CAPACITY_MAX}; {capacity} is out of range"
                            ),
                        )
                        .with_help("the runtime allocates the ring buffer up front; keep windows small"),
                    );
                }
            }
            crate::ir::StateSlot::Map {
                key_types,
                value_type,
                max_keys,
                ..
            } => {
                for (k, key_type) in key_types.iter().enumerate() {
                    enum_shape(
                        key_type,
                        base.clone().key("key_types").index(k),
                        diagnostics,
                    );
                }
                enum_shape(value_type, base.clone().key("value_type"), diagnostics);
                if *max_keys == 0 {
                    diagnostics.push(
                        Diagnostic::rule(
                            DiagCode::MaxKeysZero,
                            base.key("max_keys"),
                            "a map with `max_keys` 0 can never hold an entry",
                        )
                        .with_help("declare the map's real capacity bound"),
                    );
                }
            }
        }
    }
}

fn enum_shape(spec: &TypeSpec, path: DiagPath, diagnostics: &mut Vec<Diagnostic>) {
    let TypeSpec::Object(TypeSpecObject::Enum { variants }) = spec else {
        return;
    };
    if variants.len() < 2 {
        diagnostics.push(
            Diagnostic::rule(
                DiagCode::EnumTooSmall,
                path.clone(),
                "an enum needs at least 2 variants",
            )
            .with_help("a one-variant enum is a constant; drop the slot or add variants"),
        );
    }
    let mut seen = std::collections::BTreeSet::new();
    for variant in variants {
        if !seen.insert(variant.as_str()) {
            diagnostics.push(
                Diagnostic::rule(
                    DiagCode::DuplicateVariant,
                    path.clone(),
                    format!("variant `{variant}` is declared twice"),
                )
                .with_help("variants are a set; remove the repeat"),
            );
        }
    }
}
