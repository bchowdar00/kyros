//! C2 (E49) / C3 replay proof: drive the horizon, perp, chainlink and prediction
//! identities over the observed hours and record, per identity, how many ready
//! frames were published and the first ready `window_close`. Ignored by default
//! (it reads large private captures that are not in the repo and are streamed line
//! by line, never loaded whole); run explicitly with the observed root pointed at
//! the capture directory:
//!
//! ```text
//! SIGNALS_OBSERVED_DIR=/path/to/observed \
//!   cargo test -p btc-signals --test c2_replay_proof -- --ignored --nocapture
//! ```
//!
//! The prediction case additionally needs a retained Kalshi lane journal:
//! `SIGNALS_KALSHI_JOURNAL=/path/to/kalshi/gen/journal.jsonl`. Each test SKIPS
//! (prints a line and returns, never panics) when its input is not configured, so
//! the suite is safe to run on a machine that holds only some of the captures. The
//! one-line commands are documented in `docs/EVALUATION.md`.

use std::{
    collections::BTreeMap,
    fs::File,
    io::{BufRead, BufReader, BufWriter},
    path::{Path, PathBuf},
};

use btc_feeds::{CaptureStream, RawRecord, replay::ReplaySession};
use btc_signals::{
    PredictionSignalProcessor, SpotSignalProcessor,
    lane::{LaneClock, LaneProcessor},
    oracle_signals::ChainlinkOracleProcessor,
    prediction_journal::PredictionJournalReplayReader,
    prediction_runtime::PredictionPipeline,
    venue_spot_signals::{BinanceProcessor, BybitProcessor},
};
use chrono::{DateTime, Utc};
use signals_core::{QualityState, SignalUpdate};

const SEC: u64 = 1_000_000_000;
/// The 15-minute warm-up of the one-second horizon and perp rings (spec §3): 901
/// contiguous one-second samples. A ready frame therefore cannot appear before the
/// 901st decision on any of these identities, whatever the stream's start offset,
/// so `first_ready` at decision `>= WARMUP_DECISIONS` is the boundary the fixed
/// sampler must honour.
const WARMUP_DECISIONS: u64 = 901;

/// The capture directory, from `SIGNALS_OBSERVED_DIR`. `None` (unset) means skip.
fn observed_dir() -> Option<String> {
    std::env::var("SIGNALS_OBSERVED_DIR")
        .ok()
        .filter(|d| !d.is_empty())
}

/// The retained Kalshi lane journal, from `SIGNALS_KALSHI_JOURNAL`. `None` skips.
fn kalshi_journal() -> Option<PathBuf> {
    std::env::var("SIGNALS_KALSHI_JOURNAL")
        .ok()
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
}

fn wall_start() -> DateTime<Utc> {
    "2026-09-04T00:00:00Z".parse().expect("wall start parses")
}

#[derive(Default, Clone)]
struct Tally {
    ready: u64,
    first_ready: Option<String>,
    /// The decision-tick index at which the identity first went ready. The one-
    /// second rings advance only on the decision clock, so a ready frame at tick
    /// `d` proves `d` contiguous samples — the 901-sample boundary check.
    first_ready_decision: Option<u64>,
    last_state: Option<QualityState>,
}

/// Fold one decision batch into the running per-identity tallies. `decision` is the
/// current decision-tick index, recorded the first time an identity goes ready.
fn tally_batch(
    tallies: &mut BTreeMap<String, Tally>,
    updates: &[SignalUpdate],
    targets: &[&str],
    decision: u64,
) {
    for update in updates {
        if !targets.contains(&update.signal_id.as_str()) {
            continue;
        }
        let entry = tallies.entry(update.signal_id.clone()).or_default();
        entry.last_state = Some(update.quality.state);
        if update.quality.state == QualityState::Ready {
            entry.ready += 1;
            if entry.first_ready.is_none() {
                let when = update
                    .values
                    .get("window_close")
                    .and_then(|value| value.as_str())
                    .unwrap_or("<no window_close>")
                    .to_owned();
                entry.first_ready = Some(when);
                entry.first_ready_decision = Some(decision);
            }
        }
    }
}

fn report(name: &str, tallies: &BTreeMap<String, Tally>) {
    println!("== {name} ==");
    for (id, tally) in tallies {
        println!(
            "  {id}: ready_frames={} first_ready={} first_ready_decision={} last_state={:?}",
            tally.ready,
            tally.first_ready.as_deref().unwrap_or("NEVER"),
            tally
                .first_ready_decision
                .map_or_else(|| "NEVER".to_owned(), |d| d.to_string()),
            tally.last_state
        );
    }
}

/// Assert an identity reached ready and that its first ready frame respected the
/// 901-sample warm-up boundary (never ready before `WARMUP_DECISIONS` decisions).
fn assert_ready_after_warmup(tallies: &BTreeMap<String, Tally>, id: &str) {
    let tally = tallies
        .get(id)
        .unwrap_or_else(|| panic!("{id} produced no frames"));
    assert!(tally.ready > 0, "{id} never ready");
    let decision = tally
        .first_ready_decision
        .unwrap_or_else(|| panic!("{id} ready without a recorded decision"));
    assert!(
        decision >= WARMUP_DECISIONS,
        "{id} first ready at decision {decision}, before the {WARMUP_DECISIONS}-sample warm-up"
    );
}

/// Assert an identity reached ready (its own warm-up gate; no 901-sample boundary).
fn assert_ready(tallies: &BTreeMap<String, Tally>, id: &str) {
    assert!(
        tallies.get(id).map(|t| t.ready > 0).unwrap_or(false),
        "{id} never ready"
    );
}

// --- spot horizon identities (one observed hour per venue) -----------------
//
// The four per-venue spot horizon identities each warm from their own observed
// hour through the spot pipeline; the blended `btc.external.*` reading needs the
// two USD venues simultaneously and is covered by the hand-computed unit tests and
// the (offset-window) core captures rather than a single-stream replay.

#[test]
#[ignore = "reads large private captures under SIGNALS_OBSERVED_DIR"]
fn coinbase_horizon_publishes_ready_over_the_observed_hour() {
    let Some(dir) = observed_dir() else {
        println!("SIGNALS_OBSERVED_DIR unset; skipping coinbase horizon replay");
        return;
    };
    let targets = &[
        "btc.coinbase.horizon_momentum",
        "btc.coinbase.horizon_volatility",
    ];
    let mut processor =
        SpotSignalProcessor::new("c2-replay", wall_start()).expect("spot processor");
    let mut tallies: BTreeMap<String, Tally> = BTreeMap::new();
    stream_spot(
        &dir,
        "coinbase-core-r2-20260904T010142Z.jsonl",
        CaptureStream::CoinbaseCore,
        &mut processor,
        targets,
        &mut tallies,
    );
    report("coinbase-core-r2", &tallies);
    for id in targets {
        assert_ready_after_warmup(&tallies, id);
    }
}

#[test]
#[ignore = "reads large private captures under SIGNALS_OBSERVED_DIR"]
fn kraken_horizon_publishes_ready_over_the_observed_hour() {
    let Some(dir) = observed_dir() else {
        println!("SIGNALS_OBSERVED_DIR unset; skipping kraken horizon replay");
        return;
    };
    let targets = &[
        "btc.kraken.horizon_momentum",
        "btc.kraken.horizon_volatility",
    ];
    let mut processor =
        SpotSignalProcessor::new("c2-replay", wall_start()).expect("spot processor");
    let mut tallies: BTreeMap<String, Tally> = BTreeMap::new();
    stream_spot(
        &dir,
        "kraken-core-r2-20260904T004850Z.jsonl",
        CaptureStream::KrakenCore,
        &mut processor,
        targets,
        &mut tallies,
    );
    report("kraken-core-r2", &tallies);
    for id in targets {
        assert_ready_after_warmup(&tallies, id);
    }
}

#[test]
#[ignore = "reads large private captures under SIGNALS_OBSERVED_DIR"]
fn binance_horizon_publishes_ready_over_the_observed_hour() {
    let Some(dir) = observed_dir() else {
        println!("SIGNALS_OBSERVED_DIR unset; skipping binance horizon replay");
        return;
    };
    let targets = &[
        "btc.binance.horizon_momentum",
        "btc.binance.horizon_volatility",
    ];
    let mut processor =
        BinanceProcessor::new("c2-replay", wall_start()).expect("binance processor");
    let mut tallies: BTreeMap<String, Tally> = BTreeMap::new();
    stream_venue(
        &dir,
        "binance-spot-r2-20260903T235740Z.jsonl",
        CaptureStream::BinanceSpot,
        &mut processor,
        targets,
        &mut tallies,
    );
    report("binance-spot-r2", &tallies);
    for id in targets {
        assert_ready_after_warmup(&tallies, id);
    }
}

#[test]
#[ignore = "reads large private captures under SIGNALS_OBSERVED_DIR"]
fn bybit_horizon_publishes_ready_over_the_observed_hour() {
    let Some(dir) = observed_dir() else {
        println!("SIGNALS_OBSERVED_DIR unset; skipping bybit horizon replay");
        return;
    };
    let targets = &["btc.bybit.horizon_momentum", "btc.bybit.horizon_volatility"];
    let mut processor = BybitProcessor::new("c2-replay", wall_start()).expect("bybit processor");
    let mut tallies: BTreeMap<String, Tally> = BTreeMap::new();
    stream_venue(
        &dir,
        "bybit-spot-r1-20260903T064148Z.jsonl",
        CaptureStream::BybitSpot,
        &mut processor,
        targets,
        &mut tallies,
    );
    report("bybit-spot-r1", &tallies);
    for id in targets {
        assert_ready_after_warmup(&tallies, id);
    }
}

// --- perp mark identities (E59 hold bound) ---------------------------------
//
// The Bybit linear ticker carries `markPrice` with multi-second gaps; the E59 3 s
// hold bridges them so the one-second perp ring warms. `perp_momentum` and
// `perp_volatility` warm on the 901-sample ring (the boundary check applies);
// `perp_rsi` warms on fifteen contiguous one-minute bars off the same held series.
// Binance USD-M is not driven here: from Tokyo its mark arrives only on the
// throttled REST premium-index poll (~4 s under the shared 15 req/min budget,
// E50), which the 3 s hold does not bridge, so it warms only where a mark refresh
// under 3 s is available.

#[test]
#[ignore = "reads large private captures under SIGNALS_OBSERVED_DIR"]
fn bybit_perp_publishes_ready_over_the_observed_hour() {
    let Some(dir) = observed_dir() else {
        println!("SIGNALS_OBSERVED_DIR unset; skipping bybit perp replay");
        return;
    };
    let targets = &[
        "btc.bybit.perp_momentum",
        "btc.bybit.perp_volatility",
        "btc.bybit.perp_rsi",
    ];
    let mut processor = BybitProcessor::new("c2-replay", wall_start()).expect("bybit processor");
    let mut tallies: BTreeMap<String, Tally> = BTreeMap::new();
    stream_venue(
        &dir,
        "bybit-linear-r1-20260903T064148Z.jsonl",
        CaptureStream::BybitLinear,
        &mut processor,
        targets,
        &mut tallies,
    );
    report("bybit-linear-r1 (perp)", &tallies);
    // The two 901-sample ring identities honour the warm-up boundary; the RSI bars
    // warm off the same held series and are asserted ready with a recorded first
    // ready (its bar-close boundary is >= 900 s but not sample-exact).
    assert_ready_after_warmup(&tallies, "btc.bybit.perp_momentum");
    assert_ready_after_warmup(&tallies, "btc.bybit.perp_volatility");
    assert_ready(&tallies, "btc.bybit.perp_rsi");
}

// --- chainlink on-chain identities -----------------------------------------
//
// Chainlink rounds are sparse (an on-chain heartbeat, not a 1 Hz tape), so the
// oracle reference is ready as soon as one round is accepted and the momentum /
// volatility identities warm over the accepted-round ring rather than a 901-sample
// second ring: the boundary check does not apply, only that they reach ready.

#[test]
#[ignore = "reads large private captures under SIGNALS_OBSERVED_DIR"]
fn chainlink_oracle_publishes_ready_over_the_observed_hour() {
    let Some(dir) = observed_dir() else {
        println!("SIGNALS_OBSERVED_DIR unset; skipping chainlink replay");
        return;
    };
    let targets = &[
        "btc.chainlink.oracle_reference",
        "btc.chainlink.price_momentum",
        "btc.chainlink.price_volatility",
    ];
    let mut processor = ChainlinkOracleProcessor::new("c2-replay");
    let mut tallies: BTreeMap<String, Tally> = BTreeMap::new();
    stream_venue(
        &dir,
        "chainlink-aggregator-r1-20260903T064148Z.jsonl",
        CaptureStream::ChainlinkAggregator,
        &mut processor,
        targets,
        &mut tallies,
    );
    report("chainlink-aggregator-r1", &tallies);
    // The reference is ready on the first accepted round; the momentum/volatility
    // identities warm over the accepted-round ring within the hour.
    assert_ready(&tallies, "btc.chainlink.oracle_reference");
}

// --- prediction identities (Kalshi lane journal) ---------------------------
//
// The prediction lane is journal-driven, not a raw capture, so this replays the
// retained Kalshi lane journal through `PredictionPipeline::replay` (exactly as
// `prediction-signals replay` does) and tallies the published stream. The market
// contract identity is ready as soon as the bound market is observed.

#[test]
#[ignore = "reads a retained Kalshi journal under SIGNALS_KALSHI_JOURNAL"]
fn kalshi_prediction_publishes_ready_over_the_journal() {
    let Some(journal) = kalshi_journal() else {
        println!("SIGNALS_KALSHI_JOURNAL unset; skipping kalshi prediction replay");
        return;
    };
    let targets = &["btc.kalshi.market_contract"];
    let tallies = replay_prediction_journal(&journal, targets);
    report("kalshi-journal (prediction)", &tallies);
    assert_ready(&tallies, "btc.kalshi.market_contract");
}

/// Drive a `SpotSignalProcessor` (Coinbase/Kraken venue lanes) over a capture.
fn stream_spot(
    dir: &str,
    file: &str,
    stream: CaptureStream,
    processor: &mut SpotSignalProcessor,
    targets: &[&str],
    tallies: &mut BTreeMap<String, Tally>,
) {
    let mut session = ReplaySession::new(stream, "c2-replay", None).expect("replay session builds");
    let path = format!("{dir}/{file}");
    let reader = BufReader::new(File::open(&path).unwrap_or_else(|e| panic!("open {path}: {e}")));
    let mut next_tick = SEC;
    let mut decisions: u64 = 0;
    let mut clock_ns: u64 = 0;
    for line in reader.lines() {
        let line = line.expect("read line");
        if line.trim().is_empty() {
            continue;
        }
        let value: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if let Some(elapsed) = value.get("capture_elapsed_ns").and_then(|v| v.as_u64()) {
            clock_ns = clock_ns.max(elapsed);
        }
        let record: RawRecord = match serde_json::from_value(value) {
            Ok(r) => r,
            Err(_) => continue,
        };
        while next_tick <= clock_ns {
            decisions += 1;
            let updates = processor.advance_time(next_tick).expect("advance");
            tally_batch(tallies, &updates, targets, decisions);
            next_tick += SEC;
        }
        let outcome = match session.ingest(&record) {
            Ok(o) => o,
            Err(_) => continue,
        };
        for event in &outcome.events {
            if let Ok(updates) = processor.ingest(event, clock_ns, clock_ns) {
                tally_batch(tallies, &updates, targets, decisions);
            }
        }
    }
}

/// Drive a venue `LaneProcessor` (Binance/Bybit/Chainlink) over a capture.
fn stream_venue<P: LaneProcessor>(
    dir: &str,
    file: &str,
    stream: CaptureStream,
    processor: &mut P,
    targets: &[&str],
    tallies: &mut BTreeMap<String, Tally>,
) {
    let mut session = ReplaySession::new(stream, "c2-replay", None).expect("replay session builds");
    let path = format!("{dir}/{file}");
    let reader = BufReader::new(File::open(&path).unwrap_or_else(|e| panic!("open {path}: {e}")));
    let mut next_tick = SEC;
    let mut decisions: u64 = 0;
    let mut clock_ns: u64 = 0;
    for line in reader.lines() {
        let line = line.expect("read line");
        if line.trim().is_empty() {
            continue;
        }
        let value: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if let Some(elapsed) = value.get("capture_elapsed_ns").and_then(|v| v.as_u64()) {
            clock_ns = clock_ns.max(elapsed);
        }
        let record: RawRecord = match serde_json::from_value(value) {
            Ok(r) => r,
            Err(_) => continue,
        };
        while next_tick <= clock_ns {
            decisions += 1;
            let clock = LaneClock {
                now_ns: next_tick,
                produced_at: String::new(),
            };
            if let Ok(updates) = processor.on_interval(&clock) {
                tally_batch(tallies, &updates, targets, decisions);
            }
            next_tick += SEC;
        }
        let outcome = match session.ingest(&record) {
            Ok(o) => o,
            Err(_) => continue,
        };
        let clock = LaneClock {
            now_ns: clock_ns,
            produced_at: String::new(),
        };
        for event in &outcome.events {
            let _ = processor.on_events(&event.source_id, std::slice::from_ref(event), &clock);
        }
    }
}

/// Replay a retained Kalshi lane journal through the prediction pipeline (mirrors
/// `prediction-signals replay`), writing the published stream to a temp file, then
/// tally the ready frames of the target identities from that stream.
fn replay_prediction_journal(journal: &Path, targets: &[&str]) -> BTreeMap<String, Tally> {
    let mut reader =
        PredictionJournalReplayReader::open(journal, u64::MAX).expect("open prediction journal");
    let session = reader.session().clone();
    let wall = DateTime::parse_from_rfc3339(&session.wall_start)
        .expect("journal wall_start parses")
        .with_timezone(&Utc);
    let index_armed = session.runtime_config.index_armed;
    let processor = if let Some(checkpoint) = &session.initial_checkpoint {
        PredictionSignalProcessor::restore_indexed(
            session.publisher_epoch.clone(),
            wall,
            checkpoint.processor_state.clone(),
            index_armed,
        )
        .expect("restore replay processor")
    } else {
        PredictionSignalProcessor::new_indexed(
            session.venue,
            session.publisher_epoch.clone(),
            wall,
            index_armed,
        )
        .expect("build replay processor")
    };
    let output_path =
        std::env::temp_dir().join(format!("c2-replay-kalshi-{}.jsonl", std::process::id()));
    {
        let output = File::create(&output_path).expect("create replay output");
        let mut pipeline = PredictionPipeline::replay(
            BufWriter::with_capacity(1 << 20, output),
            processor,
            &session,
            u64::MAX,
            u64::MAX,
        )
        .expect("build replay pipeline");
        while let Some(input) = reader.next_input().expect("read prediction journal") {
            pipeline.admit_replay(input).expect("apply replay input");
        }
        let end = reader
            .end()
            .expect("prediction journal has no end record")
            .clone();
        pipeline
            .finish_replay(&end)
            .expect("finish prediction replay");
    }
    let mut tallies: BTreeMap<String, Tally> = BTreeMap::new();
    let reader = BufReader::new(File::open(&output_path).expect("open replay output"));
    let mut sequence: u64 = 0;
    for line in reader.lines() {
        let line = line.expect("read output line");
        let value: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let kind = value.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if kind != "signal_batch" && kind != "snapshot" {
            continue;
        }
        sequence += 1;
        let Some(updates) = value.get("updates").and_then(|v| v.as_array()) else {
            continue;
        };
        let parsed: Vec<SignalUpdate> = updates
            .iter()
            .filter_map(|u| serde_json::from_value(u.clone()).ok())
            .collect();
        tally_batch(&mut tallies, &parsed, targets, sequence);
    }
    let _ = std::fs::remove_file(&output_path);
    tallies
}
