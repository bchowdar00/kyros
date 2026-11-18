//! The one build path: validate -> gate -> lower -> emit both projections. The
//! end-to-end tests and the exchange's deploy path call this directly.

use crate::ir::{Envelope, Registry, Strategy};

use crate::lower;
use crate::program::Program;
use crate::validate::{self, Diagnostic};

use super::{gate, manifest, program};

/// The in-memory artifacts of a successful build.
#[derive(Debug, Clone, PartialEq)]
pub struct Artifacts {
    /// The program, typed — what the runtime executes.
    pub program: Program,
    /// The program, serialized in its emitted form (pretty JSON, trailing newline).
    pub program_json: String,
    /// The wire-dialect manifest.
    pub manifest_json: String,
}

/// A complete build outcome: every diagnostic found (validator and gate, warnings
/// included), plus the artifacts iff nothing blocked.
#[derive(Debug, Clone, PartialEq)]
pub struct BuildOutput {
    /// All diagnostics, in document order (validator first, then gate refusals).
    pub diagnostics: Vec<Diagnostic>,
    /// `Some` iff zero error diagnostics — the artifacts, built entirely in memory.
    pub artifacts: Option<Artifacts>,
}

impl BuildOutput {
    /// Whether any diagnostic blocks the build.
    pub fn has_errors(&self) -> bool {
        self.diagnostics.iter().any(Diagnostic::is_error)
    }
}

/// Compile one document against a registry and envelope.
///
/// Any error — validator or capability gate — yields `artifacts: None`; nothing is
/// partially emitted. Warnings ride along either way.
pub fn build(
    strategy: &Strategy,
    registry: &Registry,
    envelope: &Envelope,
    compiler_version: &str,
) -> BuildOutput {
    let (mut diagnostics, valid) = validate::validate(strategy, registry, envelope);
    let Some(valid) = valid else {
        return BuildOutput {
            diagnostics,
            artifacts: None,
        };
    };

    let refusals = gate::gate(&valid);
    if !refusals.is_empty() {
        diagnostics.extend(refusals);
        return BuildOutput {
            diagnostics,
            artifacts: None,
        };
    }

    let lowered = lower::lower(&valid);
    let program = program::project_program(&lowered, compiler_version);
    let program_json = program::serialize_program(&program);
    let manifest_json = manifest::emit_manifest(&lowered);
    BuildOutput {
        diagnostics,
        artifacts: Some(Artifacts {
            program,
            program_json,
            manifest_json,
        }),
    }
}
