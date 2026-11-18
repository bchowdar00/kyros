import type { TimeRange } from './types'

export interface LinearScale {
  (value: number): number
  readonly domain: readonly [number, number]
  invert(position: number): number
  readonly range: readonly [number, number]
}

export function createLinearScale(
  domain: readonly [number, number],
  range: readonly [number, number]
): LinearScale {
  const [domainStart, domainEnd] = domain
  const [rangeStart, rangeEnd] = range
  const domainSpan = domainEnd - domainStart
  const rangeSpan = rangeEnd - rangeStart

  const scale = (value: number) =>
    domainSpan === 0
      ? rangeStart + rangeSpan / 2
      : rangeStart + ((value - domainStart) / domainSpan) * rangeSpan

  return Object.assign(scale, {
    domain,
    invert: (position: number) =>
      rangeSpan === 0
        ? domainStart + domainSpan / 2
        : domainStart + ((position - rangeStart) / rangeSpan) * domainSpan,
    range,
  })
}

const SQRT_50 = Math.sqrt(50)
const SQRT_10 = Math.sqrt(10)
const SQRT_2 = Math.sqrt(2)

/**
 * Chooses a "nice" step (1, 2 or 5 times a power of ten) that splits `span`
 * into roughly `targetCount` intervals.
 */
export function niceStep(span: number, targetCount: number): number {
  if (!(span > 0) || !(targetCount > 0)) {
    return 1
  }

  const rough = span / targetCount
  const magnitude = 10 ** Math.floor(Math.log10(rough))
  const normalized = rough / magnitude

  if (normalized >= SQRT_50) {
    return magnitude * 10
  }

  if (normalized >= SQRT_10) {
    return magnitude * 5
  }

  if (normalized >= SQRT_2) {
    return magnitude * 2
  }

  return magnitude
}

/** The next value in the 1, 2, 5, 10 sequence after `step`. */
export function nextNiceStep(step: number): number {
  const magnitude = 10 ** Math.floor(Math.log10(step))
  const normalized = step / magnitude

  if (normalized < 2) {
    return magnitude * 2
  }

  if (normalized < 5) {
    return magnitude * 5
  }

  return magnitude * 10
}

function stepDecimals(step: number) {
  return Math.max(0, -Math.floor(Math.log10(step)) + 1)
}

/** Removes floating-point noise from a multiple of `step`. */
export function roundToStep(value: number, step: number): number {
  return Number(value.toFixed(stepDecimals(step)))
}

/** Nice tick values inside `[min, max]`, inclusive. */
export function linearTicks(min: number, max: number, targetCount = 5): number[] {
  if (min === max) {
    return [min]
  }

  const step = niceStep(max - min, targetCount)
  const first = Math.ceil(min / step) * step
  const last = Math.floor(max / step) * step
  const ticks: number[] = []

  for (let value = first; value <= last + step * 1e-9; value += step) {
    ticks.push(roundToStep(value, step))
  }

  return ticks
}

export interface DomainWithTicks {
  domain: readonly [number, number]
  step: number
  ticks: readonly number[]
}

/**
 * Expands `[min, max]` to nice bounds and returns the matching ticks so the
 * grid, the axis labels and the scale can never disagree.
 */
export function niceDomain(min: number, max: number, targetCount = 5): DomainWithTicks {
  if (!Number.isFinite(min) || !Number.isFinite(max)) {
    return { domain: [0, 1], step: 0.25, ticks: [0, 0.25, 0.5, 0.75, 1] }
  }

  let low = Math.min(min, max)
  let high = Math.max(min, max)

  if (low === high) {
    const padding = Math.max(Math.abs(low) * 0.05, 1)
    low -= padding
    high += padding
  }

  const step = niceStep(high - low, targetCount)
  const domainStart = roundToStep(Math.floor(low / step) * step, step)
  const domainEnd = roundToStep(Math.ceil(high / step) * step, step)

  return {
    domain: [domainStart, domainEnd],
    step,
    ticks: linearTicks(domainStart, domainEnd, targetCount),
  }
}

export interface CenteredDomainOptions {
  /** Fraction of a row kept free above and below the data. Defaults to `0.1`. */
  headroom?: number
  /** Grid rows on each side of the center. Defaults to `4`. */
  rows?: number
}

/**
 * Builds a symmetric domain around `center` (for example a price target) with
 * `rows` nice steps above and below it, growing the step until every value fits.
 */
export function centeredDomain(
  center: number,
  values: readonly number[],
  { headroom = 0.1, rows = 4 }: CenteredDomainOptions = {}
): DomainWithTicks {
  let maxDeviation = 0

  for (const value of values) {
    if (Number.isFinite(value)) {
      maxDeviation = Math.max(maxDeviation, Math.abs(value - center))
    }
  }

  if (maxDeviation === 0) {
    maxDeviation = Math.max(Math.abs(center) * 0.001, 1)
  }

  const usableRows = rows - headroom
  let step = niceStep(maxDeviation, usableRows)

  while (step * usableRows < maxDeviation) {
    step = nextNiceStep(step)
  }

  // Only as many rows as the data needs, so the line fills the plot instead of
  // floating in the middle of it. The centre tick stays the target either way.
  const usedRows = Math.min(rows, Math.max(2, Math.ceil((maxDeviation * (1 + headroom)) / step)))
  const ticks: number[] = []

  for (let row = -usedRows; row <= usedRows; row += 1) {
    ticks.push(Number((center + row * step).toPrecision(12)))
  }

  return {
    domain: [ticks[0] ?? center - usedRows * step, ticks.at(-1) ?? center + usedRows * step],
    step,
    ticks,
  }
}

/** Index of the point whose `time` is closest to `time`; assumes ascending order. */
export function findNearestIndex(points: readonly { time: number }[], time: number): number {
  if (points.length === 0) {
    return -1
  }

  let low = 0
  let high = points.length - 1

  while (low < high) {
    const middle = (low + high) >>> 1
    const middleTime = points[middle]?.time ?? 0

    if (middleTime < time) {
      low = middle + 1
    } else {
      high = middle
    }
  }

  const previous = points[low - 1]
  const current = points[low]

  if (previous && current && time - previous.time < current.time - time) {
    return low - 1
  }

  return low
}

/** Points inside `range` plus one neighbour on each side so edge segments still count. */
export function sliceToRange<T extends { time: number }>(
  points: readonly T[],
  range: TimeRange
): readonly T[] {
  const first = points[0]
  const last = points.at(-1)

  if (!first || !last || (first.time >= range.start && last.time <= range.end)) {
    return points
  }

  const startIndex = Math.max(0, findNearestIndex(points, range.start) - 1)
  const endIndex = Math.min(points.length - 1, findNearestIndex(points, range.end) + 1)

  return points.slice(startIndex, endIndex + 1)
}
