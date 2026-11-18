//! A record of how every round was resolved, checked against Gemini.
//!
//! A round resolved on Gemini's own result agrees with Gemini by
//! construction. A round resolved provisionally (Gemini was late) is
//! the one that can be wrong, so the audit goes back for Gemini's
//! result and counts any round where the two differ. It also counts,
//! for every round, whether the contract's stated rule applied to the
//! socket's index price would have given Gemini's answer: that is the
//! rule the provisional path relies on.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rust_decimal::Decimal;
use serde::Serialize;

use super::driver::UpSource;

/// Rounds kept: a day of five-minute rounds.
const ROWS_KEPT: usize = 288;
/// How often the audit goes back for results it is still missing.
pub const RECHECK_EVERY: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AuditRow {
    pub event_ticker: String,
    pub strike: Decimal,
    /// What this server resolved the round as.
    pub resolved_up: bool,
    /// Resolved before Gemini published, on the index price at expiry.
    pub provisional: bool,
    /// Gemini's settlement value, or for a provisional round the index
    /// price it was resolved on.
    pub close_price: f64,
    /// Gemini's own result; `None` until it has been read.
    pub gemini_up: Option<bool>,
    /// The socket's index price at the expiry second, if it arrived.
    pub index_at_expiry: Option<f64>,
}

impl AuditRow {
    /// Whether "index at expiry, at or above the strike" gives Gemini's
    /// answer. `None` when either is unknown.
    pub fn index_rule_agrees(&self) -> Option<bool> {
        let gemini_up = self.gemini_up?;
        let index = Decimal::try_from(self.index_at_expiry?).ok()?;
        Some((index >= self.strike) == gemini_up)
    }
}

/// What `GET /api/rounds/audit` answers.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AuditSummary {
    pub rounds: usize,
    pub provisional: usize,
    /// Rounds whose result differs from Gemini's. Should be zero.
    pub mismatches: usize,
    /// Provisional rounds Gemini has not yet published a result for.
    pub awaiting_gemini: usize,
    /// Rounds where the index rule would not have given Gemini's answer.
    pub index_rule_disagreements: usize,
    /// The newest rounds, newest first.
    pub recent: Vec<AuditRow>,
}

/// The audit log of one or more series. Cheap to clone.
#[derive(Debug, Clone, Default)]
pub struct RoundAudit {
    rows: Arc<Mutex<VecDeque<AuditRow>>>,
}

impl RoundAudit {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&self, row: AuditRow) {
        let mut rows = self.rows.lock().expect("audit poisoned");
        if rows.len() == ROWS_KEPT {
            rows.pop_front();
        }
        if row.gemini_up.is_some_and(|gemini_up| gemini_up != row.resolved_up) {
            tracing::error!(ticker = %row.event_ticker, "a round was resolved against gemini's own result");
        }
        rows.push_back(row);
    }

    /// Oldest first.
    pub fn rows(&self) -> Vec<AuditRow> {
        self.rows.lock().expect("audit poisoned").iter().cloned().collect()
    }

    /// Tickers of rounds still missing Gemini's result.
    pub fn awaiting(&self) -> Vec<String> {
        let rows = self.rows.lock().expect("audit poisoned");
        rows.iter().filter(|r| r.gemini_up.is_none()).map(|r| r.event_ticker.clone()).collect()
    }

    /// Gemini's result for a round has been read.
    pub fn set_gemini_result(&self, event_ticker: &str, gemini_up: bool) {
        let mut rows = self.rows.lock().expect("audit poisoned");
        for row in rows.iter_mut().filter(|r| r.event_ticker == event_ticker && r.gemini_up.is_none()) {
            row.gemini_up = Some(gemini_up);
            if gemini_up != row.resolved_up {
                tracing::error!(
                    ticker = event_ticker,
                    resolved_up = row.resolved_up,
                    gemini_up,
                    "a provisional round differs from gemini's result",
                );
            }
        }
    }

    pub fn summary(&self, recent: usize) -> AuditSummary {
        let rows = self.rows.lock().expect("audit poisoned");
        AuditSummary {
            rounds: rows.len(),
            provisional: rows.iter().filter(|r| r.provisional).count(),
            mismatches: rows.iter().filter(|r| r.gemini_up.is_some_and(|g| g != r.resolved_up)).count(),
            awaiting_gemini: rows.iter().filter(|r| r.gemini_up.is_none()).count(),
            index_rule_disagreements: rows.iter().filter(|r| r.index_rule_agrees() == Some(false)).count(),
            recent: rows.iter().rev().take(recent).cloned().collect(),
        }
    }

    /// One pass: read Gemini's result for every round still missing it.
    pub async fn recheck(&self, source: &impl UpSource) {
        for ticker in self.awaiting() {
            match source.event(&ticker).await {
                Ok(detail) => {
                    if let Some(result) = detail.settled {
                        self.set_gemini_result(&ticker, result.up_won);
                    }
                }
                Err(error) => tracing::debug!(%ticker, %error, "audit could not read gemini's result"),
            }
        }
    }
}

/// Go back for missing results every [`RECHECK_EVERY`], forever.
pub fn spawn_audit(audit: RoundAudit, source: impl UpSource) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(RECHECK_EVERY).await;
            audit.recheck(&source).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gemini::types::{EventDetail, Settled, UpWindow};
    use std::str::FromStr;

    fn row(ticker: &str, resolved_up: bool, provisional: bool, gemini_up: Option<bool>, index: Option<f64>) -> AuditRow {
        AuditRow {
            event_ticker: ticker.into(),
            strike: Decimal::from_str("100.00").unwrap(),
            resolved_up,
            provisional,
            close_price: index.unwrap_or(0.0),
            gemini_up,
            index_at_expiry: index,
        }
    }

    #[test]
    fn rounds_resolved_on_geminis_result_never_mismatch() {
        let audit = RoundAudit::new();
        audit.record(row("A", true, false, Some(true), Some(101.0)));
        audit.record(row("B", false, false, Some(false), Some(99.0)));
        let s = audit.summary(10);
        assert_eq!((s.rounds, s.provisional, s.mismatches, s.awaiting_gemini), (2, 0, 0, 0));
        assert_eq!(s.index_rule_disagreements, 0);
        assert_eq!(s.recent.iter().map(|r| r.event_ticker.as_str()).collect::<Vec<_>>(), vec!["B", "A"]);
    }

    #[test]
    fn a_provisional_round_waits_for_gemini_and_is_then_counted_either_way() {
        let audit = RoundAudit::new();
        audit.record(row("A", true, true, None, Some(101.0)));
        audit.record(row("B", true, true, None, Some(100.0)));
        assert_eq!(audit.awaiting(), vec!["A".to_owned(), "B".to_owned()]);
        assert_eq!(audit.summary(10).awaiting_gemini, 2);
        audit.set_gemini_result("A", true);
        audit.set_gemini_result("B", false);
        let s = audit.summary(10);
        assert_eq!((s.provisional, s.mismatches, s.awaiting_gemini), (2, 1, 0));
        assert!(audit.awaiting().is_empty());
    }

    #[test]
    fn the_index_rule_is_scored_against_gemini_where_both_are_known() {
        // At the strike exactly is Up.
        assert_eq!(row("A", true, false, Some(true), Some(100.0)).index_rule_agrees(), Some(true));
        assert_eq!(row("A", false, false, Some(false), Some(99.99)).index_rule_agrees(), Some(true));
        assert_eq!(row("A", false, false, Some(false), Some(100.5)).index_rule_agrees(), Some(false));
        assert_eq!(row("A", true, false, Some(true), None).index_rule_agrees(), None);
        assert_eq!(row("A", true, true, None, Some(100.5)).index_rule_agrees(), None);
        let audit = RoundAudit::new();
        audit.record(row("A", false, false, Some(false), Some(100.5)));
        assert_eq!(audit.summary(1).index_rule_disagreements, 1);
    }

    #[test]
    fn only_a_days_rounds_are_kept() {
        let audit = RoundAudit::new();
        for i in 0..ROWS_KEPT + 5 {
            audit.record(row(&format!("T{i}"), true, false, Some(true), None));
        }
        let rows = audit.rows();
        assert_eq!(rows.len(), ROWS_KEPT);
        assert_eq!(rows[0].event_ticker, "T5");
    }

    struct One(Option<bool>);

    impl UpSource for One {
        async fn event(&self, event_ticker: &str) -> anyhow::Result<EventDetail> {
            Ok(EventDetail {
                window: UpWindow {
                    event_ticker: event_ticker.into(),
                    symbol: String::new(),
                    starts_at_ms: 0,
                    expires_at_ms: 0,
                    strike: None,
                    market_state: String::new(),
                },
                settled: self.0.map(|up_won| Settled {
                    up_won,
                    settlement_value: Decimal::ONE,
                    resolved_at_ms: 0,
                }),
            })
        }
    }

    #[tokio::test]
    async fn a_recheck_fills_in_results_gemini_has_since_published() {
        let audit = RoundAudit::new();
        audit.record(row("A", true, true, None, Some(101.0)));
        audit.recheck(&One(None)).await;
        assert_eq!(audit.summary(1).awaiting_gemini, 1, "Gemini still has no result");
        audit.recheck(&One(Some(false))).await;
        let s = audit.summary(1);
        assert_eq!((s.awaiting_gemini, s.mismatches), (0, 1));
    }
}
