'use client'

import { QueryClient, useQuery } from '@tanstack/react-query'
import { useState } from 'react'
import { createApiClient } from '@tiny/api'
import type { StrategyBrief } from '@/mastra/capabilities/strategy/brief'
import { BuildStartedSchema, BuildStatusSchema, type BuildStatus } from './strategy-build'

/** Builds take tens of seconds; a short poll keeps the card honest without hammering. */
const POLL_INTERVAL_MS = 1_500

function client() {
  return createApiClient({ baseUrl: window.location.origin, retry: { attempts: 1 } })
}

/** Starts a build for the brief and resolves to its id, or `undefined` when refused. */
export async function startStrategyBuild(brief: StrategyBrief) {
  const response = await client().post('/api/strategy/builds', {
    schema: BuildStartedSchema,
    body: { brief },
  })
  return response.ok ? response.data.id : undefined
}

/** Follows one build until it settles. */
export function useStrategyBuild(id: string) {
  const [queryClient] = useState(() => new QueryClient())
  const query = useQuery(
    {
      queryKey: ['strategy-build', id],
      queryFn: async ({ signal }) => {
        const response = await client().get(`/api/strategy/builds/${encodeURIComponent(id)}`, {
          schema: BuildStatusSchema,
          signal,
        })
        if (!response.ok) throw response.error
        return response.data
      },
      refetchInterval: current =>
        current.state.data?.status === 'running' || current.state.data === undefined
          ? POLL_INTERVAL_MS
          : false,
      refetchIntervalInBackground: true,
      retry: false,
    },
    queryClient
  )
  return {
    build: query.data as BuildStatus | undefined,
    unreachable: query.isError,
  }
}
