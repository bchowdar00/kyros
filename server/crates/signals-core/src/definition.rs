use serde::{Deserialize, Serialize};

/// Lifecycle state for a machine-readable definition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleStatus {
    Draft,
    Experimental,
    Approved,
    Deprecated,
}

/// How an adapter receives data from its upstream source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    Websocket,
    HttpPoll,
    FileReplay,
    /// Server-sent events. Added additively by ADR 0026 for upstreams that
    /// stream over a long-lived HTTP response rather than a socket.
    Sse,
}

/// The scope shape a signal identity publishes under. Optional on a definition:
/// when absent it is derived from the identity's venue segment, and when present
/// it must agree with that derivation (ADR 0026).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeKind {
    Global,
    Market,
    MultiMarket,
}

impl ScopeKind {
    /// The wire spelling, identical to this type's serde representation. Used
    /// by the registry projection and the package registry, which carry the
    /// scope kind as a plain string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Market => "market",
            Self::MultiMarket => "multi_market",
        }
    }
}

/// Supported field representations at JSON boundaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldType {
    Bool,
    String,
    Decimal,
    U8,
    U16,
    U32,
    U64,
    I64,
    Timestamp,
    DurationMs,
    #[serde(rename = "enum")]
    Enumeration,
}

/// Whether a field is preserved from upstream or computed by this pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldLocation {
    External,
    Internal,
}

/// One named value in a normalized event or signal update.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FieldDefinition {
    pub name: String,
    #[serde(rename = "type")]
    pub field_type: FieldType,
    pub location: FieldLocation,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub variants: Vec<String>,
    /// Human-readable label for a card showing this field. Presentation
    /// only: the exchange's registry reads it, the processors never do.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Versioned contract emitted by one source adapter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceDefinition {
    pub schema_version: u16,
    pub source_id: String,
    pub version: String,
    pub name: String,
    pub description: String,
    pub status: LifecycleStatus,
    pub transport: Transport,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_heartbeat_ms: Option<u64>,
    pub fields: Vec<FieldDefinition>,
    pub event_time_field: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sequence_field: Option<String>,
}

/// The type of contract consumed by a signal processor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputKind {
    Source,
    Signal,
}

/// Exact source or upstream-signal version consumed by a signal.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignalInput {
    pub kind: InputKind,
    pub id: String,
    pub version: String,
}

/// Versioned contract emitted by one deterministic signal processor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignalDefinition {
    pub schema_version: u16,
    pub signal_id: String,
    pub version: String,
    pub name: String,
    pub description: String,
    pub status: LifecycleStatus,
    pub approximate_interval_ms: u64,
    pub staleness_bound_ms: u64,
    pub inputs: Vec<SignalInput>,
    pub fields: Vec<FieldDefinition>,
    pub event_time_field: String,
    /// Optional since ADR 0026. Absent means "derive it from the venue segment",
    /// which is what every definition written before 0026 relies on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope_kind: Option<ScopeKind>,
}
