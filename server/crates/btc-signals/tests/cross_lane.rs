//! C1 phase-2 cross-lane integration tests (ruling E52): the `--lane cross` arm
//! runs the embedded spot-reference + Bybit-derivatives processor over its full
//! `LaneSpec` stream set, replaying bounded excerpts of the observed hours through
//! the shell. It refuses when Bybit is unlisted (R7), refuses a foreign source and
//! a consumed-source-with-no-producer (F1), and its journal replays byte-identically.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use btc_signals::exit_code::ExitClass;
use btc_signals::lane::{self, LaneSpec, LaneStream};
use btc_signals::lane_checkpoint::{DEFAULT_LANE_CHECKPOINT_MAX_BYTES, load_lane_checkpoint};
use btc_signals::lane_gate::{IndexSocket, admit_lane_with};
use btc_signals::lane_journal::DEFAULT_LANE_JOURNAL_MAX_BYTES;
use btc_signals::lane_runtime::{LaneRunArguments, replay_lane, run_lane};
use signals_core::{CROSS_LANE, EnabledVenues};

fn temp_path(label: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock follows epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "cross-lane-{label}-{}-{unique}.jsonl",
        std::process::id()
    ))
}

fn enabled(raw: &str) -> EnabledVenues {
    EnabledVenues::parse(Some(raw)).expect("allow-list parses")
}

/// The bounded observed excerpt for a cross-lane stream. Fixtures live in the
/// btc-feeds crate and are shared with its completeness test.
fn cross_fixture_for(stream: btc_feeds::CaptureStream) -> Option<&'static str> {
    use btc_feeds::CaptureStream;
    match stream {
        CaptureStream::CoinbaseCore => Some(include_str!(
            "../../btc-feeds/tests/fixtures/observed/coinbase_core.jsonl"
        )),
        CaptureStream::KrakenCore => Some(include_str!(
            "../../btc-feeds/tests/fixtures/observed/kraken_core.jsonl"
        )),
        CaptureStream::BybitLinear => Some(include_str!(
            "../../btc-feeds/tests/fixtures/observed/bybit_linear.jsonl"
        )),
        CaptureStream::BybitLinearRest => Some(include_str!(
            "../../btc-feeds/tests/fixtures/observed/bybit_linear_rest.jsonl"
        )),
        CaptureStream::KrakenUsdtReference => Some(include_str!(
            "../../btc-feeds/tests/fixtures/observed/kraken_usdt_reference.jsonl"
        )),
        _ => None,
    }
}

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

/// Replays each declared stream's observed excerpt through the lane shell, so the
/// cross processor sees real venue frames. An optional foreign source proves the
/// genuine-bug refusal path.
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
            if let Some(capture) = cross_fixture_for(stream) {
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
            if let Some(foreign) = &self.foreign_source
                && cross_fixture_for(stream).is_some()
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

#[test]
fn cross_lane_refuses_unlisted_bybit_and_admits_once_listed() {
    // R7: bybit is the rights-gated venue on the cross lane's wire.
    assert_eq!(
        admit_lane_with(
            &CROSS_LANE,
            IndexSocket::NotConfigured,
            &enabled("coinbase,kraken")
        ),
        Err(ExitClass::Fatal),
        "the cross lane refuses when bybit is unlisted"
    );
    assert_eq!(ExitClass::Fatal.code(), 78);
    assert!(
        admit_lane_with(
            &CROSS_LANE,
            IndexSocket::NotConfigured,
            &enabled("coinbase,kraken,bybit")
        )
        .is_ok(),
        "the cross lane admits once bybit is listed"
    );
    // Binance is NOT required (E66: the cross lane is Bybit-only; it carries no
    // Binance streams and never asks for a binance token).
    assert!(
        admit_lane_with(
            &CROSS_LANE,
            IndexSocket::NotConfigured,
            &enabled("coinbase,kraken,bybit")
        )
        .is_ok(),
        "the cross lane needs no binance token"
    );
}

#[test]
fn shipped_cross_spec_has_an_empty_refusal_set() {
    // Every required source is produced by a declared driver stream, so start-up
    // never refuses the shipped cross lane (F1). The Kraken USDT/USD reference is
    // deliberately not required (feed-health only). The cross lane is Bybit-only
    // (E66): there are no Binance streams or legs to require.
    use std::collections::BTreeSet;
    let processor = lane::CROSS
        .build_processor("cross-inventory", chrono::DateTime::UNIX_EPOCH)
        .expect("the cross processor builds");
    let mut produced: BTreeSet<&str> = BTreeSet::new();
    let lane::LaneDriverSpec::Streams(streams) = &lane::CROSS.driver else {
        panic!("the cross lane is a Streams lane");
    };
    for stream in *streams {
        produced.extend(stream.stream.produced_source_ids().iter().copied());
    }
    for stream in lane::CROSS.reference_streams {
        produced.extend(stream.stream.produced_source_ids().iter().copied());
    }
    let required = processor.required_sources();
    let missing: Vec<&str> = required
        .iter()
        .map(String::as_str)
        .filter(|id| !produced.contains(id))
        .collect();
    assert!(
        missing.is_empty(),
        "the shipped cross spec requires only produced sources, missing: {missing:?}"
    );
}

async fn run_cross_lane_over_fixtures(
    label: &str,
) -> (
    btc_signals::lane_runtime::LaneRunReport,
    PathBuf,
    PathBuf,
    PathBuf,
) {
    run_cross_lane_over_fixtures_for(label, 2).await
}

/// The publisher epoch each fixture run stamps, derived from its label so the
/// checkpoint round-trip can resume under the identical epoch.
fn fixture_epoch(label: &str) -> String {
    format!("{label}-epoch")
}

async fn run_cross_lane_over_fixtures_for(
    label: &str,
    duration_seconds: u64,
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
        rotation: None,
        spec: &lane::CROSS,
        output: output.clone(),
        journal: journal.clone(),
        checkpoint: checkpoint.clone(),
        resume_from_checkpoint: false,
        publisher_epoch: fixture_epoch(label),
        duration_seconds,
        checkpoint_interval_seconds: 1,
        max_updates: 1_000_000,
        max_output_bytes: 64 * 1024 * 1024,
        max_journal_bytes: DEFAULT_LANE_JOURNAL_MAX_BYTES,
        max_checkpoint_bytes: DEFAULT_LANE_CHECKPOINT_MAX_BYTES,
        poll_driver: None,
        stream_runner: Some(Box::new(ReplayFixtureRunner {
            foreign_source: None,
        })),
    })
    .await
    .expect("the cross lane replays its observed streams without aborting");
    assert!(
        !shutdown_reason.interrupted(),
        "the cross lane reaches its natural deadline"
    );
    (report, output, journal, checkpoint)
}

// `start_paused` drives the run off tokio's virtual clock (the same clock
// `ProcessClock` samples through `tokio::time::Instant`), so the maintenance health
// tick that emits the interval decision fires deterministically when the runtime
// idles after the front-loaded replay — never racing a wall-clock deadline on a
// slow/contended CI runner (where the single +1 s tick could land after the +2 s
// deadline, leaving the three cross identities' `instance_status` frames unwritten).
// Same idiom as `venue_lane::a_slow_poll_interleaved_with_health_ticks_replays_byte_identically`.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn cross_lane_replays_its_observed_streams_byte_identically() {
    let (report, output, journal, checkpoint) = run_cross_lane_over_fixtures("cross-replay").await;

    // F3: the journal/checkpoint identity names the cross processor that ran.
    let document = load_lane_checkpoint(&checkpoint, DEFAULT_LANE_CHECKPOINT_MAX_BYTES)
        .expect("checkpoint loads");
    assert_eq!(document.processor_id, "btc.cross");

    // F1: the sources the declared streams produce that no cross identity consumes
    // are counted (`ignored_events`), never aborted on — the Bybit depth book and
    // the Coinbase / Kraken book streams the spot reference does not read.
    let ignored = &report.pipeline.feed_health.ignored_events;
    for source in [
        "bybit.linear.btcusdt.book-levels",
        "coinbase.exchange.btc-usd.book-levels",
    ] {
        assert!(
            ignored.get(source).is_some_and(|count| *count > 0),
            "{source} is observed-not-consumed and counted, got {ignored:?}"
        );
    }

    // The three cross identities are published (E66: Bybit-only). In this bounded
    // window perp_spot_basis and derivatives_regime publish as instance_status —
    // the 60 s basis/funding/OI warm-up cannot complete over the front-loaded
    // fixture — while liquidation_vs_spot_response awaits a cascade; their Ready
    // paths are proven at the evaluator seam in the cross_signals unit tests.
    let bytes = std::fs::read(&output).expect("output reads");
    let published: std::collections::BTreeSet<String> = String::from_utf8_lossy(&bytes)
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter_map(|value| {
            value
                .get("signal_id")
                .and_then(|id| id.as_str())
                .map(str::to_owned)
        })
        .collect();
    for id in [
        "btc.cross.perp_spot_basis",
        "btc.cross.liquidation_vs_spot_response",
        "btc.cross.derivatives_regime",
    ] {
        assert!(
            published.contains(id),
            "the cross lane publishes {id}, saw {published:?}"
        );
    }

    // E66: no binance_* field or reason (indeed no "binance" token at all) can
    // appear anywhere in the Bybit-only cross lane's output.
    assert!(
        !String::from_utf8_lossy(&bytes).contains("binance"),
        "no binance_* field or reason may appear on the Bybit-only cross lane"
    );

    // The journal replays byte-identically to the live output (bytes, not a count).
    let replay_output = temp_path("cross-replay-out");
    let _ = replay_lane(
        &lane::CROSS,
        &journal,
        &replay_output,
        1_000_000,
        64 * 1024 * 1024,
        DEFAULT_LANE_JOURNAL_MAX_BYTES,
    )
    .expect("the cross journal replays");
    let replay_bytes = std::fs::read(&replay_output).expect("replay output reads");
    assert_eq!(
        bytes, replay_bytes,
        "the cross journal replays byte-identically to the live output"
    );

    for path in [output, journal, checkpoint, replay_output] {
        let _ = std::fs::remove_file(path);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn cross_lane_refuses_a_source_it_neither_consumes_nor_ignores() {
    // A source that is neither consumed nor produced by a declared stream (a
    // wrong-lane frame) is a genuine bug the dispatch refuses (F1): the lane aborts.
    let output = temp_path("cross-foreign-output");
    let journal = temp_path("cross-foreign-journal");
    let checkpoint = temp_path("cross-foreign-checkpoint");
    let result = run_lane(LaneRunArguments {
        index_gating: None,
        rotation: None,
        spec: &lane::CROSS,
        output: output.clone(),
        journal: journal.clone(),
        checkpoint: checkpoint.clone(),
        resume_from_checkpoint: false,
        publisher_epoch: "cross-foreign-epoch".to_owned(),
        duration_seconds: 2,
        checkpoint_interval_seconds: 1,
        max_updates: 1_000_000,
        max_output_bytes: 64 * 1024 * 1024,
        max_journal_bytes: DEFAULT_LANE_JOURNAL_MAX_BYTES,
        max_checkpoint_bytes: DEFAULT_LANE_CHECKPOINT_MAX_BYTES,
        poll_driver: None,
        stream_runner: Some(Box::new(ReplayFixtureRunner {
            foreign_source: Some("binance.spot.btcusdt.book-ticker".to_owned()),
        })),
    })
    .await;
    assert!(
        result.is_err(),
        "a foreign source on the cross lane aborts the run"
    );
    for path in [output, journal, checkpoint] {
        let _ = std::fs::remove_file(path);
    }
}

/// A cross-lane spec missing the Bybit streams: the cross processor requires the
/// Bybit derivatives sources, which no declared stream produces, so start-up
/// refuses (F1 consumed-without-producer guard). The `requirement` names the
/// `cross` lane so `build_processor` still builds the real cross processor.
static CROSS_MISSING_BYBIT: LaneSpec = LaneSpec {
    requirement: &CROSS_LANE,
    venues: &["coinbase", "kraken"],
    processor_id: "btc.cross",
    processor_version: "1.0.0",
    driver: lane::LaneDriverSpec::Streams(&[
        LaneStream {
            token: "coinbase",
            stream: btc_feeds::CaptureStream::CoinbaseCore,
            label: "coinbase_core",
        },
        LaneStream {
            token: "kraken",
            stream: btc_feeds::CaptureStream::KrakenCore,
            label: "kraken_core",
        },
    ]),
    reference_streams: &[lane::KRAKEN_USDT_REFERENCE_STREAM],
    duration_default: 2,
};

#[tokio::test(flavor = "current_thread")]
async fn cross_lane_refuses_when_a_required_source_has_no_producer() {
    let output = temp_path("cross-missing-output");
    let journal = temp_path("cross-missing-journal");
    let checkpoint = temp_path("cross-missing-checkpoint");
    let result = run_lane(LaneRunArguments {
        index_gating: None,
        rotation: None,
        spec: &CROSS_MISSING_BYBIT,
        output: output.clone(),
        journal: journal.clone(),
        checkpoint: checkpoint.clone(),
        resume_from_checkpoint: false,
        publisher_epoch: "cross-missing-epoch".to_owned(),
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
    })
    .await;
    let error = result.expect_err("a required source with no producer refuses at start-up");
    let message = format!("{error:#}");
    assert!(
        message.contains("requires sources no declared stream produces"),
        "the refusal names the misconfiguration, got: {message}"
    );
    // The refusal happens before any journal or output is written.
    assert!(!journal.exists(), "no journal before the refusal");
    assert!(!output.exists(), "no output before the refusal");
    for path in [output, journal, checkpoint] {
        let _ = std::fs::remove_file(path);
    }
}

/// The cross-owned rolling fields a resume must restore verbatim (Audit M): the
/// two horizon rings, the basis warm-up clock and the tracked-cascade bookkeeping.
/// Extracted from the opaque checkpoint value so the round-trip is compared on the
/// state the audit flagged (dropping any of these would silently break a resume),
/// not on the whole document, whose embedded engine sub-states are re-derived.
fn cross_rolling_fields(state: &serde_json::Value) -> Vec<(&'static str, serde_json::Value)> {
    [
        "basis_ring",
        "spot_ring",
        "basis_warm_since",
        "cascade",
        "burst_sequence",
        "last_clustering_bybit",
    ]
    .into_iter()
    .map(|field| {
        (
            field,
            state
                .get(field)
                .unwrap_or_else(|| panic!("checkpoint carries {field}"))
                .clone(),
        )
    })
    .collect()
}

/// A tracked-cascade JSON object matching `TrackedCascade` (cross_signals.rs), a
/// long cascade with both horizons resolved — the same shape the unit test
/// `ready_liquidation_frame_is_deterministic_and_checkpoint_value_preserving`
/// seeds. Decimal fields are already `normalize()`d so they survive the ring/text
/// round-trip verbatim.
fn seeded_cascade() -> serde_json::Value {
    serde_json::json!({
        "burst_sequence": 7,
        "burst_now_ns": 100_000_000_000_u64,
        "burst_time": "2026-09-04T00:01:40.000000000Z",
        "burst_venue": "bybit",
        "burst_side": "longs_liquidated",
        "burst_side_skew": "0.6",
        "burst_notional_usd": "1250000",
        "burst_venue_count": 1,
        "feed_completeness": "complete",
        "reference_price_at_burst": "65000",
        "reference_price_at_burst_dec": "65000",
        "expected_direction": "down",
        "response_bps_5s": "-5",
        "response_bps_30s": "-10",
    })
}

#[tokio::test(flavor = "current_thread")]
async fn cross_lane_checkpoint_round_trips_the_embedded_engines() {
    // A resume must restate the cross rolling state (Audit M) — the two horizon
    // rings, the basis warm-up clock and the tracked cascade — rather than silently
    // re-warm from empty. A bounded fixture run cannot warm those (the spot leg never
    // reaches ready without a live book, the Bybit basis/funding/OI legs need the
    // embedded engine's 60 s warm-up over front-loaded data, and the liquidation
    // cascade needs its 300 s warm-up — E66 makes the lane Bybit-only), so this
    // seeds a warmed cascade + rings into the real lane checkpoint's opaque state and
    // asserts they survive restore, mirroring the unit test `ready_liquidation_frame_
    // is_deterministic_and_checkpoint_value_preserving` at the lane-document seam.
    // (An earlier version asserted only that restore returned Ok, which a resume that
    // dropped every ring would also have passed.)
    let label = "cross-checkpoint";
    let (_report, output, journal, checkpoint) = run_cross_lane_over_fixtures_for(label, 2).await;
    let document = load_lane_checkpoint(&checkpoint, DEFAULT_LANE_CHECKPOINT_MAX_BYTES)
        .expect("checkpoint loads");
    assert_eq!(document.processor_id, "btc.cross");

    // Seed non-empty rolling state into the checkpoint the run wrote, leaving every
    // embedded-engine sub-state (spot reference, Bybit features, emitters) intact.
    // Overwriting values only (no keys added) keeps `CrossCheckpointState`'s
    // deny_unknown_fields contract satisfied.
    let mut seeded = document.processor_state.clone();
    let state = seeded
        .as_object_mut()
        .expect("checkpoint state is an object");
    state.insert(
        "spot_ring".to_owned(),
        serde_json::json!([
            [100_000_000_000_u64, "65000"],
            [100_005_000_000_u64, "64967.5"],
            [100_030_000_000_u64, "64935"],
        ]),
    );
    state.insert(
        "basis_ring".to_owned(),
        serde_json::json!([[100_000_000_000_u64, "1.5"], [100_030_000_000_u64, "1.25"],]),
    );
    state.insert(
        "basis_warm_since".to_owned(),
        serde_json::json!(100_000_000_000_u64),
    );
    state.insert("burst_sequence".to_owned(), serde_json::json!(7));
    state.insert(
        "last_clustering_bybit".to_owned(),
        serde_json::json!("cascade"),
    );
    state.insert("cascade".to_owned(), seeded_cascade());
    let seeded_fields = cross_rolling_fields(&seeded);

    // Restore the seeded checkpoint into a fresh cross processor (built the way a
    // resume builds it, from the document's own wall clock and a NEW epoch — a
    // resume must not reuse an epoch already in the lineage), then re-checkpoint:
    // the seeded rolling state must come back unchanged, proving the rings, warm-up
    // clock and cascade are restored, not dropped or re-warmed. The compared fields
    // are epoch-independent, so the fresh epoch does not perturb them.
    let wall_start = chrono::DateTime::parse_from_rfc3339(&document.wall_start)
        .expect("checkpoint wall_start parses")
        .with_timezone(&chrono::Utc);
    let mut restored = lane::CROSS
        .build_processor(&format!("{label}-resume"), wall_start)
        .expect("the cross processor builds for resume");
    restored
        .restore_state(seeded)
        .expect("the seeded checkpoint state restores");
    let resumed = restored.checkpoint_state();
    assert_eq!(
        cross_rolling_fields(&resumed),
        seeded_fields,
        "the cross rolling state (rings, warm-up clock, tracked cascade) round-trips"
    );

    for path in [output, journal, checkpoint] {
        let _ = std::fs::remove_file(path);
    }
}
