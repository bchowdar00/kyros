# Tiny — one-minute BTC prediction market demo

A live, self-contained prediction-market venue that runs on real Bitcoin prices. Every 60 seconds a new market opens: the strike is BTC's price at open, and at close the real price decides whether **YES (BTC went up)** or **NO (BTC went down)** pays out $1 per contract. A Rust matching engine supplies the order book; a React frontend lets a human trade against it with a starting balance of $1,000.

There is no database, no auth, and no on-chain component. Everything lives in the memory of a single server process. That is deliberate: this is a demo of the product experience and the market microstructure, not a production exchange.

---

## Table of contents

1. [Repository layout](#repository-layout)
2. [How the market works](#how-the-market-works)
3. [Server architecture](#server-architecture)
4. [API reference](#api-reference)
5. [Frontend architecture](#frontend-architecture)
6. [Local setup](#local-setup)
7. [Testing and linting](#testing-and-linting)
8. [Configuration reference](#configuration-reference)
9. [Production deployment](#production-deployment)
10. [Known gaps and gotchas](#known-gaps-and-gotchas)

---

## Repository layout

```
.
├── dev.sh                  # runs server + frontend together (Ctrl+C stops both)
├── CLAUDE.md               # agent guidance (partly stale; see "Known gaps")
├── server/                 # Rust workspace
│   ├── Cargo.toml          # [workspace] members = crates/*, default = exchange
│   ├── rust-toolchain.toml # pins Rust 1.93.0
│   ├── railway.toml        # Railway/Nixpacks deploy config
│   ├── crates/strategy-compiler/  # library: IR → validate → lower → emit (+ tests/corpus, tests/schemas)
│   └── crates/exchange/    # the server binary (axum + tokio)
│       └── src/
│           ├── main.rs         # wiring: engine, feed, scheduler, router
│           ├── lib.rs          # module re-exports so tests/bins share code
│           ├── types.rs        # Tenths, Qty, Position, Address, Side newtypes
│           ├── ticks.rs        # the fixed 153-tick price grid
│           ├── scheduler.rs    # 60s market lifecycle loop
│           ├── engine/         # matching engine, ledger, settlement
│           │   ├── mod.rs      # Engine: place/cancel/flip/resolve/settle
│           │   ├── book.rs     # canonical YES order book
│           │   ├── accounts.rs # in-memory account store
│           │   ├── model.rs    # Order, Fill, FillKind, Account, TradeSource
│           │   └── strategies.rs # strategy data model + validation
│           ├── pricing/
│           │   ├── feed.rs     # PriceFeed store
│           │   └── fair_value.rs # digital-call fair value (Φ(d))
│           ├── automation/     # strategy executor (runs deployed strategies)
│           └── api/            # HTTP + WebSocket handlers
│               ├── mod.rs      # AppState, ApiError, broadcast wire types
│               ├── handlers.rs # accounts, orders, book, trades, history
│               ├── ws.rs       # /ws/price, /ws/book, /ws/trades
│               ├── positions.rs # /ws/positions, close, flip
│               ├── accounts.rs  # AccountRow: shared main/strategy row
│               ├── accounts_ws.rs # /ws/accounts
│               ├── trades.rs   # trade batcher task
│               ├── user_trades.rs, stats.rs
│               └── strategy/   # parse (OpenAI) + store (CRUD)
└── frontend/               # Vite + React 18 + TypeScript + Tailwind
    ├── package.json        # pnpm project (pnpm-lock.yaml)
    ├── .env.example        # VITE_API_BASE_URL / VITE_WS_BASE_URL
    ├── vite.config.ts      # no dev proxy — see Local setup
    ├── CLAUDE.md           # frontend design "constitution" (Figma rules)
    ├── docs/               # Figma recon + chart deep-dive notes
    ├── public/assets/      # Aeonik fonts, icons, favicon
    └── src/
        ├── main.tsx, App.tsx      # router + identity gate + position stream
        ├── routes/                # market.tsx (/), leaderboard.tsx
        ├── api/                   # typed HTTP/WS client layer (+ tests)
        ├── lib/store/             # zustand stores + singleton WS stores
        ├── lib/hooks/             # polling hooks (stats, holders, trades…)
        ├── lib/types/, lib/utils/, lib/mock/
        ├── components/app/        # AppChrome (header), NameGate
        ├── components/market/     # chart, order book, trade panel, etc.
        └── styles/                # globals.css tokens, automate.css
```

---

## How the market works

### Market series

The server runs several market *series* at once, each a family of back-to-back rounds with its own length. The table lives in `crates/exchange/src/series.rs`:

| Series | Slug | Round length |
|---|---|---|
| default | `btc-2m` | 120 s |
| long | `btc-15m` | 900 s |

Every series has its own order book, open interest, positions and scheduler. They all share **one ledger of balances**: a human's main account has a single USDC balance and can hold a position in every series at once. Every bot and strategy sub-account is bound to exactly one *home series* and the engine rejects its orders anywhere else (`wrong_series`). Adding a series is one line in the table.

Every series-scoped request takes an optional `series` slug (query parameter or JSON field) that defaults to `btc-2m`, so a client that knows nothing about series keeps working. `GET /api/series` lists the series and where each one's current round stands.

### The round loop

`server/src/scheduler.rs` runs one endless loop per series, with exactly one round open per series at a time:

1. Wait for the first BTC tick from the price feed.
2. Open round #1 with that price as the strike. Reset that series' price-feed window so chart data belongs to this round.
3. Every round length: read the latest BTC price, call `Exchange::resolve_and_open_next` for the series, and reset its window again.

Resolution is strict: YES wins only if close price is **strictly greater** than the strike. Equal or lower resolves NO.

There is no separate "lock" phase. Resting orders are cancelled inside resolution, and the next market opens in the same engine call, so from the outside the transition is atomic.

### Money and prices

- **All money is integer tenths of a cent** (`Tenths`). `$1 = Tenths(1000)`, `1¢ = Tenths(10)`. There is no floating point anywhere in the money path. Accounts start with `Tenths(1_000_000)` = $1,000.
- **One contract pays $1** at settlement (`PAYOUT_PER_CONTRACT = 1000`).
- **Prices live on a fixed 153-tick grid** (`server/src/ticks.rs`), expressed in tenths of a cent:
  - 0.1¢ to 3.0¢ in 0.1¢ steps (30 ticks)
  - 4¢ to 96¢ in 1¢ steps (93 ticks)
  - 97.0¢ to 99.9¢ in 0.1¢ steps (30 ticks)

  The grid is symmetric around 50¢, so the NO price (`1000 − yes`) is always a valid tick too. Orders at off-grid prices are rejected with `bad_price`.

### One canonical YES book

There is a single order book for the YES contract. "Buy NO at p" is encoded as "sell YES at 1000 − p". Positions are signed share counts: `+n` = long n YES, `−n` = long n NO.

`server/src/engine/book.rs` stores each of the 153 price levels as a FIFO linked list of orders, addressed directly by tick index. Insert and cancel are O(1); price-time priority falls out of the list order.

### Collateral and the four clearing cases

The buyer of YES at price `p` posts `p` per contract; the seller posts `1000 − p`. Together that is exactly $1, which is why **total locked collateral always equals open interest × $1**. Each fill is classified by both parties' prior positions (`FillKind`):

| Case | Buyer was | Seller was | Effect on open interest |
|---|---|---|---|
| `Open` | flat or long YES | flat or long NO | increases |
| `TransferYes` | flat/long YES | long YES | unchanged |
| `TransferNo` | long NO | flat/long NO | unchanged |
| `Close` | long NO | long YES | decreases, collateral released |

Every fill carries its kind on the wire so clients can render it.

### Order types

- **Limit** (`place_limit`): crosses against any opposing liquidity at the limit price or better, then rests the remainder. Cash is locked per resting unit and refunded on cancel.
- **Market** (`place_market`): immediate-or-cancel. If the account cannot afford the full size, the engine shrinks the order to the largest affordable quantity rather than rejecting it. Errors with `no_liquidity` if the opposite side is empty.
- **Close** (`POST /api/positions/close`): market order on the closing side sized to the current position, read under the engine lock so a stale UI cannot race.
- **Flip** (`POST /api/positions/flip`): closes to flat, then opens the opposite side using the closed position's marked notional as the budget. Both legs are market orders.

### Settlement

`Engine::settle_all` runs once per resolution:

- **Registered users** are paid $1 per winning contract. Balance is account-scoped and shared across series, so winnings roll into the next round of any series. Their leaderboard standing gains the round's realized PnL (settlement included) of the user plus their strategy sub-accounts, if any of them placed an order that round.
- **Bot accounts homed in this series** are reset to their seeded bankroll so they never go broke. Bots of other series are mid-round and untouched.
- This series' positions, entry costs and per-round PnL are zeroed; its open interest and per-round stats reset. Reservations for resting orders in *other* series are left alone.

The engine keeps the last 60 resolved markets and the last 1,000 fills in rolling buffers.

### Strategies (the "Automate" feature)

Every deployed strategy runs on its own **sub-account**: an ordinary engine account with a random address, funded by a cash transfer from the user's main account (minimum $10) and linked back to it. Each strategy therefore has an isolated position, collateral and PnL, and one user can run many strategies at once. Manual trading stays on the parent account. Public surfaces roll children up to the parent: the activity feed and holders table show the parent's name and avatar, the leaderboard ranks the parent by the combined cash of parent + children, and the trade log lists child orders under the parent with the strategy's name. Deleting a strategy cancels its orders, market-closes its position and sweeps its cash back; if there is no liquidity to close, the child is left flat-at-resolution and swept then.

A user can describe a strategy in plain English. The server sends it to OpenAI (`gpt-5.4-mini`, `api/strategy/parse.rs`) with a long system prompt that returns a tree of **blocks**: each block is one action (open, close, flip, scale in/out, hold) on YES or NO, gated by a trigger (price level, price cross, immediate, time window, PnL state). Children arm after their parent fires, giving branching logic.

Deploying stores the blocks on the strategy's sub-account (one strategy per sub-account, many sub-accounts per user). The `automation/` module runs one tokio task per active strategy on a ~150 ms jittered tick, walking a `Pending → Armed → WaitingFill → Done` state machine per block and translating fired blocks into engine calls. State resets every round, so strategies re-arm on each new market. A per-block cap of 120 fires per round prevents runaway loops.

---

## Server architecture

### Concurrency model as actually implemented

`Exchange` (`engine/mod.rs`) is a plain synchronous struct with no internal locking: one `Ledger` (cash, identity, sub-account tree, leaderboard) plus one `MarketEngine` per series (book, rounds, positions, per-round stats). It is shared as `Arc<Mutex<Exchange>>`. Every actor takes the lock, does microsecond-scale work on one series' engine, and releases it:

- HTTP handlers and WebSocket snapshot builders
- one scheduler per series (once per round)
- each strategy-executor task (on the sub-account's home series)
- one trades batcher per series (every 100 ms)

Money never moves without the ledger: a `MarketEngine` takes the `Ledger` as a parameter for every trade and performs single atomic reservations/releases on it, so two series drawing on the same balance cannot overdraw it. `Engine` is a permanent single-series facade over an `Exchange`, used by the unit tests.

Fan-out to clients goes through tokio `broadcast` channels, one pair per series (`SeriesChannels`):

| Channel | Producer | Consumers |
|---|---|---|
| `PriceFeed` broadcast | price feed task | `/ws/price` (resets filtered to the subscriber's series) |
| `book_tx` (per series) | anyone who mutates that series' book, under the lock | `/ws/book`, `/ws/positions`, `/ws/accounts` |
| `trades_tx` (per series) | that series' trades batcher | `/ws/trades`, `/ws/positions`, `/ws/accounts` |

The positions and accounts sockets wake on every series' channels because shared cash and the per-series rows can change from any of them.

Book snapshots are broadcast **while still holding the engine lock**, so subscribers see mutations in exactly the order they happened. All three WebSocket streams are snapshot-first: the first frame after connect is a full snapshot, and a subscriber that lags gets a fresh snapshot instead of being dropped.

Note that the root `CLAUDE.md` describes a single-writer task fed by `mpsc` commands. That was the original design intent; the code uses the mutex approach above. The only `mpsc` in the system is the command channel from strategy HTTP handlers to the executor manager.

### Startup sequence (`main.rs`)

1. Load `.env` (via `dotenvy`), init tracing.
2. Create the exchange (one engine per series) with a $1,000 starting balance.
3. Generate the venue registry (`registry.rs`): every series from the series table and every signal from `definitions/<underlying>/signals/`, each scoped to its series, its underlying or globally, in the strategy compiler's `Registry` format, written to `REGISTRY_PATH` (default `server/registry.json`, gitignored) and served at `GET /api/registry`. Nothing in it is hand-maintained: a new series row or a new definition file appears on the next start. Then start the signals platform (the automation runtime's matching engine, catalogued from that registry) and one round-statistics publisher per series (`automation/stats_signal.rs`), which republishes `<slug>.current.stats.v1` (volume, open interest, collateral, trades, traders, largest trade) on every change, at round open, and on a 5 s heartbeat. Then start the BTC signal producers (`signals.rs`: the spot, Polymarket, and, with credentials or opt-in, Kalshi, Binance, Bybit, oracle and cross lanes from the signals prototype), each on its own thread and runtime, writing file generations under `signals-data/<lane>/`. Then spawn the price feed, then per series a scheduler and a trades batcher; and the executor manager.
4. Bind `0.0.0.0:$PORT` (default 8080) with permissive CORS.

### Price feed

The BTC/USD feed connector writes into `pricing/feed.rs`'s `PriceFeed`, which keeps the latest price in an atomic, one current-round tick window per series (2,000 ticks per minute of round length, cleared when that series rolls), a global 30-second velocity buffer that no reset clears, and a broadcast channel of `tick` / `market_reset` messages (resets are tagged with their series). The server cannot open its first market until the first tick arrives, so the process needs outbound internet access.

---

## API reference

All money fields are in tenths of a cent. All prices are tenths-of-a-cent tick values. Errors share one shape:

```json
{ "error": "insufficient_balance", "message": "insufficient balance", "detail": { "required": 5000, "available": 1200 } }
```

Error codes: `bad_request`, `bad_address`, `bad_price`, `unknown_account` (404), `unknown_order` (404), `not_owner` (403), `zero_quantity`, `insufficient_balance`, `no_liquidity` (422), `strategy_not_found` (404), `invalid_strategy`, `allocation_too_small` (detail `{min}`), `too_many_strategies`, `not_sub_account`, `wrong_series` (detail `{home, requested}`; a bot or strategy account ordered outside its home series), `unknown_series`, plus `config_error` / `upstream_error` / `parse_error` from the strategy parser. A malformed `series` slug in a typed JSON body or query is rejected by the deserializer (422).

Every route marked *series* below takes an optional `series` slug (`btc-2m` | `btc-15m`) and defaults to `btc-2m`.

### HTTP

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/health` | `200` with a report when the index price is arriving, every series has a round open (or closed one under a minute ago) and has templates; otherwise `503` with the reasons |
| `GET` | `/live` | returns `ok` while the process is serving; the probe a host should use |
| `POST` | `/api/accounts` | `{username}` → creates an account with a random address, color, emoji and $1,000 |
| `GET` | `/api/accounts/{address}` | full account record (balance, locked, `home_series` when bound, strategy) plus the default series' position fields |
| `GET` | `/api/accounts/{address}/trades?strategy=main\|{id}` | this user's and their strategies' orders across every series — current round plus the last 10 resolved — with `series`, fill progress, VWAP, per-order realized PnL and a `settled` flag; `strategy` narrows to the main account or one strategy |
| `GET` | `/api/series` | `[{series, duration_secs, current_market_id, opens_at_ms, closes_at_ms}]` |
| `POST` | `/api/orders` *series* | `{type:"limit", address, side, price, qty, series?}` or `{type:"market", address, side, qty, series?}` → `PlaceOutcome` |
| `DELETE` | `/api/orders/{id}?address=…&series=…` | cancel a resting order; the named series is tried first, the others probed on a miss |
| `POST` | `/api/positions/close` *series* | `{address, series?}` → market-close to flat |
| `POST` | `/api/positions/flip` *series* | `{address, series?}` → close and reopen the other side |
| `GET` | `/api/book?depth=20` *series* | bids/asks levels (max depth 153) |
| `GET` | `/api/trades?limit=50` *series* | recent raw fills (max 1000) |
| `GET` | `/api/market/stats` *series* | `series`, `duration_secs`, volume, open interest, locked, trade count, unique traders, largest trade |
| `GET` | `/api/markets/history?limit=15` *series* | resolved rounds newest-first (max 60), each tagged with `series` and `duration_secs` |
| `POST` | `/api/strategy` | `{input, current?}` → OpenAI-parsed `{text, blocks}`; needs `OPENAI_API_KEY` |
| `GET` | `/api/accounts/{address}/strategies` | `{strategies: [StrategySummary]}` — every live strategy, creation order |
| `POST` | `/api/accounts/{address}/strategies` *series* | `{allocation_tenths, name?, text, blocks, series?}` → creates a funded sub-account homed in `series`, deploys, returns `StrategyDetail` (`strategy.series` on the wire); time windows are validated against that series' round length |
| `GET` | `/api/accounts/{address}/strategies/{id}` | summary + stored plan; `id` is the sub-account address |
| `PATCH` | `/api/accounts/{address}/strategies/{id}` | partial update (name, status); pause/resume drives the executor |
| `DELETE` | `/api/accounts/{address}/strategies/{id}` | stop executor, cancel orders, flatten, sweep cash to parent → `{swept_tenths, residual_position, removed, …}` |
| `POST` | `/api/accounts/{address}/strategies/{id}/fund` | `{amount_tenths}` signed: parent → strategy when positive |

`side` is `"bid"` (buy YES) or `"ask"` (sell YES, i.e. buy NO).

### WebSocket

| Path | First frame | Then |
|---|---|---|
| `/ws/price?series=…` | `{type:"snapshot", series, duration_secs, market_id, strike, opens_at_ms, closes_at_ms, ticks:[{ts_ms, price}]}` (thinned to ≤3,000 ticks) | `{type:"tick", ts_ms, price}` (shared by every series) and `{type:"market_reset", series, market_id, strike, opens_at_ms, closes_at_ms, duration_secs}` for the subscribed series only |
| `/ws/book?series=…` | `{type:"snapshot", series, bids:[{price, qty}], asks:[…]}` (depth 20) | the same full snapshot on every book change |
| `/ws/trades?series=…\|all` | `{type:"snapshot", trades:[TradeRow…]}` (30 rows) | `{type:"batch", trades:[…]}` every 100 ms when there are fills, max 20 rows per batch; `series=all` merges every series' tape (rows carry `series`) |
| `/ws/positions?address=…&series=…` | `{type:"position_snapshot", series, …, series_positions:[SeriesPositionRow…]}` — flat fields for the requested series, one row per series | a new snapshot whenever any series' activity changes the caller's cash, positions, marks or flip quotes (the parent's *manual* positions only) |
| `/ws/accounts?address=…` | `{type:"accounts_snapshot", main:AccountRow, strategies:[AccountRow…], totals:{equity_tenths, net_contributed_tenths, lifetime_pnl_tenths, market_pnl_tenths, trades_this_market, last_market_pnl_tenths, unrealized_total_tenths, market_pnl_total_tenths}}` — every `AccountRow` carries `series_positions`, `unrealized_total_tenths`, `market_pnl_total_tenths`, and `equity_tenths` spans every series | a new snapshot whenever the caller's main account or any strategy sub-account changes in any series |
| `/ws` | echo | debugging only |

A `TradeRow` is one taker order collapsed across all the makers it hit, with `series`, VWAP price, `side` (`yes`/`no` from the taker's view), and both parties' identity (address, username, color, emoji). The position snapshot's flat fields carry size, signed position, average entry, cost basis, mark, unrealized and realized PnL, balance, locked, and a precomputed flip quote for the requested series; each `SeriesPositionRow` repeats them per series and adds `market_pnl_tenths`, `trades_this_market`, `last_market` and `market_id`.

---

## Frontend architecture

Vite + React 18 + TypeScript + Tailwind + zustand. No UI library, no react-query; both are banned by `frontend/CLAUDE.md`. Path alias `@/` maps to `src/`.

### Entry and routes

- `main.tsx` mounts `App` inside `BrowserRouter`.
- `App.tsx` applies the theme attribute, attaches the shared position stream for the logged-in address, and wraps two routes in `NameGate`:
  - `/` → `routes/market.tsx`, the trading screen
  - `/leaderboard` → `routes/leaderboard.tsx`

`NameGate` (`components/app/name-gate.tsx`) is the whole "auth" system. It reads an address from `localStorage`, verifies it against `GET /api/accounts/{address}`, and if that fails (for example after a server restart) clears it and shows a name prompt that calls `POST /api/accounts`.

### API layer (`src/api/`)

A typed client the rest of the app imports from `@/api`:

- `config.ts` resolves base URLs from `VITE_API_BASE_URL` / `VITE_WS_BASE_URL` and holds route constants and tunables. Empty env means relative HTTP paths and a WS URL derived from `window.location`. A `ws://` base is upgraded to `wss://` automatically when the page is served over HTTPS.
- `http.ts` is a fetch wrapper with timeouts and a single `ApiError` type keyed by the server's error code.
- `ws.ts` is a reconnecting subscription factory with exponential backoff and jitter.
- `market.ts`, `orders.ts`, `account.ts`, `strategy.ts`, etc. are thin domain modules. `money.ts` holds tick and Tenths helpers.
- `__tests__/` covers this layer with vitest, msw, and a websocket mock.

### Stores (`src/lib/store/`)

Live data comes in through ref-counted singleton stores that open one socket on first subscriber and close it on last:

| Store | Source | Notes |
|---|---|---|
| `series-store` | `/api/series` (1 s while the market page is mounted) | the active series (mirrored to `?series=`) and every series' current round |
| `server-price-feed-store` | `/ws/price?series=…`, one store per series | tick buffer read directly by the chart's animation loop; React state throttled to ~4 Hz; the round window (`windowStart` / `windowEnd` / `durationMs`) comes from the server's snapshot |
| `order-book-store` | `/ws/book?series=…`, one store per series | `useOrderBook` (throttled ~5 Hz) and `useMarkPrice` (instant) |
| `activity-store` | `/ws/trades?series=…`, one store per series | bounded newest-first ring of 80 rows |
| `position-store` | `/ws/positions` | attached once at the root for the current address; `useMyPositionFor(series)` selects one series' row |
| `accounts-store` | `/ws/accounts` | main account, strategies and cross-series totals |
| `identity-store` | `localStorage` + `/api/accounts` | address, username, color, emoji |
| `market-ui-store` | local | trade ticket state, theme, panel toggles |
| `notifications-store` | local | toasts for fills and deployments |

Every per-series store is a factory behind a registry keyed by series: a hook called without a series reads the active one, and an instance disposes its socket when its last listener leaves. The server is the single price source; there is no browser-side Coinbase socket and no wall-clock window model. The window countdown, header price and delta, strike, chart line, order book, mark price, activity feed, position panel, stats, holders, trade history and leaderboard are all server-backed.

### Hooks (`src/lib/hooks/`)

Polling hooks over REST endpoints: `useMarketStats` (200 ms), `useHolders` (1 s), `useUserTrades` (1 s), `useRecentMarkets` and `useLeaderboard` (4 s). The series-scoped ones take an optional `series` and default to the active one. `useAccountSnapshot(series)` derives cash, locked, cross-series portfolio value and the signed position in one series from the position and accounts streams. `useTradeFillNotifier` watches every series' tape (`?series=all`) and turns fills into toasts. `useWatchingCount` is a cosmetic random walk; nothing tracks viewers server-side.

### Components

`components/market/` holds the trading screen: `chart-card` and `price-chart` / `price-chart-canvas` (hand-rolled canvas chart with a smoothed live line, strike line and countdown), `right-rail` with `trade-panel`, `limit-order-ticket`, `size-slider`, `order-book`, `activity-table`, `holders-table`, `trade-history`, `my-position`, `automate-modal` and `strategy-tree` for strategies, `related-markets` (static ETH/SOL/HYPE cards), and the toasters. `components/app/app-chrome.tsx` is the header with portfolio, cash and identity.

### Design system

All visual values are CSS custom properties in `src/styles/globals.css`, surfaced through `tailwind.config.ts`. Aeonik is bundled in `public/assets/fonts`. `frontend/CLAUDE.md` documents the token names, the Figma source of truth, and the rule that components never inline hex, pixel or opacity values. `frontend/docs/` holds the Figma recon reports and a deep-dive on the chart.

---

## Local setup

### Prerequisites

- Rust via `rustup`. The toolchain file pins 1.93.0 and rustup will install it on first build.
- Node 20+ and `pnpm` (the lockfile is `pnpm-lock.yaml`; `npm` works for running scripts once dependencies are installed).
- Outbound internet access for the price feed websocket. Without it the server starts but never opens a market.
- Optional: an OpenAI API key for the Automate modal. Copy `server/.env.example` to `server/.env`.

### 1. Server

```bash
cd server
cargo run            # dev build of crates/exchange, listens on :8080
RUST_LOG=info cargo run   # recommended: shows market open/resolve logs
```

`cargo run` from `server/` builds the `exchange` crate (the workspace default member). The binary runs from any directory.

Optional `server/.env`:

```
OPENAI_API_KEY=sk-...
PORT=8080
```

Confirm it is up:

```bash
curl localhost:8080/live            # ok
curl localhost:8080/health          # the report; 503 until every series has a round
curl localhost:8080/api/market/stats
```

### 2. Frontend

The Vite config has **no dev proxy**, so the frontend must be told where the server is:

```bash
cd frontend
cp .env.example .env.local
```

Edit `.env.local`:

```
VITE_API_BASE_URL=http://localhost:8080
VITE_WS_BASE_URL=ws://localhost:8080
```

Then:

```bash
pnpm install
pnpm dev             # http://localhost:5173
```

Enter any name at the gate and you have a $1,000 account. Your address is stored in `localStorage`; restarting the server invalidates it and the gate will prompt again.

### 3. Both at once

```bash
./dev.sh
```

Starts `cargo run` and `npm run dev` with prefixed logs and tears both down on Ctrl+C. It still needs `.env.local` in `frontend/`; the script's comment about a proxy is out of date.

---

## Testing and linting

Server:

```bash
cd server
cargo test --workspace   # ~450 exchange tests + the strategy-compiler suite (insta snapshots)
cargo clippy --workspace --all-targets
cargo build --release -p exchange
```

The engine, book, tick grid, trigger evaluation, block state machine, and translation layer are all pure and tested without tokio. One test in the strategy parser hits the real OpenAI API and skips itself when `OPENAI_API_KEY` is unset.

Frontend:

```bash
cd frontend
pnpm typecheck       # tsc -b --noEmit
pnpm lint            # eslint (strict + stylistic typescript-eslint, react-hooks)
pnpm test:run        # vitest, jsdom, msw — covers src/api
pnpm test:coverage
pnpm build           # tsc -b && vite build → dist/
```

---

## Configuration reference

### Server environment

| Variable | Default | Used for |
|---|---|---|
| `PORT` | `8080` | listen port; Railway sets this |
| `OPENAI_API_KEY` | unset | `POST /api/strategy` returns `config_error` without it |
| `RUST_LOG` | unset | `info` shows lifecycle; `debug` is noisy |

`dotenvy` loads `.env` from the working directory (`server/.env`).

### Server constants worth knowing

| Constant | Where | Value |
|---|---|---|
| `STARTING_BALANCE` | `main.rs` | $1,000 |
| `MARKET_DURATION_SECS` | `scheduler.rs` | 60 |
| `BTC_VOL_ANNUAL` | `pricing/fair_value.rs` | 0.40 (lower = sharper moves) |
| `RESOLVED_MARKETS_CAP` / `RECENT_FILLS_CAP` | `engine/mod.rs` | 60 markets / 1,000 fills |
| `BOOK_WS_DEPTH` | `api/mod.rs` | 20 levels |
| `TRADES_BATCH_INTERVAL_MS` | `api/trades.rs` | 100 ms |
| `BASE_TICK_MS`, `MAX_FIRES_PER_BLOCK_PER_ROUND` | `automation/executor.rs` | 150 ms, 120 |

### Frontend environment

| Variable | Effect when unset |
|---|---|
| `VITE_API_BASE_URL` | HTTP calls use relative `/api/...` paths |
| `VITE_WS_BASE_URL` | WS URL derived from `window.location` |

Both are read at **build time** by Vite. `frontend/src/api/config.ts` also mirrors server caps (book depth, page sizes) as constants; keep them in sync if you change the server.

---

## Production deployment

### Shape

Two deployables: a stateless-looking but actually stateful Rust process, and a static frontend bundle. They do not need to share a host as long as the frontend knows the server's public URL.

### Server on Railway (what the repo is set up for)

`server/railway.toml` configures Nixpacks with a `/live` healthcheck and on-failure restarts (`/health` is stricter and is not for restart probes). The workspace root has no package, so `railway.toml` sets `buildCommand = cargo build --release -p exchange` and `startCommand = ./target/release/exchange` explicitly.

Setup:

1. Create a Railway service from this repo and set the **root directory to `server`**. That makes Nixpacks see the workspace `Cargo.toml`.
2. Set variables: `OPENAI_API_KEY` (if you want Automate) and optionally `RUST_LOG=info`. Railway injects `PORT`.
3. Generate a public domain. Railway terminates TLS, so the frontend should use `https://` and `wss://` against it.
4. Run **exactly one replica**. State is in-process memory; two instances would be two different markets.

Any other host works the same way: build with `cargo build --release -p exchange`, run the `exchange` binary from anywhere, expose `$PORT`, and make sure the box can reach the BTC price feed and `api.openai.com`.

### Frontend as a static site

```bash
cd frontend
VITE_API_BASE_URL=https://your-server.up.railway.app \
VITE_WS_BASE_URL=wss://your-server.up.railway.app \
pnpm build
```

Upload `frontend/dist/` to any static host (Vercel, Netlify, Cloudflare Pages, S3 + CDN). Two requirements:

- Set the two env vars in the host's build settings, since they are baked in at build time.
- Configure an SPA fallback so `/leaderboard` serves `index.html`.

The server's CORS is fully permissive, so cross-origin requests from the static host work without further configuration.

### Alternative: one origin behind a reverse proxy

If you prefer to leave the frontend env vars empty, put a reverse proxy (nginx, Caddy, Cloudflare) in front that serves `dist/` and forwards `/api/*`, `/ws/*` and `/health` to the Rust process with websocket upgrade enabled. The frontend then uses relative paths and derives `wss://` from the page origin.

### Operational expectations

- **Restarts wipe everything**: accounts, balances, leaderboard, deployed strategies, market history. Users see the name gate again.
- **Memory** grows with accounts and is otherwise bounded by the rolling buffers. A few hundred users is fine on a small instance.
- **CPU** is dominated by websocket fan-out. Every book change broadcasts a full 20-level snapshot to every `/ws/book` and `/ws/positions` subscriber, so cost scales with subscribers × book churn.
- **Logs** go to stdout via `tracing`. `RUST_LOG=info` gives one line per market open and resolve, plus feed reconnects and lagging subscribers.

---

## Known gaps and gotchas

- **No authentication.** Anyone can place or cancel orders for any address by passing it in the request body. The handler carries a TODO for wallet signatures. Do not expose this to untrusted users with anything at stake.
- **`CLAUDE.md` at the root is stale in three places**: it says money is in integer cents (it is tenths of a cent), prices are 1–99 (they are a 153-tick grid in tenths), and the engine is a single-writer task fed by `mpsc` (it is behind a `Mutex`).
- **No Vite proxy.** The proxy was removed when the frontend was rebuilt, but `CLAUDE.md` and the comment in `dev.sh` still mention it. Set `frontend/.env.local`.
- **Rounds are not wall-clock aligned.** Every series' first round opens when the first price tick arrives and free-runs from there, so a 15-minute round can close at 10:07:23. Alignment to :00/:15/:30/:45 is a scheduler-only change.
- **Cosmetic data**: the "Watching" counter is a random walk, and the stat strip's volume/liquidity/trader figures are scaled by fixed display multipliers. The Related Markets cards are the *other* series (live countdown and mark).
- **Server `Cargo.toml` release profile** has `lto = false` despite the comment in `CLAUDE.md`. Turn it on if you want a smaller, faster binary at the cost of build time.
- **`agent_docs/` and `ignore-old-frontend/`** are referenced in comments but are gitignored and not in the repo.
- **Price feed dependency.** If the price feed is down the server serves stale prices; the scheduler defers round transitions while the feed has never delivered a tick but will still resolve on the last known price if the feed later stalls.
