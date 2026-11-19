'use client'

import { useQuery } from '@tanstack/react-query'
import { createApiClient, type ApiResult } from '@kyros/api'

import { ActiveMarketSchema } from './active-market'

const POLL_INTERVAL_MS = 1_000
const REQUEST_TIMEOUT_MS = 3_000
const activeMarketKeys = {
  series: (seriesId: number) => ['active-market', seriesId] as const,
}

function selectEventId(response: ApiResult<{ eventId: number }>) {
  return response.ok ? response.data.eventId : null
}

export function useActiveMarket(seriesId: number, eventId: number) {
  return useQuery({
    queryKey: activeMarketKeys.series(seriesId),
    queryFn: ({ signal }) =>
      createApiClient({
        baseUrl: window.location.origin,
        retry: { attempts: 1 },
        timeoutMs: REQUEST_TIMEOUT_MS,
      }).get(`/api/series/${seriesId}/active-market`, {
        headers: { 'Cache-Control': 'no-cache' },
        schema: ActiveMarketSchema,
        signal,
      }),
    initialData: { ok: true, data: { eventId } },
    select: selectEventId,
    // Fetch status and timestamps change each second; the selected ID usually does not.
    notifyOnChangeProps: ['data'],
    refetchInterval: POLL_INTERVAL_MS,
    refetchIntervalInBackground: false,
    refetchOnWindowFocus: 'always',
    refetchOnReconnect: 'always',
    retry: false,
    staleTime: POLL_INTERVAL_MS,
  })
}
