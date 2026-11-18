'use client'

import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { useState } from 'react'

import type { StrategyDeploy } from './exchange/deploy'
import { PublicStrategyWidget } from './public-strategy-widget'
import type { PublicStrategy } from './types'
import { useStrategyTemplate } from './use-strategy-template'

export interface ExchangeApi {
  apiUrl: string
  series: string
}

interface LivePublicStrategyWidgetProps {
  api: ExchangeApi
  deploy?: StrategyDeploy | undefined
  strategy: PublicStrategy
  inverse: boolean
  scopeLabel: string
  onClose: () => void
}

function ConnectedPublicStrategyWidget({
  api,
  deploy,
  strategy,
  inverse,
  scopeLabel,
  onClose,
}: LivePublicStrategyWidgetProps) {
  const { data, isError, refetch } = useStrategyTemplate(api.apiUrl, api.series, strategy)
  // A successful deploy changes the card's running count; read it back.
  const deployAndRefresh: StrategyDeploy | undefined =
    deploy === undefined
      ? undefined
      : {
          ...deploy,
          submit: async request => {
            const deployed = await deploy.submit(request)
            if (deployed) void refetch()
            return deployed
          },
        }

  return (
    <PublicStrategyWidget
      strategy={data}
      inverse={inverse}
      scopeLabel={scopeLabel}
      deploy={deployAndRefresh}
      onClose={onClose}
      status={isError ? 'Showing the card as loaded; the exchange did not answer.' : undefined}
    />
  )
}

/** The Run / Inverse widget over a live exchange: refetches the template when it opens. */
export function LivePublicStrategyWidget(props: LivePublicStrategyWidgetProps) {
  const [queryClient] = useState(() => new QueryClient())

  return (
    <QueryClientProvider client={queryClient}>
      <ConnectedPublicStrategyWidget {...props} />
    </QueryClientProvider>
  )
}
