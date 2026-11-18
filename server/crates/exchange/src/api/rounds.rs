//! `GET /api/rounds/audit` — how every round was resolved, checked
//! against Gemini's own results. `mismatches` should always be zero.

use axum::{Json, extract::State};

use crate::gemini::GeminiState;
use crate::gemini::audit::AuditSummary;

use super::AppState;

/// The audit of a server that does not follow Gemini is empty.
pub fn rounds_audit(gemini: Option<&GeminiState>) -> AuditSummary {
    match gemini {
        Some(gemini) => gemini.audit_summary(),
        None => AuditSummary { rounds: 0, provisional: 0, mismatches: 0, awaiting_gemini: 0, index_rule_disagreements: 0, recent: Vec::new() },
    }
}

pub async fn get_rounds_audit(State(state): State<AppState>) -> Json<AuditSummary> {
    Json(rounds_audit(state.gemini.as_ref()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gemini::audit::{AuditRow, RoundAudit};
    use crate::gemini::env::PRODUCTION;
    use crate::gemini::strikes::GeminiClock;
    use rust_decimal::Decimal;

    #[test]
    fn the_audit_is_empty_without_gemini_and_live_with_it() {
        let empty = rounds_audit(None);
        assert_eq!((empty.rounds, empty.mismatches, empty.recent.len()), (0, 0, 0));

        let gemini = GeminiState::new(PRODUCTION, GeminiClock::starting_at(0), RoundAudit::new());
        gemini.audit.record(AuditRow {
            event_ticker: "BTC05M2611181730".into(),
            strike: Decimal::ONE,
            resolved_up: true,
            provisional: true,
            close_price: 1.0,
            gemini_up: Some(false),
            index_at_expiry: Some(1.0),
        });
        let live = rounds_audit(Some(&gemini));
        assert_eq!((live.rounds, live.provisional, live.mismatches), (1, 1, 1));
        let json = serde_json::to_value(&live).unwrap();
        assert_eq!(json["recent"][0]["event_ticker"], "BTC05M2611181730");
        assert_eq!(json["recent"][0]["gemini_up"], false);
    }
}
