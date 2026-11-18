//! Durable journal for the generic venue lane (A6, ruling E24/E26).
//!
//! A third durability stack modelled on the prediction lane's
//! venue-parameterized journal, sharing the [`crate::journal_core`] framing and
//! writer machinery. Unlike the spot journal it stamps its venue set as a
//! `Vec<String>` — the lane's tokens in `LaneRequirement.required` order — and
//! its processor id/version as plain fields, so one implementation serves the
//! `binance`, `bybit` and `oracle` lanes. The spot and prediction journals are
//! untouched.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Read};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::{self, SyncSender};
use std::thread::{self, JoinHandle};

use chrono::DateTime;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use signals_core::SourceEvent;
use thiserror::Error;

use crate::digest::{hex_lower, is_lower_hex, sha256_hex};
use crate::journal_core::{
    self, JournalRecords, MAX_JOURNAL_LINE_BYTES, WriterMessage, statvfs_available_bytes,
};
use crate::lane_checkpoint::LaneCheckpointDocument;

/// Schema 2 (A6 re-audit): the session's contract fingerprint gained the required
/// `source_definition_sha256` / `signal_definition_sha256` maps, so a schema-1
/// session no longer deserializes. venue-signals was never released at schema 1,
/// so the read range is exactly the current version; a future additive change
/// raises this and adds a retained lower bound (ruling N2).
pub const LANE_JOURNAL_SCHEMA_VERSION: u16 = 2;

/// Oldest journal schema this build reads; writing is current-version only.
/// Symmetric with the checkpoint's read range so the next bump follows the N2
/// rule on both files.
pub const LANE_JOURNAL_MIN_READ_SCHEMA_VERSION: u16 = 2;

/// Whether a journal at `version` can be read by this build.
#[must_use]
pub const fn readable_journal_version(version: u16) -> bool {
    version >= LANE_JOURNAL_MIN_READ_SCHEMA_VERSION && version <= LANE_JOURNAL_SCHEMA_VERSION
}
pub const DEFAULT_LANE_JOURNAL_MAX_BYTES: u64 = 16 * 1024 * 1024 * 1024;
pub const DEFAULT_LANE_JOURNAL_QUEUE_CAPACITY: usize = 16_384;

const LANE_JOURNAL_BUFFER_BYTES: usize = journal_core::JOURNAL_BUFFER_BYTES;
const FINGERPRINT_DOMAIN: &[u8] = b"tiny.venue-lane.contract-fingerprint.v1\0";
const SIGNAL_STREAM_MESSAGE_SCHEMA: &str =
    include_str!("../../../schemas/signal-stream-message.schema.json");

fn is_false(value: &bool) -> bool {
    !*value
}

/// Semantic settings bound into the lane's contract fingerprint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaneRuntimeConfig {
    /// The lane name (`binance`, `bybit`, `oracle`).
    pub lane: String,
    /// Health-tick cadence in nanoseconds.
    pub health_interval_ns: u64,
    /// Whether this session emits observe-only `feed_health` frames. Additive
    /// capability: absent-and-false serializes to nothing so a pre-capability
    /// config re-serializes to identical fingerprint bytes.
    #[serde(default, skip_serializing_if = "is_false")]
    pub feed_health_stream: bool,
}

impl LaneRuntimeConfig {
    pub fn new(lane: impl Into<String>, health_interval_ns: u64) -> Self {
        Self {
            lane: lane.into(),
            health_interval_ns,
            feed_health_stream: true,
        }
    }

    pub fn validate(&self) -> Result<(), LaneJournalError> {
        if self.lane.trim().is_empty() || self.lane.len() > 64 {
            return Err(LaneJournalError::Invalid(
                "lane name must contain 1 to 64 bytes".to_owned(),
            ));
        }
        if self.health_interval_ns == 0 {
            return Err(LaneJournalError::Invalid(
                "health_interval_ns must be greater than zero".to_owned(),
            ));
        }
        Ok(())
    }

    /// Whether two configurations are equal apart from the additive
    /// `feed_health_stream` capability, so resuming a pre-capability checkpoint
    /// under a declaring binary is a legitimate upgrade.
    pub fn matches_ignoring_feed_health(&self, other: &Self) -> bool {
        let mut normalized = self.clone();
        normalized.feed_health_stream = other.feed_health_stream;
        &normalized == other
    }
}

/// The lane's contract fingerprint: the lane's source and signal definition
/// documents, its runtime config and the stream-message schema, hashed once. The
/// definition set is a pure function of the lane name
/// ([`crate::lane::lane_definition_documents`]), so a checkpoint's recorded
/// runtime config is enough to recompute and verify it without the processor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaneContractFingerprint {
    pub aggregate_sha256: String,
    pub source_definition_sha256: BTreeMap<String, String>,
    pub signal_definition_sha256: BTreeMap<String, String>,
    pub runtime_config_sha256: String,
}

impl LaneContractFingerprint {
    pub fn validate(&self, runtime_config: &LaneRuntimeConfig) -> Result<(), LaneJournalError> {
        for (name, value) in [
            ("aggregate_sha256", &self.aggregate_sha256),
            ("runtime_config_sha256", &self.runtime_config_sha256),
        ] {
            if value.len() != 64 || !is_lower_hex(value) {
                return Err(LaneJournalError::Invalid(format!(
                    "{name} must be a lowercase SHA-256 digest"
                )));
            }
        }
        let expected = lane_contract_fingerprint(runtime_config)?;
        if &expected != self {
            return Err(LaneJournalError::FingerprintMismatch {
                expected: expected.aggregate_sha256,
                actual: self.aggregate_sha256.clone(),
            });
        }
        Ok(())
    }
}

pub fn lane_contract_fingerprint(
    runtime_config: &LaneRuntimeConfig,
) -> Result<LaneContractFingerprint, LaneJournalError> {
    runtime_config.validate()?;
    let runtime_bytes = serde_json::to_vec(runtime_config)?;
    let (sources, signals) = crate::lane::lane_definition_documents(&runtime_config.lane)
        .ok_or_else(|| {
            LaneJournalError::Invalid(format!("unknown lane {:?}", runtime_config.lane))
        })?;
    let mut source_definition_sha256 = BTreeMap::new();
    for (id, document) in &sources {
        if source_definition_sha256
            .insert(id.clone(), sha256_hex(document.as_bytes()))
            .is_some()
        {
            return Err(LaneJournalError::Invalid(format!(
                "duplicate lane source definition identity {id}"
            )));
        }
    }
    let mut signal_definition_sha256 = BTreeMap::new();
    for (id, document) in &signals {
        if signal_definition_sha256
            .insert(id.clone(), sha256_hex(document.as_bytes()))
            .is_some()
        {
            return Err(LaneJournalError::Invalid(format!(
                "duplicate lane signal definition identity {id}"
            )));
        }
    }
    let mut aggregate = Sha256::new();
    aggregate.update(FINGERPRINT_DOMAIN);
    aggregate.update(runtime_config.lane.as_bytes());
    // Definitions in id order (BTreeMap iterates sorted), each length-prefixed so
    // no id/digest boundary is ambiguous.
    for (id, digest) in source_definition_sha256
        .iter()
        .chain(signal_definition_sha256.iter())
    {
        aggregate.update((id.len() as u64).to_be_bytes());
        aggregate.update(id.as_bytes());
        aggregate.update(digest.as_bytes());
    }
    aggregate.update(SIGNAL_STREAM_MESSAGE_SCHEMA.as_bytes());
    aggregate.update(&runtime_bytes);
    Ok(LaneContractFingerprint {
        aggregate_sha256: format!("{:x}", aggregate.finalize()),
        source_definition_sha256,
        signal_definition_sha256,
        runtime_config_sha256: sha256_hex(&runtime_bytes),
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaneJournalSession {
    pub journal_schema_version: u16,
    pub session_id: String,
    pub wall_start: String,
    pub lane: String,
    pub venues: Vec<String>,
    pub processor_id: String,
    pub processor_version: String,
    pub publisher_epoch: String,
    pub contract_fingerprint: LaneContractFingerprint,
    pub runtime_config: LaneRuntimeConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_checkpoint: Option<LaneCheckpointDocument>,
    /// Whether this session opened a rotation-successor file generation (C5). A
    /// rotated generation's journal begins mid-stream: its first book records are a
    /// `resync` re-snapshot rebuilt from the warm in-memory book, and its live rings
    /// were warm while a cold replay re-warms them from the sealed checkpoint (which
    /// carries no ring state). So the terminal stream-message-count self-check is
    /// warm-vs-cold for this session and reports the difference as the documented
    /// re-warm gap instead of failing (decision 23:30Z; C5b closes the gap). Additive,
    /// byte-safe: absent ⇒ false, and a non-rotation session omits it, so every pre-C5
    /// journal re-serializes to identical bytes.
    #[serde(default, skip_serializing_if = "is_false")]
    pub opened_by_rotation: bool,
}

/// The identity a lane's processor stamps into its journal session and
/// checkpoint.
#[derive(Debug, Clone)]
pub struct LaneIdentity {
    pub lane: &'static str,
    pub venues: Vec<String>,
    pub processor_id: String,
    pub processor_version: String,
}

impl LaneJournalSession {
    pub fn new(
        wall_start: String,
        publisher_epoch: String,
        identity: &LaneIdentity,
        runtime_config: LaneRuntimeConfig,
    ) -> Result<Self, LaneJournalError> {
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random).map_err(LaneJournalError::Entropy)?;
        let session = Self {
            journal_schema_version: LANE_JOURNAL_SCHEMA_VERSION,
            session_id: hex_lower(&random),
            wall_start,
            lane: identity.lane.to_owned(),
            venues: identity.venues.clone(),
            processor_id: identity.processor_id.clone(),
            processor_version: identity.processor_version.clone(),
            publisher_epoch,
            contract_fingerprint: lane_contract_fingerprint(&runtime_config)?,
            runtime_config,
            initial_checkpoint: None,
            opened_by_rotation: false,
        };
        session.validate()?;
        Ok(session)
    }

    pub fn resumed(
        publisher_epoch: String,
        initial_checkpoint: LaneCheckpointDocument,
        identity: &LaneIdentity,
        runtime_config: LaneRuntimeConfig,
    ) -> Result<Self, LaneJournalError> {
        initial_checkpoint
            .validate()
            .map_err(|error| LaneJournalError::Invalid(error.to_string()))?;
        if !initial_checkpoint
            .runtime_config
            .matches_ignoring_feed_health(&runtime_config)
        {
            return Err(LaneJournalError::Invalid(
                "resume runtime configuration differs from the checkpoint beyond the feed-health capability".to_owned(),
            ));
        }
        if initial_checkpoint
            .publisher_epoch_lineage
            .iter()
            .any(|epoch| epoch == &publisher_epoch)
        {
            return Err(LaneJournalError::Invalid(
                "resumed publisher_epoch already appears in checkpoint lineage".to_owned(),
            ));
        }
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random).map_err(LaneJournalError::Entropy)?;
        let contract_fingerprint = lane_contract_fingerprint(&runtime_config)?;
        let session = Self {
            journal_schema_version: LANE_JOURNAL_SCHEMA_VERSION,
            session_id: hex_lower(&random),
            wall_start: initial_checkpoint.wall_start.clone(),
            lane: identity.lane.to_owned(),
            venues: identity.venues.clone(),
            processor_id: identity.processor_id.clone(),
            processor_version: identity.processor_version.clone(),
            publisher_epoch,
            contract_fingerprint,
            runtime_config,
            initial_checkpoint: Some(initial_checkpoint),
            opened_by_rotation: false,
        };
        session.validate()?;
        Ok(session)
    }

    /// Mark this session as a rotation-opened successor generation (C5). Only the
    /// in-process rotation path sets this; a crash-restart resume leaves it false, so
    /// its honest ring re-warm keeps the strict terminal parity check. A rotation
    /// session always resumes from the sealed checkpoint, which [`Self::validate`]
    /// enforces.
    #[must_use]
    pub fn opened_by_rotation(mut self) -> Self {
        self.opened_by_rotation = true;
        self
    }

    pub fn validate(&self) -> Result<(), LaneJournalError> {
        if !readable_journal_version(self.journal_schema_version) {
            return Err(LaneJournalError::UnsupportedVersion(
                self.journal_schema_version,
            ));
        }
        if self.session_id.len() != 32 || !is_lower_hex(&self.session_id) {
            return Err(LaneJournalError::Invalid(
                "session_id must be 128 bits encoded as lowercase hexadecimal".to_owned(),
            ));
        }
        DateTime::parse_from_rfc3339(&self.wall_start).map_err(|error| {
            LaneJournalError::Invalid(format!("wall_start is invalid: {error}"))
        })?;
        if self.publisher_epoch.trim().is_empty() || self.publisher_epoch.len() > 128 {
            return Err(LaneJournalError::Invalid(
                "publisher_epoch must contain 1 to 128 bytes".to_owned(),
            ));
        }
        if self.processor_id.trim().is_empty() || self.processor_version.trim().is_empty() {
            return Err(LaneJournalError::Invalid(
                "processor id and version must be non-empty".to_owned(),
            ));
        }
        if self.lane != self.runtime_config.lane {
            return Err(LaneJournalError::Invalid(
                "session lane disagrees with runtime configuration".to_owned(),
            ));
        }
        self.runtime_config.validate()?;
        self.contract_fingerprint.validate(&self.runtime_config)?;
        if let Some(checkpoint) = &self.initial_checkpoint {
            checkpoint
                .validate()
                .map_err(|error| LaneJournalError::Invalid(error.to_string()))?;
            if checkpoint.wall_start != self.wall_start
                || checkpoint.processor_id != self.processor_id
                || !checkpoint
                    .runtime_config
                    .matches_ignoring_feed_health(&self.runtime_config)
            {
                return Err(LaneJournalError::Invalid(
                    "initial checkpoint does not match the journal contract or time origin"
                        .to_owned(),
                ));
            }
        }
        if self.opened_by_rotation && self.initial_checkpoint.is_none() {
            return Err(LaneJournalError::Invalid(
                "a rotation-opened session must carry the sealed initial checkpoint".to_owned(),
            ));
        }
        Ok(())
    }

    fn validate_writable(&self) -> Result<(), LaneJournalError> {
        if self.journal_schema_version != LANE_JOURNAL_SCHEMA_VERSION {
            return Err(LaneJournalError::Invalid(format!(
                "journals are written only at schema {LANE_JOURNAL_SCHEMA_VERSION}; refusing to write schema {}",
                self.journal_schema_version
            )));
        }
        self.validate()
    }
}

/// One admitted input to the lane pipeline. Feed-only lanes never mix venues in
/// one batch: a batch carries one stream's events.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "input_type", rename_all = "snake_case", deny_unknown_fields)]
pub enum LaneJournalInput {
    Events {
        admission_seq: u64,
        stream: String,
        events: Arc<Vec<SourceEvent>>,
        logical_receipt_ns: u64,
        now_ns: u64,
        /// Whether this batch is a C5 rotation resync — a complete book re-snapshot
        /// rebuilt from the warm in-memory book and admitted as the first records of
        /// a rotation-opened generation, not a received venue frame (decision 23:30Z,
        /// amendment 02:39Z). On the warm live process the rows are ordering-skipped
        /// and change nothing; on a cold replay they rebuild the book the sealed
        /// checkpoint does not carry. Additive, byte-safe: absent ⇒ false, and every
        /// non-resync batch omits it, so existing journals are byte-unchanged.
        #[serde(default, skip_serializing_if = "is_false")]
        resync: bool,
    },
    Connectivity {
        admission_seq: u64,
        stream: String,
        connected: bool,
        now_ns: u64,
    },
    HealthTick {
        admission_seq: u64,
        now_ns: u64,
    },
    /// An operational notice a poll driver surfaced (a start-up call or reconnect
    /// notice): journaled as evidence, produces no output (M1, ruling E39(c)).
    Notice {
        admission_seq: u64,
        stream: String,
        code: String,
        detail: String,
        now_ns: u64,
    },
}

impl LaneJournalInput {
    pub fn admission_seq(&self) -> u64 {
        match self {
            Self::Events { admission_seq, .. }
            | Self::Connectivity { admission_seq, .. }
            | Self::HealthTick { admission_seq, .. }
            | Self::Notice { admission_seq, .. } => *admission_seq,
        }
    }

    pub fn now_ns(&self) -> u64 {
        match self {
            Self::Events { now_ns, .. }
            | Self::Connectivity { now_ns, .. }
            | Self::HealthTick { now_ns, .. }
            | Self::Notice { now_ns, .. } => *now_ns,
        }
    }

    /// Whether this input is a C5 rotation resync batch (a rebuilt book snapshot,
    /// not a received venue frame). Every non-`Events` input is never a resync.
    #[must_use]
    pub fn is_resync(&self) -> bool {
        matches!(self, Self::Events { resync: true, .. })
    }

    fn validate(&self) -> Result<(), LaneJournalError> {
        match self {
            Self::Events { stream, events, .. } => {
                if stream.trim().is_empty() {
                    return Err(LaneJournalError::Invalid(
                        "event batch stream label is empty".to_owned(),
                    ));
                }
                if events.is_empty() {
                    return Err(LaneJournalError::Invalid(
                        "event batch carries no events".to_owned(),
                    ));
                }
            }
            Self::Connectivity { stream, .. } => {
                if stream.trim().is_empty() {
                    return Err(LaneJournalError::Invalid(
                        "connectivity stream label is empty".to_owned(),
                    ));
                }
            }
            Self::HealthTick { .. } => {}
            Self::Notice { stream, .. } => {
                if stream.trim().is_empty() {
                    return Err(LaneJournalError::Invalid(
                        "notice stream label is empty".to_owned(),
                    ));
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaneJournalTotals {
    pub admitted_inputs: u64,
    pub event_batches: u64,
    pub connectivity_transitions: u64,
    pub health_ticks: u64,
    pub stream_messages_written: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaneJournalEnd {
    pub totals: LaneJournalTotals,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "record_type", rename_all = "snake_case", deny_unknown_fields)]
enum LaneJournalRecord {
    LaneSession {
        #[serde(flatten)]
        session: Box<LaneJournalSession>,
    },
    Events {
        admission_seq: u64,
        stream: String,
        events: Arc<Vec<SourceEvent>>,
        logical_receipt_ns: u64,
        now_ns: u64,
        /// See [`LaneJournalInput::Events::resync`]. Additive, byte-safe.
        #[serde(default, skip_serializing_if = "is_false")]
        resync: bool,
    },
    Connectivity {
        admission_seq: u64,
        stream: String,
        connected: bool,
        now_ns: u64,
    },
    HealthTick {
        admission_seq: u64,
        now_ns: u64,
    },
    Notice {
        admission_seq: u64,
        stream: String,
        code: String,
        detail: String,
        now_ns: u64,
    },
    End {
        #[serde(flatten)]
        end: LaneJournalEnd,
    },
}

impl From<LaneJournalInput> for LaneJournalRecord {
    fn from(input: LaneJournalInput) -> Self {
        match input {
            LaneJournalInput::Events {
                admission_seq,
                stream,
                events,
                logical_receipt_ns,
                now_ns,
                resync,
            } => Self::Events {
                admission_seq,
                stream,
                events,
                logical_receipt_ns,
                now_ns,
                resync,
            },
            LaneJournalInput::Connectivity {
                admission_seq,
                stream,
                connected,
                now_ns,
            } => Self::Connectivity {
                admission_seq,
                stream,
                connected,
                now_ns,
            },
            LaneJournalInput::HealthTick {
                admission_seq,
                now_ns,
            } => Self::HealthTick {
                admission_seq,
                now_ns,
            },
            LaneJournalInput::Notice {
                admission_seq,
                stream,
                code,
                detail,
                now_ns,
            } => Self::Notice {
                admission_seq,
                stream,
                code,
                detail,
                now_ns,
            },
        }
    }
}

impl LaneJournalRecord {
    fn into_input(self) -> Option<LaneJournalInput> {
        match self {
            Self::Events {
                admission_seq,
                stream,
                events,
                logical_receipt_ns,
                now_ns,
                resync,
            } => Some(LaneJournalInput::Events {
                admission_seq,
                stream,
                events,
                logical_receipt_ns,
                now_ns,
                resync,
            }),
            Self::Connectivity {
                admission_seq,
                stream,
                connected,
                now_ns,
            } => Some(LaneJournalInput::Connectivity {
                admission_seq,
                stream,
                connected,
                now_ns,
            }),
            Self::HealthTick {
                admission_seq,
                now_ns,
            } => Some(LaneJournalInput::HealthTick {
                admission_seq,
                now_ns,
            }),
            Self::Notice {
                admission_seq,
                stream,
                code,
                detail,
                now_ns,
            } => Some(LaneJournalInput::Notice {
                admission_seq,
                stream,
                code,
                detail,
                now_ns,
            }),
            Self::LaneSession { .. } | Self::End { .. } => None,
        }
    }
}

struct LaneRecords;

impl JournalRecords for LaneRecords {
    type Input = LaneJournalInput;
    type End = LaneJournalEnd;
    type Record = LaneJournalRecord;

    fn input_record(input: LaneJournalInput) -> LaneJournalRecord {
        LaneJournalRecord::from(input)
    }

    fn end_record(end: LaneJournalEnd) -> LaneJournalRecord {
        LaneJournalRecord::End { end }
    }
}

type LaneWriterMessage = WriterMessage<LaneJournalInput, LaneJournalEnd>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LaneJournalWriteReport {
    pub records_written: u64,
    pub admitted_inputs: u64,
    pub bytes_written: u64,
    pub data_syncs: u64,
}

pub struct LaneJournalWriter {
    sender: Option<SyncSender<LaneWriterMessage>>,
    worker: Option<JoinHandle<Result<LaneJournalWriteReport, LaneJournalError>>>,
    next_admission_seq: u64,
}

impl LaneJournalWriter {
    pub fn create(
        path: &Path,
        session: LaneJournalSession,
        maximum_bytes: u64,
    ) -> Result<Self, LaneJournalError> {
        Self::create_with_queue_capacity(
            path,
            session,
            maximum_bytes,
            DEFAULT_LANE_JOURNAL_QUEUE_CAPACITY,
            journal_core::MINIMUM_FREE_SPACE_BYTES,
        )
    }

    fn create_with_queue_capacity(
        path: &Path,
        session: LaneJournalSession,
        maximum_bytes: u64,
        queue_capacity: usize,
        minimum_free_space_bytes: u64,
    ) -> Result<Self, LaneJournalError> {
        if maximum_bytes == 0 {
            return Err(LaneJournalError::Invalid(
                "maximum journal bytes must be positive".to_owned(),
            ));
        }
        if queue_capacity == 0 {
            return Err(LaneJournalError::Invalid(
                "journal queue capacity must be positive".to_owned(),
            ));
        }
        session.validate_writable()?;
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        options.mode(0o600);
        let file = options.open(path)?;
        let next_admission_seq = session
            .initial_checkpoint
            .as_ref()
            .map_or(0, |checkpoint| checkpoint.next_admission_seq);
        let mut writer = BufWriter::with_capacity(LANE_JOURNAL_BUFFER_BYTES, file);
        let watermark = journal_core::Watermark::new(
            path.to_path_buf(),
            minimum_free_space_bytes,
            statvfs_available_bytes,
        );
        let mut core = journal_core::WriterCore::with_watermark(maximum_bytes, Some(watermark));
        core.check_watermark()?;
        core.write_record(
            &mut writer,
            &LaneJournalRecord::LaneSession {
                session: Box::new(session),
            },
        )?;
        core.sync(&mut writer)?;

        let (sender, receiver) = mpsc::sync_channel(queue_capacity);
        let worker = thread::Builder::new()
            .name("btc-venue-lane-journal".to_owned())
            .spawn(
                move || -> Result<LaneJournalWriteReport, LaneJournalError> {
                    let core = journal_core::writer_loop::<LaneRecords>(writer, core, receiver)?;
                    Ok(LaneJournalWriteReport {
                        records_written: core.records_written,
                        admitted_inputs: core.admitted_inputs,
                        bytes_written: core.bytes_written,
                        data_syncs: core.data_syncs,
                    })
                },
            )?;
        Ok(Self {
            sender: Some(sender),
            worker: Some(worker),
            next_admission_seq,
        })
    }

    pub fn next_admission_seq(&self) -> u64 {
        self.next_admission_seq
    }

    pub fn enqueue(&mut self, input: LaneJournalInput) -> Result<(), LaneJournalError> {
        if input.admission_seq() != self.next_admission_seq {
            return Err(LaneJournalError::Sequence {
                expected: self.next_admission_seq,
                actual: input.admission_seq(),
            });
        }
        input.validate()?;
        let sender = self
            .sender
            .as_ref()
            .ok_or(LaneJournalError::WriterStopped)?;
        journal_core::try_enqueue(sender, WriterMessage::Input(input))
            .map_err(LaneJournalError::from)?;
        self.next_admission_seq = self
            .next_admission_seq
            .checked_add(1)
            .ok_or(LaneJournalError::Counter)?;
        Ok(())
    }

    pub fn finish(
        mut self,
        end: LaneJournalEnd,
    ) -> Result<LaneJournalWriteReport, LaneJournalError> {
        if end.totals.admitted_inputs != self.next_admission_seq {
            return Err(LaneJournalError::Invalid(
                "journal end total disagrees with admitted inputs".to_owned(),
            ));
        }
        let send_result = self
            .sender
            .take()
            .ok_or(LaneJournalError::WriterStopped)?
            .send(WriterMessage::End(end));
        let joined = self.join_worker();
        match joined {
            Err(error) => Err(error),
            Ok(report) => {
                send_result.map_err(|_| LaneJournalError::WriterStopped)?;
                Ok(report)
            }
        }
    }

    pub fn abort(mut self) -> Result<LaneJournalWriteReport, LaneJournalError> {
        self.sender.take();
        self.join_worker()
    }

    fn join_worker(&mut self) -> Result<LaneJournalWriteReport, LaneJournalError> {
        self.worker
            .take()
            .ok_or(LaneJournalError::WriterStopped)?
            .join()
            .map_err(|_| LaneJournalError::WriterPanicked)?
    }
}

#[derive(Deserialize)]
struct RecordTypeEnvelope {
    record_type: String,
}

#[derive(Deserialize)]
struct VersionEnvelope {
    journal_schema_version: u16,
}

pub struct LaneJournalReplayReader {
    reader: BufReader<File>,
    line_number: u64,
    session: LaneJournalSession,
    next_admission_seq: u64,
    last_now_ns: Option<u64>,
    maximum_bytes: u64,
    bytes_read: u64,
    line: Vec<u8>,
    end: Option<LaneJournalEnd>,
}

impl LaneJournalReplayReader {
    pub fn open(path: &Path, maximum_bytes: u64) -> Result<Self, LaneJournalError> {
        if maximum_bytes == 0 {
            return Err(LaneJournalError::Invalid(
                "maximum journal bytes must be positive".to_owned(),
            ));
        }
        let file = open_owner_only(path)?;
        let size = file.metadata()?.len();
        if size > maximum_bytes {
            return Err(LaneJournalError::SizeLimit {
                attempted: size,
                maximum: maximum_bytes,
            });
        }
        let mut reader = BufReader::with_capacity(1024 * 1024, file);
        let mut line = Vec::new();
        let first_bytes = read_bounded_line(&mut reader, &mut line)?;
        if first_bytes == 0 {
            return Err(serde_json::from_slice::<serde_json::Value>(&line)
                .expect_err("empty input cannot parse as JSON")
                .into());
        }
        let had_newline = line.ends_with(b"\n");
        let trimmed = trim_newline(&line);
        let is_final = !had_newline
            || !journal_core::more_bytes_follow(&mut reader).map_err(LaneJournalError::Io)?;
        let record_bytes: &[u8] = match journal_core::classify_first_frame(trimmed, is_final) {
            Ok(journal_core::FirstFrame::EnvelopedV5(bytes)) => bytes,
            Ok(journal_core::FirstFrame::BareV4(_)) => {
                return Err(LaneJournalError::Invalid(
                    "lane journals are CRC-enveloped; a bare first record is unsupported"
                        .to_owned(),
                ));
            }
            Err(journal_core::FramingError::TornTail) => return Err(LaneJournalError::TornTail),
            Err(journal_core::FramingError::Corruption { line }) => {
                return Err(LaneJournalError::Corruption { line });
            }
        };
        if !had_newline {
            return Err(LaneJournalError::TornTail);
        }
        let record_type: RecordTypeEnvelope = serde_json::from_slice(record_bytes)?;
        if record_type.record_type != "lane_session" {
            return Err(LaneJournalError::Invalid(
                "first record must be lane_session".to_owned(),
            ));
        }
        let version: VersionEnvelope = serde_json::from_slice(record_bytes)?;
        if !readable_journal_version(version.journal_schema_version) {
            return Err(LaneJournalError::UnsupportedVersion(
                version.journal_schema_version,
            ));
        }
        let LaneJournalRecord::LaneSession { session } = serde_json::from_slice(record_bytes)?
        else {
            return Err(LaneJournalError::Invalid(
                "first record must be lane_session".to_owned(),
            ));
        };
        let session = *session;
        session.validate()?;
        let next_admission_seq = session
            .initial_checkpoint
            .as_ref()
            .map_or(0, |checkpoint| checkpoint.next_admission_seq);
        let last_now_ns = session
            .initial_checkpoint
            .as_ref()
            .map(|checkpoint| checkpoint.checkpoint_time_ns);
        let bytes_read = u64::try_from(line.len()).map_err(|_| LaneJournalError::Counter)?;
        Ok(Self {
            reader,
            line_number: 1,
            session,
            next_admission_seq,
            last_now_ns,
            maximum_bytes,
            bytes_read,
            line,
            end: None,
        })
    }

    pub fn session(&self) -> &LaneJournalSession {
        &self.session
    }

    pub fn next_input(&mut self) -> Result<Option<LaneJournalInput>, LaneJournalError> {
        if self.end.is_some() {
            return Ok(None);
        }
        self.line.clear();
        let bytes = read_bounded_line(&mut self.reader, &mut self.line)?;
        if bytes == 0 {
            return Err(LaneJournalError::MissingEnd);
        }
        self.bytes_read = self
            .bytes_read
            .checked_add(u64::try_from(bytes).map_err(|_| LaneJournalError::Counter)?)
            .ok_or(LaneJournalError::Counter)?;
        if self.bytes_read > self.maximum_bytes {
            return Err(LaneJournalError::SizeLimit {
                attempted: self.bytes_read,
                maximum: self.maximum_bytes,
            });
        }
        self.line_number = self
            .line_number
            .checked_add(1)
            .ok_or(LaneJournalError::Counter)?;
        if !self.line.ends_with(b"\n") {
            return Err(LaneJournalError::TornTail);
        }
        let is_final =
            !journal_core::more_bytes_follow(&mut self.reader).map_err(LaneJournalError::Io)?;
        let record_bytes = match journal_core::enveloped_record_bytes(
            trim_newline(&self.line),
            is_final,
            self.line_number,
        ) {
            Ok(bytes) => bytes,
            Err(journal_core::FramingError::TornTail) => return Err(LaneJournalError::TornTail),
            Err(journal_core::FramingError::Corruption { line }) => {
                return Err(LaneJournalError::Corruption { line });
            }
        };
        let record = serde_json::from_slice::<LaneJournalRecord>(record_bytes)?;
        match record {
            LaneJournalRecord::Events { .. }
            | LaneJournalRecord::Connectivity { .. }
            | LaneJournalRecord::HealthTick { .. }
            | LaneJournalRecord::Notice { .. } => {
                let input = record.into_input().ok_or_else(|| {
                    LaneJournalError::Invalid("lane input record could not be decoded".to_owned())
                })?;
                if input.admission_seq() != self.next_admission_seq {
                    return Err(LaneJournalError::Sequence {
                        expected: self.next_admission_seq,
                        actual: input.admission_seq(),
                    });
                }
                input.validate()?;
                if self
                    .last_now_ns
                    .is_some_and(|previous| input.now_ns() < previous)
                {
                    return Err(LaneJournalError::ClockRegression);
                }
                self.last_now_ns = Some(input.now_ns());
                self.next_admission_seq = self
                    .next_admission_seq
                    .checked_add(1)
                    .ok_or(LaneJournalError::Counter)?;
                Ok(Some(input))
            }
            LaneJournalRecord::End { end } => {
                if end.totals.admitted_inputs != self.next_admission_seq {
                    return Err(LaneJournalError::Invalid(
                        "journal end total disagrees with replayed input count".to_owned(),
                    ));
                }
                self.end = Some(end);
                self.ensure_eof()?;
                Ok(None)
            }
            LaneJournalRecord::LaneSession { .. } => Err(LaneJournalError::Invalid(
                "journal contains a second session".to_owned(),
            )),
        }
    }

    pub fn end(&self) -> Option<&LaneJournalEnd> {
        self.end.as_ref()
    }

    fn ensure_eof(&mut self) -> Result<(), LaneJournalError> {
        self.line.clear();
        let bytes = read_bounded_line(&mut self.reader, &mut self.line)?;
        if bytes == 0 {
            return Ok(());
        }
        Err(LaneJournalError::Invalid(
            "journal contains records after end".to_owned(),
        ))
    }
}

fn open_owner_only(path: &Path) -> Result<File, LaneJournalError> {
    let path_metadata = std::fs::symlink_metadata(path)?;
    if path_metadata.file_type().is_symlink() || !path_metadata.is_file() {
        return Err(LaneJournalError::UnsafeTarget(
            "journal must be a regular non-symlink file".to_owned(),
        ));
    }
    #[cfg(unix)]
    if path_metadata.mode() & 0o077 != 0 {
        return Err(LaneJournalError::UnsafeTarget(format!(
            "journal permissions must deny group and other access; found {:03o}",
            path_metadata.mode() & 0o777
        )));
    }
    let file = File::open(path)?;
    #[cfg(unix)]
    {
        let metadata = file.metadata()?;
        if metadata.dev() != path_metadata.dev() || metadata.ino() != path_metadata.ino() {
            return Err(LaneJournalError::UnsafeTarget(
                "journal target changed while opening".to_owned(),
            ));
        }
    }
    Ok(file)
}

fn read_bounded_line(
    reader: &mut BufReader<File>,
    line: &mut Vec<u8>,
) -> Result<usize, LaneJournalError> {
    let bytes = reader
        .take((MAX_JOURNAL_LINE_BYTES as u64) + 1)
        .read_until(b'\n', line)?;
    if line.len() > MAX_JOURNAL_LINE_BYTES {
        return Err(LaneJournalError::LineLimit(MAX_JOURNAL_LINE_BYTES));
    }
    Ok(bytes)
}

fn trim_newline(line: &[u8]) -> &[u8] {
    line.strip_suffix(b"\n").unwrap_or(line)
}

#[derive(Debug, Error)]
pub enum LaneJournalError {
    #[error("unsupported lane journal version {0}")]
    UnsupportedVersion(u16),
    #[error("invalid lane journal: {0}")]
    Invalid(String),
    #[error("lane journal contract fingerprint mismatch: expected {expected}, found {actual}")]
    FingerprintMismatch { expected: String, actual: String },
    #[error("lane journal admission sequence mismatch: expected {expected}, found {actual}")]
    Sequence { expected: u64, actual: u64 },
    #[error("lane journal logical time regressed")]
    ClockRegression,
    #[error("lane journal exceeds {maximum} bytes at {attempted} bytes")]
    SizeLimit { attempted: u64, maximum: u64 },
    #[error("lane journal line exceeds {0} bytes")]
    LineLimit(usize),
    #[error(
        "lane journal free space {available} bytes is below the {minimum}-byte watermark; run stopped"
    )]
    LowDiskSpace { available: u64, minimum: u64 },
    #[error("lane journal tail is torn: the final record failed to parse or its CRC did not match")]
    TornTail,
    #[error("lane journal record on line {line} is corrupt")]
    Corruption { line: u64 },
    #[error("lane journal has no end record")]
    MissingEnd,
    #[error("lane journal queue is full")]
    QueueFull,
    #[error("lane journal writer stopped")]
    WriterStopped,
    #[error("lane journal writer thread panicked")]
    WriterPanicked,
    #[error("unsafe lane journal target: {0}")]
    UnsafeTarget(String),
    #[error("lane journal counter exhausted")]
    Counter,
    #[error("lane journal I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("lane journal JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("lane journal entropy failed: {0}")]
    Entropy(getrandom::Error),
}

impl From<journal_core::JournalCoreError> for LaneJournalError {
    fn from(error: journal_core::JournalCoreError) -> Self {
        use journal_core::JournalCoreError;
        match error {
            JournalCoreError::Io(error) => Self::Io(error),
            JournalCoreError::Json(error) => Self::Json(error),
            JournalCoreError::LineTooLarge { maximum } => Self::LineLimit(maximum),
            JournalCoreError::SizeLimit { attempted, maximum } => {
                Self::SizeLimit { attempted, maximum }
            }
            JournalCoreError::LowDiskSpace { available, minimum } => {
                Self::LowDiskSpace { available, minimum }
            }
            JournalCoreError::Counter => Self::Counter,
            JournalCoreError::QueueFull => Self::QueueFull,
            JournalCoreError::WriterStopped => Self::WriterStopped,
            JournalCoreError::InputAfterTerminal | JournalCoreError::MultipleTerminal => {
                Self::Invalid("journal writer received input after its terminal record".to_owned())
            }
        }
    }
}
