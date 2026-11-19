'use client'

import { createRustExchangeClient } from '@kyros/api'
import type { RustGeminiSeries } from '@kyros/domain'
import { useQuery } from '@tanstack/react-query'

import type { GeminiBookState } from '../types'

/** Gemini's book flips to `stale` after ten quiet seconds; polling at half that keeps up. */
const POLL_INTERVAL_MS = 5_000
const REQUEST_TIMEOUT_MS = 3_000

/**
 * The series' Gemini block as the exchange reports it now. The page loaded the first one, so
 * it is the first render; a failed poll keeps the last answer and says the poll failed.
 * Query data cannot be `undefined`, so a series without a block is cached as `null`.
 */
export function useGeminiSeries(
  apiUrl: string,
  series: string,
  initial: RustGeminiSeries | undefined
): GeminiBookState {
  const query = useQuery({
    queryKey: ['gemini-series', apiUrl, series] as const,
    queryFn: async ({ signal }): Promise<RustGeminiSeries | null> => {
      const result = await createRustExchangeClient({
        baseUrl: apiUrl,
        retry: { attempts: 1 },
        timeoutMs: REQUEST_TIMEOUT_MS,
      }).listSeries(signal)
      if (!result.ok) throw result.error
      return result.data.find(row => row.series === series)?.gemini ?? null
    },
    initialData: initial ?? null,
    initialDataUpdatedAt: 0,
    notifyOnChangeProps: ['data', 'isError'],
    refetchInterval: POLL_INTERVAL_MS,
    refetchIntervalInBackground: false,
    refetchOnWindowFocus: 'always',
    refetchOnReconnect: 'always',
    retry: false,
    staleTime: POLL_INTERVAL_MS,
  })
  const gemini = query.data ?? undefined
  return query.isError ? { status: 'poll-failed', gemini } : { status: 'polled', gemini }
}
