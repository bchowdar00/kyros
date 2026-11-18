//! The emit stage: runtime capability gate, then the two projections — the program
//! (what the exchange runtime executes) and the wire manifest (provenance and the
//! venue-facing registration shape).
//!
//! Everything here consumes [`crate::validate::ValidStrategy`] or the lowering result —
//! never a raw document. The gate runs first: it refuses, with ordinary diagnostics,
//! the constructs the runtime cannot faithfully run, so nothing downstream ever
//! silently mislowers.

pub mod build;
pub mod gate;
pub mod manifest;
pub mod program;

pub use build::{Artifacts, BuildOutput, build};
pub use gate::gate;
pub use manifest::{SCHEMA_PIN, emit_manifest, project_manifest};
pub use program::{emit_program, project_program, serialize_program};
