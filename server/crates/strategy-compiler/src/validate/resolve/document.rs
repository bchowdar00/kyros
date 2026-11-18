//! Resolution of names written in typed JSON: trigger-leaf signals, timer
//! instruments, arm entries, `then` and on-table leg destinations, intent
//! instruments, and cancel tags.

use crate::ir::{
    Condition, ConditionLeaf, DoItem, LeafValue, OnDest, OnTable, Strategy, TimerSpec,
    TriggerSource,
};

use crate::validate::diag::{DiagCode, DiagPath, Diagnostic};
use crate::validate::tables::Tables;

/// Check every JSON-side reference.
pub fn check(strategy: &Strategy, tables: &Tables, diagnostics: &mut Vec<Diagnostic>) {
    triggers(strategy, tables, diagnostics);
    legs(strategy, tables, diagnostics);
}

fn triggers(strategy: &Strategy, tables: &Tables, diagnostics: &mut Vec<Diagnostic>) {
    for (t, trigger) in strategy.triggers.iter().enumerate() {
        match &trigger.source {
            TriggerSource::Condition(condition) => {
                let base = DiagPath::root().key("triggers").index(t).key("condition");
                match condition {
                    Condition::Leaf(leaf) => leaf_signal(leaf, base, tables, diagnostics),
                    Condition::All(group) => {
                        for (i, leaf) in group.all.iter().enumerate() {
                            leaf_signal(
                                leaf,
                                base.clone().key("all").index(i),
                                tables,
                                diagnostics,
                            );
                        }
                    }
                    Condition::Any(group) => {
                        for (i, leaf) in group.any.iter().enumerate() {
                            leaf_signal(
                                leaf,
                                base.clone().key("any").index(i),
                                tables,
                                diagnostics,
                            );
                        }
                    }
                }
            }
            TriggerSource::Timer(timer) => {
                // Only the window clock names an instrument; `after` and `interval`
                // measure from leg entry.
                let instrument = match timer {
                    TimerSpec::WindowClock { instrument, .. } => instrument,
                    TimerSpec::After { .. } | TimerSpec::Interval { .. } => continue,
                };
                if !tables.instruments.contains_key(instrument.as_str()) {
                    diagnostics.push(unknown(
                        DiagCode::UnknownInstrument,
                        DiagPath::root()
                            .key("triggers")
                            .index(t)
                            .key("timer")
                            .key("instrument"),
                        "timer",
                        "instrument",
                        instrument.as_str(),
                        &declared_list(tables.instruments.keys()),
                    ));
                }
            }
        }
    }
}

fn leaf_signal(
    leaf: &ConditionLeaf,
    path: DiagPath,
    tables: &Tables,
    diagnostics: &mut Vec<Diagnostic>,
) {
    // Expr leaves name their signals inside the expression (resolved with every
    // other parsed string); only bare and field leaves carry a JSON `signal`.
    let signal = match leaf {
        ConditionLeaf::Bare(bare) => Some(&bare.signal),
        ConditionLeaf::Field(field) => Some(&field.signal),
        ConditionLeaf::Expr(_) => None,
    };
    if let Some(signal) = signal
        && !tables.signals.contains_key(signal.as_str())
    {
        diagnostics.push(unknown(
            DiagCode::UnknownSignal,
            path.clone().key("signal"),
            "trigger leaf",
            "signal",
            signal.as_str(),
            &declared_list(tables.signals.keys()),
        ));
    }
    let value = match leaf {
        ConditionLeaf::Field(field) => Some(&field.value),
        ConditionLeaf::Expr(expr) => Some(&expr.value),
        ConditionLeaf::Bare(_) => None,
    };
    if let Some(LeafValue::Param(param)) = value
        && !tables.params.contains_key(param.as_str())
    {
        diagnostics.push(unknown(
            DiagCode::UnknownParam,
            path.key("value"),
            "trigger leaf value",
            "param",
            param.as_str(),
            &declared_list(tables.params.keys()),
        ));
    }
}

fn legs(strategy: &Strategy, tables: &Tables, diagnostics: &mut Vec<Diagnostic>) {
    for (l, leg) in strategy.legs.iter().enumerate() {
        for (a, armed) in leg.arm.iter().enumerate() {
            if !tables.triggers.contains_key(armed.as_str()) {
                diagnostics.push(unknown(
                    DiagCode::UnknownTrigger,
                    DiagPath::root().key("legs").index(l).key("arm").index(a),
                    "arm list",
                    "trigger",
                    armed.as_str(),
                    &declared_list(tables.triggers.keys()),
                ));
            }
        }
        for (b, block) in leg.body.iter().enumerate() {
            let base = DiagPath::root().key("legs").index(l).key("body").index(b);
            if let Some(then) = &block.then
                && !tables.legs.contains_key(then.as_str())
            {
                diagnostics.push(unknown(
                    DiagCode::UnknownLeg,
                    base.clone().key("then"),
                    "transition",
                    "leg",
                    then.as_str(),
                    &declared_list(tables.legs.keys()),
                ));
            }
            for (d, item) in block.do_items.iter().enumerate() {
                let item_base = base.clone().key("do").index(d);
                match item {
                    DoItem::Statement(_) => {}
                    DoItem::Place(place_item) => {
                        let place = &place_item.place;
                        let place_base = item_base.key("place");
                        instrument(
                            place.instrument.as_str(),
                            place_base.clone(),
                            tables,
                            diagnostics,
                        );
                        if let Some(on) = &place.on {
                            on_table(on, place_base.key("on"), tables, diagnostics);
                        }
                    }
                    DoItem::Cancel(cancel_item) => {
                        let tag = cancel_item.cancel.tag.as_str();
                        if !tables.tags.contains(tag) {
                            diagnostics.push(
                                Diagnostic::rule(
                                    DiagCode::UnknownTag,
                                    item_base.key("cancel").key("tag"),
                                    format!(
                                        "cancel targets tag `{tag}` but no place intent declares it"
                                    ),
                                )
                                .with_help(format!(
                                    "declared tags: {}",
                                    declared_list(tables.tags.iter())
                                )),
                            );
                        }
                    }
                    DoItem::TargetPosition(target_item) => {
                        let target = &target_item.target_position;
                        let target_base = item_base.key("target_position");
                        instrument(
                            target.instrument.as_str(),
                            target_base.clone(),
                            tables,
                            diagnostics,
                        );
                        if let Some(on) = &target.on {
                            on_table(on, target_base.key("on"), tables, diagnostics);
                        }
                    }
                    DoItem::CancelReplace(replace_item) => {
                        // Its own tag self-declares (the replacement order carries it);
                        // only the instrument needs resolving here.
                        instrument(
                            replace_item.cancel_replace.instrument.as_str(),
                            item_base.key("cancel_replace"),
                            tables,
                            diagnostics,
                        );
                    }
                }
            }
        }
    }
}

fn instrument(nick: &str, base: DiagPath, tables: &Tables, diagnostics: &mut Vec<Diagnostic>) {
    if !tables.instruments.contains_key(nick) {
        diagnostics.push(unknown(
            DiagCode::UnknownInstrument,
            base.key("instrument"),
            "intent",
            "instrument",
            nick,
            &declared_list(tables.instruments.keys()),
        ));
    }
}

fn on_table(on: &OnTable, base: DiagPath, tables: &Tables, diagnostics: &mut Vec<Diagnostic>) {
    let keys: [(&'static str, &Option<OnDest>); 5] = [
        ("fill", &on.fill),
        ("partial", &on.partial),
        ("reject", &on.reject),
        ("cancel_ack", &on.cancel_ack),
        ("expiry", &on.expiry),
    ];
    for (key, dest) in keys {
        if let Some(OnDest::Leg(leg)) = dest
            && !tables.legs.contains_key(leg.as_str())
        {
            diagnostics.push(unknown(
                DiagCode::UnknownLeg,
                base.clone().key(key),
                "on-table destination",
                "leg",
                leg.as_str(),
                &declared_list(tables.legs.keys()),
            ));
        }
    }
}

fn unknown(
    code: DiagCode,
    path: DiagPath,
    site: &str,
    construct: &str,
    name: &str,
    declared: &str,
) -> Diagnostic {
    Diagnostic::rule(
        code,
        path,
        format!("{site} names undeclared {construct} `{name}`"),
    )
    .with_help(format!("declared {construct}s: {declared}"))
}

/// Comma-joined declared names for help lines. Sections are small by doctrine, so the
/// full list is always printable.
fn declared_list<'a>(names: impl Iterator<Item = &'a String>) -> String {
    let joined: Vec<&str> = names.map(String::as_str).collect();
    if joined.is_empty() {
        "(none)".to_string()
    } else {
        joined.join(", ")
    }
}
