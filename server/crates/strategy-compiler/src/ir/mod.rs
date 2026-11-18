//! The Tiny strategy IR, v0.3.
//!
//! Data types and nothing else. The JSON Schema handed to the LLM is generated from these
//! types with `schemars` (`schemars::schema_for!(Strategy)`); they are the single source of truth for the
//! document's shape.
//!
//! Three families live here:
//! - the IR document ([`Strategy`] and its sections) — LLM-authored, `JsonSchema`-deriving;
//! - the compiler inputs ([`Envelope`], [`Registry`]) — submission/venue-supplied, serde-only;
//! - the shared string newtypes and [`NamedMap`] container they are all built from.
//!
//! The only non-derive code is serde impls that make illegal states unrepresentable
//! (duplicate map keys, malformed identifiers, non-canonical decimals, a trigger with
//! both a condition and a timer); everything else is plain data.

pub mod block;
pub mod envelope;
pub mod expressions;
pub mod intent;
pub mod leg;
pub mod manifest;
pub mod meta;
pub mod named_map;
pub mod param;
pub mod registry;
pub mod scalars;
pub mod state;
pub mod strategy;
pub mod trigger;
pub mod type_spec;

pub use block::Block;
pub use envelope::Envelope;
pub use expressions::{Expressions, Interpolation, Table};
pub use intent::{
    Cancel, CancelItem, CancelReplace, CancelReplaceItem, DoItem, OnDest, OnTable, OrderType,
    Place, PlaceItem, PriceBand, Side, TargetPosition, TargetPositionItem, Tif,
};
pub use leg::Leg;
pub use manifest::{
    LifecycleStage, Manifest, StalenessPolicy, SubscriptionRow, WatchField, WindowClockParams,
    WireArithOp, WireComparator, WireCondition, WireExpr, WireIntentEntry, WireLeaf, WireOnTable,
    WireRearm, WireTimer, WireTimerKind, WireTrigger, WireTriggerSource,
};
pub use meta::{DslVersion, Meta};
pub use named_map::NamedMap;
pub use param::{Param, ParamValue};
pub use registry::{
    Capabilities, FieldClass, FieldDef, FieldType, Market, MarketStructure, Quote, Registry, Series,
    SignalDef, SignalLocation,
};
pub use scalars::{DecimalStr, DurationStr, ExprStr, Ident, SignedDecimalStr, StmtStr};
pub use state::{OverflowPolicy, StateSlot};
pub use strategy::Strategy;
pub use trigger::condition::{
    AllGroup, AnyGroup, BareLeaf, Comparator, Condition, ConditionLeaf, ExprLeaf, FieldLeaf,
    LeafValue,
};
pub use trigger::timer::TimerSpec;
pub use trigger::{Fire, Rearm, RearmMode, Trigger, TriggerSource};
pub use type_spec::{PrimitiveType, TypeKind, TypeSpec, TypeSpecObject};
