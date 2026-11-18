'use client'

import { useEffect, useState } from 'react'

import { canAnimate, startFrameLoop } from '../lib/animation'

/** How often the clock advances without animation frames (reduced motion, no rAF). */
const FALLBACK_TICK_MS = 1_000

/**
 * The wall clock in epoch milliseconds, advanced every animation frame while
 * `enabled`. It is what a live chart's provisional tip rides, so the line keeps
 * growing between samples even when the feed is quiet and the visible window is a
 * fixed round rather than a moving viewport. Disabled, it holds the time it last read.
 */
export function useWallClock(enabled: boolean): number {
  const [now, setNow] = useState(() => Date.now())

  useEffect(() => {
    if (!enabled) return
    const advance = () => {
      setNow(Date.now())
      return true
    }
    advance()
    if (canAnimate()) return startFrameLoop(advance)
    // Reduced motion still keeps the tip current; it only skips the intermediate frames.
    const interval = window.setInterval(advance, FALLBACK_TICK_MS)
    return () => window.clearInterval(interval)
  }, [enabled])

  return now
}
