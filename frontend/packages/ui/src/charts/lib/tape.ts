import type { ChartPoint } from './types'

/** Deterministic linear congruential generator returning values in `[0, 1)`. */
export function createSeededRandom(seed: number) {
  let state = seed >>> 0

  return () => {
    state = (Math.imul(state, 1_664_525) + 1_013_904_223) >>> 0

    return state / 4_294_967_296
  }
}

export interface PriceTickStepOptions {
  /** Absolute price distance the noise spans. */
  band: number
  /** Fraction of the gap to `target` the walk closes each tick; keeps the tape near the strike. */
  pull?: number
  /** Price the walk reverts towards, for example a market strike. */
  target: number
}

/**
 * One step of a mean-reverting price tape, for `useSimulatedTicks`. The values
 * are generated, not observed.
 */
export function createPriceTickStep({ band, pull = 0.015, target }: PriceTickStepOptions) {
  return (last: ChartPoint, random: () => number) =>
    Number((last.value + (random() - 0.5) * band * 0.08 + (target - last.value) * pull).toFixed(2))
}

/** One step of a probability tape, bounded to the 1-99 range prices imply. */
export function createProbabilityTickStep({ drift = 1.6 }: { drift?: number } = {}) {
  return (last: ChartPoint, random: () => number) =>
    Number(Math.min(99, Math.max(1, last.value + (random() - 0.5) * drift)).toFixed(2))
}

/** A soft logistic map from price distance to an implied "above target" probability. */
export function impliedProbability(price: number, target: number, band: number) {
  const distance = (price - target) / band
  const probability = 100 / (1 + Math.exp(-distance * 6))

  return Number(Math.min(99, Math.max(1, probability)).toFixed(2))
}
