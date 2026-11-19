# tiny-next

The frontend for Tiny, a prediction market. This file is the field guide: what a market
frontend has to get right, how this repository is set up to get it right, and where we are.
The enforced rules are in [`AGENTS.md`](./AGENTS.md); every agent and reviewer reads that.
This file is the why.

## The bar

A prediction market has three properties most web apps never meet. They shape everything.

- **Price is probability.** One share pays $1.00 if its outcome happens, so 31¢ *is* 31%.
  Money and probability travel as integers (`_cents`, `_bps`), exactly as the backend stores
  them. A float never carries either.
- **A quote is a promise.** If the screen says 31¢ we fill at 31¢, or we refuse and say why.
  Degraded states fail closed: a stale feed, missing depth or a failed request disables the
  action with a reason. No fallback price, no clamped default, no spinner that means nothing.
- **The feed is live and the phone is slow.** Thousands of markets, each with a moving
  order book, on a mid-tier Android over 4G. The bundle has to stay small enough to be
  interactive in two seconds, and every live number has to say whether it is `live`,
  `stale` or `disconnected`. We never fabricate a tick.

And one property that is ours: **mobile first, then expanded.** Every screen is designed
and built at 360px before it is allowed to grow.

## Setup

```sh
fnm install                     # Node from .nvmrc. Add `fnm env --use-on-cd` to your shell once.
fnm use
pnpm install --frozen-lockfile  # also installs the git hooks
pnpm exec playwright install chromium webkit   # once, for pnpm e2e
pnpm dev
```

The toolchain is pinned and enforced: a wrong Node refuses to install, a stale pnpm 9
fetches the pinned pnpm first. If `pnpm install` rewrites `pnpm-lock.yaml`, you are on the
wrong pnpm; do not commit that diff. Every environment variable is validated in
`src/env.ts` and documented in `.env.example`; all have defaults, so
nothing is needed to run locally.

## Commands

| Command | What it does |
| --- | --- |
| `pnpm dev` | Next.js dev server on :3000 |
| `pnpm check` | format · lint (warnings fail, same as CI) · typecheck · tests · knip · icon drift. Under ten seconds warm. |
| `pnpm build` | Production build |
| `pnpm e2e` | Build, then Playwright on Desktop Chrome, Pixel 7 and iPhone 15 |
| `pnpm analyze` | `next experimental-analyze`: where every kilobyte comes from |
| `pnpm lint` · `pnpm typecheck` · `pnpm test` · `pnpm knip` | The individual gates |
| `pnpm icons:generate` | Regenerate `packages/ui/src/icons` from the SVG sources |

## What is in the box

```
src, e2e, public    Next.js 16 App Router, at the workspace root. Thin routes, feature modules, the app shell.
packages/ui         Tokens, primitives, generated icons. No product knowledge, no fetching.
packages/money      Cents, Bps, banker's rounding, one locale-aware formatter. No React.
packages/domain     zod/mini wire contracts mirroring silk-road field names. No React.
packages/api        fetch → parse → typed result. Retries with a budget, idempotency keys,
                    never throws into UI. No React.
packages/testing    Shared Vitest setup and the axe-core assertion for component tests.
packages/config     tsconfig and postcss presets.
```

Dependencies point down: the app imports packages, a package never imports the app, `ui`
never imports a feature. The three pure packages import no React, Next, or DOM and are
tested in milliseconds. Nothing in `src/` consumes them yet; they exist so the first
data-bound screen has a typed boundary to land on instead of inventing one. `@kyros/domain`
marks its two assumptions about the wire format and will be corrected against the real
silk-road contract.

Installed, pinned, and waiting for a first consumer: `@tanstack/react-query`, `nuqs`,
`zustand`, `es-toolkit`. None is mounted, because a provider with no consumer ships
JavaScript for nothing. Deliberately not here yet: i18n, error reporting, analytics,
Storybook, the realtime layer, the chart library. Each arrives with the thing that needs it.

## The gates

Rules that are not failing tests are suggestions. What runs, when, and what it catches:

| When | What | Catches |
| --- | --- | --- |
| `git commit` | Biome and oxlint on staged files, about two seconds | formatting, `console.*`, `process.env` outside `env.ts`, banned imports, empty `catch`, non-null assertions, import cycles |
| `git push` | typecheck, all tests, knip, about eight seconds | type errors under the strictest flags, behaviour regressions, dead code, unused dependencies |
| CI, every PR | the above, then build, then Playwright on three devices | hydration or CSP errors, horizontal overflow on a phone, WCAG 2.2 AA violations, missing security headers, size budgets, a missing page answering 200 |

A one-line fix clears every local gate in under twenty seconds. The gates are strict on
purpose and cheap by measurement.

## Bundle policy

Measured from the production server as encoded bytes on the wire, in
`e2e/budgets.e2e.ts`. Budgets live in `e2e/routes.ts`.

| Route | HTML gzip | JS gzip | Budget |
| --- | ---: | ---: | --- |
| `/` | 4.5 KB | 156 KB | 40 KB · 170 KB |

The Next 16 + React 19 runtime is about 130 KB of that JS. Everything above it is ours to
defend, and these rules do the defending:

- **Server Components by default.** `'use client'` goes on the smallest interactive leaf.
- **Nothing ships without a consumer.** No provider, store or SDK is mounted before the
  first component that needs it.
- **Subpath imports only.** `@kyros/ui/components/button`, `radix-ui/tabs`, `zod/mini`. Root
  barrels and the full `zod` build are banned by lint, along with lodash, axios and moment.
- **One library per job**, and a written justification for any new dependency: what it
  replaces, its gzip size, alternatives searched. Over 20 KB gets `pnpm analyze` first.
- **Heavy client modules load late** through `next/dynamic` with a skeleton that matches the
  final dimensions.
- **Charts render on canvas.** Never an SVG path with more points than twice its pixel
  width. The current market prototype ships 623 KB of SVG path data, twice; that is why it
  is not under the budget gate yet.

## Type policy

- TypeScript 7 at `strict`, `noUncheckedIndexedAccess`, `exactOptionalPropertyTypes`,
  `noPropertyAccessFromIndexSignature`, `noImplicitReturns`, `erasableSyntaxOnly`. No
  `as any`, no `as unknown as`, no non-null assertions in production code.
- **Typed routes.** `typedRoutes: true` makes every `href` a compile-time check. The five
  destinations the prototype nav links to before they exist go through one named cast,
  `plannedRoute()`, so the debt is a grep away and disappears route by route.
- **Branded money.** `Cents` and `Bps` are nominal types. A bare number is not accepted, a
  float cannot be constructed, and `applyBps` rounds half-to-even on `bigint`.
- **Contracts at the boundary.** `@kyros/domain` mirrors silk-road field names verbatim;
  renaming happens only in a `*.mapper.ts`. `@kyros/api` validates every response through
  a Standard Schema and returns a discriminated `ApiResult`, never a throw.
- **Typed environment.** `env.ts` is the only reader of `process.env`, enforced by lint,
  validated at startup, public subset inlined at build.

## Mobile policy

- Base classes are the phone layout; `sm:` `md:` `lg:` `xl:` add complexity upward. `lg`
  (1024px) is the desktop line. One breakpoint system, declared in `tokens.css`.
- Layout containers never carry `min-w-[Npx]`, fixed heights on content, or
  `overflow-hidden` on the page. Viewport height is `dvh`.
- Touch targets are at least 44×44 (`min-h-touch min-w-touch`). Inputs are at least 16px.
  Fixed bars pad with `pb-safe-bottom`; the root `viewport` sets `viewportFit: 'cover'`.
- Components that live in more than one column width adapt with container queries.
  Density is a prop, not a forked component.
- Playwright runs the gated routes on Pixel 7 and iPhone 15 and fails on horizontal
  overflow.

## Security

- Strict nonce-based Content Security Policy minted per request in `src/proxy.ts`:
  `script-src 'self' 'nonce-…' 'strict-dynamic'`, no `unsafe-eval` in production,
  `frame-ancestors 'self'`, `object-src 'none'`. Next stamps the nonce on its own tags; the
  e2e suite asserts every script on the page carries it and that the nonce changes per
  request.
- HSTS with preload, `nosniff`, `strict-origin-when-cross-origin`, a Permissions-Policy that
  turns off camera, microphone, geolocation and topics, and no `x-powered-by`.
- `error.tsx` and `global-error.tsx` never render the error message; Next logs the digest.
  The reporting vendor plugs in there when one is chosen.

## Where we are

The foundation is green: `pnpm check`, `pnpm build` and the Playwright suite pass on the
pinned toolchain.

The two existing screens are a prototype transcribed from one 1726px design frame. They
stay running and are honest about what they are: the smoke test checks they render and
hydrate, but they are not under the overflow, accessibility or budget gates. Those apply
to `/` today and to the market screen the day it is rebuilt mobile-first. The market route
already has the shape that rebuild needs: one cached server-only loader in
`src/server`, a segment layout that resolves the market above the streaming
boundary so an unknown slug is a real 404, and `loading`, `error` and `not-found` files.

Next, in order: point the loader at silk-road through `@kyros/api`; a design-system pass in
`packages/ui` for touch-safe sizes, a `Sheet`, `useMediaQuery` and a contrast fix on the
`--market-away` token; then rebuild the market screen mobile-first on the canvas chart and
add it to the gated routes.

The source-grounded review of Silk Road's Temporal market lifecycle, its usable frontend contract,
and the proposed Tiny Next delivery stack live in
[`docs/silk-road/`](./docs/silk-road/README.md).

## Decisions

Short records of choices that were not obvious.

- **Every route renders per request.** A CSP nonce needs dynamic rendering, so the root
  layout awaits `connection()`. `cacheComponents` stays off until Next supports nonces with
  a prerendered shell. For a live market this costs TTFB, not JavaScript.
- **`style-src` keeps `'unsafe-inline'`.** Server-rendered `style` attributes (depth bars,
  data-driven CSS variables) are blocked by a nonce-only style policy and React does not
  reapply them on hydration. Scripts are strictly nonced; styles are the smaller risk.
- **404 decisions happen in a segment layout.** Once a `loading.tsx` boundary has streamed
  the shell, `notFound()` from the page can only produce a 200 with a `noindex` tag.
  Resolving the entity in `market/[slug]/layout.tsx` keeps the status honest. The lookup is
  wrapped in React `cache`, so the page pays nothing.
- **`zod/mini` everywhere.** Schemas reach the client through `env.ts` and `@kyros/domain`.
  Mini is a fraction of the full build; the full import is banned by lint.
- **Budgets are measured in the browser, not from manifests.** Turbopack's output does not
  expose per-route client chunks; encoded bytes on the wire are what a phone pays.
- **Copy is inline for now.** One locale, a handful of strings. A catalog arrives with the
  second locale, not before.
- **Generated files are not tracked.** `next-env.d.ts` is regenerated by dev, build and
  typegen; the Next docs say to ignore it. Icons are generated and drift-checked.

## Teeny local agent

Mastra lives in `src/mastra`. Run `pnpm studio` for
Studio at http://localhost:4111. Set `OPENAI_API_KEY` in `.env` and
`MASTRA_STORAGE_DIR` to an absolute local directory so Studio and Next.js use
the same conversation store. Local database files and workspaces are ignored by Git.

The market ChatPanel now streams Teeny replies through `/api/chat` using the AI SDK
and Mastra adapter. The first submission replaces the public strategy list; stop and
retry controls preserve the conversation. Chat survives an event rollover in the same
market series. The back button returns to saved chat cards above public strategies;
select a card to reopen its history, including after a reload. Message history and traces are
stored locally in libSQL; Studio can inspect them using the same storage directory.

The route is development-only until account authentication is integrated. It uses an
HTTP-only anonymous browser cookie to scope memory, requires same-origin requests,
validates text-only messages, and ignores client-supplied model, tool, and memory
settings. These anonymous sessions are not production user authentication.

Teeny has read-only Price Feeds tools and the strategy capability below; no account
tools, browsing, filesystem, shell, schedules, or trading. The UI does not pass fixture
market values to the model. Mastra's web app dependencies use TypeScript 6 because its
CLI does not yet support TypeScript 7.

### Teeny Price Feeds tools

Set server-only `PRICE_FEEDS_API_URL` to the public Price Feeds origin (local stack:
`http://localhost:8083`). If unset, it uses `NEXT_PUBLIC_PRICE_FEEDS_URL`; if both are
unset, tools explicitly return unavailable. No admin credentials are used. The
existing public read surface has no bearer authentication; this does not authorize
future account or admin access.

Teeny discovers active assets, reads one live SSE observation and disconnects, or
reads the last 1–1440 minutes of history through REST. Requests are uncached with a
five-second deadline and no retries. Live observations older than five seconds or
more than one second in the future are withheld. These are local assistant freshness
limits, not settlement rules. An active asset does not guarantee live ticks.

History is up to 60 LOCF samples with actual first/last observation coverage; sample
times are not tick times or exact extrema. Integer mantissa/exponent prices are
preserved. No direct database/Valkey access, background monitoring, news, or trading.
The agent and route allow up to five steps for discovery, reads, and a final answer.
Client history sends text only; browser-supplied tool results remain rejected.

### Saved chat navigation

Going back keeps an in-progress reply running and shows its chat card immediately.
Unsaved local conversations are labeled; a failed request can be reopened and retried.

New chats are named from the first message (a bounded, single-line title), stored in
Mastra's existing LibSQL thread store, and tagged with the market series slug.
`GET /api/chats` lists the current browser's chats for that market, 20 at a time;
`GET /api/chats/[id]` loads its text transcript. Both remain development-only and
check server-derived ownership. The browser sends only the latest user turn; Mastra
retrieves prior context from storage rather than accepting client tool results.

The anonymous HTTP-only cookie persists for one year. Clearing it loses access to
that browser's chats; this is not a Tiny account, multi-device history, or a durable
personal-memory product. Account-backed history must use verified Tiny user IDs
instead of accepting an identity from the browser. Earlier prototype threads without
market metadata are retained in the database but are not assigned to a market's cards.

### Building a strategy from the chat

The `strategy` capability (`src/mastra/capabilities/strategy`) is enabled when
`EXCHANGE_API_URL` and `OPENAI_API_KEY` are set. Teeny only starts a build when the
user asks for one. It fills a brief in the user's terms (entry, side, once or every
time, market or resting order, exit, size), reads real signal ids through
`listMarketSignals`, asks only about what it cannot infer, then calls
`proposeCompilation`. That renders a card in the transcript with the plan and its
assumptions and two buttons: Compile and Keep talking. Nothing compiles until Compile
is pressed.

Compile posts the brief to `POST /api/strategy/builds`, which starts the
`compile-strategy` Mastra workflow in the background and answers a run id. The card
polls `GET /api/strategy/builds/[id]` and shows the running step, elapsed time and the
steps completed while the chat stays usable. The workflow runs four steps with a
tool-less writer agent: scout (choose signals from the series' live catalogue), write
(a flat authoring document under OpenAI strict structured output, projected to the
compiler's IR), compile (the Rust `POST /api/strategy/compile` with up to three
diagnostic-driven repairs) and review (a plain-language readback checked against the
brief). A finished card shows the readback, assumptions, compiler notes and a disabled
Deploy button; a failed one shows the compiler's objections. "Ask Teeny about this"
drops either back into the conversation. Both routes are development-only and scoped
to the browser session that started the build. Compile checks run against a fixed
development envelope until a deploy form exists.

## Local exchange demo

Start `make up` in the sibling `silk-road` repository. In this frontend's
`.env.local`, enable the development-only demo:

```dotenv
EXCHANGE_DEMO_UI_URL=http://127.0.0.1:8090
EXCHANGE_DEMO_GATEWAY_URL=ws://127.0.0.1:9080/v1/ws
EXCHANGE_DEMO_FEED_URL=ws://127.0.0.1:9082/v1/stream
```

Run `pnpm dev --port 3005` and open an existing market page,
for example <http://localhost:3005/market/btc-updown-2m>. The existing order book
and UP/DOWN quote buttons now use the exchange feed. The existing Buy widget
submits price-capped immediate-or-cancel orders against the displayed quote,
with whole-lot sizing and a worst-case fee reserve inside the entered budget.
Buying DOWN sends a sell-YES order at the complementary price. Unfilled lots
cancel; commands never replay automatically after a disconnect. The page shows
demo balances, positions, open-order cancellation, and your trade history.
The order book follows the spread until manually scrolled; the center button
resumes following. A visible simulation banner distinguishes these orders from Silk Road's
BTC lifecycle. All enabled market pages share the same local demo account and
follow monofleet's current market; this is not a backend catalog mapping.

The demo uses the funded account token already served by monofleet's UI.
The server reads its exact token literal without executing upstream code;
the token stays in page memory and is never put in browser storage. This
adapter is coupled to monofleet's development HTML, fails closed if that
contract changes, and accepts only loopback URLs. Refresh the session after
an exchange restart. The local integration is disabled outside development or when
not enabled. It uses monofleet's mock signing and settlement, and does not
connect Silk Road catalog IDs to exchange market IDs.

The public feed pauses in hidden tabs and resynchronizes on return. Private
account connections remain active. Network reconnects obtain fresh snapshots;
commands are never replayed automatically. Open orders remain visible until
the exchange confirms their outcome. Open orders and trade history are paginated;
public recent trades are not displayed.

With the Silk Road stack and frontend running, verify the existing BTC page with
`pnpm e2e:exchange`. It defaults to `http://localhost:3000`;
set `EXCHANGE_TEST_BASE_URL=http://localhost:3005` for another local port.
The opt-in test places small UP/DOWN orders using demo funds and captures a
1440px screenshot. The normal `pnpm e2e` suite still targets production.
