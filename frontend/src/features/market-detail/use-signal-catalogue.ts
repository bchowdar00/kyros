'use client'

import { useQuery } from '@tanstack/react-query'
import { createRustExchangeClient } from '@kyros/api'
import type { RustSignalCatalogue } from '@kyros/domain'

/** Values stream over `/ws/signals`; this only resyncs the metadata and readiness. */
const POLL_INTERVAL_MS = 30_000
const REQUEST_TIMEOUT_MS = 3_000

export function useSignalCatalogue(apiUrl: string, series: string) {
  return useQuery({
    queryKey: ['signal-catalogue', apiUrl, series] as const,
    queryFn: async ({ signal }): Promise<RustSignalCatalogue | null> => {
      const result = await createRustExchangeClient({
        baseUrl: apiUrl,
        retry: { attempts: 1 },
        timeoutMs: REQUEST_TIMEOUT_MS,
      }).getSignals(series, signal)
      return result.ok ? result.data : null
    },
    notifyOnChangeProps: ['data'],
    refetchInterval: POLL_INTERVAL_MS,
    refetchIntervalInBackground: false,
    refetchOnWindowFocus: 'always',
    refetchOnReconnect: 'always',
    retry: false,
    staleTime: POLL_INTERVAL_MS,
  })
}
