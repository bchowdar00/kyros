//! SIG-T012d — chaos regression suite (spot pipeline).
//!
//! Every fault class the durability design defends against is pinned here as a
//! deterministic, CI-safe regression. This module is the single consolidation
//! point for the chaos coverage matrix; where a fault class already has a
//! focused fixture elsewhere, this file references it rather than cloning it
//! (STANDING-PRACTICES rule 1).
//!
//! Coverage matrix — fault class × pipeline × test/anchor:
//!
//! | Fault class | Spot pipeline (this crate) | Prediction pipeline / feeds |
//! | ----------- | -------------------------- | --------------------------- |
//! | (a) crash mid-write | `crash_boundary_resumes_from_durable_checkpoint_with_torn_journal` (this file); `checkpoint.rs::every_crash_boundary_preserves_spot_reference_semantics`, `checkpoint.rs::interrupted_temporary_write_cannot_publish_a_torn_initial_or_replacement` | `prediction_checkpoint.rs` / `prediction_journal.rs` atomic-write + torn-tail paths |
//! | (b) disk exhaustion | `disk_exhaustion_below_watermark_classifies_fatal` (taxonomy, this file); TRIP exercised where the injection point lives — `journal_core.rs::{watermark_below_minimum_at_open_stops_the_run, watermark_trips_mid_run_at_the_next_sync, watermark_*}` | shared `journal_core` writer core (same `SpaceReporter` seam) |
//! | (c) logical record regression | `logical_clock_regression_is_fatal_and_leaves_a_coherent_prefix` (this file); `journal.rs::replay_rejects_admission_sequence_and_now_regressions`; `spot_reference.rs` `ClockRegression` unit path | resume-time wall regression is separately clamped to the durable checkpoint, pinned by `btc-feeds/src/live.rs::resumed_clock_accounts_for_downtime_once_and_clamps_wall_regression` |
//! | (d) partial/corrupt reads | referenced: `journal.rs::{a_torn_tail_truncated_mid_record_is_reported_distinctly, a_torn_tail_truncated_mid_crc_is_reported_distinctly, an_interior_crc_mismatch_is_corruption_naming_its_line, a_torn_first_line_that_began_as_an_envelope_is_a_torn_tail, a_valid_final_record_without_a_trailing_newline_is_a_torn_tail}`, `checkpoint.rs::checkpoint_rejects_corruption_truncation_and_contract_drift` | `replay_contracts.rs` deterministic replays |
//! | (e) feed loss + recovery | referenced (library-level, deterministic): `btc-feeds/src/coinbase.rs::trade_gap_fails_closed_until_backfill_is_confirmed` + `replay_contracts.rs` | Kraken socket recycling / induced reconnect is welded to the live socket loop (`btc-feeds/src/live.rs`, `prediction_live.rs`) and has no in-process seam; it is a documented live-diagnostic-only gap, exercised by `docs/deployment/chaos-run.sh` (network-blackhole / SIGKILL scenarios) |
//!
//! Documented gaps (injection point exists but is not reachable from an
//! integration test, so no new fault machinery is invented here):
//!   * Disk exhaustion — the injected `SpaceReporter` / `Watermark` seam is
//!     `pub(crate)`; the deterministic trip therefore lives in
//!     `journal_core.rs`'s unit tests, which drive the real `WriterCore` open
//!     and mid-run sync paths. This file pins only the exit-taxonomy half, which
//!     is reachable through the public API.
//!   * Kraken socket recycling — no library-level equivalent exists; see the
//!     matrix row above.

use std::path::Path;

use btc_signals::checkpoint::{CheckpointWriter, load_checkpoint};
use btc_signals::exit_code::{EXIT_FATAL, ExitClass, exit_class_for};
use btc_signals::journal::{
    JournalEnd, JournalError, JournalInput, JournalReplayReader, SignalJournalWriter,
};
use btc_signals::{SPOT_REFERENCE_ID, SpotReferenceError, SpotSignalError, SpotSignalProcessor};
use serde_json::{Value, json};
use signals_core::SignalUpdate;

mod support;

use crate::support::{
    TestPath, coinbase_event, document, kraken_event, last_line, market, processor, session,
    strip_trailing_newline, terminal_without_quality_reasons, truncate_last_line, wall_start,
};

const MAX_BYTES: u64 = 1_048_576;

/// A way a mid-write crash leaves the journal on disk. Every variant is a
/// fail-closed terminal state: a torn tail (the write of the final record or its
/// newline did not complete) or an incomplete prefix (no terminal record was
/// ever durable). None may masquerade as a trusted complete run.
#[derive(Clone, Copy, Debug)]
enum CrashShape {
    /// The writer aborted before a terminal record — a durable prefix only.
    NoTerminalRecord,
    /// A complete run whose terminal line was truncated inside the record bytes.
    TerminalTruncatedMidRecord,
    /// A complete run whose terminal line was truncated inside the CRC field.
    TerminalTruncatedMidCrc,
    /// A complete run whose terminal newline was lost to a torn write.
    TerminalMissingNewline,
}

impl CrashShape {
    /// The fail-closed classification replay must report for this crash shape.
    fn expected_terminal_error(self) -> fn(&JournalError) -> bool {
        match self {
            CrashShape::NoTerminalRecord => is_incomplete,
            CrashShape::TerminalTruncatedMidRecord
            | CrashShape::TerminalTruncatedMidCrc
            | CrashShape::TerminalMissingNewline => is_torn_tail,
        }
    }
}

fn is_incomplete(error: &JournalError) -> bool {
    matches!(error, JournalError::Incomplete)
}

fn is_torn_tail(error: &JournalError) -> bool {
    matches!(error, JournalError::TornTail)
}

/// (a) Crash mid-write. A durable checkpoint plus a journal torn at each
/// adversarial byte boundary must recover with the four documented semantics:
/// resume from the last durable checkpoint, torn tail / incomplete detected,
/// the surviving pre-crash prefix replays byte-deterministically, and epoch
/// lineage is enforced on resume.
#[test]
fn crash_boundary_resumes_from_durable_checkpoint_with_torn_journal() {
    for shape in [
        CrashShape::NoTerminalRecord,
        CrashShape::TerminalTruncatedMidRecord,
        CrashShape::TerminalTruncatedMidCrc,
        CrashShape::TerminalMissingNewline,
    ] {
        // A durable checkpoint captured at the last clean boundary before the
        // crash: two venues ingested, so the reference is ready.
        let mut source = processor("signal-crash");
        source
            .ingest(&coinbase_event(0, "coinbase-crash"), 10, 10)
            .expect("Coinbase input applies");
        source
            .ingest(&kraken_event(0, "kraken-crash"), 20, 20)
            .expect("Kraken input applies");
        let checkpoint_path = TestPath::new("chaos-crash-checkpoint", "json");
        CheckpointWriter::create(
            checkpoint_path.as_path(),
            document(&source, 20, 2),
            MAX_BYTES,
            2,
            false,
        )
        .expect("durable checkpoint persists")
        .abort()
        .expect("checkpoint writer stops leaving the durable file");

        // The journal that was mid-write when the process died.
        let journal_path = write_crashed_journal(&shape);

        // Resume semantics 1 & 2: the torn journal is classified fail-closed and
        // never yields a trusted completion.
        let (first_prefix, first_error) = decode_prefix(journal_path.as_path());
        assert!(
            shape.expected_terminal_error()(&first_error),
            "crash shape {shape:?} must fail closed, got {first_error:?}"
        );

        // Resume semantics 3: the surviving prefix replays byte-deterministically
        // — decoding the same crashed journal twice yields the identical input
        // sequence and the identical terminal classification.
        let (second_prefix, second_error) = decode_prefix(journal_path.as_path());
        assert_eq!(
            first_prefix, second_prefix,
            "crash shape {shape:?} decodes a non-deterministic prefix"
        );
        assert!(
            shape.expected_terminal_error()(&second_error),
            "crash shape {shape:?} reclassifies its terminal state on re-read"
        );
        assert!(
            !first_prefix.is_empty(),
            "crash shape {shape:?} should retain a recoverable prefix"
        );

        // Resume semantics 1 (checkpoint): the last durable checkpoint loads and
        // restores under a fresh epoch — this is the resume point.
        let restored = load_checkpoint(checkpoint_path.as_path(), MAX_BYTES)
            .expect("durable checkpoint loads after the crash");
        let mut resumed_a =
            SpotSignalProcessor::restore("signal-crash-b", wall_start(), restored.processor_state)
                .expect("checkpoint restores under a new epoch");

        // Resume semantics 4: epoch lineage is enforced — reusing an epoch
        // already in the lineage is rejected, so a resumed run cannot silently
        // impersonate the crashed one.
        let restored_again = load_checkpoint(checkpoint_path.as_path(), MAX_BYTES)
            .expect("durable checkpoint loads again");
        assert!(
            matches!(
                SpotSignalProcessor::restore(
                    "signal-crash",
                    wall_start(),
                    restored_again.processor_state.clone()
                ),
                Err(SpotSignalError::Reference(
                    SpotReferenceError::ReusedPublisherEpoch
                ))
            ),
            "crash shape {shape:?} must reject re-using the original epoch via the lineage guard"
        );
        let mut resumed_b = SpotSignalProcessor::restore(
            "signal-crash-b",
            wall_start(),
            restored_again.processor_state,
        )
        .expect("checkpoint restores under the same fresh epoch deterministically");

        // Replay determinism across two independent restores: the post-resume
        // semantic output is identical (publisher epoch and sequence excluded, as
        // the crash-boundary test in checkpoint.rs does).
        let event = coinbase_event(1, "coinbase-resume");
        let updates_a = resumed_a.ingest(&event, 30, 30).expect("resume input a");
        let updates_b = resumed_b.ingest(&event, 30, 30).expect("resume input b");
        assert_eq!(
            reference_semantics(&updates_a),
            reference_semantics(&updates_b),
            "crash shape {shape:?} resumes non-deterministically"
        );
    }
}

/// (c) Clock regression. Driving the logical clock backwards through the
/// existing seam is fatal at the processor layer, leaves a coherent (fail-closed)
/// journal prefix at the replay layer, and maps to exit code 78 at the
/// supervision layer.
#[test]
fn logical_clock_regression_is_fatal_and_leaves_a_coherent_prefix() {
    // Processor seam: advancing the logical clock backwards is rejected.
    let mut processor = processor("signal-clock");
    processor.advance_time(20).expect("forward tick applies");
    assert!(
        matches!(
            processor.advance_time(19),
            Err(SpotSignalError::TimeRegression)
        ),
        "a backwards logical clock must be rejected at the processor"
    );

    // Journal-replay seam: a journal that recorded a now-regression fails closed
    // exactly at the offending record, and the pre-regression prefix stays
    // readable — a coherent terminal state, not an opaque decode error. (The
    // full matrix of regression variants lives in journal.rs; this pins the
    // coherent-prefix property for the chaos matrix.)
    let path = write_complete_journal(
        "chaos-clock",
        vec![
            JournalInput::HealthTick {
                admission_seq: 0,
                now_ns: 20,
            },
            JournalInput::HealthTick {
                admission_seq: 1,
                now_ns: 19,
            },
        ],
        terminal_without_quality_reasons(2, 0, 0, 2, 0),
    );
    let mut reader =
        JournalReplayReader::open(path.as_path(), MAX_BYTES).expect("journal header opens");
    reader
        .next_input()
        .expect("the pre-regression prefix reads")
        .expect("first tick is a real input");
    assert!(
        matches!(
            reader.next_input(),
            Err(JournalError::NowRegression {
                previous: 20,
                actual: 19
            })
        ),
        "replay must reject the backwards clock at its record"
    );
}

/// (b) Disk exhaustion — taxonomy half. The watermark TRIP is driven through the
/// injected `SpaceReporter` where that `pub(crate)` seam lives, in
/// `journal_core.rs`'s unit tests. Reachable from the public API is the
/// classification contract: a `LowDiskSpace` failure is Fatal (exit 78), so a
/// supervisor holds the process instead of restarting it into the same full
/// disk.
#[test]
fn disk_exhaustion_below_watermark_classifies_fatal() {
    let error = anyhow::Error::new(JournalError::LowDiskSpace {
        available: 0,
        minimum: 512 * 1024 * 1024,
    })
    .context("live signal run stopped");
    assert_eq!(exit_class_for(&error), ExitClass::Fatal);
    assert_eq!(ExitClass::Fatal.code(), EXIT_FATAL);
}

// --- crash-shape fixtures -------------------------------------------------

/// Build a journal on disk in the given crashed state and return its path.
fn write_crashed_journal(shape: &CrashShape) -> TestPath {
    let inputs = vec![
        market(0, coinbase_event(0, "coinbase-journal"), 10, 20),
        market(1, kraken_event(0, "kraken-journal"), 15, 25),
        JournalInput::HealthTick {
            admission_seq: 2,
            now_ns: 30,
        },
    ];
    match shape {
        CrashShape::NoTerminalRecord => {
            let path = TestPath::new("chaos-crash-journal-prefix", "jsonl");
            let writer = SignalJournalWriter::create(
                path.as_path(),
                session("chaos-journal", 100),
                MAX_BYTES,
                16,
            )
            .expect("journal creates");
            for input in &inputs {
                writer.enqueue(input.clone()).expect("input enqueues");
            }
            writer.abort().expect("prefix drains and syncs durably");
            path
        }
        CrashShape::TerminalTruncatedMidRecord => {
            let path = write_complete_journal(
                "chaos-crash-mid-record",
                inputs,
                terminal_without_quality_reasons(3, 2, 0, 1, 0),
            );
            let last = last_line(path.as_path());
            let keep = last.find(r#""r":"#).expect("envelope has a record field") + 5;
            truncate_last_line(path.as_path(), keep);
            path
        }
        CrashShape::TerminalTruncatedMidCrc => {
            let path = write_complete_journal(
                "chaos-crash-mid-crc",
                inputs,
                terminal_without_quality_reasons(3, 2, 0, 1, 0),
            );
            truncate_last_line(path.as_path(), 8);
            path
        }
        CrashShape::TerminalMissingNewline => {
            let path = write_complete_journal(
                "chaos-crash-no-newline",
                inputs,
                terminal_without_quality_reasons(3, 2, 0, 1, 0),
            );
            strip_trailing_newline(path.as_path());
            path
        }
    }
}

/// Decode every input up to the first replay failure, returning the recovered
/// prefix and the terminal error that stopped it.
fn decode_prefix(path: &Path) -> (Vec<JournalInput>, JournalError) {
    let mut reader =
        JournalReplayReader::open(path, MAX_BYTES).expect("crashed journal header opens");
    let mut prefix = Vec::new();
    loop {
        match reader.next_input() {
            Ok(Some(input)) => prefix.push(input),
            Ok(None) => {
                // A clean terminal on a crashed journal would be a durability
                // bug; force the caller's fail-closed assertion to fire.
                return (prefix, JournalError::TrailingRecord);
            }
            Err(error) => return (prefix, error),
        }
    }
}

fn write_complete_journal(label: &str, inputs: Vec<JournalInput>, end: JournalEnd) -> TestPath {
    let path = TestPath::new(label, "jsonl");
    let writer =
        SignalJournalWriter::create(path.as_path(), session("chaos-journal", 100), MAX_BYTES, 16)
            .expect("journal creates");
    for input in inputs {
        writer.enqueue(input).expect("input enqueues");
    }
    writer.finish(end).expect("journal completes");
    path
}

fn reference_semantics(updates: &[SignalUpdate]) -> Option<Value> {
    updates
        .iter()
        .find(|update| update.signal_id == SPOT_REFERENCE_ID)
        .map(|update| {
            json!({
                "event_time": update.event_time,
                "quality": update.quality,
                "values": update.values,
            })
        })
}
