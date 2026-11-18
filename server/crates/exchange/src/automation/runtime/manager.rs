//! The runtime manager: owns the deployment map, starts and stops deployment
//! tasks, and routes execution events (by account) and window notifications
//! (by series) to them.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use strategy_compiler::program::Program;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::automation::sme::task::PlatformHandle;
use crate::automation::sme::{ConnectError, ConnectSpec, VenueClock};
use crate::automation::venue::{ExecutionEvent, Venue};
use crate::types::{Address, SeriesId};

use super::deployment::{DeploymentCommand, DeploymentSpec, DeploymentStatus, RunningDeployment, effective_params, run_deployment};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartError {
    /// `(strategy_id, epoch)` is already resident.
    Duplicate(String),
    Connect(String),
    Seed(String),
}

impl std::fmt::Display for StartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StartError::Duplicate(id) => write!(f, "deployment {id} is already resident"),
            StartError::Connect(e) => write!(f, "platform connect failed: {e}"),
            StartError::Seed(e) => write!(f, "deployment seed failed: {e}"),
        }
    }
}

impl From<ConnectError> for StartError {
    fn from(e: ConnectError) -> Self {
        StartError::Connect(e.to_string())
    }
}

pub enum RuntimeCommand {
    /// An order-fate event from the venue. Travels on the same channel as
    /// `WindowSettled` so the lock-phase cancel acks a settlement emits
    /// are always delivered before the settlement itself.
    Execution(ExecutionEvent),
    Start { spec: DeploymentSpec, reply: oneshot::Sender<Result<String, StartError>> },
    Stop { id: String, reply: Option<oneshot::Sender<Option<DeploymentStatus>>> },
    Pause { id: String },
    Resume { id: String },
    SetParams { id: String, params: std::collections::BTreeMap<String, crate::automation::value::Value> },
    Status { id: String, reply: oneshot::Sender<Option<DeploymentStatus>> },
    /// A window opened in `series`. `instrument_ids` are what timers name
    /// (the concrete market id and `<series>.current`).
    WindowOpened { series: SeriesId, market_id: u32, instrument_ids: Vec<String> },
    WindowSettled { series: SeriesId, market_id: u32 },
    Ids { reply: oneshot::Sender<Vec<String>> },
}

struct Handle {
    account: Address,
    series: SeriesId,
    tx: mpsc::UnboundedSender<DeploymentCommand>,
    join: JoinHandle<DeploymentStatus>,
}

/// A cloneable sender into the manager task.
#[derive(Clone)]
pub struct RuntimeHandle {
    tx: mpsc::UnboundedSender<RuntimeCommand>,
}

impl RuntimeHandle {
    pub fn send(&self, cmd: RuntimeCommand) {
        let _ = self.tx.send(cmd);
    }

    /// Deliver a venue execution event.
    pub fn execution(&self, event: ExecutionEvent) {
        self.send(RuntimeCommand::Execution(event));
    }

    pub async fn start(&self, spec: DeploymentSpec) -> Result<String, StartError> {
        let (reply, rx) = oneshot::channel();
        self.send(RuntimeCommand::Start { spec, reply });
        rx.await.unwrap_or_else(|_| Err(StartError::Connect("runtime manager is gone".into())))
    }

    pub async fn stop(&self, id: &str) -> Option<DeploymentStatus> {
        let (reply, rx) = oneshot::channel();
        self.send(RuntimeCommand::Stop { id: id.to_string(), reply: Some(reply) });
        rx.await.ok().flatten()
    }

    pub async fn status(&self, id: &str) -> Option<DeploymentStatus> {
        let (reply, rx) = oneshot::channel();
        self.send(RuntimeCommand::Status { id: id.to_string(), reply });
        rx.await.ok().flatten()
    }

    pub async fn ids(&self) -> Vec<String> {
        let (reply, rx) = oneshot::channel();
        self.send(RuntimeCommand::Ids { reply });
        rx.await.unwrap_or_default()
    }

    pub fn pause(&self, id: &str) {
        self.send(RuntimeCommand::Pause { id: id.to_string() });
    }

    pub fn resume(&self, id: &str) {
        self.send(RuntimeCommand::Resume { id: id.to_string() });
    }

    pub fn set_params(&self, id: &str, params: std::collections::BTreeMap<String, crate::automation::value::Value>) {
        self.send(RuntimeCommand::SetParams { id: id.to_string(), params });
    }

    pub fn window_opened(&self, series: SeriesId, market_id: u32, instrument_ids: Vec<String>) {
        self.send(RuntimeCommand::WindowOpened { series, market_id, instrument_ids });
    }

    pub fn window_settled(&self, series: SeriesId, market_id: u32) {
        self.send(RuntimeCommand::WindowSettled { series, market_id });
    }
}

/// The manager's inbox. Created before the manager so the venue can be
/// built with the handle (it reports execution events into it) and then
/// handed to [`spawn_manager`] together with the receiver.
pub fn runtime_channel() -> (RuntimeHandle, mpsc::UnboundedReceiver<RuntimeCommand>) {
    let (tx, rx) = mpsc::unbounded_channel();
    (RuntimeHandle { tx }, rx)
}

/// Spawn the manager on the receiver from [`runtime_channel`].
pub fn spawn_manager(
    rx: mpsc::UnboundedReceiver<RuntimeCommand>,
    platform: PlatformHandle,
    venue: Arc<dyn Venue>,
    clock: Arc<dyn VenueClock>,
) {
    tokio::spawn(run_manager(platform, venue, clock, rx));
}

/// The connection spec a program needs: every subscription, every trigger,
/// the initial leg's mask, and the deployment's effective params (declared
/// defaults under the deploy-time overrides) so a trigger row that reads a
/// param can evaluate from the first publish.
pub fn connect_spec(
    program: &Program,
    spec: &DeploymentSpec,
    tx: mpsc::UnboundedSender<crate::automation::sme::Frame>,
) -> Result<ConnectSpec, StartError> {
    let initial_mask = program.legs.iter().find(|l| l.name == program.initial_leg).map(|l| l.arm.clone()).unwrap_or_default();
    let params = effective_params(program, &spec.params).map_err(StartError::Seed)?;
    Ok(ConnectSpec {
        owner: Some(spec.id()),
        subscriptions: program.subscriptions.clone(),
        triggers: program.triggers.clone(),
        initial_mask,
        params,
        tx,
    })
}

pub async fn run_manager(
    platform: PlatformHandle,
    venue: Arc<dyn Venue>,
    clock: Arc<dyn VenueClock>,
    mut rx: mpsc::UnboundedReceiver<RuntimeCommand>,
) {
    let mut deployments: HashMap<String, Handle> = HashMap::new();
    let mut by_account: HashMap<Address, String> = HashMap::new();
    tracing::info!("strategy runtime manager started");
    loop {
        tokio::select! {
            cmd = rx.recv() => {
                let Some(cmd) = cmd else { break };
                match cmd {
                    RuntimeCommand::Execution(ev) => {
                        if let Some(h) = by_account.get(&ev.account).and_then(|id| deployments.get(id)) {
                            let _ = h.tx.send(DeploymentCommand::Execution(ev));
                        }
                        // Hot path: no reaping on every fill.
                        continue;
                    }
                    RuntimeCommand::Start { spec, reply } => {
                        let result = start(&platform, &venue, &clock, &mut deployments, &mut by_account, spec).await;
                        let _ = reply.send(result);
                    }
                    RuntimeCommand::Stop { id, reply } => {
                        let status = match deployments.remove(&id) {
                            Some(h) => {
                                by_account.remove(&h.account);
                                let (done_tx, done_rx) = oneshot::channel();
                                let _ = h.tx.send(DeploymentCommand::Stop { reply: Some(done_tx) });
                                let _ = done_rx.await;
                                h.join.await.ok()
                            }
                            None => None,
                        };
                        if let Some(r) = reply { let _ = r.send(status); }
                    }
                    RuntimeCommand::Pause { id } => {
                        if let Some(h) = deployments.get(&id) { let _ = h.tx.send(DeploymentCommand::Pause); }
                    }
                    RuntimeCommand::Resume { id } => {
                        if let Some(h) = deployments.get(&id) { let _ = h.tx.send(DeploymentCommand::Resume); }
                    }
                    RuntimeCommand::SetParams { id, params } => {
                        if let Some(h) = deployments.get(&id) { let _ = h.tx.send(DeploymentCommand::SetParams(params)); }
                    }
                    RuntimeCommand::Status { id, reply } => {
                        let status = match deployments.get(&id) {
                            Some(h) => {
                                let (tx, rx) = oneshot::channel();
                                let _ = h.tx.send(DeploymentCommand::Status { reply: tx });
                                rx.await.ok()
                            }
                            None => None,
                        };
                        let _ = reply.send(status);
                    }
                    RuntimeCommand::WindowOpened { series, market_id, instrument_ids } => {
                        platform.window_opened(instrument_ids);
                        for h in deployments.values().filter(|h| h.series == series) {
                            let _ = h.tx.send(DeploymentCommand::WindowOpened { market_id });
                        }
                    }
                    RuntimeCommand::WindowSettled { series, market_id } => {
                        for h in deployments.values().filter(|h| h.series == series) {
                            let _ = h.tx.send(DeploymentCommand::WindowSettled { market_id });
                        }
                    }
                    RuntimeCommand::Ids { reply } => {
                        let ids: BTreeSet<String> = deployments.keys().cloned().collect();
                        let _ = reply.send(ids.into_iter().collect());
                    }
                }
                // Reap deployments whose task ended on its own (fatal).
                let finished: Vec<String> = deployments.iter().filter(|(_, h)| h.join.is_finished()).map(|(id, _)| id.clone()).collect();
                for id in finished {
                    if let Some(h) = deployments.remove(&id) {
                        by_account.remove(&h.account);
                        tracing::info!(deployment = %id, "deployment task finished");
                    }
                }
            }
        }
    }
    for (_, h) in deployments {
        let _ = h.tx.send(DeploymentCommand::Stop { reply: None });
        let _ = h.join.await;
    }
    tracing::info!("strategy runtime manager stopped");
}

async fn start(
    platform: &PlatformHandle,
    venue: &Arc<dyn Venue>,
    clock: &Arc<dyn VenueClock>,
    deployments: &mut HashMap<String, Handle>,
    by_account: &mut HashMap<Address, String>,
    spec: DeploymentSpec,
) -> Result<String, StartError> {
    let id = spec.id();
    if deployments.contains_key(&id) {
        return Err(StartError::Duplicate(id));
    }
    let (frame_tx, frame_rx) = mpsc::unbounded_channel();
    let conn = platform.connect(connect_spec(&spec.program, &spec, frame_tx)?).await?;
    let dep = match RunningDeployment::new(spec.clone(), conn, platform.clone(), venue.clone(), clock.clone()) {
        Ok(d) => d,
        Err(e) => {
            platform.disconnect(conn);
            return Err(StartError::Seed(e));
        }
    };
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let join = tokio::spawn(run_deployment(dep, frame_rx, cmd_rx));
    by_account.insert(spec.sub_account, id.clone());
    deployments.insert(id.clone(), Handle { account: spec.sub_account, series: spec.series, tx: cmd_tx, join });
    tracing::info!(deployment = %id, series = %spec.series, "deployment started");
    Ok(id)
}
