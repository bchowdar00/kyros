use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Read};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::{self, SyncSender};
use std::thread::{self, JoinHandle};

use chrono::DateTime;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use signals_core::{SourceEvent, is_spot_signal_id};
use thiserror::Error;

use crate::checkpoint::CheckpointDocument;
use crate::digest::{hex_lower, is_lower_hex, sha256_hex};
use crate::{SpotReferenceRuntimeConfig, UnavailableReason, Venue, spot_signal_contract_documents};

pub const JOURNAL_SCHEMA_VERSION: u16 = 5;
/// Schema 4 journals are read-only retained evidence (SIG-043): they predate CRC
/// framing and replay byte-identically through the current binary. New journals
/// are always written at [`JOURNAL_SCHEMA_VERSION`].
pub const RETAINED_JOURNAL_SCHEMA_VERSION: u16 = 4;
pub const DEFAULT_JOURNAL_QUEUE_CAPACITY: usize = 16_384;
pub const MAX_JOURNAL_LINE_BYTES: usize = crate::journal_core::MAX_JOURNAL_LINE_BYTES;

const JOURNAL_BUFFER_BYTES: usize = crate::journal_core::JOURNAL_BUFFER_BYTES;
const FINGERPRINT_DOMAIN: &[u8] = b"tiny.signal.contract-fingerprint.v3\0";
const SIGNAL_UPDATE_SCHEMA: &str = include_str!("../../../schemas/signal-update.schema.json");
const SIGNAL_STREAM_MESSAGE_SCHEMA: &str =
    include_str!("../../../schemas/signal-stream-message.schema.json");
const INSTANCE_STATUS_SCHEMA: &str = include_str!("../../../schemas/instance-status.schema.json");

/// Stream v3 is not output-neutral. Only the current schema is accepted; every
/// v2 fingerprint must refuse replay or restore rather than riding a legacy list.
const ACCEPTED_STREAM_SCHEMAS: &[&str] = &[SIGNAL_STREAM_MESSAGE_SCHEMA];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContractFingerprint {
    pub aggregate_sha256: String,
    pub signal_definition_sha256: BTreeMap<String, String>,
    pub source_definition_sha256: BTreeMap<String, String>,
    pub runtime_schema_sha256: BTreeMap<String, String>,
    pub runtime_config_sha256: String,
}

impl ContractFingerprint {
    fn validate(&self) -> Result<(), JournalError> {
        validate_digest("aggregate_sha256", &self.aggregate_sha256)?;
        validate_digest("runtime_config_sha256", &self.runtime_config_sha256)?;
        let documents = spot_signal_contract_documents();
        let expected_signals: Vec<&str> = documents
            .iter()
            .map(|(id, _)| *id)
            .filter(|id| is_spot_signal_id(id))
            .collect();
        let expected_sources: Vec<&str> = documents
            .iter()
            .map(|(id, _)| *id)
            .filter(|id| !is_spot_signal_id(id))
            .collect();
        if self.signal_definition_sha256.len() != expected_signals.len()
            || expected_signals
                .iter()
                .any(|id| !self.signal_definition_sha256.contains_key(*id))
        {
            return Err(JournalError::Invalid(
                "contract fingerprint does not contain the complete signal-definition set"
                    .to_owned(),
            ));
        }
        if self.source_definition_sha256.len() != expected_sources.len()
            || expected_sources
                .iter()
                .any(|id| !self.source_definition_sha256.contains_key(*id))
        {
            return Err(JournalError::Invalid(
                "contract fingerprint does not contain the complete source-definition set"
                    .to_owned(),
            ));
        }
        for (signal, digest) in &self.signal_definition_sha256 {
            validate_digest(&format!("signal_definition_sha256[{signal}]"), digest)?;
        }
        for (source, digest) in &self.source_definition_sha256 {
            validate_digest(&format!("source_definition_sha256[{source}]"), digest)?;
        }
        let expected_runtime_schemas =
            ["instance-status", "signal-stream-message", "signal-update"];
        if self.runtime_schema_sha256.len() != expected_runtime_schemas.len()
            || expected_runtime_schemas
                .iter()
                .any(|name| !self.runtime_schema_sha256.contains_key(*name))
        {
            return Err(JournalError::Invalid(
                "contract fingerprint does not contain the complete runtime-schema set".to_owned(),
            ));
        }
        for (schema, digest) in &self.runtime_schema_sha256 {
            validate_digest(&format!("runtime_schema_sha256[{schema}]"), digest)?;
        }
        Ok(())
    }
}

pub fn contract_fingerprint(
    runtime_config: &SpotReferenceRuntimeConfig,
) -> Result<ContractFingerprint, JournalError> {
    contract_fingerprint_under(runtime_config, SIGNAL_STREAM_MESSAGE_SCHEMA)
}

/// True when `recorded` matches the fingerprint recomputed under any accepted
/// stream-schema revision (see [`ACCEPTED_STREAM_SCHEMAS`]). All other inputs are
/// recomputed from `runtime_config`, so this accepts only a differing
/// stream-schema revision, never any other contract drift.
pub fn fingerprint_matches_accepted_schema(
    runtime_config: &SpotReferenceRuntimeConfig,
    recorded: &ContractFingerprint,
) -> Result<bool, JournalError> {
    for stream_schema in ACCEPTED_STREAM_SCHEMAS {
        if &contract_fingerprint_under(runtime_config, stream_schema)? == recorded {
            return Ok(true);
        }
    }
    Ok(false)
}

fn contract_fingerprint_under(
    runtime_config: &SpotReferenceRuntimeConfig,
    stream_schema: &str,
) -> Result<ContractFingerprint, JournalError> {
    runtime_config
        .validate()
        .map_err(|error| JournalError::Invalid(error.to_string()))?;
    let runtime_bytes = serde_json::to_vec(runtime_config)?;
    let documents = spot_signal_contract_documents();
    let mut signal_definition_sha256 = BTreeMap::new();
    let mut source_definition_sha256 = BTreeMap::new();
    for (id, raw) in &documents {
        // Spot signal identities are `btc.<spot venue>.<measurement>` — the
        // blended `external` layer plus each single-venue reading (ADR 0024).
        // Everything else this pipeline embeds is a source contract, whose
        // identity never carries the `btc.` domain prefix.
        let target = if is_spot_signal_id(id) {
            &mut signal_definition_sha256
        } else {
            &mut source_definition_sha256
        };
        if target
            .insert((*id).to_owned(), sha256_hex(raw.as_bytes()))
            .is_some()
        {
            return Err(JournalError::Invalid(format!(
                "duplicate contract document identity {id}"
            )));
        }
    }
    let runtime_config_sha256 = sha256_hex(&runtime_bytes);
    let runtime_schema_sha256 = BTreeMap::from([
        (
            "instance-status".to_owned(),
            sha256_hex(INSTANCE_STATUS_SCHEMA.as_bytes()),
        ),
        (
            "signal-stream-message".to_owned(),
            sha256_hex(stream_schema.as_bytes()),
        ),
        (
            "signal-update".to_owned(),
            sha256_hex(SIGNAL_UPDATE_SCHEMA.as_bytes()),
        ),
    ]);

    let mut aggregate = Sha256::new();
    aggregate.update(FINGERPRINT_DOMAIN);
    for (id, raw) in &documents {
        hash_component(&mut aggregate, id, raw.as_bytes())?;
    }
    hash_component(
        &mut aggregate,
        "instance-status",
        INSTANCE_STATUS_SCHEMA.as_bytes(),
    )?;
    hash_component(
        &mut aggregate,
        "signal-stream-message",
        stream_schema.as_bytes(),
    )?;
    hash_component(
        &mut aggregate,
        "signal-update",
        SIGNAL_UPDATE_SCHEMA.as_bytes(),
    )?;
    hash_component(&mut aggregate, "runtime_config", &runtime_bytes)?;

    Ok(ContractFingerprint {
        aggregate_sha256: hex_lower(&aggregate.finalize()),
        signal_definition_sha256,
        source_definition_sha256,
        runtime_schema_sha256,
        runtime_config_sha256,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JournalSession {
    pub journal_schema_version: u16,
    pub session_id: String,
    pub wall_start: String,
    pub venues: Vec<Venue>,
    pub publisher_epoch: String,
    pub contract_fingerprint: ContractFingerprint,
    pub runtime_config: SpotReferenceRuntimeConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_checkpoint: Option<CheckpointDocument>,
    /// Whether this generation was opened by in-process rotation (C5). A cold
    /// replay of a rotation-opened generation honestly re-warms every ring, so
    /// the replay's warm-vs-cold count difference is a documented re-warm gap
    /// rather than a corruption. Defaults false; omitted from non-rotation
    /// journals so they stay byte-unchanged.
    #[serde(default, skip_serializing_if = "is_false")]
    pub opened_by_rotation: bool,
}

/// serde `skip_serializing_if` helper: omit a `bool` field when it is false.
fn is_false(value: &bool) -> bool {
    !value
}

impl JournalSession {
    pub fn new(
        wall_start: String,
        publisher_epoch: String,
        runtime_config: SpotReferenceRuntimeConfig,
    ) -> Result<Self, JournalError> {
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random).map_err(JournalError::Entropy)?;
        let contract_fingerprint = contract_fingerprint(&runtime_config)?;
        let session = Self {
            journal_schema_version: JOURNAL_SCHEMA_VERSION,
            session_id: hex_lower(&random),
            wall_start,
            venues: vec![Venue::Coinbase, Venue::Kraken],
            publisher_epoch,
            contract_fingerprint,
            runtime_config,
            initial_checkpoint: None,
            opened_by_rotation: false,
        };
        session.validate()?;
        Ok(session)
    }

    /// Resume under the current `runtime_config`. The resumed session runs a new
    /// epoch under the current contract, so its fingerprint is recomputed under
    /// the current configuration rather than inherited from the checkpoint. The
    /// current config must match the checkpoint's apart from the additive
    /// `feed_health_stream` capability; resuming a pre-capability checkpoint under
    /// a declaring binary therefore upgrades emission going forward, while the
    /// old journal still replays gated by its own recorded config.
    pub fn resumed(
        publisher_epoch: String,
        initial_checkpoint: CheckpointDocument,
        runtime_config: SpotReferenceRuntimeConfig,
    ) -> Result<Self, JournalError> {
        initial_checkpoint
            .validate()
            .map_err(|error| JournalError::Invalid(error.to_string()))?;
        if !initial_checkpoint
            .runtime_config
            .matches_ignoring_feed_health(&runtime_config)
        {
            return Err(JournalError::Invalid(
                "resume runtime configuration differs from the checkpoint beyond the feed-health capability".to_owned(),
            ));
        }
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random).map_err(JournalError::Entropy)?;
        let contract_fingerprint = contract_fingerprint(&runtime_config)?;
        let session = Self {
            journal_schema_version: JOURNAL_SCHEMA_VERSION,
            session_id: hex_lower(&random),
            wall_start: initial_checkpoint.wall_start.clone(),
            venues: vec![Venue::Coinbase, Venue::Kraken],
            publisher_epoch,
            contract_fingerprint,
            runtime_config,
            initial_checkpoint: Some(initial_checkpoint),
            opened_by_rotation: false,
        };
        session.validate()?;
        Ok(session)
    }

    fn validate(&self) -> Result<(), JournalError> {
        if self.journal_schema_version != JOURNAL_SCHEMA_VERSION
            && self.journal_schema_version != RETAINED_JOURNAL_SCHEMA_VERSION
        {
            return Err(JournalError::UnsupportedVersion(
                self.journal_schema_version,
            ));
        }
        if self.session_id.len() != 32 || !is_lower_hex(&self.session_id) {
            return Err(JournalError::Invalid(
                "session_id must be 128 bits encoded as 32 lowercase hexadecimal characters"
                    .to_owned(),
            ));
        }
        DateTime::parse_from_rfc3339(&self.wall_start).map_err(|error| {
            JournalError::Invalid(format!("wall_start must be RFC 3339: {error}"))
        })?;
        if self.venues != [Venue::Coinbase, Venue::Kraken] {
            return Err(JournalError::Invalid(
                "journal venue set must be exactly coinbase and kraken".to_owned(),
            ));
        }
        if self.publisher_epoch.trim().is_empty() || self.publisher_epoch.len() > 128 {
            return Err(JournalError::Invalid(
                "publisher_epoch must contain 1 to 128 bytes".to_owned(),
            ));
        }
        self.runtime_config
            .validate()
            .map_err(|error| JournalError::Invalid(error.to_string()))?;
        self.contract_fingerprint.validate()?;
        if !fingerprint_matches_accepted_schema(&self.runtime_config, &self.contract_fingerprint)? {
            let expected = contract_fingerprint(&self.runtime_config)?;
            return Err(JournalError::FingerprintMismatch {
                expected: expected.aggregate_sha256,
                actual: self.contract_fingerprint.aggregate_sha256.clone(),
            });
        }
        if let Some(checkpoint) = &self.initial_checkpoint {
            checkpoint
                .validate()
                .map_err(|error| JournalError::Invalid(error.to_string()))?;
            // The embedded checkpoint must describe the same contract apart from
            // the additive feed-health capability. The checkpoint's fingerprint is
            // validated against its own config above (via the accepted-schema
            // lineage), and the session's against the session config, so a direct
            // fingerprint equality would forbid the legitimate capability upgrade
            // without adding integrity beyond the config match.
            if checkpoint.wall_start != self.wall_start
                || !checkpoint
                    .runtime_config
                    .matches_ignoring_feed_health(&self.runtime_config)
            {
                return Err(JournalError::Invalid(
                    "initial checkpoint does not match the journal time or contract".to_owned(),
                ));
            }
            if checkpoint
                .processor_state
                .reference
                .publisher_epoch_lineage
                .iter()
                .any(|prior| prior == &self.publisher_epoch)
            {
                return Err(JournalError::Invalid(
                    "resumed journal publisher_epoch already appears in checkpoint lineage"
                        .to_owned(),
                ));
            }
        }
        Ok(())
    }

    /// Validate a session that is about to be written. New journals are written
    /// only at the current schema; the read path accepts retained schema 4, but a
    /// writer must never stamp it, since a schema-4-labelled but CRC-framed line
    /// could never replay. Guards against a caller that mutated the public
    /// `journal_schema_version` field before creating a writer.
    fn validate_writable(&self) -> Result<(), JournalError> {
        if self.journal_schema_version != JOURNAL_SCHEMA_VERSION {
            return Err(JournalError::Invalid(format!(
                "journals are written only at schema {JOURNAL_SCHEMA_VERSION}; \
                 refusing to write schema {}",
                self.journal_schema_version
            )));
        }
        self.validate()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "input_type", rename_all = "snake_case", deny_unknown_fields)]
pub enum JournalInput {
    MarketEvents {
        admission_seq: u64,
        events: Arc<Vec<SourceEvent>>,
        logical_receipt_ns: u64,
        now_ns: u64,
        /// Whether this batch is a C5 rotation resync (a book re-snapshot the
        /// runtime admits as the first records of a rotation-opened generation),
        /// as opposed to a received venue frame. Record-level provenance so a
        /// journal reader never mistakes the synthesized snapshot for a live
        /// feed frame. Additive: absent ⇒ false, and a non-resync batch omits
        /// it so existing journals stay byte-identical.
        #[serde(default, skip_serializing_if = "is_false")]
        resync: bool,
    },
    Availability {
        admission_seq: u64,
        venue: Venue,
        reason: UnavailableReason,
        now_ns: u64,
    },
    HealthTick {
        admission_seq: u64,
        now_ns: u64,
    },
}

impl JournalInput {
    pub fn admission_seq(&self) -> u64 {
        match self {
            Self::MarketEvents { admission_seq, .. }
            | Self::Availability { admission_seq, .. }
            | Self::HealthTick { admission_seq, .. } => *admission_seq,
        }
    }

    pub fn now_ns(&self) -> u64 {
        match self {
            Self::MarketEvents { now_ns, .. }
            | Self::Availability { now_ns, .. }
            | Self::HealthTick { now_ns, .. } => *now_ns,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JournalTotals {
    pub admitted_inputs: u64,
    pub market_events: u64,
    pub availability_events: u64,
    pub health_ticks: u64,
    pub updates_written: u64,
    pub stream_messages_written: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawCaptureReference {
    pub path: String,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JournalEnd {
    pub totals: JournalTotals,
    pub quality_reason_entries: BTreeMap<String, u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub raw_capture_references: Vec<RawCaptureReference>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "record_type", rename_all = "snake_case", deny_unknown_fields)]
enum JournalRecord {
    Session {
        #[serde(flatten)]
        session: Box<JournalSession>,
    },
    MarketEvents {
        admission_seq: u64,
        events: Arc<Vec<SourceEvent>>,
        logical_receipt_ns: u64,
        now_ns: u64,
        /// See [`JournalInput::MarketEvents::resync`]. Additive, byte-safe.
        #[serde(default, skip_serializing_if = "is_false")]
        resync: bool,
    },
    Availability {
        admission_seq: u64,
        venue: Venue,
        reason: UnavailableReason,
        now_ns: u64,
    },
    HealthTick {
        admission_seq: u64,
        now_ns: u64,
    },
    End {
        #[serde(flatten)]
        end: JournalEnd,
    },
}

#[derive(Deserialize)]
struct JournalRecordTypeEnvelope {
    record_type: String,
}

#[derive(Deserialize)]
struct JournalVersionEnvelope {
    journal_schema_version: u16,
}

impl From<JournalInput> for JournalRecord {
    fn from(input: JournalInput) -> Self {
        match input {
            JournalInput::MarketEvents {
                admission_seq,
                events,
                logical_receipt_ns,
                now_ns,
                resync,
            } => Self::MarketEvents {
                admission_seq,
                events,
                logical_receipt_ns,
                now_ns,
                resync,
            },
            JournalInput::Availability {
                admission_seq,
                venue,
                reason,
                now_ns,
            } => Self::Availability {
                admission_seq,
                venue,
                reason,
                now_ns,
            },
            JournalInput::HealthTick {
                admission_seq,
                now_ns,
            } => Self::HealthTick {
                admission_seq,
                now_ns,
            },
        }
    }
}

type WriterMessage = crate::journal_core::WriterMessage<JournalInput, JournalEnd>;

struct SpotRecords;

impl crate::journal_core::JournalRecords for SpotRecords {
    type Input = JournalInput;
    type End = JournalEnd;
    type Record = JournalRecord;

    fn input_record(input: JournalInput) -> JournalRecord {
        JournalRecord::from(input)
    }

    fn end_record(end: JournalEnd) -> JournalRecord {
        JournalRecord::End { end }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct JournalWriteReport {
    pub records_written: u64,
    pub bytes_written: u64,
    pub data_syncs: u64,
}

pub struct SignalJournalWriter {
    sender: Option<SyncSender<WriterMessage>>,
    worker: Option<JoinHandle<Result<JournalWriteReport, JournalError>>>,
}

impl SignalJournalWriter {
    pub fn create(
        path: &Path,
        session: JournalSession,
        maximum_bytes: u64,
        queue_capacity: usize,
    ) -> Result<Self, JournalError> {
        // Production fixes the watermark at the durability constant; the
        // `_with_minimum_free_space` seam exists only so a boundary test can
        // raise the threshold above a real filesystem's actual free space.
        Self::create_with_minimum_free_space(
            path,
            session,
            maximum_bytes,
            queue_capacity,
            crate::journal_core::MINIMUM_FREE_SPACE_BYTES,
        )
    }

    fn create_with_minimum_free_space(
        path: &Path,
        session: JournalSession,
        maximum_bytes: u64,
        queue_capacity: usize,
        minimum_free_space_bytes: u64,
    ) -> Result<Self, JournalError> {
        if maximum_bytes == 0 {
            return Err(JournalError::Invalid(
                "journal maximum_bytes must be greater than zero".to_owned(),
            ));
        }
        if queue_capacity == 0 {
            return Err(JournalError::Invalid(
                "journal queue capacity must be greater than zero".to_owned(),
            ));
        }
        session.validate_writable()?;

        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            options.mode(0o600);
        }
        let file = options.open(path).map_err(JournalError::Io)?;
        let mut writer = BufWriter::with_capacity(JOURNAL_BUFFER_BYTES, file);
        let watermark = crate::journal_core::Watermark::new(
            path.to_path_buf(),
            minimum_free_space_bytes,
            crate::journal_core::statvfs_available_bytes,
        );
        let mut core =
            crate::journal_core::WriterCore::with_watermark(maximum_bytes, Some(watermark));
        // Fail closed at open when free space is already below the watermark.
        core.check_watermark()?;
        core.write_record(
            &mut writer,
            &JournalRecord::Session {
                session: Box::new(session.clone()),
            },
        )?;
        core.sync(&mut writer)?;

        let (sender, receiver) = mpsc::sync_channel(queue_capacity);
        let worker = thread::Builder::new()
            .name("btc-signal-journal".to_owned())
            .spawn(move || -> Result<JournalWriteReport, JournalError> {
                let core = crate::journal_core::writer_loop::<SpotRecords>(writer, core, receiver)?;
                Ok(JournalWriteReport {
                    records_written: core.records_written,
                    bytes_written: core.bytes_written,
                    data_syncs: core.data_syncs,
                })
            })
            .map_err(JournalError::Io)?;
        Ok(Self {
            sender: Some(sender),
            worker: Some(worker),
        })
    }

    /// Enqueue one immutable input without waiting for disk I/O.
    pub fn enqueue(&self, input: JournalInput) -> Result<(), JournalError> {
        let sender = self.sender.as_ref().ok_or(JournalError::WriterStopped)?;
        try_enqueue(sender, WriterMessage::Input(input))
    }

    /// Append the terminal record, drain the queue, sync, and join the writer.
    pub fn finish(mut self, end: JournalEnd) -> Result<JournalWriteReport, JournalError> {
        let send_result = self
            .sender
            .take()
            .ok_or(JournalError::WriterStopped)?
            .send(WriterMessage::End(end));
        let joined = self.join_worker();
        match joined {
            Err(error) => Err(error),
            Ok(report) => {
                send_result.map_err(|_| JournalError::WriterStopped)?;
                Ok(report)
            }
        }
    }

    /// Drain and sync the recoverable prefix without writing an end record.
    pub fn abort(mut self) -> Result<JournalWriteReport, JournalError> {
        self.sender.take();
        self.join_worker()
    }

    fn join_worker(&mut self) -> Result<JournalWriteReport, JournalError> {
        let worker = self.worker.take().ok_or(JournalError::WriterStopped)?;
        worker.join().map_err(|_| JournalError::WriterPanicked)?
    }
}

fn try_enqueue(
    sender: &SyncSender<WriterMessage>,
    message: WriterMessage,
) -> Result<(), JournalError> {
    crate::journal_core::try_enqueue(sender, message).map_err(JournalError::from)
}

pub struct JournalReplayReader {
    reader: BufReader<File>,
    frame_kind: crate::journal_core::FrameKind,
    line_number: u64,
    session: JournalSession,
    expected_admission_seq: u64,
    last_now_ns: Option<u64>,
    last_receipt_ns: BTreeMap<Venue, u64>,
    observed_market_events: u64,
    observed_availability_events: u64,
    observed_health_ticks: u64,
    end: Option<JournalEnd>,
}

impl JournalReplayReader {
    pub fn open(path: &Path, maximum_bytes: u64) -> Result<Self, JournalError> {
        if maximum_bytes == 0 {
            return Err(JournalError::Invalid(
                "replay maximum_bytes must be greater than zero".to_owned(),
            ));
        }
        let file = File::open(path).map_err(JournalError::Io)?;
        let size = file.metadata().map_err(JournalError::Io)?.len();
        if size > maximum_bytes {
            return Err(JournalError::SizeLimit {
                attempted: size,
                maximum: maximum_bytes,
            });
        }
        let mut reader = BufReader::with_capacity(JOURNAL_BUFFER_BYTES, file);
        let (first_line, had_newline) =
            read_raw_line(&mut reader)?.ok_or(JournalError::MissingSession)?;
        // The first line's framing decides the whole journal: a schema-5 journal
        // envelopes every line, retained schema-4 evidence does not.
        let is_final = !had_newline
            || !crate::journal_core::more_bytes_follow(&mut reader).map_err(JournalError::Io)?;
        let (frame_kind, record_bytes): (crate::journal_core::FrameKind, &[u8]) =
            match crate::journal_core::classify_first_frame(&first_line, is_final) {
                Ok(crate::journal_core::FirstFrame::EnvelopedV5(bytes)) => {
                    (crate::journal_core::FrameKind::EnvelopedV5, bytes)
                }
                Ok(crate::journal_core::FirstFrame::BareV4(bytes)) => {
                    (crate::journal_core::FrameKind::BareV4, bytes)
                }
                Err(crate::journal_core::FramingError::TornTail) => {
                    return Err(JournalError::TornTail);
                }
                Err(crate::journal_core::FramingError::Corruption { line }) => {
                    return Err(JournalError::Corruption { line });
                }
            };
        // A missing terminal newline means the file ended mid-line. For schema 5
        // that is a torn tail even when the record parses and its CRC matches; for
        // retained schema 4 it is the legacy incomplete-prefix condition, reported
        // exactly as the pre-framing reader did.
        match frame_kind {
            crate::journal_core::FrameKind::EnvelopedV5 if !had_newline => {
                return Err(JournalError::TornTail);
            }
            crate::journal_core::FrameKind::BareV4 if !had_newline => {
                return Err(JournalError::Incomplete);
            }
            _ => {}
        }
        let record_type: JournalRecordTypeEnvelope = serde_json::from_slice(record_bytes)?;
        if record_type.record_type != "session" {
            return Err(JournalError::MissingSession);
        }
        // Preflight the bounded top-level version before strict decoding. Older
        // sessions do not contain fields required by the current typed shape.
        // The framing pins the expected version: enveloped lines are schema 5,
        // bare lines are retained schema 4.
        let version: JournalVersionEnvelope = serde_json::from_slice(record_bytes)?;
        let expected_version = match frame_kind {
            crate::journal_core::FrameKind::EnvelopedV5 => JOURNAL_SCHEMA_VERSION,
            crate::journal_core::FrameKind::BareV4 => RETAINED_JOURNAL_SCHEMA_VERSION,
        };
        if version.journal_schema_version != expected_version {
            return Err(JournalError::UnsupportedVersion(
                version.journal_schema_version,
            ));
        }
        let first: JournalRecord = serde_json::from_slice(record_bytes)?;
        let JournalRecord::Session { session } = first else {
            return Err(JournalError::MissingSession);
        };
        let session = *session;
        session.validate()?;
        Ok(Self {
            reader,
            frame_kind,
            line_number: 1,
            last_now_ns: session
                .initial_checkpoint
                .as_ref()
                .map(|checkpoint| checkpoint.checkpoint_time_ns),
            session,
            expected_admission_seq: 0,
            last_receipt_ns: BTreeMap::new(),
            observed_market_events: 0,
            observed_availability_events: 0,
            observed_health_ticks: 0,
            end: None,
        })
    }

    pub fn session(&self) -> &JournalSession {
        &self.session
    }

    pub fn next_input(&mut self) -> Result<Option<JournalInput>, JournalError> {
        if self.end.is_some() {
            return Ok(None);
        }
        let record = self.next_record()?.ok_or(JournalError::Incomplete)?;
        match record {
            JournalRecord::Session { .. } => Err(JournalError::Invalid(
                "session record may appear only once at the beginning".to_owned(),
            )),
            JournalRecord::MarketEvents {
                admission_seq,
                events,
                logical_receipt_ns,
                now_ns,
                resync,
            } => {
                let input = JournalInput::MarketEvents {
                    admission_seq,
                    events,
                    logical_receipt_ns,
                    now_ns,
                    resync,
                };
                self.validate_input(&input)?;
                Ok(Some(input))
            }
            JournalRecord::Availability {
                admission_seq,
                venue,
                reason,
                now_ns,
            } => {
                let input = JournalInput::Availability {
                    admission_seq,
                    venue,
                    reason,
                    now_ns,
                };
                self.validate_input(&input)?;
                Ok(Some(input))
            }
            JournalRecord::HealthTick {
                admission_seq,
                now_ns,
            } => {
                let input = JournalInput::HealthTick {
                    admission_seq,
                    now_ns,
                };
                self.validate_input(&input)?;
                Ok(Some(input))
            }
            JournalRecord::End { end } => {
                self.validate_end(&end)?;
                if self.next_record()?.is_some() {
                    return Err(JournalError::TrailingRecord);
                }
                self.end = Some(end);
                Ok(None)
            }
        }
    }

    pub fn end(&self) -> Result<&JournalEnd, JournalError> {
        self.end.as_ref().ok_or(JournalError::Incomplete)
    }

    /// Read and frame-decode the next record, applying the journal's framing
    /// mode. A clean end of file returns `None`. A schema-5 line that fails to
    /// parse or whose CRC does not match is a torn tail (final line) or
    /// corruption (interior line); a truncated bare schema-4 line is an
    /// incomplete recoverable prefix, unchanged from before framing existed.
    fn next_record(&mut self) -> Result<Option<JournalRecord>, JournalError> {
        let Some((line, had_newline)) = read_raw_line(&mut self.reader)? else {
            return Ok(None);
        };
        self.line_number = self
            .line_number
            .checked_add(1)
            .ok_or(JournalError::Counter)?;
        let record_bytes: &[u8] = match self.frame_kind {
            crate::journal_core::FrameKind::BareV4 => {
                if !had_newline {
                    return Err(JournalError::Incomplete);
                }
                &line
            }
            crate::journal_core::FrameKind::EnvelopedV5 => {
                // A schema-5 line without a terminating newline is a torn tail even
                // if its bytes happen to parse and verify: the write did not
                // complete. This mirrors the bare-schema-4 truncation guard above.
                if !had_newline {
                    return Err(JournalError::TornTail);
                }
                let is_final = !crate::journal_core::more_bytes_follow(&mut self.reader)
                    .map_err(JournalError::Io)?;
                match crate::journal_core::enveloped_record_bytes(&line, is_final, self.line_number)
                {
                    Ok(bytes) => bytes,
                    Err(crate::journal_core::FramingError::TornTail) => {
                        return Err(JournalError::TornTail);
                    }
                    Err(crate::journal_core::FramingError::Corruption { line }) => {
                        return Err(JournalError::Corruption { line });
                    }
                }
            }
        };
        Ok(Some(serde_json::from_slice(record_bytes)?))
    }

    fn validate_input(&mut self, input: &JournalInput) -> Result<(), JournalError> {
        if input.admission_seq() != self.expected_admission_seq {
            return Err(JournalError::AdmissionSequence {
                expected: self.expected_admission_seq,
                actual: input.admission_seq(),
            });
        }
        if let Some(previous) = self.last_now_ns
            && input.now_ns() < previous
        {
            return Err(JournalError::NowRegression {
                previous,
                actual: input.now_ns(),
            });
        }

        match input {
            JournalInput::MarketEvents {
                events,
                logical_receipt_ns,
                now_ns,
                ..
            } => {
                if events.is_empty() {
                    return Err(JournalError::Invalid(
                        "journal market-event batch cannot be empty".to_owned(),
                    ));
                }
                if logical_receipt_ns > now_ns {
                    return Err(JournalError::ReceiptAfterAdmission {
                        receipt: *logical_receipt_ns,
                        now: *now_ns,
                    });
                }
                let venue = venue_for_event(&events[0])?;
                for event in events.iter().skip(1) {
                    if venue_for_event(event)? != venue {
                        return Err(JournalError::Invalid(
                            "one journal market-event batch cannot mix venues".to_owned(),
                        ));
                    }
                }
                if let Some(previous) = self.last_receipt_ns.get(&venue)
                    && logical_receipt_ns < previous
                {
                    return Err(JournalError::ReceiptRegression {
                        venue,
                        previous: *previous,
                        actual: *logical_receipt_ns,
                    });
                }
                self.last_receipt_ns.insert(venue, *logical_receipt_ns);
                self.observed_market_events = checked_add(
                    self.observed_market_events,
                    u64::try_from(events.len()).map_err(|_| JournalError::Counter)?,
                )?;
            }
            JournalInput::Availability { .. } => {
                self.observed_availability_events =
                    checked_add(self.observed_availability_events, 1)?;
            }
            JournalInput::HealthTick { .. } => {
                self.observed_health_ticks = checked_add(self.observed_health_ticks, 1)?;
            }
        }
        self.last_now_ns = Some(input.now_ns());
        self.expected_admission_seq = checked_add(self.expected_admission_seq, 1)?;
        Ok(())
    }

    fn validate_end(&self, end: &JournalEnd) -> Result<(), JournalError> {
        let observed = JournalTotals {
            admitted_inputs: self.expected_admission_seq,
            market_events: self.observed_market_events,
            availability_events: self.observed_availability_events,
            health_ticks: self.observed_health_ticks,
            updates_written: end.totals.updates_written,
            stream_messages_written: end.totals.stream_messages_written,
        };
        if end.totals.admitted_inputs != observed.admitted_inputs
            || end.totals.market_events != observed.market_events
            || end.totals.availability_events != observed.availability_events
            || end.totals.health_ticks != observed.health_ticks
        {
            return Err(JournalError::TotalsMismatch {
                expected: end.totals.clone(),
                observed,
            });
        }
        for reference in &end.raw_capture_references {
            if reference.path.trim().is_empty() {
                return Err(JournalError::Invalid(
                    "raw capture reference path cannot be empty".to_owned(),
                ));
            }
            validate_digest("raw capture reference sha256", &reference.sha256)?;
        }
        Ok(())
    }
}

/// Read one newline-delimited line, returning the bytes without the trailing
/// newline and whether a terminating newline was present. A missing newline
/// (`false`) means the file ended mid-line; framing decides whether that is a
/// torn tail (schema 5) or an incomplete prefix (schema 4). Bounds and the
/// empty-record rejection are unchanged from before framing.
fn read_raw_line(reader: &mut BufReader<File>) -> Result<Option<(Vec<u8>, bool)>, JournalError> {
    let mut line = Vec::with_capacity(4096);
    let read = reader
        .by_ref()
        .take(u64::try_from(MAX_JOURNAL_LINE_BYTES + 1).map_err(|_| JournalError::Counter)?)
        .read_until(b'\n', &mut line)
        .map_err(JournalError::Io)?;
    if read == 0 {
        return Ok(None);
    }
    if line.len() > MAX_JOURNAL_LINE_BYTES
        || (line.len() == MAX_JOURNAL_LINE_BYTES && line.last() != Some(&b'\n'))
    {
        return Err(JournalError::LineTooLarge {
            maximum: MAX_JOURNAL_LINE_BYTES,
        });
    }
    let had_newline = line.last() == Some(&b'\n');
    if had_newline {
        line.pop();
    }
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    if line.is_empty() {
        return Err(JournalError::Invalid(
            "empty journal records are not allowed".to_owned(),
        ));
    }
    Ok(Some((line, had_newline)))
}

fn venue_for_event(event: &SourceEvent) -> Result<Venue, JournalError> {
    if event.source_id.starts_with("coinbase.exchange.") {
        Ok(Venue::Coinbase)
    } else if event.source_id.starts_with("kraken.spot.") {
        Ok(Venue::Kraken)
    } else {
        Err(JournalError::Invalid(format!(
            "journal market event has unsupported source_id {}",
            event.source_id
        )))
    }
}

fn hash_component(hasher: &mut Sha256, label: &str, bytes: &[u8]) -> Result<(), JournalError> {
    let label_length = u64::try_from(label.len()).map_err(|_| JournalError::Counter)?;
    let byte_length = u64::try_from(bytes.len()).map_err(|_| JournalError::Counter)?;
    hasher.update(label_length.to_be_bytes());
    hasher.update(label.as_bytes());
    hasher.update(byte_length.to_be_bytes());
    hasher.update(bytes);
    Ok(())
}

fn validate_digest(name: &str, value: &str) -> Result<(), JournalError> {
    if value.len() == 64 && is_lower_hex(value) {
        Ok(())
    } else {
        Err(JournalError::Invalid(format!(
            "{name} must be a lowercase SHA-256 digest"
        )))
    }
}

fn checked_add(value: u64, increment: u64) -> Result<u64, JournalError> {
    value.checked_add(increment).ok_or(JournalError::Counter)
}

#[derive(Debug, Error)]
pub enum JournalError {
    #[error("journal I/O failed: {0}")]
    Io(std::io::Error),
    #[error("journal JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("operating-system entropy failed: {0}")]
    Entropy(getrandom::Error),
    #[error("unsupported journal schema version {0}")]
    UnsupportedVersion(u16),
    #[error("invalid journal: {0}")]
    Invalid(String),
    #[error("journal does not begin with a session record")]
    MissingSession,
    #[error("journal has no durable terminal record; only its prefix is recoverable")]
    Incomplete,
    #[error("journal contains data after its terminal record")]
    TrailingRecord,
    #[error("journal contract fingerprint mismatch: expected {expected}, actual {actual}")]
    FingerprintMismatch { expected: String, actual: String },
    #[error("journal admission sequence mismatch: expected {expected}, actual {actual}")]
    AdmissionSequence { expected: u64, actual: u64 },
    #[error("journal logical time regressed from {previous}ns to {actual}ns")]
    NowRegression { previous: u64, actual: u64 },
    #[error("journal receipt {receipt}ns exceeds admission time {now}ns")]
    ReceiptAfterAdmission { receipt: u64, now: u64 },
    #[error("journal {venue:?} receipt regressed from {previous}ns to {actual}ns")]
    ReceiptRegression {
        venue: Venue,
        previous: u64,
        actual: u64,
    },
    #[error("journal totals mismatch: terminal {expected:?}, observed {observed:?}")]
    TotalsMismatch {
        expected: JournalTotals,
        observed: JournalTotals,
    },
    #[error("journal queue is full; accepted inputs cannot be dropped")]
    QueueFull,
    #[error("journal writer stopped before accepting the input")]
    WriterStopped,
    #[error("journal writer thread panicked")]
    WriterPanicked,
    #[error("journal record exceeds {maximum} bytes")]
    LineTooLarge { maximum: usize },
    #[error("journal would reach {attempted} bytes; maximum is {maximum}")]
    SizeLimit { attempted: u64, maximum: u64 },
    #[error(
        "journal free space {available} bytes is below the {minimum}-byte watermark; run stopped"
    )]
    LowDiskSpace { available: u64, minimum: u64 },
    #[error("journal tail is torn: the final record failed to parse or its CRC did not match")]
    TornTail,
    #[error(
        "journal record on line {line} is corrupt: it failed to parse or its CRC did not match"
    )]
    Corruption { line: u64 },
    #[error("journal counter exhausted")]
    Counter,
}

impl From<crate::journal_core::JournalCoreError> for JournalError {
    fn from(error: crate::journal_core::JournalCoreError) -> Self {
        use crate::journal_core::JournalCoreError;
        match error {
            JournalCoreError::Io(error) => JournalError::Io(error),
            JournalCoreError::Json(error) => JournalError::Json(error),
            JournalCoreError::LineTooLarge { maximum } => JournalError::LineTooLarge { maximum },
            JournalCoreError::SizeLimit { attempted, maximum } => {
                JournalError::SizeLimit { attempted, maximum }
            }
            JournalCoreError::LowDiskSpace { available, minimum } => {
                JournalError::LowDiskSpace { available, minimum }
            }
            JournalCoreError::Counter => JournalError::Counter,
            JournalCoreError::QueueFull => JournalError::QueueFull,
            JournalCoreError::WriterStopped => JournalError::WriterStopped,
            JournalCoreError::InputAfterTerminal => {
                JournalError::Invalid("journal input followed the terminal record".to_owned())
            }
            JournalCoreError::MultipleTerminal => {
                JournalError::Invalid("journal contains more than one terminal record".to_owned())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session_with_fingerprint(fingerprint: ContractFingerprint) -> JournalSession {
        let mut config = SpotReferenceRuntimeConfig::new(100).expect("runtime config builds");
        config.feed_health_stream = false;
        JournalSession {
            journal_schema_version: JOURNAL_SCHEMA_VERSION,
            session_id: "0".repeat(32),
            wall_start: "2026-08-13T12:00:00Z".to_owned(),
            venues: vec![Venue::Coinbase, Venue::Kraken],
            publisher_epoch: "lineage".to_owned(),
            contract_fingerprint: fingerprint,
            runtime_config: config,
            initial_checkpoint: None,
            opened_by_rotation: false,
        }
    }

    #[test]
    fn v2_runtime_schema_map_is_refused() {
        let mut config = SpotReferenceRuntimeConfig::new(100).expect("runtime config builds");
        config.feed_health_stream = false;
        let mut v2 = contract_fingerprint(&config).expect("v3 fingerprint builds");
        assert!(v2.runtime_schema_sha256.remove("instance-status").is_some());
        let session = session_with_fingerprint(v2);
        assert!(
            matches!(session.validate(), Err(JournalError::Invalid(message)) if message.contains("complete runtime-schema set"))
        );
    }

    #[test]
    fn standalone_instance_status_schema_mutation_refuses_restore() {
        let mut config = SpotReferenceRuntimeConfig::new(100).expect("runtime config builds");
        config.feed_health_stream = false;
        let mut tampered = contract_fingerprint(&config).expect("v3 fingerprint builds");
        tampered
            .runtime_schema_sha256
            .insert("instance-status".to_owned(), sha256_hex(b"mutated schema"));
        let session = session_with_fingerprint(tampered);
        assert!(matches!(
            session.validate(),
            Err(JournalError::FingerprintMismatch { .. })
        ));

        // A fingerprint computed under a schema that is not in the lineage is also
        // refused, so drift beyond the reviewed revisions still fails closed.
        let unlisted = contract_fingerprint_under(&config, "{\"unlisted\":true}")
            .expect("fingerprint recomputes");
        assert!(
            !fingerprint_matches_accepted_schema(&config, &unlisted).expect("lineage check runs")
        );
    }

    #[test]
    fn resume_upgrades_capability_and_keeps_the_embedded_checkpoint_gated() {
        use chrono::Utc;

        use crate::SpotSignalProcessor;

        let wall_start = DateTime::parse_from_rfc3339("2026-08-13T12:00:00Z")
            .expect("wall start parses")
            .with_timezone(&Utc);
        let mut disabled = SpotReferenceRuntimeConfig::new(100).expect("config builds");
        disabled.feed_health_stream = false;
        let processor = SpotSignalProcessor::new("e1", wall_start).expect("processor builds");
        let checkpoint = CheckpointDocument::new(
            "0".repeat(32),
            "2026-08-13T12:00:00Z".to_owned(),
            100,
            5,
            contract_fingerprint(&disabled).expect("fingerprint builds"),
            disabled.clone(),
            processor.checkpoint_state(),
        )
        .expect("pre-capability checkpoint builds");

        let current = SpotReferenceRuntimeConfig::new(100).expect("current config builds");
        assert!(current.feed_health_stream);
        let session = JournalSession::resumed("e2".to_owned(), checkpoint, current)
            .expect("resume across the capability boundary succeeds");
        // The resumed session upgrades: it declares the capability and emits going
        // forward, while the embedded pre-capability checkpoint stays gated closed.
        assert!(session.runtime_config.feed_health_stream);
        assert!(
            !session
                .initial_checkpoint
                .expect("embedded checkpoint")
                .runtime_config
                .feed_health_stream
        );
    }

    #[test]
    fn retained_v4_journal_replays_without_framing() {
        use std::time::{SystemTime, UNIX_EPOCH};

        let runtime_config = SpotReferenceRuntimeConfig::new(100).expect("runtime config builds");
        let mut session = JournalSession::new(
            "2026-08-13T12:00:00Z".to_owned(),
            "retained-v4".to_owned(),
            runtime_config,
        )
        .expect("session builds");
        // Retained evidence predates CRC framing and carries schema 4.
        session.journal_schema_version = RETAINED_JOURNAL_SCHEMA_VERSION;

        let tick = JournalInput::HealthTick {
            admission_seq: 0,
            now_ns: 10,
        };
        let end = JournalEnd {
            totals: JournalTotals {
                admitted_inputs: 1,
                market_events: 0,
                availability_events: 0,
                health_ticks: 1,
                updates_written: 0,
                stream_messages_written: 0,
            },
            quality_reason_entries: BTreeMap::new(),
            raw_capture_references: Vec::new(),
        };

        // Serialize each record as a bare JSONL line, exactly as the pre-CRC
        // binary wrote schema 4: no envelope, no CRC.
        let mut bytes = Vec::new();
        for record in [
            JournalRecord::Session {
                session: Box::new(session.clone()),
            },
            JournalRecord::from(tick.clone()),
            JournalRecord::End { end: end.clone() },
        ] {
            serde_json::to_writer(&mut bytes, &record).expect("record serializes");
            bytes.push(b'\n');
        }

        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock follows Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "btc-signal-retained-v4-{}-{unique}.jsonl",
            std::process::id()
        ));
        std::fs::write(&path, &bytes).expect("fixture writes");

        let mut reader = JournalReplayReader::open(&path, 1_048_576).expect("v4 journal opens");
        assert_eq!(
            reader.session().journal_schema_version,
            RETAINED_JOURNAL_SCHEMA_VERSION
        );
        assert_eq!(reader.next_input().expect("tick reads"), Some(tick));
        assert!(reader.next_input().expect("terminal reads").is_none());
        assert_eq!(reader.end().expect("terminal exists"), &end);
        std::fs::remove_file(path).expect("fixture removes");
    }

    #[test]
    fn writer_refuses_to_stamp_a_retained_schema_version() {
        use std::time::{SystemTime, UNIX_EPOCH};

        let mut session = JournalSession::new(
            "2026-08-13T12:00:00Z".to_owned(),
            "reject-v4-write".to_owned(),
            SpotReferenceRuntimeConfig::new(100).expect("runtime config builds"),
        )
        .expect("session builds");
        // A caller mutates the public field to a retained schema. The read path
        // would accept schema 4, but the writer must refuse to produce it.
        session.journal_schema_version = RETAINED_JOURNAL_SCHEMA_VERSION;

        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock follows Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "btc-signal-reject-v4-{}-{unique}.jsonl",
            std::process::id()
        ));
        assert!(matches!(
            SignalJournalWriter::create(&path, session, 1_048_576, 4),
            Err(JournalError::Invalid(_))
        ));
        assert!(
            !path.exists(),
            "the writer rejects before creating the journal file"
        );
    }

    /// SIG-T012d disk-exhaustion, public writer-boundary reach. The
    /// `journal_core.rs` tests exercise the writer core; this drives the real
    /// `SignalJournalWriter` create path — file open, watermark construction, and
    /// the fail-closed `check_watermark` at open — against a real filesystem. The
    /// `_with_minimum_free_space` seam raises the threshold above the filesystem's
    /// actual free space (production fixes it at the durability constant), so the
    /// documented `LowDiskSpace` error must surface through the writer boundary.
    #[cfg(unix)]
    #[test]
    fn create_fails_closed_when_free_space_is_below_the_watermark() {
        use std::time::{SystemTime, UNIX_EPOCH};

        let session = JournalSession::new(
            "2026-08-13T12:00:00Z".to_owned(),
            "watermark-boundary".to_owned(),
            SpotReferenceRuntimeConfig::new(100).expect("runtime config builds"),
        )
        .expect("session builds");
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock follows Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "btc-signal-watermark-boundary-{}-{unique}.jsonl",
            std::process::id()
        ));
        match SignalJournalWriter::create_with_minimum_free_space(
            &path,
            session,
            1_048_576,
            4,
            u64::MAX,
        ) {
            Err(JournalError::LowDiskSpace { available, minimum }) => {
                assert_eq!(minimum, u64::MAX);
                assert!(
                    available < u64::MAX,
                    "real statvfs reports finite free space, got {available}"
                );
            }
            Err(other) => panic!("expected a LowDiskSpace refusal, got {other:?}"),
            Ok(_) => panic!("public create must fail closed below the watermark"),
        }
        // The file open precedes the watermark check, so a refused run leaves an
        // empty journal file behind; remove it.
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn bounded_queue_reports_full_and_disconnected_without_dropping() {
        let (sender, receiver) = mpsc::sync_channel(1);
        let input = || {
            WriterMessage::Input(JournalInput::HealthTick {
                admission_seq: 0,
                now_ns: 1,
            })
        };
        try_enqueue(&sender, input()).expect("first record fills the one-slot queue");
        assert!(matches!(
            try_enqueue(&sender, input()),
            Err(JournalError::QueueFull)
        ));
        drop(receiver);
        assert!(matches!(
            try_enqueue(&sender, input()),
            Err(JournalError::WriterStopped)
        ));
    }
}
