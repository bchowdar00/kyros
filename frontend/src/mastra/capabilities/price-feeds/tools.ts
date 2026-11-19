import { createTool } from '@mastra/core/tools'
import { createPriceFeedsClient } from '@kyros/api'
import { FeedSymbolSchema } from '@kyros/domain'
import * as z from 'zod/mini'

// Local chat policy, not a settlement guarantee or a provider SLA.
const MAX_LIVE_AGE_MS = 5_000
const MAX_FUTURE_SKEW_MS = 1_000
const REQUEST_TIMEOUT_MS = 5_000
const MAX_HISTORY_MINUTES = 1_440

export function createPriceFeedTools(baseUrl: string | undefined) {
  const client = baseUrl
    ? createPriceFeedsClient({
        baseUrl,
        timeoutMs: REQUEST_TIMEOUT_MS,
        retry: { attempts: 1 },
        fetch: (input, init) => fetch(input, { ...init, cache: 'no-store', redirect: 'error' }),
      })
    : undefined
  const unavailable = { status: 'unavailable', reason: 'Price Feeds is not configured.' }

  const listPriceAssets = createTool({
    id: 'list-price-assets',
    description:
      'Discover active underlying asset symbols available from Kyros Price Feeds. Use before price reads to resolve symbols; active does not guarantee fresh observations.',
    inputSchema: z.object({}),
    execute: async (_input, context) => {
      if (!client) return unavailable
      const response = await client.listAssets(context?.abortSignal)
      if (!response.ok)
        return { status: 'unavailable', reason: 'Could not retrieve supported assets.' }
      return {
        status: 'ok',
        source: 'Kyros Price Feeds',
        retrievedAt: new Date().toISOString(),
        assets: response.data.assets,
      }
    },
  })

  const getLivePrice = createTool({
    id: 'get-live-price',
    description:
      'Read one fresh underlying asset observation from Kyros Price Feeds SSE, then disconnect. Not Kyros market odds, a settlement price, or continuous monitoring. Use a canonical symbol from listPriceAssets.',
    inputSchema: z.object({ symbol: FeedSymbolSchema }),
    execute: async ({ symbol }, context) => {
      if (!client) return unavailable
      const response = await client.getLivePrice(symbol, context?.abortSignal)
      if (!response.ok)
        return {
          status: 'unavailable',
          symbol,
          reason: 'No valid live observation received within the request budget.',
        }
      const tick = response.data
      const observedMs = Math.floor(tick.publishTimeUs / 1000)
      const now = Date.now()
      const observedAt = new Date(observedMs).toISOString()
      if (now - observedMs > MAX_LIVE_AGE_MS || observedMs - now > MAX_FUTURE_SKEW_MS) {
        return {
          status: 'unavailable',
          symbol,
          observedAt,
          reason:
            'Observation is stale or its timestamp is in the future. Do not quote it as a current price.',
        }
      }
      return {
        status: 'ok',
        source: 'Kyros Price Feeds',
        retrievedAt: new Date(now).toISOString(),
        observedAt,
        ageMs: Math.max(0, now - observedMs),
        priceEncoding: 'Exact price = mantissa × 10^expo; preserve precision.',
        ...tick,
      }
    },
  })

  const getPriceHistory = createTool({
    id: 'get-price-history',
    description:
      'Read the last 1–1440 minutes of persisted underlying asset prices, sampled at up to 60 points. Carries values forward between observations; not OHLC, every tick, or settlement truth. Use canonical symbols from listPriceAssets.',
    inputSchema: z.object({
      symbol: FeedSymbolSchema,
      minutes: z.int().check(z.minimum(1), z.maximum(MAX_HISTORY_MINUTES)),
    }),
    execute: async ({ symbol, minutes }, context) => {
      if (!client) return unavailable
      const to = Math.floor(Date.now() / 1000)
      const from = to - minutes * 60
      const response = await client.getHistory(symbol, from, to, context?.abortSignal)
      if (!response.ok)
        return { status: 'unavailable', symbol, reason: 'Price history could not be retrieved.' }
      const history = response.data
      if (
        !history.points.length ||
        history.firstTickAtUs === null ||
        history.lastTickAtUs === null
      ) {
        return {
          status: 'unavailable',
          symbol,
          reason: 'No observed ticks cover the requested window.',
        }
      }
      return {
        status: 'ok',
        source: 'Kyros Price Feeds',
        symbol,
        retrievedAt: new Date().toISOString(),
        from,
        to,
        priceEncoding:
          'Exact price = mantissa × 10^expo. Point t is a sampling time in Unix seconds, not an observation timestamp.',
        limitation:
          'LOCF samples, not exact extrema or complete tick history. Use first/last observation coverage when explaining gaps; never infer news or causes from price movement alone.',
        ...history,
      }
    },
  })
  return { listPriceAssets, getLivePrice, getPriceHistory }
}
