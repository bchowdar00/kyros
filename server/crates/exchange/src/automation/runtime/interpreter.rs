//! The interpreter: one activation in, one inert action batch out.
//!
//! `interpret` walks the current leg's blocks in order. For each block:
//!
//! 1. Skip if any `(signal, field)` in `reads` is absent from the activation's
//!    snapshots (the signal has not published yet this session).
//! 2. Evaluate the guard (absent guard is true). Skip if false.
//! 3. Run the items in authored order. `set`/`push` write state now (each
//!    value reads the pre-assignment state). `place`, `cancel`,
//!    `cancel_replace` and `target_position` append inert [`Action`]s with
//!    their size and price evaluated now.
//! 4. If `then` is present, append [`Action::SwitchLeg`] and stop the walk.
//!
//! No venue call happens here; the batch executor runs the actions.

use std::collections::BTreeMap;

use rust_decimal::Decimal;
use strategy_compiler::program::{ExprNode, Item, LegFillField, OrderDef, PlatformRead, Program, TableDef};

use crate::automation::eval::{EvalCtx, EvalError, eval};
use crate::automation::sme::Activation;
use crate::automation::value::Value;

use super::state::{StateError, StateStore};

/// One consistent view of the deployment's account and market, built once per
/// activation by the venue adapter.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PlatformView {
    /// Signed position on the quoted outcome, tokens.
    pub position_size: Decimal,
    /// Average entry price of the open position, 0 when flat.
    pub position_avg_cost: Decimal,
    pub realized_pnl: Decimal,
    pub unrealized_pnl: Decimal,
    pub cash: Decimal,
    pub open_orders: u64,
    pub window_remaining_ms: u64,
    /// Price of the most recent fill, 0 if none.
    pub last_fill_price: Decimal,
}

/// Fill statistics for orders placed from one leg (and optionally one tag).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FillStats {
    pub qty: Decimal,
    pub notional: Decimal,
    pub last_price: Decimal,
    pub count: u64,
}

/// `legs.<leg>[.tag.<tag>].<field>` reads. Wiped with the session.
#[derive(Debug, Clone, Default)]
pub struct FillLedger {
    stats: BTreeMap<(String, Option<String>), FillStats>,
}

impl FillLedger {
    /// Record a fill of `qty` at `price` (outcome frame) from an order placed
    /// in `leg` with `tag`. Updates both the leg total and the tag row.
    pub fn record(&mut self, leg: &str, tag: &str, qty: Decimal, price: Decimal) {
        for key in [(leg.to_string(), None), (leg.to_string(), Some(tag.to_string()))] {
            let s = self.stats.entry(key).or_default();
            s.qty += qty;
            s.notional += qty * price;
            s.last_price = price;
            s.count += 1;
        }
    }

    pub fn read(&self, leg: &str, tag: Option<&str>, field: LegFillField) -> Value {
        let s = self.stats.get(&(leg.to_string(), tag.map(str::to_string))).cloned().unwrap_or_default();
        match field {
            LegFillField::FilledQty => Value::Decimal(s.qty),
            LegFillField::FillPrice => Value::Decimal(if s.qty.is_zero() { Decimal::ZERO } else { s.notional / s.qty }),
            LegFillField::LastFillPrice => Value::Decimal(s.last_price),
            LegFillField::Fills => Value::from_u64(s.count),
        }
    }

    pub fn clear(&mut self) {
        self.stats.clear();
    }
}

/// An order with its expressions resolved for this activation.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedOrder {
    pub def: OrderDef,
    pub size: Decimal,
    pub price: Option<Decimal>,
}

/// One inert instruction for the batch executor.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Place { intent_id: String, order: ResolvedOrder },
    Cancel { tag: String },
    CancelReplace { intent_id: String, order: ResolvedOrder },
    TargetPosition { intent_id: String, order: ResolvedOrder },
    SwitchLeg(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum InterpError {
    UnknownLeg(String),
    Eval { block: String, error: EvalError },
    State { block: String, error: StateError },
    /// A guard or condition evaluated to a non-boolean (program-format
    /// violation).
    GuardNotBoolean(String),
    SizeNotNumeric(String),
}

impl std::fmt::Display for InterpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InterpError::UnknownLeg(l) => write!(f, "unknown leg `{l}`"),
            InterpError::Eval { block, error } => write!(f, "block `{block}`: {error:?}"),
            InterpError::State { block, error } => write!(f, "block `{block}`: {error:?}"),
            InterpError::GuardNotBoolean(b) => write!(f, "block `{b}`: guard is not boolean"),
            InterpError::SizeNotNumeric(b) => write!(f, "block `{b}`: size is not numeric"),
        }
    }
}

/// Everything a body expression may read.
pub struct InterpCtx<'a> {
    pub program: &'a Program,
    pub activation: &'a Activation,
    pub state: &'a StateStore,
    pub params: &'a BTreeMap<String, Value>,
    pub platform: &'a PlatformView,
    pub fills: &'a FillLedger,
}

impl EvalCtx for InterpCtx<'_> {
    fn signal(&self, signal_id: &str, field: &str) -> Result<Value, EvalError> {
        self.activation
            .snapshots
            .get(signal_id)
            .and_then(|s| s.get(field))
            .cloned()
            .ok_or_else(|| EvalError::MissingSignalField { signal_id: signal_id.into(), field: field.into() })
    }

    fn param(&self, name: &str) -> Result<Value, EvalError> {
        if let Some(v) = self.params.get(name) {
            return Ok(v.clone());
        }
        // Fall back to the declared default so a deployment that never set
        // params still runs.
        let def = self.program.params.iter().find(|p| p.name == name).ok_or_else(|| EvalError::UnknownParam(name.into()))?;
        eval(&def.default, self)
    }

    /// Bodies pin `x / 0` to `0`.
    fn division_by_zero(&self) -> Option<Decimal> {
        Some(Decimal::ZERO)
    }

    fn event(&self, field: &str) -> Result<Value, EvalError> {
        self.activation
            .event
            .as_ref()
            .and_then(|e| e.get(field))
            .cloned()
            .ok_or_else(|| EvalError::MissingEventField(field.into()))
    }

    fn state(&self, slot: &str) -> Result<Value, EvalError> {
        self.state.scalar(slot).map_err(|_| EvalError::UnknownStateSlot(slot.into()))
    }

    fn state_map(&self, slot: &str, keys: &[Value]) -> Result<Value, EvalError> {
        self.state.map_get(slot, keys).map_err(|_| EvalError::UnknownStateSlot(slot.into()))
    }

    fn window(&self, slot: &str) -> Result<Vec<Decimal>, EvalError> {
        self.state.window(slot).map_err(|_| EvalError::UnknownStateSlot(slot.into()))
    }

    fn platform(&self, read: PlatformRead) -> Result<Value, EvalError> {
        let p = self.platform;
        Ok(match read {
            PlatformRead::PositionSize => Value::Decimal(p.position_size),
            PlatformRead::PositionAvgCost => Value::Decimal(p.position_avg_cost),
            PlatformRead::RealizedPnl => Value::Decimal(p.realized_pnl),
            PlatformRead::UnrealizedPnl => Value::Decimal(p.unrealized_pnl),
            PlatformRead::Cash => Value::Decimal(p.cash),
            PlatformRead::OpenOrders => Value::from_u64(p.open_orders),
            PlatformRead::WindowRemainingMs => Value::from_u64(p.window_remaining_ms),
            PlatformRead::LastFillPrice => Value::Decimal(p.last_fill_price),
        })
    }

    fn leg_fill(&self, leg: &str, tag: Option<&str>, field: LegFillField) -> Result<Value, EvalError> {
        Ok(self.fills.read(leg, tag, field))
    }

    fn triggered(&self, name: &str) -> Result<bool, EvalError> {
        Ok(self.activation.fired.contains(name))
    }

    fn table(&self, name: &str) -> Result<&TableDef, EvalError> {
        self.program.tables.get(name).ok_or_else(|| EvalError::UnknownTable(name.into()))
    }
}

/// Walk `leg`'s blocks for one activation, mutating `state`, and return the
/// action batch.
pub fn interpret(
    program: &Program,
    leg: &str,
    activation: &Activation,
    state: &mut StateStore,
    params: &BTreeMap<String, Value>,
    platform: &PlatformView,
    fills: &FillLedger,
) -> Result<Vec<Action>, InterpError> {
    let leg_def = program.legs.iter().find(|l| l.name == leg).ok_or_else(|| InterpError::UnknownLeg(leg.into()))?;
    let mut actions = Vec::new();
    for block in &leg_def.blocks {
        let name = block.name.clone();
        // 1. Every read must have published.
        let all_present = block
            .reads
            .iter()
            .all(|r| activation.snapshots.get(&r.signal_id).is_some_and(|s| s.contains_key(&r.field)));
        if !all_present {
            continue;
        }
        // 2. Guard.
        if let Some(guard) = &block.guard {
            let ctx = InterpCtx { program, activation, state, params, platform, fills };
            let v = eval(guard, &ctx).map_err(|error| InterpError::Eval { block: name.clone(), error })?;
            if !v.as_bool().ok_or_else(|| InterpError::GuardNotBoolean(name.clone()))? {
                continue;
            }
        }
        // 3. Items.
        for item in &block.items {
            match item {
                Item::Set { slot, keys, value } => {
                    let ctx = InterpCtx { program, activation, state, params, platform, fills };
                    let v = eval(value, &ctx).map_err(|error| InterpError::Eval { block: name.clone(), error })?;
                    let keys = match keys {
                        Some(ks) => Some(
                            ks.iter()
                                .map(|k| eval(k, &ctx))
                                .collect::<Result<Vec<_>, _>>()
                                .map_err(|error| InterpError::Eval { block: name.clone(), error })?,
                        ),
                        None => None,
                    };
                    let result = match keys {
                        Some(keys) => state.map_set(slot, &keys, v).map(|_| ()),
                        None => state.set_scalar(slot, v),
                    };
                    result.map_err(|error| InterpError::State { block: name.clone(), error })?;
                }
                Item::Push { slot, value } => {
                    let ctx = InterpCtx { program, activation, state, params, platform, fills };
                    let v = eval(value, &ctx).map_err(|error| InterpError::Eval { block: name.clone(), error })?;
                    state.push(slot, &v).map_err(|error| InterpError::State { block: name.clone(), error })?;
                }
                Item::Place { intent_id, order } => {
                    let ctx = InterpCtx { program, activation, state, params, platform, fills };
                    actions.push(Action::Place { intent_id: intent_id.clone(), order: resolve_order(order, &ctx, &name)? });
                }
                Item::Cancel { tag } => actions.push(Action::Cancel { tag: tag.clone() }),
                Item::CancelReplace { intent_id, order } => {
                    let ctx = InterpCtx { program, activation, state, params, platform, fills };
                    actions.push(Action::CancelReplace { intent_id: intent_id.clone(), order: resolve_order(order, &ctx, &name)? });
                }
                Item::TargetPosition { intent_id, order } => {
                    let ctx = InterpCtx { program, activation, state, params, platform, fills };
                    actions.push(Action::TargetPosition { intent_id: intent_id.clone(), order: resolve_order(order, &ctx, &name)? });
                }
            }
        }
        // 4. Transition ends the walk.
        if let Some(then) = &block.then {
            actions.push(Action::SwitchLeg(then.clone()));
            break;
        }
    }
    Ok(actions)
}

fn resolve_order(def: &OrderDef, ctx: &InterpCtx<'_>, block: &str) -> Result<ResolvedOrder, InterpError> {
    let num = |node: &ExprNode| -> Result<Decimal, InterpError> {
        eval(node, ctx)
            .map_err(|error| InterpError::Eval { block: block.into(), error })?
            .as_decimal()
            .ok_or_else(|| InterpError::SizeNotNumeric(block.into()))
    };
    let size = num(&def.size)?;
    let price = match &def.price {
        Some(p) => Some(num(p)?),
        None => None,
    };
    Ok(ResolvedOrder { def: def.clone(), size, price })
}
