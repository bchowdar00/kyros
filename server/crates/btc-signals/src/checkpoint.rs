use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::thread::{self, JoinHandle};

use chrono::DateTime;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::digest::is_lower_hex;
use crate::journal::{
    ContractFingerprint, contract_fingerprint, fingerprint_matches_accepted_schema,
};
use crate::{
    SPOT_SIGNAL_SET_ID, SPOT_SIGNAL_SET_VERSION, SpotReferenceRuntimeConfig,
    SpotSignalCheckpointState,
};

pub const CHECKPOINT_SCHEMA_VERSION: u16 = 4;
pub const DEFAULT_CHECKPOINT_MAX_BYTES: u64 = 1_048_576;
pub const DEFAULT_CHECKPOINT_QUEUE_CAPACITY: usize = 8;

const CHECKPOINT_HASH_DOMAIN: &[u8] = b"tiny.signal.checkpoint.v4\0";

/// Bounded recovery state needed to resume the spot-signal process safely.
///
/// Identity, quality, reference, and compatibility state are durable. Large
/// rolling trade/book histories are intentionally rebuilt after recovery, so
/// their dependent signals re-enter warm-up instead of publishing stale state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointDocument {
    pub checkpoint_schema_version: u16,
    pub processor_id: String,
    pub processor_version: String,
    pub session_id: String,
    pub wall_start: String,
    pub checkpoint_time_ns: u64,
    pub next_admission_seq: u64,
    pub contract_fingerprint: ContractFingerprint,
    pub runtime_config: SpotReferenceRuntimeConfig,
    pub processor_state: SpotSignalCheckpointState,
}

impl CheckpointDocument {
    pub fn new(
        session_id: String,
        wall_start: String,
        checkpoint_time_ns: u64,
        next_admission_seq: u64,
        contract_fingerprint: ContractFingerprint,
        runtime_config: SpotReferenceRuntimeConfig,
        processor_state: SpotSignalCheckpointState,
    ) -> Result<Self, CheckpointError> {
        let document = Self {
            checkpoint_schema_version: CHECKPOINT_SCHEMA_VERSION,
            processor_id: SPOT_SIGNAL_SET_ID.to_owned(),
            processor_version: SPOT_SIGNAL_SET_VERSION.to_owned(),
            session_id,
            wall_start,
            checkpoint_time_ns,
            next_admission_seq,
            contract_fingerprint,
            runtime_config,
            processor_state,
        };
        document.validate()?;
        Ok(document)
    }

    pub fn validate(&self) -> Result<(), CheckpointError> {
        if self.checkpoint_schema_version != CHECKPOINT_SCHEMA_VERSION {
            return Err(CheckpointError::UnsupportedVersion(
                self.checkpoint_schema_version,
            ));
        }
        if self.processor_id != SPOT_SIGNAL_SET_ID
            || self.processor_version != SPOT_SIGNAL_SET_VERSION
        {
            return Err(CheckpointError::Invalid(format!(
                "checkpoint processor is {}@{}; expected {}@{}",
                self.processor_id,
                self.processor_version,
                SPOT_SIGNAL_SET_ID,
                SPOT_SIGNAL_SET_VERSION
            )));
        }
        if self.session_id.len() != 32 || !is_lower_hex(&self.session_id) {
            return Err(CheckpointError::Invalid(
                "session_id must be 128 bits encoded as 32 lowercase hexadecimal characters"
                    .to_owned(),
            ));
        }
        DateTime::parse_from_rfc3339(&self.wall_start).map_err(|error| {
            CheckpointError::Invalid(format!("wall_start must be RFC 3339: {error}"))
        })?;
        self.runtime_config
            .validate()
            .map_err(|error| CheckpointError::Invalid(error.to_string()))?;
        if !fingerprint_matches_accepted_schema(&self.runtime_config, &self.contract_fingerprint)? {
            let expected = contract_fingerprint(&self.runtime_config)?;
            return Err(CheckpointError::FingerprintMismatch {
                expected: expected.aggregate_sha256,
                actual: self.contract_fingerprint.aggregate_sha256.clone(),
            });
        }
        self.processor_state
            .validate()
            .map_err(|error| CheckpointError::Invalid(error.to_string()))?;
        if self
            .processor_state
            .last_now_ns
            .is_some_and(|last| last > self.checkpoint_time_ns)
        {
            return Err(CheckpointError::Invalid(
                "checkpoint time precedes the processor's last admitted logical time".to_owned(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckpointEnvelope {
    payload: CheckpointDocument,
    payload_sha256: String,
}

#[derive(Debug, Deserialize)]
struct CheckpointVersionEnvelope {
    payload: CheckpointVersionPayload,
}

#[derive(Debug, Deserialize)]
struct CheckpointVersionPayload {
    checkpoint_schema_version: u16,
}

impl CheckpointEnvelope {
    fn new(payload: CheckpointDocument) -> Result<Self, CheckpointError> {
        payload.validate()?;
        let payload_sha256 = checkpoint_digest(&payload)?;
        Ok(Self {
            payload,
            payload_sha256,
        })
    }

    fn open(self) -> Result<CheckpointDocument, CheckpointError> {
        let actual = checkpoint_digest(&self.payload)?;
        if self.payload_sha256 != actual {
            return Err(CheckpointError::HashMismatch {
                expected: self.payload_sha256,
                actual,
            });
        }
        self.payload.validate()?;
        Ok(self.payload)
    }
}

/// Load a bounded, owner-only checkpoint and verify its compatibility hash.
pub fn load_checkpoint(
    path: &Path,
    maximum_bytes: u64,
) -> Result<CheckpointDocument, CheckpointError> {
    let bytes = crate::atomic_file::read_bounded(path, maximum_bytes)?;
    // Check the outer compatibility version before decoding the complete
    // payload. Older state shapes may be missing fields required by the
    // current Rust type and should fail as unsupported, not malformed JSON.
    let version: CheckpointVersionEnvelope = serde_json::from_slice(&bytes)?;
    if version.payload.checkpoint_schema_version != CHECKPOINT_SCHEMA_VERSION {
        return Err(CheckpointError::UnsupportedVersion(
            version.payload.checkpoint_schema_version,
        ));
    }
    let envelope: CheckpointEnvelope = serde_json::from_slice(&bytes)?;
    envelope.open()
}

enum WriterMessage {
    Snapshot(CheckpointDocument),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CheckpointWriteReport {
    pub checkpoints_written: u64,
    pub last_checkpoint_bytes: u64,
    pub last_checkpoint_time_ns: u64,
}

pub struct CheckpointWriter {
    sender: Option<SyncSender<WriterMessage>>,
    worker: Option<JoinHandle<Result<CheckpointWriteReport, CheckpointError>>>,
}

impl CheckpointWriter {
    /// Persist the initial state before returning, then move replacement I/O to
    /// a dedicated thread. Fresh runs refuse to replace an existing target.
    pub fn create(
        path: &Path,
        initial: CheckpointDocument,
        maximum_bytes: u64,
        queue_capacity: usize,
        replace_existing: bool,
    ) -> Result<Self, CheckpointError> {
        crate::atomic_file::validate_maximum(maximum_bytes)?;
        if queue_capacity == 0 {
            return Err(CheckpointError::Invalid(
                "checkpoint queue capacity must be greater than zero".to_owned(),
            ));
        }
        initial.validate()?;
        let bytes = serialize_checkpoint(initial.clone(), maximum_bytes)?;
        if replace_existing {
            crate::atomic_file::replace(path, &bytes)?;
        } else {
            crate::atomic_file::create(path, &bytes)?;
        }
        let initial_bytes = u64::try_from(bytes.len()).map_err(|_| CheckpointError::Counter)?;
        let state = CheckpointWriteReport {
            checkpoints_written: 1,
            last_checkpoint_bytes: initial_bytes,
            last_checkpoint_time_ns: initial.checkpoint_time_ns,
        };
        let (sender, receiver) = mpsc::sync_channel(queue_capacity);
        let path = path.to_owned();
        let worker = thread::Builder::new()
            .name("btc-signal-checkpoint".to_owned())
            .spawn(move || checkpoint_writer_loop(path, maximum_bytes, state, receiver))
            .map_err(CheckpointError::Io)?;
        Ok(Self {
            sender: Some(sender),
            worker: Some(worker),
        })
    }

    /// Enqueue an immutable snapshot without performing filesystem I/O.
    pub fn enqueue(&self, checkpoint: CheckpointDocument) -> Result<(), CheckpointError> {
        checkpoint.validate()?;
        let sender = self.sender.as_ref().ok_or(CheckpointError::WriterStopped)?;
        try_enqueue(sender, WriterMessage::Snapshot(checkpoint))
    }

    /// Ensure the final accepted state replaces the prior checkpoint and join.
    pub fn finish(
        mut self,
        checkpoint: CheckpointDocument,
    ) -> Result<CheckpointWriteReport, CheckpointError> {
        checkpoint.validate()?;
        let send_result = self
            .sender
            .take()
            .ok_or(CheckpointError::WriterStopped)?
            .send(WriterMessage::Snapshot(checkpoint));
        let joined = self.join_worker();
        match joined {
            Err(error) => Err(error),
            Ok(report) => {
                send_result.map_err(|_| CheckpointError::WriterStopped)?;
                Ok(report)
            }
        }
    }

    /// Drain snapshots already accepted before an unrelated run failure.
    pub fn abort(mut self) -> Result<CheckpointWriteReport, CheckpointError> {
        self.sender.take();
        self.join_worker()
    }

    fn join_worker(&mut self) -> Result<CheckpointWriteReport, CheckpointError> {
        self.worker
            .take()
            .ok_or(CheckpointError::WriterStopped)?
            .join()
            .map_err(|_| CheckpointError::WriterPanicked)?
    }
}

fn try_enqueue<T>(sender: &SyncSender<T>, message: T) -> Result<(), CheckpointError> {
    match sender.try_send(message) {
        Ok(()) => Ok(()),
        Err(TrySendError::Full(_)) => Err(CheckpointError::QueueFull),
        Err(TrySendError::Disconnected(_)) => Err(CheckpointError::WriterStopped),
    }
}

fn checkpoint_writer_loop(
    path: PathBuf,
    maximum_bytes: u64,
    mut report: CheckpointWriteReport,
    receiver: Receiver<WriterMessage>,
) -> Result<CheckpointWriteReport, CheckpointError> {
    while let Ok(WriterMessage::Snapshot(checkpoint)) = receiver.recv() {
        let checkpoint_time_ns = checkpoint.checkpoint_time_ns;
        let bytes = serialize_checkpoint(checkpoint, maximum_bytes)?;
        crate::atomic_file::replace(&path, &bytes)?;
        report.checkpoints_written = report
            .checkpoints_written
            .checked_add(1)
            .ok_or(CheckpointError::Counter)?;
        report.last_checkpoint_bytes =
            u64::try_from(bytes.len()).map_err(|_| CheckpointError::Counter)?;
        report.last_checkpoint_time_ns = checkpoint_time_ns;
    }
    Ok(report)
}

fn serialize_checkpoint(
    checkpoint: CheckpointDocument,
    maximum_bytes: u64,
) -> Result<Vec<u8>, CheckpointError> {
    let envelope = CheckpointEnvelope::new(checkpoint)?;
    let bytes = serde_json::to_vec(&envelope)?;
    let size = u64::try_from(bytes.len()).map_err(|_| CheckpointError::Counter)?;
    if size > maximum_bytes {
        return Err(CheckpointError::SizeLimit {
            attempted: size,
            maximum: maximum_bytes,
        });
    }
    Ok(bytes)
}

fn checkpoint_digest(checkpoint: &CheckpointDocument) -> Result<String, CheckpointError> {
    let bytes = serde_json::to_vec(checkpoint)?;
    crate::atomic_file::content_digest(CHECKPOINT_HASH_DOMAIN, &bytes)
        .map_err(CheckpointError::from)
}

#[derive(Debug, Error)]
pub enum CheckpointError {
    #[error("checkpoint I/O failed: {0}")]
    Io(std::io::Error),
    #[error("checkpoint JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("operating-system entropy failed: {0}")]
    Entropy(getrandom::Error),
    #[error("unsupported checkpoint schema version {0}")]
    UnsupportedVersion(u16),
    #[error("invalid checkpoint: {0}")]
    Invalid(String),
    #[error("checkpoint contract fingerprint mismatch: expected {expected}, actual {actual}")]
    FingerprintMismatch { expected: String, actual: String },
    #[error("checkpoint content hash mismatch: expected {expected}, actual {actual}")]
    HashMismatch { expected: String, actual: String },
    #[error("unsafe checkpoint target: {0}")]
    UnsafeTarget(String),
    #[error("checkpoint would reach {attempted} bytes; maximum is {maximum}")]
    SizeLimit { attempted: u64, maximum: u64 },
    #[error("checkpoint queue is full; accepted state cannot be silently skipped")]
    QueueFull,
    #[error("checkpoint writer stopped before accepting state")]
    WriterStopped,
    #[error("checkpoint writer thread panicked")]
    WriterPanicked,
    #[error("checkpoint counter exhausted")]
    Counter,
    #[error(transparent)]
    Journal(#[from] crate::journal::JournalError),
}

impl From<crate::atomic_file::AtomicFileError> for CheckpointError {
    fn from(error: crate::atomic_file::AtomicFileError) -> Self {
        use crate::atomic_file::AtomicFileError;
        match error {
            AtomicFileError::Io(error) => CheckpointError::Io(error),
            AtomicFileError::Entropy(error) => CheckpointError::Entropy(error),
            AtomicFileError::Counter => CheckpointError::Counter,
            AtomicFileError::SizeLimit { attempted, maximum } => {
                CheckpointError::SizeLimit { attempted, maximum }
            }
            AtomicFileError::NotRegularFile => CheckpointError::UnsafeTarget(
                "checkpoint target must be a regular file, not a symlink".to_owned(),
            ),
            AtomicFileError::TargetChanged => CheckpointError::UnsafeTarget(
                "checkpoint target changed while it was being opened".to_owned(),
            ),
            AtomicFileError::Permissions { found } => CheckpointError::UnsafeTarget(format!(
                "checkpoint permissions must deny group and other access; found {found:03o}"
            )),
            AtomicFileError::PathMissingFileName => {
                CheckpointError::Invalid("checkpoint path must name a file".to_owned())
            }
            AtomicFileError::MaximumIsZero => CheckpointError::Invalid(
                "checkpoint maximum_bytes must be greater than zero".to_owned(),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    #[test]
    fn bounded_queue_reports_full_and_disconnected_without_dropping() {
        let (sender, receiver) = mpsc::sync_channel(1);
        try_enqueue(&sender, ()).expect("first item fills the queue");
        assert!(matches!(
            try_enqueue(&sender, ()),
            Err(CheckpointError::QueueFull)
        ));
        drop(receiver);
        assert!(matches!(
            try_enqueue(&sender, ()),
            Err(CheckpointError::WriterStopped)
        ));
    }

    #[test]
    fn load_rejects_a_hash_valid_unsupported_schema_version() {
        let runtime_config = SpotReferenceRuntimeConfig::new(100).expect("runtime config builds");
        let processor = crate::SpotSignalProcessor::new(
            "signal-a",
            "2026-08-13T12:00:00Z"
                .parse()
                .expect("test timestamp parses"),
        )
        .expect("processor builds");
        let mut document = CheckpointDocument::new(
            "0123456789abcdef0123456789abcdef".to_owned(),
            "2026-08-13T12:00:00Z".to_owned(),
            0,
            0,
            contract_fingerprint(&runtime_config).expect("fingerprint builds"),
            runtime_config,
            processor.checkpoint_state(),
        )
        .expect("valid document builds");
        document.checkpoint_schema_version = CHECKPOINT_SCHEMA_VERSION + 1;
        let envelope = CheckpointEnvelope {
            payload_sha256: checkpoint_digest(&document).expect("test digest builds"),
            payload: document,
        };
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock follows Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "btc-signal-checkpoint-version-{}-{unique}.json",
            std::process::id()
        ));
        std::fs::write(
            &path,
            serde_json::to_vec(&envelope).expect("test envelope serializes"),
        )
        .expect("test checkpoint writes");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .expect("test checkpoint permissions set");
        }
        assert!(matches!(
            load_checkpoint(&path, DEFAULT_CHECKPOINT_MAX_BYTES),
            Err(CheckpointError::UnsupportedVersion(version))
                if version == CHECKPOINT_SCHEMA_VERSION + 1
        ));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn load_rejects_legacy_v2_before_expanded_state_decode() {
        let runtime_config = SpotReferenceRuntimeConfig::new(100).expect("runtime config builds");
        let processor = crate::SpotSignalProcessor::new(
            "signal-a",
            "2026-08-13T12:00:00Z"
                .parse()
                .expect("test timestamp parses"),
        )
        .expect("processor builds");
        let document = CheckpointDocument::new(
            "0123456789abcdef0123456789abcdef".to_owned(),
            "2026-08-13T12:00:00Z".to_owned(),
            0,
            0,
            contract_fingerprint(&runtime_config).expect("fingerprint builds"),
            runtime_config,
            processor.checkpoint_state(),
        )
        .expect("valid document builds");
        let mut envelope =
            serde_json::to_value(CheckpointEnvelope::new(document).expect("valid envelope builds"))
                .expect("test envelope serializes");
        envelope["payload"]["checkpoint_schema_version"] = serde_json::json!(2);
        let features = envelope["payload"]["processor_state"]["features"]
            .as_object_mut()
            .expect("checkpoint features are an object");
        assert!(!features.is_empty());
        for feature in features.values_mut() {
            feature
                .as_object_mut()
                .expect("feature checkpoint is an object")
                .remove("lifetime_output_count");
        }

        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock follows Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "btc-signal-checkpoint-legacy-v2-{}-{unique}.json",
            std::process::id()
        ));
        std::fs::write(
            &path,
            serde_json::to_vec(&envelope).expect("legacy test envelope serializes"),
        )
        .expect("legacy test checkpoint writes");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .expect("test checkpoint permissions set");
        }
        assert!(matches!(
            load_checkpoint(&path, DEFAULT_CHECKPOINT_MAX_BYTES),
            Err(CheckpointError::UnsupportedVersion(2))
        ));
        let _ = std::fs::remove_file(path);
    }
}
