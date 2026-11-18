//! Layer (a): every expression in the corpus inventory parses, and every AST is
//! snapshotted. These snapshots are the parser's contract and the anchor for future
//! differential testing (interpreter, alternative backends) — treat their stability as
//! a feature.

use std::fs;
use std::path::Path;

/// FNV-1a 64-bit — stable across Rust versions (std's DefaultHasher is not), used to
/// give each snapshot a collision-free, source-derived name.
fn fnv1a(s: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in s.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn snapshot_name(source: &str) -> String {
    let prefix: String = source
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .take(50)
        .collect();
    format!("{prefix}-{:016x}", fnv1a(source))
}

#[test]
fn corpus_inventory_parses_and_snapshots() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/expressions.txt");
    let text = fs::read_to_string(&path).expect("inventory file is readable");
    let mut count = 0usize;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        count += 1;
        let name = snapshot_name(line);
        if line.starts_with("set ") || line.starts_with("push ") {
            let ast = strategy_compiler::parse::parse_stmt(line)
                .unwrap_or_else(|e| panic!("inventory statement failed to parse:\n{e}"));
            insta::assert_debug_snapshot!(name, ast);
        } else {
            let ast = strategy_compiler::parse::parse_expr(line)
                .unwrap_or_else(|e| panic!("inventory expression failed to parse:\n{e}"));
            insta::assert_debug_snapshot!(name, ast);
        }
    }
    assert_eq!(
        count, 46,
        "inventory count drifted — update this assert deliberately"
    );
}
