use std::path::Path;
use std::sync::Arc;

use btc_signals::checkpoint::CheckpointDocument;
use btc_signals::journal::{
    JOURNAL_SCHEMA_VERSION, JournalEnd, JournalError, JournalInput, JournalReplayReader,
    JournalSession, JournalTotals, SignalJournalWriter, contract_fingerprint,
};
use btc_signals::{SpotReferenceRuntimeConfig, SpotSignalProcessor, UnavailableReason, Venue};
use serde_json::json;

mod support;

use crate::support::{
    TestPath, bare_coinbase_event, bare_kraken_event, coinbase_event, kraken_event, last_line,
    market, processor, session, strip_trailing_newline, terminal, truncate_last_line, wall_start,
};

const MAX_BYTES: u64 = 1_048_576;

#[test]
fn complete_journal_round_trips_every_input_variant_in_global_order() {
    let path = TestPath::new("journal-round-trip", "jsonl");
    let session = session("journal-test", 100);
    let inputs = vec![
        market(0, bare_coinbase_event(0, "coinbase-test"), 10, 20),
        JournalInput::Availability {
            admission_seq: 1,
            venue: Venue::Kraken,
            reason: UnavailableReason::Disconnected,
            now_ns: 21,
        },
        JournalInput::HealthTick {
            admission_seq: 2,
            now_ns: 100,
        },
        // Cross-venue receipt inversion is legal. Only each venue must be monotone.
        market(3, bare_kraken_event(0, "kraken-test"), 5, 101),
    ];
    let end = terminal(4, 2, 1, 1, 3);

    let writer = SignalJournalWriter::create(path.as_path(), session.clone(), MAX_BYTES, 16)
        .expect("journal creates");
    for input in &inputs {
        writer.enqueue(input.clone()).expect("input enqueues");
    }
    let report = writer.finish(end.clone()).expect("terminal record syncs");
    assert_eq!(report.records_written, 6);
    assert!(report.bytes_written > 0);
    assert!(report.data_syncs >= 2);

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        assert_eq!(
            std::fs::metadata(path.as_path())
                .expect("journal metadata reads")
                .mode()
                & 0o777,
            0o600
        );
    }

    let mut reader =
        JournalReplayReader::open(path.as_path(), MAX_BYTES).expect("complete journal opens");
    assert_eq!(reader.session(), &session);
    let mut replayed = Vec::new();
    while let Some(input) = reader.next_input().expect("input validates") {
        replayed.push(input);
    }
    assert_eq!(replayed, inputs);
    assert_eq!(reader.end().expect("terminal exists"), &end);
}

#[test]
fn multi_event_batch_round_trips_and_counts_normalized_events() {
    let path = TestPath::new("journal-market-batch", "jsonl");
    let input = JournalInput::MarketEvents {
        admission_seq: 0,
        events: Arc::new(vec![
            bare_coinbase_event(0, "coinbase-test"),
            bare_coinbase_event(1, "coinbase-test"),
        ]),
        logical_receipt_ns: 10,
        now_ns: 20,
        resync: false,
    };
    let writer =
        SignalJournalWriter::create(path.as_path(), session("journal-test", 100), MAX_BYTES, 4)
            .expect("journal creates");
    writer.enqueue(input.clone()).expect("batch enqueues");
    writer
        .finish(terminal(1, 2, 0, 0, 0))
        .expect("journal completes");

    let mut reader = JournalReplayReader::open(path.as_path(), MAX_BYTES).expect("journal opens");
    assert_eq!(reader.next_input().expect("batch validates"), Some(input));
    assert!(reader.next_input().expect("terminal validates").is_none());
}

#[test]
fn replay_rejects_empty_and_mixed_venue_market_batches() {
    let empty_path = write_complete(
        "empty-market-batch",
        vec![JournalInput::MarketEvents {
            admission_seq: 0,
            events: Arc::new(Vec::new()),
            logical_receipt_ns: 1,
            now_ns: 1,
            resync: false,
        }],
        terminal(1, 0, 0, 0, 0),
    );
    let mut empty_reader =
        JournalReplayReader::open(empty_path.as_path(), MAX_BYTES).expect("header opens");
    assert!(matches!(
        empty_reader.next_input(),
        Err(JournalError::Invalid(_))
    ));

    let mixed_path = write_complete(
        "mixed-market-batch",
        vec![JournalInput::MarketEvents {
            admission_seq: 0,
            events: Arc::new(vec![
                bare_coinbase_event(0, "coinbase-test"),
                bare_kraken_event(0, "kraken-test"),
            ]),
            logical_receipt_ns: 1,
            now_ns: 1,
            resync: false,
        }],
        terminal(1, 2, 0, 0, 0),
    );
    let mut mixed_reader =
        JournalReplayReader::open(mixed_path.as_path(), MAX_BYTES).expect("header opens");
    assert!(matches!(
        mixed_reader.next_input(),
        Err(JournalError::Invalid(_))
    ));
}

#[test]
fn availability_round_trip_preserves_degraded_processor_input() {
    let path = TestPath::new("journal-availability", "jsonl");
    let writer =
        SignalJournalWriter::create(path.as_path(), session("journal-test", 100), MAX_BYTES, 4)
            .expect("journal creates");
    let availability = JournalInput::Availability {
        admission_seq: 0,
        venue: Venue::Coinbase,
        reason: UnavailableReason::SubscriptionRejected,
        now_ns: 44,
    };
    writer
        .enqueue(availability.clone())
        .expect("availability enqueues");
    writer
        .finish(terminal(1, 0, 1, 0, 1))
        .expect("journal completes");

    let mut reader = JournalReplayReader::open(path.as_path(), MAX_BYTES).expect("journal opens");
    assert_eq!(
        reader.next_input().expect("input reads"),
        Some(availability)
    );
    assert!(reader.next_input().expect("terminal reads").is_none());
}

#[test]
fn journal_replay_reproduces_disconnect_degradation_exactly() {
    let path = TestPath::new("journal-processor-replay", "jsonl");
    let inputs = vec![
        market(0, coinbase_event(0, "coinbase-test"), 10, 20),
        market(1, kraken_event(0, "kraken-test"), 25, 30),
        JournalInput::Availability {
            admission_seq: 2,
            venue: Venue::Kraken,
            reason: UnavailableReason::Disconnected,
            now_ns: 40,
        },
        JournalInput::HealthTick {
            admission_seq: 3,
            now_ns: 1_000_000_040,
        },
    ];

    let mut original = processor("journal-test");
    let original_updates = apply_inputs(&mut original, inputs.clone());
    assert_eq!(original_updates[2].quality.reasons, ["kraken_disconnected"]);
    let end = JournalEnd {
        totals: JournalTotals {
            admitted_inputs: 4,
            market_events: 2,
            availability_events: 1,
            health_ticks: 1,
            updates_written: u64::try_from(original_updates.len()).expect("update count fits"),
            stream_messages_written: 0,
        },
        quality_reason_entries: original.quality_reason_entries().clone(),
        raw_capture_references: Vec::new(),
    };
    let writer =
        SignalJournalWriter::create(path.as_path(), session("journal-test", 100), MAX_BYTES, 16)
            .expect("journal creates");
    for input in inputs {
        writer.enqueue(input).expect("input enqueues");
    }
    writer.finish(end.clone()).expect("journal completes");

    let mut reader = JournalReplayReader::open(path.as_path(), MAX_BYTES).expect("journal opens");
    let mut replay = processor("journal-test");
    let mut replayed_inputs = Vec::new();
    while let Some(input) = reader.next_input().expect("journal input validates") {
        replayed_inputs.push(input);
    }
    let replayed_updates = apply_inputs(&mut replay, replayed_inputs);
    assert_eq!(replayed_updates, original_updates);
    assert_eq!(replay.quality_reason_entries(), end.quality_reason_entries);
    assert_eq!(reader.end().expect("terminal exists"), &end);
}

#[test]
fn resumed_journal_embeds_the_state_needed_for_self_contained_replay() {
    let path = TestPath::new("journal-resumed-session", "jsonl");
    let mut original = processor("journal-test");
    original
        .ingest(&coinbase_event(0, "coinbase-test"), 10, 20)
        .expect("Coinbase input applies");
    original
        .ingest(&kraken_event(0, "kraken-test"), 25, 30)
        .expect("Kraken input applies");
    let runtime_config = SpotReferenceRuntimeConfig::new(100).expect("runtime config constructs");
    let checkpoint = CheckpointDocument::new(
        "0123456789abcdef0123456789abcdef".to_owned(),
        "2026-08-13T12:00:00Z".to_owned(),
        30,
        2,
        contract_fingerprint(&runtime_config).expect("fingerprint constructs"),
        runtime_config,
        original.checkpoint_state(),
    )
    .expect("checkpoint constructs");
    let resumed_session = JournalSession::resumed(
        "journal-resumed".to_owned(),
        checkpoint.clone(),
        checkpoint.runtime_config.clone(),
    )
    .expect("resumed journal session constructs");
    let input = JournalInput::Availability {
        admission_seq: 0,
        venue: Venue::Coinbase,
        reason: UnavailableReason::Disconnected,
        now_ns: 40,
    };
    let mut expected_processor = SpotSignalProcessor::restore(
        "journal-resumed",
        wall_start(),
        checkpoint.processor_state.clone(),
    )
    .expect("checkpoint restores");
    let expected_updates = apply_inputs(&mut expected_processor, vec![input.clone()]);
    let end = JournalEnd {
        totals: JournalTotals {
            admitted_inputs: 1,
            market_events: 0,
            availability_events: 1,
            health_ticks: 0,
            updates_written: u64::try_from(expected_updates.len()).expect("update count fits"),
            stream_messages_written: 0,
        },
        quality_reason_entries: expected_processor.quality_reason_entries().clone(),
        raw_capture_references: Vec::new(),
    };
    let writer = SignalJournalWriter::create(path.as_path(), resumed_session, MAX_BYTES, 4)
        .expect("resumed journal creates");
    writer.enqueue(input).expect("input enqueues");
    writer.finish(end.clone()).expect("journal completes");

    let mut reader = JournalReplayReader::open(path.as_path(), MAX_BYTES).expect("journal opens");
    let session = reader.session().clone();
    let initial = session
        .initial_checkpoint
        .expect("resumed journal embeds its checkpoint");
    let mut replay_processor = SpotSignalProcessor::restore(
        &session.publisher_epoch,
        wall_start(),
        initial.processor_state,
    )
    .expect("embedded checkpoint restores");
    let mut inputs = Vec::new();
    while let Some(input) = reader.next_input().expect("journal input validates") {
        inputs.push(input);
    }
    assert_eq!(
        apply_inputs(&mut replay_processor, inputs),
        expected_updates
    );
    assert_eq!(reader.end().expect("terminal exists"), &end);
}

#[test]
fn missing_terminal_is_an_incomplete_recoverable_prefix() {
    let path = TestPath::new("journal-incomplete", "jsonl");
    let writer =
        SignalJournalWriter::create(path.as_path(), session("journal-test", 100), MAX_BYTES, 4)
            .expect("journal creates");
    writer
        .enqueue(JournalInput::HealthTick {
            admission_seq: 0,
            now_ns: 10,
        })
        .expect("tick enqueues");
    writer.abort().expect("prefix drains and syncs");

    let mut reader = JournalReplayReader::open(path.as_path(), MAX_BYTES).expect("prefix opens");
    assert!(reader.next_input().expect("prefix input reads").is_some());
    assert!(matches!(reader.next_input(), Err(JournalError::Incomplete)));
    assert!(matches!(reader.end(), Err(JournalError::Incomplete)));
}

#[test]
fn replay_rejects_tampered_fingerprint_and_unsupported_versions() {
    let fingerprint_path = complete_one_tick("fingerprint");
    mutate_first_record(fingerprint_path.as_path(), |first| {
        first["contract_fingerprint"]["aggregate_sha256"] = json!("0".repeat(64));
    });
    assert!(matches!(
        JournalReplayReader::open(fingerprint_path.as_path(), MAX_BYTES),
        Err(JournalError::FingerprintMismatch { .. })
    ));

    let version_path = complete_one_tick("version");
    mutate_first_record(version_path.as_path(), |first| {
        first["journal_schema_version"] = json!(JOURNAL_SCHEMA_VERSION + 1);
    });
    assert!(matches!(
        JournalReplayReader::open(version_path.as_path(), MAX_BYTES),
        Err(JournalError::UnsupportedVersion(version))
            if version == JOURNAL_SCHEMA_VERSION + 1
    ));

    let legacy_v2_path = complete_one_tick("legacy-v2");
    mutate_first_record(legacy_v2_path.as_path(), |first| {
        first["journal_schema_version"] = json!(2);
    });
    assert!(matches!(
        JournalReplayReader::open(legacy_v2_path.as_path(), MAX_BYTES),
        Err(JournalError::UnsupportedVersion(2))
    ));
}

#[test]
fn replay_preflights_a_schema3_session_before_current_fingerprint_decode() {
    let schema3_path = complete_one_tick("schema3-shape");
    mutate_first_record(schema3_path.as_path(), |first| {
        first["journal_schema_version"] = json!(3);
        let removed = first["contract_fingerprint"]
            .as_object_mut()
            .expect("contract fingerprint is an object")
            .remove("runtime_schema_sha256");
        assert!(removed.is_some(), "current-only schema hash field existed");
    });

    assert!(matches!(
        JournalReplayReader::open(schema3_path.as_path(), MAX_BYTES),
        Err(JournalError::UnsupportedVersion(3))
    ));
}

#[test]
fn replay_rejects_admission_sequence_and_now_regressions() {
    let sequence_path = write_complete(
        "sequence",
        vec![JournalInput::HealthTick {
            admission_seq: 1,
            now_ns: 1,
        }],
        terminal(1, 0, 0, 1, 0),
    );
    let mut sequence_reader =
        JournalReplayReader::open(sequence_path.as_path(), MAX_BYTES).expect("header opens");
    assert!(matches!(
        sequence_reader.next_input(),
        Err(JournalError::AdmissionSequence {
            expected: 0,
            actual: 1
        })
    ));

    let time_path = write_complete(
        "time",
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
        terminal(2, 0, 0, 2, 0),
    );
    let mut time_reader =
        JournalReplayReader::open(time_path.as_path(), MAX_BYTES).expect("header opens");
    time_reader.next_input().expect("first time is valid");
    assert!(matches!(
        time_reader.next_input(),
        Err(JournalError::NowRegression {
            previous: 20,
            actual: 19
        })
    ));
}

#[test]
fn replay_rejects_receipt_after_admission_and_per_venue_regression() {
    let future_path = write_complete(
        "future-receipt",
        vec![market(0, bare_coinbase_event(0, "coinbase-test"), 11, 10)],
        terminal(1, 1, 0, 0, 0),
    );
    let mut future_reader =
        JournalReplayReader::open(future_path.as_path(), MAX_BYTES).expect("header opens");
    assert!(matches!(
        future_reader.next_input(),
        Err(JournalError::ReceiptAfterAdmission {
            receipt: 11,
            now: 10
        })
    ));

    let regression_path = write_complete(
        "receipt-regression",
        vec![
            market(0, bare_coinbase_event(0, "coinbase-test"), 10, 10),
            market(1, bare_kraken_event(0, "kraken-test"), 5, 11),
            market(2, bare_coinbase_event(1, "coinbase-test"), 9, 12),
        ],
        terminal(3, 3, 0, 0, 0),
    );
    let mut regression_reader =
        JournalReplayReader::open(regression_path.as_path(), MAX_BYTES).expect("header opens");
    regression_reader
        .next_input()
        .expect("first Coinbase receipt is valid");
    regression_reader
        .next_input()
        .expect("cross-venue inversion is valid");
    assert!(matches!(
        regression_reader.next_input(),
        Err(JournalError::ReceiptRegression {
            venue: Venue::Coinbase,
            previous: 10,
            actual: 9
        })
    ));
}

#[test]
fn replay_rejects_terminal_totals_mismatch_and_trailing_records() {
    let totals_path = write_complete(
        "totals",
        vec![JournalInput::HealthTick {
            admission_seq: 0,
            now_ns: 1,
        }],
        terminal(2, 0, 0, 2, 0),
    );
    let mut totals_reader =
        JournalReplayReader::open(totals_path.as_path(), MAX_BYTES).expect("header opens");
    totals_reader.next_input().expect("tick reads");
    assert!(matches!(
        totals_reader.next_input(),
        Err(JournalError::TotalsMismatch { .. })
    ));

    let trailing_path = complete_one_tick("trailing");
    let terminal_line = std::fs::read_to_string(trailing_path.as_path())
        .expect("journal reads")
        .lines()
        .last()
        .expect("terminal line exists")
        .to_owned();
    let mut text = std::fs::read_to_string(trailing_path.as_path()).expect("journal reads");
    text.push_str(&terminal_line);
    text.push('\n');
    std::fs::write(trailing_path.as_path(), text).expect("test corruption writes");
    let mut trailing_reader =
        JournalReplayReader::open(trailing_path.as_path(), MAX_BYTES).expect("header opens");
    trailing_reader.next_input().expect("tick reads");
    assert!(matches!(
        trailing_reader.next_input(),
        Err(JournalError::TrailingRecord)
    ));
}

#[test]
fn writer_failure_is_reported_and_cannot_produce_a_trusted_completion() {
    let path = TestPath::new("journal-writer-failure", "jsonl");
    // The bound must clear the session header, which carries one digest per
    // published spot contract document, and still be crossed by the padded
    // event below.
    let writer =
        SignalJournalWriter::create(path.as_path(), session("journal-test", 100), 8_192, 4)
            .expect("session fits the bounded journal");
    let mut large = bare_coinbase_event(0, "coinbase-test");
    large
        .values
        .insert("padding".to_owned(), json!("x".repeat(4_096)));
    writer
        .enqueue(market(0, large, 1, 1))
        .expect("bounded enqueue itself succeeds");
    assert!(matches!(
        writer.finish(terminal(1, 1, 0, 0, 0)),
        Err(JournalError::SizeLimit { .. })
    ));

    let mut reader = JournalReplayReader::open(path.as_path(), MAX_BYTES)
        .expect("durable session prefix remains readable");
    assert!(matches!(reader.next_input(), Err(JournalError::Incomplete)));
}

#[derive(serde::Deserialize)]
struct CrcFrame {
    c: String,
    r: Box<serde_json::value::RawValue>,
}

#[test]
fn every_schema5_line_is_a_crc_envelope_over_its_exact_record_bytes() {
    let path = complete_one_tick("crc-framed");
    let text = std::fs::read_to_string(path.as_path()).expect("journal reads");
    assert_eq!(text.lines().count(), 3, "session, one tick, and terminal");
    for line in text.lines() {
        let frame: CrcFrame = serde_json::from_str(line).expect("line is a CRC envelope");
        assert_eq!(frame.c.len(), 8, "CRC is eight hex characters");
        // The CRC covers exactly the serialized record bytes, verified against
        // an independent CRC-32C implementation in the test support module.
        assert_eq!(
            frame.c,
            support::crc32c_hex(frame.r.get().as_bytes()),
            "on-disk CRC matches the record bytes it frames"
        );
    }
}

#[test]
fn a_torn_tail_truncated_mid_record_is_reported_distinctly() {
    let path = complete_one_tick("torn-mid-record");
    let last = last_line(path.as_path());
    let keep = last.find(r#""r":"#).expect("envelope has a record field") + 5;
    truncate_last_line(path.as_path(), keep);

    let mut reader = JournalReplayReader::open(path.as_path(), MAX_BYTES).expect("session opens");
    reader.next_input().expect("intact health tick reads");
    assert!(matches!(reader.next_input(), Err(JournalError::TornTail)));
}

#[test]
fn a_torn_tail_truncated_mid_crc_is_reported_distinctly() {
    let path = complete_one_tick("torn-mid-crc");
    // Keep only the opening of the CRC field: `{"c":"ab`.
    truncate_last_line(path.as_path(), 8);

    let mut reader = JournalReplayReader::open(path.as_path(), MAX_BYTES).expect("session opens");
    reader.next_input().expect("intact health tick reads");
    assert!(matches!(reader.next_input(), Err(JournalError::TornTail)));
}

#[test]
fn an_interior_crc_mismatch_is_corruption_naming_its_line() {
    let path = write_complete(
        "interior-corruption",
        vec![
            JournalInput::HealthTick {
                admission_seq: 0,
                now_ns: 1,
            },
            JournalInput::HealthTick {
                admission_seq: 1,
                now_ns: 2,
            },
        ],
        terminal(2, 0, 0, 2, 0),
    );
    // Corrupt the CRC of the first data record (line 2) while later records
    // remain, so the failure is interior corruption, not a torn tail.
    flip_crc_char(path.as_path(), 1);

    let mut reader = JournalReplayReader::open(path.as_path(), MAX_BYTES).expect("session opens");
    assert!(matches!(
        reader.next_input(),
        Err(JournalError::Corruption { line: 2 })
    ));
}

#[test]
fn a_torn_first_line_that_began_as_an_envelope_is_a_torn_tail() {
    // A process that died mid-write on a fresh journal leaves a truncated
    // envelope as the only line. It begins with the schema-5 envelope prefix but
    // does not parse, and must be reported as a torn tail, not an opaque decode
    // error.
    let path = TestPath::new("journal-torn-first-line", "jsonl");
    std::fs::write(path.as_path(), br#"{"c":"deadbeef","r":{"record_ty"#)
        .expect("torn first line writes");
    assert!(matches!(
        JournalReplayReader::open(path.as_path(), MAX_BYTES),
        Err(JournalError::TornTail)
    ));
}

#[test]
fn a_valid_final_record_without_a_trailing_newline_is_a_torn_tail() {
    // The terminal record's bytes are intact and its CRC verifies, but the
    // terminating newline was lost to a torn write. A schema-5 line without its
    // newline is a torn tail: the write did not complete, so the journal is not a
    // trustworthy complete run.
    let path = complete_one_tick("no-terminal-newline");
    strip_trailing_newline(path.as_path());

    let mut reader = JournalReplayReader::open(path.as_path(), MAX_BYTES).expect("session opens");
    reader.next_input().expect("intact health tick reads");
    assert!(matches!(reader.next_input(), Err(JournalError::TornTail)));
}

/// Flip one hexadecimal character of the CRC on the 0-based `line_index` line so
/// its envelope still parses but its CRC no longer matches its record bytes.
fn flip_crc_char(path: &Path, line_index: usize) {
    let text = std::fs::read_to_string(path).expect("journal reads");
    let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
    let line = &lines[line_index];
    let crc_start = line.find(r#""c":""#).expect("envelope has a CRC field") + 5;
    let mut bytes = line.clone().into_bytes();
    bytes[crc_start] = if bytes[crc_start] == b'0' { b'1' } else { b'0' };
    lines[line_index] = String::from_utf8(bytes).expect("mutation stays UTF-8");
    let mut out = lines.join("\n");
    out.push('\n');
    std::fs::write(path, out).expect("corrupted journal writes");
}

#[test]
fn feed_health_capability_enters_the_fingerprint_and_stays_backward_identical() {
    let enabled = SpotReferenceRuntimeConfig::new(100).expect("enabled config");
    assert!(
        enabled.feed_health_stream,
        "a fresh session declares the capability"
    );
    let mut disabled = SpotReferenceRuntimeConfig::new(100).expect("disabled config");
    disabled.feed_health_stream = false;

    // Both configurations are valid: the flag is exempt from the exact-match check.
    enabled.validate().expect("enabled config validates");
    disabled.validate().expect("disabled config validates");

    // The capability changes the fingerprint, so a declaring session is
    // distinguishable from a pre-capability one.
    let enabled_fp = contract_fingerprint(&enabled).expect("enabled fingerprint");
    let disabled_fp = contract_fingerprint(&disabled).expect("disabled fingerprint");
    assert_ne!(enabled_fp.aggregate_sha256, disabled_fp.aggregate_sha256);
    assert_ne!(
        enabled_fp.runtime_config_sha256,
        disabled_fp.runtime_config_sha256
    );

    // A disabled (pre-capability) config serializes without the field, so its
    // bytes — and therefore its fingerprint — are identical to what a binary
    // predating the capability produced. This is what keeps retained-evidence
    // replays byte-identical.
    let disabled_json = serde_json::to_value(&disabled).expect("serializes");
    assert!(
        disabled_json.get("feed_health_stream").is_none(),
        "an absent-and-false capability must serialize to nothing"
    );
    let enabled_json = serde_json::to_value(&enabled).expect("serializes");
    assert_eq!(enabled_json.get("feed_health_stream"), Some(&json!(true)));

    // The recorded capability survives a journal session round-trip so replay can
    // gate emission on it.
    let session = JournalSession::new(
        "2026-08-13T12:00:00Z".to_owned(),
        "journal-fh".to_owned(),
        disabled.clone(),
    )
    .expect("disabled session constructs");
    let encoded = serde_json::to_vec(&session).expect("session serializes");
    let decoded: JournalSession = serde_json::from_slice(&encoded).expect("session deserializes");
    assert!(!decoded.runtime_config.feed_health_stream);
}

fn complete_one_tick(label: &str) -> TestPath {
    write_complete(
        label,
        vec![JournalInput::HealthTick {
            admission_seq: 0,
            now_ns: 1,
        }],
        terminal(1, 0, 0, 1, 0),
    )
}

fn write_complete(label: &str, inputs: Vec<JournalInput>, end: JournalEnd) -> TestPath {
    let path = TestPath::new(&format!("journal-{label}"), "jsonl");
    let writer =
        SignalJournalWriter::create(path.as_path(), session("journal-test", 100), MAX_BYTES, 16)
            .expect("journal creates");
    for input in inputs {
        writer.enqueue(input).expect("input enqueues");
    }
    writer.finish(end).expect("journal completes");
    path
}

fn mutate_first_record(path: &Path, mutate: impl FnOnce(&mut serde_json::Value)) {
    let text = std::fs::read_to_string(path).expect("journal reads");
    let mut lines = text.lines();
    let mut envelope: serde_json::Value =
        serde_json::from_str(lines.next().expect("session line exists")).expect("session parses");
    // Schema-5 journals wrap each record in a CRC envelope. Mutate the inner
    // record and re-frame it with a fresh, valid CRC so the reader reaches the
    // condition under test rather than rejecting the tampered CRC first.
    let record = envelope
        .get_mut("r")
        .expect("session line is a CRC envelope");
    mutate(record);
    let record_bytes = serde_json::to_vec(&*record).expect("record serializes");
    envelope["c"] = serde_json::Value::String(support::crc32c_hex(&record_bytes));
    let mut corrupted = serde_json::to_string(&envelope).expect("session serializes");
    corrupted.push('\n');
    for line in lines {
        corrupted.push_str(line);
        corrupted.push('\n');
    }
    std::fs::write(path, corrupted).expect("test corruption writes");
}

fn apply_inputs(
    processor: &mut SpotSignalProcessor,
    inputs: Vec<JournalInput>,
) -> Vec<signals_core::SignalUpdate> {
    let mut updates = Vec::new();
    for input in inputs {
        let batch = match input {
            JournalInput::MarketEvents {
                events,
                logical_receipt_ns,
                now_ns,
                ..
            } => {
                let mut batch = Vec::new();
                for event in events.iter() {
                    batch.extend(
                        processor
                            .ingest(event, logical_receipt_ns, now_ns)
                            .expect("market input applies"),
                    );
                }
                batch
            }
            JournalInput::Availability {
                venue,
                reason,
                now_ns,
                ..
            } => processor
                .mark_unavailable(venue, reason, now_ns)
                .expect("availability applies"),
            JournalInput::HealthTick { now_ns, .. } => {
                processor.advance_time(now_ns).expect("health tick applies")
            }
        };
        updates.extend(batch);
    }
    updates
}
