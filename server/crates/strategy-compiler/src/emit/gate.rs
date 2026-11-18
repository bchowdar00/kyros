//! The runtime capability gate: refuse what the exchange runtime cannot run.
//!
//! As of v0.3 the gate refuses nothing: every construct the validator accepts —
//! timers, on tables, cancel-replace, edge fire modes, `any` groups, multi-signal
//! conditions, every [`crate::program::PlatformRead`], and `legs.*` fill-context
//! reads — lowers faithfully. The module stays as the place a future runtime limit
//! goes: a refusal is an ordinary error diagnostic emitted here, before lowering, so
//! nothing downstream ever silently mislowers.

use crate::validate::{Diagnostic, ValidStrategy};

/// Sweep a validated strategy for runtime-refused constructs.
///
/// Returns every refusal (collect-all, document order — the same contract as the
/// validator). Empty means the strategy lowers faithfully; any entry is an error and
/// the build must not proceed. Currently always empty.
pub fn gate(_valid: &ValidStrategy) -> Vec<Diagnostic> {
    Vec::new()
}
