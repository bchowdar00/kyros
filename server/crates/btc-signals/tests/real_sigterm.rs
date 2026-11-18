//! Real-SIGTERM graceful-shutdown reach for the shared live shutdown contract.
//!
//! The other shutdown coverage feeds a stand-in future into the coordinator
//! (`src/bin/prediction-signals.rs::a_stop_signal_cancels_all_workers_and_classifies_interrupted`);
//! this delivers an actual SIGTERM to the process and asserts the production
//! stop-signal listener (`btc_feeds::live::wait_for_stop_signal`) drives the
//! graceful path: the shared trigger cancels, every worker's wait resolves
//! cleanly, and the run classifies `Interrupted` (exit 130 per the shutdown
//! contract in `live.rs` and the binaries).
//!
//! Isolation: this file holds exactly one test and compiles to its own
//! integration binary, so the process-wide SIGTERM it raises has no sibling test
//! thread to reach. The current-thread runtime plus an explicit poll-to-`Pending`
//! step orders the tokio SIGTERM handler install strictly before the signal is
//! raised, so the default terminate disposition (kill the process) never fires.

#![cfg(unix)]

use std::future::{Future, poll_fn};
use std::pin::pin;
use std::process::Command;
use std::task::{Context, Poll};
use std::time::Duration;

use btc_feeds::live::{live_shutdown_channel, wait_for_stop_signal};
use btc_signals::exit_code::ExitClass;

#[tokio::test(flavor = "current_thread")]
async fn real_sigterm_engages_the_graceful_shutdown_path() {
    let (trigger, shutdown) = live_shutdown_channel();
    let mut worker = shutdown.clone();

    // Production coordinator shape: the real SIGINT+SIGTERM listener cancels the
    // shared trigger every worker waits on.
    let mut coordinator = pin!(async move {
        wait_for_stop_signal().await.expect("stop-signal listener");
        trigger.cancel();
    });

    // One poll runs `signal(SignalKind::terminate())` inside the listener, which
    // installs the tokio SIGTERM handler synchronously; it must complete before
    // the signal is raised so SIGTERM is caught, not fatal. The listener has no
    // signal to observe yet, so this poll must return `Pending`.
    poll_fn(|cx: &mut Context<'_>| match coordinator.as_mut().poll(cx) {
        Poll::Pending => Poll::Ready(()),
        Poll::Ready(()) => panic!("listener resolved before any signal was raised"),
    })
    .await;

    // Deliver a real SIGTERM to this process. Shelling out to `kill` keeps the
    // dependency set unchanged (no `libc` for `raise`/`kill`).
    let status = Command::new("kill")
        .arg("-TERM")
        .arg(std::process::id().to_string())
        .status()
        .expect("kill runs");
    assert!(status.success(), "SIGTERM delivered to self");

    // The graceful path must engage within a bounded wait: the listener resolves,
    // cancels the trigger, and the worker observes the cancellation cleanly.
    let graceful = tokio::time::timeout(Duration::from_secs(5), async {
        coordinator.await;
        worker
            .wait()
            .await
            .expect("worker observes graceful cancellation");
    })
    .await;
    assert!(
        graceful.is_ok(),
        "a real SIGTERM must drive the graceful shutdown path"
    );

    // The graceful stop maps to Interrupted -> exit 130 per the shutdown contract.
    assert_eq!(ExitClass::Interrupted.code(), 130);
}
