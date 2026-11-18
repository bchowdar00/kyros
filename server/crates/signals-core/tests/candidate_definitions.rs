//! Drift guard for the unadopted BTC candidate definition drafts.
//!
//! Schema conformance for `definitions/btc/candidates/*.json` is asserted by
//! `schema_definitions.rs` (its `candidates` arm applies the same
//! signal-definition schema as adopted definitions). This test covers only what
//! that one does not: that no candidate draft has silently double-appeared on
//! the adopted surface, and that the directory carries its explanatory README.
//!
//! The directory is allowed to be empty. Adoption drains it one identity at a
//! time, and the last adoption must not be the one that breaks this guard: an
//! empty candidate directory is the intended terminal state, so the assertions
//! below are "the directory exists and everything in it is clean", never "the
//! directory is non-empty".

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use signals_core::SignalDefinition;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn json_files(directory: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = fs::read_dir(directory)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", directory.display()))
        .map(|entry| entry.expect("directory entry").path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect();
    files.sort();
    files
}

fn parse_definition(path: &Path) -> SignalDefinition {
    let bytes =
        fs::read(path).unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
    serde_json::from_slice(&bytes)
        .unwrap_or_else(|error| panic!("failed to parse {}: {error}", path.display()))
}

#[test]
fn candidate_directory_exists_and_has_a_readme() {
    let directory = repo_root().join("definitions/btc/candidates");
    assert!(
        directory.is_dir(),
        "the candidate directory must exist at {}",
        directory.display()
    );
    let readme = directory.join("README.md");
    assert!(
        readme.is_file(),
        "the candidate directory must document its unadopted status at {}",
        readme.display()
    );
}

#[test]
fn candidate_drafts_have_not_double_appeared_on_the_adopted_surface() {
    let root = repo_root();
    let candidates_dir = root.join("definitions/btc/candidates");
    let signals_dir = root.join("definitions/btc/signals");

    // No non-empty requirement: an emptied candidate directory means every
    // draft has been adopted, which is a success state, not drift.
    let candidate_files = json_files(&candidates_dir);

    // The adopted surface, keyed by identity and by on-disk file name.
    let adopted: BTreeSet<(String, String)> = json_files(&signals_dir)
        .iter()
        .map(|path| {
            let definition = parse_definition(path);
            (definition.signal_id, definition.version)
        })
        .collect();

    for candidate_path in candidate_files {
        let definition = parse_definition(&candidate_path);
        let identity = (definition.signal_id.clone(), definition.version.clone());
        assert!(
            !adopted.contains(&identity),
            "candidate {}@{} is also present on the adopted surface (definitions/btc/signals)",
            identity.0,
            identity.1
        );

        // A candidate must not shadow an adopted file name either: an
        // owner-approved change moves the file, it never leaves a copy behind.
        let file_name = candidate_path
            .file_name()
            .expect("candidate has a file name");
        let shadow = signals_dir.join(file_name);
        assert!(
            !shadow.exists(),
            "candidate file {} is also present at {}",
            candidate_path.display(),
            shadow.display()
        );
    }
}
