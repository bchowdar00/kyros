//! State-slot lowering: everything the emitters need to read, write, and default a
//! slot, precomputed from the IR `TypeSpec`s.

use crate::ir::{OverflowPolicy, StateSlot, TypeKind, TypeSpec};

/// A slot's value default — what an uninitialized scalar or an absent map key reads
/// as: zero for numerics, `false` for booleans, the first declared variant for enums.
#[derive(Debug, Clone, PartialEq)]
pub enum DefaultValue {
    /// Zero, for numeric types (`u8`, `decimal`).
    Zero,
    /// `false`, for booleans.
    False,
    /// The empty string, for strings.
    EmptyStr,
    /// The first declared variant, for enums.
    Variant(String),
}

/// One lowered state slot.
#[derive(Debug, Clone, PartialEq)]
pub enum SlotLower {
    /// A scalar slot.
    Scalar {
        /// The slot name.
        name: String,
        /// The IR default (`on_start` materializes it).
        default: DefaultValue,
    },
    /// A bounded ring buffer of numeric values.
    Window {
        /// The slot name.
        name: String,
        /// The ring capacity.
        capacity: u32,
    },
    /// A bounded map slot.
    Map {
        /// The slot name.
        name: String,
        /// Per-position: is the key type numeric? Numeric key parts wrap in
        /// `str(...)` in the encoded-key expression; enum parts are already strings.
        key_numeric: Vec<bool>,
        /// What an absent key reads as (mirrors scalar defaults; the corpus's
        /// `+ 1`-on-first-event counters depend on it).
        value_default: DefaultValue,
        /// The declared capacity.
        max_keys: u32,
        /// The declared overflow policy.
        overflow: OverflowPolicy,
    },
}

impl SlotLower {
    /// The slot's name.
    pub fn name(&self) -> &str {
        match self {
            SlotLower::Scalar { name, .. }
            | SlotLower::Map { name, .. }
            | SlotLower::Window { name, .. } => name,
        }
    }
}

/// Lower the state section, document order.
pub fn lower_state(slots: &[StateSlot]) -> Vec<SlotLower> {
    slots
        .iter()
        .map(|slot| match slot {
            StateSlot::Scalar { name, value_type } => SlotLower::Scalar {
                name: name.as_str().to_string(),
                default: default_of(value_type),
            },
            StateSlot::Window { name, capacity, .. } => SlotLower::Window {
                name: name.as_str().to_string(),
                capacity: *capacity,
            },
            StateSlot::Map {
                name,
                key_types,
                value_type,
                max_keys,
                overflow,
            } => SlotLower::Map {
                name: name.as_str().to_string(),
                key_numeric: key_types.iter().map(is_numeric).collect(),
                value_default: default_of(value_type),
                max_keys: *max_keys,
                overflow: *overflow,
            },
        })
        .collect()
}

/// The IR default of a value type: zero for numerics, `false` for booleans, the first
/// declared variant for enums.
pub fn default_of(spec: &TypeSpec) -> DefaultValue {
    match spec.kind() {
        TypeKind::U8 | TypeKind::Decimal => DefaultValue::Zero,
        TypeKind::Bool => DefaultValue::False,
        TypeKind::Str => DefaultValue::EmptyStr,
        TypeKind::Enum(variants) => DefaultValue::Variant(
            variants
                .first()
                .expect("the validator rejects enums with no variants")
                .as_str()
                .to_string(),
        ),
    }
}

/// Whether a key type is numeric (booleans, strings and enums are symbolic key parts).
fn is_numeric(spec: &TypeSpec) -> bool {
    match spec.kind() {
        TypeKind::U8 | TypeKind::Decimal => true,
        TypeKind::Bool | TypeKind::Str | TypeKind::Enum(_) => false,
    }
}
