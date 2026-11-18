//! Param lowering: the declared type erased to its kind, the default to a typed
//! literal. The validator has already checked that the default fits the type, so every
//! arm here is total.

use crate::ir::{Param, ParamValue, TypeKind};

/// One lowered param.
#[derive(Debug, Clone, PartialEq)]
pub struct ParamLower {
    /// The param name.
    pub name: String,
    /// The value type.
    pub kind: ParamKindLower,
    /// The default, as a typed literal.
    pub default: ParamDefaultLower,
}

/// A param's value type, spelling erased.
#[derive(Debug, Clone, PartialEq)]
pub enum ParamKindLower {
    /// An unsigned 8-bit integer.
    U8,
    /// A signed decimal.
    Decimal,
    /// A boolean.
    Bool,
    /// Free text.
    Str,
    /// A closed variant set.
    Enum(Vec<String>),
}

/// A param default, typed.
#[derive(Debug, Clone, PartialEq)]
pub enum ParamDefaultLower {
    /// A canonical (signed) decimal string; `u8` defaults land here too.
    Decimal(String),
    /// A boolean.
    Bool(bool),
    /// A variant name.
    Variant(String),
    /// Text.
    Str(String),
}

/// Lower the params section, document order.
pub fn lower_params(params: &[Param]) -> Vec<ParamLower> {
    params
        .iter()
        .map(|param| {
            let (kind, default) = match (param.type_spec.kind(), &param.default) {
                (TypeKind::U8, ParamValue::Text(t)) => {
                    (ParamKindLower::U8, ParamDefaultLower::Decimal(t.clone()))
                }
                (TypeKind::Decimal, ParamValue::Text(t)) => {
                    (ParamKindLower::Decimal, ParamDefaultLower::Decimal(t.clone()))
                }
                (TypeKind::Bool, ParamValue::Bool(b)) => {
                    (ParamKindLower::Bool, ParamDefaultLower::Bool(*b))
                }
                (TypeKind::Str, ParamValue::Text(t)) => {
                    (ParamKindLower::Str, ParamDefaultLower::Str(t.clone()))
                }
                (TypeKind::Enum(variants), ParamValue::Text(t)) => (
                    ParamKindLower::Enum(
                        variants.iter().map(|v| v.as_str().to_string()).collect(),
                    ),
                    ParamDefaultLower::Variant(t.clone()),
                ),
                (TypeKind::U8 | TypeKind::Decimal | TypeKind::Str | TypeKind::Enum(_), ParamValue::Bool(_))
                | (TypeKind::Bool, ParamValue::Text(_)) => {
                    unreachable!("the validator rejects param defaults that do not fit the type")
                }
            };
            ParamLower {
                name: param.name.as_str().to_string(),
                kind,
                default,
            }
        })
        .collect()
}
