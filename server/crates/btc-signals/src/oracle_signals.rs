//! The `oracle_reference` family core (B3 §5.1) and the oracle lane's Chainlink
//! processor (identity 2, `btc.chainlink.oracle_reference`).
//!
//! This is the first value-emitting [`LaneProcessor`] on the A6 shell. The
//! family core ([`OracleReferenceEmitter`] plus [`oracle_reference_values`]) is
//! the shared, venue-parameterised computation the B3 spec pins as identical
//! across `btc.cfbrti.*`, `btc.chainlink.*` and A2's `btc.pyth.*` (R11): all
//! decimal, `rust_decimal`, round-half-to-even to eight places, no binary float.
//! Brief (A) wires the Chainlink parameterisation onto the oracle lane; the CF
//! parameterisation reuses the same core on the Kalshi prediction lane in brief
//! (B).
//!
//! Contract (ADR 0027 amendment): the shell hands the processor a [`LaneClock`]
//! on every live input and every interval tick — `now_ns` (journaled logical) is
//! what the fold and the coalescing rule read; `produced_at` (wall RFC 3339) is
//! stamped onto every [`SignalUpdate`]. The processor owns the per-instance
//! decision `sequence` (restarted at zero each publisher epoch, exactly as the
//! spot pipeline does) and the coalescing memory; the shell stamps the
//! stream-message sequence and frames `signal_batch` / `instance_status`.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use chrono::{DateTime, Utc};
use rust_decimal::{Decimal, RoundingStrategy};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use signals_core::{
    FieldType, ProducerStatus, Quality, QualityState, SIGNAL_UPDATE_SCHEMA_VERSION,
    SignalDefinition, SignalScope, SignalUpdate, SourceEvent,
};

use crate::feed_health::{
    FeedAvailability, FeedCadence, FeedContract, FeedHealthBoard, FeedHealthSnapshot,
};
use crate::lane::{LaneClock, LaneDefinitionDocument, LaneProcessor, LaneProcessorError};
use crate::realized_volatility::{classify_volatility_ratio, root_mean_square};
use crate::spot_signals::{
    SpotSignalError, checked_div, decimal_value as spot_decimal_value, direction_word, return_bps,
};
use crate::stream_accounting::ProducerHealth;

/// Half-even output scale for every decimal the family publishes (B3 §5.1).
const OUTPUT_DECIMAL_PLACES: u32 = 8;

/// The oracle lane's signal-set id and version (the checkpoint processor id).
pub const ORACLE_SIGNAL_SET_ID: &str = "btc.oracle";
pub const ORACLE_SIGNAL_SET_VERSION: &str = "1.0.0";

/// The Chainlink on-chain feed the oracle lane polls (source owned by A5) and the
/// stream label the poll driver reports under.
pub const CHAINLINK_SOURCE_ID: &str = "chainlink.ethereum.btc-usd.rounds";
pub const CHAINLINK_STREAM_LABEL: &str = "chainlink_aggregator";
const CHAINLINK_SIGNAL_ID: &str = "btc.chainlink.oracle_reference";

/// The Chainlink BTC/USD aggregator's published feed parameters (B3 §5.1): a
/// 0.5 % deviation threshold OR a one-hour heartbeat, so `staleness_threshold_ms`
/// is the heartbeat plus five minutes of publication slack.
const CHAINLINK_STALENESS_THRESHOLD_MS: u64 = 3_900_000;
const CHAINLINK_HEARTBEAT_SECONDS: u32 = 3600;
const CHAINLINK_DEVIATION_THRESHOLD_BPS: u32 = 50;

const CHAINLINK_SIGNAL_DOCUMENT: &str =
    include_str!("../../../definitions/btc/signals/btc.chainlink.oracle_reference@1.0.0.json");
const CHAINLINK_SOURCE_DOCUMENT: &str =
    include_str!("../../../definitions/btc/sources/chainlink.ethereum.btc-usd.rounds@1.0.0.json");

/// The two C2 Chainlink momentum/volatility identities (E49) built live on this
/// processor's accepted-round series. They read the same accepted-round fold the
/// `oracle_reference` emit path reads (a bounded ring of accepted rounds), so the
/// three identities replay byte-identically in one process.
const CHAINLINK_MOMENTUM_SIGNAL_ID: &str = "btc.chainlink.price_momentum";
const CHAINLINK_VOLATILITY_SIGNAL_ID: &str = "btc.chainlink.price_volatility";
const CHAINLINK_MOMENTUM_DOCUMENT: &str =
    include_str!("../../../definitions/btc/signals/btc.chainlink.price_momentum@1.0.0.json");
const CHAINLINK_VOLATILITY_DOCUMENT: &str =
    include_str!("../../../definitions/btc/signals/btc.chainlink.price_volatility@1.0.0.json");

/// The not-ready reason for both C2 Chainlink identities when neither the
/// fifteen-minute time horizon nor the count horizon is satisfied (registry entry
/// 119, index 118 — appended by C2, reused here, never re-declared).
const CHAINLINK_ROUNDS_SPARSE: &str = "chainlink_rounds_sparse";
const CHAINLINK_WARMING_UP: &str = "chainlink_warming_up";

/// The C2 time horizons in milliseconds: 15 minutes, 1 hour, 4 hours.
const HORIZON_15M_MS: u64 = 15 * 60 * 1_000;
const HORIZON_1H_MS: u64 = 60 * 60 * 1_000;
const HORIZON_4H_MS: u64 = 4 * 60 * 60 * 1_000;

/// The fewest accepted rounds the ring always keeps, so the last-5-rounds count
/// horizon (newest plus five back) survives a sparse quiet period that would
/// otherwise evict older rounds from the four-hour window.
const MIN_RING_ROUNDS: usize = 6;

/// A hard upper bound on the ring so an adversarial burst of deviation-triggered
/// rounds cannot grow it without limit; far above any plausible round count in a
/// four-hour window at Chainlink's cadence.
const MAX_RING_ROUNDS: usize = 4_096;

/// The oracle lane's `(sources, signals)` definition documents for the contract
/// fingerprint. A pure function so the fingerprint recomputes from a checkpoint's
/// recorded config without a processor in hand (mirrors `lane_definition_documents`).
#[must_use]
pub fn oracle_lane_definitions() -> (Vec<LaneDefinitionDocument>, Vec<LaneDefinitionDocument>) {
    (
        vec![(
            CHAINLINK_SOURCE_ID.to_owned(),
            CHAINLINK_SOURCE_DOCUMENT.to_owned(),
        )],
        vec![
            (
                CHAINLINK_SIGNAL_ID.to_owned(),
                CHAINLINK_SIGNAL_DOCUMENT.to_owned(),
            ),
            (
                CHAINLINK_MOMENTUM_SIGNAL_ID.to_owned(),
                CHAINLINK_MOMENTUM_DOCUMENT.to_owned(),
            ),
            (
                CHAINLINK_VOLATILITY_SIGNAL_ID.to_owned(),
                CHAINLINK_VOLATILITY_DOCUMENT.to_owned(),
            ),
        ],
    )
}

/// The oracle lane's embedded signal contract documents `(id, json)`. The
/// Chainlink processor publishes `btc.chainlink.oracle_reference` and the two C2
/// live identities `btc.chainlink.price_momentum` / `btc.chainlink.price_volatility`;
/// the CF and Pyth oracle references and the Kalshi settlement family are
/// published elsewhere (brief B / A2) and are not claimed here. The two Pyth C2
/// momentum/volatility identities ship refuse-closed with no processor and are
/// therefore NOT listed here (they stay in the test's `pending_processor` set,
/// on the `btc.pyth.oracle_reference` precedent).
#[must_use]
pub fn oracle_signal_contract_documents() -> Vec<(&'static str, &'static str)> {
    vec![
        (CHAINLINK_SIGNAL_ID, CHAINLINK_SIGNAL_DOCUMENT),
        (CHAINLINK_MOMENTUM_SIGNAL_ID, CHAINLINK_MOMENTUM_DOCUMENT),
        (
            CHAINLINK_VOLATILITY_SIGNAL_ID,
            CHAINLINK_VOLATILITY_DOCUMENT,
        ),
        (PYTH_SIGNAL_ID, PYTH_SIGNAL_DOCUMENT),
    ]
}

/// Render a decimal at the family's fixed eight half-even places, as wire text.
fn decimal_value(value: Decimal) -> Value {
    json!(
        value
            .round_dp_with_strategy(OUTPUT_DECIMAL_PLACES, RoundingStrategy::MidpointNearestEven)
            .to_string()
    )
}

fn parse_utc(value: &str) -> Result<DateTime<Utc>, LaneProcessorError> {
    DateTime::parse_from_rfc3339(value)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|error| {
            LaneProcessorError::Computation(format!("undecodable timestamp {value}: {error}"))
        })
}

/// Whole milliseconds between two RFC 3339 instants, truncated toward zero and
/// floored at zero. `later - earlier`; a negative or equal span is zero.
fn elapsed_ms(earlier: &str, later: &str) -> Result<u64, LaneProcessorError> {
    let earlier = parse_utc(earlier)?;
    let later = parse_utc(later)?;
    let ms = later.signed_duration_since(earlier).num_milliseconds();
    Ok(u64::try_from(ms.max(0)).unwrap_or(0))
}

/// The venue-parameterised, wire-ready `values` map for one accepted
/// oracle-reference observation (B3 §5.1). `value` is the accepted oracle value
/// (already validated positive by the caller); `publish_time` is the source's own
/// event time; `observation_time` is the decision instant (`clock.produced_at`);
/// `previous_publish_time` is the last accepted observation's publish time, or
/// `None` before a second one exists; `extra` carries the venue-specific fields
/// (Chainlink: `round_id` / `phase_id` / `heartbeat_seconds` / `deviation_threshold_bps`;
/// CF: `avg_60s_value` / `avg_60s_window_size` / `index_id`).
pub fn oracle_reference_values(
    value: Decimal,
    publish_time: &str,
    observation_time: &str,
    previous_publish_time: Option<&str>,
    staleness_threshold_ms: u64,
    extra: BTreeMap<String, Value>,
) -> Result<BTreeMap<String, Value>, LaneProcessorError> {
    let staleness_ms = elapsed_ms(publish_time, observation_time)?;
    let update_interval_ms = match previous_publish_time {
        Some(previous) => elapsed_ms(previous, publish_time)?,
        None => 0,
    };
    let freshness = if staleness_ms <= staleness_threshold_ms {
        "fresh"
    } else {
        "stale"
    };
    let mut values = BTreeMap::from([
        ("value".to_owned(), decimal_value(value)),
        ("publish_time".to_owned(), json!(publish_time)),
        ("staleness_ms".to_owned(), json!(staleness_ms)),
        ("update_interval_ms".to_owned(), json!(update_interval_ms)),
        ("freshness".to_owned(), json!(freshness)),
        (
            "staleness_threshold_ms".to_owned(),
            json!(staleness_threshold_ms),
        ),
        ("observation_time".to_owned(), json!(observation_time)),
    ]);
    values.extend(extra);
    Ok(values)
}

/// The last ready decision published for the (single, global) instance: its
/// change key and the decision clock it was published on. A later evaluation that
/// reproduces the change key is suppressed until the declared interval falls due —
/// clock-only fields (`timestamp` / `duration_ms`) are outside the key, so
/// `staleness_ms` / `update_interval_ms` refresh on the interval and never trigger
/// their own publication (B3 §5.1).
#[derive(Debug, Clone)]
struct PublishedDecision {
    change_key: BTreeMap<String, Value>,
    published_ns: u64,
}

/// One value-emitting, coalescing emitter for a single global-scope oracle
/// identity. Mirrors the prediction lane's `PredictionEmitter` shape verbatim
/// (do not invent): the decision `sequence` counts published decisions only and
/// restarts at zero each publisher epoch.
pub struct OracleReferenceEmitter {
    definition: SignalDefinition,
    next_sequence: u64,
    lifetime_output_count: u64,
    ever_ready: bool,
    change_key_fields: BTreeSet<String>,
    last_published: Option<PublishedDecision>,
}

impl OracleReferenceEmitter {
    /// Build an emitter from a signal definition document (the embedded JSON).
    pub fn from_document(document: &str) -> Result<Self, LaneProcessorError> {
        let definition: SignalDefinition = serde_json::from_str(document).map_err(|error| {
            LaneProcessorError::InvalidState(format!("oracle signal document is invalid: {error}"))
        })?;
        let change_key_fields = definition
            .fields
            .iter()
            .filter(|field| {
                !matches!(
                    field.field_type,
                    FieldType::Timestamp | FieldType::DurationMs
                )
            })
            .map(|field| field.name.clone())
            .collect();
        Ok(Self {
            definition,
            next_sequence: 0,
            lifetime_output_count: 0,
            ever_ready: false,
            change_key_fields,
            last_published: None,
        })
    }

    /// The emitter's signal definition. `pub(crate)` so the reusing prediction
    /// lane can register its declared interval alongside its market identities.
    pub(crate) fn definition(&self) -> &SignalDefinition {
        &self.definition
    }

    /// Restart the per-generation output sequence at zero for a new file
    /// generation (C5), keeping the warm coalescing/lifetime state. `pub(crate)`
    /// so the Kalshi prediction lane resets this reused oracle-reference emitter
    /// alongside its market identities at a rotation boundary, exactly as a
    /// resume's fresh rebuild starts it at zero.
    pub(crate) fn reset_output_sequence(&mut self) {
        self.next_sequence = 0;
    }

    fn change_key(&self, values: &BTreeMap<String, Value>) -> BTreeMap<String, Value> {
        values
            .iter()
            .filter(|(name, _)| self.change_key_fields.contains(name.as_str()))
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect()
    }

    /// Publish this ready instance when it has not been published this epoch, when
    /// its change key differs, or when its declared interval has fallen due.
    fn publishes(
        &self,
        change_key: &BTreeMap<String, Value>,
        now_ns: u64,
    ) -> Result<bool, LaneProcessorError> {
        let Some(last) = self.last_published.as_ref() else {
            return Ok(true);
        };
        if &last.change_key != change_key {
            return Ok(true);
        }
        let interval_ns = self
            .definition
            .approximate_interval_ms
            .checked_mul(1_000_000)
            .ok_or_else(|| LaneProcessorError::Computation("interval overflow".to_owned()))?;
        Ok(now_ns.saturating_sub(last.published_ns) >= interval_ns)
    }

    /// Evaluate one decision. `ready_values` is `Some` for a ready reading and
    /// `None` for a non-ready one (which carries at least one reason). Returns the
    /// update to write, or `None` when the coalescing rule suppresses a ready
    /// restatement. A non-ready evaluation is never coalesced: the shell's
    /// `instance_status` path owns its own revision/periodic logic.
    /// Evaluate one decision. `pub(crate)` so the Kalshi prediction lane can reuse
    /// this exact global-scope oracle emitter for `btc.cfbrti.oracle_reference`
    /// (E43) rather than reimplement the coalescing rule.
    pub(crate) fn emit(
        &mut self,
        publisher_epoch: &str,
        clock: &LaneClock,
        ready_values: Option<BTreeMap<String, Value>>,
        mut reasons: Vec<String>,
    ) -> Result<Option<SignalUpdate>, LaneProcessorError> {
        reasons.sort();
        reasons.dedup();
        let ready = ready_values.is_some();
        let change_key = ready_values.as_ref().map(|values| self.change_key(values));
        let published = match &change_key {
            Some(change_key) => self.publishes(change_key, clock.now_ns)?,
            None => true,
        };
        let quality = if ready {
            Quality {
                state: QualityState::Ready,
                reasons: Vec::new(),
            }
        } else {
            if reasons.is_empty() {
                return Err(LaneProcessorError::Computation(
                    "a non-ready oracle output requires a reason".to_owned(),
                ));
            }
            Quality {
                state: if self.ever_ready {
                    QualityState::Degraded
                } else {
                    QualityState::WarmingUp
                },
                reasons,
            }
        };
        let (valid_from, valid_until) =
            SignalUpdate::validity_window(&clock.produced_at, self.definition.staleness_bound_ms)
                .map_err(|error| LaneProcessorError::Computation(error.to_string()))?;
        let update = SignalUpdate {
            schema_version: SIGNAL_UPDATE_SCHEMA_VERSION,
            signal_id: self.definition.signal_id.clone(),
            signal_version: self.definition.version.clone(),
            publisher_epoch: publisher_epoch.to_owned(),
            sequence: self.next_sequence,
            event_time: clock.produced_at.clone(),
            produced_at: clock.produced_at.clone(),
            valid_from,
            valid_until,
            quality,
            values: ready_values.clone().unwrap_or_default(),
            scope: SignalScope::Global,
        };
        update
            .validate_against(&self.definition)
            .map_err(|error| LaneProcessorError::Computation(error.to_string()))?;
        self.ever_ready |= ready;
        if let (Some(change_key), true) = (change_key, published) {
            self.last_published = Some(PublishedDecision {
                change_key,
                published_ns: clock.now_ns,
            });
            self.next_sequence = self
                .next_sequence
                .checked_add(1)
                .ok_or_else(|| LaneProcessorError::Computation("sequence overflow".to_owned()))?;
        } else if !ready {
            // A non-ready evaluation ends the published decision's standing: the
            // consumer now holds a status beside it, so a later recovery is new
            // information even if it restates the change key.
            self.last_published = None;
        }
        // `lifetime_output_count` counts EMITTED updates only (audit F6): a
        // coalesced (suppressed) ready evaluation is not returned and must not
        // inflate the persisted telemetry. A non-ready evaluation is always
        // returned, so it counts.
        if published {
            self.lifetime_output_count =
                self.lifetime_output_count.checked_add(1).ok_or_else(|| {
                    LaneProcessorError::Computation("output counter overflow".to_owned())
                })?;
        }
        // Decision-sequence invariant (audit F5): only a ready, published decision
        // consumes a `next_sequence`, so ready decisions are numbered contiguously
        // from zero within an epoch. A non-ready evaluation carries the NEXT ready
        // decision's sequence as an unstamped placeholder; the shell reframes it as
        // `instance_status`, which does not carry the update's own sequence, so no
        // two frames on the wire ever share a decision sequence. A consumer of the
        // raw `Vec<SignalUpdate>` outside `StreamAccounting` must read a non-ready
        // update's `sequence` as unstamped, never as a decision id.
        Ok(if published { Some(update) } else { None })
    }
}

/// Serialize a `u128` round id as a decimal string in the checkpoint JSON.
///
/// serde_json renders an integer wider than `u64` as a JSON number it then
/// refuses to emit (`Error("number out of range")`), and real Chainlink round
/// ids are `phase_id << 64 | aggregator_round_id`, which exceed `u64::MAX`.
/// Encoding as a decimal string keeps the checkpoint valid and lossless, and
/// matches the wire `round_id`, which is already decimal text.
mod round_id_dec {
    use serde::{Deserialize, Deserializer, Serializer};

    pub(super) fn serialize<S: Serializer>(value: &u128, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&value.to_string())
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<u128, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse::<u128>().map_err(serde::de::Error::custom)
    }
}

/// The `Option<u128>` variant of [`round_id_dec`]: `None` stays JSON `null`,
/// `Some` becomes a decimal string.
mod optional_round_id_dec {
    use serde::{Deserialize, Deserializer, Serializer};

    pub(super) fn serialize<S: Serializer>(
        value: &Option<u128>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match value {
            Some(inner) => serializer.serialize_some(&inner.to_string()),
            None => serializer.serialize_none(),
        }
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<u128>, D::Error> {
        let text: Option<String> = Option::deserialize(deserializer)?;
        text.map(|value| value.parse::<u128>().map_err(serde::de::Error::custom))
            .transpose()
    }
}

/// The accepted-round fold state (bounded): what the next decision reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AcceptedRound {
    /// The answer as canonical decimal text (its own eight-place render).
    value: String,
    /// The round's `updated_at`, the family's `publish_time`.
    publish_time: String,
    /// The proxy round id as decimal text (u80 on chain).
    round_id: String,
    /// The numeric round id, for the reorg-withhold comparison. Encoded as a
    /// decimal string on the wire because real round ids exceed `u64::MAX`.
    #[serde(with = "round_id_dec")]
    round_id_num: u128,
    /// The locally derived `round_id >> 64` phase.
    phase_id: u64,
}

/// Map a shared spot/volatility helper error into the lane error type.
fn map_spot<T>(result: Result<T, SpotSignalError>) -> Result<T, LaneProcessorError> {
    result.map_err(|error| LaneProcessorError::Computation(error.to_string()))
}

/// One accepted round retained in the momentum/volatility ring: the published
/// answer as canonical eight-place decimal text (the exact value the reference
/// identity publishes) and the round's `updated_at`. Serialised in the checkpoint
/// exactly like [`AcceptedRound`]'s `value` / `publish_time`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RoundSample {
    value: String,
    publish_time: String,
}

/// A bounded ring of accepted rounds, feeding the two C2 Chainlink identities
/// (`price_momentum` / `price_volatility`). It is advanced from the SAME
/// accepted-round path the `oracle_reference` emit reads — a strictly higher
/// valid round — so a reorg withhold or an invalid round adds nothing and never
/// resets it (the processor's existing reorg/invalid semantics, no new reset).
/// Sampling is per accepted round (event), not one-second: Chainlink rounds are
/// sparse. Eviction is relative to the newest round's publish time (deterministic
/// under replay), keeping the four-hour window plus at least the last
/// [`MIN_RING_ROUNDS`] rounds so the count horizon survives a quiet period.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RoundMomentumRing {
    rounds: VecDeque<RoundSample>,
}

/// One accepted round projected to the decision instant: its published price and
/// its age (now − publish time, floored at zero), oldest-first.
struct RoundView {
    price: Decimal,
    age_ms: u64,
}

impl RoundMomentumRing {
    /// Push a newly accepted round (its published eight-place value text and its
    /// `updated_at`), then evict rounds beyond the four-hour window measured from
    /// the newest round, always keeping at least [`MIN_RING_ROUNDS`].
    fn push_round(
        &mut self,
        value: String,
        publish_time: String,
    ) -> Result<(), LaneProcessorError> {
        self.rounds.push_back(RoundSample {
            value,
            publish_time,
        });
        let newest = self
            .rounds
            .back()
            .expect("just pushed a round")
            .publish_time
            .clone();
        while self.rounds.len() > MIN_RING_ROUNDS {
            let front = &self.rounds.front().expect("ring is non-empty").publish_time;
            if elapsed_ms(front, &newest)? > HORIZON_4H_MS {
                self.rounds.pop_front();
            } else {
                break;
            }
        }
        while self.rounds.len() > MAX_RING_ROUNDS {
            self.rounds.pop_front();
        }
        Ok(())
    }

    /// Project every retained round to the decision instant `now` (RFC 3339),
    /// oldest-first.
    fn views(&self, now: &str) -> Result<Vec<RoundView>, LaneProcessorError> {
        let mut views = Vec::with_capacity(self.rounds.len());
        for round in &self.rounds {
            let price = Decimal::from_str_exact(&round.value).map_err(|error| {
                LaneProcessorError::Computation(format!(
                    "ring value {} is invalid: {error}",
                    round.value
                ))
            })?;
            views.push(RoundView {
                price,
                age_ms: elapsed_ms(&round.publish_time, now)?,
            });
        }
        Ok(views)
    }

    /// Ready when at least two accepted rounds fall inside the fifteen-minute time
    /// horizon, or the count horizon is satisfied (at least two accepted rounds
    /// observed at all — the last-1-round return exists). Otherwise sparse.
    fn is_ready(views: &[RoundView]) -> bool {
        let in_15m = views
            .iter()
            .filter(|view| view.age_ms < HORIZON_15M_MS)
            .count();
        in_15m >= 2 || views.len() >= 2
    }

    /// The count of accepted rounds inside the trailing four-hour window.
    fn rounds_in_4h(views: &[RoundView]) -> u16 {
        u16::try_from(
            views
                .iter()
                .filter(|view| view.age_ms < HORIZON_4H_MS)
                .count(),
        )
        .unwrap_or(u16::MAX)
    }

    /// The simple bps return of the newest answer against the newest accepted
    /// round whose time is at or before `now − h` (age ≥ `h`). When no round is
    /// that old the horizon has no causal anchor: report zero rather than
    /// fabricating a return against a too-young round.
    fn time_return(views: &[RoundView], horizon_ms: u64) -> Result<Decimal, LaneProcessorError> {
        let newest = views.last().expect("ready ring is non-empty").price;
        for view in views.iter().rev() {
            if view.age_ms >= horizon_ms {
                return map_spot(return_bps(newest, view.price));
            }
        }
        Ok(Decimal::ZERO)
    }

    /// The simple bps return of the newest answer against the round `n` positions
    /// back (regardless of age), clamped to the oldest retained round when fewer
    /// than `n + 1` rounds exist so the recent-move signal degrades gracefully
    /// rather than hiding to zero.
    fn count_return(views: &[RoundView], n: usize) -> Result<Decimal, LaneProcessorError> {
        let newest = views.last().expect("ready ring is non-empty").price;
        let len = views.len();
        let index = len - 1 - n.min(len - 1);
        map_spot(return_bps(newest, views[index].price))
    }

    /// The `price_momentum` value map, or `None` (sparse) when not ready.
    fn momentum_values(
        &self,
        now: &str,
    ) -> Result<Option<BTreeMap<String, Value>>, LaneProcessorError> {
        let views = self.views(now)?;
        if !Self::is_ready(&views) {
            return Ok(None);
        }
        let return_1h = Self::time_return(&views, HORIZON_1H_MS)?;
        let newest_age = views.last().expect("ready ring is non-empty").age_ms;
        let values = BTreeMap::from([
            (
                "return_bps_15m".to_owned(),
                spot_decimal_value(Self::time_return(&views, HORIZON_15M_MS)?),
            ),
            ("return_bps_1h".to_owned(), spot_decimal_value(return_1h)),
            (
                "return_bps_4h".to_owned(),
                spot_decimal_value(Self::time_return(&views, HORIZON_4H_MS)?),
            ),
            (
                "return_bps_last_1_round".to_owned(),
                spot_decimal_value(Self::count_return(&views, 1)?),
            ),
            (
                "return_bps_last_5_rounds".to_owned(),
                spot_decimal_value(Self::count_return(&views, 5)?),
            ),
            ("direction_1h".to_owned(), json!(direction_word(return_1h))),
            ("rounds_in_4h".to_owned(), json!(Self::rounds_in_4h(&views))),
            ("newest_round_age_ms".to_owned(), json!(newest_age)),
            ("window_close".to_owned(), json!(now)),
        ]);
        Ok(Some(values))
    }

    /// The `price_volatility` value map, or `None` (sparse) when not ready.
    fn volatility_values(
        &self,
        now: &str,
    ) -> Result<Option<BTreeMap<String, Value>>, LaneProcessorError> {
        let views = self.views(now)?;
        if !Self::is_ready(&views) {
            return Ok(None);
        }
        // Per-round bps returns paired with the newer endpoint's age.
        let mut per_round: Vec<(Decimal, u64)> = Vec::with_capacity(views.len().saturating_sub(1));
        for index in 1..views.len() {
            let ret = map_spot(return_bps(views[index].price, views[index - 1].price))?;
            per_round.push((ret, views[index].age_ms));
        }
        let volatility_15m = Self::window_rms(&per_round, HORIZON_15M_MS)?;
        let volatility_4h = Self::window_rms(&per_round, HORIZON_4H_MS)?;
        // 15m / 4h with the micro convention: a zero long window → ratio 1.
        let short_long_ratio = if volatility_4h.is_zero() {
            Decimal::ONE
        } else {
            map_spot(checked_div(volatility_15m, volatility_4h))?
        };
        let regime = classify_volatility_ratio(short_long_ratio);
        let newest_age = views.last().expect("ready ring is non-empty").age_ms;
        let values = BTreeMap::from([
            (
                "volatility_bps_15m".to_owned(),
                spot_decimal_value(volatility_15m),
            ),
            (
                "volatility_bps_1h".to_owned(),
                spot_decimal_value(Self::window_rms(&per_round, HORIZON_1H_MS)?),
            ),
            (
                "volatility_bps_4h".to_owned(),
                spot_decimal_value(volatility_4h),
            ),
            (
                "short_long_ratio".to_owned(),
                spot_decimal_value(short_long_ratio),
            ),
            ("regime".to_owned(), json!(regime)),
            ("rounds_in_4h".to_owned(), json!(Self::rounds_in_4h(&views))),
            ("newest_round_age_ms".to_owned(), json!(newest_age)),
            ("window_close".to_owned(), json!(now)),
        ]);
        Ok(Some(values))
    }

    /// Root mean square of the per-round returns whose newer endpoint falls inside
    /// the trailing `horizon_ms` window; an empty window is zero volatility.
    fn window_rms(
        per_round: &[(Decimal, u64)],
        horizon_ms: u64,
    ) -> Result<Decimal, LaneProcessorError> {
        let values: Vec<Decimal> = per_round
            .iter()
            .filter(|(_, age)| *age < horizon_ms)
            .map(|(ret, _)| *ret)
            .collect();
        if values.is_empty() {
            return Ok(Decimal::ZERO);
        }
        map_spot(root_mean_square(&values))
    }
}

/// The Chainlink oracle lane processor's bounded resume state (B3 §6): the
/// accepted round, the previous publish time (for `update_interval_ms`), the last
/// published round id (for the reorg withhold), any sticky block, and the
/// emitter's lifetime/readiness memory. The decision sequence and coalescing
/// memory are NOT persisted: a resume opens a new publisher epoch, so they
/// restart — exactly the spot pipeline's rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ChainlinkCheckpointState {
    accepted: Option<AcceptedRound>,
    previous_publish_time: Option<String>,
    #[serde(with = "optional_round_id_dec")]
    last_published_round_id: Option<u128>,
    pending_block: Option<String>,
    ever_ready: bool,
    lifetime_output_count: u64,
    /// The C2 accepted-round ring feeding the momentum/volatility identities, and
    /// each identity's lifetime/readiness memory. Persisted so a resume reproduces
    /// their frames without re-warming a four-hour sparse window (the accepted-round
    /// fold is checkpointed for the same reason). A pre-C2 checkpoint carries a
    /// different lane contract fingerprint and is refused before restore, so these
    /// always accompany the accepted round on a post-C2 checkpoint.
    momentum_ring: RoundMomentumRing,
    momentum_ever_ready: bool,
    momentum_lifetime_output_count: u64,
    volatility_ever_ready: bool,
    volatility_lifetime_output_count: u64,
}

/// A sticky non-ready condition that outlives one event: an invalid answer or a
/// reorg withhold, cleared only when a strictly higher valid round is accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingBlock {
    RoundInvalid,
    ReorgDetected,
}

impl PendingBlock {
    fn reason(self) -> &'static str {
        match self {
            Self::RoundInvalid => "chainlink_round_invalid",
            Self::ReorgDetected => "chainlink_reorg_detected",
        }
    }

    fn from_reason(reason: &str) -> Option<Self> {
        match reason {
            "chainlink_round_invalid" => Some(Self::RoundInvalid),
            "chainlink_reorg_detected" => Some(Self::ReorgDetected),
            _ => None,
        }
    }
}

/// The oracle lane's Chainlink processor (identity 2). A pure fold over journaled
/// round events, connectivity transitions and interval ticks, so a replay
/// reproduces every frame. Readiness/warm-up/staleness follow B3 §6:
/// `chainlink_warming_up` before the first accepted round; `chainlink_rpc_unavailable`
/// once the driver reports every endpoint down (it rotates internally, so a single
/// endpoint failing never reaches here); `chainlink_round_invalid` on a
/// non-positive answer; `chainlink_reorg_detected` on a round-id regression, held
/// until a round above the last published one; age publishes as `freshness: stale`,
/// never a status.
pub struct ChainlinkOracleProcessor {
    publisher_epoch: String,
    emitter: OracleReferenceEmitter,
    /// The two C2 identities built live on the accepted-round series. Each is a
    /// generic global-scope coalescing emitter (the same one the reference uses),
    /// so it numbers its own decisions and coalesces on its own change key.
    momentum_emitter: OracleReferenceEmitter,
    volatility_emitter: OracleReferenceEmitter,
    /// The bounded accepted-round ring the two C2 identities read (per round, not
    /// one-second). Fed only from the accepted-round path in `ingest_round`.
    momentum_ring: RoundMomentumRing,
    board: FeedHealthBoard,
    connected: bool,
    accepted: Option<AcceptedRound>,
    previous_publish_time: Option<String>,
    last_published_round_id: Option<u128>,
    pending_block: Option<PendingBlock>,
    /// True on a resumed processor until its first poll (audit F7): the driver has
    /// not run, so a stale restored round republishes rather than reporting
    /// `chainlink_rpc_unavailable` for a feed that has not failed once.
    awaiting_first_poll: bool,
    /// Sources the lane routing (`configure_source_routing`) marked
    /// observed-not-consumed: the composite lane's Kalshi index and Pyth Hermes
    /// frames (C1 phase 1). Their events are counted in feed health and routed
    /// past the Chainlink computation — never marking the poll alive, never
    /// producing a decision — so the Chainlink reading is byte-identical to the
    /// poll-only lane. Empty on the interim poll-only lane. Runtime routing
    /// config, handed over after construction and not checkpointed.
    observed_not_consumed: BTreeSet<String>,
}

impl ChainlinkOracleProcessor {
    /// The lane's single feed contract (the poll feed), used by the feed-health
    /// board and by the feed-only fallback path's fingerprint parity.
    fn feed_contract() -> FeedContract {
        FeedContract {
            source_id: CHAINLINK_STREAM_LABEL.to_owned(),
            venue: "oracle".to_owned(),
            expected_heartbeat_ms: None,
            cadence: FeedCadence::EventDriven,
            liveness_bound_override_ms: None,
        }
    }

    /// Build the processor for a publisher epoch.
    pub fn new(publisher_epoch: impl Into<String>) -> Self {
        let emitter = OracleReferenceEmitter::from_document(CHAINLINK_SIGNAL_DOCUMENT)
            .expect("the embedded Chainlink oracle-reference document is valid");
        let momentum_emitter = OracleReferenceEmitter::from_document(CHAINLINK_MOMENTUM_DOCUMENT)
            .expect("the embedded Chainlink price-momentum document is valid");
        let volatility_emitter =
            OracleReferenceEmitter::from_document(CHAINLINK_VOLATILITY_DOCUMENT)
                .expect("the embedded Chainlink price-volatility document is valid");
        let board = FeedHealthBoard::new(vec![Self::feed_contract()], 0);
        Self {
            publisher_epoch: publisher_epoch.into(),
            emitter,
            momentum_emitter,
            volatility_emitter,
            momentum_ring: RoundMomentumRing::default(),
            board,
            connected: false,
            accepted: None,
            previous_publish_time: None,
            last_published_round_id: None,
            pending_block: None,
            awaiting_first_poll: false,
            observed_not_consumed: BTreeSet::new(),
        }
    }

    /// Fold one round event into the accepted state. Sets the sticky block on an
    /// invalid answer or a reorg; clears it and advances the accepted round on a
    /// strictly higher valid round.
    fn ingest_round(&mut self, event: &SourceEvent) -> Result<(), LaneProcessorError> {
        let round_id = required_text(&event.values, "round_id")?;
        let round_id_num = round_id.parse::<u128>().map_err(|error| {
            LaneProcessorError::Computation(format!("undecodable round_id {round_id}: {error}"))
        })?;
        let phase_id = event
            .values
            .get("phase_id")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                LaneProcessorError::Computation("round is missing phase_id".to_owned())
            })?;
        let answer_text = required_text(&event.values, "answer")?;
        let answer = Decimal::from_str_exact(answer_text).map_err(|error| {
            LaneProcessorError::Computation(format!("undecodable answer {answer_text}: {error}"))
        })?;
        // A non-positive answer is never a published zero (B3 §5.1/§6).
        if answer <= Decimal::ZERO {
            self.pending_block = Some(PendingBlock::RoundInvalid);
            return Ok(());
        }
        // The published high-water mark disambiguates a reorg from a duplicate
        // (B3 §6, audit F1). The Chainlink adapter's per-run duplicate/regression
        // memory is not checkpointed, so the first poll after a restart re-serves
        // the current `latestRoundData()` round — EQUAL to the last published one
        // whenever no new on-chain round landed during the restart. That equality
        // is a re-observation, never a reorg.
        if let Some(published) = self.last_published_round_id {
            if round_id_num < published {
                // A strictly lower round is a chain reorganisation: withhold until
                // a round above the last published one is observed. Never republish
                // in place.
                self.pending_block = Some(PendingBlock::ReorgDetected);
                return Ok(());
            }
            if round_id_num == published {
                // A duplicate of the current round: no state change. `self.accepted`
                // already holds this round, so the value keeps publishing on the
                // interval (the first post-resume decision republishes it, as
                // ADR 0027 says). `previous_publish_time` must not advance — the
                // update interval is measured between distinct rounds — and any
                // outstanding reorg withhold stands, because this round is not
                // above the last published one.
                return Ok(());
            }
        }
        self.previous_publish_time = self
            .accepted
            .as_ref()
            .map(|round| round.publish_time.clone());
        let value = answer
            .round_dp_with_strategy(OUTPUT_DECIMAL_PLACES, RoundingStrategy::MidpointNearestEven)
            .to_string();
        // The C2 momentum/volatility ring follows the accepted-round fold exactly:
        // this is the one place a strictly higher valid round is accepted, so it is
        // the one place the ring advances. A reorg withhold or an invalid round
        // returns earlier and never reaches here, so the ring is never reset.
        self.momentum_ring
            .push_round(value.clone(), event.event_time.clone())?;
        self.accepted = Some(AcceptedRound {
            value,
            publish_time: event.event_time.clone(),
            round_id: round_id.to_owned(),
            round_id_num,
            phase_id,
        });
        self.pending_block = None;
        Ok(())
    }

    /// Evaluate all three identities for this decision instant: the oracle
    /// reference plus the two C2 identities built on the accepted-round ring. Each
    /// identity coalesces independently, so any subset may emit on a given tick.
    fn decide(&mut self, clock: &LaneClock) -> Result<Vec<SignalUpdate>, LaneProcessorError> {
        let mut updates = Vec::with_capacity(3);
        updates.extend(self.decide_reference(clock)?);
        updates.extend(self.decide_momentum(clock)?);
        updates.extend(self.decide_volatility(clock)?);
        Ok(updates)
    }

    /// Evaluate the C2 `price_momentum` identity from the accepted-round ring. It
    /// is independent of the reference's reorg/invalid/connectivity gating: its
    /// readiness is purely the round-count criterion, and the ring only advances on
    /// an accepted round, so during a withhold it simply restates (and coalesces).
    fn decide_momentum(
        &mut self,
        clock: &LaneClock,
    ) -> Result<Option<SignalUpdate>, LaneProcessorError> {
        let (ready_values, reasons) =
            match self.momentum_ring.momentum_values(&clock.produced_at)? {
                Some(values) => (Some(values), Vec::new()),
                // Before the first ready frame the identity is warming up (a fresh
                // Chainlink lane sees one round per heartbeat), a Warming-class
                // state; `chainlink_rounds_sparse` (Degraded) is reserved for a
                // window that has been ready and then thins out (audit MAJOR).
                None if !self.momentum_emitter.ever_ready => {
                    (None, vec![CHAINLINK_WARMING_UP.to_owned()])
                }
                None => (None, vec![CHAINLINK_ROUNDS_SPARSE.to_owned()]),
            };
        self.momentum_emitter
            .emit(&self.publisher_epoch, clock, ready_values, reasons)
    }

    /// Evaluate the C2 `price_volatility` identity from the accepted-round ring.
    fn decide_volatility(
        &mut self,
        clock: &LaneClock,
    ) -> Result<Option<SignalUpdate>, LaneProcessorError> {
        let (ready_values, reasons) =
            match self.momentum_ring.volatility_values(&clock.produced_at)? {
                Some(values) => (Some(values), Vec::new()),
                None if !self.volatility_emitter.ever_ready => {
                    (None, vec![CHAINLINK_WARMING_UP.to_owned()])
                }
                None => (None, vec![CHAINLINK_ROUNDS_SPARSE.to_owned()]),
            };
        self.volatility_emitter
            .emit(&self.publisher_epoch, clock, ready_values, reasons)
    }

    /// Build the oracle reference's `values` (ready) or reasons (non-ready).
    fn decide_reference(
        &mut self,
        clock: &LaneClock,
    ) -> Result<Option<SignalUpdate>, LaneProcessorError> {
        let (ready_values, reasons) = if let Some(block) = self.pending_block {
            (None, vec![block.reason().to_owned()])
        } else if let Some(accepted) = self.accepted.clone() {
            // A resumed processor republishes its restored round on the first
            // decision, before any poll (audit F7 / ADR 0027): its staleness shows
            // the age, and only an observed poll failure (a driver-reported
            // disconnect) yields `chainlink_rpc_unavailable`.
            if self.connected || self.awaiting_first_poll {
                let value = Decimal::from_str_exact(&accepted.value).map_err(|error| {
                    LaneProcessorError::Computation(format!("accepted value is invalid: {error}"))
                })?;
                let extra = BTreeMap::from([
                    ("round_id".to_owned(), json!(accepted.round_id)),
                    ("phase_id".to_owned(), json!(accepted.phase_id)),
                    (
                        "heartbeat_seconds".to_owned(),
                        json!(CHAINLINK_HEARTBEAT_SECONDS),
                    ),
                    (
                        "deviation_threshold_bps".to_owned(),
                        json!(CHAINLINK_DEVIATION_THRESHOLD_BPS),
                    ),
                ]);
                let values = oracle_reference_values(
                    value,
                    &accepted.publish_time,
                    &clock.produced_at,
                    self.previous_publish_time.as_deref(),
                    CHAINLINK_STALENESS_THRESHOLD_MS,
                    extra,
                )?;
                // The reorg high-water mark advances only on a published round.
                self.last_published_round_id = Some(accepted.round_id_num);
                (Some(values), Vec::new())
            } else {
                // Frames stopped: every endpoint is down (the driver rotates on a
                // single failure), so the instance is unavailable, not merely aged.
                (None, vec!["chainlink_rpc_unavailable".to_owned()])
            }
        } else {
            // No round accepted yet: warming, whether or not a socket is up.
            (None, vec!["chainlink_warming_up".to_owned()])
        };
        self.emitter
            .emit(&self.publisher_epoch, clock, ready_values, reasons)
    }
}

fn required_text<'a>(
    values: &'a BTreeMap<String, Value>,
    field: &'static str,
) -> Result<&'a str, LaneProcessorError> {
    values
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| LaneProcessorError::Computation(format!("round is missing {field}")))
}

impl LaneProcessor for ChainlinkOracleProcessor {
    fn signal_set_id(&self) -> &str {
        ORACLE_SIGNAL_SET_ID
    }

    fn signal_set_version(&self) -> &str {
        ORACLE_SIGNAL_SET_VERSION
    }

    fn signal_count(&self) -> usize {
        3
    }

    fn venues(&self) -> &'static [&'static str] {
        &["oracle"]
    }

    fn source_definitions(&self) -> Vec<LaneDefinitionDocument> {
        oracle_lane_definitions().0
    }

    fn signal_definitions(&self) -> Vec<LaneDefinitionDocument> {
        oracle_lane_definitions().1
    }

    fn feed_contracts(&self) -> Vec<FeedContract> {
        vec![Self::feed_contract()]
    }

    /// The oracle processor consumes exactly the Chainlink round source. On the
    /// composite lane (C1) the Kalshi index and Pyth Hermes stream sources are
    /// produced but not consumed here; declaring the consumed set turns on the
    /// runtime's observed-not-consumed routing so those frames are counted and
    /// routed past this computation rather than aborting the lane.
    fn consumed_sources(&self) -> Option<BTreeSet<String>> {
        Some(BTreeSet::from([CHAINLINK_SOURCE_ID.to_owned()]))
    }

    /// The Chainlink round source is required — a lane whose streams cannot
    /// produce it is a start-up misconfiguration (F1). The composite lane's
    /// `poll_sources` declares it, so the guard is satisfied.
    fn required_sources(&self) -> BTreeSet<String> {
        BTreeSet::from([CHAINLINK_SOURCE_ID.to_owned()])
    }

    fn set_observed_not_consumed(&mut self, sources: BTreeSet<String>) {
        self.observed_not_consumed = sources;
    }

    fn on_events(
        &mut self,
        stream: &str,
        events: &[SourceEvent],
        clock: &LaneClock,
    ) -> Result<Vec<SignalUpdate>, LaneProcessorError> {
        // Route each event by whether the lane marked its source
        // observed-not-consumed. The composite lane's Kalshi index and Pyth
        // Hermes frames (C1 phase 1) are counted in feed health and routed past
        // the Chainlink computation: they never mark the poll feed alive and
        // never produce a decision, so an ignored-only batch leaves the Chainlink
        // reading byte-identical to the poll-only lane. A consumed (Chainlink)
        // batch runs the original path unchanged — the first event proves the
        // poll feed live and fresh and clears the post-resume wait (audit F7).
        let mut consumed_any = false;
        for event in events {
            if self.observed_not_consumed.contains(&event.source_id) {
                self.board.note_ignored(&event.source_id, clock.now_ns);
                continue;
            }
            if !consumed_any {
                self.board.observe_event(stream, clock.now_ns);
                self.connected = true;
                self.awaiting_first_poll = false;
                consumed_any = true;
            }
            self.ingest_round(event)?;
        }
        if consumed_any {
            self.decide(clock)
        } else {
            Ok(Vec::new())
        }
    }

    fn on_connectivity(
        &mut self,
        stream: &str,
        connected: bool,
        clock: &LaneClock,
    ) -> Result<Vec<SignalUpdate>, LaneProcessorError> {
        // Only the Chainlink poll's connectivity gates the reading. The composite
        // lane's Kalshi index and Pyth Hermes streams (C1 phase 1) are
        // observed-not-consumed: their connect/disconnect frames are journaled by
        // the runtime but never mark the poll feed alive or produce a decision, so
        // the Chainlink reading stays byte-identical to the poll-only lane.
        if stream != CHAINLINK_STREAM_LABEL {
            return Ok(Vec::new());
        }
        if connected {
            self.board.observe_connected(stream, clock.now_ns);
        } else {
            self.board.observe_availability(
                stream,
                FeedAvailability::Down,
                "chainlink_rpc_unavailable",
                clock.now_ns,
            );
        }
        self.connected = connected;
        // A poll has now reported (up or down): a later disconnect is a real
        // failure, so `chainlink_rpc_unavailable` is in scope from here (audit F7).
        self.awaiting_first_poll = false;
        self.decide(clock)
    }

    fn on_interval(&mut self, clock: &LaneClock) -> Result<Vec<SignalUpdate>, LaneProcessorError> {
        self.board.observe_tick(clock.now_ns);
        self.decide(clock)
    }

    fn producer_health(&self) -> ProducerHealth {
        if self.pending_block.is_none() && self.connected && self.accepted.is_some() {
            return ProducerHealth {
                status: ProducerStatus::Ready,
                reasons: Vec::new(),
            };
        }
        let reason = match self.pending_block {
            Some(block) => block.reason().to_owned(),
            None if self.accepted.is_none() => "chainlink_warming_up".to_owned(),
            None => "chainlink_rpc_unavailable".to_owned(),
        };
        let status = if self.emitter.ever_ready {
            ProducerStatus::Degraded
        } else {
            ProducerStatus::Starting
        };
        ProducerHealth {
            status,
            reasons: vec![reason],
        }
    }

    fn feed_health_snapshot(&self) -> FeedHealthSnapshot {
        self.board.snapshot()
    }

    fn feed_health_emission_revision(&self) -> u64 {
        self.board.emission_revision()
    }

    fn checkpoint_state(&self) -> serde_json::Value {
        serde_json::to_value(ChainlinkCheckpointState {
            accepted: self.accepted.clone(),
            previous_publish_time: self.previous_publish_time.clone(),
            last_published_round_id: self.last_published_round_id,
            pending_block: self.pending_block.map(|block| block.reason().to_owned()),
            ever_ready: self.emitter.ever_ready,
            lifetime_output_count: self.emitter.lifetime_output_count,
            momentum_ring: self.momentum_ring.clone(),
            momentum_ever_ready: self.momentum_emitter.ever_ready,
            momentum_lifetime_output_count: self.momentum_emitter.lifetime_output_count,
            volatility_ever_ready: self.volatility_emitter.ever_ready,
            volatility_lifetime_output_count: self.volatility_emitter.lifetime_output_count,
        })
        .expect("Chainlink oracle checkpoint state serializes")
    }

    fn restore_state(&mut self, state: serde_json::Value) -> Result<(), LaneProcessorError> {
        let state: ChainlinkCheckpointState = serde_json::from_value(state)
            .map_err(|error| LaneProcessorError::InvalidState(error.to_string()))?;
        let pending_block = match state.pending_block.as_deref() {
            Some(reason) => Some(PendingBlock::from_reason(reason).ok_or_else(|| {
                LaneProcessorError::InvalidState(format!("unknown pending block {reason}"))
            })?),
            None => None,
        };
        self.accepted = state.accepted;
        self.previous_publish_time = state.previous_publish_time;
        self.last_published_round_id = state.last_published_round_id;
        self.pending_block = pending_block;
        self.momentum_ring = state.momentum_ring;
        // A resume opens a new publisher epoch: the decision sequence restarts at
        // zero and the coalescing memory starts empty, so the first post-resume
        // decision republishes the instance (the spot pipeline's rule). The
        // lifetime/readiness memory carries. The two C2 emitters follow the same
        // rule, and their accepted-round ring carries so momentum/volatility do not
        // re-warm a sparse four-hour window.
        self.emitter.next_sequence = 0;
        self.emitter.last_published = None;
        self.emitter.ever_ready = state.ever_ready;
        self.emitter.lifetime_output_count = state.lifetime_output_count;
        self.momentum_emitter.next_sequence = 0;
        self.momentum_emitter.last_published = None;
        self.momentum_emitter.ever_ready = state.momentum_ever_ready;
        self.momentum_emitter.lifetime_output_count = state.momentum_lifetime_output_count;
        self.volatility_emitter.next_sequence = 0;
        self.volatility_emitter.last_published = None;
        self.volatility_emitter.ever_ready = state.volatility_ever_ready;
        self.volatility_emitter.lifetime_output_count = state.volatility_lifetime_output_count;
        // A resumed lane must not claim the socket live until it reconnects, but
        // it has not polled either: its first decision republishes the restored
        // round rather than reporting an unfailed feed unavailable (audit F7).
        self.connected = false;
        self.awaiting_first_poll = true;
        Ok(())
    }

    fn reset_output_sequences(&mut self) {
        // C5 Design B: this processor publishes three per-identity emitters — the
        // reference plus the two C2 momentum/volatility identities — so a rotation
        // restarts every one's decision sequence at zero for the fresh output epoch,
        // exactly as a resume's `restore_state` does above. The accepted round, the
        // momentum ring and the coalescing/lifetime memory carry across untouched.
        self.emitter.reset_output_sequence();
        self.momentum_emitter.reset_output_sequence();
        self.volatility_emitter.reset_output_sequence();
    }
}

// ---------------------------------------------------------------------------
// Pyth Hermes oracle reference (S-RULINGS E48/E56, C1 phase 1).
// ---------------------------------------------------------------------------
//
// The value-carrying `btc.pyth.oracle_reference` on the oracle lane, fed from the
// Pyth Hermes aggregate-price stream (`pyth.hermes.btc-usd.price`). It reuses the
// shared family core ([`OracleReferenceEmitter`] + [`oracle_reference_values`])
// exactly as the Chainlink reference does — same eight half-even decimal places,
// same coalescing rule, same shared fields — and adds the Pyth-specific
// quantities the Hermes source carries (confidence, EMA price/confidence, and the
// stream/latest discriminator).
//
// E56 raised the identity from the relay-fed contract to a Hermes-fed one. The
// repository's pre-release versioning contract (docs/CONTRACTS.md,
// definitions/btc/README.md) admits only a single replace-in-place V1, and the
// registry projection admits only `N.0.0` / `0.N.0`, so E56's "move to 1.1.0" is
// applied as the reviewed correction of 1.0.0 in place (coordinator ruling,
// 2026-09-05).
//
// This processor is self-contained and leaves the Chainlink code byte-unchanged;
// wiring it onto the composite oracle lane's driver (so `pyth.hermes.btc-usd.price`
// is CONSUMED rather than observed-not-consumed) is a follow-up in the lane files.

/// The Pyth Hermes source the reference consumes and the stream label the driver
/// reports it under.
pub const PYTH_SOURCE_ID: &str = "pyth.hermes.btc-usd.price";
pub const PYTH_STREAM_LABEL: &str = "pyth_hermes";
const PYTH_SIGNAL_ID: &str = "btc.pyth.oracle_reference";

/// The Pyth reference's own signal-set / checkpoint identity.
pub const PYTH_SIGNAL_SET_ID: &str = "btc.pyth.oracle";
pub const PYTH_SIGNAL_SET_VERSION: &str = "1.0.0";

/// Pyth is a continuous, roughly one-hertz aggregate, so freshness compares
/// against a market-data bound: the default staleness threshold is five seconds
/// (the source's expected reconnect heartbeat is thirty seconds; the price cadence
/// is far tighter). A reading older than the threshold still publishes ready with
/// `freshness = stale` — a published measurement, not a quality state.
const PYTH_STALENESS_THRESHOLD_MS: u64 = 5_000;

const PYTH_SIGNAL_DOCUMENT: &str =
    include_str!("../../../definitions/btc/signals/btc.pyth.oracle_reference@1.0.0.json");
const PYTH_SOURCE_DOCUMENT: &str =
    include_str!("../../../definitions/btc/sources/pyth.hermes.btc-usd.price@1.0.0.json");

/// The oracle lane's Pyth `(sources, signals)` definition documents, a pure
/// function mirroring [`oracle_lane_definitions`] so the contract fingerprint
/// recomputes from a checkpoint's recorded config without a processor in hand.
#[must_use]
pub fn pyth_oracle_lane_definitions() -> (Vec<LaneDefinitionDocument>, Vec<LaneDefinitionDocument>)
{
    (
        vec![(PYTH_SOURCE_ID.to_owned(), PYTH_SOURCE_DOCUMENT.to_owned())],
        vec![(PYTH_SIGNAL_ID.to_owned(), PYTH_SIGNAL_DOCUMENT.to_owned())],
    )
}

/// Render a decimal at the family's fixed eight half-even places as wire text
/// (the [`decimal_value`] payload without the JSON wrapper).
fn decimal_string(value: Decimal) -> String {
    value
        .round_dp_with_strategy(OUTPUT_DECIMAL_PLACES, RoundingStrategy::MidpointNearestEven)
        .to_string()
}

/// Read a normalized decimal-string field from a Pyth event and parse it exactly.
fn pyth_decimal(
    values: &BTreeMap<String, Value>,
    field: &'static str,
) -> Result<Decimal, LaneProcessorError> {
    let text = values
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| LaneProcessorError::Computation(format!("Pyth event is missing {field}")))?;
    Decimal::from_str_exact(text).map_err(|error| {
        LaneProcessorError::Computation(format!("Pyth {field} {text} is not a decimal: {error}"))
    })
}

/// One accepted Pyth aggregate observation, carried as wire-ready eight-place
/// strings for the value-scale quantities plus the source discriminator.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct AcceptedPyth {
    value: String,
    confidence: String,
    ema_price: String,
    ema_conf: String,
    update_kind: String,
    publish_time: String,
}

/// The bounded resume state. There is no backfill on the Hermes source, so the
/// reference re-warms rather than fetching; only the accepted observation, the
/// update-interval anchor, and the emitter's lifetime/readiness memory carry.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PythCheckpointState {
    accepted: Option<AcceptedPyth>,
    previous_publish_time: Option<String>,
    ever_ready: bool,
    lifetime_output_count: u64,
}

/// The value-emitting Pyth oracle-reference processor.
pub struct PythOracleProcessor {
    publisher_epoch: String,
    emitter: OracleReferenceEmitter,
    board: FeedHealthBoard,
    connected: bool,
    accepted: Option<AcceptedPyth>,
    previous_publish_time: Option<String>,
    /// Sources the lane routing marked observed-not-consumed (any non-Pyth stream
    /// once this processor shares the composite oracle lane). Handed over after
    /// construction and not checkpointed.
    observed_not_consumed: BTreeSet<String>,
}

impl PythOracleProcessor {
    /// The Pyth stream's feed contract for the feed-health board.
    fn feed_contract() -> FeedContract {
        FeedContract {
            source_id: PYTH_STREAM_LABEL.to_owned(),
            venue: "oracle".to_owned(),
            expected_heartbeat_ms: Some(30_000),
            cadence: FeedCadence::Periodic,
            liveness_bound_override_ms: None,
        }
    }

    /// Build the processor for a publisher epoch.
    pub fn new(publisher_epoch: impl Into<String>) -> Self {
        let emitter = OracleReferenceEmitter::from_document(PYTH_SIGNAL_DOCUMENT)
            .expect("the embedded Pyth oracle-reference document is valid");
        let board = FeedHealthBoard::new(vec![Self::feed_contract()], 0);
        Self {
            publisher_epoch: publisher_epoch.into(),
            emitter,
            board,
            connected: false,
            accepted: None,
            previous_publish_time: None,
            observed_not_consumed: BTreeSet::new(),
        }
    }

    /// Fold one Hermes price event into the accepted observation. The source has
    /// already rescaled the provider integers by `10^expo` with exact decimal
    /// arithmetic and validated the shape — an unknown Hermes shape is refused
    /// upstream in the adapter with an `UnsupportedMessage` notice and never
    /// reaches here — so this reads the normalized decimal strings, re-rounds them
    /// to the family's eight half-even places, and chains the update interval to
    /// the previous accepted observation.
    fn ingest_price(&mut self, event: &SourceEvent) -> Result<(), LaneProcessorError> {
        let value = pyth_decimal(&event.values, "price")?;
        let confidence = pyth_decimal(&event.values, "conf")?;
        let ema_price = pyth_decimal(&event.values, "ema_price")?;
        let ema_conf = pyth_decimal(&event.values, "ema_conf")?;
        let update_kind = event
            .values
            .get("update_kind")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                LaneProcessorError::Computation("Pyth event is missing update_kind".to_owned())
            })?
            .to_owned();
        if event.event_time.is_empty() {
            return Err(LaneProcessorError::Computation(
                "Pyth event carries no event_time".to_owned(),
            ));
        }
        self.previous_publish_time = self
            .accepted
            .as_ref()
            .map(|accepted| accepted.publish_time.clone());
        self.accepted = Some(AcceptedPyth {
            value: decimal_string(value),
            confidence: decimal_string(confidence),
            ema_price: decimal_string(ema_price),
            ema_conf: decimal_string(ema_conf),
            update_kind,
            publish_time: event.event_time.clone(),
        });
        Ok(())
    }

    /// Evaluate the reference for this decision instant. Warming until the first
    /// accepted aggregate; ready (with a fresh/stale freshness measurement) while
    /// the stream is up; unavailable while it is down.
    fn decide(&mut self, clock: &LaneClock) -> Result<Vec<SignalUpdate>, LaneProcessorError> {
        let (ready_values, reasons) = if let Some(accepted) = self.accepted.clone() {
            if self.connected {
                let value = Decimal::from_str_exact(&accepted.value).map_err(|error| {
                    LaneProcessorError::Computation(format!("accepted value is invalid: {error}"))
                })?;
                let extra = BTreeMap::from([
                    ("confidence".to_owned(), json!(accepted.confidence)),
                    ("ema_price".to_owned(), json!(accepted.ema_price)),
                    ("ema_conf".to_owned(), json!(accepted.ema_conf)),
                    ("update_kind".to_owned(), json!(accepted.update_kind)),
                ]);
                let values = oracle_reference_values(
                    value,
                    &accepted.publish_time,
                    &clock.produced_at,
                    self.previous_publish_time.as_deref(),
                    PYTH_STALENESS_THRESHOLD_MS,
                    extra,
                )?;
                (Some(values), Vec::new())
            } else {
                // The stream dropped: no fresh aggregate can arrive, so the
                // instance is unavailable rather than merely aged.
                (None, vec!["pyth_disconnected".to_owned()])
            }
        } else {
            // No aggregate accepted yet.
            (None, vec!["pyth_warming_up".to_owned()])
        };
        Ok(self
            .emitter
            .emit(&self.publisher_epoch, clock, ready_values, reasons)?
            .into_iter()
            .collect())
    }
}

impl LaneProcessor for PythOracleProcessor {
    fn signal_set_id(&self) -> &str {
        PYTH_SIGNAL_SET_ID
    }

    fn signal_set_version(&self) -> &str {
        PYTH_SIGNAL_SET_VERSION
    }

    fn signal_count(&self) -> usize {
        1
    }

    fn venues(&self) -> &'static [&'static str] {
        &["oracle"]
    }

    fn source_definitions(&self) -> Vec<LaneDefinitionDocument> {
        pyth_oracle_lane_definitions().0
    }

    fn signal_definitions(&self) -> Vec<LaneDefinitionDocument> {
        pyth_oracle_lane_definitions().1
    }

    fn feed_contracts(&self) -> Vec<FeedContract> {
        vec![Self::feed_contract()]
    }

    /// The Pyth reference consumes exactly the Hermes price source. Declaring the
    /// consumed set turns on the runtime's observed-not-consumed routing for the
    /// other oracle-lane streams once this processor shares that lane.
    fn consumed_sources(&self) -> Option<BTreeSet<String>> {
        Some(BTreeSet::from([PYTH_SOURCE_ID.to_owned()]))
    }

    /// The Hermes source is NOT required: it is rights- and credential-gated
    /// (ruling R7 / E56), so a deployment without the Pyth key omits the stream and
    /// the reference stays warming rather than the lane refusing to start.
    fn required_sources(&self) -> BTreeSet<String> {
        BTreeSet::new()
    }

    fn set_observed_not_consumed(&mut self, sources: BTreeSet<String>) {
        self.observed_not_consumed = sources;
    }

    fn on_events(
        &mut self,
        stream: &str,
        events: &[SourceEvent],
        clock: &LaneClock,
    ) -> Result<Vec<SignalUpdate>, LaneProcessorError> {
        let mut consumed_any = false;
        for event in events {
            if event.source_id == PYTH_SOURCE_ID {
                if !consumed_any {
                    self.board.observe_event(stream, clock.now_ns);
                    self.connected = true;
                    consumed_any = true;
                }
                self.ingest_price(event)?;
            } else {
                // Any non-Pyth frame is observed-not-consumed: counted in feed
                // health and routed past this computation.
                self.board.note_ignored(&event.source_id, clock.now_ns);
            }
        }
        if consumed_any {
            self.decide(clock)
        } else {
            Ok(Vec::new())
        }
    }

    fn on_connectivity(
        &mut self,
        stream: &str,
        connected: bool,
        clock: &LaneClock,
    ) -> Result<Vec<SignalUpdate>, LaneProcessorError> {
        // Only the Pyth stream's connectivity gates the reading.
        if stream != PYTH_STREAM_LABEL {
            return Ok(Vec::new());
        }
        if connected {
            self.board.observe_connected(stream, clock.now_ns);
        } else {
            self.board.observe_availability(
                stream,
                FeedAvailability::Down,
                "pyth_disconnected",
                clock.now_ns,
            );
        }
        self.connected = connected;
        self.decide(clock)
    }

    fn on_interval(&mut self, clock: &LaneClock) -> Result<Vec<SignalUpdate>, LaneProcessorError> {
        self.board.observe_tick(clock.now_ns);
        self.decide(clock)
    }

    fn producer_health(&self) -> ProducerHealth {
        if self.connected && self.accepted.is_some() {
            return ProducerHealth {
                status: ProducerStatus::Ready,
                reasons: Vec::new(),
            };
        }
        let reason = if self.accepted.is_none() {
            "pyth_warming_up"
        } else {
            "pyth_disconnected"
        };
        let status = if self.emitter.ever_ready {
            ProducerStatus::Degraded
        } else {
            ProducerStatus::Starting
        };
        ProducerHealth {
            status,
            reasons: vec![reason.to_owned()],
        }
    }

    fn feed_health_snapshot(&self) -> FeedHealthSnapshot {
        self.board.snapshot()
    }

    fn feed_health_emission_revision(&self) -> u64 {
        self.board.emission_revision()
    }

    fn checkpoint_state(&self) -> serde_json::Value {
        serde_json::to_value(PythCheckpointState {
            accepted: self.accepted.clone(),
            previous_publish_time: self.previous_publish_time.clone(),
            ever_ready: self.emitter.ever_ready,
            lifetime_output_count: self.emitter.lifetime_output_count,
        })
        .expect("Pyth oracle checkpoint state serializes")
    }

    fn restore_state(&mut self, state: serde_json::Value) -> Result<(), LaneProcessorError> {
        let state: PythCheckpointState = serde_json::from_value(state)
            .map_err(|error| LaneProcessorError::InvalidState(error.to_string()))?;
        self.accepted = state.accepted;
        self.previous_publish_time = state.previous_publish_time;
        // A resume opens a new publisher epoch: the decision sequence restarts at
        // zero and the coalescing memory starts empty, so the first post-resume
        // decision republishes the instance. The lifetime/readiness memory carries.
        self.emitter.next_sequence = 0;
        self.emitter.last_published = None;
        self.emitter.ever_ready = state.ever_ready;
        self.emitter.lifetime_output_count = state.lifetime_output_count;
        // A resumed lane must not claim the stream live until it reconnects.
        self.connected = false;
        Ok(())
    }

    fn reset_output_sequences(&mut self) {
        // C5 Design B: this processor publishes the single `btc.pyth.oracle_reference`
        // identity, so a rotation restarts its emitter's decision sequence at zero for
        // the fresh output epoch, exactly as `restore_state` does above. The accepted
        // reading and the coalescing/lifetime memory carry across untouched.
        self.emitter.reset_output_sequence();
    }
}

// ---------------------------------------------------------------------------
// The composite oracle-lane processor (C1: Chainlink poll + Pyth Hermes stream).
// ---------------------------------------------------------------------------
//
// The oracle lane runs one [`LaneProcessor`]; this wraps the Chainlink processor
// (unchanged) and the Pyth processor so both publish on the same publisher epoch
// and journal byte-identically in one process. The Chainlink sub-processor still
// owns the Chainlink poll and counts the Kalshi index sources observed-not-consumed
// exactly as before, so `btc.chainlink.oracle_reference` and its two C2 identities
// stay byte-identical; the Pyth sub-processor owns the (now CONSUMED) Hermes
// stream and publishes `btc.pyth.oracle_reference`. The lane's checkpoint identity
// remains `btc.oracle`.
pub struct OracleLaneProcessor {
    chainlink: ChainlinkOracleProcessor,
    pyth: PythOracleProcessor,
}

impl OracleLaneProcessor {
    /// Build the composite for a publisher epoch.
    pub fn new(publisher_epoch: impl Into<String>) -> Self {
        let epoch = publisher_epoch.into();
        Self {
            chainlink: ChainlinkOracleProcessor::new(epoch.clone()),
            pyth: PythOracleProcessor::new(epoch),
        }
    }
}

impl LaneProcessor for OracleLaneProcessor {
    fn signal_set_id(&self) -> &str {
        ORACLE_SIGNAL_SET_ID
    }

    fn signal_set_version(&self) -> &str {
        ORACLE_SIGNAL_SET_VERSION
    }

    fn signal_count(&self) -> usize {
        self.chainlink.signal_count() + self.pyth.signal_count()
    }

    fn venues(&self) -> &'static [&'static str] {
        &["oracle"]
    }

    fn source_definitions(&self) -> Vec<LaneDefinitionDocument> {
        let mut documents = self.chainlink.source_definitions();
        documents.extend(self.pyth.source_definitions());
        documents
    }

    fn signal_definitions(&self) -> Vec<LaneDefinitionDocument> {
        let mut documents = self.chainlink.signal_definitions();
        documents.extend(self.pyth.signal_definitions());
        documents
    }

    fn feed_contracts(&self) -> Vec<FeedContract> {
        let mut contracts = self.chainlink.feed_contracts();
        contracts.extend(self.pyth.feed_contracts());
        contracts
    }

    fn consumed_sources(&self) -> Option<BTreeSet<String>> {
        let mut consumed = self.chainlink.consumed_sources().unwrap_or_default();
        if let Some(pyth) = self.pyth.consumed_sources() {
            consumed.extend(pyth);
        }
        Some(consumed)
    }

    fn required_sources(&self) -> BTreeSet<String> {
        let mut required = self.chainlink.required_sources();
        required.extend(self.pyth.required_sources());
        required
    }

    fn set_observed_not_consumed(&mut self, sources: BTreeSet<String>) {
        // The Chainlink sub-processor owns every non-Pyth stream on this lane (the
        // Chainlink poll plus the Kalshi index sources it counts
        // observed-not-consumed); the Pyth sub-processor only ever sees its own
        // consumed Hermes stream, so the routed set goes to Chainlink.
        self.chainlink.set_observed_not_consumed(sources);
    }

    fn on_events(
        &mut self,
        stream: &str,
        events: &[SourceEvent],
        clock: &LaneClock,
    ) -> Result<Vec<SignalUpdate>, LaneProcessorError> {
        if stream == PYTH_STREAM_LABEL {
            self.pyth.on_events(stream, events, clock)
        } else {
            self.chainlink.on_events(stream, events, clock)
        }
    }

    fn on_connectivity(
        &mut self,
        stream: &str,
        connected: bool,
        clock: &LaneClock,
    ) -> Result<Vec<SignalUpdate>, LaneProcessorError> {
        if stream == PYTH_STREAM_LABEL {
            self.pyth.on_connectivity(stream, connected, clock)
        } else {
            self.chainlink.on_connectivity(stream, connected, clock)
        }
    }

    fn on_interval(&mut self, clock: &LaneClock) -> Result<Vec<SignalUpdate>, LaneProcessorError> {
        let mut updates = self.chainlink.on_interval(clock)?;
        updates.extend(self.pyth.on_interval(clock)?);
        Ok(updates)
    }

    fn reset_output_sequences(&mut self) {
        // The composite owns no emitter of its own: it delegates the rotation
        // sequence reset to each sub-processor, which restarts every per-identity
        // emitter it publishes (the three Chainlink identities and the one Pyth
        // identity) at zero for the fresh output epoch.
        self.chainlink.reset_output_sequences();
        self.pyth.reset_output_sequences();
    }

    fn producer_health(&self) -> ProducerHealth {
        // The lane's producer health is its required constituent's (Chainlink).
        // The Pyth reference is rights-gated and non-required (ruling R7 / E56), so
        // its warming or disconnected state is carried in its own per-signal
        // instance_status, not in the whole lane's process-level status.
        self.chainlink.producer_health()
    }

    fn feed_health_snapshot(&self) -> FeedHealthSnapshot {
        let mut snapshot = self.chainlink.feed_health_snapshot();
        let pyth = self.pyth.feed_health_snapshot();
        snapshot.taken_at_ns = snapshot.taken_at_ns.max(pyth.taken_at_ns);
        snapshot.feeds.extend(pyth.feeds);
        snapshot.escalations.extend(pyth.escalations);
        snapshot.escalations_dropped += pyth.escalations_dropped;
        for (source, count) in pyth.ignored_events {
            *snapshot.ignored_events.entry(source).or_default() += count;
        }
        snapshot.reference_frames_rejected += pyth.reference_frames_rejected;
        snapshot.final_decisions_expired += pyth.final_decisions_expired;
        snapshot
    }

    fn feed_health_emission_revision(&self) -> u64 {
        self.chainlink.feed_health_emission_revision() + self.pyth.feed_health_emission_revision()
    }

    fn checkpoint_state(&self) -> serde_json::Value {
        json!({
            "chainlink": self.chainlink.checkpoint_state(),
            "pyth": self.pyth.checkpoint_state(),
        })
    }

    fn restore_state(&mut self, state: serde_json::Value) -> Result<(), LaneProcessorError> {
        let Value::Object(mut map) = state else {
            return Err(LaneProcessorError::InvalidState(
                "oracle composite checkpoint is not an object".to_owned(),
            ));
        };
        let chainlink = map.remove("chainlink").ok_or_else(|| {
            LaneProcessorError::InvalidState(
                "oracle composite checkpoint is missing the chainlink state".to_owned(),
            )
        })?;
        self.chainlink.restore_state(chainlink)?;
        // A checkpoint without a pyth section re-warms the reference rather than
        // failing (the Chainlink state is the load-bearing half).
        if let Some(pyth) = map.remove("pyth") {
            self.pyth.restore_state(pyth)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clock(now_ns: u64, produced_at: &str) -> LaneClock {
        LaneClock {
            now_ns,
            produced_at: produced_at.to_owned(),
        }
    }

    fn round_event(round_id: &str, phase_id: u64, answer: &str, updated_at: &str) -> SourceEvent {
        SourceEvent {
            schema_version: 1,
            source_id: CHAINLINK_SOURCE_ID.to_owned(),
            source_version: "1.0.0".to_owned(),
            publisher_epoch: "oracle-test".to_owned(),
            sequence: 0,
            event_time: updated_at.to_owned(),
            received_at: updated_at.to_owned(),
            values: BTreeMap::from([
                ("round_id".to_owned(), json!(round_id)),
                ("phase_id".to_owned(), json!(phase_id)),
                ("answer".to_owned(), json!(answer)),
                ("updated_at".to_owned(), json!(updated_at)),
            ]),
        }
    }

    /// The processor now emits up to three identities per decision (the reference
    /// plus the two C2 identities). The reference-behaviour tests scope to the
    /// reference identity through these helpers so a C2 emission never perturbs an
    /// assertion about `btc.chainlink.oracle_reference`.
    fn ref_updates(updates: &[SignalUpdate]) -> Vec<&SignalUpdate> {
        updates
            .iter()
            .filter(|update| update.signal_id == CHAINLINK_SIGNAL_ID)
            .collect()
    }

    fn ref_count(updates: &[SignalUpdate]) -> usize {
        ref_updates(updates).len()
    }

    fn only_ref(updates: &[SignalUpdate]) -> &SignalUpdate {
        let refs = ref_updates(updates);
        assert_eq!(refs.len(), 1, "exactly one reference update this decision");
        refs[0]
    }

    fn momentum_update(updates: &[SignalUpdate]) -> &SignalUpdate {
        updates
            .iter()
            .find(|update| update.signal_id == CHAINLINK_MOMENTUM_SIGNAL_ID)
            .expect("a price_momentum update is emitted")
    }

    fn volatility_update(updates: &[SignalUpdate]) -> &SignalUpdate {
        updates
            .iter()
            .find(|update| update.signal_id == CHAINLINK_VOLATILITY_SIGNAL_ID)
            .expect("a price_volatility update is emitted")
    }

    #[test]
    fn staleness_floors_at_zero_and_freshness_is_inclusive_at_threshold() {
        // An observation whose publish time is AFTER the decision instant floors
        // staleness at zero rather than reporting a negative age.
        let floored = oracle_reference_values(
            Decimal::from(100),
            "2026-09-03T00:00:01.000000000Z",
            "2026-09-03T00:00:00.000000000Z",
            None,
            5000,
            BTreeMap::new(),
        )
        .expect("values compute");
        assert_eq!(floored["staleness_ms"], json!(0u64));
        assert_eq!(floored["update_interval_ms"], json!(0u64));

        // Exactly at the threshold is fresh (inclusive); one ms past is stale.
        let at_threshold = oracle_reference_values(
            Decimal::from(100),
            "2026-09-03T00:00:00.000000000Z",
            "2026-09-03T00:00:05.000000000Z",
            None,
            5000,
            BTreeMap::new(),
        )
        .expect("values compute");
        assert_eq!(at_threshold["staleness_ms"], json!(5000u64));
        assert_eq!(at_threshold["freshness"], json!("fresh"));

        let past_threshold = oracle_reference_values(
            Decimal::from(100),
            "2026-09-03T00:00:00.000000000Z",
            "2026-09-03T00:00:05.001000000Z",
            None,
            5000,
            BTreeMap::new(),
        )
        .expect("values compute");
        assert_eq!(past_threshold["staleness_ms"], json!(5001u64));
        assert_eq!(past_threshold["freshness"], json!("stale"));
    }

    #[test]
    fn oracle_reference_core_fields_identical_across_cfbrti_and_chainlink() {
        // The seven shared core fields are a pure function of the core inputs —
        // the venue-specific `extra` map never changes them (family invariant R11).
        let value = Decimal::from_str_exact("77667.797").expect("decimal");
        let cf_extra = BTreeMap::from([
            ("avg_60s_value".to_owned(), decimal_value(value)),
            ("avg_60s_window_size".to_owned(), json!(60u32)),
            ("index_id".to_owned(), json!("BRTI")),
        ]);
        let chainlink_extra = BTreeMap::from([
            ("round_id".to_owned(), json!("101")),
            ("phase_id".to_owned(), json!(0u64)),
            ("heartbeat_seconds".to_owned(), json!(3600u32)),
            ("deviation_threshold_bps".to_owned(), json!(50u32)),
        ]);
        let cf = oracle_reference_values(
            value,
            "2026-09-03T00:00:00.000000000Z",
            "2026-09-03T00:00:02.000000000Z",
            Some("2026-09-02T23:59:59.000000000Z"),
            5000,
            cf_extra,
        )
        .expect("cf values");
        let chainlink = oracle_reference_values(
            value,
            "2026-09-03T00:00:00.000000000Z",
            "2026-09-03T00:00:02.000000000Z",
            Some("2026-09-02T23:59:59.000000000Z"),
            5000,
            chainlink_extra,
        )
        .expect("chainlink values");
        for field in [
            "value",
            "publish_time",
            "staleness_ms",
            "update_interval_ms",
            "freshness",
            "staleness_threshold_ms",
            "observation_time",
        ] {
            assert_eq!(cf[field], chainlink[field], "core field {field} matches");
        }
        // Decimals render at up to eight half-even places (the tree convention,
        // no trailing-zero padding), identically for both venues.
        assert_eq!(cf["value"], json!("77667.797"));
        assert_eq!(cf["update_interval_ms"], json!(1000u64));
    }

    #[test]
    fn chainlink_value_renders_fixed_eight_decimals() {
        // The processor re-renders the adapter's answer at eight half-even places
        // (vector 7766779700000 -> "77667.79700000").
        let mut processor = ChainlinkOracleProcessor::new("oracle-test");
        let updates = processor
            .on_events(
                CHAINLINK_STREAM_LABEL,
                &[round_event(
                    "101",
                    0,
                    "77667.79700000",
                    "2026-09-03T00:00:00.000000000Z",
                )],
                &clock(1_000_000_000, "2026-09-03T00:00:01.000000000Z"),
            )
            .expect("event admits");
        let reference = only_ref(&updates);
        assert_eq!(reference.signal_id, CHAINLINK_SIGNAL_ID);
        assert_eq!(reference.values["value"], json!("77667.79700000"));
        assert_eq!(reference.values["freshness"], json!("fresh"));
        assert_eq!(reference.values["round_id"], json!("101"));
        assert_eq!(reference.values["heartbeat_seconds"], json!(3600u32));
    }

    #[test]
    fn round_id_regression_withholds_publication_until_new_high_round() {
        let mut processor = ChainlinkOracleProcessor::new("oracle-test");
        let cl = clock(1_000_000_000, "2026-09-03T00:00:01.000000000Z");

        // Round 100 is accepted and published.
        let first = processor
            .on_events(
                CHAINLINK_STREAM_LABEL,
                &[round_event(
                    "100",
                    0,
                    "77000.00000000",
                    "2026-09-03T00:00:00.000000000Z",
                )],
                &cl,
            )
            .expect("first round admits");
        assert_eq!(only_ref(&first).quality.state, QualityState::Ready);
        assert_eq!(only_ref(&first).values["round_id"], json!("100"));

        // A regression to round 99 withholds publication with the reorg reason and
        // never republishes a value in place.
        let regressed = processor
            .on_events(
                CHAINLINK_STREAM_LABEL,
                &[round_event(
                    "99",
                    0,
                    "70000.00000000",
                    "2026-09-03T00:00:00.000000000Z",
                )],
                &clock(1_500_000_000, "2026-09-03T00:00:01.500000000Z"),
            )
            .expect("regression admits");
        assert_eq!(only_ref(&regressed).quality.state, QualityState::Degraded);
        assert_eq!(
            only_ref(&regressed).quality.reasons,
            vec!["chainlink_reorg_detected".to_owned()]
        );
        assert!(only_ref(&regressed).values.is_empty());

        // Round 100 again is still at-or-below the published high-water mark: held.
        let repeat = processor
            .on_events(
                CHAINLINK_STREAM_LABEL,
                &[round_event(
                    "100",
                    0,
                    "77000.00000000",
                    "2026-09-03T00:00:00.000000000Z",
                )],
                &clock(2_000_000_000, "2026-09-03T00:00:02.000000000Z"),
            )
            .expect("repeat admits");
        assert_eq!(
            only_ref(&repeat).quality.reasons,
            vec!["chainlink_reorg_detected".to_owned()]
        );

        // A round above the last published one clears the withhold and publishes.
        let recovered = processor
            .on_events(
                CHAINLINK_STREAM_LABEL,
                &[round_event(
                    "101",
                    0,
                    "78000.00000000",
                    "2026-09-03T00:00:03.000000000Z",
                )],
                &clock(3_000_000_000, "2026-09-03T00:00:03.000000000Z"),
            )
            .expect("recovery admits");
        assert_eq!(only_ref(&recovered).quality.state, QualityState::Ready);
        assert_eq!(only_ref(&recovered).values["round_id"], json!("101"));
        assert_eq!(
            only_ref(&recovered).values["value"],
            json!("78000.00000000")
        );
    }

    #[test]
    fn warms_up_before_the_first_round_and_a_non_positive_answer_is_invalid() {
        let mut processor = ChainlinkOracleProcessor::new("oracle-test");
        // A health tick before any round: warming, no value.
        let warming = processor
            .on_interval(&clock(1_000_000_000, "2026-09-03T00:00:01.000000000Z"))
            .expect("interval admits");
        // Before any accepted round the reference and both C2 identities warm up
        // (the ring is empty): three non-ready evaluations, one per identity. The
        // C2 identities carry the Warming-class `chainlink_warming_up`, never the
        // Degraded `chainlink_rounds_sparse`, until they have first been ready
        // (audit MAJOR: a fresh lane must not read as degraded for hours).
        assert_eq!(only_ref(&warming).quality.state, QualityState::WarmingUp);
        assert_eq!(
            only_ref(&warming).quality.reasons,
            vec!["chainlink_warming_up".to_owned()]
        );
        assert_eq!(
            momentum_update(&warming).quality.reasons,
            vec![CHAINLINK_WARMING_UP.to_owned()]
        );
        assert_eq!(
            volatility_update(&warming).quality.reasons,
            vec![CHAINLINK_WARMING_UP.to_owned()]
        );

        // A non-positive answer is never a published zero.
        let invalid = processor
            .on_events(
                CHAINLINK_STREAM_LABEL,
                &[round_event(
                    "100",
                    0,
                    "0.00000000",
                    "2026-09-03T00:00:00.000000000Z",
                )],
                &clock(2_000_000_000, "2026-09-03T00:00:02.000000000Z"),
            )
            .expect("invalid round admits");
        assert_eq!(
            only_ref(&invalid).quality.reasons,
            vec!["chainlink_round_invalid".to_owned()]
        );
        assert!(only_ref(&invalid).values.is_empty());
    }

    #[test]
    fn checkpoint_state_round_trips_the_accepted_round() {
        let mut processor = ChainlinkOracleProcessor::new("oracle-test");
        processor
            .on_events(
                CHAINLINK_STREAM_LABEL,
                &[round_event(
                    "100",
                    0,
                    "77000.00000000",
                    "2026-09-03T00:00:00.000000000Z",
                )],
                &clock(1_000_000_000, "2026-09-03T00:00:01.000000000Z"),
            )
            .expect("round admits");
        let state = processor.checkpoint_state();
        let mut restored = ChainlinkOracleProcessor::new("oracle-epoch-2");
        restored.restore_state(state).expect("state restores");
        // A resume opens a new epoch: the decision sequence restarts at zero and
        // the first post-resume decision republishes the retained round.
        // The socket must reconnect before a value publishes again.
        let after_resume = restored
            .on_connectivity(
                CHAINLINK_STREAM_LABEL,
                true,
                &clock(2_000_000_000, "2026-09-03T00:00:02.000000000Z"),
            )
            .expect("connectivity admits");
        assert_eq!(only_ref(&after_resume).quality.state, QualityState::Ready);
        assert_eq!(
            only_ref(&after_resume).sequence,
            0,
            "sequence restarts at zero on resume"
        );
        assert_eq!(only_ref(&after_resume).values["round_id"], json!("100"));
    }

    #[test]
    fn checkpoint_serializes_a_real_scale_round_id() {
        // Real Chainlink round ids are `phase_id << 64 | aggregator_round_id` and
        // exceed `u64::MAX`; serde_json cannot emit a `u128` that large as a JSON
        // number ("number out of range"), which panicked `checkpoint_state` (S3).
        // With the decimal-string encoding the checkpoint serialises and
        // round-trips losslessly.
        let phase: u64 = 5;
        let round_id_num: u128 = ((phase as u128) << 64) | 123_456;
        assert!(round_id_num > u128::from(u64::MAX));
        let round_id = round_id_num.to_string();

        let mut processor = ChainlinkOracleProcessor::new("oracle-test");
        processor
            .on_events(
                CHAINLINK_STREAM_LABEL,
                &[round_event(
                    &round_id,
                    phase,
                    "77000.00000000",
                    "2026-09-03T00:00:00.000000000Z",
                )],
                &clock(1_000_000_000, "2026-09-03T00:00:01.000000000Z"),
            )
            .expect("round admits");

        // Serialises (no panic) and the wide ids are decimal text on the wire.
        let state = processor.checkpoint_state();
        assert_eq!(state["accepted"]["round_id_num"], json!(round_id));
        assert_eq!(state["last_published_round_id"], json!(round_id));

        // Round-trips: the restored processor republishes the same round id.
        let mut restored = ChainlinkOracleProcessor::new("oracle-epoch-2");
        restored.restore_state(state).expect("state restores");
        let after_resume = restored
            .on_connectivity(
                CHAINLINK_STREAM_LABEL,
                true,
                &clock(2_000_000_000, "2026-09-03T00:00:02.000000000Z"),
            )
            .expect("connectivity admits");
        assert_eq!(only_ref(&after_resume).values["round_id"], json!(round_id));
    }

    #[test]
    fn checkpoint_serializes_a_real_observed_chainlink_round() {
        // The round id observed in the real Chainlink aggregator hour
        // (scratchpad/observed/chainlink-aggregator-r1-20260903T064148Z.jsonl):
        // the first `latestRoundData()` result decodes to roundId
        // 0x07_0000000000005d95 = phase 7, aggregator round 23957, answer
        // 7749205892627 (77492.05892627 at 8 dp). The round id exceeds
        // `u64::MAX`, so its first checkpoint after acceptance is the exact shape
        // that panicked in the smoke run — from real data, it must serialise.
        let round_id_num: u128 = 129_127_208_515_966_885_269;
        assert_eq!(round_id_num >> 64, 7);
        assert!(round_id_num > u128::from(u64::MAX));
        let round_id = round_id_num.to_string();

        let mut processor = ChainlinkOracleProcessor::new("oracle-test");
        let published = processor
            .on_events(
                CHAINLINK_STREAM_LABEL,
                &[round_event(
                    &round_id,
                    7,
                    "77492.05892627",
                    "2026-09-03T06:49:35.000000000Z",
                )],
                &clock(1_000_000_000, "2026-09-03T06:49:36.000000000Z"),
            )
            .expect("observed round admits");
        assert_eq!(only_ref(&published).values["round_id"], json!(round_id));

        // The first checkpoint after accepting the observed round serialises and
        // round-trips.
        let state = processor.checkpoint_state();
        assert_eq!(state["accepted"]["round_id_num"], json!(round_id));
        assert_eq!(state["last_published_round_id"], json!(round_id));
        let mut restored = ChainlinkOracleProcessor::new("oracle-epoch-2");
        restored
            .restore_state(state)
            .expect("observed state restores");
    }

    #[test]
    fn a_repeated_round_is_a_duplicate_not_a_reorg() {
        // Audit F1: with no prior reorg, re-serving the current round (equal id)
        // must not flag a reorg — it is a duplicate. The value keeps publishing.
        let mut processor = ChainlinkOracleProcessor::new("oracle-test");
        let first = processor
            .on_events(
                CHAINLINK_STREAM_LABEL,
                &[round_event(
                    "100",
                    0,
                    "77000.00000000",
                    "2026-09-03T00:00:00.000000000Z",
                )],
                &clock(1_000_000_000, "2026-09-03T00:00:01.000000000Z"),
            )
            .expect("first round admits");
        assert_eq!(only_ref(&first).quality.state, QualityState::Ready);
        assert_eq!(only_ref(&first).values["round_id"], json!("100"));

        // The SAME round re-served, well within the 60 s interval: the change key
        // is unchanged, so the decision coalesces (no frame), and crucially it is
        // never a reorg.
        let duplicate = processor
            .on_events(
                CHAINLINK_STREAM_LABEL,
                &[round_event(
                    "100",
                    0,
                    "77000.00000000",
                    "2026-09-03T00:00:00.000000000Z",
                )],
                &clock(1_200_000_000, "2026-09-03T00:00:01.200000000Z"),
            )
            .expect("duplicate admits");
        assert_eq!(
            ref_count(&duplicate),
            0,
            "a within-interval duplicate coalesces the reference, and is never a reorg"
        );
        // Once the interval falls due the same round refreshes as a ready value,
        // still never a reorg.
        let refreshed = processor
            .on_events(
                CHAINLINK_STREAM_LABEL,
                &[round_event(
                    "100",
                    0,
                    "77000.00000000",
                    "2026-09-03T00:00:00.000000000Z",
                )],
                &clock(62_000_000_000, "2026-09-03T00:01:02.000000000Z"),
            )
            .expect("interval refresh admits");
        assert_eq!(only_ref(&refreshed).quality.state, QualityState::Ready);
        assert_eq!(only_ref(&refreshed).values["round_id"], json!("100"));
    }

    #[test]
    fn a_resumed_lane_republishes_the_same_round_without_a_reorg() {
        // Audit F1 (blocker): on restart, the adapter re-serves the current round
        // (equal to the last published one when no new on-chain round landed). The
        // resumed processor must republish it, not withhold it as a reorg.
        let mut processor = ChainlinkOracleProcessor::new("oracle-test");
        processor
            .on_events(
                CHAINLINK_STREAM_LABEL,
                &[round_event(
                    "100",
                    0,
                    "77000.00000000",
                    "2026-09-03T00:00:00.000000000Z",
                )],
                &clock(1_000_000_000, "2026-09-03T00:00:01.000000000Z"),
            )
            .expect("round admits");
        let state = processor.checkpoint_state();

        let mut resumed = ChainlinkOracleProcessor::new("oracle-epoch-2");
        resumed.restore_state(state).expect("state restores");
        // The first post-resume poll re-serves round 100 (== last published).
        let after = resumed
            .on_events(
                CHAINLINK_STREAM_LABEL,
                &[round_event(
                    "100",
                    0,
                    "77000.00000000",
                    "2026-09-03T00:00:00.000000000Z",
                )],
                &clock(2_000_000_000, "2026-09-03T00:00:02.000000000Z"),
            )
            .expect("first post-resume poll admits");
        assert_eq!(
            only_ref(&after).quality.state,
            QualityState::Ready,
            "the current round republishes"
        );
        assert_eq!(only_ref(&after).values["round_id"], json!("100"));
        assert!(
            only_ref(&after).quality.reasons.is_empty(),
            "a re-served round is never a reorg on resume"
        );
    }

    #[test]
    fn a_resumed_lane_republishes_before_its_first_poll_never_rpc_unavailable() {
        // Audit F7: an interval tick before the first post-resume poll republishes
        // the restored round (stale), never `chainlink_rpc_unavailable` for a feed
        // that has not failed once.
        let mut processor = ChainlinkOracleProcessor::new("oracle-test");
        processor
            .on_events(
                CHAINLINK_STREAM_LABEL,
                &[round_event(
                    "100",
                    0,
                    "77000.00000000",
                    "2026-09-03T00:00:00.000000000Z",
                )],
                &clock(1_000_000_000, "2026-09-03T00:00:01.000000000Z"),
            )
            .expect("round admits");
        let state = processor.checkpoint_state();

        let mut resumed = ChainlinkOracleProcessor::new("oracle-epoch-2");
        resumed.restore_state(state).expect("state restores");
        let tick = resumed
            .on_interval(&clock(2_000_000_000, "2026-09-03T00:00:02.000000000Z"))
            .expect("interval before any poll admits");
        assert_eq!(
            only_ref(&tick).quality.state,
            QualityState::Ready,
            "the restored round republishes before the first poll"
        );
        assert_eq!(only_ref(&tick).values["round_id"], json!("100"));
        // After a real poll failure (all endpoints down) the reason appears.
        let down = resumed
            .on_connectivity(
                CHAINLINK_STREAM_LABEL,
                false,
                &clock(3_000_000_000, "2026-09-03T00:00:03.000000000Z"),
            )
            .expect("disconnect admits");
        assert_eq!(
            only_ref(&down).quality.reasons,
            vec!["chainlink_rpc_unavailable".to_owned()],
            "an observed poll failure is unavailable"
        );
    }

    #[test]
    fn a_ready_reading_coalesces_on_the_interval_and_refreshes_when_due() {
        // Audit F4: the slice-1 coalescing contract. A 1 s tick over an unchanged
        // round emits nothing; a tick at or past the 60 s declared interval emits
        // one refreshed frame.
        let mut processor = ChainlinkOracleProcessor::new("oracle-test");
        let first = processor
            .on_events(
                CHAINLINK_STREAM_LABEL,
                &[round_event(
                    "100",
                    0,
                    "77000.00000000",
                    "2026-09-03T00:00:00.000000000Z",
                )],
                &clock(1_000_000_000, "2026-09-03T00:00:01.000000000Z"),
            )
            .expect("round admits");
        assert_eq!(only_ref(&first).sequence, 0);

        // One second later, same round: the reference is suppressed (its change key
        // is unchanged). The C2 identities stay sparse (one accepted round) and emit
        // their own non-ready evaluations, so only the reference is asserted here.
        let one_second = processor
            .on_interval(&clock(2_000_000_000, "2026-09-03T00:00:02.000000000Z"))
            .expect("1 s tick admits");
        assert_eq!(
            ref_count(&one_second),
            0,
            "an unchanged reference reading is suppressed within the interval"
        );

        // Sixty-one seconds after the publish: the interval has fallen due, so the
        // reading refreshes as exactly one frame with the next decision sequence.
        let refreshed = processor
            .on_interval(&clock(62_000_000_000, "2026-09-03T00:01:02.000000000Z"))
            .expect("60 s+ tick admits");
        assert_eq!(
            only_ref(&refreshed).sequence,
            1,
            "the refresh is the next reference decision"
        );
        assert_eq!(only_ref(&refreshed).quality.state, QualityState::Ready);
    }

    /// One deterministic input to the processor: a poll that served `round`, or an
    /// interval tick at `now_ns` / `produced_at`.
    #[derive(Clone)]
    enum Action {
        Poll {
            round: u128,
            now_ns: u64,
            produced_at: &'static str,
        },
        Tick {
            now_ns: u64,
            produced_at: &'static str,
        },
    }

    /// Apply one action; return the ready (round_id, value) it published, and
    /// assert it never carries a reorg reason (the F1 failure surfaces here).
    fn apply_action(
        processor: &mut ChainlinkOracleProcessor,
        action: &Action,
    ) -> Option<(String, String)> {
        let updates = match action {
            Action::Poll {
                round,
                now_ns,
                produced_at,
            } => processor
                .on_events(
                    CHAINLINK_STREAM_LABEL,
                    &[round_event(
                        &round.to_string(),
                        0,
                        "77000.00000000",
                        "2026-09-03T00:00:00.000000000Z",
                    )],
                    &clock(*now_ns, produced_at),
                )
                .expect("poll admits"),
            Action::Tick {
                now_ns,
                produced_at,
            } => processor
                .on_interval(&clock(*now_ns, produced_at))
                .expect("tick admits"),
        };
        for update in &updates {
            assert!(
                !update
                    .quality
                    .reasons
                    .iter()
                    .any(|reason| reason == "chainlink_reorg_detected"),
                "a resumed or steady lane must never falsely report a reorg"
            );
        }
        // Scope the reorg/republication invariant to the reference identity: the C2
        // momentum/volatility identities carry no round_id and their own readiness.
        updates
            .into_iter()
            .filter(|update| update.signal_id == CHAINLINK_SIGNAL_ID)
            .find_map(|update| {
                (update.quality.state == QualityState::Ready).then(|| {
                    (
                        update.values["round_id"].as_str().unwrap().to_owned(),
                        update.values["value"].as_str().unwrap().to_owned(),
                    )
                })
            })
    }

    #[test]
    fn oracle_lane_resume_at_every_frame_boundary() {
        // Spec §8 / audit F2: an uninterrupted run and a run interrupted and
        // resumed at EVERY input boundary agree on the ready value stream after the
        // resume point. The resume re-serves the current round (the real F1
        // condition), so a false reorg or a withheld value would diverge here.
        let actions = [
            Action::Poll {
                round: 100,
                now_ns: 1_000_000_000,
                produced_at: "2026-09-03T00:00:01.000000000Z",
            },
            Action::Tick {
                now_ns: 2_000_000_000,
                produced_at: "2026-09-03T00:00:02.000000000Z",
            },
            Action::Poll {
                round: 101,
                now_ns: 63_000_000_000,
                produced_at: "2026-09-03T00:01:03.000000000Z",
            },
            Action::Tick {
                now_ns: 64_000_000_000,
                produced_at: "2026-09-03T00:01:04.000000000Z",
            },
            Action::Poll {
                round: 102,
                now_ns: 125_000_000_000,
                produced_at: "2026-09-03T00:02:05.000000000Z",
            },
        ];

        // Reference: the uninterrupted run's ready value per action.
        let mut reference_proc = ChainlinkOracleProcessor::new("oracle-ref");
        let reference: Vec<Option<(String, String)>> = actions
            .iter()
            .map(|action| apply_action(&mut reference_proc, action))
            .collect();
        assert!(reference.iter().any(Option::is_some));

        // Resume at every boundary k = 1..len: run [0..k], checkpoint, restore,
        // run [k..], and check the resumed ready stream against the reference.
        for k in 1..actions.len() {
            let mut before = ChainlinkOracleProcessor::new("oracle-before");
            let current_round = actions[..k]
                .iter()
                .filter_map(|action| apply_action(&mut before, action))
                .map(|(round, _)| round)
                .next_back();
            let state = before.checkpoint_state();

            let mut after = ChainlinkOracleProcessor::new("oracle-after");
            after.restore_state(state).expect("state restores");
            let resumed: Vec<(String, String)> = actions[k..]
                .iter()
                .filter_map(|action| apply_action(&mut after, action))
                .collect();

            // Nothing the uninterrupted run publishes from the boundary onward is
            // lost or withheld on resume: the reference tail is a subsequence of the
            // resumed ready stream (F1 would drop the current round here).
            let expected_tail: Vec<(String, String)> =
                reference[k..].iter().filter_map(Clone::clone).collect();
            let mut it = resumed.iter();
            for expected in &expected_tail {
                assert!(
                    it.any(|value| value == expected),
                    "resume at boundary {k} lost {expected:?}"
                );
            }
            // Nothing spurious: every resumed round is either the reference tail or
            // the current round republished at the resume point (ADR 0027).
            let allowed: BTreeSet<String> = expected_tail
                .iter()
                .map(|(round, _)| round.clone())
                .chain(current_round.clone())
                .collect();
            for (round, _) in &resumed {
                assert!(
                    allowed.contains(round),
                    "resume at boundary {k} published a spurious round {round}"
                );
            }
        }
    }

    // --- C2: Chainlink price momentum / volatility (E49) -------------------

    #[test]
    fn c2_a_single_round_still_warms_up_for_momentum_and_volatility() {
        // Spec §6 sparse case, refined by the audit MAJOR: one accepted round
        // satisfies neither the fifteen-minute horizon (needs two rounds in the
        // window) nor the count horizon (needs two rounds at all), so both C2
        // identities are not ready and carry no values. Because they have never
        // been ready, the reason is the Warming-class `chainlink_warming_up`, not
        // the Degraded `chainlink_rounds_sparse` (which is reserved for a window
        // that has been ready and then thins below two rounds).
        let mut processor = ChainlinkOracleProcessor::new("oracle-test");
        let updates = processor
            .on_events(
                CHAINLINK_STREAM_LABEL,
                &[round_event(
                    "100",
                    0,
                    "100.00000000",
                    "2026-09-03T11:59:00.000000000Z",
                )],
                &clock(1_000_000_000, "2026-09-03T12:00:00.000000000Z"),
            )
            .expect("round admits");
        for identity in [momentum_update(&updates), volatility_update(&updates)] {
            assert_eq!(identity.quality.state, QualityState::WarmingUp);
            assert_eq!(
                identity.quality.reasons,
                vec![CHAINLINK_WARMING_UP.to_owned()]
            );
            assert!(identity.values.is_empty());
        }
    }

    #[test]
    fn c2_ready_with_two_rounds_in_fifteen_minutes_hand_verifiable() {
        // Spec §6: a ready case with two rounds inside the fifteen-minute window,
        // plus a third anchor twenty minutes old. NOW = 12:00:00.
        //   r0 = 100 @ 11:40 (20m old, the newest round >= 15m old)
        //   r1 = 110 @ 11:50 (10m old, in 15m)
        //   r2 = 121 @ 11:55 ( 5m old, in 15m)
        // Momentum: return_bps_15m = (121-100)/100*1e4 = 2100; return_bps_1h/4h = 0
        // (no round >= 1h/4h old); last-1-round = (121-110)/110*1e4 = 1000;
        // last-5-rounds clamps to the oldest = 2100; direction_1h flat; rounds_in_4h
        // = 3; newest age 300000 ms.
        // Volatility: per-round returns are (110/100)=1000 and (121/110)=1000 bps,
        // both inside every window, so RMS = sqrt((1000^2+1000^2)/2) = 1000 for 15m,
        // 1h and 4h; short_long_ratio = 1000/1000 = 1 -> stable.
        let mut processor = ChainlinkOracleProcessor::new("oracle-test");
        let updates = processor
            .on_events(
                CHAINLINK_STREAM_LABEL,
                &[
                    round_event("100", 0, "100.00000000", "2026-09-03T11:40:00.000000000Z"),
                    round_event("101", 0, "110.00000000", "2026-09-03T11:50:00.000000000Z"),
                    round_event("102", 0, "121.00000000", "2026-09-03T11:55:00.000000000Z"),
                ],
                &clock(3_000_000_000, "2026-09-03T12:00:00.000000000Z"),
            )
            .expect("rounds admit");

        let momentum = momentum_update(&updates);
        assert_eq!(momentum.quality.state, QualityState::Ready);
        assert_eq!(momentum.values["return_bps_15m"], json!("2100"));
        assert_eq!(momentum.values["return_bps_1h"], json!("0"));
        assert_eq!(momentum.values["return_bps_4h"], json!("0"));
        assert_eq!(momentum.values["return_bps_last_1_round"], json!("1000"));
        assert_eq!(momentum.values["return_bps_last_5_rounds"], json!("2100"));
        assert_eq!(momentum.values["direction_1h"], json!("flat"));
        assert_eq!(momentum.values["rounds_in_4h"], json!(3u16));
        assert_eq!(momentum.values["newest_round_age_ms"], json!(300_000u64));
        assert_eq!(
            momentum.values["window_close"],
            json!("2026-09-03T12:00:00.000000000Z")
        );

        let volatility = volatility_update(&updates);
        assert_eq!(volatility.quality.state, QualityState::Ready);
        assert_eq!(volatility.values["volatility_bps_15m"], json!("1000"));
        assert_eq!(volatility.values["volatility_bps_1h"], json!("1000"));
        assert_eq!(volatility.values["volatility_bps_4h"], json!("1000"));
        assert_eq!(volatility.values["short_long_ratio"], json!("1"));
        assert_eq!(volatility.values["regime"], json!("stable"));
        assert_eq!(volatility.values["rounds_in_4h"], json!(3u16));
        assert_eq!(volatility.values["newest_round_age_ms"], json!(300_000u64));
    }

    #[test]
    fn c2_count_horizon_momentum_is_ready_regardless_of_age() {
        // Spec §6: count-horizon momentum with the last-1 / last-5-round returns,
        // regardless of age. Six rounds, all older than fifteen minutes (so the
        // fifteen-minute horizon holds none), still make the identity ready through
        // the count horizon. NOW = 12:00:00, prices chosen so the two count returns
        // differ and are both clean:
        //   r0 = 125 @ 08:30, r1 = 120 @ 09:00, r2 = 115 @ 10:00,
        //   r3 = 110 @ 10:30, r4 = 100 @ 11:00, r5 = 110 @ 11:40.
        // last-1-round  = (110-100)/100*1e4 =  1000
        // last-5-rounds = (110-125)/125*1e4 = -1200
        let mut processor = ChainlinkOracleProcessor::new("oracle-test");
        let updates = processor
            .on_events(
                CHAINLINK_STREAM_LABEL,
                &[
                    round_event("100", 0, "125.00000000", "2026-09-03T08:30:00.000000000Z"),
                    round_event("101", 0, "120.00000000", "2026-09-03T09:00:00.000000000Z"),
                    round_event("102", 0, "115.00000000", "2026-09-03T10:00:00.000000000Z"),
                    round_event("103", 0, "110.00000000", "2026-09-03T10:30:00.000000000Z"),
                    round_event("104", 0, "100.00000000", "2026-09-03T11:00:00.000000000Z"),
                    round_event("105", 0, "110.00000000", "2026-09-03T11:40:00.000000000Z"),
                ],
                &clock(4_000_000_000, "2026-09-03T12:00:00.000000000Z"),
            )
            .expect("rounds admit");

        let momentum = momentum_update(&updates);
        assert_eq!(
            momentum.quality.state,
            QualityState::Ready,
            "ready through the count horizon with no round inside 15m"
        );
        assert_eq!(momentum.values["return_bps_last_1_round"], json!("1000"));
        assert_eq!(momentum.values["return_bps_last_5_rounds"], json!("-1200"));
        // All six rounds are at least fifteen minutes old, so the fifteen-minute
        // time return has no in-window anchor beyond the newest round itself: zero.
        assert_eq!(momentum.values["return_bps_15m"], json!("0"));
        assert_eq!(momentum.values["rounds_in_4h"], json!(6u16));
        assert_eq!(momentum.values["newest_round_age_ms"], json!(1_200_000u64));
    }

    #[test]
    fn c2_ring_survives_checkpoint_restore() {
        // The accepted-round ring is checkpointed, so a resumed processor reproduces
        // the ready momentum/volatility frames without re-warming a sparse window.
        let mut processor = ChainlinkOracleProcessor::new("oracle-test");
        processor
            .on_events(
                CHAINLINK_STREAM_LABEL,
                &[
                    round_event("100", 0, "100.00000000", "2026-09-03T11:40:00.000000000Z"),
                    round_event("101", 0, "110.00000000", "2026-09-03T11:50:00.000000000Z"),
                    round_event("102", 0, "121.00000000", "2026-09-03T11:55:00.000000000Z"),
                ],
                &clock(3_000_000_000, "2026-09-03T12:00:00.000000000Z"),
            )
            .expect("rounds admit");
        let state = processor.checkpoint_state();

        let mut resumed = ChainlinkOracleProcessor::new("oracle-epoch-2");
        resumed.restore_state(state).expect("state restores");
        // An interval tick before any post-resume poll: the restored ring publishes
        // ready momentum/volatility immediately (no re-warm).
        let tick = resumed
            .on_interval(&clock(4_000_000_000, "2026-09-03T12:00:01.000000000Z"))
            .expect("interval admits");
        let momentum = momentum_update(&tick);
        assert_eq!(momentum.quality.state, QualityState::Ready);
        assert_eq!(momentum.values["return_bps_last_1_round"], json!("1000"));
        assert_eq!(momentum.sequence, 0, "the C2 sequence restarts on resume");
        assert_eq!(volatility_update(&tick).quality.state, QualityState::Ready);
    }
}
