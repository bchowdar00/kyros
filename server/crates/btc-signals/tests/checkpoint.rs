use std::path::Path;

use btc_signals::checkpoint::{CheckpointError, CheckpointWriter, load_checkpoint};
use btc_signals::{SpotSignalProcessor, UnavailableReason, Venue};
use serde_json::{Value, json};
use signals_core::{SignalUpdate, SourceEvent};

mod support;

use crate::support::{TestPath, coinbase_event, document, kraken_event, processor, wall_start};

const MAX_BYTES: u64 = 1_048_576;

#[test]
fn checkpoint_round_trips_and_atomically_replaces_owner_only_state() {
    let path = TestPath::new("checkpoint-round-trip", "json");
    let mut processor = processor("signal-a");
    processor
        .ingest(&coinbase_event(0, "coinbase-a"), 10, 10)
        .expect("Coinbase input applies");
    let initial = document(&processor, 10, 1);
    let writer = CheckpointWriter::create(path.as_path(), initial.clone(), MAX_BYTES, 4, false)
        .expect("initial checkpoint persists");
    assert_eq!(
        load_checkpoint(path.as_path(), MAX_BYTES).expect("checkpoint loads"),
        initial
    );

    processor
        .ingest(&kraken_event(0, "kraken-a"), 20, 20)
        .expect("Kraken input applies");
    let final_checkpoint = document(&processor, 20, 2);
    let report = writer
        .finish(final_checkpoint.clone())
        .expect("final checkpoint commits");
    assert_eq!(report.checkpoints_written, 2);
    assert_eq!(report.last_checkpoint_time_ns, 20);
    assert_eq!(
        load_checkpoint(path.as_path(), MAX_BYTES).expect("replacement loads"),
        final_checkpoint
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        assert_eq!(
            std::fs::metadata(path.as_path())
                .expect("checkpoint metadata reads")
                .mode()
                & 0o777,
            0o600
        );
    }
}

#[test]
fn fresh_writer_refuses_to_overwrite_recovery_state() {
    let path = TestPath::new("checkpoint-no-overwrite", "json");
    let checkpoint = document(&processor("signal-a"), 0, 0);
    CheckpointWriter::create(path.as_path(), checkpoint.clone(), MAX_BYTES, 1, false)
        .expect("first checkpoint creates")
        .abort()
        .expect("first writer stops");
    assert!(matches!(
        CheckpointWriter::create(path.as_path(), checkpoint, MAX_BYTES, 1, false),
        Err(CheckpointError::Io(error)) if error.kind() == std::io::ErrorKind::AlreadyExists
    ));
}

#[test]
fn interrupted_temporary_write_cannot_publish_a_torn_initial_or_replacement() {
    let path = TestPath::absent("checkpoint-interrupted-atomic-write", "json");
    let temporary = TestPath::sibling(&path, "interrupted");
    std::fs::write(temporary.as_path(), b"{\"payload\":").expect("torn temporary writes");
    set_private_permissions(temporary.as_path());
    assert!(
        !path.as_path().exists(),
        "a torn initial temporary file must not publish the final path"
    );

    let initial = document(&processor("signal-a"), 0, 0);
    let writer = CheckpointWriter::create(path.as_path(), initial.clone(), MAX_BYTES, 2, false)
        .expect("atomic initial publication ignores an unrelated torn temporary");
    assert_eq!(
        load_checkpoint(path.as_path(), MAX_BYTES).expect("initial checkpoint loads"),
        initial
    );

    std::fs::write(temporary.as_path(), b"{\"payload\":null}")
        .expect("replacement remnant rewrites");
    set_private_permissions(temporary.as_path());
    assert_eq!(
        load_checkpoint(path.as_path(), MAX_BYTES)
            .expect("a torn replacement temporary leaves the prior final readable"),
        initial
    );
    let final_checkpoint = document(&processor("signal-a"), 1, 1);
    writer
        .finish(final_checkpoint.clone())
        .expect("atomic replacement commits");
    assert_eq!(
        load_checkpoint(path.as_path(), MAX_BYTES).expect("replacement checkpoint loads"),
        final_checkpoint
    );
}

#[test]
fn checkpoint_load_enforces_the_byte_limit_before_parsing() {
    let path = TestPath::new("checkpoint-load-size-limit", "json");
    let checkpoint = document(&processor("signal-a"), 0, 0);
    CheckpointWriter::create(path.as_path(), checkpoint, MAX_BYTES, 1, false)
        .expect("checkpoint creates")
        .abort()
        .expect("writer stops");
    let size = std::fs::metadata(path.as_path())
        .expect("checkpoint metadata reads")
        .len();
    assert!(matches!(
        load_checkpoint(path.as_path(), size - 1),
        Err(CheckpointError::SizeLimit {
            attempted,
            maximum
        }) if attempted == size && maximum == size - 1
    ));
}

#[test]
fn checkpoint_writer_persists_the_final_snapshot_under_sustained_submission() {
    let path = TestPath::new("checkpoint-writer-load", "json");
    let processor = processor("signal-a");
    let writer = CheckpointWriter::create(
        path.as_path(),
        document(&processor, 0, 0),
        MAX_BYTES,
        64,
        false,
    )
    .expect("checkpoint writer starts");
    for sequence in 1..=32 {
        writer
            .enqueue(document(&processor, sequence, sequence))
            .expect("bounded writer accepts sustained snapshots");
    }
    let final_checkpoint = document(&processor, 33, 33);
    let report = writer
        .finish(final_checkpoint.clone())
        .expect("final snapshot drains and persists");
    assert_eq!(report.checkpoints_written, 34);
    assert_eq!(report.last_checkpoint_time_ns, 33);
    assert_eq!(
        load_checkpoint(path.as_path(), MAX_BYTES).expect("final checkpoint loads"),
        final_checkpoint
    );
}

#[test]
fn checkpoint_rejects_corruption_truncation_and_contract_drift() {
    let corrupt = TestPath::new("checkpoint-corrupt", "json");
    let checkpoint = document(&processor("signal-a"), 0, 0);
    CheckpointWriter::create(corrupt.as_path(), checkpoint.clone(), MAX_BYTES, 1, false)
        .expect("checkpoint creates")
        .abort()
        .expect("writer stops");
    let mut envelope: Value =
        serde_json::from_slice(&std::fs::read(corrupt.as_path()).expect("checkpoint bytes read"))
            .expect("envelope parses");
    envelope["payload"]["checkpoint_time_ns"] = json!(1);
    std::fs::write(
        corrupt.as_path(),
        serde_json::to_vec(&envelope).expect("mutated envelope serializes"),
    )
    .expect("test corruption writes");
    assert!(matches!(
        load_checkpoint(corrupt.as_path(), MAX_BYTES),
        Err(CheckpointError::HashMismatch { .. })
    ));

    let truncated = TestPath::new("checkpoint-truncated", "json");
    std::fs::write(truncated.as_path(), b"{\"payload\":").expect("truncated checkpoint writes");
    set_private_permissions(truncated.as_path());
    assert!(matches!(
        load_checkpoint(truncated.as_path(), MAX_BYTES),
        Err(CheckpointError::Json(_))
    ));

    let mut drifted = checkpoint;
    drifted.contract_fingerprint.aggregate_sha256 = "0".repeat(64);
    assert!(matches!(
        drifted.validate(),
        Err(CheckpointError::FingerprintMismatch { .. })
    ));
}

#[cfg(unix)]
#[test]
fn checkpoint_rejects_symlinks_and_group_readable_files() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let target = TestPath::new("checkpoint-target", "json");
    let checkpoint = document(&processor("signal-a"), 0, 0);
    CheckpointWriter::create(target.as_path(), checkpoint, MAX_BYTES, 1, false)
        .expect("target creates")
        .abort()
        .expect("writer stops");

    let link = TestPath::absent("checkpoint-link", "json");
    symlink(target.as_path(), link.as_path()).expect("test symlink creates");
    assert!(matches!(
        load_checkpoint(link.as_path(), MAX_BYTES),
        Err(CheckpointError::UnsafeTarget(_))
    ));

    std::fs::set_permissions(target.as_path(), std::fs::Permissions::from_mode(0o640))
        .expect("permissions change");
    assert!(matches!(
        load_checkpoint(target.as_path(), MAX_BYTES),
        Err(CheckpointError::UnsafeTarget(_))
    ));
}

#[test]
fn every_crash_boundary_preserves_spot_reference_semantics() {
    let inputs = [
        Input::Market(coinbase_event(0, "coinbase-a"), 10, 10),
        Input::Market(kraken_event(0, "kraken-a"), 20, 20),
        Input::Health(1_000_000_020),
        Input::Unavailable(
            Venue::Kraken,
            UnavailableReason::Disconnected,
            1_000_000_030,
        ),
        Input::Market(kraken_event(0, "kraken-b"), 1_000_000_040, 1_000_000_040),
        Input::Health(2_000_000_040),
    ];
    let mut uninterrupted = processor("signal-a");
    let expected: Vec<Vec<SignalUpdate>> = inputs
        .iter()
        .map(|input| apply(&mut uninterrupted, input))
        .collect();

    for split in 0..=inputs.len() {
        let mut before_crash = processor("signal-a");
        for input in &inputs[..split] {
            apply(&mut before_crash, input);
        }
        let checkpoint_state = before_crash.checkpoint_state();
        let checkpoint_time = inputs[..split].last().map_or(0, Input::now_ns);
        let mut resumed = SpotSignalProcessor::restore(
            format!("signal-resumed-{split}"),
            wall_start(),
            checkpoint_state,
        )
        .expect("checkpoint restores under a new epoch");

        for (offset, input) in inputs[split..].iter().enumerate() {
            let actual = apply(&mut resumed, input);
            assert_eq!(
                reference_semantics(&actual),
                reference_semantics(&expected[split + offset]),
                "semantic output differs after crash boundary {split}"
            );
        }

        let mut actual_state =
            serde_json::to_value(resumed.checkpoint_state()).expect("resumed state serializes");
        let mut expected_state = serde_json::to_value(uninterrupted.checkpoint_state())
            .expect("uninterrupted state serializes");
        for state in [&mut actual_state, &mut expected_state] {
            let reference = state
                .get_mut("reference")
                .and_then(Value::as_object_mut)
                .expect("checkpoint reference state is an object");
            reference.remove("publisher_epoch_lineage");
            reference.remove("previous_next_sequence");
            state
                .as_object_mut()
                .expect("checkpoint state is an object")
                .remove("features");
        }
        assert_eq!(
            actual_state, expected_state,
            "processor state differs after crash boundary {split} at {checkpoint_time}ns"
        );
    }
}

#[test]
fn restore_rotates_identity_and_startup_unavailability_fails_closed() {
    let mut original = processor("signal-a");
    apply(
        &mut original,
        &Input::Market(coinbase_event(0, "coinbase-a"), 10, 10),
    );
    let ready = apply(
        &mut original,
        &Input::Market(kraken_event(0, "kraken-a"), 20, 20),
    )
    .into_iter()
    .find(|update| update.signal_id == btc_signals::SPOT_REFERENCE_ID)
    .expect("two venues produce a ready update");
    assert!(ready.values.contains_key("reference_price"));

    assert!(
        SpotSignalProcessor::restore("signal-a", wall_start(), original.checkpoint_state())
            .is_err()
    );
    let mut resumed =
        SpotSignalProcessor::restore("signal-b", wall_start(), original.checkpoint_state())
            .expect("new epoch restores");
    let second_generation = resumed.checkpoint_state();
    assert_eq!(
        second_generation.reference.publisher_epoch_lineage,
        ["signal-a", "signal-b"]
    );
    assert!(
        SpotSignalProcessor::restore("signal-a", wall_start(), second_generation.clone()).is_err()
    );
    assert!(SpotSignalProcessor::restore("signal-b", wall_start(), second_generation).is_err());
    document(&resumed, 20, 0);
    let degraded = resumed
        .mark_unavailable(Venue::Coinbase, UnavailableReason::Disconnected, 30)
        .expect("startup unavailability applies")
        .into_iter()
        .find(|update| update.signal_id == btc_signals::SPOT_REFERENCE_ID)
        .expect("reference quality transition emits");
    assert_eq!(degraded.publisher_epoch, "signal-b");
    assert_eq!(degraded.sequence, 0);
    assert!(!degraded.values.contains_key("reference_price"));
    assert_eq!(degraded.quality.reasons, ["coinbase_disconnected"]);
}

#[derive(Clone)]
enum Input {
    Market(SourceEvent, u64, u64),
    Unavailable(Venue, UnavailableReason, u64),
    Health(u64),
}

impl Input {
    fn now_ns(&self) -> u64 {
        match self {
            Self::Market(_, _, now_ns) | Self::Unavailable(_, _, now_ns) | Self::Health(now_ns) => {
                *now_ns
            }
        }
    }
}

fn apply(processor: &mut SpotSignalProcessor, input: &Input) -> Vec<SignalUpdate> {
    match input {
        Input::Market(event, receipt, now) => processor.ingest(event, *receipt, *now),
        Input::Unavailable(venue, reason, now) => processor.mark_unavailable(*venue, *reason, *now),
        Input::Health(now) => processor.advance_time(*now),
    }
    .expect("test input applies")
}

fn semantic_update(update: &SignalUpdate) -> Value {
    json!({
        "event_time": update.event_time,
        "produced_at": update.produced_at,
        "quality": update.quality,
        "values": update.values,
    })
}

fn reference_semantics(updates: &[SignalUpdate]) -> Option<Value> {
    updates
        .iter()
        .find(|update| update.signal_id == btc_signals::SPOT_REFERENCE_ID)
        .map(semantic_update)
}

fn set_private_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .expect("private permissions set");
    }
}

/// Audit A1/F4: a settlement-window pause survives a **real** restart.
///
/// The pause is recovery state, not a derived value: while a quarter's prints
/// are unrepaired its settlement window must be withheld, and a process that
/// restarted mid-recovery with an empty processor would admit the next quarter
/// print and publish an average over a window with lost prints. This writes the
/// checkpoint, loads it from disk, restores a processor from it, builds a new
/// pipeline, and checks the new pipeline still withholds.
#[test]
fn settlement_window_pause_survives_a_real_restart() {
    use btc_signals::prediction_checkpoint::{
        PREDICTION_CHECKPOINT_SCHEMA_VERSION, load_prediction_checkpoint,
    };
    use btc_signals::prediction_journal::{
        DEFAULT_PREDICTION_JOURNAL_MAX_BYTES, PredictionJournalInput, PredictionJournalSession,
        PredictionJournalWriter, PredictionRuntimeConfig,
    };
    use btc_signals::prediction_runtime::{PredictionCheckpointContext, PredictionPipeline};
    use btc_signals::{PredictionSignalProcessor, SettlementWindowPause};
    use signals_core::PredictionMarketVenue;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    const EPOCH: &str = "a1-settlement-pause-epoch";
    const RESTART_EPOCH: &str = "a1-settlement-pause-epoch-2";
    const WALL_START: &str = "2026-09-03T06:00:00Z";
    /// The withheld quarter — named by its DERIVED close (ruling E28: the
    /// window start plus 60 s, landing on a `:00/:15/:30/:45` boundary) — and
    /// the interval a repair must cover.
    const QUARTER_CLOSE_MS: i64 = 1_788_001_200_000;
    const QUARTER_CLOSE_TEXT: &str = "2026-08-29T11:00:00.000Z";

    let checkpoint_path = TestPath::absent("prediction-settlement-pause", "json");
    let journal_path = TestPath::absent("prediction-settlement-pause", "journal.jsonl");

    let session = PredictionJournalSession::new(
        WALL_START.to_owned(),
        EPOCH.to_owned(),
        PredictionRuntimeConfig::new(PredictionMarketVenue::Kalshi, 2_000),
    )
    .expect("session builds");
    let checkpoint_context = PredictionCheckpointContext {
        path: checkpoint_path.as_path().to_path_buf(),
        maximum_bytes: MAX_BYTES,
        session_id: session.session_id.clone(),
        wall_start: session.wall_start.clone(),
        contract_fingerprint: session.contract_fingerprint.clone(),
        runtime_config: session.runtime_config.clone(),
        publisher_epoch_lineage: vec![EPOCH.to_owned()],
    };
    let pause = SettlementWindowPause {
        index_id: "BRTI".to_owned(),
        quarter_close_time: QUARTER_CLOSE_MS,
        after_index_time: 1_788_000_300_000,
        through_index_time: 1_788_001_195_000,
    };

    // --- Before the restart: pause, then checkpoint.
    let mut before = PredictionPipeline::live(
        Vec::new(),
        PredictionSignalProcessor::new(
            PredictionMarketVenue::Kalshi,
            EPOCH,
            WALL_START.parse().expect("wall start parses"),
        )
        .expect("processor builds"),
        &session,
        PredictionJournalWriter::create(
            journal_path.as_path(),
            session.clone(),
            DEFAULT_PREDICTION_JOURNAL_MAX_BYTES,
        )
        .expect("journal creates"),
        checkpoint_context.clone(),
        1_000,
        1_000_000,
    )
    .expect("live pipeline builds");
    before
        .pause_settlement_window(pause.clone())
        .expect("the pause is accepted");
    before
        .checkpoint_live(1_000_000_000)
        .expect("the checkpoint writes");

    // --- The restart: load the persisted checkpoint from disk and restore.
    let document = load_prediction_checkpoint(checkpoint_path.as_path(), MAX_BYTES)
        .expect("the checkpoint loads");
    assert_eq!(
        document.checkpoint_schema_version,
        PREDICTION_CHECKPOINT_SCHEMA_VERSION
    );
    assert_eq!(
        document.processor_state.settlement_window_pause.as_ref(),
        Some(&pause),
        "the pause is part of the persisted processor state"
    );
    // A restart takes a fresh publisher epoch; the checkpoint carries the prior
    // one in its lineage.
    let restored = PredictionSignalProcessor::restore(
        RESTART_EPOCH,
        WALL_START.parse().expect("wall start parses"),
        document.processor_state.clone(),
    )
    .expect("the processor restores");
    let restart_session = PredictionJournalSession::new(
        WALL_START.to_owned(),
        RESTART_EPOCH.to_owned(),
        PredictionRuntimeConfig::new(PredictionMarketVenue::Kalshi, 2_000),
    )
    .expect("restart session builds");
    let mut after =
        PredictionPipeline::replay(Vec::new(), restored, &restart_session, 1_000, 1_000_000)
            .expect("the restarted pipeline builds");
    assert_eq!(
        after.settlement_window_pause(),
        Some(&pause),
        "the restart resumes still paused"
    );

    // --- The restarted pipeline withholds the quarter it was paused on.
    let mut values = BTreeMap::new();
    values.insert("index_id".to_owned(), json!("BRTI"));
    values.insert("event_time".to_owned(), json!(QUARTER_CLOSE_TEXT));
    values.insert("quarter_close_time".to_owned(), json!(QUARTER_CLOSE_TEXT));
    values.insert("window_end_inclusive".to_owned(), json!(QUARTER_CLOSE_TEXT));
    values.insert("window_as_of".to_owned(), json!(QUARTER_CLOSE_TEXT));
    values.insert("window_average".to_owned(), json!("64248.50000000"));
    let settlement = SourceEvent {
        schema_version: 1,
        source_id: "kalshi.cfbenchmarks.brti.settlement-window".to_owned(),
        source_version: "1.0.0".to_owned(),
        publisher_epoch: RESTART_EPOCH.to_owned(),
        sequence: 0,
        event_time: QUARTER_CLOSE_TEXT.to_owned(),
        received_at: QUARTER_CLOSE_TEXT.to_owned(),
        values,
    };
    let generation = after
        .current_generation_for_auxiliary_source()
        .expect("an auxiliary source takes the current generation");
    after
        .admit_replay(PredictionJournalInput::MarketEvents {
            admission_seq: after.next_admission_seq(),
            generation,
            events: Arc::new(vec![settlement.clone()]),
            logical_receipt_ns: 2_000_000_000,
            now_ns: 2_000_000_000,
        })
        .expect("a withheld settlement window is dropped, not raised");
    assert_eq!(
        after.settlement_windows_withheld(),
        1,
        "the paused quarter published nothing"
    );

    // Proof the pause is what suppressed it: with the pause released the same
    // event reaches the processor, which does not yet consume that identity.
    after.resume_settlement_window();
    assert!(after.settlement_window_pause().is_none());
    assert!(
        after
            .admit_replay(PredictionJournalInput::MarketEvents {
                admission_seq: after.next_admission_seq(),
                generation,
                events: Arc::new(vec![settlement]),
                logical_receipt_ns: 3_000_000_000,
                now_ns: 3_000_000_000,
            })
            .is_err(),
        "an unpaused pipeline hands the event to the processor"
    );
    assert_eq!(after.settlement_windows_withheld(), 1);
}

/// Audit A1/F4: a withheld settlement window is NOT written to the journal, so a
/// replay of that journal reproduces the live output instead of publishing the
/// window the live lane suppressed. The pause is out-of-band state the journal
/// never carries; filtering the event before it is journaled is what keeps
/// replay byte-identical to live.
#[test]
fn a_withheld_settlement_window_is_absent_from_the_journal_and_replay() {
    use btc_signals::prediction_journal::{
        DEFAULT_PREDICTION_JOURNAL_MAX_BYTES, PredictionJournalInput,
        PredictionJournalReplayReader, PredictionJournalSession, PredictionJournalWriter,
        PredictionRuntimeConfig,
    };
    use btc_signals::prediction_runtime::{PredictionCheckpointContext, PredictionPipeline};
    use btc_signals::{PredictionSignalProcessor, SettlementWindowPause};
    use signals_core::PredictionMarketVenue;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    const EPOCH: &str = "a1-journal-consistency-epoch";
    const WALL_START: &str = "2026-09-03T06:00:00Z";
    const QUARTER_CLOSE_MS: i64 = 1_788_001_200_000;
    const QUARTER_CLOSE_TEXT: &str = "2026-08-29T11:00:00.000Z";

    let checkpoint_path = TestPath::absent("prediction-journal-consistency", "json");
    let journal_path = TestPath::absent("prediction-journal-consistency", "journal.jsonl");

    let session = PredictionJournalSession::new(
        WALL_START.to_owned(),
        EPOCH.to_owned(),
        PredictionRuntimeConfig::new(PredictionMarketVenue::Kalshi, 2_000),
    )
    .expect("session builds");
    let checkpoint_context = PredictionCheckpointContext {
        path: checkpoint_path.as_path().to_path_buf(),
        maximum_bytes: MAX_BYTES,
        session_id: session.session_id.clone(),
        wall_start: session.wall_start.clone(),
        contract_fingerprint: session.contract_fingerprint.clone(),
        runtime_config: session.runtime_config.clone(),
        publisher_epoch_lineage: vec![EPOCH.to_owned()],
    };
    let mut live = PredictionPipeline::live(
        Vec::new(),
        PredictionSignalProcessor::new(
            PredictionMarketVenue::Kalshi,
            EPOCH,
            WALL_START.parse().expect("wall start parses"),
        )
        .expect("processor builds"),
        &session,
        PredictionJournalWriter::create(
            journal_path.as_path(),
            session.clone(),
            DEFAULT_PREDICTION_JOURNAL_MAX_BYTES,
        )
        .expect("journal creates"),
        checkpoint_context,
        1_000,
        1_000_000,
    )
    .expect("live pipeline builds");

    // The live lane is paused on the outstanding quarter.
    live.pause_settlement_window(SettlementWindowPause {
        index_id: "BRTI".to_owned(),
        quarter_close_time: QUARTER_CLOSE_MS,
        after_index_time: 1_788_000_300_000,
        through_index_time: 1_788_001_195_000,
    })
    .expect("the pause is accepted");

    let mut values = BTreeMap::new();
    values.insert("index_id".to_owned(), json!("BRTI"));
    values.insert("event_time".to_owned(), json!(QUARTER_CLOSE_TEXT));
    values.insert("quarter_close_time".to_owned(), json!(QUARTER_CLOSE_TEXT));
    values.insert("window_end_inclusive".to_owned(), json!(QUARTER_CLOSE_TEXT));
    values.insert("window_as_of".to_owned(), json!(QUARTER_CLOSE_TEXT));
    values.insert("window_average".to_owned(), json!("64248.50000000"));
    let settlement = SourceEvent {
        schema_version: 1,
        source_id: "kalshi.cfbenchmarks.brti.settlement-window".to_owned(),
        source_version: "1.0.0".to_owned(),
        publisher_epoch: EPOCH.to_owned(),
        sequence: 0,
        event_time: QUARTER_CLOSE_TEXT.to_owned(),
        received_at: QUARTER_CLOSE_TEXT.to_owned(),
        values,
    };
    let generation = live
        .current_generation_for_auxiliary_source()
        .expect("an auxiliary source takes the current generation");
    live.admit_live(PredictionJournalInput::MarketEvents {
        admission_seq: live.next_admission_seq(),
        generation,
        events: Arc::new(vec![settlement]),
        logical_receipt_ns: 2_000_000_000,
        now_ns: 2_000_000_000,
    })
    .expect("a withheld settlement window is admitted without publishing it");
    assert_eq!(
        live.settlement_windows_withheld(),
        1,
        "the paused quarter published nothing live"
    );
    live.finish_live(3_000_000_000)
        .expect("the live journal finishes");

    // The journal carries no settlement event at all: this batch held only the
    // withheld window, so it was skipped entirely rather than journaled empty.
    let mut reader = PredictionJournalReplayReader::open(journal_path.as_path(), MAX_BYTES)
        .expect("the journal reopens");
    let mut journaled_settlement_events = 0usize;
    let mut replay = PredictionPipeline::replay(
        Vec::new(),
        PredictionSignalProcessor::new(
            PredictionMarketVenue::Kalshi,
            EPOCH,
            WALL_START.parse().expect("wall start parses"),
        )
        .expect("replay processor builds"),
        reader.session(),
        1_000,
        1_000_000,
    )
    .expect("replay pipeline builds");
    while let Some(input) = reader.next_input().expect("journal input reads") {
        if let PredictionJournalInput::MarketEvents { events, .. } = &input {
            journaled_settlement_events += events
                .iter()
                .filter(|event| event.source_id == "kalshi.cfbenchmarks.brti.settlement-window")
                .count();
        }
        replay
            .admit_replay(input)
            .expect("every journaled input replays without error");
    }
    assert_eq!(
        journaled_settlement_events, 0,
        "the withheld settlement event is absent from the journal"
    );
    assert_eq!(
        replay.settlement_windows_withheld(),
        0,
        "replay has no pause and withholds nothing — the journal already excluded it"
    );
}
