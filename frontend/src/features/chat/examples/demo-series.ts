import type { ChartPoint } from '@kyros/ui/charts/lib/types'
import { createSeededRandom } from '@kyros/ui/charts/lib/tape'

/** A fixed moment so every gallery render is identical: 2026-09-05 14:30 UTC. */
export const DEMO_START = Date.UTC(2026, 8, 5, 14, 30)
export const DEMO_TIME_ZONE = 'UTC'
export const SECOND_MS = 1_000

export interface DemoWalkOptions {
  seed: number
  count: number
  /** First sample time; later samples step by `stepMs`. */
  start?: number
  stepMs?: number
  /** Value at the first sample. */
  base: number
  /** Added every step, before noise. */
  drift?: number
  /** Scale of the per-step noise. */
  volatility: number
  /** Clamp range, for bounded signals such as probabilities. */
  clamp?: readonly [number, number]
}

/**
 * Seeded random walk with a touch of mean reversion, so a series wanders like a
 * price rather than a coin flip. Deterministic for a given seed; never live data.
 */
export function demoWalk({
  seed,
  count,
  start = DEMO_START,
  stepMs = SECOND_MS,
  base,
  drift = 0,
  volatility,
  clamp,
}: DemoWalkOptions): ChartPoint[] {
  const random = createSeededRandom(seed)
  let value = base
  let velocity = 0
  return Array.from({ length: count }, (_, index) => {
    // Two uniforms summed approximate a bell curve well enough for a demo tape.
    const shock = (random() + random() - 1) * volatility
    velocity = velocity * 0.6 + shock
    value += drift + velocity - (value - base) * 0.02
    if (clamp) value = Math.min(clamp[1], Math.max(clamp[0], value))
    return { time: start + index * stepMs, value }
  })
}

/** `count` independent draws from `draw`, which receives a seeded uniform generator. */
export function demoSamples<T>(seed: number, count: number, draw: (random: () => number) => T) {
  const random = createSeededRandom(seed)
  return Array.from({ length: count }, () => draw(random))
}
