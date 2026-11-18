'use client'

import { useEffect, useState } from 'react'

import { canAnimate, startFrameLoop } from './lib/animation'
import type { ChartPoint, TimeRange } from './lib/types'

interface LiveChartWindowOptions {
  clock?: 'sample' | 'wall'
  live: boolean
  points: readonly ChartPoint[]
  spanMs: number
}

/** Advances only the viewport. Observed prices belong to the application's feed. */
export function useLiveChartWindow({
  clock = 'wall',
  live,
  points,
  spanMs,
}: LiveChartWindowOptions) {
  const [wallNow, setWallNow] = useState(() =>
    clock === 'sample' ? (points.at(-1)?.time ?? 0) : Date.now()
  )

  useEffect(() => {
    if (!live) return
    const advanceClock = (dt: number) => {
      setWallNow(previous => (clock === 'sample' ? previous + dt : Date.now()))
      return true
    }
    if (canAnimate()) return startFrameLoop(advanceClock)
    // Reduced motion still updates the clock; it only skips intermediate animation frames.
    const interval = window.setInterval(() => advanceClock(1_000), 1_000)
    return () => window.clearInterval(interval)
  }, [clock, live])

  const latestTime = points.at(-1)?.time ?? wallNow
  const now = live ? Math.max(latestTime, wallNow) : latestTime
  const firstTime = points[0]?.time ?? now - spanMs
  const range: TimeRange = {
    end: Math.max(now, firstTime + 1_000),
    start: live ? now - spanMs : Math.max(firstTime, now - spanMs),
  }

  return { now, range }
}
