//! A6 lane-shell integration tests (ruling E24/E26): the oracle lane runs
//! feed-only through the whole shell, the rights-gated venues refuse when
//! unlisted, and every lane's checkpoint round-trips.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use btc_signals::exit_code::ExitClass;
use btc_signals::lane::{self, FixturePollDriver, LaneProcessor, PollDriver};
use btc_signals::lane_checkpoint::{DEFAULT_LANE_CHECKPOINT_MAX_BYTES, load_lane_checkpoint};
use btc_signals::lane_gate::{IndexSocket, admit_lane_with};
use btc_signals::lane_journal::{DEFAULT_LANE_JOURNAL_MAX_BYTES, LaneJournalReplayReader};
use btc_signals::lane_runtime::{
    LaneIndexGating, LaneRotation, LaneRunArguments, replay_lane, replay_lane_with_processor,
    run_lane, run_lane_with_processor, run_lane_with_processor_and_hook,
};
use btc_signals::stream_accounting::StreamHook;
use signals_core::{BINANCE_LANE, BYBIT_LANE, EnabledVenues, ORACLE_LANE};

fn temp_path(label: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock follows epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "venue-lane-{label}-{}-{unique}.jsonl",
        std::process::id()
    ))
}

fn frame_types(bytes: &[u8]) -> Vec<String> {
    bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .filter_map(|line| serde_json::from_slice::<serde_json::Value>(line).ok())
        .filter_map(|value| {
            value
                .get("type")
                .and_then(|kind| kind.as_str())
                .map(str::to_owned)
        })
        .collect()
}

fn enabled(raw: &str) -> EnabledVenues {
    EnabledVenues::parse(Some(raw)).expect("allow-list parses")
}

/// Explicit disarmed gating: no index token, no credentials. The composite oracle
/// lane omits both index streams and runs the Chainlink poll alone — the poll-only
/// behaviour these oracle tests (standalone and rotation) assert, pinned so it
/// never depends on the host env (`SIGNALS_ENABLED_VENUES`/`PYTH_API_KEY`/Kalshi
/// credentials). The composite streams-ACTIVE path is covered deterministically by
/// the dedicated `composite_oracle_lane_*_armed*` tests below (which inject an
/// armed gating directly), so every path here is host-env-independent.
fn disarmed_index_gating() -> LaneIndexGating {
    LaneIndexGating {
        enabled: EnabledVenues::parse(None).expect("allow-list parses"),
        hermes_keyed: false,
        kalshi_index_credentialed: false,
    }
}

/// Explicit armed gating: both index families listed, keyed, and the Kalshi index
/// credentials present. The composite oracle lane drives its Kalshi index and Pyth
/// Hermes streams (through the injected fixture runner — no host env, no network),
/// so `run_venue_rotation` exercises the composite streams ACTIVE across generation
/// boundaries. A non-composite (Streams) lane ignores it — its driver opens no
/// index socket — so the same helper stays correct for binance/bybit.
fn armed_index_gating() -> LaneIndexGating {
    LaneIndexGating {
        enabled: EnabledVenues::parse(Some("cfbrti,pyth")).expect("allow-list parses"),
        hermes_keyed: true,
        kalshi_index_credentialed: true,
    }
}

#[test]
fn rights_gated_lanes_refuse_unlisted_and_admit_once_listed() {
    // R7: binance and bybit refuse (exit 78) until listed; oracle always admits.
    for requirement in [&BINANCE_LANE, &BYBIT_LANE] {
        assert_eq!(
            admit_lane_with(
                requirement,
                IndexSocket::NotConfigured,
                &enabled("coinbase,kraken")
            ),
            Err(ExitClass::Fatal),
            "{} refuses when unlisted",
            requirement.lane
        );
    }
    assert_eq!(ExitClass::Fatal.code(), 78);
    let listed = enabled("coinbase,kraken,binance,bybit");
    for requirement in [&BINANCE_LANE, &BYBIT_LANE] {
        assert!(
            admit_lane_with(requirement, IndexSocket::NotConfigured, &listed).is_ok(),
            "{} admits once listed",
            requirement.lane
        );
    }
    // Oracle admits even with the allow-list naming nothing relevant.
    assert!(
        admit_lane_with(
            &ORACLE_LANE,
            IndexSocket::NotConfigured,
            &enabled("coinbase")
        )
        .is_ok()
    );
}

#[tokio::test(flavor = "current_thread")]
async fn oracle_lane_runs_feed_only_reaches_ready_checkpoints_and_replays() {
    let output = temp_path("oracle-output");
    let journal = temp_path("oracle-journal");
    let checkpoint = temp_path("oracle-checkpoint");
    let replay_output = temp_path("oracle-replay");

    let driver: Box<dyn PollDriver> = Box::new(FixturePollDriver::new(
        "chainlink_aggregator",
        Duration::from_millis(20),
        0,
    ));
    let (report, shutdown_reason) = run_lane(LaneRunArguments {
        index_gating: Some(disarmed_index_gating()),
        spec: &lane::ORACLE,
        output: output.clone(),
        journal: journal.clone(),
        checkpoint: checkpoint.clone(),
        resume_from_checkpoint: false,
        publisher_epoch: "oracle-a6-test".to_owned(),
        duration_seconds: 2,
        checkpoint_interval_seconds: 1,
        max_updates: 10_000,
        max_output_bytes: 4_194_304,
        max_journal_bytes: DEFAULT_LANE_JOURNAL_MAX_BYTES,
        max_checkpoint_bytes: DEFAULT_LANE_CHECKPOINT_MAX_BYTES,
        poll_driver: Some(driver),
        // The oracle lane is composite (C1): its Chainlink poll runs through the
        // injected `poll_driver`, and its Kalshi index and Pyth Hermes streams
        // through this fixture runner (connect, emit nothing). The streams are
        // observed-not-consumed, so the Chainlink reading is byte-identical to the
        // poll-only lane while the composite run/replay paths are exercised.
        stream_runner: Some(Box::new(FixtureStreamRunner)),
        rotation: None,
    })
    .await
    .expect("oracle lane runs to its natural deadline");
    assert!(
        !shutdown_reason.interrupted(),
        "reached the natural deadline"
    );

    // A feed-only lane publishes no signal batch, only control and feed_health.
    assert_eq!(report.pipeline.signal_batches_written, 0);
    assert_eq!(report.pipeline.reset_messages_written, 1);
    assert!(
        report.pipeline.feed_health_messages_written > 0,
        "the lane emits feed_health frames"
    );

    let bytes = std::fs::read(&output).expect("output reads");
    let types = frame_types(&bytes);
    assert_eq!(types.first().map(String::as_str), Some("reset"));
    assert!(types.iter().any(|kind| kind == "feed_health"));
    assert!(types.iter().any(|kind| kind == "status"));

    // The fixture poll driver serves no rounds, so the value-emitting oracle
    // processor never accepts one: it stays warming and publishes no signal
    // batch, only control and feed_health.
    assert_eq!(report.pipeline.signal_batches_written, 0);
    let ready = bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .filter_map(|line| serde_json::from_slice::<serde_json::Value>(line).ok())
        .any(|value| {
            value.get("type").and_then(|k| k.as_str()) == Some("status")
                && value.get("status").and_then(|s| s.as_str()) == Some("ready")
        });
    assert!(
        !ready,
        "with no rounds served the oracle lane stays warming"
    );

    // The checkpoint round-trips: it loads and restores into the real processor.
    let document = load_lane_checkpoint(&checkpoint, DEFAULT_LANE_CHECKPOINT_MAX_BYTES)
        .expect("checkpoint loads");
    assert_eq!(document.processor_id, "btc.oracle");
    let mut restored = lane::ORACLE
        .build_processor("oracle-a6-test-resume", chrono::DateTime::UNIX_EPOCH)
        .expect("the oracle processor builds");
    restored
        .restore_state(document.processor_state.clone())
        .expect("checkpoint state restores");

    // The journal replays byte-identically to the live output (m1: bytes, not a
    // count).
    let _replay_report = replay_lane(
        &lane::ORACLE,
        &journal,
        &replay_output,
        10_000,
        4_194_304,
        DEFAULT_LANE_JOURNAL_MAX_BYTES,
    )
    .expect("oracle journal replays");
    let replay_bytes = std::fs::read(&replay_output).expect("replay output reads");
    assert_eq!(
        bytes, replay_bytes,
        "the replay output is byte-identical to the live output"
    );

    for path in [output, journal, checkpoint, replay_output] {
        let _ = std::fs::remove_file(path);
    }
}

/// M4: a resume whose checkpoint was written by a different processor id OR
/// version is refused before any socket opens.
#[test]
fn resume_refuses_a_processor_id_or_version_mismatch() {
    use btc_signals::lane_checkpoint::LaneCheckpointDocument;
    use btc_signals::lane_journal::{LaneIdentity, LaneRuntimeConfig, lane_contract_fingerprint};
    use btc_signals::lane_runtime::ensure_resume_matches_processor;

    let runtime_config = LaneRuntimeConfig::new("oracle", 1_000_000_000);
    let fingerprint = lane_contract_fingerprint(&runtime_config).expect("fingerprint builds");
    let checkpoint = LaneCheckpointDocument::new(
        "btc.oracle.feed_only".to_owned(),
        "1.0.0".to_owned(),
        "0123456789abcdef0123456789abcdef".to_owned(),
        "2026-09-03T00:00:00Z".to_owned(),
        0,
        0,
        fingerprint,
        runtime_config,
        serde_json::json!({"live_streams": [], "ever_ready": false}),
        vec!["oracle-epoch".to_owned()],
    )
    .expect("checkpoint builds");

    let base = LaneIdentity {
        lane: "oracle",
        venues: vec!["oracle".to_owned()],
        processor_id: "btc.oracle.feed_only".to_owned(),
        processor_version: "1.0.0".to_owned(),
    };
    ensure_resume_matches_processor(&checkpoint, &base).expect("matching processor resumes");

    let wrong_id = LaneIdentity {
        processor_id: "btc.oracle.signals".to_owned(),
        ..base.clone()
    };
    assert!(
        ensure_resume_matches_processor(&checkpoint, &wrong_id).is_err(),
        "a processor id mismatch refuses resume"
    );

    let wrong_version = LaneIdentity {
        processor_version: "2.0.0".to_owned(),
        ..base
    };
    assert!(
        ensure_resume_matches_processor(&checkpoint, &wrong_version).is_err(),
        "a processor version mismatch refuses resume"
    );
}

/// A journal recorded by one lane cannot be replayed through another lane's
/// `--lane` spec: the guard fires before any processor is built, with a Fatal
/// (exit 78) message that names the mismatch, so a wrong-lane rebuild is never
/// emitted as an authoritative replay.
#[tokio::test(flavor = "current_thread")]
async fn replay_refuses_a_journal_recorded_by_a_different_lane() {
    let output = temp_path("mismatch-output");
    let journal = temp_path("mismatch-journal");
    let checkpoint = temp_path("mismatch-checkpoint");
    let driver: Box<dyn PollDriver> = Box::new(FixturePollDriver::new(
        "chainlink_aggregator",
        Duration::from_millis(20),
        0,
    ));
    run_lane(LaneRunArguments {
        index_gating: Some(disarmed_index_gating()),
        spec: &lane::ORACLE,
        output: output.clone(),
        journal: journal.clone(),
        checkpoint: checkpoint.clone(),
        resume_from_checkpoint: false,
        publisher_epoch: "mismatch-oracle".to_owned(),
        duration_seconds: 2,
        checkpoint_interval_seconds: 1,
        max_updates: 10_000,
        max_output_bytes: 4_194_304,
        max_journal_bytes: DEFAULT_LANE_JOURNAL_MAX_BYTES,
        max_checkpoint_bytes: DEFAULT_LANE_CHECKPOINT_MAX_BYTES,
        poll_driver: Some(driver),
        // The oracle lane is composite (C1): its Chainlink poll runs through the
        // injected `poll_driver`, and its Kalshi index and Pyth Hermes streams
        // through this fixture runner (connect, emit nothing). The streams are
        // observed-not-consumed, so the Chainlink reading is byte-identical to the
        // poll-only lane while the composite run/replay paths are exercised.
        stream_runner: Some(Box::new(FixtureStreamRunner)),
        rotation: None,
    })
    .await
    .expect("the oracle lane produces a journal");

    // Replaying the oracle journal through the Binance lane spec is refused.
    let mismatch_replay = temp_path("mismatch-replay");
    let error = replay_lane(
        &lane::BINANCE,
        &journal,
        &mismatch_replay,
        10_000,
        4_194_304,
        DEFAULT_LANE_JOURNAL_MAX_BYTES,
    )
    .expect_err("a wrong-lane replay is refused");
    assert!(
        error
            .to_string()
            .contains("does not match the --lane binance"),
        "the message names the lane mismatch: {error}"
    );
    assert_eq!(
        btc_signals::exit_code::exit_class_for(&error),
        ExitClass::Fatal,
        "a wrong-lane replay is a fatal configuration error (exit 78)"
    );

    for path in [output, journal, checkpoint] {
        let _ = std::fs::remove_file(path);
    }
}

// --- Chainlink oracle driver (A5 transport through the A6 seam) -------------

/// The three `eth_call` reply shapes A5's tests build, reproduced here so the
/// oracle lane runs end to end over a fixture transport without a network.
fn decimals_reply(value: u128) -> String {
    format!(r#"{{"jsonrpc":"2.0","id":1,"result":"0x{value:064x}"}}"#)
}

fn description_reply(text: &str) -> String {
    let mut data_hex = String::new();
    for byte in text.as_bytes() {
        data_hex.push_str(&format!("{byte:02x}"));
    }
    while !data_hex.len().is_multiple_of(64) {
        data_hex.push('0');
    }
    format!(
        r#"{{"jsonrpc":"2.0","id":1,"result":"0x{:064x}{:064x}{data_hex}"}}"#,
        32u128,
        text.len() as u128
    )
}

fn round_reply(rid: u128, secs: u128) -> String {
    format!(
        r#"{{"jsonrpc":"2.0","id":1,"result":"0x{rid:064x}{:064x}{secs:064x}{secs:064x}{rid:064x}"}}"#,
        7_766_779_700_000u128
    )
}

/// A fixture transport that verifies the BTC/USD target and then serves a fresh
/// advancing round on every latest-round poll (so each poll admits one event),
/// starting from `start`.
fn serving_transport_from(start: u128) -> btc_feeds::live::ChainlinkPollTransport<'static> {
    let mut round: u128 = start;
    Box::new(move |_endpoint: String, body: String| {
        let reply = if body.contains("313ce567") {
            decimals_reply(8)
        } else if body.contains("7284e416") {
            description_reply("BTC / USD")
        } else {
            round += 1;
            round_reply(round, 1_756_400_000 + round)
        };
        Box::pin(async move { Some(reply) })
    })
}

fn serving_transport() -> btc_feeds::live::ChainlinkPollTransport<'static> {
    serving_transport_from(100)
}

/// A scripted poll driver that yields one advancing round per poll with a
/// near-now timestamp, and never a deadline outage. It drives the oracle lane's
/// restart-equivalence test on the processor alone, independent of the real
/// Chainlink driver's start-up/deadline machinery (whose logical deadline is
/// measured from run start and so does not compose with a resumed clock — an
/// A5/A6 driver-seam gap tracked separately).
struct ScriptedRoundDriver {
    next_round: u128,
    base_secs: u128,
    /// When false the driver re-serves the SAME round every poll: over one run the
    /// change key never moves (frame-level coalescing), and after a resume the
    /// first poll re-serves the current round (the F1 same-round condition).
    advance: bool,
}

impl PollDriver for ScriptedRoundDriver {
    fn stream_label(&self) -> &'static str {
        "chainlink_aggregator"
    }
    fn poll_interval(&self) -> Duration {
        Duration::from_millis(20)
    }
    fn poll<'a>(
        &'a mut self,
        _now_ns: u64,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<lane::PollBatch, lane::PollDriverError>> + 'a>,
    > {
        let round = self.next_round;
        if self.advance {
            self.next_round += 1;
        }
        let secs = self.base_secs + round;
        let updated_at = chrono::DateTime::from_timestamp(secs as i64, 0)
            .expect("timestamp is representable")
            .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        let event = signals_core::SourceEvent {
            schema_version: 1,
            source_id: "chainlink.ethereum.btc-usd.rounds".to_owned(),
            source_version: "1.0.0".to_owned(),
            publisher_epoch: "scripted".to_owned(),
            sequence: 0,
            event_time: updated_at.clone(),
            received_at: updated_at.clone(),
            values: std::collections::BTreeMap::from([
                ("round_id".to_owned(), serde_json::json!(round.to_string())),
                ("phase_id".to_owned(), serde_json::json!(0u64)),
                ("answer".to_owned(), serde_json::json!("77667.79700000")),
                ("updated_at".to_owned(), serde_json::json!(updated_at)),
            ]),
        };
        Box::pin(async move {
            Ok(lane::PollBatch {
                connected: true,
                events: vec![event],
                notices: Vec::new(),
            })
        })
    }
}

fn oracle_poll_options(duration_seconds: u64) -> btc_feeds::live::ChainlinkPollOptions {
    btc_feeds::live::ChainlinkPollOptions {
        endpoints: vec![
            "https://a.example.invalid".to_owned(),
            "https://b.example.invalid".to_owned(),
        ],
        duration: Duration::from_secs(duration_seconds),
        publisher_epoch: "oracle-chainlink-test".to_owned(),
        shutdown: None,
    }
}

/// The A5 Chainlink driver, through the A6 seam, drives the oracle lane to ready
/// over a fixture transport, and the journal replays byte-identically — no
/// network. This is the merge A6 was blocked on: the fixture driver is gone from
/// the production path and the real transport runs behind the same seam.
#[tokio::test(flavor = "current_thread")]
async fn oracle_lane_runs_through_the_chainlink_driver_and_replays() {
    use btc_signals::lane::ChainlinkPollDriver;

    let output = temp_path("oracle-cl-output");
    let journal = temp_path("oracle-cl-journal");
    let checkpoint = temp_path("oracle-cl-checkpoint");
    let replay_output = temp_path("oracle-cl-replay");

    let driver: Box<dyn PollDriver> = Box::new(
        ChainlinkPollDriver::with_transport(
            "chainlink_aggregator",
            oracle_poll_options(3),
            Duration::from_millis(20),
            serving_transport(),
        )
        .expect("driver builds"),
    );
    let (report, shutdown_reason) = run_lane(LaneRunArguments {
        index_gating: Some(disarmed_index_gating()),
        spec: &lane::ORACLE,
        output: output.clone(),
        journal: journal.clone(),
        checkpoint: checkpoint.clone(),
        resume_from_checkpoint: false,
        publisher_epoch: "oracle-chainlink-test".to_owned(),
        duration_seconds: 3,
        checkpoint_interval_seconds: 1,
        max_updates: 100_000,
        max_output_bytes: 8_388_608,
        max_journal_bytes: DEFAULT_LANE_JOURNAL_MAX_BYTES,
        max_checkpoint_bytes: DEFAULT_LANE_CHECKPOINT_MAX_BYTES,
        poll_driver: Some(driver),
        // The oracle lane is composite (C1): its Chainlink poll runs through the
        // injected `poll_driver`, and its Kalshi index and Pyth Hermes streams
        // through this fixture runner (connect, emit nothing). The streams are
        // observed-not-consumed, so the Chainlink reading is byte-identical to the
        // poll-only lane while the composite run/replay paths are exercised.
        stream_runner: Some(Box::new(FixtureStreamRunner)),
        rotation: None,
    })
    .await
    .expect("oracle lane runs to its natural deadline over the fixture transport");
    assert!(
        !shutdown_reason.interrupted(),
        "reached the natural deadline"
    );
    // The value-emitting oracle processor now publishes btc.chainlink.oracle_reference
    // once verification completes and rounds are served.
    assert!(
        report.pipeline.signal_batches_written > 0,
        "the oracle lane publishes signal batches over the Chainlink driver"
    );

    // The lane reaches ready once verification completes and a round is served.
    let bytes = std::fs::read(&output).expect("output reads");
    let ready = bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .filter_map(|line| serde_json::from_slice::<serde_json::Value>(line).ok())
        .any(|value| {
            value.get("type").and_then(|k| k.as_str()) == Some("status")
                && value.get("status").and_then(|s| s.as_str()) == Some("ready")
        });
    assert!(
        ready,
        "the oracle lane reaches ready over the Chainlink driver"
    );

    // A signal_batch carries the identity's ready reading: the fixture serves the
    // 7766779700000 answer, which renders "77667.79700000", fresh, with the round
    // and phase carried alongside.
    let update = bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .filter_map(|line| serde_json::from_slice::<serde_json::Value>(line).ok())
        .filter(|value| value.get("type").and_then(|k| k.as_str()) == Some("signal_batch"))
        .filter_map(|value| value.get("updates").and_then(|u| u.as_array()).cloned())
        .flatten()
        .find(|update| {
            update.get("signal_id").and_then(|s| s.as_str())
                == Some("btc.chainlink.oracle_reference")
        })
        .expect("a chainlink oracle-reference update is published");
    let values = update
        .get("values")
        .and_then(|v| v.as_object())
        .expect("values");
    assert_eq!(
        values.get("value").and_then(|v| v.as_str()),
        Some("77667.79700000")
    );
    // The fixture serves a round whose `updated_at` predates the run, so the
    // reference correctly publishes `freshness: stale` — staleness is a published
    // value on an oracle reference, not a status (B3 §6).
    assert_eq!(
        values.get("freshness").and_then(|v| v.as_str()),
        Some("stale")
    );
    assert!(
        values
            .get("staleness_ms")
            .and_then(|v| v.as_u64())
            .is_some()
    );
    assert_eq!(
        values.get("heartbeat_seconds").and_then(|v| v.as_u64()),
        Some(3600)
    );
    assert!(
        values.contains_key("round_id"),
        "the round id rides on the update"
    );
    assert!(
        values.contains_key("phase_id"),
        "the phase id rides on the update"
    );
    assert_eq!(
        update
            .get("scope")
            .and_then(|s| s.get("type"))
            .and_then(|t| t.as_str()),
        Some("global"),
        "the oracle reference is globally scoped"
    );

    let _ = replay_lane(
        &lane::ORACLE,
        &journal,
        &replay_output,
        100_000,
        8_388_608,
        DEFAULT_LANE_JOURNAL_MAX_BYTES,
    )
    .expect("oracle journal replays");
    let replay_bytes = std::fs::read(&replay_output).expect("replay output reads");
    assert_eq!(
        bytes, replay_bytes,
        "the replay output is byte-identical to the live output"
    );

    // M1: the start-up call notices (the epoch-open Connected and the decimals /
    // description replies) are journaled as evidence — a first-tick abort keeps a
    // trail — and replay through them stays byte-identical (above).
    let journal_text =
        String::from_utf8_lossy(&std::fs::read(&journal).expect("journal reads")).into_owned();
    assert!(
        journal_text.contains("\"record_type\":\"notice\""),
        "the lane journal records the driver's start-up notices"
    );

    for path in [output, journal, checkpoint, replay_output] {
        let _ = std::fs::remove_file(path);
    }
}

/// C1 inventory: the composite oracle lane is the Chainlink poll (its one
/// consumed source) plus the Kalshi index and Pyth Hermes streams, whose six
/// sources are observed-not-consumed in phase 1 and disjoint from the poll's.
#[test]
fn composite_oracle_lane_inventory_is_chainlink_poll_plus_index_streams() {
    use btc_signals::lane::LaneDriverSpec;
    let LaneDriverSpec::Composite {
        poll_sources,
        streams,
        ..
    } = &lane::ORACLE.driver
    else {
        panic!("the oracle lane is composite (C1)");
    };
    assert_eq!(
        *poll_sources,
        [btc_signals::oracle_signals::CHAINLINK_SOURCE_ID],
        "the poll produces exactly the Chainlink round source"
    );
    let produced: std::collections::BTreeSet<&str> = streams
        .iter()
        .flat_map(|stream| stream.stream.produced_source_ids().iter().copied())
        .collect();
    assert_eq!(
        produced,
        std::collections::BTreeSet::from([
            "kalshi.cfbenchmarks.brti.value",
            "kalshi.cfbenchmarks.brti.value-5hz",
            "kalshi.cfbenchmarks.brti.settlement-window",
            "kalshi.cfbenchmarks.brti.history",
            "kalshi.pyth.btc-usd.value",
            "pyth.hermes.btc-usd.price",
        ]),
        "the index streams produce the six phase-1 observed-not-consumed sources"
    );
    assert!(
        !produced.contains(btc_signals::oracle_signals::CHAINLINK_SOURCE_ID),
        "the consumed poll source is never among the observed-not-consumed stream sources"
    );
}

/// C1: the composite oracle lane drives its Chainlink poll AND the Kalshi index +
/// Pyth Hermes streams concurrently over bounded observed excerpts. The Kalshi
/// index frames are counted (`ignored_events`) and routed past the computation,
/// while the Pyth Hermes frames are now CONSUMED and publish
/// `btc.pyth.oracle_reference`; the `btc.chainlink.oracle_reference` reading stays
/// byte-identical to the poll-only lane, and the whole run replays byte-identically.
#[tokio::test(flavor = "current_thread")]
async fn composite_oracle_lane_counts_index_streams_ignored_and_replays() {
    use btc_signals::lane::ChainlinkPollDriver;

    let output = temp_path("oracle-composite-output");
    let journal = temp_path("oracle-composite-journal");
    let checkpoint = temp_path("oracle-composite-checkpoint");
    let replay_output = temp_path("oracle-composite-replay");

    let driver: Box<dyn PollDriver> = Box::new(
        ChainlinkPollDriver::with_transport(
            "chainlink_aggregator",
            oracle_poll_options(3),
            Duration::from_millis(20),
            serving_transport(),
        )
        .expect("driver builds"),
    );
    let (report, shutdown_reason) = run_lane(LaneRunArguments {
        // Armed for cfbrti+pyth and keyed so both index streams drive under
        // the gate; the ReplayFixtureRunner then delivers their observed frames.
        index_gating: Some(LaneIndexGating {
            enabled: EnabledVenues::parse(Some("cfbrti,pyth")).unwrap(),
            hermes_keyed: true,
            kalshi_index_credentialed: true,
        }),
        spec: &lane::ORACLE,
        output: output.clone(),
        journal: journal.clone(),
        checkpoint: checkpoint.clone(),
        resume_from_checkpoint: false,
        publisher_epoch: "oracle-composite-test".to_owned(),
        duration_seconds: 3,
        checkpoint_interval_seconds: 1,
        max_updates: 100_000,
        max_output_bytes: 8_388_608,
        max_journal_bytes: DEFAULT_LANE_JOURNAL_MAX_BYTES,
        max_checkpoint_bytes: DEFAULT_LANE_CHECKPOINT_MAX_BYTES,
        poll_driver: Some(driver),
        // The Kalshi index and Pyth Hermes excerpts replay through the real
        // ReplaySession, so the composite lane sees genuine index frames.
        stream_runner: Some(Box::new(ReplayFixtureRunner {
            foreign_source: None,
        })),
        rotation: None,
    })
    .await
    .expect("the composite oracle lane runs to its deadline over poll + index excerpts");
    assert!(
        !shutdown_reason.interrupted(),
        "reached the natural deadline"
    );

    // The Kalshi index frames are observed-not-consumed: counted, never consumed
    // by the oracle computation. The Pyth Hermes frames are now CONSUMED (C1), so
    // they are NOT among the ignored counts.
    let ignored = &report.pipeline.feed_health.ignored_events;
    assert!(
        ignored
            .get("kalshi.cfbenchmarks.brti.value")
            .is_some_and(|count| *count > 0),
        "the Kalshi CF BRTI value frames are counted observed-not-consumed, got {ignored:?}"
    );
    assert!(
        !ignored.contains_key("pyth.hermes.btc-usd.price"),
        "the Pyth Hermes price frames are consumed, not observed-not-consumed, got {ignored:?}"
    );

    // The Chainlink reading is byte-identical to the poll-only lane: the same
    // 77667.79700000 answer the driver-only test asserts, unperturbed by the
    // index streams sharing the lane.
    let bytes = std::fs::read(&output).expect("output reads");
    let update = bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .filter_map(|line| serde_json::from_slice::<serde_json::Value>(line).ok())
        .filter(|value| value.get("type").and_then(|k| k.as_str()) == Some("signal_batch"))
        .filter_map(|value| value.get("updates").and_then(|u| u.as_array()).cloned())
        .flatten()
        .find(|update| {
            update.get("signal_id").and_then(|s| s.as_str())
                == Some("btc.chainlink.oracle_reference")
        })
        .expect("a chainlink oracle-reference update is published on the composite lane");
    assert_eq!(
        update
            .get("values")
            .and_then(|v| v.as_object())
            .and_then(|values| values.get("value"))
            .and_then(|v| v.as_str()),
        Some("77667.79700000"),
        "the Chainlink reading is byte-identical to the poll-only lane"
    );

    // The consumed Pyth Hermes stream publishes a ready btc.pyth.oracle_reference
    // decision carrying its value (freshness is a published measurement, so a
    // reading whose replayed publish time is old still publishes ready).
    let pyth_update = bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .filter_map(|line| serde_json::from_slice::<serde_json::Value>(line).ok())
        .filter(|value| value.get("type").and_then(|k| k.as_str()) == Some("signal_batch"))
        .filter_map(|value| value.get("updates").and_then(|u| u.as_array()).cloned())
        .flatten()
        .find(|update| {
            update.get("signal_id").and_then(|s| s.as_str()) == Some("btc.pyth.oracle_reference")
        })
        .expect("a pyth oracle-reference update is published on the composite lane");
    assert!(
        pyth_update
            .get("values")
            .and_then(|v| v.as_object())
            .and_then(|values| values.get("value"))
            .and_then(|v| v.as_str())
            .is_some_and(|value| !value.is_empty()),
        "the Pyth reference carries a value"
    );

    // The whole composite run — poll frames, index frames, feed health — replays
    // byte-identically.
    let _ = replay_lane(
        &lane::ORACLE,
        &journal,
        &replay_output,
        100_000,
        8_388_608,
        DEFAULT_LANE_JOURNAL_MAX_BYTES,
    )
    .expect("composite oracle journal replays");
    let replay_bytes = std::fs::read(&replay_output).expect("replay output reads");
    assert_eq!(
        bytes, replay_bytes,
        "the composite lane replay output is byte-identical to the live output"
    );

    for path in [output, journal, checkpoint, replay_output] {
        let _ = std::fs::remove_file(path);
    }
}

/// C1/B1-B2: with the index sockets disarmed and the Hermes stream unkeyed
/// (injected gating — no host env, no dial), the composite oracle lane omits both
/// index streams and runs the Chainlink poll ALONE, still reaching ready and
/// writing its session record. A missing token/credential can never take the
/// Chainlink reading down.
#[tokio::test(flavor = "current_thread")]
async fn composite_oracle_lane_runs_the_poll_alone_when_disarmed() {
    use btc_signals::lane::ChainlinkPollDriver;

    let output = temp_path("oracle-disarmed-output");
    let journal = temp_path("oracle-disarmed-journal");
    let checkpoint = temp_path("oracle-disarmed-checkpoint");

    let driver: Box<dyn PollDriver> = Box::new(
        ChainlinkPollDriver::with_transport(
            "chainlink_aggregator",
            oracle_poll_options(3),
            Duration::from_millis(20),
            serving_transport(),
        )
        .expect("driver builds"),
    );
    let (report, _) = run_lane(LaneRunArguments {
        index_gating: Some(LaneIndexGating {
            enabled: EnabledVenues::parse(None).unwrap(),
            hermes_keyed: false,
            kalshi_index_credentialed: false,
        }),
        spec: &lane::ORACLE,
        output: output.clone(),
        journal: journal.clone(),
        checkpoint: checkpoint.clone(),
        resume_from_checkpoint: false,
        publisher_epoch: "oracle-disarmed".to_owned(),
        duration_seconds: 3,
        checkpoint_interval_seconds: 1,
        max_updates: 100_000,
        max_output_bytes: 8_388_608,
        max_journal_bytes: DEFAULT_LANE_JOURNAL_MAX_BYTES,
        max_checkpoint_bytes: DEFAULT_LANE_CHECKPOINT_MAX_BYTES,
        poll_driver: Some(driver),
        // A fixture stream runner is supplied but must never be invoked: both
        // index streams are omitted, so no stream worker is built.
        stream_runner: Some(Box::new(FixtureStreamRunner)),
        rotation: None,
    })
    .await
    .expect("the disarmed composite lane runs the poll alone without error");

    assert!(
        report.live_reports.is_empty(),
        "no stream worker runs when both index streams are omitted, got {:?}",
        report.live_reports
    );
    assert!(
        report.poll_cycles.is_some_and(|cycles| cycles > 0),
        "the Chainlink poll runs alone"
    );
    // The lane still wrote its session record (journal) and reached ready.
    let journal_bytes = std::fs::read(&journal).expect("journal reads");
    assert!(
        !journal_bytes.is_empty(),
        "the lane writes its session record"
    );
    let ready = std::fs::read(&output)
        .expect("output reads")
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .filter_map(|line| serde_json::from_slice::<serde_json::Value>(line).ok())
        .any(|value| value.get("status").and_then(|s| s.as_str()) == Some("ready"));
    assert!(ready, "the oracle lane reaches ready on the poll alone");

    for path in [output, journal, checkpoint] {
        let _ = std::fs::remove_file(path);
    }
}

/// C1/B1-B2: armed for cfbrti+pyth and keyed, the composite lane drives all three
/// feeds — the Chainlink poll plus the two index-stream workers (fixture runner,
/// no network). Injected gating; no host env read.
#[tokio::test(flavor = "current_thread")]
async fn composite_oracle_lane_declares_all_three_feeds_when_armed_and_keyed() {
    use btc_signals::lane::ChainlinkPollDriver;

    let output = temp_path("oracle-armed-output");
    let journal = temp_path("oracle-armed-journal");
    let checkpoint = temp_path("oracle-armed-checkpoint");

    let driver: Box<dyn PollDriver> = Box::new(
        ChainlinkPollDriver::with_transport(
            "chainlink_aggregator",
            oracle_poll_options(3),
            Duration::from_millis(20),
            serving_transport(),
        )
        .expect("driver builds"),
    );
    let (report, _) = run_lane(LaneRunArguments {
        index_gating: Some(LaneIndexGating {
            enabled: EnabledVenues::parse(Some("cfbrti,pyth")).unwrap(),
            hermes_keyed: true,
            kalshi_index_credentialed: true,
        }),
        spec: &lane::ORACLE,
        output: output.clone(),
        journal: journal.clone(),
        checkpoint: checkpoint.clone(),
        resume_from_checkpoint: false,
        publisher_epoch: "oracle-armed".to_owned(),
        duration_seconds: 3,
        checkpoint_interval_seconds: 1,
        max_updates: 100_000,
        max_output_bytes: 8_388_608,
        max_journal_bytes: DEFAULT_LANE_JOURNAL_MAX_BYTES,
        max_checkpoint_bytes: DEFAULT_LANE_CHECKPOINT_MAX_BYTES,
        poll_driver: Some(driver),
        stream_runner: Some(Box::new(FixtureStreamRunner)),
        rotation: None,
    })
    .await
    .expect("the armed+keyed composite lane runs poll + both index streams");

    assert_eq!(
        report.live_reports.len(),
        2,
        "both index-stream workers run alongside the poll (kalshi_index + pyth_hermes)"
    );
    assert!(
        report.poll_cycles.is_some_and(|cycles| cycles > 0),
        "the Chainlink poll runs alongside the index streams"
    );

    for path in [output, journal, checkpoint] {
        let _ = std::fs::remove_file(path);
    }
}

/// The oracle poll options over an explicit endpoint list.
fn oracle_poll_options_over(
    endpoints: Vec<&str>,
    duration_seconds: u64,
) -> btc_feeds::live::ChainlinkPollOptions {
    btc_feeds::live::ChainlinkPollOptions {
        endpoints: endpoints.into_iter().map(str::to_owned).collect(),
        duration: Duration::from_secs(duration_seconds),
        publisher_epoch: "oracle-chainlink-test".to_owned(),
        shutdown: None,
    }
}

/// B1/M1, at production cadence: the driver's first `poll()` runs the whole
/// start-up sequence eagerly and the first round poll, so it verifies and emits
/// the first event within the first tick, and a healthy 100 s run never trips
/// the whole-run-outage exit.
#[tokio::test(flavor = "current_thread")]
async fn oracle_driver_verifies_and_emits_within_the_first_tick() {
    use btc_signals::lane::ChainlinkPollDriver;

    let mut driver = ChainlinkPollDriver::with_transport(
        "chainlink_aggregator",
        oracle_poll_options(100),
        Duration::from_secs(60),
        serving_transport(),
    )
    .expect("driver builds");

    // M1: the first tick (t=0) verifies and admits the first round event.
    let batch = driver.poll(0).await.expect("the first tick succeeds");
    assert!(batch.connected, "verified and served within the first tick");
    assert!(
        !batch.events.is_empty(),
        "the first round event lands in the first tick"
    );
    // A healthy 100 s run at 60 s cadence never reports a whole-run outage.
    for now_ns in [60_000_000_000u64, 120_000_000_000] {
        driver
            .poll(now_ns)
            .await
            .expect("a verified run never exits transient");
    }
}

/// B1: [down, down, healthy] verifies within the first tick (the eager start-up
/// rotates past the two dead endpoints to the healthy one) — never a whole-run
/// outage, even though verification would have needed three ticks one-per-poll.
#[tokio::test(flavor = "current_thread")]
async fn oracle_driver_verifies_within_the_first_tick_over_a_recovering_endpoint_set() {
    use btc_signals::lane::ChainlinkPollDriver;

    // Only the third endpoint answers; the first two always fault.
    let healthy = "https://c.example.invalid".to_owned();
    let mut round: u128 = 200;
    let transport: btc_feeds::live::ChainlinkPollTransport<'static> =
        Box::new(move |endpoint: String, body: String| {
            let reply = if endpoint != healthy {
                None
            } else if body.contains("313ce567") {
                Some(decimals_reply(8))
            } else if body.contains("7284e416") {
                Some(description_reply("BTC / USD"))
            } else {
                round += 1;
                Some(round_reply(round, 1_756_400_000 + round))
            };
            Box::pin(async move { reply })
        });
    let mut driver = ChainlinkPollDriver::with_transport(
        "chainlink_aggregator",
        oracle_poll_options_over(
            vec![
                "https://a.example.invalid",
                "https://b.example.invalid",
                "https://c.example.invalid",
            ],
            240,
        ),
        Duration::from_secs(60),
        transport,
    )
    .expect("driver builds");

    let batch = driver.poll(0).await.expect("the first tick succeeds");
    assert!(
        batch.connected,
        "the eager start-up verifies on the healthy endpoint within the first tick"
    );
    // A 240 s run: never a whole-run outage once verified.
    for now_ns in [60_000_000_000u64, 120_000_000_000, 180_000_000_000] {
        driver
            .poll(now_ns)
            .await
            .expect("a verified run never exits transient");
    }
}

/// B1/M5: with every endpoint down for the whole run the driver never verifies,
/// so the SHELL (which owns the deadline) exits transient (75) — the class a
/// supervisor restarts — and the journal carries the feed-down evidence. Runs at
/// a fast fixture cadence with a fixture transport; no network.
#[tokio::test(flavor = "current_thread")]
async fn oracle_lane_exits_retryable_when_every_endpoint_is_down_for_the_whole_run() {
    use btc_signals::exit_code::{ExitClass, exit_class_for};
    use btc_signals::lane::ChainlinkPollDriver;

    let output = temp_path("oracle-down-output");
    let journal = temp_path("oracle-down-journal");
    let checkpoint = temp_path("oracle-down-checkpoint");

    let transport: btc_feeds::live::ChainlinkPollTransport<'static> =
        Box::new(|_endpoint: String, _body: String| Box::pin(async move { None }));
    let driver: Box<dyn PollDriver> = Box::new(
        ChainlinkPollDriver::with_transport(
            "chainlink_aggregator",
            oracle_poll_options(2),
            Duration::from_millis(20),
            transport,
        )
        .expect("driver builds"),
    );
    let result = run_lane(LaneRunArguments {
        index_gating: Some(disarmed_index_gating()),
        spec: &lane::ORACLE,
        output: output.clone(),
        journal: journal.clone(),
        checkpoint: checkpoint.clone(),
        resume_from_checkpoint: false,
        publisher_epoch: "oracle-chainlink-test".to_owned(),
        duration_seconds: 2,
        checkpoint_interval_seconds: 1,
        max_updates: 100_000,
        max_output_bytes: 8_388_608,
        max_journal_bytes: DEFAULT_LANE_JOURNAL_MAX_BYTES,
        max_checkpoint_bytes: DEFAULT_LANE_CHECKPOINT_MAX_BYTES,
        poll_driver: Some(driver),
        // The oracle lane is composite (C1): its Chainlink poll runs through the
        // injected `poll_driver`, and its Kalshi index and Pyth Hermes streams
        // through this fixture runner (connect, emit nothing). The streams are
        // observed-not-consumed, so the Chainlink reading is byte-identical to the
        // poll-only lane while the composite run/replay paths are exercised.
        stream_runner: Some(Box::new(FixtureStreamRunner)),
        rotation: None,
    })
    .await;

    let error = result.expect_err("a whole-run endpoint outage fails the run");
    assert_eq!(
        exit_class_for(&error),
        ExitClass::Retryable,
        "a whole-run outage exits 75, not fatal or clean: {error:?}"
    );
    assert_eq!(ExitClass::Retryable.code(), 75);

    for path in [output, journal, checkpoint] {
        let _ = std::fs::remove_file(path);
    }
}

/// M2 error path: a worker failure runs the durable close in abort mode — the
/// journal has no terminal record and replay refuses it — and `run_lane` returns
/// the error rather than skipping the close.
struct FailingPollDriver {
    polls: u32,
    fail_after: u32,
}

impl PollDriver for FailingPollDriver {
    fn stream_label(&self) -> &'static str {
        "chainlink_aggregator"
    }
    fn poll_interval(&self) -> Duration {
        Duration::from_millis(20)
    }
    fn poll<'a>(
        &'a mut self,
        _now_ns: u64,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<lane::PollBatch, lane::PollDriverError>> + 'a>,
    > {
        Box::pin(async move {
            self.polls += 1;
            if self.polls > self.fail_after {
                Err(lane::PollDriverError::Poll(
                    "injected oracle poll failure".to_owned(),
                ))
            } else {
                Ok(lane::PollBatch {
                    connected: true,
                    events: Vec::new(),
                    notices: Vec::new(),
                })
            }
        })
    }
}

#[tokio::test(flavor = "current_thread")]
async fn a_failing_worker_aborts_the_journal_without_a_terminal() {
    use btc_signals::lane_journal::LaneJournalReplayReader;

    let output = temp_path("fail-output");
    let journal = temp_path("fail-journal");
    let checkpoint = temp_path("fail-checkpoint");

    let driver: Box<dyn PollDriver> = Box::new(FailingPollDriver {
        polls: 0,
        fail_after: 1,
    });
    let result = run_lane(LaneRunArguments {
        index_gating: Some(disarmed_index_gating()),
        spec: &lane::ORACLE,
        output: output.clone(),
        journal: journal.clone(),
        checkpoint: checkpoint.clone(),
        resume_from_checkpoint: false,
        publisher_epoch: "oracle-fail-test".to_owned(),
        duration_seconds: 30,
        checkpoint_interval_seconds: 1,
        max_updates: 10_000,
        max_output_bytes: 4_194_304,
        max_journal_bytes: DEFAULT_LANE_JOURNAL_MAX_BYTES,
        max_checkpoint_bytes: DEFAULT_LANE_CHECKPOINT_MAX_BYTES,
        poll_driver: Some(driver),
        // The oracle lane is composite (C1): its Chainlink poll runs through the
        // injected `poll_driver`, and its Kalshi index and Pyth Hermes streams
        // through this fixture runner (connect, emit nothing). The streams are
        // observed-not-consumed, so the Chainlink reading is byte-identical to the
        // poll-only lane while the composite run/replay paths are exercised.
        stream_runner: Some(Box::new(FixtureStreamRunner)),
        rotation: None,
    })
    .await;
    assert!(result.is_err(), "a failing worker fails the run");

    // The journal was aborted, not completed: it has no terminal record, so a
    // replay opens the recoverable prefix and then reports a missing end.
    let mut reader = LaneJournalReplayReader::open(&journal, DEFAULT_LANE_JOURNAL_MAX_BYTES)
        .expect("the aborted journal opens");
    let mut saw_terminal_error = false;
    loop {
        match reader.next_input() {
            Ok(Some(_)) => continue,
            Ok(None) => break,
            Err(_) => {
                saw_terminal_error = true;
                break;
            }
        }
    }
    assert!(
        saw_terminal_error && reader.end().is_none(),
        "an aborted journal has no trusted terminal record"
    );

    for path in [output, journal, checkpoint] {
        let _ = std::fs::remove_file(path);
    }
}

// --- M2: the N-stream lane shell driven through run_lane with a fixture runner ---

/// A synthetic four-stream lane (an arity the old one-or-two guard rejected),
/// mixing three websocket streams with one `http_poll` stream (`BybitLinearRest`)
/// so the shell is exercised over both transports (ruling E41). The capture
/// streams are never dialled — the fixture runner ignores them.
static FOUR_STREAM_LANE: btc_signals::lane::LaneSpec = btc_signals::lane::LaneSpec {
    requirement: &BINANCE_LANE,
    venues: &["binance"],
    processor_id: "btc.fixture.feed_only",
    processor_version: "1.0.0",
    driver: btc_signals::lane::LaneDriverSpec::Streams(&[
        btc_signals::lane::LaneStream {
            token: "binance",
            stream: btc_feeds::CaptureStream::CoinbaseCore,
            label: "fixture_s0",
        },
        btc_signals::lane::LaneStream {
            token: "binance",
            stream: btc_feeds::CaptureStream::KrakenCore,
            label: "fixture_s1",
        },
        btc_signals::lane::LaneStream {
            token: "binance",
            stream: btc_feeds::CaptureStream::BinanceSpot,
            label: "fixture_s2",
        },
        // An http_poll stream in the mix: the shell drives poll and websocket
        // streams side by side.
        btc_signals::lane::LaneStream {
            token: "binance",
            stream: btc_feeds::CaptureStream::BybitLinearRest,
            label: "fixture_s3",
        },
    ]),
    // No reference streams: the fixture drives the shell over its four declared
    // streams only (the LaneSpec `reference_streams` field arrived with B1).
    reference_streams: &[],
    duration_default: 2,
};

fn fixture_stream_report() -> btc_feeds::live::LiveReport {
    btc_feeds::live::LiveReport {
        records: 1,
        events: 0,
        notices: 1,
        recoveries: 0,
        coinbase_trade_backfills: 0,
        connection_attempts: 1,
        successful_connections: 1,
        reconnects: 0,
        read_idle_timeouts: 0,
        induced_coinbase_trade_drops: 0,
        induced_disconnects: 0,
        kraken_tokens_created: 0,
        interrupted: false,
        kraken_trade_reconciliation: None,
    }
}

/// A fixture stream runner: instead of dialling a socket, it delivers one
/// Connected outcome through the shell's handle (so the stream reads live) and
/// then waits for the deadline coordinator's shutdown — enough to drive the
/// joined workers, the maintenance timer, and the durable close.
struct FixtureStreamRunner;

impl btc_signals::lane_runtime::StreamRunner for FixtureStreamRunner {
    fn run_stream<'a>(
        &'a self,
        options: btc_feeds::live::LiveOptions,
        clock: btc_feeds::live::ProcessClock,
        handle: &'a mut (
                    dyn FnMut(
            btc_feeds::live::TimedIngestOutcome,
        )
            -> Result<btc_feeds::live::LiveControl, btc_feeds::live::LiveError>
                        + 'a
                ),
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<btc_feeds::live::LiveReport, btc_feeds::live::LiveError>,
                > + 'a,
        >,
    > {
        Box::pin(async move {
            let now = clock.elapsed_ns()?;
            handle(btc_feeds::live::TimedIngestOutcome {
                outcome: btc_feeds::IngestOutcome {
                    events: Vec::new(),
                    notices: vec![btc_feeds::FeedNotice {
                        code: btc_feeds::FeedNoticeCode::Connected,
                        detail: "fixture stream connected".to_owned(),
                        recovery: None,
                        subscription: None,
                    }],
                },
                logical_receipt_ns: now,
            })?;
            if let Some(mut shutdown) = options.shutdown {
                let _ = shutdown.wait().await;
            }
            Ok(fixture_stream_report())
        })
    }
}

/// M2: a four-stream `Streams` lane runs end to end through `run_lane` — the
/// joined workers, the maintenance timer, and the single durable close — and its
/// journal replays byte-identically. No network: a fixture runner stands in for
/// the socket. Complements `join_lane_workers`'s unit test, which no live path
/// reaches.
#[tokio::test(flavor = "current_thread")]
async fn a_four_stream_lane_runs_through_run_streams_and_replays() {
    let output = temp_path("streams-output");
    let journal = temp_path("streams-journal");
    let checkpoint = temp_path("streams-checkpoint");
    let replay_output = temp_path("streams-replay");

    let (report, shutdown_reason) = run_lane_with_processor(
        LaneRunArguments {
            index_gating: None,
            spec: &FOUR_STREAM_LANE,
            output: output.clone(),
            journal: journal.clone(),
            checkpoint: checkpoint.clone(),
            resume_from_checkpoint: false,
            publisher_epoch: "fixture-streams".to_owned(),
            duration_seconds: 2,
            checkpoint_interval_seconds: 1,
            max_updates: 10_000,
            max_output_bytes: 4_194_304,
            max_journal_bytes: DEFAULT_LANE_JOURNAL_MAX_BYTES,
            max_checkpoint_bytes: DEFAULT_LANE_CHECKPOINT_MAX_BYTES,
            poll_driver: None,
            stream_runner: Some(Box::new(FixtureStreamRunner)),
            rotation: None,
        },
        btc_feeds::live::ProcessClock::start(),
        None,
        Box::new(FOUR_STREAM_LANE.feed_only_processor()),
    )
    .await
    .expect("the four-stream lane runs to its deadline");
    assert!(
        !shutdown_reason.interrupted(),
        "reached the natural deadline"
    );
    assert_eq!(
        report.live_reports.len(),
        4,
        "all four joined stream workers reported"
    );

    // Ready once all four fixture streams connect (the shell joined them all).
    let bytes = std::fs::read(&output).expect("output reads");
    let ready = String::from_utf8_lossy(&bytes)
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .any(|value| {
            value.get("type").and_then(|k| k.as_str()) == Some("status")
                && value.get("status").and_then(|s| s.as_str()) == Some("ready")
        });
    assert!(ready, "the four-stream lane reaches ready");

    // The checkpoint half of the maintenance timer: the file loads, carries this
    // lane's processor identity, and its state restores into a fresh processor —
    // the restored state equals the live state the run checkpointed.
    let document = load_lane_checkpoint(&checkpoint, DEFAULT_LANE_CHECKPOINT_MAX_BYTES)
        .expect("the four-stream checkpoint loads");
    assert_eq!(document.processor_id, "btc.fixture.feed_only");
    assert_eq!(document.processor_version, "1.0.0");
    let mut restored = FOUR_STREAM_LANE.feed_only_processor();
    restored
        .restore_state(document.processor_state.clone())
        .expect("the checkpoint state restores into a fresh processor");

    let _ = replay_lane_with_processor(
        &FOUR_STREAM_LANE,
        &journal,
        &replay_output,
        10_000,
        4_194_304,
        DEFAULT_LANE_JOURNAL_MAX_BYTES,
        Box::new(FOUR_STREAM_LANE.feed_only_processor()),
    )
    .expect("the four-stream journal replays");
    let replay_bytes = std::fs::read(&replay_output).expect("replay output reads");
    assert_eq!(
        bytes, replay_bytes,
        "the four-stream journal replays byte-identically"
    );

    for path in [output, journal, checkpoint, replay_output] {
        let _ = std::fs::remove_file(path);
    }
}

/// E39(c) regression: a round poll whose transport awaits past a health tick must
/// NOT stamp its journal input with the poll-start time — that would drop it
/// below the interleaved health tick and regress logical time, so the journal no
/// longer replays. A slow round transport under a paused clock forces exactly
/// that interleaving; with admission-time stamping the journal replays
/// byte-identically. (This exact shape regressed the smoke journal before the fix:
/// "lane journal logical time regressed".)
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_slow_poll_interleaved_with_health_ticks_replays_byte_identically() {
    use btc_signals::lane::ChainlinkPollDriver;

    let output = temp_path("slow-poll-output");
    let journal = temp_path("slow-poll-journal");
    let checkpoint = temp_path("slow-poll-checkpoint");
    let replay_output = temp_path("slow-poll-replay");

    // Startup replies immediately; each round poll awaits 1.5 s (virtual), so a
    // 1 s health tick is admitted mid-poll — the interleaving the bug needs.
    let mut round: u128 = 300;
    let transport: btc_feeds::live::ChainlinkPollTransport<'static> =
        Box::new(move |_endpoint: String, body: String| {
            if body.contains("313ce567") {
                Box::pin(async move { Some(decimals_reply(8)) })
                    as std::pin::Pin<Box<dyn std::future::Future<Output = Option<String>>>>
            } else if body.contains("7284e416") {
                Box::pin(async move { Some(description_reply("BTC / USD")) })
            } else {
                round += 1;
                let (rid, secs) = (round, 1_756_400_000 + round);
                Box::pin(async move {
                    tokio::time::sleep(Duration::from_millis(1_500)).await;
                    Some(round_reply(rid, secs))
                })
            }
        });
    let driver: Box<dyn PollDriver> = Box::new(
        ChainlinkPollDriver::with_transport(
            "chainlink_aggregator",
            oracle_poll_options(5),
            Duration::from_millis(200),
            transport,
        )
        .expect("driver builds"),
    );
    let (_report, _shutdown) = run_lane(LaneRunArguments {
        index_gating: Some(disarmed_index_gating()),
        spec: &lane::ORACLE,
        output: output.clone(),
        journal: journal.clone(),
        checkpoint: checkpoint.clone(),
        resume_from_checkpoint: false,
        publisher_epoch: "oracle-slow-poll".to_owned(),
        duration_seconds: 5,
        checkpoint_interval_seconds: 1,
        max_updates: 100_000,
        max_output_bytes: 8_388_608,
        max_journal_bytes: DEFAULT_LANE_JOURNAL_MAX_BYTES,
        max_checkpoint_bytes: DEFAULT_LANE_CHECKPOINT_MAX_BYTES,
        poll_driver: Some(driver),
        // The oracle lane is composite (C1): its Chainlink poll runs through the
        // injected `poll_driver`, and its Kalshi index and Pyth Hermes streams
        // through this fixture runner (connect, emit nothing). The streams are
        // observed-not-consumed, so the Chainlink reading is byte-identical to the
        // poll-only lane while the composite run/replay paths are exercised.
        stream_runner: Some(Box::new(FixtureStreamRunner)),
        rotation: None,
    })
    .await
    .expect("the slow-poll oracle lane runs to its deadline");

    let bytes = std::fs::read(&output).expect("output reads");
    // The journal must replay (logical time never regressed) AND byte-identically.
    let _ = replay_lane(
        &lane::ORACLE,
        &journal,
        &replay_output,
        100_000,
        8_388_608,
        DEFAULT_LANE_JOURNAL_MAX_BYTES,
    )
    .expect("the interleaved journal replays: logical time stayed monotonic");
    let replay_bytes = std::fs::read(&replay_output).expect("replay output reads");
    assert_eq!(
        bytes, replay_bytes,
        "the slow-poll journal replays byte-identically"
    );

    for path in [output, journal, checkpoint, replay_output] {
        let _ = std::fs::remove_file(path);
    }
}
/// Full-lane resume over the real F1 condition (B3 §8, audit F2/F3): segment one
/// publishes a round; segment two resumes and its first poll re-serves THAT SAME
/// round (a fixed-round driver, no round-1_000_000 detour). The resumed lane
/// republishes the current round — never a `chainlink_reorg_detected` withhold —
/// and its journal replays byte-identically. Driven through the `PollDriver` seam
/// (the real `ChainlinkPollDriver`'s deadline is measured from run start and does
/// not compose with a resumed clock; that gap is closed on the shell-deadline
/// rebase, at which point this can run over the real driver).
#[tokio::test(flavor = "current_thread")]
async fn oracle_lane_resume_over_the_same_round_never_reorgs_and_replays() {
    let output = temp_path("oracle-resume-a-output");
    let journal = temp_path("oracle-resume-a-journal");
    let checkpoint = temp_path("oracle-resume-checkpoint");

    // Segment one publishes round 100 (fixed), checkpointing every second.
    let driver: Box<dyn PollDriver> = Box::new(ScriptedRoundDriver {
        next_round: 100,
        base_secs: 1_756_400_000,
        advance: false,
    });
    run_lane(LaneRunArguments {
        index_gating: Some(disarmed_index_gating()),
        spec: &lane::ORACLE,
        output: output.clone(),
        journal: journal.clone(),
        checkpoint: checkpoint.clone(),
        resume_from_checkpoint: false,
        publisher_epoch: "oracle-resume-epoch-1".to_owned(),
        duration_seconds: 2,
        checkpoint_interval_seconds: 1,
        max_updates: 100_000,
        max_output_bytes: 8_388_608,
        max_journal_bytes: DEFAULT_LANE_JOURNAL_MAX_BYTES,
        max_checkpoint_bytes: DEFAULT_LANE_CHECKPOINT_MAX_BYTES,
        poll_driver: Some(driver),
        // The oracle lane is composite (C1): its Chainlink poll runs through the
        // injected `poll_driver`, and its Kalshi index and Pyth Hermes streams
        // through this fixture runner (connect, emit nothing). The streams are
        // observed-not-consumed, so the Chainlink reading is byte-identical to the
        // poll-only lane while the composite run/replay paths are exercised.
        stream_runner: Some(Box::new(FixtureStreamRunner)),
        rotation: None,
    })
    .await
    .expect("segment one runs to its deadline");

    let resume_output = temp_path("oracle-resume-b-output");
    let resume_journal = temp_path("oracle-resume-b-journal");
    let resume_replay = temp_path("oracle-resume-b-replay");
    // Segment two re-serves the SAME round 100: the first post-resume poll hits
    // the exact equal-round condition F1 mishandled.
    let driver: Box<dyn PollDriver> = Box::new(ScriptedRoundDriver {
        next_round: 100,
        base_secs: 1_756_400_000,
        advance: false,
    });
    run_lane(LaneRunArguments {
        index_gating: Some(disarmed_index_gating()),
        spec: &lane::ORACLE,
        output: resume_output.clone(),
        journal: resume_journal.clone(),
        checkpoint: checkpoint.clone(),
        resume_from_checkpoint: true,
        publisher_epoch: "oracle-resume-epoch-2".to_owned(),
        duration_seconds: 2,
        checkpoint_interval_seconds: 1,
        max_updates: 100_000,
        max_output_bytes: 8_388_608,
        max_journal_bytes: DEFAULT_LANE_JOURNAL_MAX_BYTES,
        max_checkpoint_bytes: DEFAULT_LANE_CHECKPOINT_MAX_BYTES,
        poll_driver: Some(driver),
        // The oracle lane is composite (C1): its Chainlink poll runs through the
        // injected `poll_driver`, and its Kalshi index and Pyth Hermes streams
        // through this fixture runner (connect, emit nothing). The streams are
        // observed-not-consumed, so the Chainlink reading is byte-identical to the
        // poll-only lane while the composite run/replay paths are exercised.
        stream_runner: Some(Box::new(FixtureStreamRunner)),
        rotation: None,
    })
    .await
    .expect("segment two resumes and runs to its deadline");

    let live = std::fs::read(&resume_output).expect("resume output reads");
    let frames: Vec<serde_json::Value> = live
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .filter_map(|line| serde_json::from_slice::<serde_json::Value>(line).ok())
        .collect();

    // The resumed lane republishes round 100 (never withheld as a reorg).
    let republished = frames
        .iter()
        .filter(|value| value.get("type").and_then(|k| k.as_str()) == Some("signal_batch"))
        .filter_map(|value| value.get("updates").and_then(|u| u.as_array()))
        .flatten()
        .any(|update| {
            update.get("signal_id").and_then(|s| s.as_str())
                == Some("btc.chainlink.oracle_reference")
                && update
                    .get("values")
                    .and_then(|v| v.get("round_id"))
                    .and_then(|r| r.as_str())
                    == Some("100")
        });
    assert!(
        republished,
        "the resumed lane republishes the current round"
    );
    let reorg = frames.iter().any(|value| {
        value.get("type").and_then(|k| k.as_str()) == Some("instance_status")
            && value
                .get("reasons")
                .and_then(|r| r.as_array())
                .is_some_and(|reasons| {
                    reasons
                        .iter()
                        .any(|reason| reason.as_str() == Some("chainlink_reorg_detected"))
                })
    });
    assert!(
        !reorg,
        "re-serving the current round on resume is never a reorg"
    );

    // The resumed journal replays byte-identically to its live output.
    let _ = replay_lane(
        &lane::ORACLE,
        &resume_journal,
        &resume_replay,
        100_000,
        8_388_608,
        DEFAULT_LANE_JOURNAL_MAX_BYTES,
    )
    .expect("resumed journal replays");
    let replayed = std::fs::read(&resume_replay).expect("resume replay reads");
    assert_eq!(
        live, replayed,
        "the resumed segment replays byte-identically"
    );

    for path in [
        output,
        journal,
        checkpoint,
        resume_output,
        resume_journal,
        resume_replay,
    ] {
        let _ = std::fs::remove_file(path);
    }
}

/// Frame-level coalescing (audit F4): a fixed round polled every 20 ms over a
/// multi-second run publishes ONE `signal_batch` (the first decision), not one per
/// poll — the change key never moves and the 60 s interval never falls due.
#[tokio::test(flavor = "current_thread")]
async fn oracle_lane_coalesces_an_unchanged_round_to_one_frame() {
    let output = temp_path("oracle-coalesce-output");
    let journal = temp_path("oracle-coalesce-journal");
    let checkpoint = temp_path("oracle-coalesce-checkpoint");

    let driver: Box<dyn PollDriver> = Box::new(ScriptedRoundDriver {
        next_round: 100,
        base_secs: 1_756_400_000,
        advance: false,
    });
    let (report, _) = run_lane(LaneRunArguments {
        index_gating: Some(disarmed_index_gating()),
        spec: &lane::ORACLE,
        output: output.clone(),
        journal: journal.clone(),
        checkpoint: checkpoint.clone(),
        resume_from_checkpoint: false,
        publisher_epoch: "oracle-coalesce".to_owned(),
        duration_seconds: 3,
        checkpoint_interval_seconds: 1,
        max_updates: 100_000,
        max_output_bytes: 8_388_608,
        max_journal_bytes: DEFAULT_LANE_JOURNAL_MAX_BYTES,
        max_checkpoint_bytes: DEFAULT_LANE_CHECKPOINT_MAX_BYTES,
        poll_driver: Some(driver),
        // The oracle lane is composite (C1): its Chainlink poll runs through the
        // injected `poll_driver`, and its Kalshi index and Pyth Hermes streams
        // through this fixture runner (connect, emit nothing). The streams are
        // observed-not-consumed, so the Chainlink reading is byte-identical to the
        // poll-only lane while the composite run/replay paths are exercised.
        stream_runner: Some(Box::new(FixtureStreamRunner)),
        rotation: None,
    })
    .await
    .expect("the coalescing run reaches its deadline");
    // ~150 polls of the same round over three seconds, but the reading changes
    // once, so exactly one signal batch is written.
    assert_eq!(
        report.pipeline.signal_batches_written, 1,
        "an unchanged round coalesces to a single frame"
    );

    for path in [output, journal, checkpoint] {
        let _ = std::fs::remove_file(path);
    }
}

// --- F1: the venue lanes run their real B1/B2 processor over the full LaneSpec
// stream set, replaying bounded excerpts of the observed hours through the shell.

/// The bounded observed excerpt for a lane stream, or `None` for a stream with no
/// fixture (the Kraken reference socket — the lane runs without reference frames;
/// the identities simply publish `usdt_reference_stale`). Fixtures live in the
/// btc-feeds crate and are shared with its completeness test.
fn f1_fixture_for(stream: btc_feeds::CaptureStream) -> Option<&'static str> {
    use btc_feeds::CaptureStream;
    match stream {
        CaptureStream::BinanceSpot => Some(include_str!(
            "../../btc-feeds/tests/fixtures/observed/binance_spot.jsonl"
        )),
        CaptureStream::BinanceUsdmPublic => Some(include_str!(
            "../../btc-feeds/tests/fixtures/observed/binance_usdm_public.jsonl"
        )),
        CaptureStream::BinanceUsdmRest => Some(include_str!(
            "../../btc-feeds/tests/fixtures/observed/binance_usdm_rest.jsonl"
        )),
        CaptureStream::BybitSpot => Some(include_str!(
            "../../btc-feeds/tests/fixtures/observed/bybit_spot.jsonl"
        )),
        CaptureStream::BybitLinear => Some(include_str!(
            "../../btc-feeds/tests/fixtures/observed/bybit_linear.jsonl"
        )),
        CaptureStream::BybitLinearRest => Some(include_str!(
            "../../btc-feeds/tests/fixtures/observed/bybit_linear_rest.jsonl"
        )),
        // C1: the composite oracle lane's index streams. Their events are
        // observed-not-consumed in phase 1, so the excerpt proves they are
        // counted (`ignored_events`) and routed past the Chainlink computation.
        CaptureStream::KalshiIndex => Some(include_str!(
            "../../btc-feeds/tests/fixtures/observed/kalshi_index.jsonl"
        )),
        CaptureStream::PythHermesStream => Some(include_str!(
            "../../btc-feeds/tests/fixtures/observed/pyth_hermes_stream.jsonl"
        )),
        _ => None,
    }
}

/// A stream runner that replays the observed excerpt for each declared stream
/// through the lane shell: it normalizes the raw records with the same
/// `ReplaySession` the capture/replay path uses and delivers the events, so the
/// venue processor sees real venue frames — including the unowned sources whose
/// events must be counted (`ignored_events`) rather than aborting the lane.
/// An extra foreign source can be appended to one stream to prove the genuine-bug
/// refusal path (a source that is neither consumed nor declared-ignored).
struct ReplayFixtureRunner {
    foreign_source: Option<String>,
}

impl btc_signals::lane_runtime::StreamRunner for ReplayFixtureRunner {
    fn run_stream<'a>(
        &'a self,
        options: btc_feeds::live::LiveOptions,
        clock: btc_feeds::live::ProcessClock,
        handle: &'a mut (
                    dyn FnMut(
            btc_feeds::live::TimedIngestOutcome,
        )
            -> Result<btc_feeds::live::LiveControl, btc_feeds::live::LiveError>
                        + 'a
                ),
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<btc_feeds::live::LiveReport, btc_feeds::live::LiveError>,
                > + 'a,
        >,
    > {
        Box::pin(async move {
            let stream = options.stream;
            let mut report = fixture_stream_report();
            // A Connected notice marks the stream live, exactly like a real dial.
            let now = clock.elapsed_ns()?;
            handle(btc_feeds::live::TimedIngestOutcome {
                outcome: btc_feeds::IngestOutcome {
                    events: Vec::new(),
                    notices: vec![btc_feeds::FeedNotice {
                        code: btc_feeds::FeedNoticeCode::Connected,
                        detail: "fixture stream connected".to_owned(),
                        recovery: None,
                        subscription: None,
                    }],
                },
                logical_receipt_ns: now,
            })?;
            if let Some(capture) = f1_fixture_for(stream) {
                let mut session = btc_feeds::replay::ReplaySession::new(
                    stream,
                    options.publisher_epoch.clone(),
                    None,
                )
                .expect("replay session builds for the fixture stream");
                let mut events = 0_u64;
                let mut records = 0_u64;
                for line in capture.lines() {
                    if line.trim().is_empty() {
                        continue;
                    }
                    let record: btc_feeds::RawRecord =
                        serde_json::from_str(line).expect("fixture record parses");
                    let outcome = session.ingest(&record).expect("fixture record replays");
                    records += 1;
                    events += outcome.events.len() as u64;
                    if !outcome.events.is_empty() || !outcome.notices.is_empty() {
                        let now = clock.elapsed_ns()?;
                        handle(btc_feeds::live::TimedIngestOutcome {
                            outcome: btc_feeds::IngestOutcome {
                                events: outcome.events,
                                notices: outcome.notices,
                            },
                            logical_receipt_ns: now,
                        })?;
                    }
                }
                report.records = records;
                report.events = events;
            }
            // Optionally deliver one foreign event on the lane's own book stream to
            // prove the genuine-bug refusal path (a source neither consumed nor
            // declared-ignored aborts the lane).
            if let Some(foreign) = &self.foreign_source
                && f1_fixture_for(stream).is_some()
            {
                let now = clock.elapsed_ns()?;
                let mut values = std::collections::BTreeMap::new();
                values.insert("noop".to_owned(), serde_json::Value::from(0));
                let event = signals_core::SourceEvent {
                    schema_version: 1,
                    source_id: foreign.clone(),
                    source_version: "1.0.0".to_owned(),
                    publisher_epoch: options.publisher_epoch.clone(),
                    sequence: 1,
                    event_time: "2026-09-04T00:00:00Z".to_owned(),
                    received_at: "2026-09-04T00:00:00Z".to_owned(),
                    values,
                };
                handle(btc_feeds::live::TimedIngestOutcome {
                    outcome: btc_feeds::IngestOutcome {
                        events: vec![event],
                        notices: Vec::new(),
                    },
                    logical_receipt_ns: now,
                })?;
            }
            if let Some(mut shutdown) = options.shutdown {
                let _ = shutdown.wait().await;
            }
            Ok(report)
        })
    }
}

/// The distinct publisher_epoch values across every stream-v3 frame in a lane's
/// output.
fn output_publisher_epochs(bytes: &[u8]) -> std::collections::BTreeSet<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter_map(|value| {
            value
                .get("publisher_epoch")
                .and_then(|epoch| epoch.as_str())
                .map(str::to_owned)
        })
        .collect()
}

async fn run_venue_lane_over_fixtures(
    spec: &'static lane::LaneSpec,
    label: &str,
) -> (btc_signals::lane_runtime::LaneRunReport, PathBuf) {
    let output = temp_path(&format!("{label}-output"));
    let journal = temp_path(&format!("{label}-journal"));
    let checkpoint = temp_path(&format!("{label}-checkpoint"));
    let (report, shutdown_reason) = run_lane(LaneRunArguments {
        index_gating: None,
        spec,
        output: output.clone(),
        journal: journal.clone(),
        checkpoint: checkpoint.clone(),
        resume_from_checkpoint: false,
        publisher_epoch: format!("{label}-epoch"),
        duration_seconds: 2,
        checkpoint_interval_seconds: 1,
        max_updates: 1_000_000,
        max_output_bytes: 64 * 1024 * 1024,
        max_journal_bytes: DEFAULT_LANE_JOURNAL_MAX_BYTES,
        max_checkpoint_bytes: DEFAULT_LANE_CHECKPOINT_MAX_BYTES,
        poll_driver: None,
        stream_runner: Some(Box::new(ReplayFixtureRunner {
            foreign_source: None,
        })),
        rotation: None,
    })
    .await
    .expect("the venue lane replays its observed streams without aborting");
    assert!(
        !shutdown_reason.interrupted(),
        "the lane reaches its natural deadline"
    );
    let _ = std::fs::remove_file(&journal);
    let _ = std::fs::remove_file(&checkpoint);
    (report, output)
}

#[tokio::test(flavor = "current_thread")]
async fn binance_lane_replays_its_observed_streams_without_aborting() {
    let (report, output) = run_venue_lane_over_fixtures(&lane::BINANCE, "binance-f1").await;
    // F3: the journal/checkpoint identity names the venue processor that ran.
    let ignored = &report.pipeline.feed_health.ignored_events;
    // F1: the two unowned Binance sources its declared streams produce are counted
    // as ignored, not aborted on: usdm depth (from the public socket) and the OI
    // 5-min history (from the REST poll).
    assert!(
        ignored
            .get("binance.usdm.btcusdt.book-levels")
            .is_some_and(|count| *count > 0),
        "usdm depth is observed-not-consumed and counted, got {ignored:?}"
    );
    assert!(
        ignored
            .get("binance.usdm.btcusdt.open-interest-hist")
            .is_some_and(|count| *count > 0),
        "usdm open-interest-hist is observed-not-consumed and counted, got {ignored:?}"
    );
    // One publisher epoch across the whole output (spec §2, R5).
    let bytes = std::fs::read(&output).expect("output reads");
    let epochs = output_publisher_epochs(&bytes);
    assert_eq!(
        epochs,
        std::collections::BTreeSet::from(["binance-f1-epoch".to_owned()]),
        "the lane publishes under exactly one publisher epoch"
    );
    let _ = std::fs::remove_file(output);
}

#[tokio::test(flavor = "current_thread")]
async fn bybit_lane_replays_its_observed_streams_without_aborting() {
    let (report, output) = run_venue_lane_over_fixtures(&lane::BYBIT, "bybit-f1").await;
    let ignored = &report.pipeline.feed_health.ignored_events;
    // F1: the three unowned Bybit sources: the linear 50-level book (from the
    // linear socket) and the linear OI + funding history (from the REST poll).
    for source in [
        "bybit.linear.btcusdt.book-levels",
        "bybit.linear.btcusdt.open-interest",
        "bybit.linear.btcusdt.funding-history",
    ] {
        assert!(
            ignored.get(source).is_some_and(|count| *count > 0),
            "{source} is observed-not-consumed and counted, got {ignored:?}"
        );
    }
    let bytes = std::fs::read(&output).expect("output reads");
    let epochs = output_publisher_epochs(&bytes);
    assert_eq!(
        epochs,
        std::collections::BTreeSet::from(["bybit-f1-epoch".to_owned()]),
        "the lane publishes under exactly one publisher epoch"
    );
    let _ = std::fs::remove_file(output);
}

// --- F1 replay parity: the documented `venue-signals replay --lane binance|bybit`
// recovery path. The venue processors' clock threading changed during the port
// (`wall_start` now flows from the LaneClock via `run_lane_with_processor` and is
// rebuilt from the journal session on `replay_lane_with_processor`), so the
// byte-identical replay the oracle lane already proves must be proven for the
// venue processors too — the oracle assertions (~:161, ~:993) never touched them.

/// Run a venue lane live over its observed fixtures, RETAINING the journal and
/// checkpoint (unlike `run_venue_lane_over_fixtures`, which deletes them) so the
/// caller can replay or resume.
async fn run_venue_lane_capturing(
    spec: &'static lane::LaneSpec,
    label: &str,
) -> (
    btc_signals::lane_runtime::LaneRunReport,
    PathBuf,
    PathBuf,
    PathBuf,
) {
    let output = temp_path(&format!("{label}-output"));
    let journal = temp_path(&format!("{label}-journal"));
    let checkpoint = temp_path(&format!("{label}-checkpoint"));
    let (report, shutdown_reason) = run_lane(LaneRunArguments {
        index_gating: None,
        spec,
        output: output.clone(),
        journal: journal.clone(),
        checkpoint: checkpoint.clone(),
        resume_from_checkpoint: false,
        publisher_epoch: format!("{label}-epoch"),
        duration_seconds: 2,
        checkpoint_interval_seconds: 1,
        max_updates: 1_000_000,
        max_output_bytes: 64 * 1024 * 1024,
        max_journal_bytes: DEFAULT_LANE_JOURNAL_MAX_BYTES,
        max_checkpoint_bytes: DEFAULT_LANE_CHECKPOINT_MAX_BYTES,
        poll_driver: None,
        stream_runner: Some(Box::new(ReplayFixtureRunner {
            foreign_source: None,
        })),
        rotation: None,
    })
    .await
    .expect("the venue lane replays its observed streams without aborting");
    assert!(
        !shutdown_reason.interrupted(),
        "the lane reaches its natural deadline"
    );
    (report, output, journal, checkpoint)
}

/// F1 replay parity: a venue lane's retained journal, replayed through
/// `replay_lane_with_processor` over a FRESHLY built venue processor (rebuilt from
/// the journal session's own `wall_start`/`publisher_epoch`), reproduces the live
/// stream output byte-for-byte. Also pins the journal session identity to the venue
/// processor and asserts the run is non-vacuous (updates written and a ready frame),
/// so a lane that silently produced nothing could not pass. Mirrors the oracle
/// byte-identity test (~:161) and the four-stream one (~:993).
async fn assert_venue_lane_replays_byte_identically(
    spec: &'static lane::LaneSpec,
    label: &str,
    expected_processor_id: &str,
) {
    let (report, output, journal, checkpoint) = run_venue_lane_capturing(spec, label).await;

    // Non-vacuous: the venue processor published updates and the lane reached ready.
    assert!(
        report.pipeline.updates_written > 0,
        "{label}: the venue lane wrote signal updates, got {}",
        report.pipeline.updates_written
    );
    let bytes = std::fs::read(&output).expect("output reads");
    let ready = String::from_utf8_lossy(&bytes)
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .any(|value| {
            value.get("type").and_then(|k| k.as_str()) == Some("status")
                && value.get("status").and_then(|s| s.as_str()) == Some("ready")
        });
    assert!(ready, "{label}: the venue lane reaches ready");

    // F3: the journal session identity names the venue processor that ran.
    let reader = LaneJournalReplayReader::open(&journal, DEFAULT_LANE_JOURNAL_MAX_BYTES)
        .expect("the journal opens for its session record");
    let session = reader.session().clone();
    drop(reader);
    assert_eq!(
        session.processor_id, expected_processor_id,
        "{label}: the journal session names the venue processor"
    );

    // Build a FRESH venue processor from the journal's own session (the changed
    // clock threading: `wall_start` comes from the session, not a constant) and
    // replay through the seam.
    let wall_start = chrono::DateTime::parse_from_rfc3339(&session.wall_start)
        .expect("the session wall_start parses")
        .with_timezone(&chrono::Utc);
    let processor = spec
        .build_processor(&session.publisher_epoch, wall_start)
        .expect("the venue processor builds for replay");
    assert_eq!(
        processor.processor_id(),
        expected_processor_id,
        "{label}: the rebuilt replay processor is the venue processor"
    );

    let replay_output = temp_path(&format!("{label}-replay"));
    let _ = replay_lane_with_processor(
        spec,
        &journal,
        &replay_output,
        1_000_000,
        64 * 1024 * 1024,
        DEFAULT_LANE_JOURNAL_MAX_BYTES,
        processor,
    )
    .expect("the venue journal replays");
    let replay_bytes = std::fs::read(&replay_output).expect("replay output reads");
    assert_eq!(
        bytes, replay_bytes,
        "{label}: the venue journal replays byte-identically to the live output"
    );

    for path in [output, journal, checkpoint, replay_output] {
        let _ = std::fs::remove_file(path);
    }
}

// The venue's 13 spot families + derivatives publish on the interval tick, not per
// data event (venue_spot_signals.rs: "every other input mutates state and the 13
// families publish on the interval tick"), so `updates_written > 0` and the ready
// producer status here depend on at least one `emit_interval` firing. `start_paused`
// drives that tick off tokio's virtual clock (which `ProcessClock` samples), so it
// fires deterministically when the runtime idles after the front-loaded replay
// instead of racing the wall-clock deadline on a slow CI runner. Byte-identity is
// unaffected. Idiom: `a_slow_poll_interleaved_with_health_ticks_replays_byte_identically`.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn binance_lane_journal_replays_byte_identically() {
    assert_venue_lane_replays_byte_identically(
        &lane::BINANCE,
        "binance-replay",
        "btc.binance.venue",
    )
    .await;
}

// `start_paused`: the venue Ready decisions + `updates_written` publish only on the
// interval tick (see `binance_lane_journal_replays_byte_identically`); drive it off
// the virtual clock so it never races the wall-clock deadline.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn bybit_lane_journal_replays_byte_identically() {
    assert_venue_lane_replays_byte_identically(&lane::BYBIT, "bybit-replay", "btc.bybit.venue")
        .await;
}

/// F1 resume parity: a venue lane checkpointed on a first segment resumes through
/// `run_lane_with_processor` — the exact seam whose `wall_start`/clock threading
/// into the venue processor changed during the port — over a LaneClock resumed from
/// the checkpoint and a freshly built venue processor, and the resumed segment's own
/// journal replays byte-identically over another fresh venue processor. The resumed
/// session carries the venue identity and the second publisher epoch.
///
/// This is the venue analogue of the oracle resume-and-replay test (~:1103): the
/// resumed segment's journal→output byte-identity is how this codebase proves resume
/// parity. (A literal "continuation bytes equal the uninterrupted run's tail" is not
/// expressible here — the whole-fixture `ReplayFixtureRunner` re-delivers the entire
/// excerpt each run, and every frame is stamped with the segment's own
/// publisher_epoch, so a fresh run and a resumed run never share a byte-level tail.)
async fn assert_venue_lane_resume_replays_byte_identically(
    spec: &'static lane::LaneSpec,
    label: &str,
    expected_processor_id: &str,
) {
    // Segment one establishes a checkpoint over the observed fixtures.
    let (_report, seg1_output, seg1_journal, checkpoint) =
        run_venue_lane_capturing(spec, &format!("{label}-seg1")).await;

    // Segment two resumes from that checkpoint through the changed seam: a LaneClock
    // resumed from the checkpoint's wall_start and a venue processor built on it.
    let checkpoint_doc = load_lane_checkpoint(&checkpoint, DEFAULT_LANE_CHECKPOINT_MAX_BYTES)
        .expect("the segment-one checkpoint loads");
    assert_eq!(
        checkpoint_doc.processor_id, expected_processor_id,
        "{label}: the checkpoint names the venue processor"
    );
    let resume_wall_start = chrono::DateTime::parse_from_rfc3339(&checkpoint_doc.wall_start)
        .expect("the checkpoint wall_start parses")
        .with_timezone(&chrono::Utc);
    let clock =
        btc_feeds::live::ProcessClock::resume(resume_wall_start, checkpoint_doc.checkpoint_time_ns)
            .expect("the logical clock resumes from the checkpoint");
    let resume_epoch = format!("{label}-epoch-2");
    let processor = spec
        .build_processor(&resume_epoch, clock.wall_start())
        .expect("the venue processor builds for the resumed segment");

    let resume_output = temp_path(&format!("{label}-seg2-output"));
    let resume_journal = temp_path(&format!("{label}-seg2-journal"));
    let resume_checkpoint = temp_path(&format!("{label}-seg2-checkpoint"));
    let (report, shutdown_reason) = run_lane_with_processor(
        LaneRunArguments {
            index_gating: Some(disarmed_index_gating()),
            spec,
            output: resume_output.clone(),
            journal: resume_journal.clone(),
            checkpoint: resume_checkpoint.clone(),
            resume_from_checkpoint: true,
            publisher_epoch: resume_epoch.clone(),
            duration_seconds: 2,
            checkpoint_interval_seconds: 1,
            max_updates: 1_000_000,
            max_output_bytes: 64 * 1024 * 1024,
            max_journal_bytes: DEFAULT_LANE_JOURNAL_MAX_BYTES,
            max_checkpoint_bytes: DEFAULT_LANE_CHECKPOINT_MAX_BYTES,
            poll_driver: None,
            stream_runner: Some(Box::new(ReplayFixtureRunner {
                foreign_source: None,
            })),
            rotation: None,
        },
        clock,
        Some(checkpoint_doc),
        processor,
    )
    .await
    .expect("the venue lane resumes and runs to its deadline");
    assert!(
        !shutdown_reason.interrupted(),
        "{label}: the resumed lane reaches its natural deadline"
    );
    // The resume restored real state: the report accounts admitted inputs.
    assert!(
        report.pipeline.admitted_inputs > 0,
        "{label}: the resumed segment admitted inputs, got {}",
        report.pipeline.admitted_inputs
    );
    // Non-vacuous, mirroring the replay pair (updates_written + a ready frame):
    // a resume-restore regression that publishes nothing must not pass on the
    // admitted-inputs count alone.
    assert!(
        report.pipeline.updates_written > 0,
        "{label}: the resumed segment wrote signal updates, got {}",
        report.pipeline.updates_written
    );
    let resume_bytes = std::fs::read(&resume_output).expect("the resume output reads");
    let resume_ready = String::from_utf8_lossy(&resume_bytes)
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .any(|value| {
            value.get("type").and_then(|k| k.as_str()) == Some("status")
                && value.get("status").and_then(|s| s.as_str()) == Some("ready")
        });
    assert!(
        resume_ready,
        "{label}: the resumed venue lane reaches ready"
    );

    // The resumed session names the venue processor and carries the second epoch.
    let reader = LaneJournalReplayReader::open(&resume_journal, DEFAULT_LANE_JOURNAL_MAX_BYTES)
        .expect("the resumed journal opens for its session record");
    let session = reader.session().clone();
    drop(reader);
    assert_eq!(
        session.processor_id, expected_processor_id,
        "{label}: the resumed journal names the venue processor"
    );
    assert_eq!(
        session.publisher_epoch, resume_epoch,
        "{label}: the resumed session records the second publisher epoch"
    );

    // The resumed segment replays byte-identically over a fresh venue processor —
    // the changed clock threading survives a resume.
    let live = std::fs::read(&resume_output).expect("the resume output reads");
    let replay_wall_start = chrono::DateTime::parse_from_rfc3339(&session.wall_start)
        .expect("the resumed session wall_start parses")
        .with_timezone(&chrono::Utc);
    let replay_processor = spec
        .build_processor(&session.publisher_epoch, replay_wall_start)
        .expect("the venue processor rebuilds for the resumed replay");
    let resume_replay = temp_path(&format!("{label}-seg2-replay"));
    let _ = replay_lane_with_processor(
        spec,
        &resume_journal,
        &resume_replay,
        1_000_000,
        64 * 1024 * 1024,
        DEFAULT_LANE_JOURNAL_MAX_BYTES,
        replay_processor,
    )
    .expect("the resumed venue journal replays");
    let replayed = std::fs::read(&resume_replay).expect("the resume replay reads");
    assert_eq!(
        live, replayed,
        "{label}: the resumed segment replays byte-identically to its live output"
    );

    for path in [
        seg1_output,
        seg1_journal,
        checkpoint,
        resume_output,
        resume_journal,
        resume_checkpoint,
        resume_replay,
    ] {
        let _ = std::fs::remove_file(path);
    }
}

// `start_paused`: the resumed venue segment's `updates_written > 0` + ready decision
// publish only on the interval tick (see `binance_lane_journal_replays_byte_identically`);
// drive it off the virtual clock so it never races the wall-clock deadline.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn binance_lane_resume_replays_byte_identically() {
    assert_venue_lane_resume_replays_byte_identically(
        &lane::BINANCE,
        "binance-resume",
        "btc.binance.venue",
    )
    .await;
}

// `start_paused`: same as `binance_lane_resume_replays_byte_identically` — the venue
// Ready decisions publish only on the interval tick; drive it off the virtual clock.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn bybit_lane_resume_replays_byte_identically() {
    assert_venue_lane_resume_replays_byte_identically(
        &lane::BYBIT,
        "bybit-resume",
        "btc.bybit.venue",
    )
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_venue_lane_refuses_a_source_it_neither_consumes_nor_ignores() {
    // A source that is neither consumed nor produced by a declared stream (a
    // wrong-lane frame) is a genuine bug the dispatch still refuses (F1 10b): the
    // lane aborts rather than silently dropping it.
    let output = temp_path("binance-foreign-output");
    let journal = temp_path("binance-foreign-journal");
    let checkpoint = temp_path("binance-foreign-checkpoint");
    let result = run_lane(LaneRunArguments {
        index_gating: None,
        spec: &lane::BINANCE,
        output: output.clone(),
        journal: journal.clone(),
        checkpoint: checkpoint.clone(),
        resume_from_checkpoint: false,
        publisher_epoch: "binance-foreign-epoch".to_owned(),
        duration_seconds: 2,
        checkpoint_interval_seconds: 1,
        max_updates: 1_000_000,
        max_output_bytes: 64 * 1024 * 1024,
        max_journal_bytes: DEFAULT_LANE_JOURNAL_MAX_BYTES,
        max_checkpoint_bytes: DEFAULT_LANE_CHECKPOINT_MAX_BYTES,
        poll_driver: None,
        stream_runner: Some(Box::new(ReplayFixtureRunner {
            foreign_source: Some("bybit.spot.btcusdt.trades".to_owned()),
        })),
        rotation: None,
    })
    .await;
    assert!(
        result.is_err(),
        "a foreign source on the binance lane aborts the run"
    );
    for path in [output, journal, checkpoint] {
        let _ = std::fs::remove_file(path);
    }
}

#[test]
fn build_processor_yields_the_full_venue_signal_set() {
    // A regression from the venue processor back to the feed-only interim cannot
    // pass: each venue lane's processor must publish its full identity set (13
    // spot siblings + 7 spot TA identities (C3a / E51) + 2 C2 spot horizon
    // identities + the C3b `tick_velocity` spot identity (ruling E51) + the
    // derivatives families + their perp mark TA and C2 identities). Since ruling
    // E62 Bybit publishes 37 and Binance 35 — Binance no longer produces
    // `liquidation_pressure`/`perp_cvd` (their `market`-socket inputs were deleted).
    for spec in [&lane::BINANCE, &lane::BYBIT] {
        let lane = spec.requirement.lane;
        let expected = if lane == "binance" { 35 } else { 37 };
        let processor = spec
            .build_processor("build-processor-test", chrono::DateTime::UNIX_EPOCH)
            .expect("the venue processor builds");
        assert_eq!(
            processor.signal_definitions().len(),
            expected,
            "{lane} must publish {expected} identities, not a feed-only 0",
        );
        assert_eq!(processor.signal_count(), expected);
    }
}

#[test]
fn a_feed_only_era_binance_checkpoint_is_refused_by_the_venue_processor() {
    // F3: the journal/checkpoint identity now comes from the running processor.
    // The Binance venue processor stamps `btc.binance.venue`, so a checkpoint left
    // by the retired feed-only Binance lane (`btc.binance.feed_only`) is refused on
    // resume with a clear identity error — never handed to the processor to fail
    // later with an opaque serde `deny_unknown_fields`.
    use btc_signals::lane_checkpoint::LaneCheckpointDocument;
    use btc_signals::lane_journal::{LaneIdentity, LaneRuntimeConfig, lane_contract_fingerprint};
    use btc_signals::lane_runtime::ensure_resume_matches_processor;

    let processor = lane::BINANCE
        .build_processor("resume-identity-test", chrono::DateTime::UNIX_EPOCH)
        .expect("the binance venue processor builds");
    assert_eq!(
        processor.processor_id(),
        "btc.binance.venue",
        "the running processor names itself, not the feed-only interim"
    );

    let runtime_config = LaneRuntimeConfig::new("binance", 1_000_000_000);
    let fingerprint = lane_contract_fingerprint(&runtime_config).expect("fingerprint builds");
    let feed_only_checkpoint = LaneCheckpointDocument::new(
        "btc.binance.feed_only".to_owned(),
        "1.0.0".to_owned(),
        "0123456789abcdef0123456789abcdef".to_owned(),
        "2026-09-03T00:00:00Z".to_owned(),
        0,
        0,
        fingerprint,
        runtime_config,
        serde_json::json!({"live_streams": [], "ever_ready": false}),
        vec!["binance-epoch".to_owned()],
    )
    .expect("checkpoint builds");

    let identity = LaneIdentity {
        lane: "binance",
        venues: vec!["binance".to_owned()],
        processor_id: processor.processor_id().to_owned(),
        processor_version: processor.processor_version().to_owned(),
    };
    let error = ensure_resume_matches_processor(&feed_only_checkpoint, &identity)
        .expect_err("a feed-only-era checkpoint is refused by the venue processor");
    let message = format!("{error:#}");
    assert!(
        message.contains("btc.binance.feed_only") && message.contains("btc.binance.venue"),
        "the refusal is an identity error naming both ids, got: {message}"
    );
}

// --- F0: an unknown lane name is a fatal start-up misconfiguration (no fallback).

/// A lane whose name matches no `build_processor` arm. Its rights requirement is a
/// throwaway with an unknown lane name; the fixture never dials, because the run
/// refuses while building the processor.
static UNKNOWN_LANE_REQUIREMENT: signals_core::LaneRequirement = signals_core::LaneRequirement {
    lane: "definitely-not-a-real-lane",
    required: &[],
    index_sockets: &[],
};

static UNKNOWN_LANE: btc_signals::lane::LaneSpec = btc_signals::lane::LaneSpec {
    requirement: &UNKNOWN_LANE_REQUIREMENT,
    venues: &["binance"],
    processor_id: "btc.unknown.feed_only",
    processor_version: "1.0.0",
    driver: btc_signals::lane::LaneDriverSpec::Streams(&[btc_signals::lane::LaneStream {
        token: "binance",
        stream: btc_feeds::CaptureStream::BinanceSpot,
        label: "unknown_s0",
    }]),
    reference_streams: &[],
    duration_default: 2,
};

/// F0: `build_processor` has no wildcard, so a lane whose name matches no arm is a
/// fatal misconfiguration, not a silent feed-only lane. `run_lane` refuses it while
/// building the processor — BEFORE any journal, output, checkpoint, or stream worker
/// is created — so a blind restart cannot mask it. (The feed-only injection seam
/// `run_lane_with_processor` is exercised by the four-stream fixture above.)
#[tokio::test(flavor = "current_thread")]
async fn run_lane_refuses_an_unknown_lane_before_dialing_a_stream() {
    let output = temp_path("unknown-lane-output");
    let journal = temp_path("unknown-lane-journal");
    let checkpoint = temp_path("unknown-lane-checkpoint");
    let result = run_lane(LaneRunArguments {
        index_gating: None,
        spec: &UNKNOWN_LANE,
        output: output.clone(),
        journal: journal.clone(),
        checkpoint: checkpoint.clone(),
        resume_from_checkpoint: false,
        publisher_epoch: "unknown-lane-epoch".to_owned(),
        duration_seconds: 2,
        checkpoint_interval_seconds: 1,
        max_updates: 10_000,
        max_output_bytes: 4_194_304,
        max_journal_bytes: DEFAULT_LANE_JOURNAL_MAX_BYTES,
        max_checkpoint_bytes: DEFAULT_LANE_CHECKPOINT_MAX_BYTES,
        poll_driver: None,
        stream_runner: Some(Box::new(FixtureStreamRunner)),
        rotation: None,
    })
    .await;
    let error = result.expect_err("an unknown lane is a fatal misconfiguration");
    let message = format!("{error:#}");
    assert!(
        message.contains("no processor is defined for lane definitely-not-a-real-lane"),
        "the refusal names the unknown lane, got: {message}"
    );
    // The refusal happens before anything is created on disk.
    assert!(
        !journal.exists(),
        "no journal is written before the refusal"
    );
    assert!(!output.exists(), "no output is written before the refusal");
    assert!(
        !checkpoint.exists(),
        "no checkpoint is written before the refusal"
    );
}

// ---------------------------------------------------------------------------
// C5 in-process generation rotation (Design B): the venue shell seals one file
// generation and opens the next in place, keeping the processor and its rings
// warm while the output stream resets like a fresh process (new output epoch,
// sequence 0, a `reset` frame). These drive binance/bybit (Streams) and oracle
// (Poll) through the `*_with_processor` seams, with the fixed `_venue` names.
// ---------------------------------------------------------------------------

use serde_json::Value;

fn binance_spec() -> &'static lane::LaneSpec {
    &lane::BINANCE
}
fn bybit_spec() -> &'static lane::LaneSpec {
    &lane::BYBIT
}
fn oracle_spec() -> &'static lane::LaneSpec {
    &lane::ORACLE
}
type LaneSpecFn = fn() -> &'static lane::LaneSpec;
const ROTATION_LANES: &[(&str, LaneSpecFn)] = &[
    ("binance", binance_spec),
    ("bybit", bybit_spec),
    ("oracle", oracle_spec),
];

/// The driver pair for a lane run: a poll driver (Poll lane) or a stream runner
/// (Streams lane).
type LaneDrivers = (
    Option<Box<dyn PollDriver>>,
    Option<Box<dyn btc_signals::lane_runtime::StreamRunner>>,
);

/// A fresh, owned temp lane root for a rotation run. Keyed on the pid AND a
/// process-wide monotonic counter as well as the wall nanos: `SystemTime` has only
/// ~microsecond resolution on macOS, so two concurrent rotation tests can sample
/// the same nanos and would otherwise share a root — the second one's gen-0 open
/// then fails Fatal on the first's directory. The counter makes every root unique
/// within a process regardless of clock resolution.
fn rotation_lane_root(label: &str) -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "c5-venue-rot-{label}-{}-{seq}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create lane root");
    dir
}

/// The driver for a lane: the observed-fixture stream runner for a Streams lane,
/// the fixture poll driver for a Poll lane (polling fast enough to place rounds
/// in every generation), or — for the composite oracle lane (C1) — both, so its
/// Chainlink poll and its replayed index streams drive together.
fn lane_drivers(spec: &'static lane::LaneSpec) -> LaneDrivers {
    match &spec.driver {
        btc_signals::lane::LaneDriverSpec::Poll { label, .. } => (
            Some(Box::new(FixturePollDriver::new(
                label,
                Duration::from_millis(20),
                0,
            ))),
            None,
        ),
        btc_signals::lane::LaneDriverSpec::Streams(_) => (
            None,
            Some(Box::new(ReplayFixtureRunner {
                foreign_source: None,
            })),
        ),
        btc_signals::lane::LaneDriverSpec::Composite { label, .. } => (
            Some(Box::new(FixturePollDriver::new(
                label,
                Duration::from_millis(20),
                0,
            ))),
            Some(Box::new(ReplayFixtureRunner {
                foreign_source: None,
            })),
        ),
    }
}

/// Run a lane in rotation mode through `run_lane_with_processor`, returning the
/// lane root holding the sealed generations. `rotate_seconds` bypasses the CLI's
/// 60 s floor via the seam so a short test rotates several times.
async fn run_venue_rotation(
    spec: &'static lane::LaneSpec,
    label: &str,
    rotate_seconds: u64,
    duration_seconds: u64,
    stream_hook: Option<StreamHook>,
) -> PathBuf {
    let lane_root = rotation_lane_root(label);
    let epoch = format!("{label}-rot-epoch");
    let clock = btc_feeds::live::ProcessClock::start();
    let processor = spec
        .build_processor(&epoch, clock.wall_start())
        .expect("the venue processor builds");
    let (poll_driver, stream_runner) = lane_drivers(spec);
    let (_report, shutdown_reason) = run_lane_with_processor_and_hook(
        LaneRunArguments {
            // Armed so the composite oracle lane's index streams drive ACROSS the
            // rotation boundaries (the poll-only rotation path is covered by the
            // standalone oracle tests); a Streams lane (binance/bybit) ignores it.
            index_gating: Some(armed_index_gating()),
            spec,
            output: PathBuf::new(),
            journal: PathBuf::new(),
            checkpoint: lane_root.join("checkpoint.json"),
            resume_from_checkpoint: false,
            publisher_epoch: epoch,
            duration_seconds,
            checkpoint_interval_seconds: 1,
            max_updates: 5_000_000,
            max_output_bytes: 128 * 1024 * 1024,
            max_journal_bytes: DEFAULT_LANE_JOURNAL_MAX_BYTES,
            max_checkpoint_bytes: DEFAULT_LANE_CHECKPOINT_MAX_BYTES,
            poll_driver,
            stream_runner,
            rotation: Some(LaneRotation {
                lane_root: lane_root.clone(),
                rotate_seconds,
                headroom_factor: 2,
            }),
        },
        clock,
        None,
        processor,
        stream_hook,
    )
    .await
    .expect("the rotation run completes");
    assert!(
        !shutdown_reason.interrupted(),
        "{label}: the rotation run reaches its deadline"
    );
    lane_root
}

/// Run the same lane once unrotated (explicit mode), returning its single output
/// path — the value baseline the rotated run is compared against.
async fn run_venue_unrotated(
    spec: &'static lane::LaneSpec,
    label: &str,
    duration_seconds: u64,
) -> PathBuf {
    let output = temp_path(&format!("{label}-unrot-output"));
    let journal = temp_path(&format!("{label}-unrot-journal"));
    let checkpoint = temp_path(&format!("{label}-unrot-checkpoint"));
    let epoch = format!("{label}-unrot-epoch");
    let clock = btc_feeds::live::ProcessClock::start();
    let processor = spec
        .build_processor(&epoch, clock.wall_start())
        .expect("the venue processor builds");
    let (poll_driver, stream_runner) = lane_drivers(spec);
    let (_report, shutdown_reason) = run_lane_with_processor(
        LaneRunArguments {
            index_gating: Some(disarmed_index_gating()),
            spec,
            output: output.clone(),
            journal: journal.clone(),
            checkpoint: checkpoint.clone(),
            resume_from_checkpoint: false,
            publisher_epoch: epoch,
            duration_seconds,
            checkpoint_interval_seconds: 1,
            max_updates: 5_000_000,
            max_output_bytes: 128 * 1024 * 1024,
            max_journal_bytes: DEFAULT_LANE_JOURNAL_MAX_BYTES,
            max_checkpoint_bytes: DEFAULT_LANE_CHECKPOINT_MAX_BYTES,
            poll_driver,
            stream_runner,
            rotation: None,
        },
        clock,
        None,
        processor,
    )
    .await
    .expect("the unrotated run completes");
    assert!(
        !shutdown_reason.interrupted(),
        "{label}: the unrotated run reaches its deadline"
    );
    let _ = std::fs::remove_file(journal);
    let _ = std::fs::remove_file(checkpoint);
    output
}

/// The generation directories under a lane root, chronological (the unix suffix
/// is fixed-width, so a lexicographic sort is chronological).
fn generation_dirs(lane_root: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(lane_root)
        .expect("read lane root")
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.is_dir()
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("gen-live-"))
        })
        .collect();
    dirs.sort();
    dirs
}

/// Parse a stream output file into its JSON frames.
fn read_frames(path: &Path) -> Vec<Value> {
    let bytes = std::fs::read(path).expect("read output");
    String::from_utf8(bytes)
        .expect("utf8 output")
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str::<Value>(line).expect("json frame"))
        .collect()
}

fn frame_type(frame: &Value) -> &str {
    frame.get("type").and_then(Value::as_str).unwrap_or("")
}

/// The Ready readings in one output, keyed `(signal_id, produced_at)` → values.
/// Every `signal_batch` update is Ready (warming/degraded go to `instance_status`).
fn ready_readings(frames: &[Value]) -> std::collections::BTreeMap<(String, String), Value> {
    let mut readings = std::collections::BTreeMap::new();
    for frame in frames.iter().filter(|f| frame_type(f) == "signal_batch") {
        let produced_at = frame
            .get("produced_at")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if let Some(updates) = frame.get("updates").and_then(Value::as_array) {
            for update in updates {
                let id = update
                    .get("signal_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let values = update.get("values").cloned().unwrap_or(Value::Null);
                readings.insert((id, produced_at.clone()), values);
            }
        }
    }
    readings
}

/// A copy of a values object with wall-clock-derived freshness/latency fields
/// removed — embedded timestamps (`observation_time`) and elapsed-time metadata
/// (`book_age_ms`, `recovery_ms`, …). Two independent live runs sample the wall
/// clock at different instants, so these jitter run-to-run regardless of rotation;
/// stripping them isolates "rotation changed the computed signal values" from
/// ordinary wall-clock jitter. Non-object values are returned unchanged.
fn strip_time_fields(value: &Value) -> Value {
    const FRESHNESS: [&str; 4] = ["time", "age", "ms", "latency"];
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(key, _)| !FRESHNESS.iter().any(|needle| key.contains(needle)))
                .map(|(key, val)| (key.clone(), val.clone()))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// One identity's reading at a single decision: its quality plus the wall-immune
/// payload that quality carries — a Ready reading's time-stripped values, or a
/// non-ready state's name and sorted reasons.
#[derive(Clone, PartialEq, Eq, Debug)]
enum IdentityReading {
    Ready(Value),
    NotReady { state: String, reasons: Vec<String> },
}

/// Every identity's decision sequence across an output, in emission (decision)
/// order: Ready readings from `signal_batch` frames (written every decision) and
/// non-ready quality states from `instance_status` frames (written on a state
/// transition or a periodic heartbeat). Keyed by identity; each value is that
/// identity's ordered readings with CONSECUTIVE IDENTICAL readings collapsed.
///
/// The collapse is deliberate and is not the `last_ready_values` collapse the C5
/// audit flagged: it keeps every distinct value/quality-state CHANGE (so a rotation
/// that perturbs a post-boundary decision — an OFI baseline reset, a book that
/// diverges then re-converges, a one-decision quality flap — is preserved and
/// caught), while dropping only timing-driven repeats that two independent live runs
/// need not agree on: periodic status re-emissions, an off-by-one health tick, or a
/// rotation boundary re-emitting the warm value. Two runs sample the wall clock at
/// different instants, so `produced_at` and embedded timestamps cannot be compared
/// across runs directly — this captures the wall-immune decision content only.
fn identity_decision_sequences(
    frames: &[Value],
) -> std::collections::BTreeMap<String, Vec<IdentityReading>> {
    let mut sequences: std::collections::BTreeMap<String, Vec<IdentityReading>> =
        std::collections::BTreeMap::new();
    let mut push = |id: String, reading: IdentityReading| {
        let seq = sequences.entry(id).or_default();
        if seq.last() != Some(&reading) {
            seq.push(reading);
        }
    };
    for frame in frames {
        match frame_type(frame) {
            "signal_batch" => {
                if let Some(updates) = frame.get("updates").and_then(Value::as_array) {
                    for update in updates {
                        let id = update
                            .get("signal_id")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned();
                        let values = update.get("values").cloned().unwrap_or(Value::Null);
                        push(id, IdentityReading::Ready(strip_time_fields(&values)));
                    }
                }
            }
            "instance_status" => {
                let id = frame
                    .get("signal_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let state = frame
                    .get("state")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let mut reasons: Vec<String> = frame
                    .get("reasons")
                    .and_then(Value::as_array)
                    .map(|array| {
                        array
                            .iter()
                            .filter_map(|reason| reason.as_str().map(str::to_owned))
                            .collect()
                    })
                    .unwrap_or_default();
                reasons.sort();
                push(id, IdentityReading::NotReady { state, reasons });
            }
            _ => {}
        }
    }
    sequences
}

/// The Ready identity set of the first / last `signal_batch` in an output.
fn first_batch_ready_ids(frames: &[Value]) -> std::collections::BTreeSet<String> {
    batch_ready_ids(frames.iter().find(|f| frame_type(f) == "signal_batch"))
}
fn last_batch_ready_ids(frames: &[Value]) -> std::collections::BTreeSet<String> {
    batch_ready_ids(
        frames
            .iter()
            .rev()
            .find(|f| frame_type(f) == "signal_batch"),
    )
}
fn batch_ready_ids(batch: Option<&Value>) -> std::collections::BTreeSet<String> {
    let mut ids = std::collections::BTreeSet::new();
    if let Some(updates) = batch
        .and_then(|b| b.get("updates"))
        .and_then(Value::as_array)
    {
        for update in updates {
            if let Some(id) = update.get("signal_id").and_then(Value::as_str) {
                ids.insert(id.to_owned());
            }
        }
    }
    ids
}

/// The `(publisher_epoch, sequence)` of an output's opening `reset` frame.
fn opening_reset(frames: &[Value]) -> Option<(String, u64)> {
    let first = frames.first()?;
    if frame_type(first) != "reset" {
        return None;
    }
    Some((
        first.get("publisher_epoch")?.as_str()?.to_owned(),
        first.get("sequence")?.as_u64()?,
    ))
}

const ROTATE_SECONDS: u64 = 1;
const RUN_SECONDS: u64 = 3;

/// The `Venue` a rotation-lane label publishes under, or `None` for the oracle lane
/// (which publishes no per-venue spot identities, so its non-ring set is empty). The
/// replay lower bound derives its checked identities from this, never a hand-list.
fn venue_for_label(label: &str) -> Option<btc_signals::Venue> {
    match label {
        "binance" => Some(btc_signals::Venue::Binance),
        "bybit" => Some(btc_signals::Venue::Bybit),
        _ => None,
    }
}

/// The full-string non-ring venue identities for a rotation lane: the SNAPSHOT-DIRECT
/// families (quote, microprice, actionable depth imbalance) that reach Ready straight
/// from the resync-rebuilt book/quote and the checkpoint-carried USDT reference, with
/// no warm-up ring. Derived from the identity catalogue minus the ring identities (the
/// sole authority is `btc_signals::venue_spot_signals::non_ring_venue_spot_signal_ids`),
/// never hand-picked here — so a wrong deep-book OR bbo resync (spot_quote consumes the
/// bbo-levels source) fails the lower bound. Empty for the oracle lane.
fn non_ring_ids_for_label(label: &str) -> Vec<String> {
    venue_for_label(label)
        .map(|venue| {
            btc_signals::venue_spot_signals::non_ring_venue_spot_signal_ids(venue)
                .into_iter()
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// Test 1: rotation is invisible to the values. The rotated run's Ready identities
/// and their values equal an unrotated run's (envelope epoch/sequence and the
/// reset frames are excluded by construction — we read only `signal_batch` update
/// readings), every identity Ready before a boundary is Ready in the first decision
/// after it (rings/state stayed warm), and each generation resets like a fresh
/// process (a `reset` at sequence 0 under a per-generation output epoch).
// NOT `start_paused`: this test cross-compares a rotated run against a SEPARATE
// unrotated run. Under a virtual clock the two independent runs' event-vs-tick
// interleaving diverges (tokio `select!` randomizes coincident 1 s health/rotation
// ticks), so the published-identity SET differs run-to-run (observed: line 2985
// flaked ~1/3). It is already hardened against tick-count jitter on the real clock
// by collapsing consecutive-identical readings, so it stays wall-clock driven.
#[tokio::test(flavor = "current_thread")]
async fn rotation_is_invisible_to_the_values_venue() {
    // The ring-bearing venue lanes: "invisible to the values" and warm-ring
    // continuity across a boundary are their concern. The oracle lane emits values
    // on change (not every generation), so its rotation correctness is proven by
    // the byte-identity / resume tests (below) rather than this value comparison.
    for (label, spec_fn) in ROTATION_LANES
        .iter()
        .filter(|(label, _)| *label != "oracle")
    {
        let spec = spec_fn();
        let lane_root = run_venue_rotation(spec, label, ROTATE_SECONDS, RUN_SECONDS, None).await;
        let dirs = generation_dirs(&lane_root);
        assert!(
            dirs.len() >= 3,
            "{label}: rotated at least twice, got {} generations",
            dirs.len()
        );

        // Reset-like-a-fresh-process: generation 0 uses the process epoch verbatim
        // and each later generation a distinct `<epoch>-<unix>` output epoch, each
        // opening with a `reset` at sequence 0.
        let process_epoch = format!("{label}-rot-epoch");
        let mut seen_epochs = std::collections::BTreeSet::new();
        let mut concatenated: Vec<Value> = Vec::new();
        for (index, dir) in dirs.iter().enumerate() {
            let frames = read_frames(&dir.join("output.jsonl"));
            if let Some((epoch, sequence)) = opening_reset(&frames) {
                assert_eq!(sequence, 0, "{label}: generation {index} resets at 0");
                if index == 0 {
                    assert_eq!(
                        epoch, process_epoch,
                        "{label}: gen 0 keeps the process epoch"
                    );
                } else {
                    assert!(
                        epoch.starts_with(&format!("{process_epoch}-")) && epoch != process_epoch,
                        "{label}: gen {index} output epoch {epoch} extends the process epoch"
                    );
                }
                assert!(
                    seen_epochs.insert(epoch),
                    "{label}: output epochs are distinct"
                );
            }
            concatenated.extend(frames);
        }

        // Continuity: wherever a boundary has a decision on both sides, every
        // identity Ready in the last decision before it is Ready in the first
        // decision after it — the rings/state stayed warm across the in-place swap.
        // The ring-bearing venue lanes must show this on at least one boundary; the
        // oracle lane emits values on change (a static fixture round need not emit
        // every generation), so its cross-boundary continuity is not required.
        let mut continuity_proven = false;
        for pair in dirs.windows(2) {
            let before = read_frames(&pair[0].join("output.jsonl"));
            let after = read_frames(&pair[1].join("output.jsonl"));
            let before_ready = last_batch_ready_ids(&before);
            let after_ready = first_batch_ready_ids(&after);
            if before_ready.is_empty() || after_ready.is_empty() {
                continue;
            }
            assert!(
                before_ready.is_subset(&after_ready),
                "{label}: identities Ready before a boundary stay Ready after it \
                 (before: {before_ready:?}, after: {after_ready:?})"
            );
            continuity_proven = true;
        }
        assert!(
            continuity_proven,
            "{label}: at least one boundary carries Ready identities across it"
        );

        // Values AND quality states invisible, PER DECISION (not just the last
        // reading): the rotated run's ordered decision sequence for every identity —
        // Ready values and non-ready quality states, across ALL generations, so gen 1
        // and gen 2's post-boundary decisions are included, not only gen 0's — equals
        // an unrotated run's. Consecutive-identical readings are collapsed, so this
        // compares data-driven value/state CHANGES (immune to wall-clock jitter, an
        // off-by-one health tick, and a boundary re-emitting the warm value) while
        // still catching any post-boundary perturbation the old last-value check missed.
        let unrotated_output = run_venue_unrotated(spec, label, RUN_SECONDS).await;
        let unrotated = read_frames(&unrotated_output);
        let rotated_seqs = identity_decision_sequences(&concatenated);
        let unrotated_seqs = identity_decision_sequences(&unrotated);
        assert!(
            !rotated_seqs.is_empty(),
            "{label}: the rotated run published decisions"
        );
        assert_eq!(
            rotated_seqs.keys().collect::<Vec<_>>(),
            unrotated_seqs.keys().collect::<Vec<_>>(),
            "{label}: rotation does not change which identities publish"
        );
        for (id, rotated_seq) in &rotated_seqs {
            let unrotated_seq = unrotated_seqs
                .get(id)
                .expect("identity present in both runs (key sets asserted equal)");
            assert_eq!(
                rotated_seq, unrotated_seq,
                "{label}: rotation changed identity {id}'s per-decision values/quality states"
            );
        }
        for path in [lane_root, unrotated_output] {
            let _ = std::fs::remove_dir_all(&path);
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Test 2: each file generation's journal replays byte-identically to its output —
/// byte-for-byte for generation 0 and every non-ring identity in later generations;
/// ring identities re-warm honestly (the sealed checkpoint carries no ring state,
/// C5b), so a later generation's replay is never *ahead* of its live output and
/// every reading present in both is byte-equal.
// `start_paused`: `dirs.len() >= 3` requires the +1 s / +2 s rotation ticks to fire
// inside the 3 s window; drive them off the virtual clock so the generation count is
// deterministic, not host-speed dependent (see `rotation_is_invisible_to_the_values_venue`).
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn each_file_generation_journal_replays_byte_identically_venue() {
    for (label, spec_fn) in ROTATION_LANES {
        let spec = spec_fn();
        let lane_root = run_venue_rotation(spec, label, ROTATE_SECONDS, RUN_SECONDS, None).await;
        let dirs = generation_dirs(&lane_root);
        assert!(dirs.len() >= 3, "{label}: rotated at least twice");
        for (index, dir) in dirs.iter().enumerate() {
            assert_generation_replays(spec, label, index, dir);
        }
        let _ = std::fs::remove_dir_all(lane_root);
    }
}

/// The in-process stream hook observes every frame of every generation, in
/// order, across rotation boundaries: the hook belongs to the process, not
/// the file, so sealing and reopening a generation never drops it.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn stream_hook_survives_rotation_and_sees_every_frame_venue() {
    use std::sync::{Arc, Mutex};

    let (label, spec_fn) = &ROTATION_LANES[0];
    let spec = spec_fn();
    let seen: Arc<Mutex<Vec<(String, String, u64)>>> = Arc::new(Mutex::new(Vec::new()));
    let hook: StreamHook = {
        let seen = seen.clone();
        Arc::new(move |message: &signals_core::SignalStreamMessage| {
            let value = serde_json::to_value(message).expect("message serializes");
            seen.lock().unwrap().push((
                frame_type(&value).to_owned(),
                value["publisher_epoch"].as_str().unwrap_or_default().to_owned(),
                message.sequence(),
            ));
        })
    };
    let lane_root = run_venue_rotation(spec, label, ROTATE_SECONDS, RUN_SECONDS, Some(hook)).await;
    let dirs = generation_dirs(&lane_root);
    assert!(dirs.len() >= 2, "{label}: rotated at least once");
    let written: Vec<(String, String, u64)> = dirs
        .iter()
        .flat_map(|dir| read_frames(&dir.join("output.jsonl")))
        .map(|frame| {
            (
                frame_type(&frame).to_owned(),
                frame["publisher_epoch"].as_str().unwrap_or_default().to_owned(),
                frame["sequence"].as_u64().unwrap_or_default(),
            )
        })
        .collect();
    assert!(!written.is_empty(), "{label}: the run wrote frames");
    assert_eq!(*seen.lock().unwrap(), written, "{label}: the hook saw every written frame across every generation, in order");
    let _ = std::fs::remove_dir_all(lane_root);
}

/// Test 3: resume-after-seal. A later generation's journal is a fresh session
/// resumed from the checkpoint sealed at the previous generation (it carries that
/// initial checkpoint and a distinct output epoch), and replaying it reproduces the
/// generation's output under the same re-warm invariant as test 2.
// `start_paused`: `dirs.len() >= 2` requires a rotation tick (+1 s) to seal a
// boundary inside the 3 s window; drive it off the virtual clock so at least one
// seal-and-open always happens (see `rotation_is_invisible_to_the_values_venue`).
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn resume_after_seal_replays_the_next_generation_venue() {
    for (label, spec_fn) in ROTATION_LANES {
        let spec = spec_fn();
        let lane_root = run_venue_rotation(spec, label, ROTATE_SECONDS, RUN_SECONDS, None).await;
        let dirs = generation_dirs(&lane_root);
        assert!(
            dirs.len() >= 2,
            "{label}: at least one seal-and-open boundary"
        );

        // Generation 1 is a resumed session: it carries the sealed initial
        // checkpoint and a distinct output epoch (the process epoch + a suffix).
        let gen1 = &dirs[1];
        let reader = LaneJournalReplayReader::open(
            &gen1.join("journal.jsonl"),
            DEFAULT_LANE_JOURNAL_MAX_BYTES,
        )
        .expect("the resumed generation journal opens");
        let session = reader.session().clone();
        drop(reader);
        assert!(
            session.initial_checkpoint.is_some(),
            "{label}: generation 1 resumes from the sealed checkpoint"
        );
        let process_epoch = format!("{label}-rot-epoch");
        assert!(
            session
                .publisher_epoch
                .starts_with(&format!("{process_epoch}-"))
                && session.publisher_epoch != process_epoch,
            "{label}: generation 1 emits under a new output epoch"
        );

        assert_generation_replays(spec, label, 1, gen1);
        let _ = std::fs::remove_dir_all(lane_root);
    }
}

/// Replay one generation's journal over a processor rebuilt from its session's
/// admission epoch, and check the result against the live output: byte-identical
/// for generation 0; for later generations, replay is never ahead of live and every
/// Ready reading present in both is byte-equal (the honest ring re-warm).
fn assert_generation_replays(spec: &'static lane::LaneSpec, label: &str, index: usize, dir: &Path) {
    let journal = dir.join("journal.jsonl");
    let live_output = dir.join("output.jsonl");
    let reader = LaneJournalReplayReader::open(&journal, DEFAULT_LANE_JOURNAL_MAX_BYTES)
        .expect("the generation journal opens for its session");
    let session = reader.session().clone();
    drop(reader);
    let wall_start = chrono::DateTime::parse_from_rfc3339(&session.wall_start)
        .expect("the session wall_start parses")
        .with_timezone(&chrono::Utc);
    // Rebuild the processor from the session's output epoch, exactly as
    // `replay_lane` does in production. The replay pipeline re-stamps every emitted
    // update with that output epoch, so the processor's own build epoch does not
    // reach the output — replay reproduces the live generation's frames regardless.
    let processor = spec
        .build_processor(&session.publisher_epoch, wall_start)
        .expect("the venue processor rebuilds for replay");
    let replay_output = temp_path(&format!("{label}-gen{index}-replay"));
    let result = replay_lane_with_processor(
        spec,
        &journal,
        &replay_output,
        5_000_000,
        128 * 1024 * 1024,
        DEFAULT_LANE_JOURNAL_MAX_BYTES,
        processor,
    );
    // Every generation replays cleanly now: generation 0 byte-for-byte, and a rotated
    // generation ≥ 1 through the resync records (its book rebuilds from them) plus the
    // gap-aware terminal check (its warm rings re-warm cold, so the sealed checkpoint
    // carrying no ring state moves only the stream-message count, reported as the
    // re-warm gap — full byte-identity is C5b). Any error here is a real failure.
    let report = result.unwrap_or_else(|error| panic!("{label}: gen {index} replays: {error:#}"));
    if index == 0 {
        assert_eq!(
            report.rewarm_stream_message_gap, 0,
            "{label}: generation 0 has no re-warm gap"
        );
    }

    let live_bytes = std::fs::read(&live_output).expect("live output reads");
    let replay_bytes = std::fs::read(&replay_output).expect("replay output reads");
    if index == 0 {
        assert_eq!(
            live_bytes, replay_bytes,
            "{label}: generation 0 replays byte-identically"
        );
    } else if live_bytes != replay_bytes {
        // Later generations differ only by the honest ring re-warm: the sealed
        // checkpoint carries no ring/bar state (C5b closes this), so a cold replay
        // starts every time-ring identity (book_resilience, macd, …) cold and re-warms
        // it within the generation. Per decision 22:36Z the replay's ring values may
        // lag live's during that window — "values equal once warm; states may lag" —
        // so this generation cannot assert per-reading value equality (test 1 proves
        // the values are invisible to rotation, warm-vs-warm). What it MUST hold is
        // that the replay is never *ahead* of live: every (signal_id, produced_at) the
        // replay reports Ready, live also reported Ready at that instant — a cold
        // replay can only reach Ready later than, or with, the warm live run, never
        // before it. It never invents a decision live lacked.
        let live = read_frames(&live_output);
        let replay = read_frames(&replay_output);
        let live_ready = ready_readings(&live);
        let replay_ready = ready_readings(&replay);
        for key in replay_ready.keys() {
            assert!(
                live_ready.contains_key(key),
                "{label}: gen {index} replay reading {key:?} is absent from live (replay never ahead of live)"
            );
        }
        // Lower bound (finding 4): every non-ring venue identity is computed from the
        // current book/quote alone (plus the checkpoint-carried USDT reference), so once
        // the resync rebuilds the deep book AND the bbo book it MUST reach Ready in the
        // replay at the same instant live did, with byte-equal computed values. This is
        // the check a wrong rebuilt book (a bids-only deep resync, or a wrong bbo resync
        // that mis-maps sides/truncates levels — spot_quote consumes the bbo) fails: it
        // would drop or corrupt these readings. Ring/stateful identities (book_resilience,
        // the TA rings, OFI/flow) may lag and are excluded. The set is derived from the
        // identity catalogue minus the ring identities. Wall freshness is stripped.
        for signal_id in &non_ring_ids_for_label(label) {
            for (key, live_value) in &live_ready {
                if &key.0 != signal_id {
                    continue;
                }
                let replay_value = replay_ready.get(key).unwrap_or_else(|| {
                    panic!(
                        "{label}: gen {index} non-ring reading {key:?} live produced is missing \
                         from replay — the resync did not rebuild the book correctly"
                    )
                });
                assert_eq!(
                    strip_time_fields(live_value),
                    strip_time_fields(replay_value),
                    "{label}: gen {index} non-ring reading {key:?} must match live's computed values"
                );
            }
        }
    }
    let _ = std::fs::remove_file(replay_output);
}

/// Test 4: rotation-mode CLI validation — rotation (`--lane-root`/`--rotate-seconds`)
/// and explicit (`--output`/`--journal`) modes are mutually exclusive, the rotation
/// pair must be complete and `--rotate-seconds` at least the 60 s floor, and a
/// well-formed rotation invocation is accepted (it fails later, not at the usage
/// layer). Usage errors exit with clap's usage class (2).
#[test]
fn rotation_mode_cli_validation_venue() {
    let bin = env!("CARGO_BIN_EXE_venue-signals");
    let run = |args: &[&str]| -> i32 {
        std::process::Command::new(bin)
            .args(args)
            .output()
            .expect("spawn venue-signals")
            .status
            .code()
            .expect("venue-signals exits with a code")
    };
    let root = rotation_lane_root("cli");

    // Both modes at once → usage error.
    assert_eq!(
        run(&[
            "run",
            "--lane",
            "binance",
            "--publisher-epoch",
            "e",
            "--lane-root",
            root.to_str().unwrap(),
            "--rotate-seconds",
            "60",
            "--output",
            "/tmp/c5-o",
            "--journal",
            "/tmp/c5-j",
            "--checkpoint",
            "/tmp/c5-c",
        ]),
        2,
        "rotation and explicit modes are mutually exclusive"
    );
    // Incomplete rotation pair → usage error.
    assert_eq!(
        run(&[
            "run",
            "--lane",
            "binance",
            "--publisher-epoch",
            "e",
            "--lane-root",
            root.to_str().unwrap(),
        ]),
        2,
        "rotation mode requires --rotate-seconds"
    );
    // Below the rotate-seconds floor → usage error.
    assert_eq!(
        run(&[
            "run",
            "--lane",
            "binance",
            "--publisher-epoch",
            "e",
            "--lane-root",
            root.to_str().unwrap(),
            "--rotate-seconds",
            "30",
        ]),
        2,
        "--rotate-seconds must be at least 60"
    );
    // Neither mode's paths → usage error.
    assert_eq!(
        run(&["run", "--lane", "binance", "--publisher-epoch", "e"]),
        2,
        "explicit mode requires --output/--journal/--checkpoint"
    );
    // A well-formed rotation invocation passes CLI validation (the checkpoint
    // defaults under the lane root); it fails later — not with the usage class —
    // because the lane root does not exist.
    let missing = root.join("does-not-exist");
    let accepted = run(&[
        "run",
        "--lane",
        "oracle",
        "--publisher-epoch",
        "e",
        "--lane-root",
        missing.to_str().unwrap(),
        "--rotate-seconds",
        "60",
        "--duration-seconds",
        "1",
    ]);
    assert_ne!(
        accepted, 2,
        "a well-formed rotation invocation is accepted at the usage layer, got {accepted}"
    );
    // Parity with spot/prediction: `--headroom-factor` is accepted in rotation mode
    // (RUN-META provenance only, default 2) and never a usage error.
    let with_headroom = run(&[
        "run",
        "--lane",
        "oracle",
        "--publisher-epoch",
        "e",
        "--lane-root",
        missing.to_str().unwrap(),
        "--rotate-seconds",
        "60",
        "--headroom-factor",
        "3",
        "--duration-seconds",
        "1",
    ]);
    assert_ne!(
        with_headroom, 2,
        "a rotation invocation with --headroom-factor is accepted at the usage layer, got {with_headroom}"
    );
    let _ = std::fs::remove_dir_all(root);
}

// ---------------------------------------------------------------------------
// C5 resync regression: Bybit book deltas spread across a rotation boundary.
//
// The whole-fixture ReplayFixtureRunner above delivers every fixture record on
// connect, so the entire Bybit book (snapshot + deltas) lands in generation 0 and
// later generations carry no book records — the delta-first fatal the resync records
// exist to prevent is never reached. The PacedFixtureRunner instead spreads the
// fixture over wall time, so the initial snapshot warms the book in generation 0
// while subsequent deltas fall into later generations. A later generation's journal
// then begins (after its resync re-snapshot) with a real book DELTA — exactly the
// `book delta arrived before a complete snapshot` fatal a standalone cold replay hit
// before decision 23:30Z. With the resync records written at each rotation the book
// rebuilds and that generation replays.
// ---------------------------------------------------------------------------

const BYBIT_SPOT_BOOK_SOURCE: &str = "bybit.spot.btcusdt.book-levels";

/// A stream runner that replays the observed excerpt like [`ReplayFixtureRunner`] but
/// paces delivery: it waits `gap` before each record's outcome (racing the wait
/// against shutdown so the run still ends at its deadline), so book snapshot/delta
/// groups straddle rotation boundaries instead of all landing in generation 0.
struct PacedFixtureRunner {
    gap: Duration,
}

impl btc_signals::lane_runtime::StreamRunner for PacedFixtureRunner {
    fn run_stream<'a>(
        &'a self,
        options: btc_feeds::live::LiveOptions,
        clock: btc_feeds::live::ProcessClock,
        handle: &'a mut (
                    dyn FnMut(
            btc_feeds::live::TimedIngestOutcome,
        )
            -> Result<btc_feeds::live::LiveControl, btc_feeds::live::LiveError>
                        + 'a
                ),
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<btc_feeds::live::LiveReport, btc_feeds::live::LiveError>,
                > + 'a,
        >,
    > {
        Box::pin(async move {
            let stream = options.stream;
            let gap = self.gap;
            let mut shutdown = options.shutdown;
            let mut report = fixture_stream_report();
            let now = clock.elapsed_ns()?;
            handle(btc_feeds::live::TimedIngestOutcome {
                outcome: btc_feeds::IngestOutcome {
                    events: Vec::new(),
                    notices: vec![btc_feeds::FeedNotice {
                        code: btc_feeds::FeedNoticeCode::Connected,
                        detail: "paced fixture stream connected".to_owned(),
                        recovery: None,
                        subscription: None,
                    }],
                },
                logical_receipt_ns: now,
            })?;
            if let Some(capture) = f1_fixture_for(stream) {
                let mut session = btc_feeds::replay::ReplaySession::new(
                    stream,
                    options.publisher_epoch.clone(),
                    None,
                )
                .expect("replay session builds for the paced fixture stream");
                let mut events = 0_u64;
                let mut records = 0_u64;
                'delivery: for line in capture.lines() {
                    if line.trim().is_empty() {
                        continue;
                    }
                    let record: btc_feeds::RawRecord =
                        serde_json::from_str(line).expect("fixture record parses");
                    let outcome = session.ingest(&record).expect("fixture record replays");
                    records += 1;
                    events += outcome.events.len() as u64;
                    if outcome.events.is_empty() && outcome.notices.is_empty() {
                        continue;
                    }
                    // Pace delivery, but never outlive the run: a shutdown at the
                    // natural deadline ends delivery at once so the lane returns.
                    if let Some(shutdown) = shutdown.as_mut() {
                        tokio::select! {
                            _ = shutdown.wait() => break 'delivery,
                            () = tokio::time::sleep(gap) => {}
                        }
                    } else {
                        tokio::time::sleep(gap).await;
                    }
                    let now = clock.elapsed_ns()?;
                    handle(btc_feeds::live::TimedIngestOutcome {
                        outcome: btc_feeds::IngestOutcome {
                            events: outcome.events,
                            notices: outcome.notices,
                        },
                        logical_receipt_ns: now,
                    })?;
                }
                report.records = records;
                report.events = events;
            }
            if let Some(mut shutdown) = shutdown {
                let _ = shutdown.wait().await;
            }
            Ok(report)
        })
    }
}

/// Run the Bybit lane in rotation mode over the paced fixture runner, returning the
/// lane root holding the sealed generations.
async fn run_bybit_paced_rotation(
    rotate_seconds: u64,
    duration_seconds: u64,
    gap: Duration,
) -> PathBuf {
    let spec = bybit_spec();
    let lane_root = rotation_lane_root("bybit-paced");
    let epoch = "bybit-paced-epoch".to_owned();
    let clock = btc_feeds::live::ProcessClock::start();
    let processor = spec
        .build_processor(&epoch, clock.wall_start())
        .expect("the bybit processor builds");
    let (_report, shutdown_reason) = run_lane_with_processor(
        LaneRunArguments {
            index_gating: Some(disarmed_index_gating()),
            spec,
            output: PathBuf::new(),
            journal: PathBuf::new(),
            checkpoint: lane_root.join("checkpoint.json"),
            resume_from_checkpoint: false,
            publisher_epoch: epoch,
            duration_seconds,
            checkpoint_interval_seconds: 1,
            max_updates: 5_000_000,
            max_output_bytes: 128 * 1024 * 1024,
            max_journal_bytes: DEFAULT_LANE_JOURNAL_MAX_BYTES,
            max_checkpoint_bytes: DEFAULT_LANE_CHECKPOINT_MAX_BYTES,
            poll_driver: None,
            stream_runner: Some(Box::new(PacedFixtureRunner { gap })),
            rotation: Some(LaneRotation {
                lane_root: lane_root.clone(),
                rotate_seconds,
                headroom_factor: 2,
            }),
        },
        clock,
        None,
        processor,
    )
    .await
    .expect("the paced rotation run completes");
    assert!(
        !shutdown_reason.interrupted(),
        "the paced rotation run reaches its deadline"
    );
    lane_root
}

/// The ordered book-source records of one generation's journal, each as
/// `(is_resync, first_update_kind)` — the fields the delta-first regression reads.
fn book_record_shape(journal: &Path) -> Vec<(bool, String)> {
    let mut reader = LaneJournalReplayReader::open(journal, DEFAULT_LANE_JOURNAL_MAX_BYTES)
        .expect("the generation journal opens");
    let mut shape = Vec::new();
    while let Some(input) = reader.next_input().expect("journal input reads") {
        if let btc_signals::lane_journal::LaneJournalInput::Events { events, .. } = &input
            && events
                .first()
                .is_some_and(|event| event.source_id == BYBIT_SPOT_BOOK_SOURCE)
        {
            let update_kind = events
                .first()
                .and_then(|event| event.values.get("update_kind"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned();
            shape.push((input.is_resync(), update_kind));
        }
    }
    shape
}

/// C5 regression: a Bybit generation whose journal begins (after its rotation resync
/// snapshot) with a real book DELTA replays standalone — the resync records rebuild
/// the book so the delta is contiguous, where a cold replay without them fatals with
/// `book delta arrived before a complete snapshot` (decision 23:30Z). Also asserts the
/// resync record is present and tagged, and that the replay reports the honest re-warm
/// gap rather than failing.
// `start_paused`: needs a rotation tick (+1 s) AND the paced runner's 120 ms-spaced
// events (a `tokio::time::sleep`, virtual-clock aware) to straddle a boundary so a
// later generation opens delta-first. Driving both off the virtual clock makes the
// straddle deterministic instead of host-speed dependent.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn bybit_delta_first_generation_replays_via_resync_records() {
    let lane_root = run_bybit_paced_rotation(1, 3, Duration::from_millis(120)).await;
    let dirs = generation_dirs(&lane_root);
    assert!(
        dirs.len() >= 2,
        "the paced run rotated at least once, got {}",
        dirs.len()
    );

    // Find a generation ≥ 1 whose book stream begins with a resync snapshot followed
    // by a real delta before any real snapshot — the delta-first scenario.
    let mut proven = false;
    for (index, dir) in dirs.iter().enumerate().skip(1) {
        let shape = book_record_shape(&dir.join("journal.jsonl"));
        let first_real = shape.iter().find(|(is_resync, _)| !*is_resync);
        let Some((_, first_real_kind)) = first_real else {
            continue;
        };
        if first_real_kind != "delta" {
            continue;
        }
        // The delta-first generation must open its book stream with a tagged resync
        // snapshot (the rebuild that makes the following delta contiguous on replay).
        let first_book = shape.first().expect("the generation carries book records");
        assert!(
            first_book.0 && first_book.1 == "snapshot",
            "gen {index}: the first book record is the resync snapshot, got {first_book:?}"
        );

        // The generation replays standalone: its book rebuilds from the resync records,
        // so the leading delta is contiguous (no `book delta arrived before a complete
        // snapshot` fatal). The terminal check is gap-aware for a rotated generation.
        let journal = dir.join("journal.jsonl");
        let reader = LaneJournalReplayReader::open(&journal, DEFAULT_LANE_JOURNAL_MAX_BYTES)
            .expect("the delta-first generation journal opens");
        let session = reader.session().clone();
        drop(reader);
        let wall_start = chrono::DateTime::parse_from_rfc3339(&session.wall_start)
            .expect("session wall_start parses")
            .with_timezone(&chrono::Utc);
        let processor = bybit_spec()
            .build_processor(&session.publisher_epoch, wall_start)
            .expect("the bybit processor rebuilds for replay");
        let replay_output = temp_path(&format!("bybit-delta-first-gen{index}-replay"));
        let report = replay_lane_with_processor(
            bybit_spec(),
            &journal,
            &replay_output,
            5_000_000,
            128 * 1024 * 1024,
            DEFAULT_LANE_JOURNAL_MAX_BYTES,
            processor,
        )
        .unwrap_or_else(|error| {
            panic!("gen {index}: delta-first generation replays via resync records: {error:#}")
        });
        // Honest re-warm (not a no-op): the rotated generation's rings re-warm cold, so
        // the terminal count is a REPORTED re-warm gap, never a fatal disagreement — and
        // the property that gap stands for is that the replay is never AHEAD of live.
        // Every Ready reading the replay produced was also produced by the warm live run
        // at that instant; a cold re-warm can only reach Ready with or after live, never
        // invent a decision live lacked.
        let live_ready = ready_readings(&read_frames(&dir.join("output.jsonl")));
        let replay_ready = ready_readings(&read_frames(&replay_output));
        for key in replay_ready.keys() {
            assert!(
                live_ready.contains_key(key),
                "gen {index}: delta-first replay reading {key:?} is absent from live \
                 (a re-warm must never run ahead of live); reported re-warm gap {}",
                report.rewarm_stream_message_gap
            );
        }
        let _ = std::fs::remove_file(replay_output);
        proven = true;
        break;
    }
    assert!(
        proven,
        "the paced run produced a generation whose book stream begins with a real delta \
         after its resync snapshot (spread the deltas wider if this ever fails)"
    );
    let _ = std::fs::remove_dir_all(lane_root);
}

// ---------------------------------------------------------------------------
// C5 Design B — the per-identity DECISION sequence restarts at zero each generation.
//
// `begin_generation` calls `LaneProcessor::reset_output_sequences` on the warm
// processor it carries across an in-place rotation boundary, and the trait contract
// (lane.rs) requires every PUBLISHING processor to override that default no-op: it
// must restart each per-identity emitter's decision counter for the fresh output
// epoch (a composite delegating to every sub-processor it owns). A publishing
// processor that drops the override silently carries a continued per-identity
// sequence into the new epoch — the reset stream frame still opens at sequence 0
// (the shell zeroes the STREAM sequence unconditionally), but the first Ready
// DECISION each identity publishes keeps counting, so the live output diverges from
// a replay of that generation.
//
// This drives the REAL shell rotation path (`run_lane_with_processor` with a
// `LaneRotation`, not a direct `reset_output_sequences` call) for a boxed
// `dyn LaneProcessor`, and asserts the first Ready decision every identity publishes
// after a boundary is sequence 0. It runs the composite oracle lane (the Chainlink
// reference plus its two C2 momentum/volatility emitters, reached through
// `OracleLaneProcessor`) and one ring-bearing venue lane; each fails this assertion
// if its processor omits the override. The matching cross-lane assertion lives in
// the cross-lane suite, where the cross fixtures live.
// ---------------------------------------------------------------------------

/// The decision `sequence` of the FIRST Ready `signal_batch` update each identity
/// publishes across `frames`. Ready decisions are the only `signal_batch` updates
/// (warming/degraded route to `instance_status`), so this reads exactly the
/// per-identity counter `reset_output_sequences` must restart at zero.
fn first_ready_decision_sequences(frames: &[Value]) -> std::collections::BTreeMap<String, u64> {
    let mut first: std::collections::BTreeMap<String, u64> = std::collections::BTreeMap::new();
    for frame in frames.iter().filter(|f| frame_type(f) == "signal_batch") {
        if let Some(updates) = frame.get("updates").and_then(Value::as_array) {
            for update in updates {
                let id = update
                    .get("signal_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let sequence = update
                    .get("sequence")
                    .and_then(Value::as_u64)
                    .unwrap_or_default();
                first.entry(id).or_insert(sequence);
            }
        }
    }
    first
}

/// Run a lane in rotation mode through the real shell (`run_lane_with_processor`
/// with a `LaneRotation`, so `begin_generation` runs at each boundary) over the
/// injected drivers, returning the lane root holding the sealed generations.
async fn run_rotation_with_drivers(
    spec: &'static lane::LaneSpec,
    label: &str,
    gating: LaneIndexGating,
    poll_driver: Option<Box<dyn PollDriver>>,
    stream_runner: Option<Box<dyn btc_signals::lane_runtime::StreamRunner>>,
    rotate_seconds: u64,
    duration_seconds: u64,
) -> PathBuf {
    let lane_root = rotation_lane_root(label);
    let epoch = format!("{label}-seq-reset-epoch");
    let clock = btc_feeds::live::ProcessClock::start();
    let processor = spec
        .build_processor(&epoch, clock.wall_start())
        .expect("the processor builds");
    let (_report, shutdown_reason) = run_lane_with_processor(
        LaneRunArguments {
            index_gating: Some(gating),
            spec,
            output: PathBuf::new(),
            journal: PathBuf::new(),
            checkpoint: lane_root.join("checkpoint.json"),
            resume_from_checkpoint: false,
            publisher_epoch: epoch,
            duration_seconds,
            checkpoint_interval_seconds: 1,
            max_updates: 5_000_000,
            max_output_bytes: 128 * 1024 * 1024,
            max_journal_bytes: DEFAULT_LANE_JOURNAL_MAX_BYTES,
            max_checkpoint_bytes: DEFAULT_LANE_CHECKPOINT_MAX_BYTES,
            poll_driver,
            stream_runner,
            rotation: Some(LaneRotation {
                lane_root: lane_root.clone(),
                rotate_seconds,
                headroom_factor: 2,
            }),
        },
        clock,
        None,
        processor,
    )
    .await
    .expect("the rotation run completes");
    assert!(
        !shutdown_reason.interrupted(),
        "{label}: the rotation run reaches its deadline"
    );
    lane_root
}

/// Assert every identity's first Ready decision in each rotation-opened generation
/// (index >= 1) restarts at sequence 0, and that at least one identity actually
/// re-published a Ready decision after a boundary (so the assertion is not vacuous).
fn assert_post_boundary_decisions_restart_at_zero(label: &str, lane_root: &Path) {
    let dirs = generation_dirs(lane_root);
    assert!(
        dirs.len() >= 2,
        "{label}: rotated at least once, got {} generations",
        dirs.len()
    );
    let mut post_boundary_ready = 0_usize;
    for (index, dir) in dirs.iter().enumerate().skip(1) {
        let frames = read_frames(&dir.join("output.jsonl"));
        for (id, sequence) in first_ready_decision_sequences(&frames) {
            assert_eq!(
                sequence, 0,
                "{label}: generation {index} identity {id} first Ready decision is sequence \
                 {sequence}, not 0 — a publishing processor that omits reset_output_sequences \
                 carries the counter across the boundary"
            );
            post_boundary_ready += 1;
        }
    }
    assert!(
        post_boundary_ready > 0,
        "{label}: at least one identity publishes a Ready decision after a boundary"
    );
}

// `start_paused`: `dirs.len() >= 2` plus a post-boundary Ready decision require the
// rotation tick and (for the venue lane) the interval decision to fire inside the 4 s
// window; drive them off the virtual clock so the post-boundary restart is always
// observed, not host-speed dependent (see `rotation_is_invisible_to_the_values_venue`).
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn rotation_restarts_each_published_identitys_decision_sequence() {
    // The composite oracle lane: the Chainlink poll serves an advancing round every
    // poll (so the reference and both C2 identities publish Ready in every
    // generation), its index streams connect and emit nothing (observed-not-consumed).
    // A missing override on ChainlinkOracleProcessor OR the OracleLaneProcessor
    // composite carries the reference/momentum/volatility counters across a boundary.
    let oracle_root = run_rotation_with_drivers(
        &lane::ORACLE,
        "oracle",
        disarmed_index_gating(),
        Some(Box::new(ScriptedRoundDriver {
            next_round: 1,
            base_secs: 1_756_400_000,
            advance: true,
        })),
        Some(Box::new(FixtureStreamRunner)),
        1,
        4,
    )
    .await;
    assert_post_boundary_decisions_restart_at_zero("oracle", &oracle_root);
    let _ = std::fs::remove_dir_all(&oracle_root);

    // One ring-bearing venue lane: the paced fixture runner spreads its spot /
    // derivatives events across generations so the identities stay Ready across a
    // boundary. The venue processor already overrides reset_output_sequences, so this
    // is the passing control that proves the harness observes the restart.
    let binance_root = run_rotation_with_drivers(
        &lane::BINANCE,
        "binance",
        disarmed_index_gating(),
        None,
        Some(Box::new(PacedFixtureRunner {
            gap: Duration::from_millis(40),
        })),
        1,
        4,
    )
    .await;
    assert_post_boundary_decisions_restart_at_zero("binance", &binance_root);
    let _ = std::fs::remove_dir_all(&binance_root);
}
