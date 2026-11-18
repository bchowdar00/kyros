'use client'

import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { useState } from 'react'

import type { ExchangeApi } from './live-public-strategy-widget'
import { PublicStrategiesList, type PublicStrategiesListProps } from './public-strategies-list'
import type { PublicStrategy } from './types'
import { useStrategyTemplates } from './use-strategy-templates'

export interface LivePublicStrategiesListProps
  extends Omit<PublicStrategiesListProps, 'strategies'> {
  api: ExchangeApi
  /** The cards the page loaded; the first render, until the exchange answers again. */
  initial: readonly PublicStrategy[]
}

function ConnectedPublicStrategiesList({ api, initial, ...props }: LivePublicStrategiesListProps) {
  const { data } = useStrategyTemplates(api.apiUrl, api.series, initial)
  return <PublicStrategiesList {...props} strategies={data} />
}

/** The public strategy cards over a live exchange: polled, so readiness changes land on the page. */
export function LivePublicStrategiesList(props: LivePublicStrategiesListProps) {
  const [queryClient] = useState(() => new QueryClient())

  return (
    <QueryClientProvider client={queryClient}>
      <ConnectedPublicStrategiesList {...props} />
    </QueryClientProvider>
  )
}
