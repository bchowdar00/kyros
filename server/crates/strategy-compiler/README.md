# strategy-compiler

Vendored copy of the Tiny strategy compiler prototype.

- Source: https://github.com/tiny-xyz/compiler-prototype
- Commit: `2d4918f4a99038bb2cdbfa7836a9eb98cb0d17fa` (branch `main`)
- Vendored: 2026-09-04

The compiler turns an LLM-authored strategy IR document (JSON) into two artifacts:

- a **program** (`program.json`, typed as `strategy_compiler::Program`) — the closed,
  interpretable form the exchange runtime executes: subscriptions, trigger rows with
  fire/rearm modes, legs with arm masks, block plans whose guards and order
  expressions are a small op-tree, the intent index, state slots and tables;
- a **manifest** (`manifest.json`) — the pinned wire-schema registration shape, kept
  for provenance and the venue-facing seam.

Upstream emitted a Python module the prototype harness exec'd; that backend and
its bundle/tar format are gone. The runtime deserializes the program straight into
the Rust types (the exchange crate depends on this one), so there is no generated
code, no sandbox, and no second contract to keep in sync. The exchange crate
depends on this crate; `crates/exchange/src/api/strategy/parse.rs` is where the LLM
output will be handed to `strategy_compiler::compile()` (not wired yet).

## Layout

One library crate, no binary, no I/O. `strategy_compiler::compile()` in
`src/lib.rs` is the single public entry point.

```
src/ir/         IR document, registry, envelope, and manifest types. Data only. JSON Schema derives.
src/parse/      lexer, parser, AST for the expression/statement strings.
src/validate/   the passes, diagnostics, lints → ValidStrategy.
src/lower/      ValidStrategy → Lowered (substituted ASTs; exprs.rs lowers them to the op-tree).
src/program/    the Program types: the runtime's executable artifact (serde + JSON Schema).
src/emit/       program and manifest projections, capability gate, build.
registry/       tiny.json — the Tiny venue's registry (the 1-minute BTC up/down family).
tests/          integration tests + insta snapshots.
tests/corpus/   eight hand-written strategy fixtures plus registry/registry.json (test data only).
tests/fixtures/tiny/  strategies written against registry/tiny.json.
tests/schemas/  wire schemas vendored from tiny-xyz/tiny-schemas at the commit
                recorded in `emit::SCHEMA_PIN` (was a git submodule upstream).
```

Tests locate `tests/corpus/` and `tests/schemas/` relative to `CARGO_MANIFEST_DIR`.

## Left out of the vendored copy

- `agent/` — the Python LLM front end (Anthropic SDK + FastAPI). Separate product.
- `runs/`, `out/`, `.env`, `target/` — per-run scratch output and secrets.
- The `schemas` git submodule; its files are copied in instead.
- The `tinyc` CLI crate. The server calls the library directly; the CLI's
  `check`/`build` were thin file-IO wrappers around `emit::build`.
- The Python emitter (`emit/python/`) and `bundle.json`. Replaced by the program.

## The program format

`tiny-program/2`. One JSON object, byte-deterministic for a given document and
compiler version. Decimals are canonical strings everywhere (the runtime picks the
arithmetic type); durations are integer milliseconds. Expressions are an op-tree
tagged on `op`: literals (`decimal`, `bool`, `duration_ms`, `variant`, `str`),
resolved reads (`signal` by id, `event`, `state`, `state_map`, `envelope` with its
value baked in, `platform` for the closed `PlatformRead` set, `param` by name,
`leg_fill` for per-leg fill context, `entry` under a reducer), operators spelled as
written (`+ - * / < <= > >= == != and or neg not`), `if`, `call` over the math
builtins, `reduce` over a window slot, `lookup`, and `triggered`. Named expressions
are inlined at compile time. Trigger expr leaves are the same node shape, lowered from
the same grammar, so the runtime needs one evaluator.

The capability gate currently refuses nothing.

## IR v0.3 (what changed from v0.2)

- `dsl_version` is exactly `"0.3"`; no compatibility shim.
- State slots: `decimal`, `bool` and `string` primitives; `window` slots (bounded
  numeric ring buffers) with `push` and the reducers `count`, `sum`, `max_of`,
  `min_of`, `oldest`, `newest`, `count_where(w, entry.value ...)`.
- `params`: deploy-time knobs readable as `param.<name>` in bodies and as
  `{"param": name}` leaf values; string fields compare against `{"str": ...}`.
- Trigger expr leaves are full expressions over `signal.<nick>.<field>`, `param.*`
  and `envelope.*` reads (cross-signal), with no `signal` key; the validator keeps
  them pure (`trigger-expr-impure`).
- Platform reads: `position.size`, `position.avg_cost`, `realized_pnl`,
  `unrealized_pnl`, `cash`, `open_orders`, `window.remaining_ms`, `last_fill.price`.
- Fill context: `legs.<leg>.{filled_qty,fill_price,last_fill_price,fills}` and
  `legs.<leg>.tag.<tag>.<field>`.
- Timers: `after` and `interval` beside `window_clock`; `rearm.cooldown`.
- Orders: `post_only`, `expire_after`, `reduce_only` (gated on the instrument's
  registry `capabilities`), and the `target_position` item.

## Commands (from `server/`)

```bash
cargo build -p strategy-compiler
cargo test  -p strategy-compiler
cargo test  --workspace            # with the exchange
```

Upstream design notes live in the source repo's `CLAUDE.md`; the invariants that
matter most: the IR describes and never computes, everything is bounded at
compile time, output is byte-deterministic, and nothing downstream of the
validator accepts an unvalidated document.
