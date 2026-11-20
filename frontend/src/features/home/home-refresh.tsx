'use client'

import { useRouter } from 'next/navigation'
import { useEffect } from 'react'

const REFRESH_MS = 10_000

/**
 * Keeps the home page current. The page is rendered on the server from lists kept there
 * (Gemini's markets, the record on Arc), so asking again is cheap, and a list that was not
 * ready for the first paint arrives with the next one. Draws nothing.
 */
export function HomeRefresh() {
  const router = useRouter()

  useEffect(() => {
    const timer = window.setInterval(() => router.refresh(), REFRESH_MS)
    return () => window.clearInterval(timer)
  }, [router])

  return null
}
