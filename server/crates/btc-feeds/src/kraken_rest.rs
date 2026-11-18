//! Observe-only Kraken public-trade reconciliation (SIG-041, ADR 0013).
//!
//! A live-runner-only maintenance job periodically fetches one bounded page of
//! recent Kraken public trades over REST and cross-checks their `trade_id`
//! membership against the WebSocket-observed trade tape. It is *strictly
//! observe-only*: a divergence produces a report entry and an escalation-log
//! notice and nothing else. It never mutates adapter state, never triggers a
//! recovery action, never feeds the feed-health board, and never touches the
//! journal. Its state lives entirely in the live runner, so it can never enter
//! admitted-record state and replay is unaffected by construction.
//!
//! Kraken trade IDs are book-scoped and are not a `previous + 1` completeness
//! primitive (`SIG-014`); a REST id absent from the observed tape is therefore
//! reported as a *candidate* silent loss for human calibration, never acted on.

use std::collections::BTreeSet;
use std::time::Duration;

use reqwest::header::{ACCEPT, CACHE_CONTROL, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use thiserror::Error;

use crate::secure_http::{self, SecureHttpError};
use signals_core::SourceEvent;

const TRADES_URL: &str = "https://api.kraken.com/0/public/Trades";
const PAIR_QUERY: &str = "XBTUSD";
const RESULT_PAIR_KEY: &str = "XXBTZUSD";
/// One bounded page per cycle. Kraken caps a public trades page at 1000 rows.
const PAGE_COUNT: usize = 1_000;
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
/// The BTC/USD WebSocket trade source whose ids form the observed tape.
const BTC_TRADE_SOURCE_ID: &str = "kraken.spot.btc-usd.trades";
/// Bounded retained observed window. Smallest ids are evicted past this.
const MAX_OBSERVED_TRADE_IDS: usize = 200_000;
/// Bounded run-report growth; every retained kind fails closed to a counter.
const MAX_REPORT_ENTRIES: usize = 256;
const MAX_ESCALATIONS: usize = 256;
const MAX_MISSING_SAMPLE: usize = 32;
const MAX_DETAIL_BYTES: usize = 200;

/// Fixed-host, bounded Kraken public-trade reconciliation client and state.
pub struct KrakenTradeReconciler {
    client: reqwest::Client,
    cadence_ms: u64,
    observed: BTreeSet<u64>,
    cycles: u64,
    divergent_cycles: u64,
    inconclusive_cycles: u64,
    fetch_failures: u64,
    observed_evicted: u64,
    entries: Vec<KrakenReconciliationEntry>,
    entries_dropped: u64,
    escalations: Vec<KrakenReconciliationEscalation>,
    escalations_dropped: u64,
}

impl KrakenTradeReconciler {
    /// Build a reconciler on the shared secure-HTTP boundary.
    pub fn new(cadence: Duration) -> Result<Self, KrakenRestError> {
        Ok(Self {
            client: secure_http::client()?,
            cadence_ms: u64::try_from(cadence.as_millis()).unwrap_or(u64::MAX),
            observed: BTreeSet::new(),
            cycles: 0,
            divergent_cycles: 0,
            inconclusive_cycles: 0,
            fetch_failures: 0,
            observed_evicted: 0,
            entries: Vec::new(),
            entries_dropped: 0,
            escalations: Vec::new(),
            escalations_dropped: 0,
        })
    }

    /// Record every admitted BTC/USD trade id from a normalized outcome.
    ///
    /// This reads the `SourceEvent`s the live pipeline emits — the ids the
    /// adapter actually *admitted*, from both snapshot and update trades. A
    /// trade the venue delivered on the wire but the adapter dropped as a
    /// duplicate or out-of-order id (`kraken.rs`) emits no event, so it never
    /// enters this tape; the observe-only reconciliation may then surface it as
    /// a candidate divergence, which is reported for calibration and never acted
    /// on. Ids are copied into live-runner-local state and nothing is written
    /// back.
    pub fn observe_events(&mut self, events: &[SourceEvent]) {
        for event in events {
            if event.source_id == BTC_TRADE_SOURCE_ID
                && let Some(id) = event
                    .values
                    .get("trade_id")
                    .and_then(serde_json::Value::as_u64)
            {
                self.observed.insert(id);
            }
        }
        while self.observed.len() > MAX_OBSERVED_TRADE_IDS {
            let Some(smallest) = self.observed.iter().next().copied() else {
                break;
            };
            self.observed.remove(&smallest);
            self.observed_evicted = self.observed_evicted.saturating_add(1);
        }
    }

    /// Build one spawnable bounded REST fetch.
    ///
    /// The returned future owns a cloned client and borrows nothing, so the live
    /// runner runs it as an independent task off the receive loop and only does
    /// the cheap, synchronous [`Self::ingest_fetch_outcome`] on completion. This
    /// keeps socket-receipt stamping unaffected by REST latency — the fetch never
    /// blocks the loop that reads and timestamps provider messages.
    pub fn spawn_fetch(&self) -> impl std::future::Future<Output = FetchOutcome> + Send + 'static {
        let client = self.client.clone();
        async move { fetch_page(client).await }
    }

    /// Fold one completed fetch into the observe-only results. Pure and
    /// non-blocking: a REST outage or malformed page becomes a failed cycle so
    /// the live run is never affected by the observe-only maintenance job.
    pub fn ingest_fetch_outcome(&mut self, outcome: FetchOutcome) {
        match outcome {
            Ok(page) => self.record_cycle(&page),
            Err(error) => self.record_fetch_failure(&error.to_string()),
        }
    }

    /// Record a reconciliation task that panicked or was cancelled as a failed
    /// cycle. Never a run error: the maintenance job stays observe-only.
    pub fn record_task_failure(&mut self) {
        self.record_fetch_failure("reconciliation task did not complete");
    }

    fn record_cycle(&mut self, page: &FetchedPage) {
        self.cycles = self.cycles.saturating_add(1);
        let cycle = self.cycles;
        match evaluate(&self.observed, page) {
            CycleResult::Matched { in_window, window } => {
                self.push_entry(KrakenReconciliationEntry {
                    cycle,
                    outcome: "matched",
                    rest_trades: as_u64(page.rows),
                    rest_trades_in_window: in_window,
                    window_low: Some(window.0),
                    window_high: Some(window.1),
                    missing_trade_ids: 0,
                    missing_sample: Vec::new(),
                    detail: None,
                });
            }
            CycleResult::Divergent {
                in_window,
                window,
                missing,
            } => {
                self.divergent_cycles = self.divergent_cycles.saturating_add(1);
                let missing_count = as_u64(missing.len());
                let sample = bounded_sample(&missing);
                self.push_entry(KrakenReconciliationEntry {
                    cycle,
                    outcome: "divergent",
                    rest_trades: as_u64(page.rows),
                    rest_trades_in_window: in_window,
                    window_low: Some(window.0),
                    window_high: Some(window.1),
                    missing_trade_ids: missing_count,
                    missing_sample: sample.clone(),
                    detail: None,
                });
                let escalation = KrakenReconciliationEscalation {
                    cycle,
                    missing_trade_ids: missing_count,
                    missing_sample: sample,
                    window_low: window.0,
                    window_high: window.1,
                };
                // Emit the escalation-log notice as it happens (live-only,
                // bounded, one line) so a killed run never loses divergence
                // evidence; the run report retains it too, and the binary's
                // end-of-run summary uses this same format.
                eprintln!("{}", escalation.log_notice());
                self.push_escalation(escalation);
            }
            CycleResult::Inconclusive { reason } => {
                self.inconclusive_cycles = self.inconclusive_cycles.saturating_add(1);
                self.push_entry(KrakenReconciliationEntry {
                    cycle,
                    outcome: "inconclusive",
                    rest_trades: as_u64(page.rows),
                    rest_trades_in_window: 0,
                    window_low: None,
                    window_high: None,
                    missing_trade_ids: 0,
                    missing_sample: Vec::new(),
                    detail: Some(reason.to_owned()),
                });
            }
        }
    }

    fn record_fetch_failure(&mut self, detail: &str) {
        self.cycles = self.cycles.saturating_add(1);
        self.fetch_failures = self.fetch_failures.saturating_add(1);
        let cycle = self.cycles;
        self.push_entry(KrakenReconciliationEntry {
            cycle,
            outcome: "fetch_failed",
            rest_trades: 0,
            rest_trades_in_window: 0,
            window_low: None,
            window_high: None,
            missing_trade_ids: 0,
            missing_sample: Vec::new(),
            detail: Some(bounded_detail(detail)),
        });
    }

    fn push_entry(&mut self, entry: KrakenReconciliationEntry) {
        if self.entries.len() >= MAX_REPORT_ENTRIES {
            self.entries_dropped = self.entries_dropped.saturating_add(1);
            return;
        }
        self.entries.push(entry);
    }

    fn push_escalation(&mut self, escalation: KrakenReconciliationEscalation) {
        if self.escalations.len() >= MAX_ESCALATIONS {
            self.escalations_dropped = self.escalations_dropped.saturating_add(1);
            return;
        }
        self.escalations.push(escalation);
    }

    /// Freeze the observe-only results for the run report.
    pub fn into_summary(self) -> KrakenReconciliationSummary {
        KrakenReconciliationSummary {
            cadence_ms: self.cadence_ms,
            cycles: self.cycles,
            divergent_cycles: self.divergent_cycles,
            inconclusive_cycles: self.inconclusive_cycles,
            fetch_failures: self.fetch_failures,
            observed_trade_ids: as_u64(self.observed.len()),
            observed_evicted: self.observed_evicted,
            entries: self.entries,
            entries_dropped: self.entries_dropped,
            escalations: self.escalations,
            escalations_dropped: self.escalations_dropped,
        }
    }
}

/// The observe-only reconciliation results, attached to the live run report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct KrakenReconciliationSummary {
    /// Configured fetch cadence in milliseconds, saturating at `u64::MAX`.
    pub cadence_ms: u64,
    /// Cycles recorded, counting matched, divergent, inconclusive and failed
    /// fetches alike.
    pub cycles: u64,
    /// Cycles with at least one in-window REST id absent from the tape.
    pub divergent_cycles: u64,
    /// Cycles that could not be judged: no tape yet, no ids on the page, no
    /// window overlap, or a page missing numeric ids.
    pub inconclusive_cycles: u64,
    /// Cycles whose REST fetch errored, panicked or was cancelled.
    pub fetch_failures: u64,
    /// Distinct trade ids retained in the observed tape at end of run.
    pub observed_trade_ids: u64,
    /// Smallest-first evictions from the tape past its 200,000-id bound.
    pub observed_evicted: u64,
    /// Per-cycle entries in cycle order, capped at 256.
    pub entries: Vec<KrakenReconciliationEntry>,
    /// Entries discarded once that 256-entry cap was reached.
    pub entries_dropped: u64,
    /// Divergence notices in cycle order, capped at 256.
    pub escalations: Vec<KrakenReconciliationEscalation>,
    /// Escalations discarded once that 256-entry cap was reached.
    pub escalations_dropped: u64,
}

/// One reconciliation cycle's distinct report entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct KrakenReconciliationEntry {
    /// The 1-based cycle ordinal this entry describes.
    pub cycle: u64,
    /// The cycle verdict: `matched`, `divergent`, `inconclusive` or
    /// `fetch_failed`.
    pub outcome: &'static str,
    /// Rows in the fetched REST page, including rows with no numeric id. Zero
    /// on `fetch_failed`.
    pub rest_trades: u64,
    /// Rows whose id fell inside the retained observed window and were
    /// therefore judged. Zero unless the outcome is `matched` or `divergent`.
    pub rest_trades_in_window: u64,
    /// Smallest retained observed trade id; `None` when no window was judged.
    pub window_low: Option<u64>,
    /// Largest retained observed trade id; `None` when no window was judged.
    pub window_high: Option<u64>,
    /// Count of in-window REST ids absent from the observed tape. Zero unless
    /// the outcome is `divergent`.
    pub missing_trade_ids: u64,
    /// The first 32 of those ids, in ascending REST-page order.
    pub missing_sample: Vec<u64>,
    /// Why the cycle was inconclusive, or the fetch-failure cause truncated to
    /// 200 bytes with control characters replaced. `None` otherwise.
    pub detail: Option<String>,
}

/// A divergence escalation-log notice: candidate silent trade loss for review.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct KrakenReconciliationEscalation {
    /// The 1-based reconciliation cycle that diverged.
    pub cycle: u64,
    /// Count of in-window REST trade ids absent from the observed tape.
    pub missing_trade_ids: u64,
    /// The first 32 of those ids, in ascending REST-page order.
    pub missing_sample: Vec<u64>,
    /// Smallest retained observed trade id; the inclusive lower judged bound.
    pub window_low: u64,
    /// Largest retained observed trade id; the inclusive upper judged bound.
    pub window_high: u64,
}

impl KrakenReconciliationEscalation {
    /// The canonical one-line escalation-log notice, shared by the live
    /// as-it-happens emission and the binary's end-of-run summary so both use
    /// exactly one format.
    #[must_use]
    pub fn log_notice(&self) -> String {
        format!(
            "kraken-trade-reconciliation escalation: cycle {} found {} REST trade(s) in window [{}, {}] absent from the observed tape (sample: {:?})",
            self.cycle,
            self.missing_trade_ids,
            self.window_low,
            self.window_high,
            self.missing_sample,
        )
    }
}

/// One completed REST fetch, ready to fold into the observe-only results.
pub type FetchOutcome = Result<FetchedPage, KrakenRestError>;

/// One bounded page of recent Kraken public trades (fields are opaque to
/// callers; a completed page is folded in via `ingest_fetch_outcome`).
pub struct FetchedPage {
    rows: usize,
    trade_ids: Vec<u64>,
    rows_without_id: usize,
}

async fn fetch_page(client: reqwest::Client) -> FetchOutcome {
    let response = client
        .get(TRADES_URL)
        .header(ACCEPT, HeaderValue::from_static("application/json"))
        .header(CACHE_CONTROL, HeaderValue::from_static("no-cache"))
        .query(&[("pair", PAIR_QUERY)])
        .query(&[("count", PAGE_COUNT)])
        .send()
        .await
        .map_err(|_| KrakenRestError::Http)?;
    let body = secure_http::bounded_body(response, MAX_RESPONSE_BYTES).await?;
    parse_trades(&body)
}

enum CycleResult {
    Matched {
        in_window: u64,
        window: (u64, u64),
    },
    Divergent {
        in_window: u64,
        window: (u64, u64),
        missing: Vec<u64>,
    },
    Inconclusive {
        reason: &'static str,
    },
}

/// Pure comparison of one REST page against the observed trade tape.
///
/// Only REST ids inside the retained observed window `[min, max]` are judged:
/// ids above the observed maximum may simply not have reached the WebSocket
/// yet, and ids below the retained minimum were evicted from the bounded tape.
fn evaluate(observed: &BTreeSet<u64>, page: &FetchedPage) -> CycleResult {
    if page.rows_without_id > 0 {
        return CycleResult::Inconclusive {
            reason: "Kraken REST omitted numeric trade ids",
        };
    }
    let (Some(&window_low), Some(&window_high)) =
        (observed.iter().next(), observed.iter().next_back())
    else {
        return CycleResult::Inconclusive {
            reason: "no observed trade tape yet",
        };
    };
    if page.trade_ids.is_empty() {
        return CycleResult::Inconclusive {
            reason: "Kraken REST returned no trades",
        };
    }
    let mut in_window = 0_u64;
    let mut missing = Vec::new();
    for &id in &page.trade_ids {
        if id < window_low || id > window_high {
            continue;
        }
        in_window = in_window.saturating_add(1);
        if !observed.contains(&id) {
            missing.push(id);
        }
    }
    if in_window == 0 {
        return CycleResult::Inconclusive {
            reason: "no overlap between the REST page and the observed window",
        };
    }
    if missing.is_empty() {
        CycleResult::Matched {
            in_window,
            window: (window_low, window_high),
        }
    } else {
        CycleResult::Divergent {
            in_window,
            window: (window_low, window_high),
            missing,
        }
    }
}

fn parse_trades(body: &[u8]) -> Result<FetchedPage, KrakenRestError> {
    let response: KrakenTradesResponse =
        serde_json::from_slice(body).map_err(|_| KrakenRestError::InvalidResponse)?;
    if !response.error.is_empty() {
        return Err(KrakenRestError::Provider);
    }
    let result = response.result.ok_or(KrakenRestError::InvalidResponse)?;
    let raw_pair = result
        .get(RESULT_PAIR_KEY)
        .ok_or(KrakenRestError::InvalidResponse)?;
    let rows: Vec<Vec<serde_json::Value>> =
        serde_json::from_str(raw_pair.get()).map_err(|_| KrakenRestError::InvalidResponse)?;
    if rows.len() > PAGE_COUNT {
        return Err(KrakenRestError::InvalidResponse);
    }
    let mut trade_ids = Vec::with_capacity(rows.len());
    let mut rows_without_id = 0_usize;
    for row in &rows {
        match row.get(6).and_then(serde_json::Value::as_u64) {
            Some(id) => trade_ids.push(id),
            None => rows_without_id = rows_without_id.saturating_add(1),
        }
    }
    Ok(FetchedPage {
        rows: rows.len(),
        trade_ids,
        rows_without_id,
    })
}

#[derive(Deserialize)]
struct KrakenTradesResponse {
    #[serde(default)]
    error: Vec<String>,
    result: Option<std::collections::BTreeMap<String, Box<RawValue>>>,
}

fn bounded_sample(ids: &[u64]) -> Vec<u64> {
    ids.iter().take(MAX_MISSING_SAMPLE).copied().collect()
}

fn bounded_detail(value: &str) -> String {
    let mut end = value.len().min(MAX_DETAIL_BYTES);
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    // Provider bytes reach operator terminals via the escalation log; control
    // characters are replaced so payloads cannot inject terminal sequences.
    value[..end]
        .chars()
        .map(|character| {
            if character.is_control() {
                char::REPLACEMENT_CHARACTER
            } else {
                character
            }
        })
        .collect()
}

fn as_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

/// Why one observe-only REST fetch could not produce a page. Every variant is
/// folded into a `fetch_failed` cycle and never fails the live run.
#[derive(Debug, Error)]
pub enum KrakenRestError {
    /// The secure-HTTP boundary rejected the request, the transport failed, or
    /// the bounded body exceeded the 4 MiB response cap.
    #[error("secure Kraken HTTP request failed")]
    Http,
    /// The body did not deserialize, omitted the `XXBTZUSD` result key, or
    /// carried more than the 1000-row page Kraken caps a trades page at.
    #[error("Kraken returned an invalid bounded trade response")]
    InvalidResponse,
    /// Kraken answered with a non-empty `error` array instead of a page.
    #[error("Kraken REST returned a provider error array")]
    Provider,
}

impl From<SecureHttpError> for KrakenRestError {
    fn from(_: SecureHttpError) -> Self {
        Self::Http
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::Duration;

    use serde_json::json;
    use signals_core::SourceEvent;

    use super::{FetchedPage, KrakenRestError, KrakenTradeReconciler, parse_trades};

    fn reconciler() -> KrakenTradeReconciler {
        KrakenTradeReconciler::new(Duration::from_secs(30)).expect("reconciler constructs")
    }

    fn trade_event(trade_id: u64) -> SourceEvent {
        SourceEvent {
            schema_version: 1,
            source_id: "kraken.spot.btc-usd.trades".to_owned(),
            source_version: "1.0.0".to_owned(),
            publisher_epoch: "test".to_owned(),
            sequence: trade_id,
            event_time: "2026-08-16T12:00:00Z".to_owned(),
            received_at: "2026-08-16T12:00:00Z".to_owned(),
            values: BTreeMap::from([("trade_id".to_owned(), json!(trade_id))]),
        }
    }

    fn page(trade_ids: &[u64]) -> FetchedPage {
        FetchedPage {
            rows: trade_ids.len(),
            trade_ids: trade_ids.to_vec(),
            rows_without_id: 0,
        }
    }

    #[test]
    fn observe_events_records_only_btc_trade_ids_and_bounds_the_window() {
        let mut reconciler = reconciler();
        // A USDT trade and a non-trade event must not enter the BTC tape.
        let usdt = SourceEvent {
            source_id: "kraken.spot.usdt-usd.trades".to_owned(),
            ..trade_event(999)
        };
        let ticker = SourceEvent {
            source_id: "kraken.spot.btc-usd.ticker".to_owned(),
            ..trade_event(998)
        };
        reconciler.observe_events(&[trade_event(10), trade_event(11), usdt, ticker]);
        assert_eq!(reconciler.observed.len(), 2);
        assert!(reconciler.observed.contains(&10));
        assert!(reconciler.observed.contains(&11));
    }

    #[test]
    fn matching_window_records_a_clean_report_entry_without_escalation() {
        let mut reconciler = reconciler();
        reconciler.observe_events(&[trade_event(100), trade_event(101), trade_event(102)]);
        // REST page fully covers the observed window; id 103 is above the
        // observed maximum (not yet on WS) and is correctly ignored.
        reconciler.record_cycle(&page(&[100, 101, 102, 103]));

        assert_eq!(reconciler.cycles, 1);
        assert_eq!(reconciler.divergent_cycles, 0);
        assert!(reconciler.escalations.is_empty());
        let entry = &reconciler.entries[0];
        assert_eq!(entry.outcome, "matched");
        assert_eq!(entry.rest_trades_in_window, 3);
        assert_eq!(entry.missing_trade_ids, 0);
        assert_eq!(entry.window_high, Some(102));
    }

    #[test]
    fn divergent_window_records_entry_and_escalation_without_mutating_the_tape() {
        let mut reconciler = reconciler();
        reconciler.observe_events(&[trade_event(200), trade_event(202)]);
        // REST reports 201 within the observed window [200, 202]; WS never
        // delivered it -> a candidate silent loss: report entry + escalation.
        reconciler.record_cycle(&page(&[200, 201, 202]));

        assert_eq!(reconciler.divergent_cycles, 1);
        let entry = &reconciler.entries[0];
        assert_eq!(entry.outcome, "divergent");
        assert_eq!(entry.missing_trade_ids, 1);
        assert_eq!(entry.missing_sample, vec![201]);
        assert_eq!(reconciler.escalations.len(), 1);
        assert_eq!(reconciler.escalations[0].missing_sample, vec![201]);
        assert_eq!(reconciler.escalations[0].window_high, 202);
        // Strictly observe-only: the divergence never repaired the tape.
        assert!(!reconciler.observed.contains(&201));
        assert_eq!(reconciler.observed.len(), 2);
    }

    #[test]
    fn an_empty_tape_or_a_non_overlapping_page_is_inconclusive_never_divergent() {
        let mut reconciler = reconciler();
        reconciler.record_cycle(&page(&[1, 2, 3]));
        assert_eq!(reconciler.inconclusive_cycles, 1);
        assert_eq!(reconciler.divergent_cycles, 0);

        reconciler.observe_events(&[trade_event(500), trade_event(501)]);
        // A page entirely above the observed maximum has no overlap.
        reconciler.record_cycle(&page(&[600, 601]));
        assert_eq!(reconciler.inconclusive_cycles, 2);
        assert_eq!(reconciler.divergent_cycles, 0);
        assert!(reconciler.escalations.is_empty());
    }

    #[test]
    fn parse_extracts_trade_ids_and_flags_rows_without_numeric_ids() {
        let body = json!({
            "error": [],
            "result": {
                "XXBTZUSD": [
                    ["60000.0", "0.10", 1786726500.1, "b", "l", "", 42_u64],
                    ["60001.0", "0.20", 1786726501.2, "s", "m", "", 43_u64]
                ],
                "last": "1786726501200000000"
            }
        })
        .to_string();
        let parsed = parse_trades(body.as_bytes()).expect("valid Kraken trades response parses");
        assert_eq!(parsed.rows, 2);
        assert_eq!(parsed.trade_ids, vec![42, 43]);
        assert_eq!(parsed.rows_without_id, 0);

        // A legacy six-element row (no numeric id) is flagged, not silently kept.
        let legacy = json!({
            "error": [],
            "result": {"XXBTZUSD": [["60000.0", "0.10", 1786726500.1, "b", "l", ""]], "last": "0"}
        })
        .to_string();
        let parsed = parse_trades(legacy.as_bytes()).expect("legacy row parses");
        assert_eq!(parsed.rows_without_id, 1);
        assert!(parsed.trade_ids.is_empty());
    }

    #[test]
    fn a_provider_error_array_is_a_fetch_failure_not_a_divergence() {
        let body = json!({"error": ["EGeneral:Invalid arguments"], "result": null}).to_string();
        assert!(parse_trades(body.as_bytes()).is_err());
    }

    #[test]
    fn completion_folding_is_synchronous_and_covers_pages_errors_and_task_failure() {
        // The on-completion work (what the live loop does after a spawned fetch
        // resolves) is a synchronous, non-blocking fold — this is the property
        // that keeps socket-receipt stamping unaffected by REST latency.
        let mut reconciler = reconciler();
        reconciler.observe_events(&[trade_event(300), trade_event(301)]);
        reconciler.ingest_fetch_outcome(Ok(page(&[300, 301])));
        assert_eq!(reconciler.cycles, 1);
        assert_eq!(reconciler.entries[0].outcome, "matched");

        // A fetch error folds into a failed cycle, never a run error.
        reconciler.ingest_fetch_outcome(Err(KrakenRestError::Http));
        assert_eq!(reconciler.fetch_failures, 1);
        assert_eq!(reconciler.entries[1].outcome, "fetch_failed");

        // A task panic/join error is recorded as a failed cycle too.
        reconciler.record_task_failure();
        assert_eq!(reconciler.fetch_failures, 2);
        assert_eq!(reconciler.entries[2].outcome, "fetch_failed");
    }

    #[test]
    fn fetch_failure_detail_is_capped_by_bytes_at_a_char_boundary() {
        let mut reconciler = reconciler();
        // 500 two-byte characters = 1000 bytes; the cap is on bytes, not chars.
        let long = "é".repeat(500);
        reconciler.record_fetch_failure(&long);
        let detail = reconciler.entries[0]
            .detail
            .as_ref()
            .expect("fetch failure carries a detail");
        assert!(detail.len() <= super::MAX_DETAIL_BYTES);
        // Truncation kept a valid UTF-8 boundary (100 whole 'é' = 200 bytes).
        assert_eq!(detail.len(), 200);
        assert!(detail.chars().all(|c| c == 'é'));
    }
}
