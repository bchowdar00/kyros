//! Declaration diagnostics (P1): duplicate names, reserved names, and registry-id
//! resolution failures — everything table construction can see without an AST.

use std::collections::BTreeMap;

use crate::ir::{ParamValue, Registry, SignedDecimalStr, Strategy, TypeKind};

use crate::parse::reserved;

use super::diag::{DiagCode, DiagPath, Diagnostic};
use super::tables::{InstrumentEntry, SignalEntry, Tables};

/// Emit every declaration diagnostic. Order of emission is document order per
/// construct family; the assembler's sort makes the final order authoritative.
pub fn check(
    strategy: &Strategy,
    registry: &Registry,
    tables: &Tables,
    diagnostics: &mut Vec<Diagnostic>,
) {
    duplicates(strategy, diagnostics);
    reserved_names(strategy, diagnostics);
    unresolved_ids(strategy, registry, tables, diagnostics);
    param_defaults(strategy, diagnostics);
}

/// Duplicate names in the array-shaped sections (triggers, legs, state slots) and
/// among block names within each leg. Anchored at the SECOND occurrence; the message
/// names the first. (Name-keyed maps reject duplicates at deserialization.)
fn duplicates(strategy: &Strategy, diagnostics: &mut Vec<Diagnostic>) {
    let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
    for (i, slot) in strategy.state.iter().enumerate() {
        if let Some(first) = seen.insert(slot.name().as_str(), i)
            && first != i
        {
            diagnostics.push(duplicate(
                "state slot",
                slot.name().as_str(),
                DiagPath::root().key("state").index(i).key("name"),
                format!("state[{first}]"),
            ));
        }
    }

    seen.clear();
    for (i, param) in strategy.params.iter().enumerate() {
        if let Some(first) = seen.insert(param.name.as_str(), i)
            && first != i
        {
            diagnostics.push(duplicate(
                "param",
                param.name.as_str(),
                DiagPath::root().key("params").index(i).key("name"),
                format!("params[{first}]"),
            ));
        }
    }

    seen.clear();
    for (i, trigger) in strategy.triggers.iter().enumerate() {
        if let Some(first) = seen.insert(trigger.name.as_str(), i)
            && first != i
        {
            diagnostics.push(duplicate(
                "trigger",
                trigger.name.as_str(),
                DiagPath::root().key("triggers").index(i).key("name"),
                format!("triggers[{first}]"),
            ));
        }
    }

    seen.clear();
    for (i, leg) in strategy.legs.iter().enumerate() {
        if let Some(first) = seen.insert(leg.name.as_str(), i)
            && first != i
        {
            diagnostics.push(duplicate(
                "leg",
                leg.name.as_str(),
                DiagPath::root().key("legs").index(i).key("name"),
                format!("legs[{first}]"),
            ));
        }
    }

    for (l, leg) in strategy.legs.iter().enumerate() {
        let mut blocks: BTreeMap<&str, usize> = BTreeMap::new();
        for (b, block) in leg.body.iter().enumerate() {
            if let Some(first) = blocks.insert(block.name.as_str(), b)
                && first != b
            {
                diagnostics.push(duplicate(
                    "block",
                    block.name.as_str(),
                    DiagPath::root()
                        .key("legs")
                        .index(l)
                        .key("body")
                        .index(b)
                        .key("name"),
                    format!("legs[{l}].body[{first}]"),
                ));
            }
        }
    }
}

fn duplicate(construct: &str, name: &str, path: DiagPath, first_site: String) -> Diagnostic {
    Diagnostic::rule(
        DiagCode::DuplicateName,
        path,
        format!("duplicate {construct} name `{name}` — first declared at {first_site}"),
    )
    .with_help("every name in a section must be unique; rename one of them")
}

/// Every declared identifier checked against the reserved set.
fn reserved_names(strategy: &Strategy, diagnostics: &mut Vec<Diagnostic>) {
    let mut check = |name: &str, construct: &str, path: DiagPath| {
        if let Some(category) = reserved::reserved_category(name) {
            diagnostics.push(
                Diagnostic::rule(
                    DiagCode::ReservedName,
                    path,
                    format!("`{name}` is {category} and cannot name {construct}"),
                )
                .with_help(
                    "rename it; reserved words are the grammar's keywords, namespace heads, \
                     builtin names, and 'stay'",
                ),
            );
        }
    };

    for (nick, _) in strategy.instruments.iter() {
        check(
            nick.as_str(),
            "an instrument",
            DiagPath::root().key("instruments").name(nick.as_str()),
        );
    }
    for (nick, _) in strategy.signals.iter() {
        check(
            nick.as_str(),
            "a signal",
            DiagPath::root().key("signals").name(nick.as_str()),
        );
    }
    for (i, slot) in strategy.state.iter().enumerate() {
        check(
            slot.name().as_str(),
            "a state slot",
            DiagPath::root().key("state").index(i).key("name"),
        );
    }
    for (i, param) in strategy.params.iter().enumerate() {
        check(
            param.name.as_str(),
            "a param",
            DiagPath::root().key("params").index(i).key("name"),
        );
    }
    if let Some(expressions) = &strategy.expressions {
        if let Some(named) = &expressions.named {
            for (name, _) in named.iter() {
                check(
                    name.as_str(),
                    "an expression",
                    DiagPath::root()
                        .key("expressions")
                        .key("named")
                        .name(name.as_str()),
                );
            }
        }
        if let Some(tables) = &expressions.tables {
            for (name, _) in tables.iter() {
                check(
                    name.as_str(),
                    "a table",
                    DiagPath::root()
                        .key("expressions")
                        .key("tables")
                        .name(name.as_str()),
                );
            }
        }
    }
    for (i, trigger) in strategy.triggers.iter().enumerate() {
        check(
            trigger.name.as_str(),
            "a trigger",
            DiagPath::root().key("triggers").index(i).key("name"),
        );
    }
    for (l, leg) in strategy.legs.iter().enumerate() {
        check(
            leg.name.as_str(),
            "a leg",
            DiagPath::root().key("legs").index(l).key("name"),
        );
        for (b, block) in leg.body.iter().enumerate() {
            let base = DiagPath::root().key("legs").index(l).key("body").index(b);
            check(block.name.as_str(), "a block", base.key("name"));
            // TAGS ARE EXEMPT from the reserved-name check (ruled 2026-07-20): tags
            // never enter expression space — they live in JSON positions only
            // (`place.tag`, `cancel.tag`, and any future tag-filter leaf values, which
            // are also JSON positions) — so a tag named `entry` cannot grammatically
            // collide with the `entry.*` namespace. The corpus relies on this.
        }
    }
}

/// Registry-id resolution failures, read off the already-built tables.
fn unresolved_ids(
    strategy: &Strategy,
    registry: &Registry,
    tables: &Tables,
    diagnostics: &mut Vec<Diagnostic>,
) {
    for (nick, id) in strategy.signals.iter() {
        if tables.signals.get(nick.as_str()) == Some(&SignalEntry::Unresolved) {
            diagnostics.push(
                Diagnostic::rule(
                    DiagCode::UnknownSignalId,
                    DiagPath::root().key("signals").name(nick.as_str()),
                    format!("signal id `{id}` is not in the registry"),
                )
                // Unconditional (ruled 2026-07-20): own-order streams are the one
                // ratified signal family with no registry entry yet, so every
                // resolution failure carries the pointer.
                .with_help(
                    "check the id against the registry's signal list; own-order signals \
                     are not yet supported",
                ),
            );
        }
    }

    for (nick, id) in strategy.instruments.iter() {
        if tables.instruments.get(nick.as_str()) == Some(&InstrumentEntry::Unresolved) {
            let mut diag = Diagnostic::rule(
                DiagCode::UnknownInstrumentId,
                DiagPath::root().key("instruments").name(nick.as_str()),
                format!("instrument id `{id}` is not a market or series in the registry"),
            );
            // A bare series id is the likely near-miss; say exactly how to spell it.
            if registry.series.iter().any(|s| s.series_id == *id) {
                diag = diag.with_help(format!(
                    "series instruments are referenced as `{id}.current`"
                ));
            }
            diagnostics.push(diag);
        }
    }
}

/// Every param default fits its declared type: a canonical signed decimal for
/// `decimal`, a canonical integer in `0..=255` for `u8`, a JSON boolean for `bool`,
/// any text for `string`, a declared variant for enums.
fn param_defaults(strategy: &Strategy, diagnostics: &mut Vec<Diagnostic>) {
    for (i, param) in strategy.params.iter().enumerate() {
        let path = DiagPath::root().key("params").index(i).key("default");
        let rendered = param.default.render();
        let problem: Option<String> = match (param.type_spec.kind(), &param.default) {
            (TypeKind::Decimal, ParamValue::Text(t)) => SignedDecimalStr::parse(t.clone())
                .err()
                .map(|_| format!("`{t}` is not a canonical decimal (e.g. `0.04`, `-1.5`, `12`)")),
            (TypeKind::U8, ParamValue::Text(t)) => match t.parse::<u16>() {
                Ok(n) if n <= 255 && SignedDecimalStr::parse(t.clone()).is_ok() => None,
                _ => Some(format!("`{t}` is not an integer in 0..=255")),
            },
            (TypeKind::Bool, ParamValue::Bool(_)) => None,
            (TypeKind::Bool, ParamValue::Text(t)) => {
                Some(format!("`{t}` is text — a bool default is JSON `true` or `false`"))
            }
            (TypeKind::Str, ParamValue::Text(_)) => None,
            (TypeKind::Enum(variants), ParamValue::Text(t)) => {
                if variants.iter().any(|v| v.as_str() == t) {
                    None
                } else {
                    let known: Vec<&str> = variants.iter().map(|v| v.as_str()).collect();
                    Some(format!(
                        "`{t}` is not a declared variant (variants: {})",
                        known.join(", ")
                    ))
                }
            }
            (
                TypeKind::Decimal | TypeKind::U8 | TypeKind::Str | TypeKind::Enum(_),
                ParamValue::Bool(_),
            ) => Some(format!(
                "`{rendered}` is a boolean — the declared type is not bool"
            )),
        };
        if let Some(problem) = problem {
            diagnostics.push(
                Diagnostic::rule(
                    DiagCode::ParamDefaultMismatch,
                    path,
                    format!(
                        "param `{}` default does not fit its type: {problem}",
                        param.name
                    ),
                )
                .with_help(
                    "the default is the value the strategy deploys with; write it in the \
                     declared type",
                ),
            );
        }
    }
}
