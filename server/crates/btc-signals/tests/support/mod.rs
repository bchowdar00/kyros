#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use btc_signals::checkpoint::CheckpointDocument;
use btc_signals::journal::{
    JournalEnd, JournalInput, JournalSession, JournalTotals, contract_fingerprint,
};
use btc_signals::{
    COINBASE_TICKER_SOURCE_ID, COINBASE_TICKER_SOURCE_VERSION, KRAKEN_TICKER_SOURCE_ID,
    KRAKEN_TICKER_SOURCE_VERSION, SpotReferenceRuntimeConfig, SpotSignalProcessor,
};
use chrono::{DateTime, Utc};
use serde_json::json;
use signals_core::SourceEvent;

static TEST_PATH_COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn wall_start() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-08-13T12:00:00Z")
        .expect("wall start parses")
        .with_timezone(&Utc)
}

/// CRC-32C (Castagnoli), independent of the crate implementation so tests that
/// re-frame or inspect journal envelopes verify against an external oracle.
pub fn crc32c(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFF_u32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0x82F6_3B78
            } else {
                crc >> 1
            };
        }
    }
    crc ^ 0xFFFF_FFFF
}

/// Lowercase, zero-padded eight-character hexadecimal CRC-32C.
pub fn crc32c_hex(bytes: &[u8]) -> String {
    format!("{:08x}", crc32c(bytes))
}

pub struct TestPath(PathBuf);

impl TestPath {
    pub fn new(label: &str, extension: &str) -> Self {
        let path = Self::unique(label, extension);
        Self(path)
    }

    pub fn absent(label: &str, extension: &str) -> Self {
        Self(Self::unique(label, extension))
    }

    pub fn sibling(path: &Self, label: &str) -> Self {
        let name = path
            .as_path()
            .file_name()
            .expect("test checkpoint has a file name")
            .to_string_lossy();
        Self(
            path.as_path()
                .with_file_name(format!(".{name}.{label}.tmp")),
        )
    }

    fn unique(label: &str, extension: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock follows Unix epoch")
            .as_nanos();
        // A process-wide counter guarantees uniqueness even when two threads read
        // the clock within the same nanosecond.
        let seq = TEST_PATH_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "btc-signal-{label}-{}-{unique}-{seq}.{extension}",
            std::process::id()
        ))
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

// --- shared spot-signal fixtures ------------------------------------------

/// A fresh spot-signal processor bound to `epoch` and the canonical wall start.
pub fn processor(epoch: &str) -> SpotSignalProcessor {
    SpotSignalProcessor::new(epoch, wall_start()).expect("processor constructs")
}

/// A checkpoint document over `processor`'s state at the given logical time and
/// next admission sequence, with the canonical depth-100 runtime config.
pub fn document(
    processor: &SpotSignalProcessor,
    checkpoint_time_ns: u64,
    next_admission_seq: u64,
) -> CheckpointDocument {
    let runtime_config = SpotReferenceRuntimeConfig::new(100).expect("runtime config constructs");
    CheckpointDocument::new(
        "0123456789abcdef0123456789abcdef".to_owned(),
        "2026-08-13T12:00:00Z".to_owned(),
        checkpoint_time_ns,
        next_admission_seq,
        contract_fingerprint(&runtime_config).expect("fingerprint constructs"),
        runtime_config,
        processor.checkpoint_state(),
    )
    .expect("checkpoint document constructs")
}

/// A journal session for `publisher_epoch` at the given book `depth`.
pub fn session(publisher_epoch: &str, depth: u16) -> JournalSession {
    JournalSession::new(
        "2026-08-13T12:00:00Z".to_owned(),
        publisher_epoch.to_owned(),
        SpotReferenceRuntimeConfig::new(depth).expect("test runtime config is valid"),
    )
    .expect("journal session constructs")
}

/// A single-event market journal input.
pub fn market(
    admission_seq: u64,
    event: SourceEvent,
    logical_receipt_ns: u64,
    now_ns: u64,
) -> JournalInput {
    JournalInput::MarketEvents {
        admission_seq,
        events: Arc::new(vec![event]),
        logical_receipt_ns,
        now_ns,
        resync: false,
    }
}

/// A terminal record carrying the canned `kraken_disconnected` quality reason.
pub fn terminal(
    admitted_inputs: u64,
    market_events: u64,
    availability_events: u64,
    health_ticks: u64,
    updates_written: u64,
) -> JournalEnd {
    JournalEnd {
        totals: JournalTotals {
            admitted_inputs,
            market_events,
            availability_events,
            health_ticks,
            updates_written,
            stream_messages_written: 0,
        },
        quality_reason_entries: BTreeMap::from([("kraken_disconnected".to_owned(), 1)]),
        raw_capture_references: Vec::new(),
    }
}

/// A terminal record with no recorded quality reasons.
pub fn terminal_without_quality_reasons(
    admitted_inputs: u64,
    market_events: u64,
    availability_events: u64,
    health_ticks: u64,
    updates_written: u64,
) -> JournalEnd {
    JournalEnd {
        totals: JournalTotals {
            admitted_inputs,
            market_events,
            availability_events,
            health_ticks,
            updates_written,
            stream_messages_written: 0,
        },
        quality_reason_entries: BTreeMap::new(),
        raw_capture_references: Vec::new(),
    }
}

/// A fully populated Coinbase ticker event for `sequence` under `publisher_epoch`.
pub fn coinbase_event(sequence: u64, publisher_epoch: &str) -> SourceEvent {
    SourceEvent {
        schema_version: 1,
        source_id: COINBASE_TICKER_SOURCE_ID.to_owned(),
        source_version: COINBASE_TICKER_SOURCE_VERSION.to_owned(),
        publisher_epoch: publisher_epoch.to_owned(),
        sequence,
        event_time: "2026-08-13T12:00:00Z".to_owned(),
        received_at: "2026-08-13T12:00:00.001Z".to_owned(),
        values: BTreeMap::from([
            ("instrument".to_owned(), json!("BTC-USD")),
            ("trade_id".to_owned(), json!(1)),
            ("price".to_owned(), json!("60001")),
            ("last_size".to_owned(), json!("0.01")),
            ("best_bid".to_owned(), json!("60000")),
            ("best_bid_size".to_owned(), json!("1")),
            ("best_ask".to_owned(), json!("60002")),
            ("best_ask_size".to_owned(), json!("1")),
            ("open_24h".to_owned(), json!("59000")),
            ("volume_24h".to_owned(), json!("100")),
            ("low_24h".to_owned(), json!("58000")),
            ("high_24h".to_owned(), json!("61000")),
            ("volume_30d".to_owned(), json!("3000")),
            ("event_time".to_owned(), json!("2026-08-13T12:00:00Z")),
            ("source_sequence".to_owned(), json!(sequence + 1)),
        ]),
    }
}

/// A fully populated Kraken ticker event for `sequence` under `publisher_epoch`.
pub fn kraken_event(sequence: u64, publisher_epoch: &str) -> SourceEvent {
    SourceEvent {
        schema_version: 1,
        source_id: KRAKEN_TICKER_SOURCE_ID.to_owned(),
        source_version: KRAKEN_TICKER_SOURCE_VERSION.to_owned(),
        publisher_epoch: publisher_epoch.to_owned(),
        sequence,
        event_time: "2026-08-13T12:00:00.002Z".to_owned(),
        received_at: "2026-08-13T12:00:00.003Z".to_owned(),
        values: BTreeMap::from([
            ("instrument".to_owned(), json!("BTC/USD")),
            ("best_bid".to_owned(), json!("60001")),
            ("best_bid_size".to_owned(), json!("1")),
            ("best_ask".to_owned(), json!("60003")),
            ("best_ask_size".to_owned(), json!("1")),
            ("last_price".to_owned(), json!("60002")),
            ("volume_24h".to_owned(), json!("100")),
            ("vwap_24h".to_owned(), json!("60000")),
            ("low_24h".to_owned(), json!("58000")),
            ("high_24h".to_owned(), json!("61000")),
            ("change_24h".to_owned(), json!("-1")),
            ("change_percent_24h".to_owned(), json!("-0.1")),
            ("trade_count_24h".to_owned(), json!(100)),
            ("update_kind".to_owned(), json!("update")),
            ("event_time".to_owned(), json!("2026-08-13T12:00:00.002Z")),
        ]),
    }
}

/// A Coinbase ticker envelope with no `values`, used where a test exercises
/// journal framing/ordering rather than processor ingestion. Behaviorally
/// distinct from `coinbase_event` (empty payload, smaller on-disk size).
pub fn bare_coinbase_event(sequence: u64, publisher_epoch: &str) -> SourceEvent {
    SourceEvent {
        schema_version: 1,
        source_id: COINBASE_TICKER_SOURCE_ID.to_owned(),
        source_version: COINBASE_TICKER_SOURCE_VERSION.to_owned(),
        publisher_epoch: publisher_epoch.to_owned(),
        sequence,
        event_time: "2026-08-13T12:00:00Z".to_owned(),
        received_at: "2026-08-13T12:00:00.001Z".to_owned(),
        values: BTreeMap::new(),
    }
}

/// A Kraken ticker envelope with no `values`. Behaviorally distinct from
/// `kraken_event` (empty payload and different timestamps).
pub fn bare_kraken_event(sequence: u64, publisher_epoch: &str) -> SourceEvent {
    SourceEvent {
        schema_version: 1,
        source_id: KRAKEN_TICKER_SOURCE_ID.to_owned(),
        source_version: KRAKEN_TICKER_SOURCE_VERSION.to_owned(),
        publisher_epoch: publisher_epoch.to_owned(),
        sequence,
        event_time: "2026-08-13T12:00:00Z".to_owned(),
        received_at: "2026-08-13T12:00:00.001Z".to_owned(),
        values: BTreeMap::new(),
    }
}

// --- shared byte-level journal surgery ------------------------------------

/// The final line of `path`, without its terminating newline.
pub fn last_line(path: &Path) -> String {
    std::fs::read_to_string(path)
        .expect("journal reads")
        .lines()
        .last()
        .expect("journal has a final line")
        .to_owned()
}

/// Rewrite `path` keeping every line but truncating the final line to its first
/// `keep` bytes and dropping its terminating newline.
pub fn truncate_last_line(path: &Path, keep: usize) {
    let text = std::fs::read_to_string(path).expect("journal reads");
    let mut lines: Vec<&str> = text.lines().collect();
    let last = lines.pop().expect("journal has a final line");
    let mut out = String::new();
    for line in &lines {
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(&last[..keep.min(last.len())]);
    std::fs::write(path, out).expect("truncated journal writes");
}

/// Drop a single trailing newline from `path`, if present.
pub fn strip_trailing_newline(path: &Path) {
    let mut bytes = std::fs::read(path).expect("journal reads");
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
    }
    std::fs::write(path, bytes).expect("journal rewrites");
}
