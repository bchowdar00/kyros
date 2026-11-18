# BTC candidate signal definitions — NONE OUTSTANDING

This directory holds **unadopted candidate drafts**: complete, schema-valid V1
signal definitions that nothing consumes at runtime. No processor embeds them,
the live runner does not emit them, and `signals validate` does not count them
(it scans only each domain's `sources/` and `signals/` directories).

**As of 2026-09-02 it is empty.** All thirteen T1 candidate drafts have been
adopted and moved into `../signals/`; the published surface is 25 source
definitions and 62 signal definitions.

A candidate enters the surface only through an **owner-approved change** that, as
one vertical edit, moves the file from this directory into
`definitions/btc/signals/`, wires the registry export, and lands the processor
and runtime integration under the expansion gate. Placing a draft here is not
adoption.

## What was adopted

- **2026-09-01, seven prediction identities** (ADR 0023):
  `btc.{polymarket,kalshi}.pair_cost_state`,
  `btc.{polymarket,kalshi}.venue_repricing_lag`,
  `btc.{polymarket,kalshi}.tie_rule_context`, and
  `btc.external.early_move_fade_context`.
- **2026-09-02, two Polymarket identities** (ADR 0023 amendment):
  `btc.polymarket.settlement_anchor_state` and
  `btc.polymarket.oracle_divergence`, the second re-homed from
  `btc.external.oracle_divergence` because it is computed per bound Polymarket
  market and published under that market's scope.
- **2026-09-02, the four spot-microstructure measurements, as PER-VENUE
  identities** (ADR 0023 amendment; ADR 0024): `effective_spread`,
  `kyle_lambda_drift`, `sweep_detection` and `book_resilience`, each published
  once under `btc.coinbase.*` and once under `btc.kraken.*` — eight identities
  from four drafts. Each measurement is a strictly same-venue formula (a trade
  is priced against its own venue's prevailing midpoint, a sweep is one venue's
  batch, a depletion compares one venue's consecutive top-of-book updates), so
  it is published per venue and is ready when that venue's own book-and-trade
  join is fresh.

Their computations stay in
`crates/btc-signals/src/candidate_prediction_signals.rs`,
`crates/btc-signals/src/candidate_settlement_signals.rs` and
`crates/btc-signals/src/candidate_spot_signals.rs`, which the processors now
call; the module names record where each computation was specified, not its
adoption state.

## What remains deferred

- **Blended `btc.external.*` variants of the four microstructure
  measurements.** Not adopted now and not drafted here: a cross-venue form
  would first have to rule which venue's book prices another venue's trade, and
  how two venues' independent depletions combine. Deferred pending evidence
  (ADR 0023 amendment, 2026-09-02).
- **The Kalshi settlement-anchor variants** of
  `settlement_anchor_state`/`oracle_divergence`. Kalshi resolves on the CF
  Benchmarks BRTI, which remains rights-gated, so they are not built until BRTI
  access is cleared and the schedule is represented in-repo. That deferral is
  why the two prediction venues publish different-sized identity sets.
