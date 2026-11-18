//! Family D negatives: structural rules and envelope sanity.

mod common;

#[test]
fn structural_negatives() {
    for (name, rendered) in common::rendered_cases(&common::structural_cases()) {
        insta::assert_snapshot!(name, rendered);
    }
}

#[test]
fn envelope_sanity() {
    // Envelope diagnostics anchor on the `envelope.*` pseudo-path; the document is
    // the clean base — every finding below is the envelope's own.
    let diagnostics = common::run_with_envelope(common::BASE, &common::bad_envelope());
    insta::assert_snapshot!("neg_envelope_sanity", common::render(&diagnostics));
}
