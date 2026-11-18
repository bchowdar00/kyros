//! Family F negatives: each idiom lint's firing case AND its near-miss (the boundary
//! is part of the contract — a near-miss snapshot of "(no diagnostics)" pins where
//! the lint stops).

mod common;

#[test]
fn lint_negatives() {
    for (name, rendered) in common::rendered_cases(&common::lints_cases()) {
        insta::assert_snapshot!(name, rendered);
    }
}
