'use client'

import { useQuery } from '@tanstack/react-query'
import { createRustExchangeClient } from '@kyros/api'

import { toPublicStrategy } from './strategy-template.mapper'
import type { PublicStrategy } from './types'

const REQUEST_TIMEOUT_MS = 3_000

/**
 * The card as the exchange reports it right now. Opened from a card the page loaded, so the
 * loaded card is the first render and the refetch only brings the figures up to date.
 */
export function useStrategyTemplate(apiUrl: string, series: string, initial: PublicStrategy) {
  return useQuery({
    queryKey: ['strategy-template', apiUrl, series, initial.templateId] as const,
    queryFn: async ({ signal }): Promise<PublicStrategy> => {
      const result = await createRustExchangeClient({
        baseUrl: apiUrl,
        retry: { attempts: 1 },
        timeoutMs: REQUEST_TIMEOUT_MS,
      }).getStrategyTemplate(initial.templateId, series, signal)
      // A failed refetch keeps the card the page loaded; the query reports the error.
      if (!result.ok) throw result.error
      return toPublicStrategy(result.data)
    },
    initialData: initial,
    initialDataUpdatedAt: 0,
    notifyOnChangeProps: ['data', 'isError'],
    refetchOnWindowFocus: false,
    retry: false,
    staleTime: 0,
  })
}
