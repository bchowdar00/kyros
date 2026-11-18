use std::ffi::OsString;
use std::path::{Path, PathBuf};

use chrono::DateTime;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::digest::is_lower_hex;
use crate::prediction_journal::{
    PredictionContractFingerprint, PredictionJournalError, PredictionRuntimeConfig,
    prediction_contract_fingerprint, prediction_fingerprint_matches_accepted_schema,
};
use crate::{PREDICTION_SIGNAL_VERSION, PredictionSignalCheckpointState};

// Bumped to v6 for the A1 settlement-window pause: the processor state now
// carries the withheld quarter and the interval a repair must cover, so a
// restart mid-recovery resumes paused (audit A1/F4).
//
// Bumped to v7 for the E37b settlement-completion high-water mark: the processor
// state now carries the derived close of the last window whose one final ready
// decision was published, so a resume after the close does not re-publish it.
// Like the v6 field it is optional and omitted when absent, so a v5/v6 document
// is byte-identical to a v7 one that has not yet reached a close and still
// matches its recorded digest (ruling N2).
//
// Bumped to v8 for the E37c pending-final-decision slot: the processor state now
// carries a departed Kalshi market's frozen final settlement decision across the
// rollover gap. Like the v6/v7 fields it is optional and omitted when absent, so
// a v8 document with no pending decision has a processor state byte-identical to
// the v7 shape — the two full documents differ only in this version field — and
// every running v5/v6/v7 checkpoint still matches its own recorded digest (ruling
// N2). What the bump buys is a typed refusal on rollback, not a survivable
// rollback: a pre-E37c binary that rolled back cannot resume from a v8 document
// either way. Its readable range already stops at v7, so the VersionEnvelope
// preflight rejects a v8 document BY VERSION before strict decoding ever reaches
// the slot's `deny_unknown_fields` — and were the version equal, the unknown
// field would fail that strict decode instead. Both outcomes propagate a load
// error that keeps the lane from starting; the bump only makes it the typed
// by-version diagnostic `PREDICTION_CHECKPOINT_MIN_READ_SCHEMA_VERSION` bounds,
// rather than an opaque unknown-field parse failure.
pub const PREDICTION_CHECKPOINT_SCHEMA_VERSION: u16 = 8;

/// Oldest checkpoint version this build can READ.
///
/// v6 added one optional field to the processor state, so a v5 document is a v6
/// document with no settlement-window pause — which is what a lane that never
/// paused would write anyway. Reading it is therefore a rename, not a
/// migration: the field defaults to `None` and, because it is omitted from the
/// serialized form when absent, a v5 payload still digests to its recorded
/// hash.
///
/// This exists because the running lanes hold v5 checkpoints (ruling N2).
/// Refusing them would turn every restart after this release into a crash loop
/// that only an operator deleting `checkpoint.json` — and with it the publisher
/// epoch lineage — could clear. A v5 document is read and then superseded: the
/// next checkpoint this process writes is a fresh v6 document.
pub const PREDICTION_CHECKPOINT_MIN_READ_SCHEMA_VERSION: u16 = 5;

/// Whether this build can read a checkpoint stamped `version`.
const fn readable_checkpoint_version(version: u16) -> bool {
    version >= PREDICTION_CHECKPOINT_MIN_READ_SCHEMA_VERSION
        && version <= PREDICTION_CHECKPOINT_SCHEMA_VERSION
}
pub const PREDICTION_PROCESSOR_ID: &str = "btc.prediction-market.feed-derived";
pub const DEFAULT_PREDICTION_CHECKPOINT_MAX_BYTES: u64 = 1_048_576;

// Bumped to v2 for the composite round (ADR 0016): the processor state now embeds
// the spot-reference checkpoint and four new feature entries per venue, so the
// hashed state domain changes with the schema.
const CHECKPOINT_HASH_DOMAIN: &[u8] = b"tiny.prediction-signal.checkpoint.v2\0";
const MAX_PUBLISHER_EPOCH_LINEAGE: usize = 1_024;

/// Bounded recovery state for one venue-scoped prediction-signal process.
///
/// Market-relative quote, book, and flow windows deliberately do not survive
/// recovery. The restored market identity is retained only so discovery can
/// emit a visible recovery transition before it rebuilds that state.
///
/// **Rollback hazard (C5).** The processor state now carries an additive
/// `active_binding` (`PredictionSignalCheckpointState::active_binding`), written
/// whenever a market is bound — in every mode, not only under rotation. Because
/// both this document and the processor state are `#[serde(deny_unknown_fields)]`,
/// a *pre-C5* binary cannot read a checkpoint that carries `active_binding`: the
/// unknown field is a hard decode error, not a rename. This is intentionally not
/// worked around here — the checkpoint schema version does not move (the field is
/// serde-default and omitted when absent, so an unbound checkpoint is still
/// byte-identical to the pre-C5 shape). The rollback path is the standard one for
/// any prediction-lane change: the prediction lane supersedes and starts fresh —
/// an operator reverting to a pre-C5 binary removes `checkpoint.json` (or the lane
/// treats the unreadable checkpoint as a fresh epoch), losing only the honest
/// re-warm a restart already incurs. No behaviour changes on the C5 path.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PredictionCheckpointDocument {
    pub checkpoint_schema_version: u16,
    pub processor_id: String,
    pub processor_version: String,
    pub session_id: String,
    pub wall_start: String,
    pub checkpoint_time_ns: u64,
    pub next_admission_seq: u64,
    pub contract_fingerprint: PredictionContractFingerprint,
    pub runtime_config: PredictionRuntimeConfig,
    pub processor_state: PredictionSignalCheckpointState,
    pub publisher_epoch_lineage: Vec<String>,
}

impl PredictionCheckpointDocument {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        session_id: String,
        wall_start: String,
        checkpoint_time_ns: u64,
        next_admission_seq: u64,
        contract_fingerprint: PredictionContractFingerprint,
        runtime_config: PredictionRuntimeConfig,
        processor_state: PredictionSignalCheckpointState,
        publisher_epoch_lineage: Vec<String>,
    ) -> Result<Self, PredictionCheckpointError> {
        let document = Self {
            checkpoint_schema_version: PREDICTION_CHECKPOINT_SCHEMA_VERSION,
            processor_id: PREDICTION_PROCESSOR_ID.to_owned(),
            processor_version: PREDICTION_SIGNAL_VERSION.to_owned(),
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

    pub fn validate(&self) -> Result<(), PredictionCheckpointError> {
        // Read range, not equality: a v5 document restores (see
        // [`PREDICTION_CHECKPOINT_MIN_READ_SCHEMA_VERSION`]). Writing is still
        // current-version only — `new` stamps it and the envelope enforces it.
        if !readable_checkpoint_version(self.checkpoint_schema_version) {
            return Err(PredictionCheckpointError::UnsupportedVersion(
                self.checkpoint_schema_version,
            ));
        }
        if self.processor_id != PREDICTION_PROCESSOR_ID
            || self.processor_version != PREDICTION_SIGNAL_VERSION
        {
            return Err(PredictionCheckpointError::Invalid(format!(
                "checkpoint processor is {}@{}; expected {}@{}",
                self.processor_id,
                self.processor_version,
                PREDICTION_PROCESSOR_ID,
                PREDICTION_SIGNAL_VERSION
            )));
        }
        if self.session_id.len() != 32 || !is_lower_hex(&self.session_id) {
            return Err(PredictionCheckpointError::Invalid(
                "session_id must be 128 bits encoded as lowercase hexadecimal".to_owned(),
            ));
        }
        DateTime::parse_from_rfc3339(&self.wall_start).map_err(|error| {
            PredictionCheckpointError::Invalid(format!("wall_start is invalid: {error}"))
        })?;
        self.runtime_config
            .validate()
            .map_err(|error| PredictionCheckpointError::Invalid(error.to_string()))?;
        if !prediction_fingerprint_matches_accepted_schema(
            &self.runtime_config,
            &self.contract_fingerprint,
        )? {
            let expected = prediction_contract_fingerprint(&self.runtime_config)?;
            return Err(PredictionCheckpointError::FingerprintMismatch {
                expected: expected.aggregate_sha256,
                actual: self.contract_fingerprint.aggregate_sha256.clone(),
            });
        }
        self.processor_state
            .validate(self.runtime_config.index_armed)
            .map_err(|error| PredictionCheckpointError::Invalid(error.to_string()))?;
        if self.processor_state.venue != self.runtime_config.venue {
            return Err(PredictionCheckpointError::Invalid(
                "processor-state venue disagrees with runtime configuration".to_owned(),
            ));
        }
        if self
            .processor_state
            .last_now_ns
            .is_some_and(|last| last > self.checkpoint_time_ns)
        {
            return Err(PredictionCheckpointError::Invalid(
                "checkpoint time precedes the last admitted logical time".to_owned(),
            ));
        }
        if self.publisher_epoch_lineage.is_empty()
            || self.publisher_epoch_lineage.len() > MAX_PUBLISHER_EPOCH_LINEAGE
        {
            return Err(PredictionCheckpointError::Invalid(
                "publisher_epoch_lineage must contain 1 to 1024 entries".to_owned(),
            ));
        }
        let mut seen = std::collections::BTreeSet::new();
        for epoch in &self.publisher_epoch_lineage {
            if epoch.trim().is_empty() || epoch.len() > 128 || !seen.insert(epoch) {
                return Err(PredictionCheckpointError::Invalid(
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
struct PredictionCheckpointEnvelope {
    payload: PredictionCheckpointDocument,
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

impl PredictionCheckpointEnvelope {
    fn new(payload: PredictionCheckpointDocument) -> Result<Self, PredictionCheckpointError> {
        payload.validate()?;
        // Read old, write current: a document restored from an older version is
        // never written back under that version.
        if payload.checkpoint_schema_version != PREDICTION_CHECKPOINT_SCHEMA_VERSION {
            return Err(PredictionCheckpointError::UnsupportedVersion(
                payload.checkpoint_schema_version,
            ));
        }
        let payload_sha256 = checkpoint_digest(&payload)?;
        Ok(Self {
            payload,
            payload_sha256,
        })
    }

    fn open(self) -> Result<PredictionCheckpointDocument, PredictionCheckpointError> {
        let actual = checkpoint_digest(&self.payload)?;
        if actual != self.payload_sha256 {
            return Err(PredictionCheckpointError::HashMismatch {
                expected: self.payload_sha256,
                actual,
            });
        }
        self.payload.validate()?;
        Ok(self.payload)
    }
}

pub fn load_prediction_checkpoint(
    path: &Path,
    maximum_bytes: u64,
) -> Result<PredictionCheckpointDocument, PredictionCheckpointError> {
    let bytes = crate::atomic_file::read_bounded(path, maximum_bytes)?;
    // Preflight before strict decoding because an older state shape can lack
    // fields that the current payload type requires.
    let version: VersionEnvelope = serde_json::from_slice(&bytes)?;
    if !readable_checkpoint_version(version.payload.checkpoint_schema_version) {
        return Err(PredictionCheckpointError::UnsupportedVersion(
            version.payload.checkpoint_schema_version,
        ));
    }
    serde_json::from_slice::<PredictionCheckpointEnvelope>(&bytes)?.open()
}

/// Outcome of resuming from a checkpoint that may predate a contract change.
pub enum PredictionCheckpointResume {
    /// The checkpoint was read and its contract fingerprint matches this binary.
    Resume(Box<PredictionCheckpointDocument>),
    /// A readable checkpoint whose contract fingerprint differs from this binary
    /// was archived beside its generation; the lane must start a fresh epoch.
    Superseded {
        /// Where the superseded checkpoint was moved.
        archived: PathBuf,
        /// The superseded (stored) contract fingerprint.
        superseded_fingerprint: String,
        /// This binary's contract fingerprint.
        expected_fingerprint: String,
    },
}

/// Resume from the checkpoint at `path`, superseding it if its contract changed.
///
/// A checkpoint that is readable by version (ruling N2) but whose contract
/// fingerprint differs from the running binary is the release path for every
/// contract change: the definitions or schema that feed the prediction contract
/// moved (e.g. the E45 promotion of every source and signal to `approved`),
/// which is a fresh epoch, not a restart. Rather than bailing — which turns the
/// release into a crash loop only an operator deleting `checkpoint.json` (and
/// with it the publisher-epoch lineage) could clear — the superseded checkpoint
/// is archived beside its generation with a `.superseded-<fingerprint>` suffix
/// and the caller starts a fresh epoch. Every other load error still propagates.
pub fn resume_prediction_checkpoint(
    path: &Path,
    maximum_bytes: u64,
) -> Result<PredictionCheckpointResume, PredictionCheckpointError> {
    match load_prediction_checkpoint(path, maximum_bytes) {
        Ok(document) => Ok(PredictionCheckpointResume::Resume(Box::new(document))),
        Err(PredictionCheckpointError::FingerprintMismatch { expected, actual }) => {
            let archived = archive_superseded_checkpoint(path, &actual)?;
            Ok(PredictionCheckpointResume::Superseded {
                archived,
                superseded_fingerprint: actual,
                expected_fingerprint: expected,
            })
        }
        Err(error) => Err(error),
    }
}

/// Move a superseded checkpoint aside so a fresh epoch can create a new one at
/// the original path. The archive lives beside the generation with a
/// `.superseded-<fingerprint>` suffix naming the contract it recorded.
pub fn archive_superseded_checkpoint(
    path: &Path,
    superseded_fingerprint: &str,
) -> Result<PathBuf, PredictionCheckpointError> {
    let mut archived = OsString::from(path.as_os_str());
    archived.push(format!(".superseded-{superseded_fingerprint}"));
    let archived = PathBuf::from(archived);
    std::fs::rename(path, &archived)?;
    Ok(archived)
}

/// Create or atomically replace one owner-only prediction checkpoint.
pub fn write_prediction_checkpoint(
    path: &Path,
    document: PredictionCheckpointDocument,
    maximum_bytes: u64,
    replace_existing: bool,
) -> Result<PredictionCheckpointWriteReport, PredictionCheckpointError> {
    crate::atomic_file::validate_maximum(maximum_bytes)?;
    let checkpoint_time_ns = document.checkpoint_time_ns;
    let bytes = serialize_checkpoint(document, maximum_bytes)?;
    if replace_existing {
        crate::atomic_file::replace(path, &bytes)?;
    } else {
        crate::atomic_file::create(path, &bytes)?;
    }
    Ok(PredictionCheckpointWriteReport {
        checkpoint_bytes: u64::try_from(bytes.len())
            .map_err(|_| PredictionCheckpointError::Counter)?,
        checkpoint_time_ns,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PredictionCheckpointWriteReport {
    pub checkpoint_bytes: u64,
    pub checkpoint_time_ns: u64,
}

fn serialize_checkpoint(
    document: PredictionCheckpointDocument,
    maximum_bytes: u64,
) -> Result<Vec<u8>, PredictionCheckpointError> {
    let bytes = serde_json::to_vec(&PredictionCheckpointEnvelope::new(document)?)?;
    let size = u64::try_from(bytes.len()).map_err(|_| PredictionCheckpointError::Counter)?;
    if size > maximum_bytes {
        return Err(PredictionCheckpointError::SizeLimit {
            attempted: size,
            maximum: maximum_bytes,
        });
    }
    Ok(bytes)
}

fn checkpoint_digest(
    document: &PredictionCheckpointDocument,
) -> Result<String, PredictionCheckpointError> {
    let bytes = serde_json::to_vec(document)?;
    crate::atomic_file::content_digest(CHECKPOINT_HASH_DOMAIN, &bytes)
        .map_err(PredictionCheckpointError::from)
}

#[derive(Debug, Error)]
pub enum PredictionCheckpointError {
    #[error("unsupported prediction checkpoint version {0}")]
    UnsupportedVersion(u16),
    #[error("invalid prediction checkpoint: {0}")]
    Invalid(String),
    #[error(
        "prediction checkpoint contract fingerprint mismatch: expected {expected}, found {actual}"
    )]
    FingerprintMismatch { expected: String, actual: String },
    #[error("prediction checkpoint hash mismatch: expected {expected}, found {actual}")]
    HashMismatch { expected: String, actual: String },
    #[error("unsafe prediction checkpoint target: {0}")]
    UnsafeTarget(String),
    #[error("prediction checkpoint exceeds {maximum} bytes at {attempted} bytes")]
    SizeLimit { attempted: u64, maximum: u64 },
    #[error("prediction checkpoint counter exhausted")]
    Counter,
    #[error("prediction checkpoint I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("prediction checkpoint JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("prediction checkpoint entropy failed: {0}")]
    Entropy(getrandom::Error),
    #[error(transparent)]
    Journal(#[from] PredictionJournalError),
}

impl From<crate::atomic_file::AtomicFileError> for PredictionCheckpointError {
    fn from(error: crate::atomic_file::AtomicFileError) -> Self {
        use crate::atomic_file::AtomicFileError;
        match error {
            AtomicFileError::Io(error) => PredictionCheckpointError::Io(error),
            AtomicFileError::Entropy(error) => PredictionCheckpointError::Entropy(error),
            AtomicFileError::Counter => PredictionCheckpointError::Counter,
            AtomicFileError::SizeLimit { attempted, maximum } => {
                PredictionCheckpointError::SizeLimit { attempted, maximum }
            }
            AtomicFileError::NotRegularFile => PredictionCheckpointError::UnsafeTarget(
                "checkpoint target must be a regular non-symlink file".to_owned(),
            ),
            AtomicFileError::TargetChanged => PredictionCheckpointError::UnsafeTarget(
                "checkpoint target changed while opening".to_owned(),
            ),
            AtomicFileError::Permissions { found } => {
                PredictionCheckpointError::UnsafeTarget(format!(
                    "checkpoint permissions must deny group and other access; found {found:03o}"
                ))
            }
            AtomicFileError::PathMissingFileName => {
                PredictionCheckpointError::Invalid("checkpoint path must name a file".to_owned())
            }
            AtomicFileError::MaximumIsZero => PredictionCheckpointError::Invalid(
                "maximum checkpoint bytes must be positive".to_owned(),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PredictionSignalProcessor, prediction_signal_count};
    use chrono::{DateTime, Utc};
    use signals_core::PredictionMarketVenue;

    fn temporary_path(prefix: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "{prefix}-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock follows epoch")
                .as_nanos()
        ))
    }

    fn checkpoint() -> PredictionCheckpointDocument {
        let runtime = PredictionRuntimeConfig::new(PredictionMarketVenue::Polymarket, 2_000);
        let processor = PredictionSignalProcessor::new(
            PredictionMarketVenue::Polymarket,
            "epoch-a",
            "2026-08-15T12:00:00Z"
                .parse::<DateTime<Utc>>()
                .expect("wall start parses"),
        )
        .expect("processor builds");
        assert_eq!(
            processor.definitions().count(),
            prediction_signal_count(PredictionMarketVenue::Polymarket)
        );
        PredictionCheckpointDocument::new(
            "0123456789abcdef0123456789abcdef".to_owned(),
            "2026-08-15T12:00:00Z".to_owned(),
            0,
            0,
            prediction_contract_fingerprint(&runtime).expect("fingerprint builds"),
            runtime,
            processor.checkpoint_state(),
            vec!["epoch-a".to_owned()],
        )
        .expect("checkpoint builds")
    }

    #[test]
    fn genuine_old_shape_is_rejected_by_version_before_strict_decode() {
        let path = std::env::temp_dir().join(format!(
            "prediction-old-checkpoint-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock follows epoch")
                .as_nanos()
        ));
        let bytes = br#"{"payload":{"checkpoint_schema_version":3},"payload_sha256":"unused"}"#;
        std::fs::write(&path, bytes).expect("fixture writes");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .expect("fixture permissions set");
        }
        assert!(matches!(
            load_prediction_checkpoint(&path, DEFAULT_PREDICTION_CHECKPOINT_MAX_BYTES),
            Err(PredictionCheckpointError::UnsupportedVersion(3))
        ));
        std::fs::remove_file(path).expect("fixture removes");
    }

    #[test]
    fn immediately_prior_schema_four_checkpoint_is_refused() {
        // Schema 5, 6 and 7 load; the schema 4 below (the bump
        // that embedded the spot-reference state and moved the hash domain to v2)
        // must refuse at the version preflight, before any strict decode, so a
        // stale checkpoint can never seed a resumed run.
        let path = std::env::temp_dir().join(format!(
            "prediction-schema4-checkpoint-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock follows epoch")
                .as_nanos()
        ));
        let bytes = br#"{"payload":{"checkpoint_schema_version":4},"payload_sha256":"unused"}"#;
        std::fs::write(&path, bytes).expect("fixture writes");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .expect("fixture permissions set");
        }
        assert!(matches!(
            load_prediction_checkpoint(&path, DEFAULT_PREDICTION_CHECKPOINT_MAX_BYTES),
            Err(PredictionCheckpointError::UnsupportedVersion(4))
        ));
        std::fs::remove_file(path).expect("fixture removes");
    }

    /// Ruling N2: the running Tokyo lanes hold **schema 5** checkpoints. This
    /// build must read one — treating the absent settlement-window pause and the
    /// absent E37b completion mark as `None` — resume from it, and write the
    /// current schema next. Refusing it would turn every restart after this
    /// release into a crash loop, because `live-run.sh` appends
    /// `--resume-from-checkpoint` whenever `checkpoint.json` exists and
    /// `bin/prediction-signals.rs` bails on a load failure. This exercises that
    /// exact loader call.
    #[test]
    fn a_schema_five_checkpoint_restores_and_the_next_write_is_current() {
        use crate::prediction_journal::PredictionJournalSession;
        use crate::prediction_runtime::PredictionPipeline;

        let path = temporary_path("prediction-schema5-checkpoint");

        // A genuine v5 document: the current shape minus the field v6 added,
        // digested as a v5 writer would have digested it.
        let current = checkpoint();
        let mut five = current.clone();
        five.checkpoint_schema_version = 5;
        assert!(
            five.processor_state.settlement_window_pause.is_none(),
            "a v5 writer had no pause to record"
        );
        let payload_sha256 = checkpoint_digest(&five).expect("the v5 payload digests");
        let encoded = serde_json::to_vec(&PredictionCheckpointEnvelope {
            payload: five,
            payload_sha256,
        })
        .expect("the v5 envelope serializes");
        assert!(
            !String::from_utf8_lossy(&encoded).contains("settlement_window_pause")
                && !String::from_utf8_lossy(&encoded).contains("settlement_final_close_ms")
                && !String::from_utf8_lossy(&encoded).contains("pending_final_decision"),
            "a v5 payload carries no trace of the v6/v7/v8 fields, which is what keeps its digest valid"
        );
        std::fs::write(&path, &encoded).expect("fixture writes");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .expect("fixture permissions set");
        }

        // The loader `--resume-from-checkpoint` calls accepts it.
        let restored = load_prediction_checkpoint(&path, DEFAULT_PREDICTION_CHECKPOINT_MAX_BYTES)
            .expect("a schema-5 checkpoint loads");
        assert_eq!(restored.checkpoint_schema_version, 5);
        assert_eq!(restored.processor_state.settlement_window_pause, None);
        assert_eq!(restored.processor_state, current.processor_state);

        // And the pipeline resumes from it.
        let processor = PredictionSignalProcessor::restore(
            "epoch-b",
            "2026-08-15T12:00:00Z"
                .parse::<DateTime<Utc>>()
                .expect("wall start parses"),
            restored.processor_state.clone(),
        )
        .expect("the processor restores from schema 5");
        let session = PredictionJournalSession::new(
            "2026-08-15T12:00:00Z".to_owned(),
            "epoch-b".to_owned(),
            PredictionRuntimeConfig::new(PredictionMarketVenue::Polymarket, 2_000),
        )
        .expect("session builds");
        let pipeline =
            PredictionPipeline::replay(Vec::new(), processor, &session, 1_000, 1_000_000)
                .expect("the resumed pipeline builds");
        assert_eq!(pipeline.settlement_window_pause(), None);

        // The next write is the current schema, and restores again.
        let next = PredictionCheckpointDocument::new(
            restored.session_id.clone(),
            restored.wall_start.clone(),
            restored.checkpoint_time_ns + 1,
            restored.next_admission_seq,
            restored.contract_fingerprint.clone(),
            restored.runtime_config.clone(),
            restored.processor_state.clone(),
            restored.publisher_epoch_lineage.clone(),
        )
        .expect("the successor checkpoint builds");
        assert_eq!(
            next.checkpoint_schema_version,
            PREDICTION_CHECKPOINT_SCHEMA_VERSION
        );
        write_prediction_checkpoint(
            &path,
            next.clone(),
            DEFAULT_PREDICTION_CHECKPOINT_MAX_BYTES,
            true,
        )
        .expect("the current-schema checkpoint replaces the schema-5 one");
        let reloaded = load_prediction_checkpoint(&path, DEFAULT_PREDICTION_CHECKPOINT_MAX_BYTES)
            .expect("the current-schema checkpoint loads");
        assert_eq!(reloaded, next);
        assert_eq!(
            reloaded.checkpoint_schema_version,
            PREDICTION_CHECKPOINT_SCHEMA_VERSION
        );

        std::fs::remove_file(path).expect("fixture removes");
    }

    /// Ruling N2, v8: every version in the readable range 5..=8 loads through the
    /// real loader (an on-disk v5/v6/v7 checkpoint keeps its own recorded digest
    /// because reading it neither rewrites its bytes nor restamps its version),
    /// and v8 is the version a fresh write stamps.
    ///
    /// The load-bearing case is a genuine pre-v8 (v5/v6/v7) shape: its on-disk
    /// JSON carries no `pending_final_decision` key at all, so loading it exercises
    /// the optional-default path — the field defaults to `None` under the slot's
    /// `deny_unknown_fields` — rather than a v8 struct restamped with an older
    /// version integer. Each pre-v8 fixture is checked for slot absence on disk and
    /// for a `None` slot after load; the v5 fixture in the sibling test drops the
    /// v6/v7 fields too.
    #[test]
    fn every_readable_version_five_through_eight_loads() {
        assert_eq!(PREDICTION_CHECKPOINT_SCHEMA_VERSION, 8);
        for version in
            PREDICTION_CHECKPOINT_MIN_READ_SCHEMA_VERSION..=PREDICTION_CHECKPOINT_SCHEMA_VERSION
        {
            let path = temporary_path(&format!("prediction-schema{version}-checkpoint"));
            let mut document = checkpoint();
            document.checkpoint_schema_version = version;
            // A pre-v8 writer had no slot to record; assert the shape we serialize
            // is the genuine slotless one, not merely a v8 struct wearing an older
            // version integer.
            assert!(
                document.processor_state.pending_final_decision.is_none(),
                "a schema-{version} fixture carries no pending final-decision slot"
            );
            let payload_sha256 = checkpoint_digest(&document).expect("the payload digests");
            let encoded = serde_json::to_vec(&PredictionCheckpointEnvelope {
                payload: document,
                payload_sha256,
            })
            .expect("the envelope serializes");
            if version < PREDICTION_CHECKPOINT_SCHEMA_VERSION {
                assert!(
                    !String::from_utf8_lossy(&encoded).contains("pending_final_decision"),
                    "a schema-{version} document's JSON carries no slot field — a real pre-v8 shape"
                );
            }
            std::fs::write(&path, &encoded).expect("fixture writes");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                    .expect("fixture permissions set");
            }
            let restored =
                load_prediction_checkpoint(&path, DEFAULT_PREDICTION_CHECKPOINT_MAX_BYTES)
                    .unwrap_or_else(|error| panic!("a schema-{version} checkpoint loads: {error}"));
            assert_eq!(restored.checkpoint_schema_version, version);
            // The slotless on-disk shape restores with the field defaulted to None.
            assert!(
                restored.processor_state.pending_final_decision.is_none(),
                "a schema-{version} checkpoint restores with no pending final-decision slot"
            );
            std::fs::remove_file(path).expect("fixture removes");
        }
    }

    /// Ruling N2, v8: a v8 checkpoint carrying the E37c pending-final-decision slot
    /// round-trips through a write/load cycle unchanged, and a v8 checkpoint
    /// WITHOUT the slot is byte-identical to the v7 shape except for the version
    /// field — the slot is `skip_serializing_if = "Option::is_none"`, so its
    /// processor state carries no trace of the field and every running v5/v6/v7
    /// checkpoint still matches its recorded digest.
    #[test]
    fn a_v8_slot_round_trips_and_a_slotless_v8_matches_v7_but_for_the_version() {
        use crate::prediction_signals::PendingFinalDecisionState;

        // A v8 checkpoint WITHOUT the slot: its full encoding differs from the v7
        // shape only in the version field, and its processor state names no slot.
        let slotless = checkpoint();
        assert_eq!(slotless.checkpoint_schema_version, 8);
        assert!(slotless.processor_state.pending_final_decision.is_none());
        let v8_bytes = serde_json::to_vec(&slotless).expect("the slotless v8 encodes");
        let mut as_v7 = slotless.clone();
        as_v7.checkpoint_schema_version = 7;
        let v7_bytes = serde_json::to_vec(&as_v7).expect("the v7 shape encodes");
        assert_eq!(
            String::from_utf8_lossy(&v8_bytes).replacen(
                "\"checkpoint_schema_version\":8",
                "\"checkpoint_schema_version\":7",
                1,
            ),
            String::from_utf8_lossy(&v7_bytes),
            "a slotless v8 document differs from the v7 shape only in the version field"
        );
        assert!(
            !String::from_utf8_lossy(&v8_bytes).contains("pending_final_decision"),
            "a slotless v8 carries no trace of the field, which is what keeps N2 digest validity"
        );

        // A v8 checkpoint WITH the slot round-trips through write/load unchanged.
        let mut filled = checkpoint();
        filled.processor_state.pending_final_decision = Some(PendingFinalDecisionState {
            market_id: "KXBTC15M-1".to_owned(),
            market_generation: 1,
            quarter_close_time: "2026-08-15T12:15:00Z".to_owned(),
            quarter_close_ms: 1_755_259_200_000,
            deadline_ns: 905_000_000_000,
            anchor_price: "60150.25000000".to_owned(),
            threshold: "60000".to_owned(),
            spot_reference_mid: "60000".to_owned(),
            comparator: ">=".to_owned(),
            tie_outcome: "yes".to_owned(),
        });
        let path = temporary_path("prediction-v8-slot-checkpoint");
        write_prediction_checkpoint(
            &path,
            filled.clone(),
            DEFAULT_PREDICTION_CHECKPOINT_MAX_BYTES,
            false,
        )
        .expect("the v8-with-slot checkpoint writes");
        let reloaded = load_prediction_checkpoint(&path, DEFAULT_PREDICTION_CHECKPOINT_MAX_BYTES)
            .expect("the v8-with-slot checkpoint loads");
        assert_eq!(reloaded, filled, "the pending slot round-trips unchanged");
        assert_eq!(reloaded.checkpoint_schema_version, 8);
        std::fs::remove_file(path).expect("fixture removes");
    }

    /// A document restored under an older version is never written back under
    /// it: the write path stamps and enforces the current version.
    #[test]
    fn an_older_version_document_cannot_be_written_back() {
        let mut five = checkpoint();
        five.checkpoint_schema_version = 5;
        assert!(five.validate().is_ok(), "v5 is readable");
        let path = temporary_path("prediction-schema5-writeback");
        assert!(matches!(
            write_prediction_checkpoint(
                &path,
                five,
                DEFAULT_PREDICTION_CHECKPOINT_MAX_BYTES,
                false
            ),
            Err(PredictionCheckpointError::UnsupportedVersion(5))
        ));
        assert!(!path.exists(), "a refused write leaves no file");
    }

    #[test]
    fn write_and_load_round_trip_owner_only_checkpoint() {
        let path = std::env::temp_dir().join(format!(
            "prediction-checkpoint-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock follows epoch")
                .as_nanos()
        ));
        let expected = checkpoint();
        write_prediction_checkpoint(
            &path,
            expected.clone(),
            DEFAULT_PREDICTION_CHECKPOINT_MAX_BYTES,
            false,
        )
        .expect("checkpoint writes");
        let actual = load_prediction_checkpoint(&path, DEFAULT_PREDICTION_CHECKPOINT_MAX_BYTES)
            .expect("checkpoint loads");
        assert_eq!(actual, expected);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path)
                    .expect("metadata reads")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        std::fs::remove_file(path).expect("fixture removes");
    }

    /// The release path for a contract change (ruling N2/E45): a checkpoint that
    /// is readable by version but whose contract fingerprint differs from this
    /// binary is archived beside its generation and the lane starts a fresh
    /// epoch, instead of the loader bailing into a restart loop.
    #[test]
    fn a_fingerprint_mismatch_is_superseded_into_a_fresh_epoch() {
        let path = temporary_path("prediction-superseded-checkpoint");

        // A current-version document whose stored contract fingerprint no longer
        // matches this binary's — exactly what the E45 status promotion produced
        // for every running v5/v6 checkpoint.
        let mut document = checkpoint();
        let superseded_fingerprint = "0".repeat(64);
        document.contract_fingerprint.aggregate_sha256 = superseded_fingerprint.clone();
        let payload_sha256 = checkpoint_digest(&document).expect("the payload digests");
        let encoded = serde_json::to_vec(&PredictionCheckpointEnvelope {
            payload: document,
            payload_sha256,
        })
        .expect("the envelope serializes");
        std::fs::write(&path, &encoded).expect("fixture writes");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .expect("fixture permissions set");
        }

        // The plain loader still bails — this is a genuine fingerprint mismatch.
        assert!(matches!(
            load_prediction_checkpoint(&path, DEFAULT_PREDICTION_CHECKPOINT_MAX_BYTES),
            Err(PredictionCheckpointError::FingerprintMismatch { .. })
        ));

        // The resume path supersedes it instead: the file is archived beside its
        // generation and the caller is told to start a fresh epoch.
        let outcome = resume_prediction_checkpoint(&path, DEFAULT_PREDICTION_CHECKPOINT_MAX_BYTES)
            .expect("a fingerprint mismatch is superseded, not fatal");
        match outcome {
            PredictionCheckpointResume::Superseded {
                archived,
                superseded_fingerprint: superseded,
                expected_fingerprint,
            } => {
                assert_eq!(superseded, superseded_fingerprint);
                assert_ne!(expected_fingerprint, superseded_fingerprint);
                assert_eq!(
                    archived.file_name().and_then(|name| name.to_str()),
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .map(|name| format!("{name}.superseded-{superseded_fingerprint}"))
                        .as_deref()
                );
                assert!(archived.exists(), "the superseded checkpoint is archived");
                assert!(
                    !path.exists(),
                    "the original path is cleared for a fresh epoch"
                );
                std::fs::remove_file(archived).expect("archive removes");
            }
            PredictionCheckpointResume::Resume(_) => panic!("a mismatched contract must supersede"),
        }
    }
}
