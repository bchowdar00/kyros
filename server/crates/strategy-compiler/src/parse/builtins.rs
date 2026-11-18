//! The closed builtin set: one const table carrying name, arity, and category.
//!
//! Single source of truth — the parser's arity check, the "did you mean" suggestion,
//! and the future validator's category rules all read from here.

/// Every builtin in the v0.2 grammar (ir-spec.md §13 as amended). A closed set: calling
/// anything else is a parse error — except the superseded v0.1 names (`crossed`,
/// `fired`), which get dedicated repair-steering diagnostics in the parser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Builtin {
    /// `abs(x)`
    Abs,
    /// `min(a, b)` — strictly binary.
    Min,
    /// `max(a, b)` — strictly binary.
    Max,
    /// `clamp(x, lo, hi)` — the guard-rail primitive.
    Clamp,
    /// `floor(x)`
    Floor,
    /// `ceil(x)`
    Ceil,
    /// `round(x)`
    Round,
    /// `exp(x)`
    Exp,
    /// `ln(x)`
    Ln,
    /// `sqrt(x)`
    Sqrt,
    /// `pow(x, y)`
    Pow,
    /// `norm_cdf(x)`
    NormCdf,
    /// `lookup(table.NAME, key)` — first argument must be a `table.` ref (syntax rule).
    Lookup,
    /// `count(w)`
    Count,
    /// `sum(w)`
    Sum,
    /// `max_of(w)`
    MaxOf,
    /// `min_of(w)`
    MinOf,
    /// `oldest(w)`
    Oldest,
    /// `newest(w)`
    Newest,
    /// `count_where(w, pred)` — `pred` reads `entry.value`.
    CountWhere,
    /// `triggered(NAME)` — the one cause primitive: true iff the named trigger is among
    /// what fired this activation. Takes a BARE trigger name (the explicit carve-out
    /// from no-bare-identifiers; the name resolves against the triggers section later,
    /// in the validator).
    Triggered,
}

/// Coarse grouping the validator reuses (guard-position rules, reducer restrictions).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltinCategory {
    /// Plain math over decimals.
    Math,
    /// Analytic functions with totality-policy domain edges.
    Analytic,
    /// Table lookup.
    Table,
    /// Window reducers: O(capacity) scans; `count_where` takes the `entry.value` lambda.
    Reducer,
    /// The guard-position cause primitive (`triggered`).
    Trigger,
}

/// One row of the builtin table.
#[derive(Debug, Clone, Copy)]
pub struct BuiltinInfo {
    /// The builtin.
    pub builtin: Builtin,
    /// Its source spelling.
    pub name: &'static str,
    /// Its fixed arity (every builtin has one; there are no variadic functions).
    pub arity: usize,
    /// Its category.
    pub category: BuiltinCategory,
}

/// The complete v1 inventory, in §6 order.
pub const BUILTINS: &[BuiltinInfo] = &[
    BuiltinInfo {
        builtin: Builtin::Abs,
        name: "abs",
        arity: 1,
        category: BuiltinCategory::Math,
    },
    BuiltinInfo {
        builtin: Builtin::Min,
        name: "min",
        arity: 2,
        category: BuiltinCategory::Math,
    },
    BuiltinInfo {
        builtin: Builtin::Max,
        name: "max",
        arity: 2,
        category: BuiltinCategory::Math,
    },
    BuiltinInfo {
        builtin: Builtin::Clamp,
        name: "clamp",
        arity: 3,
        category: BuiltinCategory::Math,
    },
    BuiltinInfo {
        builtin: Builtin::Floor,
        name: "floor",
        arity: 1,
        category: BuiltinCategory::Math,
    },
    BuiltinInfo {
        builtin: Builtin::Ceil,
        name: "ceil",
        arity: 1,
        category: BuiltinCategory::Math,
    },
    BuiltinInfo {
        builtin: Builtin::Round,
        name: "round",
        arity: 1,
        category: BuiltinCategory::Math,
    },
    BuiltinInfo {
        builtin: Builtin::Exp,
        name: "exp",
        arity: 1,
        category: BuiltinCategory::Analytic,
    },
    BuiltinInfo {
        builtin: Builtin::Ln,
        name: "ln",
        arity: 1,
        category: BuiltinCategory::Analytic,
    },
    BuiltinInfo {
        builtin: Builtin::Sqrt,
        name: "sqrt",
        arity: 1,
        category: BuiltinCategory::Analytic,
    },
    BuiltinInfo {
        builtin: Builtin::Pow,
        name: "pow",
        arity: 2,
        category: BuiltinCategory::Analytic,
    },
    BuiltinInfo {
        builtin: Builtin::NormCdf,
        name: "norm_cdf",
        arity: 1,
        category: BuiltinCategory::Analytic,
    },
    BuiltinInfo {
        builtin: Builtin::Lookup,
        name: "lookup",
        arity: 2,
        category: BuiltinCategory::Table,
    },
    BuiltinInfo {
        builtin: Builtin::Count,
        name: "count",
        arity: 1,
        category: BuiltinCategory::Reducer,
    },
    BuiltinInfo {
        builtin: Builtin::Sum,
        name: "sum",
        arity: 1,
        category: BuiltinCategory::Reducer,
    },
    BuiltinInfo {
        builtin: Builtin::MaxOf,
        name: "max_of",
        arity: 1,
        category: BuiltinCategory::Reducer,
    },
    BuiltinInfo {
        builtin: Builtin::MinOf,
        name: "min_of",
        arity: 1,
        category: BuiltinCategory::Reducer,
    },
    BuiltinInfo {
        builtin: Builtin::Oldest,
        name: "oldest",
        arity: 1,
        category: BuiltinCategory::Reducer,
    },
    BuiltinInfo {
        builtin: Builtin::Newest,
        name: "newest",
        arity: 1,
        category: BuiltinCategory::Reducer,
    },
    BuiltinInfo {
        builtin: Builtin::CountWhere,
        name: "count_where",
        arity: 2,
        category: BuiltinCategory::Reducer,
    },
    BuiltinInfo {
        builtin: Builtin::Triggered,
        name: "triggered",
        arity: 1,
        category: BuiltinCategory::Trigger,
    },
];

/// Find a builtin by its source spelling.
pub fn find(name: &str) -> Option<&'static BuiltinInfo> {
    BUILTINS.iter().find(|info| info.name == name)
}

/// The nearest builtin by edit distance, only when the distance is <= 2 — the
/// "did you mean" source. Ties resolve to the earliest table entry.
pub fn suggest(name: &str) -> Option<&'static str> {
    let mut best: Option<(usize, &'static str)> = None;
    for info in BUILTINS {
        let distance = levenshtein(name, info.name);
        let closer = match best {
            Some((best_distance, _)) => distance < best_distance,
            None => true,
        };
        if distance <= 2 && closer {
            best = Some((distance, info.name));
        }
    }
    best.map(|(_, name)| name)
}

/// Classic two-row Levenshtein distance over bytes (builtin names are ASCII).
fn levenshtein(a: &str, b: &str) -> usize {
    let a = a.as_bytes();
    let b = b.as_bytes();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut curr = vec![0usize; b.len() + 1];
    for (i, &byte_a) in a.iter().enumerate() {
        curr[0] = i + 1;
        for (j, &byte_b) in b.iter().enumerate() {
            let substitution = usize::from(byte_a != byte_b);
            curr[j + 1] = (prev[j] + substitution)
                .min(prev[j + 1] + 1)
                .min(curr[j] + 1);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_builtin_is_findable_by_name() {
        for info in BUILTINS {
            let found = find(info.name).expect("present");
            assert_eq!(found.builtin, info.builtin);
        }
        assert!(find("nope").is_none());
    }

    #[test]
    fn suggest_fires_within_distance_two_only() {
        assert_eq!(suggest("clamq"), Some("clamp"));
        assert_eq!(suggest("sqrtt"), Some("sqrt"));
        assert_eq!(suggest("completely_unrelated"), None);
    }
}
