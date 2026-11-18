//! Bounded resume state for the generic venue lane (A6, ruling E24/E26).
//!
//! Modelled on the prediction checkpoint but lane-parameterized: the processor
//! id and version are plain fields the lane's processor stamps, and the
//! processor state is an opaque `serde_json::Value` so one document type serves
//! the feed-only `binance`/`bybit`/`oracle` processors today and any future
//! signal processor without a new envelope. Ruling N2: the reader accepts the
//! running lane's current schema, which is exactly what it writes.

use std::path::Path;

use chrono::DateTime;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::digest::is_lower_hex;
use crate::lane_journal::{LaneContractFingerprint, LaneJournalError, LaneRuntimeConfig};

/// Schema 2 (A6 re-audit): the contract fingerprint gained the required
/// `source_definition_sha256` / `signal_definition_sha256` maps, so a schema-1
/// document no longer deserializes. venue-signals was never released at schema 1,
/// so there is no retained payload to read — the read range is exactly the
/// current version. When a future field is added additively, raise the current
/// version and lower [`LANE_CHECKPOINT_MIN_READ_SCHEMA_VERSION`] to keep reading
/// the retained shape (ruling N2).
pub const LANE_CHECKPOINT_SCHEMA_VERSION: u16 = 2;
pub const LANE_CHECKPOINT_MIN_READ_SCHEMA_VERSION: u16 = 2;
pub const DEFAULT_LANE_CHECKPOINT_MAX_BYTES: u64 = 1_048_576;

/// Whether a stored checkpoint version is inside the accepted read range.
fn readable_checkpoint_version(version: u16) -> bool {
    (LANE_CHECKPOINT_MIN_READ_SCHEMA_VERSION..=LANE_CHECKPOINT_SCHEMA_VERSION).contains(&version)
}
const MAX_PUBLISHER_EPOCH_LINEAGE: usize = 1024;
const CHECKPOINT_HASH_DOMAIN: &[u8] = b"tiny.venue-lane.checkpoint.v1\0";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaneCheckpointDocument {
    pub checkpoint_schema_version: u16,
    pub processor_id: String,
    pub processor_version: String,
    pub session_id: String,
    pub wall_start: String,
    pub checkpoint_time_ns: u64,
    pub next_admission_seq: u64,
    pub contract_fingerprint: LaneContractFingerprint,
    pub runtime_config: LaneRuntimeConfig,
    pub processor_state: serde_json::Value,
    pub publisher_epoch_lineage: Vec<String>,
}

impl LaneCheckpointDocument {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        processor_id: String,
        processor_version: String,
        session_id: String,
        wall_start: String,
        checkpoint_time_ns: u64,
        next_admission_seq: u64,
        contract_fingerprint: LaneContractFingerprint,
        runtime_config: LaneRuntimeConfig,
        processor_state: serde_json::Value,
        publisher_epoch_lineage: Vec<String>,
    ) -> Result<Self, LaneCheckpointError> {
        let document = Self {
            checkpoint_schema_version: LANE_CHECKPOINT_SCHEMA_VERSION,
            processor_id,
            processor_version,
            session_id,
            wall_start,
            checkpoint_time_ns,
            next_admission_seq,
            contract_fingerprint,
            runtime_config,
            processor_state,
            publisher_epoch_lineage,
        };
        document.validate()?;
        Ok(document)
    }

    pub fn validate(&self) -> Result<(), LaneCheckpointError> {
        if self.checkpoint_schema_version != LANE_CHECKPOINT_SCHEMA_VERSION {
            return Err(LaneCheckpointError::UnsupportedVersion(
                self.checkpoint_schema_version,
            ));
        }
        if self.processor_id.trim().is_empty() || self.processor_version.trim().is_empty() {
            return Err(LaneCheckpointError::Invalid(
                "processor id and version must be non-empty".to_owned(),
            ));
        }
        if self.session_id.len() != 32 || !is_lower_hex(&self.session_id) {
            return Err(LaneCheckpointError::Invalid(
                "session_id must be 128 bits encoded as lowercase hexadecimal".to_owned(),
            ));
        }
        DateTime::parse_from_rfc3339(&self.wall_start).map_err(|error| {
            LaneCheckpointError::Invalid(format!("wall_start must be RFC 3339: {error}"))
        })?;
        self.runtime_config
            .validate()
            .map_err(|error| LaneCheckpointError::Invalid(error.to_string()))?;
        self.contract_fingerprint
            .validate(&self.runtime_config)
            .map_err(LaneCheckpointError::from)?;
        if self.publisher_epoch_lineage.is_empty()
            || self.publisher_epoch_lineage.len() > MAX_PUBLISHER_EPOCH_LINEAGE
        {
            return Err(LaneCheckpointError::Invalid(
                "publisher_epoch_lineage must contain 1 to 1024 entries".to_owned(),
            ));
        }
        let mut seen = std::collections::BTreeSet::new();
        for epoch in &self.publisher_epoch_lineage {
            if epoch.trim().is_empty() || epoch.len() > 128 || !seen.insert(epoch.as_str()) {
                return Err(LaneCheckpointError::Invalid(
                    "publisher_epoch_lineage contains an empty, oversized, or duplicate epoch"
                        .to_owned(),
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LaneCheckpointEnvelope {
    payload: LaneCheckpointDocument,
    payload_sha256: String,
}

#[derive(Debug, Deserialize)]
struct VersionEnvelope {
    payload: VersionPayload,
}

#[derive(Debug, Deserialize)]
struct VersionPayload {
    checkpoint_schema_version: u16,
}

impl LaneCheckpointEnvelope {
    fn new(payload: LaneCheckpointDocument) -> Result<Self, LaneCheckpointError> {
        payload.validate()?;
        let payload_sha256 = checkpoint_digest(&payload)?;
        Ok(Self {
            payload,
            payload_sha256,
        })
    }

    fn open(self) -> Result<LaneCheckpointDocument, LaneCheckpointError> {
        let actual = checkpoint_digest(&self.payload)?;
        if actual != self.payload_sha256 {
            return Err(LaneCheckpointError::HashMismatch {
                expected: self.payload_sha256,
                actual,
            });
        }
        self.payload.validate()?;
        Ok(self.payload)
    }
}

pub fn load_lane_checkpoint(
    path: &Path,
    maximum_bytes: u64,
) -> Result<LaneCheckpointDocument, LaneCheckpointError> {
    let bytes = crate::atomic_file::read_bounded(path, maximum_bytes)?;
    // Preflight the version before decoding the full payload: an older shape can
    // lack fields the current document type requires and must fail as
    // unsupported, not as malformed JSON.
    let version: VersionEnvelope = serde_json::from_slice(&bytes)?;
    if !readable_checkpoint_version(version.payload.checkpoint_schema_version) {
        return Err(LaneCheckpointError::UnsupportedVersion(
            version.payload.checkpoint_schema_version,
        ));
    }
    serde_json::from_slice::<LaneCheckpointEnvelope>(&bytes)?.open()
}

/// Create or atomically replace one owner-only lane checkpoint.
pub fn write_lane_checkpoint(
    path: &Path,
    document: LaneCheckpointDocument,
    maximum_bytes: u64,
    replace_existing: bool,
) -> Result<LaneCheckpointWriteReport, LaneCheckpointError> {
    crate::atomic_file::validate_maximum(maximum_bytes)?;
    let checkpoint_time_ns = document.checkpoint_time_ns;
    let bytes = serialize_checkpoint(document, maximum_bytes)?;
    if replace_existing {
        crate::atomic_file::replace(path, &bytes)?;
    } else {
        crate::atomic_file::create(path, &bytes)?;
    }
    Ok(LaneCheckpointWriteReport {
        checkpoint_bytes: u64::try_from(bytes.len()).map_err(|_| LaneCheckpointError::Counter)?,
        checkpoint_time_ns,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LaneCheckpointWriteReport {
    pub checkpoint_bytes: u64,
    pub checkpoint_time_ns: u64,
}

fn serialize_checkpoint(
    document: LaneCheckpointDocument,
    maximum_bytes: u64,
) -> Result<Vec<u8>, LaneCheckpointError> {
    let bytes = serde_json::to_vec(&LaneCheckpointEnvelope::new(document)?)?;
    let size = u64::try_from(bytes.len()).map_err(|_| LaneCheckpointError::Counter)?;
    if size > maximum_bytes {
        return Err(LaneCheckpointError::SizeLimit {
            attempted: size,
            maximum: maximum_bytes,
        });
    }
    Ok(bytes)
}

fn checkpoint_digest(document: &LaneCheckpointDocument) -> Result<String, LaneCheckpointError> {
    let bytes = serde_json::to_vec(document)?;
    crate::atomic_file::content_digest(CHECKPOINT_HASH_DOMAIN, &bytes)
        .map_err(LaneCheckpointError::from)
}

#[derive(Debug, Error)]
pub enum LaneCheckpointError {
    #[error("unsupported lane checkpoint version {0}")]
    UnsupportedVersion(u16),
    #[error("invalid lane checkpoint: {0}")]
    Invalid(String),
    #[error("lane checkpoint contract fingerprint mismatch: expected {expected}, found {actual}")]
    FingerprintMismatch { expected: String, actual: String },
    #[error("lane checkpoint hash mismatch: expected {expected}, found {actual}")]
    HashMismatch { expected: String, actual: String },
    #[error("unsafe lane checkpoint target: {0}")]
    UnsafeTarget(String),
    #[error("lane checkpoint exceeds {maximum} bytes at {attempted} bytes")]
    SizeLimit { attempted: u64, maximum: u64 },
    #[error("lane checkpoint counter exhausted")]
    Counter,
    #[error("lane checkpoint I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("lane checkpoint JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("lane checkpoint entropy failed: {0}")]
    Entropy(getrandom::Error),
    #[error(transparent)]
    Journal(#[from] LaneJournalError),
}

impl From<crate::atomic_file::AtomicFileError> for LaneCheckpointError {
    fn from(error: crate::atomic_file::AtomicFileError) -> Self {
        use crate::atomic_file::AtomicFileError;
        match error {
            AtomicFileError::Io(error) => Self::Io(error),
            AtomicFileError::Entropy(error) => Self::Entropy(error),
            AtomicFileError::Counter => Self::Counter,
            AtomicFileError::SizeLimit { attempted, maximum } => {
                Self::SizeLimit { attempted, maximum }
            }
            AtomicFileError::NotRegularFile => Self::UnsafeTarget(
                "checkpoint target must be a regular non-symlink file".to_owned(),
            ),
            AtomicFileError::TargetChanged => {
                Self::UnsafeTarget("checkpoint target changed while opening".to_owned())
            }
            AtomicFileError::Permissions { found } => Self::UnsafeTarget(format!(
                "checkpoint permissions must deny group and other access; found {found:03o}"
            )),
            AtomicFileError::PathMissingFileName => {
                Self::Invalid("checkpoint path must name a file".to_owned())
            }
            AtomicFileError::MaximumIsZero => {
                Self::Invalid("checkpoint maximum_bytes must be greater than zero".to_owned())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;
    use crate::lane_journal::{LaneRuntimeConfig, lane_contract_fingerprint};

    fn sample_document() -> LaneCheckpointDocument {
        let runtime_config = LaneRuntimeConfig::new("oracle", 1_000_000_000);
        let fingerprint = lane_contract_fingerprint(&runtime_config).expect("fingerprint builds");
        LaneCheckpointDocument::new(
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
        .expect("document builds")
    }

    fn temp_checkpoint_path(label: &str) -> std::path::PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock follows Unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "lane-checkpoint-{label}-{}-{unique}.json",
            std::process::id()
        ))
    }

    #[test]
    fn a_current_version_checkpoint_round_trips() {
        let path = temp_checkpoint_path("roundtrip");
        write_lane_checkpoint(
            &path,
            sample_document(),
            DEFAULT_LANE_CHECKPOINT_MAX_BYTES,
            false,
        )
        .expect("checkpoint writes");
        let loaded = load_lane_checkpoint(&path, DEFAULT_LANE_CHECKPOINT_MAX_BYTES)
            .expect("checkpoint loads");
        assert_eq!(
            loaded.checkpoint_schema_version,
            LANE_CHECKPOINT_SCHEMA_VERSION
        );
        assert_eq!(loaded, sample_document());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_pre_bump_schema_version_is_rejected_before_decoding() {
        // The schema-2 fingerprint gained required fields (m7): a document
        // labelled schema 1 is outside the read range and must fail as
        // unsupported, not as malformed JSON.
        let envelope = LaneCheckpointEnvelope::new(sample_document()).expect("envelope builds");
        let mut value = serde_json::to_value(&envelope).expect("envelope serializes");
        value["payload"]["checkpoint_schema_version"] = serde_json::json!(1);
        let path = temp_checkpoint_path("legacy-v1");
        std::fs::write(
            &path,
            serde_json::to_vec(&value).expect("legacy envelope serializes"),
        )
        .expect("legacy checkpoint writes");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .expect("permissions set");
        }
        assert!(matches!(
            load_lane_checkpoint(&path, DEFAULT_LANE_CHECKPOINT_MAX_BYTES),
            Err(LaneCheckpointError::UnsupportedVersion(1))
        ));
        let _ = std::fs::remove_file(path);
    }
}
