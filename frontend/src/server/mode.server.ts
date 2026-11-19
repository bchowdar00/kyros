import 'server-only'

import { createRustExchangeClient } from '@kyros/api'
import { cache } from 'react'

import { env } from '@/env'
import { type SiteMode, toSiteMode } from '@/features/mode/mode.mapper'

/** The nav's mode badge. `undefined` when no exchange is configured: there is no badge. */
export const loadSiteMode = cache(async (): Promise<SiteMode | undefined> => {
  const baseUrl = env.EXCHANGE_API_URL

  if (baseUrl === undefined) {
    return undefined
  }

  // Mirrors the catalog client in `silk-road.server.ts`, which does not export its own.
  const exchange = createRustExchangeClient({
    baseUrl,
    fetch: (input, init) => fetch(input, { ...init, cache: 'no-store' }),
    retry: { attempts: 1 },
    timeoutMs: 2_000,
  })
  const result = await exchange.listSeries()

  return toSiteMode(result.ok ? result.data : undefined)
})
