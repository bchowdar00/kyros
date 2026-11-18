//! C5 in-process file-generation rotation for `prediction-signals run`.
//!
//! These drive the live prediction pipeline (`admit_live` + `PredictionPipeline::
//! rotate`) over real generation directories, exactly as the binary's `maintain`
//! rotation timer does, and prove:
//!  1. rotation is invisible to the decision values (the warm processor, rings and
//!     the E37c pending slot survive the in-place swap), including a rotation
//!     boundary placed inside the E37c pending slot;
//!  2. each generation's journal replays byte-identically to its own output for
//!     the identities the checkpoint carries (generation 0 fully; ring identities
//!     honestly re-warm on a rotated generation's replay, which is C5b's remit);
//!  3. replaying generation N+1's journal from the checkpoint sealed at N
//!     reproduces N+1's output on the same terms;
//!  4. the rotation-mode CLI mutual-exclusion and defaults.
//!
//! The two-epoch split is asserted throughout: source events keep the process
//! admission epoch while each generation publishes under its own output epoch.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use btc_signals::PredictionSignalProcessor;
use btc_signals::file_generation::{
    FileGenerationCaps, FileGenerationLayout, OpenFileGeneration, RunMeta,
};
use btc_signals::prediction_checkpoint::{
    DEFAULT_PREDICTION_CHECKPOINT_MAX_BYTES, PredictionCheckpointDocument,
    load_prediction_checkpoint, write_prediction_checkpoint,
};
use btc_signals::prediction_journal::{
    PredictionJournalInput, PredictionJournalReplayReader, PredictionJournalSession,
    PredictionJournalWriter, PredictionRuntimeConfig,
};
use btc_signals::prediction_runtime::{PredictionCheckpointContext, PredictionPipeline};
use chrono::{DateTime, Duration as ChronoDuration, SecondsFormat, Utc};
use serde_json::{Value, json};
use signals_core::{
    MarketTransitionReason, PredictionMarketVenue, Quality, QualityState, SignalScope,
    SignalStreamMessage, SignalUpdate, SourceEvent,
};

const EPOCH: &str = "live-prediction-rotation";
const SECOND_NS: u64 = 1_000_000_000;
const MS: u64 = 1_000_000;
const WALL_START: &str = "2026-08-15T12:00:00Z";
const MAX_UPDATES: u64 = 100_000;
const MAX_OUTPUT_BYTES: u64 = 50_000_000;
const MAX_JOURNAL_BYTES: u64 = 50_000_000;

static COUNTER: AtomicU64 = AtomicU64::new(0);

// ---------------------------------------------------------------------------
// Scratch directory
// ---------------------------------------------------------------------------

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
            "btc-prediction-rotation-{tag}-{}-{nanos}-{unique}",
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

// ---------------------------------------------------------------------------
// Fixture events (mirroring the proven in-module builders, public API only)
// ---------------------------------------------------------------------------

fn timestamp(second: u64) -> String {
    DateTime::parse_from_rfc3339(WALL_START)
        .expect("origin parses")
        .with_timezone(&Utc)
        .checked_add_signed(ChronoDuration::seconds(i64::try_from(second).unwrap()))
        .expect("timestamp fits")
        .to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// A market/index source event carrying the process admission epoch — the epoch
/// the live feeds stamp and the pipeline validates against, unchanged across
/// rotation.
fn source_event(
    source_id: &str,
    sequence: u64,
    second: u64,
    mut values: BTreeMap<String, Value>,
) -> SourceEvent {
    let event_time = timestamp(second);
    values.insert("event_time".to_owned(), json!(event_time));
    SourceEvent {
        schema_version: 1,
        source_id: source_id.to_owned(),
        source_version: "1.0.0".to_owned(),
        publisher_epoch: EPOCH.to_owned(),
        sequence,
        event_time: event_time.clone(),
        received_at: event_time,
        values,
    }
}

fn kalshi_contract(sequence: u64, market: u64, open_second: u64) -> SourceEvent {
    source_event(
        "kalshi.btc.market-contracts",
        sequence,
        open_second,
        BTreeMap::from([
            (
                "market_ticker".to_owned(),
                json!(format!("KXBTC15M-{market}")),
            ),
            (
                "event_ticker".to_owned(),
                json!(format!("KXBTC15M-E{market}")),
            ),
            ("title".to_owned(), json!("BTC price in fifteen minutes")),
            ("status".to_owned(), json!("active")),
            ("open_time".to_owned(), json!(timestamp(open_second))),
            ("close_time".to_owned(), json!(timestamp(open_second + 900))),
            (
                "expected_expiration_time".to_owned(),
                json!(timestamp(open_second + 900)),
            ),
            (
                "legal_expiration_time".to_owned(),
                json!(timestamp(open_second + 1_800)),
            ),
            ("benchmark".to_owned(), json!("cf_benchmarks_brti")),
            ("averaging_window_seconds".to_owned(), json!(60)),
            ("comparator".to_owned(), json!("greater_than_or_equal")),
            ("tie_outcome".to_owned(), json!("yes")),
            ("reference_value".to_owned(), json!("60100")),
            ("rules_sha256".to_owned(), json!("b".repeat(64))),
        ]),
    )
}

fn kalshi_quote(sequence: u64, market: u64, second: u64) -> SourceEvent {
    source_event(
        "kalshi.btc.quotes",
        sequence,
        second,
        BTreeMap::from([
            (
                "market_ticker".to_owned(),
                json!(format!("KXBTC15M-{market}")),
            ),
            ("last_price".to_owned(), json!("0.41")),
            ("best_bid".to_owned(), json!("0.40")),
            ("best_ask".to_owned(), json!("0.42")),
            ("best_bid_size".to_owned(), json!("12")),
            ("best_ask_size".to_owned(), json!("6")),
            ("volume".to_owned(), json!("100")),
            ("open_interest".to_owned(), json!("50")),
        ]),
    )
}

fn cf_value(sequence: u64, second: u64, index_value: &str, avg: &str, window: u32) -> SourceEvent {
    source_event(
        "kalshi.cfbenchmarks.brti.value",
        sequence,
        second,
        BTreeMap::from([
            ("index_id".to_owned(), json!("BRTI")),
            ("index_value".to_owned(), json!(index_value)),
            ("kalshi_received_at".to_owned(), json!(timestamp(second))),
            ("upstream_sequence".to_owned(), json!(sequence)),
            ("avg_60s_value".to_owned(), json!(avg)),
            ("avg_60s_window_size".to_owned(), json!(window)),
            (
                "avg_60s_window_start".to_owned(),
                json!(timestamp(second - 60)),
            ),
            (
                "avg_60s_window_end_exclusive".to_owned(),
                json!(timestamp(second)),
            ),
        ]),
    )
}

fn cf_settlement(
    sequence: u64,
    second: u64,
    close_second: u64,
    as_of_second: u64,
    window: u32,
) -> SourceEvent {
    source_event(
        "kalshi.cfbenchmarks.brti.settlement-window",
        sequence,
        second,
        BTreeMap::from([
            ("index_id".to_owned(), json!("BRTI")),
            ("kalshi_received_at".to_owned(), json!(timestamp(second))),
            ("upstream_sequence".to_owned(), json!(sequence)),
            ("index_value".to_owned(), json!("60150.00000000")),
            ("window_average".to_owned(), json!("60150.00000000")),
            ("window_size".to_owned(), json!(window)),
            (
                "window_start_exclusive".to_owned(),
                json!(timestamp(close_second - 60)),
            ),
            (
                "window_end_inclusive".to_owned(),
                json!(timestamp(close_second)),
            ),
            ("window_as_of".to_owned(), json!(timestamp(as_of_second))),
            (
                "quarter_close_time".to_owned(),
                json!(timestamp(close_second)),
            ),
        ]),
    )
}

fn coinbase_ticker(sequence: u64, price: &str, second: u64) -> SourceEvent {
    let event_time = timestamp(second);
    SourceEvent {
        schema_version: 1,
        source_id: "coinbase.exchange.btc-usd.ticker".to_owned(),
        source_version: "1.0.0".to_owned(),
        publisher_epoch: "coinbase-feed".to_owned(),
        sequence,
        event_time: event_time.clone(),
        received_at: event_time.clone(),
        values: BTreeMap::from([
            ("instrument".to_owned(), json!("BTC-USD")),
            ("trade_id".to_owned(), json!(100 + sequence)),
            ("price".to_owned(), json!(price)),
            ("last_size".to_owned(), json!("0.01")),
            ("best_bid".to_owned(), json!(price)),
            ("best_bid_size".to_owned(), json!("1.25")),
            ("best_ask".to_owned(), json!(price)),
            ("best_ask_size".to_owned(), json!("2.5")),
            ("open_24h".to_owned(), json!("59800")),
            ("volume_24h".to_owned(), json!("1250.5")),
            ("low_24h".to_owned(), json!("59000")),
            ("high_24h".to_owned(), json!("61000")),
            ("volume_30d".to_owned(), json!("30000")),
            ("event_time".to_owned(), json!(event_time)),
            ("source_sequence".to_owned(), json!(900 + sequence)),
        ]),
    }
}

fn kraken_ticker(sequence: u64, price: &str, second: u64) -> SourceEvent {
    let event_time = timestamp(second);
    SourceEvent {
        schema_version: 1,
        source_id: "kraken.spot.btc-usd.ticker".to_owned(),
        source_version: "1.0.0".to_owned(),
        publisher_epoch: "kraken-feed".to_owned(),
        sequence,
        event_time: event_time.clone(),
        received_at: event_time.clone(),
        values: BTreeMap::from([
            ("instrument".to_owned(), json!("BTC/USD")),
            ("best_bid".to_owned(), json!(price)),
            ("best_bid_size".to_owned(), json!("0.02")),
            ("best_ask".to_owned(), json!(price)),
            ("best_ask_size".to_owned(), json!("0.17")),
            ("last_price".to_owned(), json!(price)),
            ("volume_24h".to_owned(), json!("973.7")),
            ("vwap_24h".to_owned(), json!("60010")),
            ("low_24h".to_owned(), json!("59000")),
            ("high_24h".to_owned(), json!("61000")),
            ("change_24h".to_owned(), json!("-72.9")),
            ("change_percent_24h".to_owned(), json!("-0.11")),
            ("trade_count_24h".to_owned(), json!(46574)),
            ("update_kind".to_owned(), json!("update")),
            ("event_time".to_owned(), json!(event_time)),
        ]),
    }
}

/// A Kalshi book-levels group (single affirmative axis). All rows share one
/// `update_group`/`update_count` and ride one `MarketEvents` batch, so their
/// receipt time and event time match — the grouping the ingest requires.
fn kalshi_book_group(
    seq_base: u64,
    market: u64,
    second: u64,
    kind: &str,
    rows: &[(&str, &str, &str)],
) -> Vec<SourceEvent> {
    let count = u32::try_from(rows.len()).expect("row count fits");
    let group = format!("kbk-{second}");
    rows.iter()
        .enumerate()
        .map(|(index, (side, price, size))| {
            let seq = seq_base + index as u64;
            source_event(
                "kalshi.btc.book-levels",
                seq,
                second,
                BTreeMap::from([
                    (
                        "market_ticker".to_owned(),
                        json!(format!("KXBTC15M-{market}")),
                    ),
                    ("upstream_sequence".to_owned(), json!(seq)),
                    ("update_group".to_owned(), json!(group)),
                    (
                        "update_index".to_owned(),
                        json!(u32::try_from(index).unwrap()),
                    ),
                    ("update_count".to_owned(), json!(count)),
                    ("update_kind".to_owned(), json!(kind)),
                    ("side".to_owned(), json!(side)),
                    ("price".to_owned(), json!(price)),
                    ("size".to_owned(), json!(size)),
                    ("event_time_basis".to_owned(), json!("provider")),
                ]),
            )
        })
        .collect()
}

/// A Polymarket book-levels group for one outcome token (a snapshot group must
/// carry exactly one outcome).
fn polymarket_book_group(
    seq_base: u64,
    market: u64,
    second: u64,
    outcome: &str,
    kind: &str,
    rows: &[(&str, &str, &str)],
) -> Vec<SourceEvent> {
    let count = u32::try_from(rows.len()).expect("row count fits");
    let asset = if outcome == "up" {
        format!("up-{market}")
    } else {
        format!("down-{market}")
    };
    let group = format!("pbk-{outcome}-{second}");
    rows.iter()
        .enumerate()
        .map(|(index, (side, price, size))| {
            let seq = seq_base + index as u64;
            source_event(
                "polymarket.btc.book-levels",
                seq,
                second,
                BTreeMap::from([
                    ("condition_id".to_owned(), json!(format!("cond-{market}"))),
                    ("asset_id".to_owned(), json!(asset)),
                    ("outcome".to_owned(), json!(outcome)),
                    ("update_group".to_owned(), json!(group)),
                    (
                        "update_index".to_owned(),
                        json!(u32::try_from(index).unwrap()),
                    ),
                    ("update_count".to_owned(), json!(count)),
                    ("update_kind".to_owned(), json!(kind)),
                    ("side".to_owned(), json!(side)),
                    ("price".to_owned(), json!(price)),
                    ("size".to_owned(), json!(size)),
                ]),
            )
        })
        .collect()
}

/// The book events (snapshot or delta) for one warm step, for the given venue.
fn book_events(
    venue: PredictionMarketVenue,
    market: u64,
    second: u64,
    kind: &str,
    bid_size: &str,
) -> Vec<SourceEvent> {
    let seq_base = second * 100;
    match venue {
        PredictionMarketVenue::Kalshi => kalshi_book_group(
            seq_base,
            market,
            second,
            kind,
            &[("bid", "0.40", bid_size), ("ask", "0.42", "100")],
        ),
        PredictionMarketVenue::Polymarket => {
            let mut events = polymarket_book_group(
                seq_base,
                market,
                second,
                "up",
                kind,
                &[("bid", "0.40", bid_size), ("ask", "0.42", "100")],
            );
            events.extend(polymarket_book_group(
                seq_base + 10,
                market,
                second,
                "down",
                kind,
                &[("bid", "0.58", "100"), ("ask", "0.60", "100")],
            ));
            events
        }
    }
}

// ---------------------------------------------------------------------------
// Decode helpers
// ---------------------------------------------------------------------------

fn decode(bytes: &[u8]) -> Vec<SignalStreamMessage> {
    bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).expect("line decodes"))
        .collect()
}

/// One decision's published updates, in wire order, tagged with the batch's
/// publisher epoch and sequence so the two-epoch split can be inspected.
struct Decision {
    updates: Vec<SignalUpdate>,
}

fn decisions(bytes: &[u8]) -> Vec<Decision> {
    decode(bytes)
        .into_iter()
        .filter_map(|message| match message {
            SignalStreamMessage::SignalBatch { updates, .. } => Some(Decision { updates }),
            _ => None,
        })
        .collect()
}

/// The output epoch a generation publishes under, read from its first frame (the
/// reset ensure_stream_started emits), so it is available even when the
/// generation emits no decisions.
fn generation_output_epoch(bytes: &[u8]) -> String {
    match decode(bytes).into_iter().next() {
        Some(SignalStreamMessage::Reset {
            publisher_epoch, ..
        }) => publisher_epoch,
        _ => String::new(),
    }
}

fn has_reset_at_zero(bytes: &[u8], epoch: &str) -> bool {
    decode(bytes).iter().any(|message| {
        matches!(
            message,
            SignalStreamMessage::Reset { publisher_epoch, sequence, .. }
                if publisher_epoch == epoch && *sequence == 0
        )
    })
}

/// The (signal_id, scope) → last ready value map a run reaches, with epoch and
/// sequence normalised out, so a rotated run and an unrotated run can be compared
/// on decision content alone.
fn final_ready_values(bytes: &[u8]) -> BTreeMap<(String, String), BTreeMap<String, Value>> {
    let mut latest = BTreeMap::new();
    for decision in decisions(bytes) {
        for update in decision.updates {
            if update.quality.state == QualityState::Ready {
                let key = (update.signal_id.clone(), scope_key(&update.scope));
                latest.insert(key, update.values.clone());
            }
        }
    }
    latest
}

fn scope_key(scope: &SignalScope) -> String {
    match scope {
        SignalScope::Global => "global".to_owned(),
        SignalScope::Market {
            market_id,
            market_generation,
        } => format!("market:{market_id}:{market_generation}"),
        other => format!("{other:?}"),
    }
}

fn ready_identities(decision: &Decision) -> Vec<(String, String)> {
    decision
        .updates
        .iter()
        .filter(|update| update.quality.state == QualityState::Ready)
        .map(|update| (update.signal_id.clone(), scope_key(&update.scope)))
        .collect()
}

/// The identities that re-warm on a rotated generation's cold replay: any
/// identity that, anywhere in `bytes`, publishes a non-ready readiness signal — a
/// `SignalBatch` update carrying quality reasons, or an `InstanceStatus` /
/// `Snapshot` status entry (how a warming or degraded identity announces itself).
/// On a rotated generation's cold replay every identity whose value depends on
/// rolling ring / order-book / oracle-reference state the sealed checkpoint does
/// not carry re-warms and shows up here (C5b's remit), so excluding them leaves
/// exactly the identities reproducible from the checkpoint + admitted inputs alone
/// (e.g. the static market contract). Deliberately reason-string-agnostic: an
/// identity is comparable only if it is Ready with NO reasons every time it
/// appears, which guarantees its value never depended on an unwarmed window.
fn rewarming_identities(bytes: &[u8]) -> BTreeSet<(String, String)> {
    let mut rewarming = BTreeSet::new();
    for message in decode(bytes) {
        match message {
            SignalStreamMessage::SignalBatch { updates, .. } => {
                for update in updates {
                    if !update.quality.reasons.is_empty()
                        || update.quality.state != QualityState::Ready
                    {
                        rewarming.insert((update.signal_id.clone(), scope_key(&update.scope)));
                    }
                }
            }
            SignalStreamMessage::Snapshot {
                updates, statuses, ..
            } => {
                for update in updates {
                    if !update.quality.reasons.is_empty()
                        || update.quality.state != QualityState::Ready
                    {
                        rewarming.insert((update.signal_id.clone(), scope_key(&update.scope)));
                    }
                }
                for status in statuses {
                    rewarming.insert((status.signal_id.clone(), scope_key(&status.scope)));
                }
            }
            SignalStreamMessage::InstanceStatus {
                signal_id, scope, ..
            } => {
                rewarming.insert((signal_id, scope_key(&scope)));
            }
            _ => {}
        }
    }
    rewarming
}

/// Per-identity `event_time` → (quality, values) trajectory of the READY
/// `SignalBatch` updates for identities NOT in `rewarming`. Keyed by the update's
/// own logical `event_time` (identical across a warm run and its cold replay, so
/// stable) rather than by decision index, because a cold replay publishes fewer
/// decisions than the warm run (warm identities emit on their interval even where
/// a cold one has nothing yet). A non-ring identity's every replay emission must
/// match the live emission at the same `event_time`.
/// Per `(signal_id, scope key)`: every emission keyed by `event_time`, with its
/// quality and value map.
type ReadyTrajectory =
    BTreeMap<(String, String), BTreeMap<String, (Quality, BTreeMap<String, Value>)>>;

fn nonring_ready_trajectory(
    bytes: &[u8],
    rewarming: &BTreeSet<(String, String)>,
) -> ReadyTrajectory {
    let mut trajectory: ReadyTrajectory = BTreeMap::new();
    for decision in decisions(bytes) {
        for update in decision.updates {
            let key = (update.signal_id.clone(), scope_key(&update.scope));
            if rewarming.contains(&key) {
                continue;
            }
            trajectory.entry(key).or_default().insert(
                update.event_time.clone(),
                (update.quality.clone(), update.values.clone()),
            );
        }
    }
    trajectory
}

/// One decision's per-identity published content — quality (state + reasons) and
/// values — with the envelope epoch/sequence normalised out, so a rotated run and
/// an unrotated run (or a replay and its live output) can be compared on decision
/// content alone.
fn decision_content(
    decision: &Decision,
) -> BTreeMap<(String, String), (Quality, BTreeMap<String, Value>)> {
    decision
        .updates
        .iter()
        .map(|update| {
            (
                (update.signal_id.clone(), scope_key(&update.scope)),
                (update.quality.clone(), update.values.clone()),
            )
        })
        .collect()
}

/// The last published content (quality + values) each identity reaches over a run,
/// whether or not it is Ready, so a resume can be compared against the live run for
/// every identity — not just the Ready ones.
fn final_content_all(
    bytes: &[u8],
) -> BTreeMap<(String, String), (Quality, BTreeMap<String, Value>)> {
    let mut latest = BTreeMap::new();
    for decision in decisions(bytes) {
        for (key, content) in decision_content(&decision) {
            latest.insert(key, content);
        }
    }
    latest
}

// ---------------------------------------------------------------------------
// Live rotating run harness
// ---------------------------------------------------------------------------

struct RotatingRun {
    layout: FileGenerationLayout,
    pipeline: Option<PredictionPipeline<BufWriter<File>>>,
    caps: FileGenerationCaps,
    lane: String,
    epoch_prefix: String,
    rotate_seconds: u64,
    current: Option<OpenFileGeneration>,
    gens: Vec<OpenFileGeneration>,
    next_unix: u64,
}

fn run_meta(
    lane: &str,
    epoch: &str,
    generation: &str,
    started_utc: &str,
    caps: FileGenerationCaps,
    resumed: bool,
) -> RunMeta {
    RunMeta {
        lane: lane.to_owned(),
        publisher_epoch: epoch.to_owned(),
        generation: generation.to_owned(),
        started_utc: started_utc.to_owned(),
        duration_seconds: 60,
        max_output_bytes: caps.max_output_bytes,
        max_journal_bytes: caps.max_journal_bytes,
        max_updates: caps.max_updates,
        headroom_factor: 2,
        resumed,
    }
}

impl RotatingRun {
    fn new(lane_root: &Path, venue: PredictionMarketVenue, armed: bool, lane: &str) -> Self {
        let caps = FileGenerationCaps {
            max_updates: MAX_UPDATES,
            max_output_bytes: MAX_OUTPUT_BYTES,
            max_journal_bytes: MAX_JOURNAL_BYTES,
        };
        let layout = FileGenerationLayout::new(lane_root, lane.to_owned()).expect("layout binds");
        let next_unix = 1_000;
        let generation = format!("gen-live-{lane}-{next_unix}");
        let meta = run_meta(lane, EPOCH, &generation, WALL_START, caps, false);
        let open = layout
            .open_next_with_rotation(next_unix, None, &meta, 60)
            .expect("open gen0");

        let config = PredictionRuntimeConfig::new(venue, 2_000).with_index_armed(armed);
        let session =
            PredictionJournalSession::new(WALL_START.to_owned(), EPOCH.to_owned(), config)
                .expect("session builds");
        let processor = PredictionSignalProcessor::new_indexed(
            venue,
            EPOCH,
            WALL_START.parse().expect("wall start parses"),
            armed,
        )
        .expect("processor builds");
        let checkpoint_path = lane_root.join("checkpoint.json");
        let context = PredictionCheckpointContext {
            path: checkpoint_path.clone(),
            maximum_bytes: DEFAULT_PREDICTION_CHECKPOINT_MAX_BYTES,
            session_id: session.session_id.clone(),
            wall_start: session.wall_start.clone(),
            contract_fingerprint: session.contract_fingerprint.clone(),
            runtime_config: session.runtime_config.clone(),
            publisher_epoch_lineage: vec![EPOCH.to_owned()],
        };
        let initial = PredictionCheckpointDocument::new(
            context.session_id.clone(),
            context.wall_start.clone(),
            0,
            0,
            context.contract_fingerprint.clone(),
            context.runtime_config.clone(),
            processor.checkpoint_state(),
            context.publisher_epoch_lineage.clone(),
        )
        .expect("initial checkpoint builds");
        write_prediction_checkpoint(
            &checkpoint_path,
            initial,
            DEFAULT_PREDICTION_CHECKPOINT_MAX_BYTES,
            false,
        )
        .expect("initial checkpoint persists");

        let output = File::create(&open.output_path).expect("output file");
        let writer = BufWriter::with_capacity(1024 * 1024, output);
        let journal =
            PredictionJournalWriter::create(&open.journal_path, session.clone(), MAX_JOURNAL_BYTES)
                .expect("journal writer");
        let pipeline = PredictionPipeline::live(
            writer,
            processor,
            &session,
            journal,
            context,
            caps.max_updates,
            caps.max_output_bytes,
        )
        .expect("live pipeline builds");

        Self {
            layout,
            pipeline: Some(pipeline),
            caps,
            lane: lane.to_owned(),
            epoch_prefix: EPOCH.to_owned(),
            rotate_seconds: 60,
            current: Some(open.clone()),
            gens: vec![open],
            next_unix,
        }
    }

    fn admit(&mut self, input: PredictionJournalInput) {
        self.pipeline
            .as_mut()
            .expect("pipeline live")
            .admit_live(input)
            .expect("admit_live");
    }

    fn rotate_at(&mut self, now_ns: u64) {
        self.next_unix += 100;
        let now_unix = self.next_unix;
        let generation = format!("gen-live-{}-{now_unix}", self.lane);
        let output_epoch = format!("{}-{now_unix}", self.epoch_prefix);
        let stamp = timestamp(now_ns / SECOND_NS);
        let mut departed = self.current.take().expect("current generation");
        let departed_name = departed
            .dir
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_owned)
            .expect("departed name");
        let meta = run_meta(
            &self.lane,
            &self.epoch_prefix,
            &generation,
            &stamp,
            self.caps,
            true,
        );
        let open = self
            .layout
            .open_next_with_rotation(now_unix, Some(&departed_name), &meta, self.rotate_seconds)
            .expect("open successor generation");
        self.pipeline
            .as_mut()
            .expect("pipeline live")
            .rotate(now_ns, &open, output_epoch, self.caps)
            .expect("rotate");
        departed.seal(&stamp).expect("seal departed RUN-META");
        self.current = Some(open.clone());
        self.gens.push(open);
    }

    /// Finish the run and return the per-generation (output bytes, journal path)
    /// pairs in order.
    fn finish(mut self, now_ns: u64) -> Vec<GenerationArtifacts> {
        let mut pipeline = self.pipeline.take().expect("pipeline live");
        pipeline.sync_output().expect("sync output");
        pipeline.finish_live(now_ns).expect("finish live");
        if let Some(mut current) = self.current.take() {
            let _ = current.seal(timestamp(now_ns / SECOND_NS));
        }
        self.gens
            .iter()
            .map(|generation| GenerationArtifacts {
                output: std::fs::read(&generation.output_path).expect("read output"),
                journal_path: generation.journal_path.clone(),
            })
            .collect()
    }
}

struct GenerationArtifacts {
    output: Vec<u8>,
    journal_path: PathBuf,
}

/// Replay one generation's journal into an in-memory sink, returning its bytes —
/// the same path `prediction-signals replay` takes.
fn replay_journal(journal_path: &Path) -> Vec<u8> {
    let mut reader =
        PredictionJournalReplayReader::open(journal_path, MAX_JOURNAL_BYTES).expect("open journal");
    let session = reader.session().clone();
    let wall_start = DateTime::parse_from_rfc3339(&session.wall_start)
        .expect("wall_start parses")
        .with_timezone(&Utc);
    let index_armed = session.runtime_config.index_armed;
    let processor = if let Some(checkpoint) = &session.initial_checkpoint {
        if session.admission_epoch.is_some() {
            PredictionSignalProcessor::restore_rotated_indexed(
                session.publisher_epoch.clone(),
                wall_start,
                checkpoint.processor_state.clone(),
                index_armed,
            )
            .expect("restore rotated replay processor")
        } else {
            PredictionSignalProcessor::restore_indexed(
                session.publisher_epoch.clone(),
                wall_start,
                checkpoint.processor_state.clone(),
                index_armed,
            )
            .expect("restore replay processor")
        }
    } else {
        PredictionSignalProcessor::new_indexed(
            session.venue,
            session.publisher_epoch.clone(),
            wall_start,
            index_armed,
        )
        .expect("build replay processor")
    };
    let sink = SharedSink(Arc::new(std::sync::Mutex::new(Vec::new())));
    let mut pipeline = PredictionPipeline::replay(
        sink.clone(),
        processor,
        &session,
        MAX_UPDATES,
        MAX_OUTPUT_BYTES,
    )
    .expect("replay pipeline builds");
    while let Some(input) = reader.next_input().expect("read journal") {
        pipeline.admit_replay(input).expect("apply replay input");
    }
    let end = reader.end().expect("journal end").clone();
    pipeline.finish_replay(&end).expect("finish replay");
    sink.0.lock().expect("sink lock").clone()
}

/// An in-memory `Write` sink whose bytes can be recovered after the pipeline that
/// owned it is dropped.
#[derive(Clone)]
struct SharedSink(Arc<std::sync::Mutex<Vec<u8>>>);

impl Write for SharedSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("sink lock").extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Fixtures: a warm-up + roll timeline driven through the pipeline
// ---------------------------------------------------------------------------

/// Admit a warm-up step at `second`: spot ticks (both cores), a Kalshi market
/// quote + CF value (or a Polymarket quote), and a health tick, producing one
/// decision. `seq` is the running admission-sequence cursor, advanced and
/// returned.
fn warm_step(
    run: &mut RotatingRun,
    venue: PredictionMarketVenue,
    armed: bool,
    market: u64,
    second: u64,
    seq: &mut u64,
) {
    let now = second * SECOND_NS;
    run.admit(PredictionJournalInput::SpotEvents {
        admission_seq: *seq,
        events: Arc::new(vec![
            coinbase_ticker(second, "60000", second),
            kraken_ticker(second, "60000", second),
        ]),
        logical_receipt_ns: now,
        now_ns: now,
    });
    *seq += 1;

    let market_events = match venue {
        PredictionMarketVenue::Kalshi if armed => vec![
            kalshi_quote(second, market, second),
            cf_value(second, second, "60200.5", "60150.25000000", 60),
        ],
        PredictionMarketVenue::Kalshi => vec![kalshi_quote(second, market, second)],
        PredictionMarketVenue::Polymarket => vec![
            polymarket_quote_leg(second * 2, market, second, "up", "0.40", "0.42"),
            polymarket_quote_leg(second * 2 + 1, market, second, "down", "0.58", "0.60"),
        ],
    };
    run.admit(PredictionJournalInput::MarketEvents {
        admission_seq: *seq,
        generation: market,
        events: Arc::new(market_events),
        logical_receipt_ns: now,
        now_ns: now,
    });
    *seq += 1;

    // A book-levels delta each step. On a rotated generation's cold replay these
    // arrive before any snapshot; the book re-warms (not-ready, `snapshot_missing`)
    // rather than failing — the honest re-warm the resync would otherwise avoid,
    // which the prediction lane does not need because a delta-before-snapshot is
    // non-fatal here.
    run.admit(PredictionJournalInput::MarketEvents {
        admission_seq: *seq,
        generation: market,
        events: Arc::new(book_events(venue, market, second, "delta", "110")),
        logical_receipt_ns: now,
        now_ns: now,
    });
    *seq += 1;

    run.admit(PredictionJournalInput::HealthTick {
        admission_seq: *seq,
        now_ns: now + MS,
    });
    *seq += 1;
}

// ---------------------------------------------------------------------------
// Test 1: rotation is invisible to the frames
// ---------------------------------------------------------------------------

#[test]
fn rotation_is_invisible_to_the_frames_prediction() {
    for (venue, armed, lane) in [
        (PredictionMarketVenue::Kalshi, true, "kalshi"),
        (PredictionMarketVenue::Polymarket, false, "polymarket"),
    ] {
        // Unrotated reference.
        let unrotated_dir = TempDir::new("t1-unrot");
        let mut unrotated = RotatingRun::new(unrotated_dir.path(), venue, armed, lane);
        let mut seq = 0_u64;
        unrotated.admit(PredictionJournalInput::Bind {
            admission_seq: seq,
            generation: 1,
            previous_market_id: None,
            reason: MarketTransitionReason::InitialBind,
            event: kalshi_or_polymarket_contract(venue, 100, 1, 0),
            logical_receipt_ns: 0,
            now_ns: 0,
        });
        seq += 1;
        for second in 1..=8 {
            warm_step(&mut unrotated, venue, armed, 1, 100 + second * 10, &mut seq);
        }
        let unrotated_arts = unrotated.finish(200 * SECOND_NS);
        assert_eq!(
            unrotated_arts.len(),
            1,
            "{lane}: unrotated is one generation"
        );
        let unrotated_final = final_ready_values(&unrotated_arts[0].output);

        // Rotated: identical inputs, rotate twice mid-warm-up.
        let rotated_dir = TempDir::new("t1-rot");
        let mut rotated = RotatingRun::new(rotated_dir.path(), venue, armed, lane);
        let mut seq = 0_u64;
        rotated.admit(PredictionJournalInput::Bind {
            admission_seq: seq,
            generation: 1,
            previous_market_id: None,
            reason: MarketTransitionReason::InitialBind,
            event: kalshi_or_polymarket_contract(venue, 100, 1, 0),
            logical_receipt_ns: 0,
            now_ns: 0,
        });
        seq += 1;
        for second in 1..=3 {
            warm_step(&mut rotated, venue, armed, 1, 100 + second * 10, &mut seq);
        }
        // Ready identities in the last decision before boundary 1.
        let pre_boundary =
            decisions(&std::fs::read(&rotated.gens[0].output_path).unwrap_or_default());
        let ready_before: Vec<(String, String)> = pre_boundary
            .last()
            .map(ready_identities)
            .unwrap_or_default();

        rotated.rotate_at(135 * SECOND_NS);
        for second in 4..=6 {
            warm_step(&mut rotated, venue, armed, 1, 100 + second * 10, &mut seq);
        }
        rotated.rotate_at(165 * SECOND_NS);
        for second in 7..=8 {
            warm_step(&mut rotated, venue, armed, 1, 100 + second * 10, &mut seq);
        }
        let rotated_arts = rotated.finish(200 * SECOND_NS);
        assert_eq!(
            rotated_arts.len(),
            3,
            "{lane}: rotated is three generations"
        );

        // (a) Rotation is invisible to the decision content: for EVERY decision
        // (including every post-boundary one) the rotated run's per-identity values
        // AND quality states equal the unrotated run's, for EVERY identity — not
        // merely the terminal Ready set. The warm processor, rings and E37c pending
        // slot survive the in-place swap, so only the envelope epoch/sequence (which
        // `decision_content` normalises out) may differ.
        let mut rotated_concat = Vec::new();
        for art in &rotated_arts {
            rotated_concat.extend_from_slice(&art.output);
        }
        let unrotated_decisions = decisions(&unrotated_arts[0].output);
        let rotated_decisions = decisions(&rotated_concat);
        assert_eq!(
            rotated_decisions.len(),
            unrotated_decisions.len(),
            "{lane}: rotation preserves the number of decisions"
        );
        // The armed Kalshi fixture publishes decisions; the Polymarket lane needs
        // its (undriven) Chainlink anchor to publish, so its decision stream may be
        // empty here — the per-decision comparison below then holds vacuously while
        // the readiness-continuity and epoch assertions still cover it.
        if venue == PredictionMarketVenue::Kalshi {
            assert!(
                !rotated_decisions.is_empty(),
                "{lane}: the armed fixture produced decisions to compare"
            );
        }
        for (index, (rotated_decision, unrotated_decision)) in rotated_decisions
            .iter()
            .zip(&unrotated_decisions)
            .enumerate()
        {
            assert_eq!(
                decision_content(rotated_decision),
                decision_content(unrotated_decision),
                "{lane}: decision {index} values and states are identical across rotation"
            );
        }
        // The values the run converges to are therefore identical too.
        let rotated_final = final_ready_values(&rotated_concat);
        assert_eq!(
            rotated_final, unrotated_final,
            "{lane}: rotation does not change the decision values the run converges to"
        );
        // The armed Kalshi fixture warms identities to Ready without any external
        // oracle feed; the Polymarket lane needs its Chainlink TWAP anchor to
        // ready its identities, which this fixture does not drive, so the ready
        // set is asserted non-empty only for Kalshi.
        let require_ready = venue == PredictionMarketVenue::Kalshi;
        if require_ready {
            assert!(
                !rotated_final.is_empty(),
                "{lane}: the fixture produced ready decisions"
            );
        }

        // (b) Two-epoch split: each generation publishes under its own output
        // epoch with a reset at sequence 0; the first is the prefix verbatim. The
        // epoch is read from the reset frame so it holds even for a lane that
        // emits no decisions in this fixture.
        let gen0_epoch = generation_output_epoch(&rotated_arts[0].output);
        let gen1_epoch = generation_output_epoch(&rotated_arts[1].output);
        assert_eq!(gen0_epoch, EPOCH, "{lane}: gen0 output epoch is the prefix");
        assert!(
            has_reset_at_zero(&rotated_arts[0].output, EPOCH),
            "{lane}: gen0 reset"
        );
        assert!(
            gen1_epoch.starts_with(&format!("{EPOCH}-")) && gen1_epoch != EPOCH,
            "{lane}: gen1 output epoch is per-generation ({gen1_epoch})"
        );
        assert!(
            has_reset_at_zero(&rotated_arts[1].output, &gen1_epoch),
            "{lane}: gen1 reset"
        );
        // The stream sequence restarts: gen1's very first frame is the reset at 0.
        let gen1_frames = decode(&rotated_arts[1].output);
        assert!(
            matches!(
                gen1_frames.first(),
                Some(SignalStreamMessage::Reset { sequence: 0, .. })
            ),
            "{lane}: gen1's first frame is the reset at sequence 0"
        );

        // (c) Readiness continuity: every identity Ready before a boundary is
        // Ready in the first decision after it.
        let gen1_decisions = decisions(&rotated_arts[1].output);
        let ready_after = gen1_decisions
            .first()
            .map(ready_identities)
            .unwrap_or_default();
        for identity in &ready_before {
            assert!(
                ready_after.contains(identity),
                "{lane}: {identity:?} was Ready before the boundary and must be Ready in the first decision after it"
            );
        }
        if require_ready {
            assert!(
                !ready_before.is_empty(),
                "{lane}: at least one identity was Ready before the boundary"
            );
        }
    }
}

fn kalshi_or_polymarket_contract(
    venue: PredictionMarketVenue,
    sequence: u64,
    market: u64,
    open_second: u64,
) -> SourceEvent {
    match venue {
        PredictionMarketVenue::Kalshi => kalshi_contract(sequence, market, open_second),
        PredictionMarketVenue::Polymarket => polymarket_contract(sequence, market, open_second),
    }
}

fn polymarket_contract(sequence: u64, market: u64, open_second: u64) -> SourceEvent {
    source_event(
        "polymarket.btc.market-contracts",
        sequence,
        open_second,
        BTreeMap::from([
            ("market_id".to_owned(), json!(format!("pm-{market}"))),
            ("condition_id".to_owned(), json!(format!("cond-{market}"))),
            (
                "question_id".to_owned(),
                json!(format!("question-{market}")),
            ),
            ("slug".to_owned(), json!(format!("btc-updown-{market}"))),
            (
                "question".to_owned(),
                json!("Will BTC be at or above the opening value?"),
            ),
            ("up_token_id".to_owned(), json!(format!("up-{market}"))),
            ("down_token_id".to_owned(), json!(format!("down-{market}"))),
            ("window_open".to_owned(), json!(timestamp(open_second))),
            (
                "window_close".to_owned(),
                json!(timestamp(open_second + 300)),
            ),
            (
                "resolution_source".to_owned(),
                json!("https://data.chain.link/streams/btc-usd-twap-60s-streams"),
            ),
            ("comparator".to_owned(), json!("greater_than_or_equal")),
            ("tie_outcome".to_owned(), json!("up")),
            ("minimum_tick_size".to_owned(), json!("0.01")),
            ("minimum_order_size".to_owned(), json!("5")),
            ("order_book_enabled".to_owned(), json!(true)),
            ("active".to_owned(), json!(true)),
            ("closed".to_owned(), json!(false)),
            ("rules_sha256".to_owned(), json!("a".repeat(64))),
        ]),
    )
}

/// One Polymarket outcome's best quote (the source is per-outcome: condition_id
/// + asset_id + outcome). A complete book needs the up and down legs.
fn polymarket_quote_leg(
    sequence: u64,
    market: u64,
    second: u64,
    outcome: &str,
    best_bid: &str,
    best_ask: &str,
) -> SourceEvent {
    let asset = if outcome == "up" {
        format!("up-{market}")
    } else {
        format!("down-{market}")
    };
    source_event(
        "polymarket.btc.quotes",
        sequence,
        second,
        BTreeMap::from([
            ("condition_id".to_owned(), json!(format!("cond-{market}"))),
            ("asset_id".to_owned(), json!(asset)),
            ("outcome".to_owned(), json!(outcome)),
            ("best_bid".to_owned(), json!(best_bid)),
            ("best_ask".to_owned(), json!(best_ask)),
            ("spread".to_owned(), json!("0.02")),
        ]),
    )
}

// ---------------------------------------------------------------------------
// Test 1 addendum: E37c pending slot survives a rotation boundary (Kalshi)
// ---------------------------------------------------------------------------

#[test]
fn rotation_boundary_inside_the_e37c_pending_slot_still_releases_the_departed_final_decision_prediction()
 {
    use btc_signals::{KALSHI_ORACLE_DIVERGENCE_ID, KALSHI_SETTLEMENT_ANCHOR_STATE_ID};
    let dir = TempDir::new("t1-e37c");
    let mut run = RotatingRun::new(dir.path(), PredictionMarketVenue::Kalshi, true, "kalshi");
    let mut seq = 0_u64;
    run.admit(PredictionJournalInput::Bind {
        admission_seq: seq,
        generation: 1,
        previous_market_id: None,
        reason: MarketTransitionReason::InitialBind,
        event: kalshi_contract(0, 1, 0),
        logical_receipt_ns: 0,
        now_ns: 0,
    });
    seq += 1;
    // Warm the composite spot reference so the divergence twin has a mid to freeze.
    for second in [897_u64, 898] {
        run.admit(PredictionJournalInput::SpotEvents {
            admission_seq: seq,
            events: Arc::new(vec![
                coinbase_ticker(second, "60000", second),
                kraken_ticker(second, "60000", second),
            ]),
            logical_receipt_ns: second * SECOND_NS,
            now_ns: second * SECOND_NS,
        });
        seq += 1;
    }
    // Ready settlement anchor one second before the close, window still incomplete.
    run.admit(PredictionJournalInput::MarketEvents {
        admission_seq: seq,
        generation: 1,
        events: Arc::new(vec![
            cf_value(1, 899, "60200.5", "60150.25000000", 60),
            cf_settlement(2, 899, 900, 899, 59),
        ]),
        logical_receipt_ns: 899 * SECOND_NS,
        now_ns: 899 * SECOND_NS,
    });
    seq += 1;
    // Scheduled rollover AT the close freezes the pending E37c final decision.
    run.admit(PredictionJournalInput::Unavailable {
        admission_seq: seq,
        generation: 2,
        previous_market_id: Some("KXBTC15M-1".to_owned()),
        reason: MarketTransitionReason::ScheduledRollover,
        now_ns: 900 * SECOND_NS,
    });
    seq += 1;

    // *** The rotation boundary lands INSIDE the E37c pending slot: after the
    // roll froze the departed final decision, before the completing frame. ***
    run.rotate_at(900 * SECOND_NS + 10 * MS);

    // The successor market binds in the new generation.
    run.admit(PredictionJournalInput::Bind {
        admission_seq: seq,
        generation: 2,
        previous_market_id: Some("KXBTC15M-1".to_owned()),
        reason: MarketTransitionReason::ScheduledRollover,
        event: kalshi_contract(10, 2, 900),
        logical_receipt_ns: 900 * SECOND_NS + 100 * MS,
        now_ns: 900 * SECOND_NS + 100 * MS,
    });
    seq += 1;
    // The completing frame for the DEPARTED quarter arrives after the boundary.
    run.admit(PredictionJournalInput::MarketEvents {
        admission_seq: seq,
        generation: 2,
        events: Arc::new(vec![cf_settlement(3, 900, 900, 900, 60)]),
        logical_receipt_ns: 900 * SECOND_NS + 200 * MS,
        now_ns: 900 * SECOND_NS + 200 * MS,
    });

    let arts = run.finish(901 * SECOND_NS);
    assert_eq!(arts.len(), 2, "one rotation → two generations");
    // The departed final decision must be released into the NEW generation's
    // output, proving the pending slot survived the in-place swap.
    let successor = decisions(&arts[1].output);
    for id in [
        KALSHI_SETTLEMENT_ANCHOR_STATE_ID,
        KALSHI_ORACLE_DIVERGENCE_ID,
    ] {
        let released = successor
            .iter()
            .flat_map(|d| d.updates.iter())
            .any(|update| {
                update.signal_id == id
                    && matches!(
                        &update.scope,
                        SignalScope::Market { market_id, market_generation }
                            if market_id == "KXBTC15M-1" && *market_generation == 1
                    )
                    && update.quality.state == QualityState::Ready
                    && update.values.get("settlement_window_complete") == Some(&json!(true))
            });
        assert!(
            released,
            "{id}: the departed final decision is released after a rotation boundary inside the pending slot"
        );
    }
}

// ---------------------------------------------------------------------------
// Test 2 & 3: per-generation journal replay
// ---------------------------------------------------------------------------

fn build_rotated_three(
    dir: &Path,
    venue: PredictionMarketVenue,
    armed: bool,
    lane: &str,
) -> Vec<GenerationArtifacts> {
    let mut run = RotatingRun::new(dir, venue, armed, lane);
    let mut seq = 0_u64;
    run.admit(PredictionJournalInput::Bind {
        admission_seq: seq,
        generation: 1,
        previous_market_id: None,
        reason: MarketTransitionReason::InitialBind,
        event: kalshi_or_polymarket_contract(venue, 100, 1, 0),
        logical_receipt_ns: 0,
        now_ns: 0,
    });
    seq += 1;
    // Seed a full book snapshot in generation 0 so the book warms live; the
    // per-step deltas that follow (including those in later generations) ride on
    // it. A rotated generation's cold replay receives only deltas and re-warms.
    run.admit(PredictionJournalInput::MarketEvents {
        admission_seq: seq,
        generation: 1,
        events: Arc::new(book_events(venue, 1, 0, "snapshot", "100")),
        logical_receipt_ns: 0,
        now_ns: 0,
    });
    seq += 1;
    for second in 1..=3 {
        warm_step(&mut run, venue, armed, 1, 100 + second * 10, &mut seq);
    }
    run.rotate_at(135 * SECOND_NS);
    for second in 4..=6 {
        warm_step(&mut run, venue, armed, 1, 100 + second * 10, &mut seq);
    }
    run.rotate_at(165 * SECOND_NS);
    for second in 7..=8 {
        warm_step(&mut run, venue, armed, 1, 100 + second * 10, &mut seq);
    }
    run.finish(200 * SECOND_NS)
}

/// The self-sufficient first generation (it carries its own initial Bind) replays
/// byte-identically to its output — proving the two-epoch replay path and that
/// rotation leaves generation 0 exactly as an unrotated run would leave it.
#[test]
fn generation_zero_journal_replays_byte_identically_prediction() {
    for (venue, armed, lane) in [
        (PredictionMarketVenue::Kalshi, true, "kalshi"),
        (PredictionMarketVenue::Polymarket, false, "polymarket"),
    ] {
        let dir = TempDir::new("t2-gen0");
        let arts = build_rotated_three(dir.path(), venue, armed, lane);
        let gen0_replay = replay_journal(&arts[0].journal_path);
        assert_eq!(
            gen0_replay, arts[0].output,
            "{lane}: generation 0 journal replays byte-identically to its output"
        );
    }
}

#[test]
fn each_file_generation_journal_replays_byte_identically_prediction() {
    for (venue, armed, lane) in [
        (PredictionMarketVenue::Kalshi, true, "kalshi"),
        (PredictionMarketVenue::Polymarket, false, "polymarket"),
    ] {
        let dir = TempDir::new("t2");
        let arts = build_rotated_three(dir.path(), venue, armed, lane);
        assert_eq!(arts.len(), 3, "{lane}: three generations");

        // Generation 0 has no prior warmth to lose, so its journal replays
        // byte-identically to its output.
        let gen0_replay = replay_journal(&arts[0].journal_path);
        assert_eq!(
            gen0_replay, arts[0].output,
            "{lane}: generation 0 journal replays byte-identically"
        );

        // Rotated generations: replay is standalone from the journal alone — the
        // active binding is rebuilt from the sealed checkpoint (no fresh Bind) and
        // the book re-warms from the post-boundary deltas, so `replay_journal`
        // succeeds (its `finish_replay` tolerance passes; a failure would panic
        // here). The checkpoint carries no ring/book/reference state, so those ring
        // identities honestly re-warm (full byte-identity for them is C5b) and are
        // excluded here via `ring_identities`. EVERY OTHER (non-ring) identity must
        // replay byte-for-byte — identical quality state AND values — at every
        // decision, not merely a non-empty stream with the right epoch/reset.
        for (index, art) in arts.iter().enumerate().skip(1) {
            let replay = replay_journal(&art.journal_path);
            assert!(
                !replay.is_empty(),
                "{lane}: generation {index} replays to a non-empty stream"
            );
            let live_epoch = generation_output_epoch(&art.output);
            let replay_epoch = generation_output_epoch(&replay);
            assert_eq!(
                live_epoch, replay_epoch,
                "{lane}: generation {index} replays under the same output epoch"
            );
            assert!(
                has_reset_at_zero(&replay, &replay_epoch),
                "{lane}: generation {index} replay re-emits the reset at sequence 0"
            );

            // Identities that re-warm on the cold replay are C5b's remit and
            // excluded; every other identity is reproducible from the checkpoint +
            // admitted inputs and must replay byte-for-byte — identical quality
            // state AND values — at every decision it publishes (matched by logical
            // `event_time`, because the cold replay publishes fewer decisions than
            // the warm run).
            let rewarming = rewarming_identities(&replay);
            let live_trajectory = nonring_ready_trajectory(&art.output, &rewarming);
            let replay_trajectory = nonring_ready_trajectory(&replay, &rewarming);
            let mut compared_nonring = 0_usize;
            for (key, replay_points) in &replay_trajectory {
                let live_points = live_trajectory.get(key);
                for (event_time, replay_value) in replay_points {
                    assert_eq!(
                        live_points.and_then(|points| points.get(event_time)),
                        Some(replay_value),
                        "{lane}: generation {index} non-ring {key:?} at {event_time} must replay byte-for-byte"
                    );
                    compared_nonring += 1;
                }
            }

            // The armed Kalshi lane readies the static market contract from the
            // sealed binding alone; the Polymarket lane needs its (undriven)
            // Chainlink anchor to ready anything, so its non-ring set is empty in
            // this fixture and only the epoch/reset framing above covers it. For
            // Kalshi the contract must be republished as a Ready non-ring identity —
            // rebuilt purely from the checkpoint binding — proving the comparison is
            // non-vacuous.
            if venue == PredictionMarketVenue::Kalshi {
                let contract_id = format!("btc.{lane}.market_contract");
                assert!(
                    replay_trajectory
                        .keys()
                        .any(|(signal_id, _)| signal_id == &contract_id),
                    "{lane}: generation {index} replay republishes the market contract from the sealed binding"
                );
                assert!(
                    !rewarming
                        .iter()
                        .any(|(signal_id, _)| signal_id == &contract_id),
                    "{lane}: the market contract is non-ring and must be compared, not tolerated"
                );
                assert!(
                    compared_nonring > 0,
                    "{lane}: generation {index} compared at least one non-ring identity"
                );
            }
        }
    }
}

#[test]
fn resume_after_seal_replays_the_next_generation_prediction() {
    for (venue, armed, lane) in [
        (PredictionMarketVenue::Kalshi, true, "kalshi"),
        (PredictionMarketVenue::Polymarket, false, "polymarket"),
    ] {
        let dir = TempDir::new("t3");
        let arts = build_rotated_three(dir.path(), venue, armed, lane);
        assert_eq!(arts.len(), 3, "{lane}: three generations");
        // Replaying generation N+1's journal (which embeds the checkpoint sealed
        // at N as its initial_checkpoint) resumes it standalone. The resume is
        // driven BY the sealed checkpoint: N+1's journal has no Bind, so every
        // market-relative identity is rebuilt from the checkpoint's `active_binding`
        // (proven load-bearing by the market_contract identity below). This asserts
        // the resume reproduces the live run for EVERY non-ring identity — not one
        // identity behind an `if let Some`. Ring identities honestly re-warm (their
        // rolling windows are not checkpointed, C5b), so they are excluded via
        // `ring_identities`; every other identity's terminal state AND values must
        // match the warm live run.
        let contract_id = format!("btc.{lane}.market_contract");
        for (index, art) in arts.iter().enumerate().skip(1) {
            let replay = replay_journal(&art.journal_path);
            assert!(
                !replay.is_empty(),
                "{lane}: generation {index} resumes to a non-empty stream from the sealed checkpoint"
            );
            let live_final = final_content_all(&art.output);
            let replay_final = final_content_all(&replay);
            let rewarming = rewarming_identities(&replay);
            let keys: BTreeSet<(String, String)> = live_final
                .keys()
                .chain(replay_final.keys())
                .cloned()
                .collect();
            let mut compared_nonring = 0_usize;
            for key in keys {
                if rewarming.contains(&key) {
                    continue;
                }
                assert_eq!(
                    replay_final.get(&key),
                    live_final.get(&key),
                    "{lane}: generation {index} resumes non-ring {key:?} to the live terminal state and values"
                );
                compared_nonring += 1;
            }
            // The armed Kalshi lane readies the static market contract from the
            // sealed binding alone (proving the resume is checkpoint-driven); the
            // Polymarket lane readies nothing without its undriven Chainlink anchor,
            // so only the non-empty resume is asserted there.
            if venue == PredictionMarketVenue::Kalshi {
                assert!(
                    replay_final
                        .keys()
                        .any(|(signal_id, _)| signal_id == &contract_id),
                    "{lane}: generation {index} rebuilds the market contract from the sealed checkpoint's binding"
                );
                assert!(
                    !rewarming
                        .iter()
                        .any(|(signal_id, _)| signal_id == &contract_id),
                    "{lane}: the market contract is non-ring and is compared, not tolerated"
                );
                assert!(
                    compared_nonring > 0,
                    "{lane}: generation {index} compared at least one non-ring identity on resume"
                );
            }
        }
    }
}

/// Each in-process rotation is a fresh publisher (output) epoch, so the sealed
/// checkpoint's `publisher_epoch_lineage` records every generation in order,
/// exactly as `bin::run` records a restart's process epoch. After two rotations
/// the lineage carries three epochs: the gen-0 prefix verbatim, then one
/// `<prefix>-<unix>` per rotation, in open order.
#[test]
fn two_rotations_extend_the_publisher_epoch_lineage_prediction() {
    for (venue, armed, lane) in [
        (PredictionMarketVenue::Kalshi, true, "kalshi"),
        (PredictionMarketVenue::Polymarket, false, "polymarket"),
    ] {
        let dir = TempDir::new("t-lineage");
        let arts = build_rotated_three(dir.path(), venue, armed, lane);
        assert_eq!(arts.len(), 3, "{lane}: three generations (two rotations)");

        let document = load_prediction_checkpoint(
            &dir.path().join("checkpoint.json"),
            DEFAULT_PREDICTION_CHECKPOINT_MAX_BYTES,
        )
        .expect("sealed checkpoint loads");
        let lineage = &document.publisher_epoch_lineage;
        assert_eq!(
            lineage.len(),
            3,
            "{lane}: two rotations extend the lineage to three epochs (found {lineage:?})"
        );
        assert_eq!(
            lineage[0], EPOCH,
            "{lane}: the first lineage entry is the gen-0 prefix verbatim"
        );
        for (index, epoch) in lineage.iter().enumerate().skip(1) {
            assert!(
                epoch.starts_with(&format!("{EPOCH}-")) && epoch != EPOCH,
                "{lane}: rotation {index} recorded a per-generation output epoch ({epoch})"
            );
        }
        // Recorded in open order: each rotated epoch's unix suffix strictly grows.
        assert!(
            lineage[1] < lineage[2],
            "{lane}: rotated epochs are recorded in open order ({lineage:?})"
        );
    }
}

/// The live `--resume-from-checkpoint` (crash-recovery) path ignores the
/// checkpoint's `active_binding` and re-discovers, while the gap-free rotation
/// replay path rebuilds it. Proven at the processor boundary the two paths diverge
/// on (`restore_indexed` vs `restore_rotated_indexed`), driven by the same sealed
/// checkpoint that carries a live binding.
#[test]
fn live_crash_resume_ignores_active_binding_and_rediscovers_prediction() {
    for (venue, armed, lane) in [
        (PredictionMarketVenue::Kalshi, true, "kalshi"),
        (PredictionMarketVenue::Polymarket, false, "polymarket"),
    ] {
        let dir = TempDir::new("t-crash");
        let arts = build_rotated_three(dir.path(), venue, armed, lane);
        assert_eq!(arts.len(), 3, "{lane}: three generations");

        // Generation 1's journal embeds the checkpoint sealed at generation 0; a
        // market was bound before the boundary, so that checkpoint carries the
        // active binding.
        let reader = PredictionJournalReplayReader::open(&arts[1].journal_path, MAX_JOURNAL_BYTES)
            .expect("open generation 1 journal");
        let session = reader.session().clone();
        let checkpoint = session
            .initial_checkpoint
            .clone()
            .expect("a rotated generation carries the sealed checkpoint");
        assert!(
            checkpoint.processor_state.active_binding.is_some(),
            "{lane}: the sealed checkpoint carries the active binding"
        );
        let wall_start = DateTime::parse_from_rfc3339(&session.wall_start)
            .expect("wall_start parses")
            .with_timezone(&Utc);

        // Gap-free rotation replay: the binding is rebuilt.
        let rotated = PredictionSignalProcessor::restore_rotated_indexed(
            session.publisher_epoch.clone(),
            wall_start,
            checkpoint.processor_state.clone(),
            armed,
        )
        .expect("restore rotated processor");
        assert!(
            rotated.active_market_id().is_some(),
            "{lane}: the gap-free rotation replay rebuilds the active binding"
        );

        // Live crash resume: the binding is ignored and the lane re-discovers.
        let crash = PredictionSignalProcessor::restore_indexed(
            session.publisher_epoch.clone(),
            wall_start,
            checkpoint.processor_state.clone(),
            armed,
        )
        .expect("restore crash-resume processor");
        assert!(
            crash.active_market_id().is_none(),
            "{lane}: live --resume-from-checkpoint ignores active_binding and re-discovers"
        );
    }
}

// ---------------------------------------------------------------------------
// Test 4: rotation-mode CLI validation
// ---------------------------------------------------------------------------

fn prediction_signals_bin() -> &'static str {
    env!("CARGO_BIN_EXE_prediction-signals")
}

fn run_cli(args: &[&str]) -> std::process::Output {
    std::process::Command::new(prediction_signals_bin())
        .args(args)
        .output()
        .expect("spawn prediction-signals")
}

#[test]
fn rotation_mode_cli_validation_prediction() {
    // --lane-root and --output are mutually exclusive (usage class).
    let conflict = run_cli(&[
        "run",
        "--venue",
        "kalshi",
        "--lane-root",
        "/tmp/does-not-matter",
        "--rotate-seconds",
        "60",
        "--output",
        "/tmp/o.jsonl",
        "--publisher-epoch",
        "e",
    ]);
    assert!(
        !conflict.status.success(),
        "lane-root + output must be rejected"
    );
    let stderr = String::from_utf8_lossy(&conflict.stderr);
    assert!(
        stderr.contains("cannot be used with") || stderr.contains("conflict"),
        "mutual exclusion is a usage error: {stderr}"
    );

    // --rotate-seconds requires --lane-root.
    let orphan = run_cli(&[
        "run",
        "--venue",
        "kalshi",
        "--rotate-seconds",
        "60",
        "--publisher-epoch",
        "e",
    ]);
    assert!(
        !orphan.status.success(),
        "rotate-seconds without lane-root must be rejected"
    );

    // --lane-root requires --rotate-seconds.
    let orphan_root = run_cli(&[
        "run",
        "--venue",
        "kalshi",
        "--lane-root",
        "/tmp/x",
        "--publisher-epoch",
        "e",
    ]);
    assert!(
        !orphan_root.status.success(),
        "lane-root without rotate-seconds must be rejected"
    );

    // Rotation period below the floor is rejected.
    let short = run_cli(&[
        "run",
        "--venue",
        "kalshi",
        "--lane-root",
        "/tmp/x",
        "--rotate-seconds",
        "30",
        "--publisher-epoch",
        "e",
    ]);
    assert!(
        !short.status.success(),
        "rotate-seconds below 60 must be rejected"
    );

    // Explicit mode without --output is rejected.
    let missing_output = run_cli(&[
        "run",
        "--venue",
        "kalshi",
        "--journal",
        "/tmp/j.jsonl",
        "--checkpoint",
        "/tmp/c.json",
        "--publisher-epoch",
        "e",
    ]);
    assert!(
        !missing_output.status.success(),
        "explicit mode requires --output"
    );
}

// ---------------------------------------------------------------------------
// Test: rotation restarts every per-identity decision sequence at zero
// ---------------------------------------------------------------------------

/// The greatest per-identity ready `sequence` any decision in `bytes` reaches,
/// so a run that advanced its emitters beyond zero can be told apart from one
/// that never published.
fn max_identity_sequence(decisions: &[Decision]) -> u64 {
    decisions
        .iter()
        .flat_map(|decision| decision.updates.iter())
        .filter(|update| update.quality.state == QualityState::Ready)
        .map(|update| update.sequence)
        .max()
        .unwrap_or(0)
}

/// Assert that, in decision order, every identity's ready `sequence` values start
/// at zero and increment by one — the "first ready decision in an epoch is
/// sequence zero, later ones increment" contract, per identity. Non-ready and
/// coalesced (suppressed) evaluations consume no sequence and never appear here,
/// so the published ready updates for one identity are contiguous from zero.
fn assert_first_ready_sequences_restart(decisions: &[Decision], label: &str) {
    let mut expected: BTreeMap<(String, String), u64> = BTreeMap::new();
    let mut asserted = 0_usize;
    for decision in decisions {
        for update in &decision.updates {
            if update.quality.state != QualityState::Ready {
                continue;
            }
            let key = (update.signal_id.clone(), scope_key(&update.scope));
            let want = expected.entry(key.clone()).or_insert(0);
            assert_eq!(
                update.sequence, *want,
                "{label}: {key:?} ready decision #{want} must carry sequence {want}, got {}",
                update.sequence
            );
            *want += 1;
            asserted += 1;
        }
    }
    assert!(
        asserted > 0,
        "{label}: at least one ready decision was checked"
    );
    assert!(
        expected.values().any(|&count| count > 1),
        "{label}: at least one identity published more than once, so the increment is exercised"
    );
}

/// C5 Design B: each file generation is a fresh output epoch, so the first ready
/// decision of EVERY identity in a rotation-opened generation carries per-identity
/// `sequence` zero and later ones increment — exactly as the spot lane's
/// `rotate_output_epoch` restarts its emitters. The warm processor crosses the
/// boundary otherwise intact, so before the fix its emitters carried their
/// departed sequence into the successor's epoch. The rotated generation's journal
/// replay (a crash-resume of that generation from the sealed checkpoint) numbers
/// its identities from zero too, so the live run and its replay agree and the
/// durable lifetime counters the resume restores are not what drives the
/// sequence.
#[test]
fn rotation_restarts_per_identity_decision_sequence_prediction() {
    // The armed Kalshi fixture publishes decisions without an external oracle feed.
    let dir = TempDir::new("t-seq-reset");
    let arts = build_rotated_three(dir.path(), PredictionMarketVenue::Kalshi, true, "kalshi");
    assert_eq!(arts.len(), 3, "three generations (two rotations)");

    // Generation 0 advanced at least one identity's sequence beyond zero, so a
    // sequence carried across the boundary would be visible in generation 1.
    let gen0_max = max_identity_sequence(&decisions(&arts[0].output));
    assert!(
        gen0_max > 0,
        "gen0 advanced an identity's sequence beyond zero (max {gen0_max})"
    );

    // Generation 1 (rotation-opened): every identity restarts at zero.
    assert_first_ready_sequences_restart(&decisions(&arts[1].output), "gen1 live");

    // The replay of generation 1's journal reproduces the same numbering.
    let gen1_replay = replay_journal(&arts[1].journal_path);
    assert_first_ready_sequences_restart(&decisions(&gen1_replay), "gen1 replay");
}
