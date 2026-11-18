//! The gate's positive contract: all eight corpus fixtures pass with zero refusals.
//! The corpus uses timers, on tables, cancel-replace, edge fire modes, platform
//! position reads, and cross-signal conjunctions — a gate that refuses any of them
//! fails the assignment. (As of v0.3 the gate refuses nothing at all.)

mod common;

#[test]
fn corpus_passes_the_gate() {
    for name in common::STRATEGIES {
        let valid = common::corpus_valid(name);
        let refusals = strategy_compiler::emit::gate(&valid);
        assert!(
            refusals.is_empty(),
            "{name} was refused by the capability gate:\n{}",
            common::render(&refusals)
        );
    }
}
