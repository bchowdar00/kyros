//! Strategy automation: the event-driven runtime.
//!
//! Four stages, channels between them, one-directional:
//!
//! ```text
//! publishers ──▶ signals (catalogue, typed ingest, snapshots)
//!            ──▶ sme     (dedup condition rows, fire/rearm, masks, timers, activations)
//!            ──▶ runtime (interpreter over strategy_compiler::Program, action batches,
//!                          lifecycle drain, deployment tasks, manager)
//!            ──▶ venue   (Venue trait: orders in, ExecutionEvents out)
//! ```
//!
//! * [`value`] and [`eval`]: the runtime value type and the one `ExprNode`
//!   evaluator shared by trigger leaves and block bodies, with the pinned
//!   totality rules documented on `eval`.
//! * [`signals`]: the catalogue from the compiler's registry and the whole-event
//!   typed ingest.
//! * [`sme`]: the Signals Matching Engine and its task
//!   (`sme::task::spawn_platform`).
//! * [`runtime`]: state store, interpreter, deployment task, manager
//!   (`runtime::spawn_manager`).
//! * [`venue`]: the trait the runtime places orders through, `FakeVenue`
//!   for tests, and `EngineVenue` over the real exchange (with the series
//!   map, the translation table and the venue clock).
//! * [`signal_bus`] and the publishers ([`spot_signal`], [`series_signals`],
//!   [`stats_signal`], [`lane_bridge`]): the one path every signal takes,
//!   platform first, then `/ws/signals`. The lane bridge drains the
//!   in-process message stream the lane supervisor (`crate::signals`)
//!   creates from the vendored lanes' stream hooks.
//!
//! `main.rs` wires all of it: the lanes and their message stream, the
//! platform, the bus, the bridge and the other publishers, the execution
//! sink and `EngineVenue`, the runtime manager, and the scheduler's window
//! notifications. The strategy HTTP routes (`crate::api::strategy`)
//! compile a submitted document, store the record on a funded sub-account
//! and start the deployment.

pub mod eval;
pub mod lane_bridge;
pub mod runtime;
pub mod series_signals;
pub mod signal_bus;
pub mod signals;
pub mod sme;
pub mod delta_signal;
pub mod spot_signal;
pub mod stats_signal;
pub mod value;
pub mod venue;
