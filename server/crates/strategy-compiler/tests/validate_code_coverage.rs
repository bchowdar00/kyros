//! The coverage contract (PERMANENT): every code in the `DiagCode` roster is
//! exercised by the negative suite. Adding a code without a negative case fails this
//! test by name; deleting a case that was a code's only exercise does too.

use std::collections::BTreeSet;

use strategy_compiler::validate::{DiagCode, DiagDetail};

mod common;

#[test]
fn every_diag_code_is_exercised() {
    let mut exercised: BTreeSet<&'static str> = BTreeSet::new();
    let mut record = |diagnostics: &[strategy_compiler::validate::Diagnostic]| {
        for diagnostic in diagnostics {
            if let DiagDetail::Rule { code, .. } = &diagnostic.detail {
                exercised.insert(code.as_str());
            }
        }
    };

    // The catalog, plus the three scenario documents that carry codes the catalog
    // cannot (cycles suppress their dependents; the envelope cases need their own
    // envelopes).
    for case in common::all_cases() {
        record(&common::run(case.json));
    }
    record(&common::run(common::CYCLE_DOC));
    record(&common::run_with_envelope(
        common::MISSING_READ_DOC,
        &common::sparse_envelope(),
    ));
    record(&common::run_with_envelope(
        common::BASE,
        &common::bad_envelope(),
    ));

    let missing: Vec<&str> = DiagCode::ALL
        .iter()
        .map(|code| code.as_str())
        .filter(|code| !exercised.contains(code))
        .collect();
    assert!(
        missing.is_empty(),
        "diag codes with no negative-suite exercise: {}",
        missing.join(", ")
    );
}
