//! The per-deployment state store: scalar slots, bounded map slots, and
//! bounded numeric windows. Seeded from `Program.state`, wiped back to the
//! defaults at every market session reset.

use std::collections::{BTreeMap, VecDeque};

use rust_decimal::Decimal;
use strategy_compiler::ir::OverflowPolicy;
use strategy_compiler::program::{ExprNode, StateSlot};

use crate::automation::eval::{EvalCtx, EvalError, eval};
use crate::automation::value::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateError {
    UnknownSlot(String),
    /// A scalar access on a map slot or vice versa.
    WrongKind(String),
    KeyArity { slot: String, expected: usize, got: usize },
    /// A `push` of a non-numeric value.
    NotNumeric(String),
    BadDefault(String, EvalError),
}

#[derive(Debug, Clone)]
struct MapSlot {
    key_arity: usize,
    default: Value,
    max_keys: u32,
    overflow: OverflowPolicy,
    entries: BTreeMap<Vec<String>, Value>,
}

#[derive(Debug, Clone)]
struct WindowSlot {
    capacity: usize,
    buf: VecDeque<Decimal>,
}

#[derive(Debug, Clone, Default)]
pub struct StateStore {
    scalars: BTreeMap<String, Value>,
    scalar_defaults: BTreeMap<String, Value>,
    maps: BTreeMap<String, MapSlot>,
    windows: BTreeMap<String, WindowSlot>,
}

/// Literal defaults need no reads at all.
struct NoReads;

impl EvalCtx for NoReads {
    fn signal(&self, signal_id: &str, field: &str) -> Result<Value, EvalError> {
        Err(EvalError::MissingSignalField { signal_id: signal_id.into(), field: field.into() })
    }
    fn param(&self, name: &str) -> Result<Value, EvalError> {
        Err(EvalError::UnknownParam(name.into()))
    }
    fn division_by_zero(&self) -> Option<Decimal> {
        Some(Decimal::ZERO)
    }
}

impl StateStore {
    pub fn new(slots: &[StateSlot]) -> Result<StateStore, StateError> {
        let mut store = StateStore::default();
        for slot in slots {
            match slot {
                StateSlot::Scalar { name, default } => {
                    let v = literal(name, default)?;
                    store.scalar_defaults.insert(name.clone(), v.clone());
                    store.scalars.insert(name.clone(), v);
                }
                StateSlot::Window { name, capacity } => {
                    store.windows.insert(name.clone(), WindowSlot { capacity: (*capacity).max(1) as usize, buf: VecDeque::new() });
                }
                StateSlot::Map { name, key_arity, default, max_keys, overflow } => {
                    store.maps.insert(
                        name.clone(),
                        MapSlot { key_arity: *key_arity, default: literal(name, default)?, max_keys: *max_keys, overflow: *overflow, entries: BTreeMap::new() },
                    );
                }
            }
        }
        Ok(store)
    }

    /// Back to the declared defaults: scalars reseeded, maps and windows emptied.
    pub fn clear(&mut self) {
        for (name, default) in &self.scalar_defaults {
            self.scalars.insert(name.clone(), default.clone());
        }
        for m in self.maps.values_mut() {
            m.entries.clear();
        }
        for w in self.windows.values_mut() {
            w.buf.clear();
        }
    }

    pub fn scalar(&self, slot: &str) -> Result<Value, StateError> {
        if let Some(v) = self.scalars.get(slot) {
            return Ok(v.clone());
        }
        if self.maps.contains_key(slot) || self.windows.contains_key(slot) {
            return Err(StateError::WrongKind(slot.into()));
        }
        Err(StateError::UnknownSlot(slot.into()))
    }

    pub fn set_scalar(&mut self, slot: &str, value: Value) -> Result<(), StateError> {
        match self.scalars.get_mut(slot) {
            Some(v) => {
                *v = value;
                Ok(())
            }
            None if self.maps.contains_key(slot) || self.windows.contains_key(slot) => Err(StateError::WrongKind(slot.into())),
            None => Err(StateError::UnknownSlot(slot.into())),
        }
    }

    pub fn map_get(&self, slot: &str, keys: &[Value]) -> Result<Value, StateError> {
        let m = self.map(slot)?;
        if keys.len() != m.key_arity {
            return Err(StateError::KeyArity { slot: slot.into(), expected: m.key_arity, got: keys.len() });
        }
        let key: Vec<String> = keys.iter().map(Value::canonical).collect();
        Ok(m.entries.get(&key).cloned().unwrap_or_else(|| m.default.clone()))
    }

    /// Write a map entry. A new key on a full map follows the overflow
    /// policy (`RefuseUpdate`: dropped, existing entries untouched). Returns
    /// whether the write took effect.
    pub fn map_set(&mut self, slot: &str, keys: &[Value], value: Value) -> Result<bool, StateError> {
        let arity = self.map(slot)?.key_arity;
        if keys.len() != arity {
            return Err(StateError::KeyArity { slot: slot.into(), expected: arity, got: keys.len() });
        }
        let key: Vec<String> = keys.iter().map(Value::canonical).collect();
        let m = self.maps.get_mut(slot).expect("checked above");
        if !m.entries.contains_key(&key) && m.entries.len() >= m.max_keys as usize {
            return Ok(match m.overflow {
                OverflowPolicy::RefuseUpdate => false,
            });
        }
        m.entries.insert(key, value);
        Ok(true)
    }

    pub fn push(&mut self, slot: &str, value: &Value) -> Result<(), StateError> {
        let d = value.as_decimal().ok_or_else(|| StateError::NotNumeric(slot.into()))?;
        let w = self.window_mut(slot)?;
        if w.buf.len() >= w.capacity {
            w.buf.pop_front();
        }
        w.buf.push_back(d);
        Ok(())
    }

    /// Elements oldest first.
    pub fn window(&self, slot: &str) -> Result<Vec<Decimal>, StateError> {
        match self.windows.get(slot) {
            Some(w) => Ok(w.buf.iter().copied().collect()),
            None if self.scalars.contains_key(slot) || self.maps.contains_key(slot) => Err(StateError::WrongKind(slot.into())),
            None => Err(StateError::UnknownSlot(slot.into())),
        }
    }

    /// Every scalar's current value, for status payloads.
    pub fn scalars(&self) -> &BTreeMap<String, Value> {
        &self.scalars
    }

    fn map(&self, slot: &str) -> Result<&MapSlot, StateError> {
        match self.maps.get(slot) {
            Some(m) => Ok(m),
            None if self.scalars.contains_key(slot) || self.windows.contains_key(slot) => Err(StateError::WrongKind(slot.into())),
            None => Err(StateError::UnknownSlot(slot.into())),
        }
    }

    fn window_mut(&mut self, slot: &str) -> Result<&mut WindowSlot, StateError> {
        let wrong = self.scalars.contains_key(slot) || self.maps.contains_key(slot);
        match self.windows.get_mut(slot) {
            Some(w) => Ok(w),
            None if wrong => Err(StateError::WrongKind(slot.into())),
            None => Err(StateError::UnknownSlot(slot.into())),
        }
    }
}

fn literal(slot: &str, node: &ExprNode) -> Result<Value, StateError> {
    eval(node, &NoReads).map_err(|e| StateError::BadDefault(slot.into(), e))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slots(json: serde_json::Value) -> Vec<StateSlot> {
        serde_json::from_value(json).unwrap()
    }

    fn store() -> StateStore {
        StateStore::new(&slots(serde_json::json!([
            {"kind": "scalar", "name": "quotes", "default": {"op": "decimal", "value": "0"}},
            {"kind": "scalar", "name": "armed", "default": {"op": "bool", "value": false}},
            {"kind": "window", "name": "spreads", "capacity": 3},
            {"kind": "map", "name": "seen", "key_arity": 2, "default": {"op": "decimal", "value": "0"}, "max_keys": 2, "overflow": "refuse_update"}
        ])))
        .unwrap()
    }

    fn d(s: &str) -> Value {
        Value::Decimal(s.parse().unwrap())
    }

    #[test]
    fn scalars_seed_set_and_clear() {
        let mut s = store();
        assert_eq!(s.scalar("quotes").unwrap(), d("0"));
        s.set_scalar("quotes", d("3")).unwrap();
        s.set_scalar("armed", Value::Bool(true)).unwrap();
        assert_eq!(s.scalar("quotes").unwrap(), d("3"));
        s.clear();
        assert_eq!(s.scalar("quotes").unwrap(), d("0"));
        assert_eq!(s.scalar("armed").unwrap(), Value::Bool(false));
        assert_eq!(s.scalar("nope"), Err(StateError::UnknownSlot("nope".into())));
        assert_eq!(s.scalar("spreads"), Err(StateError::WrongKind("spreads".into())));
    }

    #[test]
    fn windows_evict_oldest_at_capacity() {
        let mut s = store();
        for v in ["1", "2", "3", "4"] {
            s.push("spreads", &d(v)).unwrap();
        }
        assert_eq!(s.window("spreads").unwrap(), vec!["2".parse().unwrap(), "3".parse().unwrap(), "4".parse().unwrap()]);
        assert_eq!(s.push("spreads", &Value::Bool(true)), Err(StateError::NotNumeric("spreads".into())));
        s.clear();
        assert!(s.window("spreads").unwrap().is_empty());
    }

    #[test]
    fn maps_default_bound_and_refuse_overflow() {
        let mut s = store();
        let k1 = [d("1"), Value::Text("a".into())];
        let k2 = [d("2"), Value::Text("b".into())];
        let k3 = [d("3"), Value::Text("c".into())];
        assert_eq!(s.map_get("seen", &k1).unwrap(), d("0"));
        assert!(s.map_set("seen", &k1, d("10")).unwrap());
        assert!(s.map_set("seen", &k2, d("20")).unwrap());
        assert!(!s.map_set("seen", &k3, d("30")).unwrap(), "full: refused");
        assert!(s.map_set("seen", &k1, d("11")).unwrap(), "existing key still writable");
        assert_eq!(s.map_get("seen", &k1).unwrap(), d("11"));
        assert_eq!(s.map_get("seen", &k3).unwrap(), d("0"));
        assert_eq!(
            s.map_get("seen", &[d("1")]),
            Err(StateError::KeyArity { slot: "seen".into(), expected: 2, got: 1 })
        );
        // Keys compare by canonical form: 1.0 and 1 are the same key.
        assert_eq!(s.map_get("seen", &[d("1.0"), Value::Text("a".into())]).unwrap(), d("11"));
    }
}
