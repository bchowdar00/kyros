import 'server-only'

import { cache } from 'react'
import { createApiClient } from '@tiny/api'
import * as z from 'zod/mini'
import type { ChartPoint } from '@tiny/ui/charts/lib/types'

import { env } from '@/env'
import { toPriceHistory } from '@/features/market-detail/price-history.mapper'

const historySchema = z.object({
  s: z.literal('ok'),
  points: z.array(
    z.object({
      t: z.int(),
      mantissa: z.string().check(z.maxLength(20), z.regex(/^-?\d+$/)),
      expo: z.int(),
    })
  ),
  firstTickAtUs: z.nullable(z.int()),
  lastTickAtUs: z.nullable(z.int()),
})

/** One bounded history request per rendered market; the browser then joins the live stream. */
export const loadPriceHistory = cache(
  async (symbol: string, durationMs: number): Promise<readonly ChartPoint[]> => {
    const baseUrl = env.NEXT_PUBLIC_PRICE_FEEDS_URL
    if (baseUrl === undefined) return []
    const to = Math.floor(Date.now() / 1_000)
    const history = await createApiClient({
      baseUrl,
      fetch: (input, init) => fetch(input, { ...init, cache: 'no-store' }),
      retry: { attempts: 1 },
      timeoutMs: 2_000,
    }).get('/v1/prices', {
      schema: historySchema,
      searchParams: { symbol, from: to - Math.ceil(durationMs / 1_000), to, resolution: 300 },
    })
    return history.ok ? toPriceHistory(history.data) : []
  }
)
