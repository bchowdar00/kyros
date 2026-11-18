//! The venue registry, generated at start-up.
//!
//! The registry is the contract strategies are compiled against: every
//! market the exchange runs and every signal a strategy may subscribe to,
//! with typed fields. Its shape is the strategy compiler's
//! [`strategy_compiler::Registry`] (`markets[]`, `series[]`, `signals[]`),
//! the same document the automation prototype's harness reads.
//!
//! Nothing in it is hand-maintained. It is **derived** from the two places
//! a market or signal already has to exist to work at all:
//!
//! * **Series** come from the series table ([`crate::series::SERIES`]).
//!   Adding a row there already spawns the engine, scheduler, market maker
//!   and bots; this module adds the registry entry from the same row, plus
//!   the six exchange-published internals every series has (strike,
//!   delta from strike, price, window state, trades, stats).
//! * **Signals** come from the signal definition files the signals crates
//!   ship under `definitions/<underlying>/signals/` (one JSON per
//!   identity, validated by `schemas/signal-definition.schema.json`). A
//!   processor cannot ship without its definition, so the file you already
//!   write to add a signal is its registry entry. Each is tagged with the
//!   lane that publishes it and whether that lane is running in this
//!   process.
//! * The live BTC price feed (Pyth Pro, or the Coinbase fallback) is the
//!   one external signal.
//!
//! # Scope
//!
//! Every signal carries a [`Scope`] in `metadata.scope`, and every series
//! an `underlying` ([`crate::series::SeriesSpec::underlying`]). The two
//! meet in [`signals_for`], the one answer to "which signals apply to
//! this market": the series' own internals (`Scope::Series`), every
//! signal about its underlying (`Scope::Underlying`, derived from the
//! definitions directory the file lives in), and cross-asset signals
//! (`Scope::Global`). A definition may override its derived scope with an
//! explicit `scope` object; the prototype's `scope_kind` is provenance
//! only and does not widen a BTC signal to other assets.
//!
//! The document is built as JSON and then parsed through the compiler's
//! own `Registry` type (`deny_unknown_fields`), so an entry this module
//! cannot express is a start-up error rather than a silently wrong file.
//! The result is written to `REGISTRY_PATH` (default `registry.json`) and
//! served at `GET /api/registry`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use strategy_compiler::Registry;

use crate::copy::{CopyBook, copy_dir};
use crate::series::{SERIES, SeriesSpec};

/// Where the generated registry is written.
pub const REGISTRY_PATH_ENV: &str = "REGISTRY_PATH";
/// Root of the signal definition tree (`<root>/<underlying>/signals/*.json`).
/// When unset the module tries the working directory's `definitions`, then
/// the checkout's.
pub const DEFINITIONS_DIR_ENV: &str = "SIGNALS_DEFINITIONS_DIR";

const DEFAULT_REGISTRY_PATH: &str = "registry.json";
const DEFINITIONS_RELATIVE: &str = "definitions";
const DEFINITIONS_IN_CHECKOUT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../definitions");
/// The subdirectory of an underlying that holds its signal definitions.
const SIGNALS_SUBDIR: &str = "signals";
/// The underlying the live price feed is about.
const PRICE_FEED_UNDERLYING: &str = "btc";

/// Finest price step a strategy may name, in dollars: the engine's book
/// is a tenth-of-a-cent grid at the extremes (`ticks.rs`).
const QUOTE_TICK: &str = "0.001";
const QUOTE_RANGE: [&str; 2] = ["0.001", "0.999"];

/// Which live BTC price source the exchange is running on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PriceSource {
    PythPro,
    Coinbase,
}

/// What the registry is built from beyond the static tables.
#[derive(Debug, Clone)]
pub struct Inputs<'a> {
    pub price_source: PriceSource,
    /// Names of the signal lanes the supervisor started, so each signal
    /// can say whether it is being published in this process.
    pub live_lanes: &'a [&'a str],
    /// Override for the definitions root (tests). `None` resolves from
    /// the environment and the defaults.
    pub definitions_dir: Option<PathBuf>,
    /// Override for the copy directory (tests); see [`crate::copy`].
    pub copy_dir: Option<PathBuf>,
}

/// Which markets a signal applies to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Scope {
    /// One series' own state: strike, book, window, tape, stats.
    Series { series_id: String },
    /// Everything about one asset or event, whichever series trades it.
    Underlying { underlying: String },
    /// Cross-asset: applies to every market.
    Global,
}

impl Scope {
    /// Read a signal's scope from its registry metadata. A signal without
    /// one applies nowhere, so a hand-edited registry cannot leak a signal
    /// into every market by omission.
    pub fn of(def: &strategy_compiler::ir::SignalDef) -> Option<Scope> {
        let meta = def.metadata.as_ref()?;
        serde_json::from_value(meta.get("scope")?.clone()).ok()
    }

    /// Whether a signal with this scope applies to `series`.
    pub fn applies_to(&self, series: &SeriesSpec) -> bool {
        match self {
            Scope::Series { series_id } => series_id == series.slug,
            Scope::Underlying { underlying } => underlying == series.underlying,
            Scope::Global => true,
        }
    }
}

/// Every signal that applies to `series`: its own internals, its
/// underlying's feeds, and the global ones, in registry order. Unknown
/// slugs resolve to nothing.
pub fn signals_for<'a>(
    registry: &'a Registry,
    series_slug: &str,
) -> impl Iterator<Item = &'a strategy_compiler::ir::SignalDef> + 'a {
    let spec = SERIES.iter().find(|s| s.slug == series_slug);
    registry
        .signals
        .iter()
        .filter(move |def| spec.is_some_and(|spec| Scope::of(def).is_some_and(|sc| sc.applies_to(spec))))
}

/// Build the registry, write it to `REGISTRY_PATH`, and return it. Errors
/// only on a definition file the compiler's type cannot express or an
/// unwritable path; a missing definitions directory yields a registry
/// with the series and the price feed and a warning.
pub fn generate_and_write(inputs: &Inputs<'_>) -> Result<Registry> {
    let registry = build(inputs)?;
    let path = std::env::var(REGISTRY_PATH_ENV)
        .ok()
        .filter(|p| !p.trim().is_empty())
        .map_or_else(|| PathBuf::from(DEFAULT_REGISTRY_PATH), PathBuf::from);
    write(&registry, &path)?;
    tracing::info!(
        path = %path.display(),
        series = registry.series.len(),
        signals = registry.signals.len(),
        "registry: generated"
    );
    Ok(registry)
}

/// Serialize pretty JSON to `path` atomically (write a sibling, rename).
pub fn write(registry: &Registry, path: &Path) -> Result<()> {
    let text = serde_json::to_string_pretty(registry)?;
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create {}", parent.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("rename to {}", path.display()))?;
    Ok(())
}

/// Assemble the registry as JSON and parse it through the compiler's type.
pub fn build(inputs: &Inputs<'_>) -> Result<Registry> {
    let mut signals: Vec<Value> = Vec::new();
    signals.push(price_feed_signal(inputs.price_source));
    // Scoped to the `nba` underlying: they reach a series only once one
    // with that underlying is in the table.
    signals.extend(nba_signals());
    for spec in SERIES {
        signals.extend(series_signals(spec));
    }
    match definitions_dir(inputs.definitions_dir.as_deref()) {
        Some(root) => {
            let (defs, count) = definition_signals(&root, inputs.live_lanes)?;
            tracing::info!(root = %root.display(), count, "registry: signal definitions loaded");
            signals.extend(defs);
        }
        None => tracing::warn!(
            "registry: no signal definitions root found (set {DEFINITIONS_DIR_ENV}); only exchange signals listed"
        ),
    }
    // The words a person reads, from `copy/signals/<indicator>.json`.
    // A signal with no copy file keeps its spec name and shows no field
    // by default; a warning names it so the file gets written.
    let book = match copy_dir(inputs.copy_dir.as_deref()) {
        Some(dir) => CopyBook::load(&dir)?,
        None => {
            tracing::warn!("registry: no copy directory found (set {}); signals keep their spec names", crate::copy::COPY_DIR_ENV);
            CopyBook::default()
        }
    };
    let mut without_copy = Vec::new();
    for signal in &mut signals {
        if !apply_copy(signal, &book) {
            without_copy.push(signal["signal_id"].as_str().unwrap_or("?").to_string());
        }
    }
    if !without_copy.is_empty() {
        tracing::warn!(count = without_copy.len(), first = %without_copy[0], "registry: signals without copy");
    }
    let doc = json!({
        "markets": [],
        "series": SERIES.iter().map(series_entry).collect::<Vec<_>>(),
        "signals": signals,
    });
    serde_json::from_value(doc).context("generated registry does not match the compiler's Registry type")
}

/// Merge a signal's copy into its registry entry: the title becomes its
/// name, the two sentences its description (the spec prose moves to
/// `metadata.spec_description`), and each field gains its label, its
/// description and `headline: true` when the copy lists it. Fields the
/// copy does not list are plumbing and get `headline: false`. Returns
/// whether the signal had copy at all.
fn apply_copy(signal: &mut Value, book: &CopyBook) -> bool {
    let id = signal["signal_id"].as_str().unwrap_or("").to_string();
    let copy = book.for_signal_in(&id, underlying_of(signal));
    if let Some(copy) = copy {
        let spec = signal["description"].as_str().unwrap_or("").to_string();
        if !spec.is_empty() && spec != copy.description {
            signal["metadata"]["spec_description"] = json!(spec);
        }
        signal["name"] = json!(copy.title);
        signal["description"] = json!(copy.description);
    }
    if let Some(fields) = signal["fields"].as_array_mut() {
        for field in fields {
            let name = field["name"].as_str().unwrap_or("").to_string();
            match copy.and_then(|c| c.fields.get(&name)) {
                Some(fc) => {
                    field["label"] = json!(fc.label);
                    field["description"] = json!(fc.description);
                    field["headline"] = json!(true);
                }
                None => {
                    field["headline"] = json!(false);
                }
            }
        }
    }
    copy.is_some()
}

/// The underlying of a series internal, from the series its metadata
/// names; `None` for every other signal.
fn underlying_of(signal: &Value) -> Option<&'static str> {
    let slug = signal["metadata"]["series_id"].as_str()?;
    crate::types::SeriesId::from_slug(slug).ok().map(|id| id.underlying())
}

fn definitions_dir(override_dir: Option<&Path>) -> Option<PathBuf> {
    let candidates: Vec<PathBuf> = override_dir
        .map(Path::to_path_buf)
        .into_iter()
        .chain(std::env::var(DEFINITIONS_DIR_ENV).ok().map(PathBuf::from))
        .chain([PathBuf::from(DEFINITIONS_RELATIVE), PathBuf::from(DEFINITIONS_IN_CHECKOUT)])
        .collect();
    candidates.into_iter().find(|p| p.is_dir())
}

// ───────────────────────────── series ─────────────────────────────

/// `120` → `2m`, `900` → `15m`, `90` → `90s`.
pub fn cadence(duration_secs: u64) -> String {
    if duration_secs.is_multiple_of(3600) {
        format!("{}h", duration_secs / 3600)
    } else if duration_secs.is_multiple_of(60) {
        format!("{}m", duration_secs / 60)
    } else {
        format!("{duration_secs}s")
    }
}

/// The authoring description of a series, by underlying.
fn series_description(spec: &SeriesSpec) -> String {
    let c = cadence(spec.duration_secs);
    match spec.underlying {
        "nba" => format!(
            "{c} NBA head-to-head. Every window is one simulated game between two of the league's teams; the matchup and the sharp pregame line are captured when the window opens, and at close the final score decides home or away. The book quotes the home side; away is its complement."
        ),
        _ => format!(
            "{c} BTC up/down. The strike is the BTC price when the round opens; at close the price decides up (at or above the strike) or down. The book quotes the up side; down is its complement."
        ),
    }
}

fn series_entry(spec: &SeriesSpec) -> Value {
    json!({
        "series_id": spec.slug,
        "structure": "outcome",
        "cadence": cadence(spec.duration_secs),
        "outcomes": spec.outcomes,
        "quote": { "tick": QUOTE_TICK, "range": QUOTE_RANGE },
        // The engine implements all three order flags (`PlaceOpts` on
        // `MarketEngine`; `EngineVenue` arms good-till-time expiry), so
        // the compiler accepts them.
        "capabilities": { "post_only": true, "gtd": true, "reduce_only": true },
        "metadata": {
            "underlying": spec.underlying,
            "quote_ccy": "usd",
            "venue": "tiny",
            "duration_secs": spec.duration_secs,
            "description": series_description(spec),
            "_runtime_note": "The book is a non-uniform tick grid (0.1c at the extremes, 1c in the middle). Limit prices snap to the nearest venue tick at placement; the registry tick is the finest grid a price expression may name."
        }
    })
}

/// Per-series round statistics: the six numbers on the stats strip,
/// published by `automation::stats_signal` on every change.
pub const STATS_FIELDS: [(&str, &str, &str); 6] = [
    ("volume", "Round Volume", "decimal"),
    ("open_interest", "Open Interest (lots)", "u32"),
    ("collateral", "Open Interest", "decimal"),
    ("trades", "Trades", "u32"),
    ("traders", "Traders", "u32"),
    ("largest_trade", "Largest Trade", "u32"),
];

/// Signal id of a series' round statistics.
pub fn stats_signal_id(slug: &str) -> String {
    format!("{slug}.current.stats.v1")
}

/// The underlying of the simulated NBA series.
pub const NBA_UNDERLYING: &str = "nba";

/// Signal ids of the NBA world's feeds and derived signals.
pub const NBA_PBP_SIGNAL: &str = "nba.sim.pbp.v1";
pub const NBA_SHARP_LINE_SIGNAL: &str = "nba.sim.sharp_line.v1";
pub const NBA_MARGIN_SIGNAL: &str = "nba.sim.margin.v1";
pub const NBA_WINPROB_SIGNAL: &str = "nba.sim.winprob.v1";

/// Signal id of a series' matchup internal (NBA series only).
pub fn matchup_signal_id(slug: &str) -> String {
    format!("{slug}.current.matchup.v1")
}

/// The `enum` field declaration over the league's team codes.
fn team_field(name: &str, label: &str) -> Value {
    json!({"name": name, "label": label, "type": "enum", "variants": crate::sim::nba::league::team_codes()})
}

/// The four signals the NBA world publishes, scoped to every series with
/// the `nba` underlying: the two feeds (play-by-play and the sharp line)
/// and the two derived from them (margin and model win probability). The
/// play-by-play metadata mirrors the roster table so the ids the events
/// carry resolve to names. Each is marked `live`: the catalogue only
/// assumes that for series-scoped internals, and these are published by
/// this process whenever an NBA series is in the table.
pub fn nba_signals() -> Vec<Value> {
    use crate::sim::nba::league::{ROSTERS, team_codes};
    let scope = json!(Scope::Underlying { underlying: NBA_UNDERLYING.to_string() });
    let players: BTreeMap<String, Value> = ROSTERS
        .iter()
        .map(|p| (p.id.to_string(), json!({"name": p.name, "team": p.team})))
        .collect();
    vec![
        json!({
            "signal_id": NBA_PBP_SIGNAL, "version": "1", "location": "internal",
            "name": "Play-by-play",
            "description": "Every play of the active simulated NBA game with full game context: action, result, team, player, quarter, clock and the scores before and after. Game-scoped: the final stays as recorded state until the next tipoff. clock_remaining is published last.",
            "fields": [
                {"name": "seq", "label": "Play", "type": "u32"},
                {"name": "action", "label": "Action", "type": "enum", "variants": ["tipoff", "two_pointer", "three_pointer", "free_throw", "turnover", "rebound", "foul", "final"]},
                {"name": "result", "label": "Result", "type": "enum", "variants": ["made", "missed", "none"]},
                team_field("team", "Team"),
                {"name": "player", "label": "Player", "type": "string"},
                {"name": "phase", "label": "Phase", "type": "enum", "variants": ["in_play", "final"]},
                team_field("home_team", "Home"),
                team_field("away_team", "Away"),
                {"name": "quarter", "label": "Quarter", "type": "u8"},
                {"name": "home_score_before", "label": "Home Before", "type": "u32"},
                {"name": "away_score_before", "label": "Away Before", "type": "u32"},
                {"name": "home_score", "label": "Home Score", "type": "u32"},
                {"name": "away_score", "label": "Away Score", "type": "u32"},
                {"name": "clock_remaining", "label": "Clock", "type": "decimal"}
            ],
            "metadata": {
                "scope": scope, "producer": "nba_world", "live": true, "approximate_interval_ms": 0,
                "team_semantics": "offense for shots, turnovers and free throws; defense for fouls and rebounds",
                "teams": team_codes(), "players": players
            }
        }),
        json!({
            "signal_id": NBA_SHARP_LINE_SIGNAL, "version": "1", "location": "internal",
            "name": "Sharp line",
            "description": "A sharp book's de-vigged fair win probabilities for the game in play or the upcoming one. Publishes on a line move of at least half a point and on a matchup change; the matchup change is how the next game is announced before its window opens.",
            "fields": [
                team_field("home_team", "Home"),
                team_field("away_team", "Away"),
                {"name": "home_prob", "label": "Home Win", "type": "decimal"},
                {"name": "away_prob", "label": "Away Win", "type": "decimal"}
            ],
            "metadata": { "scope": scope, "producer": "sharp_line", "venue": "pinnacle_like", "live": true, "approximate_interval_ms": 500 }
        }),
        json!({
            "signal_id": NBA_MARGIN_SIGNAL, "version": "1", "location": "internal",
            "name": "Score margin",
            "description": "Home score minus away score for the game in play, republished on every play and reset to zero at tipoff.",
            "fields": [ {"name": "value", "label": "Margin", "type": "decimal"} ],
            "metadata": { "scope": scope, "producer": "score_margin", "inputs": [NBA_PBP_SIGNAL], "live": true, "approximate_interval_ms": 0 }
        }),
        json!({
            "signal_id": NBA_WINPROB_SIGNAL, "version": "1", "location": "internal",
            "name": "Model win probability",
            "description": "P(home): the sharp prior before tipoff, then the margin and clock diffusion model in play with the pregame prior frozen as drift. The same formula the bots' fair value uses.",
            "fields": [ {"name": "home", "label": "Home Win", "type": "decimal"} ],
            "metadata": { "scope": scope, "producer": "pbp_winprob", "inputs": [NBA_PBP_SIGNAL, NBA_SHARP_LINE_SIGNAL], "live": true, "approximate_interval_ms": 0 }
        }),
    ]
}

/// The internals the exchange publishes for every series: four shared
/// (price, window, trades, stats) plus the underlying's own reference
/// signals (BTC: strike and delta; NBA: matchup).
fn series_signals(spec: &SeriesSpec) -> Vec<Value> {
    let s = spec.slug;
    let sid = |kind: &str| format!("{s}.current.{kind}.v1");
    let scope = json!(Scope::Series { series_id: s.to_string() });
    let [pos, neg] = spec.outcomes;
    let (pos_label, neg_label) = (field_label(pos), field_label(neg));
    let mut out = Vec::new();
    if spec.underlying == NBA_UNDERLYING {
        out.push(json!({
            "signal_id": matchup_signal_id(s), "version": "1", "location": "internal",
            "name": "Round Matchup",
            "description": format!("The game the current {s} round trades: the home and away teams and the sharp pregame home win probability, captured when the round opened, with the world's game number."),
            "fields": [
                team_field("home_team", "Home"),
                team_field("away_team", "Away"),
                {"name": "pregame_home_prob", "label": "Pregame Home Win", "type": "decimal"},
                {"name": "game_seq", "label": "Game", "type": "u64"}
            ],
            "metadata": { "series_id": s, "scope": scope }
        }));
    } else {
        out.push(json!({
            "signal_id": sid("strike"), "version": "1", "location": "internal",
            "name": "Round Strike",
            "description": format!("The strike of the current {s} round: the BTC price captured when it opened."),
            "fields": [ {"name": "price", "label": "Strike", "type": "decimal"} ],
            "metadata": { "series_id": s, "scope": scope }
        }));
        out.push(json!({
            "signal_id": sid("delta"), "version": "1", "location": "internal",
            "name": "Strike Delta",
            "description": format!("How far the live BTC price sits from the current {s} round's strike, in signed basis points, with the spot and strike it was computed from. Republished on every spot tick while a round is open."),
            "fields": [
                {"name": "delta_bps", "label": "Delta (bps)", "type": "decimal"},
                {"name": "spot", "label": "Spot", "type": "decimal"},
                {"name": "strike", "label": "Strike", "type": "decimal"}
            ],
            "metadata": { "series_id": s, "scope": scope, "approximate_interval_ms": 0 }
        }));
    }
    out.extend([
        json!({
            "signal_id": sid("price"), "version": "1", "location": "internal",
            "name": "Market Price",
            "description": format!("Top of book and last trade for the current {s} round, as {pos}-side probabilities; {neg} is the complement."),
            "fields": [
                {"name": pos, "label": pos_label, "type": "decimal"}, {"name": neg, "label": neg_label, "type": "decimal"},
                {"name": "bid", "label": "Best Bid", "type": "decimal"}, {"name": "ask", "label": "Best Ask", "type": "decimal"},
                {"name": "last", "label": "Last Trade", "type": "decimal"}
            ],
            // Republished on every book change: no interval to declare, so
            // `0` marks it as per-event for the compiler's requote-rate lint.
            "metadata": { "series_id": s, "scope": scope, "quotes_outcome": pos, "outcome_fields": {pos: pos, neg: neg}, "approximate_interval_ms": 0 }
        }),
        json!({
            "signal_id": sid("window"), "version": "1", "location": "internal",
            "name": "Round Clock",
            "description": format!("Lifecycle of the current {s} round: phase, round sequence and time remaining. Wakes strategies at every transition."),
            "fields": [
                {"name": "phase", "label": "Phase", "type": "enum", "variants": ["open", "locked", "settled"]},
                {"name": "seq", "label": "Round", "type": "u32"},
                {"name": "remaining", "label": "Time Left", "type": "duration_ms"}
            ],
            "metadata": { "series_id": s, "scope": scope, "role": "state" }
        }),
        json!({
            "signal_id": sid("trades"), "version": "1", "location": "internal",
            "name": "Trade Tape",
            "description": format!("Every fill in the current {s} round: taker, side, price and size."),
            "fields": [
                {"name": "taker", "label": "Taker", "type": "string"},
                {"name": "side", "label": "Side", "type": "enum", "variants": ["buy", "sell"]},
                {"name": "price", "label": "Fill Price", "type": "decimal"},
                {"name": "size", "label": "Fill Size", "type": "decimal"}
            ],
            "metadata": { "series_id": s, "scope": scope, "quotes_outcome": pos, "approximate_interval_ms": 0 }
        }),
        json!({
            "signal_id": stats_signal_id(s), "version": "1", "location": "internal",
            "name": "Round Stats",
            "description": format!("Round statistics for the current {s} round, republished on every change and reset when a round opens: notional volume and collateral locked in dollars, open interest and largest fill in contracts, fill count, and distinct traders by root identity."),
            "fields": STATS_FIELDS.iter().map(|(n, l, t)| json!({"name": n, "label": l, "type": t})).collect::<Vec<_>>(),
            "metadata": { "series_id": s, "scope": scope, "levels": "cumulative within the round; derive rates with a window" }
        }),
    ]);
    out
}

/// The registry id of the live price feed signal for `source`.
pub fn price_feed_signal_id(source: PriceSource) -> String {
    match source {
        PriceSource::PythPro => "pyth.btc_usd.spot.v1".to_string(),
        PriceSource::Coinbase => "coinbase.btc_usd.spot.v1".to_string(),
    }
}

fn price_feed_signal(source: PriceSource) -> Value {
    let scope = json!(Scope::Underlying { underlying: PRICE_FEED_UNDERLYING.to_string() });
    match source {
        PriceSource::PythPro => json!({
            "signal_id": price_feed_signal_id(source), "version": "1", "location": "external",
            "name": "BTC/USD Spot",
            "description": "Pyth Pro BTC/USD aggregate (feed id 1), real-time channel: price and confidence interval in dollars. The price every round's strike and settlement are read from.",
            "fields": [ {"name": "last", "label": "BTC/USD Spot", "type": "decimal"}, {"name": "conf", "label": "Confidence", "type": "decimal"} ],
            "metadata": { "venue": "pyth", "pair": "btc_usd", "scope": scope }
        }),
        PriceSource::Coinbase => json!({
            "signal_id": price_feed_signal_id(source), "version": "1", "location": "external",
            "name": "BTC/USD Spot",
            "description": "Coinbase BTC-USD public ticker (keyless fallback when no Pyth key is set). The price every round's strike and settlement are read from.",
            "fields": [ {"name": "last", "label": "BTC/USD Spot", "type": "decimal"} ],
            "metadata": { "venue": "coinbase", "pair": "btc_usd", "scope": scope }
        }),
    }
}

// ─────────────────────── signal definitions ───────────────────────

/// The lane that publishes a definition, from its id's venue segment.
pub fn lane_for(signal_id: &str) -> &'static str {
    match signal_id.split('.').nth(1).unwrap_or("") {
        "coinbase" | "kraken" | "external" => "spot",
        "polymarket" => "polymarket",
        "kalshi" => "kalshi",
        "binance" => "binance",
        "bybit" => "bybit",
        "chainlink" | "cfbrti" | "pyth" => "oracle",
        "cross" => "cross",
        "multi" => "multi",
        _ => "unknown",
    }
}

/// Translate one definition file into a registry signal entry. `underlying`
/// is the directory the file lives under; an explicit `scope` object in the
/// file overrides it (for a truly cross-asset signal, `{"kind": "global"}`).
fn translate_definition(def: &Value, underlying: &str, live_lanes: &[&str]) -> Result<Value> {
    let id = def["signal_id"].as_str().context("definition has no signal_id")?;
    let version = def["version"].as_str().context("definition has no version")?;
    let major = version.split('.').next().unwrap_or("1");
    let fields = def["fields"]
        .as_array()
        .context("definition has no fields")?
        .iter()
        .map(|f| {
            let mut out = serde_json::Map::new();
            out.insert("name".into(), f["name"].clone());
            let label = f
                .get("label")
                .and_then(Value::as_str)
                .map_or_else(|| field_label(f["name"].as_str().unwrap_or("")), str::to_string);
            out.insert("label".into(), json!(label));
            out.insert("type".into(), f["type"].clone());
            if let Some(v) = f.get("variants") {
                out.insert("variants".into(), v.clone());
            }
            Value::Object(out)
        })
        .collect::<Vec<_>>();
    let lane = lane_for(id);
    let mut metadata = BTreeMap::new();
    metadata.insert("lane", json!(lane));
    metadata.insert("live", json!(live_lanes.contains(&lane)));
    metadata.insert("definition_version", json!(version));
    let scope = match def.get("scope") {
        Some(v) => serde_json::from_value::<Scope>(v.clone())
            .with_context(|| format!("{id}: `scope` is not a registry scope"))?,
        None => Scope::Underlying { underlying: underlying.to_string() },
    };
    metadata.insert("scope", json!(scope));
    for key in [
        "name",
        "status",
        "approximate_interval_ms",
        "staleness_bound_ms",
        "inputs",
        "event_time_field",
        "scope_kind",
    ] {
        if let Some(v) = def.get(key) {
            metadata.insert(key, v.clone());
        }
    }
    Ok(json!({
        "signal_id": format!("{id}.v{major}"),
        "version": major,
        "name": def["name"].as_str().unwrap_or(id),
        "location": "internal",
        "description": def["description"].as_str().unwrap_or(""),
        "fields": fields,
        "metadata": metadata,
    }))
}

/// A readable label for a definition field that declares none:
/// `volatility_bps_60s` → `Volatility (bps) 60s`, `input_age_ms` →
/// `Input Age (ms)`, `rsi_14` → `RSI 14`.
pub fn field_label(name: &str) -> String {
    name.split('_')
        .filter(|w| !w.is_empty())
        .map(|w| match w {
            "bps" | "ms" | "pct" | "usd" => format!("({w})"),
            "rsi" | "roc" | "macd" | "atr" | "ema" | "vwap" | "oi" | "cvd" | "btc" | "usdt" | "twap" => w.to_ascii_uppercase(),
            _ => {
                let mut c = w.chars();
                match c.next() {
                    Some(first) => first.to_ascii_uppercase().to_string() + c.as_str(),
                    None => String::new(),
                }
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Read every `<root>/<underlying>/signals/*.json`, underlyings and files
/// each sorted by name for a stable document. Returns the entries and how
/// many files were read.
fn definition_signals(root: &Path, live_lanes: &[&str]) -> Result<(Vec<Value>, usize)> {
    let mut underlyings: Vec<PathBuf> = std::fs::read_dir(root)
        .with_context(|| format!("read {}", root.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.join(SIGNALS_SUBDIR).is_dir())
        .collect();
    underlyings.sort();
    let mut out = Vec::new();
    let mut count = 0;
    for dir in &underlyings {
        let underlying = dir
            .file_name()
            .and_then(|n| n.to_str())
            .with_context(|| format!("non-UTF-8 underlying directory {}", dir.display()))?;
        let mut paths: Vec<PathBuf> = std::fs::read_dir(dir.join(SIGNALS_SUBDIR))
            .with_context(|| format!("read {}", dir.display()))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .collect();
        paths.sort();
        for path in &paths {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("read {}", path.display()))?;
            let def: Value = serde_json::from_str(&text)
                .with_context(|| format!("parse {}", path.display()))?;
            out.push(
                translate_definition(&def, underlying, live_lanes)
                    .with_context(|| format!("translate {}", path.display()))?,
            );
        }
        count += paths.len();
    }
    Ok((out, count))
}

/// The internals an NBA series would get, typed, for tests that run
/// before an NBA row is in the table.
pub fn nba_series_internals_for_tests(spec: &SeriesSpec) -> Vec<strategy_compiler::ir::SignalDef> {
    series_signals(spec).into_iter().map(|v| serde_json::from_value(v).expect("NBA internals type")).collect()
}

/// A registry for hand-built test states: exchange signals only, no
/// definition files, Coinbase as the price source. Not `cfg(test)` so the
/// crate's integration tests and `engine::strategies::test_support` can
/// compile fixtures against it.
pub fn for_tests() -> std::sync::Arc<Registry> {
    let empty = std::env::temp_dir().join("tiny-registry-no-definitions");
    let _ = std::fs::create_dir_all(&empty);
    std::sync::Arc::new(
        build(&Inputs {
            price_source: PriceSource::Coinbase,
            live_lanes: &[],
            definitions_dir: Some(empty.clone()),
            copy_dir: Some(empty),
        })
        .expect("test registry builds"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use strategy_compiler::ir::{FieldType, SignalLocation};

    fn inputs<'a>(live: &'a [&'a str]) -> Inputs<'a> {
        Inputs {
            price_source: PriceSource::PythPro,
            live_lanes: live,
            definitions_dir: Some(PathBuf::from(DEFINITIONS_IN_CHECKOUT)),
            copy_dir: None,
        }
    }

    #[test]
    fn every_signal_has_copy_and_every_copy_field_exists() {
        let r = build(&inputs(&[])).expect("builds");
        let book = CopyBook::load(&copy_dir(None).expect("copy directory in the checkout")).unwrap();
        let mut missing = Vec::new();
        // One copy file covers every venue's variant of an indicator, and the
        // variants differ a little (the blend has `combined_imbalance`, the
        // venues `imbalance`), so a copy field must exist in at least one.
        // The series internals of every underlying count, whether or not a
        // series with that underlying is in the table yet.
        let nba_spec = SeriesSpec {
            id: crate::types::SeriesId(0),
            slug: "nba-h2h",
            duration_secs: 300,
            underlying: NBA_UNDERLYING,
            outcomes: crate::series::NBA_OUTCOMES,
        };
        let extra: Vec<strategy_compiler::ir::SignalDef> = series_signals(&nba_spec)
            .into_iter()
            .map(|v| serde_json::from_value(v).expect("NBA internals type"))
            .collect();
        let mut fields_by_indicator: std::collections::BTreeMap<&str, std::collections::BTreeSet<&str>> = Default::default();
        for sig in r.signals.iter().chain(extra.iter()) {
            let entry = fields_by_indicator.entry(crate::copy::indicator_of(&sig.signal_id)).or_default();
            entry.extend(sig.fields.iter().map(|f| f.name.as_str()));
        }
        for key in book.indicators() {
            // A per-underlying file (`nba.price`) is keyed by the shared indicator.
            let (underlying, indicator) = crate::copy::split_key(key);
            if let Some(u) = underlying {
                assert!(SERIES.iter().any(|s| s.underlying == u) || u == NBA_UNDERLYING, "copy file `{key}` names an unknown underlying");
            }
            let known = fields_by_indicator.get(indicator).unwrap_or_else(|| panic!("copy file `{key}` matches no signal"));
            for name in book.get(key).unwrap().fields.keys() {
                assert!(known.contains(name.as_str()), "{key}: copy names a field `{name}` no definition has");
            }
        }
        for sig in &r.signals {
            let underlying = sig.metadata.as_ref().and_then(|m| m["series_id"].as_str()).and_then(|s| crate::types::SeriesId::from_slug(s).ok()).map(|id| id.underlying());
            let Some(copy) = book.for_signal_in(&sig.signal_id, underlying) else {
                missing.push(sig.signal_id.clone());
                continue;
            };
            assert_eq!(sig.name.as_deref(), Some(copy.title.as_str()), "{}", sig.signal_id);
            assert_eq!(sig.description, copy.description, "{}", sig.signal_id);
            for (name, fc) in &copy.fields {
                let Some(field) = sig.fields.iter().find(|f| f.name.as_str() == name) else { continue };
                assert_eq!(field.label.as_deref(), Some(fc.label.as_str()));
                assert_eq!(field.description.as_deref(), Some(fc.description.as_str()));
                assert_eq!(field.headline, Some(true));
            }
            for field in &sig.fields {
                if !copy.fields.contains_key(field.name.as_str()) {
                    assert_eq!(field.headline, Some(false), "{}.{}", sig.signal_id, field.name.as_str());
                    assert!(field.description.is_none());
                }
            }
            assert!(
                sig.fields.iter().any(|f| f.headline == Some(true)) || copy.fields.is_empty(),
                "{}: copy lists fields but none landed",
                sig.signal_id
            );
        }
        assert!(missing.is_empty(), "signals without a copy file: {missing:?}");
        // Descriptions are two sentences, not spec prose.
        for indicator in book.indicators() {
            let copy = book.get(indicator).unwrap();
            for (name, text) in std::iter::once(("description", &copy.description))
                .chain(copy.fields.iter().map(|(n, f)| (n.as_str(), &f.description)))
            {
                let sentences = text.split(". ").count();
                assert!((1..=2).contains(&sentences), "{indicator}.{name}: {sentences} sentences");
                assert!(text.len() <= 320, "{indicator}.{name}: too long");
            }
        }
    }

    #[test]
    fn cadence_formats() {
        assert_eq!(cadence(120), "2m");
        assert_eq!(cadence(900), "15m");
        assert_eq!(cadence(90), "90s");
        assert_eq!(cadence(3600), "1h");
    }

    #[test]
    fn every_series_row_becomes_a_series_with_its_six_internals() {
        let r = build(&inputs(&[])).expect("builds");
        assert_eq!(r.series.len(), SERIES.len());
        for spec in SERIES {
            let s = r.series.iter().find(|s| s.series_id == spec.slug).expect("series listed");
            assert_eq!(s.metadata.as_ref().unwrap()["underlying"], spec.underlying);
            assert_eq!(s.cadence.as_millis(), spec.duration_secs * 1000);
            let labels: Vec<String> = spec.outcomes.iter().map(|o| o.to_string()).collect();
            assert_eq!(s.outcomes.as_deref(), Some(&labels[..]));
            if !spec.id.is_btc() {
                continue;
            }
            for kind in ["strike", "delta", "price", "window", "trades", "stats"] {
                let id = format!("{}.current.{kind}.v1", spec.slug);
                let sig = r.signals.iter().find(|x| x.signal_id == id).unwrap_or_else(|| panic!("{id}"));
                assert_eq!(sig.location, SignalLocation::Internal);
                assert_eq!(sig.metadata.as_ref().unwrap()["series_id"], spec.slug);
            }
            let window = r.signals.iter().find(|x| x.signal_id == format!("{}.current.window.v1", spec.slug)).unwrap();
            assert_eq!(window.metadata.as_ref().unwrap()["role"], "state");
            // The per-event internals declare a zero interval so the
            // compiler's requote-rate lint sees them as sub-second.
            for kind in ["price", "trades", "delta"] {
                let sig = r.signals.iter().find(|x| x.signal_id == format!("{}.current.{kind}.v1", spec.slug)).unwrap();
                assert_eq!(sig.metadata.as_ref().unwrap()["approximate_interval_ms"], 0);
            }
            let delta = r.signals.iter().find(|x| x.signal_id == format!("{}.current.delta.v1", spec.slug)).unwrap();
            let names: Vec<&str> = delta.fields.iter().map(|f| f.name.as_str()).collect();
            assert_eq!(names, ["delta_bps", "spot", "strike"]);
            let strike = r.signals.iter().find(|x| x.signal_id == format!("{}.current.strike.v1", spec.slug)).unwrap();
            assert!(strike.metadata.as_ref().unwrap().get("approximate_interval_ms").is_none(), "once a round");
        }
    }

    /// The NBA entries type through the compiler's `Registry`, every
    /// field is a real compiler type, the roster metadata mirrors the
    /// league table, and each has a copy file. Built as a registry of
    /// their own so the test holds before an NBA series row exists.
    #[test]
    fn nba_signals_type_and_mirror_the_roster() {
        use crate::sim::nba::league::{ROSTERS, TEAMS};
        let nba_spec = SeriesSpec {
            id: crate::types::SeriesId(0),
            slug: "nba-h2h",
            duration_secs: 300,
            underlying: NBA_UNDERLYING,
            outcomes: crate::series::NBA_OUTCOMES,
        };
        let mut signals = nba_signals();
        signals.extend(series_signals(&nba_spec));
        let book = CopyBook::load(&copy_dir(None).expect("copy directory in the checkout")).unwrap();
        for signal in &mut signals {
            assert!(apply_copy(signal, &book), "{} has no copy file", signal["signal_id"]);
        }
        let doc = json!({ "markets": [], "series": [series_entry(&nba_spec)], "signals": signals });
        let r: Registry = serde_json::from_value(doc).expect("NBA entries match the compiler's Registry type");
        assert_eq!(r.series[0].outcomes.as_deref(), Some(&["home".to_string(), "away".to_string()][..]));
        let ids: Vec<&str> = r.signals.iter().map(|s| s.signal_id.as_str()).collect();
        for id in [NBA_PBP_SIGNAL, NBA_SHARP_LINE_SIGNAL, NBA_MARGIN_SIGNAL, NBA_WINPROB_SIGNAL] {
            assert!(ids.contains(&id), "{id}");
            let def = r.signals.iter().find(|s| s.signal_id == id).unwrap();
            assert_eq!(Scope::of(def), Some(Scope::Underlying { underlying: "nba".into() }));
            assert!(Scope::of(def).unwrap().applies_to(&nba_spec));
            assert!(!Scope::of(def).unwrap().applies_to(&SERIES[0]), "not a BTC signal");
            assert_eq!(def.metadata.as_ref().unwrap()["live"], true, "{id} is catalogued live");
        }
        let expected_internals: Vec<String> =
            ["matchup", "price", "window", "trades", "stats"].iter().map(|k| format!("nba-h2h.current.{k}.v1")).collect();
        for id in &expected_internals {
            assert!(ids.contains(&id.as_str()), "{id}");
        }
        assert!(!ids.iter().any(|id| id.contains(".strike.") || id.contains(".delta.")), "no BTC internals on an NBA series");
        let price = r.signals.iter().find(|s| s.signal_id == "nba-h2h.current.price.v1").unwrap();
        assert_eq!(price.fields[0].name.as_str(), "home");
        assert_eq!(price.fields[1].name.as_str(), "away");
        assert_eq!(price.metadata.as_ref().unwrap()["quotes_outcome"], "home");
        // The NBA series' price and clock carry their own copy: home and
        // away are headline fields and nothing speaks of Up or the strike.
        assert_eq!(price.fields[0].headline, Some(true), "home is a headline field");
        assert_eq!(price.fields[1].headline, Some(true), "away is a headline field");
        assert!(!price.description.contains("Up"), "{}", price.description);
        let window = r.signals.iter().find(|s| s.signal_id == "nba-h2h.current.window.v1").unwrap();
        assert!(!window.description.contains("BTC"), "{}", window.description);
        let btc_price = r.signals.iter().find(|s| s.signal_id == "btc-2m.current.price.v1");
        assert!(btc_price.is_none(), "this registry is NBA-only");
        let pbp = r.signals.iter().find(|s| s.signal_id == NBA_PBP_SIGNAL).unwrap();
        assert_eq!(pbp.fields.last().unwrap().name.as_str(), "clock_remaining", "published last");
        let ty = |n: &str| pbp.fields.iter().find(|f| f.name.as_str() == n).map(|f| f.field_type.clone());
        assert!(matches!(ty("team"), Some(FieldType::Enum { variants }) if variants.len() == TEAMS.len()));
        assert_eq!(ty("clock_remaining"), Some(FieldType::Decimal));
        assert_eq!(ty("quarter"), Some(FieldType::U8));
        // The player travels as a name, so a document compares it to a string literal.
        assert_eq!(ty("player"), Some(FieldType::String));
        let players = &pbp.metadata.as_ref().unwrap()["players"];
        assert_eq!(players.as_object().unwrap().len(), ROSTERS.len());
        for p in ROSTERS {
            assert_eq!(players[p.id.to_string()]["name"], p.name);
            assert_eq!(players[p.id.to_string()]["team"], p.team);
        }
        assert_eq!(pbp.name.as_deref(), Some("Play-by-play"));
        assert!(pbp.fields.iter().find(|f| f.name.as_str() == "clock_remaining").unwrap().headline == Some(true));
        // The catalogue ingests each one whole.
        let store = crate::automation::signals::SignalStore::from_registry(&r);
        assert!(store.contains(NBA_WINPROB_SIGNAL));
    }

    #[test]
    fn every_definition_file_translates_and_types_resolve() {
        let dir = PathBuf::from(DEFINITIONS_IN_CHECKOUT).join("btc").join(SIGNALS_SUBDIR);
        let files = std::fs::read_dir(&dir).unwrap().filter(|e| e.as_ref().unwrap().path().extension().is_some_and(|x| x == "json")).count();
        assert!(files > 150, "expected the full definition set, got {files}");
        let r = build(&inputs(&["spot", "polymarket"])).expect("every definition translates");
        let from_defs: Vec<_> = r.signals.iter().filter(|s| s.metadata.as_ref().is_some_and(|m| m.get("lane").is_some())).collect();
        assert_eq!(from_defs.len(), files);
        // The RSI definition carries an enum and a u16 and a timestamp; all
        // must land as real compiler types, not be dropped.
        let rsi = r.signals.iter().find(|s| s.signal_id == "btc.coinbase.rsi.v1").expect("rsi listed");
        assert_eq!(rsi.version, "1");
        let ty = |n: &str| rsi.fields.iter().find(|f| f.name.as_str() == n).map(|f| f.field_type.clone());
        assert_eq!(ty("rsi_14"), Some(FieldType::Decimal));
        assert_eq!(ty("bar_count"), Some(FieldType::U16));
        assert_eq!(ty("window_close"), Some(FieldType::Timestamp));
        assert!(matches!(ty("regime"), Some(FieldType::Enum { .. })));
        let m = rsi.metadata.as_ref().unwrap();
        assert_eq!(m["lane"], "spot");
        assert_eq!(m["live"], true);
        assert_eq!(m["definition_version"], "1.0.0");
        let poly = r.signals.iter().find(|s| s.signal_id == "btc.polymarket.rsi.v1").unwrap();
        assert_eq!(poly.metadata.as_ref().unwrap()["live"], true);
        let kalshi = r.signals.iter().find(|s| s.signal_id.starts_with("btc.kalshi.")).unwrap();
        assert_eq!(kalshi.metadata.as_ref().unwrap()["live"], false);
    }

    #[test]
    fn every_signal_carries_a_scope_and_series_resolve_their_own() {
        let r = build(&inputs(&[])).unwrap();
        for def in &r.signals {
            assert!(Scope::of(def).is_some(), "{} has no scope", def.signal_id);
        }
        let ids = |slug: &str| -> Vec<String> { signals_for(&r, slug).map(|d| d.signal_id.clone()).collect() };
        let a = ids("btc-2m");
        let b = ids("btc-15m");
        // Own internals only.
        assert!(a.iter().any(|s| s == "btc-2m.current.price.v1"));
        assert!(!a.iter().any(|s| s == "btc-15m.current.price.v1"));
        assert!(b.iter().any(|s| s == "btc-15m.current.price.v1"));
        // The same BTC feed set for both series: everything but the internals.
        let feeds = |v: &[String]| -> Vec<String> { v.iter().filter(|s| !s.contains(".current.")).cloned().collect() };
        assert_eq!(feeds(&a), feeds(&b));
        assert!(feeds(&a).iter().any(|s| s == "btc.coinbase.rsi.v1"));
        assert!(feeds(&a).iter().any(|s| s == "pyth.btc_usd.spot.v1"));
        // The prototype's `scope_kind: global` stays BTC-scoped.
        let disp = r.signals.iter().find(|s| s.signal_id == "btc.multi.oracle_dispersion.v1").unwrap();
        assert_eq!(Scope::of(disp), Some(Scope::Underlying { underlying: "btc".into() }));
        assert_eq!(disp.metadata.as_ref().unwrap()["scope_kind"], "global");
        assert!(ids("nope").is_empty());
    }

    #[test]
    fn readable_names_ride_through() {
        let r = build(&inputs(&[])).unwrap();
        // The copy file wins over the hand-written and spec names; the spec
        // name survives in metadata for the compiler.
        let stats = r.signals.iter().find(|s| s.signal_id == "btc-2m.current.stats.v1").unwrap();
        assert_eq!(stats.name.as_deref(), Some("Round activity"));
        assert_eq!(stats.fields[0].label.as_deref(), Some("Volume"));
        assert_eq!(stats.fields[0].headline, Some(true));
        let rsi = r.signals.iter().find(|s| s.signal_id == "btc.coinbase.rsi.v1").unwrap();
        assert_eq!(rsi.name.as_deref(), Some("RSI"));
        assert_eq!(rsi.fields[0].label.as_deref(), Some("RSI"));
        assert!(rsi.fields[0].description.as_deref().is_some_and(|d| d.contains("seventy")));
        assert_eq!(rsi.metadata.as_ref().unwrap()["name"], "Coinbase BTC/USD one-minute Wilder RSI(14)");
        assert!(rsi.metadata.as_ref().unwrap()["spec_description"].as_str().is_some_and(|d| d.len() > 200));
        let window_close = rsi.fields.iter().find(|f| f.name.as_str() == "window_close").unwrap();
        assert_eq!(window_close.headline, Some(false));
        assert_eq!(window_close.label.as_deref(), Some("Window Close"));
        for def in &r.signals {
            assert!(def.name.is_some(), "{} has no name", def.signal_id);
            for f in &def.fields {
                assert!(f.label.as_deref().is_some_and(|l| !l.is_empty()), "{}.{} has no label", def.signal_id, f.name.as_str());
            }
        }
        assert_eq!(field_label("volatility_bps_60s"), "Volatility (bps) 60s");
        assert_eq!(field_label("input_age_ms"), "Input Age (ms)");
        assert_eq!(field_label("regime"), "Regime");
    }

    #[test]
    fn explicit_scope_in_a_definition_overrides_the_directory() {
        let live: &[&str] = &[];
        let def = json!({
            "signal_id": "x.y", "version": "2.1.0", "fields": [{"name": "v", "type": "decimal"}],
            "scope": {"kind": "global"}
        });
        let out = translate_definition(&def, "eth", live).unwrap();
        assert_eq!(out["signal_id"], "x.y.v2");
        assert_eq!(out["metadata"]["scope"], json!({"kind": "global"}));
        let plain = json!({"signal_id": "x.z", "version": "1.0.0", "fields": []});
        let out = translate_definition(&plain, "eth", live).unwrap();
        assert_eq!(out["metadata"]["scope"], json!({"kind": "underlying", "underlying": "eth"}));
        let bad = json!({"signal_id": "x.w", "version": "1.0.0", "fields": [], "scope": {"kind": "planet"}});
        assert!(translate_definition(&bad, "eth", live).is_err());
    }

    #[test]
    fn price_source_is_the_one_external_signal() {
        let r = build(&Inputs { price_source: PriceSource::Coinbase, live_lanes: &[], definitions_dir: Some(PathBuf::from(DEFINITIONS_IN_CHECKOUT)), copy_dir: None }).unwrap();
        let ext: Vec<_> = r.signals.iter().filter(|s| s.location == SignalLocation::External).collect();
        assert_eq!(ext.len(), 1);
        assert_eq!(ext[0].signal_id, "coinbase.btc_usd.spot.v1");
    }

    #[test]
    fn round_trips_through_json() {
        let r = build(&inputs(&["spot"])).unwrap();
        let text = serde_json::to_string_pretty(&r).unwrap();
        let back: Registry = serde_json::from_str(&text).unwrap();
        assert_eq!(back, r);
    }
}
