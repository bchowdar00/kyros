//! The one plausibility window every millisecond event timestamp in this crate
//! is checked against, and the shared millisecond-to-RFC-3339 conversion built
//! on it. Keeping the bound in a single place is what makes every adapter and
//! the latency analyzer agree on which timestamps are admissible.

use chrono::{DateTime, SecondsFormat};

use crate::AdapterError;

// Plausibility window shared by every millisecond event timestamp this crate
// parses, ~2001-09-09 to ~2100-01-01. Provider feeds report Unix milliseconds,
// but some historical archives switched individual feeds to MICROSECONDS; a
// microsecond value is ~1000x larger and lands far above this ceiling, so the
// bound fails such a unit mismatch closed instead of emitting a year-56000
// event. One authority for the window keeps every adapter and the latency
// analyzer in agreement.
/// Inclusive lower bound of the admissible window, ~2001-09-09T01:46:40Z in
/// Unix milliseconds.
pub(crate) const MIN_EVENT_MILLIS: i64 = 1_000_000_000_000;
/// Inclusive upper bound of the admissible window, 2100-01-01T00:00:00Z in
/// Unix milliseconds. A microsecond value lands far above this and is
/// rejected as a unit mismatch.
pub(crate) const MAX_EVENT_MILLIS: i64 = 4_102_444_800_000;

/// Convert Unix milliseconds to an RFC 3339 timestamp, rejecting values outside
/// the shared plausibility window as a unit mismatch.
pub(crate) fn millis_to_rfc3339(millis: i64, field: &str) -> Result<String, AdapterError> {
    if !(MIN_EVENT_MILLIS..=MAX_EVENT_MILLIS).contains(&millis) {
        return Err(AdapterError::InvalidPayload(format!(
            "{field} millisecond timestamp {millis} is implausible; expected \
             {MIN_EVENT_MILLIS}..={MAX_EVENT_MILLIS} (a value outside this window \
             indicates a unit mismatch such as microseconds)"
        )));
    }
    DateTime::from_timestamp_millis(millis)
        .map(|value| value.to_rfc3339_opts(SecondsFormat::Millis, true))
        .ok_or_else(|| {
            AdapterError::InvalidPayload(format!(
                "{field} millisecond timestamp {millis} is out of range"
            ))
        })
}
