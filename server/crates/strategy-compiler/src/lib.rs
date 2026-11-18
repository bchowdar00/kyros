//! The Tiny strategy compiler: IR types, parse, validate, lower, emit.
//!
//! One library, no I/O. [`compile`] is the single entry point: it takes an
//! LLM-authored [`ir::Strategy`] plus the venue-supplied [`ir::Registry`] and
//! [`ir::Envelope`] and returns every diagnostic found and, iff nothing
//! blocked, the emitted artifacts.
//!
//! Each stage is a module, and each stage is a function from the previous
//! stage's output:
//!
//! - [`ir`] — the document, registry, envelope and manifest types. Data only.
//! - [`parse`] — lexer, parser and AST for the expression/statement strings.
//! - [`validate`] — the passes, diagnostics and lints; produces a
//!   [`validate::ValidStrategy`].
//! - [`lower`] — `ValidStrategy` → [`lower::Lowered`].
//! - [`emit`] — the program and the manifest from a `Lowered`.
//! - [`program`] — the program types: the runtime's executable artifact.
//!
//! Invariants: the IR describes and never computes, everything is bounded at
//! compile time, output is byte-deterministic, and nothing downstream of the
//! validator accepts an unvalidated document.

pub mod emit;
pub mod ir;
pub mod lower;
pub mod parse;
pub mod program;
pub mod validate;

pub use emit::{Artifacts, BuildOutput};
pub use program::Program;
pub use ir::{Envelope, Registry, Strategy};
pub use validate::Diagnostic;

/// Compiler version stamped into emitted artifacts.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// A successful compilation: the artifacts plus any warnings that rode along.
#[derive(Debug, Clone, PartialEq)]
pub struct Compiled {
    /// The program (typed and serialized) and the manifest.
    pub artifacts: Artifacts,
    /// Non-blocking diagnostics (warnings and lints).
    pub warnings: Vec<Diagnostic>,
}

/// Compile one strategy document against a registry and envelope.
///
/// `Err` carries every diagnostic (errors and warnings) when anything blocked
/// the build; nothing is partially emitted. `Ok` carries the artifacts and any
/// warnings.
pub fn compile(
    strategy: &Strategy,
    registry: &Registry,
    envelope: &Envelope,
) -> Result<Compiled, Vec<Diagnostic>> {
    let output = emit::build(strategy, registry, envelope, VERSION);
    match output.artifacts {
        Some(artifacts) if !output.has_errors() => Ok(Compiled {
            artifacts,
            warnings: output.diagnostics,
        }),
        _ => Err(output.diagnostics),
    }
}
