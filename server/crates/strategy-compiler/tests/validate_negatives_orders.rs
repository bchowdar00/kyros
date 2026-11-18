//! Family E negatives: substitution DAG and order-expression rules, plus the
//! cycle-cascade case and the read-set envelope check.

mod common;

#[test]
fn orders_negatives() {
    for (name, rendered) in common::rendered_cases(&common::orders_cases()) {
        insta::assert_snapshot!(name, rendered);
    }
}

#[test]
fn cycle_reports_once_and_poisons_dependents() {
    // Cascade-suppression design case 2: `a -> b -> a` is ONE expr-cycle diagnostic.
    // `c` references `b` (transitively poisoned: no diagnostic), and the size built
    // on `c` suppresses its clamp/magnitude checks rather than echoing the cycle.
    let diagnostics = common::run(common::CYCLE_DOC);
    assert_eq!(
        diagnostics.len(),
        1,
        "expected exactly the expr-cycle diagnostic:\n{}",
        common::render(&diagnostics)
    );
    insta::assert_snapshot!("neg_expr_cycle", common::render(&diagnostics));
}

#[test]
fn missing_envelope_read_field() {
    // The program reads envelope.max_position_per_market; the supplied envelope
    // omits it. This is the post-P5 half of the split envelope pass.
    let diagnostics =
        common::run_with_envelope(common::MISSING_READ_DOC, &common::sparse_envelope());
    insta::assert_snapshot!(
        "neg_envelope_missing_read_field",
        common::render(&diagnostics)
    );
}
