//! Strategy storage: the record a sub-account carries for a deployed
//! strategy.
//!
//! A strategy is an LLM-authored IR document (`strategy_compiler::Strategy`)
//! compiled against the venue registry and an envelope into a
//! `strategy_compiler::Program`. The record stored on the sub-account keeps
//! all three plus the lifecycle fields the API and the runtime need. The
//! runtime (`crate::automation::runtime`) executes the program; the engine
//! only stores the record and uses its presence to tell a live strategy
//! sub-account from an orphan awaiting the settlement sweep.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use strategy_compiler::ir::{Envelope, Strategy};
use strategy_compiler::program::Program;

use crate::types::SeriesId;

/// Lifecycle of a stored strategy. `Active` runs; `Paused` keeps the
/// record and the deployment resident but delivers no activations;
/// `Archived` is a soft-deleted state the API does not use today.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StrategyStatus {
    Active,
    Paused,
    Archived,
}

/// A deployed strategy, stored on `Account.strategy`. There is no
/// per-strategy id on the wire beyond the sub-account address; the
/// runtime addresses the deployment by `deployment_id`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeployedStrategy {
    pub name: String,
    pub status: StrategyStatus,
    pub created_at_ms: u64,
    /// Series the strategy trades — always the sub-account's home series.
    pub series: SeriesId,
    /// `Program.strategy_id`: a content hash of the source document.
    pub strategy_id: String,
    /// Distinguishes deployments of the same document.
    pub deploy_epoch: u64,
    /// The runtime's key: `<strategy_id>:<deploy_epoch>`.
    pub deployment_id: String,
    /// The authored document, verbatim.
    pub source: Strategy,
    /// The envelope the program was compiled and is risk-gated against.
    pub envelope: Envelope,
    /// Deploy-time params as submitted (JSON); the runtime types them
    /// against the program's declared params at start.
    #[serde(default)]
    pub params: BTreeMap<String, Json>,
    /// The compiled program the runtime executes. Shared with the
    /// template catalogue for a template deploy, so a hundred deployments
    /// of one card hold one program.
    pub program: Arc<Program>,
    /// The public template this was deployed from, if any; the count key
    /// on the strategy cards. `None` for an authored document.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template_id: Option<String>,
    /// Which of the template's two documents: `true` for Inverse.
    #[serde(default)]
    pub inverse: bool,
}

impl DeployedStrategy {
    /// Assemble a record from a compiled program. `name` defaults to the
    /// document's `meta.name`.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        name: Option<String>,
        source: Strategy,
        envelope: Envelope,
        params: BTreeMap<String, Json>,
        program: Arc<Program>,
        series: SeriesId,
        deploy_epoch: u64,
        created_at_ms: u64,
    ) -> Self {
        let name = name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map_or_else(|| source.meta.name.clone(), str::to_string);
        let deployment_id = format!("{}:{deploy_epoch}", program.strategy_id);
        DeployedStrategy {
            name,
            status: StrategyStatus::Active,
            created_at_ms,
            series,
            strategy_id: program.strategy_id.clone(),
            deploy_epoch,
            deployment_id,
            source,
            envelope,
            params,
            program,
            template_id: None,
            inverse: false,
        }
    }

    /// Mark the record as a deployment of public template `template_id`,
    /// on its Inverse document when `inverse`.
    pub fn from_template(mut self, template_id: &str, inverse: bool) -> Self {
        self.template_id = Some(template_id.to_string());
        self.inverse = inverse;
        self
    }
}

/// Partial update for `PATCH`. Only `Some` fields are applied. An empty
/// patch (all `None`) is a no-op.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct StrategyPatch {
    pub name: Option<String>,
    pub status: Option<StrategyStatus>,
}

/// Errors returned by `Exchange::deploy_strategy_in` and friends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StrategyError {
    AccountNotFound,
    StrategyNotFound,
    InvalidStrategy(String),
}

impl fmt::Display for StrategyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StrategyError::AccountNotFound => write!(f, "account not found"),
            StrategyError::StrategyNotFound => write!(f, "strategy not found"),
            StrategyError::InvalidStrategy(msg) => write!(f, "invalid strategy: {msg}"),
        }
    }
}

impl std::error::Error for StrategyError {}

/// Test support: a compiled record for tests that only need a sub-account
/// to count as occupied (the settlement sweep, account rows, trade
/// history). The program is a one-trigger, one-leg document against the
/// generated test registry that never places an order. Public (not
/// `cfg(test)`) so the crate's integration tests can use it too.
pub mod test_support {
    use super::*;

    /// A valid document trading `series` that does nothing. The compiler
    /// requires at least one signal, so it subscribes to the series'
    /// round clock without ever reading it.
    pub fn idle_document(series: SeriesId) -> Strategy {
        let instrument = format!("{}.current", series.slug());
        let clock = format!("{}.current.window.v1", series.slug());
        serde_json::from_value(serde_json::json!({
            "meta": {
                "name": "Idle",
                "description": "Does nothing; test placeholder.",
                "dsl_version": "0.3"
            },
            "instruments": { "market": instrument },
            "signals": { "clock": clock },
            "state": [],
            "triggers": [
                { "name": "tick",
                  "timer": { "kind": "interval", "every": "1s" },
                  "fire": "if_true", "rearm": { "mode": "immediate" } }
            ],
            "legs": [
                { "name": "idle", "resident": true, "arm": ["tick"], "body": [] }
            ]
        }))
        .expect("idle document parses")
    }

    /// A $100 envelope with a $50 loss budget and ten-lot orders.
    pub fn test_envelope() -> Envelope {
        serde_json::from_str(
            r#"{"allocation": "100", "loss_budget": "50", "max_order_size": "10"}"#,
        )
        .expect("envelope parses")
    }

    /// Compile [`idle_document`] and wrap it in a record named `name`.
    pub fn idle_record(name: &str, series: SeriesId, deploy_epoch: u64) -> DeployedStrategy {
        let source = idle_document(series);
        let envelope = test_envelope();
        let registry = crate::registry::for_tests();
        let compiled = strategy_compiler::compile(&source, &registry, &envelope)
            .unwrap_or_else(|diags| panic!("idle document compiles: {diags:#?}"));
        DeployedStrategy::new(
            Some(name.to_string()),
            source,
            envelope,
            BTreeMap::new(),
            Arc::new(compiled.artifacts.program),
            series,
            deploy_epoch,
            0,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use crate::types::DEFAULT_SERIES;

    #[test]
    fn record_defaults_name_to_the_document_and_keys_the_deployment() {
        let r = idle_record("Named", DEFAULT_SERIES, 7);
        assert_eq!(r.name, "Named");
        assert_eq!(r.status, StrategyStatus::Active);
        assert_eq!(r.series, DEFAULT_SERIES);
        assert_eq!(r.deployment_id, format!("{}:7", r.strategy_id));
        assert_eq!(r.program.strategy_id, r.strategy_id);

        let source = idle_document(DEFAULT_SERIES);
        let compiled = strategy_compiler::compile(&source, &crate::registry::for_tests(), &test_envelope()).unwrap();
        let unnamed = DeployedStrategy::new(
            Some("  ".into()),
            source,
            test_envelope(),
            BTreeMap::new(),
            Arc::new(compiled.artifacts.program),
            DEFAULT_SERIES,
            1,
            0,
        );
        assert_eq!(unnamed.name, "Idle");
        assert_eq!((unnamed.template_id.as_deref(), unnamed.inverse), (None, false));
    }

    #[test]
    fn record_round_trips_through_json() {
        let r = idle_record("RT", DEFAULT_SERIES, 1);
        let text = serde_json::to_string(&r).unwrap();
        assert!(!text.contains("template_id"), "an authored record carries no template key");
        let back: DeployedStrategy = serde_json::from_str(&text).unwrap();
        assert_eq!(back, r);

        // A template deployment keeps its key; a record written before the
        // fields existed still reads (serde defaults).
        let t = idle_record("T", DEFAULT_SERIES, 2).from_template("late-closer", true);
        let back: DeployedStrategy = serde_json::from_str(&serde_json::to_string(&t).unwrap()).unwrap();
        assert_eq!((back.template_id.as_deref(), back.inverse), (Some("late-closer"), true));
        let mut old: serde_json::Value = serde_json::from_str(&text).unwrap();
        old.as_object_mut().unwrap().remove("inverse");
        let back: DeployedStrategy = serde_json::from_value(old).unwrap();
        assert_eq!(back, r);
    }
}
