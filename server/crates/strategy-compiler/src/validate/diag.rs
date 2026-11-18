//! Validator diagnostics: severity, structured document paths, and the closed code
//! roster.
//!
//! Diagnostics are machine-readable repair input for the LLM (structured path, stable
//! kebab-case code, standalone message) and human-readable at the same time. The
//! validator collects EVERY diagnostic it can find — the assembled list per round is
//! walk (parse) diagnostics merged with validator findings, sorted in document order.
//!
//! Message style rules (shared with `parse::error`): messages start lowercase, name the
//! construct rather than validator internals, and stand alone without the document
//! visible; help is concrete and imperative.

use std::cmp::Ordering;
use std::fmt;

use serde::{Serialize, Serializer};

use crate::parse::error::ParseError;
use crate::parse::span::Span;

/// Diagnostic severity. Warnings never block the typestate gate; a `ValidStrategy` is
/// produced iff zero errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Blocks validation.
    Error,
    /// Rides along; the three idiom lints and sparse-in nits.
    Warning,
}

/// One segment of a structured document path.
///
/// Rust note: `PartialOrd`/`Ord` are hand-written on [`DiagPath`], not derived here —
/// derived enum ordering compares variant discriminants first, which has no document
/// meaning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathSeg {
    /// A fixed field name (`legs`, `if`, `condition`).
    Key(&'static str),
    /// A document-authored name (map keys: `expressions.named.<name>`).
    Name(String),
    /// An array index.
    Index(usize),
}

/// A structured path into the document (`legs[0].body[2].if`).
///
/// Structured — not a string — so diagnostics sort in true document order: string
/// sorting puts `body[10]` before `body[2]`. `Display` renders the dotted form; serde
/// serializes the rendered string (the repair-loop contract wants the string form).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagPath(Vec<PathSeg>);

impl DiagPath {
    /// An empty path; extend with the builder methods.
    pub fn root() -> Self {
        DiagPath(Vec::new())
    }

    /// Append a fixed field name.
    pub fn key(mut self, key: &'static str) -> Self {
        self.0.push(PathSeg::Key(key));
        self
    }

    /// Append a document-authored name.
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.0.push(PathSeg::Name(name.into()));
        self
    }

    /// Append an array index.
    pub fn index(mut self, index: usize) -> Self {
        self.0.push(PathSeg::Index(index));
        self
    }

    /// Parse a walker-rendered path string (`legs[0].body[2].if`) into segments.
    ///
    /// The walker emits `String` paths; adopting its diagnostics needs them structured
    /// for sorting. Every walker path is dotted keys and bracketed indices, so this
    /// never fails; unparseable text (impossible today) is kept whole as a `Name`.
    pub fn from_walker(path: &str) -> Self {
        let mut segs = Vec::new();
        for part in path.split('.') {
            let (head, rest) = match part.find('[') {
                Some(pos) => (&part[..pos], &part[pos..]),
                None => (part, ""),
            };
            if !head.is_empty() {
                segs.push(PathSeg::Name(head.to_string()));
            }
            for idx in rest.split('[').skip(1) {
                match idx.strip_suffix(']').and_then(|i| i.parse().ok()) {
                    Some(i) => segs.push(PathSeg::Index(i)),
                    None => segs.push(PathSeg::Name(format!("[{idx}"))),
                }
            }
        }
        DiagPath(segs)
    }

    /// The canonical sort rank of the section this path roots in. Document section
    /// order, with the envelope pseudo-section last.
    pub fn section_rank(&self) -> u8 {
        let head = match self.0.first() {
            Some(PathSeg::Key(k)) => *k,
            Some(PathSeg::Name(n)) => n.as_str(),
            Some(PathSeg::Index(_)) | None => return u8::MAX,
        };
        match head {
            "meta" => 0,
            "instruments" => 1,
            "signals" => 2,
            "state" => 3,
            "expressions" => 4,
            "triggers" => 5,
            "legs" => 6,
            "envelope" => 7,
            _ => u8::MAX,
        }
    }
}

impl PartialOrd for DiagPath {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for DiagPath {
    /// Segment-wise document order: numeric on index pairs, string otherwise. Sibling
    /// fields of one object order lexicographically (not by JSON field order) — a
    /// documented simplification; the emission-index tiebreak in the assembler keeps
    /// the final list stable regardless.
    fn cmp(&self, other: &Self) -> Ordering {
        let rank = self.section_rank().cmp(&other.section_rank());
        if rank != Ordering::Equal {
            return rank;
        }
        for (a, b) in self.0.iter().zip(other.0.iter()) {
            let ord = match (a, b) {
                (PathSeg::Index(x), PathSeg::Index(y)) => x.cmp(y),
                (a, b) => seg_str(a).cmp(seg_str(b)),
            };
            if ord != Ordering::Equal {
                return ord;
            }
        }
        self.0.len().cmp(&other.0.len())
    }
}

/// The string form of a non-index segment; indices render for cross-kind comparison.
fn seg_str(seg: &PathSeg) -> &str {
    match seg {
        PathSeg::Key(k) => k,
        PathSeg::Name(n) => n,
        // Cross-kind comparison of an index against a key cannot occur on paths built
        // from one document, but Ord must be total; an index sorts as empty (first).
        PathSeg::Index(_) => "",
    }
}

impl fmt::Display for DiagPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut first = true;
        for seg in &self.0 {
            match seg {
                PathSeg::Key(k) => {
                    if !first {
                        write!(f, ".")?;
                    }
                    write!(f, "{k}")?;
                }
                PathSeg::Name(n) => {
                    if !first {
                        write!(f, ".")?;
                    }
                    write!(f, "{n}")?;
                }
                PathSeg::Index(i) => write!(f, "[{i}]")?,
            }
            first = false;
        }
        Ok(())
    }
}

impl Serialize for DiagPath {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

/// One validator or parse finding, anchored by document path.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Diagnostic {
    /// Blocks the gate, or rides along.
    pub severity: Severity,
    /// Where in the document.
    pub path: DiagPath,
    /// The finding itself.
    pub detail: DiagDetail,
}

/// The two diagnostic families. Externally tagged in JSON (`{"parse": ...}` /
/// `{"rule": ...}`) so tooling branches on the family before the code space.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagDetail {
    /// A walk diagnostic, adopted wholesale — [`ParseError`] carries its own code,
    /// span, message, help, and source.
    Parse(ParseError),
    /// A validator finding.
    Rule {
        /// The machine-readable failure class.
        code: DiagCode,
        /// Standalone human description.
        message: String,
        /// Span inside the expression string, when the finding anchors to one.
        #[serde(skip_serializing_if = "Option::is_none")]
        span: Option<Span>,
        /// The expression string the span indexes into.
        #[serde(skip_serializing_if = "Option::is_none")]
        source: Option<String>,
        /// Concrete, imperative fix guidance.
        #[serde(skip_serializing_if = "Option::is_none")]
        help: Option<String>,
    },
}

impl Diagnostic {
    /// Adopt a walk (parse) diagnostic. Always an error.
    pub fn parse(path: &str, error: ParseError) -> Self {
        Diagnostic {
            severity: Severity::Error,
            path: DiagPath::from_walker(path),
            detail: DiagDetail::Parse(error),
        }
    }

    /// A validator finding; severity comes from the code.
    pub fn rule(code: DiagCode, path: DiagPath, message: impl Into<String>) -> Self {
        Diagnostic {
            severity: code.severity(),
            path,
            detail: DiagDetail::Rule {
                code,
                message: message.into(),
                span: None,
                source: None,
                help: None,
            },
        }
    }

    /// Attach the expression string and the span inside it.
    pub fn with_source(mut self, source: impl Into<String>, span: Span) -> Self {
        if let DiagDetail::Rule {
            span: s,
            source: src,
            ..
        } = &mut self.detail
        {
            *s = Some(span);
            *src = Some(source.into());
        }
        self
    }

    /// Attach help text.
    pub fn with_help(mut self, help: impl Into<String>) -> Self {
        if let DiagDetail::Rule { help: h, .. } = &mut self.detail {
            *h = Some(help.into());
        }
        self
    }

    /// Whether this diagnostic blocks the typestate gate.
    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sev = match self.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
        };
        match &self.detail {
            DiagDetail::Parse(err) => {
                // Re-render the parse error with the document path on the header line.
                writeln!(
                    f,
                    "{sev}[{}] at {}: {}",
                    err.code.as_str(),
                    self.path,
                    err.message
                )?;
                writeln!(f, "  {}", err.source)?;
                let start = err.span.start as usize;
                let width = (err.span.end.saturating_sub(err.span.start) as usize).max(1);
                writeln!(f, "  {}{}", " ".repeat(start), "^".repeat(width))?;
                if let (Some(expected), Some(found)) = (&err.expected, &err.found) {
                    writeln!(f, "  = expected {expected}, found {found}")?;
                }
                if let Some(help) = &err.help {
                    writeln!(f, "  = help: {help}")?;
                }
                Ok(())
            }
            DiagDetail::Rule {
                code,
                message,
                span,
                source,
                help,
            } => {
                writeln!(f, "{sev}[{}] at {}: {}", code.as_str(), self.path, message)?;
                if let (Some(span), Some(source)) = (span, source) {
                    writeln!(f, "  {source}")?;
                    let start = span.start as usize;
                    let width = (span.end.saturating_sub(span.start) as usize).max(1);
                    writeln!(f, "  {}{}", " ".repeat(start), "^".repeat(width))?;
                }
                if let Some(help) = help {
                    writeln!(f, "  = help: {help}")?;
                }
                Ok(())
            }
        }
    }
}

/// The closed validator code roster: 75 codes, 67 errors and 8 warnings. Tooling
/// branches on these; messages are never parsed. The coverage contract test diffs the
/// codes exercised by the negative suite against this roster — adding a code here
/// without a negative test fails that gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DiagCode {
    // Family A — resolution, uniqueness, reservedness.
    /// A second declaration of a name an array cannot deduplicate (triggers, legs,
    /// state slots, block names per leg).
    DuplicateName,
    /// A declared identifier in the reserved set (keywords, namespace heads, builtin
    /// names, `stay`).
    ReservedName,
    /// A trigger leaf or `signal.*` read naming no declared signal nickname.
    UnknownSignal,
    /// A `signals` map value matching no registry signal id.
    UnknownSignalId,
    /// An intent or timer naming no declared instrument nickname.
    UnknownInstrument,
    /// An `instruments` map value that is neither a market id nor `<series>.current`.
    UnknownInstrumentId,
    /// An arm entry or `triggered(NAME)` naming no declared trigger.
    UnknownTrigger,
    /// A `then`, on-table destination, or `legs.*` read naming no declared leg.
    UnknownLeg,
    /// A `state.*` read or statement lvalue naming no declared slot.
    UnknownStateSlot,
    /// An `expr.*` reference naming no named expression.
    UnknownNamedExpr,
    /// An `expr.NAME.field` reference — named expressions have no fields.
    ExprRefHasField,
    /// A `table.*` reference naming no constant table.
    UnknownTable,
    /// A cancel targeting a tag no place intent declares.
    UnknownTag,
    /// A `param.*` read or `{"param": ...}` leaf value naming no declared param.
    UnknownParam,

    // Family B — registry cross-checks.
    /// A field name absent from the signal's registry entry.
    UnknownField,
    /// An ordering comparator on a non-numeric field.
    ComparatorTypeMismatch,
    /// A leaf value incompatible with the field or expression type.
    ValueTypeMismatch,
    /// A trigger leaf expression that is not numeric (an enum, string or boolean
    /// value where arithmetic is expected).
    LeafFieldNotNumeric,
    /// An outcome-structured market traded without an `outcome`.
    OutcomeRequired,
    /// A linear market traded with an `outcome`.
    OutcomeForbidden,
    /// An `outcome` matching no registry outcome label.
    UnknownOutcome,

    // Family C — context restrictions.
    /// `triggered()` outside a block's `if` guard.
    TriggeredOutsideGuard,
    /// An `event.*` read in a block whose guard does not pin exactly one
    /// single-signal condition trigger.
    EventUnpinned,
    /// A `table.*` reference anywhere but `lookup`'s first argument.
    TableOutsideLookup,
    /// A trigger leaf expression reading anything but signals, params and envelope
    /// constants, or using comparisons, booleans, conditionals, tables or `triggered()`.
    TriggerExprImpure,
    /// A window used as a value, a reducer over a non-window, a `push` to a
    /// non-window, a `set` on a window, or an impure `count_where` predicate.
    WindowMisuse,
    /// An `entry.*` read outside a `count_where` predicate, or a field other than
    /// `value`.
    EntryOutsideReducer,
    /// A read of a gate parameter (`max_exposure_fraction`, `pause_at_consumed`).
    EnvelopeUnreadable,
    /// An `envelope.*` read outside the envelope's field set.
    UnknownEnvelopeField,
    /// A `platform.*` read outside the closed path set.
    UnknownPlatformPath,
    /// A malformed `legs.*` read (bad shape or field outside the closed set).
    LegsReadInvalid,
    /// A `legs.<leg>.tag.<tag>.*` read naming a tag no order placed from that leg
    /// carries.
    LegsUnknownTag,
    /// An `event.*` read inside a named expression (site-dependent references are
    /// forbidden there).
    EventInNamedExpr,
    /// An indexed `set` on a scalar slot.
    SetScalarIndexed,
    /// An unindexed `set` on a map slot.
    SetMapUnindexed,
    /// A `set` index list whose arity differs from the slot's `key_types`.
    IndexArityMismatch,
    /// A `set` index expression definitely incompatible with its key type.
    IndexTypeMismatch,
    /// A `set` right-hand side definitely incompatible with the slot's value type.
    SetRhsTypeMismatch,

    // Family G — typing-lite.
    /// A guard whose root definitely is not boolean.
    GuardNotBoolean,
    /// Definitely incompatible operand types.
    TypeMismatch,
    /// A variant literal outside the compared enum's variant set.
    UnknownVariant,

    // Family D — structural.
    /// An `all`/`any` group with fewer than two leaves.
    GroupTooSmall,
    /// A constant table with `keys` and `values` of different lengths.
    TableLengthMismatch,
    /// A constant table with fewer than two entries.
    TableTooSmall,
    /// Constant table keys not strictly increasing.
    TableKeysNotIncreasing,
    /// An inline enum with fewer than two variants.
    EnumTooSmall,
    /// An inline enum with a repeated variant.
    DuplicateVariant,
    /// A map slot with `max_keys` of zero.
    MaxKeysZero,
    /// A window slot with a capacity outside 1..=4096 or a non-numeric value type.
    WindowShape,
    /// A param default that does not fit the param's declared type.
    ParamDefaultMismatch,
    /// A required section empty, or an optional section present but empty.
    EmptySection,
    /// A limit order missing `price` or `price_band`.
    LimitRequiresPrice,
    /// A market order carrying `price` or `price_band`.
    MarketForbidsPrice,
    /// A cancel-replace whose order type is not `limit`.
    CancelReplaceNotLimit,
    /// An order flag the instrument's registry capabilities do not declare.
    VenueCapabilityMissing,
    /// `post_only` on a market order, or `expire_after` on an IOC order.
    OrderFlagMisuse,
    /// A price band whose low bound exceeds its high bound.
    BandInverted,
    /// A non-resident leg with no transition out.
    NonResidentNoExit,
    /// An `interval` timer with a period under the 250 ms floor.
    IntervalTooFast,
    /// An `interval` timer with `rearm: never` — it can only ever fire once.
    IntervalNeverRearms,
    /// An `on` table with no keys — omit it (sparse-in).
    EmptyOnTable,
    /// An on-table destination written `stay` — the default; omit it (sparse-in).
    ExplicitStay,
    /// An envelope gate fraction above one.
    EnvelopeFractionOutOfRange,
    /// An envelope loss budget exceeding the allocation.
    EnvelopeBudgetExceedsAllocation,
    /// The program reads an optional envelope field the submission omits.
    EnvelopeMissingReadField,

    // Family E — substitution and order expressions.
    /// A named-expression reference cycle.
    ExprCycle,
    /// A price or size expression not clamp-wrapped post-substitution.
    NotClampWrapped,
    /// A price clamp whose bounds are not literal decimals — the band cannot be
    /// verified.
    BandUnverifiable,
    /// Literal clamp bounds outside the declared price band.
    BandViolation,
    /// A size (or max_size) expression with no `envelope.*` reference.
    SizeWithoutEnvelope,
    /// A size whose value (the clamp's first argument) does not derive from
    /// `envelope.*`, `platform.position.*` or the literal zero.
    SizeNotEnvelopeScaled,

    // Family F — idiom lints and arm-set warnings.
    /// A block gated on an edge-fired trigger reading state that other blocks mutate.
    LintMissedWake,
    /// An intent block with nothing stopping consecutive re-fire.
    LintMachineGun,
    /// A guard threshold not implied by the armed trigger's same-field leaf.
    LintCoverage,
    /// A requote block gated on an unpaced `if_true` trigger over a sub-second signal.
    LintRequoteRate,
    /// `triggered(X)` in a leg that does not arm X.
    TriggeredNotArmed,
    /// A trigger no leg arms.
    TriggerUnarmed,
}

impl DiagCode {
    /// The stable kebab-case slug, identical to the serde serialization.
    pub fn as_str(self) -> &'static str {
        match self {
            DiagCode::DuplicateName => "duplicate-name",
            DiagCode::ReservedName => "reserved-name",
            DiagCode::UnknownSignal => "unknown-signal",
            DiagCode::UnknownSignalId => "unknown-signal-id",
            DiagCode::UnknownInstrument => "unknown-instrument",
            DiagCode::UnknownInstrumentId => "unknown-instrument-id",
            DiagCode::UnknownTrigger => "unknown-trigger",
            DiagCode::UnknownLeg => "unknown-leg",
            DiagCode::UnknownStateSlot => "unknown-state-slot",
            DiagCode::UnknownNamedExpr => "unknown-named-expr",
            DiagCode::ExprRefHasField => "expr-ref-has-field",
            DiagCode::UnknownTable => "unknown-table",
            DiagCode::UnknownTag => "unknown-tag",
            DiagCode::UnknownParam => "unknown-param",
            DiagCode::UnknownField => "unknown-field",
            DiagCode::ComparatorTypeMismatch => "comparator-type-mismatch",
            DiagCode::ValueTypeMismatch => "value-type-mismatch",
            DiagCode::LeafFieldNotNumeric => "leaf-field-not-numeric",
            DiagCode::OutcomeRequired => "outcome-required",
            DiagCode::OutcomeForbidden => "outcome-forbidden",
            DiagCode::UnknownOutcome => "unknown-outcome",
            DiagCode::TriggeredOutsideGuard => "triggered-outside-guard",
            DiagCode::EventUnpinned => "event-unpinned",
            DiagCode::TableOutsideLookup => "table-outside-lookup",
            DiagCode::TriggerExprImpure => "trigger-expr-impure",
            DiagCode::WindowMisuse => "window-misuse",
            DiagCode::EntryOutsideReducer => "entry-outside-reducer",
            DiagCode::EnvelopeUnreadable => "envelope-unreadable",
            DiagCode::UnknownEnvelopeField => "unknown-envelope-field",
            DiagCode::UnknownPlatformPath => "unknown-platform-path",
            DiagCode::LegsReadInvalid => "legs-read-invalid",
            DiagCode::LegsUnknownTag => "legs-unknown-tag",
            DiagCode::EventInNamedExpr => "event-in-named-expr",
            DiagCode::SetScalarIndexed => "set-scalar-indexed",
            DiagCode::SetMapUnindexed => "set-map-unindexed",
            DiagCode::IndexArityMismatch => "index-arity-mismatch",
            DiagCode::IndexTypeMismatch => "index-type-mismatch",
            DiagCode::SetRhsTypeMismatch => "set-rhs-type-mismatch",
            DiagCode::GuardNotBoolean => "guard-not-boolean",
            DiagCode::TypeMismatch => "type-mismatch",
            DiagCode::UnknownVariant => "unknown-variant",
            DiagCode::GroupTooSmall => "group-too-small",
            DiagCode::TableLengthMismatch => "table-length-mismatch",
            DiagCode::TableTooSmall => "table-too-small",
            DiagCode::TableKeysNotIncreasing => "table-keys-not-increasing",
            DiagCode::EnumTooSmall => "enum-too-small",
            DiagCode::DuplicateVariant => "duplicate-variant",
            DiagCode::MaxKeysZero => "max-keys-zero",
            DiagCode::WindowShape => "window-shape",
            DiagCode::ParamDefaultMismatch => "param-default-mismatch",
            DiagCode::EmptySection => "empty-section",
            DiagCode::LimitRequiresPrice => "limit-requires-price",
            DiagCode::MarketForbidsPrice => "market-forbids-price",
            DiagCode::CancelReplaceNotLimit => "cancel-replace-not-limit",
            DiagCode::VenueCapabilityMissing => "venue-capability-missing",
            DiagCode::OrderFlagMisuse => "order-flag-misuse",
            DiagCode::BandInverted => "band-inverted",
            DiagCode::NonResidentNoExit => "non-resident-no-exit",
            DiagCode::IntervalTooFast => "interval-too-fast",
            DiagCode::IntervalNeverRearms => "interval-never-rearms",
            DiagCode::EmptyOnTable => "empty-on-table",
            DiagCode::ExplicitStay => "explicit-stay",
            DiagCode::EnvelopeFractionOutOfRange => "envelope-fraction-out-of-range",
            DiagCode::EnvelopeBudgetExceedsAllocation => "envelope-budget-exceeds-allocation",
            DiagCode::EnvelopeMissingReadField => "envelope-missing-read-field",
            DiagCode::ExprCycle => "expr-cycle",
            DiagCode::NotClampWrapped => "not-clamp-wrapped",
            DiagCode::BandUnverifiable => "band-unverifiable",
            DiagCode::BandViolation => "band-violation",
            DiagCode::SizeWithoutEnvelope => "size-without-envelope",
            DiagCode::SizeNotEnvelopeScaled => "size-not-envelope-scaled",
            DiagCode::LintMissedWake => "lint-missed-wake",
            DiagCode::LintMachineGun => "lint-machine-gun",
            DiagCode::LintCoverage => "lint-coverage",
            DiagCode::LintRequoteRate => "lint-requote-rate",
            DiagCode::TriggeredNotArmed => "triggered-not-armed",
            DiagCode::TriggerUnarmed => "trigger-unarmed",
        }
    }

    /// The code's fixed severity: the three idiom lints, the two arm-set warnings, the
    /// two sparse-in nits and the once-only interval are warnings; everything else is
    /// an error.
    pub fn severity(self) -> Severity {
        match self {
            DiagCode::EmptyOnTable
            | DiagCode::ExplicitStay
            | DiagCode::IntervalNeverRearms
            | DiagCode::LintMissedWake
            | DiagCode::LintMachineGun
            | DiagCode::LintCoverage
            | DiagCode::LintRequoteRate
            | DiagCode::TriggeredNotArmed
            | DiagCode::TriggerUnarmed => Severity::Warning,
            _ => Severity::Error,
        }
    }

    /// The full roster, for the coverage contract test.
    pub const ALL: &'static [DiagCode] = &[
        DiagCode::DuplicateName,
        DiagCode::ReservedName,
        DiagCode::UnknownSignal,
        DiagCode::UnknownSignalId,
        DiagCode::UnknownInstrument,
        DiagCode::UnknownInstrumentId,
        DiagCode::UnknownTrigger,
        DiagCode::UnknownLeg,
        DiagCode::UnknownStateSlot,
        DiagCode::UnknownNamedExpr,
        DiagCode::ExprRefHasField,
        DiagCode::UnknownTable,
        DiagCode::UnknownTag,
        DiagCode::UnknownParam,
        DiagCode::UnknownField,
        DiagCode::ComparatorTypeMismatch,
        DiagCode::ValueTypeMismatch,
        DiagCode::LeafFieldNotNumeric,
        DiagCode::OutcomeRequired,
        DiagCode::OutcomeForbidden,
        DiagCode::UnknownOutcome,
        DiagCode::TriggeredOutsideGuard,
        DiagCode::EventUnpinned,
        DiagCode::TableOutsideLookup,
        DiagCode::TriggerExprImpure,
        DiagCode::WindowMisuse,
        DiagCode::EntryOutsideReducer,
        DiagCode::EnvelopeUnreadable,
        DiagCode::UnknownEnvelopeField,
        DiagCode::UnknownPlatformPath,
        DiagCode::LegsReadInvalid,
        DiagCode::LegsUnknownTag,
        DiagCode::EventInNamedExpr,
        DiagCode::SetScalarIndexed,
        DiagCode::SetMapUnindexed,
        DiagCode::IndexArityMismatch,
        DiagCode::IndexTypeMismatch,
        DiagCode::SetRhsTypeMismatch,
        DiagCode::GuardNotBoolean,
        DiagCode::TypeMismatch,
        DiagCode::UnknownVariant,
        DiagCode::GroupTooSmall,
        DiagCode::TableLengthMismatch,
        DiagCode::TableTooSmall,
        DiagCode::TableKeysNotIncreasing,
        DiagCode::EnumTooSmall,
        DiagCode::DuplicateVariant,
        DiagCode::MaxKeysZero,
        DiagCode::WindowShape,
        DiagCode::ParamDefaultMismatch,
        DiagCode::EmptySection,
        DiagCode::LimitRequiresPrice,
        DiagCode::MarketForbidsPrice,
        DiagCode::CancelReplaceNotLimit,
        DiagCode::VenueCapabilityMissing,
        DiagCode::OrderFlagMisuse,
        DiagCode::BandInverted,
        DiagCode::NonResidentNoExit,
        DiagCode::IntervalTooFast,
        DiagCode::IntervalNeverRearms,
        DiagCode::EmptyOnTable,
        DiagCode::ExplicitStay,
        DiagCode::EnvelopeFractionOutOfRange,
        DiagCode::EnvelopeBudgetExceedsAllocation,
        DiagCode::EnvelopeMissingReadField,
        DiagCode::ExprCycle,
        DiagCode::NotClampWrapped,
        DiagCode::BandUnverifiable,
        DiagCode::BandViolation,
        DiagCode::SizeWithoutEnvelope,
        DiagCode::SizeNotEnvelopeScaled,
        DiagCode::LintMissedWake,
        DiagCode::LintMachineGun,
        DiagCode::LintCoverage,
        DiagCode::LintRequoteRate,
        DiagCode::TriggeredNotArmed,
        DiagCode::TriggerUnarmed,
    ];
}
