//! Integration tests for the shared file-generation layout (C5 B1).
//!
//! These assert the on-disk layout the three runtimes will produce is
//! byte-compatible with what `docs/deployment/production-btc/live-run.sh`
//! produces today, and that healthcheck.sh / retention.sh readers accept it.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use btc_signals::file_generation::{
    CURRENT_FILE_NAME, FileGenerationError, FileGenerationLayout, JOURNAL_FILE_NAME,
    OUTPUT_FILE_NAME, RUN_META_FILE_NAME, RunMeta,
};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A unique, caller-owned scratch directory removed on drop.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(tag: &str) -> Self {
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "btc-file-generation-{tag}-{}-{nanos}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn sample_meta(lane: &str, now_unix: u64) -> RunMeta {
    RunMeta {
        lane: lane.to_string(),
        publisher_epoch: format!("live-{lane}-{now_unix}"),
        generation: format!("gen-live-{lane}-{now_unix}"),
        started_utc: "2026-09-04T12:00:00Z".to_string(),
        duration_seconds: 3600,
        max_output_bytes: 5_368_709_120,
        max_journal_bytes: 5_368_709_120,
        max_updates: 34_000_000,
        headroom_factor: 2,
        resumed: false,
    }
}

#[test]
fn open_next_names_and_places_the_generation() {
    let root = TempDir::new("naming");
    let layout = FileGenerationLayout::new(root.path(), "spot").unwrap();
    let now_unix = 1_700_000_000;
    let meta = sample_meta("spot", now_unix);

    let open = layout.open_next(now_unix, None, &meta).unwrap();

    let name = "gen-live-spot-1700000000";
    assert_eq!(open.dir, root.path().join(name));
    assert_eq!(
        open.output_path,
        root.path().join(name).join(OUTPUT_FILE_NAME)
    );
    assert_eq!(
        open.journal_path,
        root.path().join(name).join(JOURNAL_FILE_NAME)
    );
    assert_eq!(open.opened_unix, now_unix);
    assert!(open.dir.is_dir());
    assert!(open.run_meta_path().is_file());
    // The runtime, not the layout, creates output/journal.
    assert!(!open.output_path.exists());
    assert!(!open.journal_path.exists());
}

#[test]
fn open_next_with_rotation_records_the_period_not_the_duration() {
    let root = TempDir::new("rotation-period");
    let layout = FileGenerationLayout::new(root.path(), "spot").unwrap();
    let now_unix = 1_700_000_500;
    // duration_seconds (total run bound) differs from the rotation period.
    let meta = sample_meta("spot", now_unix); // duration_seconds == 3600
    let rotate_seconds = 900;

    let open = layout
        .open_next_with_rotation(now_unix, None, &meta, rotate_seconds)
        .unwrap();
    let document = std::fs::read_to_string(open.run_meta_path()).unwrap();
    let json: serde_json::Value = serde_json::from_str(&document).unwrap();

    // duration_seconds is preserved verbatim; rotation_seconds is the period.
    assert_eq!(json["duration_seconds"], serde_json::json!(3600));
    assert_eq!(json["rotation_seconds"], serde_json::json!(900));

    // The thin open_next wrapper still records rotation_seconds == duration.
    let now_unix = 1_700_000_501;
    let meta = sample_meta("spot", now_unix);
    let open = layout.open_next(now_unix, None, &meta).unwrap();
    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(open.run_meta_path()).unwrap()).unwrap();
    assert_eq!(json["rotation_seconds"], serde_json::json!(3600));
}

#[test]
fn new_accepts_a_symlinked_lane_root() {
    let backing = TempDir::new("symlink-target");
    let real = backing.path().join("real-lane-root");
    std::fs::create_dir(&real).unwrap();
    let link = backing.path().join("lane-root-link");
    std::os::unix::fs::symlink(&real, &link).unwrap();

    // The lane root is a symlink to an owned directory: it must be accepted and
    // usable, with the generation created through the link into the target.
    let layout = FileGenerationLayout::new(&link, "spot").unwrap();
    let now_unix = 1_700_000_600;
    let meta = sample_meta("spot", now_unix);
    let open = layout.open_next(now_unix, None, &meta).unwrap();
    assert!(open.run_meta_path().is_file());
    assert!(
        real.join("gen-live-spot-1700000600").is_dir(),
        "generation should be created under the symlink target"
    );
}

#[test]
fn open_next_refuses_an_existing_generation() {
    let root = TempDir::new("refuse-existing");
    let layout = FileGenerationLayout::new(root.path(), "spot").unwrap();
    let now_unix = 1_700_000_001;
    let meta = sample_meta("spot", now_unix);

    layout.open_next(now_unix, None, &meta).unwrap();
    let again = layout.open_next(now_unix, None, &meta);
    assert!(matches!(
        again,
        Err(FileGenerationError::GenerationExists { .. })
    ));
}

#[test]
fn open_next_refuses_metadata_that_disagrees_with_the_directory() {
    let root = TempDir::new("refuse-meta");
    let layout = FileGenerationLayout::new(root.path(), "spot").unwrap();
    let now_unix = 1_700_000_002;

    let mut wrong_lane = sample_meta("spot", now_unix);
    wrong_lane.lane = "bybit".to_string();
    assert!(matches!(
        layout.open_next(now_unix, None, &wrong_lane),
        Err(FileGenerationError::MetaMismatch { field: "lane", .. })
    ));

    let mut wrong_generation = sample_meta("spot", now_unix);
    wrong_generation.generation = "gen-live-spot-9".to_string();
    assert!(matches!(
        layout.open_next(now_unix, None, &wrong_generation),
        Err(FileGenerationError::MetaMismatch {
            field: "generation",
            ..
        })
    ));
}

#[test]
fn new_refuses_a_lane_root_that_is_not_a_directory() {
    let root = TempDir::new("not-a-dir");
    let file = root.path().join("a-file");
    std::fs::write(&file, b"x").unwrap();
    assert!(matches!(
        FileGenerationLayout::new(&file, "spot"),
        Err(FileGenerationError::LaneRootNotDirectory { .. })
    ));

    let missing = root.path().join("does-not-exist");
    assert!(matches!(
        FileGenerationLayout::new(&missing, "spot"),
        Err(FileGenerationError::LaneRootNotDirectory { .. })
    ));
}

#[test]
fn current_is_written_atomically_with_no_temp_left_behind() {
    let root = TempDir::new("current-atomic");
    let layout = FileGenerationLayout::new(root.path(), "binance").unwrap();
    let now_unix = 1_700_000_100;
    let meta = sample_meta("binance", now_unix);

    let open = layout.open_next(now_unix, None, &meta).unwrap();

    let current = root.path().join(CURRENT_FILE_NAME);
    assert_eq!(
        std::fs::read_to_string(&current).unwrap(),
        "gen-live-binance-1700000100\n"
    );
    // No temp files survive an atomic write.
    assert!(!root.path().join("CURRENT.tmp").exists());
    assert!(!open.dir.join(format!("{RUN_META_FILE_NAME}.tmp")).exists());
}

#[test]
fn seal_sets_sealed_utc_and_changes_nothing_else() {
    let root = TempDir::new("seal");
    let layout = FileGenerationLayout::new(root.path(), "spot").unwrap();
    let now_unix = 1_700_000_200;
    let meta = sample_meta("spot", now_unix);

    let mut open = layout
        .open_next(now_unix, Some("gen-live-spot-1700000100"), &meta)
        .unwrap();
    let before = std::fs::read_to_string(open.run_meta_path()).unwrap();
    assert!(before.contains("  \"sealed_utc\": null\n"));
    assert!(before.contains("  \"rotated_from\": \"gen-live-spot-1700000100\",\n"));

    open.seal("2026-09-04T13:00:00Z").unwrap();
    let after = std::fs::read_to_string(open.run_meta_path()).unwrap();

    let expected = before.replace(
        "  \"sealed_utc\": null\n",
        "  \"sealed_utc\": \"2026-09-04T13:00:00Z\"\n",
    );
    assert_eq!(after, expected);
    assert_eq!(open.sealed_utc(), Some("2026-09-04T13:00:00Z"));
    assert!(!open.dir.join(format!("{RUN_META_FILE_NAME}.tmp")).exists());

    // Everything but sealed_utc is byte-stable; confirm semantically too.
    let a: serde_json::Value = serde_json::from_str(&before).unwrap();
    let b: serde_json::Value = serde_json::from_str(&after).unwrap();
    for key in [
        "lane",
        "publisher_epoch",
        "generation",
        "started_utc",
        "duration_seconds",
        "max_output_bytes",
        "max_journal_bytes",
        "max_updates",
        "headroom_factor",
        "resumed",
        "rotated_from",
        "rotation_seconds",
    ] {
        assert_eq!(a[key], b[key], "field {key} changed on seal");
    }
    assert_eq!(b["sealed_utc"], serde_json::json!("2026-09-04T13:00:00Z"));
}

/// The `RUN-META.json` this module writes reproduces, byte-for-byte, the ten
/// fields live-run.sh writes — proven by running live-run.sh's own heredoc block
/// and comparing. The three rotation fields (`rotated_from`, `rotation_seconds`,
/// `sealed_utc`) are the only additions; the projection below removes exactly
/// those three lines and the trailing comma they forced onto `resumed`, then the
/// remainder must equal the script's output verbatim.
#[test]
fn run_meta_matches_live_run_sh_byte_for_byte() {
    let script = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/deployment/production-btc/live-run.sh");
    let script_text = std::fs::read_to_string(&script)
        .unwrap_or_else(|error| panic!("cannot read live-run.sh at {script:?}: {error}"));
    let heredoc = extract_run_meta_heredoc(&script_text);

    let fixture = TempDir::new("live-run-fixture");
    let generation_dir = fixture.path().join("gen-live-spot-1700000000");
    std::fs::create_dir_all(&generation_dir).unwrap();
    let checkpoint = fixture.path().join("checkpoint.json"); // does not exist -> resumed:false

    let mut program = String::new();
    program.push_str("set -eu\n");
    program.push_str("KIND='spot'\n");
    program.push_str("EPOCH='live-spot-1700000000'\n");
    program.push_str("DURATION_SECONDS=3600\n");
    program.push_str("MAX_OUTPUT_BYTES=5368709120\n");
    program.push_str("MAX_JOURNAL_BYTES=5368709120\n");
    program.push_str("MAX_UPDATES=34000000\n");
    program.push_str("HEADROOM_FACTOR=2\n");
    program.push_str(&format!("GENERATION='{}'\n", generation_dir.display()));
    program.push_str(&format!("CHECKPOINT='{}'\n", checkpoint.display()));
    program.push_str(&heredoc);

    let status = Command::new("bash")
        .arg("-c")
        .arg(&program)
        .status()
        .unwrap_or_else(|error| panic!("cannot run bash for the live-run.sh fixture: {error}"));
    assert!(status.success(), "live-run.sh RUN-META block failed");

    let script_meta = std::fs::read_to_string(generation_dir.join(RUN_META_FILE_NAME)).unwrap();

    // Use the script's own started_utc (it calls `date` at write time) so the
    // two documents share every value.
    let script_json: serde_json::Value = serde_json::from_str(&script_meta).unwrap();
    let started_utc = script_json["started_utc"].as_str().unwrap().to_string();

    let root = TempDir::new("live-run-ours");
    let layout = FileGenerationLayout::new(root.path(), "spot").unwrap();
    let mut meta = sample_meta("spot", 1_700_000_000);
    meta.started_utc = started_utc;
    let open = layout.open_next(1_700_000_000, None, &meta).unwrap();
    let ours = std::fs::read_to_string(open.run_meta_path()).unwrap();

    assert_eq!(
        project_to_live_run_fields(&ours),
        script_meta,
        "projected RUN-META must equal live-run.sh's output byte-for-byte"
    );

    // And the shared keys agree semantically with the additions present.
    let ours_json: serde_json::Value = serde_json::from_str(&ours).unwrap();
    for (key, value) in script_json.as_object().unwrap() {
        assert_eq!(&ours_json[key], value, "shared field {key}");
    }
    assert_eq!(ours_json["rotated_from"], serde_json::Value::Null);
    assert_eq!(ours_json["rotation_seconds"], serde_json::json!(3600));
    assert_eq!(ours_json["sealed_utc"], serde_json::Value::Null);
}

/// Extract the `cat > "$GENERATION/RUN-META.json" <<EOF ... EOF` block verbatim.
fn extract_run_meta_heredoc(script: &str) -> String {
    let start_marker = "cat > \"$GENERATION/RUN-META.json\" <<EOF";
    let start = script
        .find(start_marker)
        .expect("live-run.sh RUN-META heredoc marker not found");
    let rest = &script[start..];
    let mut collected = String::new();
    let mut seen_open = false;
    for line in rest.lines() {
        collected.push_str(line);
        collected.push('\n');
        if seen_open && line == "EOF" {
            return collected;
        }
        if line.contains("<<EOF") {
            seen_open = true;
        }
    }
    panic!("live-run.sh RUN-META heredoc terminator not found");
}

/// Drop the three rotation lines and the comma they forced onto `resumed`.
fn project_to_live_run_fields(document: &str) -> String {
    let mut lines: Vec<&str> = document.split('\n').collect();
    lines.retain(|line| {
        let key = line.trim_start();
        !(key.starts_with("\"rotated_from\":")
            || key.starts_with("\"rotation_seconds\":")
            || key.starts_with("\"sealed_utc\":"))
    });
    let mut projected = lines.join("\n");
    projected = projected.replace("  \"resumed\": false,\n", "  \"resumed\": false\n");
    projected = projected.replace("  \"resumed\": true,\n", "  \"resumed\": true\n");
    projected
}

#[test]
fn healthcheck_style_reader_accepts_the_layout() {
    let root = TempDir::new("healthcheck");
    let lane = "bybit";
    let layout = FileGenerationLayout::new(root.path(), lane).unwrap();
    let now_unix = 1_700_000_300;
    let meta = sample_meta(lane, now_unix);
    let open = layout.open_next(now_unix, None, &meta).unwrap();

    // The runtime would create output.jsonl; simulate the first write.
    std::fs::write(&open.output_path, b"{}\n").unwrap();

    // healthcheck.sh: read CURRENT -> $lane_root/$generation -> output.jsonl mtime.
    let current = std::fs::read_to_string(root.path().join(CURRENT_FILE_NAME)).unwrap();
    let generation = current.trim_end_matches('\n');
    assert_eq!(generation, format!("gen-live-{lane}-{now_unix}"));
    let generation_dir = root.path().join(generation);
    assert!(generation_dir.is_dir());
    assert_eq!(generation_dir, open.dir);

    let output = generation_dir.join(OUTPUT_FILE_NAME);
    assert_eq!(output, open.output_path);
    let mtime = std::fs::metadata(&output).unwrap().modified().unwrap();
    let age = SystemTime::now().duration_since(mtime).unwrap();
    assert!(
        age < Duration::from_secs(30),
        "freshly written output is stale"
    );
}

#[test]
fn retention_style_name_check_accepts_the_layout() {
    let root = TempDir::new("retention");
    let lane = "oracle";
    let layout = FileGenerationLayout::new(root.path(), lane).unwrap();
    let now_unix = 1_700_000_400;
    let meta = sample_meta(lane, now_unix);
    let open = layout.open_next(now_unix, None, &meta).unwrap();

    let name = open.dir.file_name().unwrap().to_str().unwrap();
    assert!(
        retention_name_ok(name, lane),
        "{name} should match gen-live-{lane}-<digits>"
    );

    // Counter-examples the retention guard must reject.
    assert!(!retention_name_ok("gen-live-oracle-", lane));
    assert!(!retention_name_ok("gen-live-oracle-12x", lane));
    assert!(!retention_name_ok("gen-live-binance-123", lane));
    assert!(!retention_name_ok("checkpoint.json", lane));
}

/// retention.sh guard 2: basename is exactly `gen-live-<lane>-<digits>`.
fn retention_name_ok(name: &str, lane: &str) -> bool {
    let prefix = format!("gen-live-{lane}-");
    match name.strip_prefix(&prefix) {
        Some(suffix) => !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit()),
        None => false,
    }
}
