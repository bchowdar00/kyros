use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Read};
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::{self, SyncSender};
use std::thread::{self, JoinHandle};
#[cfg(test)]
use std::time::{Duration, Instant};

use btc_feeds::prediction_live::{PredictionDiscoveryAttempt, PredictionSocketTransition};
use chrono::DateTime;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use signals_core::{MarketTransitionReason, PredictionMarketVenue, SourceEvent};
use thiserror::Error;

use crate::digest::{hex_lower, is_lower_hex, sha256_hex};
use crate::prediction_checkpoint::PredictionCheckpointDocument;
use crate::prediction_contract_documents_for;

pub const PREDICTION_JOURNAL_SCHEMA_VERSION: u16 = 8;
/// Schema 5 prediction journals are CRC-framed read-only retained evidence
/// (ADR 0012, T012b): they predate the socket-connectivity input variant, so
/// they can never contain one and replay byte-identically through the current
/// binary. Enveloped framing is shared with the current schema; only the recorded
/// version distinguishes them. Schemas 6 and 7 are intentionally NOT retained:
/// schema 6 predates the composite spot-events input variant (ADR 0016), while
/// schema 7's connectivity input did not bind transport ownership. Both refuse
/// with UnsupportedVersion rather than being misparsed or assigned guessed
/// ownership, and their evidence must be refreshed at the applicable gate.
pub const RETAINED_ENVELOPED_PREDICTION_JOURNAL_SCHEMA_VERSION: u16 = 5;
/// Schema 4 prediction journals are read-only retained evidence (SIG-043): they
/// predate CRC framing and replay byte-identically through the current binary.
/// New journals are always written at [`PREDICTION_JOURNAL_SCHEMA_VERSION`].
pub const RETAINED_PREDICTION_JOURNAL_SCHEMA_VERSION: u16 = 4;
pub const DEFAULT_PREDICTION_JOURNAL_MAX_BYTES: u64 = 16 * 1024 * 1024 * 1024;
pub const DEFAULT_PREDICTION_JOURNAL_QUEUE_CAPACITY: usize = 16_384;
pub const MAX_PREDICTION_JOURNAL_LINE_BYTES: usize = crate::journal_core::MAX_JOURNAL_LINE_BYTES;

const FINGERPRINT_DOMAIN: &[u8] = b"tiny.prediction-signal.contract-fingerprint.v1\0";
const PREDICTION_JOURNAL_BUFFER_BYTES: usize = crate::journal_core::JOURNAL_BUFFER_BYTES;
#[cfg(test)]
const PREDICTION_JOURNAL_SYNC_INTERVAL: Duration = crate::journal_core::JOURNAL_SYNC_INTERVAL;
const SIGNAL_UPDATE_SCHEMA: &str = include_str!("../../../schemas/signal-update.schema.json");
const SIGNAL_STREAM_MESSAGE_SCHEMA: &str =
    include_str!("../../../schemas/signal-stream-message.schema.json");
const INSTANCE_STATUS_SCHEMA: &str = include_str!("../../../schemas/instance-status.schema.json");

/// Stream v3 is not output-neutral. Only the current schema is accepted; every
/// v2 fingerprint must refuse replay or restore rather than riding a legacy list.
const ACCEPTED_STREAM_SCHEMAS: &[&str] = &[SIGNAL_STREAM_MESSAGE_SCHEMA];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PredictionRuntimeConfig {
    pub venue: PredictionMarketVenue,
    pub discovery_retry_ms: u64,
    /// Whether this session emits observe-only `feed_health` stream frames. It is
    /// a semantic capability bound into the contract fingerprint. Absent-and-false
    /// serializes to nothing, so a pre-capability config re-serializes to
    /// identical fingerprint bytes and its replay stays byte-identical.
    #[serde(default, skip_serializing_if = "is_false")]
    pub feed_health_stream: bool,
    /// Whether the Kalshi lane's `cfbrti` index socket is armed (E43): CF-index
    /// ingestion, the global `btc.cfbrti.oracle_reference` emission and the two
    /// Kalshi settlement identities are on exactly when this is true. It is bound
    /// into the contract fingerprint, so a resume under a different arming is a
    /// contract change and is refused. Absent-and-false serializes to nothing, so
    /// the deployed disarmed configuration's fingerprint bytes are unchanged.
    #[serde(default, skip_serializing_if = "is_false")]
    pub index_armed: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl PredictionRuntimeConfig {
    pub fn new(venue: PredictionMarketVenue, discovery_retry_ms: u64) -> Self {
        Self {
            venue,
            discovery_retry_ms,
            feed_health_stream: true,
            index_armed: false,
        }
    }

    /// Arm or disarm the Kalshi CF index socket (E43). Meaningful on Kalshi
    /// alone; the deployed four-venue configuration leaves it disarmed.
    #[must_use]
    pub fn with_index_armed(mut self, index_armed: bool) -> Self {
        self.index_armed = index_armed;
        self
    }

    pub fn validate(&self) -> Result<(), PredictionJournalError> {
        if self.discovery_retry_ms == 0 || self.discovery_retry_ms > 60_000 {
            return Err(PredictionJournalError::Invalid(
                "prediction discovery retry must be in 1..=60000 milliseconds".to_owned(),
            ));
        }
        Ok(())
    }

    /// Whether two configurations are equal apart from the `feed_health_stream`
    /// capability. Resuming a pre-capability checkpoint under a declaring binary
    /// is a legitimate additive upgrade, so the flag alone may differ.
    pub fn matches_ignoring_feed_health(&self, other: &Self) -> bool {
        let mut normalized = self.clone();
        normalized.feed_health_stream = other.feed_health_stream;
        &normalized == other
    }
}

/// Closed transport ownership for prediction-runner connectivity evidence.
/// One worker's socket lifecycle must never mutate unrelated feed-health rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PredictionTransport {
    Kalshi,
    Polymarket,
    Coinbase,
    Kraken,
    PolymarketRtds,
    /// The Kalshi index socket: a second, dedicated authenticated connection on
    /// the Kalshi lane carrying the CF Benchmarks BRTI channels and the Pyth
    /// relay. Separate from `Kalshi` because the market socket is torn down on
    /// every 15-minute rollover and every rediscovery, while the index socket —
    /// which has no market binding — stays up; a shared transport would mark
    /// BRTI sick 96 times a day for reasons that have nothing to do with it.
    KalshiIndex,
}

impl PredictionTransport {
    pub const fn for_venue(venue: PredictionMarketVenue) -> Self {
        match venue {
            PredictionMarketVenue::Kalshi => Self::Kalshi,
            PredictionMarketVenue::Polymarket => Self::Polymarket,
        }
    }

    pub const fn health_label(self) -> &'static str {
        match self {
            Self::Kalshi => "kalshi",
            Self::Polymarket => "polymarket",
            Self::Coinbase => "coinbase",
            Self::Kraken => "kraken",
            Self::PolymarketRtds => "polymarket_rtds",
            Self::KalshiIndex => "kalshi_index",
        }
    }

    pub const fn is_allowed_for(self, venue: PredictionMarketVenue) -> bool {
        matches!(self, Self::Coinbase | Self::Kraken)
            || matches!(
                (venue, self),
                (PredictionMarketVenue::Kalshi, Self::Kalshi)
                    | (PredictionMarketVenue::Kalshi, Self::KalshiIndex)
                    | (PredictionMarketVenue::Polymarket, Self::Polymarket)
                    | (PredictionMarketVenue::Polymarket, Self::PolymarketRtds)
            )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PredictionContractFingerprint {
    pub aggregate_sha256: String,
    pub signal_definition_sha256: BTreeMap<String, String>,
    pub source_definition_sha256: BTreeMap<String, String>,
    pub runtime_schema_sha256: BTreeMap<String, String>,
    pub runtime_config_sha256: String,
}

impl PredictionContractFingerprint {
    pub fn validate(
        &self,
        runtime_config: &PredictionRuntimeConfig,
    ) -> Result<(), PredictionJournalError> {
        for (name, value) in [
            ("aggregate_sha256", &self.aggregate_sha256),
            ("runtime_config_sha256", &self.runtime_config_sha256),
        ] {
            validate_digest(name, value)?;
        }
        let documents =
            prediction_contract_documents_for(runtime_config.venue, runtime_config.index_armed);
        let expected_signals = documents
            .iter()
            .map(|(id, _)| *id)
            .filter(|id| id.starts_with("btc."))
            .collect::<Vec<_>>();
        let expected_sources = documents
            .iter()
            .map(|(id, _)| *id)
            .filter(|id| !id.starts_with("btc."))
            .collect::<Vec<_>>();
        if self.signal_definition_sha256.len() != expected_signals.len()
            || expected_signals
                .iter()
                .any(|id| !self.signal_definition_sha256.contains_key(*id))
        {
            return Err(PredictionJournalError::Invalid(
                "prediction fingerprint does not contain the complete signal-definition set"
                    .to_owned(),
            ));
        }
        if self.source_definition_sha256.len() != expected_sources.len()
            || expected_sources
                .iter()
                .any(|id| !self.source_definition_sha256.contains_key(*id))
        {
            return Err(PredictionJournalError::Invalid(
                "prediction fingerprint does not contain the complete source-definition set"
                    .to_owned(),
            ));
        }
        for (id, digest) in self
            .signal_definition_sha256
            .iter()
            .chain(self.source_definition_sha256.iter())
            .chain(self.runtime_schema_sha256.iter())
        {
            validate_digest(id, digest)?;
        }
        let expected_runtime_schemas =
            ["instance-status", "signal-stream-message", "signal-update"];
        if self.runtime_schema_sha256.len() != expected_runtime_schemas.len()
            || expected_runtime_schemas
                .iter()
                .any(|name| !self.runtime_schema_sha256.contains_key(*name))
        {
            return Err(PredictionJournalError::Invalid(
                "prediction fingerprint does not contain the complete runtime-schema set"
                    .to_owned(),
            ));
        }
        if !prediction_fingerprint_matches_accepted_schema(runtime_config, self)? {
            let expected = prediction_contract_fingerprint(runtime_config)?;
            return Err(PredictionJournalError::FingerprintMismatch {
                expected: expected.aggregate_sha256,
                actual: self.aggregate_sha256.clone(),
            });
        }
        Ok(())
    }
}

pub fn prediction_contract_fingerprint(
    runtime_config: &PredictionRuntimeConfig,
) -> Result<PredictionContractFingerprint, PredictionJournalError> {
    prediction_contract_fingerprint_under(runtime_config, SIGNAL_STREAM_MESSAGE_SCHEMA)
}

/// True when `recorded` matches the prediction fingerprint recomputed under any
/// accepted stream-schema revision (see [`ACCEPTED_STREAM_SCHEMAS`]). Every other
/// input is recomputed from `runtime_config`, so only a differing stream-schema
/// revision is accepted, never any other contract drift.
pub fn prediction_fingerprint_matches_accepted_schema(
    runtime_config: &PredictionRuntimeConfig,
    recorded: &PredictionContractFingerprint,
) -> Result<bool, PredictionJournalError> {
    for stream_schema in ACCEPTED_STREAM_SCHEMAS {
        if &prediction_contract_fingerprint_under(runtime_config, stream_schema)? == recorded {
            return Ok(true);
        }
    }
    Ok(false)
}

fn prediction_contract_fingerprint_under(
    runtime_config: &PredictionRuntimeConfig,
    stream_schema: &str,
) -> Result<PredictionContractFingerprint, PredictionJournalError> {
    runtime_config.validate()?;
    let runtime_bytes = serde_json::to_vec(runtime_config)?;
    let documents =
        prediction_contract_documents_for(runtime_config.venue, runtime_config.index_armed);
    let mut signals = BTreeMap::new();
    let mut sources = BTreeMap::new();
    for (id, raw) in &documents {
        let target = if id.starts_with("btc.") {
            &mut signals
        } else {
            &mut sources
        };
        if target
            .insert((*id).to_owned(), sha256_hex(raw.as_bytes()))
            .is_some()
        {
            return Err(PredictionJournalError::Invalid(format!(
                "duplicate prediction contract identity {id}"
            )));
        }
    }
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
    let runtime_config_sha256 = sha256_hex(&runtime_bytes);
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
    Ok(PredictionContractFingerprint {
        aggregate_sha256: format!("{:x}", aggregate.finalize()),
        signal_definition_sha256: signals,
        source_definition_sha256: sources,
        runtime_schema_sha256,
        runtime_config_sha256,
    })
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PredictionJournalSession {
    pub journal_schema_version: u16,
    pub session_id: String,
    pub wall_start: String,
    pub venue: PredictionMarketVenue,
    pub publisher_epoch: String,
    pub contract_fingerprint: PredictionContractFingerprint,
    pub runtime_config: PredictionRuntimeConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_checkpoint: Option<PredictionCheckpointDocument>,
    /// The immutable admission epoch that source events are stamped with and
    /// validated against, when it differs from `publisher_epoch` (the per-file-
    /// generation *output* epoch used on emitted frames). Absent (`None`) means
    /// the two are the same — every journal written before in-process generation
    /// rotation, and every non-rotation run, so those journals are byte-unchanged
    /// and the schema version does not move. Set only by
    /// [`PredictionJournalSession::with_admission_epoch`] on a rotated
    /// generation, where the feeds keep stamping the process `--publisher-epoch`
    /// while each generation publishes under its own `<prefix>-<unix>` output
    /// epoch (C5 two-epoch split).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admission_epoch: Option<String>,
    /// Set when this journal begins a file generation opened by in-process
    /// rotation rather than a process start (C5). Such a generation inherits a
    /// warm processor whose rolling ring/bar state the checkpoint does not carry,
    /// so a cold replay honestly re-warms those identities and its terminal
    /// stream-message/update counts differ from the warm live run. The replay
    /// terminal self-check reads this flag to report that warm-vs-cold difference
    /// as the documented re-warm gap instead of failing. Absent (`false`) on
    /// every non-rotation journal, so those are byte-unchanged.
    #[serde(default, skip_serializing_if = "is_false")]
    pub opened_by_rotation: bool,
}

impl PredictionJournalSession {
    pub fn new(
        wall_start: String,
        publisher_epoch: String,
        runtime_config: PredictionRuntimeConfig,
    ) -> Result<Self, PredictionJournalError> {
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random).map_err(PredictionJournalError::Entropy)?;
        let session = Self {
            journal_schema_version: PREDICTION_JOURNAL_SCHEMA_VERSION,
            session_id: hex_lower(&random),
            wall_start,
            venue: runtime_config.venue,
            publisher_epoch,
            contract_fingerprint: prediction_contract_fingerprint(&runtime_config)?,
            runtime_config,
            initial_checkpoint: None,
            admission_epoch: None,
            opened_by_rotation: false,
        };
        session.validate()?;
        Ok(session)
    }

    /// Resume under the current `runtime_config`. The resumed session runs a new
    /// epoch under the current contract, so its fingerprint is recomputed under
    /// the current configuration. The current config must match the checkpoint's
    /// apart from the additive `feed_health_stream` capability; resuming a
    /// pre-capability checkpoint under a declaring binary upgrades emission going
    /// forward, while the old journal still replays gated by its recorded config.
    pub fn resumed(
        publisher_epoch: String,
        initial_checkpoint: PredictionCheckpointDocument,
        runtime_config: PredictionRuntimeConfig,
    ) -> Result<Self, PredictionJournalError> {
        initial_checkpoint
            .validate()
            .map_err(|error| PredictionJournalError::Invalid(error.to_string()))?;
        if !initial_checkpoint
            .runtime_config
            .matches_ignoring_feed_health(&runtime_config)
        {
            return Err(PredictionJournalError::Invalid(
                "resume runtime configuration differs from the checkpoint beyond the feed-health capability".to_owned(),
            ));
        }
        if initial_checkpoint
            .publisher_epoch_lineage
            .iter()
            .any(|epoch| epoch == &publisher_epoch)
        {
            return Err(PredictionJournalError::Invalid(
                "resumed publisher_epoch already appears in checkpoint lineage".to_owned(),
            ));
        }
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random).map_err(PredictionJournalError::Entropy)?;
        let contract_fingerprint = prediction_contract_fingerprint(&runtime_config)?;
        let session = Self {
            journal_schema_version: PREDICTION_JOURNAL_SCHEMA_VERSION,
            session_id: hex_lower(&random),
            wall_start: initial_checkpoint.wall_start.clone(),
            venue: runtime_config.venue,
            publisher_epoch,
            contract_fingerprint,
            runtime_config,
            initial_checkpoint: Some(initial_checkpoint),
            admission_epoch: None,
            opened_by_rotation: false,
        };
        session.validate()?;
        Ok(session)
    }

    /// The immutable admission epoch source events are validated against.
    ///
    /// Falls back to `publisher_epoch` when [`Self::admission_epoch`] is `None`,
    /// so a single-epoch (non-rotated) session validates events against its own
    /// epoch exactly as before the two-epoch split.
    #[must_use]
    pub fn admission_epoch(&self) -> &str {
        self.admission_epoch
            .as_deref()
            .unwrap_or(&self.publisher_epoch)
    }

    /// Stamp the admission epoch for a rotated generation, whose output epoch
    /// (`publisher_epoch`) differs from the process admission epoch the feeds
    /// keep using. Passing an `admission_epoch` equal to `publisher_epoch` leaves
    /// the field `None` (the single-epoch default), so a caller can wire this
    /// unconditionally without perturbing a non-rotated session's bytes.
    pub fn with_admission_epoch(
        mut self,
        admission_epoch: impl Into<String>,
    ) -> Result<Self, PredictionJournalError> {
        let admission_epoch = admission_epoch.into();
        self.admission_epoch = if admission_epoch == self.publisher_epoch {
            None
        } else {
            Some(admission_epoch)
        };
        self.validate()?;
        Ok(self)
    }

    /// Mark this session as beginning a rotation-opened file generation (C5), so
    /// the replay terminal self-check tolerates the warm-vs-cold re-warm gap.
    #[must_use]
    pub fn opened_by_rotation(mut self) -> Self {
        self.opened_by_rotation = true;
        self
    }

    pub fn validate(&self) -> Result<(), PredictionJournalError> {
        if self.journal_schema_version != PREDICTION_JOURNAL_SCHEMA_VERSION
            && self.journal_schema_version != RETAINED_ENVELOPED_PREDICTION_JOURNAL_SCHEMA_VERSION
            && self.journal_schema_version != RETAINED_PREDICTION_JOURNAL_SCHEMA_VERSION
        {
            return Err(PredictionJournalError::UnsupportedVersion(
                self.journal_schema_version,
            ));
        }
        if self.session_id.len() != 32 || !is_lower_hex(&self.session_id) {
            return Err(PredictionJournalError::Invalid(
                "session_id must be 128 bits encoded as lowercase hexadecimal".to_owned(),
            ));
        }
        DateTime::parse_from_rfc3339(&self.wall_start).map_err(|error| {
            PredictionJournalError::Invalid(format!("wall_start is invalid: {error}"))
        })?;
        if self.publisher_epoch.trim().is_empty() || self.publisher_epoch.len() > 128 {
            return Err(PredictionJournalError::Invalid(
                "publisher_epoch must contain 1 to 128 bytes".to_owned(),
            ));
        }
        if let Some(admission_epoch) = &self.admission_epoch {
            if admission_epoch.trim().is_empty() || admission_epoch.len() > 128 {
                return Err(PredictionJournalError::Invalid(
                    "admission_epoch must contain 1 to 128 bytes".to_owned(),
                ));
            }
            if admission_epoch == &self.publisher_epoch {
                return Err(PredictionJournalError::Invalid(
                    "admission_epoch must be omitted when it equals publisher_epoch".to_owned(),
                ));
            }
        }
        if self.venue != self.runtime_config.venue {
            return Err(PredictionJournalError::Invalid(
                "session venue disagrees with runtime configuration".to_owned(),
            ));
        }
        self.runtime_config.validate()?;
        self.contract_fingerprint.validate(&self.runtime_config)?;
        if let Some(checkpoint) = &self.initial_checkpoint {
            checkpoint
                .validate()
                .map_err(|error| PredictionJournalError::Invalid(error.to_string()))?;
            // Same contract apart from the additive feed-health capability. Each
            // fingerprint is validated against its own config via the accepted-
            // schema lineage, so a direct fingerprint equality would forbid the
            // legitimate capability upgrade without adding integrity.
            if checkpoint.wall_start != self.wall_start
                || !checkpoint
                    .runtime_config
                    .matches_ignoring_feed_health(&self.runtime_config)
            {
                return Err(PredictionJournalError::Invalid(
                    "initial checkpoint does not match the journal contract or time origin"
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
    fn validate_writable(&self) -> Result<(), PredictionJournalError> {
        if self.journal_schema_version != PREDICTION_JOURNAL_SCHEMA_VERSION {
            return Err(PredictionJournalError::Invalid(format!(
                "journals are written only at schema {PREDICTION_JOURNAL_SCHEMA_VERSION}; \
                 refusing to write schema {}",
                self.journal_schema_version
            )));
        }
        self.validate()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "input_type", rename_all = "snake_case", deny_unknown_fields)]
pub enum PredictionJournalInput {
    Discovery {
        admission_seq: u64,
        generation: u64,
        attempt: PredictionDiscoveryAttempt,
        now_ns: u64,
    },
    Bind {
        admission_seq: u64,
        generation: u64,
        previous_market_id: Option<String>,
        reason: MarketTransitionReason,
        event: SourceEvent,
        logical_receipt_ns: u64,
        now_ns: u64,
    },
    MarketEvents {
        admission_seq: u64,
        generation: u64,
        events: Arc<Vec<SourceEvent>>,
        logical_receipt_ns: u64,
        now_ns: u64,
    },
    Unavailable {
        admission_seq: u64,
        generation: u64,
        previous_market_id: Option<String>,
        reason: MarketTransitionReason,
        now_ns: u64,
    },
    /// A batch of Coinbase/Kraken ticker events feeding the internally hosted spot
    /// reference (ADR 0016). Ticker admissions are NOT market-scoped: they carry no
    /// generation and never move the market lifecycle, so they are admitted through
    /// their own variant rather than the generation-checked `MarketEvents`.
    SpotEvents {
        admission_seq: u64,
        events: Arc<Vec<SourceEvent>>,
        logical_receipt_ns: u64,
        now_ns: u64,
    },
    HealthTick {
        admission_seq: u64,
        now_ns: u64,
    },
    /// A socket-level connectivity transition (ADR 0012, T012b). It drives only
    /// the observe-only feed-health board: it never marks the bound market
    /// unavailable and never writes a market transition, so a transport blip does
    /// not blank a signal or move the market lifecycle.
    Connectivity {
        admission_seq: u64,
        transport: PredictionTransport,
        transition: PredictionSocketTransition,
        now_ns: u64,
    },
}

impl PredictionJournalInput {
    pub fn admission_seq(&self) -> u64 {
        match self {
            Self::Discovery { admission_seq, .. }
            | Self::Bind { admission_seq, .. }
            | Self::MarketEvents { admission_seq, .. }
            | Self::SpotEvents { admission_seq, .. }
            | Self::Unavailable { admission_seq, .. }
            | Self::HealthTick { admission_seq, .. }
            | Self::Connectivity { admission_seq, .. } => *admission_seq,
        }
    }

    pub fn now_ns(&self) -> u64 {
        match self {
            Self::Discovery { now_ns, .. }
            | Self::Bind { now_ns, .. }
            | Self::MarketEvents { now_ns, .. }
            | Self::SpotEvents { now_ns, .. }
            | Self::Unavailable { now_ns, .. }
            | Self::HealthTick { now_ns, .. }
            | Self::Connectivity { now_ns, .. } => *now_ns,
        }
    }

    fn validate(&self, venue: PredictionMarketVenue) -> Result<(), PredictionJournalError> {
        match self {
            Self::Discovery {
                generation,
                attempt,
                ..
            } => {
                if *generation == 0 || attempt.venue != venue {
                    return Err(PredictionJournalError::Invalid(
                        "discovery generation or venue is invalid".to_owned(),
                    ));
                }
                attempt
                    .validate()
                    .map_err(|error| PredictionJournalError::Invalid(error.to_string()))?;
            }
            Self::Bind {
                generation,
                previous_market_id,
                reason,
                event,
                logical_receipt_ns,
                now_ns,
                ..
            } => {
                if *generation == 0
                    || !matches!(
                        reason,
                        MarketTransitionReason::InitialBind
                            | MarketTransitionReason::ScheduledRollover
                            | MarketTransitionReason::RecoveryBind
                    )
                    || logical_receipt_ns > now_ns
                    || event.publisher_epoch.is_empty()
                {
                    return Err(PredictionJournalError::Invalid(
                        "bind admission is invalid".to_owned(),
                    ));
                }
                let market_field = match venue {
                    PredictionMarketVenue::Polymarket => "market_id",
                    PredictionMarketVenue::Kalshi => "market_ticker",
                };
                let next_market_id = event
                    .values
                    .get(market_field)
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| {
                        PredictionJournalError::Invalid(
                            "bind admission is missing its venue market identity".to_owned(),
                        )
                    })?;
                let identities_are_valid = match reason {
                    MarketTransitionReason::InitialBind => previous_market_id.is_none(),
                    MarketTransitionReason::ScheduledRollover => previous_market_id
                        .as_deref()
                        .is_some_and(|previous| previous != next_market_id),
                    MarketTransitionReason::RecoveryBind => previous_market_id.is_some(),
                    _ => false,
                };
                if !identities_are_valid {
                    return Err(PredictionJournalError::Invalid(
                        "bind admission market identities disagree with its transition reason"
                            .to_owned(),
                    ));
                }
            }
            Self::MarketEvents {
                generation,
                events,
                logical_receipt_ns,
                now_ns,
                ..
            } => {
                if *generation == 0
                    || events.is_empty()
                    || logical_receipt_ns > now_ns
                    || events.iter().any(|event| event.publisher_epoch.is_empty())
                {
                    return Err(PredictionJournalError::Invalid(
                        "market-event admission is empty or has invalid timing".to_owned(),
                    ));
                }
            }
            Self::SpotEvents {
                events,
                logical_receipt_ns,
                now_ns,
                ..
            } => {
                if events.is_empty()
                    || logical_receipt_ns > now_ns
                    || events.iter().any(|event| {
                        event.publisher_epoch.is_empty()
                            || !matches!(
                                event.source_id.as_str(),
                                crate::COINBASE_TICKER_SOURCE_ID | crate::KRAKEN_TICKER_SOURCE_ID
                            )
                    })
                {
                    return Err(PredictionJournalError::Invalid(
                        "spot-event admission is empty, mistimed, or carries a non-ticker source"
                            .to_owned(),
                    ));
                }
            }
            Self::Unavailable {
                generation,
                previous_market_id,
                reason,
                ..
            } => {
                if *generation == 0
                    || !matches!(
                        reason,
                        MarketTransitionReason::ScheduledRollover
                            | MarketTransitionReason::DiscoveryUnreachable
                            | MarketTransitionReason::NextMarketUnlisted
                            | MarketTransitionReason::InvalidDiscovery
                            | MarketTransitionReason::CloseToNextOpenGap
                            | MarketTransitionReason::ProviderLifecycleChange
                    )
                {
                    return Err(PredictionJournalError::Invalid(
                        "unavailable admission has an invalid generation or reason".to_owned(),
                    ));
                }
                if matches!(
                    reason,
                    MarketTransitionReason::ScheduledRollover
                        | MarketTransitionReason::ProviderLifecycleChange
                ) && previous_market_id.is_none()
                {
                    return Err(PredictionJournalError::Invalid(
                        "market lifecycle unavailability requires the prior market identity"
                            .to_owned(),
                    ));
                }
            }
            Self::HealthTick { .. } => {}
            Self::Connectivity { transport, .. } => {
                if !transport.is_allowed_for(venue) {
                    return Err(PredictionJournalError::Invalid(
                        "connectivity transport is not owned by this venue".to_owned(),
                    ));
                }
            }
        }
        Ok(())
    }

    /// Validate the stateful market-lifecycle admission rules without changing
    /// the serialized journal row. Replay calls this against reconstructed state
    /// before applying each input, so a bound→Bind row and a mismatched departure
    /// fail before they can produce stream output.
    pub fn validate_lifecycle_against(
        &self,
        active_market: Option<(&str, u64)>,
    ) -> Result<(), PredictionJournalError> {
        match self {
            Self::Bind { .. } if active_market.is_some() => Err(PredictionJournalError::Invalid(
                "bind admission is valid only while the venue is unbound".to_owned(),
            )),
            Self::Unavailable {
                generation,
                previous_market_id,
                ..
            } => {
                let (active_market_id, active_generation) = active_market.ok_or_else(|| {
                    PredictionJournalError::Invalid(
                        "unavailable admission requires a bound market".to_owned(),
                    )
                })?;
                if previous_market_id.as_deref() != Some(active_market_id) {
                    return Err(PredictionJournalError::Invalid(
                        "unavailable previous_market_id disagrees with the active market"
                            .to_owned(),
                    ));
                }
                let expected_generation = active_generation.checked_add(1).ok_or_else(|| {
                    PredictionJournalError::Invalid(
                        "active market generation cannot advance".to_owned(),
                    )
                })?;
                if *generation != expected_generation {
                    return Err(PredictionJournalError::Invalid(format!(
                        "unavailable generation must be active generation + 1 (expected {expected_generation}, found {generation})"
                    )));
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PredictionJournalTotals {
    pub admitted_inputs: u64,
    pub discovery_attempts: u64,
    pub bindings: u64,
    pub market_event_batches: u64,
    pub spot_event_batches: u64,
    pub unavailable_transitions: u64,
    pub health_ticks: u64,
    pub updates_written: u64,
    pub stream_messages_written: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PredictionJournalEnd {
    pub totals: PredictionJournalTotals,
    pub quality_reason_entries: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "record_type", rename_all = "snake_case", deny_unknown_fields)]
enum PredictionJournalRecord {
    PredictionSession {
        #[serde(flatten)]
        session: Box<PredictionJournalSession>,
    },
    Discovery {
        admission_seq: u64,
        generation: u64,
        attempt: PredictionDiscoveryAttempt,
        now_ns: u64,
    },
    Bind {
        admission_seq: u64,
        generation: u64,
        previous_market_id: Option<String>,
        reason: MarketTransitionReason,
        event: SourceEvent,
        logical_receipt_ns: u64,
        now_ns: u64,
    },
    MarketEvents {
        admission_seq: u64,
        generation: u64,
        events: Arc<Vec<SourceEvent>>,
        logical_receipt_ns: u64,
        now_ns: u64,
    },
    SpotEvents {
        admission_seq: u64,
        events: Arc<Vec<SourceEvent>>,
        logical_receipt_ns: u64,
        now_ns: u64,
    },
    Unavailable {
        admission_seq: u64,
        generation: u64,
        previous_market_id: Option<String>,
        reason: MarketTransitionReason,
        now_ns: u64,
    },
    HealthTick {
        admission_seq: u64,
        now_ns: u64,
    },
    Connectivity {
        admission_seq: u64,
        transport: PredictionTransport,
        transition: PredictionSocketTransition,
        now_ns: u64,
    },
    End {
        #[serde(flatten)]
        end: PredictionJournalEnd,
    },
}

impl From<PredictionJournalInput> for PredictionJournalRecord {
    fn from(input: PredictionJournalInput) -> Self {
        match input {
            PredictionJournalInput::Discovery {
                admission_seq,
                generation,
                attempt,
                now_ns,
            } => Self::Discovery {
                admission_seq,
                generation,
                attempt,
                now_ns,
            },
            PredictionJournalInput::Bind {
                admission_seq,
                generation,
                previous_market_id,
                reason,
                event,
                logical_receipt_ns,
                now_ns,
            } => Self::Bind {
                admission_seq,
                generation,
                previous_market_id,
                reason,
                event,
                logical_receipt_ns,
                now_ns,
            },
            PredictionJournalInput::MarketEvents {
                admission_seq,
                generation,
                events,
                logical_receipt_ns,
                now_ns,
            } => Self::MarketEvents {
                admission_seq,
                generation,
                events,
                logical_receipt_ns,
                now_ns,
            },
            PredictionJournalInput::SpotEvents {
                admission_seq,
                events,
                logical_receipt_ns,
                now_ns,
            } => Self::SpotEvents {
                admission_seq,
                events,
                logical_receipt_ns,
                now_ns,
            },
            PredictionJournalInput::Unavailable {
                admission_seq,
                generation,
                previous_market_id,
                reason,
                now_ns,
            } => Self::Unavailable {
                admission_seq,
                generation,
                previous_market_id,
                reason,
                now_ns,
            },
            PredictionJournalInput::HealthTick {
                admission_seq,
                now_ns,
            } => Self::HealthTick {
                admission_seq,
                now_ns,
            },
            PredictionJournalInput::Connectivity {
                admission_seq,
                transport,
                transition,
                now_ns,
            } => Self::Connectivity {
                admission_seq,
                transport,
                transition,
                now_ns,
            },
        }
    }
}

impl PredictionJournalRecord {
    fn into_input(self) -> Option<PredictionJournalInput> {
        match self {
            Self::Discovery {
                admission_seq,
                generation,
                attempt,
                now_ns,
            } => Some(PredictionJournalInput::Discovery {
                admission_seq,
                generation,
                attempt,
                now_ns,
            }),
            Self::Bind {
                admission_seq,
                generation,
                previous_market_id,
                reason,
                event,
                logical_receipt_ns,
                now_ns,
            } => Some(PredictionJournalInput::Bind {
                admission_seq,
                generation,
                previous_market_id,
                reason,
                event,
                logical_receipt_ns,
                now_ns,
            }),
            Self::MarketEvents {
                admission_seq,
                generation,
                events,
                logical_receipt_ns,
                now_ns,
            } => Some(PredictionJournalInput::MarketEvents {
                admission_seq,
                generation,
                events,
                logical_receipt_ns,
                now_ns,
            }),
            Self::SpotEvents {
                admission_seq,
                events,
                logical_receipt_ns,
                now_ns,
            } => Some(PredictionJournalInput::SpotEvents {
                admission_seq,
                events,
                logical_receipt_ns,
                now_ns,
            }),
            Self::Unavailable {
                admission_seq,
                generation,
                previous_market_id,
                reason,
                now_ns,
            } => Some(PredictionJournalInput::Unavailable {
                admission_seq,
                generation,
                previous_market_id,
                reason,
                now_ns,
            }),
            Self::HealthTick {
                admission_seq,
                now_ns,
            } => Some(PredictionJournalInput::HealthTick {
                admission_seq,
                now_ns,
            }),
            Self::Connectivity {
                admission_seq,
                transport,
                transition,
                now_ns,
            } => Some(PredictionJournalInput::Connectivity {
                admission_seq,
                transport,
                transition,
                now_ns,
            }),
            Self::PredictionSession { .. } | Self::End { .. } => None,
        }
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

type PredictionWriterMessage =
    crate::journal_core::WriterMessage<PredictionJournalInput, PredictionJournalEnd>;

struct PredictionRecords;

impl crate::journal_core::JournalRecords for PredictionRecords {
    type Input = PredictionJournalInput;
    type End = PredictionJournalEnd;
    type Record = PredictionJournalRecord;

    fn input_record(input: PredictionJournalInput) -> PredictionJournalRecord {
        PredictionJournalRecord::from(input)
    }

    fn end_record(end: PredictionJournalEnd) -> PredictionJournalRecord {
        PredictionJournalRecord::End { end }
    }
}

pub struct PredictionJournalWriter {
    sender: Option<SyncSender<PredictionWriterMessage>>,
    worker: Option<JoinHandle<Result<PredictionJournalWriteReport, PredictionJournalError>>>,
    venue: PredictionMarketVenue,
    next_admission_seq: u64,
}

impl PredictionJournalWriter {
    pub fn create(
        path: &Path,
        session: PredictionJournalSession,
        maximum_bytes: u64,
    ) -> Result<Self, PredictionJournalError> {
        // Production fixes the watermark at the durability constant; the trailing
        // `minimum_free_space_bytes` seam exists only so a boundary test can raise
        // the threshold above a real filesystem's actual free space.
        Self::create_with_queue_capacity(
            path,
            session,
            maximum_bytes,
            DEFAULT_PREDICTION_JOURNAL_QUEUE_CAPACITY,
            crate::journal_core::MINIMUM_FREE_SPACE_BYTES,
        )
    }

    fn create_with_queue_capacity(
        path: &Path,
        session: PredictionJournalSession,
        maximum_bytes: u64,
        queue_capacity: usize,
        minimum_free_space_bytes: u64,
    ) -> Result<Self, PredictionJournalError> {
        if maximum_bytes == 0 {
            return Err(PredictionJournalError::Invalid(
                "maximum journal bytes must be positive".to_owned(),
            ));
        }
        if queue_capacity == 0 {
            return Err(PredictionJournalError::Invalid(
                "journal queue capacity must be positive".to_owned(),
            ));
        }
        session.validate_writable()?;
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        options.mode(0o600);
        let file = options.open(path)?;
        let venue = session.venue;
        let next_admission_seq = session
            .initial_checkpoint
            .as_ref()
            .map_or(0, |checkpoint| checkpoint.next_admission_seq);
        let mut writer = BufWriter::with_capacity(PREDICTION_JOURNAL_BUFFER_BYTES, file);
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
            &PredictionJournalRecord::PredictionSession {
                session: Box::new(session),
            },
        )?;
        core.sync(&mut writer)?;

        let (sender, receiver) = mpsc::sync_channel(queue_capacity);
        let worker = thread::Builder::new()
            .name("btc-prediction-journal".to_owned())
            .spawn(
                move || -> Result<PredictionJournalWriteReport, PredictionJournalError> {
                    let core = crate::journal_core::writer_loop::<PredictionRecords>(
                        writer, core, receiver,
                    )?;
                    Ok(PredictionJournalWriteReport {
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
            venue,
            next_admission_seq,
        })
    }

    /// Enqueue one validated immutable input without waiting for disk I/O.
    pub fn enqueue(&mut self, input: PredictionJournalInput) -> Result<(), PredictionJournalError> {
        if input.admission_seq() != self.next_admission_seq {
            return Err(PredictionJournalError::Sequence {
                expected: self.next_admission_seq,
                actual: input.admission_seq(),
            });
        }
        input.validate(self.venue)?;
        let sender = self
            .sender
            .as_ref()
            .ok_or(PredictionJournalError::WriterStopped)?;
        try_enqueue_prediction(sender, PredictionWriterMessage::Input(input))?;
        self.next_admission_seq = self
            .next_admission_seq
            .checked_add(1)
            .ok_or(PredictionJournalError::Counter)?;
        Ok(())
    }

    pub fn finish(
        mut self,
        end: PredictionJournalEnd,
    ) -> Result<PredictionJournalWriteReport, PredictionJournalError> {
        if end.totals.admitted_inputs != self.next_admission_seq {
            return Err(PredictionJournalError::Invalid(
                "journal end total disagrees with admitted inputs".to_owned(),
            ));
        }
        let send_result = self
            .sender
            .take()
            .ok_or(PredictionJournalError::WriterStopped)?
            .send(PredictionWriterMessage::End(end));
        let joined = self.join_worker();
        match joined {
            Err(error) => Err(error),
            Ok(report) => {
                send_result.map_err(|_| PredictionJournalError::WriterStopped)?;
                Ok(report)
            }
        }
    }

    /// Drain and sync the recoverable prefix without writing an end record.
    pub fn abort(mut self) -> Result<PredictionJournalWriteReport, PredictionJournalError> {
        self.sender.take();
        self.join_worker()
    }

    fn join_worker(&mut self) -> Result<PredictionJournalWriteReport, PredictionJournalError> {
        let worker = self
            .worker
            .take()
            .ok_or(PredictionJournalError::WriterStopped)?;
        worker
            .join()
            .map_err(|_| PredictionJournalError::WriterPanicked)?
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PredictionJournalWriteReport {
    pub records_written: u64,
    pub admitted_inputs: u64,
    pub bytes_written: u64,
    pub data_syncs: u64,
}

fn try_enqueue_prediction(
    sender: &SyncSender<PredictionWriterMessage>,
    message: PredictionWriterMessage,
) -> Result<(), PredictionJournalError> {
    crate::journal_core::try_enqueue(sender, message).map_err(PredictionJournalError::from)
}

#[cfg(test)]
type PredictionWriterState = crate::journal_core::WriterCore;

pub struct PredictionJournalReplayReader {
    reader: BufReader<File>,
    frame_kind: crate::journal_core::FrameKind,
    line_number: u64,
    session: PredictionJournalSession,
    next_admission_seq: u64,
    last_now_ns: Option<u64>,
    maximum_bytes: u64,
    bytes_read: u64,
    line: Vec<u8>,
    end: Option<PredictionJournalEnd>,
}

impl PredictionJournalReplayReader {
    pub fn open(path: &Path, maximum_bytes: u64) -> Result<Self, PredictionJournalError> {
        if maximum_bytes == 0 {
            return Err(PredictionJournalError::Invalid(
                "maximum journal bytes must be positive".to_owned(),
            ));
        }
        let file = open_owner_only(path)?;
        let size = file.metadata()?.len();
        if size > maximum_bytes {
            return Err(PredictionJournalError::SizeLimit {
                attempted: size,
                maximum: maximum_bytes,
            });
        }
        let mut reader = BufReader::with_capacity(1024 * 1024, file);
        let mut line = Vec::new();
        let first_bytes = read_bounded_line(&mut reader, &mut line)?;
        if first_bytes == 0 {
            // A zero-byte journal (created but never synced before a crash) has no
            // first record. Surface the same serde end-of-input error the
            // pre-framing reader produced, not a framing error. Parsing an empty
            // slice yields "EOF while parsing a value", identical to the legacy
            // decode of the first record.
            return Err(serde_json::from_slice::<serde_json::Value>(&line)
                .expect_err("empty input cannot parse as JSON")
                .into());
        }
        let had_newline = line.ends_with(b"\n");
        let trimmed = trim_newline(&line);
        // The first line's framing decides the whole journal: a schema-5 journal
        // envelopes every line, retained schema-4 evidence does not.
        let is_final = !had_newline
            || !crate::journal_core::more_bytes_follow(&mut reader)
                .map_err(PredictionJournalError::Io)?;
        let (frame_kind, record_bytes): (crate::journal_core::FrameKind, &[u8]) =
            match crate::journal_core::classify_first_frame(trimmed, is_final) {
                Ok(crate::journal_core::FirstFrame::EnvelopedV5(bytes)) => {
                    (crate::journal_core::FrameKind::EnvelopedV5, bytes)
                }
                Ok(crate::journal_core::FirstFrame::BareV4(bytes)) => {
                    (crate::journal_core::FrameKind::BareV4, bytes)
                }
                Err(crate::journal_core::FramingError::TornTail) => {
                    return Err(PredictionJournalError::TornTail);
                }
                Err(crate::journal_core::FramingError::Corruption { line }) => {
                    return Err(PredictionJournalError::Corruption { line });
                }
            };
        // A missing terminal newline means the file ended mid-line. For schema 5
        // that is a torn tail even when the record parses and its CRC matches; for
        // retained schema 4 it is the legacy truncated-line condition, reported
        // exactly as the pre-framing reader did at open.
        match frame_kind {
            crate::journal_core::FrameKind::EnvelopedV5 if !had_newline => {
                return Err(PredictionJournalError::TornTail);
            }
            crate::journal_core::FrameKind::BareV4 if !had_newline => {
                return Err(PredictionJournalError::TruncatedLine);
            }
            _ => {}
        }
        let record_type: RecordTypeEnvelope = serde_json::from_slice(record_bytes)?;
        if record_type.record_type != "prediction_session" {
            return Err(PredictionJournalError::Invalid(
                "first record must be prediction_session".to_owned(),
            ));
        }
        // Preflight compatibility before strict decoding so genuine older
        // shapes fail as unsupported rather than as missing current fields. The
        // framing pins the accepted versions: enveloped lines are the current
        // schema 8 or its CRC-framed retained predecessor schema 5 (both share
        // the envelope; only the recorded version distinguishes them), while bare
        // lines are the pre-CRC retained schema 4. Schemas 6 and 7 fail here before
        // strict record decoding because their admitted-input shapes differ.
        let version: VersionEnvelope = serde_json::from_slice(record_bytes)?;
        let version_accepted = match frame_kind {
            crate::journal_core::FrameKind::EnvelopedV5 => {
                version.journal_schema_version == PREDICTION_JOURNAL_SCHEMA_VERSION
                    || version.journal_schema_version
                        == RETAINED_ENVELOPED_PREDICTION_JOURNAL_SCHEMA_VERSION
            }
            crate::journal_core::FrameKind::BareV4 => {
                version.journal_schema_version == RETAINED_PREDICTION_JOURNAL_SCHEMA_VERSION
            }
        };
        if !version_accepted {
            return Err(PredictionJournalError::UnsupportedVersion(
                version.journal_schema_version,
            ));
        }
        let PredictionJournalRecord::PredictionSession { session } =
            serde_json::from_slice(record_bytes)?
        else {
            return Err(PredictionJournalError::Invalid(
                "first record must be prediction_session".to_owned(),
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
        let bytes_read = u64::try_from(line.len()).map_err(|_| PredictionJournalError::Counter)?;
        Ok(Self {
            reader,
            frame_kind,
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

    pub fn session(&self) -> &PredictionJournalSession {
        &self.session
    }

    pub fn next_input(&mut self) -> Result<Option<PredictionJournalInput>, PredictionJournalError> {
        if self.end.is_some() {
            return Ok(None);
        }
        self.line.clear();
        let bytes = read_bounded_line(&mut self.reader, &mut self.line)?;
        if bytes == 0 {
            return Err(PredictionJournalError::MissingEnd);
        }
        self.bytes_read = self
            .bytes_read
            .checked_add(u64::try_from(bytes).map_err(|_| PredictionJournalError::Counter)?)
            .ok_or(PredictionJournalError::Counter)?;
        if self.bytes_read > self.maximum_bytes {
            return Err(PredictionJournalError::SizeLimit {
                attempted: self.bytes_read,
                maximum: self.maximum_bytes,
            });
        }
        self.line_number = self
            .line_number
            .checked_add(1)
            .ok_or(PredictionJournalError::Counter)?;
        let had_newline = self.line.ends_with(b"\n");
        let record_bytes: &[u8] = match self.frame_kind {
            crate::journal_core::FrameKind::BareV4 => {
                if !had_newline {
                    return Err(PredictionJournalError::TruncatedLine);
                }
                trim_newline(&self.line)
            }
            crate::journal_core::FrameKind::EnvelopedV5 => {
                // A schema-5 line without a terminating newline is a torn tail even
                // if its bytes happen to parse and verify: the write did not
                // complete. This mirrors the bare-schema-4 truncation guard above.
                if !had_newline {
                    return Err(PredictionJournalError::TornTail);
                }
                let is_final = !crate::journal_core::more_bytes_follow(&mut self.reader)
                    .map_err(PredictionJournalError::Io)?;
                match crate::journal_core::enveloped_record_bytes(
                    trim_newline(&self.line),
                    is_final,
                    self.line_number,
                ) {
                    Ok(bytes) => bytes,
                    Err(crate::journal_core::FramingError::TornTail) => {
                        return Err(PredictionJournalError::TornTail);
                    }
                    Err(crate::journal_core::FramingError::Corruption { line }) => {
                        return Err(PredictionJournalError::Corruption { line });
                    }
                }
            }
        };
        let record = serde_json::from_slice::<PredictionJournalRecord>(record_bytes)?;
        match record {
            PredictionJournalRecord::Discovery { .. }
            | PredictionJournalRecord::Bind { .. }
            | PredictionJournalRecord::MarketEvents { .. }
            | PredictionJournalRecord::SpotEvents { .. }
            | PredictionJournalRecord::Unavailable { .. }
            | PredictionJournalRecord::HealthTick { .. }
            | PredictionJournalRecord::Connectivity { .. } => {
                let input = record.into_input().ok_or_else(|| {
                    PredictionJournalError::Invalid(
                        "prediction input record could not be decoded".to_owned(),
                    )
                })?;
                if input.admission_seq() != self.next_admission_seq {
                    return Err(PredictionJournalError::Sequence {
                        expected: self.next_admission_seq,
                        actual: input.admission_seq(),
                    });
                }
                input.validate(self.session.venue)?;
                if self
                    .last_now_ns
                    .is_some_and(|previous| input.now_ns() < previous)
                {
                    return Err(PredictionJournalError::ClockRegression);
                }
                self.last_now_ns = Some(input.now_ns());
                self.next_admission_seq = self
                    .next_admission_seq
                    .checked_add(1)
                    .ok_or(PredictionJournalError::Counter)?;
                Ok(Some(input))
            }
            PredictionJournalRecord::End { end } => {
                if end.totals.admitted_inputs != self.next_admission_seq {
                    return Err(PredictionJournalError::Invalid(
                        "journal end total disagrees with replayed input count".to_owned(),
                    ));
                }
                self.end = Some(end);
                self.ensure_eof()?;
                Ok(None)
            }
            PredictionJournalRecord::PredictionSession { .. } => Err(
                PredictionJournalError::Invalid("journal contains a second session".to_owned()),
            ),
        }
    }

    pub fn end(&self) -> Option<&PredictionJournalEnd> {
        self.end.as_ref()
    }

    fn ensure_eof(&mut self) -> Result<(), PredictionJournalError> {
        self.line.clear();
        let bytes = read_bounded_line(&mut self.reader, &mut self.line)?;
        if bytes == 0 {
            return Ok(());
        }
        // Legacy parity: before framing, trailing bytes without a terminating
        // newline were reported as a truncated line (the bound reader checked the
        // newline), while a complete extra line was records-after-end. Preserve
        // both distinctions.
        if !self.line.ends_with(b"\n") {
            return Err(PredictionJournalError::TruncatedLine);
        }
        Err(PredictionJournalError::Invalid(
            "journal contains records after end".to_owned(),
        ))
    }
}

fn open_owner_only(path: &Path) -> Result<File, PredictionJournalError> {
    let path_metadata = std::fs::symlink_metadata(path)?;
    if path_metadata.file_type().is_symlink() || !path_metadata.is_file() {
        return Err(PredictionJournalError::UnsafeTarget(
            "journal must be a regular non-symlink file".to_owned(),
        ));
    }
    #[cfg(unix)]
    if path_metadata.mode() & 0o077 != 0 {
        return Err(PredictionJournalError::UnsafeTarget(format!(
            "journal permissions must deny group and other access; found {:03o}",
            path_metadata.mode() & 0o777
        )));
    }
    let file = File::open(path)?;
    #[cfg(unix)]
    {
        let metadata = file.metadata()?;
        if metadata.dev() != path_metadata.dev() || metadata.ino() != path_metadata.ino() {
            return Err(PredictionJournalError::UnsafeTarget(
                "journal target changed while opening".to_owned(),
            ));
        }
    }
    Ok(file)
}

/// Read one line into `line` (keeping any trailing newline) and return the byte
/// count. A missing terminating newline is not rejected here: the caller decides
/// from the framing mode whether a truncated final line is a torn tail (schema
/// 5) or a truncated bare record (schema 4).
fn read_bounded_line(
    reader: &mut BufReader<File>,
    line: &mut Vec<u8>,
) -> Result<usize, PredictionJournalError> {
    let bytes = reader
        .take((MAX_PREDICTION_JOURNAL_LINE_BYTES as u64) + 1)
        .read_until(b'\n', line)?;
    if line.len() > MAX_PREDICTION_JOURNAL_LINE_BYTES {
        return Err(PredictionJournalError::LineLimit(
            MAX_PREDICTION_JOURNAL_LINE_BYTES,
        ));
    }
    Ok(bytes)
}

fn trim_newline(line: &[u8]) -> &[u8] {
    line.strip_suffix(b"\n").unwrap_or(line)
}

fn hash_component(
    aggregate: &mut Sha256,
    id: &str,
    bytes: &[u8],
) -> Result<(), PredictionJournalError> {
    aggregate.update(
        u64::try_from(id.len())
            .map_err(|_| PredictionJournalError::Counter)?
            .to_be_bytes(),
    );
    aggregate.update(id.as_bytes());
    aggregate.update(
        u64::try_from(bytes.len())
            .map_err(|_| PredictionJournalError::Counter)?
            .to_be_bytes(),
    );
    aggregate.update(bytes);
    Ok(())
}

fn validate_digest(name: &str, value: &str) -> Result<(), PredictionJournalError> {
    if value.len() != 64 || !is_lower_hex(value) {
        return Err(PredictionJournalError::Invalid(format!(
            "{name} must be a lowercase SHA-256 digest"
        )));
    }
    Ok(())
}

/// Test-only: write a CRC-enveloped prediction journal stamped at an arbitrary
/// schema version. It reproduces the enveloped framing the current writer emits
/// (`{"c":<crc>,"r":<record>}` per line) so a retained-version fixture (for
/// example the CRC-framed schema 5 that predates the socket-connectivity variant)
/// can be crafted and replayed through the current binary. The `PredictionJournalRecord`
/// shape is module-private, so this lives beside it rather than in a test module.
#[cfg(test)]
pub(crate) fn write_enveloped_prediction_journal_at_version(
    path: &Path,
    mut session: PredictionJournalSession,
    inputs: &[PredictionJournalInput],
    end: PredictionJournalEnd,
    schema_version: u16,
) -> std::io::Result<()> {
    use std::io::Write as _;

    session.journal_schema_version = schema_version;
    let mut records = vec![PredictionJournalRecord::PredictionSession {
        session: Box::new(session),
    }];
    for input in inputs {
        records.push(PredictionJournalRecord::from(input.clone()));
    }
    records.push(PredictionJournalRecord::End { end });

    let mut bytes = Vec::new();
    for record in &records {
        let record_json = serde_json::to_string(record).expect("record serializes");
        let crc = crate::crc32c::crc32c_hex(record_json.as_bytes());
        writeln!(bytes, "{{\"c\":\"{crc}\",\"r\":{record_json}}}").expect("line buffers");
    }
    std::fs::write(path, &bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum PredictionJournalError {
    #[error("unsupported prediction journal version {0}")]
    UnsupportedVersion(u16),
    #[error("invalid prediction journal: {0}")]
    Invalid(String),
    #[error(
        "prediction journal contract fingerprint mismatch: expected {expected}, found {actual}"
    )]
    FingerprintMismatch { expected: String, actual: String },
    #[error("prediction journal admission sequence mismatch: expected {expected}, found {actual}")]
    Sequence { expected: u64, actual: u64 },
    #[error("prediction journal logical time regressed")]
    ClockRegression,
    #[error("prediction journal exceeds {maximum} bytes at {attempted} bytes")]
    SizeLimit { attempted: u64, maximum: u64 },
    #[error("prediction journal line exceeds {0} bytes")]
    LineLimit(usize),
    #[error("prediction journal ends with a truncated JSONL record")]
    TruncatedLine,
    #[error(
        "prediction journal free space {available} bytes is below the {minimum}-byte watermark; run stopped"
    )]
    LowDiskSpace { available: u64, minimum: u64 },
    #[error(
        "prediction journal tail is torn: the final record failed to parse or its CRC did not match"
    )]
    TornTail,
    #[error(
        "prediction journal record on line {line} is corrupt: it failed to parse or its CRC did not match"
    )]
    Corruption { line: u64 },
    #[error("prediction journal has no end record")]
    MissingEnd,
    #[error("prediction journal queue is full")]
    QueueFull,
    #[error("prediction journal writer stopped")]
    WriterStopped,
    #[error("prediction journal writer thread panicked")]
    WriterPanicked,
    #[error("unsafe prediction journal target: {0}")]
    UnsafeTarget(String),
    #[error("prediction journal counter exhausted")]
    Counter,
    #[error("prediction journal I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("prediction journal JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("prediction journal entropy failed: {0}")]
    Entropy(getrandom::Error),
}

impl From<crate::journal_core::JournalCoreError> for PredictionJournalError {
    fn from(error: crate::journal_core::JournalCoreError) -> Self {
        use crate::journal_core::JournalCoreError;
        match error {
            JournalCoreError::Io(error) => PredictionJournalError::Io(error),
            JournalCoreError::Json(error) => PredictionJournalError::Json(error),
            JournalCoreError::LineTooLarge { maximum } => {
                PredictionJournalError::LineLimit(maximum)
            }
            JournalCoreError::SizeLimit { attempted, maximum } => {
                PredictionJournalError::SizeLimit { attempted, maximum }
            }
            JournalCoreError::LowDiskSpace { available, minimum } => {
                PredictionJournalError::LowDiskSpace { available, minimum }
            }
            JournalCoreError::Counter => PredictionJournalError::Counter,
            JournalCoreError::QueueFull => PredictionJournalError::QueueFull,
            JournalCoreError::WriterStopped => PredictionJournalError::WriterStopped,
            JournalCoreError::InputAfterTerminal => PredictionJournalError::Invalid(
                "prediction journal input followed the terminal record".to_owned(),
            ),
            JournalCoreError::MultipleTerminal => PredictionJournalError::Invalid(
                "prediction journal contains more than one terminal record".to_owned(),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use btc_feeds::prediction_live::{
        PredictionDiscoveryBodyEncoding, PredictionDiscoveryResult, PredictionDiscoveryStage,
    };

    use super::*;

    #[test]
    fn v2_prediction_schema_map_and_instance_schema_mutation_are_refused() {
        let mut config = PredictionRuntimeConfig::new(PredictionMarketVenue::Kalshi, 2_000);
        config.feed_health_stream = false;
        let current = prediction_contract_fingerprint(&config).expect("v3 fingerprint builds");
        let mut v2 = current.clone();
        assert!(v2.runtime_schema_sha256.remove("instance-status").is_some());
        assert!(matches!(
            v2.validate(&config),
            Err(PredictionJournalError::Invalid(message)) if message.contains("complete runtime-schema set")
        ));

        let mut mutated = current;
        mutated
            .runtime_schema_sha256
            .insert("instance-status".to_owned(), sha256_hex(b"mutated schema"));
        assert!(matches!(
            mutated.validate(&config),
            Err(PredictionJournalError::FingerprintMismatch { .. })
        ));
    }

    #[test]
    fn resume_upgrades_capability_and_keeps_the_embedded_checkpoint_gated() {
        use chrono::Utc;

        use crate::PredictionSignalProcessor;

        let wall_start = DateTime::parse_from_rfc3339("2026-08-15T12:00:00Z")
            .expect("wall start parses")
            .with_timezone(&Utc);
        let mut disabled = PredictionRuntimeConfig::new(PredictionMarketVenue::Kalshi, 2_000);
        disabled.feed_health_stream = false;
        let processor =
            PredictionSignalProcessor::new(PredictionMarketVenue::Kalshi, "e1", wall_start)
                .expect("processor builds");
        let checkpoint = PredictionCheckpointDocument::new(
            "0".repeat(32),
            "2026-08-15T12:00:00Z".to_owned(),
            100,
            5,
            prediction_contract_fingerprint(&disabled).expect("fingerprint builds"),
            disabled.clone(),
            processor.checkpoint_state(),
            vec!["e1".to_owned()],
        )
        .expect("pre-capability checkpoint builds");

        let current = PredictionRuntimeConfig::new(PredictionMarketVenue::Kalshi, 2_000);
        assert!(current.feed_health_stream);
        let session = PredictionJournalSession::resumed("e2".to_owned(), checkpoint, current)
            .expect("resume across the capability boundary succeeds");
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
    fn retained_v4_prediction_journal_replays_without_framing() {
        let path = temporary_test_path("retained-v4");
        let mut session = PredictionJournalSession::new(
            "2026-08-15T12:00:00Z".to_owned(),
            "retained-v4".to_owned(),
            PredictionRuntimeConfig::new(PredictionMarketVenue::Kalshi, 2_000),
        )
        .expect("session builds");
        // Retained evidence predates CRC framing and carries schema 4.
        session.journal_schema_version = RETAINED_PREDICTION_JOURNAL_SCHEMA_VERSION;

        let tick = PredictionJournalInput::HealthTick {
            admission_seq: 0,
            now_ns: 10,
        };
        let end = PredictionJournalEnd {
            totals: PredictionJournalTotals {
                admitted_inputs: 1,
                discovery_attempts: 0,
                bindings: 0,
                market_event_batches: 0,
                spot_event_batches: 0,
                unavailable_transitions: 0,
                health_ticks: 1,
                updates_written: 0,
                stream_messages_written: 0,
            },
            quality_reason_entries: BTreeMap::new(),
        };

        // Serialize each record as a bare JSONL line, exactly as the pre-CRC
        // binary wrote schema 4: no envelope, no CRC.
        let mut bytes = Vec::new();
        for record in [
            PredictionJournalRecord::PredictionSession {
                session: Box::new(session.clone()),
            },
            PredictionJournalRecord::from(tick.clone()),
            PredictionJournalRecord::End { end: end.clone() },
        ] {
            serde_json::to_writer(&mut bytes, &record).expect("record serializes");
            bytes.push(b'\n');
        }
        std::fs::write(&path, &bytes).expect("fixture writes");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .expect("fixture permissions set");
        }

        let mut reader =
            PredictionJournalReplayReader::open(&path, 1_048_576).expect("v4 journal opens");
        assert_eq!(
            reader.session().journal_schema_version,
            RETAINED_PREDICTION_JOURNAL_SCHEMA_VERSION
        );
        assert_eq!(reader.next_input().expect("tick reads"), Some(tick));
        assert!(reader.next_input().expect("terminal reads").is_none());
        assert_eq!(reader.end(), Some(&end));
        std::fs::remove_file(path).expect("fixture removes");
    }

    #[test]
    fn retained_v4_session_without_trailing_newline_fails_open_as_truncated() {
        let path = temporary_test_path("retained-v4-no-newline");
        let mut session = PredictionJournalSession::new(
            "2026-08-15T12:00:00Z".to_owned(),
            "retained-v4".to_owned(),
            PredictionRuntimeConfig::new(PredictionMarketVenue::Kalshi, 2_000),
        )
        .expect("session builds");
        session.journal_schema_version = RETAINED_PREDICTION_JOURNAL_SCHEMA_VERSION;

        // A bare schema-4 session line with NO trailing newline. This must fail at
        // open with the legacy TruncatedLine error, byte-for-byte the pre-framing
        // behavior, not open and later surface MissingEnd.
        let bytes = serde_json::to_vec(&PredictionJournalRecord::PredictionSession {
            session: Box::new(session),
        })
        .expect("session serializes");
        std::fs::write(&path, &bytes).expect("fixture writes");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .expect("fixture permissions set");
        }
        assert!(matches!(
            PredictionJournalReplayReader::open(&path, 1_048_576),
            Err(PredictionJournalError::TruncatedLine)
        ));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn writer_refuses_to_stamp_a_retained_schema_version() {
        let path = temporary_test_path("reject-v4-write");
        let mut session = PredictionJournalSession::new(
            "2026-08-15T12:00:00Z".to_owned(),
            "reject-v4-write".to_owned(),
            PredictionRuntimeConfig::new(PredictionMarketVenue::Kalshi, 2_000),
        )
        .expect("session builds");
        session.journal_schema_version = RETAINED_PREDICTION_JOURNAL_SCHEMA_VERSION;

        assert!(matches!(
            PredictionJournalWriter::create(&path, session, 1_048_576),
            Err(PredictionJournalError::Invalid(_))
        ));
        assert!(
            !path.exists(),
            "the writer rejects before creating the journal file"
        );
    }

    #[test]
    fn schema_six_prediction_journal_refuses_via_preflight() {
        let path = temporary_test_path("reject-v6");
        let session = PredictionJournalSession::new(
            "2026-08-15T12:00:00Z".to_owned(),
            "reject-v6".to_owned(),
            PredictionRuntimeConfig::new(PredictionMarketVenue::Kalshi, 2_000),
        )
        .expect("session builds");
        let end = PredictionJournalEnd {
            totals: PredictionJournalTotals {
                admitted_inputs: 0,
                discovery_attempts: 0,
                bindings: 0,
                market_event_batches: 0,
                spot_event_batches: 0,
                unavailable_transitions: 0,
                health_ticks: 0,
                updates_written: 0,
                stream_messages_written: 0,
            },
            quality_reason_entries: BTreeMap::new(),
        };
        // Schema 6 is not in the accepted set {current 8, enveloped 5, bare 4}: it
        // sits between the current schema and its retained predecessor. An enveloped
        // journal stamped at 6 must be refused at open by the version preflight,
        // never misparsed against the current record shapes.
        write_enveloped_prediction_journal_at_version(&path, session, &[], end, 6)
            .expect("v6 fixture writes");
        assert!(matches!(
            PredictionJournalReplayReader::open(&path, 1_048_576),
            Err(PredictionJournalError::UnsupportedVersion(6))
        ));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn schema_seven_connectivity_without_transport_refuses_via_preflight() {
        use std::io::Write as _;

        let path = temporary_test_path("reject-v7-connectivity");
        let mut session = PredictionJournalSession::new(
            "2026-08-15T12:00:00Z".to_owned(),
            "reject-v7-connectivity".to_owned(),
            PredictionRuntimeConfig::new(PredictionMarketVenue::Polymarket, 2_000),
        )
        .expect("session builds");
        session.journal_schema_version = 7;
        let records = [
            serde_json::to_value(PredictionJournalRecord::PredictionSession {
                session: Box::new(session),
            })
            .expect("session serializes"),
            serde_json::json!({
                "record_type": "connectivity",
                "admission_seq": 0,
                "transition": "disconnected",
                "now_ns": 10
            }),
        ];
        let mut bytes = Vec::new();
        for record in records {
            let record_json = serde_json::to_string(&record).expect("record serializes");
            let crc = crate::crc32c::crc32c_hex(record_json.as_bytes());
            writeln!(bytes, "{{\"c\":\"{crc}\",\"r\":{record_json}}}").expect("line buffers");
        }
        std::fs::write(&path, bytes).expect("v7 fixture writes");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .expect("fixture permissions set");
        }

        assert!(matches!(
            PredictionJournalReplayReader::open(&path, 1_048_576),
            Err(PredictionJournalError::UnsupportedVersion(7))
        ));
        std::fs::remove_file(path).expect("fixture removes");
    }

    #[test]
    fn connectivity_input_round_trips_through_writer_and_reader() {
        let path = temporary_test_path("connectivity-round-trip");
        let session = PredictionJournalSession::new(
            "2026-08-15T12:00:00Z".to_owned(),
            "connectivity-round-trip".to_owned(),
            PredictionRuntimeConfig::new(PredictionMarketVenue::Polymarket, 2_000),
        )
        .expect("session builds");
        let inputs = [
            PredictionJournalInput::Connectivity {
                admission_seq: 0,
                transport: PredictionTransport::Coinbase,
                transition: PredictionSocketTransition::Disconnected,
                now_ns: 10,
            },
            PredictionJournalInput::Connectivity {
                admission_seq: 1,
                transport: PredictionTransport::Kraken,
                transition: PredictionSocketTransition::Reconnecting,
                now_ns: 20,
            },
            PredictionJournalInput::Connectivity {
                admission_seq: 2,
                transport: PredictionTransport::PolymarketRtds,
                transition: PredictionSocketTransition::Connected,
                now_ns: 30,
            },
        ];
        let mut writer =
            PredictionJournalWriter::create(&path, session, DEFAULT_PREDICTION_JOURNAL_MAX_BYTES)
                .expect("journal creates");
        for input in &inputs {
            writer
                .enqueue(input.clone())
                .expect("connectivity enqueues");
        }
        let end = PredictionJournalEnd {
            totals: PredictionJournalTotals {
                admitted_inputs: 3,
                discovery_attempts: 0,
                bindings: 0,
                market_event_batches: 0,
                spot_event_batches: 0,
                unavailable_transitions: 0,
                health_ticks: 0,
                updates_written: 0,
                stream_messages_written: 0,
            },
            quality_reason_entries: BTreeMap::new(),
        };
        writer.finish(end.clone()).expect("journal finishes");

        let mut reader =
            PredictionJournalReplayReader::open(&path, DEFAULT_PREDICTION_JOURNAL_MAX_BYTES)
                .expect("journal opens");
        assert_eq!(
            reader.session().journal_schema_version,
            PREDICTION_JOURNAL_SCHEMA_VERSION
        );
        for input in &inputs {
            assert_eq!(
                reader.next_input().expect("connectivity reads").as_ref(),
                Some(input)
            );
        }
        assert!(reader.next_input().expect("terminal reads").is_none());
        assert_eq!(reader.end(), Some(&end));
        let unknown_transport = serde_json::to_string(&inputs[0])
            .expect("connectivity serializes")
            .replace("\"transport\":\"coinbase\"", "\"transport\":\"unknown\"");
        assert!(
            serde_json::from_str::<PredictionJournalInput>(&unknown_transport).is_err(),
            "transport ownership is a closed contract"
        );
        std::fs::remove_file(path).expect("fixture removes");
    }

    #[test]
    fn connectivity_transport_is_fail_closed_by_venue() {
        let invalid = PredictionJournalInput::Connectivity {
            admission_seq: 0,
            transport: PredictionTransport::PolymarketRtds,
            transition: PredictionSocketTransition::Disconnected,
            now_ns: 10,
        };
        assert!(invalid.validate(PredictionMarketVenue::Kalshi).is_err());
        for transport in [
            PredictionTransport::Kalshi,
            PredictionTransport::Coinbase,
            PredictionTransport::Kraken,
        ] {
            let valid = PredictionJournalInput::Connectivity {
                admission_seq: 0,
                transport,
                transition: PredictionSocketTransition::Disconnected,
                now_ns: 10,
            };
            valid
                .validate(PredictionMarketVenue::Kalshi)
                .expect("Kalshi-owned transport validates");
        }
    }

    #[test]
    fn retained_v5_enveloped_journal_reads_through_the_current_binary() {
        // A CRC-framed schema-5 journal (the version tonight's soak writes)
        // predates the socket-connectivity variant. It must still open and replay
        // through the current binary: enveloped framing is shared, and only the
        // recorded version distinguishes 5 from 6.
        let path = temporary_test_path("retained-v5");
        let session = PredictionJournalSession::new(
            "2026-08-15T12:00:00Z".to_owned(),
            "retained-v5".to_owned(),
            PredictionRuntimeConfig::new(PredictionMarketVenue::Kalshi, 2_000),
        )
        .expect("session builds");
        let tick = PredictionJournalInput::HealthTick {
            admission_seq: 0,
            now_ns: 10,
        };
        let end = PredictionJournalEnd {
            totals: PredictionJournalTotals {
                admitted_inputs: 1,
                discovery_attempts: 0,
                bindings: 0,
                market_event_batches: 0,
                spot_event_batches: 0,
                unavailable_transitions: 0,
                health_ticks: 1,
                updates_written: 0,
                stream_messages_written: 0,
            },
            quality_reason_entries: BTreeMap::new(),
        };
        write_enveloped_prediction_journal_at_version(
            &path,
            session,
            std::slice::from_ref(&tick),
            end.clone(),
            RETAINED_ENVELOPED_PREDICTION_JOURNAL_SCHEMA_VERSION,
        )
        .expect("v5 fixture writes");

        let mut reader =
            PredictionJournalReplayReader::open(&path, DEFAULT_PREDICTION_JOURNAL_MAX_BYTES)
                .expect("v5 journal opens through the current binary");
        assert_eq!(
            reader.session().journal_schema_version,
            RETAINED_ENVELOPED_PREDICTION_JOURNAL_SCHEMA_VERSION
        );
        assert_eq!(reader.next_input().expect("tick reads"), Some(tick));
        assert!(reader.next_input().expect("terminal reads").is_none());
        assert_eq!(reader.end(), Some(&end));
        std::fs::remove_file(path).expect("fixture removes");
    }

    /// Write a complete current-schema prediction journal of `count` health ticks and
    /// return its path, for corruption/truncation fixtures.
    fn journal_with_health_ticks(label: &str, count: u64) -> std::path::PathBuf {
        let path = temporary_test_path(label);
        let session = PredictionJournalSession::new(
            "2026-08-15T12:00:00Z".to_owned(),
            format!("prediction-{label}"),
            PredictionRuntimeConfig::new(PredictionMarketVenue::Kalshi, 2_000),
        )
        .expect("session builds");
        let mut writer =
            PredictionJournalWriter::create(&path, session, DEFAULT_PREDICTION_JOURNAL_MAX_BYTES)
                .expect("journal creates");
        for seq in 0..count {
            writer
                .enqueue(PredictionJournalInput::HealthTick {
                    admission_seq: seq,
                    now_ns: seq + 1,
                })
                .expect("tick enqueues");
        }
        let end = PredictionJournalEnd {
            totals: PredictionJournalTotals {
                admitted_inputs: count,
                discovery_attempts: 0,
                bindings: 0,
                market_event_batches: 0,
                spot_event_batches: 0,
                unavailable_transitions: 0,
                health_ticks: count,
                updates_written: 0,
                stream_messages_written: 0,
            },
            quality_reason_entries: BTreeMap::new(),
        };
        writer.finish(end).expect("journal finishes");
        path
    }

    fn rewrite_truncating_last_line(path: &std::path::Path, keep: usize) {
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

    fn strip_trailing_newline(path: &std::path::Path) {
        let mut bytes = std::fs::read(path).expect("journal reads");
        if bytes.last() == Some(&b'\n') {
            bytes.pop();
        }
        std::fs::write(path, bytes).expect("journal rewrites");
    }

    fn flip_crc_char(path: &std::path::Path, line_index: usize) {
        let text = std::fs::read_to_string(path).expect("journal reads");
        let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
        let crc_start = lines[line_index]
            .find(r#""c":""#)
            .expect("envelope has a CRC field")
            + 5;
        let mut bytes = lines[line_index].clone().into_bytes();
        bytes[crc_start] = if bytes[crc_start] == b'0' { b'1' } else { b'0' };
        lines[line_index] = String::from_utf8(bytes).expect("mutation stays UTF-8");
        let mut out = lines.join("\n");
        out.push('\n');
        std::fs::write(path, out).expect("corrupted journal writes");
    }

    #[test]
    fn prediction_torn_final_record_is_a_torn_tail() {
        let path = journal_with_health_ticks("pred-torn-mid-record", 1);
        let last = std::fs::read_to_string(&path)
            .expect("journal reads")
            .lines()
            .last()
            .expect("final line")
            .to_owned();
        let keep = last.find(r#""r":"#).expect("envelope has a record field") + 5;
        rewrite_truncating_last_line(&path, keep);

        let mut reader =
            PredictionJournalReplayReader::open(&path, DEFAULT_PREDICTION_JOURNAL_MAX_BYTES)
                .expect("session opens");
        reader.next_input().expect("intact tick reads");
        assert!(matches!(
            reader.next_input(),
            Err(PredictionJournalError::TornTail)
        ));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn prediction_valid_final_record_without_a_newline_is_a_torn_tail() {
        let path = journal_with_health_ticks("pred-no-terminal-newline", 1);
        strip_trailing_newline(&path);

        let mut reader =
            PredictionJournalReplayReader::open(&path, DEFAULT_PREDICTION_JOURNAL_MAX_BYTES)
                .expect("session opens");
        reader.next_input().expect("intact tick reads");
        assert!(matches!(
            reader.next_input(),
            Err(PredictionJournalError::TornTail)
        ));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn prediction_interior_crc_mismatch_is_corruption_naming_its_line() {
        let path = journal_with_health_ticks("pred-interior-corruption", 2);
        // Corrupt the CRC of the first data record (line 2) while later records
        // remain, so the failure is interior corruption, not a torn tail.
        flip_crc_char(&path, 1);

        let mut reader =
            PredictionJournalReplayReader::open(&path, DEFAULT_PREDICTION_JOURNAL_MAX_BYTES)
                .expect("session opens");
        assert!(matches!(
            reader.next_input(),
            Err(PredictionJournalError::Corruption { line: 2 })
        ));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn zero_byte_prediction_journal_open_surfaces_a_json_error() {
        let path = temporary_test_path("pred-zero-byte");
        std::fs::write(&path, b"").expect("empty file writes");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .expect("fixture permissions set");
        }
        assert!(matches!(
            PredictionJournalReplayReader::open(&path, 1_048_576),
            Err(PredictionJournalError::Json(_))
        ));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn junk_without_a_newline_after_end_is_a_truncated_line() {
        let path = journal_with_health_ticks("pred-junk-after-end", 1);
        let mut bytes = std::fs::read(&path).expect("journal reads");
        bytes.extend_from_slice(br#"{"garbage""#); // trailing bytes, no newline
        std::fs::write(&path, bytes).expect("journal rewrites");

        let mut reader =
            PredictionJournalReplayReader::open(&path, DEFAULT_PREDICTION_JOURNAL_MAX_BYTES)
                .expect("session opens");
        reader.next_input().expect("intact tick reads");
        assert!(matches!(
            reader.next_input(),
            Err(PredictionJournalError::TruncatedLine)
        ));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn direct_input_records_round_trip_through_the_streaming_reader() {
        let path = temporary_test_path("roundtrip");
        let session = PredictionJournalSession::new(
            "2026-08-15T12:00:00Z".to_owned(),
            "prediction-journal-test".to_owned(),
            PredictionRuntimeConfig::new(PredictionMarketVenue::Polymarket, 2_000),
        )
        .expect("session builds");
        let body = "{}".to_owned();
        let input = PredictionJournalInput::Discovery {
            admission_seq: 0,
            generation: 1,
            attempt: PredictionDiscoveryAttempt {
                schema_version: 1,
                venue: PredictionMarketVenue::Polymarket,
                stage: PredictionDiscoveryStage::ExactMarket,
                request_path: "/markets?slug=test&closed=false".to_owned(),
                retrieved_at: "2026-08-15T12:00:00Z".to_owned(),
                http_status: Some(200),
                response_body_encoding: PredictionDiscoveryBodyEncoding::Utf8,
                response_body_sha256: sha256_hex(body.as_bytes()),
                response_body: body,
                result: PredictionDiscoveryResult::Invalid,
                selected_market_id: None,
            },
            now_ns: 0,
        };
        let end = PredictionJournalEnd {
            totals: PredictionJournalTotals {
                admitted_inputs: 1,
                discovery_attempts: 1,
                bindings: 0,
                market_event_batches: 0,
                spot_event_batches: 0,
                unavailable_transitions: 0,
                health_ticks: 0,
                updates_written: 0,
                stream_messages_written: 0,
            },
            quality_reason_entries: BTreeMap::new(),
        };
        let mut writer = PredictionJournalWriter::create(&path, session.clone(), 1_048_576)
            .expect("journal creates");
        writer.enqueue(input.clone()).expect("input enqueues");
        let report = writer.finish(end.clone()).expect("journal finishes");
        assert_eq!(report.records_written, 3);
        assert_eq!(report.admitted_inputs, 1);
        assert!(report.data_syncs >= 2);

        let mut reader =
            PredictionJournalReplayReader::open(&path, 1_048_576).expect("journal opens");
        assert_eq!(reader.session(), &session);
        assert_eq!(reader.next_input().expect("input reads"), Some(input));
        assert_eq!(reader.next_input().expect("end reads"), None);
        assert_eq!(reader.end(), Some(&end));
        std::fs::remove_file(path).expect("journal removes");
    }

    #[test]
    fn polymarket_recovery_bind_uses_the_public_market_identity() {
        let mut input = PredictionJournalInput::Bind {
            admission_seq: 0,
            generation: 2,
            previous_market_id: Some("gamma-market-42".to_owned()),
            reason: MarketTransitionReason::RecoveryBind,
            event: SourceEvent {
                schema_version: 1,
                source_id: "polymarket.btc.market-contracts".to_owned(),
                source_version: "1.0.0".to_owned(),
                publisher_epoch: "prediction-journal-recovery-test".to_owned(),
                sequence: 0,
                event_time: "2026-08-15T12:00:00Z".to_owned(),
                received_at: "2026-08-15T12:00:00Z".to_owned(),
                values: BTreeMap::from([
                    ("market_id".to_owned(), serde_json::json!("gamma-market-42")),
                    ("condition_id".to_owned(), serde_json::json!("0xcondition")),
                ]),
            },
            logical_receipt_ns: 1,
            now_ns: 1,
        };
        input
            .validate(PredictionMarketVenue::Polymarket)
            .expect("same Gamma market identity is a valid recovery bind");

        if let PredictionJournalInput::Bind {
            previous_market_id, ..
        } = &mut input
        {
            *previous_market_id = Some("gamma-market-41".to_owned());
        }
        input
            .validate(PredictionMarketVenue::Polymarket)
            .expect("recovery may bind the successor missed while the process was down");

        if let PredictionJournalInput::Bind {
            previous_market_id, ..
        } = &mut input
        {
            *previous_market_id = None;
        }
        assert!(input.validate(PredictionMarketVenue::Polymarket).is_err());
    }

    #[test]
    fn bounded_writer_queue_reports_backpressure_and_disconnect() {
        let (sender, receiver) = mpsc::sync_channel(1);
        try_enqueue_prediction(
            &sender,
            PredictionWriterMessage::Input(PredictionJournalInput::HealthTick {
                admission_seq: 0,
                now_ns: 0,
            }),
        )
        .expect("first queue slot accepts");
        assert!(matches!(
            try_enqueue_prediction(
                &sender,
                PredictionWriterMessage::Input(PredictionJournalInput::HealthTick {
                    admission_seq: 1,
                    now_ns: 1,
                }),
            ),
            Err(PredictionJournalError::QueueFull)
        ));
        drop(receiver);
        assert!(matches!(
            try_enqueue_prediction(
                &sender,
                PredictionWriterMessage::Input(PredictionJournalInput::HealthTick {
                    admission_seq: 1,
                    now_ns: 1,
                }),
            ),
            Err(PredictionJournalError::WriterStopped)
        ));
    }

    #[test]
    fn idle_writer_periodically_syncs_its_durable_prefix() {
        let path = temporary_test_path("periodic-sync");
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        options.mode(0o600);
        let file = options.open(&path).expect("test journal creates");
        let mut writer = BufWriter::new(file);
        let mut state = PredictionWriterState::new(1_048_576);
        state.last_sync_at = Instant::now()
            .checked_sub(PREDICTION_JOURNAL_SYNC_INTERVAL)
            .expect("sync interval fits");

        state
            .sync_if_due(&mut writer)
            .expect("elapsed sync interval flushes");

        assert_eq!(state.data_syncs, 1);
        drop(writer);
        std::fs::remove_file(path).expect("journal removes");
    }

    #[test]
    fn genuine_v3_shape_is_rejected_before_current_fields_are_decoded() {
        let path = temporary_test_path("version3");
        std::fs::write(
            &path,
            b"{\"record_type\":\"prediction_session\",\"journal_schema_version\":3}\n",
        )
        .expect("fixture writes");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .expect("fixture permissions set");
        }
        assert!(matches!(
            PredictionJournalReplayReader::open(&path, 1_048_576),
            Err(PredictionJournalError::UnsupportedVersion(3))
        ));
        std::fs::remove_file(path).expect("fixture removes");
    }

    /// SIG-T012d disk-exhaustion, public writer-boundary reach. Drives the real
    /// `PredictionJournalWriter` create path — file open, watermark construction,
    /// and the fail-closed `check_watermark` at open — against a real filesystem.
    /// The trailing `minimum_free_space_bytes` seam raises the threshold above the
    /// filesystem's actual free space (production fixes it at the durability
    /// constant), so the documented `LowDiskSpace` error must surface through the
    /// writer boundary.
    #[cfg(unix)]
    #[test]
    fn create_fails_closed_when_free_space_is_below_the_watermark() {
        let path = temporary_test_path("watermark-boundary");
        let session = PredictionJournalSession::new(
            "2026-08-15T12:00:00Z".to_owned(),
            "prediction-watermark-boundary".to_owned(),
            PredictionRuntimeConfig::new(PredictionMarketVenue::Polymarket, 2_000),
        )
        .expect("session builds");
        match PredictionJournalWriter::create_with_queue_capacity(
            &path,
            session,
            1_048_576,
            DEFAULT_PREDICTION_JOURNAL_QUEUE_CAPACITY,
            u64::MAX,
        ) {
            Err(PredictionJournalError::LowDiskSpace { available, minimum }) => {
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

    fn temporary_test_path(label: &str) -> std::path::PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock follows epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "prediction-journal-{label}-{}-{unique}.jsonl",
            std::process::id()
        ))
    }
}

#[cfg(test)]
mod proptests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use btc_feeds::prediction_live::{
        PredictionDiscoveryAttempt, PredictionDiscoveryBodyEncoding, PredictionDiscoveryResult,
        PredictionDiscoveryStage,
    };
    use proptest::prelude::*;

    use super::*;

    /// SHA-256 of the empty byte string, which a `None`-encoded empty discovery
    /// body must carry; hard-coded so the test never calls the crate digest.
    const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    static PROPTEST_PATH_COUNTER: AtomicU64 = AtomicU64::new(0);

    #[derive(Debug, Clone)]
    enum InputSpec {
        Health,
        Discovery {
            generation: u64,
            path_suffix: String,
        },
        Unavailable {
            generation: u64,
            reason_pick: u8,
        },
        Bind {
            generation: u64,
            market_id: String,
            epoch: String,
        },
        Market {
            generation: u64,
            epoch: String,
            event_count: usize,
        },
        Connectivity {
            transition_pick: u8,
        },
    }

    fn input_spec() -> impl Strategy<Value = InputSpec> {
        prop_oneof![
            Just(InputSpec::Health),
            any::<u8>().prop_map(|transition_pick| InputSpec::Connectivity { transition_pick }),
            (1_u64..=1_000, "[a-z0-9-]{0,20}").prop_map(|(generation, path_suffix)| {
                InputSpec::Discovery {
                    generation,
                    path_suffix,
                }
            }),
            (1_u64..=1_000, any::<u8>()).prop_map(|(generation, reason_pick)| {
                InputSpec::Unavailable {
                    generation,
                    reason_pick,
                }
            }),
            (1_u64..=1_000, "[a-z0-9-]{1,20}", "[a-z0-9-]{1,30}").prop_map(
                |(generation, market_id, epoch)| InputSpec::Bind {
                    generation,
                    market_id,
                    epoch,
                }
            ),
            (1_u64..=1_000, "[a-z0-9-]{1,30}", 1_usize..=3).prop_map(
                |(generation, epoch, event_count)| InputSpec::Market {
                    generation,
                    epoch,
                    event_count,
                }
            ),
        ]
    }

    fn socket_transition(pick: u8) -> PredictionSocketTransition {
        match pick % 3 {
            0 => PredictionSocketTransition::Disconnected,
            1 => PredictionSocketTransition::Reconnecting,
            _ => PredictionSocketTransition::Connected,
        }
    }

    fn unavailable_reason(pick: u8) -> MarketTransitionReason {
        match pick % 4 {
            0 => MarketTransitionReason::DiscoveryUnreachable,
            1 => MarketTransitionReason::NextMarketUnlisted,
            2 => MarketTransitionReason::InvalidDiscovery,
            _ => MarketTransitionReason::CloseToNextOpenGap,
        }
    }

    fn unreachable_attempt(
        venue: PredictionMarketVenue,
        suffix: &str,
    ) -> PredictionDiscoveryAttempt {
        PredictionDiscoveryAttempt {
            schema_version: 1,
            venue,
            stage: PredictionDiscoveryStage::CurrentMarket,
            request_path: format!("/discovery/{suffix}"),
            retrieved_at: "2026-08-15T12:00:00Z".to_owned(),
            http_status: None,
            response_body_encoding: PredictionDiscoveryBodyEncoding::None,
            response_body: String::new(),
            response_body_sha256: EMPTY_SHA256.to_owned(),
            result: PredictionDiscoveryResult::Unreachable,
            selected_market_id: None,
        }
    }

    fn source_event(venue: PredictionMarketVenue, market_id: &str, epoch: &str) -> SourceEvent {
        let market_field = match venue {
            PredictionMarketVenue::Polymarket => "market_id",
            PredictionMarketVenue::Kalshi => "market_ticker",
        };
        SourceEvent {
            schema_version: 1,
            source_id: "prediction.proptest".to_owned(),
            source_version: "1.0.0".to_owned(),
            publisher_epoch: epoch.to_owned(),
            sequence: 0,
            event_time: "2026-08-15T12:00:00Z".to_owned(),
            received_at: "2026-08-15T12:00:00Z".to_owned(),
            values: BTreeMap::from([(
                market_field.to_owned(),
                serde_json::Value::String(market_id.to_owned()),
            )]),
        }
    }

    fn market_event(epoch: &str, index: usize) -> SourceEvent {
        SourceEvent {
            schema_version: 1,
            source_id: "prediction.proptest".to_owned(),
            source_version: "1.0.0".to_owned(),
            publisher_epoch: epoch.to_owned(),
            sequence: index as u64,
            event_time: "2026-08-15T12:00:00Z".to_owned(),
            received_at: "2026-08-15T12:00:00Z".to_owned(),
            values: BTreeMap::new(),
        }
    }

    /// Assemble validated inputs with sequential admission and monotonic clock,
    /// plus the matching end totals, so the sequence writes and replays cleanly.
    fn assemble(
        venue: PredictionMarketVenue,
        specs: &[(InputSpec, u64)],
    ) -> (Vec<PredictionJournalInput>, PredictionJournalEnd) {
        let mut now: u64 = 0;
        let mut inputs = Vec::with_capacity(specs.len());
        let mut totals = PredictionJournalTotals {
            admitted_inputs: 0,
            discovery_attempts: 0,
            bindings: 0,
            market_event_batches: 0,
            spot_event_batches: 0,
            unavailable_transitions: 0,
            health_ticks: 0,
            updates_written: 0,
            stream_messages_written: 0,
        };
        for (index, (spec, delta)) in specs.iter().enumerate() {
            now = now.saturating_add(*delta);
            let admission_seq = index as u64;
            let input = match spec {
                InputSpec::Health => {
                    totals.health_ticks += 1;
                    PredictionJournalInput::HealthTick {
                        admission_seq,
                        now_ns: now,
                    }
                }
                InputSpec::Discovery {
                    generation,
                    path_suffix,
                } => {
                    totals.discovery_attempts += 1;
                    PredictionJournalInput::Discovery {
                        admission_seq,
                        generation: *generation,
                        attempt: unreachable_attempt(venue, path_suffix),
                        now_ns: now,
                    }
                }
                InputSpec::Unavailable {
                    generation,
                    reason_pick,
                } => {
                    totals.unavailable_transitions += 1;
                    PredictionJournalInput::Unavailable {
                        admission_seq,
                        generation: *generation,
                        previous_market_id: None,
                        reason: unavailable_reason(*reason_pick),
                        now_ns: now,
                    }
                }
                InputSpec::Bind {
                    generation,
                    market_id,
                    epoch,
                } => {
                    totals.bindings += 1;
                    PredictionJournalInput::Bind {
                        admission_seq,
                        generation: *generation,
                        previous_market_id: None,
                        reason: MarketTransitionReason::InitialBind,
                        event: source_event(venue, market_id, epoch),
                        logical_receipt_ns: now,
                        now_ns: now,
                    }
                }
                InputSpec::Market {
                    generation,
                    epoch,
                    event_count,
                } => {
                    totals.market_event_batches += 1;
                    let events = (0..*event_count)
                        .map(|inner| market_event(epoch, inner))
                        .collect::<Vec<_>>();
                    PredictionJournalInput::MarketEvents {
                        admission_seq,
                        generation: *generation,
                        events: Arc::new(events),
                        logical_receipt_ns: now,
                        now_ns: now,
                    }
                }
                // A connectivity input contributes to `admitted_inputs` (via the
                // sequence length) but to no per-type total, matching the pipeline.
                InputSpec::Connectivity { transition_pick } => {
                    PredictionJournalInput::Connectivity {
                        admission_seq,
                        transport: PredictionTransport::for_venue(venue),
                        transition: socket_transition(*transition_pick),
                        now_ns: now,
                    }
                }
            };
            inputs.push(input);
        }
        totals.admitted_inputs = inputs.len() as u64;
        (
            inputs,
            PredictionJournalEnd {
                totals,
                quality_reason_entries: BTreeMap::new(),
            },
        )
    }

    fn proptest_journal_path() -> std::path::PathBuf {
        let unique = PROPTEST_PATH_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "prediction-journal-proptest-{}-{unique}.jsonl",
            std::process::id()
        ))
    }

    proptest! {
        #[test]
        fn journal_inputs_round_trip_through_writer_and_reader(
            venue in prop_oneof![
                Just(PredictionMarketVenue::Polymarket),
                Just(PredictionMarketVenue::Kalshi),
            ],
            discovery_retry_ms in 1_u64..=60_000,
            publisher_epoch in "[a-z0-9-]{1,40}",
            specs in proptest::collection::vec((input_spec(), 0_u64..1_000_000), 0..12),
        ) {
            let runtime_config = PredictionRuntimeConfig::new(venue, discovery_retry_ms);
            let session = PredictionJournalSession::new(
                "2026-08-15T12:00:00Z".to_owned(),
                publisher_epoch,
                runtime_config,
            )
            .expect("session builds from valid runtime configuration");
            let (inputs, end) = assemble(venue, &specs);

            let path = proptest_journal_path();
            let mut writer =
                PredictionJournalWriter::create(&path, session.clone(), 64 * 1024 * 1024)
                    .expect("journal creates");
            for input in &inputs {
                writer.enqueue(input.clone()).expect("input enqueues");
            }
            writer.finish(end.clone()).expect("journal finishes");

            let mut reader = PredictionJournalReplayReader::open(&path, 64 * 1024 * 1024)
                .expect("journal opens");
            prop_assert_eq!(reader.session(), &session);
            for expected in &inputs {
                let decoded = reader.next_input().expect("input reads");
                prop_assert_eq!(decoded.as_ref(), Some(expected));
            }
            prop_assert_eq!(reader.next_input().expect("end reads"), None);
            prop_assert_eq!(reader.end(), Some(&end));

            std::fs::remove_file(&path).expect("journal removes");
        }
    }
}
