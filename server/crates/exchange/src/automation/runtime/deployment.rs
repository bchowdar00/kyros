//! One running deployment: the record, the action batch executor, the
//! lifecycle drain, and the Tokio task that drives them from platform frames
//! and manager commands. Direct port of the prototype's `runner.py` control
//! flow with the interpreter in place of generated handlers.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::sync::Arc;

use rust_decimal::Decimal;
use strategy_compiler::ir::{OrderType, Side, Tif};
use strategy_compiler::program::{OnTable, Program};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

use crate::automation::sme::task::PlatformHandle;
use crate::automation::sme::{Activation, ConnId, Frame, VenueClock};
use crate::automation::value::{Value, parse_decimal};
use crate::automation::venue::{AccountView, ClientOrderId, ExecutionEvent, ExecutionKind, RejectReason, Venue, VenueOrder};
use crate::series::is_quoted_outcome;
use crate::types::{Address, SeriesId};

use super::interpreter::{Action, FillLedger, PlatformView, ResolvedOrder, interpret};
use super::risk::{self, OpenOrders};
use super::state::StateStore;

/// Venue submissions (placements and replacements) per second per
/// deployment. What a deployment costs is engine lock time per order, not
/// wakes, so this is the one hard ceiling; the extra orders are rejected
/// through the intent's `on.reject` table, never paused.
pub const MAX_ORDERS_PER_SEC: usize = 20;
/// Lifecycle events one drain may apply before it is declared non-quiescing.
pub const MAX_LIFECYCLE_EVENTS_PER_DRAIN: usize = 1000;
/// Log lines kept for the status payload.
pub const LOG_LINES_KEPT: usize = 100;

/// Mint a process-unique deploy epoch. `strategy_id` is a content hash of
/// the document, so two deployments of the same document (on two
/// sub-accounts, or a redeploy) need distinct epochs to be distinct
/// deployments; a global counter is the simplest correct choice.
pub fn next_deploy_epoch() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// What the manager needs to start a deployment.
#[derive(Debug, Clone)]
pub struct DeploymentSpec {
    pub program: Arc<Program>,
    pub deploy_epoch: u64,
    pub sub_account: Address,
    /// Home series; every order goes to this series' engine.
    pub series: SeriesId,
    pub params: BTreeMap<String, Value>,
}

impl DeploymentSpec {
    pub fn id(&self) -> String {
        format!("{}:{}", self.program.strategy_id, self.deploy_epoch)
    }
}

/// Commands from the manager to one deployment task.
#[derive(Debug)]
pub enum DeploymentCommand {
    Pause,
    Resume,
    Stop { reply: Option<oneshot::Sender<()>> },
    SetParams(BTreeMap<String, Value>),
    Execution(ExecutionEvent),
    /// A window opened in the home series.
    WindowOpened { market_id: u32 },
    /// The home series settled a window.
    WindowSettled { market_id: u32 },
    Status { reply: oneshot::Sender<DeploymentStatus> },
}

/// One order the runtime placed (or tried to).
#[derive(Debug, Clone, PartialEq)]
pub struct LogicalOrder {
    pub client_order_id: ClientOrderId,
    pub intent_id: String,
    pub tag: String,
    /// The leg it was placed from (fill ledger key).
    pub leg: String,
    pub outcome: Option<String>,
    pub side: Side,
    pub order_type: OrderType,
    pub tif: Tif,
    pub price: Option<Decimal>,
    pub requested: Decimal,
    pub remaining: Decimal,
    pub filled: Decimal,
    pub activation_seq: u64,
    pub attempt: u32,
    pub ordinal: u64,
    pub venue_order_id: Option<u64>,
    pub market_id: Option<u32>,
    pub live: bool,
    /// Snapshot of the intent's transition table at placement.
    pub on: OnTable,
}

/// Status payload for the API.
#[derive(Debug, Clone, PartialEq)]
pub struct DeploymentStatus {
    pub id: String,
    pub strategy_id: String,
    pub deploy_epoch: u64,
    pub series: SeriesId,
    pub current_leg: String,
    pub armed_triggers: Vec<String>,
    pub paused: bool,
    pub stopping: bool,
    pub failure_reason: Option<String>,
    pub activations: u64,
    /// Queued activations absorbed into a later one with the same cause
    /// (see [`coalesce`]).
    pub coalesced: u64,
    /// Submissions refused by the order-rate gate ([`MAX_ORDERS_PER_SEC`]).
    pub rate_rejects: u64,
    pub live_orders: Vec<LogicalOrder>,
    pub params: BTreeMap<String, Value>,
    pub state: BTreeMap<String, Value>,
    pub log: Vec<String>,
}

pub struct RunningDeployment {
    pub id: String,
    pub spec: DeploymentSpec,
    pub conn: ConnId,
    pub current_leg: String,
    pub state: StateStore,
    pub params: BTreeMap<String, Value>,
    pub fills: FillLedger,
    pub activation_seq: u64,
    pub coalesced: u64,
    pub rate_rejects: u64,
    /// When the submissions of the last second went to the venue.
    submissions: VecDeque<Instant>,
    /// The account as last read from the venue, valid until something
    /// that can change it happens (a venue call, a lifecycle event, the
    /// next activation). One engine lock per activation, not one per
    /// read.
    view_cache: Option<AccountView>,
    orders: BTreeMap<u64, LogicalOrder>,
    by_client: HashMap<ClientOrderId, u64>,
    next_ordinal: u64,
    lifecycle_queue: VecDeque<ExecutionEvent>,
    pub paused: bool,
    pub stopping: bool,
    pub failure_reason: Option<String>,
    pub market_generation: u64,
    /// After a settlement, stay masked until the next window opens.
    awaiting_window: bool,
    /// Sessions requested from the platform; frames older than this are dropped.
    pub session: u64,
    last_fill_price: Decimal,
    log: VecDeque<String>,
    platform: PlatformHandle,
    venue: Arc<dyn Venue>,
    clock: Arc<dyn VenueClock>,
}

/// The params a deployment runs with: every declared default, overridden
/// by the deploy-time values. The same map goes to the matching engine
/// (a trigger row that reads a param evaluates against it) and to the
/// interpreter, so a declared param with no override is never unknown.
pub fn effective_params(program: &Program, overrides: &BTreeMap<String, Value>) -> Result<BTreeMap<String, Value>, String> {
    let mut params = BTreeMap::new();
    for p in &program.params {
        let v = crate::automation::eval::eval(&p.default, &NoReads).map_err(|e| format!("param default {}: {e:?}", p.name))?;
        params.insert(p.name.clone(), v);
    }
    for (k, v) in overrides {
        params.insert(k.clone(), v.clone());
    }
    Ok(params)
}

impl RunningDeployment {
    pub fn new(spec: DeploymentSpec, conn: ConnId, platform: PlatformHandle, venue: Arc<dyn Venue>, clock: Arc<dyn VenueClock>) -> Result<Self, String> {
        let state = StateStore::new(&spec.program.state).map_err(|e| format!("state seed: {e:?}"))?;
        let params = effective_params(&spec.program, &spec.params)?;
        Ok(RunningDeployment {
            id: spec.id(),
            current_leg: spec.program.initial_leg.clone(),
            spec,
            conn,
            state,
            params,
            fills: FillLedger::default(),
            activation_seq: 0,
            coalesced: 0,
            rate_rejects: 0,
            submissions: VecDeque::new(),
            view_cache: None,
            orders: BTreeMap::new(),
            by_client: HashMap::new(),
            next_ordinal: 1,
            lifecycle_queue: VecDeque::new(),
            paused: false,
            stopping: false,
            failure_reason: None,
            market_generation: 0,
            awaiting_window: false,
            session: 0,
            last_fill_price: Decimal::ZERO,
            log: VecDeque::new(),
            platform,
            venue,
            clock,
        })
    }

    fn program(&self) -> &Program {
        &self.spec.program
    }

    pub fn log_line(&mut self, line: impl Into<String>) {
        if self.log.len() >= LOG_LINES_KEPT {
            self.log.pop_front();
        }
        let line = line.into();
        tracing::debug!(deployment = %self.id, "{line}");
        self.log.push_back(line);
    }

    pub fn live_orders(&self) -> impl Iterator<Item = &LogicalOrder> {
        self.orders.values().filter(|o| o.live)
    }

    fn arm_for(&self, leg: &str) -> BTreeSet<String> {
        if self.paused || self.awaiting_window {
            return BTreeSet::new();
        }
        self.program().legs.iter().find(|l| l.name == leg).map(|l| l.arm.iter().cloned().collect()).unwrap_or_default()
    }

    fn push_mask(&self) {
        self.platform.set_mask(self.conn, self.arm_for(&self.current_leg));
    }

    pub fn status(&self) -> DeploymentStatus {
        DeploymentStatus {
            id: self.id.clone(),
            strategy_id: self.program().strategy_id.clone(),
            deploy_epoch: self.spec.deploy_epoch,
            series: self.spec.series,
            current_leg: self.current_leg.clone(),
            armed_triggers: self.arm_for(&self.current_leg).into_iter().collect(),
            paused: self.paused,
            stopping: self.stopping,
            failure_reason: self.failure_reason.clone(),
            activations: self.activation_seq,
            coalesced: self.coalesced,
            rate_rejects: self.rate_rejects,
            live_orders: self.live_orders().cloned().collect(),
            params: self.params.clone(),
            state: self.state.scalars().clone(),
            log: self.log.iter().cloned().collect(),
        }
    }

    // ───────────────────────────── activations ─────────────────────────────

    /// One activation frame: interpret, execute, drain.
    pub fn on_activation(&mut self, activation: &Activation) {
        if self.paused || self.stopping {
            return;
        }
        self.activation_seq += 1;
        self.view_cache = None;
        let platform = self.platform_view();
        let leg = self.current_leg.clone();
        let result = interpret(&self.spec.program, &leg, activation, &mut self.state, &self.params, &platform, &self.fills);
        match result {
            Ok(actions) => {
                self.log_line(format!("[activation] #{} leg {leg} fired {:?} -> {} actions", self.activation_seq, activation.fired, actions.len()));
                self.execute_batch(actions);
            }
            Err(e) => self.fatal(format!("interpreter error: {e}")),
        }
        self.drain_lifecycle();
    }

    /// The account view, read from the venue only when the cache is stale.
    fn account_view(&mut self) -> AccountView {
        if let Some(view) = &self.view_cache {
            return view.clone();
        }
        let view = self.venue.account_view(self.spec.series, self.spec.sub_account);
        self.view_cache = Some(view.clone());
        view
    }

    /// One consistent view of the account and window for this activation.
    pub fn platform_view(&mut self) -> PlatformView {
        let view = self.account_view();
        let instrument = self.program().instruments.values().next().cloned().unwrap_or_default();
        PlatformView {
            position_size: view.position_size,
            position_avg_cost: view.position_avg_cost,
            realized_pnl: view.realized_pnl,
            unrealized_pnl: view.unrealized_pnl,
            cash: view.cash,
            open_orders: self.live_orders().filter(|o| o.venue_order_id.is_some() && o.tif == Tif::Gtc).count() as u64,
            window_remaining_ms: self.clock.window_remaining_ms(&instrument).unwrap_or(0),
            last_fill_price: self.last_fill_price,
        }
    }

    // ─────────────────────────── action batch ───────────────────────────

    /// Execute a batch in list order. Any invariant failure is fatal for this
    /// deployment only.
    pub fn execute_batch(&mut self, actions: Vec<Action>) {
        for action in actions {
            if self.stopping {
                return;
            }
            let result = match action {
                Action::Place { intent_id, order } => self.place(&intent_id, order, 0, None),
                Action::Cancel { tag } => self.cancel_tag(&tag),
                Action::CancelReplace { intent_id, order } => self.cancel_replace(&intent_id, order),
                Action::TargetPosition { intent_id, order } => self.target_position(&intent_id, order),
                Action::SwitchLeg(name) => self.switch_leg(&name),
            };
            if let Err(e) = result {
                self.fatal(format!("action batch failure: {e}"));
                return;
            }
        }
    }

    fn client_order_id(&self, intent_id: &str, activation_seq: u64, attempt: u32) -> ClientOrderId {
        Arc::from(format!("{}/{}/{intent_id}/{activation_seq}/{attempt}", self.program().strategy_id, self.spec.deploy_epoch))
    }

    fn new_logical(&mut self, intent_id: &str, order: &ResolvedOrder, on: OnTable, activation_seq: u64, attempt: u32) -> Result<u64, String> {
        let client_order_id = self.client_order_id(intent_id, activation_seq, attempt);
        if self.by_client.contains_key(&client_order_id) {
            return Err(format!("duplicate client order id {client_order_id}"));
        }
        let ordinal = self.next_ordinal;
        self.next_ordinal += 1;
        let logical = LogicalOrder {
            client_order_id: client_order_id.clone(),
            intent_id: intent_id.to_string(),
            tag: order.def.tag.clone(),
            leg: self.current_leg.clone(),
            outcome: order.def.outcome.clone(),
            side: order.def.side,
            order_type: order.def.order_type,
            tif: order.def.tif,
            price: order.price,
            // The venue trades whole contracts (`translate::floor_size`), so
            // the logical order carries the size it will actually submit;
            // otherwise a full fill leaves a phantom fractional remainder.
            requested: order.size.floor(),
            remaining: order.size.floor(),
            filled: Decimal::ZERO,
            activation_seq,
            attempt,
            ordinal,
            venue_order_id: None,
            market_id: None,
            live: true,
            on,
        };
        self.orders.insert(ordinal, logical);
        self.by_client.insert(client_order_id, ordinal);
        Ok(ordinal)
    }

    fn venue_order(&self, logical: &LogicalOrder, def: &strategy_compiler::program::OrderDef) -> VenueOrder {
        VenueOrder {
            client_order_id: logical.client_order_id.clone(),
            instrument_id: def.instrument_id.clone(),
            outcome: def.outcome.clone(),
            side: def.side,
            order_type: def.order_type,
            tif: def.tif,
            size: logical.remaining,
            price: logical.price,
            post_only: def.post_only,
            expire_after_ms: def.expire_after_ms,
            reduce_only: def.reduce_only,
        }
    }

    fn queue_local_reject(&mut self, ordinal: u64, reason: RejectReason, detail: String) {
        let Some(order) = self.orders.get(&ordinal) else { return };
        let ev = ExecutionEvent {
            series: self.spec.series,
            account: self.spec.sub_account,
            client_order_id: order.client_order_id.clone(),
            venue_order_id: None,
            kind: ExecutionKind::Reject { reason, detail: detail.clone() },
            filled_qty: Decimal::ZERO,
            remaining_qty: Decimal::ZERO,
            price: None,
            market_id: None,
        };
        let intent = order.intent_id.clone();
        self.log_line(format!("[reject] {intent}: {}: {detail}", reason.as_str()));
        self.lifecycle_queue.push_back(ev);
    }

    /// Summarize live resting orders for the risk gate against `position`
    /// (signed, quoted frame).
    fn open_orders_for_risk(&self, position: Decimal) -> OpenOrders {
        let resting: Vec<VenueOrder> = self
            .live_orders()
            .filter(|o| o.venue_order_id.is_some() && o.tif == Tif::Gtc)
            .map(|o| VenueOrder {
                client_order_id: o.client_order_id.clone(),
                instrument_id: String::new(),
                outcome: o.outcome.clone(),
                side: o.side,
                order_type: o.order_type,
                tif: o.tif,
                size: o.remaining,
                price: o.price,
                post_only: false,
                expire_after_ms: None,
                reduce_only: false,
            })
            .collect();
        OpenOrders::summarize(position, resting.iter())
    }

    /// The order-rate gate: admit a submission to the venue if fewer than
    /// [`MAX_ORDERS_PER_SEC`] went in the last second, recording it.
    /// Otherwise the caller queues a local reject with the returned detail.
    fn admit_submission(&mut self) -> Result<(), String> {
        let now = Instant::now();
        while self.submissions.front().is_some_and(|t| now.duration_since(*t).as_secs() >= 1) {
            self.submissions.pop_front();
        }
        if self.submissions.len() >= MAX_ORDERS_PER_SEC {
            self.rate_rejects += 1;
            return Err(format!("order rate exceeds {MAX_ORDERS_PER_SEC}/s"));
        }
        self.submissions.push_back(now);
        Ok(())
    }

    /// Place one order: local checks, risk gate, rate gate, venue submit.
    fn place(&mut self, intent_id: &str, order: ResolvedOrder, attempt: u32, activation_seq: Option<u64>) -> Result<(), String> {
        let intent = self.program().intents.get(intent_id).ok_or_else(|| format!("unknown intent {intent_id}"))?.clone();
        let max_size = parse_decimal(&intent.max_size).unwrap_or(Decimal::ZERO);
        let seq = activation_seq.unwrap_or(self.activation_seq);
        let ordinal = self.new_logical(intent_id, &order, intent.on.clone(), seq, attempt)?;
        if order.size <= Decimal::ZERO {
            self.queue_local_reject(ordinal, RejectReason::InvalidOrder, format!("qty {} must be positive", order.size));
            return Ok(());
        }
        if order.size > max_size {
            self.queue_local_reject(ordinal, RejectReason::EnvelopeViolation, format!("qty {} exceeds intent max_size {max_size}", order.size));
            return Ok(());
        }
        if let Some(p) = order.price.filter(|p| *p <= Decimal::ZERO || *p >= Decimal::ONE) {
            self.queue_local_reject(ordinal, RejectReason::PriceBand, format!("price {p} outside (0, 1)"));
            return Ok(());
        }
        let logical = self.orders.get(&ordinal).expect("just inserted").clone();
        let venue_order = self.venue_order(&logical, &order.def);
        let view = self.account_view();
        if let Err((reason, detail)) = risk::check(&self.program().envelope, &view, &self.open_orders_for_risk(view.position_size), &venue_order) {
            self.queue_local_reject(ordinal, reason, detail);
            return Ok(());
        }
        if let Err(detail) = self.admit_submission() {
            self.queue_local_reject(ordinal, RejectReason::EnvelopeViolation, detail);
            return Ok(());
        }
        self.view_cache = None;
        match self.venue.submit(self.spec.series, self.spec.sub_account, &venue_order) {
            Ok(accepted) => {
                if let Some(o) = self.orders.get_mut(&ordinal) {
                    o.venue_order_id = Some(accepted.venue_order_id);
                    o.market_id = accepted.market_id;
                }
                self.log_line(format!("[order] {intent_id} {} -> venue #{}", logical.client_order_id, accepted.venue_order_id));
            }
            Err(reason) => self.queue_local_reject(ordinal, reason, "venue refused".into()),
        }
        Ok(())
    }

    fn live_tagged(&self, tag: &str) -> Vec<(u64, u64)> {
        self.live_orders()
            .filter(|o| o.tag == tag)
            .filter_map(|o| o.venue_order_id.map(|v| (o.ordinal, v)))
            .collect()
    }

    fn cancel_tag(&mut self, tag: &str) -> Result<(), String> {
        self.view_cache = None;
        for (_, venue_id) in self.live_tagged(tag) {
            if let Err(e) = self.venue.cancel(self.spec.series, self.spec.sub_account, venue_id) {
                self.log_line(format!("[cancel] #{venue_id} failed: {e}"));
            }
        }
        Ok(())
    }

    fn cancel_replace(&mut self, intent_id: &str, order: ResolvedOrder) -> Result<(), String> {
        let targets = self.live_tagged(&order.def.tag);
        if targets.is_empty() {
            return Ok(());
        }
        if targets.len() != 1 {
            return Err(format!("cancel_replace tag `{}` matched {} orders", order.def.tag, targets.len()));
        }
        let (old_ordinal, old_venue_id) = targets[0];
        let intent = self.program().intents.get(intent_id).ok_or_else(|| format!("unknown intent {intent_id}"))?.clone();
        let max_size = parse_decimal(&intent.max_size).unwrap_or(Decimal::ZERO);
        let seq = self.activation_seq;
        let ordinal = self.new_logical(intent_id, &order, intent.on.clone(), seq, 0)?;
        if order.size <= Decimal::ZERO || order.size > max_size {
            self.queue_local_reject(ordinal, RejectReason::EnvelopeViolation, format!("qty {} outside (0, {max_size}]", order.size));
            return Ok(());
        }
        let logical = self.orders.get(&ordinal).expect("just inserted").clone();
        let venue_order = self.venue_order(&logical, &order.def);
        if let Err(detail) = self.admit_submission() {
            self.queue_local_reject(ordinal, RejectReason::EnvelopeViolation, detail);
            return Ok(());
        }
        self.view_cache = None;
        match self.venue.cancel_replace(self.spec.series, self.spec.sub_account, old_venue_id, &venue_order) {
            Ok(accepted) => {
                // The replacement is atomic at the venue: the old order is
                // gone the moment this returns, before its `Replaced` event
                // has travelled through the manager. Retire it now so the
                // next activation (a burst of requotes) finds exactly one
                // live order under the tag.
                if let Some(old) = self.orders.get_mut(&old_ordinal) {
                    old.live = false;
                }
                if let Some(o) = self.orders.get_mut(&ordinal) {
                    o.venue_order_id = Some(accepted.venue_order_id);
                    o.market_id = accepted.market_id;
                }
                self.log_line(format!("[replace] {intent_id} #{old_venue_id} -> #{}", accepted.venue_order_id));
            }
            Err(reason) => self.queue_local_reject(ordinal, reason, "venue refused replace".into()),
        }
        Ok(())
    }

    /// Reach a target signed position with one market IOC delta order.
    fn target_position(&mut self, intent_id: &str, order: ResolvedOrder) -> Result<(), String> {
        let view = self.account_view();
        // Target is in the strategy's outcome frame; position is quoted-frame.
        let quoted = is_quoted_outcome(order.def.outcome.as_deref());
        let current = if quoted { view.position_size } else { -view.position_size };
        let delta = order.size - current;
        if delta.is_zero() {
            return Ok(());
        }
        let mut def = order.def.clone();
        def.side = if delta > Decimal::ZERO { Side::Buy } else { Side::Sell };
        def.order_type = OrderType::Market;
        def.tif = Tif::Ioc;
        def.price = None;
        let resolved = ResolvedOrder { def, size: delta.abs(), price: None };
        self.place(intent_id, resolved, 0, None)
    }

    /// Move the leg pointer and push the new mask. `stay` is a no-op.
    ///
    /// Live orders survive a leg change: IR v0.3 has no exit disposition, so
    /// strategies pull orders explicitly with `cancel` items (as
    /// `fair-discount`'s `pull` block does) and `stop` cancels everything.
    fn switch_leg(&mut self, name: &str) -> Result<(), String> {
        if name == "stay" || name == self.current_leg {
            return Ok(());
        }
        if !self.program().legs.iter().any(|l| l.name == name) {
            return Err(format!("unknown leg `{name}`"));
        }
        let old = std::mem::replace(&mut self.current_leg, name.to_string());
        self.push_mask();
        self.log_line(format!("[leg] {old} -> {name}"));
        Ok(())
    }

    // ────────────────────────────── lifecycle ──────────────────────────────

    pub fn enqueue_execution(&mut self, event: ExecutionEvent) {
        if self.stopping {
            return;
        }
        self.lifecycle_queue.push_back(event);
    }

    /// Apply queued execution events FIFO. No strategy code runs here.
    pub fn drain_lifecycle(&mut self) {
        let mut count = 0usize;
        while let Some(event) = self.lifecycle_queue.pop_front() {
            if self.stopping {
                return;
            }
            count += 1;
            if count > MAX_LIFECYCLE_EVENTS_PER_DRAIN {
                self.fatal("lifecycle queue did not quiesce".into());
                return;
            }
            if let Err(e) = self.apply_lifecycle(event) {
                self.fatal(format!("lifecycle failure: {e}"));
                return;
            }
        }
    }

    fn apply_lifecycle(&mut self, event: ExecutionEvent) -> Result<(), String> {
        // An order fate can move cash, position and PnL.
        self.view_cache = None;
        let Some(&ordinal) = self.by_client.get(&event.client_order_id) else {
            // Not ours (e.g. a stale event after a redeploy): ignore.
            return Ok(());
        };
        let order = self.orders.get_mut(&ordinal).ok_or("order index desync")?;
        // Prices arrive in the quoted frame; complement for the other outcome.
        let price = event.price.map(|p| if is_quoted_outcome(order.outcome.as_deref()) { p } else { Decimal::ONE - p });
        order.filled += event.filled_qty;
        order.remaining = event.remaining_qty;
        let kind_key = event.kind.table_key();
        let dead = matches!(event.kind, ExecutionKind::Fill | ExecutionKind::Reject { .. } | ExecutionKind::CancelAck | ExecutionKind::Expiry | ExecutionKind::Replaced)
            || (matches!(event.kind, ExecutionKind::Partial) && event.remaining_qty.is_zero());
        if dead {
            order.live = false;
        }
        let on = order.on.clone();
        let (leg, tag, intent) = (order.leg.clone(), order.tag.clone(), order.intent_id.clone());
        if let (Some(p), true) = (price, event.filled_qty > Decimal::ZERO) {
            self.fills.record(&leg, &tag, event.filled_qty, p);
            self.last_fill_price = p;
        }
        let target = match kind_key {
            "fill" => on.fill,
            "partial" => on.partial,
            "reject" => on.reject,
            "cancel_ack" => on.cancel_ack,
            "expiry" => on.expiry,
            // `replaced` is mechanical: the replacement carries the intent.
            _ => "stay".to_string(),
        };
        self.log_line(format!("[lifecycle] {intent} {kind_key} -> {target}"));
        self.switch_leg(&target)?;
        self.check_budget();
        Ok(())
    }

    fn check_budget(&mut self) {
        if self.stopping {
            return;
        }
        let env = &self.program().envelope;
        let loss_budget = parse_decimal(env.loss_budget.as_str()).unwrap_or(Decimal::ZERO);
        let pause_at = env.pause_at_consumed.as_ref().and_then(|d| parse_decimal(d.as_str()));
        let view = self.account_view();
        if view.realized_pnl_lifetime < -loss_budget {
            self.fatal("maximum realized loss exceeded".into());
            return;
        }
        if self.paused {
            return;
        }
        if let Some(threshold) = pause_at {
            let consumed = (-view.realized_pnl_lifetime).max(Decimal::ZERO) + risk::open_cost(&view) + self.open_orders_for_risk(view.position_size).resting_notional;
            if consumed >= loss_budget * threshold {
                self.pause(format!("loss budget consumption {consumed} reached {}", loss_budget * threshold));
            }
        }
    }

    // ─────────────────────────── pause / fatal / stop ───────────────────────────

    pub fn pause(&mut self, reason: String) {
        if self.paused {
            return;
        }
        self.paused = true;
        self.log_line(format!("[pause] {reason}"));
        self.push_mask();
    }

    pub fn resume(&mut self) {
        if !self.paused {
            return;
        }
        self.paused = false;
        self.log_line("[resume]");
        self.push_mask();
    }

    pub fn fatal(&mut self, reason: String) {
        if self.stopping {
            return;
        }
        self.failure_reason = Some(reason.clone());
        self.log_line(format!("[fatal] {reason}"));
        self.stop();
    }

    /// Cancel every live order, disconnect, mark stopping.
    pub fn stop(&mut self) {
        if self.stopping {
            return;
        }
        self.stopping = true;
        let live: Vec<u64> = self.live_orders().filter_map(|o| o.venue_order_id).collect();
        for venue_id in live {
            let _ = self.venue.cancel(self.spec.series, self.spec.sub_account, venue_id);
        }
        for o in self.orders.values_mut() {
            o.live = false;
        }
        self.platform.disconnect(self.conn);
        self.log_line("[stop]");
    }

    pub fn set_params(&mut self, params: BTreeMap<String, Value>) {
        for (k, v) in params {
            self.params.insert(k, v);
        }
        self.platform.set_params(self.conn, self.params.clone());
    }

    // ─────────────────────────── market session ───────────────────────────

    pub fn on_window_opened(&mut self, market_id: u32) {
        if self.awaiting_window {
            self.awaiting_window = false;
            self.log_line(format!("[window] {market_id} opened; re-arming {}", self.current_leg));
            self.push_mask();
        }
    }

    /// Settlement of the home series: drain first (lock-phase cancel acks),
    /// then rewind everything but the account.
    pub fn on_window_settled(&mut self, market_id: u32) {
        self.drain_lifecycle();
        if self.stopping {
            return;
        }
        self.check_budget();
        if self.stopping {
            return;
        }
        let old = self.current_leg.clone();
        self.market_generation += 1;
        self.awaiting_window = true;
        self.session += 1;
        self.platform.reset_session(self.conn);
        self.state.clear();
        self.fills.clear();
        self.last_fill_price = Decimal::ZERO;
        for o in self.orders.values_mut() {
            o.live = false;
        }
        self.current_leg = self.program().initial_leg.clone();
        if old != self.current_leg {
            self.log_line(format!("[leg] {old} -> {} (market reset)", self.current_leg));
        }
        self.log_line(format!("[reset] market {market_id} settled -> {}", self.current_leg));
    }
}

/// Literal param defaults read nothing.
struct NoReads;

impl crate::automation::eval::EvalCtx for NoReads {
    fn signal(&self, signal_id: &str, field: &str) -> Result<Value, crate::automation::eval::EvalError> {
        Err(crate::automation::eval::EvalError::MissingSignalField { signal_id: signal_id.into(), field: field.into() })
    }
    fn param(&self, name: &str) -> Result<Value, crate::automation::eval::EvalError> {
        Err(crate::automation::eval::EvalError::UnknownParam(name.into()))
    }
    fn division_by_zero(&self) -> Option<Decimal> {
        Some(Decimal::ZERO)
    }
}

/// Whether `next` can be absorbed into `head`: the same cause (the same
/// publishing signal, or both timers) and the same fired set. Two such
/// activations differ only in the snapshot they carry, and the later one
/// carries the latest.
fn same_cause(head: &Activation, next: &Activation) -> bool {
    head.session == next.session && head.event_signal == next.event_signal && head.fired == next.fired
}

/// Drain the activations already queued behind `head` and collapse the
/// run of consecutive same-cause ones into the latest. A maker requoting
/// on every tick of a fast signal wakes once with the newest snapshot
/// instead of once per tick it fell behind by. The first frame that
/// differs (another trigger, another signal, a newer session) ends the
/// run and is returned to be processed next, in order, so a crossing fire
/// is never absorbed into an unrelated one and `event.*` reads always see
/// the event their fired set belongs to. Returns the activation to run,
/// how many were absorbed, and the frame that ended the run.
pub(crate) fn coalesce(head: Activation, frames: &mut mpsc::UnboundedReceiver<Frame>) -> (Activation, u64, Option<Frame>) {
    let mut latest = head;
    let mut absorbed = 0;
    while let Ok(frame) = frames.try_recv() {
        match frame {
            Frame::Activation(next) if same_cause(&latest, &next) => {
                latest = next;
                absorbed += 1;
            }
            other => return (latest, absorbed, Some(other)),
        }
    }
    (latest, absorbed, None)
}

/// The deployment task: frames from the platform and commands from the
/// manager, until stopped. Returns the final status.
pub async fn run_deployment(
    mut dep: RunningDeployment,
    mut frames: mpsc::UnboundedReceiver<Frame>,
    mut commands: mpsc::UnboundedReceiver<DeploymentCommand>,
) -> DeploymentStatus {
    dep.log_line(format!("[start] leg {}", dep.current_leg));
    let mut stop_replies: Vec<oneshot::Sender<()>> = Vec::new();
    // A frame `coalesce` pulled off the channel but did not absorb; it is
    // processed before the channel is read again.
    let mut pending: Option<Frame> = None;
    while !dep.stopping {
        if let Some(frame) = pending.take() {
            handle_frame(&mut dep, frame, &mut frames, &mut pending);
            continue;
        }
        tokio::select! {
            biased;
            cmd = commands.recv() => {
                let Some(cmd) = cmd else { dep.stop(); break };
                match cmd {
                    DeploymentCommand::Pause => dep.pause("paused via API".into()),
                    DeploymentCommand::Resume => dep.resume(),
                    DeploymentCommand::Stop { reply } => {
                        if let Some(r) = reply { stop_replies.push(r); }
                        dep.stop();
                    }
                    DeploymentCommand::SetParams(p) => dep.set_params(p),
                    DeploymentCommand::Execution(ev) => {
                        dep.enqueue_execution(ev);
                        dep.drain_lifecycle();
                    }
                    DeploymentCommand::WindowOpened { market_id } => dep.on_window_opened(market_id),
                    DeploymentCommand::WindowSettled { market_id } => dep.on_window_settled(market_id),
                    DeploymentCommand::Status { reply } => { let _ = reply.send(dep.status()); }
                }
            }
            frame = frames.recv() => {
                let Some(frame) = frame else { dep.fatal("platform connection closed".into()); break };
                handle_frame(&mut dep, frame, &mut frames, &mut pending);
            }
        }
    }
    for r in stop_replies {
        let _ = r.send(());
    }
    dep.status()
}

/// One platform frame: drop it if it predates the current session,
/// otherwise coalesce the activations queued behind it and run the result.
fn handle_frame(dep: &mut RunningDeployment, frame: Frame, frames: &mut mpsc::UnboundedReceiver<Frame>, pending: &mut Option<Frame>) {
    if frame.session() < dep.session {
        return;
    }
    let Frame::Activation(act) = frame;
    let (act, absorbed, rest) = coalesce(act, frames);
    dep.coalesced += absorbed;
    *pending = rest;
    dep.on_activation(&act);
}
