'use client'

import { useRouter } from 'next/navigation'
import { useEffect } from 'react'

/** How often the waiting screen re-asks the server whether a game has opened. */
const REFRESH_INTERVAL_MS = 15_000

/**
 * A leaf that re-fetches the route on an interval, so a series that is waiting for its next
 * game moves to the live market without the viewer reloading. The timer clears on unmount.
 */
export function MarketWaitingRefresh() {
  const router = useRouter()

  useEffect(() => {
    const timer = window.setInterval(() => router.refresh(), REFRESH_INTERVAL_MS)
    return () => window.clearInterval(timer)
  }, [router])

  return null
}
