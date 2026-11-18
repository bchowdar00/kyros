//! Artifact identity: the canonical document serialization, its hash, and the
//! strategy id derived from it.
//!
//! The canonical serialization is the validated document re-serialized by serde: struct
//! fields in declaration order (the ratified section order), name-keyed maps in
//! `NamedMap`'s lexicographic order, array sections in document order, decimals as
//! their canonical source strings. Two spellings of the same document (reordered JSON
//! keys, whitespace) serialize identically; any semantic change changes the bytes.

use sha2::{Digest, Sha256};

/// The canonical serialization of a validated document.
///
/// Compact (no whitespace) — the exact bytes the source hash covers.
pub fn canonical_document(strategy: &crate::ir::Strategy) -> String {
    serde_json::to_string(strategy)
        .expect("IR types serialize infallibly: no maps with non-string keys, no non-finite floats")
}

/// sha256 over the canonical serialization, as 64 lowercase hex digits.
pub fn source_hash(strategy: &crate::ir::Strategy) -> String {
    let digest = Sha256::digest(canonical_document(strategy).as_bytes());
    hex(&digest)
}

/// `Strategy_` + the first 12 hex of the source hash.
///
/// Recompiling the same document under a newer compiler preserves the id (and
/// therefore runtime state keyed on it); any document change resets it.
pub fn strategy_id(source_hash: &str) -> String {
    format!("Strategy_{}", &source_hash[..12])
}

/// Lowercase hex, no allocation tricks — 32 bytes, once per compile.
pub fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}
