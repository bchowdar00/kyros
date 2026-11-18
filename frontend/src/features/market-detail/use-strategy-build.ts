'use client'

import { QueryClient, useQuery } from '@tanstack/react-query'
import { useState } from 'react'
import * as z from 'zod/mini'
import { createApiClient } from '@tiny/api'
import type { StrategyBrief } from '@/mastra/capabilities/strategy/brief'
import { BuildStartedSchema, BuildStatusSchema, type BuildStatus } from './strategy-build'

/** Builds take tens of seconds; a short poll keeps the card honest without hammering. */
const POLL_INTERVAL_MS = 1_500

function client() {
  return createApiClient({ baseUrl: window.location.origin, retry: { attempts: 1 } })
}

/** A 429 (resting) or 503 (writer not switched on) carries a user-facing line to show as-is. */
const BuildNoticeSchema = z.object({ message: z.string() })

/** What starting a build produced. */
export type StartBuildOutcome =
  /** The build is running under this id. */
  | { kind: 'started'; id: string }
  /** The meter refused with a line meant for the user (resting, or not switched on here). */
  | { kind: 'notice'; message: string }
  /** The build could not be started for any other reason. */
  | { kind: 'error' }

/**
 * Starts a build for the brief. The build route is an internal Next route (not the exchange),
 * so it is called with `fetch` directly; a 429/503 carries a user-facing line this reads back
 * so the card can show it as returned, rather than a generic refusal.
 */
export async function startStrategyBuild(brief: StrategyBrief): Promise<StartBuildOutcome> {
  let response: Response
  try {
    response = await fetch(new URL('/api/strategy/builds', window.location.origin), {
      method: 'POST',
      headers: { 'content-type': 'application/json', accept: 'application/json' },
      body: JSON.stringify({ brief }),
      cache: 'no-store',
    })
  } catch {
    return { kind: 'error' }
  }
  let body: unknown
  try {
    body = await response.json()
  } catch {
    body = undefined
  }
  if (response.ok) {
    const started = z.safeParse(BuildStartedSchema, body)
    return started.success ? { kind: 'started', id: started.data.id } : { kind: 'error' }
  }
  if (response.status === 429 || response.status === 503) {
    const notice = z.safeParse(BuildNoticeSchema, body)
    if (notice.success) return { kind: 'notice', message: notice.data.message }
  }
  return { kind: 'error' }
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
