//! The program: the compiler's executable artifact, consumed by the exchange runtime.
//!
//! A `Program` is a direct, serializable projection of [`crate::lower::Lowered`]: the
//! same subscriptions, trigger rows, legs, block plans, intent index, state slots and
//! tables, with every expression lowered to a closed [`ExprNode`] op-tree. It describes
//! and never computes — the runtime interprets it. There is no generated code and no
//! sandbox; the runtime deserializes straight into these types (the exchange crate
//! depends on this one) and the JSON on disk is the stored artifact, not a second
//! contract.
//!
//! String-canonical throughout: every decimal is its canonical source string, never a
//! float. The runtime chooses the arithmetic type. Serialization order is struct
//! declaration order plus `BTreeMap` order, so emission is byte-deterministic.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ir::{Comparator, Envelope, Fire, Interpolation, OrderType, OverflowPolicy, RearmMode, Side, Tif};

/// The program format tag stamped on every emitted program. Bump on any change to the
/// shapes in this module that a runtime could not read.
pub const FORMAT: &str = "tiny-program/2";

/// One compiled strategy, ready to run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Program {
    /// [`FORMAT`].
    pub format: String,
    /// The compiler version that produced the program.
    pub compiler_version: String,
    /// `Strategy_` + the first 12 hex of `source_hash`. Same document, same id.
    pub strategy_id: String,
    /// sha256 over the canonical re-serialization of the validated document.
    pub source_hash: String,
    /// `meta.name`.
    pub name: String,
    /// `meta.description` — the authored English.
    pub description: String,
    /// `meta.dsl_version`.
    pub dsl_version: String,
    /// Signal nickname -> resolved signal id.
    pub signals: BTreeMap<String, String>,
    /// Instrument nickname -> venue instrument id.
    pub instruments: BTreeMap<String, String>,
    /// The resolved signal ids the strategy subscribes to, lexicographic by nickname.
    pub subscriptions: Vec<String>,
    /// The submission envelope, verbatim. Envelope reads inside expressions are
    /// already resolved to their values; this copy is for the risk gate.
    pub envelope: Envelope,
    /// The state slots, document order.
    pub state: Vec<StateSlot>,
    /// The deploy-time params, document order, with their types and defaults — enough
    /// for the runtime to validate a re-parameterization without the source document.
    pub params: Vec<ParamDef>,
    /// The document-level triggers, document order. One row each; legs mask them.
    pub triggers: Vec<TriggerDef>,
    /// The legs, document order. The first is the initial leg.
    pub legs: Vec<LegDef>,
    /// The initial leg's name.
    pub initial_leg: String,
    /// `intent_id` -> totalized transition table and resolved size bound.
    pub intents: BTreeMap<String, IntentDef>,
    /// Constant tables by name.
    pub tables: BTreeMap<String, TableDef>,
}

// ────────────────────────────────── state ──────────────────────────────────

/// One declared state slot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum StateSlot {
    /// A scalar slot.
    Scalar {
        /// The slot name.
        name: String,
        /// What the slot reads as before any write: a literal node.
        default: ExprNode,
    },
    /// A bounded ring buffer of numeric values: `push` appends, evicting the oldest at
    /// capacity; starts empty.
    Window {
        /// The slot name.
        name: String,
        /// The ring capacity.
        capacity: u32,
    },
    /// A bounded map slot, keyed by a fixed-arity tuple of values.
    Map {
        /// The slot name.
        name: String,
        /// The number of key parts every read and write supplies.
        key_arity: usize,
        /// What an absent key reads as: a literal node.
        default: ExprNode,
        /// The declared capacity.
        max_keys: u32,
        /// What a write of a new key does once the map is full.
        overflow: OverflowPolicy,
    },
}

// ────────────────────────────────── params ──────────────────────────────────

/// One deploy-time param: a typed value the user sets at deploy and may change on a
/// running strategy. [`ExprNode::Param`] reads it by name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ParamDef {
    /// The param name.
    pub name: String,
    /// The value type; a re-parameterization must fit it.
    #[serde(rename = "type")]
    pub param_type: ParamType,
    /// The value the strategy deploys with: a literal node of the declared type.
    pub default: ExprNode,
}

/// A param's value type.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ParamType {
    /// An unsigned 8-bit integer.
    U8,
    /// An exact decimal (signed).
    Decimal,
    /// A boolean.
    Bool,
    /// Free text.
    String,
    /// One of a closed variant set.
    Enum {
        /// The legal variants.
        variants: Vec<String>,
    },
}

// ───────────────────────────────── triggers ─────────────────────────────────

/// One document-level trigger: its wake source plus edge and rearm semantics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TriggerDef {
    /// The trigger name, as legs arm it and `triggered(NAME)` reads it.
    pub name: String,
    /// The wake source.
    pub source: TriggerSource,
    /// Fire mode.
    pub fire: Fire,
    /// Rearm mode.
    pub rearm: RearmMode,
    /// Minimum gap between fires, milliseconds, on top of the rearm mode. Checked after
    /// the fire/rearm state machine decides a fire; a suppressed fire still advances
    /// bookkeeping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_ms: Option<u64>,
}

/// A trigger's wake source.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TriggerSource {
    /// Same-moment conjunction over the latest snapshot of every referenced signal.
    /// A lone authored leaf is a one-element group.
    All {
        /// The conjuncts.
        leaves: Vec<Leaf>,
    },
    /// Stateless disjunction.
    Any {
        /// The disjuncts.
        leaves: Vec<Leaf>,
    },
    /// Fires when the instrument's window clock reaches the remaining threshold.
    /// Armed on leg entry, cancelled on leg exit, measured per arming.
    WindowClock {
        /// The resolved venue instrument id.
        instrument_id: String,
        /// The threshold before window close, milliseconds.
        remaining_ms: u64,
    },
    /// Fires once, this long after the arming leg was entered. Re-armed (and the clock
    /// restarted) on every mask change that includes it.
    After {
        /// Milliseconds from arming.
        elapsed_ms: u64,
    },
    /// Fires every period while the trigger is in the mask, first one period after
    /// arming.
    Interval {
        /// The period, milliseconds.
        every_ms: u64,
    },
}

/// One condition leaf.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Leaf {
    /// True on every publish of the signal.
    Always {
        /// The resolved signal id.
        signal_id: String,
    },
    /// One field of the signal compared against a value.
    Field {
        /// The resolved signal id.
        signal_id: String,
        /// The field name.
        field: String,
        /// The comparison.
        comparator: Comparator,
        /// What the field is compared against.
        value: LeafValue,
    },
    /// Arithmetic over subscribed signals' fields compared against a value: the
    /// same-moment rule applies — evaluated against the latest snapshot of every
    /// referenced signal whenever any of them publishes. Missing field or division
    /// by zero is false.
    Expr {
        /// The arithmetic: [`ExprNode::Signal`], [`ExprNode::Param`] and
        /// [`ExprNode::Envelope`] reads, literals, arithmetic and math builtins.
        expr: ExprNode,
        /// The comparison.
        comparator: Comparator,
        /// What the arithmetic is compared against (numeric).
        value: LeafValue,
    },
}

/// What a comparing leaf compares against.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum LeafValue {
    /// A literal, string-canonical: a decimal, a variant name, or text, per the
    /// field's registry type.
    Literal {
        /// The canonical value string.
        value: String,
    },
    /// The current value of a deploy-time param. Its declared type matches the
    /// field's; a param change re-evaluates the row.
    Param {
        /// The param name, a key into [`Program::params`].
        name: String,
    },
}

// ─────────────────────────────────── legs ───────────────────────────────────

/// One leg: a delivery mask plus its blocks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LegDef {
    /// The leg name.
    pub name: String,
    /// Whether the leg may idle forever.
    pub resident: bool,
    /// The trigger names delivered while this leg is current. Empty is terminal.
    pub arm: Vec<String>,
    /// The blocks, document order. On an activation the runtime walks them in
    /// order; a block whose `then` runs ends the activation.
    pub blocks: Vec<BlockDef>,
}

/// One block: guard, do-items, transition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BlockDef {
    /// The block name (the middle segment of its intents' ids).
    pub name: String,
    /// The guard; absent means the block always runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guard: Option<ExprNode>,
    /// Every signal field the guard, statements, and order expressions read. The
    /// block does not run on an activation in which any of them has not published.
    pub reads: Vec<SignalRead>,
    /// The do-items, authored order.
    pub items: Vec<Item>,
    /// The leg to switch to after the items run. Immediate and terminal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub then: Option<String>,
}

/// One (signal, field) a block reads from the activation snapshot.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SignalRead {
    /// The resolved signal id.
    pub signal_id: String,
    /// The field name.
    pub field: String,
}

/// One do-item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Item {
    /// `set state.<slot>[keys] = value`. The value reads the pre-assignment state.
    Set {
        /// The target slot.
        slot: String,
        /// The key parts; present iff the slot is a map.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        keys: Option<Vec<ExprNode>>,
        /// The new value.
        value: ExprNode,
    },
    /// `push state.<window> = value`: append, evicting the oldest at capacity.
    Push {
        /// The target window slot.
        slot: String,
        /// The appended value.
        value: ExprNode,
    },
    /// Place an order.
    Place {
        /// The intent id, a key into [`Program::intents`].
        intent_id: String,
        /// The order.
        order: OrderDef,
    },
    /// Cancel every live order carrying the tag.
    Cancel {
        /// The tag.
        tag: String,
    },
    /// Atomically replace the one live order carrying the tag.
    CancelReplace {
        /// The intent id, a key into [`Program::intents`].
        intent_id: String,
        /// The replacement order; carries the same tag.
        order: OrderDef,
    },
    /// Reach a target signed position on the outcome: the runtime computes the delta
    /// against the live position and places one market IOC order (side from the
    /// delta's sign), or nothing when already there. `order.size` is the TARGET;
    /// `order.side` is a placeholder.
    TargetPosition {
        /// The intent id, a key into [`Program::intents`].
        intent_id: String,
        /// The target and the delta order's shape.
        order: OrderDef,
    },
}

/// An order body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OrderDef {
    /// The venue instrument id (`<series>.current` resolves at placement).
    pub instrument_id: String,
    /// The outcome; present iff the instrument is outcome-structured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    /// Buy or sell.
    pub side: Side,
    /// Market or limit.
    pub order_type: OrderType,
    /// Time in force.
    pub tif: Tif,
    /// The logical order tag.
    pub tag: String,
    /// The size expression.
    pub size: ExprNode,
    /// The price expression; present iff `order_type` is limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price: Option<ExprNode>,
    /// Rest or reject, never take (limit orders only).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub post_only: bool,
    /// Good-till-time: expire this many milliseconds after placement, reported as
    /// `expiry` (resting orders only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expire_after_ms: Option<u64>,
    /// May only shrink the position: the runtime clamps the size to the live position.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reduce_only: bool,
}

// ────────────────────────────────── intents ──────────────────────────────────

/// What the runtime snapshots onto an order at placement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IntentDef {
    /// The totalized transition table.
    pub on: OnTable,
    /// The per-intent size bound, canonical decimal. Enforced by the runtime
    /// without trusting the size expression.
    pub max_size: String,
}

/// The five-key transition table. Each value is `stay` or a leg name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OnTable {
    /// On full fill.
    pub fill: String,
    /// On partial fill.
    pub partial: String,
    /// On venue rejection.
    pub reject: String,
    /// On cancel acknowledgement.
    pub cancel_ack: String,
    /// On expiry.
    pub expiry: String,
}

// ────────────────────────────────── tables ──────────────────────────────────

/// A constant lookup table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TableDef {
    /// The key column, ascending, canonical decimals (signed domain legal).
    pub keys: Vec<String>,
    /// The value column, canonical decimals.
    pub values: Vec<String>,
    /// The interpolation between neighboring keys.
    pub interpolation: Interpolation,
}

// ───────────────────────────────── expressions ─────────────────────────────────

/// The closed expression op-tree. Every node is total: the runtime's evaluation of a
/// node either yields a value or, for the analytic builtins' domain edges and
/// division by zero, the IR's pinned totality result.
///
/// Reads are resolved: signal reads name the signal id, envelope reads carry the
/// resolved value. Named expressions are already inlined; there is no reference
/// node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExprNode {
    /// A canonical decimal literal (integers included).
    Decimal {
        /// The canonical decimal string.
        value: String,
    },
    /// A boolean literal.
    Bool {
        /// The value.
        value: bool,
    },
    /// A duration literal, milliseconds.
    DurationMs {
        /// The value.
        value: u64,
    },
    /// An enum variant literal.
    Variant {
        /// The variant name.
        value: String,
    },
    /// A string literal. Only defaults produce these today (there is no string
    /// literal in the expression grammar); strings otherwise arrive via signal fields
    /// and params.
    Str {
        /// The text.
        value: String,
    },
    /// The current value of a deploy-time param. Runtime state, never inlined.
    Param {
        /// The param name, a key into [`Program::params`].
        name: String,
    },
    /// A field of a subscribed signal's latest snapshot.
    Signal {
        /// The resolved signal id.
        signal_id: String,
        /// The field name.
        field: String,
    },
    /// A field of the triggering event.
    Event {
        /// The field name.
        field: String,
    },
    /// A scalar state slot.
    State {
        /// The slot name.
        slot: String,
    },
    /// A map state slot at a key.
    StateMap {
        /// The slot name.
        slot: String,
        /// The key parts, in declared order.
        keys: Vec<ExprNode>,
    },
    /// An envelope field, resolved at compile time.
    Envelope {
        /// The field spelling.
        field: String,
        /// The resolved canonical decimal value.
        value: String,
    },
    /// A runtime-provided platform value.
    Platform {
        /// Which value.
        read: PlatformRead,
    },
    /// A field of the fill record of orders placed from a leg (optionally restricted
    /// to one tag): `legs.<leg>.<field>` or `legs.<leg>.tag.<tag>.<field>`.
    LegFill {
        /// The leg the orders were placed from.
        leg: String,
        /// Restrict to orders carrying this tag, when present.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tag: Option<String>,
        /// Which field of the record.
        field: LegFillField,
    },
    /// Numeric negation.
    Neg {
        /// The operand.
        arg: Box<ExprNode>,
    },
    /// Boolean negation.
    Not {
        /// The operand.
        arg: Box<ExprNode>,
    },
    /// `+`
    #[serde(rename = "+")]
    Add {
        /// Left operand.
        lhs: Box<ExprNode>,
        /// Right operand.
        rhs: Box<ExprNode>,
    },
    /// `-`
    #[serde(rename = "-")]
    Sub {
        /// Left operand.
        lhs: Box<ExprNode>,
        /// Right operand.
        rhs: Box<ExprNode>,
    },
    /// `*`
    #[serde(rename = "*")]
    Mul {
        /// Left operand.
        lhs: Box<ExprNode>,
        /// Right operand.
        rhs: Box<ExprNode>,
    },
    /// `/`
    #[serde(rename = "/")]
    Div {
        /// Left operand.
        lhs: Box<ExprNode>,
        /// Right operand.
        rhs: Box<ExprNode>,
    },
    /// `<`
    #[serde(rename = "<")]
    Lt {
        /// Left operand.
        lhs: Box<ExprNode>,
        /// Right operand.
        rhs: Box<ExprNode>,
    },
    /// `<=`
    #[serde(rename = "<=")]
    Le {
        /// Left operand.
        lhs: Box<ExprNode>,
        /// Right operand.
        rhs: Box<ExprNode>,
    },
    /// `>`
    #[serde(rename = ">")]
    Gt {
        /// Left operand.
        lhs: Box<ExprNode>,
        /// Right operand.
        rhs: Box<ExprNode>,
    },
    /// `>=`
    #[serde(rename = ">=")]
    Ge {
        /// Left operand.
        lhs: Box<ExprNode>,
        /// Right operand.
        rhs: Box<ExprNode>,
    },
    /// `==`
    #[serde(rename = "==")]
    Eq {
        /// Left operand.
        lhs: Box<ExprNode>,
        /// Right operand.
        rhs: Box<ExprNode>,
    },
    /// `!=`
    #[serde(rename = "!=")]
    Ne {
        /// Left operand.
        lhs: Box<ExprNode>,
        /// Right operand.
        rhs: Box<ExprNode>,
    },
    /// `and`
    And {
        /// Left operand.
        lhs: Box<ExprNode>,
        /// Right operand.
        rhs: Box<ExprNode>,
    },
    /// `or`
    Or {
        /// Left operand.
        lhs: Box<ExprNode>,
        /// Right operand.
        rhs: Box<ExprNode>,
    },
    /// `if c then v ... else e`: the first arm whose condition holds, else the
    /// fallback.
    If {
        /// The arms, source order.
        arms: Vec<IfArm>,
        /// The fallback value.
        #[serde(rename = "else")]
        else_value: Box<ExprNode>,
    },
    /// A builtin call. Arity is fixed per builtin and was checked at parse time.
    Call {
        /// The builtin.
        builtin: Builtin,
        /// The arguments, source order.
        args: Vec<ExprNode>,
    },
    /// `lookup(table.NAME, key)`.
    Lookup {
        /// The table name, a key into [`Program::tables`].
        table: String,
        /// The key expression.
        key: Box<ExprNode>,
    },
    /// `triggered(NAME)`: true iff the named trigger is among what fired this
    /// activation.
    Triggered {
        /// The trigger name.
        name: String,
    },
    /// A reducer over a window slot: an O(capacity) scan. Pinned totality over an
    /// empty window: `count`, `sum` and `count_where` are `0`; `max_of`, `min_of`,
    /// `oldest` and `newest` are `0` too (the numeric zero).
    Reduce {
        /// Which reducer.
        reducer: Reducer,
        /// The window slot name.
        slot: String,
        /// `count_where`'s predicate, evaluated per element with [`ExprNode::Entry`]
        /// bound; absent for every other reducer.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        predicate: Option<Box<ExprNode>>,
    },
    /// The element under a `count_where` predicate (`entry.value`).
    Entry,
}

/// The window reducers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Reducer {
    /// Number of elements.
    Count,
    /// Sum of elements.
    Sum,
    /// Largest element.
    MaxOf,
    /// Smallest element.
    MinOf,
    /// The oldest element.
    Oldest,
    /// The newest element.
    Newest,
    /// Number of elements for which the predicate holds.
    CountWhere,
}

/// One `if c then v` arm.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IfArm {
    /// The condition.
    pub condition: ExprNode,
    /// The value when the condition holds.
    pub value: ExprNode,
}

/// The runtime-provided platform values a program may read. The runtime builds one
/// consistent view of all of them per activation.
///
/// [`PlatformRead::from_path`] is THE closed path set: the validator accepts exactly
/// what it resolves and the lowerer maps through it, so a new read is added here alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PlatformRead {
    /// `platform.position.size`: the deployment's signed position on the quoted
    /// outcome, in tokens. Decimal.
    PositionSize,
    /// `platform.position.avg_cost`: average entry price of the open position, `0`
    /// when flat. Decimal.
    PositionAvgCost,
    /// `platform.realized_pnl`: realized profit and loss this window. Decimal.
    RealizedPnl,
    /// `platform.unrealized_pnl`: the open position marked at the current mark.
    /// Decimal.
    UnrealizedPnl,
    /// `platform.cash`: free cash in the sub-account. Decimal.
    Cash,
    /// `platform.open_orders`: the count of live resting orders. Unsigned integer.
    OpenOrders,
    /// `platform.window.remaining_ms`: time to close of the instrument's current
    /// window. Duration, milliseconds.
    WindowRemainingMs,
    /// `platform.last_fill.price`: price of the most recent fill, `0` if none. Decimal.
    LastFillPrice,
}

impl PlatformRead {
    /// Resolve a spelled `platform.<path>` (segments after the head) to a read.
    pub fn from_path(spelled: &[&str]) -> Option<PlatformRead> {
        match spelled {
            ["position", "size"] => Some(PlatformRead::PositionSize),
            ["position", "avg_cost"] => Some(PlatformRead::PositionAvgCost),
            ["realized_pnl"] => Some(PlatformRead::RealizedPnl),
            ["unrealized_pnl"] => Some(PlatformRead::UnrealizedPnl),
            ["cash"] => Some(PlatformRead::Cash),
            ["open_orders"] => Some(PlatformRead::OpenOrders),
            ["window", "remaining_ms"] => Some(PlatformRead::WindowRemainingMs),
            ["last_fill", "price"] => Some(PlatformRead::LastFillPrice),
            _ => None,
        }
    }

    /// Every read's source spelling, for help text.
    pub const PATHS: &'static [&'static str] = &[
        "platform.position.size",
        "platform.position.avg_cost",
        "platform.realized_pnl",
        "platform.unrealized_pnl",
        "platform.cash",
        "platform.open_orders",
        "platform.window.remaining_ms",
        "platform.last_fill.price",
    ];
}

/// The per-leg fill record fields a `legs.<leg>[.tag.<tag>].<field>` read names. The
/// runtime keeps a fill ledger keyed by the leg (and tag) an order was placed from,
/// wiped with the rest of the session at market settlement. Prices are in the
/// strategy's outcome frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LegFillField {
    /// Total quantity filled by orders placed in that leg, this session. Decimal.
    FilledQty,
    /// Volume-weighted average fill price of those orders, `0` if none. Decimal.
    FillPrice,
    /// Price of the most recent fill from that leg, `0` if none. Decimal.
    LastFillPrice,
    /// Number of fill events from that leg. Unsigned integer.
    Fills,
}

impl LegFillField {
    /// Resolve a field spelling.
    pub fn from_name(name: &str) -> Option<LegFillField> {
        match name {
            "filled_qty" => Some(LegFillField::FilledQty),
            "fill_price" => Some(LegFillField::FillPrice),
            "last_fill_price" => Some(LegFillField::LastFillPrice),
            "fills" => Some(LegFillField::Fills),
            _ => None,
        }
    }

    /// Every field's spelling, for help text.
    pub const NAMES: &'static [&'static str] =
        &["filled_qty", "fill_price", "last_fill_price", "fills"];
}

/// The math builtins. `round` is half-up, pinned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Builtin {
    /// `abs(x)`
    Abs,
    /// `min(a, b)`
    Min,
    /// `max(a, b)`
    Max,
    /// `clamp(x, lo, hi)`
    Clamp,
    /// `floor(x)`
    Floor,
    /// `ceil(x)`
    Ceil,
    /// `round(x)`, half-up.
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
}
