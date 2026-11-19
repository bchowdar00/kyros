'use client'

import { useEffect, useState } from 'react'
import { createSeededRandom } from '@kyros/ui/charts/lib/tape'
import type { ChartPoint } from '@kyros/ui/charts/lib/types'

const MAX_TICKS = 4_000

export interface DueTicksOptions {
  /** Tape time now; every whole interval it has passed `last` produces a tick. */
  now: number
  intervalMs: number
  /** The newest sample so far, base or tick. */
  last: ChartPoint
  maxTicks: number
  random: () => number
  step: (last: ChartPoint, random: () => number) => number
}

/**
 * Extends `ticks` up to `now`. Ticks are generated one interval apart from the
 * last sample, so a slow frame or a backgrounded tab catches up in one go
 * instead of leaving a gap in the line.
 */
export function appendDueTicks(
  ticks: readonly ChartPoint[],
  { intervalMs, last, maxTicks, now, random, step }: DueTicksOptions
): readonly ChartPoint[] {
  const due = Math.floor((now - last.time) / intervalMs)

  if (due < 1) {
    return ticks
  }

  const appended = [...ticks]
  let cursor = last

  for (let index = 0; index < due; index += 1) {
    cursor = { time: cursor.time + intervalMs, value: step(cursor, random) }
    appended.push(cursor)
  }

  return appended.length > maxTicks ? appended.slice(appended.length - maxTicks) : appended
}

/** Playground-only observations. The shared card owns the scrolling window. */
export function useSimulatedTape({
  base,
  intervalMs,
  seed,
  step,
}: {
  base: readonly ChartPoint[]
  intervalMs: number
  seed: number
  step: (last: ChartPoint, random: () => number) => number
}) {
  const [ticks, setTicks] = useState<readonly ChartPoint[]>([])
  const [random] = useState(() => createSeededRandom(seed))

  useEffect(() => {
    const timer = window.setInterval(() => {
      if (document.hidden) return
      setTicks(previous => {
        const last = previous.at(-1) ?? base.at(-1)
        return last
          ? appendDueTicks(previous, {
              intervalMs,
              last,
              maxTicks: MAX_TICKS,
              now: last.time + intervalMs,
              random,
              step,
            })
          : previous
      })
    }, intervalMs)
    return () => window.clearInterval(timer)
  }, [base, intervalMs, random, step])

  return { points: ticks.length > 0 ? [...base, ...ticks] : base }
}
