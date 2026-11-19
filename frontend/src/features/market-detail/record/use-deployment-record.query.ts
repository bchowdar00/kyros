'use client'

import type { RustRecordView } from '@kyros/domain'
import { createRustExchangeClient } from '@kyros/api'
import { useQuery } from '@tanstack/react-query'

/** A seal lands once per settled round, so the record line catches up at this pace. */
const POLL_INTERVAL_MS = 15_000
const REQUEST_TIMEOUT_MS = 3_000
const NOT_FOUND = 404

export type DeploymentRecord = { status: 'none' } | { status: 'found'; record: RustRecordView }

/**
 * The on-chain record of the deployment running in the sub-account at `address`. A 404 is an
 * answer (no record kept for it); any other failure throws so the query reports an error.
 */
export function useDeploymentRecord(apiUrl: string, address: string) {
  return useQuery({
    queryKey: ['deployment-record', apiUrl, address] as const,
    queryFn: async ({ signal }): Promise<DeploymentRecord> => {
      const result = await createRustExchangeClient({
        baseUrl: apiUrl,
        retry: { attempts: 1 },
        timeoutMs: REQUEST_TIMEOUT_MS,
      }).getRecord(address, signal)
      if (result.ok) return { status: 'found', record: result.data }
      if (result.error.status === NOT_FOUND) return { status: 'none' }
      throw result.error
    },
    refetchInterval: POLL_INTERVAL_MS,
    refetchIntervalInBackground: false,
    retry: false,
    staleTime: POLL_INTERVAL_MS,
  })
}
