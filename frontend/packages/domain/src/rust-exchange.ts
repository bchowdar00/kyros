import * as z from 'zod/mini'

// Wire contracts of the Rust exchange in `server/crates/exchange/src/api/*`. Field names
// mirror the Rust structs verbatim. Money is integer tenths of a cent (`$1 = 1000`); YES
// prices are integer tenths of a cent, 1..999, on the fixed 153-tick grid.

/** `0x` plus 40 lowercase hex characters, as `Address` serializes. */
export const RustAddressSchema = z.string().check(z.regex(/^0x[0-9a-f]{40}$/))
export const RustSeriesSlugSchema = z.string().check(z.regex(/^[a-z0-9-]+$/))
export const RustPriceTicksSchema = z.int().check(z.minimum(1), z.maximum(999))
const tenths = z.int()
const unixMs = z.int().check(z.minimum(0))

/** What a BTC round opened on: its strike. */
export const RustBtcRoundReferenceSchema = z.object({ strike_btc_usd: z.number() })

/** What an NBA round opened on: the two teams, the pregame home win probability, the game number. */
export const RustNbaRoundReferenceSchema = z.object({
  home_team: z.string(),
  away_team: z.string(),
  pregame_home_prob: z.number(),
  game_seq: z.int(),
})

/**
 * What the exchange follows on Gemini for one series. `env` is where the book and the
 * results come from; `phase` is where the series stands between Gemini's windows; `book`
 * says whether the mirrored book is being fed (`stale` after ten quiet seconds). The
 * contract fields are absent between rounds.
 */
export const RustGeminiSeriesSchema = z.object({
  env: z.enum(['production', 'sandbox']),
  /** `real` sends this operator's strategy orders to Gemini; absent (or `paper`) keeps them pretend. */
  orders: z.optional(z.enum(['paper', 'real'])),
  /** Gemini's series code, e.g. `BTC05M`. */
  series: z.string(),
  phase: z.enum(['waiting_for_window', 'open', 'waiting_for_result']),
  book: z.enum(['waiting', 'live', 'stale']),
  event_ticker: z.optional(z.string()),
  /** Gemini's instrument symbol for the Up contract. */
  symbol: z.optional(z.string()),
  /** A decimal string, exactly as Gemini publishes the strike. */
  strike: z.optional(z.string()),
  starts_at_ms: z.optional(unixMs),
  expires_at_ms: z.optional(unixMs),
})

export const RustSeriesInfoSchema = z.object({
  series: RustSeriesSlugSchema,
  duration_secs: z.int().check(z.minimum(1)),
  /** The series underlying, e.g. `btc`; absent on an older exchange. */
  underlying: z.optional(z.string()),
  /** Outcome labels, positive side first (`up`/`down`). */
  outcomes: z.optional(z.array(z.string())),
  current_market_id: z.nullable(z.int()),
  opens_at_ms: z.nullable(unixMs),
  closes_at_ms: z.nullable(unixMs),
  /**
   * What the round opened on: `RustBtcRoundReferenceSchema` or `RustNbaRoundReferenceSchema`
   * by underlying. Left unread here so one row with a reference this client cannot read
   * does not fail the whole list; the mapper parses it and drops only that series.
   */
  reference: z.optional(z.unknown()),
  /** Absent when the exchange is not following Gemini. */
  gemini: z.optional(RustGeminiSeriesSchema),
})
export const RustSeriesListSchema = z.array(RustSeriesInfoSchema)

const hash32 = z.string().check(z.regex(/^0x[0-9a-f]{64}$/))

/** One settled round of a deployment. `tx_hash` is null until the seal is on chain. */
export const RustSealViewSchema = z.object({
  round_id: z.int().check(z.minimum(0)),
  won: z.boolean(),
  pnl_cents: z.int(),
  journal_hash: hash32,
  tx_hash: z.nullable(hash32),
  /** Lines in the round's journal, served at `/api/records/{id}/{round_id}/journal`. */
  lines: z.int().check(z.minimum(0)),
})

/**
 * `GET /api/records/{id}`: a deployment's on-chain record. `{id}` is the deployment id or
 * the sub-account address. `open_tx` is null until the deployment is recorded on chain.
 */
export const RustRecordViewSchema = z.object({
  deployment_id: hash32,
  account: RustAddressSchema,
  series: RustSeriesSlugSchema,
  deployment: z.string(),
  strategy_id: z.string(),
  program_hash: hash32,
  budget_cents: z.int().check(z.minimum(0)),
  open_tx: z.nullable(hash32),
  seals: z.array(RustSealViewSchema),
})

const RustMarketStatusSchema = z.union([
  z.object({ kind: z.literal('open') }),
  z.object({
    kind: z.literal('resolved'),
    outcome: z.enum(['yes', 'no']),
    /** BTC rounds only. */
    close_price_btc_usd: z.optional(z.number()),
    /** NBA rounds only: the final score. */
    home_score: z.optional(z.int()),
    away_score: z.optional(z.int()),
  }),
])

/**
 * One round, as returned by `GET /api/markets/history`. Timestamps are unix seconds. The
 * reference is flattened: a BTC round carries `strike_btc_usd`.
 */
export const RustMarketSchema = z.object({
  id: z.int(),
  series: z.optional(RustSeriesSlugSchema),
  strike_btc_usd: z.optional(z.number()),
  /** NBA rounds carry the matchup flattened at the top level instead of a strike. */
  home_team: z.optional(z.string()),
  away_team: z.optional(z.string()),
  pregame_home_prob: z.optional(z.number()),
  game_seq: z.optional(z.int()),
  opened_at: z.int(),
  closes_at: z.int(),
  duration_secs: z.optional(z.int()),
  status: RustMarketStatusSchema,
})
export const RustMarketsHistorySchema = z.array(RustMarketSchema)

/** One engine fill, as `GET /api/trades` lists them (newest first) and stats carry the largest. */
export const RustFillSchema = z.object({
  taker: RustAddressSchema,
  maker: RustAddressSchema,
  taker_side: z.enum(['bid', 'ask']),
  price: RustPriceTicksSchema,
  qty: z.int(),
  ts: unixMs,
  kind: z.enum(['open', 'transfer_yes', 'transfer_no', 'close']),
})

export const RustMarketStatsSchema = z.object({
  series: RustSeriesSlugSchema,
  duration_secs: z.int(),
  market_id: z.nullable(z.int()),
  opened_at_ms: z.nullable(unixMs),
  closes_at_ms: z.nullable(unixMs),
  total_volume_tenths: tenths,
  open_interest_contracts: z.int(),
  total_locked_tenths: tenths,
  total_trades: z.int(),
  unique_traders: z.int(),
  largest_trade: z.nullable(RustFillSchema),
})

/** `GET /api/signals?series=`: every signal that applies to the series, with latest values. */
export const RustSignalScopeSchema = z.discriminatedUnion('kind', [
  z.object({ kind: z.literal('series'), series_id: RustSeriesSlugSchema }),
  z.object({ kind: z.literal('underlying'), underlying: z.string() }),
  z.object({ kind: z.literal('global') }),
])

export const RustSignalFieldSchema = z.object({
  name: z.string(),
  label: z.string(),
  type: z.enum([
    'decimal',
    'u8',
    'u16',
    'u32',
    'u64',
    'i64',
    'enum',
    'duration_ms',
    'timestamp',
    'bool',
    'string',
  ]),
  variants: z.optional(z.array(z.string())),
  /** Two sentences on what the number means, from the server's copy files. */
  description: z.optional(z.string()),
  /** Shown by default; the rest are plumbing behind "Show all fields". */
  headline: z.boolean(),
  /** Absent until the signal first publishes. Decimals and timestamps arrive as strings. */
  value: z.optional(z.union([z.string(), z.number(), z.boolean()])),
})

export const RustSignalSchema = z.object({
  signal_id: z.string(),
  name: z.string(),
  description: z.string(),
  location: z.enum(['internal', 'external']),
  scope: RustSignalScopeSchema,
  lane: z.optional(z.string()),
  live: z.boolean(),
  quality: z.object({ state: z.string(), reasons: z.array(z.string()) }),
  ts_ms: z.optional(unixMs),
  /** When the signal last became ready; absent while warming up, degraded or never published. */
  ready_since_ms: z.optional(unixMs),
  fields: z.array(RustSignalFieldSchema),
})

/** One published value set, as `/ws/signals` and the bus carry it. */
export const RustSignalFrameSchema = z.object({
  signal_id: z.string(),
  ts_ms: unixMs,
  quality: z.object({ state: z.string(), reasons: z.array(z.string()) }),
  values: z.record(z.string(), z.union([z.string(), z.number(), z.boolean()])),
})

/** One signal's retained frames, oldest first, as the socket and the history route carry them. */
export const RustSignalHistorySchema = z.object({
  signal_id: z.string(),
  frames: z.array(RustSignalFrameSchema),
})

/**
 * `/ws/signals`: a `snapshot` answers each subscribe, then a coalesced `batch` per tick. A
 * subscribe carrying `history_ms` is answered first with a `history` of retained frames.
 */
export const RustSignalsMessageSchema = z.discriminatedUnion('kind', [
  z.object({ kind: z.literal('snapshot'), frames: z.array(RustSignalFrameSchema) }),
  z.object({ kind: z.literal('history'), signals: z.array(RustSignalHistorySchema) }),
  z.object({ kind: z.literal('batch'), frames: z.array(RustSignalFrameSchema) }),
  z.object({ kind: z.literal('error'), message: z.string() }),
])

export const RustSignalCatalogueSchema = z.object({
  series: RustSeriesSlugSchema,
  underlying: z.string(),
  signals: z.array(RustSignalSchema),
})

/** `GET /api/signals/history?series=&ids=&history_ms=&max_points=`. */
export const RustSignalsHistorySchema = z.object({
  series: RustSeriesSlugSchema,
  since_ms: unixMs,
  signals: z.array(RustSignalHistorySchema),
})

const RustUserOrderStatusSchema = z.enum([
  'filled',
  'partially_filled',
  'rested',
  'killed',
  'cancelled',
])

/** One of the caller's orders, as `GET /api/accounts/{address}/trades` lists them. */
export const RustUserTradeRowSchema = z.object({
  series: RustSeriesSlugSchema,
  order_id: z.int(),
  ts: unixMs,
  /** `yes` bought YES; `no` sold YES or bought NO. */
  side: z.enum(['yes', 'no']),
  qty_requested: z.int(),
  filled_qty: z.int(),
  avg_price: z.optional(RustPriceTicksSchema),
  limit_price: z.optional(RustPriceTicksSchema),
  source: z.enum(['ui', 'strategy', 'bot']),
  order_type: z.enum(['market', 'limit']),
  status: RustUserOrderStatusSchema,
  market_id: z.int(),
  strategy_id: z.optional(RustAddressSchema),
  strategy_name: z.optional(z.string()),
  realized_pnl_tenths: tenths,
  settled: z.boolean(),
})
export const RustUserTradesSchema = z.object({ trades: z.array(RustUserTradeRowSchema) })
/** A signal a template side reads that the exchange's strategy platform cannot act on yet. */
const RustWaitingSignalSchema = z.object({
  signal_id: z.string(),
  /** The registry's readable name. */
  name: z.string(),
})

/**
 * Readiness of a template side or card. A deployment whose signal is warming up, degraded or
 * unpublished never fires and never errors, so the exchange says so up front. Absent from an
 * exchange that predates the fields, which then reads as ready.
 */
const templateReadiness = {
  ready: z.optional(z.boolean()),
  waiting_on: z.optional(z.array(RustWaitingSignalSchema)),
}

/** One side of a public strategy template: what Run (`primary`) or Inverse (`inverse`) deploys. */
export const RustStrategyTemplateSideSchema = z.object({
  name: z.string(),
  logic: z.string(),
  running: z.int(),
  deployed_tenths: tenths,
  ...templateReadiness,
})

/** `GET /api/strategies/templates/{id}?series=`: one public strategy card. */
export const RustStrategyTemplateSchema = z.object({
  id: z.string(),
  title: z.string(),
  series: z.array(RustSeriesSlugSchema),
  creator: z.object({
    handle: z.string(),
    username: z.string(),
    /** Portrait filename under the app's market avatars folder; null while unassigned. */
    avatar: z.nullable(z.string()),
  }),
  envelope: z.object({
    loss_budget_fraction: z.string(),
    /** Dollars per order as a fraction of the allocation. */
    order_fraction: z.optional(z.string()),
    max_order_size: z.string(),
    max_open_orders: z.optional(z.string()),
    max_position_per_market: z.optional(z.string()),
  }),
  primary: RustStrategyTemplateSideSchema,
  inverse: RustStrategyTemplateSideSchema,
  running: z.int(),
  deployed_tenths: tenths,
  /** Portrait filenames of a few accounts running it. */
  runners: z.array(z.string()),
  /** Both sides ready; `waiting_on` is the union of both sides' waiting signals. */
  ...templateReadiness,
})

/** `GET /api/strategies/templates?series=`: every template listed on the series. */
export const RustStrategyTemplateCatalogueSchema = z.object({
  series: RustSeriesSlugSchema,
  templates: z.array(RustStrategyTemplateSchema),
})

const RustStrategyStatusSchema = z.enum(['active', 'paused', 'archived'])

/** One series' slice of a strategy's exposure, from the account row. */
const RustStrategySeriesPositionSchema = z.object({
  series: RustSeriesSlugSchema,
  market_id: z.nullable(z.int()),
  side: z.enum(['yes', 'no', 'flat']),
  size_shares: z.int(),
  signed_position: z.int(),
  avg_entry_yes_tenths: z.nullable(z.int()),
  cost_basis_tenths: tenths,
  market_pnl_tenths: tenths,
  trades_this_market: z.int(),
})

/**
 * `GET /api/accounts/{address}/strategies`: one row per live strategy sub-account (the
 * `AccountRow` of `api/accounts.rs` restricted to strategies). Money is tenths of a cent.
 */
export const RustStrategySummarySchema = z.object({
  id: RustAddressSchema,
  kind: z.literal('strategy'),
  name: z.string(),
  status: z.nullable(RustStrategyStatusSchema),
  created_at_ms: z.nullable(unixMs),
  home_series: z.nullable(RustSeriesSlugSchema),
  trades_this_market: z.int(),
  series_positions: z.array(RustStrategySeriesPositionSchema),
  balance_tenths: tenths,
  locked_tenths: tenths,
  equity_tenths: tenths,
  net_contributed_tenths: tenths,
  lifetime_pnl_tenths: tenths,
  lifetime_realized_pnl_tenths: tenths,
})

export const RustStrategyListSchema = z.object({
  strategies: z.array(RustStrategySummarySchema),
})

/** The stored record as `GET /api/accounts/{address}/strategies/{id}` and a deploy return it. */
export const RustStrategyRecordSchema = z.object({
  name: z.string(),
  status: RustStrategyStatusSchema,
  created_at_ms: unixMs,
  series: RustSeriesSlugSchema,
  strategy_id: z.string(),
  deploy_epoch: z.int(),
  deployment_id: z.string(),
  /** The public template this runs; absent for an authored document. */
  template_id: z.optional(z.string()),
  inverse: z.boolean(),
})

/** The deployment as the runtime reports it; absent once it has failed and been reaped. */
export const RustStrategyRuntimeSchema = z.object({
  current_leg: z.string(),
  armed_triggers: z.array(z.string()),
  paused: z.boolean(),
  stopping: z.boolean(),
  failure_reason: z.nullable(z.string()),
  activations: z.int(),
  log: z.array(z.string()),
})

/** A deploy's answer: the summary row, the stored record and the live runtime status. */
export const RustStrategyDetailSchema = z.object({
  ...RustStrategySummarySchema.shape,
  strategy: RustStrategyRecordSchema,
  runtime: z.optional(RustStrategyRuntimeSchema),
})

/** `POST /api/accounts/{address}/templates/{id}/deploy` body. */
export interface RustDeployTemplateRequest {
  /** Cash moved into the new sub-account; also the envelope's allocation. */
  allocation_tenths: number
  /** `true` deploys the template's Inverse document. */
  inverse: boolean
  series: string
}

export const RustCreateAccountResponseSchema = z.object({
  address: RustAddressSchema,
  username: z.string(),
  color: z.string(),
  emoji: z.string(),
  usdc_balance: tenths,
})

/** `GET /api/accounts/{address}`; position fields describe the default series only. */
export const RustAccountSchema = z.object({
  address: RustAddressSchema,
  username: z.optional(z.string()),
  color: z.optional(z.string()),
  emoji: z.optional(z.string()),
  usdc_balance: tenths,
  locked: tenths,
})

export const RustPlaceOutcomeSchema = z.object({
  order_id: z.int(),
  fills: z.array(RustFillSchema),
  status: z.enum(['filled', 'partially_filled', 'rested', 'killed']),
  resting_qty: z.int(),
})

export const RustApiErrorSchema = z.object({
  error: z.string(),
  message: z.string(),
  detail: z.optional(z.unknown()),
})

// ─── WebSocket frames ─────────────────────────────────────────────────────────

const RustTickSchema = z.object({ ts_ms: unixMs, price: z.number() })

/** The price socket's first frame; `GET /api/price/history?series=&max_points=` serves it too. */
export const RustPriceSnapshotSchema = z.object({
  type: z.literal('snapshot'),
  series: RustSeriesSlugSchema,
  duration_secs: z.int(),
  market_id: z.nullable(z.int()),
  strike: z.nullable(z.number()),
  opens_at_ms: z.nullable(unixMs),
  closes_at_ms: z.nullable(unixMs),
  ticks: z.array(RustTickSchema),
})

/** `/ws/price`: one snapshot on connect, then ticks and round resets. */
export const RustPriceFrameSchema = z.union([
  RustPriceSnapshotSchema,
  z.object({ type: z.literal('tick'), ts_ms: unixMs, price: z.number() }),
  z.object({
    type: z.literal('market_reset'),
    series: RustSeriesSlugSchema,
    market_id: z.int(),
    strike: z.number(),
    opens_at_ms: unixMs,
    closes_at_ms: unixMs,
    duration_secs: z.int(),
  }),
])

const RustBookLevelSchema = z.object({ price: RustPriceTicksSchema, qty: z.int() })

/** One sample of the round's YES midpoint, in tenths of a cent. */
const RustMidPointSchema = z.object({ ts_ms: unixMs, mid: RustPriceTicksSchema })

/**
 * `/ws/book`: a full 20-level snapshot per side on connect and on every change. Every frame
 * carries the open round and the YES midpoint the book implies (`mid` is null on an empty
 * book); the frame a client receives on join also carries the round's midpoint history, the
 * same points `GET /api/book/mid-history` serves.
 */
export const RustBookFrameSchema = z.object({
  type: z.literal('snapshot'),
  series: RustSeriesSlugSchema,
  market_id: z.optional(z.nullable(z.int())),
  mid: z.optional(z.nullable(RustPriceTicksSchema)),
  mid_history: z.optional(z.array(RustMidPointSchema)),
  bids: z.array(RustBookLevelSchema),
  asks: z.array(RustBookLevelSchema),
})

const RustTradeIdentitySchema = z.object({
  address: RustAddressSchema,
  root_address: RustAddressSchema,
  username: z.optional(z.string()),
  color: z.optional(z.string()),
  emoji: z.optional(z.string()),
})

export const RustTradeRowSchema = z.object({
  series: RustSeriesSlugSchema,
  price: RustPriceTicksSchema,
  qty: z.int(),
  ts: unixMs,
  side: z.enum(['yes', 'no']),
  taker: RustTradeIdentitySchema,
  maker: RustTradeIdentitySchema,
})

/** `/ws/trades`: a snapshot on connect, then one batch per taker order. */
export const RustTradesFrameSchema = z.object({
  type: z.enum(['snapshot', 'batch']),
  trades: z.array(RustTradeRowSchema),
})

const RustPositionSideSchema = z.enum(['yes', 'no', 'flat'])

const RustSeriesPositionRowSchema = z.object({
  series: RustSeriesSlugSchema,
  market_id: z.nullable(z.int()),
  side: RustPositionSideSchema,
  size_shares: z.int(),
  signed_position: z.int(),
  avg_entry_yes_tenths: z.nullable(z.int()),
  cost_basis_tenths: tenths,
  mark_yes_tenths: z.nullable(z.int()),
  /** What the position is worth at the mark; `null` only while nothing can mark it. */
  mark_value_tenths: z.nullable(tenths),
  unrealized_pnl_tenths: z.nullable(tenths),
  /** This round only: realized so far plus the open position marked to the book. */
  market_pnl_tenths: tenths,
})

/**
 * `/ws/positions?address=&series=`: rebuilt on every book or trade broadcast. The same
 * snapshot answers `GET /api/accounts/{address}/positions?series=` once.
 */
export const RustPositionFrameSchema = z.object({
  type: z.literal('position_snapshot'),
  as_of_unix_ms: unixMs,
  series: RustSeriesSlugSchema,
  market_id: z.nullable(z.int()),
  address: RustAddressSchema,
  side: RustPositionSideSchema,
  size_shares: z.int(),
  signed_position: z.int(),
  avg_entry_yes_tenths: z.nullable(z.int()),
  cost_basis_tenths: tenths,
  mark_yes_tenths: z.nullable(z.int()),
  unrealized_pnl_tenths: z.nullable(tenths),
  realized_pnl_this_market_tenths: tenths,
  balance_tenths: tenths,
  locked_tenths: tenths,
  series_positions: z.array(RustSeriesPositionRowSchema),
})

/** One of the user's accounts on `/ws/accounts`: the main account or a live strategy. */
export const RustAccountRowSchema = z.object({
  /** The account address; for a strategy this is also its id. */
  id: RustAddressSchema,
  kind: z.enum(['main', 'strategy']),
  /** Strategy name, or the username (fallback "Main") on the main row. */
  name: z.string(),
  status: z.nullable(z.enum(['active', 'paused', 'archived'])),
  /** The one series a strategy trades; `null` on the main row. */
  home_series: z.nullable(RustSeriesSlugSchema),
  trades_this_market: z.int(),
  series_positions: z.array(RustSeriesPositionRowSchema),
  balance_tenths: tenths,
  locked_tenths: tenths,
  /** Σ (realized this round + unrealized) over every series. */
  market_pnl_total_tenths: tenths,
  /** `equity − net contributed`, live. */
  lifetime_pnl_tenths: tenths,
})

/**
 * The bankroll a strategy is compiled and risk-gated against (`strategy_compiler::ir::Envelope`).
 * Dollars and contract counts as canonical decimal strings; the optional caps are omitted, not
 * null, on the wire.
 */
export const RustStrategyEnvelopeSchema = z.object({
  allocation: z.string(),
  loss_budget: z.string(),
  round_budget: z.optional(z.string()),
  max_order_size: z.string(),
  max_exposure_fraction: z.optional(z.string()),
  pause_at_consumed: z.optional(z.string()),
  max_position_per_market: z.optional(z.string()),
  max_open_orders: z.optional(z.string()),
})

/** `POST /api/accounts/{parent}/strategies` request body. */
export const RustCreateStrategyRequestSchema = z.object({
  /** Cash moved from the parent into the new sub-account, tenths of a cent. */
  allocation_tenths: tenths,
  name: z.optional(z.string()),
  /** The authored IR document, exactly as the compiler accepted it. */
  strategy: z.record(z.string(), z.unknown()),
  envelope: RustStrategyEnvelopeSchema,
  params: z.optional(z.record(z.string(), z.unknown())),
})

/**
 * `POST /api/accounts/{parent}/strategies` answer: the new sub-account's row flattened with the
 * stored record and the runtime status. Only what the deploy screens read is pinned.
 */
export const RustCreateStrategyResponseSchema = z.object({
  /** The sub-account address, which is also the strategy id. */
  id: RustAddressSchema,
  kind: z.literal('strategy'),
  name: z.string(),
  status: z.nullable(z.enum(['active', 'paused', 'archived'])),
  home_series: z.nullable(RustSeriesSlugSchema),
  strategy: z.object({ series: z.string(), strategy_id: z.string(), deployment_id: z.string() }),
})

/** A resting order owned by the user or one of their strategies. Cancel as `account`. */
export const RustOpenOrderRowSchema = z.object({
  series: RustSeriesSlugSchema,
  order_id: z.int(),
  market_id: z.int(),
  account: RustAddressSchema,
  account_kind: z.enum(['main', 'strategy']),
  account_name: z.string(),
  /** `bid` buys YES; `ask` sells YES or buys NO. */
  side: z.enum(['bid', 'ask']),
  price: RustPriceTicksSchema,
  qty_open: z.int(),
  placed_ts: z.int(),
})

/**
 * `/ws/accounts?address=`: the main account, every live strategy, every resting order
 * they hold on any book, and their merged order log (the user-trades rows), newest first.
 * Rebuilt on every broadcast and pushed only when something changed.
 */
export const RustAccountsFrameSchema = z.object({
  type: z.literal('accounts_snapshot'),
  as_of_unix_ms: unixMs,
  address: RustAddressSchema,
  main: RustAccountRowSchema,
  strategies: z.array(RustAccountRowSchema),
  open_orders: z.array(RustOpenOrderRowSchema),
  trades: z.array(RustUserTradeRowSchema),
})

export type RustSeriesInfo = z.infer<typeof RustSeriesInfoSchema>
export type RustGeminiSeries = z.infer<typeof RustGeminiSeriesSchema>
export type RustSealView = z.infer<typeof RustSealViewSchema>
export type RustRecordView = z.infer<typeof RustRecordViewSchema>
export type RustBtcRoundReference = z.infer<typeof RustBtcRoundReferenceSchema>
export type RustNbaRoundReference = z.infer<typeof RustNbaRoundReferenceSchema>
export type RustMarket = z.infer<typeof RustMarketSchema>
export type RustMarketStats = z.infer<typeof RustMarketStatsSchema>
export type RustSignal = z.infer<typeof RustSignalSchema>
export type RustSignalField = z.infer<typeof RustSignalFieldSchema>
export type RustSignalCatalogue = z.infer<typeof RustSignalCatalogueSchema>
export type RustSignalFrame = z.infer<typeof RustSignalFrameSchema>
export type RustSignalHistory = z.infer<typeof RustSignalHistorySchema>
export type RustSignalsHistory = z.infer<typeof RustSignalsHistorySchema>
export type RustSignalsMessage = z.infer<typeof RustSignalsMessageSchema>
export type RustUserTradeRow = z.infer<typeof RustUserTradeRowSchema>
export type RustUserTrades = z.infer<typeof RustUserTradesSchema>
export type RustPriceSnapshot = z.infer<typeof RustPriceSnapshotSchema>
export type RustFill = z.infer<typeof RustFillSchema>
export type RustStrategyTemplate = z.infer<typeof RustStrategyTemplateSchema>
export type RustStrategyTemplateCatalogue = z.infer<typeof RustStrategyTemplateCatalogueSchema>
export type RustStrategySummary = z.infer<typeof RustStrategySummarySchema>
export type RustStrategyList = z.infer<typeof RustStrategyListSchema>
export type RustStrategyDetail = z.infer<typeof RustStrategyDetailSchema>
export type RustCreateAccountResponse = z.infer<typeof RustCreateAccountResponseSchema>
export type RustAccount = z.infer<typeof RustAccountSchema>
export type RustPlaceOutcome = z.infer<typeof RustPlaceOutcomeSchema>
export type RustApiError = z.infer<typeof RustApiErrorSchema>
export type RustPriceFrame = z.infer<typeof RustPriceFrameSchema>
export type RustBookFrame = z.infer<typeof RustBookFrameSchema>
export type RustTradeRow = z.infer<typeof RustTradeRowSchema>
export type RustTradesFrame = z.infer<typeof RustTradesFrameSchema>
export type RustPositionFrame = z.infer<typeof RustPositionFrameSchema>
export type RustAccountRow = z.infer<typeof RustAccountRowSchema>
export type RustStrategyEnvelope = z.infer<typeof RustStrategyEnvelopeSchema>
export type RustCreateStrategyRequest = z.infer<typeof RustCreateStrategyRequestSchema>
export type RustCreateStrategyResponse = z.infer<typeof RustCreateStrategyResponseSchema>
export type RustOpenOrderRow = z.infer<typeof RustOpenOrderRowSchema>
export type RustAccountsFrame = z.infer<typeof RustAccountsFrameSchema>

/** Tenths of a cent to the micro-dollar strings the exchange screen formats. */
export function tenthsToMicros(value: number): string {
  return String(BigInt(value) * 1000n)
}
