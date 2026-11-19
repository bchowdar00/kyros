//! The public strategy templates: the catalogue behind the strategy cards.
//!
//! One JSON file per template under `crates/exchange/templates/` (the
//! README there pins the file format). Each file is the card metadata,
//! the envelope ratios, per-series substitution values, and two IR v0.3
//! documents: `primary` and `inverse`. The documents are written with
//! placeholders (`{{series}}`, `{{spot}}`, and the file's own names), so
//! one template stamps into one concrete document per listed series and
//! side.
//!
//! The files are embedded at compile time and loaded once at boot by
//! [`TemplateCatalogue::load`]: every stamped document is compiled against
//! the generated registry and the process refuses to start on any
//! diagnostic *or warning*, so a template that does not compile clean
//! cannot ship. The catalogue keeps the compiled programs so a template
//! deploy never compiles on the request path.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use strategy_compiler::ir::{DecimalStr, Envelope, Registry, Strategy};
use strategy_compiler::program::Program;

use crate::registry::{PriceSource, price_feed_signal_id};
use crate::types::SeriesId;

/// The embedded template files, in catalogue order.
const TEMPLATE_FILES: &[(&str, &str)] = &[
    ("fair-value-discount", include_str!("../templates/fair-value-discount.json")),
    ("momentum-rider", include_str!("../templates/momentum-rider.json")),
    ("late-closer", include_str!("../templates/late-closer.json")),
    ("oracle-gap", include_str!("../templates/oracle-gap.json")),
    ("order-flow-lead", include_str!("../templates/order-flow-lead.json")),
    ("vwap-reversion", include_str!("../templates/vwap-reversion.json")),
    ("sweep-follower", include_str!("../templates/sweep-follower.json")),
    ("absorption-fade", include_str!("../templates/absorption-fade.json")),
    ("polymarket-discount", include_str!("../templates/polymarket-discount.json")),
    ("polymarket-momentum", include_str!("../templates/polymarket-momentum.json")),
    ("trend-rider", include_str!("../templates/trend-rider.json")),
    ("inside-quote", include_str!("../templates/inside-quote.json")),
    ("late-fade", include_str!("../templates/late-fade.json")),
    ("model-vs-crowd", include_str!("../templates/model-vs-crowd.json")),
    ("lead-change", include_str!("../templates/lead-change.json")),
    ("venue-gap", include_str!("../templates/venue-gap.json")),
    ("fourth-quarter", include_str!("../templates/fourth-quarter.json")),
    ("pregame-pick", include_str!("../templates/pregame-pick.json")),
    ("shooting-streak", include_str!("../templates/shooting-streak.json")),
];

// ───────────────────────────── File shape ─────────────────────────────

/// Who authored the template, as the card shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Creator {
    pub handle: String,
    pub username: String,
    /// Portrait filename under the frontend's avatars folder; `None`
    /// while no portrait is assigned.
    pub avatar: Option<String>,
}

/// Ratios and lot sizes the deploy route turns into a concrete
/// [`Envelope`] for a user's allocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvelopeRatios {
    /// Loss budget as a fraction of the allocation.
    pub loss_budget_fraction: DecimalStr,
    /// Dollars per order as a fraction of the allocation; stamped into the
    /// documents as `{{stake}}` so sizes read
    /// `envelope.allocation * {{stake}} / price`.
    pub order_fraction: DecimalStr,
    /// Lot ceiling per order, the cap on every dollar-sized order.
    pub max_order_size: DecimalStr,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_open_orders: Option<DecimalStr>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_position_per_market: Option<DecimalStr>,
}

/// One template file, parsed. The two documents stay as raw JSON text
/// because placeholders may sit inside expression strings; they are
/// stamped and parsed per series.
#[derive(Debug, Clone, Deserialize)]
struct TemplateFile {
    id: String,
    title: String,
    #[serde(default = "default_listed")]
    listed: bool,
    creator: Creator,
    series: Vec<String>,
    logic: String,
    inverse_logic: String,
    envelope: EnvelopeRatios,
    #[serde(default)]
    substitutions: BTreeMap<String, BTreeMap<String, String>>,
    primary: serde_json::Value,
    inverse: serde_json::Value,
}

/// A template file with no `listed` key is listed; the key only ever parks one.
fn default_listed() -> bool {
    true
}

// ───────────────────────────── Catalogue ─────────────────────────────

/// Which of a template's two documents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Primary,
    Inverse,
}

impl Side {
    pub const ALL: [Side; 2] = [Side::Primary, Side::Inverse];

    pub fn is_inverse(self) -> bool {
        self == Side::Inverse
    }
}

/// One stamped, compiled document.
#[derive(Debug, Clone)]
pub struct CompiledSide {
    /// The document after substitution, verbatim.
    pub source: Strategy,
    pub program: Arc<Program>,
}

/// One template with every document it stamps to.
#[derive(Debug, Clone)]
pub struct Template {
    pub id: String,
    pub title: String,
    /// A template with `listed: false` is compiled and can be fetched by id
    /// and deployed, but the list route leaves it out.
    pub listed: bool,
    pub creator: Creator,
    /// The series this template is listed on, file order.
    pub series: Vec<SeriesId>,
    pub logic: String,
    pub inverse_logic: String,
    pub envelope: EnvelopeRatios,
    compiled: BTreeMap<(SeriesId, Side), CompiledSide>,
}

impl Template {
    pub fn lists(&self, series: SeriesId) -> bool {
        self.series.contains(&series)
    }

    /// The compiled document for one series and side; `None` when the
    /// template is not listed on `series`.
    pub fn compiled(&self, series: SeriesId, side: Side) -> Option<&CompiledSide> {
        self.compiled.get(&(series, side))
    }

    pub fn prose(&self, side: Side) -> &str {
        match side {
            Side::Primary => &self.logic,
            Side::Inverse => &self.inverse_logic,
        }
    }
}

/// Every template, loaded and compiled, in catalogue order.
#[derive(Debug, Clone, Default)]
pub struct TemplateCatalogue {
    templates: Vec<Template>,
}

impl TemplateCatalogue {
    /// Parse, stamp and compile every embedded template against
    /// `registry`. `price_source` picks the `{{spot}}` signal id.
    pub fn load(registry: &Registry, price_source: PriceSource) -> Result<Self, LoadError> {
        Self::load_files(TEMPLATE_FILES, registry, price_source)
    }

    fn load_files(files: &[(&str, &str)], registry: &Registry, price_source: PriceSource) -> Result<Self, LoadError> {
        let spot = price_feed_signal_id(price_source);
        let mut templates = Vec::with_capacity(files.len());
        for (name, text) in files {
            let file: TemplateFile = serde_json::from_str(text).map_err(|e| LoadError::Parse { file: name.to_string(), error: e.to_string() })?;
            if file.id != *name {
                return Err(LoadError::IdMismatch { file: name.to_string(), id: file.id });
            }
            templates.push(compile_template(file, registry, &spot)?);
        }
        Ok(TemplateCatalogue { templates })
    }

    pub fn iter(&self) -> impl Iterator<Item = &Template> {
        self.templates.iter()
    }

    pub fn len(&self) -> usize {
        self.templates.len()
    }

    pub fn is_empty(&self) -> bool {
        self.templates.is_empty()
    }

    pub fn get(&self, id: &str) -> Option<&Template> {
        self.templates.iter().find(|t| t.id == id)
    }

    /// The templates listed on `series`, catalogue order.
    pub fn for_series(&self, series: SeriesId) -> impl Iterator<Item = &Template> {
        self.templates.iter().filter(move |t| t.lists(series))
    }

    /// The number of stamped documents across every template.
    pub fn document_count(&self) -> usize {
        self.templates.iter().map(|t| t.compiled.len()).sum()
    }
}

/// Stamp and compile one file for every series it lists, both sides.
fn compile_template(file: TemplateFile, registry: &Registry, spot: &str) -> Result<Template, LoadError> {
    let mut series = Vec::with_capacity(file.series.len());
    for slug in &file.series {
        let id: SeriesId = slug.parse().map_err(|_| LoadError::UnknownSeries { template: file.id.clone(), slug: slug.clone() })?;
        if series.contains(&id) {
            return Err(LoadError::DuplicateSeries { template: file.id.clone(), slug: slug.clone() });
        }
        series.push(id);
    }
    if series.is_empty() {
        return Err(LoadError::NoSeries { template: file.id.clone() });
    }
    // A reference envelope for the boot compile. The compiler only reads
    // the envelope for expression resolution and the risk gate's shape;
    // the concrete numbers come from the user's allocation at deploy.
    let envelope = reference_envelope(&file.envelope);

    let mut compiled = BTreeMap::new();
    for &s in &series {
        let empty = BTreeMap::new();
        let values = file.substitutions.get(s.slug()).unwrap_or(&empty);
        let builtins = [("series", s.slug()), ("spot", spot), ("stake", file.envelope.order_fraction.as_str())];
        for side in Side::ALL {
            let raw = match side {
                Side::Primary => &file.primary,
                Side::Inverse => &file.inverse,
            };
            let at = || Location { template: file.id.clone(), series: s.slug().to_string(), side };
            let text = stamp(&raw.to_string(), &builtins, values).map_err(|placeholder| LoadError::UnresolvedPlaceholder { at: at(), placeholder })?;
            let source: Strategy = serde_json::from_str(&text).map_err(|e| LoadError::Document { at: at(), error: e.to_string() })?;
            let result = strategy_compiler::compile(&source, registry, &envelope).map_err(|diags| LoadError::Compile { at: at(), diagnostics: render(&diags) })?;
            if !result.warnings.is_empty() {
                return Err(LoadError::Compile { at: at(), diagnostics: render(&result.warnings) });
            }
            compiled.insert((s, side), CompiledSide { source, program: Arc::new(result.artifacts.program) });
        }
    }
    Ok(Template {
        id: file.id,
        title: file.title,
        listed: file.listed,
        creator: file.creator,
        series,
        logic: file.logic,
        inverse_logic: file.inverse_logic,
        envelope: file.envelope,
        compiled,
    })
}

/// The envelope a template is compiled against at boot: a $1,000
/// allocation with the file's ratios applied.
fn reference_envelope(ratios: &EnvelopeRatios) -> Envelope {
    envelope_for(ratios, rust_decimal::Decimal::from(1_000))
}

/// The concrete envelope for `allocation` dollars under `ratios`.
pub fn envelope_for(ratios: &EnvelopeRatios, allocation: rust_decimal::Decimal) -> Envelope {
    let fraction: rust_decimal::Decimal = ratios.loss_budget_fraction.as_str().parse().unwrap_or(rust_decimal::Decimal::ONE);
    let loss_budget = (allocation * fraction).normalize();
    Envelope {
        allocation: decimal_str(allocation.normalize()),
        loss_budget: decimal_str(loss_budget),
        round_budget: None,
        max_order_size: ratios.max_order_size.clone(),
        max_exposure_fraction: None,
        pause_at_consumed: None,
        max_position_per_market: ratios.max_position_per_market.clone(),
        max_open_orders: ratios.max_open_orders.clone(),
    }
}

/// A normalized `rust_decimal` value is always a canonical decimal string,
/// which is the only way to construct a [`DecimalStr`].
pub(crate) fn decimal_str(d: rust_decimal::Decimal) -> DecimalStr {
    serde_json::from_value(serde_json::Value::String(d.normalize().to_string())).expect("normalized decimal is canonical")
}

/// Replace every `{{name}}` in `text`. `builtins` are the loader-supplied
/// names (`series`, `spot`, `stake`); everything else comes from `values`.
/// The first placeholder with no value is the error.
fn stamp(text: &str, builtins: &[(&str, &str)], values: &BTreeMap<String, String>) -> Result<String, String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            return Err(after.chars().take(32).collect());
        };
        let name = after[..end].trim();
        let value = match builtins.iter().find(|(k, _)| *k == name) {
            Some((_, v)) => v,
            None => values.get(name).map(String::as_str).ok_or_else(|| name.to_string())?,
        };
        out.push_str(value);
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

fn render(diags: &[strategy_compiler::Diagnostic]) -> Vec<String> {
    diags.iter().map(|d| d.to_string().trim_end().to_string()).collect()
}

// ───────────────────────────── Errors ─────────────────────────────

/// Where in the catalogue a document-level failure happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    pub template: String,
    pub series: String,
    pub side: Side,
}

impl fmt::Display for Location {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let side = match self.side {
            Side::Primary => "primary",
            Side::Inverse => "inverse",
        };
        write!(f, "{} on {} ({side})", self.template, self.series)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadError {
    Parse { file: String, error: String },
    IdMismatch { file: String, id: String },
    UnknownSeries { template: String, slug: String },
    DuplicateSeries { template: String, slug: String },
    NoSeries { template: String },
    UnresolvedPlaceholder { at: Location, placeholder: String },
    Document { at: Location, error: String },
    /// Errors, or warnings on an otherwise successful compile.
    Compile { at: Location, diagnostics: Vec<String> },
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoadError::Parse { file, error } => write!(f, "template `{file}.json` does not parse: {error}"),
            LoadError::IdMismatch { file, id } => write!(f, "template `{file}.json` declares id `{id}`"),
            LoadError::UnknownSeries { template, slug } => write!(f, "template `{template}` lists unknown series `{slug}`"),
            LoadError::DuplicateSeries { template, slug } => write!(f, "template `{template}` lists `{slug}` twice"),
            LoadError::NoSeries { template } => write!(f, "template `{template}` lists no series"),
            LoadError::UnresolvedPlaceholder { at, placeholder } => write!(f, "{at}: no value for placeholder `{{{{{placeholder}}}}}`"),
            LoadError::Document { at, error } => write!(f, "{at}: stamped document does not parse: {error}"),
            LoadError::Compile { at, diagnostics } => {
                write!(f, "{at}: compile is not clean:")?;
                for d in diagnostics {
                    write!(f, "\n  {d}")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for LoadError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{self, Inputs};
    use crate::series::SERIES;

    /// A registry with the checkout's signal definitions, so the external
    /// signals the templates read are present.
    fn full_registry() -> Registry {
        registry::build(&Inputs { price_source: PriceSource::GeminiIndex, live_lanes: &[], definitions_dir: None, copy_dir: None }).expect("registry builds")
    }

    #[test]
    fn every_embedded_template_compiles_clean_on_every_listed_series() {
        let registry = full_registry();
        let catalogue = TemplateCatalogue::load(&registry, PriceSource::GeminiIndex).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(catalogue.len(), TEMPLATE_FILES.len());
        let listings: usize = catalogue.iter().map(|t| t.series.len()).sum();
        assert_eq!(catalogue.document_count(), listings * 2, "two documents per listing");
        for t in catalogue.iter() {
            for &s in &t.series {
                for side in Side::ALL {
                    let c = t.compiled(s, side).expect("listed series compiles");
                    assert!(c.program.instruments.values().all(|i| *i == format!("{}.current", s.slug())), "{}: trades its own series", t.id);
                    assert!(!c.program.subscriptions.iter().any(|id| id.contains("{{")), "{}: placeholders stamped", t.id);
                }
            }
            let primary = t.compiled(t.series[0], Side::Primary).unwrap();
            let inverse = t.compiled(t.series[0], Side::Inverse).unwrap();
            assert_ne!(primary.program.strategy_id, inverse.program.strategy_id, "{}: inverse is its own document", t.id);
        }
        // The published mix: eight on both series, two 5-minute only, three 15-minute only.
        let five = catalogue.for_series(SERIES[0].id).count();
        let fifteen = catalogue.for_series(SERIES[1].id).count();
        assert_eq!((five, fifteen), (10, 11));
        // The NBA set: six NBA templates list the nba-h2h series
        // (team-loyalty stays parked: its team-name comparison does not
        // type against the string-typed matchup signal).
        assert_eq!(catalogue.for_series(SERIES[2].id).count(), 6, "the NBA set");
    }

    #[test]
    fn listed_defaults_true_and_parked_templates_load_unlisted() {
        let registry = full_registry();
        let catalogue = TemplateCatalogue::load(&registry, PriceSource::GeminiIndex).unwrap_or_else(|e| panic!("{e}"));
        // A file with no `listed` key loads listed.
        assert!(catalogue.get("inside-quote").unwrap().listed);
        // The six parked files declare `listed: false`.
        for id in ["fair-value-discount", "late-closer", "sweep-follower", "polymarket-discount", "polymarket-momentum", "trend-rider"] {
            assert!(!catalogue.get(id).unwrap().listed, "{id} is parked");
        }
    }

    #[test]
    fn spot_placeholder_follows_the_price_source() {
        let registry = registry::build(&Inputs { price_source: PriceSource::GeminiIndex, live_lanes: &[], definitions_dir: None, copy_dir: None }).unwrap();
        let catalogue = TemplateCatalogue::load(&registry, PriceSource::GeminiIndex).unwrap_or_else(|e| panic!("{e}"));
        let fair = catalogue.get("fair-value-discount").unwrap();
        let c = fair.compiled(SERIES[0].id, Side::Primary).unwrap();
        assert!(c.program.subscriptions.iter().any(|s| s == "btc-5m.current.delta.v1"), "{:?}", c.program.subscriptions);
        assert_eq!(fair.creator.avatar.as_deref(), Some("strategy-01.jpg"));
        // The spot built-in stamps the source's feed id even though no
        // shipped template reads it today.
        let values = BTreeMap::new();
        let id = price_feed_signal_id(PriceSource::GeminiIndex);
        let builtins = [("series", "btc-5m"), ("spot", id.as_str()), ("stake", "0.3")];
        assert_eq!(stamp("{{spot}}", &builtins, &values).unwrap(), "gemini.btc_usd.spot.v1");
    }

    #[test]
    fn every_template_compiles_clean_on_the_gemini_index() {
        let registry = registry::build(&Inputs { price_source: PriceSource::GeminiIndex, live_lanes: &[], definitions_dir: None, copy_dir: None }).unwrap();
        let catalogue = TemplateCatalogue::load(&registry, PriceSource::GeminiIndex).unwrap_or_else(|e| panic!("{e}"));
        assert!(catalogue.for_series(SERIES[0].id).count() > 0);
        let values = BTreeMap::new();
        let id = price_feed_signal_id(PriceSource::GeminiIndex);
        let builtins = [("series", "btc-5m"), ("spot", id.as_str()), ("stake", "0.3")];
        assert_eq!(stamp("{{spot}}", &builtins, &values).unwrap(), "gemini.btc_usd.spot.v1");
    }

    #[test]
    fn envelope_for_scales_the_loss_budget() {
        let ratios: EnvelopeRatios =
            serde_json::from_value(serde_json::json!({ "loss_budget_fraction": "0.5", "order_fraction": "0.3", "max_order_size": "50", "max_open_orders": "2" })).unwrap();
        let env = envelope_for(&ratios, rust_decimal::Decimal::new(2500, 2));
        assert_eq!(env.allocation.as_str(), "25");
        assert_eq!(env.loss_budget.as_str(), "12.5");
        assert_eq!(env.max_order_size.as_str(), "50");
        assert_eq!(env.max_open_orders.as_ref().map(|d| d.as_str()), Some("2"));
    }

    #[test]
    fn stamp_fills_builtins_and_substitutions_and_reports_a_missing_name() {
        let values: BTreeMap<String, String> = [("pull".to_string(), "10s".to_string())].into_iter().collect();
        let builtins = [("series", "btc-5m"), ("spot", "x.v1"), ("stake", "0.3")];
        assert_eq!(stamp("{{series}}.current {{ spot }} {{pull}} * {{stake}}", &builtins, &values).unwrap(), "btc-5m.current x.v1 10s * 0.3");
        assert_eq!(stamp("{{series}} {{nope}}", &builtins, &values).unwrap_err(), "nope");
    }

    #[test]
    fn a_broken_template_is_refused_with_its_location() {
        let registry = full_registry();
        let mut doc: serde_json::Value = serde_json::from_str(TEMPLATE_FILES[0].1).unwrap();
        doc["primary"]["legs"][0]["arm"] = serde_json::json!(["missing"]);
        let text = doc.to_string();
        let err = TemplateCatalogue::load_files(&[("fair-value-discount", &text)], &registry, PriceSource::GeminiIndex).unwrap_err();
        match err {
            LoadError::Compile { at, diagnostics } => {
                assert_eq!(at.template, "fair-value-discount");
                assert_eq!(at.side, Side::Primary);
                assert!(!diagnostics.is_empty());
            }
            other => panic!("expected a compile error, got {other}"),
        }

        let mut doc: serde_json::Value = serde_json::from_str(TEMPLATE_FILES[0].1).unwrap();
        doc["substitutions"]["btc-5m"] = serde_json::json!({});
        let text = doc.to_string();
        let err = TemplateCatalogue::load_files(&[("fair-value-discount", &text)], &registry, PriceSource::GeminiIndex).unwrap_err();
        assert!(matches!(&err, LoadError::UnresolvedPlaceholder { at, placeholder } if at.series == "btc-5m" && !placeholder.is_empty()), "{err}");

        let err = TemplateCatalogue::load_files(&[("other-id", TEMPLATE_FILES[0].1)], &registry, PriceSource::GeminiIndex).unwrap_err();
        assert!(matches!(err, LoadError::IdMismatch { .. }), "{err}");
    }
}
