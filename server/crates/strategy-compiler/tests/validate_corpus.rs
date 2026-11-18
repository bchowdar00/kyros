//! The corpus gate: all eight fixtures validate with ZERO ERRORS, and the exact
//! warning set of each is snapshotted — a warning-clean corpus is asserted, never
//! assumed, and a new warning on any fixture is a reviewed diff.

use std::fs;
use std::path::{Path, PathBuf};

mod common;

const STRATEGIES: [&str; 8] = [
    "break-quoter",
    "capitulation-fade",
    "curve-discount",
    "endgame-sweep",
    "favorite-panic",
    "late-momentum",
    "momentum-flip",
    "pinnacle-bid",
];

fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus")
}

#[test]
fn corpus_validates_clean_of_errors() {
    for name in STRATEGIES {
        let text =
            fs::read_to_string(corpus_dir().join(format!("{name}.json"))).expect("fixture reads");
        let (diagnostics, valid) = common::run_full(&text, &common::envelope());
        let errors: Vec<String> = diagnostics
            .iter()
            .filter(|d| d.is_error())
            .map(|d| d.to_string())
            .collect();
        assert!(
            errors.is_empty(),
            "{name} has validation errors:\n{}",
            errors.join("\n")
        );
        assert!(
            valid.is_some(),
            "{name}: zero errors must produce the ValidStrategy typestate"
        );
    }
}

#[test]
fn corpus_warnings_are_exactly_these() {
    for name in STRATEGIES {
        let text =
            fs::read_to_string(corpus_dir().join(format!("{name}.json"))).expect("fixture reads");
        let (diagnostics, _) = common::run_full(&text, &common::envelope());
        let warnings: Vec<String> = diagnostics
            .iter()
            .filter(|d| !d.is_error())
            .map(|d| d.to_string())
            .collect();
        let rendered = if warnings.is_empty() {
            "(no warnings)".to_string()
        } else {
            warnings.join("\n")
        };
        insta::assert_snapshot!(format!("corpus_warnings__{name}"), rendered);
    }
}
