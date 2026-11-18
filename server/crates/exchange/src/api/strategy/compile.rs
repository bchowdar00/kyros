//! Compiling a submitted strategy document against the venue.
//!
//! Every strategy reaches the exchange as an IR document
//! (`strategy_compiler::Strategy`) plus an envelope. [`compile_for_venue`]
//! runs the compiler against the registry generated at start-up and
//! derives the one series the program trades; the deploy route and the
//! stateless `POST /api/strategy` compile-check both go through it, so a
//! document that passes the check is exactly a document that deploys.

use std::collections::BTreeMap;

use axum::{Json, extract::State};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::Value as Json_;
use strategy_compiler::ir::{Envelope, Registry, Strategy};
use strategy_compiler::program::{ParamType, Program};
use strategy_compiler::{Compiled, Diagnostic};

use crate::api::{ApiError, AppState};
use crate::automation::value::{Value, parse_decimal};
use crate::types::SeriesId;

/// Compile `strategy` against `registry` and derive its home series.
pub fn compile_for_venue(
    registry: &Registry,
    strategy: &Strategy,
    envelope: &Envelope,
) -> Result<(Compiled, SeriesId), ApiError> {
    let compiled = strategy_compiler::compile(strategy, registry, envelope).map_err(ApiError::CompileFailed)?;
    let series = home_series(&compiled.artifacts.program)?;
    Ok((compiled, series))
}

/// The one series every instrument of `program` trades in. A sub-account
/// is pinned to one series, so a program naming two is refused.
pub fn home_series(program: &Program) -> Result<SeriesId, ApiError> {
    let mut home: Option<SeriesId> = None;
    for (nickname, instrument) in &program.instruments {
        let slug = instrument
            .strip_suffix(".current")
            .ok_or_else(|| ApiError::InvalidStrategy(format!("instrument `{nickname}` = `{instrument}` is not a `<series>.current` market")))?;
        let series: SeriesId = slug.parse().map_err(|_| ApiError::UnknownSeries(slug.to_string()))?;
        match home {
            None => home = Some(series),
            Some(h) if h == series => {}
            Some(h) => {
                return Err(ApiError::InvalidStrategy(format!(
                    "a strategy trades one series; instruments name both `{}` and `{}`",
                    h.slug(),
                    series.slug()
                )));
            }
        }
    }
    home.ok_or_else(|| ApiError::InvalidStrategy("strategy declares no instruments".into()))
}

/// Type submitted params against the program's declared params. Unknown
/// names and values of the wrong shape are refused; declared params that
/// are absent keep their defaults (the runtime fills them in).
pub fn params_to_values(program: &Program, params: &BTreeMap<String, Json_>) -> Result<BTreeMap<String, Value>, ApiError> {
    let mut out = BTreeMap::new();
    for (name, raw) in params {
        let def = program
            .params
            .iter()
            .find(|p| &p.name == name)
            .ok_or_else(|| ApiError::InvalidStrategy(format!("unknown param `{name}`")))?;
        let bad = || ApiError::InvalidStrategy(format!("param `{name}` does not fit its declared type {:?}: {raw}", def.param_type));
        let value = match &def.param_type {
            ParamType::Decimal | ParamType::U8 => {
                let d = match raw {
                    Json_::Number(n) => parse_decimal(&n.to_string()),
                    Json_::String(s) => parse_decimal(s),
                    _ => None,
                }
                .ok_or_else(bad)?;
                if def.param_type == ParamType::U8 && (d.is_sign_negative() || !d.fract().is_zero() || d > Decimal::from(u8::MAX)) {
                    return Err(bad());
                }
                Value::Decimal(d)
            }
            ParamType::Bool => Value::Bool(raw.as_bool().ok_or_else(bad)?),
            ParamType::String => Value::Text(raw.as_str().ok_or_else(bad)?.to_string()),
            ParamType::Enum { variants } => {
                let s = raw.as_str().ok_or_else(bad)?;
                if !variants.iter().any(|v| v == s) {
                    return Err(bad());
                }
                Value::Text(s.to_string())
            }
        };
        out.insert(name.clone(), value);
    }
    Ok(out)
}

/// `POST /api/strategy` — compile a document without deploying it.
#[derive(Debug, Deserialize)]
pub struct CompileRequest {
    pub strategy: Strategy,
    pub envelope: Envelope,
}

/// What a successful compile tells the author.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CompileResponse {
    pub strategy_id: String,
    pub name: String,
    /// The compiler's canonical restatement of the document, verbatim from `meta`.
    pub description: String,
    /// The series the program trades.
    pub series: SeriesId,
    pub subscriptions: Vec<String>,
    /// Instrument nickname → venue instrument id.
    pub instruments: BTreeMap<String, String>,
    /// Declared params and their types, so a deploy form can render them.
    pub params: Vec<ParamSummary>,
    pub legs: Vec<String>,
    pub warnings: Vec<Diagnostic>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ParamSummary {
    pub name: String,
    #[serde(rename = "type")]
    pub param_type: ParamType,
}

pub fn summarize(compiled: &Compiled, series: SeriesId) -> CompileResponse {
    let program = &compiled.artifacts.program;
    CompileResponse {
        strategy_id: program.strategy_id.clone(),
        name: program.name.clone(),
        description: program.description.clone(),
        series,
        subscriptions: program.subscriptions.clone(),
        instruments: program.instruments.clone(),
        params: program.params.iter().map(|p| ParamSummary { name: p.name.clone(), param_type: p.param_type.clone() }).collect(),
        legs: program.legs.iter().map(|l| l.name.clone()).collect(),
        warnings: compiled.warnings.clone(),
    }
}

/// `POST /api/strategy` and `POST /api/strategy/compile` — the stateless compile-check.
pub async fn compile_strategy(
    State(state): State<AppState>,
    Json(req): Json<CompileRequest>,
) -> Result<Json<CompileResponse>, ApiError> {
    let (compiled, series) = compile_for_venue(&state.registry, &req.strategy, &req.envelope)?;
    Ok(Json(summarize(&compiled, series)))
}

/// `GET /api/strategy/schema` — the JSON Schema of the IR document.
pub async fn get_strategy_schema() -> Json<serde_json::Value> {
    Json(serde_json::to_value(schemars::schema_for!(Strategy)).expect("schema serializes"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::strategies::test_support::{idle_document, test_envelope};
    use crate::types::DEFAULT_SERIES;

    #[test]
    fn idle_document_compiles_and_names_its_series() {
        let registry = crate::registry::for_tests();
        let (compiled, series) = compile_for_venue(&registry, &idle_document(SeriesId(1)), &test_envelope()).unwrap();
        assert_eq!(series, SeriesId(1));
        let summary = summarize(&compiled, series);
        assert_eq!(summary.name, "Idle");
        assert_eq!(summary.instruments["market"], format!("{}.current", SeriesId(1).slug()));
        assert_eq!(summary.subscriptions, vec![format!("{}.current.window.v1", SeriesId(1).slug())]);
    }

    #[test]
    fn unknown_series_and_two_series_are_refused() {
        let registry = crate::registry::for_tests();
        let mut doc = idle_document(DEFAULT_SERIES);
        doc.instruments = serde_json::from_value(serde_json::json!({"market": "nope.current"})).unwrap();
        let err = compile_for_venue(&registry, &doc, &test_envelope()).unwrap_err();
        // The compiler refuses an instrument the registry does not list.
        assert!(matches!(err, ApiError::CompileFailed(_)), "{err:?}");

        let mut two = idle_document(DEFAULT_SERIES);
        two.instruments = serde_json::from_value(serde_json::json!({
            "a": format!("{}.current", SeriesId(0).slug()),
            "b": format!("{}.current", SeriesId(1).slug())
        }))
        .unwrap();
        let err = compile_for_venue(&registry, &two, &test_envelope()).unwrap_err();
        assert!(matches!(err, ApiError::InvalidStrategy(_) | ApiError::CompileFailed(_)), "{err:?}");
    }

    #[test]
    fn compile_failure_lists_diagnostics() {
        let registry = crate::registry::for_tests();
        let mut doc = idle_document(DEFAULT_SERIES);
        doc.legs[0].arm = serde_json::from_value(serde_json::json!(["missing"])).unwrap();
        match compile_for_venue(&registry, &doc, &test_envelope()) {
            Err(ApiError::CompileFailed(diags)) => assert!(!diags.is_empty()),
            other => panic!("expected compile failure, got {other:?}"),
        }
    }

    #[test]
    fn params_are_typed_against_the_program() {
        let registry = crate::registry::for_tests();
        let mut doc = idle_document(DEFAULT_SERIES);
        doc.params = serde_json::from_value(serde_json::json!([
            {"name": "edge", "type": "decimal", "default": "0.04"},
            {"name": "lots", "type": "u8", "default": "2"},
            {"name": "on", "type": "bool", "default": true}
        ]))
        .unwrap();
        let (compiled, _) = compile_for_venue(&registry, &doc, &test_envelope()).unwrap();
        let program = &compiled.artifacts.program;
        let mut raw = BTreeMap::new();
        raw.insert("edge".to_string(), serde_json::json!("0.05"));
        raw.insert("lots".to_string(), serde_json::json!(3));
        raw.insert("on".to_string(), serde_json::json!(false));
        let typed = params_to_values(program, &raw).unwrap();
        assert_eq!(typed["edge"], Value::Decimal("0.05".parse().unwrap()));
        assert_eq!(typed["lots"], Value::Decimal(Decimal::from(3)));
        assert_eq!(typed["on"], Value::Bool(false));

        raw.insert("lots".to_string(), serde_json::json!(300));
        assert!(matches!(params_to_values(program, &raw).unwrap_err(), ApiError::InvalidStrategy(_)));
        let mut unknown = BTreeMap::new();
        unknown.insert("zzz".to_string(), serde_json::json!(1));
        assert!(matches!(params_to_values(program, &unknown).unwrap_err(), ApiError::InvalidStrategy(_)));
    }
}
