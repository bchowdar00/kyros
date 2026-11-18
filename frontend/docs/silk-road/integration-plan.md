# Tiny Next integration and content plan

## Product decision

Ship the first integration as a truthful **market lifecycle view**, not a partially working
trading market. It should answer five questions in order:

1. What resolves this Market?
2. Is its window scheduled, active, closed, determined or settled?
3. What exact opening evidence is recorded?
4. What is the current display price and how fresh is it?
5. What closing evidence and result were persisted?

The existing visual language can stay, but the content hierarchy must follow available authority.

## Market page content

| Order | Region | Available now | Failure/degraded state |
| ---: | --- | --- | --- |
| 1 | Identity | Category, Event title, Market question, UP/DOWN labels | Missing Market slug is a real 404 |
| 2 | Status strip | Market/Event state, UTC window, countdown derived from `endsAt` | Clock never advances durable state; show “awaiting Silk Road” |
| 3 | Evidence panel | Opening price/time, live price, closing price/time | Missing observation says “awaiting exact closing point” |
| 4 | Outcome | Leading direction while active; final YES/NO only when determined | Direction unavailable when exact values cannot be compared |
| 5 | Lifecycle timeline | Reference captured -> window closed -> outcome determined -> Event settled | Derive only from persisted fields, never Temporal guesses |
| 6 | Rules and provenance | Resolution rule, settlement source, symbol, workflow ID | Workflow ID is copyable diagnostic context |
| 7 | Recent windows | Prior Event/Market windows for the same Series | Paginated/virtualized; empty is an explicit state |

Do not render the order book, strategy rail, signals, positions or trade controls in this capability.
When trading contracts arrive, add those as a separate `trading` capability so a lifecycle Market
does not look executable merely because its content is known.

Home should eventually group active/recent Markets by Series, but only after the backend discovery
read model exists. Until then, link directly to known Market slugs or keep the data-bound home behind
a feature flag; do not ship the observatory's N+1 traversal as the production feed.

## Target code shape

```text
packages/domain/src/
  market-catalog.ts        camelCase Silk Road REST schemas and enums
  market-lifecycle.ts      automation evidence and composed lifecycle schemas
  price-feed.ts            SSE tick and price-history schemas
packages/api/src/
  cursor.ts                checked cursor traversal
  silk-road.ts             public catalog/event/market reads
  price-feeds.ts           history reads; ApiResult error translation
src/server/
  market-detail.server.ts  cached Market-by-slug composition and 404 decision
  home.server.ts           discovery endpoint consumer when available

src/features/market-lifecycle/
  types.ts                 view models only
  market-lifecycle.mapper.ts
  exact-price.ts           exact comparison/formatting without float domain values
  price-stream.ts          one external ref-counted stream store
  use-price-stream.ts      useSyncExternalStore binding
  market-lifecycle-screen.tsx
  lifecycle-status.tsx
  price-evidence.tsx
  lifecycle-timeline.tsx
  recent-windows.tsx
```

Wire names stay verbatim in `packages/domain`. Only the mapper produces display names. Server data
arrives as props; the smallest realtime leaf subscribes to the external store. URL-selected Series
or history window uses `nuqs`; no server data enters Zustand.

## Delivery stack

Each step is one Graphite PR, at most 400 changed lines, and keeps `pnpm check`, `pnpm build` and
`pnpm e2e` green.

### 0. Contract decision with Silk Road

- Agree on Market-detail and discovery read models, browser stream v2 and timestamp precision.
- Record unresolved protocol choices as ADRs in the owning backend repository.
- Treat the current REST shape as pinned input until a versioned replacement lands.

Exit: example payloads and failure semantics are approved by both frontend and backend owners.

### 1. Replace assumed domain contracts

- Add exact `zod/mini` schemas for cursor pages, Series, Event, Market, automation evidence, price
  history and tick frames.
- Preserve camelCase provenance names and exact decimal strings.
- Delete the incompatible embedded Event/Market/Outcome assumptions when their callers migrate.
- Add boundary and malformed-payload tests for every exported schema.

Exit: checked examples from Silk Road and Price Feeds parse; unsafe numbers and incomplete evidence
fail validation.

### 2. Add typed read clients

- Extend `@tiny/api` with checked cursor traversal and focused Silk Road/Price Feeds functions.
- Use one retry budget, return `ApiResult`, propagate cancellation and reject non-advancing cursors.
- Add MSW tests for pagination, 404, 422, 429, 5xx, timeout and invalid JSON/schema responses.

Exit: no feature code calls `fetch`, and no partial page can masquerade as a complete catalog.

### 3. Bind the durable market route

- Add server-only Silk Road configuration and replace the fixture body in
  `market-detail.server.ts`.
- Resolve the Market above `loading.tsx`, preserving the real 404 behavior and cached duplicate
  lookup between layout, metadata and page.
- Introduce a lifecycle-specific view model and explicit unavailable states.
- Keep this incomplete product slice behind a feature flag until its route is fully gated.

Exit: a real active, closed-unresolved and settled Market each render without fabricated fields.

### 4. Build the lifecycle content surface

- Implement the seven content regions above with semantic HTML, keyboard access and exact values.
- Keep the current 1440px desktop gate and mobile-coming-soon CSS branch.
- Add component behavior/a11y tests and 1440px design QA evidence.

Exit: the route has loading/error/not-found parity, zero axe violations and no prototype-only data.

### 5. Add realtime after stream v2

- Add one multiplexed external connection store, ref-counted subscriptions and
  `useSyncExternalStore` binding.
- Track `connecting`, `live`, `stale` and `disconnected`; on sequence gaps, mark stale and
  re-snapshot. Pause non-session streaming while hidden.
- Update only the live-price leaf. Never write ticks into TanStack Query or use them for settlement.

Exit: reconnect, gap, malformed frame, stale timeout and unmount behavior are deterministic tests;
every live number exposes freshness and no action is enabled from display-only data.

### 6. Add history and discovery

- Use the backend discovery read model for home cards and the related-window endpoint for history.
- If BTC spot history is shown, label it as “BTC price,” not probability, and show raw coverage.
- Virtualize unbounded lists and keep URL filters/shareable selection in `nuqs`.

Exit: request count is bounded per page, cursor pagination is complete and route byte budgets pass.

### 7. Add trading only with an exchange contract

- Require authoritative quotes, depth, order admission, balances, positions, fills, idempotent money
  mutations and authenticated sessions.
- Map integer cents/basis points through `@tiny/money`; fail closed on stale depth or admission.
- Restore order book, strategy and trade controls only as their real owners land.

Exit: the displayed executable quote is the quote the backend will honor, or the action is disabled
with a reason.

## Test matrix

| Layer | Required proof |
| --- | --- |
| Domain | Exact contract examples, enum exhaustiveness, optional observation/result pairing, integer precision |
| API | Full pagination, retry budget, abort, status translation, schema rejection |
| Mapper | Active, closed-unresolved, determined and settled states; exact equal-price resolves UP |
| Realtime | Snapshot, sequence gap, stale/disconnected, reconnect, hidden tab and ref counts |
| Components | Role/name assertions, keyboard flow, visible freshness, zero axe violations |
| E2E | Real 404, loading/error states, lifecycle fixture through MSW, CSP/CORS, 1440 screenshot, budgets |
| Backend contract | Silk Road + Price Feeds consumer examples in their integration suites |

## Release blockers

- Frontend-oriented Market detail and discovery reads are unavailable.
- The mounted browser stream cannot provide snapshot/sequence gap recovery.
- Silk Road read/write authentication is not implemented.
- No exchange authority exists for tradable probability, depth or order admission.

These are capability boundaries, not reasons to invent UI data. The lifecycle detail can ship once
the first two are resolved; trading remains independently blocked by the latter two plus the full
exchange contract.
