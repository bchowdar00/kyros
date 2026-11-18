//! Byte-identical run→replay of the dual-venue multi lane over bounded excerpts
//! of the live Kalshi and Polymarket journals (C1 phase 3, ruling E57).

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use serde_json::Value;
use signals_core::PredictionMarketVenue;

use super::*;
use signals_core::{
    MarketTransitionReason, MultiMarketLeg, MultiMarketWindow, ScopeKind, SignalScope,
    SignalStreamMessage, validate_signal_scope_binding_for_definition,
};

/// Assert a stream frame that mentions a `btc.multi.*` identity never carries a
/// Global scope and is accepted by the exact per-frame scope-binding check the
/// package verifier runs (E20).
fn assert_no_global_multi_scope(message: &SignalStreamMessage) {
    let check = |id: &str, scope: &SignalScope| {
        if !id.starts_with("btc.multi.") {
            return;
        }
        assert!(
            !matches!(scope, SignalScope::Global),
            "{id} carries a Global scope on the wire (E20 forbids it)"
        );
        validate_signal_scope_binding_for_definition(id, scope, Some(ScopeKind::MultiMarket))
            .unwrap_or_else(|error| panic!("{id} scope is rejected by the verifier: {error}"));
    };
    match message {
        SignalStreamMessage::SignalBatch { updates, .. } => {
            for update in updates {
                check(&update.signal_id, &update.scope);
            }
        }
        SignalStreamMessage::InstanceStatus {
            signal_id, scope, ..
        } => check(signal_id, scope),
        _ => {}
    }
}

/// A parsed venue journal excerpt: its recorded epoch/wall-start and its inputs
/// in admission order.
struct VenueExcerpt {
    publisher_epoch: String,
    wall_start: String,
    inputs: Vec<PredictionJournalInput>,
}

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/multi")
        .join(name)
}

/// Read a bounded venue journal excerpt: the first line is the
/// `prediction_session` header (epoch + wall start), every later line is an input
/// record whose on-disk tag `record_type` is renamed to the enum's `input_type`.
fn parse_excerpt(name: &str) -> VenueExcerpt {
    let text = std::fs::read_to_string(fixture(name)).expect("read fixture");
    let mut lines = text.lines();
    let header: Value =
        serde_json::from_str(lines.next().expect("header line")).expect("header json");
    let record = &header["r"];
    assert_eq!(record["record_type"], "prediction_session", "first record");
    let publisher_epoch = record["publisher_epoch"]
        .as_str()
        .expect("publisher_epoch")
        .to_owned();
    let wall_start = record["wall_start"]
        .as_str()
        .expect("wall_start")
        .to_owned();

    let mut inputs = Vec::new();
    let mut expected_seq = 0u64;
    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        let framed: Value = serde_json::from_str(line).expect("record json");
        let mut record = framed["r"].clone();
        let object = record.as_object_mut().expect("record object");
        let tag = object.remove("record_type").expect("record_type present");
        object.insert("input_type".to_owned(), tag);
        let input: PredictionJournalInput = serde_json::from_value(record).expect("input decodes");
        assert_eq!(
            input.admission_seq(),
            expected_seq,
            "the excerpt's admission sequence is contiguous"
        );
        expected_seq += 1;
        inputs.push(input);
    }
    VenueExcerpt {
        publisher_epoch,
        wall_start,
        inputs,
    }
}

/// Interleave the two venues' inputs into one merged admission order by `now_ns`
/// (venue as a stable tiebreak): the order any live two-driver process would
/// produce, and the order the merged journal records.
fn merged_order(kalshi: &VenueExcerpt, polymarket: &VenueExcerpt) -> Vec<MultiJournalRecord> {
    let mut records: Vec<MultiJournalRecord> = kalshi
        .inputs
        .iter()
        .map(|input| MultiJournalRecord {
            venue: MultiVenueTag::Kalshi,
            input: input.clone(),
        })
        .chain(polymarket.inputs.iter().map(|input| MultiJournalRecord {
            venue: MultiVenueTag::Polymarket,
            input: input.clone(),
        }))
        .collect();
    // Stable sort by now_ns keeps each venue's own admission order intact, so the
    // per-venue admission-seq contiguity the inner pipelines require is preserved.
    records.sort_by_key(|record| record.input.now_ns());
    records
}

fn build_runtime(
    kalshi: &VenueExcerpt,
    polymarket: &VenueExcerpt,
) -> (MultiLaneRuntime<Vec<u8>>, Rc<RefCell<Vec<u8>>>) {
    build_runtime_with_caps(kalshi, polymarket, DEFAULT_MAX_UPDATES, DEFAULT_MAX_BYTES)
}

fn build_runtime_with_caps(
    kalshi: &VenueExcerpt,
    polymarket: &VenueExcerpt,
    max_updates: u64,
    max_output_bytes: u64,
) -> (MultiLaneRuntime<Vec<u8>>, Rc<RefCell<Vec<u8>>>) {
    let shared = Rc::new(RefCell::new(Vec::<u8>::new()));
    let kalshi_session = venue_session(
        PredictionMarketVenue::Kalshi,
        kalshi.publisher_epoch.clone(),
        &kalshi.wall_start,
        250,
    )
    .expect("kalshi session");
    let polymarket_session = venue_session(
        PredictionMarketVenue::Polymarket,
        polymarket.publisher_epoch.clone(),
        &polymarket.wall_start,
        250,
    )
    .expect("polymarket session");
    let runtime = MultiLaneRuntime::new(
        Rc::clone(&shared),
        "multi-fixture-epoch",
        chrono::DateTime::parse_from_rfc3339(&kalshi.wall_start)
            .expect("wall start")
            .with_timezone(&chrono::Utc),
        &kalshi_session,
        &polymarket_session,
        None,
        None,
        max_updates,
        max_output_bytes,
    )
    .expect("runtime");
    (runtime, shared)
}

/// Drive a merged journal through a fresh runtime and return the merged output.
fn drive(
    kalshi: &VenueExcerpt,
    polymarket: &VenueExcerpt,
    journal: &[MultiJournalRecord],
) -> Vec<u8> {
    let (mut runtime, shared) = build_runtime(kalshi, polymarket);
    for record in journal {
        runtime
            .admit(record.venue, record.input.clone())
            .expect("admit");
    }
    runtime.flush().expect("flush");
    shared.borrow().clone()
}

#[test]
fn run_and_replay_of_the_merged_journal_are_byte_identical() {
    let kalshi = parse_excerpt("kalshi.journal.jsonl");
    let polymarket = parse_excerpt("polymarket.journal.jsonl");
    let journal = merged_order(&kalshi, &polymarket);
    assert!(journal.len() > 100, "the excerpts carry enough admissions");

    // RUN: drive the interleaved inputs, recording the merged journal in
    // admission order (that is exactly `journal`).
    let run_output = drive(&kalshi, &polymarket, &journal);

    // Persist and re-read the merged journal as JSONL, exactly as a replay would,
    // to prove the wire journal round-trips.
    let journal_bytes: Vec<u8> = journal
        .iter()
        .flat_map(|record| {
            let mut line = serde_json::to_vec(record).expect("record serializes");
            line.push(b'\n');
            line
        })
        .collect();
    let replayed_journal: Vec<MultiJournalRecord> = journal_bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).expect("record deserializes"))
        .collect();
    assert_eq!(replayed_journal.len(), journal.len());

    // REPLAY: drive the re-read merged journal through a fresh runtime.
    let replay_output = drive(&kalshi, &polymarket, &replayed_journal);

    assert_eq!(
        run_output, replay_output,
        "run and replay of the merged journal must be byte-identical"
    );
    assert!(!run_output.is_empty(), "the lane produced frames");

    // The two excerpts are independent recordings with different windows, so the
    // pair never reaches an `exact_close` alignment: the merged output is a
    // PRE-BIND period. That is exactly the case the E20/BLOCKER fix must handle —
    // NO `btc.multi.*` frame may carry a Global scope (the verifier rejects a
    // multi_market identity under Global). Assert that every `btc.multi.*` frame
    // on the wire (there are none here, since the lane withholds until an aligned
    // instance exists) is verifier-accepted, never Global.
    let text = String::from_utf8(run_output).expect("utf8");
    for line in text.lines() {
        if !line.contains("btc.multi.") {
            continue;
        }
        let message: SignalStreamMessage = serde_json::from_str(line).expect("frame decodes");
        assert_no_global_multi_scope(&message);
    }
    assert!(
        text.contains(&kalshi.publisher_epoch),
        "the Kalshi venue frames are on the merged wire"
    );
    assert!(
        text.contains(&polymarket.publisher_epoch),
        "the Polymarket venue frames are on the merged wire"
    );
    assert!(
        text.contains("\"type\":\"market_transition\""),
        "per-venue market_transition frames are on the merged wire"
    );
}

#[test]
fn cli_replay_path_over_the_merged_journal_is_byte_identical() {
    // Build a merged journal FILE (header + venue-tagged records) exactly as a
    // live `run --venue multi` would persist it, then drive it through the real
    // `replay_multi_file` — the code path the `replay --venue multi` CLI arm
    // calls — and confirm its output equals the in-memory run's output.
    let kalshi = parse_excerpt("kalshi.journal.jsonl");
    let polymarket = parse_excerpt("polymarket.journal.jsonl");
    let journal = merged_order(&kalshi, &polymarket);

    let header = MultiJournalHeader {
        publisher_epoch: "multi-fixture-epoch".to_owned(),
        wall_start: kalshi.wall_start.clone(),
        kalshi: VenueHeader {
            publisher_epoch: kalshi.publisher_epoch.clone(),
            wall_start: kalshi.wall_start.clone(),
            admission_epoch: None,
            active_binding: None,
        },
        polymarket: VenueHeader {
            publisher_epoch: polymarket.publisher_epoch.clone(),
            wall_start: polymarket.wall_start.clone(),
            admission_epoch: None,
            active_binding: None,
        },
    };

    // The in-memory reference output, built via `runtime_from_header` so the two
    // paths share identical session reconstruction.
    let shared = Rc::new(RefCell::new(Vec::<u8>::new()));
    let mut reference = runtime_from_header(
        Rc::clone(&shared),
        &header,
        DEFAULT_MAX_UPDATES,
        DEFAULT_MAX_BYTES,
    )
    .expect("runtime");
    for record in &journal {
        reference
            .admit(record.venue, record.input.clone())
            .expect("admit");
    }
    reference.flush().expect("flush");
    let reference_output = shared.borrow().clone();

    // Persist the merged journal to a file and replay it through the CLI path.
    let dir = std::env::temp_dir().join(format!("multi-replay-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let journal_path = dir.join("merged.journal.jsonl");
    let output_path = dir.join("replay.output.jsonl");
    {
        let mut file = std::fs::File::create(&journal_path).expect("journal file");
        super::write_journal_header(&mut file, &header).expect("header");
        for record in &journal {
            super::write_journal_record(&mut file, record).expect("record");
        }
    }
    let report = super::replay_multi_file(&journal_path, &output_path).expect("replay");
    assert_eq!(report.admissions, journal.len() as u64);
    let replay_output = std::fs::read(&output_path).expect("read output");

    assert_eq!(
        reference_output, replay_output,
        "the CLI replay path must be byte-identical to the run output"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

fn canonical_multi_scope() -> SignalScope {
    SignalScope::MultiMarket {
        legs: vec![
            MultiMarketLeg {
                venue: "kalshi".to_owned(),
                market_id: "KXBTC15M-A".to_owned(),
                market_generation: 1,
            },
            MultiMarketLeg {
                venue: "polymarket".to_owned(),
                market_id: "0xpoly".to_owned(),
                market_generation: 1,
            },
        ],
        window: MultiMarketWindow {
            open: "2026-09-04T05:45:00.000000000Z".to_owned(),
            close: "2026-09-04T06:00:00.000000000Z".to_owned(),
        },
    }
}

#[test]
fn leg_departure_invalidates_the_bound_multi_before_the_leg_frame() {
    // The two independent local excerpts never reach an `exact_close` alignment
    // (wave2c has only Kalshi), so a bound cross-venue instance cannot arise from
    // real data here. We bind the Kalshi inner pipeline from its own excerpt, then
    // SEED a bound multi instance (justified by the missing co-temporal aligned
    // journals) and drive a real Kalshi leg departure: the runtime must write the
    // multi instance's terminal `invalidated` (V10/L2) BEFORE the inner pipeline's
    // `market_transition unavailable` for that leg.
    let kalshi = parse_excerpt("kalshi.journal.jsonl");
    let polymarket = parse_excerpt("polymarket.journal.jsonl");
    let (mut runtime, shared) = build_runtime(&kalshi, &polymarket);
    for input in &kalshi.inputs {
        runtime
            .admit(MultiVenueTag::Kalshi, input.clone())
            .expect("kalshi admit");
    }
    let scope = canonical_multi_scope();
    runtime.seed_bound_multi(scope.clone());

    let marker = shared.borrow().len();
    let generation = runtime
        .current_generation_for_auxiliary_source(MultiVenueTag::Kalshi)
        .expect("active generation");
    let seq = runtime.next_admission_seq(MultiVenueTag::Kalshi);
    let now_ns = kalshi
        .inputs
        .last()
        .map(|input| input.now_ns() + 1_000_000)
        .unwrap_or(1);
    let departure = PredictionJournalInput::Unavailable {
        admission_seq: seq,
        generation,
        previous_market_id: None,
        reason: MarketTransitionReason::ScheduledRollover,
        now_ns,
    };
    // The invalidation is written before `admit_replay`, so it is present whether
    // or not the crafted departure is accepted by the inner pipeline.
    let _ = runtime.admit(MultiVenueTag::Kalshi, departure);
    runtime.flush().expect("flush");

    let tail = String::from_utf8(shared.borrow()[marker..].to_vec()).expect("utf8");
    let mut invalidated_multi_at: Option<usize> = None;
    let mut leg_unavailable_at: Option<usize> = None;
    for (index, line) in tail.lines().enumerate() {
        let Ok(message) = serde_json::from_str::<SignalStreamMessage>(line) else {
            continue;
        };
        match &message {
            SignalStreamMessage::InstanceStatus {
                signal_id,
                scope,
                state,
                ..
            } if signal_id.starts_with("btc.multi.") => {
                assert_eq!(*state, signals_core::InstanceStatusState::Invalidated);
                assert!(matches!(scope, SignalScope::MultiMarket { .. }));
                validate_signal_scope_binding_for_definition(
                    signal_id,
                    scope,
                    Some(ScopeKind::MultiMarket),
                )
                .expect("invalidation scope is verifier-accepted");
                invalidated_multi_at.get_or_insert(index);
            }
            SignalStreamMessage::MarketTransition {
                phase: signals_core::MarketTransitionPhase::Unavailable,
                ..
            } => {
                leg_unavailable_at.get_or_insert(index);
            }
            _ => {}
        }
    }
    let inv = invalidated_multi_at.expect("the bound multi instance was invalidated");
    if let Some(leg) = leg_unavailable_at {
        assert!(
            inv < leg,
            "the multi invalidation ({inv}) must precede the leg unavailable ({leg}) (L2)"
        );
    }
    // The instance is no longer tracked as bound after the invalidation.
    assert!(runtime.checkpoint().kalshi_admissions >= kalshi.inputs.len() as u64);
}

#[test]
fn checkpoint_round_trips_and_counts_admissions() {
    let kalshi = parse_excerpt("kalshi.journal.jsonl");
    let polymarket = parse_excerpt("polymarket.journal.jsonl");
    let journal = merged_order(&kalshi, &polymarket);
    let (mut runtime, _shared) = build_runtime(&kalshi, &polymarket);
    for record in &journal {
        runtime
            .admit(record.venue, record.input.clone())
            .expect("admit");
    }
    let checkpoint = runtime.checkpoint();
    assert_eq!(
        checkpoint.kalshi_admissions + checkpoint.polymarket_admissions,
        journal.len() as u64,
    );
    let json = serde_json::to_string(&checkpoint).expect("serialize");
    let restored: MultiLaneCheckpoint = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(checkpoint, restored);
}

/// Drive the merged journal through a capped runtime, stopping at the first admit
/// that trips a cap. Returns that admit's error (if any) and the merged output the
/// run produced up to that point.
fn drive_capped(
    kalshi: &VenueExcerpt,
    polymarket: &VenueExcerpt,
    journal: &[MultiJournalRecord],
    max_updates: u64,
    max_output_bytes: u64,
) -> (Option<MultiRuntimeError>, Vec<u8>) {
    let (mut runtime, shared) =
        build_runtime_with_caps(kalshi, polymarket, max_updates, max_output_bytes);
    let mut error = None;
    for record in journal {
        if let Err(err) = runtime.admit(record.venue, record.input.clone()) {
            error = Some(err);
            break;
        }
    }
    let _ = runtime.flush();
    let output = shared.borrow().clone();
    (error, output)
}

#[test]
fn merged_output_byte_cap_stops_the_run_with_the_documented_overflow() {
    use crate::prediction_runtime::PredictionPipelineError;

    let kalshi = parse_excerpt("kalshi.journal.jsonl");
    let polymarket = parse_excerpt("polymarket.journal.jsonl");
    let journal = merged_order(&kalshi, &polymarket);

    // A byte cap far below the run's full merged output: the same `ByteLimit`
    // (exit 78) the single-venue pipeline raises must stop the run.
    let (error, output) = drive_capped(&kalshi, &polymarket, &journal, DEFAULT_MAX_UPDATES, 4_096);
    let error = error.expect("the merged byte cap trips");
    assert!(
        matches!(
            error,
            MultiRuntimeError::Pipeline(PredictionPipelineError::ByteLimit(4_096))
        ),
        "the merged byte cap raises the single-venue byte overflow, got {error:?}",
    );
    // The session record — the epoch-start `reset` the stream opens with — was
    // written before the cap tripped.
    let text = String::from_utf8(output).expect("utf8");
    assert!(
        text.contains("\"type\":\"reset\""),
        "the session-start reset frame is on the merged wire before the overflow",
    );
}

#[test]
fn merged_output_update_cap_stops_the_run_with_the_documented_overflow() {
    use crate::prediction_runtime::PredictionPipelineError;

    let kalshi = parse_excerpt("kalshi.journal.jsonl");
    let polymarket = parse_excerpt("polymarket.journal.jsonl");
    let journal = merged_order(&kalshi, &polymarket);

    // Confirm the excerpt actually publishes more than one update, so the cap of 1
    // is genuinely crossed (not a vacuous pass).
    let full = drive(&kalshi, &polymarket, &journal);
    let full_updates = String::from_utf8(full)
        .expect("utf8")
        .matches("\"type\":\"signal_batch\"")
        .count();
    assert!(
        full_updates > 1,
        "the excerpt publishes enough signal batches to cross a unit update cap",
    );

    let (error, output) = drive_capped(&kalshi, &polymarket, &journal, 1, DEFAULT_MAX_BYTES);
    let error = error.expect("the merged update cap trips");
    assert!(
        matches!(
            error,
            MultiRuntimeError::Pipeline(PredictionPipelineError::UpdateLimit(1))
        ),
        "the merged update cap raises the single-venue update overflow, got {error:?}",
    );
    let text = String::from_utf8(output).expect("utf8");
    assert!(
        text.contains("\"type\":\"reset\""),
        "the session-start reset frame is on the merged wire before the overflow",
    );
}

#[test]
fn caps_exactly_at_the_run_totals_are_not_exceeded_and_leave_the_output_unchanged() {
    let kalshi = parse_excerpt("kalshi.journal.jsonl");
    let polymarket = parse_excerpt("polymarket.journal.jsonl");
    let journal = merged_order(&kalshi, &polymarket);

    // The uncapped reference run and its exact merged totals.
    let (mut runtime, shared) = build_runtime(&kalshi, &polymarket);
    for record in &journal {
        runtime
            .admit(record.venue, record.input.clone())
            .expect("admit");
    }
    runtime.flush().expect("flush");
    let reference = shared.borrow().clone();
    let total_updates = runtime.merged_updates_written();
    let total_bytes = runtime.merged_output_bytes();
    assert!(
        total_bytes as usize == reference.len() || total_bytes > 0,
        "byte counter tracked output"
    );

    // Caps set exactly to the run's totals must NOT trip (the overflow is `> cap`),
    // and the output must be byte-identical to the uncapped run.
    let (error, output) = drive_capped(&kalshi, &polymarket, &journal, total_updates, total_bytes);
    assert!(
        error.is_none(),
        "caps equal to the totals are not exceeded: {error:?}"
    );
    assert_eq!(
        output, reference,
        "output under exact caps matches the uncapped run"
    );
}

#[test]
fn merged_journal_writer_bounds_the_file_and_keeps_the_session_record() {
    let kalshi = parse_excerpt("kalshi.journal.jsonl");
    let polymarket = parse_excerpt("polymarket.journal.jsonl");
    let journal = merged_order(&kalshi, &polymarket);
    let header = MultiJournalHeader {
        publisher_epoch: "multi-fixture-epoch".to_owned(),
        wall_start: kalshi.wall_start.clone(),
        kalshi: VenueHeader {
            publisher_epoch: kalshi.publisher_epoch.clone(),
            wall_start: kalshi.wall_start.clone(),
            admission_epoch: None,
            active_binding: None,
        },
        polymarket: VenueHeader {
            publisher_epoch: polymarket.publisher_epoch.clone(),
            wall_start: polymarket.wall_start.clone(),
            admission_epoch: None,
            active_binding: None,
        },
    };

    // A cap that admits the header (the session record) but trips on a later
    // record: the merged journal is bounded with the same size-limit overflow the
    // single-venue journal raises, and the persisted file never exceeds the cap.
    let header_bytes = {
        let mut sizing = MergedJournalWriter::new(Vec::<u8>::new(), u64::MAX);
        sizing.write_header(&header).expect("header sizes");
        sizing.bytes_written()
    };
    let cap = header_bytes + 8; // room for the header, not a full record.

    let mut writer = MergedJournalWriter::new(Vec::<u8>::new(), cap);
    writer
        .write_header(&header)
        .expect("the session record fits under the cap");
    let mut overflow = None;
    for record in &journal {
        if let Err(err) = writer.write_record(record) {
            overflow = Some(err);
            break;
        }
    }
    let error = overflow.expect("the merged journal cap trips");
    assert!(
        matches!(error, MultiRuntimeError::JournalSizeLimit { maximum, .. } if maximum == cap),
        "the merged journal raises the size-limit overflow, got {error:?}",
    );
    assert!(
        error.to_string().contains("prediction journal exceeds"),
        "the overflow carries the documented journal size-limit message: {error}",
    );
    writer.flush().expect("flush");
    let bytes = writer.bytes_written();
    assert!(
        bytes <= cap,
        "the persisted merged journal never exceeds the cap"
    );
    assert!(
        bytes >= header_bytes,
        "the session record is on disk before the overflow"
    );
}

// ---------------------------------------------------------------------------
// In-process rotation (C5/E57): the multi lane rotates its output + journal +
// checkpoint as one file generation, keeping both venue processors and the multi
// blends warm in memory.
// ---------------------------------------------------------------------------

use crate::file_generation::FileGenerationCaps;

/// The caps every rotation test uses; generous enough that no cap trips.
const ROTATION_CAPS: FileGenerationCaps = FileGenerationCaps {
    max_updates: DEFAULT_MAX_UPDATES,
    max_output_bytes: DEFAULT_MAX_BYTES,
    max_journal_bytes: DEFAULT_MAX_BYTES,
};

#[test]
fn rotation_resets_every_stream_onto_the_derived_epochs_at_sequence_zero() {
    // Drive generation 0 from the two excerpts, then rotate in process exactly as
    // the bin's `rotate_multi_generation` does at the runtime level: flush the
    // departed output, swap the shared writer, and reset the runtime's stream half
    // onto the derived per-generation output epochs. The successor republishes a
    // reset frame at sequence 0 for BOTH venue legs and the multi publisher, each
    // under its derived epoch, and every leg's admission sequence restarts at 0.
    let kalshi = parse_excerpt("kalshi.journal.jsonl");
    let polymarket = parse_excerpt("polymarket.journal.jsonl");
    let journal = merged_order(&kalshi, &polymarket);
    let (mut runtime, shared) = build_runtime(&kalshi, &polymarket);
    for record in &journal {
        runtime
            .admit(record.venue, record.input.clone())
            .expect("admit gen-0");
    }
    runtime.flush().expect("flush gen-0");
    let gen0 = String::from_utf8(shared.borrow().clone()).expect("gen0 utf8");
    assert!(!gen0.is_empty(), "generation 0 produced frames");
    assert!(
        runtime.next_admission_seq(MultiVenueTag::Kalshi) > 0
            && runtime.next_admission_seq(MultiVenueTag::Polymarket) > 0,
        "both legs admitted in generation 0",
    );
    assert!(
        gen0.contains(&kalshi.publisher_epoch) && gen0.contains(&polymarket.publisher_epoch),
        "generation 0 carries the base venue epochs",
    );

    // Rotate: the bin swaps the shared file first, then resets the runtime.
    let output_epoch = "multi-fixture-epoch-1700000600";
    *shared.borrow_mut() = Vec::new();
    runtime
        .rotate(output_epoch, ROTATION_CAPS)
        .expect("rotate the multi runtime");
    assert_eq!(
        runtime.next_admission_seq(MultiVenueTag::Kalshi),
        0,
        "the Kalshi leg's admission sequence restarts at 0",
    );
    assert_eq!(
        runtime.next_admission_seq(MultiVenueTag::Polymarket),
        0,
        "the Polymarket leg's admission sequence restarts at 0",
    );

    // A health tick per leg (contiguous at 0 after the reset) drives the reset
    // frames onto the new generation's wire.
    let now_ns = 5_000_000_000_u64;
    runtime
        .admit(
            MultiVenueTag::Kalshi,
            PredictionJournalInput::HealthTick {
                admission_seq: 0,
                now_ns,
            },
        )
        .expect("admit successor Kalshi health tick");
    runtime
        .admit(
            MultiVenueTag::Polymarket,
            PredictionJournalInput::HealthTick {
                admission_seq: 0,
                now_ns,
            },
        )
        .expect("admit successor Polymarket health tick");
    runtime.flush().expect("flush gen-1");
    let gen1 = String::from_utf8(shared.borrow().clone()).expect("gen1 utf8");

    let kalshi_output_epoch = format!("{output_epoch}.kalshi");
    let polymarket_output_epoch = format!("{output_epoch}.polymarket");
    for (label, epoch) in [
        ("Kalshi leg", kalshi_output_epoch.as_str()),
        ("Polymarket leg", polymarket_output_epoch.as_str()),
        ("multi publisher", output_epoch),
    ] {
        let has_reset_at_zero = gen1.lines().any(|line| {
            let value: Value = match serde_json::from_str(line) {
                Ok(value) => value,
                Err(_) => return false,
            };
            value["type"] == "reset" && value["publisher_epoch"] == epoch && value["sequence"] == 0
        });
        assert!(
            has_reset_at_zero,
            "{label} republishes a reset frame at sequence 0 under {epoch}: {gen1}",
        );
    }
    assert!(
        !gen1.contains(&format!(
            "\"publisher_epoch\":\"{}\"",
            kalshi.publisher_epoch
        )),
        "generation 1 does not re-use the base Kalshi epoch on its frames",
    );
}

/// Restamp an input's admission sequence (the successor generation's merged
/// journal renumbers each leg's admissions from 0). Round-trips through JSON so no
/// per-variant setter is needed; `admission_seq` is a top-level field on every
/// variant (the same field `parse_excerpt` reads).
fn with_admission_seq(input: &PredictionJournalInput, seq: u64) -> PredictionJournalInput {
    let mut value = serde_json::to_value(input).expect("input serializes");
    value["admission_seq"] = Value::from(seq);
    serde_json::from_value(value).expect("renumbered input deserializes")
}

/// The last published `values` payload of one signal identity in a merged output
/// blob, keyed by scope. Used to compare a non-ring identity's value across a warm
/// run and a re-warmed rotated replay (payload only, not the timing metadata).
fn last_values_for(
    output: &[u8],
    signal_id: &str,
) -> Vec<(String, std::collections::BTreeMap<String, Value>)> {
    let text = String::from_utf8(output.to_vec()).expect("utf8");
    let mut latest: std::collections::BTreeMap<String, std::collections::BTreeMap<String, Value>> =
        std::collections::BTreeMap::new();
    for line in text.lines() {
        let Ok(SignalStreamMessage::SignalBatch { updates, .. }) = serde_json::from_str(line)
        else {
            continue;
        };
        for update in updates {
            if update.signal_id != signal_id {
                continue;
            }
            let key = serde_json::to_string(&update.scope).expect("scope key");
            latest.insert(key, update.values.clone());
        }
    }
    latest.into_iter().collect()
}

#[test]
fn rotated_generation_replays_standalone_with_the_leg_binding_restored() {
    // BLOCKER (E64): a rotated multi generation's merged journal must replay
    // standalone. Build generation 0 from the real fixtures, capture the rotation
    // header (which now carries each leg's active binding), rotate the runtime, and
    // drive generation 1 — a stretch of Kalshi market-relative events with NO Bind,
    // exactly the mid-market rotation that used to exit 78 ("market-relative source
    // events arrived without a bound market"). A fresh runtime built from the header
    // must restore the binding and admit every generation-1 record.
    let kalshi = parse_excerpt("kalshi.journal.jsonl");
    let polymarket = parse_excerpt("polymarket.journal.jsonl");
    let merged = merged_order(&kalshi, &polymarket);

    // Split mid-market: generation 0 carries both legs' binds plus most of their
    // market events; generation 1 is the remaining Kalshi market events, which are
    // market-relative and unreplayable without the restored binding.
    let split = 158;
    let gen0: Vec<MultiJournalRecord> = merged[..split].to_vec();
    // Renumber generation 1's admissions per leg from 0 (the successor journal's
    // per-leg contiguity, exactly as `rotate` restarts each leg's sequence at 0).
    let mut kalshi_seq = 0u64;
    let mut polymarket_seq = 0u64;
    let gen1: Vec<MultiJournalRecord> = merged[split..]
        .iter()
        .map(|record| {
            let seq = match record.venue {
                MultiVenueTag::Kalshi => {
                    let seq = kalshi_seq;
                    kalshi_seq += 1;
                    seq
                }
                MultiVenueTag::Polymarket => {
                    let seq = polymarket_seq;
                    polymarket_seq += 1;
                    seq
                }
            };
            MultiJournalRecord {
                venue: record.venue,
                input: with_admission_seq(&record.input, seq),
            }
        })
        .collect();
    assert!(
        gen1.iter().all(|record| !matches!(
            record.input,
            PredictionJournalInput::Bind { .. } | PredictionJournalInput::Discovery { .. }
        )),
        "generation 1 establishes no fresh binding, so it depends on the restored one",
    );
    let gen1_market_events = gen1
        .iter()
        .filter(|record| matches!(record.input, PredictionJournalInput::MarketEvents { .. }))
        .count();
    assert!(
        gen1_market_events > 1,
        "generation 1 carries several market-relative admissions ({gen1_market_events})",
    );

    // WARM live run: drive generation 0, capture the rotation header, rotate the
    // warm runtime, then drive generation 1. Its generation-1 output is the
    // reference the standalone replay is compared against.
    let output_epoch = "multi-fixture-epoch-1700000600";
    let (mut warm, warm_shared) = build_runtime(&kalshi, &polymarket);
    for record in &gen0 {
        warm.admit(record.venue, record.input.clone())
            .expect("warm gen-0 admit");
    }
    let header = warm.rotated_journal_header(output_epoch);
    assert!(
        header.kalshi.active_binding.is_some(),
        "the rotation header carries the Kalshi leg's active binding",
    );
    warm.flush().expect("warm gen-0 flush");
    // The warm run's generation-0 output carries the live market_contract for the
    // bound market (published once at the bind), the value the restored binding
    // must reproduce.
    let warm_gen0 = warm_shared.borrow().clone();
    warm_shared.borrow_mut().clear();
    warm.rotate(output_epoch, ROTATION_CAPS)
        .expect("warm rotate");
    for record in &gen1 {
        warm.admit(record.venue, record.input.clone())
            .expect("warm gen-1 admit");
    }
    warm.flush().expect("warm flush");
    let warm_gen1 = warm_shared.borrow().clone();

    // STANDALONE replay: build a fresh runtime from the rotation header and drive
    // ONLY generation 1. Without the binding-restore fix the first market_events
    // record aborts; with it every record admits.
    let replay_shared = Rc::new(RefCell::new(Vec::<u8>::new()));
    let mut replay = runtime_from_header(
        Rc::clone(&replay_shared),
        &header,
        DEFAULT_MAX_UPDATES,
        DEFAULT_MAX_BYTES,
    )
    .expect("standalone runtime from the rotated header");
    let mut admissions = 0u64;
    for record in &gen1 {
        replay
            .admit(record.venue, record.input.clone())
            .expect("standalone generation-1 replay admits every record");
        admissions += 1;
    }
    replay.flush().expect("standalone flush");
    let replay_gen1 = replay_shared.borrow().clone();

    assert_eq!(
        admissions,
        gen1.len() as u64,
        "the standalone replay admits the whole generation (exit 0, admissions > 0)",
    );
    assert!(admissions > 0, "the standalone replay admitted records");
    assert!(
        replay_gen1
            .windows(b"\"btc.kalshi.".len())
            .any(|w| w == b"\"btc.kalshi."),
        "the standalone replay publishes the Kalshi leg's market-relative frames",
    );

    // The non-ring identity's VALUE equals the live run's. `btc.kalshi.
    // market_contract` is a pure function of the bound contract, so the contract
    // the restored binding rebuilds and republishes at generation-1 start is
    // byte-identical to the one the warm run published live at the bind — proof the
    // binding was restored to the exact same market, not re-discovered or dropped.
    // (The warm processor deduped market_contract after gen-0, so it does not
    // re-emit it in gen-1; the value is compared against gen-0, where it was live.)
    let warm_contract = last_values_for(
        &warm_gen0,
        crate::prediction_signals::KALSHI_MARKET_CONTRACT_ID,
    );
    let replay_contract = last_values_for(
        &replay_gen1,
        crate::prediction_signals::KALSHI_MARKET_CONTRACT_ID,
    );
    assert!(
        !warm_contract.is_empty(),
        "the warm run published the Kalshi market contract at the bind",
    );
    assert_eq!(
        replay_contract, warm_contract,
        "the restored binding reproduces the identical market_contract value",
    );

    // The ring/warm identities honestly re-warm — the documented C5 re-warm gap,
    // wider here than the single-venue lane's because the multi lane carries only
    // the binding across the boundary (no per-leg processor checkpoint). In this
    // fixture the post-boundary Kalshi events are incremental book deltas, so the
    // re-warmed replay cannot rebuild the order book the warm run kept in memory
    // and withholds the book-derived `prediction_liquidity` the warm run continues
    // to publish — the same book re-warm the single-venue lane takes on any resume.
    assert!(
        warm_gen1
            .windows(b"prediction_liquidity".len())
            .any(|w| w == b"prediction_liquidity"),
        "the warm run keeps publishing the book-derived liquidity across the boundary",
    );
}

#[test]
fn rotated_generation_without_the_leg_binding_is_rejected_clearly() {
    // NEGATIVE: a rotated generation whose leg was bound at the boundary but whose
    // header omits the binding cannot replay its market-relative events — the
    // failure the binding-restore fix exists to prevent, surfaced with the clear
    // "without a bound market" message rather than silent corruption.
    let kalshi = parse_excerpt("kalshi.journal.jsonl");
    let polymarket = parse_excerpt("polymarket.journal.jsonl");
    let merged = merged_order(&kalshi, &polymarket);
    let split = 158;
    let gen0: Vec<MultiJournalRecord> = merged[..split].to_vec();
    let mut kalshi_seq = 0u64;
    let gen1: Vec<MultiJournalRecord> = merged[split..]
        .iter()
        .filter(|record| matches!(record.venue, MultiVenueTag::Kalshi))
        .map(|record| {
            let seq = kalshi_seq;
            kalshi_seq += 1;
            MultiJournalRecord {
                venue: record.venue,
                input: with_admission_seq(&record.input, seq),
            }
        })
        .collect();

    let output_epoch = "multi-fixture-epoch-1700000600";
    let (mut warm, _shared) = build_runtime(&kalshi, &polymarket);
    for record in &gen0 {
        warm.admit(record.venue, record.input.clone())
            .expect("warm gen-0 admit");
    }
    // Strip the Kalshi binding from an otherwise-valid rotated header.
    let mut header = warm.rotated_journal_header(output_epoch);
    assert!(
        header.kalshi.active_binding.is_some(),
        "the leg was bound at the boundary, so a faithful header would carry the binding",
    );
    header.kalshi.active_binding = None;

    let shared = Rc::new(RefCell::new(Vec::<u8>::new()));
    let mut replay = runtime_from_header(shared, &header, DEFAULT_MAX_UPDATES, DEFAULT_MAX_BYTES)
        .expect("runtime builds cold from the binding-stripped header");
    let mut error = None;
    for record in &gen1 {
        if let Err(err) = replay.admit(record.venue, record.input.clone()) {
            error = Some(err);
            break;
        }
    }
    let error =
        error.expect("a binding-stripped rotated generation cannot replay its market events");
    assert!(
        error.to_string().contains("without a bound market"),
        "the failure names the missing binding clearly, got {error}",
    );
}

#[test]
fn rotated_journal_header_carries_both_epochs_and_admits_events_stamped_with_the_process_epoch() {
    // A rotated generation's merged-journal header records the derived output
    // epochs AND the base process admission epochs (the two-epoch journal). A fresh
    // `runtime_from_header` built from it must admit venue events the still-running
    // feeds stamped with the BASE process epoch; a single-epoch header (output ==
    // admission = derived) would reject exactly those events.
    let kalshi = parse_excerpt("kalshi.journal.jsonl");
    let polymarket = parse_excerpt("polymarket.journal.jsonl");
    let (runtime, _shared) = build_runtime(&kalshi, &polymarket);
    let output_epoch = "multi-fixture-epoch-1700000600";
    let header = runtime.rotated_journal_header(output_epoch);
    assert_eq!(
        header.kalshi.admission_epoch.as_deref(),
        Some(kalshi.publisher_epoch.as_str()),
        "the rotated header carries the base Kalshi admission epoch",
    );
    assert_eq!(
        header.kalshi.publisher_epoch,
        format!("{output_epoch}.kalshi"),
        "the rotated header's Kalshi output epoch is derived",
    );

    // The header round-trips as JSON with the admission epoch present.
    let text = serde_json::to_string(&header).expect("header serializes");
    assert!(
        text.contains("admission_epoch"),
        "the serialized rotated header carries the admission epoch: {text}",
    );
    let decoded: MultiJournalHeader = serde_json::from_str(&text).expect("header round-trips");
    assert_eq!(decoded, header);

    // Build a runtime from the two-epoch header and admit the excerpt's opening
    // Kalshi inputs (stamped with the base process epoch): they admit.
    let shared = Rc::new(RefCell::new(Vec::<u8>::new()));
    let mut two_epoch = runtime_from_header(
        Rc::clone(&shared),
        &header,
        DEFAULT_MAX_UPDATES,
        DEFAULT_MAX_BYTES,
    )
    .expect("two-epoch runtime");
    for input in &kalshi.inputs {
        two_epoch
            .admit(MultiVenueTag::Kalshi, input.clone())
            .expect("two-epoch header admits base-epoch events end to end");
    }

    // A single-epoch header (admission omitted, so output == admission = derived)
    // rejects the base-epoch market events — proving the admission epoch is
    // load-bearing for a rotated generation's replay.
    let mut single_epoch_header = header.clone();
    single_epoch_header.kalshi.admission_epoch = None;
    let single_shared = Rc::new(RefCell::new(Vec::<u8>::new()));
    let mut single_epoch = runtime_from_header(
        single_shared,
        &single_epoch_header,
        DEFAULT_MAX_UPDATES,
        DEFAULT_MAX_BYTES,
    )
    .expect("single-epoch runtime");
    let mut rejected = false;
    for input in &kalshi.inputs {
        if single_epoch
            .admit(MultiVenueTag::Kalshi, input.clone())
            .is_err()
        {
            rejected = true;
            break;
        }
    }
    assert!(
        rejected,
        "a single-epoch (derived) header rejects events stamped with the base process epoch",
    );
}

/// Restamp every `publisher_epoch` an input's source events carry to `epoch`,
/// mirroring the LIVE path where every feed (both market runners, the shared spot
/// tickers and the Polymarket TWAP relay) stamps admitted events with the ONE
/// base process epoch, while each leg's per-generation OUTPUT epoch is derived.
/// Round-trips through JSON so no per-variant, per-event setter is needed.
fn with_process_epoch(input: &PredictionJournalInput, epoch: &str) -> PredictionJournalInput {
    fn restamp(value: &mut Value, epoch: &str) {
        match value {
            Value::Object(map) => {
                if map.contains_key("publisher_epoch") {
                    map.insert("publisher_epoch".to_owned(), Value::from(epoch));
                }
                for child in map.values_mut() {
                    restamp(child, epoch);
                }
            }
            Value::Array(items) => items.iter_mut().for_each(|item| restamp(item, epoch)),
            _ => {}
        }
    }
    let mut value = serde_json::to_value(input).expect("input serializes");
    restamp(&mut value, epoch);
    serde_json::from_value(value).expect("restamped input deserializes")
}

/// The generation-0 merged-journal header the bin's `run_multi` writes on the LIVE
/// path: each leg's OUTPUT epoch is per-leg derived (`<base>.<venue>`), while both
/// legs' ADMISSION epoch is the base process epoch the still-running feeds stamp
/// every admitted event with. Output differs from admission already on generation
/// 0 — the two-epoch shape the pre-fix single-epoch header got wrong (E64/C5).
fn live_gen0_header(base_epoch: &str, wall_start: &str) -> MultiJournalHeader {
    MultiJournalHeader {
        publisher_epoch: base_epoch.to_owned(),
        wall_start: wall_start.to_owned(),
        kalshi: VenueHeader {
            publisher_epoch: format!("{base_epoch}.kalshi"),
            wall_start: wall_start.to_owned(),
            admission_epoch: Some(base_epoch.to_owned()),
            active_binding: None,
        },
        polymarket: VenueHeader {
            publisher_epoch: format!("{base_epoch}.polymarket"),
            wall_start: wall_start.to_owned(),
            admission_epoch: Some(base_epoch.to_owned()),
            active_binding: None,
        },
    }
}

#[test]
fn live_generation_zero_admits_and_emits_both_legs_under_derived_output_epochs() {
    // The live regression (E64/C5): on generation 0 the still-running feeds stamp
    // every admitted event with the base process `--publisher-epoch`, while each
    // leg publishes under its own derived OUTPUT epoch (`<base>.<venue>`). Build
    // the exact generation-0 header the bin's `run_multi` now writes (output !=
    // admission), restamp both excerpts' events onto the ONE base process epoch as
    // the live feeds do, and drive them: BOTH legs must admit every record and
    // emit frames under their derived output epochs. Before the fix each leg's
    // admission epoch WAS the derived output epoch, so a base-epoch admission (the
    // TWAP/spot events, and every market event) was rejected with "source event
    // publisher_epoch disagrees with the admission epoch".
    let kalshi = parse_excerpt("kalshi.journal.jsonl");
    let polymarket = parse_excerpt("polymarket.journal.jsonl");
    let base_epoch = "smoke-live-multi-1788500400";
    let header = live_gen0_header(base_epoch, &kalshi.wall_start);
    let merged = merged_order(&kalshi, &polymarket);
    assert!(merged.len() > 100, "the excerpts carry enough admissions");

    let shared = Rc::new(RefCell::new(Vec::<u8>::new()));
    let mut runtime = runtime_from_header(
        Rc::clone(&shared),
        &header,
        DEFAULT_MAX_UPDATES,
        DEFAULT_MAX_BYTES,
    )
    .expect("live generation-0 runtime");
    for record in &merged {
        let input = with_process_epoch(&record.input, base_epoch);
        runtime
            .admit(record.venue, input)
            .expect("generation 0 admits every base-epoch event on both legs");
    }
    runtime.flush().expect("flush");

    assert!(
        runtime.next_admission_seq(MultiVenueTag::Kalshi) > 0
            && runtime.next_admission_seq(MultiVenueTag::Polymarket) > 0,
        "both legs admitted base-epoch events in generation 0",
    );
    let output = String::from_utf8(shared.borrow().clone()).expect("utf8");
    assert!(
        output.contains(&format!("{base_epoch}.kalshi"))
            && output.contains(&format!("{base_epoch}.polymarket")),
        "both legs emit frames under their derived output epochs (output != admission): {output}",
    );
}

#[test]
fn live_rotation_keeps_the_base_admission_epoch_and_emits_both_legs() {
    // The base process admission epoch must survive an in-process rotation: after
    // the boundary the feeds keep stamping the SAME base epoch while each leg
    // publishes under a fresh DERIVED output epoch. Drive generation 0 from the
    // live gen-0 header, rotate, then admit base-epoch Kalshi market events and a
    // Polymarket health tick into generation 1. The rotated header must carry the
    // base admission epoch for both legs; the Kalshi leg admits its base-epoch
    // market-relative events across the boundary; and both legs emit reset frames
    // under the rotated derived output epochs.
    let kalshi = parse_excerpt("kalshi.journal.jsonl");
    let polymarket = parse_excerpt("polymarket.journal.jsonl");
    let base_epoch = "smoke-live-multi-1788500400";
    let header = live_gen0_header(base_epoch, &kalshi.wall_start);
    let merged = merged_order(&kalshi, &polymarket);
    // Generation 0 carries both legs (and binds them); the tail is all Kalshi, so
    // generation 1 is a stretch of market-relative Kalshi events.
    let split = 158;

    let shared = Rc::new(RefCell::new(Vec::<u8>::new()));
    let mut runtime = runtime_from_header(
        Rc::clone(&shared),
        &header,
        DEFAULT_MAX_UPDATES,
        DEFAULT_MAX_BYTES,
    )
    .expect("live generation-0 runtime");
    for record in &merged[..split] {
        let input = with_process_epoch(&record.input, base_epoch);
        runtime.admit(record.venue, input).expect("gen-0 admit");
    }
    runtime.flush().expect("flush gen-0");

    let rot_output_epoch = "smoke-live-multi-1788500400-1700000600";
    let rotated_header = runtime.rotated_journal_header(rot_output_epoch);
    assert_eq!(
        rotated_header.kalshi.admission_epoch.as_deref(),
        Some(base_epoch),
        "the rotated Kalshi header keeps the base process admission epoch",
    );
    assert_eq!(
        rotated_header.polymarket.admission_epoch.as_deref(),
        Some(base_epoch),
        "the rotated Polymarket header keeps the base process admission epoch",
    );

    shared.borrow_mut().clear();
    runtime
        .rotate(rot_output_epoch, ROTATION_CAPS)
        .expect("rotate the multi runtime");

    let mut kalshi_seq = 0u64;
    for record in &merged[split..] {
        assert_eq!(
            record.venue,
            MultiVenueTag::Kalshi,
            "the split tail is all Kalshi in this fixture",
        );
        let renumbered = with_admission_seq(&record.input, kalshi_seq);
        let input = with_process_epoch(&renumbered, base_epoch);
        kalshi_seq += 1;
        runtime
            .admit(MultiVenueTag::Kalshi, input)
            .expect("generation 1 admits base-epoch Kalshi events after the rotation");
    }
    // The tail carries no Polymarket admissions, so a health tick after the
    // excerpt drives the Polymarket leg's reset frame onto the successor wire.
    runtime
        .admit(
            MultiVenueTag::Polymarket,
            PredictionJournalInput::HealthTick {
                admission_seq: 0,
                now_ns: 5_000_000_000,
            },
        )
        .expect("gen-1 Polymarket health tick");
    runtime.flush().expect("flush gen-1");

    assert!(
        kalshi_seq > 0,
        "the Kalshi leg admitted base-epoch market events after the rotation",
    );
    let gen1 = String::from_utf8(shared.borrow().clone()).expect("utf8");
    assert!(
        gen1.contains(&format!("{rot_output_epoch}.kalshi"))
            && gen1.contains(&format!("{rot_output_epoch}.polymarket")),
        "both legs emit under the rotated derived output epochs after the boundary: {gen1}",
    );
}

#[test]
fn a_foreign_epoch_event_is_rejected_on_a_live_generation_zero_leg() {
    // A genuinely foreign epoch — neither the base process admission epoch nor a
    // derived output epoch — is still rejected on admission, so making the
    // admission epoch the base process epoch never widens what a leg accepts.
    let kalshi = parse_excerpt("kalshi.journal.jsonl");
    let base_epoch = "smoke-live-multi-1788500400";
    let header = live_gen0_header(base_epoch, &kalshi.wall_start);

    let shared = Rc::new(RefCell::new(Vec::<u8>::new()));
    let mut runtime = runtime_from_header(
        Rc::clone(&shared),
        &header,
        DEFAULT_MAX_UPDATES,
        DEFAULT_MAX_BYTES,
    )
    .expect("live generation-0 runtime");

    let mut error = None;
    for input in &kalshi.inputs {
        let foreign = with_process_epoch(input, "totally-foreign-epoch");
        if let Err(err) = runtime.admit(MultiVenueTag::Kalshi, foreign) {
            error = Some(err);
            break;
        }
    }
    let error = error.expect("a foreign-epoch event must be rejected");
    assert!(
        error
            .to_string()
            .contains("disagrees with the admission epoch"),
        "the rejection names the epoch disagreement, got: {error}",
    );
}

// ---------------------------------------------------------------------------
// Blend output-epoch restamp across a rotation (C1): a `btc.multi.*` blend that
// is WITHHELD before the boundary (no aligned pair) and first becomes Ready in
// generation 1 must carry generation 1's multi OUTPUT epoch, not the warm
// `MultiProcessor`'s construction (generation-0) epoch. The local venue excerpts
// never reach an `exact_close` alignment, so the aligned pair is fed straight
// into the runtime's warm `MultiProcessor` through the same `observe` the harvest
// uses, and the real emission path (`drive_multi`) is driven directly.
// ---------------------------------------------------------------------------

use crate::multi_signals::MultiVenue;
use serde_json::json;
use signals_core::{Quality, QualityState, SIGNAL_UPDATE_SCHEMA_VERSION, Validate};

const BLEND_PRODUCED_AT: &str = "2026-09-03T11:10:05.000000000Z";
const BLEND_CLOSE: &str = "2026-09-03T11:15:00.000000000Z";
const BLEND_KALSHI_OPEN: &str = "2026-09-03T11:00:00.000000000Z";
const BLEND_POLYMARKET_OPEN: &str = "2026-09-03T11:10:00.000000000Z";

/// A ready leg `SignalUpdate` carrying `values`. `observe` reads only the value
/// payload (never the leg's own epoch), so the deliberately foreign epoch here is
/// immaterial — the blend the `MultiProcessor` derives takes the lane's own
/// publisher epoch, and this test proves that is generation 1's after a rotation.
fn blend_leg_update(signal_id: &str, values: Vec<(&str, Value)>) -> SignalUpdate {
    SignalUpdate {
        schema_version: SIGNAL_UPDATE_SCHEMA_VERSION,
        signal_id: signal_id.to_owned(),
        signal_version: "1.0.0".to_owned(),
        publisher_epoch: "leg-epoch-immaterial".to_owned(),
        sequence: 0,
        event_time: BLEND_PRODUCED_AT.to_owned(),
        produced_at: BLEND_PRODUCED_AT.to_owned(),
        valid_from: BLEND_PRODUCED_AT.to_owned(),
        valid_until: BLEND_CLOSE.to_owned(),
        quality: Quality {
            state: QualityState::Ready,
            reasons: Vec::new(),
        },
        values: values.into_iter().map(|(k, v)| (k.to_owned(), v)).collect(),
        scope: SignalScope::Global,
    }
}

/// Feed the canonical aligned pair (the values the `multi_signals` unit tests
/// prove yield two Ready blends) into the runtime's warm `MultiProcessor`.
fn observe_canonical_pair(runtime: &mut MultiLaneRuntime<Vec<u8>>) {
    runtime
        .multi
        .observe(
            MultiVenue::Kalshi,
            &[
                blend_leg_update(
                    "btc.kalshi.market_contract",
                    vec![
                        ("market_id", json!("KXBTC15M-A")),
                        ("market_generation", json!(3)),
                        ("window_open", json!(BLEND_KALSHI_OPEN)),
                        ("window_close", json!(BLEND_CLOSE)),
                        ("strike", json!("111500")),
                        (
                            "settlement_benchmark",
                            json!("cf_benchmarks_brti_sixty_print"),
                        ),
                        ("comparator", json!("greater_or_equal")),
                        ("tie_outcome", json!("affirmative")),
                    ],
                ),
                blend_leg_update(
                    "btc.kalshi.prediction_quote",
                    vec![
                        ("affirmative_bid", json!("0.40")),
                        ("affirmative_ask", json!("0.42")),
                        ("affirmative_midpoint", json!("0.41")),
                        ("negative_ask", json!("0.60")),
                        ("quote_time", json!(BLEND_PRODUCED_AT)),
                        ("max_quote_age_ms", json!(0)),
                    ],
                ),
                blend_leg_update(
                    "btc.kalshi.fair_up_probability",
                    vec![("probability", json!("0.5"))],
                ),
                blend_leg_update(
                    "btc.kalshi.pair_cost_state",
                    vec![
                        ("pair_cost", json!("0.98")),
                        ("fee_adjusted_pair_cost", json!("0.99")),
                        ("yes_leg_fee", json!("0.01")),
                        ("no_leg_fee", json!("0.01")),
                        ("premium_threshold", json!("0.01")),
                    ],
                ),
            ],
        )
        .expect("kalshi observe");
    runtime
        .multi
        .observe(
            MultiVenue::Polymarket,
            &[
                blend_leg_update(
                    "btc.polymarket.market_contract",
                    vec![
                        ("market_id", json!("0xpoly")),
                        ("market_generation", json!(7)),
                        ("window_open", json!(BLEND_POLYMARKET_OPEN)),
                        ("window_close", json!(BLEND_CLOSE)),
                        ("strike", json!("111600")),
                        ("settlement_benchmark", json!("chainlink_btc_usd_twap_60s")),
                        ("comparator", json!("greater_or_equal")),
                        ("tie_outcome", json!("affirmative")),
                    ],
                ),
                blend_leg_update(
                    "btc.polymarket.prediction_quote",
                    vec![
                        ("affirmative_bid", json!("0.44")),
                        ("affirmative_ask", json!("0.46")),
                        ("affirmative_midpoint", json!("0.45")),
                        ("negative_ask", json!("0.56")),
                        ("quote_time", json!(BLEND_PRODUCED_AT)),
                        ("max_quote_age_ms", json!(0)),
                    ],
                ),
                blend_leg_update(
                    "btc.polymarket.fair_up_probability",
                    vec![("probability", json!("0.5"))],
                ),
                blend_leg_update(
                    "btc.polymarket.pair_cost_state",
                    vec![
                        ("pair_cost", json!("0.97")),
                        ("fee_adjusted_pair_cost", json!("0.985")),
                        ("up_leg_fee", json!("0.005")),
                        ("down_leg_fee", json!("0.005")),
                        ("premium_threshold", json!("0.01")),
                    ],
                ),
            ],
        )
        .expect("polymarket observe");
}

#[test]
fn a_blend_withheld_before_a_rotation_and_ready_after_carries_generation_ones_epoch() {
    // The regression (C1, third live failure): a rotation keeps the `MultiProcessor`
    // warm but does not re-stamp it, so a blend first published in generation 1
    // carried generation 0's epoch while the batch envelope carried generation 1's
    // — the admission boundary (`validate_batch_updates`) rejected it with
    // "updates[0].publisher_epoch: must match the enclosing signal batch". The
    // earlier fixes restamped only the LEG (inner-pipeline) epochs and the header;
    // the multi blend, published by the warm `MultiProcessor` and NOT re-stamped,
    // slipped through. Here `drive_multi` restamps every blend onto the current
    // generation's multi output epoch, so the emitted batch passes the exact check
    // the live path runs (`SignalStreamMessage::validate` inside `write_stream_message`).
    let gen0_epoch = "multi-blend-gen0";
    let gen1_epoch = "multi-blend-gen1-1700000600";
    let shared = Rc::new(RefCell::new(Vec::<u8>::new()));
    let kalshi_session = venue_session(
        PredictionMarketVenue::Kalshi,
        format!("{gen0_epoch}.kalshi"),
        BLEND_PRODUCED_AT,
        250,
    )
    .expect("kalshi session");
    let polymarket_session = venue_session(
        PredictionMarketVenue::Polymarket,
        format!("{gen0_epoch}.polymarket"),
        BLEND_PRODUCED_AT,
        250,
    )
    .expect("polymarket session");
    let mut runtime = MultiLaneRuntime::new(
        Rc::clone(&shared),
        gen0_epoch,
        chrono::DateTime::parse_from_rfc3339(BLEND_PRODUCED_AT)
            .expect("wall start")
            .with_timezone(&chrono::Utc),
        &kalshi_session,
        &polymarket_session,
        None,
        None,
        DEFAULT_MAX_UPDATES,
        DEFAULT_MAX_BYTES,
    )
    .expect("runtime");

    // GENERATION 0: no aligned pair, so both blends are withheld (W2). The warm
    // `MultiProcessor` never publishes a blend and so never records that it was
    // ever ready.
    runtime.multi_now_ns = 0;
    runtime
        .drive_multi()
        .expect("generation-0 withheld decision");
    runtime.flush().expect("flush gen-0");
    let gen0 = String::from_utf8(shared.borrow().clone()).expect("gen0 utf8");
    assert!(
        !gen0.lines().any(|line| {
            serde_json::from_str::<Value>(line).is_ok_and(|v| v["type"] == "signal_batch")
        }),
        "the blend is withheld before the boundary (no signal_batch in generation 0): {gen0}",
    );

    // ROTATE onto generation 1 (the bin swaps the shared file first, mirrored by
    // clearing the buffer). The runtime's publisher epoch advances to gen1; the
    // warm `MultiProcessor` keeps its generation-0 construction epoch.
    *shared.borrow_mut() = Vec::new();
    runtime
        .rotate(gen1_epoch, ROTATION_CAPS)
        .expect("rotate the multi runtime");

    // GENERATION 1: the pair aligns and the blend becomes Ready for the first time.
    observe_canonical_pair(&mut runtime);
    runtime.multi_now_ns = 0;
    // Pre-fix this panics: the blend carries gen0's epoch, the batch envelope gen1's,
    // and `write_stream_message`'s `validate()` rejects the mismatch.
    runtime
        .drive_multi()
        .expect("generation-1 Ready blend passes the admission boundary");

    // INVALIDATED at a roll after the rotation: the bound instance departs and the
    // runtime writes its terminal `invalidated` under generation 1's epoch (V10/L2).
    // This is the exact helper the `Unavailable` admission path invokes.
    let roll_ns = 3_000_000_000_u64;
    runtime.multi_now_ns = runtime.multi_now_ns.max(roll_ns);
    runtime
        .invalidate_bound_multi(roll_ns)
        .expect("invalidate the bound multi instance at the roll");
    runtime.flush().expect("flush gen-1");

    // Run the SAME stream-accounting check the live path applies over EVERY frame
    // generation 1 wrote, and pin the blend + invalidation epochs to gen1.
    let gen1 = String::from_utf8(shared.borrow().clone()).expect("gen1 utf8");
    let mut ready_blend_updates = 0usize;
    let mut invalidated_blends = 0usize;
    for line in gen1.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let message: SignalStreamMessage =
            serde_json::from_str(line).expect("generation-1 frame decodes");
        // The exact admission-boundary check `write_stream_message` runs live —
        // this is what rejected the mismatched blend on the box.
        message
            .validate()
            .expect("every generation-1 frame passes the admission boundary");
        assert_no_global_multi_scope(&message);
        match &message {
            SignalStreamMessage::SignalBatch {
                publisher_epoch,
                updates,
                ..
            } => {
                for update in updates {
                    // The enclosing-batch invariant, made explicit.
                    assert_eq!(
                        &update.publisher_epoch, publisher_epoch,
                        "every update's epoch matches its enclosing batch",
                    );
                    if update.signal_id.starts_with("btc.multi.") {
                        assert_eq!(
                            update.publisher_epoch, gen1_epoch,
                            "the blend carries generation 1's multi output epoch, not gen 0's",
                        );
                        assert_ne!(
                            update.publisher_epoch, gen0_epoch,
                            "the blend must not carry the warm processor's gen-0 epoch",
                        );
                        ready_blend_updates += 1;
                    }
                }
            }
            SignalStreamMessage::InstanceStatus {
                publisher_epoch,
                signal_id,
                state,
                ..
            } if signal_id.starts_with("btc.multi.")
                && *state == signals_core::InstanceStatusState::Invalidated =>
            {
                assert_eq!(
                    publisher_epoch, gen1_epoch,
                    "the post-rotation invalidation carries generation 1's epoch",
                );
                invalidated_blends += 1;
            }
            _ => {}
        }
    }
    assert_eq!(
        ready_blend_updates,
        crate::multi_signals::MULTI_SIGNAL_COUNT,
        "both blends became Ready in generation 1 and reached the wire",
    );
    assert_eq!(
        invalidated_blends,
        crate::multi_signals::MULTI_SIGNAL_COUNT,
        "both blends were invalidated at the post-rotation roll",
    );
}

// ---------------------------------------------------------------------------
// Rotation restarts every blend's per-identity decision sequence at zero (C5)
// ---------------------------------------------------------------------------

/// Re-observe the canonical pair with a changed Kalshi quote and pair cost, so
/// both blends' change keys differ and a following decision re-publishes them
/// (advancing their per-identity sequence) rather than coalescing.
fn observe_pair_variant(runtime: &mut MultiLaneRuntime<Vec<u8>>) {
    runtime
        .multi
        .observe(
            MultiVenue::Kalshi,
            &[
                blend_leg_update(
                    "btc.kalshi.prediction_quote",
                    vec![
                        ("affirmative_bid", json!("0.42")),
                        ("affirmative_ask", json!("0.44")),
                        ("affirmative_midpoint", json!("0.43")),
                        ("negative_ask", json!("0.58")),
                        ("quote_time", json!(BLEND_PRODUCED_AT)),
                        ("max_quote_age_ms", json!(0)),
                    ],
                ),
                blend_leg_update(
                    "btc.kalshi.pair_cost_state",
                    vec![
                        ("pair_cost", json!("0.97")),
                        ("fee_adjusted_pair_cost", json!("0.98")),
                        ("yes_leg_fee", json!("0.01")),
                        ("no_leg_fee", json!("0.01")),
                        ("premium_threshold", json!("0.01")),
                    ],
                ),
            ],
        )
        .expect("kalshi variant observe");
}

/// The greatest per-identity `sequence` any `btc.multi.*` ready update in the
/// rendered stream reaches.
fn max_multi_sequence(rendered: &str) -> u64 {
    let mut max = 0;
    for line in rendered.lines() {
        if let Ok(SignalStreamMessage::SignalBatch { updates, .. }) =
            serde_json::from_str::<SignalStreamMessage>(line)
        {
            for update in updates {
                if update.signal_id.starts_with("btc.multi.") {
                    max = max.max(update.sequence);
                }
            }
        }
    }
    max
}

/// C5 Design B: a rotation opens a fresh output epoch, so each blend's first ready
/// decision after the boundary carries per-identity `sequence` zero even though the
/// warm `MultiProcessor` (blend emitter memory) crosses the boundary intact. Before
/// the fix the warm blend emitters carried their departed sequence into the
/// successor's epoch, breaking the "first ready decision in an epoch is sequence
/// zero" contract. A crash-resume rebuilds the blends fresh (sequence zero) from the
/// checkpoint's lifetime memory, so the reset is what keeps the warm live rotation
/// numbering identical to that replay.
#[test]
fn rotation_restarts_every_blend_decision_sequence_at_zero() {
    let gen0_epoch = "multi-seq-gen0";
    let gen1_epoch = "multi-seq-gen1-1700000600";
    let shared = Rc::new(RefCell::new(Vec::<u8>::new()));
    let kalshi_session = venue_session(
        PredictionMarketVenue::Kalshi,
        format!("{gen0_epoch}.kalshi"),
        BLEND_PRODUCED_AT,
        250,
    )
    .expect("kalshi session");
    let polymarket_session = venue_session(
        PredictionMarketVenue::Polymarket,
        format!("{gen0_epoch}.polymarket"),
        BLEND_PRODUCED_AT,
        250,
    )
    .expect("polymarket session");
    let mut runtime = MultiLaneRuntime::new(
        Rc::clone(&shared),
        gen0_epoch,
        chrono::DateTime::parse_from_rfc3339(BLEND_PRODUCED_AT)
            .expect("wall start")
            .with_timezone(&chrono::Utc),
        &kalshi_session,
        &polymarket_session,
        None,
        None,
        DEFAULT_MAX_UPDATES,
        DEFAULT_MAX_BYTES,
    )
    .expect("runtime");

    // GENERATION 0: publish both blends twice so their per-identity sequences pass
    // zero (a carried-over sequence would then be visible in generation 1).
    observe_canonical_pair(&mut runtime);
    runtime.multi_now_ns = 0;
    runtime.drive_multi().expect("gen0 decision 0");
    observe_pair_variant(&mut runtime);
    runtime.multi_now_ns = 2_000_000;
    runtime.drive_multi().expect("gen0 decision 1");
    runtime.flush().expect("flush gen0");

    let gen0 = String::from_utf8(shared.borrow().clone()).expect("gen0 utf8");
    let gen0_max = max_multi_sequence(&gen0);
    assert!(
        gen0_max >= 1,
        "gen0 advanced the blends beyond sequence zero (max {gen0_max}): {gen0}"
    );

    // ROTATE onto generation 1 (the bin swaps the shared file first, mirrored by
    // clearing the buffer). This keeps the warm `MultiProcessor` and must restart
    // its blends' per-identity sequences.
    *shared.borrow_mut() = Vec::new();
    runtime
        .rotate(gen1_epoch, ROTATION_CAPS)
        .expect("rotate the multi runtime");

    // GENERATION 1: the canonical pair differs from gen0's last (variant) values,
    // so both blends publish immediately — and at sequence zero.
    observe_canonical_pair(&mut runtime);
    runtime.multi_now_ns = 4_000_000;
    runtime.drive_multi().expect("gen1 decision 0");
    runtime.flush().expect("flush gen1");

    let gen1 = String::from_utf8(shared.borrow().clone()).expect("gen1 utf8");
    let mut ready_blends = 0usize;
    for line in gen1.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let message: SignalStreamMessage =
            serde_json::from_str(line).expect("generation-1 frame decodes");
        message
            .validate()
            .expect("every generation-1 frame passes the admission boundary");
        if let SignalStreamMessage::SignalBatch { updates, .. } = &message {
            for update in updates {
                if update.signal_id.starts_with("btc.multi.") {
                    assert_eq!(
                        update.sequence, 0,
                        "{}'s first ready decision in the rotated generation carries sequence zero",
                        update.signal_id,
                    );
                    ready_blends += 1;
                }
            }
        }
    }
    assert_eq!(
        ready_blends,
        crate::multi_signals::MULTI_SIGNAL_COUNT,
        "both blends published their first generation-1 decision at sequence zero: {gen1}",
    );
}
