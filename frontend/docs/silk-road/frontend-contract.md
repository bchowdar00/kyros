# Frontend contract and gaps

## Contract Tiny Next can use today

Silk Road public reads are anonymous REST endpoints under `/v1`. They return camelCase JSON in a
cursor envelope:

```json
{ "items": [], "nextCursor": null, "hasMore": false }
```

The default page size is 20 and the maximum is 100. A client must keep following `nextCursor` while
`hasMore` is true and reject a missing or non-advancing cursor.

| Request | Filters | Purpose |
| --- | --- | --- |
| `GET /v1/categories` | cursor, limit | Category catalog |
| `GET /v1/tags` | cursor, limit | Tag catalog |
| `GET /v1/series` | cursor, limit, category, status | Recurring declarations; `status=active` drives Automation |
| `GET /v1/events` | cursor, limit, `seriesId`, slug | Generated windows |
| `GET /v1/markets` | cursor, limit, `eventId`, slug | Market status, result, copy, timing and evidence |

The lifecycle correlation path is Series `id` -> Event `seriesId` -> Market `eventId`. The Market's
`metadata.automation.priceSymbol` selects the Price Feeds stream; do not hard-code BTC in frontend
logic even though BTC is the only current automation implementation.

### Durable lifecycle evidence

```json
{
  "automation": {
    "kind": "btc_updown",
    "workflowId": "market-cycle:100001:1788436800",
    "seriesId": 100001,
    "priceSymbol": "Crypto.BTC/USD",
    "reference": {
      "instant": "2026-09-03T12:00:00Z",
      "mantissa": "123456789",
      "exponent": -8,
      "publishTimeMicros": "1788436800000000"
    },
    "observation": {
      "instant": "2026-09-03T12:00:10Z",
      "mantissa": "123456790",
      "exponent": -8,
      "publishTimeMicros": "1788436810000000"
    },
    "result": "yes"
  }
}
```

`observation` and metadata `result` are absent until determination. The top-level REST result is
`MARKET_RESULT_YES` or `MARKET_RESULT_NO`; statuses use the analogous prefixed enum strings.

### Price Feeds

| Request | Shape | Semantics |
| --- | --- | --- |
| `GET /v1/stream/prices?symbols=...` | SSE `TickMessage` frames | Transient, lossy display ticks; 15-second comment heartbeat |
| `GET /v1/prices?symbol=&from=&to=&resolution=` | Exact-decimal sampled history | BTC spot history, maximum 24-hour range and 2–1000 points |
| `GET /v1/price?symbol=&t=` | One immutable point | Settlement-grade point read; frontend normally reads persisted evidence instead |

An SSE tick is `{v, provider, symbol, mantissa, expo, publishTimeUs}`. `mantissa` is a decimal
string; `publishTimeUs` is currently a JSON number. Rendering must preserve the exact decimal
without converting the domain value to floating point.

## Source-of-truth rules

| UI fact | Authority | Freshness behavior |
| --- | --- | --- |
| Series/Event/Market status and final result | Silk Road | Refetch; never infer a durable transition from time or a tick |
| Opening/closing evidence | `market.metadata.automation` | Immutable once persisted |
| Current BTC display price | Price Feeds SSE | Label `live`, `stale` or `disconnected`; never use to settle |
| BTC history | Price Feeds range read | Show coverage gaps; it is not probability history |
| Temporal progress | Derived from durable market fields | Workflow ID is correlation only, not proof that a step ran |
| Tradable price/depth | No current authority | Unknown; hide or disable trading UI |

## Tiny Next contract mismatch

The current `packages/domain/src/catalog.ts` is an earlier assumption, not the implemented REST
contract. It uses snake_case, embeds Markets in Events, models outcomes as child rows and expects
`opens_at`/`occurs_at`/`closes_at`. Silk Road actually serves separate camelCase Series, Event and
Market pages with `startsAt`/`endsAt`, binary labels on Market, enum-prefixed statuses and arbitrary
metadata. Those schemas should be replaced, not adapted around.

The prototype view model also contains data the lifecycle does not provide:

- probability and outcome quote series;
- order-book levels and spread;
- strategy, order, position and PnL state;
- sports scoreboard and signals;
- tradable deployment or order-admission state beyond constant placeholders.

Those sections must not receive synthetic fallbacks. The initial data-bound screen is a lifecycle
market screen; the richer trading screen stays behind a separate capability gate until its owning
services and contracts exist.

## Backend gaps to resolve before a production discovery surface

1. Add a frontend-oriented read endpoint that returns a Market with its Event and Series, addressable
   by Market slug. The current detail lookup needs Market-by-slug, then all Events for its Series to
   find `eventId`, then a scan of Series pages to find the Series.
2. Add a cursor-paginated discovery read model with server-side lifecycle/status/category filters.
   The observatory's Series -> Events -> Markets fan-out is acceptable for a local tool, not a home
   feed with thousands of Markets.
3. Publish a browser realtime contract with a snapshot, stable asset identity, sequence, explicit
   status events and source timestamp. Current symbol SSE cannot satisfy Tiny Next's gap detection
   and re-snapshot invariant.
4. Decide whether `publishTimeUs` must become a decimal string for JavaScript precision consistency.
5. Add caller authentication before treating any admin mutation as a frontend-accessible operation.

The newer `tiny.api.pricefeeds.v1.PublicPriceFeedsService` protobuf describes snapshot and status
events, but it is not mounted by the checked-in Price Feeds server. Tiny Next must implement against
the executable REST/Connect routes, not that intended contract.

## Recommended network boundary

- `SILKROAD_API_URL` is server-only. Server loaders call Silk Road through `@tiny/api`; the browser
  does not need direct durable-state access or Silk Road CORS.
- `NEXT_PUBLIC_PRICE_FEEDS_URL` is the one browser-visible origin required for native `EventSource`.
  Add only its origin to CSP `connect-src` and its exact Tiny origin to Price Feeds CORS.
- Do not store service endpoints in local storage in the product. Environment and deployment own
  them; the standalone Market UI's endpoint picker is an operations convenience.
- Prefer existing REST plus `zod/mini` boundary validation for this slice. Do not add protobuf or a
  Connect client to the browser bundle without a measured consumer-driven reason.
