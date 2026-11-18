import { createSeededRandom } from './tape'
import type { ChartPoint, TimeRange } from './types'

/** Shape anchors (0–100) sampled evenly across the range; the walk wanders around them. */
export const DEFAULT_MOCK_ANCHORS: readonly number[] = [
  48, 50, 47, 49, 46, 45, 43, 44, 41, 40, 38, 39, 36, 34, 35, 37, 34, 32, 30, 31, 28, 27, 25, 26,
  29, 31, 28, 26, 24, 27, 30, 28, 25, 23, 26, 29, 27, 24, 26, 28, 31,
]

/** Cubic Hermite interpolation with Catmull-Rom tangents: smooth turns, no plateaus. */
function catmullRom(before: number, from: number, to: number, after: number, t: number) {
  const tangentFrom = (to - before) / 2
  const tangentTo = (after - from) / 2
  const t2 = t * t
  const t3 = t2 * t

  return (
    (2 * t3 - 3 * t2 + 1) * from +
    (t3 - 2 * t2 + t) * tangentFrom +
    (-2 * t3 + 3 * t2) * to +
    (t3 - t2) * tangentTo
  )
}

export interface MockWalkOptions {
  anchors?: readonly number[]
  count: number
  range: TimeRange
  seed: number
  /** Scales the order-flow noise; `1` matches live prediction-market ticks. */
  volatility?: number
}

/**
 * Seeded, correlated order-flow process with rare event shocks. Values stay
 * within 1–99 and the first and last samples sit exactly on their anchors.
 */
export function createMockWalk({
  anchors = DEFAULT_MOCK_ANCHORS,
  count,
  range,
  seed,
  volatility = 1,
}: MockWalkOptions): ChartPoint[] {
  const random = createSeededRandom(seed)
  const safeCount = Math.max(2, count)
  let microTrend = 0
  let microVelocity = 0

  return Array.from({ length: safeCount }, (_, index) => {
    const progress = index / (safeCount - 1)
    const anchorPosition = progress * (anchors.length - 1)
    const lowerAnchorIndex = Math.floor(anchorPosition)
    const upperAnchorIndex = Math.min(lowerAnchorIndex + 1, anchors.length - 1)
    const anchorProgress = anchorPosition - lowerAnchorIndex
    const lowerAnchor = anchors[lowerAnchorIndex] ?? 50
    const upperAnchor = anchors[upperAnchorIndex] ?? lowerAnchor
    const beforeAnchor = anchors[lowerAnchorIndex - 1] ?? lowerAnchor
    const afterAnchor = anchors[upperAnchorIndex + 1] ?? upperAnchor
    const baseline = catmullRom(beforeAnchor, lowerAnchor, upperAnchor, afterAnchor, anchorProgress)
    const isEndpoint = index === 0 || index === safeCount - 1

    if (!isEndpoint) {
      const orderFlowShock = (random() - 0.5) * 0.11 * volatility
      const newsShock = random() < 0.0015 ? (random() - 0.5) * 1.8 * volatility : 0

      microVelocity = microVelocity * 0.91 + orderFlowShock + newsShock
      microTrend = microTrend * 0.992 + microVelocity
    }

    const quotedSpreadNoise = isEndpoint ? 0 : (random() - 0.5) * 0.18 * volatility
    const microMovement = isEndpoint ? 0 : microTrend * 0.22 + quotedSpreadNoise
    const value = Number(Math.min(99, Math.max(1, baseline + microMovement)).toFixed(2))

    return {
      time: Math.round(range.start + progress * (range.end - range.start)),
      value,
    }
  })
}

export type MockProbabilityOptions = MockWalkOptions

/** Probability ticks on a 0–100 scale. */
export function createMockProbabilityPoints(options: MockProbabilityOptions): ChartPoint[] {
  return createMockWalk(options)
}

/** Complements a probability series so two outcomes always sum to 100. */
export function complementProbabilityPoints(points: readonly ChartPoint[]): ChartPoint[] {
  return points.map(point => ({ ...point, value: Number((100 - point.value).toFixed(2)) }))
}

export interface MockPriceOptions extends MockWalkOptions {
  /** Absolute price distance represented by the full 0–100 walk band. */
  band: number
  /** Price the walk is centred on, for example a market target. */
  basePrice: number
  /** Decimal places kept on generated prices. Defaults to `2`. */
  decimals?: number
}

/** Price ticks that wander around `basePrice` with the same shape as the probability walk. */
export function createMockPricePoints({
  band,
  basePrice,
  decimals = 2,
  ...options
}: MockPriceOptions): ChartPoint[] {
  return createMockWalk(options).map(point => {
    const price = basePrice + ((point.value - 50) / 50) * band
    const volume = Math.round(0.05 + Math.abs(point.value - 50) * 0.4)

    return { time: point.time, value: Number(price.toFixed(decimals)), volume }
  })
}
