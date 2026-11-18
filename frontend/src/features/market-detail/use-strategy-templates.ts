'use client'

import { useQuery } from '@tanstack/react-query'
import { createRustExchangeClient } from '@tiny/api'

import { toPublicStrategies } from './strategy-template.mapper'
import type { PublicStrategy } from './types'

/**
 * Readiness moves with the signal lanes (a lane warms up, degrades and recovers), so the
 * column resyncs at this pace: a card that cannot deploy drops out, and comes back, without
 * a reload. The Run / Inverse widget refetches its own card on open.
 */
const POLL_INTERVAL_MS = 15_000
const REQUEST_TIMEOUT_MS = 3_000

/**
 * The strategy column as the exchange reports it now. The page loaded the first cards, so
 * they are the first render and a failed refetch keeps the last answer.
 */
export function useStrategyTemplates(
  apiUrl: string,
  series: string,
  initial: readonly PublicStrategy[]
) {
  return useQuery({
    queryKey: ['strategy-templates', apiUrl, series] as const,
    queryFn: async ({ signal }): Promise<readonly PublicStrategy[]> => {
      const result = await createRustExchangeClient({
        baseUrl: apiUrl,
        retry: { attempts: 1 },
        timeoutMs: REQUEST_TIMEOUT_MS,
      }).getStrategyTemplates(series, signal)
      if (!result.ok) throw result.error
      return toPublicStrategies(result.data)
    },
    initialData: initial,
    initialDataUpdatedAt: 0,
    notifyOnChangeProps: ['data'],
    refetchInterval: POLL_INTERVAL_MS,
    refetchIntervalInBackground: false,
    refetchOnWindowFocus: 'always',
    refetchOnReconnect: 'always',
    retry: false,
    staleTime: POLL_INTERVAL_MS,
  })
}
