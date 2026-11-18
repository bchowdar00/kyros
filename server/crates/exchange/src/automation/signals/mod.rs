//! Signal ingestion: the catalogue, typed events, and the snapshot store.
//!
//! The catalogue is loaded from the compiler's `ir::Registry` (the same file
//! strategies are validated against), so a strategy can only subscribe to ids
//! the platform knows. Every signal starts disabled; a publisher enables its
//! own signal when it starts. Ingest is whole-event only: the field set must
//! equal the declared set, every value is parsed exactly against its declared
//! type, and the snapshot is replaced atomically.

use std::collections::BTreeMap;

use rust_decimal::Decimal;
use strategy_compiler::ir::Registry;
use strategy_compiler::ir::registry::FieldType;

use super::value::{Value, parse_decimal};

/// A publisher-supplied field value, before typing against the registry.
#[derive(Debug, Clone, PartialEq)]
pub enum SignalValue {
    Decimal(Decimal),
    UInt(u64),
    Bool(bool),
    /// An enum variant or free text.
    Text(String),
}

impl SignalValue {
    pub fn decimal(s: &str) -> SignalValue {
        SignalValue::Decimal(parse_decimal(s).unwrap_or(Decimal::ZERO))
    }

    pub fn text(s: &str) -> SignalValue {
        SignalValue::Text(s.to_string())
    }
}

/// One whole publisher event.
#[derive(Debug, Clone, PartialEq)]
pub struct SignalEvent {
    pub signal_id: String,
    pub fields: BTreeMap<String, SignalValue>,
    pub ts_ms: u64,
}

impl SignalEvent {
    pub fn new(signal_id: &str, ts_ms: u64, fields: impl IntoIterator<Item = (&'static str, SignalValue)>) -> Self {
        SignalEvent {
            signal_id: signal_id.to_string(),
            fields: fields.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
            ts_ms,
        }
    }
}

/// Why an event was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IngestError {
    UnknownSignal(String),
    Disabled(String),
    /// The field set differs from the declaration.
    FieldSetMismatch { signal_id: String, missing: Vec<String>, extra: Vec<String> },
    /// A value did not fit its declared type.
    BadValue { signal_id: String, field: String, reason: String },
}

impl std::fmt::Display for IngestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IngestError::UnknownSignal(s) => write!(f, "unknown signal `{s}`"),
            IngestError::Disabled(s) => write!(f, "signal `{s}` is disabled"),
            IngestError::FieldSetMismatch { signal_id, missing, extra } => {
                write!(f, "{signal_id}: whole event field mismatch; missing={missing:?}, extra={extra:?}")
            }
            IngestError::BadValue { signal_id, field, reason } => write!(f, "{signal_id}.{field}: {reason}"),
        }
    }
}

impl std::error::Error for IngestError {}

/// One catalogued signal.
#[derive(Debug, Clone)]
pub struct SignalDesc {
    pub signal_id: String,
    pub enabled: bool,
    /// Declared fields in registry order.
    pub fields: Vec<(String, FieldType)>,
}

impl SignalDesc {
    fn field(&self, name: &str) -> Option<&FieldType> {
        self.fields.iter().find(|(n, _)| n == name).map(|(_, t)| t)
    }
}

/// A stored field: the exact value plus when it was published.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldRecord {
    pub value: Value,
    pub ts_ms: u64,
}

/// Latest snapshot of one signal: field -> record.
pub type Snapshot = BTreeMap<String, FieldRecord>;

/// The catalogue plus every signal's latest snapshot.
#[derive(Debug, Default)]
pub struct SignalStore {
    catalogue: BTreeMap<String, SignalDesc>,
    snapshots: BTreeMap<String, Snapshot>,
}

impl SignalStore {
    /// Catalogue every signal in `registry`, all disabled.
    pub fn from_registry(registry: &Registry) -> Self {
        let mut store = SignalStore::default();
        for def in &registry.signals {
            store.register(
                &def.signal_id,
                def.fields.iter().map(|f| (f.name.as_str().to_string(), f.field_type.clone())),
            );
        }
        store
    }

    /// Add a signal to the catalogue (disabled). Re-registering an id
    /// replaces its descriptor and keeps its snapshot.
    pub fn register(&mut self, signal_id: &str, fields: impl IntoIterator<Item = (String, FieldType)>) {
        self.catalogue.insert(
            signal_id.to_string(),
            SignalDesc { signal_id: signal_id.to_string(), enabled: false, fields: fields.into_iter().collect() },
        );
        self.snapshots.entry(signal_id.to_string()).or_default();
    }

    pub fn enable(&mut self, signal_id: &str) -> bool {
        match self.catalogue.get_mut(signal_id) {
            Some(desc) => {
                desc.enabled = true;
                true
            }
            None => false,
        }
    }

    pub fn contains(&self, signal_id: &str) -> bool {
        self.catalogue.contains_key(signal_id)
    }

    pub fn desc(&self, signal_id: &str) -> Option<&SignalDesc> {
        self.catalogue.get(signal_id)
    }

    pub fn signal_ids(&self) -> impl Iterator<Item = &str> {
        self.catalogue.keys().map(String::as_str)
    }

    /// The latest snapshot of a signal (empty until it first publishes).
    pub fn snapshot(&self, signal_id: &str) -> Option<&Snapshot> {
        self.snapshots.get(signal_id)
    }

    /// Whether the signal has ever published.
    pub fn has_published(&self, signal_id: &str) -> bool {
        self.snapshots.get(signal_id).is_some_and(|s| !s.is_empty())
    }

    /// The exact latest value of one field.
    pub fn field(&self, signal_id: &str, field: &str) -> Option<&Value> {
        self.snapshots.get(signal_id)?.get(field).map(|r| &r.value)
    }

    /// Forget a signal's snapshot, as if it had never published. The
    /// catalogue entry and its enabled flag are untouched, so the next
    /// whole event ingests normally. Returns whether anything was cleared.
    pub fn clear(&mut self, signal_id: &str) -> bool {
        match self.snapshots.get_mut(signal_id) {
            Some(snapshot) if !snapshot.is_empty() => {
                snapshot.clear();
                true
            }
            _ => false,
        }
    }

    /// Validate and store one whole event. Returns the typed field values that
    /// were stored, in declared order. Unknown and disabled ids are refused
    /// with an error the caller may treat as a silent no-op.
    pub fn ingest(&mut self, event: &SignalEvent) -> Result<BTreeMap<String, Value>, IngestError> {
        let desc = self
            .catalogue
            .get(&event.signal_id)
            .ok_or_else(|| IngestError::UnknownSignal(event.signal_id.clone()))?;
        if !desc.enabled {
            return Err(IngestError::Disabled(event.signal_id.clone()));
        }
        let declared: Vec<&str> = desc.fields.iter().map(|(n, _)| n.as_str()).collect();
        let missing: Vec<String> = declared
            .iter()
            .filter(|n| !event.fields.contains_key(**n))
            .map(|n| n.to_string())
            .collect();
        let extra: Vec<String> = event.fields.keys().filter(|k| !declared.contains(&k.as_str())).cloned().collect();
        if !missing.is_empty() || !extra.is_empty() {
            return Err(IngestError::FieldSetMismatch { signal_id: event.signal_id.clone(), missing, extra });
        }
        let mut typed = BTreeMap::new();
        for (name, raw) in &event.fields {
            let ty = desc.field(name).expect("field set was checked");
            let value = type_value(ty, raw).map_err(|reason| IngestError::BadValue {
                signal_id: event.signal_id.clone(),
                field: name.clone(),
                reason,
            })?;
            typed.insert(name.clone(), value);
        }
        let snapshot = self.snapshots.entry(event.signal_id.clone()).or_default();
        for (name, value) in &typed {
            snapshot.insert(name.clone(), FieldRecord { value: value.clone(), ts_ms: event.ts_ms });
        }
        Ok(typed)
    }
}

/// Parse a publisher value against a declared type. Exact: decimals stay
/// decimals, integers are range-checked, variants must be declared.
pub fn type_value(ty: &FieldType, raw: &SignalValue) -> Result<Value, String> {
    match ty {
        FieldType::Decimal => match raw {
            SignalValue::Decimal(d) => Ok(Value::Decimal(*d)),
            SignalValue::UInt(n) => Ok(Value::Decimal(Decimal::from(*n))),
            SignalValue::Text(s) => parse_decimal(s).map(Value::Decimal).ok_or_else(|| format!("`{s}` is not a decimal")),
            SignalValue::Bool(_) => Err("expected decimal, got bool".into()),
        },
        FieldType::U8
        | FieldType::U16
        | FieldType::U32
        | FieldType::U64
        | FieldType::DurationMs
        | FieldType::Timestamp => {
            let n = match raw {
                SignalValue::UInt(n) => *n,
                SignalValue::Decimal(d) if d.fract().is_zero() && d.is_sign_positive() || d.is_zero() => {
                    u64::try_from(d.trunc().mantissa()).map_err(|_| format!("`{d}` out of range"))?
                }
                SignalValue::Text(s) => s.parse::<u64>().map_err(|_| format!("`{s}` is not an unsigned integer"))?,
                other => return Err(format!("expected unsigned integer, got {other:?}")),
            };
            let max = match ty {
                FieldType::U8 => u64::from(u8::MAX),
                FieldType::U16 => u64::from(u16::MAX),
                FieldType::U32 => u64::from(u32::MAX),
                _ => u64::MAX,
            };
            if n > max {
                return Err(format!("{n} out of range for {ty:?}"));
            }
            Ok(Value::from_u64(n))
        }
        FieldType::Enum { variants } => {
            let s = match raw {
                SignalValue::Text(s) => s.clone(),
                other => return Err(format!("expected variant, got {other:?}")),
            };
            if !variants.contains(&s) {
                return Err(format!("variant `{s}` not in {variants:?}"));
            }
            Ok(Value::Text(s))
        }
        FieldType::I64 => match raw {
            SignalValue::UInt(n) => i64::try_from(*n).map(|i| Value::Decimal(Decimal::from(i))).map_err(|_| format!("{n} out of range for i64")),
            SignalValue::Decimal(d) if d.fract().is_zero() => Ok(Value::Decimal(d.trunc())),
            SignalValue::Text(s) => s.parse::<i64>().map(|i| Value::Decimal(Decimal::from(i))).map_err(|_| format!("`{s}` is not a signed integer")),
            other => Err(format!("expected signed integer, got {other:?}")),
        },
        FieldType::Bool => match raw {
            SignalValue::Bool(b) => Ok(Value::Bool(*b)),
            SignalValue::Text(s) if s == "true" || s == "false" => Ok(Value::Bool(s == "true")),
            other => Err(format!("expected bool, got {other:?}")),
        },
        FieldType::String => match raw {
            SignalValue::Text(s) => Ok(Value::Text(s.clone())),
            SignalValue::Decimal(d) => Ok(Value::Text(d.normalize().to_string())),
            SignalValue::UInt(n) => Ok(Value::Text(n.to_string())),
            SignalValue::Bool(b) => Ok(Value::Text(b.to_string())),
        },
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// The two-signal store the prototype's trigger tests use: `a{x,y}` decimal,
    /// `b{kind}` enum(no|yes), both enabled.
    pub(crate) fn ab_store() -> SignalStore {
        let mut s = SignalStore::default();
        s.register("a", [("x".to_string(), FieldType::Decimal), ("y".to_string(), FieldType::Decimal)]);
        s.register("b", [("kind".to_string(), FieldType::Enum { variants: vec!["no".into(), "yes".into()] })]);
        s.enable("a");
        s.enable("b");
        s
    }

    #[test]
    fn registry_catalogue_starts_disabled() {
        let text = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../strategy-compiler/registry/tiny.json"
        ))
        .unwrap();
        let registry: Registry = serde_json::from_str(&text).unwrap();
        let mut store = SignalStore::from_registry(&registry);
        assert!(store.contains("coinbase.btc_usd.spot.v1"));
        let ev = SignalEvent::new("coinbase.btc_usd.spot.v1", 1, [("last", SignalValue::decimal("50000"))]);
        assert_eq!(store.ingest(&ev), Err(IngestError::Disabled("coinbase.btc_usd.spot.v1".into())));
        store.enable("coinbase.btc_usd.spot.v1");
        store.ingest(&ev).unwrap();
        assert_eq!(store.field("coinbase.btc_usd.spot.v1", "last"), Some(&Value::Decimal(Decimal::from(50000))));
    }

    #[test]
    fn whole_event_requires_every_field() {
        let mut s = ab_store();
        let err = s.ingest(&SignalEvent::new("a", 1, [("x", SignalValue::decimal("1"))])).unwrap_err();
        assert_eq!(
            err,
            IngestError::FieldSetMismatch { signal_id: "a".into(), missing: vec!["y".into()], extra: vec![] }
        );
    }

    #[test]
    fn registry_types_are_checked() {
        let mut s = ab_store();
        let err = s.ingest(&SignalEvent::new("b", 1, [("kind", SignalValue::text("maybe"))])).unwrap_err();
        assert!(matches!(err, IngestError::BadValue { .. }), "{err}");
        s.register("counts", [("small".to_string(), FieldType::U8)]);
        s.enable("counts");
        let err = s.ingest(&SignalEvent::new("counts", 1, [("small", SignalValue::UInt(256))])).unwrap_err();
        assert!(err.to_string().contains("out of range"));
        s.ingest(&SignalEvent::new("counts", 1, [("small", SignalValue::UInt(255))])).unwrap();
    }

    #[test]
    fn clear_forgets_the_snapshot_but_keeps_the_signal_enabled() {
        let mut s = ab_store();
        assert!(!s.clear("a"), "nothing to clear before the first publish");
        s.ingest(&SignalEvent::new("a", 1, [("x", SignalValue::decimal("1")), ("y", SignalValue::decimal("2"))])).unwrap();
        assert!(s.has_published("a"));
        assert!(s.clear("a"));
        assert!(!s.has_published("a"));
        assert_eq!(s.field("a", "x"), None);
        assert!(!s.clear("a"), "idempotent");
        assert!(!s.clear("zzz"), "unknown ids are a no-op");
        s.ingest(&SignalEvent::new("a", 2, [("x", SignalValue::decimal("3")), ("y", SignalValue::decimal("4"))])).unwrap();
        assert_eq!(s.field("a", "x"), Some(&Value::Decimal(Decimal::from(3))));
    }

    #[test]
    fn unknown_signal_is_refused() {
        let mut s = ab_store();
        assert_eq!(
            s.ingest(&SignalEvent::new("zzz", 1, [])),
            Err(IngestError::UnknownSignal("zzz".into()))
        );
    }
}
