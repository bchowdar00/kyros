'use client'

import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { useRouter } from 'next/navigation'
import { useEffect, useState } from 'react'

import { useActiveMarket } from './active-market.query'

interface LiveMarketPollerProps {
  eventId: number
  seriesId: number
}

// Recover if a route refresh fails or briefly receives the previous event during rollover.
const REFRESH_RETRY_MS = 5_000

function ActiveMarketMonitor({ eventId, seriesId }: LiveMarketPollerProps) {
  const router = useRouter()
  const { data: activeEventId } = useActiveMarket(seriesId, eventId)

  useEffect(() => {
    if (activeEventId === null || activeEventId === eventId) {
      return
    }

    router.refresh()
    const retry = window.setInterval(() => router.refresh(), REFRESH_RETRY_MS)
    return () => window.clearInterval(retry)
  }, [activeEventId, eventId, router])

  if (activeEventId === null) {
    return (
      <output className="block px-4xl py-sm text-label text-muted-foreground">
        Live market check unavailable. Retrying…
      </output>
    )
  }

  return null
}

export function LiveMarketPoller(props: LiveMarketPollerProps) {
  const [queryClient] = useState(() => new QueryClient())

  return (
    <QueryClientProvider client={queryClient}>
      <ActiveMarketMonitor {...props} />
    </QueryClientProvider>
  )
}
