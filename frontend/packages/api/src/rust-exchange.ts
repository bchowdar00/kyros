import {
  RustAccountSchema,
  RustApiErrorSchema,
  RustBookFrameSchema,
  RustCreateAccountResponseSchema,
  RustCreateStrategyResponseSchema,
  RustFillSchema,
  RustMarketStatsSchema,
  RustMarketsHistorySchema,
  RustPlaceOutcomeSchema,
  RustPositionFrameSchema,
  RustPriceSnapshotSchema,
  RustRecordViewSchema,
  RustSeriesListSchema,
  RustSignalCatalogueSchema,
  RustSignalsHistorySchema,
  RustStrategyDetailSchema,
  RustStrategyListSchema,
  RustUserTradesSchema,
  RustStrategyTemplateCatalogueSchema,
  RustStrategyTemplateSchema,
  type RustAccount,
  type RustBookFrame,
  type RustCreateAccountResponse,
  type RustDeployTemplateRequest,
  type RustCreateStrategyRequest,
  type RustCreateStrategyResponse,
  type RustFill,
  type RustMarket,
  type RustMarketStats,
  type RustPlaceOutcome,
  type RustPositionFrame,
  type RustPriceSnapshot,
  type RustRecordView,
  type RustSeriesInfo,
  type RustSignalCatalogue,
  type RustSignalsHistory,
  type RustStrategyDetail,
  type RustStrategyList,
  type RustUserTrades,
  type RustStrategyTemplate,
  type RustStrategyTemplateCatalogue,
} from '@kyros/domain'
import * as z from 'zod/mini'

import { type ApiClientOptions, createApiClient } from './client'
import { ApiError, type ApiResult, isRetryableStatus } from './errors'
import { DEFAULT_RETRY_POLICY, defaultSleep, retryDelayMs, type RetryPolicy } from './retry'
import type { InferOutput, StandardSchema } from './schema'

export interface RustPlaceOrderInput {
  address: string
  series: string
  side: 'bid' | 'ask'
  /** YES price in tenths of a cent on the tick grid. Omit for a market order. */
  price?: number
  qty: number
}

export interface RustSignalsHistoryInput {
  series: string
  /** Absent means every signal of the series. */
  ids?: readonly string[]
  historyMs?: number
  maxPoints?: number
}

export interface RustExchangeClient {
  listSeries(signal?: AbortSignal): Promise<ApiResult<RustSeriesInfo[]>>
  getMarketsHistory(
    series: string,
    limit: number,
    signal?: AbortSignal
  ): Promise<ApiResult<RustMarket[]>>
  getMarketStats(series: string, signal?: AbortSignal): Promise<ApiResult<RustMarketStats>>
  getSignals(series: string, signal?: AbortSignal): Promise<ApiResult<RustSignalCatalogue>>
  getSignalsHistory(
    input: RustSignalsHistoryInput,
    signal?: AbortSignal
  ): Promise<ApiResult<RustSignalsHistory>>
  getPriceHistory(
    series: string,
    maxPoints?: number,
    signal?: AbortSignal
  ): Promise<ApiResult<RustPriceSnapshot>>
  /** The canonical YES book, best first each side, `depth` levels (default 20, up to 153). */
  getBook(
    series: string,
    depth?: number,
    signal?: AbortSignal
  ): Promise<ApiResult<Pick<RustBookFrame, 'bids' | 'asks'>>>
  /** The most recent engine fills in the series, newest first (default 50, up to 1000). */
  getTrades(series: string, limit?: number, signal?: AbortSignal): Promise<ApiResult<RustFill[]>>
  getUserTrades(address: string, signal?: AbortSignal): Promise<ApiResult<RustUserTrades>>
  getPositions(
    address: string,
    series?: string,
    signal?: AbortSignal
  ): Promise<ApiResult<RustPositionFrame>>
  getStrategyTemplates(
    series: string,
    signal?: AbortSignal
  ): Promise<ApiResult<RustStrategyTemplateCatalogue>>
  getStrategyTemplate(
    id: string,
    series: string,
    signal?: AbortSignal
  ): Promise<ApiResult<RustStrategyTemplate>>
  createAccount(username: string): Promise<ApiResult<RustCreateAccountResponse>>
  /**
   * Compile `strategy` against `envelope`, fund a sub-account of `parent` with
   * `allocation_tenths` and start the deployment. A compile failure is a 400 that moves no money.
   */
  createStrategy(
    parent: string,
    input: RustCreateStrategyRequest
  ): Promise<ApiResult<RustCreateStrategyResponse>>
  getAccount(address: string, signal?: AbortSignal): Promise<ApiResult<RustAccount>>
  /**
   * A deployment's on-chain record, by deployment id or by sub-account address. A deployment
   * the exchange holds no record for answers 404.
   */
  getRecord(id: string, signal?: AbortSignal): Promise<ApiResult<RustRecordView>>
  placeOrder(input: RustPlaceOrderInput): Promise<ApiResult<RustPlaceOutcome>>
  cancelOrder(orderId: number, address: string, series: string): Promise<ApiResult<undefined>>
  listStrategies(address: string, signal?: AbortSignal): Promise<ApiResult<RustStrategyList>>
  /**
   * Deploy one side of a public template into a new funded sub-account. Moves money, so it
   * carries an idempotency key: the exchange answers a repeat of the same key with the first
   * deployment instead of funding a second one, which is what makes the retry safe.
   */
  deployTemplate(
    address: string,
    templateId: string,
    body: RustDeployTemplateRequest,
    idempotencyKey: string
  ): Promise<ApiResult<RustStrategyDetail>>
  /**
   * Pause or resume a deployed strategy. `parent` is the owning account, `id` the strategy's
   * sub-account address. A status change pauses or resumes the deployment; returns its detail.
   */
  updateStrategyStatus(
    parent: string,
    id: string,
    status: 'active' | 'paused'
  ): Promise<ApiResult<RustStrategyDetail>>
  /**
   * Stop a deployed strategy: cancel its resting orders, flatten its position and sweep its cash
   * back to the parent. Returns what was swept and whether the sub-account is gone immediately.
   */
  stopStrategy(parent: string, id: string): Promise<ApiResult<RustDeleteStrategyResponse>>
}

const MUTATION_TIMEOUT_MS = 5_000

/** `GET /api/book` answers the socket's levels without the frame envelope. */
const RustBookSnapshotSchema = z.pick(RustBookFrameSchema, { bids: true, asks: true })
/** `GET /api/trades` is a bare array of engine fills, unlike the socket's collapsed rows. */
const RustTradesSchema = z.array(RustFillSchema)

/**
 * `DELETE /api/accounts/{parent}/strategies/{id}` report. Money is tenths of a cent. When the
 * position could not be flattened (no liquidity) the sub-account is left for the resolution
 * sweep: `removed` is false, `swept_tenths` is 0 and `residual_position` is the signed size left.
 */
const RustDeleteStrategyResponseSchema = z.object({
  swept_tenths: z.int(),
  cancelled_orders: z.int().check(z.minimum(0)),
  residual_position: z.int(),
  removed: z.boolean(),
})
export type RustDeleteStrategyResponse = z.infer<typeof RustDeleteStrategyResponseSchema>

/**
 * The exchange answers errors as `{error, message}` with a 4xx status; the generic client
 * discards that body, so mutations parse it here and surface the server's `error` code as
 * the ApiError message prefix for callers to branch on.
 *
 * A mutation is attempted exactly once unless it carries an idempotency key, in which case a
 * network failure, a timeout or a retryable status is retried under the client's policy and
 * wall-clock budget, the same rule the generic client applies.
 */
async function mutate<Schema extends StandardSchema>(
  options: ApiClientOptions,
  build: () => Request,
  schema: Schema | undefined,
  idempotencyKey?: string
): Promise<ApiResult<InferOutput<Schema>>> {
  const policy: RetryPolicy = { ...DEFAULT_RETRY_POLICY, ...options.retry }
  const sleep = options.sleep ?? defaultSleep
  const startedAt = Date.now()
  let attemptIndex = 0
  for (;;) {
    const result = await mutateOnce(options, build(), schema)
    if (result.ok || idempotencyKey === undefined || !result.error.retryable) return result
    const delay = retryDelayMs(attemptIndex, policy)
    const isLastAttempt = attemptIndex + 1 >= policy.attempts
    const wouldExceedBudget = Date.now() - startedAt + delay > policy.budgetMs
    if (isLastAttempt || wouldExceedBudget) return result
    await sleep(delay)
    attemptIndex += 1
  }
}

async function mutateOnce<Schema extends StandardSchema>(
  options: ApiClientOptions,
  request: Request,
  schema: Schema | undefined
): Promise<ApiResult<InferOutput<Schema>>> {
  const fetchImpl = options.fetch ?? globalThis.fetch
  let response: Response
  try {
    response = await fetchImpl(request)
  } catch (cause) {
    const isTimeout = cause instanceof DOMException && cause.name === 'TimeoutError'
    return {
      ok: false,
      error: new ApiError(
        isTimeout ? 'timeout' : 'network',
        isTimeout ? 'Exchange did not answer in time' : 'Exchange unreachable',
        { cause, retryable: true }
      ),
    }
  }
  let payload: unknown = undefined
  const text = await response.text()
  if (text.length > 0) {
    try {
      payload = JSON.parse(text)
    } catch (cause) {
      return {
        ok: false,
        error: new ApiError('parse', 'Exchange response is not JSON', { cause, retryable: false }),
      }
    }
  }
  if (!response.ok) {
    const failure = RustApiErrorSchema.safeParse(payload)
    const message = failure.success
      ? `${failure.data.error}: ${failure.data.message}`
      : `${response.status} ${response.statusText}`.trim()
    return {
      ok: false,
      error: new ApiError('http', message, {
        retryable: isRetryableStatus(response.status),
        status: response.status,
      }),
    }
  }
  if (schema === undefined) {
    // Empty-bodied 2xx (cancel). The caller's declared output type is `undefined`.
    return { ok: true, data: undefined as InferOutput<Schema> }
  }
  const result = await schema['~standard'].validate(payload)
  if (result.issues) {
    return {
      ok: false,
      error: new ApiError('parse', 'Exchange response did not match its contract', {
        retryable: false,
        status: response.status,
      }),
    }
  }
  return { ok: true, data: result.value as InferOutput<Schema> }
}

export function createRustExchangeClient(options: ApiClientOptions): RustExchangeClient {
  const client = createApiClient(options)
  const base = options.baseUrl.endsWith('/') ? options.baseUrl : `${options.baseUrl}/`
  const url = (path: string, query?: Record<string, string>) => {
    const target = new URL(path, base)
    for (const [key, value] of Object.entries(query ?? {})) target.searchParams.set(key, value)
    return target
  }
  const jsonRequest = (target: URL, method: string, body?: unknown, idempotencyKey?: string) =>
    new Request(target, {
      method,
      headers: {
        accept: 'application/json',
        'content-type': 'application/json',
        ...(idempotencyKey === undefined ? {} : { 'idempotency-key': idempotencyKey }),
      },
      ...(body === undefined ? {} : { body: JSON.stringify(body) }),
      signal: AbortSignal.timeout(options.timeoutMs ?? MUTATION_TIMEOUT_MS),
    })

  return {
    listSeries(signal) {
      return client.get('api/series', {
        schema: RustSeriesListSchema,
        ...(signal === undefined ? {} : { signal }),
      })
    },
    getMarketsHistory(series, limit, signal) {
      return client.get('api/markets/history', {
        schema: RustMarketsHistorySchema,
        searchParams: { series, limit },
        ...(signal === undefined ? {} : { signal }),
      })
    },
    getMarketStats(series, signal) {
      return client.get('api/market/stats', {
        schema: RustMarketStatsSchema,
        searchParams: { series },
        ...(signal === undefined ? {} : { signal }),
      })
    },
    getSignals(series, signal) {
      return client.get('api/signals', {
        schema: RustSignalCatalogueSchema,
        searchParams: { series },
        ...(signal === undefined ? {} : { signal }),
      })
    },
    getSignalsHistory(input, signal) {
      return client.get('api/signals/history', {
        schema: RustSignalsHistorySchema,
        searchParams: {
          series: input.series,
          ids: input.ids?.join(','),
          history_ms: input.historyMs,
          max_points: input.maxPoints,
        },
        ...(signal === undefined ? {} : { signal }),
      })
    },
    getPriceHistory(series, maxPoints, signal) {
      return client.get('api/price/history', {
        schema: RustPriceSnapshotSchema,
        searchParams: { series, max_points: maxPoints },
        ...(signal === undefined ? {} : { signal }),
      })
    },
    getBook(series, depth, signal) {
      return client.get('api/book', {
        schema: RustBookSnapshotSchema,
        searchParams: { series, depth },
        ...(signal === undefined ? {} : { signal }),
      })
    },
    getTrades(series, limit, signal) {
      return client.get('api/trades', {
        schema: RustTradesSchema,
        searchParams: { series, limit },
        ...(signal === undefined ? {} : { signal }),
      })
    },
    getUserTrades(address, signal) {
      return client.get(`api/accounts/${address}/trades`, {
        schema: RustUserTradesSchema,
        ...(signal === undefined ? {} : { signal }),
      })
    },
    getPositions(address, series, signal) {
      return client.get(`api/accounts/${address}/positions`, {
        schema: RustPositionFrameSchema,
        searchParams: { series },
        ...(signal === undefined ? {} : { signal }),
      })
    },
    getStrategyTemplates(series, signal) {
      return client.get('api/strategies/templates', {
        schema: RustStrategyTemplateCatalogueSchema,
        searchParams: { series },
        ...(signal === undefined ? {} : { signal }),
      })
    },
    getStrategyTemplate(id, series, signal) {
      return client.get(`api/strategies/templates/${encodeURIComponent(id)}`, {
        schema: RustStrategyTemplateSchema,
        searchParams: { series },
        ...(signal === undefined ? {} : { signal }),
      })
    },
    getAccount(address, signal) {
      return client.get(`api/accounts/${address}`, {
        schema: RustAccountSchema,
        ...(signal === undefined ? {} : { signal }),
      })
    },
    getRecord(id, signal) {
      return client.get(`api/records/${id}`, {
        schema: RustRecordViewSchema,
        ...(signal === undefined ? {} : { signal }),
      })
    },
    listStrategies(address, signal) {
      return client.get(`api/accounts/${address}/strategies`, {
        schema: RustStrategyListSchema,
        ...(signal === undefined ? {} : { signal }),
      })
    },
    createAccount(username) {
      return mutate(
        options,
        () => jsonRequest(url('api/accounts'), 'POST', { username }),
        RustCreateAccountResponseSchema
      )
    },
    deployTemplate(address, templateId, body, idempotencyKey) {
      const target = url(
        `api/accounts/${address}/templates/${encodeURIComponent(templateId)}/deploy`
      )
      return mutate(
        options,
        () => jsonRequest(target, 'POST', body, idempotencyKey),
        RustStrategyDetailSchema,
        idempotencyKey
      )
    },
    createStrategy(parent, input) {
      return mutate(
        options,
        () => jsonRequest(url(`api/accounts/${parent}/strategies`), 'POST', input),
        RustCreateStrategyResponseSchema
      )
    },
    placeOrder(input) {
      const body =
        input.price === undefined
          ? {
              type: 'market',
              address: input.address,
              side: input.side,
              qty: input.qty,
              series: input.series,
            }
          : {
              type: 'limit',
              address: input.address,
              side: input.side,
              price: input.price,
              qty: input.qty,
              series: input.series,
            }
      return mutate(
        options,
        () => jsonRequest(url('api/orders'), 'POST', body),
        RustPlaceOutcomeSchema
      )
    },
    cancelOrder(orderId, address, series) {
      return mutate<StandardSchema<unknown, undefined>>(
        options,
        () => jsonRequest(url(`api/orders/${orderId}`, { address, series }), 'DELETE'),
        undefined
      )
    },
    updateStrategyStatus(parent, id, status) {
      return mutate(
        options,
        () => jsonRequest(url(`api/accounts/${parent}/strategies/${id}`), 'PATCH', { status }),
        RustStrategyDetailSchema
      )
    },
    stopStrategy(parent, id) {
      return mutate(
        options,
        () => jsonRequest(url(`api/accounts/${parent}/strategies/${id}`), 'DELETE'),
        RustDeleteStrategyResponseSchema
      )
    },
  }
}
