//! The runtime value type shared by the signals platform, the SME and the
//! interpreter.
//!
//! Everything numeric is a [`Decimal`]: decimals, the registry's `u8`/`u32`
//! integers, and `duration_ms` (as a count of milliseconds). That matches the
//! compiler's typing, which folds all of those into one `Numeric` class so
//! `platform.window.remaining_ms > 15s` type-checks. Enum variants and free
//! text are both [`Value::Text`]; the validator already guaranteed a variant
//! is only ever compared with a variant of the same field.
//!
//! No `f64` crosses this boundary. The analytic builtins (`exp`, `ln`, `sqrt`,
//! `pow`, `norm_cdf`) run inside `rust_decimal`'s `maths` feature.

use std::fmt;

use rust_decimal::Decimal;

/// A runtime value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    /// Any number: decimals, integers, durations in milliseconds.
    Decimal(Decimal),
    /// A boolean.
    Bool(bool),
    /// An enum variant or free text.
    Text(String),
}

impl Value {
    pub fn zero() -> Value {
        Value::Decimal(Decimal::ZERO)
    }

    pub fn from_u64(n: u64) -> Value {
        Value::Decimal(Decimal::from(n))
    }

    pub fn as_decimal(&self) -> Option<Decimal> {
        match self {
            Value::Decimal(d) => Some(*d),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            Value::Text(s) => Some(s),
            _ => None,
        }
    }

    /// The value as the compiler's canonical string form: decimals normalized
    /// (no trailing zeros), booleans `true`/`false`, text verbatim.
    pub fn canonical(&self) -> String {
        match self {
            Value::Decimal(d) => d.normalize().to_string(),
            Value::Bool(b) => b.to_string(),
            Value::Text(s) => s.clone(),
        }
    }

    /// Parse a canonical literal against the shape of `like`: a decimal when
    /// `like` is numeric, `true`/`false` when boolean, verbatim text otherwise.
    /// `None` when the literal does not parse in that shape.
    pub fn parse_like(literal: &str, like: &Value) -> Option<Value> {
        match like {
            Value::Decimal(_) => parse_decimal(literal).map(Value::Decimal),
            Value::Bool(_) => match literal {
                "true" => Some(Value::Bool(true)),
                "false" => Some(Value::Bool(false)),
                _ => None,
            },
            Value::Text(_) => Some(Value::Text(literal.to_string())),
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.canonical())
    }
}

/// Parse a canonical decimal string. Exact: `"0.04"` is exactly 4/100.
pub fn parse_decimal(s: &str) -> Option<Decimal> {
    s.parse::<Decimal>().ok()
}

/// Compare two values with a comparator. Ordering comparators on text or
/// booleans are `None` (the validator forbids them; treat as "does not hold").
pub fn compare(cmp: strategy_compiler::ir::Comparator, lhs: &Value, rhs: &Value) -> Option<bool> {
    use strategy_compiler::ir::Comparator as C;
    match (lhs, rhs) {
        (Value::Decimal(a), Value::Decimal(b)) => Some(match cmp {
            C::Eq => a == b,
            C::Ne => a != b,
            C::Lt => a < b,
            C::Le => a <= b,
            C::Gt => a > b,
            C::Ge => a >= b,
        }),
        (Value::Bool(a), Value::Bool(b)) => match cmp {
            C::Eq => Some(a == b),
            C::Ne => Some(a != b),
            _ => None,
        },
        (Value::Text(a), Value::Text(b)) => match cmp {
            C::Eq => Some(a == b),
            C::Ne => Some(a != b),
            _ => None,
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use strategy_compiler::ir::Comparator;

    #[test]
    fn canonical_normalizes_decimals() {
        assert_eq!(Value::Decimal(parse_decimal("0.400").unwrap()).canonical(), "0.4");
        assert_eq!(Value::Decimal(Decimal::from(5)).canonical(), "5");
        assert_eq!(Value::Bool(true).canonical(), "true");
    }

    #[test]
    fn parse_like_follows_shape() {
        assert_eq!(
            Value::parse_like("0.3", &Value::zero()),
            Some(Value::Decimal(parse_decimal("0.3").unwrap()))
        );
        assert_eq!(Value::parse_like("yes", &Value::Text(String::new())), Some(Value::Text("yes".into())));
        assert_eq!(Value::parse_like("maybe", &Value::Bool(false)), None);
        assert_eq!(Value::parse_like("abc", &Value::zero()), None);
    }

    #[test]
    fn compare_is_exact_and_typed() {
        let a = Value::Decimal(parse_decimal("0.1").unwrap());
        let b = Value::Decimal(parse_decimal("0.2").unwrap());
        let sum = Value::Decimal(a.as_decimal().unwrap() + b.as_decimal().unwrap());
        assert_eq!(compare(Comparator::Eq, &sum, &Value::Decimal(parse_decimal("0.3").unwrap())), Some(true));
        assert_eq!(compare(Comparator::Lt, &a, &b), Some(true));
        assert_eq!(compare(Comparator::Lt, &Value::Text("a".into()), &Value::Text("b".into())), None);
        assert_eq!(compare(Comparator::Eq, &a, &Value::Text("0.1".into())), None);
    }
}
