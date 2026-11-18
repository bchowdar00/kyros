//! The platform task: one Tokio task owns the [`SignalsPlatform`] and drains
//! two channels. Publishers, the runtime and timers all talk to it through
//! a cloned [`PlatformHandle`].
//!
//! Signal-side commands (events, enables, snapshot clears) travel on their
//! own channel and are drained first (`biased` select), so a burst of
//! control traffic (connects, status queries, mask changes, timer ticks)
//! never sits in front of a signal a strategy is waiting on. Each channel
//! keeps its own order, which is what matters: a signal's enable always
//! precedes its first event, and a clear always lands between the events
//! around it.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};

use crate::automation::signals::SignalEvent;
use crate::automation::value::Value;

use super::{ConnId, ConnectError, ConnectSpec, SignalsPlatform, Stats, TimerRequest, TimerSink};

pub enum PlatformCommand {
    Ingest(SignalEvent),
    Enable(String),
    /// The signal's publisher reported it not ready: forget its snapshot.
    ClearSnapshot(String),
    Connect { spec: ConnectSpec, reply: oneshot::Sender<Result<ConnId, ConnectError>> },
    SetMask { conn: ConnId, names: BTreeSet<String> },
    ResetSession { conn: ConnId },
    SetParams { conn: ConnId, params: BTreeMap<String, Value> },
    Disconnect { conn: ConnId },
    WindowOpened { instrument_ids: Vec<String> },
    TimerFired { conn: ConnId, name: String, generation: u64 },
    Stats { reply: oneshot::Sender<Stats> },
}

impl PlatformCommand {
    /// Whether the command rides the signal-side channel (drained first).
    fn is_signal_side(&self) -> bool {
        matches!(self, PlatformCommand::Ingest(_) | PlatformCommand::Enable(_) | PlatformCommand::ClearSnapshot(_))
    }
}

/// A cloneable sender into the platform task.
#[derive(Clone)]
pub struct PlatformHandle {
    /// Signal-side commands: drained ahead of everything else.
    signals_tx: mpsc::UnboundedSender<PlatformCommand>,
    /// Everything else, in order.
    tx: mpsc::UnboundedSender<PlatformCommand>,
}

/// The two receivers behind a [`PlatformHandle`].
pub struct PlatformInbox {
    pub signals: mpsc::UnboundedReceiver<PlatformCommand>,
    pub control: mpsc::UnboundedReceiver<PlatformCommand>,
}

impl PlatformInbox {
    /// The next command from a test's point of view: signal-side first,
    /// then control, as the task itself drains them.
    pub fn try_recv(&mut self) -> Result<PlatformCommand, mpsc::error::TryRecvError> {
        self.signals.try_recv().or_else(|_| self.control.try_recv())
    }
}

impl PlatformHandle {
    /// A handle over bare channels with no task behind them, so a test can
    /// read what a client sends.
    pub fn channel() -> (PlatformHandle, PlatformInbox) {
        let (signals_tx, signals) = mpsc::unbounded_channel();
        let (tx, control) = mpsc::unbounded_channel();
        (PlatformHandle { signals_tx, tx }, PlatformInbox { signals, control })
    }

    pub fn send(&self, cmd: PlatformCommand) {
        // The platform outlives every client; a failed send means shutdown.
        let lane = if cmd.is_signal_side() { &self.signals_tx } else { &self.tx };
        let _ = lane.send(cmd);
    }

    pub fn ingest(&self, event: SignalEvent) {
        self.send(PlatformCommand::Ingest(event));
    }

    pub fn enable(&self, signal_id: &str) {
        self.send(PlatformCommand::Enable(signal_id.to_string()));
    }

    pub fn clear_snapshot(&self, signal_id: &str) {
        self.send(PlatformCommand::ClearSnapshot(signal_id.to_string()));
    }

    pub async fn connect(&self, spec: ConnectSpec) -> Result<ConnId, ConnectError> {
        let (reply, rx) = oneshot::channel();
        self.send(PlatformCommand::Connect { spec, reply });
        rx.await.unwrap_or_else(|_| Err(ConnectError::UnknownSignal("platform task is gone".into())))
    }

    pub fn set_mask(&self, conn: ConnId, names: BTreeSet<String>) {
        self.send(PlatformCommand::SetMask { conn, names });
    }

    pub fn reset_session(&self, conn: ConnId) {
        self.send(PlatformCommand::ResetSession { conn });
    }

    pub fn set_params(&self, conn: ConnId, params: BTreeMap<String, Value>) {
        self.send(PlatformCommand::SetParams { conn, params });
    }

    pub fn disconnect(&self, conn: ConnId) {
        self.send(PlatformCommand::Disconnect { conn });
    }

    pub fn window_opened(&self, instrument_ids: Vec<String>) {
        self.send(PlatformCommand::WindowOpened { instrument_ids });
    }

    pub async fn stats(&self) -> Option<Stats> {
        let (reply, rx) = oneshot::channel();
        self.send(PlatformCommand::Stats { reply });
        rx.await.ok()
    }
}

/// A [`TimerSink`] that sleeps on Tokio and reports back into the platform.
pub struct TokioTimerSink {
    tx: mpsc::UnboundedSender<PlatformCommand>,
}

impl TimerSink for TokioTimerSink {
    fn schedule(&mut self, request: TimerRequest) {
        let tx = self.tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(request.delay_ms)).await;
            loop {
                if request.cancelled.load(Ordering::Relaxed) {
                    return;
                }
                if tx
                    .send(PlatformCommand::TimerFired { conn: request.conn, name: request.name.clone(), generation: request.generation })
                    .is_err()
                {
                    return;
                }
                match request.repeat_ms {
                    Some(period) => tokio::time::sleep(Duration::from_millis(period)).await,
                    None => return,
                }
            }
        });
    }
}

/// Spawn the platform task around a store and clock. Returns the handle.
pub fn spawn_platform(
    store: crate::automation::signals::SignalStore,
    clock: Arc<dyn super::VenueClock>,
) -> PlatformHandle {
    let (handle, inbox) = PlatformHandle::channel();
    let platform = SignalsPlatform::new(store, clock, Box::new(TokioTimerSink { tx: handle.tx.clone() }));
    tokio::spawn(run_platform(platform, inbox));
    handle
}

pub async fn run_platform(mut platform: SignalsPlatform, inbox: PlatformInbox) {
    let PlatformInbox { mut signals, mut control } = inbox;
    loop {
        let cmd = tokio::select! {
            biased;
            cmd = signals.recv() => match cmd {
                Some(cmd) => cmd,
                // Every handle is gone; the control side is closed too.
                None => break,
            },
            cmd = control.recv() => match cmd {
                Some(cmd) => cmd,
                None => break,
            },
        };
        match cmd {
            PlatformCommand::Ingest(event) => {
                if let Err(e) = platform.ingest(&event) {
                    tracing::debug!(signal = %event.signal_id, error = %e, "signal event dropped");
                }
            }
            PlatformCommand::Enable(id) => {
                platform.enable(&id);
            }
            PlatformCommand::ClearSnapshot(id) => platform.clear_snapshot(&id),
            PlatformCommand::Connect { spec, reply } => {
                let _ = reply.send(platform.connect(spec));
            }
            PlatformCommand::SetMask { conn, names } => platform.set_mask(conn, names),
            PlatformCommand::ResetSession { conn } => platform.reset_session(conn),
            PlatformCommand::SetParams { conn, params } => platform.set_params(conn, params),
            PlatformCommand::Disconnect { conn } => platform.disconnect(conn),
            PlatformCommand::WindowOpened { instrument_ids } => {
                let ids: Vec<&str> = instrument_ids.iter().map(String::as_str).collect();
                platform.window_opened(&ids);
            }
            PlatformCommand::TimerFired { conn, name, generation } => platform.timer_fired(conn, &name, generation),
            PlatformCommand::Stats { reply } => {
                let _ = reply.send(platform.stats());
            }
        }
    }
    tracing::info!("signals platform stopped");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::automation::signals::SignalValue;
    use crate::automation::signals::tests::ab_store;
    use crate::automation::sme::NoWindows;

    /// Signal-side commands are drained ahead of control commands sent
    /// earlier, and each side keeps its own order.
    #[test]
    fn signal_side_commands_are_drained_first_in_order() {
        let (handle, mut inbox) = PlatformHandle::channel();
        handle.disconnect(7);
        handle.enable("a");
        handle.ingest(SignalEvent::new("a", 1, [("x", SignalValue::decimal("1"))]));
        handle.clear_snapshot("a");
        handle.set_mask(7, BTreeSet::new());
        let kinds: Vec<&str> = std::iter::from_fn(|| inbox.try_recv().ok())
            .map(|cmd| match cmd {
                PlatformCommand::Enable(_) => "enable",
                PlatformCommand::Ingest(_) => "ingest",
                PlatformCommand::ClearSnapshot(_) => "clear",
                PlatformCommand::Disconnect { .. } => "disconnect",
                PlatformCommand::SetMask { .. } => "set_mask",
                _ => "other",
            })
            .collect();
        assert_eq!(kinds, ["enable", "ingest", "clear", "disconnect", "set_mask"]);
    }

    /// The running task drains both channels (control traffic queued ahead
    /// of an event never wedges it) and stops once every handle is gone.
    #[tokio::test]
    async fn task_drains_both_channels_and_stops_without_handles() {
        let (handle, inbox) = PlatformHandle::channel();
        let platform = SignalsPlatform::new(ab_store(), Arc::new(NoWindows), Box::new(crate::automation::sme::RecordingSink::default()));
        // Queue control traffic first, then the event, before the task runs.
        for _ in 0..100 {
            handle.set_mask(99, BTreeSet::new());
        }
        handle.ingest(SignalEvent::new("a", 1, [("x", SignalValue::decimal("1")), ("y", SignalValue::decimal("2"))]));
        let (reply, rx) = oneshot::channel();
        handle.send(PlatformCommand::Stats { reply });
        let task = tokio::spawn(run_platform(platform, inbox));
        assert!(rx.await.is_ok(), "the task drained everything");
        drop(handle);
        task.await.unwrap();
    }
}
